//! 不変条件の検査の入口（`m4/04` §11.5）。
//!
//! 検査そのものは [`LogicalQuery::validate`]（L1〜L10。`logical.rs`）と [`PhysicalQuery::validate`]
//! （P1〜P11。`physical.rs`）にある。ここは、プランの構築を検査する関数（`build` の不変条件 §5.10 など）を
//! 足す場所で、区画は 2 つ。論理は L1、物理は L2 が、自分の区画の中だけを編集する。

use std::collections::HashSet;

use super::logical::{JoinKind, LogicalPlan, LogicalQuery};
use super::physical::PhysicalQuery;
use crate::error::{Error, Result};
use crate::expr::{ColId, ExprKind};

// ===== region: 論理（L1） =====

/// `build` の直後と各ルールの後に走る検査（`m4/04` §5.10・§11.5）。`stage` は `"build"` かルールの名前。
///
/// `LogicalQuery::validate`（L1〜L10）に加えて、Join の左右の出力が互いに素、根の plan に自由な列がない、
/// Semi / Anti は R2（`sublink`）より前に現れない、を確かめる。
pub fn validate_logical(q: &LogicalQuery, stage: &str) -> Result<()> {
    q.validate()?;
    let semi_allowed = !matches!(stage, "build" | "const_fold");
    let mut roots: Vec<(&str, &LogicalPlan)> = vec![("plan", &q.plan)];
    roots.extend(q.ctes.iter().map(|c| (c.name.as_str(), &c.plan)));
    for (name, p) in roots {
        if let Some(c) = p.outer_refs().into_iter().next() {
            return Err(Error::internal(format!(
                "invalid logical plan [{stage}] root {name} has a free column #{}",
                c.0
            )));
        }
        check_tree(p, stage, semi_allowed)?;
    }
    Ok(())
}

fn check_tree(p: &LogicalPlan, stage: &str, semi_allowed: bool) -> Result<()> {
    if let LogicalPlan::Join {
        kind, left, right, ..
    } = p
    {
        if !semi_allowed && matches!(kind, JoinKind::Semi | JoinKind::Anti) {
            return Err(Error::internal(format!(
                "invalid logical plan [{stage}] Semi / Anti join before the sublink rule"
            )));
        }
        let l: HashSet<ColId> = left.output_cols().into_iter().collect();
        if let Some(c) = right.output_cols().into_iter().find(|c| l.contains(c)) {
            return Err(Error::internal(format!(
                "invalid logical plan [{stage}] Join outputs of both sides share #{}",
                c.0
            )));
        }
    }
    for c in p.children() {
        check_tree(c, stage, semi_allowed)?;
    }
    // 式の中の副問い合わせ。
    let mut err = None;
    for e in p.exprs() {
        e.walk(&mut |n| {
            if err.is_none()
                && let ExprKind::SubLink { query, .. } = &n.kind
            {
                err = check_tree(&query.plan, stage, semi_allowed).err();
            }
            err.is_none()
        });
    }
    err.map_or(Ok(()), Err)
}

// ===== region: 物理（L2） =====

/// `physicalize` の後に走る検査。
pub fn validate_physical(q: &PhysicalQuery) -> Result<()> {
    q.validate()
}
