//! チェッカー（10 章 §5.4 の 3）。5ms ごとに不変条件の `check_live` を呼ぶ。

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::target::StressConn;
use super::{Invariant, Shared};

pub(super) const CHECK_INTERVAL: Duration = Duration::from_millis(5);

pub(super) fn run(
    shared: &Shared<'_>,
    invariants: &[Box<dyn Invariant>],
    conn: &mut dyn StressConn,
) {
    while !shared.done.load(Ordering::SeqCst) && !shared.stop.load(Ordering::SeqCst) {
        for inv in invariants {
            if let Err(observed) = inv.check_live(conn) {
                shared.fail(inv.name(), observed);
                return;
            }
        }
        shared.checks.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(CHECK_INTERVAL);
    }
}
