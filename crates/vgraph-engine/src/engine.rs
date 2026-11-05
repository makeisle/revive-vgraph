//! Engine — Vcs 창구 + 캐시 + synced/on_update 상태머신 + 이벤트.
//!
//! 선형화 지점은 ref CAS 하나 (설계 §6-3). commit 경쟁: 서로소 → auto-rebase,
//! 겹침 → ConflictReport (설계 §4.4). 이벤트는 통지일 뿐 — 뷰 자동 전환 없음 (§3.3).

use crate::error::*;
use crate::ops::{EditOp, WalHeader};
use crate::provider::{GraphStoreProvider, RegistryEntry};
use crate::schema::{Schema, SchemaIssue, SchemaMode, SCHEMA_SLUG};
use crate::view::{View, ViewSupport};
use smol_str::SmolStr;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use vgraph_core::addr::valid_branch_name;
use vgraph_core::canon::{payload_hash, props_from_json, props_json};
use vgraph_core::{
    apply, diff, Clock, Commit, Delta, DeltaBuilder, Dir, GraphAddr, GraphId, GraphState, Hash256,
    Hit, IdGen, NodeSel, Props, Uuid, Value, WorkingId,
};
use vgraph_store::{CasOutcome, EdgeRow, LinkRows, NodeRow, StorageBackend, StorageStats};

#[derive(Debug, Clone, Copy)]
pub struct EngineOptions {
    /// commit 시 head 표현 동기 upsert (설계상 비동기 — v0은 인라인, 의미 동일).
    pub auto_upsert_head: bool,
}
impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            auto_upsert_head: true,
        }
    }
}

enum Lookup {
    Found(GraphId),
    Ambiguous(Vec<GraphId>),
    Missing,
}

type DynBackend = Arc<dyn StorageBackend + Send + Sync>;

struct Support {
    backend: DynBackend,
    io: Arc<AtomicU64>,
}
impl ViewSupport for Support {
    fn fetch_props(&self, h: &Hash256) -> Props {
        self.io.fetch_add(1, Ordering::SeqCst);
        match self.backend.get_payload(h) {
            Ok(Some(s)) => serde_json::from_str::<serde_json::Value>(&s)
                .ok()
                .and_then(|j| props_from_json(&j).ok())
                .unwrap_or_default(),
            _ => Props::default(),
        }
    }
    fn scan_index(&self, key: &str, value_json: &str) -> Vec<Hash256> {
        self.backend.scan_index(key, value_json).unwrap_or_default()
    }
}

/// 세션 재사용 키: (브랜치, 작업 공간 이름, 세션 sid).
type SessionKey = (String, Option<String>, u64);

struct GraphCtx {
    id: GraphId,
    ns: SmolStr,
    slug: SmolStr,
    owner: String,
    backend: DynBackend,
    cache: Mutex<HashMap<Hash256, Arc<View>>>,
    io_reads: Arc<AtomicU64>,
    subs: Mutex<Vec<(Sender<GraphEvent>, Option<Vec<String>>)>>,
    commit_lock: Mutex<()>,
    /// (branch, workspace, session-sid) → 세션 재사용 (골든 C2). workspace None = 기본.
    sessions: Mutex<HashMap<SessionKey, EditSession>>,
}

/// 작업 공간 이름 규칙 — 주소 세그먼트(`vgraph-core` addr)와 같다: 영문·숫자·`-`·`_`, 비어 있으면 안 됨.
fn valid_workspace_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl GraphCtx {
    fn support(&self) -> Arc<dyn ViewSupport> {
        Arc::new(Support {
            backend: self.backend.clone(),
            io: self.io_reads.clone(),
        })
    }
    fn emit(&self, branch: &str, ev: GraphEvent) {
        self.subs.lock().unwrap().retain(|(s, filter)| {
            let pass = match filter {
                Some(bs) => bs.iter().any(|b| b == branch),
                None => true,
            };
            !pass || s.send(ev.clone()).is_ok()
        });
    }

    /// 브랜치 무관 이벤트 (Drift) — 필터 없이 전 구독자에게.
    fn emit_all(&self, ev: GraphEvent) {
        self.subs
            .lock()
            .unwrap()
            .retain(|(s, _)| s.send(ev.clone()).is_ok());
    }
}

struct Inner {
    provider: Box<dyn GraphStoreProvider>,
    idgen: Arc<dyn IdGen>,
    clock: Arc<dyn Clock>,
    opts: EngineOptions,
    graphs: Mutex<HashMap<GraphId, Arc<GraphCtx>>>,
    entries: Mutex<HashMap<GraphId, RegistryEntry>>,
    by_name: Mutex<HashMap<(SmolStr, SmolStr), GraphId>>,
    /// 레지스트리에 같은 (ns, slug)가 둘 이상인 이름 → 그 이름을 가진 모든 GraphId.
    /// 이 이름은 `by_name`에 첫 항목이 남아 있어도 조회에서 쓰지 않는다 (한쪽 데이터를 가리지 않기 위해).
    conflicts: Mutex<HashMap<(SmolStr, SmolStr), Vec<GraphId>>>,
    sid_counter: AtomicU64,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    pub fn open(
        provider: Box<dyn GraphStoreProvider>,
        idgen: Arc<dyn IdGen>,
        clock: Arc<dyn Clock>,
        opts: EngineOptions,
    ) -> EngineResult<Engine> {
        let engine = Engine {
            inner: Arc::new(Inner {
                provider,
                idgen,
                clock,
                opts,
                graphs: Mutex::new(HashMap::new()),
                entries: Mutex::new(HashMap::new()),
                by_name: Mutex::new(HashMap::new()),
                conflicts: Mutex::new(HashMap::new()),
                sid_counter: AtomicU64::new(1),
            }),
        };
        engine.refresh_registry()?;
        Ok(engine)
    }

    /// 레지스트리에 같은 이름(ns, slug)이 둘 이상 올라 있는 경우 `(ns, slug, 그 GraphId들)`.
    /// 정상 저장소에서는 비어 있다. 이런 이름은 조회가 거부된다(`Session::resolve`).
    pub fn duplicate_names(&self) -> Vec<(String, String, Vec<GraphId>)> {
        self.inner
            .conflicts
            .lock()
            .unwrap()
            .iter()
            .map(|((ns, slug), ids)| (ns.to_string(), slug.to_string(), ids.clone()))
            .collect()
    }

    /// 레지스트리를 다시 읽어 아직 모르는 항목만 더한다. 이미 아는 항목·열린 그래프(`graphs`)·편집 세션은
    /// 건드리지 않는다. 여러 스레드가 동시에 불러도 안전하다: 읽기는 잠금 밖에서 하고, 반영은
    /// `entries → by_name → conflicts` 순서로 세 잠금을 함께 쥐고 "모르는 항목만 추가"하므로
    /// 멱등이다. 이 세 잠금을 겹쳐 쥐는 곳은 여기뿐이고 다른 곳은 한 번에 하나만 쥐므로 교착이 없다.
    fn refresh_registry(&self) -> EngineResult<()> {
        let list = self.inner.provider.registry()?;
        let mut entries = self.inner.entries.lock().unwrap();
        let mut by_name = self.inner.by_name.lock().unwrap();
        let mut conflicts = self.inner.conflicts.lock().unwrap();
        for e in list {
            if entries.contains_key(&e.id) {
                continue;
            }
            let key = (e.ns.clone(), e.slug.clone());
            match by_name.get(&key) {
                None => {
                    by_name.insert(key, e.id);
                }
                Some(first) => conflicts
                    .entry(key)
                    .or_insert_with(|| vec![*first])
                    .push(e.id),
            }
            entries.insert(e.id, e);
        }
        Ok(())
    }

    /// 이미 아는 이름만 본다 (다시 읽기 없음).
    fn lookup_known(&self, key: &(SmolStr, SmolStr)) -> Lookup {
        if let Some(ids) = self.inner.conflicts.lock().unwrap().get(key) {
            return Lookup::Ambiguous(ids.clone());
        }
        match self.inner.by_name.lock().unwrap().get(key) {
            Some(id) => Lookup::Found(*id),
            None => Lookup::Missing,
        }
    }

    /// 이름 → 그래프. 모르는 이름일 때만 레지스트리를 한 번 다시 읽고 다시 찾는다 (아는 이름은 비용 0).
    fn lookup(&self, ns: &str, slug: &str) -> Lookup {
        let key = (SmolStr::new(ns), SmolStr::new(slug));
        match self.lookup_known(&key) {
            Lookup::Missing => {
                // 다시 읽기 실패(예: 다른 프로세스가 한 줄을 쓰는 도중에 읽음)는 "아직 모름"으로 취급한다.
                // 영구 손상이면 이미 `open`에서 실패했고, 일시적이면 다음 조회에서 다시 읽는다.
                let _ = self.refresh_registry();
                self.lookup_known(&key)
            }
            known => known,
        }
    }

    pub fn session(&self, user: &str) -> Session {
        Session {
            engine: self.clone(),
            user: user.to_string(),
            sid: self.inner.sid_counter.fetch_add(1, Ordering::SeqCst),
        }
    }

    fn ctx(&self, id: GraphId) -> EngineResult<Arc<GraphCtx>> {
        {
            let graphs = self.inner.graphs.lock().unwrap();
            if let Some(c) = graphs.get(&id) {
                return Ok(c.clone());
            }
        }
        let entry = self
            .inner
            .entries
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(ResolveError::UnknownGraph)?;
        let backend = self.inner.provider.open_graph(id)?;
        let ctx = Arc::new(GraphCtx {
            id,
            ns: entry.ns.clone(),
            slug: entry.slug.clone(),
            owner: entry.owner.clone(),
            backend,
            cache: Mutex::new(HashMap::new()),
            io_reads: Arc::new(AtomicU64::new(0)),
            subs: Mutex::new(Vec::new()),
            commit_lock: Mutex::new(()),
            sessions: Mutex::new(HashMap::new()),
        });
        self.inner.graphs.lock().unwrap().insert(id, ctx.clone());
        Ok(ctx)
    }

    // ---------- materialization (설계 §3.2 적재 경로) ----------

    fn commit_chain(backend: &DynBackend, target: Hash256) -> EngineResult<Vec<(Hash256, Commit)>> {
        let mut chain = Vec::new();
        let mut cur = target;
        loop {
            let bytes = backend.get_blob(&cur)?.ok_or(ResolveError::UnknownCommit)?;
            let commit = Commit::from_json(
                std::str::from_utf8(&bytes).map_err(|e| EngineError::Corrupt(e.to_string()))?,
            )
            .map_err(|e| EngineError::Corrupt(e.to_string()))?;
            let parents = commit.parents.clone();
            chain.push((cur, commit));
            match parents.first() {
                Some(p) => cur = *p,
                None => break,
            }
        }
        Ok(chain)
    }

    fn load_delta(backend: &DynBackend, h: &Hash256) -> EngineResult<Delta> {
        let bytes = backend
            .get_blob(h)?
            .ok_or_else(|| EngineError::Corrupt(format!("missing delta {h}")))?;
        Delta::from_json(
            std::str::from_utf8(&bytes).map_err(|e| EngineError::Corrupt(e.to_string()))?,
        )
        .map_err(|e| EngineError::Corrupt(e.to_string()))
    }

    fn rows_to_state(ctx: &GraphCtx, branch: &str) -> EngineResult<GraphState> {
        let support = ctx.support();
        let mut s = GraphState::new();
        for r in ctx.backend.scan_nodes(branch)? {
            s.insert_node(vgraph_core::Node {
                uuid: r.uuid,
                labels: r.labels,
                slug: r.slug,
                props: support.fetch_props(&r.payload),
            })?;
        }
        for r in ctx.backend.scan_edges(branch)? {
            s.insert_edge(vgraph_core::Edge {
                uuid: r.uuid,
                etype: r.etype,
                from: r.from,
                to: r.to,
                props: support.fetch_props(&r.payload),
            })?;
        }
        Ok(s)
    }

    /// 연결 계층 워터마크가 체인 위에 있으면 rows에서 시작 + 잔여 delta (설계 §5.7),
    /// 아니면 root부터 fold.
    ///
    /// rows 단축은 "워터마크 → rows 스캔" 스냅샷 일관성이 필요하다. 동시 upsert가
    /// 끼면 잔여 delta가 이중 적용되므로, **스캔 후 워터마크 재확인** 낙관적 재시도
    /// (실패 누적 시 항상-정확한 root fold로 폴백 — 골든 K2가 감시).
    fn materialize_state(&self, ctx: &GraphCtx, target: Hash256) -> EngineResult<GraphState> {
        let chain = Self::commit_chain(&ctx.backend, target)?;

        'shortcut: for _ in 0..4 {
            // 워터마크 단축 탐색 (매 시도마다 신선하게)
            let mut best: Option<(usize, String, Hash256)> = None;
            for (branch, _) in ctx.backend.list_refs()? {
                if let Some(wm) = ctx.backend.watermark(&branch)? {
                    if let Some(idx) = chain.iter().position(|(h, _)| *h == wm) {
                        if best.as_ref().is_none_or(|(b, _, _)| idx < *b) {
                            best = Some((idx, branch, wm));
                        }
                    }
                }
            }
            let Some((idx, branch, wm)) = best else {
                break 'shortcut;
            };
            let mut state = match Self::rows_to_state(ctx, &branch) {
                Ok(s) => s,
                Err(_) => continue 'shortcut, // 스캔 중 경합 — 재시도
            };
            if ctx.backend.watermark(&branch)? != Some(wm) {
                continue 'shortcut; // rows가 스캔 중 전진 — 재시도
            }
            for i in (0..idx).rev() {
                let d = Self::load_delta(&ctx.backend, &chain[i].1.delta)?;
                apply(&mut state, &d)?;
            }
            return Ok(state);
        }

        // 폴백: 불변 blob만 읽는 root fold (경합 불가)
        let mut state = GraphState::new();
        for i in (0..chain.len()).rev() {
            let d = Self::load_delta(&ctx.backend, &chain[i].1.delta)?;
            apply(&mut state, &d)?;
        }
        Ok(state)
    }

    fn materialize_view(&self, ctx: &GraphCtx, target: Hash256) -> EngineResult<Arc<View>> {
        // rows가 정확히 target이면 lazy-props 경로 (골든 B7) — 동일한 재확인 규약
        for (branch, _) in ctx.backend.list_refs()? {
            if ctx.backend.watermark(&branch)? == Some(target) {
                let nodes = ctx.backend.scan_nodes(&branch)?;
                let edges = ctx.backend.scan_edges(&branch)?;
                if ctx.backend.watermark(&branch)? == Some(target) {
                    return Ok(Arc::new(View::from_rows(
                        ctx.id,
                        target,
                        nodes,
                        edges,
                        ctx.support(),
                    )));
                }
            }
        }
        let state = self.materialize_state(ctx, target)?;
        Ok(Arc::new(View::from_state(
            ctx.id,
            target,
            &state,
            ctx.support(),
        )))
    }
}

// ---------- EditSession (on_update, 설계 §4) ----------

struct WorkInner {
    graph: GraphId,
    wid: WorkingId,
    owner: String,
    branch: Option<String>, // None = detached (골든 D9b)
    /// None = 기본 작업 공간.
    workspace: Option<String>,
    base_commit: Option<Hash256>,
    builder: DeltaBuilder,
    backend: DynBackend,
    idgen: Arc<dyn IdGen>,
}

impl WorkInner {
    fn record(&mut self, op: EditOp) -> EngineResult<()> {
        op.apply(&mut self.builder.work)?; // 실패 시 WAL 미기록 = op 원자성 (골든 C8)
        self.backend
            .append_wal(self.wid, &self.owner, &op.to_json())?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct EditSession {
    inner: Arc<Mutex<WorkInner>>,
}

impl std::fmt::Debug for EditSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let w = self.inner.lock().unwrap();
        write!(f, "EditSession({}, base={:?})", w.wid, w.base_commit)
    }
}

impl EditSession {
    pub fn wid(&self) -> WorkingId {
        self.inner.lock().unwrap().wid
    }
    pub fn graph(&self) -> GraphId {
        self.inner.lock().unwrap().graph
    }
    /// epoch이 아니라 base — WorkingCopy의 버전 주장 (인터페이스 §2.3).
    pub fn base(&self) -> Option<Hash256> {
        self.inner.lock().unwrap().base_commit
    }
    pub fn branch(&self) -> Option<String> {
        self.inner.lock().unwrap().branch.clone()
    }
    /// 작업 공간 이름 (`None` = 기본 작업 공간).
    pub fn workspace(&self) -> Option<String> {
        self.inner.lock().unwrap().workspace.clone()
    }
    pub fn dirty(&self) -> bool {
        !self.inner.lock().unwrap().builder.build().is_empty()
    }
    pub fn pending(&self) -> Delta {
        self.inner.lock().unwrap().builder.build()
    }

    pub fn add_node(
        &self,
        labels: &[&str],
        slug: Option<&str>,
        props: Props,
    ) -> EngineResult<Uuid> {
        let mut w = self.inner.lock().unwrap();
        let uuid = w.idgen.next_uuid();
        w.record(EditOp::AddNode {
            uuid,
            labels: labels.iter().map(|l| SmolStr::new(l)).collect(),
            slug: slug.map(SmolStr::new),
            props,
        })?;
        Ok(uuid)
    }

    pub fn add_edge(&self, etype: &str, from: Uuid, to: Uuid, props: Props) -> EngineResult<Uuid> {
        let mut w = self.inner.lock().unwrap();
        let uuid = w.idgen.next_uuid();
        w.record(EditOp::AddEdge {
            uuid,
            etype: SmolStr::new(etype),
            from,
            to,
            props,
        })?;
        Ok(uuid)
    }

    pub fn set_prop(&self, elem: Uuid, key: &str, value: Value) -> EngineResult<()> {
        self.inner.lock().unwrap().record(EditOp::SetProp {
            elem,
            key: SmolStr::new(key),
            value,
        })
    }
    pub fn unset_prop(&self, elem: Uuid, key: &str) -> EngineResult<()> {
        self.inner.lock().unwrap().record(EditOp::UnsetProp {
            elem,
            key: SmolStr::new(key),
        })
    }
    pub fn remove_node(&self, uuid: Uuid, cascade: bool) -> EngineResult<()> {
        self.inner
            .lock()
            .unwrap()
            .record(EditOp::RemoveNode { uuid, cascade })
    }
    pub fn remove_edge(&self, uuid: Uuid) -> EngineResult<()> {
        self.inner
            .lock()
            .unwrap()
            .record(EditOp::RemoveEdge { uuid })
    }
    pub fn set_slug(&self, uuid: Uuid, slug: Option<&str>) -> EngineResult<()> {
        self.inner.lock().unwrap().record(EditOp::SetSlug {
            uuid,
            slug: slug.map(SmolStr::new),
        })
    }
    pub fn add_label(&self, uuid: Uuid, label: &str) -> EngineResult<()> {
        self.inner.lock().unwrap().record(EditOp::AddLabel {
            uuid,
            label: SmolStr::new(label),
        })
    }
    pub fn remove_label(&self, uuid: Uuid, label: &str) -> EngineResult<()> {
        self.inner.lock().unwrap().record(EditOp::RemoveLabel {
            uuid,
            label: SmolStr::new(label),
        })
    }

    // ---------- 읽기 = base+pending 투영 (골든 C7) ----------

    pub fn node_prop(&self, elem: Uuid, key: &str) -> Option<Value> {
        let w = self.inner.lock().unwrap();
        w.builder
            .work
            .node(elem)
            .and_then(|n| n.props.get(key).cloned())
            .or_else(|| {
                w.builder
                    .work
                    .edge(elem)
                    .and_then(|e| e.props.get(key).cloned())
            })
    }
    pub fn by_slug(&self, slug: &str) -> Option<Uuid> {
        self.inner.lock().unwrap().builder.work.by_slug(slug)
    }
    pub fn neighbors(
        &self,
        start: Uuid,
        dir: Dir,
        types: Option<&[&str]>,
        prop_filter: Option<(&str, &Value)>,
        min_depth: u32,
        max_depth: u32,
        limit: Option<usize>,
    ) -> Vec<Hit> {
        self.inner.lock().unwrap().builder.work.neighbors(
            start,
            dir,
            types,
            prop_filter,
            min_depth,
            max_depth,
            limit,
        )
    }
}

// ---------- Session (사용자 창구) ----------

pub struct Session {
    engine: Engine,
    user: String,
    sid: u64,
}

impl Session {
    fn acl(&self, ctx: &GraphCtx) -> Result<(), ResolveError> {
        if self.user == ctx.owner {
            Ok(())
        } else {
            Err(ResolveError::NoPermission)
        }
    }

    /// 이름 → GraphId (CLI 등 도구용).
    ///
    /// 모르는 이름이면 레지스트리를 다시 읽어 본다. 레지스트리에 같은 이름이 둘 이상이면 `None`
    /// (한쪽을 고르지 않는다 — 사유는 `resolve`의 오류).
    pub fn graph_id(&self, ns: &str, slug: &str) -> Option<GraphId> {
        match self.engine.lookup(ns, slug) {
            Lookup::Found(id) => Some(id),
            Lookup::Ambiguous(_) | Lookup::Missing => None,
        }
    }

    /// 내가 소유한 그래프 목록 (ns, slug, id).
    ///
    /// 다른 프로세스가 만든 그래프도 보이도록 먼저 레지스트리를 다시 읽는다(목록은 드문 호출).
    pub fn graphs(&self) -> Vec<(String, String, GraphId)> {
        // 읽기 실패는 아는 항목만으로 목록을 돌려준다 — 다음 호출에서 다시 읽는다.
        let _ = self.engine.refresh_registry();
        self.engine
            .inner
            .entries
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.owner == self.user)
            .map(|e| (e.ns.to_string(), e.slug.to_string(), e.id))
            .collect()
    }

    pub fn create_graph(&self, ns: &str, slug: &str) -> EngineResult<GraphId> {
        let key = (SmolStr::new(ns), SmolStr::new(slug));
        let exists = || EngineError::Corrupt(format!("graph exists: {ns}/{slug}"));
        if !matches!(self.engine.lookup_known(&key), Lookup::Missing) {
            return Err(exists());
        }
        let id = self.engine.inner.idgen.next_graph();
        let entry = RegistryEntry {
            id,
            ns: key.0.clone(),
            slug: key.1.clone(),
            owner: self.user.clone(),
        };
        // 이름 확인과 등록을 공급자가 한 덩어리로 처리한다(다른 프로세스가 먼저 만든 이름 포함).
        if !self.engine.inner.provider.register_new(&entry)? {
            // 거부는 이미 확정이다. 다시 읽기는 이 엔진이 그 이름을 알게 하려는 것이라 실패해도 무시한다.
            let _ = self.engine.refresh_registry();
            return Err(exists());
        }
        self.engine.inner.entries.lock().unwrap().insert(id, entry);
        self.engine.inner.by_name.lock().unwrap().insert(key, id);
        self.engine.ctx(id)?; // 백엔드 생성
        Ok(id)
    }

    pub fn resolve(&self, addr: &str) -> Result<(GraphId, Hash256), ResolveError> {
        let a: GraphAddr = addr
            .parse()
            .map_err(|e| ResolveError::BadAddr(format!("{e}")))?;
        let id = match self.engine.lookup(&a.ns, &a.slug) {
            Lookup::Found(id) => id,
            Lookup::Ambiguous(ids) => {
                let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
                return Err(ResolveError::AmbiguousGraph(format!(
                    "graph name {}/{} is ambiguous: the registry lists it {} times ({}); \
                     remove the extra line from registry.jsonl",
                    a.ns,
                    a.slug,
                    ids.len(),
                    ids.join(", ")
                )));
            }
            Lookup::Missing => return Err(ResolveError::UnknownGraph),
        };
        let ctx = self
            .engine
            .ctx(id)
            .map_err(|_| ResolveError::UnknownGraph)?;
        self.acl(&ctx)?;
        if let Some(c) = a.commit {
            if !ctx.backend.has_blob(&c).unwrap_or(false) {
                return Err(ResolveError::UnknownCommit);
            }
            return Ok((id, c));
        }
        let branch = a.branch.as_deref().unwrap_or("main");
        match ctx.backend.get_ref(branch) {
            Ok(Some(h)) => Ok((id, h)),
            Ok(None) => Err(ResolveError::UnknownBranch),
            Err(_) => Err(ResolveError::UnknownGraph),
        }
    }

    pub fn checkout(&self, g: GraphId, c: Hash256) -> EngineResult<Arc<View>> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        if let Some(v) = ctx.cache.lock().unwrap().get(&c) {
            return Ok(v.clone());
        }
        let view = self.engine.materialize_view(&ctx, c)?;
        // 경쟁 시 기존 엔트리 우선 (동일 hash = 동일 내용이므로 어느 쪽이든 정확)
        let out = ctx
            .cache
            .lock()
            .unwrap()
            .entry(c)
            .or_insert_with(|| view.clone())
            .clone();
        Ok(out)
    }

    pub fn at(&self, g: GraphId, c: Hash256) -> EngineResult<Arc<View>> {
        self.checkout(g, c)
    }

    pub fn edit(&self, g: GraphId, branch: &str) -> Result<EditSession, EditStartError> {
        self.edit_opts(g, branch, None, false)
    }

    pub fn edit_opts(
        &self,
        g: GraphId,
        branch: &str,
        at: Option<Hash256>,
        force_detach: bool,
    ) -> Result<EditSession, EditStartError> {
        self.edit_opts_in(g, branch, at, force_detach, None)
    }

    /// 이름 있는 작업 공간에서 편집 시작. `None`이면 `edit`과 같다.
    /// 같은 사용자·브랜치라도 이름이 다르면 WAL이 따로 쌓인다.
    pub fn edit_in(
        &self,
        g: GraphId,
        branch: &str,
        workspace: Option<&str>,
    ) -> Result<EditSession, EditStartError> {
        self.edit_opts_in(g, branch, None, false, workspace)
    }

    fn edit_opts_in(
        &self,
        g: GraphId,
        branch: &str,
        at: Option<Hash256>,
        force_detach: bool,
        workspace: Option<&str>,
    ) -> Result<EditSession, EditStartError> {
        if let Some(w) = workspace {
            if !valid_workspace_name(w) {
                return Err(ResolveError::BadAddr(format!(
                    "invalid workspace name {w:?} (allowed: letters, digits, '-', '_')"
                ))
                .into());
            }
        }
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx).map_err(EngineError::from)?;
        let head = ctx.backend.get_ref(branch).map_err(EngineError::from)?;

        // 과거 commit 편집 게이트 (골든 D9)
        let (base, detached) = match at {
            Some(c) if head != Some(c) => {
                if !force_detach {
                    return Err(EditStartError::NotHead {
                        head: head.unwrap_or_default(),
                        hint: EditHint::BranchRecommended,
                    });
                }
                (Some(c), true)
            }
            Some(c) => (Some(c), false),
            None => (head, false),
        };

        // 세션 재사용 (골든 C2) — detached는 재사용하지 않음
        let key: SessionKey = (branch.to_string(), workspace.map(String::from), self.sid);
        if !detached {
            if let Some(es) = ctx.sessions.lock().unwrap().get(&key) {
                return Ok(es.clone());
            }
        }

        let wid = self.engine.inner.idgen.next_working();
        let base_state = match base {
            Some(c) => self.engine.materialize_state(&ctx, c)?,
            None => GraphState::new(),
        };
        let hdr = WalHeader {
            base,
            branch: if detached {
                None
            } else {
                Some(branch.to_string())
            },
            owner: self.user.clone(),
            workspace: workspace.map(String::from),
        };
        ctx.backend
            .append_wal(wid, &self.user, &hdr.to_json())
            .map_err(EngineError::from)?;
        let es = EditSession {
            inner: Arc::new(Mutex::new(WorkInner {
                graph: g,
                wid,
                owner: self.user.clone(),
                branch: if detached {
                    None
                } else {
                    Some(branch.to_string())
                },
                workspace: workspace.map(String::from),
                base_commit: base,
                builder: DeltaBuilder::new(&base_state),
                backend: ctx.backend.clone(),
                idgen: self.engine.inner.idgen.clone(),
            })),
        };
        if !detached {
            ctx.sessions.lock().unwrap().insert(key, es.clone());
        }
        Ok(es)
    }

    pub fn commit(&self, es: &EditSession, msg: &str) -> EngineResult<CommitOutcome> {
        let g = es.graph();
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let _serial = ctx.commit_lock.lock().unwrap();
        let mut w = es.inner.lock().unwrap();

        let mut cur_delta = w.builder.build();
        if cur_delta.is_empty() {
            return Ok(CommitOutcome::Empty); // 골든 D5
        }
        // external link 통합: 새 Ref 값 → lockfile 핀 등록 (설계 §7.2, 골든 I1·I6)
        self.integrate_links(&ctx, &mut w)?;
        cur_delta = w.builder.build();

        // KG 오버레이 commit 게이트 (설계 §2.2, 골든 D8·H2·H3) — touched 요소만
        let mut warnings: Vec<SchemaIssue> = Vec::new();
        if let Some(schema) = Schema::of_state(&w.builder.work) {
            if schema.mode != SchemaMode::Off {
                let (tn, te) = touched_of(&cur_delta, &w.builder.work);
                let issues = schema.validate_elements(&w.builder.work, &tn, &te);
                if !issues.is_empty() {
                    if schema.mode == SchemaMode::Strict {
                        return Ok(CommitOutcome::SchemaViolation(issues)); // head 불변
                    }
                    warnings = issues; // warn: 통과 + 경고 동반
                }
            }
        }
        let mut rebased_onto: Option<Hash256> = None;

        for _attempt in 0..64 {
            let commit = Commit {
                parents: w.base_commit.iter().copied().collect(),
                delta: cur_delta.hash(),
                schema_rev: None,
                author: w.owner.clone(),
                ts: self.engine.inner.clock.now(),
                msg: msg.to_string(),
            };
            let chash = commit.hash();
            // K3 순서: delta → commit blob fsync 후 CAS (설계 §6-6)
            ctx.backend
                .put_blob(&cur_delta.hash(), cur_delta.canonical_json().as_bytes())?;
            ctx.backend
                .put_blob(&chash, commit.canonical_json().as_bytes())?;

            let branch = match &w.branch {
                None => {
                    // detached (골든 D9b): head를 움직이지 않고 종료
                    Self::cleanup(&ctx, &mut w, self.sid);
                    return Ok(CommitOutcome::Committed {
                        commit: chash,
                        warnings: warnings.clone(),
                    });
                }
                Some(b) => b.clone(),
            };

            match ctx
                .backend
                .cas_ref(&branch, w.base_commit.as_ref(), &chash)?
            {
                CasOutcome::Committed => {
                    self.post_commit(&ctx, &branch, &w, &cur_delta, chash)?;
                    let from = w.base_commit;
                    ctx.emit(
                        &branch,
                        GraphEvent::HeadAdvanced {
                            branch: branch.clone(),
                            from,
                            to: chash,
                        },
                    );
                    Self::cleanup(&ctx, &mut w, self.sid);
                    self.notify_dependents(&ctx); // 링크 드리프트 통지 (골든 I2)
                    return Ok(match rebased_onto {
                        Some(onto) => CommitOutcome::Rebased {
                            onto,
                            committed: chash,
                        },
                        None => CommitOutcome::Committed {
                            commit: chash,
                            warnings: warnings.clone(),
                        },
                    });
                }
                CasOutcome::Conflict { actual } => {
                    let actual = match actual {
                        Some(a) => a,
                        None => {
                            w.base_commit = None;
                            continue;
                        }
                    };
                    let theirs_state = self.engine.materialize_state(&ctx, actual)?;
                    let theirs_delta = diff(w.builder.base(), &theirs_state);
                    let items = detect_conflicts(&cur_delta, &theirs_delta);
                    if !items.is_empty() {
                        return Ok(CommitOutcome::Conflicts(ConflictReport {
                            graph: ctx.id,
                            base: w.base_commit,
                            ours: SideRef::Working(w.wid),
                            theirs: SideRef::Commit(actual),
                            items,
                        }));
                    }
                    // auto-rebase (골든 D6): 이미 사라진 요소 제거를 정리 후 재적용
                    let pruned = prune_delta(&cur_delta, &theirs_state);
                    let mut new_work = theirs_state.clone();
                    if let Err(e) = apply(&mut new_work, &pruned) {
                        return Ok(CommitOutcome::Conflicts(ConflictReport {
                            graph: ctx.id,
                            base: w.base_commit,
                            ours: SideRef::Working(w.wid),
                            theirs: SideRef::Commit(actual),
                            items: vec![ConflictItem::ApplyFailed {
                                reason: e.to_string(),
                            }],
                        }));
                    }
                    w.builder = DeltaBuilder::with_work(theirs_state, new_work);
                    w.base_commit = Some(actual);
                    rebased_onto = Some(actual);
                    cur_delta = w.builder.build();
                    if cur_delta.is_empty() {
                        // 상대 커밋이 내 변경을 이미 포함 (F9류 수렴)
                        Self::cleanup(&ctx, &mut w, self.sid);
                        return Ok(CommitOutcome::Empty);
                    }
                }
            }
        }
        Err(EngineError::Corrupt("commit retry exhausted".into()))
    }

    fn cleanup(ctx: &GraphCtx, w: &mut WorkInner, sid: u64) {
        let _ = ctx.backend.drop_wal(w.wid);
        if let Some(b) = &w.branch {
            let key = (b.clone(), w.workspace.clone(), sid);
            ctx.sessions.lock().unwrap().remove(&key);
        }
    }

    /// 소스 계층 payload + (옵션) head 표현 + synced 캐시 승급 (설계 §4.1-4).
    fn post_commit(
        &self,
        ctx: &GraphCtx,
        branch: &str,
        w: &WorkInner,
        delta: &Delta,
        chash: Hash256,
    ) -> EngineResult<()> {
        self.publish_state(ctx, branch, &w.builder.work, delta, chash)
    }

    /// commit/merge 공용 발행 경로.
    fn publish_state(
        &self,
        ctx: &GraphCtx,
        branch: &str,
        work: &GraphState,
        delta: &Delta,
        chash: Hash256,
    ) -> EngineResult<()> {
        let mut touched_nodes: BTreeSet<Uuid> = BTreeSet::new();
        let mut touched_edges: BTreeSet<Uuid> = BTreeSet::new();
        for n in &delta.nodes_added {
            touched_nodes.insert(n.uuid);
        }
        for e in &delta.edges_added {
            touched_edges.insert(e.uuid);
        }
        for p in &delta.props_set {
            if work.node(p.elem).is_some() {
                touched_nodes.insert(p.elem);
            } else if work.edge(p.elem).is_some() {
                touched_edges.insert(p.elem);
            }
        }
        for l in &delta.labels_delta {
            touched_nodes.insert(l.elem);
        }
        for s in &delta.slugs_set {
            touched_nodes.insert(s.elem);
        }

        // 소스 계층: 불변 payload row (항상 기록 — 검색·lazy 로드의 재료)
        for u in &touched_nodes {
            if let Some(n) = work.node(*u) {
                ctx.backend
                    .put_payload(&payload_hash(&n.props), &props_json(&n.props))?;
            }
        }
        for u in &touched_edges {
            if let Some(e) = work.edge(*u) {
                ctx.backend
                    .put_payload(&payload_hash(&e.props), &props_json(&e.props))?;
            }
        }

        // 연결 계층: head 표현 upsert (설계 §5.5).
        // 브랜치 첫 발행(워터마크 없음)은 full baseline — 이후부터 증분.
        // (신규 브랜치의 lazy 구축이 여기서 완결. 부분 rows가 전체로 오인되는 것 방지)
        if self.engine.inner.opts.auto_upsert_head {
            let first_publish = ctx.backend.watermark(branch)?.is_none();
            let rows = if first_publish {
                LinkRows {
                    upsert_nodes: work
                        .nodes()
                        .map(|n| NodeRow {
                            uuid: n.uuid,
                            labels: n.labels.clone(),
                            slug: n.slug.clone(),
                            payload: payload_hash(&n.props),
                        })
                        .collect(),
                    upsert_edges: work
                        .edges()
                        .map(|e| EdgeRow {
                            uuid: e.uuid,
                            etype: e.etype.clone(),
                            from: e.from,
                            to: e.to,
                            payload: payload_hash(&e.props),
                        })
                        .collect(),
                    watermark: chash,
                    ..Default::default()
                }
            } else {
                LinkRows {
                    upsert_nodes: touched_nodes
                        .iter()
                        .filter_map(|u| work.node(*u))
                        .map(|n| NodeRow {
                            uuid: n.uuid,
                            labels: n.labels.clone(),
                            slug: n.slug.clone(),
                            payload: payload_hash(&n.props),
                        })
                        .collect(),
                    remove_nodes: delta.nodes_removed.iter().map(|n| n.uuid).collect(),
                    upsert_edges: touched_edges
                        .iter()
                        .filter_map(|u| work.edge(*u))
                        .map(|e| EdgeRow {
                            uuid: e.uuid,
                            etype: e.etype.clone(),
                            from: e.from,
                            to: e.to,
                            payload: payload_hash(&e.props),
                        })
                        .collect(),
                    remove_edges: delta.edges_removed.iter().map(|e| e.uuid).collect(),
                    adjacency: Vec::new(),
                    remove_adjacency: Vec::new(),
                    watermark: chash,
                }
            };
            ctx.backend.upsert_head(branch, &rows)?;

            // 첫 발행은 baseline 전체 payload 필요 (lazy 로드 재료) — 미기록분 보충
            if first_publish {
                for n in work.nodes() {
                    if !touched_nodes.contains(&n.uuid) {
                        ctx.backend
                            .put_payload(&payload_hash(&n.props), &props_json(&n.props))?;
                    }
                }
                for e in work.edges() {
                    if !touched_edges.contains(&e.uuid) {
                        ctx.backend
                            .put_payload(&payload_hash(&e.props), &props_json(&e.props))?;
                    }
                }
            }
        }

        // synced 승급: W의 work가 그대로 새 commit의 공유 뷰가 됨 (재fold 불필요, 골든 D1)
        let view = View::from_state(ctx.id, chash, work, ctx.support());
        ctx.cache.lock().unwrap().insert(chash, Arc::new(view));
        Ok(())
    }

    pub fn discard(&self, es: EditSession) -> EngineResult<()> {
        let g = es.graph();
        let ctx = self.engine.ctx(g)?;
        let mut w = es.inner.lock().unwrap();
        Self::cleanup(&ctx, &mut w, self.sid);
        Ok(())
    }

    pub fn branch(&self, g: GraphId, from: Hash256, name: &str) -> EngineResult<()> {
        // 새로 만드는 이름만 규칙을 따진다. 이름 인자로 쓰는 기존 경로(log·merge 등)는 옛 이름도 그대로 받는다.
        valid_branch_name(name).map_err(|e| EngineError::BadBranchName(e.to_string()))?;
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        if !ctx.backend.has_blob(&from)? {
            return Err(ResolveError::UnknownCommit.into());
        }
        match ctx.backend.cas_ref(name, None, &from)? {
            CasOutcome::Committed => Ok(()),
            CasOutcome::Conflict { .. } => Err(EngineError::BranchExists),
        }
    }

    /// 도달 가능한 모든 commit (양쪽 parent 포함), ts 내림차순 (골든 F11 — merge 계보 포함).
    pub fn log(&self, g: GraphId, branch: &str) -> EngineResult<Vec<Hash256>> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let head = ctx
            .backend
            .get_ref(branch)?
            .ok_or(ResolveError::UnknownBranch)?;
        let mut seen: HashMap<Hash256, Commit> = HashMap::new();
        let mut stack = vec![head];
        while let Some(h) = stack.pop() {
            if seen.contains_key(&h) {
                continue;
            }
            let c = Engine::load_commit(&ctx.backend, &h)?;
            stack.extend(c.parents.iter().copied());
            seen.insert(h, c);
        }
        let mut v: Vec<(Hash256, Commit)> = seen.into_iter().collect();
        v.sort_by(|a, b| (&b.1.ts, &b.0).cmp(&(&a.1.ts, &a.0)));
        Ok(v.into_iter().map(|(h, _)| h).collect())
    }

    pub fn subscribe(
        &self,
        g: GraphId,
        branches: Option<&[&str]>,
    ) -> EngineResult<Receiver<GraphEvent>> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let (tx, rx) = channel();
        ctx.subs.lock().unwrap().push((
            tx,
            branches.map(|bs| bs.iter().map(|b| b.to_string()).collect()),
        ));
        Ok(rx)
    }

    pub fn list_working(&self, g: GraphId) -> EngineResult<Vec<WorkingId>> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        Ok(ctx.backend.list_wals(&self.user)?)
    }

    /// 크래시 후 WAL replay 복원 (골든 C5).
    pub fn resume(&self, g: GraphId, wid: WorkingId) -> EngineResult<EditSession> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let wal = ctx.backend.read_wal(wid)?;
        let hdr_line = wal.first().ok_or(EngineError::UnknownWorking)?;
        let hdr =
            WalHeader::from_json(hdr_line).map_err(|e| EngineError::Corrupt(e.to_string()))?;
        if hdr.owner != self.user {
            return Err(ResolveError::NoPermission.into());
        }
        let base_state = match hdr.base {
            Some(c) => self.engine.materialize_state(&ctx, c)?,
            None => GraphState::new(),
        };
        let mut builder = DeltaBuilder::new(&base_state);
        for line in &wal[1..] {
            let op = EditOp::from_json(line).map_err(|e| EngineError::Corrupt(e.to_string()))?;
            op.apply(&mut builder.work)?;
        }
        let es = EditSession {
            inner: Arc::new(Mutex::new(WorkInner {
                graph: g,
                wid,
                owner: self.user.clone(),
                branch: hdr.branch.clone(),
                workspace: hdr.workspace.clone(),
                base_commit: hdr.base,
                builder,
                backend: ctx.backend.clone(),
                idgen: self.engine.inner.idgen.clone(),
            })),
        };
        if let Some(b) = &hdr.branch {
            ctx.sessions
                .lock()
                .unwrap()
                .insert((b.clone(), hdr.workspace.clone(), self.sid), es.clone());
        }
        Ok(es)
    }

    // ---------- TestHooks (인터페이스 §3.4 — cfg(test) 전용은 아니나 문서상 계측 면) ----------

    pub fn test_evict_all(&self, g: GraphId) -> EngineResult<()> {
        let ctx = self.engine.ctx(g)?;
        // 핀 보호: 외부에서 Arc를 쥔 뷰(strong_count>1)는 축출 금지 (골든 B8)
        ctx.cache
            .lock()
            .unwrap()
            .retain(|_, v| Arc::strong_count(v) > 1);
        Ok(())
    }
    pub fn test_cache_contains(&self, g: GraphId, c: Hash256) -> bool {
        self.engine
            .ctx(g)
            .map(|ctx| ctx.cache.lock().unwrap().contains_key(&c))
            .unwrap_or(false)
    }
    pub fn test_io_reads(&self, g: GraphId) -> u64 {
        self.engine
            .ctx(g)
            .map(|ctx| ctx.io_reads.load(Ordering::SeqCst))
            .unwrap_or(0)
    }
    pub fn test_stats(&self, g: GraphId) -> EngineResult<StorageStats> {
        Ok(self.engine.ctx(g)?.backend.stats()?)
    }
    /// head 표현 강제 구축 (auto_upsert off 엔진의 B4 시뮬레이션용).
    pub fn test_flush_head(&self, g: GraphId, branch: &str) -> EngineResult<()> {
        let ctx = self.engine.ctx(g)?;
        let head = ctx
            .backend
            .get_ref(branch)?
            .ok_or(ResolveError::UnknownBranch)?;
        let state = self.engine.materialize_state(&ctx, head)?;
        for n in state.nodes() {
            ctx.backend
                .put_payload(&payload_hash(&n.props), &props_json(&n.props))?;
        }
        for e in state.edges() {
            ctx.backend
                .put_payload(&payload_hash(&e.props), &props_json(&e.props))?;
        }
        let rows = LinkRows {
            upsert_nodes: state
                .nodes()
                .map(|n| NodeRow {
                    uuid: n.uuid,
                    labels: n.labels.clone(),
                    slug: n.slug.clone(),
                    payload: payload_hash(&n.props),
                })
                .collect(),
            upsert_edges: state
                .edges()
                .map(|e| EdgeRow {
                    uuid: e.uuid,
                    etype: e.etype.clone(),
                    from: e.from,
                    to: e.to,
                    payload: payload_hash(&e.props),
                })
                .collect(),
            watermark: head,
            ..Default::default()
        };
        ctx.backend.upsert_head(branch, &rows)?;
        Ok(())
    }
    pub fn test_drop_derived(&self, g: GraphId) -> EngineResult<()> {
        Ok(self.engine.ctx(g)?.backend.reset_derived()?)
    }
    /// 캐시 우회 재-materialize dump (골든 K2의 torn-read 검증).
    pub fn test_state_dump(&self, g: GraphId, c: Hash256) -> EngineResult<String> {
        let ctx = self.engine.ctx(g)?;
        Ok(self.engine.materialize_state(&ctx, c)?.dump(Some(&c)))
    }
}

// ---------- 충돌 검출 (P3 스코프: commit 경쟁의 2-way 특수형. 3-way 일반화는 P4) ----------

fn prune_delta(ours: &Delta, theirs_state: &GraphState) -> Delta {
    let mut d = ours.clone();
    d.nodes_removed
        .retain(|n| theirs_state.node(n.uuid).is_some());
    d.edges_removed
        .retain(|e| theirs_state.edge(e.uuid).is_some());
    d
}

fn detect_conflicts(ours: &Delta, theirs: &Delta) -> Vec<ConflictItem> {
    let mut items = Vec::new();

    let theirs_removed: BTreeSet<Uuid> = theirs
        .nodes_removed
        .iter()
        .map(|n| n.uuid)
        .chain(theirs.edges_removed.iter().map(|e| e.uuid))
        .collect();
    let ours_removed: BTreeSet<Uuid> = ours
        .nodes_removed
        .iter()
        .map(|n| n.uuid)
        .chain(ours.edges_removed.iter().map(|e| e.uuid))
        .collect();
    let theirs_removed_nodes: BTreeSet<Uuid> =
        theirs.nodes_removed.iter().map(|n| n.uuid).collect();
    let ours_removed_nodes: BTreeSet<Uuid> = ours.nodes_removed.iter().map(|n| n.uuid).collect();

    // PropBothSet — 같은 (elem, key)를 양쪽이 다른 값으로 (골든 D7·F3; 같은 값 = F9 auto-resolve)
    let theirs_props: BTreeMap<(Uuid, &SmolStr), &vgraph_core::PropSet> = theirs
        .props_set
        .iter()
        .map(|p| ((p.elem, &p.key), p))
        .collect();
    for p in &ours.props_set {
        if let Some(tp) = theirs_props.get(&(p.elem, &p.key)) {
            if tp.new != p.new {
                items.push(ConflictItem::PropBothSet {
                    elem: p.elem,
                    key: p.key.clone(),
                    base: p.old.clone(),
                    ours: p.new.clone(),
                    theirs: tp.new.clone(),
                });
            }
        }
    }

    // RemovedVsEdit (골든 F4)
    let ours_edited: BTreeSet<Uuid> = ours
        .props_set
        .iter()
        .map(|p| p.elem)
        .chain(ours.labels_delta.iter().map(|l| l.elem))
        .chain(ours.slugs_set.iter().map(|s| s.elem))
        .collect();
    let theirs_edited: BTreeSet<Uuid> = theirs
        .props_set
        .iter()
        .map(|p| p.elem)
        .chain(theirs.labels_delta.iter().map(|l| l.elem))
        .chain(theirs.slugs_set.iter().map(|s| s.elem))
        .collect();
    for e in ours_edited.intersection(&theirs_removed) {
        items.push(ConflictItem::RemovedVsEdit {
            elem: *e,
            removed_by: Side::Theirs,
        });
    }
    for e in theirs_edited.intersection(&ours_removed) {
        items.push(ConflictItem::RemovedVsEdit {
            elem: *e,
            removed_by: Side::Ours,
        });
    }

    // DanglingEdge (골든 F5)
    for e in &ours.edges_added {
        for end in [e.from, e.to] {
            if theirs_removed_nodes.contains(&end) {
                items.push(ConflictItem::DanglingEdge {
                    edge: e.uuid,
                    missing_end: end,
                    removed_by: Side::Theirs,
                });
            }
        }
    }
    for e in &theirs.edges_added {
        for end in [e.from, e.to] {
            if ours_removed_nodes.contains(&end) {
                items.push(ConflictItem::DanglingEdge {
                    edge: e.uuid,
                    missing_end: end,
                    removed_by: Side::Ours,
                });
            }
        }
    }

    // SlugCollision (골든 F6)
    let slugs_of = |d: &Delta| -> BTreeMap<SmolStr, Uuid> {
        d.nodes_added
            .iter()
            .filter_map(|n| n.slug.clone().map(|s| (s, n.uuid)))
            .chain(
                d.slugs_set
                    .iter()
                    .filter_map(|s| s.new.clone().map(|sl| (sl, s.elem))),
            )
            .collect()
    };
    let ours_slugs = slugs_of(ours);
    let theirs_slugs = slugs_of(theirs);
    for (slug, ou) in &ours_slugs {
        if let Some(tu) = theirs_slugs.get(slug) {
            if tu != ou {
                items.push(ConflictItem::SlugCollision {
                    slug: slug.clone(),
                    ours: *ou,
                    theirs: *tu,
                });
            }
        }
    }

    items
}

// ================= P4: 머지 (3-way, 설계 §4.4·인터페이스 §3.3) =================

impl Engine {
    fn load_commit(backend: &DynBackend, h: &Hash256) -> EngineResult<Commit> {
        let bytes = backend.get_blob(h)?.ok_or(ResolveError::UnknownCommit)?;
        Commit::from_json(
            std::str::from_utf8(&bytes).map_err(|e| EngineError::Corrupt(e.to_string()))?,
        )
        .map_err(|e| EngineError::Corrupt(e.to_string()))
    }

    fn ancestors(backend: &DynBackend, h: Hash256) -> EngineResult<BTreeSet<Hash256>> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![h];
        while let Some(x) = stack.pop() {
            if !seen.insert(x) {
                continue;
            }
            stack.extend(Self::load_commit(backend, &x)?.parents);
        }
        Ok(seen)
    }

    /// a가 b의 조상인가 (b에서 도달 가능).
    fn is_ancestor(backend: &DynBackend, a: Hash256, b: Hash256) -> EngineResult<bool> {
        Ok(Self::ancestors(backend, b)?.contains(&a))
    }

    fn common_ancestor(
        backend: &DynBackend,
        a: Hash256,
        b: Hash256,
    ) -> EngineResult<Option<Hash256>> {
        let anc_a = Self::ancestors(backend, a)?;
        let mut seen = BTreeSet::new();
        let mut queue = std::collections::VecDeque::from([b]);
        while let Some(x) = queue.pop_front() {
            if !seen.insert(x) {
                continue;
            }
            if anc_a.contains(&x) {
                return Ok(Some(x));
            }
            queue.extend(Self::load_commit(backend, &x)?.parents);
        }
        Ok(None)
    }
}

impl Session {
    pub fn commit_meta(&self, g: GraphId, c: Hash256) -> EngineResult<Commit> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        Engine::load_commit(&ctx.backend, &c)
    }

    /// three-way merge (골든 F1~F12). merge delta는 parents[0]=dst 기준 —
    /// materialize의 first-parent 체인과 정합.
    pub fn merge(
        &self,
        g: GraphId,
        src: &str,
        dst: &str,
        opts: &MergeOpts,
    ) -> EngineResult<MergeOutcome> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let _serial = ctx.commit_lock.lock().unwrap();
        let hs = ctx
            .backend
            .get_ref(src)?
            .ok_or(ResolveError::UnknownBranch)?;
        let hd = ctx
            .backend
            .get_ref(dst)?
            .ok_or(ResolveError::UnknownBranch)?;

        if hs == hd || Engine::is_ancestor(&ctx.backend, hs, hd)? {
            return Ok(MergeOutcome::UpToDate(hd)); // 골든 F12
        }
        if Engine::is_ancestor(&ctx.backend, hd, hs)? {
            // fast-forward (골든 F1) — merge commit 없이 ref만 전진
            match ctx.backend.cas_ref(dst, Some(&hd), &hs)? {
                CasOutcome::Committed => {
                    let hd_state = self.engine.materialize_state(&ctx, hd)?;
                    let hs_state = self.engine.materialize_state(&ctx, hs)?;
                    let d = diff(&hd_state, &hs_state);
                    self.publish_state(&ctx, dst, &hs_state, &d, hs)?;
                    ctx.emit(
                        dst,
                        GraphEvent::HeadAdvanced {
                            branch: dst.to_string(),
                            from: Some(hd),
                            to: hs,
                        },
                    );
                    self.notify_dependents(&ctx);
                    return Ok(MergeOutcome::FastForward(hs));
                }
                CasOutcome::Conflict { .. } => {
                    return Err(EngineError::Corrupt(
                        "concurrent ref move during merge".into(),
                    ))
                }
            }
        }

        let base = Engine::common_ancestor(&ctx.backend, hs, hd)?
            .ok_or_else(|| EngineError::Corrupt("no common ancestor".into()))?;
        let base_state = self.engine.materialize_state(&ctx, base)?;
        let ours_state = self.engine.materialize_state(&ctx, hd)?;
        let theirs_state = self.engine.materialize_state(&ctx, hs)?;
        let ours_delta = diff(&base_state, &ours_state);
        let mut theirs_delta = diff(&base_state, &theirs_state);
        let items = detect_conflicts(&ours_delta, &theirs_delta);

        // 해소 입력 (골든 F8) — v0: PropBothSet만
        let mut resolved = vec![false; items.len()];
        for r in &opts.resolutions {
            if let Some(ConflictItem::PropBothSet { elem, key, .. }) = items.get(r.item) {
                let (elem, key) = (*elem, key.clone());
                match &r.choose {
                    Choice::Theirs => {} // theirs propset 유지 → 적용 시 덮어씀
                    Choice::Ours => {
                        theirs_delta
                            .props_set
                            .retain(|p| !(p.elem == elem && p.key == key));
                    }
                    Choice::Value(v) => {
                        for p in theirs_delta.props_set.iter_mut() {
                            if p.elem == elem && p.key == key {
                                p.new = Some(v.clone());
                            }
                        }
                    }
                }
                resolved[r.item] = true;
            }
        }
        let items: Vec<ConflictItem> = items
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !resolved[*i])
            .map(|(_, it)| it)
            .collect();
        if !items.is_empty() {
            return Ok(MergeOutcome::Conflicts(ConflictReport {
                graph: ctx.id,
                base: Some(base),
                ours: SideRef::Commit(hd),
                theirs: SideRef::Commit(hs),
                items,
            }));
        }

        let pruned = prune_delta(&theirs_delta, &ours_state);
        let mut merged = ours_state.clone();
        if let Err(e) = apply(&mut merged, &pruned) {
            return Ok(MergeOutcome::Conflicts(ConflictReport {
                graph: ctx.id,
                base: Some(base),
                ours: SideRef::Commit(hd),
                theirs: SideRef::Commit(hs),
                items: vec![ConflictItem::ApplyFailed {
                    reason: e.to_string(),
                }],
            }));
        }
        let merged_delta = diff(&ours_state, &merged); // 빈 delta 허용 (F9 수렴 병합)

        // 스키마 게이트: 각자는 valid여도 병합 결과가 위반이면 SchemaClash (골든 F7)
        if let Some(schema) = Schema::of_state(&merged) {
            if schema.mode == SchemaMode::Strict {
                let (tn, te) = touched_of(&merged_delta, &merged);
                let issues = schema.validate_elements(&merged, &tn, &te);
                if !issues.is_empty() {
                    return Ok(MergeOutcome::Conflicts(ConflictReport {
                        graph: ctx.id,
                        base: Some(base),
                        ours: SideRef::Commit(hd),
                        theirs: SideRef::Commit(hs),
                        items: issues
                            .into_iter()
                            .map(|issue| ConflictItem::SchemaClash { issue })
                            .collect(),
                    }));
                }
            }
        }
        let commit = Commit {
            parents: vec![hd, hs], // dst 먼저 (골든 F2)
            delta: merged_delta.hash(),
            schema_rev: None,
            author: self.user.clone(),
            ts: self.engine.inner.clock.now(),
            msg: format!("merge {src} into {dst}"),
        };
        let chash = commit.hash();
        ctx.backend.put_blob(
            &merged_delta.hash(),
            merged_delta.canonical_json().as_bytes(),
        )?;
        ctx.backend
            .put_blob(&chash, commit.canonical_json().as_bytes())?;
        match ctx.backend.cas_ref(dst, Some(&hd), &chash)? {
            CasOutcome::Committed => {
                self.publish_state(&ctx, dst, &merged, &merged_delta, chash)?;
                ctx.emit(
                    dst,
                    GraphEvent::HeadAdvanced {
                        branch: dst.to_string(),
                        from: Some(hd),
                        to: chash,
                    },
                );
                self.notify_dependents(&ctx);
                Ok(MergeOutcome::Merged(chash))
            }
            CasOutcome::Conflict { .. } => Err(EngineError::Corrupt(
                "concurrent ref move during merge".into(),
            )),
        }
    }
}

/// delta가 건드린 (노드, 엣지) 집합 — 스키마 게이트·발행 경로 공용.
fn touched_of(delta: &Delta, state: &GraphState) -> (BTreeSet<Uuid>, BTreeSet<Uuid>) {
    let mut tn = BTreeSet::new();
    let mut te = BTreeSet::new();
    for n in &delta.nodes_added {
        tn.insert(n.uuid);
    }
    for e in &delta.edges_added {
        te.insert(e.uuid);
    }
    for p in &delta.props_set {
        if state.node(p.elem).is_some() {
            tn.insert(p.elem);
        } else if state.edge(p.elem).is_some() {
            te.insert(p.elem);
        }
    }
    for l in &delta.labels_delta {
        tn.insert(l.elem);
    }
    for s in &delta.slugs_set {
        tn.insert(s.elem);
    }
    (tn, te)
}

// ================= P5: KG 오버레이 창구 =================

impl Session {
    /// 스키마 설정 = commit (설계 §2.2 "스키마 변경도 commit", 골든 H4).
    pub fn set_schema(
        &self,
        g: GraphId,
        branch: &str,
        schema: &Schema,
    ) -> EngineResult<CommitOutcome> {
        let es = match self.edit(g, branch) {
            Ok(es) => es,
            Err(EditStartError::Engine(e)) => return Err(e),
            Err(EditStartError::NotHead { .. }) => {
                return Err(EngineError::Corrupt("unexpected NotHead".into()))
            }
        };
        let props = schema.to_props();
        match es.by_slug(SCHEMA_SLUG) {
            Some(u) => {
                for (k, v) in props {
                    es.set_prop(u, &k, v)?;
                }
            }
            None => {
                es.add_node(&["__system"], Some(SCHEMA_SLUG), props)?;
            }
        }
        self.commit(&es, "set schema")
    }

    pub fn schema(&self, g: GraphId, c: Hash256) -> EngineResult<Option<Schema>> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let state = self.engine.materialize_state(&ctx, c)?;
        Ok(Schema::of_state(&state))
    }

    /// 소급 dry-run 검증 (골든 H5) — 상태 무변화.
    pub fn validate(
        &self,
        g: GraphId,
        c: Hash256,
        schema: &Schema,
    ) -> EngineResult<Vec<SchemaIssue>> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let state = self.engine.materialize_state(&ctx, c)?;
        Ok(schema.validate_full(&state))
    }
}

// ================= P7: GC (설계 §5.7) =================

#[derive(Debug, Clone)]
pub struct GcPlan {
    /// 브랜치 head ∪ 외부 수신 핀 (설계 §7.3 — 핀은 GC 루트, 골든 I5).
    pub roots: Vec<Hash256>,
    pub keep_blobs: BTreeSet<Hash256>,
    pub keep_payloads: BTreeSet<Hash256>,
    pub orphan_blobs: Vec<Hash256>,
    pub orphan_payloads: Vec<Hash256>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcReport {
    pub removed_blobs: u64,
    pub removed_payloads: u64,
}

impl Session {
    pub fn gc_plan(&self, g: GraphId) -> EngineResult<GcPlan> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let mut roots: Vec<Hash256> = ctx
            .backend
            .list_refs()?
            .into_iter()
            .map(|(_, h)| h)
            .collect();
        // 외부 수신 핀: 다른 그래프 main head lockfile에서 나를 가리키는 pin
        let ids: Vec<GraphId> = self
            .engine
            .inner
            .entries
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        for id in ids {
            if id == ctx.id {
                continue;
            }
            let Ok(octx) = self.engine.ctx(id) else {
                continue;
            };
            let Ok(Some(head)) = octx.backend.get_ref("main") else {
                continue;
            };
            let Ok(state) = self.engine.materialize_state(&octx, head) else {
                continue;
            };
            if let Some(lf) = state.by_slug(LOCKFILE_SLUG) {
                for (_, v) in as_map(state.node(lf).unwrap().props.get("targets")) {
                    if let Value::Map(m) = v {
                        if map_str(&m, "graph").and_then(|s| s.parse::<GraphId>().ok())
                            == Some(ctx.id)
                        {
                            if let Some(pin) = map_str(&m, "pin").and_then(|s| s.parse().ok()) {
                                roots.push(pin);
                            }
                        }
                    }
                }
            }
        }
        roots.sort();
        roots.dedup();

        let mut keep_blobs: BTreeSet<Hash256> = BTreeSet::new();
        let mut keep_payloads: BTreeSet<Hash256> = BTreeSet::new();
        for &root in &roots {
            for h in Engine::ancestors(&ctx.backend, root)? {
                let c = Engine::load_commit(&ctx.backend, &h)?;
                keep_blobs.insert(h);
                keep_blobs.insert(c.delta);
            }
            // payload keep-set = 루트 상태의 payload (rows lazy 로드 재료).
            // 과거 commit은 delta blob에 payload 전문이 내장 → fold로 재구성 가능 = 회수 안전.
            let state = self.engine.materialize_state(&ctx, root)?;
            for n in state.nodes() {
                keep_payloads.insert(payload_hash(&n.props));
            }
            for e in state.edges() {
                keep_payloads.insert(payload_hash(&e.props));
            }
        }
        let orphan_blobs = ctx
            .backend
            .list_blobs()?
            .into_iter()
            .filter(|h| !keep_blobs.contains(h))
            .collect();
        let orphan_payloads = ctx
            .backend
            .list_payloads()?
            .into_iter()
            .filter(|h| !keep_payloads.contains(h))
            .collect();
        Ok(GcPlan {
            roots,
            keep_blobs,
            keep_payloads,
            orphan_blobs,
            orphan_payloads,
        })
    }

    /// mark & sweep 실행. detached commit(어느 ref에서도 미도달)도 회수 대상 —
    /// D9b의 "히스토리에 남는다"는 GC 전까지다 (git reflog 만료와 동일 정책).
    pub fn gc_run(&self, g: GraphId) -> EngineResult<GcReport> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let plan = self.gc_plan(g)?;
        for h in &plan.orphan_blobs {
            ctx.backend.delete_blob(h)?;
        }
        for h in &plan.orphan_payloads {
            ctx.backend.delete_payload(h)?;
        }
        Ok(GcReport {
            removed_blobs: plan.orphan_blobs.len() as u64,
            removed_payloads: plan.orphan_payloads.len() as u64,
        })
    }
}

// ================= P6: external link (설계 §7, lmd 패턴 이식) =================

const LOCKFILE_SLUG: &str = "__lockfile";

#[derive(Debug, Clone, PartialEq)]
pub struct LockEntry {
    pub graph: GraphId,
    pub branch: String,
    pub pin: Hash256,
}

fn as_map(v: Option<&Value>) -> BTreeMap<SmolStr, Value> {
    match v {
        Some(Value::Map(m)) => m.clone(),
        _ => BTreeMap::new(),
    }
}

fn map_str(m: &BTreeMap<SmolStr, Value>, k: &str) -> Option<String> {
    match m.get(k) {
        Some(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

impl Session {
    /// commit 전 훅: delta의 새 Ref 값을 해석해 lockfile 시스템 노드에 핀 기록.
    /// 같은 대상 그래프 = 단일 핀 공유 (골든 I6). 핀에 노드가 없으면 head로 핀 승격.
    fn integrate_links(&self, ctx: &GraphCtx, w: &mut WorkInner) -> EngineResult<()> {
        let delta = w.builder.build();
        let mut addrs: Vec<String> = Vec::new();
        for n in &delta.nodes_added {
            for v in n.props.values() {
                if let Value::Ref(a) = v {
                    addrs.push(a.to_string());
                }
            }
        }
        for e in &delta.edges_added {
            for v in e.props.values() {
                if let Value::Ref(a) = v {
                    addrs.push(a.to_string());
                }
            }
        }
        for p in &delta.props_set {
            if let Some(Value::Ref(a)) = &p.new {
                addrs.push(a.to_string());
            }
        }
        if addrs.is_empty() {
            return Ok(());
        }

        let lf = match w.builder.work.by_slug(LOCKFILE_SLUG) {
            Some(u) => u,
            None => {
                let u = w.idgen.next_uuid();
                w.builder.work.insert_node(vgraph_core::Node {
                    uuid: u,
                    labels: vec![SmolStr::new("__system")],
                    slug: Some(SmolStr::new(LOCKFILE_SLUG)),
                    props: Props::new(),
                })?;
                u
            }
        };
        let mut targets = as_map(w.builder.work.node(lf).unwrap().props.get("targets"));
        let mut nodes = as_map(w.builder.work.node(lf).unwrap().props.get("nodes"));

        for addr in addrs {
            let a: GraphAddr = addr
                .parse()
                .map_err(|e| EngineError::Link(format!("잘못된 링크 주소 {addr}: {e}")))?;
            let key = format!("{}/{}", a.ns, a.slug);
            let gid = match self.engine.lookup(&a.ns, &a.slug) {
                Lookup::Found(id) => id,
                Lookup::Ambiguous(_) => {
                    return Err(EngineError::Link(format!(
                        "링크 대상 이름이 레지스트리에 중복돼 있음: {key}"
                    )))
                }
                Lookup::Missing => {
                    return Err(EngineError::Link(format!("미등록 링크 대상: {key}")))
                }
            };
            if gid == ctx.id {
                continue; // 그래프 내부 참조는 lockfile 불요
            }
            let tctx = self.engine.ctx(gid)?;
            let branch = a.branch.as_deref().unwrap_or("main").to_string();
            let head = tctx
                .backend
                .get_ref(&branch)?
                .ok_or(ResolveError::UnknownBranch)?;
            let existing_pin: Option<Hash256> = match targets.get(key.as_str()) {
                Some(Value::Map(m)) => map_str(m, "pin").and_then(|s| s.parse().ok()),
                _ => None,
            };
            let sel = a
                .node
                .clone()
                .ok_or_else(|| EngineError::Link(format!("링크는 노드까지 지정: {addr}")))?;

            // 핀에서 해석 시도 → 없으면 head로 핀 승격 (I6: 신규 노드 링크)
            let resolve_at = |pin: Hash256| -> EngineResult<Option<(Uuid, Hash256)>> {
                let tstate = self.engine.materialize_state(&tctx, pin)?;
                let nuuid = match &sel {
                    NodeSel::Slug(s) => tstate.by_slug(s),
                    NodeSel::Uuid(u) => tstate.node(*u).map(|n| n.uuid),
                };
                Ok(nuuid.map(|u| (u, payload_hash(&tstate.node(u).unwrap().props))))
            };
            let (pin, nuuid, nhash) = match existing_pin {
                Some(p) => match resolve_at(p)? {
                    Some((u, h)) => (p, u, h),
                    None => {
                        let (u, h) = resolve_at(head)?.ok_or_else(|| {
                            EngineError::Link(format!("링크 대상 노드 없음: {addr}"))
                        })?;
                        (head, u, h)
                    }
                },
                None => {
                    let (u, h) = resolve_at(head)?
                        .ok_or_else(|| EngineError::Link(format!("링크 대상 노드 없음: {addr}")))?;
                    (head, u, h)
                }
            };

            // 핀 승격 시 같은 대상의 기존 노드 해시 재계산 (단일 핀 일관성)
            if existing_pin.is_some() && existing_pin != Some(pin) {
                let tstate = self.engine.materialize_state(&tctx, pin)?;
                for (naddr, v) in nodes.clone() {
                    let Ok(na) = naddr.as_str().parse::<GraphAddr>() else {
                        continue;
                    };
                    if format!("{}/{}", na.ns, na.slug) != key {
                        continue;
                    }
                    if let Value::Map(mut e) = v {
                        if let Some(u) = map_str(&e, "uuid").and_then(|s| s.parse::<Uuid>().ok()) {
                            if let Some(n) = tstate.node(u) {
                                e.insert(
                                    SmolStr::new("hash"),
                                    Value::Str(payload_hash(&n.props).to_string()),
                                );
                                nodes.insert(naddr, Value::Map(e));
                            }
                        }
                    }
                }
            }

            let mut tmap = BTreeMap::new();
            tmap.insert(SmolStr::new("graph"), Value::Str(gid.to_string()));
            tmap.insert(SmolStr::new("branch"), Value::Str(branch));
            tmap.insert(SmolStr::new("pin"), Value::Str(pin.to_string()));
            targets.insert(SmolStr::new(&key), Value::Map(tmap));
            let mut nmap = BTreeMap::new();
            nmap.insert(SmolStr::new("uuid"), Value::Str(nuuid.to_string()));
            nmap.insert(SmolStr::new("hash"), Value::Str(nhash.to_string()));
            nodes.insert(SmolStr::new(&addr), Value::Map(nmap));
        }

        w.builder
            .work
            .set_prop(lf, "targets", Value::Map(targets))?;
        w.builder.work.set_prop(lf, "nodes", Value::Map(nodes))?;
        Ok(())
    }

    pub fn lockfile(&self, g: GraphId, branch: &str) -> EngineResult<BTreeMap<String, LockEntry>> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let head = ctx
            .backend
            .get_ref(branch)?
            .ok_or(ResolveError::UnknownBranch)?;
        let state = self.engine.materialize_state(&ctx, head)?;
        let mut out = BTreeMap::new();
        if let Some(lf) = state.by_slug(LOCKFILE_SLUG) {
            for (k, v) in as_map(state.node(lf).unwrap().props.get("targets")) {
                if let Value::Map(m) = v {
                    if let (Some(gid), Some(br), Some(pin)) = (
                        map_str(&m, "graph"),
                        map_str(&m, "branch"),
                        map_str(&m, "pin"),
                    ) {
                        if let (Ok(gid), Ok(pin)) = (gid.parse(), pin.parse()) {
                            out.insert(
                                k.to_string(),
                                LockEntry {
                                    graph: gid,
                                    branch: br,
                                    pin,
                                },
                            );
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// head 기준 드리프트 목록 (골든 I2·I3) — 핀 노드 해시 vs 현재 대상 head 해시.
    pub fn drifts(&self, g: GraphId, branch: &str) -> EngineResult<Vec<DriftItem>> {
        let ctx = self.engine.ctx(g)?;
        self.acl(&ctx)?;
        let head = ctx
            .backend
            .get_ref(branch)?
            .ok_or(ResolveError::UnknownBranch)?;
        let state = self.engine.materialize_state(&ctx, head)?;
        self.drift_items(&state)
    }

    fn drift_items(&self, state: &GraphState) -> EngineResult<Vec<DriftItem>> {
        let Some(lf) = state.by_slug(LOCKFILE_SLUG) else {
            return Ok(vec![]);
        };
        let node = state.node(lf).unwrap();
        let targets = as_map(node.props.get("targets"));
        let nodes = as_map(node.props.get("nodes"));
        let mut out = Vec::new();
        for (addr, entry) in &nodes {
            let Ok(a) = addr.as_str().parse::<GraphAddr>() else {
                continue;
            };
            let key = format!("{}/{}", a.ns, a.slug);
            let Some(Value::Map(t)) = targets.get(key.as_str()) else {
                continue;
            };
            let (Some(gid), Some(branch), Some(pin)) =
                (map_str(t, "graph"), map_str(t, "branch"), map_str(t, "pin"))
            else {
                continue;
            };
            let (Ok(gid), Ok(pin)) = (gid.parse::<GraphId>(), pin.parse::<Hash256>()) else {
                continue;
            };
            let Value::Map(e) = entry else { continue };
            let (Some(nu), Some(nh)) = (map_str(e, "uuid"), map_str(e, "hash")) else {
                continue;
            };
            let (Ok(nu), Ok(pinned_hash)) = (nu.parse::<Uuid>(), nh.parse::<Hash256>()) else {
                continue;
            };
            let Ok(tctx) = self.engine.ctx(gid) else {
                continue;
            };
            let Ok(Some(cur_head)) = tctx.backend.get_ref(&branch) else {
                continue;
            };
            if cur_head == pin {
                continue; // 대상 전진 없음
            }
            let cur_state = self.engine.materialize_state(&tctx, cur_head)?;
            let current_hash = match cur_state.node(nu) {
                Some(n) => payload_hash(&n.props),
                None => Hash256::default(),
            };
            if current_hash != pinned_hash {
                out.push(DriftItem {
                    target: key,
                    node: nu,
                    pin,
                    current_head: cur_head,
                    pinned_hash,
                    current_hash,
                });
            }
        }
        Ok(out)
    }

    /// commit/merge 후: 나를 핀한 다른 그래프들에 드리프트 이벤트 통지 (골든 I2).
    fn notify_dependents(&self, committed: &GraphCtx) {
        let my_key = format!("{}/{}", committed.ns, committed.slug);
        let ids: Vec<GraphId> = self
            .engine
            .inner
            .entries
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        for id in ids {
            if id == committed.id {
                continue;
            }
            let Ok(octx) = self.engine.ctx(id) else {
                continue;
            };
            let Ok(Some(head)) = octx.backend.get_ref("main") else {
                continue;
            };
            let Ok(state) = self.engine.materialize_state(&octx, head) else {
                continue;
            };
            let Ok(items) = self.drift_items(&state) else {
                continue;
            };
            for item in items.into_iter().filter(|i| i.target == my_key) {
                octx.emit_all(GraphEvent::Drift { item });
            }
        }
    }

    /// 핀 갱신 = commit (설계 §7.2, 골든 I4).
    pub fn link_upgrade(
        &self,
        g: GraphId,
        branch: &str,
        target: &str,
        to: Hash256,
    ) -> EngineResult<CommitOutcome> {
        let es = match self.edit(g, branch) {
            Ok(es) => es,
            Err(EditStartError::Engine(e)) => return Err(e),
            Err(EditStartError::NotHead { .. }) => {
                return Err(EngineError::Corrupt("unexpected NotHead".into()))
            }
        };
        let lf = es
            .by_slug(LOCKFILE_SLUG)
            .ok_or_else(|| EngineError::Link("external link 없음".into()))?;
        let mut targets = match es.node_prop(lf, "targets") {
            Some(Value::Map(m)) => m,
            _ => BTreeMap::new(),
        };
        let mut nodes = match es.node_prop(lf, "nodes") {
            Some(Value::Map(m)) => m,
            _ => BTreeMap::new(),
        };
        let Some(Value::Map(tmap)) = targets.get(target).cloned() else {
            return Err(EngineError::Link(format!("미등록 링크 대상: {target}")));
        };
        let gid: GraphId = map_str(&tmap, "graph")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| EngineError::Link("lockfile 파싱 실패".into()))?;
        let tctx = self.engine.ctx(gid)?;
        let tstate = self.engine.materialize_state(&tctx, to)?;
        let mut tmap = tmap;
        tmap.insert(SmolStr::new("pin"), Value::Str(to.to_string()));
        targets.insert(SmolStr::new(target), Value::Map(tmap));
        for (addr, v) in nodes.clone() {
            let Ok(a) = addr.as_str().parse::<GraphAddr>() else {
                continue;
            };
            if format!("{}/{}", a.ns, a.slug) != target {
                continue;
            }
            if let Value::Map(mut e) = v {
                if let Some(nu) = map_str(&e, "uuid").and_then(|s| s.parse::<Uuid>().ok()) {
                    if let Some(n) = tstate.node(nu) {
                        e.insert(
                            SmolStr::new("hash"),
                            Value::Str(payload_hash(&n.props).to_string()),
                        );
                        nodes.insert(addr, Value::Map(e));
                    }
                }
            }
        }
        es.set_prop(lf, "targets", Value::Map(targets))?;
        es.set_prop(lf, "nodes", Value::Map(nodes))?;
        self.commit(&es, &format!("link upgrade {target}"))
    }

    /// Ref 해석 — 항상 lockfile 핀 버전 기준 (골든 I7). 대상 전진과 무관.
    pub fn deref(&self, from: &View, addr: &str) -> EngineResult<(Arc<View>, Uuid)> {
        let a: GraphAddr = addr
            .parse()
            .map_err(|e| EngineError::Link(format!("잘못된 주소: {e}")))?;
        let key = format!("{}/{}", a.ns, a.slug);
        let lf = from
            .by_slug(LOCKFILE_SLUG)
            .ok_or_else(|| EngineError::Link("external link 없음".into()))?;
        let targets = match from.node_prop(lf, "targets") {
            Some(Value::Map(m)) => m,
            _ => BTreeMap::new(),
        };
        let Some(Value::Map(t)) = targets.get(key.as_str()) else {
            return Err(EngineError::Corrupt(format!("unknown link target {key}")));
        };
        let gid: GraphId = map_str(t, "graph")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| EngineError::Link("lockfile 파싱 실패".into()))?;
        let pin: Hash256 = map_str(t, "pin")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| EngineError::Link("lockfile 파싱 실패".into()))?;
        let v = self.checkout(gid, pin)?;
        let nodes = match from.node_prop(lf, "nodes") {
            Some(Value::Map(m)) => m,
            _ => BTreeMap::new(),
        };
        let uuid = match nodes.get(addr) {
            Some(Value::Map(e)) => map_str(e, "uuid").and_then(|s| s.parse().ok()),
            _ => None,
        }
        .or_else(|| match &a.node {
            Some(NodeSel::Slug(s)) => v.by_slug(s),
            Some(NodeSel::Uuid(u)) => Some(*u),
            None => None,
        })
        .ok_or_else(|| EngineError::Link("링크 노드 해석 실패".into()))?;
        Ok((v, uuid))
    }
}
