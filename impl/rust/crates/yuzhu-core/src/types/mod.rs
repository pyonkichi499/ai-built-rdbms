//! Type system (lowest layer). Types are identified by OID + typmod, as in
//! PostgreSQL.

pub mod bpchar;
pub mod cmp;
pub mod datetime;
pub mod datum;
mod float_fmt;
pub mod funcs;
pub mod hash;
pub mod io;
pub mod numeric;
pub mod ops;
pub mod regex;
pub mod sys;
pub mod typmod;

pub use datum::{Datum, Row, cmp_datum};

/// Object identifier (same numbering as PostgreSQL).
pub type Oid = u32;

/// 同じ型どうしの比較関数（NULL は渡されない。整数の幅違いは可）。B+Tree の演算子クラスと
/// `IndexKeyColumn` が使う（`catalog::opclass::CmpFn` は B2 がこの別名を再公開する）。
pub type CmpFn = fn(&Datum, &Datum) -> std::cmp::Ordering;

/// regclass / regtype / regproc の名前と OID の相互変換の口（`types` は `catalog` より下の層なので
/// trait で受ける。`m4/09-types-functions.md` §3.4、00 への変更提案 P-1）。
pub trait OidNames: std::fmt::Debug {
    /// regclassin: 名前（修飾可・引用符可）から OID。見つからなければ 42P01。数字だけなら OID としてそのまま。
    fn class_oid(&self, name: &str) -> crate::error::Result<Oid>;
    /// regclassout: 検索パスで見えれば修飾なし、見えなければ `schema.name`。存在しなければ `None`。
    fn class_name(&self, oid: Oid) -> Option<String>;
    /// regtypein: SQL の型名（別名・引用符・typmod 付きを許す）から OID。
    fn type_oid(&self, name: &str) -> crate::error::Result<Oid>;
    /// regtypeout: `format_type_be` 相当。
    fn type_name(&self, oid: Oid) -> Option<String>;
    /// regprocout: `builtin::regproc_name` 相当。
    fn proc_name(&self, oid: Oid) -> Option<String>;
    /// regnamespacein: スキーマ名から OID。見つからなければ 3F000。既定は未対応。
    fn namespace_oid(&self, name: &str) -> crate::error::Result<Oid> {
        Err(crate::error::Error::internal(format!(
            "regnamespace input is not available: {name}"
        )))
    }
    /// regnamespaceout。存在しなければ `None`。
    fn namespace_name(&self, _oid: Oid) -> Option<String> {
        None
    }
}

/// テキストの入出力が参照する設定。session が文ごとに作る（`m4/00-contracts.md` §12.4）。
#[derive(Clone, Copy, Debug)]
pub struct TypeEnv<'a> {
    pub extra_float_digits: i32,
    /// 日時の入出力に必要（DateStyle・TimeZone）。`None` の文脈（initdb・一部のテスト）で日時型を
    /// 入出力したら `Error::internal`。
    pub datetime: Option<yuzhu_datetime::DateTimeEnv<'a>>,
    /// regclass / regtype / regproc の名前表示。`None` の文脈では数字で扱う。
    pub names: Option<&'a dyn OidNames>,
}

impl Default for TypeEnv<'_> {
    fn default() -> Self {
        TypeEnv {
            extra_float_digits: 1,
            datetime: None,
            names: None,
        }
    }
}

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
    pub const NUMERIC: Oid = 1700;
    pub const VARCHAR: Oid = 1043;
    /// `int2vector`（M4。カタログ用）。
    pub const INT2VECTOR: Oid = 22;
    /// `bpchar`（`char(n)`）。
    pub const BPCHAR: Oid = 1042;
    pub const DATE: Oid = 1082;
    /// `time`（M5。キーワードは `0A000`）。
    pub const TIME: Oid = 1083;
    pub const TIMESTAMP: Oid = 1114;
    /// `interval`（M5。キーワードは `0A000`）。
    pub const INTERVAL: Oid = 1186;
    /// `timetz`（M5。キーワードは `0A000`）。
    pub const TIMETZ: Oid = 1266;
    pub const REGCLASS: Oid = 2205;
    pub const REGTYPE: Oid = 2206;
    pub const REGNAMESPACE: Oid = 4089;
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
    pub const NUMERIC: SqlType = SqlType::of(oid::NUMERIC);
    pub const TEXT: SqlType = SqlType::of(oid::TEXT);
    pub const VARCHAR: SqlType = SqlType::of(oid::VARCHAR);
    pub const UNKNOWN: SqlType = SqlType::of(oid::UNKNOWN);
    pub const NAME: SqlType = SqlType::of(oid::NAME);
    pub const OID: SqlType = SqlType::of(oid::OID);
    pub const INT2VECTOR: SqlType = SqlType::of(oid::INT2VECTOR);
    pub const BPCHAR: SqlType = SqlType::of(oid::BPCHAR);
    pub const DATE: SqlType = SqlType::of(oid::DATE);
    pub const TIMESTAMP: SqlType = SqlType::of(oid::TIMESTAMP);
    pub const TIMESTAMPTZ: SqlType = SqlType::of(oid::TIMESTAMPTZ);
    pub const REGCLASS: SqlType = SqlType::of(oid::REGCLASS);
    pub const REGTYPE: SqlType = SqlType::of(oid::REGTYPE);

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
        None if ty.oid == oid::NUMERIC => {
            crate::catalog::builtin::format_type_name(ty.oid, Some(ty.typmod))
        }
        None => type_display_name(ty.oid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_env_default_and_new_type_constants() {
        let env = TypeEnv::default();
        assert_eq!(env.extra_float_digits, 1);
        assert!(env.datetime.is_none() && env.names.is_none());
        assert_eq!(SqlType::BPCHAR.oid, 1042);
        assert_eq!(SqlType::DATE.oid, 1082);
        assert_eq!(SqlType::TIMESTAMP.oid, 1114);
        assert_eq!(SqlType::TIMESTAMPTZ.oid, 1184);
        assert_eq!(SqlType::REGCLASS.oid, 2205);
        assert_eq!(SqlType::REGTYPE.oid, 2206);
        assert_eq!(SqlType::INT2VECTOR.oid, 22);
        assert_eq!(oid::INTERVAL, 1186);
        assert_eq!(oid::TIME, 1083);
        assert_eq!(oid::TIMETZ, 1266);
        // bpchar は Datum::BpChar で持つので is_string_like（Text の変種）には含めない。
        assert!(!SqlType::BPCHAR.is_string_like());
    }

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
