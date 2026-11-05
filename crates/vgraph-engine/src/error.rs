//! 오류·도메인 결과 타입 (인터페이스 §2.4·§6).
//! 원칙: 충돌은 Err가 아니라 CommitOutcome의 변형(도메인 결과)이다.

use smol_str::SmolStr;
use vgraph_core::{GraphId, Hash256, StateError, Uuid, Value, WorkingId};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ResolveError {
    #[error("unknown graph")]
    UnknownGraph,
    #[error("unknown branch")]
    UnknownBranch,
    #[error("unknown commit")]
    UnknownCommit,
    #[error("no permission")]
    NoPermission,
    #[error("bad address: {0}")]
    BadAddr(String),
    /// 레지스트리에 같은 (ns,slug)가 둘 이상 — 주소 문법 오류가 아니라 저장소 상태 문제.
    #[error("{0}")]
    AmbiguousGraph(String),
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error("store: {0}")]
    Store(#[from] vgraph_store::StoreError),
    #[error("state: {0}")]
    State(#[from] StateError),
    #[error("branch exists")]
    BranchExists,
    /// 새로 만드는 branch 이름이 규칙 위반 — 주소 해석 오류와 구분한다.
    #[error("{0}")]
    BadBranchName(String),
    /// external link 입력 오류 — 저장소 손상이 아님 (UX-2: "corrupt store" 오표현 방지).
    #[error("link: {0}")]
    Link(String),
    #[error("unknown working copy")]
    UnknownWorking,
    #[error("corrupt store: {0}")]
    Corrupt(String),
}

pub type EngineResult<T> = Result<T, EngineError>;

/// 과거 commit(비head) 편집 게이트 (설계 §4.1-5, 골든 D9).
#[derive(Debug)]
pub enum EditStartError {
    NotHead { head: Hash256, hint: EditHint },
    Engine(EngineError),
}
#[derive(Debug, PartialEq)]
pub enum EditHint {
    BranchRecommended,
}
impl From<EngineError> for EditStartError {
    fn from(e: EngineError) -> Self {
        EditStartError::Engine(e)
    }
}
impl From<ResolveError> for EditStartError {
    fn from(e: ResolveError) -> Self {
        EditStartError::Engine(e.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

/// commit 경쟁 충돌 시 ours는 미커밋 working일 수 있다 (골든 D7).
#[derive(Debug, Clone, PartialEq)]
pub enum SideRef {
    Commit(Hash256),
    Working(WorkingId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConflictItem {
    PropBothSet {
        elem: Uuid,
        key: SmolStr,
        base: Option<Value>,
        ours: Option<Value>,
        theirs: Option<Value>,
    },
    RemovedVsEdit {
        elem: Uuid,
        removed_by: Side,
    },
    DanglingEdge {
        edge: Uuid,
        missing_end: Uuid,
        removed_by: Side,
    },
    SlugCollision {
        slug: SmolStr,
        ours: Uuid,
        theirs: Uuid,
    },
    /// 병합 결과가 스키마 위반 — 각자는 valid여도 (골든 F7).
    SchemaClash {
        issue: crate::schema::SchemaIssue,
    },
    /// 충돌 검출은 통과했으나 rebase 적용이 실패한 방어적 케이스 (엔진 내부 폴백).
    ApplyFailed {
        reason: String,
    },
}

/// review_queue(ssot-engine) 입력 스키마의 엔진측 원본 (P8 접속점).
#[derive(Debug, Clone, PartialEq)]
pub struct ConflictReport {
    pub graph: GraphId,
    pub base: Option<Hash256>,
    pub ours: SideRef,
    pub theirs: SideRef,
    pub items: Vec<ConflictItem>,
}

#[derive(Debug)]
pub enum CommitOutcome {
    /// warn 모드 스키마 경고 동반 가능 (골든 H3).
    Committed {
        commit: Hash256,
        warnings: Vec<crate::schema::SchemaIssue>,
    },
    Rebased {
        onto: Hash256,
        committed: Hash256,
    },
    /// 무변경 seal — commit 미생성 (골든 D5).
    Empty,
    Conflicts(ConflictReport),
    /// strict 모드 거부 — head 불변 (골든 D8·H2).
    SchemaViolation(Vec<crate::schema::SchemaIssue>),
}

#[derive(Debug)]
pub enum MergeOutcome {
    FastForward(Hash256),
    Merged(Hash256),
    /// src가 이미 dst 계보에 포함 (골든 F12).
    UpToDate(Hash256),
    Conflicts(ConflictReport),
}

/// 충돌 해소 입력 (골든 F8). v0은 PropBothSet 항목만 해소 지원.
#[derive(Debug, Clone, Default)]
pub struct MergeOpts {
    pub resolutions: Vec<Resolution>,
}
#[derive(Debug, Clone)]
pub struct Resolution {
    /// ConflictReport.items 인덱스.
    pub item: usize,
    pub choose: Choice,
}
#[derive(Debug, Clone)]
pub enum Choice {
    Ours,
    Theirs,
    Value(Value),
}

/// external link 드리프트 (설계 §7.3, 골든 I2).
#[derive(Debug, Clone, PartialEq)]
pub struct DriftItem {
    /// "ns/slug"
    pub target: String,
    pub node: Uuid,
    pub pin: Hash256,
    /// 대상 브랜치의 현재 head — link_upgrade의 `to`로 그대로 사용 (UX-3).
    pub current_head: Hash256,
    pub pinned_hash: Hash256,
    pub current_hash: Hash256,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GraphEvent {
    HeadAdvanced {
        branch: String,
        from: Option<Hash256>,
        to: Hash256,
    },
    /// 대상 그래프 전진으로 핀과 실체가 어긋남 — 링크는 깨지지 않는다 (설계 §7.3).
    Drift { item: DriftItem },
}
