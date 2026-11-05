//! 골든 V 계열 v2 — 유사도 사이드카 (골든 TC §2-V + V9~V11 확장).
//! V9 = 선택 필드 staleness(무관 prop 변경은 재임베딩 없음),
//! V10 = Props 청킹(노드당 다중 벡터 + max-pooling), V11 = 멀티모델 병존.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use vgraph_core::value::props;
use vgraph_core::*;
use vgraph_engine::*;
use vgraph_vec::*;

fn toy() -> HashBagEmbedder {
    HashBagEmbedder::default() // model_id "toy-32"
}

fn spec() -> EmbedSpec {
    EmbedSpec::default() // fields=None, Whole
}

struct Fv {
    _dir: TempDir,
    root: PathBuf,
    engine: Engine,
    g: GraphId,
    n1: Uuid, // req-login
    n2: Uuid, // req-loan
    n3: Uuid, // pol-perf
    c0: Hash256,
}

fn committed(o: CommitOutcome) -> Hash256 {
    match o {
        CommitOutcome::Committed { commit, .. } => commit,
        other => panic!("{other:?}"),
    }
}

fn fv() -> Fv {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let engine = Engine::open(
        Box::new(FsProvider::new(&root).unwrap()),
        Arc::new(TestIdGen::new()),
        Arc::new(TestClock::default()),
        EngineOptions::default(),
    )
    .unwrap();
    let sess = engine.session("ceo");
    let g = sess.create_graph("acme", "kb").unwrap();
    let es = sess.edit(g, "main").unwrap();
    let n1 = es
        .add_node(
            &["Requirement"],
            Some("req-login"),
            props([("text", "이메일 로그인 인증".into())]),
        )
        .unwrap();
    let n2 = es
        .add_node(
            &["Requirement"],
            Some("req-loan"),
            props([("text", "대출 한도 다섯 권".into())]),
        )
        .unwrap();
    let n3 = es
        .add_node(
            &["Policy"],
            Some("pol-perf"),
            props([("text", "응답 시간 이백 밀리초".into())]),
        )
        .unwrap();
    let c0 = committed(sess.commit(&es, "FV").unwrap());
    Fv {
        _dir: dir,
        root,
        engine,
        g,
        n1,
        n2,
        n3,
        c0,
    }
}

fn idx_of(f: &Fv) -> VecIndex {
    VecIndex::for_graph(&f.root, f.g, &toy().model_id()).unwrap()
}

#[test]
fn v1_pending_all_missing() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let idx = idx_of(&f);
    let pending = idx.pending(&v0, &spec()).unwrap();
    assert_eq!(pending.len(), 3);
    assert_eq!(
        pending.iter().map(|p| p.node).collect::<Vec<_>>(),
        vec![f.n1, f.n2, f.n3]
    );
    assert!(pending.iter().all(|p| p.reason == PendingReason::Missing));
}

#[test]
fn v2_embed_pending_idempotent() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let idx = idx_of(&f);
    assert_eq!(embed_pending(&idx, &v0, &toy(), &spec()).unwrap(), 3);
    assert_eq!(embed_pending(&idx, &v0, &toy(), &spec()).unwrap(), 0);
    assert_eq!(idx.pending(&v0, &spec()).unwrap(), vec![]);
}

#[test]
fn v3_search_top1_deterministic() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let idx = idx_of(&f);
    embed_pending(&idx, &v0, &toy(), &spec()).unwrap();
    let hits = idx
        .search(&v0, &toy().embed("이메일 로그인").unwrap(), 2)
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].node, f.n1);
    assert_eq!(hits[0].part, "full"); // Whole 청커
    assert!(hits[0].score > hits[1].score);
}

#[test]
fn v4_epoch_join_branch_isolation() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let idx = idx_of(&f);
    embed_pending(&idx, &v0, &toy(), &spec()).unwrap();
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let ed = sess.edit(f.g, "dev-b").unwrap();
    let n4 = ed
        .add_node(
            &["Requirement"],
            Some("req-dev"),
            props([("text", "이메일 로그인 추가 개선".into())]),
        )
        .unwrap();
    let d1 = committed(sess.commit(&ed, "d1").unwrap());
    let v_dev = sess.checkout(f.g, d1).unwrap();
    assert_eq!(embed_pending(&idx, &v_dev, &toy(), &spec()).unwrap(), 1);
    let q = toy().embed("이메일 로그인").unwrap();
    assert!(!idx
        .search(&v0, &q, 10)
        .unwrap()
        .iter()
        .any(|h| h.node == n4));
    assert!(idx
        .search(&v_dev, &q, 10)
        .unwrap()
        .iter()
        .any(|h| h.node == n4));
}

#[test]
fn v5_stale_excluded_and_queued() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let idx = idx_of(&f);
    embed_pending(&idx, &v0, &toy(), &spec()).unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n1, "text", "전화번호 인증으로 변경".into())
        .unwrap();
    let c1 = committed(sess.commit(&es, "c1").unwrap());
    let v1 = sess.checkout(f.g, c1).unwrap();
    let pending = idx.pending(&v1, &spec()).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].node, f.n1);
    assert_eq!(pending[0].reason, PendingReason::Stale);
    let q = toy().embed("이메일 로그인").unwrap();
    assert!(!idx
        .search(&v1, &q, 10)
        .unwrap()
        .iter()
        .any(|h| h.node == f.n1));
}

#[test]
fn v6_reembed_restores_and_old_view_consistent() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let idx = idx_of(&f);
    embed_pending(&idx, &v0, &toy(), &spec()).unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n1, "text", "전화번호 인증으로 변경".into())
        .unwrap();
    let c1 = committed(sess.commit(&es, "c1").unwrap());
    let v1 = sess.checkout(f.g, c1).unwrap();
    assert_eq!(embed_pending(&idx, &v1, &toy(), &spec()).unwrap(), 1);
    let hits = idx
        .search(&v1, &toy().embed("전화번호 인증").unwrap(), 1)
        .unwrap();
    assert_eq!(hits[0].node, f.n1);
    let q_old = toy().embed("이메일 로그인").unwrap();
    assert!(!idx
        .search(&v0, &q_old, 10)
        .unwrap()
        .iter()
        .any(|h| h.node == f.n1));
}

#[test]
fn v7_system_nodes_excluded() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let idx = idx_of(&f);
    embed_pending(&idx, &v0, &toy(), &spec()).unwrap();
    let schema = Schema {
        mode: SchemaMode::Warn,
        node_classes: Default::default(),
        edge_classes: Default::default(),
    };
    let c2 = committed(sess.set_schema(f.g, "main", &schema).unwrap());
    let v2 = sess.checkout(f.g, c2).unwrap();
    assert_eq!(idx.pending(&v2, &spec()).unwrap(), vec![]);
}

#[test]
fn v8_prune_dead_embeddings() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let idx = idx_of(&f);
    embed_pending(&idx, &v0, &toy(), &spec()).unwrap();
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let ed = sess.edit(f.g, "dev-b").unwrap();
    let n4 = ed
        .add_node(
            &["Requirement"],
            Some("req-dev"),
            props([("text", "개선".into())]),
        )
        .unwrap();
    let d1 = committed(sess.commit(&ed, "d1").unwrap());
    let v_dev = sess.checkout(f.g, d1).unwrap();
    embed_pending(&idx, &v_dev, &toy(), &spec()).unwrap();
    let keep: BTreeSet<Uuid> = v0.node_hashes().iter().map(|(u, _)| *u).collect();
    assert_eq!(idx.prune(&keep).unwrap(), 1);
    assert!(idx.parts(n4).unwrap().is_empty());
    assert!(!idx.parts(f.n2).unwrap().is_empty());
    let _ = f.n3;
}

/// V9 — 선택 필드(2단계 staleness): 임베딩 대상 아닌 prop 변경 → 재임베딩 없음,
/// node_hash는 조용히 갱신되어 새 뷰에서도 검색이 즉시 성립.
#[test]
fn v9_field_selective_staleness() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let idx = idx_of(&f);
    let sp = EmbedSpec {
        fields: Some(vec!["text".into()]),
        chunker: Chunker::Whole { max_chars: 8000 },
        ..Default::default()
    };
    assert_eq!(embed_pending(&idx, &v0, &toy(), &sp).unwrap(), 3);
    // 임베딩 무관 prop(status)만 변경
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n1, "status", "active".into()).unwrap();
    let c1 = committed(sess.commit(&es, "c1").unwrap());
    let v1 = sess.checkout(f.g, c1).unwrap();
    // 텍스트 불변 → pending 없음 (재임베딩 회피) + node_hash 자동 갱신
    assert_eq!(idx.pending(&v1, &sp).unwrap(), vec![]);
    // 갱신 덕에 새 뷰(epoch)에서 즉시 검색 성립
    let hits = idx
        .search(&v1, &toy().embed("이메일 로그인").unwrap(), 1)
        .unwrap();
    assert_eq!(hits[0].node, f.n1);
}

/// V10 — Props 청킹: 노드당 다중 벡터, 검색은 max-pooling + 최고 청크(part) 반환.
#[test]
fn v10_props_chunking_multi_vector() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let es = sess.edit(f.g, "main").unwrap();
    // title은 질의와 무관하게 (토이 임베더 = 표면 유사도 — 문자 겹침이 판정 기준)
    let n_doc = es
        .add_node(
            &["Doc"],
            Some("doc-1"),
            props([
                ("title", "시스템 소개".into()),
                ("body", "카드 결제와 환불 처리 절차".into()),
            ]),
        )
        .unwrap();
    let c1 = committed(sess.commit(&es, "doc").unwrap());
    let v1 = sess.checkout(f.g, c1).unwrap();
    let idx = idx_of(&f);
    let sp = EmbedSpec {
        fields: None,
        chunker: Chunker::Props,
        ..Default::default()
    };
    embed_pending(&idx, &v1, &toy(), &sp).unwrap();
    // 노드당 청크 2개 (다중 벡터)
    assert_eq!(idx.parts(n_doc).unwrap(), vec!["p:body", "p:title"]);
    // body 쪽 질의 → max-pooling 1위 = n_doc, provenance = p:body
    let hits = idx
        .search(&v1, &toy().embed("카드 결제 환불 처리").unwrap(), 1)
        .unwrap();
    assert_eq!(hits[0].node, n_doc);
    assert_eq!(hits[0].part, "p:body");
}

/// V11 — 멀티모델 병존: (uuid, model, part) 키 — 같은 노드에 모델별 벡터 공존.
#[test]
fn v11_multi_model_coexist() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let m32 = HashBagEmbedder { dim: 32 };
    let m64 = HashBagEmbedder { dim: 64 };
    let idx32 = VecIndex::for_graph(&f.root, f.g, &m32.model_id()).unwrap();
    let idx64 = VecIndex::for_graph(&f.root, f.g, &m64.model_id()).unwrap();
    assert_eq!(embed_pending(&idx32, &v0, &m32, &spec()).unwrap(), 3);
    assert_eq!(embed_pending(&idx64, &v0, &m64, &spec()).unwrap(), 3);
    // 각 모델 네임스페이스에서 독립 검색 (차원도 다름)
    let h32 = idx32
        .search(&v0, &m32.embed("이메일 로그인").unwrap(), 1)
        .unwrap();
    let h64 = idx64
        .search(&v0, &m64.embed("이메일 로그인").unwrap(), 1)
        .unwrap();
    assert_eq!(h32[0].node, f.n1);
    assert_eq!(h64[0].node, f.n1);
    // 파트 격리 확인 (모델별)
    assert_eq!(idx32.parts(f.n1).unwrap(), vec!["full"]);
    assert_eq!(idx64.parts(f.n1).unwrap(), vec!["full"]);
}

/// V12 — 명시 embed_target: 지정 텍스트만 청크(t0, t1…), 타깃 외 prop 변경은 재임베딩 0.
#[test]
fn v12_explicit_embed_target() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let es = sess.edit(f.g, "main").unwrap();
    let n_doc = es
        .add_node(
            &["Doc"],
            Some("doc-t"),
            props([
                ("text", "본문 전체 텍스트는 따로 있고 매우 길다".into()),
                (
                    "embed_target",
                    Value::List(vec![
                        Value::Str("카드 결제".into()),
                        Value::Str("환불 절차".into()),
                    ]),
                ),
            ]),
        )
        .unwrap();
    let c1 = committed(sess.commit(&es, "doc").unwrap());
    let v1 = sess.checkout(f.g, c1).unwrap();
    let idx = idx_of(&f);
    embed_pending(&idx, &v1, &toy(), &spec()).unwrap();
    // 명시 타깃만 청크로 (본문 text는 임베딩 안 됨)
    assert_eq!(idx.parts(n_doc).unwrap(), vec!["t0", "t1"]);
    let hits = idx
        .search(&v1, &toy().embed("환불 절차").unwrap(), 1)
        .unwrap();
    assert_eq!(hits[0].node, n_doc);
    assert_eq!(hits[0].part, "t1");
    // 타깃 외 prop(text) 변경 → 타깃 불변 → 재임베딩 없음 (node_hash만 조용히 갱신)
    let es2 = sess.edit(f.g, "main").unwrap();
    es2.set_prop(n_doc, "text", "본문이 완전히 바뀜".into())
        .unwrap();
    let c2 = committed(sess.commit(&es2, "c2").unwrap());
    let v2 = sess.checkout(f.g, c2).unwrap();
    assert_eq!(idx.pending(&v2, &spec()).unwrap(), vec![]);
    // 갱신된 node_hash 덕에 새 epoch에서 즉시 검색 성립
    let hits2 = idx
        .search(&v2, &toy().embed("카드 결제").unwrap(), 1)
        .unwrap();
    assert_eq!(hits2[0].node, n_doc);
    assert_eq!(hits2[0].part, "t0");
}

/// V13 — 폴백 토글: 타깃 없는 노드는 fallback=off면 임베딩 대상 아님. 빈 배열 = 없음.
#[test]
fn v13_fallback_toggle() {
    let f = fv();
    let sess = f.engine.session("ceo");
    // 빈 배열 타깃 노드 추가 — "없음"으로 취급되어야 함
    let es = sess.edit(f.g, "main").unwrap();
    es.add_node(
        &["Doc"],
        Some("doc-e"),
        props([
            ("text", "폴백으로만 임베딩될 텍스트".into()),
            ("embed_target", Value::List(vec![])),
        ]),
    )
    .unwrap();
    let c1 = committed(sess.commit(&es, "doc-e").unwrap());
    let v1 = sess.checkout(f.g, c1).unwrap();
    let idx = idx_of(&f);
    let no_fb = EmbedSpec {
        fallback: false,
        ..Default::default()
    };
    // 폴백 off: 명시 타깃 있는 노드가 없으므로 임베딩 대상 0
    assert_eq!(idx.pending(&v1, &no_fb).unwrap(), vec![]);
    // 폴백 on: 전 노드(빈 배열 포함) 대상
    assert_eq!(idx.pending(&v1, &spec()).unwrap().len(), 4);
}

/// Window 청커 경계 (V10 보조): 겹침 슬라이딩 + 진행 보장.
#[test]
fn window_chunker_boundaries() {
    let f = fv();
    let sess = f.engine.session("ceo");
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    let sp = EmbedSpec {
        fields: Some(vec!["text".into()]),
        chunker: Chunker::Window {
            size: 5,
            overlap: 2,
        },
        ..Default::default()
    };
    let chunks = extract_chunks(&v0, f.n1, &sp); // "이메일 로그인 인증" = 10 chars
    assert!(chunks.len() >= 2);
    assert!(chunks
        .iter()
        .enumerate()
        .all(|(i, (p, _))| p == &format!("w{i}")));
    // 재결합 시 원문 포함 (겹침 감안 — 마지막 청크 끝 = 원문 끝)
    assert!(chunks.last().unwrap().1.ends_with("인증"));
}
