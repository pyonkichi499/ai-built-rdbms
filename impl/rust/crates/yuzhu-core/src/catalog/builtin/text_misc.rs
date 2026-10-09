//! 組み込み表の行: bpchar・bytea・uuid（TY が持つ）。bpchar の穴埋めは TY-3（`m5/05` §5.2）。
//!
//! `bpcharcmp` と比較演算子の実体（`bpchareq` ほか）を「呼べる関数」にする。`pg_proc` の行は
//! `core.rs` が持つ。

#![allow(clippy::wildcard_imports)]

use super::*;

const BPCHAR: Oid = oid::BPCHAR;

/// 呼べる関数（`FUNCTIONS` に結合される）。
pub static FUNCTIONS: &[BuiltinFunction] = &[
    func(
        1078,
        "bpcharcmp",
        &[BPCHAR, BPCHAR],
        oid::INT4,
        crate::types::bpchar::bpcharcmp,
    ),
    func(1048, "bpchareq", &[BPCHAR, BPCHAR], oid::BOOL, ops::cmp_eq),
    func(1049, "bpcharlt", &[BPCHAR, BPCHAR], oid::BOOL, ops::cmp_lt),
    func(1050, "bpcharle", &[BPCHAR, BPCHAR], oid::BOOL, ops::cmp_le),
    func(1051, "bpchargt", &[BPCHAR, BPCHAR], oid::BOOL, ops::cmp_gt),
    func(1052, "bpcharge", &[BPCHAR, BPCHAR], oid::BOOL, ops::cmp_ge),
    func(1053, "bpcharne", &[BPCHAR, BPCHAR], oid::BOOL, ops::cmp_ne),
];
