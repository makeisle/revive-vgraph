//! vgraph-engine — 캐시 계층(P2) + 쓰기 상태머신(P3).
//!
//! - View: 불변 materialized 그래프. 캐시 키 = (graph, commit) → 무효화 없음 (설계 §3.2).
//! - EditSession: on_update COW — 편집은 op-log(WAL)로 내구화, seal 시 diff→delta (설계 §4).
//! - commit: blob fsync → ref CAS 선형화 → 서로소면 auto-rebase, 겹치면 ConflictReport (설계 §4.4).
//! - 순회는 View에서만 — 디스크 계층에 hop 쿼리 없음 (§5.5 규율의 타입 강제).

pub mod engine;
pub mod error;
pub mod interchange;
pub mod ops;
pub mod provider;
pub mod review;
pub mod schema;
pub mod view;

pub use engine::{EditSession, Engine, EngineOptions, LockEntry, Session};
pub use engine::{GcPlan, GcReport};
pub use error::{
    Choice, CommitOutcome, ConflictItem, ConflictReport, DriftItem, EditHint, EditStartError,
    EngineError, EngineResult, GraphEvent, MergeOpts, MergeOutcome, Resolution, ResolveError, Side,
    SideRef,
};
pub use provider::{FsProvider, GraphStoreProvider, RegistryEntry};
pub use schema::{EdgeClassDef, NodeClassDef, Schema, SchemaIssue, SchemaMode};
pub use view::{EdgeInfo, FindMatch, FindPage, FindQuery, ScanPage, View, WalkHit, WalkQuery};
