//! JSON 가져오기·내보내기(R5, vgraph-json/1) 골든 — 엔진 공용 모듈 `interchange`.
//!
//! 모킹 경계: 저장소·시계·ID 발급기만 시험용(TestIdGen/TestClock + 임시 디렉터리 SQLite).
//! 내보내기·검증·적용·커밋 로직은 실제 구현을 그대로 탄다.

mod util;

use std::collections::BTreeMap;
use std::time::Instant;
use util::{engine_at, new_env};
use vgraph_core::value::props;
use vgraph_core::*;
use vgraph_engine::interchange::{self, InterchangeError, ProblemCode, FORMAT};
use vgraph_engine::{
    Choice, CommitOutcome, MergeOpts, MergeOutcome, NodeClassDef, Resolution, Schema, SchemaMode,
    Session,
};

const SRC: &str = "acme/src";
const DST: &str = "acme/dst";

fn committed(o: CommitOutcome) -> Hash256 {
    match o {
        CommitOutcome::Committed { commit, .. } => commit,
        other => panic!("커밋 기대, 실제: {other:?}"),
    }
}

/// 값 종류를 두루 담은 시험 그래프(커밋 1개 + 스키마 커밋 1개).
fn build_source(sess: &Session, name: &str) -> GraphId {
    let (ns, slug) = name.split_once('/').unwrap();
    let g = sess.create_graph(ns, slug).unwrap();
    let es = sess.edit(g, "main").unwrap();
    let a = es
        .add_node(
            &["Rule", "Doc"],
            Some("rule-b"),
            props([
                ("name", "규칙 B".into()),
                ("n", 7.into()),
                ("f", 1.0.into()),
                ("flag", true.into()),
                ("none", Value::Null),
                ("dec", Value::Decimal("12.50".into())),
                ("ts", Value::Ts("2026-10-03T00:00:00Z".into())),
                ("raw", Value::Bytes(vec![0, 255, 16])),
                (
                    "list",
                    Value::List(vec![1.into(), "x".into(), Value::Float(2.5)]),
                ),
            ]),
        )
        .unwrap();
    let b = es
        .add_node(
            &["Item"],
            Some("item-a"),
            props([("name", "항목 A".into())]),
        )
        .unwrap();
    let c = es.add_node(&["Item"], Some("item-c"), props([])).unwrap();
    es.add_edge("참조", a, b, props([])).unwrap();
    // 같은 (etype, from, to)가 속성만 다르게 둘 — 중복 엣지 보존 + 정렬 보조 키
    es.add_edge("관련", b, c, props([("w", 2.into())])).unwrap();
    es.add_edge("관련", b, c, props([("w", 1.into())])).unwrap();
    es.add_edge("관련", c, c, props([])).unwrap();
    committed(sess.commit(&es, "src").unwrap());
    let mut node_classes = BTreeMap::new();
    node_classes.insert(
        "Rule".into(),
        NodeClassDef {
            required: vec!["name".into()],
        },
    );
    let schema = Schema {
        mode: SchemaMode::Strict,
        node_classes,
        edge_classes: BTreeMap::new(),
    };
    committed(sess.set_schema(g, "main", &schema).unwrap());
    g
}

fn export_text(sess: &Session, addr: &str) -> String {
    interchange::export(sess, addr).unwrap().text
}

fn problem_codes(e: &InterchangeError) -> Vec<(ProblemCode, String)> {
    match e {
        InterchangeError::Rejected(ps) => ps.iter().map(|p| (p.code, p.subject.clone())).collect(),
        other => panic!("Rejected 기대, 실제: {other:?}"),
    }
}

fn head_is_none(sess: &Session, addr: &str) -> bool {
    sess.resolve(&format!("graph://{addr}")).is_err()
}

// ---------- 형식·결정성 ----------

#[test]
fn export_is_sorted_and_labeled_with_format() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    build_source(&sess, SRC);
    let text = export_text(&sess, SRC);
    let j: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(j["format"], FORMAT);
    let slugs: Vec<&str> = j["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["slug"].as_str().unwrap())
        .collect();
    assert_eq!(
        slugs,
        ["__schema", "item-a", "item-c", "rule-b"],
        "slug 순, __schema 포함"
    );
    let rule = &j["nodes"][3];
    assert_eq!(
        rule["labels"],
        serde_json::json!(["Doc", "Rule"]),
        "라벨 정렬"
    );
    let edges = j["edges"].as_array().unwrap();
    let keys: Vec<(&str, &str, &str)> = edges
        .iter()
        .map(|e| {
            (
                e["etype"].as_str().unwrap(),
                e["from"].as_str().unwrap(),
                e["to"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        [
            ("관련", "item-a", "item-c"),
            ("관련", "item-a", "item-c"),
            ("관련", "item-c", "item-c"),
            ("참조", "rule-b", "item-a"),
        ]
    );
    // 중복 엣지의 순서는 props canonical 표기 순 (w=1 이 w=2 보다 앞)
    assert_eq!(edges[0]["props"]["w"], 1);
    assert_eq!(edges[1]["props"]["w"], 2);
    // 같은 그래프를 다시 내보내면 같은 바이트
    assert_eq!(text, export_text(&sess, SRC));
}

#[test]
fn export_by_branch_and_commit_address() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = build_source(&sess, SRC);
    let head = sess.resolve(&format!("graph://{SRC}")).unwrap().1;
    let first = sess.log(g, "main").unwrap().into_iter().last().unwrap();
    let at_first = interchange::export(&sess, &format!("{SRC}.{first}")).unwrap();
    assert_eq!(at_first.nodes, 3, "첫 커밋에는 __schema 가 없다");
    let at_head = interchange::export(&sess, &format!("{SRC}@main.{head}")).unwrap();
    assert_eq!(at_head.nodes, 4);
    assert_eq!(at_head.edges, 4);
    assert_eq!(at_head.epoch, head);
}

#[test]
fn export_unknown_graph_is_error() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let e = interchange::export(&sess, "acme/none").unwrap_err();
    assert!(matches!(e, InterchangeError::Engine(_)), "{e:?}");
}

// ---------- 왕복 ----------

#[test]
fn roundtrip_fixture_is_byte_identical_and_one_commit() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    build_source(&sess, SRC);
    let dg = sess.create_graph("acme", "dst").unwrap();
    let before = export_text(&sess, SRC);

    let doc = interchange::parse_document(&before).unwrap();
    let r = interchange::import_replace(&sess, DST, doc, "import").unwrap();
    assert!(r.commit.is_some());
    assert_eq!((r.nodes, r.edges), (4, 4));
    assert_eq!(
        sess.log(dg, "main").unwrap().len(),
        1,
        "가져오기 1회 = 커밋 1개"
    );

    assert_eq!(
        before,
        export_text(&sess, DST),
        "왕복 바이트 동일(__schema 포함)"
    );
    // __schema 는 일반 노드로 들어가 스키마로 동작한다
    let c = sess.resolve(&format!("graph://{DST}")).unwrap().1;
    assert!(sess.schema(dg, c).unwrap().is_some());
}

#[test]
fn roundtrip_engine_f0_fixture_is_byte_identical() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    util::build_f0_engine(&sess);
    sess.create_graph("acme", "dst").unwrap();
    let before = export_text(&sess, "acme/bookclub");
    let doc = interchange::parse_document(&before).unwrap();
    interchange::import_replace(&sess, DST, doc, "f0").unwrap();
    assert_eq!(before, export_text(&sess, DST));
}

#[test]
fn roundtrip_2500_nodes_is_byte_identical() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "src").unwrap();
    let es = sess.edit(g, "main").unwrap();
    let mut ids = Vec::new();
    for i in 0..2500 {
        let label = if i % 2 == 0 { "Item" } else { "Rule" };
        ids.push(
            es.add_node(
                &[label],
                Some(&format!("n-{i:04}")),
                props([
                    ("name", format!("노드 {i}").into()),
                    ("i", (i as i64).into()),
                ]),
            )
            .unwrap(),
        );
    }
    for i in 1..2500 {
        es.add_edge("참조", ids[i - 1], ids[i], props([])).unwrap();
    }
    committed(sess.commit(&es, "big").unwrap());
    sess.create_graph("acme", "dst").unwrap();

    let t0 = Instant::now();
    let before = export_text(&sess, SRC);
    let t_export = t0.elapsed();
    let doc = interchange::parse_document(&before).unwrap();
    let t1 = Instant::now();
    interchange::import_replace(&sess, DST, doc, "big import").unwrap();
    let t_import = t1.elapsed();
    eprintln!("2500노드+2499엣지: 내보내기 {t_export:?}, 가져오기 {t_import:?}");
    assert_eq!(before, export_text(&sess, DST));
}

// ---------- 가져오기 검증: 저장소를 건드리기 전에 거부 ----------

fn doc_text(nodes: &str, edges: &str) -> String {
    format!(r#"{{"format":"{FORMAT}","nodes":[{nodes}],"edges":[{edges}]}}"#)
}

fn assert_untouched(sess: &Session, g: GraphId, addr: &str) {
    assert!(head_is_none(sess, addr), "head 불변(커밋 0)");
    assert!(
        sess.list_working(g).unwrap().is_empty(),
        "working copy 흔적 없음"
    );
}

#[test]
fn import_rejects_dangling_edges_listing_missing_slugs() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    let text = doc_text(
        r#"{"slug":"a","labels":[],"props":{}}"#,
        r#"{"etype":"t","from":"a","to":"ghost-1","props":{}},{"etype":"t","from":"ghost-2","to":"a","props":{}},{"etype":"t","from":"a","to":"ghost-1","props":{}}"#,
    );
    let doc = interchange::parse_document(&text);
    // 검증은 파싱 단계(저장소 접근 전)에서 끝난다
    let e = doc.unwrap_err();
    let codes = problem_codes(&e);
    assert_eq!(
        codes,
        vec![
            (ProblemCode::MissingSlug, "ghost-1".to_string()),
            (ProblemCode::MissingSlug, "ghost-2".to_string()),
        ],
        "중복 없이 정렬된 빠진 slug 목록"
    );
    assert_untouched(&sess, g, DST);
}

#[test]
fn import_rejects_duplicate_slugs_and_lockfile_node_together() {
    let text = doc_text(
        r#"{"slug":"a","labels":[],"props":{}},{"slug":"a","labels":["X"],"props":{}},{"slug":"__lockfile","labels":["__system"],"props":{}}"#,
        "",
    );
    let e = interchange::parse_document(&text).unwrap_err();
    assert_eq!(
        problem_codes(&e),
        vec![
            (ProblemCode::DuplicateSlug, "a".to_string()),
            (ProblemCode::LockfileNode, "__lockfile".to_string()),
        ]
    );
}

#[test]
fn parse_rejects_malformed_documents() {
    let cases: Vec<(String, &str)> = vec![
        ("not json".into(), "JSON"),
        ("[]".into(), "객체"),
        (r#"{"nodes":[],"edges":[]}"#.into(), "format"),
        (
            r#"{"format":"vgraph-json/9","nodes":[],"edges":[]}"#.into(),
            "vgraph-json/9",
        ),
        (format!(r#"{{"format":"{FORMAT}","edges":[]}}"#), "nodes"),
        (format!(r#"{{"format":"{FORMAT}","nodes":[]}}"#), "edges"),
        (doc_text(r#"{"labels":[],"props":{}}"#, ""), "slug"),
        (
            doc_text(r#"{"slug":"","labels":[],"props":{}}"#, ""),
            "slug",
        ),
        (
            doc_text(r#"{"slug":"a","labels":"x","props":{}}"#, ""),
            "labels",
        ),
        (
            doc_text(r#"{"slug":"a","labels":[],"props":[1]}"#, ""),
            "props",
        ),
        (
            doc_text(r#"{"slug":"a","labels":[],"props":{"x":1e999}}"#, ""),
            "JSON",
        ),
        (
            doc_text(
                r#"{"slug":"a","labels":[],"props":{}}"#,
                r#"{"etype":"t","to":"a","props":{}}"#,
            ),
            "from",
        ),
    ];
    for (text, needle) in cases {
        match interchange::parse_document(&text) {
            Err(InterchangeError::Format(m)) => {
                assert!(
                    m.contains(needle),
                    "{needle} 가 오류문에 없음: {m} (입력: {text})"
                )
            }
            other => panic!("Format 오류 기대 ({text}), 실제: {other:?}"),
        }
    }
}

// ---------- 가져오기 거부: 대상 상태 ----------

#[test]
fn replace_rejects_non_empty_graph() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    build_source(&sess, SRC);
    let doc = interchange::parse_document(&doc_text(r#"{"slug":"z","labels":[],"props":{}}"#, ""))
        .unwrap();
    let e = interchange::import_replace(&sess, SRC, doc, "x").unwrap_err();
    assert!(
        matches!(e, InterchangeError::NotEmpty { nodes: 4, edges: 4 }),
        "{e:?}"
    );
}

#[test]
fn replace_rejects_pending_edits_and_keeps_them() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    let es = sess.edit(g, "main").unwrap();
    es.add_node(&["Item"], Some("mine"), props([])).unwrap();
    let doc = interchange::parse_document(&doc_text(r#"{"slug":"z","labels":[],"props":{}}"#, ""))
        .unwrap();
    let e = interchange::import_replace(&sess, DST, doc, "x").unwrap_err();
    assert!(matches!(e, InterchangeError::PendingEdits), "{e:?}");
    assert!(es.dirty(), "사용자의 미커밋 편집은 그대로");
    assert!(head_is_none(&sess, DST));
}

#[test]
fn replace_unknown_graph_and_commit_address_are_errors() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    sess.create_graph("acme", "dst").unwrap();
    let mk = || interchange::parse_document(&doc_text("", "")).unwrap();
    assert!(matches!(
        interchange::import_replace(&sess, "acme/none", mk(), "x").unwrap_err(),
        InterchangeError::BadAddr(_)
    ));
    let h = Hash256([7; 32]);
    assert!(matches!(
        interchange::import_replace(&sess, &format!("{DST}.{h}"), mk(), "x").unwrap_err(),
        InterchangeError::BadAddr(_)
    ));
}

#[test]
fn empty_document_commits_nothing() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    let doc = interchange::parse_document(&doc_text("", "")).unwrap();
    let r = interchange::import_replace(&sess, DST, doc, "x").unwrap();
    assert_eq!((r.commit, r.nodes, r.edges), (None, 0, 0));
    assert_untouched(&sess, g, DST);
}

// ---------- commit 단계 실패는 작업분을 버린다 ----------

#[test]
fn schema_violation_discards_work_and_leaves_head() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    // __schema 가 들어 있는 문서: Rule 은 name 필수인데 name 이 없는 Rule 노드가 함께 있다
    let schema_props = {
        let mut nc = BTreeMap::new();
        nc.insert(
            "Rule".into(),
            NodeClassDef {
                required: vec!["name".into()],
            },
        );
        Schema {
            mode: SchemaMode::Strict,
            node_classes: nc,
            edge_classes: BTreeMap::new(),
        }
        .to_props()
    };
    let mut s = String::new();
    vgraph_core::canon::write_value(&Value::Map(schema_props), &mut s);
    let text = doc_text(
        &format!(
            r#"{{"slug":"__schema","labels":["__system"],"props":{s}}},{{"slug":"r","labels":["Rule"],"props":{{}}}}"#
        ),
        "",
    );
    let doc = interchange::parse_document(&text).unwrap();
    let e = interchange::import_replace(&sess, DST, doc, "x").unwrap_err();
    assert!(
        matches!(e, InterchangeError::SchemaViolation(ref v) if !v.is_empty()),
        "{e:?}"
    );
    assert_untouched(&sess, g, DST);
}

#[test]
fn unregistered_link_target_discards_work() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    let text = doc_text(
        r#"{"slug":"a","labels":[],"props":{"link":{"$ref":"graph://nowhere/none#x"}}}"#,
        "",
    );
    let doc = interchange::parse_document(&text).unwrap();
    let e = interchange::import_replace(&sess, DST, doc, "x").unwrap_err();
    assert!(matches!(e, InterchangeError::Engine(_)), "{e:?}");
    assert_untouched(&sess, g, DST);
    // 같은 그래프에 이어서 정상 가져오기가 된다 (흔적이 남지 않았다는 증거)
    let ok = interchange::parse_document(&doc_text(r#"{"slug":"a","labels":[],"props":{}}"#, ""))
        .unwrap();
    assert!(interchange::import_replace(&sess, DST, ok, "x")
        .unwrap()
        .commit
        .is_some());
}

// ---------- 내보내기 거부 ----------

#[test]
fn export_rejects_slugless_nodes_listing_them() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "src").unwrap();
    let es = sess.edit(g, "main").unwrap();
    let u1 = es.add_node(&["Item", "A"], None, props([])).unwrap();
    es.add_node(&["Item"], Some("ok"), props([])).unwrap();
    committed(sess.commit(&es, "x").unwrap());
    let e = interchange::export(&sess, SRC).unwrap_err();
    let codes = problem_codes(&e);
    assert_eq!(codes.len(), 1);
    assert_eq!(codes[0].0, ProblemCode::SluglessNode);
    assert!(
        codes[0].1.contains(&u1.to_string()) && codes[0].1.contains("A,Item"),
        "{codes:?}"
    );
}

#[test]
fn export_omits_lockfile_but_rejects_edges_touching_it() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "src").unwrap();
    let es = sess.edit(g, "main").unwrap();
    es.add_node(&["__system"], Some("__lockfile"), props([]))
        .unwrap();
    let a = es.add_node(&["Item"], Some("a"), props([])).unwrap();
    committed(sess.commit(&es, "x").unwrap());
    let ex = interchange::export(&sess, SRC).unwrap();
    assert_eq!(ex.nodes, 1, "lockfile 은 내보내지 않는다");
    assert!(!ex.text.contains("__lockfile"));

    let es = sess.edit(g, "main").unwrap();
    let lf = es.by_slug("__lockfile").unwrap();
    es.add_edge("t", a, lf, props([])).unwrap();
    committed(sess.commit(&es, "y").unwrap());
    let e = interchange::export(&sess, SRC).unwrap_err();
    let codes = problem_codes(&e);
    assert_eq!(codes.len(), 1);
    assert_eq!(codes[0].0, ProblemCode::LockfileEdge);
}

// ---------- 같은 root 의 다른 엔진 인스턴스 (파일 경유) ----------

#[test]
fn export_file_roundtrip_across_engines() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    build_source(&sess, SRC);
    let ex = interchange::export(&sess, SRC).unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("out.json");
    ex.write_to(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), ex.text);

    let other = tempfile::TempDir::new().unwrap();
    let engine2 = engine_at(other.path(), true, 1);
    let s2 = engine2.session("u");
    s2.create_graph("acme", "dst").unwrap();
    let doc = interchange::read_document(&path).unwrap();
    interchange::import_replace(&s2, DST, doc, "from file").unwrap();
    assert_eq!(ex.text, export_text(&s2, DST));

    assert!(matches!(
        interchange::read_document(&dir.path().join("none.json")).unwrap_err(),
        InterchangeError::Io(_)
    ));
}

// ================= Step 4b: mode=merge — 가져오기 전용 브랜치 + 기존 3-way 머지 =================

const VIA: &str = "sync";

fn jdoc(nodes: serde_json::Value, edges: serde_json::Value) -> interchange::Document {
    interchange::parse_document(
        &serde_json::json!({ "format": FORMAT, "nodes": nodes, "edges": edges }).to_string(),
    )
    .unwrap()
}

fn jn(slug: &str, labels: &[&str], props: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "slug": slug, "labels": labels, "props": props })
}

fn je(etype: &str, from: &str, to: &str, props: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "etype": etype, "from": from, "to": to, "props": props })
}

/// 기준 문서: a(name, extra) b c, 엣지 a→b, b→c.
fn base_doc() -> interchange::Document {
    jdoc(
        serde_json::json!([
            jn(
                "a",
                &["Item"],
                serde_json::json!({ "name": "A", "extra": 1 })
            ),
            jn("b", &["Item"], serde_json::json!({ "name": "B" })),
            jn("c", &["Item"], serde_json::json!({ "name": "C" })),
        ]),
        serde_json::json!([
            je("t", "a", "b", serde_json::json!({})),
            je("t", "b", "c", serde_json::json!({})),
        ]),
    )
}

fn sync(
    sess: &Session,
    addr: &str,
    doc: interchange::Document,
) -> Result<interchange::MergeImportReport, InterchangeError> {
    interchange::import_merge(sess, addr, VIA, doc, "sync")
}

fn is_merge(r: &interchange::MergeImportReport, want: &str) -> bool {
    use interchange::SyncOutcome as S;
    use vgraph_engine::MergeOutcome as M;
    matches!(
        (&r.outcome, want),
        (S::FirstImport { .. }, "first")
            | (S::Merge(M::FastForward(_)), "ff")
            | (S::Merge(M::Merged(_)), "merged")
            | (S::Merge(M::UpToDate(_)), "up_to_date")
            | (S::Merge(M::Conflicts(_)), "conflicts")
    )
}

fn head_of(sess: &Session, addr: &str) -> Hash256 {
    sess.resolve(&format!("graph://{addr}")).unwrap().1
}

/// 문서 하나를 새 그래프에 replace 로 넣고 내보낸 텍스트 — "문서와 똑같은 그래프"의 기대값.
fn expected_export(sess: &Session, name: &str, doc: interchange::Document) -> String {
    let (ns, slug) = name.split_once('/').unwrap();
    sess.create_graph(ns, slug).unwrap();
    interchange::import_replace(sess, name, doc, "expected").unwrap();
    export_text(sess, name)
}

fn edit_main(sess: &Session, g: GraphId, f: impl FnOnce(&vgraph_engine::EditSession)) {
    let es = sess.edit(g, "main").unwrap();
    f(&es);
    committed(sess.commit(&es, "direct edit").unwrap());
}

fn conflict_items(r: &interchange::MergeImportReport) -> &[vgraph_engine::ConflictItem] {
    match &r.outcome {
        interchange::SyncOutcome::Merge(MergeOutcome::Conflicts(c)) => &c.items,
        other => panic!("충돌 기대, 실제: {other:?}"),
    }
}

/// [핵심] 첫 가져오기: 빈 대상에 replace 로 넣고 같은 커밋에서 가져오기 전용 브랜치가 생긴다.
#[test]
fn merge_first_import_creates_via_branch_at_same_commit() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    let r = sync(&sess, DST, base_doc()).unwrap();
    assert!(is_merge(&r, "first"), "{:?}", r.outcome);
    assert_eq!((r.nodes, r.edges), (3, 2));
    assert_eq!(
        head_of(&sess, "acme/dst@sync"),
        head_of(&sess, DST),
        "같은 커밋"
    );
    assert_eq!(sess.log(g, "main").unwrap().len(), 1);
}

/// [핵심] 첫 가져오기인데 대상이 비어 있지 않으면 거부 — 기준점이 없어 충돌을 가릴 수 없다.
#[test]
fn merge_first_import_rejects_non_empty_target() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    build_source(&sess, "acme/dst");
    let before = head_of(&sess, DST);
    let e = sync(&sess, DST, base_doc()).unwrap_err();
    assert!(
        matches!(e, InterchangeError::NoBaseline(ref m) if m.contains("기준점")),
        "{e:?}"
    );
    assert_eq!(head_of(&sess, DST), before);
    assert!(
        head_is_none(&sess, "acme/dst@sync"),
        "via 브랜치도 만들지 않는다"
    );
}

/// [핵심] 문서만 바뀐 경우 7종 → 충돌 없이 대상에 반영(빨리 감기), 결과는 문서와 같은 그래프.
#[test]
fn merge_document_only_changes_are_applied_without_conflict() {
    let cases: Vec<(&str, interchange::Document)> = vec![
        (
            "속성 변경",
            jdoc(
                serde_json::json!([
                    jn(
                        "a",
                        &["Item"],
                        serde_json::json!({ "name": "A2", "extra": 1 })
                    ),
                    jn("b", &["Item"], serde_json::json!({ "name": "B" })),
                    jn("c", &["Item"], serde_json::json!({ "name": "C" })),
                ]),
                serde_json::json!([
                    je("t", "a", "b", serde_json::json!({})),
                    je("t", "b", "c", serde_json::json!({}))
                ]),
            ),
        ),
        (
            "속성 삭제",
            jdoc(
                serde_json::json!([
                    jn("a", &["Item"], serde_json::json!({ "name": "A" })),
                    jn("b", &["Item"], serde_json::json!({ "name": "B" })),
                    jn("c", &["Item"], serde_json::json!({ "name": "C" })),
                ]),
                serde_json::json!([
                    je("t", "a", "b", serde_json::json!({})),
                    je("t", "b", "c", serde_json::json!({}))
                ]),
            ),
        ),
        (
            "라벨 변경",
            jdoc(
                serde_json::json!([
                    jn(
                        "a",
                        &["Rule"],
                        serde_json::json!({ "name": "A", "extra": 1 })
                    ),
                    jn("b", &["Item"], serde_json::json!({ "name": "B" })),
                    jn("c", &["Item"], serde_json::json!({ "name": "C" })),
                ]),
                serde_json::json!([
                    je("t", "a", "b", serde_json::json!({})),
                    je("t", "b", "c", serde_json::json!({}))
                ]),
            ),
        ),
        (
            "노드 추가",
            jdoc(
                serde_json::json!([
                    jn(
                        "a",
                        &["Item"],
                        serde_json::json!({ "name": "A", "extra": 1 })
                    ),
                    jn("b", &["Item"], serde_json::json!({ "name": "B" })),
                    jn("c", &["Item"], serde_json::json!({ "name": "C" })),
                    jn("d", &["Item"], serde_json::json!({ "name": "D" })),
                ]),
                serde_json::json!([
                    je("t", "a", "b", serde_json::json!({})),
                    je("t", "b", "c", serde_json::json!({}))
                ]),
            ),
        ),
        (
            "노드 삭제(닿은 엣지 포함)",
            jdoc(
                serde_json::json!([
                    jn(
                        "a",
                        &["Item"],
                        serde_json::json!({ "name": "A", "extra": 1 })
                    ),
                    jn("b", &["Item"], serde_json::json!({ "name": "B" })),
                ]),
                serde_json::json!([je("t", "a", "b", serde_json::json!({}))]),
            ),
        ),
        (
            "엣지 추가",
            jdoc(
                serde_json::json!([
                    jn(
                        "a",
                        &["Item"],
                        serde_json::json!({ "name": "A", "extra": 1 })
                    ),
                    jn("b", &["Item"], serde_json::json!({ "name": "B" })),
                    jn("c", &["Item"], serde_json::json!({ "name": "C" })),
                ]),
                serde_json::json!([
                    je("t", "a", "b", serde_json::json!({})),
                    je("t", "b", "c", serde_json::json!({})),
                    je("t", "a", "c", serde_json::json!({ "w": 3 }))
                ]),
            ),
        ),
        (
            "엣지 삭제",
            jdoc(
                serde_json::json!([
                    jn(
                        "a",
                        &["Item"],
                        serde_json::json!({ "name": "A", "extra": 1 })
                    ),
                    jn("b", &["Item"], serde_json::json!({ "name": "B" })),
                    jn("c", &["Item"], serde_json::json!({ "name": "C" })),
                ]),
                serde_json::json!([je("t", "a", "b", serde_json::json!({}))]),
            ),
        ),
        (
            "엣지 속성 변경",
            jdoc(
                serde_json::json!([
                    jn(
                        "a",
                        &["Item"],
                        serde_json::json!({ "name": "A", "extra": 1 })
                    ),
                    jn("b", &["Item"], serde_json::json!({ "name": "B" })),
                    jn("c", &["Item"], serde_json::json!({ "name": "C" })),
                ]),
                serde_json::json!([
                    je("t", "a", "b", serde_json::json!({ "w": 9 })),
                    je("t", "b", "c", serde_json::json!({}))
                ]),
            ),
        ),
    ];
    for (i, (name, changed)) in cases.into_iter().enumerate() {
        let env = new_env(true);
        let sess = env.engine.session("u");
        let g = sess.create_graph("acme", "dst").unwrap();
        sync(&sess, DST, base_doc()).unwrap();
        let r = sync(&sess, DST, changed.clone()).unwrap();
        assert!(is_merge(&r, "ff"), "{name}: {:?}", r.outcome);
        assert!(r.via_commit.is_some(), "{name}: via 에 새 커밋");
        assert_eq!(
            export_text(&sess, DST),
            expected_export(&sess, &format!("acme/exp{i}"), changed),
            "{name}: 문서와 같은 그래프"
        );
        assert_eq!(head_of(&sess, DST), head_of(&sess, "acme/dst@sync"));
        assert_eq!(
            sess.log(g, "main").unwrap().len(),
            2,
            "{name}: 커밋은 via 의 1개만 늘어남"
        );
    }
}

/// [핵심] 그래프 쪽만 바뀐 경우(대상에서 직접 속성 추가) → 이후 가져오기에서도 유지된다.
#[test]
fn merge_keeps_graph_side_edits_across_imports() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    sync(&sess, DST, base_doc()).unwrap();
    edit_main(&sess, g, |es| {
        es.set_prop(es.by_slug("a").unwrap(), "note", "mine".into())
            .unwrap();
    });
    // 같은 문서 → 변경 없음: via 는 대상의 조상이라 이미 최신
    let r = sync(&sess, DST, base_doc()).unwrap();
    assert!(is_merge(&r, "up_to_date"), "{:?}", r.outcome);
    // 문서의 다른 곳이 바뀌면 병합 커밋이 만들어지고 note 는 남는다
    let changed = jdoc(
        serde_json::json!([
            jn(
                "a",
                &["Item"],
                serde_json::json!({ "name": "A", "extra": 1 })
            ),
            jn("b", &["Item"], serde_json::json!({ "name": "B2" })),
            jn("c", &["Item"], serde_json::json!({ "name": "C" })),
        ]),
        serde_json::json!([
            je("t", "a", "b", serde_json::json!({})),
            je("t", "b", "c", serde_json::json!({}))
        ]),
    );
    let r = sync(&sess, DST, changed).unwrap();
    assert!(is_merge(&r, "merged"), "{:?}", r.outcome);
    let text = export_text(&sess, DST);
    assert!(
        text.contains("\"note\":\"mine\"") && text.contains("\"name\":\"B2\""),
        "{text}"
    );
}

/// [핵심] 양쪽이 같은 속성을 다르게 바꾸면 충돌 보고 + 대상 불변, 이후 기존 merge 로 해소한다.
#[test]
fn merge_conflict_reports_prop_both_set_and_resolves_with_existing_merge() {
    for choose_theirs in [true, false] {
        let env = new_env(true);
        let sess = env.engine.session("u");
        let g = sess.create_graph("acme", "dst").unwrap();
        sync(&sess, DST, base_doc()).unwrap();
        edit_main(&sess, g, |es| {
            es.set_prop(es.by_slug("a").unwrap(), "name", "T".into())
                .unwrap();
        });
        let before = head_of(&sess, DST);
        let doc = jdoc(
            serde_json::json!([
                jn(
                    "a",
                    &["Item"],
                    serde_json::json!({ "name": "D", "extra": 1 })
                ),
                jn("b", &["Item"], serde_json::json!({ "name": "B" })),
                jn("c", &["Item"], serde_json::json!({ "name": "C" })),
            ]),
            serde_json::json!([
                je("t", "a", "b", serde_json::json!({})),
                je("t", "b", "c", serde_json::json!({}))
            ]),
        );
        let r = sync(&sess, DST, doc).unwrap();
        assert!(is_merge(&r, "conflicts"), "{:?}", r.outcome);
        assert!(r.via_commit.is_some(), "via 에는 문서가 반영됨");
        assert!(matches!(
            conflict_items(&r)[0],
            vgraph_engine::ConflictItem::PropBothSet { .. }
        ));
        assert_eq!(head_of(&sess, DST), before, "대상 브랜치 불변");

        let choose = if choose_theirs {
            Choice::Theirs
        } else {
            Choice::Ours
        };
        let out = sess
            .merge(
                g,
                VIA,
                "main",
                &MergeOpts {
                    resolutions: vec![Resolution { item: 0, choose }],
                },
            )
            .unwrap();
        assert!(matches!(out, MergeOutcome::Merged(_)), "{out:?}");
        let text = export_text(&sess, DST);
        let want = if choose_theirs {
            "\"name\":\"D\""
        } else {
            "\"name\":\"T\""
        };
        assert!(text.contains(want), "{text}");
    }
}

/// [핵심] 한쪽은 지우고 다른 쪽은 고친 경우 → 충돌(삭제 대 수정, 번호 해소 불가), 대상 불변.
#[test]
fn merge_removed_vs_edit_is_conflict() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    sync(&sess, DST, base_doc()).unwrap();
    edit_main(&sess, g, |es| {
        es.set_prop(es.by_slug("b").unwrap(), "name", "고침".into())
            .unwrap();
    });
    let before = head_of(&sess, DST);
    let doc = jdoc(
        serde_json::json!([
            jn(
                "a",
                &["Item"],
                serde_json::json!({ "name": "A", "extra": 1 })
            ),
            jn("c", &["Item"], serde_json::json!({ "name": "C" })),
        ]),
        serde_json::json!([]),
    );
    let r = sync(&sess, DST, doc).unwrap();
    assert!(conflict_items(&r)
        .iter()
        .any(|i| matches!(i, vgraph_engine::ConflictItem::RemovedVsEdit { .. })));
    assert_eq!(head_of(&sess, DST), before);
}

/// [중요] 같은 문서를 두 번 → 두 번째는 UpToDate, 커밋 0.
#[test]
fn merge_same_document_twice_is_up_to_date_without_commits() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    sync(&sess, DST, base_doc()).unwrap();
    let (m, v) = (head_of(&sess, DST), head_of(&sess, "acme/dst@sync"));
    let r = sync(&sess, DST, base_doc()).unwrap();
    assert!(is_merge(&r, "up_to_date"), "{:?}", r.outcome);
    assert_eq!(r.via_commit, None);
    assert_eq!(
        (head_of(&sess, DST), head_of(&sess, "acme/dst@sync")),
        (m, v)
    );
    assert!(sess.list_working(g).unwrap().is_empty(), "작업분 흔적 없음");
}

/// 충돌 대기 중 재가져오기: via 를 새 문서에 다시 맞추고 충돌을 새로 계산한다(대상은 계속 불변).
#[test]
fn merge_reimport_while_conflict_pending_recomputes_against_new_document() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    sync(&sess, DST, base_doc()).unwrap();
    edit_main(&sess, g, |es| {
        es.set_prop(es.by_slug("a").unwrap(), "name", "T".into())
            .unwrap();
    });
    let with_name = |name: &str| {
        jdoc(
            serde_json::json!([
                jn(
                    "a",
                    &["Item"],
                    serde_json::json!({ "name": name, "extra": 1 })
                ),
                jn("b", &["Item"], serde_json::json!({ "name": "B" })),
                jn("c", &["Item"], serde_json::json!({ "name": "C" })),
            ]),
            serde_json::json!([
                je("t", "a", "b", serde_json::json!({})),
                je("t", "b", "c", serde_json::json!({}))
            ]),
        )
    };
    let main_before = head_of(&sess, DST);
    assert!(is_merge(
        &sync(&sess, DST, with_name("D")).unwrap(),
        "conflicts"
    ));
    // 같은 충돌 문서를 다시 넣어도 여전히 충돌, via 는 늘지 않는다
    let via1 = head_of(&sess, "acme/dst@sync");
    let r = sync(&sess, DST, with_name("D")).unwrap();
    assert!(is_merge(&r, "conflicts"));
    assert_eq!(
        (r.via_commit, head_of(&sess, "acme/dst@sync")),
        (None, via1)
    );
    // 문서가 대상과 같은 값으로 바뀌면 충돌이 사라지고 병합된다
    let r = sync(&sess, DST, with_name("T")).unwrap();
    assert!(is_merge(&r, "merged"), "{:?}", r.outcome);
    assert_ne!(head_of(&sess, DST), main_before);
}

/// 이 모드에서 문서 안 같은 (etype, from, to) 중복은 거부 — 저장소 접근 전에.
#[test]
fn merge_rejects_duplicate_edges_in_document() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    let doc = jdoc(
        serde_json::json!([
            jn("a", &[], serde_json::json!({})),
            jn("b", &[], serde_json::json!({}))
        ]),
        serde_json::json!([
            je("t", "a", "b", serde_json::json!({ "w": 1 })),
            je("t", "a", "b", serde_json::json!({ "w": 2 }))
        ]),
    );
    let e = sync(&sess, DST, doc).unwrap_err();
    assert_eq!(
        problem_codes(&e),
        vec![(ProblemCode::DuplicateEdge, "t a→b".to_string())]
    );
    assert!(head_is_none(&sess, DST) && head_is_none(&sess, "acme/dst@sync"));
    assert!(sess.list_working(g).unwrap().is_empty());
}

/// via 쪽에 같은 (etype, from, to)가 여럿이면 uuid 가 가장 작은 하나만 남기고 문서 props 로 맞춘다.
#[test]
fn merge_collapses_duplicate_edges_on_via_side() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    // replace 는 중복 엣지를 허용하므로 via 쪽에 중복이 생길 수 있다
    let dup = jdoc(
        serde_json::json!([
            jn("a", &[], serde_json::json!({})),
            jn("b", &[], serde_json::json!({}))
        ]),
        serde_json::json!([
            je("t", "a", "b", serde_json::json!({ "w": 1 })),
            je("t", "a", "b", serde_json::json!({ "w": 2 }))
        ]),
    );
    interchange::import_replace(&sess, DST, dup, "dup").unwrap();
    sess.branch(g, head_of(&sess, DST), VIA).unwrap();
    let one = jdoc(
        serde_json::json!([
            jn("a", &[], serde_json::json!({})),
            jn("b", &[], serde_json::json!({}))
        ]),
        serde_json::json!([je("t", "a", "b", serde_json::json!({ "w": 5 }))]),
    );
    let r = sync(&sess, DST, one.clone()).unwrap();
    assert!(is_merge(&r, "ff"), "{:?}", r.outcome);
    assert_eq!(
        export_text(&sess, DST),
        expected_export(&sess, "acme/exp", one)
    );
}

/// `__lockfile` 은 문서에 있으면 거부하고, via 쪽에 있어도 지우거나 건드리지 않는다.
#[test]
fn merge_rejects_lockfile_in_document_and_leaves_via_lockfile_alone() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    sync(&sess, DST, base_doc()).unwrap();
    let es = sess.edit(g, VIA).unwrap();
    es.add_node(&["__system"], Some("__lockfile"), props([("x", 1.into())]))
        .unwrap();
    committed(sess.commit(&es, "lock").unwrap());
    let via_before = head_of(&sess, "acme/dst@sync");

    // 문서에 __lockfile 이 있으면 파싱 단계에서 거부된다
    let bad = serde_json::json!({ "format": FORMAT, "nodes": [jn("__lockfile", &[], serde_json::json!({}))], "edges": [] });
    assert!(matches!(
        interchange::parse_document(&bad.to_string()),
        Err(InterchangeError::Rejected(_))
    ));
    let r = sync(&sess, DST, base_doc()).unwrap();
    assert_eq!(r.via_commit, None, "lockfile 은 문서 비교 대상이 아니다");
    let (vg, vc) = sess.resolve(&format!("graph://acme/dst@{VIA}")).unwrap();
    assert_eq!(vc, via_before);
    assert!(sess
        .checkout(vg, vc)
        .unwrap()
        .by_slug("__lockfile")
        .is_some());
}

/// 실패 경로: 스키마 위반이면 via·대상 head 불변, 작업분 없음. 이어서 올바른 문서는 정상 반영된다.
#[test]
fn merge_schema_violation_leaves_both_branches_and_working_copies_clean() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    let mut nc = BTreeMap::new();
    nc.insert(
        "Rule".into(),
        NodeClassDef {
            required: vec!["name".into()],
        },
    );
    let mut s = String::new();
    vgraph_core::canon::write_value(
        &Value::Map(
            Schema {
                mode: SchemaMode::Strict,
                node_classes: nc,
                edge_classes: BTreeMap::new(),
            }
            .to_props(),
        ),
        &mut s,
    );
    let schema_node: serde_json::Value = serde_json::from_str(&format!(
        r#"{{"slug":"__schema","labels":["__system"],"props":{s}}}"#
    ))
    .unwrap();
    let ok = jdoc(
        serde_json::json!([
            schema_node.clone(),
            jn("r", &["Rule"], serde_json::json!({ "name": "R" }))
        ]),
        serde_json::json!([]),
    );
    sync(&sess, DST, ok).unwrap();
    let (m, v) = (head_of(&sess, DST), head_of(&sess, "acme/dst@sync"));
    let bad = jdoc(
        serde_json::json!([
            schema_node,
            jn("r", &["Rule"], serde_json::json!({ "name": "R" })),
            jn("r2", &["Rule"], serde_json::json!({}))
        ]),
        serde_json::json!([]),
    );
    let e = sync(&sess, DST, bad).unwrap_err();
    assert!(
        matches!(e, InterchangeError::SchemaViolation(ref v) if !v.is_empty()),
        "{e:?}"
    );
    assert_eq!(
        (head_of(&sess, DST), head_of(&sess, "acme/dst@sync")),
        (m, v)
    );
    assert!(sess.list_working(g).unwrap().is_empty());
}

/// 실패 경로: 링크 대상 미등록 → 같은 보장.
#[test]
fn merge_unregistered_link_leaves_both_branches_clean() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    sync(&sess, DST, base_doc()).unwrap();
    let (m, v) = (head_of(&sess, DST), head_of(&sess, "acme/dst@sync"));
    let doc = jdoc(
        serde_json::json!([jn(
            "a",
            &["Item"],
            serde_json::json!({ "link": { "$ref": "graph://nowhere/none#x" } })
        )]),
        serde_json::json!([]),
    );
    let e = sync(&sess, DST, doc).unwrap_err();
    assert!(matches!(e, InterchangeError::Engine(_)), "{e:?}");
    assert_eq!(
        (head_of(&sess, DST), head_of(&sess, "acme/dst@sync")),
        (m, v)
    );
    assert!(sess.list_working(g).unwrap().is_empty());
}

/// 인자·상태 오류: via 이름 형식, via == 대상, via 의 미커밋 편집, via 는 있는데 대상 브랜치가 없음.
#[test]
fn merge_argument_and_state_errors() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    sync(&sess, DST, base_doc()).unwrap();
    for bad in ["", "a.b", "a b", "a@b"] {
        let e = interchange::import_merge(&sess, DST, bad, base_doc(), "x").unwrap_err();
        assert!(matches!(e, InterchangeError::BadAddr(_)), "{bad:?}: {e:?}");
    }
    let e = interchange::import_merge(&sess, DST, "main", base_doc(), "x").unwrap_err();
    assert!(matches!(e, InterchangeError::BadAddr(_)), "{e:?}");
    let e = interchange::import_merge(&sess, "acme/dst@other", VIA, base_doc(), "x").unwrap_err();
    assert!(matches!(e, InterchangeError::NoBaseline(_)), "{e:?}");
    let e = interchange::import_merge(&sess, "acme/none", VIA, base_doc(), "x").unwrap_err();
    assert!(matches!(e, InterchangeError::BadAddr(_)), "{e:?}");

    let es = sess.edit(g, VIA).unwrap();
    es.add_node(&["Item"], Some("mine"), props([])).unwrap();
    let e = sync(&sess, DST, base_doc()).unwrap_err();
    assert!(matches!(e, InterchangeError::PendingEdits), "{e:?}");
    assert!(es.dirty(), "사용자의 편집은 그대로");
}

/// [핵심] via_branch 에 `/` 가 든 이름(`import/feed`)도 쓸 수 있다 — 첫 가져오기와 이어지는 동기화 모두.
#[test]
fn merge_via_branch_with_slash_works_for_first_and_following_imports() {
    const SLASH_VIA: &str = "import/feed";
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    let r = interchange::import_merge(&sess, DST, SLASH_VIA, base_doc(), "first").unwrap();
    assert!(is_merge(&r, "first"), "{:?}", r.outcome);
    assert_eq!(
        head_of(&sess, "acme/dst@import/feed"),
        head_of(&sess, DST),
        "via 브랜치는 대상과 같은 커밋"
    );
    let changed = jdoc(
        serde_json::json!([
            jn("a", &["Item"], serde_json::json!({ "name": "A2" })),
            jn("b", &["Item"], serde_json::json!({ "name": "B" })),
        ]),
        serde_json::json!([je("t", "a", "b", serde_json::json!({}))]),
    );
    let r = interchange::import_merge(&sess, DST, SLASH_VIA, changed, "second").unwrap();
    assert!(is_merge(&r, "ff"), "{:?}", r.outcome);
    assert_eq!(
        head_of(&sess, "acme/dst@import/feed"),
        head_of(&sess, DST),
        "대상에 병합됨"
    );
    assert_eq!(sess.log(g, "main").unwrap().len(), 2);
}

/// [중요] via_branch 가 브랜치 이름 규칙을 어기면 거부하고 저장소는 그대로, 문구에 via_branch 가 들어간다.
#[test]
fn merge_via_branch_rule_violations_are_rejected_without_touching_store() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    sess.create_graph("acme", "dst").unwrap();
    for bad in ["a//b", "/a", "a/", "a b", "a.b"] {
        let e = interchange::import_merge(&sess, DST, bad, base_doc(), "x").unwrap_err();
        assert!(
            matches!(e, InterchangeError::BadAddr(ref m) if m.contains("via_branch")),
            "{bad:?}: {e:?}"
        );
        assert!(head_is_none(&sess, DST), "{bad:?}: 대상 불변");
    }
}

// ---------- 모르는 칸 거부 (M1) ----------

fn unknown_fields(e: &InterchangeError) -> Vec<String> {
    match e {
        InterchangeError::UnknownFields(v) => v.clone(),
        other => panic!("UnknownFields 기대, 실제: {other:?}"),
    }
}

/// [핵심] `labels`를 `label`로 쓴 문서는 라벨을 잃은 채 들어가지 않고 위치와 함께 거부된다.
#[test]
fn import_rejects_label_typo_with_location_and_commits_nothing() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    let text = doc_text(
        r#"{"slug":"a","labels":["X"],"props":{}},{"slug":"b","labels":["X"],"props":{}},{"slug":"c","labels":["X"],"props":{}},{"slug":"d","label":["Item"],"props":{}}"#,
        "",
    );
    let e = interchange::parse_document(&text).unwrap_err();
    assert_eq!(unknown_fields(&e), vec!["nodes[3].label".to_string()]);
    assert!(e.to_string().contains("nodes[3].label"), "{e}");
    assert_untouched(&sess, g, DST);
}

/// 최상위·엣지의 모르는 칸도 거부하고, 여러 개면 한 번에 모두 나열한다.
#[test]
fn import_rejects_top_level_and_edge_unknown_fields_listing_all() {
    let text = format!(
        r#"{{"format":"{FORMAT}","version":2,"nodes":[{{"slug":"a","labels":[],"props":{{}},"id":1}},{{"slug":"b","props":{{}}}}],"edges":[{{"etype":"t","from":"a","to":"b","kind":"x"}},{{"etype":"t","from":"a","to":"b","props":{{}},"weight":1,"note":"n"}}],"extra":[]}}"#
    );
    let e = interchange::parse_document(&text).unwrap_err();
    let mut got = unknown_fields(&e);
    got.sort();
    let mut want: Vec<String> = [
        "version",
        "extra",
        "nodes[0].id",
        "edges[0].kind",
        "edges[1].weight",
        "edges[1].note",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    want.sort();
    assert_eq!(got, want);
}

/// 목록은 잘라 내지 않는다 (20개를 넘어도 전부).
#[test]
fn unknown_fields_list_is_not_truncated() {
    let nodes: Vec<String> = (0..30)
        .map(|i| format!(r#"{{"slug":"n{i}","oops":1}}"#))
        .collect();
    let text = doc_text(&nodes.join(","), "");
    let e = interchange::parse_document(&text).unwrap_err();
    assert_eq!(unknown_fields(&e).len(), 30);
    assert!(e.to_string().contains("nodes[29].oops"), "{e}");
}

/// props 안의 키는 사용자 데이터라 모르는 칸이 아니다.
#[test]
fn props_keys_are_not_unknown_fields() {
    let text = doc_text(
        r#"{"slug":"a","labels":[],"props":{"label":"자유 키","kind":1}}"#,
        "",
    );
    assert!(interchange::parse_document(&text).is_ok());
}

/// [중요] 내보낸 문서는 그대로 가져와진다 (왕복).
#[test]
fn exported_document_passes_unknown_field_check() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    build_source(&sess, SRC);
    let text = export_text(&sess, SRC);
    let doc = interchange::parse_document(&text).unwrap();
    assert_eq!(doc.nodes.len(), 4, "3개 + __schema");
    assert_eq!(doc.edges.len(), 4);
}

/// merge 모드도 같은 문서 검사를 거치며, 거부되면 가져오기 전용 브랜치조차 생기지 않는다.
#[test]
fn merge_path_rejects_unknown_fields_before_touching_store() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let g = sess.create_graph("acme", "dst").unwrap();
    let text = doc_text(r#"{"slug":"a","label":["X"],"props":{}}"#, "");
    let e = interchange::parse_document(&text).unwrap_err();
    assert_eq!(unknown_fields(&e), vec!["nodes[0].label".to_string()]);
    assert_untouched(&sess, g, DST);
    assert!(head_is_none(&sess, "acme/dst@import"), "via 브랜치 없음");
}

// ---------- 나눠 받기: 문서를 문자 경계에서 조각으로 ----------

/// 한글·이모지(3·4바이트 문자)를 섞은 노드 `count`개짜리 그래프의 내보내기.
fn multibyte_export(sess: &Session, count: usize) -> interchange::Exported {
    let g = sess.create_graph("acme", "parts").unwrap();
    let es = sess.edit(g, "main").unwrap();
    for i in 0..count {
        es.add_node(
            &["Item"],
            Some(&format!("n-{i:04}")),
            props([("name", format!("한글🙂노드 {i} 𝄞").into())]),
        )
        .unwrap();
    }
    committed(sess.commit(&es, "seed").unwrap());
    interchange::export(sess, "acme/parts").unwrap()
}

fn join_parts(ex: &interchange::Exported, max: usize) -> (String, usize) {
    let mut out = String::new();
    let mut at = 0usize;
    let mut n = 0usize;
    loop {
        let p = ex.part(at, max).unwrap();
        assert_eq!(p.offset, at);
        assert!(
            !p.text.is_empty() && p.text.len() <= max,
            "조각 크기 {}",
            p.text.len()
        );
        out.push_str(p.text);
        n += 1;
        match p.next_offset {
            Some(next) => {
                assert_eq!(next, at + p.text.len());
                at = next;
            }
            None => return (out, n),
        }
    }
}

#[test]
fn export_parts_concatenate_to_the_whole_document_on_char_boundaries() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let ex = multibyte_export(&sess, 400);
    assert!(ex.text.len() > 4 * interchange::PART_MIN_BYTES);
    for max in [
        1024,
        1025,
        1026,
        1027,
        4099,
        20_000,
        interchange::PART_MAX_BYTES,
    ] {
        let (joined, n) = join_parts(&ex, max);
        assert_eq!(joined, ex.text, "max_bytes={max}: 바이트 동일");
        assert!(n >= ex.text.len().div_ceil(max), "조각 수 {n}");
    }
    // 문서가 조각 하나보다 작으면 조각 하나, 다음 없음
    let small = multibyte_export_small(&sess);
    let p = small.part(0, interchange::PART_MAX_BYTES).unwrap();
    assert_eq!((p.text, p.next_offset), (small.text.as_str(), None));
}

fn multibyte_export_small(sess: &Session) -> interchange::Exported {
    let g = sess.create_graph("acme", "tiny").unwrap();
    let es = sess.edit(g, "main").unwrap();
    es.add_node(&["Item"], Some("only"), props([("name", "하나".into())]))
        .unwrap();
    committed(sess.commit(&es, "seed").unwrap());
    interchange::export(sess, "acme/tiny").unwrap()
}

#[test]
fn export_part_rejects_out_of_range_size_and_bad_offsets() {
    let env = new_env(true);
    let sess = env.engine.session("u");
    let ex = multibyte_export(&sess, 400);
    let is_part = |e: InterchangeError| matches!(e, InterchangeError::Part(_));
    assert!(is_part(
        ex.part(0, interchange::PART_MIN_BYTES - 1).unwrap_err()
    ));
    assert!(is_part(
        ex.part(0, interchange::PART_MAX_BYTES + 1).unwrap_err()
    ));
    assert!(is_part(ex.part(0, 0).unwrap_err()));
    assert!(
        is_part(ex.part(ex.text.len(), 1024).unwrap_err()),
        "끝 위치"
    );
    assert!(is_part(ex.part(ex.text.len() + 1, 1024).unwrap_err()));
    let mid_char = ex
        .text
        .char_indices()
        .find(|(_, c)| c.len_utf8() > 1)
        .map(|(i, _)| i + 1)
        .unwrap();
    assert!(!ex.text.is_char_boundary(mid_char));
    assert!(is_part(ex.part(mid_char, 1024).unwrap_err()), "문자 중간");
}
