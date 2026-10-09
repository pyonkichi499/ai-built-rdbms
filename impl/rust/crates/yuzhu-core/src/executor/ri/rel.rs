//! RI が文の途中で開く関係と、制約ごとの検索の計画（`m5/07` §4.4）。
//!
//! `RiEnv` は session が実装して `RiQueue::new` に渡す（executor は session を `use` しない）。

use std::sync::Arc;

use super::keys::RiKeyCmp;
use crate::catalog::TableDef;
use crate::catalog::fk::ForeignKeyDef;
use crate::error::Result;
use crate::planner::physical::{PhysCheck, PhysExpr};
use crate::storage::{IndexHandle, RelHandle};
use crate::txn::{CommandId, RegisteredSnapshot, Xid};
use crate::types::Oid;

/// RI が必要とするもの。
pub trait RiEnv: Send + Sync + std::fmt::Debug {
    /// リレーションロックを取った後に呼ぶ（FK-D17）。カタログ用スナップショットを取り直して最新の定義を読み、
    /// `RelHandle`・NOT NULL・CHECK・DEFAULT の実行用の式を作って返す。
    fn open_relation(&self, oid: Oid) -> Result<RiRel>;
    /// 最新のスナップショットを取る（登録される。Drop で外れる）。
    fn latest_snapshot(&self, own: Option<Xid>, curcid: CommandId) -> RegisteredSnapshot;
}

/// RI が開いた表。
#[derive(Debug)]
pub struct RiRel {
    pub def: Arc<TableDef>,
    pub rel: RelHandle,
    pub not_null: Vec<bool>,
    pub checks: Vec<PhysCheck>,
    /// 列ごとの DEFAULT（式を持たない列は `None`）。
    pub defaults: Vec<Option<PhysExpr>>,
}

/// 制約ごとの検索の計画。初めて使うときに作ってキューに覚える。
#[derive(Debug)]
pub struct FkPlan {
    pub fk: Arc<ForeignKeyDef>,
    pub child: Arc<RiRel>,
    pub parent: Arc<RiRel>,
    /// 列の組ごと。
    pub cmp: Vec<RiKeyCmp>,
    pub parent_index: IndexHandle,
    /// 親の索引の列位置 i に入れるプローブは、FK のキーの何番目か。
    pub parent_perm: Vec<usize>,
    /// 子で使える索引（FK-D12）とその並べ替え。`None` なら全件走査。
    pub child_index: Option<(IndexHandle, Vec<usize>)>,
}

/// `parent_index` の列順に対する FK のキーの並べ替え `perm` を求める。
///
/// `index_attnums` は索引のキー列（attnum、索引の列順）、`fk_cols` は FK の列（attnum、FK の列順）。
/// 索引の全列が FK の列に含まれ、かつ列数が同じときだけ `Some`（重複する FK 列は先に見つかった位置を使う）。
pub fn index_perm(index_attnums: &[i16], fk_cols: &[i16]) -> Option<Vec<usize>> {
    if index_attnums.len() != fk_cols.len() {
        return None;
    }
    let mut used = vec![false; fk_cols.len()];
    let mut perm = Vec::with_capacity(index_attnums.len());
    for a in index_attnums {
        let pos = fk_cols
            .iter()
            .enumerate()
            .position(|(i, c)| c == a && !used[i])?;
        used[pos] = true;
        perm.push(pos);
    }
    Some(perm)
}

/// 子の索引（FK-D12）: FK の列を先頭側に全部含む索引のうち、キー列数が最小のものの並べ替え。
/// 索引の先頭 `fk_cols.len()` 列が FK の列の並べ替えになっていれば使える。
pub fn prefix_perm(index_attnums: &[i16], fk_cols: &[i16]) -> Option<Vec<usize>> {
    if index_attnums.len() < fk_cols.len() || fk_cols.is_empty() {
        return None;
    }
    index_perm(&index_attnums[..fk_cols.len()], fk_cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perm_reorders_to_index_order() {
        // 索引 (b, a)、FK の列 (a, b) → 索引位置 0 には FK の 1 番目（b）が入る。
        assert_eq!(index_perm(&[2, 1], &[1, 2]), Some(vec![1, 0]));
        assert_eq!(index_perm(&[1, 2], &[1, 2]), Some(vec![0, 1]));
    }

    #[test]
    fn perm_rejects_mismatch() {
        assert_eq!(index_perm(&[1], &[1, 2]), None);
        assert_eq!(index_perm(&[1, 3], &[1, 2]), None);
        assert_eq!(index_perm(&[1, 1], &[1, 2]), None);
    }

    #[test]
    fn prefix_perm_allows_extra_trailing_columns() {
        assert_eq!(prefix_perm(&[2, 1, 9], &[1, 2]), Some(vec![1, 0]));
        assert_eq!(prefix_perm(&[1], &[1, 2]), None);
        assert_eq!(prefix_perm(&[3, 1, 2], &[1, 2]), None);
        assert_eq!(prefix_perm(&[1], &[]), None);
    }
}
