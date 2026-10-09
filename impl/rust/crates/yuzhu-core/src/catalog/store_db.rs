//! `pg_database` の読み取り側（DB-1。09 章 §4.2）。書き込み側は DB-2 が足す。
//!
//! `DatabaseRow` / `RoleRow` に `conn_limit` が入るまでの間、接続数の上限は
//! [`SharedCatalogStore::database_conn_limit`] / [`SharedCatalogStore::role_conn_limit`] で読む。

use super::rows::column_index;
use super::schema::oids;
use super::store::{DatabaseRow, SharedCatalogStore};
use crate::error::{Error, Result};
use crate::storage::HeapTuple;
use crate::txn::Snapshot;
use crate::types::{Datum, Oid};

fn bad(catalog: &str, column: &str, d: &Datum) -> Error {
    Error::internal(format!("{catalog}.{column} has an unexpected value {d:?}"))
}

fn oid_of(d: &Datum, catalog: &str, column: &str) -> Result<Oid> {
    match d {
        Datum::Oid(v) => Ok(*v),
        other => Err(bad(catalog, column, other)),
    }
}

fn i32_of(d: &Datum, catalog: &str, column: &str) -> Result<i32> {
    match d {
        Datum::Int4(v) => Ok(*v),
        other => Err(bad(catalog, column, other)),
    }
}

fn bool_of(d: &Datum, catalog: &str, column: &str) -> Result<bool> {
    match d {
        Datum::Bool(v) => Ok(*v),
        other => Err(bad(catalog, column, other)),
    }
}

fn text_of(d: &Datum, catalog: &str, column: &str) -> Result<String> {
    match d {
        Datum::Text(s) => Ok(s.clone()),
        other => Err(bad(catalog, column, other)),
    }
}

fn database_row(t: &HeapTuple) -> Result<DatabaseRow> {
    let col = |n: &str| column_index(oids::PG_DATABASE, n);
    let r = &t.row;
    Ok(DatabaseRow {
        oid: oid_of(&r[0], "pg_database", "oid")?,
        name: text_of(&r[col("datname")], "pg_database", "datname")?,
        allow_conn: bool_of(&r[col("datallowconn")], "pg_database", "datallowconn")?,
        is_template: bool_of(&r[col("datistemplate")], "pg_database", "datistemplate")?,
    })
}

impl SharedCatalogStore {
    /// 全走査して、`keep` を通した行を返す（`pg_database` / `pg_authid` は数行）。
    fn scan_shared(
        &self,
        catalog_oid: Oid,
        snap: &Snapshot,
        keep: &mut dyn FnMut(&HeapTuple) -> Result<bool>,
    ) -> Result<Vec<HeapTuple>> {
        let (_, rel) = self
            .rel(catalog_oid)
            .ok_or_else(|| Error::internal(format!("{catalog_oid} is not a shared catalog")))?;
        let storage = self.storage();
        let mut scan = storage.begin_scan(rel, snap)?;
        let mut out = Vec::new();
        while let Some(t) = storage.scan_next(&mut scan)? {
            if keep(&t)? {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// OID でデータベースを引く。
    pub fn database_by_oid(&self, snap: &Snapshot, oid: Oid) -> Result<Option<DatabaseRow>> {
        let found = self.scan_shared(oids::PG_DATABASE, snap, &mut |t| {
            Ok(oid_of(&t.row[0], "pg_database", "oid")? == oid)
        })?;
        found.first().map(database_row).transpose()
    }

    /// すべての行。oid の昇順。
    pub fn databases(&self, snap: &Snapshot) -> Result<Vec<DatabaseRow>> {
        let found = self.scan_shared(oids::PG_DATABASE, snap, &mut |_| Ok(true))?;
        let mut rows = found.iter().map(database_row).collect::<Result<Vec<_>>>()?;
        rows.sort_by_key(|r| r.oid);
        Ok(rows)
    }

    /// `datconnlimit`（-1 = 無制限）。行が無ければ `None`。
    pub fn database_conn_limit(&self, snap: &Snapshot, oid: Oid) -> Result<Option<i32>> {
        let idx = column_index(oids::PG_DATABASE, "datconnlimit");
        let found = self.scan_shared(oids::PG_DATABASE, snap, &mut |t| {
            Ok(oid_of(&t.row[0], "pg_database", "oid")? == oid)
        })?;
        found
            .first()
            .map(|t| i32_of(&t.row[idx], "pg_database", "datconnlimit"))
            .transpose()
    }

    /// `rolconnlimit`（-1 = 無制限）。行が無ければ `None`。
    pub fn role_conn_limit(&self, snap: &Snapshot, oid: Oid) -> Result<Option<i32>> {
        let idx = column_index(oids::PG_AUTHID, "rolconnlimit");
        let found = self.scan_shared(oids::PG_AUTHID, snap, &mut |t| {
            Ok(oid_of(&t.row[0], "pg_authid", "oid")? == oid)
        })?;
        found
            .first()
            .map(|t| i32_of(&t.row[idx], "pg_authid", "rolconnlimit"))
            .transpose()
    }
}

// ===== DB-2: 書き込み側（09 章 §4.2、§3.2） =====

use super::rows::RowBuilder;
use crate::datadir::OidAllocator;
use crate::storage::vfs::Vfs;
use crate::storage::{SimpleHeapOp, WriteCtx};
use crate::txn::Xid;
use crate::types::{Row, oid};

/// 最大 100 万回 OID を試す（`CatalogStore::get_new_oid` と同じ）。
const MAX_OID_ATTEMPTS: u32 = 1_000_000;

/// `pg_database` の行のうち、`DatabaseRow` に無い項目（所有者・接続数の上限・`datminmxid`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseInfo {
    pub row: DatabaseRow,
    pub owner: Oid,
    pub conn_limit: i32,
    pub min_mxid: u32,
}

/// `CREATE DATABASE` が書く行（`pg_database` と `yz_datxid` の 2 行）。
#[derive(Clone, Debug)]
pub struct NewDatabase {
    pub oid: Oid,
    pub name: String,
    pub owner: Oid,
    pub is_template: bool,
    pub allow_conn: bool,
    pub conn_limit: i32,
    /// `frozen8` の下位 32 ビット。
    pub frozen_xid: u32,
    /// テンプレートの値のコピー。
    pub min_mxid: u32,
    /// `min(テンプレートの yz_datxid の値, oldest_xmin())`（03 VC-D13）。
    pub frozen8: Xid,
}

/// `ALTER DATABASE` の 3 つのフラグ。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DatabaseFlagUpdate {
    pub allow_conn: Option<bool>,
    pub is_template: Option<bool>,
    pub conn_limit: Option<i32>,
}

fn xid32_of(d: &Datum, catalog: &str, column: &str) -> Result<u32> {
    match d {
        Datum::Xid(v) => Ok(*v),
        other => Err(bad(catalog, column, other)),
    }
}

fn database_info(t: &HeapTuple) -> Result<DatabaseInfo> {
    let col = |n: &str| column_index(oids::PG_DATABASE, n);
    let r = &t.row;
    Ok(DatabaseInfo {
        row: database_row(t)?,
        owner: oid_of(&r[col("datdba")], "pg_database", "datdba")?,
        conn_limit: i32_of(&r[col("datconnlimit")], "pg_database", "datconnlimit")?,
        min_mxid: xid32_of(&r[col("datminmxid")], "pg_database", "datminmxid")?,
    })
}

impl SharedCatalogStore {
    fn database_tuples(
        &self,
        snap: &Snapshot,
        keep: &mut dyn FnMut(&HeapTuple) -> Result<bool>,
    ) -> Result<Vec<HeapTuple>> {
        self.scan_shared(oids::PG_DATABASE, snap, keep)
    }

    /// 名前でデータベースを引く（所有者などつき）。
    pub fn database_info_by_name(
        &self,
        snap: &Snapshot,
        name: &str,
    ) -> Result<Option<DatabaseInfo>> {
        let idx = column_index(oids::PG_DATABASE, "datname");
        let found = self.database_tuples(snap, &mut |t| {
            Ok(text_of(&t.row[idx], "pg_database", "datname")? == name)
        })?;
        found.first().map(database_info).transpose()
    }

    /// OID でデータベースを引く（所有者などつき）。
    pub fn database_info_by_oid(&self, snap: &Snapshot, oid: Oid) -> Result<Option<DatabaseInfo>> {
        let found = self.database_tuples(snap, &mut |t| {
            Ok(oid_of(&t.row[0], "pg_database", "oid")? == oid)
        })?;
        found.first().map(database_info).transpose()
    }

    /// 所有しているデータベースの名前（`DROP ROLE` の DETAIL 用）。名前の昇順。
    pub fn databases_owned_by(&self, snap: &Snapshot, role: Oid) -> Result<Vec<String>> {
        let idx = column_index(oids::PG_DATABASE, "datdba");
        let found = self.database_tuples(snap, &mut |t| {
            Ok(oid_of(&t.row[idx], "pg_database", "datdba")? == role)
        })?;
        let mut names = found
            .iter()
            .map(|t| database_row(t).map(|r| r.name))
            .collect::<Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    /// `pg_database` に使える OID を採る。`OidAllocator` から採り、全版が見えるスナップショットで行との
    /// 重複を避け、`base/<oid>` が既にあるもの（孤児。DB-D17）も避ける。
    /// `CatalogStore::get_new_oid` は共有カタログを受け付けない。
    pub fn get_new_database_oid(&self, alloc: &OidAllocator, vfs: &dyn Vfs) -> Result<Oid> {
        let any = super::store::snapshot_any();
        for _ in 0..MAX_OID_ATTEMPTS {
            let candidate = alloc.next_raw()?;
            if candidate < oid::FIRST_NORMAL_OBJECT_ID {
                continue;
            }
            let taken = self.database_tuples(&any, &mut |t| {
                Ok(oid_of(&t.row[0], "pg_database", "oid")? == candidate)
            })?;
            if !taken.is_empty() {
                continue;
            }
            let dir = crate::dbase::database_dir(candidate);
            let exists = vfs.exists(&dir).map_err(|e| {
                Error::new(
                    crate::error::sqlstate::IO_ERROR,
                    format!("could not stat \"{}\": {e}", dir.display()),
                )
            })?;
            if !exists {
                return Ok(candidate);
            }
        }
        Err(Error::new(
            crate::error::sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "could not find a free OID in pg_database",
        ))
    }

    fn database_rel(&self) -> Result<&crate::storage::RelHandle> {
        self.rel(oids::PG_DATABASE)
            .map(|(_, rel)| rel)
            .ok_or_else(|| Error::internal("pg_database is not a shared catalog"))
    }

    /// `pg_database` と `yz_datxid` に 1 行ずつ挿入する（同じ `WriteCtx`）。
    pub fn insert_database(&self, w: &WriteCtx, row: &NewDatabase) -> Result<()> {
        let values = RowBuilder::new(oids::PG_DATABASE)
            .set("oid", Datum::Oid(row.oid))
            .set("datname", Datum::Text(row.name.clone()))
            .set("datdba", Datum::Oid(row.owner))
            .set("encoding", Datum::Int4(6))
            .set("datlocprovider", Datum::Char(b'c'))
            .set("datistemplate", Datum::Bool(row.is_template))
            .set("datallowconn", Datum::Bool(row.allow_conn))
            .set("dathasloginevt", Datum::Bool(false))
            .set("datconnlimit", Datum::Int4(row.conn_limit))
            .set("datfrozenxid", Datum::Xid(row.frozen_xid))
            .set("datminmxid", Datum::Xid(row.min_mxid))
            .set("dattablespace", Datum::Oid(1663))
            .set("datcollate", Datum::Text("C".to_owned()))
            .set("datctype", Datum::Text("C".to_owned()))
            .build();
        self.storage().insert(self.database_rel()?, w, &values)?;
        self.insert_datxid(w, row.oid, row.frozen8)
    }

    /// 両方の行を削除する。別のトランザクションが同じ行を更新中なら `XX000 tuple concurrently updated`
    /// （待たない）。
    pub fn delete_database(&self, w: &WriteCtx, snap: &Snapshot, oid: Oid) -> Result<()> {
        let found = self.database_tuples(snap, &mut |t| {
            Ok(oid_of(&t.row[0], "pg_database", "oid")? == oid)
        })?;
        let Some(t) = found.first() else {
            return Err(Error::new(
                crate::error::sqlstate::INVALID_CATALOG_NAME,
                format!("database with OID {oid} does not exist"),
            ));
        };
        self.storage()
            .delete(self.database_rel()?, w, snap, t.tid, None, None)?
            .expect_simple(SimpleHeapOp::Delete)?;
        self.delete_datxid(w, snap, oid)
    }

    /// `ALTER DATABASE` の 3 つのフラグ（MVCC の更新）。
    pub fn update_database_flags(
        &self,
        w: &WriteCtx,
        snap: &Snapshot,
        oid: Oid,
        f: &DatabaseFlagUpdate,
    ) -> Result<()> {
        let found = self.database_tuples(snap, &mut |t| {
            Ok(oid_of(&t.row[0], "pg_database", "oid")? == oid)
        })?;
        let Some(t) = found.first() else {
            return Err(Error::new(
                crate::error::sqlstate::INVALID_CATALOG_NAME,
                format!("database with OID {oid} does not exist"),
            ));
        };
        let mut new: Row = t.row.clone();
        if let Some(v) = f.allow_conn {
            new[column_index(oids::PG_DATABASE, "datallowconn")] = Datum::Bool(v);
        }
        if let Some(v) = f.is_template {
            new[column_index(oids::PG_DATABASE, "datistemplate")] = Datum::Bool(v);
        }
        if let Some(v) = f.conn_limit {
            new[column_index(oids::PG_DATABASE, "datconnlimit")] = Datum::Int4(v);
        }
        let out = self
            .storage()
            .update(self.database_rel()?, w, snap, t.tid, &new, None, None)?;
        out.result.expect_simple(SimpleHeapOp::Update)
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;
    use crate::testing::TestCluster;

    fn insert(tc: &TestCluster, name: &str, owner: Oid) -> Oid {
        let c = &tc.cluster;
        let (db, _) = c.connect_simple("postgres", "postgres").unwrap();
        let shared = &db.shared;
        c.locks()
            .register_backend(crate::backend::BackendId(900), 1);
        let new_oid = shared
            .get_new_database_oid(c.oid_allocator(), &*c.stack().vfs)
            .unwrap();
        let (xid, guard) = c
            .txn_manager()
            .assign_xid(crate::backend::BackendId(900))
            .unwrap();
        let w = WriteCtx { xid, cid: 0 };
        shared
            .insert_database(
                &w,
                &NewDatabase {
                    oid: new_oid,
                    name: name.to_owned(),
                    owner,
                    is_template: false,
                    allow_conn: true,
                    conn_limit: 7,
                    frozen_xid: 3,
                    min_mxid: 1,
                    frozen8: Xid(3),
                },
            )
            .unwrap();
        c.txn_manager().commit(xid, &[]).unwrap();
        drop(guard);
        new_oid
    }

    #[test]
    fn insert_read_update_delete() {
        let tc = TestCluster::new();
        let c = &tc.cluster;
        let (db, _) = c.connect_simple("postgres", "postgres").unwrap();
        let shared = &db.shared;
        let oid = insert(&tc, "m5db_a", 10);
        let snap = c.txn_manager().take_snapshot(None, 0);
        let info = shared
            .database_info_by_name(&snap, "m5db_a")
            .unwrap()
            .unwrap();
        assert_eq!(
            (info.row.oid, info.owner, info.conn_limit, info.min_mxid),
            (oid, 10, 7, 1)
        );
        assert!(oid >= oid::FIRST_NORMAL_OBJECT_ID);
        assert_eq!(shared.databases_owned_by(&snap, 10).unwrap().len(), 4);
        assert_eq!(shared.datxid(&snap, oid).unwrap(), Some(Xid(3)));
        drop(snap);

        let (xid, guard) = c
            .txn_manager()
            .assign_xid(crate::backend::BackendId(900))
            .unwrap();
        let w = WriteCtx { xid, cid: 0 };
        let snap = c.txn_manager().take_snapshot(None, 0);
        shared
            .update_database_flags(
                &w,
                &snap,
                oid,
                &DatabaseFlagUpdate {
                    allow_conn: Some(false),
                    is_template: Some(true),
                    conn_limit: Some(2),
                },
            )
            .unwrap();
        c.txn_manager().commit(xid, &[]).unwrap();
        drop(guard);
        drop(snap);
        let snap = c.txn_manager().take_snapshot(None, 0);
        let info = shared.database_info_by_oid(&snap, oid).unwrap().unwrap();
        assert_eq!(
            (info.row.allow_conn, info.row.is_template, info.conn_limit),
            (false, true, 2)
        );
        drop(snap);

        let (xid, guard) = c
            .txn_manager()
            .assign_xid(crate::backend::BackendId(900))
            .unwrap();
        let w = WriteCtx { xid, cid: 0 };
        let snap = c.txn_manager().take_snapshot(None, 0);
        shared.delete_database(&w, &snap, oid).unwrap();
        c.txn_manager().commit(xid, &[]).unwrap();
        drop(guard);
        drop(snap);
        let snap = c.txn_manager().take_snapshot(None, 0);
        assert!(shared.database_info_by_oid(&snap, oid).unwrap().is_none());
        assert_eq!(shared.datxid(&snap, oid).unwrap(), None);
    }

    #[test]
    fn new_oid_skips_orphan_directory() {
        let tc = TestCluster::new();
        let c = &tc.cluster;
        let (db, _) = c.connect_simple("postgres", "postgres").unwrap();
        let shared = &db.shared;
        let first = shared
            .get_new_database_oid(c.oid_allocator(), &*c.stack().vfs)
            .unwrap();
        c.stack()
            .vfs
            .create_dir(&crate::dbase::database_dir(first + 1))
            .unwrap();
        let second = shared
            .get_new_database_oid(c.oid_allocator(), &*c.stack().vfs)
            .unwrap();
        assert!(
            second > first + 1,
            "{second} should skip the orphan {}",
            first + 1
        );
    }
}
