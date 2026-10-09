//! Static tables of built-in types, casts, operators and functions.
//! OIDs are identical to PostgreSQL's. They are the runtime lookup tables
//! of the analyzer and, in M2, also the source of the initial contents of
//! `pg_type`, `pg_cast`, `pg_operator` and `pg_proc` (`catalog::rows`).
//!
//! `TYPES`, `CASTS`, `OPERATORS`, `FUNCTIONS` and `PROCS` use the
//! PostgreSQL 17 OIDs and attribute values (taken from a PostgreSQL 17
//! catalog; `pg_proc` rows that no table references are not listed).

//! 表の本体は `core`（M1〜M4 の行）、`numeric`・`datetime`・`text_misc`・`array`（M5 で各章が足す行）、
//! `runtime`（`FnKind::Runtime` などの本体）に分かれる。ここは型の定義・表の結合・検索関数。

mod array;
mod core;
mod datetime;
mod numeric;
mod runtime;
mod text_misc;
mod type_io;

pub use self::core::operator_merge_hash;
pub use self::runtime::format_type_name;
#[allow(clippy::wildcard_imports)]
use self::runtime::*;
pub use self::type_io::{TYPE_PROCS, TypeProcs, array_type_rows, type_procs};

use super::{CatalogReader, FnKind};
use crate::error::Result;
use crate::executor::{RuntimeInfo, SessionInfo};
use crate::types::funcs;
use crate::types::ops::{self, BuiltinFn};
use crate::types::{Datum, Oid, oid};
use std::sync::LazyLock;

/// 起動後に一度だけ組み立てる静的な表（`&'static [T]` のように使える）。`Copy` なので
/// `for t in TYPES`・`.iter()`・`TYPES[i..]`・`.chain(PROCS)` がそのまま書ける。
pub struct Table<T: 'static>(&'static LazyLock<Vec<T>>);

impl<T: 'static> std::fmt::Debug for Table<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Table(len = {})", self.len())
    }
}

impl<T: 'static> Clone for Table<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: 'static> Copy for Table<T> {}

impl<T: 'static> std::ops::Deref for Table<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.0.as_slice()
    }
}

impl<T: 'static> IntoIterator for Table<T> {
    type Item = &'static T;
    type IntoIter = std::slice::Iter<'static, T>;
    fn into_iter(self) -> Self::IntoIter {
        LazyLock::force(self.0).iter()
    }
}

impl<T: 'static> IntoIterator for &Table<T> {
    type Item = &'static T;
    type IntoIter = std::slice::Iter<'static, T>;
    fn into_iter(self) -> Self::IntoIter {
        LazyLock::force(self.0).iter()
    }
}

static CASTS_CELL: LazyLock<Vec<BuiltinCast>> = LazyLock::new(|| {
    self::core::CORE_CASTS
        .iter()
        .chain(datetime::TIME_CASTS)
        .cloned()
        .collect()
});
static OPERATORS_CELL: LazyLock<Vec<BuiltinOperator>> = LazyLock::new(|| {
    self::core::CORE_OPERATORS
        .iter()
        .chain(datetime::INTERVAL_OPERATORS)
        .chain(datetime::TIME_OPERATORS)
        .cloned()
        .collect()
});
static OPERATOR_META_CELL: LazyLock<Vec<OperatorMeta>> = LazyLock::new(|| {
    self::core::CORE_OPERATOR_META
        .iter()
        .chain(datetime::INTERVAL_OPERATOR_META)
        .chain(datetime::TIME_OPERATOR_META)
        .cloned()
        .collect()
});
static AGGREGATES_CELL: LazyLock<Vec<BuiltinAggregate>> = LazyLock::new(|| {
    self::core::CORE_AGGREGATES
        .iter()
        .chain(datetime::INTERVAL_AGGREGATES)
        .chain(datetime::TIME_AGGREGATES)
        .cloned()
        .collect()
});

/// `pg_cast` の全行: `core.rs` の行 + `datetime::TIME_CASTS`。
pub static CASTS: Table<BuiltinCast> = Table(&CASTS_CELL);
/// `pg_operator` の全行: `core.rs` の行 + interval・time の演算子。
pub static OPERATORS: Table<BuiltinOperator> = Table(&OPERATORS_CELL);
/// 演算子の `oprcom` などの付加情報。
pub static OPERATOR_META: Table<OperatorMeta> = Table(&OPERATOR_META_CELL);
/// 集約関数の全行: `core.rs` の行 + interval・time の `min` / `max`。
pub static AGGREGATES: Table<BuiltinAggregate> = Table(&AGGREGATES_CELL);

static TYPES_CELL: LazyLock<Vec<BuiltinType>> = LazyLock::new(type_io::merged_types);
static PROCS_CELL: LazyLock<Vec<BuiltinProc>> = LazyLock::new(|| {
    self::core::PROCS
        .iter()
        .chain(type_io::TYPE_IO_PROCS)
        .chain(numeric::PROCS)
        .chain(datetime::INTERVAL_PROCS)
        .chain(datetime::TIME_PROCS)
        .chain(datetime::TD3_PROCS)
        .cloned()
        .collect()
});
static FUNCTIONS_CELL: LazyLock<Vec<BuiltinFunction>> = LazyLock::new(|| {
    let mut v = self::core::FUNCTIONS.to_vec();
    v.extend_from_slice(numeric::FUNCTIONS);
    v.extend_from_slice(text_misc::FUNCTIONS);
    v.extend_from_slice(datetime::INTERVAL_FUNCTIONS);
    v.extend_from_slice(datetime::TIME_FUNCTIONS);
    v.extend_from_slice(datetime::TD3_FUNCTIONS);
    let casts = cast_functions(&v);
    v.extend(casts);
    v
});

/// `pg_type` の全行: `core.rs` の行 + `type_io::EXTRA_TYPES` + 配列型の行（`array_type_rows()`）。
pub static TYPES: Table<BuiltinType> = Table(&TYPES_CELL);
/// `pg_proc` の全行: `core.rs` の行 + `type_io::TYPE_IO_PROCS`。
pub static PROCS: Table<BuiltinProc> = Table(&PROCS_CELL);
/// 呼べる関数: `core.rs` の行 + 関数形式のキャスト（`cast_functions`）。
pub static FUNCTIONS: Table<BuiltinFunction> = Table(&FUNCTIONS_CELL);

/// 関数形式のキャスト（`int4(1.5)`、`bpchar('a'::name)`。`m5/05` §5.1.5）。
/// `CASTS` の行のうち `castfunc` が 0 でなく、`pg_proc` のその行の名前が変換先の型名のものを、
/// `existing` が持っていなければ関数として足す（strict。`volatility` は `PROCS` どおり）。
fn cast_functions(existing: &[BuiltinFunction]) -> Vec<BuiltinFunction> {
    let mut out: Vec<BuiltinFunction> = Vec::new();
    for c in CASTS {
        if c.func_oid == 0 || existing.iter().chain(&out).any(|f| f.oid == c.func_oid) {
            continue;
        }
        let (Some(p), Some(target)) = (proc_by_oid(c.func_oid), type_by_oid(c.target)) else {
            continue;
        };
        if p.name != target.name {
            continue;
        }
        let kind = match c.method {
            CastMethod::Function(f) => FnKind::Pure(f),
            CastMethod::Env(f) => FnKind::Env(f),
            CastMethod::Binary | CastMethod::InOut => continue,
        };
        out.push(BuiltinFunction {
            oid: p.oid,
            name: p.name,
            args: p.args,
            result: p.result,
            strict: p.strict,
            kind,
        });
    }
    out
}

/// A row of `pg_type`.
#[derive(Clone, Debug)]
pub struct BuiltinType {
    pub oid: Oid,
    /// `typname`.
    pub name: &'static str,
    /// `typlen` (`-1` varlena, `-2` C string).
    pub typlen: i16,
    pub typbyval: bool,
    /// `typtype`: `b` base, `c` composite, `p` pseudo.
    pub typtype: char,
    /// `typcategory` (`B` boolean, `N` numeric, `S` string, `X` unknown, ...).
    pub category: char,
    /// `typispreferred`.
    pub preferred: bool,
    /// `typarray` as in PostgreSQL (0 if none). `catalog::rows` writes 0 for
    /// types whose array type has no row in yuzhu (`m2.md` §1.3).
    pub array_oid: Oid,
    /// Names of the I/O functions (`typinput` / `typoutput`). The actual
    /// conversion is `types::io::{input_text, output_text}`.
    pub input: &'static str,
    pub output: &'static str,
    /// `typdelim`.
    pub delim: char,
    /// `typrelid` (the catalog of a composite type, else 0).
    pub relid: Oid,
    /// `typelem`.
    pub elem: Oid,
    /// `typalign`: `c`, `s`, `i` or `d`.
    pub align: char,
    /// `typstorage`: `p` plain, `x` extended, `m` main.
    pub storage: char,
    /// `typcollation`.
    pub collation: Oid,
    /// `pg_proc` OIDs of `typinput` / `typoutput`.
    pub input_oid: Oid,
    pub output_oid: Oid,
}

/// `pg_cast.castcontext`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CastContext {
    /// `i`: may be applied implicitly anywhere.
    Implicit,
    /// `a`: only in assignment (INSERT/UPDATE target) or explicitly.
    Assignment,
    /// `e`: only with an explicit cast.
    Explicit,
}

impl CastContext {
    /// The `pg_cast.castcontext` character.
    pub fn code(self) -> char {
        match self {
            CastContext::Implicit => 'i',
            CastContext::Assignment => 'a',
            CastContext::Explicit => 'e',
        }
    }
}

/// `pg_cast.castmethod`.
#[derive(Clone, Copy, Debug)]
pub enum CastMethod {
    /// `f`: call a function taking the source value.
    Function(BuiltinFn),
    /// `b`: binary-coercible; the datum is reused as-is.
    Binary,
    /// `i`: via the source type's output and target type's input functions.
    InOut,
    /// TypeEnv（TimeZone・now・regclass の名前）を使う変換。stable なので定数畳み込みしない
    /// （`m4/09-types-functions.md` D-9-4、11 §7.1 の C-14）。
    Env(fn(&[Datum], &crate::types::TypeEnv<'_>) -> Result<Datum>),
}

/// A row of `pg_cast`.
#[derive(Clone, Debug)]
pub struct BuiltinCast {
    pub source: Oid,
    pub target: Oid,
    pub context: CastContext,
    pub method: CastMethod,
    /// `castfunc` (0 for binary-coercible and I/O casts).
    pub func_oid: Oid,
    /// `castmethod` as `pg_cast` shows it. It differs from `method` for the
    /// binary-coercible casts between `Datum` variants (`int4` to `oid`),
    /// which run a function that reinterprets the bits.
    pub pg_method: char,
}

/// A row of `pg_operator`. Prefix operators have `left = None`.
#[derive(Clone, Debug)]
pub struct BuiltinOperator {
    pub oid: Oid,
    pub name: &'static str,
    pub left: Option<Oid>,
    pub right: Oid,
    pub result: Oid,
    pub func: BuiltinFn,
}

/// The `pg_operator` attributes that the runtime does not need, one entry per
/// operator of `OPERATORS` (same OID), kept apart so that `BuiltinOperator`
/// stays as the analyzer and executor use it.
#[derive(Clone, Debug)]
pub struct OperatorMeta {
    pub oid: Oid,
    /// `oprcode`: the `pg_proc` OID of the implementing function.
    pub proc_oid: Oid,
    /// `oprcom` / `oprnegate` as in PostgreSQL (0 if none). `catalog::rows`
    /// writes 0 when the other operator is not in `OPERATORS`.
    pub com: Oid,
    pub negate: Oid,
}

impl BuiltinOperator {
    /// `oprkind`: `b` infix, `l` prefix.
    pub fn kind(&self) -> char {
        if self.left.is_some() { 'b' } else { 'l' }
    }
}

/// A callable function. Its `pg_proc` attributes (`provolatile`, `prosrc`, ...)
/// are in the `PROCS` row with the same OID.
#[derive(Clone, Debug)]
pub struct BuiltinFunction {
    pub oid: Oid,
    pub name: &'static str,
    pub args: &'static [Oid],
    pub result: Oid,
    pub strict: bool,
    pub kind: FnKind,
}

/// A row of `pg_proc` for a function that no SQL can call by itself but that
/// the other tables point to: operator functions (`oprcode`), cast functions,
/// type I/O functions, access method handlers. The user-callable functions
/// are in `FUNCTIONS`; `PROCS` lists both so that `pg_proc` is generated from
/// one table and `regproc` output can name any of them.
#[derive(Clone, Debug)]
pub struct BuiltinProc {
    pub oid: Oid,
    pub name: &'static str,
    pub args: &'static [Oid],
    pub result: Oid,
    pub strict: bool,
    /// `provolatile`.
    pub volatility: char,
    /// `proparallel`.
    pub parallel: char,
    pub leakproof: bool,
    /// `procost`.
    pub cost: f32,
    pub prosrc: &'static str,
}

#[allow(clippy::too_many_arguments)]
const fn ty(
    oid: Oid,
    name: &'static str,
    typlen: i16,
    typbyval: bool,
    typtype: char,
    category: char,
    preferred: bool,
    delim: char,
    relid: Oid,
    elem: Oid,
    array_oid: Oid,
    input: (&'static str, Oid),
    output: (&'static str, Oid),
    align: char,
    storage: char,
    collation: Oid,
) -> BuiltinType {
    BuiltinType {
        oid,
        name,
        typlen,
        typbyval,
        typtype,
        category,
        preferred,
        array_oid,
        input: input.0,
        output: output.0,
        delim,
        relid,
        elem,
        align,
        storage,
        collation,
        input_oid: input.1,
        output_oid: output.1,
    }
}

/// OID of `numeric` (not implemented yet; see `TYPES`).
pub const NUMERIC: Oid = 1700;
/// OID of `date` (not implemented yet).
pub const DATE: Oid = 1082;
/// OID of `interval` (not implemented yet).
pub const INTERVAL: Oid = 1186;
/// OID of the pseudo-type `anynonarray` (polymorphic argument).
pub const ANYNONARRAY: Oid = 2776;

/// Whether `oid` is a polymorphic pseudo-type.
pub fn is_polymorphic(oid: Oid) -> bool {
    oid == ANYNONARRAY
}

/// Whether values of the type can exist in yuzhu (have a `Datum`
/// representation and I/O functions).
pub fn is_supported_type(t: Oid) -> bool {
    matches!(
        t,
        oid::BOOL
            | oid::CHAR
            | oid::INT2
            | oid::INT4
            | oid::INT8
            | oid::FLOAT4
            | oid::FLOAT8
            | oid::NUMERIC
            | oid::TEXT
            | oid::VARCHAR
            | oid::BPCHAR
            | oid::BYTEA
            | oid::UUID
            | oid::DATE
            | oid::TIMESTAMP
            | oid::TIMESTAMPTZ
            | oid::REGCLASS
            | oid::REGTYPE
            | oid::REGNAMESPACE
            | oid::NAME
            | oid::UNKNOWN
            | oid::OID
            | oid::REGPROC
            | oid::TID
            | oid::XID
            | oid::CID
            | oid::OIDVECTOR
            | oid::INT2VECTOR
            | oid::PG_NODE_TREE
            | oid::INT4_ARRAY
            | oid::VOID
    )
}

/// Types that exist so that catalog columns can be declared, but whose
/// columns only ever hold NULL in M2 (`aclitem`, `timestamptz`, `anyarray`
/// and the array types of `m2.md` §1.3). Storing anything else is `0A000`.
pub fn is_null_only_type(t: Oid) -> bool {
    if matches!(t, oid::ACLITEM | oid::ANYARRAY) {
        return true;
    }
    // 配列型の行（`array_type_rows()`）は、値を持てるようになるまで（TY-5）NULL 専用。
    !is_supported_type(t) && type_by_oid(t).is_some_and(|r| r.category == 'A' && r.elem != 0)
}

/// `cast` with `castmethod` taken from `method`.
const fn cast(
    source: Oid,
    target: Oid,
    context: CastContext,
    method: CastMethod,
    func_oid: Oid,
) -> BuiltinCast {
    let pg_method = match method {
        CastMethod::Function(_) | CastMethod::Env(_) => 'f',
        CastMethod::Binary => 'b',
        CastMethod::InOut => 'i',
    };
    BuiltinCast {
        source,
        target,
        context,
        method,
        func_oid,
        pg_method,
    }
}

/// A function cast (`castmethod = 'f'`).
const fn cast_fn(
    source: Oid,
    target: Oid,
    context: CastContext,
    f: BuiltinFn,
    func_oid: Oid,
) -> BuiltinCast {
    cast(source, target, context, CastMethod::Function(f), func_oid)
}

/// A cast that PostgreSQL defines as binary-coercible (`castmethod = 'b'`)
/// but whose `Datum` variant changes (`int4` to `oid`), so it runs `f`.
const fn cast_bin(source: Oid, target: Oid, context: CastContext, f: BuiltinFn) -> BuiltinCast {
    BuiltinCast {
        source,
        target,
        context,
        method: CastMethod::Function(f),
        func_oid: 0,
        pg_method: 'b',
    }
}

/// `left = 0` makes a prefix operator.
const fn op(
    oid: Oid,
    name: &'static str,
    left: Oid,
    right: Oid,
    result: Oid,
    func: BuiltinFn,
) -> BuiltinOperator {
    BuiltinOperator {
        oid,
        name,
        left: if left == 0 { None } else { Some(left) },
        right,
        result,
        func,
    }
}

/// A strict function with a pure body.
const fn func(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    func: BuiltinFn,
) -> BuiltinFunction {
    BuiltinFunction {
        oid,
        name,
        args,
        result,
        strict: true,
        kind: FnKind::Pure(func),
    }
}

/// A function that reads the catalog or the session (`FnKind::Context`).
const fn ctx_func(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    f: fn(&[Datum], &dyn CatalogReader, &SessionInfo) -> Result<Datum>,
) -> BuiltinFunction {
    BuiltinFunction {
        oid,
        name,
        args,
        result,
        strict: true,
        kind: FnKind::Context(f),
    }
}

/// A strict function that reads the execution environment
/// (`FnKind::Runtime`).
const fn runtime_func(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    f: fn(&[Datum], &dyn RuntimeInfo) -> Result<Datum>,
) -> BuiltinFunction {
    BuiltinFunction {
        oid,
        name,
        args,
        result,
        strict: true,
        kind: FnKind::Runtime(f),
    }
}

/// Like [`runtime_func`], but called even when an argument is NULL.
const fn nonstrict_runtime_func(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    f: fn(&[Datum], &dyn RuntimeInfo) -> Result<Datum>,
) -> BuiltinFunction {
    BuiltinFunction {
        oid,
        name,
        args,
        result,
        strict: false,
        kind: FnKind::Runtime(f),
    }
}

/// A function that is called even when an argument is NULL.
const fn nonstrict_func(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    func: BuiltinFn,
) -> BuiltinFunction {
    BuiltinFunction {
        oid,
        name,
        args,
        result,
        strict: false,
        kind: FnKind::Pure(func),
    }
}

// ----- lookups ---------------------------------------------------------------

pub fn type_by_oid(oid: Oid) -> Option<&'static BuiltinType> {
    TYPES.iter().find(|t| t.oid == oid)
}

pub fn type_by_name(name: &str) -> Option<&'static BuiltinType> {
    TYPES.iter().find(|t| t.name == name)
}

pub fn find_cast(source: Oid, target: Oid) -> Option<&'static BuiltinCast> {
    CASTS
        .iter()
        .find(|c| c.source == source && c.target == target)
}

pub fn operators_named(name: &str) -> Vec<&'static BuiltinOperator> {
    OPERATORS.iter().filter(|o| o.name == name).collect()
}

pub fn functions_named(name: &str) -> Vec<&'static BuiltinFunction> {
    FUNCTIONS.iter().filter(|f| f.name == name).collect()
}

/// The `OPERATORS` row with this OID.
pub fn operator_by_oid(oid: Oid) -> Option<&'static BuiltinOperator> {
    OPERATORS.iter().find(|o| o.oid == oid)
}

/// The `pg_proc` row with this OID (any function in `PROCS`).
pub fn proc_by_oid(oid: Oid) -> Option<&'static BuiltinProc> {
    PROCS.iter().find(|p| p.oid == oid)
}

/// The display name of a function OID for `regproc` output (PostgreSQL's
/// `regprocout`, `m2.md` §4.2): the bare name when no other function has the
/// same name, `pg_catalog.name` when overloaded, `None` for an OID that is
/// not in `PROCS` (shown as a number). 0 (`-`) is the caller's job.
pub fn regproc_name(oid: Oid) -> Option<String> {
    let p = proc_by_oid(oid)?;
    let overloaded = PROCS.iter().filter(|q| q.name == p.name).count() > 1;
    Some(if overloaded {
        format!("pg_catalog.{}", p.name)
    } else {
        p.name.to_owned()
    })
}

/// The `pg_operator` attributes of an operator of `OPERATORS`.
pub fn operator_meta(oid: Oid) -> Option<&'static OperatorMeta> {
    OPERATOR_META.iter().find(|m| m.oid == oid)
}

// 以降は本体コード（関数・補助表）の区画。各担当は自分の区画の中だけを編集する（m4/11-tests-plan.md §4.2）。
// ===== region: numeric (T1) =====
// ===== region: datetime-bpchar (T2) =====
// ===== region: agg-set-sys (T3) =====
/// `generate_series`（`FnKind::Set`。FROM 句でだけ呼べる。`m4/09-types-functions.md` §10.1）。
const fn generate_series_fn(
    oid: Oid,
    args: &'static [Oid],
    result: Oid,
    begin: fn(&[Datum]) -> Result<Box<dyn super::SetIter>>,
) -> BuiltinFunction {
    BuiltinFunction {
        oid,
        name: "generate_series",
        args,
        result,
        strict: true,
        kind: FnKind::Set(super::SetFn {
            begin,
            column_name: "generate_series",
        }),
    }
}

pub use super::{AggKind, BuiltinAggregate};

/// `pg_type` の `"any"`（`count(any)` の引数）。
const ANY_OID: Oid = 2276;

const fn agg(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    kind: AggKind,
) -> BuiltinAggregate {
    BuiltinAggregate {
        oid,
        name,
        args,
        result,
        kind,
    }
}

/// 名前が一致する集約関数（`pg_catalog` の組み込みだけ）。
pub fn aggregates_named(name: &str) -> Vec<&'static BuiltinAggregate> {
    AGGREGATES.iter().filter(|a| a.name == name).collect()
}

#[cfg(test)]
mod t3_tests {
    use super::*;

    #[test]
    fn format_type_name_matches_postgresql() {
        let f = format_type_name;
        for (oid, tm, want) in [
            (1700, Some(-1), "numeric"),
            (1700, Some(655_366), "numeric(10,2)"),
            (1700, Some(4), "numeric(0,0)"),
            (1700, None, "numeric"),
            (1042, Some(-1), "bpchar"),
            (1042, None, "character"),
            (1042, Some(5), "character(1)"),
            (1042, Some(4), "character"),
            (1114, Some(-1), "timestamp without time zone"),
            (1114, Some(3), "timestamp(3) without time zone"),
            (1184, Some(-1), "timestamp with time zone"),
            (1184, Some(0), "timestamp(0) with time zone"),
            (1114, None, "timestamp without time zone"),
            (22, None, "int2vector"),
            (2205, None, "regclass"),
            (2206, None, "regtype"),
            (25, Some(5), "text(5)"),
            (1009, None, "text[]"),
        ] {
            assert_eq!(f(oid, tm), want, "{oid} {tm:?}");
        }
        // numeric(10,-2): the scale is a signed 11-bit field.
        let tm = ((10 << 16) | (-2_i32 & 0x7ff)) + 4;
        assert_eq!(f(1700, Some(tm)), "numeric(10,-2)");
        assert_eq!(crate::types::type_display_name(1700), "numeric");
        assert_eq!(crate::types::type_display_name(1042), "character");
        assert_eq!(
            crate::types::type_display_name(1114),
            "timestamp without time zone"
        );
        assert_eq!(
            crate::types::type_display_name(1184),
            "timestamp with time zone"
        );
    }

    #[test]
    fn operator_lookup_and_regex_operators() {
        assert_eq!(operator_by_oid(639).unwrap().name, "~");
        assert!(operator_by_oid(1).is_none());
        for (o, proc_oid, negate) in [
            (639, 79, 640),
            (640, 1252, 639),
            (641, 1254, 642),
            (642, 1256, 641),
            (1226, 1240, 1227),
            (1227, 1241, 1226),
            (1228, 1238, 1229),
            (1229, 1239, 1228),
        ] {
            let m = operator_meta(o).unwrap();
            assert_eq!((m.proc_oid, m.negate), (proc_oid, negate), "{o}");
            assert!(proc_by_oid(proc_oid).is_some());
        }
        let name_eq = operator_by_oid(639).unwrap();
        let args = [Datum::Text("pg_class".into()), Datum::Text("^pg_".into())];
        assert_eq!((name_eq.func)(&args).unwrap(), Datum::Bool(true));
        let name_ne = operator_by_oid(1227).unwrap();
        assert_eq!((name_ne.func)(&args).unwrap(), Datum::Bool(false));
    }

    #[test]
    fn generate_series_rows_are_set_functions() {
        for o in [1066, 1067, 1068, 1069] {
            let f = FUNCTIONS.iter().find(|f| f.oid == o).unwrap();
            assert!(matches!(f.kind, FnKind::Set(_)), "{o}");
            assert_eq!(f.name, "generate_series");
            assert!(proc_by_oid(o).is_some());
        }
        // int4・int8・numeric・timestamp（TD3）。
        assert_eq!(functions_named("generate_series").len(), 5);
    }
}
// ===== region: sequence (Q1) =====
// ===== region: btree-procs (B2) =====
// ===== region: explain-deparse (E1) =====

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use super::*;
    use crate::catalog::TableDef;
    use crate::catalog::fake::table_def;
    use crate::error::Error;

    #[derive(Debug)]
    struct FakeRuntime {
        pid: i32,
        blocked_by: Vec<i32>,
        /// `check_interrupts` fails from this call on.
        cancel_after: std::cell::Cell<Option<u32>>,
        checks: std::cell::Cell<u32>,
    }

    impl FakeRuntime {
        fn new(pid: i32, blocked_by: Vec<i32>) -> Self {
            FakeRuntime {
                pid,
                blocked_by,
                cancel_after: std::cell::Cell::new(None),
                checks: std::cell::Cell::new(0),
            }
        }
    }

    impl RuntimeInfo for FakeRuntime {
        fn backend_pid(&self) -> i32 {
            self.pid
        }

        fn is_blocked_by(&self, pid: i32, among: &[i32]) -> bool {
            pid == self.pid && among.iter().any(|p| self.blocked_by.contains(p))
        }

        fn blocking_pids(&self, pid: i32) -> Vec<i32> {
            if pid == self.pid {
                self.blocked_by.clone()
            } else {
                Vec::new()
            }
        }

        fn check_interrupts(&self) -> Result<()> {
            let n = self.checks.get() + 1;
            self.checks.set(n);
            match self.cancel_after.get() {
                Some(limit) if n > limit => Err(Error::new(
                    crate::error::sqlstate::QUERY_CANCELED,
                    "canceling statement due to user request",
                )),
                _ => Ok(()),
            }
        }
    }

    fn call_runtime(oid: Oid, args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum> {
        let f = FUNCTIONS.iter().find(|f| f.oid == oid).unwrap();
        assert!(f.strict);
        let FnKind::Runtime(func) = f.kind else {
            panic!("not a runtime function");
        };
        func(args, rt)
    }

    #[test]
    fn backend_functions_are_runtime_kind() {
        for (oid, name, volatility) in [
            (2026, "pg_backend_pid", 's'),
            (2626, "pg_sleep", 'v'),
            (3378, "pg_isolation_test_session_is_blocked", 'v'),
        ] {
            assert!(matches!(functions_named(name)[0].kind, FnKind::Runtime(_)));
            let p = proc_by_oid(oid).unwrap();
            assert_eq!((p.name, p.volatility), (name, volatility));
        }
        assert_eq!(type_by_oid(oid::VOID).unwrap().typtype, 'p');
        assert_eq!(type_by_oid(oid::INT4_ARRAY).unwrap().elem, oid::INT4);
        assert_eq!(type_by_oid(oid::INT4).unwrap().array_oid, oid::INT4_ARRAY);
        assert_eq!(format_type_name(oid::INT4_ARRAY, None), "integer[]");
        assert_eq!(format_type_name(oid::VOID, None), "void");
    }

    #[test]
    fn pg_backend_pid_and_blocked() {
        let rt = FakeRuntime::new(4242, vec![7]);
        assert_eq!(call_runtime(2026, &[], &rt).unwrap(), Datum::Int4(4242));
        let blocked = |pid, among: Vec<Option<i32>>| {
            call_runtime(3378, &[Datum::Int4(pid), Datum::Int4Array(among)], &rt).unwrap()
        };
        assert_eq!(blocked(4242, vec![Some(7)]), Datum::Bool(true));
        let null_err = call_runtime(
            3378,
            &[Datum::Int4(4242), Datum::Int4Array(vec![None, Some(7)])],
            &rt,
        )
        .unwrap_err();
        assert_eq!(null_err.sqlstate, crate::error::sqlstate::INTERNAL_ERROR);
        assert_eq!(null_err.message, "array must not contain nulls");
        assert_eq!(blocked(4242, vec![Some(8)]), Datum::Bool(false));
        assert_eq!(blocked(4242, vec![]), Datum::Bool(false));
        assert_eq!(blocked(1, vec![Some(7)]), Datum::Bool(false));
    }

    #[test]
    fn pg_blocking_pids_returns_the_runtime_pids_as_int4_array() {
        let rt = FakeRuntime::new(4242, vec![7, 9]);
        let call = |pid| call_runtime(2561, &[Datum::Int4(pid)], &rt).unwrap();
        assert_eq!(call(4242), Datum::Int4Array(vec![Some(7), Some(9)]));
        assert_eq!(call(1), Datum::Int4Array(vec![]));
        let p = proc_by_oid(2561).unwrap();
        assert_eq!(
            (p.name, p.args, p.result),
            ("pg_blocking_pids", &[23][..], 1007)
        );
        assert_eq!((p.volatility, p.parallel), ('v', 's'));
    }

    #[test]
    fn pg_sleep_returns_void_and_checks_interrupts() {
        let rt = FakeRuntime::new(1, vec![]);
        for secs in [0.0, -5.0, f64::NAN, f64::NEG_INFINITY] {
            assert_eq!(
                call_runtime(2626, &[Datum::Float8(secs)], &rt).unwrap(),
                Datum::Void
            );
        }
        assert_eq!(rt.checks.get(), 0);
        let start = std::time::Instant::now();
        assert_eq!(
            call_runtime(2626, &[Datum::Float8(0.05)], &rt).unwrap(),
            Datum::Void
        );
        assert!(start.elapsed() >= std::time::Duration::from_millis(50));
        assert!(rt.checks.get() >= 2);
    }

    #[test]
    fn pg_sleep_is_cancelable() {
        let rt = FakeRuntime::new(1, vec![]);
        rt.cancel_after.set(Some(3));
        let start = std::time::Instant::now();
        let e = call_runtime(2626, &[Datum::Float8(60.0)], &rt).unwrap_err();
        assert_eq!(e.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        let e = call_runtime(2626, &[Datum::Float8(f64::INFINITY)], &rt).unwrap_err();
        assert_eq!(e.sqlstate, crate::error::sqlstate::QUERY_CANCELED);
    }

    #[test]
    fn type_lookup() {
        assert_eq!(type_by_name("int4").unwrap().oid, oid::INT4);
        assert_eq!(type_by_oid(oid::VARCHAR).unwrap().name, "varchar");
        assert_eq!(type_by_oid(oid::UNKNOWN).unwrap().typlen, -2);
        assert!(type_by_name("integer").is_none());
        assert_eq!(type_by_name("char").unwrap().oid, oid::CHAR);
        assert_eq!(type_by_oid(oid::TID).unwrap().typlen, 6);
        assert_eq!(type_by_oid(oid::OID).unwrap().input_oid, 1798);
    }

    fn all_unique<T: std::hash::Hash + Eq>(items: impl Iterator<Item = T>) -> bool {
        let mut seen = HashSet::new();
        items.into_iter().all(|i| seen.insert(i))
    }

    #[test]
    fn oids_are_unique() {
        assert!(all_unique(TYPES.iter().map(|t| t.oid)));
        assert!(all_unique(TYPES.iter().map(|t| t.name)));
        assert!(all_unique(OPERATORS.iter().map(|o| o.oid)));
        assert!(all_unique(FUNCTIONS.iter().map(|f| f.oid)));
        assert!(all_unique(PROCS.iter().map(|p| p.oid)));
        assert!(all_unique(OPERATOR_META.iter().map(|m| m.oid)));
        assert!(all_unique(CASTS.iter().map(|c| (c.source, c.target))));
        // PROCS は区画ごとに追記するので OID 順は要求しない（`proc_by_oid` は線形探索）。
    }

    #[test]
    fn functions_agree_with_procs() {
        for f in FUNCTIONS {
            let p = proc_by_oid(f.oid).unwrap_or_else(|| panic!("no PROCS row for {}", f.name));
            assert_eq!(p.name, f.name, "{}", f.oid);
            assert_eq!(p.args, f.args, "{}", f.name);
            assert_eq!(p.result, f.result, "{}", f.name);
            assert_eq!(p.strict, f.strict, "{}", f.name);
        }
    }

    #[test]
    fn operators_have_meta_and_procs() {
        for o in OPERATORS {
            let m =
                operator_meta(o.oid).unwrap_or_else(|| panic!("no meta for operator {}", o.oid));
            let p =
                proc_by_oid(m.proc_oid).unwrap_or_else(|| panic!("no proc for operator {}", o.oid));
            assert_eq!(p.result, o.result, "operator {} {}", o.oid, o.name);
            let expected_args: Vec<Oid> = o.left.into_iter().chain([o.right]).collect();
            assert_eq!(
                p.args,
                expected_args.as_slice(),
                "operator {} {}",
                o.oid,
                o.name
            );
        }
        assert_eq!(OPERATORS.len(), OPERATOR_META.len());
    }

    #[test]
    fn operators_refer_to_known_types() {
        for o in OPERATORS {
            for t in o.left.into_iter().chain([o.right, o.result]) {
                assert!(type_by_oid(t).is_some(), "operator {} uses type {t}", o.oid);
            }
        }
    }

    #[test]
    fn casts_are_consistent() {
        for c in CASTS {
            assert!(type_by_oid(c.source).is_some(), "{}", c.source);
            assert!(type_by_oid(c.target).is_some(), "{}", c.target);
            match c.pg_method {
                'f' => {
                    assert!(matches!(
                        c.method,
                        CastMethod::Function(_) | CastMethod::Env(_)
                    ));
                    assert_ne!(c.func_oid, 0, "{}->{}", c.source, c.target);
                }
                'b' => assert_eq!(c.func_oid, 0, "{}->{}", c.source, c.target),
                'i' => assert!(matches!(c.method, CastMethod::InOut)),
                other => panic!("castmethod {other}"),
            }
            if c.func_oid != 0 {
                assert!(
                    proc_by_oid(c.func_oid).is_some(),
                    "{}->{}",
                    c.source,
                    c.target
                );
            }
        }
        let c = find_cast(oid::INT4, oid::OID).unwrap();
        assert_eq!(c.pg_method, 'b');
        assert_eq!(c.context, CastContext::Implicit);
        let c = find_cast(oid::OID, oid::INT8).unwrap();
        assert_eq!(
            (c.pg_method, c.func_oid, c.context),
            ('f', 1288, CastContext::Assignment)
        );
        let c = find_cast(oid::OID, oid::REGPROC).unwrap();
        assert!(matches!(c.method, CastMethod::Binary));
        assert_eq!(CastContext::Explicit.code(), 'e');
    }

    #[test]
    fn types_refer_to_known_rows() {
        for t in TYPES {
            if t.elem != 0 {
                assert!(type_by_oid(t.elem).is_some(), "{}", t.name);
            }
            assert!(proc_by_oid(t.input_oid).is_some(), "{}", t.name);
            assert!(proc_by_oid(t.output_oid).is_some(), "{}", t.name);
            assert_eq!(proc_by_oid(t.input_oid).unwrap().name, t.input);
            assert_eq!(proc_by_oid(t.output_oid).unwrap().name, t.output);
            assert!(matches!(t.align, 'c' | 's' | 'i' | 'd'), "{}", t.name);
            assert!(matches!(t.storage, 'p' | 'x' | 'm' | 'e'), "{}", t.name);
        }
        // Values of every type a catalog column can hold must be storable.
        for c in crate::catalog::schema::CATALOGS
            .iter()
            .flat_map(|d| d.columns)
        {
            assert!(type_by_oid(c.type_oid).is_some(), "{}", c.name);
            assert!(
                is_supported_type(c.type_oid) || is_null_only_type(c.type_oid),
                "{} has type {}",
                c.name,
                c.type_oid
            );
        }
    }

    #[test]
    fn system_types_are_supported() {
        for t in [
            oid::CHAR,
            oid::OID,
            oid::REGPROC,
            oid::TID,
            oid::XID,
            oid::CID,
            oid::OIDVECTOR,
            oid::PG_NODE_TREE,
        ] {
            assert!(is_supported_type(t), "{t}");
            assert!(!is_null_only_type(t), "{t}");
        }
        for t in [oid::ACLITEM, oid::ANYARRAY, oid::TEXT_ARRAY, oid::OID_ARRAY] {
            assert!(!is_supported_type(t), "{t}");
            assert!(is_null_only_type(t), "{t}");
        }
        assert!(is_supported_type(NUMERIC));
    }

    #[test]
    fn name_lookups() {
        assert_eq!(
            operators_named("=")
                .iter()
                .filter(|o| o.right == oid::OID)
                .count(),
            1
        );
        assert_eq!(
            operators_named("=")
                .iter()
                .filter(|o| o.right == oid::TEXT)
                .count(),
            2
        );
        assert_eq!(functions_named("format_type").len(), 1);
        assert!(!functions_named("format_type")[0].strict);
        assert!(functions_named("pg_get_userbyid")[0].strict);
        assert!(find_cast(oid::TEXT, oid::CHAR).is_some());
        assert!(find_cast(oid::BOOL, oid::OID).is_none());
    }

    #[test]
    fn operator_oids_follow_postgresql() {
        // int4 + int8 is int48pl (692); int8 + int4 is int84pl (688).
        let find = |l, r, name: &str| {
            operators_named(name)
                .into_iter()
                .find(|o| o.left == Some(l) && o.right == r)
                .map(|o| o.oid)
        };
        assert_eq!(find(oid::INT4, oid::INT8, "+"), Some(692));
        assert_eq!(find(oid::INT8, oid::INT4, "+"), Some(688));
        assert_eq!(find(oid::INT2, oid::INT4, "/"), Some(546));
        assert_eq!(find(oid::OID, oid::OID, "="), Some(607));
        assert_eq!(operator_meta(551).unwrap().proc_oid, 177);
        assert_eq!(operator_meta(96).unwrap().proc_oid, 65);
    }

    #[test]
    fn regproc_names() {
        assert_eq!(regproc_name(42).as_deref(), Some("int4in"));
        assert_eq!(regproc_name(1242).as_deref(), Some("boolin"));
        // `float8` exists for several argument types.
        assert_eq!(regproc_name(316).as_deref(), Some("pg_catalog.float8"));
        assert_eq!(regproc_name(99_999), None);
        assert_eq!(regproc_name(0), None);
    }

    #[test]
    fn format_type_names() {
        let f = |t, m| format_type_name(t, m);
        assert_eq!(f(16, None), "boolean");
        assert_eq!(f(23, None), "integer");
        assert_eq!(f(20, None), "bigint");
        assert_eq!(f(21, None), "smallint");
        assert_eq!(f(25, None), "text");
        assert_eq!(f(18, None), "\"char\"");
        assert_eq!(f(19, None), "name");
        assert_eq!(f(26, None), "oid");
        assert_eq!(f(700, None), "real");
        assert_eq!(f(701, None), "double precision");
        assert_eq!(f(1043, None), "character varying");
        assert_eq!(f(1043, Some(14)), "character varying(10)");
        assert_eq!(f(1043, Some(5)), "character varying(1)");
        assert_eq!(f(1043, Some(4)), "character varying");
        assert_eq!(f(1043, Some(-1)), "character varying");
        assert_eq!(f(99_999, None), "???");
        assert_eq!(f(0, None), "-");
        assert_eq!(f(1009, None), "text[]");
        assert_eq!(f(1028, None), "oid[]");
        assert_eq!(f(30, None), "oidvector");
        assert_eq!(f(25, Some(5)), "text(5)");
        assert_eq!(f(23, Some(5)), "integer");
        assert_eq!(f(701, Some(5)), "double precision");
        assert_eq!(f(20, Some(3)), "bigint");
        assert_eq!(f(16, Some(5)), "boolean");
        assert_eq!(f(21, Some(5)), "smallint");
        assert_eq!(f(700, Some(5)), "real");
        assert_eq!(f(NUMERIC, Some(655_366)), "numeric(10,2)");
        assert_eq!(f(2249, None), "record");
        assert_eq!(f(83, None), "pg_class");
        assert_eq!(f(TIMESTAMPTZ, None), "timestamp with time zone");
    }

    const TIMESTAMPTZ: Oid = oid::TIMESTAMPTZ;

    #[test]
    fn format_type_function_is_not_strict() {
        let call = |a: Datum, b: Datum| format_type(&[a, b]).unwrap();
        assert_eq!(
            call(Datum::Oid(23), Datum::Null),
            Datum::Text("integer".into())
        );
        assert_eq!(call(Datum::Null, Datum::Null), Datum::Null);
        assert_eq!(call(Datum::Null, Datum::Int4(14)), Datum::Null);
        assert_eq!(
            call(Datum::Oid(1043), Datum::Int4(14)),
            Datum::Text("character varying(10)".into())
        );
        assert!(format_type(&[Datum::Oid(1)]).is_err());
    }

    /// A catalog with named roles and tables in `pg_catalog` and `public`.
    #[derive(Debug, Default)]
    struct TestCatalog {
        roles: HashMap<Oid, String>,
        tables: Vec<Arc<TableDef>>,
        /// Indexes of the tables (`public`, or `pg_catalog` when the namespace is 11).
        indexes: Vec<Arc<crate::catalog::IndexDef>>,
    }

    impl CatalogReader for TestCatalog {
        fn table(&self, schema: Option<&str>, name: &str) -> Result<Option<Arc<TableDef>>> {
            let order = ["pg_catalog", "public"];
            Ok(order
                .iter()
                .filter(|s| schema.is_none_or(|x| x == **s))
                .find_map(|s| {
                    self.tables
                        .iter()
                        .find(|t| t.schema == *s && t.name == name)
                })
                .cloned())
        }
        fn table_by_oid(&self, oid: Oid) -> Result<Option<Arc<TableDef>>> {
            Ok(self.tables.iter().find(|t| t.oid == oid).cloned())
        }
        fn current_database(&self) -> &'static str {
            "postgres"
        }
        fn search_path(&self) -> &[String] {
            &[]
        }
        fn role_name(&self, oid: Oid) -> Result<Option<String>> {
            Ok(self.roles.get(&oid).cloned())
        }
        fn visible_namespaces(&self) -> Result<Vec<Oid>> {
            Ok(vec![11, 2200])
        }
        fn relation_kind(
            &self,
            schema: Option<&str>,
            name: &str,
        ) -> Result<Option<(Oid, crate::catalog::RelKind)>> {
            if let Some(t) = self.table(schema, name)? {
                return Ok(Some((t.oid, crate::catalog::RelKind::Table)));
            }
            let order = [(11, "pg_catalog"), (2200, "public")];
            Ok(order
                .iter()
                .filter(|(_, s)| schema.is_none_or(|x| x == *s))
                .find_map(|(nsp, _)| {
                    self.indexes
                        .iter()
                        .find(|i| i.namespace == *nsp && i.name == name)
                })
                .map(|i| (i.oid, crate::catalog::RelKind::Index)))
        }
        fn index_by_oid(&self, oid: Oid) -> Result<Option<Arc<crate::catalog::IndexDef>>> {
            Ok(self.indexes.iter().find(|i| i.oid == oid).cloned())
        }
    }

    fn session() -> SessionInfo {
        SessionInfo {
            current_user: "postgres".into(),
            session_user: "postgres".into(),
            database: "postgres".into(),
            current_schema: Some("public".into()),
        }
    }

    fn table_in(schema: &str, oid: Oid, name: &str) -> Arc<TableDef> {
        let mut t = table_def(oid, name, vec![], vec![]);
        t.schema = schema.to_owned();
        t.namespace = if schema == "public" { 2200 } else { 11 };
        Arc::new(t)
    }

    #[test]
    fn pg_get_userbyid_names_roles() {
        let mut cat = TestCatalog::default();
        cat.roles.insert(10, "postgres".into());
        let s = session();
        assert_eq!(
            pg_get_userbyid(&[Datum::Oid(10)], &cat, &s).unwrap(),
            Datum::Text("postgres".into())
        );
        assert_eq!(
            pg_get_userbyid(&[Datum::Oid(99_999)], &cat, &s).unwrap(),
            Datum::Text("unknown (OID=99999)".into())
        );
        let e: Error = pg_get_userbyid(&[Datum::Int4(1)], &cat, &s).unwrap_err();
        assert_eq!(e.sqlstate, crate::error::sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn pg_table_is_visible_follows_the_search_path() {
        let cat = TestCatalog {
            roles: HashMap::new(),
            tables: vec![
                table_in("pg_catalog", 1259, "pg_class"),
                table_in("public", 16384, "t"),
                // Shadowed by the pg_catalog table of the same name.
                table_in("public", 16385, "pg_class"),
                table_in("public", 16386, "u"),
            ],
            indexes: vec![
                index_in(11, 2662, "pg_class_oid_index", 1259),
                index_in(2200, 16390, "t_pkey", 16384),
                // Shadowed by a table of the same name found first.
                index_in(2200, 16391, "u", 16386),
                // Shadowed by the pg_catalog index of the same name.
                index_in(2200, 16392, "pg_class_oid_index", 16384),
            ],
        };
        let s = session();
        let call = |o: Oid| pg_table_is_visible(&[Datum::Oid(o)], &cat, &s).unwrap();
        assert_eq!(call(1259), Datum::Bool(true));
        assert_eq!(call(16384), Datum::Bool(true));
        assert_eq!(call(16385), Datum::Bool(false));
        assert_eq!(call(99_999), Datum::Null);
        // Indexes count too (`\di`).
        assert_eq!(call(2662), Datum::Bool(true));
        assert_eq!(call(16390), Datum::Bool(true));
        assert_eq!(call(16391), Datum::Bool(false));
        assert_eq!(call(16392), Datum::Bool(false));
    }

    fn index_in(nsp: Oid, oid: Oid, name: &str, table_oid: Oid) -> Arc<crate::catalog::IndexDef> {
        Arc::new(crate::catalog::IndexDef {
            oid,
            name: name.into(),
            namespace: nsp,
            table_oid,
            locator: crate::storage::smgr::RelFileLocator {
                spc_oid: 1663,
                db_oid: 5,
                rel_number: crate::storage::smgr::RelFileNumber(oid),
            },
            columns: vec![],
            unique: false,
            primary: false,
            constraint: None,
        })
    }

    #[test]
    fn function_kinds() {
        assert!(matches!(
            functions_named("pg_get_userbyid")[0].kind,
            FnKind::Context(_)
        ));
        assert!(matches!(functions_named("upper")[0].kind, FnKind::Pure(_)));
        let FnKind::Pure(f) = functions_named("pg_encoding_to_char")[0].kind else {
            panic!("pure");
        };
        assert_eq!(f(&[Datum::Int4(6)]).unwrap(), Datum::Text("UTF8".into()));
    }
}

#[cfg(test)]
mod agg_tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn has_48_rows_with_unique_oid_and_signature() {
        assert_eq!(AGGREGATES.len(), 48);
        let oids: HashSet<_> = AGGREGATES.iter().map(|a| a.oid).collect();
        assert_eq!(oids.len(), AGGREGATES.len());
        let sigs: HashSet<_> = AGGREGATES.iter().map(|a| (a.name, a.args)).collect();
        assert_eq!(sigs.len(), AGGREGATES.len());
    }

    #[test]
    fn counts_per_name() {
        let n = |s: &str| aggregates_named(s).len();
        assert_eq!(
            (n("count"), n("sum"), n("avg"), n("min"), n("max")),
            (2, 6, 6, 15, 15)
        );
        assert_eq!(
            (n("bool_and"), n("bool_or"), n("every"), n("nope")),
            (1, 1, 1, 0)
        );
    }

    #[test]
    fn result_types_follow_pg17() {
        let find = |name: &str, arg: Oid| {
            aggregates_named(name)
                .into_iter()
                .find(|a| a.args == [arg])
                .unwrap()
        };
        assert_eq!(find("sum", oid::INT2).result, oid::INT8);
        assert_eq!(find("sum", oid::INT4).result, oid::INT8);
        assert_eq!(find("sum", oid::INT8).result, oid::NUMERIC);
        assert_eq!(find("avg", oid::FLOAT4).result, oid::FLOAT8);
        assert_eq!(find("avg", oid::INT4).result, oid::NUMERIC);
        assert_eq!(find("every", oid::BOOL).kind, AggKind::BoolAnd);
        let c = aggregates_named("count");
        assert!(
            c.iter()
                .any(|a| a.args.is_empty() && a.kind == AggKind::CountStar)
        );
        assert!(
            c.iter()
                .any(|a| a.args == [ANY_OID] && a.kind == AggKind::Count)
        );
        assert!(c.iter().all(|a| a.result == oid::INT8));
    }

    #[test]
    fn min_max_return_argument_type() {
        for a in AGGREGATES
            .iter()
            .filter(|a| matches!(a.kind, AggKind::Min | AggKind::Max))
        {
            assert_eq!(a.args, [a.result], "{}", a.name);
        }
    }

    /// `PROCS` に行がある OID は、名前・引数・結果型が `AGGREGATES` と一致する。
    #[test]
    fn agrees_with_procs_when_present() {
        for a in AGGREGATES {
            if let Some(p) = proc_by_oid(a.oid) {
                assert_eq!(
                    (p.name, p.args, p.result),
                    (a.name, a.args, a.result),
                    "{}",
                    a.oid
                );
            }
        }
    }
}
