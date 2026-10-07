//! 定数の書式（`m4/10-explain-copy-compat.md` §4.3。PostgreSQL の `get_const_expr`）。

use super::typename::{format_array_of, format_sql_type};
use crate::catalog::CatalogReader;
use crate::error::{Error, Result};
use crate::types::io::try_output_text_env;
use crate::types::{Datum, SqlType, TypeEnv, oid};

/// `'...'` に包む。`'` は `''`、`\` はそのまま（`standard_conforming_strings = on`）。
pub fn quote_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push('\'');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// 値の出力文字列（NULL でない値）。
fn value_text(
    d: &Datum,
    ty: SqlType,
    type_env: &TypeEnv<'_>,
    catalog: &dyn CatalogReader,
) -> Result<String> {
    if ty.oid == oid::REGCLASS
        && let Datum::Oid(v) = d
        && *v != 0
        && let Some(name) = catalog.relation_name(*v)?
    {
        return Ok(name);
    }
    try_output_text_env(d, ty, type_env)?
        .ok_or_else(|| Error::internal(format!("cannot print a constant of type {}", ty.oid)))
}

/// 定数 1 つ（`Expr.ty` で書式を決める。`Datum` の変種ではない）。
pub fn literal(
    d: &Datum,
    ty: SqlType,
    type_env: &TypeEnv<'_>,
    catalog: &dyn CatalogReader,
) -> Result<String> {
    if d.is_null() {
        return Ok(format!("NULL::{}", format_sql_type(ty)));
    }
    let text = value_text(d, ty, type_env, catalog)?;
    let label = |quoted: String| format!("{quoted}::{}", format_sql_type(ty));
    Ok(match ty.oid {
        oid::BOOL => match d {
            Datum::Bool(true) => "true".to_owned(),
            Datum::Bool(false) => "false".to_owned(),
            _ => text,
        },
        oid::UNKNOWN => quote_literal(&text),
        oid::INT4 => {
            if text.starts_with('-') {
                label(quote_literal(&text))
            } else {
                text
            }
        }
        oid::NUMERIC => {
            let looks_numeric = text.starts_with(|c: char| c.is_ascii_digit())
                && text.contains(['.', 'e', 'E'])
                && text
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'));
            if looks_numeric {
                if ty.typmod >= 0 { label(text) } else { text }
            } else {
                label(quote_literal(&text))
            }
        }
        _ => label(quote_literal(&text)),
    })
}

/// `array_out` の要素のクォート。
fn array_element(s: &str) -> String {
    let needs = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars().any(|c| {
            matches!(c, '{' | '}' | ',' | '"' | '\\')
                || matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}')
        });
    if !needs {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// 配列定数（`Plan` の `IN` 用）: `'{1,2,3}'::integer[]`。要素の型は `elem_ty`。
pub fn array_literal(
    elems: &[Datum],
    elem_ty: SqlType,
    type_env: &TypeEnv<'_>,
    catalog: &dyn CatalogReader,
) -> Result<String> {
    let mut parts = Vec::with_capacity(elems.len());
    for d in elems {
        if d.is_null() {
            parts.push("NULL".to_owned());
        } else {
            parts.push(array_element(&value_text(d, elem_ty, type_env, catalog)?));
        }
    }
    let body = format!("{{{}}}", parts.join(","));
    Ok(format!(
        "{}::{}",
        quote_literal(&body),
        format_array_of(elem_ty.oid)
    ))
}

/// PostgreSQL の `like_escape`: エスケープ文字の直後の文字の前に `\` を置き、エスケープ文字自体は消す。
/// エスケープ文字でない `\` は `\\` にする。`escape` が空なら「エスケープなし」で、`\` を倍にする。
/// `escape` が `\` ならパターンはそのまま。パターンがエスケープ文字で終わるなら `None`（PostgreSQL は
/// `22025` のエラー。畳み込まずに関数呼び出しのまま出す）。
pub fn like_escape(pattern: &str, escape: &str) -> Option<String> {
    let mut esc_chars = escape.chars();
    let esc = esc_chars.next();
    if esc_chars.next().is_some() {
        return None;
    }
    let Some(esc) = esc else {
        return Some(pattern.replace('\\', "\\\\"));
    };
    if esc == '\\' {
        return Some(pattern.to_owned());
    }
    let mut out = String::with_capacity(pattern.len() + 2);
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == esc {
            out.push('\\');
            out.push(chars.next()?);
        } else if c == '\\' {
            out.push_str("\\\\");
        } else {
            out.push(c);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::FakeCatalog;
    use crate::types::Datum;

    #[allow(clippy::needless_pass_by_value)]
    fn lit(d: Datum, ty: SqlType) -> String {
        let cat = FakeCatalog::new("db");
        let env = TypeEnv::default();
        literal(&d, ty, &env, &cat).unwrap()
    }

    fn num(s: &str) -> Datum {
        crate::types::io::input_text_env(s, SqlType::NUMERIC, &TypeEnv::default()).unwrap()
    }

    #[test]
    fn nulls_are_labelled() {
        assert_eq!(lit(Datum::Null, SqlType::INT4), "NULL::integer");
        assert_eq!(lit(Datum::Null, SqlType::TEXT), "NULL::text");
        assert_eq!(lit(Datum::Null, SqlType::BOOL), "NULL::boolean");
        assert_eq!(
            lit(Datum::Null, SqlType::varchar(5)),
            "NULL::character varying(5)"
        );
    }

    #[test]
    fn booleans_and_integers() {
        assert_eq!(lit(Datum::Bool(true), SqlType::BOOL), "true");
        assert_eq!(lit(Datum::Bool(false), SqlType::BOOL), "false");
        assert_eq!(lit(Datum::Int4(5), SqlType::INT4), "5");
        assert_eq!(lit(Datum::Int4(0), SqlType::INT4), "0");
        assert_eq!(lit(Datum::Int4(-1), SqlType::INT4), "'-1'::integer");
        assert_eq!(lit(Datum::Int8(5), SqlType::INT8), "'5'::bigint");
        assert_eq!(
            lit(Datum::Int8(2_147_483_648), SqlType::INT8),
            "'2147483648'::bigint"
        );
        assert_eq!(lit(Datum::Int2(3), SqlType::INT2), "'3'::smallint");
    }

    #[test]
    fn numerics() {
        assert_eq!(lit(num("1.5"), SqlType::NUMERIC), "1.5");
        assert_eq!(lit(num("1.50"), SqlType::NUMERIC), "1.50");
        assert_eq!(lit(num("1"), SqlType::NUMERIC), "'1'::numeric");
        assert_eq!(lit(num("-1.5"), SqlType::NUMERIC), "'-1.5'::numeric");
        assert_eq!(lit(num("NaN"), SqlType::NUMERIC), "'NaN'::numeric");
    }

    #[test]
    fn floats() {
        assert_eq!(
            lit(Datum::Float8(1.5), SqlType::FLOAT8),
            "'1.5'::double precision"
        );
        assert_eq!(
            lit(Datum::Float8(1e10), SqlType::FLOAT8),
            "'10000000000'::double precision"
        );
        assert_eq!(
            lit(Datum::Float8(1e100), SqlType::FLOAT8),
            "'1e+100'::double precision"
        );
        assert_eq!(
            lit(Datum::Float8(f64::INFINITY), SqlType::FLOAT8),
            "'Infinity'::double precision"
        );
        assert_eq!(lit(Datum::Float4(1.5), SqlType::FLOAT4), "'1.5'::real");
    }

    #[test]
    fn strings() {
        assert_eq!(lit(Datum::Text("abc".into()), SqlType::TEXT), "'abc'::text");
        assert_eq!(lit(Datum::Text(String::new()), SqlType::TEXT), "''::text");
        assert_eq!(
            lit(Datum::Text("it's".into()), SqlType::TEXT),
            "'it''s'::text"
        );
        assert_eq!(
            lit(Datum::Text("a\\b".into()), SqlType::TEXT),
            "'a\\b'::text"
        );
        assert_eq!(
            lit(Datum::Text("a\nb".into()), SqlType::TEXT),
            "'a\nb'::text"
        );
        assert_eq!(lit(Datum::Text("abc".into()), SqlType::UNKNOWN), "'abc'");
        assert_eq!(
            lit(Datum::Text("ab".into()), SqlType::VARCHAR),
            "'ab'::character varying"
        );
        assert_eq!(
            lit(Datum::BpChar("ab".into()), SqlType::BPCHAR),
            "'ab'::bpchar"
        );
        assert_eq!(lit(Datum::Oid(5), SqlType::OID), "'5'::oid");
    }

    #[test]
    fn array_constants_quote_like_array_out() {
        let cat = FakeCatalog::new("db");
        let env = TypeEnv::default();
        let ints = [Datum::Int4(1), Datum::Int4(2), Datum::Null];
        assert_eq!(
            array_literal(&ints, SqlType::INT4, &env, &cat).unwrap(),
            "'{1,2,NULL}'::integer[]"
        );
        let texts = [
            Datum::Text("a b".into()),
            Datum::Null,
            Datum::Text("NULL".into()),
            Datum::Text(String::new()),
            Datum::Text("x,y".into()),
            Datum::Text("q\"r\\s".into()),
            Datum::Text("it's".into()),
        ];
        assert_eq!(
            array_literal(&texts, SqlType::TEXT, &env, &cat).unwrap(),
            r#"'{"a b",NULL,"NULL","","x,y","q\"r\\s",it''s}'::text[]"#
        );
        assert_eq!(
            array_literal(&[Datum::Int8(1)], SqlType::INT8, &env, &cat).unwrap(),
            "'{1}'::bigint[]"
        );
        assert_eq!(
            array_literal(&[Datum::Text("a".into())], SqlType::VARCHAR, &env, &cat).unwrap(),
            "'{a}'::character varying[]"
        );
    }

    #[test]
    fn like_escape_rules() {
        assert_eq!(like_escape("a!%", "!").as_deref(), Some("a\\%"));
        assert_eq!(like_escape("a!!b", "!").as_deref(), Some("a\\!b"));
        assert_eq!(like_escape("a\\b!_", "!").as_deref(), Some("a\\\\b\\_"));
        assert_eq!(like_escape("a\\b", "").as_deref(), Some("a\\\\b"));
        assert_eq!(like_escape("a\\%", "\\").as_deref(), Some("a\\%"));
        assert_eq!(like_escape("a!", "!"), None);
        assert_eq!(like_escape("a", "ab"), None);
    }
}
