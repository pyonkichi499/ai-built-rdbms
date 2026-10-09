//! `FnKind::Runtime` / `FnKind::Env` などカタログ・セッションを見る関数の本体。

#![allow(clippy::wildcard_imports)]

use super::*;

// ----- catalog-aware function bodies ---------------------------------------

/// `CastMethod::Env` 用: `text -> regclass`（1079。`nextval('name'::text)` が通る）。
pub(super) fn cast_text_to_regclass(
    args: &[Datum],
    env: &crate::types::TypeEnv<'_>,
) -> Result<Datum> {
    let Some(Datum::Text(s)) = args.first() else {
        return Err(crate::error::Error::internal(
            "text to regclass expects a text",
        ));
    };
    crate::types::sys::regclass_in(s, env.names).map(Datum::Oid)
}

/// `to_regclass(text) -> regclass`: the relation's OID, or NULL if it does not exist.
pub(super) fn to_regclass(
    args: &[Datum],
    catalog: &dyn CatalogReader,
    _session: &SessionInfo,
) -> Result<Datum> {
    use crate::types::OidNames;
    let Some(Datum::Text(name)) = args.first() else {
        return Err(crate::error::Error::internal(
            "to_regclass expects a text argument",
        ));
    };
    match crate::catalog::names::CatalogNames(catalog).class_oid(name) {
        Ok(oid) => Ok(Datum::Oid(oid)),
        Err(e)
            if e.sqlstate == crate::error::sqlstate::UNDEFINED_TABLE
                || e.sqlstate == crate::error::sqlstate::INVALID_NAME =>
        {
            Ok(Datum::Null)
        }
        Err(e) => Err(e),
    }
}

/// `to_regtype(text) -> regtype`: the type's OID, or NULL if it does not exist (or the name is malformed).
pub(super) fn to_regtype(
    args: &[Datum],
    catalog: &dyn CatalogReader,
    _session: &SessionInfo,
) -> Result<Datum> {
    use crate::error::sqlstate;
    use crate::types::OidNames;
    let Some(Datum::Text(name)) = args.first() else {
        return Err(crate::error::Error::internal(
            "to_regtype expects a text argument",
        ));
    };
    // 空文字列は NULL。構文エラー（`a%`、`a b` など）は PG と同じく 42601 で返す。
    if name.trim().is_empty() {
        return Ok(Datum::Null);
    }
    match crate::catalog::names::CatalogNames(catalog).type_oid(name) {
        Ok(oid) => Ok(Datum::Oid(oid)),
        Err(e)
            if e.sqlstate == sqlstate::UNDEFINED_OBJECT || e.sqlstate == sqlstate::INVALID_NAME =>
        {
            Ok(Datum::Null)
        }
        Err(e) => Err(e),
    }
}
/// `pg_get_userbyid(oid) -> name`: the role name, or `unknown (OID=n)`.
pub(super) fn pg_get_userbyid(
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
pub(super) fn pg_backend_pid(_args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
    Ok(Datum::Int4(runtime.backend_pid()))
}

/// `statement_timestamp() -> timestamptz`: the start of the running statement (the wall clock
/// when the session does not track it).
pub(super) fn statement_timestamp(args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
    match runtime.statement_timestamp() {
        Some(us) => Ok(Datum::TimestampTz(yuzhu_datetime::TimestampTz(us))),
        None => clock_timestamp(args, runtime),
    }
}

/// `clock_timestamp()` / `statement_timestamp() -> timestamptz`: the wall-clock time at the call
/// (statement start is not tracked separately; `now()` / `transaction_timestamp()` are session
/// values and never reach here).
#[allow(clippy::unnecessary_wraps)]
pub(super) fn clock_timestamp(_args: &[Datum], _runtime: &dyn RuntimeInfo) -> Result<Datum> {
    const UNIX_TO_PG_SECS: i64 = 946_684_800;
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_micros()).ok())
        .map_or(0, |us| us - UNIX_TO_PG_SECS * 1_000_000);
    Ok(Datum::TimestampTz(yuzhu_datetime::TimestampTz(micros)))
}

/// Placeholder for `now()` / `transaction_timestamp()`: the analyzer turns them into
/// `CURRENT_TIMESTAMP`, so this is never called.
pub(super) fn now_unreachable(_args: &[Datum]) -> Result<Datum> {
    Err(crate::error::Error::internal(
        "now() is rewritten to a session value by the analyzer",
    ))
}

/// Placeholder for `date(timestamptz)` and friends: the analyzer rewrites them into the
/// explicit cast with the same OID, so this is never called.
pub(super) fn cast_rewritten_unreachable(_args: &[Datum]) -> Result<Datum> {
    Err(crate::error::Error::internal(
        "the function form of a timezone-dependent cast is rewritten to a cast by the analyzer",
    ))
}

pub(super) fn text_arg<'a>(args: &'a [Datum], i: usize, func: &str) -> Result<&'a str> {
    match args.get(i) {
        Some(Datum::Text(s)) => Ok(s),
        _ => Err(crate::error::Error::internal(format!(
            "{func} expects a text argument"
        ))),
    }
}

pub(super) fn unrecognized_parameter(name: &str) -> crate::error::Error {
    crate::error::Error::new(
        crate::error::sqlstate::UNDEFINED_OBJECT,
        format!("unrecognized configuration parameter \"{name}\""),
    )
}

/// `current_setting(text) -> text`.
pub(super) fn current_setting(args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
    let name = text_arg(args, 0, "current_setting")?;
    match runtime.get_setting(name)? {
        Some(v) => Ok(Datum::Text(v)),
        None => Err(unrecognized_parameter(name)),
    }
}

/// `current_setting(text, bool) -> text`: NULL for an unknown parameter when
/// `missing_ok` is true.
pub(super) fn current_setting_missing_ok(
    args: &[Datum],
    runtime: &dyn RuntimeInfo,
) -> Result<Datum> {
    let name = text_arg(args, 0, "current_setting")?;
    match (runtime.get_setting(name)?, args.get(1)) {
        (Some(v), _) => Ok(Datum::Text(v)),
        (None, Some(Datum::Bool(true))) => Ok(Datum::Null),
        (None, _) => Err(unrecognized_parameter(name)),
    }
}

/// `set_config(text, text, bool) -> text` (not strict: a NULL value resets
/// the parameter, a NULL name is an error, a NULL `is_local` means false).
pub(super) fn set_config(args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
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
pub(super) fn txid_current_if_assigned(
    _args: &[Datum],
    runtime: &dyn RuntimeInfo,
) -> Result<Datum> {
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
pub(super) fn pg_sleep(args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
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

/// `pg_isolation_test_session_is_blocked(int4, int4[]) -> bool`。意味は M5 契約 CR-1
/// （`RuntimeInfo::is_blocked_by` が pid を `BackendId` に直して `LockManager::is_blocked_by` に委ねる）。
/// 配列に NULL があれば `XX000 array must not contain nulls`（PostgreSQL と同じ）。
pub(super) fn pg_isolation_test_session_is_blocked(
    args: &[Datum],
    runtime: &dyn RuntimeInfo,
) -> Result<Datum> {
    let (Some(Datum::Int4(pid)), Some(array)) = (args.first(), args.get(1)) else {
        return Err(crate::error::Error::internal(
            "pg_isolation_test_session_is_blocked expects (int4, int4[])",
        ));
    };
    let among = int4_array_without_nulls(array)?;
    Ok(Datum::Bool(runtime.is_blocked_by(*pid, &among)))
}

/// `pg_blocking_pids(int4) -> int4[]`: the pids of the backends blocking `pid` (empty when it is
/// not waiting or unknown).
pub(super) fn pg_blocking_pids(args: &[Datum], runtime: &dyn RuntimeInfo) -> Result<Datum> {
    let Some(Datum::Int4(pid)) = args.first() else {
        return Err(crate::error::Error::internal(
            "pg_blocking_pids expects an int4 argument",
        ));
    };
    Ok(Datum::Int4Array(
        runtime.blocking_pids(*pid).into_iter().map(Some).collect(),
    ))
}

/// The elements of an `int4[]` argument; a NULL element is `XX000 array must not contain nulls`.
fn int4_array_without_nulls(array: &Datum) -> Result<Vec<i32>> {
    let nulls = || crate::error::Error::internal("array must not contain nulls");
    match array {
        Datum::Int4Array(items) => items.iter().map(|v| v.ok_or_else(nulls)).collect(),
        Datum::Array(a) => a
            .items
            .iter()
            .map(|d| match d {
                Datum::Int4(v) => Ok(*v),
                _ => Err(nulls()),
            })
            .collect(),
        _ => Err(crate::error::Error::internal("expected an int4[] argument")),
    }
}

/// `pg_table_is_visible(oid) -> bool`: NULL when the relation does not
/// exist; otherwise whether an unqualified reference to its name finds this
/// relation (`pg_catalog` first, then the `search_path`). Tables, sequences
/// and indexes all count (`\di` asks about indexes; `m4/07` §4.2).
pub(super) fn pg_table_is_visible(
    args: &[Datum],
    catalog: &dyn CatalogReader,
    _session: &SessionInfo,
) -> Result<Datum> {
    let Some(Datum::Oid(rel)) = args.first() else {
        return Err(crate::error::Error::internal(
            "pg_table_is_visible expects an oid argument",
        ));
    };
    let name = match (catalog.table_by_oid(*rel)?, catalog.index_by_oid(*rel)?) {
        (Some(t), _) => t.name.clone(),
        (None, Some(i)) => i.name.clone(),
        (None, None) => return Ok(Datum::Null),
    };
    let found = catalog.relation_kind(None, &name)?;
    Ok(Datum::Bool(found.is_some_and(|(oid, _)| oid == *rel)))
}

/// `format_type(oid, int4) -> text`. Not strict: a NULL type gives NULL, a
/// NULL typmod means "no modifier".
pub(super) fn format_type(args: &[Datum]) -> Result<Datum> {
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
    let given = typmod.is_some();
    let m = typmod.filter(|m| *m >= 0);
    let name = match type_oid {
        oid::BOOL => "boolean".to_owned(),
        oid::INT8 => "bigint".to_owned(),
        oid::INT2 => "smallint".to_owned(),
        oid::INT4 => "integer".to_owned(),
        oid::FLOAT4 => "real".to_owned(),
        oid::FLOAT8 => "double precision".to_owned(),
        oid::VARCHAR => "character varying".to_owned(),
        // `bpchar` with a typmod of -1 is not the same as `character` (PostgreSQL prints `bpchar`).
        oid::BPCHAR if given && m.is_none() => "bpchar".to_owned(),
        oid::BPCHAR => "character".to_owned(),
        oid::TIMESTAMP | oid::TIMESTAMPTZ => "timestamp".to_owned(),
        oid::TIME | oid::TIMETZ => "time".to_owned(),
        _ => quote_identifier(t.name),
    };
    // The text after the (optional) modifier: `timestamp(3) without time zone`.
    let suffix = match type_oid {
        oid::TIMESTAMP | oid::TIME => " without time zone",
        oid::TIMESTAMPTZ | oid::TIMETZ => " with time zone",
        _ => "",
    };
    let modifier = match (type_oid, m) {
        (_, None) => String::new(),
        (oid::VARCHAR | oid::BPCHAR, Some(m)) if m > crate::types::VARHDRSZ => {
            format!("({})", m - crate::types::VARHDRSZ)
        }
        (NUMERIC, Some(m)) => yuzhu_numeric::format_typmod(m),
        // The special-cased names never print a modifier.
        (oid::BOOL | oid::INT2 | oid::INT4 | oid::INT8 | oid::FLOAT4 | oid::FLOAT8, _) => {
            String::new()
        }
        // varchar / char with a length of 0 or less print nothing; any other type is shown with the raw modifier
        // (PostgreSQL falls back to `(typmod)` for a type without `typmodout`).
        (oid::VARCHAR | oid::BPCHAR, _) => String::new(),
        (_, Some(m)) => format!("({m})"),
    };
    format!("{name}{modifier}{suffix}")
}

/// `quote_identifier` for type names: double quotes unless the name is made
/// of lower-case letters, digits and underscores (and does not start with a
/// digit) and is not a reserved word that can be a type name (`char`).
pub(super) fn quote_identifier(name: &str) -> String {
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

pub(super) fn pg_function_is_visible(
    args: &[Datum],
    _catalog: &dyn CatalogReader,
    _session: &SessionInfo,
) -> Result<Datum> {
    // 組み込み関数はすべて `pg_catalog` にあり、検索パスの先頭なので見える。
    let Some(Datum::Oid(f)) = args.first() else {
        return Err(crate::error::Error::internal(
            "pg_function_is_visible expects an oid argument",
        ));
    };
    Ok(proc_by_oid(*f).map_or(Datum::Null, |_| Datum::Bool(true)))
}

/// 保存式の出力に使う `TypeEnv`（ISO・UTC。セッションの DateStyle・TimeZone は渡らない）で `f` を呼ぶ。
pub(super) fn with_stored_type_env<R>(f: impl FnOnce(&crate::types::TypeEnv<'_>) -> R) -> R {
    let tz = yuzhu_datetime::TimeZone::utc();
    let zones = yuzhu_datetime::ZoneDb::without_tzdata();
    let env = crate::types::TypeEnv {
        datetime: Some(yuzhu_datetime::DateTimeEnv::new(&tz, &zones)),
        ..crate::types::TypeEnv::default()
    };
    f(&env)
}

pub(super) fn text_or_null(v: Option<String>) -> Datum {
    v.map_or(Datum::Null, Datum::Text)
}

/// `pg_get_expr(pg_node_tree, oid, bool) -> text`。2 引数版（`pretty = false`）にも使える。
pub(super) fn pg_get_expr_pretty(
    args: &[Datum],
    catalog: &dyn CatalogReader,
    _session: &SessionInfo,
) -> Result<Datum> {
    let (Some(Datum::Text(src)), Some(Datum::Oid(relid))) = (args.first(), args.get(1)) else {
        return Err(crate::error::Error::internal(
            "pg_get_expr expects (pg_node_tree, oid)",
        ));
    };
    let pretty = matches!(args.get(2), Some(Datum::Bool(true)));
    with_stored_type_env(|env| {
        crate::deparse::stored::pg_get_expr(src, *relid, pretty, catalog, env).map(text_or_null)
    })
}

/// `pg_get_constraintdef(oid [, bool]) -> text`。
pub(super) fn pg_get_constraintdef(
    args: &[Datum],
    catalog: &dyn CatalogReader,
    _session: &SessionInfo,
) -> Result<Datum> {
    let Some(Datum::Oid(oid)) = args.first() else {
        return Err(crate::error::Error::internal(
            "pg_get_constraintdef expects an oid",
        ));
    };
    let pretty = matches!(args.get(1), Some(Datum::Bool(true)));
    with_stored_type_env(|env| {
        crate::deparse::stored::pg_get_constraintdef(*oid, pretty, catalog, env).map(text_or_null)
    })
}

/// `pg_get_indexdef(oid [, int4, bool]) -> text`。
pub(super) fn pg_get_indexdef(
    args: &[Datum],
    catalog: &dyn CatalogReader,
    _session: &SessionInfo,
) -> Result<Datum> {
    let Some(Datum::Oid(oid)) = args.first() else {
        return Err(crate::error::Error::internal(
            "pg_get_indexdef expects an oid",
        ));
    };
    let column = args
        .get(1)
        .and_then(Datum::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(0);
    let pretty = matches!(args.get(2), Some(Datum::Bool(true)));
    crate::deparse::stored::pg_get_indexdef(*oid, column, pretty, catalog).map(text_or_null)
}
// ===== region: misc (C1) =====
