//! Regression tests for PostgreSQL-compatibility review findings.

use yuzhu_core::error::Error;
use yuzhu_core::testing::TestCluster;
use yuzhu_core::{ColumnDesc, Notice, ResultSink, Session};

#[derive(Default)]
struct S {
    rows: Vec<Vec<Option<String>>>,
    errs: Vec<(String, Option<u32>, Option<String>)>,
}

impl ResultSink for S {
    fn row_description(&mut self, _: &[ColumnDesc]) -> std::io::Result<()> {
        Ok(())
    }
    fn data_row(&mut self, v: &[Option<String>]) -> std::io::Result<()> {
        self.rows.push(v.to_vec());
        Ok(())
    }
    fn command_complete(&mut self, _: &str) -> std::io::Result<()> {
        Ok(())
    }
    fn empty_query(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn error(&mut self, e: &Error) -> std::io::Result<()> {
        self.errs
            .push((e.message.clone(), e.position, e.hint.clone()));
        Ok(())
    }
    fn notice(&mut self, _: &Notice) -> std::io::Result<()> {
        Ok(())
    }
    fn parameter_status(&mut self, _: &str, _: &str) -> std::io::Result<()> {
        Ok(())
    }
}

fn q(s: &mut Session, sql: &str) -> S {
    let mut k = S::default();
    s.execute_simple(sql, &mut k).unwrap();
    k
}

fn err(s: &mut Session, sql: &str) -> (String, Option<u32>, Option<String>) {
    q(s, sql).errs.remove(0)
}

fn cell(s: &mut Session, sql: &str) -> Option<String> {
    let r = q(s, sql);
    assert!(r.errs.is_empty(), "{:?}", r.errs);
    r.rows[0][0].clone()
}

#[test]
fn psql_l_access_privileges_query_runs() {
    let tc = TestCluster::new();
    let mut s = tc.session("postgres").unwrap();
    let r = q(
        &mut s,
        "SELECT d.datname, pg_catalog.array_to_string(d.datacl, E'\\n') AS acl, \
         pg_catalog.array_length(d.datacl, 1) FROM pg_catalog.pg_database d ORDER BY 1",
    );
    assert!(r.errs.is_empty(), "{:?}", r.errs);
    assert_eq!(r.rows.len(), 3);
}

#[test]
fn error_messages_use_pg_type_names() {
    let tc = TestCluster::new();
    let mut s = tc.session("postgres").unwrap();
    let m = |s: &mut Session, sql: &str| err(s, sql).0;
    assert_eq!(
        m(&mut s, "select 'a'::\"char\" + 1"),
        "operator does not exist: \"char\" + integer"
    );
    assert_eq!(
        m(&mut s, "select ctid + 1 from pg_class"),
        "operator does not exist: tid + integer"
    );
    assert_eq!(
        m(&mut s, "select xmin::int from pg_class"),
        "cannot cast type xid to integer"
    );
    assert_eq!(
        m(&mut s, "select 5::xid"),
        "cannot cast type integer to xid"
    );
}

#[test]
fn format_type_ignores_typmod_of_special_cased_types() {
    let tc = TestCluster::new();
    let mut s = tc.session("postgres").unwrap();
    for (oid, expected) in [
        (23, "integer"),
        (701, "double precision"),
        (20, "bigint"),
        (16, "boolean"),
        (25, "text(5)"),
    ] {
        let sql = format!("select format_type({oid}, 5)");
        assert_eq!(cell(&mut s, &sql).as_deref(), Some(expected));
    }
}

#[test]
fn operator_merge_and_hash_flags() {
    let tc = TestCluster::new();
    let mut s = tc.session("postgres").unwrap();
    let r = q(
        &mut s,
        "select oid, oprcanmerge, oprcanhash from pg_operator where oid in (15, 96, 98) order by oid",
    );
    let got: Vec<_> = r
        .rows
        .iter()
        .map(|r| {
            (
                r[0].clone().unwrap(),
                r[1].clone().unwrap(),
                r[2].clone().unwrap(),
            )
        })
        .collect();
    let t = |a: &str, b: &str, c: &str| (a.to_owned(), b.to_owned(), c.to_owned());
    assert_eq!(got.len(), 3);
    for e in [t("15", "t", "t"), t("96", "t", "t"), t("98", "t", "t")] {
        assert!(got.contains(&e), "{e:?} in {got:?}");
    }
}

#[test]
fn hidden_table_name_reference_and_ddl_without_position() {
    let tc = TestCluster::new();
    let mut s = tc.session("postgres").unwrap();
    q(&mut s, "create table s4(a int)");
    let (m, _, hint) = err(&mut s, "select s4.a from s4 t");
    assert_eq!(m, "invalid reference to FROM-clause entry for table \"s4\"");
    assert_eq!(
        hint.as_deref(),
        Some("Perhaps you meant to reference the table alias \"t\".")
    );
    let (m, _, _) = err(&mut s, "select x.a from s4 t");
    assert_eq!(m, "missing FROM-clause entry for table \"x\"");

    for sql in [
        "create table s4(x int)",
        "drop table z2",
        "create table t(a int, a int)",
        "create table s1(ctid int)",
    ] {
        assert_eq!(err(&mut s, sql).1, None, "{sql}");
    }
}

#[test]
fn oidvector_error_quotes_the_rest() {
    let tc = TestCluster::new();
    let mut s = tc.session("postgres").unwrap();
    assert_eq!(
        err(&mut s, "select '1,2'::oidvector").0,
        "invalid input syntax for type oid: \",2\""
    );
}
