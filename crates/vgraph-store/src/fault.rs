//! FaultBackend — 크래시 시뮬레이션 래퍼 (계획 D7, 골든 K3).
//!
//! commit 시퀀스 [put_blob(delta), put_blob(commit), cas_ref] 기준:
//! - BeforeDeltaFsync    → 첫 put_blob에서 크래시 (아무것도 안 씀)
//! - AfterDeltaBeforeCas → blob은 써지고 cas_ref 직전 크래시
//! - AfterCas            → cas는 적용되고 응답 직전 크래시
//!
//! 논리 절단 시뮬레이션 — 물리 fsync 검증은 P3의 프로세스 kill E2E 소관 (계획 §5).

use crate::api::*;
use std::sync::mpsc::Receiver;
use std::sync::Mutex;
use vgraph_core::{Hash256, Uuid, WorkingId};

pub struct FaultBackend<B> {
    inner: B,
    crash: Mutex<Option<CrashPoint>>,
}

impl<B> FaultBackend<B> {
    pub fn new(inner: B) -> Self {
        FaultBackend {
            inner,
            crash: Mutex::new(None),
        }
    }
    pub fn arm(&self, p: CrashPoint) {
        *self.crash.lock().unwrap() = Some(p);
    }
    pub fn disarm(&self) {
        *self.crash.lock().unwrap() = None;
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
        self.inner.upsert_head(branch, rows)
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
        self.inner.drop_wal(w)
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
