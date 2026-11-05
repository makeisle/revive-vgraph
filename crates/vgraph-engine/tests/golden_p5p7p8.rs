//! P5 게이트: H(오버레이)·D8·F7 + P7 게이트: GC(I5·고아·payload) + P8: review_queue 어댑터.

mod util;

use std::collections::BTreeMap;
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

/// FS 픽스처 (골든 §1.3).
fn fs_schema(mode: SchemaMode) -> Schema {
    let mut node_classes = BTreeMap::new();
    node_classes.insert(
        "Capability".into(),
        NodeClassDef {
            required: vec!["name".into()],
        },
    );
    node_classes.insert(
        "Requirement".into(),
        NodeClassDef {
            required: vec!["text".into()],
        },
    );
    node_classes.insert(
        "Policy".into(),
        NodeClassDef {
            required: vec!["name".into()],
        },
    );
    let mut edge_classes = BTreeMap::new();
    edge_classes.insert(
        "parent".into(),
        EdgeClassDef {
            domain: vec!["Requirement".into()],
            range: vec!["Capability".into()],
            max_out: None,
        },
    );
    edge_classes.insert(
        "impacts".into(),
        EdgeClassDef {
            domain: vec!["Capability".into()],
            range: vec!["Capability".into()],
            max_out: None,
        },
    );
    edge_classes.insert(
        "policy".into(),
        EdgeClassDef {
            domain: vec!["Requirement".into()],
            range: vec!["Policy".into()],
            max_out: Some(1),
        },
    );
    Schema {
        mode,
        node_classes,
        edge_classes,
    }
}

// ================= H. KG 오버레이 =================

#[test]
fn h1_no_schema_free_commit() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let es = sess.edit(f.g, "main").unwrap();
    let w1 = es
        .add_node(&["Whatever"], Some("w1"), props([("x", 1.into())]))
        .unwrap();
    es.add_edge("anything", f.n[0], w1, Props::new()).unwrap();
    committed(sess.commit(&es, "free").unwrap());
}

#[test]
fn h2_strict_required_and_domain() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let c1 = committed(
        sess.set_schema(f.g, "main", &fs_schema(SchemaMode::Strict))
            .unwrap(),
    );

    // H2a: required 누락
    let es = sess.edit(f.g, "main").unwrap();
    let nx = es
        .add_node(
            &["Requirement"],
            Some("req-x"),
            props([("status", "draft".into())]),
        )
        .unwrap();
    match sess.commit(&es, "bad").unwrap() {
        CommitOutcome::SchemaViolation(issues) => assert_eq!(
            issues,
            vec![SchemaIssue::RequiredMissing {
                class: "Requirement".into(),
                elem: nx,
                key: "text".into()
            }]
        ),
        other => panic!("{other:?}"),
    }
    sess.discard(es).unwrap();

    // H2b: domain 위반 — from이 Capability인 parent (to는 range 적합인 Capability)
    let es = sess.edit(f.g, "main").unwrap();
    let ex = es.add_edge("parent", f.n[0], f.n[1], Props::new()).unwrap();
    match sess.commit(&es, "bad2").unwrap() {
        CommitOutcome::SchemaViolation(issues) => assert_eq!(
            issues,
            vec![SchemaIssue::DomainViolation {
                edge_class: "parent".into(),
                edge: ex,
                end: f.n[0],
                expected: vec!["Requirement".into()],
            }]
        ),
        other => panic!("{other:?}"),
    }
    // head 불변
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, c1)
    );
}

#[test]
fn h3_warn_passes_with_warnings() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    committed(
        sess.set_schema(f.g, "main", &fs_schema(SchemaMode::Warn))
            .unwrap(),
    );
    let es = sess.edit(f.g, "main").unwrap();
    es.add_node(
        &["Requirement"],
        Some("req-x"),
        props([("status", "draft".into())]),
    )
    .unwrap();
    match sess.commit(&es, "warned").unwrap() {
        CommitOutcome::Committed { warnings, .. } => {
            assert_eq!(warnings.len(), 1);
            assert!(matches!(warnings[0], SchemaIssue::RequiredMissing { .. }));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn h4_schema_is_a_commit() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let c1 = committed(
        sess.set_schema(f.g, "main", &fs_schema(SchemaMode::Strict))
            .unwrap(),
    );
    assert_eq!(sess.log(f.g, "main").unwrap(), vec![c1, f.c0]);
    let loaded = sess.schema(f.g, c1).unwrap().expect("schema at c1");
    assert_eq!(loaded, fs_schema(SchemaMode::Strict));
    assert_eq!(sess.schema(f.g, f.c0).unwrap(), None); // 과거엔 스키마 없음
}

#[test]
fn h5_validate_dry_run() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    // H5a: F0은 FS 적합
    assert_eq!(
        sess.validate(f.g, f.c0, &fs_schema(SchemaMode::Strict))
            .unwrap(),
        vec![]
    );
    // H5b: 스키마 없는 상태에서 위반 데이터 commit → 소급 검증이 잡는다
    let es = sess.edit(f.g, "main").unwrap();
    es.add_node(
        &["Requirement"],
        Some("req-x"),
        props([("status", "draft".into())]),
    )
    .unwrap();
    let c1 = committed(sess.commit(&es, "no-schema").unwrap());
    let issues = sess
        .validate(f.g, c1, &fs_schema(SchemaMode::Strict))
        .unwrap();
    assert_eq!(issues.len(), 1);
    assert!(matches!(issues[0], SchemaIssue::RequiredMissing { .. }));
}

#[test]
fn d8_strict_rejects_unset_required() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let c1 = committed(
        sess.set_schema(f.g, "main", &fs_schema(SchemaMode::Strict))
            .unwrap(),
    );
    let es = sess.edit(f.g, "main").unwrap();
    es.unset_prop(f.n[0], "name").unwrap();
    match sess.commit(&es, "bad").unwrap() {
        CommitOutcome::SchemaViolation(issues) => assert_eq!(
            issues,
            vec![SchemaIssue::RequiredMissing {
                class: "Capability".into(),
                elem: f.n[0],
                key: "name".into()
            }]
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        sess.resolve("graph://acme/bookclub@main").unwrap(),
        (f.g, c1)
    );
}

#[test]
fn f7_schema_clash_on_merge() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let c1 = committed(
        sess.set_schema(f.g, "main", &fs_schema(SchemaMode::Strict))
            .unwrap(),
    );
    sess.branch(f.g, c1, "dev-b").unwrap();
    // 각자는 valid: policy(n3→n5) 하나씩 (max_out=1)
    let es = sess.edit(f.g, "main").unwrap();
    es.add_edge("policy", f.n[2], f.n[4], Props::new()).unwrap();
    committed(sess.commit(&es, "a").unwrap());
    let es = sess.edit(f.g, "dev-b").unwrap();
    es.add_edge("policy", f.n[2], f.n[4], Props::new()).unwrap();
    committed(sess.commit(&es, "b").unwrap());
    // 병합 결과 = n3의 policy 엣지 2개 → SchemaClash
    match sess
        .merge(f.g, "dev-b", "main", &MergeOpts::default())
        .unwrap()
    {
        MergeOutcome::Conflicts(report) => assert_eq!(
            report.items,
            vec![ConflictItem::SchemaClash {
                issue: SchemaIssue::Cardinality {
                    edge_class: "policy".into(),
                    elem: f.n[2],
                    max: 1,
                    found: 2,
                }
            }]
        ),
        other => panic!("{other:?}"),
    }
}

// ================= GC (P7) =================

#[test]
fn gc_i5_external_pin_is_root() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    // FL: GB 생성 + GA가 ext-sla 링크 (pin=B0)
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
    committed(sess.commit(&el, "link").unwrap());
    // GB 전진 (B0는 이제 head가 아님)
    let b1 = set_prop_commit(&sess, gb, "main", m1, "rev", 2.into());
    // GC: B0는 GA의 핀 → 루트로 보존
    let plan = sess.gc_plan(gb).unwrap();
    assert!(plan.roots.contains(&b0));
    assert!(plan.roots.contains(&b1));
    assert!(!plan.orphan_blobs.contains(&b0));
    sess.gc_run(gb).unwrap();
    let v = sess.at(gb, b0).unwrap(); // 핀 버전 생존
    assert_eq!(v.node_prop(m1, "rev"), Some(Value::Int(1)));
}

fn set_prop_commit(
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

#[test]
fn gc_collects_detached_orphans() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let c1 = set_prop_commit(&sess, f.g, "main", f.n[0], "priority", 9.into());
    // detached commit (D9b 스타일) — 어느 ref에서도 미도달
    let es = sess.edit_opts(f.g, "main", Some(f.c0), true).unwrap();
    es.set_prop(f.n[4], "name", "응답 150ms".into()).unwrap();
    let cd = committed(sess.commit(&es, "detached").unwrap());
    let plan = sess.gc_plan(f.g).unwrap();
    assert!(plan.orphan_blobs.contains(&cd)); // 고아 회수 대상 (K3-② 동형)
    assert!(plan.keep_blobs.contains(&c1) && plan.keep_blobs.contains(&f.c0));
    let report = sess.gc_run(f.g).unwrap();
    assert!(report.removed_blobs >= 2); // cd commit + cd delta
                                        // 회수 후: detached는 소멸, 계보는 생존
    assert!(matches!(
        sess.checkout(f.g, cd),
        Err(EngineError::Resolve(ResolveError::UnknownCommit))
    ));
    assert!(sess.at(f.g, f.c0).is_ok());
    assert!(sess.at(f.g, c1).is_ok());
}

#[test]
fn gc_payloads_head_only_history_still_folds() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let c1 = set_prop_commit(&sess, f.g, "main", f.n[0], "priority", 9.into());
    // 구 payload(priority:1)는 head 상태에 없음 → 회수 대상
    let report = sess.gc_run(f.g).unwrap();
    assert!(report.removed_payloads >= 1);
    assert_eq!(report.removed_blobs, 0); // 계보는 전부 도달 가능
                                         // 과거는 fold로 여전히 재구성 (payload row 불필요 — delta에 내장)
    let v0 = sess.at(f.g, f.c0).unwrap();
    assert_eq!(v0.node_prop(f.n[0], "priority"), Some(Value::Int(1)));
    // head lazy 경로도 무손상 (head payload는 keep)
    sess.test_evict_all(f.g).unwrap();
    let v1 = sess.checkout(f.g, c1).unwrap();
    assert_eq!(v1.node_prop(f.n[0], "priority"), Some(Value::Int(9)));
}

// ================= P8: review_queue 어댑터 =================

#[test]
fn p8_conflict_to_propose_queue() {
    let env = new_env(true);
    let s_a = env.engine.session("ceo");
    let s_b = env.engine.session("ceo");
    let f = build_f0_engine(&s_a);
    // D7 시나리오로 실제 ConflictReport 생성
    let w1 = s_a.edit(f.g, "main").unwrap();
    let w2 = s_b.edit(f.g, "main").unwrap();
    w1.set_prop(f.n[0], "status", "deprecated".into()).unwrap();
    w2.set_prop(f.n[0], "status", "beta".into()).unwrap();
    committed(s_a.commit(&w1, "w1").unwrap());
    let report = match s_b.commit(&w2, "w2").unwrap() {
        CommitOutcome::Conflicts(r) => r,
        other => panic!("{other:?}"),
    };

    // Proposal JSON 스키마 검증 (queue.rs 정합: id/target/field/new_value/status)
    let proposals = review::to_proposals(&report, "acme/bookclub@main");
    assert_eq!(proposals.len(), 1);
    let p = &proposals[0];
    assert_eq!(p["id"].as_str().unwrap().len(), 16); // sha256[..16] — 멱등 키 공식 복제
    assert!(p["target"]
        .as_str()
        .unwrap()
        .starts_with("vgraph://acme/bookclub@main#"));
    assert_eq!(p["field"], "status");
    assert_eq!(p["status"], "pending");
    assert_eq!(p["rule_id"], "vgraph-merge");
    assert_eq!(p["new_value"]["kind"], "prop_both_set");
    assert_eq!(p["new_value"]["ours"], "beta"); // 미커밋 working 측
    assert_eq!(p["new_value"]["theirs"], "deprecated"); // 선행 commit 측
    assert_eq!(p["new_value"]["apply"], "deprecated"); // 승인 시 채택 값 = theirs

    // 디렉터리 enqueue: 파일 생성 + 멱등 (동일 id 재기록 없음 — tombstone 존중)
    let dir = tempfile::TempDir::new().unwrap();
    let queue = dir.path().join("propose");
    let written = review::enqueue_dir(&queue, &report, "acme/bookclub@main").unwrap();
    assert_eq!(written.len(), 1);
    assert!(queue.join(format!("{}.json", written[0])).exists());
    let again = review::enqueue_dir(&queue, &report, "acme/bookclub@main").unwrap();
    assert_eq!(again.len(), 0); // 멱등
}
