//! vgraph-store — StorageBackend 공급 인터페이스 (인터페이스 §4) + SQLite 백엔드.
//!
//! 계약 요약:
//! - Blob/Payload: content-addressed·불변·멱등 INSERT (UPDATE 없음).
//! - Ref: CAS가 유일한 갱신 경로 = 시스템의 유일한 선형화 지점 (설계 §6-3).
//! - HeadProvision: 워터마크-근사 파생 표현. 벌크 scan·점조회만 — hop 반복 서빙 금지.
//! - 파생 계층은 log replay로 재구축 가능해야 한다 (conformance가 검증).

pub mod api;
pub mod conformance;
pub mod fault;
pub mod sqlite;

pub use api::*;
pub use fault::FaultBackend;
pub use sqlite::SqliteBackend;
