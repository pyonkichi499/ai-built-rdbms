//! カタログの整合検査（`m4/07-catalog-ddl.md` §8.3）。
//!
//! DROP の取りこぼし（D07-2 の「二重の網」の前提）と、WAL の REDO 後の欠損を検出する。
//! 条件 1〜9 はユーザーオブジェクト（`oid >= 16384`）について確かめ、問題の文字列を返す（空なら正常）。
//! 条件 10 のうち「`relfilenode` のファイルが存在する」も確かめる（`base/<db>/` の孤児の検出は
//! ディレクトリの一覧が要るので、呼び出し側のテストが行う）。
//! 全 Rust の統合テストの終わりと、クラッシュ試験の各クラッシュ点で呼ぶ。

use std::collections::{HashMap, HashSet};

use super::rows::column_index;
use super::schema::oids;
use super::store::CatalogStore;
use crate::engine::{Cluster, DatabaseHandle};
use crate::error::Result;
use crate::txn::Snapshot;
use crate::types::{Datum, Oid, Row, oid};

fn oid_of(d: &Datum) -> Oid {
    match d {
        Datum::Oid(v) => *v,
        _ => 0,
    }
}

fn int_of(d: &Datum) -> i64 {
    match d {
        Datum::Int2(v) => i64::from(*v),
        Datum::Int4(v) => i64::from(*v),
        Datum::Int8(v) => *v,
        _ => 0,
    }
}

fn char_of(d: &Datum) -> u8 {
    match d {
        Datum::Char(v) => *v,
        _ => 0,
    }
}

fn bool_of(d: &Datum) -> bool {
    matches!(d, Datum::Bool(true))
}

fn all_rows(store: &CatalogStore, snap: &Snapshot, catalog: Oid) -> Result<Vec<Row>> {
    Ok(store
        .scan(snap, catalog, |_| Ok(true))?
        .into_iter()
        .map(|t| t.row)
        .collect())
}

/// 条件 1〜9。`Cluster` なしで呼べる（`CatalogStore` だけを使う）。
#[allow(clippy::too_many_lines)]
pub fn check_catalog_store(store: &CatalogStore, snap: &Snapshot) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let user = |o: Oid| o >= oid::FIRST_NORMAL_OBJECT_ID;
    let c = |cat: Oid, n: &str| column_index(cat, n);

    let class_rows = all_rows(store, snap, oids::PG_CLASS)?;
    let attr_rows = all_rows(store, snap, oids::PG_ATTRIBUTE)?;
    let attrdef_rows = all_rows(store, snap, oids::PG_ATTRDEF)?;
    let con_rows = all_rows(store, snap, oids::PG_CONSTRAINT)?;
    let index_rows = all_rows(store, snap, oids::PG_INDEX)?;
    let dep_rows = all_rows(store, snap, oids::PG_DEPEND)?;
    let seq_rows = all_rows(store, snap, oids::PG_SEQUENCE)?;

    let relkind = c(oids::PG_CLASS, "relkind");
    let kinds: HashMap<Oid, u8> = class_rows
        .iter()
        .map(|r| (oid_of(&r[0]), char_of(&r[relkind])))
        .collect();
    let con_oids: HashSet<Oid> = con_rows.iter().map(|r| oid_of(&r[0])).collect();
    let attrdef_oids: HashSet<Oid> = attrdef_rows.iter().map(|r| oid_of(&r[0])).collect();

    // 1. 参照先の pg_class が存在する。
    for r in &attr_rows {
        let rel = oid_of(&r[c(oids::PG_ATTRIBUTE, "attrelid")]);
        if user(rel) && !kinds.contains_key(&rel) {
            problems.push(format!("1: pg_attribute row of missing relation {rel}"));
        }
    }
    for r in &attrdef_rows {
        let rel = oid_of(&r[c(oids::PG_ATTRDEF, "adrelid")]);
        if !kinds.contains_key(&rel) {
            problems.push(format!(
                "1: pg_attrdef {} of missing relation {rel}",
                oid_of(&r[0])
            ));
        }
    }
    for r in &con_rows {
        let rel = oid_of(&r[c(oids::PG_CONSTRAINT, "conrelid")]);
        if rel != 0 && !kinds.contains_key(&rel) {
            problems.push(format!(
                "1: pg_constraint {} of missing relation {rel}",
                oid_of(&r[0])
            ));
        }
    }
    let (indexrelid, indrelid) = (
        c(oids::PG_INDEX, "indexrelid"),
        c(oids::PG_INDEX, "indrelid"),
    );
    for r in &index_rows {
        for (what, o) in [
            ("indexrelid", oid_of(&r[indexrelid])),
            ("indrelid", oid_of(&r[indrelid])),
        ] {
            if !kinds.contains_key(&o) {
                problems.push(format!("1: pg_index.{what} {o} has no pg_class row"));
            }
        }
    }

    // 2. 索引の relkind。
    let mut index_ids = HashSet::new();
    for r in &index_rows {
        let (i, t) = (oid_of(&r[indexrelid]), oid_of(&r[indrelid]));
        index_ids.insert(i);
        if kinds.get(&i).is_some_and(|k| *k != b'i') {
            problems.push(format!("2: pg_index.indexrelid {i} is not relkind 'i'"));
        }
        if kinds.get(&t).is_some_and(|k| *k != b'r') {
            problems.push(format!("2: pg_index.indrelid {t} is not relkind 'r'"));
        }
    }
    for (o, k) in &kinds {
        if *k == b'i' && !index_ids.contains(o) {
            problems.push(format!("2: index {o} has no pg_index row"));
        }
    }

    // 3. 制約の索引。
    let index_table: HashMap<Oid, Oid> = index_rows
        .iter()
        .map(|r| (oid_of(&r[indexrelid]), oid_of(&r[indrelid])))
        .collect();
    for r in &con_rows {
        let (con, kind) = (
            oid_of(&r[0]),
            char_of(&r[c(oids::PG_CONSTRAINT, "contype")]),
        );
        let indid = oid_of(&r[c(oids::PG_CONSTRAINT, "conindid")]);
        let rel = oid_of(&r[c(oids::PG_CONSTRAINT, "conrelid")]);
        if matches!(kind, b'p' | b'u') && indid == 0 {
            problems.push(format!(
                "3: constraint {con} of type {} has no index",
                char::from(kind)
            ));
        }
        if indid != 0 {
            match index_table.get(&indid) {
                None => problems.push(format!(
                    "3: constraint {con} refers to missing index {indid}"
                )),
                Some(t) if *t != rel => {
                    problems.push(format!(
                        "3: constraint {con} and its index {indid} belong to different tables"
                    ));
                }
                Some(_) => {}
            }
        }
    }

    // 4. 制約が所有する索引の (索引 -> 制約, i) はちょうど 1 行。制約には各キー列への a がある。
    let dep = |n: &str| c(oids::PG_DEPEND, n);
    for r in &con_rows {
        let con = oid_of(&r[0]);
        let indid = oid_of(&r[c(oids::PG_CONSTRAINT, "conindid")]);
        if indid == 0 {
            continue;
        }
        let internal = dep_rows
            .iter()
            .filter(|d| {
                oid_of(&d[dep("classid")]) == oids::PG_CLASS
                    && oid_of(&d[dep("objid")]) == indid
                    && oid_of(&d[dep("refclassid")]) == oids::PG_CONSTRAINT
                    && oid_of(&d[dep("refobjid")]) == con
                    && char_of(&d[dep("deptype")]) == b'i'
            })
            .count();
        if internal != 1 {
            problems.push(format!(
                "4: index {indid} has {internal} internal dependencies on constraint {con}"
            ));
        }
        if let Datum::Int2Vector(key) = &r[c(oids::PG_CONSTRAINT, "conkey")] {
            let rel = oid_of(&r[c(oids::PG_CONSTRAINT, "conrelid")]);
            for attnum in key {
                let has = dep_rows.iter().any(|d| {
                    oid_of(&d[dep("classid")]) == oids::PG_CONSTRAINT
                        && oid_of(&d[dep("objid")]) == con
                        && oid_of(&d[dep("refobjid")]) == rel
                        && int_of(&d[dep("refobjsubid")]) == i64::from(*attnum)
                        && char_of(&d[dep("deptype")]) == b'a'
                });
                if !has {
                    problems.push(format!(
                        "4: constraint {con} has no dependency on column {attnum}"
                    ));
                }
            }
        }
    }

    // 5. pg_depend の両端。
    let exists = |class: Oid, id: Oid| match class {
        oids::PG_CLASS => kinds.contains_key(&id),
        oids::PG_CONSTRAINT => con_oids.contains(&id),
        oids::PG_ATTRDEF => attrdef_oids.contains(&id),
        _ => true,
    };
    for d in &dep_rows {
        for (side, class, id) in [
            (
                "dependent",
                oid_of(&d[dep("classid")]),
                oid_of(&d[dep("objid")]),
            ),
            (
                "referenced",
                oid_of(&d[dep("refclassid")]),
                oid_of(&d[dep("refobjid")]),
            ),
        ] {
            if !exists(class, id) {
                problems.push(format!(
                    "5: pg_depend {side} end ({class}, {id}) does not exist"
                ));
            }
        }
    }

    // 6. シーケンス。
    let seq_ids: HashSet<Oid> = seq_rows.iter().map(|r| oid_of(&r[0])).collect();
    for (o, k) in &kinds {
        if user(*o) && *k == b'S' && !seq_ids.contains(o) {
            problems.push(format!("6: sequence {o} has no pg_sequence row"));
        }
    }
    for o in &seq_ids {
        if kinds.get(o) != Some(&b'S') {
            problems.push(format!(
                "6: pg_sequence.seqrelid {o} is not a sequence in pg_class"
            ));
        }
    }

    // 7. relhasindex。
    let hasindex = c(oids::PG_CLASS, "relhasindex");
    for r in &class_rows {
        let o = oid_of(&r[0]);
        if user(o)
            && char_of(&r[relkind]) == b'r'
            && index_rows.iter().any(|i| oid_of(&i[indrelid]) == o)
            && !bool_of(&r[hasindex])
        {
            problems.push(format!(
                "7: table {o} has an index but relhasindex is false"
            ));
        }
    }

    // 8. relnatts / relchecks。
    let (natts, nchecks) = (
        c(oids::PG_CLASS, "relnatts"),
        c(oids::PG_CLASS, "relchecks"),
    );
    let (arel, anum, adrop) = (
        c(oids::PG_ATTRIBUTE, "attrelid"),
        c(oids::PG_ATTRIBUTE, "attnum"),
        c(oids::PG_ATTRIBUTE, "attisdropped"),
    );
    for r in &class_rows {
        let o = oid_of(&r[0]);
        if !user(o) || !matches!(char_of(&r[relkind]), b'r' | b'S' | b'i') {
            continue;
        }
        let n = attr_rows
            .iter()
            .filter(|a| oid_of(&a[arel]) == o && int_of(&a[anum]) > 0 && !bool_of(&a[adrop]))
            .count();
        if i64::try_from(n).unwrap_or(-1) != int_of(&r[natts]) {
            problems.push(format!(
                "8: relation {o} has {n} attributes but relnatts = {}",
                int_of(&r[natts])
            ));
        }
        let checks = con_rows
            .iter()
            .filter(|k| {
                oid_of(&k[c(oids::PG_CONSTRAINT, "conrelid")]) == o
                    && char_of(&k[c(oids::PG_CONSTRAINT, "contype")]) == b'c'
            })
            .count();
        if i64::try_from(checks).unwrap_or(-1) != int_of(&r[nchecks]) {
            problems.push(format!(
                "8: relation {o} has {checks} CHECK constraints but relchecks = {}",
                int_of(&r[nchecks])
            ));
        }
    }

    // 9. atthasdef と pg_attrdef、attidentity とシーケンス。
    let (hasdef, ident) = (
        c(oids::PG_ATTRIBUTE, "atthasdef"),
        c(oids::PG_ATTRIBUTE, "attidentity"),
    );
    let defs: HashSet<(Oid, i64)> = attrdef_rows
        .iter()
        .map(|r| {
            (
                oid_of(&r[c(oids::PG_ATTRDEF, "adrelid")]),
                int_of(&r[c(oids::PG_ATTRDEF, "adnum")]),
            )
        })
        .collect();
    let mut with_default = HashSet::new();
    for a in &attr_rows {
        let (rel, num) = (oid_of(&a[arel]), int_of(&a[anum]));
        if !user(rel) || num <= 0 {
            continue;
        }
        if bool_of(&a[hasdef]) {
            with_default.insert((rel, num));
            if !defs.contains(&(rel, num)) {
                problems.push(format!(
                    "9: column {num} of {rel} has atthasdef but no pg_attrdef row"
                ));
            }
        }
        if char_of(&a[ident]) != 0 {
            let has = dep_rows.iter().any(|d| {
                oid_of(&d[dep("refobjid")]) == rel
                    && int_of(&d[dep("refobjsubid")]) == num
                    && char_of(&d[dep("deptype")]) == b'i'
                    && oid_of(&d[dep("classid")]) == oids::PG_CLASS
            });
            if !has {
                problems.push(format!(
                    "9: identity column {num} of {rel} has no sequence dependency"
                ));
            }
        }
    }
    for (rel, num) in &defs {
        if !with_default.contains(&(*rel, *num)) {
            problems.push(format!(
                "9: pg_attrdef of column {num} of {rel} but atthasdef is false"
            ));
        }
    }
    Ok(problems)
}

/// 条件 1〜9 と、条件 10 の「`relfilenode` のファイルがすべて存在する」。
pub fn check_catalog(
    cluster: &Cluster,
    db: &DatabaseHandle,
    snap: &Snapshot,
) -> Result<Vec<String>> {
    let mut problems = check_catalog_store(&db.catalog, snap)?;
    for r in all_rows(&db.catalog, snap, oids::PG_CLASS)? {
        let o = oid_of(&r[0]);
        if super::schema::catalog_def(o).is_some() || o < oid::FIRST_NORMAL_OBJECT_ID {
            continue;
        }
        if let Some(row) = db.catalog.relation_row(snap, o)?
            && !cluster.storage().storage_exists(row.locator)?
        {
            problems.push(format!(
                "10: the file of relation {o} ({}) is missing",
                row.name
            ));
        } else if let Some(row) = db.catalog.relation_row(snap, o)?
            && matches!(
                char_of(&r[column_index(oids::PG_CLASS, "relkind")]),
                b'i' | b'S'
            )
            && cluster
                .stack()
                .smgr
                .nblocks(row.locator, crate::storage::smgr::ForkNumber::Main)?
                == 0
        {
            // 索引（メタページ）とシーケンス（1 ページ）は空にならない。unlink 済みのファイルは 0 バイトで残る（D13）。
            problems.push(format!(
                "10: the file of relation {o} ({}) is empty (unlinked by mistake?)",
                row.name
            ));
        }
    }
    Ok(problems)
}

/// 条件 10 の後半: `base/<db>/` に、`pg_class.relfilenode` のどれにも対応しないファイルがない。
/// クラッシュ後は孤児を許す（M3 の D15）ので、クラッシュ試験では呼ばない。
pub fn check_orphan_files(
    cluster: &Cluster,
    db: &DatabaseHandle,
    snap: &Snapshot,
) -> Result<Vec<String>> {
    let filenode = column_index(oids::PG_CLASS, "relfilenode");
    let known: HashSet<Oid> = all_rows(&db.catalog, snap, oids::PG_CLASS)?
        .iter()
        .map(|r| match oid_of(&r[filenode]) {
            0 => oid_of(&r[0]),
            n => n,
        })
        .collect();
    let dir = std::path::PathBuf::from(format!("base/{}", db.oid));
    let files = cluster
        .stack()
        .vfs
        .read_dir(&dir)
        .map_err(|e| crate::error::Error::from_io(&e, "reading the database directory"))?;
    let mut problems = Vec::new();
    for f in files {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
        let Ok(number) = digits.parse::<Oid>() else {
            continue;
        };
        if !known.contains(&number) {
            problems.push(format!("10: orphan file {name} in base/{}", db.oid));
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::store::test_env::*;

    fn problems(e: &Env) -> Vec<String> {
        check_catalog_store(&e.store, &snap()).unwrap()
    }

    #[test]
    fn a_fresh_catalog_and_a_populated_one_are_consistent() {
        let e = env();
        assert!(problems(&e).is_empty());
        let spec = pk_spec(&e, "p");
        write_table(&e, &spec);
        let spec2 = plain_spec(&e, "t");
        write_table(&e, &spec2);
        assert_eq!(problems(&e), Vec::<String>::new());
    }

    #[test]
    fn missing_pieces_are_reported() {
        let e = env();
        let spec = pk_spec(&e, "p");
        write_table(&e, &spec);
        // delete the pg_depend rows of the first constraint: conditions 4
        let con = spec.indexes[0].constraint.as_ref().unwrap().oid;
        e.store
            .delete_dependencies(
                &wctx(),
                &snap(),
                crate::catalog::store::DependFilter::Dependent {
                    class_id: crate::catalog::depend::classes::CONSTRAINT,
                    obj_id: con,
                },
            )
            .unwrap();
        let p = problems(&e);
        assert!(p.iter().any(|m| m.starts_with("4:")), "{p:?}");
    }

    #[test]
    fn a_dangling_pg_class_row_is_reported() {
        let e = env();
        let spec = plain_spec(&e, "t");
        write_table(&e, &spec);
        // drop only the pg_class row by hand
        let plan = crate::catalog::depend::DropPlan {
            items: vec![crate::catalog::depend::DropItem {
                addr: crate::catalog::depend::ObjectAddress::relation(spec.oid),
                kind: crate::catalog::depend::DropKind::Table,
                description: "table t".into(),
                locator: None,
                owner_table: None,
                attnum: None,
            }],
            cascaded: vec![],
        };
        e.store.drop_objects(&wctx(), &snap(), &plan).unwrap();
        // the table's attribute rows went too, but its pg_attrdef / pg_constraint rows remain
        let p = problems(&e);
        assert!(p.iter().any(|m| m.starts_with("1:")), "{p:?}");
    }
}

#[cfg(test)]
mod orphan_tests {
    use super::*;
    use crate::bootstrap::{InitdbOptions, initdb};
    use crate::debug_knobs::DebugKnobs;
    use crate::engine::ClusterOptions;
    use crate::storage::vfs::{OpenMode, SimVfs, Vfs};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_file_without_a_pg_class_row_is_an_orphan() {
        let sim = SimVfs::new(1);
        initdb(Arc::new(sim.clone()), &InitdbOptions::new("postgres")).unwrap();
        let cluster = Cluster::open(
            Arc::new(sim.clone()),
            ClusterOptions {
                data_dir: PathBuf::from("/sim/data"),
                shared_buffers: 256,
                max_connections: 16,
                checkpoint_timeout: Duration::from_secs(3600),
                max_wal_size: 64 << 20,
                background_checkpointer: false,
                knobs: DebugKnobs::default(),
            },
        )
        .unwrap();
        let (db, _) = cluster.connect("postgres", "postgres").unwrap();
        let snap = cluster.txn_manager().snapshot(None, 0);
        let first = check_orphan_files(&cluster, &db, &snap).unwrap();
        assert!(first.is_empty(), "{first:?}");
        let stray = PathBuf::from(format!("base/{}/99999", db.oid));
        let vfs: &dyn Vfs = &sim;
        vfs.open(Path::new(&stray), OpenMode::CreateNew).unwrap();
        let p = check_orphan_files(&cluster, &db, &snap).unwrap();
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("99999"));
    }
}
