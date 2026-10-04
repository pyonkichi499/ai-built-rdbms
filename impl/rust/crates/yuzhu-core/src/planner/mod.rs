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

pub use plan::*;

use crate::analyzer::{
    BoundExpr, BoundExprKind, BoundFrom, BoundInsert, BoundSelect, BoundStatement,
};
use crate::error::{Error, Result};
use crate::storage::RelHandle;

/// Plans a SELECT or INSERT. DDL statements are not planned.
pub fn plan(stmt: &BoundStatement) -> Result<PhysicalPlan> {
    match stmt {
        BoundStatement::Select(s) => Ok(plan_select(s)),
        BoundStatement::Insert(i) => Ok(plan_insert(i)),
        BoundStatement::Update(_) | BoundStatement::Delete(_) => Err(Error::not_supported(
            "UPDATE and DELETE planning is not implemented yet",
        )),
        BoundStatement::CreateTable(_)
        | BoundStatement::DropTable(_)
        | BoundStatement::Checkpoint => Err(Error::internal(
            "utility statements are executed by the session, not planned",
        )),
    }
}

fn column_ref(index: usize, src: &BoundExpr) -> BoundExpr {
    BoundExpr::new(BoundExprKind::ColumnRef { index }, src.ty, src.span)
}

/// Plans a SELECT (or bare VALUES).
pub fn plan_select(s: &BoundSelect) -> PhysicalPlan {
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

    if !s.order_by.is_empty() {
        let keys = s
            .order_by
            .iter()
            .map(|k| SortKey {
                expr: column_ref(k.target, &s.targets[k.target]),
                descending: k.descending,
                nulls_first: k.nulls_first,
            })
            .collect();
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
    PhysicalPlan::Insert {
        rel: RelHandle::from_table(&i.table),
        input: Box::new(plan_select(&i.source)),
        column_map: i.column_map.clone(),
        defaults: i.defaults.clone(),
        checks,
        not_null: i.table.columns.iter().map(|c| c.not_null).collect(),
        table_name: i.table.name.clone(),
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
