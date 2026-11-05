//! KG 오버레이 (설계 §2.2, P5) — 스키마도 그래프 데이터다.
//!
//! 저장 = `__schema` 시스템 노드 (lockfile과 동일 패턴): 스키마 변경이 자연스럽게
//! commit이 되고, 브랜치·버전을 그래프와 함께 탄다 (골든 H4).
//! 검증 시점 = commit 게이트 (편집 중 자유, seal 시 일괄 — strict/warn/off).

use smol_str::SmolStr;
use std::collections::{BTreeMap, BTreeSet};
use vgraph_core::{GraphState, Uuid, Value};

pub const SCHEMA_SLUG: &str = "__schema";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaMode {
    Strict,
    Warn,
    Off,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodeClassDef {
    /// 필수 prop 키.
    pub required: Vec<SmolStr>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EdgeClassDef {
    /// 허용 from-라벨 (빈 벡터 = 제약 없음).
    pub domain: Vec<SmolStr>,
    /// 허용 to-라벨.
    pub range: Vec<SmolStr>,
    /// 같은 from 노드에서 이 타입 엣지의 최대 개수 (cardinality 0..N).
    pub max_out: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    pub mode: SchemaMode,
    pub node_classes: BTreeMap<SmolStr, NodeClassDef>,
    pub edge_classes: BTreeMap<SmolStr, EdgeClassDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SchemaIssue {
    RequiredMissing {
        class: SmolStr,
        elem: Uuid,
        key: SmolStr,
    },
    DomainViolation {
        edge_class: SmolStr,
        edge: Uuid,
        end: Uuid,
        expected: Vec<SmolStr>,
    },
    RangeViolation {
        edge_class: SmolStr,
        edge: Uuid,
        end: Uuid,
        expected: Vec<SmolStr>,
    },
    Cardinality {
        edge_class: SmolStr,
        elem: Uuid,
        max: u32,
        found: u32,
    },
}

impl Schema {
    // ---------- Value 직렬화 (__schema 노드 props) ----------

    pub fn to_props(&self) -> vgraph_core::Props {
        let mut p = vgraph_core::Props::new();
        p.insert(
            SmolStr::new("mode"),
            Value::Str(
                match self.mode {
                    SchemaMode::Strict => "strict",
                    SchemaMode::Warn => "warn",
                    SchemaMode::Off => "off",
                }
                .into(),
            ),
        );
        let mut ncs = BTreeMap::new();
        for (name, nc) in &self.node_classes {
            let mut m = BTreeMap::new();
            m.insert(
                SmolStr::new("required"),
                Value::List(
                    nc.required
                        .iter()
                        .map(|k| Value::Str(k.to_string()))
                        .collect(),
                ),
            );
            ncs.insert(name.clone(), Value::Map(m));
        }
        p.insert(SmolStr::new("node_classes"), Value::Map(ncs));
        let mut ecs = BTreeMap::new();
        for (name, ec) in &self.edge_classes {
            let mut m = BTreeMap::new();
            m.insert(
                SmolStr::new("domain"),
                Value::List(
                    ec.domain
                        .iter()
                        .map(|k| Value::Str(k.to_string()))
                        .collect(),
                ),
            );
            m.insert(
                SmolStr::new("range"),
                Value::List(ec.range.iter().map(|k| Value::Str(k.to_string())).collect()),
            );
            if let Some(mx) = ec.max_out {
                m.insert(SmolStr::new("max_out"), Value::Int(mx as i64));
            }
            ecs.insert(name.clone(), Value::Map(m));
        }
        p.insert(SmolStr::new("edge_classes"), Value::Map(ecs));
        p
    }

    fn str_list(v: Option<&Value>) -> Vec<SmolStr> {
        match v {
            Some(Value::List(xs)) => xs
                .iter()
                .filter_map(|x| match x {
                    Value::Str(s) => Some(SmolStr::new(s)),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn from_props(p: &vgraph_core::Props) -> Option<Schema> {
        let mode = match p.get("mode") {
            Some(Value::Str(s)) => match s.as_str() {
                "strict" => SchemaMode::Strict,
                "warn" => SchemaMode::Warn,
                _ => SchemaMode::Off,
            },
            _ => return None,
        };
        let mut node_classes = BTreeMap::new();
        if let Some(Value::Map(ncs)) = p.get("node_classes") {
            for (name, v) in ncs {
                if let Value::Map(m) = v {
                    node_classes.insert(
                        name.clone(),
                        NodeClassDef {
                            required: Self::str_list(m.get("required")),
                        },
                    );
                }
            }
        }
        let mut edge_classes = BTreeMap::new();
        if let Some(Value::Map(ecs)) = p.get("edge_classes") {
            for (name, v) in ecs {
                if let Value::Map(m) = v {
                    edge_classes.insert(
                        name.clone(),
                        EdgeClassDef {
                            domain: Self::str_list(m.get("domain")),
                            range: Self::str_list(m.get("range")),
                            max_out: match m.get("max_out") {
                                Some(Value::Int(i)) => Some(*i as u32),
                                _ => None,
                            },
                        },
                    );
                }
            }
        }
        Some(Schema {
            mode,
            node_classes,
            edge_classes,
        })
    }

    /// 그래프 상태에서 활성 스키마 로드.
    pub fn of_state(state: &GraphState) -> Option<Schema> {
        let u = state.by_slug(SCHEMA_SLUG)?;
        Self::from_props(&state.node(u)?.props)
    }

    // ---------- 검증 ----------

    /// touched 요소만 검증 (commit 게이트 경로). full 검증은 전 요소를 touched로.
    pub fn validate_elements(
        &self,
        state: &GraphState,
        nodes: &BTreeSet<Uuid>,
        edges: &BTreeSet<Uuid>,
    ) -> Vec<SchemaIssue> {
        let mut issues = Vec::new();
        for u in nodes {
            let Some(n) = state.node(*u) else { continue }; // 삭제된 요소는 검증 대상 아님
            for label in &n.labels {
                if let Some(nc) = self.node_classes.get(label) {
                    for req in &nc.required {
                        if !n.props.contains_key(req) {
                            issues.push(SchemaIssue::RequiredMissing {
                                class: label.clone(),
                                elem: *u,
                                key: req.clone(),
                            });
                        }
                    }
                }
            }
        }
        let mut card_seen: BTreeSet<(SmolStr, Uuid)> = BTreeSet::new();
        for u in edges {
            let Some(e) = state.edge(*u) else { continue };
            let Some(ec) = self.edge_classes.get(&e.etype) else {
                continue;
            };
            let label_hit = |end: Uuid, allowed: &[SmolStr]| -> bool {
                allowed.is_empty()
                    || state
                        .node(end)
                        .is_some_and(|n| n.labels.iter().any(|l| allowed.contains(l)))
            };
            if !label_hit(e.from, &ec.domain) {
                issues.push(SchemaIssue::DomainViolation {
                    edge_class: e.etype.clone(),
                    edge: *u,
                    end: e.from,
                    expected: ec.domain.clone(),
                });
            }
            if !label_hit(e.to, &ec.range) {
                issues.push(SchemaIssue::RangeViolation {
                    edge_class: e.etype.clone(),
                    edge: *u,
                    end: e.to,
                    expected: ec.range.clone(),
                });
            }
            if let Some(max) = ec.max_out {
                if card_seen.insert((e.etype.clone(), e.from)) {
                    let found = state
                        .edges()
                        .filter(|x| x.etype == e.etype && x.from == e.from)
                        .count() as u32;
                    if found > max {
                        issues.push(SchemaIssue::Cardinality {
                            edge_class: e.etype.clone(),
                            elem: e.from,
                            max,
                            found,
                        });
                    }
                }
            }
        }
        issues
    }

    /// 전체 상태 소급 검증 (dry-run — 골든 H5).
    pub fn validate_full(&self, state: &GraphState) -> Vec<SchemaIssue> {
        let nodes: BTreeSet<Uuid> = state.nodes().map(|n| n.uuid).collect();
        let edges: BTreeSet<Uuid> = state.edges().map(|e| e.uuid).collect();
        self.validate_elements(state, &nodes, &edges)
    }
}
