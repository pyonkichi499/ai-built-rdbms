//! Name resolution scope: the (at most one, in M1) FROM item whose columns
//! form the input row.

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
    /// Schema of the table when referenced without an alias (allows
    /// `schema.table.column`).
    pub(super) schema: Option<String>,
    /// Base table OID (0 for VALUES or a table being created).
    pub(super) table_oid: Oid,
    pub(super) columns: Vec<ScopeColumn>,
}

/// What kind of clause an expression appears in (PostgreSQL's
/// `ParseExprKind`); affects which references are allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExprKind {
    SelectTarget,
    Where,
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
}

impl Scope {
    pub(super) fn empty() -> Self {
        Scope { rel: None }
    }

    pub(super) fn with_rel(rel: ScopeRel) -> Self {
        Scope { rel: Some(rel) }
    }

    /// Does a qualifier (`t` or `schema.t`) name the FROM item?
    fn qualifier_matches(rel: &ScopeRel, qual: &[Ident]) -> bool {
        match qual {
            [t] => t.value == rel.refname,
            [s, t] => rel.schema.as_deref() == Some(s.value.as_str()) && t.value == rel.refname,
            _ => false,
        }
    }

    fn missing_from(qual: &[Ident], span: Span) -> Error {
        let t = qual.last().map_or("", |i| i.value.as_str());
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
    ) -> Result<(usize, &ScopeColumn)> {
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
                _ => return Err(Self::missing_from(qual, span)),
            }
        }
        let found = self.rel.as_ref().and_then(|rel| {
            rel.columns
                .iter()
                .enumerate()
                .find(|(_, c)| c.name == colname.value)
        });
        if let Some(f) = found {
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
                _ => Err(Self::missing_from(q, span)),
            },
            (None, Some(rel)) => Ok(rel.columns.iter().enumerate().collect()),
        }
    }
}
