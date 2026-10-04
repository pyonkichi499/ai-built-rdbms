//! Types, literals, tables and INSERT statements, plus their SQL rendering.

use std::fmt::Write as _;

use crate::ast::Expr;
use crate::rng::Rng;

/// The M1 column types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ty {
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Bool,
    Text,
    Varchar(u16),
}

/// Coarse type category used by the expression generator. Any two members of
/// the same category can be compared, and `Int`/`Float` mix freely in
/// arithmetic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cat {
    Int,
    Float,
    Bool,
    Text,
}

pub(crate) const ALL_CATS: [Cat; 4] = [Cat::Int, Cat::Float, Cat::Bool, Cat::Text];

impl Ty {
    pub(crate) fn cat(self) -> Cat {
        match self {
            Ty::Int2 | Ty::Int4 | Ty::Int8 => Cat::Int,
            Ty::Float4 | Ty::Float8 => Cat::Float,
            Ty::Bool => Cat::Bool,
            Ty::Text | Ty::Varchar(_) => Cat::Text,
        }
    }

    /// SQL spelling. `spelling` selects among synonyms (`int4`/`integer`/`int`, ...).
    pub(crate) fn sql(self, spelling: u8) -> String {
        let s = match (self, spelling % 3) {
            (Ty::Int2, 1) => "smallint",
            (Ty::Int2, _) => "int2",
            (Ty::Int4, 1) => "integer",
            (Ty::Int4, 2) => "int",
            (Ty::Int4, _) => "int4",
            (Ty::Int8, 1) => "bigint",
            (Ty::Int8, _) => "int8",
            (Ty::Float4, 1) => "real",
            (Ty::Float4, _) => "float4",
            (Ty::Float8, 1) => "double precision",
            (Ty::Float8, _) => "float8",
            (Ty::Bool, 1) => "boolean",
            (Ty::Bool, _) => "bool",
            (Ty::Text, _) => "text",
            (Ty::Varchar(n), 1) => return format!("character varying({n})"),
            (Ty::Varchar(n), _) => return format!("varchar({n})"),
        };
        s.to_string()
    }

    pub(crate) fn default_for(cat: Cat) -> Ty {
        match cat {
            Cat::Int => Ty::Int4,
            Cat::Float => Ty::Float8,
            Cat::Bool => Ty::Bool,
            Cat::Text => Ty::Text,
        }
    }

    pub(crate) fn random_of(rng: &mut Rng, cat: Cat) -> Ty {
        match cat {
            Cat::Int => *rng.pick(&[Ty::Int2, Ty::Int4, Ty::Int4, Ty::Int8]),
            Cat::Float => *rng.pick(&[Ty::Float4, Ty::Float8, Ty::Float8]),
            Cat::Bool => Ty::Bool,
            Cat::Text => {
                if rng.chance(1, 3) {
                    Ty::Varchar(u16::try_from(rng.range(1, 8)).expect("small"))
                } else {
                    Ty::Text
                }
            }
        }
    }

    pub(crate) fn random(rng: &mut Rng) -> Ty {
        match rng.weighted(&[1, 3, 2, 1, 2, 2, 3, 2]) {
            0 => Ty::Int2,
            1 => Ty::Int4,
            2 => Ty::Int8,
            3 => Ty::Float4,
            4 => Ty::Float8,
            5 => Ty::Bool,
            6 => Ty::Text,
            _ => Ty::Varchar(u16::try_from(rng.range(1, 8)).expect("small")),
        }
    }
}

/// A literal value. Floats are kept as their textual input form so that
/// special values (`NaN`, `-Infinity`, `1e-320`) round-trip exactly.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Lit {
    Null,
    Int(i64),
    Float(String),
    Bool(bool),
    Text(String),
}

/// How a literal is spelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LitForm {
    /// `5`, `'abc'`, `TRUE`, `NULL` (untyped where PostgreSQL treats it so).
    Bare,
    /// `(lit::type)`.
    Cast,
    /// `CAST(lit AS type)`.
    CastFn,
    /// Unquoted decimal (`1.5`, PostgreSQL type numeric). Floats only; only
    /// generated when the `decimal-literals` feature is on.
    Unquoted,
}

pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub(crate) fn render_lit(lit: &Lit, ty: Ty, form: LitForm, out: &mut String) {
    let base = match lit {
        Lit::Null => "NULL".to_string(),
        Lit::Int(v) if *v == i64::MIN => "'-9223372036854775808'::int8".to_string(),
        Lit::Int(v) if *v < 0 => format!("(-{})", v.unsigned_abs()),
        Lit::Int(v) => v.to_string(),
        Lit::Float(s) if form == LitForm::Unquoted => s.clone(),
        Lit::Float(s) | Lit::Text(s) => quote(s),
        Lit::Bool(true) => "TRUE".to_string(),
        Lit::Bool(false) => "FALSE".to_string(),
    };
    match form {
        LitForm::Bare | LitForm::Unquoted => {
            if matches!(lit, Lit::Int(i64::MIN)) {
                let _ = write!(out, "({base})");
            } else {
                out.push_str(&base);
            }
        }
        LitForm::Cast => {
            let _ = write!(out, "({base}::{})", ty.sql(0));
        }
        LitForm::CastFn => {
            let _ = write!(out, "CAST({base} AS {})", ty.sql(0));
        }
    }
}

pub(crate) fn int_range(ty: Ty) -> (i64, i64) {
    match ty {
        Ty::Int2 => (i64::from(i16::MIN), i64::from(i16::MAX)),
        Ty::Int4 => (i64::from(i32::MIN), i64::from(i32::MAX)),
        _ => (i64::MIN, i64::MAX),
    }
}

pub(crate) fn gen_int(rng: &mut Rng, ty: Ty) -> i64 {
    let (lo, hi) = int_range(ty);
    match rng.weighted(&[25, 50, 25]) {
        0 => *rng.pick(&[lo, hi, 0, 1, -1, lo + 1, hi - 1, 2, 10, -10, 100]),
        1 => rng.range(-20, 20),
        _ => rng.range(lo, hi),
    }
}

pub(crate) fn gen_float(rng: &mut Rng, ty: Ty) -> String {
    const EDGES: &[&str] = &[
        "0",
        "-0",
        "1",
        "-1",
        "0.5",
        "1.5",
        "2.5",
        "-2.5",
        "0.1",
        "1e10",
        "-1e10",
        "1e-10",
        "123456789.125",
        "NaN",
        "Infinity",
        "-Infinity",
        "3.4028235e38",
        "1e-45",
        "1.7976931348623157e308",
        "-1e308",
        "5e-324",
        "2147483647.5",
        "9.2233720368547758e18",
        "32767.5",
    ];
    if rng.chance(3, 10) {
        let s = *rng.pick(EDGES);
        // Keep float4 columns mostly in range; out-of-range inputs still
        // appear sometimes and must fail identically (22003).
        if ty == Ty::Float4 && s.contains("e308") && rng.chance(3, 4) {
            return "1e30".to_string();
        }
        return s.to_string();
    }
    let int_part = rng.range(-1000, 1000);
    match rng.below(3) {
        0 => int_part.to_string(),
        1 => format!("{int_part}.{}", rng.range(0, 99)),
        _ => format!("{int_part}.{:03}e{}", rng.range(0, 999), rng.range(-5, 5)),
    }
}

pub(crate) fn gen_text(rng: &mut Rng, max_len: Option<u16>) -> String {
    const POOL: &[&str] = &[
        "",
        "a",
        "A",
        "b",
        "ab",
        "abc",
        "abd",
        "ABC",
        "a%",
        "a_c",
        "%",
        "_",
        " a",
        "a ",
        "a  ",
        "O'Reilly",
        "\\",
        "a\\b",
        "zz",
        "0",
        "12",
        "-7",
        "true",
        "NaN",
        "あ",
        "あいう",
        "é",
        "\t",
    ];
    let s = if rng.chance(1, 2) {
        (*rng.pick(POOL)).to_string()
    } else {
        let len = rng.below(7);
        (0..len)
            .map(|_| *rng.pick(&['a', 'b', 'c', 'x', 'A', ' ', '%', '_', '1']))
            .collect()
    };
    match max_len {
        Some(n) if s.chars().count() > n as usize && rng.chance(4, 5) => {
            s.chars().take(n as usize).collect()
        }
        _ => s,
    }
}

/// A non-NULL literal of the given type.
pub(crate) fn gen_lit(rng: &mut Rng, ty: Ty) -> Lit {
    match ty {
        Ty::Int2 | Ty::Int4 | Ty::Int8 => Lit::Int(gen_int(rng, ty)),
        Ty::Float4 | Ty::Float8 => Lit::Float(gen_float(rng, ty)),
        Ty::Bool => Lit::Bool(rng.chance(1, 2)),
        Ty::Text => Lit::Text(gen_text(rng, None)),
        Ty::Varchar(n) => Lit::Text(gen_text(rng, Some(n))),
    }
}

/// The type PostgreSQL gives a bare integer constant (as rendered by `render_lit`).
pub(crate) fn bare_int_ty(v: i64) -> Ty {
    // `-2147483648` is lexed as `-(2147483648)`, whose operand is already int8.
    if v > i64::from(i32::MIN) && i32::try_from(v).is_ok() {
        Ty::Int4
    } else {
        Ty::Int8
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Column {
    pub(crate) ty: Ty,
    pub(crate) spelling: u8,
    pub(crate) not_null: bool,
    pub(crate) default: Option<(Lit, LitForm)>,
}

#[derive(Clone, Debug)]
pub(crate) struct Check {
    pub(crate) named: bool,
    pub(crate) expr: Expr,
    /// Rendered as a column constraint of this column (otherwise a table constraint).
    pub(crate) column: Option<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct Table {
    pub(crate) cols: Vec<Column>,
    pub(crate) checks: Vec<Check>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Schema {
    pub(crate) tables: Vec<Table>,
}

pub(crate) fn col_name(i: usize) -> String {
    format!("c{i}")
}

impl Table {
    pub(crate) fn create_sql(&self, name: &str, names: &[String]) -> String {
        let cx = crate::ast::Rctx {
            names,
            quals: vec![name.to_string()],
        };
        let mut s = format!("CREATE TABLE {name} (");
        for (i, c) in self.cols.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            let _ = write!(s, "{} {}", col_name(i), c.ty.sql(c.spelling));
            if c.not_null {
                s.push_str(" NOT NULL");
            }
            if let Some((lit, form)) = &c.default {
                s.push_str(" DEFAULT ");
                render_lit(lit, c.ty, *form, &mut s);
            }
            for (k, ck) in self.checks.iter().enumerate() {
                if ck.column == Some(i) {
                    render_check(&mut s, k, ck, &cx);
                }
            }
        }
        for (k, ck) in self.checks.iter().enumerate() {
            if ck.column.is_none() {
                s.push(',');
                render_check(&mut s, k, ck, &cx);
            }
        }
        s.push(')');
        s
    }
}

fn render_check(s: &mut String, k: usize, ck: &Check, cx: &crate::ast::Rctx<'_>) {
    if ck.named {
        let _ = write!(s, " CONSTRAINT ck{k}");
    }
    s.push_str(" CHECK (");
    ck.expr.render(cx, s);
    s.push(')');
}

/// One value in an INSERT's VALUES list.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Val {
    Lit(Lit, LitForm, Ty),
    Default,
}

#[derive(Clone, Debug)]
pub(crate) struct Insert {
    pub(crate) table: usize,
    /// `None`: no column list (all columns in order). `Some(vec![])`: DEFAULT VALUES.
    pub(crate) cols: Option<Vec<usize>>,
    pub(crate) rows: Vec<Vec<Val>>,
}

impl Insert {
    pub(crate) fn sql(&self, names: &[String]) -> String {
        let name = &names[self.table];
        let mut s = format!("INSERT INTO {name}");
        match &self.cols {
            Some(c) if c.is_empty() => {
                s.push_str(" DEFAULT VALUES");
                return s;
            }
            Some(c) => {
                let list: Vec<String> = c.iter().map(|&i| col_name(i)).collect();
                let _ = write!(s, " ({})", list.join(", "));
            }
            None => {}
        }
        s.push_str(" VALUES ");
        for (r, row) in self.rows.iter().enumerate() {
            if r > 0 {
                s.push_str(", ");
            }
            s.push('(');
            for (i, v) in row.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                match v {
                    Val::Default => s.push_str("DEFAULT"),
                    Val::Lit(lit, form, ty) => render_lit(lit, *ty, *form, &mut s),
                }
            }
            s.push(')');
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_rendering() {
        let mut s = String::new();
        render_lit(&Lit::Int(-5), Ty::Int2, LitForm::Cast, &mut s);
        assert_eq!(s, "((-5)::int2)");
        s.clear();
        render_lit(&Lit::Int(i64::MIN), Ty::Int8, LitForm::Bare, &mut s);
        assert_eq!(s, "('-9223372036854775808'::int8)");
        s.clear();
        render_lit(&Lit::Text("O'R".into()), Ty::Text, LitForm::Bare, &mut s);
        assert_eq!(s, "'O''R'");
        s.clear();
        render_lit(
            &Lit::Float("NaN".into()),
            Ty::Float4,
            LitForm::CastFn,
            &mut s,
        );
        assert_eq!(s, "CAST('NaN' AS float4)");
        assert_eq!(bare_int_ty(2_147_483_648), Ty::Int8);
        assert_eq!(bare_int_ty(-2_147_483_648), Ty::Int8);
        assert_eq!(bare_int_ty(-2_147_483_647), Ty::Int4);
    }
}
