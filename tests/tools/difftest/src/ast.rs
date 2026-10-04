//! Expression and SELECT trees: rendering to SQL, type categories,
//! column visitors and shrinking (used by the minimizer).

use std::fmt::Write as _;

use crate::schema::{Cat, Lit, LitForm, Schema, Ty, col_name, render_lit};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    NeBang,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Concat,
}

impl BinOp {
    fn sql(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Eq => "=",
            BinOp::Ne => "<>",
            BinOp::NeBang => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "AND",
            BinOp::Or => "OR",
            BinOp::Concat => "||",
        }
    }

    pub(crate) const ARITH: [BinOp; 5] =
        [BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::Div, BinOp::Mod];
    pub(crate) const CMP: [BinOp; 7] = [
        BinOp::Eq,
        BinOp::Ne,
        BinOp::NeBang,
        BinOp::Lt,
        BinOp::Le,
        BinOp::Gt,
        BinOp::Ge,
    ];
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum UnOp {
    Neg,
    Plus,
    Not,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Func {
    Length,
    Lower,
    Upper,
    Abs,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AggFunc {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
    BoolAnd,
    BoolOr,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SubKind {
    Exists,
    In { neg: bool },
    Scalar,
}

#[derive(Clone, Debug)]
pub(crate) enum Expr {
    /// Column `col` of FROM item `item` (whose table is `table`).
    Col {
        item: usize,
        table: usize,
        col: usize,
        qualify: bool,
    },
    Lit {
        lit: Lit,
        ty: Ty,
        form: LitForm,
    },
    Unary {
        op: UnOp,
        e: Box<Expr>,
    },
    Binary {
        op: BinOp,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    IsNull {
        e: Box<Expr>,
        neg: bool,
    },
    IsBool {
        e: Box<Expr>,
        neg: bool,
        val: bool,
    },
    Between {
        e: Box<Expr>,
        lo: Box<Expr>,
        hi: Box<Expr>,
        neg: bool,
    },
    InList {
        e: Box<Expr>,
        list: Vec<Expr>,
        neg: bool,
    },
    Like {
        e: Box<Expr>,
        pat: Box<Expr>,
        neg: bool,
    },
    /// Simple form when `operand` is set (`whens` are `(value, result)`),
    /// searched form otherwise (`whens` are `(condition, result)`).
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        els: Option<Box<Expr>>,
    },
    Coalesce(Vec<Expr>),
    NullIf(Box<Expr>, Box<Expr>),
    Cast {
        e: Box<Expr>,
        ty: Ty,
        spelling: u8,
        func_syntax: bool,
    },
    Func {
        f: Func,
        arg: Box<Expr>,
    },
    Agg {
        f: AggFunc,
        arg: Option<Box<Expr>>,
        distinct: bool,
    },
    /// Uncorrelated subquery.
    Sub {
        kind: SubKind,
        lhs: Option<Box<Expr>>,
        query: Box<Select>,
    },
}

/// Rendering context: real table names (by schema index) and the qualifier
/// of each FROM item of the current query level.
#[derive(Debug)]
pub(crate) struct Rctx<'a> {
    pub(crate) names: &'a [String],
    pub(crate) quals: Vec<String>,
}

impl Expr {
    pub(crate) fn lit(lit: Lit, ty: Ty, form: LitForm) -> Expr {
        Expr::Lit { lit, ty, form }
    }

    pub(crate) fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            op,
            l: Box::new(l),
            r: Box::new(r),
        }
    }

    /// A trivial literal of the category, used when shrinking.
    pub(crate) fn simple(cat: Cat) -> Expr {
        match cat {
            Cat::Int => Expr::lit(Lit::Int(0), Ty::Int4, LitForm::Bare),
            Cat::Float => Expr::lit(Lit::Float("0".into()), Ty::Float8, LitForm::Cast),
            Cat::Bool => Expr::lit(Lit::Bool(true), Ty::Bool, LitForm::Bare),
            Cat::Text => Expr::lit(Lit::Text(String::new()), Ty::Text, LitForm::Bare),
        }
    }

    pub(crate) fn is_bare_null(&self) -> bool {
        matches!(
            self,
            Expr::Lit {
                lit: Lit::Null,
                form: LitForm::Bare,
                ..
            }
        )
    }

    pub(crate) fn cat(&self, s: &Schema) -> Cat {
        match self {
            Expr::Col { table, col, .. } => s.tables[*table].cols[*col].ty.cat(),
            Expr::Lit { ty, .. } | Expr::Cast { ty, .. } => ty.cat(),
            Expr::Unary { op: UnOp::Not, .. }
            | Expr::IsNull { .. }
            | Expr::IsBool { .. }
            | Expr::Between { .. }
            | Expr::InList { .. }
            | Expr::Like { .. } => Cat::Bool,
            Expr::Unary { e, .. } => e.cat(s),
            Expr::Binary { op, l, r } => match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                    if l.cat(s) == Cat::Float || r.cat(s) == Cat::Float {
                        Cat::Float
                    } else {
                        Cat::Int
                    }
                }
                BinOp::Concat => Cat::Text,
                _ => Cat::Bool,
            },
            Expr::Case { whens, .. } => whens[0].1.cat(s),
            Expr::Coalesce(v) => v[0].cat(s),
            Expr::NullIf(l, _) => l.cat(s),
            Expr::Func { f, arg } => match f {
                Func::Length => Cat::Int,
                Func::Lower | Func::Upper => Cat::Text,
                Func::Abs => arg.cat(s),
            },
            Expr::Agg { f, arg, .. } => match f {
                AggFunc::CountStar | AggFunc::Count | AggFunc::Sum => Cat::Int,
                AggFunc::Avg => Cat::Float,
                AggFunc::BoolAnd | AggFunc::BoolOr => Cat::Bool,
                AggFunc::Min | AggFunc::Max => arg.as_ref().map_or(Cat::Int, |a| a.cat(s)),
            },
            Expr::Sub { kind, query, .. } => match kind {
                SubKind::Exists | SubKind::In { .. } => Cat::Bool,
                SubKind::Scalar => match &query.items[0] {
                    SelItem::Expr(e, _) => e.cat(s),
                    SelItem::Star => Cat::Int,
                },
            },
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn render(&self, cx: &Rctx<'_>, out: &mut String) {
        match self {
            Expr::Col {
                item, col, qualify, ..
            } => {
                if *qualify {
                    let _ = write!(out, "{}.", cx.quals[*item]);
                }
                out.push_str(&col_name(*col));
            }
            Expr::Lit { lit, ty, form } => render_lit(lit, *ty, *form, out),
            Expr::Unary { op, e } => {
                out.push_str(match op {
                    UnOp::Neg => "(- ",
                    UnOp::Plus => "(+ ",
                    UnOp::Not => "(NOT ",
                });
                e.render(cx, out);
                out.push(')');
            }
            Expr::Binary { op, l, r } => {
                out.push('(');
                l.render(cx, out);
                let _ = write!(out, " {} ", op.sql());
                r.render(cx, out);
                out.push(')');
            }
            Expr::IsNull { e, neg } => {
                out.push('(');
                e.render(cx, out);
                out.push_str(if *neg { " IS NOT NULL)" } else { " IS NULL)" });
            }
            Expr::IsBool { e, neg, val } => {
                out.push('(');
                e.render(cx, out);
                out.push_str(if *neg { " IS NOT " } else { " IS " });
                out.push_str(if *val { "TRUE)" } else { "FALSE)" });
            }
            Expr::Between { e, lo, hi, neg } => {
                out.push('(');
                e.render(cx, out);
                out.push_str(if *neg { " NOT BETWEEN " } else { " BETWEEN " });
                lo.render(cx, out);
                out.push_str(" AND ");
                hi.render(cx, out);
                out.push(')');
            }
            Expr::InList { e, list, neg } => {
                out.push('(');
                e.render(cx, out);
                out.push_str(if *neg { " NOT IN (" } else { " IN (" });
                render_list(list, cx, out);
                out.push_str("))");
            }
            Expr::Like { e, pat, neg } => {
                out.push('(');
                e.render(cx, out);
                out.push_str(if *neg { " NOT LIKE " } else { " LIKE " });
                pat.render(cx, out);
                out.push(')');
            }
            Expr::Case {
                operand,
                whens,
                els,
            } => {
                out.push_str("CASE");
                if let Some(o) = operand {
                    out.push(' ');
                    o.render(cx, out);
                }
                for (w, t) in whens {
                    out.push_str(" WHEN ");
                    w.render(cx, out);
                    out.push_str(" THEN ");
                    t.render(cx, out);
                }
                if let Some(e) = els {
                    out.push_str(" ELSE ");
                    e.render(cx, out);
                }
                out.push_str(" END");
            }
            Expr::Coalesce(v) => {
                out.push_str("COALESCE(");
                render_list(v, cx, out);
                out.push(')');
            }
            Expr::NullIf(a, b) => {
                out.push_str("NULLIF(");
                a.render(cx, out);
                out.push_str(", ");
                b.render(cx, out);
                out.push(')');
            }
            Expr::Cast {
                e,
                ty,
                spelling,
                func_syntax,
            } => {
                if *func_syntax {
                    out.push_str("CAST(");
                    e.render(cx, out);
                    let _ = write!(out, " AS {})", ty.sql(*spelling));
                } else {
                    out.push('(');
                    e.render(cx, out);
                    let _ = write!(out, "::{})", ty.sql(*spelling));
                }
            }
            Expr::Func { f, arg } => {
                out.push_str(match f {
                    Func::Length => "length(",
                    Func::Lower => "lower(",
                    Func::Upper => "upper(",
                    Func::Abs => "abs(",
                });
                arg.render(cx, out);
                out.push(')');
            }
            Expr::Agg { f, arg, distinct } => {
                out.push_str(match f {
                    AggFunc::CountStar => "count(*",
                    AggFunc::Count => "count(",
                    AggFunc::Sum => "sum(",
                    AggFunc::Avg => "avg(",
                    AggFunc::Min => "min(",
                    AggFunc::Max => "max(",
                    AggFunc::BoolAnd => "bool_and(",
                    AggFunc::BoolOr => "bool_or(",
                });
                if *distinct {
                    out.push_str("DISTINCT ");
                }
                if let Some(a) = arg {
                    a.render(cx, out);
                }
                out.push(')');
            }
            Expr::Sub { kind, lhs, query } => {
                let inner = query.render(cx.names);
                match kind {
                    SubKind::Exists => {
                        let _ = write!(out, "(EXISTS ({inner}))");
                    }
                    SubKind::In { neg } => {
                        out.push('(');
                        if let Some(l) = lhs {
                            l.render(cx, out);
                        }
                        let kw = if *neg { "NOT IN" } else { "IN" };
                        let _ = write!(out, " {kw} ({inner}))");
                    }
                    SubKind::Scalar => {
                        let _ = write!(out, "({inner})");
                    }
                }
            }
        }
    }

    /// Direct subexpressions of the current query level (subqueries are opaque).
    pub(crate) fn children(&self) -> Vec<&Expr> {
        match self {
            Expr::Col { .. } | Expr::Lit { .. } => vec![],
            Expr::Unary { e, .. }
            | Expr::IsNull { e, .. }
            | Expr::IsBool { e, .. }
            | Expr::Cast { e, .. }
            | Expr::Func { arg: e, .. } => vec![e],
            Expr::Binary { l, r, .. } | Expr::Like { e: l, pat: r, .. } | Expr::NullIf(l, r) => {
                vec![l, r]
            }
            Expr::Between { e, lo, hi, .. } => vec![e, lo, hi],
            Expr::InList { e, list, .. } => {
                let mut v: Vec<&Expr> = vec![e];
                v.extend(list.iter());
                v
            }
            Expr::Case {
                operand,
                whens,
                els,
            } => {
                let mut v: Vec<&Expr> = Vec::new();
                if let Some(o) = operand {
                    v.push(o);
                }
                for (w, t) in whens {
                    v.push(w);
                    v.push(t);
                }
                if let Some(e) = els {
                    v.push(e);
                }
                v
            }
            Expr::Coalesce(list) => list.iter().collect(),
            Expr::Agg { arg, .. } => arg.iter().map(AsRef::as_ref).collect(),
            Expr::Sub { lhs, .. } => lhs.iter().map(AsRef::as_ref).collect(),
        }
    }

    /// Same order as [`Expr::children`].
    pub(crate) fn children_mut(&mut self) -> Vec<&mut Expr> {
        match self {
            Expr::Col { .. } | Expr::Lit { .. } => vec![],
            Expr::Unary { e, .. }
            | Expr::IsNull { e, .. }
            | Expr::IsBool { e, .. }
            | Expr::Cast { e, .. }
            | Expr::Func { arg: e, .. } => vec![e],
            Expr::Binary { l, r, .. } | Expr::Like { e: l, pat: r, .. } | Expr::NullIf(l, r) => {
                vec![l, r]
            }
            Expr::Between { e, lo, hi, .. } => vec![e, lo, hi],
            Expr::InList { e, list, .. } => {
                let mut v: Vec<&mut Expr> = vec![e];
                v.extend(list.iter_mut());
                v
            }
            Expr::Case {
                operand,
                whens,
                els,
            } => {
                let mut v: Vec<&mut Expr> = Vec::new();
                if let Some(o) = operand {
                    v.push(o);
                }
                for (w, t) in whens {
                    v.push(w);
                    v.push(t);
                }
                if let Some(e) = els {
                    v.push(e);
                }
                v
            }
            Expr::Coalesce(list) => list.iter_mut().collect(),
            Expr::Agg { arg, .. } => arg.iter_mut().map(AsMut::as_mut).collect(),
            Expr::Sub { lhs, .. } => lhs.iter_mut().map(AsMut::as_mut).collect(),
        }
    }

    /// Visits every column reference, including those inside subqueries.
    pub(crate) fn visit_cols(&self, f: &mut dyn FnMut(&Expr)) {
        if let Expr::Col { .. } = self {
            f(self);
        }
        if let Expr::Sub { query, .. } = self {
            query.visit_cols(f);
        }
        for c in self.children() {
            c.visit_cols(f);
        }
    }

    pub(crate) fn visit_cols_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        if let Expr::Col { .. } = self {
            f(self);
        }
        if let Expr::Sub { query, .. } = self {
            query.visit_cols_mut(f);
        }
        for c in self.children_mut() {
            c.visit_cols_mut(f);
        }
    }

    pub(crate) fn contains_col(&self) -> bool {
        let mut found = false;
        self.visit_cols(&mut |_| found = true);
        found
    }

    /// Smaller variants of this expression with the same category.
    pub(crate) fn shrinks(&self, s: &Schema) -> Vec<Expr> {
        let cat = self.cat(s);
        let mut out = Vec::new();
        let children = self.children();
        for c in &children {
            if !c.is_bare_null() && c.cat(s) == cat {
                out.push((*c).clone());
            }
        }
        if !matches!(self, Expr::Lit { .. } | Expr::Col { .. }) {
            out.push(Expr::simple(cat));
            out.push(Expr::lit(Lit::Null, Ty::default_for(cat), LitForm::Cast));
        }
        // Node-specific list shortening.
        match self {
            Expr::InList { e, list, neg } if list.len() > 1 => {
                for i in 0..list.len() {
                    let mut l = list.clone();
                    l.remove(i);
                    out.push(Expr::InList {
                        e: e.clone(),
                        list: l,
                        neg: *neg,
                    });
                }
            }
            Expr::Coalesce(list) if list.len() > 1 => {
                for i in 1..list.len() {
                    let mut l = list.clone();
                    l.remove(i);
                    out.push(Expr::Coalesce(l));
                }
            }
            Expr::Case {
                operand,
                whens,
                els,
            } => {
                if els.is_some() {
                    out.push(Expr::Case {
                        operand: operand.clone(),
                        whens: whens.clone(),
                        els: None,
                    });
                }
                if whens.len() > 1 {
                    for i in 0..whens.len() {
                        let mut w = whens.clone();
                        w.remove(i);
                        out.push(Expr::Case {
                            operand: operand.clone(),
                            whens: w,
                            els: els.clone(),
                        });
                    }
                }
            }
            Expr::Sub { query, kind, lhs } => {
                for q in query.shrinks(s) {
                    out.push(Expr::Sub {
                        kind: *kind,
                        lhs: lhs.clone(),
                        query: Box::new(q),
                    });
                }
            }
            _ => {}
        }
        for (i, c) in children.iter().enumerate() {
            for cs in c.shrinks(s) {
                let mut e = self.clone();
                *e.children_mut().swap_remove(i) = cs;
                out.push(e);
            }
        }
        out
    }
}

fn render_list(list: &[Expr], cx: &Rctx<'_>, out: &mut String) {
    for (i, e) in list.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        e.render(cx, out);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JoinKind {
    Comma,
    Cross,
    Inner,
    Left,
    Right,
    Full,
}

#[derive(Clone, Debug)]
pub(crate) struct FromItem {
    pub(crate) table: usize,
    pub(crate) alias: Option<String>,
    /// How this item joins the items before it (`None` for the first item).
    pub(crate) join: Option<(JoinKind, Option<Expr>)>,
}

#[derive(Clone, Debug)]
pub(crate) enum SelItem {
    Star,
    Expr(Expr, Option<String>),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OrderKey {
    /// 1-based output column number.
    pub(crate) pos: usize,
    pub(crate) desc: bool,
    pub(crate) nulls_first: Option<bool>,
}

#[derive(Clone, Debug)]
pub(crate) struct Select {
    pub(crate) distinct: bool,
    pub(crate) items: Vec<SelItem>,
    pub(crate) from: Vec<FromItem>,
    pub(crate) where_: Option<Expr>,
    /// Aggregate query (GROUP BY and/or aggregate calls).
    pub(crate) grouped: bool,
    pub(crate) group_by: Vec<Expr>,
    pub(crate) having: Option<Expr>,
    /// Either empty or a key for every output column (a total order on output rows).
    pub(crate) order_by: Vec<OrderKey>,
    pub(crate) limit: Option<u32>,
    pub(crate) offset: Option<u32>,
}

impl Select {
    pub(crate) fn ordered(&self) -> bool {
        !self.order_by.is_empty()
    }

    fn item_width(&self, item: &SelItem, s: &Schema) -> usize {
        match item {
            SelItem::Star => self.from.iter().map(|f| s.tables[f.table].cols.len()).sum(),
            SelItem::Expr(..) => 1,
        }
    }

    pub(crate) fn width(&self, s: &Schema) -> usize {
        self.items.iter().map(|i| self.item_width(i, s)).sum()
    }

    pub(crate) fn quals(&self, names: &[String]) -> Vec<String> {
        self.from
            .iter()
            .map(|f| f.alias.clone().unwrap_or_else(|| names[f.table].clone()))
            .collect()
    }

    pub(crate) fn render(&self, names: &[String]) -> String {
        let cx = Rctx {
            names,
            quals: self.quals(names),
        };
        let mut s = String::from("SELECT ");
        if self.distinct {
            s.push_str("DISTINCT ");
        }
        for (i, it) in self.items.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            match it {
                SelItem::Star => s.push('*'),
                SelItem::Expr(e, alias) => {
                    e.render(&cx, &mut s);
                    if let Some(a) = alias {
                        let _ = write!(s, " AS {a}");
                    }
                }
            }
        }
        s.push_str(" FROM ");
        for (k, f) in self.from.iter().enumerate() {
            let mut on = None;
            if let Some((kind, cond)) = &f.join {
                s.push_str(match kind {
                    JoinKind::Comma => ", ",
                    JoinKind::Cross => " CROSS JOIN ",
                    JoinKind::Inner => " JOIN ",
                    JoinKind::Left => " LEFT JOIN ",
                    JoinKind::Right => " RIGHT JOIN ",
                    JoinKind::Full => " FULL JOIN ",
                });
                on = cond.as_ref();
            }
            s.push_str(&names[f.table]);
            if let Some(a) = &f.alias {
                let _ = write!(s, " AS {a}");
            }
            if let Some(c) = on {
                s.push_str(" ON ");
                c.render(&cx, &mut s);
            }
            let _ = k;
        }
        if let Some(w) = &self.where_ {
            s.push_str(" WHERE ");
            w.render(&cx, &mut s);
        }
        if !self.group_by.is_empty() {
            s.push_str(" GROUP BY ");
            render_list(&self.group_by, &cx, &mut s);
        }
        if let Some(h) = &self.having {
            s.push_str(" HAVING ");
            h.render(&cx, &mut s);
        }
        if !self.order_by.is_empty() {
            s.push_str(" ORDER BY ");
            for (i, k) in self.order_by.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                let _ = write!(s, "{}", k.pos);
                if k.desc {
                    s.push_str(" DESC");
                }
                match k.nulls_first {
                    Some(true) => s.push_str(" NULLS FIRST"),
                    Some(false) => s.push_str(" NULLS LAST"),
                    None => {}
                }
            }
        }
        if let Some(l) = self.limit {
            let _ = write!(s, " LIMIT {l}");
        }
        if let Some(o) = self.offset {
            let _ = write!(s, " OFFSET {o}");
        }
        s
    }

    fn exprs(&self) -> Vec<&Expr> {
        let mut v: Vec<&Expr> = Vec::new();
        for it in &self.items {
            if let SelItem::Expr(e, _) = it {
                v.push(e);
            }
        }
        for f in &self.from {
            if let Some((_, Some(c))) = &f.join {
                v.push(c);
            }
        }
        v.extend(self.where_.iter());
        v.extend(self.group_by.iter());
        v.extend(self.having.iter());
        v
    }

    fn exprs_mut(&mut self) -> Vec<&mut Expr> {
        let mut v: Vec<&mut Expr> = Vec::new();
        for it in &mut self.items {
            if let SelItem::Expr(e, _) = it {
                v.push(e);
            }
        }
        for f in &mut self.from {
            if let Some((_, Some(c))) = &mut f.join {
                v.push(c);
            }
        }
        v.extend(self.where_.iter_mut());
        v.extend(self.group_by.iter_mut());
        v.extend(self.having.iter_mut());
        v
    }

    pub(crate) fn visit_cols(&self, f: &mut dyn FnMut(&Expr)) {
        for e in self.exprs() {
            e.visit_cols(f);
        }
    }

    pub(crate) fn visit_cols_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        for e in self.exprs_mut() {
            e.visit_cols_mut(f);
        }
    }

    /// Calls `f` with every table index referenced by FROM items (including subqueries).
    pub(crate) fn visit_tables_mut(&mut self, f: &mut dyn FnMut(&mut usize)) {
        for it in &mut self.from {
            f(&mut it.table);
        }
        for e in self.exprs_mut() {
            visit_sub_tables_mut(e, f);
        }
    }

    /// Tables whose every column is referenced (`SELECT *`), including subqueries.
    pub(crate) fn star_tables(&self, out: &mut Vec<usize>) {
        if self.items.iter().any(|i| matches!(i, SelItem::Star)) {
            out.extend(self.from.iter().map(|f| f.table));
        }
        for e in self.exprs() {
            collect_sub_star(e, out);
        }
    }

    /// The same query without ORDER BY / LIMIT / OFFSET (for TLP).
    pub(crate) fn unordered(&self) -> Select {
        let mut q = self.clone();
        q.order_by.clear();
        q.limit = None;
        q.offset = None;
        q
    }

    fn remove_item(&self, i: usize, s: &Schema) -> Select {
        let start: usize = self.items[..i]
            .iter()
            .map(|it| self.item_width(it, s))
            .sum::<usize>()
            + 1;
        let w = self.item_width(&self.items[i], s);
        let mut q = self.clone();
        q.items.remove(i);
        if !q.order_by.is_empty() {
            q.order_by = self
                .order_by
                .iter()
                .filter(|k| k.pos < start || k.pos >= start + w)
                .map(|k| OrderKey {
                    pos: if k.pos >= start + w { k.pos - w } else { k.pos },
                    ..*k
                })
                .collect();
        }
        q
    }

    /// Smaller variants of this query (used by the minimizer).
    pub(crate) fn shrinks(&self, s: &Schema) -> Vec<Select> {
        let mut out = Vec::new();
        let with = |f: &dyn Fn(&mut Select)| {
            let mut q = self.clone();
            f(&mut q);
            q
        };
        if self.items.len() > 1 {
            for i in 0..self.items.len() {
                out.push(self.remove_item(i, s));
            }
        }
        if self.where_.is_some() {
            out.push(with(&|q| q.where_ = None));
        }
        if self.having.is_some() {
            out.push(with(&|q| q.having = None));
        }
        if self.distinct {
            out.push(with(&|q| q.distinct = false));
        }
        if self.limit.is_some() {
            out.push(with(&|q| q.limit = None));
        }
        if self.offset.is_some() {
            out.push(with(&|q| q.offset = None));
        }
        if self.ordered() && self.limit.is_none() && self.offset.is_none() {
            out.push(with(&|q| q.order_by.clear()));
        }
        // Drop the last FROM item when nothing refers to it.
        if self.from.len() > 1 && !self.items.iter().any(|i| matches!(i, SelItem::Star)) {
            let last = self.from.len() - 1;
            let mut used = false;
            self.visit_cols(&mut |e| {
                if let Expr::Col { item, .. } = e {
                    used |= *item == last;
                }
            });
            if !used {
                out.push(with(&|q| {
                    q.from.pop();
                }));
            }
        }
        // Drop the alias / qualification of a single-table query.
        if self.from.len() == 1 {
            let mut qualified = false;
            self.visit_cols(&mut |e| {
                if let Expr::Col { qualify, .. } = e {
                    qualified |= *qualify;
                }
            });
            if qualified || self.from[0].alias.is_some() {
                let mut q = self.clone();
                q.from[0].alias = None;
                q.visit_cols_mut(&mut |e| {
                    if let Expr::Col { qualify, .. } = e {
                        *qualify = false;
                    }
                });
                out.push(q);
            }
        }
        // Simplify join kinds.
        for k in 1..self.from.len() {
            match &self.from[k].join {
                Some((JoinKind::Left | JoinKind::Right | JoinKind::Full, on)) => {
                    let on = on.clone();
                    out.push(with(&|q| {
                        q.from[k].join = Some((JoinKind::Inner, on.clone()));
                    }));
                }
                Some((JoinKind::Inner, _)) => {
                    out.push(with(&|q| q.from[k].join = Some((JoinKind::Cross, None))));
                }
                _ => {}
            }
        }
        // Shrink individual expressions.
        let n = self.exprs().len();
        for i in 0..n {
            let e = self.exprs()[i];
            for cand in e.shrinks(s) {
                let mut q = self.clone();
                *q.exprs_mut().swap_remove(i) = cand;
                out.push(q);
            }
        }
        out
    }
}

fn visit_sub_tables_mut(e: &mut Expr, f: &mut dyn FnMut(&mut usize)) {
    if let Expr::Sub { query, .. } = e {
        query.visit_tables_mut(f);
    }
    for c in e.children_mut() {
        visit_sub_tables_mut(c, f);
    }
}

fn collect_sub_star(e: &Expr, out: &mut Vec<usize>) {
    if let Expr::Sub { query, .. } = e {
        query.star_tables(out);
    }
    for c in e.children() {
        collect_sub_star(c, out);
    }
}
