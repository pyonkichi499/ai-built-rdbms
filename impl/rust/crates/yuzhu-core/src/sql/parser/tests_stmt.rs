//! Parser tests for the M4 statements of chapters 07 (indexes, ALTER TABLE,
//! TRUNCATE, VACUUM), 08 (sequences, IDENTITY, OVERRIDING), 09 (type names,
//! datetime keywords) and 10 (EXPLAIN options, COPY).

#![allow(clippy::many_single_char_names, clippy::too_many_lines)]

use super::parse;
use crate::error::{Error, sqlstate};
use crate::sql::ast::{
    AlterSequenceAction, AlterTableAction, ColumnConstraintKind, CopyDirection, CopyOptionValue,
    CopySource, ExplainValue, Expr, GeneratedWhen, IndexElemKind, NullsOrder, OverridingKind,
    RoleSpec, SeqOptionKind, SeqPersistence, SessionValueKind, SortDirection, Statement,
    TableConstraintKind, TableElement,
};

fn one(sql: &str) -> Statement {
    let mut v = parse(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    assert_eq!(v.len(), 1, "{sql}");
    v.remove(0)
}

fn err(sql: &str) -> Error {
    match parse(sql) {
        Err(e) => e,
        Ok(v) => panic!("expected an error for {sql:?}, got {v:?}"),
    }
}

fn syntax(sql: &str, near: &str, pos: u32) {
    let e = err(sql);
    assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR, "{sql}");
    let expected = if near.is_empty() {
        "syntax error at end of input".to_string()
    } else {
        format!("syntax error at or near \"{near}\"")
    };
    assert_eq!(e.message, expected, "{sql}");
    assert_eq!(e.cursor_byte, Some(pos), "{sql}");
}

fn unsupported(sql: &str, msg: &str, pos: u32) {
    let e = err(sql);
    assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED, "{sql}");
    assert_eq!(e.message, msg, "{sql}");
    assert_eq!(e.cursor_byte, Some(pos), "{sql}");
}

fn create_table(sql: &str) -> crate::sql::ast::CreateTable {
    match one(sql) {
        Statement::CreateTable(c) => c,
        s => panic!("{s:?}"),
    }
}

fn create_index(sql: &str) -> crate::sql::ast::CreateIndex {
    match one(sql) {
        Statement::CreateIndex(c) => c,
        s => panic!("{s:?}"),
    }
}

fn seq_kinds(opts: &[crate::sql::ast::SeqOption]) -> Vec<String> {
    opts.iter()
        .map(|o| match &o.kind {
            SeqOptionKind::As(t) => format!("as {}", t.names[0].value),
            SeqOptionKind::Increment(n) => format!("inc {}", n.text),
            SeqOptionKind::MinValue(n) => format!("min {:?}", n.as_ref().map(|n| &n.text)),
            SeqOptionKind::MaxValue(n) => format!("max {:?}", n.as_ref().map(|n| &n.text)),
            SeqOptionKind::Start(n) => format!("start {}", n.text),
            SeqOptionKind::Restart(n) => format!("restart {:?}", n.as_ref().map(|n| &n.text)),
            SeqOptionKind::Cache(n) => format!("cache {}", n.text),
            SeqOptionKind::Cycle(b) => format!("cycle {b}"),
            SeqOptionKind::OwnedBy(n) => format!(
                "owned {}",
                n.parts
                    .iter()
                    .map(|p| p.value.as_str())
                    .collect::<Vec<_>>()
                    .join(".")
            ),
            SeqOptionKind::SequenceName(n) => format!("name {}", n.name().value),
        })
        .collect()
}

// ----- 07: indexes ---------------------------------------------------------

#[test]
fn create_index_basic() {
    let c = create_index(
        "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS i ON ONLY s.t USING btree (a, b DESC NULLS LAST, c ASC NULLS FIRST)",
    );
    assert!(c.unique && c.concurrently && c.if_not_exists);
    assert_eq!(c.name.unwrap().value, "i");
    assert_eq!(c.table.parts.len(), 2);
    assert_eq!(c.method.unwrap().value, "btree");
    assert_eq!(c.columns.len(), 3);
    assert_eq!(c.columns[0].direction, None);
    assert_eq!(c.columns[1].direction, Some(SortDirection::Desc));
    assert_eq!(c.columns[1].nulls, Some(NullsOrder::Last));
    assert_eq!(c.columns[2].direction, Some(SortDirection::Asc));
    assert_eq!(c.columns[2].nulls, Some(NullsOrder::First));
    assert!(matches!(&c.columns[0].kind, IndexElemKind::Column(i) if i.value == "a"));
}

#[test]
fn create_index_unnamed_and_options() {
    let c = create_index(
        "create index on t (a text_pattern_ops, b COLLATE \"C\") include (x) with (fillfactor = 70, deduplicate_items) tablespace ts where a > 1",
    );
    assert!(c.name.is_none() && !c.unique);
    assert_eq!(
        c.columns[0].opclass.as_ref().unwrap().name().value,
        "text_pattern_ops"
    );
    assert_eq!(c.columns[1].collation.as_ref().unwrap().name().value, "C");
    assert_eq!(c.options.len(), 2);
    assert_eq!(c.options[0].name.value, "fillfactor");
    assert_eq!(c.options[0].value.as_deref(), Some("70"));
    assert_eq!(c.options[1].value, None);
    assert_eq!(c.include.len(), 1);
    assert_eq!(c.tablespace.unwrap().value, "ts");
    assert!(c.where_clause.is_some());
    let c = create_index("CREATE INDEX i ON t (a) NULLS NOT DISTINCT WITH (fillfactor='50')");
    assert!(c.nulls_not_distinct);
    assert_eq!(c.options[0].value.as_deref(), Some("50"));
}

#[test]
fn create_index_expressions_and_nulls_opclass() {
    let c = create_index("CREATE INDEX i ON t (lower(a), (a + 1), a nulls first)");
    assert!(matches!(
        c.columns[0].kind,
        IndexElemKind::Expr(Expr::Function { .. })
    ));
    assert!(matches!(
        c.columns[1].kind,
        IndexElemKind::Expr(Expr::BinaryOp { .. })
    ));
    assert!(c.columns[2].opclass.is_none());
    assert_eq!(c.columns[2].nulls, Some(NullsOrder::First));
}

#[test]
fn create_index_errors() {
    syntax("CREATE INDEX IF NOT EXISTS ON t (a)", "ON", 27);
    syntax("CREATE INDEX i ON t (a + 1)", "+", 23);
    syntax("CREATE INDEX i ON t ()", ")", 21);
    syntax("CREATE INDEX i ON t (a", "", 22);
    syntax("CREATE INDEX i t (a)", "t", 15);
    unsupported(
        "CREATE INDEX i ON t (a text_ops (x=1))",
        "operator class parameters is not supported yet",
        32,
    );
}

#[test]
fn drop_index_and_table_behavior() {
    let Statement::DropIndex(d) = one("DROP INDEX CONCURRENTLY IF EXISTS a, s.b CASCADE") else {
        panic!()
    };
    assert!(d.concurrently && d.if_exists && d.cascade);
    assert_eq!(d.names.len(), 2);
    let Statement::DropIndex(d) = one("DROP INDEX i RESTRICT") else {
        panic!()
    };
    assert!(!d.cascade);
    let Statement::DropTable(d) = one("DROP TABLE t CASCADE") else {
        panic!()
    };
    assert_eq!(d.behavior, Some(crate::sql::ast::DropBehavior::Cascade));
    syntax("DROP INDEX", "", 10);
}

#[test]
fn create_table_with_options_and_keys() {
    let c = create_table(
        "CREATE TABLE t (a int PRIMARY KEY WITH (fillfactor=80), b int UNIQUE, \
         c int, d int, CONSTRAINT u UNIQUE NULLS NOT DISTINCT (c, d) INCLUDE (b) \
         WITH (fillfactor=70) USING INDEX TABLESPACE ts, PRIMARY KEY (c) DEFERRABLE) WITH (fillfactor = 90)",
    );
    assert_eq!(c.options.len(), 1);
    assert_eq!(c.options[0].value.as_deref(), Some("90"));
    let TableElement::Column(a) = &c.elements[0] else {
        panic!()
    };
    let ColumnConstraintKind::PrimaryKey(p) = &a.constraints[0].kind else {
        panic!()
    };
    assert_eq!(p.options[0].name.value, "fillfactor");
    let TableElement::Constraint(u) = &c.elements[4] else {
        panic!()
    };
    let TableConstraintKind::Unique(k) = &u.kind else {
        panic!()
    };
    assert_eq!(k.columns.len(), 2);
    assert!(k.params.nulls_not_distinct);
    assert_eq!(k.params.include.len(), 1);
    assert_eq!(k.params.tablespace.as_ref().unwrap().value, "ts");
    assert!(!u.deferrable);
    let TableElement::Constraint(pk) = &c.elements[5] else {
        panic!()
    };
    assert!(pk.deferrable);
    let c =
        create_table("CREATE TABLE t (a int UNIQUE DEFERRABLE INITIALLY DEFERRED, b int NOT NULL)");
    let TableElement::Column(a) = &c.elements[0] else {
        panic!()
    };
    assert!(a.constraints[0].deferrable);
    let TableElement::Column(b) = &c.elements[1] else {
        panic!()
    };
    assert!(!b.constraints[0].deferrable);
    let c = create_table("CREATE TABLE t (a int)");
    assert!(c.options.is_empty());
    unsupported(
        "CREATE TABLE t (a int) WITH OIDS",
        "CREATE TABLE ... WITH is not supported yet",
        23,
    );
}

// ----- 07: ALTER TABLE / TRUNCATE / VACUUM ---------------------------------

#[test]
fn alter_table() {
    let Statement::AlterTable(a) = one(
        "ALTER TABLE IF EXISTS ONLY s.t ADD CONSTRAINT k PRIMARY KEY (a, b) WITH (fillfactor=60)",
    ) else {
        panic!()
    };
    assert!(a.if_exists && a.only);
    let AlterTableAction::AddConstraint(c) = a.action else {
        panic!()
    };
    assert_eq!(c.name.unwrap().value, "k");
    assert!(matches!(c.kind, TableConstraintKind::PrimaryKey(k) if k.columns.len() == 2));
    let Statement::AlterTable(a) = one("ALTER TABLE t ADD UNIQUE USING INDEX ix") else {
        panic!()
    };
    let AlterTableAction::AddConstraint(c) = a.action else {
        panic!()
    };
    let TableConstraintKind::Unique(k) = c.kind else {
        panic!()
    };
    assert_eq!(k.params.using_index.unwrap().value, "ix");
    let Statement::AlterTable(a) = one("ALTER TABLE t OWNER TO CURRENT_USER") else {
        panic!()
    };
    assert_eq!(a.action, AlterTableAction::OwnerTo(RoleSpec::CurrentUser));
    let Statement::AlterTable(a) = one("ALTER TABLE t OWNER TO bob;") else {
        panic!()
    };
    assert!(matches!(a.action, AlterTableAction::OwnerTo(RoleSpec::Name(i)) if i.value == "bob"));
    let Statement::AlterTable(a) = one("ALTER TABLE t OWNER TO public") else {
        panic!()
    };
    assert_eq!(a.action, AlterTableAction::OwnerTo(RoleSpec::Public));
    for (sql, what) in [
        ("ALTER TABLE t ADD COLUMN b int", "ADD COLUMN"),
        ("ALTER TABLE t DROP CONSTRAINT c", "DROP CONSTRAINT"),
        ("ALTER TABLE t RENAME TO u", "RENAME"),
    ] {
        let Statement::AlterTable(a) = one(sql) else {
            panic!()
        };
        assert!(
            matches!(a.action, AlterTableAction::Other { what: w, .. } if w == what),
            "{sql}"
        );
    }
}

#[test]
fn alter_table_errors() {
    let e = err("ALTER TABLE t OWNER TO none");
    assert_eq!(e.sqlstate, sqlstate::RESERVED_NAME);
    assert_eq!(e.message, "role name \"none\" is reserved");
    assert_eq!(e.cursor_byte, Some(23));
    syntax("ALTER TABLE t FOO", "FOO", 14);
    syntax("ALTER TABLE t OWNER bob", "bob", 20);
    unsupported(
        "ALTER TABLE t ADD PRIMARY KEY (a), ADD UNIQUE (b)",
        "multiple ALTER TABLE actions is not supported yet",
        33,
    );
    unsupported(
        "ALTER INDEX i RENAME TO j",
        "ALTER INDEX is not supported yet",
        6,
    );
}

#[test]
fn truncate() {
    let Statement::Truncate(t) = one("TRUNCATE TABLE ONLY a, b * RESTART IDENTITY CASCADE") else {
        panic!()
    };
    assert_eq!(t.tables.len(), 2);
    assert!(t.only && t.restart_identity && t.cascade);
    let Statement::Truncate(t) = one("truncate t continue identity restrict") else {
        panic!()
    };
    assert!(!t.restart_identity && !t.cascade && !t.only);
    syntax("TRUNCATE", "", 8);
    syntax("TRUNCATE t RESTART", "", 18);
}

#[test]
fn vacuum_and_analyze() {
    let Statement::Vacuum(v) = one("VACUUM") else {
        panic!()
    };
    assert!(v.vacuum && v.options.is_empty() && v.targets.is_empty());
    let Statement::Vacuum(v) = one("VACUUM FULL FREEZE VERBOSE ANALYZE a, b (x, y)") else {
        panic!()
    };
    let names: Vec<_> = v.options.iter().map(|o| o.name.value.as_str()).collect();
    assert_eq!(names, ["full", "freeze", "verbose", "analyze"]);
    assert_eq!(v.targets.len(), 2);
    assert_eq!(v.targets[1].columns.len(), 2);
    let Statement::Vacuum(v) =
        one("VACUUM (ANALYZE, verbose false, parallel 2, index_cleanup auto) t")
    else {
        panic!()
    };
    assert_eq!(v.options.len(), 4);
    assert_eq!(v.options[0].name.value, "analyze");
    assert_eq!(v.options[0].value, None);
    assert_eq!(v.options[1].value.as_deref(), Some("false"));
    assert_eq!(v.options[2].value.as_deref(), Some("2"));
    let Statement::Vacuum(v) = one("ANALYSE VERBOSE t (a)") else {
        panic!()
    };
    assert!(!v.vacuum);
    assert_eq!(v.options[0].name.value, "verbose");
    let Statement::Vacuum(v) = one("ANALYZE") else {
        panic!()
    };
    assert!(!v.vacuum);
    syntax("VACUUM (", "", 8);
    syntax("ANALYZE FULL", "FULL", 8);
}

// ----- 08: sequences -------------------------------------------------------

#[test]
fn create_sequence() {
    let Statement::CreateSequence(s) = one(
        "CREATE SEQUENCE IF NOT EXISTS s.q AS smallint INCREMENT BY -2 MINVALUE 1 MAXVALUE +10 \
         START WITH 5 CACHE 3 NO CYCLE OWNED BY t.c",
    ) else {
        panic!()
    };
    assert!(s.if_not_exists);
    assert_eq!(s.persistence, SeqPersistence::Permanent);
    assert_eq!(
        seq_kinds(&s.options),
        [
            "as int2",
            "inc -2",
            "min Some(\"1\")",
            "max Some(\"10\")",
            "start 5",
            "cache 3",
            "cycle false",
            "owned t.c"
        ]
    );
    let Statement::CreateSequence(s) = one(
        "CREATE TEMP SEQUENCE q NO MINVALUE NO MAXVALUE CYCLE RESTART SEQUENCE NAME x OWNED BY NONE",
    ) else {
        panic!()
    };
    assert_eq!(s.persistence, SeqPersistence::Temporary);
    assert_eq!(
        seq_kinds(&s.options),
        [
            "min None",
            "max None",
            "cycle true",
            "restart None",
            "name x",
            "owned none"
        ]
    );
    let Statement::CreateSequence(s) = one("CREATE UNLOGGED SEQUENCE q START 1.5") else {
        panic!()
    };
    assert_eq!(s.persistence, SeqPersistence::Unlogged);
    assert_eq!(seq_kinds(&s.options), ["start 1.5"]);
    let Statement::CreateSequence(s) = one("CREATE SEQUENCE q") else {
        panic!()
    };
    assert!(s.options.is_empty());
    let Statement::CreateSequence(s) = one("create sequence q restart with 7 restart 8") else {
        panic!()
    };
    assert_eq!(
        seq_kinds(&s.options),
        ["restart Some(\"7\")", "restart Some(\"8\")"]
    );
}

#[test]
fn create_sequence_errors() {
    syntax("CREATE SEQUENCE q INCREMENT", "", 27);
    syntax("CREATE SEQUENCE q INCREMENT BY x", "x", 31);
    syntax("CREATE SEQUENCE q NO FOO", "FOO", 21);
    syntax("CREATE SEQUENCE q CACHE 1 2", "2", 26);
    syntax("CREATE SEQUENCE q SEQUENCE x", "x", 27);
    unsupported(
        "CREATE TEMP TABLE t (a int)",
        "temporary tables is not supported yet",
        7,
    );
}

#[test]
fn alter_and_drop_sequence() {
    let Statement::AlterSequence(a) = one("ALTER SEQUENCE IF EXISTS q INCREMENT 5 RESTART") else {
        panic!()
    };
    assert!(a.if_exists);
    let AlterSequenceAction::Options(o) = a.action else {
        panic!()
    };
    assert_eq!(seq_kinds(&o), ["inc 5", "restart None"]);
    let Statement::AlterSequence(a) = one("ALTER SEQUENCE q OWNER TO bob") else {
        panic!()
    };
    assert!(matches!(
        a.action,
        AlterSequenceAction::OwnerTo(RoleSpec::Name(_))
    ));
    let Statement::AlterSequence(a) = one("ALTER SEQUENCE q RENAME TO r") else {
        panic!()
    };
    assert!(matches!(a.action, AlterSequenceAction::RenameTo(i) if i.value == "r"));
    let Statement::AlterSequence(a) = one("ALTER SEQUENCE q SET SCHEMA s") else {
        panic!()
    };
    assert!(matches!(a.action, AlterSequenceAction::SetSchema(_)));
    syntax("ALTER SEQUENCE q", "", 16);
    unsupported(
        "ALTER SEQUENCE q SET LOGGED",
        "ALTER SEQUENCE ... SET LOGGED is not supported yet",
        21,
    );
    let Statement::DropSequence(d) = one("DROP SEQUENCE IF EXISTS a, b CASCADE") else {
        panic!()
    };
    assert!(d.if_exists && d.cascade && d.names.len() == 2);
}

#[test]
fn serial_and_identity_columns() {
    let c = create_table("CREATE TABLE t (a serial, b bigserial, c smallserial)");
    let TableElement::Column(a) = &c.elements[0] else {
        panic!()
    };
    assert_eq!(a.type_name.names[0].value, "serial");
    let c = create_table(
        "CREATE TABLE t (a int GENERATED ALWAYS AS IDENTITY, \
         b bigint GENERATED BY DEFAULT AS IDENTITY (START WITH 10 INCREMENT BY 2 CACHE 5 SEQUENCE NAME s))",
    );
    let TableElement::Column(a) = &c.elements[0] else {
        panic!()
    };
    assert!(matches!(
        &a.constraints[0].kind,
        ColumnConstraintKind::Identity { when: GeneratedWhen::Always, options } if options.is_empty()
    ));
    let TableElement::Column(b) = &c.elements[1] else {
        panic!()
    };
    let ColumnConstraintKind::Identity { when, options } = &b.constraints[0].kind else {
        panic!()
    };
    assert_eq!(*when, GeneratedWhen::ByDefault);
    assert_eq!(
        seq_kinds(options),
        ["start 10", "inc 2", "cache 5", "name s"]
    );
    unsupported(
        "CREATE TABLE t (a int GENERATED ALWAYS AS (1) STORED)",
        "generated columns is not supported yet",
        22,
    );
    syntax(
        "CREATE TABLE t (a int GENERATED ALWAYS AS IDENTITY ())",
        ")",
        52,
    );
    syntax(
        "CREATE TABLE t (a int GENERATED SOMETIMES)",
        "SOMETIMES",
        32,
    );
}

#[test]
fn insert_overriding() {
    let Statement::Insert(i) = one("INSERT INTO t OVERRIDING SYSTEM VALUE VALUES (1)") else {
        panic!()
    };
    assert_eq!(i.overriding, Some(OverridingKind::System));
    let Statement::Insert(i) = one("INSERT INTO t (a) OVERRIDING USER VALUE SELECT 1") else {
        panic!()
    };
    assert_eq!(i.overriding, Some(OverridingKind::User));
    assert_eq!(i.columns.len(), 1);
    let Statement::Insert(i) = one("INSERT INTO t VALUES (1)") else {
        panic!()
    };
    assert_eq!(i.overriding, None);
    syntax(
        "INSERT INTO t OVERRIDING SYSTEM VALUE DEFAULT VALUES",
        "DEFAULT",
        38,
    );
    syntax("INSERT INTO t OVERRIDING FOO VALUE VALUES (1)", "FOO", 25);
}

// ----- 10: EXPLAIN / COPY --------------------------------------------------

#[test]
fn explain_options() {
    let Statement::Explain(x) = one("EXPLAIN ANALYSE VERBOSE SELECT 1") else {
        panic!()
    };
    let names: Vec<_> = x.options.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["analyze", "verbose"]);
    assert!(x.options.iter().all(|o| o.value.is_none()));
    assert_eq!(
        (x.options[0].name_span.start, x.options[0].name_span.end),
        (8, 15)
    );
    let Statement::Explain(x) = one(
        "EXPLAIN (ANALYZE, costs off, timing 'on', buffers 1, format TEXT, summary 2.5, x -1, verbose true) DELETE FROM t",
    ) else {
        panic!()
    };
    assert_eq!(x.options[0].name, "analyze");
    assert_eq!(x.options[1].value, Some(ExplainValue::Word("off".into())));
    assert_eq!(x.options[2].value, Some(ExplainValue::Word("on".into())));
    assert_eq!(x.options[3].value, Some(ExplainValue::Integer(1)));
    assert_eq!(x.options[4].value, Some(ExplainValue::Word("text".into())));
    assert_eq!(x.options[5].value, Some(ExplainValue::Other("2.5".into())));
    assert_eq!(x.options[6].value, Some(ExplainValue::Integer(-1)));
    assert_eq!(x.options[7].value, Some(ExplainValue::Word("true".into())));
    assert!(matches!(*x.statement, Statement::Delete(_)));
    // The same option twice is kept as written.
    let Statement::Explain(x) = one("EXPLAIN (costs, costs off) SELECT 1") else {
        panic!()
    };
    assert_eq!(x.options.len(), 2);
}

#[test]
fn explain_errors() {
    syntax("EXPLAIN VERBOSE ANALYZE SELECT 1", "ANALYZE", 16);
    syntax("EXPLAIN (analyze true false) SELECT 1", "false", 22);
    syntax("EXPLAIN CREATE TABLE t (a int)", "CREATE", 8);
    syntax("EXPLAIN EXPLAIN SELECT 1", "EXPLAIN", 8);
    syntax("EXPLAIN (analyze", "", 16);
    syntax("EXPLAIN ()", ")", 9);
}

#[test]
fn copy_from_stdin() {
    let Statement::Copy(c) = one("COPY s.t (a, b) FROM STDIN") else {
        panic!()
    };
    assert_eq!(c.table.parts.len(), 2);
    assert_eq!(c.columns.len(), 2);
    assert_eq!(c.direction, CopyDirection::From);
    assert_eq!(c.source, CopySource::Stdin);
    assert!(c.options.is_empty() && c.where_clause.is_none());
    let Statement::Copy(c) = one(
        "COPY t FROM STDIN WITH (FORMAT text, DELIMITER ',', NULL 'x', HEADER, FREEZE off, \
         FORCE_NOT_NULL (a, b), FORCE_QUOTE *, ON_ERROR stop, ENCODING 'UTF8', X 3) WHERE a > 1",
    ) else {
        panic!()
    };
    let o = &c.options;
    assert_eq!(o[0].name, "format");
    assert_eq!(o[0].value, Some(CopyOptionValue::Word("text".into())));
    assert_eq!(o[1].value, Some(CopyOptionValue::String(",".into())));
    assert_eq!(o[3].value, None);
    assert_eq!(o[4].value, Some(CopyOptionValue::Word("off".into())));
    assert_eq!(
        o[5].value,
        Some(CopyOptionValue::List(vec!["a".into(), "b".into()]))
    );
    assert_eq!(o[6].value, Some(CopyOptionValue::Star));
    assert_eq!(o[9].value, Some(CopyOptionValue::Integer(3)));
    assert!(c.where_clause.is_some());
    assert_eq!((o[0].name_span.start, o[0].name_span.end), (24, 30));
}

#[test]
fn copy_legacy_options() {
    let Statement::Copy(c) = one(
        "COPY BINARY t FROM STDIN BINARY DELIMITER AS ';' NULL AS 'n' CSV HEADER QUOTE '\"' ESCAPE '\\' \
         FORCE NOT NULL a, b FORCE QUOTE * FORCE NULL c",
    ) else {
        panic!()
    };
    let names: Vec<_> = c.options.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "format",
            "format",
            "delimiter",
            "null",
            "format",
            "header",
            "quote",
            "escape",
            "force_not_null",
            "force_quote",
            "force_null"
        ]
    );
    assert_eq!(
        c.options[0].value,
        Some(CopyOptionValue::Word("binary".into()))
    );
    assert_eq!(
        c.options[4].value,
        Some(CopyOptionValue::Word("csv".into()))
    );
    assert_eq!(
        c.options[2].value,
        Some(CopyOptionValue::String(";".into()))
    );
    assert_eq!(c.options[9].value, Some(CopyOptionValue::Star));
}

#[test]
fn copy_to_and_errors() {
    let Statement::Copy(c) = one("COPY t TO STDOUT") else {
        panic!()
    };
    assert_eq!(c.direction, CopyDirection::To);
    let Statement::Copy(c) = one("COPY t FROM '/tmp/f'") else {
        panic!()
    };
    assert_eq!(c.source, CopySource::File("/tmp/f".into()));
    let Statement::Copy(c) = one("COPY t TO PROGRAM 'cat'") else {
        panic!()
    };
    assert_eq!(c.source, CopySource::Program("cat".into()));
    unsupported(
        "COPY (SELECT 1) TO STDOUT",
        "COPY TO is not supported yet",
        16,
    );
    syntax("COPY (SELECT 1) FROM STDIN", "FROM", 16);
    syntax("COPY t FROM STDIN, null", ",", 17);
    syntax("COPY t STDIN", "STDIN", 7);
    syntax("COPY t TO STDOUT WHERE a = 1", "WHERE", 17);
    syntax("COPY t FROM STDIN (", "", 19);
}

// ----- 09: type names and datetime keywords --------------------------------

fn cast_type(sql: &str) -> crate::sql::ast::TypeName {
    let Statement::Query(q) = one(sql) else {
        panic!()
    };
    let crate::sql::ast::QueryBody::Select(s) = q.body else {
        panic!()
    };
    let crate::sql::ast::SelectItem::Expr {
        expr: Expr::Cast { type_name, .. },
        ..
    } = &s.targets[0]
    else {
        panic!("{sql}")
    };
    type_name.clone()
}

#[test]
fn type_names() {
    for (ty, name, mods, tz_len) in [
        ("numeric(10, 2)", "numeric", 2, 0),
        ("decimal(5)", "numeric", 1, 0),
        ("numeric", "numeric", 0, 0),
        ("char(3)", "bpchar", 1, 0),
        ("character(3)", "bpchar", 1, 0),
        ("char", "bpchar", 1, 0),
        ("bpchar", "bpchar", 0, 0),
        ("timestamp(3)", "timestamp", 1, 0),
        ("timestamp(3) without time zone", "timestamp", 1, 0),
        ("timestamp(6) with time zone", "timestamptz", 1, 0),
        ("timestamp with time zone", "timestamptz", 0, 0),
        ("date", "date", 0, 0),
        ("regclass", "regclass", 0, 0),
        ("regtype", "regtype", 0, 0),
        ("int2vector", "int2vector", 0, 0),
        ("pg_catalog.regclass", "regclass", 0, 1),
    ] {
        let t = cast_type(&format!("SELECT CAST(x AS {ty})"));
        assert_eq!(t.names.last().unwrap().value, name, "{ty}");
        assert_eq!(t.names.len(), 1 + tz_len, "{ty}");
        assert_eq!(t.modifiers.len(), mods, "{ty}");
    }
    let t = cast_type("SELECT x::numeric(10,2)[]");
    assert_eq!(t.array_bounds, [None]);
    syntax("SELECT CAST(x AS timestamp(-1))", "-", 27);
}

fn session_kind(sql: &str) -> (SessionValueKind, u32, u32) {
    let Statement::Query(q) = one(sql) else {
        panic!()
    };
    let crate::sql::ast::QueryBody::Select(s) = q.body else {
        panic!()
    };
    let crate::sql::ast::SelectItem::Expr {
        expr: Expr::SessionValue { kind, span },
        ..
    } = &s.targets[0]
    else {
        panic!("{sql}")
    };
    (*kind, span.start, span.end)
}

#[test]
fn datetime_keywords() {
    assert_eq!(
        session_kind("SELECT CURRENT_DATE").0,
        SessionValueKind::CurrentDate
    );
    assert_eq!(
        session_kind("SELECT current_timestamp").0,
        SessionValueKind::CurrentTimestamp { precision: -1 }
    );
    assert_eq!(
        session_kind("SELECT CURRENT_TIMESTAMP(3)"),
        (SessionValueKind::CurrentTimestamp { precision: 3 }, 7, 27)
    );
    assert_eq!(
        session_kind("SELECT LOCALTIMESTAMP").0,
        SessionValueKind::LocalTimestamp { precision: -1 }
    );
    assert_eq!(
        session_kind("SELECT localtimestamp(9)").0,
        SessionValueKind::LocalTimestamp { precision: 6 }
    );
    assert_eq!(SessionValueKind::CurrentDate.column_name(), "current_date");
    assert_eq!(
        SessionValueKind::CurrentTimestamp { precision: 2 }.column_name(),
        "current_timestamp"
    );
    assert_eq!(
        SessionValueKind::LocalTimestamp { precision: -1 }.column_name(),
        "localtimestamp"
    );
    unsupported(
        "SELECT CURRENT_TIME",
        "CURRENT_TIME is not supported yet",
        7,
    );
    unsupported("SELECT LOCALTIME", "LOCALTIME is not supported yet", 7);
    syntax("SELECT CURRENT_TIMESTAMP(x)", "x", 25);
    syntax("SELECT CURRENT_DATE(1)", "(", 19);
}
