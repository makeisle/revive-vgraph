//! 이름 있는 작업 공간 — 같은 사용자·같은 브랜치에서도 이름이 다르면 편집이 섞이지 않는다.
//! 이름 없는 기본 작업 공간의 동작(머리말 바이트·세션 재사용·WAL resume)은 그대로여야 한다.

mod util;

use util::*;
use vgraph_core::*;
use vgraph_engine::ops::WalHeader;
use vgraph_engine::*;

fn head_of(sess: &Session) -> Hash256 {
    sess.resolve("graph://acme/bookclub@main").unwrap().1
}

fn assert_committed(out: &CommitOutcome) {
    assert!(
        matches!(
            out,
            CommitOutcome::Committed { .. } | CommitOutcome::Rebased { .. }
        ),
        "expected committed/rebased, got {out:?}"
    );
}

#[test]
fn workspaces_do_not_see_each_other() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let a = sess.edit_in(f.g, "main", Some("a")).unwrap();
    let b = sess.edit_in(f.g, "main", Some("b")).unwrap();
    assert_ne!(a.wid(), b.wid());
    assert_eq!(a.workspace().as_deref(), Some("a"));
    assert_eq!(b.workspace().as_deref(), Some("b"));
    a.set_prop(f.n[0], "priority", 7.into()).unwrap();
    b.set_prop(f.n[1], "priority", 8.into()).unwrap();
    assert_eq!(a.pending().props_set.len(), 1);
    assert_eq!(a.pending().props_set[0].elem, f.n[0]);
    assert_eq!(b.pending().props_set.len(), 1);
    assert_eq!(b.pending().props_set[0].elem, f.n[1]);
    // 기본 작업 공간도 따로다.
    let d = sess.edit(f.g, "main").unwrap();
    assert!(!d.dirty());
    assert_eq!(d.workspace(), None);
    // 같은 이름은 재사용(C2와 같은 규칙).
    assert_eq!(sess.edit_in(f.g, "main", Some("a")).unwrap().wid(), a.wid());
}

#[test]
fn disjoint_edits_in_two_workspaces_both_land() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let a = sess.edit_in(f.g, "main", Some("a")).unwrap();
    let b = sess.edit_in(f.g, "main", Some("b")).unwrap();
    a.set_prop(f.n[0], "priority", 7.into()).unwrap();
    b.set_prop(f.n[1], "priority", 8.into()).unwrap();
    assert_committed(&sess.commit(&a, "a").unwrap());
    let out = sess.commit(&b, "b").unwrap();
    assert!(matches!(out, CommitOutcome::Rebased { .. }), "got {out:?}");
    let head = head_of(&sess);
    let v = sess.checkout(f.g, head).unwrap();
    assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(7)));
    assert_eq!(v.node_prop(f.n[1], "priority"), Some(Value::Int(8)));
}

#[test]
fn same_prop_in_two_workspaces_conflicts_and_keeps_first_head() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let a = sess.edit_in(f.g, "main", Some("a")).unwrap();
    let b = sess.edit_in(f.g, "main", Some("b")).unwrap();
    a.set_prop(f.n[0], "priority", 7.into()).unwrap();
    b.set_prop(f.n[0], "priority", 8.into()).unwrap();
    assert_committed(&sess.commit(&a, "a").unwrap());
    let head_after_a = head_of(&sess);
    let out = sess.commit(&b, "b").unwrap();
    assert!(matches!(out, CommitOutcome::Conflicts(_)), "got {out:?}");
    assert_eq!(head_of(&sess), head_after_a);
    let v = sess.checkout(f.g, head_after_a).unwrap();
    assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(7)));
}

#[test]
fn resume_of_other_workspace_is_not_reused_by_default_edit() {
    let dir = tempfile::TempDir::new().unwrap();
    let (g, wid_a, wid_b);
    {
        let engine = engine_at(dir.path(), true, 1);
        let sess = engine.session("ceo");
        let f = build_f0_engine(&sess);
        let a = sess.edit_in(f.g, "main", Some("a")).unwrap();
        let b = sess.edit_in(f.g, "main", Some("b")).unwrap();
        a.set_prop(f.n[0], "priority", 7.into()).unwrap();
        b.set_prop(f.n[1], "priority", 8.into()).unwrap();
        g = f.g;
        wid_a = a.wid();
        wid_b = b.wid();
        // 크래시: cleanup 없이 엔진 소멸
    }
    let engine2 = engine_at(dir.path(), true, 1000);
    let sess2 = engine2.session("ceo");
    let rb = sess2.resume(g, wid_b).unwrap();
    assert_eq!(rb.workspace().as_deref(), Some("b"));
    let d = sess2.edit(g, "main").unwrap();
    assert_ne!(d.wid(), wid_b, "default edit must not reuse workspace b");
    assert!(!d.dirty());
    let a2 = sess2.edit_in(g, "main", Some("a")).unwrap();
    assert_ne!(a2.wid(), wid_b, "workspace a must not reuse workspace b");
    assert_ne!(a2.wid(), wid_a); // a는 resume하지 않았으므로 새 WAL
                                 // b 자신은 재사용된다.
    assert_eq!(sess2.edit_in(g, "main", Some("b")).unwrap().wid(), wid_b);
}

#[test]
fn named_workspace_reopens_after_commit() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    let a = sess.edit_in(f.g, "main", Some("a")).unwrap();
    a.set_prop(f.n[0], "priority", 7.into()).unwrap();
    assert_committed(&sess.commit(&a, "first").unwrap());
    let a2 = sess.edit_in(f.g, "main", Some("a")).unwrap();
    assert_ne!(a2.wid(), a.wid());
    assert!(!a2.dirty());
    a2.set_prop(f.n[0], "priority", 9.into()).unwrap();
    assert_committed(&sess.commit(&a2, "second").unwrap());
    let head = head_of(&sess);
    let v = sess.checkout(f.g, head).unwrap();
    assert_eq!(v.node_prop(f.n[0], "priority"), Some(Value::Int(9)));
}

#[test]
fn invalid_workspace_names_are_rejected() {
    let env = new_env(true);
    let sess = env.engine.session("ceo");
    let f = build_f0_engine(&sess);
    for bad in ["", "a b", "a/b", "한글", "a.b", "a\"b"] {
        let r = sess.edit_in(f.g, "main", Some(bad));
        assert!(
            matches!(
                r,
                Err(EditStartError::Engine(EngineError::Resolve(
                    ResolveError::BadAddr(_)
                )))
            ),
            "name {bad:?} should be rejected, got {r:?}"
        );
    }
    assert!(sess.edit_in(f.g, "main", Some("Ok-name_1")).is_ok());
}

#[test]
fn default_workspace_header_bytes_unchanged() {
    let h = WalHeader {
        base: None,
        branch: Some("main".into()),
        owner: "ceo".into(),
        workspace: None,
    };
    assert_eq!(
        h.to_json(),
        r#"{"hdr":true,"base":null,"branch":"main","owner":"ceo"}"#
    );
    let n = WalHeader {
        workspace: Some("a".into()),
        ..h
    };
    assert_eq!(
        n.to_json(),
        r#"{"hdr":true,"base":null,"branch":"main","owner":"ceo","workspace":"a"}"#
    );
    let back = WalHeader::from_json(&n.to_json()).unwrap();
    assert_eq!(back.workspace.as_deref(), Some("a"));
}

#[test]
fn legacy_wal_without_workspace_resumes_as_default() {
    let legacy = r#"{"hdr":true,"base":null,"branch":"main","owner":"ceo"}"#;
    assert_eq!(WalHeader::from_json(legacy).unwrap().workspace, None);

    let dir = tempfile::TempDir::new().unwrap();
    let (g, wid);
    {
        let engine = engine_at(dir.path(), true, 1);
        let sess = engine.session("ceo");
        let f = build_f0_engine(&sess);
        let es = sess.edit(f.g, "main").unwrap(); // 기본 = 이름 키 없는 머리말
        es.set_prop(f.n[0], "priority", 9.into()).unwrap();
        g = f.g;
        wid = es.wid();
    }
    let engine2 = engine_at(dir.path(), true, 1000);
    let sess2 = engine2.session("ceo");
    let es2 = sess2.resume(g, wid).unwrap();
    assert_eq!(es2.workspace(), None);
    assert_eq!(sess2.edit(g, "main").unwrap().wid(), wid); // 기본 키로 재사용
    assert_committed(&sess2.commit(&es2, "legacy").unwrap());
}
