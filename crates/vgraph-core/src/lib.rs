//! vgraph-core — LPG 코어 모델 + 가역 delta + canonical 직렬화/해시 + 주소.
//!
//! 설계: docs/vgraph_design_v0.md v0.1 / 인터페이스: docs/vgraph_interfaces_v0.md v0.1
//! 골든 TC: docs/vgraph_golden_tc_v0.md v0.2
//!
//! 이 크레이트는 IO가 없다. `GraphState`는 정확성 오라클(레퍼런스 모델)이며
//! 성능 최적화는 vgraph-engine(P2)의 View 소관이다.

pub mod addr;
pub mod canon;
pub mod clock;
pub mod commit;
pub mod delta;
pub mod fixtures;
pub mod id;
pub mod model;
pub mod state;
pub mod value;

pub use addr::{GraphAddr, NodeSel};
pub use canon::Hash256;
pub use clock::{Clock, SystemClock, TestClock};
pub use commit::Commit;
pub use delta::{apply, diff, Delta, DeltaBuilder, LabelsDelta, PropSet, SlugSet};
pub use id::{GraphId, IdGen, SystemIdGen, TestIdGen, Uuid, WorkingId};
pub use model::{Edge, Node};
pub use state::{Dir, GraphState, Hit, StateError};
pub use value::{Key, Props, Value};
