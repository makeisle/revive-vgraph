//! MCP 계층 골든 — 프로토콜(initialize/tools) + 전체 시나리오를 도구 경유로 관통.
//! handle_message가 순수 함수라 stdio 없이 결정적으로 검증한다.

use serde_json::{json, Value as J};
use std::sync::Arc;
use tempfile::TempDir;
use vgraph_core::{TestClock, TestIdGen};
use vgraph_engine::{Engine, EngineOptions, FsProvider};
use vgraph_mcp::{McpConfig, McpServer};
use vgraph_vec::EmbedSpec;

fn server() -> (TempDir, McpServer) {
    let dir = TempDir::new().unwrap();
    let engine = Engine::open(
        Box::new(FsProvider::new(dir.path()).unwrap()),
        Arc::new(TestIdGen::new()),
        Arc::new(TestClock::default()),
        EngineOptions::default(),
    )
    .unwrap();
    let cfg = McpConfig {
        root: dir.path().to_path_buf(),
        user: "ceo".into(),
        embedder: Some("toy".into()),
        embed: EmbedSpec::default(),
    };
    (dir, McpServer::with_engine(engine, cfg))
}

fn rpc(srv: &McpServer, method: &str, params: J) -> J {
    srv.handle_message(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .expect("response expected")
}

/// tools/call → content[0].text(JSON) 파싱. isError면 panic.
fn call(srv: &McpServer, name: &str, args: J) -> J {
    let resp = rpc(
        srv,
        "tools/call",
        json!({ "name": name, "arguments": args }),
    );
    let result = &resp["result"];
    assert_ne!(result, &J::Null, "no result: {resp}");
    assert!(
        !result["isError"].as_bool().unwrap_or(false),
        "tool error: {}",
        result["content"][0]["text"]
    );
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn call_err(srv: &McpServer, name: &str, args: J) -> String {
    let resp = rpc(
        srv,
        "tools/call",
        json!({ "name": name, "arguments": args }),
    );
    assert!(
        resp["result"]["isError"].as_bool().unwrap_or(false),
        "expected error: {resp}"
    );
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn protocol_basics() {
    let (_d, srv) = server();
    // initialize — protocolVersion echo + 서버 전역 instructions (자기 기술)
    let resp = rpc(
        &srv,
        "initialize",
        json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {} }),
    );
    assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(resp["result"]["serverInfo"]["name"], "vgraph-mcp");
    let instructions = resp["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("$ref")); // Ref 규약이 전역 안내에 존재 (UX-1)
    assert!(instructions.contains("embed_target"));
    // tools/list
    let tools = rpc(&srv, "tools/list", json!({}));
    let list = tools["result"]["tools"].as_array().unwrap();
    assert!(list.iter().all(|t| t["inputSchema"]["type"] == "object"));
    // 알 수 없는 메서드 → -32601
    let err = rpc(&srv, "no/such", json!({}));
    assert_eq!(err["error"]["code"], -32601);
    // notification (id 없음) → 무응답
    assert!(srv
        .handle_message(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .is_none());
    // ping
    assert_eq!(rpc(&srv, "ping", json!({}))["result"], json!({}));
}

#[test]
fn full_scenario_via_mcp() {
    let (_d, srv) = server();

    // 생성 + 편집(배치) + 커밋
    call(&srv, "vgraph_create", json!({ "name": "acme/bookclub" }));
    let r = call(
        &srv,
        "vgraph_edit",
        json!({
            "graph": "acme/bookclub",
            "ops": [
                { "op": "add_node", "labels": ["Capability"], "slug": "cap-auth",
                  "props": { "name": "회원 인증", "status": "active", "priority": 1 } },
                { "op": "add_node", "labels": ["Capability"], "slug": "cap-loan",
                  "props": { "name": "도서 대출", "status": "active" } },
                { "op": "add_node", "labels": ["Requirement"], "slug": "req-001",
                  "props": { "text": "이메일 로그인", "status": "draft" } },
                { "op": "add_edge", "etype": "parent", "from": "req-001", "to": "cap-auth" },
            ],
        }),
    );
    assert_eq!(r["applied"], 4);
    assert_eq!(r["pending"]["nodes_added"], 3);

    // pending은 호출 간 유지 (WAL resume)
    let p = call(&srv, "vgraph_pending", json!({ "graph": "acme/bookclub" }));
    assert_eq!(p["pending"]["nodes_added"], 3);

    let c = call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/bookclub", "message": "F0" }),
    );
    assert_eq!(c["outcome"], "committed");
    let c0 = c["commit"].as_str().unwrap().to_string();

    // 조회 — epoch 동반
    let show = call(&srv, "vgraph_show", json!({ "addr": "acme/bookclub" }));
    assert_eq!(show["node_count"], 3);
    assert_eq!(show["epoch"].as_str().unwrap(), c0);
    let node = call(
        &srv,
        "vgraph_show",
        json!({ "addr": "acme/bookclub#cap-auth" }),
    );
    assert_eq!(node["props"]["name"], "회원 인증");

    // 검색(속성) + 순회
    let hits = call(
        &srv,
        "vgraph_search",
        json!({ "graph": "acme/bookclub", "mode": "prop", "key": "status", "value": "active" }),
    );
    assert_eq!(hits["hits"].as_array().unwrap().len(), 2);
    let nb = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": "acme/bookclub#req-001", "dir": "out" }),
    );
    assert_eq!(nb["hits"][0]["slug"], "cap-auth");

    // 브랜치 + 양쪽 편집 → 충돌 → theirs 해소
    call(
        &srv,
        "vgraph_branch",
        json!({ "graph": "acme/bookclub", "name": "dev-b" }),
    );
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/bookclub@dev-b",
        "ops": [{ "op": "set_prop", "node": "cap-auth", "key": "status", "value": "deprecated" }] }),
    );
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/bookclub@dev-b", "message": "dev" }),
    );
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/bookclub",
        "ops": [{ "op": "set_prop", "node": "cap-auth", "key": "status", "value": "beta" }] }),
    );
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/bookclub", "message": "main" }),
    );

    let m = call(
        &srv,
        "vgraph_merge",
        json!({ "graph": "acme/bookclub", "src": "dev-b", "dst": "main" }),
    );
    assert_eq!(m["outcome"], "conflicts");
    let item = &m["report"]["items"][0];
    assert_eq!(item["kind"], "prop_both_set");
    assert_eq!(item["index"], 0);
    assert_eq!(item["ours"], "beta");
    assert_eq!(item["theirs"], "deprecated");

    let m2 = call(
        &srv,
        "vgraph_merge",
        json!({ "graph": "acme/bookclub", "src": "dev-b", "dst": "main", "theirs": [0] }),
    );
    assert_eq!(m2["outcome"], "merged");

    // 머지 후 값 + log 계보
    let node = call(
        &srv,
        "vgraph_show",
        json!({ "addr": "acme/bookclub#cap-auth" }),
    );
    assert_eq!(node["props"]["status"], "deprecated");
    let log = call(&srv, "vgraph_log", json!({ "graph": "acme/bookclub" }));
    let commits = log["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 4); // merge, dev, main, F0
    assert_eq!(commits[0]["merge"], true);

    // diff: C0 → head
    let d = call(
        &srv,
        "vgraph_diff",
        json!({
        "from": format!("acme/bookclub.{c0}"), "to": "acme/bookclub" }),
    );
    assert_eq!(d["props_set"][0]["key"], "status");

    // 유사도: 큐 소화 → similar 검색
    let e = call(
        &srv,
        "vgraph_embed_pending",
        json!({ "graph": "acme/bookclub" }),
    );
    assert_eq!(e["embedded"], 3);
    let sim = call(
        &srv,
        "vgraph_search",
        json!({ "graph": "acme/bookclub", "mode": "similar", "query": "이메일 로그인", "k": 2 }),
    );
    assert_eq!(sim["hits"][0]["slug"], "req-001");
    assert_eq!(sim["pending_embeddings"], 0);

    // links / admin 스모크
    let lock = call(
        &srv,
        "vgraph_links",
        json!({ "graph": "acme/bookclub", "action": "lockfile" }),
    );
    assert_eq!(lock["lockfile"].as_array().unwrap().len(), 0);
    let gc = call(
        &srv,
        "vgraph_admin",
        json!({ "graph": "acme/bookclub", "action": "gc_plan" }),
    );
    assert_eq!(gc["orphan_blobs"], 0);

    // 오류 경로: 미등록 그래프
    let err = call_err(&srv, "vgraph_show", json!({ "addr": "acme/nope" }));
    assert!(err.contains("미등록") || err.contains("unknown"));
}

/// P5 잔여 항목 해소: WAL working copy가 **서버 인스턴스 재시작(새 Engine, 같은 root)**을
/// 넘어 resume 되는지 — 지금까지는 같은 세션 내 다중 호출만 검증돼 있었다.
/// (프로세스 재시작의 저장소 레벨 등가물: Engine을 버리고 같은 root로 새로 연다.)
#[test]
fn wal_resume_across_engine_restart() {
    let dir = TempDir::new().unwrap();
    let mk = || {
        let engine = Engine::open(
            Box::new(FsProvider::new(dir.path()).unwrap()),
            Arc::new(TestIdGen::new()),
            Arc::new(TestClock::default()),
            EngineOptions::default(),
        )
        .unwrap();
        McpServer::with_engine(
            engine,
            McpConfig {
                root: dir.path().to_path_buf(),
                user: "ceo".into(),
                embedder: None,
                embed: EmbedSpec::default(),
            },
        )
    };

    // 인스턴스 1: 그래프 생성 + 편집(커밋 안 함) → drop.
    let srv1 = mk();
    call(&srv1, "vgraph_create", json!({ "name": "acme/restart" }));
    let r = call(
        &srv1,
        "vgraph_edit",
        json!({ "graph": "acme/restart", "ops": [
        { "op": "add_node", "labels": ["Note"], "slug": "n1", "props": { "t": "본문" } } ] }),
    );
    assert_eq!(r["pending"]["nodes_added"], 1);
    drop(srv1);

    // 인스턴스 2 (재시작): pending이 WAL에서 resume → commit → 조회까지 성립.
    let srv2 = mk();
    let p = call(&srv2, "vgraph_pending", json!({ "graph": "acme/restart" }));
    assert_eq!(
        p["pending"]["nodes_added"], 1,
        "WAL resume across restart: {p}"
    );
    let c = call(
        &srv2,
        "vgraph_commit",
        json!({ "graph": "acme/restart", "message": "resumed" }),
    );
    assert_eq!(c["outcome"], "committed");
    let show = call(&srv2, "vgraph_show", json!({ "addr": "acme/restart#n1" }));
    assert_eq!(show["props"]["t"], "본문");
}

/// UX 개선 검증 (라이브 테스트 결함 반영): Ref 축약 보정·조기 검증·시스템 노드·target_head.
#[test]
fn ux_improvements() {
    let (_d, srv) = server();
    // 대상 그래프 + 노드 준비
    call(&srv, "vgraph_create", json!({ "name": "acme/policy" }));
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/policy", "ops": [
        { "op": "add_node", "labels": ["Policy"], "slug": "sla",
          "props": { "name": "외부 SLA", "rev": 1 } } ] }),
    );
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/policy", "message": "B0" }),
    );
    call(&srv, "vgraph_create", json!({ "name": "acme/kb" }));
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/kb", "ops": [
        { "op": "add_node", "labels": ["Doc"], "slug": "d1", "props": { "t": "본문" } } ] }),
    );
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/kb", "message": "c0" }),
    );

    // UX-2: 미등록 대상 링크 → edit 시점 즉시 명확한 오류 (commit까지 안 감)
    let err = call_err(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/kb", "ops": [
        { "op": "set_prop", "node": "d1", "key": "extern",
          "value": { "$ref": "acme/nope#x" } } ] }),
    );
    assert!(err.contains("미등록"), "{err}");
    assert!(!err.contains("corrupt"), "{err}");

    // UX-1: 축약 표기(ns/slug#node) 자동 보정 → commit 성공 + lockfile 핀
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/kb", "ops": [
        { "op": "set_prop", "node": "d1", "key": "extern",
          "value": { "$ref": "acme/policy#sla" } } ] }),
    );
    let c = call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/kb", "message": "link" }),
    );
    assert_eq!(c["outcome"], "committed");
    let lock = call(
        &srv,
        "vgraph_links",
        json!({ "graph": "acme/kb", "action": "lockfile" }),
    );
    assert_eq!(lock["lockfile"].as_array().unwrap().len(), 1);

    // UX-4: show가 시스템 노드(__lockfile)를 분리 표기 — 사용자 노드 수는 1
    let show = call(&srv, "vgraph_show", json!({ "addr": "acme/kb" }));
    assert_eq!(show["node_count"], 1);
    assert_eq!(show["system_nodes"], 1);
    assert!(show["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["system"] == json!(true)));

    // UX-3: 대상 전진 → drifts에 target_head 포함 → 그 값으로 upgrade 성립
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/policy", "ops": [
        { "op": "set_prop", "node": "sla", "key": "rev", "value": 2 } ] }),
    );
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/policy", "message": "B1" }),
    );
    let drifts = call(
        &srv,
        "vgraph_links",
        json!({ "graph": "acme/kb", "action": "drifts" }),
    );
    let head = drifts["drifts"][0]["target_head"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(head.starts_with("sha256:"));
    let up = call(
        &srv,
        "vgraph_links",
        json!({ "graph": "acme/kb", "action": "upgrade",
        "target": "acme/policy", "to": head }),
    );
    assert_eq!(up["outcome"], "committed");
    let drifts2 = call(
        &srv,
        "vgraph_links",
        json!({ "graph": "acme/kb", "action": "drifts" }),
    );
    assert_eq!(drifts2["drifts"].as_array().unwrap().len(), 0);
}
