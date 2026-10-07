//! 計画時の定数畳み込み（観測できる範囲）。
//!
//! PostgreSQL の `eval_const_expressions` は、列を含まない部分式をすべて計画時に評価するので、定数の
//! エラー（`2147483647 + 1`）は、空の表・`LIMIT 0`・偽の WHERE で 1 行も評価されなくても出る。ここは
//! 各「最大の定数部分木」を（実行時と同じ遅延評価で）評価して、最初のエラーを返す。セッションの値や
//! context / runtime 関数を含む部分木は定数ではない（PostgreSQL では stable / volatile）。
//!
//! M3 の `planner::check_constant_exprs` を新しい Bound（`Var`）に移したもの。評価に `EvalCtx`
//! （セッション・runtime）が要るので `PlanEnv` からは呼べず、session が `plan` の前に呼ぶ。
//! L1 の `rules::const_fold` が `EvalCtx::for_constant_folding`（04-3）を得たら、そちらに移す。

use crate::analyzer::bound::{
    BoundExpr, BoundExprKind, BoundInsert, BoundQuery, BoundSelect, BoundSetExpr, BoundStatement,
    UpdateSource,
};
use crate::catalog::{CastMethod, FnKind};
use crate::error::Result;
use crate::executor::EvalCtx;
use crate::expr::lower_single_rel;
use crate::types::Datum;

/// 文の定数部分式を評価してリテラルに置き換える。最初のエラーを返す。
pub fn check_constant_exprs(stmt: &mut BoundStatement, ctx: &EvalCtx<'_>) -> Result<()> {
    let f = ConstFolder { ctx };
    match stmt {
        BoundStatement::Select(q) => f.query(q),
        BoundStatement::Insert(i) => f.insert(i),
        BoundStatement::Update(u) => {
            // 目的リストは列の順: SET 式と DEFAULT は列の順に畳み、その後に WHERE。
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
    /// INSERT: PostgreSQL の目的リストは列の順で、省略した列の DEFAULT を持つ。1 行の VALUES（または
    /// 引き上げられる単純な SELECT）は自分の式を自分の列の位置で出す。他の入力は DEFAULT の後に簡約する。
    fn insert(&self, i: &mut BoundInsert) -> Result<()> {
        let inline = i.coercions.is_none()
            && !matches!(&i.source.body, BoundSetExpr::Values { rows, .. } if rows.len() != 1);
        for col in 0..i.column_map.len() {
            match i.column_map[col] {
                None => {
                    if let Some(d) = &mut i.defaults[col] {
                        self.expr(d)?;
                    }
                }
                Some(k) if inline => match &mut i.source.body {
                    BoundSetExpr::Values { rows, .. } => {
                        self.expr(&mut rows[0][k])?;
                    }
                    BoundSetExpr::Select(s) => {
                        if let Some(t) = s.targets.get_mut(k) {
                            self.expr(t)?;
                        }
                    }
                    BoundSetExpr::SetOp { .. } => {}
                },
                Some(_) => {}
            }
        }
        self.query(&mut i.source)?;
        i.coercions
            .iter_mut()
            .flatten()
            .try_for_each(|e| self.expr(e).map(drop))
    }

    fn query(&self, q: &mut BoundQuery) -> Result<()> {
        match &mut q.body {
            BoundSetExpr::Values { rows, .. } => {
                for e in rows.iter_mut().flatten() {
                    self.expr(e)?;
                }
            }
            BoundSetExpr::Select(s) => self.select(s)?,
            BoundSetExpr::SetOp { .. } => {}
        }
        for e in q.limit.iter_mut().chain(&mut q.offset) {
            self.expr(e)?;
        }
        Ok(())
    }

    fn select(&self, s: &mut BoundSelect) -> Result<()> {
        for e in s.filter.iter_mut().chain(&mut s.targets) {
            self.expr(e)?;
        }
        Ok(())
    }

    /// `e` が定数かを返す（定数なら評価済み）。そうでなければ定数の子を畳む。
    fn expr(&self, e: &mut BoundExpr) -> Result<bool> {
        self.reduce_case(e)?;
        if !is_foldable(e) {
            self.children(e)?;
            // 子を畳んだ結果、`e` 自身が定数になることがある（`CASE WHEN false THEN col ELSE 1 END`）。
            if !is_foldable(e) {
                return Ok(false);
            }
        }
        if !matches!(e.kind, BoundExprKind::Literal(_)) {
            let v = self.eval(e)?;
            e.kind = BoundExprKind::Literal(v);
        }
        Ok(true)
    }

    fn eval(&self, e: &BoundExpr) -> Result<Datum> {
        crate::executor::eval::eval_const(&lower_single_rel(e)?, &Vec::new(), self.ctx)
    }

    /// 定数が偽 / NULL の CASE の腕を落とし、定数が真の腕で CASE を終わらせる（PostgreSQL と同じ）。
    /// 腕が残らなければ結果の式になる。
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

    /// `expr` と同じだが、定数式の値を返す。
    fn const_value(&self, e: &mut BoundExpr) -> Result<Option<Datum>> {
        if !is_foldable(e) {
            self.children(e)?;
            // 子を畳んだ結果、`e` が定数になることがある（`COALESCE(-7::int8, col)` は最初の項で決まる）。
            if !is_foldable(e) {
                return Ok(None);
            }
        }
        if let BoundExprKind::Literal(d) = &e.kind {
            return Ok(Some(d.clone()));
        }
        let v = self.eval(e)?;
        e.kind = BoundExprKind::Literal(v.clone());
        Ok(Some(v))
    }

    /// COALESCE / AND / OR の項を、`decides` が真になる最初の定数まで畳む（残りは評価せずに捨てる）。
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
            K::Literal(_)
            | K::Column(_)
            | K::SessionValue(_)
            | K::Aggregate(_)
            | K::SubLink { .. }
            | K::SubLinkOutput(_) => Ok(()),
            K::Operator { args, .. } | K::Function { args, .. } | K::MinMax { args, .. } => {
                each(args)
            }
            // PostgreSQL は決める最初の項で簡約を止め、残りは評価せずに捨てる。
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
                // 定数の条件は腕を落とす（偽 / NULL）か CASE を終わらせる（真）。死んだ結果は簡約しない。
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

/// 遅延評価の項（COALESCE / AND / OR）: 結果を決める最初のリテラルまでの項がすべて定数なら定数
/// （PostgreSQL は残りを捨てる。`COALESCE(46, col)` は `46`）。
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

/// 列参照・セッションの値・純粋でない関数・`Env` のキャスト・副問い合わせ・集約を含まない。
fn is_foldable(e: &BoundExpr) -> bool {
    use BoundExprKind as K;
    let all = |xs: &[BoundExpr]| xs.iter().all(is_foldable);
    match &e.kind {
        K::Literal(_) => true,
        K::Column(_)
        | K::SessionValue(_)
        | K::Aggregate(_)
        | K::SubLink { .. }
        | K::SubLinkOutput(_) => false,
        K::Operator { args, .. } | K::MinMax { args, .. } => all(args),
        K::Function { func, args } => matches!(func.kind, FnKind::Pure(_)) && all(args),
        K::Coalesce(args) => short_circuit_foldable(args, |d| !d.is_null()),
        K::And(args) => short_circuit_foldable(args, |d| matches!(d, Datum::Bool(false))),
        K::Or(args) => short_circuit_foldable(args, |d| matches!(d, Datum::Bool(true))),
        K::Cast { expr, method, .. } => !matches!(method, CastMethod::Env(_)) && is_foldable(expr),
        K::CoerceTypmod { expr, .. }
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
