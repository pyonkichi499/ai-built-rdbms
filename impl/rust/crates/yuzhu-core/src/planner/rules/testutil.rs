//! プランナのテストの足場（`m4/04` §11.1・§11.2。持ち主は L1）。
//!
//! 製品のコードは使わない。`plan_golden`（統合テスト）と単体テストが使うので `pub` にしてある
//! （`#[doc(hidden)]`）。2 つの道具がある。
//!
//! - **手で組み立てる**: [`Fx`] と `col` / `int` / `op_eq` / `filter` など。1 つのルールを適用して印字を比べる。
//! - **SQL から**: [`Fixture`]。DDL（`CREATE TABLE` / `CREATE [UNIQUE] INDEX`）から表と索引を作り、SQL を
//!   解析 → build → 各ルールまで進めて各段階の印字を返す（ストレージなし。`nblocks` は偽物）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::{RuleCtx, RuleFn};
use crate::analyzer::bound::{BoundDdl, BoundStatement, IndexConstraintKind};
use crate::catalog::{
    BuiltinOperator, CatalogReader, ColumnDef, IndexColumn, IndexConstraintRef, IndexDef, RelKind,
    TableDef, builtin,
};
use crate::error::{Error, Result};
use crate::expr::ColId;
use crate::planner::logical::{ColumnArena, ColumnInfo, LExpr, LogicalPlan, LogicalQuery};
use crate::planner::{PlanEnv, PlanTrace, PlannerSettings, build, print};
use crate::storage::smgr::{DEFAULTTABLESPACE_OID, RelFileLocator, RelFileNumber};
use crate::storage::{
    HeapScan, HeapTuple, RelHandle, TableStore, TmResult, UpdateOutcome, WriteCtx,
};
use crate::txn::Snapshot;
use crate::types::{Datum, Oid, SqlType, Tid, TypeEnv, oid};

fn locator_of(oid: Oid) -> RelFileLocator {
    RelFileLocator {
        spc_oid: DEFAULTTABLESPACE_OID,
        db_oid: 5,
        rel_number: RelFileNumber(oid),
    }
}

fn table_def(oid: Oid, name: &str, columns: Vec<ColumnDef>) -> TableDef {
    TableDef {
        oid,
        namespace: 2200,
        schema: "public".into(),
        name: name.into(),
        kind: RelKind::Table,
        locator: locator_of(oid),
        columns,
        checks: Vec::new(),
        indexes: Vec::new(),
        sequence: None,
        identity_seqs: Vec::new(),
    }
}

// ----- 手で組み立てる -------------------------------------------------------------------

/// 台帳と、そこから `Get` などを作る補助。
#[derive(Default, Debug)]
#[doc(hidden)]
pub struct Fx {
    pub arena: ColumnArena,
    next_oid: u32,
}

impl Fx {
    pub fn new() -> Self {
        Fx {
            arena: ColumnArena::default(),
            next_oid: 16384,
        }
    }

    /// `t(c0 int4, c1 int4, ...)` を `ncols` 列で走査する `Get`。
    pub fn get(&mut self, name: &str, ncols: usize) -> LogicalPlan {
        let columns: Vec<ColumnDef> = (0..ncols)
            .map(|i| ColumnDef {
                name: format!("c{i}"),
                attnum: i16::try_from(i + 1).unwrap(),
                ty: SqlType::INT4,
                not_null: false,
                default: None,
                identity: None,
            })
            .collect();
        self.next_oid += 1;
        let table: Arc<TableDef> = Arc::new(table_def(self.next_oid, name, columns));
        let cols = (0..ncols)
            .map(|i| {
                self.arena.add(ColumnInfo {
                    name: format!("c{i}"),
                    qualifier: Some(name.to_owned()),
                    ty: SqlType::INT4,
                    origin: Some((table.oid, i16::try_from(i + 1).unwrap())),
                })
            })
            .collect();
        LogicalPlan::Get {
            rel: RelHandle::from_table(&table),
            table,
            alias: None,
            cols,
            system_columns: Vec::new(),
        }
    }

    /// 台帳に列を 1 つ足す（`Project` の計算列など）。
    pub fn new_col(&mut self, name: &str) -> ColId {
        self.arena.add(ColumnInfo {
            name: name.to_owned(),
            qualifier: None,
            ty: SqlType::INT4,
            origin: None,
        })
    }

    /// `plan` を根にした `LogicalQuery`（出力は根の出力列）。
    pub fn query(self, plan: LogicalPlan) -> LogicalQuery {
        let output = plan.output_cols();
        LogicalQuery {
            plan,
            arena: self.arena,
            output,
            columns: Vec::new(),
            ctes: Vec::new(),
            n_subplans_hint: 0,
        }
    }
}

pub fn col(id: u32) -> LExpr {
    LExpr::column(ColId(id), SqlType::INT4)
}

pub fn int(v: i32) -> LExpr {
    LExpr::literal(Datum::Int4(v), SqlType::INT4)
}

pub fn eq_op() -> &'static BuiltinOperator {
    named_op("=", SqlType::INT4)
}

/// `name` の（両辺が `ty` の）演算子。
pub fn named_op(name: &str, ty: SqlType) -> &'static BuiltinOperator {
    builtin::operators_named(name)
        .into_iter()
        .find(|o| o.left == Some(ty.oid) && o.right == ty.oid)
        .unwrap_or_else(|| panic!("operator {name} for {}", ty.oid))
}

/// `a <op> b`（int4 の演算子）。
pub fn op(name: &str, a: LExpr, b: LExpr) -> LExpr {
    let o = named_op(name, SqlType::INT4);
    LExpr::new(
        crate::expr::ExprKind::Operator {
            op: o,
            args: vec![a, b],
        },
        SqlType::of(o.result),
        crate::error::Span::default(),
    )
}

pub fn op_eq(a: LExpr, b: LExpr) -> LExpr {
    op("=", a, b)
}

pub fn filter(input: LogicalPlan, predicate: LExpr) -> LogicalPlan {
    LogicalPlan::Filter {
        input: Box::new(input),
        predicate,
    }
}

pub fn join(
    kind: crate::planner::logical::JoinKind,
    left: LogicalPlan,
    right: LogicalPlan,
    on: Option<LExpr>,
) -> LogicalPlan {
    LogicalPlan::Join {
        kind,
        left: Box::new(left),
        right: Box::new(right),
        on,
    }
}

// ----- SQL から -------------------------------------------------------------------------

/// `nblocks` を返すだけの `TableStore`（ほかの操作は使わない）。
#[derive(Debug, Default)]
struct BlocksStore {
    blocks: Mutex<HashMap<Oid, u32>>,
}

fn unused<T>() -> Result<T> {
    Err(Error::internal(
        "the planner fixture store only has nblocks",
    ))
}

impl TableStore for BlocksStore {
    fn create_storage(&self, _w: &WriteCtx, _rel: RelFileLocator) -> Result<()> {
        unused()
    }
    fn storage_exists(&self, _rel: RelFileLocator) -> Result<bool> {
        unused()
    }
    fn unlink_storage(&self, _rel: RelFileLocator) -> Result<()> {
        unused()
    }
    fn insert(&self, _rel: &RelHandle, _w: &WriteCtx, _row: &[Datum]) -> Result<Tid> {
        unused()
    }
    fn delete(
        &self,
        _rel: &RelHandle,
        _w: &WriteCtx,
        _snap: &Snapshot,
        _tid: Tid,
    ) -> Result<TmResult> {
        unused()
    }
    fn update(
        &self,
        _rel: &RelHandle,
        _w: &WriteCtx,
        _snap: &Snapshot,
        _tid: Tid,
        _new_row: &[Datum],
    ) -> Result<UpdateOutcome> {
        unused()
    }
    fn begin_scan(&self, _rel: &RelHandle, _snap: &Snapshot) -> Result<HeapScan> {
        unused()
    }
    fn scan_next(&self, _scan: &mut HeapScan) -> Result<Option<HeapTuple>> {
        unused()
    }
    fn fetch(&self, _rel: &RelHandle, _snap: &Snapshot, _tid: Tid) -> Result<Option<HeapTuple>> {
        unused()
    }
    fn nblocks(&self, rel: &RelHandle) -> Result<u32> {
        Ok(self
            .blocks
            .lock()
            .map_err(|_| Error::internal("poisoned"))?
            .get(&rel.oid)
            .copied()
            .unwrap_or(0))
    }
}

/// 表と索引だけを持つ偽のカタログ。
#[derive(Debug)]
struct MemCatalog {
    tables: Vec<Arc<TableDef>>,
    search_path: Vec<String>,
    next_oid: Oid,
}

impl MemCatalog {
    fn alloc(&mut self) -> Oid {
        self.next_oid += 1;
        self.next_oid
    }
}

impl CatalogReader for MemCatalog {
    fn table(&self, schema: Option<&str>, name: &str) -> Result<Option<Arc<TableDef>>> {
        let schema = schema.unwrap_or("public");
        Ok(self
            .tables
            .iter()
            .find(|t| t.schema == schema && t.name == name)
            .cloned())
    }
    fn table_by_oid(&self, oid: Oid) -> Result<Option<Arc<TableDef>>> {
        Ok(self.tables.iter().find(|t| t.oid == oid).cloned())
    }
    #[allow(clippy::unnecessary_literal_bound)]
    fn current_database(&self) -> &str {
        "postgres"
    }
    fn search_path(&self) -> &[String] {
        &self.search_path
    }
    fn role_name(&self, _oid: Oid) -> Result<Option<String>> {
        Ok(None)
    }
    fn visible_namespaces(&self) -> Result<Vec<Oid>> {
        Ok(vec![11, 2200])
    }
}

/// `pg_opfamily` の OID（`m4/11` §7.2）。
fn opfamily(ty: SqlType) -> Oid {
    match ty.oid {
        oid::BOOL => 424,
        oid::BPCHAR => 426,
        oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ => 434,
        oid::FLOAT4 | oid::FLOAT8 => 1970,
        oid::INT2 | oid::INT4 | oid::INT8 => 1976,
        oid::NUMERIC => 1988,
        oid::OID => 1989,
        _ => 1994,
    }
}

/// 表と索引を持つ偽のカタログと、`nblocks` を返す偽のストレージ。
#[derive(Debug)]
#[doc(hidden)]
pub struct Fixture {
    catalog: MemCatalog,
    store: BlocksStore,
    pub settings: PlannerSettings,
}

impl Fixture {
    /// `CREATE TABLE` / `CREATE [UNIQUE] INDEX` の列を実行して作る。`validate_plans` は真。
    pub fn new(ddl: &str) -> Result<Self> {
        let mut f = Fixture {
            catalog: MemCatalog {
                tables: Vec::new(),
                search_path: vec!["public".into()],
                next_oid: 16383,
            },
            store: BlocksStore::default(),
            settings: PlannerSettings {
                validate_plans: true,
                ..PlannerSettings::default()
            },
        };
        f.exec_ddl(ddl)?;
        Ok(f)
    }

    /// 文を足す（`CREATE TABLE` / `CREATE INDEX`）。
    pub fn exec_ddl(&mut self, ddl: &str) -> Result<()> {
        for stmt in crate::sql::parse(ddl)? {
            let BoundStatement::Ddl(d) = crate::analyzer::analyze(&stmt, &self.catalog)? else {
                return Err(Error::internal("the fixture takes only DDL"));
            };
            match d {
                BoundDdl::CreateTable(ct) => {
                    let table_oid = self.catalog.alloc();
                    let mut def = table_def(table_oid, &ct.name, ct.columns.clone());
                    for k in &ct.constraints {
                        let attnums = k.columns.clone();
                        let primary = k.kind == IndexConstraintKind::PrimaryKey;
                        if primary {
                            for a in &attnums {
                                def.columns[usize::try_from(*a - 1).unwrap_or(0)].not_null = true;
                            }
                        }
                        let names: Vec<&str> = attnums
                            .iter()
                            .map(|a| {
                                def.columns[usize::try_from(*a - 1).unwrap_or(0)]
                                    .name
                                    .as_str()
                            })
                            .collect();
                        let name = k.name.clone().unwrap_or_else(|| {
                            if primary {
                                format!("{}_pkey", ct.name)
                            } else {
                                format!("{}_{}_key", ct.name, names.join("_"))
                            }
                        });
                        let cols = attnums.iter().map(|a| (*a, false, false)).collect();
                        let ix = self.make_index(&def, name, cols, true, primary, true);
                        def.indexes.push(Arc::new(ix));
                    }
                    self.catalog.tables.push(Arc::new(def));
                }
                BoundDdl::CreateIndex(ci) => {
                    let mut def = (*ci.table).clone();
                    let cols: Vec<(i16, bool, bool)> = ci
                        .columns
                        .iter()
                        .map(|c| (c.attnum, c.descending, c.nulls_first))
                        .collect();
                    let name = ci.name.clone().unwrap_or_else(|| {
                        let names: Vec<&str> = cols
                            .iter()
                            .map(|(a, _, _)| {
                                def.columns[usize::try_from(*a - 1).unwrap_or(0)]
                                    .name
                                    .as_str()
                            })
                            .collect();
                        format!("{}_{}_idx", def.name, names.join("_"))
                    });
                    let ix = self.make_index(&def, name, cols, ci.unique, false, false);
                    def.indexes.push(Arc::new(ix));
                    def.indexes.sort_by_key(|i| i.oid);
                    self.catalog.tables.retain(|t| t.oid != ci.table.oid);
                    self.catalog.tables.push(Arc::new(def));
                }
                _ => {
                    return Err(Error::internal(
                        "the fixture takes only CREATE TABLE / INDEX",
                    ));
                }
            }
        }
        Ok(())
    }

    fn make_index(
        &mut self,
        table: &TableDef,
        name: String,
        cols: Vec<(i16, bool, bool)>,
        unique: bool,
        primary: bool,
        constraint: bool,
    ) -> IndexDef {
        let index_oid = self.catalog.alloc();
        IndexDef {
            oid: index_oid,
            name: name.clone(),
            namespace: 2200,
            table_oid: table.oid,
            locator: locator_of(index_oid),
            columns: cols
                .into_iter()
                .map(|(attnum, descending, nulls_first)| IndexColumn {
                    attnum,
                    opclass: 0,
                    opfamily: opfamily(table.columns[usize::try_from(attnum - 1).unwrap_or(0)].ty),
                    descending,
                    nulls_first,
                })
                .collect(),
            unique,
            primary,
            constraint: constraint.then(|| IndexConstraintRef {
                oid: index_oid + 1_000_000,
                name,
            }),
        }
    }

    /// 表の `nblocks`（計画時のサイズの手がかり）。
    pub fn set_nblocks(&self, table: &str, n: u32) -> Result<()> {
        let t = self
            .catalog
            .table(None, table)?
            .ok_or_else(|| Error::internal(format!("no table {table}")))?;
        self.store
            .blocks
            .lock()
            .map_err(|_| Error::internal("poisoned"))?
            .insert(t.oid, n);
        Ok(())
    }

    /// 1 文を解析する。
    pub fn analyze(&self, sql: &str) -> Result<BoundStatement> {
        let stmts = crate::sql::parse(sql)?;
        if stmts.len() != 1 {
            return Err(Error::internal("one statement expected"));
        }
        crate::analyzer::analyze(&stmts[0], &self.catalog)
    }

    /// `f(&PlanEnv)` を呼ぶ。
    pub fn with_env<R>(&self, f: impl FnOnce(&PlanEnv<'_>) -> R) -> R {
        let type_env = TypeEnv::default();
        let env = PlanEnv {
            catalog: &self.catalog,
            storage: &self.store,
            settings: &self.settings,
            type_env: &type_env,
            want_explain: false,
            explain_verbose: false,
        };
        f(&env)
    }

    /// build 直後の論理プラン。
    pub fn build(&self, sql: &str) -> Result<LogicalQuery> {
        let stmt = self.analyze(sql)?;
        self.with_env(|env| build::build_statement(&stmt, env))
    }

    /// build の後、名前を指定したルールだけ（指定の順）を適用した論理プラン。
    pub fn plan_with(&self, sql: &str, rules: &[&str]) -> Result<LogicalQuery> {
        let mut q = self.build(sql)?;
        self.with_env(|env| super::run_only(&mut q, env, rules))?;
        Ok(q)
    }

    /// build とすべてのルールを適用した論理プラン。
    pub fn plan_all(&self, sql: &str) -> Result<LogicalQuery> {
        let names: Vec<&str> = super::RULES.iter().map(|(n, _)| *n).collect();
        self.plan_with(sql, &names)
    }

    /// `build` とルールごとの段階の印字（`(段階名, 印字)`）。
    pub fn stages(&self, sql: &str) -> Result<Vec<(String, String)>> {
        let stmt = self.analyze(sql)?;
        let mut trace = Collect(Vec::new());
        self.with_env(|env| {
            let mut q = build::build_statement(&stmt, env)?;
            super::run(&mut q, env, &mut trace)
        })?;
        Ok(trace.0)
    }

    /// 1 つのルールを `q` に適用する。
    pub fn apply(&self, q: &mut LogicalQuery, rule: RuleFn) -> Result<()> {
        self.with_env(|env| rule(q, &RuleCtx { env }))
    }

    /// 印字だけ（`plan_all` の `print_logical`）。
    pub fn print_all(&self, sql: &str) -> Result<String> {
        Ok(print::print_logical(&self.plan_all(sql)?))
    }
}

struct Collect(Vec<(String, String)>);

impl PlanTrace for Collect {
    fn logical(&mut self, stage: &str, q: &LogicalQuery) {
        self.0.push((stage.to_owned(), print::print_logical(q)));
    }
    fn physical(&mut self, _q: &crate::planner::physical::PhysicalQuery) {}
}
