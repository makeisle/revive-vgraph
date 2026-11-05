//! vgraph-vec v2 — 의미(유사도) 검색 사이드카.
//!
//! v2 확장 (2026-08-11):
//! - **노드당 다중 벡터**: PK (uuid, model, part) — 청크(part)와 모델이 각각 다중화.
//!   검색은 청크 점수를 노드 단위 **max-pooling**, 최고 청크(part)를 provenance로 반환.
//! - **EmbedSpec**: 어떤 텍스트를 임베딩하나 = 필드 선택(allowlist) + 청킹 전략
//!   (Whole/Props/Window). 정책은 사이드카 소유 — 그래프 본체 불가침.
//! - **2단계 staleness**: payload hash(빠른 경로) → 추출 텍스트 hash(정밀).
//!   임베딩 대상 아닌 prop만 변한 경우 재임베딩 없이 node_hash만 조용히 갱신.
//! - **Embedder = 가역 플러그인**: `providers::embedder_from_spec("toy" | "openai:…" |
//!   "ollama:…" | "http:…")`. 배치 호출·Result 기반 (외부 API 실패 전파).
//!
//! 불변 원칙 (v1 계승): 임베딩은 그래프 밖(*.vec.db), 큐는 계산형(멱등),
//! epoch 조인(뷰 실재+node_hash 일치), stale은 검색 제외.

pub mod providers;

use rusqlite::{params, Connection};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;
use vgraph_core::canon::sha256;
use vgraph_core::{Hash256, Uuid, Value};
use vgraph_engine::View;

pub type VecResult<T> = Result<T, VecError>;

#[derive(Debug, thiserror::Error)]
pub enum VecError {
    #[error("backend: {0}")]
    Backend(String),
    #[error("embedder: {0}")]
    Embed(String),
    #[error("dimension mismatch: expected {expected}, got {got}")]
    Dim { expected: usize, got: usize },
}

impl From<rusqlite::Error> for VecError {
    fn from(e: rusqlite::Error) -> Self {
        VecError::Backend(e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SimHit {
    pub node: Uuid,
    pub score: f32,
    /// 최고 점수 청크 (provenance) — "p:<prop>" | "full" | "w<i>".
    pub part: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingReason {
    Missing,
    Stale,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingEmbed {
    pub node: Uuid,
    pub node_hash: Hash256,
    pub reason: PendingReason,
}

// ---------- Embedder 플러그인 ----------

#[derive(Debug, thiserror::Error)]
#[error("embed provider: {0}")]
pub struct EmbedError(pub String);

/// 임베딩 생성기. 구현: 내장(HashBag) · 외부 API(OpenAI) · 로컬 서버(Ollama) · 자가 호스팅(http).
pub trait Embedder: Send + Sync {
    /// 벡터 네임스페이스 (테이블 model 컬럼) — 모델 교체 시 벡터가 섞이지 않는 격리 키.
    fn model_id(&self) -> String;
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut v = self.embed_batch(std::slice::from_ref(&text.to_string()))?;
        v.pop()
            .ok_or_else(|| EmbedError("empty batch result".into()))
    }
}

/// 결정적 해시-bag (골든 §2-V ToyEmbedder 명세): `v[(c × 2654435761) mod dim] += 1`.
/// 표면(문자) 유사도 대역 — 골든 게이트·데모 전용. 의미검색은 실 모델로.
pub struct HashBagEmbedder {
    pub dim: usize,
}

impl Default for HashBagEmbedder {
    fn default() -> Self {
        HashBagEmbedder { dim: 32 }
    }
}

impl Embedder for HashBagEmbedder {
    fn model_id(&self) -> String {
        format!("toy-{}", self.dim)
    }
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; self.dim];
                for c in t.chars() {
                    let h = (c as u32).wrapping_mul(2654435761) as usize % self.dim;
                    v[h] += 1.0;
                }
                v
            })
            .collect())
    }
}

// ---------- EmbedSpec: 무엇을 어떻게 임베딩하나 ----------

#[derive(Debug, Clone, PartialEq)]
pub enum Chunker {
    /// 전체 텍스트(라벨+선택 필드 연결) 1벡터, max_chars 절단.
    Whole { max_chars: usize },
    /// 선택된 문자열 prop당 1청크 (part = "p:<key>") — LPG 노드에 자연스러운 분할.
    Props,
    /// 문자 슬라이딩 윈도 (part = "w<i>") — 긴 단일 텍스트용.
    Window { size: usize, overlap: usize },
}

/// 노드가 임베딩 텍스트를 직접 지정하는 예약 prop 키.
/// 값 = Str 또는 List<Str> — 지정 시 **이 텍스트들만** 임베딩 (spec 추출보다 우선).
pub const EMBED_TARGET_KEY: &str = "embed_target";

#[derive(Debug, Clone, PartialEq)]
pub struct EmbedSpec {
    /// None = 모든 문자열 prop. Some = allowlist. (폴백 추출 경로에만 적용)
    pub fields: Option<Vec<String>>,
    pub chunker: Chunker,
    /// `embed_target`이 없거나 비었을 때 spec 추출로 폴백할지 (기본 on).
    /// off면 명시 타깃 있는 노드만 임베딩된다.
    pub fallback: bool,
}

impl Default for EmbedSpec {
    fn default() -> Self {
        EmbedSpec {
            fields: None,
            chunker: Chunker::Whole { max_chars: 8000 },
            fallback: true,
        }
    }
}

fn selected_props(view: &View, node: Uuid, spec: &EmbedSpec) -> Vec<(String, String)> {
    view.props_of(node)
        .unwrap_or_default()
        .iter()
        .filter(|(k, _)| {
            spec.fields
                .as_ref()
                .map_or(true, |f| f.iter().any(|x| x == k.as_str()))
        })
        .filter_map(|(k, v)| match v {
            Value::Str(s) if !s.trim().is_empty() => Some((k.to_string(), s.clone())),
            _ => None,
        })
        .collect()
}

fn joined_text(view: &View, node: Uuid, spec: &EmbedSpec) -> String {
    let mut parts: Vec<String> = view
        .node_labels(node)
        .unwrap_or_default()
        .iter()
        .map(|l| l.to_string())
        .collect();
    parts.extend(selected_props(view, node, spec).into_iter().map(|(_, t)| t));
    parts.join(" ").trim().to_string()
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 노드 → (part, text) 청크 목록. 빈 결과 = 임베딩 대상 아님.
///
/// 우선순위:
/// ① `embed_target` prop (Str | List<Str>) — 명시 타깃만, part = "t<i>"
/// ② 타깃 없음/빈 배열 → `spec.fallback` on이면 필드+청커 추출, off면 대상 아님
pub fn extract_chunks(view: &View, node: Uuid, spec: &EmbedSpec) -> Vec<(String, String)> {
    let props = view.props_of(node).unwrap_or_default();
    let targets: Vec<String> = match props.get(EMBED_TARGET_KEY) {
        Some(Value::Str(s)) if !s.trim().is_empty() => vec![s.clone()],
        Some(Value::List(xs)) => xs
            .iter()
            .filter_map(|x| match x {
                Value::Str(s) if !s.trim().is_empty() => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    };
    if !targets.is_empty() {
        return targets
            .into_iter()
            .enumerate()
            .map(|(i, t)| (format!("t{i}"), t))
            .collect();
    }
    if !spec.fallback {
        return vec![];
    }
    match &spec.chunker {
        Chunker::Whole { max_chars } => {
            let t = joined_text(view, node, spec);
            if t.is_empty() {
                vec![]
            } else {
                vec![("full".to_string(), truncate_chars(&t, *max_chars))]
            }
        }
        Chunker::Props => selected_props(view, node, spec)
            .into_iter()
            .map(|(k, t)| (format!("p:{k}"), t))
            .collect(),
        Chunker::Window { size, overlap } => {
            let t = joined_text(view, node, spec);
            if t.is_empty() {
                return vec![];
            }
            let chars: Vec<char> = t.chars().collect();
            let size = (*size).max(1);
            let step = size.saturating_sub(*overlap).max(1);
            let mut out = Vec::new();
            let mut i = 0;
            let mut w = 0;
            while i < chars.len() {
                let end = (i + size).min(chars.len());
                out.push((format!("w{w}"), chars[i..end].iter().collect()));
                if end == chars.len() {
                    break;
                }
                i += step;
                w += 1;
            }
            out
        }
    }
}

fn text_hash(t: &str) -> Hash256 {
    sha256(t.as_bytes())
}

// ---------- VecIndex (v2 스키마) ----------

const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS embeddings (
    uuid      TEXT NOT NULL,
    model     TEXT NOT NULL,
    part      TEXT NOT NULL,
    node_hash TEXT NOT NULL,
    text_hash TEXT NOT NULL,
    dim       INTEGER NOT NULL,
    vec       BLOB NOT NULL,
    PRIMARY KEY (uuid, model, part)
);
"#;

pub struct VecIndex {
    conn: Mutex<Connection>,
    model: String,
}

impl VecIndex {
    pub fn open(path: &Path, model: &str) -> VecResult<Self> {
        let conn = Connection::open(path)?;
        // v1 → v2 마이그레이션: 임베딩은 파생물 — drop-rebuild가 합법 (설계 §6-1 원칙)
        let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if ver < 2 {
            conn.execute_batch("DROP TABLE IF EXISTS embeddings;")?;
            conn.execute_batch("PRAGMA user_version = 2;")?;
        }
        conn.execute_batch(SCHEMA_V2)?;
        Ok(VecIndex {
            conn: Mutex::new(conn),
            model: model.to_string(),
        })
    }

    /// 관례 경로: `<root>/<graph-id>.vec.db`.
    pub fn for_graph(root: &Path, graph: vgraph_core::GraphId, model: &str) -> VecResult<Self> {
        Self::open(&root.join(format!("{graph}.vec.db")), model)
    }

    pub fn upsert_part(
        &self,
        node: Uuid,
        part: &str,
        node_hash: Hash256,
        text_h: Hash256,
        vector: &[f32],
    ) -> VecResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO embeddings(uuid, model, part, node_hash, text_hash, dim, vec)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(uuid, model, part)
             DO UPDATE SET node_hash=?4, text_hash=?5, dim=?6, vec=?7",
            params![
                node.to_string(),
                self.model,
                part,
                node_hash.to_string(),
                text_h.to_string(),
                vector.len() as i64,
                to_blob(vector)
            ],
        )?;
        Ok(())
    }

    pub fn remove(&self, node: Uuid) -> VecResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM embeddings WHERE uuid=?1 AND model=?2",
            params![node.to_string(), self.model],
        )?;
        Ok(())
    }

    /// 노드의 청크(part) 목록 — 다중 벡터 확인용.
    pub fn parts(&self, node: Uuid) -> VecResult<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT part FROM embeddings WHERE uuid=?1 AND model=?2 ORDER BY part")?;
        let rows = stmt.query_map(params![node.to_string(), self.model], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// uuid → (node_hash, part→text_hash)
    fn known(&self) -> VecResult<HashMap<Uuid, (Hash256, BTreeMap<String, Hash256>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT uuid, node_hash, part, text_hash FROM embeddings WHERE model=?1")?;
        let rows = stmt.query_map(params![self.model], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out: HashMap<Uuid, (Hash256, BTreeMap<String, Hash256>)> = HashMap::new();
        for row in rows {
            let (u, nh, part, th) = row?;
            let u: Uuid = u.parse().map_err(|e| VecError::Backend(format!("{e:?}")))?;
            let nh: Hash256 = nh.parse().map_err(|e| VecError::Backend(format!("{e}")))?;
            let th: Hash256 = th.parse().map_err(|e| VecError::Backend(format!("{e}")))?;
            out.entry(u)
                .or_insert((nh, BTreeMap::new()))
                .1
                .insert(part, th);
            out.get_mut(&u).unwrap().0 = nh;
        }
        Ok(out)
    }

    fn refresh_node_hash(&self, node: Uuid, h: Hash256) -> VecResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE embeddings SET node_hash=?3 WHERE uuid=?1 AND model=?2",
            params![node.to_string(), self.model, h.to_string()],
        )?;
        Ok(())
    }

    /// **staleness 큐** (2단계): payload hash 동일 → 스킵 (빠른 경로).
    /// payload 변경 시 텍스트 추출 → 텍스트 hash까지 동일하면 **재임베딩 없이 node_hash만
    /// 갱신** (임베딩 무관 prop 변경). 텍스트가 실제로 달라진 것만 Stale.
    pub fn pending(&self, view: &View, spec: &EmbedSpec) -> VecResult<Vec<PendingEmbed>> {
        let known = self.known()?;
        let mut out = Vec::new();
        for (u, h) in view.node_hashes() {
            if view
                .node_slug(u)
                .flatten()
                .is_some_and(|s| s.starts_with("__"))
            {
                continue; // 시스템 노드
            }
            let entry = known.get(&u);
            if let Some((nh, _)) = entry {
                if *nh == h {
                    continue; // 빠른 경로
                }
            }
            let chunks = extract_chunks(view, u, spec);
            if chunks.is_empty() {
                if entry.is_some() {
                    out.push(PendingEmbed {
                        node: u,
                        node_hash: h,
                        reason: PendingReason::Stale,
                    });
                }
                continue;
            }
            let cur: BTreeMap<String, Hash256> = chunks
                .iter()
                .map(|(p, t)| (p.clone(), text_hash(t)))
                .collect();
            match entry {
                None => out.push(PendingEmbed {
                    node: u,
                    node_hash: h,
                    reason: PendingReason::Missing,
                }),
                Some((_, parts)) if *parts == cur => {
                    self.refresh_node_hash(u, h)?; // 텍스트 불변 — 조용히 최신화
                }
                Some(_) => out.push(PendingEmbed {
                    node: u,
                    node_hash: h,
                    reason: PendingReason::Stale,
                }),
            }
        }
        Ok(out)
    }

    /// 브루트포스 코사인 + epoch 조인 + **노드 단위 max-pooling**.
    /// 결과 = score 내림차순, 동점 uuid 오름차순. part = 최고 점수 청크.
    pub fn search(&self, view: &View, query: &[f32], k: usize) -> VecResult<Vec<SimHit>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT uuid, node_hash, part, dim, vec FROM embeddings WHERE model=?1 ORDER BY uuid, part",
        )?;
        let rows = stmt.query_map(params![self.model], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Vec<u8>>(4)?,
            ))
        })?;
        let mut best: BTreeMap<Uuid, (f32, String)> = BTreeMap::new();
        for row in rows {
            let (u, nh, part, dim, blob) = row?;
            if dim as usize != query.len() {
                continue;
            }
            let node: Uuid = u.parse().map_err(|e| VecError::Backend(format!("{e:?}")))?;
            let node_hash: Hash256 = nh.parse().map_err(|e| VecError::Backend(format!("{e}")))?;
            // epoch 조인: 뷰 실재 + 최신 임베딩만
            if view.node_payload_hash(node) != Some(node_hash) {
                continue;
            }
            let score = cosine(query, &from_blob(&blob));
            match best.get(&node) {
                Some((s, _)) if *s >= score => {}
                _ => {
                    best.insert(node, (score, part));
                }
            }
        }
        let mut hits: Vec<SimHit> = best
            .into_iter()
            .map(|(node, (score, part))| SimHit { node, score, part })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.node.cmp(&b.node))
        });
        hits.truncate(k);
        Ok(hits)
    }

    /// keep 밖 uuid 임베딩 정리 (실운용 keep = 전 브랜치 head 합집합).
    pub fn prune(&self, keep: &std::collections::BTreeSet<Uuid>) -> VecResult<u64> {
        let all: Vec<Uuid> = self.known()?.into_keys().collect();
        let mut removed = 0;
        for u in all {
            if !keep.contains(&u) {
                self.remove(u)?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// pending 소화 — 전체 청크를 **한 번의 배치 호출**로 임베딩 (외부 API 효율).
/// 멱등: 재실행 시 0. 반환 = 재임베딩된 노드 수.
pub fn embed_pending(
    idx: &VecIndex,
    view: &View,
    embedder: &dyn Embedder,
    spec: &EmbedSpec,
) -> VecResult<usize> {
    let pending = idx.pending(view, spec)?;
    let mut plan: Vec<(Uuid, Hash256, Vec<(String, Hash256)>)> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for p in &pending {
        let chunks = extract_chunks(view, p.node, spec);
        plan.push((
            p.node,
            p.node_hash,
            chunks
                .iter()
                .map(|(part, t)| (part.clone(), text_hash(t)))
                .collect(),
        ));
        texts.extend(chunks.into_iter().map(|(_, t)| t));
    }
    let vectors = if texts.is_empty() {
        Vec::new()
    } else {
        embedder
            .embed_batch(&texts)
            .map_err(|e| VecError::Embed(e.to_string()))?
    };
    if vectors.len() != texts.len() {
        return Err(VecError::Embed(format!(
            "batch size mismatch: {} texts, {} vectors",
            texts.len(),
            vectors.len()
        )));
    }
    let mut i = 0;
    for (node, nh, parts) in plan {
        idx.remove(node)?; // 구 청크 제거 후 재구성
        for (part, th) in parts {
            idx.upsert_part(node, &part, nh, th, &vectors[i])?;
            i += 1;
        }
    }
    Ok(pending.len())
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}
