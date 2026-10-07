//! `CREATE INDEX` / `DROP INDEX`、`build_from_heap`、演算子クラスの解決
//! （`m4/07-catalog-ddl.md` §5.2、§5.2.1、§5.3、§6.6）。

use std::collections::HashSet;

use super::{DdlCtx, RelOptTarget, validate_reloptions};
use crate::analyzer::query::{BoundCreateIndex, BoundDropIndex};
use crate::catalog::builtin::format_type_name;
use crate::catalog::depend::{ObjectAddress, cascade_notice, plan_drop};
use crate::catalog::naming::{choose_index_column_names, choose_index_name};
use crate::catalog::opclass::{OpClass, default_opclass, opclass_by_name};
use crate::catalog::store::{NewIndex, NewIndexColumn};
use crate::catalog::{ColumnDef, IndexColumn, IndexDef, TableDef};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::interrupt::InterruptFlag;
use crate::storage::btree::{cmp_key_tid, cmp_keys};
use crate::storage::{
    BuildStats, BuildUnique, IndexHandle, IndexStore, RelHandle, TableStore, TupleState, WriteCtx,
};
use crate::txn::Xid;
use crate::types::io::output_text_env;
use crate::types::{Datum, Oid, SqlType, Tid, TypeEnv};

/// 索引のキー列の最大数（`INDEX_MAX_KEYS`）。
pub const INDEX_MAX_KEYS: usize = 32;

/// 列の型と、明示された演算子クラス名から、使う演算子クラスを決める（07 §6.6）。
/// 戻り値は静的な表（`catalog::opclass`）の要素。
pub(crate) fn resolve_opclass(ty: SqlType, requested: Option<&str>) -> Result<&'static OpClass> {
    match requested {
        Some(name) => {
            // `pg_catalog.int4_ops` だけを許す。それ以外の修飾は存在しないものとして扱う。
            let bare = name.strip_prefix("pg_catalog.").unwrap_or(name);
            let oc = opclass_by_name(bare).ok_or_else(|| {
                Error::new(
                    sqlstate::UNDEFINED_OBJECT,
                    format!("operator class \"{name}\" does not exist for access method \"btree\""),
                )
            })?;
            if !opclass_accepts(oc, ty.oid) {
                return Err(Error::new(
                    sqlstate::DATATYPE_MISMATCH,
                    format!(
                        "operator class \"{}\" does not accept data type {}",
                        oc.name,
                        format_type_name(ty.oid, None)
                    ),
                ));
            }
            Ok(oc)
        }
        None => default_opclass(ty.oid).ok_or_else(|| {
            Error::new(
                sqlstate::UNDEFINED_OBJECT,
                format!(
                    "data type {} has no default operator class for access method \"btree\"",
                    format_type_name(ty.oid, None)
                ),
            )
            .with_hint(
                "You must specify an operator class for the index or define a default operator class for the data type.",
            )
        }),
    }
}

/// 索引の opclass としてバイナリ互換とみなす型（D07-19）。varchar → text、regclass / regtype / regproc → oid。
fn coercible_index_type(ty: Oid) -> Oid {
    match ty {
        1043 => 25,
        2205 | 2206 | 24 => 26,
        t => t,
    }
}

fn opclass_accepts(oc: &OpClass, ty: Oid) -> bool {
    oc.input_type == ty || coercible_index_type(ty) == oc.input_type
}

/// 索引のキー列 1 つ（索引に載る列の情報と、表の列の型・名前）。
#[derive(Clone, Debug)]
pub(crate) struct KeyPart {
    pub column: IndexColumn,
    pub ty: SqlType,
    pub colname: String,
}

/// `attnums` の各列について、既定の演算子クラス・昇順・NULLS LAST のキーを作る
/// （PRIMARY KEY / UNIQUE 制約が使う）。
pub(crate) fn default_key_parts(columns: &[ColumnDef], attnums: &[i16]) -> Result<Vec<KeyPart>> {
    attnums
        .iter()
        .map(|&attnum| {
            let col = columns
                .iter()
                .find(|c| c.attnum == attnum)
                .ok_or_else(|| Error::internal(format!("index key column {attnum} not found")))?;
            let oc = resolve_opclass(col.ty, None)?;
            Ok(KeyPart {
                column: IndexColumn {
                    attnum,
                    opclass: oc.oid,
                    opfamily: oc.family,
                    descending: false,
                    nulls_first: false,
                },
                ty: col.ty,
                colname: col.name.clone(),
            })
        })
        .collect()
}

/// 索引の列名（`ChooseIndexColumnNames`）を付けた `NewIndexColumn` の並び。
pub(crate) fn new_index_columns(parts: &[KeyPart]) -> Vec<NewIndexColumn> {
    let names: Vec<&str> = parts.iter().map(|p| p.colname.as_str()).collect();
    choose_index_column_names(&names)
        .into_iter()
        .zip(parts)
        .map(|(name, p)| NewIndexColumn {
            name,
            column: p.column.clone(),
            ty: p.ty,
        })
        .collect()
}

#[allow(clippy::too_many_lines, clippy::single_match_else)]
pub fn create_index(ctx: &mut DdlCtx<'_>, b: &BoundCreateIndex) -> Result<String> {
    // 1.
    if b.concurrently && ctx.in_transaction_block {
        return Err(Error::new(
            sqlstate::ACTIVE_SQL_TRANSACTION,
            "CREATE INDEX CONCURRENTLY cannot run inside a transaction block",
        ));
    }
    // 2.
    let table = &b.table;
    if table.is_system_catalog() {
        return Err(system_catalog_error(&table.name));
    }
    // 3.
    if b.method != "btree" {
        return Err(match b.method.as_str() {
            "hash" | "gist" | "spgist" | "gin" | "brin" => Error::not_supported(format!(
                "index access method \"{}\" is not supported yet",
                b.method
            )),
            other => Error::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("access method \"{other}\" does not exist"),
            ),
        });
    }
    // 4.
    if b.columns.len() > INDEX_MAX_KEYS {
        return Err(Error::new(
            sqlstate::TOO_MANY_COLUMNS,
            format!("cannot use more than {INDEX_MAX_KEYS} columns in an index"),
        ));
    }
    let mut parts = Vec::with_capacity(b.columns.len());
    for c in &b.columns {
        let col = table
            .columns
            .iter()
            .find(|d| d.attnum == c.attnum)
            .ok_or_else(|| Error::internal(format!("index column {} not found", c.attnum)))?;
        let oc = resolve_opclass(col.ty, c.opclass.as_deref())?;
        parts.push(KeyPart {
            column: IndexColumn {
                attnum: c.attnum,
                opclass: oc.oid,
                opfamily: oc.family,
                descending: c.descending,
                nulls_first: c.nulls_first,
            },
            ty: col.ty,
            colname: col.name.clone(),
        });
    }
    // 5.
    validate_reloptions(RelOptTarget::Index, &b.options)?;
    // 6.
    let columns = new_index_columns(&parts);
    let name = match &b.name {
        Some(n) => {
            if ctx.catalog.relation_kind(Some(&table.schema), n)?.is_some() {
                if b.if_not_exists {
                    ctx.notice(
                        Severity::Notice,
                        sqlstate::DUPLICATE_TABLE,
                        format!("relation \"{n}\" already exists, skipping"),
                        None,
                    );
                    return Ok("CREATE INDEX".into());
                }
                return Err(Error::new(
                    sqlstate::DUPLICATE_TABLE,
                    format!("relation \"{n}\" already exists"),
                ));
            }
            n.clone()
        }
        None => {
            let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
            choose_index_name(
                &table.name,
                table.namespace,
                &names,
                false,
                false,
                &ctx.name_lookup(),
                &HashSet::new(),
            )?
        }
    };
    // 7.
    let oid = ctx
        .db
        .catalog
        .get_new_relation_oid(ctx.cluster.oid_allocator())?;
    let locator = ctx.locator_for(oid);
    let def = IndexDef {
        oid,
        name: name.clone(),
        namespace: table.namespace,
        table_oid: table.oid,
        locator,
        columns: parts.iter().map(|p| p.column.clone()).collect(),
        unique: b.unique,
        primary: false,
        constraint: None,
    };
    let handle = IndexHandle::from_def(&def, table);
    let owner = table_owner(ctx, table)?;
    // 8.
    let w = ctx.write_ctx()?;
    ctx.create_file(&w, locator)?;
    ctx.index_store().init_index(&w, &handle)?;
    // 9.
    let stats = build_from_heap(ctx, &w, table, &def, &handle)?;
    // 10.
    ctx.db.catalog.create_index(
        &w,
        ctx.snapshot,
        &NewIndex {
            oid,
            name,
            namespace: table.namespace,
            owner,
            table_oid: table.oid,
            relfilenode: oid,
            columns,
            unique: b.unique,
            primary: false,
            constraint: None,
            stats,
        },
        true,
    )?;
    // 11.
    ctx.mark_catalog_dirty();
    Ok("CREATE INDEX".into())
}

pub(crate) fn system_catalog_error(name: &str) -> Error {
    Error::new(
        sqlstate::INSUFFICIENT_PRIVILEGE,
        format!("permission denied: \"{name}\" is a system catalog"),
    )
}

/// 表の所有者（`pg_class.relowner`）。索引は表の所有者のものになる。
pub(crate) fn table_owner(ctx: &DdlCtx<'_>, table: &TableDef) -> Result<Oid> {
    ctx.db
        .catalog
        .relation_row(ctx.snapshot, table.oid)?
        .map(|r| r.owner)
        .ok_or_else(|| Error::internal(format!("table {} has no pg_class row", table.oid)))
}

/// `build_from_heap` が使う部品。
pub(crate) struct BuildEnv<'a> {
    pub heap: &'a dyn TableStore,
    pub indexes: &'a dyn IndexStore,
    pub interrupts: &'a InterruptFlag,
    pub type_env: &'a TypeEnv<'a>,
    /// 自分の XID（`tuple_state` に渡す）。
    pub own: Option<Xid>,
}

struct Entry {
    key: Vec<Datum>,
    tid: Tid,
    live: bool,
}

/// ヒープ（可視性判定なし）を全走査し、索引に入れるべき `(key, tid)` を集めてソートし、
/// `IndexStore::build` で一括構築する（07 §5.2.1）。戻り値は `pg_class.relpages` / `reltuples` に書く値。
pub(crate) fn build_from_heap(
    ctx: &mut DdlCtx<'_>,
    w: &WriteCtx,
    table: &TableDef,
    index: &IndexDef,
    handle: &IndexHandle,
) -> Result<BuildStats> {
    let env = BuildEnv {
        heap: ctx.heap(),
        indexes: ctx.index_store(),
        interrupts: ctx.interrupts,
        type_env: ctx.type_env,
        own: ctx.txn.xid,
    };
    build_with(&env, w, table, index, handle)
}

pub(crate) fn build_with(
    env: &BuildEnv<'_>,
    w: &WriteCtx,
    table: &TableDef,
    index: &IndexDef,
    handle: &IndexHandle,
) -> Result<BuildStats> {
    let rel = RelHandle::from_table(table);
    let mut scan = env.heap.begin_scan_all(&rel)?;
    let mut entries: Vec<Entry> = Vec::new();
    while let Some(t) = env.heap.scan_next(&mut scan)? {
        env.interrupts.check()?;
        let live = match env.heap.tuple_state(&t, env.own)? {
            TupleState::InsertAborted => continue,
            TupleState::Live => true,
            TupleState::DeadCommitted | TupleState::DeletedBySelf => false,
            TupleState::InsertInProgress(x) | TupleState::DeleteInProgress(x) => {
                return Err(Error::internal(format!(
                    "cannot build index \"{}\": the heap holds a version of in-progress transaction {}",
                    index.name, x.0
                )));
            }
        };
        let key = index
            .columns
            .iter()
            .map(|c| {
                usize::try_from(c.attnum - 1)
                    .ok()
                    .and_then(|i| t.row.get(i))
                    .cloned()
                    .ok_or_else(|| {
                        Error::internal(format!("index column {} is not in the row", c.attnum))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        entries.push(Entry {
            key,
            tid: t.tid,
            live,
        });
    }
    entries.sort_by(|a, b| cmp_key_tid(handle, &a.key, a.tid, &b.key, b.tid));
    if index.unique {
        check_duplicates(env, table, index, handle, &entries)?;
    }
    let mut it = entries.into_iter().map(|e| (e.key, e.tid));
    env.indexes.build(w, handle, &mut it, BuildUnique::No)
}

/// 生きている版だけの重複を調べる（D07-7）。NULL を含むキーは重複とみなさない。
fn check_duplicates(
    env: &BuildEnv<'_>,
    table: &TableDef,
    index: &IndexDef,
    handle: &IndexHandle,
    entries: &[Entry],
) -> Result<()> {
    let mut prev: Option<&Entry> = None;
    for e in entries
        .iter()
        .filter(|e| e.live && !e.key.iter().any(Datum::is_null))
    {
        env.interrupts.check()?;
        if let Some(p) = prev
            && cmp_keys(handle, &p.key, &e.key).is_eq()
        {
            let names: Vec<&str> = handle.columns.iter().map(|c| c.name.as_str()).collect();
            let values: Vec<String> = handle
                .columns
                .iter()
                .zip(&e.key)
                .map(|(c, d)| output_text_env(d, c.ty, env.type_env).unwrap_or_default())
                .collect();
            return Err(Error::new(
                sqlstate::UNIQUE_VIOLATION,
                format!("could not create unique index \"{}\"", index.name),
            )
            .with_detail(format!(
                "Key ({})=({}) is duplicated.",
                names.join(", "),
                values.join(", ")
            ))
            .with_table(table.schema.clone(), table.name.clone())
            .with_constraint(index.name.clone()));
        }
        prev = Some(e);
    }
    Ok(())
}

pub fn drop_index(ctx: &mut DdlCtx<'_>, b: &BoundDropIndex) -> Result<String> {
    // 1.
    if b.concurrently {
        if ctx.in_transaction_block {
            return Err(Error::new(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "DROP INDEX CONCURRENTLY cannot run inside a transaction block",
            ));
        }
        if b.indexes.len() + b.missing.len() > 1 {
            return Err(Error::not_supported(
                "DROP INDEX CONCURRENTLY does not support dropping multiple objects",
            ));
        }
        if b.behavior == crate::catalog::depend::DropBehavior::Cascade {
            return Err(Error::not_supported(
                "DROP INDEX CONCURRENTLY does not support CASCADE",
            ));
        }
    }
    // 2.
    for name in &b.missing {
        ctx.notice(
            Severity::Notice,
            sqlstate::SUCCESSFUL_COMPLETION,
            format!("index \"{name}\" does not exist, skipping"),
            None,
        );
    }
    if b.indexes.is_empty() {
        return Ok("DROP INDEX".into());
    }
    // 3.
    let roots: Vec<ObjectAddress> = b
        .indexes
        .iter()
        .map(|i| ObjectAddress::relation(i.oid))
        .collect();
    let visible = ctx.catalog.visible_namespaces()?;
    let plan = plan_drop(&ctx.db.catalog, ctx.snapshot, &roots, b.behavior, &visible)?;
    // 4.
    if let Some((message, detail)) = cascade_notice(&plan) {
        ctx.notice(
            Severity::Notice,
            sqlstate::SUCCESSFUL_COMPLETION,
            message,
            detail,
        );
    }
    // 5.
    let w = ctx.write_ctx()?;
    let locators = ctx.db.catalog.drop_objects(&w, ctx.snapshot, &plan)?;
    // 6.
    ctx.schedule_unlinks(&locators)?;
    // 7.
    ctx.mark_catalog_dirty();
    Ok("DROP INDEX".into())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::analyzer::query::{BoundCreateIndex, BoundDdl, BoundDropIndex, BoundIndexColumn};
    use crate::catalog::CatalogReader;
    use crate::catalog::depend::DropBehavior;
    use crate::catalog::fake::table_def;
    use crate::ddl::testkit::{Harness, col, create_table_ddl, key};
    use crate::executor::nodes::test_util::FakeIndexStore;
    use crate::storage::smgr::RelFileLocator;
    use crate::storage::{DirtyResult, HeapScan, HeapTuple, TmResult, UpdateOutcome};
    use crate::txn::Snapshot;
    use crate::types::Row;

    // ----- resolve_opclass --------------------------------------------------------

    fn opclass_error(ty: SqlType, requested: Option<&str>) -> Error {
        resolve_opclass(ty, requested).unwrap_err()
    }

    #[test]
    fn default_opclasses_by_type() {
        assert_eq!(
            resolve_opclass(SqlType::INT4, None).unwrap().name,
            "int4_ops"
        );
        assert_eq!(
            resolve_opclass(SqlType::TEXT, None).unwrap().name,
            "text_ops"
        );
        // varchar(5) は text_ops、regclass は oid_ops（バイナリ互換）。
        assert_eq!(
            resolve_opclass(SqlType::new(crate::types::oid::VARCHAR, 9), None)
                .unwrap()
                .name,
            "text_ops"
        );
        assert_eq!(
            resolve_opclass(SqlType::REGCLASS, None).unwrap().name,
            "oid_ops"
        );
    }

    #[test]
    fn explicit_opclass_rules() {
        assert_eq!(
            resolve_opclass(SqlType::INT4, Some("int4_ops"))
                .unwrap()
                .name,
            "int4_ops"
        );
        assert_eq!(
            resolve_opclass(SqlType::INT4, Some("pg_catalog.int4_ops"))
                .unwrap()
                .name,
            "int4_ops"
        );
        // varchar 列に varchar_ops / text_ops。
        let varchar = SqlType::new(crate::types::oid::VARCHAR, 9);
        assert!(resolve_opclass(varchar, Some("varchar_ops")).is_ok());
        assert!(resolve_opclass(varchar, Some("text_ops")).is_ok());

        let e = opclass_error(SqlType::INT4, Some("nosuch_ops"));
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(
            e.message,
            "operator class \"nosuch_ops\" does not exist for access method \"btree\""
        );
        // pg_catalog 以外の修飾は存在しないものとして扱う。
        let e = opclass_error(SqlType::INT4, Some("public.int4_ops"));
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);

        let e = opclass_error(SqlType::INT4, Some("text_ops"));
        assert_eq!(e.sqlstate, sqlstate::DATATYPE_MISMATCH);
        assert_eq!(
            e.message,
            "operator class \"text_ops\" does not accept data type integer"
        );
    }

    #[test]
    fn a_type_without_an_opclass_is_rejected_with_a_hint() {
        let e = opclass_error(SqlType::of(crate::types::oid::XID), None);
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(
            e.message,
            "data type xid has no default operator class for access method \"btree\""
        );
        assert!(
            e.hint
                .as_deref()
                .unwrap()
                .starts_with("You must specify an operator class")
        );
    }

    #[test]
    fn key_parts_use_default_opclasses_and_name_duplicates() {
        let cols = vec![col("a", 1, SqlType::INT4), col("b", 2, SqlType::TEXT)];
        let parts = default_key_parts(&cols, &[1, 2, 1]).unwrap();
        let new_cols = new_index_columns(&parts);
        let labels: Vec<&str> = new_cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(labels, ["a", "b", "a1"]);
        assert!(
            parts
                .iter()
                .all(|p| !p.column.descending && !p.column.nulls_first)
        );
        assert!(default_key_parts(&cols, &[3]).is_err());
        let xid_cols = vec![col("x", 1, SqlType::of(crate::types::oid::XID))];
        assert_eq!(
            default_key_parts(&xid_cols, &[1]).unwrap_err().sqlstate,
            sqlstate::UNDEFINED_OBJECT
        );
    }

    // ----- build_from_heap -------------------------------------------------------

    /// 版ごとの状態を指定できる `TableStore`（`begin_scan_all` と `tuple_state` だけ）。
    #[derive(Debug)]
    struct ScriptedHeap {
        tuples: Vec<(HeapTuple, TupleState)>,
    }

    impl ScriptedHeap {
        fn new(rows: Vec<(Row, TupleState)>) -> ScriptedHeap {
            ScriptedHeap {
                tuples: rows
                    .into_iter()
                    .enumerate()
                    .map(|(i, (row, st))| {
                        (
                            HeapTuple {
                                tid: Tid {
                                    block: 0,
                                    offset: u16::try_from(i + 1).unwrap(),
                                },
                                xmin: Xid(3),
                                xmax: Xid::INVALID,
                                cmin: 0,
                                cmax: 0,
                                row,
                            },
                            st,
                        )
                    })
                    .collect(),
            }
        }
    }

    fn unused<T>() -> Result<T> {
        Err(Error::internal("not used by the test"))
    }

    impl TableStore for ScriptedHeap {
        fn create_storage(&self, _: &WriteCtx, _: RelFileLocator) -> Result<()> {
            Ok(())
        }
        fn storage_exists(&self, _: RelFileLocator) -> Result<bool> {
            Ok(true)
        }
        fn unlink_storage(&self, _: RelFileLocator) -> Result<()> {
            Ok(())
        }
        fn insert(&self, _: &RelHandle, _: &WriteCtx, _: &[Datum]) -> Result<Tid> {
            unused()
        }
        fn delete(&self, _: &RelHandle, _: &WriteCtx, _: &Snapshot, _: Tid) -> Result<TmResult> {
            unused()
        }
        fn update(
            &self,
            _: &RelHandle,
            _: &WriteCtx,
            _: &Snapshot,
            _: Tid,
            _: &[Datum],
        ) -> Result<UpdateOutcome> {
            unused()
        }
        fn begin_scan(&self, _: &RelHandle, _: &Snapshot) -> Result<HeapScan> {
            unused()
        }
        fn scan_next(&self, scan: &mut HeapScan) -> Result<Option<HeapTuple>> {
            Ok(scan.pop_buffered())
        }
        fn fetch(&self, _: &RelHandle, _: &Snapshot, _: Tid) -> Result<Option<HeapTuple>> {
            Ok(None)
        }
        fn begin_scan_all(&self, rel: &RelHandle) -> Result<HeapScan> {
            let snap = crate::catalog::store::snapshot_any();
            Ok(HeapScan::from_tuples(
                rel.clone(),
                snap,
                self.tuples.iter().map(|(t, _)| t.clone()).collect(),
            ))
        }
        fn tuple_state(&self, t: &HeapTuple, _: Option<Xid>) -> Result<TupleState> {
            self.tuples
                .iter()
                .find(|(x, _)| x.tid == t.tid)
                .map(|(_, s)| *s)
                .ok_or_else(|| Error::internal("unknown tuple"))
        }
        fn fetch_dirty(&self, _: &RelHandle, _: Option<Xid>, _: Tid) -> Result<DirtyResult> {
            unused()
        }
    }

    struct Fixture {
        table: TableDef,
        def: IndexDef,
        handle: IndexHandle,
    }

    /// `t(a int4, b text)` と、`(attnum, descending)` の列を持つ索引。
    fn fixture(unique: bool, keys: &[(i16, bool)]) -> Fixture {
        let table = table_def(
            16400,
            "t",
            vec![col("a", 1, SqlType::INT4), col("b", 2, SqlType::TEXT)],
            vec![],
        );
        let columns: Vec<IndexColumn> = keys
            .iter()
            .map(|&(attnum, descending)| {
                let ty = table.columns[usize::try_from(attnum - 1).unwrap()].ty;
                let oc = resolve_opclass(ty, None).unwrap();
                IndexColumn {
                    attnum,
                    opclass: oc.oid,
                    opfamily: oc.family,
                    descending,
                    nulls_first: descending,
                }
            })
            .collect();
        let def = IndexDef {
            oid: 16401,
            name: "t_idx".into(),
            namespace: table.namespace,
            table_oid: table.oid,
            locator: RelFileLocator {
                spc_oid: 1663,
                db_oid: 5,
                rel_number: crate::storage::smgr::RelFileNumber(16401),
            },
            columns,
            unique,
            primary: false,
            constraint: None,
        };
        let handle = IndexHandle::from_def(&def, &table);
        Fixture { table, def, handle }
    }

    fn int_text(a: Option<i32>, b: &str) -> Row {
        vec![a.map_or(Datum::Null, Datum::Int4), Datum::Text(b.into())]
    }

    fn build(f: &Fixture, heap: &ScriptedHeap, indexes: &FakeIndexStore) -> Result<BuildStats> {
        let intr = InterruptFlag::default();
        let type_env = TypeEnv::default();
        let env = BuildEnv {
            heap,
            indexes,
            interrupts: &intr,
            type_env: &type_env,
            own: Some(Xid(3)),
        };
        let w = WriteCtx {
            xid: Xid(3),
            cid: 0,
        };
        indexes.init_index(&w, &f.handle).unwrap();
        build_with(&env, &w, &f.table, &f.def, &f.handle)
    }

    fn tids(indexes: &FakeIndexStore) -> Vec<u16> {
        indexes
            .entries(16401)
            .into_iter()
            .map(|(_, t)| t.offset)
            .collect()
    }

    #[test]
    fn build_sorts_by_key_then_tid_and_skips_aborted_inserts() {
        let f = fixture(false, &[(1, false)]);
        let heap = ScriptedHeap::new(vec![
            (int_text(Some(3), "c"), TupleState::Live),
            (int_text(Some(1), "a"), TupleState::Live),
            (int_text(Some(2), "x"), TupleState::InsertAborted),
            (int_text(Some(1), "d"), TupleState::DeadCommitted),
            (int_text(Some(2), "e"), TupleState::DeletedBySelf),
            (int_text(None, "n"), TupleState::Live),
        ]);
        let indexes = FakeIndexStore::default();
        let stats = build(&f, &heap, &indexes).unwrap();
        // 1(tid 2)、1(tid 4)、2(tid 5)、3(tid 1)、NULL（既定の NULLS LAST）。
        assert_eq!(tids(&indexes), vec![2, 4, 5, 1, 6]);
        assert_eq!(stats.tuples, 5);
    }

    #[test]
    fn build_honours_descending_and_nulls_first() {
        let f = fixture(false, &[(1, true)]);
        let heap = ScriptedHeap::new(vec![
            (int_text(Some(1), "a"), TupleState::Live),
            (int_text(None, "n"), TupleState::Live),
            (int_text(Some(2), "b"), TupleState::Live),
        ]);
        let indexes = FakeIndexStore::default();
        build(&f, &heap, &indexes).unwrap();
        // DESC は NULLS FIRST も付く: NULL、2、1。
        assert_eq!(tids(&indexes), vec![2, 3, 1]);
    }

    #[test]
    fn unique_build_ignores_dead_versions_and_nulls() {
        let f = fixture(true, &[(1, false)]);
        let heap = ScriptedHeap::new(vec![
            (int_text(Some(1), "dead"), TupleState::DeadCommitted),
            (int_text(Some(1), "own-dead"), TupleState::DeletedBySelf),
            (int_text(Some(1), "live"), TupleState::Live),
            (int_text(None, "n1"), TupleState::Live),
            (int_text(None, "n2"), TupleState::Live),
        ]);
        let indexes = FakeIndexStore::default();
        let stats = build(&f, &heap, &indexes).unwrap();
        assert_eq!(stats.tuples, 5);
    }

    #[test]
    fn unique_build_reports_the_first_duplicate() {
        let f = fixture(true, &[(1, false), (2, false)]);
        let heap = ScriptedHeap::new(vec![
            (int_text(Some(2), "y"), TupleState::Live),
            (int_text(Some(1), "x"), TupleState::Live),
            (int_text(Some(1), "x"), TupleState::Live),
            (int_text(Some(1), "x"), TupleState::Live),
            (int_text(Some(2), "y"), TupleState::Live),
        ]);
        let indexes = FakeIndexStore::default();
        let e = build(&f, &heap, &indexes).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNIQUE_VIOLATION);
        assert_eq!(e.message, "could not create unique index \"t_idx\"");
        assert_eq!(
            e.detail.as_deref(),
            Some("Key (a, b)=(1, x) is duplicated.")
        );
        assert_eq!(e.schema(), Some("public"));
        assert_eq!(e.table(), Some("t"));
        assert_eq!(e.constraint(), Some("t_idx"));
        // 何も入れずに失敗する。
        assert_eq!(indexes.len(16401), 0);
    }

    #[test]
    fn unique_build_does_not_cut_long_keys() {
        let f = fixture(true, &[(2, false)]);
        let long = "x".repeat(5000);
        let heap = ScriptedHeap::new(vec![
            (int_text(Some(1), &long), TupleState::Live),
            (int_text(Some(2), &long), TupleState::Live),
        ]);
        let e = build(&f, &heap, &FakeIndexStore::default()).unwrap_err();
        assert_eq!(
            e.detail.as_deref(),
            Some(format!("Key (b)=({long}) is duplicated.").as_str())
        );
    }

    #[test]
    fn build_fails_on_versions_of_other_transactions() {
        for state in [
            TupleState::InsertInProgress(Xid(9)),
            TupleState::DeleteInProgress(Xid(9)),
        ] {
            let f = fixture(false, &[(1, false)]);
            let heap = ScriptedHeap::new(vec![(int_text(Some(1), "a"), state)]);
            let e = build(&f, &heap, &FakeIndexStore::default()).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        }
    }

    #[test]
    fn build_checks_interrupts_per_row() {
        let f = fixture(false, &[(1, false)]);
        let heap = ScriptedHeap::new(vec![(int_text(Some(1), "a"), TupleState::Live)]);
        let indexes = FakeIndexStore::default();
        let intr = InterruptFlag::default();
        intr.request_cancel();
        let type_env = TypeEnv::default();
        let env = BuildEnv {
            heap: &heap,
            indexes: &indexes,
            interrupts: &intr,
            type_env: &type_env,
            own: None,
        };
        let w = WriteCtx {
            xid: Xid(3),
            cid: 0,
        };
        let e = build_with(&env, &w, &f.table, &f.def, &f.handle).unwrap_err();
        assert_eq!(e.sqlstate.0, "57014");
    }

    // ----- CREATE INDEX / DROP INDEX（実際の TestCluster）----------------------

    fn setup_table(h: &mut Harness, rows: &[(Option<i32>, &str)]) -> Arc<TableDef> {
        h.exec_w(BoundDdl::CreateTable(create_table_ddl(
            "t",
            vec![col("a", 1, SqlType::INT4), col("b", 2, SqlType::TEXT)],
        )))
        .unwrap();
        let t = h.table("t").unwrap();
        let rows: Vec<Row> = rows.iter().map(|(a, b)| int_text(*a, b)).collect();
        h.run(|ctx| {
            let w = ctx.write_ctx()?;
            let rel = RelHandle::from_table(&t);
            for r in &rows {
                ctx.heap().insert(&rel, &w, r)?;
            }
            Ok(())
        })
        .unwrap();
        h.table("t").unwrap()
    }

    fn create_index_ddl(
        t: &Arc<TableDef>,
        name: Option<&str>,
        cols: &[(i16, bool)],
    ) -> BoundCreateIndex {
        BoundCreateIndex {
            table: Arc::clone(t),
            name: name.map(str::to_owned),
            unique: false,
            if_not_exists: false,
            concurrently: false,
            method: "btree".into(),
            columns: cols
                .iter()
                .map(|&(attnum, desc)| BoundIndexColumn {
                    attnum,
                    opclass: None,
                    descending: desc,
                    nulls_first: desc,
                    span: crate::error::Span::default(),
                })
                .collect(),
            options: Vec::new(),
        }
    }

    fn error_of(r: Result<String>) -> Error {
        r.unwrap_err()
    }

    #[test]
    fn create_index_validation_errors_come_before_any_write() {
        let mut h = Harness::new();
        let t = setup_table(&mut h, &[]);
        let mut b = create_index_ddl(&t, Some("i1"), &[(1, false)]);
        b.concurrently = true;
        h.in_block = true;
        let e = error_of(h.exec(BoundDdl::CreateIndex(b)));
        assert_eq!(e.sqlstate, sqlstate::ACTIVE_SQL_TRANSACTION);
        assert_eq!(
            e.message,
            "CREATE INDEX CONCURRENTLY cannot run inside a transaction block"
        );
        h.in_block = false;

        let mut b = create_index_ddl(&t, Some("i1"), &[(1, false)]);
        b.method = "hash".into();
        let e = error_of(h.exec(BoundDdl::CreateIndex(b)));
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            e.message,
            "index access method \"hash\" is not supported yet"
        );
        let mut b = create_index_ddl(&t, Some("i1"), &[(1, false)]);
        b.method = "nosuch".into();
        let e = error_of(h.exec(BoundDdl::CreateIndex(b)));
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(e.message, "access method \"nosuch\" does not exist");

        let cols: Vec<(i16, bool)> = vec![(1, false); 33];
        let e = error_of(h.exec(BoundDdl::CreateIndex(create_index_ddl(&t, None, &cols))));
        assert_eq!(e.sqlstate, sqlstate::TOO_MANY_COLUMNS);
        assert_eq!(e.message, "cannot use more than 32 columns in an index");

        let mut b = create_index_ddl(&t, Some("i1"), &[(1, false)]);
        b.columns[0].opclass = Some("text_ops".into());
        let e = error_of(h.exec(BoundDdl::CreateIndex(b)));
        assert_eq!(e.sqlstate, sqlstate::DATATYPE_MISMATCH);

        let mut b = create_index_ddl(&t, Some("i1"), &[(1, false)]);
        b.options = vec![crate::analyzer::query::RelOption {
            namespace: None,
            name: "fillfactor".into(),
            value: Some("5".into()),
        }];
        let e = error_of(h.exec(BoundDdl::CreateIndex(b)));
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);

        let pg_class = h.reader(|c| c.table(Some("pg_catalog"), "pg_class").unwrap().unwrap());
        let e = error_of(h.exec(BoundDdl::CreateIndex(create_index_ddl(
            &pg_class,
            None,
            &[(1, false)],
        ))));
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        assert_eq!(
            e.message,
            "permission denied: \"pg_class\" is a system catalog"
        );
        assert!(h.txn.pending_creates.len() == 1, "only the table's file");
        h.rollback();
    }

    #[test]
    fn create_index_names_and_notices() {
        let mut h = Harness::new();
        let t = setup_table(&mut h, &[(Some(1), "a"), (Some(2), "b")]);
        h.exec(BoundDdl::CreateIndex(create_index_ddl(
            &t,
            None,
            &[(1, true)],
        )))
        .unwrap();
        let t = h.table("t").unwrap();
        let idx = &t.indexes[0];
        assert_eq!(idx.name, "t_a_idx");
        assert_eq!(idx.indoption(0), 3);
        assert!(!idx.unique && !idx.primary && idx.constraint.is_none());
        // 同じ列でも名前は連番。
        h.exec(BoundDdl::CreateIndex(create_index_ddl(
            &t,
            None,
            &[(1, false)],
        )))
        .unwrap();
        let t = h.table("t").unwrap();
        let names: Vec<&str> = t.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["t_a_idx", "t_a_idx1"]);
        // 明示名の衝突。
        let e = error_of(h.exec(BoundDdl::CreateIndex(create_index_ddl(
            &t,
            Some("t_a_idx"),
            &[(2, false)],
        ))));
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_TABLE);
        assert_eq!(e.message, "relation \"t_a_idx\" already exists");
        // IF NOT EXISTS は NOTICE。
        let mut b = create_index_ddl(&t, Some("t_a_idx"), &[(2, false)]);
        b.if_not_exists = true;
        h.notices.clear();
        assert_eq!(h.exec(BoundDdl::CreateIndex(b)).unwrap(), "CREATE INDEX");
        assert_eq!(h.notices.len(), 1);
        assert_eq!(
            h.notices[0].message,
            "relation \"t_a_idx\" already exists, skipping"
        );
        assert_eq!(h.notices[0].sqlstate, sqlstate::DUPLICATE_TABLE);
        // 表の relhasindex が立つ。
        assert!(
            h.db.catalog
                .relation_row(
                    &h.tc.cluster.txn_manager().snapshot(h.txn.xid, h.txn.cid),
                    t.oid
                )
                .unwrap()
                .unwrap()
                .has_index
        );
        h.commit();
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn create_unique_index_with_duplicates_fails_and_rolls_back() {
        let mut h = Harness::new();
        let t = setup_table(&mut h, &[(Some(1), "a"), (Some(1), "b")]);
        let mut b = create_index_ddl(&t, Some("u1"), &[(1, false)]);
        b.unique = true;
        let e = h.exec(BoundDdl::CreateIndex(b)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNIQUE_VIOLATION);
        assert_eq!(e.message, "could not create unique index \"u1\"");
        assert_eq!(e.detail.as_deref(), Some("Key (a)=(1) is duplicated."));
        assert_eq!(e.constraint(), Some("u1"));
        let files = h.txn.pending_creates.clone();
        assert_eq!(files.len(), 2, "the table and the half-built index");
        h.rollback();
        assert!(files.iter().all(|l| h.file_removed(*l)));
        assert!(h.table("t").is_none());
        assert!(h.check().is_empty());
    }

    #[test]
    fn drop_index_removes_rows_and_files_at_commit() {
        let mut h = Harness::new();
        let t = setup_table(&mut h, &[(Some(1), "a")]);
        h.exec(BoundDdl::CreateIndex(create_index_ddl(
            &t,
            Some("i1"),
            &[(1, false)],
        )))
        .unwrap();
        h.commit();
        let t = h.table("t").unwrap();
        let loc = t.indexes[0].locator;
        assert!(h.file_exists(loc));
        let drop = |t: &Arc<TableDef>, behavior| {
            BoundDdl::DropIndex(BoundDropIndex {
                indexes: vec![Arc::clone(&t.indexes[0])],
                missing: vec!["nosuch".into()],
                behavior,
                concurrently: false,
            })
        };
        h.begin();
        assert_eq!(
            h.exec(drop(&t, DropBehavior::Restrict)).unwrap(),
            "DROP INDEX"
        );
        assert_eq!(
            h.notices.last().unwrap().message,
            "index \"nosuch\" does not exist, skipping"
        );
        // ロールバックなら何も消えない。
        h.rollback();
        assert!(h.file_exists(loc));
        assert_eq!(h.table("t").unwrap().indexes.len(), 1);
        h.begin();
        h.exec(drop(&t, DropBehavior::Restrict)).unwrap();
        h.commit();
        assert!(h.file_removed(loc));
        let t = h.table("t").unwrap();
        assert!(t.indexes.is_empty());
        // 表の relhasindex は下ろさない（D07-9）。
        let snap = h.tc.cluster.txn_manager().snapshot(None, 0);
        assert!(
            h.db.catalog
                .relation_row(&snap, t.oid)
                .unwrap()
                .unwrap()
                .has_index
        );
        assert!(h.check().is_empty());
    }

    #[test]
    fn drop_index_concurrently_rules() {
        let mut h = Harness::new();
        let t = setup_table(&mut h, &[]);
        h.exec(BoundDdl::CreateIndex(create_index_ddl(
            &t,
            Some("i1"),
            &[(1, false)],
        )))
        .unwrap();
        let t = h.table("t").unwrap();
        let mk = |behavior, concurrently, missing: &[&str]| {
            BoundDdl::DropIndex(BoundDropIndex {
                indexes: vec![Arc::clone(&t.indexes[0])],
                missing: missing.iter().map(|s| (*s).to_owned()).collect(),
                behavior,
                concurrently,
            })
        };
        h.in_block = true;
        let e = error_of(h.exec(mk(DropBehavior::Restrict, true, &[])));
        assert_eq!(
            e.message,
            "DROP INDEX CONCURRENTLY cannot run inside a transaction block"
        );
        h.in_block = false;
        let e = error_of(h.exec(mk(DropBehavior::Restrict, true, &["x"])));
        assert_eq!(
            e.message,
            "DROP INDEX CONCURRENTLY does not support dropping multiple objects"
        );
        let e = error_of(h.exec(mk(DropBehavior::Cascade, true, &[])));
        assert_eq!(
            e.message,
            "DROP INDEX CONCURRENTLY does not support CASCADE"
        );
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        h.rollback();
    }

    #[test]
    fn drop_of_a_constraint_index_is_refused_even_with_cascade() {
        let mut h = Harness::new();
        let mut ct = create_table_ddl("d1", vec![col("a", 1, SqlType::INT4)]);
        ct.constraints = vec![key(
            crate::analyzer::query::IndexConstraintKind::PrimaryKey,
            None,
            &[1],
        )];
        h.exec_w(BoundDdl::CreateTable(ct)).unwrap();
        h.commit();
        let t = h.table("d1").unwrap();
        h.begin();
        for behavior in [DropBehavior::Restrict, DropBehavior::Cascade] {
            let e = error_of(h.exec(BoundDdl::DropIndex(BoundDropIndex {
                indexes: vec![Arc::clone(&t.indexes[0])],
                missing: vec![],
                behavior,
                concurrently: false,
            })));
            assert_eq!(e.sqlstate, sqlstate::DEPENDENT_OBJECTS_STILL_EXIST);
            assert_eq!(
                e.message,
                "cannot drop index d1_pkey because constraint d1_pkey on table d1 requires it"
            );
            assert_eq!(
                e.hint.as_deref(),
                Some("You can drop constraint d1_pkey on table d1 instead.")
            );
        }
        h.rollback();
    }
}
