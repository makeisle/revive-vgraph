//! P3 게이트: C(편집), D(커밋), E(브랜치 기본), J(이벤트), K(동시성·재구축).

mod util;

use std::time::Duration;
use util::*;
use vgraph_core::value::props;
use vgraph_core::*;
use vgraph_engine::*;

fn committed(o: CommitOutcome) -> Hash256 {
    match o {
        CommitOutcome::Committed { commit, .. } => commit,
        other => panic!("expected Committed, got {other:?}"),
    }
}

// ---------- C. 편집 ----------

#[test]
fn c1_cow_base_untouched() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[2], "status", "active".into()).unwrap();
    assert_eq!(
        es.node_prop(f.n[2], "status"),
        Some(Value::Str("active".into()))
    );
    let v0 = sess.checkout(f.g, f.c0).unwrap();
    assert_eq!(
        v0.node_prop(f.n[2], "status"),
        Some(Value::Str("draft".into()))
    );
}

#[test]
fn c2_session_reuses_working_copy() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es1 = sess.edit(f.g, "main").unwrap();
    es1.set_prop(f.n[0], "priority", 9.into()).unwrap();
    let es2 = sess.edit(f.g, "main").unwrap();
    assert_eq!(es1.wid(), es2.wid()); // 동일 W 재사용
    assert!(es2.dirty()); // 이전 편집이 보임
}

#[test]
fn c3_pending_preview() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    let n6 = es
        .add_node(
            &["Requirement"],
            Some("req-003"),
            props([("text", "알림".into())]),
        )
        .unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    es.remove_edge(f.e[2]).unwrap();
    let d = es.pending();
    assert_eq!(d.nodes_added.len(), 1);
    assert_eq!(d.nodes_added[0].uuid, n6);
    assert_eq!(
        d.props_set,
        vec![PropSet {
            elem: f.n[0],
            key: "priority".into(),
            old: Some(Value::Int(1)),
            new: Some(Value::Int(9)),
        }]
    );
    assert_eq!(d.edges_removed.len(), 1);
    assert_eq!(d.edges_removed[0].uuid, f.e[2]);
}

#[test]
fn c4_dangling_policy() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    // Reject: e4가 걸려 있음 → 오류, W 무변화
    let es = sess.edit(f.g, "main").unwrap();
    match es.remove_node(f.n[4], false) {
        Err(EngineError::State(StateError::DanglingEdge(e))) => assert_eq!(e, f.e[3]),
        other => panic!("{other:?}"),
    }
    assert!(es.pending().is_empty());
    // Cascade: n5 + e4 함께 제거
    es.remove_node(f.n[4], true).unwrap();
    let d = es.pending();
    assert_eq!(d.nodes_removed.len(), 1);
    assert_eq!(d.edges_removed.len(), 1);
}

#[test]
fn c5_wal_replay_after_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    let (g, wid, expected);
    {
        let engine = engine_at(dir.path(), true, 1);
        let sess = engine.session("ceo");
        let f = build_f0_engine(&sess);
        let es = sess.edit(f.g, "main").unwrap();
        es.set_prop(f.n[0], "priority", 9.into()).unwrap();
        es.set_prop(f.n[2], "status", "active".into()).unwrap();
        es.remove_edge(f.e[2]).unwrap();
        g = f.g;
        wid = es.wid();
        expected = es.pending();
        // 크래시: cleanup 없이 엔진 소멸
    }
    let engine2 = engine_at(dir.path(), true, 1000);
    let sess2 = engine2.session("ceo");
    assert_eq!(sess2.list_working(g).unwrap(), vec![wid]);
    let es2 = sess2.resume(g, wid).unwrap();
    assert_eq!(es2.pending(), expected); // WAL replay = 편집 3건 복원
}

#[test]
fn c6_discard() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    sess.discard(es).unwrap();
    assert_eq!(sess.list_working(f.g).unwrap(), vec![]);
    let v = sess.checkout(f.g, f.c0).unwrap();
    assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(1)));
}

#[test]
fn c7_read_through_overlay() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    let e5 = es
        .add_edge("related", f.n[0], f.n[1], Props::new())
        .unwrap();
    assert_eq!(es.base(), Some(f.c0)); // epoch이 아니라 base+pending
    assert_eq!(
        es.neighbors(f.n[0], Dir::Out, None, None, 1, 1, None),
        vec![Hit {
            node: f.n[1],
            via: e5,
            depth: 1
        }]
    );
}

#[test]
fn c8_edit_errors_atomic() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    let ghost: Uuid = "00000000000000000000000099".parse().unwrap();
    assert!(matches!(
        es.set_prop(ghost, "x", 1.into()),
        Err(EngineError::State(StateError::UnknownElem(_)))
    ));
    assert!(matches!(
        es.add_node(&["X"], Some("cap-auth"), Props::new()),
        Err(EngineError::State(StateError::SlugTaken(_)))
    ));
    assert!(matches!(
        es.add_edge("t", f.n[0], ghost, Props::new()),
        Err(EngineError::State(StateError::UnknownElem(_)))
    ));
    assert!(es.pending().is_empty()); // 실패 op는 흔적 없음
}

// ---------- D. 커밋 ----------

#[test]
fn d1_commit_advances_and_promotes() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    let c1 = committed(sess.commit(&es, "d1").unwrap());
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, c1)
    );
    assert!(sess.test_cache_contains(f.g, c1)); // W→synced 승급 (재fold 없이 캐시됨)
    let v = sess.checkout(f.g, c1).unwrap();
    assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(9)));
}

#[test]
fn d5_empty_commit_rejected() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    assert!(matches!(
        sess.commit(&es, "noop").unwrap(),
        CommitOutcome::Empty
    ));
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, f.c0)
    );
}

#[test]
fn d6_concurrent_disjoint_auto_rebase() {
    let env = new_env(true);
    let s_a = env.engine.session("ceo");
    let s_b = env.engine.session("ceo");
    let f = build_f0_engine(&s_a);
    let w1 = s_a.edit(f.g, "main").unwrap();
    let w2 = s_b.edit(f.g, "main").unwrap();
    w1.set_prop(f.n[0], "priority", 9.into()).unwrap();
    w2.set_prop(f.n[2], "text", "이메일+SSO 로그인".into())
        .unwrap();
    let c1 = committed(s_a.commit(&w1, "w1").unwrap());
    match s_b.commit(&w2, "w2").unwrap() {
        CommitOutcome::Rebased { onto, committed } => {
            assert_eq!(onto, c1);
            let v = s_b.checkout(f.g, committed).unwrap();
            assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(9)));
            assert_eq!(
                v.node_prop(f.n[2], "text"),
                Some(Value::Str("이메일+SSO 로그인".into()))
            );
        }
        other => panic!("expected Rebased, got {other:?}"),
    }
}

#[test]
fn d7_concurrent_overlap_conflict_report() {
    let env = new_env(true);
    let s_a = env.engine.session("ceo");
    let s_b = env.engine.session("ceo");
    let f = build_f0_engine(&s_a);
    let w1 = s_a.edit(f.g, "main").unwrap();
    let w2 = s_b.edit(f.g, "main").unwrap();
    w1.set_prop(f.n[0], "status", "deprecated".into()).unwrap();
    w2.set_prop(f.n[0], "status", "beta".into()).unwrap();
    let wid2 = w2.wid();
    let c1 = committed(s_a.commit(&w1, "w1").unwrap());
    match s_b.commit(&w2, "w2").unwrap() {
        CommitOutcome::Conflicts(report) => {
            assert_eq!(report.base, Some(f.c0));
            assert_eq!(report.ours, SideRef::Working(wid2));
            assert_eq!(report.theirs, SideRef::Commit(c1));
            assert_eq!(
                report.items,
                vec![ConflictItem::PropBothSet {
                    elem: f.n[0],
                    key: "status".into(),
                    base: Some(Value::Str("active".into())),
                    ours: Some(Value::Str("beta".into())),
                    theirs: Some(Value::Str("deprecated".into())),
                }]
            );
        }
        other => panic!("expected Conflicts, got {other:?}"),
    }
    // head 불변 (충돌은 아무것도 바꾸지 않는다)
    assert_eq!(sess_head(&s_a), c1);

    fn sess_head(s: &Session) -> Hash256 {
        s.resolve("graph://acme/bookclub@main").unwrap().1
    }
}

#[test]
fn d9_detached_edit_gate() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    let c1 = committed(sess.commit(&es, "c1").unwrap());
    // D9a: 과거 commit 편집 → 경고 게이트
    match sess.edit_opts(f.g, "main", Some(f.c0), false) {
        Err(EditStartError::NotHead { head, hint }) => {
            assert_eq!(head, c1);
            assert_eq!(hint, EditHint::BranchRecommended);
        }
        other => panic!("{other:?}"),
    }
    // D9b: 강행 → detached commit, 어느 head도 안 움직임
    let es = sess.edit_opts(f.g, "main", Some(f.c0), true).unwrap();
    es.set_prop(f.n[4], "name", "응답 150ms".into()).unwrap();
    let cd = committed(sess.commit(&es, "detached").unwrap());
    assert_ne!(cd, c1);
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, c1)
    );
    assert_eq!(sess.log(f.g, "main").unwrap(), vec![c1, f.c0]); // cd 미포함
}

#[test]
fn d10_commit_emits_single_event() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let rx = sess.subscribe(f.g, None).unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    let c1 = committed(sess.commit(&es, "c1").unwrap());
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        GraphEvent::HeadAdvanced {
            branch: "main".into(),
            from: Some(f.c0),
            to: c1
        }
    );
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err()); // 정확히 1회
}

// ---------- E. 브랜치 기본 ----------

#[test]
fn e_branch_suite() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    // E1: O(1) 생성 — blob·payload 증가 0
    let s0 = sess.test_stats(f.g).unwrap();
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let s1 = sess.test_stats(f.g).unwrap();
    assert_eq!(s0, s1);
    assert_eq!(
        sess.resolve("graph://acme/bookclub@dev-b").unwrap(),
        (f.g, f.c0)
    );
    // E3: 즉시 체크아웃 (lazy 구축)
    let core = vgraph_core::fixtures::build_f0();
    assert_eq!(
        sess.checkout(f.g, f.c0).unwrap().dump(),
        core.state.dump(Some(&core.c0))
    );
    // E4: 중복 이름
    assert!(matches!(
        sess.branch(f.g, f.c0, "dev-b"),
        Err(EngineError::BranchExists)
    ));
    // E2 + E5: 격리 + payload 공유 (n1 수정 → 새 payload row 1건만)
    let es = sess.edit(f.g, "dev-b").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    let d1 = committed(sess.commit(&es, "d1").unwrap());
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, f.c0)
    );
    assert_eq!(
        sess.resolve("graph://acme/bookclub@dev-b").unwrap(),
        (f.g, d1)
    );
    let s2 = sess.test_stats(f.g).unwrap();
    assert_eq!(s2.payload_rows, s1.payload_rows + 1);
}

// ---------- J. 이벤트 ----------

#[test]
fn j3_j4_event_order_and_filter() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let rx_all = sess.subscribe(f.g, None).unwrap();
    let rx_dev = sess.subscribe(f.g, Some(&["dev-b"])).unwrap();

    let mut heads = vec![f.c0];
    for i in 0..3 {
        let es = sess.edit(f.g, "main").unwrap();
        es.set_prop(f.n[0], "priority", (10 + i).into()).unwrap();
        heads.push(committed(sess.commit(&es, "c").unwrap()));
    }
    // J3: 브랜치별 순서 보장
    for i in 0..3 {
        assert_eq!(
            rx_all.recv_timeout(Duration::from_secs(1)).unwrap(),
            GraphEvent::HeadAdvanced {
                branch: "main".into(),
                from: Some(heads[i]),
                to: heads[i + 1],
            }
        );
    }
    // J4: 필터 — main 커밋은 dev-b 구독자에 미도달
    assert!(rx_dev.recv_timeout(Duration::from_millis(200)).is_err());
}

// ---------- K. 동시성 · 재구축 ----------

#[test]
fn k1_concurrent_commits_serialize_without_loss() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let mut handles = Vec::new();
    for i in 0..8 {
        let engine = env.engine.clone();
        let g = f.g;
        handles.push(std::thread::spawn(move || {
            let s = engine.session("ceo");
            let es = s.edit(g, "main").unwrap();
            es.add_node(
                &["Capability"],
                Some(&format!("cap-p{i}")),
                props([("status", "active".into())]),
            )
            .unwrap();
            match s.commit(&es, "k1").unwrap() {
                CommitOutcome::Committed { .. } | CommitOutcome::Rebased { .. } => (),
                other => panic!("thread {i}: {other:?}"),
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(sess.log(f.g, "main").unwrap().len(), 9); // C0 + 8, 유실 0
    let head = sess.resolve("graph://acme/bookclub@main").unwrap().1;
    let v = sess.checkout(f.g, head).unwrap();
    assert_eq!(v.matches(&["Capability"], &Props::new()).len(), 10); // 기존 2 + 신규 8
}

#[test]
fn k2_reader_never_sees_torn_state() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let writer = {
        let engine = env.engine.clone();
        let g = f.g;
        let n1 = f.n[0];
        std::thread::spawn(move || {
            let s = engine.session("ceo");
            for k in 0..20 {
                let es = s.edit(g, "main").unwrap();
                es.set_prop(n1, "priority", (100 + k).into()).unwrap();
                s.commit(&es, "w").unwrap();
            }
        })
    };
    let reader = {
        let engine = env.engine.clone();
        let g = f.g;
        std::thread::spawn(move || {
            let s = engine.session("ceo");
            for _ in 0..30 {
                let (_, c) = s.resolve("graph://acme/bookclub@main").unwrap();
                let v = s.checkout(g, c).unwrap();
                // 뷰 dump ≡ 캐시 우회 재-materialize dump (fold ≡ 캐시, torn read 없음)
                assert_eq!(v.dump(), s.test_state_dump(g, c).unwrap());
            }
        })
    };
    writer.join().unwrap();
    reader.join().unwrap();
}

#[test]
fn k4_derived_drop_then_rebuild() {
    let dir = tempfile::TempDir::new().unwrap();
    let (g, c1, n1);
    {
        let engine = engine_at(dir.path(), true, 1);
        let sess = engine.session("ceo");
        let f = build_f0_engine(&sess);
        let es = sess.edit(f.g, "main").unwrap();
        es.set_prop(f.n[0], "priority", 9.into()).unwrap();
        c1 = committed(sess.commit(&es, "c1").unwrap());
        sess.test_drop_derived(f.g).unwrap(); // 파생 계층 전파괴
        g = f.g;
        n1 = f.n[0];
    }
    let engine2 = engine_at(dir.path(), true, 1000);
    let sess2 = engine2.session("ceo");
    let v = sess2.checkout(g, c1).unwrap(); // log replay 재구축 (root fold)
    assert_eq!(v.node_prop(n1, "priority"), Some(Value::Int(9)));
    assert_eq!(v.dump(), sess2.test_state_dump(g, c1).unwrap());
}
