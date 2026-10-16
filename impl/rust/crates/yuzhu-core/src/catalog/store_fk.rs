//! 外部キーのカタログの読み書き（`m5/07` §3、§4.2）。`pg_constraint`（contype `f`）と `pg_depend` の行。
//!
//! `store.rs` の子モジュールとして読み込まれる（`#[path]`）ので、`CatalogStore` の非公開の項目を使える。

use std::collections::BTreeSet;
use std::sync::Arc;

use super::{CatalogStore, as_oid, as_text, bad_value, column_index, depend_row, oids};
use crate::catalog::depend::{DependType, NewDepend, ObjectAddress, classes};
use crate::catalog::fk::{FkAction, FkEqOps, FkMatch, ForeignKeyDef};
use crate::catalog::rows::RowBuilder;
use crate::error::{Error, Result};
use crate::txn::Snapshot;
use crate::types::array::{ArrayValue, array_view};
use crate::types::{Datum, Oid, Row};

/// `(foreign_keys, referenced_by)`。
pub type ForeignKeyLists = (Vec<Arc<ForeignKeyDef>>, Vec<Arc<ForeignKeyDef>>);

/// `int2[]`（下限 1）の `Datum`。
pub fn int2_array(v: &[i16]) -> Datum {
    Datum::Array(Box::new(ArrayValue::from_int2s(v)))
}

/// `oid[]`（下限 1）の `Datum`。
pub fn oid_array(v: &[Oid]) -> Datum {
    Datum::Array(Box::new(ArrayValue::from_oids(v)))
}

fn ch(c: char) -> Datum {
    Datum::Char(u8::try_from(c).unwrap_or(0))
}

fn match_code(m: FkMatch) -> char {
    match m {
        FkMatch::Simple => 's',
        FkMatch::Full => 'f',
    }
}

/// `pg_constraint` の FK の行（§3.1）。
pub fn fk_constraint_row(def: &ForeignKeyDef, namespace: Oid) -> Row {
    let pf: Vec<Oid> = def.eq_ops.iter().map(|o| o.pf).collect();
    let pp: Vec<Oid> = def.eq_ops.iter().map(|o| o.pp).collect();
    let ff: Vec<Oid> = def.eq_ops.iter().map(|o| o.ff).collect();
    let mut b = RowBuilder::new(oids::PG_CONSTRAINT)
        .set("oid", Datum::Oid(def.oid))
        .set("conname", Datum::Text(def.name.clone()))
        .set("connamespace", Datum::Oid(namespace))
        .set("contype", ch('f'))
        .set("condeferrable", Datum::Bool(false))
        .set("condeferred", Datum::Bool(false))
        .set("convalidated", Datum::Bool(true))
        .set("conrelid", Datum::Oid(def.child))
        .set("contypid", Datum::Oid(0))
        .set("conindid", Datum::Oid(def.parent_index))
        .set("conparentid", Datum::Oid(0))
        .set("confrelid", Datum::Oid(def.parent))
        .set("confupdtype", ch(def.on_update.code()))
        .set("confdeltype", ch(def.on_delete.code()))
        .set("confmatchtype", ch(match_code(def.match_type)))
        .set("conislocal", Datum::Bool(true))
        .set("coninhcount", Datum::Int2(0))
        .set("connoinherit", Datum::Bool(true))
        .set("conkey", int2_array(&def.child_cols))
        .set("confkey", int2_array(&def.parent_cols))
        .set("conpfeqop", oid_array(&pf))
        .set("conppeqop", oid_array(&pp))
        .set("conffeqop", oid_array(&ff));
    if !def.set_cols.is_empty() {
        b = b.set("confdelsetcols", int2_array(&def.set_cols));
    }
    b.build()
}

/// `pg_depend` の行（§3.2）。子の列（a）、親の一意インデックス（n）、親の列（n）。
pub fn fk_depends(def: &ForeignKeyDef) -> Vec<NewDepend> {
    let dependent = ObjectAddress::constraint(def.oid);
    let mut out = Vec::with_capacity(def.child_cols.len() + def.parent_cols.len() + 1);
    for &a in &def.child_cols {
        out.push(NewDepend {
            dependent,
            referenced: ObjectAddress::column(def.child, a),
            deptype: DependType::Auto,
        });
    }
    out.push(NewDepend {
        dependent,
        referenced: ObjectAddress::relation(def.parent_index),
        deptype: DependType::Normal,
    });
    for &a in &def.parent_cols {
        out.push(NewDepend {
            dependent,
            referenced: ObjectAddress::column(def.parent, a),
            deptype: DependType::Normal,
        });
    }
    out
}

fn int2s(d: &Datum, column: &str) -> Result<Vec<i16>> {
    match d {
        Datum::Null => Ok(Vec::new()),
        d => array_view(d)
            .ok_or_else(|| bad_value("pg_constraint", column, d))?
            .to_int2s(),
    }
}

fn oid_list(d: &Datum, column: &str) -> Result<Vec<Oid>> {
    match d {
        Datum::Null => Ok(Vec::new()),
        d => array_view(d)
            .ok_or_else(|| bad_value("pg_constraint", column, d))?
            .to_oids(),
    }
}

fn code(d: &Datum, column: &str) -> Result<char> {
    match d {
        Datum::Char(c) => Ok(char::from(*c)),
        other => Err(bad_value("pg_constraint", column, other)),
    }
}

/// contype `f` の行から `ForeignKeyDef` を作る。
pub fn fk_from_row(row: &Row) -> Result<ForeignKeyDef> {
    let col = |n: &str| column_index(oids::PG_CONSTRAINT, n);
    let oid = as_oid(&row[col("oid")], "pg_constraint", "oid")?;
    let action = |name: &str| -> Result<FkAction> {
        let c = code(&row[col(name)], name)?;
        FkAction::from_code(c).ok_or_else(|| {
            Error::corrupted(format!("constraint {oid} has an unknown {name} '{c}'"))
        })
    };
    let match_type = match code(&row[col("confmatchtype")], "confmatchtype")? {
        's' => FkMatch::Simple,
        'f' => FkMatch::Full,
        c => {
            return Err(Error::corrupted(format!(
                "constraint {oid} has an unsupported confmatchtype '{c}'"
            )));
        }
    };
    let pf = oid_list(&row[col("conpfeqop")], "conpfeqop")?;
    let pp = oid_list(&row[col("conppeqop")], "conppeqop")?;
    let ff = oid_list(&row[col("conffeqop")], "conffeqop")?;
    if pf.len() != pp.len() || pf.len() != ff.len() {
        return Err(Error::corrupted(format!(
            "constraint {oid} has operator lists of different lengths"
        )));
    }
    let eq_ops = pf
        .iter()
        .zip(&pp)
        .zip(&ff)
        .map(|((&pf, &pp), &ff)| FkEqOps { pf, pp, ff })
        .collect();
    let child_cols = int2s(&row[col("conkey")], "conkey")?;
    let parent_cols = int2s(&row[col("confkey")], "confkey")?;
    if child_cols.is_empty() || child_cols.len() != parent_cols.len() {
        return Err(Error::corrupted(format!(
            "constraint {oid} has inconsistent conkey / confkey"
        )));
    }
    Ok(ForeignKeyDef {
        oid,
        name: as_text(&row[col("conname")], "pg_constraint", "conname")?.to_owned(),
        child: as_oid(&row[col("conrelid")], "pg_constraint", "conrelid")?,
        parent: as_oid(&row[col("confrelid")], "pg_constraint", "confrelid")?,
        parent_index: as_oid(&row[col("conindid")], "pg_constraint", "conindid")?,
        child_cols,
        parent_cols,
        match_type,
        on_update: action("confupdtype")?,
        on_delete: action("confdeltype")?,
        set_cols: int2s(&row[col("confdelsetcols")], "confdelsetcols")?,
        eq_ops,
    })
}

impl CatalogStore {
    /// contype `f` の行のうち `keep` に当たるものを、OID 昇順で返す。
    fn scan_foreign_keys(
        &self,
        snap: &Snapshot,
        keep: impl Fn(&ForeignKeyDef) -> bool,
    ) -> Result<Vec<Arc<ForeignKeyDef>>> {
        let contype = column_index(oids::PG_CONSTRAINT, "contype");
        let found = self.scan(snap, oids::PG_CONSTRAINT, |r| {
            Ok(matches!(&r[contype], Datum::Char(b'f')))
        })?;
        let mut out = Vec::new();
        for t in &found {
            let fk = fk_from_row(&t.row)?;
            if keep(&fk) {
                out.push(Arc::new(fk));
            }
        }
        out.sort_by_key(|f| f.oid);
        Ok(out)
    }

    /// 子（`conrelid`）が `table` の FK。OID 昇順。
    pub fn foreign_keys_of(&self, snap: &Snapshot, table: Oid) -> Result<Vec<Arc<ForeignKeyDef>>> {
        self.scan_foreign_keys(snap, |f| f.child == table)
    }

    /// 親（`confrelid`）が `table` の FK。OID 昇順。
    pub fn foreign_keys_referencing(
        &self,
        snap: &Snapshot,
        table: Oid,
    ) -> Result<Vec<Arc<ForeignKeyDef>>> {
        self.scan_foreign_keys(snap, |f| f.parent == table)
    }

    /// `TableDef` に載せる 2 つの一覧: `(foreign_keys, referenced_by)`。自己参照は両方に入る。
    pub fn load_foreign_keys(&self, snap: &Snapshot, table: Oid) -> Result<ForeignKeyLists> {
        let all = self.scan_foreign_keys(snap, |f| f.child == table || f.parent == table)?;
        let out = all.iter().filter(|f| f.child == table).cloned().collect();
        let incoming = all.iter().filter(|f| f.parent == table).cloned().collect();
        Ok((out, incoming))
    }

    /// `pg_constraint` の 1 行と `pg_depend` の行（§3.2）を書く。`namespace` は子の表の名前空間
    /// （同じコマンドで作った表の行は `snap` から見えないので、呼び出し側が渡す）。
    pub fn insert_foreign_key(
        &self,
        w: &crate::storage::WriteCtx,
        _snap: &Snapshot,
        def: &ForeignKeyDef,
        namespace: Oid,
    ) -> Result<()> {
        self.insert_row(w, oids::PG_CONSTRAINT, &fk_constraint_row(def, namespace))?;
        for d in fk_depends(def) {
            self.insert_row(w, oids::PG_DEPEND, &depend_row(&d))?;
        }
        Ok(())
    }

    /// `pg_constraint` の行と、`objid = oid` の `pg_depend` の行を消す。
    pub fn delete_foreign_key(
        &self,
        w: &crate::storage::WriteCtx,
        snap: &Snapshot,
        oid: Oid,
    ) -> Result<()> {
        let n = self.delete_where(w, snap, oids::PG_CONSTRAINT, |r| {
            Ok(as_oid(&r[0], "pg_constraint", "oid")? == oid
                && matches!(
                    &r[column_index(oids::PG_CONSTRAINT, "contype")],
                    Datum::Char(b'f')
                ))
        })?;
        if n == 0 {
            return Err(Error::internal(format!(
                "foreign key constraint {oid} not found"
            )));
        }
        self.delete_dependencies(
            w,
            snap,
            super::DependFilter::Dependent {
                class_id: classes::CONSTRAINT,
                obj_id: oid,
            },
        )?;
        Ok(())
    }

    /// `rel` と FK でつながる相手の表（`rel` が参照する表と `rel` を参照する表）。`constraint` が `Some` なら
    /// その名前の制約（`rel` の子側または親側）の相手だけ。自分自身は含まない。重複なし・昇順。
    pub fn fk_peer_tables(
        &self,
        snap: &Snapshot,
        rel: Oid,
        constraint: Option<&str>,
    ) -> Result<Vec<Oid>> {
        let fks = self.scan_foreign_keys(snap, |f| {
            (f.child == rel || f.parent == rel) && constraint.is_none_or(|n| f.name == n)
        })?;
        let peers: BTreeSet<Oid> = fks
            .iter()
            .flat_map(|f| [f.child, f.parent])
            .filter(|&o| o != rel)
            .collect();
        Ok(peers.into_iter().collect())
    }
}
