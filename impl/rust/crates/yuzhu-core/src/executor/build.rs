//! Builds an executor tree from a physical plan (moved from `executor/mod.rs`
//! in M2; owned by 担当 H2).

use super::BoxedExecutor;
use super::nodes::collect_constants;
use super::nodes::{
    DeleteExec, DistinctExec, FilterExec, InsertExec, LimitExec, ProjectExec, ResultExec,
    SeqScanExec, SortExec, UpdateExec, ValuesExec,
};
use crate::analyzer::BoundExpr;
use crate::planner::PhysicalPlan;

/// Builds an executor tree for `plan`.
pub fn build(plan: &PhysicalPlan) -> BoxedExecutor {
    match plan {
        PhysicalPlan::Result { exprs } => Box::new(ResultExec::new(exprs.clone())),
        PhysicalPlan::Values { rows } => Box::new(ValuesExec::new(rows.clone())),
        PhysicalPlan::SeqScan {
            rel,
            system_columns,
            ..
        } => Box::new(SeqScanExec::new(rel.clone(), system_columns.clone())),
        PhysicalPlan::Filter { input, predicate } => {
            Box::new(FilterExec::new(build(input), predicate.clone()))
        }
        PhysicalPlan::Project { input, exprs } => {
            Box::new(ProjectExec::new(build(input), exprs.clone()))
        }
        PhysicalPlan::Sort { input, keys } => Box::new(SortExec::new(build(input), keys.clone())),
        PhysicalPlan::Distinct { input } => Box::new(DistinctExec::new(build(input))),
        PhysicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            let mut folds = Vec::new();
            collect_folds(input, &mut folds);
            Box::new(LimitExec::new(build(input), limit.clone(), offset.clone()).with_folds(folds))
        }
        PhysicalPlan::Insert {
            rel,
            input,
            column_map,
            defaults,
            checks,
            not_null,
            table_name,
        } => Box::new(InsertExec::new(
            rel.clone(),
            table_name.clone(),
            build(input),
            column_map.clone(),
            defaults.clone(),
            checks.clone(),
            not_null.clone(),
        )),
        PhysicalPlan::Update {
            rel,
            input,
            assignments,
            checks,
            not_null,
            table_name,
        } => Box::new(UpdateExec::new(
            rel.clone(),
            table_name.clone(),
            build(input),
            assignments.clone(),
            checks.clone(),
            not_null.clone(),
        )),
        PhysicalPlan::Delete { rel, input } => Box::new(DeleteExec::new(rel.clone(), build(input))),
    }
}

/// LIMIT の下にある Project / Filter / Result の定数部分式を集める。
fn collect_folds(plan: &PhysicalPlan, out: &mut Vec<BoundExpr>) {
    match plan {
        PhysicalPlan::Result { exprs } => {
            for e in exprs {
                collect_constants(e, out);
            }
        }
        PhysicalPlan::Project { input, exprs } => {
            for e in exprs {
                collect_constants(e, out);
            }
            collect_folds(input, out);
        }
        PhysicalPlan::Filter { input, predicate } => {
            collect_constants(predicate, out);
            collect_folds(input, out);
        }
        PhysicalPlan::Sort { input, .. } | PhysicalPlan::Distinct { input } => {
            collect_folds(input, out);
        }
        _ => {}
    }
}
