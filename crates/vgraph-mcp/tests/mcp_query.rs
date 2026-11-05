//! 검색 기능(R1~R5)의 MCP 계층 시험 — Step 1: 기준선 고정.
//!
//! # 겹층 모양 픽스처 `layered_fixture(&srv)` (Step 2~5가 재사용한다)
//!
//! 그래프 `acme/layered` (기존 도구 vgraph_create/edit/commit 만으로 구성, 커밋 1개).
//!
//! 노드 (slug / 라벨 / 속성):
//! - `orders/{id} 화면` / page / name="주문 조회", method="GET",
//!   tags=["주문","조회"], desc="Order 를 ID 로 조회한다"   <- 시작 노드
//! - `CAT-1`   / category  / title="주문 관리", keywords=["주문","order"]
//! - `Order`   / entity    / name="Order", note="주문 엔티티"
//! - `CHK-1`   / check     / text="주문 ID 는 양수여야 한다"
//! - `PRV-1`   / privacy   / text="주문 조회는 본인 것만 허용"
//! - `LOG-9`   / log       / text="CHK-1 에서 뻗는 2단계 (확장 라벨 경유)"
//! - `NOTE-P`  / doc       / text="PRV-1 에서 뻗는 2단계 (확장 라벨 경유)"
//! - `CAT-2`   / category  / title="CAT-1 에서만 닿는 2단계 (확장 라벨 아님 경유)"
//! - `Customer`/ entity    / name="Customer" — Order 에서만 닿는 2단계 (확장 라벨 아님 경유)
//!
//! 엣지 (모두 시작 노드에서 out 방향):
//! - 시작 -분류-> CAT-1, 시작 -엔티티-> Order, 시작 -점검-> CHK-1, 시작 -점검-> PRV-1
//! - CHK-1 -기록-> LOG-9, PRV-1 -기록-> NOTE-P
//! - CAT-1 -관련-> CAT-2, Order -관련-> Customer
//!
//! 확장 라벨 노드 확장(1단계 전부, 2단계는 확장 라벨 check/privacy/log/audit 노드에서만 뻗음)의 기대 결과:
//! 1단계 = {CAT-1, Order, CHK-1, PRV-1}, 2단계 = {LOG-9, NOTE-P}.
//! **CAT-2 와 Customer 는 나오면 안 된다** (확장 라벨이 아닌 노드에서만 닿음).

use serde_json::{json, Value as J};
use std::collections::BTreeSet;
use std::sync::Arc;
use tempfile::TempDir;
use vgraph_core::{TestClock, TestIdGen};
use vgraph_engine::{Engine, EngineOptions, FsProvider};
use vgraph_mcp::{tool_defs, McpConfig, McpServer};
use vgraph_vec::EmbedSpec;

const LAYERED: &str = "acme/layered";
const START_SLUG: &str = "orders/{id} 화면";

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

/// tools/call → content[0].text(JSON) 파싱. isError 면 panic.
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

/// 오류를 기대하는 호출 — 오류 메시지 반환. 성공하면 panic.
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

fn node(slug: &str, label: &str, props: J) -> J {
    json!({ "op": "add_node", "labels": [label], "slug": slug, "props": props })
}

fn edge(etype: &str, from: &str, to: &str) -> J {
    json!({ "op": "add_edge", "etype": etype, "from": from, "to": to })
}

/// 겹층 모양 픽스처를 만들고 커밋한다. 구성은 파일 상단 주석 참조.
fn layered_fixture(srv: &McpServer) {
    call(srv, "vgraph_create", json!({ "name": LAYERED }));
    let ops = json!([
        node(
            START_SLUG,
            "page",
            json!({
            "name": "주문 조회", "method": "GET",
            "tags": ["주문", "조회"], "desc": "Order 를 ID 로 조회한다" })
        ),
        node(
            "CAT-1",
            "category",
            json!({ "title": "주문 관리", "keywords": ["주문", "order"] })
        ),
        node(
            "Order",
            "entity",
            json!({ "name": "Order", "note": "주문 엔티티" })
        ),
        node(
            "CHK-1",
            "check",
            json!({ "text": "주문 ID 는 양수여야 한다" })
        ),
        node(
            "PRV-1",
            "privacy",
            json!({ "text": "주문 조회는 본인 것만 허용" })
        ),
        node(
            "LOG-9",
            "log",
            json!({ "text": "CHK-1 에서 뻗는 2단계 (확장 라벨 경유)" })
        ),
        node(
            "NOTE-P",
            "doc",
            json!({ "text": "PRV-1 에서 뻗는 2단계 (확장 라벨 경유)" })
        ),
        node(
            "CAT-2",
            "category",
            json!({ "title": "CAT-1 에서만 닿는 2단계 (확장 라벨 아님 경유)" })
        ),
        node("Customer", "entity", json!({ "name": "Customer" })),
        edge("분류", START_SLUG, "CAT-1"),
        edge("엔티티", START_SLUG, "Order"),
        edge("점검", START_SLUG, "CHK-1"),
        edge("점검", START_SLUG, "PRV-1"),
        edge("기록", "CHK-1", "LOG-9"),
        edge("기록", "PRV-1", "NOTE-P"),
        edge("관련", "CAT-1", "CAT-2"),
        edge("관련", "Order", "Customer"),
    ]);
    let r = call(srv, "vgraph_edit", json!({ "graph": LAYERED, "ops": ops }));
    assert_eq!(r["applied"], 17);
    let c = call(
        srv,
        "vgraph_commit",
        json!({ "graph": LAYERED, "message": "layered fixture" }),
    );
    assert_eq!(c["outcome"], "committed", "{c}");
}

// ───────────────────────── slug 왕복 ─────────────────────────

/// 공백·`/`·`{}`·한글이 든 slug 가 edit → commit → show 로 그대로 돌아오는지.
#[test]
fn slug_roundtrip_special_chars() {
    let (_d, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/slug" }));
    let slugs = [START_SLUG, "주문 조회", "a b/c{d}한글"];
    let ops: Vec<J> = slugs
        .iter()
        .map(|s| {
            node(
                s,
                "page",
                json!({ "name": format!("속성 {s}"), "tags": ["가", "나"] }),
            )
        })
        .collect();
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/slug", "ops": ops }),
    );
    let c = call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/slug", "message": "slugs" }),
    );
    assert_eq!(c["outcome"], "committed", "{c}");

    for s in slugs {
        let n = call(
            &srv,
            "vgraph_show",
            json!({ "addr": format!("acme/slug#{s}") }),
        );
        assert_eq!(n["slug"], s, "slug 불일치: {n}");
        assert_eq!(n["labels"], json!(["page"]), "라벨 불일치 ({s}): {n}");
        assert_eq!(
            n["props"]["name"],
            format!("속성 {s}"),
            "속성 불일치 ({s}): {n}"
        );
        assert_eq!(
            n["props"]["tags"],
            json!(["가", "나"]),
            "리스트 속성 불일치 ({s}): {n}"
        );
    }
}

/// 픽스처 자체가 만들어지고 시작 노드를 slug 로 되읽을 수 있다.
#[test]
fn layered_fixture_builds_and_reads_back() {
    let (_d, srv) = server();
    layered_fixture(&srv);
    let show = call(&srv, "vgraph_show", json!({ "addr": LAYERED }));
    assert_eq!(show["node_count"], 9);
    assert_eq!(show["edge_count"], 8);
    let n = call(
        &srv,
        "vgraph_show",
        json!({ "addr": format!("{LAYERED}#{START_SLUG}") }),
    );
    assert_eq!(n["slug"], START_SLUG);
    assert_eq!(n["labels"], json!(["page"]));
    assert_eq!(n["props"]["tags"], json!(["주문", "조회"]));
}

// ───────────────────────── 회귀 TC ─────────────────────────

/// 새 인자 없이 vgraph_neighbors 를 부르면 응답이 고정 기댓값과 같다 (제약-1).
#[test]
fn neighbors_legacy_args_unchanged() {
    let (_d, srv) = server();
    layered_fixture(&srv);
    let got = call(
        &srv,
        "vgraph_neighbors",
        json!({
        "addr": format!("{LAYERED}#{START_SLUG}"), "dir": "out", "depth": 2 }),
    );
    let expected: J = serde_json::from_str(EXPECTED_NEIGHBORS).unwrap();
    assert_eq!(got, expected, "실제 응답: {got}");
}

/// 기존 16종이 모두 있고 각 required 집합이 그대로다. 전체 개수는 단언하지 않는다.
#[test]
fn legacy_tool_defs_unchanged() {
    let legacy: [(&str, &[&str]); 16] = [
        ("vgraph_list", &[]),
        ("vgraph_create", &["name"]),
        ("vgraph_show", &["addr"]),
        ("vgraph_search", &["graph"]),
        ("vgraph_neighbors", &["addr"]),
        ("vgraph_edit", &["graph", "ops"]),
        ("vgraph_pending", &["graph"]),
        ("vgraph_commit", &["graph", "message"]),
        ("vgraph_discard", &["graph"]),
        ("vgraph_branch", &["graph", "name"]),
        ("vgraph_merge", &["graph", "src", "dst"]),
        ("vgraph_log", &["graph"]),
        ("vgraph_diff", &["from", "to"]),
        ("vgraph_links", &["graph", "action"]),
        ("vgraph_admin", &["graph", "action"]),
        ("vgraph_embed_pending", &["graph"]),
    ];
    let defs = tool_defs();
    for (name, required) in legacy {
        let def = defs
            .iter()
            .find(|d| d["name"] == name)
            .unwrap_or_else(|| panic!("기존 도구 누락: {name}"));
        let mut got: Vec<String> = def["inputSchema"]["required"]
            .as_array()
            .unwrap_or_else(|| panic!("required 없음: {name}"))
            .iter()
            .map(|r| r.as_str().unwrap().to_string())
            .collect();
        got.sort();
        let mut want: Vec<String> = required.iter().map(|r| r.to_string()).collect();
        want.sort();
        assert_eq!(got, want, "required 변경됨: {name}");
    }
}

/// 오류 경로: 시작 노드 없는 주소·없는 slug 는 오류로 알린다 (기존 동작 고정).
#[test]
fn neighbors_legacy_errors_unchanged() {
    let (_d, srv) = server();
    layered_fixture(&srv);
    let no_node = call_err(&srv, "vgraph_neighbors", json!({ "addr": LAYERED }));
    assert!(no_node.contains("시작 노드 필요"), "{no_node}");
    let unknown = call_err(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": format!("{LAYERED}#없는노드") }),
    );
    assert!(!unknown.is_empty());
}

/// 노드 250개 그래프에서 vgraph_show 그래프 요약의 노드 목록은 여전히 200개에서 잘린다.
#[test]
fn show_summary_still_truncates_200() {
    let (_d, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/big" }));
    let ops: Vec<J> = (0..250)
        .map(|i| node(&format!("n{i:03}"), "Note", json!({ "i": i })))
        .collect();
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/big", "ops": ops }),
    );
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/big", "message": "250" }),
    );

    let show = call(&srv, "vgraph_show", json!({ "addr": "acme/big" }));
    assert_eq!(show["node_count"], 250);
    assert_eq!(show["nodes"].as_array().unwrap().len(), 200);
    assert_eq!(show["truncated"], true);
}

// ───────────────────────── Step 3: vgraph_find · vgraph_scan · neighbors 확장 ─────────────────────────

fn slugs_at(hits: &J, depth: u64) -> BTreeSet<String> {
    hits.as_array()
        .unwrap()
        .iter()
        .filter(|h| h["depth"] == depth)
        .map(|h| h["slug"].as_str().unwrap().to_string())
        .collect()
}

fn set_of(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|x| x.to_string()).collect()
}

fn hit<'a>(hits: &'a J, slug: &str) -> &'a J {
    hits.as_array()
        .unwrap()
        .iter()
        .find(|h| h["slug"] == slug)
        .unwrap_or_else(|| panic!("hit 없음: {slug} in {hits}"))
}

/// [핵심] 도구 호출 2번(find 1 + neighbors 1)으로 겹층 조회 결과가 라벨·속성·via_type과 함께 온다.
/// 기대: 1단계 {CAT-1, Order, CHK-1, PRV-1}, 2단계 {LOG-9, NOTE-P}, CAT-2·Customer 는 없음.
#[test]
fn two_calls_find_then_neighbors() {
    let (_d, srv) = server();
    layered_fixture(&srv);

    // 호출 1: 시작 노드 찾기
    let found = call(
        &srv,
        "vgraph_find",
        json!({ "graph": LAYERED, "labels": ["page"], "text": "주문",
                "include": ["labels", "props"] }),
    );
    let items = found["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{found}");
    assert_eq!(items[0]["slug"], START_SLUG);
    assert_eq!(items[0]["matched_key"], "name", "{found}");
    assert_eq!(items[0]["excerpt"], "주문 조회", "{found}");
    assert_eq!(items[0]["labels"], json!(["page"]));
    assert_eq!(items[0]["props"]["method"], "GET");
    assert_eq!(found["done"], true);
    assert_eq!(found["next_cursor"], J::Null);
    let start = items[0]["slug"].as_str().unwrap();

    // 호출 2: 확장 라벨 노드 확장 순회
    let walked = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": format!("{LAYERED}#{start}"), "depth": 2,
                "expand_labels": ["check", "privacy", "log", "audit"],
                "include": ["labels", "props"] }),
    );
    assert_eq!(walked["truncated"], false);
    let hits = &walked["hits"];
    assert_eq!(
        slugs_at(hits, 1),
        set_of(&["CAT-1", "Order", "CHK-1", "PRV-1"]),
        "{walked}"
    );
    assert_eq!(slugs_at(hits, 2), set_of(&["LOG-9", "NOTE-P"]), "{walked}");
    for forbidden in ["CAT-2", "Customer"] {
        assert!(
            hits.as_array()
                .unwrap()
                .iter()
                .all(|h| h["slug"] != forbidden),
            "{forbidden} 은 나오면 안 됨: {walked}"
        );
    }
    let chk = hit(hits, "CHK-1");
    assert_eq!(chk["labels"], json!(["check"]));
    assert_eq!(chk["props"]["text"], "주문 ID 는 양수여야 한다");
    assert_eq!(chk["via_type"], "점검");
    assert_eq!(hit(hits, "LOG-9")["via_type"], "기록");
    assert_eq!(hit(hits, "LOG-9")["labels"], json!(["log"]));
    // 기본 필드는 기존과 같은 이름
    for k in ["depth", "uuid", "slug", "via"] {
        assert!(!chk[k].is_null(), "기본 필드 {k} 누락: {chk}");
    }
}

/// 새 인자 경로에서 필터를 하나도 걸지 않으면(큰 limit 만) 기존 경로와 같은 hit 열이다(via_type 제외).
#[test]
fn neighbors_new_path_without_filters_matches_legacy() {
    let (_d, srv) = server();
    layered_fixture(&srv);
    let addr = format!("{LAYERED}#{START_SLUG}");
    let legacy = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": addr, "depth": 2 }),
    );
    let mut new = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": addr, "depth": 2, "limit": 100 }),
    );
    assert_eq!(new["truncated"], false);
    for h in new["hits"].as_array_mut().unwrap() {
        assert!(h["via_type"].is_string());
        h.as_object_mut().unwrap().remove("via_type");
        assert!(
            h.get("labels").is_none() && h.get("props").is_none(),
            "include 없으면 내용 없음: {h}"
        );
    }
    assert_eq!(new["hits"], legacy["hits"]);
    assert_eq!(new["epoch"], legacy["epoch"]);
}

/// etypes 가 etype 보다 우선하고, emit_labels 는 결과에 낼 노드만 거른다(뻗는 것과 별개).
#[test]
fn neighbors_etypes_precedence_and_emit_filter() {
    let (_d, srv) = server();
    layered_fixture(&srv);
    let addr = format!("{LAYERED}#{START_SLUG}");
    let r = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": addr, "etype": "분류", "etypes": ["엔티티"] }),
    );
    assert_eq!(slugs_at(&r["hits"], 1), set_of(&["Order"]), "{r}");

    let two = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": addr, "etypes": ["분류", "엔티티"] }),
    );
    assert_eq!(
        slugs_at(&two["hits"], 1),
        set_of(&["CAT-1", "Order"]),
        "{two}"
    );

    // emit 은 log/doc 만, 뻗기는 check/privacy 만 → 2단계의 LOG-9, NOTE-P 만 나온다
    let emit = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": addr, "depth": 2, "emit_labels": ["log", "doc"],
                "expand_labels": ["check", "privacy"] }),
    );
    assert_eq!(
        slugs_at(&emit["hits"], 2),
        set_of(&["LOG-9", "NOTE-P"]),
        "{emit}"
    );
    assert!(slugs_at(&emit["hits"], 1).is_empty(), "{emit}");

    // prop_keys: 지정한 키만
    let keyed = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": addr, "include": ["props"], "prop_keys": ["title"],
                "etypes": ["분류"] }),
    );
    assert_eq!(
        keyed["hits"][0]["props"],
        json!({ "title": "주문 관리" }),
        "{keyed}"
    );
    assert!(keyed["hits"][0].get("labels").is_none());
}

/// 오류 경로: 알 수 없는 include 값, 0 limit, 배열 아닌 etypes 는 오류.
#[test]
fn neighbors_new_args_errors() {
    let (_d, srv) = server();
    layered_fixture(&srv);
    let addr = format!("{LAYERED}#{START_SLUG}");
    let e = call_err(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": addr, "include": ["foo"] }),
    );
    assert!(e.contains("include"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": addr, "limit": 0 }),
    );
    assert!(e.contains("limit"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": addr, "etypes": "기록" }),
    );
    assert!(e.contains("etypes"), "{e}");
}

/// [핵심] limit·max_bytes 에 걸리면 truncated:true. max_bytes 는 hits 배열 JSON 직렬화 바이트 수 기준.
#[test]
fn neighbors_truncated_by_limit_and_max_bytes() {
    let (_d, srv) = server();
    layered_fixture(&srv);
    let addr = format!("{LAYERED}#{START_SLUG}");
    let base = json!({ "addr": addr, "depth": 2, "include": ["labels", "props"] });
    let with = |k: &str, v: J| {
        let mut a = base.clone();
        a[k] = v;
        a
    };

    let full = call(&srv, "vgraph_neighbors", with("limit", json!(1000)));
    assert_eq!(full["truncated"], false);
    let total = full["hits"].as_array().unwrap().len();
    assert_eq!(total, 8);

    // limit
    let cut = call(&srv, "vgraph_neighbors", with("limit", json!(3)));
    assert_eq!(cut["hits"].as_array().unwrap().len(), 3);
    assert_eq!(cut["truncated"], true, "{cut}");
    let exact = call(&srv, "vgraph_neighbors", with("limit", json!(total)));
    assert_eq!(exact["truncated"], false, "딱 맞으면 잘림 아님: {exact}");
    assert_eq!(exact["hits"], full["hits"]);

    // max_bytes — 전체 직렬화 크기에 딱 맞으면 통과, 1바이트 모자라면 잘림
    let size = full["hits"].to_string().len();
    let fit = call(&srv, "vgraph_neighbors", with("max_bytes", json!(size)));
    assert_eq!(fit["truncated"], false, "{fit}");
    assert_eq!(fit["hits"], full["hits"]);
    let tight = call(&srv, "vgraph_neighbors", with("max_bytes", json!(size - 1)));
    assert_eq!(tight["truncated"], true, "{tight}");
    let kept = tight["hits"].as_array().unwrap().len();
    assert!(kept < total && kept > 0, "{tight}");
    assert!(tight["hits"].to_string().len() < size);
    assert_eq!(
        tight["hits"].as_array().unwrap()[..],
        full["hits"].as_array().unwrap()[..kept]
    );
    let none = call(&srv, "vgraph_neighbors", with("max_bytes", json!(2)));
    assert_eq!(none["hits"], json!([]));
    assert_eq!(none["truncated"], true);
}

/// 별도 작은 그래프 — 양쪽에서 닿는 경우.
///
/// 노드: S(시작, doc) · N(doc, 비확장) · R(check, 확장) · Y(doc) · X(doc) · Z(doc)
/// 엣지(생성 순): S-a->N, S-a->R, N-b->Y, R-c->Y, S-a->X, R-d->X, X-e->Z
/// 호출: S 에서 depth 2, expand_labels [check, privacy, log, audit].
///
/// ① Y 는 비확장 N 과 확장 R 양쪽에서 닿는다. N 은 뻗지 않으므로 Y 는 R 경유(via_type "c")로 2단계 방출.
/// ② X 는 1단계에서 비확장으로 잡혔는데 R 에서도 닿는다(R-d->X). X 는 1단계로 **한 번만** 나오고
///    (2단계 중복 없음), X 는 비확장이라 뻗지 않으므로 Z 는 나오지 않는다.
/// 기대: 1단계 {N, R, X}, 2단계 {Y(via c)}, Z 없음, X 한 번.
#[test]
fn walk_reached_from_both_rule_and_nonrule() {
    let (_d, srv) = server();
    let g = "acme/both";
    call(&srv, "vgraph_create", json!({ "name": g }));
    let ops = json!([
        node("S", "doc", json!({ "t": "s" })),
        node("N", "doc", json!({ "t": "n" })),
        node("R", "check", json!({ "t": "r" })),
        node("Y", "doc", json!({ "t": "y" })),
        node("X", "doc", json!({ "t": "x" })),
        node("Z", "doc", json!({ "t": "z" })),
        edge("a", "S", "N"),
        edge("a", "S", "R"),
        edge("b", "N", "Y"),
        edge("c", "R", "Y"),
        edge("a", "S", "X"),
        edge("d", "R", "X"),
        edge("e", "X", "Z"),
    ]);
    call(&srv, "vgraph_edit", json!({ "graph": g, "ops": ops }));
    let c = call(
        &srv,
        "vgraph_commit",
        json!({ "graph": g, "message": "both" }),
    );
    assert_eq!(c["outcome"], "committed", "{c}");

    let r = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": format!("{g}#S"), "depth": 2,
                "expand_labels": ["check", "privacy", "log", "audit"], "include": ["labels"] }),
    );
    let hits = &r["hits"];
    assert_eq!(slugs_at(hits, 1), set_of(&["N", "R", "X"]), "{r}");
    assert_eq!(slugs_at(hits, 2), set_of(&["Y"]), "{r}");
    assert_eq!(
        hit(hits, "Y")["via_type"],
        "c",
        "Y 는 확장 라벨 R 경유여야 함: {r}"
    );
    let x_count = hits
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["slug"] == "X")
        .count();
    assert_eq!(x_count, 1, "X 는 한 번만: {r}");
    assert!(
        hits.as_array().unwrap().iter().all(|h| h["slug"] != "Z"),
        "Z 는 나오면 안 됨: {r}"
    );
    assert_eq!(hit(hits, "X")["depth"], 1);
}

/// find: 대소문자 무시·한글·리스트 안 문자열·text_keys·props 완전 일치·라벨만.
#[test]
fn find_text_props_labels() {
    let (_d, srv) = server();
    layered_fixture(&srv);

    // 키 지정 — name 만 본다: Order 만(엔드포인트의 desc 'Order 를…' 는 제외)
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": LAYERED, "text": "ORDER", "text_keys": ["name"] }),
    );
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{r}");
    assert_eq!(items[0]["slug"], "Order");
    assert_eq!(items[0]["matched_key"], "name");
    assert_eq!(items[0]["excerpt"], "Order");

    // 키 미지정 — 리스트 안 문자열(CAT-1 keywords)도 맞는다
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": LAYERED, "text": "order", "labels": ["category"] }),
    );
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{r}");
    assert_eq!(items[0]["slug"], "CAT-1");
    assert_eq!(items[0]["matched_key"], "keywords");

    // 속성 완전 일치
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": LAYERED, "props": { "method": "GET" } }),
    );
    assert_eq!(r["items"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["items"][0]["slug"], START_SLUG);
    assert!(
        r["items"][0].get("matched_key").is_none(),
        "text 조건 없으면 matched_key 없음"
    );

    // 라벨만 + include props(prop_keys) — labels 는 싣지 않음
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": LAYERED, "labels": ["check"], "include": ["props"],
                "prop_keys": ["text"] }),
    );
    assert_eq!(r["items"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(
        r["items"][0]["props"],
        json!({ "text": "주문 ID 는 양수여야 한다" })
    );
    assert!(r["items"][0].get("labels").is_none());

    // 맞는 게 없으면 빈 목록 + done
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": LAYERED, "text": "없는문장zzz" }),
    );
    assert_eq!(r["items"], json!([]));
    assert_eq!(r["done"], true);
}

/// find 커서: limit 1 로 끝까지 이어 받은 결과 = 한 번에 받은 결과, 중복 0. 오류 경로 포함.
#[test]
fn find_cursor_paging_and_errors() {
    let (_d, srv) = server();
    layered_fixture(&srv);
    let all = call(
        &srv,
        "vgraph_find",
        json!({ "graph": LAYERED, "text": "주문", "limit": 100 }),
    );
    let want: Vec<String> = all["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uuid"].as_str().unwrap().to_string())
        .collect();
    assert!(want.len() >= 3, "{all}");

    let mut got: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..50 {
        let mut a = json!({ "graph": LAYERED, "text": "주문", "limit": 1 });
        if let Some(c) = &cursor {
            a["cursor"] = json!(c);
        }
        let r = call(&srv, "vgraph_find", a);
        for i in r["items"].as_array().unwrap() {
            got.push(i["uuid"].as_str().unwrap().to_string());
        }
        if r["done"] == true {
            assert_eq!(r["next_cursor"], J::Null);
            break;
        }
        cursor = Some(r["next_cursor"].as_str().unwrap().to_string());
    }
    assert_eq!(got, want);

    // 오류: 잘못된 커서·include·limit·없는 그래프
    let e = call_err(
        &srv,
        "vgraph_find",
        json!({ "graph": LAYERED, "cursor": "엉터리" }),
    );
    assert!(e.contains("커서"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_find",
        json!({ "graph": LAYERED, "include": ["foo"] }),
    );
    assert!(e.contains("include"), "{e}");
    let e = call_err(&srv, "vgraph_find", json!({ "graph": LAYERED, "limit": 0 }));
    assert!(e.contains("limit"), "{e}");
    let e = call_err(&srv, "vgraph_find", json!({ "graph": "acme/없음" }));
    assert!(!e.is_empty());
}

/// [핵심] 2,500 노드·2,500 엣지 그래프를 scan 으로 끝까지 받아 합계가 show 의 count 와 같고 중복 0.
#[test]
fn scan_full_graph_sums_match_show() {
    let (_d, srv) = server();
    let g = "acme/big2500";
    call(&srv, "vgraph_create", json!({ "name": g }));
    const N: usize = 2500;
    let mut ops: Vec<J> = (0..N)
        .map(|i| {
            node(
                &format!("n{i:04}"),
                if i % 2 == 0 { "A" } else { "B" },
                json!({ "i": i }),
            )
        })
        .collect();
    // 사슬 2,499 + 닫는 엣지 1 = 2,500
    ops.extend((0..N).map(|i| edge("next", &format!("n{i:04}"), &format!("n{:04}", (i + 1) % N))));
    call(&srv, "vgraph_edit", json!({ "graph": g, "ops": ops }));
    let c = call(
        &srv,
        "vgraph_commit",
        json!({ "graph": g, "message": "2500" }),
    );
    assert_eq!(c["outcome"], "committed", "{c}");
    let show = call(&srv, "vgraph_show", json!({ "addr": g }));
    assert_eq!(show["node_count"], N);
    assert_eq!(show["edge_count"], N);

    // (고유 uuid 집합, 받은 항목 총수, 쪽 수, epoch)
    let drain = |kind: &str, limit: usize, extra: J| -> (BTreeSet<String>, usize, usize, J) {
        let mut seen = BTreeSet::new();
        let (mut total, mut pages) = (0usize, 0usize);
        let mut cursor: Option<String> = None;
        let mut epoch = J::Null;
        loop {
            let mut a = json!({ "graph": g, "kind": kind, "limit": limit });
            for (k, v) in extra.as_object().unwrap() {
                a[k] = v.clone();
            }
            if let Some(c) = &cursor {
                a["cursor"] = json!(c);
            }
            let r = call(&srv, "vgraph_scan", a);
            if epoch.is_null() {
                epoch = r["epoch"].clone();
            }
            assert_eq!(r["epoch"], epoch, "쪽마다 epoch 같아야 함");
            pages += 1;
            for i in r["items"].as_array().unwrap() {
                total += 1;
                seen.insert(i["uuid"].as_str().unwrap().to_string());
            }
            if r["done"] == true {
                assert_eq!(r["next_cursor"], J::Null);
                break;
            }
            cursor = Some(r["next_cursor"].as_str().unwrap().to_string());
            assert!(pages < 100, "무한 루프");
        }
        (seen, total, pages, epoch)
    };

    let (nodes, node_total, node_pages, epoch) = drain("nodes", 1000, json!({}));
    assert_eq!(node_total, N, "노드 합계");
    assert_eq!(nodes.len(), N, "노드 중복 0");
    assert_eq!(node_pages, 3);
    assert_eq!(epoch, show["epoch"]);
    let (edges, edge_total, _, _) = drain("edges", 700, json!({}));
    assert_eq!(edge_total, N, "엣지 합계");
    assert_eq!(edges.len(), N, "엣지 중복 0");
    // limit 200 + include_props 로도 끝까지
    let (_, small_total, small_pages, _) = drain("nodes", 200, json!({ "include_props": true }));
    assert_eq!(small_total, N);
    assert_eq!(small_pages, 13);
    // 라벨 필터
    let (a_nodes, a_total, _, _) = drain("nodes", 1000, json!({ "labels": ["A"] }));
    assert_eq!((a_nodes.len(), a_total), (N / 2, N / 2));
}

/// scan 항목 모양: 노드 {uuid, slug, labels, props?}, 엣지 {uuid, type, from, to, from_slug, to_slug}. 오류 경로 포함.
#[test]
fn scan_item_shapes_and_errors() {
    let (_d, srv) = server();
    layered_fixture(&srv);
    let n = call(
        &srv,
        "vgraph_scan",
        json!({ "graph": LAYERED, "kind": "nodes", "labels": ["check"], "include_props": true }),
    );
    assert_eq!(n["items"].as_array().unwrap().len(), 1, "{n}");
    let it = &n["items"][0];
    assert_eq!(it["slug"], "CHK-1");
    assert_eq!(it["labels"], json!(["check"]));
    assert_eq!(it["props"]["text"], "주문 ID 는 양수여야 한다");
    assert!(it.get("system").is_none());
    let bare = call(
        &srv,
        "vgraph_scan",
        json!({ "graph": LAYERED, "kind": "nodes", "labels": ["check"] }),
    );
    assert!(
        bare["items"][0].get("props").is_none(),
        "include_props 없으면 props 없음"
    );

    let e = call(
        &srv,
        "vgraph_scan",
        json!({ "graph": LAYERED, "kind": "edges" }),
    );
    let items = e["items"].as_array().unwrap();
    assert_eq!(items.len(), 8);
    let log9 = items.iter().find(|i| i["to_slug"] == "LOG-9").unwrap();
    assert_eq!(log9["type"], "기록");
    assert_eq!(log9["from_slug"], "CHK-1");
    assert!(!log9["uuid"].is_null() && !log9["from"].is_null() && !log9["to"].is_null());

    let err = call_err(
        &srv,
        "vgraph_scan",
        json!({ "graph": LAYERED, "kind": "foo" }),
    );
    assert!(err.contains("kind"), "{err}");
    let err = call_err(&srv, "vgraph_scan", json!({ "graph": LAYERED }));
    assert!(err.contains("kind"), "{err}");
    let err = call_err(
        &srv,
        "vgraph_scan",
        json!({ "graph": LAYERED, "kind": "edges", "labels": ["category"] }),
    );
    assert!(err.contains("labels"), "{err}");
    let err = call_err(
        &srv,
        "vgraph_scan",
        json!({ "graph": LAYERED, "kind": "nodes", "limit": 0 }),
    );
    assert!(err.contains("limit"), "{err}");
}

/// scan kind=edges include_props=true: 엣지 속성이 항목에 실린다. 속성 없는 엣지는 빈 객체, include_props 없으면 props 키 없음.
#[test]
fn scan_edges_include_props() {
    let (_d, srv) = server();
    let g = "acme/edgeprops";
    call(&srv, "vgraph_create", json!({ "name": g }));
    let ops = json!([
        node("A", "doc", json!({})),
        node("B", "doc", json!({})),
        json!({ "op": "add_edge", "etype": "참조", "from": "A", "to": "B",
                "props": { "weight": 3, "note": "근거", "tags": ["x", "y"] } }),
        edge("관련", "B", "A"),
    ]);
    call(&srv, "vgraph_edit", json!({ "graph": g, "ops": ops }));
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": g, "message": "ep" }),
    );

    let r = call(
        &srv,
        "vgraph_scan",
        json!({ "graph": g, "kind": "edges", "include_props": true }),
    );
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{r}");
    let with = items.iter().find(|i| i["type"] == "참조").unwrap();
    assert_eq!(
        with["props"],
        json!({ "weight": 3, "note": "근거", "tags": ["x", "y"] }),
        "{r}"
    );
    let without = items.iter().find(|i| i["type"] == "관련").unwrap();
    assert_eq!(without["props"], json!({}), "{r}");

    let bare = call(&srv, "vgraph_scan", json!({ "graph": g, "kind": "edges" }));
    assert!(bare["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i.get("props").is_none()));
}

/// [핵심] 커서는 epoch 를 담는다 — head 가 움직여도 같은 버전에서 이어 읽고,
/// 주소가 다른 commit 을 가리키면 오류, 깨진 커서도 오류.
#[test]
fn scan_cursor_epoch_pinned_and_mismatch_errors() {
    let (_d, srv) = server();
    layered_fixture(&srv);
    let first = call(
        &srv,
        "vgraph_scan",
        json!({ "graph": LAYERED, "kind": "nodes", "limit": 4 }),
    );
    assert_eq!(first["done"], false);
    let epoch_a = first["epoch"].as_str().unwrap().to_string();
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    assert!(cursor.starts_with(&epoch_a), "커서에 epoch 포함: {cursor}");

    // head 전진: 새 노드 하나 + 커밋
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": LAYERED, "ops": [node("NEW", "doc", json!({}))] }),
    );
    let c = call(
        &srv,
        "vgraph_commit",
        json!({ "graph": LAYERED, "message": "advance" }),
    );
    let epoch_b = c["commit"].as_str().unwrap().to_string();
    assert_ne!(epoch_a, epoch_b);

    // 기본 주소(head=B)로 이어 받아도 A 에서 이어 읽는다: 나머지 5개, NEW 없음
    let rest = call(
        &srv,
        "vgraph_scan",
        json!({ "graph": LAYERED, "kind": "nodes", "cursor": cursor }),
    );
    assert_eq!(rest["epoch"], epoch_a, "{rest}");
    assert_eq!(rest["items"].as_array().unwrap().len(), 5, "{rest}");
    assert!(rest["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["slug"] != "NEW"));
    assert_eq!(rest["done"], true);

    // 주소가 다른 epoch(B)를 명시하면 오류
    let err = call_err(
        &srv,
        "vgraph_scan",
        json!({ "graph": format!("{LAYERED}.{epoch_b}"), "kind": "nodes", "cursor": cursor }),
    );
    assert!(err.contains("epoch"), "{err}");
    // 같은 epoch(A)를 명시하면 정상
    let ok = call(
        &srv,
        "vgraph_scan",
        json!({ "graph": format!("{LAYERED}.{epoch_a}"), "kind": "nodes", "cursor": cursor }),
    );
    assert_eq!(ok["items"], rest["items"]);
    // 깨진 커서
    let err = call_err(
        &srv,
        "vgraph_scan",
        json!({ "graph": LAYERED, "kind": "nodes", "cursor": "xyz" }),
    );
    assert!(err.contains("커서"), "{err}");
    let err = call_err(
        &srv,
        "vgraph_scan",
        json!({ "graph": LAYERED, "kind": "nodes", "cursor": format!("{epoch_a}|없는uuid") }),
    );
    assert!(err.contains("커서"), "{err}");
}

/// [중요] 새 도구·새 인자 설명문에 형식과 예시가 있다 (제약-3), find 설명에 한계 문구.
#[test]
fn new_tool_and_arg_descriptions_have_format_and_examples() {
    let defs = tool_defs();
    let def = |n: &str| {
        defs.iter()
            .find(|d| d["name"] == n)
            .unwrap_or_else(|| panic!("도구 없음: {n}"))
            .clone()
    };
    let desc = |d: &J| d["description"].as_str().unwrap().to_string();

    let find = def("vgraph_find");
    let fd = desc(&find);
    assert!(
        fd.contains("노드마다 첫 번째로 맞은 키 하나만 보고"),
        "{fd}"
    );
    assert!(
        fd.contains(
            "대소문자 접기는 문자 1:1 — 소문자가 여러 글자로 바뀌는 드문 문자(예: İ)는 정확히 접히지 않음"
        ),
        "{fd}"
    );
    assert!(
        fd.contains("{\"graph\":\"acme/catalog\""),
        "호출 예시 JSON: {fd}"
    );
    assert!(fd.contains("next_cursor") && fd.contains("limit"), "{fd}");

    let scan = def("vgraph_scan");
    let sd = desc(&scan);
    assert!(
        sd.contains("{\"graph\":\"acme/catalog\",\"kind\":\"nodes\""),
        "{sd}"
    );
    assert!(sd.contains("next_cursor") && sd.contains("epoch"), "{sd}");

    let neigh = def("vgraph_neighbors");
    let nd = desc(&neigh);
    for arg in [
        "etypes",
        "emit_labels",
        "expand_labels",
        "include",
        "prop_keys",
        "limit",
        "max_bytes",
        "truncated",
        "via_type",
    ] {
        assert!(nd.contains(arg), "neighbors 설명에 {arg} 없음: {nd}");
    }
    assert!(
        nd.contains("\"expand_labels\":[\"Category\",\"Brand\"]"),
        "예시: {nd}"
    );

    // 새 인자마다 properties 에 설명(형식·예시)이 있다
    let described = |d: &J, args: &[&str]| {
        for a in args {
            let p = &d["inputSchema"]["properties"][*a];
            assert!(p.is_object(), "{}: {a} 속성 없음", d["name"]);
            assert!(
                p["description"].as_str().is_some_and(|s| !s.is_empty()),
                "{}: {a} 설명 없음",
                d["name"]
            );
        }
    };
    described(
        &neigh,
        &[
            "etypes",
            "emit_labels",
            "expand_labels",
            "include",
            "prop_keys",
            "limit",
            "max_bytes",
        ],
    );
    described(
        &find,
        &[
            "labels",
            "props",
            "text",
            "text_keys",
            "include",
            "prop_keys",
            "limit",
            "cursor",
        ],
    );
    described(&scan, &["labels", "include_props", "limit", "cursor"]);
    assert_eq!(find["inputSchema"]["required"], json!(["graph"]));
    assert_eq!(scan["inputSchema"]["required"], json!(["graph", "kind"]));

    // 기존 neighbors 설명은 덧붙이기만 — 원문이 앞에 그대로 있고 required 불변
    assert!(
        nd.starts_with("순회 — addr#node에서 dir/etype/depth로"),
        "{nd}"
    );
    assert_eq!(neigh["inputSchema"]["required"], json!(["addr"]));
}

/// SERVER_INSTRUCTIONS: 기존 문장(embed_target 등)은 그대로, 끝에 조회 패턴 예시가 덧붙음.
#[test]
fn server_instructions_append_query_pattern() {
    let ins = vgraph_mcp::SERVER_INSTRUCTIONS;
    assert!(ins.contains("embed_target"), "기존 문장 유지");
    let sys = ins
        .find("시스템 노드 — 직접 수정 금지.")
        .expect("기존 마지막 문장");
    let pat = ins.find("조회 패턴").expect("조회 패턴 예시");
    assert!(pat > sys, "뒤에 덧붙임");
    let tail = &ins[pat..];
    for s in [
        "vgraph_find",
        "vgraph_neighbors",
        "\"expand_labels\":[\"Category\",\"Brand\"]",
        "\"depth\":2",
        "\"include\":[\"labels\",\"props\"]",
        "vgraph_scan",
    ] {
        assert!(tail.contains(s), "{s} 없음: {tail}");
    }
}

/// SERVER_INSTRUCTIONS: 첫 줄은 범용 정체성 문구이고, 특정 사용처 이름을 담지 않는다.
#[test]
fn server_instructions_identity_is_generic() {
    let ins = vgraph_mcp::SERVER_INSTRUCTIONS;
    assert_eq!(
        ins.lines().next(),
        Some("vgraph = 버저닝·브랜칭 그래프 데이터베이스. 규약:")
    );
    let lower = ins.to_lowercase();
    for s in ["ssot", "grove"] {
        assert!(!lower.contains(s), "{s} 포함: {ins}");
    }
}

const EXPECTED_NEIGHBORS: &str = r#"{
  "epoch": "sha256:e8d8b944ab47ea0235bfb4e7fdf6e733d35c523c502745ab8bdf27934dca9e57",
  "hits": [
    {"depth":1,"slug":"CAT-1","uuid":"00000000000000000000000004","via":"00000000000000000000000012"},
    {"depth":1,"slug":"Order","uuid":"00000000000000000000000005","via":"00000000000000000000000013"},
    {"depth":1,"slug":"CHK-1","uuid":"00000000000000000000000006","via":"00000000000000000000000014"},
    {"depth":1,"slug":"PRV-1","uuid":"00000000000000000000000007","via":"00000000000000000000000015"},
    {"depth":2,"slug":"LOG-9","uuid":"00000000000000000000000008","via":"00000000000000000000000016"},
    {"depth":2,"slug":"NOTE-P","uuid":"00000000000000000000000009","via":"00000000000000000000000017"},
    {"depth":2,"slug":"CAT-2","uuid":"00000000000000000000000010","via":"00000000000000000000000018"},
    {"depth":2,"slug":"Customer","uuid":"00000000000000000000000011","via":"00000000000000000000000019"}
  ]
}"#;

// ================= Step 4a: vgraph_export · vgraph_import (R5, mode=replace) =================

fn tmp_file(dir: &TempDir, name: &str) -> String {
    dir.path().join(name).to_string_lossy().into_owned()
}

/// [핵심] 새 도구 둘의 설명문에 형식·예시·제한이 있고 기존 도구 이름은 그대로다(제약-1·3·8).
#[test]
fn export_import_tool_descriptions_have_format_and_examples() {
    let defs = tool_defs();
    let def = |n: &str| {
        defs.iter()
            .find(|t| t["name"] == n)
            .unwrap_or_else(|| panic!("{n} 없음"))
    };
    let ex = def("vgraph_export")["description"]
        .as_str()
        .unwrap()
        .to_string();
    let im = def("vgraph_import")["description"]
        .as_str()
        .unwrap()
        .to_string();
    for (name, d) in [("export", &ex), ("import", &im)] {
        assert!(d.contains("vgraph-json/1"), "{name}: 형식 이름 — {d}");
        assert!(
            d.contains("\"nodes\"") && d.contains("\"edges\""),
            "{name}: 형식 예시 — {d}"
        );
        assert!(
            d.contains("acme/catalog"),
            "{name}: 호출 예시(중립 그래프) — {d}"
        );
        assert!(d.contains("slug"), "{name}: {d}");
        assert!(
            !d.to_lowercase().contains("grove"),
            "{name}: 사용처 전용 문구 금지 — {d}"
        );
    }
    assert!(
        ex.contains("slug 없는 노드") && ex.contains("__lockfile"),
        "{ex}"
    );
    assert!(ex.contains("path"), "{ex}");
    assert!(im.contains("replace") && im.contains("빈 그래프"), "{im}");
    assert!(im.contains("document") && im.contains("path"), "{im}");
    assert!(im.contains("커밋 한 번") || im.contains("커밋 1개"), "{im}");
    assert!(
        im.contains("고정 버전") && im.contains("최신"),
        "링크(Ref) 핀 안내 — {im}"
    );
    assert_eq!(
        def("vgraph_import")["inputSchema"]["required"],
        json!(["graph"])
    );
    assert_eq!(
        def("vgraph_export")["inputSchema"]["required"],
        json!(["graph"])
    );
    // 기존 16종 + Step 3 두 개는 그대로 있다
    for n in [
        "vgraph_list",
        "vgraph_create",
        "vgraph_show",
        "vgraph_search",
        "vgraph_neighbors",
        "vgraph_edit",
        "vgraph_pending",
        "vgraph_commit",
        "vgraph_discard",
        "vgraph_branch",
        "vgraph_merge",
        "vgraph_log",
        "vgraph_diff",
        "vgraph_links",
        "vgraph_admin",
        "vgraph_embed_pending",
        "vgraph_find",
        "vgraph_scan",
    ] {
        def(n);
    }
}

/// [핵심] 인라인 내보내기 → 파일 내보내기 → 가져오기 → 다시 내보내기: 바이트 동일, 커밋 1개.
#[test]
fn export_then_import_roundtrip_is_byte_identical_with_one_commit() {
    let (dir, srv) = server();
    layered_fixture(&srv);
    call(&srv, "vgraph_create", json!({ "name": "acme/copy" }));

    let inline = call(&srv, "vgraph_export", json!({ "graph": LAYERED }));
    assert_eq!(inline["nodes"], 9);
    assert_eq!(inline["edges"], 8);
    assert_eq!(inline["document"]["format"], "vgraph-json/1");
    assert!(inline["epoch"].as_str().unwrap().starts_with("sha256:"));

    let f1 = tmp_file(&dir, "a.json");
    let r = call(
        &srv,
        "vgraph_export",
        json!({ "graph": LAYERED, "path": f1 }),
    );
    assert_eq!(r["path"], f1.as_str());
    assert!(
        r.get("document").is_none(),
        "파일로 썼으면 본문은 싣지 않는다: {r}"
    );

    let imp = call(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/copy", "path": f1 }),
    );
    assert_eq!(imp["outcome"], "committed");
    assert_eq!(
        (imp["nodes"].clone(), imp["edges"].clone()),
        (json!(9), json!(8))
    );
    let log = call(&srv, "vgraph_log", json!({ "graph": "acme/copy" }));
    assert_eq!(
        log["commits"].as_array().unwrap().len(),
        1,
        "가져오기 1회 = 커밋 1개"
    );

    let f2 = tmp_file(&dir, "b.json");
    call(
        &srv,
        "vgraph_export",
        json!({ "graph": "acme/copy", "path": f2 }),
    );
    assert_eq!(
        std::fs::read(&f1).unwrap(),
        std::fs::read(&f2).unwrap(),
        "왕복 바이트 동일"
    );
    // 기존 도구로 읽어도 같은 내용
    let n = call(
        &srv,
        "vgraph_show",
        json!({ "addr": format!("acme/copy#{START_SLUG}") }),
    );
    assert_eq!(n["props"]["method"], "GET");
}

/// 인라인 document 는 객체로도 JSON 문자열로도 받는다.
#[test]
fn import_accepts_inline_document_as_object_or_string() {
    let (_dir, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/a" }));
    call(&srv, "vgraph_create", json!({ "name": "acme/b" }));
    let doc = json!({
        "format": "vgraph-json/1",
        "nodes": [
            { "slug": "x", "labels": ["Item"], "props": { "name": "엑스" } },
            { "slug": "y", "labels": [], "props": {} }
        ],
        "edges": [ { "etype": "관련", "from": "x", "to": "y", "props": {} } ]
    });
    let r = call(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/a", "document": doc }),
    );
    assert_eq!(r["outcome"], "committed");
    let r = call(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/b", "document": doc.to_string(), "message": "문자열로" }),
    );
    assert_eq!(r["outcome"], "committed");
    let log = call(&srv, "vgraph_log", json!({ "graph": "acme/b" }));
    assert_eq!(log["commits"][0]["msg"], "문자열로");
}

/// [핵심] 끝이 없는 엣지 → 거부, 빠진 slug 목록, head 불변·작업분 없음.
#[test]
fn import_rejects_dangling_edge_listing_missing_slugs_without_commit() {
    let (_dir, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/a" }));
    let doc = json!({
        "format": "vgraph-json/1",
        "nodes": [ { "slug": "x", "labels": [], "props": {} } ],
        "edges": [ { "etype": "t", "from": "x", "to": "빠진-1", "props": {} },
                   { "etype": "t", "from": "빠진-2", "to": "x", "props": {} } ]
    });
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/a", "document": doc }),
    );
    assert!(
        e.contains("빠진-1") && e.contains("빠진-2") && e.contains("missing_slug"),
        "{e}"
    );
    let list = call(&srv, "vgraph_list", json!({}));
    assert_eq!(list["graphs"][0]["head"], J::Null, "커밋 0");
    let p = call(&srv, "vgraph_pending", json!({ "graph": "acme/a" }));
    assert_eq!(p["working"], J::Null, "작업분 흔적 없음: {p}");
}

/// replace 거부: 비어 있지 않은 그래프, 미커밋 편집, 잘못된 인자.
#[test]
fn import_rejects_non_empty_pending_and_bad_arguments() {
    let (dir, srv) = server();
    layered_fixture(&srv);
    let f = tmp_file(&dir, "a.json");
    call(
        &srv,
        "vgraph_export",
        json!({ "graph": LAYERED, "path": f }),
    );

    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": LAYERED, "path": f }),
    );
    assert!(e.contains("빈 그래프"), "{e}");

    call(&srv, "vgraph_create", json!({ "name": "acme/empty" }));
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/empty", "ops": [node("mine", "Item", json!({}))] }),
    );
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/empty", "path": f }),
    );
    assert!(e.contains("미커밋 편집"), "{e}");
    let p = call(&srv, "vgraph_pending", json!({ "graph": "acme/empty" }));
    assert!(p["working"].is_string(), "사용자의 편집은 그대로: {p}");
    call(&srv, "vgraph_discard", json!({ "graph": "acme/empty" }));

    let e = call_err(&srv, "vgraph_import", json!({ "graph": "acme/empty" }));
    assert!(e.contains("document") && e.contains("path"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/empty", "path": f, "document": {} }),
    );
    assert!(e.contains("document") && e.contains("path"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/empty", "path": f, "mode": "bogus" }),
    );
    assert!(e.contains("replace"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/empty", "path": "없는파일.json" }),
    );
    assert!(e.contains("없는파일.json"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/nowhere", "path": f }),
    );
    assert!(e.contains("미등록"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/empty", "document": "{" }),
    );
    assert!(e.contains("형식 오류"), "{e}");
}

/// 내보내기 거부: slug 없는 노드는 목록(uuid·라벨)과 함께, 없는 그래프·커밋 없는 그래프는 오류.
#[test]
fn export_rejects_slugless_nodes_and_unknown_graphs() {
    let (_dir, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/s" }));
    let r = call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/s", "ops": [
            { "op": "add_node", "labels": ["Item"], "props": {} },
            node("ok", "Item", json!({})) ] }),
    );
    let uuid = r["results"][0]["uuid"].as_str().unwrap().to_string();
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/s", "message": "m" }),
    );
    let e = call_err(&srv, "vgraph_export", json!({ "graph": "acme/s" }));
    assert!(
        e.contains("slugless_node") && e.contains(&uuid) && e.contains("Item"),
        "{e}"
    );
    assert!(e.contains("set_slug"), "고치는 방법 안내: {e}");

    let e = call_err(&srv, "vgraph_export", json!({ "graph": "acme/none" }));
    assert!(!e.is_empty());
    call(&srv, "vgraph_create", json!({ "name": "acme/nocommit" }));
    let e = call_err(&srv, "vgraph_export", json!({ "graph": "acme/nocommit" }));
    assert!(!e.is_empty());
}

/// 링크(Ref): 축약 주소는 edit 와 같이 보정되고, 미등록 대상은 가져오기 전에 명확히 거부된다.
#[test]
fn import_normalizes_refs_and_rejects_unregistered_targets() {
    let (_dir, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/other" }));
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/other", "ops": [node("t", "Item", json!({}))] }),
    );
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/other", "message": "t" }),
    );
    call(&srv, "vgraph_create", json!({ "name": "acme/a" }));
    call(&srv, "vgraph_create", json!({ "name": "acme/b" }));
    let mk = |target: &str| {
        json!({ "format": "vgraph-json/1",
                "nodes": [ { "slug": "x", "labels": [], "props": { "link": { "$ref": target } } } ],
                "edges": [] })
    };
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/b", "document": mk("acme/missing#t") }),
    );
    assert!(e.contains("미등록"), "{e}");
    let list = call(&srv, "vgraph_list", json!({}));
    let head_of = |name: &str| {
        list["graphs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["name"] == name)
            .unwrap()["head"]
            .clone()
    };
    assert_eq!(head_of("acme/b"), J::Null, "거부된 가져오기는 커밋 0");

    let r = call(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/a", "document": mk("acme/other#t") }),
    );
    assert_eq!(r["outcome"], "committed");
    let n = call(&srv, "vgraph_show", json!({ "addr": "acme/a#x" }));
    assert_eq!(
        n["props"]["link"]["$ref"], "graph://acme/other#t",
        "축약형이 완전한 주소로 보정됨"
    );
}

// ================= Step 4b: vgraph_import mode=merge (가져오기 전용 브랜치 + 기존 머지) =================

fn merge_doc(a_name: &str, with_c: bool) -> J {
    let mut nodes = vec![
        json!({ "slug": "a", "labels": ["Item"], "props": { "name": a_name, "extra": 1 } }),
        json!({ "slug": "b", "labels": ["Item"], "props": { "name": "B" } }),
    ];
    let mut edges = vec![json!({ "etype": "t", "from": "a", "to": "b", "props": {} })];
    if with_c {
        nodes.push(json!({ "slug": "c", "labels": ["Rule"], "props": {} }));
        edges.push(json!({ "etype": "t", "from": "b", "to": "c", "props": {} }));
    }
    json!({ "format": "vgraph-json/1", "nodes": nodes, "edges": edges })
}

fn import_merge(srv: &McpServer, graph: &str, doc: J) -> J {
    call(
        srv,
        "vgraph_import",
        json!({ "graph": graph, "mode": "merge", "via_branch": "sync", "document": doc }),
    )
}

/// [핵심] 설명문: merge 모드·via_branch·충돌 해소 방법·번호 해소 한계가 적혀 있고, vgraph_merge 에 선택 인자 ours 가 더해졌다.
#[test]
fn import_merge_descriptions_cover_modes_conflicts_and_limits() {
    let defs = tool_defs();
    let def = |n: &str| defs.iter().find(|t| t["name"] == n).unwrap();
    let im = def("vgraph_import")["description"]
        .as_str()
        .unwrap()
        .to_string();
    for needle in [
        "mode=merge",
        "via_branch",
        "가져오기 전용 브랜치",
        "vgraph_merge",
        "theirs",
        "ours",
        "충돌",
        "prop_both_set",
        "삭제",
        "번호로 풀 수 없",
        "acme/catalog",
        "up_to_date",
    ] {
        assert!(im.contains(needle), "{needle} 없음: {im}");
    }
    assert!(!im.to_lowercase().contains("grove"), "{im}");
    let props = &def("vgraph_import")["inputSchema"]["properties"];
    assert_eq!(props["mode"]["enum"], json!(["replace", "merge"]));
    assert!(props["via_branch"]["description"]
        .as_str()
        .unwrap()
        .contains("기본값 없음"));
    assert_eq!(
        def("vgraph_import")["inputSchema"]["required"],
        json!(["graph"])
    );
    // 기존 도구 vgraph_merge: 원래 인자는 그대로, 선택 인자 ours 만 추가
    let m = def("vgraph_merge");
    assert_eq!(m["inputSchema"]["required"], json!(["graph", "src", "dst"]));
    assert!(m["inputSchema"]["properties"]["theirs"].is_object());
    assert!(m["inputSchema"]["properties"]["ours"].is_object());
    assert!(m["description"]
        .as_str()
        .unwrap()
        .starts_with("src를 dst로 3-way 머지."));
}

/// [핵심] 첫 가져오기 → 문서만 바뀐 두 번째 → 충돌 없이 반영(fast_forward), 같은 문서 세 번째는 up_to_date·커밋 0.
#[test]
fn import_merge_first_then_document_changes_then_up_to_date() {
    let (_dir, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/m" }));
    let r = import_merge(&srv, "acme/m", merge_doc("A", false));
    assert_eq!(r["outcome"], "first_import");
    assert_eq!(r["via_branch"], "sync");
    assert_eq!(
        (r["nodes"].clone(), r["edges"].clone()),
        (json!(2), json!(1))
    );
    let main = call(&srv, "vgraph_log", json!({ "graph": "acme/m" }));
    let via = call(&srv, "vgraph_log", json!({ "graph": "acme/m@sync" }));
    assert_eq!(
        main["commits"][0]["commit"], via["commits"][0]["commit"],
        "같은 커밋에서 분기"
    );

    let r = import_merge(&srv, "acme/m", merge_doc("A2", true));
    assert_eq!(r["outcome"], "fast_forward", "{r}");
    assert!(r["via_commit"].is_string());
    let a = call(&srv, "vgraph_show", json!({ "addr": "acme/m#a" }));
    assert_eq!(a["props"]["name"], "A2");
    assert!(call(&srv, "vgraph_show", json!({ "addr": "acme/m#c" }))["labels"] == json!(["Rule"]));

    let n_before = call(&srv, "vgraph_log", json!({ "graph": "acme/m" }))["commits"]
        .as_array()
        .unwrap()
        .len();
    let r = import_merge(&srv, "acme/m", merge_doc("A2", true));
    assert_eq!(r["outcome"], "up_to_date", "{r}");
    assert!(r["via_commit"].is_null());
    let n_after = call(&srv, "vgraph_log", json!({ "graph": "acme/m" }))["commits"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(n_before, n_after, "커밋 0");
}

/// [핵심] 그래프 쪽 편집은 가져오기 후에도 유지되고(merged), 같은 속성 충돌은 보고되며 대상은 불변이다.
/// 충돌은 기존 vgraph_merge 의 theirs/ours 로 해소된다.
#[test]
fn import_merge_keeps_graph_edits_and_reports_conflicts_resolvable_with_merge_tool() {
    for pick in ["theirs", "ours"] {
        let (_dir, srv) = server();
        call(&srv, "vgraph_create", json!({ "name": "acme/m" }));
        import_merge(&srv, "acme/m", merge_doc("A", false));
        // 대상 브랜치에서 직접 편집: a.name 변경 + b.note 추가
        call(
            &srv,
            "vgraph_edit",
            json!({ "graph": "acme/m", "ops": [
            { "op": "set_prop", "node": "a", "key": "name", "value": "T" },
            { "op": "set_prop", "node": "b", "key": "note", "value": "mine" } ] }),
        );
        call(
            &srv,
            "vgraph_commit",
            json!({ "graph": "acme/m", "message": "direct" }),
        );
        let head =
            call(&srv, "vgraph_log", json!({ "graph": "acme/m" }))["commits"][0]["commit"].clone();

        // 문서가 a.name 을 다르게 바꿈 → 충돌, 대상 불변
        let r = import_merge(&srv, "acme/m", merge_doc("D", false));
        assert_eq!(r["outcome"], "conflicts", "{r}");
        assert_eq!(r["report"]["items"][0]["kind"], "prop_both_set", "{r}");
        let hint = r["resolve"].as_str().unwrap();
        for needle in [
            "vgraph_merge",
            "\"src\":\"sync\"",
            "\"dst\":\"main\"",
            "theirs",
            "ours",
        ] {
            assert!(hint.contains(needle), "{needle} 없음: {hint}");
        }
        let after =
            call(&srv, "vgraph_log", json!({ "graph": "acme/m" }))["commits"][0]["commit"].clone();
        assert_eq!(head, after, "대상 브랜치 불변");
        assert_eq!(
            call(&srv, "vgraph_show", json!({ "addr": "acme/m#a" }))["props"]["name"],
            "T"
        );

        // 기존 vgraph_merge 로 해소
        let m = call(
            &srv,
            "vgraph_merge",
            json!({ "graph": "acme/m", "src": "sync", "dst": "main", pick: [0] }),
        );
        assert_eq!(m["outcome"], "merged", "{m}");
        let want = if pick == "theirs" { "D" } else { "T" };
        let a = call(&srv, "vgraph_show", json!({ "addr": "acme/m#a" }));
        assert_eq!(a["props"]["name"], want);
        let b = call(&srv, "vgraph_show", json!({ "addr": "acme/m#b" }));
        assert_eq!(b["props"]["note"], "mine", "그래프 쪽 편집 유지");
    }
}

/// [핵심] 삭제 대 수정 충돌: 번호로 풀 수 없다는 안내가 응답에 있다.
#[test]
fn import_merge_removed_vs_edit_conflict_says_not_resolvable_by_number() {
    let (_dir, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/m" }));
    import_merge(&srv, "acme/m", merge_doc("A", true));
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/m", "ops": [
        { "op": "set_prop", "node": "c", "key": "x", "value": 1 } ] }),
    );
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/m", "message": "edit c" }),
    );
    let r = import_merge(&srv, "acme/m", merge_doc("A", false)); // 문서에서 c 삭제
    assert_eq!(r["outcome"], "conflicts", "{r}");
    let kinds: Vec<&str> = r["report"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"removed_vs_edit"), "{kinds:?}");
    let hint = r["resolve"].as_str().unwrap();
    assert!(hint.contains("번호로 풀 수 없"), "{hint}");
}

/// 오류: 인자 조합, 첫 가져오기인데 비어 있지 않음, 문서 안 중복 엣지, via 편집 중.
#[test]
fn import_merge_argument_and_state_errors() {
    let (_dir, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/m" }));
    let doc = merge_doc("A", false);
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/m", "mode": "merge", "document": doc }),
    );
    assert!(e.contains("via_branch"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/m", "via_branch": "sync", "document": doc }),
    );
    assert!(
        e.contains("via_branch") && e.contains("merge"),
        "replace 에서는 쓰지 않음: {e}"
    );
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/m", "mode": "bogus", "document": doc }),
    );
    assert!(e.contains("replace") && e.contains("merge"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/m", "mode": "merge", "via_branch": "a.b", "document": doc }),
    );
    assert!(e.contains("via_branch"), "{e}");

    let dup = json!({ "format": "vgraph-json/1",
        "nodes": [ { "slug": "a", "labels": [], "props": {} }, { "slug": "b", "labels": [], "props": {} } ],
        "edges": [ { "etype": "t", "from": "a", "to": "b", "props": { "w": 1 } },
                   { "etype": "t", "from": "a", "to": "b", "props": { "w": 2 } } ] });
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/m", "mode": "merge", "via_branch": "sync", "document": dup }),
    );
    assert!(e.contains("duplicate_edge"), "{e}");
    assert_eq!(
        call(&srv, "vgraph_list", json!({}))["graphs"][0]["head"],
        J::Null,
        "커밋 0"
    );

    // 비어 있지 않은 대상 + via 없음 → 기준점 없음
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/m", "ops": [node("x", "Item", json!({}))] }),
    );
    call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/m", "message": "x" }),
    );
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/m", "mode": "merge", "via_branch": "sync", "document": doc }),
    );
    assert!(e.contains("기준점") && e.contains("빈 그래프"), "{e}");
}

/// CLI 쪽 공유 규약과 같은 응답: Ref 보정은 merge 모드에도 적용된다(미등록 대상 → 가져오기 전 거부).
#[test]
fn import_merge_normalizes_refs_and_rejects_unregistered_targets() {
    let (_dir, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/m" }));
    let doc = json!({ "format": "vgraph-json/1", "edges": [],
        "nodes": [ { "slug": "x", "labels": [], "props": { "link": { "$ref": "acme/missing#t" } } } ] });
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/m", "mode": "merge", "via_branch": "sync", "document": doc }),
    );
    assert!(e.contains("미등록"), "{e}");
    assert_eq!(
        call(&srv, "vgraph_list", json!({}))["graphs"][0]["head"],
        J::Null
    );
}

// ================= Step 5: 규모 실측 (#[ignore] — 기본 시험에서 제외) =================
//
// 실행: cargo test --release -p vgraph-mcp --test mcp_query -- --ignored --nocapture scale_
// 결과는 출력일 뿐 단언하지 않는다(응답 시간은 머신에 따라 달라지므로 목표치는 보고서의 제안).

fn scale_doc(nodes: usize, edges: usize) -> (String, usize) {
    let labels = ["Item", "Category", "Brand"];
    let mut ns = Vec::with_capacity(nodes);
    let mut prop_bytes = 0usize;
    for i in 0..nodes {
        let name = format!("항목 {i} 배송 정책 문서");
        let desc = format!(
            "{i}번 항목의 설명입니다. 주문이 접수되면 재고를 확인하고 배송 일정을 안내합니다. {}",
            "한글 문장을 반복해서 속성 크기를 키웁니다. ".repeat(20 + i % 30)
        );
        prop_bytes += name.len() + desc.len();
        ns.push(json!({
            "slug": format!("n-{i:05}"), "labels": [labels[i % 3]],
            "props": { "name": name, "desc": desc, "status": if i % 4 == 0 { "active" } else { "draft" },
                       "tags": ["태그A", "태그B"], "n": i as i64 }
        }));
    }
    let mut es = Vec::with_capacity(edges);
    for i in 0..edges {
        let from = i % nodes;
        let to = (from * 7 + 1 + i / nodes) % nodes;
        let to = if to == from { (to + 1) % nodes } else { to };
        es.push(json!({
            "etype": if i % 2 == 0 { "참조" } else { "관련" },
            "from": format!("n-{from:05}"), "to": format!("n-{to:05}"), "props": {}
        }));
    }
    (
        json!({ "format": "vgraph-json/1", "nodes": ns, "edges": es }).to_string(),
        prop_bytes,
    )
}

/// 같은 호출을 `runs`번 재서 (최소 ms, 중앙값 ms, 응답 바이트)를 출력한다.
fn measure(srv: &McpServer, label: &str, tool: &str, args: J, runs: usize) {
    let mut times = Vec::with_capacity(runs);
    let mut bytes = 0usize;
    for _ in 0..runs {
        let t = std::time::Instant::now();
        let resp = rpc(
            srv,
            "tools/call",
            json!({ "name": tool, "arguments": args }),
        );
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        assert!(
            !resp["result"]["isError"].as_bool().unwrap_or(false),
            "{label}: {resp}"
        );
        bytes = resp["result"]["content"][0]["text"].as_str().unwrap().len();
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!(
        "| {label} | {:.1} | {:.1} | {bytes} |",
        times[0],
        times[times.len() / 2]
    );
}

#[test]
#[ignore = "규모 실측 — --release --ignored 로 실행"]
fn scale_measurement_3000_nodes_6000_edges() {
    let (dir, srv) = server();
    let (text, prop_bytes) = scale_doc(3000, 6000);
    let path = tmp_file(&dir, "scale.json");
    std::fs::write(&path, &text).unwrap();
    call(&srv, "vgraph_create", json!({ "name": "acme/scale" }));
    let t = std::time::Instant::now();
    let r = call(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/scale", "path": path }),
    );
    eprintln!(
        "\n[가져오기] 노드 {} 엣지 {} 문서 {} 바이트, 속성 문자열 합계 {} 바이트, 소요 {:.2}s",
        r["nodes"],
        r["edges"],
        text.len(),
        prop_bytes,
        t.elapsed().as_secs_f64()
    );
    eprintln!("| 조건 | 최소 ms | 중앙값 ms | 응답 바이트 |\n|---|---|---|---|");
    let g = "acme/scale";
    let runs = 7;
    measure(
        &srv,
        "find text=배송 (키 미지정, limit 50)",
        "vgraph_find",
        json!({ "graph": g, "text": "배송" }),
        runs,
    );
    measure(
        &srv,
        "find text=배송 (키 미지정, limit 500, include props)",
        "vgraph_find",
        json!({ "graph": g, "text": "배송", "limit": 500, "include": ["props"] }),
        runs,
    );
    measure(
        &srv,
        "find text=배송 text_keys=[name]",
        "vgraph_find",
        json!({ "graph": g, "text": "배송", "text_keys": ["name"] }),
        runs,
    );
    measure(
        &srv,
        "find text=항목 1234 (히트 소수)",
        "vgraph_find",
        json!({ "graph": g, "text": "항목 1234 " }),
        runs,
    );
    measure(
        &srv,
        "find text=없는말 (전수 탐색, 히트 0)",
        "vgraph_find",
        json!({ "graph": g, "text": "없는말zzz" }),
        runs,
    );
    measure(
        &srv,
        "neighbors depth2 expand=[Category,Brand] labels+props",
        "vgraph_neighbors",
        json!({ "addr": format!("{g}#n-00001"), "depth": 2, "expand_labels": ["Category", "Brand"],
                "include": ["labels", "props"] }),
        runs,
    );
    measure(
        &srv,
        "neighbors depth2 expand=[Category,Brand] limit 100",
        "vgraph_neighbors",
        json!({ "addr": format!("{g}#n-00001"), "depth": 2, "expand_labels": ["Category", "Brand"],
                "include": ["labels", "props"], "limit": 100 }),
        runs,
    );
    measure(
        &srv,
        "neighbors both depth3 expand=[Category,Brand] labels+props limit 1000",
        "vgraph_neighbors",
        json!({ "addr": format!("{g}#n-00001"), "dir": "both", "depth": 3,
                "expand_labels": ["Category", "Brand"], "include": ["labels", "props"], "limit": 1000 }),
        runs,
    );
    measure(
        &srv,
        "neighbors depth2 (기존 인자만)",
        "vgraph_neighbors",
        json!({ "addr": format!("{g}#n-00001"), "depth": 2 }),
        runs,
    );
    for (kind, props, limit) in [
        ("nodes", false, 200),
        ("nodes", true, 200),
        ("nodes", false, 1000),
        ("nodes", true, 1000),
        ("edges", false, 1000),
        ("edges", true, 1000),
    ] {
        measure(
            &srv,
            &format!("scan {kind} include_props={props} limit {limit} (1페이지)"),
            "vgraph_scan",
            json!({ "graph": g, "kind": kind, "include_props": props, "limit": limit }),
            runs,
        );
    }
    for (kind, props) in [("nodes", false), ("nodes", true), ("edges", false)] {
        let mut times = Vec::new();
        let mut bytes = 0usize;
        let mut pages = 0usize;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut cursor: Option<String> = None;
            bytes = 0;
            pages = 0;
            loop {
                let mut a =
                    json!({ "graph": g, "kind": kind, "include_props": props, "limit": 1000 });
                if let Some(c) = &cursor {
                    a["cursor"] = json!(c);
                }
                let resp = rpc(
                    &srv,
                    "tools/call",
                    json!({ "name": "vgraph_scan", "arguments": a }),
                );
                let text = resp["result"]["content"][0]["text"].as_str().unwrap();
                bytes += text.len();
                pages += 1;
                let j: J = serde_json::from_str(text).unwrap();
                if j["done"] == true {
                    break;
                }
                cursor = j["next_cursor"].as_str().map(String::from);
            }
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!("| scan {kind} include_props={props} 전체({pages}페이지, limit 1000) | {:.1} | {:.1} | {bytes} |", times[0], times[1]);
    }
    measure(
        &srv,
        "search mode=prop status=active (히트 750, 비교용)",
        "vgraph_search",
        json!({ "graph": g, "mode": "prop", "key": "status", "value": "active" }),
        runs,
    );
    measure(
        &srv,
        "search mode=prop n=1234 (히트 1)",
        "vgraph_search",
        json!({ "graph": g, "mode": "prop", "key": "n", "value": 1234 }),
        runs,
    );
}

// ================= M1: 모르는 칸이 있는 가져오기 문서 거부 =================

/// [핵심] `labels`를 `label`로 쓴 문서 → 도구 오류에 위치가 있고 그래프는 그대로다 (replace·merge 모두).
#[test]
fn import_rejects_unknown_field_with_location_and_leaves_graph_unchanged() {
    let (_dir, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": "acme/a" }));
    let typo = json!({
        "format": "vgraph-json/1",
        "nodes": [
            { "slug": "x", "labels": ["Item"], "props": {} },
            { "slug": "y", "label": ["Item"], "props": {} }
        ],
        "edges": [ { "etype": "t", "from": "x", "to": "y", "kind": "k" } ]
    });
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/a", "document": typo }),
    );
    assert!(
        e.contains("nodes[1].label") && e.contains("edges[0].kind"),
        "{e}"
    );
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/a", "mode": "merge", "via_branch": "import", "document": typo }),
    );
    assert!(e.contains("nodes[1].label"), "{e}");
    let list = call(&srv, "vgraph_list", json!({}));
    assert_eq!(list["graphs"][0]["head"], J::Null, "커밋 0");
    let p = call(&srv, "vgraph_pending", json!({ "graph": "acme/a" }));
    assert_eq!(p["working"], J::Null, "작업분 흔적 없음: {p}");
}

/// 도구 설명문에 거부 규칙과 예시 오류가 있다.
#[test]
fn import_tool_description_mentions_unknown_field_rejection() {
    let defs = tool_defs();
    let im = defs.iter().find(|t| t["name"] == "vgraph_import").unwrap()["description"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        im.contains("모르는 칸") && im.contains("nodes[3].label"),
        "{im}"
    );
}

// ---------- 브랜치 이름: "/" 허용, 규칙 밖 이름은 만들 때 거부 ----------

fn seeded_for_branches(srv: &McpServer) {
    call(srv, "vgraph_create", json!({ "name": "acme/br" }));
    call(
        srv,
        "vgraph_edit",
        json!({ "graph": "acme/br", "ops": [
            { "op": "add_node", "labels": ["Item"], "slug": "n1", "props": { "v": 0 } } ] }),
    );
    call(
        srv,
        "vgraph_commit",
        json!({ "graph": "acme/br", "message": "seed" }),
    );
}

/// [핵심] `feature/x` 를 만들고 `acme/br@feature/x` 로 조회·편집·커밋·병합한다.
#[test]
fn branch_with_slash_create_show_edit_commit_merge() {
    let (_d, srv) = server();
    seeded_for_branches(&srv);
    let b = call(
        &srv,
        "vgraph_branch",
        json!({ "graph": "acme/br", "name": "feature/x" }),
    );
    assert_eq!(b["branch"], "feature/x");
    let shown = call(
        &srv,
        "vgraph_show",
        json!({ "addr": "acme/br@feature/x#n1" }),
    );
    assert_eq!(shown["props"]["v"], 0, "{shown}");
    call(
        &srv,
        "vgraph_edit",
        json!({ "graph": "acme/br@feature/x",
            "ops": [{ "op": "set_prop", "node": "n1", "key": "v", "value": 5 }] }),
    );
    let c = call(
        &srv,
        "vgraph_commit",
        json!({ "graph": "acme/br@feature/x", "message": "x" }),
    );
    assert_eq!(c["outcome"], "committed", "{c}");
    let main_v = call(&srv, "vgraph_show", json!({ "addr": "acme/br#n1" }));
    assert_eq!(main_v["props"]["v"], 0, "main 은 그대로: {main_v}");
    let log = call(&srv, "vgraph_log", json!({ "graph": "acme/br@feature/x" }));
    assert_eq!(log["commits"].as_array().map(Vec::len), Some(2), "{log}");
    let m = call(
        &srv,
        "vgraph_merge",
        json!({ "graph": "acme/br", "src": "feature/x", "dst": "main" }),
    );
    assert_eq!(m["outcome"], "fast_forward", "{m}");
    let after = call(&srv, "vgraph_show", json!({ "addr": "acme/br#n1" }));
    assert_eq!(after["props"]["v"], 5, "{after}");
}

/// [핵심] 규칙 위반 이름은 만들 때 거부되고, 같은 문구가 전달된다. 거부 뒤 브랜치는 생기지 않는다.
#[test]
fn branch_with_invalid_name_is_rejected_with_rule_message() {
    let (_d, srv) = server();
    seeded_for_branches(&srv);
    for bad in ["a//b", "/a", "a/", "a b", "a.b"] {
        let e = call_err(
            &srv,
            "vgraph_branch",
            json!({ "graph": "acme/br", "name": bad }),
        );
        assert!(e.contains("branch") && e.contains("/"), "{bad:?}: {e}");
    }
    // 같은 문구가 주소로 들어온 규칙 밖 브랜치에도 쓰인다
    let e = call_err(&srv, "vgraph_show", json!({ "addr": "acme/br@a//b#n1" }));
    assert!(e.contains("branch") && e.contains("/"), "{e}");
}

// ---------- vgraph_find 선택 인자(order·normalize·any) ----------

const RANKED: &str = "acme/rank";

/// 정렬·정규화·여러 표현 시험용 그래프. 입력 순서가 uuid 순서다.
fn ranked_fixture(srv: &McpServer) {
    call(srv, "vgraph_create", json!({ "name": RANKED }));
    let ops = json!([
        node(
            "n-a",
            "item",
            json!({ "name": "소셜 로그인 안내", "desc": "외부 계정으로 접속" })
        ),
        node(
            "n-b",
            "item",
            json!({ "name": "로그인", "desc": "계정 접속" })
        ),
        node("n-c", "doc", json!({ "text": "로그-인 절차" })),
        node("n-d", "item", json!({ "name": "결제", "status": "active" })),
        node(
            "n-e",
            "item",
            json!({ "name": "배송 조회", "status": "active" })
        ),
    ]);
    call(srv, "vgraph_edit", json!({ "graph": RANKED, "ops": ops }));
    call(
        srv,
        "vgraph_commit",
        json!({ "graph": RANKED, "message": "seed" }),
    );
}

/// tools/call 응답 본문 원문(파싱 전).
fn call_raw(srv: &McpServer, name: &str, args: J) -> String {
    let resp = rpc(
        srv,
        "tools/call",
        json!({ "name": name, "arguments": args }),
    );
    assert!(
        !resp["result"]["isError"].as_bool().unwrap_or(false),
        "tool error: {resp}"
    );
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

fn find_slugs(r: &J) -> Vec<String> {
    r["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["slug"].as_str().unwrap().to_string())
        .collect()
}

/// [회귀] 옵션 없는 `vgraph_find` 응답은 이 스텝 이전 코드가 만든 바이트와 같다
/// (텍스트 있음·없음, 라벨·속성 필터, 커서 이어 받기). 기대값은 변경 전 코드의 실제 응답이다.
#[test]
fn find_without_new_options_response_bytes_unchanged() {
    let (_d, srv) = server();
    ranked_fixture(&srv);
    let first_page = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "text": "로그", "limit": 1 }),
    );
    let cursor = first_page["next_cursor"].as_str().unwrap().to_string();
    let cases = [
        (
            json!({ "graph": RANKED, "text": "로그인" }),
            r#"{
  "done": true,
  "epoch": "sha256:f97c5068d943dd3f9d46c23d8a14d128a3cf05260cba1fa537b426669c83b270",
  "items": [
    {
      "excerpt": "소셜 로그인 안내",
      "matched_key": "name",
      "slug": "n-a",
      "uuid": "00000000000000000000000003"
    },
    {
      "excerpt": "로그인",
      "matched_key": "name",
      "slug": "n-b",
      "uuid": "00000000000000000000000004"
    }
  ],
  "next_cursor": null
}"#,
        ),
        (
            json!({ "graph": RANKED, "labels": ["item"] }),
            r#"{
  "done": true,
  "epoch": "sha256:f97c5068d943dd3f9d46c23d8a14d128a3cf05260cba1fa537b426669c83b270",
  "items": [
    {
      "slug": "n-a",
      "uuid": "00000000000000000000000003"
    },
    {
      "slug": "n-b",
      "uuid": "00000000000000000000000004"
    },
    {
      "slug": "n-d",
      "uuid": "00000000000000000000000006"
    },
    {
      "slug": "n-e",
      "uuid": "00000000000000000000000007"
    }
  ],
  "next_cursor": null
}"#,
        ),
        (
            json!({ "graph": RANKED, "props": { "status": "active" } }),
            r#"{
  "done": true,
  "epoch": "sha256:f97c5068d943dd3f9d46c23d8a14d128a3cf05260cba1fa537b426669c83b270",
  "items": [
    {
      "slug": "n-d",
      "uuid": "00000000000000000000000006"
    },
    {
      "slug": "n-e",
      "uuid": "00000000000000000000000007"
    }
  ],
  "next_cursor": null
}"#,
        ),
        (
            json!({ "graph": RANKED, "text": "로그", "limit": 1 }),
            r#"{
  "done": false,
  "epoch": "sha256:f97c5068d943dd3f9d46c23d8a14d128a3cf05260cba1fa537b426669c83b270",
  "items": [
    {
      "excerpt": "소셜 로그인 안내",
      "matched_key": "name",
      "slug": "n-a",
      "uuid": "00000000000000000000000003"
    }
  ],
  "next_cursor": "sha256:f97c5068d943dd3f9d46c23d8a14d128a3cf05260cba1fa537b426669c83b270|00000000000000000000000003"
}"#,
        ),
        (
            json!({ "graph": RANKED, "text": "로그", "limit": 1, "cursor": cursor }),
            r#"{
  "done": false,
  "epoch": "sha256:f97c5068d943dd3f9d46c23d8a14d128a3cf05260cba1fa537b426669c83b270",
  "items": [
    {
      "excerpt": "로그인",
      "matched_key": "name",
      "slug": "n-b",
      "uuid": "00000000000000000000000004"
    }
  ],
  "next_cursor": "sha256:f97c5068d943dd3f9d46c23d8a14d128a3cf05260cba1fa537b426669c83b270|00000000000000000000000004"
}"#,
        ),
    ];
    for (args, expected) in cases {
        let got = call_raw(&srv, "vgraph_find", args.clone());
        assert_eq!(got, expected, "args: {args}");
    }
}

/// [핵심] uuid 순이면 `limit:1`에서 빠지는 완전 일치 노드가 `relevance`에서는 첫째로 나온다.
#[test]
fn find_relevance_puts_exact_match_first_with_limit_one() {
    let (_d, srv) = server();
    ranked_fixture(&srv);
    let plain = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "text": "로그인", "limit": 1 }),
    );
    assert_eq!(
        find_slugs(&plain),
        ["n-a"],
        "uuid 순은 부분 일치가 먼저: {plain}"
    );
    let ranked = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "text": "로그인", "order": "relevance", "limit": 1 }),
    );
    assert_eq!(find_slugs(&ranked), ["n-b"], "{ranked}");
    assert_eq!(ranked["order"], "relevance", "{ranked}");
    assert_eq!(ranked["next_cursor"], J::Null, "{ranked}");
    assert_eq!(ranked["total_matches"], 2, "{ranked}");
    assert_eq!(ranked["done"], false, "limit 에 잘렸음을 알린다: {ranked}");
    assert_eq!(ranked["items"][0]["score"], 30, "{ranked}");
}

/// `cursor`와 `relevance`를 함께 주면 거부한다.
#[test]
fn find_relevance_with_cursor_is_rejected() {
    let (_d, srv) = server();
    ranked_fixture(&srv);
    let first = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "text": "로그", "limit": 1 }),
    );
    let cursor = first["next_cursor"].as_str().unwrap();
    let e = call_err(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "text": "로그", "order": "relevance", "cursor": cursor }),
    );
    assert!(e.contains("cursor") && e.contains("relevance"), "{e}");
}

/// 잘못된 인자 형식은 명확한 오류.
#[test]
fn find_new_option_argument_errors() {
    let (_d, srv) = server();
    ranked_fixture(&srv);
    let e = call_err(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "order": "newest" }),
    );
    assert!(e.contains("order") && e.contains("relevance"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "normalize": "yes" }),
    );
    assert!(e.contains("normalize"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "any": "배송" }),
    );
    assert!(e.contains("any"), "{e}");
}

/// [핵심] `normalize`로 띄어쓰기·붙임표·밑줄을 무시하고 맞춘다. 발췌는 원문.
#[test]
fn find_normalize_ignores_space_hyphen_underscore() {
    let (_d, srv) = server();
    ranked_fixture(&srv);
    let plain = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "text": "로그인" }),
    );
    assert_eq!(
        find_slugs(&plain),
        ["n-a", "n-b"],
        "옵션 없이는 n-c 가 안 맞음"
    );
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "text": "로그인", "normalize": true }),
    );
    assert_eq!(find_slugs(&r), ["n-a", "n-b", "n-c"], "{r}");
    assert_eq!(
        r["items"][2]["excerpt"], "로그-인 절차",
        "발췌는 원문 기준: {r}"
    );
    assert!(
        r.get("order").is_none(),
        "relevance 가 아니면 형식 불변: {r}"
    );
    // 바늘 쪽 구분 문자도 무시
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "text": "로그_인", "normalize": true }),
    );
    assert_eq!(find_slugs(&r), ["n-a", "n-b", "n-c"], "{r}");
}

/// [핵심] `any`는 여러 표현 중 하나라도 맞으면 나온다. `text`도 후보 표현에 포함된다.
#[test]
fn find_any_is_or_and_includes_text() {
    let (_d, srv) = server();
    ranked_fixture(&srv);
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "any": ["결제", "배송"] }),
    );
    assert_eq!(find_slugs(&r), ["n-d", "n-e"], "{r}");
    assert_eq!(r["items"][0]["matched_expr"], "결제", "{r}");
    assert_eq!(r["items"][1]["matched_expr"], "배송", "{r}");
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "text": "로그인", "any": ["배송"] }),
    );
    assert_eq!(find_slugs(&r), ["n-a", "n-b", "n-e"], "{r}");
    // any 를 안 준 응답에는 matched_expr 가 없다
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "text": "로그인", "normalize": true }),
    );
    assert!(r["items"][0].get("matched_expr").is_none(), "{r}");
}

/// 옵션 조합: 라벨 필터 + any + normalize + relevance.
#[test]
fn find_options_combine_with_label_filter() {
    let (_d, srv) = server();
    ranked_fixture(&srv);
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": RANKED, "labels": ["item"], "any": ["로그 인", "배송"],
            "normalize": true, "order": "relevance" }),
    );
    // n-b: name 완전 일치(30) > n-e: 앞부분 일치(20, "배송 조회") > n-a: 부분 일치(10)
    assert_eq!(find_slugs(&r), ["n-b", "n-e", "n-a"], "{r}");
    assert_eq!(r["total_matches"], 3, "{r}");
    assert_eq!(r["done"], true, "{r}");
    assert_eq!(r["items"][1]["matched_expr"], "배송", "{r}");
    assert!(r["next_cursor"].is_null(), "{r}");
}

/// 도구 설명문에 새 인자와 예시가 있다.
#[test]
fn find_tool_description_mentions_new_options() {
    let defs = tool_defs();
    let find = defs
        .iter()
        .find(|t| t["name"] == "vgraph_find")
        .expect("vgraph_find");
    let desc = find["description"].as_str().unwrap();
    for needle in ["order", "relevance", "normalize", "any", "total_matches"] {
        assert!(desc.contains(needle), "설명문에 {needle} 없음: {desc}");
    }
    let props = &find["inputSchema"]["properties"];
    for k in ["order", "normalize", "any"] {
        let d = props[k]["description"].as_str().unwrap_or_default();
        assert!(d.contains("예"), "{k} 인자 설명에 예시 없음: {props}");
    }
}

// ---------- 주소 문자열 제외(skip_ref_like)와 간결 출력(compact) ----------

const COMPACT: &str = "acme/compact";

/// 간결 출력·주소 제외 시험용 그래프. 슬러그 없는 노드 하나와 엣지를 포함한다.
fn compact_fixture(srv: &McpServer) {
    call(srv, "vgraph_create", json!({ "name": COMPACT }));
    let ops = json!([
        node(
            "hub",
            "item",
            json!({ "name": "로그인 허브", "embed_target": "로그인 허브 안내" })
        ),
        // 값이 주소 형식뿐인 문자열 속성
        node(
            "n-a",
            "item",
            json!({ "name": "소셜 접속", "link": "graph://acme/rank#n-b", "embed_target": "소셜 접속 안내" })
        ),
        // 주소가 문장 일부로 섞인 값
        node(
            "n-b",
            "item",
            json!({ "name": "접속 문서", "desc": "자세한 내용은 graph://acme/rank#n-b 참고" })
        ),
        // 주소만 담은 문자열 리스트
        node(
            "n-c",
            "doc",
            json!({ "links": ["graph://acme/rank#n-a", "graph://acme/rank#n-c"] })
        ),
        // 주소 문자열과 일반 문장이 섞인 리스트
        node(
            "n-d",
            "doc",
            json!({ "links": ["graph://acme/rank#n-a", "참고: acme/rank 문서"] })
        ),
        // 앞뒤 공백이 붙은 주소뿐인 값
        node("n-e", "doc", json!({ "link": "  graph://acme/rank#n-e\n" })),
        // 주소 형식이 아닌(축약형) 값은 일반 텍스트다
        node("n-f", "doc", json!({ "link": "acme/rank#n-f" })),
        json!({ "op": "add_node", "labels": ["item"], "props": { "name": "무명 로그인 노드" } }),
    ]);
    call(srv, "vgraph_edit", json!({ "graph": COMPACT, "ops": ops }));
    call(
        srv,
        "vgraph_commit",
        json!({ "graph": COMPACT, "message": "seed" }),
    );
    let anon = call(
        srv,
        "vgraph_find",
        json!({ "graph": COMPACT, "text": "무명" }),
    );
    let anon_uuid = anon["items"][0]["uuid"].as_str().unwrap().to_string();
    let edges = json!([
        edge("참조", "hub", "n-a"),
        edge("참조", "hub", "n-b"),
        edge("관련", "n-a", &anon_uuid),
        edge("관련", &anon_uuid, "n-b"),
    ]);
    call(
        srv,
        "vgraph_edit",
        json!({ "graph": COMPACT, "ops": edges }),
    );
    call(
        srv,
        "vgraph_commit",
        json!({ "graph": COMPACT, "message": "edges" }),
    );
    call(srv, "vgraph_embed_pending", json!({ "graph": COMPACT }));
}

/// 옵션 없이 부른 `vgraph_neighbors`·`vgraph_scan`·`vgraph_search` 응답의 기준 바이트.
/// 기대값은 이 스텝 이전 코드(HEAD a1ab655)가 같은 픽스처로 실제로 낸 응답 원문이다.
const COMPACT_BASELINE: &[(&str, &str, &str)] = &[
    (
        "vgraph_neighbors",
        r#"{"addr":"acme/compact#hub","depth":2,"dir":"both"}"#,
        r#"{
  "epoch": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1",
  "hits": [
    {
      "depth": 1,
      "slug": "n-a",
      "uuid": "00000000000000000000000004",
      "via": "00000000000000000000000012"
    },
    {
      "depth": 1,
      "slug": "n-b",
      "uuid": "00000000000000000000000005",
      "via": "00000000000000000000000013"
    },
    {
      "depth": 2,
      "slug": null,
      "uuid": "00000000000000000000000010",
      "via": "00000000000000000000000014"
    }
  ]
}"#,
    ),
    (
        "vgraph_neighbors",
        r#"{"addr":"acme/compact#hub"}"#,
        r#"{
  "epoch": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1",
  "hits": [
    {
      "depth": 1,
      "slug": "n-a",
      "uuid": "00000000000000000000000004",
      "via": "00000000000000000000000012"
    },
    {
      "depth": 1,
      "slug": "n-b",
      "uuid": "00000000000000000000000005",
      "via": "00000000000000000000000013"
    }
  ]
}"#,
    ),
    (
        "vgraph_scan",
        r#"{"graph":"acme/compact","kind":"nodes"}"#,
        r#"{
  "done": true,
  "epoch": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1",
  "items": [
    {
      "labels": [
        "item"
      ],
      "slug": "hub",
      "uuid": "00000000000000000000000003"
    },
    {
      "labels": [
        "item"
      ],
      "slug": "n-a",
      "uuid": "00000000000000000000000004"
    },
    {
      "labels": [
        "item"
      ],
      "slug": "n-b",
      "uuid": "00000000000000000000000005"
    },
    {
      "labels": [
        "doc"
      ],
      "slug": "n-c",
      "uuid": "00000000000000000000000006"
    },
    {
      "labels": [
        "doc"
      ],
      "slug": "n-d",
      "uuid": "00000000000000000000000007"
    },
    {
      "labels": [
        "doc"
      ],
      "slug": "n-e",
      "uuid": "00000000000000000000000008"
    },
    {
      "labels": [
        "doc"
      ],
      "slug": "n-f",
      "uuid": "00000000000000000000000009"
    },
    {
      "labels": [
        "item"
      ],
      "slug": null,
      "uuid": "00000000000000000000000010"
    }
  ],
  "kind": "nodes",
  "next_cursor": null
}"#,
    ),
    (
        "vgraph_scan",
        r#"{"graph":"acme/compact","kind":"nodes","limit":3}"#,
        r#"{
  "done": false,
  "epoch": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1",
  "items": [
    {
      "labels": [
        "item"
      ],
      "slug": "hub",
      "uuid": "00000000000000000000000003"
    },
    {
      "labels": [
        "item"
      ],
      "slug": "n-a",
      "uuid": "00000000000000000000000004"
    },
    {
      "labels": [
        "item"
      ],
      "slug": "n-b",
      "uuid": "00000000000000000000000005"
    }
  ],
  "kind": "nodes",
  "next_cursor": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1|00000000000000000000000005"
}"#,
    ),
    (
        "vgraph_scan",
        r#"{"graph":"acme/compact","kind":"edges"}"#,
        r#"{
  "done": true,
  "epoch": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1",
  "items": [
    {
      "from": "00000000000000000000000003",
      "from_slug": "hub",
      "to": "00000000000000000000000004",
      "to_slug": "n-a",
      "type": "참조",
      "uuid": "00000000000000000000000012"
    },
    {
      "from": "00000000000000000000000003",
      "from_slug": "hub",
      "to": "00000000000000000000000005",
      "to_slug": "n-b",
      "type": "참조",
      "uuid": "00000000000000000000000013"
    },
    {
      "from": "00000000000000000000000004",
      "from_slug": "n-a",
      "to": "00000000000000000000000010",
      "type": "관련",
      "uuid": "00000000000000000000000014"
    },
    {
      "from": "00000000000000000000000010",
      "to": "00000000000000000000000005",
      "to_slug": "n-b",
      "type": "관련",
      "uuid": "00000000000000000000000015"
    }
  ],
  "kind": "edges",
  "next_cursor": null
}"#,
    ),
    (
        "vgraph_scan",
        r#"{"graph":"acme/compact","include_props":true,"kind":"nodes","limit":2}"#,
        r#"{
  "done": false,
  "epoch": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1",
  "items": [
    {
      "labels": [
        "item"
      ],
      "props": {
        "embed_target": "로그인 허브 안내",
        "name": "로그인 허브"
      },
      "slug": "hub",
      "uuid": "00000000000000000000000003"
    },
    {
      "labels": [
        "item"
      ],
      "props": {
        "embed_target": "소셜 접속 안내",
        "link": "graph://acme/rank#n-b",
        "name": "소셜 접속"
      },
      "slug": "n-a",
      "uuid": "00000000000000000000000004"
    }
  ],
  "kind": "nodes",
  "next_cursor": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1|00000000000000000000000004"
}"#,
    ),
    (
        "vgraph_scan",
        r#"{"graph":"acme/compact","include_props":true,"kind":"edges","limit":2}"#,
        r#"{
  "done": false,
  "epoch": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1",
  "items": [
    {
      "from": "00000000000000000000000003",
      "from_slug": "hub",
      "props": {},
      "to": "00000000000000000000000004",
      "to_slug": "n-a",
      "type": "참조",
      "uuid": "00000000000000000000000012"
    },
    {
      "from": "00000000000000000000000003",
      "from_slug": "hub",
      "props": {},
      "to": "00000000000000000000000005",
      "to_slug": "n-b",
      "type": "참조",
      "uuid": "00000000000000000000000013"
    }
  ],
  "kind": "edges",
  "next_cursor": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1|00000000000000000000000013"
}"#,
    ),
    (
        "vgraph_search",
        r#"{"graph":"acme/compact","key":"name","mode":"prop","value":"로그인 허브"}"#,
        r#"{
  "epoch": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1",
  "hits": [
    {
      "slug": "hub",
      "uuid": "00000000000000000000000003"
    }
  ],
  "mode": "prop"
}"#,
    ),
    (
        "vgraph_search",
        r#"{"graph":"acme/compact","k":3,"mode":"similar","query":"로그인"}"#,
        r#"{
  "epoch": "sha256:45f4a5c1e9d227ef50023f18f5c6861dcea5d606911a695ddc0833d9bcb4c4d1",
  "hits": [
    {
      "part": "full",
      "score": 0.6324555277824402,
      "slug": null,
      "uuid": "00000000000000000000000010"
    },
    {
      "part": "t0",
      "score": 0.5773502588272095,
      "slug": "hub",
      "uuid": "00000000000000000000000003"
    },
    {
      "part": "full",
      "score": 0.29711252450942993,
      "slug": "n-b",
      "uuid": "00000000000000000000000005"
    }
  ],
  "mode": "similar",
  "model": "toy-32",
  "pending_embeddings": 0
}"#,
    ),
];

fn size_of_call(srv: &McpServer, tool: &str, args: &J) -> usize {
    call_raw(srv, tool, args.clone()).len()
}

fn with_compact(args: &J) -> J {
    let mut a = args.clone();
    a["compact"] = json!(true);
    a
}

/// [회귀] 옵션이 없거나 `compact:false`·`null`이면 응답은 변경 전 바이트와 같다.
#[test]
fn compact_off_responses_bytes_unchanged() {
    let (_d, srv) = server();
    compact_fixture(&srv);
    for (tool, args, expected) in COMPACT_BASELINE {
        let args: J = serde_json::from_str(args).unwrap();
        assert_eq!(
            &call_raw(&srv, tool, args.clone()),
            expected,
            "{tool} {args}"
        );
        for off in [json!(false), J::Null] {
            let mut a = args.clone();
            a["compact"] = off.clone();
            assert_eq!(
                &call_raw(&srv, tool, a),
                expected,
                "{tool} compact={off} {args}"
            );
        }
    }
}

const UUID_ANON: &str = "00000000000000000000000010";

/// [핵심] compact 응답에는 정한 고정 칸이 없다 — 도구별 정확한 모양을 고정한다.
#[test]
fn compact_drops_fixed_columns_for_all_four_tools() {
    let (_d, srv) = server();
    compact_fixture(&srv);

    // neighbors (기존 경로): epoch·via 없음, slug 있으면 uuid 없음, slug 없으면 uuid 유지
    let r = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": "acme/compact#hub", "dir": "both", "depth": 2, "compact": true }),
    );
    assert_eq!(
        r,
        json!({ "hits": [
            { "depth": 1, "slug": "n-a" },
            { "depth": 1, "slug": "n-b" },
            { "depth": 2, "uuid": UUID_ANON },
        ] })
    );

    // neighbors (새 인자 경로): via 는 빠지고 via_type·truncated 는 남는다
    let r = call(
        &srv,
        "vgraph_neighbors",
        json!({ "addr": "acme/compact#hub", "limit": 1, "include": ["labels"], "compact": true }),
    );
    assert_eq!(
        r,
        json!({ "hits": [
            { "depth": 1, "slug": "n-a", "via_type": "참조", "labels": ["item"] },
        ], "truncated": true })
    );

    // scan nodes
    let r = call(
        &srv,
        "vgraph_scan",
        json!({ "graph": COMPACT, "kind": "nodes", "limit": 3, "compact": true }),
    );
    assert_eq!(r["kind"], "nodes");
    assert_eq!(r["done"], false);
    assert!(r.get("epoch").is_none(), "{r}");
    assert_eq!(
        r["items"],
        json!([
            { "slug": "hub", "labels": ["item"] },
            { "slug": "n-a", "labels": ["item"] },
            { "slug": "n-b", "labels": ["item"] },
        ])
    );
    assert!(r["next_cursor"].as_str().unwrap().contains('|'), "{r}");

    // scan edges: 엣지 uuid 없음, 양끝은 slug 가 있으면 *_slug 만, 없으면 uuid(from/to)
    let r = call(
        &srv,
        "vgraph_scan",
        json!({ "graph": COMPACT, "kind": "edges", "compact": true }),
    );
    assert!(r.get("epoch").is_none(), "{r}");
    assert_eq!(
        r["items"],
        json!([
            { "type": "참조", "from_slug": "hub", "to_slug": "n-a" },
            { "type": "참조", "from_slug": "hub", "to_slug": "n-b" },
            { "type": "관련", "from_slug": "n-a", "to": UUID_ANON },
            { "type": "관련", "from": UUID_ANON, "to_slug": "n-b" },
        ])
    );

    // search prop
    let r = call(
        &srv,
        "vgraph_search",
        json!({ "graph": COMPACT, "mode": "prop", "key": "name", "value": "로그인 허브", "compact": true }),
    );
    assert_eq!(r, json!({ "mode": "prop", "hits": [{ "slug": "hub" }] }));

    // search similar: score·part·model 은 남고 epoch 와 slug 있는 hit 의 uuid 는 빠진다
    let r = call(
        &srv,
        "vgraph_search",
        json!({ "graph": COMPACT, "mode": "similar", "query": "로그인", "k": 3, "compact": true }),
    );
    assert!(r.get("epoch").is_none(), "{r}");
    assert_eq!(r["mode"], "similar");
    assert_eq!(r["model"], "toy-32");
    assert_eq!(r["pending_embeddings"], 0);
    let hits = r["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0]["uuid"], UUID_ANON);
    assert!(
        hits[0].get("slug").is_none(),
        "slug 없는 hit 은 uuid 만: {r}"
    );
    assert_eq!(hits[1]["slug"], "hub");
    assert!(hits[1].get("uuid").is_none(), "{r}");
    for h in hits {
        assert!(h["score"].is_number() && h["part"].is_string(), "{h}");
    }

    // find: epoch 없음, matched_key·excerpt 유지
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": COMPACT, "text": "로그인", "compact": true }),
    );
    assert!(r.get("epoch").is_none(), "{r}");
    assert_eq!(r["done"], true);
    assert_eq!(r["next_cursor"], J::Null);
    assert_eq!(
        r["items"],
        json!([
            { "slug": "hub", "matched_key": "embed_target", "excerpt": "로그인 허브 안내" },
            { "uuid": UUID_ANON, "matched_key": "name", "excerpt": "무명 로그인 노드" },
        ])
    );
}

/// 질의별 응답 크기 시험용: (도구, 설명, 인자). 같은 질의를 옵션 없이·compact 로 부른다.
fn compact_size_cases() -> Vec<(&'static str, &'static str, J)> {
    vec![
        (
            "vgraph_find",
            "text=로그인, include labels",
            json!({ "graph": COMPACT, "text": "로그인", "include": ["labels"] }),
        ),
        ("vgraph_find", "전체 나열", json!({ "graph": COMPACT })),
        (
            "vgraph_neighbors",
            "hub both depth 2",
            json!({ "addr": "acme/compact#hub", "dir": "both", "depth": 2 }),
        ),
        (
            "vgraph_scan",
            "nodes",
            json!({ "graph": COMPACT, "kind": "nodes" }),
        ),
        (
            "vgraph_scan",
            "edges",
            json!({ "graph": COMPACT, "kind": "edges" }),
        ),
        (
            "vgraph_search",
            "prop",
            json!({ "graph": COMPACT, "mode": "prop", "key": "name", "value": "로그인 허브" }),
        ),
        (
            "vgraph_search",
            "similar k=3",
            json!({ "graph": COMPACT, "mode": "similar", "query": "로그인", "k": 3 }),
        ),
    ]
}

/// [핵심] 같은 질의의 compact 응답은 옵션 없는 응답보다 작다(수치는 --nocapture 로 본다).
#[test]
fn compact_responses_are_smaller_for_all_four_tools() {
    let (_d, srv) = server();
    compact_fixture(&srv);
    for (tool, what, args) in compact_size_cases() {
        let plain = size_of_call(&srv, tool, &args);
        let compact = size_of_call(&srv, tool, &with_compact(&args));
        eprintln!(
            "SIZE {tool:<17} {what:<26} plain={plain:>5} compact={compact:>5} ratio={:.2}",
            plain as f64 / compact as f64
        );
        assert!(compact < plain, "{tool} {what}: {compact} !< {plain}");
    }
}

/// [중요] compact 에서도 커서로 이어 받는다 — 커서는 빼지 않고 모양도 같다.
#[test]
fn compact_keeps_cursor_paging() {
    let (_d, srv) = server();
    compact_fixture(&srv);

    // scan nodes 를 3개씩: 모든 노드를 한 번씩 만난다(slug 없는 노드는 uuid 로)
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let mut a = json!({ "graph": COMPACT, "kind": "nodes", "limit": 3, "compact": true });
        if let Some(c) = &cursor {
            a["cursor"] = json!(c);
        }
        let r = call(&srv, "vgraph_scan", a);
        for i in r["items"].as_array().unwrap() {
            seen.push(
                i["slug"]
                    .as_str()
                    .or_else(|| i["uuid"].as_str())
                    .unwrap()
                    .to_string(),
            );
        }
        if r["done"] == true {
            assert_eq!(r["next_cursor"], J::Null);
            break;
        }
        cursor = Some(r["next_cursor"].as_str().unwrap().to_string());
    }
    assert_eq!(
        seen,
        ["hub", "n-a", "n-b", "n-c", "n-d", "n-e", "n-f", UUID_ANON]
    );

    // find: compact 첫 쪽의 커서를 compact 호출에도, compact 아닌 호출에도 넘길 수 있다
    let first = call(
        &srv,
        "vgraph_find",
        json!({ "graph": COMPACT, "text": "로그인", "limit": 1, "compact": true }),
    );
    assert_eq!(first["items"][0]["slug"], "hub");
    assert_eq!(first["done"], false);
    let cur = first["next_cursor"].as_str().unwrap();
    let second = call(
        &srv,
        "vgraph_find",
        json!({ "graph": COMPACT, "text": "로그인", "limit": 1, "cursor": cur, "compact": true }),
    );
    assert_eq!(second["items"][0]["uuid"], UUID_ANON);
    assert_eq!(second["done"], true);
    let plain_second = call(
        &srv,
        "vgraph_find",
        json!({ "graph": COMPACT, "text": "로그인", "limit": 1, "cursor": cur }),
    );
    assert_eq!(plain_second["items"][0]["uuid"], UUID_ANON);
}

/// 값이 그래프 주소 형식뿐인 속성만 빠진다. 문장 속 주소·축약형·일반 텍스트는 그대로.
#[test]
fn skip_ref_like_drops_nodes_matching_only_by_address_string() {
    let (_d, srv) = server();
    compact_fixture(&srv);
    let slugs = |args: J| find_slugs(&call(&srv, "vgraph_find", args));

    // 끄면(기본) 주소 문자열도 텍스트로 맞는다
    assert_eq!(
        slugs(json!({ "graph": COMPACT, "text": "acme/rank" })),
        ["n-a", "n-b", "n-c", "n-d", "n-e", "n-f"]
    );
    assert_eq!(
        slugs(json!({ "graph": COMPACT, "text": "acme/rank", "skip_ref_like": false })),
        ["n-a", "n-b", "n-c", "n-d", "n-e", "n-f"]
    );
    // 켜면: n-a(주소뿐), n-c(주소뿐인 리스트), n-e(공백만 감싼 주소)가 빠진다
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": COMPACT, "text": "acme/rank", "skip_ref_like": true }),
    );
    assert_eq!(find_slugs(&r), ["n-b", "n-d", "n-f"]);
    // 섞인 리스트는 일반 문장 원소로 맞고, 발췌도 그 원소에서 나온다
    let n_d = &r["items"][1];
    assert_eq!(n_d["matched_key"], "links");
    assert_eq!(n_d["excerpt"], "참고: acme/rank 문서");
    // 문장 속 주소·축약형은 그대로 맞는다
    assert_eq!(r["items"][0]["matched_key"], "desc");
    assert_eq!(r["items"][2]["matched_key"], "link");

    assert_eq!(
        slugs(json!({ "graph": COMPACT, "text": "graph://acme", "skip_ref_like": true })),
        ["n-b"]
    );
    // 주소와 무관한 일치는 영향이 없다
    assert_eq!(
        call(
            &srv,
            "vgraph_find",
            json!({ "graph": COMPACT, "text": "로그인", "skip_ref_like": true })
        ),
        call(
            &srv,
            "vgraph_find",
            json!({ "graph": COMPACT, "text": "로그인" })
        ),
    );
    // 텍스트 조건이 없으면 효과 없음
    assert_eq!(
        slugs(json!({ "graph": COMPACT, "labels": ["doc"], "skip_ref_like": true })),
        ["n-c", "n-d", "n-e", "n-f"]
    );
}

/// [중요] 옵션 조합: relevance·normalize·any·compact 와 함께 쓸 수 있다.
#[test]
fn skip_ref_like_combines_with_other_find_options() {
    let (_d, srv) = server();
    compact_fixture(&srv);
    // relevance + skip_ref_like + compact
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": COMPACT, "text": "acme/rank", "order": "relevance",
                "skip_ref_like": true, "compact": true, "limit": 10 }),
    );
    assert!(r.get("epoch").is_none(), "{r}");
    assert_eq!(r["order"], "relevance");
    assert_eq!(r["total_matches"], 3);
    // n-f 는 앞부분 일치(20), n-b·n-d 는 부분 일치(10, 동점은 uuid 순)
    assert_eq!(find_slugs(&r), ["n-f", "n-b", "n-d"]);
    assert_eq!(r["items"][0]["score"], 20);
    assert!(r["items"][0].get("uuid").is_none(), "{r}");

    // any + normalize + skip_ref_like
    let r = call(
        &srv,
        "vgraph_find",
        json!({ "graph": COMPACT, "any": ["참고: ACME", "graph://acme/rank#n-b"],
                "normalize": true, "skip_ref_like": true }),
    );
    // 두 번째 표현은 n-b 의 문장 속 주소에는 맞고, n-a·n-c·n-e 의 주소뿐인 값은 건너뛴다
    assert_eq!(find_slugs(&r), ["n-b", "n-d"]);
    assert_eq!(r["items"][0]["matched_expr"], "graph://acme/rank#n-b");
    assert_eq!(r["items"][1]["matched_expr"], "참고: ACME");
}

/// [중요] 잘못된 인자 타입은 오류다(조용히 무시하지 않는다).
#[test]
fn compact_and_skip_ref_like_reject_wrong_argument_types() {
    let (_d, srv) = server();
    compact_fixture(&srv);
    let bases = [
        ("vgraph_find", json!({ "graph": COMPACT, "text": "a" })),
        ("vgraph_neighbors", json!({ "addr": "acme/compact#hub" })),
        ("vgraph_scan", json!({ "graph": COMPACT, "kind": "nodes" })),
        (
            "vgraph_search",
            json!({ "graph": COMPACT, "mode": "prop", "key": "name", "value": "x" }),
        ),
        (
            "vgraph_search",
            json!({ "graph": COMPACT, "mode": "similar", "query": "x" }),
        ),
    ];
    for (tool, base) in &bases {
        for bad in [json!("yes"), json!(1), json!([true])] {
            let mut a = base.clone();
            a["compact"] = bad.clone();
            let e = call_err(&srv, tool, a);
            assert!(
                e.contains("compact") && e.contains("true/false"),
                "{tool} {bad}: {e}"
            );
        }
    }
    for bad in [json!("yes"), json!(1), json!({})] {
        let e = call_err(
            &srv,
            "vgraph_find",
            json!({ "graph": COMPACT, "text": "a", "skip_ref_like": bad }),
        );
        assert!(
            e.contains("skip_ref_like") && e.contains("true/false"),
            "{bad}: {e}"
        );
    }
}

/// [중요] 도구 설명문·스키마에 새 인자와 예시가 있고, 다른 도구는 손대지 않았다.
#[test]
fn compact_and_skip_ref_like_are_documented_only_on_target_tools() {
    let defs = tool_defs();
    assert_eq!(defs.len(), 20, "도구 수 불변");
    let targets = [
        "vgraph_find",
        "vgraph_neighbors",
        "vgraph_scan",
        "vgraph_search",
    ];
    let by = |n: &str| defs.iter().find(|d| d["name"] == n).unwrap().clone();
    for n in targets {
        let d = by(n);
        let desc = d["description"].as_str().unwrap();
        assert!(desc.contains("compact"), "{n} 설명문에 compact");
        assert!(
            desc.contains("\"compact\":true"),
            "{n} 설명문에 compact 예시"
        );
        let p = &d["inputSchema"]["properties"]["compact"];
        assert_eq!(p["type"], "boolean", "{n}: {d}");
        assert!(p["description"].as_str().unwrap().contains("예"), "{n}");
    }
    let find = by("vgraph_find");
    let desc = find["description"].as_str().unwrap();
    assert!(desc.contains("skip_ref_like"));
    assert!(desc.contains("graph://"), "주소 판정 규칙 문서화");
    assert!(desc.contains("\"skip_ref_like\":true"), "예시");
    let p = &find["inputSchema"]["properties"]["skip_ref_like"];
    assert_eq!(p["type"], "boolean");
    assert!(p["description"].as_str().unwrap().contains("예"));
    for d in &defs {
        let n = d["name"].as_str().unwrap();
        let props = &d["inputSchema"]["properties"];
        if !targets.contains(&n) {
            assert!(
                props.get("compact").is_none(),
                "{n}에 compact 가 있으면 안 됨"
            );
        }
        if n != "vgraph_find" {
            assert!(props.get("skip_ref_like").is_none(), "{n}");
        }
    }
}
