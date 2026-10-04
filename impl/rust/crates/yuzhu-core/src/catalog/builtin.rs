//! Static tables of built-in types, casts, operators and functions.
//! OIDs are identical to PostgreSQL's. In M2 these tables become the initial
//! contents of `pg_type`, `pg_cast`, `pg_operator` and `pg_proc`.
//!
//! `CASTS`, `OPERATORS` and `FUNCTIONS` use the PostgreSQL 17 OIDs.

use super::FnKind;
use crate::types::ops::{self, BuiltinFn};
use crate::types::{Oid, oid};

/// A row of `pg_type`.
#[derive(Debug)]
pub struct BuiltinType {
    pub oid: Oid,
    /// `typname`.
    pub name: &'static str,
    /// `typlen` (`-1` varlena, `-2` C string).
    pub typlen: i16,
    pub typbyval: bool,
    /// `typtype`: `b` base, `p` pseudo.
    pub typtype: char,
    /// `typcategory` (`B` boolean, `N` numeric, `S` string, `X` unknown, ...).
    pub category: char,
    /// `typispreferred`.
    pub preferred: bool,
    /// `typarray` (0 if none).
    pub array_oid: Oid,
    /// Names of the I/O functions (`typinput` / `typoutput`). The actual
    /// conversion is `types::io::{input_text, output_text}`.
    pub input: &'static str,
    pub output: &'static str,
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

/// A row of `pg_proc`.
#[derive(Debug)]
pub struct BuiltinFunction {
    pub oid: Oid,
    pub name: &'static str,
    pub args: &'static [Oid],
    pub result: Oid,
    pub strict: bool,
    pub kind: FnKind,
}

#[allow(clippy::too_many_arguments)]
const fn ty(
    oid: Oid,
    name: &'static str,
    typlen: i16,
    typbyval: bool,
    category: char,
    preferred: bool,
    array_oid: Oid,
    io: (&'static str, &'static str),
) -> BuiltinType {
    BuiltinType {
        oid,
        name,
        typlen,
        typbyval,
        typtype: 'b',
        category,
        preferred,
        array_oid,
        input: io.0,
        output: io.1,
    }
}

/// Built-in types (M1 set plus `name` and `oid` for catalog stubs).
pub static TYPES: &[BuiltinType] = &[
    ty(
        oid::BOOL,
        "bool",
        1,
        true,
        'B',
        true,
        1000,
        ("boolin", "boolout"),
    ),
    ty(
        oid::NAME,
        "name",
        64,
        false,
        'S',
        false,
        1003,
        ("namein", "nameout"),
    ),
    ty(
        oid::INT8,
        "int8",
        8,
        true,
        'N',
        false,
        1016,
        ("int8in", "int8out"),
    ),
    ty(
        oid::INT2,
        "int2",
        2,
        true,
        'N',
        false,
        1005,
        ("int2in", "int2out"),
    ),
    ty(
        oid::INT4,
        "int4",
        4,
        true,
        'N',
        false,
        1007,
        ("int4in", "int4out"),
    ),
    ty(
        oid::TEXT,
        "text",
        -1,
        false,
        'S',
        true,
        1009,
        ("textin", "textout"),
    ),
    ty(
        oid::OID,
        "oid",
        4,
        true,
        'N',
        true,
        1028,
        ("oidin", "oidout"),
    ),
    ty(
        oid::FLOAT4,
        "float4",
        4,
        true,
        'N',
        false,
        1021,
        ("float4in", "float4out"),
    ),
    ty(
        oid::FLOAT8,
        "float8",
        8,
        true,
        'N',
        true,
        1022,
        ("float8in", "float8out"),
    ),
    BuiltinType {
        oid: oid::UNKNOWN,
        name: "unknown",
        typlen: -2,
        typbyval: false,
        typtype: 'p',
        category: 'X',
        preferred: false,
        array_oid: 0,
        input: "unknownin",
        output: "unknownout",
    },
    ty(
        oid::VARCHAR,
        "varchar",
        -1,
        false,
        'S',
        false,
        1015,
        ("varcharin", "varcharout"),
    ),
    // Types yuzhu does not implement yet. They are listed (with their
    // operators below) so that operator resolution sees the same candidate
    // categories as PostgreSQL: e.g. `'1' + '2'` is ambiguous (42725)
    // because `+` also exists for date and interval. The analyzer rejects
    // values of these types with 0A000.
    ty(
        NUMERIC,
        "numeric",
        -1,
        false,
        'N',
        false,
        1231,
        ("numeric_in", "numeric_out"),
    ),
    ty(
        DATE,
        "date",
        4,
        true,
        'D',
        false,
        1182,
        ("date_in", "date_out"),
    ),
    ty(
        INTERVAL,
        "interval",
        16,
        false,
        'T',
        true,
        1187,
        ("interval_in", "interval_out"),
    ),
    BuiltinType {
        oid: ANYNONARRAY,
        name: "anynonarray",
        typlen: 4,
        typbyval: true,
        typtype: 'p',
        category: 'P',
        preferred: false,
        array_oid: 0,
        input: "anynonarray_in",
        output: "anynonarray_out",
    },
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

/// Whether values of the type can exist in yuzhu M1 (have a `Datum`
/// representation and I/O functions).
pub fn is_supported_type(t: Oid) -> bool {
    matches!(
        t,
        oid::BOOL
            | oid::INT2
            | oid::INT4
            | oid::INT8
            | oid::FLOAT4
            | oid::FLOAT8
            | oid::TEXT
            | oid::VARCHAR
            | oid::NAME
            | oid::UNKNOWN
    )
}

const fn cast(source: Oid, target: Oid, context: CastContext, method: CastMethod) -> BuiltinCast {
    BuiltinCast {
        source,
        target,
        context,
        method,
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

/// Built-in casts (`pg_cast`), as in PostgreSQL's `pg_cast.dat` for the
/// M1 types. Conversions to and from string types that have no entry here
/// are I/O conversions decided by the analyzer (`find_coercion_pathway`).
/// Same-type length coercion (`varchar(n)`) is a separate `CoerceTypmod`
/// step, not a row here.
pub static CASTS: &[BuiltinCast] = &[
    cast(
        oid::INT2,
        oid::INT4,
        CastContext::Implicit,
        CastMethod::Function(ops::to_int4),
    ),
    cast(
        oid::INT2,
        oid::INT8,
        CastContext::Implicit,
        CastMethod::Function(ops::to_int8),
    ),
    cast(
        oid::INT2,
        oid::FLOAT4,
        CastContext::Implicit,
        CastMethod::Function(ops::to_float4),
    ),
    cast(
        oid::INT2,
        oid::FLOAT8,
        CastContext::Implicit,
        CastMethod::Function(ops::to_float8),
    ),
    cast(
        oid::INT4,
        oid::INT8,
        CastContext::Implicit,
        CastMethod::Function(ops::to_int8),
    ),
    cast(
        oid::INT4,
        oid::INT2,
        CastContext::Assignment,
        CastMethod::Function(ops::to_int2),
    ),
    cast(
        oid::INT4,
        oid::FLOAT4,
        CastContext::Implicit,
        CastMethod::Function(ops::to_float4),
    ),
    cast(
        oid::INT4,
        oid::FLOAT8,
        CastContext::Implicit,
        CastMethod::Function(ops::to_float8),
    ),
    cast(
        oid::INT8,
        oid::INT2,
        CastContext::Assignment,
        CastMethod::Function(ops::to_int2),
    ),
    cast(
        oid::INT8,
        oid::INT4,
        CastContext::Assignment,
        CastMethod::Function(ops::to_int4),
    ),
    cast(
        oid::INT8,
        oid::FLOAT4,
        CastContext::Implicit,
        CastMethod::Function(ops::to_float4),
    ),
    cast(
        oid::INT8,
        oid::FLOAT8,
        CastContext::Implicit,
        CastMethod::Function(ops::to_float8),
    ),
    cast(
        oid::FLOAT4,
        oid::INT2,
        CastContext::Assignment,
        CastMethod::Function(ops::to_int2),
    ),
    cast(
        oid::FLOAT4,
        oid::INT4,
        CastContext::Assignment,
        CastMethod::Function(ops::to_int4),
    ),
    cast(
        oid::FLOAT4,
        oid::INT8,
        CastContext::Assignment,
        CastMethod::Function(ops::to_int8),
    ),
    cast(
        oid::FLOAT4,
        oid::FLOAT8,
        CastContext::Implicit,
        CastMethod::Function(ops::to_float8),
    ),
    cast(
        oid::FLOAT8,
        oid::INT2,
        CastContext::Assignment,
        CastMethod::Function(ops::to_int2),
    ),
    cast(
        oid::FLOAT8,
        oid::INT4,
        CastContext::Assignment,
        CastMethod::Function(ops::to_int4),
    ),
    cast(
        oid::FLOAT8,
        oid::INT8,
        CastContext::Assignment,
        CastMethod::Function(ops::to_int8),
    ),
    cast(
        oid::FLOAT8,
        oid::FLOAT4,
        CastContext::Assignment,
        CastMethod::Function(ops::to_float4),
    ),
    cast(
        oid::INT4,
        oid::BOOL,
        CastContext::Explicit,
        CastMethod::Function(ops::int4_to_bool),
    ),
    cast(
        oid::BOOL,
        oid::INT4,
        CastContext::Explicit,
        CastMethod::Function(ops::bool_to_int4),
    ),
    cast(
        oid::BOOL,
        oid::TEXT,
        CastContext::Assignment,
        CastMethod::Function(ops::bool_to_text),
    ),
    cast(
        oid::BOOL,
        oid::VARCHAR,
        CastContext::Assignment,
        CastMethod::Function(ops::bool_to_text),
    ),
    cast(
        oid::TEXT,
        oid::VARCHAR,
        CastContext::Implicit,
        CastMethod::Binary,
    ),
    cast(
        oid::VARCHAR,
        oid::TEXT,
        CastContext::Implicit,
        CastMethod::Binary,
    ),
    cast(
        oid::TEXT,
        oid::NAME,
        CastContext::Implicit,
        CastMethod::Function(ops::text_to_name),
    ),
    cast(
        oid::VARCHAR,
        oid::NAME,
        CastContext::Implicit,
        CastMethod::Function(ops::text_to_name),
    ),
    cast(
        oid::NAME,
        oid::TEXT,
        CastContext::Implicit,
        CastMethod::Function(ops::text_identity),
    ),
    cast(
        oid::NAME,
        oid::VARCHAR,
        CastContext::Assignment,
        CastMethod::Function(ops::text_identity),
    ),
    cast(
        oid::INT2,
        NUMERIC,
        CastContext::Implicit,
        CastMethod::Function(ops::unsupported),
    ),
    cast(
        oid::INT4,
        NUMERIC,
        CastContext::Implicit,
        CastMethod::Function(ops::unsupported),
    ),
    cast(
        oid::INT8,
        NUMERIC,
        CastContext::Implicit,
        CastMethod::Function(ops::unsupported),
    ),
    cast(
        oid::FLOAT4,
        NUMERIC,
        CastContext::Assignment,
        CastMethod::Function(ops::unsupported),
    ),
    cast(
        oid::FLOAT8,
        NUMERIC,
        CastContext::Assignment,
        CastMethod::Function(ops::unsupported),
    ),
    cast(
        NUMERIC,
        oid::INT2,
        CastContext::Assignment,
        CastMethod::Function(ops::unsupported),
    ),
    cast(
        NUMERIC,
        oid::INT4,
        CastContext::Assignment,
        CastMethod::Function(ops::unsupported),
    ),
    cast(
        NUMERIC,
        oid::INT8,
        CastContext::Assignment,
        CastMethod::Function(ops::unsupported),
    ),
    cast(
        NUMERIC,
        oid::FLOAT4,
        CastContext::Implicit,
        CastMethod::Function(ops::unsupported),
    ),
    cast(
        NUMERIC,
        oid::FLOAT8,
        CastContext::Implicit,
        CastMethod::Function(ops::unsupported),
    ),
];

/// Built-in operators (`pg_operator`). Cross-width integer operators share
/// the body of the result type's operator (the bodies accept any width).
/// `text || anynonarray` (2779) and `anynonarray || text` (2780) are SQL
/// functions `$1::text || $2` in PostgreSQL: the analyzer converts the
/// polymorphic argument to text explicitly, so their body is `textcat`.
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
    op(1752, "=", NUMERIC, NUMERIC, oid::BOOL, ops::unsupported),
    op(1753, "<>", NUMERIC, NUMERIC, oid::BOOL, ops::unsupported),
    op(1754, "<", NUMERIC, NUMERIC, oid::BOOL, ops::unsupported),
    op(1755, "<=", NUMERIC, NUMERIC, oid::BOOL, ops::unsupported),
    op(1756, ">", NUMERIC, NUMERIC, oid::BOOL, ops::unsupported),
    op(1757, ">=", NUMERIC, NUMERIC, oid::BOOL, ops::unsupported),
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
    op(172, "/", oid::INT2, oid::INT4, oid::INT4, ops::int4div),
    op(553, "+", oid::INT4, oid::INT2, oid::INT4, ops::int4pl),
    op(557, "-", oid::INT4, oid::INT2, oid::INT4, ops::int4mi),
    op(545, "*", oid::INT4, oid::INT2, oid::INT4, ops::int4mul),
    op(173, "/", oid::INT4, oid::INT2, oid::INT4, ops::int4div),
    op(688, "+", oid::INT4, oid::INT8, oid::INT8, ops::int8pl),
    op(689, "-", oid::INT4, oid::INT8, oid::INT8, ops::int8mi),
    op(690, "*", oid::INT4, oid::INT8, oid::INT8, ops::int8mul),
    op(691, "/", oid::INT4, oid::INT8, oid::INT8, ops::int8div),
    op(692, "+", oid::INT8, oid::INT4, oid::INT8, ops::int8pl),
    op(693, "-", oid::INT8, oid::INT4, oid::INT8, ops::int8mi),
    op(694, "*", oid::INT8, oid::INT4, oid::INT8, ops::int8mul),
    op(695, "/", oid::INT8, oid::INT4, oid::INT8, ops::int8div),
    op(818, "+", oid::INT2, oid::INT8, oid::INT8, ops::int8pl),
    op(819, "-", oid::INT2, oid::INT8, oid::INT8, ops::int8mi),
    op(820, "*", oid::INT2, oid::INT8, oid::INT8, ops::int8mul),
    op(821, "/", oid::INT2, oid::INT8, oid::INT8, ops::int8div),
    op(822, "+", oid::INT8, oid::INT2, oid::INT8, ops::int8pl),
    op(823, "-", oid::INT8, oid::INT2, oid::INT8, ops::int8mi),
    op(824, "*", oid::INT8, oid::INT2, oid::INT8, ops::int8mul),
    op(825, "/", oid::INT8, oid::INT2, oid::INT8, ops::int8div),
    op(
        586,
        "+",
        oid::FLOAT4,
        oid::FLOAT4,
        oid::FLOAT4,
        ops::float4pl,
    ),
    op(
        587,
        "-",
        oid::FLOAT4,
        oid::FLOAT4,
        oid::FLOAT4,
        ops::float4mi,
    ),
    op(
        589,
        "*",
        oid::FLOAT4,
        oid::FLOAT4,
        oid::FLOAT4,
        ops::float4mul,
    ),
    op(
        588,
        "/",
        oid::FLOAT4,
        oid::FLOAT4,
        oid::FLOAT4,
        ops::float4div,
    ),
    op(
        591,
        "+",
        oid::FLOAT8,
        oid::FLOAT8,
        oid::FLOAT8,
        ops::float8pl,
    ),
    op(
        592,
        "-",
        oid::FLOAT8,
        oid::FLOAT8,
        oid::FLOAT8,
        ops::float8mi,
    ),
    op(
        594,
        "*",
        oid::FLOAT8,
        oid::FLOAT8,
        oid::FLOAT8,
        ops::float8mul,
    ),
    op(
        593,
        "/",
        oid::FLOAT8,
        oid::FLOAT8,
        oid::FLOAT8,
        ops::float8div,
    ),
    op(
        1116,
        "+",
        oid::FLOAT4,
        oid::FLOAT8,
        oid::FLOAT8,
        ops::float8pl,
    ),
    op(
        1117,
        "-",
        oid::FLOAT4,
        oid::FLOAT8,
        oid::FLOAT8,
        ops::float8mi,
    ),
    op(
        1119,
        "*",
        oid::FLOAT4,
        oid::FLOAT8,
        oid::FLOAT8,
        ops::float8mul,
    ),
    op(
        1118,
        "/",
        oid::FLOAT4,
        oid::FLOAT8,
        oid::FLOAT8,
        ops::float8div,
    ),
    op(
        1126,
        "+",
        oid::FLOAT8,
        oid::FLOAT4,
        oid::FLOAT8,
        ops::float8pl,
    ),
    op(
        1127,
        "-",
        oid::FLOAT8,
        oid::FLOAT4,
        oid::FLOAT8,
        ops::float8mi,
    ),
    op(
        1129,
        "*",
        oid::FLOAT8,
        oid::FLOAT4,
        oid::FLOAT8,
        ops::float8mul,
    ),
    op(
        1128,
        "/",
        oid::FLOAT8,
        oid::FLOAT4,
        oid::FLOAT8,
        ops::float8div,
    ),
    op(1758, "+", NUMERIC, NUMERIC, NUMERIC, ops::unsupported),
    op(1759, "-", NUMERIC, NUMERIC, NUMERIC, ops::unsupported),
    op(1760, "*", NUMERIC, NUMERIC, NUMERIC, ops::unsupported),
    op(1761, "/", NUMERIC, NUMERIC, NUMERIC, ops::unsupported),
    op(1762, "%", NUMERIC, NUMERIC, NUMERIC, ops::unsupported),
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
    op(1751, "-", 0, NUMERIC, NUMERIC, ops::unsupported),
    op(1336, "-", 0, INTERVAL, INTERVAL, ops::unsupported),
    op(1911, "+", 0, oid::INT2, oid::INT2, ops::identity),
    op(1912, "+", 0, oid::INT4, oid::INT4, ops::identity),
    op(1910, "+", 0, oid::INT8, oid::INT8, ops::identity),
    op(1913, "+", 0, oid::FLOAT4, oid::FLOAT4, ops::identity),
    op(1914, "+", 0, oid::FLOAT8, oid::FLOAT8, ops::identity),
    op(1915, "+", 0, NUMERIC, NUMERIC, ops::unsupported),
    op(
        965,
        "^",
        oid::FLOAT8,
        oid::FLOAT8,
        oid::FLOAT8,
        ops::float8pow,
    ),
    op(1038, "^", NUMERIC, NUMERIC, NUMERIC, ops::unsupported),
    op(654, "||", oid::TEXT, oid::TEXT, oid::TEXT, ops::textcat),
    op(2779, "||", oid::TEXT, ANYNONARRAY, oid::TEXT, ops::textcat),
    op(2780, "||", ANYNONARRAY, oid::TEXT, oid::TEXT, ops::textcat),
    op(1209, "~~", oid::TEXT, oid::TEXT, oid::BOOL, ops::textlike),
    op(1210, "!~~", oid::TEXT, oid::TEXT, oid::BOOL, ops::textnlike),
    op(
        1627,
        "~~*",
        oid::TEXT,
        oid::TEXT,
        oid::BOOL,
        ops::texticlike,
    ),
    op(
        1628,
        "!~~*",
        oid::TEXT,
        oid::TEXT,
        oid::BOOL,
        ops::texticnlike,
    ),
];

/// Built-in functions (`pg_proc`). `current_database()` and
/// `current_schema()` are resolved here but replaced by the analyzer with
/// a session value (built-ins cannot see the session).
pub static FUNCTIONS: &[BuiltinFunction] = &[
    func(1394, "abs", &[oid::FLOAT4], oid::FLOAT4, ops::float4abs),
    func(1395, "abs", &[oid::FLOAT8], oid::FLOAT8, ops::float8abs),
    func(1396, "abs", &[oid::INT8], oid::INT8, ops::int8abs),
    func(1397, "abs", &[oid::INT4], oid::INT4, ops::int4abs),
    func(1398, "abs", &[oid::INT2], oid::INT2, ops::int2abs),
    func(1705, "abs", &[NUMERIC], NUMERIC, ops::unsupported),
    func(1317, "length", &[oid::TEXT], oid::INT4, ops::textlen),
    func(870, "lower", &[oid::TEXT], oid::TEXT, ops::lower),
    func(871, "upper", &[oid::TEXT], oid::TEXT, ops::upper),
    func(
        1258,
        "textcat",
        &[oid::TEXT, oid::TEXT],
        oid::TEXT,
        ops::textcat,
    ),
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
    func(
        1401,
        "varchar",
        &[oid::NAME],
        oid::VARCHAR,
        ops::text_identity,
    ),
];

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_lookup() {
        assert_eq!(type_by_name("int4").unwrap().oid, oid::INT4);
        assert_eq!(type_by_oid(oid::VARCHAR).unwrap().name, "varchar");
        assert_eq!(type_by_oid(oid::UNKNOWN).unwrap().typlen, -2);
        assert!(type_by_name("integer").is_none());
        // OIDs are unique.
        for (i, a) in TYPES.iter().enumerate() {
            for b in &TYPES[i + 1..] {
                assert_ne!(a.oid, b.oid);
            }
        }
    }
}
