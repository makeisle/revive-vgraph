//! P2 게이트: A(주소해석), B(체크아웃·뷰·캐시), G(서치·순회) + View↔오라클 differential.

mod util;

use util::*;
use vgraph_core::value::props;
use vgraph_core::*;
use vgraph_engine::*;

// ---------- A. 주소 해석 ----------

#[test]
fn a_resolve_suite() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);

    // A1·A2
    assert_eq!(sess.resolve("graph://acme/bookclub").unwrap(), (f.g, f.c0));
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, f.c0)
    );
    // A4
    let v = sess.checkout(f.g, f.c0).unwrap();
    assert_eq!(v.by_slug("cap-auth"), Some(f.n[0]));
    // A5
    assert_eq!(
        sess.resolve("graph://acme/nope").unwrap_err(),
        ResolveError::UnknownGraph
    );
    assert_eq!(
        sess.resolve("graph://acme/bookclub@nope").unwrap_err(),
        ResolveError::UnknownBranch
    );
    let missing = "f".repeat(64);
    assert_eq!(
        sess.resolve(&format!("graph://acme/bookclub@main.{missing}"))
            .unwrap_err(),
        ResolveError::UnknownCommit
    );
    // A6 — 존재 비노출
    let guest = env.engine.session("guest");
    assert_eq!(
        guest.resolve("graph://acme/bookclub").unwrap_err(),
        ResolveError::NoPermission
    );
    // A3·A7 — 불변 핀 vs 가변 별칭, 별칭 1회 해석
    let (_, c_before) = sess.resolve("graph://acme/bookclub@main").unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    let c1 = match sess.commit(&es, "c1").unwrap() {
        CommitOutcome::Committed { commit, .. } => commit,
        o => panic!("{o:?}"),
    };
    assert_eq!(
        sess.resolve(&format!("graph://acme/bookclub@main.{}", f.c0))
            .unwrap(),
        (f.g, f.c0)
    );
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, c1)
    );
    // A7: 앞서 해석한 hash로의 checkout은 여전히 C0
    let v0 = sess.checkout(f.g, c_before).unwrap();
    assert_eq!(v0.epoch(), f.c0);
}

// ---------- B. 체크아웃 · 뷰 · 캐시 ----------

#[test]
fn b1_cold_checkout_matches_golden_dump() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.test_evict_all(f.g).unwrap();
    let v = sess.checkout(f.g, f.c0).unwrap();
    let core = vgraph_core::fixtures::build_f0();
    assert_eq!(v.dump(), core.state.dump(Some(&core.c0))); // ≡ F0_DUMP lock
}

#[test]
fn b2_view_shared_arc() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let v1 = sess.checkout(f.g, f.c0).unwrap();
    let v2 = sess.checkout(f.g, f.c0).unwrap();
    assert!(std::sync::Arc::ptr_eq(&v1, &v2));
}

#[test]
fn b3_epoch_immutable_under_head_advance() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let v = sess.checkout(f.g, f.c0).unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    sess.commit(&es, "c1").unwrap();
    assert_eq!(v.epoch(), f.c0);
    assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(1)));
}

#[test]
fn b4_watermark_lag_applies_residual_deltas() {
    let env = new_env(false); // head 표현 자동 upsert OFF
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.test_flush_head(f.g, "main").unwrap(); // rows·워터마크 = C0에 고정
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    sess.commit(&es, "c1").unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[2], "status", "active".into()).unwrap();
    let c2 = match sess.commit(&es, "c2").unwrap() {
        CommitOutcome::Committed { commit, .. } => commit,
        o => panic!("{o:?}"),
    };
    sess.test_evict_all(f.g).unwrap();
    // rows(C0) + 잔여 delta(c1, c2) 적용 경로
    let v = sess.checkout(f.g, c2).unwrap();
    assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(9)));
    assert_eq!(
        v.node_prop(f.n[2], "status"),
        Some(Value::Str("active".into()))
    );
}

#[test]
fn b5_time_travel_backward() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    let c1 = match sess.commit(&es, "c1").unwrap() {
        CommitOutcome::Committed { commit, .. } => commit,
        o => panic!("{o:?}"),
    };
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[2], "status", "active".into()).unwrap();
    sess.commit(&es, "c2").unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.remove_edge(f.e[2]).unwrap();
    sess.commit(&es, "c3").unwrap();

    let v = sess.at(f.g, c1).unwrap();
    assert!(v.has_edge(f.e[2])); // e3 아직 존재
    assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(9)));
    assert_eq!(
        v.node_prop(f.n[2], "status"),
        Some(Value::Str("draft".into()))
    );
}

#[test]
fn b6_evict_and_rebuild_identical() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let v1 = sess.checkout(f.g, f.c0).unwrap();
    let d = v1.dump();
    drop(v1);
    sess.test_evict_all(f.g).unwrap();
    assert!(!sess.test_cache_contains(f.g, f.c0));
    let v2 = sess.checkout(f.g, f.c0).unwrap();
    assert_eq!(v2.dump(), d);
}

#[test]
fn b7_lazy_props_single_fault() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.test_evict_all(f.g).unwrap();
    let v = sess.checkout(f.g, f.c0).unwrap(); // 워터마크==C0 → rows(lazy) 경로
    let io0 = sess.test_io_reads(f.g);
    assert_eq!(
        v.node_prop(f.n[0], "name"),
        Some(Value::Str("회원 인증".into()))
    );
    assert_eq!(sess.test_io_reads(f.g), io0 + 1); // 최초 접근 = D1 폴트 1회
    assert_eq!(
        v.node_prop(f.n[0], "name"),
        Some(Value::Str("회원 인증".into()))
    );
    assert_eq!(sess.test_io_reads(f.g), io0 + 1); // 2회차 = 캐시
}

#[test]
fn b8_pinned_view_survives_evict() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let _v = sess.checkout(f.g, f.c0).unwrap(); // 세션이 Arc 보유 = 핀
    sess.test_evict_all(f.g).unwrap();
    assert!(sess.test_cache_contains(f.g, f.c0));
}

#[test]
fn b9_explicit_update_both_views_valid() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let v1 = sess.checkout(f.g, f.c0).unwrap();
    let rx = sess.subscribe(f.g, None).unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    let c1 = match sess.commit(&es, "c1").unwrap() {
        CommitOutcome::Committed { commit, .. } => commit,
        o => panic!("{o:?}"),
    };
    let ev = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
    assert_eq!(
        ev,
        GraphEvent::HeadAdvanced {
            branch: "main".into(),
            from: Some(f.c0),
            to: c1
        }
    );
    // 명시적 재해석으로만 최신화 — 기존 뷰도 계속 유효
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, c1)
    );
    let v2 = sess.checkout(f.g, c1).unwrap();
    assert_eq!(v2.epoch(), c1);
    assert_eq!(v1.epoch(), f.c0);
}

// ---------- G. 서치 · 순회 ----------

#[test]
fn g_traversal_suite() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let [n1, n2, n3, n4, n5] = f.n;
    let [e1, e2, e3, e4] = f.e;
    let v = sess.checkout(f.g, f.c0).unwrap();

    // G1
    assert_eq!(
        v.neighbors(n1, Dir::In, Some(&["parent"]), None, 1, 1, None),
        vec![Hit {
            node: n3,
            via: e1,
            depth: 1
        }]
    );
    // G2
    assert_eq!(
        v.neighbors(n4, Dir::Out, None, None, 1, 2, None),
        vec![
            Hit {
                node: n2,
                via: e2,
                depth: 1
            },
            Hit {
                node: n5,
                via: e4,
                depth: 1
            },
            Hit {
                node: n1,
                via: e3,
                depth: 2
            },
        ]
    );
    // G3
    assert_eq!(
        v.neighbors(n4, Dir::Out, None, None, 1, 2, Some(2))
            .iter()
            .map(|h| h.node)
            .collect::<Vec<_>>(),
        vec![n2, n5]
    );
    // G4 — 필터는 방출만 거른다 (n5 제외, n1은 depth2로 여전히 도달)
    let active = Value::Str("active".into());
    assert_eq!(
        v.neighbors(n4, Dir::Out, None, Some(("status", &active)), 1, 2, None),
        vec![
            Hit {
                node: n2,
                via: e2,
                depth: 1
            },
            Hit {
                node: n1,
                via: e3,
                depth: 2
            }
        ]
    );
    // G5
    assert_eq!(
        v.matches(&["Capability"], &props([("status", "active".into())])),
        vec![n1, n2]
    );
    // G9
    assert_eq!(v.by_slug("cap-auth"), Some(n1));
    assert_eq!(v.by_slug("nope"), None);
}

#[test]
fn g6_find_by_prop_epoch_join_branch_isolation() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    // dev-b에만 n6(status=active) 추가
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let es = sess.edit(f.g, "dev-b").unwrap();
    es.add_node(
        &["Requirement"],
        Some("req-x"),
        props([("status", "active".into())]),
    )
    .unwrap();
    sess.commit(&es, "d1").unwrap();
    // main 뷰 검색: n6의 payload는 소스 계층에 존재하지만 epoch 조인으로 배제
    let v = sess.checkout(f.g, f.c0).unwrap();
    assert_eq!(
        v.find_by_prop("status", &Value::Str("active".into())),
        vec![f.n[0], f.n[1], f.n[3]]
    );
}

#[test]
fn g8_search_differs_across_epochs() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[2], "status", "active".into()).unwrap();
    let c1 = match sess.commit(&es, "c1").unwrap() {
        CommitOutcome::Committed { commit, .. } => commit,
        o => panic!("{o:?}"),
    };
    let active = Value::Str("active".into());
    assert_eq!(
        sess.at(f.g, f.c0)
            .unwrap()
            .find_by_prop("status", &active)
            .len(),
        3
    );
    assert_eq!(
        sess.at(f.g, c1)
            .unwrap()
            .find_by_prop("status", &active)
            .len(),
        4
    );
}

#[test]
fn g10_g12_multigraph_and_cycle() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let [n1, n2, ..] = f.n;
    let es = sess.edit(f.g, "main").unwrap();
    let e5 = es.add_edge("related", n1, n2, Props::new()).unwrap();
    let e6 = es.add_edge("related", n1, n2, Props::new()).unwrap();
    let c1 = match sess.commit(&es, "multi").unwrap() {
        CommitOutcome::Committed { commit, .. } => commit,
        o => panic!("{o:?}"),
    };
    let v = sess.checkout(f.g, c1).unwrap();
    // G10: 노드 1회, via = 최소 uuid 엣지 — 두 엣지 모두 실재
    assert_eq!(
        v.neighbors(n1, Dir::Out, Some(&["related"]), None, 1, 1, None),
        vec![Hit {
            node: n2,
            via: e5,
            depth: 1
        }]
    );
    assert!(v.has_edge(e5) && v.has_edge(e6));
    // G12: e3(n2→n1)과 사이클 — 종료·시작노드 제외
    assert_eq!(
        v.neighbors(n1, Dir::Out, None, None, 1, 3, None),
        vec![Hit {
            node: n2,
            via: e5,
            depth: 1
        }]
    );
}

// ---------- View ↔ GraphState 오라클 differential ----------

#[test]
fn view_differential_vs_oracle() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let core = vgraph_core::fixtures::build_f0();
    let v = sess.checkout(f.g, f.c0).unwrap();

    for start in f.n {
        for dir in [Dir::Out, Dir::In, Dir::Both] {
            for types in [None, Some(vec!["parent"]), Some(vec!["parent", "impacts"])] {
                for max in [1u32, 2, 3] {
                    let ts = types.as_deref();
                    assert_eq!(
                        v.neighbors(start, dir, ts, None, 1, max, None),
                        core.state.neighbors(start, dir, ts, None, 1, max, None),
                        "start={start} dir={dir:?} types={types:?} max={max}"
                    );
                }
            }
        }
    }
}

// ---------- H. 브랜치 이름 규칙 ("/" 허용) ----------

fn commit_of(sess: &Session, es: &EditSession, msg: &str) -> Hash256 {
    match sess.commit(es, msg).unwrap() {
        CommitOutcome::Committed { commit, .. } => commit,
        o => panic!("{o:?}"),
    }
}

/// [핵심] `feature/x` 를 만들고 주소로 조회·편집·커밋·로그·병합한다.
#[test]
fn h1_slash_branch_create_resolve_edit_commit_merge() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "feature/x").unwrap();
    assert_eq!(
        sess.resolve("graph://acme/bookclub@feature/x").unwrap(),
        (f.g, f.c0)
    );
    assert_eq!(
        sess.resolve(&format!("graph://acme/bookclub@feature/x.{}", f.c0))
            .unwrap(),
        (f.g, f.c0)
    );

    let es = sess.edit(f.g, "feature/x").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    let c1 = commit_of(&sess, &es, "x1");
    assert_eq!(
        sess.resolve("graph://acme/bookclub@feature/x").unwrap(),
        (f.g, c1)
    );
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, f.c0)
    );
    assert_eq!(sess.log(f.g, "feature/x").unwrap().first(), Some(&c1));

    // 병합: 이름 인자로 src="feature/x", dst="main"
    match sess
        .merge(f.g, "feature/x", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::FastForward(h) => assert_eq!(h, c1),
        o => panic!("{o:?}"),
    }
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, c1)
    );
    // 같은 이름으로 다시 만들면 기존 오류(BranchExists) 그대로
    assert!(matches!(
        sess.branch(f.g, f.c0, "feature/x").unwrap_err(),
        EngineError::BranchExists
    ));
}

/// [핵심] 규칙 위반 이름은 만들 때 거부되고, 아무 ref 도 생기지 않는다. 오류 문구는 주소 해석과 같다.
#[test]
fn h2_invalid_branch_names_rejected_on_create() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let rule = valid_branch_name_err("a//b");
    for bad in ["a//b", "/a", "a/", "a b", "a.b", "", "a@b"] {
        let e = sess.branch(f.g, f.c0, bad).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains(&valid_branch_name_err(bad)), "{bad:?}: {msg}");
        assert!(matches!(e, EngineError::BadBranchName(_)), "{bad:?}: {msg}");
        assert!(!msg.starts_with("bad address"), "{bad:?}: {msg}");
    }
    assert!(rule.contains("branch"), "{rule}");
    // 거부된 이름의 ref 는 없다
    for bad in ["a//b", "/a", "a/", "a b", "a.b"] {
        assert!(sess.log(f.g, bad).is_err(), "{bad:?} ref 가 생겼다");
    }
}

fn valid_branch_name_err(name: &str) -> String {
    vgraph_core::addr::valid_branch_name(name)
        .unwrap_err()
        .to_string()
}

/// [중요] 새 규칙 이전에 만들어진 규칙 밖 이름(공백)의 ref 가 있어도 목록·로그·병합(이름 인자)은 그대로 동작한다.
/// 시험에서는 저장소 계층(SqliteBackend)에 직접 ref 를 써 넣어 옛 저장소를 흉내 낸다.
#[test]
fn h3_legacy_out_of_rule_branch_still_works_by_name() {
    use vgraph_store::{CasOutcome, RefStore, SqliteBackend};
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "priority", 9.into()).unwrap();
    let c1 = commit_of(&sess, &es, "c1");

    let db = SqliteBackend::open(&env._dir.path().join(format!("{}.db", f.g))).unwrap();
    assert_eq!(
        db.cas_ref("old name", None, &f.c0).unwrap(),
        CasOutcome::Committed
    );

    // 이름 인자 경로: 로그
    assert_eq!(sess.log(f.g, "old name").unwrap(), vec![f.c0]);
    // 병합: 옛 이름이 dst 이고 main(c1)이 src → fast-forward
    match sess
        .merge(f.g, "main", "old name", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::FastForward(h) => assert_eq!(h, c1),
        o => panic!("{o:?}"),
    }
    assert_eq!(sess.log(f.g, "old name").unwrap().first(), Some(&c1));
    // 새로 만들기는 여전히 거부
    assert!(sess.branch(f.g, c1, "old name 2").is_err());
}
