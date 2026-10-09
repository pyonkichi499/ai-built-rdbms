//! 更新件数のスレッドローカル集計（`m5/03` §5.9、VC-D20）。
//!
//! ホットパス（`HeapStore::delete` / `update`）は共有ロックを取らない。文の終わりに
//! セッションが [`drain`] で取り出し、共有の件数表（autovacuum の対象選択。VC-6）に足す。

use std::cell::RefCell;
use std::collections::HashMap;

use crate::storage::smgr::RelFileLocator;

thread_local! {
    static MODIFIED: RefCell<HashMap<RelFileLocator, u64>> = RefCell::new(HashMap::new());
}

/// このスレッドの `rel` の更新（UPDATE / DELETE した件数）に `n` を足す。
pub fn note_modified(rel: RelFileLocator, n: u32) {
    MODIFIED.with(|m| {
        *m.borrow_mut().entry(rel).or_insert(0) += u64::from(n);
    });
}

/// このスレッドの分を取り出して空にする。`RelFileLocator` の昇順。
pub fn drain() -> Vec<(RelFileLocator, u64)> {
    MODIFIED.with(|m| {
        let mut v: Vec<_> = m.borrow_mut().drain().collect();
        v.sort_by_key(|(rel, _)| *rel);
        v
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::smgr::RelFileNumber;

    fn rel(n: u32) -> RelFileLocator {
        RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(n),
        }
    }

    #[test]
    fn accumulates_per_relation_and_drain_empties() {
        let _ = drain();
        note_modified(rel(20), 1);
        note_modified(rel(10), 2);
        note_modified(rel(20), 4);
        assert_eq!(drain(), vec![(rel(10), 2), (rel(20), 5)]);
        assert!(drain().is_empty());
    }

    #[test]
    fn counts_are_per_thread() {
        let _ = drain();
        note_modified(rel(1), 3);
        let other = std::thread::spawn(|| {
            note_modified(rel(1), 7);
            drain()
        })
        .join()
        .unwrap();
        assert_eq!(other, vec![(rel(1), 7)]);
        assert_eq!(drain(), vec![(rel(1), 3)]);
    }

    #[test]
    fn large_counts_do_not_overflow_u32() {
        let _ = drain();
        note_modified(rel(1), u32::MAX);
        note_modified(rel(1), u32::MAX);
        assert_eq!(drain(), vec![(rel(1), 2 * u64::from(u32::MAX))]);
    }
}
