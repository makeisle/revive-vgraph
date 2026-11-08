//! FaultBackend — 크래시 시뮬레이션 래퍼 (계획 D7, 골든 K3).
//!
//! commit 시퀀스 [put_blob(delta), put_blob(commit), cas_ref] 기준:
//! - BeforeDeltaFsync    → 첫 put_blob에서 크래시 (아무것도 안 씀)
//! - AfterDeltaBeforeCas → blob은 써지고 cas_ref 직전 크래시
//! - AfterCas            → cas는 적용되고 응답 직전 크래시
//!
//! `CrashPoint`(api.rs) 밖의 고장 지점 둘은 별도 무장 함수로 둔다 (`CrashPoint` 변경은 이 모듈 범위 밖):
//! - arm_after_upsert_head → upsert_head를 적용한 뒤 오류 반환 (발행 도중 끊김, P2)
//! - arm_drop_wal_fails    → drop_wal이 오류 반환 (작업본 정리 실패, H-C4)
//!
//! 논리 절단 시뮬레이션 — 물리 fsync 검증은 P3의 프로세스 kill E2E 소관 (계획 §5).

use crate::api::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Mutex;
use vgraph_core::{Hash256, Uuid, WorkingId};

pub struct FaultBackend<B> {
    inner: B,
    crash: Mutex<Option<CrashPoint>>,
    after_upsert_head: AtomicBool,
    drop_wal_fails: AtomicBool,
}

impl<B> FaultBackend<B> {
    pub fn new(inner: B) -> Self {
        FaultBackend {
            inner,
            crash: Mutex::new(None),
            after_upsert_head: AtomicBool::new(false),
            drop_wal_fails: AtomicBool::new(false),
        }
    }
    pub fn arm(&self, p: CrashPoint) {
        *self.crash.lock().unwrap() = Some(p);
    }
    /// 다음 `upsert_head`부터 적용은 하되 오류를 돌려준다 (`disarm`까지).
    pub fn arm_after_upsert_head(&self) {
        self.after_upsert_head.store(true, Ordering::SeqCst);
    }
    /// `drop_wal`이 오류를 돌려준다 (`disarm`까지). 작업본은 지워지지 않는다.
    pub fn arm_drop_wal_fails(&self) {
        self.drop_wal_fails.store(true, Ordering::SeqCst);
    }
    pub fn disarm(&self) {
        *self.crash.lock().unwrap() = None;
        self.after_upsert_head.store(false, Ordering::SeqCst);
        self.drop_wal_fails.store(false, Ordering::SeqCst);
    }
    pub fn into_inner(self) -> B {
        self.inner
    }
    fn armed(&self) -> Option<CrashPoint> {
        *self.crash.lock().unwrap()
    }
}

impl<B: BlobStore> BlobStore for FaultBackend<B> {
    fn put_blob(&self, hash: &Hash256, bytes: &[u8]) -> StoreResult<()> {
        if self.armed() == Some(CrashPoint::BeforeDeltaFsync) {
            return Err(StoreError::Crashed(CrashPoint::BeforeDeltaFsync));
        }
        self.inner.put_blob(hash, bytes)
    }
    fn get_blob(&self, hash: &Hash256) -> StoreResult<Option<Vec<u8>>> {
        self.inner.get_blob(hash)
    }
    fn has_blob(&self, hash: &Hash256) -> StoreResult<bool> {
        self.inner.has_blob(hash)
    }
    fn list_blobs(&self) -> StoreResult<Vec<Hash256>> {
        self.inner.list_blobs()
    }
    fn delete_blob(&self, hash: &Hash256) -> StoreResult<()> {
        self.inner.delete_blob(hash)
    }
}

impl<B: RefStore> RefStore for FaultBackend<B> {
    fn cas_ref(
        &self,
        branch: &str,
        expect: Option<&Hash256>,
        new: &Hash256,
    ) -> StoreResult<CasOutcome> {
        match self.armed() {
            Some(CrashPoint::AfterDeltaBeforeCas) => {
                Err(StoreError::Crashed(CrashPoint::AfterDeltaBeforeCas))
            }
            Some(CrashPoint::AfterCas) => {
                // CAS는 내구성 있게 적용된 뒤 응답만 유실
                let _ = self.inner.cas_ref(branch, expect, new)?;
                Err(StoreError::Crashed(CrashPoint::AfterCas))
            }
            _ => self.inner.cas_ref(branch, expect, new),
        }
    }
    fn get_ref(&self, branch: &str) -> StoreResult<Option<Hash256>> {
        self.inner.get_ref(branch)
    }
    fn list_refs(&self) -> StoreResult<Vec<(String, Hash256)>> {
        self.inner.list_refs()
    }
}

impl<B: PayloadStore> PayloadStore for FaultBackend<B> {
    fn put_payload(&self, hash: &Hash256, props_json: &str) -> StoreResult<()> {
        self.inner.put_payload(hash, props_json)
    }
    fn get_payload(&self, hash: &Hash256) -> StoreResult<Option<String>> {
        self.inner.get_payload(hash)
    }
    fn scan_index(&self, key: &str, value_json: &str) -> StoreResult<Vec<Hash256>> {
        self.inner.scan_index(key, value_json)
    }
    fn list_payloads(&self) -> StoreResult<Vec<Hash256>> {
        self.inner.list_payloads()
    }
    fn delete_payload(&self, hash: &Hash256) -> StoreResult<()> {
        self.inner.delete_payload(hash)
    }
}

impl<B: HeadProvision> HeadProvision for FaultBackend<B> {
    fn upsert_head(&self, branch: &str, rows: &LinkRows) -> StoreResult<()> {
        self.inner.upsert_head(branch, rows)?;
        if self.after_upsert_head.load(Ordering::SeqCst) {
            // 적용은 내구성 있게 끝난 뒤 응답만 유실
            return Err(StoreError::Backend(
                "injected: crash after upsert_head".into(),
            ));
        }
        Ok(())
    }
    fn scan_nodes(&self, branch: &str) -> StoreResult<Vec<NodeRow>> {
        self.inner.scan_nodes(branch)
    }
    fn scan_edges(&self, branch: &str) -> StoreResult<Vec<EdgeRow>> {
        self.inner.scan_edges(branch)
    }
    fn adjacency(&self, branch: &str, uuid: Uuid) -> StoreResult<Option<Vec<u8>>> {
        self.inner.adjacency(branch, uuid)
    }
    fn watermark(&self, branch: &str) -> StoreResult<Option<Hash256>> {
        self.inner.watermark(branch)
    }
    fn set_watermark(&self, branch: &str, commit: &Hash256) -> StoreResult<()> {
        self.inner.set_watermark(branch, commit)
    }
    fn reset_derived(&self) -> StoreResult<()> {
        self.inner.reset_derived()
    }
}

impl<B: WalStore> WalStore for FaultBackend<B> {
    fn append_wal(&self, w: WorkingId, owner: &str, op_json: &str) -> StoreResult<u64> {
        self.inner.append_wal(w, owner, op_json)
    }
    fn read_wal(&self, w: WorkingId) -> StoreResult<Vec<String>> {
        self.inner.read_wal(w)
    }
    fn list_wals(&self, owner: &str) -> StoreResult<Vec<WorkingId>> {
        self.inner.list_wals(owner)
    }
    fn drop_wal(&self, w: WorkingId) -> StoreResult<()> {
        if self.drop_wal_fails.load(Ordering::SeqCst) {
            return Err(StoreError::Backend("injected: drop_wal failed".into()));
        }
        self.inner.drop_wal(w)
    }
    fn quarantine_wal(&self, w: WorkingId) -> StoreResult<bool> {
        self.inner.quarantine_wal(w)
    }
}

impl<B: RefWatch> RefWatch for FaultBackend<B> {
    fn subscribe(&self) -> Receiver<RefEvent> {
        self.inner.subscribe()
    }
}

impl<B: StorageBackend> StorageBackend for FaultBackend<B> {
    fn stats(&self) -> StoreResult<StorageStats> {
        self.inner.stats()
    }
}
