//! 式 → SQL テキスト（EXPLAIN と `pg_get_expr` が共有する。`m4/10-explain-copy-compat.md` §4.1）。
//!
//! 依存: `deparse::{mod, expr, literal, ident, typename}` は `expr` と `catalog` だけに依存し、
//! `stored` だけが `analyzer` に依存する（00 §4.1 の例外 2）。P0-b の時点では型と、`0A000`
//! （`deparse is not supported yet`）を返す入口だけ。

pub mod expr;
pub mod ident;
pub mod literal;
pub mod stored;
pub mod typename;

use crate::catalog::CatalogReader;
use crate::error::Result;
use crate::expr::Expr;
use crate::types::TypeEnv;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeparseMode {
    /// 計画後の式（EXPLAIN）。定数畳み込み済みの形を前提にする。
    Plan,
    /// 保存された式（`pg_get_expr`、制約の定義）。解析しただけで、畳み込んでいない。
    Stored,
}

#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct DeparseOptions {
    pub mode: DeparseMode,
    /// `PRETTYFLAG_PAREN`: 括弧を最小にする（psql の `\d` が使う）。
    pub pretty_paren: bool,
    /// `PRETTYFLAG_INDENT`: `CASE` を複数行にする。
    pub indent: bool,
}

impl DeparseOptions {
    /// EXPLAIN: 括弧は常に付ける、字下げなし（1 行）。
    pub const EXPLAIN: DeparseOptions = DeparseOptions {
        mode: DeparseMode::Plan,
        pretty_paren: false,
        indent: false,
    };

    /// `pg_get_expr(.., pretty)` と `pg_get_constraintdef(.., pretty)`。`pretty = false` でも `indent` は true。
    pub const fn stored(pretty: bool) -> DeparseOptions {
        DeparseOptions {
            mode: DeparseMode::Stored,
            pretty_paren: pretty,
            indent: true,
        }
    }
}

/// 列参照の表示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColText {
    pub text: String,
    pub wrap: bool,
}

/// 列の型 `C` ごとに 1 つ実装する。修飾するかどうかの判断は実装が持つ。
pub trait ColumnNamer<C> {
    fn name(&self, col: &C) -> Result<ColText>;
}

/// `SubLink` の副問い合わせ `Q` の表示用の名前。`Stored` では使わない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubPlanLabel {
    /// `SubPlan 1` / `InitPlan 1`。
    pub name: String,
    /// ハッシュ化した `SubPlan`（`(hashed SubPlan 1)`）。
    pub hashed: bool,
    /// `InitPlan` か（`(InitPlan 1).col1` の形にする）。
    pub init_plan: bool,
}

pub trait SubLinkRenderer<Q> {
    fn label(&self, q: &Q) -> Result<SubPlanLabel>;
}

pub struct DeparseCtx<'a, C, Q> {
    pub opts: DeparseOptions,
    pub namer: &'a dyn ColumnNamer<C>,
    pub sublinks: Option<&'a dyn SubLinkRenderer<Q>>,
    /// 定数の出力（DateStyle、`extra_float_digits`）に使う。
    pub type_env: &'a TypeEnv<'a>,
    /// `regclass` 定数の名前、関数名の可視性に使う。
    pub catalog: &'a dyn CatalogReader,
}

impl<C, Q> std::fmt::Debug for DeparseCtx<'_, C, Q> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeparseCtx")
            .field("opts", &self.opts)
            .finish_non_exhaustive()
    }
}

/// 式 1 つを文字列にする。根の式として扱う。
pub fn deparse_expr<C: Clone, Q: Clone>(
    e: &Expr<C, Q>,
    cx: &DeparseCtx<'_, C, Q>,
) -> Result<String> {
    let mut d = expr::Deparser::new(cx);
    d.root(e)?;
    Ok(d.finish())
}

/// 式の並び（Group Key、Sort Key、Output）を要素ごとに文字列にする。
pub fn deparse_list<C: Clone, Q: Clone>(
    es: &[Expr<C, Q>],
    cx: &DeparseCtx<'_, C, Q>,
) -> Result<Vec<String>> {
    es.iter().map(|e| deparse_expr(e, cx)).collect()
}

#[cfg(test)]
mod tests;
