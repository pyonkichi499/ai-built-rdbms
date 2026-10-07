//! CREATE TABLE / DROP TABLE、シーケンス（SERIAL・IDENTITY・CREATE / ALTER / DROP SEQUENCE）の解析と型名
//! （`m4/08-sequence-serial.md` §4.8〜§4.9、§5.4〜§5.8、`m4/07-catalog-ddl.md` §12.2）。

use std::collections::HashSet;
use std::sync::Arc;

use super::Analyzer;
use super::bound::{
    BoundAlterAction, BoundAlterSequence, BoundCreateSequence, BoundCreateTable, BoundDropSequence,
    BoundDropTable, BoundExpr, OwnedByTarget, SeqOwner,
};
use super::ddl_constraint::{analyze_index_constraints, convert_options};
use super::expr::{ExprCtx, sole_column_ref};
use super::scope::{ParseExprKind, ScopeColumn, ScopeRel, ScopeStack};
use super::select::display_name;
use crate::catalog::naming::{self, NameLookup};
use crate::catalog::schema::oids;
use crate::catalog::seq_params::{InitMode, SeqOptions, init_params, parse_seq_int};
use crate::catalog::{
    BoundExprSource, CatalogReader, CheckDef, ColumnDef, IdentityKind, RelKind, TableDef, builtin,
};
use crate::ddl::depend::collect_regclass_refs;
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{
    AlterSequence, AlterSequenceAction, ColumnConstraintKind, CreateSequence, CreateTable,
    DropSequence, DropTable, Expr, GeneratedWhen, Ident, Literal, ObjectName, OverridingKind,
    SeqOption, SeqOptionKind, SeqPersistence, SourceExpr, TableConstraintKind, TableElement,
    TypeName,
};
use crate::types::{Oid, SqlType, oid, typmod};

/// 以前ここにあった定義は `catalog::naming`（07 §4.2）へ移った。テスト（`tests.rs`）が名前を使う。
#[cfg(test)]
pub(super) use crate::catalog::naming::make_object_name;

/// PostgreSQL type names that yuzhu does not implement yet (0A000 rather
/// than 42704 "does not exist").
const KNOWN_UNSUPPORTED_TYPES: &[&str] = &[
    "time", "timetz", "interval", "bytea", "json", "jsonb", "uuid", "money", "inet", "cidr",
    "macaddr", "bit", "varbit", "xml", "point", "oid",
];

/// 作れる名前空間は `public` だけ（`pg_catalog` は 42501、その他は 3F000）。
const CREATE_NAMESPACE: Oid = oids::NAMESPACE_PUBLIC;

/// 1 つの列の制約の並びを PostgreSQL の `transformColumnDefinition` と同じ順に検査する状態。
#[allow(clippy::struct_excessive_bools)]
struct ColumnState<'a> {
    column: &'a str,
    table: &'a str,
    saw_nullable: bool,
    is_not_null: bool,
    saw_default: bool,
    saw_identity: bool,
}

impl ColumnState<'_> {
    fn conflict(&self, span: Span) -> Error {
        Error::syntax_at(
            span,
            format!(
                "conflicting NULL/NOT NULL declarations for column \"{}\" of table \"{}\"",
                self.column, self.table
            ),
        )
    }

    fn null(&mut self, span: Span) -> Result<()> {
        if self.saw_nullable && self.is_not_null {
            return Err(self.conflict(span));
        }
        self.is_not_null = false;
        self.saw_nullable = true;
        Ok(())
    }

    fn not_null(&mut self, span: Span) -> Result<()> {
        if self.saw_nullable && !self.is_not_null {
            return Err(self.conflict(span));
        }
        self.is_not_null = true;
        self.saw_nullable = true;
        Ok(())
    }

    fn default(&mut self, span: Span) -> Result<()> {
        if self.saw_default {
            return Err(Error::syntax_at(
                span,
                format!(
                    "multiple default values specified for column \"{}\" of table \"{}\"",
                    self.column, self.table
                ),
            ));
        }
        self.saw_default = true;
        self.both(span)
    }

    fn identity(&mut self, span: Span) -> Result<()> {
        if self.saw_identity {
            return Err(Error::syntax_at(
                span,
                format!(
                    "multiple identity specifications for column \"{}\" of table \"{}\"",
                    self.column, self.table
                ),
            ));
        }
        self.saw_identity = true;
        // IDENTITY は暗黙の NOT NULL。
        self.not_null(span)?;
        self.both(span)
    }

    /// 各制約の後の検査。
    fn both(&self, span: Span) -> Result<()> {
        if self.saw_default && self.saw_identity {
            return Err(Error::syntax_at(
                span,
                format!(
                    "both default and identity specified for column \"{}\" of table \"{}\"",
                    self.column, self.table
                ),
            ));
        }
        Ok(())
    }
}

/// `catalog::naming` の衝突の判定を、解析側の `CatalogReader` で行う。作れる名前空間は 1 つ（`public`）
/// なので、`nsp` は見ずに `schema` で引く。
struct ReaderLookup<'a> {
    catalog: &'a dyn CatalogReader,
    schema: &'a str,
}

impl NameLookup for ReaderLookup<'_> {
    fn relation_exists(&self, _nsp: Oid, name: &str) -> Result<bool> {
        Ok(self
            .catalog
            .relation_kind(Some(self.schema), name)?
            .is_some())
    }

    /// `CatalogReader` には制約の名前を引く口がない（11 の issues）。同じ文の中の衝突は `taken` が防ぐ。
    fn constraint_exists(&self, _nsp: Oid, _name: &str) -> Result<bool> {
        Ok(false)
    }
}

/// `serial` 系の型名なら、列の整数型。
fn serial_type(tn: &TypeName) -> Result<Option<Oid>> {
    let [name] = tn.names.as_slice() else {
        return Ok(None);
    };
    let ty = match name.value.as_str() {
        "smallserial" | "serial2" => oid::INT2,
        "serial" | "serial4" => oid::INT4,
        "bigserial" | "serial8" => oid::INT8,
        _ => return Ok(None),
    };
    if !tn.array_bounds.is_empty() {
        return Err(Error::not_supported("array of serial is not implemented").with_span(tn.span));
    }
    Ok(Some(ty))
}

fn identity_kind(when: GeneratedWhen) -> IdentityKind {
    match when {
        GeneratedWhen::Always => IdentityKind::Always,
        GeneratedWhen::ByDefault => IdentityKind::ByDefault,
    }
}

/// オプションの出どころ。エラーの文言と許すオプションが違う。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SeqOptMode {
    Create,
    Alter,
    /// IDENTITY 列のオプション（`AS` は列の型。`SEQUENCE NAME` を許す）。
    Identity,
}

/// `parse_seq_options` の結果。
struct ParsedSeqOptions<'a> {
    opts: SeqOptions,
    owned_by: Option<&'a ObjectName>,
    sequence_name: Option<&'a ObjectName>,
}

fn seq_option_slot(kind: &SeqOptionKind) -> usize {
    match kind {
        SeqOptionKind::As(_) => 0,
        SeqOptionKind::Increment(_) => 1,
        SeqOptionKind::MinValue(_) => 2,
        SeqOptionKind::MaxValue(_) => 3,
        SeqOptionKind::Start(_) => 4,
        SeqOptionKind::Restart(_) => 5,
        SeqOptionKind::Cache(_) => 6,
        SeqOptionKind::Cycle(_) => 7,
        SeqOptionKind::OwnedBy(_) => 8,
        SeqOptionKind::SequenceName(_) => 9,
    }
}

impl Analyzer<'_> {
    /// Resolves a type name to a supported `SqlType` (with typmod).
    pub(super) fn resolve_type_name(&self, tn: &TypeName) -> Result<SqlType> {
        if !tn.array_bounds.is_empty() {
            // Only one-dimensional int4[] exists (text input / output only).
            if tn.array_bounds.len() == 1 {
                let mut base = tn.clone();
                base.array_bounds.clear();
                if self
                    .resolve_type_name(&base)
                    .is_ok_and(|t| t.oid == oid::INT4)
                {
                    return Ok(SqlType::of(oid::INT4_ARRAY));
                }
            }
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
        let unsupported =
            || Error::not_supported(format!("type {name} is not supported yet")).with_span(tn.span);
        let t = match self.catalog.type_by_name(name) {
            Some(t) if builtin::is_supported_type(t.oid) && t.oid != oid::UNKNOWN => t,
            Some(_) => return Err(unsupported()),
            None if KNOWN_UNSUPPORTED_TYPES.contains(&name) => return Err(unsupported()),
            None => return Err(undefined()),
        };
        if tn.modifiers.is_empty() {
            return Ok(SqlType::of(t.oid));
        }
        if !typmod::takes_typmod(t.oid) {
            return Err(Error::syntax_at(
                tn.span,
                format!("type modifier is not allowed for type \"{name}\""),
            ));
        }
        let mut mods = Vec::with_capacity(tn.modifiers.len());
        for m in &tn.modifiers {
            let Expr::Literal {
                value: Literal::Integer(s),
                ..
            } = m
            else {
                return Err(Error::syntax_at(
                    tn.span,
                    "type modifiers must be simple constants or identifiers",
                ));
            };
            mods.push(super::expr::parse_int_literal(s).unwrap_or(i64::MAX));
        }
        let typmod = typmod::typmod_in(t.oid, &mods).map_err(|e| e.with_span(tn.span))?;
        Ok(SqlType::new(t.oid, typmod))
    }

    /// `[db.][schema.]name` を (スキーマ, 名前) に分ける。`a.b.c.d` は 42601、別の DB は 0A000。
    fn relation_parts<'n>(&self, name: &'n ObjectName) -> Result<(Option<&'n Ident>, &'n Ident)> {
        if name.parts.len() > 3 {
            return Err(Error::syntax_at(
                name.span,
                format!(
                    "improper relation name (too many dotted names): {}",
                    display_name(name)
                ),
            ));
        }
        if name.parts.len() == 3 && name.parts[0].value != self.catalog.current_database() {
            return Err(Error::not_supported(format!(
                "cross-database references are not implemented: {}",
                display_name(name)
            ))
            .with_span(name.span));
        }
        Ok((name.schema(), name.name()))
    }

    /// CREATE の対象のスキーマ（`public` だけ）と名前。
    fn create_target(&self, name: &ObjectName) -> Result<(String, String)> {
        let (schema, rel) = self.relation_parts(name)?;
        let rel = rel.value.clone();
        let schema = match schema {
            None => match self
                .catalog
                .search_path()
                .iter()
                .find(|s| matches!(s.as_str(), "public" | "pg_catalog"))
                .map(String::as_str)
            {
                Some("public") => "public".to_owned(),
                Some(_) => {
                    return Err(Error::new(
                        sqlstate::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied to create \"pg_catalog.{rel}\""),
                    )
                    .with_detail("System catalog modifications are currently disallowed."));
                }
                None => {
                    return Err(Error::new(
                        sqlstate::INVALID_SCHEMA_NAME,
                        "no schema has been selected to create in",
                    )
                    .with_span(name.span));
                }
            },
            Some(s) if s.value == "public" => "public".to_owned(),
            Some(s) if s.value == "pg_catalog" => {
                return Err(Error::new(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to create \"pg_catalog.{rel}\""),
                )
                .with_detail("System catalog modifications are currently disallowed."));
            }
            Some(s) => {
                return Err(Error::new(
                    sqlstate::INVALID_SCHEMA_NAME,
                    format!("schema \"{}\" does not exist", s.value),
                )
                .with_span(s.span));
            }
        };
        Ok((schema, rel))
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn analyze_create_table(&self, ct: &CreateTable) -> Result<BoundCreateTable> {
        let (schema, name) = self.create_target(&ct.name)?;
        let exists = self.catalog.table(Some(&schema), &name)?.is_some();
        if exists && ct.if_not_exists {
            // The session reports `NOTICE: relation "x" already exists,
            // skipping`; the definition is not analyzed (as in PG).
            return Ok(BoundCreateTable {
                schema,
                name,
                if_not_exists: true,
                columns: vec![],
                checks: vec![],
                constraints: vec![],
                sequences: vec![],
                options: vec![],
                default_refs: vec![],
            });
        }

        // Pass 1: columns and constraints.
        let lookup = ReaderLookup {
            catalog: self.catalog,
            schema: &schema,
        };
        let mut columns: Vec<ColumnDef> = Vec::new();
        let mut default_exprs: Vec<Option<&SourceExpr>> = Vec::new();
        let mut checks: Vec<PendingCheck<'_>> = Vec::new();
        let mut sequences: Vec<BoundCreateSequence> = Vec::new();
        // 同じ文の中で先に決めた名前（表自身・シーケンス）。
        let mut taken_relations: HashSet<String> = HashSet::from([name.clone()]);
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
                    let attnum = i16::try_from(columns.len() + 1).unwrap_or(i16::MAX);
                    let serial = serial_type(&cd.type_name)?;
                    let ty = if let Some(t) = serial {
                        SqlType::of(t)
                    } else {
                        let ty = self.resolve_type_name(&cd.type_name)?;
                        // int4[] has text I/O only: it cannot be stored in a heap tuple.
                        if ty.oid == oid::INT4_ARRAY {
                            return Err(Error::not_supported("array types are not supported yet")
                                .with_span(cd.type_name.span));
                        }
                        ty
                    };
                    if serial.is_some() {
                        let out = init_params(
                            &SeqOptions {
                                as_type: Some(ty.oid),
                                ..SeqOptions::default()
                            },
                            false,
                            InitMode::Create,
                        )?;
                        let seq_name = naming::choose_relation_name(
                            &name,
                            Some(cname),
                            "seq",
                            CREATE_NAMESPACE,
                            false,
                            &lookup,
                            &taken_relations,
                        )?;
                        taken_relations.insert(seq_name.clone());
                        sequences.push(BoundCreateSequence {
                            schema: schema.clone(),
                            namespace: CREATE_NAMESPACE,
                            name: seq_name,
                            if_not_exists: false,
                            params: out.params,
                            initial: out.state,
                            owner: SeqOwner::NewTableColumn {
                                attnum,
                                serial_default: true,
                            },
                            for_identity: false,
                        });
                    }
                    let mut st = ColumnState {
                        column: cname,
                        table: &name,
                        saw_nullable: false,
                        is_not_null: false,
                        saw_default: false,
                        saw_identity: false,
                    };
                    let mut default: Option<&SourceExpr> = None;
                    let mut identity: Option<IdentityKind> = None;
                    for c in &cd.constraints {
                        match &c.kind {
                            ColumnConstraintKind::NotNull => st.not_null(c.span)?,
                            ColumnConstraintKind::Null => st.null(c.span)?,
                            ColumnConstraintKind::Default(e) => {
                                st.default(c.span)?;
                                default = Some(e);
                            }
                            ColumnConstraintKind::Check(e) => checks.push(PendingCheck {
                                name: c.name.as_ref(),
                                expr: e,
                            }),
                            // PRIMARY KEY / UNIQUE は `analyze_index_constraints` が要素から集める。
                            ColumnConstraintKind::PrimaryKey(_)
                            | ColumnConstraintKind::Unique(_) => {}
                            ColumnConstraintKind::Identity { when, options } => {
                                st.identity(c.span)?;
                                let seq = self.identity_sequence(
                                    &schema,
                                    &name,
                                    cname,
                                    attnum,
                                    ty,
                                    options,
                                    &lookup,
                                    &taken_relations,
                                )?;
                                taken_relations.insert(seq.name.clone());
                                sequences.push(seq);
                                identity = Some(identity_kind(*when));
                            }
                            ColumnConstraintKind::References { .. } => {
                                return Err(Error::not_supported(
                                    "FOREIGN KEY constraints are not supported yet",
                                )
                                .with_span(c.span));
                            }
                        }
                        st.both(c.span)?;
                    }
                    if serial.is_some() {
                        // SERIAL は制約の並びの末尾に DEFAULT と NOT NULL が足されたものとして扱う。
                        st.default(cd.type_name.span)?;
                        st.not_null(cd.type_name.span)?;
                    }
                    columns.push(ColumnDef {
                        name: cname.clone(),
                        attnum,
                        ty,
                        not_null: st.is_not_null,
                        default: None,
                        identity,
                    });
                    default_exprs.push(default);
                }
                TableElement::Constraint(tc) => match &tc.kind {
                    TableConstraintKind::Check(e) => checks.push(PendingCheck {
                        name: tc.name.as_ref(),
                        expr: e,
                    }),
                    TableConstraintKind::PrimaryKey(_) | TableConstraintKind::Unique(_) => {}
                    TableConstraintKind::ForeignKey { .. } => {
                        return Err(Error::not_supported(
                            "FOREIGN KEY constraints are not supported yet",
                        )
                        .with_span(tc.span));
                    }
                },
            }
        }
        let constraints = analyze_index_constraints(&name, &ct.elements, &mut columns)?;

        // PG reports an existing relation only after the column definitions
        // have been transformed (type modifiers, NULL/NOT NULL conflicts).
        if exists {
            return Err(Error::new(
                sqlstate::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists"),
            ));
        }

        // Pass 2: DEFAULT expressions (type-checked now, stored as text).
        let mut default_refs: Vec<(i16, Oid)> = Vec::new();
        for (col, def) in columns.iter_mut().zip(default_exprs) {
            if let Some(src) = def {
                let bound = self.default_expr(&src.expr, col)?;
                // PG（AddRelationNewConstraints）: NULL 定数の DEFAULT は既定値なしと同じ。
                if matches!(
                    &bound.kind,
                    super::bound::BoundExprKind::Literal(crate::types::Datum::Null)
                ) {
                    continue;
                }
                default_refs.extend(
                    collect_regclass_refs(&bound)
                        .into_iter()
                        .map(|o| (col.attnum, o)),
                );
                col.default = Some(BoundExprSource {
                    expr_sql: src.text.clone(),
                });
            }
        }

        // Pass 3: CHECK constraints over the new columns, and their names.
        let scope = ScopeStack::single(vec![ScopeRel {
            rte: crate::expr::RteId(0),
            hidden_name: None,
            refname: name.clone(),
            schema: Some(schema.clone()),
            table_oid: 0,
            system_columns: false,
            columns: columns
                .iter()
                .map(|c| ScopeColumn {
                    name: c.name.clone(),
                    ty: c.ty,
                    attnum: c.attnum,
                })
                .collect(),
        }]);
        let cx = ExprCtx::new(&scope, ParseExprKind::Check);
        // 明示の名前（CHECK と PRIMARY KEY / UNIQUE）は自動名に先立って取られる。
        let mut taken: HashSet<String> =
            constraints.iter().filter_map(|c| c.name.clone()).collect();
        let mut explicit: HashSet<&str> = HashSet::new();
        for n in checks.iter().filter_map(|c| c.name) {
            if !explicit.insert(n.value.as_str()) {
                return Err(Error::new(
                    sqlstate::DUPLICATE_OBJECT,
                    format!("check constraint \"{}\" already exists", n.value),
                ));
            }
            taken.insert(n.value.clone());
        }
        let mut check_defs = Vec::with_capacity(checks.len());
        for c in &checks {
            let b = self.transform_expr(&c.expr.expr, &cx)?;
            let b = self.coerce_to_boolean(b, "CHECK")?;
            let cname = if let Some(n) = c.name {
                n.value.clone()
            } else {
                let col = sole_column_ref(&b).map(|i| columns[i].name.as_str());
                let chosen = naming::choose_constraint_name(
                    &name,
                    col,
                    "check",
                    CREATE_NAMESPACE,
                    &lookup,
                    &taken,
                )?;
                taken.insert(chosen.clone());
                chosen
            };
            check_defs.push(CheckDef {
                name: cname,
                expr_sql: c.expr.text.clone(),
                no_inherit: c.expr.no_inherit,
            });
        }

        Ok(BoundCreateTable {
            schema,
            name,
            if_not_exists: ct.if_not_exists,
            columns,
            checks: check_defs,
            constraints,
            sequences,
            options: convert_options(&ct.options),
            default_refs,
        })
    }

    /// IDENTITY 列の暗黙のシーケンス（08 §5.8）。
    #[allow(clippy::too_many_arguments)]
    fn identity_sequence(
        &self,
        schema: &str,
        table: &str,
        column: &str,
        attnum: i16,
        ty: SqlType,
        options: &[SeqOption],
        lookup: &ReaderLookup<'_>,
        taken: &HashSet<String>,
    ) -> Result<BoundCreateSequence> {
        let parsed = self.parse_seq_options(options, SeqOptMode::Identity)?;
        let mut opts = parsed.opts;
        opts.as_type = Some(ty.oid);
        let out = init_params(&opts, true, InitMode::Create)?;
        let seq_name = if let Some(n) = parsed.sequence_name {
            let (s, name) = self.create_target(n)?;
            if s != schema {
                return Err(Error::not_supported(
                    "an identity sequence in another schema is not supported yet",
                )
                .with_span(n.span));
            }
            if taken.contains(&name) || lookup.relation_exists(CREATE_NAMESPACE, &name)? {
                return Err(Error::new(
                    sqlstate::DUPLICATE_TABLE,
                    format!("relation \"{name}\" already exists"),
                ));
            }
            name
        } else {
            naming::choose_relation_name(
                table,
                Some(column),
                "seq",
                CREATE_NAMESPACE,
                false,
                lookup,
                taken,
            )?
        };
        Ok(BoundCreateSequence {
            schema: schema.to_owned(),
            namespace: CREATE_NAMESPACE,
            name: seq_name,
            if_not_exists: false,
            params: out.params,
            initial: out.state,
            owner: SeqOwner::NewTableColumn {
                attnum,
                serial_default: false,
            },
            for_identity: true,
        })
    }

    pub(super) fn analyze_drop_table(&self, dt: &DropTable) -> Result<BoundDropTable> {
        let mut tables: Vec<Arc<TableDef>> = Vec::new();
        let mut missing = Vec::new();
        for n in &dt.names {
            let (schema, rel) = self.relation_parts(n)?;
            let found = self
                .catalog
                .relation_kind(schema.map(|s| s.value.as_str()), &rel.value)?;
            if let Some((_, kind)) = found.filter(|(_, k)| *k != RelKind::Table) {
                let hint = if kind == RelKind::Index {
                    "Use DROP INDEX to remove an index."
                } else {
                    "Use DROP SEQUENCE to remove a sequence."
                };
                return Err(Error::new(
                    sqlstate::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a table", rel.value),
                )
                .with_hint(hint)
                .with_span(n.span));
            }
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
        Ok(BoundDropTable {
            tables,
            missing,
            behavior: if dt.behavior == Some(crate::sql::ast::DropBehavior::Cascade) {
                crate::catalog::depend::DropBehavior::Cascade
            } else {
                crate::catalog::depend::DropBehavior::Restrict
            },
        })
    }

    // ----- シーケンス（08 §5.4〜§5.6） ---------------------------------------------------------

    /// `CREATE / ALTER SEQUENCE` と IDENTITY のオプションを `SeqOptions` にする。同じオプションの重複は
    /// 42601、数値は `parse_seq_int`、`AS` の型は解決する（存在しなければ 42704）。
    fn parse_seq_options<'a>(
        &self,
        options: &'a [SeqOption],
        mode: SeqOptMode,
    ) -> Result<ParsedSeqOptions<'a>> {
        let mut out = ParsedSeqOptions {
            opts: SeqOptions::default(),
            owned_by: None,
            sequence_name: None,
        };
        let mut seen = [false; 10];
        for o in options {
            let slot = seq_option_slot(&o.kind);
            let redundant = || {
                Error::new(sqlstate::SYNTAX_ERROR, "conflicting or redundant options")
                    .with_span(o.span)
            };
            // SEQUENCE NAME は重複を調べない（PostgreSQL の `init_params` と同じ）。
            if slot != 9 && std::mem::replace(&mut seen[slot], true) {
                return Err(redundant());
            }
            match &o.kind {
                SeqOptionKind::As(tn) => {
                    // IDENTITY の AS は列の型。PostgreSQL は型を表す AS を先頭に足すので、利用者の AS は重複。
                    if mode == SeqOptMode::Identity {
                        return Err(redundant());
                    }
                    out.opts.as_type = Some(self.resolve_type_name(tn)?.oid);
                }
                SeqOptionKind::Increment(n) => out.opts.increment = Some(parse_seq_int(&n.text)?),
                SeqOptionKind::MinValue(n) => {
                    out.opts.min = Some(n.as_ref().map(|n| parse_seq_int(&n.text)).transpose()?);
                }
                SeqOptionKind::MaxValue(n) => {
                    out.opts.max = Some(n.as_ref().map(|n| parse_seq_int(&n.text)).transpose()?);
                }
                SeqOptionKind::Start(n) => out.opts.start = Some(parse_seq_int(&n.text)?),
                SeqOptionKind::Restart(n) => {
                    out.opts.restart =
                        Some(n.as_ref().map(|n| parse_seq_int(&n.text)).transpose()?);
                }
                SeqOptionKind::Cache(n) => out.opts.cache = Some(parse_seq_int(&n.text)?),
                SeqOptionKind::Cycle(b) => out.opts.cycle = Some(*b),
                SeqOptionKind::OwnedBy(n) => {
                    if mode == SeqOptMode::Identity {
                        return Err(Error::not_supported(
                            "OWNED BY is not supported in identity column options",
                        )
                        .with_span(o.span));
                    }
                    out.owned_by = Some(n);
                }
                SeqOptionKind::SequenceName(n) => {
                    if mode != SeqOptMode::Identity {
                        return Err(Error::new(
                            sqlstate::SYNTAX_ERROR,
                            "invalid sequence option SEQUENCE NAME",
                        )
                        .with_span(o.span));
                    }
                    out.sequence_name = Some(n);
                }
            }
        }
        Ok(out)
    }

    /// `OWNED BY NONE | [schema.]table.column` の解析側（08 §5.5 の表）。
    fn resolve_owned_by(&self, seq_schema: &str, name: &ObjectName) -> Result<OwnedByTarget> {
        let parts = &name.parts;
        if parts.len() < 2 {
            if parts.len() == 1 && parts[0].value == "none" {
                return Ok(OwnedByTarget::None);
            }
            return Err(
                Error::new(sqlstate::SYNTAX_ERROR, "invalid OWNED BY option")
                    .with_hint("Specify OWNED BY table.column or OWNED BY NONE.")
                    .with_span(name.span),
            );
        }
        let (rel_parts, column) = parts.split_at(parts.len() - 1);
        let column = &column[0];
        let rel = ObjectName {
            parts: rel_parts.to_vec(),
            span: name.span,
        };
        let (schema, table) = self.relation_parts(&rel)?;
        let Some((table_oid, kind)) = self
            .catalog
            .relation_kind(schema.map(|s| s.value.as_str()), &table.value)?
        else {
            return Err(Error::new(
                sqlstate::UNDEFINED_TABLE,
                format!("relation \"{}\" does not exist", display_name(&rel)),
            )
            .with_span(rel.span));
        };
        if kind != RelKind::Table {
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("sequence cannot be owned by relation \"{}\"", table.value),
            )
            .with_detail(format!(
                "This operation is not supported for {}.",
                relkind_plural(kind)
            )));
        }
        let def = self
            .catalog
            .table_by_oid(table_oid)?
            .ok_or_else(|| Error::internal("relation vanished during analysis"))?;
        if def.schema != seq_schema {
            return Err(Error::new(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "sequence must be in same schema as table it is linked to",
            ));
        }
        let Some(col) = def.column(&column.value) else {
            return Err(Error::new(
                sqlstate::UNDEFINED_COLUMN,
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    column.value, def.name
                ),
            ));
        };
        Ok(OwnedByTarget::Column {
            table: table_oid,
            attnum: col.attnum,
        })
    }

    pub(super) fn analyze_create_sequence(
        &self,
        cs: &CreateSequence,
    ) -> Result<BoundCreateSequence> {
        match cs.persistence {
            SeqPersistence::Permanent => {}
            SeqPersistence::Temporary => {
                return Err(
                    Error::not_supported("temporary sequences are not supported yet")
                        .with_span(cs.span),
                );
            }
            SeqPersistence::Unlogged => {
                return Err(
                    Error::not_supported("unlogged sequences are not supported yet")
                        .with_span(cs.span),
                );
            }
        }
        let (schema, name) = self.create_target(&cs.name)?;
        let exists = self.catalog.relation_kind(Some(&schema), &name)?.is_some();
        let bound = |params, initial, owner| BoundCreateSequence {
            schema: schema.clone(),
            namespace: CREATE_NAMESPACE,
            name: name.clone(),
            if_not_exists: cs.if_not_exists,
            params,
            initial,
            owner,
            for_identity: false,
        };
        if exists && cs.if_not_exists {
            // オプションは検査しない（PostgreSQL の `DefineSequence` と同じ順序）。
            let out = init_params(&SeqOptions::default(), false, InitMode::Create)?;
            return Ok(bound(out.params, out.state, SeqOwner::None));
        }
        let parsed = self.parse_seq_options(&cs.options, SeqOptMode::Create)?;
        let owner = match parsed.owned_by {
            None => SeqOwner::None,
            Some(n) => match self.resolve_owned_by(&schema, n)? {
                OwnedByTarget::None => SeqOwner::None,
                OwnedByTarget::Column { table, attnum } => SeqOwner::Column { table, attnum },
            },
        };
        let out = init_params(&parsed.opts, false, InitMode::Create)?;
        if exists {
            return Err(Error::new(
                sqlstate::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists"),
            ));
        }
        Ok(bound(out.params, out.state, owner))
    }

    pub(super) fn analyze_alter_sequence(&self, a: &AlterSequence) -> Result<BoundAlterSequence> {
        let (schema, rel) = self.relation_parts(&a.name)?;
        let shown = display_name(&a.name);
        let found = self
            .catalog
            .relation_kind(schema.map(|s| s.value.as_str()), &rel.value)?;
        let Some((oid, kind)) = found else {
            if a.if_exists {
                return Ok(BoundAlterSequence {
                    target: None,
                    missing_name: shown,
                    action: BoundAlterAction::OwnerNoop,
                });
            }
            return Err(Error::new(
                sqlstate::UNDEFINED_TABLE,
                format!("relation \"{shown}\" does not exist"),
            ));
        };
        if kind != RelKind::Sequence {
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("cannot open relation \"{}\"", rel.value),
            )
            .with_detail(format!(
                "This operation is not supported for {}.",
                relkind_plural(kind)
            )));
        }
        let def = self
            .catalog
            .table_by_oid(oid)?
            .ok_or_else(|| Error::internal("relation vanished during analysis"))?;
        let action = match &a.action {
            AlterSequenceAction::Options(options) => {
                let parsed = self.parse_seq_options(options, SeqOptMode::Alter)?;
                let owned_by = parsed
                    .owned_by
                    .map(|n| self.resolve_owned_by(&def.schema, n))
                    .transpose()?;
                BoundAlterAction::Options {
                    options: parsed.opts,
                    owned_by,
                }
            }
            AlterSequenceAction::OwnerTo(_) => BoundAlterAction::OwnerNoop,
            AlterSequenceAction::RenameTo(_) => {
                return Err(super::not_supported("ALTER SEQUENCE ... RENAME", a.span));
            }
            AlterSequenceAction::SetSchema(_) => {
                return Err(super::not_supported(
                    "ALTER SEQUENCE ... SET SCHEMA",
                    a.span,
                ));
            }
        };
        Ok(BoundAlterSequence {
            target: Some(def),
            missing_name: shown,
            action,
        })
    }

    pub(super) fn analyze_drop_sequence(&self, d: &DropSequence) -> Result<BoundDropSequence> {
        let mut targets: Vec<Arc<TableDef>> = Vec::new();
        let mut missing = Vec::new();
        for n in &d.names {
            let (schema, rel) = self.relation_parts(n)?;
            let shown = display_name(n);
            let found = self
                .catalog
                .relation_kind(schema.map(|s| s.value.as_str()), &rel.value)?;
            let Some((oid, kind)) = found else {
                if d.if_exists {
                    missing.push(shown);
                    continue;
                }
                return Err(Error::new(
                    sqlstate::UNDEFINED_TABLE,
                    format!("sequence \"{shown}\" does not exist"),
                )
                .with_span(n.span));
            };
            if kind != RelKind::Sequence {
                let hint = if kind == RelKind::Index {
                    "Use DROP INDEX to remove an index."
                } else {
                    "Use DROP TABLE to remove a table."
                };
                return Err(Error::new(
                    sqlstate::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a sequence", rel.value),
                )
                .with_hint(hint)
                .with_span(n.span));
            }
            if targets.iter().any(|t| t.oid == oid) {
                continue;
            }
            let def = self
                .catalog
                .table_by_oid(oid)?
                .ok_or_else(|| Error::internal("relation vanished during analysis"))?;
            targets.push(def);
        }
        Ok(BoundDropSequence {
            targets,
            missing,
            cascade: d.cascade,
        })
    }
}

/// `This operation is not supported for {tables|indexes|sequences}.` の名詞。
fn relkind_plural(kind: RelKind) -> &'static str {
    match kind {
        RelKind::Table => "tables",
        RelKind::Index => "indexes",
        RelKind::Sequence => "sequences",
    }
}

impl Analyzer<'_> {
    /// `ALTER TABLE ... ADD [CONSTRAINT n] CHECK (expr)`。`table` が `None`（`IF EXISTS` で表がない）のときは
    /// 式を解析せず、実行されない束縛だけを返す。
    pub(super) fn analyze_add_check(
        &self,
        target: super::bound::AlterTarget,
        c: &crate::sql::ast::TableConstraint,
        e: &SourceExpr,
    ) -> Result<super::bound::BoundAlterTableAddCheck> {
        let super::bound::AlterTarget::Found(t) = &target else {
            return Ok(super::bound::BoundAlterTableAddCheck {
                target,
                name: String::new(),
                expr_sql: e.text.clone(),
                no_inherit: e.no_inherit,
                expr: None,
            });
        };
        let scope = ScopeStack::single(vec![ScopeRel {
            rte: crate::expr::RteId(0),
            hidden_name: None,
            refname: t.name.clone(),
            schema: Some(t.schema.clone()),
            table_oid: t.oid,
            system_columns: false,
            columns: t
                .columns
                .iter()
                .map(|col| ScopeColumn {
                    name: col.name.clone(),
                    ty: col.ty,
                    attnum: col.attnum,
                })
                .collect(),
        }]);
        let cx = ExprCtx::new(&scope, ParseExprKind::Check);
        let b = self.transform_expr(&e.expr, &cx)?;
        let b = self.coerce_to_boolean(b, "CHECK")?;
        let mut taken: HashSet<String> = t.checks.iter().map(|k| k.name.clone()).collect();
        taken.extend(
            t.indexes
                .iter()
                .filter_map(|i| i.constraint.as_ref().map(|k| k.name.clone())),
        );
        let name = if let Some(n) = &c.name {
            if taken.contains(&n.value) {
                return Err(Error::new(
                    sqlstate::DUPLICATE_OBJECT,
                    format!(
                        "constraint \"{}\" for relation \"{}\" already exists",
                        n.value, t.name
                    ),
                ));
            }
            n.value.clone()
        } else {
            let lookup = ReaderLookup {
                catalog: self.catalog,
                schema: &t.schema,
            };
            let col = sole_column_ref(&b)
                .and_then(|i| t.columns.get(i))
                .map(|x| x.name.as_str());
            naming::choose_constraint_name(&t.name, col, "check", t.namespace, &lookup, &taken)?
        };
        Ok(super::bound::BoundAlterTableAddCheck {
            target,
            name,
            expr_sql: e.text.clone(),
            no_inherit: e.no_inherit,
            expr: Some(b),
        })
    }
}

/// One CHECK constraint collected from the CREATE TABLE elements.
struct PendingCheck<'a> {
    name: Option<&'a Ident>,
    expr: &'a SourceExpr,
}

// ----- IDENTITY の規則（08 §4.9、§5.8。N1 が INSERT / UPDATE の解析から呼ぶ） --------------------

/// INSERT の 1 つの列について、指定の形。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)] // `Omitted` / `DefaultKeyword` は単体テストと将来の呼び出し用
pub(super) enum GivenValue {
    /// 列リストにない。
    Omitted,
    /// `VALUES` の `DEFAULT`。
    DefaultKeyword,
    /// それ以外の式・SELECT の出力。
    Value,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum IdentityInsert {
    /// 与えられた値を使う。
    Given,
    /// `nextval` を使う。
    Generate,
}

fn always_detail(col: &ColumnDef) -> String {
    format!(
        "Column \"{}\" is an identity column defined as GENERATED ALWAYS.",
        col.name
    )
}

/// `col.identity` が `None` の列は呼ばない（普通の DEFAULT の規則）。呼ばれたら値あり = `Given`、
/// それ以外 = `Generate`（DEFAULT を使う）。表は 08 §5.8。
pub(super) fn identity_insert_rule(
    col: &ColumnDef,
    given: GivenValue,
    overriding: Option<OverridingKind>,
) -> Result<IdentityInsert> {
    if given != GivenValue::Value {
        return Ok(IdentityInsert::Generate);
    }
    if overriding.is_none() && col.identity == Some(IdentityKind::Always) {
        return Err(Error::new(
            sqlstate::GENERATED_ALWAYS,
            format!(
                "cannot insert a non-DEFAULT value into column \"{}\"",
                col.name
            ),
        )
        .with_detail(always_detail(col))
        .with_hint("Use OVERRIDING SYSTEM VALUE to override."));
    }
    // USER VALUE は identity 列だけに効く（与えた値を捨てる）。SYSTEM VALUE と指定なしは与えた値を使う。
    Ok(
        if overriding == Some(OverridingKind::User) && col.identity.is_some() {
            IdentityInsert::Generate
        } else {
            IdentityInsert::Given
        },
    )
}

/// `UPDATE ... SET col = <式>`。`GENERATED ALWAYS` の列は `DEFAULT` にしか更新できない（428C9）。
pub(super) fn identity_update_rule(col: &ColumnDef, is_default_keyword: bool) -> Result<()> {
    if col.identity == Some(IdentityKind::Always) && !is_default_keyword {
        return Err(Error::new(
            sqlstate::GENERATED_ALWAYS,
            format!("column \"{}\" can only be updated to DEFAULT", col.name),
        )
        .with_detail(always_detail(col)));
    }
    Ok(())
}

/// IDENTITY の暗黙の DEFAULT: `nextval(<seq oid>::regclass)` を列型に代入キャストしたもの。identity でない
/// 列は `None`（INSERT の defaults、UPDATE の DEFAULT、COPY の列変換が使う。`Var` を含まない）。
pub(super) fn identity_default_expr(
    table: &TableDef,
    col: &ColumnDef,
    catalog: &dyn CatalogReader,
) -> Result<Option<BoundExpr>> {
    if col.identity.is_none() {
        return Ok(None);
    }
    let Some(&(_, seq)) = table.identity_seqs.iter().find(|(a, _)| *a == col.attnum) else {
        return Err(Error::internal(format!(
            "identity column \"{}\" of table \"{}\" has no sequence",
            col.name, table.name
        )));
    };
    let with_default = ColumnDef {
        default: Some(BoundExprSource {
            expr_sql: format!("nextval('{seq}'::regclass)"),
        }),
        ..col.clone()
    };
    super::analyze_column_default_bound(catalog, &with_default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::bound::{BoundDdl, BoundStatement};
    use crate::analyzer::tests::{catalog, run};
    use crate::catalog::fake::{FakeCatalog, TableBuilder};

    fn ddl(c: &FakeCatalog, sql: &str) -> BoundDdl {
        match run(c, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
            BoundStatement::Ddl(d) => d,
            other => panic!("{sql}: {other:?}"),
        }
    }

    fn create_table(c: &FakeCatalog, sql: &str) -> BoundCreateTable {
        match ddl(c, sql) {
            BoundDdl::CreateTable(b) => b,
            other => panic!("{other:?}"),
        }
    }

    fn create_seq(c: &FakeCatalog, sql: &str) -> BoundCreateSequence {
        match ddl(c, sql) {
            BoundDdl::CreateSequence(b) => b,
            other => panic!("{other:?}"),
        }
    }

    /// (SQLSTATE, メッセージ)。
    #[track_caller]
    fn fail(c: &FakeCatalog, sql: &str) -> (&'static str, String) {
        match run(c, sql) {
            Ok(s) => panic!("expected an error for {sql}, got {s:?}"),
            Err(e) => (e.sqlstate.code(), e.message),
        }
    }

    #[track_caller]
    fn fail_is(c: &FakeCatalog, sql: &str, code: &str, message: &str) {
        let (c1, m1) = fail(c, sql);
        assert_eq!((c1, m1.as_str()), (code, message), "{sql}");
    }

    /// `regclass` は 09（T）が `is_supported_type` に足す。それまでは `nextval('..'::regclass)` を解析できない。
    #[allow(clippy::print_stderr)]
    fn regclass_ready() -> bool {
        let ok = builtin::is_supported_type(oid::REGCLASS);
        if !ok {
            eprintln!("skipped: regclass is not supported yet (09)");
        }
        ok
    }

    fn with_seq_catalog() -> FakeCatalog {
        let mut c = catalog();
        c.add(
            &TableBuilder::new("own")
                .serial("id", SqlType::INT4)
                .column("v", SqlType::TEXT),
        );
        c.add(&TableBuilder::new("idt").identity("id", SqlType::INT8, IdentityKind::Always));
        c
    }

    #[test]
    fn serial_types_become_integer_columns_with_sequences() {
        let c = catalog();
        let b = create_table(
            &c,
            "CREATE TABLE s1 (a serial, b smallserial, c bigserial, d serial2, e serial4, f serial8, g int)",
        );
        let types: Vec<Oid> = b.columns.iter().map(|c| c.ty.oid).collect();
        assert_eq!(
            types,
            [
                oid::INT4,
                oid::INT2,
                oid::INT8,
                oid::INT2,
                oid::INT4,
                oid::INT8,
                oid::INT4
            ]
        );
        assert!(
            b.columns[..6]
                .iter()
                .all(|c| c.not_null && c.default.is_none())
        );
        assert!(!b.columns[6].not_null);
        assert_eq!(b.sequences.len(), 6);
        let s = &b.sequences[0];
        assert_eq!(s.name, "s1_a_seq");
        assert_eq!(s.schema, "public");
        assert_eq!(s.namespace, 2200);
        assert_eq!(
            s.owner,
            SeqOwner::NewTableColumn {
                attnum: 1,
                serial_default: true
            }
        );
        assert!(!s.for_identity);
        assert_eq!(s.params.type_oid, oid::INT4);
        assert_eq!(
            (s.params.start, s.params.min, s.params.max),
            (1, 1, i64::from(i32::MAX))
        );
        assert_eq!(
            (s.initial.last_value, s.initial.log_cnt, s.initial.is_called),
            (1, 0, false)
        );
        assert_eq!(b.sequences[1].params.max, i64::from(i16::MAX));
        assert_eq!(b.sequences[2].params.max, i64::MAX);
        let names: Vec<&str> = b.sequences.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "s1_a_seq", "s1_b_seq", "s1_c_seq", "s1_d_seq", "s1_e_seq", "s1_f_seq"
            ]
        );
        let attnums: Vec<i16> = b
            .sequences
            .iter()
            .map(|s| match s.owner {
                SeqOwner::NewTableColumn { attnum, .. } => attnum,
                _ => 0,
            })
            .collect();
        assert_eq!(attnums, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn serial_ignores_a_modifier_and_if_not_exists_makes_nothing() {
        let c = with_seq_catalog();
        let b = create_table(
            &c,
            "CREATE TABLE s2 (a serial PRIMARY KEY, b int DEFAULT 3)",
        );
        assert_eq!(b.sequences.len(), 1);
        assert!(b.columns[0].not_null);
        assert_eq!(b.constraints.len(), 1);
        // 表がすでにあれば何も解析しない（シーケンスも作らない）。
        let b = create_table(&c, "CREATE TABLE IF NOT EXISTS own (a serial, b nope)");
        assert!(b.if_not_exists && b.sequences.is_empty() && b.columns.is_empty());
    }

    #[test]
    fn serial_names_avoid_existing_relations_and_each_other() {
        let mut c = catalog();
        c.add(&TableBuilder::new("y_a_seq"));
        c.add(&TableBuilder::new("y_a_seq1"));
        let b = create_table(&c, "CREATE TABLE y (a serial)");
        assert_eq!(b.sequences[0].name, "y_a_seq2");
        // 同じ文の中で先に決めた名前（IDENTITY の SEQUENCE NAME）を避ける。
        let b = create_table(
            &c,
            "CREATE TABLE w (i int GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME w_s_seq), s serial)",
        );
        let names: Vec<&str> = b.sequences.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["w_s_seq", "w_s_seq1"]);
        let b = create_table(&c, "CREATE TABLE \"x 17\" (\"a b\" serial)");
        assert_eq!(b.sequences[0].name, "x 17_a b_seq");
        let long_t = "t".repeat(40);
        let long_c = "c".repeat(40);
        let b = create_table(&c, &format!("CREATE TABLE {long_t} ({long_c} serial)"));
        let n = &b.sequences[0].name;
        assert!(n.len() <= 63 && n.ends_with("_seq"), "{n}");
        // 表の名前が衝突する自動名: t_a_seq という表に serial a を持つ t を作る。
        let mut c2 = catalog();
        c2.add(&TableBuilder::new("t_a_seq"));
        let b = create_table(&c2, "CREATE TABLE t_a (seq serial)");
        assert_eq!(b.sequences[0].name, "t_a_seq_seq");
    }

    #[test]
    fn serial_errors_and_column_constraint_conflicts() {
        let c = catalog();
        fail_is(
            &c,
            "CREATE TABLE x1 (a serial[])",
            "0A000",
            "array of serial is not implemented",
        );
        fail_is(
            &c,
            "CREATE TABLE x4 (a serial DEFAULT 5)",
            "42601",
            "multiple default values specified for column \"a\" of table \"x4\"",
        );
        fail_is(
            &c,
            "CREATE TABLE x5 (a serial NULL)",
            "42601",
            "conflicting NULL/NOT NULL declarations for column \"a\" of table \"x5\"",
        );
        fail_is(
            &c,
            "CREATE TABLE x5 (a int NULL NOT NULL)",
            "42601",
            "conflicting NULL/NOT NULL declarations for column \"a\" of table \"x5\"",
        );
        fail_is(
            &c,
            "CREATE TABLE x5 (a int NOT NULL NULL)",
            "42601",
            "conflicting NULL/NOT NULL declarations for column \"a\" of table \"x5\"",
        );
        fail_is(
            &c,
            "CREATE TABLE x4 (a int DEFAULT 1 DEFAULT 2)",
            "42601",
            "multiple default values specified for column \"a\" of table \"x4\"",
        );
        fail_is(
            &c,
            "CREATE TABLE x2 (a int GENERATED ALWAYS AS IDENTITY DEFAULT 1)",
            "42601",
            "both default and identity specified for column \"a\" of table \"x2\"",
        );
        fail_is(
            &c,
            "CREATE TABLE x2 (a int DEFAULT 1 GENERATED ALWAYS AS IDENTITY)",
            "42601",
            "both default and identity specified for column \"a\" of table \"x2\"",
        );
        fail_is(
            &c,
            "CREATE TABLE x2 (a serial GENERATED ALWAYS AS IDENTITY)",
            "42601",
            "both default and identity specified for column \"a\" of table \"x2\"",
        );
        fail_is(
            &c,
            "CREATE TABLE x3 (a int GENERATED ALWAYS AS IDENTITY GENERATED BY DEFAULT AS IDENTITY)",
            "42601",
            "multiple identity specifications for column \"a\" of table \"x3\"",
        );
        // NULL と IDENTITY（暗黙の NOT NULL）。
        fail_is(
            &c,
            "CREATE TABLE x5 (a int NULL GENERATED ALWAYS AS IDENTITY)",
            "42601",
            "conflicting NULL/NOT NULL declarations for column \"a\" of table \"x5\"",
        );
        // 正しい組み合わせ。
        let b = create_table(
            &c,
            "CREATE TABLE ok1 (a serial NOT NULL, b int NOT NULL NOT NULL)",
        );
        assert!(b.columns[0].not_null && b.columns[1].not_null);
    }

    #[test]
    fn identity_columns_get_an_internal_sequence() {
        let c = catalog();
        let b = create_table(
            &c,
            "CREATE TABLE i1 (id bigint GENERATED ALWAYS AS IDENTITY (START WITH 10 INCREMENT BY 5 MINVALUE 10 MAXVALUE 1000 CACHE 3 CYCLE), \
             d smallint GENERATED BY DEFAULT AS IDENTITY, n int)",
        );
        assert_eq!(b.columns[0].identity, Some(IdentityKind::Always));
        assert_eq!(b.columns[1].identity, Some(IdentityKind::ByDefault));
        assert_eq!(b.columns[2].identity, None);
        assert!(b.columns[0].not_null && b.columns[1].not_null && !b.columns[2].not_null);
        assert!(b.columns.iter().all(|c| c.default.is_none()));
        assert_eq!(b.sequences.len(), 2);
        let s = &b.sequences[0];
        assert_eq!(s.name, "i1_id_seq");
        assert!(s.for_identity);
        assert_eq!(
            s.owner,
            SeqOwner::NewTableColumn {
                attnum: 1,
                serial_default: false
            }
        );
        assert_eq!(s.params.type_oid, oid::INT8);
        assert_eq!(
            (
                s.params.start,
                s.params.increment,
                s.params.min,
                s.params.max,
                s.params.cache,
                s.params.cycle
            ),
            (10, 5, 10, 1000, 3, true)
        );
        assert_eq!((s.initial.last_value, s.initial.is_called), (10, false));
        assert_eq!(b.sequences[1].params.type_oid, oid::INT2);
        assert_eq!(b.sequences[1].params.max, i64::from(i16::MAX));
    }

    #[test]
    fn identity_options_and_type_errors() {
        let c = catalog();
        let b = create_table(
            &c,
            "CREATE TABLE i2 (id int GENERATED BY DEFAULT AS IDENTITY (SEQUENCE NAME my_seq))",
        );
        assert_eq!(b.sequences[0].name, "my_seq");
        fail_is(
            &c,
            "CREATE TABLE i3 (id text GENERATED ALWAYS AS IDENTITY)",
            "22023",
            "identity column type must be smallint, integer, or bigint",
        );
        fail_is(
            &c,
            "CREATE TABLE i3 (id numeric GENERATED ALWAYS AS IDENTITY)",
            "22023",
            "identity column type must be smallint, integer, or bigint",
        );
        fail_is(
            &c,
            "CREATE TABLE i3 (id int GENERATED ALWAYS AS IDENTITY (AS bigint))",
            "42601",
            "conflicting or redundant options",
        );
        fail_is(
            &c,
            "CREATE TABLE i3 (id int GENERATED ALWAYS AS IDENTITY (START 1 START 2))",
            "42601",
            "conflicting or redundant options",
        );
        fail_is(
            &c,
            "CREATE TABLE i3 (id int GENERATED ALWAYS AS IDENTITY (MAXVALUE 0))",
            "22023",
            "MINVALUE (1) must be less than MAXVALUE (0)",
        );
        fail_is(
            &c,
            "CREATE TABLE i3 (id int GENERATED ALWAYS AS IDENTITY (OWNED BY t.a))",
            "0A000",
            "OWNED BY is not supported in identity column options",
        );
        // 明示の名前が既存のリレーションや同じ文の別のシーケンスと衝突する。
        fail_is(
            &c,
            "CREATE TABLE i3 (id int GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME t))",
            "42P07",
            "relation \"t\" already exists",
        );
        fail_is(
            &c,
            "CREATE TABLE i3 (a int GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME q), b int GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME q))",
            "42P07",
            "relation \"q\" already exists",
        );
    }

    #[test]
    fn create_table_keeps_with_options_and_constraint_names() {
        let c = catalog();
        let b = create_table(
            &c,
            "CREATE TABLE ck (a int CHECK (a > 0), b int, CHECK (a < b), CONSTRAINT ck_a_check1 CHECK (b > 0)) WITH (fillfactor = 70)",
        );
        let names: Vec<&str> = b.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["ck_a_check", "ck_check", "ck_a_check1"]);
        assert_eq!(b.options.len(), 1);
        fail_is(
            &c,
            "CREATE TABLE ck (a int, CONSTRAINT k CHECK (a > 0), CONSTRAINT k CHECK (a < 9))",
            "42710",
            "check constraint \"k\" already exists",
        );
    }

    #[test]
    fn create_sequence_defaults_and_options() {
        let c = catalog();
        let s = create_seq(&c, "CREATE SEQUENCE s");
        assert_eq!(
            (s.schema.as_str(), s.name.as_str(), s.namespace),
            ("public", "s", 2200)
        );
        assert_eq!(s.owner, SeqOwner::None);
        assert!(!s.for_identity && !s.if_not_exists);
        let p = s.params;
        assert_eq!(
            (
                p.type_oid,
                p.start,
                p.increment,
                p.min,
                p.max,
                p.cache,
                p.cycle,
                p.owned_by
            ),
            (oid::INT8, 1, 1, 1, i64::MAX, 1, false, None)
        );
        let s = create_seq(
            &c,
            "CREATE SEQUENCE IF NOT EXISTS public.s2 AS smallint START WITH 5 INCREMENT BY -1 MINVALUE -10 MAXVALUE 7 CACHE 4 CYCLE",
        );
        assert!(s.if_not_exists);
        assert_eq!(s.params.type_oid, oid::INT2);
        assert_eq!(
            (
                s.params.start,
                s.params.increment,
                s.params.min,
                s.params.max,
                s.params.cache,
                s.params.cycle
            ),
            (5, -1, -10, 7, 4, true)
        );
        let s = create_seq(
            &c,
            "CREATE SEQUENCE s3 AS integer NO MINVALUE NO MAXVALUE NO CYCLE RESTART WITH 9",
        );
        assert_eq!(s.params.type_oid, oid::INT4);
        assert_eq!(s.initial.last_value, 9);
        assert_eq!(s.params.start, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn create_sequence_errors() {
        let c = with_seq_catalog();
        fail_is(
            &c,
            "CREATE TEMP SEQUENCE s",
            "0A000",
            "temporary sequences are not supported yet",
        );
        fail_is(
            &c,
            "CREATE UNLOGGED SEQUENCE s",
            "0A000",
            "unlogged sequences are not supported yet",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE pg_catalog.zz",
            "42501",
            "permission denied to create \"pg_catalog.zz\"",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE nosuch.s",
            "3F000",
            "schema \"nosuch\" does not exist",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s INCREMENT 1 INCREMENT 2",
            "42601",
            "conflicting or redundant options",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s CYCLE NO CYCLE",
            "42601",
            "conflicting or redundant options",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s SEQUENCE NAME x",
            "42601",
            "invalid sequence option SEQUENCE NAME",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s AS nope",
            "42704",
            "type \"nope\" does not exist",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s AS text",
            "22023",
            "sequence type must be smallint, integer, or bigint",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s MAXVALUE 9223372036854775808",
            "22003",
            "value \"9223372036854775808\" is out of range for type bigint",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s START 1.5",
            "22P02",
            "invalid input syntax for type bigint: \"1.5\"",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s INCREMENT 0",
            "22023",
            "INCREMENT must not be zero",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s AS smallint MAXVALUE 40000",
            "22023",
            "MAXVALUE (40000) is out of range for sequence data type smallint",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s MINVALUE 5 MAXVALUE 1",
            "22023",
            "MINVALUE (5) must be less than MAXVALUE (1)",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s START 0",
            "22023",
            "START value (0) cannot be less than MINVALUE (1)",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s CACHE 0",
            "22023",
            "CACHE (0) must be greater than zero",
        );
        // 同名のリレーション: init_params の検査より後。
        fail_is(
            &c,
            "CREATE SEQUENCE t",
            "42P07",
            "relation \"t\" already exists",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE t INCREMENT 0",
            "22023",
            "INCREMENT must not be zero",
        );
        // IF NOT EXISTS で存在すればオプションは見ない。
        let s = create_seq(&c, "CREATE SEQUENCE IF NOT EXISTS t INCREMENT 0");
        assert!(s.if_not_exists);
        assert_eq!(s.name, "t");
    }

    #[test]
    fn create_sequence_owned_by() {
        let c = with_seq_catalog();
        let own = c.table(None, "own").unwrap().unwrap();
        let s = create_seq(&c, "CREATE SEQUENCE s OWNED BY own.v");
        assert_eq!(
            s.owner,
            SeqOwner::Column {
                table: own.oid,
                attnum: 2
            }
        );
        let s = create_seq(&c, "CREATE SEQUENCE s OWNED BY public.own.id");
        assert_eq!(
            s.owner,
            SeqOwner::Column {
                table: own.oid,
                attnum: 1
            }
        );
        assert_eq!(
            create_seq(&c, "CREATE SEQUENCE s OWNED BY NONE").owner,
            SeqOwner::None
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s OWNED BY nope",
            "42601",
            "invalid OWNED BY option",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s OWNED BY nope.a",
            "42P01",
            "relation \"nope\" does not exist",
        );
        fail_is(
            &c,
            "CREATE SEQUENCE s OWNED BY own.nope",
            "42703",
            "column \"nope\" of relation \"own\" does not exist",
        );
        let (code, msg) = fail(&c, "CREATE SEQUENCE s OWNED BY own_id_seq.last_value");
        assert_eq!(
            (code, msg.as_str()),
            (
                "42809",
                "sequence cannot be owned by relation \"own_id_seq\""
            )
        );
        let e = run(&c, "CREATE SEQUENCE s OWNED BY own_id_seq.last_value").unwrap_err();
        assert_eq!(
            e.detail.as_deref(),
            Some("This operation is not supported for sequences.")
        );
        let e = run(&c, "CREATE SEQUENCE s OWNED BY nope").unwrap_err();
        assert_eq!(
            e.hint.as_deref(),
            Some("Specify OWNED BY table.column or OWNED BY NONE.")
        );
    }

    #[test]
    fn alter_sequence_analysis() {
        let c = with_seq_catalog();
        let BoundDdl::AlterSequence(a) = ddl(
            &c,
            "ALTER SEQUENCE own_id_seq RESTART INCREMENT 7 OWNED BY own.v",
        ) else {
            panic!()
        };
        let own = c.table(None, "own").unwrap().unwrap();
        assert_eq!(a.target.as_ref().unwrap().name, "own_id_seq");
        let BoundAlterAction::Options { options, owned_by } = a.action else {
            panic!()
        };
        assert_eq!(options.restart, Some(None));
        assert_eq!(options.increment, Some(7));
        assert_eq!(
            owned_by,
            Some(OwnedByTarget::Column {
                table: own.oid,
                attnum: 2
            })
        );
        let BoundDdl::AlterSequence(a) = ddl(&c, "ALTER SEQUENCE IF EXISTS nope MAXVALUE 5") else {
            panic!()
        };
        assert!(a.target.is_none());
        assert_eq!(a.missing_name, "nope");
        let BoundDdl::AlterSequence(a) = ddl(&c, "ALTER SEQUENCE own_id_seq OWNER TO postgres")
        else {
            panic!()
        };
        assert!(matches!(a.action, BoundAlterAction::OwnerNoop));
        fail_is(
            &c,
            "ALTER SEQUENCE nope MAXVALUE 5",
            "42P01",
            "relation \"nope\" does not exist",
        );
        fail_is(
            &c,
            "ALTER SEQUENCE t MAXVALUE 5",
            "42809",
            "cannot open relation \"t\"",
        );
        let e = run(&c, "ALTER SEQUENCE t MAXVALUE 5").unwrap_err();
        assert_eq!(
            e.detail.as_deref(),
            Some("This operation is not supported for tables.")
        );
        fail_is(
            &c,
            "ALTER SEQUENCE own_id_seq SEQUENCE NAME x",
            "42601",
            "invalid sequence option SEQUENCE NAME",
        );
        fail_is(
            &c,
            "ALTER SEQUENCE own_id_seq MAXVALUE 1 MAXVALUE 2",
            "42601",
            "conflicting or redundant options",
        );
        fail_is(
            &c,
            "ALTER SEQUENCE own_id_seq RENAME TO x",
            "0A000",
            "ALTER SEQUENCE ... RENAME is not supported yet",
        );
        fail_is(
            &c,
            "ALTER SEQUENCE own_id_seq SET SCHEMA x",
            "0A000",
            "ALTER SEQUENCE ... SET SCHEMA is not supported yet",
        );
    }

    #[test]
    fn drop_sequence_names() {
        let c = with_seq_catalog();
        let BoundDdl::DropSequence(d) = ddl(
            &c,
            "DROP SEQUENCE IF EXISTS own_id_seq, nope, own_id_seq CASCADE",
        ) else {
            panic!()
        };
        assert_eq!(d.targets.len(), 1);
        assert_eq!(d.missing, ["nope"]);
        assert!(d.cascade);
        fail_is(
            &c,
            "DROP SEQUENCE nope",
            "42P01",
            "sequence \"nope\" does not exist",
        );
        fail_is(&c, "DROP SEQUENCE t", "42809", "\"t\" is not a sequence");
        let e = run(&c, "DROP SEQUENCE t").unwrap_err();
        assert_eq!(e.hint.as_deref(), Some("Use DROP TABLE to remove a table."));
    }

    fn col(identity: Option<IdentityKind>) -> ColumnDef {
        ColumnDef {
            name: "id".into(),
            attnum: 1,
            ty: SqlType::INT8,
            not_null: true,
            default: None,
            identity,
        }
    }

    #[test]
    fn identity_insert_rule_covers_every_cell() {
        use GivenValue::{DefaultKeyword, Omitted, Value};
        use IdentityInsert::{Generate, Given};
        let always = col(Some(IdentityKind::Always));
        let by_default = col(Some(IdentityKind::ByDefault));
        for given in [Omitted, DefaultKeyword] {
            for ov in [
                None,
                Some(OverridingKind::System),
                Some(OverridingKind::User),
            ] {
                assert_eq!(identity_insert_rule(&always, given, ov).unwrap(), Generate);
                assert_eq!(
                    identity_insert_rule(&by_default, given, ov).unwrap(),
                    Generate
                );
            }
        }
        let e = identity_insert_rule(&always, Value, None).unwrap_err();
        assert_eq!(e.sqlstate.code(), "428C9");
        assert_eq!(
            e.message,
            "cannot insert a non-DEFAULT value into column \"id\""
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("Column \"id\" is an identity column defined as GENERATED ALWAYS.")
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("Use OVERRIDING SYSTEM VALUE to override.")
        );
        assert_eq!(
            identity_insert_rule(&always, Value, Some(OverridingKind::System)).unwrap(),
            Given
        );
        assert_eq!(
            identity_insert_rule(&always, Value, Some(OverridingKind::User)).unwrap(),
            Generate
        );
        assert_eq!(
            identity_insert_rule(&by_default, Value, None).unwrap(),
            Given
        );
        assert_eq!(
            identity_insert_rule(&by_default, Value, Some(OverridingKind::System)).unwrap(),
            Given
        );
        assert_eq!(
            identity_insert_rule(&by_default, Value, Some(OverridingKind::User)).unwrap(),
            Generate
        );
    }

    #[test]
    fn identity_update_rule_only_restricts_always() {
        let always = col(Some(IdentityKind::Always));
        let by_default = col(Some(IdentityKind::ByDefault));
        identity_update_rule(&always, true).unwrap();
        identity_update_rule(&by_default, false).unwrap();
        identity_update_rule(&col(None), false).unwrap();
        let e = identity_update_rule(&always, false).unwrap_err();
        assert_eq!(e.sqlstate.code(), "428C9");
        assert_eq!(e.message, "column \"id\" can only be updated to DEFAULT");
        assert_eq!(
            e.detail.as_deref(),
            Some("Column \"id\" is an identity column defined as GENERATED ALWAYS.")
        );
    }

    #[test]
    fn identity_default_expr_calls_nextval_on_the_sequence() {
        if !regclass_ready() {
            return;
        }
        let c = with_seq_catalog();
        let idt = c.table(None, "idt").unwrap().unwrap();
        let seq = idt.identity_seqs[0].1;
        let e = identity_default_expr(&idt, &idt.columns[0], &c).unwrap();
        let e = e.expect("identity default");
        assert_eq!(e.ty.oid, oid::INT8);
        assert_eq!(collect_regclass_refs(&e), vec![seq]);
        let own = c.table(None, "own").unwrap().unwrap();
        assert!(
            identity_default_expr(&own, &own.columns[1], &c)
                .unwrap()
                .is_none()
        );
        // identity でない列は DEFAULT を持たなくても None。
        assert!(
            identity_default_expr(&idt, &col(None), &c)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn default_null_is_no_default_and_no_inherit_is_kept() {
        let c = catalog();
        let b = create_table(
            &c,
            "CREATE TABLE dn (m text DEFAULT NULL, a int CHECK (a > 1) NO INHERIT, b int, CONSTRAINT kk CHECK (b > 0))",
        );
        assert!(b.columns[0].default.is_none());
        assert_eq!(
            b.checks
                .iter()
                .map(|k| (k.name.as_str(), k.no_inherit))
                .collect::<Vec<_>>(),
            vec![("dn_a_check", true), ("kk", false)]
        );
    }

    #[test]
    fn default_expressions_record_regclass_references() {
        if !regclass_ready() {
            return;
        }
        let c = with_seq_catalog();
        let seq = c.table(None, "own_id_seq").unwrap().unwrap().oid;
        let b = create_table(
            &c,
            "CREATE TABLE r (a int, b bigint DEFAULT nextval('own_id_seq'))",
        );
        assert_eq!(b.default_refs, vec![(2, seq)]);
        assert!(
            create_table(&c, "CREATE TABLE r (a int DEFAULT 1)")
                .default_refs
                .is_empty()
        );
    }

    #[test]
    fn type_names_use_typmod_and_known_unsupported_list() {
        let c = catalog();
        let b = create_table(
            &c,
            "CREATE TABLE ty (a numeric(10,2), b varchar(5), c numeric)",
        );
        assert_eq!(
            b.columns[0].ty,
            SqlType::new(oid::NUMERIC, (10 << 16) + 2 + 4)
        );
        assert_eq!(b.columns[1].ty, SqlType::varchar(5));
        assert_eq!(b.columns[2].ty, SqlType::of(oid::NUMERIC));
        fail_is(
            &c,
            "CREATE TABLE ty (a varchar(0))",
            "22023",
            "length for type varchar must be at least 1",
        );
        fail_is(
            &c,
            "CREATE TABLE ty (a numeric(0))",
            "22023",
            "NUMERIC precision 0 must be between 1 and 1000",
        );
        fail_is(
            &c,
            "CREATE TABLE ty (a nope)",
            "42704",
            "type \"nope\" does not exist",
        );
        fail_is(
            &c,
            "CREATE TABLE ty (a interval)",
            "0A000",
            "type interval is not supported yet",
        );
        fail_is(
            &c,
            "CREATE TABLE ty (a int[2][3])",
            "0A000",
            "array types are not supported yet",
        );
    }
}
