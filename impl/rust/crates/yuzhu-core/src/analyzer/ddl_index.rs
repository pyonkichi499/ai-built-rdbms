//! CREATE INDEX / DROP INDEX / DROP TABLE（`behavior`）/ TRUNCATE / VACUUM / ALTER TABLE の解析
//! （AST → `Bound*`。`m4/07-catalog-ddl.md` §4.6・§7.3、11 §7.3 の G-1）。
//!
//! 名前の解決、`42809` / `42P01` / `42704` の判定、列と opclass 名の解決まで行う。opclass の存在・型との
//! 適合、アクセスメソッド、システムカタログ、列数の上限などは実行時（`ddl/`）が検査する（07 §7.3 の「検」）。
//!
//! `analyze`（`mod.rs`）の分岐と、`ddl.rs` の `analyze_drop_table`（`behavior` を見ない M2 の版）からの
//! 呼び出しは、持ち主（Q1・統合）が配線する。

#![allow(dead_code, clippy::unused_self)]

use std::sync::Arc;

use super::Analyzer;
use super::bound::{
    AlterTarget, BoundAlterTableAddConstraint, BoundAlterTableOwner, BoundCreateIndex,
    BoundDropIndex, BoundDropTable, BoundIndexColumn, BoundTruncate, BoundVacuum, OwnerSpec,
    VacuumOption, VacuumTarget,
};
use super::ddl_constraint::{
    analyze_add_constraint, convert_options, placeholder_add_constraint, unsupported,
};
use super::select::display_name;
use crate::catalog::depend::DropBehavior;
use crate::catalog::{IndexDef, RelKind, TableDef};
use crate::error::{Error, Result, sqlstate};
use crate::sql::ast::{
    self, AlterTable, AlterTableAction, CreateIndex, DropIndex, DropTable, IndexElemKind,
    NullsOrder, ObjectName, RoleSpec, SortDirection, Truncate, Vacuum,
};
use crate::types::Oid;

/// `ALTER TABLE` の解析結果。
#[derive(Debug, Clone)]
pub(super) enum BoundAlterTable {
    AddConstraint(BoundAlterTableAddConstraint),
    AddCheck(super::bound::BoundAlterTableAddCheck),
    Owner(BoundAlterTableOwner),
}

/// `This operation is not supported for sequences.` など（42809 の DETAIL）。
fn not_supported_for(kind: RelKind) -> &'static str {
    match kind {
        RelKind::Sequence => "This operation is not supported for sequences.",
        RelKind::Index => "This operation is not supported for indexes.",
        RelKind::Table => "This operation is not supported for tables.",
    }
}

fn does_not_exist(name: &ObjectName) -> Error {
    Error::new(
        sqlstate::UNDEFINED_TABLE,
        format!("relation \"{}\" does not exist", display_name(name)),
    )
    .with_span(name.span)
}

impl Analyzer<'_> {
    /// リレーション名（表・索引・シーケンス）の検査と検索。見つからなければ `None`。
    fn lookup_relation(&self, name: &ObjectName) -> Result<Option<(Oid, RelKind)>> {
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
        let schema = name.schema().map(|s| s.value.as_str());
        self.catalog.relation_kind(schema, &name.name().value)
    }

    /// 表（`TableDef`）を引く。`lookup_relation` で `Table` / `Sequence` と分かったものだけ呼ぶ。
    fn relation_def(&self, name: &ObjectName) -> Result<Arc<TableDef>> {
        let schema = name.schema().map(|s| s.value.as_str());
        self.catalog
            .table(schema, &name.name().value)?
            .ok_or_else(|| does_not_exist(name))
    }

    /// `CREATE [UNIQUE] INDEX [CONCURRENTLY] [IF NOT EXISTS] [name] ON table [USING am] (cols) ...`。
    #[allow(clippy::too_many_lines)]
    pub(super) fn analyze_create_index(&self, ci: &CreateIndex) -> Result<BoundCreateIndex> {
        let table = match self.lookup_relation(&ci.table)? {
            None => return Err(does_not_exist(&ci.table)),
            Some((_, RelKind::Table)) => self.relation_def(&ci.table)?,
            Some((_, RelKind::Sequence)) => {
                return Err(Error::new(
                    sqlstate::WRONG_OBJECT_TYPE,
                    format!(
                        "cannot create index on relation \"{}\"",
                        ci.table.name().value
                    ),
                )
                .with_detail(not_supported_for(RelKind::Sequence))
                .with_span(ci.table.span));
            }
            Some((_, RelKind::Index)) => {
                return Err(Error::new(
                    sqlstate::WRONG_OBJECT_TYPE,
                    format!("cannot open relation \"{}\"", ci.table.name().value),
                )
                .with_detail(not_supported_for(RelKind::Index))
                .with_span(ci.table.span));
            }
        };
        if table.kind != RelKind::Table {
            return Err(Error::internal("relation kind mismatch in CREATE INDEX"));
        }
        if ci.where_clause.is_some() {
            return Err(unsupported(
                "partial indexes are not supported yet",
                ci.span,
            ));
        }
        if !ci.include.is_empty() {
            return Err(unsupported(
                "INCLUDE columns are not supported yet",
                ci.include[0].span,
            ));
        }
        if ci.nulls_not_distinct {
            return Err(unsupported(
                "NULLS NOT DISTINCT is not supported yet",
                ci.span,
            ));
        }
        if let Some(ts) = &ci.tablespace {
            return Err(unsupported("TABLESPACE is not supported yet", ts.span));
        }

        let method = ci
            .method
            .as_ref()
            .map_or_else(|| "btree".to_owned(), |m| m.value.to_lowercase());
        let mut columns = Vec::with_capacity(ci.columns.len());
        for elem in &ci.columns {
            if let Some(co) = &elem.collation {
                return Err(unsupported("collations are not supported yet", co.span));
            }
            let id = match &elem.kind {
                IndexElemKind::Column(id) => id,
                IndexElemKind::Expr(_) => {
                    return Err(unsupported(
                        "index expressions are not supported yet",
                        elem.span,
                    ));
                }
            };
            let Some(col) = table.column(&id.value) else {
                if crate::catalog::schema::SYSTEM_COLUMNS
                    .iter()
                    .any(|(n, _, _)| *n == id.value)
                {
                    return Err(unsupported(
                        "index creation on system columns is not supported",
                        id.span,
                    ));
                }
                return Err(Error::new(
                    sqlstate::UNDEFINED_COLUMN,
                    format!("column \"{}\" does not exist", id.value),
                )
                .with_span(id.span));
            };
            let opclass = elem
                .opclass
                .as_ref()
                .map(|oc| self.opclass_name(oc, &method))
                .transpose()?;
            let descending = elem.direction == Some(SortDirection::Desc);
            // NULLS の省略は「DESC なら先頭、ASC なら末尾」（07 §3.4）。
            let nulls_first = match elem.nulls {
                Some(NullsOrder::First) => true,
                Some(NullsOrder::Last) => false,
                None => descending,
            };
            columns.push(BoundIndexColumn {
                attnum: col.attnum,
                opclass,
                descending,
                nulls_first,
                span: elem.span,
            });
        }
        Ok(BoundCreateIndex {
            table,
            name: ci.name.as_ref().map(|n| n.value.clone()),
            unique: ci.unique,
            if_not_exists: ci.if_not_exists,
            concurrently: ci.concurrently,
            method,
            columns,
            options: convert_options(&ci.options),
        })
    }

    /// opclass の名前。修飾は `pg_catalog` だけ許す。ほかのスキーマには opclass がない。
    fn opclass_name(&self, oc: &ObjectName, method: &str) -> Result<String> {
        match oc.parts.as_slice() {
            [n] => Ok(n.value.clone()),
            [s, n] if s.value == "pg_catalog" => Ok(n.value.clone()),
            _ => Err(Error::new(
                sqlstate::UNDEFINED_OBJECT,
                format!(
                    "operator class \"{}\" does not exist for access method \"{method}\"",
                    display_name(oc)
                ),
            )
            .with_span(oc.span)),
        }
    }

    /// `DROP INDEX [CONCURRENTLY] [IF EXISTS] name [, ...] [CASCADE | RESTRICT]`。
    pub(super) fn analyze_drop_index(&self, di: &DropIndex) -> Result<BoundDropIndex> {
        let mut indexes: Vec<Arc<IndexDef>> = Vec::new();
        let mut missing = Vec::new();
        for n in &di.names {
            let wrong = |hint: &str| {
                Error::new(
                    sqlstate::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not an index", n.name().value),
                )
                .with_hint(hint)
            };
            let not_found = || {
                Error::new(
                    sqlstate::UNDEFINED_OBJECT,
                    format!("index \"{}\" does not exist", n.name().value),
                )
            };
            match self.lookup_relation(n)? {
                Some((_, RelKind::Table)) => {
                    return Err(wrong("Use DROP TABLE to remove a table."));
                }
                Some((_, RelKind::Sequence)) => {
                    return Err(wrong("Use DROP SEQUENCE to remove a sequence."));
                }
                Some((_, RelKind::Index)) => {
                    let schema = n.schema().map(|s| s.value.as_str());
                    let Some(idx) = self.catalog.index_by_name(schema, &n.name().value)? else {
                        return Err(not_found());
                    };
                    if !indexes.iter().any(|x| x.oid == idx.oid) {
                        indexes.push(idx);
                    }
                }
                None if di.if_exists => missing.push(n.name().value.clone()),
                None => return Err(not_found()),
            }
        }
        Ok(BoundDropIndex {
            indexes,
            missing,
            behavior: if di.cascade {
                DropBehavior::Cascade
            } else {
                DropBehavior::Restrict
            },
            concurrently: di.concurrently,
        })
    }

    /// `DROP TABLE [IF EXISTS] name [, ...] [CASCADE | RESTRICT]`。M2 の `analyze_drop_table`
    /// （`ddl.rs`）と違い、`behavior` を解決し、索引・シーケンスを `42809` にする。
    pub(super) fn analyze_drop_table_behavior(&self, dt: &DropTable) -> Result<BoundDropTable> {
        let mut tables: Vec<Arc<TableDef>> = Vec::new();
        let mut missing = Vec::new();
        for n in &dt.names {
            let wrong = |hint: &str| {
                Error::new(
                    sqlstate::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a table", n.name().value),
                )
                .with_hint(hint)
            };
            match self.lookup_relation(n)? {
                Some((_, RelKind::Index)) => {
                    return Err(wrong("Use DROP INDEX to remove an index."));
                }
                Some((_, RelKind::Sequence)) => {
                    return Err(wrong("Use DROP SEQUENCE to remove a sequence."));
                }
                Some((_, RelKind::Table)) => {
                    let t = self.relation_def(n)?;
                    if !tables.iter().any(|x| x.oid == t.oid) {
                        tables.push(t);
                    }
                }
                None if dt.if_exists => missing.push(n.name().value.clone()),
                None => {
                    return Err(Error::new(
                        sqlstate::UNDEFINED_TABLE,
                        format!("table \"{}\" does not exist", n.name().value),
                    ));
                }
            }
        }
        Ok(BoundDropTable {
            tables,
            missing,
            behavior: match dt.behavior {
                Some(ast::DropBehavior::Cascade) => DropBehavior::Cascade,
                _ => DropBehavior::Restrict,
            },
        })
    }

    /// `ALTER TABLE [IF EXISTS] [ONLY] name ADD ... | OWNER TO role`。
    pub(super) fn analyze_alter_table(&self, at: &AlterTable) -> Result<BoundAlterTable> {
        // 対応しない操作は、表の有無にかかわらず 0A000（表の検索より先）。
        if let AlterTableAction::Other { what, span } = &at.action {
            return Err(super::not_supported(
                &format!("ALTER TABLE ... {what}"),
                *span,
            ));
        }
        let target = match self.lookup_relation(&at.name)? {
            None if at.if_exists => AlterTarget::Missing(at.name.name().value.clone()),
            None => return Err(does_not_exist(&at.name)),
            Some((_, kind)) => {
                match (&at.action, kind) {
                    (AlterTableAction::AddConstraint(_), RelKind::Sequence | RelKind::Index) => {
                        return Err(Error::new(
                            sqlstate::WRONG_OBJECT_TYPE,
                            format!(
                                "ALTER action ADD CONSTRAINT cannot be performed on relation \"{}\"",
                                at.name.name().value
                            ),
                        )
                        .with_detail(not_supported_for(kind))
                        .with_span(at.name.span));
                    }
                    (AlterTableAction::OwnerTo(_), RelKind::Index) => {
                        return Err(Error::new(
                            sqlstate::WRONG_OBJECT_TYPE,
                            format!("cannot change owner of index \"{}\"", at.name.name().value),
                        )
                        .with_hint("Change the ownership of the index's table instead.")
                        .with_span(at.name.span));
                    }
                    _ => {}
                }
                AlterTarget::Found(self.relation_def(&at.name)?)
            }
        };
        match &at.action {
            AlterTableAction::AddConstraint(c) => {
                if let ast::TableConstraintKind::Check(e) = &c.kind {
                    return self
                        .analyze_add_check(target, c, e)
                        .map(BoundAlterTable::AddCheck);
                }
                let constraint = match &target {
                    AlterTarget::Found(t) => analyze_add_constraint(t, c)?,
                    AlterTarget::Missing(_) => placeholder_add_constraint(c)?,
                };
                Ok(BoundAlterTable::AddConstraint(
                    BoundAlterTableAddConstraint { target, constraint },
                ))
            }
            AlterTableAction::OwnerTo(role) => {
                let new_owner = match role {
                    RoleSpec::Name(n) => OwnerSpec::Name(n.value.clone()),
                    RoleSpec::CurrentUser | RoleSpec::CurrentRole => OwnerSpec::CurrentUser,
                    RoleSpec::SessionUser => OwnerSpec::SessionUser,
                    RoleSpec::Public => {
                        return Err(Error::new(
                            sqlstate::UNDEFINED_OBJECT,
                            "role \"public\" does not exist",
                        ));
                    }
                };
                Ok(BoundAlterTable::Owner(BoundAlterTableOwner {
                    target,
                    new_owner,
                }))
            }
            AlterTableAction::Other { .. } => unreachable!("handled above"),
        }
    }

    /// `TRUNCATE [TABLE] [ONLY] name [, ...] [RESTART | CONTINUE IDENTITY] [CASCADE | RESTRICT]`。
    pub(super) fn analyze_truncate(&self, t: &Truncate) -> Result<BoundTruncate> {
        let mut tables: Vec<Arc<TableDef>> = Vec::new();
        for n in &t.tables {
            match self.lookup_relation(n)? {
                None => {
                    return Err(Error::new(
                        sqlstate::UNDEFINED_TABLE,
                        format!("relation \"{}\" does not exist", display_name(n)),
                    ));
                }
                Some((_, RelKind::Table)) => {
                    let def = self.relation_def(n)?;
                    if !tables.iter().any(|x| x.oid == def.oid) {
                        tables.push(def);
                    }
                }
                Some(_) => {
                    return Err(Error::new(
                        sqlstate::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is not a table", n.name().value),
                    ));
                }
            }
        }
        Ok(BoundTruncate {
            tables,
            restart_identity: t.restart_identity,
            cascade: t.cascade,
        })
    }

    /// `VACUUM [(options)] [table [(cols)], ...]` と `ANALYZE ...`。オプションの名前・値の検査は実行時。
    pub(super) fn analyze_vacuum(&self, v: &Vacuum) -> Result<BoundVacuum> {
        let options: Vec<VacuumOption> = v
            .options
            .iter()
            .map(|o| VacuumOption {
                name: o.name.value.clone(),
                value: o.value.clone(),
            })
            .collect();
        // ANALYZE 文は常に分析する。VACUUM は `ANALYZE` オプションが真のとき。
        let analyze = !v.vacuum
            || options.iter().any(|o| {
                o.name == "analyze"
                    && o.value
                        .as_deref()
                        .is_none_or(|s| matches!(s.to_lowercase().as_str(), "true" | "on" | "1"))
            });
        let mut targets = Vec::with_capacity(v.targets.len());
        for t in &v.targets {
            let (table, name) = match self.lookup_relation(&t.name)? {
                None => return Err(does_not_exist(&t.name)),
                Some((_, RelKind::Table)) => (Some(self.relation_def(&t.name)?), t),
                // 索引・シーケンスは、実行時に WARNING を出して飛ばす。
                Some(_) => (None, t),
            };
            // 列リストがあるのに ANALYZE でない場合は、実行時が 0A000 にする（列は検査しない）。
            if let (Some(def), true) = (&table, analyze) {
                for c in &name.columns {
                    if def.column(&c.value).is_none() {
                        return Err(Error::new(
                            sqlstate::UNDEFINED_COLUMN,
                            format!(
                                "column \"{}\" of relation \"{}\" does not exist",
                                c.value, def.name
                            ),
                        )
                        .with_span(c.span));
                    }
                }
            }
            targets.push(VacuumTarget {
                name: name.name.name().value.clone(),
                table,
                columns: name.columns.iter().map(|c| c.value.clone()).collect(),
            });
        }
        Ok(BoundVacuum {
            vacuum: v.vacuum,
            analyze,
            options,
            targets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::bound::IndexConstraintKind;
    use crate::catalog::fake::{FakeCatalog, TableBuilder, default_sequence_params};
    use crate::sql::ast::Statement;
    use crate::types::SqlType;

    fn catalog() -> FakeCatalog {
        let mut c = FakeCatalog::new("postgres");
        c.add(
            &TableBuilder::new("t1")
                .column("a", SqlType::INT4)
                .column("b", SqlType::TEXT)
                .column("c", SqlType::INT4)
                .primary_key(&["a"])
                .index(Some("t1_b_idx"), &["b"], false),
        );
        c.add(&TableBuilder::new("t2").column("x", SqlType::INT4));
        c.add_sequence("ds1", default_sequence_params(SqlType::INT4, None));
        c
    }

    fn parse1(sql: &str) -> Statement {
        crate::sql::parse(sql).unwrap().remove(0)
    }

    fn create_index(c: &FakeCatalog, sql: &str) -> Result<BoundCreateIndex> {
        let Statement::CreateIndex(ci) = parse1(sql) else {
            panic!("not CREATE INDEX: {sql}");
        };
        Analyzer { catalog: c }.analyze_create_index(&ci)
    }

    fn drop_index(c: &FakeCatalog, sql: &str) -> Result<BoundDropIndex> {
        let Statement::DropIndex(d) = parse1(sql) else {
            panic!("not DROP INDEX: {sql}");
        };
        Analyzer { catalog: c }.analyze_drop_index(&d)
    }

    fn drop_table(c: &FakeCatalog, sql: &str) -> Result<BoundDropTable> {
        let Statement::DropTable(d) = parse1(sql) else {
            panic!("not DROP TABLE: {sql}");
        };
        Analyzer { catalog: c }.analyze_drop_table_behavior(&d)
    }

    fn alter(c: &FakeCatalog, sql: &str) -> Result<BoundAlterTable> {
        let Statement::AlterTable(a) = parse1(sql) else {
            panic!("not ALTER TABLE: {sql}");
        };
        Analyzer { catalog: c }.analyze_alter_table(&a)
    }

    fn truncate(c: &FakeCatalog, sql: &str) -> Result<BoundTruncate> {
        let Statement::Truncate(t) = parse1(sql) else {
            panic!("not TRUNCATE: {sql}");
        };
        Analyzer { catalog: c }.analyze_truncate(&t)
    }

    fn vacuum(c: &FakeCatalog, sql: &str) -> Result<BoundVacuum> {
        let Statement::Vacuum(v) = parse1(sql) else {
            panic!("not VACUUM: {sql}");
        };
        Analyzer { catalog: c }.analyze_vacuum(&v)
    }

    fn check<T: std::fmt::Debug>(r: Result<T>, state: &str, message: &str) -> Error {
        let e = r.expect_err(message);
        assert_eq!(e.sqlstate.code(), state, "{e:?}");
        assert_eq!(e.message, message);
        e
    }

    #[test]
    fn create_index_basic() {
        let c = catalog();
        let b = create_index(
            &c,
            "CREATE UNIQUE INDEX i1 ON t1 (a, b DESC, c NULLS FIRST)",
        )
        .unwrap();
        assert_eq!(b.table.name, "t1");
        assert_eq!(b.name.as_deref(), Some("i1"));
        assert!(b.unique && !b.if_not_exists && !b.concurrently);
        assert_eq!(b.method, "btree");
        let cols: Vec<_> = b
            .columns
            .iter()
            .map(|c| (c.attnum, c.descending, c.nulls_first))
            .collect();
        assert_eq!(
            cols,
            vec![(1, false, false), (2, true, true), (3, false, true)]
        );
        assert!(b.columns.iter().all(|c| c.opclass.is_none()));

        let b = create_index(
            &c,
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS i2 ON public.t1 USING BTREE (b DESC NULLS LAST, a text_ops) WITH (fillfactor = 70)",
        )
        .unwrap();
        assert!(b.concurrently && b.if_not_exists && !b.unique);
        assert_eq!(b.method, "btree");
        assert_eq!(
            (b.columns[0].descending, b.columns[0].nulls_first),
            (true, false)
        );
        assert_eq!(b.columns[1].opclass.as_deref(), Some("text_ops"));
        assert_eq!(b.options[0].name, "fillfactor");
        assert_eq!(b.options[0].value.as_deref(), Some("70"));

        // 名前なし、重複する列、pg_catalog 修飾の opclass、btree 以外のアクセスメソッド（実行時が判定）
        let b = create_index(
            &c,
            "CREATE INDEX ON t1 USING hash (a, a pg_catalog.int4_ops)",
        )
        .unwrap();
        assert_eq!(b.name, None);
        assert_eq!(b.method, "hash");
        assert_eq!(b.columns.len(), 2);
        assert_eq!(b.columns[1].opclass.as_deref(), Some("int4_ops"));
    }

    #[test]
    fn create_index_errors() {
        let c = catalog();
        let e = check(
            create_index(&c, "CREATE INDEX ON nosuch (a)"),
            "42P01",
            "relation \"nosuch\" does not exist",
        );
        assert!(e.cursor_byte.is_some());
        let e = check(
            create_index(&c, "CREATE INDEX ON t1 (a, zz)"),
            "42703",
            "column \"zz\" does not exist",
        );
        assert!(e.cursor_byte.is_some());
        let e = check(
            create_index(&c, "CREATE INDEX ON ds1 (last_value)"),
            "42809",
            "cannot create index on relation \"ds1\"",
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("This operation is not supported for sequences.")
        );
        let e = check(
            create_index(&c, "CREATE INDEX ON t1_pkey (a)"),
            "42809",
            "cannot open relation \"t1_pkey\"",
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("This operation is not supported for indexes.")
        );
        check(
            create_index(&c, "CREATE INDEX ON t1 (a nosuch.int4_ops)"),
            "42704",
            "operator class \"nosuch.int4_ops\" does not exist for access method \"btree\"",
        );
        check(
            create_index(&c, "CREATE INDEX ON t1 (ctid)"),
            "0A000",
            "index creation on system columns is not supported",
        );
        for (sql, msg) in [
            (
                "CREATE INDEX ON t1 ((a + 1))",
                "index expressions are not supported yet",
            ),
            (
                "CREATE INDEX ON t1 (a) WHERE a > 0",
                "partial indexes are not supported yet",
            ),
            (
                "CREATE INDEX ON t1 (a) INCLUDE (b)",
                "INCLUDE columns are not supported yet",
            ),
            (
                "CREATE UNIQUE INDEX ON t1 (a) NULLS NOT DISTINCT",
                "NULLS NOT DISTINCT is not supported yet",
            ),
            (
                "CREATE INDEX ON t1 (b COLLATE \"C\")",
                "collations are not supported yet",
            ),
            (
                "CREATE INDEX ON t1 (a) TABLESPACE pg_default",
                "TABLESPACE is not supported yet",
            ),
            (
                "CREATE INDEX ON t1 (lower(b))",
                "index expressions are not supported yet",
            ),
        ] {
            let e = check(create_index(&c, sql), "0A000", msg);
            assert!(e.cursor_byte.is_some(), "{sql}");
        }
    }

    #[test]
    fn drop_index_cases() {
        let c = catalog();
        let b = drop_index(&c, "DROP INDEX t1_b_idx, t1_pkey, t1_b_idx").unwrap();
        assert_eq!(
            b.indexes
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            vec!["t1_b_idx", "t1_pkey"]
        );
        assert!(b.missing.is_empty() && !b.concurrently);
        assert_eq!(b.behavior, DropBehavior::Restrict);

        let b = drop_index(
            &c,
            "DROP INDEX CONCURRENTLY IF EXISTS nosuch, t1_b_idx CASCADE",
        )
        .unwrap();
        assert_eq!(b.missing, vec!["nosuch"]);
        assert_eq!(b.indexes.len(), 1);
        assert!(b.concurrently);
        assert_eq!(b.behavior, DropBehavior::Cascade);

        check(
            drop_index(&c, "DROP INDEX nosuch"),
            "42704",
            "index \"nosuch\" does not exist",
        );
        let e = check(
            drop_index(&c, "DROP INDEX t1"),
            "42809",
            "\"t1\" is not an index",
        );
        assert_eq!(e.hint.as_deref(), Some("Use DROP TABLE to remove a table."));
        let e = check(
            drop_index(&c, "DROP INDEX ds1"),
            "42809",
            "\"ds1\" is not an index",
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("Use DROP SEQUENCE to remove a sequence.")
        );
    }

    #[test]
    fn drop_table_behavior() {
        let c = catalog();
        let b = drop_table(&c, "DROP TABLE t1, t2, t1").unwrap();
        assert_eq!(
            b.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );
        assert_eq!(b.behavior, DropBehavior::Restrict);
        let b = drop_table(&c, "DROP TABLE IF EXISTS nosuch, t2 CASCADE").unwrap();
        assert_eq!(b.missing, vec!["nosuch"]);
        assert_eq!(b.tables.len(), 1);
        assert_eq!(b.behavior, DropBehavior::Cascade);
        assert_eq!(
            drop_table(&c, "DROP TABLE t2 RESTRICT").unwrap().behavior,
            DropBehavior::Restrict
        );

        check(
            drop_table(&c, "DROP TABLE nosuch"),
            "42P01",
            "table \"nosuch\" does not exist",
        );
        let e = check(
            drop_table(&c, "DROP TABLE t1_pkey"),
            "42809",
            "\"t1_pkey\" is not a table",
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("Use DROP INDEX to remove an index.")
        );
        let e = check(
            drop_table(&c, "DROP TABLE ds1"),
            "42809",
            "\"ds1\" is not a table",
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("Use DROP SEQUENCE to remove a sequence.")
        );
    }

    #[test]
    fn alter_table_add_constraint() {
        let c = catalog();
        let BoundAlterTable::AddConstraint(b) = alter(
            &c,
            "ALTER TABLE t1 ADD CONSTRAINT u UNIQUE (b, c) WITH (fillfactor = 80)",
        )
        .unwrap() else {
            panic!()
        };
        let AlterTarget::Found(t) = &b.target else {
            panic!()
        };
        assert_eq!(t.name, "t1");
        assert_eq!(b.constraint.name.as_deref(), Some("u"));
        assert_eq!(b.constraint.kind, IndexConstraintKind::Unique);
        assert_eq!(b.constraint.columns, vec![2, 3]);
        assert_eq!(b.constraint.options.len(), 1);

        // ADD CHECK: 自動名（表名_列名_check）・明示名・表がないとき。
        let BoundAlterTable::AddCheck(b) = alter(&c, "ALTER TABLE t1 ADD CHECK (a > 0)").unwrap()
        else {
            panic!()
        };
        assert_eq!(b.name, "t1_a_check");
        assert_eq!(b.expr_sql, "a > 0");
        assert!(b.expr.is_some());
        let BoundAlterTable::AddCheck(b) = alter(
            &c,
            "ALTER TABLE t1 ADD CONSTRAINT mine CHECK (a > 0 AND c > 0)",
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(b.name, "mine");
        let BoundAlterTable::AddCheck(b) =
            alter(&c, "ALTER TABLE IF EXISTS nosuch ADD CHECK (a > 0)").unwrap()
        else {
            panic!()
        };
        assert!(matches!(&b.target, AlterTarget::Missing(_)) && b.expr.is_none());
        check(
            alter(&c, "ALTER TABLE t1 ADD CHECK (zz > 0)"),
            "42703",
            "column \"zz\" does not exist",
        );

        // IF EXISTS で表がない
        let BoundAlterTable::AddConstraint(b) =
            alter(&c, "ALTER TABLE IF EXISTS nosuch ADD PRIMARY KEY (a)").unwrap()
        else {
            panic!()
        };
        assert!(matches!(&b.target, AlterTarget::Missing(n) if n == "nosuch"));
        assert_eq!(b.constraint.kind, IndexConstraintKind::PrimaryKey);

        let e = check(
            alter(&c, "ALTER TABLE nosuch ADD PRIMARY KEY (a)"),
            "42P01",
            "relation \"nosuch\" does not exist",
        );
        assert!(e.cursor_byte.is_some());
        let e = check(
            alter(&c, "ALTER TABLE ds1 ADD UNIQUE (a)"),
            "42809",
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"ds1\"",
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("This operation is not supported for sequences.")
        );
        let e = check(
            alter(&c, "ALTER TABLE t1_pkey ADD UNIQUE (a)"),
            "42809",
            "ALTER action ADD CONSTRAINT cannot be performed on relation \"t1_pkey\"",
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("This operation is not supported for indexes.")
        );
        check(
            alter(&c, "ALTER TABLE t1 ADD UNIQUE (a, a)"),
            "42701",
            "column \"a\" appears twice in unique constraint",
        );
        check(
            alter(&c, "ALTER TABLE t1 ADD PRIMARY KEY (zz)"),
            "42703",
            "column \"zz\" of relation \"t1\" does not exist",
        );
        check(
            alter(&c, "ALTER TABLE t1 ADD UNIQUE (a) DEFERRABLE"),
            "0A000",
            "DEFERRABLE constraints are not supported yet",
        );
        check(
            alter(
                &c,
                "ALTER TABLE t1 ADD CONSTRAINT x UNIQUE USING INDEX t1_b_idx",
            ),
            "0A000",
            "ALTER TABLE ... ADD CONSTRAINT USING INDEX is not supported yet",
        );
    }

    #[test]
    fn alter_table_other_and_owner() {
        let c = catalog();
        let BoundAlterTable::Owner(b) = alter(&c, "ALTER TABLE t1 OWNER TO bob").unwrap() else {
            panic!()
        };
        assert_eq!(b.new_owner, OwnerSpec::Name("bob".into()));
        for (sql, owner) in [
            (
                "ALTER TABLE t1 OWNER TO CURRENT_USER",
                OwnerSpec::CurrentUser,
            ),
            (
                "ALTER TABLE t1 OWNER TO CURRENT_ROLE",
                OwnerSpec::CurrentUser,
            ),
            (
                "ALTER TABLE t1 OWNER TO SESSION_USER",
                OwnerSpec::SessionUser,
            ),
        ] {
            let BoundAlterTable::Owner(b) = alter(&c, sql).unwrap() else {
                panic!()
            };
            assert_eq!(b.new_owner, owner, "{sql}");
        }
        let BoundAlterTable::Owner(b) =
            alter(&c, "ALTER TABLE IF EXISTS nosuch OWNER TO bob").unwrap()
        else {
            panic!()
        };
        assert!(matches!(b.target, AlterTarget::Missing(_)));
        check(
            alter(&c, "ALTER TABLE t1 OWNER TO PUBLIC"),
            "42704",
            "role \"public\" does not exist",
        );
        let e = check(
            alter(&c, "ALTER TABLE t1_pkey OWNER TO bob"),
            "42809",
            "cannot change owner of index \"t1_pkey\"",
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("Change the ownership of the index's table instead.")
        );
        let e = alter(&c, "ALTER TABLE t1 DROP COLUMN b").unwrap_err();
        assert_eq!(e.sqlstate.code(), "0A000");
        assert!(e.message.starts_with("ALTER TABLE ... "), "{}", e.message);
        assert!(
            e.message.ends_with(" is not supported yet"),
            "{}",
            e.message
        );
    }

    #[test]
    fn truncate_cases() {
        let c = catalog();
        let b = truncate(&c, "TRUNCATE t1, t2, t1 RESTART IDENTITY CASCADE").unwrap();
        assert_eq!(
            b.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );
        assert!(b.restart_identity && b.cascade);
        let b = truncate(&c, "TRUNCATE TABLE ONLY t2").unwrap();
        assert!(!b.restart_identity && !b.cascade);

        check(
            truncate(&c, "TRUNCATE nosuch"),
            "42P01",
            "relation \"nosuch\" does not exist",
        );
        check(
            truncate(&c, "TRUNCATE t1_pkey"),
            "42809",
            "\"t1_pkey\" is not a table",
        );
        check(
            truncate(&c, "TRUNCATE t2, ds1"),
            "42809",
            "\"ds1\" is not a table",
        );
    }

    #[test]
    fn vacuum_cases() {
        let c = catalog();
        let b = vacuum(&c, "VACUUM").unwrap();
        assert!(b.vacuum && !b.analyze && b.options.is_empty() && b.targets.is_empty());
        let b = vacuum(&c, "VACUUM ANALYZE t1 (a, b), t1_pkey, ds1").unwrap();
        assert!(b.vacuum && b.analyze);
        assert_eq!(b.options.len(), 1);
        assert_eq!(b.options[0].name, "analyze");
        assert_eq!(b.targets.len(), 3);
        assert_eq!(b.targets[0].name, "t1");
        assert!(b.targets[0].table.is_some());
        assert_eq!(b.targets[0].columns, vec!["a", "b"]);
        assert!(b.targets[1].table.is_none() && b.targets[2].table.is_none());

        let b = vacuum(&c, "VACUUM (ANALYZE, FULL) t2").unwrap();
        assert!(b.analyze);
        assert_eq!(
            b.options
                .iter()
                .map(|o| o.name.as_str())
                .collect::<Vec<_>>(),
            vec!["analyze", "full"]
        );
        assert!(!vacuum(&c, "VACUUM (ANALYZE false) t2").unwrap().analyze);
        let b = vacuum(&c, "ANALYZE t1 (c)").unwrap();
        assert!(!b.vacuum && b.analyze);
        let b = vacuum(&c, "ANALYZE (VERBOSE)").unwrap();
        assert!(b.analyze && b.options[0].name == "verbose");
        // 未知のオプションと、ANALYZE なしの列リストは実行時の検査
        let b = vacuum(&c, "VACUUM (foo) t1").unwrap();
        assert_eq!(b.options[0].name, "foo");
        let b = vacuum(&c, "VACUUM t1 (zz)").unwrap();
        assert!(!b.analyze);
        assert_eq!(b.targets[0].columns, vec!["zz"]);

        check(
            vacuum(&c, "VACUUM nosuch"),
            "42P01",
            "relation \"nosuch\" does not exist",
        );
        check(
            vacuum(&c, "ANALYZE nosuch"),
            "42P01",
            "relation \"nosuch\" does not exist",
        );
        let e = check(
            vacuum(&c, "ANALYZE t1 (zz)"),
            "42703",
            "column \"zz\" of relation \"t1\" does not exist",
        );
        assert!(e.cursor_byte.is_some());
    }
}
