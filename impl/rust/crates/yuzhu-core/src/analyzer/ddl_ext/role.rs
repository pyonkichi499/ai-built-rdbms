//! `CREATE / ALTER / DROP ROLE` の `BoundDdl` の組み立て（M5 AU-3。`m5/08` §4.5・§6.5）。

use crate::analyzer::not_supported;
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{
    AlterRoleAction, AlterRoleStmt, CreateRoleStmt, DropRoleStmt, RoleKeyword, RoleOption,
    RoleOptionItem, RoleSpec, Secret,
};

/// `CREATE ROLE` の束縛結果。既定値は適用済み。
#[allow(clippy::struct_excessive_bools)] // pg_authid の属性列に 1 対 1 で対応する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundCreateRole {
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub create_role: bool,
    pub create_db: bool,
    pub can_login: bool,
    pub replication: bool,
    pub bypass_rls: bool,
    pub conn_limit: i32,
    /// `PASSWORD NULL` と省略は `None`。
    pub password: Option<Secret>,
    pub valid_until: Option<String>,
}

impl Default for BoundCreateRole {
    fn default() -> Self {
        BoundCreateRole {
            name: String::new(),
            superuser: false,
            inherit: true,
            create_role: false,
            create_db: false,
            can_login: false,
            replication: false,
            bypass_rls: false,
            conn_limit: -1,
            password: None,
            valid_until: None,
        }
    }
}

/// `ALTER ROLE` の対象。`CURRENT_USER` / `CURRENT_ROLE` / `SESSION_USER` は M5 では同じ（接続しているロール）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleTarget {
    Name(String),
    CurrentUser,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundAlterRole {
    pub target: RoleTarget,
    pub action: BoundAlterRoleAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundAlterRoleAction {
    /// 指定された項目だけ `Some`。
    Attrs(RoleChanges),
    Rename(String),
}

impl Default for BoundAlterRole {
    fn default() -> Self {
        BoundAlterRole {
            target: RoleTarget::CurrentUser,
            action: BoundAlterRoleAction::Attrs(RoleChanges::default()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleChanges {
    pub superuser: Option<bool>,
    pub inherit: Option<bool>,
    pub create_role: Option<bool>,
    pub create_db: Option<bool>,
    pub can_login: Option<bool>,
    pub replication: Option<bool>,
    pub bypass_rls: Option<bool>,
    pub conn_limit: Option<i32>,
    /// `Some(None)` = `PASSWORD NULL`。
    pub password: Option<Option<Secret>>,
    pub valid_until: Option<String>,
}

fn conflicting(span: Span) -> Error {
    Error::new(sqlstate::SYNTAX_ERROR, "conflicting or redundant options").with_span(span)
}

/// オプションを先頭から見て `RoleChanges` にする。同じ種類が 2 回あれば `42601`（位置はあとのほう）。
fn collect_options(options: &[RoleOptionItem]) -> Result<RoleChanges> {
    // UNENCRYPTED PASSWORD は構文の段階（PostgreSQL では gram.y）で拒否される。
    if let Some(item) = options
        .iter()
        .find(|i| matches!(i.option, RoleOption::UnencryptedPassword))
    {
        return Err(
            Error::not_supported("UNENCRYPTED PASSWORD is no longer supported")
                .with_hint("Remove UNENCRYPTED to store the password in encrypted form instead.")
                .with_span(item.span),
        );
    }
    let mut c = RoleChanges::default();
    for item in options {
        let span = item.span;
        match &item.option {
            RoleOption::Superuser(v) => set(&mut c.superuser, *v, span)?,
            RoleOption::CreateDb(v) => set(&mut c.create_db, *v, span)?,
            RoleOption::CreateRole(v) => set(&mut c.create_role, *v, span)?,
            RoleOption::Inherit(v) => set(&mut c.inherit, *v, span)?,
            RoleOption::Login(v) => set(&mut c.can_login, *v, span)?,
            RoleOption::Replication(v) => set(&mut c.replication, *v, span)?,
            RoleOption::BypassRls(v) => set(&mut c.bypass_rls, *v, span)?,
            RoleOption::ConnectionLimit(n) => {
                set(&mut c.conn_limit, *n, span)?;
                if *n < -1 {
                    return Err(Error::new(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format!("invalid connection limit: {n}"),
                    ));
                }
            }
            RoleOption::Password(p) => set(&mut c.password, p.clone(), span)?,
            RoleOption::ValidUntil(s) => set(&mut c.valid_until, s.clone(), span)?,
            RoleOption::Sysid(_) | RoleOption::UnencryptedPassword => {}
            RoleOption::InRole(_) | RoleOption::Role(_) | RoleOption::Admin(_) => {
                return Err(not_supported("role membership", span));
            }
        }
    }
    Ok(c)
}

fn set<T>(slot: &mut Option<T>, v: T, span: Span) -> Result<()> {
    if slot.is_some() {
        return Err(conflicting(span));
    }
    *slot = Some(v);
    Ok(())
}

/// `CREATE ROLE`。`CREATE USER` は `can_login` の既定が真（明示の `NOLOGIN` が勝つ）。
pub fn analyze_create(s: &CreateRoleStmt) -> Result<BoundCreateRole> {
    let c = collect_options(&s.options)?;
    let d = BoundCreateRole::default();
    Ok(BoundCreateRole {
        name: s.name.clone(),
        superuser: c.superuser.unwrap_or(d.superuser),
        inherit: c.inherit.unwrap_or(d.inherit),
        create_role: c.create_role.unwrap_or(d.create_role),
        create_db: c.create_db.unwrap_or(d.create_db),
        can_login: c.can_login.unwrap_or(s.keyword == RoleKeyword::User),
        replication: c.replication.unwrap_or(d.replication),
        bypass_rls: c.bypass_rls.unwrap_or(d.bypass_rls),
        conn_limit: c.conn_limit.unwrap_or(d.conn_limit),
        password: c.password.unwrap_or(None),
        valid_until: c.valid_until,
    })
}

/// `ALTER ROLE`。
pub fn analyze_alter(s: &AlterRoleStmt) -> Result<BoundAlterRole> {
    let target = match &s.target {
        RoleSpec::Name(id) => RoleTarget::Name(id.value.clone()),
        RoleSpec::CurrentUser | RoleSpec::CurrentRole | RoleSpec::SessionUser => {
            RoleTarget::CurrentUser
        }
        RoleSpec::Public => {
            return Err(
                Error::new(sqlstate::UNDEFINED_OBJECT, "role \"public\" does not exist")
                    .with_span(s.target_span),
            );
        }
    };
    let action = match &s.action {
        AlterRoleAction::Options(o) => BoundAlterRoleAction::Attrs(collect_options(o)?),
        AlterRoleAction::Rename(n) => BoundAlterRoleAction::Rename(n.clone()),
        AlterRoleAction::Unsupported(what) => return Err(not_supported(what, s.span)),
    };
    Ok(BoundAlterRole { target, action })
}

/// `DROP ROLE`。名前は先頭から 1 つずつ処理される。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundDropRole {
    pub names: Vec<RoleTarget>,
    pub if_exists: bool,
}

/// `DROP ROLE`。`PUBLIC` は存在しないロール（42704）、`CURRENT_USER` などは自分自身。
pub fn analyze_drop(s: &DropRoleStmt) -> Result<BoundDropRole> {
    let mut names = Vec::with_capacity(s.names.len());
    for (spec, span) in &s.names {
        names.push(match spec {
            RoleSpec::Name(id) => RoleTarget::Name(id.value.clone()),
            RoleSpec::CurrentUser | RoleSpec::CurrentRole | RoleSpec::SessionUser => {
                RoleTarget::CurrentUser
            }
            RoleSpec::Public => {
                return Err(Error::new(
                    sqlstate::UNDEFINED_OBJECT,
                    "role \"public\" does not exist",
                )
                .with_span(*span));
            }
        });
    }
    Ok(BoundDropRole {
        names,
        if_exists: s.if_exists,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::Statement;
    use crate::sql::parser::parse;

    fn create(sql: &str) -> Result<BoundCreateRole> {
        match parse(sql).unwrap().remove(0) {
            Statement::CreateRole(s) => analyze_create(&s),
            other => panic!("{other:?}"),
        }
    }

    fn alter(sql: &str) -> Result<BoundAlterRole> {
        match parse(sql).unwrap().remove(0) {
            Statement::AlterRole(s) => analyze_alter(&s),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn create_defaults() {
        let b = create("CREATE ROLE r").unwrap();
        assert_eq!(
            b,
            BoundCreateRole {
                name: "r".into(),
                ..Default::default()
            }
        );
        assert!(!b.can_login && b.inherit && b.conn_limit == -1);
    }

    #[test]
    fn create_user_logs_in_unless_nologin() {
        assert!(create("CREATE USER u").unwrap().can_login);
        assert!(!create("CREATE USER u NOLOGIN").unwrap().can_login);
        assert!(!create("CREATE GROUP g").unwrap().can_login);
    }

    #[test]
    fn create_attributes() {
        let b = create(
            "CREATE ROLE r SUPERUSER CREATEDB CREATEROLE NOINHERIT LOGIN REPLICATION BYPASSRLS \
             CONNECTION LIMIT 3 PASSWORD 'pw' VALID UNTIL 'infinity' SYSID 9",
        )
        .unwrap();
        assert!(b.superuser && b.create_db && b.create_role && !b.inherit);
        assert!(b.can_login && b.replication && b.bypass_rls);
        assert_eq!(b.conn_limit, 3);
        assert_eq!(b.password, Some(Secret("pw".into())));
        assert_eq!(b.valid_until.as_deref(), Some("infinity"));
        assert_eq!(
            create("CREATE ROLE r PASSWORD NULL").unwrap().password,
            None
        );
    }

    #[test]
    fn conflicting_options_point_at_the_later_one() {
        let sql = "CREATE ROLE r SUPERUSER NOSUPERUSER";
        let e = create(sql).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "conflicting or redundant options");
        assert_eq!(
            e.cursor_byte.unwrap() as usize,
            sql.find("NOSUPERUSER").unwrap()
        );
        assert!(create("CREATE ROLE r PASSWORD 'a' PASSWORD NULL").is_err());
        assert!(create("CREATE ROLE r CREATEDB CREATEDB").is_err());
        assert!(create("CREATE ROLE r VALID UNTIL 'a' VALID UNTIL 'b'").is_err());
    }

    #[test]
    fn unsupported_options() {
        let e = create("CREATE ROLE r UNENCRYPTED PASSWORD 'x'").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "UNENCRYPTED PASSWORD is no longer supported");
        let e = create("CREATE ROLE r IN ROLE a").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "role membership is not supported yet");
        let e = alter("ALTER ROLE r SET x = 1").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert!(e.message.ends_with("is not supported yet"));
    }

    #[test]
    fn connection_limit_range() {
        assert_eq!(
            create("CREATE ROLE r CONNECTION LIMIT -1")
                .unwrap()
                .conn_limit,
            -1
        );
        assert_eq!(
            create("CREATE ROLE r CONNECTION LIMIT 0")
                .unwrap()
                .conn_limit,
            0
        );
        let e = create("CREATE ROLE r CONNECTION LIMIT -2").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(e.message, "invalid connection limit: -2");
    }

    #[test]
    fn alter_sets_only_given_items() {
        let b = alter("ALTER ROLE r NOLOGIN CONNECTION LIMIT 2").unwrap();
        assert_eq!(b.target, RoleTarget::Name("r".into()));
        let BoundAlterRoleAction::Attrs(c) = b.action else {
            panic!()
        };
        assert_eq!(c.can_login, Some(false));
        assert_eq!(c.conn_limit, Some(2));
        assert_eq!(c.superuser, None);
        assert_eq!(c.password, None);
        let BoundAlterRoleAction::Attrs(c) = alter("ALTER ROLE r PASSWORD NULL").unwrap().action
        else {
            panic!()
        };
        assert_eq!(c.password, Some(None));
    }

    #[test]
    fn drop_binds_names_and_current_user() {
        let Statement::DropRole(d) = parse("DROP ROLE IF EXISTS a, current_user")
            .unwrap()
            .remove(0)
        else {
            panic!()
        };
        let b = analyze_drop(&d).unwrap();
        assert!(b.if_exists);
        assert_eq!(
            b.names,
            [RoleTarget::Name("a".into()), RoleTarget::CurrentUser]
        );
        let Statement::DropRole(d) = parse("DROP ROLE public").unwrap().remove(0) else {
            panic!()
        };
        assert_eq!(
            analyze_drop(&d).unwrap_err().sqlstate,
            sqlstate::UNDEFINED_OBJECT
        );
    }

    #[test]
    fn alter_targets_and_rename() {
        assert_eq!(
            alter("ALTER ROLE CURRENT_USER").unwrap().target,
            RoleTarget::CurrentUser
        );
        assert_eq!(
            alter("ALTER USER session_user").unwrap().target,
            RoleTarget::CurrentUser
        );
        assert_eq!(
            alter("ALTER ROLE a RENAME TO b").unwrap().action,
            BoundAlterRoleAction::Rename("b".into())
        );
        let e = alter("ALTER ROLE public NOLOGIN").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
    }
}
