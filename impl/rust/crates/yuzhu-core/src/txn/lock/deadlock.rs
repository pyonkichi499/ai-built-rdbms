//! デッドロック検出（01 章 §5.4。LK-2）。
//!
//! `deadlock_timeout` 待っても付与されなかった待ち手が、`LockTable` の `Mutex` を持ったまま
//! 自分から出発して 1 回だけ [`find_cycle`] を呼ぶ。辺は `LockTable::blockers`
//! （hard: 保持者 → soft: 待ち行列で先にいる待ち手）の順に辿る。待ち行列の並べ替えはしない
//! （soft 辺を含む循環も 40P01。LK-D1）。被害者は検査した本人。
//!
//! 注: `wait.rs` の `acquire` は、統合段階で `super::deadlock::{find_cycle, deadlock_error}` に
//! 切り替えるまで暫定の同名関数を使っている。それまでこのモジュールの関数は本番ビルドでは未使用。

use std::collections::HashSet;

use super::table::{Hop, LockTable};
use crate::backend::BackendId;
use crate::error::{Error, sqlstate};

/// `start` から出発して `start` に戻る循環を探す。見つかったら、循環上の各待ち手
/// （待っているタグとモード）を `start` から順に返す。
/// 一度訪れたバックエンドは二度訪れない（PG の `visitedProcs`）。`start` を通らない循環は無視する。
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn find_cycle(t: &LockTable, start: BackendId) -> Option<Vec<Hop>> {
    let mut visited = HashSet::new();
    let mut path = Vec::new();
    dfs(t, start, start, &mut visited, &mut path).then_some(path)
}

fn dfs(
    t: &LockTable,
    cur: BackendId,
    start: BackendId,
    visited: &mut HashSet<BackendId>,
    path: &mut Vec<Hop>,
) -> bool {
    if !visited.insert(cur) {
        return false;
    }
    // 待っていなければ出る辺がない
    let Some(w) = t.waiting_not_granted(cur) else {
        return false;
    };
    path.push(Hop {
        waiter: cur,
        tag: w.tag,
        mode: w.mode,
    });
    for b in t.blockers(cur) {
        if b == start || dfs(t, b, start, visited, path) {
            return true;
        }
    }
    path.pop();
    false
}

/// `40P01 deadlock detected`。DETAIL は循環の各待ち手につき 1 行
/// （PG の `DeadLockReport` と同じ書式）。`cycle` は空でないこと。
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn deadlock_error(t: &LockTable, cycle: &[Hop]) -> Error {
    let detail = cycle
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let next = &cycle[(i + 1) % cycle.len()];
            format!(
                "Process {} waits for {} on {}; blocked by process {}.",
                t.pid_of(h.waiter),
                h.mode.pg_name(),
                h.tag.describe(),
                t.pid_of(next.waiter)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Error::new(sqlstate::DEADLOCK_DETECTED, "deadlock detected")
        .with_detail(detail)
        .with_hint("See server log for query details.")
}

#[cfg(test)]
#[path = "tests_deadlock.rs"]
mod tests_deadlock;
