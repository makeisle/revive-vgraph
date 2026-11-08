//! 백엔드 무관 conformance 스위트 (계획 P1.2, 인터페이스 §4).
//! SQLite/PG 등 모든 StorageBackend 구현이 동일 스위트를 통과해야 한다.

use crate::api::*;
use smol_str::SmolStr;
use std::time::Duration;
use vgraph_core::canon::sha256;
use vgraph_core::{Hash256, IdGen, TestIdGen};

/// 같은 물리 저장소를 다시 열 수 있는 팩토리 (reopen 검증용).
pub trait BackendFactory {
    type B: StorageBackend;
    fn open(&self) -> Self::B;
}

/// 전체 스위트. `mk()` 호출마다 **새 물리 저장소**의 팩토리를 만들어야 한다.
pub fn run_all<F: BackendFactory>(mk: impl Fn() -> F) {
    blob_roundtrip(&mk());
    cas_semantics(&mk());
    payload_insert_only_and_index(&mk());
    head_upsert_idempotent(&mk());
    wal_lifecycle(&mk());
    wal_quarantine(&mk());
    watch_events(&mk());
    reopen_persistence(&mk());
}

fn h(n: u8) -> Hash256 {
    sha256(&[n])
}

pub fn blob_roundtrip<F: BackendFactory>(f: &F) {
    let b = f.open();
    let hash = sha256(b"delta-bytes");
    b.put_blob(&hash, b"delta-bytes").unwrap();
    b.put_blob(&hash, b"delta-bytes").unwrap(); // 멱등
    assert_eq!(
        b.get_blob(&hash).unwrap().as_deref(),
        Some(&b"delta-bytes"[..])
    );
    assert!(b.has_blob(&hash).unwrap());
    assert_eq!(b.get_blob(&h(9)).unwrap(), None);
    assert_eq!(b.stats().unwrap().blobs, 1);
}

pub fn cas_semantics<F: BackendFactory>(f: &F) {
    let b = f.open();
    assert_eq!(b.get_ref("main").unwrap(), None);
    // 초기: expect=None
    assert_eq!(
        b.cas_ref("main", None, &h(1)).unwrap(),
        CasOutcome::Committed
    );
    // 잘못된 expect → Conflict{actual}
    assert_eq!(
        b.cas_ref("main", None, &h(2)).unwrap(),
        CasOutcome::Conflict { actual: Some(h(1)) }
    );
    assert_eq!(
        b.cas_ref("main", Some(&h(2)), &h(3)).unwrap(),
        CasOutcome::Conflict { actual: Some(h(1)) }
    );
    // 올바른 expect → 전진
    assert_eq!(
        b.cas_ref("main", Some(&h(1)), &h(2)).unwrap(),
        CasOutcome::Committed
    );
    assert_eq!(b.get_ref("main").unwrap(), Some(h(2)));
    // 브랜치 독립성
    assert_eq!(
        b.cas_ref("dev-b", None, &h(5)).unwrap(),
        CasOutcome::Committed
    );
    assert_eq!(
        b.list_refs().unwrap(),
        vec![("dev-b".to_string(), h(5)), ("main".to_string(), h(2))]
    );
}

pub fn payload_insert_only_and_index<F: BackendFactory>(f: &F) {
    let b = f.open();
    let p_active = r#"{"status":"active","text":"x"}"#;
    let p_draft = r#"{"status":"draft"}"#;
    let ha = sha256(p_active.as_bytes());
    let hd = sha256(p_draft.as_bytes());
    b.put_payload(&ha, p_active).unwrap();
    b.put_payload(&ha, p_active).unwrap(); // 멱등 (불변 row)
    b.put_payload(&hd, p_draft).unwrap();
    assert_eq!(b.get_payload(&ha).unwrap().as_deref(), Some(p_active));
    assert_eq!(b.stats().unwrap().payload_rows, 2);
    // 등가 검색: 버전 무관 hash 목록 (hash 오름차순)
    let mut expect = vec![ha];
    expect.sort();
    assert_eq!(b.scan_index("status", "\"active\"").unwrap(), expect);
    assert_eq!(
        b.scan_index("status", "\"gone\"").unwrap(),
        Vec::<Hash256>::new()
    );
}

pub fn head_upsert_idempotent<F: BackendFactory>(f: &F) {
    let b = f.open();
    let idg = TestIdGen::starting_at(50);
    let (u1, u2, e1) = (idg.next_uuid(), idg.next_uuid(), idg.next_uuid());
    let rows = LinkRows {
        upsert_nodes: vec![
            NodeRow {
                uuid: u1,
                labels: vec![SmolStr::new("A")],
                slug: Some(SmolStr::new("s1")),
                payload: h(1),
            },
            NodeRow {
                uuid: u2,
                labels: vec![SmolStr::new("B")],
                slug: None,
                payload: h(2),
            },
        ],
        upsert_edges: vec![EdgeRow {
            uuid: e1,
            etype: SmolStr::new("rel"),
            from: u1,
            to: u2,
            payload: h(3),
        }],
        adjacency: vec![(u1, vec![1, 2, 3])],
        watermark: h(7),
        ..Default::default()
    };
    b.upsert_head("main", &rows).unwrap();
    let first_nodes = b.scan_nodes("main").unwrap();
    let first_edges = b.scan_edges("main").unwrap();
    // 멱등 재적용 (골든 K5)
    b.upsert_head("main", &rows).unwrap();
    assert_eq!(b.scan_nodes("main").unwrap(), first_nodes);
    assert_eq!(b.scan_edges("main").unwrap(), first_edges);
    assert_eq!(first_nodes.len(), 2);
    assert_eq!(b.adjacency("main", u1).unwrap(), Some(vec![1, 2, 3]));
    assert_eq!(b.watermark("main").unwrap(), Some(h(7)));
    // 제거 배치
    let rm = LinkRows {
        remove_nodes: vec![u2],
        remove_edges: vec![e1],
        watermark: h(8),
        ..Default::default()
    };
    b.upsert_head("main", &rm).unwrap();
    assert_eq!(b.scan_nodes("main").unwrap().len(), 1);
    assert_eq!(b.scan_edges("main").unwrap().len(), 0);
    assert_eq!(b.watermark("main").unwrap(), Some(h(8)));
    // 브랜치 격리
    assert_eq!(b.scan_nodes("dev-b").unwrap().len(), 0);
}

pub fn wal_lifecycle<F: BackendFactory>(f: &F) {
    let b = f.open();
    let idg = TestIdGen::starting_at(90);
    let w = idg.next_working();
    assert_eq!(b.append_wal(w, "ceo", r#"{"op":"a"}"#).unwrap(), 1);
    assert_eq!(b.append_wal(w, "ceo", r#"{"op":"b"}"#).unwrap(), 2);
    assert_eq!(b.append_wal(w, "ceo", r#"{"op":"c"}"#).unwrap(), 3);
    assert_eq!(
        b.read_wal(w).unwrap(),
        vec![r#"{"op":"a"}"#, r#"{"op":"b"}"#, r#"{"op":"c"}"#]
    );
    assert_eq!(b.list_wals("ceo").unwrap(), vec![w]);
    assert_eq!(b.list_wals("guest").unwrap(), vec![]);
    b.drop_wal(w).unwrap();
    assert_eq!(b.read_wal(w).unwrap(), Vec::<String>::new());
    assert_eq!(b.list_wals("ceo").unwrap(), vec![]);
}

pub fn wal_quarantine<F: BackendFactory>(f: &F) {
    let b = f.open();
    let idg = TestIdGen::starting_at(190);
    let (bad, other, absent) = (idg.next_working(), idg.next_working(), idg.next_working());
    b.append_wal(bad, "ceo", r#"{"hdr":true}"#).unwrap();
    b.append_wal(bad, "ceo", r#"{"op":"x"}"#).unwrap();
    b.append_wal(other, "ceo", r#"{"op":"y"}"#).unwrap();
    assert_eq!(b.list_wals("ceo").unwrap(), vec![bad, other]);

    // 격리: 소유자별 목록에서 빠지고, 내용은 보존되며, 다른 작업본은 영향 없음
    assert!(b.quarantine_wal(bad).unwrap());
    assert_eq!(b.list_wals("ceo").unwrap(), vec![other]);
    assert_eq!(
        b.read_wal(bad).unwrap(),
        vec![r#"{"hdr":true}"#, r#"{"op":"x"}"#]
    );
    assert_eq!(b.read_wal(other).unwrap(), vec![r#"{"op":"y"}"#]);
    // 다른 소유자 목록에도 나타나지 않는다
    assert_eq!(b.list_wals("guest").unwrap(), vec![]);
    // 멱등: 이미 격리된 것은 변화 없음(false), 내용도 그대로
    assert!(!b.quarantine_wal(bad).unwrap());
    assert_eq!(b.read_wal(bad).unwrap().len(), 2);
    // 없는 id: 오류 아님, false
    assert!(!b.quarantine_wal(absent).unwrap());
    assert_eq!(b.list_wals("ceo").unwrap(), vec![other]);
    // 격리된 작업본도 삭제할 수 있다
    b.drop_wal(bad).unwrap();
    assert_eq!(b.read_wal(bad).unwrap(), Vec::<String>::new());
}

pub fn watch_events<F: BackendFactory>(f: &F) {
    let b = f.open();
    let rx = b.subscribe();
    b.cas_ref("main", None, &h(1)).unwrap();
    b.cas_ref("main", Some(&h(1)), &h(2)).unwrap();
    let e1 = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        e1,
        RefEvent {
            branch: "main".into(),
            from: None,
            to: h(1)
        }
    );
    let e2 = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        e2,
        RefEvent {
            branch: "main".into(),
            from: Some(h(1)),
            to: h(2)
        }
    );
}

pub fn reopen_persistence<F: BackendFactory>(f: &F) {
    let hash = sha256(b"persist");
    {
        let b = f.open();
        b.put_blob(&hash, b"persist").unwrap();
        b.cas_ref("main", None, &h(1)).unwrap();
        b.put_payload(&h(2), r#"{"k":1}"#).unwrap();
    } // drop = 연결 종료
    let b2 = f.open();
    assert_eq!(
        b2.get_blob(&hash).unwrap().as_deref(),
        Some(&b"persist"[..])
    );
    assert_eq!(b2.get_ref("main").unwrap(), Some(h(1)));
    assert_eq!(
        b2.get_payload(&h(2)).unwrap().as_deref(),
        Some(r#"{"k":1}"#)
    );
}
