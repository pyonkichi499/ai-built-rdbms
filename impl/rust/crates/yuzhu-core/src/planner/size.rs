//! サイズの手がかり（`m4/04` §3.6。持ち主は L2）。
//!
//! 統計がない M4 の結合の向き（ハッシュのビルド側）と、内側 Index Scan の採否にだけ使う「ブロック相当の重み」。
//! 規則は 04-D8 の表。重みの絶対値に意味はなく、比べるためだけに使う。

use std::cell::RefCell;
use std::collections::HashMap;

use super::PlanEnv;
use super::index_select;
use super::logical::{JoinKind, LExpr, LogicalPlan};
use super::util::{conjuncts, expr_refs};
use crate::error::Result;
use crate::expr::{CteId, ExprKind};
use crate::storage::RelHandle;
use crate::types::{Datum, Oid};

/// 1 ブロックの行数の見立て（INL の採否にだけ使う）。
pub const ROWS_PER_BLOCK_EST: f64 = 100.0;
/// 等値以外の述語 1 つの係数。
pub const FILTER_FACTOR: f64 = 1.0 / 3.0;
/// `col = 定数式` 1 つの係数。
pub const EQ_FACTOR: f64 = 0.1;
/// 述語が多くても係数の積はこれより小さくしない。
pub const MIN_FILTER_FACTOR: f64 = 1.0 / 27.0;
/// 一意インデックスの全列等値（1 行）。
pub const POINT_LOOKUP_WEIGHT: f64 = 0.01;
pub const MIN_WEIGHT: f64 = 0.01;

#[derive(Debug)]
pub struct SizeCtx<'a> {
    env: &'a PlanEnv<'a>,
    nblocks: RefCell<HashMap<Oid, u32>>,
    /// 共有 CTE の重み（`physicalize` が CTE を物理化するときに登録する）。
    ctes: RefCell<HashMap<CteId, f64>>,
}

impl<'a> SizeCtx<'a> {
    pub fn new(env: &'a PlanEnv<'a>) -> Self {
        SizeCtx {
            env,
            nblocks: RefCell::new(HashMap::new()),
            ctes: RefCell::new(HashMap::new()),
        }
    }

    /// 共有 CTE の計画の重みを登録する（`CteScan` の重みになる）。
    pub fn register_cte(&self, cte: CteId, weight: f64) {
        self.ctes.borrow_mut().insert(cte, weight);
    }

    /// 「ブロック相当の重み」。`nblocks` の失敗（storage のエラー）は伝える。
    pub fn estimate(&self, p: &LogicalPlan) -> Result<f64> {
        use LogicalPlan as L;
        Ok(match p {
            L::Get { rel, .. } => f64::from(self.nblocks(rel)?.max(1)),
            L::Filter { input, predicate } => {
                if let L::Get { table, cols, .. } = &**input {
                    let conj = conjuncts(predicate.clone());
                    if index_select::is_point_lookup(table, cols, &conj) {
                        return Ok(POINT_LOOKUP_WEIGHT);
                    }
                }
                self.estimate(input)? * filter_factor(predicate)
            }
            L::Project { input, .. }
            | L::Sort { input, .. }
            | L::Distinct { input, .. }
            | L::Insert { input, .. }
            | L::Update { input, .. }
            | L::Delete { input, .. } => self.estimate(input)?,
            L::CteScan { cte, .. } => self.ctes.borrow().get(cte).copied().unwrap_or(1.0),
            L::Limit { input, limit, .. } => {
                let w = self.estimate(input)?;
                match limit.as_ref().and_then(const_count) {
                    Some(n) => w.min((n / ROWS_PER_BLOCK_EST).max(MIN_WEIGHT)),
                    None => w,
                }
            }
            L::Aggregate {
                input, group_by, ..
            } => {
                if group_by.is_empty() {
                    MIN_WEIGHT
                } else {
                    self.estimate(input)? * 0.5
                }
            }
            L::Join {
                kind,
                left,
                right,
                on,
            } => {
                let (l, r) = (self.estimate(left)?, self.estimate(right)?);
                match kind {
                    JoinKind::Inner if on.is_none() => l * r,
                    JoinKind::Inner => l.max(r),
                    JoinKind::Left => l,
                    JoinKind::Full => l + r,
                    JoinKind::Semi | JoinKind::Anti => l * 0.5,
                }
            }
            L::SetOp {
                op, left, right, ..
            } => {
                use crate::analyzer::bound::SetOpKind;
                let (l, r) = (self.estimate(left)?, self.estimate(right)?);
                match op {
                    SetOpKind::Union => l + r,
                    SetOpKind::Intersect => l.min(r),
                    SetOpKind::Except => l,
                }
            }
            #[allow(clippy::cast_precision_loss)]
            L::Values { rows, .. } => (rows.len() as f64 / ROWS_PER_BLOCK_EST).max(MIN_WEIGHT),
            L::FunctionScan { func, args, .. } => match (func.name, args.as_slice()) {
                ("generate_series", [a, b, ..]) => match (const_count(a), const_count(b)) {
                    (Some(a), Some(b)) => ((b - a + 1.0) / ROWS_PER_BLOCK_EST).max(MIN_WEIGHT),
                    _ => 1.0,
                },
                _ => 1.0,
            },
            L::Result { .. } | L::Empty { .. } => MIN_WEIGHT,
        })
    }

    /// キャッシュつきのブロック数。0 ブロックの表は呼び出し側が `max(.., 1)` とする。
    pub fn nblocks(&self, rel: &RelHandle) -> Result<u32> {
        if let Some(n) = self.nblocks.borrow().get(&rel.oid) {
            return Ok(*n);
        }
        let n = self.env.storage.nblocks(rel)?;
        self.nblocks.borrow_mut().insert(rel.oid, n);
        Ok(n)
    }
}

/// 整数の定数式の値。
fn const_count(e: &LExpr) -> Option<f64> {
    match &e.kind {
        ExprKind::Literal(Datum::Int2(v)) => Some(f64::from(*v)),
        ExprKind::Literal(Datum::Int4(v)) => Some(f64::from(*v)),
        #[allow(clippy::cast_precision_loss)]
        ExprKind::Literal(Datum::Int8(v)) => Some(*v as f64),
        _ => None,
    }
}

/// 述語の conjunct ごとの係数の積（下限 `MIN_FILTER_FACTOR`）。
fn filter_factor(predicate: &LExpr) -> f64 {
    let mut f = 1.0;
    for c in conjuncts(predicate.clone()) {
        f *= if is_col_eq_const(&c) {
            EQ_FACTOR
        } else {
            FILTER_FACTOR
        };
    }
    f.max(MIN_FILTER_FACTOR)
}

/// `col = 定数式`（どちらの向きでも。定数式は列を含まない）。
fn is_col_eq_const(e: &LExpr) -> bool {
    let ExprKind::Operator { op, args } = &e.kind else {
        return false;
    };
    if op.name != "=" {
        return false;
    }
    let [a, b] = args.as_slice() else {
        return false;
    };
    let is_col = |x: &LExpr| matches!(x.kind, ExprKind::Column(_));
    (is_col(a) && expr_refs(b).is_empty()) || (is_col(b) && expr_refs(a).is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::TableBuilder;
    use crate::error::Span;
    use crate::executor::nodes::test_util::Fixture;
    use crate::expr::ColId;
    use crate::planner::PlannerSettings;
    use crate::planner::logical::{ColumnArena, ColumnInfo};
    use crate::types::{SqlType, TypeEnv};
    use std::sync::Arc;

    #[allow(clippy::struct_field_names)]
    struct Env {
        f: Fixture,
        settings: PlannerSettings,
        type_env: TypeEnv<'static>,
    }

    impl Env {
        fn new() -> Self {
            Env {
                f: Fixture::new(),
                settings: PlannerSettings::default(),
                type_env: TypeEnv::default(),
            }
        }

        fn env(&self) -> PlanEnv<'_> {
            PlanEnv {
                catalog: &self.f.catalog,
                storage: &self.f.storage,
                settings: &self.settings,
                type_env: &self.type_env,
                want_explain: false,
                explain_verbose: false,
            }
        }
    }

    fn get(e: &mut Env, arena: &mut ColumnArena, name: &str, rows: usize) -> LogicalPlan {
        let t = e.f.catalog.add(
            &TableBuilder::new(name)
                .column_nn("a", SqlType::INT4)
                .column("b", SqlType::INT4)
                .primary_key(&["a"]),
        );
        for i in 0..rows {
            e.f.storage.add_row(
                t.oid,
                vec![Datum::Int4(i32::try_from(i).unwrap()), Datum::Int4(0)],
            );
        }
        let cols = t
            .columns
            .iter()
            .map(|c| {
                arena.add(ColumnInfo {
                    name: c.name.clone(),
                    qualifier: Some(name.to_owned()),
                    ty: c.ty,
                    origin: Some((t.oid, c.attnum)),
                })
            })
            .collect();
        LogicalPlan::Get {
            rel: RelHandle::from_table(&t),
            table: Arc::clone(&t),
            alias: None,
            cols,
            system_columns: vec![],
        }
    }

    fn eq(a: LExpr, b: LExpr) -> LExpr {
        let op = crate::catalog::builtin::operators_named("=")
            .into_iter()
            .find(|o| o.left == Some(SqlType::INT4.oid) && o.right == SqlType::INT4.oid)
            .unwrap();
        LExpr::new(
            ExprKind::Operator {
                op,
                args: vec![a, b],
            },
            SqlType::BOOL,
            Span::default(),
        )
    }

    fn lt(a: LExpr, b: LExpr) -> LExpr {
        let op = crate::catalog::builtin::operators_named("<")
            .into_iter()
            .find(|o| o.left == Some(SqlType::INT4.oid) && o.right == SqlType::INT4.oid)
            .unwrap();
        LExpr::new(
            ExprKind::Operator {
                op,
                args: vec![a, b],
            },
            SqlType::BOOL,
            Span::default(),
        )
    }

    fn int(v: i32) -> LExpr {
        LExpr::literal(Datum::Int4(v), SqlType::INT4)
    }

    fn col(p: &LogicalPlan, i: usize) -> LExpr {
        LExpr::column(p.output_cols()[i], SqlType::INT4)
    }

    #[allow(clippy::many_single_char_names)]
    #[test]
    fn get_filter_limit_and_joins() {
        let mut e = Env::new();
        let mut arena = ColumnArena::default();
        let t = get(&mut e, &mut arena, "t", 3);
        let u = get(&mut e, &mut arena, "u", 0);
        let env = e.env();
        let s = SizeCtx::new(&env);
        // 行がある表は 1 ブロック、空の表も 1（`max(.., 1)`）。
        assert_eq!(s.estimate(&t).unwrap(), 1.0);
        assert_eq!(s.estimate(&u).unwrap(), 1.0);
        // b = 1 は EQ_FACTOR、b < 1 は FILTER_FACTOR、積には下限。
        let f = LogicalPlan::Filter {
            input: Box::new(t.clone()),
            predicate: eq(col(&t, 1), int(1)),
        };
        assert!((s.estimate(&f).unwrap() - EQ_FACTOR).abs() < 1e-9);
        let f = LogicalPlan::Filter {
            input: Box::new(t.clone()),
            predicate: LExpr::and_all(vec![
                lt(col(&t, 1), int(1)),
                lt(col(&t, 1), int(2)),
                lt(col(&t, 1), int(3)),
                lt(col(&t, 1), int(4)),
            ]),
        };
        assert!((s.estimate(&f).unwrap() - MIN_FILTER_FACTOR).abs() < 1e-9);
        // 主キーの等値は 1 行。
        let f = LogicalPlan::Filter {
            input: Box::new(t.clone()),
            predicate: eq(col(&t, 0), int(1)),
        };
        assert_eq!(s.estimate(&f).unwrap(), POINT_LOOKUP_WEIGHT);
        // LIMIT 10 は max(10 / 100, 0.01) = 0.1 で頭打ち。
        let l = LogicalPlan::Limit {
            input: Box::new(t.clone()),
            limit: Some(LExpr::literal(Datum::Int8(10), SqlType::INT8)),
            offset: None,
        };
        assert!((s.estimate(&l).unwrap() - 0.1).abs() < 1e-9);
        // 結合。
        let j = |kind, on| LogicalPlan::Join {
            kind,
            left: Box::new(t.clone()),
            right: Box::new(u.clone()),
            on,
        };
        assert_eq!(s.estimate(&j(JoinKind::Inner, None)).unwrap(), 1.0);
        assert_eq!(
            s.estimate(&j(JoinKind::Full, Some(LExpr::bool_lit(true))))
                .unwrap(),
            2.0
        );
        assert_eq!(
            s.estimate(&j(JoinKind::Semi, Some(LExpr::bool_lit(true))))
                .unwrap(),
            0.5
        );
        // 集約。
        let a = LogicalPlan::Aggregate {
            input: Box::new(t.clone()),
            group_by: vec![],
            aggs: vec![],
        };
        assert_eq!(s.estimate(&a).unwrap(), MIN_WEIGHT);
        let _ = ColId(0);
    }

    #[test]
    fn nblocks_are_cached_and_cte_weights_registered() {
        let mut e = Env::new();
        let mut arena = ColumnArena::default();
        let t = get(&mut e, &mut arena, "t", 2);
        let env = e.env();
        let s = SizeCtx::new(&env);
        let LogicalPlan::Get { rel, .. } = &t else {
            unreachable!()
        };
        assert_eq!(s.nblocks(rel).unwrap(), 1);
        let scan = LogicalPlan::CteScan {
            cte: CteId(0),
            alias: None,
            cols: vec![],
        };
        assert_eq!(s.estimate(&scan).unwrap(), 1.0);
        s.register_cte(CteId(0), 7.0);
        assert_eq!(s.estimate(&scan).unwrap(), 7.0);
    }
}
