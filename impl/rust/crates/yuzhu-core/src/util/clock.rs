//! 壁時計。`now()` 系・`statement_timestamp()`・`clock_timestamp()` はすべてここを通る
//! （`m5/06` §4.2、§5.2。`SystemTime::now()` を呼ぶのは `SystemClock` だけ）。

use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// PostgreSQL のエポック（2000-01-01 00:00:00 UTC）の UNIX 時刻（マイクロ秒）。
pub const PG_EPOCH_UNIX_MICROS: i64 = 946_684_800_000_000;

/// 壁時計。
pub trait Clock: Send + Sync + Debug {
    /// UNIX エポック（1970-01-01 00:00:00 UTC）からのマイクロ秒。
    fn unix_micros(&self) -> i64;

    /// PostgreSQL のエポック（2000-01-01 00:00:00 UTC）からのマイクロ秒。`timestamptz` の値そのもの。
    fn pg_micros(&self) -> i64 {
        self.unix_micros().saturating_sub(PG_EPOCH_UNIX_MICROS)
    }
}

/// OS の時計。時計が 1970 年より前なら 0。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_micros(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_micros()).ok())
            .unwrap_or(0)
    }
}

/// テスト用の時計。進めなければ止まっている。
#[derive(Debug)]
pub struct ManualClock {
    unix_micros: AtomicI64,
}

impl ManualClock {
    /// `unix_micros`（UNIX エポックからのマイクロ秒）で始まる時計を作る。
    pub fn new(unix_micros: i64) -> Arc<Self> {
        Arc::new(Self {
            unix_micros: AtomicI64::new(unix_micros),
        })
    }

    /// 時刻を設定する。
    pub fn set(&self, unix_micros: i64) {
        self.unix_micros.store(unix_micros, Ordering::SeqCst);
    }

    /// 時刻を `micros` マイクロ秒進める（負なら戻す）。
    pub fn advance(&self, micros: i64) {
        self.unix_micros.fetch_add(micros, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn unix_micros(&self) -> i64 {
        self.unix_micros.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_after_2020_and_monotonic_enough() {
        let c = SystemClock;
        // 2020-01-01 00:00:00 UTC
        assert!(c.unix_micros() > 1_577_836_800_000_000);
        let (a, b) = (c.unix_micros(), c.pg_micros());
        assert!(b >= a - PG_EPOCH_UNIX_MICROS && b - (a - PG_EPOCH_UNIX_MICROS) < 1_000_000);
        assert!(c.pg_micros() > 0);
    }

    #[test]
    fn manual_clock_stands_still_until_advanced() {
        let c = ManualClock::new(PG_EPOCH_UNIX_MICROS + 5);
        assert_eq!(c.pg_micros(), 5);
        assert_eq!(c.pg_micros(), 5);
        c.advance(1_000_000);
        assert_eq!(c.pg_micros(), 1_000_005);
        c.set(0);
        assert_eq!(c.unix_micros(), 0);
        assert_eq!(c.pg_micros(), -PG_EPOCH_UNIX_MICROS);
    }

    #[test]
    fn clock_is_usable_as_trait_object() {
        let c: Arc<dyn Clock> = ManualClock::new(PG_EPOCH_UNIX_MICROS);
        assert_eq!(c.pg_micros(), 0);
    }
}
