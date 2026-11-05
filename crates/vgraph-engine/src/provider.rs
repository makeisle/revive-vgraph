//! 그래프 저장소 공급자 — 그래프당 백엔드 1개 (설계 §10.2) + 영속 레지스트리.

use smol_str::SmolStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vgraph_core::GraphId;
use vgraph_store::{SqliteBackend, StorageBackend, StoreError, StoreResult};

#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub id: GraphId,
    pub ns: SmolStr,
    pub slug: SmolStr,
    pub owner: String,
}

pub trait GraphStoreProvider: Send + Sync {
    fn open_graph(&self, id: GraphId) -> StoreResult<Arc<dyn StorageBackend + Send + Sync>>;
    fn registry(&self) -> StoreResult<Vec<RegistryEntry>>;
    fn register(&self, e: &RegistryEntry) -> StoreResult<()>;

    /// 같은 (ns, slug)가 아직 없을 때만 등록한다. 등록했으면 `true`, 이미 있으면 `false`.
    ///
    /// 기본 구현은 "읽고 → 등록"이라 여러 프로세스가 같은 순간 부르면 둘 다 성공할 수 있다.
    /// 프로세스 간 공유 저장소를 쓰는 공급자는 이 메서드를 덮어써 두 단계를 원자적으로 묶어야 한다.
    fn register_new(&self, e: &RegistryEntry) -> StoreResult<bool> {
        if self
            .registry()?
            .iter()
            .any(|r| r.ns == e.ns && r.slug == e.slug)
        {
            return Ok(false);
        }
        self.register(e)?;
        Ok(true)
    }
}

/// 만들기 구간(읽기 → 이름 확인 → 한 줄 추가)을 지키는 잠금 파일 이름. `<root>/registry.lock`.
const LOCK_FILE: &str = "registry.lock";
/// 잠금을 쥔 구간은 파일 한 번 읽고 한 줄 붙이는 시간뿐이다. 이보다 오래된 잠금 파일은
/// 쥔 프로세스가 비정상 종료해 남은 것으로 본다.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(10);
/// 다른 프로세스의 잠금이 풀리길 기다리는 최대 시간. 넘기면 오류(무한 대기 금지).
const LOCK_WAIT_MAX: Duration = Duration::from_secs(5);
const LOCK_POLL: Duration = Duration::from_millis(5);

/// 잠금 파일을 만든 동안만 살아 있는 보호 구간. 버려질 때(오류·패닉 포함) 파일을 지운다.
struct RegistryLock {
    path: PathBuf,
}

impl RegistryLock {
    fn acquire(root: &Path) -> StoreResult<RegistryLock> {
        let path = root.join(LOCK_FILE);
        let started = Instant::now();
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(RegistryLock { path }),
                // Windows 는 막 지워지는 중인 파일에 만들기를 시도하면 PermissionDenied 를 준다 — 같은 "사용 중".
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::AlreadyExists | ErrorKind::PermissionDenied
                    ) =>
                {
                    let mut cleanup_failed = None;
                    if Self::is_stale(&path) {
                        match fs::remove_file(&path) {
                            // 치웠거나 다른 프로세스가 먼저 치웠다 — 곧바로 다시 만들어 본다.
                            Ok(()) => continue,
                            Err(e) if e.kind() == ErrorKind::NotFound => continue,
                            // 치우지 못하면 대기 상한 검사로 넘어가 무한 반복을 막는다.
                            Err(e) => cleanup_failed = Some(e),
                        }
                    }
                    if started.elapsed() > LOCK_WAIT_MAX {
                        let note = cleanup_failed
                            .map(|e| format!(" (stale lock could not be removed: {e})"))
                            .unwrap_or_default();
                        return Err(StoreError::Backend(format!(
                            "registry lock busy: {}{note}",
                            path.display()
                        )));
                    }
                    std::thread::sleep(LOCK_POLL);
                }
                Err(e) => return Err(StoreError::Backend(e.to_string())),
            }
        }
    }

    fn is_stale(path: &Path) -> bool {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > LOCK_STALE_AFTER)
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        // 지우지 못하면 LOCK_STALE_AFTER 뒤 다음 사용자가 치운다 — Drop 에서 올릴 곳이 없다.
        let _ = fs::remove_file(&self.path);
    }
}

/// 파일 기반 공급자: `<root>/<graph-id>.db` + `<root>/registry.jsonl` (append-only).
pub struct FsProvider {
    root: PathBuf,
}

impl FsProvider {
    pub fn new(root: impl Into<PathBuf>) -> StoreResult<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(FsProvider { root })
    }
}

impl GraphStoreProvider for FsProvider {
    fn open_graph(&self, id: GraphId) -> StoreResult<Arc<dyn StorageBackend + Send + Sync>> {
        let path = self.root.join(format!("{id}.db"));
        Ok(Arc::new(SqliteBackend::open(&path)?))
    }

    fn registry(&self) -> StoreResult<Vec<RegistryEntry>> {
        let path = self.root.join("registry.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&path).map_err(|e| StoreError::Backend(e.to_string()))?;
        let mut out = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let j: serde_json::Value =
                serde_json::from_str(line).map_err(|e| StoreError::Backend(e.to_string()))?;
            let s = |k: &str| -> StoreResult<String> {
                Ok(j.get(k)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| StoreError::Backend(format!("registry missing {k}")))?
                    .to_string())
            };
            out.push(RegistryEntry {
                id: s("id")?
                    .parse()
                    .map_err(|e| StoreError::Backend(format!("registry id: {e:?}")))?,
                ns: SmolStr::new(s("ns")?),
                slug: SmolStr::new(s("slug")?),
                owner: s("owner")?,
            });
        }
        Ok(out)
    }

    fn register_new(&self, e: &RegistryEntry) -> StoreResult<bool> {
        let _lock = RegistryLock::acquire(&self.root)?;
        if self
            .registry()?
            .iter()
            .any(|r| r.ns == e.ns && r.slug == e.slug)
        {
            return Ok(false);
        }
        self.register(e)?;
        Ok(true)
    }

    fn register(&self, e: &RegistryEntry) -> StoreResult<()> {
        let path = self.root.join("registry.jsonl");
        let line = format!(
            "{{\"id\":\"{}\",\"ns\":\"{}\",\"slug\":\"{}\",\"owner\":\"{}\"}}\n",
            e.id, e.ns, e.slug, e.owner
        );
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        f.write_all(line.as_bytes())
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }
}
