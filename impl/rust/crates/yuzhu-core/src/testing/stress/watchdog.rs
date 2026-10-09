//! ウォッチドッグ（10 章 §5.4 の 4）。一定時間コミットが 1 件も増えなければ失敗にする。
//!
//! デッドロック検出（LK-2）が壊れると全スレッドが待ちに入って止まる。そのとき `lock_status()` を
//! 失敗ログに残す。止まったスレッドは join できないので、停止要求のあと [`HANG_GRACE`] 経っても
//! ワーカーが戻らなければ、ログを書いたうえでプロセスを中断する（テストがぶら下がり続けるのを防ぐ）。

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::Shared;

/// 停止要求から、ワーカーが戻るまで待つ最長の時間。
pub const HANG_GRACE: Duration = Duration::from_secs(60);

pub(super) fn run(shared: &Shared<'_>, watchdog: Duration, on_hang: &dyn Fn()) {
    let tick = (watchdog / 10).clamp(Duration::from_millis(5), Duration::from_millis(100));
    let mut last = shared.committed.load(Ordering::SeqCst);
    let mut last_change = Instant::now();
    let mut fired_at: Option<Instant> = None;
    while !shared.done.load(Ordering::SeqCst) {
        std::thread::sleep(tick);
        if let Some(t) = fired_at {
            if t.elapsed() > HANG_GRACE {
                on_hang();
                eprintln!(
                    "stress: workers did not stop within {HANG_GRACE:?} of the watchdog; aborting the process"
                );
                std::process::abort();
            }
            continue;
        }
        if shared.stop.load(Ordering::SeqCst) {
            // ほかの失敗か時間切れで止めている最中。ワーカーが戻らない場合だけ見張る。
            fired_at = Some(Instant::now());
            continue;
        }
        let now = shared.committed.load(Ordering::SeqCst);
        if now != last {
            last = now;
            last_change = Instant::now();
        } else if last_change.elapsed() >= watchdog {
            shared.fail(
                "watchdog",
                format!(
                    "no transaction committed for {:?} (committed so far: {now})",
                    last_change.elapsed()
                ),
            );
            fired_at = Some(Instant::now());
        }
    }
}
