//! 종료 지점 복구 (인시던트 2026-11-07 결함 B).
//!
//! 커밋·병합은 "권위 계층"(ref CAS)이 먼저 반영되고 "파생 계층"(읽기 색인 rows·워터마크)이 뒤따른다.
//! 둘 사이에서 끊기면 워터마크가 head 뒤에 남는다. 다음 커밋이 자기 delta만 증분 반영하면
//! 건너뛴 커밋의 노드가 rows에서 영구히 빠진다. 아래 시험은 그 순서를 엔진 수준 `FaultBackend`
//! (`CrashPoint::AfterCas`: ref 반영 뒤 오류 반환)로 만들고, 불변식 ①~⑤를 대조한다.
//!
//! 모킹 경계: 모킹한 것은 없다. 실제 SqliteBackend 위에 고장 주입 래퍼만 얹었다 (경계 = 저장소).

mod util;

use tempfile::TempDir;
use util::*;
use vgraph_core::value::props;
use vgraph_core::*;
use vgraph_engine::*;
use vgraph_store::{BlobStore, CrashPoint, RefStore};

const USER: &str = "team";
/// 엔진 세대별 id 시작값 간격 (uuid·작업본 id 충돌 방지).
const GEN: u64 = 1000;

fn slugs(items: &[&str]) -> std::collections::BTreeSet<String> {
    items.iter().map(ToString::to_string).collect()
}

fn committed(out: CommitOutcome) -> Hash256 {
    match out {
        CommitOutcome::Committed { commit, .. } => commit,
        other => panic!("커밋 성공이어야 함: {other:?}"),
    }
}

/// 그래프 생성 + seed 커밋. (엔진, 고장 슬롯, 그래프, seed 커밋)
fn seeded(root: &std::path::Path) -> (Engine, FaultSlot, GraphId, Hash256) {
    let (engine, slot) = fault_engine(root, 1);
    let s = engine.session(USER);
    let g = s.create_graph("team", "g").unwrap();
    let c = committed(commit_node(&s, g, "w0", "seed"));
    (engine, slot, g, c)
}

/// w7이 victim 노드를 커밋하다가 ref 반영 직후 끊긴다. 남은 것: 새 head(victim 커밋), 옛 워터마크.
fn crash_victim_after_cas(engine: &Engine, slot: &FaultSlot, g: GraphId) -> EditSession {
    let s = engine.session(USER);
    let es7 = open_ws(&s, g, "w7");
    es7.add_node(&["T"], Some("victim"), props([("v", "x".into())]))
        .unwrap();
    arm(slot, CrashPoint::AfterCas);
    let r = s.commit(&es7, "victim");
    disarm(slot);
    assert!(r.is_err(), "AfterCas는 오류를 돌려줘야 함: {r:?}");
    es7
}

/// w7이 victim을 커밋하다가 upsert_head 적용 직후 끊긴다 (P2). ref·읽기 색인은 이미 victim 커밋 기준이다.
fn crash_victim_after_upsert_head(engine: &Engine, slot: &FaultSlot, g: GraphId) -> EditSession {
    let s = engine.session(USER);
    let es7 = open_ws(&s, g, "w7");
    es7.add_node(&["T"], Some("victim"), props([("v", "x".into())]))
        .unwrap();
    arm_after_upsert_head(slot);
    let r = s.commit(&es7, "victim");
    disarm(slot);
    assert!(
        r.is_err(),
        "upsert_head 뒤 끊김은 오류를 돌려줘야 함: {r:?}"
    );
    es7
}

fn not_conflicts(out: &CommitOutcome, what: &str) {
    assert!(
        !matches!(out, CommitOutcome::Conflicts(_)),
        "{what}: 이미 반영된 작업본이 충돌로 막힘: {out:?}"
    );
}

/// 불변식 ①②③④ + ⑥(⑦).
fn all_invariants(s: &Session, root: &std::path::Path, g: GraphId, what: &str) {
    assert_ok(check_invariants(root, g, "main", true), what);
    assert_ok(check_no_reflected_leftover(s, root, g, "main"), what);
}

fn assert_ok(r: Result<(), String>, what: &str) {
    if let Err(e) = r {
        panic!("{what}: {e}");
    }
}

// ---------------- T3: 같은 엔진에서 이어 쓰기 ----------------

#[test]
fn t3_after_cas_then_same_engine_continues_keeps_victim() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    crash_victim_after_cas(&e1, &slot, g);

    // 다시 열지 않고 다른 작업 공간이 이어서 커밋
    let s1 = e1.session(USER);
    committed(commit_node(&s1, g, "w2", "next"));

    assert_ok(
        check_invariants(root, g, "main", true),
        "같은 엔진에서 이어 쓴 뒤 불변식 ①②③④",
    );
    assert_eq!(
        visible_slugs(&s1, root, g, "main"),
        slugs(&["seed", "victim", "next"]),
        "같은 엔진에서 읽기"
    );

    // 다시 열기 → 유지 (불변식 ⑤)
    drop(s1);
    drop(e1);
    drop(slot);
    let (e2, _slot2) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    assert_eq!(
        visible_slugs(&s2, root, g, "main"),
        slugs(&["seed", "victim", "next"]),
        "다시 연 뒤 읽기"
    );
    assert_ok(check_invariants(root, g, "main", true), "다시 연 뒤 불변식");
}

// ---------------- 재기동 경로: P1 -> 다시 열기 -> 다른 작업 공간 커밋 -> 다시 열기 ----------------

#[test]
fn p1_reopen_then_other_workspace_commit_then_reopen_keeps_victim() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    crash_victim_after_cas(&e1, &slot, g);
    drop(e1);
    drop(slot);

    // 재기동: 엔진을 열어 그래프를 만지는 순간 워터마크가 head로 따라잡혀야 한다 (읽기 전용 접근 포함)
    let (e2, _s2slot) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    assert_eq!(
        visible_slugs(&s2, root, g, "main"),
        slugs(&["seed", "victim"]),
        "재기동 직후 읽기"
    );
    committed(commit_node(&s2, g, "w2", "next"));
    assert_ok(
        check_invariants(root, g, "main", true),
        "재기동 뒤 다음 커밋 후 불변식",
    );
    drop(s2);
    drop(e2);

    let (e3, _s3slot) = fault_engine(root, 2 * GEN);
    let s3 = e3.session(USER);
    assert_eq!(
        visible_slugs(&s3, root, g, "main"),
        slugs(&["seed", "victim", "next"]),
        "두 번째 재기동 뒤 읽기"
    );
}

// ---------------- 따라잡기는 사라진 요소의 rows도 치운다 ----------------

#[test]
fn catch_up_also_removes_rows_of_elements_deleted_by_the_skipped_commit() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (engine, slot) = fault_engine(root, 1);
    let s = engine.session(USER);
    let g = s.create_graph("team", "g").unwrap();
    let es = open_ws(&s, g, "w0");
    let a = es
        .add_node(&["T"], Some("a"), props([("v", "a".into())]))
        .unwrap();
    let b = es
        .add_node(&["T"], Some("b"), props([("v", "b".into())]))
        .unwrap();
    es.add_edge("link", a, b, Props::new()).unwrap();
    committed(s.commit(&es, "seed").unwrap());

    // 끊기는 커밋: b(와 연결된 엣지)를 지우고 victim 추가
    let es7 = open_ws(&s, g, "w7");
    es7.remove_node(b, true).unwrap();
    es7.add_node(&["T"], Some("victim"), props([("v", "x".into())]))
        .unwrap();
    arm(&slot, CrashPoint::AfterCas);
    assert!(s.commit(&es7, "victim").is_err());
    disarm(&slot);

    committed(commit_node(&s, g, "w2", "next"));
    assert_ok(
        check_invariants(root, g, "main", true),
        "지워진 요소의 rows가 남지 않아야 함",
    );
    assert_eq!(
        visible_slugs(&s, root, g, "main"),
        slugs(&["a", "victim", "next"])
    );
}

// ---------------- T5: 병합 ----------------

#[test]
fn t5_fast_forward_merge_cut_after_cas_keeps_feature_node() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (engine, slot, g, seed) = seeded(root);
    let s = engine.session(USER);
    s.branch(g, seed, "feat").unwrap();
    let f = s.edit_in(g, "feat", Some("f")).unwrap();
    f.add_node(&["T"], Some("feat-node"), props([("v", "f".into())]))
        .unwrap();
    committed(s.commit(&f, "feat").unwrap());

    arm(&slot, CrashPoint::AfterCas);
    let r = s.merge(g, "feat", "main", &MergeOpts::default());
    disarm(&slot);
    assert!(r.is_err(), "AfterCas는 오류를 돌려줘야 함: {r:?}");

    committed(commit_node(&s, g, "w2", "next"));
    assert_ok(
        check_invariants(root, g, "main", true),
        "fast-forward 병합이 끊긴 뒤 다음 main 커밋의 불변식 ①②③",
    );
    drop(s);
    drop(engine);
    drop(slot);

    let (e2, _slot2) = fault_engine(root, GEN);
    assert_eq!(
        visible_slugs(&e2.session(USER), root, g, "main"),
        slugs(&["seed", "feat-node", "next"])
    );
}

#[test]
fn t5_merge_commit_cut_after_cas_keeps_feature_node() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (engine, slot, g, seed) = seeded(root);
    let s = engine.session(USER);
    s.branch(g, seed, "feat").unwrap();
    let f = s.edit_in(g, "feat", Some("f")).unwrap();
    f.add_node(&["T"], Some("feat-node"), props([("v", "f".into())]))
        .unwrap();
    committed(s.commit(&f, "feat").unwrap());
    committed(commit_node(&s, g, "w1", "main-node")); // main도 앞서 가서 fast-forward가 아니게 한다

    arm(&slot, CrashPoint::AfterCas);
    let r = s.merge(g, "feat", "main", &MergeOpts::default());
    disarm(&slot);
    assert!(r.is_err(), "AfterCas는 오류를 돌려줘야 함: {r:?}");

    committed(commit_node(&s, g, "w2", "next"));
    assert_ok(
        check_invariants(root, g, "main", true),
        "병합 커밋이 끊긴 뒤 다음 main 커밋의 불변식 ①②③",
    );
    drop(s);
    drop(engine);
    drop(slot);

    let (e2, _slot2) = fault_engine(root, GEN);
    assert_eq!(
        visible_slugs(&e2.session(USER), root, g, "main"),
        slugs(&["seed", "feat-node", "main-node", "next"])
    );
}

// ---------------- 회귀: 끊김 없는 경로는 그대로 ----------------

#[test]
fn regression_uninterrupted_commits_and_merges_keep_rows_in_sync() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (engine, _slot, g, seed) = seeded(root);
    let s = engine.session(USER);
    assert_ok(check_invariants(root, g, "main", true), "seed 뒤");

    // 편집(삭제·엣지 포함) 커밋
    let es = open_ws(&s, g, "w1");
    let a = es
        .add_node(&["T"], Some("a"), props([("v", "a".into())]))
        .unwrap();
    let b = es
        .add_node(&["T"], Some("b"), props([("v", "b".into())]))
        .unwrap();
    es.add_edge("link", a, b, Props::new()).unwrap();
    committed(s.commit(&es, "ab").unwrap());
    assert_ok(check_invariants(root, g, "main", true), "노드·엣지 추가 뒤");
    let es = open_ws(&s, g, "w1");
    es.set_prop(a, "v", "a2".into()).unwrap();
    es.remove_node(b, true).unwrap();
    committed(s.commit(&es, "edit").unwrap());
    assert_ok(check_invariants(root, g, "main", true), "수정·삭제 뒤");

    // 병합 커밋 (둘 다 앞섬)
    s.branch(g, seed, "feat").unwrap();
    let f = s.edit_in(g, "feat", Some("f")).unwrap();
    f.add_node(&["T"], Some("feat-node"), props([("v", "f".into())]))
        .unwrap();
    committed(s.commit(&f, "feat").unwrap());
    assert_ok(check_invariants(root, g, "feat", true), "feat 커밋 뒤");
    assert!(matches!(
        s.merge(g, "feat", "main", &MergeOpts::default()).unwrap(),
        MergeOutcome::Merged(_)
    ));
    assert_ok(check_invariants(root, g, "main", true), "병합 커밋 뒤");

    // fast-forward 병합
    let head = raw_backend(root, g)
        .get_ref("main")
        .unwrap()
        .expect("main head");
    s.branch(g, head, "feat2").unwrap();
    let f2 = s.edit_in(g, "feat2", Some("f2")).unwrap();
    f2.add_node(&["T"], Some("ff-node"), props([("v", "g".into())]))
        .unwrap();
    committed(s.commit(&f2, "ff").unwrap());
    assert!(matches!(
        s.merge(g, "feat2", "main", &MergeOpts::default()).unwrap(),
        MergeOutcome::FastForward(_)
    ));
    assert_ok(check_invariants(root, g, "main", true), "fast-forward 뒤");
    assert_eq!(
        visible_slugs(&s, root, g, "main"),
        slugs(&["seed", "a", "feat-node", "ff-node"])
    );
}

// ---------------- Err: 따라잡기가 못 접어도 그래프는 열린다 ----------------

#[test]
fn catch_up_failure_does_not_make_the_graph_unopenable() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    crash_victim_after_cas(&e1, &slot, g);
    drop(e1);
    drop(slot);

    // head 커밋의 delta 블롭이 사라진 상태 (접기 불가)
    let b = raw_backend(root, g);
    let head = b.get_ref("main").unwrap().unwrap();
    let chain = first_parent_chain(&b, head).unwrap();
    b.delete_blob(&chain[0].delta).unwrap();
    drop(b);

    let (e2, _slot2) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    assert_eq!(s2.graph_id("team", "g"), Some(g), "그래프는 열려야 함");
    assert!(
        s2.checkout(g, head).is_err(),
        "접을 수 없는 head는 오류로 드러나야 함 (패닉·무음 금지)"
    );
}

// ======================================================================
// Step 3 (결함 C): 되살린 작업본이 "이미 head 체인에 있는가" 판별
// ======================================================================

// ---------------- T1 ----------------

#[test]
fn t1_p1_reopen_leftover_commits_first_then_other_workspace_then_reopen_keeps_victim() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    let _stale = crash_victim_after_cas(&e1, &slot, g);
    drop(e1);
    drop(slot);

    let (e2, _slot2) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    // 남은 작업본(w7)이 먼저 커밋을 시도한다: 몇 번을 해도 충돌로 막히지 않는다
    for i in 0..3 {
        let es = open_ws(&s2, g, "w7");
        let out = s2.commit(&es, "retry").unwrap();
        not_conflicts(&out, &format!("남은 작업본 재시도 {i}"));
    }
    assert_eq!(
        visible_slugs(&s2, root, g, "main"),
        slugs(&["seed", "victim"])
    );
    all_invariants(&s2, root, g, "남은 작업본 커밋 뒤");

    committed(commit_node(&s2, g, "w2", "next"));
    all_invariants(&s2, root, g, "다른 작업 공간 커밋 뒤");
    drop(s2);
    drop(e2);

    let (e3, _slot3) = fault_engine(root, 2 * GEN);
    let s3 = e3.session(USER);
    assert_eq!(
        visible_slugs(&s3, root, g, "main"),
        slugs(&["seed", "victim", "next"]),
        "다시 연 뒤에도 victim 유지"
    );
    all_invariants(&s3, root, g, "다시 연 뒤");
}

// ---------------- T2 ----------------

#[test]
fn t2_p1_other_workspace_commits_first_then_leftover_commit_adds_victim_once() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    let es7 = crash_victim_after_cas(&e1, &slot, g);
    let s1 = e1.session(USER);

    committed(commit_node(&s1, g, "w2", "next")); // base가 바뀐다
    let out = s1.commit(&es7, "victim again").unwrap(); // 남은 작업본이 뒤늦게 커밋
    not_conflicts(&out, "다른 작업 공간이 먼저 커밋한 뒤 남은 작업본 커밋");

    all_invariants(&s1, root, g, "T2 순서 뒤");
    assert_eq!(
        visible_slugs(&s1, root, g, "main"),
        slugs(&["seed", "victim", "next"])
    );
    // 다음 커밋이 끝난 뒤에도 같다
    committed(commit_node(&s1, g, "w3", "after"));
    all_invariants(&s1, root, g, "T2 순서 + 다음 커밋 뒤");
}

#[test]
fn t2_p1_other_workspace_commits_first_then_reopen_leftover_is_dropped() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    let _stale = crash_victim_after_cas(&e1, &slot, g);
    committed(commit_node(&e1.session(USER), g, "w2", "next"));
    drop(e1);
    drop(slot);

    let (e2, _slot2) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    let es = open_ws(&s2, g, "w7");
    let out = s2.commit(&es, "retry").unwrap();
    not_conflicts(&out, "다시 연 뒤 남은 작업본");
    all_invariants(&s2, root, g, "T2 재기동 변형");
    assert_eq!(
        visible_slugs(&s2, root, g, "main"),
        slugs(&["seed", "victim", "next"])
    );
}

// ---------------- T4-a: P2 (upsert_head 적용 뒤 끊김) ----------------

#[test]
fn t4a_p2_leftover_retry_before_other_commit_is_not_blocked() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    let es7 = crash_victim_after_upsert_head(&e1, &slot, g);
    let s1 = e1.session(USER);

    let out = s1.commit(&es7, "retry").unwrap();
    not_conflicts(&out, "P2 뒤 같은 작업본 재커밋(다른 커밋 전)");
    all_invariants(&s1, root, g, "P2 재커밋 뒤");
    committed(commit_node(&s1, g, "w2", "next"));
    all_invariants(&s1, root, g, "P2 -> 다른 작업 공간 커밋 뒤");
    assert_eq!(
        visible_slugs(&s1, root, g, "main"),
        slugs(&["seed", "victim", "next"])
    );
}

#[test]
fn t4a_p2_leftover_retry_after_other_commit_is_not_blocked() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    let es7 = crash_victim_after_upsert_head(&e1, &slot, g);
    let s1 = e1.session(USER);

    committed(commit_node(&s1, g, "w2", "next"));
    let out = s1.commit(&es7, "retry").unwrap();
    not_conflicts(&out, "P2 뒤 다른 작업 공간 커밋 다음에 같은 작업본 재커밋");
    all_invariants(&s1, root, g, "P2 -> 다른 작업 공간 -> 재커밋 뒤");
    assert_eq!(
        visible_slugs(&s1, root, g, "main"),
        slugs(&["seed", "victim", "next"])
    );
}

#[test]
fn t4a_p2_reopen_leftover_is_not_blocked() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    let _stale = crash_victim_after_upsert_head(&e1, &slot, g);
    drop(e1);
    drop(slot);

    let (e2, _slot2) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    let es = open_ws(&s2, g, "w7");
    not_conflicts(&s2.commit(&es, "retry").unwrap(), "P2 -> 다시 연 뒤");
    committed(commit_node(&s2, g, "w2", "next"));
    all_invariants(&s2, root, g, "P2 -> 다시 열기 -> 다른 작업 공간");
    drop(s2);
    drop(e2);

    let (e3, _slot3) = fault_engine(root, 2 * GEN);
    let s3 = e3.session(USER);
    assert_eq!(
        visible_slugs(&s3, root, g, "main"),
        slugs(&["seed", "victim", "next"])
    );
    all_invariants(&s3, root, g, "두 번째 재기동 뒤");
}

// ---------------- T4-b: drop_wal 실패 (H-C4, 불변식 ⑦) ----------------

#[test]
fn t4b_drop_wal_failure_keeps_commit_success_and_leftover_is_cleaned_next_call() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    let s1 = e1.session(USER);

    let es7 = open_ws(&s1, g, "w7");
    es7.add_node(&["T"], Some("victim"), props([("v", "x".into())]))
        .unwrap();
    arm_drop_wal_fails(&slot);
    let out = s1.commit(&es7, "victim").unwrap(); // 정리가 실패해도 커밋 성공 응답
    disarm(&slot);
    committed(out);
    assert_eq!(
        s1.list_working(g).unwrap().len(),
        1,
        "drop_wal 실패로 작업본이 남아 있어야 이 시험이 의미가 있다"
    );

    // 다음 호출: 같은 작업 공간에서 새 편집·커밋이 막히지 않는다
    let es = open_ws(&s1, g, "w7");
    es.add_node(&["T"], Some("w7b"), props([("v", "w7b".into())]))
        .unwrap();
    committed(s1.commit(&es, "w7b").unwrap());
    all_invariants(&s1, root, g, "drop_wal 실패 다음 호출 뒤");
    assert!(
        s1.list_working(g).unwrap().is_empty(),
        "남은 작업본이 정리되어야 함: {:?}",
        s1.list_working(g).unwrap()
    );
    assert_eq!(
        visible_slugs(&s1, root, g, "main"),
        slugs(&["seed", "victim", "w7b"])
    );
}

#[test]
fn t4b_drop_wal_failure_then_other_commit_then_reopen_is_cleaned() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    let s1 = e1.session(USER);
    let es7 = open_ws(&s1, g, "w7");
    es7.add_node(&["T"], Some("victim"), props([("v", "x".into())]))
        .unwrap();
    arm_drop_wal_fails(&slot);
    committed(s1.commit(&es7, "victim").unwrap());
    disarm(&slot);
    committed(commit_node(&s1, g, "w2", "next"));
    drop(s1);
    drop(e1);
    drop(slot);

    let (e2, _slot2) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    all_invariants(
        &s2,
        root,
        g,
        "drop_wal 실패 -> 다른 작업 공간 -> 다시 연 뒤",
    );
    assert!(s2.list_working(g).unwrap().is_empty());
}

// ---------------- T6: 되살리기가 "working copy 없음"으로 이어지지 않는다 ----------------

#[test]
fn t6_after_t2_order_reviving_leftover_never_fails_with_corrupt_state() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g, _seed) = seeded(root);
    let es7 = crash_victim_after_cas(&e1, &slot, g);
    let s1 = e1.session(USER);
    committed(commit_node(&s1, g, "w2", "next"));
    not_conflicts(&s1.commit(&es7, "again").unwrap(), "T2 순서");
    drop(s1);
    drop(e1);
    drop(slot);

    // 새 프로세스: 남은 작업본은 옛 base, 워터마크는 head
    let (e2, _slot2) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    for wid in s2.list_working(g).unwrap() {
        match s2.resume(g, wid) {
            Ok(_) | Err(EngineError::UnknownWorking) => {}
            Err(e) => panic!("되살리기가 상태 오류로 실패: {e}"),
        }
    }
    // MCP find_work 흐름: 되살릴 작업본이 없으면 새로 열어 편집·커밋한다
    committed(commit_node(&s2, g, "w7", "w7-new"));
    all_invariants(&s2, root, g, "T6");
}

// ---------------- 보수성: 반영 여부가 확실하지 않으면 버리지 않는다 ----------------

#[test]
fn leftover_with_unreflected_content_is_still_resumed_and_committable() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, _slot, g, _seed) = seeded(root);
    let s1 = e1.session(USER);
    // 커밋되지 않은 편집이 WAL에만 있다 (끊김 없음)
    let es = open_ws(&s1, g, "w7");
    es.add_node(&["T"], Some("pending"), props([("v", "p".into())]))
        .unwrap();
    let wid = es.wid();
    committed(commit_node(&s1, g, "w2", "next")); // head 전진
    drop(es);
    drop(s1);
    drop(e1);

    let (e2, _slot2) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    let es = s2
        .resume(g, wid)
        .expect("반영되지 않은 작업본은 되살아나야 함");
    let out = s2.commit(&es, "pending").unwrap();
    assert!(
        matches!(
            out,
            CommitOutcome::Committed { .. } | CommitOutcome::Rebased { .. }
        ),
        "커밋 또는 재배치여야 함: {out:?}"
    );
    assert_eq!(
        visible_slugs(&s2, root, g, "main"),
        slugs(&["seed", "next", "pending"])
    );
    all_invariants(&s2, root, g, "보수성");
}

// ---------------- 충돌 판정 불변: 같은 slug 다른 값은 여전히 충돌 ----------------

#[test]
fn real_conflicts_are_still_reported() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, _slot, g, _seed) = seeded(root);
    let s1 = e1.session(USER);
    let a = open_ws(&s1, g, "wa");
    let b = open_ws(&s1, g, "wb");
    a.add_node(&["T"], Some("dup"), props([("v", "a".into())]))
        .unwrap();
    b.add_node(&["T"], Some("dup"), props([("v", "b".into())]))
        .unwrap();
    committed(s1.commit(&a, "a").unwrap());
    let out = s1.commit(&b, "b").unwrap();
    assert!(
        matches!(out, CommitOutcome::Conflicts(_)),
        "같은 slug 다른 값은 충돌로 보고되어야 함: {out:?}"
    );
}
