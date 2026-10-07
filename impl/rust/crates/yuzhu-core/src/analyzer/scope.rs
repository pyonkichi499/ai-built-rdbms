//! 名前解決のスコープ（`m4/03-parser-analyzer.md` §3.3、§5.2、`m4/02-pipeline-refactor.md` §3.4.2）。
//!
//! スコープは [`ScopeStack`]（[`ScopeFrame`] の列。末尾が現在のレベル）。1 つのフレームが 1 つの
//! `BoundSelect`（または UPDATE / DELETE、VALUES の行）の名前空間で、`rels` が FROM 句の項目
//! （各 [`NsItem`] が [`ScopeRel`] と可視性の旗を持つ）。名前解決の結果は式（普通は
//! `Var { rte, col, levels_up }`、結合の併合列は子の列の式）。`levels_up` は**フレームを数える**
//! （11 §7.1 の C-3。`BoundQuery` は数えない）。
//!
//! 結合（JOIN）の RTE を指す `Var` は作らない（02 の D2）。結合の列は `NsItem.join_exprs`（子の列の式。
//! `levels_up = 0` の鋳型）に展開済みで、解決のたびに `levels_up` を直して返す。

use super::bound::BoundExpr;
use super::cte::CteScope;
use crate::catalog::SystemColumn;
use crate::error::{Error, Result, Span, sqlstate};
use crate::expr::{ExprKind, RteId, Var};
use crate::sql::ast::Ident;
use crate::types::{Oid, SqlType};

#[derive(Debug, Clone)]
pub(super) struct ScopeColumn {
    pub(super) name: String,
    pub(super) ty: SqlType,
    /// attnum in the base table (0 if not a base table column).
    pub(super) attnum: i16,
}

/// A FROM item visible to expressions (PostgreSQL's `ParseNamespaceItem`).
#[derive(Debug, Clone)]
pub(super) struct ScopeRel {
    /// `BoundSelect.rtable` の添字。
    pub(super) rte: RteId,
    /// The reference name: alias if given, else the table name.
    pub(super) refname: String,
    /// The table name hidden by an alias (for the "invalid reference" error).
    pub(super) hidden_name: Option<String>,
    /// Schema of the table when referenced without an alias (allows
    /// `schema.table.column`).
    pub(super) schema: Option<String>,
    /// Base table OID (0 for VALUES or a table being created).
    pub(super) table_oid: Oid,
    pub(super) columns: Vec<ScopeColumn>,
    /// システム列を参照できるか（SELECT / UPDATE / DELETE の実表）。
    pub(super) system_columns: bool,
}

/// 名前空間の 1 項目（`ParseNamespaceItem`）。[`ScopeRel`] に可視性の旗と、結合・派生表の付随情報を足す。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub(super) struct NsItem {
    pub(super) rel: ScopeRel,
    /// 修飾名 `t.a` の `t` に使えるか。別名なしの JOIN・派生表・VALUES は false。
    pub(super) rel_visible: bool,
    /// 修飾なしの列名の探索対象か。JOIN の子の項目は false（結合 RTE 自身が true）。
    pub(super) cols_visible: bool,
    /// 同じ FROM 句の左にある項目（右辺の副問い合わせ・関数・JOIN の ON から見たとき）。参照すると
    /// 診断のエラーになる（LATERAL は 0A000）。名前解決の対象から外れる。
    pub(super) lateral_only: bool,
    /// `lateral_only` の項目が「LATERAL を付ければ参照できる」位置にあるか（HINT を出すかだけに使う）。
    pub(super) lateral_ok: bool,
    /// 結合 RTE の項目だけ: 列ごとの展開式（子の列。`levels_up = 0` の鋳型）。
    pub(super) join_exprs: Option<Vec<BoundExpr>>,
    /// 出力列の由来 `(table_oid, attnum)`（派生表・CTE の列。空なら `rel.table_oid` と `ScopeColumn.attnum`）。
    pub(super) origins: Vec<(Oid, i16)>,
}

impl NsItem {
    /// 名前も列も見える、普通の項目。
    pub(super) fn new(rel: ScopeRel) -> Self {
        NsItem {
            rel,
            rel_visible: true,
            cols_visible: true,
            lateral_only: false,
            lateral_ok: false,
            join_exprs: None,
            origins: Vec::new(),
        }
    }
}

impl From<ScopeRel> for NsItem {
    fn from(rel: ScopeRel) -> Self {
        NsItem::new(rel)
    }
}

/// `ScopeStack::single` / `with_frame` が受け取る項目の並び（`ScopeRel` の列も `NsItem` の列も渡せる）。
pub(super) trait IntoNs {
    fn into_ns(self) -> Vec<NsItem>;
}

impl IntoNs for Vec<NsItem> {
    fn into_ns(self) -> Vec<NsItem> {
        self
    }
}

impl IntoNs for Vec<ScopeRel> {
    fn into_ns(self) -> Vec<NsItem> {
        self.into_iter().map(NsItem::new).collect()
    }
}

/// What kind of clause an expression appears in (PostgreSQL's
/// `ParseExprKind`); affects which references are allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ParseExprKind {
    SelectTarget,
    Where,
    /// Right-hand side of UPDATE SET.
    UpdateSet,
    OrderBy,
    Limit,
    Offset,
    Values,
    /// Column DEFAULT: column references are not allowed (0A000).
    ColumnDefault,
    Check,
    /// `JOIN ... ON`（`m4/03` §5.3.2）。
    JoinOn,
    /// FROM 句の関数の引数（暗黙の LATERAL。§5.4.2）。
    FromFunction,
    /// `RETURNING`（§5.10.2）。
    Returning,
    Having,
    GroupBy,
    DistinctOn,
    Filter,
}

impl ParseExprKind {
    /// Name used in messages such as `ORDER BY "x" is ambiguous`.
    pub(super) fn name(self) -> &'static str {
        match self {
            ParseExprKind::SelectTarget => "SELECT",
            ParseExprKind::Where => "WHERE",
            ParseExprKind::UpdateSet => "UPDATE",
            ParseExprKind::OrderBy => "ORDER BY",
            ParseExprKind::Limit => "LIMIT",
            ParseExprKind::Offset => "OFFSET",
            ParseExprKind::Values => "VALUES",
            ParseExprKind::ColumnDefault => "DEFAULT",
            ParseExprKind::Check => "CHECK",
            ParseExprKind::JoinOn => "JOIN/ON",
            ParseExprKind::FromFunction => "FROM function",
            ParseExprKind::Returning => "RETURNING",
            ParseExprKind::Having => "HAVING",
            ParseExprKind::GroupBy => "GROUP BY",
            ParseExprKind::DistinctOn => "DISTINCT ON",
            ParseExprKind::Filter => "FILTER",
        }
    }
}

/// 1 つのスコープ（`BoundSelect` / DML / VALUES の行）の名前空間。
#[derive(Debug, Clone, Default)]
pub(super) struct ScopeFrame {
    pub(super) rels: Vec<NsItem>,
}

/// FROM 句の解析中の状態（RTE と名前空間。`RteId` は `rtable` の添字。03 §4.5）。
#[derive(Debug, Default)]
pub(super) struct FromBuilder {
    pub(super) rtable: Vec<crate::analyzer::bound::Rte>,
    pub(super) ns: Vec<NsItem>,
}

/// フレームの列。末尾が現在のレベル（`levels_up = 0`）。
#[derive(Debug, Clone, Default)]
pub(super) struct ScopeStack {
    frames: Vec<ScopeFrame>,
    /// FROM 句の関数の引数を解析している間は true（暗黙の LATERAL。`lateral_only` の項目に当たったら
    /// 0A000。D3-6）。
    lateral_active: bool,
}

/// 問い合わせ 1 つ（`BoundQuery`）の解析環境（`m4/03` §3.3）。
#[derive(Clone, Copy)]
pub(super) struct QueryEnv<'a> {
    /// 相関参照の解決先: この問い合わせの本体の SELECT から見て `levels_up = 1` 以降のフレーム。
    pub(super) outer: Option<&'a ScopeStack>,
    /// この `BoundQuery` の 1 つ外側の CTE スコープ。
    pub(super) ctes: &'a CteScope<'a>,
    /// unknown 型の出力列を text にするか（`INSERT ... SELECT` は false。D3-15）。
    pub(super) resolve_unknowns: bool,
}

impl<'a> QueryEnv<'a> {
    /// 最上位の問い合わせの環境（外側のフレームなし）。
    pub(super) fn root(resolve_unknowns: bool, ctes: &'a CteScope<'a>) -> Self {
        QueryEnv {
            outer: None,
            ctes,
            resolve_unknowns,
        }
    }
}

/// `*` / `t.*` の展開で出る列 1 つ。
#[derive(Debug, Clone)]
pub(super) struct StarColumn {
    pub(super) name: String,
    pub(super) expr: BoundExpr,
    /// 出力列の由来 `(table_oid, attnum)`。
    pub(super) origin: (Oid, i16),
}

/// システム列の名前（`SYSTEM_COLUMNS` と同じ並び）。
const SYSTEM_COLUMN_KINDS: [SystemColumn; 6] = [
    SystemColumn::Ctid,
    SystemColumn::Xmin,
    SystemColumn::Cmin,
    SystemColumn::Xmax,
    SystemColumn::Cmax,
    SystemColumn::TableOid,
];

fn col_index(i: usize) -> Result<u16> {
    u16::try_from(i)
        .ok()
        .filter(|c| *c < crate::expr::SYSTEM_COL_BASE)
        .ok_or_else(|| {
            Error::new(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many columns in a range table entry",
            )
        })
}

/// 式の中のすべての `Var` の `levels_up` を `n` だけ増やし、`span` を付ける（展開式の鋳型を使うとき）。
fn shift_levels(e: &BoundExpr, n: u16, span: Span) -> BoundExpr {
    let mut out = if n == 0 {
        e.clone()
    } else {
        e.try_rewrite(&mut |x| {
            Ok(match &x.kind {
                ExprKind::Column(v) => Some(BoundExpr::new(
                    ExprKind::Column(v.with_levels_up(v.levels_up + n)),
                    x.ty,
                    x.span,
                )),
                _ => None,
            })
        })
        .unwrap_or_else(|_| e.clone())
    };
    out.span = span;
    out
}

/// 項目の中で名前に一致する列（ユーザー列がすべて。なければシステム列）。`levels_up = 0` の式。
fn item_columns(item: &NsItem, name: &str) -> Result<Vec<BoundExpr>> {
    let mut out = Vec::new();
    for (i, c) in item.rel.columns.iter().enumerate() {
        if c.name != name {
            continue;
        }
        out.push(match &item.join_exprs {
            Some(exprs) => exprs
                .get(i)
                .cloned()
                .ok_or_else(|| Error::internal("join column without an expansion"))?,
            None => BoundExpr::column(Var::user(item.rel.rte, col_index(i)?), c.ty),
        });
    }
    if out.is_empty()
        && item.rel.system_columns
        && let Some(e) = system_column(&item.rel, name)
    {
        out.push(e);
    }
    Ok(out)
}

/// システム列（`ctid` など）の式。
fn system_column(rel: &ScopeRel, name: &str) -> Option<BoundExpr> {
    let (kind, (_, _, type_oid)) = SYSTEM_COLUMN_KINDS
        .into_iter()
        .zip(crate::catalog::schema::SYSTEM_COLUMNS)
        .find(|(_, (n, _, _))| *n == name)?;
    Some(BoundExpr::column(
        Var::system(rel.rte, kind),
        SqlType::of(type_oid),
    ))
}

/// 項目が名前の列を持つか（参照できない位置でも。診断用）。
fn has_column(item: &NsItem, name: &str) -> bool {
    item.rel.columns.iter().any(|c| c.name == name)
        || (item.rel.system_columns && system_column(&item.rel, name).is_some())
}

impl NsItem {
    /// 修飾名（`t` または `schema.t`）がこの項目を指すか（名前で見える項目だけ）。
    fn qualifier_matches(&self, qual: &[Ident]) -> bool {
        if !self.rel_visible {
            return false;
        }
        match qual {
            [t] => t.value == self.rel.refname,
            [s, t] => {
                self.rel.schema.as_deref() == Some(s.value.as_str()) && t.value == self.rel.refname
            }
            _ => false,
        }
    }

    /// 名前解決の対象か（左の項目は LATERAL なしでは参照できない）。
    fn usable(&self) -> bool {
        !self.lateral_only
    }
}

fn limit_err() -> Error {
    Error::new(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many nested scopes")
}

const IMPLICIT_LATERAL: &str = "implicit LATERAL reference in a function in FROM";

impl ScopeStack {
    /// 空のフレーム 1 つ（FROM なし）。
    pub(super) fn empty() -> Self {
        ScopeStack {
            frames: vec![ScopeFrame::default()],
            lateral_active: false,
        }
    }

    /// 1 つのフレーム。
    pub(super) fn single(rels: impl IntoNs) -> Self {
        ScopeStack {
            frames: vec![ScopeFrame {
                rels: rels.into_ns(),
            }],
            lateral_active: false,
        }
    }

    /// 外側のフレームの列に、現在のフレームを足した写し。
    #[must_use]
    pub(super) fn with_frame(&self, rels: impl IntoNs) -> Self {
        let mut s = self.clone();
        s.frames.push(ScopeFrame {
            rels: rels.into_ns(),
        });
        s
    }

    /// FROM 句の関数の引数用（暗黙の LATERAL を 0A000 にする）。
    #[must_use]
    pub(super) fn with_lateral_active(mut self) -> Self {
        self.lateral_active = true;
        self
    }

    /// 現在のフレーム。
    fn current(&self) -> Option<&ScopeFrame> {
        self.frames.last()
    }

    /// `Var` が指す項目（`levels_up` を見て外側のフレームも探す）。
    fn item_of(&self, var: Var) -> Option<&NsItem> {
        let n = self.frames.len();
        let up = usize::from(var.levels_up);
        let frame = self.frames.get(n.checked_sub(1 + up)?)?;
        frame.rels.iter().find(|r| r.rel.rte == var.rte)
    }

    /// 出力列の由来（`markTargetListOrigin`。実表の列と、派生表・CTE を通した列。システム列は負の
    /// attnum）。
    pub(super) fn origin(&self, var: Var) -> (Oid, i16) {
        let Some(item) = self.item_of(var) else {
            return (0, 0);
        };
        if let Some(sc) = var.system_column() {
            if item.rel.table_oid == 0 {
                return (0, 0);
            }
            let i = usize::from(crate::expr::system_col_index(sc));
            let attnum = crate::catalog::schema::SYSTEM_COLUMNS
                .get(i)
                .map_or(0, |(_, a, _)| *a);
            return (item.rel.table_oid, attnum);
        }
        if !item.origins.is_empty() {
            return item
                .origins
                .get(usize::from(var.col))
                .copied()
                .unwrap_or((0, 0));
        }
        if item.rel.table_oid == 0 {
            return (0, 0);
        }
        item.rel
            .columns
            .get(usize::from(var.col))
            .map_or((0, 0), |c| (item.rel.table_oid, c.attnum))
    }

    /// 式が `Var` 1 つなら、その由来（結合の併合列で展開した式が `Var` のときも）。
    pub(super) fn origin_of_expr(&self, e: &BoundExpr) -> (Oid, i16) {
        match &e.kind {
            ExprKind::Column(v) => self.origin(*v),
            _ => (0, 0),
        }
    }

    /// 参照できない項目（左の項目・結合の子の列）に同名の列があるときの診断（DETAIL / HINT）。
    fn column_diag(&self, name: &str, mut e: Error) -> Error {
        let mut lateral: Vec<&NsItem> = Vec::new();
        let mut hidden: Vec<&NsItem> = Vec::new();
        for item in self.frames.iter().rev().flat_map(|f| f.rels.iter()) {
            if !has_column(item, name) {
                continue;
            }
            if item.lateral_only {
                lateral.push(item);
            } else if !item.cols_visible && item.rel_visible {
                hidden.push(item);
            }
        }
        if let Some(first) = lateral.first() {
            e = if lateral.len() == 1 {
                e.with_detail(format!(
                    "There is a column named \"{name}\" in table \"{}\", but it cannot be referenced from this part of the query.",
                    first.rel.refname
                ))
            } else {
                e.with_detail(format!(
                    "There are columns named \"{name}\", but they are in tables that cannot be referenced from this part of the query."
                ))
            };
            if lateral.iter().any(|i| i.lateral_ok) {
                e = e.with_hint(
                    "To reference that column, you must mark this subquery with LATERAL.",
                );
            }
        } else if let Some(first) = hidden.first() {
            e = if hidden.len() == 1 {
                e.with_detail(format!(
                    "There is a column named \"{name}\" in table \"{}\", but it cannot be referenced from this part of the query.",
                    first.rel.refname
                ))
            } else {
                e.with_detail(format!(
                    "There are columns named \"{name}\", but they are in tables that cannot be referenced from this part of the query."
                ))
            };
            e = e.with_hint("Try using a table-qualified name.");
        }
        e
    }

    /// 修飾名の表が見つからない（または参照できない）ときのエラー（`errorMissingRTE`）。
    fn missing_from(&self, qual: &[Ident], span: Span) -> Error {
        let t = qual.last().map_or("", |i| i.value.as_str());
        let items = || self.frames.iter().rev().flat_map(|f| f.rels.iter());
        if let Some(item) = items().find(|r| r.lateral_only && r.qualifier_matches(qual)) {
            if self.lateral_active {
                return super::not_supported(IMPLICIT_LATERAL, span);
            }
            let mut e = Error::new(
                sqlstate::UNDEFINED_TABLE,
                format!("invalid reference to FROM-clause entry for table \"{t}\""),
            )
            .with_detail(format!(
                "There is an entry for table \"{t}\", but it cannot be referenced from this part of the query."
            ))
            .with_span(span);
            if item.lateral_ok {
                e = e.with_hint(
                    "To reference that table, you must mark this subquery with LATERAL.",
                );
            }
            return e;
        }
        let hidden_match = |r: &&NsItem| match qual {
            [q] | [_, q] => r.rel_visible && r.rel.hidden_name.as_deref() == Some(q.value.as_str()),
            _ => false,
        };
        if let Some(rel) = items().find(hidden_match) {
            return Error::new(
                sqlstate::UNDEFINED_TABLE,
                format!("invalid reference to FROM-clause entry for table \"{t}\""),
            )
            .with_hint(format!(
                "Perhaps you meant to reference the table alias \"{}\".",
                rel.rel.refname
            ))
            .with_span(span);
        }
        Error::new(
            sqlstate::UNDEFINED_TABLE,
            format!("missing FROM-clause entry for table \"{t}\""),
        )
        .with_span(span)
    }

    fn ambiguous(name: &str, span: Span) -> Error {
        Error::new(
            sqlstate::AMBIGUOUS_COLUMN,
            format!("column reference \"{name}\" is ambiguous"),
        )
        .with_span(span)
    }

    /// Resolves `col`, `t.col` or `schema.t.col` to a column expression (`Var`、結合の併合列は展開した式)。
    /// 現在のフレームから外側へ探す。`span` は列参照の先頭からの範囲（エラーの位置）。
    pub(super) fn resolve_column(&self, parts: &[Ident], span: Span) -> Result<BoundExpr> {
        let Some((colname, qual)) = parts.split_last() else {
            return Err(Error::internal("empty column reference"));
        };
        if parts.len() > 3 {
            let name: Vec<&str> = parts.iter().map(|p| p.value.as_str()).collect();
            return Err(Error::syntax_at(
                span,
                format!(
                    "improper qualified name (too many dotted names): {}",
                    name.join(".")
                ),
            ));
        }
        let name = colname.value.as_str();
        for (up, frame) in self.frames.iter().rev().enumerate() {
            let levels_up = u16::try_from(up).map_err(|_| limit_err())?;
            if qual.is_empty() {
                if let Some(e) = Self::find_unqualified(frame, name, span)? {
                    return Ok(shift_levels(&e, levels_up, span));
                }
                continue;
            }
            let Some(item) = frame
                .rels
                .iter()
                .find(|r| r.usable() && r.qualifier_matches(qual))
            else {
                continue;
            };
            let mut hits = item_columns(item, name)?;
            return match hits.len() {
                0 => Err(Error::new(
                    sqlstate::UNDEFINED_COLUMN,
                    format!(
                        "column {}.{} does not exist",
                        qual.last().map_or("", |q| q.value.as_str()),
                        name
                    ),
                )
                .with_span(span)),
                1 => Ok(shift_levels(&hits.remove(0), levels_up, span)),
                _ => Err(Self::ambiguous(name, span)),
            };
        }
        if !qual.is_empty() {
            return Err(self.missing_from(qual, span));
        }
        Err(self.missing_column(name, span))
    }

    /// 修飾なしの列が見つからなかったときのエラー（暗黙の LATERAL・全行参照・診断つきの 42703）。
    fn missing_column(&self, name: &str, span: Span) -> Error {
        let items = || self.frames.iter().rev().flat_map(|f| f.rels.iter());
        if self.lateral_active && items().any(|i| i.lateral_only && has_column(i, name)) {
            return super::not_supported(IMPLICIT_LATERAL, span);
        }
        if items().any(|i| i.usable() && i.rel_visible && i.rel.refname == name) {
            return Error::not_supported(format!(
                "whole-row reference to \"{name}\" is not supported yet"
            ))
            .with_span(span);
        }
        self.column_diag(
            name,
            Error::new(
                sqlstate::UNDEFINED_COLUMN,
                format!("column \"{name}\" does not exist"),
            )
            .with_span(span),
        )
    }

    /// 修飾なしの列名を 1 つのフレームから探す（項目ごとにユーザー列、なければシステム列）。
    fn find_unqualified(frame: &ScopeFrame, name: &str, span: Span) -> Result<Option<BoundExpr>> {
        let mut found: Option<BoundExpr> = None;
        for item in frame.rels.iter().filter(|r| r.usable() && r.cols_visible) {
            let mut hits = item_columns(item, name)?;
            if hits.len() > 1 || (!hits.is_empty() && found.is_some()) {
                return Err(Self::ambiguous(name, span));
            }
            if let Some(h) = hits.pop() {
                found = Some(h);
            }
        }
        Ok(found)
    }

    /// `*`（qual = None。現在のフレームだけ）と `t.*`（内側のフレームから）の展開。FROM 句の順。
    pub(super) fn expand_star(
        &self,
        qual: Option<&[Ident]>,
        span: Span,
    ) -> Result<Vec<StarColumn>> {
        let mut out = Vec::new();
        match qual {
            None => {
                let rels: &[NsItem] = self.current().map_or(&[], |f| f.rels.as_slice());
                let visible: Vec<&NsItem> = rels
                    .iter()
                    .filter(|r| r.usable() && r.cols_visible)
                    .collect();
                if rels.is_empty() || visible.is_empty() {
                    return Err(Error::syntax_at(
                        span,
                        "SELECT * with no tables specified is not valid",
                    ));
                }
                for item in visible {
                    self.expand_item(item, 0, span, &mut out)?;
                }
            }
            Some(q) => {
                for (up, frame) in self.frames.iter().rev().enumerate() {
                    let levels_up = u16::try_from(up).map_err(|_| limit_err())?;
                    if let Some(item) = frame
                        .rels
                        .iter()
                        .find(|r| r.usable() && r.qualifier_matches(q))
                    {
                        self.expand_item(item, levels_up, span, &mut out)?;
                        return Ok(out);
                    }
                }
                return Err(self.missing_from(q, span));
            }
        }
        Ok(out)
    }

    /// 項目の列をすべて（システム列は含めない）。
    fn expand_item(
        &self,
        item: &NsItem,
        levels_up: u16,
        span: Span,
        out: &mut Vec<StarColumn>,
    ) -> Result<()> {
        for (i, c) in item.rel.columns.iter().enumerate() {
            let template = match &item.join_exprs {
                Some(exprs) => exprs
                    .get(i)
                    .cloned()
                    .ok_or_else(|| Error::internal("join column without an expansion"))?,
                None => BoundExpr::column(Var::user(item.rel.rte, col_index(i)?), c.ty),
            };
            let expr = shift_levels(&template, levels_up, span);
            let origin = match &expr.kind {
                ExprKind::Column(v) => self.origin(*v),
                _ => (0, 0),
            };
            out.push(StarColumn {
                name: c.name.clone(),
                expr,
                origin,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Span;

    fn ident(s: &str) -> Ident {
        Ident {
            value: s.to_owned(),
            quoted: false,
            span: Span::default(),
        }
    }

    fn rel(rte: u16, name: &str, cols: &[&str], table_oid: Oid) -> ScopeRel {
        ScopeRel {
            rte: RteId(rte),
            refname: name.to_owned(),
            hidden_name: None,
            schema: Some("public".to_owned()),
            table_oid,
            columns: cols
                .iter()
                .enumerate()
                .map(|(i, c)| ScopeColumn {
                    name: (*c).to_owned(),
                    ty: SqlType::INT4,
                    attnum: i16::try_from(i + 1).unwrap(),
                })
                .collect(),
            system_columns: table_oid != 0,
        }
    }

    fn var(e: &BoundExpr) -> Var {
        match &e.kind {
            ExprKind::Column(v) => *v,
            other => panic!("not a column: {other:?}"),
        }
    }

    fn resolve(s: &ScopeStack, parts: &[&str]) -> Result<BoundExpr> {
        let idents: Vec<Ident> = parts.iter().map(|p| ident(p)).collect();
        s.resolve_column(&idents, Span::default())
    }

    #[test]
    fn levels_up_counts_frames() {
        let outer = ScopeStack::single(vec![rel(0, "t", &["a", "b"], 16384)]);
        let inner = outer.with_frame(vec![rel(0, "u", &["b", "c"], 16385)]);
        let a = resolve(&inner, &["a"]).unwrap();
        assert_eq!(var(&a), Var::user(RteId(0), 0).with_levels_up(1));
        // 内側のフレームが先に見つかる
        let b = resolve(&inner, &["b"]).unwrap();
        assert_eq!(var(&b), Var::user(RteId(0), 0));
        let tb = resolve(&inner, &["t", "b"]).unwrap();
        assert_eq!(var(&tb), Var::user(RteId(0), 1).with_levels_up(1));
        assert_eq!(inner.origin(var(&tb)), (16384, 2));
        assert_eq!(inner.origin(var(&b)), (16385, 1));
    }

    #[test]
    fn errors_match_m3_messages() {
        let s = ScopeStack::single(vec![rel(0, "t", &["a"], 16384)]);
        let e = resolve(&s, &["z"]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_COLUMN);
        assert_eq!(e.message, "column \"z\" does not exist");
        let e = resolve(&s, &["t", "z"]).unwrap_err();
        assert_eq!(e.message, "column t.z does not exist");
        let e = resolve(&s, &["x", "a"]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
        assert_eq!(e.message, "missing FROM-clause entry for table \"x\"");
        let e = ScopeStack::empty()
            .expand_star(None, Span::default())
            .unwrap_err();
        assert_eq!(e.message, "SELECT * with no tables specified is not valid");
    }

    #[test]
    fn system_columns_are_vars_and_have_negative_attnum() {
        let s = ScopeStack::single(vec![rel(0, "t", &["a"], 16384)]);
        let c = resolve(&s, &["ctid"]).unwrap();
        assert_eq!(var(&c), Var::system(RteId(0), SystemColumn::Ctid));
        assert_eq!(s.origin(var(&c)), (16384, -1));
        // ユーザー列が先
        let s = ScopeStack::single(vec![rel(0, "t", &["xmin"], 16384)]);
        let c = resolve(&s, &["xmin"]).unwrap();
        assert!(!var(&c).is_system());
    }

    #[test]
    fn ambiguous_unqualified_column() {
        let s = ScopeStack::single(vec![rel(0, "t", &["a"], 16384), rel(1, "u", &["a"], 16385)]);
        let e = resolve(&s, &["a"]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::AMBIGUOUS_COLUMN);
    }

    #[test]
    fn system_column_candidates_are_per_rte() {
        let s = ScopeStack::single(vec![rel(0, "t", &["a"], 16384), rel(1, "u", &["b"], 16385)]);
        let e = resolve(&s, &["ctid"]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::AMBIGUOUS_COLUMN);
        // 片方がユーザー列として持てば、そちらと system 列とで曖昧
        let s = ScopeStack::single(vec![rel(0, "t", &["a"], 16384), rel(1, "u", &["b"], 0)]);
        assert_eq!(var(&resolve(&s, &["ctid"]).unwrap()).rte, RteId(0));
    }

    #[test]
    fn lateral_only_items_are_diagnosed() {
        let mut left = NsItem::new(rel(0, "t", &["a"], 16384));
        left.lateral_only = true;
        left.lateral_ok = true;
        let s = ScopeStack::single(vec![left.clone()]);
        let e = resolve(&s, &["a"]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_COLUMN);
        assert_eq!(
            e.detail.as_deref(),
            Some(
                "There is a column named \"a\" in table \"t\", but it cannot be referenced from this part of the query."
            )
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("To reference that column, you must mark this subquery with LATERAL.")
        );
        let e = resolve(&s, &["t", "a"]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
        assert_eq!(
            e.message,
            "invalid reference to FROM-clause entry for table \"t\""
        );
        assert!(e.hint.is_some());
        let e = resolve(&s.clone().with_lateral_active(), &["a"]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        left.lateral_ok = false;
        let e = resolve(&ScopeStack::single(vec![left]), &["a"]).unwrap_err();
        assert!(e.hint.is_none());
    }

    #[test]
    fn join_columns_expand_to_child_vars_with_levels() {
        let mut j = NsItem::new(rel(2, "", &["a", "b"], 0));
        j.rel_visible = false;
        j.join_exprs = Some(vec![
            BoundExpr::column(Var::user(RteId(0), 0), SqlType::INT4),
            BoundExpr::column(Var::user(RteId(1), 1), SqlType::INT4),
        ]);
        let mut t = NsItem::new(rel(0, "t", &["a"], 16384));
        t.cols_visible = false;
        let outer = ScopeStack::single(vec![t, j]);
        let inner = outer.with_frame(vec![rel(0, "u", &["z"], 16385)]);
        let b = resolve(&inner, &["b"]).unwrap();
        assert_eq!(var(&b), Var::user(RteId(1), 1).with_levels_up(1));
        // 子の表は修飾すれば見える。修飾なしでは見えない（システム列）
        let ct = resolve(&outer, &["t", "ctid"]).unwrap();
        assert!(var(&ct).is_system());
        let e = resolve(&outer, &["ctid"]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_COLUMN);
        assert_eq!(e.hint.as_deref(), Some("Try using a table-qualified name."));
        let star = outer.expand_star(None, Span::default()).unwrap();
        assert_eq!(star.len(), 2);
        assert_eq!(var(&star[1].expr), Var::user(RteId(1), 1));
    }
}
