//! 골든 픽스처 F0 (골든 TC §1.1) — TestIdGen 할당 순서까지 문서와 동일.
//! store/engine 테스트가 공유하므로 라이브러리 모듈로 둔다 (프로덕션 경로 아님).

use crate::value::props;
use crate::*;

pub struct F0 {
    pub state: GraphState,
    pub delta: Delta,
    pub commit: Commit,
    pub c0: Hash256,
    /// n1..n5 = ulid(3)..ulid(7)
    pub n: [Uuid; 5],
    /// e1..e4 = ulid(8)..ulid(11)
    pub e: [Uuid; 4],
    pub idg: TestIdGen,
    pub clock: TestClock,
}

pub fn build_f0() -> F0 {
    let idg = TestIdGen::new();
    let clock = TestClock::default();
    let _ga = idg.next(); // GA = ulid(1) — create_graph 소비
    let _wf = idg.next(); // WF = ulid(2) — edit 세션 소비

    let base = GraphState::new();
    let mut b = DeltaBuilder::new(&base);

    let n1 = idg.next_uuid();
    b.work
        .insert_node(Node::new(
            n1,
            &["Capability"],
            Some("cap-auth"),
            props([
                ("name", "회원 인증".into()),
                ("status", "active".into()),
                ("priority", 1.into()),
            ]),
        ))
        .unwrap();
    let n2 = idg.next_uuid();
    b.work
        .insert_node(Node::new(
            n2,
            &["Capability"],
            Some("cap-loan"),
            props([
                ("name", "도서 대출".into()),
                ("status", "active".into()),
                ("priority", 2.into()),
            ]),
        ))
        .unwrap();
    let n3 = idg.next_uuid();
    b.work
        .insert_node(Node::new(
            n3,
            &["Requirement"],
            Some("req-001"),
            props([("text", "이메일 로그인".into()), ("status", "draft".into())]),
        ))
        .unwrap();
    let n4 = idg.next_uuid();
    b.work
        .insert_node(Node::new(
            n4,
            &["Requirement"],
            Some("req-002"),
            props([
                ("text", "대출 한도 5권".into()),
                ("status", "active".into()),
            ]),
        ))
        .unwrap();
    let n5 = idg.next_uuid();
    b.work
        .insert_node(Node::new(
            n5,
            &["Policy"],
            Some("pol-perf"),
            props([("name", "응답 200ms".into())]),
        ))
        .unwrap();

    let e1 = idg.next_uuid();
    b.work
        .insert_edge(Edge::new(e1, "parent", n3, n1, Props::new()))
        .unwrap();
    let e2 = idg.next_uuid();
    b.work
        .insert_edge(Edge::new(e2, "parent", n4, n2, Props::new()))
        .unwrap();
    let e3 = idg.next_uuid();
    b.work
        .insert_edge(Edge::new(
            e3,
            "impacts",
            n2,
            n1,
            props([("weight", 0.7.into())]),
        ))
        .unwrap();
    let e4 = idg.next_uuid();
    b.work
        .insert_edge(Edge::new(e4, "policy", n4, n5, Props::new()))
        .unwrap();

    let delta = b.build();
    let mut state = GraphState::new();
    apply(&mut state, &delta).unwrap();

    let commit = Commit {
        parents: vec![],
        delta: delta.hash(),
        schema_rev: None,
        author: "ceo".into(),
        ts: clock.now(), // t0+1s
        msg: "F0".into(),
    };
    let c0 = commit.hash();

    F0 {
        state,
        delta,
        commit,
        c0,
        n: [n1, n2, n3, n4, n5],
        e: [e1, e2, e3, e4],
        idg,
        clock,
    }
}
