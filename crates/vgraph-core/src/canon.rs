//! canonical 직렬화 + SHA-256 content-hash (계획 결정 D1·D3·D4).
//!
//! 규약:
//! - 객체 키 정렬(BTreeMap), 무공백, UTF-8.
//! - Int = 십진. Float = Rust 최단 round-trip 표기, 정수형이면 ".0" 강제 부착
//!   (파스 시 '.'/'e' 유무로 Int/Float 구분 → 타입 보존). NaN/∞ 금지(panic).
//! - Decimal={"$dec":s}, Bytes={"$hex":hex}, Ts={"$ts":s}, Ref={"$ref":s}.
//!   일반 Map의 단일 "$…" 키는 예약(사용 금지).
//! - 해시 표기 = "sha256:<hex64>" — lmd manifest와 동일 포맷 (D1).

use crate::model::{Edge, Node};
use crate::value::{Props, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Hash256(pub [u8; 32]);

impl fmt::Display for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", hex(&self.0))
    }
}
impl fmt::Debug for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}…", &hex(&self.0)[..8])
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid hash literal: {0}")]
pub struct HashParseError(String);

impl FromStr for Hash256 {
    type Err = HashParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let h = s.strip_prefix("sha256:").unwrap_or(s);
        if h.len() != 64 {
            return Err(HashParseError(s.into()));
        }
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&h[2 * i..2 * i + 2], 16)
                .map_err(|_| HashParseError(s.into()))?;
        }
        Ok(Hash256(out))
    }
}

pub fn sha256(bytes: &[u8]) -> Hash256 {
    Hash256(Sha256::digest(bytes).into())
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

/// JSON 문자열 리터럴(따옴표 포함, serde_json 이스케이프 = 결정적).
pub fn esc(s: &str) -> String {
    serde_json::to_string(s).expect("string escape")
}

pub fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => {
            assert!(f.is_finite(), "canonical Float는 NaN/∞ 금지 (D4)");
            let s = format!("{f}");
            out.push_str(&s);
            if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                out.push_str(".0"); // 타입 보존: Float은 항상 소수점 표기
            }
        }
        Value::Decimal(s) => {
            out.push_str("{\"$dec\":");
            out.push_str(&esc(s));
            out.push('}');
        }
        Value::Str(s) => out.push_str(&esc(s)),
        Value::Bytes(b) => {
            out.push_str("{\"$hex\":");
            out.push_str(&esc(&hex(b)));
            out.push('}');
        }
        Value::Ts(s) => {
            out.push_str("{\"$ts\":");
            out.push_str(&esc(s));
            out.push('}');
        }
        Value::Ref(s) => {
            out.push_str("{\"$ref\":");
            out.push_str(&esc(s));
            out.push('}');
        }
        Value::List(xs) => {
            out.push('[');
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(x, out);
            }
            out.push(']');
        }
        Value::Map(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                debug_assert!(!(m.len() == 1 && k.starts_with('$')), "단일 $키 Map은 예약");
                out.push_str(&esc(k));
                out.push(':');
                write_value(x, out);
            }
            out.push('}');
        }
    }
}

pub fn props_json(p: &Props) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in p.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&esc(k));
        out.push(':');
        write_value(v, &mut out);
    }
    out.push('}');
    out
}

/// 소스 계층 payload 해시 (설계 §5.4) — props canonical bytes의 SHA-256.
pub fn payload_hash(p: &Props) -> Hash256 {
    sha256(props_json(p).as_bytes())
}

/// dump/blob용 Node 직렬화 — 필드 순서 고정: uuid, slug, labels, props (골든 F0_DUMP).
pub fn node_json(n: &Node) -> String {
    let mut out = String::from("{\"uuid\":");
    out.push_str(&esc(&n.uuid.to_string()));
    out.push_str(",\"slug\":");
    match &n.slug {
        Some(s) => out.push_str(&esc(s)),
        None => out.push_str("null"),
    }
    out.push_str(",\"labels\":[");
    for (i, l) in n.labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&esc(l));
    }
    out.push_str("],\"props\":");
    out.push_str(&props_json(&n.props));
    out.push('}');
    out
}

/// 필드 순서 고정: uuid, type, from, to, props.
pub fn edge_json(e: &Edge) -> String {
    format!(
        "{{\"uuid\":{},\"type\":{},\"from\":{},\"to\":{},\"props\":{}}}",
        esc(&e.uuid.to_string()),
        esc(&e.etype),
        esc(&e.from.to_string()),
        esc(&e.to.to_string()),
        props_json(&e.props)
    )
}

// ---------- 파싱 (blob 재구성용 — 쓰기는 위 writer만 사용) ----------

#[derive(Debug, thiserror::Error)]
#[error("canonical parse error: {0}")]
pub struct ParseError(pub String);

pub fn value_from_json(j: &serde_json::Value) -> Result<Value, ParseError> {
    use serde_json::Value as J;
    Ok(match j {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().ok_or_else(|| ParseError("bad number".into()))?)
            }
        }
        J::String(s) => Value::Str(s.clone()),
        J::Array(xs) => Value::List(xs.iter().map(value_from_json).collect::<Result<_, _>>()?),
        J::Object(m) => {
            if m.len() == 1 {
                let (k, v) = m.iter().next().unwrap();
                let s = || -> Result<&str, ParseError> {
                    v.as_str()
                        .ok_or_else(|| ParseError(format!("{k} must be string")))
                };
                match k.as_str() {
                    "$dec" => return Ok(Value::Decimal(s()?.into())),
                    "$ts" => return Ok(Value::Ts(s()?.into())),
                    "$ref" => return Ok(Value::Ref(s()?.into())),
                    "$hex" => {
                        return Ok(Value::Bytes(
                            hex_decode(s()?).ok_or_else(|| ParseError("bad hex".into()))?,
                        ))
                    }
                    _ => {}
                }
            }
            let mut out = std::collections::BTreeMap::new();
            for (k, v) in m {
                out.insert(smol_str::SmolStr::new(k), value_from_json(v)?);
            }
            Value::Map(out)
        }
    })
}

pub fn props_from_json(j: &serde_json::Value) -> Result<Props, ParseError> {
    let obj = j
        .as_object()
        .ok_or_else(|| ParseError("props must be object".into()))?;
    let mut p = Props::new();
    for (k, v) in obj {
        p.insert(smol_str::SmolStr::new(k), value_from_json(v)?);
    }
    Ok(p)
}

fn get<'a>(j: &'a serde_json::Value, k: &str) -> Result<&'a serde_json::Value, ParseError> {
    j.get(k)
        .ok_or_else(|| ParseError(format!("missing field {k}")))
}

fn get_uuid(j: &serde_json::Value, k: &str) -> Result<crate::id::Uuid, ParseError> {
    get(j, k)?
        .as_str()
        .ok_or_else(|| ParseError(format!("{k} must be string")))?
        .parse()
        .map_err(|e| ParseError(format!("{k}: {e:?}")))
}

pub fn node_from_json(j: &serde_json::Value) -> Result<Node, ParseError> {
    let labels: Vec<smol_str::SmolStr> = get(j, "labels")?
        .as_array()
        .ok_or_else(|| ParseError("labels must be array".into()))?
        .iter()
        .map(|l| l.as_str().map(smol_str::SmolStr::new))
        .collect::<Option<_>>()
        .ok_or_else(|| ParseError("labels must be strings".into()))?;
    Ok(Node {
        uuid: get_uuid(j, "uuid")?,
        slug: get(j, "slug")?.as_str().map(smol_str::SmolStr::new),
        labels,
        props: props_from_json(get(j, "props")?)?,
    })
}

pub fn edge_from_json(j: &serde_json::Value) -> Result<Edge, ParseError> {
    Ok(Edge {
        uuid: get_uuid(j, "uuid")?,
        etype: smol_str::SmolStr::new(
            get(j, "type")?
                .as_str()
                .ok_or_else(|| ParseError("type must be string".into()))?,
        ),
        from: get_uuid(j, "from")?,
        to: get_uuid(j, "to")?,
        props: props_from_json(get(j, "props")?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::props;

    #[test]
    fn float_type_preserved() {
        let mut s = String::new();
        write_value(&Value::Float(1.0), &mut s);
        assert_eq!(s, "1.0");
        let mut s2 = String::new();
        write_value(&Value::Float(0.7), &mut s2);
        assert_eq!(s2, "0.7");
        let j: serde_json::Value = serde_json::from_str("1.0").unwrap();
        assert_eq!(value_from_json(&j).unwrap(), Value::Float(1.0));
        let j2: serde_json::Value = serde_json::from_str("1").unwrap();
        assert_eq!(value_from_json(&j2).unwrap(), Value::Int(1));
    }

    #[test]
    fn payload_hash_stable_and_shared() {
        let a = props([("b", 1.into()), ("a", "x".into())]);
        let b = props([("a", "x".into()), ("b", 1.into())]);
        assert_eq!(payload_hash(&a), payload_hash(&b)); // 키 순서 무관 (dedup 전제)
    }

    #[test]
    fn hash_display_roundtrip() {
        let h = sha256(b"hello");
        let s = h.to_string();
        assert!(s.starts_with("sha256:"));
        assert_eq!(s.parse::<Hash256>().unwrap(), h);
    }
}
