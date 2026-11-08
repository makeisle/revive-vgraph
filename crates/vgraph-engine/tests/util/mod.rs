//! 엔진 골든 테스트 공용 유틸 — F0을 공개 창구(Session)로만 구축.
//! TestIdGen 소비 순서 = 골든 §1.1 (create_graph=1, edit 세션=2, n1..=3..).

// 시험 바이너리마다 쓰는 도구가 달라 일부는 쓰이지 않는다.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use vgraph_core::canon::payload_hash;
use vgraph_core::value::props;
use vgraph_core::*;
use vgraph_engine::*;
use vgraph_store::{
    BlobStore, CrashPoint, FaultBackend, HeadProvision, RefStore, SqliteBackend, StorageBackend,
    StoreResult,
};

pub struct Env {
    pub _dir: TempDir,
    pub engine: Engine,
}

pub fn new_env(auto_upsert: bool) -> Env {
    let dir = TempDir::new().unwrap();
    let engine = engine_at(dir.path(), auto_upsert, 1);
    Env { _dir: dir, engine }
}

pub fn engine_at(root: &Path, auto_upsert: bool, idgen_start: u64) -> Engine {
    Engine::open(
        Box::new(FsProvider::new(root).unwrap()),
        Arc::new(TestIdGen::starting_at(idgen_start)),
        Arc::new(TestClock::default()),
        EngineOptions {
            auto_upsert_head: auto_upsert,
        },
    )
    .unwrap()
}

#[allow(dead_code)] // 테스트 바이너리별로 일부 필드만 사용
pub struct EngineF0 {
    pub g: GraphId,
    pub n: [Uuid; 5],
    pub e: [Uuid; 4],
    pub c0: Hash256,
}

/// 공개 창구로 F0 구축 + core 골든 lock의 C0과 일치 검증 (표현 경로 간 항등).
pub fn build_f0_engine(sess: &Session) -> EngineF0 {
    let g = sess.create_graph("acme", "bookclub").unwrap();
    let es = sess.edit(g, "main").unwrap();
    let n1 = es
        .add_node(
            &["Capability"],
            Some("cap-auth"),
            props([
                ("name", "회원 인증".into()),
                ("status", "active".into()),
                ("priority", 1.into()),
            ]),
        )
        .unwrap();
    let n2 = es
        .add_node(
            &["Capability"],
            Some("cap-loan"),
            props([
                ("name", "도서 대출".into()),
                ("status", "active".into()),
                ("priority", 2.into()),
            ]),
        )
        .unwrap();
    let n3 = es
        .add_node(
            &["Requirement"],
            Some("req-001"),
            props([("text", "이메일 로그인".into()), ("status", "draft".into())]),
        )
        .unwrap();
    let n4 = es
        .add_node(
            &["Requirement"],
            Some("req-002"),
            props([
                ("text", "대출 한도 5권".into()),
                ("status", "active".into()),
            ]),
        )
        .unwrap();
    let n5 = es
        .add_node(
            &["Policy"],
            Some("pol-perf"),
            props([("name", "응답 200ms".into())]),
        )
        .unwrap();
    let e1 = es.add_edge("parent", n3, n1, Props::new()).unwrap();
    let e2 = es.add_edge("parent", n4, n2, Props::new()).unwrap();
    let e3 = es
        .add_edge("impacts", n2, n1, props([("weight", 0.7.into())]))
        .unwrap();
    let e4 = es.add_edge("policy", n4, n5, Props::new()).unwrap();

    let c0 = match sess.commit(&es, "F0").unwrap() {
        CommitOutcome::Committed { commit, .. } => commit,
        other => panic!("F0 commit failed: {other:?}"),
    };

    // 엔진 경로 C0 ≡ core 픽스처 C0 (골든 lock과 동일) — canonical 항등의 교차 검증
    assert_eq!(
        c0,
        vgraph_core::fixtures::build_f0().c0,
        "engine C0 != core C0"
    );

    EngineF0 {
        g,
        n: [n1, n2, n3, n4, n5],
        e: [e1, e2, e3, e4],
        c0,
    }
}

// ======================================================================
// 종료 지점 복구 시험 공용 도구 (crash_recovery.rs — Step 3·4에서도 재사용)
//
// 고장 주입은 엔진 수준 `FaultBackend`(논리 절단). 불변식 검사는 엔진을 거치지 않고
// 저장소 파일을 직접 열어 판정한다 (읽기 색인 rows <-> 처음부터 접은 상태 대조).
// ======================================================================

/// 엔진이 연 그래프 백엔드(FaultBackend)를 시험이 잡을 수 있게 하는 칸.
pub type FaultSlot = Arc<Mutex<Option<Arc<FaultBackend<SqliteBackend>>>>>;

/// `FsProvider`와 같되 그래프 백엔드만 `FaultBackend`로 감싼다.
pub struct FaultProvider {
    fs: FsProvider,
    root: PathBuf,
    slot: FaultSlot,
}

impl GraphStoreProvider for FaultProvider {
    fn open_graph(&self, id: GraphId) -> StoreResult<Arc<dyn StorageBackend + Send + Sync>> {
        let b = Arc::new(FaultBackend::new(SqliteBackend::open(
            &self.root.join(format!("{id}.db")),
        )?));
        *self.slot.lock().unwrap() = Some(b.clone());
        Ok(b)
    }
    fn registry(&self) -> StoreResult<Vec<RegistryEntry>> {
        self.fs.registry()
    }
    fn register(&self, e: &RegistryEntry) -> StoreResult<()> {
        self.fs.register(e)
    }
    fn register_new(&self, e: &RegistryEntry) -> StoreResult<bool> {
        self.fs.register_new(e)
    }
}

/// 고장 주입이 가능한 엔진. `idgen_start`는 엔진 세대마다 다르게 준다 (uuid·작업본 id 충돌 방지).
pub fn fault_engine(root: &Path, idgen_start: u64) -> (Engine, FaultSlot) {
    let slot: FaultSlot = Arc::new(Mutex::new(None));
    let provider = FaultProvider {
        fs: FsProvider::new(root).unwrap(),
        root: root.to_path_buf(),
        slot: slot.clone(),
    };
    let engine = Engine::open(
        Box::new(provider),
        Arc::new(TestIdGen::starting_at(idgen_start)),
        Arc::new(TestClock::default()),
        EngineOptions::default(),
    )
    .unwrap();
    (engine, slot)
}

/// 슬롯의 백엔드를 지정 지점에서 끊기게 한다 (그래프를 한 번 연 뒤여야 한다).
pub fn arm(slot: &FaultSlot, point: CrashPoint) {
    slot.lock()
        .unwrap()
        .as_ref()
        .expect("그래프가 아직 열리지 않음")
        .arm(point);
}

/// 다음 `upsert_head`가 적용된 뒤 오류를 돌려주게 한다 (P2: 발행 도중 끊김). `disarm`으로 푼다.
pub fn arm_after_upsert_head(slot: &FaultSlot) {
    slot.lock()
        .unwrap()
        .as_ref()
        .expect("그래프가 아직 열리지 않음")
        .arm_after_upsert_head();
}

/// `drop_wal`이 오류를 돌려주게 한다 (H-C4: 작업본 정리 실패). `disarm`으로 푼다.
pub fn arm_drop_wal_fails(slot: &FaultSlot) {
    slot.lock()
        .unwrap()
        .as_ref()
        .expect("그래프가 아직 열리지 않음")
        .arm_drop_wal_fails();
}

pub fn disarm(slot: &FaultSlot) {
    slot.lock()
        .unwrap()
        .as_ref()
        .expect("그래프가 아직 열리지 않음")
        .disarm();
}

/// MCP `find_work`와 같은 방식: 같은 작업 공간의 남은 작업본이 있으면 되살리고, 없으면 새로 연다.
pub fn open_ws(s: &Session, g: GraphId, ws: &str) -> EditSession {
    for wid in s.list_working(g).unwrap() {
        if let Ok(es) = s.resume(g, wid) {
            if es.branch().as_deref() == Some("main") && es.workspace().as_deref() == Some(ws) {
                return es;
            }
        }
    }
    s.edit_in(g, "main", Some(ws)).unwrap()
}

/// 작업 공간 `ws`에서 노드 하나(slug)를 추가하고 커밋한다.
pub fn commit_node(s: &Session, g: GraphId, ws: &str, slug: &str) -> CommitOutcome {
    let es = open_ws(s, g, ws);
    es.add_node(&["T"], Some(slug), props([("v", slug.into())]))
        .unwrap();
    s.commit(&es, slug).unwrap()
}

/// 저장소 파일을 직접 연다 (엔진 밖 관찰용).
pub fn raw_backend(root: &Path, g: GraphId) -> SqliteBackend {
    SqliteBackend::open(&root.join(format!("{g}.db"))).unwrap()
}

fn load_commit(b: &SqliteBackend, h: &Hash256) -> Result<Commit, String> {
    let bytes = b
        .get_blob(h)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("커밋 블롭 없음 {h}"))?;
    Commit::from_json(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

fn load_delta(b: &SqliteBackend, h: &Hash256) -> Result<Delta, String> {
    let bytes = b
        .get_blob(h)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("delta 블롭 없음 {h}"))?;
    Delta::from_json(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

/// 첫 부모만 따라간 체인 (head 먼저). 병합 커밋의 둘째 부모는 따라가지 않는다.
pub fn first_parent_chain(b: &SqliteBackend, head: Hash256) -> Result<Vec<Commit>, String> {
    let mut out = Vec::new();
    let mut cur = Some(head);
    while let Some(h) = cur {
        let c = load_commit(b, &h)?;
        cur = c.parents.first().copied();
        out.push(c);
    }
    Ok(out)
}

/// 읽기 색인·워터마크를 쓰지 않고 불변 블롭만으로 처음부터 접는다 (불변식 ③의 판정 수단).
pub fn fold_from_blobs(b: &SqliteBackend, head: Hash256) -> Result<GraphState, String> {
    let mut state = GraphState::new();
    for c in first_parent_chain(b, head)?.iter().rev() {
        let d = load_delta(b, &c.delta)?;
        apply(&mut state, &d).map_err(|e| format!("처음부터 접기 실패: {e}"))?;
    }
    Ok(state)
}

/// 불변식 ①: uuid 기준 읽기 색인 rows == 처음부터 접은 상태 (내용 해시까지).
pub fn check_rows_match_fold(b: &SqliteBackend, branch: &str) -> Result<(), String> {
    let head = b
        .get_ref(branch)
        .map_err(|e| e.to_string())?
        .ok_or("브랜치 없음")?;
    let folded = fold_from_blobs(b, head)?;
    let want_nodes: BTreeMap<Uuid, Hash256> = folded
        .nodes()
        .map(|n| (n.uuid, payload_hash(&n.props)))
        .collect();
    let want_edges: BTreeMap<Uuid, Hash256> = folded
        .edges()
        .map(|e| (e.uuid, payload_hash(&e.props)))
        .collect();
    let got_nodes: BTreeMap<Uuid, Hash256> = b
        .scan_nodes(branch)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| (r.uuid, r.payload))
        .collect();
    let got_edges: BTreeMap<Uuid, Hash256> = b
        .scan_edges(branch)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| (r.uuid, r.payload))
        .collect();
    if want_nodes != got_nodes {
        return Err(format!(
            "노드 rows 불일치: 접은 상태 {} / rows {} / 접은 상태에만 있음 {:?} / rows에만 있음 {:?}",
            want_nodes.len(),
            got_nodes.len(),
            want_nodes
                .keys()
                .filter(|k| !got_nodes.contains_key(k))
                .collect::<Vec<_>>(),
            got_nodes
                .keys()
                .filter(|k| !want_nodes.contains_key(k))
                .collect::<Vec<_>>(),
        ));
    }
    if want_edges != got_edges {
        return Err(format!(
            "엣지 rows 불일치: 접은 상태 {} / rows {}",
            want_edges.len(),
            got_edges.len()
        ));
    }
    Ok(())
}

/// 불변식 ④: 첫 부모 체인 안에서 같은 uuid의 추가가 2번 이상 없다.
pub fn check_no_double_add(b: &SqliteBackend, branch: &str) -> Result<(), String> {
    let head = b
        .get_ref(branch)
        .map_err(|e| e.to_string())?
        .ok_or("브랜치 없음")?;
    let mut adds: BTreeSet<Uuid> = BTreeSet::new();
    for c in first_parent_chain(b, head)? {
        let d = load_delta(b, &c.delta)?;
        let ids = d
            .nodes_added
            .iter()
            .map(|n| n.uuid)
            .chain(d.edges_added.iter().map(|e| e.uuid));
        for u in ids {
            if !adds.insert(u) {
                return Err(format!("같은 uuid를 체인에서 두 번 추가: {u}"));
            }
        }
    }
    Ok(())
}

/// 불변식 ①③④ + (`expect_watermark_at_head`일 때) ②. ②는 "다음 커밋이 끝난 뒤" 시점에만 요구한다.
pub fn check_invariants(
    root: &Path,
    g: GraphId,
    branch: &str,
    expect_watermark_at_head: bool,
) -> Result<(), String> {
    let b = raw_backend(root, g);
    let head = b
        .get_ref(branch)
        .map_err(|e| e.to_string())?
        .ok_or("브랜치 없음")?;
    fold_from_blobs(&b, head)?; // ③
    check_no_double_add(&b, branch)?; // ④
    if expect_watermark_at_head {
        let wm = b.watermark(branch).map_err(|e| e.to_string())?;
        if wm != Some(head) {
            return Err(format!("워터마크 {wm:?} != head {head}")); // ②
        }
    }
    check_rows_match_fold(&b, branch) // ①
}

/// 불변식 ⑥(⑦ 포함): 엔진이 작업본을 되살려 본 뒤에도, "추가 내용이 전부 이미 head 체인에 들어 있는"
/// 작업본이 이름만 남아 있거나, 남은 작업본이 되살려지지 않아 막히는 일이 없다.
/// `find_work`와 같은 방식으로 먼저 한 번씩 되살려 본 다음 남은 것을 검사한다.
pub fn check_no_reflected_leftover(
    s: &Session,
    root: &Path,
    g: GraphId,
    branch: &str,
) -> Result<(), String> {
    for wid in s.list_working(g).map_err(|e| e.to_string())? {
        let _ = s.resume(g, wid); // 반영된 작업본은 여기서 정리되어야 한다
    }
    let b = raw_backend(root, g);
    let head = b
        .get_ref(branch)
        .map_err(|e| e.to_string())?
        .ok_or("브랜치 없음")?;
    let mut in_chain: BTreeSet<Uuid> = BTreeSet::new();
    for c in first_parent_chain(&b, head)? {
        let d = load_delta(&b, &c.delta)?;
        in_chain.extend(d.nodes_added.iter().map(|n| n.uuid));
        in_chain.extend(d.edges_added.iter().map(|e| e.uuid));
    }
    for wid in s.list_working(g).map_err(|e| e.to_string())? {
        let es = s
            .resume(g, wid)
            .map_err(|e| format!("남은 작업본 {wid:?}을 되살릴 수 없음(막힘): {e}"))?;
        let d = es.pending();
        let adds: Vec<Uuid> = d
            .nodes_added
            .iter()
            .map(|n| n.uuid)
            .chain(d.edges_added.iter().map(|e| e.uuid))
            .collect();
        if !adds.is_empty() && adds.iter().all(|u| in_chain.contains(u)) {
            return Err(format!(
                "내용이 이미 head 체인에 있는 작업본 {wid:?}이 이름만 남아 있음"
            ));
        }
    }
    Ok(())
}

/// 불변식 ⑤ 쪽: 엔진을 거쳐 head를 읽었을 때 보이는 slug 집합.
pub fn visible_slugs(s: &Session, root: &Path, g: GraphId, branch: &str) -> BTreeSet<String> {
    let head = raw_backend(root, g).get_ref(branch).unwrap().unwrap();
    let v = s.checkout(g, head).unwrap();
    v.to_state()
        .nodes()
        .filter_map(|n| n.slug.as_ref().map(ToString::to_string))
        .collect()
}
