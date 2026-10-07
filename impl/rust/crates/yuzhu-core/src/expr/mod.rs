//! 式の木（`m4/00-contracts.md` §6、`m4/02-pipeline-refactor.md` §3.1）。
//!
//! 3 つの層（Bound・論理プラン・物理プラン）の式は、この 1 つの定義 `Expr<C, Q>` から作る。
//! `C` は列参照の型（Bound: [`Var`]、論理: [`ColId`]、物理: [`PhysCol`]）、`Q` は副問い合わせの型
//! （Bound: `Box<BoundQuery>`、論理: `Box<LogicalSubquery>`、物理: [`SubPlanId`]）。型別名は
//! 定義する側のモジュール（`analyzer`、`planner::logical`、`planner::physical`）に置く。
//!
//! `expr` は `analyzer`・`planner`・`executor` のどれにも依存しない。走査と書き換えは [`walk`]。

pub mod walk;

use crate::catalog::{
    BuiltinAggregate, BuiltinFunction, BuiltinOperator, CastMethod, SystemColumn,
};
use crate::error::Span;
pub use crate::sql::ast::SessionValueKind;
use crate::types::{Datum, SqlType};

pub use walk::lower_single_rel;

/// `BoundSelect.rtable` の添字（DML では対象表が `RteId(0)`）。超えたら `54000`。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct RteId(pub u16);

/// `ColumnArena` の添字。1 つの文（副問い合わせ・CTE を含む）の中で一意。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ColId(pub u32);

/// `ExecCtx.params` の添字。超えたら `54000`。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ParamId(pub u16);

/// `PhysicalQuery.subplans` の添字。超えたら `54000`。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct SubPlanId(pub u16);

/// Bound では宣言した `BoundQuery.ctes` の添字、論理では `LogicalQuery.ctes` の添字。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct CteId(pub u16);

/// システム列を指す `Var.col` の下限。`Var.col >= SYSTEM_COL_BASE` ならシステム列
/// （`col - SYSTEM_COL_BASE` が [`system_col_index`] の添字）。
pub const SYSTEM_COL_BASE: u16 = 0x8000;

/// システム列の添字（`Ctid=0, Xmin=1, Cmin=2, Xmax=3, Cmax=4, TableOid=5`）。
pub fn system_col_index(sc: SystemColumn) -> u16 {
    match sc {
        SystemColumn::Ctid => 0,
        SystemColumn::Xmin => 1,
        SystemColumn::Cmin => 2,
        SystemColumn::Xmax => 3,
        SystemColumn::Cmax => 4,
        SystemColumn::TableOid => 5,
    }
}

/// [`system_col_index`] の逆。範囲外は `None`。
pub fn system_col_from_index(i: u16) -> Option<SystemColumn> {
    match i {
        0 => Some(SystemColumn::Ctid),
        1 => Some(SystemColumn::Xmin),
        2 => Some(SystemColumn::Cmin),
        3 => Some(SystemColumn::Xmax),
        4 => Some(SystemColumn::Cmax),
        5 => Some(SystemColumn::TableOid),
        _ => None,
    }
}

/// Bound 層の列参照（PostgreSQL の `Var`）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Var {
    pub rte: RteId,
    /// `Rte.columns` の 0 始まりの位置。システム列は `SYSTEM_COL_BASE + system_col_index(..)`。
    pub col: u16,
    /// 0 = 同じスコープ。1 以上 = 外側のスコープ（相関参照）。数えるのは「rtable を持つスコープ」の
    /// 入れ子: `BoundSelect`・DML・`BoundSetExpr::Values` の各行。`BoundQuery` は数えない
    /// （集合演算の腕・CTE 本体は兄弟）。11 §7.1 の C-3。
    pub levels_up: u16,
}

impl Var {
    /// 同じスコープのユーザー列。
    pub fn user(rte: RteId, col: u16) -> Var {
        Var {
            rte,
            col,
            levels_up: 0,
        }
    }

    /// 同じスコープのシステム列。
    pub fn system(rte: RteId, sc: SystemColumn) -> Var {
        Var {
            rte,
            col: SYSTEM_COL_BASE + system_col_index(sc),
            levels_up: 0,
        }
    }

    /// システム列ならその種類。
    pub fn system_column(&self) -> Option<SystemColumn> {
        if self.col >= SYSTEM_COL_BASE {
            system_col_from_index(self.col - SYSTEM_COL_BASE)
        } else {
            None
        }
    }

    /// `levels_up` を `n` に直した `Var`（副問い合わせの外側参照を作る）。
    #[must_use]
    pub fn with_levels_up(self, n: u16) -> Var {
        Var {
            levels_up: n,
            ..self
        }
    }

    pub fn is_system(&self) -> bool {
        self.col >= SYSTEM_COL_BASE
    }

    /// 現在のレベルの `Var` か（`levels_up == 0`）。
    pub fn is_local(&self) -> bool {
        self.levels_up == 0
    }
}

/// 物理層の列参照。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PhysCol {
    /// 評価中の行の位置。
    Local(usize),
    /// 実行時パラメータ（相関サブクエリ・`NestedLoopParam` が外側の値を渡す）。
    Param(ParamId),
}

/// `IS [NOT] TRUE | FALSE | UNKNOWN`（NULL を返さない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolTestKind {
    IsTrue,
    IsNotTrue,
    IsFalse,
    IsNotFalse,
    IsUnknown,
    IsNotUnknown,
}

/// 型つきの式。`Span` はエラー位置（論理・物理の層では `Span::default()` でよい）。
#[derive(Clone, Debug)]
pub struct Expr<C, Q> {
    pub kind: ExprKind<C, Q>,
    pub ty: SqlType,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind<C, Q> {
    // ---- M1〜M3 から引き継ぐ変種 ----
    /// 定数（ノードの型に変換済み）。
    Literal(Datum),
    /// 列参照。`C` = `Var`（Bound）/ `ColId`（論理）/ `PhysCol`（物理）。
    Column(C),
    /// 演算子の呼び出し（オペランドは演算子の引数型に変換済み。前置演算子は引数 1 つ）。
    Operator {
        op: &'static BuiltinOperator,
        args: Vec<Expr<C, Q>>,
    },
    /// 関数の呼び出し（引数は変換済み。NULL の短絡は `func.strict`）。
    Function {
        func: &'static BuiltinFunction,
        args: Vec<Expr<C, Q>>,
    },
    /// `ty.oid` への型変換。typmod は別の `CoerceTypmod` が上に付く。strict。
    /// `implicit`: アナライザが暗黙のキャストを挿入したとき true（deparse が根の暗黙のキャストを隠す。
    /// `same_as` は無視して比べる）。`CastMethod::Env` は定数畳み込みしない（11 §7.1 の C-14）。
    Cast {
        expr: Box<Expr<C, Q>>,
        method: CastMethod,
        implicit: bool,
    },
    /// `ty.typmod` への長さ・精度の変換。`explicit` = 黙って切り詰める。
    CoerceTypmod {
        expr: Box<Expr<C, Q>>,
        explicit: bool,
    },
    /// 三値論理の AND / OR（2 個以上。0 個・1 個は作らない。`Expr::and_all` を使う）。
    And(Vec<Expr<C, Q>>),
    Or(Vec<Expr<C, Q>>),
    Not(Box<Expr<C, Q>>),
    IsNull(Box<Expr<C, Q>>),
    IsNotNull(Box<Expr<C, Q>>),
    BoolTest {
        expr: Box<Expr<C, Q>>,
        test: BoolTestKind,
    },
    /// 検索 CASE（単純 CASE はアナライザが条件に展開済み）。ELSE なしは NULL。
    Case {
        arms: Vec<(Expr<C, Q>, Expr<C, Q>)>,
        else_result: Option<Box<Expr<C, Q>>>,
    },
    /// 最初の非 NULL（遅延評価）。1 個以上。
    Coalesce(Vec<Expr<C, Q>>),
    /// `left = right`（`eq_op`）なら NULL、そうでなければ `left`。
    NullIf {
        left: Box<Expr<C, Q>>,
        right: Box<Expr<C, Q>>,
        eq_op: &'static BuiltinOperator,
    },
    /// `x IS [NOT] DISTINCT FROM y`（`eq_op` で NULL 安全に比較。NULL を返さない）。
    /// 00 §6.2 の変種一覧には無いが、M3 の `BoundExprKind::DistinctFrom` を引き継ぐ。
    DistinctFrom {
        left: Box<Expr<C, Q>>,
        right: Box<Expr<C, Q>>,
        eq_op: &'static BuiltinOperator,
        negated: bool,
    },
    /// `GREATEST` / `LEAST`（NULL の引数は無視。すべて NULL のときだけ NULL）。`cmp` は共通型の
    /// `>`（greatest）または `<`（least）。00 §6.2 に無いが M3 の `BoundExprKind::MinMax` を引き継ぐ。
    MinMax {
        greatest: bool,
        args: Vec<Expr<C, Q>>,
        cmp: &'static BuiltinOperator,
    },
    /// `expr [NOT] LIKE pattern`（C 照合順序。ILIKE は大文字小文字を畳む。`escape` の既定は `\`）。
    Like {
        expr: Box<Expr<C, Q>>,
        pattern: Box<Expr<C, Q>>,
        escape: Option<Box<Expr<C, Q>>>,
        negated: bool,
        case_insensitive: bool,
    },
    /// `expr [NOT] IN (list)`（共通型にそろえて `eq_op` で比較。一致なしで NULL があれば NULL）。
    InList {
        expr: Box<Expr<C, Q>>,
        list: Vec<Expr<C, Q>>,
        eq_op: &'static BuiltinOperator,
        negated: bool,
    },
    /// セッションの値（`current_user`、`CURRENT_TIMESTAMP` など）。
    SessionValue(SessionValueKind),
    // ---- M4 の新しい変種 ----
    /// 集約の呼び出し。Bound にだけ現れる（論理プランへの変換で `Aggregate` ノードの出力列の
    /// `Column(ColId)` に置き換わる）。
    Aggregate(Box<AggCall<C, Q>>),
    /// 副問い合わせ式。`query` の型は層ごとに違う。`test` は Any / All で必須（`SubLinkOutput(i)` を含む）。
    SubLink {
        kind: SubLinkKind,
        test: Option<Box<Expr<C, Q>>>,
        query: Q,
    },
    /// `SubLink.test` の中でだけ使う: 副問い合わせの現在の行の i 番目の出力列。
    SubLinkOutput(u16),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubLinkKind {
    /// `(SELECT ...)` 1 行 1 列。0 行は NULL、2 行以上は 21000。
    Scalar,
    /// `EXISTS (SELECT ...)`。
    Exists,
    /// 「副問い合わせのある行について test が true」の三値論理 OR。`x IN (SELECT ...)` は Any
    /// （test = `x = SubLinkOutput(0)`）、`x NOT IN (...)` は `Not(Any)`。
    Any,
    /// 「すべての行について test が true」の三値論理 AND。`x <> ALL (SELECT ...)` など。
    All,
}

/// 集約の呼び出し。
#[derive(Clone, Debug)]
pub struct AggCall<C, Q> {
    pub func: &'static BuiltinAggregate,
    /// `count(*)` は空。
    pub args: Vec<Expr<C, Q>>,
    pub distinct: bool,
    pub filter: Option<Expr<C, Q>>,
    /// `agg(args ORDER BY ...)`。空なら入力順。
    pub order_by: Vec<AggOrderKey<C, Q>>,
}

/// 集約呼び出しの ORDER BY の 1 キー。
#[derive(Clone, Debug)]
pub struct AggOrderKey<C, Q> {
    pub expr: Expr<C, Q>,
    pub descending: bool,
    pub nulls_first: bool,
}

impl<C, Q> Expr<C, Q> {
    pub fn new(kind: ExprKind<C, Q>, ty: SqlType, span: Span) -> Self {
        Expr { kind, ty, span }
    }

    /// `span = Span::default()` の定数。
    pub fn literal(d: Datum, ty: SqlType) -> Self {
        Expr::new(ExprKind::Literal(d), ty, Span::default())
    }

    /// `span = Span::default()` の列参照。
    pub fn column(c: C, ty: SqlType) -> Self {
        Expr::new(ExprKind::Column(c), ty, Span::default())
    }

    /// 型つきの NULL。
    pub fn null_of(ty: SqlType) -> Self {
        Expr::literal(Datum::Null, ty)
    }

    pub fn bool_lit(b: bool) -> Self {
        Expr::literal(Datum::Bool(b), SqlType::BOOL)
    }

    /// 0 個 → `bool_lit(true)`、1 個 → そのまま、2 個以上 → `And`（子の `And` は 1 段に平らにする）。
    /// 定数畳み込みはしない。
    pub fn and_all(parts: Vec<Self>) -> Self {
        let mut flat: Vec<Self> = Vec::with_capacity(parts.len());
        for p in parts {
            match p.kind {
                ExprKind::And(inner) => flat.extend(inner),
                kind => flat.push(Expr { kind, ..p }),
            }
        }
        match flat.len() {
            0 => Expr::bool_lit(true),
            1 => flat.remove(0),
            _ => Expr::new(ExprKind::And(flat), SqlType::BOOL, Span::default()),
        }
    }
}
