//! `HeapScan`: scan state that holds neither pins nor latches
//! (`m2.md` §4.4, §6.5.4).
//!
//! 担当 D が実装する。フィールドは非公開（不透明な型）。

use std::collections::VecDeque;

use crate::storage::{HeapTuple, RelHandle};
use crate::txn::Snapshot;

#[derive(Debug)]
pub struct HeapScan {
    #[allow(dead_code)]
    rel: RelHandle,
    #[allow(dead_code)]
    snapshot: Snapshot,
    /// Fixed when the scan begins; blocks added later are not read.
    #[allow(dead_code)]
    nblocks: u32,
    #[allow(dead_code)]
    next_block: u32,
    /// Visible tuples of the current block, already decoded.
    #[allow(dead_code)]
    buf: VecDeque<HeapTuple>,
}

impl HeapScan {
    /// The scan before the first block. `nblocks` is the block count at the
    /// time the scan begins.
    pub fn new(rel: RelHandle, snapshot: Snapshot, nblocks: u32) -> HeapScan {
        HeapScan {
            rel,
            snapshot,
            nblocks,
            next_block: 0,
            buf: VecDeque::new(),
        }
    }
}

#[cfg(test)]
impl HeapScan {
    /// A scan preloaded with `tuples` (no blocks to read), for fake
    /// `TableStore`s in executor and planner unit tests.
    pub(crate) fn from_tuples(rel: RelHandle, snapshot: Snapshot, tuples: Vec<HeapTuple>) -> Self {
        let mut s = HeapScan::new(rel, snapshot, 0);
        s.buf = tuples.into();
        s
    }

    /// Takes the next preloaded tuple.
    pub(crate) fn pop_buffered(&mut self) -> Option<HeapTuple> {
        self.buf.pop_front()
    }
}
