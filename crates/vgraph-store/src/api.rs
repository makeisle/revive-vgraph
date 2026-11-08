//! StorageBackend 6-trait 분해 (인터페이스 §4). 핸들 = 그래프당 1개 (설계 §10.2).

use smol_str::SmolStr;
use std::sync::mpsc::Receiver;
use vgraph_core::{Hash256, Uuid, WorkingId};

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("backend: {0}")]
    Backend(String),
    /// FaultBackend가 주입하는 크래시 시뮬레이션 (계획 D7).
    #[error("crash injected at {0:?}")]
    Crashed(CrashPoint),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Backend(e.to_string())
    }
}

/// 골든 K3의 3지점 (인터페이스 §3.4 CrashPoint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    BeforeDeltaFsync,
    AfterDeltaBeforeCas,
    AfterCas,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CasOutcome {
    Committed,
    Conflict { actual: Option<Hash256> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RefEvent {
    pub branch: String,
    pub from: Option<Hash256>,
    pub to: Hash256,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeRow {
    pub uuid: Uuid,
    pub labels: Vec<SmolStr>,
    pub slug: Option<SmolStr>,
    pub payload: Hash256,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeRow {
    pub uuid: Uuid,
    pub etype: SmolStr,
    pub from: Uuid,
    pub to: Uuid,
    pub payload: Hash256,
}

/// head 표현 증분 배치 — 하나의 트랜잭션으로 적용, 멱등 (골든 K5).
#[derive(Debug, Clone, Default)]
pub struct LinkRows {
    pub upsert_nodes: Vec<NodeRow>,
    pub remove_nodes: Vec<Uuid>,
    pub upsert_edges: Vec<EdgeRow>,
    pub remove_edges: Vec<Uuid>,
    /// (노드, 인접 블록). 블록 포맷은 엔진 소관 (P2에서 CSR).
    pub adjacency: Vec<(Uuid, Vec<u8>)>,
    pub remove_adjacency: Vec<Uuid>,
    /// 배치 적용 완료 시점의 applied_commit (설계 §6-4).
    pub watermark: Hash256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageStats {
    pub blobs: u64,
    pub payload_rows: u64,
}

// ---------- 권위 계층 ----------

pub trait BlobStore {
    /// content-addressed·멱등. commit 경로에서는 내구성 보장(fsync) 후 반환.
    fn put_blob(&self, hash: &Hash256, bytes: &[u8]) -> StoreResult<()>;
    fn get_blob(&self, hash: &Hash256) -> StoreResult<Option<Vec<u8>>>;
    fn has_blob(&self, hash: &Hash256) -> StoreResult<bool>;
    /// GC 전용 (P7) — mark&sweep의 sweep 재료.
    fn list_blobs(&self) -> StoreResult<Vec<Hash256>>;
    fn delete_blob(&self, hash: &Hash256) -> StoreResult<()>;
}

pub trait RefStore {
    /// 유일한 선형화 지점. expect 불일치 = Conflict{actual} (오류 아님).
    fn cas_ref(
        &self,
        branch: &str,
        expect: Option<&Hash256>,
        new: &Hash256,
    ) -> StoreResult<CasOutcome>;
    fn get_ref(&self, branch: &str) -> StoreResult<Option<Hash256>>;
    fn list_refs(&self) -> StoreResult<Vec<(String, Hash256)>>;
}

// ---------- 파생 계층 ----------

pub trait PayloadStore {
    /// 불변 row·INSERT만·멱등 (설계 §5.4).
    fn put_payload(&self, hash: &Hash256, props_json: &str) -> StoreResult<()>;
    fn get_payload(&self, hash: &Hash256) -> StoreResult<Option<String>>;
    /// 버전 무관 hash 목록 (등가 검색). epoch 조인은 호출측(뷰) 책임 (인터페이스 §2.1).
    fn scan_index(&self, key: &str, value_json: &str) -> StoreResult<Vec<Hash256>>;
    /// GC 전용 (P7).
    fn list_payloads(&self) -> StoreResult<Vec<Hash256>>;
    fn delete_payload(&self, hash: &Hash256) -> StoreResult<()>;
}

pub trait HeadProvision {
    /// 트랜잭션 적용 + 워터마크 전진. 재적용 안전 (골든 K5).
    fn upsert_head(&self, branch: &str, rows: &LinkRows) -> StoreResult<()>;
    fn scan_nodes(&self, branch: &str) -> StoreResult<Vec<NodeRow>>;
    fn scan_edges(&self, branch: &str) -> StoreResult<Vec<EdgeRow>>;
    fn adjacency(&self, branch: &str, uuid: Uuid) -> StoreResult<Option<Vec<u8>>>;
    fn watermark(&self, branch: &str) -> StoreResult<Option<Hash256>>;
    /// 테스트 훅(set_watermark 지연 시뮬레이션)용 — 프로덕션 경로는 upsert_head가 전진시킨다.
    fn set_watermark(&self, branch: &str, commit: &Hash256) -> StoreResult<()>;
    /// 파생 계층 전체 초기화 — 재구축 가능성(설계 §6-1)의 실행 수단 (골든 K4).
    fn reset_derived(&self) -> StoreResult<()>;
}

// ---------- working ----------

pub trait WalStore {
    /// 반환 = 부여된 seq (1부터).
    fn append_wal(&self, w: WorkingId, owner: &str, op_json: &str) -> StoreResult<u64>;
    fn read_wal(&self, w: WorkingId) -> StoreResult<Vec<String>>;
    fn list_wals(&self, owner: &str) -> StoreResult<Vec<WorkingId>>;
    fn drop_wal(&self, w: WorkingId) -> StoreResult<()>;
    /// 되살릴 수 없는 작업본을 소유자별 목록(`list_wals`)에서 빼되 내용은 보존한다 (격리).
    /// `read_wal(w)`은 격리 뒤에도 같은 줄을 돌려준다. 반환 = 이번 호출로 격리됐으면 `true`
    /// (없는 id이거나 이미 격리돼 있으면 `false` — 오류 아님, 멱등). 스키마·내용은 바꾸지 않는다.
    fn quarantine_wal(&self, w: WorkingId) -> StoreResult<bool>;
}

// ---------- 구독 ----------

pub trait RefWatch {
    /// head 전진 통지. 핸들 내(in-process) 브로드캐스트 — PG 백엔드는 LISTEN/NOTIFY.
    fn subscribe(&self) -> Receiver<RefEvent>;
}

pub trait StorageBackend:
    BlobStore + RefStore + PayloadStore + HeadProvision + WalStore + RefWatch
{
    fn stats(&self) -> StoreResult<StorageStats>;
}
