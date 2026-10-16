//! `VACUUM` / `ANALYZE` の実行の枠（03 §6.7。VC-3a）。
//!
//! - [`execute_vacuum`]: `ddl::execute_standalone` から。テーブルごとに `commit_and_restart` し、最後に 1 回
//!   データベースの凍結境界を進める。ブロックの中では `25001`。
//! - [`execute_analyze`]: `ANALYZE`（VACUUM を伴わないもの）。ユーザーのトランザクションの中で動く。
//! - [`vacuum`]: `ddl::execute`（トランザクションの中）から呼ばれる M4 からの口。`ANALYZE` 文は
//!   [`execute_analyze`] と同じに動く。`VACUUM` はオプションを検査するだけで何もしない
//!   （session が `VACUUM` を [`execute_vacuum`] へ回すまでの間）。
//!
//! 本体（ページの走査・インデックスの掃除・統計）は `crate::vacuum` の `vacuum_relation` /
//! `analyze_relation`（VC-3b）。ここはオプションの検査、対象の列挙、トランザクションの区切りだけを持つ。

use super::{DdlCtx, TxnControl};
use crate::analyzer::ddl_ext::vacuum::{BoundAnalyze, VacuumOptions, resolve_options};
use crate::analyzer::query::{BoundVacuum, VacuumTarget};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::txn::lock::LockScope;
use crate::types::Oid;
use crate::vacuum::{
    ClogTruncateOutcome, VacuumEnv, VacuumRelOutcome, analyze_relation,
    update_datfrozenxid_and_truncate_clog, vacuum_relation,
};

/// 本体の関数の差し替え口（試験で呼び出しの順序を記録するため）。実運用は [`RealSteps`]。
trait VacuumSteps {
    fn vacuum_relation(
        &mut self,
        env: &mut VacuumEnv<'_>,
        relid: Oid,
        opts: &VacuumOptions,
    ) -> Result<VacuumRelOutcome>;
    fn analyze_relation(
        &mut self,
        env: &mut VacuumEnv<'_>,
        relid: Oid,
        columns: &[i16],
        opts: &VacuumOptions,
    ) -> Result<()>;
    fn finish_database(&mut self, env: &mut VacuumEnv<'_>) -> Result<ClogTruncateOutcome>;
}

struct RealSteps;

impl VacuumSteps for RealSteps {
    fn vacuum_relation(
        &mut self,
        env: &mut VacuumEnv<'_>,
        relid: Oid,
        opts: &VacuumOptions,
    ) -> Result<VacuumRelOutcome> {
        vacuum_relation(env, relid, opts)
    }

    fn analyze_relation(
        &mut self,
        env: &mut VacuumEnv<'_>,
        relid: Oid,
        columns: &[i16],
        opts: &VacuumOptions,
    ) -> Result<()> {
        analyze_relation(env, relid, columns, opts.verbose, opts.skip_locked)
    }

    fn finish_database(&mut self, env: &mut VacuumEnv<'_>) -> Result<ClogTruncateOutcome> {
        update_datfrozenxid_and_truncate_clog(env)
    }
}

/// 実行する 1 テーブル。
#[derive(Debug, PartialEq, Eq)]
struct Item {
    oid: Oid,
    /// ANALYZE の列リスト（attnum）。空は全列。
    columns: Vec<i16>,
}

fn env_of<'a>(ctx: &'a mut DdlCtx<'_>, own_xid: bool) -> VacuumEnv<'a> {
    VacuumEnv {
        cluster: ctx.cluster,
        db: ctx.db,
        backend: ctx.backend,
        wait: ctx.wait,
        interrupts: ctx.interrupts,
        notices: &mut *ctx.notices,
        lock_scope: LockScope::Transaction,
        own_xid: if own_xid { ctx.txn.xid } else { None },
        is_autovacuum: false,
    }
}

/// 対象を列挙する。表でないもの（索引・シーケンス）は WARNING を出して飛ばす。
/// 空なら現在のデータベースの全テーブル（OID の昇順）。
fn collect_items(
    ctx: &mut DdlCtx<'_>,
    targets: &[VacuumTarget],
    vacuum: bool,
) -> Result<Vec<Item>> {
    if targets.is_empty() {
        return Ok(ctx
            .db
            .catalog
            .list_vacuum_relations(ctx.snapshot)?
            .into_iter()
            .map(|r| Item {
                oid: r.oid,
                columns: Vec::new(),
            })
            .collect());
    }
    let verb = if vacuum { "vacuum" } else { "analyze" };
    let mut items = Vec::with_capacity(targets.len());
    for t in targets {
        let Some(table) = &t.table else {
            ctx.notice(
                Severity::Warning,
                sqlstate::WARNING,
                format!(
                    "skipping \"{}\" --- cannot {verb} non-tables or special system tables",
                    t.name
                ),
                None,
            );
            continue;
        };
        let columns = t
            .columns
            .iter()
            .map(|c| {
                table.column(c).map(|d| d.attnum).ok_or_else(|| {
                    Error::new(
                        sqlstate::UNDEFINED_COLUMN,
                        format!(
                            "column \"{c}\" of relation \"{}\" does not exist",
                            table.name
                        ),
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        items.push(Item {
            oid: table.oid,
            columns,
        });
    }
    Ok(items)
}

fn refuse_in_block(ctx: &DdlCtx<'_>) -> Result<()> {
    if ctx.in_transaction_block || !ctx.outside_block {
        return Err(Error::new(
            sqlstate::ACTIVE_SQL_TRANSACTION,
            "VACUUM cannot run inside a transaction block",
        ));
    }
    Ok(())
}

/// `VACUUM`（`ANALYZE` 付きを含む）。テーブルごとにコミットするので、途中のエラー・キャンセルでも
/// 終わったテーブルの結果は残る（PG と同じ）。コマンドタグは `VACUUM`。
#[allow(clippy::needless_pass_by_value)]
pub fn execute_vacuum(ctl: &mut dyn TxnControl, v: BoundVacuum) -> Result<String> {
    run_vacuum(ctl, &v, &mut RealSteps)
}

fn run_vacuum(
    ctl: &mut dyn TxnControl,
    v: &BoundVacuum,
    steps: &mut dyn VacuumSteps,
) -> Result<String> {
    if !v.vacuum {
        // ANALYZE 文は暗黙のトランザクションの中で動く（区切らない）。
        let mut ctx = ctl.ddl_ctx()?;
        return analyze_in(&mut ctx, v, steps);
    }
    // 1. session が先に検査するが、ここでも確かめる。
    let items;
    let opts;
    {
        let mut ctx = ctl.ddl_ctx()?;
        refuse_in_block(&ctx)?;
        // 2.
        opts = resolve_options(true, &v.options, &v.targets)?;
        // 3.
        items = if opts.only_database_stats {
            Vec::new()
        } else {
            collect_items(&mut ctx, &v.targets, true)?
        };
    }
    // 4. テーブルごとに: VACUUM → コミット →（ANALYZE → コミット）。
    for item in &items {
        let outcome = {
            let mut ctx = ctl.ddl_ctx()?;
            ctx.interrupts.check()?;
            let outcome = {
                let mut env = env_of(&mut ctx, false);
                steps.vacuum_relation(&mut env, item.oid, &opts)?
            };
            if let VacuumRelOutcome::Done(s) = &outcome {
                ctx.txn.note_wal(s.last_lsn);
            }
            outcome
        };
        ctl.commit_and_restart()?;
        if opts.analyze && matches!(outcome, VacuumRelOutcome::Done(_)) {
            {
                let mut ctx = ctl.ddl_ctx()?;
                let mut env = env_of(&mut ctx, false);
                steps.analyze_relation(&mut env, item.oid, &item.columns, &opts)?;
            }
            ctl.commit_and_restart()?;
        }
    }
    // 5.
    if !opts.skip_database_stats {
        let mut ctx = ctl.ddl_ctx()?;
        let mut env = env_of(&mut ctx, false);
        steps.finish_database(&mut env)?;
    }
    Ok("VACUUM".into())
}

/// `ANALYZE`（`VACUUM` を伴わない）。ユーザーのトランザクションの中（ブロックの中も可）。
/// コマンドタグは `ANALYZE`。
#[allow(clippy::needless_pass_by_value)]
pub fn execute_analyze(ctx: &mut DdlCtx<'_>, a: BoundAnalyze) -> Result<String> {
    let mut opts = resolve_options(false, &[], &a.targets)?;
    opts.verbose = a.verbose;
    opts.skip_locked = a.skip_locked;
    analyze_targets(ctx, &opts, &a.targets, &mut RealSteps)
}

fn analyze_in(
    ctx: &mut DdlCtx<'_>,
    b: &BoundVacuum,
    steps: &mut dyn VacuumSteps,
) -> Result<String> {
    let opts = resolve_options(false, &b.options, &b.targets)?;
    analyze_targets(ctx, &opts, &b.targets, steps)
}

fn analyze_targets(
    ctx: &mut DdlCtx<'_>,
    opts: &VacuumOptions,
    targets: &[VacuumTarget],
    steps: &mut dyn VacuumSteps,
) -> Result<String> {
    let items = collect_items(ctx, targets, false)?;
    for item in &items {
        ctx.interrupts.check()?;
        let mut env = env_of(ctx, true);
        steps.analyze_relation(&mut env, item.oid, &item.columns, opts)?;
    }
    Ok("ANALYZE".into())
}

/// `ddl::execute`（トランザクションの中）からの口。
///
/// - `ANALYZE` 文: [`execute_analyze`] と同じ。
/// - `VACUUM`: 25001 の検査とオプションの検査だけで、何もしない。実際の VACUUM は
///   `ddl::execute_standalone` が [`execute_vacuum`] を呼ぶ経路で動く（session がそちらへ回すまでの口）。
pub fn vacuum(ctx: &mut DdlCtx<'_>, b: &BoundVacuum) -> Result<String> {
    if !b.vacuum {
        return analyze_in(ctx, b, &mut RealSteps);
    }
    refuse_in_block(ctx)?;
    resolve_options(true, &b.options, &b.targets)?;
    collect_items(ctx, &b.targets, true)?;
    Ok("VACUUM".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::query::{BoundDdl, VacuumOption};
    use crate::catalog::fake::table_def;
    use crate::catalog::reader::StatementCatalog;
    use crate::ddl::testkit::{Harness, col};
    use crate::session::Notice;
    use crate::txn::Xid;
    use crate::types::{SqlType, TypeEnv};
    use crate::vacuum::VacuumRelStats;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    type Log = Rc<RefCell<Vec<String>>>;

    /// 呼び出しの順序を記録する `TxnControl`（`commit_and_restart` も同じログに書く）。
    struct FakeCtl {
        h: Harness,
        log: Log,
    }

    impl TxnControl for FakeCtl {
        fn ddl_ctx(&mut self) -> Result<DdlCtx<'_>> {
            let h = &mut self.h;
            let snap = Box::leak(Box::new(
                h.tc.cluster.txn_manager().snapshot(h.txn.xid, h.txn.cid),
            ));
            let search_path = Box::leak(Box::new(vec!["public".to_owned()]));
            let catalog = Box::leak(Box::new(StatementCatalog {
                db: &h.db,
                snapshot: snap,
                gen_at_snapshot: h.db.cache.generation(),
                bypass_cache: true,
                search_path,
            }));
            let wait = Box::leak(Box::new(crate::txn::lock::WaitCtl {
                lock_timeout: None,
                deadlock_timeout: std::time::Duration::from_secs(1),
                interrupts: &h.intr,
            }));
            let env = Box::leak(Box::new(TypeEnv::default()));
            Ok(DdlCtx {
                cluster: &h.tc.cluster,
                db: &h.db,
                snapshot: snap,
                catalog,
                txn: &mut h.txn,
                role_oid: h.role,
                notices: &mut h.notices,
                type_env: env,
                in_transaction_block: h.in_block,
                interrupts: &h.intr,
                backend: crate::backend::BackendId(0),
                wait,
                session: &h.info,
                role: &h.role_row,
                password: crate::ddl::role::PasswordPolicy::default(),
                outside_block: !h.in_block,
            })
        }

        fn commit_and_restart(&mut self) -> Result<()> {
            self.log.borrow_mut().push("commit".into());
            self.h.txn = crate::txn::Transaction::new();
            Ok(())
        }
    }

    #[derive(Default)]
    struct Rec {
        log: Log,
        fail_on: Option<Oid>,
        own_xids: Vec<Option<Xid>>,
        options: Vec<VacuumOptions>,
    }

    impl VacuumSteps for Rec {
        fn vacuum_relation(
            &mut self,
            env: &mut VacuumEnv<'_>,
            relid: Oid,
            opts: &VacuumOptions,
        ) -> Result<VacuumRelOutcome> {
            self.log.borrow_mut().push(format!("vacuum {relid}"));
            self.own_xids.push(env.own_xid);
            self.options.push(opts.clone());
            if self.fail_on == Some(relid) {
                return Err(Error::new(sqlstate::INTERNAL_ERROR, "boom"));
            }
            Ok(VacuumRelOutcome::Done(VacuumRelStats::untouched()))
        }

        fn analyze_relation(
            &mut self,
            env: &mut VacuumEnv<'_>,
            relid: Oid,
            columns: &[i16],
            opts: &VacuumOptions,
        ) -> Result<()> {
            self.log.borrow_mut().push(format!(
                "analyze {relid} {columns:?} verbose={} skip_locked={}",
                opts.verbose, opts.skip_locked
            ));
            self.own_xids.push(env.own_xid);
            Ok(())
        }

        fn finish_database(&mut self, _env: &mut VacuumEnv<'_>) -> Result<ClogTruncateOutcome> {
            self.log.borrow_mut().push("finish".into());
            Ok(ClogTruncateOutcome::default())
        }
    }

    fn setup() -> (FakeCtl, Rec) {
        let log = Log::default();
        (
            FakeCtl {
                h: Harness::new(),
                log: Rc::clone(&log),
            },
            Rec {
                log,
                ..Rec::default()
            },
        )
    }

    fn opt(name: &str, value: Option<&str>) -> VacuumOption {
        VacuumOption {
            name: name.into(),
            value: value.map(str::to_owned),
        }
    }

    fn table(oid: Oid, name: &str) -> Arc<crate::catalog::TableDef> {
        Arc::new(table_def(
            oid,
            name,
            vec![col("a", 1, SqlType::INT4), col("b", 2, SqlType::INT4)],
            vec![],
        ))
    }

    fn target(t: &Arc<crate::catalog::TableDef>, columns: &[&str]) -> VacuumTarget {
        VacuumTarget {
            name: t.name.clone(),
            table: Some(Arc::clone(t)),
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    fn non_table(name: &str) -> VacuumTarget {
        VacuumTarget {
            name: name.into(),
            table: None,
            columns: Vec::new(),
        }
    }

    fn bound(vacuum: bool, options: Vec<VacuumOption>, targets: Vec<VacuumTarget>) -> BoundVacuum {
        BoundVacuum {
            vacuum,
            analyze: !vacuum,
            options,
            targets,
        }
    }

    fn events(ctl: &FakeCtl) -> Vec<String> {
        ctl.log.borrow().clone()
    }

    #[test]
    fn vacuum_commits_after_each_table_and_finishes_the_database_once() {
        let (mut ctl, mut rec) = setup();
        let (t1, t2) = (table(16400, "t1"), table(16401, "t2"));
        let v = bound(true, vec![], vec![target(&t1, &[]), target(&t2, &[])]);
        assert_eq!(run_vacuum(&mut ctl, &v, &mut rec).unwrap(), "VACUUM");
        assert_eq!(
            events(&ctl),
            ["vacuum 16400", "commit", "vacuum 16401", "commit", "finish"]
        );
        assert_eq!(rec.own_xids, [None, None], "VACUUM は XID を持たない");
        assert!(!rec.options[0].analyze && rec.options[0].truncate);
    }

    #[test]
    fn no_targets_means_every_table_in_oid_order() {
        let (mut ctl, mut rec) = setup();
        let all = ctl
            .h
            .db
            .catalog
            .list_vacuum_relations(&ctl.h.tc.cluster.txn_manager().snapshot(None, 0));
        let all = all.unwrap();
        assert!(all.len() > 5);
        run_vacuum(&mut ctl, &bound(true, vec![], vec![]), &mut rec).unwrap();
        let log = events(&ctl);
        let vacuumed: Vec<&String> = log.iter().filter(|e| e.starts_with("vacuum ")).collect();
        let want: Vec<String> = all.iter().map(|r| format!("vacuum {}", r.oid)).collect();
        assert_eq!(vacuumed, want.iter().collect::<Vec<_>>());
        assert_eq!(log.iter().filter(|e| *e == "commit").count(), all.len());
        assert_eq!(log.last().unwrap(), "finish");
    }

    #[test]
    fn analyze_option_runs_after_the_vacuum_of_the_same_table() {
        let (mut ctl, mut rec) = setup();
        let t1 = table(16400, "t1");
        let v = bound(
            true,
            vec![opt("analyze", None), opt("verbose", None)],
            vec![target(&t1, &["b", "a"])],
        );
        run_vacuum(&mut ctl, &v, &mut rec).unwrap();
        assert_eq!(
            events(&ctl),
            [
                "vacuum 16400",
                "commit",
                "analyze 16400 [2, 1] verbose=true skip_locked=false",
                "commit",
                "finish"
            ]
        );
        assert!(rec.options[0].analyze);
    }

    #[test]
    fn database_stats_options() {
        let (mut ctl, mut rec) = setup();
        let t1 = table(16400, "t1");
        let v = bound(
            true,
            vec![opt("skip_database_stats", None)],
            vec![target(&t1, &[])],
        );
        run_vacuum(&mut ctl, &v, &mut rec).unwrap();
        assert_eq!(events(&ctl), ["vacuum 16400", "commit"]);

        let (mut ctl, mut rec) = setup();
        let v = bound(true, vec![opt("only_database_stats", None)], vec![]);
        run_vacuum(&mut ctl, &v, &mut rec).unwrap();
        assert_eq!(events(&ctl), ["finish"]);
    }

    #[test]
    fn an_error_keeps_the_finished_tables_and_stops() {
        let (mut ctl, mut rec) = setup();
        rec.fail_on = Some(16401);
        let (t1, t2, t3) = (table(16400, "t1"), table(16401, "t2"), table(16402, "t3"));
        let v = bound(
            true,
            vec![],
            vec![target(&t1, &[]), target(&t2, &[]), target(&t3, &[])],
        );
        let e = run_vacuum(&mut ctl, &v, &mut rec).unwrap_err();
        assert_eq!(e.message, "boom");
        assert_eq!(events(&ctl), ["vacuum 16400", "commit", "vacuum 16401"]);
    }

    #[test]
    fn cancel_is_checked_between_tables() {
        let (mut ctl, mut rec) = setup();
        ctl.h.intr.request_cancel();
        let t1 = table(16400, "t1");
        let e = run_vacuum(
            &mut ctl,
            &bound(true, vec![], vec![target(&t1, &[])]),
            &mut rec,
        )
        .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::QUERY_CANCELED);
        assert!(events(&ctl).is_empty());
    }

    #[test]
    fn vacuum_is_refused_in_a_block_before_anything_runs() {
        let (mut ctl, mut rec) = setup();
        ctl.h.in_block = true;
        // 不正なオプションより 25001 が先。
        let v = bound(true, vec![opt("foo", None)], vec![]);
        let e = run_vacuum(&mut ctl, &v, &mut rec).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::ACTIVE_SQL_TRANSACTION);
        assert_eq!(e.message, "VACUUM cannot run inside a transaction block");
        assert!(events(&ctl).is_empty());
    }

    #[test]
    fn option_errors_happen_before_any_work() {
        let (mut ctl, mut rec) = setup();
        let t1 = table(16400, "t1");
        for (options, state, message) in [
            (
                vec![opt("full", None)],
                sqlstate::FEATURE_NOT_SUPPORTED,
                "VACUUM FULL is not supported yet",
            ),
            (
                vec![opt("foo", None)],
                sqlstate::SYNTAX_ERROR,
                "unrecognized VACUUM option \"foo\"",
            ),
        ] {
            let v = bound(true, options, vec![target(&t1, &[])]);
            let e = run_vacuum(&mut ctl, &v, &mut rec).unwrap_err();
            assert_eq!((e.sqlstate, e.message.as_str()), (state, message));
        }
        // 列リストがあるのに ANALYZE がない。
        let v = bound(true, vec![], vec![target(&t1, &["a"])]);
        let e = run_vacuum(&mut ctl, &v, &mut rec).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert!(events(&ctl).is_empty());
    }

    #[test]
    fn non_tables_are_skipped_with_a_warning() {
        let (mut ctl, mut rec) = setup();
        let t1 = table(16400, "t1");
        let v = bound(true, vec![], vec![non_table("t1_pkey"), target(&t1, &[])]);
        run_vacuum(&mut ctl, &v, &mut rec).unwrap();
        assert_eq!(
            events(&ctl),
            ["vacuum 16400", "commit", "finish"],
            "索引は飛ばす"
        );
        let notices: &Vec<Notice> = &ctl.h.notices;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].severity, Severity::Warning);
        assert_eq!(notices[0].sqlstate, sqlstate::WARNING);
        assert_eq!(
            notices[0].message,
            "skipping \"t1_pkey\" --- cannot vacuum non-tables or special system tables"
        );
    }

    #[test]
    fn analyze_runs_inside_a_block_without_committing() {
        let (mut ctl, mut rec) = setup();
        ctl.h.in_block = true;
        ctl.h.begin();
        let xid = ctl.h.txn.xid;
        let (t1, t2) = (table(16400, "t1"), table(16401, "t2"));
        let v = bound(
            false,
            vec![opt("verbose", None)],
            vec![target(&t1, &["a"]), target(&t2, &[])],
        );
        let mut ctx = ctl.ddl_ctx().unwrap();
        assert_eq!(analyze_in(&mut ctx, &v, &mut rec).unwrap(), "ANALYZE");
        assert_eq!(
            events(&ctl),
            [
                "analyze 16400 [1] verbose=true skip_locked=false",
                "analyze 16401 [] verbose=true skip_locked=false"
            ]
        );
        assert_eq!(
            rec.own_xids,
            [xid, xid],
            "外側のトランザクションの XID を渡す"
        );
    }

    #[test]
    fn analyze_statement_through_run_vacuum_does_not_commit() {
        let (mut ctl, mut rec) = setup();
        let t1 = table(16400, "t1");
        let v = bound(
            false,
            vec![opt("skip_locked", None)],
            vec![target(&t1, &[])],
        );
        assert_eq!(run_vacuum(&mut ctl, &v, &mut rec).unwrap(), "ANALYZE");
        assert_eq!(
            events(&ctl),
            ["analyze 16400 [] verbose=false skip_locked=true"]
        );
    }

    #[test]
    fn analyze_rejects_vacuum_only_options_and_warns_about_non_tables() {
        let (mut ctl, mut rec) = setup();
        let mut ctx = ctl.ddl_ctx().unwrap();
        let v = bound(false, vec![opt("full", None)], vec![]);
        let e = analyze_in(&mut ctx, &v, &mut rec).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "unrecognized ANALYZE option \"full\"");
        let v = bound(false, vec![], vec![non_table("s1")]);
        analyze_in(&mut ctx, &v, &mut rec).unwrap();
        assert_eq!(
            ctx.notices[0].message,
            "skipping \"s1\" --- cannot analyze non-tables or special system tables"
        );
    }

    #[test]
    fn execute_analyze_takes_the_bound_analyze() {
        let (mut ctl, _) = setup();
        ctl.h.in_block = true;
        let t1 = table(16400, "t1");
        let mut ctx = ctl.ddl_ctx().unwrap();
        let a = BoundAnalyze {
            verbose: true,
            skip_locked: false,
            targets: vec![target(&t1, &["a"])],
        };
        assert_eq!(execute_analyze(&mut ctx, a).unwrap(), "ANALYZE");
    }

    // ----- ddl::execute 経由（M4 からの口）-----

    fn run(h: &mut Harness, b: BoundVacuum) -> Result<String> {
        h.exec(BoundDdl::Vacuum(b))
    }

    #[test]
    fn vacuum_through_execute_validates_and_takes_no_xid() {
        let mut h = Harness::new();
        assert_eq!(run(&mut h, bound(true, vec![], vec![])).unwrap(), "VACUUM");
        assert_eq!(
            run(&mut h, bound(false, vec![], vec![])).unwrap(),
            "ANALYZE"
        );
        assert!(h.txn.xid.is_none(), "no XID is taken");
        assert!(!h.txn.catalog_dirty);
        let e = run(&mut h, bound(true, vec![opt("full", None)], vec![])).unwrap_err();
        assert_eq!(e.message, "VACUUM FULL is not supported yet");
        h.in_block = true;
        let e = run(&mut h, bound(true, vec![], vec![])).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::ACTIVE_SQL_TRANSACTION);
        assert_eq!(
            run(&mut h, bound(false, vec![], vec![])).unwrap(),
            "ANALYZE"
        );
    }

    #[test]
    fn execute_warns_about_non_tables_and_checks_column_lists() {
        let mut h = Harness::new();
        let t1 = table(16400, "t1");
        let mut b = bound(true, vec![], vec![non_table("v1_a")]);
        assert_eq!(run(&mut h, b.clone()).unwrap(), "VACUUM");
        assert_eq!(
            h.notices[0].message,
            "skipping \"v1_a\" --- cannot vacuum non-tables or special system tables"
        );
        b.targets = vec![target(&t1, &["a"])];
        let e = run(&mut h, b.clone()).unwrap_err();
        assert_eq!(
            e.message,
            "ANALYZE option must be specified when a column list is provided"
        );
        b.options = vec![opt("analyze", None)];
        assert_eq!(run(&mut h, b).unwrap(), "VACUUM");
    }
}
