//! 엔진 골든 테스트 공용 유틸 — F0을 공개 창구(Session)로만 구축.
//! TestIdGen 소비 순서 = 골든 §1.1 (create_graph=1, edit 세션=2, n1..=3..).

use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use vgraph_core::value::props;
use vgraph_core::*;
use vgraph_engine::*;

pub struct Env {
    pub _dir: TempDir,
    pub engine: Engine,
}

pub fn new_env(auto_upsert: bool) -> Env {
    let dir = TempDir::new().unwrap();
    let engine = engine_at(dir.path(), auto_upsert, 1);
    Env { _dir: dir, engine }
}

pub fn engine_at(root: &Path, auto_upsert: bool, idgen_start: u64) -> Engine {
    Engine::open(
        Box::new(FsProvider::new(root).unwrap()),
        Arc::new(TestIdGen::starting_at(idgen_start)),
        Arc::new(TestClock::default()),
        EngineOptions {
            auto_upsert_head: auto_upsert,
        },
    )
    .unwrap()
}

#[allow(dead_code)] // 테스트 바이너리별로 일부 필드만 사용
pub struct EngineF0 {
    pub g: GraphId,
    pub n: [Uuid; 5],
    pub e: [Uuid; 4],
    pub c0: Hash256,
}

/// 공개 창구로 F0 구축 + core 골든 lock의 C0과 일치 검증 (표현 경로 간 항등).
pub fn build_f0_engine(sess: &Session) -> EngineF0 {
    let g = sess.create_graph("acme", "bookclub").unwrap();
    let es = sess.edit(g, "main").unwrap();
    let n1 = es
        .add_node(
            &["Capability"],
            Some("cap-auth"),
            props([
                ("name", "회원 인증".into()),
                ("status", "active".into()),
                ("priority", 1.into()),
            ]),
        )
        .unwrap();
    let n2 = es
        .add_node(
            &["Capability"],
            Some("cap-loan"),
            props([
                ("name", "도서 대출".into()),
                ("status", "active".into()),
                ("priority", 2.into()),
            ]),
        )
        .unwrap();
    let n3 = es
        .add_node(
            &["Requirement"],
            Some("req-001"),
            props([("text", "이메일 로그인".into()), ("status", "draft".into())]),
        )
        .unwrap();
    let n4 = es
        .add_node(
            &["Requirement"],
            Some("req-002"),
            props([
                ("text", "대출 한도 5권".into()),
                ("status", "active".into()),
            ]),
        )
        .unwrap();
    let n5 = es
        .add_node(
            &["Policy"],
            Some("pol-perf"),
            props([("name", "응답 200ms".into())]),
        )
        .unwrap();
    let e1 = es.add_edge("parent", n3, n1, Props::new()).unwrap();
    let e2 = es.add_edge("parent", n4, n2, Props::new()).unwrap();
    let e3 = es
        .add_edge("impacts", n2, n1, props([("weight", 0.7.into())]))
        .unwrap();
    let e4 = es.add_edge("policy", n4, n5, Props::new()).unwrap();

    let c0 = match sess.commit(&es, "F0").unwrap() {
        CommitOutcome::Committed { commit, .. } => commit,
        other => panic!("F0 commit failed: {other:?}"),
    };

    // 엔진 경로 C0 ≡ core 픽스처 C0 (골든 lock과 동일) — canonical 항등의 교차 검증
    assert_eq!(
        c0,
        vgraph_core::fixtures::build_f0().c0,
        "engine C0 != core C0"
    );

    EngineF0 {
        g,
        n: [n1, n2, n3, n4, n5],
        e: [e1, e2, e3, e4],
        c0,
    }
}
