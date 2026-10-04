//! Random generation of schemas, rows and queries.
//!
//! Generation only consumes the random stream; it never looks at server
//! responses, so a seed always yields the same SQL.

use crate::ast::{
    AggFunc, BinOp, Expr, FromItem, Func, JoinKind, OrderKey, SelItem, Select, SubKind, UnOp,
};
use crate::rng::Rng;
use crate::schema::{
    ALL_CATS, Cat, Check, Column, Insert, Lit, LitForm, Schema, Table, Ty, Val, bare_int_ty,
    gen_float, gen_int, gen_lit, gen_text, int_range,
};

/// Optional SQL features, grouped by the milestone that introduces them.
#[derive(Clone, Copy, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Features {
    pub(crate) joins: bool,
    pub(crate) aggregates: bool,
    pub(crate) subqueries: bool,
    pub(crate) decimal_literals: bool,
    pub(crate) tlp: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct GenConfig {
    pub(crate) features: Features,
    pub(crate) max_tables: usize,
    pub(crate) max_cols: usize,
    pub(crate) max_rows: usize,
    pub(crate) max_depth: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Qual {
    Never,
    Sometimes,
    Always,
}

/// What an expression may refer to.
#[derive(Clone, Debug)]
pub(crate) struct Scope {
    /// Table index of each visible FROM item.
    pub(crate) items: Vec<usize>,
    pub(crate) qualify: Qual,
    /// Restrict column references to this column of item 0 (column CHECK).
    pub(crate) only_col: Option<usize>,
    /// Grouped context: leaves are these expressions, aggregates or literals.
    pub(crate) group: Option<Vec<Expr>>,
    pub(crate) allow_sub: bool,
}

impl Scope {
    fn plain(items: Vec<usize>, qualify: Qual) -> Scope {
        Scope {
            items,
            qualify,
            only_col: None,
            group: None,
            allow_sub: true,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Gen<'a> {
    pub(crate) rng: &'a mut Rng,
    pub(crate) cfg: &'a GenConfig,
    pub(crate) schema: &'a Schema,
}

pub(crate) fn gen_schema(rng: &mut Rng, cfg: &GenConfig) -> Schema {
    let mut schema = Schema::default();
    let ntables = 1 + rng.below(cfg.max_tables.max(1));
    for t in 0..ntables {
        let ncols = 1 + rng.below(cfg.max_cols.max(1));
        let mut cols = Vec::new();
        for _ in 0..ncols {
            let ty = Ty::random(rng);
            let default = if rng.chance(1, 4) {
                let lit = if rng.chance(1, 10) {
                    Lit::Null
                } else {
                    gen_lit(rng, ty)
                };
                let form = if rng.chance(3, 4) || lit == Lit::Null {
                    LitForm::Bare
                } else {
                    LitForm::Cast
                };
                Some((lit, form))
            } else {
                None
            };
            cols.push(Column {
                ty,
                spelling: u8::try_from(rng.below(3)).expect("small"),
                not_null: rng.chance(1, 5),
                default,
            });
        }
        schema.tables.push(Table {
            cols,
            checks: Vec::new(),
        });
        let mut checks = Vec::new();
        {
            let mut g = Gen {
                rng: &mut *rng,
                cfg,
                schema: &schema,
            };
            for c in 0..ncols {
                if g.rng.chance(1, 5) {
                    let expr = g.column_check(t, c);
                    let named = g.rng.chance(1, 3);
                    checks.push(Check {
                        named,
                        expr,
                        column: Some(c),
                    });
                }
            }
            if g.rng.chance(1, 7) {
                let sc = Scope {
                    items: vec![t],
                    qualify: Qual::Never,
                    only_col: None,
                    group: None,
                    allow_sub: false,
                };
                let expr = g.expr(&sc, Cat::Bool, 2);
                let named = g.rng.chance(1, 2);
                checks.push(Check {
                    named,
                    expr,
                    column: None,
                });
            }
        }
        schema.tables[t].checks = checks;
    }
    schema
}

pub(crate) fn gen_inserts(rng: &mut Rng, cfg: &GenConfig, schema: &Schema) -> Vec<Insert> {
    let mut out = Vec::new();
    for (t, table) in schema.tables.iter().enumerate() {
        let target = rng.below(cfg.max_rows + 1);
        let mut produced = 0;
        while produced < target {
            let n = table.cols.len();
            let cols = match rng.weighted(&[55, 40, 5]) {
                0 => None,
                1 => {
                    let mut idx: Vec<usize> = (0..n).collect();
                    rng.shuffle(&mut idx);
                    idx.truncate(1 + rng.below(n));
                    Some(idx)
                }
                _ => Some(Vec::new()),
            };
            let nrows = match &cols {
                Some(c) if c.is_empty() => 1,
                _ if rng.chance(3, 20) => 2 + rng.below(2),
                _ => 1,
            };
            let col_idx: Vec<usize> = cols.clone().unwrap_or_else(|| (0..n).collect());
            let mut rows = Vec::new();
            for _ in 0..nrows {
                let row = col_idx
                    .iter()
                    .map(|&c| gen_val(rng, table.cols[c].ty))
                    .collect();
                rows.push(row);
            }
            if cols.as_ref().is_some_and(Vec::is_empty) {
                rows = vec![Vec::new()];
            }
            produced += nrows;
            out.push(Insert {
                table: t,
                cols,
                rows,
            });
        }
    }
    out
}

fn gen_val(rng: &mut Rng, ty: Ty) -> Val {
    match rng.weighted(&[8, 5, 6, 81]) {
        0 => Val::Lit(Lit::Null, LitForm::Bare, ty),
        1 => Val::Default,
        2 => edge_val(rng, ty),
        _ => {
            let lit = gen_lit(rng, ty);
            if rng.chance(3, 20) {
                // Typed literal, possibly of a different type of the same
                // category (exercises assignment casts).
                Val::Lit(lit, LitForm::Cast, Ty::random_of(rng, ty.cat()))
            } else {
                Val::Lit(lit, LitForm::Bare, ty)
            }
        }
    }
}

/// Values that are likely to be rejected or coerced in interesting ways.
fn edge_val(rng: &mut Rng, ty: Ty) -> Val {
    match ty {
        Ty::Int2 | Ty::Int4 => {
            let wider = if ty == Ty::Int2 { Ty::Int4 } else { Ty::Int8 };
            let (lo, hi) = int_range(ty);
            let w = gen_int(rng, wider);
            let v = *rng.pick(&[lo - 1, hi + 1, w]);
            Val::Lit(Lit::Int(v), LitForm::Bare, wider)
        }
        Ty::Int8 | Ty::Float4 | Ty::Float8 | Ty::Bool => {
            let s = match ty.cat() {
                Cat::Int => *rng.pick(&[
                    " 12 ",
                    "+3",
                    "0x1F",
                    "1_000",
                    "1.5",
                    "abc",
                    "",
                    "9223372036854775808",
                ]),
                Cat::Float => *rng.pick(&[
                    " 1.5 ",
                    "1e39",
                    "-1e400",
                    "inf",
                    "-Infinity",
                    "nan",
                    "1e-400",
                    "x",
                ]),
                _ => *rng.pick(&["yes", "no", "on", "off", "1", "0", " t ", "tr", "maybe"]),
            };
            Val::Lit(Lit::Text(s.to_string()), LitForm::Bare, Ty::Text)
        }
        Ty::Text => Val::Lit(Lit::Text(gen_text(rng, None)), LitForm::Bare, Ty::Text),
        Ty::Varchar(n) => {
            let mut s = gen_text(rng, None);
            while s.chars().count() <= n as usize {
                s.push(*rng.pick(&['z', ' ']));
            }
            Val::Lit(Lit::Text(s), LitForm::Bare, Ty::Text)
        }
    }
}

impl Gen<'_> {
    fn column_check(&mut self, t: usize, c: usize) -> Expr {
        let ty = self.schema.tables[t].cols[c].ty;
        let col = Expr::Col {
            item: 0,
            table: t,
            col: c,
            qualify: false,
        };
        if self.rng.chance(1, 2) {
            let sc = Scope {
                items: vec![t],
                qualify: Qual::Never,
                only_col: Some(c),
                group: None,
                allow_sub: false,
            };
            return self.expr(&sc, Cat::Bool, 2);
        }
        // Permissive templates so that most rows still get in.
        match ty.cat() {
            Cat::Int | Cat::Float => {
                if self.rng.chance(1, 2) {
                    let lit = self.literal(ty.cat());
                    Expr::bin(BinOp::Ne, col, lit)
                } else {
                    Expr::bin(
                        BinOp::Gt,
                        col,
                        Expr::lit(Lit::Int(-1000), Ty::Int4, LitForm::Bare),
                    )
                }
            }
            Cat::Text => Expr::bin(
                BinOp::Lt,
                Expr::Func {
                    f: Func::Length,
                    arg: Box::new(col),
                },
                Expr::lit(Lit::Int(6), Ty::Int4, LitForm::Bare),
            ),
            Cat::Bool => Expr::IsBool {
                e: Box::new(col),
                neg: true,
                val: false,
            },
        }
    }

    fn any_cat(&mut self) -> Cat {
        *self.rng.pick(&ALL_CATS)
    }

    pub(crate) fn expr(&mut self, sc: &Scope, cat: Cat, depth: u32) -> Expr {
        if depth == 0 || self.rng.chance(1, 5) {
            return self.leaf(sc, cat);
        }
        let d = depth - 1;
        if self.rng.chance(3, 20) {
            return self.common(sc, cat, d);
        }
        match cat {
            Cat::Int => self.int_expr(sc, d),
            Cat::Float => self.float_expr(sc, d),
            Cat::Text => self.text_expr(sc, d),
            Cat::Bool => self.bool_expr(sc, d),
        }
    }

    /// Like `expr`, but may produce an untyped `NULL` (only used where the
    /// other operand determines the type).
    fn operand2(&mut self, sc: &Scope, cat: Cat, d: u32) -> Expr {
        if self.rng.chance(1, 14) {
            Expr::lit(Lit::Null, Ty::default_for(cat), LitForm::Bare)
        } else {
            self.expr(sc, cat, d)
        }
    }

    fn int_expr(&mut self, sc: &Scope, d: u32) -> Expr {
        match self.rng.weighted(&[50, 9, 7, 9, 18]) {
            0 => {
                let op = *self.rng.pick(&BinOp::ARITH);
                let l = self.expr(sc, Cat::Int, d);
                let r = self.operand2(sc, Cat::Int, d);
                Expr::bin(op, l, r)
            }
            1 => {
                let op = if self.rng.chance(4, 5) {
                    UnOp::Neg
                } else {
                    UnOp::Plus
                };
                Expr::Unary {
                    op,
                    e: Box::new(self.expr(sc, Cat::Int, d)),
                }
            }
            2 => Expr::Func {
                f: Func::Abs,
                arg: Box::new(self.expr(sc, Cat::Int, d)),
            },
            3 => Expr::Func {
                f: Func::Length,
                arg: Box::new(self.expr(sc, Cat::Text, d)),
            },
            _ => self.cast_to(sc, Cat::Int, d),
        }
    }

    fn float_expr(&mut self, sc: &Scope, d: u32) -> Expr {
        match self.rng.weighted(&[50, 9, 7, 18]) {
            0 => {
                let op = *self
                    .rng
                    .pick(&[BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::Div]);
                let other = if self.rng.chance(1, 2) {
                    Cat::Float
                } else {
                    Cat::Int
                };
                if self.rng.chance(1, 2) {
                    let l = self.expr(sc, Cat::Float, d);
                    let r = self.operand2(sc, other, d);
                    Expr::bin(op, l, r)
                } else {
                    let l = self.expr(sc, other, d);
                    let r = self.expr(sc, Cat::Float, d);
                    Expr::bin(op, l, r)
                }
            }
            1 => Expr::Unary {
                op: UnOp::Neg,
                e: Box::new(self.expr(sc, Cat::Float, d)),
            },
            2 => Expr::Func {
                f: Func::Abs,
                arg: Box::new(self.expr(sc, Cat::Float, d)),
            },
            _ => self.cast_to(sc, Cat::Float, d),
        }
    }

    fn text_expr(&mut self, sc: &Scope, d: u32) -> Expr {
        match self.rng.weighted(&[40, 20, 25]) {
            0 => {
                let l = self.expr(sc, Cat::Text, d);
                let r = self.operand2(sc, Cat::Text, d);
                Expr::bin(BinOp::Concat, l, r)
            }
            1 => Expr::Func {
                f: if self.rng.chance(1, 2) {
                    Func::Lower
                } else {
                    Func::Upper
                },
                arg: Box::new(self.expr(sc, Cat::Text, d)),
            },
            _ => self.cast_to(sc, Cat::Text, d),
        }
    }

    #[allow(clippy::many_single_char_names)]
    fn bool_expr(&mut self, sc: &Scope, d: u32) -> Expr {
        let sub_ok = sc.allow_sub && self.cfg.features.subqueries;
        let w = [30, 20, 6, 8, 5, 7, 8, 8, 3, if sub_ok { 8 } else { 0 }];
        match self.rng.weighted(&w) {
            0 => {
                let c = self.any_cat();
                let op = *self.rng.pick(&BinOp::CMP);
                let l = self.expr(sc, c, d);
                let r = self.operand2(sc, c, d);
                Expr::bin(op, l, r)
            }
            1 => {
                let op = if self.rng.chance(1, 2) {
                    BinOp::And
                } else {
                    BinOp::Or
                };
                let l = self.expr(sc, Cat::Bool, d);
                let r = self.operand2(sc, Cat::Bool, d);
                Expr::bin(op, l, r)
            }
            2 => Expr::Unary {
                op: UnOp::Not,
                e: Box::new(self.expr(sc, Cat::Bool, d)),
            },
            3 => {
                let c = self.any_cat();
                Expr::IsNull {
                    e: Box::new(self.expr(sc, c, d)),
                    neg: self.rng.chance(1, 2),
                }
            }
            4 => Expr::IsBool {
                e: Box::new(self.expr(sc, Cat::Bool, d)),
                neg: self.rng.chance(1, 2),
                val: self.rng.chance(1, 2),
            },
            5 => {
                let c = self.any_cat();
                Expr::Between {
                    e: Box::new(self.expr(sc, c, d)),
                    lo: Box::new(self.operand2(sc, c, d)),
                    hi: Box::new(self.operand2(sc, c, d)),
                    neg: self.rng.chance(1, 3),
                }
            }
            6 => {
                let c = self.any_cat();
                let e = Box::new(self.expr(sc, c, d));
                let n = 1 + self.rng.below(4);
                let list = (0..n).map(|_| self.operand2(sc, c, d.min(1))).collect();
                Expr::InList {
                    e,
                    list,
                    neg: self.rng.chance(1, 3),
                }
            }
            7 => {
                let e = Box::new(self.expr(sc, Cat::Text, d));
                let pat = if self.rng.chance(2, 3) {
                    let p = *self.rng.pick(&[
                        "%", "_", "a%", "%a", "%b%", "a_c", "", "A%", "\\%", "%\\_%", "_%_", "%\\",
                        "a%c", "%%", " %",
                    ]);
                    Expr::lit(Lit::Text(p.to_string()), Ty::Text, LitForm::Bare)
                } else {
                    self.operand2(sc, Cat::Text, d)
                };
                Expr::Like {
                    e,
                    pat: Box::new(pat),
                    neg: self.rng.chance(1, 3),
                }
            }
            8 => self.cast_to(sc, Cat::Bool, d),
            _ => self.subquery_pred(sc, d),
        }
    }

    /// CASE / COALESCE / NULLIF / CAST / scalar subquery producing `cat`.
    #[allow(clippy::many_single_char_names)]
    fn common(&mut self, sc: &Scope, cat: Cat, d: u32) -> Expr {
        let sub_ok = sc.allow_sub && self.cfg.features.subqueries;
        match self
            .rng
            .weighted(&[40, 25, 20, 15, if sub_ok { 6 } else { 0 }])
        {
            0 => {
                let n = 1 + self.rng.below(3);
                let mut whens = Vec::new();
                let operand = if self.rng.chance(2, 5) {
                    let oc = self.any_cat();
                    let o = self.expr(sc, oc, d);
                    for i in 0..n {
                        let v = self.operand2(sc, oc, d);
                        let r = if i == 0 {
                            self.expr(sc, cat, d)
                        } else {
                            self.operand2(sc, cat, d)
                        };
                        whens.push((v, r));
                    }
                    Some(Box::new(o))
                } else {
                    for i in 0..n {
                        let c = self.expr(sc, Cat::Bool, d);
                        let r = if i == 0 {
                            self.expr(sc, cat, d)
                        } else {
                            self.operand2(sc, cat, d)
                        };
                        whens.push((c, r));
                    }
                    None
                };
                let els = if self.rng.chance(7, 10) {
                    Some(Box::new(self.operand2(sc, cat, d)))
                } else {
                    None
                };
                Expr::Case {
                    operand,
                    whens,
                    els,
                }
            }
            1 => {
                let n = 2 + self.rng.below(2);
                let mut v = vec![self.expr(sc, cat, d)];
                for _ in 1..n {
                    v.push(self.operand2(sc, cat, d));
                }
                Expr::Coalesce(v)
            }
            2 => {
                let a = self.expr(sc, cat, d);
                let b = self.operand2(sc, cat, d);
                Expr::NullIf(Box::new(a), Box::new(b))
            }
            3 => self.cast_to(sc, cat, d),
            _ => {
                let q = self.sub_select(cat, true);
                Expr::Sub {
                    kind: SubKind::Scalar,
                    lhs: None,
                    query: Box::new(q),
                }
            }
        }
    }

    fn cast(&mut self, e: Expr, ty: Ty) -> Expr {
        Expr::Cast {
            e: Box::new(e),
            ty,
            spelling: u8::try_from(self.rng.below(3)).expect("small"),
            func_syntax: self.rng.chance(1, 3),
        }
    }

    fn text_lit(s: &str) -> Expr {
        Expr::lit(Lit::Text(s.to_string()), Ty::Text, LitForm::Bare)
    }

    fn cast_to(&mut self, sc: &Scope, cat: Cat, d: u32) -> Expr {
        let ty = Ty::random_of(self.rng, cat);
        match cat {
            Cat::Int => match self.rng.weighted(&[30, 35, 10, 25]) {
                0 => {
                    let e = self.expr(sc, Cat::Int, d);
                    self.cast(e, ty)
                }
                1 => {
                    let e = self.expr(sc, Cat::Float, d);
                    self.cast(e, ty)
                }
                2 => {
                    // bool -> int4 is the only boolean/integer cast.
                    let e = self.expr(sc, Cat::Bool, d);
                    let e = self.cast(e, Ty::Int4);
                    if ty == Ty::Int4 { e } else { self.cast(e, ty) }
                }
                _ => {
                    let src = if self.rng.chance(3, 5) {
                        let s = *self.rng.pick(&[
                            "12",
                            " -7 ",
                            "+3",
                            "0x1F",
                            "0o17",
                            "0b101",
                            "1_000",
                            "1.5",
                            "32768",
                            "-2147483648",
                            "2147483648",
                            "abc",
                            "",
                            "9223372036854775807",
                        ]);
                        Self::text_lit(s)
                    } else {
                        let e = self.expr(sc, Cat::Int, d);
                        self.cast(e, Ty::Text)
                    };
                    self.cast(src, ty)
                }
            },
            Cat::Float => match self.rng.weighted(&[40, 25, 35]) {
                0 => {
                    let e = self.expr(sc, Cat::Int, d);
                    self.cast(e, ty)
                }
                1 => {
                    let e = self.expr(sc, Cat::Float, d);
                    self.cast(e, ty)
                }
                _ => {
                    let src = if self.rng.chance(3, 5) {
                        let s = *self.rng.pick(&[
                            " 1.5 ", "NaN", "-inf", "Infinity", "1e39", "1e-50", "-0", "abc", "",
                            "4.4e-39", "1e400", ".5", "5.", "+1.25e+2",
                        ]);
                        Self::text_lit(s)
                    } else {
                        let e = self.expr(sc, Cat::Float, d);
                        self.cast(e, Ty::Text)
                    };
                    self.cast(src, ty)
                }
            },
            Cat::Text => {
                let c = self.any_cat();
                let e = self.expr(sc, c, d);
                self.cast(e, ty)
            }
            Cat::Bool => {
                if self.rng.chance(1, 2) {
                    let e = self.expr(sc, Cat::Int, d);
                    let e = self.cast(e, Ty::Int4);
                    self.cast(e, Ty::Bool)
                } else {
                    let s = *self.rng.pick(&[
                        "t", "f", "yes", "no", "on", "off", "1", "0", " true ", "tr", "maybe",
                        "TRUE",
                    ]);
                    self.cast(Self::text_lit(s), Ty::Bool)
                }
            }
        }
    }

    fn leaf(&mut self, sc: &Scope, cat: Cat) -> Expr {
        if let Some(groups) = &sc.group {
            let matching: Vec<&Expr> = groups
                .iter()
                .filter(|g| g.cat(self.schema) == cat)
                .collect();
            if !matching.is_empty() && self.rng.chance(1, 2) {
                return (*self.rng.pick(&matching)).clone();
            }
            if self.rng.chance(3, 4) {
                return self.aggregate(sc, cat);
            }
            return self.literal(cat);
        }
        if self.rng.chance(2, 3) {
            if let Some(c) = self.column(sc, cat) {
                return c;
            }
        }
        self.literal(cat)
    }

    pub(crate) fn column(&mut self, sc: &Scope, cat: Cat) -> Option<Expr> {
        let mut cands = Vec::new();
        for (item, &table) in sc.items.iter().enumerate() {
            for (col, c) in self.schema.tables[table].cols.iter().enumerate() {
                if c.ty.cat() == cat && sc.only_col.is_none_or(|oc| oc == col && item == 0) {
                    cands.push((item, table, col));
                }
            }
        }
        if cands.is_empty() {
            return None;
        }
        let (item, table, col) = *self.rng.pick(&cands);
        let qualify = match sc.qualify {
            Qual::Never => false,
            Qual::Always => true,
            Qual::Sometimes => self.rng.chance(3, 10),
        };
        Some(Expr::Col {
            item,
            table,
            col,
            qualify,
        })
    }

    pub(crate) fn literal(&mut self, cat: Cat) -> Expr {
        if self.rng.chance(1, 14) {
            let ty = Ty::random_of(self.rng, cat);
            let form = if self.rng.chance(1, 2) {
                LitForm::Cast
            } else {
                LitForm::CastFn
            };
            return Expr::lit(Lit::Null, ty, form);
        }
        match cat {
            Cat::Int => {
                let ty = Ty::random_of(self.rng, cat);
                let v = gen_int(self.rng, ty);
                if self.rng.chance(7, 10) {
                    Expr::lit(Lit::Int(v), bare_int_ty(v), LitForm::Bare)
                } else {
                    Expr::lit(Lit::Int(v), ty, LitForm::Cast)
                }
            }
            Cat::Float => {
                let ty = Ty::random_of(self.rng, cat);
                let s = gen_float(self.rng, ty);
                let plain = s
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-')
                    && s.contains('.');
                let form = if self.cfg.features.decimal_literals && plain && self.rng.chance(1, 3) {
                    LitForm::Unquoted
                } else if self.rng.chance(1, 4) {
                    LitForm::CastFn
                } else {
                    LitForm::Cast
                };
                Expr::lit(Lit::Float(s), ty, form)
            }
            Cat::Bool => Expr::lit(Lit::Bool(self.rng.chance(1, 2)), Ty::Bool, LitForm::Bare),
            Cat::Text => {
                let ty = Ty::random_of(self.rng, cat);
                let s = gen_text(self.rng, None);
                if self.rng.chance(3, 4) {
                    Expr::lit(Lit::Text(s), Ty::Text, LitForm::Bare)
                } else {
                    Expr::lit(Lit::Text(s), ty, LitForm::Cast)
                }
            }
        }
    }

    fn aggregate(&mut self, sc: &Scope, cat: Cat) -> Expr {
        let inner = Scope {
            group: None,
            allow_sub: false,
            ..sc.clone()
        };
        let (f, arg_cat) = match cat {
            Cat::Int => match self.rng.weighted(&[30, 25, 25, 20]) {
                0 => (AggFunc::CountStar, None),
                1 => {
                    let c = self.any_cat();
                    (AggFunc::Count, Some(c))
                }
                2 => (AggFunc::Sum, Some(Cat::Int)),
                _ => (
                    *self.rng.pick(&[AggFunc::Min, AggFunc::Max]),
                    Some(Cat::Int),
                ),
            },
            // sum/avg over floats depend on summation order, so they are not generated.
            Cat::Float => {
                if self.rng.chance(1, 2) {
                    (AggFunc::Avg, Some(Cat::Int))
                } else {
                    (
                        *self.rng.pick(&[AggFunc::Min, AggFunc::Max]),
                        Some(Cat::Float),
                    )
                }
            }
            Cat::Text => (
                *self.rng.pick(&[AggFunc::Min, AggFunc::Max]),
                Some(Cat::Text),
            ),
            Cat::Bool => (
                *self.rng.pick(&[AggFunc::BoolAnd, AggFunc::BoolOr]),
                Some(Cat::Bool),
            ),
        };
        let arg = arg_cat.map(|c| Box::new(self.expr(&inner, c, 1)));
        let distinct = arg.is_some() && self.rng.chance(1, 7);
        Expr::Agg { f, arg, distinct }
    }

    fn subquery_pred(&mut self, sc: &Scope, d: u32) -> Expr {
        if self.rng.chance(1, 3) {
            let c = self.any_cat();
            let q = self.sub_select(c, false);
            let e = Expr::Sub {
                kind: SubKind::Exists,
                lhs: None,
                query: Box::new(q),
            };
            if self.rng.chance(1, 3) {
                Expr::Unary {
                    op: UnOp::Not,
                    e: Box::new(e),
                }
            } else {
                e
            }
        } else {
            let c = self.any_cat();
            let lhs = self.expr(sc, c, d);
            let q = self.sub_select(c, false);
            Expr::Sub {
                kind: SubKind::In {
                    neg: self.rng.chance(1, 3),
                },
                lhs: Some(Box::new(lhs)),
                query: Box::new(q),
            }
        }
    }

    /// An uncorrelated single-column subquery. `scalar` adds `ORDER BY 1 LIMIT 1`.
    fn sub_select(&mut self, cat: Cat, scalar: bool) -> Select {
        let table = self.rng.below(self.schema.tables.len());
        let sc = Scope {
            items: vec![table],
            qualify: Qual::Sometimes,
            only_col: None,
            group: None,
            allow_sub: false,
        };
        let item = self.expr(&sc, cat, 2);
        let where_ = if self.rng.chance(3, 5) {
            Some(self.expr(&sc, Cat::Bool, 2))
        } else {
            None
        };
        Select {
            distinct: !scalar && self.rng.chance(1, 5),
            items: vec![SelItem::Expr(item, None)],
            from: vec![FromItem {
                table,
                alias: None,
                join: None,
            }],
            where_,
            grouped: false,
            group_by: Vec::new(),
            having: None,
            order_by: if scalar {
                vec![OrderKey {
                    pos: 1,
                    desc: self.rng.chance(1, 2),
                    nulls_first: None,
                }]
            } else {
                Vec::new()
            },
            limit: if scalar { Some(1) } else { None },
            offset: None,
        }
    }

    fn equi_join(&mut self, items: &[usize], k: usize) -> Option<Expr> {
        let mut pairs = Vec::new();
        for (ci, c) in self.schema.tables[items[k]].cols.iter().enumerate() {
            for (j, &tj) in items[..k].iter().enumerate() {
                for (cj, c2) in self.schema.tables[tj].cols.iter().enumerate() {
                    if c.ty.cat() == c2.ty.cat() {
                        pairs.push((j, tj, cj, ci));
                    }
                }
            }
        }
        if pairs.is_empty() {
            return None;
        }
        let (j, tj, cj, ci) = *self.rng.pick(&pairs);
        Some(Expr::bin(
            BinOp::Eq,
            Expr::Col {
                item: j,
                table: tj,
                col: cj,
                qualify: true,
            },
            Expr::Col {
                item: k,
                table: items[k],
                col: ci,
                qualify: true,
            },
        ))
    }

    fn gen_from(&mut self) -> (Vec<FromItem>, Qual) {
        let nt = self.schema.tables.len();
        let join = self.cfg.features.joins && self.rng.chance(2, 5);
        let nitems = if join { 2 + self.rng.below(2) } else { 1 };
        let mut from = Vec::new();
        for k in 0..nitems {
            let table = self.rng.below(nt);
            let alias = if join {
                Some(format!("a{k}"))
            } else if self.rng.chance(1, 5) {
                Some("x".to_string())
            } else {
                None
            };
            from.push(FromItem {
                table,
                alias,
                join: None,
            });
        }
        let items: Vec<usize> = from.iter().map(|f| f.table).collect();
        for k in 1..nitems {
            let kinds: &[JoinKind] = if k == nitems - 1 {
                &[
                    JoinKind::Comma,
                    JoinKind::Cross,
                    JoinKind::Inner,
                    JoinKind::Inner,
                    JoinKind::Left,
                    JoinKind::Right,
                    JoinKind::Full,
                ]
            } else {
                &[
                    JoinKind::Cross,
                    JoinKind::Inner,
                    JoinKind::Inner,
                    JoinKind::Left,
                    JoinKind::Right,
                    JoinKind::Full,
                ]
            };
            let mut kind = *self.rng.pick(kinds);
            let on = match kind {
                JoinKind::Comma | JoinKind::Cross => None,
                // FULL JOIN needs a merge- or hash-joinable condition.
                JoinKind::Full => {
                    let e = self.equi_join(&items, k);
                    if e.is_none() {
                        kind = JoinKind::Cross;
                    }
                    e
                }
                _ => {
                    let sc = Scope::plain(items[..=k].to_vec(), Qual::Always);
                    Some(self.expr(&sc, Cat::Bool, 2))
                }
            };
            from[k].join = Some((kind, on));
        }
        let qual = if join { Qual::Always } else { Qual::Sometimes };
        (from, qual)
    }

    pub(crate) fn select(&mut self) -> Select {
        let (from, qual) = self.gen_from();
        let items_t: Vec<usize> = from.iter().map(|f| f.table).collect();
        let base = Scope::plain(items_t, qual);
        let depth = self.cfg.max_depth;

        let grouped = self.cfg.features.aggregates && self.rng.chance(3, 10);
        let mut group_by = Vec::new();
        if grouped {
            for _ in 0..self.rng.below(3) {
                let c = self.any_cat();
                let g = if self.rng.chance(3, 4) {
                    self.column(&base, c)
                } else {
                    Some(self.expr(&base, c, 1)).filter(Expr::contains_col)
                };
                if let Some(g) = g {
                    group_by.push(g);
                }
            }
        }
        let gscope = Scope {
            group: Some(group_by.clone()),
            ..base.clone()
        };

        let mut items = Vec::new();
        if !grouped && self.rng.chance(3, 20) {
            items.push(SelItem::Star);
        } else {
            let n = 1 + self.rng.below(4);
            for i in 0..n {
                let e = if grouped {
                    if !group_by.is_empty() && self.rng.chance(1, 3) {
                        self.rng.pick(&group_by).clone()
                    } else {
                        let c = self.any_cat();
                        self.expr(&gscope, c, 2)
                    }
                } else {
                    let c = self.any_cat();
                    let dd = self.rng.below(depth as usize + 1);
                    self.expr(&base, c, u32::try_from(dd).expect("small"))
                };
                let alias = if self.rng.chance(1, 6) {
                    Some(format!("o{i}"))
                } else {
                    None
                };
                items.push(SelItem::Expr(e, alias));
            }
        }
        let where_ = if self.rng.chance(3, 4) {
            Some(self.expr(&base, Cat::Bool, depth))
        } else {
            None
        };
        let having = if grouped && self.rng.chance(3, 10) {
            Some(self.expr(&gscope, Cat::Bool, 2))
        } else {
            None
        };
        let mut q = Select {
            distinct: self.rng.chance(1, 5),
            items,
            from,
            where_,
            grouped,
            group_by,
            having,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        };
        if self.rng.chance(1, 2) {
            let w = q.width(self.schema);
            let mut pos: Vec<usize> = (1..=w).collect();
            self.rng.shuffle(&mut pos);
            q.order_by = pos
                .into_iter()
                .map(|p| OrderKey {
                    pos: p,
                    desc: self.rng.chance(1, 2),
                    nulls_first: match self.rng.below(3) {
                        0 => Some(true),
                        1 => Some(false),
                        _ => None,
                    },
                })
                .collect();
            if self.rng.chance(2, 5) {
                q.limit = Some(u32::try_from(self.rng.below(10)).expect("small"));
            }
            if self.rng.chance(1, 5) {
                q.offset = Some(u32::try_from(self.rng.below(6)).expect("small"));
            }
        }
        q
    }

    /// A predicate over the query's FROM items for TLP partitioning.
    pub(crate) fn tlp_predicate(&mut self, q: &Select) -> Expr {
        let items: Vec<usize> = q.from.iter().map(|f| f.table).collect();
        let qual = if q.from.len() > 1 {
            Qual::Always
        } else {
            Qual::Sometimes
        };
        let sc = Scope::plain(items, qual);
        self.expr(&sc, Cat::Bool, self.cfg.max_depth)
    }
}
