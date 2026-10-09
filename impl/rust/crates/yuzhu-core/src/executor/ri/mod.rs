//! 参照整合性（FK）。M5 のスタブで、持ち主の章（FK-2）が中身を埋める（07 §4.5）。
//!
//! F0b-2 は、`executor/dml.rs` が呼ぶフックの名前と署名を固定しただけ。FK の無い表では
//! 何もしないので、いま呼んでも結果は変わらない。
pub mod action;
pub mod check;
pub mod keys;
pub mod queue;
pub mod rel;

use super::ExecCtx;
use crate::error::Result;
use crate::storage::RelHandle;
use crate::types::{Datum, Tid};

/// 行の挿入の直後に呼ぶ。
pub fn after_insert(
    _ctx: &mut ExecCtx<'_>,
    _rel: &RelHandle,
    _tid: Tid,
    _new_row: &[Datum],
) -> Result<()> {
    Ok(())
}

/// 行の更新の直後に呼ぶ。
pub fn after_update(
    _ctx: &mut ExecCtx<'_>,
    _rel: &RelHandle,
    _old_tid: Tid,
    _old_row: &[Datum],
    _new_tid: Tid,
    _new_row: &[Datum],
) -> Result<()> {
    Ok(())
}

/// 行の削除の直後に呼ぶ。
pub fn after_delete(_ctx: &mut ExecCtx<'_>, _rel: &RelHandle, _old_row: &[Datum]) -> Result<()> {
    Ok(())
}

/// 文の終わりに 1 回呼ぶ。キューが空なら何もしない。
pub fn finish_statement(_ctx: &mut ExecCtx<'_>) -> Result<()> {
    Ok(())
}
