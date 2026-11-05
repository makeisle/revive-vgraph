//! Value 타입 (설계 §2.1). canonical 표기는 canon.rs (결정 D4).
//!
//! Float: NaN/∞ 금지 (canonical 시점에 검증). Ts/Decimal/Ref는 문자열 보관.

use smol_str::SmolStr;
use std::collections::BTreeMap;

pub type Key = SmolStr;
/// BTreeMap = 키 정렬이 자료구조에서 보장 → canonical 직렬화 결정성.
pub type Props = BTreeMap<Key, Value>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// 정밀 십진 — canonical은 문자열 그대로 (수치 해석 안 함).
    Decimal(SmolStr),
    Str(String),
    Bytes(Vec<u8>),
    /// RFC3339 문자열.
    Ts(SmolStr),
    List(Vec<Value>),
    Map(BTreeMap<Key, Value>),
    /// external link 주소 문자열 (해석은 엔진 소관 — 설계 §7).
    Ref(SmolStr),
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}
impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Int(i as i64)
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

/// 픽스처/테스트용 Props 구성 헬퍼.
pub fn props<const N: usize>(kv: [(&str, Value); N]) -> Props {
    kv.into_iter().map(|(k, v)| (SmolStr::new(k), v)).collect()
}
