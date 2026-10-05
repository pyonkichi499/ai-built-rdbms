//! Static tables of built-in types, casts, operators and functions.
//! OIDs are identical to PostgreSQL's. They are the runtime lookup tables
//! of the analyzer and, in M2, also the source of the initial contents of
//! `pg_type`, `pg_cast`, `pg_operator` and `pg_proc` (`catalog::rows`).
//!
//! `TYPES`, `CASTS`, `OPERATORS`, `FUNCTIONS` and `PROCS` use the
//! PostgreSQL 17 OIDs and attribute values (taken from a PostgreSQL 17
//! catalog; `pg_proc` rows that no table references are not listed).

use super::{CatalogReader, FnKind};
use crate::error::Result;
use crate::executor::{RuntimeInfo, SessionInfo};
use crate::types::funcs;
use crate::types::ops::{self, BuiltinFn};
use crate::types::{Datum, Oid, oid};

/// A row of `pg_type`.
#[derive(Debug)]
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
}

/// A row of `pg_cast`.
#[derive(Debug)]
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
#[derive(Debug)]
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
#[derive(Debug)]
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
#[derive(Debug)]
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
#[derive(Debug)]
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

/// Built-in types: the M1 set, the system types that catalog columns use
/// (`m2.md` §1.3), the pseudo-types and composite types needed to close the
/// references of the catalog rows, and the array types of §1.3.
#[rustfmt::skip]
pub static TYPES: &[BuiltinType] = &[
    ty(16, "bool", 1, true, 'b', 'B', true, ',', 0, 0, 1000, ("boolin", 1242), ("boolout", 1243), 'c', 'p', 0),
    ty(18, "char", 1, true, 'b', 'Z', false, ',', 0, 0, 1002, ("charin", 1245), ("charout", 33), 'c', 'p', 0),
    ty(19, "name", 64, false, 'b', 'S', false, ',', 0, 18, 1003, ("namein", 34), ("nameout", 35), 'c', 'p', 950),
    ty(20, "int8", 8, true, 'b', 'N', false, ',', 0, 0, 1016, ("int8in", 460), ("int8out", 461), 'd', 'p', 0),
    ty(21, "int2", 2, true, 'b', 'N', false, ',', 0, 0, 1005, ("int2in", 38), ("int2out", 39), 's', 'p', 0),
    ty(23, "int4", 4, true, 'b', 'N', false, ',', 0, 0, 1007, ("int4in", 42), ("int4out", 43), 'i', 'p', 0),
    ty(24, "regproc", 4, true, 'b', 'N', false, ',', 0, 0, 1008, ("regprocin", 44), ("regprocout", 45), 'i', 'p', 0),
    ty(25, "text", -1, false, 'b', 'S', true, ',', 0, 0, 1009, ("textin", 46), ("textout", 47), 'i', 'x', 100),
    ty(26, "oid", 4, true, 'b', 'N', true, ',', 0, 0, 1028, ("oidin", 1798), ("oidout", 1799), 'i', 'p', 0),
    ty(27, "tid", 6, false, 'b', 'U', false, ',', 0, 0, 1010, ("tidin", 48), ("tidout", 49), 's', 'p', 0),
    ty(28, "xid", 4, true, 'b', 'U', false, ',', 0, 0, 1011, ("xidin", 50), ("xidout", 51), 'i', 'p', 0),
    ty(29, "cid", 4, true, 'b', 'U', false, ',', 0, 0, 1012, ("cidin", 52), ("cidout", 53), 'i', 'p', 0),
    ty(30, "oidvector", -1, false, 'b', 'A', false, ',', 0, 26, 1013, ("oidvectorin", 54), ("oidvectorout", 55), 'i', 'p', 0),
    ty(71, "pg_type", -1, false, 'c', 'C', false, ',', 1247, 0, 210, ("record_in", 2290), ("record_out", 2291), 'd', 'x', 0),
    ty(75, "pg_attribute", -1, false, 'c', 'C', false, ',', 1249, 0, 270, ("record_in", 2290), ("record_out", 2291), 'd', 'x', 0),
    ty(81, "pg_proc", -1, false, 'c', 'C', false, ',', 1255, 0, 272, ("record_in", 2290), ("record_out", 2291), 'd', 'x', 0),
    ty(83, "pg_class", -1, false, 'c', 'C', false, ',', 1259, 0, 273, ("record_in", 2290), ("record_out", 2291), 'd', 'x', 0),
    ty(194, "pg_node_tree", -1, false, 'b', 'Z', false, ',', 0, 0, 0, ("pg_node_tree_in", 195), ("pg_node_tree_out", 196), 'i', 'x', 100),
    ty(269, "table_am_handler", 4, true, 'p', 'P', false, ',', 0, 0, 0, ("table_am_handler_in", 267), ("table_am_handler_out", 268), 'i', 'p', 0),
    ty(325, "index_am_handler", 4, true, 'p', 'P', false, ',', 0, 0, 0, ("index_am_handler_in", 326), ("index_am_handler_out", 327), 'i', 'p', 0),
    ty(700, "float4", 4, true, 'b', 'N', false, ',', 0, 0, 1021, ("float4in", 200), ("float4out", 201), 'i', 'p', 0),
    ty(701, "float8", 8, true, 'b', 'N', true, ',', 0, 0, 1022, ("float8in", 214), ("float8out", 215), 'd', 'p', 0),
    ty(705, "unknown", -2, false, 'p', 'X', false, ',', 0, 0, 0, ("unknownin", 109), ("unknownout", 110), 'c', 'p', 0),
    ty(1002, "_char", -1, false, 'b', 'A', false, ',', 0, 18, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 0),
    ty(1003, "_name", -1, false, 'b', 'A', false, ',', 0, 19, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 950),
    ty(1005, "_int2", -1, false, 'b', 'A', false, ',', 0, 21, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 0),
    ty(1007, "_int4", -1, false, 'b', 'A', false, ',', 0, 23, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 0),
    ty(1008, "_regproc", -1, false, 'b', 'A', false, ',', 0, 24, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 0),
    ty(1009, "_text", -1, false, 'b', 'A', false, ',', 0, 25, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 100),
    ty(1010, "_tid", -1, false, 'b', 'A', false, ',', 0, 27, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 0),
    ty(1011, "_xid", -1, false, 'b', 'A', false, ',', 0, 28, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 0),
    ty(1012, "_cid", -1, false, 'b', 'A', false, ',', 0, 29, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 0),
    ty(1013, "_oidvector", -1, false, 'b', 'A', false, ',', 0, 30, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 0),
    ty(1028, "_oid", -1, false, 'b', 'A', false, ',', 0, 26, 0, ("array_in", 750), ("array_out", 751), 'i', 'x', 0),
    ty(1033, "aclitem", 16, false, 'b', 'U', false, ',', 0, 0, 1034, ("aclitemin", 1031), ("aclitemout", 1032), 'd', 'p', 0),
    ty(1034, "_aclitem", -1, false, 'b', 'A', false, ',', 0, 1033, 0, ("array_in", 750), ("array_out", 751), 'd', 'x', 0),
    ty(1043, "varchar", -1, false, 'b', 'S', false, ',', 0, 0, 1015, ("varcharin", 1046), ("varcharout", 1047), 'i', 'x', 100),
    ty(1082, "date", 4, true, 'b', 'D', false, ',', 0, 0, 1182, ("date_in", 1084), ("date_out", 1085), 'i', 'p', 0),
    ty(1184, "timestamptz", 8, true, 'b', 'D', true, ',', 0, 0, 1185, ("timestamptz_in", 1150), ("timestamptz_out", 1151), 'd', 'p', 0),
    ty(1186, "interval", 16, false, 'b', 'T', true, ',', 0, 0, 1187, ("interval_in", 1160), ("interval_out", 1161), 'd', 'p', 0),
    ty(1248, "pg_database", -1, false, 'c', 'C', false, ',', 1262, 0, 10052, ("record_in", 2290), ("record_out", 2291), 'd', 'x', 0),
    ty(1700, "numeric", -1, false, 'b', 'N', false, ',', 0, 0, 1231, ("numeric_in", 1701), ("numeric_out", 1702), 'i', 'm', 0),
    ty(2249, "record", -1, false, 'p', 'P', false, ',', 0, 0, 2287, ("record_in", 2290), ("record_out", 2291), 'd', 'x', 0),
    ty(2275, "cstring", -2, false, 'p', 'P', false, ',', 0, 0, 1263, ("cstring_in", 2292), ("cstring_out", 2293), 'c', 'p', 0),
    ty(2277, "anyarray", -1, false, 'p', 'P', false, ',', 0, 0, 0, ("anyarray_in", 2296), ("anyarray_out", 2297), 'd', 'x', 0),
    ty(2278, "void", 4, true, 'p', 'P', false, ',', 0, 0, 0, ("void_in", 2298), ("void_out", 2299), 'i', 'p', 0),
    ty(2281, "internal", 8, true, 'p', 'P', false, ',', 0, 0, 0, ("internal_in", 2304), ("internal_out", 2305), 'd', 'p', 0),
    ty(2776, "anynonarray", 4, true, 'p', 'P', false, ',', 0, 0, 0, ("anynonarray_in", 2777), ("anynonarray_out", 2778), 'i', 'p', 0),
    ty(2842, "pg_authid", -1, false, 'c', 'C', false, ',', 1260, 0, 10057, ("record_in", 2290), ("record_out", 2291), 'd', 'x', 0),
];

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
            | oid::NAME
            | oid::UNKNOWN
            | oid::OID
            | oid::REGPROC
            | oid::TID
            | oid::XID
            | oid::CID
            | oid::OIDVECTOR
            | oid::PG_NODE_TREE
            | oid::INT4_ARRAY
            | oid::VOID
    )
}

/// Types that exist so that catalog columns can be declared, but whose
/// columns only ever hold NULL in M2 (`aclitem`, `timestamptz`, `anyarray`
/// and the array types of `m2.md` §1.3). Storing anything else is `0A000`.
pub fn is_null_only_type(t: Oid) -> bool {
    matches!(
        t,
        oid::ACLITEM
            | oid::ACLITEM_ARRAY
            | oid::TIMESTAMPTZ
            | oid::ANYARRAY
            | oid::TEXT_ARRAY
            | oid::INT2_ARRAY
            | oid::OID_ARRAY
            | oid::CHAR_ARRAY
    )
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
        CastMethod::Function(_) => 'f',
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

/// Built-in casts (`pg_cast`), as in PostgreSQL's `pg_cast.dat`. Conversions
/// to and from string types that have no entry here are I/O conversions
/// decided by the analyzer (`find_coercion_pathway`). Same-type length
/// coercion (`varchar(n)`) is a separate `CoerceTypmod` step, not a row here.
#[rustfmt::skip]
pub static CASTS: &[BuiltinCast] = &[
    cast(oid::INT2, oid::INT4, CastContext::Implicit, CastMethod::Function(ops::to_int4), 313),
    cast(oid::INT2, oid::INT8, CastContext::Implicit, CastMethod::Function(ops::to_int8), 754),
    cast(oid::INT2, oid::FLOAT4, CastContext::Implicit, CastMethod::Function(ops::to_float4), 236),
    cast(oid::INT2, oid::FLOAT8, CastContext::Implicit, CastMethod::Function(ops::to_float8), 235),
    cast(oid::INT4, oid::INT8, CastContext::Implicit, CastMethod::Function(ops::to_int8), 481),
    cast(oid::INT4, oid::INT2, CastContext::Assignment, CastMethod::Function(ops::to_int2), 314),
    cast(oid::INT4, oid::FLOAT4, CastContext::Implicit, CastMethod::Function(ops::to_float4), 318),
    cast(oid::INT4, oid::FLOAT8, CastContext::Implicit, CastMethod::Function(ops::to_float8), 316),
    cast(oid::INT8, oid::INT2, CastContext::Assignment, CastMethod::Function(ops::to_int2), 714),
    cast(oid::INT8, oid::INT4, CastContext::Assignment, CastMethod::Function(ops::to_int4), 480),
    cast(oid::INT8, oid::FLOAT4, CastContext::Implicit, CastMethod::Function(ops::to_float4), 652),
    cast(oid::INT8, oid::FLOAT8, CastContext::Implicit, CastMethod::Function(ops::to_float8), 482),
    cast(oid::FLOAT4, oid::INT2, CastContext::Assignment, CastMethod::Function(ops::to_int2), 238),
    cast(oid::FLOAT4, oid::INT4, CastContext::Assignment, CastMethod::Function(ops::to_int4), 319),
    cast(oid::FLOAT4, oid::INT8, CastContext::Assignment, CastMethod::Function(ops::to_int8), 653),
    cast(oid::FLOAT4, oid::FLOAT8, CastContext::Implicit, CastMethod::Function(ops::to_float8), 311),
    cast(oid::FLOAT8, oid::INT2, CastContext::Assignment, CastMethod::Function(ops::to_int2), 237),
    cast(oid::FLOAT8, oid::INT4, CastContext::Assignment, CastMethod::Function(ops::to_int4), 317),
    cast(oid::FLOAT8, oid::INT8, CastContext::Assignment, CastMethod::Function(ops::to_int8), 483),
    cast(oid::FLOAT8, oid::FLOAT4, CastContext::Assignment, CastMethod::Function(ops::to_float4), 312),
    cast(oid::INT4, oid::BOOL, CastContext::Explicit, CastMethod::Function(ops::int4_to_bool), 2557),
    cast(oid::BOOL, oid::INT4, CastContext::Explicit, CastMethod::Function(ops::bool_to_int4), 2558),
    cast(oid::BOOL, oid::TEXT, CastContext::Assignment, CastMethod::Function(ops::bool_to_text), 2971),
    cast(oid::BOOL, oid::VARCHAR, CastContext::Assignment, CastMethod::Function(ops::bool_to_text), 2971),
    cast(oid::TEXT, oid::VARCHAR, CastContext::Implicit, CastMethod::Binary, 0),
    cast(oid::VARCHAR, oid::TEXT, CastContext::Implicit, CastMethod::Binary, 0),
    cast(oid::TEXT, oid::NAME, CastContext::Implicit, CastMethod::Function(ops::text_to_name), 407),
    cast(oid::VARCHAR, oid::NAME, CastContext::Implicit, CastMethod::Function(ops::text_to_name), 1400),
    cast(oid::NAME, oid::TEXT, CastContext::Implicit, CastMethod::Function(ops::text_identity), 406),
    cast(oid::NAME, oid::VARCHAR, CastContext::Assignment, CastMethod::Function(ops::text_identity), 1401),
    cast(oid::INT2, NUMERIC, CastContext::Implicit, CastMethod::Function(ops::int_to_numeric), 1782),
    cast(oid::INT4, NUMERIC, CastContext::Implicit, CastMethod::Function(ops::int_to_numeric), 1740),
    cast(oid::INT8, NUMERIC, CastContext::Implicit, CastMethod::Function(ops::int_to_numeric), 1781),
    cast(oid::FLOAT4, NUMERIC, CastContext::Assignment, CastMethod::Function(ops::float_to_numeric), 1742),
    cast(oid::FLOAT8, NUMERIC, CastContext::Assignment, CastMethod::Function(ops::float_to_numeric), 1743),
    cast(NUMERIC, oid::INT2, CastContext::Assignment, CastMethod::Function(ops::numeric_to_int2), 1783),
    cast(NUMERIC, oid::INT4, CastContext::Assignment, CastMethod::Function(ops::numeric_to_int4), 1744),
    cast(NUMERIC, oid::INT8, CastContext::Assignment, CastMethod::Function(ops::numeric_to_int8), 1779),
    cast(NUMERIC, oid::FLOAT4, CastContext::Implicit, CastMethod::Function(ops::numeric_to_float4), 1745),
    cast(NUMERIC, oid::FLOAT8, CastContext::Implicit, CastMethod::Function(ops::numeric_to_float8), 1746),
    // oid / regproc: the PostgreSQL binary-coercible casts convert the Datum
    // variant here (`cast_bin`: shown as `castmethod = 'b'`).
    cast_bin(oid::INT4, oid::OID, CastContext::Implicit, ops::int_to_oid),
    cast_bin(oid::INT4, oid::REGPROC, CastContext::Implicit, ops::int_to_oid),
    cast_fn(oid::INT2, oid::OID, CastContext::Implicit, ops::int_to_oid, 313),
    cast_fn(oid::INT2, oid::REGPROC, CastContext::Implicit, ops::int_to_oid, 313),
    cast_fn(oid::INT8, oid::OID, CastContext::Implicit, ops::int8_to_oid, 1287),
    cast_fn(oid::INT8, oid::REGPROC, CastContext::Implicit, ops::int8_to_oid, 1287),
    cast_bin(oid::OID, oid::INT4, CastContext::Assignment, ops::oid_to_int4),
    cast_bin(oid::REGPROC, oid::INT4, CastContext::Assignment, ops::oid_to_int4),
    cast_fn(oid::OID, oid::INT8, CastContext::Assignment, ops::oid_to_int8, 1288),
    cast_fn(oid::REGPROC, oid::INT8, CastContext::Assignment, ops::oid_to_int8, 1288),
    cast(oid::OID, oid::REGPROC, CastContext::Implicit, CastMethod::Binary, 0),
    cast(oid::REGPROC, oid::OID, CastContext::Implicit, CastMethod::Binary, 0),
    // "char"
    cast_fn(oid::CHAR, oid::INT4, CastContext::Explicit, ops::char_to_int4, 77),
    cast_fn(oid::INT4, oid::CHAR, CastContext::Explicit, ops::int4_to_char, 78),
    cast_fn(oid::CHAR, oid::TEXT, CastContext::Implicit, ops::char_to_text, 946),
    cast_fn(oid::CHAR, oid::VARCHAR, CastContext::Assignment, ops::char_to_text, 946),
    cast_fn(oid::TEXT, oid::CHAR, CastContext::Assignment, ops::text_to_char, 944),
    cast_fn(oid::VARCHAR, oid::CHAR, CastContext::Assignment, ops::text_to_char, 944),
    // pg_node_tree holds text.
    cast(oid::PG_NODE_TREE, oid::TEXT, CastContext::Implicit, CastMethod::Binary, 0),
];

/// Built-in operators (`pg_operator`). Cross-width integer operators share
/// the body of the result type's operator (the bodies accept any width).
/// `text || anynonarray` (2779) and `anynonarray || text` (2780) are SQL
/// functions `$1::text || $2` in PostgreSQL: the analyzer converts the
/// polymorphic argument to text explicitly, so their body is `textcat`.
#[rustfmt::skip]
pub static OPERATORS: &[BuiltinOperator] = &[
    op(94, "=", oid::INT2, oid::INT2, oid::BOOL, ops::cmp_eq),
    op(519, "<>", oid::INT2, oid::INT2, oid::BOOL, ops::cmp_ne),
    op(95, "<", oid::INT2, oid::INT2, oid::BOOL, ops::cmp_lt),
    op(522, "<=", oid::INT2, oid::INT2, oid::BOOL, ops::cmp_le),
    op(520, ">", oid::INT2, oid::INT2, oid::BOOL, ops::cmp_gt),
    op(524, ">=", oid::INT2, oid::INT2, oid::BOOL, ops::cmp_ge),
    op(96, "=", oid::INT4, oid::INT4, oid::BOOL, ops::cmp_eq),
    op(518, "<>", oid::INT4, oid::INT4, oid::BOOL, ops::cmp_ne),
    op(97, "<", oid::INT4, oid::INT4, oid::BOOL, ops::cmp_lt),
    op(523, "<=", oid::INT4, oid::INT4, oid::BOOL, ops::cmp_le),
    op(521, ">", oid::INT4, oid::INT4, oid::BOOL, ops::cmp_gt),
    op(525, ">=", oid::INT4, oid::INT4, oid::BOOL, ops::cmp_ge),
    op(410, "=", oid::INT8, oid::INT8, oid::BOOL, ops::cmp_eq),
    op(411, "<>", oid::INT8, oid::INT8, oid::BOOL, ops::cmp_ne),
    op(412, "<", oid::INT8, oid::INT8, oid::BOOL, ops::cmp_lt),
    op(414, "<=", oid::INT8, oid::INT8, oid::BOOL, ops::cmp_le),
    op(413, ">", oid::INT8, oid::INT8, oid::BOOL, ops::cmp_gt),
    op(415, ">=", oid::INT8, oid::INT8, oid::BOOL, ops::cmp_ge),
    op(532, "=", oid::INT2, oid::INT4, oid::BOOL, ops::cmp_eq),
    op(538, "<>", oid::INT2, oid::INT4, oid::BOOL, ops::cmp_ne),
    op(534, "<", oid::INT2, oid::INT4, oid::BOOL, ops::cmp_lt),
    op(540, "<=", oid::INT2, oid::INT4, oid::BOOL, ops::cmp_le),
    op(536, ">", oid::INT2, oid::INT4, oid::BOOL, ops::cmp_gt),
    op(542, ">=", oid::INT2, oid::INT4, oid::BOOL, ops::cmp_ge),
    op(533, "=", oid::INT4, oid::INT2, oid::BOOL, ops::cmp_eq),
    op(539, "<>", oid::INT4, oid::INT2, oid::BOOL, ops::cmp_ne),
    op(535, "<", oid::INT4, oid::INT2, oid::BOOL, ops::cmp_lt),
    op(541, "<=", oid::INT4, oid::INT2, oid::BOOL, ops::cmp_le),
    op(537, ">", oid::INT4, oid::INT2, oid::BOOL, ops::cmp_gt),
    op(543, ">=", oid::INT4, oid::INT2, oid::BOOL, ops::cmp_ge),
    op(15, "=", oid::INT4, oid::INT8, oid::BOOL, ops::cmp_eq),
    op(36, "<>", oid::INT4, oid::INT8, oid::BOOL, ops::cmp_ne),
    op(37, "<", oid::INT4, oid::INT8, oid::BOOL, ops::cmp_lt),
    op(80, "<=", oid::INT4, oid::INT8, oid::BOOL, ops::cmp_le),
    op(76, ">", oid::INT4, oid::INT8, oid::BOOL, ops::cmp_gt),
    op(82, ">=", oid::INT4, oid::INT8, oid::BOOL, ops::cmp_ge),
    op(416, "=", oid::INT8, oid::INT4, oid::BOOL, ops::cmp_eq),
    op(417, "<>", oid::INT8, oid::INT4, oid::BOOL, ops::cmp_ne),
    op(418, "<", oid::INT8, oid::INT4, oid::BOOL, ops::cmp_lt),
    op(420, "<=", oid::INT8, oid::INT4, oid::BOOL, ops::cmp_le),
    op(419, ">", oid::INT8, oid::INT4, oid::BOOL, ops::cmp_gt),
    op(430, ">=", oid::INT8, oid::INT4, oid::BOOL, ops::cmp_ge),
    op(1862, "=", oid::INT2, oid::INT8, oid::BOOL, ops::cmp_eq),
    op(1863, "<>", oid::INT2, oid::INT8, oid::BOOL, ops::cmp_ne),
    op(1864, "<", oid::INT2, oid::INT8, oid::BOOL, ops::cmp_lt),
    op(1866, "<=", oid::INT2, oid::INT8, oid::BOOL, ops::cmp_le),
    op(1865, ">", oid::INT2, oid::INT8, oid::BOOL, ops::cmp_gt),
    op(1867, ">=", oid::INT2, oid::INT8, oid::BOOL, ops::cmp_ge),
    op(1868, "=", oid::INT8, oid::INT2, oid::BOOL, ops::cmp_eq),
    op(1869, "<>", oid::INT8, oid::INT2, oid::BOOL, ops::cmp_ne),
    op(1870, "<", oid::INT8, oid::INT2, oid::BOOL, ops::cmp_lt),
    op(1872, "<=", oid::INT8, oid::INT2, oid::BOOL, ops::cmp_le),
    op(1871, ">", oid::INT8, oid::INT2, oid::BOOL, ops::cmp_gt),
    op(1873, ">=", oid::INT8, oid::INT2, oid::BOOL, ops::cmp_ge),
    op(620, "=", oid::FLOAT4, oid::FLOAT4, oid::BOOL, ops::cmp_eq),
    op(621, "<>", oid::FLOAT4, oid::FLOAT4, oid::BOOL, ops::cmp_ne),
    op(622, "<", oid::FLOAT4, oid::FLOAT4, oid::BOOL, ops::cmp_lt),
    op(624, "<=", oid::FLOAT4, oid::FLOAT4, oid::BOOL, ops::cmp_le),
    op(623, ">", oid::FLOAT4, oid::FLOAT4, oid::BOOL, ops::cmp_gt),
    op(625, ">=", oid::FLOAT4, oid::FLOAT4, oid::BOOL, ops::cmp_ge),
    op(670, "=", oid::FLOAT8, oid::FLOAT8, oid::BOOL, ops::cmp_eq),
    op(671, "<>", oid::FLOAT8, oid::FLOAT8, oid::BOOL, ops::cmp_ne),
    op(672, "<", oid::FLOAT8, oid::FLOAT8, oid::BOOL, ops::cmp_lt),
    op(673, "<=", oid::FLOAT8, oid::FLOAT8, oid::BOOL, ops::cmp_le),
    op(674, ">", oid::FLOAT8, oid::FLOAT8, oid::BOOL, ops::cmp_gt),
    op(675, ">=", oid::FLOAT8, oid::FLOAT8, oid::BOOL, ops::cmp_ge),
    op(1120, "=", oid::FLOAT4, oid::FLOAT8, oid::BOOL, ops::cmp_eq),
    op(1121, "<>", oid::FLOAT4, oid::FLOAT8, oid::BOOL, ops::cmp_ne),
    op(1122, "<", oid::FLOAT4, oid::FLOAT8, oid::BOOL, ops::cmp_lt),
    op(1124, "<=", oid::FLOAT4, oid::FLOAT8, oid::BOOL, ops::cmp_le),
    op(1123, ">", oid::FLOAT4, oid::FLOAT8, oid::BOOL, ops::cmp_gt),
    op(1125, ">=", oid::FLOAT4, oid::FLOAT8, oid::BOOL, ops::cmp_ge),
    op(1130, "=", oid::FLOAT8, oid::FLOAT4, oid::BOOL, ops::cmp_eq),
    op(1131, "<>", oid::FLOAT8, oid::FLOAT4, oid::BOOL, ops::cmp_ne),
    op(1132, "<", oid::FLOAT8, oid::FLOAT4, oid::BOOL, ops::cmp_lt),
    op(1134, "<=", oid::FLOAT8, oid::FLOAT4, oid::BOOL, ops::cmp_le),
    op(1133, ">", oid::FLOAT8, oid::FLOAT4, oid::BOOL, ops::cmp_gt),
    op(1135, ">=", oid::FLOAT8, oid::FLOAT4, oid::BOOL, ops::cmp_ge),
    op(91, "=", oid::BOOL, oid::BOOL, oid::BOOL, ops::cmp_eq),
    op(85, "<>", oid::BOOL, oid::BOOL, oid::BOOL, ops::cmp_ne),
    op(58, "<", oid::BOOL, oid::BOOL, oid::BOOL, ops::cmp_lt),
    op(1694, "<=", oid::BOOL, oid::BOOL, oid::BOOL, ops::cmp_le),
    op(59, ">", oid::BOOL, oid::BOOL, oid::BOOL, ops::cmp_gt),
    op(1695, ">=", oid::BOOL, oid::BOOL, oid::BOOL, ops::cmp_ge),
    op(98, "=", oid::TEXT, oid::TEXT, oid::BOOL, ops::cmp_eq),
    op(531, "<>", oid::TEXT, oid::TEXT, oid::BOOL, ops::cmp_ne),
    op(664, "<", oid::TEXT, oid::TEXT, oid::BOOL, ops::cmp_lt),
    op(665, "<=", oid::TEXT, oid::TEXT, oid::BOOL, ops::cmp_le),
    op(666, ">", oid::TEXT, oid::TEXT, oid::BOOL, ops::cmp_gt),
    op(667, ">=", oid::TEXT, oid::TEXT, oid::BOOL, ops::cmp_ge),
    op(93, "=", oid::NAME, oid::NAME, oid::BOOL, ops::cmp_eq),
    op(643, "<>", oid::NAME, oid::NAME, oid::BOOL, ops::cmp_ne),
    op(660, "<", oid::NAME, oid::NAME, oid::BOOL, ops::cmp_lt),
    op(661, "<=", oid::NAME, oid::NAME, oid::BOOL, ops::cmp_le),
    op(662, ">", oid::NAME, oid::NAME, oid::BOOL, ops::cmp_gt),
    op(663, ">=", oid::NAME, oid::NAME, oid::BOOL, ops::cmp_ge),
    op(1752, "=", NUMERIC, NUMERIC, oid::BOOL, ops::cmp_eq),
    op(1753, "<>", NUMERIC, NUMERIC, oid::BOOL, ops::cmp_ne),
    op(1754, "<", NUMERIC, NUMERIC, oid::BOOL, ops::cmp_lt),
    op(1755, "<=", NUMERIC, NUMERIC, oid::BOOL, ops::cmp_le),
    op(1756, ">", NUMERIC, NUMERIC, oid::BOOL, ops::cmp_gt),
    op(1757, ">=", NUMERIC, NUMERIC, oid::BOOL, ops::cmp_ge),
    op(1093, "=", DATE, DATE, oid::BOOL, ops::unsupported),
    op(1094, "<>", DATE, DATE, oid::BOOL, ops::unsupported),
    op(1095, "<", DATE, DATE, oid::BOOL, ops::unsupported),
    op(1096, "<=", DATE, DATE, oid::BOOL, ops::unsupported),
    op(1097, ">", DATE, DATE, oid::BOOL, ops::unsupported),
    op(1098, ">=", DATE, DATE, oid::BOOL, ops::unsupported),
    op(1330, "=", INTERVAL, INTERVAL, oid::BOOL, ops::unsupported),
    op(1331, "<>", INTERVAL, INTERVAL, oid::BOOL, ops::unsupported),
    op(1332, "<", INTERVAL, INTERVAL, oid::BOOL, ops::unsupported),
    op(1333, "<=", INTERVAL, INTERVAL, oid::BOOL, ops::unsupported),
    op(1334, ">", INTERVAL, INTERVAL, oid::BOOL, ops::unsupported),
    op(1335, ">=", INTERVAL, INTERVAL, oid::BOOL, ops::unsupported),
    op(550, "+", oid::INT2, oid::INT2, oid::INT2, ops::int2pl),
    op(554, "-", oid::INT2, oid::INT2, oid::INT2, ops::int2mi),
    op(526, "*", oid::INT2, oid::INT2, oid::INT2, ops::int2mul),
    op(527, "/", oid::INT2, oid::INT2, oid::INT2, ops::int2div),
    op(529, "%", oid::INT2, oid::INT2, oid::INT2, ops::int2mod),
    op(551, "+", oid::INT4, oid::INT4, oid::INT4, ops::int4pl),
    op(555, "-", oid::INT4, oid::INT4, oid::INT4, ops::int4mi),
    op(514, "*", oid::INT4, oid::INT4, oid::INT4, ops::int4mul),
    op(528, "/", oid::INT4, oid::INT4, oid::INT4, ops::int4div),
    op(530, "%", oid::INT4, oid::INT4, oid::INT4, ops::int4mod),
    op(684, "+", oid::INT8, oid::INT8, oid::INT8, ops::int8pl),
    op(685, "-", oid::INT8, oid::INT8, oid::INT8, ops::int8mi),
    op(686, "*", oid::INT8, oid::INT8, oid::INT8, ops::int8mul),
    op(687, "/", oid::INT8, oid::INT8, oid::INT8, ops::int8div),
    op(439, "%", oid::INT8, oid::INT8, oid::INT8, ops::int8mod),
    op(552, "+", oid::INT2, oid::INT4, oid::INT4, ops::int4pl),
    op(556, "-", oid::INT2, oid::INT4, oid::INT4, ops::int4mi),
    op(544, "*", oid::INT2, oid::INT4, oid::INT4, ops::int4mul),
    op(546, "/", oid::INT2, oid::INT4, oid::INT4, ops::int4div),
    op(553, "+", oid::INT4, oid::INT2, oid::INT4, ops::int4pl),
    op(557, "-", oid::INT4, oid::INT2, oid::INT4, ops::int4mi),
    op(545, "*", oid::INT4, oid::INT2, oid::INT4, ops::int4mul),
    op(547, "/", oid::INT4, oid::INT2, oid::INT4, ops::int4div),
    op(692, "+", oid::INT4, oid::INT8, oid::INT8, ops::int8pl),
    op(693, "-", oid::INT4, oid::INT8, oid::INT8, ops::int8mi),
    op(694, "*", oid::INT4, oid::INT8, oid::INT8, ops::int8mul),
    op(695, "/", oid::INT4, oid::INT8, oid::INT8, ops::int8div),
    op(688, "+", oid::INT8, oid::INT4, oid::INT8, ops::int8pl),
    op(689, "-", oid::INT8, oid::INT4, oid::INT8, ops::int8mi),
    op(690, "*", oid::INT8, oid::INT4, oid::INT8, ops::int8mul),
    op(691, "/", oid::INT8, oid::INT4, oid::INT8, ops::int8div),
    op(822, "+", oid::INT2, oid::INT8, oid::INT8, ops::int8pl),
    op(823, "-", oid::INT2, oid::INT8, oid::INT8, ops::int8mi),
    op(824, "*", oid::INT2, oid::INT8, oid::INT8, ops::int8mul),
    op(825, "/", oid::INT2, oid::INT8, oid::INT8, ops::int8div),
    op(818, "+", oid::INT8, oid::INT2, oid::INT8, ops::int8pl),
    op(819, "-", oid::INT8, oid::INT2, oid::INT8, ops::int8mi),
    op(820, "*", oid::INT8, oid::INT2, oid::INT8, ops::int8mul),
    op(821, "/", oid::INT8, oid::INT2, oid::INT8, ops::int8div),
    op(586, "+", oid::FLOAT4, oid::FLOAT4, oid::FLOAT4, ops::float4pl),
    op(587, "-", oid::FLOAT4, oid::FLOAT4, oid::FLOAT4, ops::float4mi),
    op(589, "*", oid::FLOAT4, oid::FLOAT4, oid::FLOAT4, ops::float4mul),
    op(588, "/", oid::FLOAT4, oid::FLOAT4, oid::FLOAT4, ops::float4div),
    op(591, "+", oid::FLOAT8, oid::FLOAT8, oid::FLOAT8, ops::float8pl),
    op(592, "-", oid::FLOAT8, oid::FLOAT8, oid::FLOAT8, ops::float8mi),
    op(594, "*", oid::FLOAT8, oid::FLOAT8, oid::FLOAT8, ops::float8mul),
    op(593, "/", oid::FLOAT8, oid::FLOAT8, oid::FLOAT8, ops::float8div),
    op(1116, "+", oid::FLOAT4, oid::FLOAT8, oid::FLOAT8, ops::float8pl),
    op(1117, "-", oid::FLOAT4, oid::FLOAT8, oid::FLOAT8, ops::float8mi),
    op(1119, "*", oid::FLOAT4, oid::FLOAT8, oid::FLOAT8, ops::float8mul),
    op(1118, "/", oid::FLOAT4, oid::FLOAT8, oid::FLOAT8, ops::float8div),
    op(1126, "+", oid::FLOAT8, oid::FLOAT4, oid::FLOAT8, ops::float8pl),
    op(1127, "-", oid::FLOAT8, oid::FLOAT4, oid::FLOAT8, ops::float8mi),
    op(1129, "*", oid::FLOAT8, oid::FLOAT4, oid::FLOAT8, ops::float8mul),
    op(1128, "/", oid::FLOAT8, oid::FLOAT4, oid::FLOAT8, ops::float8div),
    op(1758, "+", NUMERIC, NUMERIC, NUMERIC, ops::numeric_add),
    op(1759, "-", NUMERIC, NUMERIC, NUMERIC, ops::numeric_sub),
    op(1760, "*", NUMERIC, NUMERIC, NUMERIC, ops::numeric_mul),
    op(1761, "/", NUMERIC, NUMERIC, NUMERIC, ops::numeric_div),
    op(1762, "%", NUMERIC, NUMERIC, NUMERIC, ops::numeric_mod),
    op(1100, "+", DATE, oid::INT4, DATE, ops::unsupported),
    op(2555, "+", oid::INT4, DATE, DATE, ops::unsupported),
    op(1099, "-", DATE, DATE, oid::INT4, ops::unsupported),
    op(1101, "-", DATE, oid::INT4, DATE, ops::unsupported),
    op(1337, "+", INTERVAL, INTERVAL, INTERVAL, ops::unsupported),
    op(1338, "-", INTERVAL, INTERVAL, INTERVAL, ops::unsupported),
    op(1583, "*", INTERVAL, oid::FLOAT8, INTERVAL, ops::unsupported),
    op(1584, "*", oid::FLOAT8, INTERVAL, INTERVAL, ops::unsupported),
    op(1585, "/", INTERVAL, oid::FLOAT8, INTERVAL, ops::unsupported),
    op(559, "-", 0, oid::INT2, oid::INT2, ops::int2um),
    op(558, "-", 0, oid::INT4, oid::INT4, ops::int4um),
    op(484, "-", 0, oid::INT8, oid::INT8, ops::int8um),
    op(584, "-", 0, oid::FLOAT4, oid::FLOAT4, ops::float4um),
    op(585, "-", 0, oid::FLOAT8, oid::FLOAT8, ops::float8um),
    op(1751, "-", 0, NUMERIC, NUMERIC, ops::numeric_uminus),
    op(473, "@", 0, oid::INT8, oid::INT8, ops::int8abs),
    op(682, "@", 0, oid::INT2, oid::INT2, ops::int2abs),
    op(773, "@", 0, oid::INT4, oid::INT4, ops::int4abs),
    op(590, "@", 0, oid::FLOAT4, oid::FLOAT4, ops::float4abs),
    op(595, "@", 0, oid::FLOAT8, oid::FLOAT8, ops::float8abs),
    op(1763, "@", 0, NUMERIC, NUMERIC, ops::numeric_abs),
    op(1336, "-", 0, INTERVAL, INTERVAL, ops::unsupported),
    op(1917, "+", 0, oid::INT2, oid::INT2, ops::identity),
    op(1918, "+", 0, oid::INT4, oid::INT4, ops::identity),
    op(1916, "+", 0, oid::INT8, oid::INT8, ops::identity),
    op(1919, "+", 0, oid::FLOAT4, oid::FLOAT4, ops::identity),
    op(1920, "+", 0, oid::FLOAT8, oid::FLOAT8, ops::identity),
    op(1921, "+", 0, NUMERIC, NUMERIC, ops::identity),
    op(965, "^", oid::FLOAT8, oid::FLOAT8, oid::FLOAT8, ops::float8pow),
    op(1038, "^", NUMERIC, NUMERIC, NUMERIC, ops::numeric_power),
    op(654, "||", oid::TEXT, oid::TEXT, oid::TEXT, ops::textcat),
    op(2779, "||", oid::TEXT, ANYNONARRAY, oid::TEXT, ops::textcat),
    op(2780, "||", ANYNONARRAY, oid::TEXT, oid::TEXT, ops::textcat),
    op(1209, "~~", oid::TEXT, oid::TEXT, oid::BOOL, ops::textlike),
    op(1210, "!~~", oid::TEXT, oid::TEXT, oid::BOOL, ops::textnlike),
    op(1627, "~~*", oid::TEXT, oid::TEXT, oid::BOOL, ops::texticlike),
    op(1628, "!~~*", oid::TEXT, oid::TEXT, oid::BOOL, ops::texticnlike),
    op(631, "<", oid::CHAR, oid::CHAR, oid::BOOL, ops::cmp_lt),
    op(632, "<=", oid::CHAR, oid::CHAR, oid::BOOL, ops::cmp_le),
    op(630, "<>", oid::CHAR, oid::CHAR, oid::BOOL, ops::cmp_ne),
    op(92, "=", oid::CHAR, oid::CHAR, oid::BOOL, ops::cmp_eq),
    op(633, ">", oid::CHAR, oid::CHAR, oid::BOOL, ops::cmp_gt),
    op(634, ">=", oid::CHAR, oid::CHAR, oid::BOOL, ops::cmp_ge),
    op(255, "<", oid::NAME, oid::TEXT, oid::BOOL, ops::cmp_lt),
    op(256, "<=", oid::NAME, oid::TEXT, oid::BOOL, ops::cmp_le),
    op(259, "<>", oid::NAME, oid::TEXT, oid::BOOL, ops::cmp_ne),
    op(254, "=", oid::NAME, oid::TEXT, oid::BOOL, ops::cmp_eq),
    op(258, ">", oid::NAME, oid::TEXT, oid::BOOL, ops::cmp_gt),
    op(257, ">=", oid::NAME, oid::TEXT, oid::BOOL, ops::cmp_ge),
    op(261, "<", oid::TEXT, oid::NAME, oid::BOOL, ops::cmp_lt),
    op(262, "<=", oid::TEXT, oid::NAME, oid::BOOL, ops::cmp_le),
    op(265, "<>", oid::TEXT, oid::NAME, oid::BOOL, ops::cmp_ne),
    op(260, "=", oid::TEXT, oid::NAME, oid::BOOL, ops::cmp_eq),
    op(264, ">", oid::TEXT, oid::NAME, oid::BOOL, ops::cmp_gt),
    op(263, ">=", oid::TEXT, oid::NAME, oid::BOOL, ops::cmp_ge),
    op(609, "<", oid::OID, oid::OID, oid::BOOL, ops::cmp_lt),
    op(611, "<=", oid::OID, oid::OID, oid::BOOL, ops::cmp_le),
    op(608, "<>", oid::OID, oid::OID, oid::BOOL, ops::cmp_ne),
    op(607, "=", oid::OID, oid::OID, oid::BOOL, ops::cmp_eq),
    op(610, ">", oid::OID, oid::OID, oid::BOOL, ops::cmp_gt),
    op(612, ">=", oid::OID, oid::OID, oid::BOOL, ops::cmp_ge),
    op(2799, "<", oid::TID, oid::TID, oid::BOOL, ops::cmp_lt),
    op(2801, "<=", oid::TID, oid::TID, oid::BOOL, ops::cmp_le),
    op(402, "<>", oid::TID, oid::TID, oid::BOOL, ops::cmp_ne),
    op(387, "=", oid::TID, oid::TID, oid::BOOL, ops::cmp_eq),
    op(2800, ">", oid::TID, oid::TID, oid::BOOL, ops::cmp_gt),
    op(2802, ">=", oid::TID, oid::TID, oid::BOOL, ops::cmp_ge),
    op(3316, "<>", oid::XID, oid::INT4, oid::BOOL, ops::xid_ne_int4),
    op(353, "=", oid::XID, oid::INT4, oid::BOOL, ops::xid_eq_int4),
    op(3315, "<>", oid::XID, oid::XID, oid::BOOL, ops::cmp_ne),
    op(352, "=", oid::XID, oid::XID, oid::BOOL, ops::cmp_eq),
    op(385, "=", oid::CID, oid::CID, oid::BOOL, ops::cmp_eq),
    op(1874, "&", oid::INT2, oid::INT2, oid::INT2, funcs::int2and),
    op(1875, "|", oid::INT2, oid::INT2, oid::INT2, funcs::int2or),
    op(1876, "#", oid::INT2, oid::INT2, oid::INT2, funcs::int2xor),
    op(1877, "~", 0, oid::INT2, oid::INT2, funcs::int2not),
    op(1878, "<<", oid::INT2, oid::INT4, oid::INT2, funcs::int2shl),
    op(1879, ">>", oid::INT2, oid::INT4, oid::INT2, funcs::int2shr),
    op(1880, "&", oid::INT4, oid::INT4, oid::INT4, funcs::int4and),
    op(1881, "|", oid::INT4, oid::INT4, oid::INT4, funcs::int4or),
    op(1882, "#", oid::INT4, oid::INT4, oid::INT4, funcs::int4xor),
    op(1883, "~", 0, oid::INT4, oid::INT4, funcs::int4not),
    op(1884, "<<", oid::INT4, oid::INT4, oid::INT4, funcs::int4shl),
    op(1885, ">>", oid::INT4, oid::INT4, oid::INT4, funcs::int4shr),
    op(1886, "&", oid::INT8, oid::INT8, oid::INT8, funcs::int8and),
    op(1887, "|", oid::INT8, oid::INT8, oid::INT8, funcs::int8or),
    op(1888, "#", oid::INT8, oid::INT8, oid::INT8, funcs::int8xor),
    op(1889, "~", 0, oid::INT8, oid::INT8, funcs::int8not),
    op(1890, "<<", oid::INT8, oid::INT4, oid::INT8, funcs::int8shl),
    op(1891, ">>", oid::INT8, oid::INT4, oid::INT8, funcs::int8shr),
    op(641, "~", oid::TEXT, oid::TEXT, oid::BOOL, funcs::textregexeq),
    op(642, "!~", oid::TEXT, oid::TEXT, oid::BOOL, funcs::textregexne),
    op(1228, "~*", oid::TEXT, oid::TEXT, oid::BOOL, funcs::texticregexeq),
    op(1229, "!~*", oid::TEXT, oid::TEXT, oid::BOOL, funcs::texticregexne),
];

/// `(oid, oprcanmerge, oprcanhash)` for the operators of PostgreSQL 17 where
/// either is true; every other operator has both false.
static OPERATOR_MERGE_HASH: &[(Oid, bool, bool)] = &[
    (15, true, true),
    (91, true, true),
    (92, true, true),
    (93, true, true),
    (94, true, true),
    (96, true, true),
    (98, true, true),
    (254, true, true),
    (260, true, true),
    (352, false, true),
    (385, false, true),
    (387, true, true),
    (410, true, true),
    (416, true, true),
    (532, true, true),
    (533, true, true),
    (607, true, true),
    (620, true, true),
    (649, true, true),
    (670, true, true),
    (900, true, false),
    (974, false, true),
    (1054, true, true),
    (1070, true, true),
    (1093, true, true),
    (1108, true, true),
    (1120, true, true),
    (1130, true, true),
    (1201, true, true),
    (1220, true, true),
    (1320, true, true),
    (1330, true, true),
    (1550, true, true),
    (1752, true, true),
    (1784, true, false),
    (1804, true, false),
    (1862, true, true),
    (1868, true, true),
    (1955, true, true),
    (2060, true, true),
    (2347, true, false),
    (2360, true, false),
    (2373, true, false),
    (2386, true, false),
    (2536, true, false),
    (2542, true, false),
    (2860, true, true),
    (2972, true, true),
    (2988, true, true),
    (3188, true, false),
    (3222, true, true),
    (3240, true, true),
    (3362, true, true),
    (3516, true, true),
    (3629, true, false),
    (3676, true, false),
    (3882, true, true),
    (5068, true, true),
];

/// `(oprcanmerge, oprcanhash)` of the operator `oid`.
pub fn operator_merge_hash(oid: Oid) -> (bool, bool) {
    OPERATOR_MERGE_HASH
        .iter()
        .find(|e| e.0 == oid)
        .map_or((false, false), |e| (e.1, e.2))
}
/// `oprcode`, `oprcom` and `oprnegate` of every operator (PostgreSQL 17
/// values), ordered by operator OID.
#[rustfmt::skip]
pub static OPERATOR_META: &[OperatorMeta] = &[
    OperatorMeta { oid: 15, proc_oid: 852, com: 416, negate: 36 },
    OperatorMeta { oid: 36, proc_oid: 853, com: 417, negate: 15 },
    OperatorMeta { oid: 37, proc_oid: 854, com: 419, negate: 82 },
    OperatorMeta { oid: 58, proc_oid: 56, com: 59, negate: 1695 },
    OperatorMeta { oid: 59, proc_oid: 57, com: 58, negate: 1694 },
    OperatorMeta { oid: 76, proc_oid: 855, com: 418, negate: 80 },
    OperatorMeta { oid: 80, proc_oid: 856, com: 430, negate: 76 },
    OperatorMeta { oid: 82, proc_oid: 857, com: 420, negate: 37 },
    OperatorMeta { oid: 85, proc_oid: 84, com: 85, negate: 91 },
    OperatorMeta { oid: 91, proc_oid: 60, com: 91, negate: 85 },
    OperatorMeta { oid: 92, proc_oid: 61, com: 92, negate: 630 },
    OperatorMeta { oid: 93, proc_oid: 62, com: 93, negate: 643 },
    OperatorMeta { oid: 94, proc_oid: 63, com: 94, negate: 519 },
    OperatorMeta { oid: 95, proc_oid: 64, com: 520, negate: 524 },
    OperatorMeta { oid: 96, proc_oid: 65, com: 96, negate: 518 },
    OperatorMeta { oid: 97, proc_oid: 66, com: 521, negate: 525 },
    OperatorMeta { oid: 98, proc_oid: 67, com: 98, negate: 531 },
    OperatorMeta { oid: 254, proc_oid: 240, com: 260, negate: 259 },
    OperatorMeta { oid: 255, proc_oid: 241, com: 264, negate: 257 },
    OperatorMeta { oid: 256, proc_oid: 242, com: 263, negate: 258 },
    OperatorMeta { oid: 257, proc_oid: 243, com: 262, negate: 255 },
    OperatorMeta { oid: 258, proc_oid: 244, com: 261, negate: 256 },
    OperatorMeta { oid: 259, proc_oid: 245, com: 265, negate: 254 },
    OperatorMeta { oid: 260, proc_oid: 247, com: 254, negate: 265 },
    OperatorMeta { oid: 261, proc_oid: 248, com: 258, negate: 263 },
    OperatorMeta { oid: 262, proc_oid: 249, com: 257, negate: 264 },
    OperatorMeta { oid: 263, proc_oid: 250, com: 256, negate: 261 },
    OperatorMeta { oid: 264, proc_oid: 251, com: 255, negate: 262 },
    OperatorMeta { oid: 265, proc_oid: 252, com: 259, negate: 260 },
    OperatorMeta { oid: 352, proc_oid: 68, com: 352, negate: 3315 },
    OperatorMeta { oid: 353, proc_oid: 1319, com: 0, negate: 3316 },
    OperatorMeta { oid: 385, proc_oid: 69, com: 385, negate: 0 },
    OperatorMeta { oid: 387, proc_oid: 1292, com: 387, negate: 402 },
    OperatorMeta { oid: 402, proc_oid: 1265, com: 402, negate: 387 },
    OperatorMeta { oid: 410, proc_oid: 467, com: 410, negate: 411 },
    OperatorMeta { oid: 411, proc_oid: 468, com: 411, negate: 410 },
    OperatorMeta { oid: 412, proc_oid: 469, com: 413, negate: 415 },
    OperatorMeta { oid: 413, proc_oid: 470, com: 412, negate: 414 },
    OperatorMeta { oid: 414, proc_oid: 471, com: 415, negate: 413 },
    OperatorMeta { oid: 415, proc_oid: 472, com: 414, negate: 412 },
    OperatorMeta { oid: 416, proc_oid: 474, com: 15, negate: 417 },
    OperatorMeta { oid: 417, proc_oid: 475, com: 36, negate: 416 },
    OperatorMeta { oid: 418, proc_oid: 476, com: 76, negate: 430 },
    OperatorMeta { oid: 419, proc_oid: 477, com: 37, negate: 420 },
    OperatorMeta { oid: 420, proc_oid: 478, com: 82, negate: 419 },
    OperatorMeta { oid: 430, proc_oid: 479, com: 80, negate: 418 },
    OperatorMeta { oid: 439, proc_oid: 945, com: 0, negate: 0 },
    OperatorMeta { oid: 473, proc_oid: 1230, com: 0, negate: 0 },
    OperatorMeta { oid: 484, proc_oid: 462, com: 0, negate: 0 },
    OperatorMeta { oid: 514, proc_oid: 141, com: 514, negate: 0 },
    OperatorMeta { oid: 518, proc_oid: 144, com: 518, negate: 96 },
    OperatorMeta { oid: 519, proc_oid: 145, com: 519, negate: 94 },
    OperatorMeta { oid: 520, proc_oid: 146, com: 95, negate: 522 },
    OperatorMeta { oid: 521, proc_oid: 147, com: 97, negate: 523 },
    OperatorMeta { oid: 522, proc_oid: 148, com: 524, negate: 520 },
    OperatorMeta { oid: 523, proc_oid: 149, com: 525, negate: 521 },
    OperatorMeta { oid: 524, proc_oid: 151, com: 522, negate: 95 },
    OperatorMeta { oid: 525, proc_oid: 150, com: 523, negate: 97 },
    OperatorMeta { oid: 526, proc_oid: 152, com: 526, negate: 0 },
    OperatorMeta { oid: 527, proc_oid: 153, com: 0, negate: 0 },
    OperatorMeta { oid: 528, proc_oid: 154, com: 0, negate: 0 },
    OperatorMeta { oid: 529, proc_oid: 155, com: 0, negate: 0 },
    OperatorMeta { oid: 530, proc_oid: 156, com: 0, negate: 0 },
    OperatorMeta { oid: 531, proc_oid: 157, com: 531, negate: 98 },
    OperatorMeta { oid: 532, proc_oid: 158, com: 533, negate: 538 },
    OperatorMeta { oid: 533, proc_oid: 159, com: 532, negate: 539 },
    OperatorMeta { oid: 534, proc_oid: 160, com: 537, negate: 542 },
    OperatorMeta { oid: 535, proc_oid: 161, com: 536, negate: 543 },
    OperatorMeta { oid: 536, proc_oid: 162, com: 535, negate: 540 },
    OperatorMeta { oid: 537, proc_oid: 163, com: 534, negate: 541 },
    OperatorMeta { oid: 538, proc_oid: 164, com: 539, negate: 532 },
    OperatorMeta { oid: 539, proc_oid: 165, com: 538, negate: 533 },
    OperatorMeta { oid: 540, proc_oid: 166, com: 543, negate: 536 },
    OperatorMeta { oid: 541, proc_oid: 167, com: 542, negate: 537 },
    OperatorMeta { oid: 542, proc_oid: 168, com: 541, negate: 534 },
    OperatorMeta { oid: 543, proc_oid: 169, com: 540, negate: 535 },
    OperatorMeta { oid: 544, proc_oid: 170, com: 545, negate: 0 },
    OperatorMeta { oid: 545, proc_oid: 171, com: 544, negate: 0 },
    OperatorMeta { oid: 546, proc_oid: 172, com: 0, negate: 0 },
    OperatorMeta { oid: 547, proc_oid: 173, com: 0, negate: 0 },
    OperatorMeta { oid: 550, proc_oid: 176, com: 550, negate: 0 },
    OperatorMeta { oid: 551, proc_oid: 177, com: 551, negate: 0 },
    OperatorMeta { oid: 552, proc_oid: 178, com: 553, negate: 0 },
    OperatorMeta { oid: 553, proc_oid: 179, com: 552, negate: 0 },
    OperatorMeta { oid: 554, proc_oid: 180, com: 0, negate: 0 },
    OperatorMeta { oid: 555, proc_oid: 181, com: 0, negate: 0 },
    OperatorMeta { oid: 556, proc_oid: 182, com: 0, negate: 0 },
    OperatorMeta { oid: 557, proc_oid: 183, com: 0, negate: 0 },
    OperatorMeta { oid: 558, proc_oid: 212, com: 0, negate: 0 },
    OperatorMeta { oid: 559, proc_oid: 213, com: 0, negate: 0 },
    OperatorMeta { oid: 584, proc_oid: 206, com: 0, negate: 0 },
    OperatorMeta { oid: 585, proc_oid: 220, com: 0, negate: 0 },
    OperatorMeta { oid: 586, proc_oid: 204, com: 586, negate: 0 },
    OperatorMeta { oid: 587, proc_oid: 205, com: 0, negate: 0 },
    OperatorMeta { oid: 588, proc_oid: 203, com: 0, negate: 0 },
    OperatorMeta { oid: 589, proc_oid: 202, com: 589, negate: 0 },
    OperatorMeta { oid: 590, proc_oid: 207, com: 0, negate: 0 },
    OperatorMeta { oid: 591, proc_oid: 218, com: 591, negate: 0 },
    OperatorMeta { oid: 592, proc_oid: 219, com: 0, negate: 0 },
    OperatorMeta { oid: 593, proc_oid: 217, com: 0, negate: 0 },
    OperatorMeta { oid: 594, proc_oid: 216, com: 594, negate: 0 },
    OperatorMeta { oid: 595, proc_oid: 221, com: 0, negate: 0 },
    OperatorMeta { oid: 607, proc_oid: 184, com: 607, negate: 608 },
    OperatorMeta { oid: 608, proc_oid: 185, com: 608, negate: 607 },
    OperatorMeta { oid: 609, proc_oid: 716, com: 610, negate: 612 },
    OperatorMeta { oid: 610, proc_oid: 1638, com: 609, negate: 611 },
    OperatorMeta { oid: 611, proc_oid: 717, com: 612, negate: 610 },
    OperatorMeta { oid: 612, proc_oid: 1639, com: 611, negate: 609 },
    OperatorMeta { oid: 620, proc_oid: 287, com: 620, negate: 621 },
    OperatorMeta { oid: 621, proc_oid: 288, com: 621, negate: 620 },
    OperatorMeta { oid: 622, proc_oid: 289, com: 623, negate: 625 },
    OperatorMeta { oid: 623, proc_oid: 291, com: 622, negate: 624 },
    OperatorMeta { oid: 624, proc_oid: 290, com: 625, negate: 623 },
    OperatorMeta { oid: 625, proc_oid: 292, com: 624, negate: 622 },
    OperatorMeta { oid: 630, proc_oid: 70, com: 630, negate: 92 },
    OperatorMeta { oid: 631, proc_oid: 1246, com: 633, negate: 634 },
    OperatorMeta { oid: 632, proc_oid: 72, com: 634, negate: 633 },
    OperatorMeta { oid: 633, proc_oid: 73, com: 631, negate: 632 },
    OperatorMeta { oid: 634, proc_oid: 74, com: 632, negate: 631 },
    OperatorMeta { oid: 641, proc_oid: 1244, com: 0, negate: 642 },
    OperatorMeta { oid: 642, proc_oid: 1240, com: 0, negate: 641 },
    OperatorMeta { oid: 643, proc_oid: 659, com: 643, negate: 93 },
    OperatorMeta { oid: 654, proc_oid: 1258, com: 0, negate: 0 },
    OperatorMeta { oid: 660, proc_oid: 655, com: 662, negate: 663 },
    OperatorMeta { oid: 661, proc_oid: 656, com: 663, negate: 662 },
    OperatorMeta { oid: 662, proc_oid: 657, com: 660, negate: 661 },
    OperatorMeta { oid: 663, proc_oid: 658, com: 661, negate: 660 },
    OperatorMeta { oid: 664, proc_oid: 740, com: 666, negate: 667 },
    OperatorMeta { oid: 665, proc_oid: 741, com: 667, negate: 666 },
    OperatorMeta { oid: 666, proc_oid: 742, com: 664, negate: 665 },
    OperatorMeta { oid: 667, proc_oid: 743, com: 665, negate: 664 },
    OperatorMeta { oid: 670, proc_oid: 293, com: 670, negate: 671 },
    OperatorMeta { oid: 671, proc_oid: 294, com: 671, negate: 670 },
    OperatorMeta { oid: 672, proc_oid: 295, com: 674, negate: 675 },
    OperatorMeta { oid: 673, proc_oid: 296, com: 675, negate: 674 },
    OperatorMeta { oid: 674, proc_oid: 297, com: 672, negate: 673 },
    OperatorMeta { oid: 675, proc_oid: 298, com: 673, negate: 672 },
    OperatorMeta { oid: 682, proc_oid: 1253, com: 0, negate: 0 },
    OperatorMeta { oid: 684, proc_oid: 463, com: 684, negate: 0 },
    OperatorMeta { oid: 685, proc_oid: 464, com: 0, negate: 0 },
    OperatorMeta { oid: 686, proc_oid: 465, com: 686, negate: 0 },
    OperatorMeta { oid: 687, proc_oid: 466, com: 0, negate: 0 },
    OperatorMeta { oid: 688, proc_oid: 1274, com: 692, negate: 0 },
    OperatorMeta { oid: 689, proc_oid: 1275, com: 0, negate: 0 },
    OperatorMeta { oid: 690, proc_oid: 1276, com: 694, negate: 0 },
    OperatorMeta { oid: 691, proc_oid: 1277, com: 0, negate: 0 },
    OperatorMeta { oid: 692, proc_oid: 1278, com: 688, negate: 0 },
    OperatorMeta { oid: 693, proc_oid: 1279, com: 0, negate: 0 },
    OperatorMeta { oid: 694, proc_oid: 1280, com: 690, negate: 0 },
    OperatorMeta { oid: 695, proc_oid: 1281, com: 0, negate: 0 },
    OperatorMeta { oid: 773, proc_oid: 1251, com: 0, negate: 0 },
    OperatorMeta { oid: 818, proc_oid: 837, com: 822, negate: 0 },
    OperatorMeta { oid: 819, proc_oid: 838, com: 0, negate: 0 },
    OperatorMeta { oid: 820, proc_oid: 839, com: 824, negate: 0 },
    OperatorMeta { oid: 821, proc_oid: 840, com: 0, negate: 0 },
    OperatorMeta { oid: 822, proc_oid: 841, com: 818, negate: 0 },
    OperatorMeta { oid: 823, proc_oid: 942, com: 0, negate: 0 },
    OperatorMeta { oid: 824, proc_oid: 943, com: 820, negate: 0 },
    OperatorMeta { oid: 825, proc_oid: 948, com: 0, negate: 0 },
    OperatorMeta { oid: 965, proc_oid: 232, com: 0, negate: 0 },
    OperatorMeta { oid: 1038, proc_oid: 1739, com: 0, negate: 0 },
    OperatorMeta { oid: 1093, proc_oid: 1086, com: 1093, negate: 1094 },
    OperatorMeta { oid: 1094, proc_oid: 1091, com: 1094, negate: 1093 },
    OperatorMeta { oid: 1095, proc_oid: 1087, com: 1097, negate: 1098 },
    OperatorMeta { oid: 1096, proc_oid: 1088, com: 1098, negate: 1097 },
    OperatorMeta { oid: 1097, proc_oid: 1089, com: 1095, negate: 1096 },
    OperatorMeta { oid: 1098, proc_oid: 1090, com: 1096, negate: 1095 },
    OperatorMeta { oid: 1099, proc_oid: 1140, com: 0, negate: 0 },
    OperatorMeta { oid: 1100, proc_oid: 1141, com: 2555, negate: 0 },
    OperatorMeta { oid: 1101, proc_oid: 1142, com: 0, negate: 0 },
    OperatorMeta { oid: 1116, proc_oid: 281, com: 1126, negate: 0 },
    OperatorMeta { oid: 1117, proc_oid: 282, com: 0, negate: 0 },
    OperatorMeta { oid: 1118, proc_oid: 280, com: 0, negate: 0 },
    OperatorMeta { oid: 1119, proc_oid: 279, com: 1129, negate: 0 },
    OperatorMeta { oid: 1120, proc_oid: 299, com: 1130, negate: 1121 },
    OperatorMeta { oid: 1121, proc_oid: 300, com: 1131, negate: 1120 },
    OperatorMeta { oid: 1122, proc_oid: 301, com: 1133, negate: 1125 },
    OperatorMeta { oid: 1123, proc_oid: 303, com: 1132, negate: 1124 },
    OperatorMeta { oid: 1124, proc_oid: 302, com: 1135, negate: 1123 },
    OperatorMeta { oid: 1125, proc_oid: 304, com: 1134, negate: 1122 },
    OperatorMeta { oid: 1126, proc_oid: 285, com: 1116, negate: 0 },
    OperatorMeta { oid: 1127, proc_oid: 286, com: 0, negate: 0 },
    OperatorMeta { oid: 1128, proc_oid: 284, com: 0, negate: 0 },
    OperatorMeta { oid: 1129, proc_oid: 283, com: 1119, negate: 0 },
    OperatorMeta { oid: 1130, proc_oid: 305, com: 1120, negate: 1131 },
    OperatorMeta { oid: 1131, proc_oid: 306, com: 1121, negate: 1130 },
    OperatorMeta { oid: 1132, proc_oid: 307, com: 1123, negate: 1135 },
    OperatorMeta { oid: 1133, proc_oid: 309, com: 1122, negate: 1134 },
    OperatorMeta { oid: 1134, proc_oid: 308, com: 1125, negate: 1133 },
    OperatorMeta { oid: 1135, proc_oid: 310, com: 1124, negate: 1132 },
    OperatorMeta { oid: 1209, proc_oid: 850, com: 0, negate: 1210 },
    OperatorMeta { oid: 1210, proc_oid: 851, com: 0, negate: 1209 },
    OperatorMeta { oid: 1228, proc_oid: 1238, com: 0, negate: 1229 },
    OperatorMeta { oid: 1229, proc_oid: 1239, com: 0, negate: 1228 },
    OperatorMeta { oid: 1330, proc_oid: 1162, com: 1330, negate: 1331 },
    OperatorMeta { oid: 1331, proc_oid: 1163, com: 1331, negate: 1330 },
    OperatorMeta { oid: 1332, proc_oid: 1164, com: 1334, negate: 1335 },
    OperatorMeta { oid: 1333, proc_oid: 1165, com: 1335, negate: 1334 },
    OperatorMeta { oid: 1334, proc_oid: 1167, com: 1332, negate: 1333 },
    OperatorMeta { oid: 1335, proc_oid: 1166, com: 1333, negate: 1332 },
    OperatorMeta { oid: 1336, proc_oid: 1168, com: 0, negate: 0 },
    OperatorMeta { oid: 1337, proc_oid: 1169, com: 1337, negate: 0 },
    OperatorMeta { oid: 1338, proc_oid: 1170, com: 0, negate: 0 },
    OperatorMeta { oid: 1583, proc_oid: 1618, com: 1584, negate: 0 },
    OperatorMeta { oid: 1584, proc_oid: 1624, com: 1583, negate: 0 },
    OperatorMeta { oid: 1585, proc_oid: 1326, com: 0, negate: 0 },
    OperatorMeta { oid: 1627, proc_oid: 1633, com: 0, negate: 1628 },
    OperatorMeta { oid: 1628, proc_oid: 1634, com: 0, negate: 1627 },
    OperatorMeta { oid: 1694, proc_oid: 1691, com: 1695, negate: 59 },
    OperatorMeta { oid: 1695, proc_oid: 1692, com: 1694, negate: 58 },
    OperatorMeta { oid: 1751, proc_oid: 1771, com: 0, negate: 0 },
    OperatorMeta { oid: 1752, proc_oid: 1718, com: 1752, negate: 1753 },
    OperatorMeta { oid: 1753, proc_oid: 1719, com: 1753, negate: 1752 },
    OperatorMeta { oid: 1754, proc_oid: 1722, com: 1756, negate: 1757 },
    OperatorMeta { oid: 1755, proc_oid: 1723, com: 1757, negate: 1756 },
    OperatorMeta { oid: 1756, proc_oid: 1720, com: 1754, negate: 1755 },
    OperatorMeta { oid: 1757, proc_oid: 1721, com: 1755, negate: 1754 },
    OperatorMeta { oid: 1758, proc_oid: 1724, com: 1758, negate: 0 },
    OperatorMeta { oid: 1759, proc_oid: 1725, com: 0, negate: 0 },
    OperatorMeta { oid: 1760, proc_oid: 1726, com: 1760, negate: 0 },
    OperatorMeta { oid: 1761, proc_oid: 1727, com: 0, negate: 0 },
    OperatorMeta { oid: 1762, proc_oid: 1729, com: 0, negate: 0 },
    OperatorMeta { oid: 1763, proc_oid: 1704, com: 0, negate: 0 },
    OperatorMeta { oid: 1862, proc_oid: 1850, com: 1868, negate: 1863 },
    OperatorMeta { oid: 1863, proc_oid: 1851, com: 1869, negate: 1862 },
    OperatorMeta { oid: 1864, proc_oid: 1852, com: 1871, negate: 1867 },
    OperatorMeta { oid: 1865, proc_oid: 1853, com: 1870, negate: 1866 },
    OperatorMeta { oid: 1866, proc_oid: 1854, com: 1873, negate: 1865 },
    OperatorMeta { oid: 1867, proc_oid: 1855, com: 1872, negate: 1864 },
    OperatorMeta { oid: 1868, proc_oid: 1856, com: 1862, negate: 1869 },
    OperatorMeta { oid: 1869, proc_oid: 1857, com: 1863, negate: 1868 },
    OperatorMeta { oid: 1870, proc_oid: 1858, com: 1865, negate: 1873 },
    OperatorMeta { oid: 1871, proc_oid: 1859, com: 1864, negate: 1872 },
    OperatorMeta { oid: 1872, proc_oid: 1860, com: 1867, negate: 1871 },
    OperatorMeta { oid: 1873, proc_oid: 1861, com: 1866, negate: 1870 },
    OperatorMeta { oid: 1874, proc_oid: 1892, com: 0, negate: 0 },
    OperatorMeta { oid: 1875, proc_oid: 1893, com: 0, negate: 0 },
    OperatorMeta { oid: 1876, proc_oid: 1894, com: 0, negate: 0 },
    OperatorMeta { oid: 1877, proc_oid: 1895, com: 0, negate: 0 },
    OperatorMeta { oid: 1878, proc_oid: 1896, com: 0, negate: 0 },
    OperatorMeta { oid: 1879, proc_oid: 1897, com: 0, negate: 0 },
    OperatorMeta { oid: 1880, proc_oid: 1898, com: 0, negate: 0 },
    OperatorMeta { oid: 1881, proc_oid: 1899, com: 0, negate: 0 },
    OperatorMeta { oid: 1882, proc_oid: 1900, com: 0, negate: 0 },
    OperatorMeta { oid: 1883, proc_oid: 1901, com: 0, negate: 0 },
    OperatorMeta { oid: 1884, proc_oid: 1902, com: 0, negate: 0 },
    OperatorMeta { oid: 1885, proc_oid: 1903, com: 0, negate: 0 },
    OperatorMeta { oid: 1886, proc_oid: 1904, com: 0, negate: 0 },
    OperatorMeta { oid: 1887, proc_oid: 1905, com: 0, negate: 0 },
    OperatorMeta { oid: 1888, proc_oid: 1906, com: 0, negate: 0 },
    OperatorMeta { oid: 1889, proc_oid: 1907, com: 0, negate: 0 },
    OperatorMeta { oid: 1890, proc_oid: 1908, com: 0, negate: 0 },
    OperatorMeta { oid: 1891, proc_oid: 1909, com: 0, negate: 0 },
    OperatorMeta { oid: 1916, proc_oid: 1910, com: 0, negate: 0 },
    OperatorMeta { oid: 1917, proc_oid: 1911, com: 0, negate: 0 },
    OperatorMeta { oid: 1918, proc_oid: 1912, com: 0, negate: 0 },
    OperatorMeta { oid: 1919, proc_oid: 1913, com: 0, negate: 0 },
    OperatorMeta { oid: 1920, proc_oid: 1914, com: 0, negate: 0 },
    OperatorMeta { oid: 1921, proc_oid: 1915, com: 0, negate: 0 },
    OperatorMeta { oid: 2555, proc_oid: 2550, com: 1100, negate: 0 },
    OperatorMeta { oid: 2779, proc_oid: 2003, com: 0, negate: 0 },
    OperatorMeta { oid: 2780, proc_oid: 2004, com: 0, negate: 0 },
    OperatorMeta { oid: 2799, proc_oid: 2791, com: 2800, negate: 2802 },
    OperatorMeta { oid: 2800, proc_oid: 2790, com: 2799, negate: 2801 },
    OperatorMeta { oid: 2801, proc_oid: 2793, com: 2802, negate: 2800 },
    OperatorMeta { oid: 2802, proc_oid: 2792, com: 2801, negate: 2799 },
    OperatorMeta { oid: 3315, proc_oid: 3308, com: 3315, negate: 352 },
    OperatorMeta { oid: 3316, proc_oid: 3309, com: 0, negate: 353 },
];

/// Built-in functions (`pg_proc`). `current_database()` and
/// `current_schema()` are resolved here but replaced by the analyzer with
/// a session value (built-ins cannot see the session).
#[rustfmt::skip]
pub static FUNCTIONS: &[BuiltinFunction] = &[
    func(1394, "abs", &[oid::FLOAT4], oid::FLOAT4, ops::float4abs),
    func(1395, "abs", &[oid::FLOAT8], oid::FLOAT8, ops::float8abs),
    func(1396, "abs", &[oid::INT8], oid::INT8, ops::int8abs),
    func(1397, "abs", &[oid::INT4], oid::INT4, ops::int4abs),
    func(1398, "abs", &[oid::INT2], oid::INT2, ops::int2abs),
    func(1705, "abs", &[NUMERIC], NUMERIC, ops::numeric_abs),
    func(1706, "sign", &[NUMERIC], NUMERIC, ops::numeric_sign),
    func(1730, "sqrt", &[NUMERIC], NUMERIC, ops::numeric_sqrt),
    func(1734, "ln", &[NUMERIC], NUMERIC, ops::numeric_ln),
    func(1736, "log", &[NUMERIC, NUMERIC], NUMERIC, ops::numeric_log),
    func(1741, "log", &[NUMERIC], NUMERIC, ops::numeric_log10),
    func(1481, "log10", &[NUMERIC], NUMERIC, ops::numeric_log10),
    func(1707, "round", &[NUMERIC, oid::INT4], NUMERIC, ops::numeric_round),
    func(1708, "round", &[NUMERIC], NUMERIC, ops::numeric_round0),
    func(1709, "trunc", &[NUMERIC, oid::INT4], NUMERIC, ops::numeric_trunc),
    func(1710, "trunc", &[NUMERIC], NUMERIC, ops::numeric_trunc0),
    func(1711, "ceil", &[NUMERIC], NUMERIC, ops::numeric_ceil),
    func(2167, "ceiling", &[NUMERIC], NUMERIC, ops::numeric_ceil),
    func(1712, "floor", &[NUMERIC], NUMERIC, ops::numeric_floor),
    func(1257, "textlen", &[oid::TEXT], oid::INT4, ops::textlen),
    func(1317, "length", &[oid::TEXT], oid::INT4, ops::textlen),
    func(1622, "repeat", &[oid::TEXT, oid::INT4], oid::TEXT, ops::repeat),
    func(870, "lower", &[oid::TEXT], oid::TEXT, ops::lower),
    func(871, "upper", &[oid::TEXT], oid::TEXT, ops::upper),
    func(1258, "textcat", &[oid::TEXT, oid::TEXT], oid::TEXT, ops::textcat),
    func(89, "version", &[], oid::TEXT, ops::version),
    func(861, "current_database", &[], oid::NAME, ops::unsupported),
    func(1402, "current_schema", &[], oid::NAME, ops::unsupported),
    func(313, "int4", &[oid::INT2], oid::INT4, ops::to_int4),
    func(314, "int2", &[oid::INT4], oid::INT2, ops::to_int2),
    func(481, "int8", &[oid::INT4], oid::INT8, ops::to_int8),
    func(480, "int4", &[oid::INT8], oid::INT4, ops::to_int4),
    func(754, "int8", &[oid::INT2], oid::INT8, ops::to_int8),
    func(714, "int2", &[oid::INT8], oid::INT2, ops::to_int2),
    func(236, "float4", &[oid::INT2], oid::FLOAT4, ops::to_float4),
    func(235, "float8", &[oid::INT2], oid::FLOAT8, ops::to_float8),
    func(318, "float4", &[oid::INT4], oid::FLOAT4, ops::to_float4),
    func(316, "float8", &[oid::INT4], oid::FLOAT8, ops::to_float8),
    func(652, "float4", &[oid::INT8], oid::FLOAT4, ops::to_float4),
    func(482, "float8", &[oid::INT8], oid::FLOAT8, ops::to_float8),
    func(238, "int2", &[oid::FLOAT4], oid::INT2, ops::to_int2),
    func(319, "int4", &[oid::FLOAT4], oid::INT4, ops::to_int4),
    func(653, "int8", &[oid::FLOAT4], oid::INT8, ops::to_int8),
    func(237, "int2", &[oid::FLOAT8], oid::INT2, ops::to_int2),
    func(317, "int4", &[oid::FLOAT8], oid::INT4, ops::to_int4),
    func(483, "int8", &[oid::FLOAT8], oid::INT8, ops::to_int8),
    func(311, "float8", &[oid::FLOAT4], oid::FLOAT8, ops::to_float8),
    func(312, "float4", &[oid::FLOAT8], oid::FLOAT4, ops::to_float4),
    func(2557, "bool", &[oid::INT4], oid::BOOL, ops::int4_to_bool),
    func(2558, "int4", &[oid::BOOL], oid::INT4, ops::bool_to_int4),
    func(2971, "text", &[oid::BOOL], oid::TEXT, ops::bool_to_text),
    func(407, "name", &[oid::TEXT], oid::NAME, ops::text_to_name),
    func(406, "text", &[oid::NAME], oid::TEXT, ops::text_identity),
    func(1400, "name", &[oid::VARCHAR], oid::NAME, ops::text_to_name),
    func(1401, "varchar", &[oid::NAME], oid::VARCHAR, ops::text_identity),
    ctx_func(1642, "pg_get_userbyid", &[oid::OID], oid::NAME, pg_get_userbyid),
    func(1597, "pg_encoding_to_char", &[oid::INT4], oid::NAME, ops::pg_encoding_to_char),
    func(2176, "array_length", &[oid::ANYARRAY, oid::INT4], oid::INT4, ops::array_unsupported),
    func(395, "array_to_string", &[oid::ANYARRAY, oid::TEXT], oid::TEXT, ops::array_unsupported),
    ctx_func(2079, "pg_table_is_visible", &[oid::OID], oid::BOOL, pg_table_is_visible),
    nonstrict_func(1081, "format_type", &[oid::OID, oid::INT4], oid::TEXT, format_type),
    func(1716, "pg_get_expr", &[oid::PG_NODE_TREE, oid::OID], oid::TEXT, ops::pg_get_expr),
    runtime_func(2026, "pg_backend_pid", &[], oid::INT4, pg_backend_pid),
    runtime_func(2077, "current_setting", &[oid::TEXT], oid::TEXT, current_setting),
    runtime_func(3294, "current_setting", &[oid::TEXT, oid::BOOL], oid::TEXT, current_setting_missing_ok),
    nonstrict_runtime_func(2078, "set_config", &[oid::TEXT, oid::TEXT, oid::BOOL], oid::TEXT, set_config),
    runtime_func(3348, "txid_current_if_assigned", &[], oid::INT8, txid_current_if_assigned),
    runtime_func(2626, "pg_sleep", &[oid::FLOAT8], oid::VOID, pg_sleep),
    runtime_func(3378, "pg_isolation_test_session_is_blocked", &[oid::INT4, oid::INT4_ARRAY], oid::BOOL, pg_isolation_test_session_is_blocked),
    func(877, "substr", &[oid::TEXT, oid::INT4, oid::INT4], oid::TEXT, funcs::substr),
    func(883, "substr", &[oid::TEXT, oid::INT4], oid::TEXT, funcs::substr_no_len),
    func(936, "substring", &[oid::TEXT, oid::INT4, oid::INT4], oid::TEXT, funcs::substr),
    func(937, "substring", &[oid::TEXT, oid::INT4], oid::TEXT, funcs::substr_no_len),
    func(3060, "left", &[oid::TEXT, oid::INT4], oid::TEXT, funcs::left),
    func(3061, "right", &[oid::TEXT, oid::INT4], oid::TEXT, funcs::right),
    func(2087, "replace", &[oid::TEXT, oid::TEXT, oid::TEXT], oid::TEXT, funcs::replace),
    func(884, "btrim", &[oid::TEXT, oid::TEXT], oid::TEXT, funcs::btrim),
    func(885, "btrim", &[oid::TEXT], oid::TEXT, funcs::btrim),
    func(875, "ltrim", &[oid::TEXT, oid::TEXT], oid::TEXT, funcs::ltrim),
    func(881, "ltrim", &[oid::TEXT], oid::TEXT, funcs::ltrim),
    func(876, "rtrim", &[oid::TEXT, oid::TEXT], oid::TEXT, funcs::rtrim),
    func(882, "rtrim", &[oid::TEXT], oid::TEXT, funcs::rtrim),
    func(849, "position", &[oid::TEXT, oid::TEXT], oid::INT4, funcs::strpos),
    func(868, "strpos", &[oid::TEXT, oid::TEXT], oid::INT4, funcs::strpos),
    func(879, "lpad", &[oid::TEXT, oid::INT4, oid::TEXT], oid::TEXT, funcs::lpad),
    func(873, "lpad", &[oid::TEXT, oid::INT4], oid::TEXT, funcs::lpad),
    func(880, "rpad", &[oid::TEXT, oid::INT4, oid::TEXT], oid::TEXT, funcs::rpad),
    func(874, "rpad", &[oid::TEXT, oid::INT4], oid::TEXT, funcs::rpad),
    func(1381, "char_length", &[oid::TEXT], oid::INT4, funcs::char_length),
    func(1367, "character_length", &[oid::TEXT], oid::INT4, funcs::char_length),
    func(3062, "reverse", &[oid::TEXT], oid::TEXT, funcs::reverse),
    func(872, "initcap", &[oid::TEXT], oid::TEXT, funcs::initcap),
    func(1620, "ascii", &[oid::TEXT], oid::INT4, funcs::ascii),
    func(1621, "chr", &[oid::INT4], oid::TEXT, funcs::chr),
    func(2311, "md5", &[oid::TEXT], oid::TEXT, funcs::md5),
    func(940, "mod", &[oid::INT2, oid::INT2], oid::INT2, ops::int2mod),
    func(941, "mod", &[oid::INT4, oid::INT4], oid::INT4, ops::int4mod),
    func(947, "mod", &[oid::INT8, oid::INT8], oid::INT8, ops::int8mod),
    func(2308, "ceil", &[oid::FLOAT8], oid::FLOAT8, funcs::dceil),
    func(2320, "ceiling", &[oid::FLOAT8], oid::FLOAT8, funcs::dceil),
    func(2309, "floor", &[oid::FLOAT8], oid::FLOAT8, funcs::dfloor),
    func(2310, "sign", &[oid::FLOAT8], oid::FLOAT8, funcs::dsign),
    func(2339, "round", &[oid::FLOAT8], oid::FLOAT8, funcs::dround),
    func(2340, "trunc", &[oid::FLOAT8], oid::FLOAT8, funcs::dtrunc),
    func(1344, "sqrt", &[oid::FLOAT8], oid::FLOAT8, funcs::dsqrt),
    func(1345, "cbrt", &[oid::FLOAT8], oid::FLOAT8, funcs::dcbrt),
    func(1347, "exp", &[oid::FLOAT8], oid::FLOAT8, funcs::dexp),
    func(1341, "ln", &[oid::FLOAT8], oid::FLOAT8, funcs::dlog1),
    func(1340, "log", &[oid::FLOAT8], oid::FLOAT8, funcs::dlog10),
    func(1339, "log10", &[oid::FLOAT8], oid::FLOAT8, funcs::dlog10),
    func(1368, "power", &[oid::FLOAT8, oid::FLOAT8], oid::FLOAT8, ops::float8pow),
    func(2088, "split_part", &[oid::TEXT, oid::TEXT, oid::INT4], oid::TEXT, funcs::split_part),
    func(3696, "starts_with", &[oid::TEXT, oid::TEXT], oid::BOOL, funcs::starts_with),
    func(878, "translate", &[oid::TEXT, oid::TEXT, oid::TEXT], oid::TEXT, funcs::translate),
    func(1282, "quote_ident", &[oid::TEXT], oid::TEXT, funcs::quote_ident),
    func(1283, "quote_literal", &[oid::TEXT], oid::TEXT, funcs::quote_literal),
    func(1374, "octet_length", &[oid::TEXT], oid::INT4, funcs::octet_length),
    func(1811, "bit_length", &[oid::TEXT], oid::INT4, funcs::bit_length),
    func(1973, "div", &[NUMERIC, NUMERIC], NUMERIC, funcs::numeric_div_trunc),
    func(5044, "gcd", &[oid::INT4, oid::INT4], oid::INT4, funcs::int4gcd),
    func(5045, "gcd", &[oid::INT8, oid::INT8], oid::INT8, funcs::int8gcd),
    func(5046, "lcm", &[oid::INT4, oid::INT4], oid::INT4, funcs::int4lcm),
    func(5047, "lcm", &[oid::INT8, oid::INT8], oid::INT8, funcs::int8lcm),
    func(1600, "asin", &[oid::FLOAT8], oid::FLOAT8, funcs::dasin),
    func(1601, "acos", &[oid::FLOAT8], oid::FLOAT8, funcs::dacos),
    func(1602, "atan", &[oid::FLOAT8], oid::FLOAT8, funcs::datan),
    func(1603, "atan2", &[oid::FLOAT8, oid::FLOAT8], oid::FLOAT8, funcs::datan2),
    func(1604, "sin", &[oid::FLOAT8], oid::FLOAT8, funcs::dsin),
    func(1605, "cos", &[oid::FLOAT8], oid::FLOAT8, funcs::dcos),
    func(1606, "tan", &[oid::FLOAT8], oid::FLOAT8, funcs::dtan),
    func(2462, "sinh", &[oid::FLOAT8], oid::FLOAT8, funcs::dsinh),
    func(2463, "cosh", &[oid::FLOAT8], oid::FLOAT8, funcs::dcosh),
    func(2464, "tanh", &[oid::FLOAT8], oid::FLOAT8, funcs::dtanh),
    func(1608, "degrees", &[oid::FLOAT8], oid::FLOAT8, funcs::degrees),
    func(1609, "radians", &[oid::FLOAT8], oid::FLOAT8, funcs::radians),
    nonstrict_func(3058, "concat", &[oid::TEXT], oid::TEXT, funcs::concat),
    nonstrict_func(3059, "concat_ws", &[oid::TEXT, oid::TEXT], oid::TEXT, funcs::concat_ws),
];

/// Every `pg_proc` row of yuzhu (`m2.md` §6.8.2), ordered by OID: the callable
/// functions, the operator and cast functions, the type I/O functions and
/// the access method handlers. Attribute values are those of PostgreSQL 17
/// (`prolang` is always 12 and `prosupport` 0 in the catalog rows).
#[rustfmt::skip]
pub static PROCS: &[BuiltinProc] = &[
    BuiltinProc { oid: 3, name: "heap_tableam_handler", args: &[2281], result: 269, strict: true, volatility: 'v', parallel: 's', leakproof: false, cost: 1.0, prosrc: "heap_tableam_handler" },
    BuiltinProc { oid: 33, name: "charout", args: &[18], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "charout" },
    BuiltinProc { oid: 34, name: "namein", args: &[2275], result: 19, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "namein" },
    BuiltinProc { oid: 35, name: "nameout", args: &[19], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "nameout" },
    BuiltinProc { oid: 38, name: "int2in", args: &[2275], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2in" },
    BuiltinProc { oid: 39, name: "int2out", args: &[21], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2out" },
    BuiltinProc { oid: 42, name: "int4in", args: &[2275], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4in" },
    BuiltinProc { oid: 43, name: "int4out", args: &[23], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4out" },
    BuiltinProc { oid: 44, name: "regprocin", args: &[2275], result: 24, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "regprocin" },
    BuiltinProc { oid: 45, name: "regprocout", args: &[24], result: 2275, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "regprocout" },
    BuiltinProc { oid: 46, name: "textin", args: &[2275], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textin" },
    BuiltinProc { oid: 47, name: "textout", args: &[25], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textout" },
    BuiltinProc { oid: 48, name: "tidin", args: &[2275], result: 27, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "tidin" },
    BuiltinProc { oid: 49, name: "tidout", args: &[27], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "tidout" },
    BuiltinProc { oid: 50, name: "xidin", args: &[2275], result: 28, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "xidin" },
    BuiltinProc { oid: 51, name: "xidout", args: &[28], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "xidout" },
    BuiltinProc { oid: 52, name: "cidin", args: &[2275], result: 29, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "cidin" },
    BuiltinProc { oid: 53, name: "cidout", args: &[29], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "cidout" },
    BuiltinProc { oid: 54, name: "oidvectorin", args: &[2275], result: 30, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "oidvectorin" },
    BuiltinProc { oid: 55, name: "oidvectorout", args: &[30], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "oidvectorout" },
    BuiltinProc { oid: 56, name: "boollt", args: &[16, 16], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "boollt" },
    BuiltinProc { oid: 57, name: "boolgt", args: &[16, 16], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "boolgt" },
    BuiltinProc { oid: 60, name: "booleq", args: &[16, 16], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "booleq" },
    BuiltinProc { oid: 61, name: "chareq", args: &[18, 18], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "chareq" },
    BuiltinProc { oid: 62, name: "nameeq", args: &[19, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "nameeq" },
    BuiltinProc { oid: 63, name: "int2eq", args: &[21, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int2eq" },
    BuiltinProc { oid: 64, name: "int2lt", args: &[21, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int2lt" },
    BuiltinProc { oid: 65, name: "int4eq", args: &[23, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int4eq" },
    BuiltinProc { oid: 66, name: "int4lt", args: &[23, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int4lt" },
    BuiltinProc { oid: 67, name: "texteq", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "texteq" },
    BuiltinProc { oid: 68, name: "xideq", args: &[28, 28], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "xideq" },
    BuiltinProc { oid: 69, name: "cideq", args: &[29, 29], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "cideq" },
    BuiltinProc { oid: 70, name: "charne", args: &[18, 18], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "charne" },
    BuiltinProc { oid: 72, name: "charle", args: &[18, 18], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "charle" },
    BuiltinProc { oid: 73, name: "chargt", args: &[18, 18], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "chargt" },
    BuiltinProc { oid: 74, name: "charge", args: &[18, 18], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "charge" },
    BuiltinProc { oid: 77, name: "int4", args: &[18], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "chartoi4" },
    BuiltinProc { oid: 78, name: "char", args: &[23], result: 18, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "i4tochar" },
    BuiltinProc { oid: 84, name: "boolne", args: &[16, 16], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "boolne" },
    BuiltinProc { oid: 89, name: "version", args: &[], result: 25, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "pgsql_version" },
    BuiltinProc { oid: 109, name: "unknownin", args: &[2275], result: 705, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "unknownin" },
    BuiltinProc { oid: 110, name: "unknownout", args: &[705], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "unknownout" },
    BuiltinProc { oid: 141, name: "int4mul", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4mul" },
    BuiltinProc { oid: 144, name: "int4ne", args: &[23, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int4ne" },
    BuiltinProc { oid: 145, name: "int2ne", args: &[21, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int2ne" },
    BuiltinProc { oid: 146, name: "int2gt", args: &[21, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int2gt" },
    BuiltinProc { oid: 147, name: "int4gt", args: &[23, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int4gt" },
    BuiltinProc { oid: 148, name: "int2le", args: &[21, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int2le" },
    BuiltinProc { oid: 149, name: "int4le", args: &[23, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int4le" },
    BuiltinProc { oid: 150, name: "int4ge", args: &[23, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int4ge" },
    BuiltinProc { oid: 151, name: "int2ge", args: &[21, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int2ge" },
    BuiltinProc { oid: 152, name: "int2mul", args: &[21, 21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2mul" },
    BuiltinProc { oid: 153, name: "int2div", args: &[21, 21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2div" },
    BuiltinProc { oid: 154, name: "int4div", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4div" },
    BuiltinProc { oid: 155, name: "int2mod", args: &[21, 21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2mod" },
    BuiltinProc { oid: 156, name: "int4mod", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4mod" },
    BuiltinProc { oid: 157, name: "textne", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "textne" },
    BuiltinProc { oid: 158, name: "int24eq", args: &[21, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int24eq" },
    BuiltinProc { oid: 159, name: "int42eq", args: &[23, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int42eq" },
    BuiltinProc { oid: 160, name: "int24lt", args: &[21, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int24lt" },
    BuiltinProc { oid: 161, name: "int42lt", args: &[23, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int42lt" },
    BuiltinProc { oid: 162, name: "int24gt", args: &[21, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int24gt" },
    BuiltinProc { oid: 163, name: "int42gt", args: &[23, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int42gt" },
    BuiltinProc { oid: 164, name: "int24ne", args: &[21, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int24ne" },
    BuiltinProc { oid: 165, name: "int42ne", args: &[23, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int42ne" },
    BuiltinProc { oid: 166, name: "int24le", args: &[21, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int24le" },
    BuiltinProc { oid: 167, name: "int42le", args: &[23, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int42le" },
    BuiltinProc { oid: 168, name: "int24ge", args: &[21, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int24ge" },
    BuiltinProc { oid: 169, name: "int42ge", args: &[23, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int42ge" },
    BuiltinProc { oid: 170, name: "int24mul", args: &[21, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int24mul" },
    BuiltinProc { oid: 171, name: "int42mul", args: &[23, 21], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int42mul" },
    BuiltinProc { oid: 172, name: "int24div", args: &[21, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int24div" },
    BuiltinProc { oid: 173, name: "int42div", args: &[23, 21], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int42div" },
    BuiltinProc { oid: 176, name: "int2pl", args: &[21, 21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2pl" },
    BuiltinProc { oid: 177, name: "int4pl", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4pl" },
    BuiltinProc { oid: 178, name: "int24pl", args: &[21, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int24pl" },
    BuiltinProc { oid: 179, name: "int42pl", args: &[23, 21], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int42pl" },
    BuiltinProc { oid: 180, name: "int2mi", args: &[21, 21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2mi" },
    BuiltinProc { oid: 181, name: "int4mi", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4mi" },
    BuiltinProc { oid: 182, name: "int24mi", args: &[21, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int24mi" },
    BuiltinProc { oid: 183, name: "int42mi", args: &[23, 21], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int42mi" },
    BuiltinProc { oid: 184, name: "oideq", args: &[26, 26], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "oideq" },
    BuiltinProc { oid: 185, name: "oidne", args: &[26, 26], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "oidne" },
    BuiltinProc { oid: 195, name: "pg_node_tree_in", args: &[2275], result: 194, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "pg_node_tree_in" },
    BuiltinProc { oid: 196, name: "pg_node_tree_out", args: &[194], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "pg_node_tree_out" },
    BuiltinProc { oid: 200, name: "float4in", args: &[2275], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4in" },
    BuiltinProc { oid: 201, name: "float4out", args: &[700], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4out" },
    BuiltinProc { oid: 202, name: "float4mul", args: &[700, 700], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4mul" },
    BuiltinProc { oid: 203, name: "float4div", args: &[700, 700], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4div" },
    BuiltinProc { oid: 204, name: "float4pl", args: &[700, 700], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4pl" },
    BuiltinProc { oid: 205, name: "float4mi", args: &[700, 700], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4mi" },
    BuiltinProc { oid: 206, name: "float4um", args: &[700], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4um" },
    BuiltinProc { oid: 207, name: "float4abs", args: &[700], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4abs" },
    BuiltinProc { oid: 212, name: "int4um", args: &[23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4um" },
    BuiltinProc { oid: 213, name: "int2um", args: &[21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2um" },
    BuiltinProc { oid: 214, name: "float8in", args: &[2275], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8in" },
    BuiltinProc { oid: 215, name: "float8out", args: &[701], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8out" },
    BuiltinProc { oid: 216, name: "float8mul", args: &[701, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8mul" },
    BuiltinProc { oid: 217, name: "float8div", args: &[701, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8div" },
    BuiltinProc { oid: 218, name: "float8pl", args: &[701, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8pl" },
    BuiltinProc { oid: 219, name: "float8mi", args: &[701, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8mi" },
    BuiltinProc { oid: 220, name: "float8um", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8um" },
    BuiltinProc { oid: 221, name: "float8abs", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8abs" },
    BuiltinProc { oid: 232, name: "dpow", args: &[701, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dpow" },
    BuiltinProc { oid: 235, name: "float8", args: &[21], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "i2tod" },
    BuiltinProc { oid: 236, name: "float4", args: &[21], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "i2tof" },
    BuiltinProc { oid: 237, name: "int2", args: &[701], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dtoi2" },
    BuiltinProc { oid: 238, name: "int2", args: &[700], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "ftoi2" },
    BuiltinProc { oid: 240, name: "nameeqtext", args: &[19, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "nameeqtext" },
    BuiltinProc { oid: 241, name: "namelttext", args: &[19, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "namelttext" },
    BuiltinProc { oid: 242, name: "nameletext", args: &[19, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "nameletext" },
    BuiltinProc { oid: 243, name: "namegetext", args: &[19, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "namegetext" },
    BuiltinProc { oid: 244, name: "namegttext", args: &[19, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "namegttext" },
    BuiltinProc { oid: 245, name: "namenetext", args: &[19, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "namenetext" },
    BuiltinProc { oid: 247, name: "texteqname", args: &[25, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "texteqname" },
    BuiltinProc { oid: 248, name: "textltname", args: &[25, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "textltname" },
    BuiltinProc { oid: 249, name: "textlename", args: &[25, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "textlename" },
    BuiltinProc { oid: 250, name: "textgename", args: &[25, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "textgename" },
    BuiltinProc { oid: 251, name: "textgtname", args: &[25, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "textgtname" },
    BuiltinProc { oid: 252, name: "textnename", args: &[25, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "textnename" },
    BuiltinProc { oid: 267, name: "table_am_handler_in", args: &[2275], result: 269, strict: false, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "table_am_handler_in" },
    BuiltinProc { oid: 268, name: "table_am_handler_out", args: &[269], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "table_am_handler_out" },
    BuiltinProc { oid: 279, name: "float48mul", args: &[700, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float48mul" },
    BuiltinProc { oid: 280, name: "float48div", args: &[700, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float48div" },
    BuiltinProc { oid: 281, name: "float48pl", args: &[700, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float48pl" },
    BuiltinProc { oid: 282, name: "float48mi", args: &[700, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float48mi" },
    BuiltinProc { oid: 283, name: "float84mul", args: &[701, 700], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float84mul" },
    BuiltinProc { oid: 284, name: "float84div", args: &[701, 700], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float84div" },
    BuiltinProc { oid: 285, name: "float84pl", args: &[701, 700], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float84pl" },
    BuiltinProc { oid: 286, name: "float84mi", args: &[701, 700], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float84mi" },
    BuiltinProc { oid: 287, name: "float4eq", args: &[700, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float4eq" },
    BuiltinProc { oid: 288, name: "float4ne", args: &[700, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float4ne" },
    BuiltinProc { oid: 289, name: "float4lt", args: &[700, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float4lt" },
    BuiltinProc { oid: 290, name: "float4le", args: &[700, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float4le" },
    BuiltinProc { oid: 291, name: "float4gt", args: &[700, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float4gt" },
    BuiltinProc { oid: 292, name: "float4ge", args: &[700, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float4ge" },
    BuiltinProc { oid: 293, name: "float8eq", args: &[701, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float8eq" },
    BuiltinProc { oid: 294, name: "float8ne", args: &[701, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float8ne" },
    BuiltinProc { oid: 295, name: "float8lt", args: &[701, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float8lt" },
    BuiltinProc { oid: 296, name: "float8le", args: &[701, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float8le" },
    BuiltinProc { oid: 297, name: "float8gt", args: &[701, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float8gt" },
    BuiltinProc { oid: 298, name: "float8ge", args: &[701, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float8ge" },
    BuiltinProc { oid: 299, name: "float48eq", args: &[700, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float48eq" },
    BuiltinProc { oid: 300, name: "float48ne", args: &[700, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float48ne" },
    BuiltinProc { oid: 301, name: "float48lt", args: &[700, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float48lt" },
    BuiltinProc { oid: 302, name: "float48le", args: &[700, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float48le" },
    BuiltinProc { oid: 303, name: "float48gt", args: &[700, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float48gt" },
    BuiltinProc { oid: 304, name: "float48ge", args: &[700, 701], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float48ge" },
    BuiltinProc { oid: 305, name: "float84eq", args: &[701, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float84eq" },
    BuiltinProc { oid: 306, name: "float84ne", args: &[701, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float84ne" },
    BuiltinProc { oid: 307, name: "float84lt", args: &[701, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float84lt" },
    BuiltinProc { oid: 308, name: "float84le", args: &[701, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float84le" },
    BuiltinProc { oid: 309, name: "float84gt", args: &[701, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float84gt" },
    BuiltinProc { oid: 310, name: "float84ge", args: &[701, 700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float84ge" },
    BuiltinProc { oid: 311, name: "float8", args: &[700], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "ftod" },
    BuiltinProc { oid: 312, name: "float4", args: &[701], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dtof" },
    BuiltinProc { oid: 313, name: "int4", args: &[21], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "i2toi4" },
    BuiltinProc { oid: 314, name: "int2", args: &[23], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "i4toi2" },
    BuiltinProc { oid: 316, name: "float8", args: &[23], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "i4tod" },
    BuiltinProc { oid: 317, name: "int4", args: &[701], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dtoi4" },
    BuiltinProc { oid: 318, name: "float4", args: &[23], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "i4tof" },
    BuiltinProc { oid: 319, name: "int4", args: &[700], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "ftoi4" },
    BuiltinProc { oid: 326, name: "index_am_handler_in", args: &[2275], result: 325, strict: false, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "index_am_handler_in" },
    BuiltinProc { oid: 327, name: "index_am_handler_out", args: &[325], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "index_am_handler_out" },
    BuiltinProc { oid: 330, name: "bthandler", args: &[2281], result: 325, strict: true, volatility: 'v', parallel: 's', leakproof: false, cost: 1.0, prosrc: "bthandler" },
    BuiltinProc { oid: 395, name: "array_to_string", args: &[2277, 25], result: 25, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "array_to_text" },
    BuiltinProc { oid: 406, name: "text", args: &[19], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "name_text" },
    BuiltinProc { oid: 407, name: "name", args: &[25], result: 19, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "text_name" },
    BuiltinProc { oid: 460, name: "int8in", args: &[2275], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8in" },
    BuiltinProc { oid: 461, name: "int8out", args: &[20], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8out" },
    BuiltinProc { oid: 462, name: "int8um", args: &[20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8um" },
    BuiltinProc { oid: 463, name: "int8pl", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8pl" },
    BuiltinProc { oid: 464, name: "int8mi", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8mi" },
    BuiltinProc { oid: 465, name: "int8mul", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8mul" },
    BuiltinProc { oid: 466, name: "int8div", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8div" },
    BuiltinProc { oid: 467, name: "int8eq", args: &[20, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int8eq" },
    BuiltinProc { oid: 468, name: "int8ne", args: &[20, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int8ne" },
    BuiltinProc { oid: 469, name: "int8lt", args: &[20, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int8lt" },
    BuiltinProc { oid: 470, name: "int8gt", args: &[20, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int8gt" },
    BuiltinProc { oid: 471, name: "int8le", args: &[20, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int8le" },
    BuiltinProc { oid: 472, name: "int8ge", args: &[20, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int8ge" },
    BuiltinProc { oid: 474, name: "int84eq", args: &[20, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int84eq" },
    BuiltinProc { oid: 475, name: "int84ne", args: &[20, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int84ne" },
    BuiltinProc { oid: 476, name: "int84lt", args: &[20, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int84lt" },
    BuiltinProc { oid: 477, name: "int84gt", args: &[20, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int84gt" },
    BuiltinProc { oid: 478, name: "int84le", args: &[20, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int84le" },
    BuiltinProc { oid: 479, name: "int84ge", args: &[20, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int84ge" },
    BuiltinProc { oid: 480, name: "int4", args: &[20], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int84" },
    BuiltinProc { oid: 481, name: "int8", args: &[23], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int48" },
    BuiltinProc { oid: 482, name: "float8", args: &[20], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "i8tod" },
    BuiltinProc { oid: 483, name: "int8", args: &[701], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dtoi8" },
    BuiltinProc { oid: 652, name: "float4", args: &[20], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "i8tof" },
    BuiltinProc { oid: 653, name: "int8", args: &[700], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "ftoi8" },
    BuiltinProc { oid: 655, name: "namelt", args: &[19, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "namelt" },
    BuiltinProc { oid: 656, name: "namele", args: &[19, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "namele" },
    BuiltinProc { oid: 657, name: "namegt", args: &[19, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "namegt" },
    BuiltinProc { oid: 658, name: "namege", args: &[19, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "namege" },
    BuiltinProc { oid: 659, name: "namene", args: &[19, 19], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "namene" },
    BuiltinProc { oid: 714, name: "int2", args: &[20], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int82" },
    BuiltinProc { oid: 716, name: "oidlt", args: &[26, 26], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "oidlt" },
    BuiltinProc { oid: 717, name: "oidle", args: &[26, 26], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "oidle" },
    BuiltinProc { oid: 740, name: "text_lt", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "text_lt" },
    BuiltinProc { oid: 741, name: "text_le", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "text_le" },
    BuiltinProc { oid: 742, name: "text_gt", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "text_gt" },
    BuiltinProc { oid: 743, name: "text_ge", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "text_ge" },
    BuiltinProc { oid: 750, name: "array_in", args: &[2275, 26, 23], result: 2277, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "array_in" },
    BuiltinProc { oid: 751, name: "array_out", args: &[2277], result: 2275, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "array_out" },
    BuiltinProc { oid: 754, name: "int8", args: &[21], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int28" },
    BuiltinProc { oid: 837, name: "int82pl", args: &[20, 21], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int82pl" },
    BuiltinProc { oid: 838, name: "int82mi", args: &[20, 21], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int82mi" },
    BuiltinProc { oid: 839, name: "int82mul", args: &[20, 21], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int82mul" },
    BuiltinProc { oid: 840, name: "int82div", args: &[20, 21], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int82div" },
    BuiltinProc { oid: 841, name: "int28pl", args: &[21, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int28pl" },
    BuiltinProc { oid: 849, name: "position", args: &[25, 25], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textpos" },
    BuiltinProc { oid: 850, name: "textlike", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textlike" },
    BuiltinProc { oid: 851, name: "textnlike", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textnlike" },
    BuiltinProc { oid: 852, name: "int48eq", args: &[23, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int48eq" },
    BuiltinProc { oid: 853, name: "int48ne", args: &[23, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int48ne" },
    BuiltinProc { oid: 854, name: "int48lt", args: &[23, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int48lt" },
    BuiltinProc { oid: 855, name: "int48gt", args: &[23, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int48gt" },
    BuiltinProc { oid: 856, name: "int48le", args: &[23, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int48le" },
    BuiltinProc { oid: 857, name: "int48ge", args: &[23, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int48ge" },
    BuiltinProc { oid: 861, name: "current_database", args: &[], result: 19, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "current_database" },
    BuiltinProc { oid: 868, name: "strpos", args: &[25, 25], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textpos" },
    BuiltinProc { oid: 870, name: "lower", args: &[25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "lower" },
    BuiltinProc { oid: 871, name: "upper", args: &[25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "upper" },
    BuiltinProc { oid: 872, name: "initcap", args: &[25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "initcap" },
    BuiltinProc { oid: 873, name: "lpad", args: &[25, 23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "lpad" },
    BuiltinProc { oid: 874, name: "rpad", args: &[25, 23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "rpad" },
    BuiltinProc { oid: 875, name: "ltrim", args: &[25, 25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "ltrim" },
    BuiltinProc { oid: 876, name: "rtrim", args: &[25, 25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "rtrim" },
    BuiltinProc { oid: 877, name: "substr", args: &[25, 23, 23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_substr" },
    BuiltinProc { oid: 878, name: "translate", args: &[25, 25, 25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "translate" },
    BuiltinProc { oid: 879, name: "lpad", args: &[25, 23, 25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "lpad" },
    BuiltinProc { oid: 880, name: "rpad", args: &[25, 23, 25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "rpad" },
    BuiltinProc { oid: 881, name: "ltrim", args: &[25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "ltrim1" },
    BuiltinProc { oid: 882, name: "rtrim", args: &[25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "rtrim1" },
    BuiltinProc { oid: 883, name: "substr", args: &[25, 23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_substr_no_len" },
    BuiltinProc { oid: 884, name: "btrim", args: &[25, 25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "btrim" },
    BuiltinProc { oid: 885, name: "btrim", args: &[25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "btrim1" },
    BuiltinProc { oid: 936, name: "substring", args: &[25, 23, 23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_substr" },
    BuiltinProc { oid: 937, name: "substring", args: &[25, 23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_substr_no_len" },
    BuiltinProc { oid: 940, name: "mod", args: &[21, 21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2mod" },
    BuiltinProc { oid: 941, name: "mod", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4mod" },
    BuiltinProc { oid: 942, name: "int28mi", args: &[21, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int28mi" },
    BuiltinProc { oid: 943, name: "int28mul", args: &[21, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int28mul" },
    BuiltinProc { oid: 944, name: "char", args: &[25], result: 18, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_char" },
    BuiltinProc { oid: 945, name: "int8mod", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8mod" },
    BuiltinProc { oid: 946, name: "text", args: &[18], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "char_text" },
    BuiltinProc { oid: 947, name: "mod", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8mod" },
    BuiltinProc { oid: 948, name: "int28div", args: &[21, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int28div" },
    BuiltinProc { oid: 1031, name: "aclitemin", args: &[2275], result: 1033, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "aclitemin" },
    BuiltinProc { oid: 1032, name: "aclitemout", args: &[1033], result: 2275, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "aclitemout" },
    BuiltinProc { oid: 1046, name: "varcharin", args: &[2275, 26, 23], result: 1043, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "varcharin" },
    BuiltinProc { oid: 1047, name: "varcharout", args: &[1043], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "varcharout" },
    BuiltinProc { oid: 1081, name: "format_type", args: &[26, 23], result: 25, strict: false, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "format_type" },
    BuiltinProc { oid: 1084, name: "date_in", args: &[2275], result: 1082, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "date_in" },
    BuiltinProc { oid: 1085, name: "date_out", args: &[1082], result: 2275, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "date_out" },
    BuiltinProc { oid: 1086, name: "date_eq", args: &[1082, 1082], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "date_eq" },
    BuiltinProc { oid: 1087, name: "date_lt", args: &[1082, 1082], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "date_lt" },
    BuiltinProc { oid: 1088, name: "date_le", args: &[1082, 1082], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "date_le" },
    BuiltinProc { oid: 1089, name: "date_gt", args: &[1082, 1082], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "date_gt" },
    BuiltinProc { oid: 1090, name: "date_ge", args: &[1082, 1082], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "date_ge" },
    BuiltinProc { oid: 1091, name: "date_ne", args: &[1082, 1082], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "date_ne" },
    BuiltinProc { oid: 1140, name: "date_mi", args: &[1082, 1082], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "date_mi" },
    BuiltinProc { oid: 1141, name: "date_pli", args: &[1082, 23], result: 1082, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "date_pli" },
    BuiltinProc { oid: 1142, name: "date_mii", args: &[1082, 23], result: 1082, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "date_mii" },
    BuiltinProc { oid: 1150, name: "timestamptz_in", args: &[2275, 26, 23], result: 1184, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamptz_in" },
    BuiltinProc { oid: 1151, name: "timestamptz_out", args: &[1184], result: 2275, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "timestamptz_out" },
    BuiltinProc { oid: 1160, name: "interval_in", args: &[2275, 26, 23], result: 1186, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_in" },
    BuiltinProc { oid: 1161, name: "interval_out", args: &[1186], result: 2275, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_out" },
    BuiltinProc { oid: 1162, name: "interval_eq", args: &[1186, 1186], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "interval_eq" },
    BuiltinProc { oid: 1163, name: "interval_ne", args: &[1186, 1186], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "interval_ne" },
    BuiltinProc { oid: 1164, name: "interval_lt", args: &[1186, 1186], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "interval_lt" },
    BuiltinProc { oid: 1165, name: "interval_le", args: &[1186, 1186], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "interval_le" },
    BuiltinProc { oid: 1166, name: "interval_ge", args: &[1186, 1186], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "interval_ge" },
    BuiltinProc { oid: 1167, name: "interval_gt", args: &[1186, 1186], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "interval_gt" },
    BuiltinProc { oid: 1168, name: "interval_um", args: &[1186], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_um" },
    BuiltinProc { oid: 1169, name: "interval_pl", args: &[1186, 1186], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_pl" },
    BuiltinProc { oid: 1170, name: "interval_mi", args: &[1186, 1186], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_mi" },
    BuiltinProc { oid: 1230, name: "int8abs", args: &[20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8abs" },
    BuiltinProc { oid: 1238, name: "texticregexeq", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "texticregexeq" },
    BuiltinProc { oid: 1239, name: "texticregexne", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "texticregexne" },
    BuiltinProc { oid: 1240, name: "textregexne", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textregexne" },
    BuiltinProc { oid: 1242, name: "boolin", args: &[2275], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "boolin" },
    BuiltinProc { oid: 1243, name: "boolout", args: &[16], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "boolout" },
    BuiltinProc { oid: 1244, name: "textregexeq", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textregexeq" },
    BuiltinProc { oid: 1245, name: "charin", args: &[2275], result: 18, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "charin" },
    BuiltinProc { oid: 1246, name: "charlt", args: &[18, 18], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "charlt" },
    BuiltinProc { oid: 1251, name: "int4abs", args: &[23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4abs" },
    BuiltinProc { oid: 1253, name: "int2abs", args: &[21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2abs" },
    BuiltinProc { oid: 1257, name: "textlen", args: &[25], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textlen" },
    BuiltinProc { oid: 1258, name: "textcat", args: &[25, 25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textcat" },
    BuiltinProc { oid: 1265, name: "tidne", args: &[27, 27], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "tidne" },
    BuiltinProc { oid: 1274, name: "int84pl", args: &[20, 23], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int84pl" },
    BuiltinProc { oid: 1275, name: "int84mi", args: &[20, 23], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int84mi" },
    BuiltinProc { oid: 1276, name: "int84mul", args: &[20, 23], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int84mul" },
    BuiltinProc { oid: 1277, name: "int84div", args: &[20, 23], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int84div" },
    BuiltinProc { oid: 1278, name: "int48pl", args: &[23, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int48pl" },
    BuiltinProc { oid: 1279, name: "int48mi", args: &[23, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int48mi" },
    BuiltinProc { oid: 1280, name: "int48mul", args: &[23, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int48mul" },
    BuiltinProc { oid: 1281, name: "int48div", args: &[23, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int48div" },
    BuiltinProc { oid: 1282, name: "quote_ident", args: &[25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "quote_ident" },
    BuiltinProc { oid: 1283, name: "quote_literal", args: &[25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "quote_literal" },
    BuiltinProc { oid: 1287, name: "oid", args: &[20], result: 26, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "i8tooid" },
    BuiltinProc { oid: 1288, name: "int8", args: &[26], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "oidtoi8" },
    BuiltinProc { oid: 1292, name: "tideq", args: &[27, 27], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "tideq" },
    BuiltinProc { oid: 1317, name: "length", args: &[25], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textlen" },
    BuiltinProc { oid: 1319, name: "xideqint4", args: &[28, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "xideq" },
    BuiltinProc { oid: 1326, name: "interval_div", args: &[1186, 701], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_div" },
    BuiltinProc { oid: 1339, name: "log10", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dlog10" },
    BuiltinProc { oid: 1340, name: "log", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dlog10" },
    BuiltinProc { oid: 1341, name: "ln", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dlog1" },
    BuiltinProc { oid: 1344, name: "sqrt", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dsqrt" },
    BuiltinProc { oid: 1345, name: "cbrt", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dcbrt" },
    BuiltinProc { oid: 1347, name: "exp", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dexp" },
    BuiltinProc { oid: 1367, name: "character_length", args: &[25], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textlen" },
    BuiltinProc { oid: 1368, name: "power", args: &[701, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dpow" },
    BuiltinProc { oid: 1374, name: "octet_length", args: &[25], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textoctetlen" },
    BuiltinProc { oid: 1381, name: "char_length", args: &[25], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "textlen" },
    BuiltinProc { oid: 1394, name: "abs", args: &[700], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4abs" },
    BuiltinProc { oid: 1395, name: "abs", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8abs" },
    BuiltinProc { oid: 1396, name: "abs", args: &[20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8abs" },
    BuiltinProc { oid: 1397, name: "abs", args: &[23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4abs" },
    BuiltinProc { oid: 1398, name: "abs", args: &[21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2abs" },
    BuiltinProc { oid: 1400, name: "name", args: &[1043], result: 19, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "text_name" },
    BuiltinProc { oid: 1401, name: "varchar", args: &[19], result: 1043, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "name_text" },
    BuiltinProc { oid: 1402, name: "current_schema", args: &[], result: 19, strict: true, volatility: 's', parallel: 'u', leakproof: false, cost: 1.0, prosrc: "current_schema" },
    BuiltinProc { oid: 1481, name: "log10", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_log10" },
    BuiltinProc { oid: 1597, name: "pg_encoding_to_char", args: &[23], result: 19, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "PG_encoding_to_char" },
    BuiltinProc { oid: 1600, name: "asin", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dasin" },
    BuiltinProc { oid: 1601, name: "acos", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dacos" },
    BuiltinProc { oid: 1602, name: "atan", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "datan" },
    BuiltinProc { oid: 1603, name: "atan2", args: &[701, 701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "datan2" },
    BuiltinProc { oid: 1604, name: "sin", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dsin" },
    BuiltinProc { oid: 1605, name: "cos", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dcos" },
    BuiltinProc { oid: 1606, name: "tan", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dtan" },
    BuiltinProc { oid: 1608, name: "degrees", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "degrees" },
    BuiltinProc { oid: 1609, name: "radians", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "radians" },
    BuiltinProc { oid: 1618, name: "interval_mul", args: &[1186, 701], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "interval_mul" },
    BuiltinProc { oid: 1620, name: "ascii", args: &[25], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "ascii" },
    BuiltinProc { oid: 1621, name: "chr", args: &[23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "chr" },
    BuiltinProc { oid: 1622, name: "repeat", args: &[25, 23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "repeat" },
    BuiltinProc { oid: 1624, name: "mul_d_interval", args: &[701, 1186], result: 1186, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "mul_d_interval" },
    BuiltinProc { oid: 1633, name: "texticlike", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "texticlike" },
    BuiltinProc { oid: 1634, name: "texticnlike", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "texticnlike" },
    BuiltinProc { oid: 1638, name: "oidgt", args: &[26, 26], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "oidgt" },
    BuiltinProc { oid: 1639, name: "oidge", args: &[26, 26], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "oidge" },
    BuiltinProc { oid: 1642, name: "pg_get_userbyid", args: &[26], result: 19, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "pg_get_userbyid" },
    BuiltinProc { oid: 1691, name: "boolle", args: &[16, 16], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "boolle" },
    BuiltinProc { oid: 1692, name: "boolge", args: &[16, 16], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "boolge" },
    BuiltinProc { oid: 1701, name: "numeric_in", args: &[2275, 26, 23], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_in" },
    BuiltinProc { oid: 1702, name: "numeric_out", args: &[1700], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_out" },
    BuiltinProc { oid: 1704, name: "numeric_abs", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_abs" },
    BuiltinProc { oid: 1705, name: "abs", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_abs" },
    BuiltinProc { oid: 1706, name: "sign", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_sign" },
    BuiltinProc { oid: 1707, name: "round", args: &[1700, 23], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_round" },
    BuiltinProc { oid: 1708, name: "round", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_round" },
    BuiltinProc { oid: 1709, name: "trunc", args: &[1700, 23], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_trunc" },
    BuiltinProc { oid: 1710, name: "trunc", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_trunc" },
    BuiltinProc { oid: 1711, name: "ceil", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_ceil" },
    BuiltinProc { oid: 1712, name: "floor", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_floor" },
    BuiltinProc { oid: 1716, name: "pg_get_expr", args: &[194, 26], result: 25, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "pg_get_expr" },
    BuiltinProc { oid: 1718, name: "numeric_eq", args: &[1700, 1700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_eq" },
    BuiltinProc { oid: 1719, name: "numeric_ne", args: &[1700, 1700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_ne" },
    BuiltinProc { oid: 1720, name: "numeric_gt", args: &[1700, 1700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_gt" },
    BuiltinProc { oid: 1721, name: "numeric_ge", args: &[1700, 1700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_ge" },
    BuiltinProc { oid: 1722, name: "numeric_lt", args: &[1700, 1700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_lt" },
    BuiltinProc { oid: 1723, name: "numeric_le", args: &[1700, 1700], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_le" },
    BuiltinProc { oid: 1724, name: "numeric_add", args: &[1700, 1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_add" },
    BuiltinProc { oid: 1725, name: "numeric_sub", args: &[1700, 1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_sub" },
    BuiltinProc { oid: 1726, name: "numeric_mul", args: &[1700, 1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_mul" },
    BuiltinProc { oid: 1727, name: "numeric_div", args: &[1700, 1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_div" },
    BuiltinProc { oid: 1729, name: "numeric_mod", args: &[1700, 1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_mod" },
    BuiltinProc { oid: 1730, name: "sqrt", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_sqrt" },
    BuiltinProc { oid: 1734, name: "ln", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_ln" },
    BuiltinProc { oid: 1736, name: "log", args: &[1700, 1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_log" },
    BuiltinProc { oid: 1739, name: "numeric_power", args: &[1700, 1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_power" },
    BuiltinProc { oid: 1740, name: "numeric", args: &[23], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int4_numeric" },
    BuiltinProc { oid: 1741, name: "log", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_log10" },
    BuiltinProc { oid: 1742, name: "numeric", args: &[700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float4_numeric" },
    BuiltinProc { oid: 1743, name: "numeric", args: &[701], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "float8_numeric" },
    BuiltinProc { oid: 1744, name: "int4", args: &[1700], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_int4" },
    BuiltinProc { oid: 1745, name: "float4", args: &[1700], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_float4" },
    BuiltinProc { oid: 1746, name: "float8", args: &[1700], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_float8" },
    BuiltinProc { oid: 1771, name: "numeric_uminus", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_uminus" },
    BuiltinProc { oid: 1779, name: "int8", args: &[1700], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_int8" },
    BuiltinProc { oid: 1781, name: "numeric", args: &[20], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int8_numeric" },
    BuiltinProc { oid: 1782, name: "numeric", args: &[21], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int2_numeric" },
    BuiltinProc { oid: 1783, name: "int2", args: &[1700], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_int2" },
    BuiltinProc { oid: 1798, name: "oidin", args: &[2275], result: 26, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "oidin" },
    BuiltinProc { oid: 1799, name: "oidout", args: &[26], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "oidout" },
    BuiltinProc { oid: 1811, name: "bit_length", args: &[25], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "bit_length" },
    BuiltinProc { oid: 1850, name: "int28eq", args: &[21, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int28eq" },
    BuiltinProc { oid: 1851, name: "int28ne", args: &[21, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int28ne" },
    BuiltinProc { oid: 1852, name: "int28lt", args: &[21, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int28lt" },
    BuiltinProc { oid: 1853, name: "int28gt", args: &[21, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int28gt" },
    BuiltinProc { oid: 1854, name: "int28le", args: &[21, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int28le" },
    BuiltinProc { oid: 1855, name: "int28ge", args: &[21, 20], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int28ge" },
    BuiltinProc { oid: 1856, name: "int82eq", args: &[20, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int82eq" },
    BuiltinProc { oid: 1857, name: "int82ne", args: &[20, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int82ne" },
    BuiltinProc { oid: 1858, name: "int82lt", args: &[20, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int82lt" },
    BuiltinProc { oid: 1859, name: "int82gt", args: &[20, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int82gt" },
    BuiltinProc { oid: 1860, name: "int82le", args: &[20, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int82le" },
    BuiltinProc { oid: 1861, name: "int82ge", args: &[20, 21], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int82ge" },
    BuiltinProc { oid: 1892, name: "int2and", args: &[21, 21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2and" },
    BuiltinProc { oid: 1893, name: "int2or", args: &[21, 21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2or" },
    BuiltinProc { oid: 1894, name: "int2xor", args: &[21, 21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2xor" },
    BuiltinProc { oid: 1895, name: "int2not", args: &[21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2not" },
    BuiltinProc { oid: 1896, name: "int2shl", args: &[21, 23], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2shl" },
    BuiltinProc { oid: 1897, name: "int2shr", args: &[21, 23], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2shr" },
    BuiltinProc { oid: 1898, name: "int4and", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4and" },
    BuiltinProc { oid: 1899, name: "int4or", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4or" },
    BuiltinProc { oid: 1900, name: "int4xor", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4xor" },
    BuiltinProc { oid: 1901, name: "int4not", args: &[23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4not" },
    BuiltinProc { oid: 1902, name: "int4shl", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4shl" },
    BuiltinProc { oid: 1903, name: "int4shr", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4shr" },
    BuiltinProc { oid: 1904, name: "int8and", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8and" },
    BuiltinProc { oid: 1905, name: "int8or", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8or" },
    BuiltinProc { oid: 1906, name: "int8xor", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8xor" },
    BuiltinProc { oid: 1907, name: "int8not", args: &[20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8not" },
    BuiltinProc { oid: 1908, name: "int8shl", args: &[20, 23], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8shl" },
    BuiltinProc { oid: 1909, name: "int8shr", args: &[20, 23], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8shr" },
    BuiltinProc { oid: 1910, name: "int8up", args: &[20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8up" },
    BuiltinProc { oid: 1911, name: "int2up", args: &[21], result: 21, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int2up" },
    BuiltinProc { oid: 1912, name: "int4up", args: &[23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4up" },
    BuiltinProc { oid: 1913, name: "float4up", args: &[700], result: 700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float4up" },
    BuiltinProc { oid: 1914, name: "float8up", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "float8up" },
    BuiltinProc { oid: 1915, name: "numeric_uplus", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_uplus" },
    BuiltinProc { oid: 1973, name: "div", args: &[1700, 1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_div_trunc" },
    BuiltinProc { oid: 2003, name: "textanycat", args: &[25, 2776], result: 25, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "select $1 operator(pg_catalog.||) $2::pg_catalog.text" },
    BuiltinProc { oid: 2004, name: "anytextcat", args: &[2776, 25], result: 25, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "select $1::pg_catalog.text operator(pg_catalog.||) $2" },
    BuiltinProc { oid: 2026, name: "pg_backend_pid", args: &[], result: 23, strict: true, volatility: 's', parallel: 'r', leakproof: false, cost: 1.0, prosrc: "pg_backend_pid" },
    BuiltinProc { oid: 2077, name: "current_setting", args: &[25], result: 25, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "show_config_by_name" },
    BuiltinProc { oid: 2078, name: "set_config", args: &[25, 25, 16], result: 25, strict: false, volatility: 'v', parallel: 'u', leakproof: false, cost: 1.0, prosrc: "set_config_by_name" },
    BuiltinProc { oid: 2079, name: "pg_table_is_visible", args: &[26], result: 16, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 10.0, prosrc: "pg_table_is_visible" },
    BuiltinProc { oid: 2087, name: "replace", args: &[25, 25, 25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "replace_text" },
    BuiltinProc { oid: 2088, name: "split_part", args: &[25, 25, 23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "split_part" },
    BuiltinProc { oid: 2167, name: "ceiling", args: &[1700], result: 1700, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "numeric_ceil" },
    BuiltinProc { oid: 2176, name: "array_length", args: &[2277, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "array_length" },
    BuiltinProc { oid: 2290, name: "record_in", args: &[2275, 26, 23], result: 2249, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "record_in" },
    BuiltinProc { oid: 2291, name: "record_out", args: &[2249], result: 2275, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "record_out" },
    BuiltinProc { oid: 2292, name: "cstring_in", args: &[2275], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "cstring_in" },
    BuiltinProc { oid: 2293, name: "cstring_out", args: &[2275], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "cstring_out" },
    BuiltinProc { oid: 2296, name: "anyarray_in", args: &[2275], result: 2277, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anyarray_in" },
    BuiltinProc { oid: 2297, name: "anyarray_out", args: &[2277], result: 2275, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anyarray_out" },
    BuiltinProc { oid: 2298, name: "void_in", args: &[2275], result: 2278, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "void_in" },
    BuiltinProc { oid: 2299, name: "void_out", args: &[2278], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "void_out" },
    BuiltinProc { oid: 2304, name: "internal_in", args: &[2275], result: 2281, strict: false, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "internal_in" },
    BuiltinProc { oid: 2305, name: "internal_out", args: &[2281], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "internal_out" },
    BuiltinProc { oid: 2308, name: "ceil", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dceil" },
    BuiltinProc { oid: 2309, name: "floor", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dfloor" },
    BuiltinProc { oid: 2310, name: "sign", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dsign" },
    BuiltinProc { oid: 2311, name: "md5", args: &[25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "md5_text" },
    BuiltinProc { oid: 2320, name: "ceiling", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dceil" },
    BuiltinProc { oid: 2339, name: "round", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dround" },
    BuiltinProc { oid: 2340, name: "trunc", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dtrunc" },
    BuiltinProc { oid: 2462, name: "sinh", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dsinh" },
    BuiltinProc { oid: 2463, name: "cosh", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dcosh" },
    BuiltinProc { oid: 2464, name: "tanh", args: &[701], result: 701, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "dtanh" },
    BuiltinProc { oid: 2550, name: "integer_pl_date", args: &[23, 1082], result: 1082, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "" },
    BuiltinProc { oid: 2557, name: "bool", args: &[23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "int4_bool" },
    BuiltinProc { oid: 2558, name: "int4", args: &[16], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "bool_int4" },
    BuiltinProc { oid: 2626, name: "pg_sleep", args: &[701], result: 2278, strict: true, volatility: 'v', parallel: 's', leakproof: false, cost: 1.0, prosrc: "pg_sleep" },
    BuiltinProc { oid: 2777, name: "anynonarray_in", args: &[2275], result: 2776, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anynonarray_in" },
    BuiltinProc { oid: 2778, name: "anynonarray_out", args: &[2776], result: 2275, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "anynonarray_out" },
    BuiltinProc { oid: 2790, name: "tidgt", args: &[27, 27], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "tidgt" },
    BuiltinProc { oid: 2791, name: "tidlt", args: &[27, 27], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "tidlt" },
    BuiltinProc { oid: 2792, name: "tidge", args: &[27, 27], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "tidge" },
    BuiltinProc { oid: 2793, name: "tidle", args: &[27, 27], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "tidle" },
    BuiltinProc { oid: 2971, name: "text", args: &[16], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "booltext" },
    BuiltinProc { oid: 3058, name: "concat", args: &[25], result: 25, strict: false, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_concat" },
    BuiltinProc { oid: 3059, name: "concat_ws", args: &[25, 25], result: 25, strict: false, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_concat_ws" },
    BuiltinProc { oid: 3060, name: "left", args: &[25, 23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_left" },
    BuiltinProc { oid: 3061, name: "right", args: &[25, 23], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_right" },
    BuiltinProc { oid: 3062, name: "reverse", args: &[25], result: 25, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_reverse" },
    BuiltinProc { oid: 3294, name: "current_setting", args: &[25, 16], result: 25, strict: true, volatility: 's', parallel: 's', leakproof: false, cost: 1.0, prosrc: "show_config_by_name_missing_ok" },
    BuiltinProc { oid: 3308, name: "xidneq", args: &[28, 28], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "xidneq" },
    BuiltinProc { oid: 3309, name: "xidneqint4", args: &[28, 23], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: true, cost: 1.0, prosrc: "xidneq" },
    BuiltinProc { oid: 3348, name: "txid_current_if_assigned", args: &[], result: 20, strict: true, volatility: 's', parallel: 'u', leakproof: false, cost: 1.0, prosrc: "pg_current_xact_id_if_assigned" },
    BuiltinProc { oid: 3378, name: "pg_isolation_test_session_is_blocked", args: &[23, 1007], result: 16, strict: true, volatility: 'v', parallel: 's', leakproof: false, cost: 1.0, prosrc: "pg_isolation_test_session_is_blocked" },
    BuiltinProc { oid: 3696, name: "starts_with", args: &[25, 25], result: 16, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "text_starts_with" },
    BuiltinProc { oid: 5044, name: "gcd", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4gcd" },
    BuiltinProc { oid: 5045, name: "gcd", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8gcd" },
    BuiltinProc { oid: 5046, name: "lcm", args: &[23, 23], result: 23, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int4lcm" },
    BuiltinProc { oid: 5047, name: "lcm", args: &[20, 20], result: 20, strict: true, volatility: 'i', parallel: 's', leakproof: false, cost: 1.0, prosrc: "int8lcm" },
];

// ----- catalog-aware function bodies ---------------------------------------

/// `pg_get_userbyid(oid) -> name`: the role name, or `unknown (OID=n)`.
fn pg_get_userbyid(
    args: &[Datum],
    catalog: &dyn CatalogReader,
    _session: &SessionInfo,
) -> Result<Datum> {
    let Some(Datum::Oid(role)) = args.first() else {
        return Err(crate::error::Error::internal(
            "pg_get_userbyid expects an oid argument",
        ));
    };
    Ok(Datum::Text(match catalog.role_name(*role)? {
        Some(name) => name,
        None => format!("unknown (OID={role})"),
    }))
}

/// `pg_backend_pid() -> int4`.
#[allow(clippy::unnecessary_wraps)]
fn pg_backend_pid(_args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
    Ok(Datum::Int4(runtime.backend_pid()))
}

fn text_arg<'a>(args: &'a [Datum], i: usize, func: &str) -> Result<&'a str> {
    match args.get(i) {
        Some(Datum::Text(s)) => Ok(s),
        _ => Err(crate::error::Error::internal(format!(
            "{func} expects a text argument"
        ))),
    }
}

fn unrecognized_parameter(name: &str) -> crate::error::Error {
    crate::error::Error::new(
        crate::error::sqlstate::UNDEFINED_OBJECT,
        format!("unrecognized configuration parameter \"{name}\""),
    )
}

/// `current_setting(text) -> text`.
fn current_setting(args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
    let name = text_arg(args, 0, "current_setting")?;
    match runtime.get_setting(name)? {
        Some(v) => Ok(Datum::Text(v)),
        None => Err(unrecognized_parameter(name)),
    }
}

/// `current_setting(text, bool) -> text`: NULL for an unknown parameter when
/// `missing_ok` is true.
fn current_setting_missing_ok(args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
    let name = text_arg(args, 0, "current_setting")?;
    match (runtime.get_setting(name)?, args.get(1)) {
        (Some(v), _) => Ok(Datum::Text(v)),
        (None, Some(Datum::Bool(true))) => Ok(Datum::Null),
        (None, _) => Err(unrecognized_parameter(name)),
    }
}

/// `set_config(text, text, bool) -> text` (not strict: a NULL value resets
/// the parameter, a NULL name is an error, a NULL `is_local` means false).
fn set_config(args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
    let Some(Datum::Text(name)) = args.first() else {
        return Err(crate::error::Error::new(
            crate::error::sqlstate::INVALID_PARAMETER_VALUE,
            "SET requires parameter name",
        ));
    };
    let value = match args.get(1) {
        Some(Datum::Text(v)) => Some(v.as_str()),
        _ => None,
    };
    let local = matches!(args.get(2), Some(Datum::Bool(true)));
    Ok(Datum::Text(runtime.set_setting(name, value, local)?))
}

/// `txid_current_if_assigned() -> int8`: NULL while no XID is assigned.
#[allow(clippy::unnecessary_wraps)]
fn txid_current_if_assigned(_args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
    Ok(runtime
        .current_xid()
        .and_then(|x| i64::try_from(x).ok())
        .map_or(Datum::Null, Datum::Int8))
}

/// Interval between interrupt checks of `pg_sleep`.
const SLEEP_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// `pg_sleep(float8) -> void`: returns at once for a non-positive or NaN
/// argument; otherwise sleeps in slices of 10 ms, checking for cancellation
/// (statement timeout, `CancelRequest`) before each slice.
fn pg_sleep(args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
    let Some(Datum::Float8(secs)) = args.first() else {
        return Err(crate::error::Error::internal(
            "pg_sleep expects a float8 argument",
        ));
    };
    // `!(secs > 0.0)` also covers NaN.
    if secs.is_nan() || *secs <= 0.0 {
        return Ok(Datum::Void);
    }
    let total = std::time::Duration::try_from_secs_f64(*secs).unwrap_or(std::time::Duration::MAX);
    let start = std::time::Instant::now();
    loop {
        runtime.check_interrupts()?;
        let elapsed = start.elapsed();
        if elapsed >= total {
            return Ok(Datum::Void);
        }
        std::thread::sleep(total.saturating_sub(elapsed).min(SLEEP_CHECK_INTERVAL));
    }
}

/// `pg_isolation_test_session_is_blocked(int4, int4[]) -> bool`. NULL
/// elements of the array are ignored.
fn pg_isolation_test_session_is_blocked(
    args: &[Datum],
    runtime: &dyn RuntimeInfo,
) -> Result<Datum> {
    let (Some(Datum::Int4(pid)), Some(Datum::Int4Array(among))) = (args.first(), args.get(1))
    else {
        return Err(crate::error::Error::internal(
            "pg_isolation_test_session_is_blocked expects (int4, int4[])",
        ));
    };
    let among: Vec<i32> = among.iter().flatten().copied().collect();
    Ok(Datum::Bool(runtime.is_blocked_by(*pid, &among)))
}

/// `pg_table_is_visible(oid) -> bool`: NULL when the relation does not
/// exist; otherwise whether an unqualified reference to its name finds this
/// relation (`pg_catalog` first, then the `search_path`).
fn pg_table_is_visible(
    args: &[Datum],
    catalog: &dyn CatalogReader,
    _session: &SessionInfo,
) -> Result<Datum> {
    let Some(Datum::Oid(rel)) = args.first() else {
        return Err(crate::error::Error::internal(
            "pg_table_is_visible expects an oid argument",
        ));
    };
    let Some(def) = catalog.table_by_oid(*rel)? else {
        return Ok(Datum::Null);
    };
    let found = catalog.table(None, &def.name)?;
    Ok(Datum::Bool(found.is_some_and(|t| t.oid == def.oid)))
}

/// `format_type(oid, int4) -> text`. Not strict: a NULL type gives NULL, a
/// NULL typmod means "no modifier".
fn format_type(args: &[Datum]) -> Result<Datum> {
    let [type_arg, typmod_arg] = args else {
        return Err(crate::error::Error::internal(
            "format_type expects two arguments",
        ));
    };
    let Datum::Oid(type_oid) = type_arg else {
        return Ok(Datum::Null);
    };
    let typmod = typmod_arg.as_i64().and_then(|v| i32::try_from(v).ok());
    Ok(Datum::Text(format_type_name(*type_oid, typmod)))
}

/// The name of a type as `format_type` prints it (`m2.md` §6.10): `-` for 0,
/// `???` for an unknown OID, the SQL spelling for the types PostgreSQL
/// renames (`integer`, `character varying`, ...), `elem[]` for array types,
/// and the type modifier in parentheses when `typmod >= 0`.
pub fn format_type_name(type_oid: Oid, typmod: Option<i32>) -> String {
    if type_oid == 0 {
        return "-".to_owned();
    }
    let Some(t) = type_by_oid(type_oid) else {
        return "???".to_owned();
    };
    if t.category == 'A' && t.elem != 0 && t.name.starts_with('_') {
        return format!("{}[]", format_type_name(t.elem, typmod));
    }
    let name = match type_oid {
        oid::BOOL => "boolean".to_owned(),
        oid::INT8 => "bigint".to_owned(),
        oid::INT2 => "smallint".to_owned(),
        oid::INT4 => "integer".to_owned(),
        oid::FLOAT4 => "real".to_owned(),
        oid::FLOAT8 => "double precision".to_owned(),
        oid::VARCHAR => "character varying".to_owned(),
        oid::TIMESTAMPTZ => "timestamp with time zone".to_owned(),
        _ => quote_identifier(t.name),
    };
    let Some(m) = typmod.filter(|m| *m >= 0) else {
        return name;
    };
    match type_oid {
        oid::VARCHAR => {
            if m > crate::types::VARHDRSZ {
                format!("{name}({})", m - crate::types::VARHDRSZ)
            } else {
                name
            }
        }
        NUMERIC => {
            let tmp = m - crate::types::VARHDRSZ;
            if tmp >= 0 {
                format!("{name}({},{})", (tmp >> 16) & 0xffff, tmp & 0xffff)
            } else {
                name
            }
        }
        // These are special-cased by PostgreSQL and never print a modifier.
        oid::BOOL | oid::INT2 | oid::INT4 | oid::INT8 | oid::FLOAT4 | oid::FLOAT8 => name,
        _ => format!("{name}({m})"),
    }
}

/// `quote_identifier` for type names: double quotes unless the name is made
/// of lower-case letters, digits and underscores (and does not start with a
/// digit) and is not a reserved word that can be a type name (`char`).
fn quote_identifier(name: &str) -> String {
    let simple = name
        .bytes()
        .enumerate()
        .all(|(i, b)| b.is_ascii_lowercase() || b == b'_' || (i > 0 && b.is_ascii_digit()));
    if simple && name != "char" {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
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
        assert_eq!(blocked(4242, vec![None, Some(7)]), Datum::Bool(true));
        assert_eq!(blocked(4242, vec![Some(8)]), Datum::Bool(false));
        assert_eq!(blocked(4242, vec![None]), Datum::Bool(false));
        assert_eq!(blocked(4242, vec![]), Datum::Bool(false));
        assert_eq!(blocked(1, vec![Some(7)]), Datum::Bool(false));
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
        assert!(PROCS.windows(2).all(|w| w[0].oid < w[1].oid));
        assert!(OPERATOR_META.windows(2).all(|w| w[0].oid < w[1].oid));
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
                    assert!(matches!(c.method, CastMethod::Function(_)));
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
        for t in [
            oid::ACLITEM,
            oid::TIMESTAMPTZ,
            oid::ANYARRAY,
            oid::TEXT_ARRAY,
            oid::OID_ARRAY,
        ] {
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
            ],
        };
        let s = session();
        let call = |o: Oid| pg_table_is_visible(&[Datum::Oid(o)], &cat, &s).unwrap();
        assert_eq!(call(1259), Datum::Bool(true));
        assert_eq!(call(16384), Datum::Bool(true));
        assert_eq!(call(16385), Datum::Bool(false));
        assert_eq!(call(99_999), Datum::Null);
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
