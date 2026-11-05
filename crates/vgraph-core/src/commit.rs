//! commit 객체 (설계 §5.2) — content-hash ID.

use crate::canon::{esc, sha256, Hash256, ParseError};

#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    /// merge commit이면 2개 (dst 먼저).
    pub parents: Vec<Hash256>,
    pub delta: Hash256,
    /// KG 오버레이 버전 (스키마 변경도 commit — 설계 §2.2).
    pub schema_rev: Option<Hash256>,
    pub author: String,
    /// RFC3339 (Clock 주입 — 인터페이스 §1.1).
    pub ts: String,
    pub msg: String,
}

impl Commit {
    /// canonical: 키 정렬 무공백 JSON.
    pub fn canonical_json(&self) -> String {
        let parents = self
            .parents
            .iter()
            .map(|h| esc(&h.to_string()))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"author\":{},\"delta\":{},\"msg\":{},\"parents\":[{}],\"schema_rev\":{},\"ts\":{}}}",
            esc(&self.author),
            esc(&self.delta.to_string()),
            esc(&self.msg),
            parents,
            match &self.schema_rev {
                Some(h) => esc(&h.to_string()),
                None => "null".into(),
            },
            esc(&self.ts)
        )
    }

    pub fn hash(&self) -> Hash256 {
        sha256(self.canonical_json().as_bytes())
    }

    pub fn from_json(s: &str) -> Result<Commit, ParseError> {
        let j: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ParseError(e.to_string()))?;
        let str_of = |k: &str| -> Result<String, ParseError> {
            Ok(j.get(k)
                .and_then(|v| v.as_str())
                .ok_or_else(|| ParseError(format!("missing {k}")))?
                .to_string())
        };
        let hash_of = |s: &str| -> Result<Hash256, ParseError> {
            s.parse().map_err(|e| ParseError(format!("{e}")))
        };
        Ok(Commit {
            parents: j
                .get("parents")
                .and_then(|v| v.as_array())
                .ok_or_else(|| ParseError("missing parents".into()))?
                .iter()
                .map(|p| {
                    p.as_str()
                        .ok_or_else(|| ParseError("parent must be string".into()))
                        .and_then(hash_of)
                })
                .collect::<Result<_, _>>()?,
            delta: hash_of(&str_of("delta")?)?,
            schema_rev: match j.get("schema_rev") {
                Some(v) if !v.is_null() => Some(hash_of(
                    v.as_str().ok_or_else(|| ParseError("schema_rev".into()))?,
                )?),
                _ => None,
            },
            author: str_of("author")?,
            ts: str_of("ts")?,
            msg: str_of("msg")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canon::sha256;

    #[test]
    fn commit_hash_roundtrip() {
        let c = Commit {
            parents: vec![sha256(b"p")],
            delta: sha256(b"d"),
            schema_rev: None,
            author: "ceo".into(),
            ts: "2026-08-09T00:00:01Z".into(),
            msg: "F0".into(),
        };
        let json = c.canonical_json();
        let c2 = Commit::from_json(&json).unwrap();
        assert_eq!(c, c2);
        assert_eq!(c.hash(), c2.hash());
    }
}
