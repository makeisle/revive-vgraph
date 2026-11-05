//! P8 — ssot-engine review_queue(ProposeQueue) 접속 어댑터.
//!
//! 현행 스키마 근거: ssot-engine review_queue의 Proposal 형식 — 요지는 아래와 같다.
//! - Proposal JSON = { id, target, field, new_value, rule_id?, status } (FieldChange flatten)
//! - id = sha256(target \0 field \0 json(new_value)) hex 앞 16자 — **멱등 enqueue의 키**
//! - 거부 tombstone: 같은 id 재제출 시 Rejected 유지
//!
//! 매핑 원칙: 충돌 1건 = 제안 1건, `new_value` = **theirs(상대측) 값** —
//! 승인 = theirs 채택, 거부 = ours 유지. (그들의 "제안된 변경" 의미론과 일치)

use crate::error::{ConflictItem, ConflictReport, Side, SideRef};
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;
use vgraph_core::canon::write_value;
use vgraph_core::Value;

fn value_to_json(v: &Value) -> serde_json::Value {
    let mut s = String::new();
    write_value(v, &mut s);
    serde_json::from_str(&s).unwrap_or(serde_json::Value::Null)
}

fn opt_value_to_json(v: &Option<Value>) -> serde_json::Value {
    match v {
        Some(x) => value_to_json(x),
        None => serde_json::Value::Null,
    }
}

/// ProposeQueue의 id 공식 복제 (queue.rs FieldChange::id).
fn proposal_id(target: &str, field: &str, new_value: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(target.as_bytes());
    h.update(b"\0");
    h.update(field.as_bytes());
    h.update(b"\0");
    h.update(
        serde_json::to_string(new_value)
            .unwrap_or_default()
            .as_bytes(),
    );
    format!("{:x}", h.finalize())[..16].to_string()
}

fn side_str(s: Side) -> &'static str {
    match s {
        Side::Ours => "ours",
        Side::Theirs => "theirs",
    }
}

fn side_ref_json(s: &SideRef) -> serde_json::Value {
    match s {
        SideRef::Commit(h) => serde_json::json!({ "commit": h.to_string() }),
        SideRef::Working(w) => serde_json::json!({ "working": w.to_string() }),
    }
}

/// ConflictReport → ProposeQueue 호환 Proposal JSON 목록.
/// `graph_addr` = "ns/slug@branch" (target URI의 그래프 부분).
pub fn to_proposals(report: &ConflictReport, graph_addr: &str) -> Vec<serde_json::Value> {
    let ctx = serde_json::json!({
        "base": report.base.map(|h| h.to_string()),
        "ours": side_ref_json(&report.ours),
        "theirs": side_ref_json(&report.theirs),
    });
    report
        .items
        .iter()
        .map(|item| {
            let (target, field, new_value) = match item {
                ConflictItem::PropBothSet { elem, key, base, ours, theirs } => (
                    format!("vgraph://{graph_addr}#{elem}"),
                    key.to_string(),
                    serde_json::json!({
                        "kind": "prop_both_set",
                        "base": opt_value_to_json(base),
                        "ours": opt_value_to_json(ours),
                        "theirs": opt_value_to_json(theirs),
                        "apply": opt_value_to_json(theirs), // 승인 시 채택될 값
                    }),
                ),
                ConflictItem::RemovedVsEdit { elem, removed_by } => (
                    format!("vgraph://{graph_addr}#{elem}"),
                    "__exists".to_string(),
                    serde_json::json!({ "kind": "removed_vs_edit", "removed_by": side_str(*removed_by) }),
                ),
                ConflictItem::DanglingEdge { edge, missing_end, removed_by } => (
                    format!("vgraph://{graph_addr}#{edge}"),
                    "__dangling".to_string(),
                    serde_json::json!({
                        "kind": "dangling_edge",
                        "missing_end": missing_end.to_string(),
                        "removed_by": side_str(*removed_by),
                    }),
                ),
                ConflictItem::SlugCollision { slug, ours, theirs } => (
                    format!("vgraph://{graph_addr}#slug/{slug}"),
                    "__slug".to_string(),
                    serde_json::json!({
                        "kind": "slug_collision",
                        "ours": ours.to_string(),
                        "theirs": theirs.to_string(),
                    }),
                ),
                ConflictItem::SchemaClash { issue } => (
                    format!("vgraph://{graph_addr}"),
                    "__schema".to_string(),
                    serde_json::json!({ "kind": "schema_clash", "issue": format!("{issue:?}") }),
                ),
                ConflictItem::ApplyFailed { reason } => (
                    format!("vgraph://{graph_addr}"),
                    "__merge".to_string(),
                    serde_json::json!({ "kind": "apply_failed", "reason": reason }),
                ),
            };
            let id = proposal_id(&target, &field, &new_value);
            serde_json::json!({
                "id": id,
                "target": target,
                "field": field,
                "new_value": new_value,
                "rule_id": "vgraph-merge",
                "status": "pending",
                "context": ctx,
            })
        })
        .collect()
}

/// ProposeQueue 디렉터리에 직접 기록 (`<id>.json`) — ssot-engine이 그대로 읽는다.
/// 반환 = 기록된 id 목록. 이미 존재하는 id(중복/거부 tombstone)는 건드리지 않는다 (멱등).
pub fn enqueue_dir(
    dir: &Path,
    report: &ConflictReport,
    graph_addr: &str,
) -> io::Result<Vec<String>> {
    std::fs::create_dir_all(dir)?;
    let mut written = Vec::new();
    for p in to_proposals(report, graph_addr) {
        let id = p["id"].as_str().unwrap_or_default().to_string();
        let path = dir.join(format!("{id}.json"));
        if path.exists() {
            continue; // 멱등 + tombstone 존중 (queue.rs enqueue와 동일 의미)
        }
        std::fs::write(&path, serde_json::to_string_pretty(&p)?)?;
        written.push(id);
    }
    Ok(written)
}
