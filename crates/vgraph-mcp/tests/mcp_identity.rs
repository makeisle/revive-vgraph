//! 요청별 신원 — 서버 하나를 서로 다른 (사용자, 작업 공간) 신원으로 번갈아·동시에 부른다.
//! 모킹 없음: 실제 엔진과 임시 디렉터리.
//!
//! ACL(소유자 전용) 판정은 엔진 `Session::acl`(vgraph-engine/src/engine.rs)에 있다. 엔진·CLI
//! 쪽에는 이를 직접 확인하는 기존 시험이 없어(NoPermission 참조는 error.rs·engine.rs뿐),
//! 여기서는 같은 판정이 신원의 사용자에 따라 달라지는지를 도구 호출로 확인한다.

use serde_json::{json, Value as J};
use std::path::Path;
use std::sync::Barrier;
use tempfile::TempDir;
use vgraph_mcp::{Identity, McpConfig, McpServer};

const OWNER: &str = "owner";
const GRAPH: &str = "acme/catalog";
const THREADS: usize = 4;
const ROUNDS: i64 = 6;

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn server_and_identity_are_send_and_sync() {
    assert_send_sync::<McpServer>();
    assert_send_sync::<Identity>();
}

fn config(root: &Path) -> McpConfig {
    McpConfig {
        root: root.to_path_buf(),
        user: OWNER.into(),
        embedder: None,
        embed: Default::default(),
    }
}

fn who(user: &str, ws: Option<&str>) -> Identity {
    Identity::new(user, ws.map(String::from)).expect("신원")
}

fn raw(srv: &McpServer, id: &Identity, name: &str, args: J) -> (bool, J) {
    let resp = srv
        .handle_message_as(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": name, "arguments": args } }),
            id,
        )
        .expect("response");
    let r = &resp["result"];
    let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
    let is_err = r["isError"].as_bool().unwrap_or(false);
    let body = serde_json::from_str(&text).unwrap_or(J::String(text));
    (is_err, body)
}

fn call(srv: &McpServer, id: &Identity, name: &str, args: J) -> J {
    let (is_err, body) = raw(srv, id, name, args);
    assert!(!is_err, "tool error ({name}): {body}");
    body
}

fn set_prop(srv: &McpServer, id: &Identity, node: &str, v: i64) {
    call(
        srv,
        id,
        "vgraph_edit",
        json!({ "graph": GRAPH,
            "ops": [{ "op": "set_prop", "node": node, "key": "v", "value": v }] }),
    );
}

fn commit(srv: &McpServer, id: &Identity, msg: &str) -> J {
    call(
        srv,
        id,
        "vgraph_commit",
        json!({ "graph": GRAPH, "message": msg }),
    )
}

fn prop_of(srv: &McpServer, id: &Identity, slug: &str) -> J {
    call(
        srv,
        id,
        "vgraph_show",
        json!({ "addr": format!("{GRAPH}#{slug}") }),
    )["props"]["v"]
        .clone()
}

fn pending(srv: &McpServer, id: &Identity) -> J {
    call(srv, id, "vgraph_pending", json!({ "graph": GRAPH }))
}

/// 노드 n0..n{count-1} 를 커밋해 둔 저장소와 그 위의 서버 하나.
fn seeded(count: usize) -> (TempDir, McpServer) {
    let dir = TempDir::new().unwrap();
    let srv = McpServer::new(config(dir.path())).expect("mcp 서버");
    let base = who(OWNER, None);
    call(&srv, &base, "vgraph_create", json!({ "name": GRAPH }));
    let ops: Vec<J> = (0..count)
        .map(|i| json!({ "op": "add_node", "labels": ["Item"], "slug": format!("n{i}"), "props": { "v": 0 } }))
        .collect();
    call(
        &srv,
        &base,
        "vgraph_edit",
        json!({ "graph": GRAPH, "ops": ops }),
    );
    commit(&srv, &base, "seed");
    (dir, srv)
}

#[test]
fn two_identities_on_one_server_both_edits_land() {
    let (_dir, srv) = seeded(2);
    let a = who(OWNER, Some("a"));
    let b = who(OWNER, Some("b"));
    set_prop(&srv, &a, "n0", 1);
    set_prop(&srv, &b, "n1", 2);

    // 번갈아 편집해도 서로의 pending 이 섞이지 않는다
    set_prop(&srv, &a, "n0", 11);
    let pa = pending(&srv, &a);
    let pb = pending(&srv, &b);
    assert_eq!(pa["pending"]["props_set"], json!(1), "a: {pa}");
    assert_eq!(pb["pending"]["props_set"], json!(1), "b: {pb}");
    assert_ne!(pa["working"], pb["working"]);
    // 기본 신원(작업 공간 없음)에는 편집이 보이지 않는다
    assert_eq!(pending(&srv, &who(OWNER, None))["working"], J::Null);

    commit(&srv, &a, "a");
    commit(&srv, &b, "b");
    let d = who(OWNER, None);
    assert_eq!(prop_of(&srv, &d, "n0"), json!(11));
    assert_eq!(prop_of(&srv, &d, "n1"), json!(2));
}

#[test]
fn concurrent_identities_lose_no_edit_and_do_not_leak_pending() {
    let (_dir, srv) = seeded(THREADS);
    // 모든 스레드가 같은 순간에 출발하고, 라운드마다 다시 모여 겹쳐서 돈다 (직렬로 끝나면 통과하는 구조 방지)
    let start = Barrier::new(THREADS);
    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let srv = &srv;
            let start = &start;
            scope.spawn(move || {
                let me = who(OWNER, Some(&format!("w{t}")));
                let node = format!("n{t}");
                for round in 1..=ROUNDS {
                    start.wait();
                    set_prop(srv, &me, &node, round);
                    // 커밋 전: 내 작업 공간에는 내 편집 하나만 쌓여 있다
                    let p = pending(srv, &me);
                    assert_eq!(p["pending"]["props_set"], json!(1), "t{t} r{round}: {p}");
                    let out = commit(srv, &me, &format!("t{t} r{round}"));
                    assert!(
                        matches!(out["outcome"].as_str(), Some("committed" | "rebased")),
                        "t{t} r{round}: {out}"
                    );
                    // 커밋 후: 비어 있다
                    let p = pending(srv, &me);
                    assert!(
                        p["working"].is_null() || p["pending"]["props_set"] == json!(0),
                        "t{t} r{round}: {p}"
                    );
                }
            });
        }
    });
    let d = who(OWNER, None);
    for t in 0..THREADS {
        assert_eq!(
            prop_of(&srv, &d, &format!("n{t}")),
            json!(ROUNDS),
            "n{t} 유실"
        );
    }
}

#[test]
fn acl_follows_identity_user() {
    let (_dir, srv) = seeded(1);
    let stranger = who("stranger", None);
    // 다른 사용자: 목록에 보이지 않고 편집·조회가 거부된다
    let list = call(&srv, &stranger, "vgraph_list", json!({}));
    assert_eq!(list["graphs"], json!([]), "{list}");
    let (is_err, body) = raw(
        &srv,
        &stranger,
        "vgraph_edit",
        json!({ "graph": GRAPH,
            "ops": [{ "op": "set_prop", "node": "n0", "key": "v", "value": 9 }] }),
    );
    assert!(is_err, "다른 사용자의 편집이 통과함: {body}");
    // 소유자는 같은 서버에서 그대로 편집한다
    let owner = who(OWNER, Some("x"));
    set_prop(&srv, &owner, "n0", 3);
    commit(&srv, &owner, "ok");
    assert_eq!(prop_of(&srv, &who(OWNER, None), "n0"), json!(3));
    let list = call(&srv, &who(OWNER, None), "vgraph_list", json!({}));
    assert_eq!(list["graphs"][0]["name"], json!(GRAPH), "{list}");
}

#[test]
fn invalid_workspace_name_is_rejected_with_existing_message() {
    for bad in ["", "a b", "../x", "한글"] {
        let err = Identity::new(OWNER, Some(bad.into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("작업 공간 이름은 영문·숫자·-·_ 만 쓸 수 있습니다"),
            "{bad:?}: {err}"
        );
    }
    // 서버 기본 신원 쪽 검사와 같은 문구
    let dir = TempDir::new().unwrap();
    let err = McpServer::new(config(dir.path()))
        .unwrap()
        .with_workspace(Some("a b".into()))
        .err()
        .expect("거부")
        .to_string();
    assert!(
        err.contains("작업 공간 이름은 영문·숫자·-·_ 만 쓸 수 있습니다"),
        "{err}"
    );
    assert!(Identity::new(OWNER, Some("ok-1_x".into())).is_ok());
}

#[test]
fn handle_message_without_identity_uses_server_default() {
    let (_dir, srv) = seeded(1);
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "vgraph_show", "arguments": { "addr": format!("{GRAPH}#n0") } } });
    let plain = srv.handle_message(&msg).expect("response");
    let as_default = srv
        .handle_message_as(&msg, &who(OWNER, None))
        .expect("response");
    assert_eq!(plain, as_default);
    // handle_line 도 같은 경로
    let line = srv.handle_line(&msg.to_string()).expect("line");
    assert_eq!(serde_json::from_str::<J>(&line).unwrap(), plain);
}

// ---------- 링크 대상 확인 ----------
//
// `normalize_ref` 의 "링크 대상 그래프 미등록" 판정은 `Session::graph_id` 가 맡는데, 이는 엔진의
// 이름 등록부(by_name)를 사용자와 무관하게 조회한다(vgraph-engine/src/engine.rs). 따라서 이 판정
// 결과는 신원의 사용자에 따라 갈리지 않는다. 아래 시험은 그 계약(다른 사용자의 그래프도 등록돼
// 있으면 링크 대상이 된다, 미등록이면 거부)을 고정한다. 사용자 의존이 없으므로 "요청 신원 대신 서버
// 기본 사용자"로 바꾼 변이는 관측으로 구분되지 않는다(동치 변이).

fn add_linked_node(srv: &McpServer, id: &Identity, target: &str) -> (bool, J) {
    raw(
        srv,
        id,
        "vgraph_edit",
        json!({ "graph": GRAPH,
            "ops": [{ "op": "add_node", "labels": ["Item"], "slug": "linker",
                "props": { "link": { "$ref": target } } }] }),
    )
}

#[test]
fn link_target_check_is_by_registered_name_not_by_user() {
    let (_dir, srv) = seeded(1);
    let stranger = who("stranger", None);
    call(
        &srv,
        &stranger,
        "vgraph_create",
        json!({ "name": "other/shared" }),
    );

    // 소유자가 다른 사용자의 그래프로 링크를 건다 — 등록돼 있으므로 통과
    let (is_err, body) = add_linked_node(&srv, &who(OWNER, Some("x")), "other/shared#t");
    assert!(!is_err, "{body}");
    // 같은 링크를 다른 사용자 신원이 걸어도 판정은 같다 (편집 권한은 별개: 소유자 전용이라 거부됨)
    let (is_err, body) = add_linked_node(&srv, &stranger, "other/shared#t");
    assert!(is_err && !body.to_string().contains("미등록"), "{body}");
    // 미등록 대상은 어떤 신원이든 같은 이유로 거부
    let (is_err, body) = add_linked_node(&srv, &who(OWNER, Some("y")), "other/missing#t");
    assert!(
        is_err && body.to_string().contains("링크 대상 그래프 미등록"),
        "{body}"
    );
}

// ---------- 가져오기의 작업 공간 ----------

fn doc(slug: &str) -> J {
    json!({ "format": "vgraph-json/1",
        "nodes": [{ "slug": slug, "labels": ["Item"], "props": { "n": 1 } }], "edges": [] })
}

fn add_pending(srv: &McpServer, id: &Identity, graph: &str, slug: &str) {
    call(
        srv,
        id,
        "vgraph_edit",
        json!({ "graph": graph,
            "ops": [{ "op": "add_node", "labels": ["Item"], "slug": slug, "props": {} }] }),
    );
}

fn pending_nodes(srv: &McpServer, id: &Identity, graph: &str) -> J {
    call(srv, id, "vgraph_pending", json!({ "graph": graph }))["pending"]["nodes_added"].clone()
}

const PENDING_MSG: &str = "미커밋 편집";

#[test]
fn import_replace_follows_request_workspace() {
    let (_dir, srv) = seeded(0);
    let g = "acme/imp";
    call(
        &srv,
        &who(OWNER, None),
        "vgraph_create",
        json!({ "name": g }),
    );
    let a = who(OWNER, Some("a"));
    let b = who(OWNER, Some("b"));
    add_pending(&srv, &a, g, "mine");

    // a 의 미커밋 편집은 a 의 가져오기만 막는다
    let (is_err, body) = raw(
        &srv,
        &a,
        "vgraph_import",
        json!({ "graph": g, "document": doc("x") }),
    );
    assert!(
        is_err && body.to_string().contains(PENDING_MSG),
        "a: {body}"
    );
    // 기본 작업 공간·다른 작업 공간은 a 의 편집을 보지 않으므로 가져오기가 된다
    let r = call(
        &srv,
        &b,
        "vgraph_import",
        json!({ "graph": g, "document": doc("x") }),
    );
    assert_eq!(r["outcome"], "committed", "{r}");
    // a 의 미커밋 편집은 그대로 남아 있다
    assert_eq!(pending_nodes(&srv, &a, g), json!(1));
}

#[test]
fn import_replace_default_identity_ignores_other_workspace_pending() {
    let (_dir, srv) = seeded(0);
    let g = "acme/imp2";
    call(
        &srv,
        &who(OWNER, None),
        "vgraph_create",
        json!({ "name": g }),
    );
    add_pending(&srv, &who(OWNER, Some("a")), g, "mine");
    let r = call(
        &srv,
        &who(OWNER, None),
        "vgraph_import",
        json!({ "graph": g, "document": doc("x") }),
    );
    assert_eq!(r["outcome"], "committed", "{r}");
    // 기본 신원의 미커밋 편집이 있으면 기본 신원은 막힌다
    let g2 = "acme/imp3";
    call(
        &srv,
        &who(OWNER, None),
        "vgraph_create",
        json!({ "name": g2 }),
    );
    add_pending(&srv, &who(OWNER, None), g2, "mine");
    let (is_err, body) = raw(
        &srv,
        &who(OWNER, None),
        "vgraph_import",
        json!({ "graph": g2, "document": doc("x") }),
    );
    assert!(is_err && body.to_string().contains(PENDING_MSG), "{body}");
}

#[test]
fn import_merge_first_import_follows_request_workspace() {
    let (_dir, srv) = seeded(0);
    let g = "acme/mrg1";
    call(
        &srv,
        &who(OWNER, None),
        "vgraph_create",
        json!({ "name": g }),
    );
    let a = who(OWNER, Some("a"));
    let b = who(OWNER, Some("b"));
    add_pending(&srv, &a, g, "mine");
    let args = json!({ "graph": g, "document": doc("x"), "mode": "merge", "via_branch": "feed" });

    let (is_err, body) = raw(&srv, &a, "vgraph_import", args.clone());
    assert!(
        is_err && body.to_string().contains(PENDING_MSG),
        "a: {body}"
    );
    let r = call(&srv, &b, "vgraph_import", args);
    assert_eq!(r["outcome"], "first_import", "{r}");
    assert_eq!(pending_nodes(&srv, &a, g), json!(1));
}

#[test]
fn import_merge_sync_follows_request_workspace() {
    let (_dir, srv) = seeded(0);
    let g = "acme/mrg2";
    call(
        &srv,
        &who(OWNER, None),
        "vgraph_create",
        json!({ "name": g }),
    );
    let a = who(OWNER, Some("a"));
    let b = who(OWNER, Some("b"));
    let first = json!({ "graph": g, "document": doc("x"), "mode": "merge", "via_branch": "feed" });
    let r = call(&srv, &b, "vgraph_import", first);
    assert_eq!(r["outcome"], "first_import", "{r}");

    // a 가 via 브랜치에 미커밋 편집을 쌓아 둔다
    let via = format!("{g}@feed");
    add_pending(&srv, &a, &via, "mine");
    let second = json!({ "graph": g, "document": doc("y"), "mode": "merge", "via_branch": "feed" });

    // 같은 작업 공간(a)의 동기화는 막히고, 다른 작업 공간(b)은 a 의 편집을 건드리지 않고 진행한다
    let (is_err, body) = raw(&srv, &a, "vgraph_import", second.clone());
    assert!(
        is_err && body.to_string().contains(PENDING_MSG),
        "a: {body}"
    );
    let r = call(&srv, &b, "vgraph_import", second);
    assert_ne!(r["outcome"], "first_import", "{r}");
    assert_eq!(pending_nodes(&srv, &a, &via), json!(1));
}
