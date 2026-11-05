//! P4 게이트: F(머지 — FF/3-way/충돌 매트릭스/해소/계보) + P6 게이트: I(external link).

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

fn set_and_commit(
    sess: &Session,
    g: GraphId,
    branch: &str,
    elem: Uuid,
    key: &str,
    v: Value,
) -> Hash256 {
    let es = sess.edit(g, branch).unwrap();
    es.set_prop(elem, key, v).unwrap();
    committed(sess.commit(&es, "c").unwrap())
}

// ================= F. 머지 =================

#[test]
fn f1_fast_forward() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    set_and_commit(&sess, f.g, "dev-b", f.n[0], "priority", 9.into());
    let d2 = set_and_commit(&sess, f.g, "dev-b", f.n[2], "status", "active".into());
    match sess
        .merge(f.g, "dev-b", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::FastForward(h) => assert_eq!(h, d2),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, d2)
    );
}

#[test]
fn f2_three_way_disjoint_merge_commit() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let c1 = set_and_commit(&sess, f.g, "main", f.n[3], "text", "대출 한도 7권".into());
    let d1 = set_and_commit(&sess, f.g, "dev-b", f.n[0], "priority", 9.into());
    let m1 = match sess
        .merge(f.g, "dev-b", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::Merged(h) => h,
        other => panic!("{other:?}"),
    };
    assert_eq!(sess.commit_meta(f.g, m1).unwrap().parents, vec![c1, d1]); // dst 먼저
    let v = sess.checkout(f.g, m1).unwrap();
    assert_eq!(
        v.node_prop(f.n[3], "text"),
        Some(Value::Str("대출 한도 7권".into()))
    );
    assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(9)));
}

#[test]
fn f3_prop_conflict_reports_only_overlap() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "status", "deprecated".into()).unwrap();
    es.set_prop(f.n[3], "text", "대출 한도 7권".into()).unwrap();
    let c1 = committed(sess.commit(&es, "c1").unwrap());
    let d1 = set_and_commit(&sess, f.g, "dev-b", f.n[0], "status", "beta".into());
    match sess
        .merge(f.g, "dev-b", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::Conflicts(report) => {
            assert_eq!(report.base, Some(f.c0));
            assert_eq!(report.ours, SideRef::Commit(c1));
            assert_eq!(report.theirs, SideRef::Commit(d1));
            assert_eq!(
                report.items,
                vec![ConflictItem::PropBothSet {
                    elem: f.n[0],
                    key: "status".into(),
                    base: Some(Value::Str("active".into())),
                    ours: Some(Value::Str("deprecated".into())),
                    theirs: Some(Value::Str("beta".into())),
                }]
            ); // n4 항목 없음 — 무충돌 변경은 report 미포함
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, c1)
    ); // head 불변
}

#[test]
fn f4_removed_vs_edit() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.remove_node(f.n[4], true).unwrap();
    sess.commit(&es, "rm").unwrap();
    set_and_commit(&sess, f.g, "dev-b", f.n[4], "name", "응답 100ms".into());
    match sess
        .merge(f.g, "dev-b", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::Conflicts(report) => assert_eq!(
            report.items,
            vec![ConflictItem::RemovedVsEdit {
                elem: f.n[4],
                removed_by: Side::Ours
            }]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn f5_dangling_edge() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.remove_node(f.n[4], true).unwrap();
    sess.commit(&es, "rm").unwrap();
    let es = sess.edit(f.g, "dev-b").unwrap();
    let e5 = es.add_edge("policy", f.n[2], f.n[4], Props::new()).unwrap();
    sess.commit(&es, "link").unwrap();
    match sess
        .merge(f.g, "dev-b", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::Conflicts(report) => assert_eq!(
            report.items,
            vec![ConflictItem::DanglingEdge {
                edge: e5,
                missing_end: f.n[4],
                removed_by: Side::Ours,
            }]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn f6_slug_collision() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    let na = es
        .add_node(&["Capability"], Some("cap-pay"), Props::new())
        .unwrap();
    sess.commit(&es, "a").unwrap();
    let es = sess.edit(f.g, "dev-b").unwrap();
    let nb = es
        .add_node(&["Capability"], Some("cap-pay"), Props::new())
        .unwrap();
    sess.commit(&es, "b").unwrap();
    match sess
        .merge(f.g, "dev-b", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::Conflicts(report) => assert_eq!(
            report.items,
            vec![ConflictItem::SlugCollision {
                slug: "cap-pay".into(),
                ours: na,
                theirs: nb
            }]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn f8_resolution_theirs() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let es = sess.edit(f.g, "main").unwrap();
    es.set_prop(f.n[0], "status", "deprecated".into()).unwrap();
    es.set_prop(f.n[3], "text", "대출 한도 7권".into()).unwrap();
    sess.commit(&es, "c1").unwrap();
    set_and_commit(&sess, f.g, "dev-b", f.n[0], "status", "beta".into());
    let opts = MergeOpts {
        resolutions: vec![Resolution {
            item: 0,
            choose: Choice::Theirs,
        }],
    };
    let m1 = match sess.merge(f.g, "dev-b", "main", &opts).unwrap() {
        MergeOutcome::Merged(h) => h,
        other => panic!("{other:?}"),
    };
    let v = sess.checkout(f.g, m1).unwrap();
    assert_eq!(
        v.node_prop(f.n[0], "status"),
        Some(Value::Str("beta".into()))
    );
    assert_eq!(
        v.node_prop(f.n[3], "text"),
        Some(Value::Str("대출 한도 7권".into()))
    );
}

#[test]
fn f9_same_value_convergence() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    set_and_commit(&sess, f.g, "main", f.n[0], "status", "beta".into());
    set_and_commit(&sess, f.g, "dev-b", f.n[0], "status", "beta".into());
    let m1 = match sess
        .merge(f.g, "dev-b", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::Merged(h) => h, // 동일 값 = 충돌 아님 (auto-resolve)
        other => panic!("{other:?}"),
    };
    let v = sess.checkout(f.g, m1).unwrap();
    assert_eq!(
        v.node_prop(f.n[0], "status"),
        Some(Value::Str("beta".into()))
    );
    assert_eq!(sess.commit_meta(f.g, m1).unwrap().parents.len(), 2);
}

#[test]
fn f10_common_ancestor_from_old_branch_point() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    set_and_commit(&sess, f.g, "main", f.n[0], "priority", 9.into());
    set_and_commit(&sess, f.g, "main", f.n[2], "status", "active".into());
    sess.branch(f.g, f.c0, "hotfix").unwrap(); // 과거 시점(C0)에서 분기
    set_and_commit(&sess, f.g, "hotfix", f.n[4], "name", "응답 150ms".into());
    let m1 = match sess
        .merge(f.g, "hotfix", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::Merged(h) => h, // ancestor = C0 기준 3-way
        other => panic!("{other:?}"),
    };
    let v = sess.checkout(f.g, m1).unwrap();
    assert_eq!(
        v.node_prop(f.n[4], "name"),
        Some(Value::Str("응답 150ms".into()))
    );
    assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(9)));
    assert_eq!(
        v.node_prop(f.n[2], "status"),
        Some(Value::Str("active".into()))
    );
}

#[test]
fn f11_f12_log_lineage_and_remerge() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    sess.branch(f.g, f.c0, "dev-b").unwrap();
    let c1 = set_and_commit(&sess, f.g, "main", f.n[3], "text", "대출 한도 7권".into());
    let d1 = set_and_commit(&sess, f.g, "dev-b", f.n[0], "priority", 9.into());
    let m1 = match sess
        .merge(f.g, "dev-b", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::Merged(h) => h,
        other => panic!("{other:?}"),
    };
    // F11: ts desc, merge 계보(D1) 포함
    assert_eq!(sess.log(f.g, "main").unwrap(), vec![m1, d1, c1, f.c0]);
    // F12: 재머지 = UpToDate, 중복 merge commit 없음
    match sess
        .merge(f.g, "dev-b", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::UpToDate(h) => assert_eq!(h, m1),
        other => panic!("{other:?}"),
    }
}

// ================= I. external link =================

struct Fl {
    f: EngineF0,
    gb: GraphId,
    m1: Uuid,
    b0: Hash256,
    l1: Hash256,
}

fn build_fl(sess: &Session) -> Fl {
    let f = build_f0_engine(sess);
    let gb = sess.create_graph("acme", "policy-kg").unwrap();
    let eb = sess.edit(gb, "main").unwrap();
    let m1 = eb
        .add_node(
            &["Policy"],
            Some("ext-sla"),
            props([("name", "외부 SLA".into()), ("rev", 1.into())]),
        )
        .unwrap();
    let b0 = committed(sess.commit(&eb, "B0").unwrap());
    let el = sess.edit(f.g, "main").unwrap();
    el.set_prop(
        f.n[4],
        "extern",
        Value::Ref("graph://acme/policy-kg#ext-sla".into()),
    )
    .unwrap();
    let l1 = committed(sess.commit(&el, "link").unwrap());
    Fl { f, gb, m1, b0, l1 }
}

#[test]
fn i1_lockfile_pin_registered() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let fl = build_fl(&sess);
    let lock = sess.lockfile(fl.f.g, "main").unwrap();
    assert_eq!(lock.len(), 1);
    assert_eq!(
        lock["acme/policy-kg"],
        LockEntry {
            graph: fl.gb,
            branch: "main".into(),
            pin: fl.b0
        }
    );
    assert_eq!(sess.drifts(fl.f.g, "main").unwrap(), vec![]);
}

#[test]
fn i2_drift_detected_on_target_advance() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let fl = build_fl(&sess);
    let rx = sess.subscribe(fl.f.g, None).unwrap();
    // GB 전진: m1.rev = 2
    set_and_commit(&sess, fl.gb, "main", fl.m1, "rev", 2.into());
    match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        GraphEvent::Drift { item } => {
            assert_eq!(item.target, "acme/policy-kg");
            assert_eq!(item.node, fl.m1);
            assert_eq!(item.pin, fl.b0);
            assert_ne!(item.pinned_hash, item.current_hash);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(sess.drifts(fl.f.g, "main").unwrap().len(), 1);
    // GA 자체는 무변화 — 링크는 깨지지 않는다
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (fl.f.g, fl.l1)
    );
}

#[test]
fn i3_unrelated_advance_no_drift() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let fl = build_fl(&sess);
    let rx = sess.subscribe(fl.f.g, None).unwrap();
    // GB 전진하되 m1 무관 (m2 추가만)
    let eb = sess.edit(fl.gb, "main").unwrap();
    eb.add_node(
        &["Policy"],
        Some("ext-quota"),
        props([("name", "쿼터".into())]),
    )
    .unwrap();
    sess.commit(&eb, "B1").unwrap();
    assert_eq!(sess.drifts(fl.f.g, "main").unwrap(), vec![]);
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err()); // 드리프트 이벤트 없음
}

#[test]
fn i4_link_upgrade_clears_drift() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let fl = build_fl(&sess);
    let b1 = set_and_commit(&sess, fl.gb, "main", fl.m1, "rev", 2.into());
    assert_eq!(sess.drifts(fl.f.g, "main").unwrap().len(), 1);
    let l2 = committed(
        sess.link_upgrade(fl.f.g, "main", "acme/policy-kg", b1)
            .unwrap(),
    );
    let lock = sess.lockfile(fl.f.g, "main").unwrap();
    assert_eq!(lock["acme/policy-kg"].pin, b1);
    assert_eq!(sess.drifts(fl.f.g, "main").unwrap(), vec![]);
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (fl.f.g, l2)
    ); // 핀 갱신도 commit
}

#[test]
fn i6_single_shared_pin_per_target() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let fl = build_fl(&sess);
    // GB에 m2 추가(B1) 후 GA가 m2도 링크 → 단일 핀이 B1로 승격, 항목은 1개
    let eb = sess.edit(fl.gb, "main").unwrap();
    eb.add_node(
        &["Policy"],
        Some("ext-quota"),
        props([("name", "쿼터".into())]),
    )
    .unwrap();
    let b1 = committed(sess.commit(&eb, "B1").unwrap());
    let el = sess.edit(fl.f.g, "main").unwrap();
    el.set_prop(
        fl.f.n[3],
        "extern2",
        Value::Ref("graph://acme/policy-kg#ext-quota".into()),
    )
    .unwrap();
    sess.commit(&el, "link2").unwrap();
    let lock = sess.lockfile(fl.f.g, "main").unwrap();
    assert_eq!(lock.len(), 1); // 대상당 단일 핀
    assert_eq!(lock["acme/policy-kg"].pin, b1); // m2가 필요해 핀 승격
    assert_eq!(sess.drifts(fl.f.g, "main").unwrap(), vec![]); // m1 내용 불변 → 드리프트 없음
}

#[test]
fn i7_deref_always_pinned_version() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let fl = build_fl(&sess);
    // 대상은 rev=2로 전진했지만 핀은 B0
    set_and_commit(&sess, fl.gb, "main", fl.m1, "rev", 2.into());
    let v = sess.checkout(fl.f.g, fl.l1).unwrap();
    let (tv, node) = sess.deref(&v, "graph://acme/policy-kg#ext-sla").unwrap();
    assert_eq!(tv.epoch(), fl.b0);
    assert_eq!(node, fl.m1);
    assert_eq!(tv.node_prop(node, "rev"), Some(Value::Int(1))); // 핀 기준 — rev=2 아님
}
