//! 各スレッドの直近の操作の記録。失敗時に `target/stress-failure-<seed>.log` へ書く。

use std::collections::VecDeque;

/// 1 スレッドが覚えておく操作の数。
pub const RING_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
pub struct OpRecord {
    /// 実行開始からの経過ミリ秒。
    pub t_ms: u32,
    pub thread: u16,
    pub sql: String,
    pub outcome: String,
}

/// 直近 [`RING_CAPACITY`] 件を覚える。
#[derive(Debug, Default)]
pub struct OpRing {
    records: VecDeque<OpRecord>,
}

impl OpRing {
    pub fn new() -> Self {
        OpRing::default()
    }

    pub fn push(&mut self, rec: OpRecord) {
        if self.records.len() == RING_CAPACITY {
            self.records.pop_front();
        }
        self.records.push_back(rec);
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 古い順。
    pub fn records(&self) -> Vec<OpRecord> {
        self.records.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_last_256() {
        let mut ring = OpRing::new();
        assert!(ring.is_empty());
        for i in 0..300u32 {
            ring.push(OpRecord {
                t_ms: i,
                thread: 0,
                sql: format!("q{i}"),
                outcome: "ok".into(),
            });
        }
        assert_eq!(ring.len(), RING_CAPACITY);
        let r = ring.records();
        assert_eq!(r.first().unwrap().t_ms, 44);
        assert_eq!(r.last().unwrap().t_ms, 299);
    }
}
