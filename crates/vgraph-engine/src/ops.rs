//! WorkingCopy op-log (설계 §4.1-3) — WAL 직렬화 + 결정적 replay.
//! WAL은 엔진 내부용이라 content-hash 대상이 아니지만, 파서 재사용을 위해
//! 값 인코딩은 canon 규약을 그대로 쓴다.

use smol_str::SmolStr;
use vgraph_core::canon::{esc, value_from_json, write_value, ParseError};
use vgraph_core::{GraphState, Node, Props, StateError, Uuid, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    AddNode {
        uuid: Uuid,
        labels: Vec<SmolStr>,
        slug: Option<SmolStr>,
        props: Props,
    },
    AddEdge {
        uuid: Uuid,
        etype: SmolStr,
        from: Uuid,
        to: Uuid,
        props: Props,
    },
    SetProp {
        elem: Uuid,
        key: SmolStr,
        value: Value,
    },
    UnsetProp {
        elem: Uuid,
        key: SmolStr,
    },
    RemoveNode {
        uuid: Uuid,
        cascade: bool,
    },
    RemoveEdge {
        uuid: Uuid,
    },
    SetSlug {
        uuid: Uuid,
        slug: Option<SmolStr>,
    },
    AddLabel {
        uuid: Uuid,
        label: SmolStr,
    },
    RemoveLabel {
        uuid: Uuid,
        label: SmolStr,
    },
}

fn props_json(p: &Props) -> String {
    vgraph_core::canon::props_json(p)
}

fn opt_str(s: &Option<SmolStr>) -> String {
    match s {
        Some(x) => esc(x),
        None => "null".into(),
    }
}

impl EditOp {
    pub fn to_json(&self) -> String {
        match self {
            EditOp::AddNode { uuid, labels, slug, props } => format!(
                "{{\"op\":\"add_node\",\"uuid\":{},\"labels\":[{}],\"slug\":{},\"props\":{}}}",
                esc(&uuid.to_string()),
                labels.iter().map(|l| esc(l)).collect::<Vec<_>>().join(","),
                opt_str(slug),
                props_json(props)
            ),
            EditOp::AddEdge { uuid, etype, from, to, props } => format!(
                "{{\"op\":\"add_edge\",\"uuid\":{},\"type\":{},\"from\":{},\"to\":{},\"props\":{}}}",
                esc(&uuid.to_string()),
                esc(etype),
                esc(&from.to_string()),
                esc(&to.to_string()),
                props_json(props)
            ),
            EditOp::SetProp { elem, key, value } => {
                let mut v = String::new();
                write_value(value, &mut v);
                format!(
                    "{{\"op\":\"set_prop\",\"elem\":{},\"key\":{},\"value\":{}}}",
                    esc(&elem.to_string()),
                    esc(key),
                    v
                )
            }
            EditOp::UnsetProp { elem, key } => format!(
                "{{\"op\":\"unset_prop\",\"elem\":{},\"key\":{}}}",
                esc(&elem.to_string()),
                esc(key)
            ),
            EditOp::RemoveNode { uuid, cascade } => format!(
                "{{\"op\":\"remove_node\",\"uuid\":{},\"cascade\":{}}}",
                esc(&uuid.to_string()),
                cascade
            ),
            EditOp::RemoveEdge { uuid } => {
                format!("{{\"op\":\"remove_edge\",\"uuid\":{}}}", esc(&uuid.to_string()))
            }
            EditOp::SetSlug { uuid, slug } => format!(
                "{{\"op\":\"set_slug\",\"uuid\":{},\"slug\":{}}}",
                esc(&uuid.to_string()),
                opt_str(slug)
            ),
            EditOp::AddLabel { uuid, label } => format!(
                "{{\"op\":\"add_label\",\"uuid\":{},\"label\":{}}}",
                esc(&uuid.to_string()),
                esc(label)
            ),
            EditOp::RemoveLabel { uuid, label } => format!(
                "{{\"op\":\"remove_label\",\"uuid\":{},\"label\":{}}}",
                esc(&uuid.to_string()),
                esc(label)
            ),
        }
    }

    pub fn from_json(s: &str) -> Result<EditOp, ParseError> {
        let j: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ParseError(e.to_string()))?;
        let str_of = |k: &str| -> Result<String, ParseError> {
            Ok(j.get(k)
                .and_then(|v| v.as_str())
                .ok_or_else(|| ParseError(format!("missing {k}")))?
                .to_string())
        };
        let uuid_of = |k: &str| -> Result<Uuid, ParseError> {
            str_of(k)?
                .parse()
                .map_err(|e| ParseError(format!("{k}: {e:?}")))
        };
        let slug_of =
            |k: &str| -> Option<SmolStr> { j.get(k).and_then(|v| v.as_str()).map(SmolStr::new) };
        let props_of = |k: &str| -> Result<Props, ParseError> {
            vgraph_core::canon::props_from_json(
                j.get(k).ok_or_else(|| ParseError(format!("missing {k}")))?,
            )
        };
        let op = j
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ParseError("missing op".into()))?;
        Ok(match op {
            "add_node" => EditOp::AddNode {
                uuid: uuid_of("uuid")?,
                labels: j
                    .get("labels")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| ParseError("labels".into()))?
                    .iter()
                    .filter_map(|l| l.as_str().map(SmolStr::new))
                    .collect(),
                slug: slug_of("slug"),
                props: props_of("props")?,
            },
            "add_edge" => EditOp::AddEdge {
                uuid: uuid_of("uuid")?,
                etype: SmolStr::new(str_of("type")?),
                from: uuid_of("from")?,
                to: uuid_of("to")?,
                props: props_of("props")?,
            },
            "set_prop" => EditOp::SetProp {
                elem: uuid_of("elem")?,
                key: SmolStr::new(str_of("key")?),
                value: value_from_json(j.get("value").ok_or_else(|| ParseError("value".into()))?)?,
            },
            "unset_prop" => EditOp::UnsetProp {
                elem: uuid_of("elem")?,
                key: SmolStr::new(str_of("key")?),
            },
            "remove_node" => EditOp::RemoveNode {
                uuid: uuid_of("uuid")?,
                cascade: j.get("cascade").and_then(|v| v.as_bool()).unwrap_or(false),
            },
            "remove_edge" => EditOp::RemoveEdge {
                uuid: uuid_of("uuid")?,
            },
            "set_slug" => EditOp::SetSlug {
                uuid: uuid_of("uuid")?,
                slug: slug_of("slug"),
            },
            "add_label" => EditOp::AddLabel {
                uuid: uuid_of("uuid")?,
                label: SmolStr::new(str_of("label")?),
            },
            "remove_label" => EditOp::RemoveLabel {
                uuid: uuid_of("uuid")?,
                label: SmolStr::new(str_of("label")?),
            },
            other => return Err(ParseError(format!("unknown op {other}"))),
        })
    }

    /// work state에 적용 — 세션 경로와 WAL replay 경로가 동일 코드를 탄다 (결정성).
    pub fn apply(&self, s: &mut GraphState) -> Result<(), StateError> {
        match self {
            EditOp::AddNode {
                uuid,
                labels,
                slug,
                props,
            } => {
                let mut labels: Vec<SmolStr> = labels.clone();
                labels.sort();
                labels.dedup();
                s.insert_node(Node {
                    uuid: *uuid,
                    labels,
                    slug: slug.clone(),
                    props: props.clone(),
                })
            }
            EditOp::AddEdge {
                uuid,
                etype,
                from,
                to,
                props,
            } => s.insert_edge(vgraph_core::Edge {
                uuid: *uuid,
                etype: etype.clone(),
                from: *from,
                to: *to,
                props: props.clone(),
            }),
            EditOp::SetProp { elem, key, value } => {
                s.set_prop(*elem, key, value.clone()).map(|_| ())
            }
            EditOp::UnsetProp { elem, key } => s.unset_prop(*elem, key).map(|_| ()),
            EditOp::RemoveNode { uuid, cascade } => s.remove_node(*uuid, *cascade).map(|_| ()),
            EditOp::RemoveEdge { uuid } => s.remove_edge(*uuid).map(|_| ()),
            EditOp::SetSlug { uuid, slug } => s.set_slug(*uuid, slug.as_deref()).map(|_| ()),
            EditOp::AddLabel { uuid, label } => s.add_label(*uuid, label).map(|_| ()),
            EditOp::RemoveLabel { uuid, label } => s.remove_label(*uuid, label).map(|_| ()),
        }
    }
}

/// WAL 첫 레코드 — 세션 메타 (resume의 앵커).
pub struct WalHeader {
    pub base: Option<vgraph_core::Hash256>,
    pub branch: Option<String>,
    pub owner: String,
    /// 이름 있는 작업 공간. `None` = 기본 작업 공간(머리말에 키를 쓰지 않는다).
    pub workspace: Option<String>,
}

impl WalHeader {
    pub fn to_json(&self) -> String {
        let workspace = match &self.workspace {
            Some(w) => format!(",\"workspace\":{}", esc(w)),
            None => String::new(),
        };
        format!(
            "{{\"hdr\":true,\"base\":{},\"branch\":{},\"owner\":{}{}}}",
            match &self.base {
                Some(h) => esc(&h.to_string()),
                None => "null".into(),
            },
            match &self.branch {
                Some(b) => esc(b),
                None => "null".into(),
            },
            esc(&self.owner),
            workspace
        )
    }

    pub fn from_json(s: &str) -> Result<WalHeader, ParseError> {
        let j: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ParseError(e.to_string()))?;
        if j.get("hdr").and_then(|v| v.as_bool()) != Some(true) {
            return Err(ParseError("not a wal header".into()));
        }
        Ok(WalHeader {
            base: j
                .get("base")
                .and_then(|v| v.as_str())
                .map(|s| s.parse())
                .transpose()
                .map_err(|e| ParseError(format!("{e}")))?,
            branch: j.get("branch").and_then(|v| v.as_str()).map(String::from),
            owner: j
                .get("owner")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ParseError("owner".into()))?
                .to_string(),
            workspace: j
                .get("workspace")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}
