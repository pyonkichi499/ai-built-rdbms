//! PRIMARY KEY / UNIQUE の解析（`m4/07-catalog-ddl.md` §6.9）。
//!
//! `analyze_create_table`（`ddl.rs`）と `ALTER TABLE ... ADD`（`ddl_index.rs`）が呼ぶ。PostgreSQL の
//! `transformIndexConstraints` と同じ手順で、キー列の解決・PRIMARY KEY の重複検査・同じキーの統合を行う。

use super::bound::{BoundIndexConstraint, IndexConstraintKind, RelOption};
use crate::catalog::{ColumnDef, TableDef};
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{
    self, ColumnConstraintKind, Ident, IndexParams, TableConstraint, TableConstraintKind,
    TableElement,
};

/// `0A000` で、メッセージは文言どおり（位置つき）。
pub(super) fn unsupported(message: &str, span: Span) -> Error {
    Error::not_supported(message).with_span(span)
}

/// `WITH (...)` の AST を Bound の `RelOption` にする。
pub(super) fn convert_options(options: &[ast::RelOption]) -> Vec<RelOption> {
    options
        .iter()
        .map(|o| RelOption {
            namespace: o.namespace.as_ref().map(|n| n.value.clone()),
            name: o.name.value.clone(),
            value: o.value.clone(),
        })
        .collect()
}

/// 集めた PRIMARY KEY / UNIQUE の 1 件。
struct Pending<'a> {
    name: Option<&'a Ident>,
    primary: bool,
    columns: Vec<i16>,
    options: Vec<RelOption>,
    span: Span,
}

/// 「`kind` の制約」の表示名（`appears twice in primary key constraint`）。
fn kind_label(primary: bool) -> &'static str {
    if primary { "primary key" } else { "unique" }
}

/// 未対応の索引パラメータ・`DEFERRABLE` を `0A000` にする（07 §6.9 の 1）。
fn reject_unsupported(
    params: &IndexParams,
    deferrable: bool,
    span: Span,
    alter: bool,
) -> Result<()> {
    if deferrable {
        return Err(unsupported(
            "DEFERRABLE constraints are not supported yet",
            span,
        ));
    }
    if params.nulls_not_distinct {
        return Err(unsupported("NULLS NOT DISTINCT is not supported yet", span));
    }
    if !params.include.is_empty() {
        return Err(unsupported("INCLUDE columns are not supported yet", span));
    }
    if params.tablespace.is_some() {
        return Err(unsupported(
            "USING INDEX TABLESPACE is not supported yet",
            span,
        ));
    }
    if params.using_index.is_some() {
        let what = if alter {
            "ALTER TABLE ... ADD CONSTRAINT USING INDEX"
        } else {
            "USING INDEX"
        };
        return Err(super::not_supported(what, span));
    }
    Ok(())
}

/// キー列の名前を attnum にする。なければ `42703`、同じ列が 2 回なら `42701`。
/// PG の位置は列でなく制約（キーワード）の先頭。
fn resolve_key_columns(
    names: &[Ident],
    columns: &[ColumnDef],
    primary: bool,
    span: Span,
) -> Result<Vec<i16>> {
    let mut out: Vec<i16> = Vec::with_capacity(names.len());
    for n in names {
        let Some(col) = columns.iter().find(|c| c.name == n.value) else {
            return Err(Error::new(
                sqlstate::UNDEFINED_COLUMN,
                format!("column \"{}\" named in key does not exist", n.value),
            )
            .with_span(span));
        };
        if out.contains(&col.attnum) {
            return Err(Error::new(
                sqlstate::DUPLICATE_COLUMN,
                format!(
                    "column \"{}\" appears twice in {} constraint",
                    n.value,
                    kind_label(primary)
                ),
            )
            .with_span(span));
        }
        out.push(col.attnum);
    }
    Ok(out)
}

/// `pending` に積む。PRIMARY KEY が 2 つ目なら `42P16`（位置は 2 つ目）。
fn push_pending<'a>(
    table_name: &str,
    p: Pending<'a>,
    pending: &mut Vec<Pending<'a>>,
) -> Result<()> {
    if p.primary && pending.iter().any(|q| q.primary) {
        return Err(Error::new(
            sqlstate::INVALID_TABLE_DEFINITION,
            format!("multiple primary keys for table \"{table_name}\" are not allowed"),
        )
        .with_span(p.span));
    }
    pending.push(p);
    Ok(())
}

/// CREATE TABLE の要素（列定義の列制約と表制約）から PRIMARY KEY / UNIQUE を集めて解決する。
/// PRIMARY KEY の列は `columns[i].not_null = true` にする。重複は統合する（07 §6.9 の 4）。
#[allow(clippy::too_many_lines)]
pub(super) fn analyze_index_constraints(
    table_name: &str,
    elements: &[TableElement],
    columns: &mut [ColumnDef],
) -> Result<Vec<BoundIndexConstraint>> {
    // 1〜3: 集める。要素の順に、列の解決 → PRIMARY KEY が 1 つだけ、の順で検査する。
    let mut pending: Vec<Pending<'_>> = Vec::new();
    // 名前つきの CHECK（名前、位置）。PRIMARY KEY / UNIQUE の名前との衝突の検査に使う。
    let mut check_names: Vec<(&str, Span)> = Vec::new();
    for elem in elements {
        match elem {
            TableElement::Column(cd) => {
                for c in &cd.constraints {
                    let (primary, params) = match &c.kind {
                        ColumnConstraintKind::PrimaryKey(p) => (true, p),
                        ColumnConstraintKind::Unique(p) => (false, p),
                        ColumnConstraintKind::Check(_) => {
                            if let Some(n) = &c.name {
                                check_names.push((n.value.as_str(), c.span));
                            }
                            continue;
                        }
                        _ => continue,
                    };
                    reject_unsupported(params, c.deferrable, c.span, false)?;
                    let keys = resolve_key_columns(
                        std::slice::from_ref(&cd.name),
                        columns,
                        primary,
                        c.span,
                    )?;
                    push_pending(
                        table_name,
                        Pending {
                            name: c.name.as_ref(),
                            primary,
                            columns: keys,
                            options: convert_options(&params.options),
                            span: c.span,
                        },
                        &mut pending,
                    )?;
                }
            }
            TableElement::Constraint(tc) => {
                let (primary, kc) = match &tc.kind {
                    TableConstraintKind::PrimaryKey(k) => (true, k),
                    TableConstraintKind::Unique(k) => (false, k),
                    TableConstraintKind::Check(_) => {
                        if let Some(n) = &tc.name {
                            check_names.push((n.value.as_str(), tc.span));
                        }
                        continue;
                    }
                    TableConstraintKind::ForeignKey { .. } => continue,
                };
                reject_unsupported(&kc.params, tc.deferrable, tc.span, false)?;
                let keys = resolve_key_columns(&kc.columns, columns, primary, tc.span)?;
                push_pending(
                    table_name,
                    Pending {
                        name: tc.name.as_ref(),
                        primary,
                        columns: keys,
                        options: convert_options(&kc.params.options),
                        span: tc.span,
                    },
                    &mut pending,
                )?;
            }
        }
    }

    // 4: 統合。PRIMARY KEY を先頭に置き、残りは出現順。キー列の並びが同じものは捨てる。
    let (primary, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|p| p.primary);
    let mut kept: Vec<Pending<'_>> = Vec::with_capacity(primary.len() + rest.len());
    for p in primary.into_iter().chain(rest) {
        if let Some(k) = kept.iter_mut().find(|k| k.columns == p.columns) {
            if k.name.is_none() {
                k.name = p.name;
            }
        } else {
            kept.push(p);
        }
    }

    // 5: 名前の検査。CHECK の名前と衝突する PRIMARY KEY / UNIQUE（どちらが先でも同じ文言）。
    // 位置は付けない（PG も付けない）。
    for k in &kept {
        let Some(n) = k.name else { continue };
        if check_names.iter().any(|(c, _)| *c == n.value) {
            return Err(Error::new(
                sqlstate::DUPLICATE_OBJECT,
                format!(
                    "constraint \"{}\" for relation \"{table_name}\" already exists",
                    n.value
                ),
            ));
        }
    }

    // PRIMARY KEY の列は NOT NULL にする（明示の NULL は黙って上書き。矛盾は呼び出し側が 42601）。
    for k in kept.iter().filter(|k| k.primary) {
        for attnum in &k.columns {
            if let Some(c) = columns.iter_mut().find(|c| c.attnum == *attnum) {
                c.not_null = true;
            }
        }
    }

    Ok(kept
        .into_iter()
        .map(|k| BoundIndexConstraint {
            name: k.name.map(|n| n.value.clone()),
            kind: if k.primary {
                IndexConstraintKind::PrimaryKey
            } else {
                IndexConstraintKind::Unique
            },
            columns: k.columns,
            options: k.options,
            span: k.span,
        })
        .collect())
}

/// `ALTER TABLE ... ADD` の 1 件（PRIMARY KEY / UNIQUE）。重複の統合はしない。列の解決と `WITH` の
/// 取り出しだけで、PRIMARY KEY の `not_null` の更新は実行時（07 §5.4）。
/// CHECK / FOREIGN KEY は呼び出し側（`analyze_alter_table`）が `0A000` にする。
pub(super) fn analyze_add_constraint(
    table: &TableDef,
    c: &TableConstraint,
) -> Result<BoundIndexConstraint> {
    let (primary, kc) = match &c.kind {
        TableConstraintKind::PrimaryKey(k) => (true, k),
        TableConstraintKind::Unique(k) => (false, k),
        TableConstraintKind::Check(_) => {
            return Err(super::not_supported("ALTER TABLE ... ADD CHECK", c.span));
        }
        TableConstraintKind::ForeignKey { .. } => {
            return Err(super::not_supported(
                "ALTER TABLE ... ADD FOREIGN KEY",
                c.span,
            ));
        }
    };
    reject_unsupported(&kc.params, c.deferrable, c.span, true)?;
    // ALTER TABLE ADD PRIMARY KEY は PG が別の文言（`of relation`）で位置なしで報告する。
    // UNIQUE は CREATE TABLE と同じ `named in key`（PG 17 で確認）。
    if let Some(n) = kc
        .columns
        .iter()
        .filter(|_| primary)
        .find(|n| !table.columns.iter().any(|c| c.name == n.value))
    {
        return Err(Error::new(
            sqlstate::UNDEFINED_COLUMN,
            format!(
                "column \"{}\" of relation \"{}\" does not exist",
                n.value, table.name
            ),
        ));
    }
    let columns = resolve_key_columns(&kc.columns, &table.columns, primary, c.span)?;
    Ok(BoundIndexConstraint {
        name: c.name.as_ref().map(|n| n.value.clone()),
        kind: if primary {
            IndexConstraintKind::PrimaryKey
        } else {
            IndexConstraintKind::Unique
        },
        columns,
        options: convert_options(&kc.params.options),
        span: c.span,
    })
}

/// `ALTER TABLE IF EXISTS` で表がなかったときの制約（実行されない。列は解決しない）。未対応の機能は
/// 表があるときと同じく `0A000` にする。
pub(super) fn placeholder_add_constraint(c: &TableConstraint) -> Result<BoundIndexConstraint> {
    let (primary, kc) = match &c.kind {
        TableConstraintKind::PrimaryKey(k) => (true, k),
        TableConstraintKind::Unique(k) => (false, k),
        TableConstraintKind::Check(_) => {
            return Err(super::not_supported("ALTER TABLE ... ADD CHECK", c.span));
        }
        TableConstraintKind::ForeignKey { .. } => {
            return Err(super::not_supported(
                "ALTER TABLE ... ADD FOREIGN KEY",
                c.span,
            ));
        }
    };
    reject_unsupported(&kc.params, c.deferrable, c.span, true)?;
    Ok(BoundIndexConstraint {
        name: c.name.as_ref().map(|n| n.value.clone()),
        kind: if primary {
            IndexConstraintKind::PrimaryKey
        } else {
            IndexConstraintKind::Unique
        },
        columns: Vec::new(),
        options: convert_options(&kc.params.options),
        span: c.span,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::{FakeCatalog, TableBuilder};
    use crate::sql::ast::Statement;
    use crate::types::SqlType;

    /// `CREATE TABLE` の列定義から、すべて nullable な int4 列を作る。
    fn parse_create(sql: &str) -> (String, Vec<TableElement>, Vec<ColumnDef>) {
        let Statement::CreateTable(ct) = crate::sql::parse(sql).unwrap().remove(0) else {
            panic!("not CREATE TABLE: {sql}");
        };
        let mut columns = Vec::new();
        for e in &ct.elements {
            if let TableElement::Column(cd) = e {
                columns.push(ColumnDef {
                    name: cd.name.value.clone(),
                    attnum: i16::try_from(columns.len() + 1).unwrap(),
                    ty: SqlType::INT4,
                    not_null: false,
                    default: None,
                    identity: None,
                });
            }
        }
        (ct.name.name().value.clone(), ct.elements, columns)
    }

    fn analyze(sql: &str) -> Result<(Vec<BoundIndexConstraint>, Vec<ColumnDef>)> {
        let (name, elements, mut columns) = parse_create(sql);
        let cs = analyze_index_constraints(&name, &elements, &mut columns)?;
        Ok((cs, columns))
    }

    fn ok(sql: &str) -> (Vec<BoundIndexConstraint>, Vec<ColumnDef>) {
        analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    }

    fn err(sql: &str) -> Error {
        analyze(sql).expect_err(sql)
    }

    fn summary(cs: &[BoundIndexConstraint]) -> Vec<(Option<&str>, IndexConstraintKind, Vec<i16>)> {
        cs.iter()
            .map(|c| (c.name.as_deref(), c.kind, c.columns.clone()))
            .collect()
    }

    use IndexConstraintKind::{PrimaryKey as Pk, Unique as Uq};

    #[test]
    fn column_and_table_level_keys() {
        let (cs, cols) =
            ok("CREATE TABLE t (a int PRIMARY KEY, b int UNIQUE, c int, UNIQUE (b, c))");
        assert_eq!(
            summary(&cs),
            vec![
                (None, Pk, vec![1]),
                (None, Uq, vec![2]),
                (None, Uq, vec![2, 3])
            ]
        );
        assert!(cols[0].not_null && !cols[1].not_null && !cols[2].not_null);
        assert!(cs.iter().all(|c| c.options.is_empty()));
    }

    #[test]
    fn primary_key_comes_first_and_makes_columns_not_null() {
        let (cs, cols) = ok("CREATE TABLE t (a int, b int, UNIQUE (a), PRIMARY KEY (b, a))");
        assert_eq!(
            summary(&cs),
            vec![(None, Pk, vec![2, 1]), (None, Uq, vec![1])]
        );
        assert!(cols[0].not_null && cols[1].not_null);
    }

    #[test]
    fn explicit_names_and_options() {
        let (cs, _) = ok(
            "CREATE TABLE t (a int CONSTRAINT pk PRIMARY KEY WITH (fillfactor = 70), \
             b int, CONSTRAINT u UNIQUE (b) WITH (fillfactor = 90))",
        );
        assert_eq!(cs[0].name.as_deref(), Some("pk"));
        assert_eq!(
            cs[0].options,
            vec![RelOption {
                namespace: None,
                name: "fillfactor".into(),
                value: Some("70".into())
            }]
        );
        assert_eq!(cs[1].name.as_deref(), Some("u"));
        assert_eq!(cs[1].options[0].value.as_deref(), Some("90"));
    }

    #[test]
    fn duplicates_are_merged() {
        // unique(a), unique(a) -> 1 つ
        let (cs, _) = ok("CREATE TABLE t (a int, UNIQUE (a), UNIQUE (a))");
        assert_eq!(summary(&cs), vec![(None, Uq, vec![1])]);
        // primary key (a), unique (a) -> PRIMARY KEY 1 つ
        let (cs, _) = ok("CREATE TABLE t (a int, PRIMARY KEY (a), UNIQUE (a))");
        assert_eq!(summary(&cs), vec![(None, Pk, vec![1])]);
        // 捨てられる側の名前を、無名の残る側に移す
        let (cs, _) = ok("CREATE TABLE t (a int PRIMARY KEY, CONSTRAINT u2 UNIQUE (a))");
        assert_eq!(summary(&cs), vec![(Some("u2"), Pk, vec![1])]);
        // 残る側が名前を持てば、そのまま
        let (cs, _) =
            ok("CREATE TABLE t (a int CONSTRAINT p PRIMARY KEY, CONSTRAINT u2 UNIQUE (a))");
        assert_eq!(summary(&cs), vec![(Some("p"), Pk, vec![1])]);
        // 並びが違えば別
        let (cs, _) = ok("CREATE TABLE t (a int, b int, UNIQUE (a, b), UNIQUE (b, a))");
        assert_eq!(
            summary(&cs),
            vec![(None, Uq, vec![1, 2]), (None, Uq, vec![2, 1])]
        );
    }

    #[test]
    fn merge_keeps_options_of_the_kept_one() {
        let (cs, _) = ok(
            "CREATE TABLE t (a int, UNIQUE (a) WITH (fillfactor = 60), UNIQUE (a) WITH (fillfactor = 80))",
        );
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].options[0].value.as_deref(), Some("60"));
    }

    #[test]
    fn multiple_primary_keys() {
        for sql in [
            "CREATE TABLE t (a int PRIMARY KEY, b int PRIMARY KEY)",
            "CREATE TABLE t (a int PRIMARY KEY, b int, PRIMARY KEY (b))",
            "CREATE TABLE t (a int, b int, PRIMARY KEY (a), PRIMARY KEY (a))",
        ] {
            let e = err(sql);
            assert_eq!(e.sqlstate.code(), "42P16", "{sql}");
            assert_eq!(
                e.message,
                "multiple primary keys for table \"t\" are not allowed"
            );
            assert!(e.cursor_byte.is_some());
        }
        // 位置は 2 つ目
        let sql = "CREATE TABLE t (a int PRIMARY KEY, b int PRIMARY KEY)";
        let e = err(sql);
        assert_eq!(
            e.cursor_byte,
            Some(u32::try_from(sql.rfind("PRIMARY").unwrap()).unwrap())
        );
    }

    #[test]
    fn key_column_errors() {
        let e = err("CREATE TABLE t (a int, PRIMARY KEY (zz))");
        assert_eq!(e.sqlstate.code(), "42703");
        assert_eq!(e.message, "column \"zz\" named in key does not exist");
        assert!(e.cursor_byte.is_some());
        let e = err("CREATE TABLE t (a int, UNIQUE (a, zz))");
        assert_eq!(e.sqlstate.code(), "42703");

        let e = err("CREATE TABLE t (a int, PRIMARY KEY (a, a))");
        assert_eq!(e.sqlstate.code(), "42701");
        assert_eq!(
            e.message,
            "column \"a\" appears twice in primary key constraint"
        );
        let e = err("CREATE TABLE t (a int, UNIQUE (a, a))");
        assert_eq!(e.message, "column \"a\" appears twice in unique constraint");
    }

    #[test]
    fn unsupported_features_are_0a000() {
        for (sql, msg) in [
            (
                "CREATE TABLE t (a int PRIMARY KEY DEFERRABLE)",
                "DEFERRABLE constraints are not supported yet",
            ),
            (
                "CREATE TABLE t (a int, UNIQUE (a) DEFERRABLE INITIALLY DEFERRED)",
                "DEFERRABLE constraints are not supported yet",
            ),
            (
                "CREATE TABLE t (a int, UNIQUE NULLS NOT DISTINCT (a))",
                "NULLS NOT DISTINCT is not supported yet",
            ),
            (
                "CREATE TABLE t (a int, b int, PRIMARY KEY (a) INCLUDE (b))",
                "INCLUDE columns are not supported yet",
            ),
            (
                "CREATE TABLE t (a int PRIMARY KEY USING INDEX TABLESPACE pg_default)",
                "USING INDEX TABLESPACE is not supported yet",
            ),
        ] {
            let e = err(sql);
            assert_eq!(e.sqlstate.code(), "0A000", "{sql}");
            assert_eq!(e.message, msg, "{sql}");
            assert!(e.cursor_byte.is_some(), "{sql}");
        }
    }

    #[test]
    fn constraint_name_clashes_with_check() {
        for sql in [
            "CREATE TABLE cc2 (a int CONSTRAINT c1 PRIMARY KEY, CONSTRAINT c1 CHECK (a > 0))",
            "CREATE TABLE cc2 (a int, CONSTRAINT c1 CHECK (a > 0), CONSTRAINT c1 UNIQUE (a))",
            "CREATE TABLE cc2 (a int CONSTRAINT c1 CHECK (a > 0) CONSTRAINT c1 UNIQUE)",
        ] {
            let e = err(sql);
            assert_eq!(e.sqlstate.code(), "42710", "{sql}");
            assert_eq!(
                e.message, "constraint \"c1\" for relation \"cc2\" already exists",
                "{sql}"
            );
        }
        // PRIMARY KEY / UNIQUE どうしの重複は実行時（42P07）。ここでは通す。
        let (cs, _) =
            ok("CREATE TABLE t (a int, b int, CONSTRAINT c1 UNIQUE (a), CONSTRAINT c1 UNIQUE (b))");
        assert_eq!(cs.len(), 2);
    }

    #[test]
    fn no_constraints() {
        let (cs, cols) = ok("CREATE TABLE t (a int, b int CHECK (b > 0))");
        assert!(cs.is_empty());
        assert!(cols.iter().all(|c| !c.not_null));
    }

    fn fake_table() -> std::sync::Arc<TableDef> {
        let mut c = FakeCatalog::new("postgres");
        c.add(
            &TableBuilder::new("a1")
                .column("a", SqlType::INT4)
                .column("b", SqlType::TEXT),
        )
    }

    fn alter(table: &TableDef, sql: &str) -> Result<BoundIndexConstraint> {
        let Statement::AlterTable(at) = crate::sql::parse(sql).unwrap().remove(0) else {
            panic!("not ALTER TABLE");
        };
        let ast::AlterTableAction::AddConstraint(tc) = at.action else {
            panic!("not ADD CONSTRAINT");
        };
        analyze_add_constraint(table, &tc)
    }

    #[test]
    fn add_constraint_resolves_columns() {
        let t = fake_table();
        let c = alter(
            &t,
            "ALTER TABLE a1 ADD CONSTRAINT k PRIMARY KEY (b, a) WITH (fillfactor = 50)",
        )
        .unwrap();
        assert_eq!(c.name.as_deref(), Some("k"));
        assert_eq!(c.kind, Pk);
        assert_eq!(c.columns, vec![2, 1]);
        assert_eq!(c.options[0].name, "fillfactor");
        let c = alter(&t, "ALTER TABLE a1 ADD UNIQUE (a)").unwrap();
        assert_eq!((c.name, c.kind, c.columns), (None, Uq, vec![1]));

        let e = alter(&t, "ALTER TABLE a1 ADD UNIQUE (a, a)").unwrap_err();
        assert_eq!(e.sqlstate.code(), "42701");
        assert_eq!(e.message, "column \"a\" appears twice in unique constraint");
        let e = alter(&t, "ALTER TABLE a1 ADD PRIMARY KEY (zz)").unwrap_err();
        assert_eq!(e.sqlstate.code(), "42703");
        assert_eq!(e.message, "column \"zz\" of relation \"a1\" does not exist");
        let e = alter(&t, "ALTER TABLE a1 ADD UNIQUE (a, zz)").unwrap_err();
        assert_eq!(e.sqlstate.code(), "42703");
        assert_eq!(e.message, "column \"zz\" named in key does not exist");
        let e = alter(&t, "ALTER TABLE a1 ADD UNIQUE (a, zz)").unwrap_err();
        assert_eq!(e.sqlstate.code(), "42703");
        assert_eq!(e.message, "column \"zz\" named in key does not exist");
        let e = alter(&t, "ALTER TABLE a1 ADD UNIQUE (a) DEFERRABLE").unwrap_err();
        assert_eq!(e.sqlstate.code(), "0A000");
    }
}
