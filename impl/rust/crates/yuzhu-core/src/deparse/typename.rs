//! `format_type`（`m4/10-explain-copy-compat.md` §4.8）。
//!
//! SQL 関数 `format_type(oid, int4)` と、deparse のキャスト・定数のラベルが共有する。
//! `catalog::builtin::format_type_name` とは独立に完結させている（typmod の書式の正は、この関数）。

use super::ident::quote_identifier;
use crate::catalog::builtin;
use crate::types::{Oid, SqlType, VARHDRSZ, oid};

/// `format_type(oid, typmod)`。`typmod` が `None` または負なら「指定なし」。
///
/// 0 は `-`、型表にない OID は `???`、配列型は `{要素}[]`。
pub fn format_type(type_oid: Oid, typmod: Option<i32>) -> String {
    if type_oid == 0 {
        return "-".to_owned();
    }
    let Some(t) = builtin::type_by_oid(type_oid) else {
        // `pg_type` に行がまだない型でも、PostgreSQL の綴りで答える。
        return match type_oid {
            oid::INT2VECTOR => "int2vector".to_owned(),
            oid::REGCLASS => "regclass".to_owned(),
            oid::REGTYPE => "regtype".to_owned(),
            oid::VOID => "void".to_owned(),
            _ => "???".to_owned(),
        };
    };
    if t.category == 'A' && t.elem != 0 && t.name.starts_with('_') {
        return format!("{}[]", format_type(t.elem, typmod));
    }
    let typmod = typmod.filter(|m| *m >= 0);
    match type_oid {
        oid::BOOL => "boolean".to_owned(),
        oid::INT8 => "bigint".to_owned(),
        oid::INT2 => "smallint".to_owned(),
        oid::INT4 => "integer".to_owned(),
        oid::FLOAT4 => "real".to_owned(),
        oid::FLOAT8 => "double precision".to_owned(),
        oid::VARCHAR => with_length("character varying", typmod),
        oid::BPCHAR => match typmod {
            Some(m) if m > VARHDRSZ => format!("character({})", m - VARHDRSZ),
            // `bpchar` は引数なしのときこの綴り（`character` ではない）。
            _ => "bpchar".to_owned(),
        },
        oid::NUMERIC => match typmod {
            Some(m) if m >= VARHDRSZ => {
                let tmp = m - VARHDRSZ;
                format!("numeric({},{})", (tmp >> 16) & 0xffff, tmp & 0xffff)
            }
            _ => "numeric".to_owned(),
        },
        oid::TIMESTAMP => with_time_zone("timestamp", "without", typmod),
        oid::TIMESTAMPTZ => with_time_zone("timestamp", "with", typmod),
        oid::TIME => with_time_zone("time", "without", typmod),
        oid::TIMETZ => with_time_zone("time", "with", typmod),
        _ => quote_identifier(t.name),
    }
}

/// `SqlType`（oid と typmod）の型名。`format_type_with_typemod`。
pub fn format_sql_type(ty: SqlType) -> String {
    format_type(ty.oid, Some(ty.typmod))
}

/// 配列型 `{要素}[]` の名前（配列型の OID を持たない型のためにも使う）。typmod は付けない。
pub fn format_array_of(elem: Oid) -> String {
    format!("{}[]", format_type(elem, None))
}

fn with_length(name: &str, typmod: Option<i32>) -> String {
    match typmod {
        Some(m) if m > VARHDRSZ => format!("{name}({})", m - VARHDRSZ),
        _ => name.to_owned(),
    }
}

/// `timestamp(p) without time zone` の形。
fn with_time_zone(base: &str, zone: &str, typmod: Option<i32>) -> String {
    match typmod {
        Some(p) => format!("{base}({p}) {zone} time zone"),
        None => format!("{base} {zone} time zone"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(o: Oid, m: i32) -> String {
        format_type(o, Some(m))
    }

    #[test]
    fn base_types_without_modifier() {
        let cases: &[(Oid, &str)] = &[
            (16, "boolean"),
            (18, "\"char\""),
            (19, "name"),
            (20, "bigint"),
            (21, "smallint"),
            (23, "integer"),
            (22, "int2vector"),
            (25, "text"),
            (26, "oid"),
            (700, "real"),
            (701, "double precision"),
            (1042, "bpchar"),
            (1043, "character varying"),
            (1082, "date"),
            (1114, "timestamp without time zone"),
            (1184, "timestamp with time zone"),
            (1700, "numeric"),
            (2205, "regclass"),
            (2206, "regtype"),
            (2278, "void"),
            (0, "-"),
        ];
        for (o, want) in cases {
            assert_eq!(format_type(*o, None), *want, "oid {o}");
            assert_eq!(format_type(*o, Some(-1)), *want, "oid {o} with -1");
        }
    }

    #[test]
    fn modifiers() {
        assert_eq!(f(1042, 5), "character(1)");
        assert_eq!(f(1042, 4), "bpchar");
        assert_eq!(f(1043, 12), "character varying(8)");
        assert_eq!(f(1043, 4), "character varying");
        assert_eq!(f(1700, 655_366), "numeric(10,2)");
        assert_eq!(f(1700, 4), "numeric(0,0)");
        assert_eq!(f(1114, 3), "timestamp(3) without time zone");
        assert_eq!(f(1184, 0), "timestamp(0) with time zone");
        // Types without a modifier output ignore it.
        assert_eq!(f(23, 7), "integer");
        assert_eq!(f(25, 7), "text");
    }

    #[test]
    fn arrays_and_unknown() {
        assert_eq!(format_type(1007, None), "integer[]");
        assert_eq!(format_type(1009, None), "text[]");
        assert_eq!(format_array_of(oid::INT8), "bigint[]");
        assert_eq!(format_array_of(oid::VARCHAR), "character varying[]");
        assert_eq!(format_type(999_999, None), "???");
    }

    #[test]
    fn sql_type_uses_its_typmod() {
        assert_eq!(format_sql_type(SqlType::varchar(8)), "character varying(8)");
        assert_eq!(format_sql_type(SqlType::INT4), "integer");
    }
}
