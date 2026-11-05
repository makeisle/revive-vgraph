//! P1 게이트: SQLite 백엔드 conformance (계획 P1.4).

use std::path::PathBuf;
use tempfile::TempDir;
use vgraph_store::conformance::{run_all, BackendFactory};
use vgraph_store::SqliteBackend;

struct TempFactory {
    _dir: TempDir,
    path: PathBuf,
}

impl TempFactory {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("g.db");
        TempFactory { _dir: dir, path }
    }
}

impl BackendFactory for TempFactory {
    type B = SqliteBackend;
    fn open(&self) -> SqliteBackend {
        SqliteBackend::open(&self.path).unwrap()
    }
}

#[test]
fn sqlite_conformance() {
    run_all(TempFactory::new);
}
