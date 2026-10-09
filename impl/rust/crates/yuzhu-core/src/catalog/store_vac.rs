//! VACUUM のためのカタログの読み書き（03 §4.3。VC-3a が `yz_relxid` / `yz_datxid` の通常のアクセサと
//! VACUUM の対象の列挙を書く。その場の上書き（`set_relstats_inplace` / `set_datxid_inplace`）は
//! `TableStore::inplace_update`（03 §5.8）の持ち主が足す）。
//!
//! `store.rs` の子モジュールとして読み込まれる（`#[path]`）ので、`CatalogStore` の非公開の項目を使える。

use std::collections::HashMap;

use super::{
    CatalogStore, DEFAULTTABLESPACE_OID, Oid, RelFileLocator, RelFileNumber, SharedCatalogStore,
    as_char, as_i32, as_i64, as_oid, as_text, column_index, oids, schema,
};
use crate::error::{Error, Result};
use crate::storage::SimpleHeapOp;
use crate::txn::{Snapshot, Xid};
use crate::types::Datum;

/// `VACUUM` が扱うリレーション 1 つ分の `pg_class` の行から取った情報。
#[derive(Clone, Debug, PartialEq)]
pub struct VacRelInfo {
    pub oid: Oid,
    pub name: String,
    pub schema: String,
    pub relkind: char,
    pub is_shared: bool,
    pub locator: RelFileLocator,
    pub relpages: i32,
    pub reltuples: f32,
}

/// 全テーブルの `relfrozenxid8` の最小値（03 §3.4）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelxidMin {
    pub min: Xid,
    /// `yz_relxid` に行がないテーブルの名前。最古の境界（`Xid::FIRST_NORMAL`）として扱う（fail-closed）。
    pub missing: Vec<String>,
}

/// 行がないものを最古として扱うときの値。
const FAIL_CLOSED_XID: Xid = Xid(3);

fn xid_of_i64(v: i64, catalog: &str, column: &str) -> Result<Xid> {
    u64::try_from(v).map(Xid).map_err(|_| {
        Error::internal(format!(
            "{catalog}.{column} has a negative transaction ID {v}"
        ))
    })
}

fn i64_of_xid(x: Xid) -> Result<i64> {
    i64::try_from(x.0).map_err(|_| Error::internal("transaction ID does not fit in 64-bit signed"))
}

fn as_f32(d: &Datum, catalog: &str, column: &str) -> Result<f32> {
    match d {
        Datum::Float4(v) => Ok(*v),
        other => Err(super::bad_value(catalog, column, other)),
    }
}

impl CatalogStore {
    /// `pg_namespace` の OID から名前への対応。
    fn namespace_names(&self, snap: &Snapshot) -> Result<HashMap<Oid, String>> {
        let name_col = column_index(oids::PG_NAMESPACE, "nspname");
        let rows = self.scan(snap, oids::PG_NAMESPACE, |_| Ok(true))?;
        rows.iter()
            .map(|t| {
                Ok((
                    as_oid(&t.row[0], "pg_namespace", "oid")?,
                    as_text(&t.row[name_col], "pg_namespace", "nspname")?.to_owned(),
                ))
            })
            .collect()
    }

    fn vac_rel_info(&self, row: &[Datum], namespaces: &HashMap<Oid, String>) -> Result<VacRelInfo> {
        let col = |n: &str| column_index(oids::PG_CLASS, n);
        let oid = as_oid(&row[0], "pg_class", "oid")?;
        let namespace = as_oid(&row[col("relnamespace")], "pg_class", "relnamespace")?;
        let tablespace = as_oid(&row[col("reltablespace")], "pg_class", "reltablespace")?;
        let relfilenode = as_oid(&row[col("relfilenode")], "pg_class", "relfilenode")?;
        let is_shared = super::as_bool(&row[col("relisshared")], "pg_class", "relisshared")?;
        // 写像されたカタログは `relfilenode = 0`。ファイルは固定の場所にある。
        let locator = match schema::catalog_def(oid) {
            Some(def) => def.locator(self.db_oid),
            None => RelFileLocator {
                spc_oid: if tablespace == 0 {
                    DEFAULTTABLESPACE_OID
                } else {
                    tablespace
                },
                db_oid: if is_shared { 0 } else { self.db_oid },
                rel_number: RelFileNumber(relfilenode),
            },
        };
        Ok(VacRelInfo {
            oid,
            name: as_text(&row[col("relname")], "pg_class", "relname")?.to_owned(),
            schema: namespaces
                .get(&namespace)
                .cloned()
                .unwrap_or_else(|| namespace.to_string()),
            relkind: char::from(as_char(&row[col("relkind")], "pg_class", "relkind")?),
            is_shared,
            locator,
            relpages: as_i32(&row[col("relpages")], "pg_class", "relpages")?,
            reltuples: as_f32(&row[col("reltuples")], "pg_class", "reltuples")?,
        })
    }

    /// `relkind = 'r'` の全リレーション（共有カタログを含む）。OID の昇順。引数なしの `VACUUM` の対象。
    pub fn list_vacuum_relations(&self, snap: &Snapshot) -> Result<Vec<VacRelInfo>> {
        let namespaces = self.namespace_names(snap)?;
        let kind = column_index(oids::PG_CLASS, "relkind");
        let found = self.scan(snap, oids::PG_CLASS, |r| {
            Ok(as_char(&r[kind], "pg_class", "relkind")? == b'r')
        })?;
        let mut out = found
            .iter()
            .map(|t| self.vac_rel_info(&t.row, &namespaces))
            .collect::<Result<Vec<_>>>()?;
        out.sort_by_key(|r| r.oid);
        Ok(out)
    }

    /// `relkind` を問わず 1 件。表でないリレーションに WARNING を出すときに使う。消えていれば `None`。
    pub fn vacuum_relation_info(&self, snap: &Snapshot, oid: Oid) -> Result<Option<VacRelInfo>> {
        let found = self.scan(snap, oids::PG_CLASS, |r| {
            Ok(as_oid(&r[0], "pg_class", "oid")? == oid)
        })?;
        let Some(t) = found.first() else {
            return Ok(None);
        };
        let namespaces = self.namespace_names(snap)?;
        self.vac_rel_info(&t.row, &namespaces).map(Some)
    }

    /// `yz_relxid` の `relid` の行の `relfrozenxid8`。行がなければ `None`。
    pub fn relxid(&self, snap: &Snapshot, relid: Oid) -> Result<Option<Xid>> {
        let frozen = column_index(oids::YZ_RELXID, "relfrozenxid8");
        let found = self.scan(snap, oids::YZ_RELXID, |r| {
            Ok(as_oid(&r[0], "yz_relxid", "relid")? == relid)
        })?;
        found
            .first()
            .map(|t| {
                xid_of_i64(
                    as_i64(&t.row[frozen], "yz_relxid", "relfrozenxid8")?,
                    "yz_relxid",
                    "relfrozenxid8",
                )
            })
            .transpose()
    }

    /// 全テーブル（`relkind = 'r'`）の `relfrozenxid8` の最小値。`yz_relxid` に行がないテーブルは
    /// `Xid(3)` とみなし、名前を `missing` に返す（fail-closed。境界を進めない側に倒す）。
    /// テーブルが 1 つもなければ `min` は `Xid(u64::MAX)`（呼び出し側が `min()` で畳む）。
    pub fn min_relxid(&self, snap: &Snapshot) -> Result<RelxidMin> {
        let frozen = column_index(oids::YZ_RELXID, "relfrozenxid8");
        let by_relid: HashMap<Oid, Xid> = self
            .scan(snap, oids::YZ_RELXID, |_| Ok(true))?
            .iter()
            .map(|t| {
                Ok((
                    as_oid(&t.row[0], "yz_relxid", "relid")?,
                    xid_of_i64(
                        as_i64(&t.row[frozen], "yz_relxid", "relfrozenxid8")?,
                        "yz_relxid",
                        "relfrozenxid8",
                    )?,
                ))
            })
            .collect::<Result<_>>()?;
        let mut min = Xid(u64::MAX);
        let mut missing = Vec::new();
        for rel in self.list_vacuum_relations(snap)? {
            if let Some(x) = by_relid.get(&rel.oid) {
                min = min.min(*x);
            } else {
                min = min.min(FAIL_CLOSED_XID);
                missing.push(rel.name);
            }
        }
        Ok(RelxidMin { min, missing })
    }

    /// `yz_relxid` の `relid` の行を通常の UPDATE で書き換える（TRUNCATE。前のコマンドが作った行だけ）。
    pub fn update_relxid(
        &self,
        w: &crate::storage::WriteCtx,
        snap: &Snapshot,
        relid: Oid,
        xid: Xid,
    ) -> Result<()> {
        let frozen_col = column_index(oids::YZ_RELXID, "relfrozenxid8");
        let value = i64_of_xid(xid)?;
        self.update_row_where(
            w,
            snap,
            oids::YZ_RELXID,
            |r| Ok(as_oid(&r[0], "yz_relxid", "relid")? == relid),
            |old| {
                let mut row = old.clone();
                row[frozen_col] = Datum::Int8(value);
                row
            },
        )
    }
}

impl SharedCatalogStore {
    /// `yz_datxid` の `db` の行の `datfrozenxid8`。行がなければ `None`。
    pub fn datxid(&self, snap: &Snapshot, db: Oid) -> Result<Option<Xid>> {
        let frozen = column_index(oids::YZ_DATXID, "datfrozenxid8");
        let found = self.scan_datxid(snap, |r| Ok(as_oid(&r[0], "yz_datxid", "datid")? == db))?;
        found
            .first()
            .map(|t| {
                xid_of_i64(
                    as_i64(&t.row[frozen], "yz_datxid", "datfrozenxid8")?,
                    "yz_datxid",
                    "datfrozenxid8",
                )
            })
            .transpose()
    }

    /// 全データベースの `datfrozenxid8` の最小値。新品のデータベースの番兵（`i64::MAX`）は大きいので最小に
    /// 影響しない。`pg_database` にあって `yz_datxid` に行がないデータベースは `Xid(3)` とみなす（fail-closed）。
    pub fn min_datxid(&self, snap: &Snapshot) -> Result<Xid> {
        let frozen = column_index(oids::YZ_DATXID, "datfrozenxid8");
        let by_db: HashMap<Oid, Xid> = self
            .scan_datxid(snap, |_| Ok(true))?
            .iter()
            .map(|t| {
                Ok((
                    as_oid(&t.row[0], "yz_datxid", "datid")?,
                    xid_of_i64(
                        as_i64(&t.row[frozen], "yz_datxid", "datfrozenxid8")?,
                        "yz_datxid",
                        "datfrozenxid8",
                    )?,
                ))
            })
            .collect::<Result<_>>()?;
        let (db_rel, db_storage) = (&self.database.1, &self.storage);
        let mut scan = db_storage.begin_scan(db_rel, snap)?;
        let mut min = Xid(u64::MAX);
        while let Some(t) = db_storage.scan_next(&mut scan)? {
            let oid = as_oid(&t.row[0], "pg_database", "oid")?;
            min = min.min(by_db.get(&oid).copied().unwrap_or(FAIL_CLOSED_XID));
        }
        Ok(min)
    }

    /// CREATE DATABASE が `pg_database` の行と同じ `WriteCtx` で呼ぶ（09 章）。
    pub fn insert_datxid(&self, w: &crate::storage::WriteCtx, db: Oid, v: Xid) -> Result<()> {
        let row = super::rows::RowBuilder::new(oids::YZ_DATXID)
            .set("datid", Datum::Oid(db))
            .set("datfrozenxid8", Datum::Int8(i64_of_xid(v)?))
            .build();
        self.storage.insert(&self.datxid.1, w, &row)?;
        Ok(())
    }

    /// DROP DATABASE が呼ぶ。行がなくてもエラーにしない。
    pub fn delete_datxid(
        &self,
        w: &crate::storage::WriteCtx,
        snap: &Snapshot,
        db: Oid,
    ) -> Result<()> {
        let victims = self.scan_datxid(snap, |r| Ok(as_oid(&r[0], "yz_datxid", "datid")? == db))?;
        for t in &victims {
            self.storage
                .delete(&self.datxid.1, w, snap, t.tid, None, None)?
                .expect_simple(SimpleHeapOp::Delete)?;
        }
        Ok(())
    }

    fn scan_datxid(
        &self,
        snap: &Snapshot,
        mut keep: impl FnMut(&crate::types::Row) -> Result<bool>,
    ) -> Result<Vec<crate::storage::HeapTuple>> {
        let mut scan = self.storage.begin_scan(&self.datxid.1, snap)?;
        let mut out = Vec::new();
        while let Some(t) = self.storage.scan_next(&mut scan)? {
            if keep(&t.row)? {
                out.push(t);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_env::{DB, env, plain_spec, snap, write_table};
    use super::*;
    use crate::catalog::rows::DATFROZEN_PRISTINE;
    use crate::storage::WriteCtx;

    fn w(xid: u64) -> WriteCtx {
        WriteCtx {
            xid: Xid(xid),
            cid: 1,
        }
    }

    #[test]
    fn initial_rows_are_the_oldest_boundary() {
        let e = env();
        let s = snap();
        // initdb は pg_class の 'r' のカタログごとに 1 行を書く。
        let rels = e.store.list_vacuum_relations(&s).unwrap();
        assert!(!rels.is_empty());
        assert!(rels.windows(2).all(|p| p[0].oid < p[1].oid), "oid 昇順");
        for r in &rels {
            assert_eq!(r.relkind, 'r');
            assert_eq!(
                e.store.relxid(&s, r.oid).unwrap(),
                Some(Xid(3)),
                "{}",
                r.name
            );
        }
        let m = e.store.min_relxid(&s).unwrap();
        assert_eq!(m.min, Xid(3));
        assert!(m.missing.is_empty(), "{:?}", m.missing);
        assert_eq!(e.store.relxid(&s, 999_999).unwrap(), None);
    }

    #[test]
    fn catalogs_are_listed_with_schema_and_locator() {
        let e = env();
        let s = snap();
        let class = e
            .store
            .list_vacuum_relations(&s)
            .unwrap()
            .into_iter()
            .find(|r| r.name == "pg_class")
            .expect("pg_class is listed");
        assert_eq!(class.schema, "pg_catalog");
        assert!(!class.is_shared);
        assert_eq!(
            class.locator,
            schema::catalog_def(oids::PG_CLASS).unwrap().locator(DB)
        );
        let db = e
            .store
            .vacuum_relation_info(&s, oids::PG_DATABASE)
            .unwrap()
            .expect("pg_database is in pg_class");
        assert!(db.is_shared);
        assert_eq!(db.locator.db_oid, 0);
        assert!(e.store.vacuum_relation_info(&s, 999_999).unwrap().is_none());
    }

    #[test]
    fn user_tables_are_listed_and_relxid_follows_insert_update_delete() {
        let e = env();
        let spec = plain_spec(&e, "vt");
        write_table(&e, &spec);
        let t = spec.oid;
        let s = snap();
        let info = e.store.vacuum_relation_info(&s, t).unwrap().unwrap();
        assert_eq!((info.name.as_str(), info.schema.as_str()), ("vt", "public"));
        assert_eq!(info.locator.rel_number, RelFileNumber(t));
        assert_eq!(info.locator.db_oid, DB);
        assert!(
            e.store
                .list_vacuum_relations(&s)
                .unwrap()
                .iter()
                .any(|r| r.oid == t)
        );
        // 行がないテーブルは fail-closed。
        let m = e.store.min_relxid(&s).unwrap();
        assert_eq!(m.min, Xid(3));
        assert_eq!(m.missing, vec!["vt".to_owned()]);
        e.store.insert_relxid(&w(100), t, Xid(50)).unwrap();
        assert_eq!(e.store.relxid(&s, t).unwrap(), Some(Xid(50)));
        assert!(e.store.min_relxid(&s).unwrap().missing.is_empty());
        e.store.update_relxid(&w(101), &s, t, Xid(70)).unwrap();
        assert_eq!(e.store.relxid(&s, t).unwrap(), Some(Xid(70)));
        e.store.delete_relxid(&w(102), &s, t).unwrap();
        assert_eq!(e.store.relxid(&s, t).unwrap(), None);
    }

    #[test]
    fn update_of_a_missing_relxid_row_is_an_internal_error() {
        let e = env();
        let err = e
            .store
            .update_relxid(&w(100), &snap(), 999_999, Xid(9))
            .unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn datxid_initial_rows_and_min() {
        let e = env();
        let s = snap();
        assert_eq!(e.shared.datxid(&s, 1).unwrap(), Some(Xid(3)));
        assert_eq!(e.shared.datxid(&s, 5).unwrap(), Some(Xid(3)));
        assert_eq!(
            e.shared.datxid(&s, 4).unwrap(),
            Some(Xid(u64::try_from(DATFROZEN_PRISTINE).unwrap()))
        );
        assert_eq!(e.shared.datxid(&s, 77).unwrap(), None);
        // template0 の番兵は最小に影響しない。
        assert_eq!(e.shared.min_datxid(&s).unwrap(), Xid(3));
    }

    #[test]
    fn datxid_insert_delete_and_fail_closed() {
        let e = env();
        let s = snap();
        e.shared.insert_datxid(&w(100), 20, Xid(500)).unwrap();
        assert_eq!(e.shared.datxid(&s, 20).unwrap(), Some(Xid(500)));
        e.shared.delete_datxid(&w(101), &s, 20).unwrap();
        assert_eq!(e.shared.datxid(&s, 20).unwrap(), None);
        // 行を消しても二重削除はエラーにならない。
        e.shared.delete_datxid(&w(102), &s, 20).unwrap();
        // postgres(5) の行を消すと、pg_database にあって yz_datxid にない → Xid(3)。
        e.shared.delete_datxid(&w(103), &s, 5).unwrap();
        e.shared.delete_datxid(&w(104), &s, 1).unwrap();
        assert_eq!(e.shared.min_datxid(&s).unwrap(), Xid(3));
    }
}
