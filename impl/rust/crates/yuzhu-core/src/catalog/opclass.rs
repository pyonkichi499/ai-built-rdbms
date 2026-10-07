//! 演算子クラス・演算子族の静的な表（`m4/00-contracts.md` §11.3、`m4/06-btree.md` §4.7）。
//!
//! プランナと B+Tree は実行時にカタログを引かず、この表を引く（M4 にはユーザー定義の opclass がない）。
//! OID は PostgreSQL 17 の `pg_opfamily` / `pg_opclass` / `pg_amop` / `pg_amproc` から写した
//! （`pg_opclass` の 10000 番台は initdb が振る値で、版によってずれうる）。
//!
//! `AMOPS`（120 行）と `AMPROCS`（24 行）は、24 組の `COMPARISONS` から `const fn` で展開する。

use crate::types::{Datum, Oid, cmp_datum};

pub use crate::types::CmpFn;

const BTREE_AM: Oid = 403;

const BOOL_OPS: Oid = 424;
const BPCHAR_OPS: Oid = 426;
const DATETIME_OPS: Oid = 434;
const FLOAT_OPS: Oid = 1970;
const INTEGER_OPS: Oid = 1976;
const NUMERIC_OPS: Oid = 1988;
const OID_OPS: Oid = 1989;
const TEXT_OPS: Oid = 1994;

#[derive(Debug)]
pub struct OpFamily {
    pub oid: Oid,
    pub name: &'static str,
}

#[derive(Debug)]
pub struct OpClass {
    pub oid: Oid,
    pub name: &'static str,
    pub family: Oid,
    pub input_type: Oid,
    pub is_default: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct AmOp {
    pub family: Oid,
    pub left: Oid,
    pub right: Oid,
    /// 1 `<`、2 `<=`、3 `=`、4 `>=`、5 `>`。
    pub strategy: u8,
    pub operator: Oid,
}

#[derive(Clone, Copy, Debug)]
pub struct AmProc {
    pub family: Oid,
    pub left: Oid,
    pub right: Oid,
    /// 1 = 比較関数。
    pub support: u8,
    pub proc_oid: Oid,
    pub cmp: CmpFn,
}

/// `pg_am` の btree の OID（`pg_opfamily.opfmethod`）。
pub const fn btree_am_oid() -> Oid {
    BTREE_AM
}

pub static OPFAMILIES: &[OpFamily] = &[
    OpFamily {
        oid: BOOL_OPS,
        name: "bool_ops",
    },
    OpFamily {
        oid: BPCHAR_OPS,
        name: "bpchar_ops",
    },
    OpFamily {
        oid: DATETIME_OPS,
        name: "datetime_ops",
    },
    OpFamily {
        oid: FLOAT_OPS,
        name: "float_ops",
    },
    OpFamily {
        oid: INTEGER_OPS,
        name: "integer_ops",
    },
    OpFamily {
        oid: NUMERIC_OPS,
        name: "numeric_ops",
    },
    OpFamily {
        oid: OID_OPS,
        name: "oid_ops",
    },
    OpFamily {
        oid: TEXT_OPS,
        name: "text_ops",
    },
];

const fn class(
    oid: Oid,
    name: &'static str,
    family: Oid,
    input_type: Oid,
    is_default: bool,
) -> OpClass {
    OpClass {
        oid,
        name,
        family,
        input_type,
        is_default,
    }
}

pub static OPCLASSES: &[OpClass] = &[
    class(10003, "bool_ops", BOOL_OPS, 16, true),
    class(10004, "bpchar_ops", BPCHAR_OPS, 1042, true),
    class(3122, "date_ops", DATETIME_OPS, 1082, true),
    class(3128, "timestamp_ops", DATETIME_OPS, 1114, true),
    class(3127, "timestamptz_ops", DATETIME_OPS, 1184, true),
    class(10012, "float4_ops", FLOAT_OPS, 700, true),
    class(3123, "float8_ops", FLOAT_OPS, 701, true),
    class(1979, "int2_ops", INTEGER_OPS, 21, true),
    class(1978, "int4_ops", INTEGER_OPS, 23, true),
    class(3124, "int8_ops", INTEGER_OPS, 20, true),
    class(3125, "numeric_ops", NUMERIC_OPS, 1700, true),
    class(1981, "oid_ops", OID_OPS, 26, true),
    class(3126, "text_ops", TEXT_OPS, 25, true),
    class(10044, "varchar_ops", TEXT_OPS, 25, false),
    class(10028, "name_ops", TEXT_OPS, 19, true),
];

/// 同じ（族、左の型、右の型）の比較演算子 5 つと比較関数。
struct Comparison {
    family: Oid,
    left: Oid,
    right: Oid,
    /// `<`、`<=`、`=`、`>=`、`>` の順（strategy 1〜5）。
    ops: [Oid; 5],
    proc_oid: Oid,
    proc_name: &'static str,
}

const fn cmpn(
    family: Oid,
    left: Oid,
    right: Oid,
    ops: [Oid; 5],
    proc_oid: Oid,
    proc_name: &'static str,
) -> Comparison {
    Comparison {
        family,
        left,
        right,
        ops,
        proc_oid,
        proc_name,
    }
}

const COMPARISON_COUNT: usize = 24;

/// `m4/06-btree.md` §4.7 の `AMOPS` と `AMPROCS` の表。
static COMPARISONS: [Comparison; COMPARISON_COUNT] = [
    cmpn(
        BOOL_OPS,
        16,
        16,
        [58, 1694, 91, 1695, 59],
        1693,
        "btboolcmp",
    ),
    cmpn(
        BPCHAR_OPS,
        1042,
        1042,
        [1058, 1059, 1054, 1061, 1060],
        1078,
        "bpcharcmp",
    ),
    cmpn(
        DATETIME_OPS,
        1082,
        1082,
        [1095, 1096, 1093, 1098, 1097],
        1092,
        "date_cmp",
    ),
    cmpn(
        DATETIME_OPS,
        1114,
        1114,
        [2062, 2063, 2060, 2065, 2064],
        2045,
        "timestamp_cmp",
    ),
    cmpn(
        DATETIME_OPS,
        1184,
        1184,
        [1322, 1323, 1320, 1325, 1324],
        1314,
        "timestamptz_cmp",
    ),
    cmpn(
        FLOAT_OPS,
        700,
        700,
        [622, 624, 620, 625, 623],
        354,
        "btfloat4cmp",
    ),
    cmpn(
        FLOAT_OPS,
        700,
        701,
        [1122, 1124, 1120, 1125, 1123],
        2194,
        "btfloat48cmp",
    ),
    cmpn(
        FLOAT_OPS,
        701,
        700,
        [1132, 1134, 1130, 1135, 1133],
        2195,
        "btfloat84cmp",
    ),
    cmpn(
        FLOAT_OPS,
        701,
        701,
        [672, 673, 670, 675, 674],
        355,
        "btfloat8cmp",
    ),
    cmpn(
        INTEGER_OPS,
        21,
        21,
        [95, 522, 94, 524, 520],
        350,
        "btint2cmp",
    ),
    cmpn(
        INTEGER_OPS,
        21,
        23,
        [534, 540, 532, 542, 536],
        2190,
        "btint24cmp",
    ),
    cmpn(
        INTEGER_OPS,
        21,
        20,
        [1864, 1866, 1862, 1867, 1865],
        2192,
        "btint28cmp",
    ),
    cmpn(
        INTEGER_OPS,
        23,
        21,
        [535, 541, 533, 543, 537],
        2191,
        "btint42cmp",
    ),
    cmpn(
        INTEGER_OPS,
        23,
        23,
        [97, 523, 96, 525, 521],
        351,
        "btint4cmp",
    ),
    cmpn(
        INTEGER_OPS,
        23,
        20,
        [37, 80, 15, 82, 76],
        2188,
        "btint48cmp",
    ),
    cmpn(
        INTEGER_OPS,
        20,
        21,
        [1870, 1872, 1868, 1873, 1871],
        2193,
        "btint82cmp",
    ),
    cmpn(
        INTEGER_OPS,
        20,
        23,
        [418, 420, 416, 430, 419],
        2189,
        "btint84cmp",
    ),
    cmpn(
        INTEGER_OPS,
        20,
        20,
        [412, 414, 410, 415, 413],
        842,
        "btint8cmp",
    ),
    cmpn(
        NUMERIC_OPS,
        1700,
        1700,
        [1754, 1755, 1752, 1757, 1756],
        1769,
        "numeric_cmp",
    ),
    cmpn(OID_OPS, 26, 26, [609, 611, 607, 612, 610], 356, "btoidcmp"),
    cmpn(TEXT_OPS, 19, 19, [660, 661, 93, 663, 662], 359, "btnamecmp"),
    cmpn(
        TEXT_OPS,
        19,
        25,
        [255, 256, 254, 257, 258],
        246,
        "btnametextcmp",
    ),
    cmpn(
        TEXT_OPS,
        25,
        19,
        [261, 262, 260, 263, 264],
        253,
        "bttextnamecmp",
    ),
    cmpn(TEXT_OPS, 25, 25, [664, 665, 98, 667, 666], 360, "bttextcmp"),
];

const AMOP_COUNT: usize = COMPARISON_COUNT * 5;

#[allow(clippy::cast_possible_truncation)]
const fn expand_amops() -> [AmOp; AMOP_COUNT] {
    const ZERO: AmOp = AmOp {
        family: 0,
        left: 0,
        right: 0,
        strategy: 0,
        operator: 0,
    };
    let mut out = [ZERO; AMOP_COUNT];
    let mut i = 0;
    while i < COMPARISON_COUNT {
        let c = &COMPARISONS[i];
        let mut s = 0;
        while s < 5 {
            out[i * 5 + s] = AmOp {
                family: c.family,
                left: c.left,
                right: c.right,
                strategy: (s + 1) as u8,
                operator: c.ops[s],
            };
            s += 1;
        }
        i += 1;
    }
    out
}

const fn expand_amprocs() -> [AmProc; COMPARISON_COUNT] {
    const ZERO: AmProc = AmProc {
        family: 0,
        left: 0,
        right: 0,
        support: 0,
        proc_oid: 0,
        cmp: cmp_datum,
    };
    let mut out = [ZERO; COMPARISON_COUNT];
    let mut i = 0;
    while i < COMPARISON_COUNT {
        let c = &COMPARISONS[i];
        out[i] = AmProc {
            family: c.family,
            left: c.left,
            right: c.right,
            support: 1,
            proc_oid: c.proc_oid,
            cmp: cmp_datum,
        };
        i += 1;
    }
    out
}

static AMOPS_ARRAY: [AmOp; AMOP_COUNT] = expand_amops();
static AMPROCS_ARRAY: [AmProc; COMPARISON_COUNT] = expand_amprocs();

pub static AMOPS: &[AmOp] = &AMOPS_ARRAY;
pub static AMPROCS: &[AmProc] = &AMPROCS_ARRAY;

/// バイナリ互換の型 → 入力型（varchar → text、regclass / regtype / regproc → oid）。
fn binary_coercible_input(type_oid: Oid) -> Oid {
    match type_oid {
        1043 => 25,
        2205 | 2206 | 24 => 26,
        t => t,
    }
}

/// 型の既定の演算子クラス。入力型が一致する既定の opclass、なければバイナリ互換の型の入力型で引く。
pub fn default_opclass(type_oid: Oid) -> Option<&'static OpClass> {
    let input = binary_coercible_input(type_oid);
    OPCLASSES
        .iter()
        .find(|c| c.is_default && c.input_type == input)
}

/// 列の型の族（`default_opclass(type_oid).family`）。
pub fn family_of_type(type_oid: Oid) -> Option<Oid> {
    default_opclass(type_oid).map(|c| c.family)
}

pub fn opclass_by_oid(oid: Oid) -> Option<&'static OpClass> {
    OPCLASSES.iter().find(|c| c.oid == oid)
}

pub fn opclass_by_name(name: &str) -> Option<&'static OpClass> {
    OPCLASSES.iter().find(|c| c.name == name)
}

/// `(opfamily, 左の型, 右の型)` の比較関数。
pub fn comparator(family: Oid, left: Oid, right: Oid) -> Option<CmpFn> {
    AMPROCS
        .iter()
        .find(|p| p.family == family && p.left == left && p.right == right && p.support == 1)
        .map(|p| p.cmp)
}

/// この演算子（`pg_operator.oid`）は、この opfamily の btree で使えるか。使えるなら
/// `(strategy, 左の型, 右の型)`。
pub fn operator_strategy(operator: Oid, family: Oid) -> Option<(u8, Oid, Oid)> {
    AMOPS
        .iter()
        .find(|a| a.operator == operator && a.family == family)
        .map(|a| (a.strategy, a.left, a.right))
}

/// `Datum` の変種から型の OID。表にない変種は `None`。
pub fn datum_type_oid(d: &Datum) -> Option<Oid> {
    Some(match d {
        Datum::Bool(_) => 16,
        Datum::Int2(_) => 21,
        Datum::Int4(_) => 23,
        Datum::Int8(_) => 20,
        Datum::Float4(_) => 700,
        Datum::Float8(_) => 701,
        Datum::Numeric(_) => 1700,
        Datum::Text(_) => 25,
        Datum::BpChar(_) => 1042,
        Datum::Oid(_) => 26,
        Datum::Date(_) => 1082,
        Datum::Timestamp(_) => 1114,
        Datum::TimestampTz(_) => 1184,
        _ => return None,
    })
}

/// 索引の列（型 `col_type`）の値と、スキャンキーの値 `d` を比べる関数。
/// `comparator(族, 列の opclass の入力型, datum_type_oid(d))` を引く。引けなければ `None`。
pub fn column_comparator(col_type: Oid, d: &Datum) -> Option<CmpFn> {
    let class = default_opclass(col_type)?;
    comparator(class.family, class.input_type, datum_type_oid(d)?)
}

/// `pg_proc` の行を作るための名前（`AMPROCS` の `proc_oid` → 名前）。
pub fn cmp_proc_name(proc_oid: Oid) -> Option<&'static str> {
    COMPARISONS
        .iter()
        .find(|c| c.proc_oid == proc_oid)
        .map(|c| c.proc_name)
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;
    use std::collections::HashSet;

    use super::*;
    use crate::catalog::builtin;
    use crate::types::cmp::cmp_with_nulls;
    use crate::types::oid;

    #[test]
    fn table_sizes() {
        assert_eq!(OPFAMILIES.len(), 8);
        assert_eq!(OPCLASSES.len(), 15);
        assert_eq!(AMOPS.len(), 120);
        assert_eq!(AMPROCS.len(), 24);
        assert_eq!(btree_am_oid(), 403);
    }

    #[test]
    fn oids_are_unique() {
        let fam: HashSet<_> = OPFAMILIES.iter().map(|f| f.oid).collect();
        assert_eq!(fam.len(), OPFAMILIES.len());
        let cls: HashSet<_> = OPCLASSES.iter().map(|c| c.oid).collect();
        assert_eq!(cls.len(), OPCLASSES.len());
        let names: HashSet<_> = OPCLASSES.iter().map(|c| c.name).collect();
        assert_eq!(names.len(), OPCLASSES.len());
        let ops: HashSet<_> = AMOPS.iter().map(|a| (a.family, a.operator)).collect();
        assert_eq!(ops.len(), AMOPS.len());
        for c in OPCLASSES {
            assert!(OPFAMILIES.iter().any(|f| f.oid == c.family), "{}", c.name);
        }
    }

    #[test]
    fn every_pair_has_five_strategies_and_a_proc() {
        let pairs: HashSet<_> = AMOPS.iter().map(|a| (a.family, a.left, a.right)).collect();
        assert_eq!(pairs.len(), 24);
        for &(f, l, r) in &pairs {
            let mut s: Vec<u8> = AMOPS
                .iter()
                .filter(|a| (a.family, a.left, a.right) == (f, l, r))
                .map(|a| a.strategy)
                .collect();
            s.sort_unstable();
            assert_eq!(s, [1, 2, 3, 4, 5], "{f} {l} {r}");
            let procs: Vec<_> = AMPROCS
                .iter()
                .filter(|p| (p.family, p.left, p.right) == (f, l, r))
                .collect();
            assert_eq!(procs.len(), 1);
            assert_eq!(procs[0].support, 1);
        }
    }

    #[test]
    fn one_default_per_type() {
        let mut seen = HashSet::new();
        for c in OPCLASSES.iter().filter(|c| c.is_default) {
            assert!(seen.insert(c.input_type), "{}", c.name);
        }
        // 既定でないのは varchar_ops だけ。
        let non_default: Vec<_> = OPCLASSES.iter().filter(|c| !c.is_default).collect();
        assert_eq!(non_default.len(), 1);
        assert_eq!(non_default[0].name, "varchar_ops");
    }

    #[test]
    fn key_oids_match_postgres() {
        let o = |n| opclass_by_name(n).unwrap().oid;
        assert_eq!(o("int4_ops"), 1978);
        assert_eq!(o("int2_ops"), 1979);
        assert_eq!(o("oid_ops"), 1981);
        assert_eq!(o("text_ops"), 3126);
        assert_eq!(o("date_ops"), 3122);
        assert_eq!(opclass_by_oid(1978).unwrap().family, 1976);
        assert_eq!(operator_strategy(96, 1976), Some((3, 23, 23)));
        assert_eq!(operator_strategy(37, 1976), Some((1, 23, 20)));
        assert_eq!(operator_strategy(93, 1994), Some((3, 19, 19)));
        // 型をまたぐ日時の演算子は入れない（M4-Q60）。
        assert!(
            AMOPS
                .iter()
                .filter(|a| a.family == 434)
                .all(|a| a.left == a.right)
        );
        assert_eq!(operator_strategy(96, 1970), None);
    }

    #[test]
    fn amops_exist_in_operators_with_matching_shape() {
        for a in AMOPS {
            let op = builtin::OPERATORS
                .iter()
                .find(|o| o.oid == a.operator)
                .unwrap_or_else(|| panic!("operator {} missing", a.operator));
            let expected = ["<", "<=", "=", ">=", ">"][usize::from(a.strategy) - 1];
            assert_eq!(op.name, expected, "operator {}", a.operator);
            assert_eq!(op.left, Some(a.left), "operator {}", a.operator);
            assert_eq!(op.right, a.right, "operator {}", a.operator);
            assert_eq!(op.result, oid::BOOL);
        }
    }

    #[test]
    fn amprocs_exist_in_pg_proc() {
        for p in AMPROCS {
            let row = builtin::PROCS
                .iter()
                .find(|r| r.oid == p.proc_oid)
                .unwrap_or_else(|| panic!("proc {} missing", p.proc_oid));
            assert_eq!(row.args, &[p.left, p.right]);
            assert_eq!(row.result, oid::INT4);
            assert!(row.strict);
            assert_eq!(row.volatility, 'i');
            assert_eq!(Some(row.name), cmp_proc_name(p.proc_oid));
        }
        assert_eq!(cmp_proc_name(1), None);
    }

    #[test]
    fn default_opclass_lookup() {
        let name = |t| default_opclass(t).map(|c| c.name);
        assert_eq!(name(23), Some("int4_ops"));
        assert_eq!(name(1043), Some("text_ops"));
        assert_eq!(name(2205), Some("oid_ops"));
        assert_eq!(name(2206), Some("oid_ops"));
        assert_eq!(name(24), Some("oid_ops"));
        assert_eq!(name(19), Some("name_ops"));
        assert_eq!(name(1042), Some("bpchar_ops"));
        assert_eq!(name(1186), None); // interval
        assert_eq!(name(28), None); // xid
        assert_eq!(name(18), None); // "char"
        assert_eq!(family_of_type(1043), Some(1994));
        assert_eq!(family_of_type(23), Some(1976));
        assert_eq!(family_of_type(1186), None);
        let v = opclass_by_name("varchar_ops").unwrap();
        assert_eq!(v.input_type, 25);
        assert!(opclass_by_name("nope").is_none());
        assert!(opclass_by_oid(1).is_none());
    }

    #[test]
    fn comparators_resolve() {
        assert!(comparator(1976, 23, 20).is_some());
        assert!(comparator(1976, 23, 700).is_none());
        assert!(column_comparator(23, &Datum::Int8(1)).is_some());
        assert!(column_comparator(700, &Datum::Float8(1.0)).is_some());
        assert!(column_comparator(19, &Datum::Text("a".into())).is_some());
        assert!(column_comparator(25, &Datum::Text("a".into())).is_some());
        assert!(column_comparator(1043, &Datum::Text("a".into())).is_some());
        assert!(column_comparator(1082, &Datum::Int4(1)).is_none());
        assert!(column_comparator(1186, &Datum::Int4(1)).is_none());
        assert!(column_comparator(23, &Datum::Null).is_none());
    }

    fn samples(t: Oid) -> Vec<Datum> {
        use yuzhu_datetime::{Date, Timestamp, TimestampTz};
        match t {
            16 => vec![Datum::Bool(false), Datum::Bool(true)],
            21 => [i16::MIN, -1, 0, 1, 7, i16::MAX].map(Datum::Int2).to_vec(),
            23 => [i32::MIN, -5, 0, 1, 7, 70000, i32::MAX]
                .map(Datum::Int4)
                .to_vec(),
            20 => [i64::MIN, -5, 0, 1, 7, 70000, i64::MAX]
                .map(Datum::Int8)
                .to_vec(),
            700 => [
                f32::NEG_INFINITY,
                -1.5,
                0.0,
                1.0,
                7.0,
                f32::INFINITY,
                f32::NAN,
            ]
            .map(Datum::Float4)
            .to_vec(),
            701 => [
                f64::NEG_INFINITY,
                -1.5,
                0.0,
                1.0,
                7.0,
                1e300,
                f64::INFINITY,
                f64::NAN,
            ]
            .map(Datum::Float8)
            .to_vec(),
            1700 => ["-10.5", "0", "1.0", "1.00", "7", "123456789.123"]
                .iter()
                .map(|s| Datum::Numeric(yuzhu_numeric::Numeric::parse(s).unwrap()))
                .collect(),
            25 | 19 => ["", "a", "ab", "b", "B", "é"]
                .map(|s| Datum::Text(s.to_owned()))
                .to_vec(),
            1042 => ["a", "a  ", "ab", "b "]
                .map(|s| Datum::BpChar(s.to_owned()))
                .to_vec(),
            26 => [0u32, 1, 7, 4_000_000_000].map(Datum::Oid).to_vec(),
            1082 => [-100, 0, 1, 9000].map(|d| Datum::Date(Date(d))).to_vec(),
            1114 => [-100, 0, 1, 9_000_000_000]
                .map(|d| Datum::Timestamp(Timestamp(d)))
                .to_vec(),
            1184 => [-100, 0, 1, 9_000_000_000]
                .map(|d| Datum::TimestampTz(TimestampTz(d)))
                .to_vec(),
            _ => panic!("no samples for {t}"),
        }
    }

    #[test]
    fn operators_agree_with_cmp() {
        for a in AMOPS {
            let op = builtin::OPERATORS
                .iter()
                .find(|o| o.oid == a.operator)
                .expect("operator exists");
            let cmp = comparator(a.family, a.left, a.right).unwrap();
            for x in samples(a.left) {
                for y in samples(a.right) {
                    let ord = cmp(&x, &y);
                    let want = match a.strategy {
                        1 => ord == Ordering::Less,
                        2 => ord != Ordering::Greater,
                        3 => ord == Ordering::Equal,
                        4 => ord != Ordering::Less,
                        _ => ord == Ordering::Greater,
                    };
                    let got = (op.func)(&[x.clone(), y.clone()]).unwrap();
                    assert_eq!(
                        got,
                        Datum::Bool(want),
                        "operator {} ({}) on {x:?} {y:?}",
                        a.operator,
                        op.name
                    );
                }
            }
        }
    }

    #[test]
    fn all_opclass_cmp_matches_cmp_datum() {
        for p in AMPROCS {
            for x in samples(p.left) {
                for y in samples(p.right) {
                    assert_eq!((p.cmp)(&x, &y), cmp_datum(&x, &y));
                }
            }
        }
        for c in OPCLASSES {
            let f = comparator(c.family, c.input_type, c.input_type).unwrap();
            let s = samples(c.input_type);
            assert_eq!(f(&s[0], &s[s.len() - 1]), cmp_datum(&s[0], &s[s.len() - 1]));
        }
    }

    #[test]
    fn cmp_with_nulls_matches_comparator() {
        for c in OPCLASSES.iter().filter(|c| c.is_default) {
            let cmp = comparator(c.family, c.input_type, c.input_type).unwrap();
            let mut vals = samples(c.input_type);
            vals.push(Datum::Null);
            for desc in [false, true] {
                for nulls_first in [false, true] {
                    for x in &vals {
                        for y in &vals {
                            let want = match (x.is_null(), y.is_null()) {
                                (true, true) => Ordering::Equal,
                                (true, false) => {
                                    if nulls_first {
                                        Ordering::Less
                                    } else {
                                        Ordering::Greater
                                    }
                                }
                                (false, true) => {
                                    if nulls_first {
                                        Ordering::Greater
                                    } else {
                                        Ordering::Less
                                    }
                                }
                                (false, false) => {
                                    let o = cmp(x, y);
                                    if desc { o.reverse() } else { o }
                                }
                            };
                            assert_eq!(cmp_with_nulls(x, y, desc, nulls_first), want);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn datum_type_oids() {
        assert_eq!(datum_type_oid(&Datum::Int8(1)), Some(20));
        assert_eq!(datum_type_oid(&Datum::BpChar(String::new())), Some(1042));
        assert_eq!(datum_type_oid(&Datum::Null), None);
        assert_eq!(datum_type_oid(&Datum::Void), None);
    }
}
