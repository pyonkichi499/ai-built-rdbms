//! 組み込み表の行: 型の入出力まわり（`m5/05-types-core.md` §5.1、TY-1）。
//!
//! # 新しい型を足すときの 8 点チェックリスト（§5.1.1）
//!
//! `bytea`・`uuid`・配列・`interval`・`time` のすべてがこの表に従う。単体テスト `checklist_*` が
//! `TYPES` の全行について、該当する項目の漏れを検査する。
//!
//! | # | 項目 | 置き場所 | 規則 |
//! |---|---|---|---|
//! | 1 | OID 定数と `SqlType` の定数 | `types/mod.rs` | 値は `pg_type.dat`。実機の `SELECT oid, typname FROM pg_type` で確かめる |
//! | 2 | `Datum` の変種と入出力 | `types/datum.rs`、`types/io.rs`、型のモジュール | `input_text` / `output_text` / `input_is_eager` |
//! | 3 | `pg_type` の行と `TYPE_PROCS` | `catalog/builtin/*.rs`、このファイル | `ty(..)` の 16 引数 + `TYPE_PROCS` の recv / send / typmodin / typmodout / typsubscript。`typarray` は PG の値。配列型の行は `array_type_rows()` が作る |
//! | 4 | `pg_proc` の行 | `PROCS`（`tools/gen_procs.sh` が生成） | 入出力・recv・send・typmodin・typmodout・キャスト関数・演算子の実体・`FUNCTIONS` の OID すべて。PG の OID のまま |
//! | 5 | キャストの行 | `CASTS` | PG の `pg_cast` と同じ行。長さ強制（typmod）の行は入れない。文字列との変換は自動 I/O 変換に任せる |
//! | 6 | 演算子・関数の行 | `OPERATORS`・`OPERATOR_META`・`FUNCTIONS` | `oprcode`・`com`・`negate` は PG の値 |
//! | 7 | 比較・ハッシュ・opclass | `cmp_datum`、`hash_datum`、`catalog/opclass.rs` | btree の opclass は `AMOPS` / `AMPROCS` に行を足す |
//! | 8 | ディスク形式と `AttrDesc` | `storage/heap/tuple.rs` の `Kind`、`TYPES` の typlen / align | `Kind` の追加は 1 型 1 分岐 |
//!
//! 各担当は、自分の型の行を [`EXTRA_TYPES`] の区画に足す（`time` / `interval` は TD）。
//! 配列型の行は要素型の行から自動で作られる。

#![allow(clippy::wildcard_imports)]

use super::*;
use std::sync::LazyLock;

/// `pg_type` の recv / send / typmodin / typmodout / typsubscript / typanalyze（TY-D11）。
/// 各 (名前, `pg_proc.oid`)。`("", 0)` は列が 0。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeProcs {
    pub type_oid: Oid,
    pub recv: (&'static str, Oid),
    pub send: (&'static str, Oid),
    pub modin: (&'static str, Oid),
    pub modout: (&'static str, Oid),
    pub subscript: (&'static str, Oid),
    pub analyze: (&'static str, Oid),
}

const NONE: (&str, Oid) = ("", 0);

const fn tp(
    type_oid: Oid,
    recv: (&'static str, Oid),
    send: (&'static str, Oid),
    modin: (&'static str, Oid),
    modout: (&'static str, Oid),
    subscript: (&'static str, Oid),
) -> TypeProcs {
    TypeProcs {
        type_oid,
        recv,
        send,
        modin,
        modout,
        subscript,
        analyze: NONE,
    }
}

/// スカラー型（と複合型・疑似型）の `typreceive` ほか。値は PG17 の `pg_type`。
/// `time`（1083）と `interval`（1186）は TD が行を足すときにここへ足す
/// （recv / send は `time_recv` 2470 / `time_send` 2471 / `interval_recv` 2478 / `interval_send` 2479、
/// typmod は `timetypmodin` 2909 / `timetypmodout` 2910 / `intervaltypmodin` 2903 / `intervaltypmodout` 2904。
/// `pg_proc` の行は TD が `INTERVAL_PROCS` などに足す）。
/// 配列型は要素型から [`type_procs`] が導く。
#[rustfmt::skip]
pub static TYPE_PROCS: &[TypeProcs] = &[
    tp(16, ("boolrecv", 2436), ("boolsend", 2437), ("", 0), ("", 0), ("", 0)), // bool
    tp(17, ("bytearecv", 2412), ("byteasend", 2413), ("", 0), ("", 0), ("", 0)), // bytea
    tp(18, ("charrecv", 2434), ("charsend", 2435), ("", 0), ("", 0), ("", 0)), // char
    tp(19, ("namerecv", 2422), ("namesend", 2423), ("", 0), ("", 0), ("raw_array_subscript_handler", 6180)), // name
    tp(20, ("int8recv", 2408), ("int8send", 2409), ("", 0), ("", 0), ("", 0)), // int8
    tp(21, ("int2recv", 2404), ("int2send", 2405), ("", 0), ("", 0), ("", 0)), // int2
    tp(22, ("int2vectorrecv", 2410), ("int2vectorsend", 2411), ("", 0), ("", 0), ("array_subscript_handler", 6179)), // int2vector
    tp(23, ("int4recv", 2406), ("int4send", 2407), ("", 0), ("", 0), ("", 0)), // int4
    tp(24, ("regprocrecv", 2444), ("regprocsend", 2445), ("", 0), ("", 0), ("", 0)), // regproc
    tp(25, ("textrecv", 2414), ("textsend", 2415), ("", 0), ("", 0), ("", 0)), // text
    tp(26, ("oidrecv", 2418), ("oidsend", 2419), ("", 0), ("", 0), ("", 0)), // oid
    tp(27, ("tidrecv", 2438), ("tidsend", 2439), ("", 0), ("", 0), ("", 0)), // tid
    tp(28, ("xidrecv", 2440), ("xidsend", 2441), ("", 0), ("", 0), ("", 0)), // xid
    tp(29, ("cidrecv", 2442), ("cidsend", 2443), ("", 0), ("", 0), ("", 0)), // cid
    tp(30, ("oidvectorrecv", 2420), ("oidvectorsend", 2421), ("", 0), ("", 0), ("array_subscript_handler", 6179)), // oidvector
    tp(71, ("record_recv", 2402), ("record_send", 2403), ("", 0), ("", 0), ("", 0)), // pg_type
    tp(75, ("record_recv", 2402), ("record_send", 2403), ("", 0), ("", 0), ("", 0)), // pg_attribute
    tp(81, ("record_recv", 2402), ("record_send", 2403), ("", 0), ("", 0), ("", 0)), // pg_proc
    tp(83, ("record_recv", 2402), ("record_send", 2403), ("", 0), ("", 0), ("", 0)), // pg_class
    tp(194, ("pg_node_tree_recv", 197), ("pg_node_tree_send", 198), ("", 0), ("", 0), ("", 0)), // pg_node_tree
    tp(700, ("float4recv", 2424), ("float4send", 2425), ("", 0), ("", 0), ("", 0)), // float4
    tp(701, ("float8recv", 2426), ("float8send", 2427), ("", 0), ("", 0), ("", 0)), // float8
    tp(705, ("unknownrecv", 2416), ("unknownsend", 2417), ("", 0), ("", 0), ("", 0)), // unknown
    tp(1042, ("bpcharrecv", 2430), ("bpcharsend", 2431), ("bpchartypmodin", 2913), ("bpchartypmodout", 2914), ("", 0)), // bpchar
    tp(1043, ("varcharrecv", 2432), ("varcharsend", 2433), ("varchartypmodin", 2915), ("varchartypmodout", 2916), ("", 0)), // varchar
    tp(1082, ("date_recv", 2468), ("date_send", 2469), ("", 0), ("", 0), ("", 0)), // date
    tp(1114, ("timestamp_recv", 2474), ("timestamp_send", 2475), ("timestamptypmodin", 2905), ("timestamptypmodout", 2906), ("", 0)), // timestamp
    tp(1184, ("timestamptz_recv", 2476), ("timestamptz_send", 2477), ("timestamptztypmodin", 2907), ("timestamptztypmodout", 2908), ("", 0)), // timestamptz
    tp(1248, ("record_recv", 2402), ("record_send", 2403), ("", 0), ("", 0), ("", 0)), // pg_database
    tp(1700, ("numeric_recv", 2460), ("numeric_send", 2461), ("numerictypmodin", 2917), ("numerictypmodout", 2918), ("", 0)), // numeric
    tp(2205, ("regclassrecv", 2452), ("regclasssend", 2453), ("", 0), ("", 0), ("", 0)), // regclass
    tp(2206, ("regtyperecv", 2454), ("regtypesend", 2455), ("", 0), ("", 0), ("", 0)), // regtype
    tp(2249, ("record_recv", 2402), ("record_send", 2403), ("", 0), ("", 0), ("", 0)), // record
    tp(2275, ("cstring_recv", 2500), ("cstring_send", 2501), ("", 0), ("", 0), ("", 0)), // cstring
    tp(2277, ("anyarray_recv", 2502), ("anyarray_send", 2503), ("", 0), ("", 0), ("", 0)), // anyarray
    tp(2278, ("void_recv", 3120), ("void_send", 3121), ("", 0), ("", 0), ("", 0)), // void
    tp(2842, ("record_recv", 2402), ("record_send", 2403), ("", 0), ("", 0), ("", 0)), // pg_authid
    tp(2950, ("uuid_recv", 2961), ("uuid_send", 2962), ("", 0), ("", 0), ("", 0)), // uuid
    tp(4089, ("regnamespacerecv", 4087), ("regnamespacesend", 4088), ("", 0), ("", 0), ("", 0)), // regnamespace
    tp(5078, ("anycompatiblearray_recv", 5090), ("anycompatiblearray_send", 5091), ("", 0), ("", 0), ("", 0)), // anycompatiblearray
    // ===== region: time / interval (TD) =====
    // ===== region: end =====
];

const ARRAY_RECV: (&str, Oid) = ("array_recv", 2400);
const ARRAY_SEND: (&str, Oid) = ("array_send", 2401);
const ARRAY_SUBSCRIPT: (&str, Oid) = ("array_subscript_handler", 6179);
const ARRAY_ANALYZE: (&str, Oid) = ("array_typanalyze", 3816);

/// 型の recv / send / typmodin / typmodout / typsubscript / typanalyze。行を持たない型は `None`（列は 0）。
/// 配列型は `array_recv` などで、typmodin / typmodout は要素型に従う。
pub fn type_procs(type_oid: Oid) -> Option<&'static TypeProcs> {
    TYPE_PROCS
        .iter()
        .find(|p| p.type_oid == type_oid)
        .or_else(|| ARRAY_TYPE_PROCS.iter().find(|p| p.type_oid == type_oid))
}

static ARRAY_TYPE_PROCS: LazyLock<Vec<TypeProcs>> = LazyLock::new(|| {
    array_type_rows()
        .iter()
        .map(|a| {
            let e = TYPE_PROCS.iter().find(|p| p.type_oid == a.elem);
            TypeProcs {
                type_oid: a.oid,
                recv: ARRAY_RECV,
                send: ARRAY_SEND,
                modin: e.map_or(NONE, |p| p.modin),
                modout: e.map_or(NONE, |p| p.modout),
                subscript: ARRAY_SUBSCRIPT,
                analyze: ARRAY_ANALYZE,
            }
        })
        .collect()
});

// ----- 新しい基本型の行（bytea・uuid・ポリモーフィック 3 型）-----------------------

/// `core.rs` の `TYPES` に無い基本型の行。結合後の `TYPES` と `array_type_rows()` の入力になる。
#[rustfmt::skip]
pub static EXTRA_TYPES: &[BuiltinType] = &[
    ty(17,   "bytea",              -1, false, 'b', 'U', false, ',', 0, 0, 1001, ("byteain", 1244),   ("byteaout", 31),      'i', 'x', 0),
    ty(2950, "uuid",               16, false, 'b', 'U', false, ',', 0, 0, 2951, ("uuid_in", 2952),   ("uuid_out", 2953),    'c', 'p', 0),
    ty(2283, "anyelement",          4, true,  'p', 'P', false, ',', 0, 0, 0,    ("anyelement_in", 2312), ("anyelement_out", 2313), 'i', 'p', 0),
    ty(5077, "anycompatible",       4, true,  'p', 'P', false, ',', 0, 0, 0,    ("anycompatible_in", 5086), ("anycompatible_out", 5087), 'i', 'p', 0),
    ty(5078, "anycompatiblearray", -1, false, 'p', 'P', false, ',', 0, 0, 0,    ("anycompatiblearray_in", 5088), ("anycompatiblearray_out", 5089), 'd', 'x', 0),
    // ===== region: time / interval (TD) =====
    // ===== region: end =====
];

/// 配列型の OID（31 個。PG17 の `pg_type.typarray` の値。§5.1.2 の表）。要素型の行が表にあるものだけ行を作る。
const ARRAY_OIDS: &[Oid] = &[
    1000, 1001, 1002, 1003, 1005, 1006, 1007, 1008, 1009, 1010, 1011, 1012, 1013, 1014, 1015, 1016,
    1021, 1022, 1028, 1034, 1115, 1182, 1183, 1185, 1187, 1231, 1263, 2210, 2211, 2287, 2951,
];

/// 結合前の基本型の行（`core.rs` の静的な行 + [`EXTRA_TYPES`]）。
fn base_type_rows() -> impl Iterator<Item = &'static BuiltinType> {
    super::core::TYPES.iter().chain(EXTRA_TYPES)
}

static ARRAY_ROWS: LazyLock<Vec<BuiltinType>> = LazyLock::new(|| {
    let mut rows = Vec::new();
    for e in base_type_rows() {
        if !ARRAY_OIDS.contains(&e.array_oid)
            || e.category == 'A' && e.oid != oid::INT2VECTOR && e.oid != oid::OIDVECTOR
        {
            continue;
        }
        let is_record = e.oid == oid::RECORD;
        let name: &'static str = Box::leak(format!("_{}", e.name).into_boxed_str());
        rows.push(BuiltinType {
            oid: e.array_oid,
            name,
            typlen: -1,
            typbyval: false,
            typtype: if is_record { 'p' } else { 'b' },
            category: if is_record { 'P' } else { 'A' },
            preferred: false,
            array_oid: 0,
            input: "array_in",
            output: "array_out",
            delim: ',',
            relid: 0,
            elem: e.oid,
            align: if e.align == 'd' { 'd' } else { 'i' },
            storage: 'x',
            collation: e.collation,
            input_oid: 750,
            output_oid: 751,
        });
    }
    rows
});

/// 配列型の行を要素型の行から作る（§5.1.2）。`TYPES` に静的に並べる代わりにこの関数が返し、
/// 結合後の `TYPES` に入る。`core.rs` にある M2 の静的な配列型の行（`_char` `_int2` など）は
/// 同じ OID のこの関数の結果で置き換わる。
pub fn array_type_rows() -> &'static [BuiltinType] {
    &ARRAY_ROWS
}

/// 結合後の `TYPES`: `core.rs` の行（配列型の行は `array_type_rows()` で置き換え）+ `EXTRA_TYPES` + 配列型の行。
pub(super) fn merged_types() -> Vec<BuiltinType> {
    let arrays = array_type_rows();
    base_type_rows()
        .filter(|t| arrays.iter().all(|a| a.oid != t.oid))
        .cloned()
        .chain(arrays.iter().cloned())
        .collect()
}

/// `pg_proc` の行で `core.rs` の `PROCS` に無いもの（recv / send / typmodin / typmodout / 配列 / bytea・uuid・
/// ポリモーフィック型の入出力）。`tools/gen_procs.sh` で PG17 から生成した。
#[rustfmt::skip]
pub static TYPE_IO_PROCS: &[BuiltinProc] = &[
    BuiltinProc { oid: 31, name: "byteaout", args: &[17], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "byteaout" },
    BuiltinProc { oid: 197, name: "pg_node_tree_recv", args: &[2281], result: 194, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "pg_node_tree_recv" },
    BuiltinProc { oid: 198, name: "pg_node_tree_send", args: &[194], result: 17, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "pg_node_tree_send" },
    BuiltinProc { oid: 1244, name: "byteain", args: &[2275], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "byteain" },
    BuiltinProc { oid: 2312, name: "anyelement_in", args: &[2275], result: 2283, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anyelement_in" },
    BuiltinProc { oid: 2313, name: "anyelement_out", args: &[2283], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anyelement_out" },
    BuiltinProc { oid: 2400, name: "array_recv", args: &[2281, 26, 23], result: 2277, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "array_recv" },
    BuiltinProc { oid: 2401, name: "array_send", args: &[2277], result: 17, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "array_send" },
    BuiltinProc { oid: 2402, name: "record_recv", args: &[2281, 26, 23], result: 2249, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "record_recv" },
    BuiltinProc { oid: 2403, name: "record_send", args: &[2249], result: 17, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "record_send" },
    BuiltinProc { oid: 2404, name: "int2recv", args: &[2281], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2recv" },
    BuiltinProc { oid: 2405, name: "int2send", args: &[21], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2send" },
    BuiltinProc { oid: 2406, name: "int4recv", args: &[2281], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4recv" },
    BuiltinProc { oid: 2407, name: "int4send", args: &[23], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4send" },
    BuiltinProc { oid: 2408, name: "int8recv", args: &[2281], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8recv" },
    BuiltinProc { oid: 2409, name: "int8send", args: &[20], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8send" },
    BuiltinProc { oid: 2410, name: "int2vectorrecv", args: &[2281], result: 22, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2vectorrecv" },
    BuiltinProc { oid: 2411, name: "int2vectorsend", args: &[22], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2vectorsend" },
    BuiltinProc { oid: 2412, name: "bytearecv", args: &[2281], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "bytearecv" },
    BuiltinProc { oid: 2413, name: "byteasend", args: &[17], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "byteasend" },
    BuiltinProc { oid: 2414, name: "textrecv", args: &[2281], result: 25, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textrecv" },
    BuiltinProc { oid: 2415, name: "textsend", args: &[25], result: 17, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textsend" },
    BuiltinProc { oid: 2416, name: "unknownrecv", args: &[2281], result: 705, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "unknownrecv" },
    BuiltinProc { oid: 2417, name: "unknownsend", args: &[705], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "unknownsend" },
    BuiltinProc { oid: 2418, name: "oidrecv", args: &[2281], result: 26, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "oidrecv" },
    BuiltinProc { oid: 2419, name: "oidsend", args: &[26], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "oidsend" },
    BuiltinProc { oid: 2420, name: "oidvectorrecv", args: &[2281], result: 30, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "oidvectorrecv" },
    BuiltinProc { oid: 2421, name: "oidvectorsend", args: &[30], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "oidvectorsend" },
    BuiltinProc { oid: 2422, name: "namerecv", args: &[2281], result: 19, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "namerecv" },
    BuiltinProc { oid: 2423, name: "namesend", args: &[19], result: 17, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "namesend" },
    BuiltinProc { oid: 2424, name: "float4recv", args: &[2281], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4recv" },
    BuiltinProc { oid: 2425, name: "float4send", args: &[700], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4send" },
    BuiltinProc { oid: 2426, name: "float8recv", args: &[2281], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8recv" },
    BuiltinProc { oid: 2427, name: "float8send", args: &[701], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8send" },
    BuiltinProc { oid: 2430, name: "bpcharrecv", args: &[2281, 26, 23], result: 1042, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "bpcharrecv" },
    BuiltinProc { oid: 2431, name: "bpcharsend", args: &[1042], result: 17, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "bpcharsend" },
    BuiltinProc { oid: 2432, name: "varcharrecv", args: &[2281, 26, 23], result: 1043, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "varcharrecv" },
    BuiltinProc { oid: 2433, name: "varcharsend", args: &[1043], result: 17, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "varcharsend" },
    BuiltinProc { oid: 2434, name: "charrecv", args: &[2281], result: 18, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "charrecv" },
    BuiltinProc { oid: 2435, name: "charsend", args: &[18], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "charsend" },
    BuiltinProc { oid: 2436, name: "boolrecv", args: &[2281], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "boolrecv" },
    BuiltinProc { oid: 2437, name: "boolsend", args: &[16], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "boolsend" },
    BuiltinProc { oid: 2438, name: "tidrecv", args: &[2281], result: 27, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "tidrecv" },
    BuiltinProc { oid: 2439, name: "tidsend", args: &[27], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "tidsend" },
    BuiltinProc { oid: 2440, name: "xidrecv", args: &[2281], result: 28, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "xidrecv" },
    BuiltinProc { oid: 2441, name: "xidsend", args: &[28], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "xidsend" },
    BuiltinProc { oid: 2442, name: "cidrecv", args: &[2281], result: 29, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "cidrecv" },
    BuiltinProc { oid: 2443, name: "cidsend", args: &[29], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "cidsend" },
    BuiltinProc { oid: 2444, name: "regprocrecv", args: &[2281], result: 24, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "regprocrecv" },
    BuiltinProc { oid: 2445, name: "regprocsend", args: &[24], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "regprocsend" },
    BuiltinProc { oid: 2452, name: "regclassrecv", args: &[2281], result: 2205, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "regclassrecv" },
    BuiltinProc { oid: 2453, name: "regclasssend", args: &[2205], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "regclasssend" },
    BuiltinProc { oid: 2454, name: "regtyperecv", args: &[2281], result: 2206, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "regtyperecv" },
    BuiltinProc { oid: 2455, name: "regtypesend", args: &[2206], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "regtypesend" },
    BuiltinProc { oid: 2460, name: "numeric_recv", args: &[2281, 26, 23], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_recv" },
    BuiltinProc { oid: 2461, name: "numeric_send", args: &[1700], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_send" },
    BuiltinProc { oid: 2468, name: "date_recv", args: &[2281], result: 1082, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "date_recv" },
    BuiltinProc { oid: 2469, name: "date_send", args: &[1082], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "date_send" },
    BuiltinProc { oid: 2474, name: "timestamp_recv", args: &[2281, 26, 23], result: 1114, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamp_recv" },
    BuiltinProc { oid: 2475, name: "timestamp_send", args: &[1114], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamp_send" },
    BuiltinProc { oid: 2476, name: "timestamptz_recv", args: &[2281, 26, 23], result: 1184, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamptz_recv" },
    BuiltinProc { oid: 2477, name: "timestamptz_send", args: &[1184], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamptz_send" },
    BuiltinProc { oid: 2500, name: "cstring_recv", args: &[2281], result: 2275, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "cstring_recv" },
    BuiltinProc { oid: 2501, name: "cstring_send", args: &[2275], result: 17, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "cstring_send" },
    BuiltinProc { oid: 2502, name: "anyarray_recv", args: &[2281], result: 2277, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anyarray_recv" },
    BuiltinProc { oid: 2503, name: "anyarray_send", args: &[2277], result: 17, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anyarray_send" },
    BuiltinProc { oid: 2903, name: "intervaltypmodin", args: &[1263], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "intervaltypmodin" },
    BuiltinProc { oid: 2904, name: "intervaltypmodout", args: &[23], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "intervaltypmodout" },
    BuiltinProc { oid: 2905, name: "timestamptypmodin", args: &[1263], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamptypmodin" },
    BuiltinProc { oid: 2906, name: "timestamptypmodout", args: &[23], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamptypmodout" },
    BuiltinProc { oid: 2907, name: "timestamptztypmodin", args: &[1263], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamptztypmodin" },
    BuiltinProc { oid: 2908, name: "timestamptztypmodout", args: &[23], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamptztypmodout" },
    BuiltinProc { oid: 2909, name: "timetypmodin", args: &[1263], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timetypmodin" },
    BuiltinProc { oid: 2910, name: "timetypmodout", args: &[23], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timetypmodout" },
    BuiltinProc { oid: 2913, name: "bpchartypmodin", args: &[1263], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "bpchartypmodin" },
    BuiltinProc { oid: 2914, name: "bpchartypmodout", args: &[23], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "bpchartypmodout" },
    BuiltinProc { oid: 2915, name: "varchartypmodin", args: &[1263], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "varchartypmodin" },
    BuiltinProc { oid: 2916, name: "varchartypmodout", args: &[23], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "varchartypmodout" },
    BuiltinProc { oid: 2917, name: "numerictypmodin", args: &[1263], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numerictypmodin" },
    BuiltinProc { oid: 2918, name: "numerictypmodout", args: &[23], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numerictypmodout" },
    BuiltinProc { oid: 2952, name: "uuid_in", args: &[2275], result: 2950, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "uuid_in" },
    BuiltinProc { oid: 2953, name: "uuid_out", args: &[2950], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "uuid_out" },
    BuiltinProc { oid: 2961, name: "uuid_recv", args: &[2281], result: 2950, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "uuid_recv" },
    BuiltinProc { oid: 2962, name: "uuid_send", args: &[2950], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "uuid_send" },
    BuiltinProc { oid: 3120, name: "void_recv", args: &[2281], result: 2278, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "void_recv" },
    BuiltinProc { oid: 3121, name: "void_send", args: &[2278], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "void_send" },
    BuiltinProc { oid: 3816, name: "array_typanalyze", args: &[2281], result: 16, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "array_typanalyze" },
    BuiltinProc { oid: 4087, name: "regnamespacerecv", args: &[2281], result: 4089, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "regnamespacerecv" },
    BuiltinProc { oid: 4088, name: "regnamespacesend", args: &[4089], result: 17, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "regnamespacesend" },
    BuiltinProc { oid: 5086, name: "anycompatible_in", args: &[2275], result: 5077, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anycompatible_in" },
    BuiltinProc { oid: 5087, name: "anycompatible_out", args: &[5077], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anycompatible_out" },
    BuiltinProc { oid: 5088, name: "anycompatiblearray_in", args: &[2275], result: 5078, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anycompatiblearray_in" },
    BuiltinProc { oid: 5089, name: "anycompatiblearray_out", args: &[5078], result: 2275, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anycompatiblearray_out" },
    BuiltinProc { oid: 5090, name: "anycompatiblearray_recv", args: &[2281], result: 5078, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anycompatiblearray_recv" },
    BuiltinProc { oid: 5091, name: "anycompatiblearray_send", args: &[5078], result: 17, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anycompatiblearray_send" },
    BuiltinProc { oid: 6179, name: "array_subscript_handler", args: &[2281], result: 2281, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "array_subscript_handler" },
    BuiltinProc { oid: 6180, name: "raw_array_subscript_handler", args: &[2281], result: 2281, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "raw_array_subscript_handler" },
    // ===== region: end =====
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn checklist_types_have_procs_and_rows() {
        for t in TYPES.iter() {
            // 3: 入出力関数の行
            for (name, o) in [(t.input, t.input_oid), (t.output, t.output_oid)] {
                let p = proc_by_oid(o).unwrap_or_else(|| panic!("{}: no proc {o}", t.name));
                assert_eq!(p.name, name, "{}", t.name);
            }
            // 3: TYPE_PROCS の各 OID は PROCS にあり、名前が一致する
            if let Some(tp) = type_procs(t.oid) {
                for (name, o) in [
                    tp.recv,
                    tp.send,
                    tp.modin,
                    tp.modout,
                    tp.subscript,
                    tp.analyze,
                ] {
                    if o == 0 {
                        continue;
                    }
                    let p = proc_by_oid(o).unwrap_or_else(|| panic!("{}: no proc {o}", t.name));
                    assert_eq!(p.name, name, "{}", t.name);
                }
            }
            // 配列型: typelem の行があり、要素型の typarray が自分
            if t.category == 'A' && t.name.starts_with('_') {
                let e = type_by_oid(t.elem).unwrap();
                assert_eq!(e.array_oid, t.oid, "{}", t.name);
                let tp = type_procs(t.oid).unwrap();
                assert_eq!(tp.recv.1, 2400);
                assert_eq!(tp.send.1, 2401);
                assert_eq!(tp.subscript.1, 6179);
                assert_eq!(tp.analyze.1, 3816);
            }
            // 8: typlen / align の組み合わせ
            assert!(matches!(t.typlen, -2 | -1 | 1..=64), "{}", t.name);
        }
    }

    #[test]
    fn checklist_array_rows_follow_the_spec() {
        let rows = array_type_rows();
        let oids: HashSet<_> = rows.iter().map(|r| r.oid).collect();
        assert_eq!(oids.len(), rows.len());
        for r in rows {
            assert!(ARRAY_OIDS.contains(&r.oid), "{}", r.name);
            assert_eq!((r.input_oid, r.output_oid, r.storage), (750, 751, 'x'));
        }
        let by = |o: Oid| rows.iter().find(|r| r.oid == o).unwrap();
        assert_eq!(by(1009).name, "_text");
        assert_eq!((by(1009).collation, by(1003).collation), (100, 950));
        assert_eq!(
            (by(1005).align, by(1016).align, by(2287).align),
            ('i', 'd', 'd')
        );
        assert_eq!((by(2287).typtype, by(2287).category), ('p', 'P'));
        assert_eq!(by(1034).align, 'd');
        assert_eq!(by(1001).elem, oid::BYTEA);
        assert_eq!(by(2951).elem, oid::UUID);
        // time / interval の行（TD）が入るまでは 29 行。
        assert!(rows.len() >= 29, "{}", rows.len());
    }

    #[test]
    fn typmod_procs_follow_the_element() {
        let p = |o| type_procs(o).unwrap();
        assert_eq!(p(1231).modin.1, 2917);
        assert_eq!(p(1231).modout.1, 2918);
        assert_eq!(p(oid::NUMERIC).modin.1, 2917);
        assert_eq!(p(1014).modin.1, 2913);
        assert_eq!(p(1007).modin.1, 0);
        assert_eq!(p(oid::NAME).subscript.1, 6180);
        assert_eq!(p(oid::INT2VECTOR).subscript.1, 6179);
        assert_eq!(p(oid::BYTEA).recv.1, 2412);
        assert_eq!(p(oid::UUID).send.1, 2962);
    }

    #[test]
    fn new_base_types_exist() {
        for (o, n) in [
            (oid::BYTEA, "bytea"),
            (oid::UUID, "uuid"),
            (oid::ANYELEMENT, "anyelement"),
            (oid::ANYCOMPATIBLE, "anycompatible"),
            (oid::ANYCOMPATIBLEARRAY, "anycompatiblearray"),
        ] {
            assert_eq!(type_by_oid(o).unwrap().name, n);
        }
        assert_eq!(type_by_oid(oid::BOOL).unwrap().array_oid, 1000);
        assert!(type_by_oid(1000).is_some());
    }

    #[test]
    fn function_form_casts_are_callable() {
        // numeric 関係と bpchar 関係（§5.1.5）
        for o in [
            1740, 1781, 1782, 1742, 1743, 1783, 1744, 1779, 1745, 1746, 408, 409, 401, 860, 944,
        ] {
            if let Some(c) = CASTS.iter().find(|c| c.func_oid == o) {
                let f = FUNCTIONS.iter().find(|f| f.oid == o);
                let p = proc_by_oid(o).unwrap();
                let target = type_by_oid(c.target).unwrap();
                if p.name == target.name {
                    assert!(f.is_some(), "cast function {o} ({}) missing", p.name);
                }
            }
        }
        // 既存の date() などは重複しない
        let mut seen = HashSet::new();
        assert!(FUNCTIONS.iter().all(|f| seen.insert(f.oid)));
    }
}
