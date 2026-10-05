//! Type system (lowest layer). Types are identified by OID + typmod, as in
//! PostgreSQL.

pub mod datum;
mod float_fmt;
pub mod io;
pub mod ops;
pub mod sys;

pub use datum::{Datum, Row, cmp_datum};

/// Object identifier (same numbering as PostgreSQL).
pub type Oid = u32;

/// Built-in type OIDs (identical to PostgreSQL's `pg_type.oid`).
pub mod oid {
    use super::Oid;
    pub const BOOL: Oid = 16;
    pub const CHAR: Oid = 18;
    pub const NAME: Oid = 19;
    pub const INT8: Oid = 20;
    pub const INT2: Oid = 21;
    pub const INT4: Oid = 23;
    pub const TEXT: Oid = 25;
    pub const OID: Oid = 26;
    pub const REGPROC: Oid = 24;
    pub const TID: Oid = 27;
    pub const XID: Oid = 28;
    pub const CID: Oid = 29;
    pub const OIDVECTOR: Oid = 30;
    pub const PG_NODE_TREE: Oid = 194;
    pub const ACLITEM: Oid = 1033;
    pub const TIMESTAMPTZ: Oid = 1184;
    pub const ANYARRAY: Oid = 2277;
    pub const ACLITEM_ARRAY: Oid = 1034;
    pub const TEXT_ARRAY: Oid = 1009;
    pub const INT2_ARRAY: Oid = 1005;
    pub const OID_ARRAY: Oid = 1028;
    pub const CHAR_ARRAY: Oid = 1002;
    /// `int4[]`（M3。テキスト入出力だけ）。
    pub const INT4_ARRAY: Oid = 1007;
    /// `void`（M3。`pg_sleep` の戻り値）。
    pub const VOID: Oid = 2278;
    /// First OID handed out for objects created by genbki (initdb), as in
    /// `src/include/access/transam.h`.
    pub const FIRST_GENBKI_OBJECT_ID: Oid = 10000;
    pub const FLOAT4: Oid = 700;
    pub const FLOAT8: Oid = 701;
    pub const UNKNOWN: Oid = 705;
    pub const VARCHAR: Oid = 1043;
    /// OIDs of user-created objects start here.
    pub const FIRST_NORMAL_OBJECT_ID: Oid = 16384;
}

/// `ItemPointerData`: heap block number and 1-based line pointer offset
/// (offset 0 is invalid). Ordered by physical position.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Tid {
    pub block: u32,
    pub offset: u16,
}

/// Size of the `varchar` typmod header: `typmod = n + VARHDRSZ`.
pub const VARHDRSZ: i32 = 4;

/// Maximum identifier length in bytes (`NAMEDATALEN - 1`).
pub const MAX_IDENTIFIER_LENGTH: usize = 63;

/// A SQL type: OID plus type modifier (`-1` = no modifier).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SqlType {
    pub oid: Oid,
    pub typmod: i32,
}

impl SqlType {
    pub const BOOL: SqlType = SqlType::of(oid::BOOL);
    pub const INT2: SqlType = SqlType::of(oid::INT2);
    pub const INT4: SqlType = SqlType::of(oid::INT4);
    pub const INT8: SqlType = SqlType::of(oid::INT8);
    pub const FLOAT4: SqlType = SqlType::of(oid::FLOAT4);
    pub const FLOAT8: SqlType = SqlType::of(oid::FLOAT8);
    pub const TEXT: SqlType = SqlType::of(oid::TEXT);
    pub const VARCHAR: SqlType = SqlType::of(oid::VARCHAR);
    pub const UNKNOWN: SqlType = SqlType::of(oid::UNKNOWN);
    pub const NAME: SqlType = SqlType::of(oid::NAME);
    pub const OID: SqlType = SqlType::of(oid::OID);

    pub const fn new(oid: Oid, typmod: i32) -> Self {
        SqlType { oid, typmod }
    }

    /// The type without a modifier (`typmod = -1`).
    pub const fn of(oid: Oid) -> Self {
        SqlType { oid, typmod: -1 }
    }

    /// `varchar(n)`.
    pub const fn varchar(n: i32) -> Self {
        SqlType {
            oid: oid::VARCHAR,
            typmod: n + VARHDRSZ,
        }
    }

    /// For `varchar(n)`, returns `Some(n)`.
    pub fn varchar_len(self) -> Option<i32> {
        (self.oid == oid::VARCHAR && self.typmod >= VARHDRSZ).then(|| self.typmod - VARHDRSZ)
    }

    /// Types whose values are stored as `Datum::Text`.
    pub fn is_string_like(self) -> bool {
        matches!(
            self.oid,
            oid::TEXT | oid::VARCHAR | oid::UNKNOWN | oid::NAME
        )
    }
}

/// The type name as PostgreSQL's `format_type_be` prints it (no typmod),
/// e.g. `integer`, `double precision`, `character varying`. Used in error
/// messages such as `invalid input syntax for type integer`.
pub fn type_display_name(oid: Oid) -> String {
    match oid {
        oid::BOOL => "boolean".into(),
        oid::NAME => "name".into(),
        oid::INT8 => "bigint".into(),
        oid::INT2 => "smallint".into(),
        oid::INT4 => "integer".into(),
        oid::TEXT => "text".into(),
        oid::OID => "oid".into(),
        oid::FLOAT4 => "real".into(),
        oid::FLOAT8 => "double precision".into(),
        oid::UNKNOWN => "unknown".into(),
        oid::VARCHAR => "character varying".into(),
        other => crate::catalog::builtin::format_type_name(other, None),
    }
}

/// The type name with its modifier, as `format_type_with_typemod` prints it,
/// e.g. `character varying(3)`.
pub fn format_type(ty: SqlType) -> String {
    match ty.varchar_len() {
        Some(n) => format!("character varying({n})"),
        None => type_display_name(ty.oid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_type_helpers() {
        let v = SqlType::varchar(3);
        assert_eq!(v.typmod, 7);
        assert_eq!(v.varchar_len(), Some(3));
        assert_eq!(SqlType::VARCHAR.varchar_len(), None);
        assert_eq!(format_type(v), "character varying(3)");
        assert_eq!(format_type(SqlType::INT4), "integer");
        assert!(SqlType::UNKNOWN.is_string_like());
        assert!(!SqlType::INT8.is_string_like());
    }
}
