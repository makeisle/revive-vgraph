//! 내보내기 나눠 받기(`max_bytes`·`cursor`)와 가져오기 나눠 올리기(`upload`) 시험.
//! 모킹 없음: 실제 엔진 + 임시 디렉터리, 시각·ID 발급만 시험용(TestClock/TestIdGen).

use serde_json::{json, Value as J};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use vgraph_core::{TestClock, TestIdGen};
use vgraph_engine::{Engine, EngineOptions, FsProvider};
use vgraph_mcp::{Identity, McpConfig, McpServer, UploadLimits};

const GRAPH: &str = "acme/catalog";
const DST: &str = "acme/copy";

fn server_with(user: &str) -> (TempDir, McpServer) {
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
        user: user.into(),
        embedder: None,
        embed: Default::default(),
    };
    (dir, McpServer::with_engine(engine, cfg))
}

fn server() -> (TempDir, McpServer) {
    server_with("owner")
}

fn raw_as(srv: &McpServer, id: Option<&Identity>, name: &str, args: J) -> (bool, J) {
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args } });
    let resp = match id {
        Some(i) => srv.handle_message_as(&msg, i),
        None => srv.handle_message(&msg),
    }
    .expect("response");
    let r = &resp["result"];
    let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
    let is_err = r["isError"].as_bool().unwrap_or(false);
    let body = serde_json::from_str(&text).unwrap_or(J::String(text));
    (is_err, body)
}

fn call(srv: &McpServer, name: &str, args: J) -> J {
    let (e, b) = raw_as(srv, None, name, args);
    assert!(!e, "tool error ({name}): {b}");
    b
}

fn call_err(srv: &McpServer, name: &str, args: J) -> String {
    let (e, b) = raw_as(srv, None, name, args);
    assert!(e, "오류 기대 ({name}): {b}");
    b.as_str()
        .map(String::from)
        .unwrap_or_else(|| b.to_string())
}

fn call_as(srv: &McpServer, id: &Identity, name: &str, args: J) -> J {
    let (e, b) = raw_as(srv, Some(id), name, args);
    assert!(!e, "tool error ({name}): {b}");
    b
}

fn call_err_as(srv: &McpServer, id: &Identity, name: &str, args: J) -> String {
    let (e, b) = raw_as(srv, Some(id), name, args);
    assert!(e, "오류 기대 ({name}): {b}");
    b.as_str()
        .map(String::from)
        .unwrap_or_else(|| b.to_string())
}

fn node(slug: &str, label: &str, props: J) -> J {
    json!({ "op": "add_node", "labels": [label], "slug": slug, "props": props })
}

/// 작은 그래프(노드 3·엣지 2, 한글 포함) — 커밋 1개.
fn small_graph(srv: &McpServer, name: &str) {
    call(srv, "vgraph_create", json!({ "name": name }));
    call(
        srv,
        "vgraph_edit",
        json!({ "graph": name, "ops": [
            node("a-1", "Item", json!({ "name": "첫째 노드", "n": 1 })),
            node("b-2", "Item", json!({ "name": "둘째 노드", "n": 2 })),
            node("c-3", "Rule", json!({ "name": "규칙 🙂", "n": 3 })),
            { "op": "add_edge", "etype": "참조", "from": "a-1", "to": "c-3" },
            { "op": "add_edge", "etype": "참조", "from": "b-2", "to": "c-3" },
        ] }),
    );
    call(
        srv,
        "vgraph_commit",
        json!({ "graph": name, "message": "seed" }),
    );
}

fn tmp_file(dir: &TempDir, name: &str) -> String {
    dir.path().join(name).to_string_lossy().into_owned()
}

/// 한 번에 내보낸 문서의 바이트(파일 내보내기).
fn export_bytes(srv: &McpServer, dir: &TempDir, graph: &str, file: &str) -> String {
    let path = tmp_file(dir, file);
    call(
        srv,
        "vgraph_export",
        json!({ "graph": graph, "path": path }),
    );
    std::fs::read_to_string(path).unwrap()
}

/// 브랜치의 커밋 수. 커밋이 하나도 없는 그래프는 main 브랜치가 아직 없어 0.
fn commit_count(srv: &McpServer, graph: &str) -> usize {
    let (is_err, body) = raw_as(srv, None, "vgraph_log", json!({ "graph": graph }));
    if is_err {
        assert_eq!(body, json!("unknown branch"), "{body}");
        return 0;
    }
    body["commits"].as_array().unwrap().len()
}

// ================= 옵션 없는 응답 불변 (변경 전 코드 59a9876에서 받은 값) =================

const BASE_EXPORT_INLINE: &str = r#"{"document":{"edges":[{"etype":"참조","from":"a-1","props":{},"to":"c-3"},{"etype":"참조","from":"b-2","props":{},"to":"c-3"}],"format":"vgraph-json/1","nodes":[{"labels":["Item"],"props":{"n":1,"name":"첫째 노드"},"slug":"a-1"},{"labels":["Item"],"props":{"n":2,"name":"둘째 노드"},"slug":"b-2"},{"labels":["Rule"],"props":{"n":3,"name":"규칙 🙂"},"slug":"c-3"}]},"edges":2,"epoch":"sha256:c9e44ac4bcb9024f6b61b6b5b058006253dae3a9268aef027e88ff948aca9494","nodes":3}"#;
const BASE_EXPORT_PATH: &str = r#"{"edges":2,"epoch":"sha256:c9e44ac4bcb9024f6b61b6b5b058006253dae3a9268aef027e88ff948aca9494","nodes":3}"#;
const BASE_IMPORT_REPLACE: &str = r#"{"commit":"sha256:169263a98adf3bc545562d4508b72779b8cbbf082450b8ab47d181ea613f5026","edges":2,"nodes":3,"outcome":"committed","warnings":[]}"#;
const BASE_IMPORT_MERGE_FIRST: &str = r#"{"commit":"sha256:b9739fdac276118dabdc02df9d77c9ec440ab8ea10af603fad4480e9f5ec5213","edges":2,"nodes":3,"outcome":"first_import","via_branch":"imp","via_commit":"sha256:b9739fdac276118dabdc02df9d77c9ec440ab8ea10af603fad4480e9f5ec5213","warnings":[]}"#;
const BASE_IMPORT_MERGE_SAME: &str = r#"{"commit":"sha256:b9739fdac276118dabdc02df9d77c9ec440ab8ea10af603fad4480e9f5ec5213","edges":2,"nodes":3,"outcome":"up_to_date","via_branch":"imp","via_commit":null,"warnings":[]}"#;
const BASE_ERR_MERGE_NO_VIA: &str =
    "mode=merge에는 via_branch(가져오기 전용 브랜치 이름)가 필요합니다 — 기본값 없음";
const BASE_ERR_NO_DOC: &str = "document와 path 중 정확히 하나를 주세요";
const BASE_ERR_UNKNOWN_FIELD: &str =
    "가져오기 문서에 모르는 칸 1 건이 있어 거부됨 (아무것도 넣지 않음): nodes[0].label";
const BASE_ERR_NOT_EMPTY: &str =
    "replace 가져오기는 빈 그래프에만 가능합니다 (현재 노드 3개, 엣지 2개)";

#[test]
fn without_options_export_and_import_responses_are_byte_identical_to_before() {
    let (dir, srv) = server();
    small_graph(&srv, GRAPH);
    let inline = call(&srv, "vgraph_export", json!({ "graph": GRAPH }));
    assert_eq!(inline.to_string(), BASE_EXPORT_INLINE);
    let mut with_path = call(
        &srv,
        "vgraph_export",
        json!({ "graph": GRAPH, "path": tmp_file(&dir, "out.json") }),
    );
    with_path.as_object_mut().unwrap().remove("path");
    assert_eq!(with_path.to_string(), BASE_EXPORT_PATH);

    call(&srv, "vgraph_create", json!({ "name": DST }));
    let imp = call(
        &srv,
        "vgraph_import",
        json!({ "graph": DST, "document": inline["document"] }),
    );
    assert_eq!(imp.to_string(), BASE_IMPORT_REPLACE);
    call(&srv, "vgraph_create", json!({ "name": "acme/m" }));
    let merge = json!({ "graph": "acme/m", "mode": "merge", "via_branch": "imp",
        "document": inline["document"] });
    assert_eq!(
        call(&srv, "vgraph_import", merge.clone()).to_string(),
        BASE_IMPORT_MERGE_FIRST
    );
    assert_eq!(
        call(&srv, "vgraph_import", merge).to_string(),
        BASE_IMPORT_MERGE_SAME
    );
    let doc = inline["document"].clone();
    assert_eq!(
        call_err(
            &srv,
            "vgraph_import",
            json!({ "graph": DST, "mode": "merge", "document": doc })
        ),
        BASE_ERR_MERGE_NO_VIA
    );
    assert_eq!(
        call_err(&srv, "vgraph_import", json!({ "graph": DST })),
        BASE_ERR_NO_DOC
    );
    let bad = json!({"format":"vgraph-json/1","nodes":[{"slug":"x","label":["A"],"props":{}}],"edges":[]});
    assert_eq!(
        call_err(
            &srv,
            "vgraph_import",
            json!({ "graph": DST, "document": bad })
        ),
        BASE_ERR_UNKNOWN_FIELD
    );
    assert_eq!(
        call_err(
            &srv,
            "vgraph_import",
            json!({ "graph": GRAPH, "document": doc })
        ),
        BASE_ERR_NOT_EMPTY
    );
}

// ================= 내보내기 나눠 받기 =================

const BIG_NODES: usize = 4_200;
const PART: usize = 64 * 1024;

/// 3MB가 넘는 그래프(한글·이모지 포함)를 한 번에 가져오기로 만든다.
fn big_graph(srv: &McpServer, name: &str) {
    call(srv, "vgraph_create", json!({ "name": name }));
    let mut doc = String::from("{\"format\":\"vgraph-json/1\",\"nodes\":[");
    for i in 0..BIG_NODES {
        if i > 0 {
            doc.push(',');
        }
        doc.push_str(&format!(
            "{{\"slug\":\"n-{i:05}\",\"labels\":[\"Item\"],\"props\":{{\"name\":\"한글 노드 {i} 🙂\",\"note\":\"{}{i}\"}}}}",
            "내용 설명 문장입니다 가나다라마바사 ".repeat(16)
        ));
    }
    doc.push_str("],\"edges\":[]}");
    call(
        srv,
        "vgraph_import",
        json!({ "graph": name, "document": doc }),
    );
}

/// 조각을 끝까지 받아 이어 붙인다. `between`은 조각 `n`개를 받은 뒤마다 불린다.
fn download(
    srv: &McpServer,
    graph: &str,
    max: usize,
    mut between: impl FnMut(usize),
) -> (String, usize, J) {
    let mut out = String::new();
    let mut r = call(
        srv,
        "vgraph_export",
        json!({ "graph": graph, "max_bytes": max }),
    );
    let first = r.clone();
    let mut n = 1usize;
    loop {
        out.push_str(r["chunk"].as_str().expect("chunk"));
        if r["done"] == json!(true) {
            assert!(r["next_cursor"].is_null(), "{r}");
            return (out, n, first);
        }
        between(n);
        let cursor = r["next_cursor"].as_str().expect("next_cursor").to_string();
        r = call(
            srv,
            "vgraph_export",
            json!({ "graph": graph, "max_bytes": max, "cursor": cursor }),
        );
        n += 1;
    }
}

#[test]
fn big_graph_in_64k_parts_equals_one_shot_export_even_when_head_moves() {
    let (dir, srv) = server();
    big_graph(&srv, GRAPH);
    let whole = export_bytes(&srv, &dir, GRAPH, "whole.json");
    assert!(whole.len() > 3 * 1024 * 1024, "{}바이트", whole.len());

    // 조각 사이(3번째 조각 뒤)에 같은 브랜치에 새 커밋이 생긴다 — 같은 판을 끝까지.
    let mut moved = false;
    let (joined, n, first) = download(&srv, GRAPH, PART, |after| {
        if after == 3 {
            call(
                &srv,
                "vgraph_edit",
                json!({ "graph": GRAPH, "ops": [node("zz-new", "Item", json!({}))] }),
            );
            call(
                &srv,
                "vgraph_commit",
                json!({ "graph": GRAPH, "message": "mid" }),
            );
            moved = true;
        }
    });
    assert!(moved);
    assert!(joined == whole, "이어 붙인 문서가 첫 판과 다름");
    assert!(n >= whole.len() / PART, "조각 수 {n}");
    assert_eq!(first["done"], json!(false));
    assert_eq!(first["offset"], json!(0));
    assert_eq!(first["total_bytes"], json!(whole.len()));
    assert_eq!(first["nodes"], json!(BIG_NODES));
    assert!(first["chunk"].as_str().unwrap().len() <= PART);
    // 새 커밋은 다음 내보내기에는 보인다
    let now = export_bytes(&srv, &dir, GRAPH, "now.json");
    assert_ne!(now, whole);
    assert!(now.contains("zz-new"));
}

#[test]
fn small_document_is_one_part_with_done_and_no_cursor() {
    let (_d, srv) = server();
    small_graph(&srv, GRAPH);
    let r = call(
        &srv,
        "vgraph_export",
        json!({ "graph": GRAPH, "max_bytes": 65536 }),
    );
    assert_eq!(r["done"], json!(true));
    assert!(r["next_cursor"].is_null());
    assert_eq!(r["offset"], json!(0));
    assert_eq!(
        r["epoch"],
        json!("sha256:c9e44ac4bcb9024f6b61b6b5b058006253dae3a9268aef027e88ff948aca9494")
    );
    let chunk = r["chunk"].as_str().unwrap();
    assert_eq!(r["total_bytes"], json!(chunk.len()));
    assert!(r.get("document").is_none() && r.get("path").is_none());
    let doc: J = serde_json::from_str(chunk).unwrap();
    assert_eq!(doc["format"], json!("vgraph-json/1"));
}

fn multibyte_graph(srv: &McpServer, name: &str, count: usize) {
    call(srv, "vgraph_create", json!({ "name": name }));
    let ops: Vec<J> = (0..count)
        .map(|i| {
            node(
                &format!("m-{i}"),
                "Item",
                json!({ "t": format!("가{}🙂나다{i}", "한".repeat(i % 7)) }),
            )
        })
        .collect();
    call(srv, "vgraph_edit", json!({ "graph": name, "ops": ops }));
    call(
        srv,
        "vgraph_commit",
        json!({ "graph": name, "message": "mb" }),
    );
}

#[test]
fn multibyte_text_is_never_cut_inside_a_character() {
    let (dir, srv) = server();
    multibyte_graph(&srv, "acme/mb", 300);
    let whole = export_bytes(&srv, &dir, "acme/mb", "mb.json");
    // 조각이 작을수록 한글(3바이트)·이모지(4바이트) 경계에 자주 걸린다
    for max in [1024usize, 1025, 1026, 1027, 3001] {
        let (joined, n, _) = download(&srv, "acme/mb", max, |_| {});
        assert_eq!(joined, whole, "max_bytes={max}");
        assert!(n > 1);
    }
}

#[test]
fn export_parts_reject_bad_arguments_and_bad_cursors() {
    let (dir, srv) = server();
    small_graph(&srv, GRAPH);
    multibyte_graph(&srv, "acme/two", 80);
    let r = call(
        &srv,
        "vgraph_export",
        json!({ "graph": "acme/two", "max_bytes": 1024 }),
    );
    assert_eq!(r["done"], json!(false));
    let cursor = r["next_cursor"].as_str().unwrap().to_string();
    let epoch = r["epoch"].as_str().unwrap().to_string();
    let total = r["total_bytes"].as_u64().unwrap();
    let first_len = r["chunk"].as_str().unwrap().len();
    let with_cursor = |c: &str| json!({ "graph": "acme/two", "max_bytes": 1024, "cursor": c });
    let path = tmp_file(&dir, "x.json");

    // 인자 조합·범위
    let e = call_err(
        &srv,
        "vgraph_export",
        json!({ "graph": "acme/two", "max_bytes": 1024, "path": path }),
    );
    assert!(e.contains("path") && e.contains("max_bytes"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_export",
        json!({ "graph": "acme/two", "cursor": cursor, "path": path }),
    );
    assert!(e.contains("path"), "{e}");
    let e = call_err(
        &srv,
        "vgraph_export",
        json!({ "graph": "acme/two", "cursor": cursor }),
    );
    assert!(e.contains("max_bytes"), "cursor만 — {e}");
    for bad in [
        json!(0),
        json!(1023),
        json!(4 * 1024 * 1024 + 1),
        json!(-5),
        json!("64k"),
        json!(1.5),
    ] {
        let e = call_err(
            &srv,
            "vgraph_export",
            json!({ "graph": "acme/two", "max_bytes": bad }),
        );
        assert!(e.contains("max_bytes"), "{bad}: {e}");
    }

    // 커서 위조·오류 — 전부 거부
    let forged = [
        "garbage".to_string(),
        String::new(),
        "ex1|".to_string(),
        format!("ex1|{epoch}|1024"),
        format!("ex1|{epoch}|abc|{total}"),
        format!("ex1|{epoch}|{total}|{total}"),
        format!("ex1|{epoch}|{}|{total}", total + 5),
        format!("ex1|{epoch}|1024|{}", total + 1),
        format!("ex1|sha256:{}|1024|{total}", "0".repeat(64)),
        format!("ex1|not-a-hash|1024|{total}"),
        // 다른 도구(scan)의 커서 모양
        format!("{epoch}|01ARZ3NDEKTSV4RRFFQ69G5FAV"),
    ];
    for c in &forged {
        let e = call_err(&srv, "vgraph_export", with_cursor(c));
        assert!(e.contains("커서"), "{c}: {e}");
    }
    // 문자 중간을 가리키는 커서
    let text = call(
        &srv,
        "vgraph_export",
        json!({ "graph": "acme/two", "max_bytes": 4 * 1024 * 1024 }),
    )["chunk"]
        .as_str()
        .unwrap()
        .to_string();
    let mid = text
        .char_indices()
        .find(|(i, c)| *i > 1024 && c.len_utf8() > 1)
        .map(|(i, _)| i + 1)
        .unwrap();
    let e = call_err(
        &srv,
        "vgraph_export",
        with_cursor(&format!("ex1|{epoch}|{mid}|{total}")),
    );
    assert!(e.contains("커서"), "{e}");
    // 다른 그래프의 커밋을 가리키는 커서 · 주소가 다른 커밋을 못 박은 경우
    let other = call(
        &srv,
        "vgraph_export",
        json!({ "graph": GRAPH, "max_bytes": 1024 }),
    );
    let oe = other["epoch"].as_str().unwrap();
    call_err(
        &srv,
        "vgraph_export",
        with_cursor(&format!("ex1|{oe}|1024|{total}")),
    );
    let e = call_err(
        &srv,
        "vgraph_export",
        json!({ "graph": format!("acme/two.{oe}"), "max_bytes": 1024, "cursor": cursor }),
    );
    assert!(e.contains("epoch"), "{e}");
    // 올바른 커서는 그대로 통한다
    let next = call(&srv, "vgraph_export", with_cursor(&cursor));
    assert_eq!(next["offset"], json!(first_len));
}

// ================= 가져오기 나눠 올리기 =================

/// 문서를 `size` 바이트 안팎(문자 경계)으로 잘라 올린다. 확정은 부르지 않는다.
fn upload_parts(
    srv: &McpServer,
    id: Option<&Identity>,
    graph: &str,
    extra: J,
    doc: &str,
    size: usize,
) -> String {
    let mut begin = json!({ "graph": graph, "upload": "begin" });
    for (k, v) in extra.as_object().unwrap() {
        begin[k] = v.clone();
    }
    let (e, b) = raw_as(srv, id, "vgraph_import", begin);
    assert!(!e, "begin: {b}");
    let up = b["upload_id"].as_str().expect("upload_id").to_string();
    let mut seq = 0u64;
    let mut at = 0usize;
    while at < doc.len() {
        let mut end = (at + size).min(doc.len());
        while !doc.is_char_boundary(end) {
            end -= 1;
        }
        let (e, b) = raw_as(
            srv,
            id,
            "vgraph_import",
            json!({ "graph": graph, "upload": "append", "upload_id": up, "seq": seq,
                "chunk": &doc[at..end] }),
        );
        assert!(!e, "append {seq}: {b}");
        assert_eq!(b["next_seq"], json!(seq + 1));
        seq += 1;
        at = end;
    }
    up
}

fn commit_upload(srv: &McpServer, graph: &str, up: &str) -> J {
    call(
        srv,
        "vgraph_import",
        json!({ "graph": graph, "upload": "commit", "upload_id": up }),
    )
}

#[test]
fn uploading_in_parts_then_commit_equals_one_shot_import_replace() {
    let (dir, srv) = server();
    small_graph(&srv, GRAPH);
    let doc = export_bytes(&srv, &dir, GRAPH, "doc.json");
    call(&srv, "vgraph_create", json!({ "name": DST }));
    call(&srv, "vgraph_create", json!({ "name": "acme/once" }));
    let once = call(
        &srv,
        "vgraph_import",
        json!({ "graph": "acme/once", "document": doc }),
    );

    let up = upload_parts(&srv, None, DST, json!({ "message": "옮김" }), &doc, 37);
    assert_eq!(commit_count(&srv, DST), 0, "확정 전에는 커밋 0");
    let done = commit_upload(&srv, DST, &up);
    // 응답 형식은 한 번에 가져오기와 같다
    let keys = |v: &J| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&done), keys(&once));
    assert_eq!(done["outcome"], json!("committed"));
    assert_eq!(done["nodes"], once["nodes"]);
    assert_eq!(done["edges"], once["edges"]);
    assert_eq!(commit_count(&srv, DST), 1, "확정은 커밋 1");
    assert_eq!(export_bytes(&srv, &dir, DST, "dst.json"), doc, "결과 동일");
    let log = call(&srv, "vgraph_log", json!({ "graph": DST }));
    assert_eq!(log["commits"][0]["msg"], json!("옮김"));
    // 확정하면 업로드는 지워진다
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": DST, "upload": "commit", "upload_id": up }),
    );
    assert!(e.contains("업로드"), "{e}");
}

#[test]
fn uploading_in_parts_supports_merge_mode() {
    let (dir, srv) = server();
    small_graph(&srv, GRAPH);
    let doc = export_bytes(&srv, &dir, GRAPH, "doc.json");
    call(&srv, "vgraph_create", json!({ "name": "acme/m" }));
    let merge = json!({ "mode": "merge", "via_branch": "imp" });
    let up = upload_parts(&srv, None, "acme/m", merge.clone(), &doc, 50);
    let r = commit_upload(&srv, "acme/m", &up);
    assert_eq!(r["outcome"], json!("first_import"));
    assert_eq!(r["via_branch"], json!("imp"));
    // 같은 문서를 다시 조각으로 올리면 기존 규칙대로 up_to_date
    let up = upload_parts(&srv, None, "acme/m", merge, &doc, 80);
    let r = commit_upload(&srv, "acme/m", &up);
    assert_eq!(r["outcome"], json!("up_to_date"));
    assert_eq!(commit_count(&srv, "acme/m"), 1);
}

#[test]
fn big_document_uploaded_in_64k_chunks_equals_one_shot_import() {
    let (dir, srv) = server();
    big_graph(&srv, GRAPH);
    let doc = export_bytes(&srv, &dir, GRAPH, "big.json");
    call(&srv, "vgraph_create", json!({ "name": DST }));
    let up = upload_parts(&srv, None, DST, json!({}), &doc, PART);
    let r = commit_upload(&srv, DST, &up);
    assert_eq!(r["nodes"], json!(BIG_NODES));
    assert_eq!(commit_count(&srv, DST), 1);
    assert!(export_bytes(&srv, &dir, DST, "big2.json") == doc);
}

#[test]
fn unknown_field_document_is_rejected_at_commit_and_graph_is_unchanged() {
    let (_d, srv) = server();
    call(&srv, "vgraph_create", json!({ "name": DST }));
    let bad =
        r#"{"format":"vgraph-json/1","nodes":[{"slug":"x","label":["A"],"props":{}}],"edges":[]}"#;
    let up = upload_parts(&srv, None, DST, json!({}), bad, 10);
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": DST, "upload": "commit", "upload_id": up }),
    );
    assert!(
        e.contains("모르는 칸") && e.contains("nodes[0].label"),
        "{e}"
    );
    assert_eq!(commit_count(&srv, DST), 0);
    // 실패한 확정은 업로드를 남기지 않는다
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": DST, "upload": "append", "upload_id": up, "seq": 99, "chunk": "x" }),
    );
    assert!(e.contains("업로드"), "{e}");
    // 그래프는 여전히 비어 있어 replace가 된다
    let ok = r#"{"format":"vgraph-json/1","nodes":[],"edges":[]}"#;
    call(
        &srv,
        "vgraph_import",
        json!({ "graph": DST, "document": ok }),
    );
    // 깨진 JSON도 확정에서 거부
    let up = upload_parts(&srv, None, DST, json!({}), "{\"format\":", 4);
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": DST, "upload": "commit", "upload_id": up }),
    );
    assert!(e.contains("형식 오류"), "{e}");
}

#[test]
fn abort_leaves_no_trace() {
    let (dir, srv) = server();
    small_graph(&srv, GRAPH);
    let doc = export_bytes(&srv, &dir, GRAPH, "doc.json");
    call(&srv, "vgraph_create", json!({ "name": DST }));
    let up = upload_parts(&srv, None, DST, json!({}), &doc, 40);
    let r = call(
        &srv,
        "vgraph_import",
        json!({ "graph": DST, "upload": "abort", "upload_id": up }),
    );
    assert_eq!(r["aborted"], json!(true));
    for action in ["append", "commit", "abort"] {
        let e = call_err(
            &srv,
            "vgraph_import",
            json!({ "graph": DST, "upload": action, "upload_id": up, "seq": 0, "chunk": "x" }),
        );
        assert!(e.contains("업로드"), "{action}: {e}");
    }
    assert_eq!(commit_count(&srv, DST), 0);

    // 슬롯도 돌아온다: 상한 1개인 서버에서 abort 뒤 다시 begin 가능
    let (_d2, srv2) = server();
    let srv2 = srv2.with_upload_limits(UploadLimits {
        max_open: 1,
        ..UploadLimits::default()
    });
    call(&srv2, "vgraph_create", json!({ "name": DST }));
    let a = upload_parts(&srv2, None, DST, json!({}), "{", 1);
    let e = call_err(
        &srv2,
        "vgraph_import",
        json!({ "graph": DST, "upload": "begin" }),
    );
    assert!(e.contains("동시"), "{e}");
    call(
        &srv2,
        "vgraph_import",
        json!({ "graph": DST, "upload": "abort", "upload_id": a }),
    );
    upload_parts(&srv2, None, DST, json!({}), "{", 1);
}

#[test]
fn only_the_starting_identity_can_use_an_upload_and_failures_look_like_missing() {
    let (dir, srv) = server();
    small_graph(&srv, GRAPH);
    let doc = export_bytes(&srv, &dir, GRAPH, "doc.json");
    call(&srv, "vgraph_create", json!({ "name": DST }));
    let owner = Identity::new("owner", None).unwrap();
    let other_user = Identity::new("mallory", None).unwrap();
    let other_ws = Identity::new("owner", Some("ws2".into())).unwrap();
    let up = upload_parts(&srv, Some(&owner), DST, json!({}), &doc[..40], 20);
    let none_msg = call_err_as(
        &srv,
        &owner,
        "vgraph_import",
        json!({ "graph": DST, "upload": "abort", "upload_id": "up-없는-id" }),
    );
    for who in [&other_user, &other_ws] {
        for action in ["append", "commit", "abort"] {
            let msg = call_err_as(
                &srv,
                who,
                "vgraph_import",
                json!({ "graph": DST, "upload": action, "upload_id": up, "seq": 2, "chunk": "x" }),
            );
            assert_eq!(msg, none_msg, "{who:?} {action}: 존재 여부가 드러남");
        }
    }
    // 남의 시도는 업로드를 건드리지 않는다 — 주인은 이어 쓸 수 있다
    let r = call_as(
        &srv,
        &owner,
        "vgraph_import",
        json!({ "graph": DST, "upload": "append", "upload_id": up, "seq": 2, "chunk": &doc[40..] }),
    );
    assert_eq!(r["next_seq"], json!(3));
    call_as(
        &srv,
        &owner,
        "vgraph_import",
        json!({ "graph": DST, "upload": "commit", "upload_id": up }),
    );
    assert_eq!(commit_count(&srv, DST), 1);
    // 신원마다 따로 시작한 업로드의 id는 겹치지 않고 추측하기 어렵다
    let a = upload_parts(&srv, Some(&owner), DST, json!({}), "x", 1);
    let b = upload_parts(&srv, Some(&other_ws), DST, json!({}), "x", 1);
    assert_ne!(a, b);
    assert!(a.starts_with("up-") && a.len() > 40, "{a}");
}

#[test]
fn size_count_and_order_limits_reject_clearly() {
    let (_d, srv) = server();
    let srv = srv.with_upload_limits(UploadLimits {
        max_bytes: 100,
        max_open: 2,
        idle: Duration::from_secs(3600),
    });
    call(&srv, "vgraph_create", json!({ "name": DST }));
    let begin = |s: &McpServer| {
        raw_as(
            s,
            None,
            "vgraph_import",
            json!({ "graph": DST, "upload": "begin" }),
        )
    };
    let id1 = begin(&srv).1["upload_id"].as_str().unwrap().to_string();
    let r = begin(&srv).1;
    assert_eq!(r["max_bytes"], json!(100));
    assert_eq!(r["next_seq"], json!(0));
    let (e, msg) = begin(&srv);
    assert!(e && msg.as_str().unwrap().contains("동시"), "{msg}");
    // 크기 상한: 초과 조각은 거부하고 업로드는 그대로
    let append = |seq: u64, chunk: String| json!({ "graph": DST, "upload": "append", "upload_id": id1, "seq": seq, "chunk": chunk });
    let e = call_err(&srv, "vgraph_import", append(0, "x".repeat(101)));
    assert!(e.contains("100"), "{e}");
    let r = call(&srv, "vgraph_import", append(0, "x".repeat(100)));
    assert_eq!(r["received_bytes"], json!(100));
    let e = call_err(&srv, "vgraph_import", append(1, "y".into()));
    assert!(e.contains("100"), "{e}");
    // 순서: 틀린 seq는 거부하고 기대 값을 알려 준다
    let e = call_err(&srv, "vgraph_import", append(5, String::new()));
    assert!(e.contains("seq") && e.contains('1'), "{e}");
    let e = call_err(&srv, "vgraph_import", append(0, String::new()));
    assert!(e.contains("seq"), "같은 seq 재전송은 거부 — {e}");
}

#[test]
fn idle_uploads_expire_and_free_their_slot() {
    let (_d, srv) = server();
    // 유휴 0초: 만든 뒤 다음 접근에는 이미 만료 — 시간을 흘려보내지 않고 결정적으로 확인
    let srv = srv.with_upload_limits(UploadLimits {
        max_open: 1,
        idle: Duration::ZERO,
        ..UploadLimits::default()
    });
    call(&srv, "vgraph_create", json!({ "name": DST }));
    let begin = json!({ "graph": DST, "upload": "begin" });
    let r = call(&srv, "vgraph_import", begin.clone());
    let up = r["upload_id"].as_str().unwrap().to_string();
    let e = call_err(
        &srv,
        "vgraph_import",
        json!({ "graph": DST, "upload": "append", "upload_id": up, "seq": 0, "chunk": "x" }),
    );
    assert!(e.contains("업로드"), "{e}");
    // 만료된 것은 폐기되어 슬롯이 비므로 다시 열 수 있다(상한 1)
    call(&srv, "vgraph_import", begin.clone());
    call(&srv, "vgraph_import", begin);
    assert_eq!(commit_count(&srv, DST), 0);
}

#[test]
fn contradictory_or_malformed_upload_arguments_are_rejected() {
    let (dir, srv) = server();
    small_graph(&srv, GRAPH);
    let doc = export_bytes(&srv, &dir, GRAPH, "doc.json");
    call(&srv, "vgraph_create", json!({ "name": DST }));
    let up = call(
        &srv,
        "vgraph_import",
        json!({ "graph": DST, "upload": "begin" }),
    )["upload_id"]
        .as_str()
        .unwrap()
        .to_string();
    let imp = |a: J| call_err(&srv, "vgraph_import", a);
    let file = tmp_file(&dir, "doc.json");
    // 모순 조합
    assert!(imp(json!({ "graph": DST, "upload": "begin", "document": {} })).contains("document"));
    assert!(imp(json!({ "graph": DST, "upload": "begin", "path": file })).contains("path"));
    assert!(imp(
        json!({ "graph": DST, "upload": "append", "upload_id": up, "seq": 0,
        "chunk": "x", "document": {} })
    )
    .contains("document"));
    assert!(
        imp(json!({ "graph": DST, "upload": "commit", "upload_id": up, "path": file }))
            .contains("path")
    );
    assert!(imp(json!({ "graph": DST, "upload": "begin", "chunk": "x" })).contains("chunk"));
    assert!(imp(json!({ "graph": DST, "upload": "begin", "upload_id": up })).contains("upload_id"));
    assert!(imp(
        json!({ "graph": DST, "upload": "append", "upload_id": up, "seq": 0,
        "chunk": "x", "mode": "merge" })
    )
    .contains("begin"));
    assert!(imp(
        json!({ "graph": "acme/other", "upload": "append", "upload_id": up,
        "seq": 0, "chunk": "x" })
    )
    .contains("begin"));
    // begin도 기존 규칙으로 검증: merge는 via_branch 필요, replace에 via_branch 금지
    assert_eq!(
        imp(json!({ "graph": DST, "upload": "begin", "mode": "merge" })),
        BASE_ERR_MERGE_NO_VIA
    );
    assert!(
        imp(json!({ "graph": DST, "upload": "begin", "via_branch": "v" })).contains("via_branch")
    );
    // upload 없이 업로드 인자만
    assert!(imp(json!({ "graph": DST, "upload_id": up, "document": {} })).contains("upload"));
    assert!(imp(json!({ "graph": DST, "chunk": "x", "document": {} })).contains("upload"));
    assert!(imp(json!({ "graph": DST, "seq": 1, "document": {} })).contains("upload"));
    // 잘못된 값·빠진 인자
    assert!(imp(json!({ "graph": DST, "upload": "start" })).contains("upload"));
    assert!(imp(json!({ "graph": DST, "upload": 5 })).contains("upload"));
    assert!(
        imp(json!({ "graph": DST, "upload": "append", "seq": 0, "chunk": "x" }))
            .contains("upload_id")
    );
    assert!(
        imp(json!({ "graph": DST, "upload": "append", "upload_id": up, "chunk": "x" }))
            .contains("seq")
    );
    assert!(imp(
        json!({ "graph": DST, "upload": "append", "upload_id": up, "seq": -1,
        "chunk": "x" })
    )
    .contains("seq"));
    assert!(
        imp(json!({ "graph": DST, "upload": "append", "upload_id": up, "seq": 0 }))
            .contains("chunk")
    );
    assert!(imp(
        json!({ "graph": DST, "upload": "append", "upload_id": up, "seq": 0,
        "chunk": 7 })
    )
    .contains("chunk"));
    assert!(imp(json!({ "graph": DST, "upload": "commit" })).contains("upload_id"));
    // 이 모든 실패 뒤에도 업로드는 살아 있고 문서를 올려 확정할 수 있다
    call(
        &srv,
        "vgraph_import",
        json!({ "graph": DST, "upload": "append", "upload_id": up, "seq": 0, "chunk": &doc }),
    );
    let r = commit_upload(&srv, DST, &up);
    assert_eq!(r["outcome"], json!("committed"));
}

#[test]
fn tool_descriptions_document_parts_and_tool_count_is_unchanged() {
    let defs = vgraph_mcp::tool_defs();
    assert_eq!(defs.len(), 20);
    let def = |n: &str| defs.iter().find(|t| t["name"] == n).unwrap().clone();
    let ex = def("vgraph_export");
    let im = def("vgraph_import");
    for k in ["max_bytes", "cursor"] {
        assert!(
            ex["inputSchema"]["properties"].get(k).is_some(),
            "export.{k}"
        );
        assert!(ex["description"].as_str().unwrap().contains(k));
    }
    for k in ["upload", "upload_id", "chunk", "seq"] {
        assert!(
            im["inputSchema"]["properties"].get(k).is_some(),
            "import.{k}"
        );
        assert!(im["description"].as_str().unwrap().contains(k));
    }
    let (ed, id) = (
        ex["description"].as_str().unwrap(),
        im["description"].as_str().unwrap(),
    );
    assert!(ed.contains("next_cursor") && id.contains("begin") && id.contains("commit"));
    // 권장 조각 크기 안내(상주 서버 본문 한도 16MiB 근거)
    assert!(ed.contains("권장 max_bytes: 64KB~1MB"));
    assert!(id.contains("권장 chunk 크기: 64KB~1MB, 8MiB 이하") && id.contains("413"));
    for d in [ed, id] {
        assert!(!d.to_lowercase().contains("grove"));
    }
    assert_eq!(ex["inputSchema"]["required"], json!(["graph"]));
    assert_eq!(im["inputSchema"]["required"], json!(["graph"]));
}

/// 도구 설명문의 취소 예시가 그대로 통하고, via_branch 설명이 실제 브랜치 규칙(`/` 허용)과 맞는지 고정한다.
#[test]
fn import_description_abort_example_works_as_written_and_via_branch_mentions_slash() {
    let defs = vgraph_mcp::tool_defs();
    let im = defs.iter().find(|t| t["name"] == "vgraph_import").unwrap();
    let desc = im["description"].as_str().unwrap();
    let example = r#"{"graph":"acme/catalog","upload":"abort","upload_id":"…"}"#;
    assert!(desc.contains(example), "설명문에 취소 예시가 없음: {desc}");

    let via = im["inputSchema"]["properties"]["via_branch"]["description"]
        .as_str()
        .unwrap();
    assert!(via.contains("영문·숫자·-·_·/"), "via_branch 설명: {via}");

    // 예시 모양 그대로(upload_id만 실제 값으로) begin 뒤 호출하면 성공한다.
    let (_dir, srv) = server();
    let (e, b) = raw_as(
        &srv,
        None,
        "vgraph_import",
        json!({ "graph": "acme/catalog", "upload": "begin" }),
    );
    assert!(!e, "begin: {b}");
    let up = b["upload_id"].as_str().expect("upload_id");
    let mut args: J = serde_json::from_str(example).unwrap();
    args["upload_id"] = json!(up);
    let (e, b) = raw_as(&srv, None, "vgraph_import", args);
    assert!(!e, "abort: {b}");
}
