//! ロールの DDL 実行: CREATE / ALTER（M5 AU-3。`m5/08` §5.7・§5.8）。
//!
//! DROP ROLE は AU-3b が足す。

use super::DdlCtx;
use crate::analyzer::ddl_ext::role::{
    BoundAlterRole, BoundAlterRoleAction, BoundCreateRole, BoundDropRole, RoleChanges, RoleTarget,
};
use crate::catalog::roles::{
    AuthidRow, current_role, has_bypassrls, has_createdb, has_createrole, has_replication,
    latest_snapshot, role_name_lock, role_oid_lock,
};
use crate::catalog::rows::column_index;
use crate::catalog::schema::oids;
use crate::catalog::store::SharedCatalogStore;
use crate::control::FIRST_NORMAL_OID;
use crate::engine::Cluster;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::sql::ast::Secret;
use crate::txn::Snapshot;
use crate::txn::lock::{LockMode, LockScope};
use crate::types::{Datum, SqlType};
use crate::types::{Oid, Row};
use yuzhu_auth::scram::{is_scram_secret, make_secret, verify_plain_password};

/// `password_encryption`。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PasswordEncryption {
    #[default]
    ScramSha256,
    /// 受け付けるのは設定値としてだけ。パスワードの設定は 0A000 になる。
    Md5,
}

/// パスワードの保存形式の方針。session が `Settings` から文ごとに作る（`DdlCtx.password`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PasswordPolicy {
    pub encryption: PasswordEncryption,
    pub scram_iterations: u32,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        PasswordPolicy {
            encryption: PasswordEncryption::ScramSha256,
            scram_iterations: 4096,
        }
    }
}

/// `encrypt_password` の結果。
#[derive(Debug, PartialEq, Eq)]
pub struct EncryptedPassword {
    pub secret: Option<String>,
    /// 空のパスワードだったので消した（NOTICE を出す）。
    pub cleared_empty: bool,
}

fn md5_unsupported() -> Error {
    Error::not_supported("MD5 password encryption is not supported")
}

fn is_md5_hash(p: &str) -> bool {
    p.len() == 35
        && p.starts_with("md5")
        && p[3..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn auth_error(e: yuzhu_auth::AuthError) -> Error {
    match e {
        yuzhu_auth::AuthError::Random(_) => Error::new(
            sqlstate::INTERNAL_ERROR,
            "could not generate random numbers",
        ),
        yuzhu_auth::AuthError::InvalidParameter(m) => {
            Error::new(sqlstate::INVALID_PARAMETER_VALUE, m)
        }
        other => Error::internal(other.to_string()),
    }
}

/// パスワードの保存形式への変換（§5.7.1）。PBKDF2 が走るので、ロックの外で呼ぶ。
pub fn encrypt_password(
    policy: &PasswordPolicy,
    password: Option<&Secret>,
) -> Result<EncryptedPassword> {
    let Some(Secret(p)) = password else {
        return Ok(EncryptedPassword {
            secret: None,
            cleared_empty: false,
        });
    };
    if p.is_empty() || (is_scram_secret(p) && verify_plain_password("", p)) {
        return Ok(EncryptedPassword {
            secret: None,
            cleared_empty: true,
        });
    }
    if is_md5_hash(p) {
        return Err(md5_unsupported());
    }
    let secret = if is_scram_secret(p) {
        p.clone()
    } else if policy.encryption == PasswordEncryption::Md5 {
        return Err(md5_unsupported());
    } else {
        make_secret(p, policy.scram_iterations).map_err(auth_error)?
    };
    Ok(EncryptedPassword {
        secret: Some(secret),
        cleared_empty: false,
    })
}

fn notice_cleared(ctx: &mut DdlCtx<'_>) {
    ctx.notice(
        Severity::Notice,
        sqlstate::SUCCESSFUL_COMPLETION,
        "empty string is not a valid password, clearing password".to_owned(),
        None,
    );
}

/// `VALID UNTIL` の解釈。`infinity` は `i64::MAX`。
fn parse_valid_until(env: &crate::types::TypeEnv<'_>, s: &str) -> Result<i64> {
    match crate::types::io::input_text_env(s, SqlType::TIMESTAMPTZ, env)? {
        Datum::TimestampTz(t) => Ok(t.0),
        other => Err(Error::internal(format!(
            "timestamptz input returned {other:?}"
        ))),
    }
}

fn denied(action: &str, detail: String) -> Error {
    Error::new(
        sqlstate::INSUFFICIENT_PRIVILEGE,
        format!("permission denied to {action} role"),
    )
    .with_detail(detail)
}

fn lacks_attr(action: &str, attr: &str) -> Error {
    denied(
        action,
        format!(
            "Only roles with the {attr} attribute may {action} roles with the {attr} attribute."
        ),
    )
}

fn reserved_pg(name: &str) -> Error {
    Error::new(
        sqlstate::RESERVED_NAME,
        format!("role name \"{name}\" is reserved"),
    )
    .with_detail("Role names starting with \"pg_\" are reserved.")
}

fn lock_name(ctx: &DdlCtx<'_>, name: &str) -> Result<()> {
    ctx.cluster.locks().acquire(
        ctx.backend,
        role_name_lock(name),
        LockMode::Exclusive,
        LockScope::Transaction,
        ctx.wait,
    )
}

/// `CREATE ROLE`（PostgreSQL 17 の `CreateRole` の順序）。
pub fn create_role(ctx: &mut DdlCtx<'_>, b: &BoundCreateRole) -> Result<String> {
    // 1. 権限（最新の行）。
    let me = current_role(ctx)?;
    if !has_createrole(&me) {
        return Err(denied(
            "create",
            "Only roles with the CREATEROLE attribute may create roles.".to_owned(),
        ));
    }
    if b.superuser && !me.superuser {
        return Err(lacks_attr_create("SUPERUSER"));
    }
    if b.create_db && !has_createdb(&me) {
        return Err(lacks_attr_create("CREATEDB"));
    }
    if b.replication && !has_replication(&me) {
        return Err(lacks_attr_create("REPLICATION"));
    }
    if b.bypass_rls && !has_bypassrls(&me) {
        return Err(lacks_attr_create("BYPASSRLS"));
    }
    // 2. 予約名。
    if b.name.starts_with("pg_") {
        return Err(reserved_pg(&b.name));
    }
    // 3・4. パスワードと VALID UNTIL（ロックの外）。
    let enc = encrypt_password(&ctx.password, b.password.as_ref())?;
    if enc.cleared_empty {
        notice_cleared(ctx);
    }
    let valid_until = b
        .valid_until
        .as_deref()
        .map(|s| parse_valid_until(ctx.type_env, s))
        .transpose()?;
    // 5. 名前ロック（待つ）。
    lock_name(ctx, &b.name)?;
    // 6. 待った後なので、スナップショットを取り直して重複を検査する。
    let shared = &ctx.db.shared;
    {
        let snap = latest_snapshot(ctx);
        if shared.authid_by_name(&snap, &b.name)?.is_some() {
            return Err(Error::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("role \"{}\" already exists", b.name),
            ));
        }
    }
    // 7・8. OID を採って挿入する。
    let oid = shared.new_role_oid(ctx.cluster.oid_allocator())?;
    let row = AuthidRow {
        oid,
        name: b.name.clone(),
        superuser: b.superuser,
        inherit: b.inherit,
        create_role: b.create_role,
        create_db: b.create_db,
        can_login: b.can_login,
        replication: b.replication,
        bypass_rls: b.bypass_rls,
        conn_limit: b.conn_limit,
        password: enc.secret,
        valid_until,
    };
    let w = ctx.write_ctx()?;
    shared.insert_authid(&w, &row)?;
    ctx.mark_catalog_dirty();
    Ok("CREATE ROLE".to_owned())
}

fn lacks_attr_create(attr: &str) -> Error {
    lacks_attr("create", attr)
}

fn only_password(c: &RoleChanges) -> bool {
    c.password.is_some()
        && c.superuser.is_none()
        && c.inherit.is_none()
        && c.create_role.is_none()
        && c.create_db.is_none()
        && c.can_login.is_none()
        && c.replication.is_none()
        && c.bypass_rls.is_none()
        && c.conn_limit.is_none()
        && c.valid_until.is_none()
}

/// `ALTER ROLE`。
pub fn alter_role(ctx: &mut DdlCtx<'_>, b: &BoundAlterRole) -> Result<String> {
    // 1・2. パスワードと VALID UNTIL（ロックの外）。
    let (pw, valid_until) = match &b.action {
        BoundAlterRoleAction::Attrs(c) => {
            let pw = match &c.password {
                None => None,
                Some(None) => Some(None),
                Some(Some(s)) => {
                    let enc = encrypt_password(&ctx.password, Some(s))?;
                    if enc.cleared_empty {
                        notice_cleared(ctx);
                    }
                    Some(enc.secret)
                }
            };
            let vu = c
                .valid_until
                .as_deref()
                .map(|s| parse_valid_until(ctx.type_env, s))
                .transpose()?;
            (pw, vu)
        }
        BoundAlterRoleAction::Rename(_) => (None, None),
    };
    // 対象の名前（CURRENT_USER は OID から引く）。
    let target_name = match &b.target {
        RoleTarget::Name(n) => n.clone(),
        RoleTarget::CurrentUser => {
            let snap = latest_snapshot(ctx);
            match ctx.db.shared.authid_by_oid(&snap, ctx.role.oid)? {
                Some((_, r)) => r.name,
                None => ctx.role.name.clone(),
            }
        }
    };
    // 3. 名前ロック（名前のハッシュの昇順で取る）。
    let mut names = vec![target_name.clone()];
    if let BoundAlterRoleAction::Rename(new) = &b.action {
        names.push(new.clone());
    }
    names.sort_by_key(|n| crate::catalog::roles::fnv1a64(n.as_bytes()));
    names.dedup();
    for n in &names {
        lock_name(ctx, n)?;
    }
    // 4. 最新のスナップショットで対象を引く。
    let shared = &ctx.db.shared;
    let snap = latest_snapshot(ctx);
    let found = match &b.target {
        RoleTarget::Name(n) => shared.authid_by_name(&snap, n)?,
        RoleTarget::CurrentUser => shared.authid_by_oid(&snap, ctx.role.oid)?,
    };
    let Some((tid, target)) = found else {
        return Err(Error::new(
            sqlstate::UNDEFINED_OBJECT,
            format!("role \"{target_name}\" does not exist"),
        ));
    };
    let me = current_role(ctx)?;
    // 5・6. 権限と新しい行。
    let new = match &b.action {
        BoundAlterRoleAction::Attrs(c) => {
            check_alter_privileges(&me, &target, c)?;
            AuthidRow {
                superuser: c.superuser.unwrap_or(target.superuser),
                inherit: c.inherit.unwrap_or(target.inherit),
                create_role: c.create_role.unwrap_or(target.create_role),
                create_db: c.create_db.unwrap_or(target.create_db),
                can_login: c.can_login.unwrap_or(target.can_login),
                replication: c.replication.unwrap_or(target.replication),
                bypass_rls: c.bypass_rls.unwrap_or(target.bypass_rls),
                conn_limit: c.conn_limit.unwrap_or(target.conn_limit),
                password: pw.unwrap_or_else(|| target.password.clone()),
                valid_until: valid_until.or(target.valid_until),
                ..target.clone()
            }
        }
        BoundAlterRoleAction::Rename(new_name) => {
            if target.oid == ctx.role.oid {
                return Err(Error::not_supported("session user cannot be renamed"));
            }
            if new_name.starts_with("pg_") {
                return Err(reserved_pg(new_name));
            }
            if shared.authid_by_name(&snap, new_name)?.is_some() {
                return Err(Error::new(
                    sqlstate::DUPLICATE_OBJECT,
                    format!("role \"{new_name}\" already exists"),
                ));
            }
            if target.superuser {
                if !me.superuser {
                    return Err(denied(
                        "rename",
                        "Only roles with the SUPERUSER attribute may rename superusers.".to_owned(),
                    ));
                }
            } else if !has_createrole(&me) {
                return Err(denied(
                    "rename",
                    format!(
                        "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may rename this role.",
                        target.name
                    ),
                ));
            }
            AuthidRow {
                name: new_name.clone(),
                ..target.clone()
            }
        }
    };
    // 7. 更新。
    let w = ctx.write_ctx()?;
    ctx.db.shared.update_authid(&w, &snap, tid, &new)?;
    ctx.mark_catalog_dirty();
    Ok("ALTER ROLE".to_owned())
}

/// §5.8 手順 5。`me` は現在のロールの最新の行。
fn check_alter_privileges(me: &AuthidRow, target: &AuthidRow, c: &RoleChanges) -> Result<()> {
    // f. ブートストラップ超ユーザーから SUPERUSER は外せない。
    if target.oid == crate::catalog::schema::oids::BOOTSTRAP_SUPERUSER && c.superuser == Some(false)
    {
        return Err(Error::not_supported("permission denied to alter role")
            .with_detail("The bootstrap superuser must have the SUPERUSER attribute."));
    }
    if me.superuser {
        return Ok(());
    }
    if target.superuser || c.superuser.is_some() {
        return Err(lacks_attr("alter", "SUPERUSER"));
    }
    if (target.replication || c.replication.is_some()) && !has_replication(me) {
        return Err(lacks_attr("alter", "REPLICATION"));
    }
    if c.bypass_rls == Some(true) && !has_bypassrls(me) {
        return Err(lacks_attr("alter", "BYPASSRLS"));
    }
    if c.create_db == Some(true) && !has_createdb(me) {
        return Err(lacks_attr("alter", "CREATEDB"));
    }
    if !has_createrole(me) && !(target.oid == me.oid && only_password(c)) {
        return Err(denied(
            "alter",
            format!(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may alter this role.",
                target.name
            ),
        ));
    }
    Ok(())
}

// ----- DROP ROLE（AU-3b。`m5/08` §5.9）---------------------------------------------------

/// 所有物の種類（`DETAIL` の並び順にも使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OwnedKind {
    Database,
    Table,
    Index,
    Sequence,
    View,
    Schema,
    Function,
    Type,
}

impl OwnedKind {
    fn label(self) -> &'static str {
        match self {
            OwnedKind::Database => "database",
            OwnedKind::Table => "table",
            OwnedKind::Index => "index",
            OwnedKind::Sequence => "sequence",
            OwnedKind::View => "view",
            OwnedKind::Schema => "schema",
            OwnedKind::Function => "function",
            OwnedKind::Type => "type",
        }
    }

    fn from_relkind(c: u8) -> OwnedKind {
        match c {
            b'i' | b'I' => OwnedKind::Index,
            b'S' => OwnedKind::Sequence,
            b'v' => OwnedKind::View,
            _ => OwnedKind::Table,
        }
    }
}

/// `owned_objects` の結果の 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedObject {
    pub db_oid: Oid,
    pub db_name: String,
    pub kind: OwnedKind,
    pub name: String,
}

fn bad_value(catalog: &str, column: &str, d: &Datum) -> Error {
    Error::internal(format!("{catalog}.{column} has an unexpected value {d:?}"))
}

fn oid_col(r: &Row, catalog: Oid, cat_name: &str, col: &str) -> Result<Oid> {
    match &r[column_index(catalog, col)] {
        Datum::Oid(v) => Ok(*v),
        other => Err(bad_value(cat_name, col, other)),
    }
}

fn name_col(r: &Row, catalog: Oid, cat_name: &str, col: &str) -> Result<String> {
    match &r[column_index(catalog, col)] {
        Datum::Text(s) => Ok(s.clone()),
        other => Err(bad_value(cat_name, col, other)),
    }
}

/// `role` が持つ物を全データベースから集める（`pg_shdepend` の代わり。AU-D14）。
/// 結果は データベースの OID、種類、名前の順。
pub fn owned_objects(
    cluster: &Cluster,
    shared: &SharedCatalogStore,
    snap: &Snapshot,
    role: Oid,
) -> Result<Vec<OwnedObject>> {
    let mut out = Vec::new();
    let mut dbs = Vec::new();
    {
        let (_, rel) = shared
            .rel(oids::PG_DATABASE)
            .ok_or_else(|| Error::internal("pg_database is not a shared catalog"))?;
        let storage = shared.storage();
        let mut scan = storage.begin_scan(rel, snap)?;
        while let Some(t) = storage.scan_next(&mut scan)? {
            let r = &t.row;
            let oid = oid_col(r, oids::PG_DATABASE, "pg_database", "oid")?;
            let name = name_col(r, oids::PG_DATABASE, "pg_database", "datname")?;
            let dba = oid_col(r, oids::PG_DATABASE, "pg_database", "datdba")?;
            dbs.push((oid, name, dba == role));
        }
    }
    dbs.sort_by_key(|d| d.0);
    for (db_oid, db_name, is_owner) in dbs {
        let mut found = Vec::new();
        if is_owner {
            found.push((OwnedKind::Database, db_name.clone()));
        }
        let handle = cluster.database_handle(db_oid)?;
        let cat = &handle.catalog;
        let owned_by = |catalog: Oid, col: &'static str| {
            let idx = column_index(catalog, col);
            move |r: &Row| Ok(matches!(&r[idx], Datum::Oid(o) if *o == role))
        };
        for t in cat.scan(snap, oids::PG_CLASS, owned_by(oids::PG_CLASS, "relowner"))? {
            let kind = match &t.row[column_index(oids::PG_CLASS, "relkind")] {
                Datum::Char(c) => OwnedKind::from_relkind(*c),
                other => return Err(bad_value("pg_class", "relkind", other)),
            };
            found.push((
                kind,
                name_col(&t.row, oids::PG_CLASS, "pg_class", "relname")?,
            ));
        }
        let others = [
            (oids::PG_NAMESPACE, "nspowner", "nspname", OwnedKind::Schema),
            (oids::PG_PROC, "proowner", "proname", OwnedKind::Function),
            (oids::PG_TYPE, "typowner", "typname", OwnedKind::Type),
        ];
        for (catalog, owner_col, name_column, kind) in others {
            let cat_name =
                crate::catalog::schema::catalog_def(catalog).map_or("catalog", |d| d.name);
            for t in cat.scan(snap, catalog, owned_by(catalog, owner_col))? {
                found.push((kind, name_col(&t.row, catalog, cat_name, name_column)?));
            }
        }
        found.sort();
        out.extend(found.into_iter().map(|(kind, name)| OwnedObject {
            db_oid,
            db_name: db_name.clone(),
            kind,
            name,
        }));
    }
    Ok(out)
}

/// `DETAIL` の最大行数。これを超えた分は "and N other objects"。
const MAX_DETAIL_LINES: usize = 100;

fn plural(n: usize) -> &'static str {
    if n == 1 { "object" } else { "objects" }
}

/// 2BP01 の `DETAIL`（現在のデータベースの物は 1 行ずつ、データベースの所有は `owner of database`、
/// 他のデータベースの物はデータベースごとの件数）。
fn dependency_detail(objs: &[OwnedObject], current_db: Oid) -> String {
    let mut lines: Vec<String> = Vec::new();
    for o in objs
        .iter()
        .filter(|o| o.db_oid == current_db && o.kind != OwnedKind::Database)
    {
        lines.push(format!("owner of {} {}", o.kind.label(), o.name));
    }
    for o in objs.iter().filter(|o| o.kind == OwnedKind::Database) {
        lines.push(format!("owner of database {}", o.name));
    }
    let mut remote: Vec<(Oid, &str, usize)> = Vec::new();
    for o in objs
        .iter()
        .filter(|o| o.db_oid != current_db && o.kind != OwnedKind::Database)
    {
        match remote.last_mut() {
            Some(last) if last.0 == o.db_oid => last.2 += 1,
            _ => remote.push((o.db_oid, &o.db_name, 1)),
        }
    }
    for (_, db, n) in remote {
        lines.push(format!("{n} {} in database {db}", plural(n)));
    }
    if lines.len() > MAX_DETAIL_LINES {
        let extra = lines.len() - MAX_DETAIL_LINES;
        lines.truncate(MAX_DETAIL_LINES);
        lines.push(format!("and {extra} other {}", plural(extra)));
    }
    lines.join("\n")
}

/// `DROP ROLE`。
pub fn drop_role(ctx: &mut DdlCtx<'_>, b: &BoundDropRole) -> Result<String> {
    for target in &b.names {
        let name = match target {
            RoleTarget::Name(n) => n.clone(),
            RoleTarget::CurrentUser => return Err(current_user_error()),
        };
        drop_one(ctx, &name, b.if_exists)?;
        ctx.txn.command_counter_increment()?;
    }
    Ok("DROP ROLE".to_owned())
}

fn current_user_error() -> Error {
    Error::new(sqlstate::OBJECT_IN_USE, "current user cannot be dropped")
}

fn missing(ctx: &mut DdlCtx<'_>, name: &str, if_exists: bool) -> Result<()> {
    if if_exists {
        ctx.notice(
            Severity::Notice,
            sqlstate::SUCCESSFUL_COMPLETION,
            format!("role \"{name}\" does not exist, skipping"),
            None,
        );
        return Ok(());
    }
    Err(Error::new(
        sqlstate::UNDEFINED_OBJECT,
        format!("role \"{name}\" does not exist"),
    ))
}

fn drop_one(ctx: &mut DdlCtx<'_>, name: &str, if_exists: bool) -> Result<()> {
    // 1. 名前ロック。
    lock_name(ctx, name)?;
    // 2. 最新の行。
    let snap = latest_snapshot(ctx);
    let Some((_, row)) = ctx.db.shared.authid_by_name(&snap, name)? else {
        return missing(ctx, name, if_exists);
    };
    // 3. 自分自身。
    if row.oid == ctx.role.oid {
        return Err(current_user_error());
    }
    // 4. 権限。
    let me = current_role(ctx)?;
    if row.superuser && !me.superuser {
        return Err(denied(
            "drop",
            "Only roles with the SUPERUSER attribute may drop roles with the SUPERUSER attribute."
                .to_owned(),
        ));
    }
    if !has_createrole(&me) {
        return Err(denied(
            "drop",
            format!(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{name}\" may drop this role."
            ),
        ));
    }
    // 5. システムのロール。
    if row.oid < FIRST_NORMAL_OID {
        return Err(Error::new(
            sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
            format!("cannot drop role {name} because it is required by the database system"),
        ));
    }
    // 6. OID ロック。待っている間に消えていたら「存在しない」。
    ctx.cluster.locks().acquire(
        ctx.backend,
        role_oid_lock(row.oid),
        LockMode::AccessExclusive,
        LockScope::Transaction,
        ctx.wait,
    )?;
    let snap = latest_snapshot(ctx);
    let Some((tid, row)) = ctx.db.shared.authid_by_oid(&snap, row.oid)? else {
        return missing(ctx, name, if_exists);
    };
    // 7. 所有物。
    let objs = owned_objects(ctx.cluster, &ctx.db.shared, &snap, row.oid)?;
    if !objs.is_empty() {
        return Err(Error::new(
            sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
            format!("role \"{name}\" cannot be dropped because some objects depend on it"),
        )
        .with_detail(dependency_detail(&objs, ctx.db.oid)));
    }
    // 8. 削除。
    let w = ctx.write_ctx()?;
    ctx.db.shared.delete_authid(&w, &snap, tid)?;
    ctx.mark_catalog_dirty();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::query::BoundDdl;
    use crate::ddl::testkit::Harness;
    use crate::sql::ast::Statement;
    use crate::sql::parser::parse;

    fn bound(sql: &str) -> BoundDdl {
        match parse(sql).unwrap().remove(0) {
            Statement::CreateRole(s) => {
                BoundDdl::CreateRole(crate::analyzer::ddl_ext::role::analyze_create(&s).unwrap())
            }
            Statement::AlterRole(s) => {
                BoundDdl::AlterRole(crate::analyzer::ddl_ext::role::analyze_alter(&s).unwrap())
            }
            o => panic!("{o:?}"),
        }
    }

    fn run(h: &mut Harness, sql: &str) -> Result<String> {
        h.exec_w(bound(sql))
    }

    fn row(h: &Harness, name: &str) -> Option<AuthidRow> {
        let snap = h.tc.cluster.txn_manager().snapshot(h.txn.xid, h.txn.cid);
        h.db.shared
            .authid_by_name(&snap, name)
            .unwrap()
            .map(|x| x.1)
    }

    #[test]
    fn default_policy_matches_postgres() {
        let p = PasswordPolicy::default();
        assert_eq!(p.encryption, PasswordEncryption::ScramSha256);
        assert_eq!(p.scram_iterations, 4096);
    }

    #[test]
    fn encrypt_password_rules() {
        let p = PasswordPolicy::default();
        assert_eq!(encrypt_password(&p, None).unwrap().secret, None);
        let e = encrypt_password(&p, Some(&Secret(String::new()))).unwrap();
        assert!(e.cleared_empty && e.secret.is_none());
        let s = encrypt_password(&p, Some(&Secret("pw".into())))
            .unwrap()
            .secret
            .unwrap();
        assert!(s.starts_with("SCRAM-SHA-256$4096:"));
        let again = encrypt_password(&p, Some(&Secret(s.clone()))).unwrap();
        assert_eq!(again.secret, Some(s));
        let md5 = format!("md5{}", "0".repeat(32));
        let e = encrypt_password(&p, Some(&Secret(md5))).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        let md = PasswordPolicy {
            encryption: PasswordEncryption::Md5,
            scram_iterations: 4096,
        };
        assert!(encrypt_password(&md, Some(&Secret("x".into()))).is_err());
        let few = PasswordPolicy {
            encryption: PasswordEncryption::ScramSha256,
            scram_iterations: 10,
        };
        let s = encrypt_password(&few, Some(&Secret("x".into())))
            .unwrap()
            .secret
            .unwrap();
        assert!(s.starts_with("SCRAM-SHA-256$10:"));
    }

    #[test]
    fn create_inserts_a_row_and_rolls_back() {
        let mut h = Harness::new();
        assert_eq!(
            run(&mut h, "CREATE USER alice PASSWORD 'pw' CONNECTION LIMIT 3").unwrap(),
            "CREATE ROLE"
        );
        let r = row(&h, "alice").unwrap();
        assert!(r.can_login && r.inherit && !r.superuser);
        assert_eq!(r.conn_limit, 3);
        assert!(r.oid >= 16384);
        assert!(r.password.unwrap().starts_with("SCRAM-SHA-256$4096:"));
        h.rollback();
        assert!(row(&h, "alice").is_none());
    }

    #[test]
    fn create_errors() {
        let mut h = Harness::new();
        run(&mut h, "CREATE ROLE a").unwrap();
        let e = run(&mut h, "CREATE ROLE a").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_OBJECT);
        let e = run(&mut h, "CREATE ROLE pg_x").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::RESERVED_NAME);
        h.rollback();
    }

    #[test]
    fn empty_password_clears_with_notice() {
        let mut h = Harness::new();
        run(&mut h, "CREATE ROLE e PASSWORD ''").unwrap();
        assert!(row(&h, "e").unwrap().password.is_none());
        assert!(
            h.notices
                .iter()
                .any(|n| n.message.starts_with("empty string"))
        );
        h.rollback();
    }

    #[test]
    fn alter_changes_only_given_items() {
        let mut h = Harness::new();
        run(&mut h, "CREATE ROLE a CREATEDB CONNECTION LIMIT 2").unwrap();
        assert_eq!(run(&mut h, "ALTER ROLE a LOGIN").unwrap(), "ALTER ROLE");
        let r = row(&h, "a").unwrap();
        assert!(r.can_login && r.create_db);
        assert_eq!(r.conn_limit, 2);
        run(&mut h, "ALTER ROLE a PASSWORD 'x'").unwrap();
        assert!(row(&h, "a").unwrap().password.is_some());
        run(&mut h, "ALTER ROLE a PASSWORD NULL").unwrap();
        assert!(row(&h, "a").unwrap().password.is_none());
        let e = run(&mut h, "ALTER ROLE nobody LOGIN").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        h.rollback();
    }

    #[test]
    fn rename_rules() {
        let mut h = Harness::new();
        run(&mut h, "CREATE ROLE a").unwrap();
        run(&mut h, "CREATE ROLE b").unwrap();
        run(&mut h, "ALTER ROLE a RENAME TO c").unwrap();
        assert!(row(&h, "a").is_none() && row(&h, "c").is_some());
        let e = run(&mut h, "ALTER ROLE c RENAME TO b").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_OBJECT);
        let e = run(&mut h, "ALTER ROLE c RENAME TO pg_z").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::RESERVED_NAME);
        let e = run(&mut h, "ALTER ROLE postgres RENAME TO q").unwrap_err();
        assert_eq!(e.message, "session user cannot be renamed");
        h.rollback();
    }

    #[test]
    fn bootstrap_superuser_keeps_the_attribute() {
        let mut h = Harness::new();
        let e = run(&mut h, "ALTER ROLE postgres NOSUPERUSER").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "permission denied to alter role");
        h.rollback();
    }

    /// 一般ロールとして実行する。
    fn become_role(h: &mut Harness, name: &str) {
        let r = row(h, name).unwrap();
        h.role = r.oid;
        h.role_row = r.to_role_row();
    }

    #[test]
    fn privileges_for_ordinary_roles() {
        let mut h = Harness::new();
        run(&mut h, "CREATE ROLE plain").unwrap();
        run(&mut h, "CREATE ROLE mgr CREATEROLE").unwrap();
        run(&mut h, "CREATE ROLE victim").unwrap();
        h.commit();

        become_role(&mut h, "plain");
        let e = run(&mut h, "CREATE ROLE x").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        assert_eq!(e.message, "permission denied to create role");
        // 自分のパスワードだけは変えられる。
        run(&mut h, "ALTER ROLE plain PASSWORD 'n'").unwrap();
        let e = run(&mut h, "ALTER ROLE plain CONNECTION LIMIT 1").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        let e = run(&mut h, "ALTER ROLE victim PASSWORD 'n'").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        h.rollback();

        become_role(&mut h, "mgr");
        run(&mut h, "CREATE ROLE ok").unwrap();
        let e = run(&mut h, "CREATE ROLE s SUPERUSER").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        let e = run(&mut h, "CREATE ROLE s CREATEDB").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        run(&mut h, "ALTER ROLE victim NOLOGIN").unwrap();
        let e = run(&mut h, "ALTER ROLE postgres NOLOGIN").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        h.rollback();
    }

    #[test]
    fn privilege_check_is_made_against_the_latest_row() {
        let mut h = Harness::new();
        run(&mut h, "CREATE ROLE mgr CREATEROLE").unwrap();
        h.commit();
        become_role(&mut h, "mgr");
        // ロールの行を superuser 側から NOCREATEROLE に変えると、同じセッションでも直ちに効く。
        become_role(&mut h, "postgres");
        run(&mut h, "ALTER ROLE mgr NOCREATEROLE").unwrap();
        h.commit();
        become_role(&mut h, "mgr");
        let e = run(&mut h, "CREATE ROLE y").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        h.rollback();
    }
}

#[cfg(test)]
mod tests_valid_until {
    use super::*;
    use crate::testing::TestCluster;

    #[test]
    fn valid_until_parsing() {
        let tc = TestCluster::new();
        let st = crate::settings::Settings::new("postgres", &[]);
        let dts = st.datetime_settings(tc.cluster.zones());
        let env = crate::types::TypeEnv {
            datetime: Some(dts.env(tc.cluster.zones(), 0)),
            ..Default::default()
        };
        assert_eq!(parse_valid_until(&env, "infinity").unwrap(), i64::MAX);
        assert_eq!(
            parse_valid_until(&env, "2000-01-01 00:00:00+00").unwrap(),
            0
        );
        let e = parse_valid_until(&env, "garbage").unwrap_err();
        assert_eq!(e.sqlstate.0, "22007");
    }
}

#[cfg(test)]
mod tests_drop {
    use super::*;
    use crate::analyzer::query::BoundDdl;
    use crate::ddl::testkit::Harness;
    use crate::sql::ast::Statement;
    use crate::sql::parser::parse;

    fn run(h: &mut Harness, sql: &str) -> Result<String> {
        let b = match parse(sql).unwrap().remove(0) {
            Statement::CreateRole(s) => {
                BoundDdl::CreateRole(crate::analyzer::ddl_ext::role::analyze_create(&s).unwrap())
            }
            Statement::DropRole(s) => {
                BoundDdl::DropRole(crate::analyzer::ddl_ext::role::analyze_drop(&s).unwrap())
            }
            o => panic!("{o:?}"),
        };
        h.exec_w(b)
    }

    fn exists(h: &Harness, name: &str) -> bool {
        let snap = h.tc.cluster.txn_manager().snapshot(h.txn.xid, h.txn.cid);
        h.db.shared.authid_by_name(&snap, name).unwrap().is_some()
    }

    #[test]
    fn drop_removes_the_row_and_rolls_back() {
        let mut h = Harness::new();
        run(&mut h, "CREATE ROLE a").unwrap();
        h.commit();
        assert_eq!(run(&mut h, "DROP ROLE a").unwrap(), "DROP ROLE");
        assert!(!exists(&h, "a"));
        h.rollback();
        run(&mut h, "CREATE ROLE b").unwrap();
        assert!(exists(&h, "b"));
        h.rollback();
    }

    #[test]
    fn missing_roles_and_if_exists() {
        let mut h = Harness::new();
        let e = run(&mut h, "DROP ROLE nope").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(e.message, "role \"nope\" does not exist");
        run(&mut h, "DROP ROLE IF EXISTS nope").unwrap();
        run(&mut h, "CREATE ROLE a").unwrap();
        h.rollback();
    }

    #[test]
    fn multiple_names_see_earlier_drops() {
        let mut h = Harness::new();
        run(&mut h, "CREATE ROLE a").unwrap();
        let e = run(&mut h, "DROP ROLE a, a").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        h.rollback();
    }

    #[test]
    fn current_user_and_system_roles_are_refused() {
        let mut h = Harness::new();
        let e = run(&mut h, "DROP ROLE current_user").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OBJECT_IN_USE);
        assert_eq!(e.message, "current user cannot be dropped");
        let e = run(&mut h, "DROP ROLE postgres").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OBJECT_IN_USE);
        h.rollback();
    }

    #[test]
    fn owned_objects_block_the_drop() {
        let mut h = Harness::new();
        run(&mut h, "CREATE ROLE a").unwrap();
        h.commit();
        let snap = h.tc.cluster.txn_manager().snapshot(h.txn.xid, h.txn.cid);
        let oid =
            h.db.shared
                .authid_by_name(&snap, "a")
                .unwrap()
                .unwrap()
                .1
                .oid;
        assert!(
            owned_objects(&h.tc.cluster, &h.db.shared, &snap, oid)
                .unwrap()
                .is_empty()
        );
        // ブートストラップ超ユーザーは多くを所有する。
        let objs = owned_objects(&h.tc.cluster, &h.db.shared, &snap, 10).unwrap();
        assert!(objs.iter().any(|o| o.kind == OwnedKind::Database));
        assert!(objs.iter().any(|o| o.kind == OwnedKind::Table));
        run(&mut h, "DROP ROLE a").unwrap();
        h.rollback();
    }

    #[test]
    fn detail_formatting() {
        let o = |db: Oid, kind, name: &str| OwnedObject {
            db_oid: db,
            db_name: format!("db{db}"),
            kind,
            name: name.to_owned(),
        };
        let objs = [
            o(1, OwnedKind::Table, "t"),
            o(1, OwnedKind::Index, "t_pkey"),
            o(2, OwnedKind::Database, "db2"),
            o(3, OwnedKind::Table, "x"),
            o(3, OwnedKind::Schema, "s"),
        ];
        assert_eq!(
            dependency_detail(&objs, 1),
            "owner of table t\nowner of index t_pkey\nowner of database db2\n2 objects in database db3"
        );
        let many: Vec<OwnedObject> = (0..102)
            .map(|i| o(1, OwnedKind::Table, &format!("t{i:03}")))
            .collect();
        let d = dependency_detail(&many, 1);
        assert_eq!(d.lines().count(), 101);
        assert!(d.ends_with("and 2 other objects"));
    }
}
