//! 가역(invertible) delta (설계 §5.3) + diff 기반 정규화 빌더.
//!
//! 정규화 전략: 빌더는 base/work 두 GraphState를 들고, build() = diff(base, work).
//! 중복 set(D2)·add후remove 상쇄(D3)가 diff에서 자동으로 정규화된다.
//! removed/old에 전체 값 보존 → invert()가 구조적으로 성립 (D4).

use crate::canon::{
    edge_from_json, edge_json, esc, node_from_json, node_json, sha256, value_from_json,
    write_value, Hash256, ParseError,
};
use crate::id::Uuid;
use crate::model::{Edge, Node};
use crate::state::{GraphState, StateError};
use crate::value::Value;
use smol_str::SmolStr;

#[derive(Debug, Clone, PartialEq)]
pub struct PropSet {
    pub elem: Uuid,
    pub key: SmolStr,
    /// None = 키가 없었음(신규).
    pub old: Option<Value>,
    /// None = 키 제거(unset).
    pub new: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LabelsDelta {
    pub elem: Uuid,
    pub added: Vec<SmolStr>,
    pub removed: Vec<SmolStr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlugSet {
    pub elem: Uuid,
    pub old: Option<SmolStr>,
    pub new: Option<SmolStr>,
}

/// 요소 단위 가역 delta. 모든 벡터는 uuid/(elem,key) 오름차순 (canonical).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Delta {
    pub nodes_added: Vec<Node>,
    pub nodes_removed: Vec<Node>,
    pub edges_added: Vec<Edge>,
    pub edges_removed: Vec<Edge>,
    pub props_set: Vec<PropSet>,
    pub labels_delta: Vec<LabelsDelta>,
    pub slugs_set: Vec<SlugSet>,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.nodes_added.is_empty()
            && self.nodes_removed.is_empty()
            && self.edges_added.is_empty()
            && self.edges_removed.is_empty()
            && self.props_set.is_empty()
            && self.labels_delta.is_empty()
            && self.slugs_set.is_empty()
    }

    /// 역delta — added↔removed, old↔new 스왑. apply(invert) = 역행 (D4).
    pub fn invert(&self) -> Delta {
        Delta {
            nodes_added: self.nodes_removed.clone(),
            nodes_removed: self.nodes_added.clone(),
            edges_added: self.edges_removed.clone(),
            edges_removed: self.edges_added.clone(),
            props_set: self
                .props_set
                .iter()
                .map(|p| PropSet {
                    elem: p.elem,
                    key: p.key.clone(),
                    old: p.new.clone(),
                    new: p.old.clone(),
                })
                .collect(),
            labels_delta: self
                .labels_delta
                .iter()
                .map(|l| LabelsDelta {
                    elem: l.elem,
                    added: l.removed.clone(),
                    removed: l.added.clone(),
                })
                .collect(),
            slugs_set: self
                .slugs_set
                .iter()
                .map(|s| SlugSet {
                    elem: s.elem,
                    old: s.new.clone(),
                    new: s.old.clone(),
                })
                .collect(),
        }
    }

    // ---------- canonical 직렬화 / 해시 / 파싱 ----------

    pub fn canonical_json(&self) -> String {
        fn opt_val(v: &Option<Value>, out: &mut String) {
            match v {
                Some(x) => write_value(x, out),
                None => out.push_str("null"),
            }
        }
        fn opt_str(s: &Option<SmolStr>) -> String {
            match s {
                Some(x) => esc(x),
                None => "null".into(),
            }
        }
        let mut out = String::from("{\"edges_added\":[");
        for (i, e) in self.edges_added.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&edge_json(e));
        }
        out.push_str("],\"edges_removed\":[");
        for (i, e) in self.edges_removed.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&edge_json(e));
        }
        out.push_str("],\"labels_delta\":[");
        for (i, l) in self.labels_delta.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"elem\":{},\"added\":[{}],\"removed\":[{}]}}",
                esc(&l.elem.to_string()),
                l.added.iter().map(|s| esc(s)).collect::<Vec<_>>().join(","),
                l.removed
                    .iter()
                    .map(|s| esc(s))
                    .collect::<Vec<_>>()
                    .join(","),
            ));
        }
        out.push_str("],\"nodes_added\":[");
        for (i, n) in self.nodes_added.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&node_json(n));
        }
        out.push_str("],\"nodes_removed\":[");
        for (i, n) in self.nodes_removed.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&node_json(n));
        }
        out.push_str("],\"props_set\":[");
        for (i, p) in self.props_set.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"elem\":");
            out.push_str(&esc(&p.elem.to_string()));
            out.push_str(",\"key\":");
            out.push_str(&esc(&p.key));
            out.push_str(",\"old\":");
            opt_val(&p.old, &mut out);
            out.push_str(",\"new\":");
            opt_val(&p.new, &mut out);
            out.push('}');
        }
        out.push_str("],\"slugs_set\":[");
        for (i, s) in self.slugs_set.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"elem\":{},\"old\":{},\"new\":{}}}",
                esc(&s.elem.to_string()),
                opt_str(&s.old),
                opt_str(&s.new)
            ));
        }
        out.push_str("]}");
        out
    }

    pub fn hash(&self) -> Hash256 {
        sha256(self.canonical_json().as_bytes())
    }

    pub fn from_json(s: &str) -> Result<Delta, ParseError> {
        let j: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ParseError(e.to_string()))?;
        let arr = |k: &str| -> Result<Vec<serde_json::Value>, ParseError> {
            Ok(j.get(k)
                .and_then(|x| x.as_array())
                .ok_or_else(|| ParseError(format!("missing array {k}")))?
                .clone())
        };
        let uuid_of = |x: &serde_json::Value, k: &str| -> Result<Uuid, ParseError> {
            x.get(k)
                .and_then(|v| v.as_str())
                .ok_or_else(|| ParseError(format!("missing {k}")))?
                .parse()
                .map_err(|e| ParseError(format!("{k}: {e:?}")))
        };
        let opt_value = |x: &serde_json::Value, k: &str| -> Result<Option<Value>, ParseError> {
            let v = x.get(k).ok_or_else(|| ParseError(format!("missing {k}")))?;
            if v.is_null() {
                Ok(None)
            } else {
                Ok(Some(value_from_json(v)?))
            }
        };
        let opt_slug = |x: &serde_json::Value, k: &str| -> Result<Option<SmolStr>, ParseError> {
            Ok(x.get(k)
                .ok_or_else(|| ParseError(format!("missing {k}")))?
                .as_str()
                .map(SmolStr::new))
        };
        let strs = |x: &serde_json::Value, k: &str| -> Result<Vec<SmolStr>, ParseError> {
            x.get(k)
                .and_then(|v| v.as_array())
                .ok_or_else(|| ParseError(format!("missing {k}")))?
                .iter()
                .map(|s| s.as_str().map(SmolStr::new))
                .collect::<Option<_>>()
                .ok_or_else(|| ParseError(format!("{k} must be strings")))
        };
        Ok(Delta {
            nodes_added: arr("nodes_added")?
                .iter()
                .map(node_from_json)
                .collect::<Result<_, _>>()?,
            nodes_removed: arr("nodes_removed")?
                .iter()
                .map(node_from_json)
                .collect::<Result<_, _>>()?,
            edges_added: arr("edges_added")?
                .iter()
                .map(edge_from_json)
                .collect::<Result<_, _>>()?,
            edges_removed: arr("edges_removed")?
                .iter()
                .map(edge_from_json)
                .collect::<Result<_, _>>()?,
            props_set: arr("props_set")?
                .iter()
                .map(|x| {
                    Ok(PropSet {
                        elem: uuid_of(x, "elem")?,
                        key: SmolStr::new(
                            x.get("key")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| ParseError("missing key".into()))?,
                        ),
                        old: opt_value(x, "old")?,
                        new: opt_value(x, "new")?,
                    })
                })
                .collect::<Result<_, ParseError>>()?,
            labels_delta: arr("labels_delta")?
                .iter()
                .map(|x| {
                    Ok(LabelsDelta {
                        elem: uuid_of(x, "elem")?,
                        added: strs(x, "added")?,
                        removed: strs(x, "removed")?,
                    })
                })
                .collect::<Result<_, ParseError>>()?,
            slugs_set: arr("slugs_set")?
                .iter()
                .map(|x| {
                    Ok(SlugSet {
                        elem: uuid_of(x, "elem")?,
                        old: opt_slug(x, "old")?,
                        new: opt_slug(x, "new")?,
                    })
                })
                .collect::<Result<_, ParseError>>()?,
        })
    }
}

/// base와 work의 구조적 diff → 정규화된 canonical delta.
pub fn diff(base: &GraphState, work: &GraphState) -> Delta {
    let mut d = Delta::default();

    // 노드: 추가/제거/공통(props·labels·slug 비교)
    for n in work.nodes() {
        match base.node(n.uuid) {
            None => d.nodes_added.push(n.clone()),
            Some(b) => {
                diff_props(n.uuid, &b.props, &n.props, &mut d.props_set);
                let added: Vec<SmolStr> = n
                    .labels
                    .iter()
                    .filter(|l| !b.labels.contains(l))
                    .cloned()
                    .collect();
                let removed: Vec<SmolStr> = b
                    .labels
                    .iter()
                    .filter(|l| !n.labels.contains(l))
                    .cloned()
                    .collect();
                if !added.is_empty() || !removed.is_empty() {
                    d.labels_delta.push(LabelsDelta {
                        elem: n.uuid,
                        added,
                        removed,
                    });
                }
                if b.slug != n.slug {
                    d.slugs_set.push(SlugSet {
                        elem: n.uuid,
                        old: b.slug.clone(),
                        new: n.slug.clone(),
                    });
                }
            }
        }
    }
    for n in base.nodes() {
        if work.node(n.uuid).is_none() {
            d.nodes_removed.push(n.clone());
        }
    }

    // 엣지: 추가/제거/공통(props 비교)
    for e in work.edges() {
        match base.edge(e.uuid) {
            None => d.edges_added.push(e.clone()),
            Some(b) => diff_props(e.uuid, &b.props, &e.props, &mut d.props_set),
        }
    }
    for e in base.edges() {
        if work.edge(e.uuid).is_none() {
            d.edges_removed.push(e.clone());
        }
    }

    // canonical 정렬 (BTreeMap 순회라 대부분 이미 정렬이지만 props_set은 노드/엣지 혼합)
    d.props_set
        .sort_by(|a, b| (a.elem, &a.key).cmp(&(b.elem, &b.key)));
    d
}

fn diff_props(
    elem: Uuid,
    old: &crate::value::Props,
    new: &crate::value::Props,
    out: &mut Vec<PropSet>,
) {
    for (k, nv) in new {
        match old.get(k) {
            Some(ov) if ov == nv => {}
            ov => out.push(PropSet {
                elem,
                key: k.clone(),
                old: ov.cloned(),
                new: Some(nv.clone()),
            }),
        }
    }
    for (k, ov) in old {
        if !new.contains_key(k) {
            out.push(PropSet {
                elem,
                key: k.clone(),
                old: Some(ov.clone()),
                new: None,
            });
        }
    }
}

/// delta 적용 — 순서: 엣지제거 → 노드제거 → 노드추가 → 엣지추가 → props/labels/slug.
/// 제거를 먼저 해 "제거된 노드의 slug를 새 노드가 재사용"하는 delta가 성립한다.
/// (한 delta 안에서 기존 노드의 slug 재배치 + 그 slug 재사용 조합은 v0 미지원)
/// `apply(base, diff(base, work)) ≡ work` (K6 검증 대상).
pub fn apply(state: &mut GraphState, d: &Delta) -> Result<(), StateError> {
    for e in &d.edges_removed {
        state.remove_edge(e.uuid)?;
    }
    for n in &d.nodes_removed {
        state.remove_node(n.uuid, false)?;
    }
    for n in &d.nodes_added {
        state.insert_node(n.clone())?;
    }
    for e in &d.edges_added {
        state.insert_edge(e.clone())?;
    }
    for p in &d.props_set {
        match &p.new {
            Some(v) => {
                state.set_prop(p.elem, &p.key, v.clone())?;
            }
            None => {
                state.unset_prop(p.elem, &p.key)?;
            }
        }
    }
    for l in &d.labels_delta {
        for a in &l.added {
            state.add_label(l.elem, a)?;
        }
        for r in &l.removed {
            state.remove_label(l.elem, r)?;
        }
    }
    for s in &d.slugs_set {
        state.set_slug(s.elem, s.new.as_deref())?;
    }
    Ok(())
}

/// diff 기반 빌더 — WorkingCopy(P3)의 정규화 코어.
/// 편집은 `work`에 직접 가하고, build()가 canonical delta를 산출한다.
pub struct DeltaBuilder {
    base: GraphState,
    pub work: GraphState,
}

impl DeltaBuilder {
    pub fn new(base: &GraphState) -> Self {
        DeltaBuilder {
            base: base.clone(),
            work: base.clone(),
        }
    }
    /// rebase용: 새 base 위에 이미 적용된 work를 얹어 재구성 (엔진 P3).
    pub fn with_work(base: GraphState, work: GraphState) -> Self {
        DeltaBuilder { base, work }
    }
    pub fn base(&self) -> &GraphState {
        &self.base
    }
    pub fn build(&self) -> Delta {
        diff(&self.base, &self.work)
    }
}
