//! CREATE TABLE / DROP TABLE analysis and type names.

use std::sync::Arc;

use super::Analyzer;
use super::bound::{BoundCreateTable, BoundDropTable};
use super::expr::{ExprCtx, sole_column_ref};
use super::scope::{ExprKind, Scope, ScopeColumn, ScopeRel};
use super::select::display_name;
use crate::catalog::{BoundExprSource, CheckDef, ColumnDef, builtin};
use crate::error::{Error, Result, sqlstate};
use crate::sql::ast::{
    ColumnConstraintKind, CreateTable, DropTable, Expr, Ident, Literal, SourceExpr,
    TableConstraintKind, TableElement, TypeName,
};
use crate::types::{MAX_IDENTIFIER_LENGTH, SqlType, oid};

/// Maximum `varchar(n)` length (PostgreSQL's `MaxAttrSize` limit).
const MAX_VARCHAR_LEN: i64 = 10_485_760;

/// PostgreSQL type names that yuzhu does not implement yet (0A000 rather
/// than 42704 "does not exist").
const KNOWN_UNSUPPORTED_TYPES: &[&str] = &[
    "bpchar",
    "char",
    "numeric",
    "decimal",
    "timestamp",
    "timestamptz",
    "date",
    "time",
    "timetz",
    "interval",
    "bytea",
    "json",
    "jsonb",
    "uuid",
    "money",
    "inet",
    "cidr",
    "macaddr",
    "bit",
    "varbit",
    "xml",
    "point",
    "regclass",
    "regtype",
    "oid",
    "serial",
    "bigserial",
    "smallserial",
    "serial2",
    "serial4",
    "serial8",
];

/// PostgreSQL's `makeObjectName`: `name1_name2_label`, shortening the
/// longer of the two names until it fits in 63 bytes.
pub(super) fn make_object_name(name1: &str, name2: Option<&str>, label: &str) -> String {
    let overhead = label.len() + 1 + usize::from(name2.is_some());
    let avail = MAX_IDENTIFIER_LENGTH.saturating_sub(overhead);
    let mut n1 = name1.len();
    let mut n2 = name2.map_or(0, str::len);
    while n1 + n2 > avail {
        if n1 > n2 {
            n1 -= 1;
        } else {
            n2 -= 1;
        }
    }
    let clip = |s: &str, mut n: usize| {
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s[..n].to_owned()
    };
    let mut out = clip(name1, n1);
    if let Some(n) = name2 {
        out.push('_');
        out.push_str(&clip(n, n2));
    }
    out.push('_');
    out.push_str(label);
    out
}

/// One CHECK constraint collected from the CREATE TABLE elements.
struct PendingCheck<'a> {
    name: Option<&'a Ident>,
    expr: &'a SourceExpr,
}

impl Analyzer<'_> {
    /// Resolves a type name to a supported `SqlType` (with typmod).
    pub(super) fn resolve_type_name(&self, tn: &TypeName) -> Result<SqlType> {
        if !tn.array_bounds.is_empty() {
            return Err(
                Error::not_supported("array types are not supported yet").with_span(tn.span)
            );
        }
        let Some((last, qual)) = tn.names.split_last() else {
            return Err(Error::internal("empty type name"));
        };
        let shown = tn
            .names
            .iter()
            .map(|i| i.value.as_str())
            .collect::<Vec<_>>()
            .join(".");
        let undefined = || {
            Error::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("type \"{shown}\" does not exist"),
            )
            .with_span(tn.span)
        };
        match qual {
            [] => {}
            [s] if s.value == "pg_catalog" => {}
            _ => return Err(undefined()),
        }
        let name = last.value.as_str();
        let t = match self.catalog.type_by_name(name) {
            Some(t) if builtin::is_supported_type(t.oid) && t.oid != oid::UNKNOWN => t,
            Some(_) => {
                return Err(
                    Error::not_supported(format!("type {name} is not supported yet"))
                        .with_span(tn.span),
                );
            }
            None if KNOWN_UNSUPPORTED_TYPES.contains(&name) => {
                return Err(
                    Error::not_supported(format!("type {name} is not supported yet"))
                        .with_span(tn.span),
                );
            }
            None => return Err(undefined()),
        };
        if tn.modifiers.is_empty() {
            return Ok(SqlType::of(t.oid));
        }
        if t.oid != oid::VARCHAR {
            return Err(Error::syntax_at(
                tn.span,
                format!("type modifier is not allowed for type \"{name}\""),
            ));
        }
        let n = match tn.modifiers.as_slice() {
            [
                Expr::Literal {
                    value: Literal::Integer(s),
                    ..
                },
            ] => super::expr::parse_int_literal(s),
            [_] => {
                return Err(Error::syntax_at(
                    tn.span,
                    "type modifiers must be simple constants or identifiers",
                ));
            }
            _ => {
                return Err(
                    Error::new(sqlstate::INVALID_PARAMETER_VALUE, "invalid type modifier")
                        .with_span(tn.span),
                );
            }
        };
        let n = n.unwrap_or(i64::MAX);
        if n < 1 {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "length for type varchar must be at least 1",
            )
            .with_span(tn.span));
        }
        if n > MAX_VARCHAR_LEN {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("length for type varchar cannot exceed {MAX_VARCHAR_LEN}"),
            )
            .with_span(tn.span));
        }
        Ok(SqlType::varchar(i32::try_from(n).unwrap_or(i32::MAX)))
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn analyze_create_table(&self, ct: &CreateTable) -> Result<BoundCreateTable> {
        let name = ct.name.name().value.clone();
        if ct.name.parts.len() > 3 {
            return Err(Error::syntax_at(
                ct.name.span,
                format!(
                    "improper relation name (too many dotted names): {}",
                    display_name(&ct.name)
                ),
            ));
        }
        if ct.name.parts.len() == 3 && ct.name.parts[0].value != self.catalog.current_database() {
            return Err(Error::not_supported(format!(
                "cross-database references are not implemented: {}",
                display_name(&ct.name)
            ))
            .with_span(ct.name.span));
        }
        let schema = match ct.name.schema() {
            None => "public".to_owned(),
            Some(s) if s.value == "public" => "public".to_owned(),
            Some(s) if s.value == "pg_catalog" => {
                return Err(Error::new(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to create \"pg_catalog.{name}\""),
                )
                .with_detail("System catalog modifications are currently disallowed.")
                .with_span(s.span));
            }
            Some(s) => {
                return Err(Error::new(
                    sqlstate::INVALID_SCHEMA_NAME,
                    format!("schema \"{}\" does not exist", s.value),
                )
                .with_span(s.span));
            }
        };
        if self.catalog.table(Some(&schema), &name)?.is_some() {
            if ct.if_not_exists {
                // The session reports `NOTICE: relation "x" already exists,
                // skipping`; the definition is not analyzed (as in PG).
                return Ok(BoundCreateTable {
                    schema,
                    name,
                    if_not_exists: true,
                    columns: vec![],
                    checks: vec![],
                });
            }
            return Err(Error::new(
                sqlstate::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists"),
            ));
        }

        // Pass 1: columns and constraints.
        let mut columns: Vec<ColumnDef> = Vec::new();
        let mut default_exprs: Vec<Option<&SourceExpr>> = Vec::new();
        let mut checks: Vec<PendingCheck<'_>> = Vec::new();
        for elem in &ct.elements {
            match elem {
                TableElement::Column(cd) => {
                    let cname = &cd.name.value;
                    if columns.iter().any(|c| &c.name == cname) {
                        return Err(Error::new(
                            sqlstate::DUPLICATE_COLUMN,
                            format!("column \"{cname}\" specified more than once"),
                        ));
                    }
                    if crate::catalog::schema::SYSTEM_COLUMNS
                        .iter()
                        .any(|(n, _, _)| n == cname)
                    {
                        return Err(Error::new(
                            sqlstate::DUPLICATE_COLUMN,
                            format!("column name \"{cname}\" conflicts with a system column name"),
                        ));
                    }
                    let ty = self.resolve_type_name(&cd.type_name)?;
                    let (mut saw_null, mut saw_not_null) = (false, false);
                    let mut default: Option<&SourceExpr> = None;
                    for c in &cd.constraints {
                        let conflict = || {
                            Error::syntax_at(
                                c.span,
                                format!(
                                    "conflicting NULL/NOT NULL declarations for column \"{cname}\" of table \"{name}\""
                                ),
                            )
                        };
                        match &c.kind {
                            ColumnConstraintKind::NotNull => {
                                if saw_null {
                                    return Err(conflict());
                                }
                                saw_not_null = true;
                            }
                            ColumnConstraintKind::Null => {
                                if saw_not_null {
                                    return Err(conflict());
                                }
                                saw_null = true;
                            }
                            ColumnConstraintKind::Default(e) => {
                                if default.is_some() {
                                    return Err(Error::syntax_at(
                                        c.span,
                                        format!(
                                            "multiple default values specified for column \"{cname}\" of table \"{name}\""
                                        ),
                                    ));
                                }
                                default = Some(e);
                            }
                            ColumnConstraintKind::Check(e) => checks.push(PendingCheck {
                                name: c.name.as_ref(),
                                expr: e,
                            }),
                            ColumnConstraintKind::PrimaryKey => {
                                return Err(Error::not_supported(
                                    "PRIMARY KEY constraints are not supported yet",
                                )
                                .with_span(c.span));
                            }
                            ColumnConstraintKind::Unique => {
                                return Err(Error::not_supported(
                                    "UNIQUE constraints are not supported yet",
                                )
                                .with_span(c.span));
                            }
                            ColumnConstraintKind::References { .. } => {
                                return Err(Error::not_supported(
                                    "FOREIGN KEY constraints are not supported yet",
                                )
                                .with_span(c.span));
                            }
                        }
                    }
                    columns.push(ColumnDef {
                        name: cname.clone(),
                        attnum: i16::try_from(columns.len() + 1).unwrap_or(i16::MAX),
                        ty,
                        not_null: saw_not_null,
                        default: None,
                    });
                    default_exprs.push(default);
                }
                TableElement::Constraint(tc) => match &tc.kind {
                    TableConstraintKind::Check(e) => checks.push(PendingCheck {
                        name: tc.name.as_ref(),
                        expr: e,
                    }),
                    TableConstraintKind::PrimaryKey(_) => {
                        return Err(Error::not_supported(
                            "PRIMARY KEY constraints are not supported yet",
                        )
                        .with_span(tc.span));
                    }
                    TableConstraintKind::Unique(_) => {
                        return Err(Error::not_supported(
                            "UNIQUE constraints are not supported yet",
                        )
                        .with_span(tc.span));
                    }
                    TableConstraintKind::ForeignKey { .. } => {
                        return Err(Error::not_supported(
                            "FOREIGN KEY constraints are not supported yet",
                        )
                        .with_span(tc.span));
                    }
                },
            }
        }

        // Pass 2: DEFAULT expressions (type-checked now, stored as text).
        for (col, def) in columns.iter_mut().zip(default_exprs) {
            if let Some(src) = def {
                self.default_expr(&src.expr, col)?;
                col.default = Some(BoundExprSource {
                    expr_sql: src.text.clone(),
                });
            }
        }

        // Pass 3: CHECK constraints over the new columns, and their names.
        let scope = Scope::with_rel(ScopeRel {
            hidden_name: None,
            refname: name.clone(),
            schema: Some(schema.clone()),
            table_oid: 0,
            system_natts: None,
            columns: columns
                .iter()
                .map(|c| ScopeColumn {
                    name: c.name.clone(),
                    ty: c.ty,
                    attnum: c.attnum,
                })
                .collect(),
        });
        let cx = ExprCtx::new(&scope, ExprKind::Check);
        let mut used: Vec<String> = Vec::new();
        for c in &checks {
            if let Some(n) = c.name {
                if used.contains(&n.value) {
                    return Err(Error::new(
                        sqlstate::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{}\" for relation \"{name}\" already exists",
                            n.value
                        ),
                    )
                    .with_span(n.span));
                }
                used.push(n.value.clone());
            }
        }
        let mut check_defs = Vec::with_capacity(checks.len());
        for c in &checks {
            let b = self.transform_expr(&c.expr.expr, &cx)?;
            let b = self.coerce_to_boolean(b, "CHECK")?;
            let cname = if let Some(n) = c.name {
                n.value.clone()
            } else {
                let col = sole_column_ref(&b).map(|i| columns[i].name.as_str());
                let mut pass = 0u32;
                loop {
                    let label = if pass == 0 {
                        "check".to_owned()
                    } else {
                        format!("check{pass}")
                    };
                    let candidate = make_object_name(&name, col, &label);
                    if !used.contains(&candidate) {
                        break candidate;
                    }
                    pass += 1;
                }
            };
            if c.name.is_none() {
                used.push(cname.clone());
            }
            check_defs.push(CheckDef {
                name: cname,
                expr_sql: c.expr.text.clone(),
            });
        }

        Ok(BoundCreateTable {
            schema,
            name,
            if_not_exists: ct.if_not_exists,
            columns,
            checks: check_defs,
        })
    }

    pub(super) fn analyze_drop_table(&self, dt: &DropTable) -> Result<BoundDropTable> {
        let mut tables: Vec<Arc<crate::catalog::TableDef>> = Vec::new();
        let mut missing = Vec::new();
        for n in &dt.names {
            match self.resolve_table(n) {
                Ok(t) => {
                    if !tables.iter().any(|x| x.oid == t.oid) {
                        tables.push(t);
                    }
                }
                Err(e) if e.sqlstate == sqlstate::UNDEFINED_TABLE => {
                    if dt.if_exists {
                        missing.push(n.name().value.clone());
                    } else {
                        return Err(Error::new(
                            sqlstate::UNDEFINED_TABLE,
                            format!("table \"{}\" does not exist", n.name().value),
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(BoundDropTable { tables, missing })
    }
}
