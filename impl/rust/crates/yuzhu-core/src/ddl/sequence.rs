//! `CREATE / ALTER / DROP SEQUENCE`（`m4/08-sequence-serial.md` §5.4〜§5.6）と、表との連携
//! （`create_with_oid`・`drop_owned_by_table`・`restart_owned_by_table`。11 §7.1 の C-28）。
//!
//! 依存関係と DROP の閉包は 07 の `catalog::depend`（`plan_drop` / `CatalogStore::drop_objects`）に任せる。
//! SERIAL の `DEFAULT nextval('<oid>'::regclass)` と `pg_attrdef → シーケンス` の依存は `ddl::table` が書く。

use super::DdlCtx;
use crate::analyzer::query::{
    BoundAlterAction, BoundAlterSequence, BoundCreateSequence, BoundDropSequence, OwnedByTarget,
    SeqOwner,
};
use crate::catalog::depend::{
    DependType, DropBehavior, DropItem, DropKind, DropPlan, NewDepend, ObjectAddress,
    cascade_notice, plan_drop,
};
use crate::catalog::seq_params::{InitMode, init_params};
use crate::catalog::store::{ClassPatch, DependFilter, NewSequence};
use crate::catalog::{RelKind, SequenceParams, TableDef};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::executor::seq::handle_from_def;
use crate::storage::smgr::{RelFileLocator, RelFileNumber};
use crate::storage::{SequenceHandle, WriteCtx};
use crate::types::Oid;
use crate::wal::Lsn;

const CLASS_RELATION: Oid = 1259;

/// `CREATE SEQUENCE`。
pub fn create_sequence(ctx: &mut DdlCtx<'_>, c: &BoundCreateSequence) -> Result<String> {
    const TAG: &str = "CREATE SEQUENCE";
    if ctx
        .catalog
        .relation_kind(Some(&c.schema), &c.name)?
        .is_some()
    {
        if c.if_not_exists {
            ctx.notice(
                Severity::Notice,
                sqlstate::DUPLICATE_TABLE,
                format!("relation \"{}\" already exists, skipping", c.name),
                None,
            );
            return Ok(TAG.into());
        }
        return Err(Error::new(
            sqlstate::DUPLICATE_TABLE,
            format!("relation \"{}\" already exists", c.name),
        ));
    }
    let oid = ctx
        .db
        .catalog
        .get_new_relation_oid(ctx.cluster.oid_allocator())?;
    create_with_oid(ctx, c, oid, None)?;
    Ok(TAG.into())
}

/// OID が決まっているシーケンスを作る。`CREATE TABLE`（07 §5.1 の手順 6）が、表とシーケンスの OID を
/// 先に採った後で呼ぶ。`table_ref` は所有する表の `(OID, attnum)`（11 の C-28）。
pub fn create_with_oid(
    ctx: &mut DdlCtx<'_>,
    c: &BoundCreateSequence,
    oid: Oid,
    table_ref: Option<(Oid, i16)>,
) -> Result<()> {
    let w = ctx.write_ctx()?;
    let locator = ctx.locator_for(oid);
    ctx.create_file(&w, locator)?;
    let handle = SequenceHandle {
        oid,
        name: c.name.clone(),
        locator,
        params: c.params,
    };
    let seq = ctx.cluster.sequences();
    let mut lsn = seq.init(&w, &handle)?;
    let st = c.initial;
    if st.last_value != c.params.start || st.log_cnt != 0 || st.is_called {
        lsn = seq.reset(&handle, &c.params, Some(st.last_value))?;
    }
    ctx.txn.note_wal(lsn);

    let owned = match c.owner {
        SeqOwner::Column { table, attnum } => Some((table, attnum)),
        SeqOwner::None | SeqOwner::NewTableColumn { .. } => table_ref,
    };
    let mut params = c.params;
    params.owned_by = owned;
    ctx.db.catalog.create_sequence(
        &w,
        ctx.snapshot,
        &NewSequence {
            oid,
            name: c.name.clone(),
            namespace: c.namespace,
            owner: ctx.role_oid,
            params,
            owned_by_deptype: owned.map(|_| {
                if c.for_identity {
                    DependType::Internal
                } else {
                    DependType::Auto
                }
            }),
        },
    )?;
    ctx.mark_catalog_dirty();
    Ok(())
}

/// `ALTER SEQUENCE`（08 §5.5）。
pub fn alter_sequence(ctx: &mut DdlCtx<'_>, a: &BoundAlterSequence) -> Result<String> {
    const TAG: &str = "ALTER SEQUENCE";
    let Some(def) = &a.target else {
        ctx.notice(
            Severity::Notice,
            sqlstate::SUCCESSFUL_COMPLETION,
            format!("relation \"{}\" does not exist, skipping", a.missing_name),
            None,
        );
        return Ok(TAG.into());
    };
    let BoundAlterAction::Options { options, owned_by } = &a.action else {
        return Ok(TAG.into());
    };
    let w = ctx.write_ctx()?;
    let handle = handle_from_def(def)?;
    if !options.is_empty() {
        let seq = ctx.cluster.sequences();
        let state = seq.read(&handle)?;
        let out = init_params(
            options,
            false,
            InitMode::Alter {
                current: &handle.params,
                state,
            },
        )?;
        let new = SequenceParams {
            owned_by: handle.params.owned_by,
            ..out.params
        };
        let restart = out.restarted.then_some(out.state.last_value);
        let lsn = rewrite_file(ctx, &w, def, &handle, &new, restart)?;
        ctx.db
            .catalog
            .update_sequence_params(&w, ctx.snapshot, def.oid, &new)?;
        ctx.txn.note_wal(lsn);
    }
    if let Some(target) = owned_by {
        process_owned_by(ctx, def, *target)?;
    }
    ctx.mark_catalog_dirty();
    Ok(TAG.into())
}

/// `OWNED BY` の実行側（`process_owned_by`。08 §5.5）。IDENTITY のシーケンスの所有は変えられない。
fn process_owned_by(ctx: &mut DdlCtx<'_>, def: &TableDef, target: OwnedByTarget) -> Result<()> {
    let w = ctx.write_ctx()?;
    let seq_addr = ObjectAddress::relation(def.oid);
    let deps = ctx.db.catalog.references_of(ctx.snapshot, seq_addr)?;
    let column_deps = |ty: DependType| {
        deps.iter()
            .filter(move |d| d.referenced.class_id == CLASS_RELATION && d.deptype == ty)
    };
    if let Some(d) = column_deps(DependType::Internal).next() {
        let table = ctx
            .catalog
            .table_by_oid(d.referenced.obj_id)?
            .map_or_else(|| d.referenced.obj_id.to_string(), |t| t.name.clone());
        return Err(Error::new(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot change ownership of identity sequence",
        )
        .with_detail(format!(
            "Sequence \"{}\" is linked to table \"{table}\".",
            def.name
        )));
    }
    for d in column_deps(DependType::Auto) {
        ctx.db.catalog.delete_dependencies(
            &w,
            ctx.snapshot,
            DependFilter::Exact {
                dependent: d.dependent,
                referenced: d.referenced,
            },
        )?;
    }
    if let OwnedByTarget::Column { table, attnum } = target {
        ctx.db.catalog.record_dependency(
            &w,
            &NewDepend {
                dependent: seq_addr,
                referenced: ObjectAddress::column(table, attnum),
                deptype: DependType::Auto,
            },
        )?;
    }
    Ok(())
}

/// `DROP SEQUENCE`（08 §5.6）。
pub fn drop_sequence(ctx: &mut DdlCtx<'_>, d: &BoundDropSequence) -> Result<String> {
    const TAG: &str = "DROP SEQUENCE";
    for name in &d.missing {
        ctx.notice(
            Severity::Notice,
            sqlstate::SUCCESSFUL_COMPLETION,
            format!("sequence \"{name}\" does not exist, skipping"),
            None,
        );
    }
    if d.targets.is_empty() {
        return Ok(TAG.into());
    }
    let roots: Vec<ObjectAddress> = d
        .targets
        .iter()
        .map(|t| ObjectAddress::relation(t.oid))
        .collect();
    let behavior = if d.cascade {
        DropBehavior::Cascade
    } else {
        DropBehavior::Restrict
    };
    let visible = ctx.catalog.visible_namespaces()?;
    let plan = plan_drop(&ctx.db.catalog, ctx.snapshot, &roots, behavior, &visible)?;
    if let Some((message, detail)) = cascade_notice(&plan) {
        ctx.notice(
            Severity::Notice,
            sqlstate::SUCCESSFUL_COMPLETION,
            message,
            detail,
        );
    }
    let w = ctx.write_ctx()?;
    let locators = ctx.db.catalog.drop_objects(&w, ctx.snapshot, &plan)?;
    ctx.schedule_unlinks(&locators)?;
    ctx.mark_catalog_dirty();
    Ok(TAG.into())
}

/// 表の列が所有するシーケンス（`a` と `i`）を、依存の検査なしに落とす。表の DEFAULT は表と一緒に消える
/// ので `pg_attrdef` には触らない。
pub fn drop_owned_by_table(ctx: &mut DdlCtx<'_>, table: Oid) -> Result<()> {
    let seqs = ctx.db.catalog.owned_sequences(ctx.snapshot, table)?;
    if seqs.is_empty() {
        return Ok(());
    }
    let mut items = Vec::with_capacity(seqs.len());
    for oid in seqs {
        let def = ctx
            .catalog
            .table_by_oid(oid)?
            .filter(|d| d.kind == RelKind::Sequence)
            .ok_or_else(|| Error::internal(format!("sequence {oid} not found")))?;
        items.push(DropItem {
            addr: ObjectAddress::relation(oid),
            kind: DropKind::Sequence,
            description: format!("sequence {}", def.name),
            locator: Some(def.locator),
            owner_table: None,
            attnum: None,
        });
    }
    let plan = DropPlan {
        items,
        cascaded: Vec::new(),
    };
    let w = ctx.write_ctx()?;
    let locators = ctx.db.catalog.drop_objects(&w, ctx.snapshot, &plan)?;
    ctx.schedule_unlinks(&locators)?;
    ctx.mark_catalog_dirty();
    Ok(())
}

/// `TRUNCATE ... RESTART IDENTITY`: 表が所有するシーケンス（SERIAL も）を開始値に戻す。
pub fn restart_owned_by_table(ctx: &mut DdlCtx<'_>, table: Oid) -> Result<()> {
    for oid in ctx.db.catalog.owned_sequences(ctx.snapshot, table)? {
        let def = ctx
            .catalog
            .table_by_oid(oid)?
            .ok_or_else(|| Error::internal(format!("sequence {oid} not found")))?;
        let h = handle_from_def(&def)?;
        let w = ctx.write_ctx()?;
        let lsn = rewrite_file(ctx, &w, &def, &h, &h.params, Some(h.params.start))?;
        ctx.txn.note_wal(lsn);
    }
    Ok(())
}

/// シーケンスの状態を新しい relfilenode のファイルに書き直す（PostgreSQL の
/// `RelationSetNewRelfilenumber` と同じ。その場の書き換えはロールバックで戻らない）。
/// `restart` が `None` なら現在の状態を引き継ぐ。旧ファイルはコミットで消え、新しいファイルは
/// ロールバックで消える。`pg_class.relfilenode` を更新する。
fn rewrite_file(
    ctx: &mut DdlCtx<'_>,
    w: &WriteCtx,
    def: &TableDef,
    old: &SequenceHandle,
    params: &SequenceParams,
    restart: Option<i64>,
) -> Result<Lsn> {
    let seq = ctx.cluster.sequences();
    let cur = seq.read(old)?;
    let new_oid = ctx
        .db
        .catalog
        .get_new_relfilenumber(ctx.cluster.oid_allocator())?;
    let new_loc = RelFileLocator {
        spc_oid: old.locator.spc_oid,
        db_oid: ctx.db.oid,
        rel_number: RelFileNumber(new_oid),
    };
    ctx.create_file(w, new_loc)?;
    ctx.schedule_unlinks(&[old.locator])?;
    let new = SequenceHandle {
        locator: new_loc,
        params: *params,
        ..old.clone()
    };
    seq.init(w, &new)?;
    let mut lsn = seq.reset(&new, params, Some(restart.unwrap_or(cur.last_value)))?;
    if restart.is_none() && cur.is_called {
        lsn = seq.setval(&new, cur.last_value, true)?;
    }
    ctx.db.catalog.update_class_row(
        w,
        ctx.snapshot,
        def.oid,
        &ClassPatch {
            relfilenode: Some(new_oid),
            ..ClassPatch::default()
        },
    )?;
    Ok(lsn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::query::{BoundCreateTable, BoundDdl};
    use crate::catalog::IdentityKind;
    use crate::catalog::depend::DependRow;
    use crate::catalog::seq_params::SeqOptions;
    use crate::ddl::testkit::{Harness, col, create_table_ddl};
    use crate::storage::SeqState;
    use crate::types::SqlType;
    use std::sync::Arc;

    fn seq_ddl(name: &str, opts: &SeqOptions) -> BoundCreateSequence {
        let out = init_params(opts, false, InitMode::Create).unwrap();
        BoundCreateSequence {
            schema: "public".into(),
            namespace: 2200,
            name: name.into(),
            if_not_exists: false,
            params: out.params,
            initial: out.state,
            owner: SeqOwner::None,
            for_identity: false,
        }
    }

    fn create(h: &mut Harness, c: BoundCreateSequence) -> Arc<TableDef> {
        let name = c.name.clone();
        assert_eq!(
            h.exec_w(BoundDdl::CreateSequence(c)).unwrap(),
            "CREATE SEQUENCE"
        );
        h.table(&name).unwrap()
    }

    fn state(h: &Harness, def: &TableDef) -> SeqState {
        h.tc.cluster
            .sequences()
            .read(&handle_from_def(def).unwrap())
            .unwrap()
    }

    /// シーケンス列を持つ表の `CREATE TABLE`。`kinds[i]` = Some(true) SERIAL / Some(false) IDENTITY。
    fn table_with_sequences(name: &str, kinds: &[bool]) -> BoundCreateTable {
        let mut cols = Vec::new();
        let mut b = create_table_ddl(name, Vec::new());
        for (i, &serial) in kinds.iter().enumerate() {
            let attnum = i16::try_from(i + 1).unwrap();
            let mut c = col(&format!("c{attnum}"), attnum, SqlType::INT4);
            c.not_null = true;
            if !serial {
                c.identity = Some(IdentityKind::Always);
            }
            cols.push(c);
            let mut s = seq_ddl(&format!("{name}_c{attnum}_seq"), &SeqOptions::default());
            s.owner = SeqOwner::NewTableColumn {
                attnum,
                serial_default: serial,
            };
            s.for_identity = !serial;
            b.sequences.push(s);
        }
        b.columns = cols;
        b
    }

    fn create_table(h: &mut Harness, b: BoundCreateTable) -> Arc<TableDef> {
        let name = b.name.clone();
        h.exec_w(BoundDdl::CreateTable(b)).unwrap();
        h.table(&name).unwrap()
    }

    fn snap(h: &Harness) -> crate::txn::Snapshot {
        h.tc.cluster.txn_manager().snapshot(h.txn.xid, h.txn.cid)
    }

    fn refs(h: &Harness, oid: Oid) -> Vec<DependRow> {
        h.db.catalog
            .references_of(&snap(h), ObjectAddress::relation(oid))
            .unwrap()
    }

    fn dependents(h: &Harness, oid: Oid) -> Vec<DependRow> {
        h.db.catalog
            .dependents_of(&snap(h), ObjectAddress::relation(oid))
            .unwrap()
    }

    fn drop_ddl(h: &Harness, names: &[&str], cascade: bool) -> BoundDdl {
        BoundDdl::DropSequence(BoundDropSequence {
            targets: names.iter().map(|n| h.table(n).unwrap()).collect(),
            missing: Vec::new(),
            cascade,
        })
    }

    fn alter_ddl(
        def: &Arc<TableDef>,
        options: SeqOptions,
        owned_by: Option<OwnedByTarget>,
    ) -> BoundDdl {
        BoundDdl::AlterSequence(BoundAlterSequence {
            target: Some(Arc::clone(def)),
            missing_name: def.name.clone(),
            action: BoundAlterAction::Options { options, owned_by },
        })
    }

    #[test]
    fn create_writes_the_file_wal_and_catalog_rows() {
        let mut h = Harness::new();
        let def = create(
            &mut h,
            seq_ddl(
                "sq1",
                &SeqOptions {
                    start: Some(10),
                    increment: Some(5),
                    max: Some(Some(100)),
                    cache: Some(3),
                    ..SeqOptions::default()
                },
            ),
        );
        assert_eq!(def.kind, RelKind::Sequence);
        assert_eq!(def.columns.len(), 3);
        let p = def.sequence.unwrap();
        assert_eq!(
            (p.start, p.increment, p.min, p.max, p.cache),
            (10, 5, 1, 100, 3)
        );
        assert_eq!(p.owned_by, None);
        assert_eq!(
            state(&h, &def),
            SeqState {
                last_value: 10,
                log_cnt: 0,
                is_called: false
            }
        );
        assert!(h.txn.pending_creates.contains(&def.locator));
        assert!(h.txn.wal_flush_upto > crate::wal::Lsn::default());
        assert!(h.txn.catalog_dirty);
        h.commit();
        assert!(h.check().is_empty(), "{:?}", h.check());
        assert!(h.file_exists(def.locator));
    }

    #[test]
    fn create_with_restart_sets_the_initial_state() {
        let mut h = Harness::new();
        let mut c = seq_ddl("sq2", &SeqOptions::default());
        c.initial = SeqState {
            last_value: 7,
            log_cnt: 0,
            is_called: false,
        };
        let def = create(&mut h, c);
        assert_eq!(state(&h, &def).last_value, 7);
        assert_eq!(def.sequence.unwrap().start, 1);
        h.commit();
    }

    #[test]
    fn create_rejects_a_duplicate_and_honors_if_not_exists() {
        let mut h = Harness::new();
        create(&mut h, seq_ddl("sq3", &SeqOptions::default()));
        h.commit();
        let e = h
            .exec_w(BoundDdl::CreateSequence(seq_ddl(
                "sq3",
                &SeqOptions::default(),
            )))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_TABLE);
        assert_eq!(e.message, "relation \"sq3\" already exists");
        h.rollback();
        h.begin();
        let mut c = seq_ddl("sq3", &SeqOptions::default());
        c.if_not_exists = true;
        assert_eq!(
            h.exec(BoundDdl::CreateSequence(c)).unwrap(),
            "CREATE SEQUENCE"
        );
        assert_eq!(
            h.notices[0].message,
            "relation \"sq3\" already exists, skipping"
        );
        assert_eq!(h.notices[0].sqlstate, sqlstate::DUPLICATE_TABLE);
        h.rollback();
    }

    #[test]
    fn rollback_removes_the_sequence() {
        let mut h = Harness::new();
        let def = create(&mut h, seq_ddl("sq4", &SeqOptions::default()));
        h.rollback();
        assert!(h.relation_kind("sq4").is_none());
        assert!(h.file_removed(def.locator));
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn create_table_with_serial_and_identity_columns() {
        let mut h = Harness::new();
        let t = create_table(&mut h, table_with_sequences("st1", &[true, false]));
        let serial = h.table("st1_c1_seq").unwrap();
        let ident = h.table("st1_c2_seq").unwrap();
        assert_eq!(
            t.columns[0].default.as_ref().unwrap().expr_sql,
            format!("nextval('{}'::regclass)", serial.oid)
        );
        assert!(t.columns[1].default.is_none());
        assert_eq!(t.columns[1].identity, Some(IdentityKind::Always));
        assert_eq!(t.identity_seqs, vec![(2, ident.oid)]);
        assert_eq!(serial.sequence.unwrap().owned_by, Some((t.oid, 1)));
        assert_eq!(ident.sequence.unwrap().owned_by, Some((t.oid, 2)));
        let deps = refs(&h, serial.oid);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].deptype, DependType::Auto);
        assert_eq!(deps[0].referenced, ObjectAddress::column(t.oid, 1));
        let deps = refs(&h, ident.oid);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].deptype, DependType::Internal);
        // pg_attrdef -> シーケンス（n）は 07 の機構が書く。
        let on_serial = dependents(&h, serial.oid);
        assert_eq!(on_serial.len(), 1);
        assert_eq!(on_serial[0].deptype, DependType::Normal);
        assert!(dependents(&h, ident.oid).is_empty());
        h.commit();
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn create_table_rollback_removes_sequence_files() {
        let mut h = Harness::new();
        create_table(&mut h, table_with_sequences("st2", &[true]));
        let seq = h.table("st2_c1_seq").unwrap();
        h.rollback();
        assert!(h.relation_kind("st2_c1_seq").is_none());
        assert!(h.file_removed(seq.locator));
    }

    #[test]
    fn alter_changes_params_and_resets_the_state() {
        let mut h = Harness::new();
        let def = create(
            &mut h,
            seq_ddl(
                "sq5",
                &SeqOptions {
                    start: Some(10),
                    increment: Some(5),
                    min: Some(Some(5)),
                    max: Some(Some(100)),
                    cache: Some(3),
                    ..SeqOptions::default()
                },
            ),
        );
        h.commit();
        let handle = handle_from_def(&def).unwrap();
        let seq = Arc::clone(h.tc.cluster.sequences());
        seq.fetch(&handle, 1).unwrap();
        seq.fetch(&handle, 1).unwrap();
        let before = seq.read(&handle).unwrap();
        assert!(before.is_called && before.log_cnt > 0);
        let generation = seq.reset_generation();

        h.begin();
        let r = h
            .exec(alter_ddl(
                &def,
                SeqOptions {
                    increment: Some(7),
                    ..SeqOptions::default()
                },
                None,
            ))
            .unwrap();
        assert_eq!(r, "ALTER SEQUENCE");
        let now = h.table("sq5").unwrap();
        assert_eq!(now.sequence.unwrap().increment, 7);
        assert_eq!(
            state(&h, &now),
            SeqState {
                last_value: before.last_value,
                log_cnt: 0,
                is_called: true
            }
        );
        assert!(seq.reset_generation() > generation);
        // RESTART は (start, 0, false)。
        h.exec(alter_ddl(
            &now,
            SeqOptions {
                restart: Some(None),
                ..SeqOptions::default()
            },
            None,
        ))
        .unwrap();
        let now = h.table("sq5").unwrap();
        assert_eq!(
            state(&h, &now),
            SeqState {
                last_value: 10,
                log_cnt: 0,
                is_called: false
            }
        );
        h.exec(alter_ddl(
            &now,
            SeqOptions {
                restart: Some(Some(50)),
                ..SeqOptions::default()
            },
            None,
        ))
        .unwrap();
        let now = h.table("sq5").unwrap();
        assert_eq!(state(&h, &now).last_value, 50);
        // 現在の値（50）が新しい MAXVALUE を超えるとき。
        let e = h
            .exec(alter_ddl(
                &now,
                SeqOptions {
                    max: Some(Some(40)),
                    ..SeqOptions::default()
                },
                None,
            ))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        assert_eq!(
            e.message,
            "RESTART value (50) cannot be greater than MAXVALUE (40)"
        );
        h.rollback();
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn alter_sequence_is_undone_by_rollback_and_kept_by_commit() {
        let mut h = Harness::new();
        let def = create(
            &mut h,
            seq_ddl(
                "sq5b",
                &SeqOptions {
                    cache: Some(1),
                    ..SeqOptions::default()
                },
            ),
        );
        h.commit();
        let handle = handle_from_def(&def).unwrap();
        let seq = Arc::clone(h.tc.cluster.sequences());
        for _ in 0..3 {
            seq.fetch(&handle, 1).unwrap();
        }
        let before = state(&h, &def);
        let restart = |v| SeqOptions {
            restart: Some(Some(v)),
            ..SeqOptions::default()
        };

        h.begin();
        h.exec(alter_ddl(&def, restart(500), None)).unwrap();
        let moved = h.table("sq5b").unwrap();
        assert_ne!(moved.locator, def.locator);
        assert_eq!(state(&h, &moved).last_value, 500);
        h.rollback();
        let back = h.table("sq5b").unwrap();
        assert_eq!(back.locator, def.locator);
        assert_eq!(state(&h, &back), before);
        assert!(h.file_removed(moved.locator));

        h.begin();
        h.exec(alter_ddl(&def, restart(500), None)).unwrap();
        h.commit();
        let now = h.table("sq5b").unwrap();
        assert_ne!(now.locator, def.locator);
        assert_eq!(state(&h, &now).last_value, 500);
        assert!(h.file_removed(def.locator));
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn alter_if_exists_and_owner_are_noops() {
        let mut h = Harness::new();
        h.begin();
        let r = h
            .exec(BoundDdl::AlterSequence(BoundAlterSequence {
                target: None,
                missing_name: "nope".into(),
                action: BoundAlterAction::OwnerNoop,
            }))
            .unwrap();
        assert_eq!(r, "ALTER SEQUENCE");
        assert_eq!(
            h.notices[0].message,
            "relation \"nope\" does not exist, skipping"
        );
        let def = create(&mut h, seq_ddl("sq6", &SeqOptions::default()));
        h.notices.clear();
        h.exec(BoundDdl::AlterSequence(BoundAlterSequence {
            target: Some(def),
            missing_name: "sq6".into(),
            action: BoundAlterAction::OwnerNoop,
        }))
        .unwrap();
        assert!(h.notices.is_empty());
        h.rollback();
    }

    #[test]
    fn alter_owned_by_moves_and_detaches_the_ownership() {
        let mut h = Harness::new();
        let t = create_table(
            &mut h,
            create_table_ddl(
                "ot",
                vec![col("a", 1, SqlType::INT4), col("b", 2, SqlType::INT4)],
            ),
        );
        let def = create(&mut h, seq_ddl("sq7", &SeqOptions::default()));
        let owned = |h: &Harness| refs(h, def.oid);
        assert!(owned(&h).is_empty());
        h.exec(alter_ddl(
            &def,
            SeqOptions::default(),
            Some(OwnedByTarget::Column {
                table: t.oid,
                attnum: 1,
            }),
        ))
        .unwrap();
        let d = owned(&h);
        assert_eq!(d.len(), 1);
        assert_eq!(
            (d[0].deptype, d[0].referenced),
            (DependType::Auto, ObjectAddress::column(t.oid, 1))
        );
        // 別の列へ付け替える（古い行は消える）。
        h.exec(alter_ddl(
            &def,
            SeqOptions::default(),
            Some(OwnedByTarget::Column {
                table: t.oid,
                attnum: 2,
            }),
        ))
        .unwrap();
        let d = owned(&h);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].referenced, ObjectAddress::column(t.oid, 2));
        h.exec(alter_ddl(
            &def,
            SeqOptions::default(),
            Some(OwnedByTarget::None),
        ))
        .unwrap();
        assert!(owned(&h).is_empty());
        h.commit();
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn alter_cannot_change_the_owner_of_an_identity_sequence() {
        let mut h = Harness::new();
        let t = create_table(&mut h, table_with_sequences("idt", &[false]));
        let seq = h.table("idt_c1_seq").unwrap();
        for target in [
            OwnedByTarget::None,
            OwnedByTarget::Column {
                table: t.oid,
                attnum: 1,
            },
        ] {
            let e = h
                .exec(alter_ddl(&seq, SeqOptions::default(), Some(target)))
                .unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
            assert_eq!(e.message, "cannot change ownership of identity sequence");
            assert_eq!(
                e.detail.as_deref(),
                Some("Sequence \"idt_c1_seq\" is linked to table \"idt\".")
            );
        }
        h.rollback();
    }

    #[test]
    fn drop_removes_rows_and_schedules_the_unlink() {
        let mut h = Harness::new();
        let a = create(&mut h, seq_ddl("da", &SeqOptions::default()));
        let b = create(&mut h, seq_ddl("db", &SeqOptions::default()));
        h.commit();
        h.begin();
        let r = h.exec(drop_ddl(&h, &["da", "db"], false)).unwrap();
        assert_eq!(r, "DROP SEQUENCE");
        assert!(h.txn.pending_unlinks.contains(&a.locator));
        assert!(h.txn.pending_unlinks.contains(&b.locator));
        assert!(h.notices.is_empty());
        h.commit();
        assert!(h.relation_kind("da").is_none() && h.relation_kind("db").is_none());
        assert_eq!(h.rows_mentioning(a.oid) + h.rows_mentioning(b.oid), 0);
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn drop_reports_missing_names() {
        let mut h = Harness::new();
        h.begin();
        let r = h
            .exec(BoundDdl::DropSequence(BoundDropSequence {
                targets: Vec::new(),
                missing: vec!["nope".into()],
                cascade: false,
            }))
            .unwrap();
        assert_eq!(r, "DROP SEQUENCE");
        assert_eq!(
            h.notices[0].message,
            "sequence \"nope\" does not exist, skipping"
        );
        h.rollback();
    }

    #[test]
    fn drop_restrict_fails_while_a_default_uses_the_sequence() {
        let mut h = Harness::new();
        create_table(&mut h, table_with_sequences("dt", &[true]));
        h.commit();
        h.begin();
        let e = h.exec(drop_ddl(&h, &["dt_c1_seq"], false)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(
            e.message,
            "cannot drop sequence dt_c1_seq because other objects depend on it"
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("default value for column c1 of table dt depends on sequence dt_c1_seq")
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("Use DROP ... CASCADE to drop the dependent objects too.")
        );
        h.rollback();
    }

    #[test]
    fn drop_cascade_removes_the_default_with_a_notice() {
        let mut h = Harness::new();
        let t = create_table(&mut h, table_with_sequences("dc", &[true]));
        h.commit();
        h.begin();
        h.exec(drop_ddl(&h, &["dc_c1_seq"], true)).unwrap();
        assert_eq!(
            h.notices[0].message,
            "drop cascades to default value for column c1 of table dc"
        );
        h.commit();
        let now = h.table("dc").unwrap();
        assert!(now.columns[0].default.is_none());
        assert_eq!(now.oid, t.oid);
        assert!(h.relation_kind("dc_c1_seq").is_none());
        assert!(h.check().is_empty(), "{:?}", h.check());
    }

    #[test]
    fn drop_of_an_identity_sequence_is_refused_even_with_cascade() {
        let mut h = Harness::new();
        create_table(&mut h, table_with_sequences("di", &[false]));
        h.commit();
        for cascade in [false, true] {
            h.begin();
            let e = h.exec(drop_ddl(&h, &["di_c1_seq"], cascade)).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::DEPENDENT_OBJECTS_STILL_EXIST);
            assert_eq!(
                e.message,
                "cannot drop sequence di_c1_seq because column c1 of table di requires it"
            );
            assert_eq!(
                e.hint.as_deref(),
                Some("You can drop column c1 of table di instead.")
            );
            h.rollback();
        }
    }

    #[test]
    fn drop_table_takes_serial_and_identity_sequences_along() {
        let mut h = Harness::new();
        let t = create_table(&mut h, table_with_sequences("dtab", &[true, false]));
        let s1 = h.table("dtab_c1_seq").unwrap();
        let s2 = h.table("dtab_c2_seq").unwrap();
        h.commit();
        h.begin();
        h.run(|ctx| {
            drop_owned_by_table(ctx, t.oid)?;
            Ok(())
        })
        .unwrap();
        assert!(h.txn.pending_unlinks.contains(&s1.locator));
        assert!(h.txn.pending_unlinks.contains(&s2.locator));
        assert!(h.relation_kind("dtab_c1_seq").is_none());
        assert!(h.relation_kind("dtab_c2_seq").is_none());
        assert!(h.relation_kind("dtab").is_some());
        h.rollback();
        // 何も所有しない表では何もしない。
        h.begin();
        let plain = create_table(
            &mut h,
            create_table_ddl("plain", vec![col("a", 1, SqlType::INT4)]),
        );
        h.run(|ctx| drop_owned_by_table(ctx, plain.oid)).unwrap();
        assert!(h.txn.pending_unlinks.is_empty());
        h.rollback();
    }

    #[test]
    fn restart_owned_by_table_resets_serial_and_identity_sequences() {
        let mut h = Harness::new();
        let t = create_table(&mut h, table_with_sequences("rt", &[true, false]));
        h.commit();
        let seq = Arc::clone(h.tc.cluster.sequences());
        let defs = [h.table("rt_c1_seq").unwrap(), h.table("rt_c2_seq").unwrap()];
        for d in &defs {
            let handle = handle_from_def(d).unwrap();
            seq.fetch(&handle, 1).unwrap();
            seq.fetch(&handle, 1).unwrap();
            assert!(seq.read(&handle).unwrap().is_called);
        }
        let called = seq.read(&handle_from_def(&defs[0]).unwrap()).unwrap();
        h.begin();
        h.run(|ctx| restart_owned_by_table(ctx, t.oid)).unwrap();
        let restarted = SeqState {
            last_value: 1,
            log_cnt: 0,
            is_called: false,
        };
        // 状態は新しい relfilenode に書かれる。旧いファイルはコミットまで残る。
        let new_defs = [h.table("rt_c1_seq").unwrap(), h.table("rt_c2_seq").unwrap()];
        for (d, n) in defs.iter().zip(&new_defs) {
            assert_ne!(d.locator, n.locator);
            assert_eq!(seq.read(&handle_from_def(n).unwrap()).unwrap(), restarted);
            assert!(h.file_exists(d.locator));
        }
        assert!(h.txn.wal_flush_upto > crate::wal::Lsn::default());
        // ROLLBACK で戻る（PostgreSQL と同じ。表のデータが残るのに id だけ巻き戻らない）。
        h.rollback();
        for (d, n) in defs.iter().zip(&new_defs) {
            assert!(h.file_removed(n.locator));
            let now = h.table(&d.name).unwrap();
            assert_eq!(now.locator, d.locator);
            assert_eq!(seq.read(&handle_from_def(&now).unwrap()).unwrap(), called);
        }
        h.begin();
        h.run(|ctx| restart_owned_by_table(ctx, t.oid)).unwrap();
        h.commit();
        for d in &defs {
            let now = h.table(&d.name).unwrap();
            assert_ne!(now.locator, d.locator);
            assert_eq!(
                seq.read(&handle_from_def(&now).unwrap()).unwrap(),
                restarted
            );
            assert!(h.file_removed(d.locator));
        }
        assert!(h.check().is_empty(), "{:?}", h.check());
        // シーケンスを持たない表は何もしない。
        h.begin();
        let plain = create_table(
            &mut h,
            create_table_ddl("plain2", vec![col("a", 1, SqlType::INT4)]),
        );
        h.run(|ctx| restart_owned_by_table(ctx, plain.oid)).unwrap();
        h.rollback();
    }

    #[test]
    fn create_sequence_owned_by_an_existing_column() {
        let mut h = Harness::new();
        let t = create_table(
            &mut h,
            create_table_ddl("ob", vec![col("a", 1, SqlType::INT4)]),
        );
        h.commit();
        let mut c = seq_ddl("sq8", &SeqOptions::default());
        c.owner = SeqOwner::Column {
            table: t.oid,
            attnum: 1,
        };
        let def = create(&mut h, c);
        assert_eq!(def.sequence.unwrap().owned_by, Some((t.oid, 1)));
        let d = refs(&h, def.oid);
        assert_eq!(
            (d[0].deptype, d[0].referenced),
            (DependType::Auto, ObjectAddress::column(t.oid, 1))
        );
        h.commit();
        assert!(h.check().is_empty(), "{:?}", h.check());
    }
}
