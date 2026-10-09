//! 仮想リレーション（M5 LK-5。`spec/design/m5/01-lock-txn.md` §6.5、契約 §4.10）。
//!
//! 行を提供関数が作るリレーション（`pg_locks` など）。`pg_class` には relkind `v`、`relfilenode = 0` の
//! 行を持ち、ヒープもインデックスも持たない。読み取りは `PhysicalPlan::VirtualScan` が
//! [`VirtualRelation::rows`] を呼ぶ。INSERT / UPDATE / DELETE は 55000、TRUNCATE / DROP TABLE は 42809
//! （[`modify_error`]、[`not_a_table_error`]）。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::RelKind;
use super::rows::{AttributeSpec, ClassSpec, attribute_row, class_row};
use super::schema::{CatalogColumn, oids};
use crate::backend::{BackendId, BackendRegistry};
use crate::error::{Error, Result, sqlstate};
use crate::executor::{RuntimeInfo, SessionInfo};
use crate::txn::lock::{LockManager, LockStatusRow};
use crate::types::{Datum, Oid, Row, SqlType, oid};

/// `pg_locks` の OID（契約 §4.10）。
pub const PG_LOCKS_OID: Oid = 9811;

/// 2000-01-01 と 1970-01-01 の差（秒）。
const UNIX_TO_PG_SECS: i64 = 946_684_800;

/// 仮想リレーションの行を作るときに見られるもの。
pub struct VirtualCtx<'a> {
    pub runtime: &'a dyn RuntimeInfo,
    pub session: &'a SessionInfo,
}

impl std::fmt::Debug for VirtualCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualCtx")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// 行を提供関数が作るリレーション。
pub trait VirtualRelation: Send + Sync + std::fmt::Debug {
    fn oid(&self) -> Oid;
    fn name(&self) -> &'static str;
    fn columns(&self) -> &'static [CatalogColumn];
    /// 行の並びは列の定義の順。文の中では一度だけ呼ばれる（`VirtualScan` が溜める）。
    fn rows(&self, ctx: &VirtualCtx<'_>) -> Result<Vec<Row>>;
}

/// 仮想リレーションの一覧（08 章が `pg_roles` を足す）。
pub fn registry() -> &'static [&'static dyn VirtualRelation] {
    static REGISTRY: [&dyn VirtualRelation; 3] = [&PgLocks, &PgRoles, &PgAuthMembers];
    &REGISTRY
}

pub fn find(oid: Oid) -> Option<&'static dyn VirtualRelation> {
    registry().iter().copied().find(|r| r.oid() == oid)
}

/// initdb が `pg_class` と `pg_attribute` に書く行（データベースごと。内容は同じ）。
#[derive(Debug, Clone, Default)]
pub struct VirtualCatalogRows {
    pub class: Vec<Row>,
    pub attribute: Vec<Row>,
}

pub fn bootstrap_catalog_rows(_db_oid: Oid) -> VirtualCatalogRows {
    let mut out = VirtualCatalogRows::default();
    for rel in registry() {
        out.class.push(class_row(&ClassSpec {
            oid: rel.oid(),
            name: rel.name(),
            namespace: oids::NAMESPACE_PG_CATALOG,
            reltype: 0,
            owner: oids::BOOTSTRAP_SUPERUSER,
            relfilenode: 0,
            reltablespace: 0,
            is_shared: false,
            natts: i16::try_from(rel.columns().len()).unwrap_or(i16::MAX),
            nchecks: 0,
            replident: 'n',
            kind: RelKind::Virtual,
            has_index: false,
            relpages: 0,
            reltuples: -1.0,
        }));
        for (c, attnum) in rel.columns().iter().zip(1i16..) {
            out.attribute.extend(attribute_row(&AttributeSpec {
                relid: rel.oid(),
                name: c.name,
                attnum,
                ty: SqlType::of(c.type_oid),
                not_null: c.not_null,
                has_default: false,
                catalog_column: true,
                identity: None,
            }));
        }
    }
    out
}

/// `INSERT` / `UPDATE` / `DELETE` の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifyKind {
    Insert,
    Update,
    Delete,
}

/// 仮想リレーションへの DML（55000。PostgreSQL 17 のビューと同じ文言。実機 E17）。
pub fn modify_error(rel_name: &str, kind: ModifyKind) -> Error {
    let (message, hint) = match kind {
        ModifyKind::Insert => (
            format!("cannot insert into view \"{rel_name}\""),
            "To enable inserting into the view, provide an INSTEAD OF INSERT trigger or an unconditional ON INSERT DO INSTEAD rule.",
        ),
        ModifyKind::Update => (
            format!("cannot update view \"{rel_name}\""),
            "To enable updating the view, provide an INSTEAD OF UPDATE trigger or an unconditional ON UPDATE DO INSTEAD rule.",
        ),
        ModifyKind::Delete => (
            format!("cannot delete from view \"{rel_name}\""),
            "To enable deleting from the view, provide an INSTEAD OF DELETE trigger or an unconditional ON DELETE DO INSTEAD rule.",
        ),
    };
    Error::new(sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE, message)
        .with_detail(
            "Views that do not select from a single table or view are not automatically updatable.",
        )
        .with_hint(hint)
}

/// 仮想リレーションへの `TRUNCATE`（`is_drop = false`）と `DROP TABLE`（`true`。HINT つき）。42809。
pub fn not_a_table_error(rel_name: &str, is_drop: bool) -> Error {
    let e = Error::new(
        sqlstate::WRONG_OBJECT_TYPE,
        format!("\"{rel_name}\" is not a table"),
    );
    if is_drop {
        e.with_hint("Use DROP VIEW to remove a view.")
    } else {
        e
    }
}

// ----- pid <-> BackendId（`RuntimeInfo` の実装用。`session/locking.rs`） ---------------------

/// `pg_blocking_pids(pid)`: `pid` を止めているバックエンドの pid（hard の保持者、soft の先行する待ち手の順）。
/// `pid` が登録されていなければ空。
pub fn blocking_pids(locks: &LockManager, backends: &BackendRegistry, pid: i32) -> Vec<i32> {
    let Some(me) = backends.by_pid(pid) else {
        return Vec::new();
    };
    locks
        .blocking_backends(me.id)
        .into_iter()
        .filter_map(|b| backends.by_id(b).map(|i| i.pid))
        .collect()
}

/// `pg_isolation_test_session_is_blocked(pid, among)`: CR-1 の意味。`pid` が登録されていなければ `false`。
/// 登録のない `among` の pid は無視する。
pub fn is_blocked_by_pids(
    locks: &LockManager,
    backends: &BackendRegistry,
    pid: i32,
    among: &[i32],
) -> bool {
    let Some(me) = backends.by_pid(pid) else {
        return false;
    };
    let ids: Vec<BackendId> = among
        .iter()
        .filter_map(|p| backends.by_pid(*p).map(|i| i.id))
        .collect();
    locks.is_blocked_by(me.id, &ids)
}

// ----- pg_locks -------------------------------------------------------------

/// `pg_locks`（PostgreSQL 17 と同じ 16 列）。`virtualxid` と `virtualtransaction` は常に NULL、
/// `fastpath` は常に false（M5-LK-Q6）。
#[derive(Debug)]
pub struct PgLocks;

const fn column(name: &'static str, type_oid: Oid) -> CatalogColumn {
    CatalogColumn {
        name,
        type_oid,
        not_null: false,
    }
}

static PG_LOCKS_COLUMNS: [CatalogColumn; 16] = [
    column("locktype", oid::TEXT),
    column("database", oid::OID),
    column("relation", oid::OID),
    column("page", oid::INT4),
    column("tuple", oid::INT2),
    column("virtualxid", oid::TEXT),
    column("transactionid", oid::XID),
    column("classid", oid::OID),
    column("objid", oid::OID),
    column("objsubid", oid::INT2),
    column("virtualtransaction", oid::TEXT),
    column("pid", oid::INT4),
    column("mode", oid::TEXT),
    column("granted", oid::BOOL),
    column("fastpath", oid::BOOL),
    column("waitstart", oid::TIMESTAMPTZ),
];

impl VirtualRelation for PgLocks {
    fn oid(&self) -> Oid {
        PG_LOCKS_OID
    }

    fn name(&self) -> &'static str {
        "pg_locks"
    }

    fn columns(&self) -> &'static [CatalogColumn] {
        &PG_LOCKS_COLUMNS
    }

    fn rows(&self, ctx: &VirtualCtx<'_>) -> Result<Vec<Row>> {
        let now = now_pg_micros();
        Ok(ctx
            .runtime
            .lock_status()
            .iter()
            .map(|r| pg_locks_row(r, now))
            .collect())
    }
}

// ----- pg_roles / pg_auth_members（M5 AU-3b。08 §5.10）-----------------------------

/// `pg_roles` の OID（08 C6）。
pub const PG_ROLES_OID: Oid = 9810;
/// `pg_auth_members` の OID（PostgreSQL と同じ）。
pub const PG_AUTH_MEMBERS_OID: Oid = 1261;

/// `pg_roles`。`rolpassword` は常に `********`、`rolconfig` は常に NULL。
#[derive(Debug)]
pub struct PgRoles;

static PG_ROLES_COLUMNS: [CatalogColumn; 13] = [
    column("rolname", oid::NAME),
    column("rolsuper", oid::BOOL),
    column("rolinherit", oid::BOOL),
    column("rolcreaterole", oid::BOOL),
    column("rolcreatedb", oid::BOOL),
    column("rolcanlogin", oid::BOOL),
    column("rolreplication", oid::BOOL),
    column("rolconnlimit", oid::INT4),
    column("rolpassword", oid::TEXT),
    column("rolvaliduntil", oid::TIMESTAMPTZ),
    column("rolbypassrls", oid::BOOL),
    column("rolconfig", oid::TEXT_ARRAY),
    column("oid", oid::OID),
];

/// `pg_authid` の行から `pg_roles` の行を作る。
pub fn pg_roles_row(r: &crate::catalog::roles::AuthidRow) -> Row {
    vec![
        Datum::Text(r.name.clone()),
        Datum::Bool(r.superuser),
        Datum::Bool(r.inherit),
        Datum::Bool(r.create_role),
        Datum::Bool(r.create_db),
        Datum::Bool(r.can_login),
        Datum::Bool(r.replication),
        Datum::Int4(r.conn_limit),
        Datum::Text("********".to_owned()),
        r.valid_until.map_or(Datum::Null, |t| {
            Datum::TimestampTz(yuzhu_datetime::TimestampTz(t))
        }),
        Datum::Bool(r.bypass_rls),
        Datum::Null,
        Datum::Oid(r.oid),
    ]
}

impl VirtualRelation for PgRoles {
    fn oid(&self) -> Oid {
        PG_ROLES_OID
    }

    fn name(&self) -> &'static str {
        "pg_roles"
    }

    fn columns(&self) -> &'static [CatalogColumn] {
        &PG_ROLES_COLUMNS
    }

    fn rows(&self, ctx: &VirtualCtx<'_>) -> Result<Vec<Row>> {
        Ok(ctx
            .runtime
            .authid_rows()?
            .iter()
            .map(pg_roles_row)
            .collect())
    }
}

/// `pg_auth_members`。行は常に 0 件（メンバーシップは M6。`\du` の副問い合わせを通すためだけに置く）。
#[derive(Debug)]
pub struct PgAuthMembers;

static PG_AUTH_MEMBERS_COLUMNS: [CatalogColumn; 7] = [
    column("oid", oid::OID),
    column("roleid", oid::OID),
    column("member", oid::OID),
    column("grantor", oid::OID),
    column("admin_option", oid::BOOL),
    column("inherit_option", oid::BOOL),
    column("set_option", oid::BOOL),
];

impl VirtualRelation for PgAuthMembers {
    fn oid(&self) -> Oid {
        PG_AUTH_MEMBERS_OID
    }

    fn name(&self) -> &'static str {
        "pg_auth_members"
    }

    fn columns(&self) -> &'static [CatalogColumn] {
        &PG_AUTH_MEMBERS_COLUMNS
    }

    fn rows(&self, _ctx: &VirtualCtx<'_>) -> Result<Vec<Row>> {
        Ok(Vec::new())
    }
}

/// 現在時刻（2000-01-01 からのマイクロ秒）。
fn now_pg_micros() -> i64 {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    i64::try_from(unix.as_micros()).map_or(0, |us| us - UNIX_TO_PG_SECS * 1_000_000)
}

fn pg_locks_row(r: &LockStatusRow, now_us: i64) -> Row {
    let oid_or_null = |v: Option<Oid>| v.map_or(Datum::Null, Datum::Oid);
    vec![
        Datum::Text(r.locktype.to_owned()),
        oid_or_null(r.database),
        oid_or_null(r.relation),
        r.page.map_or(Datum::Null, |p| {
            Datum::Int4(i32::from_ne_bytes(p.to_ne_bytes()))
        }),
        r.tuple.map_or(Datum::Null, |t| {
            Datum::Int2(i16::from_ne_bytes(t.to_ne_bytes()))
        }),
        Datum::Null,
        // 外部表現は下位 32 ビット（`xid` 型）。
        r.transactionid
            .map_or(Datum::Null, |x| Datum::Xid(x.to_external())),
        oid_or_null(r.classid),
        oid_or_null(r.objid),
        r.objsubid.map_or(Datum::Null, Datum::Int2),
        Datum::Null,
        Datum::Int4(r.pid),
        Datum::Text(r.mode.to_owned()),
        Datum::Bool(r.granted),
        Datum::Bool(false),
        r.waited.map_or(Datum::Null, |w| {
            let waited = i64::try_from(w.as_micros()).unwrap_or(i64::MAX);
            Datum::TimestampTz(yuzhu_datetime::TimestampTz(now_us.saturating_sub(waited)))
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendId;
    use crate::executor::NullRuntime;
    use crate::txn::Xid;

    fn session() -> SessionInfo {
        SessionInfo {
            current_user: "u".into(),
            session_user: "u".into(),
            database: "d".into(),
            current_schema: None,
        }
    }

    fn status(locktype: &'static str, granted: bool) -> LockStatusRow {
        LockStatusRow {
            locktype,
            database: Some(5),
            relation: Some(16400),
            page: None,
            tuple: None,
            transactionid: None,
            backend: BackendId(1),
            pid: 4242,
            mode: "AccessShareLock",
            granted,
            classid: None,
            objid: None,
            objsubid: None,
            waited: None,
        }
    }

    #[test]
    fn pg_locks_has_sixteen_columns_in_pg17_order() {
        let names: Vec<&str> = PgLocks.columns().iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            [
                "locktype",
                "database",
                "relation",
                "page",
                "tuple",
                "virtualxid",
                "transactionid",
                "classid",
                "objid",
                "objsubid",
                "virtualtransaction",
                "pid",
                "mode",
                "granted",
                "fastpath",
                "waitstart"
            ]
        );
        assert_eq!(PgLocks.oid(), 9811);
    }

    #[test]
    fn registry_finds_pg_locks_only_by_its_oid() {
        assert_eq!(
            find(9811).map(super::VirtualRelation::name),
            Some("pg_locks")
        );
        assert!(find(1259).is_none());
    }

    #[test]
    fn row_for_a_relation_lock() {
        let row = pg_locks_row(&status("relation", true), 0);
        assert_eq!(row.len(), 16);
        assert_eq!(row[0], Datum::Text("relation".into()));
        assert_eq!(row[1], Datum::Oid(5));
        assert_eq!(row[2], Datum::Oid(16400));
        assert_eq!(row[3], Datum::Null);
        assert_eq!(row[5], Datum::Null);
        assert_eq!(row[10], Datum::Null);
        assert_eq!(row[11], Datum::Int4(4242));
        assert_eq!(row[12], Datum::Text("AccessShareLock".into()));
        assert_eq!(row[13], Datum::Bool(true));
        assert_eq!(row[14], Datum::Bool(false));
        assert_eq!(row[15], Datum::Null);
    }

    #[test]
    fn row_for_a_waiting_transactionid_lock_has_waitstart_and_low_32_bits() {
        let mut s = status("transactionid", false);
        s.database = None;
        s.relation = None;
        s.transactionid = Some(Xid(0x7_0000_04D2));
        s.mode = "ShareLock";
        s.waited = Some(Duration::from_secs(2));
        let row = pg_locks_row(&s, 10_000_000);
        assert_eq!(row[6], Datum::Xid(1234));
        assert_eq!(row[13], Datum::Bool(false));
        assert_eq!(
            row[15],
            Datum::TimestampTz(yuzhu_datetime::TimestampTz(8_000_000))
        );
    }

    #[test]
    fn row_for_tuple_and_object_locks() {
        let mut t = status("tuple", true);
        t.page = Some(3);
        t.tuple = Some(9);
        let row = pg_locks_row(&t, 0);
        assert_eq!(
            (row[3].clone(), row[4].clone()),
            (Datum::Int4(3), Datum::Int2(9))
        );

        let mut o = status("object", true);
        o.relation = None;
        o.classid = Some(1262);
        o.objid = Some(5);
        o.objsubid = Some(0);
        let row = pg_locks_row(&o, 0);
        assert_eq!(row[7], Datum::Oid(1262));
        assert_eq!(row[8], Datum::Oid(5));
        assert_eq!(row[9], Datum::Int2(0));
    }

    #[test]
    fn rows_come_from_the_runtime_and_are_empty_for_null_runtime() {
        let s = session();
        let ctx = VirtualCtx {
            runtime: &NullRuntime,
            session: &s,
        };
        assert!(PgLocks.rows(&ctx).unwrap().is_empty());
    }

    #[test]
    fn bootstrap_rows_describe_a_view_with_sixteen_attributes() {
        let rows = bootstrap_catalog_rows(5);
        assert_eq!(rows.class.len(), registry().len());
        // pg_locks（16 列）のほか pg_roles・pg_auth_members も仮想リレーションとして登録されている。
        let columns: usize = registry().iter().map(|r| r.columns().len()).sum();
        assert_eq!(rows.attribute.len(), columns);
        let class = &rows.class[0];
        assert_eq!(
            class[super::super::rows::column_index(oids::PG_CLASS, "oid")],
            Datum::Oid(9811)
        );
        assert_eq!(
            class[super::super::rows::column_index(oids::PG_CLASS, "relkind")],
            Datum::Char(b'v')
        );
        assert_eq!(
            class[super::super::rows::column_index(oids::PG_CLASS, "relfilenode")],
            Datum::Oid(0)
        );
    }

    #[test]
    fn pids_are_mapped_to_backends_for_blocking_queries() {
        use crate::backend::BackendInfo;
        use crate::interrupt::InterruptFlag;
        use crate::txn::lock::{LockMode, LockScope, LockTag, WaitCtl};
        use std::sync::Arc;

        let lm = LockManager::new();
        let reg = BackendRegistry::new();
        let mut guards = Vec::new();
        for n in 1..=2u64 {
            let pid = 100 + i32::try_from(n).unwrap();
            lm.register_backend(BackendId(n), pid);
            guards.push(reg.register(BackendInfo {
                id: BackendId(n),
                pid,
                db_oid: 5,
                user_oid: 10,
                client_addr: None,
                interrupts: Arc::new(InterruptFlag::default()),
            }));
        }
        let tag = LockTag::Relation { db: 5, rel: 16400 };
        let flag = InterruptFlag::default();
        let ctl = WaitCtl {
            lock_timeout: None,
            deadlock_timeout: Duration::from_secs(30),
            interrupts: &flag,
        };
        lm.acquire(
            BackendId(1),
            tag,
            LockMode::AccessShare,
            LockScope::Transaction,
            &ctl,
        )
        .unwrap();
        assert!(blocking_pids(&lm, &reg, 102).is_empty());

        let waiter_flag = Arc::new(InterruptFlag::default());
        let (lm2, f2) = (Arc::clone(&lm), Arc::clone(&waiter_flag));
        let h = std::thread::spawn(move || {
            let ctl = WaitCtl {
                lock_timeout: None,
                deadlock_timeout: Duration::from_secs(30),
                interrupts: &f2,
            };
            lm2.acquire(
                BackendId(2),
                tag,
                LockMode::AccessExclusive,
                LockScope::Transaction,
                &ctl,
            )
        });
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while lm.waiting_on(BackendId(2)).is_none() {
            assert!(std::time::Instant::now() < end, "waiter did not start");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(blocking_pids(&lm, &reg, 102), vec![101]);
        assert!(blocking_pids(&lm, &reg, 101).is_empty());
        assert!(blocking_pids(&lm, &reg, 0).is_empty());
        assert!(is_blocked_by_pids(&lm, &reg, 102, &[999, 101]));
        assert!(!is_blocked_by_pids(&lm, &reg, 102, &[999]));
        assert!(!is_blocked_by_pids(&lm, &reg, 101, &[102]));
        assert!(!is_blocked_by_pids(&lm, &reg, 0, &[101]));

        // 待ち手は pg_locks に granted = false の行で出る。
        let rows: Vec<Row> = lm
            .lock_status()
            .iter()
            .map(|r| pg_locks_row(r, 0))
            .collect();
        assert_eq!(rows.len(), 2);
        let waiting: Vec<_> = rows
            .iter()
            .filter(|r| r[13] == Datum::Bool(false))
            .collect();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0][11], Datum::Int4(102));
        assert_ne!(waiting[0][15], Datum::Null);

        lm.release_all(BackendId(1), LockScope::Session);
        h.join().unwrap().unwrap();
        lm.release_all(BackendId(2), LockScope::Session);
    }

    #[test]
    fn modify_errors_match_postgres() {
        let e = modify_error("pg_locks", ModifyKind::Insert);
        assert_eq!(e.sqlstate, sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE);
        assert_eq!(e.message, "cannot insert into view \"pg_locks\"");
        assert_eq!(
            e.detail.as_deref(),
            Some(
                "Views that do not select from a single table or view are not automatically updatable."
            )
        );
        assert!(
            e.hint
                .as_deref()
                .unwrap()
                .starts_with("To enable inserting into the view")
        );
        assert_eq!(
            modify_error("pg_locks", ModifyKind::Update).message,
            "cannot update view \"pg_locks\""
        );
        let d = modify_error("pg_locks", ModifyKind::Delete);
        assert_eq!(d.message, "cannot delete from view \"pg_locks\"");
        assert!(d.hint.as_deref().unwrap().contains("INSTEAD OF DELETE"));
    }

    #[test]
    fn not_a_table_errors() {
        let t = not_a_table_error("pg_locks", false);
        assert_eq!(t.sqlstate, sqlstate::WRONG_OBJECT_TYPE);
        assert_eq!(t.message, "\"pg_locks\" is not a table");
        assert!(t.hint.is_none());
        let d = not_a_table_error("pg_locks", true);
        assert_eq!(d.hint.as_deref(), Some("Use DROP VIEW to remove a view."));
    }
}

#[cfg(test)]
mod tests_roles {
    use super::*;
    use crate::catalog::roles::AuthidRow;
    use crate::executor::NullRuntime;

    fn row() -> AuthidRow {
        AuthidRow {
            oid: 16400,
            name: "alice".into(),
            superuser: false,
            inherit: true,
            create_role: true,
            create_db: false,
            can_login: true,
            replication: false,
            bypass_rls: false,
            conn_limit: 5,
            password: Some("SCRAM-SHA-256$4096:x".into()),
            valid_until: Some(i64::MAX),
        }
    }

    #[test]
    fn pg_roles_columns_match_pg17() {
        let names: Vec<&str> = PgRoles.columns().iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            [
                "rolname",
                "rolsuper",
                "rolinherit",
                "rolcreaterole",
                "rolcreatedb",
                "rolcanlogin",
                "rolreplication",
                "rolconnlimit",
                "rolpassword",
                "rolvaliduntil",
                "rolbypassrls",
                "rolconfig",
                "oid"
            ]
        );
        assert_eq!(PgRoles.oid(), 9810);
        assert_eq!(PgAuthMembers.oid(), 1261);
        assert_eq!(PgAuthMembers.columns().len(), 7);
    }

    #[test]
    fn password_is_masked_and_config_is_null() {
        let r = pg_roles_row(&row());
        assert_eq!(r.len(), PgRoles.columns().len());
        assert_eq!(r[8], Datum::Text("********".into()));
        assert_eq!(r[11], Datum::Null);
        assert_eq!(r[12], Datum::Oid(16400));
        let mut no_pw = row();
        no_pw.password = None;
        assert_eq!(pg_roles_row(&no_pw)[8], Datum::Text("********".into()));
    }

    #[test]
    fn registered_and_auth_members_is_empty() {
        assert!(find(9810).is_some() && find(1261).is_some());
        let s = SessionInfo {
            current_user: "u".into(),
            session_user: "u".into(),
            database: "d".into(),
            current_schema: None,
        };
        let ctx = VirtualCtx {
            runtime: &NullRuntime,
            session: &s,
        };
        assert!(PgAuthMembers.rows(&ctx).unwrap().is_empty());
        assert!(PgRoles.rows(&ctx).unwrap().is_empty());
    }
}
