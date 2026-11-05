//! GraphState — 레퍼런스 모델 (계획 결정 D5).
//!
//! 정확성 오라클: BTreeMap 기반, 최적화 금지. P2 엔진 View는 이 모델과의
//! differential test로 검증된다. 순회는 인터페이스 §2.1 canonical 계약 그대로 구현.

use crate::canon::{edge_json, node_json, Hash256};
use crate::id::Uuid;
use crate::model::{Edge, Node};
use crate::value::{Props, Value};
use smol_str::SmolStr;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum StateError {
    #[error("unknown element: {0}")]
    UnknownElem(Uuid),
    #[error("duplicate uuid: {0}")]
    DupUuid(Uuid),
    #[error("slug taken: {0}")]
    SlugTaken(SmolStr),
    #[error("dangling edge: {0}")]
    DanglingEdge(Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
    Both,
}

/// 순회 hit — 인터페이스 §2.1: (depth, uuid) 정렬, 시작 노드 제외, via=최소 uuid 엣지.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub node: Uuid,
    pub via: Uuid,
    pub depth: u32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GraphState {
    nodes: BTreeMap<Uuid, Node>,
    edges: BTreeMap<Uuid, Edge>,
    slugs: BTreeMap<SmolStr, Uuid>,
}

impl GraphState {
    pub fn new() -> Self {
        Self::default()
    }

    // ---------- 조회 ----------

    pub fn node(&self, u: Uuid) -> Option<&Node> {
        self.nodes.get(&u)
    }
    pub fn edge(&self, u: Uuid) -> Option<&Edge> {
        self.edges.get(&u)
    }
    pub fn by_slug(&self, slug: &str) -> Option<Uuid> {
        self.slugs.get(slug).copied()
    }
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }
    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.values()
    }
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// 노드에 걸린 엣지 (uuid 순).
    pub fn edges_of(&self, u: Uuid) -> Vec<&Edge> {
        self.edges
            .values()
            .filter(|e| e.from == u || e.to == u)
            .collect()
    }

    // ---------- 변경 (WorkingCopy/DeltaBuilder가 사용) ----------

    pub fn insert_node(&mut self, n: Node) -> Result<(), StateError> {
        if self.nodes.contains_key(&n.uuid) {
            return Err(StateError::DupUuid(n.uuid));
        }
        if let Some(s) = &n.slug {
            if self.slugs.contains_key(s) {
                return Err(StateError::SlugTaken(s.clone()));
            }
            self.slugs.insert(s.clone(), n.uuid);
        }
        self.nodes.insert(n.uuid, n);
        Ok(())
    }

    /// cascade=true → 걸린 엣지 함께 제거·반환. false에서 엣지 존재 시 DanglingEdge.
    pub fn remove_node(&mut self, u: Uuid, cascade: bool) -> Result<(Node, Vec<Edge>), StateError> {
        if !self.nodes.contains_key(&u) {
            return Err(StateError::UnknownElem(u));
        }
        let attached: Vec<Uuid> = self.edges_of(u).iter().map(|e| e.uuid).collect();
        if !cascade {
            if let Some(first) = attached.first() {
                return Err(StateError::DanglingEdge(*first));
            }
        }
        let removed_edges: Vec<Edge> = attached
            .iter()
            .map(|eu| self.edges.remove(eu).expect("attached edge"))
            .collect();
        let node = self.nodes.remove(&u).expect("checked");
        if let Some(s) = &node.slug {
            self.slugs.remove(s);
        }
        Ok((node, removed_edges))
    }

    pub fn insert_edge(&mut self, e: Edge) -> Result<(), StateError> {
        if self.edges.contains_key(&e.uuid) {
            return Err(StateError::DupUuid(e.uuid));
        }
        if !self.nodes.contains_key(&e.from) {
            return Err(StateError::UnknownElem(e.from));
        }
        if !self.nodes.contains_key(&e.to) {
            return Err(StateError::UnknownElem(e.to));
        }
        self.edges.insert(e.uuid, e);
        Ok(())
    }

    pub fn remove_edge(&mut self, u: Uuid) -> Result<Edge, StateError> {
        self.edges.remove(&u).ok_or(StateError::UnknownElem(u))
    }

    fn props_mut(&mut self, elem: Uuid) -> Result<&mut Props, StateError> {
        if let Some(n) = self.nodes.get_mut(&elem) {
            return Ok(&mut n.props);
        }
        if let Some(e) = self.edges.get_mut(&elem) {
            return Ok(&mut e.props);
        }
        Err(StateError::UnknownElem(elem))
    }

    pub fn set_prop(
        &mut self,
        elem: Uuid,
        key: &str,
        v: Value,
    ) -> Result<Option<Value>, StateError> {
        Ok(self.props_mut(elem)?.insert(SmolStr::new(key), v))
    }

    pub fn unset_prop(&mut self, elem: Uuid, key: &str) -> Result<Option<Value>, StateError> {
        Ok(self.props_mut(elem)?.remove(key))
    }

    pub fn set_slug(&mut self, u: Uuid, slug: Option<&str>) -> Result<Option<SmolStr>, StateError> {
        if !self.nodes.contains_key(&u) {
            return Err(StateError::UnknownElem(u));
        }
        if let Some(s) = slug {
            if let Some(&owner) = self.slugs.get(s) {
                if owner != u {
                    return Err(StateError::SlugTaken(SmolStr::new(s)));
                }
            }
        }
        let node = self.nodes.get_mut(&u).expect("checked");
        let old = node.slug.take();
        if let Some(o) = &old {
            self.slugs.remove(o);
        }
        if let Some(s) = slug {
            node.slug = Some(SmolStr::new(s));
            self.slugs.insert(SmolStr::new(s), u);
        }
        Ok(old)
    }

    pub fn add_label(&mut self, u: Uuid, label: &str) -> Result<bool, StateError> {
        let n = self.nodes.get_mut(&u).ok_or(StateError::UnknownElem(u))?;
        let l = SmolStr::new(label);
        if n.labels.contains(&l) {
            return Ok(false);
        }
        n.labels.push(l);
        n.labels.sort();
        Ok(true)
    }

    pub fn remove_label(&mut self, u: Uuid, label: &str) -> Result<bool, StateError> {
        let n = self.nodes.get_mut(&u).ok_or(StateError::UnknownElem(u))?;
        let before = n.labels.len();
        n.labels.retain(|l| l != label);
        Ok(n.labels.len() != before)
    }

    // ---------- 순회/검색 (오라클 — §2.1 canonical 계약) ----------

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
        let mut frontier: Vec<Uuid> = vec![start];
        let mut hits: Vec<Hit> = Vec::new();

        for depth in 1..=max_depth {
            // 이번 레벨에서 새로 도달한 노드 → 최소 uuid via 엣지
            let mut reached: BTreeMap<Uuid, Uuid> = BTreeMap::new();
            for &u in &frontier {
                for e in self.edges.values() {
                    let matches_type = types.is_none_or(|ts| ts.iter().any(|t| e.etype == *t));
                    if !matches_type {
                        continue;
                    }
                    let other = match dir {
                        Dir::Out if e.from == u => e.to,
                        Dir::In if e.to == u => e.from,
                        Dir::Both if e.from == u => e.to,
                        Dir::Both if e.to == u => e.from,
                        _ => continue,
                    };
                    if visited.contains(&other) {
                        continue;
                    }
                    // via = 최소 uuid 엣지 (edges는 BTreeMap 순회라 첫 도달이 최소)
                    reached.entry(other).or_insert(e.uuid);
                }
            }
            if reached.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for (node, via) in reached {
                visited.insert(node);
                next.push(node);
                // 필터는 hit 방출에만 적용, 확장(BFS)은 막지 않음 (골든 G4)
                let pass = prop_filter.is_none_or(|(k, v)| {
                    self.nodes
                        .get(&node)
                        .is_some_and(|n| n.props.get(k) == Some(v))
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

    /// 라벨 전부 보유 + props 부분일치 노드 (uuid 오름차순).
    pub fn matches(&self, labels: &[&str], props_eq: &Props) -> Vec<Uuid> {
        self.nodes
            .values()
            .filter(|n| {
                labels.iter().all(|l| n.labels.iter().any(|nl| nl == l))
                    && props_eq.iter().all(|(k, v)| n.props.get(k) == Some(v))
            })
            .map(|n| n.uuid)
            .collect()
    }

    /// 노드 대상 속성 등가 검색 (uuid 오름차순).
    pub fn find_by_prop(&self, key: &str, v: &Value) -> Vec<Uuid> {
        self.nodes
            .values()
            .filter(|n| n.props.get(key) == Some(v))
            .map(|n| n.uuid)
            .collect()
    }

    // ---------- dump (골든 F0_DUMP 스펙) ----------

    /// canonical dump: {"epoch":…,"nodes":[uuid asc],"edges":[uuid asc]} 무공백 1행.
    pub fn dump(&self, epoch: Option<&Hash256>) -> String {
        let mut out = String::from("{\"epoch\":");
        match epoch {
            Some(h) => out.push_str(&crate::canon::esc(&h.to_string())),
            None => out.push_str("null"),
        }
        out.push_str(",\"nodes\":[");
        for (i, n) in self.nodes.values().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&node_json(n));
        }
        out.push_str("],\"edges\":[");
        for (i, e) in self.edges.values().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&edge_json(e));
        }
        out.push_str("]}");
        out
    }
}
