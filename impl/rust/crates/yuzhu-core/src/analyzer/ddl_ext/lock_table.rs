//! `LOCK TABLE` の `BoundDdl` の組み立て（M5 LK-4b。01 §6.4）。
//!
//! 各名前を解決して、ロックできる種類か（`check_lockable`）を検べる。ロック自体は session が
//! 文の前に取る（§5.9）ので、ここはカタログを引くだけ。

use crate::catalog::{CatalogReader, RelKind, schema};
use crate::error::{Error, Result, sqlstate};
use crate::sql::ast::{LockTableStmt, ObjectName, TableLockMode};
use crate::txn::lock::LockMode;
use crate::types::Oid;

/// ロックする表 1 つ。`db` は実行時に決める（アナライザは接続中の DB の OID を知らない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockTableTarget {
    pub rel: Oid,
    /// 共有カタログ（ロックのキーの `db` は 0）。
    pub shared: bool,
    /// 仮想リレーション。成功して何もしない。
    pub virtual_rel: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundLockTable {
    /// 書かれた順。
    pub targets: Vec<LockTableTarget>,
    pub mode: LockMode,
    pub nowait: bool,
}

impl Default for BoundLockTable {
    fn default() -> Self {
        BoundLockTable {
            targets: Vec::new(),
            mode: LockMode::AccessExclusive,
            nowait: false,
        }
    }
}

pub fn lock_mode(mode: TableLockMode) -> LockMode {
    match mode {
        TableLockMode::AccessShare => LockMode::AccessShare,
        TableLockMode::RowShare => LockMode::RowShare,
        TableLockMode::RowExclusive => LockMode::RowExclusive,
        TableLockMode::ShareUpdateExclusive => LockMode::ShareUpdateExclusive,
        TableLockMode::Share => LockMode::Share,
        TableLockMode::ShareRowExclusive => LockMode::ShareRowExclusive,
        TableLockMode::Exclusive => LockMode::Exclusive,
        TableLockMode::AccessExclusive => LockMode::AccessExclusive,
    }
}

fn dotted(name: &ObjectName) -> String {
    name.parts
        .iter()
        .map(|p| p.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// 種類ごとの可否。`Table` はロックする、`Virtual` は何もしない、`Index` / `Sequence` は 42809。
fn check_lockable(name: &ObjectName, kind: RelKind) -> Result<()> {
    let what = match kind {
        RelKind::Table | RelKind::Virtual => return Ok(()),
        RelKind::Index => "indexes",
        RelKind::Sequence => "sequences",
    };
    Err(Error::new(
        sqlstate::WRONG_OBJECT_TYPE,
        format!("cannot lock relation \"{}\"", name.name().value),
    )
    .with_detail(format!("This operation is not supported for {what}."))
    .with_span(name.span))
}

pub fn analyze(catalog: &dyn CatalogReader, stmt: &LockTableStmt) -> Result<BoundLockTable> {
    let mut targets = Vec::with_capacity(stmt.tables.len());
    for t in &stmt.tables {
        let n = &t.name;
        if n.parts.len() > 3 {
            return Err(Error::syntax_at(
                n.span,
                format!(
                    "improper relation name (too many dotted names): {}",
                    dotted(n)
                ),
            ));
        }
        if n.parts.len() == 3 && n.parts[0].value != catalog.current_database() {
            return Err(Error::not_supported(format!(
                "cross-database references are not implemented: {}",
                dotted(n)
            ))
            .with_span(n.span));
        }
        let schema_name = n.schema().map(|s| s.value.as_str());
        let Some((oid, kind)) = catalog.relation_kind(schema_name, &n.name().value)? else {
            return Err(Error::new(
                sqlstate::UNDEFINED_TABLE,
                format!("relation \"{}\" does not exist", dotted(n)),
            )
            .with_span(n.span));
        };
        check_lockable(n, kind)?;
        targets.push(LockTableTarget {
            rel: oid,
            shared: schema::catalog_def(oid).is_some_and(|d| d.shared),
            virtual_rel: kind == RelKind::Virtual,
        });
    }
    Ok(BoundLockTable {
        targets,
        mode: lock_mode(stmt.mode),
        nowait: stmt.nowait,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::ddl_ext;
    use crate::analyzer::{BoundDdl, BoundStatement};
    use crate::catalog::fake::{FakeCatalog, TableBuilder};
    use crate::sql::ast::Statement;
    use crate::types::SqlType;

    fn cat() -> (FakeCatalog, Oid) {
        let mut c = FakeCatalog::new("postgres");
        let t = c.add(
            &TableBuilder::new("t")
                .column_nn("id", SqlType::INT4)
                .primary_key(&["id"]),
        );
        (c, t.oid)
    }

    fn run(c: &FakeCatalog, sql: &str) -> Result<BoundLockTable> {
        let stmt = crate::sql::parse(sql).unwrap().remove(0);
        match ddl_ext::analyze(c, &stmt)? {
            BoundDdl::LockTable(b) => Ok(b),
            other => panic!(
                "not LockTable: {other:?} ({:?})",
                matches!(stmt, Statement::LockTable(_))
            ),
        }
    }

    #[test]
    fn default_mode_and_order() {
        let (mut c, t) = cat();
        let u = c
            .add(&TableBuilder::new("u").column("x", SqlType::INT4))
            .oid;
        let b = run(&c, "lock table u, t").unwrap();
        assert_eq!(b.mode, LockMode::AccessExclusive);
        assert!(!b.nowait);
        let rels: Vec<Oid> = b.targets.iter().map(|x| x.rel).collect();
        assert_eq!(rels, vec![u, t]);
        assert!(b.targets.iter().all(|x| !x.virtual_rel && !x.shared));
    }

    #[test]
    fn all_modes_map() {
        let (c, _) = cat();
        let cases = [
            ("access share", LockMode::AccessShare),
            ("row share", LockMode::RowShare),
            ("row exclusive", LockMode::RowExclusive),
            ("share update exclusive", LockMode::ShareUpdateExclusive),
            ("share", LockMode::Share),
            ("share row exclusive", LockMode::ShareRowExclusive),
            ("exclusive", LockMode::Exclusive),
            ("access exclusive", LockMode::AccessExclusive),
        ];
        for (w, m) in cases {
            let b = run(&c, &format!("lock t in {w} mode nowait")).unwrap();
            assert_eq!(b.mode, m);
            assert!(b.nowait);
            assert_eq!(LockMode::from_sql_words(w), Some(m));
        }
    }

    #[test]
    fn missing_relation_is_42p01() {
        let (c, _) = cat();
        let e = run(&c, "lock table nosuch").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
        assert_eq!(e.message, "relation \"nosuch\" does not exist");
        let e = run(&c, "lock table s.nosuch").unwrap_err();
        assert_eq!(e.message, "relation \"s.nosuch\" does not exist");
    }

    #[test]
    fn index_and_sequence_are_42809_without_schema() {
        let (mut c, _) = cat();
        c.add(&TableBuilder::new("s").serial("id", SqlType::INT4));
        let e = run(&c, "lock table public.t_pkey").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::WRONG_OBJECT_TYPE);
        assert_eq!(e.message, "cannot lock relation \"t_pkey\"");
        assert_eq!(
            e.detail.as_deref(),
            Some("This operation is not supported for indexes.")
        );
        let e = run(&c, "lock table s_id_seq").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::WRONG_OBJECT_TYPE);
        assert_eq!(
            e.detail.as_deref(),
            Some("This operation is not supported for sequences.")
        );
    }

    #[test]
    fn goes_through_the_top_level_analyzer() {
        let (c, _) = cat();
        let stmt = crate::sql::parse("lock t").unwrap().remove(0);
        let b = crate::analyzer::analyze(&stmt, &c).unwrap();
        assert!(matches!(b, BoundStatement::Ddl(BoundDdl::LockTable(_))));
    }
}
