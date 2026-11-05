//! 이름 있는 작업 공간 — 같은 root·user·브랜치를 서로 다른 `with_workspace` 서버 둘이 편집한다.
//! 모킹 없음: 실제 엔진과 임시 디렉터리. 순차 호출이라 결정적이다.

use serde_json::{json, Value as J};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use vgraph_mcp::{McpConfig, McpServer, SERVER_INSTRUCTIONS};

fn config(root: &Path) -> McpConfig {
    McpConfig {
        root: root.to_path_buf(),
        user: "local".into(),
        embedder: None,
        embed: Default::default(),
    }
}

fn server(root: &Path, workspace: Option<&str>) -> McpServer {
    McpServer::new(config(root))
        .expect("mcp 서버")
        .with_workspace(workspace.map(String::from))
        .expect("작업 공간 이름")
}

fn raw(srv: &McpServer, name: &str, args: J) -> (bool, J) {
    let resp = srv
        .handle_message(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args } }))
        .expect("response");
    let r = &resp["result"];
    let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
    let is_err = r["isError"].as_bool().unwrap_or(false);
    let body = serde_json::from_str(&text).unwrap_or(J::String(text));
    (is_err, body)
}

fn call(srv: &McpServer, name: &str, args: J) -> J {
    let (is_err, body) = raw(srv, name, args);
    assert!(!is_err, "tool error: {body}");
    body
}

fn set_prop(srv: &McpServer, graph: &str, node: &str, v: i64) -> J {
    call(
        srv,
        "vgraph_edit",
        json!({ "graph": graph,
            "ops": [{ "op": "set_prop", "node": node, "key": "v", "value": v }] }),
    )
}

/// 노드 n1·n2 를 커밋해 둔 저장소.
fn seeded() -> TempDir {
    let dir = TempDir::new().unwrap();
    let s = server(dir.path(), None);
    call(&s, "vgraph_create", json!({ "name": "acme/catalog" }));
    call(
        &s,
        "vgraph_edit",
        json!({ "graph": "acme/catalog", "ops": [
            { "op": "add_node", "labels": ["Item"], "slug": "n1", "props": { "v": 0 } },
            { "op": "add_node", "labels": ["Item"], "slug": "n2", "props": { "v": 0 } } ] }),
    );
    call(
        &s,
        "vgraph_commit",
        json!({ "graph": "acme/catalog", "message": "seed" }),
    );
    dir
}

fn prop_of(srv: &McpServer, graph: &str, slug: &str) -> J {
    let r = call(
        srv,
        "vgraph_show",
        json!({ "addr": format!("{graph}#{slug}") }),
    );
    r["props"]["v"].clone()
}

#[test]
fn two_workspace_servers_edit_same_branch_and_both_land() {
    let dir = seeded();
    let a = server(dir.path(), Some("a"));
    let b = server(dir.path(), Some("b"));
    set_prop(&a, "acme/catalog", "n1", 1);
    set_prop(&b, "acme/catalog", "n2", 2);

    // 서로의 pending 에 상대 편집이 보이지 않는다
    let pa = call(&a, "vgraph_pending", json!({ "graph": "acme/catalog" }));
    let pb = call(&b, "vgraph_pending", json!({ "graph": "acme/catalog" }));
    assert_eq!(pa["pending"]["props_set"], json!(1), "a: {pa}");
    assert_eq!(pb["pending"]["props_set"], json!(1), "b: {pb}");
    assert_ne!(pa["working"], pb["working"]);
    // 기본 작업 공간 서버에는 편집이 없다
    let d = server(dir.path(), None);
    assert_eq!(
        call(&d, "vgraph_pending", json!({ "graph": "acme/catalog" }))["working"],
        J::Null
    );

    call(
        &a,
        "vgraph_commit",
        json!({ "graph": "acme/catalog", "message": "a" }),
    );
    call(
        &b,
        "vgraph_commit",
        json!({ "graph": "acme/catalog", "message": "b" }),
    );
    assert_eq!(prop_of(&d, "acme/catalog", "n1"), json!(1));
    assert_eq!(prop_of(&d, "acme/catalog", "n2"), json!(2));
}

#[test]
fn set_schema_from_named_server_does_not_touch_other_workspace_edits() {
    let dir = seeded();
    let a = server(dir.path(), Some("a"));
    let b = server(dir.path(), Some("b"));
    set_prop(&b, "acme/catalog", "n2", 2);
    call(
        &a,
        "vgraph_admin",
        json!({ "graph": "acme/catalog", "action": "set_schema",
            "schema": { "mode": "off", "node_classes": [], "edge_classes": [] } }),
    );
    // b 의 미커밋 편집은 그대로이고, 아직 커밋되지 않았다
    let pb = call(&b, "vgraph_pending", json!({ "graph": "acme/catalog" }));
    assert_eq!(pb["pending"]["props_set"], json!(1), "b: {pb}");
    assert_eq!(prop_of(&a, "acme/catalog", "n2"), json!(0));
    call(
        &b,
        "vgraph_commit",
        json!({ "graph": "acme/catalog", "message": "b" }),
    );
    assert_eq!(prop_of(&a, "acme/catalog", "n2"), json!(2));
}

fn doc(extra: &str) -> String {
    format!(
        r#"{{"format":"vgraph-json/1","nodes":[{{"slug":"x","labels":["Item"],"props":{{"n":1}}}}{extra}],"edges":[]}}"#
    )
}

#[test]
fn import_replace_checks_only_its_own_workspace_for_pending_edits() {
    let dir = TempDir::new().unwrap();
    // 그래프 등록은 서버가 열릴 때 읽으므로, 생성을 먼저 하고 작업 공간 서버를 띄운다
    call(
        &server(dir.path(), None),
        "vgraph_create",
        json!({ "name": "acme/imp" }),
    );
    let a = server(dir.path(), Some("a"));
    let b = server(dir.path(), Some("b"));
    call(
        &a,
        "vgraph_edit",
        json!({ "graph": "acme/imp", "ops": [
            { "op": "add_node", "labels": ["Item"], "slug": "mine", "props": {} } ] }),
    );

    // b 의 가져오기는 a 의 미커밋 편집에 막히지 않는다
    let r = call(
        &b,
        "vgraph_import",
        json!({ "graph": "acme/imp", "document": doc("") }),
    );
    assert_eq!(r["outcome"], "committed", "{r}");
    // a 의 미커밋 편집은 그대로다
    let pa = call(&a, "vgraph_pending", json!({ "graph": "acme/imp" }));
    assert_eq!(pa["pending"]["nodes_added"], json!(1), "a: {pa}");

    // 같은 작업 공간의 미커밋 편집은 여전히 거부된다 (빈 그래프 조건을 위해 새 그래프에서)
    call(
        &server(dir.path(), None),
        "vgraph_create",
        json!({ "name": "acme/imp2" }),
    );
    let a = server(dir.path(), Some("a"));
    call(
        &a,
        "vgraph_edit",
        json!({ "graph": "acme/imp2", "ops": [
            { "op": "add_node", "labels": ["Item"], "slug": "mine", "props": {} } ] }),
    );
    let (is_err, body) = raw(
        &a,
        "vgraph_import",
        json!({ "graph": "acme/imp2", "document": doc("") }),
    );
    assert!(is_err, "same workspace pending must be rejected: {body}");
    assert!(body.to_string().contains("미커밋 편집"), "{body}");
}

#[test]
fn import_merge_via_edit_follows_own_workspace() {
    let dir = TempDir::new().unwrap();
    // 그래프 등록은 서버가 열릴 때 읽으므로, 생성을 먼저 하고 작업 공간 서버를 띄운다
    call(
        &server(dir.path(), None),
        "vgraph_create",
        json!({ "name": "acme/mrg" }),
    );
    let a = server(dir.path(), Some("a"));
    let b = server(dir.path(), Some("b"));
    let first = call(
        &b,
        "vgraph_import",
        json!({ "graph": "acme/mrg", "document": doc(""),
            "mode": "merge", "via_branch": "feed" }),
    );
    assert_eq!(first["outcome"], "first_import", "{first}");

    // a 가 via 브랜치에 미커밋 편집을 쌓아 둔다 — b 의 동기화는 이것을 검사·폐기하지 않는다
    call(
        &a,
        "vgraph_edit",
        json!({ "graph": "acme/mrg@feed", "ops": [
            { "op": "add_node", "labels": ["Item"], "slug": "mine", "props": {} } ] }),
    );
    let second = call(
        &b,
        "vgraph_import",
        json!({ "graph": "acme/mrg",
            "document": doc(r#",{"slug":"y","labels":["Item"],"props":{}}"#),
            "mode": "merge", "via_branch": "feed" }),
    );
    assert_ne!(second["outcome"], "first_import", "{second}");
    let pa = call(&a, "vgraph_pending", json!({ "graph": "acme/mrg@feed" }));
    assert_eq!(pa["pending"]["nodes_added"], json!(1), "a: {pa}");
}

#[test]
fn invalid_workspace_name_is_rejected_naturally() {
    let dir = TempDir::new().unwrap();
    for bad in ["a b", "", "a/b", "한글"] {
        let e = McpServer::new(config(dir.path()))
            .unwrap()
            .with_workspace(Some(bad.into()))
            .err()
            .unwrap_or_else(|| panic!("{bad:?} 는 거절돼야 함"));
        assert!(
            e.to_string()
                .contains("작업 공간 이름은 영문·숫자·-·_ 만 쓸 수 있습니다"),
            "got: {e}"
        );
    }
}

#[test]
fn server_instructions_carry_workspace_guidance_and_keep_existing_text() {
    for needle in [
        "각자 다른 --workspace 이름",
        "이름 없이(기본 작업 공간) 동시에 편집하면 편집 내용이 사라질 수 있다",
        "동시에 쓰는 프로세스마다 고유",
        "같은 이름을 동시에 쓰면",
        "자동으로 합쳐지고",
        "충돌로 보고",
    ] {
        assert!(
            SERVER_INSTRUCTIONS.contains(needle),
            "SERVER_INSTRUCTIONS 에 없음: {needle}"
        );
    }
    // 기존 문장 보존
    assert!(SERVER_INSTRUCTIONS.contains("embed_target"));
    assert!(SERVER_INSTRUCTIONS.ends_with("충돌로 보고된다."));
}

#[test]
fn binary_help_carries_workspace_guidance() {
    let out = Command::new(env!("CARGO_BIN_EXE_vgraph-mcp"))
        .arg("--help")
        .output()
        .expect("vgraph-mcp 실행");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("--workspace"), "{text}");
    for needle in [
        "각자 다른 --workspace 이름",
        "사라질 수 있습니다",
        "동시에 쓰는 프로세스마다 고유",
        "같은 이름을 동시에 쓰면",
        "자동으로 합쳐지고",
        "충돌로 보고",
    ] {
        assert!(text.contains(needle), "--help 에 없음: {needle}\n{text}");
    }
}
