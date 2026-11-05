//! P1 골든 게이트: K3(크래시 3지점), K5(upsert 멱등), 통합 스모크(F0 저장→replay≡F0_DUMP).

use std::path::PathBuf;
use tempfile::TempDir;
use vgraph_core::canon::{payload_hash, props_json};
use vgraph_core::fixtures::{build_f0, F0};
use vgraph_core::*;
use vgraph_store::*;

fn temp() -> (TempDir, PathBuf) {
    let d = TempDir::new().unwrap();
    let p = d.path().join("g.db");
    (d, p)
}

/// 엔진(P3) seal 경로의 store 수준 스크립트: delta blob → commit blob → ref CAS.
fn commit_seq<B: StorageBackend>(b: &B, f: &F0) -> StoreResult<CasOutcome> {
    b.put_blob(&f.delta.hash(), f.delta.canonical_json().as_bytes())?;
    b.put_blob(&f.c0, f.commit.canonical_json().as_bytes())?;
    b.cas_ref("main", None, &f.c0)
}

fn link_rows_of(state: &GraphState, watermark: Hash256) -> LinkRows {
    LinkRows {
        upsert_nodes: state
            .nodes()
            .map(|n| NodeRow {
                uuid: n.uuid,
                labels: n.labels.clone(),
                slug: n.slug.clone(),
                payload: payload_hash(&n.props),
            })
            .collect(),
        upsert_edges: state
            .edges()
            .map(|e| EdgeRow {
                uuid: e.uuid,
                etype: e.etype.clone(),
                from: e.from,
                to: e.to,
                payload: payload_hash(&e.props),
            })
            .collect(),
        watermark,
        ..Default::default()
    }
}

#[test]
fn golden_k3_crash_before_delta_fsync() {
    let (_d, p) = temp();
    let f0 = build_f0();
    let fb = FaultBackend::new(SqliteBackend::open(&p).unwrap());
    fb.arm(CrashPoint::BeforeDeltaFsync);
    let err = commit_seq(&fb, &f0).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Crashed(CrashPoint::BeforeDeltaFsync)
    ));
    drop(fb); // 크래시 = 연결 소멸
    let b = SqliteBackend::open(&p).unwrap();
    assert_eq!(b.get_ref("main").unwrap(), None); // head 불변
    assert_eq!(b.stats().unwrap().blobs, 0); // 아무것도 안 써짐
}

#[test]
fn golden_k3_crash_after_delta_before_cas() {
    let (_d, p) = temp();
    let f0 = build_f0();
    let fb = FaultBackend::new(SqliteBackend::open(&p).unwrap());
    fb.arm(CrashPoint::AfterDeltaBeforeCas);
    let err = commit_seq(&fb, &f0).unwrap_err();
    assert!(matches!(
        err,
        StoreError::Crashed(CrashPoint::AfterDeltaBeforeCas)
    ));
    drop(fb);
    let b = SqliteBackend::open(&p).unwrap();
    assert_eq!(b.get_ref("main").unwrap(), None); // head 불변 = 일관 상태
    assert_eq!(b.stats().unwrap().blobs, 2); // 고아 blob → GC(P7) 대상
}

#[test]
fn golden_k3_crash_after_cas() {
    let (_d, p) = temp();
    let f0 = build_f0();
    let fb = FaultBackend::new(SqliteBackend::open(&p).unwrap());
    fb.arm(CrashPoint::AfterCas);
    let err = commit_seq(&fb, &f0).unwrap_err();
    assert!(matches!(err, StoreError::Crashed(CrashPoint::AfterCas)));
    drop(fb);
    let b = SqliteBackend::open(&p).unwrap();
    assert_eq!(b.get_ref("main").unwrap(), Some(f0.c0)); // CAS는 내구성 있게 완료
    assert!(b.has_blob(&f0.c0).unwrap());
}

#[test]
fn golden_k5_upsert_idempotent() {
    let (_d, p) = temp();
    let f0 = build_f0();
    let b = SqliteBackend::open(&p).unwrap();
    let rows = link_rows_of(&f0.state, f0.c0);
    b.upsert_head("main", &rows).unwrap();
    let n1 = b.scan_nodes("main").unwrap();
    let e1 = b.scan_edges("main").unwrap();
    b.upsert_head("main", &rows).unwrap(); // 재적용 = replay 안전
    assert_eq!(b.scan_nodes("main").unwrap(), n1);
    assert_eq!(b.scan_edges("main").unwrap(), e1);
    assert_eq!(b.watermark("main").unwrap(), Some(f0.c0));
}

/// 통합 스모크 (계획 P1.5): F0 chain 저장 → 재열기 → log replay → GraphState ≡ F0_DUMP.
#[test]
fn golden_smoke_f0_store_replay() {
    let (_d, p) = temp();
    let f0 = build_f0();
    {
        let b = SqliteBackend::open(&p).unwrap();
        // 소스 계층: 불변 payload row (content-hash 키 → 자동 dedup)
        for n in f0.state.nodes() {
            b.put_payload(&payload_hash(&n.props), &props_json(&n.props))
                .unwrap();
        }
        for e in f0.state.edges() {
            b.put_payload(&payload_hash(&e.props), &props_json(&e.props))
                .unwrap();
        }
        // 권위 계층: delta/commit blob + ref CAS
        assert_eq!(commit_seq(&b, &f0).unwrap(), CasOutcome::Committed);
        // 연결 계층: head 표현 + 워터마크
        b.upsert_head("main", &link_rows_of(&f0.state, f0.c0))
            .unwrap();
    } // 종료

    // 재열기 → 권위 계층에서 재구성 (설계 §6-1: 파생물은 언제든 재구축 가능)
    let b = SqliteBackend::open(&p).unwrap();
    let head = b.get_ref("main").unwrap().expect("head");
    assert_eq!(head, f0.c0);
    let commit_bytes = b.get_blob(&head).unwrap().expect("commit blob");
    let commit = Commit::from_json(std::str::from_utf8(&commit_bytes).unwrap()).unwrap();
    assert_eq!(commit.hash(), f0.c0); // content-addressed 검증
    let delta_bytes = b.get_blob(&commit.delta).unwrap().expect("delta blob");
    let delta = Delta::from_json(std::str::from_utf8(&delta_bytes).unwrap()).unwrap();
    assert_eq!(delta.hash(), commit.delta);

    let mut replayed = GraphState::new();
    apply(&mut replayed, &delta).unwrap();
    assert_eq!(replayed, f0.state);
    assert_eq!(
        replayed.dump(Some(&f0.c0)),
        f0.state.dump(Some(&f0.c0)) // ≡ F0_DUMP (core golden lock과 동일)
    );

    // head 표현 확인
    assert_eq!(b.scan_nodes("main").unwrap().len(), 5);
    assert_eq!(b.scan_edges("main").unwrap().len(), 4);
    assert_eq!(b.watermark("main").unwrap(), Some(f0.c0));

    // 구조적 공유 (설계 §5.4 dedup): 노드 5종 + 엣지 {weight} 1종 + 빈 props 1종(e1·e2·e4 공유) = 7
    assert_eq!(b.stats().unwrap().payload_rows, 7);

    // 소스 계층 등가 검색 → payload hash 목록 (epoch 조인은 엔진 소관)
    let active_hashes = b.scan_index("status", "\"active\"").unwrap();
    assert_eq!(active_hashes.len(), 3); // n1, n2, n4의 payload
}
