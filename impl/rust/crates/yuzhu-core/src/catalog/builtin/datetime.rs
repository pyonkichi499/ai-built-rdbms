//! 組み込み表の行: 日時（interval / time を含む。TD が持つ）。
//!
//! TD-1 は interval の行をここに置く。`mod.rs` / `core.rs` の表（`OPERATORS` など）は `&'static [T]` の
//! 静的な表で、このファイルの表を結合する口は F1 / 統合が足す（`m5/06` §11）。結合するときは次を守る。
//!
//! * `INTERVAL_OPERATORS` は `core.rs` にある interval の行（OID 1330〜1338、1583〜1585。関数が
//!   `ops::unsupported`）を**置き換える**。`core.rs` 側の行を消してからこの表を足すこと（OID の重複になる）。
//! * `INTERVAL_PROCS` は `core.rs` の `PROCS` に無い行だけ（既存の 1160〜1170、1326、1618、1624 は `core.rs`）。
//! * `timestamptz ± interval`（OID 1327・1329・2554）は、セッションの `TimeZone` が要る（stable）。
//!   `BuiltinOperator.func` は純粋な関数なので `needs_env`（呼ぶと内部エラー）を置いた。実行は
//!   `INTERVAL_FUNCTIONS` の `FnKind::Env` の行（OID 1189・1190・2549。`OperatorMeta.proc_oid` と同じ）で行う。
//!   演算子の呼び出しを「proc_oid の `FnKind::Env` 関数があればそちらを `TypeEnv` つきで呼ぶ」に
//!   するのは executor / analyzer の持ち主の仕事。
//! * `sum(interval)` / `avg(interval)` は `AggKind` に変種（と `AggState`）が要る。
//!   本体は `types::interval::{IntervalSum, IntervalAvg}`。`min` / `max` は `AggKind::Min` / `Max`。

#![allow(clippy::wildcard_imports)]
// 表の結合（mod.rs / core.rs）が済むまでは参照されない。
#![allow(dead_code)]

use super::*;
use crate::types::datetime::fns as dtf;
use crate::types::interval as iv;

/// interval に関わる演算子（`pg_operator`）。
#[rustfmt::skip]
pub static INTERVAL_OPERATORS: &[BuiltinOperator] = &[
    op(1330, "=", INTERVAL, INTERVAL, oid::BOOL, iv::interval_eq),
    op(1331, "<>", INTERVAL, INTERVAL, oid::BOOL, iv::interval_ne),
    op(1332, "<", INTERVAL, INTERVAL, oid::BOOL, iv::interval_lt),
    op(1333, "<=", INTERVAL, INTERVAL, oid::BOOL, iv::interval_le),
    op(1334, ">", INTERVAL, INTERVAL, oid::BOOL, iv::interval_gt),
    op(1335, ">=", INTERVAL, INTERVAL, oid::BOOL, iv::interval_ge),
    op(1336, "-", 0, INTERVAL, INTERVAL, iv::interval_um),
    op(1337, "+", INTERVAL, INTERVAL, INTERVAL, iv::interval_pl),
    op(1338, "-", INTERVAL, INTERVAL, INTERVAL, iv::interval_mi),
    op(1583, "*", INTERVAL, oid::FLOAT8, INTERVAL, iv::interval_mul),
    op(1584, "*", oid::FLOAT8, INTERVAL, INTERVAL, iv::mul_d_interval),
    op(1585, "/", INTERVAL, oid::FLOAT8, INTERVAL, iv::interval_div),
    op(1076, "+", DATE, INTERVAL, oid::TIMESTAMP, iv::date_pl_interval),
    op(1077, "-", DATE, INTERVAL, oid::TIMESTAMP, iv::date_mi_interval),
    op(2551, "+", INTERVAL, DATE, oid::TIMESTAMP, iv::interval_pl_date),
    op(2066, "+", oid::TIMESTAMP, INTERVAL, oid::TIMESTAMP, iv::timestamp_pl_interval),
    op(2068, "-", oid::TIMESTAMP, INTERVAL, oid::TIMESTAMP, iv::timestamp_mi_interval),
    op(2553, "+", INTERVAL, oid::TIMESTAMP, oid::TIMESTAMP, iv::interval_pl_timestamp),
    op(2067, "-", oid::TIMESTAMP, oid::TIMESTAMP, INTERVAL, iv::timestamp_mi),
    op(1328, "-", oid::TIMESTAMPTZ, oid::TIMESTAMPTZ, INTERVAL, iv::timestamptz_mi),
    op(1327, "+", oid::TIMESTAMPTZ, INTERVAL, oid::TIMESTAMPTZ, iv::needs_env),
    op(1329, "-", oid::TIMESTAMPTZ, INTERVAL, oid::TIMESTAMPTZ, iv::needs_env),
    op(2554, "+", INTERVAL, oid::TIMESTAMPTZ, oid::TIMESTAMPTZ, iv::needs_env),
];

/// `INTERVAL_OPERATORS` のうち、`core.rs` の `OPERATOR_META` に無い行（1330〜1338、1583〜1585 は既存）。
#[rustfmt::skip]
pub static INTERVAL_OPERATOR_META: &[OperatorMeta] = &[
    OperatorMeta { oid: 1076, proc_oid: 2071, com: 2551, negate: 0 },
    OperatorMeta { oid: 1077, proc_oid: 2072, com: 0, negate: 0 },
    OperatorMeta { oid: 1327, proc_oid: 1189, com: 2554, negate: 0 },
    OperatorMeta { oid: 1328, proc_oid: 1188, com: 0, negate: 0 },
    OperatorMeta { oid: 1329, proc_oid: 1190, com: 0, negate: 0 },
    OperatorMeta { oid: 2066, proc_oid: 2032, com: 2553, negate: 0 },
    OperatorMeta { oid: 2067, proc_oid: 2031, com: 0, negate: 0 },
    OperatorMeta { oid: 2068, proc_oid: 2033, com: 0, negate: 0 },
    OperatorMeta { oid: 2551, proc_oid: 2546, com: 1076, negate: 0 },
    OperatorMeta { oid: 2553, proc_oid: 2548, com: 2066, negate: 0 },
    OperatorMeta { oid: 2554, proc_oid: 2549, com: 1327, negate: 0 },
];

/// 利用者が呼べる interval の関数。`FnKind::Env` の 3 行（1189・1190・2549）は演算子の本体でもある。
#[rustfmt::skip]
pub static INTERVAL_FUNCTIONS: &[BuiltinFunction] = &[
    func(1175, "justify_hours", &[INTERVAL], INTERVAL, iv::interval_justify_hours),
    func(1295, "justify_days", &[INTERVAL], INTERVAL, iv::interval_justify_days),
    func(2711, "justify_interval", &[INTERVAL], INTERVAL, iv::interval_justify_interval),
    func(1390, "isfinite", &[INTERVAL], oid::BOOL, iv::interval_finite),
    func(1200, "interval", &[INTERVAL, oid::INT4], INTERVAL, iv::interval_scale),
    env_func(1189, "timestamptz_pl_interval", &[oid::TIMESTAMPTZ, INTERVAL], oid::TIMESTAMPTZ, iv::timestamptz_pl_interval),
    env_func(1190, "timestamptz_mi_interval", &[oid::TIMESTAMPTZ, INTERVAL], oid::TIMESTAMPTZ, iv::timestamptz_mi_interval),
    env_func(2549, "interval_pl_timestamptz", &[INTERVAL, oid::TIMESTAMPTZ], oid::TIMESTAMPTZ, iv::interval_pl_timestamptz),
];

/// `FnKind::Env` の strict な関数。
const fn env_func(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    f: fn(&[Datum], &crate::types::TypeEnv<'_>) -> Result<Datum>,
) -> BuiltinFunction {
    BuiltinFunction {
        oid,
        name,
        args,
        result,
        strict: true,
        kind: FnKind::Env(f),
    }
}

/// `INTERVAL_OPERATORS` と `INTERVAL_FUNCTIONS` が指す `pg_proc` の行で `core.rs` の `PROCS` に無いもの。
#[rustfmt::skip]
pub static INTERVAL_PROCS: &[BuiltinProc] = &[
    BuiltinProc { oid: 1175, name: "justify_hours", args: &[1186], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_justify_hours" },
    BuiltinProc { oid: 1188, name: "timestamptz_mi", args: &[1184, 1184], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamp_mi" },
    BuiltinProc { oid: 1189, name: "timestamptz_pl_interval", args: &[1184, 1186], result: 1184, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamptz_pl_interval" },
    BuiltinProc { oid: 1190, name: "timestamptz_mi_interval", args: &[1184, 1186], result: 1184, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamptz_mi_interval" },
    BuiltinProc { oid: 1197, name: "interval_smaller", args: &[1186, 1186], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_smaller" },
    BuiltinProc { oid: 1198, name: "interval_larger", args: &[1186, 1186], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_larger" },
    BuiltinProc { oid: 1200, name: "interval", args: &[1186, 23], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_scale" },
    BuiltinProc { oid: 1295, name: "justify_days", args: &[1186], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_justify_days" },
    BuiltinProc { oid: 1315, name: "interval_cmp", args: &[1186, 1186], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_cmp" },
    BuiltinProc { oid: 1390, name: "isfinite", args: &[1186], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_finite" },
    BuiltinProc { oid: 2031, name: "timestamp_mi", args: &[1114, 1114], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamp_mi" },
    BuiltinProc { oid: 2032, name: "timestamp_pl_interval", args: &[1114, 1186], result: 1114, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamp_pl_interval" },
    BuiltinProc { oid: 2033, name: "timestamp_mi_interval", args: &[1114, 1186], result: 1114, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamp_mi_interval" },
    BuiltinProc { oid: 2071, name: "date_pl_interval", args: &[1082, 1186], result: 1114, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "date_pl_interval" },
    BuiltinProc { oid: 2072, name: "date_mi_interval", args: &[1082, 1186], result: 1114, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "date_mi_interval" },
    BuiltinProc { oid: 2546, name: "interval_pl_date", args: &[1186, 1082], result: 1114, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "select ($2 + $1)" },
    BuiltinProc { oid: 2548, name: "interval_pl_timestamp", args: &[1186, 1114], result: 1114, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "select ($2 + $1)" },
    BuiltinProc { oid: 2549, name: "interval_pl_timestamptz", args: &[1186, 1184], result: 1184, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "select ($2 + $1)" },
    BuiltinProc { oid: 2711, name: "justify_interval", args: &[1186], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_justify_interval" },
];

/// `min(interval)`（2144）と `max(interval)`（2128）。汎用の `Min` / `Max` が `cmp_datum` で動く。
/// `sum(interval)`（2113）と `avg(interval)`（2106）は `AggKind` の変種が要る（ファイル先頭の注）。
#[rustfmt::skip]
pub static INTERVAL_AGGREGATES: &[BuiltinAggregate] = &[
    agg(2144, "min", &[INTERVAL], INTERVAL, AggKind::Min),
    agg(2128, "max", &[INTERVAL], INTERVAL, AggKind::Max),
];

// ===== TD-4: time の行（`m5/06` §3.1、§6.3、§6.5）=================================================
//
// 結合は mod.rs / type_io.rs の仕事（TD-4 は範囲外なので足していない）:
// * `TYPES` に `TIME_TYPES`、`PROCS` に `TIME_PROCS`、`OPERATORS` に `TIME_OPERATORS`、`OPERATOR_META` に
//   `TIME_OPERATOR_META`、`FUNCTIONS` に `TIME_FUNCTIONS`、`CASTS` に `TIME_CASTS`、`AGGREGATES` に
//   `TIME_AGGREGATES` を足す。OID は core.rs・INTERVAL_*・TD3_* と重ならない（`time_rows_do_not_overlap` が確かめる）。
// * `date + interval` などの interval 側の演算子は `INTERVAL_OPERATORS`。ここは time が絡むものだけ。
// * `type_io.rs` の `TYPE_PROCS` に `tp(1083, ("time_recv", 2470), ("time_send", 2471), ("timetypmodin", 2909),
//   ("timetypmodout", 2910), ("", 0))` を足す（`pg_proc` の行は TD-5 / TD-1 の持ち物）。
// * `date_part(text, time)`（1385）と `extract(text, time)`（6200）は `TIME_FUNCTIONS` にある。
// * `timetz`（1266）の行・キャスト・演算子は作らない（型名の解決が 0A000）。
// * `localtime` は `now()::time`（パーサ / アナライザの仕事）。

use crate::types::time as tm;

/// `pg_type` の `time`（1083）。配列型 `_time`（1183）の行は 05 章の配列の仕事。
#[rustfmt::skip]
pub static TIME_TYPES: &[BuiltinType] = &[
    ty(1083, "time", 8, true, 'b', 'D', false, ',', 0, 0, 1183, ("time_in", 1143), ("time_out", 1144), 'd', 'p', 0),
];

const TIME: Oid = oid::TIME;

/// time が絡む演算子（`pg_operator`）。
#[rustfmt::skip]
pub static TIME_OPERATORS: &[BuiltinOperator] = &[
    op(1108, "=", TIME, TIME, oid::BOOL, tm::time_eq),
    op(1109, "<>", TIME, TIME, oid::BOOL, tm::time_ne),
    op(1110, "<", TIME, TIME, oid::BOOL, tm::time_lt),
    op(1111, "<=", TIME, TIME, oid::BOOL, tm::time_le),
    op(1112, ">", TIME, TIME, oid::BOOL, tm::time_gt),
    op(1113, ">=", TIME, TIME, oid::BOOL, tm::time_ge),
    op(1399, "-", TIME, TIME, INTERVAL, tm::time_mi_time),
    op(1800, "+", TIME, INTERVAL, TIME, tm::time_pl_interval),
    op(1801, "-", TIME, INTERVAL, TIME, tm::time_mi_interval),
    op(1849, "+", INTERVAL, TIME, TIME, tm::interval_pl_time),
    op(1360, "+", DATE, TIME, oid::TIMESTAMP, tm::datetime_pl),
    op(1363, "+", TIME, DATE, oid::TIMESTAMP, tm::timedate_pl),
];

/// `TIME_OPERATORS` の `OperatorMeta`（`oprcode`、`oprcom`、`oprnegate`）。
#[rustfmt::skip]
pub static TIME_OPERATOR_META: &[OperatorMeta] = &[
    OperatorMeta { oid: 1108, proc_oid: 1145, com: 1108, negate: 1109 },
    OperatorMeta { oid: 1109, proc_oid: 1106, com: 1109, negate: 1108 },
    OperatorMeta { oid: 1110, proc_oid: 1102, com: 1112, negate: 1113 },
    OperatorMeta { oid: 1111, proc_oid: 1103, com: 1113, negate: 1112 },
    OperatorMeta { oid: 1112, proc_oid: 1104, com: 1110, negate: 1111 },
    OperatorMeta { oid: 1113, proc_oid: 1105, com: 1111, negate: 1110 },
    OperatorMeta { oid: 1399, proc_oid: 1690, com: 0, negate: 0 },
    OperatorMeta { oid: 1800, proc_oid: 1747, com: 1849, negate: 0 },
    OperatorMeta { oid: 1801, proc_oid: 1748, com: 0, negate: 0 },
    OperatorMeta { oid: 1849, proc_oid: 1848, com: 1800, negate: 0 },
    OperatorMeta { oid: 1360, proc_oid: 1272, com: 1363, negate: 0 },
    OperatorMeta { oid: 1363, proc_oid: 1296, com: 1360, negate: 0 },
];

/// 利用者が呼べる time の関数。`FnKind::Env` の行はセッションの `TimeZone` が要る（stable）。
#[rustfmt::skip]
pub static TIME_FUNCTIONS: &[BuiltinFunction] = &[
    func(1968, "time", &[TIME, oid::INT4], TIME, tm::time_scale),
    func(1370, "interval", &[TIME], INTERVAL, tm::time_to_interval),
    func(1419, "time", &[INTERVAL], TIME, tm::interval_to_time),
    func(1316, "time", &[oid::TIMESTAMP], TIME, tm::timestamp_to_time),
    env_func(2019, "time", &[oid::TIMESTAMPTZ], TIME, tm::timestamptz_to_time),
    func(2025, "timestamp", &[DATE, TIME], oid::TIMESTAMP, tm::timestamp_date_time),
    env_func(1176, "timestamptz", &[DATE, TIME], oid::TIMESTAMPTZ, tm::timestamptz_date_time),
    func(1377, "time_larger", &[TIME, TIME], TIME, tm::time_larger),
    func(1378, "time_smaller", &[TIME, TIME], TIME, tm::time_smaller),
    func(1107, "time_cmp", &[TIME, TIME], oid::INT4, tm::time_cmp),
    func(1385, "date_part", &[oid::TEXT, TIME], oid::FLOAT8, tm::date_part_time),
    func(6200, "extract", &[oid::TEXT, TIME], oid::NUMERIC, tm::extract_time),
];

/// `pg_cast`（`time → timetz` と `timetz → time` は作らない）。`interval → time` と
/// `timestamp / timestamptz → time` は代入、`time → interval` と `time → time` は暗黙。
/// `text ↔ time` は I/O キャスト（TY-1 の枠組みが生成する）。
#[rustfmt::skip]
pub static TIME_CASTS: &[BuiltinCast] = &[
    cast_fn(TIME, TIME, CastContext::Implicit, tm::time_scale, 1968),
    cast_fn(TIME, INTERVAL, CastContext::Implicit, tm::time_to_interval, 1370),
    cast_fn(INTERVAL, TIME, CastContext::Assignment, tm::interval_to_time, 1419),
    cast_fn(oid::TIMESTAMP, TIME, CastContext::Assignment, tm::timestamp_to_time, 1316),
    cast(oid::TIMESTAMPTZ, TIME, CastContext::Assignment, CastMethod::Env(tm::timestamptz_to_time), 2019),
];

/// `min(time)`（2139）と `max(time)`（2123）。汎用の `Min` / `Max` が `cmp_datum` で動く。
#[rustfmt::skip]
pub static TIME_AGGREGATES: &[BuiltinAggregate] = &[
    agg(2139, "min", &[TIME], TIME, AggKind::Min),
    agg(2123, "max", &[TIME], TIME, AggKind::Max),
];

const fn time_proc(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    volatility: char,
    leakproof: bool,
    prosrc: &'static str,
) -> BuiltinProc {
    BuiltinProc {
        oid,
        name,
        args,
        result,
        strict: true,
        volatility,
        parallel: 's',
        leakproof,
        cost: 1.0,
        prosrc,
    }
}

/// `TIME_*` が指す `pg_proc` の行。`min` / `max` の行（2139・2123）は strict でない（PostgreSQL は `proisstrict = f`
/// だが集約の行は `AGGREGATES` から作られる）ので、ここには入れない。`time_hash`（1688）と
/// `time_hash_extended`（3409）、`in_range`（4137）、`overlaps`（1308〜1311）は作らない。
#[rustfmt::skip]
pub static TIME_PROCS: &[BuiltinProc] = &[
    time_proc(1143, "time_in", &[2275, 26, 23], TIME, 's', false, "time_in"),
    time_proc(1144, "time_out", &[TIME], 2275, 'i', false, "time_out"),
    time_proc(1145, "time_eq", &[TIME, TIME], oid::BOOL, 'i', true, "time_eq"),
    time_proc(1106, "time_ne", &[TIME, TIME], oid::BOOL, 'i', true, "time_ne"),
    time_proc(1102, "time_lt", &[TIME, TIME], oid::BOOL, 'i', true, "time_lt"),
    time_proc(1103, "time_le", &[TIME, TIME], oid::BOOL, 'i', true, "time_le"),
    time_proc(1104, "time_gt", &[TIME, TIME], oid::BOOL, 'i', true, "time_gt"),
    time_proc(1105, "time_ge", &[TIME, TIME], oid::BOOL, 'i', true, "time_ge"),
    time_proc(1107, "time_cmp", &[TIME, TIME], oid::INT4, 'i', true, "time_cmp"),
    time_proc(1377, "time_larger", &[TIME, TIME], TIME, 'i', false, "time_larger"),
    time_proc(1378, "time_smaller", &[TIME, TIME], TIME, 'i', false, "time_smaller"),
    time_proc(1690, "time_mi_time", &[TIME, TIME], INTERVAL, 'i', false, "time_mi_time"),
    time_proc(1747, "time_pl_interval", &[TIME, INTERVAL], TIME, 'i', false, "time_pl_interval"),
    time_proc(1748, "time_mi_interval", &[TIME, INTERVAL], TIME, 'i', false, "time_mi_interval"),
    time_proc(1848, "interval_pl_time", &[INTERVAL, TIME], TIME, 'i', false, "select ($2 + $1)"),
    time_proc(1272, "datetime_pl", &[DATE, TIME], oid::TIMESTAMP, 'i', false, "datetime_timestamp"),
    time_proc(1296, "timedate_pl", &[TIME, DATE], oid::TIMESTAMP, 'i', false, "select ($2 + $1)"),
    time_proc(1968, "time", &[TIME, oid::INT4], TIME, 'i', false, "time_scale"),
    time_proc(1370, "interval", &[TIME], INTERVAL, 'i', true, "time_interval"),
    time_proc(1419, "time", &[INTERVAL], TIME, 'i', false, "interval_time"),
    time_proc(1316, "time", &[oid::TIMESTAMP], TIME, 'i', false, "timestamp_time"),
    time_proc(2019, "time", &[oid::TIMESTAMPTZ], TIME, 's', false, "timestamptz_time"),
    time_proc(2025, "timestamp", &[DATE, TIME], oid::TIMESTAMP, 'i', false, "datetime_timestamp"),
    time_proc(1176, "timestamptz", &[DATE, TIME], oid::TIMESTAMPTZ, 's', false, "select cast(cast($1 as timestamp without time zone) + cast($2 as time without time zone) as timestamp with time zone)"),
    time_proc(1385, "date_part", &[oid::TEXT, TIME], oid::FLOAT8, 'i', false, "time_part"),
    time_proc(6200, "extract", &[oid::TEXT, TIME], oid::NUMERIC, 'i', false, "extract_time"),
];

// ===== TD-3: 日時の関数の行（`m5/06` §6.5.3）=====================================================
//
// 結合は mod.rs の仕事: `FUNCTIONS_CELL` に `datetime::TD3_FUNCTIONS`、`PROCS_CELL` に `datetime::TD3_PROCS`
// を足す（OID は core.rs・INTERVAL_* と重ならない）。`isfinite(interval)`・`justify_*` は
// `INTERVAL_FUNCTIONS`。`time` の `date_part` / `extract`（1385・6200）は TD-4。`generate_series(timestamptz)`
// （939）は `SetFn` が `TypeEnv` を受け取れないので未登録。

const TEXT: Oid = oid::TEXT;
const F8: Oid = oid::FLOAT8;
const TS: Oid = oid::TIMESTAMP;
const TSTZ: Oid = oid::TIMESTAMPTZ;
const IV: Oid = oid::INTERVAL;
const NUM: Oid = oid::NUMERIC;
const I4: Oid = oid::INT4;

/// `0A000` の行（`to_char` / `to_date` / `to_timestamp(text, text)`。TD-D11）。
fn unsupported_formatting(_args: &[Datum]) -> Result<Datum> {
    Err(crate::error::Error::not_supported(
        "date/time formatting functions (to_char, to_date, to_timestamp) are not supported yet",
    ))
}

#[rustfmt::skip]
pub static TD3_FUNCTIONS: &[BuiltinFunction] = &[
    // date_part / extract
    env_func(1171, "date_part", &[TEXT, TSTZ], F8, dtf::date_part_timestamptz),
    func(1172, "date_part", &[TEXT, IV], F8, dtf::date_part_interval),
    func(1384, "date_part", &[TEXT, oid::DATE], F8, dtf::date_part_date),
    func(2021, "date_part", &[TEXT, TS], F8, dtf::date_part_timestamp),
    func(6199, "extract", &[TEXT, oid::DATE], NUM, dtf::extract_date),
    func(6202, "extract", &[TEXT, TS], NUM, dtf::extract_timestamp),
    env_func(6203, "extract", &[TEXT, TSTZ], NUM, dtf::extract_timestamptz),
    func(6204, "extract", &[TEXT, IV], NUM, dtf::extract_interval),
    // date_trunc / date_bin
    func(2020, "date_trunc", &[TEXT, TS], TS, dtf::date_trunc_timestamp),
    env_func(1217, "date_trunc", &[TEXT, TSTZ], TSTZ, dtf::date_trunc_timestamptz),
    env_func(1284, "date_trunc", &[TEXT, TSTZ, TEXT], TSTZ, dtf::date_trunc_timestamptz_zone),
    func(1218, "date_trunc", &[TEXT, IV], IV, dtf::date_trunc_interval),
    func(6177, "date_bin", &[IV, TS, TS], TS, dtf::date_bin_timestamp),
    func(6178, "date_bin", &[IV, TSTZ, TSTZ], TSTZ, dtf::date_bin_timestamptz),
    // date_add / date_subtract
    env_func(6221, "date_add", &[TSTZ, IV], TSTZ, dtf::date_add),
    env_func(6222, "date_add", &[TSTZ, IV, TEXT], TSTZ, dtf::date_add_zone),
    env_func(6223, "date_subtract", &[TSTZ, IV], TSTZ, dtf::date_subtract),
    env_func(6273, "date_subtract", &[TSTZ, IV, TEXT], TSTZ, dtf::date_subtract_zone),
    // age
    env_func(1199, "age", &[TSTZ, TSTZ], IV, dtf::age_timestamptz),
    env_func(1386, "age", &[TSTZ], IV, dtf::age_timestamptz_1),
    func(2058, "age", &[TS, TS], IV, dtf::age_timestamp),
    env_func(2059, "age", &[TS], IV, dtf::age_timestamp_1),
    // make_* / to_timestamp(float8)
    func(3846, "make_date", &[I4, I4, I4], oid::DATE, dtf::make_date),
    func(3847, "make_time", &[I4, I4, F8], oid::TIME, dtf::make_time),
    func(3461, "make_timestamp", &[I4, I4, I4, I4, I4, F8], TS, dtf::make_timestamp),
    env_func(3462, "make_timestamptz", &[I4, I4, I4, I4, I4, F8], TSTZ, dtf::make_timestamptz),
    env_func(3463, "make_timestamptz", &[I4, I4, I4, I4, I4, F8, TEXT], TSTZ, dtf::make_timestamptz_zone),
    func(3464, "make_interval", &[I4, I4, I4, I4, I4, I4, F8], IV, dtf::make_interval),
    func(1158, "to_timestamp", &[F8], TSTZ, dtf::to_timestamp_float),
    // isfinite
    func(1373, "isfinite", &[oid::DATE], oid::BOOL, dtf::isfinite_date),
    func(1389, "isfinite", &[TSTZ], oid::BOOL, dtf::isfinite_timestamptz),
    func(2048, "isfinite", &[TS], oid::BOOL, dtf::isfinite_timestamp),
    // generate_series(timestamp, timestamp, interval)
    generate_series_fn(938, &[TS, TS, IV], TS, dtf::generate_series_timestamp),
    // 0A000
    func(1768, "to_char", &[IV, TEXT], TEXT, unsupported_formatting),
    func(1770, "to_char", &[TSTZ, TEXT], TEXT, unsupported_formatting),
    func(2049, "to_char", &[TS, TEXT], TEXT, unsupported_formatting),
    func(1780, "to_date", &[TEXT, TEXT], oid::DATE, unsupported_formatting),
    func(1778, "to_timestamp", &[TEXT, TEXT], TSTZ, unsupported_formatting),
];

const fn td3_proc(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    volatility: char,
    prosrc: &'static str,
) -> BuiltinProc {
    BuiltinProc {
        oid,
        name,
        args,
        result,
        strict: true,
        volatility,
        parallel: 's',
        leakproof: false,
        cost: 1.0,
        prosrc,
    }
}

#[rustfmt::skip]
pub static TD3_PROCS: &[BuiltinProc] = &[
    td3_proc(1171, "date_part", &[TEXT, TSTZ], F8, 's', "timestamptz_part"),
    td3_proc(1172, "date_part", &[TEXT, IV], F8, 'i', "interval_part"),
    td3_proc(1384, "date_part", &[TEXT, oid::DATE], F8, 'i', "select pg_catalog.date_part($1, cast($2 as timestamp without time zone))"),
    td3_proc(2021, "date_part", &[TEXT, TS], F8, 'i', "timestamp_part"),
    td3_proc(6199, "extract", &[TEXT, oid::DATE], NUM, 'i', "extract_date"),
    td3_proc(6202, "extract", &[TEXT, TS], NUM, 'i', "extract_timestamp"),
    td3_proc(6203, "extract", &[TEXT, TSTZ], NUM, 's', "extract_timestamptz"),
    td3_proc(6204, "extract", &[TEXT, IV], NUM, 'i', "extract_interval"),
    td3_proc(2020, "date_trunc", &[TEXT, TS], TS, 'i', "timestamp_trunc"),
    td3_proc(1217, "date_trunc", &[TEXT, TSTZ], TSTZ, 's', "timestamptz_trunc"),
    td3_proc(1284, "date_trunc", &[TEXT, TSTZ, TEXT], TSTZ, 'i', "timestamptz_trunc_zone"),
    td3_proc(1218, "date_trunc", &[TEXT, IV], IV, 'i', "interval_trunc"),
    td3_proc(6177, "date_bin", &[IV, TS, TS], TS, 'i', "timestamp_bin"),
    td3_proc(6178, "date_bin", &[IV, TSTZ, TSTZ], TSTZ, 'i', "timestamptz_bin"),
    td3_proc(6221, "date_add", &[TSTZ, IV], TSTZ, 's', "timestamptz_pl_interval"),
    td3_proc(6222, "date_add", &[TSTZ, IV, TEXT], TSTZ, 'i', "timestamptz_pl_interval_at_zone"),
    td3_proc(6223, "date_subtract", &[TSTZ, IV], TSTZ, 's', "timestamptz_mi_interval"),
    td3_proc(6273, "date_subtract", &[TSTZ, IV, TEXT], TSTZ, 'i', "timestamptz_mi_interval_at_zone"),
    td3_proc(1199, "age", &[TSTZ, TSTZ], IV, 'i', "timestamptz_age"),
    td3_proc(1386, "age", &[TSTZ], IV, 's', "select pg_catalog.age(cast(current_date as timestamp with time zone), $1)"),
    td3_proc(2058, "age", &[TS, TS], IV, 'i', "timestamp_age"),
    td3_proc(2059, "age", &[TS], IV, 's', "select pg_catalog.age(cast(current_date as timestamp without time zone), $1)"),
    td3_proc(3846, "make_date", &[I4, I4, I4], oid::DATE, 'i', "make_date"),
    td3_proc(3847, "make_time", &[I4, I4, F8], oid::TIME, 'i', "make_time"),
    td3_proc(3461, "make_timestamp", &[I4, I4, I4, I4, I4, F8], TS, 'i', "make_timestamp"),
    td3_proc(3462, "make_timestamptz", &[I4, I4, I4, I4, I4, F8], TSTZ, 's', "make_timestamptz"),
    td3_proc(3463, "make_timestamptz", &[I4, I4, I4, I4, I4, F8, TEXT], TSTZ, 's', "make_timestamptz_at_timezone"),
    td3_proc(3464, "make_interval", &[I4, I4, I4, I4, I4, I4, F8], IV, 'i', "make_interval"),
    td3_proc(1158, "to_timestamp", &[F8], TSTZ, 'i', "float8_timestamptz"),
    td3_proc(1373, "isfinite", &[oid::DATE], oid::BOOL, 'i', "date_finite"),
    td3_proc(1389, "isfinite", &[TSTZ], oid::BOOL, 'i', "timestamp_finite"),
    td3_proc(2048, "isfinite", &[TS], oid::BOOL, 'i', "timestamp_finite"),
    td3_proc(938, "generate_series", &[TS, TS, IV], TS, 'i', "generate_series_timestamp"),
    td3_proc(1768, "to_char", &[IV, TEXT], TEXT, 's', "interval_to_char"),
    td3_proc(1770, "to_char", &[TSTZ, TEXT], TEXT, 's', "timestamptz_to_char"),
    td3_proc(2049, "to_char", &[TS, TEXT], TEXT, 's', "timestamp_to_char"),
    td3_proc(1780, "to_date", &[TEXT, TEXT], oid::DATE, 's', "to_date"),
    td3_proc(1778, "to_timestamp", &[TEXT, TEXT], TSTZ, 's', "to_timestamp"),
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn operators_have_unique_oids_and_meta() {
        let oids: HashSet<_> = INTERVAL_OPERATORS.iter().map(|o| o.oid).collect();
        assert_eq!(oids.len(), INTERVAL_OPERATORS.len());
        for o in INTERVAL_OPERATORS {
            let meta = INTERVAL_OPERATOR_META
                .iter()
                .chain(super::super::OPERATOR_META)
                .find(|m| m.oid == o.oid)
                .unwrap_or_else(|| panic!("meta for {} missing", o.oid));
            // oprcode は PROCS（core か INTERVAL_PROCS）にある
            assert!(
                INTERVAL_PROCS
                    .iter()
                    .chain(super::super::PROCS)
                    .any(|p| p.oid == meta.proc_oid),
                "proc {} of operator {} missing",
                meta.proc_oid,
                o.oid
            );
        }
        for m in INTERVAL_OPERATOR_META {
            assert!(INTERVAL_OPERATORS.iter().any(|o| o.oid == m.oid));
            // core.rs の表と重ならない
            assert!(super::super::OPERATOR_META.iter().all(|c| c.oid != m.oid));
        }
    }

    #[test]
    fn procs_do_not_overlap_core() {
        let oids: HashSet<_> = INTERVAL_PROCS.iter().map(|p| p.oid).collect();
        assert_eq!(oids.len(), INTERVAL_PROCS.len());
        for p in INTERVAL_PROCS {
            assert!(
                super::super::PROCS.iter().all(|c| c.oid != p.oid),
                "{}",
                p.oid
            );
        }
        for f in INTERVAL_FUNCTIONS {
            let p = INTERVAL_PROCS.iter().find(|p| p.oid == f.oid).unwrap();
            assert_eq!((p.name, p.args, p.result), (f.name, f.args, f.result));
        }
    }

    fn val(s: &str) -> Datum {
        Datum::Interval(
            yuzhu_datetime::Interval::parse(
                s,
                -1,
                &yuzhu_datetime::DateTimeEnv::new(
                    &yuzhu_datetime::TimeZone::utc(),
                    &yuzhu_datetime::ZoneDb::without_tzdata(),
                ),
            )
            .unwrap(),
        )
    }

    #[test]
    fn operator_bodies_work() {
        let by = |oid| INTERVAL_OPERATORS.iter().find(|o| o.oid == oid).unwrap();
        assert_eq!(
            (by(1330).func)(&[val("1 day"), val("24:00:00")]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            (by(1337).func)(&[val("1 day"), val("1 day")]).unwrap(),
            (by(1585).func)(&[val("4 days"), Datum::Float8(2.0)]).unwrap()
        );
        assert_eq!(by(1336).kind(), 'l');
        assert!((by(1327).func)(&[]).is_err());
    }

    #[test]
    fn aggregates_rows() {
        for a in INTERVAL_AGGREGATES {
            assert_eq!(a.args, &[INTERVAL]);
            assert_eq!(a.result, INTERVAL);
        }
    }
}

#[cfg(test)]
mod td3_tests {
    use std::collections::HashSet;

    use yuzhu_datetime::{DateTimeEnv, Interval, TimeZone, Timestamp, TimestampTz, ZoneDb};

    use super::*;
    use crate::types::TypeEnv;

    fn by(oid: Oid) -> &'static BuiltinFunction {
        TD3_FUNCTIONS.iter().find(|f| f.oid == oid).unwrap()
    }

    fn call(oid: Oid, args: &[Datum]) -> Result<Datum> {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        let mut dt = DateTimeEnv::new(&tz, &zones);
        // 2024-03-10 12:00:00 UTC
        dt.now = TimestampTz(763_387_200_000_000);
        let env = TypeEnv {
            datetime: Some(dt),
            ..TypeEnv::default()
        };
        match &by(oid).kind {
            FnKind::Pure(f) => f(args),
            FnKind::Env(f) => f(args, &env),
            _ => panic!("unexpected kind"),
        }
    }

    fn txt(s: &str) -> Datum {
        Datum::Text(s.to_owned())
    }

    fn ts(s: &str) -> Datum {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        Datum::Timestamp(Timestamp::parse(s, -1, &DateTimeEnv::new(&tz, &zones)).unwrap())
    }

    fn tstz(s: &str) -> Datum {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        Datum::TimestampTz(TimestampTz::parse(s, -1, &DateTimeEnv::new(&tz, &zones)).unwrap())
    }

    fn iv(s: &str) -> Datum {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        Datum::Interval(Interval::parse(s, -1, &DateTimeEnv::new(&tz, &zones)).unwrap())
    }

    fn numeric_text(d: Datum) -> String {
        match d {
            Datum::Numeric(n) => n.to_string(),
            other => panic!("not numeric: {other:?}"),
        }
    }

    #[test]
    fn rows_are_consistent() {
        let oids: HashSet<_> = TD3_FUNCTIONS.iter().map(|f| f.oid).collect();
        assert_eq!(oids.len(), TD3_FUNCTIONS.len());
        let proc_oids: HashSet<_> = TD3_PROCS.iter().map(|p| p.oid).collect();
        assert_eq!(proc_oids, oids);
        for f in TD3_FUNCTIONS {
            let p = TD3_PROCS.iter().find(|p| p.oid == f.oid).unwrap();
            assert_eq!(
                (p.name, p.args, p.result),
                (f.name, f.args, f.result),
                "{}",
                f.oid
            );
            // 他の表と OID が重ならない
            assert!(
                super::super::PROCS.iter().all(|c| c.oid != f.oid),
                "{}",
                f.oid
            );
            assert!(INTERVAL_PROCS.iter().all(|c| c.oid != f.oid), "{}", f.oid);
        }
    }

    #[test]
    fn extract_and_date_part() {
        assert_eq!(
            numeric_text(call(6204, &[txt("hour"), iv("100:00:00")]).unwrap()),
            "100"
        );
        assert_eq!(
            numeric_text(
                call(6204, &[txt("epoch"), iv("1 year 2 mons 3 days 04:05:06.5")]).unwrap()
            ),
            "37015506.500000"
        );
        assert_eq!(
            numeric_text(call(6204, &[txt("month"), iv("14 months")]).unwrap()),
            "2"
        );
        assert_eq!(
            call(1172, &[txt("year"), iv("25 months")]).unwrap(),
            Datum::Float8(2.0)
        );
        assert_eq!(
            numeric_text(call(6203, &[txt("epoch"), tstz("2000-01-01 00:00+00")]).unwrap()),
            "946684800.000000"
        );
        assert_eq!(
            call(2021, &[txt("dow"), ts("2024-03-10")]).unwrap(),
            Datum::Float8(0.0)
        );
        assert_eq!(
            call(6204, &[txt("foo"), iv("1 day")])
                .unwrap_err()
                .sqlstate
                .code(),
            "22023"
        );
    }

    #[test]
    fn date_trunc_and_bin() {
        let t = call(2020, &[txt("hour"), ts("2024-03-09 15:44:05")]).unwrap();
        assert_eq!(t, ts("2024-03-09 15:00:00"));
        assert_eq!(
            call(1218, &[txt("hour"), iv("1 day 04:05:06")]).unwrap(),
            iv("1 day 04:00:00")
        );
        assert_eq!(
            call(
                6177,
                &[
                    iv("15 minutes"),
                    ts("2024-03-09 15:44:05"),
                    ts("2000-01-01")
                ]
            )
            .unwrap(),
            ts("2024-03-09 15:30:00")
        );
    }

    #[test]
    fn age_and_current_date_forms() {
        assert_eq!(
            call(2058, &[ts("2024-03-15 10:00"), ts("2000-02-29 12:30")]).unwrap(),
            iv("24 years 14 days 21:30:00")
        );
        // now = 2024-03-10 12:00 UTC なので current_date = 2024-03-10
        assert_eq!(call(2059, &[ts("2024-03-09")]).unwrap(), iv("1 day"));
        assert_eq!(
            call(1386, &[tstz("2024-03-09 00:00+00")]).unwrap(),
            iv("1 day")
        );
    }

    #[test]
    fn make_and_to_timestamp() {
        assert_eq!(
            call(
                3461,
                &[
                    Datum::Int4(2024),
                    Datum::Int4(1),
                    Datum::Int4(1),
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Float8(60.0)
                ]
            )
            .unwrap(),
            ts("2024-01-01 00:01:00")
        );
        assert_eq!(
            call(3847, &[Datum::Int4(24), Datum::Int4(0), Datum::Float8(0.0)]).unwrap(),
            Datum::Time(86_400_000_000)
        );
        assert_eq!(
            call(
                3463,
                &[
                    Datum::Int4(2024),
                    Datum::Int4(1),
                    Datum::Int4(1),
                    Datum::Int4(12),
                    Datum::Int4(0),
                    Datum::Float8(0.0),
                    txt("-03")
                ]
            )
            .unwrap(),
            tstz("2024-01-01 15:00+00")
        );
        assert_eq!(
            call(
                3464,
                &[
                    Datum::Int4(1),
                    Datum::Int4(2),
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Float8(0.0)
                ]
            )
            .unwrap(),
            iv("1 year 2 mons")
        );
        assert_eq!(
            call(1158, &[Datum::Float8(1_700_000_000.123_456)]).unwrap(),
            tstz("2023-11-14 22:13:20.123456+00")
        );
        assert_eq!(
            call(1158, &[Datum::Float8(f64::NAN)])
                .unwrap_err()
                .sqlstate
                .code(),
            "22008"
        );
    }

    #[test]
    fn date_add_and_isfinite() {
        assert_eq!(
            call(6221, &[tstz("2024-03-09 12:00+00"), iv("1 day")]).unwrap(),
            tstz("2024-03-10 12:00+00")
        );
        assert_eq!(
            call(
                6273,
                &[tstz("2024-03-10 12:00+00"), iv("1 day"), txt("UTC")]
            )
            .unwrap(),
            tstz("2024-03-09 12:00+00")
        );
        assert_eq!(
            call(
                6222,
                &[tstz("2024-03-09 12:00+00"), iv("1 day"), txt("Foo/Bar")]
            )
            .unwrap_err()
            .sqlstate
            .code(),
            "22023"
        );
        assert_eq!(call(2048, &[ts("2024-01-01")]).unwrap(), Datum::Bool(true));
        assert_eq!(call(2048, &[ts("infinity")]).unwrap(), Datum::Bool(false));
    }

    fn series(args: &[Datum]) -> Result<Vec<Datum>> {
        let FnKind::Set(s) = &by(938).kind else {
            panic!()
        };
        let mut it = (s.begin)(args)?;
        let mut out = Vec::new();
        while let Some(d) = it.next()? {
            out.push(d);
            assert!(out.len() < 1000);
        }
        Ok(out)
    }

    #[test]
    fn generate_series_timestamp_rules() {
        assert_eq!(
            series(&[ts("2024-01-31"), ts("2024-05-01"), iv("1 month")]).unwrap(),
            vec![
                ts("2024-01-31"),
                ts("2024-02-29"),
                ts("2024-03-29"),
                ts("2024-04-29")
            ]
        );
        assert_eq!(
            series(&[ts("2024-01-03"), ts("2024-01-01"), iv("-1 day")])
                .unwrap()
                .len(),
            3
        );
        assert!(
            series(&[ts("2024-01-03"), ts("2024-01-01"), iv("1 day")])
                .unwrap()
                .is_empty()
        );
        assert!(
            series(&[ts("-infinity"), ts("2024-01-01"), iv("1 day")])
                .unwrap()
                .is_empty()
        );
        let e = |s: &str| {
            series(&[ts("2024-01-01"), ts("2024-01-02"), iv(s)])
                .err()
                .unwrap()
        };
        assert_eq!(e("0 sec").message, "step size cannot equal zero");
        assert_eq!(e("infinity").message, "step size cannot be infinite");
    }

    #[test]
    fn formatting_functions_are_not_supported() {
        let e = call(2049, &[ts("2024-01-01"), txt("YYYY")]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "0A000");
    }
}

#[cfg(test)]
mod td4_tests {
    use std::collections::HashSet;

    use super::*;
    use crate::catalog::opclass;
    use crate::types::{TypeEnv, oid};
    use yuzhu_datetime::{DateTimeEnv, TimeZone, ZoneDb};

    fn t(s: &str) -> Datum {
        let tz = TimeZone::utc();
        let zones = ZoneDb::without_tzdata();
        Datum::Time(
            yuzhu_datetime::Time::parse(s, -1, &DateTimeEnv::new(&tz, &zones))
                .unwrap()
                .0,
        )
    }

    #[test]
    fn time_rows_do_not_overlap() {
        let proc_oids: HashSet<_> = TIME_PROCS.iter().map(|p| p.oid).collect();
        assert_eq!(proc_oids.len(), TIME_PROCS.len());
        for p in TIME_PROCS {
            assert!(
                super::super::PROCS.iter().all(|c| c.oid != p.oid),
                "{}",
                p.oid
            );
            assert!(INTERVAL_PROCS.iter().all(|c| c.oid != p.oid), "{}", p.oid);
            assert!(TD3_PROCS.iter().all(|c| c.oid != p.oid), "{}", p.oid);
        }
        let ops: HashSet<_> = TIME_OPERATORS.iter().map(|o| o.oid).collect();
        assert_eq!(ops.len(), TIME_OPERATORS.len());
        for o in TIME_OPERATORS {
            assert!(
                super::super::OPERATORS.iter().all(|c| c.oid != o.oid),
                "{}",
                o.oid
            );
            assert!(
                INTERVAL_OPERATORS.iter().all(|c| c.oid != o.oid),
                "{}",
                o.oid
            );
        }
        assert!(super::super::TYPES.iter().all(|c| c.oid != 1083));
    }

    #[test]
    fn functions_and_operators_point_at_procs() {
        for f in TIME_FUNCTIONS {
            let p = TIME_PROCS
                .iter()
                .find(|p| p.oid == f.oid)
                .unwrap_or_else(|| panic!("{}", f.oid));
            assert_eq!((p.name, p.args, p.result), (f.name, f.args, f.result));
        }
        for m in TIME_OPERATOR_META {
            let o = TIME_OPERATORS.iter().find(|o| o.oid == m.oid).unwrap();
            let p = TIME_PROCS
                .iter()
                .chain(INTERVAL_PROCS)
                .find(|p| p.oid == m.proc_oid)
                .unwrap_or_else(|| panic!("proc {}", m.proc_oid));
            assert_eq!(p.result, o.result);
            assert_eq!(p.args.len(), 2);
        }
        assert_eq!(TIME_OPERATOR_META.len(), TIME_OPERATORS.len());
        for c in TIME_CASTS {
            let p = TIME_PROCS.iter().find(|p| p.oid == c.func_oid).unwrap();
            assert_eq!(p.args[0], c.source);
            assert_eq!(p.result, c.target);
        }
        assert_eq!(TIME_TYPES[0].oid, oid::TIME);
        assert_eq!(TIME_TYPES[0].input_oid, 1143);
        assert!(
            TIME_PROCS
                .iter()
                .any(|p| p.oid == 1143 && p.name == "time_in")
        );
        // timetz は作らない
        assert!(
            TIME_PROCS
                .iter()
                .all(|p| !p.args.contains(&1266) && p.result != 1266)
        );
    }

    #[test]
    fn time_opclass_rows_match_the_tables() {
        let rows: Vec<_> = opclass::AMOPS.iter().filter(|a| a.left == 1083).collect();
        assert_eq!(rows.len(), 5);
        for a in rows {
            assert_eq!((a.family, a.right), (1996, 1083));
            let op = TIME_OPERATORS.iter().find(|o| o.oid == a.operator).unwrap();
            let expected = ["<", "<=", "=", ">=", ">"][usize::from(a.strategy) - 1];
            assert_eq!(op.name, expected);
            assert_eq!(op.left, Some(1083));
            let cmp = opclass::comparator(1996, 1083, 1083).unwrap();
            for x in [0i64, 1, 86_400_000_000] {
                for y in [0i64, 1, 86_400_000_000] {
                    let ord = cmp(&Datum::Time(x), &Datum::Time(y));
                    let want = match a.strategy {
                        1 => ord.is_lt(),
                        2 => ord.is_le(),
                        3 => ord.is_eq(),
                        4 => ord.is_ge(),
                        _ => ord.is_gt(),
                    };
                    assert_eq!(
                        (op.func)(&[Datum::Time(x), Datum::Time(y)]).unwrap(),
                        Datum::Bool(want)
                    );
                }
            }
        }
        let p = opclass::AMPROCS.iter().find(|p| p.left == 1083).unwrap();
        let row = TIME_PROCS.iter().find(|r| r.oid == p.proc_oid).unwrap();
        assert_eq!((row.args, row.result), (&[1083u32, 1083][..], oid::INT4));
        assert!(row.strict && row.volatility == 'i');
        assert_eq!(opclass::cmp_proc_name(p.proc_oid), Some(row.name));
    }

    #[test]
    fn operator_and_cast_bodies_work() {
        let by = |oid| TIME_OPERATORS.iter().find(|o| o.oid == oid).unwrap();
        assert_eq!(
            (by(1108).func)(&[t("04:05"), t("04:05:00")]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            (by(1110).func)(&[t("04:05"), t("23:00")]).unwrap(),
            Datum::Bool(true)
        );
        let sum = (by(1800).func)(&[
            t("23:00"),
            Datum::Interval(yuzhu_datetime::Interval {
                months: 1,
                days: 1,
                micros: 7_200_000_000,
            }),
        ])
        .unwrap();
        assert_eq!(sum, t("01:00"));
        let to_iv = TIME_CASTS.iter().find(|c| c.target == INTERVAL).unwrap();
        assert_eq!(to_iv.context, CastContext::Implicit);
        let CastMethod::Function(f) = to_iv.method else {
            panic!()
        };
        assert!(matches!(f(&[t("01:00")]).unwrap(), Datum::Interval(_)));
        let tz_cast = TIME_CASTS
            .iter()
            .find(|c| c.source == oid::TIMESTAMPTZ)
            .unwrap();
        assert_eq!(tz_cast.context, CastContext::Assignment);
        assert!(matches!(tz_cast.method, CastMethod::Env(_)));
        // timestamptz -> time はセッションのゾーンの壁時計
        let zones = ZoneDb::without_tzdata();
        let tz = TimeZone::utc();
        let env = TypeEnv {
            datetime: Some(DateTimeEnv::new(&tz, &zones)),
            ..TypeEnv::default()
        };
        let CastMethod::Env(g) = tz_cast.method else {
            panic!()
        };
        let v = g(&[Datum::TimestampTz(yuzhu_datetime::TimestampTz(0))], &env).unwrap();
        assert!(matches!(v, Datum::Time(_)));
    }

    #[test]
    fn aggregates_rows() {
        for a in TIME_AGGREGATES {
            assert_eq!(a.args, &[oid::TIME]);
            assert_eq!(a.result, oid::TIME);
        }
        assert!(
            TIME_AGGREGATES
                .iter()
                .any(|a| a.oid == 2139 && a.kind == AggKind::Min)
        );
        assert!(
            TIME_AGGREGATES
                .iter()
                .any(|a| a.oid == 2123 && a.kind == AggKind::Max)
        );
    }
}
