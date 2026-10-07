//! Test double for [`CatalogReader`]: an in-memory table list. It replaces
//! M1's `MemoryCatalog` in unit tests of the analyzer, planner and executor
//! until the real `StatementCatalog` exists. Not part of the product.
//!
//! M4: [`TableBuilder`] builds tables with indexes (PRIMARY KEY / UNIQUE /
//! plain), SERIAL / IDENTITY columns (with their sequences) so that the
//! analyzer, planner and executor tests (N1〜N3・L1・L2・X1〜X3) share one
//! fixture style. `fake.rs` has no owner after A (`m4/11-tests-plan.md` §4.2).

#![allow(clippy::too_many_lines)]

use std::collections::HashMap;
use std::sync::Arc;

use super::{
    BoundExprSource, BuiltinAggregate, CatalogReader, CheckDef, ColumnDef, ConstraintDef,
    ConstraintKind, IdentityKind, IndexColumn, IndexConstraintRef, IndexDef, RelKind,
    SequenceParams, TableDef,
};
use crate::error::Result;
use crate::storage::smgr::{DEFAULTTABLESPACE_OID, RelFileLocator, RelFileNumber};
use crate::types::{Oid, SqlType, oid};

/// `public` of database 5 (the namespace of every fake relation).
const PUBLIC_NAMESPACE: Oid = 2200;

#[derive(Debug)]
pub(crate) struct FakeCatalog {
    database: String,
    search_path: Vec<String>,
    tables: HashMap<Oid, Arc<TableDef>>,
    constraints: HashMap<Oid, ConstraintDef>,
    aggregates: Vec<&'static BuiltinAggregate>,
    next_oid: Oid,
}

impl FakeCatalog {
    pub(crate) fn new(database: impl Into<String>) -> Self {
        FakeCatalog {
            database: database.into(),
            search_path: vec!["public".into()],
            tables: HashMap::new(),
            constraints: HashMap::new(),
            aggregates: Vec::new(),
            next_oid: oid::FIRST_NORMAL_OBJECT_ID,
        }
    }

    pub(crate) fn allocate_oid(&mut self) -> Oid {
        let o = self.next_oid;
        self.next_oid += 1;
        o
    }

    pub(crate) fn put_table(&mut self, def: Arc<TableDef>) {
        self.tables.insert(def.oid, def);
    }

    pub(crate) fn remove_table(&mut self, oid: Oid) -> Option<Arc<TableDef>> {
        self.tables.remove(&oid)
    }

    /// Registers aggregate functions that `aggregates_named` returns in
    /// addition to the static table (`builtin::AGGREGATES`). Lets analyzer
    /// tests use aggregates before the static table is filled.
    #[allow(dead_code)]
    pub(crate) fn add_aggregates(&mut self, aggs: &[&'static BuiltinAggregate]) {
        self.aggregates.extend_from_slice(aggs);
    }

    /// Builds the table, its indexes (OIDs after the table's), its
    /// constraints and the sequences of SERIAL / IDENTITY columns, and
    /// registers them. Returns the table definition (also reachable via
    /// `table()`).
    #[allow(dead_code)]
    pub(crate) fn add(&mut self, b: &TableBuilder) -> Arc<TableDef> {
        let table_oid = self.allocate_oid();
        let mut columns: Vec<ColumnDef> = Vec::new();
        let mut identity_seqs = Vec::new();
        let mut serial_seqs = Vec::new();
        for (i, c) in b.columns.iter().enumerate() {
            let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
            let mut def = ColumnDef {
                name: c.name.clone(),
                attnum,
                ty: c.ty,
                not_null: c.not_null,
                default: c
                    .default
                    .clone()
                    .map(|expr_sql| BoundExprSource { expr_sql }),
                identity: c.identity,
            };
            if c.serial || c.identity.is_some() {
                let seq_name = format!("{}_{}_seq", b.name, c.name);
                let seq_oid = self.allocate_oid();
                let params = default_sequence_params(c.ty, Some((table_oid, attnum)));
                serial_seqs.push((seq_oid, seq_name.clone(), params));
                def.not_null = true;
                if c.serial {
                    def.default = Some(BoundExprSource {
                        expr_sql: format!("nextval('{seq_name}'::regclass)"),
                    });
                } else {
                    identity_seqs.push((attnum, seq_oid));
                }
            }
            columns.push(def);
        }

        let mut indexes: Vec<Arc<IndexDef>> = Vec::new();
        for spec in &b.indexes {
            let cols: Vec<IndexColumn> = spec
                .columns
                .iter()
                .map(|(name, descending, nulls_first)| {
                    let pos = columns
                        .iter()
                        .position(|c| &c.name == name)
                        .unwrap_or_else(|| {
                            panic!(
                                "fake catalog: unknown index column {name} in table {}",
                                b.name
                            )
                        });
                    let family = fake_opfamily(columns[pos].ty);
                    IndexColumn {
                        attnum: columns[pos].attnum,
                        opclass: 0,
                        opfamily: family,
                        descending: *descending,
                        nulls_first: *nulls_first,
                    }
                })
                .collect();
            if spec.primary {
                for c in &cols {
                    columns[usize::try_from(c.attnum - 1).unwrap_or(0)].not_null = true;
                }
            }
            let names: Vec<&str> = spec.columns.iter().map(|(n, _, _)| n.as_str()).collect();
            let index_oid = self.allocate_oid();
            let name = spec.name.clone().unwrap_or_else(|| {
                if spec.primary {
                    format!("{}_pkey", b.name)
                } else {
                    let suffix = if spec.unique { "key" } else { "idx" };
                    format!("{}_{}_{suffix}", b.name, names.join("_"))
                }
            });
            let constraint = (spec.primary || (spec.unique && spec.constraint)).then(|| {
                let con_oid = self.allocate_oid();
                let kind = if spec.primary {
                    ConstraintKind::PrimaryKey
                } else {
                    ConstraintKind::Unique
                };
                self.constraints.insert(
                    con_oid,
                    ConstraintDef {
                        oid: con_oid,
                        name: name.clone(),
                        namespace: PUBLIC_NAMESPACE,
                        kind,
                        table_oid,
                        index_oid: Some(index_oid),
                        columns: cols.iter().map(|c| c.attnum).collect(),
                        check_sql: None,
                        no_inherit: true,
                    },
                );
                IndexConstraintRef {
                    oid: con_oid,
                    name: name.clone(),
                }
            });
            indexes.push(Arc::new(IndexDef {
                oid: index_oid,
                name,
                namespace: PUBLIC_NAMESPACE,
                table_oid,
                locator: locator_of(index_oid),
                columns: cols,
                unique: spec.unique || spec.primary,
                primary: spec.primary,
                constraint,
            }));
        }
        indexes.sort_by_key(|i| i.oid);

        let def = Arc::new(TableDef {
            oid: table_oid,
            namespace: PUBLIC_NAMESPACE,
            schema: "public".into(),
            name: b.name.clone(),
            kind: RelKind::Table,
            locator: locator_of(table_oid),
            columns,
            checks: b.checks.clone(),
            indexes,
            sequence: None,
            identity_seqs,
        });
        self.put_table(Arc::clone(&def));
        for (seq_oid, seq_name, params) in serial_seqs {
            self.put_table(Arc::new(sequence_def(seq_oid, &seq_name, params)));
        }
        def
    }

    /// A stand-alone sequence (`relkind = S`): columns `last_value`,
    /// `log_cnt`, `is_called`.
    #[allow(dead_code)]
    pub(crate) fn add_sequence(&mut self, name: &str, params: SequenceParams) -> Arc<TableDef> {
        let seq_oid = self.allocate_oid();
        let def = Arc::new(sequence_def(seq_oid, name, params));
        self.put_table(Arc::clone(&def));
        def
    }

    fn find_index_by_name(&self, name: &str) -> Option<Arc<IndexDef>> {
        self.tables
            .values()
            .flat_map(|t| t.indexes.iter())
            .find(|i| i.name == name)
            .cloned()
    }
}

/// The default sequence parameters of a SERIAL / IDENTITY column of type
/// `ty` (`int2` / `int4` / `int8`; other types are treated as `int4`).
#[allow(dead_code)]
pub(crate) fn default_sequence_params(ty: SqlType, owned_by: Option<(Oid, i16)>) -> SequenceParams {
    let (type_oid, max) = match ty.oid {
        oid::INT2 => (oid::INT2, i64::from(i16::MAX)),
        oid::INT8 => (oid::INT8, i64::MAX),
        _ => (oid::INT4, i64::from(i32::MAX)),
    };
    SequenceParams {
        type_oid,
        start: 1,
        increment: 1,
        min: 1,
        max,
        cache: 1,
        cycle: false,
        owned_by,
    }
}

fn locator_of(oid: Oid) -> RelFileLocator {
    RelFileLocator {
        spc_oid: DEFAULTTABLESPACE_OID,
        db_oid: 5,
        rel_number: RelFileNumber(oid),
    }
}

/// `pg_opfamily` OIDs of the M4 operator families (`m4/11-tests-plan.md`
/// §7.2). The fake does not model operator class OIDs (0).
fn fake_opfamily(ty: SqlType) -> Oid {
    match ty.oid {
        oid::BOOL => 424,
        oid::BPCHAR => 426,
        oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ => 434,
        oid::FLOAT4 | oid::FLOAT8 => 1970,
        oid::INT2 | oid::INT4 | oid::INT8 => 1976,
        oid::NUMERIC => 1988,
        oid::OID => 1989,
        _ => 1994,
    }
}

fn sequence_def(seq_oid: Oid, name: &str, params: SequenceParams) -> TableDef {
    let col = |name: &str, attnum, ty| ColumnDef {
        name: name.into(),
        attnum,
        ty,
        not_null: true,
        default: None,
        identity: None,
    };
    TableDef {
        oid: seq_oid,
        namespace: PUBLIC_NAMESPACE,
        schema: "public".into(),
        name: name.into(),
        kind: RelKind::Sequence,
        locator: locator_of(seq_oid),
        columns: vec![
            col("last_value", 1, SqlType::INT8),
            col("log_cnt", 2, SqlType::INT8),
            col("is_called", 3, SqlType::BOOL),
        ],
        checks: vec![],
        indexes: vec![],
        sequence: Some(params),
        identity_seqs: vec![],
    }
}

#[derive(Debug, Clone)]
struct ColSpec {
    name: String,
    ty: SqlType,
    not_null: bool,
    default: Option<String>,
    identity: Option<IdentityKind>,
    serial: bool,
}

#[derive(Debug, Clone)]
struct IndexSpec {
    name: Option<String>,
    /// (column, descending, nulls first)
    columns: Vec<(String, bool, bool)>,
    unique: bool,
    primary: bool,
    /// UNIQUE owned by a constraint (`t_a_key`), as opposed to CREATE UNIQUE INDEX.
    constraint: bool,
}

/// Describes a table for [`FakeCatalog::add`].
#[derive(Debug, Clone)]
pub(crate) struct TableBuilder {
    name: String,
    columns: Vec<ColSpec>,
    indexes: Vec<IndexSpec>,
    checks: Vec<CheckDef>,
}

#[allow(dead_code)]
impl TableBuilder {
    pub(crate) fn new(name: &str) -> Self {
        TableBuilder {
            name: name.into(),
            columns: vec![],
            indexes: vec![],
            checks: vec![],
        }
    }

    /// A nullable column.
    pub(crate) fn column(mut self, name: &str, ty: SqlType) -> Self {
        self.columns.push(ColSpec {
            name: name.into(),
            ty,
            not_null: false,
            default: None,
            identity: None,
            serial: false,
        });
        self
    }

    /// A NOT NULL column.
    pub(crate) fn column_nn(mut self, name: &str, ty: SqlType) -> Self {
        self = self.column(name, ty);
        if let Some(c) = self.columns.last_mut() {
            c.not_null = true;
        }
        self
    }

    /// Sets the DEFAULT (SQL text) of the column `name`.
    pub(crate) fn default_sql(mut self, name: &str, sql: &str) -> Self {
        if let Some(c) = self.columns.iter_mut().find(|c| c.name == name) {
            c.default = Some(sql.into());
        }
        self
    }

    /// `name serial`-like: NOT NULL, `DEFAULT nextval('t_name_seq'::regclass)` and an owned sequence.
    pub(crate) fn serial(mut self, name: &str, ty: SqlType) -> Self {
        self = self.column_nn(name, ty);
        if let Some(c) = self.columns.last_mut() {
            c.serial = true;
        }
        self
    }

    /// `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY` with an owned sequence.
    pub(crate) fn identity(mut self, name: &str, ty: SqlType, kind: IdentityKind) -> Self {
        self = self.column_nn(name, ty);
        if let Some(c) = self.columns.last_mut() {
            c.identity = Some(kind);
        }
        self
    }

    pub(crate) fn check(mut self, name: &str, expr_sql: &str) -> Self {
        self.checks.push(CheckDef {
            name: name.into(),
            expr_sql: expr_sql.into(),
            no_inherit: false,
        });
        self
    }

    /// `PRIMARY KEY (cols)`: the index `t_pkey` owned by a constraint; the columns become NOT NULL.
    pub(crate) fn primary_key(mut self, cols: &[&str]) -> Self {
        self.indexes.push(IndexSpec {
            name: None,
            columns: cols
                .iter()
                .map(|c| ((*c).to_owned(), false, false))
                .collect(),
            unique: true,
            primary: true,
            constraint: true,
        });
        self
    }

    /// `UNIQUE (cols)`: the index `t_a_b_key` owned by a constraint.
    pub(crate) fn unique(mut self, cols: &[&str]) -> Self {
        self.indexes.push(IndexSpec {
            name: None,
            columns: cols
                .iter()
                .map(|c| ((*c).to_owned(), false, false))
                .collect(),
            unique: true,
            primary: false,
            constraint: true,
        });
        self
    }

    /// `CREATE [UNIQUE] INDEX name ON t (cols)` (ascending, NULLS LAST).
    pub(crate) fn index(mut self, name: Option<&str>, cols: &[&str], unique: bool) -> Self {
        self.indexes.push(IndexSpec {
            name: name.map(str::to_owned),
            columns: cols
                .iter()
                .map(|c| ((*c).to_owned(), false, false))
                .collect(),
            unique,
            primary: false,
            constraint: false,
        });
        self
    }

    /// Like [`TableBuilder::index`] with per-column `(name, descending, nulls_first)`.
    pub(crate) fn index_ordered(
        mut self,
        name: Option<&str>,
        cols: &[(&str, bool, bool)],
        unique: bool,
    ) -> Self {
        self.indexes.push(IndexSpec {
            name: name.map(str::to_owned),
            columns: cols
                .iter()
                .map(|(c, d, n)| ((*c).to_owned(), *d, *n))
                .collect(),
            unique,
            primary: false,
            constraint: false,
        });
        self
    }
}

/// A `TableDef` in `public` of database 5 with the file named after the OID.
pub(crate) fn table_def(
    oid: Oid,
    name: &str,
    columns: Vec<super::ColumnDef>,
    checks: Vec<super::CheckDef>,
) -> TableDef {
    TableDef {
        oid,
        namespace: PUBLIC_NAMESPACE,
        schema: "public".into(),
        name: name.into(),
        kind: RelKind::Table,
        locator: locator_of(oid),
        columns,
        checks,
        indexes: Vec::new(),
        sequence: None,
        identity_seqs: Vec::new(),
    }
}

impl CatalogReader for FakeCatalog {
    /// Tables and sequences (not indexes), like the real catalog.
    fn table(&self, schema: Option<&str>, name: &str) -> Result<Option<Arc<TableDef>>> {
        let schema = schema.unwrap_or("public");
        Ok(self
            .tables
            .values()
            .find(|t| t.schema == schema && t.name == name)
            .cloned())
    }

    fn table_by_oid(&self, oid: Oid) -> Result<Option<Arc<TableDef>>> {
        Ok(self.tables.get(&oid).cloned())
    }

    fn current_database(&self) -> &str {
        &self.database
    }

    fn search_path(&self) -> &[String] {
        &self.search_path
    }

    fn role_name(&self, _oid: Oid) -> Result<Option<String>> {
        Ok(None)
    }

    fn visible_namespaces(&self) -> Result<Vec<Oid>> {
        Ok(vec![11, PUBLIC_NAMESPACE])
    }

    fn relation_kind(&self, schema: Option<&str>, name: &str) -> Result<Option<(Oid, RelKind)>> {
        if let Some(t) = self.table(schema, name)? {
            return Ok(Some((t.oid, t.kind)));
        }
        if schema.is_none_or(|s| s == "public") {
            return Ok(self
                .find_index_by_name(name)
                .map(|i| (i.oid, RelKind::Index)));
        }
        Ok(None)
    }

    fn index_by_name(&self, schema: Option<&str>, name: &str) -> Result<Option<Arc<IndexDef>>> {
        if schema.is_none_or(|s| s == "public") {
            return Ok(self.find_index_by_name(name));
        }
        Ok(None)
    }

    fn index_by_oid(&self, oid: Oid) -> Result<Option<Arc<IndexDef>>> {
        Ok(self
            .tables
            .values()
            .flat_map(|t| t.indexes.iter())
            .find(|i| i.oid == oid)
            .cloned())
    }

    fn relation_name(&self, oid: Oid) -> Result<Option<String>> {
        if let Some(t) = self.tables.get(&oid) {
            return Ok(Some(t.name.clone()));
        }
        Ok(self.index_by_oid(oid)?.map(|i| i.name.clone()))
    }

    fn constraint_by_oid(&self, oid: Oid) -> Result<Option<ConstraintDef>> {
        Ok(self.constraints.get(&oid).cloned())
    }

    fn aggregates_named(&self, name: &str) -> Vec<&'static BuiltinAggregate> {
        let mut v = super::builtin::aggregates_named(name);
        v.extend(self.aggregates.iter().copied().filter(|a| a.name == name));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{AggKind, ColumnDef};
    use crate::storage::RelHandle;
    use crate::types::SqlType;

    #[test]
    fn put_lookup_remove() {
        let mut c = FakeCatalog::new("postgres");
        let oid = c.allocate_oid();
        assert_eq!(oid, 16384);
        let col = ColumnDef {
            name: "a".into(),
            attnum: 1,
            ty: SqlType::INT4,
            not_null: false,
            default: None,
            identity: None,
        };
        c.put_table(Arc::new(table_def(oid, "t", vec![col], vec![])));
        assert_eq!(c.table(None, "t").unwrap().unwrap().oid, oid);
        assert!(c.table(Some("other"), "t").unwrap().is_none());
        assert_eq!(
            c.table_by_oid(oid).unwrap().unwrap().column_index("a"),
            Some(0)
        );
        assert_eq!(c.type_by_name("text").unwrap().oid, 25);
        assert_eq!(c.current_database(), "postgres");
        assert!(c.remove_table(oid).is_some());
        assert!(c.table(None, "t").unwrap().is_none());
    }

    #[test]
    fn builder_makes_indexes_constraints_and_sequences() {
        let mut c = FakeCatalog::new("postgres");
        let t = c.add(
            &TableBuilder::new("t")
                .serial("id", SqlType::INT4)
                .column("a", SqlType::INT4)
                .column("b", SqlType::TEXT)
                .identity("n", SqlType::INT8, IdentityKind::Always)
                .default_sql("b", "'x'::text")
                .check("t_a_check", "a > 0")
                .primary_key(&["id"])
                .unique(&["a", "b"])
                .index(None, &["b"], false)
                .index_ordered(Some("t_a_desc"), &[("a", true, true)], true),
        );
        // OID order: table, serial seq, identity seq, then indexes.
        assert_eq!(t.indexes.len(), 4);
        assert!(t.indexes.windows(2).all(|w| w[0].oid < w[1].oid));
        let pk = t.primary_key().unwrap();
        assert_eq!(pk.name, "t_pkey");
        assert!(pk.unique && pk.primary && pk.is_constraint_index());
        assert!(t.columns[0].not_null && t.columns[3].not_null);
        assert_eq!(
            t.columns[0].default.as_ref().unwrap().expr_sql,
            "nextval('t_id_seq'::regclass)"
        );
        assert_eq!(t.columns[3].identity, Some(IdentityKind::Always));
        assert_eq!(t.identity_seqs.len(), 1);
        assert_eq!(t.identity_seqs[0].0, 4);
        assert!(t.index_by_name("t_a_b_key").unwrap().unique);
        let plain = t.index_by_name("t_b_idx").unwrap();
        assert!(!plain.unique && !plain.is_constraint_index());
        let desc = t.index_by_name("t_a_desc").unwrap();
        assert_eq!(
            (desc.indoption(0), IndexDef::flags_from_indoption(3)),
            (3, (true, true))
        );
        assert_eq!(t.checks.len(), 1);

        // Sequences are tables of kind S and carry their parameters.
        let seq = c.table(None, "t_id_seq").unwrap().unwrap();
        assert_eq!(seq.kind, RelKind::Sequence);
        assert_eq!(seq.sequence.unwrap().owned_by, Some((t.oid, 1)));
        assert_eq!(seq.sequence.unwrap().max, i64::from(i32::MAX));
        let seq8 = c.table(None, "t_n_seq").unwrap().unwrap();
        assert_eq!(seq8.sequence.unwrap().max, i64::MAX);
        assert_eq!(seq8.columns.len(), 3);

        // RelHandle gets the indexes with their key columns.
        let h = RelHandle::from_table(&t);
        assert_eq!(h.indexes.len(), 4);
        let ih = h.indexes.iter().find(|i| i.name == "t_a_b_key").unwrap();
        assert_eq!(ih.table_name, "t");
        assert_eq!(
            ih.columns
                .iter()
                .map(|k| k.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(ih.columns[1].ty, SqlType::TEXT);

        // Names are shared between tables, sequences and indexes.
        assert_eq!(
            c.relation_kind(None, "t_pkey").unwrap(),
            Some((pk.oid, RelKind::Index))
        );
        assert_eq!(
            c.relation_kind(None, "t").unwrap().unwrap().1,
            RelKind::Table
        );
        assert_eq!(
            c.relation_kind(None, "t_id_seq").unwrap().unwrap().1,
            RelKind::Sequence
        );
        assert!(c.table(None, "t_pkey").unwrap().is_none());
        assert_eq!(c.index_by_oid(pk.oid).unwrap().unwrap().name, "t_pkey");
        assert_eq!(
            c.index_by_name(None, "t_pkey").unwrap().unwrap().oid,
            pk.oid
        );
        assert_eq!(c.relation_name(pk.oid).unwrap().as_deref(), Some("t_pkey"));
        let con = c
            .constraint_by_oid(pk.constraint.as_ref().unwrap().oid)
            .unwrap()
            .unwrap();
        assert_eq!(
            (con.kind, con.index_oid, con.columns.as_slice()),
            (ConstraintKind::PrimaryKey, Some(pk.oid), &[1i16][..])
        );

        let s = c.add_sequence("s", default_sequence_params(SqlType::INT2, None));
        assert_eq!(s.sequence.unwrap().max, i64::from(i16::MAX));
    }

    static COUNT_STAR: BuiltinAggregate = BuiltinAggregate {
        oid: 2803,
        name: "test_count",
        args: &[],
        result: oid::INT8,
        kind: AggKind::CountStar,
    };

    #[test]
    fn registered_aggregates_are_found() {
        let mut c = FakeCatalog::new("postgres");
        // 静的な表（builtin::AGGREGATES）の集約は登録なしで見つかる。
        assert!(!c.aggregates_named("count").is_empty());
        assert!(c.aggregates_named("test_count").is_empty());
        c.add_aggregates(&[&COUNT_STAR]);
        let found = c.aggregates_named("test_count");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, AggKind::CountStar);
        assert!(c.aggregates_named("no_such_agg").is_empty());
    }
}
