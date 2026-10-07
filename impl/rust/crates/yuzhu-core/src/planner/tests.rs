//! planner の単体テスト（`m4/02` §5.2 P0-e の 7、§6.3）。
//!
//! 旧 `planner/mod.rs` のテストを新しい経路（`build` → `rules` → `physicalize` → `build_query`）に移したもの
//! （テスト名は維持する。§5.3）。`PlannerSettings::validate_plans` を真にして、すべてのテストで
//! `LogicalQuery::validate` と `PhysicalQuery::validate` を走らせる。

use std::sync::Arc;

use super::physical::{PhysicalPlan, PhysicalQuery};
use super::{PlanEnv, PlannerSettings, plan};
use crate::analyzer::bound::{
    BoundCheck, BoundDelete, BoundDistinct, BoundExpr, BoundInsert, BoundQuery, BoundSelect,
    BoundSetExpr, BoundSortKey, BoundStatement, BoundUpdate, FromItem, OutputColumn, Rte,
    RteColumn, RteKind, UpdateSource,
};
use crate::catalog::fake::table_def;
use crate::catalog::{ColumnDef, SystemColumn, TableDef};
use crate::error::{Result, Span, sqlstate};
use crate::executor::eval::tests::GT;
use crate::executor::nodes::test_util::Fixture;
use crate::expr::{ExprKind, PhysCol, RteId, Var};
use crate::storage::RelHandle;
use crate::types::{Datum, Row, SqlType, TypeEnv, oid};

// ----- 式・Bound の組み立て -----------------------------------------------------------------

fn lit(d: Datum, ty: SqlType) -> BoundExpr {
    BoundExpr::literal(d, ty)
}

fn int(v: i32) -> BoundExpr {
    lit(Datum::Int4(v), SqlType::INT4)
}

fn text(s: &str) -> BoundExpr {
    lit(Datum::Text(s.into()), SqlType::TEXT)
}

fn null(ty: SqlType) -> BoundExpr {
    lit(Datum::Null, ty)
}

/// 対象表（`RteId(0)`）の `col` 番目の列。
fn col(col: u16, ty: SqlType) -> BoundExpr {
    BoundExpr::column(Var::user(RteId(0), col), ty)
}

fn sys_col(sc: SystemColumn, ty: SqlType) -> BoundExpr {
    BoundExpr::column(Var::system(RteId(0), sc), ty)
}

fn op(o: &'static crate::catalog::BuiltinOperator, l: BoundExpr, r: BoundExpr) -> BoundExpr {
    BoundExpr::new(
        ExprKind::Operator {
            op: o,
            args: vec![l, r],
        },
        SqlType::of(o.result),
        Span::default(),
    )
}

fn out(name: &str, ty: SqlType) -> OutputColumn {
    OutputColumn {
        name: name.into(),
        ty,
        table_oid: 0,
        attnum: 0,
    }
}

fn table_rte(table: &Arc<TableDef>) -> Rte {
    Rte {
        kind: RteKind::Table {
            table: Arc::clone(table),
        },
        refname: Some(table.name.clone()),
        columns: table
            .columns
            .iter()
            .map(|c| RteColumn {
                name: c.name.clone(),
                ty: c.ty,
            })
            .collect(),
        span: Span::default(),
    }
}

/// 単一の SELECT 本体（FROM が空なら `rtable` も空）。
fn select_body(
    rtable: Vec<Rte>,
    targets: Vec<BoundExpr>,
    n_visible: usize,
    filter: Option<BoundExpr>,
) -> BoundSelect {
    let from = if rtable.is_empty() {
        vec![]
    } else {
        vec![FromItem::Scan(RteId(0))]
    };
    BoundSelect {
        rtable,
        from,
        filter,
        group_by: vec![],
        having: None,
        has_agg: false,
        targets,
        n_visible,
        distinct: BoundDistinct::None,
    }
}

fn query_of(body: BoundSelect) -> BoundQuery {
    let columns = body.targets[..body.n_visible]
        .iter()
        .enumerate()
        .map(|(i, t)| out(&format!("c{i}"), t.ty))
        .collect();
    BoundQuery {
        ctes: vec![],
        body: BoundSetExpr::Select(Box::new(body)),
        order_by: vec![],
        limit: None,
        offset: None,
        columns,
    }
}

fn select(rtable: Vec<Rte>, targets: Vec<BoundExpr>, visible: usize) -> BoundQuery {
    query_of(select_body(rtable, targets, visible, None))
}

fn table() -> Arc<TableDef> {
    let c = |name: &str, attnum, ty| ColumnDef {
        name: name.into(),
        attnum,
        ty,
        not_null: attnum == 1,
        default: None,
        identity: None,
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

// ----- 計画と実行 --------------------------------------------------------------------------

/// `validate_plans` を真にして計画する。
fn plan_in(f: &Fixture, stmt: &BoundStatement) -> Result<PhysicalQuery> {
    let settings = PlannerSettings {
        validate_plans: true,
        ..PlannerSettings::default()
    };
    let type_env = TypeEnv::default();
    let env = PlanEnv {
        catalog: &f.catalog,
        storage: &f.storage,
        settings: &settings,
        type_env: &type_env,
        want_explain: false,
        explain_verbose: false,
    };
    plan(stmt, &env)
}

/// 計画して実行する。実行した行数（`rows_affected`）も返す。
fn run_plan(f: &mut Fixture, stmt: &BoundStatement) -> Result<(Vec<Row>, u64)> {
    f.query = plan_in(f, stmt)?;
    let mut e = crate::executor::build_query(&f.query);
    let rows = f.run(&mut e)?;
    Ok((rows, e.rows_affected()))
}

fn run(f: &mut Fixture, q: BoundQuery) -> Vec<Row> {
    run_plan(f, &BoundStatement::Select(Box::new(q))).unwrap().0
}

fn root(f: &Fixture, stmt: &BoundStatement) -> PhysicalPlan {
    plan_in(f, stmt).unwrap().root
}

// ----- SELECT ------------------------------------------------------------------------------

#[test]
fn fromless_select_is_a_result_node() {
    let q = select(vec![], vec![int(1), text("x")], 2);
    let mut f = Fixture::new();
    assert!(matches!(
        root(&f, &BoundStatement::Select(Box::new(q.clone()))),
        PhysicalPlan::Result { .. }
    ));
    assert_eq!(
        run(&mut f, q),
        vec![vec![Datum::Int4(1), Datum::Text("x".into())]]
    );
    // SELECT 1 WHERE false
    let q = query_of(select_body(
        vec![],
        vec![int(1)],
        1,
        Some(lit(Datum::Bool(false), SqlType::BOOL)),
    ));
    assert!(run(&mut f, q).is_empty());
}

#[test]
fn select_star_skips_projection() {
    let q = select(
        vec![table_rte(&table())],
        vec![col(0, SqlType::INT4), col(1, SqlType::TEXT)],
        2,
    );
    let mut f = fixture_with_rows(&[(1, "a"), (2, "b")]);
    assert!(matches!(
        root(&f, &BoundStatement::Select(Box::new(q.clone()))),
        PhysicalPlan::SeqScan { .. }
    ));
    assert_eq!(run(&mut f, q).len(), 2);
}

#[test]
fn filter_order_by_resjunk_limit() {
    // SELECT b FROM t WHERE a > 1 ORDER BY a DESC LIMIT 2 OFFSET 1
    let mut q = query_of(select_body(
        vec![table_rte(&table())],
        vec![col(1, SqlType::TEXT), col(0, SqlType::INT4)],
        1,
        Some(op(&GT, col(0, SqlType::INT4), int(1))),
    ));
    q.order_by = vec![BoundSortKey {
        target: 1,
        descending: true,
        nulls_first: true,
    }];
    q.limit = Some(lit(Datum::Int8(2), SqlType::INT8));
    q.offset = Some(lit(Datum::Int8(1), SqlType::INT8));
    let mut f = fixture_with_rows(&[(3, "c"), (1, "a"), (5, "e"), (4, "d"), (2, "b")]);
    assert_eq!(
        run(&mut f, q),
        vec![vec![Datum::Text("d".into())], vec![Datum::Text("c".into())]]
    );
}

#[test]
fn distinct_after_sort_and_values() {
    // SELECT DISTINCT column1 FROM (VALUES (2),(1),(2),(3)) ORDER BY 1
    let rows = vec![vec![int(2)], vec![int(1)], vec![int(2)], vec![int(3)]];
    let values = Rte {
        kind: RteKind::Values { rows },
        refname: None,
        columns: vec![RteColumn {
            name: "column1".into(),
            ty: SqlType::INT4,
        }],
        span: Span::default(),
    };
    let mut body = select_body(vec![values], vec![col(0, SqlType::INT4)], 1, None);
    body.distinct = BoundDistinct::All;
    let mut q = query_of(body);
    q.order_by = vec![BoundSortKey {
        target: 0,
        descending: false,
        nulls_first: false,
    }];
    let mut f = Fixture::new();
    assert_eq!(
        run(&mut f, q),
        vec![
            vec![Datum::Int4(1)],
            vec![Datum::Int4(2)],
            vec![Datum::Int4(3)]
        ]
    );
}

#[test]
fn values_body_with_order_by() {
    // VALUES (2), (1) ORDER BY 1 LIMIT 1
    let mut q = BoundQuery {
        ctes: vec![],
        body: BoundSetExpr::Values {
            rows: vec![vec![int(2)], vec![int(1)]],
            types: vec![SqlType::INT4],
        },
        order_by: vec![BoundSortKey {
            target: 0,
            descending: false,
            nulls_first: false,
        }],
        limit: Some(lit(Datum::Int8(1), SqlType::INT8)),
        offset: None,
        columns: vec![out("column1", SqlType::INT4)],
    };
    let mut f = Fixture::new();
    assert_eq!(run(&mut f, q.clone()), vec![vec![Datum::Int4(1)]]);
    // 定数のソートキーは捨てる（`Sort` が無い）。
    q.body = BoundSetExpr::Select(Box::new(select_body(vec![], vec![int(1)], 1, None)));
    q.columns = vec![out("c0", SqlType::INT4)];
    let p = root(&f, &BoundStatement::Select(Box::new(q)));
    assert!(matches!(p, PhysicalPlan::Limit { .. }));
}

#[test]
fn aggregate_bodies_are_planned() {
    let f = Fixture::new();
    let mut body = select_body(
        vec![table_rte(&table())],
        vec![col(0, SqlType::INT4)],
        1,
        None,
    );
    body.has_agg = true;
    let p = plan_in(&f, &BoundStatement::Select(Box::new(query_of(body))));
    assert!(p.is_ok(), "{:?}", p.err());
}

// ----- INSERT ------------------------------------------------------------------------------

fn check(name: &str) -> BoundCheck {
    BoundCheck {
        name: name.into(),
        expr: op(&GT, col(0, SqlType::INT4), int(0)),
    }
}

#[test]
fn insert_plan_and_execution() {
    let t = table();
    let ins = BoundInsert {
        table: t.clone(),
        source: Box::new(BoundQuery {
            ctes: vec![],
            body: BoundSetExpr::Values {
                rows: vec![vec![int(1)], vec![int(2)]],
                types: vec![SqlType::INT4],
            },
            order_by: vec![],
            limit: None,
            offset: None,
            columns: vec![out("column1", SqlType::INT4)],
        }),
        coercions: None,
        column_map: vec![Some(0), None],
        defaults: vec![None, Some(text("dflt"))],
        checks: vec![check("zz"), check("aa")],
        overriding: None,
        returning: None,
    };
    let stmt = BoundStatement::Insert(ins);
    let mut f = fixture_with_rows(&[]);
    let p = root(&f, &stmt);
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
    let (rows, n) = run_plan(&mut f, &stmt).unwrap();
    assert!(rows.is_empty());
    assert_eq!(n, 2);
    assert_eq!(
        f.storage.rows(t.oid)[1],
        vec![Datum::Int4(2), Datum::Text("dflt".into())]
    );
}

#[test]
fn insert_coercions_project_over_the_source() {
    // INSERT INTO t (b) SELECT a FROM t ... ORDER BY 1: 型変換は `Sort` の上。
    let t = table();
    let src_table = table();
    let mut src = query_of(select_body(
        vec![table_rte(&src_table)],
        vec![col(0, SqlType::INT4)],
        1,
        None,
    ));
    src.order_by = vec![BoundSortKey {
        target: 0,
        descending: false,
        nulls_first: false,
    }];
    let ins = BoundInsert {
        table: t.clone(),
        source: Box::new(src),
        coercions: Some(vec![BoundExpr::column(
            Var::user(RteId(0), 0),
            SqlType::INT4,
        )]),
        column_map: vec![Some(0), None],
        defaults: vec![None, Some(text("d"))],
        checks: vec![],
        overriding: None,
        returning: None,
    };
    let mut f = fixture_with_rows(&[(3, "x"), (1, "y"), (2, "z")]);
    let (_, n) = run_plan(&mut f, &BoundStatement::Insert(ins)).unwrap();
    assert_eq!(n, 3);
    let rows = f.storage.rows(t.oid);
    assert_eq!(rows[3], vec![Datum::Int4(1), Datum::Text("d".into())]);
    assert_eq!(rows[5], vec![Datum::Int4(3), Datum::Text("d".into())]);
}

// ----- UPDATE / DELETE ---------------------------------------------------------------------

const T: u32 = 16384;

fn dml_table() -> Arc<TableDef> {
    let c = |name: &str, attnum, ty, not_null| ColumnDef {
        name: name.into(),
        attnum,
        ty,
        not_null,
        default: None,
        identity: None,
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

fn dml_fixture(rows: &[(i32, &str, i32)]) -> Fixture {
    let mut f = Fixture::new();
    let t = dml_table();
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
        rtable: vec![table_rte(&dml_table())],
        from: vec![],
        filter,
        assignments,
        checks: vec![],
        not_null: vec![true, false, false],
        returning: None,
    }
}

fn delete(filter: Option<BoundExpr>) -> BoundDelete {
    BoundDelete {
        rtable: vec![table_rte(&dml_table())],
        from: vec![],
        filter,
        returning: None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_dml(f: &mut Fixture, stmt: BoundStatement) -> Result<u64> {
    run_plan(f, &stmt).map(|(_, n)| n)
}

fn a_gt(n: i32) -> BoundExpr {
    op(&GT, col(0, SqlType::INT4), int(n))
}

#[test]
fn update_with_where() {
    let mut f = dml_fixture(&[(1, "x", 0), (2, "y", 0)]);
    let u = update(vec![(1, UpdateSource::Expr(text("z")))], Some(a_gt(1)));
    let stmt = BoundStatement::Update(u);
    // Update <- Project <- SeqScan（WHERE は SeqScan の filter に押し込まれる。ctid を含む）。
    let PhysicalPlan::Update { input, .. } = root(&f, &stmt) else {
        panic!("expected Update");
    };
    let PhysicalPlan::Project { input, .. } = *input else {
        panic!("expected Project");
    };
    assert!(matches!(
        *input,
        PhysicalPlan::SeqScan {
            filter: Some(_),
            ..
        }
    ));
    assert_eq!(run_dml(&mut f, stmt).unwrap(), 1);
    assert_eq!(f.storage.rows(T), vec![row(1, "x", 0), row(2, "z", 0)]);
}

#[test]
fn set_expressions_see_the_old_row() {
    let mut f = dml_fixture(&[(1, "x", 10)]);
    let u = update(
        vec![
            (0, UpdateSource::Expr(col(2, SqlType::INT4))),
            (2, UpdateSource::Expr(col(0, SqlType::INT4))),
        ],
        None,
    );
    assert_eq!(run_dml(&mut f, BoundStatement::Update(u)).unwrap(), 1);
    assert_eq!(f.storage.rows(T), vec![row(10, "x", 1)]);
}

#[test]
fn update_without_where_does_not_rescan_new_versions() {
    let mut f = dml_fixture(&[(1, "a", 0), (2, "b", 0), (3, "c", 0)]);
    let u = update(vec![(2, UpdateSource::Expr(int(9)))], None);
    assert_eq!(run_dml(&mut f, BoundStatement::Update(u)).unwrap(), 3);
    assert_eq!(f.storage.rows(T).len(), 3);
    // All writes used the transaction's XID and the current command ID.
    assert_eq!(f.storage.writes(), vec![(f.txn.xid.unwrap(), 0); 3]);
}

#[test]
fn default_source() {
    let mut f = dml_fixture(&[(1, "x", 5)]);
    let u = update(
        vec![
            (1, UpdateSource::Default(None)),
            (2, UpdateSource::Default(Some(int(42)))),
        ],
        None,
    );
    run_dml(&mut f, BoundStatement::Update(u)).unwrap();
    assert_eq!(
        f.storage.rows(T),
        vec![vec![Datum::Int4(1), Datum::Null, Datum::Int4(42)]]
    );
}

#[test]
fn system_columns_in_where_are_dropped_before_update() {
    let mut f = dml_fixture(&[(1, "x", 0), (2, "y", 0)]);
    // WHERE tableoid IS NOT NULL: スキャンは `ctid` と `tableoid` を出し、`Project` が `tableoid` を落とす。
    let filter = BoundExpr::new(
        ExprKind::IsNotNull(Box::new(sys_col(
            SystemColumn::TableOid,
            SqlType::of(oid::OID),
        ))),
        SqlType::BOOL,
        Span::default(),
    );
    let u = update(vec![(2, UpdateSource::Expr(int(1)))], Some(filter));
    let stmt = BoundStatement::Update(u);
    let PhysicalPlan::Update { input, .. } = root(&f, &stmt) else {
        panic!("expected Update");
    };
    let PhysicalPlan::Project { exprs, input } = *input else {
        panic!("expected Project");
    };
    // 旧い列 3 + ctid + 新しい値 1。ctid は走査の 4 番目（システム列の先頭）。
    assert_eq!(exprs.len(), 5);
    assert!(matches!(exprs[3].kind, ExprKind::Column(PhysCol::Local(3))));
    let PhysicalPlan::SeqScan { system_columns, .. } = *input else {
        panic!("expected SeqScan");
    };
    assert_eq!(
        system_columns,
        vec![SystemColumn::Ctid, SystemColumn::TableOid]
    );
    assert_eq!(run_dml(&mut f, stmt).unwrap(), 2);
    assert_eq!(f.storage.rows(T), vec![row(1, "x", 1), row(2, "y", 1)]);
}

#[test]
fn delete_input_is_the_bare_scan_without_system_columns() {
    let f = dml_fixture(&[]);
    // 恒等の `Project` は省かれ、`SeqScan` が「ユーザー列 ++ ctid」を出す。
    let PhysicalPlan::Delete { input, .. } = root(&f, &BoundStatement::Delete(delete(None))) else {
        panic!("expected Delete");
    };
    let PhysicalPlan::SeqScan {
        columns,
        system_columns,
        ..
    } = *input
    else {
        panic!("expected SeqScan");
    };
    assert_eq!(columns.len(), 3);
    assert_eq!(system_columns, vec![SystemColumn::Ctid]);
}

#[test]
fn not_null_violation_leaves_the_row() {
    let mut f = dml_fixture(&[(1, "x", 0)]);
    let u = update(vec![(0, UpdateSource::Expr(null(SqlType::INT4)))], None);
    let e = run_dml(&mut f, BoundStatement::Update(u)).unwrap_err();
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
    let mut f = dml_fixture(&[(1, "x", 0), (5, "y", 0)]);
    let ck = |name: &str, n| BoundCheck {
        name: name.into(),
        expr: a_gt(n),
    };
    let mut u = update(vec![(0, UpdateSource::Expr(int(1)))], None);
    u.checks = vec![ck("t_zz", 0), ck("t_aa", 1)];
    let e = run_dml(&mut f, BoundStatement::Update(u)).unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::CHECK_VIOLATION);
    assert_eq!(
        e.message,
        "new row for relation \"t\" violates check constraint \"t_aa\""
    );
    assert!(f.storage.writes().is_empty());
}

#[test]
fn delete_with_and_without_where() {
    let mut f = dml_fixture(&[(1, "x", 0), (2, "y", 0), (3, "z", 0)]);
    assert_eq!(
        run_dml(&mut f, BoundStatement::Delete(delete(Some(a_gt(1))))).unwrap(),
        2
    );
    assert_eq!(f.storage.rows(T), vec![row(1, "x", 0)]);
    assert_eq!(
        run_dml(&mut f, BoundStatement::Delete(delete(None))).unwrap(),
        1
    );
    assert!(f.storage.rows(T).is_empty());
}

#[test]
fn self_modified_rows() {
    use crate::storage::TmResult;
    // Same command: skipped and not counted.
    let mut f = dml_fixture(&[(1, "x", 0)]);
    f.storage.force_result(TmResult::SelfModified { cmax: 0 });
    let u = update(vec![(2, UpdateSource::Expr(int(1)))], None);
    assert_eq!(
        run_dml(&mut f, BoundStatement::Update(u.clone())).unwrap(),
        0
    );
    // An earlier command: 27000.
    let mut f = dml_fixture(&[(1, "x", 0)]);
    f.storage.force_result(TmResult::SelfModified { cmax: 7 });
    let e = run_dml(&mut f, BoundStatement::Update(u)).unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::TRIGGERED_DATA_CHANGE_VIOLATION);
    let mut f = dml_fixture(&[(1, "x", 0)]);
    f.storage.force_result(TmResult::SelfModified { cmax: 7 });
    let d = BoundStatement::Delete(delete(None));
    let e = run_dml(&mut f, d.clone()).unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::TRIGGERED_DATA_CHANGE_VIOLATION);
    // Anything else is an internal error.
    let mut f = dml_fixture(&[(1, "x", 0)]);
    f.storage.force_result(TmResult::Deleted {
        xmax: crate::txn::Xid(9),
    });
    let e = run_dml(&mut f, d).unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
}

#[test]
fn interrupts_stop_dml() {
    let mut f = dml_fixture(&[(1, "x", 0)]);
    f.interrupts.request_terminate();
    let e = run_dml(&mut f, BoundStatement::Delete(delete(None))).unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::ADMIN_SHUTDOWN);
    assert_eq!(f.storage.rows(T).len(), 1);
}

#[test]
fn malformed_input_rows_are_internal_errors() {
    use crate::executor::eval::tests::{int as pint, text as ptext};
    use crate::executor::nodes::ValuesExec;
    use crate::planner::physical::PhysExpr;
    let mut f = dml_fixture(&[]);
    let bad = |exprs: Vec<PhysExpr>| -> crate::executor::BoxedExecutor {
        Box::new(crate::executor::nodes::DeleteExec::new(
            RelHandle::from_table(&dml_table()),
            Box::new(ValuesExec::new(vec![exprs])),
            3,
        ))
    };
    // Too short.
    let e = f.run(&mut bad(vec![pint(1)])).unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    // Last column is not a ctid.
    let e = f
        .run(&mut bad(vec![pint(1), ptext("x"), pint(2), pint(3)]))
        .unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
}

// ----- plan() の外形 -----------------------------------------------------------------------

#[test]
fn plan_rejects_utility_statements_and_plans_explain() {
    let f = Fixture::new();
    let e = plan_in(&f, &BoundStatement::Checkpoint).unwrap_err();
    assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    let explain = BoundStatement::Explain(Box::new(crate::analyzer::bound::BoundExplain {
        options: crate::analyzer::bound::ExplainOptions::default(),
        inner: BoundStatement::Select(Box::new(select(vec![], vec![int(1)], 1))),
    }));
    assert!(plan_in(&f, &explain).is_ok());
}

#[test]
fn planner_settings_default() {
    let s = PlannerSettings::default();
    assert!(s.enable_seqscan && s.enable_indexscan && s.enable_hashjoin && s.enable_nestloop);
    assert!(s.enable_hashagg && s.enable_sort && s.enable_material);
    assert_eq!(s.query_mem_limit, 256 << 20);
    assert_eq!(s.validate_plans, cfg!(debug_assertions));
}
