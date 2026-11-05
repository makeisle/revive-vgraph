//! Clock DI (인터페이스 §1.1). TestClock = t0 + 1s/tick (골든 TC §0.2).

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use std::sync::atomic::{AtomicI64, Ordering};

pub trait Clock: Send + Sync {
    /// RFC3339 (초 단위, Z) — commit ts에 사용.
    fn now(&self) -> String;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> String {
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    }
}

/// t0 = 2026-08-09T00:00:00Z, 호출마다 +1s (첫 호출 = t0+1s).
pub struct TestClock {
    base: DateTime<Utc>,
    tick: AtomicI64,
}

impl TestClock {
    pub fn new(base_rfc3339: &str) -> Self {
        TestClock {
            base: DateTime::parse_from_rfc3339(base_rfc3339)
                .expect("valid rfc3339")
                .with_timezone(&Utc),
            tick: AtomicI64::new(1),
        }
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new("2026-08-09T00:00:00Z")
    }
}

impl Clock for TestClock {
    fn now(&self) -> String {
        let n = self.tick.fetch_add(1, Ordering::SeqCst);
        (self.base + Duration::seconds(n)).to_rfc3339_opts(SecondsFormat::Secs, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_ticks() {
        let c = TestClock::default();
        assert_eq!(c.now(), "2026-08-09T00:00:01Z");
        assert_eq!(c.now(), "2026-08-09T00:00:02Z");
    }
}
