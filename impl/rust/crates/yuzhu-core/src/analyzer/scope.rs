//! Name resolution scope: the (at most one, in M1) FROM item whose columns
//! form the input row.

use std::cell::RefCell;

use crate::catalog::SystemColumn;
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::Ident;
use crate::types::{Oid, SqlType};

#[derive(Debug, Clone)]
pub(super) struct ScopeColumn {
    pub(super) name: String,
    pub(super) ty: SqlType,
    /// attnum in the base table (0 if not a base table column).
    pub(super) attnum: i16,
}

/// A FROM item visible to expressions.
#[derive(Debug, Clone)]
pub(super) struct ScopeRel {
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
    /// Number of user columns when the system columns can be referenced
    /// (SELECT / UPDATE / DELETE over a base table).
    pub(super) system_natts: Option<usize>,
}

/// What kind of clause an expression appears in (PostgreSQL's
/// `ParseExprKind`); affects which references are allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExprKind {
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
}

impl ExprKind {
    /// Name used in messages such as `ORDER BY "x" is ambiguous`.
    pub(super) fn name(self) -> &'static str {
        match self {
            ExprKind::SelectTarget => "SELECT",
            ExprKind::Where => "WHERE",
            ExprKind::UpdateSet => "UPDATE",
            ExprKind::OrderBy => "ORDER BY",
            ExprKind::Limit => "LIMIT",
            ExprKind::Offset => "OFFSET",
            ExprKind::Values => "VALUES",
            ExprKind::ColumnDefault => "DEFAULT",
            ExprKind::Check => "CHECK",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct Scope {
    pub(super) rel: Option<ScopeRel>,
    /// System columns referenced so far (appearance order, no duplicates).
    used_system: RefCell<Vec<SystemColumn>>,
}

impl Scope {
    pub(super) fn empty() -> Self {
        Scope::default()
    }

    pub(super) fn with_rel(rel: ScopeRel) -> Self {
        Scope {
            rel: Some(rel),
            used_system: RefCell::default(),
        }
    }

    /// Does a qualifier (`t` or `schema.t`) name the FROM item?
    fn qualifier_matches(rel: &ScopeRel, qual: &[Ident]) -> bool {
        match qual {
            [t] => t.value == rel.refname,
            [s, t] => rel.schema.as_deref() == Some(s.value.as_str()) && t.value == rel.refname,
            _ => false,
        }
    }

    fn missing_from(rel: Option<&ScopeRel>, qual: &[Ident], span: Span) -> Error {
        let t = qual.last().map_or("", |i| i.value.as_str());
        if let [q] = qual
            && let Some(rel) = rel
            && rel.hidden_name.as_deref() == Some(q.value.as_str())
        {
            return Error::new(
                sqlstate::UNDEFINED_TABLE,
                format!("invalid reference to FROM-clause entry for table \"{t}\""),
            )
            .with_hint(format!(
                "Perhaps you meant to reference the table alias \"{}\".",
                rel.refname
            ))
            .with_span(span);
        }
        Error::new(
            sqlstate::UNDEFINED_TABLE,
            format!("missing FROM-clause entry for table \"{t}\""),
        )
        .with_span(span)
    }

    /// Resolves `col`, `t.col` or `schema.t.col` to (input index, column).
    pub(super) fn resolve_column(
        &self,
        parts: &[Ident],
        span: Span,
    ) -> Result<(usize, ScopeColumn)> {
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
        if !qual.is_empty() {
            match &self.rel {
                Some(rel) if Self::qualifier_matches(rel, qual) => {}
                _ => return Err(Self::missing_from(self.rel.as_ref(), qual, span)),
            }
        }
        let found = self.rel.as_ref().and_then(|rel| {
            rel.columns
                .iter()
                .enumerate()
                .find(|(_, c)| c.name == colname.value)
                .map(|(i, c)| (i, c.clone()))
        });
        if let Some(f) = found {
            Ok(f)
        } else if let Some(f) = self.system_column(&colname.value) {
            Ok(f)
        } else {
            let shown = if qual.is_empty() {
                format!("column \"{}\" does not exist", colname.value)
            } else {
                format!(
                    "column {}.{} does not exist",
                    qual.last().map_or("", |q| q.value.as_str()),
                    colname.value
                )
            };
            Err(Error::new(sqlstate::UNDEFINED_COLUMN, shown).with_span(colname.span))
        }
    }

    /// Expands `*` (no qualifier) or `t.*`.
    pub(super) fn expand_star(
        &self,
        qual: Option<&[Ident]>,
        span: Span,
    ) -> Result<Vec<(usize, &ScopeColumn)>> {
        match (qual, &self.rel) {
            (None, None) => Err(Error::syntax_at(span, "SELECT * with no tables specified")),
            (Some(q), rel) => match rel {
                Some(rel) if Self::qualifier_matches(rel, q) => {
                    Ok(rel.columns.iter().enumerate().collect())
                }
                _ => Err(Self::missing_from(self.rel.as_ref(), q, span)),
            },
            (None, Some(rel)) => Ok(rel.columns.iter().enumerate().collect()),
        }
    }
}

impl Scope {
    /// Lets expressions reference the system columns of the (single) base
    /// table. User columns take precedence over system columns of the
    /// same name.
    pub(super) fn enable_system_columns(&mut self) {
        if let Some(rel) = &mut self.rel {
            rel.system_natts = Some(rel.columns.len());
        }
    }

    /// The system columns referenced so far, in appearance order.
    pub(super) fn used_system_columns(&self) -> Vec<SystemColumn> {
        self.used_system.borrow().clone()
    }

    /// Resolves a system column name to its input-row index
    /// (`natts + position in the used list`), recording the use.
    fn system_column(&self, name: &str) -> Option<(usize, ScopeColumn)> {
        let natts = self.rel.as_ref()?.system_natts?;
        let (col, (cname, attnum, type_oid)) = [
            SystemColumn::Ctid,
            SystemColumn::Xmin,
            SystemColumn::Cmin,
            SystemColumn::Xmax,
            SystemColumn::Cmax,
            SystemColumn::TableOid,
        ]
        .into_iter()
        .zip(crate::catalog::schema::SYSTEM_COLUMNS)
        .find(|(_, (n, _, _))| *n == name)?;
        let mut used = self.used_system.borrow_mut();
        let pos = used.iter().position(|c| *c == col).unwrap_or_else(|| {
            used.push(col);
            used.len() - 1
        });
        Some((
            natts + pos,
            ScopeColumn {
                name: cname.to_owned(),
                ty: SqlType::of(type_oid),
                attnum,
            },
        ))
    }
}
