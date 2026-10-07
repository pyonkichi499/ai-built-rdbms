//! アナライザの出力（`m4/00-contracts.md` §7、`m4/02-pipeline-refactor.md` §3.4）。式は [`crate::expr::Expr`] の `Var` 版
//! （[`BoundExpr`]）で、列は `Var { rte, col, levels_up }` で参照する。
//!
//! 不変条件 B1〜B11 は [`BoundQuery::validate`]（`m4/02` §3.2.2）が検査する。

#![allow(clippy::doc_markdown, clippy::too_many_lines)]

use std::sync::Arc;

use crate::catalog::depend::DropBehavior;
use crate::catalog::{CheckDef, ColumnDef, IndexDef, TableDef};
use crate::error::{Error, Result, Span};
use crate::expr::{AggCall, CteId, Expr, ExprKind, RteId, SubLinkKind, Var};
use crate::types::{Oid, SqlType};

/// 出力列のメタデータ（`ColumnDesc` の元）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputColumn {
    pub name: String,
    pub ty: SqlType,
    /// 出力式が単純な列参照のときの元の表と attnum。そうでなければ 0。
    pub table_oid: Oid,
    pub attnum: i16,
}
pub use crate::catalog::IdentityKind;

pub type BoundExpr = Expr<Var, Box<BoundQuery>>;
pub type BoundExprKind = ExprKind<Var, Box<BoundQuery>>;
pub type BoundAggCall = AggCall<Var, Box<BoundQuery>>;

/// `INSERT ... OVERRIDING { SYSTEM | USER } VALUE`。S1 が AST に同じ型を足したら、
/// `pub use crate::sql::ast::OverridingKind` に置き換える（00 §7 の約束）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OverridingKind {
    System,
    User,
}

/// 集合演算の種類（`planner::logical::SetOpKind` はこれを再公開する）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

// ----- 文 --------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum BoundStatement {
    Select(Box<BoundQuery>),
    Insert(BoundInsert),
    Update(BoundUpdate),
    Delete(BoundDelete),
    Copy(BoundCopy),
    Explain(Box<BoundExplain>),
    Ddl(BoundDdl),
    /// `CHECKPOINT`: session が直接実行する。
    Checkpoint,
}

impl BoundStatement {
    /// 行を返す文か（RowDescription を送るか）。`Select` と `Explain` は true、`Insert` / `Update` /
    /// `Delete` は `returning` が `Some` のときだけ。`SELECT FROM t`（列が 0 個）も行を返す文なので、
    /// 出力列の数では判定できない。
    pub fn returns_rows(&self) -> bool {
        match self {
            BoundStatement::Select(_) | BoundStatement::Explain(_) => true,
            BoundStatement::Insert(i) => i.returning.is_some(),
            BoundStatement::Update(u) => u.returning.is_some(),
            BoundStatement::Delete(d) => d.returning.is_some(),
            BoundStatement::Copy(_) | BoundStatement::Ddl(_) | BoundStatement::Checkpoint => false,
        }
    }
}

/// WITH + 本体 + ORDER BY / LIMIT / OFFSET。
#[derive(Debug, Clone)]
pub struct BoundQuery {
    pub ctes: Vec<BoundCte>,
    pub body: BoundSetExpr,
    /// 本体の `targets`（Select）または出力列（Values / SetOp）の位置を指す。resjunk を指してもよい。
    pub order_by: Vec<BoundSortKey>,
    /// int8 の式。レベル 0 の `Var` を含まない（B8）。
    pub limit: Option<BoundExpr>,
    pub offset: Option<BoundExpr>,
    /// 外に見える出力列（名前・型・元の表と attnum）。
    pub columns: Vec<OutputColumn>,
}

#[derive(Debug, Clone)]
pub enum BoundSetExpr {
    Select(Box<BoundSelect>),
    /// 単独の VALUES。各行は列型にそろえた式。各行は rtable が空の 1 スコープ。
    Values {
        rows: Vec<Vec<BoundExpr>>,
        types: Vec<SqlType>,
    },
    /// 腕は ORDER BY / LIMIT を持てる（括弧つき）ので `BoundQuery`。`left_coerce` / `right_coerce` は
    /// 腕の出力列を共通型に直す式の並びで、腕の出力行の i 番目を `Var { rte: RteId(0), col: i,
    /// levels_up: 0 }` で参照する（`None` = 型変換不要）。
    SetOp {
        op: SetOpKind,
        all: bool,
        left: Box<BoundQuery>,
        right: Box<BoundQuery>,
        left_coerce: Option<Vec<BoundExpr>>,
        right_coerce: Option<Vec<BoundExpr>>,
        types: Vec<SqlType>,
    },
}

#[derive(Debug, Clone)]
pub struct BoundSelect {
    /// このスコープの範囲表。JOIN 自体も RTE になる。添字が `RteId`。
    pub rtable: Vec<Rte>,
    /// FROM 句の項目（カンマ区切りごと）。空 = FROM なし。
    pub from: Vec<FromItem>,
    /// WHERE。集約を含まない。
    pub filter: Option<BoundExpr>,
    /// GROUP BY の式（位置番号・別名は解決済み）。
    pub group_by: Vec<BoundExpr>,
    /// HAVING。集約を含んでよい。
    pub having: Option<BoundExpr>,
    /// 集約・GROUP BY・HAVING のいずれかがあるか。`group_by` が空で true なら全体を 1 グループとする。
    pub has_agg: bool,
    /// 先頭 `n_visible` 個が出力列、以降は resjunk（ORDER BY 用）。集約を含んでよい。
    pub targets: Vec<BoundExpr>,
    pub n_visible: usize,
    pub distinct: BoundDistinct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundDistinct {
    None,
    All,
    /// `targets` の位置。ORDER BY の先頭と一致していることを検査済み。
    On(Vec<usize>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BoundSortKey {
    pub target: usize,
    pub descending: bool,
    pub nulls_first: bool,
}

#[derive(Debug, Clone)]
pub enum FromItem {
    /// Table / Subquery / Values / Function / CteRef の RTE。
    Scan(RteId),
    Join {
        rte: RteId,
        kind: JoinType,
        left: Box<FromItem>,
        right: Box<FromItem>,
        on: Option<BoundExpr>,
    },
}

/// `FromItem` が持つ RTE の ID（`crate::expr::RteId`）。
pub type Expr0 = crate::expr::RteId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone)]
pub struct Rte {
    pub kind: RteKind,
    /// 別名、なければ表名。別名なしの JOIN / 副問い合わせ / VALUES は `None`。
    pub refname: Option<String>,
    /// `SELECT *` で展開される順の列。表なら attnum 順のユーザー列（システム列は含まない）。
    pub columns: Vec<RteColumn>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RteColumn {
    pub name: String,
    pub ty: SqlType,
}

#[derive(Debug, Clone)]
pub enum RteKind {
    Table {
        table: Arc<TableDef>,
    },
    Subquery {
        query: Box<BoundQuery>,
    },
    Values {
        rows: Vec<Vec<BoundExpr>>,
    },
    /// FROM 句の関数呼び出し（`generate_series`）。`call` は `Function` の式。
    Function {
        call: BoundExpr,
    },
    /// `columns` の i 番目が子のどの列か（USING / NATURAL で併合した列が先頭に来る）。
    Join {
        kind: JoinType,
        left: RteId,
        right: RteId,
        sources: Vec<JoinColSource>,
    },
    /// `levels_up` は CTE を宣言した `BoundQuery` までの入れ子の深さ（`BoundQuery` を数える）。
    CteRef {
        levels_up: u16,
        cte: CteId,
    },
}

/// 子の `Rte.columns` の添字。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinColSource {
    Left(u16),
    Right(u16),
    Coalesce(u16, u16),
}

#[derive(Debug, Clone)]
pub struct BoundCte {
    pub name: String,
    pub query: BoundQuery,
    pub materialize: CteMaterialize,
    pub col_aliases: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CteMaterialize {
    Default,
    Always,
    Never,
}

// ----- DML -------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BoundInsert {
    pub table: Arc<TableDef>,
    pub source: Box<BoundQuery>,
    /// `source` の出力列を `Var { rte: 0, col: i }` で参照する式（B10）。
    pub coercions: Option<Vec<BoundExpr>>,
    pub column_map: Vec<Option<usize>>,
    /// 列型にそろえた式（`Var` を含まない。`nextval` などは含みうる）。
    pub defaults: Vec<Option<BoundExpr>>,
    /// `rte` 0 = 対象表。
    pub checks: Vec<BoundCheck>,
    pub overriding: Option<OverridingKind>,
    pub returning: Option<BoundReturning>,
}

#[derive(Debug, Clone)]
pub struct BoundUpdate {
    /// `rtable[0]` = 対象表（`RteKind::Table`）。以降は FROM 句の RTE。
    pub rtable: Vec<Rte>,
    /// `UPDATE ... FROM` の項目。対象表は含めない。
    pub from: Vec<FromItem>,
    pub filter: Option<BoundExpr>,
    /// `(attnum - 1, 代入元)`。式は `rtable` 全体の `Var` を参照してよい。
    pub assignments: Vec<(usize, UpdateSource)>,
    pub checks: Vec<BoundCheck>,
    pub not_null: Vec<bool>,
    pub returning: Option<BoundReturning>,
}

#[derive(Debug, Clone)]
pub struct BoundDelete {
    /// `rtable[0]` = 対象表。
    pub rtable: Vec<Rte>,
    /// `DELETE ... USING` の項目。
    pub from: Vec<FromItem>,
    pub filter: Option<BoundExpr>,
    pub returning: Option<BoundReturning>,
}

/// `Var { rte: 0 }` だけを含む（対象表の行に対する式）。
#[derive(Debug, Clone)]
pub struct BoundCheck {
    pub name: String,
    pub expr: BoundExpr,
}

/// 対象表の `Var` だけを含む（M4）。
#[derive(Debug, Clone)]
pub struct BoundReturning {
    pub targets: Vec<BoundExpr>,
    pub columns: Vec<OutputColumn>,
}

#[derive(Debug, Clone)]
pub enum UpdateSource {
    Expr(BoundExpr),
    /// `DEFAULT`（`None` = NULL）。式は `Var` を含まない。
    Default(Option<BoundExpr>),
}

// ----- DDL・COPY・EXPLAIN（名前だけここで固定。フィールドは持ち主の章が決める）-----------

#[derive(Debug, Clone)]
pub enum BoundDdl {
    CreateTable(BoundCreateTable),
    DropTable(BoundDropTable),
    CreateIndex(BoundCreateIndex),
    DropIndex(BoundDropIndex),
    CreateSequence(BoundCreateSequence),
    AlterSequence(BoundAlterSequence),
    DropSequence(BoundDropSequence),
    AlterTableAddConstraint(BoundAlterTableAddConstraint),
    AlterTableAddCheck(BoundAlterTableAddCheck),
    AlterTableOwner(BoundAlterTableOwner),
    Truncate(BoundTruncate),
    /// VACUUM と ANALYZE（何もしない）。
    Vacuum(BoundVacuum),
}

/// `m4/07-catalog-ddl.md`・`m4/08-sequence-serial.md`。M2 のフィールドに追加。
#[derive(Debug, Clone)]
pub struct BoundCreateTable {
    pub schema: String,
    pub name: String,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
    pub checks: Vec<CheckDef>,
    /// PRIMARY KEY / UNIQUE（07）。
    pub constraints: Vec<BoundIndexConstraint>,
    /// SERIAL / IDENTITY が作る暗黙のシーケンス（08）。`owner` は `SeqOwner::NewTableColumn`。
    /// SERIAL の列の `default` は `None`（`ddl::table` がシーケンスの OID を決めてから入れる）。
    pub sequences: Vec<BoundCreateSequence>,
    /// `WITH (...)`。`ddl` が検証する（07 §5.9）。
    pub options: Vec<RelOption>,
    /// 既定値の式が `regclass` 定数で指す既存のリレーション: (attnum, リレーションの OID)。
    pub default_refs: Vec<(i16, Oid)>,
}

#[derive(Debug, Clone)]
pub struct BoundIndexConstraint {
    /// 明示された `CONSTRAINT` 名。`None` なら実行時に `choose_index_name` で決める（D07-4）。
    pub name: Option<String>,
    pub kind: IndexConstraintKind,
    /// attnum。重複なし。
    pub columns: Vec<i16>,
    /// 索引の `WITH (fillfactor)`。
    pub options: Vec<RelOption>,
    pub span: Span,
}

/// `WITH (name = value)` の 1 項目。`value` は字面（文字列リテラルは引用符を外した中身、数値・識別子はそのまま）。
/// 値なしは `None`（`true` と同じ扱い）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelOption {
    pub namespace: Option<String>,
    pub name: String,
    pub value: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexConstraintKind {
    PrimaryKey,
    Unique,
}

/// DROP TABLE の名前解決後。
#[derive(Debug, Clone)]
pub struct BoundDropTable {
    /// 落とす表。IF EXISTS で存在しない名前は `missing`（NOTICE を出す）。重複は除去済み。
    pub tables: Vec<Arc<TableDef>>,
    pub missing: Vec<String>,
    pub behavior: DropBehavior,
}

#[derive(Debug, Clone)]
pub struct BoundCreateIndex {
    pub table: Arc<TableDef>,
    /// 明示された索引名（修飾なし。スキーマは常に表と同じ）。`None` なら `choose_index_name`。
    pub name: Option<String>,
    pub unique: bool,
    pub if_not_exists: bool,
    pub concurrently: bool,
    /// アクセスメソッド名（小文字化済み）。既定は `btree`。btree 以外は ddl が `0A000` / `42704`。
    pub method: String,
    pub columns: Vec<BoundIndexColumn>,
    pub options: Vec<RelOption>,
}

#[derive(Debug, Clone)]
pub struct BoundIndexColumn {
    pub attnum: i16,
    /// 明示された演算子クラス（修飾は pg_catalog だけ許す）。`None` なら既定。
    pub opclass: Option<String>,
    pub descending: bool,
    /// `NULLS FIRST / LAST` の省略は「DESC なら先頭、ASC なら末尾」に解決済み（07 §3.4）。
    pub nulls_first: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BoundDropIndex {
    /// 重複は除去済み。
    pub indexes: Vec<Arc<IndexDef>>,
    pub missing: Vec<String>,
    pub behavior: DropBehavior,
    pub concurrently: bool,
}

#[derive(Debug, Clone)]
pub enum AlterTarget {
    Found(Arc<TableDef>),
    /// `IF EXISTS` で存在しなかった表の名前。NOTICE を出して何もしない。
    Missing(String),
}

#[derive(Debug, Clone)]
pub struct BoundAlterTableAddConstraint {
    pub target: AlterTarget,
    pub constraint: BoundIndexConstraint,
}

/// `ALTER TABLE ... ADD [CONSTRAINT n] CHECK (expr)`。`expr` は `Var { rte: 0 }` だけを含む（既存の行の検査用）。
#[derive(Debug, Clone)]
pub struct BoundAlterTableAddCheck {
    pub target: AlterTarget,
    /// 明示名、なければ自動名（`Missing` のときは空）。
    pub name: String,
    pub expr_sql: String,
    pub no_inherit: bool,
    pub expr: Option<BoundExpr>,
}

#[derive(Debug, Clone)]
pub struct BoundAlterTableOwner {
    pub target: AlterTarget,
    pub new_owner: OwnerSpec,
}

/// `CURRENT_ROLE` は `CurrentUser` と同じ扱い。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnerSpec {
    Name(String),
    CurrentUser,
    SessionUser,
}

#[derive(Debug, Clone)]
pub struct BoundTruncate {
    pub tables: Vec<Arc<TableDef>>,
    pub restart_identity: bool,
    pub cascade: bool,
}

#[derive(Debug, Clone)]
pub struct BoundVacuum {
    /// `VACUUM`（真）か、`ANALYZE` だけ（偽）か。`VACUUM ANALYZE` は `vacuum = true`、`analyze = true`。
    pub vacuum: bool,
    pub analyze: bool,
    pub options: Vec<VacuumOption>,
    /// 空なら「全部」（何もしない）。
    pub targets: Vec<VacuumTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VacuumOption {
    pub name: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VacuumTarget {
    pub name: String,
    /// 表なら `Some`。索引・シーケンスなら `None`（実行時に WARNING を出して飛ばす）。
    pub table: Option<Arc<TableDef>>,
    /// ANALYZE の列リスト（表の列に存在することは解析済み）。
    pub columns: Vec<String>,
}

/// シーケンスの Bound は Q1 が中身を決める（08 §4.8）。`BoundCreateSequence` は C1b が CREATE TABLE
/// （07 §5.1）のために 08 §4.8 のとおり宣言した（Q1 が同じ形に揃える）。
#[derive(Debug, Clone)]
pub struct BoundCreateSequence {
    pub schema: String,
    pub namespace: Oid,
    pub name: String,
    pub if_not_exists: bool,
    /// 既定値を埋めた最終値（`init_params` の結果。`owned_by` は `None`。所有は `owner` で表す）。
    pub params: crate::catalog::SequenceParams,
    /// CREATE 時の初期状態（`RESTART n` があれば `last_value = n`）。通常は `(start, 0, false)`。
    pub initial: crate::storage::SeqState,
    pub owner: SeqOwner,
    /// IDENTITY の暗黙のシーケンス（`pg_depend` の種類が `i`）。
    pub for_identity: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqOwner {
    None,
    /// 既存の表の列（`CREATE SEQUENCE ... OWNED BY t.c`。種類は `a`）。
    Column {
        table: Oid,
        attnum: i16,
    },
    /// 同じ CREATE TABLE が作る表の列（表の OID は実行時に決まる）。`serial_default = true` は SERIAL
    /// （種類 `a`、DEFAULT を付ける）、`false` は IDENTITY（種類 `i`）。
    NewTableColumn {
        attnum: i16,
        serial_default: bool,
    },
}
/// `ALTER SEQUENCE`（`m4/08-sequence-serial.md` §4.8）。
#[derive(Debug, Clone)]
pub struct BoundAlterSequence {
    /// `None` = `IF EXISTS` で存在しなかった（NOTICE を出して終わる）。
    pub target: Option<Arc<TableDef>>,
    pub missing_name: String,
    pub action: BoundAlterAction,
}

#[derive(Debug, Clone)]
pub enum BoundAlterAction {
    Options {
        options: crate::catalog::seq_params::SeqOptions,
        owned_by: Option<OwnedByTarget>,
    },
    /// `OWNER TO`（ロールの存在を確かめて何もしない）。
    OwnerNoop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedByTarget {
    None,
    Column { table: Oid, attnum: i16 },
}

#[derive(Debug, Clone)]
pub struct BoundDropSequence {
    pub targets: Vec<Arc<TableDef>>,
    /// `IF EXISTS` で存在しなかった名前（NOTICE 用）。
    pub missing: Vec<String>,
    pub cascade: bool,
}
/// `COPY ... FROM STDIN` の解析結果（定義は `copy/mod.rs`。`m4/10-explain-copy-compat.md` §5.1）。
pub use crate::copy::BoundCopy;

#[derive(Debug, Clone)]
pub struct BoundExplain {
    pub options: ExplainOptions,
    pub inner: BoundStatement,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ExplainOptions {
    pub analyze: bool,
    pub verbose: bool,
    pub costs: bool,
    pub timing: bool,
    pub summary: bool,
}

// ----- 式の走査 ----------------------------------------------------------------

/// `e` と部分木の各ノードを先行順に `f(node, depth)` へ渡す。`SubLink` の `query` は `depth + 1` で再帰する。
fn visit_expr(e: &BoundExpr, depth: u16, f: &mut dyn FnMut(&BoundExpr, u16)) {
    e.walk(&mut |n| {
        f(n, depth);
        if let ExprKind::SubLink { query, .. } = &n.kind {
            query.walk_exprs(depth + 1, f);
        }
        true
    });
}

impl BoundSelect {
    pub fn walk_exprs(&self, depth_base: u16, f: &mut dyn FnMut(&BoundExpr, u16)) {
        for rte in &self.rtable {
            match &rte.kind {
                RteKind::Values { rows } => {
                    for e in rows.iter().flatten() {
                        visit_expr(e, depth_base, f);
                    }
                }
                RteKind::Function { call } => visit_expr(call, depth_base, f),
                // 導出表は 1 段内側（非 LATERAL なので兄弟の RTE は見えない）。
                RteKind::Subquery { query } => query.walk_exprs(depth_base + 1, f),
                RteKind::Table { .. } | RteKind::Join { .. } | RteKind::CteRef { .. } => {}
            }
        }
        for item in &self.from {
            walk_from_item_exprs(item, depth_base, f);
        }
        for e in self
            .filter
            .iter()
            .chain(&self.group_by)
            .chain(self.having.iter())
            .chain(&self.targets)
        {
            visit_expr(e, depth_base, f);
        }
    }
}

fn walk_from_item_exprs(item: &FromItem, depth_base: u16, f: &mut dyn FnMut(&BoundExpr, u16)) {
    if let FromItem::Join {
        left, right, on, ..
    } = item
    {
        walk_from_item_exprs(left, depth_base, f);
        walk_from_item_exprs(right, depth_base, f);
        if let Some(on) = on {
            visit_expr(on, depth_base, f);
        }
    }
}

impl BoundQuery {
    /// この問い合わせの中のすべての式を先行順に訪れる。`depth` は、その式が属するスコープが、この問い合わせを
    /// 含む SELECT のスコープから何段内側か（`depth_base` が基準）。
    ///
    /// 訪れるもの: CTE の本体（`depth_base`。本体の SELECT と兄弟）、本体の SELECT の filter・targets・group_by・
    /// having・`FromItem::Join.on`・`RteKind::{Values の行, Function の call}`（`depth_base`）、
    /// `RteKind::Subquery` の query（`depth_base + 1`）、集合演算の腕（`depth_base`）、VALUES の行
    /// （`depth_base`）、limit / offset（`depth_base`）。式の中の `SubLink` の query は `depth + 1` で再帰する。
    /// `order_by` は `targets` を指すだけなので式を持たない。`left_coerce` などの変換式
    /// （腕の出力行の位置を `Var` に見立てたもの）は訪れない。
    pub fn walk_exprs(&self, depth_base: u16, f: &mut dyn FnMut(&BoundExpr, u16)) {
        for cte in &self.ctes {
            cte.query.walk_exprs(depth_base, f);
        }
        match &self.body {
            BoundSetExpr::Select(s) => s.walk_exprs(depth_base, f),
            BoundSetExpr::Values { rows, .. } => {
                for e in rows.iter().flatten() {
                    visit_expr(e, depth_base, f);
                }
            }
            BoundSetExpr::SetOp { left, right, .. } => {
                left.walk_exprs(depth_base, f);
                right.walk_exprs(depth_base, f);
            }
        }
        for e in self.limit.iter().chain(self.offset.iter()) {
            visit_expr(e, depth_base, f);
        }
    }
}

// ----- 検証（B1〜B11）---------------------------------------------------------

fn bad(rule: &str, msg: impl std::fmt::Display) -> Error {
    Error::internal(format!("invalid bound query [{rule}] {msg}"))
}

/// 検証中のスコープ（rtable を持つスコープ 1 つ）。
struct ScopeInfo<'a> {
    rtable: &'a [Rte],
    /// 非 LATERAL の導出表の中から見ている間は、この SELECT の `rtable` を指せない（B11）。
    blocked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AggPolicy {
    /// 集約を含んではならない（B4）。
    Forbidden,
    /// `targets` と `having`。
    Allowed,
    /// 集約の `args` / `filter` の中（入れ子の集約は不可）。
    InsideAgg,
}

struct Validator<'a> {
    scopes: Vec<ScopeInfo<'a>>,
    /// 各 `BoundQuery` の CTE の数（`CteRef.levels_up` は `BoundQuery` を数える）。
    cte_counts: Vec<usize>,
}

impl<'a> Validator<'a> {
    fn check_var(&self, v: Var, rule_ctx: &str) -> Result<()> {
        let n = self.scopes.len();
        let up = usize::from(v.levels_up);
        if up >= n {
            return Err(bad(
                "B1",
                format!("{rule_ctx}: {v:?} refers to {up} scopes up but only {n} scopes exist"),
            ));
        }
        let scope = &self.scopes[n - 1 - up];
        if scope.blocked {
            return Err(bad(
                "B11",
                format!("{rule_ctx}: {v:?} refers to a sibling of a derived table or CTE"),
            ));
        }
        let Some(rte) = scope.rtable.get(usize::from(v.rte.0)) else {
            return Err(bad(
                "B2",
                format!(
                    "{rule_ctx}: {v:?} refers to rte {} but the scope has {}",
                    v.rte.0,
                    scope.rtable.len()
                ),
            ));
        };
        if matches!(rte.kind, RteKind::Join { .. }) {
            return Err(bad("B3", format!("{rule_ctx}: {v:?} refers to a join RTE")));
        }
        if v.is_system() {
            if !matches!(rte.kind, RteKind::Table { .. }) {
                return Err(bad(
                    "B2",
                    format!("{rule_ctx}: {v:?} is a system column of a non-table RTE"),
                ));
            }
            if v.system_column().is_none() {
                return Err(bad(
                    "B2",
                    format!("{rule_ctx}: {v:?} is not a system column"),
                ));
            }
        } else if usize::from(v.col) >= rte.columns.len() {
            return Err(bad(
                "B2",
                format!(
                    "{rule_ctx}: {v:?} refers to column {} but the RTE has {}",
                    v.col,
                    rte.columns.len()
                ),
            ));
        }
        Ok(())
    }

    /// 式 1 つ（部分木と、`SubLink` の query）を検査する。`test_cols` は `SubLink.test` の中にいるときの
    /// 副問い合わせの出力列の数。
    fn check_expr(
        &mut self,
        e: &'a BoundExpr,
        agg: AggPolicy,
        test_cols: Option<usize>,
        ctx: &str,
    ) -> Result<()> {
        match &e.kind {
            ExprKind::Column(v) => self.check_var(*v, ctx),
            ExprKind::SubLinkOutput(i) => match test_cols {
                Some(n) if usize::from(*i) < n => Ok(()),
                Some(n) => Err(bad(
                    "B5",
                    format!("{ctx}: SubLinkOutput({i}) but the subquery has {n} output columns"),
                )),
                None => Err(bad(
                    "B5",
                    format!("{ctx}: SubLinkOutput({i}) outside a SubLink test"),
                )),
            },
            ExprKind::Aggregate(call) => {
                match agg {
                    AggPolicy::Forbidden => {
                        return Err(bad(
                            "B4",
                            format!(
                                "{ctx}: aggregate {} in a clause that forbids it",
                                call.func.name
                            ),
                        ));
                    }
                    AggPolicy::InsideAgg => {
                        return Err(bad(
                            "B4",
                            format!("{ctx}: aggregate {} nested in an aggregate", call.func.name),
                        ));
                    }
                    AggPolicy::Allowed => {}
                }
                let cols: Vec<Var> = call.args.iter().flat_map(Expr::columns).collect();
                if !cols.is_empty() && cols.iter().all(|v| v.levels_up > 0) {
                    return Err(bad(
                        "B4",
                        format!(
                            "{ctx}: outer-level aggregate {} (all arguments refer to outer scopes)",
                            call.func.name
                        ),
                    ));
                }
                for a in call
                    .args
                    .iter()
                    .chain(call.filter.iter())
                    .chain(call.order_by.iter().map(|k| &k.expr))
                {
                    self.check_expr(a, AggPolicy::InsideAgg, test_cols, ctx)?;
                }
                Ok(())
            }
            ExprKind::SubLink { kind, test, query } => {
                let needs_test = matches!(kind, SubLinkKind::Any | SubLinkKind::All);
                if needs_test != test.is_some() {
                    return Err(bad(
                        "B5",
                        format!("{ctx}: SubLink {kind:?} with test = {}", test.is_some()),
                    ));
                }
                if let Some(t) = test {
                    self.check_expr(t, agg, Some(query.columns.len()), ctx)?;
                }
                self.check_query(query)
            }
            _ => {
                for c in e.children() {
                    self.check_expr(c, agg, test_cols, ctx)?;
                }
                Ok(())
            }
        }
    }

    fn check_query(&mut self, q: &'a BoundQuery) -> Result<()> {
        self.cte_counts.push(q.ctes.len());
        let r = self.check_query_inner(q);
        self.cte_counts.pop();
        r
    }

    fn check_query_inner(&mut self, q: &'a BoundQuery) -> Result<()> {
        for cte in &q.ctes {
            self.check_query(&cte.query)?;
        }
        let n_out = match &q.body {
            BoundSetExpr::Select(s) => {
                self.check_select(s)?;
                if s.n_visible > s.targets.len() {
                    return Err(bad("B6", "n_visible exceeds the number of targets"));
                }
                if q.columns.len() != s.n_visible {
                    return Err(bad(
                        "B6",
                        format!(
                            "columns.len() = {} but n_visible = {}",
                            q.columns.len(),
                            s.n_visible
                        ),
                    ));
                }
                s.targets.len()
            }
            BoundSetExpr::Values { rows, types } => {
                for row in rows {
                    if row.len() != types.len() {
                        return Err(bad("B6", "VALUES row width differs from types"));
                    }
                    // 各行は rtable が空の 1 スコープ。
                    self.scopes.push(ScopeInfo {
                        rtable: &[],
                        blocked: false,
                    });
                    let r = row
                        .iter()
                        .try_for_each(|e| self.check_expr(e, AggPolicy::Forbidden, None, "VALUES"));
                    self.scopes.pop();
                    r?;
                }
                if q.columns.len() != types.len() {
                    return Err(bad("B6", "columns.len() differs from the VALUES width"));
                }
                types.len()
            }
            BoundSetExpr::SetOp {
                left,
                right,
                left_coerce,
                right_coerce,
                types,
                ..
            } => {
                self.check_query(left)?;
                self.check_query(right)?;
                for (coerce, arm) in [(left_coerce, left), (right_coerce, right)] {
                    if let Some(c) = coerce {
                        check_coercions(c, arm.columns.len(), types.len(), "set operation")?;
                    }
                }
                if q.columns.len() != types.len() {
                    return Err(bad(
                        "B6",
                        "columns.len() differs from the set operation width",
                    ));
                }
                types.len()
            }
        };
        for k in &q.order_by {
            if k.target >= n_out {
                return Err(bad(
                    "B6",
                    format!("ORDER BY target {} >= {n_out}", k.target),
                ));
            }
        }
        // LIMIT / OFFSET: 本体の SELECT のスコープ（なければ rtable が空のスコープ）の下で解決する
        // （`levels_up = 0` は B8 で禁止。1 以上が外側）。
        let own: &'a [Rte] = match &q.body {
            BoundSetExpr::Select(s) => &s.rtable,
            _ => &[],
        };
        self.scopes.push(ScopeInfo {
            rtable: own,
            blocked: false,
        });
        let r = q.limit.iter().chain(q.offset.iter()).try_for_each(|e| {
            if e.columns().iter().any(|v| v.levels_up == 0) {
                return Err(bad(
                    "B8",
                    "LIMIT / OFFSET contains a variable of the same level",
                ));
            }
            self.check_expr(e, AggPolicy::Forbidden, None, "LIMIT / OFFSET")
        });
        self.scopes.pop();
        r
    }

    fn check_select(&mut self, s: &'a BoundSelect) -> Result<()> {
        check_from_cover(&s.rtable, &s.from, false)?;
        self.scopes.push(ScopeInfo {
            rtable: &s.rtable,
            blocked: false,
        });
        let r = self.check_select_exprs(s);
        self.scopes.pop();
        r
    }

    fn check_select_exprs(&mut self, s: &'a BoundSelect) -> Result<()> {
        self.check_rtable_and_from(&s.rtable, &s.from)?;
        if let Some(e) = &s.filter {
            self.check_expr(e, AggPolicy::Forbidden, None, "WHERE")?;
        }
        for e in &s.group_by {
            self.check_expr(e, AggPolicy::Forbidden, None, "GROUP BY")?;
        }
        if let Some(e) = &s.having {
            self.check_expr(e, AggPolicy::Allowed, None, "HAVING")?;
        }
        for e in &s.targets {
            self.check_expr(e, AggPolicy::Allowed, None, "target list")?;
        }
        let has_agg = s
            .targets
            .iter()
            .chain(s.having.iter())
            .any(Expr::contains_aggregate)
            || !s.group_by.is_empty()
            || s.having.is_some();
        if s.has_agg != has_agg {
            return Err(bad(
                "B4",
                format!("has_agg = {} but the clauses imply {has_agg}", s.has_agg),
            ));
        }
        if let BoundDistinct::On(pos) = &s.distinct
            && let Some(p) = pos.iter().find(|p| **p >= s.targets.len())
        {
            return Err(bad(
                "B6",
                format!("DISTINCT ON position {p} >= targets.len()"),
            ));
        }
        Ok(())
    }

    /// RTE の式（VALUES の行・関数呼び出し・導出表）と結合条件を検査する。
    fn check_rtable_and_from(&mut self, rtable: &'a [Rte], from: &'a [FromItem]) -> Result<()> {
        for rte in rtable {
            match &rte.kind {
                RteKind::Values { rows } => {
                    for e in rows.iter().flatten() {
                        self.check_expr(e, AggPolicy::Forbidden, None, "VALUES in FROM")?;
                    }
                }
                RteKind::Function { call } => {
                    self.check_expr(call, AggPolicy::Forbidden, None, "function in FROM")?;
                }
                RteKind::Subquery { query } => {
                    // 非 LATERAL: この SELECT の rtable（兄弟）は見えない（B11）。
                    let top = self.scopes.len() - 1;
                    let saved = std::mem::replace(&mut self.scopes[top].blocked, true);
                    let r = self.check_query(query);
                    self.scopes[top].blocked = saved;
                    r?;
                }
                RteKind::CteRef { levels_up, cte } => {
                    let n = self.cte_counts.len();
                    let up = usize::from(*levels_up);
                    if up >= n || usize::from(cte.0) >= self.cte_counts[n - 1 - up] {
                        return Err(bad(
                            "B2",
                            format!(
                                "CteRef {{ levels_up: {levels_up}, cte: {} }} is not declared",
                                cte.0
                            ),
                        ));
                    }
                }
                RteKind::Table { .. } | RteKind::Join { .. } => {}
            }
        }
        for item in from {
            self.check_from_item(item)?;
        }
        Ok(())
    }

    fn check_from_item(&mut self, item: &'a FromItem) -> Result<()> {
        if let FromItem::Join {
            left, right, on, ..
        } = item
        {
            self.check_from_item(left)?;
            self.check_from_item(right)?;
            if let Some(on) = on {
                self.check_expr(on, AggPolicy::Forbidden, None, "JOIN ON")?;
            }
        }
        Ok(())
    }

    /// B9: `rte = 0`・`levels_up = 0` のユーザー列だけを含み、`SubLink` / `Aggregate` を含まない。
    fn check_single_rel_expr(e: &BoundExpr, ncols: usize, ctx: &str) -> Result<()> {
        let mut err = None;
        e.walk(&mut |n| {
            if err.is_some() {
                return false;
            }
            match &n.kind {
                ExprKind::SubLink { .. } | ExprKind::Aggregate(_) | ExprKind::SubLinkOutput(_) => {
                    err = Some(bad(
                        "B9",
                        format!("{ctx}: subquery / aggregate is not allowed"),
                    ));
                }
                ExprKind::Column(v)
                    if v.rte.0 != 0
                        || v.levels_up != 0
                        || v.is_system()
                        || usize::from(v.col) >= ncols =>
                {
                    err = Some(bad(
                        "B9",
                        format!("{ctx}: {v:?} is not a user column of the target table"),
                    ));
                }
                _ => {}
            }
            true
        });
        err.map_or(Ok(()), Err)
    }

    /// B9（DEFAULT）: `Var` も含まない。
    fn check_no_var_expr(e: &BoundExpr, ctx: &str) -> Result<()> {
        if e.contains_aggregate() || e.contains_sublink() || !e.columns().is_empty() {
            return Err(bad(
                "B9",
                format!("{ctx}: variables, subqueries and aggregates are not allowed"),
            ));
        }
        Ok(())
    }
}

/// B10: `Var { rte: 0, col: i, levels_up: 0 }`（`i` < `source_cols`）だけを含み、`SubLink` / `Aggregate` を
/// 含まない。長さは `expected`。
fn check_coercions(c: &[BoundExpr], source_cols: usize, expected: usize, ctx: &str) -> Result<()> {
    if c.len() != expected {
        return Err(bad(
            "B10",
            format!("{ctx}: {} coercions for {expected} columns", c.len()),
        ));
    }
    for e in c {
        if e.contains_aggregate() || e.contains_sublink() {
            return Err(bad(
                "B10",
                format!("{ctx}: coercion contains a subquery or aggregate"),
            ));
        }
        if let Some(v) = e
            .columns()
            .iter()
            .find(|v| v.rte.0 != 0 || v.levels_up != 0 || usize::from(v.col) >= source_cols)
        {
            return Err(bad("B10", format!("{ctx}: coercion refers to {v:?}")));
        }
    }
    Ok(())
}

/// B7: `rtable` の非 Join の RTE は `from` の木の `Scan` のどこかにちょうど 1 回ずつ現れ、Join の RTE は
/// `FromItem::Join` の `rte` にちょうど 1 回ずつ現れる。`skip_first`: DML の `rtable[0]`（対象表）は
/// `from` に現れない。
fn check_from_cover(rtable: &[Rte], from: &[FromItem], skip_first: bool) -> Result<()> {
    fn walk(item: &FromItem, rtable: &[Rte], uses: &mut [u32]) -> Result<()> {
        match item {
            FromItem::Scan(id) => {
                let i = usize::from(id.0);
                let Some(rte) = rtable.get(i) else {
                    return Err(bad(
                        "B7",
                        format!("Scan refers to rte {i} outside the rtable"),
                    ));
                };
                if matches!(rte.kind, RteKind::Join { .. }) {
                    return Err(bad("B7", format!("Scan refers to the join RTE {i}")));
                }
                uses[i] += 1;
            }
            FromItem::Join {
                rte, left, right, ..
            } => {
                let i = usize::from(rte.0);
                let Some(r) = rtable.get(i) else {
                    return Err(bad(
                        "B7",
                        format!("Join refers to rte {i} outside the rtable"),
                    ));
                };
                if !matches!(r.kind, RteKind::Join { .. }) {
                    return Err(bad(
                        "B7",
                        format!("Join node refers to the non-join RTE {i}"),
                    ));
                }
                uses[i] += 1;
                walk(left, rtable, uses)?;
                walk(right, rtable, uses)?;
            }
        }
        Ok(())
    }
    let mut uses = vec![0u32; rtable.len()];
    for item in from {
        walk(item, rtable, &mut uses)?;
    }
    for (i, n) in uses.iter().enumerate() {
        let expected = u32::from(!(skip_first && i == 0));
        if *n != expected {
            return Err(bad(
                "B7",
                format!("rte {i} appears {n} times in the FROM tree (expected {expected})"),
            ));
        }
    }
    Ok(())
}

impl BoundQuery {
    /// B1〜B11（`m4/02-pipeline-refactor.md` §3.2.2）。違反は `Error::internal("invalid bound query [B3] ...")`
    /// （XX000）。純粋関数（副作用なし、カタログも見ない）。
    pub fn validate(&self) -> Result<()> {
        let mut v = Validator {
            scopes: Vec::new(),
            cte_counts: Vec::new(),
        };
        v.check_query(self)
    }
}

impl BoundStatement {
    /// 文に含まれるすべての問い合わせと DML の式を検査する（B1〜B11）。DDL・COPY・CHECKPOINT は検査しない。
    pub fn validate(&self) -> Result<()> {
        match self {
            BoundStatement::Select(q) => q.validate(),
            BoundStatement::Explain(e) => e.inner.validate(),
            BoundStatement::Insert(i) => i.validate(),
            BoundStatement::Update(u) => u.validate(),
            BoundStatement::Delete(d) => d.validate(),
            BoundStatement::Copy(_) | BoundStatement::Ddl(_) | BoundStatement::Checkpoint => Ok(()),
        }
    }
}

impl BoundInsert {
    /// `source` は独立した問い合わせ。`defaults` / `checks` / `returning` は B9、`coercions` は B10。
    pub fn validate(&self) -> Result<()> {
        self.source.validate()?;
        let ncols = self.table.columns.len();
        if let Some(c) = &self.coercions {
            check_coercions(
                c,
                self.source.columns.len(),
                self.source.columns.len(),
                "INSERT coercions",
            )?;
        }
        for e in self.defaults.iter().flatten() {
            Validator::check_no_var_expr(e, "INSERT default")?;
        }
        for c in &self.checks {
            Validator::check_single_rel_expr(&c.expr, ncols, "CHECK")?;
        }
        if let Some(r) = &self.returning {
            for e in &r.targets {
                Validator::check_single_rel_expr(e, ncols, "RETURNING")?;
            }
        }
        Ok(())
    }
}

/// UPDATE / DELETE の本体（`rtable[0]` が対象表）を検査する。
fn validate_dml_body<'a>(
    rtable: &'a [Rte],
    from: &'a [FromItem],
    exprs: &[(&'a BoundExpr, AggPolicy, &str)],
    returning: Option<&BoundReturning>,
) -> Result<()> {
    if !matches!(rtable.first().map(|r| &r.kind), Some(RteKind::Table { .. })) {
        return Err(bad(
            "B7",
            "rtable[0] of a DML statement must be the target table",
        ));
    }
    check_from_cover(rtable, from, true)?;
    let mut v = Validator {
        scopes: vec![ScopeInfo {
            rtable,
            blocked: false,
        }],
        cte_counts: Vec::new(),
    };
    v.check_rtable_and_from(rtable, from)?;
    for (e, policy, ctx) in exprs {
        v.check_expr(e, *policy, None, ctx)?;
    }
    if let Some(r) = returning {
        let ncols = rtable[0].columns.len();
        for e in &r.targets {
            Validator::check_single_rel_expr(e, ncols, "RETURNING")?;
        }
    }
    Ok(())
}

impl BoundUpdate {
    pub fn validate(&self) -> Result<()> {
        let mut exprs: Vec<(&BoundExpr, AggPolicy, &str)> = Vec::new();
        exprs.extend(
            self.filter
                .iter()
                .map(|e| (e, AggPolicy::Forbidden, "WHERE")),
        );
        for (_, src) in &self.assignments {
            match src {
                UpdateSource::Expr(e) => exprs.push((e, AggPolicy::Forbidden, "SET")),
                UpdateSource::Default(Some(e)) => Validator::check_no_var_expr(e, "SET DEFAULT")?,
                UpdateSource::Default(None) => {}
            }
        }
        validate_dml_body(&self.rtable, &self.from, &exprs, self.returning.as_ref())?;
        let ncols = self.rtable[0].columns.len();
        for c in &self.checks {
            Validator::check_single_rel_expr(&c.expr, ncols, "CHECK")?;
        }
        Ok(())
    }
}

impl BoundDelete {
    pub fn validate(&self) -> Result<()> {
        let exprs: Vec<(&BoundExpr, AggPolicy, &str)> = self
            .filter
            .iter()
            .map(|e| (e, AggPolicy::Forbidden, "WHERE"))
            .collect();
        validate_dml_body(&self.rtable, &self.from, &exprs, self.returning.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::table_def;
    use crate::catalog::{AggKind, BuiltinAggregate};
    use crate::expr::{CteId, RteId};
    use crate::types::{Datum, oid};

    static COUNT: BuiltinAggregate = BuiltinAggregate {
        oid: 2147,
        name: "count",
        args: &[oid::INT4],
        result: oid::INT8,
        kind: AggKind::Count,
    };

    fn var(rte: u16, col: u16, levels_up: u16) -> BoundExpr {
        Expr::column(
            Var {
                rte: RteId(rte),
                col,
                levels_up,
            },
            SqlType::INT4,
        )
    }

    fn lit(n: i32) -> BoundExpr {
        Expr::literal(Datum::Int4(n), SqlType::INT4)
    }

    fn eq(a: BoundExpr, b: BoundExpr) -> BoundExpr {
        let op = crate::catalog::builtin::operators_named("=")[0];
        Expr::new(
            ExprKind::Operator {
                op,
                args: vec![a, b],
            },
            SqlType::BOOL,
            Span::default(),
        )
    }

    fn table_rte(name: &str, ncols: u16) -> Rte {
        let columns: Vec<ColumnDef> = (1..=ncols)
            .map(|i| ColumnDef {
                name: format!("c{i}"),
                attnum: i.cast_signed(),
                ty: SqlType::INT4,
                not_null: false,
                default: None,
                identity: None,
            })
            .collect();
        let def = table_def(16384, name, columns.clone(), vec![]);
        Rte {
            kind: RteKind::Table {
                table: Arc::new(def),
            },
            refname: Some(name.into()),
            columns: columns
                .iter()
                .map(|c| RteColumn {
                    name: c.name.clone(),
                    ty: c.ty,
                })
                .collect(),
            span: Span::default(),
        }
    }

    fn out_col() -> OutputColumn {
        OutputColumn {
            name: "x".into(),
            ty: SqlType::INT4,
            table_oid: 0,
            attnum: 0,
        }
    }

    fn select_of(rtable: Vec<Rte>, from: Vec<FromItem>, targets: Vec<BoundExpr>) -> BoundSelect {
        let n = targets.len();
        BoundSelect {
            rtable,
            from,
            filter: None,
            group_by: vec![],
            having: None,
            has_agg: false,
            targets,
            n_visible: n,
            distinct: BoundDistinct::None,
        }
    }

    fn query_of(s: BoundSelect) -> BoundQuery {
        let columns = (0..s.n_visible).map(|_| out_col()).collect();
        BoundQuery {
            ctes: vec![],
            body: BoundSetExpr::Select(Box::new(s)),
            order_by: vec![],
            limit: None,
            offset: None,
            columns,
        }
    }

    /// `SELECT <targets> FROM t(2 columns)`。
    fn simple(targets: Vec<BoundExpr>) -> BoundSelect {
        select_of(
            vec![table_rte("t", 2)],
            vec![FromItem::Scan(RteId(0))],
            targets,
        )
    }

    fn rule_of(r: Result<()>) -> String {
        let e = r.expect_err("expected a validation failure");
        assert_eq!(e.sqlstate.code(), "XX000");
        e.message
    }

    fn sublink(kind: SubLinkKind, test: Option<BoundExpr>, q: BoundQuery) -> BoundExpr {
        Expr::new(
            ExprKind::SubLink {
                kind,
                test: test.map(Box::new),
                query: Box::new(q),
            },
            SqlType::BOOL,
            Span::default(),
        )
    }

    #[test]
    fn valid_queries_pass() {
        query_of(simple(vec![var(0, 0, 0), var(0, 1, 0)]))
            .validate()
            .unwrap();
        // 相関副問い合わせ: 内側の t.c1 は levels_up = 1。
        let inner = query_of(select_of(
            vec![table_rte("u", 1)],
            vec![FromItem::Scan(RteId(0))],
            vec![eq(var(0, 0, 0), var(0, 0, 1))],
        ));
        let outer = simple(vec![sublink(SubLinkKind::Exists, None, inner)]);
        query_of(outer).validate().unwrap();
        // 導出表の中から、導出表を持つ SELECT のさらに外側（levels_up = 2）は参照できる。
        let derived = query_of(select_of(vec![], vec![], vec![var(0, 0, 2)]));
        let mid = select_of(
            vec![Rte {
                kind: RteKind::Subquery {
                    query: Box::new(derived),
                },
                refname: Some("s".into()),
                columns: vec![RteColumn {
                    name: "c".into(),
                    ty: SqlType::INT4,
                }],
                span: Span::default(),
            }],
            vec![FromItem::Scan(RteId(0))],
            vec![lit(1)],
        );
        let outer = simple(vec![sublink(SubLinkKind::Scalar, None, query_of(mid))]);
        query_of(outer).validate().unwrap();
        // VALUES、LIMIT、集約。
        let mut agg = simple(vec![Expr::new(
            ExprKind::Aggregate(Box::new(AggCall {
                func: &COUNT,
                args: vec![var(0, 0, 0)],
                distinct: false,
                filter: None,
                order_by: Vec::new(),
            })),
            SqlType::INT8,
            Span::default(),
        )]);
        agg.has_agg = true;
        let mut q = query_of(agg);
        q.limit = Some(lit(5));
        q.validate().unwrap();
        let values = BoundQuery {
            ctes: vec![],
            body: BoundSetExpr::Values {
                rows: vec![vec![lit(1)], vec![lit(2)]],
                types: vec![SqlType::INT4],
            },
            order_by: vec![BoundSortKey {
                target: 0,
                descending: false,
                nulls_first: false,
            }],
            limit: None,
            offset: None,
            columns: vec![out_col()],
        };
        values.validate().unwrap();
        BoundStatement::Select(Box::new(values)).validate().unwrap();
    }

    #[test]
    fn bound_validate_rejects_each_rule() {
        // B1: 外側のスコープがないのに levels_up = 1。
        assert!(rule_of(query_of(simple(vec![var(0, 0, 1)])).validate()).contains("[B1]"));
        // B2: 列の範囲外 / rte の範囲外 / 非表のシステム列。
        assert!(rule_of(query_of(simple(vec![var(0, 5, 0)])).validate()).contains("[B2]"));
        assert!(rule_of(query_of(simple(vec![var(3, 0, 0)])).validate()).contains("[B2]"));
        // B3: Join RTE を指す Var。
        let join_rte = Rte {
            kind: RteKind::Join {
                kind: JoinType::Inner,
                left: RteId(0),
                right: RteId(1),
                sources: vec![JoinColSource::Left(0)],
            },
            refname: None,
            columns: vec![RteColumn {
                name: "c1".into(),
                ty: SqlType::INT4,
            }],
            span: Span::default(),
        };
        let s = select_of(
            vec![table_rte("a", 1), table_rte("b", 1), join_rte],
            vec![FromItem::Join {
                rte: RteId(2),
                kind: JoinType::Inner,
                left: Box::new(FromItem::Scan(RteId(0))),
                right: Box::new(FromItem::Scan(RteId(1))),
                on: None,
            }],
            vec![var(2, 0, 0)],
        );
        assert!(rule_of(query_of(s).validate()).contains("[B3]"));
        // B4: WHERE の集約 / has_agg の不一致 / 入れ子の集約 / 外側レベルの集約。
        let agg = |args: Vec<BoundExpr>| {
            Expr::new(
                ExprKind::Aggregate(Box::new(AggCall {
                    func: &COUNT,
                    args,
                    distinct: false,
                    filter: None,
                    order_by: Vec::new(),
                })),
                SqlType::INT8,
                Span::default(),
            )
        };
        let mut s = simple(vec![var(0, 0, 0)]);
        s.filter = Some(agg(vec![]));
        assert!(rule_of(query_of(s).validate()).contains("[B4]"));
        let s = simple(vec![agg(vec![var(0, 0, 0)])]); // has_agg = false のまま
        assert!(rule_of(query_of(s).validate()).contains("[B4]"));
        let mut s = simple(vec![agg(vec![agg(vec![])])]);
        s.has_agg = true;
        assert!(rule_of(query_of(s).validate()).contains("[B4]"));
        let inner = {
            let mut s = select_of(
                vec![table_rte("u", 1)],
                vec![FromItem::Scan(RteId(0))],
                vec![agg(vec![var(0, 0, 1)])],
            );
            s.has_agg = true;
            query_of(s)
        };
        let outer = simple(vec![sublink(SubLinkKind::Scalar, None, inner)]);
        assert!(rule_of(query_of(outer).validate()).contains("[B4]"));
        // B5: SubLinkOutput が test の外 / Any に test がない / Exists に test がある。
        let stray = Expr::new(ExprKind::SubLinkOutput(0), SqlType::INT4, Span::default());
        assert!(rule_of(query_of(simple(vec![stray])).validate()).contains("[B5]"));
        let sq = || query_of(select_of(vec![], vec![], vec![lit(1)]));
        let no_test = sublink(SubLinkKind::Any, None, sq());
        assert!(rule_of(query_of(simple(vec![no_test])).validate()).contains("[B5]"));
        let with_test = sublink(SubLinkKind::Exists, Some(lit(1)), sq());
        assert!(rule_of(query_of(simple(vec![with_test])).validate()).contains("[B5]"));
        let out_of_range = sublink(
            SubLinkKind::Any,
            Some(Expr::new(
                ExprKind::SubLinkOutput(4),
                SqlType::INT4,
                Span::default(),
            )),
            sq(),
        );
        assert!(rule_of(query_of(simple(vec![out_of_range])).validate()).contains("[B5]"));
        // B6: ORDER BY の位置 / columns の数 / DISTINCT ON の位置。
        let mut q = query_of(simple(vec![var(0, 0, 0)]));
        q.order_by.push(BoundSortKey {
            target: 3,
            descending: false,
            nulls_first: false,
        });
        assert!(rule_of(q.validate()).contains("[B6]"));
        let mut q = query_of(simple(vec![var(0, 0, 0)]));
        q.columns.clear();
        assert!(rule_of(q.validate()).contains("[B6]"));
        let mut s = simple(vec![var(0, 0, 0)]);
        s.distinct = BoundDistinct::On(vec![2]);
        assert!(rule_of(query_of(s).validate()).contains("[B6]"));
        // B7: FROM に現れない RTE / 2 回現れる RTE。
        let s = select_of(vec![table_rte("t", 1)], vec![], vec![lit(1)]);
        assert!(rule_of(query_of(s).validate()).contains("[B7]"));
        let s = select_of(
            vec![table_rte("t", 1)],
            vec![FromItem::Scan(RteId(0)), FromItem::Scan(RteId(0))],
            vec![lit(1)],
        );
        assert!(rule_of(query_of(s).validate()).contains("[B7]"));
        // B8: LIMIT にレベル 0 の Var / 集約。
        let mut q = query_of(simple(vec![lit(1)]));
        q.limit = Some(var(0, 0, 0));
        assert!(rule_of(q.validate()).contains("[B8]"));
        let mut q = query_of(simple(vec![lit(1)]));
        q.offset = Some(agg(vec![]));
        assert!(rule_of(q.validate()).contains("[B4]"));
        // B11: 導出表の中から兄弟（同じ FROM 句を持つ SELECT の rtable）を指す Var。
        let derived = query_of(select_of(vec![], vec![], vec![var(0, 0, 1)]));
        let s = select_of(
            vec![
                table_rte("t", 1),
                Rte {
                    kind: RteKind::Subquery {
                        query: Box::new(derived),
                    },
                    refname: Some("s".into()),
                    columns: vec![RteColumn {
                        name: "c".into(),
                        ty: SqlType::INT4,
                    }],
                    span: Span::default(),
                },
            ],
            vec![FromItem::Scan(RteId(0)), FromItem::Scan(RteId(1))],
            vec![lit(1)],
        );
        assert!(rule_of(query_of(s).validate()).contains("[B11]"));
        // CteRef が宣言されていない。
        let s = select_of(
            vec![Rte {
                kind: RteKind::CteRef {
                    levels_up: 0,
                    cte: CteId(0),
                },
                refname: None,
                columns: vec![],
                span: Span::default(),
            }],
            vec![FromItem::Scan(RteId(0))],
            vec![lit(1)],
        );
        assert!(rule_of(query_of(s).validate()).contains("[B2]"));
    }

    #[test]
    fn dml_validation_b9_b10() {
        let table = Arc::new(table_def(
            16384,
            "t",
            vec![ColumnDef {
                name: "c1".into(),
                attnum: 1,
                ty: SqlType::INT4,
                not_null: false,
                default: None,
                identity: None,
            }],
            vec![],
        ));
        let source = query_of(select_of(vec![], vec![], vec![lit(1)]));
        let ins = |defaults: Vec<Option<BoundExpr>>, coercions: Option<Vec<BoundExpr>>, checks| {
            BoundInsert {
                table: Arc::clone(&table),
                source: Box::new(source.clone()),
                coercions,
                column_map: vec![Some(0)],
                defaults,
                checks,
                overriding: None,
                returning: None,
            }
        };
        ins(vec![None], None, vec![]).validate().unwrap();
        ins(
            vec![None],
            Some(vec![var(0, 0, 0)]),
            vec![BoundCheck {
                name: "ck".into(),
                expr: eq(var(0, 0, 0), lit(1)),
            }],
        )
        .validate()
        .unwrap();
        // B9: default に Var、CHECK に別の rte。B10: coercion が出力列の外を指す / 数が違う。
        assert!(rule_of(ins(vec![Some(var(0, 0, 0))], None, vec![]).validate()).contains("[B9]"));
        let bad_check = BoundCheck {
            name: "ck".into(),
            expr: var(1, 0, 0),
        };
        assert!(rule_of(ins(vec![None], None, vec![bad_check]).validate()).contains("[B9]"));
        assert!(
            rule_of(ins(vec![None], Some(vec![var(0, 3, 0)]), vec![]).validate()).contains("[B10]")
        );
        assert!(rule_of(ins(vec![None], Some(vec![]), vec![]).validate()).contains("[B10]"));

        // UPDATE / DELETE: rtable[0] は対象表で、from に現れない。
        let del = BoundDelete {
            rtable: vec![table_rte("t", 1), table_rte("u", 1)],
            from: vec![FromItem::Scan(RteId(1))],
            filter: Some(eq(var(0, 0, 0), var(1, 0, 0))),
            returning: None,
        };
        del.validate().unwrap();
        let bad = BoundDelete {
            from: vec![],
            ..del.clone()
        };
        assert!(rule_of(bad.validate()).contains("[B7]"));
        let upd = BoundUpdate {
            rtable: vec![table_rte("t", 1)],
            from: vec![],
            filter: None,
            assignments: vec![(0, UpdateSource::Default(Some(var(0, 0, 0))))],
            checks: vec![],
            not_null: vec![false],
            returning: None,
        };
        assert!(rule_of(upd.validate()).contains("[B9]"));
    }

    #[test]
    fn returns_rows_and_walk_exprs_depths() {
        let q = query_of(simple(vec![var(0, 0, 0)]));
        assert!(BoundStatement::Select(Box::new(q.clone())).returns_rows());
        assert!(!BoundStatement::Checkpoint.returns_rows());
        // 列が 0 個の SELECT も行を返す文。
        let empty = query_of(select_of(vec![], vec![], vec![]));
        assert!(BoundStatement::Select(Box::new(empty)).returns_rows());
        let del = BoundDelete {
            rtable: vec![table_rte("t", 1)],
            from: vec![],
            filter: None,
            returning: Some(BoundReturning {
                targets: vec![],
                columns: vec![],
            }),
        };
        assert!(BoundStatement::Delete(del.clone()).returns_rows());
        let del = BoundDelete {
            returning: None,
            ..del
        };
        assert!(!BoundStatement::Delete(del).returns_rows());
        assert!(
            BoundStatement::Explain(Box::new(BoundExplain {
                options: ExplainOptions::default(),
                inner: BoundStatement::Checkpoint,
            }))
            .returns_rows()
        );

        // 深さ: 本体の式は 0、SubLink の中は 1、その中の導出表は 2。
        let derived = query_of(select_of(vec![], vec![], vec![lit(30)]));
        let inner = query_of(select_of(
            vec![Rte {
                kind: RteKind::Subquery {
                    query: Box::new(derived),
                },
                refname: None,
                columns: vec![],
                span: Span::default(),
            }],
            vec![FromItem::Scan(RteId(0))],
            vec![lit(20)],
        ));
        let outer = simple(vec![lit(10), sublink(SubLinkKind::Exists, None, inner)]);
        let mut seen = Vec::new();
        query_of(outer).walk_exprs(0, &mut |e, d| {
            if let ExprKind::Literal(Datum::Int4(n)) = &e.kind {
                seen.push((*n, d));
            }
        });
        assert_eq!(seen, vec![(10, 0), (30, 2), (20, 1)]);
    }
}
