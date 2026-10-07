//! 式の走査と書き換え（`m4/00-contracts.md` §6.5、`m4/02-pipeline-refactor.md` §3.1.3〜§3.1.5）。
//!
//! 子の順序は字面の左から右（`m4/02` §3.1.2 の表）で、`SubPlanId` / `ParamId` の割り当てと
//! deparse の安定性がこれに依存する。`SubLink` は `test` だけが子で、`query` の中へは
//! `walk` も `try_map` も降りない。`Aggregate` は `args` → `filter` の順に降りる。

#![allow(clippy::too_many_lines)]

use super::{AggCall, AggOrderKey, Expr, ExprKind, PhysCol, SubPlanId, Var};
use crate::error::{Error, Result};
use crate::types::Datum;

/// `try_map` の変換関数。葉（`Column`・`Aggregate`・`SubLink`）は必ず `Some` を返す。
type MapFn<'a, C, Q, C2, Q2> = dyn FnMut(&Expr<C, Q>) -> Result<Option<Expr<C2, Q2>>> + 'a;

impl<C, Q> Expr<C, Q> {
    /// 子の式を、`m4/02` §3.1.2 の順序で返す。`Aggregate` は `args`、`filter`。`SubLink` は `test` だけ。
    pub fn children(&self) -> Vec<&Self> {
        use ExprKind as K;
        match &self.kind {
            K::Literal(_) | K::Column(_) | K::SessionValue(_) | K::SubLinkOutput(_) => vec![],
            K::Operator { args, .. } | K::Function { args, .. } | K::MinMax { args, .. } => {
                args.iter().collect()
            }
            K::And(v) | K::Or(v) | K::Coalesce(v) => v.iter().collect(),
            K::Cast { expr, .. }
            | K::CoerceTypmod { expr, .. }
            | K::BoolTest { expr, .. }
            | K::Not(expr)
            | K::IsNull(expr)
            | K::IsNotNull(expr) => vec![&**expr],
            K::Case { arms, else_result } => {
                let mut out: Vec<&Self> = Vec::with_capacity(arms.len() * 2 + 1);
                for (cond, result) in arms {
                    out.push(cond);
                    out.push(result);
                }
                out.extend(else_result.iter().map(|e| &**e));
                out
            }
            K::NullIf { left, right, .. } | K::DistinctFrom { left, right, .. } => {
                vec![&**left, &**right]
            }
            K::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                let mut out = vec![&**expr, &**pattern];
                out.extend(escape.iter().map(|e| &**e));
                out
            }
            K::InList { expr, list, .. } => {
                let mut out = vec![&**expr];
                out.extend(list.iter());
                out
            }
            K::Aggregate(call) => {
                let mut out: Vec<&Self> = call.args.iter().collect();
                out.extend(call.filter.iter());
                out.extend(call.order_by.iter().map(|k| &k.expr));
                out
            }
            K::SubLink { test, .. } => test.iter().map(|e| &**e).collect(),
        }
    }

    /// 先行順に訪れる。`f` が `false` を返したらその部分木の子は訪れない（兄弟には進む）。
    pub fn walk(&self, f: &mut dyn FnMut(&Expr<C, Q>) -> bool) {
        if !f(self) {
            return;
        }
        for child in self.children() {
            child.walk(f);
        }
    }

    /// 部分木のどこかで `pred` が true か（`SubLink` の `query` には降りない）。
    pub fn any(&self, pred: &mut dyn FnMut(&Expr<C, Q>) -> bool) -> bool {
        let mut found = false;
        self.walk(&mut |e| {
            if found {
                return false;
            }
            if pred(e) {
                found = true;
                return false;
            }
            true
        });
        found
    }

    pub fn contains_aggregate(&self) -> bool {
        self.any(&mut |e| matches!(e.kind, ExprKind::Aggregate(_)))
    }

    pub fn contains_sublink(&self) -> bool {
        self.any(&mut |e| matches!(e.kind, ExprKind::SubLink { .. }))
    }

    /// AND の最上位の項。`And` なら平らにした各項、そうでなければ自分 1 つ。
    pub fn conjuncts(&self) -> Vec<&Self> {
        fn go<'e, C, Q>(e: &'e Expr<C, Q>, out: &mut Vec<&'e Expr<C, Q>>) {
            if let ExprKind::And(parts) = &e.kind {
                for p in parts {
                    go(p, out);
                }
            } else {
                out.push(e);
            }
        }
        let mut out = Vec::new();
        go(self, &mut out);
        out
    }

    /// 上から書き換える。各ノードでまず `f` を呼び、`Some(e)` ならその部分木を `e` に置き換えて
    /// 子へは降りない（`e.ty` と `e.span` は `e` のものを使う）。`None` なら構造を保って子へ降りる。
    ///
    /// 葉扱いの 3 変種（`Column`・`Aggregate`・`SubLink`）は `f` が `None` を返すと `Error::internal`
    /// （型パラメータが変わる変換で変換し忘れると、別の層の式に別の層の列が混ざるため）。
    /// `SubLink` を受けた `f` は `test` と `query` の両方を変換して 1 つの `SubLink` を返す責任を持つ。
    /// エラーは最初のものを返し、以降は降りない。
    pub fn try_map<C2, Q2>(&self, f: &mut MapFn<'_, C, Q, C2, Q2>) -> Result<Expr<C2, Q2>> {
        use ExprKind as K;
        if let Some(replaced) = f(self)? {
            return Ok(replaced);
        }
        let kind: K<C2, Q2> = match &self.kind {
            K::Literal(d) => K::Literal(d.clone()),
            K::SessionValue(k) => K::SessionValue(*k),
            K::SubLinkOutput(i) => K::SubLinkOutput(*i),
            K::Column(_) => return Err(unconverted("Column")),
            K::Aggregate(_) => return Err(unconverted("Aggregate")),
            K::SubLink { .. } => return Err(unconverted("SubLink")),
            K::Operator { op, args } => K::Operator {
                op,
                args: map_all(args, f)?,
            },
            K::Function { func, args } => K::Function {
                func,
                args: map_all(args, f)?,
            },
            K::MinMax {
                greatest,
                args,
                cmp,
            } => K::MinMax {
                greatest: *greatest,
                args: map_all(args, f)?,
                cmp,
            },
            K::Cast {
                expr,
                method,
                implicit,
            } => K::Cast {
                expr: map_box(expr, f)?,
                method: *method,
                implicit: *implicit,
            },
            K::CoerceTypmod { expr, explicit } => K::CoerceTypmod {
                expr: map_box(expr, f)?,
                explicit: *explicit,
            },
            K::And(v) => K::And(map_all(v, f)?),
            K::Or(v) => K::Or(map_all(v, f)?),
            K::Coalesce(v) => K::Coalesce(map_all(v, f)?),
            K::Not(e) => K::Not(map_box(e, f)?),
            K::IsNull(e) => K::IsNull(map_box(e, f)?),
            K::IsNotNull(e) => K::IsNotNull(map_box(e, f)?),
            K::BoolTest { expr, test } => K::BoolTest {
                expr: map_box(expr, f)?,
                test: *test,
            },
            K::Case { arms, else_result } => {
                let mut new_arms = Vec::with_capacity(arms.len());
                for (cond, result) in arms {
                    let cond = cond.try_map(f)?;
                    let result = result.try_map(f)?;
                    new_arms.push((cond, result));
                }
                K::Case {
                    arms: new_arms,
                    else_result: map_opt_box(else_result.as_deref(), f)?,
                }
            }
            K::NullIf { left, right, eq_op } => K::NullIf {
                left: map_box(left, f)?,
                right: map_box(right, f)?,
                eq_op,
            },
            K::DistinctFrom {
                left,
                right,
                eq_op,
                negated,
            } => K::DistinctFrom {
                left: map_box(left, f)?,
                right: map_box(right, f)?,
                eq_op,
                negated: *negated,
            },
            K::Like {
                expr,
                pattern,
                escape,
                negated,
                case_insensitive,
            } => K::Like {
                expr: map_box(expr, f)?,
                pattern: map_box(pattern, f)?,
                escape: map_opt_box(escape.as_deref(), f)?,
                negated: *negated,
                case_insensitive: *case_insensitive,
            },
            K::InList {
                expr,
                list,
                eq_op,
                negated,
            } => K::InList {
                expr: map_box(expr, f)?,
                list: map_all(list, f)?,
                eq_op,
                negated: *negated,
            },
        };
        Ok(Expr {
            kind,
            ty: self.ty,
            span: self.span,
        })
    }
}

fn unconverted(variant: &str) -> Error {
    Error::internal(format!(
        "try_map: leaf {variant} was not converted by the mapping function"
    ))
}

fn map_all<C, Q, C2, Q2>(
    v: &[Expr<C, Q>],
    f: &mut MapFn<'_, C, Q, C2, Q2>,
) -> Result<Vec<Expr<C2, Q2>>> {
    let mut out = Vec::with_capacity(v.len());
    for e in v {
        out.push(e.try_map(f)?);
    }
    Ok(out)
}

fn map_box<C, Q, C2, Q2>(
    e: &Expr<C, Q>,
    f: &mut MapFn<'_, C, Q, C2, Q2>,
) -> Result<Box<Expr<C2, Q2>>> {
    Ok(Box::new(e.try_map(f)?))
}

fn map_opt_box<C, Q, C2, Q2>(
    e: Option<&Expr<C, Q>>,
    f: &mut MapFn<'_, C, Q, C2, Q2>,
) -> Result<Option<Box<Expr<C2, Q2>>>> {
    e.map(|e| map_box(e, f)).transpose()
}

impl<C: Clone, Q: Clone> Expr<C, Q> {
    /// 型を変えない書き換え。`f` が `None` を返した葉（`Column`・`Aggregate`・`SubLink`）は clone して
    /// そのまま使う。`None` を返した `Aggregate` は `args` / `filter` に、`SubLink` は `test` に降りる
    /// （`query` は clone）。
    pub fn try_rewrite(&self, f: &mut dyn FnMut(&Self) -> Result<Option<Self>>) -> Result<Self> {
        self.try_map(&mut |e: &Self| {
            if let Some(replaced) = f(e)? {
                return Ok(Some(replaced));
            }
            let kind = match &e.kind {
                ExprKind::Column(c) => ExprKind::Column(c.clone()),
                ExprKind::Aggregate(call) => {
                    let mut args = Vec::with_capacity(call.args.len());
                    for a in &call.args {
                        args.push(a.try_rewrite(&mut *f)?);
                    }
                    let filter = call
                        .filter
                        .as_ref()
                        .map(|x| x.try_rewrite(&mut *f))
                        .transpose()?;
                    let mut order_by = Vec::with_capacity(call.order_by.len());
                    for k in &call.order_by {
                        order_by.push(AggOrderKey {
                            expr: k.expr.try_rewrite(&mut *f)?,
                            descending: k.descending,
                            nulls_first: k.nulls_first,
                        });
                    }
                    ExprKind::Aggregate(Box::new(AggCall {
                        func: call.func,
                        args,
                        distinct: call.distinct,
                        filter,
                        order_by,
                    }))
                }
                ExprKind::SubLink { kind, test, query } => {
                    let test = test
                        .as_deref()
                        .map(|t| t.try_rewrite(&mut *f).map(Box::new))
                        .transpose()?;
                    ExprKind::SubLink {
                        kind: *kind,
                        test,
                        query: query.clone(),
                    }
                }
                // 葉以外は try_map が構造を保って子へ降りる。
                _ => return Ok(None),
            };
            Ok(Some(Expr {
                kind,
                ty: e.ty,
                span: e.span,
            }))
        })
    }

    /// 出現順・重複ありの `Column` の一覧（`SubLink` の `query` の中は含まない）。
    pub fn columns(&self) -> Vec<C> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let ExprKind::Column(c) = &e.kind {
                out.push(c.clone());
            }
            true
        });
        out
    }
}

impl<C: PartialEq, Q> Expr<C, Q> {
    /// 構造の等価（`span` と `Cast.implicit` は無視。PostgreSQL の `equal()`）。型 `ty` は比べる。
    /// `Literal` の float はビット比較、`Operator` / `Function` は OID、`Cast` は `method` の種類、
    /// `Aggregate` は `func.oid`・`distinct`・`args`・`filter` で比べる。`SubLink` は常に false（保守的）。
    pub fn same_as(&self, other: &Self) -> bool {
        use ExprKind as K;
        if self.ty != other.ty {
            return false;
        }
        match (&self.kind, &other.kind) {
            (K::Literal(x), K::Literal(y)) => same_datum(x, y),
            (K::Column(x), K::Column(y)) => x == y,
            (K::SessionValue(x), K::SessionValue(y)) => x == y,
            (K::SubLinkOutput(x), K::SubLinkOutput(y)) => x == y,
            (K::Operator { op: o1, args: a1 }, K::Operator { op: o2, args: a2 }) => {
                o1.oid == o2.oid && all_same(a1, a2)
            }
            (K::Function { func: f1, args: a1 }, K::Function { func: f2, args: a2 }) => {
                f1.oid == f2.oid && all_same(a1, a2)
            }
            (
                K::MinMax {
                    greatest: g1,
                    args: a1,
                    cmp: c1,
                },
                K::MinMax {
                    greatest: g2,
                    args: a2,
                    cmp: c2,
                },
            ) => g1 == g2 && c1.oid == c2.oid && all_same(a1, a2),
            (
                K::Cast {
                    expr: e1,
                    method: m1,
                    ..
                },
                K::Cast {
                    expr: e2,
                    method: m2,
                    ..
                },
            ) => std::mem::discriminant(m1) == std::mem::discriminant(m2) && e1.same_as(e2),
            (
                K::CoerceTypmod {
                    expr: e1,
                    explicit: x1,
                },
                K::CoerceTypmod {
                    expr: e2,
                    explicit: x2,
                },
            ) => x1 == x2 && e1.same_as(e2),
            (K::And(x), K::And(y)) | (K::Or(x), K::Or(y)) | (K::Coalesce(x), K::Coalesce(y)) => {
                all_same(x, y)
            }
            (K::Not(x), K::Not(y))
            | (K::IsNull(x), K::IsNull(y))
            | (K::IsNotNull(x), K::IsNotNull(y)) => x.same_as(y),
            (K::BoolTest { expr: e1, test: t1 }, K::BoolTest { expr: e2, test: t2 }) => {
                t1 == t2 && e1.same_as(e2)
            }
            (
                K::Case {
                    arms: r1,
                    else_result: e1,
                },
                K::Case {
                    arms: r2,
                    else_result: e2,
                },
            ) => {
                r1.len() == r2.len()
                    && r1
                        .iter()
                        .zip(r2)
                        .all(|((c1, v1), (c2, v2))| c1.same_as(c2) && v1.same_as(v2))
                    && opt_same(e1.as_deref(), e2.as_deref())
            }
            (
                K::NullIf {
                    left: l1,
                    right: r1,
                    eq_op: o1,
                },
                K::NullIf {
                    left: l2,
                    right: r2,
                    eq_op: o2,
                },
            ) => o1.oid == o2.oid && l1.same_as(l2) && r1.same_as(r2),
            (
                K::DistinctFrom {
                    left: l1,
                    right: r1,
                    eq_op: o1,
                    negated: n1,
                },
                K::DistinctFrom {
                    left: l2,
                    right: r2,
                    eq_op: o2,
                    negated: n2,
                },
            ) => n1 == n2 && o1.oid == o2.oid && l1.same_as(l2) && r1.same_as(r2),
            (
                K::Like {
                    expr: e1,
                    pattern: p1,
                    escape: x1,
                    negated: n1,
                    case_insensitive: i1,
                },
                K::Like {
                    expr: e2,
                    pattern: p2,
                    escape: x2,
                    negated: n2,
                    case_insensitive: i2,
                },
            ) => {
                n1 == n2
                    && i1 == i2
                    && e1.same_as(e2)
                    && p1.same_as(p2)
                    && opt_same(x1.as_deref(), x2.as_deref())
            }
            (
                K::InList {
                    expr: e1,
                    list: l1,
                    eq_op: o1,
                    negated: n1,
                },
                K::InList {
                    expr: e2,
                    list: l2,
                    eq_op: o2,
                    negated: n2,
                },
            ) => n1 == n2 && o1.oid == o2.oid && e1.same_as(e2) && all_same(l1, l2),
            (K::Aggregate(a1), K::Aggregate(a2)) => {
                a1.func.oid == a2.func.oid
                    && a1.distinct == a2.distinct
                    && all_same(&a1.args, &a2.args)
                    && opt_same(a1.filter.as_ref(), a2.filter.as_ref())
                    && a1.order_by.len() == a2.order_by.len()
                    && a1.order_by.iter().zip(&a2.order_by).all(|(x, y)| {
                        x.descending == y.descending
                            && x.nulls_first == y.nulls_first
                            && x.expr.same_as(&y.expr)
                    })
            }
            // SubLink は常に不一致（保守的）。種類が違う組も不一致。
            _ => false,
        }
    }
}

fn same_datum(x: &Datum, y: &Datum) -> bool {
    match (x, y) {
        (Datum::Float4(p), Datum::Float4(q)) => p.to_bits() == q.to_bits(),
        (Datum::Float8(p), Datum::Float8(q)) => p.to_bits() == q.to_bits(),
        _ => x == y,
    }
}

fn all_same<C: PartialEq, Q>(x: &[Expr<C, Q>], y: &[Expr<C, Q>]) -> bool {
    x.len() == y.len() && x.iter().zip(y).all(|(p, q)| p.same_as(q))
}

fn opt_same<C: PartialEq, Q>(x: Option<&Expr<C, Q>>, y: Option<&Expr<C, Q>>) -> bool {
    match (x, y) {
        (None, None) => true,
        (Some(p), Some(q)) => p.same_as(q),
        _ => false,
    }
}

/// `rte = 0` の `Var`（1 つの表の行に対する式。CHECK・DEFAULT・COPY の列変換・RETURNING）を
/// `PhysCol::Local(col)` に直す。`col` は表のユーザー列の位置（`attnum - 1`）。
///
/// `rte != 0`・`levels_up > 0`・システム列・`SubLink`・`Aggregate`・`SubLinkOutput` を含んだら
/// `Error::internal`（アナライザが先に拒否しているはず）。それ以外は構造をそのまま写す
/// （`ty` と `span` を保つ）。副問い合わせの型 `Q` には依存しない（`BoundExpr` でも呼べる）。
pub fn lower_single_rel<Q>(e: &Expr<Var, Q>) -> Result<Expr<PhysCol, SubPlanId>> {
    e.try_map(&mut |node: &Expr<Var, Q>| match &node.kind {
        ExprKind::Column(v) => {
            if v.rte.0 != 0 || v.levels_up != 0 || v.is_system() {
                return Err(Error::internal(format!(
                    "lower_single_rel: column reference {v:?} is not a user column of the target table"
                )));
            }
            Ok(Some(Expr {
                kind: ExprKind::Column(PhysCol::Local(usize::from(v.col))),
                ty: node.ty,
                span: node.span,
            }))
        }
        ExprKind::Aggregate(_) => Err(Error::internal(
            "lower_single_rel: aggregate in a single-row expression",
        )),
        ExprKind::SubLink { .. } => Err(Error::internal(
            "lower_single_rel: subquery in a single-row expression",
        )),
        ExprKind::SubLinkOutput(_) => Err(Error::internal(
            "lower_single_rel: SubLinkOutput outside a SubLink test",
        )),
        _ => Ok(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{BuiltinAggregate, BuiltinFunction, BuiltinOperator, CastMethod, builtin};
    use crate::error::Span;
    use crate::expr::{BoolTestKind, RteId, SessionValueKind, SubLinkKind};
    use crate::types::{SqlType, oid};

    type EQ = Expr<u32, Vec<u32>>;
    type E = Expr<u32, ()>;
    type K = ExprKind<u32, ()>;

    fn op(name: &str) -> &'static BuiltinOperator {
        builtin::operators_named(name)[0]
    }

    fn func(name: &str) -> &'static BuiltinFunction {
        builtin::functions_named(name)[0]
    }

    static AGG: BuiltinAggregate = BuiltinAggregate {
        oid: 2147,
        name: "count",
        args: &[oid::INT4],
        result: oid::INT8,
        kind: crate::catalog::AggKind::Count,
    };

    fn mk(kind: K) -> E {
        Expr::new(kind, SqlType::INT4, Span::new(1, 2))
    }

    fn col(n: u32) -> E {
        mk(K::Column(n))
    }

    fn lit(n: i32) -> E {
        mk(K::Literal(Datum::Int4(n)))
    }

    #[allow(clippy::unnecessary_box_returns)]
    fn b(e: E) -> Box<E> {
        Box::new(e)
    }

    /// 全変種を 1 つずつ含む式。`Column` は出現順（字面の左から右）に 1, 2, 3, ... を持つ。
    fn full() -> E {
        let eq = op("=");
        mk(K::And(vec![
            mk(K::Operator {
                op: eq,
                args: vec![col(1), col(2)],
            }),
            mk(K::Function {
                func: func("abs"),
                args: vec![col(3)],
            }),
            mk(K::Cast {
                expr: b(col(4)),
                method: CastMethod::Binary,
                implicit: true,
            }),
            mk(K::CoerceTypmod {
                expr: b(col(5)),
                explicit: false,
            }),
            mk(K::Or(vec![col(6), col(7)])),
            mk(K::Not(b(col(8)))),
            mk(K::IsNull(b(col(9)))),
            mk(K::IsNotNull(b(col(10)))),
            mk(K::BoolTest {
                expr: b(col(11)),
                test: BoolTestKind::IsTrue,
            }),
            mk(K::Case {
                arms: vec![(col(12), col(13)), (col(14), col(15))],
                else_result: Some(b(col(16))),
            }),
            mk(K::Coalesce(vec![col(17), col(18)])),
            mk(K::NullIf {
                left: b(col(19)),
                right: b(col(20)),
                eq_op: eq,
            }),
            mk(K::DistinctFrom {
                left: b(col(21)),
                right: b(col(22)),
                eq_op: eq,
                negated: false,
            }),
            mk(K::MinMax {
                greatest: true,
                args: vec![col(23), col(24)],
                cmp: op(">"),
            }),
            mk(K::Like {
                expr: b(col(25)),
                pattern: b(col(26)),
                escape: Some(b(col(27))),
                negated: false,
                case_insensitive: false,
            }),
            mk(K::InList {
                expr: b(col(28)),
                list: vec![col(29), col(30)],
                eq_op: eq,
                negated: false,
            }),
            mk(K::SessionValue(SessionValueKind::CurrentUser)),
            mk(K::Aggregate(Box::new(AggCall {
                func: &AGG,
                args: vec![col(31), col(32)],
                distinct: false,
                order_by: Vec::new(),
                filter: Some(col(33)),
            }))),
            mk(K::SubLink {
                kind: SubLinkKind::Any,
                test: Some(b(mk(K::Operator {
                    op: eq,
                    args: vec![col(34), mk(K::SubLinkOutput(0))],
                }))),
                query: (),
            }),
        ]))
    }

    fn label(e: &E) -> String {
        match &e.kind {
            K::Column(c) => format!("c{c}"),
            K::Literal(Datum::Int4(n)) => format!("l{n}"),
            other => format!("{other:?}")
                .split(|ch: char| !ch.is_alphanumeric())
                .next()
                .unwrap_or("")
                .to_owned(),
        }
    }

    #[test]
    fn walk_visits_preorder_in_child_order() {
        let e = full();
        let mut cols = Vec::new();
        let mut names = Vec::new();
        e.walk(&mut |n| {
            names.push(label(n));
            if let K::Column(c) = &n.kind {
                cols.push(*c);
            }
            true
        });
        assert_eq!(cols, (1..=34).collect::<Vec<u32>>());
        // 親が先（先行順）。
        assert_eq!(names[0], "And");
        assert_eq!(names[1], "Operator");
        assert_eq!(names[2..4], ["c1", "c2"].map(String::from));
        // Aggregate は args、filter の順。SubLink は test に降りる。
        let agg = names.iter().position(|n| n == "Aggregate").unwrap();
        assert_eq!(
            names[agg + 1..agg + 4],
            ["c31", "c32", "c33"].map(String::from)
        );
        let sub = names.iter().position(|n| n == "SubLink").unwrap();
        assert_eq!(
            names[sub + 1..sub + 4],
            ["Operator", "c34", "SubLinkOutput"].map(String::from)
        );
        assert!(e.contains_aggregate());
        assert!(e.contains_sublink());
        assert!(!col(1).contains_aggregate());
        assert_eq!(e.columns().len(), 34);
    }

    #[test]
    fn walk_prune_and_sublink_boundary() {
        // false を返した部分木の子は訪れない（兄弟には進む）。
        let e = mk(K::And(vec![mk(K::Not(b(col(1)))), mk(K::Not(b(col(2))))]));
        let mut seen = Vec::new();
        e.walk(&mut |n| {
            seen.push(label(n));
            !matches!(n.kind, K::Not(_))
        });
        assert_eq!(seen, ["And", "Not", "Not"].map(String::from));

        // SubLink の query には降りない（Q 側に目印を入れても訪問されない）。
        let sub: EQ = Expr::new(
            ExprKind::SubLink {
                kind: SubLinkKind::Exists,
                test: None,
                query: vec![99],
            },
            SqlType::BOOL,
            Span::default(),
        );
        let mut n = 0;
        sub.walk(&mut |_| {
            n += 1;
            true
        });
        assert_eq!(n, 1);
        assert!(sub.columns().is_empty());
    }

    #[test]
    fn try_map_roundtrip_changes_column_type() {
        let e = full();
        let mapped: Expr<String, ()> = e
            .try_map(&mut |n: &E| match &n.kind {
                K::Column(c) => Ok(Some(Expr::new(
                    ExprKind::Column(format!("col{c}")),
                    n.ty,
                    n.span,
                ))),
                K::Aggregate(call) => {
                    let args = call
                        .args
                        .iter()
                        .map(|a| a.try_map(&mut |x: &E| conv_leaf(x)))
                        .collect::<Result<Vec<_>>>()?;
                    let filter = call
                        .filter
                        .as_ref()
                        .map(|a| a.try_map(&mut |x: &E| conv_leaf(x)))
                        .transpose()?;
                    let order_by = call
                        .order_by
                        .iter()
                        .map(|k| {
                            Ok(AggOrderKey {
                                expr: k.expr.try_map(&mut |x: &E| conv_leaf(x))?,
                                descending: k.descending,
                                nulls_first: k.nulls_first,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok(Some(Expr::new(
                        ExprKind::Aggregate(Box::new(AggCall {
                            func: call.func,
                            args,
                            distinct: call.distinct,
                            filter,
                            order_by,
                        })),
                        n.ty,
                        n.span,
                    )))
                }
                K::SubLink { kind, test, .. } => {
                    let test = test
                        .as_ref()
                        .map(|t| t.try_map(&mut |x: &E| conv_leaf(x)).map(Box::new))
                        .transpose()?;
                    Ok(Some(Expr::new(
                        ExprKind::SubLink {
                            kind: *kind,
                            test,
                            query: (),
                        },
                        n.ty,
                        n.span,
                    )))
                }
                _ => Ok(None),
            })
            .unwrap();
        let cols = mapped.columns();
        assert_eq!(cols.len(), 34);
        assert_eq!(cols[0], "col1");
        assert_eq!(cols[33], "col34");
        // 構造・ty・span・非子フィールドが保たれる。
        assert_eq!(mapped.ty, e.ty);
        assert_eq!(mapped.span, Span::new(1, 2));
        let root = &mapped;
        if let ExprKind::And(parts) = &root.kind {
            assert_eq!(parts.len(), 19);
            assert!(matches!(
                &parts[2].kind,
                ExprKind::Cast {
                    method: CastMethod::Binary,
                    implicit: true,
                    ..
                }
            ));
            assert!(matches!(
                &parts[14].kind,
                ExprKind::Like {
                    escape: Some(_),
                    negated: false,
                    case_insensitive: false,
                    ..
                }
            ));
            assert!(matches!(
                &parts[16].kind,
                ExprKind::SessionValue(SessionValueKind::CurrentUser)
            ));
        } else {
            panic!("root must stay an And");
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn conv_leaf(x: &E) -> Result<Option<Expr<String, ()>>> {
        match &x.kind {
            K::Column(c) => Ok(Some(Expr::new(
                ExprKind::Column(format!("col{c}")),
                x.ty,
                x.span,
            ))),
            _ => Ok(None),
        }
    }

    #[test]
    fn try_map_replaces_without_descending() {
        let e = mk(K::Operator {
            op: op("+"),
            args: vec![col(1), mk(K::Not(b(col(2))))],
        });
        let mut visited = Vec::new();
        let out: E = e
            .try_map(&mut |n: &E| {
                visited.push(label(n));
                if matches!(n.kind, K::Not(_)) {
                    return Ok(Some(Expr::new(
                        K::Literal(Datum::Int8(7)),
                        SqlType::INT8,
                        Span::new(9, 9),
                    )));
                }
                if let K::Column(c) = &n.kind {
                    return Ok(Some(col(*c)));
                }
                Ok(None)
            })
            .unwrap();
        // Not の子 c2 には降りない。
        assert_eq!(visited, ["Operator", "c1", "Not"].map(String::from));
        if let K::Operator { args, .. } = &out.kind {
            assert_eq!(args[1].ty, SqlType::INT8);
            assert_eq!(args[1].span, Span::new(9, 9));
        } else {
            panic!();
        }
    }

    #[test]
    fn try_map_leaf_not_converted_is_internal_error() {
        let agg = mk(K::Aggregate(Box::new(AggCall {
            func: &AGG,
            args: vec![],
            distinct: false,
            order_by: Vec::new(),
            filter: None,
        })));
        let sub = mk(K::SubLink {
            kind: SubLinkKind::Exists,
            test: None,
            query: (),
        });
        for (e, name) in [(col(1), "Column"), (agg, "Aggregate"), (sub, "SubLink")] {
            let r: Result<Expr<u8, ()>> = e.try_map(&mut |_| Ok(None));
            let err = r.unwrap_err();
            assert_eq!(err.sqlstate.code(), "XX000");
            assert!(err.message.contains(name), "{}", err.message);
        }
        // 葉が深い所にあっても検出する。
        let nested = mk(K::Not(b(col(1))));
        let r: Result<Expr<u8, ()>> = nested.try_map(&mut |_| Ok(None));
        assert!(r.is_err());
    }

    #[test]
    fn try_map_copies_type_independent_leaves() {
        for e in [
            lit(5),
            mk(K::SessionValue(SessionValueKind::CurrentSchema)),
            mk(K::SubLinkOutput(3)),
        ] {
            let out: Expr<u8, u8> = e.try_map(&mut |_| Ok(None)).unwrap();
            assert_eq!(format!("{:?}", out.kind), format!("{:?}", e.kind));
            assert_eq!(out.span, e.span);
        }
    }

    #[test]
    fn try_rewrite_keeps_leaves_and_descends_test() {
        let c = |n: u32| -> EQ { Expr::column(n, SqlType::INT4) };
        let agg: EQ = Expr::new(
            ExprKind::Aggregate(Box::new(AggCall {
                func: &AGG,
                args: vec![c(1)],
                distinct: true,
                order_by: Vec::new(),
                filter: Some(c(2)),
            })),
            SqlType::INT8,
            Span::default(),
        );
        let sub: EQ = Expr::new(
            ExprKind::SubLink {
                kind: SubLinkKind::Any,
                test: Some(Box::new(c(3))),
                query: vec![42],
            },
            SqlType::BOOL,
            Span::default(),
        );
        let root = Expr::new(
            ExprKind::And(vec![agg, sub, c(4)]),
            SqlType::BOOL,
            Span::default(),
        );
        // f が None を返した葉は clone。Column を 10 倍にする書き換えが Aggregate の中と SubLink の test に届く。
        let out = root
            .try_rewrite(&mut |n| match &n.kind {
                ExprKind::Column(x) => Ok(Some(c(*x * 10))),
                _ => Ok(None),
            })
            .unwrap();
        assert_eq!(out.columns(), vec![10, 20, 30, 40]);
        // query は clone。
        if let ExprKind::And(p) = &out.kind {
            match &p[1].kind {
                ExprKind::SubLink { query, kind, .. } => {
                    assert_eq!(query, &vec![42]);
                    assert_eq!(*kind, SubLinkKind::Any);
                }
                _ => panic!(),
            }
            match &p[0].kind {
                ExprKind::Aggregate(call) => assert!(call.distinct),
                _ => panic!(),
            }
        }
        // 何も書き換えなければ同じ構造。
        let same = root.try_rewrite(&mut |_| Ok(None)).unwrap();
        assert!(same.same_as(&root) || same.columns() == root.columns());
        assert_eq!(same.columns(), root.columns());
    }

    #[test]
    fn same_as_ignores_spans_and_compares_float_bits() {
        let f =
            |v: f64, s: Span| -> E { Expr::new(K::Literal(Datum::Float8(v)), SqlType::FLOAT8, s) };
        assert!(f(1.5, Span::new(0, 1)).same_as(&f(1.5, Span::new(7, 9))));
        assert!(!f(0.0, Span::default()).same_as(&f(-0.0, Span::default())));
        assert!(f(f64::NAN, Span::default()).same_as(&f(f64::NAN, Span::default())));
        // 型が違えば不一致。
        assert!(!lit(1).same_as(&Expr::new(
            K::Literal(Datum::Int4(1)),
            SqlType::INT8,
            Span::default()
        )));
        // implicit は無視する。
        let cast = |implicit| {
            mk(K::Cast {
                expr: b(col(1)),
                method: CastMethod::Binary,
                implicit,
            })
        };
        assert!(cast(true).same_as(&cast(false)));
        // Operator は OID で比べる。
        let plus = |o: &'static BuiltinOperator| {
            mk(K::Operator {
                op: o,
                args: vec![col(1), col(2)],
            })
        };
        assert!(plus(op("+")).same_as(&plus(op("+"))));
        assert!(!plus(op("+")).same_as(&plus(op("-"))));
        // Aggregate。
        let agg = |d: bool| {
            mk(K::Aggregate(Box::new(AggCall {
                func: &AGG,
                args: vec![col(1)],
                distinct: d,
                order_by: Vec::new(),
                filter: None,
            })))
        };
        assert!(agg(false).same_as(&agg(false)));
        assert!(!agg(false).same_as(&agg(true)));
        // SubLink は常に不一致（自分自身とも）。
        let sub = mk(K::SubLink {
            kind: SubLinkKind::Exists,
            test: None,
            query: (),
        });
        assert!(!sub.same_as(&sub));
        // 全変種を含む式は、自分自身と一致しない（SubLink を含むため）が、SubLink を除けば一致する。
        let mut no_sub = full();
        if let K::And(p) = &mut no_sub.kind {
            p.pop();
        }
        assert!(no_sub.same_as(&no_sub.clone()));
    }

    #[test]
    fn and_all_and_conjuncts() {
        assert!(matches!(
            Expr::<u32, ()>::and_all(vec![]).kind,
            ExprKind::Literal(Datum::Bool(true))
        ));
        let one = E::and_all(vec![col(1)]);
        assert!(matches!(one.kind, ExprKind::Column(1)));
        let nested = E::and_all(vec![
            mk(K::And(vec![col(1), col(2)])),
            col(3),
            mk(K::And(vec![mk(K::And(vec![col(4), col(5)])), col(6)])),
        ]);
        let ExprKind::And(parts) = &nested.kind else {
            panic!("expected And")
        };
        assert_eq!(parts.len(), 5); // 直下の And は平らになる（孫は conjuncts が平らにする）
        let order: Vec<u32> = nested
            .conjuncts()
            .iter()
            .map(|e| match e.kind {
                K::Column(c) => c,
                _ => 0,
            })
            .collect();
        assert_eq!(order, [1, 2, 3, 4, 5, 6]);
        assert_eq!(col(7).conjuncts().len(), 1);
        assert_eq!(nested.ty, SqlType::BOOL);
        assert_eq!(E::bool_lit(false).ty, SqlType::BOOL);
        assert!(matches!(
            E::null_of(SqlType::TEXT).kind,
            K::Literal(Datum::Null)
        ));
    }

    type BE = Expr<Var, ()>;

    fn var(rte: u16, col: u16, levels_up: u16) -> BE {
        Expr::column(
            Var {
                rte: RteId(rte),
                col,
                levels_up,
            },
            SqlType::INT4,
        )
    }

    #[test]
    fn lower_single_rel_cases() {
        // 正常: rte 0 のユーザー列は Local(col)。構造・ty・span を保つ。
        let ok = Expr::new(
            ExprKind::Operator {
                op: op("+"),
                args: vec![var(0, 2, 0), Expr::literal(Datum::Int4(1), SqlType::INT4)],
            },
            SqlType::INT8,
            Span::new(3, 4),
        );
        let lowered = lower_single_rel(&ok).unwrap();
        assert_eq!(lowered.ty, SqlType::INT8);
        assert_eq!(lowered.span, Span::new(3, 4));
        assert_eq!(lowered.columns(), vec![PhysCol::Local(2)]);

        // rte != 0、levels_up > 0、システム列、SubLink、Aggregate、SubLinkOutput は XX000。
        let sys = Expr::column(
            Var::system(RteId(0), crate::catalog::SystemColumn::Ctid),
            SqlType::INT4,
        );
        let sub: BE = Expr::new(
            ExprKind::SubLink {
                kind: SubLinkKind::Exists,
                test: None,
                query: (),
            },
            SqlType::BOOL,
            Span::default(),
        );
        let agg: BE = Expr::new(
            ExprKind::Aggregate(Box::new(AggCall {
                func: &AGG,
                args: vec![],
                distinct: false,
                order_by: Vec::new(),
                filter: None,
            })),
            SqlType::INT8,
            Span::default(),
        );
        let out: BE = Expr::new(ExprKind::SubLinkOutput(0), SqlType::INT4, Span::default());
        for e in [var(1, 0, 0), var(0, 0, 1), sys, sub, agg, out] {
            let err = lower_single_rel(&e).unwrap_err();
            assert_eq!(err.sqlstate.code(), "XX000");
        }
    }

    #[test]
    fn var_helpers() {
        let v = Var::user(RteId(1), 3);
        assert!(v.is_local() && !v.is_system());
        assert_eq!(v.system_column(), None);
        assert_eq!(v.with_levels_up(2).levels_up, 2);
        for sc in [
            crate::catalog::SystemColumn::Ctid,
            crate::catalog::SystemColumn::Xmin,
            crate::catalog::SystemColumn::Cmin,
            crate::catalog::SystemColumn::Xmax,
            crate::catalog::SystemColumn::Cmax,
            crate::catalog::SystemColumn::TableOid,
        ] {
            let s = Var::system(RteId(0), sc);
            assert!(s.is_system());
            assert_eq!(s.system_column(), Some(sc));
        }
    }
}
