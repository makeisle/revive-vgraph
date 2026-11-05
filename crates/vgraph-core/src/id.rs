//! 안정 논리 ID (ULID) + IdGen DI (인터페이스 §1.1, 골든 TC §0.2).

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use ulid::Ulid;

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub Ulid);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
        impl FromStr for $name {
            type Err = ulid::DecodeError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok($name(Ulid::from_string(s)?))
            }
        }
    };
}

id_newtype!(Uuid);
id_newtype!(GraphId);
id_newtype!(WorkingId);

/// ULID 발급기. 프로덕션 = SystemIdGen, 테스트 = TestIdGen(순차·결정적).
pub trait IdGen: Send + Sync {
    fn next(&self) -> Ulid;
    fn next_uuid(&self) -> Uuid {
        Uuid(self.next())
    }
    fn next_graph(&self) -> GraphId {
        GraphId(self.next())
    }
    fn next_working(&self) -> WorkingId {
        WorkingId(self.next())
    }
}

pub struct SystemIdGen;
impl IdGen for SystemIdGen {
    fn next(&self) -> Ulid {
        Ulid::new()
    }
}

/// 골든 TC §0.2: `ulid(k)` = 26자 zero-pad 십진 문자열을 Crockford로 해석한 ULID.
/// (십진 숫자는 전부 유효한 Crockford 문자이므로 리터럴이 그대로 성립)
pub struct TestIdGen {
    counter: AtomicU64,
}

impl TestIdGen {
    /// 카운터 1부터 시작 — 픽스처 F0의 할당 순서(GA=1, WF=2, n1=3, …)와 일치.
    pub fn new() -> Self {
        Self::starting_at(1)
    }
    pub fn starting_at(k: u64) -> Self {
        TestIdGen {
            counter: AtomicU64::new(k),
        }
    }
    pub fn peek(&self) -> u64 {
        self.counter.load(Ordering::SeqCst)
    }
}

impl Default for TestIdGen {
    fn default() -> Self {
        Self::new()
    }
}

impl IdGen for TestIdGen {
    fn next(&self) -> Ulid {
        let k = self.counter.fetch_add(1, Ordering::SeqCst);
        Ulid::from_string(&format!("{k:026}")).expect("decimal string is valid crockford")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_idgen_literals() {
        let g = TestIdGen::new();
        assert_eq!(g.next().to_string(), "00000000000000000000000001");
        assert_eq!(g.next().to_string(), "00000000000000000000000002");
        let g10 = TestIdGen::starting_at(10);
        assert_eq!(g10.next().to_string(), "00000000000000000000000010");
    }

    #[test]
    fn uuid_roundtrip() {
        let u: Uuid = "00000000000000000000000003".parse().unwrap();
        assert_eq!(u.to_string(), "00000000000000000000000003");
    }
}
