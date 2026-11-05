//! View — 불변 materialized 그래프 (캐시 계층, 설계 §5.6).
//!
//! - 위상(인접 리스트·라벨·slug) 상주, props는 lazy (payload hash → 소스 계층 폴트 + 캐시).
//! - 순회 canonical 계약(인터페이스 §2.1)을 GraphState 오라클과 동일 의미로 구현 —
//!   differential test가 이를 검증한다.

use smol_str::SmolStr;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::{Arc, OnceLock};
use vgraph_core::canon::payload_hash;
use vgraph_core::{Dir, GraphAddr, GraphId, GraphState, Hash256, Hit, Props, Uuid, Value};
use vgraph_store::{EdgeRow, NodeRow};

/// 소스 계층 접근 (payload 폴트 + 등가 검색). 엔진이 io 계측을 끼워 구현.
pub trait ViewSupport: Send + Sync {
    fn fetch_props(&self, h: &Hash256) -> Props;
    fn scan_index(&self, key: &str, value_json: &str) -> Vec<Hash256>;
}

struct VNode {
    labels: Vec<SmolStr>,
    slug: Option<SmolStr>,
    payload: Hash256,
    props: OnceLock<Props>,
}

struct VEdge {
    etype: SmolStr,
    from: Uuid,
    to: Uuid,
    payload: Hash256,
    props: OnceLock<Props>,
}

pub struct View {
    graph: GraphId,
    epoch: Hash256,
    nodes: BTreeMap<Uuid, VNode>,
    edges: BTreeMap<Uuid, VEdge>,
    /// (edge uuid, peer) — edge uuid 오름차순 정렬.
    out_adj: BTreeMap<Uuid, Vec<(Uuid, Uuid)>>,
    in_adj: BTreeMap<Uuid, Vec<(Uuid, Uuid)>>,
    slugs: BTreeMap<SmolStr, Uuid>,
    support: Arc<dyn ViewSupport>,
}

impl View {
    /// fold 경로: props 상주 (delta replay가 이미 props를 갖고 있으므로).
    pub fn from_state(
        graph: GraphId,
        epoch: Hash256,
        state: &GraphState,
        support: Arc<dyn ViewSupport>,
    ) -> View {
        let mut v = View::empty(graph, epoch, support);
        for n in state.nodes() {
            let vn = VNode {
                labels: n.labels.clone(),
                slug: n.slug.clone(),
                payload: payload_hash(&n.props),
                props: OnceLock::new(),
            };
            let _ = vn.props.set(n.props.clone());
            if let Some(s) = &n.slug {
                v.slugs.insert(s.clone(), n.uuid);
            }
            v.nodes.insert(n.uuid, vn);
        }
        for e in state.edges() {
            let ve = VEdge {
                etype: e.etype.clone(),
                from: e.from,
                to: e.to,
                payload: payload_hash(&e.props),
                props: OnceLock::new(),
            };
            let _ = ve.props.set(e.props.clone());
            v.edges.insert(e.uuid, ve);
        }
        v.build_adjacency();
        v
    }

    /// 연결 계층 rows 경로: props lazy — 최초 접근 시 D1 폴트 (골든 B7).
    pub fn from_rows(
        graph: GraphId,
        epoch: Hash256,
        nodes: Vec<NodeRow>,
        edges: Vec<EdgeRow>,
        support: Arc<dyn ViewSupport>,
    ) -> View {
        let mut v = View::empty(graph, epoch, support);
        for n in nodes {
            if let Some(s) = &n.slug {
                v.slugs.insert(s.clone(), n.uuid);
            }
            v.nodes.insert(
                n.uuid,
                VNode {
                    labels: n.labels,
                    slug: n.slug,
                    payload: n.payload,
                    props: OnceLock::new(),
                },
            );
        }
        for e in edges {
            v.edges.insert(
                e.uuid,
                VEdge {
                    etype: e.etype,
                    from: e.from,
                    to: e.to,
                    payload: e.payload,
                    props: OnceLock::new(),
                },
            );
        }
        v.build_adjacency();
        v
    }

    fn empty(graph: GraphId, epoch: Hash256, support: Arc<dyn ViewSupport>) -> View {
        View {
            graph,
            epoch,
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            out_adj: BTreeMap::new(),
            in_adj: BTreeMap::new(),
            slugs: BTreeMap::new(),
            support,
        }
    }

    fn build_adjacency(&mut self) {
        for (eu, e) in &self.edges {
            self.out_adj.entry(e.from).or_default().push((*eu, e.to));
            self.in_adj.entry(e.to).or_default().push((*eu, e.from));
        }
        for l in self.out_adj.values_mut().chain(self.in_adj.values_mut()) {
            l.sort(); // edge uuid 오름차순
        }
    }

    // ---------- GraphQuery 면 ----------

    pub fn graph(&self) -> GraphId {
        self.graph
    }
    pub fn epoch(&self) -> Hash256 {
        self.epoch
    }
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
    pub fn by_slug(&self, slug: &str) -> Option<Uuid> {
        self.slugs.get(slug).copied()
    }
    pub fn has_node(&self, u: Uuid) -> bool {
        self.nodes.contains_key(&u)
    }
    pub fn has_edge(&self, u: Uuid) -> bool {
        self.edges.contains_key(&u)
    }

    fn node_props(&self, u: Uuid) -> Option<&Props> {
        let n = self.nodes.get(&u)?;
        Some(n.props.get_or_init(|| self.support.fetch_props(&n.payload)))
    }
    fn edge_props(&self, u: Uuid) -> Option<&Props> {
        let e = self.edges.get(&u)?;
        Some(e.props.get_or_init(|| self.support.fetch_props(&e.payload)))
    }

    pub fn node_prop(&self, u: Uuid, key: &str) -> Option<Value> {
        self.node_props(u)?.get(key).cloned()
    }
    pub fn edge_prop(&self, u: Uuid, key: &str) -> Option<Value> {
        self.edge_props(u)?.get(key).cloned()
    }
    pub fn node_labels(&self, u: Uuid) -> Option<Vec<SmolStr>> {
        self.nodes.get(&u).map(|n| n.labels.clone())
    }
    pub fn node_slug(&self, u: Uuid) -> Option<Option<SmolStr>> {
        self.nodes.get(&u).map(|n| n.slug.clone())
    }
    /// 소스 계층 payload hash — 사이드카(벡터 색인 등)의 staleness 키 (lmd embed.hash 패턴).
    pub fn node_payload_hash(&self, u: Uuid) -> Option<Hash256> {
        self.nodes.get(&u).map(|n| n.payload)
    }
    /// 뷰의 (uuid, payload_hash) 전체 — 사이드카 staleness 큐 계산 재료.
    pub fn node_hashes(&self) -> Vec<(Uuid, Hash256)> {
        self.nodes.iter().map(|(u, n)| (*u, n.payload)).collect()
    }
    /// props 전체 사본 (lazy 로드 유발 가능 — D1).
    pub fn props_of(&self, u: Uuid) -> Option<Props> {
        self.node_props(u).cloned()
    }
    /// 엣지 props 전체 사본 (`props_of`의 엣지판, lazy 로드 유발 가능).
    pub fn edge_props_of(&self, u: Uuid) -> Option<Props> {
        self.edge_props(u).cloned()
    }

    /// 순회 — 오라클(GraphState::neighbors)과 동일 canonical 의미론.
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
        let mut visited: BTreeSet<Uuid> = BTreeSet::new();
        visited.insert(start);
        let mut frontier = vec![start];
        let mut hits = Vec::new();

        for depth in 1..=max_depth {
            // peer → 최소 edge uuid
            let mut reached: BTreeMap<Uuid, Uuid> = BTreeMap::new();
            for &u in &frontier {
                let mut consider = |list: Option<&Vec<(Uuid, Uuid)>>| {
                    if let Some(list) = list {
                        for (eu, peer) in list {
                            let e = &self.edges[eu];
                            if let Some(ts) = types {
                                if !ts.iter().any(|t| e.etype == *t) {
                                    continue;
                                }
                            }
                            if visited.contains(peer) {
                                continue;
                            }
                            reached
                                .entry(*peer)
                                .and_modify(|cur| {
                                    if eu < cur {
                                        *cur = *eu;
                                    }
                                })
                                .or_insert(*eu);
                        }
                    }
                };
                match dir {
                    Dir::Out => consider(self.out_adj.get(&u)),
                    Dir::In => consider(self.in_adj.get(&u)),
                    Dir::Both => {
                        consider(self.out_adj.get(&u));
                        consider(self.in_adj.get(&u));
                    }
                }
            }
            if reached.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for (node, via) in reached {
                visited.insert(node);
                next.push(node);
                // 필터는 hit 방출에만 적용 (골든 G4) — 오라클과 동일 의미론
                let pass = prop_filter.is_none_or(|(k, v)| {
                    self.node_props(node).is_some_and(|p| p.get(k) == Some(v))
                });
                if depth >= min_depth && pass {
                    hits.push(Hit { node, via, depth });
                    if let Some(l) = limit {
                        if hits.len() >= l {
                            return hits;
                        }
                    }
                }
            }
            frontier = next;
        }
        hits
    }

    pub fn matches(&self, labels: &[&str], props_eq: &Props) -> Vec<Uuid> {
        self.nodes
            .iter()
            .filter(|(u, n)| {
                labels.iter().all(|l| n.labels.iter().any(|nl| nl == l))
                    && props_eq
                        .iter()
                        .all(|(k, v)| self.node_props(**u).and_then(|p| p.get(k)) == Some(v))
            })
            .map(|(u, _)| *u)
            .collect()
    }

    /// 소스 계층 색인 + epoch 조인 (인터페이스 §2.1): scan_index의 버전 무관 hash 목록을
    /// 이 뷰에 실재하는 uuid로 좁힌다.
    pub fn find_by_prop(&self, key: &str, v: &Value) -> Vec<Uuid> {
        let mut vjson = String::new();
        vgraph_core::canon::write_value(v, &mut vjson);
        let hashes: BTreeSet<Hash256> = self.support.scan_index(key, &vjson).into_iter().collect();
        self.nodes
            .iter()
            .filter(|(_, n)| hashes.contains(&n.payload))
            .map(|(u, _)| *u)
            .collect()
    }

    /// GraphState 재구성 (working base·differential test용 — props 전체 로드).
    pub fn to_state(&self) -> GraphState {
        let mut s = GraphState::new();
        for (u, n) in &self.nodes {
            s.insert_node(vgraph_core::Node {
                uuid: *u,
                labels: n.labels.clone(),
                slug: n.slug.clone(),
                props: self.node_props(*u).cloned().unwrap_or_default(),
            })
            .expect("view→state node");
        }
        for (u, e) in &self.edges {
            s.insert_edge(vgraph_core::Edge {
                uuid: *u,
                etype: e.etype.clone(),
                from: e.from,
                to: e.to,
                props: self.edge_props(*u).cloned().unwrap_or_default(),
            })
            .expect("view→state edge");
        }
        s
    }

    /// canonical dump — GraphState::dump과 동일 포맷 (골든 F0_DUMP).
    pub fn dump(&self) -> String {
        self.to_state().dump(Some(&self.epoch))
    }
}

// ---------- 조회 면: 찾기 · 필터 순회 · 커서 나열 ----------

/// 발췌에서 맞은 부분 앞뒤로 싣는 문자 수 (바이트 아님 — UTF-8 경계 보존).
pub const EXCERPT_CONTEXT_CHARS: usize = 20;

/// 잘린 발췌 끝에 붙는 표시.
const ELLIPSIS: char = '…';

/// 찾기 입력. `labels`는 모두 가진 노드, `props_eq`는 완전 일치(모두), `text`는 대소문자 무시 부분 문자열.
/// `text_keys`가 None이면 모든 문자열 속성(리스트 안 문자열 포함)을 본다.
/// 빈 `text`는 "텍스트 조건 없음"과 같다. `after`는 이 uuid **초과**부터 이어 받는 커서.
#[derive(Debug, Clone, Copy)]
pub struct FindQuery<'a> {
    pub labels: &'a [&'a str],
    pub props_eq: &'a Props,
    pub text: Option<&'a str>,
    pub text_keys: Option<&'a [&'a str]>,
    pub limit: usize,
    pub after: Option<Uuid>,
}

/// 찾기 결과 한 건. 텍스트 조건이 없으면 `matched_key`·`excerpt`는 None.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindMatch {
    pub node: Uuid,
    pub matched_key: Option<SmolStr>,
    pub excerpt: Option<String>,
}

/// 찾기 한 쪽. `done`이 false면 `next_cursor`(마지막 uuid)로 이어 받는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindPage {
    pub items: Vec<FindMatch>,
    pub next_cursor: Option<Uuid>,
    pub done: bool,
}

/// `find_with`의 선택 옵션. 기본값(모두 꺼짐)이면 `find`와 같은 항목·순서·커서를 낸다.
///
/// - `relevance`: 일치 강도 순 정렬(아래 점수 규칙). 커서 없이 상위 `limit`개만 낸다.
/// - `normalize`: 바늘·값 모두에서 공백·밑줄·붙임표(`-` U+2010~2015 U+2212 U+FE63 U+FF0D)를
///   지우고 비교한다. 대소문자 무시는 기존과 같다. 발췌는 원문 기준이다.
/// - `any`: 여러 표현 중 하나라도 맞으면 결과. `FindQuery::text`가 있으면 그것이 첫 표현이고
///   `any`가 뒤따른다. 빈 표현(normalize 에서는 구분 문자뿐인 표현 포함)은 버리고,
///   남은 표현이 없으면 텍스트 조건이 없는 것과 같다.
///
/// 점수 규칙(relevance): 노드는 속성에서 맞아야 후보다(후보 집합은 정렬 여부와 무관).
/// 한 표현의 점수 = 필드 일치 강도 × 10 + 출처 가산. 강도는 값 전체 일치 3 · 앞부분 일치 2 ·
/// 부분 일치 1, 출처 가산은 slug 2 · 라벨 1 · 속성 0. 노드의 점수는 그 표현의 속성·slug·라벨
/// 가운데 가장 높은 값이고, 여러 표현 중에는 가장 높은 점수의 표현을 쓴다.
/// 점수 내림차순, 동점은 uuid 오름차순이라 결과가 결정적이다.
#[derive(Debug, Clone, Copy, Default)]
pub struct FindOptions<'a> {
    pub relevance: bool,
    pub normalize: bool,
    pub any: &'a [&'a str],
}

/// `find_filtered`의 텍스트 대상 필터. 기본값(꺼짐)이면 `find_with`와 같다.
/// (`FindOptions`에 필드를 더하면 그 구조체를 리터럴로 쓰는 기존 코드가 깨져 따로 둔다.)
///
/// - `skip_ref_like`: 값 전체가 그래프 주소(`graph://ns/slug[@branch][.commit][#node]`, 앞뒤 공백 허용,
///   안에 공백 없음, `GraphAddr` 문법에 맞음)뿐인 문자열은 텍스트 검색 대상에서 뺀다. 문자열 리스트는
///   원소마다 따로 판정한다. 주소가 문장 일부로 섞인 값, `graph://`가 없는 축약형(`ns/slug#node`)은
///   일반 텍스트라 그대로 검색한다. 텍스트 조건이 없으면 효과가 없다.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextFilter {
    pub skip_ref_like: bool,
}

/// `find_with` 결과 한 건. `matched_expr`는 맞은 표현 원문(텍스트 조건이 있을 때),
/// `score`는 relevance 일 때만 있다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindHit {
    pub found: FindMatch,
    pub matched_expr: Option<String>,
    pub score: Option<u32>,
}

/// `find_with` 결과 한 쪽. relevance 에서는 `next_cursor`가 늘 None 이고, `total`에 후보 전체 수가
/// 들어가며 `done`은 후보가 모두 `items`에 담겼는지(`total ≤ limit`)다. 그 밖에는 `total`이 None.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindOutcome {
    pub items: Vec<FindHit>,
    pub next_cursor: Option<Uuid>,
    pub done: bool,
    pub total: Option<usize>,
}

/// `find_with` 옵션 조합 오류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindOptionsError {
    /// relevance 정렬은 커서를 쓰지 않는다.
    RelevanceWithCursor,
}

impl std::fmt::Display for FindOptionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RelevanceWithCursor => f.write_str(
                "order=relevance는 cursor와 함께 쓸 수 없습니다 (커서 없이 상위 limit개만 줍니다)",
            ),
        }
    }
}

impl std::error::Error for FindOptionsError {}

/// 필터 순회 입력. 빈 슬라이스는 None(필터 없음)과 같다.
/// 시작 노드는 늘 뻗고, depth ≥ 1 노드는 `expand_labels` 중 하나라도 가진 때만 뻗는다.
/// `emit_labels`는 결과에 낼지만 정한다.
#[derive(Debug, Clone, Copy)]
pub struct WalkQuery<'a> {
    pub start: Uuid,
    pub dir: Dir,
    pub etypes: Option<&'a [&'a str]>,
    pub max_depth: u32,
    pub emit_labels: Option<&'a [&'a str]>,
    pub expand_labels: Option<&'a [&'a str]>,
    pub limit: Option<usize>,
}

/// 필터 순회 결과 한 건 — `Hit` + 거쳐 온 엣지의 타입.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkHit {
    pub node: Uuid,
    pub via: Uuid,
    pub via_type: SmolStr,
    pub depth: u32,
}

/// 커서 나열 한 쪽. `done`이 false면 `next_cursor`(마지막 uuid)로 이어 받는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanPage<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<Uuid>,
    pub done: bool,
}

/// 엣지 위상 정보 (props 로드 없음).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeInfo {
    pub etype: SmolStr,
    pub from: Uuid,
    pub to: Uuid,
}

/// 대소문자 접기 — 문자 하나를 문자 하나로 (인덱스가 원문과 1:1 대응하도록).
fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// `hay` 안에서 `needle`(접힌 문자열)이 처음 나오는 문자 위치.
fn find_folded(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len())
        .position(|w| w.iter().zip(needle).all(|(a, b)| fold_char(*a) == *b))
}

/// 문자열에서 needle을 찾아 앞뒤 `EXCERPT_CONTEXT_CHARS`자 발췌를 만든다.
fn excerpt_of(s: &str, needle: &[char]) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let at = find_folded(&chars, needle)?;
    let start = at.saturating_sub(EXCERPT_CONTEXT_CHARS);
    let end = (at + needle.len() + EXCERPT_CONTEXT_CHARS).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push(ELLIPSIS);
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push(ELLIPSIS);
    }
    Some(out)
}

/// 값 안의 문자열(리스트는 재귀)에서 첫 발췌.
fn value_excerpt(v: &Value, needle: &[char]) -> Option<String> {
    match v {
        Value::Str(s) => excerpt_of(s, needle),
        Value::List(items) => items.iter().find_map(|i| value_excerpt(i, needle)),
        _ => None,
    }
}

fn has_all_labels(labels: &[SmolStr], want: &[&str]) -> bool {
    want.iter().all(|l| labels.iter().any(|nl| nl == l))
}

fn has_any_label(labels: &[SmolStr], want: &[&str]) -> bool {
    want.iter().any(|l| labels.iter().any(|nl| nl == l))
}

/// 빈 슬라이스는 "필터 없음".
fn non_empty<'a>(f: Option<&'a [&'a str]>) -> Option<&'a [&'a str]> {
    f.filter(|l| !l.is_empty())
}

/// uuid 오름차순으로 `after` 초과 항목을 `limit`개까지 모아 쪽을 만든다.
fn page_of<T>(
    iter: impl Iterator<Item = (Uuid, T)>,
    after: Option<Uuid>,
    limit: usize,
) -> ScanPage<T> {
    let mut items: Vec<(Uuid, T)> = iter.take(limit.saturating_add(1)).collect();
    let done = items.len() <= limit;
    items.truncate(limit);
    let next_cursor = if done {
        None
    } else {
        items.last().map(|(u, _)| *u).or(after)
    };
    ScanPage {
        items: items.into_iter().map(|(_, t)| t).collect(),
        next_cursor,
        done,
    }
}

impl View {
    fn after_range(after: Option<Uuid>) -> (Bound<Uuid>, Bound<Uuid>) {
        (
            after.map_or(Bound::Unbounded, Bound::Excluded),
            Bound::Unbounded,
        )
    }

    /// 엣지 위상 (타입·양끝). 없으면 None.
    pub fn edge_info(&self, u: Uuid) -> Option<EdgeInfo> {
        self.edges.get(&u).map(|e| EdgeInfo {
            etype: e.etype.clone(),
            from: e.from,
            to: e.to,
        })
    }

    /// 라벨·속성·부분 문자열로 노드를 찾는다 (uuid 오름차순, 커서 이어받기).
    ///
    /// 부분 문자열은 문자 단위로 소문자 접기(1문자→1문자)한 뒤 비교한다 — 한글은 접기에 영향이 없다.
    /// 키 순서: `text_keys`를 주면 그 순서, 아니면 속성 키 오름차순. 노드마다 첫 번째로 맞은 키·발췌를 준다.
    pub fn find(&self, q: &FindQuery<'_>) -> FindPage {
        let needle: Vec<char> = q
            .text
            .map(|t| t.chars().map(fold_char).collect())
            .unwrap_or_default();
        let candidates = self
            .nodes
            .range(Self::after_range(q.after))
            .filter(|(_, n)| has_all_labels(&n.labels, q.labels))
            .filter(|(u, _)| {
                q.props_eq.is_empty()
                    || q.props_eq
                        .iter()
                        .all(|(k, v)| self.node_props(**u).and_then(|p| p.get(k)) == Some(v))
            })
            .filter_map(|(u, _)| {
                if needle.is_empty() {
                    return Some((
                        *u,
                        FindMatch {
                            node: *u,
                            matched_key: None,
                            excerpt: None,
                        },
                    ));
                }
                let props = self.node_props(*u)?;
                let hit = match q.text_keys {
                    Some(keys) => keys.iter().find_map(|k| {
                        let ex = value_excerpt(props.get(*k)?, &needle)?;
                        Some((SmolStr::new(k), ex))
                    }),
                    None => props
                        .iter()
                        .find_map(|(k, v)| Some((k.clone(), value_excerpt(v, &needle)?))),
                }?;
                let found = FindMatch {
                    node: *u,
                    matched_key: Some(hit.0),
                    excerpt: Some(hit.1),
                };
                Some((*u, found))
            });
        let page = page_of(candidates, q.after, q.limit);
        FindPage {
            items: page.items,
            next_cursor: page.next_cursor,
            done: page.done,
        }
    }
}

// ---------- find_with 보조: 정규화 · 일치 강도 · 점수 ----------

/// 강도 단계의 점수 폭(출처 가산 0~2보다 커야 단계가 뒤집히지 않는다).
const STRENGTH_STEP: u32 = 10;
/// 일치 강도: 값 전체 일치 · 앞부분 일치 · 부분 일치.
const STRENGTH_EXACT: u32 = 3;
const STRENGTH_PREFIX: u32 = 2;
const STRENGTH_CONTAINS: u32 = 1;
/// 출처 가산: slug 일치가 라벨 일치보다, 라벨 일치가 속성 일치보다 위.
const SLUG_BONUS: u32 = 2;
const LABEL_BONUS: u32 = 1;

/// normalize 가 지우는 문자: 공백류 · 밑줄 · 붙임표류(`-` U+2010~2015 U+2212 U+FE63 U+FF0D).
fn is_ignorable(c: char) -> bool {
    c.is_whitespace()
        || c == '_'
        || matches!(
            c,
            '-' | '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{FE63}' | '\u{FF0D}'
        )
}

/// 접힌 문자열과, 각 문자가 원문에서 몇 번째 문자였는지(normalize 로 지운 자리를 건너뜀).
fn prep(s: &str, normalize: bool) -> (Vec<char>, Vec<usize>) {
    let mut chars = Vec::new();
    let mut origin = Vec::new();
    for (i, c) in s.chars().enumerate() {
        if normalize && is_ignorable(c) {
            continue;
        }
        chars.push(fold_char(c));
        origin.push(i);
    }
    (chars, origin)
}

/// 찾을 표현 하나(접힌 바늘 + 원문).
struct Expr<'a> {
    raw: &'a str,
    needle: Vec<char>,
}

/// text → any 순으로 표현을 모은다. 접은 뒤 비는 표현은 버린다.
fn expressions<'a>(text: Option<&'a str>, any: &'a [&'a str], normalize: bool) -> Vec<Expr<'a>> {
    text.into_iter()
        .chain(any.iter().copied())
        .filter_map(|raw| {
            let needle = prep(raw, normalize).0;
            (!needle.is_empty()).then_some(Expr { raw, needle })
        })
        .collect()
}

/// 한 문자열 안의 일치: 강도와 원문 기준 발췌.
struct StrHit {
    strength: u32,
    excerpt: String,
}

fn match_in(s: &str, needle: &[char], normalize: bool) -> Option<StrHit> {
    let (hay, origin) = prep(s, normalize);
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let at = hay.windows(needle.len()).position(|w| w == needle)?;
    let strength = if at == 0 && needle.len() == hay.len() {
        STRENGTH_EXACT
    } else if at == 0 {
        STRENGTH_PREFIX
    } else {
        STRENGTH_CONTAINS
    };
    let start_orig = *origin.get(at)?;
    let end_orig = origin.get(at + needle.len() - 1)? + 1;
    let chars: Vec<char> = s.chars().collect();
    let start = start_orig.saturating_sub(EXCERPT_CONTEXT_CHARS);
    let end = (end_orig + EXCERPT_CONTEXT_CHARS).min(chars.len());
    let mut excerpt = String::new();
    if start > 0 {
        excerpt.push(ELLIPSIS);
    }
    excerpt.extend(&chars[start..end]);
    if end < chars.len() {
        excerpt.push(ELLIPSIS);
    }
    Some(StrHit { strength, excerpt })
}

/// 값 전체가 그래프 주소뿐인 문자열인가 (`skip_ref_like` 판정).
/// 앞뒤 공백을 뗀 뒤 비어 있지 않고, 안에 공백이 없고, `graph://` 주소 문법에 맞아야 한다.
/// 축약형(`ns/slug#node`)은 경로·앵커 표기와 구별되지 않아 주소로 보지 않는다.
fn is_ref_like(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && !t.chars().any(char::is_whitespace) && t.parse::<GraphAddr>().is_ok()
}

/// 값(리스트는 재귀)의 일치. `best`면 가장 강한 것(동률은 앞), 아니면 첫 일치.
/// `skip_ref`면 주소뿐인 문자열은 일치 대상에서 뺀다.
fn value_hit(
    v: &Value,
    needle: &[char],
    normalize: bool,
    best: bool,
    skip_ref: bool,
) -> Option<StrHit> {
    match v {
        Value::Str(s) if skip_ref && is_ref_like(s) => None,
        Value::Str(s) => match_in(s, needle, normalize),
        Value::List(items) => {
            let mut hits = items
                .iter()
                .filter_map(|i| value_hit(i, needle, normalize, best, skip_ref));
            if best {
                hits.fold(None::<StrHit>, |acc, h| match acc {
                    Some(a) if a.strength >= h.strength => Some(a),
                    _ => Some(h),
                })
            } else {
                hits.next()
            }
        }
        _ => None,
    }
}

/// 노드 하나의 텍스트 평가 결과.
struct TextEval {
    key: SmolStr,
    excerpt: String,
    expr: usize,
    score: u32,
}

impl View {
    /// 이 노드의 `exprs` 평가. 맞는 표현이 없으면 None.
    /// relevance 가 아니면 text → any 순으로 첫 번째로 맞은 표현의 첫 키(기존 `find` 규칙)다.
    fn eval_text(
        &self,
        u: Uuid,
        keys: Option<&[&str]>,
        exprs: &[Expr<'_>],
        o: &FindOptions<'_>,
        skip_ref: bool,
    ) -> Option<TextEval> {
        let props = self.node_props(u)?;
        let entries: Vec<(SmolStr, &Value)> = match keys {
            Some(ks) => ks
                .iter()
                .filter_map(|k| Some((SmolStr::new(k), props.get(*k)?)))
                .collect(),
            None => props.iter().map(|(k, v)| (k.clone(), v)).collect(),
        };
        let mut best: Option<TextEval> = None;
        for (idx, e) in exprs.iter().enumerate() {
            let mut prop_best: Option<(SmolStr, StrHit)> = None;
            for (k, v) in &entries {
                let Some(h) = value_hit(v, &e.needle, o.normalize, o.relevance, skip_ref) else {
                    continue;
                };
                if prop_best
                    .as_ref()
                    .is_none_or(|(_, cur)| h.strength > cur.strength)
                {
                    prop_best = Some((k.clone(), h));
                }
                if !o.relevance {
                    break;
                }
            }
            let Some((key, hit)) = prop_best else {
                continue;
            };
            if !o.relevance {
                return Some(TextEval {
                    key,
                    excerpt: hit.excerpt,
                    expr: idx,
                    score: 0,
                });
            }
            let score = self.relevance_score(u, &e.needle, o.normalize, hit.strength);
            if best.as_ref().is_none_or(|b| score > b.score) {
                best = Some(TextEval {
                    key,
                    excerpt: hit.excerpt,
                    expr: idx,
                    score,
                });
            }
        }
        best
    }

    /// 속성 일치 강도에 slug·라벨 일치를 견줘 가장 높은 점수를 낸다.
    fn relevance_score(
        &self,
        u: Uuid,
        needle: &[char],
        normalize: bool,
        prop_strength: u32,
    ) -> u32 {
        let mut score = prop_strength * STRENGTH_STEP;
        let Some(n) = self.nodes.get(&u) else {
            return score;
        };
        if let Some(h) = n
            .slug
            .as_deref()
            .and_then(|s| match_in(s, needle, normalize))
        {
            score = score.max(h.strength * STRENGTH_STEP + SLUG_BONUS);
        }
        for l in &n.labels {
            if let Some(h) = match_in(l, needle, normalize) {
                score = score.max(h.strength * STRENGTH_STEP + LABEL_BONUS);
            }
        }
        score
    }

    /// `find`에 선택 옵션(관련도 정렬·표기 정규화·여러 표현)을 얹은 찾기. 옵션을 모두 끄면
    /// `find`와 같은 항목·커서·done 이다. relevance 는 커서(`after`)와 함께 쓸 수 없다.
    pub fn find_with(
        &self,
        q: &FindQuery<'_>,
        o: &FindOptions<'_>,
    ) -> Result<FindOutcome, FindOptionsError> {
        self.find_filtered(q, o, &TextFilter::default())
    }

    /// `find_with`에 텍스트 대상 필터(`TextFilter`)를 더한 찾기. 필터를 끄면 `find_with`와 같다.
    pub fn find_filtered(
        &self,
        q: &FindQuery<'_>,
        o: &FindOptions<'_>,
        f: &TextFilter,
    ) -> Result<FindOutcome, FindOptionsError> {
        if o.relevance && q.after.is_some() {
            return Err(FindOptionsError::RelevanceWithCursor);
        }
        let exprs = expressions(q.text, o.any, o.normalize);
        let candidates = self
            .nodes
            .range(Self::after_range(q.after))
            .filter(|(_, n)| has_all_labels(&n.labels, q.labels))
            .filter(|(u, _)| {
                q.props_eq.is_empty()
                    || q.props_eq
                        .iter()
                        .all(|(k, v)| self.node_props(**u).and_then(|p| p.get(k)) == Some(v))
            })
            .filter_map(|(u, _)| {
                let hit = if exprs.is_empty() {
                    FindHit {
                        found: FindMatch {
                            node: *u,
                            matched_key: None,
                            excerpt: None,
                        },
                        matched_expr: None,
                        score: o.relevance.then_some(0),
                    }
                } else {
                    let ev = self.eval_text(*u, q.text_keys, &exprs, o, f.skip_ref_like)?;
                    FindHit {
                        found: FindMatch {
                            node: *u,
                            matched_key: Some(ev.key),
                            excerpt: Some(ev.excerpt),
                        },
                        matched_expr: exprs.get(ev.expr).map(|e| e.raw.to_string()),
                        score: o.relevance.then_some(ev.score),
                    }
                };
                Some((*u, hit))
            });
        if !o.relevance {
            let page = page_of(candidates, q.after, q.limit);
            return Ok(FindOutcome {
                items: page.items,
                next_cursor: page.next_cursor,
                done: page.done,
                total: None,
            });
        }
        let mut all: Vec<FindHit> = candidates.map(|(_, h)| h).collect();
        let total = all.len();
        // 후보는 uuid 오름차순으로 모였고 정렬은 안정적이라, 동점은 uuid 순으로 남는다.
        all.sort_by_key(|h| std::cmp::Reverse(h.score));
        all.truncate(q.limit);
        Ok(FindOutcome {
            items: all,
            next_cursor: None,
            done: total <= q.limit,
            total: Some(total),
        })
    }
}

impl View {
    /// 필터 순회 — `neighbors`와 같은 canonical 의미론(visited·최소 edge uuid 선택·(depth, uuid) 순서)
    /// 위에 엣지 타입 여러 개·emit/expand 라벨 필터를 얹는다. 필터를 하나도 안 주면
    /// `neighbors(start, dir, None, None, 1, max_depth, limit)`와 같은 결과다.
    pub fn walk(&self, q: &WalkQuery<'_>) -> Vec<WalkHit> {
        let etypes = non_empty(q.etypes);
        let emit = non_empty(q.emit_labels);
        let expand = non_empty(q.expand_labels);
        let mut visited: BTreeSet<Uuid> = BTreeSet::new();
        visited.insert(q.start);
        let mut frontier = vec![q.start];
        let mut hits = Vec::new();

        for depth in 1..=q.max_depth {
            // peer → 최소 edge uuid
            let mut reached: BTreeMap<Uuid, Uuid> = BTreeMap::new();
            for &u in &frontier {
                let mut consider = |list: Option<&Vec<(Uuid, Uuid)>>| {
                    for (eu, peer) in list.into_iter().flatten() {
                        if let Some(ts) = etypes {
                            if !ts.iter().any(|t| self.edges[eu].etype == *t) {
                                continue;
                            }
                        }
                        if visited.contains(peer) {
                            continue;
                        }
                        reached
                            .entry(*peer)
                            .and_modify(|cur| {
                                if eu < cur {
                                    *cur = *eu;
                                }
                            })
                            .or_insert(*eu);
                    }
                };
                match q.dir {
                    Dir::Out => consider(self.out_adj.get(&u)),
                    Dir::In => consider(self.in_adj.get(&u)),
                    Dir::Both => {
                        consider(self.out_adj.get(&u));
                        consider(self.in_adj.get(&u));
                    }
                }
            }
            if reached.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for (node, via) in reached {
                visited.insert(node);
                let labels: &[SmolStr] = self.nodes.get(&node).map_or(&[], |n| &n.labels);
                // expand 필터는 다음 단계로 뻗을지만, emit 필터는 결과에 낼지만 정한다.
                if expand.is_none_or(|l| has_any_label(labels, l)) {
                    next.push(node);
                }
                if emit.is_none_or(|l| has_any_label(labels, l)) {
                    hits.push(WalkHit {
                        node,
                        via,
                        via_type: self.edges[&via].etype.clone(),
                        depth,
                    });
                    if q.limit.is_some_and(|l| hits.len() >= l) {
                        return hits;
                    }
                }
            }
            frontier = next;
        }
        hits
    }

    /// 노드를 uuid 오름차순으로 `limit`개 나열한다 (`after` 초과부터). 라벨 필터는 "모두 가진".
    pub fn scan_nodes(&self, labels: &[&str], after: Option<Uuid>, limit: usize) -> ScanPage<Uuid> {
        let it = self
            .nodes
            .range(Self::after_range(after))
            .filter(|(_, n)| has_all_labels(&n.labels, labels))
            .map(|(u, _)| (*u, *u));
        page_of(it, after, limit)
    }

    /// 엣지를 uuid 오름차순으로 `limit`개 나열한다 (`after` 초과부터).
    pub fn scan_edges(&self, after: Option<Uuid>, limit: usize) -> ScanPage<Uuid> {
        let it = self
            .edges
            .range(Self::after_range(after))
            .map(|(u, _)| (*u, *u));
        page_of(it, after, limit)
    }
}
