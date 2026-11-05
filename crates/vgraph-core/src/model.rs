//! LPG 코어 모델 (설계 §2.1): Node/Edge 모두 1급, 안정 uuid.

use crate::id::Uuid;
use crate::value::Props;
use smol_str::SmolStr;

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub uuid: Uuid,
    /// 항상 정렬 상태 유지 (GraphState가 보장) — canonical 결정성.
    pub labels: Vec<SmolStr>,
    pub slug: Option<SmolStr>,
    pub props: Props,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub uuid: Uuid,
    pub etype: SmolStr,
    pub from: Uuid,
    pub to: Uuid,
    pub props: Props,
}

impl Node {
    pub fn new(uuid: Uuid, labels: &[&str], slug: Option<&str>, props: Props) -> Self {
        let mut labels: Vec<SmolStr> = labels.iter().map(|l| SmolStr::new(l)).collect();
        labels.sort();
        labels.dedup();
        Node {
            uuid,
            labels,
            slug: slug.map(SmolStr::new),
            props,
        }
    }
}

impl Edge {
    pub fn new(uuid: Uuid, etype: &str, from: Uuid, to: Uuid, props: Props) -> Self {
        Edge {
            uuid,
            etype: SmolStr::new(etype),
            from,
            to,
            props,
        }
    }
}
