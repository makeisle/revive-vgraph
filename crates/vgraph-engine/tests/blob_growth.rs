//! 블롭 증가 상한과 GC 뒤 상태 (인시던트 2026-11-07 결함 D, 선택-13).
//!
//! 제품 코드는 고치지 않는다. 실패·중단된 커밋 시도가 남기는 블롭(delta·commit)이 시도당 2개 이하로만
//! 늘고, 막힌 작업 공간이 무한히 재시도하지 않으며(시도가 정해진 결과로 끝남), 수동 `gc_run`이 고아를
//! 남김없이 회수하되 체인은 처음부터 접히는지(불변식 ③) 본다. `crash_recovery.rs`의 T1(P1 뒤 남은
//! 작업본 반복 재시도)·T4-a(P2 뒤 재커밋)와 같은 시나리오 틀이다.
//!
//! 모킹 경계: 모킹한 것은 없다. 실제 SqliteBackend(파일) 위에 고장 주입 래퍼만 얹었다 (경계 = 저장소).

mod util;

use tempfile::TempDir;
use util::*;
use vgraph_core::value::props;
use vgraph_core::*;
use vgraph_engine::*;
use vgraph_store::{BlobStore, CrashPoint, RefStore, StorageBackend};

const USER: &str = "team";
/// 충돌·재시도를 반복해 보는 횟수.
const ATTEMPTS: u64 = 10;
/// 커밋 시도 하나가 쓰는 블롭의 상한 (delta + commit).
const MAX_BLOBS_PER_ATTEMPT: u64 = 2;
/// 재기동한 엔진의 id 시작값 (첫 세대와 uuid·작업본 id가 겹치지 않게).
const GEN: u64 = 1000;

fn committed(out: CommitOutcome) -> Hash256 {
    match out {
        CommitOutcome::Committed { commit, .. } => commit,
        other => panic!("커밋 성공이어야 함: {other:?}"),
    }
}

fn seeded(root: &std::path::Path) -> (Engine, FaultSlot, GraphId) {
    let (engine, slot) = fault_engine(root, 1);
    let s = engine.session(USER);
    let g = s.create_graph("team", "g").unwrap();
    committed(commit_node(&s, g, "w0", "seed"));
    (engine, slot, g)
}

fn blob_count(root: &std::path::Path, g: GraphId) -> u64 {
    raw_backend(root, g).stats().unwrap().blobs
}

fn crash_victim_after_cas(engine: &Engine, slot: &FaultSlot, g: GraphId) -> EditSession {
    let s = engine.session(USER);
    let es = open_ws(&s, g, "w7");
    es.add_node(&["T"], Some("victim"), props([("v", "x".into())]))
        .unwrap();
    arm(slot, CrashPoint::AfterCas);
    let r = s.commit(&es, "victim");
    disarm(slot);
    assert!(r.is_err(), "AfterCas는 오류를 돌려줘야 함: {r:?}");
    es
}

fn crash_victim_after_upsert_head(engine: &Engine, slot: &FaultSlot, g: GraphId) -> EditSession {
    let s = engine.session(USER);
    let es = open_ws(&s, g, "w7");
    es.add_node(&["T"], Some("victim"), props([("v", "x".into())]))
        .unwrap();
    arm_after_upsert_head(slot);
    let r = s.commit(&es, "victim");
    disarm(slot);
    assert!(
        r.is_err(),
        "upsert_head 뒤 끊김은 오류를 돌려줘야 함: {r:?}"
    );
    es
}

/// 같은 base에서 wa가 먼저 커밋하고, 같은 slug 다른 값을 가진 wb가 `n`번 충돌 시도한다.
/// 시도마다 블롭 증가량을 돌려준다 (메시지를 달리해 commit 블롭 해시가 겹치지 않게 한다).
fn conflict_attempts(s: &Session, root: &std::path::Path, g: GraphId, n: u64) -> Vec<u64> {
    let a = open_ws(s, g, "wa");
    let b = open_ws(s, g, "wb");
    a.add_node(&["T"], Some("dup"), props([("v", "a".into())]))
        .unwrap();
    b.add_node(&["T"], Some("dup"), props([("v", "b".into())]))
        .unwrap();
    committed(s.commit(&a, "a").unwrap());
    let mut prev = blob_count(root, g);
    let mut grown = Vec::new();
    for i in 0..n {
        let out = s.commit(&b, &format!("b{i}")).unwrap();
        assert!(
            matches!(out, CommitOutcome::Conflicts(_)),
            "시도 {i}: 충돌로 끝나야 함(무한 재시도 없음): {out:?}"
        );
        let now = blob_count(root, g);
        grown.push(now - prev);
        prev = now;
    }
    grown
}

// ---------------- D-1: 충돌하는 커밋 시도의 블롭 증가 ----------------

/// 관측: 첫 시도 +2(delta·commit), 이후 +1(delta는 같은 해시로 재사용, commit만 새로 생김).
/// 모든 시도가 `Conflicts`로 끝나고 head는 움직이지 않는다.
#[test]
fn d1_repeated_real_conflicts_grow_at_most_two_blobs_per_attempt() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e, _slot, g) = seeded(root);
    let s = e.session(USER);

    let grown = conflict_attempts(&s, root, g, ATTEMPTS);
    eprintln!("[D-1 충돌] 시도별 블롭 증가: {grown:?}");
    assert!(
        grown.iter().all(|&x| x <= MAX_BLOBS_PER_ATTEMPT),
        "시도당 상한 초과: {grown:?}"
    );
    let mut expected = vec![1; ATTEMPTS as usize];
    expected[0] = 2;
    assert_eq!(grown, expected, "관측: 첫 시도 +2, 이후 +1");
    assert!(check_invariants(root, g, "main", true).is_ok());
}

/// T1 연결: P1(AfterCas) 뒤 남은 작업본이 반복 재시도해도 충돌로 막히지 않고 블롭이 늘지 않는다
/// (재시도마다 `Empty` — 이미 체인에 있는 작업본은 버려진다).
#[test]
fn d1_p1_leftover_retries_do_not_block_and_add_no_blobs() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g) = seeded(root);
    let _stale = crash_victim_after_cas(&e1, &slot, g);
    let after_crash = blob_count(root, g);
    drop(e1);
    drop(slot);

    let (e2, _slot2) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    for i in 0..ATTEMPTS {
        let es = open_ws(&s2, g, "w7");
        let out = s2.commit(&es, &format!("retry{i}")).unwrap();
        assert!(
            matches!(out, CommitOutcome::Empty),
            "재시도 {i}: 막힘 없이 Empty로 끝나야 함(관측): {out:?}"
        );
    }
    assert_eq!(
        blob_count(root, g),
        after_crash,
        "관측: P1 뒤 재시도는 블롭을 만들지 않는다"
    );
    assert!(check_no_reflected_leftover(&s2, root, g, "main").is_ok());
    assert!(check_invariants(root, g, "main", true).is_ok());
}

/// T4-a 연결: P2(AfterUpsertHead) 뒤 같은 (낡은) 핸들로 재커밋을 반복하면 매번 `Empty`로 끝나지만
/// commit 블롭이 호출마다 +1 고아로 남는다 (관측). 호출당 상한(2) 안이고, 정상 경로(find_work가
/// 새 작업본을 돌려줌)에는 이 반복이 없다.
#[test]
fn d1_p2_stale_handle_retries_end_empty_and_add_one_blob_each() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g) = seeded(root);
    let es7 = crash_victim_after_upsert_head(&e1, &slot, g);
    let s1 = e1.session(USER);

    let mut prev = blob_count(root, g);
    let mut grown = Vec::new();
    for i in 0..ATTEMPTS {
        let out = s1.commit(&es7, &format!("retry{i}")).unwrap();
        assert!(matches!(out, CommitOutcome::Empty), "재시도 {i}: {out:?}");
        let now = blob_count(root, g);
        grown.push(now - prev);
        prev = now;
    }
    eprintln!("[D-1 P2] 재시도별 블롭 증가: {grown:?}");
    assert!(grown.iter().all(|&x| x <= MAX_BLOBS_PER_ATTEMPT));
    assert_eq!(
        grown,
        vec![1; ATTEMPTS as usize],
        "관측: 호출마다 commit 블롭 +1"
    );
}

// ---------------- D-1 공유 블롭 (감사 "확인 필요") ----------------

/// 관측: P2 뒤 편집 없이 재커밋하면 작업본의 delta 해시가 체인의 기존 delta와 같다 → delta 블롭은 새로
/// 생기지 않고 기존 것을 재사용한다. commit 블롭만 +1 (부모·시각·메시지가 달라 해시가 다르다).
/// 따라서 GC가 이 고아 commit을 지워도 체인이 쓰는 delta는 `keep`에 남는다.
#[test]
fn p2_recommit_reuses_existing_delta_blob() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e1, slot, g) = seeded(root);
    let es7 = crash_victim_after_upsert_head(&e1, &slot, g);
    let s1 = e1.session(USER);

    let b = raw_backend(root, g);
    let head = b.get_ref("main").unwrap().unwrap();
    let chain = first_parent_chain(&b, head).unwrap();
    let chain_deltas: Vec<Hash256> = chain.iter().map(|c| c.delta).collect();
    let pending = es7.pending().hash();
    eprintln!("[공유 블롭] 작업본 delta {pending} / 체인 delta {chain_deltas:?}");
    assert!(
        chain_deltas.contains(&pending),
        "관측: 재커밋 delta 해시는 체인의 기존 delta와 같다"
    );
    let before = blob_count(root, g);

    let out = s1.commit(&es7, "recommit").unwrap();
    assert!(matches!(out, CommitOutcome::Empty), "{out:?}");
    assert_eq!(blob_count(root, g), before + 1, "관측: commit 블롭만 +1");
    assert!(b.has_blob(&pending).unwrap());

    // GC 뒤에도 공유 delta는 남고 체인은 접힌다.
    let report = s1.gc_run(g).unwrap();
    eprintln!("[공유 블롭] gc_run: {report:?}");
    assert_eq!(report.removed_blobs, 1, "고아 commit 블롭 하나만 회수");
    assert!(raw_backend(root, g).has_blob(&pending).unwrap());
    assert!(check_invariants(root, g, "main", true).is_ok());
}

// ---------------- GC 뒤 고아 0 + 체인 접기 ----------------

/// 충돌 시도들이 남긴 블롭은 전부 고아이고, `gc_run`이 모두 회수한다. 회수 뒤 고아 0, 블롭 수는
/// 시도 전으로 돌아가며 head 체인은 처음부터 접힌다(불변식 ③) — INV-007: 체인 블롭은 지워지지 않는다.
#[test]
fn gc_run_removes_all_orphans_and_chain_still_folds() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e, _slot, g) = seeded(root);
    let s = e.session(USER);

    let a = open_ws(&s, g, "wa");
    let b = open_ws(&s, g, "wb");
    a.add_node(&["T"], Some("dup"), props([("v", "a".into())]))
        .unwrap();
    b.add_node(&["T"], Some("dup"), props([("v", "b".into())]))
        .unwrap();
    committed(s.commit(&a, "a").unwrap());
    let baseline = blob_count(root, g);
    for i in 0..ATTEMPTS {
        assert!(matches!(
            s.commit(&b, &format!("b{i}")).unwrap(),
            CommitOutcome::Conflicts(_)
        ));
    }
    let grown = blob_count(root, g) - baseline;

    let plan = s.gc_plan(g).unwrap();
    eprintln!("[GC] 증가 {grown} / 회수 대상 {}", plan.orphan_blobs.len());
    assert_eq!(
        plan.orphan_blobs.len() as u64,
        grown,
        "시도가 남긴 블롭은 전부 고아"
    );

    let report = s.gc_run(g).unwrap();
    assert_eq!(report.removed_blobs, grown);
    assert_eq!(blob_count(root, g), baseline, "회수 뒤 시도 전 블롭 수");
    assert!(s.gc_plan(g).unwrap().orphan_blobs.is_empty(), "고아 0");
    assert!(
        check_invariants(root, g, "main", true).is_ok(),
        "처음부터 접기 성공(불변식 ③)"
    );
    let head = raw_backend(root, g).get_ref("main").unwrap().unwrap();
    assert!(fold_from_blobs(&raw_backend(root, g), head).is_ok());
}

// ---------------- D-2: GC 전후 .db 파일 크기 (측정만) ----------------

/// VACUUM 정책은 바꾸지 않는다. 크기는 출력만 하고 단언하지 않는다.
/// WAL 모드라 엔진이 연결을 쥔 동안에는 `.db` 본 파일이 실제 크기를 반영하지 않는다. 그래서 엔진을
/// 닫아 체크포인트가 일어난 뒤의 크기도 함께 잰다.
#[test]
fn d2_measure_db_file_size_around_gc_run() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let (e, slot, g) = seeded(root);
    let attempts = 200;
    let _ = conflict_attempts(&e.session(USER), root, g, attempts);
    let size = |p: &std::path::Path| std::fs::metadata(p).map_or(0, |m| m.len());
    let db = root.join(format!("{g}.db"));
    let wal = root.join(format!("{g}.db-wal"));
    let open_before = (size(&db), size(&wal));
    drop(e);
    drop(slot);
    let closed_before = (size(&db), size(&wal));

    let (e2, _slot2) = fault_engine(root, GEN);
    let s2 = e2.session(USER);
    let blobs_before = blob_count(root, g);
    let report = s2.gc_run(g).unwrap();
    let open_after = (size(&db), size(&wal));
    let blobs_after = blob_count(root, g);
    drop(s2);
    drop(e2);
    drop(_slot2);
    let closed_after = (size(&db), size(&wal));
    eprintln!(
        "[D-2] 충돌 시도 {attempts}회 / gc_run 회수 {report:?} / 블롭 {blobs_before} -> {blobs_after}
         [D-2] (.db, .db-wal) 바이트: 엔진 열린 채 전 {open_before:?} / 닫은 뒤 전 {closed_before:?} / gc 직후(열린 채) {open_after:?} / 닫은 뒤 후 {closed_after:?}"
    );
}
