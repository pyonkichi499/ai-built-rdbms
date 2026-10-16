//! `pg_authid` の読み書き、ロックの鍵、権限の述語（M5 AU-3。`m5/08` §4.4・§6.4）。
//!
//! 共有カタログなので `SharedCatalogStore` に別ファイルで impl する（`store.rs` は直接編集しない。08 C1）。

use super::rows::column_index;
use super::schema::oids;
use super::store::{RoleRow, SharedCatalogStore, snapshot_any};
use crate::backend::BackendId;
use crate::datadir::OidAllocator;
use crate::ddl::DdlCtx;
use crate::engine::RoleAuthInfo;
use crate::error::{Error, Result, sqlstate};
use crate::storage::{HeapTuple, SimpleHeapOp, TmResult, WriteCtx};
use crate::txn::lock::{LockManager, LockMode, LockScope, LockTag, WaitCtl};
use crate::txn::{RegisteredSnapshot, Snapshot};
use crate::types::{Datum, Oid, Row, Tid, oid};

/// `pg_authid` のクラス OID（ロックの `class`）。
pub const AUTHID_CLASS: Oid = 1260;
/// 名前のロックの `obj` に立てるビット（OID のロックと衝突させない）。
pub const ROLE_NAME_LOCK_FLAG: u64 = 1 << 63;
/// 2000-01-01 の UNIX エポックからのマイクロ秒。
pub const PG_EPOCH_UNIX_MICROS: i64 = 946_684_800_000_000;

/// 最大 100 万回 OID を試す（`CatalogStore::get_new_oid` と同じ）。
const MAX_OID_ATTEMPTS: u32 = 1_000_000;

/// `pg_authid` の 1 行（12 列すべて）。
#[allow(clippy::struct_excessive_bools)] // pg_authid の列に 1 対 1 で対応する。
#[derive(Clone, PartialEq, Eq)]
pub struct AuthidRow {
    pub oid: Oid,
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub create_role: bool,
    pub create_db: bool,
    pub can_login: bool,
    pub replication: bool,
    pub bypass_rls: bool,
    pub conn_limit: i32,
    /// `rolpassword`（SCRAM の秘密情報）。
    pub password: Option<String>,
    /// `rolvaliduntil`（2000-01-01 からのマイクロ秒。`i64::MAX` = infinity）。
    pub valid_until: Option<i64>,
}

impl std::fmt::Debug for AuthidRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthidRow")
            .field("oid", &self.oid)
            .field("name", &self.name)
            .field("superuser", &self.superuser)
            .field("inherit", &self.inherit)
            .field("create_role", &self.create_role)
            .field("create_db", &self.create_db)
            .field("can_login", &self.can_login)
            .field("replication", &self.replication)
            .field("bypass_rls", &self.bypass_rls)
            .field("conn_limit", &self.conn_limit)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("valid_until", &self.valid_until)
            .finish()
    }
}

fn bad(column: &str, d: &Datum) -> Error {
    Error::internal(format!("pg_authid.{column} has an unexpected value {d:?}"))
}

fn bool_at(r: &Row, column: &str) -> Result<bool> {
    match &r[column_index(oids::PG_AUTHID, column)] {
        Datum::Bool(v) => Ok(*v),
        other => Err(bad(column, other)),
    }
}

impl AuthidRow {
    pub fn from_row(r: &Row) -> Result<AuthidRow> {
        let at = |c: &str| &r[column_index(oids::PG_AUTHID, c)];
        let oid = match &r[0] {
            Datum::Oid(v) => *v,
            other => return Err(bad("oid", other)),
        };
        let name = match at("rolname") {
            Datum::Text(s) => s.clone(),
            other => return Err(bad("rolname", other)),
        };
        let conn_limit = match at("rolconnlimit") {
            Datum::Int4(v) => *v,
            other => return Err(bad("rolconnlimit", other)),
        };
        let password = match at("rolpassword") {
            Datum::Text(s) => Some(s.clone()),
            Datum::Null => None,
            other => return Err(bad("rolpassword", other)),
        };
        let valid_until = match at("rolvaliduntil") {
            Datum::TimestampTz(t) => Some(t.0),
            Datum::Null => None,
            other => return Err(bad("rolvaliduntil", other)),
        };
        Ok(AuthidRow {
            oid,
            name,
            superuser: bool_at(r, "rolsuper")?,
            inherit: bool_at(r, "rolinherit")?,
            create_role: bool_at(r, "rolcreaterole")?,
            create_db: bool_at(r, "rolcreatedb")?,
            can_login: bool_at(r, "rolcanlogin")?,
            replication: bool_at(r, "rolreplication")?,
            bypass_rls: bool_at(r, "rolbypassrls")?,
            conn_limit,
            password,
            valid_until,
        })
    }

    /// `pg_authid` の列の順の行。
    pub fn to_row(&self) -> Row {
        vec![
            Datum::Oid(self.oid),
            Datum::Text(self.name.clone()),
            Datum::Bool(self.superuser),
            Datum::Bool(self.inherit),
            Datum::Bool(self.create_role),
            Datum::Bool(self.create_db),
            Datum::Bool(self.can_login),
            Datum::Bool(self.replication),
            Datum::Bool(self.bypass_rls),
            Datum::Int4(self.conn_limit),
            self.password.clone().map_or(Datum::Null, Datum::Text),
            self.valid_until.map_or(Datum::Null, |t| {
                Datum::TimestampTz(yuzhu_datetime::TimestampTz(t))
            }),
        ]
    }

    /// セッションが持つ `RoleRow`（現状は M2 の 4 項目。`create_db` などは `RoleRow` が増えたら足す）。
    pub fn to_role_row(&self) -> RoleRow {
        RoleRow {
            oid: self.oid,
            name: self.name.clone(),
            can_login: self.can_login,
            superuser: self.superuser,
        }
    }

    /// `valid_until` を UNIX エポックからのマイクロ秒にする。`infinity` は `None`（期限なし）、
    /// `-infinity` は `Some(i64::MIN)`（常に期限切れ）。
    pub fn to_auth_info(&self) -> RoleAuthInfo {
        let valid_until = match self.valid_until {
            None | Some(i64::MAX) => None,
            Some(i64::MIN) => Some(i64::MIN),
            Some(pg) => Some(pg.saturating_add(PG_EPOCH_UNIX_MICROS)),
        };
        RoleAuthInfo {
            oid: self.oid,
            name: self.name.clone(),
            password: self.password.clone(),
            valid_until,
            can_login: self.can_login,
            conn_limit: self.conn_limit,
            superuser: self.superuser,
        }
    }
}

// ----- pg_authid の読み書き ---------------------------------------------------------

fn tm_error(r: TmResult, op: SimpleHeapOp) -> Result<()> {
    r.expect_simple(op)
}

impl SharedCatalogStore {
    fn scan_authid(
        &self,
        snap: &Snapshot,
        keep: &mut dyn FnMut(&Row) -> Result<bool>,
    ) -> Result<Vec<HeapTuple>> {
        let (_, rel) = self
            .rel(oids::PG_AUTHID)
            .ok_or_else(|| Error::internal("pg_authid is not a shared catalog"))?;
        let storage = self.storage();
        let mut scan = storage.begin_scan(rel, snap)?;
        let mut out = Vec::new();
        while let Some(t) = storage.scan_next(&mut scan)? {
            if keep(&t.row)? {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// 見える行すべて（OID の昇順）。
    pub fn authid_all(&self, snap: &Snapshot) -> Result<Vec<HeapTuple>> {
        let mut v = self.scan_authid(snap, &mut |_| Ok(true))?;
        v.sort_by_key(|t| match &t.row[0] {
            Datum::Oid(o) => *o,
            _ => 0,
        });
        Ok(v)
    }

    pub fn authid_by_name(&self, snap: &Snapshot, name: &str) -> Result<Option<(Tid, AuthidRow)>> {
        let idx = column_index(oids::PG_AUTHID, "rolname");
        let found = self.scan_authid(snap, &mut |r| {
            Ok(matches!(&r[idx], Datum::Text(s) if s == name))
        })?;
        found
            .first()
            .map(|t| Ok((t.tid, AuthidRow::from_row(&t.row)?)))
            .transpose()
    }

    pub fn authid_by_oid(&self, snap: &Snapshot, oid: Oid) -> Result<Option<(Tid, AuthidRow)>> {
        let found = self.scan_authid(snap, &mut |r| Ok(r[0] == Datum::Oid(oid)))?;
        found
            .first()
            .map(|t| Ok((t.tid, AuthidRow::from_row(&t.row)?)))
            .transpose()
    }

    /// `OidAllocator` から採り、全版が見えるスナップショットで重複を避ける
    /// （`CatalogStore::get_new_oid` は共有カタログを受け付けない）。
    pub fn new_role_oid(&self, alloc: &OidAllocator) -> Result<Oid> {
        let any = snapshot_any();
        for _ in 0..MAX_OID_ATTEMPTS {
            let candidate = alloc.next_raw()?;
            if candidate < oid::FIRST_NORMAL_OBJECT_ID {
                continue;
            }
            let taken = self.scan_authid(&any, &mut |r| Ok(r[0] == Datum::Oid(candidate)))?;
            if taken.is_empty() {
                return Ok(candidate);
            }
        }
        Err(Error::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "could not find a free OID in pg_authid",
        ))
    }

    pub fn insert_authid(&self, w: &WriteCtx, row: &AuthidRow) -> Result<()> {
        let (_, rel) = self
            .rel(oids::PG_AUTHID)
            .ok_or_else(|| Error::internal("pg_authid is not a shared catalog"))?;
        self.storage().insert(rel, w, &row.to_row())?;
        Ok(())
    }

    /// `TmResult` が `Ok` 以外なら `XX000 tuple concurrently updated`。
    pub fn update_authid(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        tid: Tid,
        new: &AuthidRow,
    ) -> Result<()> {
        let (_, rel) = self
            .rel(oids::PG_AUTHID)
            .ok_or_else(|| Error::internal("pg_authid is not a shared catalog"))?;
        let out = self
            .storage()
            .update(rel, w, snap, tid, &new.to_row(), None, None)?;
        tm_error(out.result, SimpleHeapOp::Update)
    }

    /// 非 `Ok` は `XX000 tuple concurrently deleted`。
    pub fn delete_authid(&self, w: &WriteCtx, snap: &Snapshot, tid: Tid) -> Result<()> {
        let (_, rel) = self
            .rel(oids::PG_AUTHID)
            .ok_or_else(|| Error::internal("pg_authid is not a shared catalog"))?;
        let r = self.storage().delete(rel, w, snap, tid, None, None)?;
        tm_error(r, SimpleHeapOp::Delete)
    }
}

// ----- ロックの鍵 ---------------------------------------------------------------------

/// FNV-1a（64 ビット）。
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// ロール名のロック（`CREATE ROLE` の重複検査と `ALTER ... RENAME` を直列にする）。
pub fn role_name_lock(name: &str) -> LockTag {
    LockTag::Object {
        db: 0,
        class: AUTHID_CLASS,
        obj: fnv1a64(name.as_bytes()) | ROLE_NAME_LOCK_FLAG,
    }
}

/// ロールの OID のロック（所有者にするロールを `DROP ROLE` から守る）。
pub fn role_oid_lock(oid: Oid) -> LockTag {
    LockTag::Object {
        db: 0,
        class: AUTHID_CLASS,
        obj: u64::from(oid),
    }
}

/// 所有者にするロール（`CREATE TABLE`・`CREATE DATABASE` の所有者）を `DROP ROLE` から守る。
/// `AccessShare`・トランザクションスコープ。取った後で、呼び出し側が `authid_by_oid` で存在を確かめ直す。
pub fn lock_role_shared(
    locks: &LockManager,
    backend: BackendId,
    role: Oid,
    wait: &WaitCtl<'_>,
) -> Result<()> {
    locks.acquire(
        backend,
        role_oid_lock(role),
        LockMode::AccessShare,
        LockScope::Transaction,
        wait,
    )
}

// ----- 権限の述語 ---------------------------------------------------------------------

pub fn has_createrole(r: &AuthidRow) -> bool {
    r.superuser || r.create_role
}

pub fn has_createdb(r: &AuthidRow) -> bool {
    r.superuser || r.create_db
}

pub fn has_replication(r: &AuthidRow) -> bool {
    r.superuser || r.replication
}

pub fn has_bypassrls(r: &AuthidRow) -> bool {
    r.superuser || r.bypass_rls
}

/// 最新のカタログ用スナップショット（D12。登録する・短命）。待った後に取り直すために使う。
pub fn latest_snapshot(ctx: &DdlCtx<'_>) -> RegisteredSnapshot {
    ctx.cluster
        .txn_manager()
        .take_snapshot(ctx.txn.xid, ctx.txn.cid)
}

/// 現在のロール（`ctx.role.oid`）の最新の行。行が無ければ（他のセッションが DROP した）属性なしのロール。
pub fn current_role(ctx: &DdlCtx<'_>) -> Result<AuthidRow> {
    let snap = latest_snapshot(ctx);
    Ok(match ctx.db.shared.authid_by_oid(&snap, ctx.role.oid)? {
        Some((_, r)) => r,
        None => AuthidRow {
            oid: ctx.role.oid,
            name: ctx.role.name.clone(),
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
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AuthidRow {
        AuthidRow {
            oid: 16384,
            name: "alice".into(),
            superuser: false,
            inherit: true,
            create_role: true,
            create_db: false,
            can_login: true,
            replication: false,
            bypass_rls: false,
            conn_limit: 5,
            password: Some("SCRAM-SHA-256$4096:aa$bb:cc".into()),
            valid_until: Some(1_000),
        }
    }

    #[test]
    fn row_round_trip() {
        let r = sample();
        assert_eq!(AuthidRow::from_row(&r.to_row()).unwrap(), r);
        let mut n = sample();
        n.password = None;
        n.valid_until = None;
        assert_eq!(AuthidRow::from_row(&n.to_row()).unwrap(), n);
        assert_eq!(r.to_row().len(), 12);
    }

    #[test]
    fn debug_hides_password() {
        let s = format!("{:?}", sample());
        assert!(!s.contains("SCRAM"));
        assert!(s.contains("<redacted>"));
    }

    #[test]
    fn valid_until_conversion() {
        let mut r = sample();
        r.valid_until = None;
        assert_eq!(r.to_auth_info().valid_until, None);
        r.valid_until = Some(i64::MAX);
        assert_eq!(r.to_auth_info().valid_until, None);
        r.valid_until = Some(i64::MIN);
        assert_eq!(r.to_auth_info().valid_until, Some(i64::MIN));
        r.valid_until = Some(0);
        assert_eq!(r.to_auth_info().valid_until, Some(946_684_800_000_000));
    }

    #[test]
    fn predicates_treat_superuser_as_all() {
        let mut r = sample();
        r.create_role = false;
        assert!(!has_createrole(&r) && !has_createdb(&r));
        assert!(!has_replication(&r) && !has_bypassrls(&r));
        r.superuser = true;
        assert!(has_createrole(&r) && has_createdb(&r));
        assert!(has_replication(&r) && has_bypassrls(&r));
    }

    #[test]
    fn lock_keys() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        let LockTag::Object { db, class, obj } = role_name_lock("x") else {
            panic!()
        };
        assert_eq!((db, class), (0, 1260));
        assert_ne!(obj & ROLE_NAME_LOCK_FLAG, 0);
        assert_eq!(
            role_oid_lock(16384),
            LockTag::Object {
                db: 0,
                class: 1260,
                obj: 16384
            }
        );
        assert_ne!(role_name_lock("a"), role_name_lock("b"));
    }
}
