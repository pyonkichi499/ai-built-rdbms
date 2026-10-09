//! `CREATE / DROP / ALTER DATABASE` のオプションの検証（M5 DB-2。`m5/09` §6.2）。
//!
//! この段階ではカタログを引かない（ロール・テンプレートの存在などは実行時）。

use crate::catalog::store_db::DatabaseFlagUpdate;
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{
    AlterDatabaseAction, AlterDatabaseStmt, CreateDatabaseStmt, DatabaseOption,
    DatabaseOptionValue as V, DropDatabaseStmt,
};

/// 検証済みの `CREATE DATABASE`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundCreateDatabase {
    pub name: String,
    /// 既定 `template1`。
    pub template: String,
    /// `None` = 実行したロール。
    pub owner: Option<String>,
    pub conn_limit: i32,
    pub is_template: bool,
    pub allow_conn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundDropDatabase {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundAlterDatabase {
    pub name: String,
    pub flags: DatabaseFlagUpdate,
}

const CREATE_OPTIONS: &[&str] = &[
    "template",
    "owner",
    "encoding",
    "lc_collate",
    "lc_ctype",
    "locale",
    "locale_provider",
    "icu_locale",
    "icu_rules",
    "builtin_locale",
    "collation_version",
    "oid",
    "strategy",
    "tablespace",
    "allow_connections",
    "connection_limit",
    "is_template",
];

const ALTER_OPTIONS: &[&str] = &["allow_connections", "connection_limit", "is_template"];

fn syntax(message: String, span: Span) -> Error {
    Error::new(sqlstate::SYNTAX_ERROR, message).with_span(span)
}

/// 重複と未知の名前（PostgreSQL は先にすべてのオプションの名前を調べる）。
fn check_names(opts: &[DatabaseOption], allowed: &[&str]) -> Result<()> {
    let mut seen: Vec<&str> = Vec::new();
    for o in opts {
        if !allowed.contains(&o.name.as_str()) {
            return Err(syntax(
                format!("option \"{}\" not recognized", o.name),
                o.span,
            ));
        }
        if seen.contains(&o.name.as_str()) {
            return Err(syntax(
                "conflicting or redundant options".to_owned(),
                o.span,
            ));
        }
        seen.push(&o.name);
    }
    Ok(())
}

/// `defGetString`。`DEFAULT` は `None`。
fn get_string(o: &DatabaseOption) -> Option<String> {
    match &o.value {
        V::Default => None,
        V::Ident(s) | V::Str(s) | V::Float(s) => Some(s.clone()),
        V::Bool(b) => Some(b.to_string()),
        V::Int(n) => Some(n.to_string()),
    }
}

/// `defGetBoolean`。
fn get_bool(o: &DatabaseOption) -> Result<bool> {
    let parsed = match &o.value {
        V::Bool(b) => Some(*b),
        V::Int(0) => Some(false),
        V::Int(1) => Some(true),
        V::Ident(s) | V::Str(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "on" => Some(true),
            "false" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    };
    parsed.ok_or_else(|| syntax(format!("{} requires a Boolean value", o.name), o.span))
}

/// `defGetInt32`。
fn get_i32(o: &DatabaseOption) -> Result<i32> {
    match &o.value {
        V::Int(n) => i32::try_from(*n).ok(),
        _ => None,
    }
    .ok_or_else(|| syntax(format!("{} requires an integer value", o.name), o.span))
}

fn not_supported(message: String, span: Span) -> Error {
    Error::new(sqlstate::FEATURE_NOT_SUPPORTED, message).with_span(span)
}

fn check_encoding(o: &DatabaseOption) -> Result<()> {
    if let V::Int(n) = &o.value {
        return if *n == 6 {
            Ok(())
        } else {
            Err(not_supported(
                format!("encoding code {n} is not supported"),
                o.span,
            ))
        };
    }
    match get_string(o) {
        None => Ok(()),
        Some(s)
            if matches!(
                s.to_ascii_lowercase().as_str(),
                "utf8" | "utf-8" | "unicode"
            ) =>
        {
            Ok(())
        }
        Some(s) => Err(not_supported(
            format!("encoding \"{s}\" is not supported"),
            o.span,
        )),
    }
}

fn check_locale(o: &DatabaseOption) -> Result<()> {
    let Some(s) = get_string(o) else {
        return Ok(());
    };
    // 引用符なしの識別子は小文字化されているので、そちらは大文字小文字を区別しない。
    let ident = matches!(o.value, V::Ident(_));
    if matches!(s.as_str(), "C" | "POSIX" | "")
        || (ident && matches!(s.to_ascii_uppercase().as_str(), "C" | "POSIX"))
    {
        return Ok(());
    }
    let shown = if o.name == "lc_ctype" {
        "LC_CTYPE"
    } else {
        "LC_COLLATE"
    };
    Err(Error::new(
        sqlstate::WRONG_OBJECT_TYPE,
        format!("invalid {shown} locale name: \"{s}\""),
    )
    .with_span(o.span))
}

fn check_locale_provider(o: &DatabaseOption) -> Result<()> {
    match get_string(o) {
        None => Ok(()),
        Some(s) if s.eq_ignore_ascii_case("libc") => Ok(()),
        Some(s) if s.eq_ignore_ascii_case("icu") || s.eq_ignore_ascii_case("builtin") => {
            Err(not_supported(
                format!(
                    "locale provider \"{}\" is not supported",
                    s.to_ascii_lowercase()
                ),
                o.span,
            ))
        }
        Some(s) => Err(Error::new(
            sqlstate::INVALID_OBJECT_DEFINITION,
            format!("unrecognized locale provider: {s}"),
        )
        .with_span(o.span)),
    }
}

fn check_strategy(o: &DatabaseOption) -> Result<()> {
    match get_string(o) {
        Some(s) if matches!(s.to_ascii_lowercase().as_str(), "wal_log" | "file_copy") => Ok(()),
        other => Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!(
                "invalid create database strategy \"{}\"",
                other.unwrap_or_else(|| "default".to_owned())
            ),
        )
        .with_hint("Valid strategies are \"wal_log\" and \"file_copy\".")
        .with_span(o.span)),
    }
}

fn check_tablespace(o: &DatabaseOption) -> Result<()> {
    match get_string(o) {
        None => Ok(()),
        Some(s) if s == "pg_default" => Ok(()),
        Some(s) => Err(Error::new(
            sqlstate::UNDEFINED_OBJECT,
            format!("tablespace \"{s}\" does not exist"),
        )
        .with_span(o.span)),
    }
}

fn connection_limit(o: &DatabaseOption) -> Result<i32> {
    let n = get_i32(o)?;
    if n < -1 {
        return Err(Error::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("invalid connection limit: {n}"),
        )
        .with_span(o.span));
    }
    Ok(n)
}

pub fn analyze_create(stmt: &CreateDatabaseStmt) -> Result<BoundCreateDatabase> {
    check_names(&stmt.options, CREATE_OPTIONS)?;
    let mut b = BoundCreateDatabase {
        name: stmt.name.clone(),
        template: "template1".to_owned(),
        owner: None,
        conn_limit: -1,
        is_template: false,
        allow_conn: true,
    };
    for o in &stmt.options {
        match o.name.as_str() {
            "template" => b.template = get_string(o).unwrap_or(b.template),
            "owner" => b.owner = get_string(o),
            "encoding" => check_encoding(o)?,
            "lc_collate" | "lc_ctype" | "locale" => check_locale(o)?,
            "locale_provider" => check_locale_provider(o)?,
            "strategy" => check_strategy(o)?,
            "tablespace" => check_tablespace(o)?,
            "allow_connections" => b.allow_conn = get_bool(o)?,
            "connection_limit" => b.conn_limit = connection_limit(o)?,
            "is_template" => b.is_template = get_bool(o)?,
            // icu_locale / icu_rules / builtin_locale / collation_version / oid
            other => {
                return Err(not_supported(
                    format!("option \"{other}\" is not supported"),
                    o.span,
                ));
            }
        }
    }
    Ok(b)
}

pub fn analyze_drop(stmt: &DropDatabaseStmt) -> Result<BoundDropDatabase> {
    if stmt.force {
        return Err(
            Error::not_supported("DROP DATABASE ... WITH (FORCE) is not supported yet")
                .with_span(stmt.span),
        );
    }
    Ok(BoundDropDatabase {
        name: stmt.name.clone(),
        if_exists: stmt.if_exists,
    })
}

pub fn analyze_alter(stmt: &AlterDatabaseStmt) -> Result<BoundAlterDatabase> {
    let opts = match &stmt.action {
        AlterDatabaseAction::Unsupported(what) => {
            return Err(Error::not_supported(format!(
                "ALTER DATABASE ... {what} is not supported yet"
            ))
            .with_span(stmt.span));
        }
        AlterDatabaseAction::Options(o) => o,
    };
    check_names(opts, ALTER_OPTIONS)?;
    let mut flags = DatabaseFlagUpdate::default();
    for o in opts {
        match o.name.as_str() {
            "allow_connections" => flags.allow_conn = Some(get_bool(o)?),
            "is_template" => flags.is_template = Some(get_bool(o)?),
            _ => flags.conn_limit = Some(connection_limit(o)?),
        }
    }
    Ok(BoundAlterDatabase {
        name: stmt.name.clone(),
        flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::Statement;
    use crate::sql::parser::parse;

    fn create(sql: &str) -> Result<BoundCreateDatabase> {
        let Statement::CreateDatabase(s) = parse(sql).unwrap().remove(0) else {
            panic!()
        };
        analyze_create(&s)
    }

    fn code(sql: &str) -> String {
        create(sql).unwrap_err().sqlstate.code().to_owned()
    }

    #[test]
    fn defaults_and_accepted_forms() {
        let b = create("create database x").unwrap();
        assert_eq!(
            b,
            BoundCreateDatabase {
                name: "x".into(),
                template: "template1".into(),
                owner: None,
                conn_limit: -1,
                is_template: false,
                allow_conn: true,
            }
        );
        let b = create(
            "create database x with template = 'template0' owner postgres encoding 'unicode' \
             lc_collate 'C' lc_ctype posix locale '' strategy WAL_LOG tablespace pg_default \
             connection limit 5 allow_connections false is_template on locale_provider libc",
        )
        .unwrap();
        assert_eq!(b.template, "template0");
        assert_eq!(b.owner.as_deref(), Some("postgres"));
        assert_eq!(
            (b.conn_limit, b.allow_conn, b.is_template),
            (5, false, true)
        );
        assert!(create("create database x encoding 6 template default owner default").is_ok());
    }

    #[test]
    fn rejected_values() {
        assert_eq!(code("create database x encoding 'sql_ascii'"), "0A000");
        assert_eq!(code("create database x encoding 7"), "0A000");
        assert_eq!(code("create database x lc_collate 'en_US.UTF-8'"), "42809");
        assert_eq!(code("create database x locale_provider icu"), "0A000");
        assert_eq!(code("create database x locale_provider foo"), "42P17");
        assert_eq!(code("create database x icu_locale 'en'"), "0A000");
        assert_eq!(code("create database x oid 3"), "0A000");
        assert_eq!(code("create database x strategy foo"), "22023");
        assert_eq!(code("create database x tablespace pg_global"), "42704");
        assert_eq!(code("create database x connection limit -5"), "22023");
        assert_eq!(code("create database x connection limit '3'"), "42601");
        assert_eq!(code("create database x connection limit 1.5"), "42601");
        assert_eq!(code("create database x allow_connections 'f'"), "42601");
        assert_eq!(code("create database x is_template foo"), "42601");
        assert_eq!(code("create database x owner a owner b"), "42601");
        assert_eq!(code("create database x foo 1"), "42601");
    }

    #[test]
    fn drop_and_alter() {
        let Statement::DropDatabase(s) = parse("drop database if exists a").unwrap().remove(0)
        else {
            panic!()
        };
        assert_eq!(
            analyze_drop(&s).unwrap(),
            BoundDropDatabase {
                name: "a".into(),
                if_exists: true
            }
        );
        let Statement::DropDatabase(s) = parse("drop database a with (force)").unwrap().remove(0)
        else {
            panic!()
        };
        assert_eq!(analyze_drop(&s).unwrap_err().sqlstate.code(), "0A000");

        let alter = |sql: &str| {
            let Statement::AlterDatabase(s) = parse(sql).unwrap().remove(0) else {
                panic!()
            };
            analyze_alter(&s)
        };
        let b = alter("alter database a connection limit 2 is_template false").unwrap();
        assert_eq!(b.flags.conn_limit, Some(2));
        assert_eq!(b.flags.is_template, Some(false));
        assert_eq!(b.flags.allow_conn, None);
        assert_eq!(
            alter("alter database a encoding 'utf8'")
                .unwrap_err()
                .sqlstate
                .code(),
            "42601"
        );
        assert_eq!(
            alter("alter database a rename to b")
                .unwrap_err()
                .sqlstate
                .code(),
            "0A000"
        );
        assert_eq!(
            alter("alter database a connection limit -2")
                .unwrap_err()
                .sqlstate
                .code(),
            "22023"
        );
    }
}
