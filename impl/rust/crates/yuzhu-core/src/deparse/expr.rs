//! 式の本体の書式（`m4/10-explain-copy-compat.md` §4.4〜§4.6）。
//!
//! PostgreSQL の `get_rule_expr` / `get_rule_expr_paren` / `isSimpleNode` に対応する。
//! 出力は 1 本のバッファに書き足す（`CASE` の字下げが、直前の空白を取り除くため）。
//!
//! - 非 pretty: 括弧を付ける式（演算子・`AND` / `OR` / `NOT` など）が自分で付ける。
//! - pretty: 式は括弧なしで書き、親が子を書くとき [`is_simple`] が偽なら括弧で包む。

use super::literal::{like_escape, literal, quote_literal};
use super::typename::format_sql_type;
use super::{
    DeparseCtx, DeparseMode, SubPlanLabel, ident::quote_identifier, literal::array_literal,
};
use crate::catalog::{BuiltinOperator, CastMethod};
use crate::error::{Error, Result};
use crate::expr::{AggCall, BoolTestKind, Expr, ExprKind, SubLinkKind};
use crate::sql::ast::SessionValueKind;
use crate::types::{Datum, SqlType};

/// 子を書くときの親の種類（括弧の判断に使う）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parent {
    /// 括弧の判断をしない位置（根、関数の引数、`COALESCE`・`CASE` の中、`ARRAY[...]` の要素）。
    None,
    /// 演算子。`simple` は、2 項で名前が 1 文字のときのその文字。`left` は子が左の引数か。
    Op {
        simple: Option<u8>,
        left: bool,
    },
    And,
    Or,
    Not,
    /// キャスト（`CoerceTypmod` を含む）。
    Cast,
    /// `IS NULL`、`IS TRUE`、`ANY`、`IS DISTINCT FROM` など。
    Other,
}

/// `IN` リストの書式の種類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InShape {
    /// 要素 1 個: `(x = e)` / `(x <> e)`。
    Single,
    /// 全部定数で 2 個以上: `x = ANY (...)` / `x <> ALL (...)`。
    Array,
    /// `OR`（`NOT IN` は `AND`）の連なり。
    Chain,
}

fn in_shape<C, Q>(list: &[Expr<C, Q>]) -> InShape {
    if list.len() == 1 {
        InShape::Single
    } else if list.len() >= 2 && list.iter().all(|e| matches!(e.kind, ExprKind::Literal(_))) {
        InShape::Array
    } else {
        InShape::Chain
    }
}

/// 2 項で名前が 1 文字の演算子のその文字（`get_simple_binary_op_name`）。
fn simple_op_char(op: &BuiltinOperator, nargs: usize) -> Option<u8> {
    match op.name.as_bytes() {
        [c] if nargs == 2 => Some(*c),
        _ => None,
    }
}

/// 演算子（の形をした式）が、`parent` の下で括弧なしで書けるか。
fn op_is_simple(child: Option<u8>, parent: Parent) -> bool {
    if let Parent::Op { simple, left } = parent {
        let Some(c) = child else { return false };
        let (lo, hi) = (matches!(c, b'+' | b'-'), matches!(c, b'*' | b'/' | b'%'));
        if !(lo || hi) {
            return false;
        }
        let Some(p) = simple else { return false };
        let (plo, phi) = (matches!(p, b'+' | b'-'), matches!(p, b'*' | b'/' | b'%'));
        if !(plo || phi) {
            return false;
        }
        if hi && plo {
            return true;
        }
        if lo && phi {
            return false;
        }
        // 同じ優先度: (a - b) - c は括弧なし、a - (b - c) は括弧あり。
        return left;
    }
    group_is_simple(parent)
}

/// `NullTest` などの「親に応じて決まる」式。
fn group_is_simple(parent: Parent) -> bool {
    matches!(parent, Parent::And | Parent::Or | Parent::Not)
}

/// `AND` / `OR` / `NOT` を `parent` の下で括弧なしで書けるか。
fn bool_is_simple(kind: BoolKind, parent: Parent) -> bool {
    matches!(
        (kind, parent),
        (BoolKind::And | BoolKind::Not, Parent::And | Parent::Or) | (BoolKind::Or, Parent::Or)
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BoolKind {
    And,
    Or,
    Not,
}

/// PostgreSQL の `isSimpleNode`: `e` を `parent` の子として書くとき、括弧が要らないか。
fn is_simple<C, Q>(e: &Expr<C, Q>, parent: Parent) -> bool {
    match &e.kind {
        ExprKind::Literal(_)
        | ExprKind::Column(_)
        | ExprKind::SubLinkOutput(_)
        | ExprKind::Aggregate(_)
        | ExprKind::Function { .. }
        | ExprKind::SessionValue(_)
        | ExprKind::Coalesce(_)
        | ExprKind::NullIf { .. }
        | ExprKind::MinMax { .. }
        | ExprKind::Case { .. } => true,
        ExprKind::Cast { expr, method, .. } => match method {
            CastMethod::Function(_) | CastMethod::Env(_) => true,
            CastMethod::Binary | CastMethod::InOut => is_simple(expr, Parent::Cast),
        },
        ExprKind::CoerceTypmod { expr, .. } => is_simple(expr, Parent::Cast),
        ExprKind::Operator { op, args } => op_is_simple(simple_op_char(op, args.len()), parent),
        ExprKind::Like { .. } => op_is_simple(None, parent),
        ExprKind::IsNull(_)
        | ExprKind::IsNotNull(_)
        | ExprKind::BoolTest { .. }
        | ExprKind::DistinctFrom { .. }
        | ExprKind::SubLink { .. } => group_is_simple(parent),
        ExprKind::And(_) => bool_is_simple(BoolKind::And, parent),
        ExprKind::Or(_) => bool_is_simple(BoolKind::Or, parent),
        ExprKind::Not(_) => bool_is_simple(BoolKind::Not, parent),
        ExprKind::InList {
            list,
            eq_op,
            negated,
            ..
        } => match in_shape(list) {
            InShape::Single => {
                let c = if *negated {
                    None
                } else {
                    simple_op_char(eq_op, 2)
                };
                op_is_simple(c, parent)
            }
            InShape::Array => false,
            InShape::Chain => bool_is_simple(
                if *negated {
                    BoolKind::And
                } else {
                    BoolKind::Or
                },
                parent,
            ),
        },
    }
}

pub(super) struct Deparser<'a, 'c, C, Q> {
    cx: &'c DeparseCtx<'a, C, Q>,
    buf: String,
    /// `CASE` の字下げ幅（`indentLevel`）。
    indent: usize,
    /// 今書いている `SubLink` の名前（`test` の中の `SubLinkOutput` が使う）。
    sub: Option<SubPlanLabel>,
}

/// `PRETTYINDENT_VAR`: `CASE` の中身の字下げ幅。
const CASE_INDENT: isize = 4;

/// `CASE` の `WHEN 条件 THEN 結果`。
type Arm<C, Q> = (Expr<C, Q>, Expr<C, Q>);

impl<'a, 'c, C: Clone, Q: Clone> Deparser<'a, 'c, C, Q> {
    pub(super) fn new(cx: &'c DeparseCtx<'a, C, Q>) -> Self {
        Deparser {
            cx,
            buf: String::new(),
            indent: 0,
            sub: None,
        }
    }

    pub(super) fn finish(self) -> String {
        self.buf
    }

    fn pretty(&self) -> bool {
        self.cx.opts.pretty_paren
    }

    fn push(&mut self, s: &str) {
        self.buf.push_str(s);
    }

    /// 非 pretty のときだけ、自分で括弧を付ける。
    fn own_paren(&mut self, f: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        if self.pretty() {
            f(self)
        } else {
            self.buf.push('(');
            f(self)?;
            self.buf.push(')');
            Ok(())
        }
    }

    /// 根の式。`Stored` は根の暗黙のキャストを隠す（連続していれば繰り返す）。
    pub(super) fn root(&mut self, e: &Expr<C, Q>) -> Result<()> {
        let mut cur = e;
        let mut hidden = false;
        if self.cx.opts.mode == DeparseMode::Stored {
            while let ExprKind::Cast {
                expr,
                implicit: true,
                ..
            }
            | ExprKind::CoerceTypmod {
                expr,
                explicit: false,
            } = &cur.kind
            {
                cur = expr;
                hidden = true;
            }
        }
        self.write(cur, if hidden { Parent::Cast } else { Parent::None })
    }

    /// `e` を `parent` の子として書く（`get_rule_expr_paren`）。
    fn write(&mut self, e: &Expr<C, Q>, parent: Parent) -> Result<()> {
        let need = self.pretty() && parent != Parent::None && !is_simple(e, parent);
        if need {
            self.buf.push('(');
        }
        self.write_node(e)?;
        if need {
            self.buf.push(')');
        }
        Ok(())
    }

    fn write_list(&mut self, es: &[Expr<C, Q>]) -> Result<()> {
        for (i, e) in es.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.write(e, Parent::None)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn write_node(&mut self, e: &Expr<C, Q>) -> Result<()> {
        match &e.kind {
            ExprKind::Literal(d) => {
                let s = literal(d, e.ty, self.cx.type_env, self.cx.catalog)
                    .map_err(|err| err.with_span(e.span))?;
                self.push(&s);
            }
            ExprKind::Column(c) => {
                let t = self.cx.namer.name(c).map_err(|err| err.with_span(e.span))?;
                if t.wrap {
                    self.push("(");
                    self.push(&t.text);
                    self.push(")");
                } else {
                    self.push(&t.text);
                }
            }
            ExprKind::Operator { op, args } => self.operator(op, args)?,
            ExprKind::Function { func, args } => {
                self.push(&quote_identifier(func.name));
                self.push("(");
                self.write_list(args)?;
                self.push(")");
            }
            ExprKind::SessionValue(v) => self.session_value(*v),
            ExprKind::Cast {
                expr,
                method: CastMethod::InOut,
                ..
            } if self.folded_literal(expr, e.ty).is_some() => {
                // 日時の文字列リテラルのキャストは、PostgreSQL では解析時に定数になる（`'2020-01-01'::date`）。
                if let Some(s) = self.folded_literal(expr, e.ty) {
                    self.push(&s);
                }
            }
            ExprKind::Cast { expr, .. } | ExprKind::CoerceTypmod { expr, .. } => {
                self.own_paren_cast(expr, e.ty)?;
            }
            ExprKind::And(args) => self.bool_chain(args, " AND ", Parent::And)?,
            ExprKind::Or(args) => self.bool_chain(args, " OR ", Parent::Or)?,
            ExprKind::Not(a) => self.own_paren(|d| {
                d.push("NOT ");
                d.write(a, Parent::Not)
            })?,
            ExprKind::IsNull(a) => self.postfix(a, " IS NULL")?,
            ExprKind::IsNotNull(a) => self.postfix(a, " IS NOT NULL")?,
            ExprKind::BoolTest { expr, test } => {
                let s = match test {
                    BoolTestKind::IsTrue => " IS TRUE",
                    BoolTestKind::IsNotTrue => " IS NOT TRUE",
                    BoolTestKind::IsFalse => " IS FALSE",
                    BoolTestKind::IsNotFalse => " IS NOT FALSE",
                    BoolTestKind::IsUnknown => " IS UNKNOWN",
                    BoolTestKind::IsNotUnknown => " IS NOT UNKNOWN",
                };
                self.postfix(expr, s)?;
            }
            ExprKind::Case { arms, else_result } => {
                self.case(arms, else_result.as_deref(), e.ty)?;
            }
            ExprKind::Coalesce(args) => {
                self.push("COALESCE(");
                self.write_list(args)?;
                self.push(")");
            }
            ExprKind::NullIf { left, right, .. } => {
                self.push("NULLIF(");
                self.write(left, Parent::None)?;
                self.push(", ");
                self.write(right, Parent::None)?;
                self.push(")");
            }
            ExprKind::DistinctFrom {
                left,
                right,
                negated,
                ..
            } => self.own_paren(|d| {
                d.write(left, Parent::Other)?;
                d.push(if *negated {
                    " IS NOT DISTINCT FROM "
                } else {
                    " IS DISTINCT FROM "
                });
                d.write(right, Parent::Other)
            })?,
            ExprKind::MinMax { greatest, args, .. } => {
                self.push(if *greatest { "GREATEST(" } else { "LEAST(" });
                self.write_list(args)?;
                self.push(")");
            }
            ExprKind::Like {
                expr,
                pattern,
                escape,
                negated,
                case_insensitive,
            } => self.like(
                expr,
                pattern,
                escape.as_deref(),
                *negated,
                *case_insensitive,
            )?,
            ExprKind::InList {
                expr,
                list,
                eq_op,
                negated,
            } => self.in_list(expr, list, eq_op, *negated)?,
            ExprKind::Aggregate(call) => self.aggregate(call)?,
            ExprKind::SubLink { kind, test, query } => {
                self.sublink(*kind, test.as_deref(), query, e)?;
            }
            ExprKind::SubLinkOutput(i) => {
                let Some(label) = &self.sub else {
                    return Err(Error::internal("SubLinkOutput outside of a SubLink test")
                        .with_span(e.span));
                };
                let hashed = if label.hashed { "hashed " } else { "" };
                let s = format!("({hashed}{}).col{}", label.name, i + 1);
                self.push(&s);
            }
        }
        Ok(())
    }

    fn operator(&mut self, op: &BuiltinOperator, args: &[Expr<C, Q>]) -> Result<()> {
        match args {
            [a] => self.own_paren(|d| {
                d.push(op.name);
                d.push(" ");
                d.write(
                    a,
                    Parent::Op {
                        simple: None,
                        left: false,
                    },
                )
            }),
            [l, r] => self.binary(op.name, l, r),
            _ => Err(Error::internal(format!(
                "operator {} with {} arguments",
                op.name,
                args.len()
            ))),
        }
    }

    /// `(l op r)`（pretty では括弧なし）。
    fn binary(&mut self, name: &str, l: &Expr<C, Q>, r: &Expr<C, Q>) -> Result<()> {
        let simple = match name.as_bytes() {
            [c] => Some(*c),
            _ => None,
        };
        self.own_paren(|d| {
            d.write(l, Parent::Op { simple, left: true })?;
            d.push(" ");
            d.push(name);
            d.push(" ");
            d.write(
                r,
                Parent::Op {
                    simple,
                    left: false,
                },
            )
        })
    }

    fn session_value(&mut self, v: SessionValueKind) {
        let precision = |name: &str, p: i32| {
            if p >= 0 {
                format!("{name}({p})")
            } else {
                name.to_owned()
            }
        };
        let s = match v {
            SessionValueKind::CurrentUser => "CURRENT_USER".to_owned(),
            SessionValueKind::SessionUser => "SESSION_USER".to_owned(),
            SessionValueKind::CurrentRole => "CURRENT_ROLE".to_owned(),
            SessionValueKind::User => "USER".to_owned(),
            SessionValueKind::CurrentCatalog => "CURRENT_CATALOG".to_owned(),
            SessionValueKind::CurrentSchema => "\"current_schema\"()".to_owned(),
            SessionValueKind::CurrentDate => "CURRENT_DATE".to_owned(),
            SessionValueKind::CurrentTimestamp { precision: p } => {
                precision("CURRENT_TIMESTAMP", p)
            }
            SessionValueKind::LocalTimestamp { precision: p } => precision("LOCALTIMESTAMP", p),
            SessionValueKind::Now => "now()".to_owned(),
            SessionValueKind::TransactionTimestamp => "transaction_timestamp()".to_owned(),
        };
        self.push(&s);
    }

    /// 文字列リテラルを `ty` の入力関数で読んだ定数の書き方。読めないとき・入力が遅延型でないときは `None`。
    fn folded_literal(&self, arg: &Expr<C, Q>, ty: SqlType) -> Option<String> {
        let ExprKind::Literal(Datum::Text(s)) = &arg.kind else {
            return None;
        };
        if crate::types::io::input_is_eager(ty.oid) {
            return None;
        }
        let d = crate::types::io::input_text_env(s, ty, self.cx.type_env).ok()?;
        literal(&d, ty, self.cx.type_env, self.cx.catalog).ok()
    }

    /// `(E(a))::{型}`。非 pretty は引数に常に括弧、pretty は引数が単純でないときだけ。
    fn own_paren_cast(&mut self, arg: &Expr<C, Q>, ty: SqlType) -> Result<()> {
        if !self.pretty() {
            self.push("(");
        }
        self.write(arg, Parent::Cast)?;
        if !self.pretty() {
            self.push(")");
        }
        self.push("::");
        self.push(&format_sql_type(ty));
        Ok(())
    }

    fn bool_chain(&mut self, args: &[Expr<C, Q>], sep: &str, parent: Parent) -> Result<()> {
        self.own_paren(|d| {
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    d.push(sep);
                }
                d.write(a, parent)?;
            }
            Ok(())
        })
    }

    fn postfix(&mut self, arg: &Expr<C, Q>, suffix: &str) -> Result<()> {
        self.own_paren(|d| {
            d.write(arg, Parent::Other)?;
            d.push(suffix);
            Ok(())
        })
    }

    /// `appendContextKeyword`。`indent` なら改行 + 字下げ、そうでなければ空白 1 つ（先頭の `CASE` を除く）。
    fn keyword(&mut self, kw: &str, before: isize, after: isize, first: bool) {
        if self.cx.opts.indent {
            self.indent = self.indent.saturating_add_signed(before);
            while self.buf.ends_with(' ') {
                self.buf.pop();
            }
            self.buf.push('\n');
            self.buf.push_str(&" ".repeat(self.indent));
            self.buf.push_str(kw);
            self.indent = self.indent.saturating_add_signed(after);
        } else {
            if !first {
                self.buf.push(' ');
            }
            self.buf.push_str(kw);
        }
    }

    fn case(
        &mut self,
        arms: &[Arm<C, Q>],
        else_result: Option<&Expr<C, Q>>,
        ty: SqlType,
    ) -> Result<()> {
        const STEP: isize = CASE_INDENT;
        self.keyword("CASE", 0, STEP, true);
        for (cond, res) in arms {
            self.keyword("WHEN ", 0, 0, false);
            self.write(cond, Parent::None)?;
            self.push(" THEN ");
            self.write(res, Parent::None)?;
        }
        self.keyword("ELSE ", 0, 0, false);
        if let Some(e) = else_result {
            self.write(e, Parent::None)?;
        } else {
            let s = literal(&Datum::Null, ty, self.cx.type_env, self.cx.catalog)?;
            self.push(&s);
        }
        self.keyword("END", -STEP, 0, false);
        Ok(())
    }

    fn like(
        &mut self,
        expr: &Expr<C, Q>,
        pattern: &Expr<C, Q>,
        escape: Option<&Expr<C, Q>>,
        negated: bool,
        ci: bool,
    ) -> Result<()> {
        let name = match (negated, ci) {
            (false, false) => "~~",
            (true, false) => "!~~",
            (false, true) => "~~*",
            (true, true) => "!~~*",
        };
        let parent = |left| Parent::Op { simple: None, left };
        self.own_paren(|d| {
            d.write(expr, parent(true))?;
            d.push(" ");
            d.push(name);
            d.push(" ");
            match escape {
                None => d.write(pattern, parent(false)),
                Some(esc) => d.like_pattern_with_escape(pattern, esc),
            }
        })
    }

    /// `ESCAPE` つきのパターン。`Plan` で両方が定数なら畳み込み、そうでなければ `like_escape(p, e)`。
    fn like_pattern_with_escape(&mut self, pattern: &Expr<C, Q>, esc: &Expr<C, Q>) -> Result<()> {
        if self.cx.opts.mode == DeparseMode::Plan
            && let (ExprKind::Literal(Datum::Text(p)), ExprKind::Literal(Datum::Text(e))) =
                (&pattern.kind, &esc.kind)
            && let Some(folded) = like_escape(p, e)
        {
            let s = format!(
                "{}::{}",
                quote_literal(&folded),
                format_sql_type(pattern.ty)
            );
            self.push(&s);
            return Ok(());
        }
        self.push("like_escape(");
        self.write(pattern, Parent::None)?;
        self.push(", ");
        self.write(esc, Parent::None)?;
        self.push(")");
        Ok(())
    }

    fn in_list(
        &mut self,
        x: &Expr<C, Q>,
        list: &[Expr<C, Q>],
        eq_op: &BuiltinOperator,
        negated: bool,
    ) -> Result<()> {
        let op_name = if negated { "<>" } else { eq_op.name };
        match in_shape(list) {
            InShape::Single => self.binary(op_name, x, &list[0]),
            InShape::Array => self.own_paren(|d| {
                d.write(x, Parent::Other)?;
                d.push(if negated { " <> ALL (" } else { " = ANY (" });
                if d.cx.opts.mode == DeparseMode::Plan {
                    let elems: Vec<Datum> = list
                        .iter()
                        .filter_map(|e| match &e.kind {
                            ExprKind::Literal(v) => Some(v.clone()),
                            _ => None,
                        })
                        .collect();
                    let elem_ty = list
                        .iter()
                        .find(|e| !matches!(e.kind, ExprKind::Literal(Datum::Null)))
                        .map_or(list[0].ty, |e| e.ty);
                    let s = array_literal(&elems, elem_ty, d.cx.type_env, d.cx.catalog)?;
                    d.push(&s);
                } else {
                    d.push("ARRAY[");
                    d.write_list(list)?;
                    d.push("]");
                }
                d.push(")");
                Ok(())
            }),
            InShape::Chain => {
                let (sep, parent) = if negated {
                    (" AND ", Parent::And)
                } else {
                    (" OR ", Parent::Or)
                };
                self.own_paren(|d| {
                    for (i, item) in list.iter().enumerate() {
                        if i > 0 {
                            d.push(sep);
                        }
                        // 合成した `x = item`。親の下で括弧が要るかは演算子と同じ規則。
                        let simple = match op_name.as_bytes() {
                            [c] => Some(*c),
                            _ => None,
                        };
                        let need = d.pretty() && !op_is_simple(simple, parent);
                        if need {
                            d.push("(");
                        }
                        d.binary(op_name, x, item)?;
                        if need {
                            d.push(")");
                        }
                    }
                    Ok(())
                })
            }
        }
    }

    fn aggregate(&mut self, call: &AggCall<C, Q>) -> Result<()> {
        self.push(&quote_identifier(call.func.name));
        self.push("(");
        if call.args.is_empty() {
            self.push("*");
        } else {
            if call.distinct {
                self.push("DISTINCT ");
            }
            self.write_list(&call.args)?;
        }
        if !call.order_by.is_empty() {
            self.push(" ORDER BY ");
            for (i, k) in call.order_by.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.write(&k.expr, Parent::None)?;
                if k.descending {
                    self.push(" DESC");
                }
                self.push(if k.nulls_first {
                    " NULLS FIRST"
                } else {
                    " NULLS LAST"
                });
            }
        }
        self.push(")");
        if let Some(f) = &call.filter {
            self.push(" FILTER (WHERE ");
            self.write(f, Parent::None)?;
            self.push(")");
        }
        Ok(())
    }

    fn sublink(
        &mut self,
        kind: SubLinkKind,
        test: Option<&Expr<C, Q>>,
        query: &Q,
        e: &Expr<C, Q>,
    ) -> Result<()> {
        let Some(renderer) = self.cx.sublinks else {
            return Err(
                Error::internal("subquery expression in a stored expression").with_span(e.span),
            );
        };
        let label = renderer.label(query).map_err(|err| err.with_span(e.span))?;
        let hashed = if label.hashed { "hashed " } else { "" };
        match kind {
            SubLinkKind::Scalar => {
                let s = if label.init_plan {
                    format!("({}).col1", label.name)
                } else {
                    format!("({hashed}{})", label.name)
                };
                self.push(&s);
            }
            SubLinkKind::Exists => {
                let s = if label.init_plan {
                    format!("({}).col1", label.name)
                } else {
                    format!("EXISTS({hashed}{})", label.name)
                };
                self.push(&s);
            }
            SubLinkKind::Any | SubLinkKind::All => {
                let Some(test) = test else {
                    return Err(Error::internal("ANY/ALL sublink without a test").with_span(e.span));
                };
                self.push(if kind == SubLinkKind::Any {
                    "(ANY "
                } else {
                    "(ALL "
                });
                let saved = self.sub.replace(label);
                let r = self.write(test, Parent::None);
                self.sub = saved;
                r?;
                self.push(")");
            }
        }
        Ok(())
    }
}
