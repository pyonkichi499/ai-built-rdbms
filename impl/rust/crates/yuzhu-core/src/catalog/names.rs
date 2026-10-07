//! `CatalogNames`: regclass / regtype / regproc の名前引き（`OidNames` の実装。`m4/09-types-functions.md`
//! §3.4・§3.9、11 §7.1 の C-17）。
//!
//! 規則は PostgreSQL 17 の実機で確認した（`regclassin`・`regtypein`・`regclassout`・`regtypeout`）。

use super::CatalogReader;
use super::builtin;
use crate::error::{Error, Result, sqlstate};
use crate::types::{MAX_IDENTIFIER_LENGTH, Oid, OidNames, oid};

/// `CatalogReader` を通して名前と OID を相互に変換する。
#[derive(Debug)]
pub struct CatalogNames<'a>(pub &'a dyn CatalogReader);

fn invalid_name() -> Error {
    Error::new(sqlstate::INVALID_NAME, "invalid name syntax")
}

fn is_scanner_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

/// 引用符なしの識別子: ASCII の大文字だけを小文字にし、63 バイトで切る
/// （`downcase_truncate_identifier`）。
fn downcase_truncate(s: &str) -> String {
    let mut out = s.to_ascii_lowercase();
    truncate_at_boundary(&mut out);
    out
}

fn truncate_at_boundary(s: &mut String) {
    if s.len() > MAX_IDENTIFIER_LENGTH {
        let mut end = MAX_IDENTIFIER_LENGTH;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// `SplitIdentifierString(s, '.')`: ドットで区切った識別子の並び。引用符つきは大文字小文字と記号を保つ。
/// 空・不正なら `42602 invalid name syntax`。
pub fn split_qualified_name(s: &str) -> Result<Vec<String>> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    let skip_ws = |i: &mut usize| {
        while *i < chars.len() && is_scanner_space(chars[*i]) {
            *i += 1;
        }
    };
    loop {
        skip_ws(&mut i);
        let mut ident = String::new();
        if chars.get(i) == Some(&'"') {
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err(invalid_name()),
                    Some('"') if chars.get(i + 1) == Some(&'"') => {
                        ident.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(c) => {
                        ident.push(*c);
                        i += 1;
                    }
                }
            }
            if ident.is_empty() {
                return Err(invalid_name());
            }
            truncate_at_boundary(&mut ident);
        } else {
            let start = i;
            while i < chars.len() && chars[i] != '.' && !is_scanner_space(chars[i]) {
                i += 1;
            }
            if i == start {
                return Err(invalid_name());
            }
            ident = downcase_truncate(&chars[start..i].iter().collect::<String>());
        }
        out.push(ident);
        skip_ws(&mut i);
        match chars.get(i) {
            None => return Ok(out),
            Some('.') => i += 1,
            Some(_) => return Err(invalid_name()),
        }
    }
}

/// 数字だけの文字列（`regclassin` / `regtypein` が OID としてそのまま読む形）。
fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn parse_oid_digits(s: &str) -> Result<Oid> {
    s.parse::<u32>().map_err(|_| {
        Error::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("value \"{s}\" is out of range for type oid"),
        )
    })
}

/// M4 の名前空間（`CREATE SCHEMA` はまだ無い）。
const KNOWN_NAMESPACES: [(&str, Oid); 3] = [("pg_catalog", 11), ("pg_toast", 99), ("public", 2200)];

impl OidNames for CatalogNames<'_> {
    fn class_oid(&self, name: &str) -> Result<Oid> {
        if all_digits(name) {
            return parse_oid_digits(name);
        }
        let mut parts = split_qualified_name(name)?;
        if parts.len() > 3 {
            return Err(Error::new(
                sqlstate::SYNTAX_ERROR,
                format!("improper relation name (too many dotted names): {name}"),
            ));
        }
        if parts.len() == 3 {
            if parts[0] != self.0.current_database() {
                return Err(Error::not_supported(format!(
                    "cross-database references are not implemented: \"{name}\""
                )));
            }
            parts.remove(0);
        }
        let (schema, rel) = match parts.as_slice() {
            [rel] => (None, rel.as_str()),
            [schema, rel] => (Some(schema.as_str()), rel.as_str()),
            _ => return Err(invalid_name()),
        };
        match self.0.relation_kind(schema, rel)? {
            Some((oid, _)) => Ok(oid),
            None => Err(Error::new(
                sqlstate::UNDEFINED_TABLE,
                match schema {
                    Some(s) => format!("relation \"{s}.{rel}\" does not exist"),
                    None => format!("relation \"{rel}\" does not exist"),
                },
            )),
        }
    }

    fn class_name(&self, oid: Oid) -> Option<String> {
        self.0.relation_name(oid).ok().flatten()
    }

    fn type_oid(&self, name: &str) -> Result<Oid> {
        if all_digits(name) {
            return parse_oid_digits(name);
        }
        type_oid_from_sql_name(self.0, name)
    }

    fn type_name(&self, oid: Oid) -> Option<String> {
        self.0.type_by_oid(oid)?;
        Some(builtin::format_type_name(oid, None))
    }

    fn proc_name(&self, oid: Oid) -> Option<String> {
        builtin::regproc_name(oid)
    }

    fn namespace_oid(&self, name: &str) -> Result<Oid> {
        if all_digits(name) {
            return parse_oid_digits(name);
        }
        let parts = split_qualified_name(name)?;
        let [schema] = parts.as_slice() else {
            return Err(Error::new(
                sqlstate::SYNTAX_ERROR,
                format!("improper qualified name (too many dotted names): {name}"),
            ));
        };
        KNOWN_NAMESPACES
            .iter()
            .find(|(n, _)| n == schema)
            .map(|(_, o)| *o)
            .ok_or_else(|| {
                Error::new(
                    sqlstate::INVALID_SCHEMA_NAME,
                    format!("schema \"{schema}\" does not exist"),
                )
            })
    }

    fn namespace_name(&self, oid: Oid) -> Option<String> {
        KNOWN_NAMESPACES
            .iter()
            .find(|(_, o)| *o == oid)
            .map(|(n, _)| (*n).to_owned())
    }
}

// ---------------------------------------------------------------------------
// regtypein: SQL の型名
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// `(テキスト, 引用符つきか)`。
    Word(String, bool),
    Number(String),
    Dot,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Other(char),
}

impl Tok {
    fn text(&self) -> String {
        match self {
            Tok::Word(w, _) | Tok::Number(w) => w.clone(),
            Tok::Dot => ".".into(),
            Tok::LParen => "(".into(),
            Tok::RParen => ")".into(),
            Tok::LBracket => "[".into(),
            Tok::RBracket => "]".into(),
            Tok::Comma => ",".into(),
            Tok::Other(c) => c.to_string(),
        }
    }
}

fn syntax_near(tok: Option<&Tok>) -> Error {
    match tok {
        Some(t) => Error::new(
            sqlstate::SYNTAX_ERROR,
            format!("syntax error at or near \"{}\"", t.text()),
        ),
        None => Error::new(sqlstate::SYNTAX_ERROR, "syntax error at end of input"),
    }
}

fn tokenize_type(s: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if is_scanner_space(c) => i += 1,
            '"' => {
                let mut w = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => {
                            return Err(Error::new(
                                sqlstate::SYNTAX_ERROR,
                                "unterminated quoted identifier",
                            ));
                        }
                        Some('"') if chars.get(i + 1) == Some(&'"') => {
                            w.push('"');
                            i += 2;
                        }
                        Some('"') => {
                            i += 1;
                            break;
                        }
                        Some(c) => {
                            w.push(*c);
                            i += 1;
                        }
                    }
                }
                if w.is_empty() {
                    return Err(Error::new(
                        sqlstate::SYNTAX_ERROR,
                        "zero-length delimited identifier",
                    ));
                }
                out.push(Tok::Word(w, true));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                out.push(Tok::Number(chars[start..i].iter().collect()));
            }
            c if c.is_alphabetic() || c == '_' || !c.is_ascii() => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric()
                        || chars[i] == '_'
                        || chars[i] == '$'
                        || !chars[i].is_ascii())
                {
                    i += 1;
                }
                out.push(Tok::Word(chars[start..i].iter().collect(), false));
            }
            '.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            other => {
                out.push(Tok::Other(other));
                i += 1;
            }
        }
    }
    Ok(out)
}

/// 型名の修飾子（`(10,2)`）の位置から、引数の数値を読む。
fn parse_type_args(toks: &[Tok], i: &mut usize) -> Result<Vec<u64>> {
    let mut args = Vec::new();
    if toks.get(*i) != Some(&Tok::LParen) {
        return Ok(args);
    }
    *i += 1;
    loop {
        match toks.get(*i) {
            Some(Tok::Number(n)) => {
                args.push(n.parse::<u64>().unwrap_or(u64::MAX));
                *i += 1;
            }
            other => return Err(syntax_near(other)),
        }
        match toks.get(*i) {
            Some(Tok::Comma) => *i += 1,
            Some(Tok::RParen) => {
                *i += 1;
                return Ok(args);
            }
            other => return Err(syntax_near(other)),
        }
    }
}

/// `regtypein` の SQL 型名の解釈。別名・引用符・`pg_catalog.` 修飾・`(n)`（捨てる）・`[]`（配列型）を扱う。
fn type_oid_from_sql_name(cat: &dyn CatalogReader, input: &str) -> Result<Oid> {
    let toks = tokenize_type(input)?;
    if toks.is_empty() {
        return Err(Error::new(
            sqlstate::SYNTAX_ERROR,
            format!("invalid type name \"{input}\""),
        ));
    }
    let Some(Tok::Word(first, quoted)) = toks.first() else {
        return Err(syntax_near(toks.first()));
    };
    let mut i = 1;
    // 修飾名 schema.name（3 つ以上は存在しない型として扱う）。
    let mut dotted: Vec<(String, bool)> = vec![(first.clone(), *quoted)];
    while toks.get(i) == Some(&Tok::Dot) {
        match toks.get(i + 1) {
            Some(Tok::Word(w, q)) => {
                dotted.push((w.clone(), *q));
                i += 2;
            }
            other => return Err(syntax_near(other)),
        }
    }
    let norm = |(w, q): &(String, bool)| {
        if *q {
            w.clone()
        } else {
            w.to_ascii_lowercase()
        }
    };
    let display = dotted.iter().map(norm).collect::<Vec<_>>().join(".");
    if dotted.len() > 3 {
        return Err(Error::new(
            sqlstate::SYNTAX_ERROR,
            format!("improper qualified name (too many dotted names): {display}"),
        ));
    }

    let base = if dotted.len() > 1 {
        parse_type_args(&toks, &mut i)?;
        if dotted.len() == 2 && norm(&dotted[0]) == "pg_catalog" {
            cat.type_by_name(&norm(&dotted[1])).map(|t| t.oid)
        } else {
            None
        }
    } else {
        resolve_unqualified(cat, &toks, &mut i, &norm(&dotted[0]), *quoted)?
    };
    // 配列の `[]` / `[n]`（何重でも 1 次元の配列型）。
    let mut is_array = false;
    while toks.get(i) == Some(&Tok::LBracket) {
        i += 1;
        if let Some(Tok::Number(_)) = toks.get(i) {
            i += 1;
        }
        if toks.get(i) != Some(&Tok::RBracket) {
            return Err(syntax_near(toks.get(i)));
        }
        i += 1;
        is_array = true;
    }
    if i < toks.len() {
        return Err(syntax_near(toks.get(i)));
    }
    let not_found = || {
        Error::new(
            sqlstate::UNDEFINED_OBJECT,
            format!(
                "type \"{display}{}\" does not exist",
                if is_array { "[]" } else { "" }
            ),
        )
    };
    let base = base.ok_or_else(not_found)?;
    if !is_array {
        return Ok(base);
    }
    let arr = cat
        .type_by_oid(base)
        .map(|t| t.array_oid)
        .filter(|a| *a != 0 && cat.type_by_oid(*a).is_some());
    arr.ok_or_else(not_found)
}

/// 修飾なしの型名（複数語・`(n)`・`timestamp(3) with time zone` を含む）を読み、`i` を進めて OID を返す。
fn resolve_unqualified(
    cat: &dyn CatalogReader,
    toks: &[Tok],
    i: &mut usize,
    word: &str,
    quoted: bool,
) -> Result<Option<Oid>> {
    // 複数語の型名。引用符つきは別名にならない。
    let mut words = vec![word.to_owned()];
    if !quoted {
        let mut take = |next: &[&str], i: &mut usize| -> bool {
            if let Some(Tok::Word(w, false)) = toks.get(*i) {
                let lw = w.to_ascii_lowercase();
                if next.contains(&lw.as_str()) {
                    words.push(lw);
                    *i += 1;
                    return true;
                }
            }
            false
        };
        match word {
            "double" => {
                take(&["precision"], i);
            }
            "character" | "char" | "nchar" | "bit" => {
                take(&["varying"], i);
            }
            "national" if take(&["character", "char"], i) => {
                take(&["varying"], i);
            }
            "timestamp" | "time" if take(&["with", "without"], i) => {
                take(&["time"], i);
                take(&["zone"], i);
            }
            _ => {}
        }
    }
    let args = parse_type_args(toks, i)?;
    // `timestamp(3) with time zone` の形。
    if !quoted
        && matches!(word, "timestamp" | "time")
        && words.len() == 1
        && let Some(Tok::Word(w, false)) = toks.get(*i)
        && (w.eq_ignore_ascii_case("with") || w.eq_ignore_ascii_case("without"))
    {
        words.push(w.to_ascii_lowercase());
        *i += 1;
        for expect in ["time", "zone"] {
            match toks.get(*i) {
                Some(Tok::Word(w, false)) if w.eq_ignore_ascii_case(expect) => {
                    words.push(expect.into());
                    *i += 1;
                }
                other => return Err(syntax_near(other)),
            }
        }
    }
    if quoted {
        Ok(cat.type_by_name(word).map(|t| t.oid))
    } else {
        resolve_alias(cat, &words, &args)
    }
}

/// 引用符なしの型名（小文字化済みの語の並び）から OID。SQL 標準の別名を含む。
fn resolve_alias(cat: &dyn CatalogReader, words: &[String], args: &[u64]) -> Result<Option<Oid>> {
    let joined = words.join(" ");
    let aliased = match joined.as_str() {
        "int" | "integer" => Some(oid::INT4),
        "smallint" => Some(oid::INT2),
        "bigint" => Some(oid::INT8),
        "real" => Some(oid::FLOAT4),
        "double precision" => Some(oid::FLOAT8),
        "float" => match args.first() {
            Some(0) => {
                return Err(Error::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "precision for type float must be at least 1 bit",
                ));
            }
            Some(1..=24) => Some(oid::FLOAT4),
            None | Some(25..=53) => Some(oid::FLOAT8),
            Some(_) => {
                return Err(Error::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "precision for type float must be less than 54 bits",
                ));
            }
        },
        "boolean" => Some(oid::BOOL),
        "character varying"
        | "char varying"
        | "national character varying"
        | "national char varying"
        | "nchar varying" => Some(oid::VARCHAR),
        "character" | "char" | "national character" | "national char" | "nchar" => {
            Some(oid::BPCHAR)
        }
        "decimal" | "dec" => Some(oid::NUMERIC),
        "timestamp" | "timestamp without time zone" => Some(oid::TIMESTAMP),
        "timestamp with time zone" => Some(oid::TIMESTAMPTZ),
        "time" | "time without time zone" => Some(oid::TIME),
        "time with time zone" => Some(oid::TIMETZ),
        _ => None,
    };
    let found = aliased.or_else(|| {
        if words.len() == 1 {
            cat.type_by_name(&words[0]).map(|t| t.oid)
        } else {
            None
        }
    });
    if found.is_none() && words.len() > 1 {
        // `foo bar` のように語が並んで型名にならないものは構文エラー。
        return Err(Error::new(
            sqlstate::SYNTAX_ERROR,
            format!("syntax error at or near \"{}\"", words[1]),
        ));
    }
    Ok(found.filter(|o| cat.type_by_oid(*o).is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::FakeCatalog;

    fn names(c: &FakeCatalog) -> CatalogNames<'_> {
        CatalogNames(c)
    }

    fn ty(n: &CatalogNames<'_>, s: &str) -> std::result::Result<Oid, (String, String)> {
        n.type_oid(s)
            .map_err(|e| (e.sqlstate.code().to_owned(), e.message))
    }

    #[test]
    fn splits_qualified_names() {
        let v = |s: &str| split_qualified_name(s).unwrap();
        assert_eq!(v("pg_class"), ["pg_class"]);
        assert_eq!(v("PG_CLASS"), ["pg_class"]);
        assert_eq!(v("\"pg_class\""), ["pg_class"]);
        assert_eq!(v("  pg_class  "), ["pg_class"]);
        assert_eq!(v("Public . Foo"), ["public", "foo"]);
        assert_eq!(v("\"A\"\"b\".c"), ["A\"b", "c"]);
        assert_eq!(v("a.b.c.d").len(), 4);
        for bad in [
            "", "   ", "foo bar", "public.", ".x", "\"Foo", "\"\"", "a..b",
        ] {
            let e = split_qualified_name(bad).unwrap_err();
            assert_eq!(e.sqlstate.code(), "42602", "{bad:?}");
            assert_eq!(e.message, "invalid name syntax");
        }
        let long = "x".repeat(70);
        assert_eq!(v(&long)[0].len(), 63);
    }

    #[test]
    fn regtype_names_resolve_like_postgresql() {
        let c = FakeCatalog::new("yuzhu");
        let n = names(&c);
        for (s, want) in [
            ("1259", 1259),
            ("int", 23),
            ("integer", 23),
            ("Integer", 23),
            ("INT4", 23),
            ("int8", 20),
            ("bigint", 20),
            ("smallint", 21),
            ("real", 700),
            ("float4", 700),
            ("float", 701),
            ("float(10)", 700),
            ("float(30)", 701),
            ("double precision", 701),
            ("boolean", 16),
            ("character varying", 1043),
            ("varchar", 1043),
            ("varchar(10)", 1043),
            ("char", 1042),
            ("character", 1042),
            ("bpchar", 1042),
            ("bpchar(3)", 1042),
            ("character(5)", 1042),
            ("national character", 1042),
            ("decimal", 1700),
            ("numeric(10,2)", 1700),
            ("timestamp", 1114),
            ("timestamp without time zone", 1114),
            ("timestamp with time zone", 1184),
            ("timestamptz", 1184),
            ("timestamp(3) with time zone", 1184),
            ("text[]", 1009),
            ("int4[]", 1007),
            ("int[]", 1007),
            ("int[][]", 1007),
            ("_int4", 1007),
            ("pg_catalog.int4", 23),
            ("PG_CATALOG.INT4", 23),
            ("\"int4\"", 23),
            ("\"char\"", 18),
            ("regclass", 2205),
            ("oid", 26),
            ("name", 19),
            ("interval", 1186),
        ] {
            assert_eq!(ty(&n, s), Ok(want), "{s}");
        }
    }

    #[test]
    fn regtype_errors() {
        let c = FakeCatalog::new("yuzhu");
        let n = names(&c);
        let code = |s: &str| ty(&n, s).unwrap_err().0;
        assert_eq!(
            ty(&n, "nosuch").unwrap_err(),
            ("42704".into(), "type \"nosuch\" does not exist".into())
        );
        assert_eq!(code("\"INT4\""), "42704");
        assert_eq!(code("double"), "42704");
        assert_eq!(code("public.int4"), "42704");
        assert_eq!(code(""), "42601");
        assert_eq!(code("foo bar"), "42601");
        assert_eq!(code("1 2"), "42601");
        assert_eq!(code("text["), "42601");
        assert_eq!(code("text]"), "42601");
        assert_eq!(code("varchar("), "42601");
        assert_eq!(code("99999999999"), "22003");
    }

    #[test]
    fn class_names_resolve_like_regclassin() {
        let mut c = FakeCatalog::new("yuzhu");
        let t = c.add(
            &crate::catalog::fake::TableBuilder::new("t1").column("a", crate::types::SqlType::INT4),
        );
        let n = names(&c);
        let err = |s: &str| {
            let e = n.class_oid(s).unwrap_err();
            (e.sqlstate.code().to_owned(), e.message)
        };
        assert_eq!(n.class_oid("t1").unwrap(), t.oid);
        assert_eq!(n.class_oid(" T1 ").unwrap(), t.oid);
        assert_eq!(n.class_oid("\"t1\"").unwrap(), t.oid);
        assert_eq!(n.class_oid("public.t1").unwrap(), t.oid);
        assert_eq!(n.class_oid("yuzhu.public.t1").unwrap(), t.oid);
        assert_eq!(n.class_oid("1259").unwrap(), 1259);
        assert_eq!(n.class_oid("99999999").unwrap(), 99_999_999);
        assert_eq!(
            err("nosuch"),
            ("42P01".into(), "relation \"nosuch\" does not exist".into())
        );
        assert_eq!(
            err("public.nosuch").1,
            "relation \"public.nosuch\" does not exist"
        );
        assert_eq!(err("\"T1\"").0, "42P01");
        for bad in ["", "foo bar", "public.", ".x"] {
            assert_eq!(
                err(bad),
                ("42602".into(), "invalid name syntax".into()),
                "{bad}"
            );
        }
        assert_eq!(
            err("a.b.c.d"),
            (
                "42601".into(),
                "improper relation name (too many dotted names): a.b.c.d".into()
            )
        );
        assert_eq!(err("other.public.t1").0, "0A000");
        assert_eq!(err("99999999999").0, "22003");
        assert_eq!(n.class_name(t.oid).as_deref(), Some("t1"));
        assert_eq!(n.class_name(99_999_999), None);
    }

    #[test]
    fn type_names_for_output() {
        let c = FakeCatalog::new("yuzhu");
        let n = names(&c);
        assert_eq!(n.type_name(23).as_deref(), Some("integer"));
        assert_eq!(n.type_name(1042).as_deref(), Some("character"));
        assert_eq!(n.type_name(1009).as_deref(), Some("text[]"));
        assert_eq!(
            n.type_name(1184).as_deref(),
            Some("timestamp with time zone")
        );
        assert_eq!(n.type_name(99_999), None);
        assert_eq!(n.proc_name(0), None);
    }
}
