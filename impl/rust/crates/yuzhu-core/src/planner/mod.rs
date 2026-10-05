//! Planner: `BoundStatement` → `PhysicalPlan` (M1 builds the physical plan
//! directly). (Owned by the executor implementer.)
//!
//! SELECT pipeline (see `BoundSelect`):
//! source (`Result` / `SeqScan` / `Values`) → `Filter` → `Project`
//! (all targets, including resjunk ORDER BY entries) → `Sort` (keys refer
//! to target positions) → `Project` (drop resjunk) → `Distinct` → `Limit`.
//! Dropping resjunk before `Distinct` is order-preserving because
//! `Distinct` keeps the first occurrence of each row.

pub mod plan;
mod simplify;

pub use plan::*;

use crate::analyzer::{
    BoundDelete, BoundExpr, BoundExprKind, BoundFrom, BoundInsert, BoundSelect, BoundStatement,
    BoundUpdate,
};
use crate::catalog::FnKind;
use crate::catalog::{SystemColumn, TableDef};
use crate::error::{Error, Result};
use crate::executor::EvalCtx;
use crate::executor::eval::eval_expr;
use crate::storage::RelHandle;
use crate::types::{Datum, SqlType, oid};

/// Plans a SELECT, INSERT, UPDATE or DELETE. DDL statements are not planned.
pub fn plan(stmt: &BoundStatement) -> Result<PhysicalPlan> {
    match stmt {
        BoundStatement::Select(s) => Ok(plan_select(s)),
        BoundStatement::Insert(i) => Ok(plan_insert(i)),
        BoundStatement::Update(u) => Ok(plan_update(u)),
        BoundStatement::Delete(d) => Ok(plan_delete(d)),
        BoundStatement::CreateTable(_)
        | BoundStatement::DropTable(_)
        | BoundStatement::Checkpoint => Err(Error::internal(
            "utility statements are executed by the session, not planned",
        )),
    }
}

/// Plan-time constant folding as far as it is observable: PostgreSQL's
/// `eval_const_expressions` evaluates every column-free subexpression while
/// planning, so a constant error (`2147483647 + 1`) is raised even when no
/// row would ever be evaluated (empty table, `LIMIT 0`, false WHERE).
/// Evaluates each maximal constant subtree (lazy CASE / COALESCE / AND / OR as
/// at run time) and discards the value; the first error is returned.
/// Subtrees with session values or context / runtime functions are not
/// constant (stable / volatile in PostgreSQL) and are skipped.
pub fn check_constant_exprs(stmt: &mut BoundStatement, ctx: &EvalCtx<'_>) -> Result<()> {
    let f = ConstFolder { ctx };
    match stmt {
        BoundStatement::Select(s) => f.select(s),
        BoundStatement::Insert(i) => f.insert(i),
        BoundStatement::Update(u) => {
            // The target list is in column order: SET expressions and
            // DEFAULTs are folded in that order, then WHERE.
            let mut order: Vec<usize> = (0..u.assignments.len()).collect();
            order.sort_by_key(|&k| u.assignments[k].0);
            for k in order {
                match &mut u.assignments[k].1 {
                    UpdateSource::Expr(e) | UpdateSource::Default(Some(e)) => {
                        f.expr(e)?;
                    }
                    UpdateSource::Default(None) => {}
                }
            }
            u.filter.iter_mut().try_for_each(|e| f.expr(e).map(drop))
        }
        BoundStatement::Delete(d) => d.filter.iter_mut().try_for_each(|e| f.expr(e).map(drop)),
        _ => Ok(()),
    }
}

struct ConstFolder<'a, 'b> {
    ctx: &'a EvalCtx<'b>,
}

impl ConstFolder<'_, '_> {
    /// INSERT: PostgreSQL's target list is in column order, holding the
    /// DEFAULT of every omitted column; a single-row VALUES (or a plain
    /// SELECT pulled up) contributes its expressions at their columns.
    /// Other sources are simplified after the defaults.
    fn insert(&self, i: &mut BoundInsert) -> Result<()> {
        let inline = i.coercions.is_none()
            && !matches!(&i.source.from, BoundFrom::Values { rows, .. } if rows.len() != 1);
        for col in 0..i.column_map.len() {
            match i.column_map[col] {
                None => {
                    if let Some(d) = &mut i.defaults[col] {
                        self.expr(d)?;
                    }
                }
                Some(k) if inline => match &mut i.source.from {
                    BoundFrom::Values { rows, .. } => {
                        self.expr(&mut rows[0][k])?;
                    }
                    _ => {
                        if let Some(t) = i.source.targets.get_mut(k) {
                            self.expr(t)?;
                        }
                    }
                },
                Some(_) => {}
            }
        }
        self.select(&mut i.source)?;
        i.coercions
            .iter_mut()
            .flatten()
            .try_for_each(|e| self.expr(e).map(drop))
    }

    fn select(&self, s: &mut BoundSelect) -> Result<()> {
        if let BoundFrom::Values { rows, .. } = &mut s.from {
            for e in rows.iter_mut().flatten() {
                self.expr(e)?;
            }
        }
        for e in s
            .filter
            .iter_mut()
            .chain(&mut s.targets)
            .chain(&mut s.limit)
            .chain(&mut s.offset)
        {
            self.expr(e)?;
        }
        Ok(())
    }

    /// Returns whether `e` is a constant (and has been evaluated if so);
    /// otherwise folds its constant children.
    fn expr(&self, e: &mut BoundExpr) -> Result<bool> {
        self.reduce_case(e)?;
        if !is_foldable(e) {
            self.children(e)?;
            // Folding the children may have made `e` itself constant
            // (`CASE WHEN false THEN col ELSE 1 END` became `1`).
            if !is_foldable(e) {
                return Ok(false);
            }
        }
        if !matches!(e.kind, BoundExprKind::Literal(_)) {
            let v = eval_expr(e, &Vec::new(), self.ctx)?;
            e.kind = BoundExprKind::Literal(v);
        }
        Ok(true)
    }

    /// Drops CASE arms whose condition is a constant false / NULL and ends
    /// the CASE at a constant true condition, as PostgreSQL's
    /// `eval_const_expressions` does; a CASE left without arms becomes its
    /// result.
    fn reduce_case(&self, e: &mut BoundExpr) -> Result<()> {
        let BoundExprKind::Case { arms, else_result } = &mut e.kind else {
            return Ok(());
        };
        let mut kept = Vec::new();
        let mut default = else_result.take();
        for (mut c, r) in std::mem::take(arms) {
            match self.const_value(&mut c)? {
                Some(Datum::Bool(true)) => {
                    default = Some(Box::new(r));
                    break;
                }
                Some(_) => {}
                None => kept.push((c, r)),
            }
        }
        if kept.is_empty() {
            let (ty, span) = (e.ty, e.span);
            *e = match default {
                Some(d) => *d,
                None => BoundExpr::new(BoundExprKind::Literal(Datum::Null), ty, span),
            };
        } else {
            e.kind = BoundExprKind::Case {
                arms: kept,
                else_result: default,
            };
        }
        Ok(())
    }

    /// Like `expr`, but returns the value of a constant expression.
    fn const_value(&self, e: &mut BoundExpr) -> Result<Option<Datum>> {
        if !is_foldable(e) {
            self.children(e)?;
            // Folding the children may have made `e` constant
            // (`COALESCE(-7::int8, col)` decides at its first operand).
            if !is_foldable(e) {
                return Ok(None);
            }
        }
        if let BoundExprKind::Literal(d) = &e.kind {
            return Ok(Some(d.clone()));
        }
        let v = eval_expr(e, &Vec::new(), self.ctx)?;
        e.kind = BoundExprKind::Literal(v.clone());
        Ok(Some(v))
    }

    /// Operands of COALESCE / AND / OR up to the first constant for which
    /// `decides` holds; the rest are dropped unevaluated.
    fn lazy_args(&self, args: &mut [BoundExpr], decides: impl Fn(&Datum) -> bool) -> Result<()> {
        for a in args.iter_mut() {
            if let Some(d) = self.const_value(a)?
                && decides(&d)
            {
                break;
            }
        }
        Ok(())
    }

    fn children(&self, e: &mut BoundExpr) -> Result<()> {
        use BoundExprKind as K;
        let each = |xs: &mut [BoundExpr]| xs.iter_mut().try_for_each(|x| self.expr(x).map(drop));
        match &mut e.kind {
            K::Literal(_) | K::ColumnRef { .. } | K::SessionValue(_) => Ok(()),
            K::Operator { args, .. } | K::Function { args, .. } | K::MinMax { args, .. } => {
                each(args)
            }
            // PostgreSQL stops simplifying at the first operand that decides
            // the result and drops the rest unevaluated.
            K::And(args) => self.lazy_args(args, |d| matches!(d, Datum::Bool(false))),
            K::Or(args) => self.lazy_args(args, |d| matches!(d, Datum::Bool(true))),
            K::Coalesce(args) => self.lazy_args(args, |d| !d.is_null()),
            K::Cast { expr, .. }
            | K::CoerceTypmod { expr, .. }
            | K::Not(expr)
            | K::IsNull(expr)
            | K::IsNotNull(expr)
            | K::BoolTest { expr, .. } => self.expr(expr).map(drop),
            K::Case { arms, else_result } => {
                // A constant condition drops its arm (false / NULL) or ends
                // the CASE (true) without simplifying the dead results.
                for (c, r) in arms {
                    match self.const_value(c)? {
                        Some(Datum::Bool(true)) => return self.expr(r).map(drop),
                        Some(_) => {}
                        None => {
                            self.expr(r)?;
                        }
                    }
                }
                else_result
                    .iter_mut()
                    .try_for_each(|x| self.expr(x).map(drop))
            }
            K::NullIf { left, right, .. } | K::DistinctFrom { left, right, .. } => {
                self.expr(left)?;
                self.expr(right).map(drop)
            }
            K::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                self.expr(expr)?;
                self.expr(pattern)?;
                escape.iter_mut().try_for_each(|x| self.expr(x).map(drop))
            }
            K::InList { expr, list, .. } => {
                self.expr(expr)?;
                each(list)
            }
        }
    }
}

/// Lazy operands (COALESCE / AND / OR): constant if every operand up to the
/// first literal that decides the result is constant, as PostgreSQL drops the
/// rest (`COALESCE(46, col)` is `46`).
fn short_circuit_foldable(args: &[BoundExpr], decides: impl Fn(&Datum) -> bool) -> bool {
    for a in args {
        if let BoundExprKind::Literal(d) = &a.kind
            && decides(d)
        {
            return true;
        }
        if !is_foldable(a) {
            return false;
        }
    }
    true
}

/// No column reference, session value or non-pure function anywhere inside.
fn is_foldable(e: &BoundExpr) -> bool {
    use BoundExprKind as K;
    let all = |xs: &[BoundExpr]| xs.iter().all(is_foldable);
    match &e.kind {
        K::Literal(_) => true,
        K::ColumnRef { .. } | K::SessionValue(_) => false,
        K::Operator { args, .. } | K::MinMax { args, .. } => all(args),
        K::Function { func, args } => matches!(func.kind, FnKind::Pure(_)) && all(args),

        K::Coalesce(args) => short_circuit_foldable(args, |d| !d.is_null()),
        K::And(args) => short_circuit_foldable(args, |d| matches!(d, Datum::Bool(false))),
        K::Or(args) => short_circuit_foldable(args, |d| matches!(d, Datum::Bool(true))),
        K::Cast { expr, .. }
        | K::CoerceTypmod { expr, .. }
        | K::Not(expr)
        | K::IsNull(expr)
        | K::IsNotNull(expr)
        | K::BoolTest { expr, .. } => is_foldable(expr),
        K::Case { arms, else_result } => {
            arms.iter().all(|(c, r)| is_foldable(c) && is_foldable(r))
                && else_result.as_deref().is_none_or(is_foldable)
        }
        K::NullIf { left, right, .. } | K::DistinctFrom { left, right, .. } => {
            is_foldable(left) && is_foldable(right)
        }
        K::Like {
            expr,
            pattern,
            escape,
            ..
        } => is_foldable(expr) && is_foldable(pattern) && escape.as_deref().is_none_or(is_foldable),
        K::InList { expr, list, .. } => is_foldable(expr) && all(list),
    }
}

fn column_ref(index: usize, src: &BoundExpr) -> BoundExpr {
    BoundExpr::new(BoundExprKind::ColumnRef { index }, src.ty, src.span)
}

/// Plans a SELECT (or bare VALUES).
pub fn plan_select(s: &BoundSelect) -> PhysicalPlan {
    let mut simplified = s.clone();
    simplified.filter = simplify::simplify_opt(s.filter.as_ref());
    simplified.targets.iter_mut().for_each(simplify::simplify);
    let s = &simplified;
    let visible = s.columns.len().min(s.targets.len());
    let has_resjunk = s.targets.len() > visible;

    // FROM-less SELECT without WHERE: a single Result node computes the
    // targets directly.
    let mut node = match (&s.from, &s.filter) {
        (BoundFrom::None, None) => PhysicalPlan::Result {
            exprs: s.targets.clone(),
        },
        (from, filter) => {
            let source = match from {
                BoundFrom::None => PhysicalPlan::Result { exprs: Vec::new() },
                BoundFrom::Table {
                    table,
                    system_columns,
                    ..
                } => PhysicalPlan::SeqScan {
                    rel: RelHandle::from_table(table),
                    columns: table.columns.iter().map(|c| c.ty).collect(),
                    system_columns: system_columns.clone(),
                },
                BoundFrom::Values { rows, .. } => PhysicalPlan::Values { rows: rows.clone() },
            };
            let filtered = match filter {
                Some(p) => PhysicalPlan::Filter {
                    input: Box::new(source),
                    predicate: p.clone(),
                },
                None => source,
            };
            if is_identity(&s.targets, from) {
                filtered
            } else {
                PhysicalPlan::Project {
                    input: Box::new(filtered),
                    exprs: s.targets.clone(),
                }
            }
        }
    };

    // PostgreSQL drops constant sort keys; with none left there is no Sort,
    // so LIMIT can stop the input early.
    let keys: Vec<SortKey> = s
        .order_by
        .iter()
        .filter(|k| !matches!(s.targets[k.target].kind, BoundExprKind::Literal(_)))
        .map(|k| SortKey {
            expr: column_ref(k.target, &s.targets[k.target]),
            descending: k.descending,
            nulls_first: k.nulls_first,
        })
        .collect();
    if !keys.is_empty() {
        node = PhysicalPlan::Sort {
            input: Box::new(node),
            keys,
        };
    }

    if has_resjunk {
        node = PhysicalPlan::Project {
            input: Box::new(node),
            exprs: s.targets[..visible]
                .iter()
                .enumerate()
                .map(|(i, t)| column_ref(i, t))
                .collect(),
        };
    }

    if s.distinct {
        node = PhysicalPlan::Distinct {
            input: Box::new(node),
        };
    }

    if s.limit.is_some() || s.offset.is_some() {
        node = PhysicalPlan::Limit {
            input: Box::new(node),
            limit: s.limit.clone(),
            offset: s.offset.clone(),
        };
    }
    node
}

/// True if `targets` is exactly `ColumnRef(0..n)` over a source producing
/// `n` columns (e.g. `SELECT * FROM t` or a bare VALUES).
fn is_identity(targets: &[BoundExpr], from: &BoundFrom) -> bool {
    let width = match from {
        BoundFrom::None => 0,
        BoundFrom::Table {
            table,
            system_columns,
            ..
        } => table.columns.len() + system_columns.len(),
        BoundFrom::Values { types, .. } => types.len(),
    };
    targets.len() == width
        && targets
            .iter()
            .enumerate()
            .all(|(i, t)| matches!(t.kind, BoundExprKind::ColumnRef { index } if index == i))
}

/// Plans an INSERT. CHECK constraints are evaluated in name order, as
/// PostgreSQL does.
pub fn plan_insert(i: &BoundInsert) -> PhysicalPlan {
    let mut checks = i.checks.clone();
    checks.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let mut input = plan_select(&i.source);
    if let Some(exprs) = &i.coercions {
        input = PhysicalPlan::Project {
            input: Box::new(input),
            exprs: exprs.clone(),
        };
    }
    PhysicalPlan::Insert {
        rel: RelHandle::from_table(&i.table),
        input: Box::new(input),
        column_map: i.column_map.clone(),
        defaults: i.defaults.clone(),
        checks,
        not_null: i.table.columns.iter().map(|c| c.not_null).collect(),
        table_name: i.table.name.clone(),
    }
}

/// The scan (and filter) feeding an UPDATE / DELETE: output rows are the
/// table's user columns followed by `ctid` (`m2.md` §4.7). WHERE may have
/// referenced system columns; a Project drops them.
fn plan_dml_input(
    table: &TableDef,
    filter: Option<&BoundExpr>,
    system_columns: &[SystemColumn],
) -> PhysicalPlan {
    let natts = table.columns.len();
    let mut scan_columns = system_columns.to_vec();
    scan_columns.push(SystemColumn::Ctid);
    let ctid_index = natts + system_columns.len();
    let mut node = PhysicalPlan::SeqScan {
        rel: RelHandle::from_table(table),
        columns: table.columns.iter().map(|c| c.ty).collect(),
        system_columns: scan_columns,
    };
    if let Some(p) = filter {
        node = PhysicalPlan::Filter {
            input: Box::new(node),
            predicate: p.clone(),
        };
    }
    if system_columns.is_empty() {
        return node;
    }
    let span = filter.map(|f| f.span).unwrap_or_default();
    let mut exprs: Vec<BoundExpr> = table
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| BoundExpr::new(BoundExprKind::ColumnRef { index: i }, c.ty, span))
        .collect();
    exprs.push(BoundExpr::new(
        BoundExprKind::ColumnRef { index: ctid_index },
        SqlType::of(oid::TID),
        span,
    ));
    PhysicalPlan::Project {
        input: Box::new(node),
        exprs,
    }
}

/// Plans an UPDATE. CHECK constraints are evaluated in name order.
pub fn plan_update(u: &BoundUpdate) -> PhysicalPlan {
    let mut checks = u.checks.clone();
    checks.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    PhysicalPlan::Update {
        rel: RelHandle::from_table(&u.table),
        input: Box::new(plan_dml_input(
            &u.table,
            simplify::simplify_opt(u.filter.as_ref()).as_ref(),
            &u.system_columns,
        )),
        assignments: u.assignments.clone(),
        checks,
        not_null: u.not_null.clone(),
        table_name: u.table.name.clone(),
    }
}

/// Plans a DELETE.
pub fn plan_delete(d: &BoundDelete) -> PhysicalPlan {
    PhysicalPlan::Delete {
        rel: RelHandle::from_table(&d.table),
        input: Box::new(plan_dml_input(
            &d.table,
            simplify::simplify_opt(d.filter.as_ref()).as_ref(),
            &d.system_columns,
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::analyzer::{BoundCheck, BoundSortKey, OutputColumn};
    use crate::catalog::fake::table_def;
    use crate::catalog::{ColumnDef, TableDef};
    use crate::executor::build;
    use crate::executor::eval::tests::{GT, col, int, lit, op, text};
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::{Datum, SqlType};

    fn out(name: &str, ty: SqlType) -> OutputColumn {
        OutputColumn {
            name: name.into(),
            ty,
            table_oid: 0,
            attnum: 0,
        }
    }

    fn select(from: BoundFrom, targets: Vec<BoundExpr>, visible: usize) -> BoundSelect {
        let columns = targets[..visible]
            .iter()
            .enumerate()
            .map(|(i, t)| out(&format!("c{i}"), t.ty))
            .collect();
        BoundSelect {
            from,
            filter: None,
            targets,
            columns,
            distinct: false,
            order_by: vec![],
            limit: None,
            offset: None,
        }
    }

    fn table() -> Arc<TableDef> {
        let c = |name: &str, attnum, ty| ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null: attnum == 1,
            default: None,
        };
        Arc::new(table_def(
            16384,
            "t",
            vec![c("a", 1, SqlType::INT4), c("b", 2, SqlType::TEXT)],
            vec![],
        ))
    }

    fn fixture_with_rows(rows: &[(i32, &str)]) -> Fixture {
        let mut f = Fixture::new();
        let t = table();
        for (a, b) in rows {
            f.storage
                .add_row(t.oid, vec![Datum::Int4(*a), Datum::Text((*b).into())]);
        }
        f.catalog.put_table(t);
        f
    }

    fn run(f: &mut Fixture, s: BoundSelect) -> Vec<Row> {
        let p = plan(&BoundStatement::Select(Box::new(s))).unwrap();
        f.run(&mut build(&p)).unwrap()
    }

    use crate::types::Row;

    #[test]
    fn fromless_select_is_a_result_node() {
        let s = select(BoundFrom::None, vec![int(1), text("x")], 2);
        assert!(matches!(plan_select(&s), PhysicalPlan::Result { .. }));
        let mut f = Fixture::new();
        assert_eq!(
            run(&mut f, s),
            vec![vec![Datum::Int4(1), Datum::Text("x".into())]]
        );
        // SELECT 1 WHERE false
        let mut s = select(BoundFrom::None, vec![int(1)], 1);
        s.filter = Some(lit(Datum::Bool(false), SqlType::BOOL));
        assert!(run(&mut f, s).is_empty());
    }

    #[test]
    fn select_star_skips_projection() {
        let s = select(
            BoundFrom::Table {
                table: table(),
                alias: None,
                system_columns: vec![],
            },
            vec![col(0, SqlType::INT4), col(1, SqlType::TEXT)],
            2,
        );
        assert!(matches!(plan_select(&s), PhysicalPlan::SeqScan { .. }));
        let mut f = fixture_with_rows(&[(1, "a"), (2, "b")]);
        assert_eq!(run(&mut f, s).len(), 2);
    }

    #[test]
    fn filter_order_by_resjunk_limit() {
        // SELECT b FROM t WHERE a > 1 ORDER BY a DESC LIMIT 2 OFFSET 1
        let mut s = select(
            BoundFrom::Table {
                table: table(),
                alias: None,
                system_columns: vec![],
            },
            vec![col(1, SqlType::TEXT), col(0, SqlType::INT4)],
            1,
        );
        s.filter = Some(op(&GT, col(0, SqlType::INT4), int(1)));
        s.order_by = vec![BoundSortKey {
            target: 1,
            descending: true,
            nulls_first: true,
        }];
        s.limit = Some(lit(Datum::Int8(2), SqlType::INT8));
        s.offset = Some(lit(Datum::Int8(1), SqlType::INT8));
        let mut f = fixture_with_rows(&[(3, "c"), (1, "a"), (5, "e"), (4, "d"), (2, "b")]);
        let rows = run(&mut f, s);
        assert_eq!(
            rows,
            vec![vec![Datum::Text("d".into())], vec![Datum::Text("c".into())]]
        );
    }

    #[test]
    fn distinct_after_sort_and_values() {
        // SELECT DISTINCT column1 FROM (VALUES (2),(1),(2),(3)) ORDER BY 1
        let rows = vec![vec![int(2)], vec![int(1)], vec![int(2)], vec![int(3)]];
        let mut s = select(
            BoundFrom::Values {
                rows,
                types: vec![SqlType::INT4],
            },
            vec![col(0, SqlType::INT4)],
            1,
        );
        s.distinct = true;
        s.order_by = vec![BoundSortKey {
            target: 0,
            descending: false,
            nulls_first: false,
        }];
        let mut f = Fixture::new();
        assert_eq!(
            run(&mut f, s),
            vec![
                vec![Datum::Int4(1)],
                vec![Datum::Int4(2)],
                vec![Datum::Int4(3)]
            ]
        );
    }

    #[test]
    fn insert_plan_and_execution() {
        let t = table();
        let ck = |name: &str| BoundCheck {
            name: name.into(),
            expr: op(&GT, col(0, SqlType::INT4), int(0)),
        };
        let ins = BoundInsert {
            table: t.clone(),
            source: Box::new(select(
                BoundFrom::Values {
                    rows: vec![vec![int(1)], vec![int(2)]],
                    types: vec![SqlType::INT4],
                },
                vec![col(0, SqlType::INT4)],
                1,
            )),
            coercions: None,
            column_map: vec![Some(0), None],
            defaults: vec![None, Some(text("dflt"))],
            checks: vec![ck("zz"), ck("aa")],
        };
        let p = plan(&BoundStatement::Insert(ins)).unwrap();
        let PhysicalPlan::Insert {
            checks, not_null, ..
        } = &p
        else {
            panic!("expected Insert, got {p:?}");
        };
        assert_eq!(
            checks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["aa", "zz"]
        );
        assert_eq!(not_null, &vec![true, false]);
        let mut f = fixture_with_rows(&[]);
        let mut e = build(&p);
        assert!(f.run(&mut e).unwrap().is_empty());
        assert_eq!(e.rows_affected(), 2);
        assert_eq!(
            f.storage.rows(t.oid)[1],
            vec![Datum::Int4(2), Datum::Text("dflt".into())]
        );
    }
}

#[cfg(test)]
mod dml_tests {
    use std::sync::Arc;

    use super::*;
    use crate::analyzer::{BoundCheck, UpdateSource};
    use crate::catalog::fake::table_def;
    use crate::catalog::{ColumnDef, TableDef};
    use crate::error::sqlstate;
    use crate::executor::build;
    use crate::executor::eval::tests::{GT, col, int, lit, null, op, text};
    use crate::executor::nodes::test_util::Fixture;
    use crate::storage::TmResult;
    use crate::types::{Datum, Row};

    const T: u32 = 16384;

    fn table() -> Arc<TableDef> {
        let c = |name: &str, attnum, ty, not_null| ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null,
            default: None,
        };
        Arc::new(table_def(
            T,
            "t",
            vec![
                c("a", 1, SqlType::INT4, true),
                c("b", 2, SqlType::TEXT, false),
                c("c", 3, SqlType::INT4, false),
            ],
            vec![],
        ))
    }

    fn fixture(rows: &[(i32, &str, i32)]) -> Fixture {
        let mut f = Fixture::new();
        let t = table();
        for (a, b, c) in rows {
            f.storage.add_row(
                T,
                vec![Datum::Int4(*a), Datum::Text((*b).into()), Datum::Int4(*c)],
            );
        }
        f.catalog.put_table(t);
        f
    }

    fn row(a: i32, b: &str, c: i32) -> Row {
        vec![Datum::Int4(a), Datum::Text(b.into()), Datum::Int4(c)]
    }

    fn update(assignments: Vec<(usize, UpdateSource)>, filter: Option<BoundExpr>) -> BoundUpdate {
        BoundUpdate {
            table: table(),
            assignments,
            filter,
            system_columns: vec![],
            checks: vec![],
            not_null: vec![true, false, false],
        }
    }

    fn run_plan(f: &mut Fixture, p: &PhysicalPlan) -> Result<u64> {
        let mut e = build(p);
        f.run(&mut e)?;
        Ok(e.rows_affected())
    }

    fn a_gt(n: i32) -> BoundExpr {
        op(&GT, col(0, SqlType::INT4), int(n))
    }

    #[test]
    fn update_with_where() {
        let mut f = fixture(&[(1, "x", 0), (2, "y", 0)]);
        let u = update(vec![(1, UpdateSource::Expr(text("z")))], Some(a_gt(1)));
        let p = plan(&BoundStatement::Update(u)).unwrap();
        // No system columns: Update <- Filter <- SeqScan(ctid).
        let PhysicalPlan::Update { input, .. } = &p else {
            panic!("expected Update");
        };
        assert!(matches!(**input, PhysicalPlan::Filter { .. }));
        assert_eq!(run_plan(&mut f, &p).unwrap(), 1);
        assert_eq!(f.storage.rows(T), vec![row(1, "x", 0), row(2, "z", 0)]);
    }

    #[test]
    fn set_expressions_see_the_old_row() {
        let mut f = fixture(&[(1, "x", 10)]);
        let u = update(
            vec![
                (0, UpdateSource::Expr(col(2, SqlType::INT4))),
                (2, UpdateSource::Expr(col(0, SqlType::INT4))),
            ],
            None,
        );
        let p = plan_update(&u);
        assert_eq!(run_plan(&mut f, &p).unwrap(), 1);
        assert_eq!(f.storage.rows(T), vec![row(10, "x", 1)]);
    }

    #[test]
    fn update_without_where_does_not_rescan_new_versions() {
        let mut f = fixture(&[(1, "a", 0), (2, "b", 0), (3, "c", 0)]);
        let u = update(vec![(2, UpdateSource::Expr(int(9)))], None);
        assert_eq!(run_plan(&mut f, &plan_update(&u)).unwrap(), 3);
        assert_eq!(f.storage.rows(T).len(), 3);
        // All writes used the transaction's XID and the current command ID.
        assert_eq!(f.storage.writes(), vec![(f.txn.xid.unwrap(), 0); 3]);
    }

    #[test]
    fn default_source() {
        let mut f = fixture(&[(1, "x", 5)]);
        let u = update(
            vec![
                (1, UpdateSource::Default(None)),
                (2, UpdateSource::Default(Some(int(42)))),
            ],
            None,
        );
        run_plan(&mut f, &plan_update(&u)).unwrap();
        assert_eq!(
            f.storage.rows(T),
            vec![vec![Datum::Int4(1), Datum::Null, Datum::Int4(42)]]
        );
    }

    #[test]
    fn system_columns_in_where_are_dropped_before_update() {
        let mut f = fixture(&[(1, "x", 0), (2, "y", 0)]);
        let mut u = update(vec![(2, UpdateSource::Expr(int(1)))], None);
        u.system_columns = vec![SystemColumn::TableOid];
        u.filter = Some(op(&GT, col(3, SqlType::of(oid::OID)), int(0)));
        // The comparison is int4 > int4 over an oid value; use a literal
        // true filter instead of an oid operator.
        u.filter = Some(lit(Datum::Bool(true), SqlType::BOOL));
        let p = plan_update(&u);
        let PhysicalPlan::Update { input, .. } = &p else {
            panic!("expected Update");
        };
        let PhysicalPlan::Project { exprs, .. } = &**input else {
            panic!("expected Project, got {input:?}");
        };
        assert_eq!(exprs.len(), 4);
        assert!(matches!(
            exprs[3].kind,
            BoundExprKind::ColumnRef { index: 4 }
        ));
        assert_eq!(run_plan(&mut f, &p).unwrap(), 2);
        assert_eq!(f.storage.rows(T), vec![row(1, "x", 1), row(2, "y", 1)]);
    }

    #[test]
    fn not_null_violation_leaves_the_row() {
        let mut f = fixture(&[(1, "x", 0)]);
        let u = update(vec![(0, UpdateSource::Expr(null(SqlType::INT4)))], None);
        let e = run_plan(&mut f, &plan_update(&u)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NOT_NULL_VIOLATION);
        assert_eq!(
            e.detail.as_deref(),
            Some("Failing row contains (null, x, 0).")
        );
        assert_eq!(f.storage.rows(T), vec![row(1, "x", 0)]);
        assert!(f.storage.writes().is_empty());
    }

    #[test]
    fn check_violation_is_found_before_writing_and_in_name_order() {
        let mut f = fixture(&[(1, "x", 0), (5, "y", 0)]);
        let ck = |name: &str, n| BoundCheck {
            name: name.into(),
            expr: a_gt(n),
        };
        let mut u = update(vec![(0, UpdateSource::Expr(int(1)))], None);
        u.checks = vec![ck("t_zz", 0), ck("t_aa", 1)];
        let e = run_plan(&mut f, &plan_update(&u)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::CHECK_VIOLATION);
        assert_eq!(
            e.message,
            "new row for relation \"t\" violates check constraint \"t_aa\""
        );
        assert!(f.storage.writes().is_empty());
    }

    #[test]
    fn delete_with_and_without_where() {
        let mut f = fixture(&[(1, "x", 0), (2, "y", 0), (3, "z", 0)]);
        let d = BoundDelete {
            table: table(),
            filter: Some(a_gt(1)),
            system_columns: vec![],
        };
        let p = plan(&BoundStatement::Delete(d)).unwrap();
        assert_eq!(run_plan(&mut f, &p).unwrap(), 2);
        assert_eq!(f.storage.rows(T), vec![row(1, "x", 0)]);
        let d = BoundDelete {
            table: table(),
            filter: None,
            system_columns: vec![],
        };
        assert_eq!(run_plan(&mut f, &plan_delete(&d)).unwrap(), 1);
        assert!(f.storage.rows(T).is_empty());
    }

    #[test]
    fn self_modified_rows() {
        // Same command: skipped and not counted.
        let mut f = fixture(&[(1, "x", 0)]);
        f.storage.force_result(TmResult::SelfModified { cmax: 0 });
        let u = update(vec![(2, UpdateSource::Expr(int(1)))], None);
        assert_eq!(run_plan(&mut f, &plan_update(&u)).unwrap(), 0);
        // An earlier command: 27000.
        let mut f = fixture(&[(1, "x", 0)]);
        f.storage.force_result(TmResult::SelfModified { cmax: 7 });
        let e = run_plan(&mut f, &plan_update(&u)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::TRIGGERED_DATA_CHANGE_VIOLATION);
        let mut f = fixture(&[(1, "x", 0)]);
        f.storage.force_result(TmResult::SelfModified { cmax: 7 });
        let d = BoundDelete {
            table: table(),
            filter: None,
            system_columns: vec![],
        };
        let e = run_plan(&mut f, &plan_delete(&d)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::TRIGGERED_DATA_CHANGE_VIOLATION);
        // Anything else is an internal error.
        let mut f = fixture(&[(1, "x", 0)]);
        f.storage.force_result(TmResult::Deleted {
            xmax: crate::txn::Xid(9),
        });
        let e = run_plan(&mut f, &plan_delete(&d)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn interrupts_stop_dml() {
        let mut f = fixture(&[(1, "x", 0)]);
        f.interrupts.request_terminate();
        let d = BoundDelete {
            table: table(),
            filter: None,
            system_columns: vec![],
        };
        let e = run_plan(&mut f, &plan_delete(&d)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::ADMIN_SHUTDOWN);
        assert_eq!(f.storage.rows(T).len(), 1);
    }

    #[test]
    fn malformed_input_rows_are_internal_errors() {
        use crate::executor::nodes::ValuesExec;
        let mut f = fixture(&[]);
        let bad = |exprs: Vec<BoundExpr>| -> crate::executor::BoxedExecutor {
            Box::new(crate::executor::nodes::DeleteExec::new(
                RelHandle::from_table(&table()),
                Box::new(ValuesExec::new(vec![exprs])),
            ))
        };
        // Too short.
        let e = f.run(&mut bad(vec![int(1)])).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        // Last column is not a ctid.
        let e = f
            .run(&mut bad(vec![int(1), text("x"), int(2), int(3)]))
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }
}
