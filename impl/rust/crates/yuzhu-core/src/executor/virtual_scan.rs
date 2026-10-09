//! 仮想リレーションの走査（M5 LK-5。`spec/design/m5/01-lock-txn.md` §6.5）。
//!
//! 最初の `next` で提供関数（`catalog::virtual_rel::find(oid).rows(..)`）を 1 回呼んで行を溜める
//! （その文の中では一貫した行になる）。`rewind` は溜めた行を先頭から読み直す。`filter` は行ごとに
//! 評価する。`system_columns` は NULL で埋める。ラッチ・ピンは持たず、ロックもしない。

use std::rc::Rc;

use crate::catalog::SystemColumn;
use crate::catalog::virtual_rel::{self, VirtualCtx};
use crate::error::{Error, Result};
use crate::executor::eval::eval_pred;
use crate::executor::instrument::Instrumentation;
use crate::executor::{ExecCtx, Executor};
use crate::planner::physical::{FilterCounter, PhysExpr};
use crate::types::{Datum, Oid, Row};

#[derive(Debug)]
pub struct VirtualScanExec {
    rel_oid: Oid,
    system_columns: Vec<SystemColumn>,
    filter: Option<PhysExpr>,
    /// 最初の `next` で作る。
    rows: Option<Vec<Row>>,
    pos: usize,
    counters: Option<(usize, Rc<Instrumentation>)>,
}

impl VirtualScanExec {
    pub fn new(rel_oid: Oid, system_columns: Vec<SystemColumn>, filter: Option<PhysExpr>) -> Self {
        VirtualScanExec {
            rel_oid,
            system_columns,
            filter,
            rows: None,
            pos: 0,
            counters: None,
        }
    }

    fn load(&self, ctx: &ExecCtx<'_>) -> Result<Vec<Row>> {
        let rel = virtual_rel::find(self.rel_oid)
            .ok_or_else(|| Error::internal(format!("unknown virtual relation {}", self.rel_oid)))?;
        let mut rows = rel.rows(&VirtualCtx {
            runtime: ctx.runtime,
            session: ctx.session,
        })?;
        if !self.system_columns.is_empty() {
            for r in &mut rows {
                r.extend(self.system_columns.iter().map(|_| Datum::Null));
            }
        }
        Ok(rows)
    }
}

impl Executor for VirtualScanExec {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>> {
        if self.rows.is_none() {
            self.rows = Some(self.load(ctx)?);
            self.pos = 0;
        }
        loop {
            ctx.check_interrupts()?;
            let Some(row) = self.rows.as_ref().and_then(|r| r.get(self.pos)).cloned() else {
                return Ok(None);
            };
            self.pos += 1;
            let Some(filter) = &self.filter else {
                return Ok(Some(row));
            };
            if eval_pred(filter, &row, ctx)? == Some(true) {
                return Ok(Some(row));
            }
            if let Some((id, instr)) = &self.counters {
                instr.add_removed(*id, FilterCounter::Filter, 1);
            }
        }
    }

    /// 溜めた行を先頭から読み直す（行は作り直さない）。
    fn rewind(&mut self, _ctx: &mut ExecCtx<'_>) -> Result<()> {
        self.pos = 0;
        Ok(())
    }

    fn set_counters(&mut self, id: usize, instr: &Rc<Instrumentation>) {
        self.counters = Some((id, Rc::clone(instr)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendId;
    use crate::catalog::virtual_rel::PG_LOCKS_OID;
    use crate::executor::nodes::test_util::{Fixture, drain};
    use crate::executor::{ExecEnv, NullRuntime, RuntimeInfo};
    use crate::txn::lock::LockStatusRow;
    use crate::types::TypeEnv;

    /// ロック表の写しを 1 行だけ返す `RuntimeInfo`。`lock_status` の呼び出し回数を数える。
    #[derive(Debug, Default)]
    struct OneLock {
        calls: std::cell::Cell<usize>,
    }

    impl RuntimeInfo for OneLock {
        fn backend_pid(&self) -> i32 {
            1
        }
        fn is_blocked_by(&self, _: i32, _: &[i32]) -> bool {
            false
        }
        fn check_interrupts(&self) -> Result<()> {
            Ok(())
        }
        fn lock_status(&self) -> Vec<LockStatusRow> {
            self.calls.set(self.calls.get() + 1);
            vec![LockStatusRow {
                locktype: "relation",
                database: Some(5),
                relation: Some(16400),
                page: None,
                tuple: None,
                transactionid: None,
                backend: BackendId(1),
                pid: 77,
                mode: "AccessShareLock",
                granted: true,
                classid: None,
                objid: None,
                objsubid: None,
                waited: None,
            }]
        }
    }

    fn run_with(rt: &dyn RuntimeInfo, exec: &mut crate::executor::BoxedExecutor) -> Vec<Row> {
        let mut f = Fixture::new();
        let tenv = TypeEnv::default();
        let env = ExecEnv::without_indexes(
            &f.catalog,
            &f.storage,
            &f.snapshot,
            &f.session,
            rt,
            &f.interrupts,
            &tenv,
        );
        let mut ctx = ExecCtx::new(env, &mut f.txn, &f.query);
        drain(exec, &mut ctx).unwrap()
    }

    #[test]
    fn scans_the_rows_the_runtime_reports() {
        let rt = OneLock::default();
        let mut e: crate::executor::BoxedExecutor =
            Box::new(VirtualScanExec::new(PG_LOCKS_OID, vec![], None));
        let rows = run_with(&rt, &mut e);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 16);
        assert_eq!(rows[0][11], Datum::Int4(77));
        assert_eq!(rt.calls.get(), 1);
    }

    #[test]
    fn empty_lock_table_gives_no_rows() {
        let mut e: crate::executor::BoxedExecutor =
            Box::new(VirtualScanExec::new(PG_LOCKS_OID, vec![], None));
        assert!(run_with(&NullRuntime, &mut e).is_empty());
    }

    #[test]
    fn system_columns_are_null_and_rewind_rereads_the_same_rows() {
        let rt = OneLock::default();
        let mut f = Fixture::new();
        let tenv = TypeEnv::default();
        let env = ExecEnv::without_indexes(
            &f.catalog,
            &f.storage,
            &f.snapshot,
            &f.session,
            &rt,
            &f.interrupts,
            &tenv,
        );
        let mut ctx = ExecCtx::new(env, &mut f.txn, &f.query);
        let mut e: crate::executor::BoxedExecutor = Box::new(VirtualScanExec::new(
            PG_LOCKS_OID,
            vec![SystemColumn::Ctid],
            None,
        ));
        let first = drain(&mut e, &mut ctx).unwrap();
        assert_eq!(first[0].len(), 17);
        assert_eq!(first[0][16], Datum::Null);
        e.rewind(&mut ctx).unwrap();
        let second = drain(&mut e, &mut ctx).unwrap();
        assert_eq!(first, second);
        // 行は作り直さない（その文の中では一貫した行）。
        assert_eq!(rt.calls.get(), 1);
    }

    #[test]
    fn unknown_relation_is_an_internal_error() {
        let mut f = Fixture::new();
        let mut e: crate::executor::BoxedExecutor =
            Box::new(VirtualScanExec::new(12345, vec![], None));
        let err = f.run(&mut e).unwrap_err();
        assert_eq!(err.sqlstate, crate::error::sqlstate::INTERNAL_ERROR);
    }
}
