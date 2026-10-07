//! DDL が書く `pg_depend` の「何を書くか」（`m4/07-catalog-ddl.md` §6.8）。
//!
//! 依存関係そのもの（`ObjectAddress`・`plan_drop`・`drop_objects`）は `catalog::depend`、行の組み立ては
//! `catalog::store`。ここは CREATE TABLE が `NewTable.extra_depends` に渡す依存と、索引の依存の一覧を作る。

use crate::analyzer::query::BoundExpr;
use crate::catalog::depend::{DependType, NewDepend, ObjectAddress};
use crate::catalog::store::NewIndex;
use crate::expr::ExprKind;
use crate::types::{Datum, Oid, oid};

/// CREATE TABLE の `extra_depends`（07 §5.1 の手順 10）。
///
/// - `owned_sequences`（SERIAL の列と、そのシーケンス）: `pg_attrdef` → シーケンス（`n`）
/// - `default_refs`（既定値の式が `regclass` 定数で指すリレーション）: `pg_attrdef` → リレーション（`n`）
///
/// `attrdef_oids` は既定値を持つ列の `(attnum, pg_attrdef の OID)`。既定値のない列の項目は無視する。
/// 同じ組は 1 つにまとめる。
pub(crate) fn default_value_depends(
    attrdef_oids: &[(i16, Oid)],
    owned_sequences: &[(i16, Oid)],
    default_refs: &[(i16, Oid)],
) -> Vec<NewDepend> {
    let mut out: Vec<NewDepend> = Vec::new();
    for &(attnum, rel) in owned_sequences.iter().chain(default_refs) {
        let Some(&(_, attrdef)) = attrdef_oids.iter().find(|(a, _)| *a == attnum) else {
            continue;
        };
        let dep = NewDepend {
            dependent: ObjectAddress::attrdef(attrdef),
            referenced: ObjectAddress::relation(rel),
            deptype: DependType::Normal,
        };
        if !out.contains(&dep) {
            out.push(dep);
        }
    }
    out
}

#[cfg_attr(not(test), allow(dead_code))]
/// 索引の依存（07 §3.6）。`CatalogStore::create_table` / `create_index` が書く行と同じもの。
///
/// - 制約が所有する索引: 制約 → 表の各キー列（`a`）、索引 → 制約（`i`）
/// - 通常の索引: 索引 → 表の各キー列（`a`。同じ列は 1 行）
pub(crate) fn index_depends(index: &NewIndex) -> Vec<NewDepend> {
    let mut columns: Vec<i16> = Vec::new();
    for c in &index.columns {
        if !columns.contains(&c.column.attnum) {
            columns.push(c.column.attnum);
        }
    }
    let mut out = Vec::new();
    if let Some(con) = &index.constraint {
        for a in columns {
            out.push(NewDepend {
                dependent: ObjectAddress::constraint(con.oid),
                referenced: ObjectAddress::column(index.table_oid, a),
                deptype: DependType::Auto,
            });
        }
        out.push(NewDepend {
            dependent: ObjectAddress::relation(index.oid),
            referenced: ObjectAddress::constraint(con.oid),
            deptype: DependType::Internal,
        });
    } else {
        for a in columns {
            out.push(NewDepend {
                dependent: ObjectAddress::relation(index.oid),
                referenced: ObjectAddress::column(index.table_oid, a),
                deptype: DependType::Auto,
            });
        }
    }
    out
}

/// 解析済みの既定値の式から、`regclass` 定数が指すリレーションの OID を集める
/// （`BoundCreateTable.default_refs` の元。analyzer が呼ぶ）。重複なし。
/// 列・副問い合わせを含む式（既定値には現れない）は空の結果を返す。
pub fn collect_regclass_refs(expr: &BoundExpr) -> Vec<Oid> {
    if expr.any(&mut |e| matches!(e.kind, ExprKind::Column(_) | ExprKind::SubLink { .. })) {
        return Vec::new();
    }
    let mut out: Vec<Oid> = Vec::new();
    expr.walk(&mut |e| {
        if let ExprKind::Literal(Datum::Oid(o)) = &e.kind
            && e.ty.oid == oid::REGCLASS
            && !out.contains(o)
        {
            out.push(*o);
        }
        true
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::query::BoundExpr;
    use crate::catalog::IndexColumn;
    use crate::catalog::store::{NewConstraint, NewIndexColumn};
    use crate::error::Span;
    use crate::expr::Expr;
    use crate::storage::BuildStats;
    use crate::types::{Datum, SqlType};

    fn regclass(oid: Oid) -> BoundExpr {
        Expr::new(
            ExprKind::Literal(Datum::Oid(oid)),
            SqlType::REGCLASS,
            Span::default(),
        )
    }

    #[test]
    fn default_value_depends_links_defaults_to_relations() {
        let attrdefs = [(1, 7001), (3, 7003)];
        let deps =
            default_value_depends(&attrdefs, &[(1, 5001)], &[(3, 5002), (3, 5002), (2, 5003)]);
        assert_eq!(
            deps,
            vec![
                NewDepend {
                    dependent: ObjectAddress::attrdef(7001),
                    referenced: ObjectAddress::relation(5001),
                    deptype: DependType::Normal,
                },
                NewDepend {
                    dependent: ObjectAddress::attrdef(7003),
                    referenced: ObjectAddress::relation(5002),
                    deptype: DependType::Normal,
                },
            ],
            "duplicates collapse and a column without a default is ignored"
        );
        assert!(default_value_depends(&[], &[], &[]).is_empty());
    }

    fn new_index(columns: &[i16], constraint: bool) -> NewIndex {
        NewIndex {
            oid: 100,
            name: "i".into(),
            namespace: 2200,
            owner: 10,
            table_oid: 50,
            relfilenode: 100,
            columns: columns
                .iter()
                .map(|&a| NewIndexColumn {
                    name: format!("c{a}"),
                    column: IndexColumn {
                        attnum: a,
                        opclass: 1978,
                        opfamily: 1976,
                        descending: false,
                        nulls_first: false,
                    },
                    ty: SqlType::INT4,
                })
                .collect(),
            unique: constraint,
            primary: false,
            constraint: constraint.then(|| NewConstraint {
                oid: 101,
                name: "i".into(),
            }),
            stats: BuildStats::default(),
        }
    }

    #[test]
    fn index_depends_follow_the_table_in_07() {
        // 通常の索引: 索引 → 各キー列（同じ列は 1 行）。
        let deps = index_depends(&new_index(&[1, 2, 1], false));
        assert_eq!(deps.len(), 2);
        assert!(
            deps.iter()
                .all(|d| d.deptype == DependType::Auto
                    && d.dependent == ObjectAddress::relation(100))
        );
        assert_eq!(deps[0].referenced, ObjectAddress::column(50, 1));
        assert_eq!(deps[1].referenced, ObjectAddress::column(50, 2));
        // 制約の索引: 制約 → 各キー列（a）、索引 → 制約（i）。
        let deps = index_depends(&new_index(&[1, 2], true));
        assert_eq!(deps.len(), 3);
        assert_eq!(deps[0].dependent, ObjectAddress::constraint(101));
        assert_eq!(deps[0].deptype, DependType::Auto);
        assert_eq!(
            deps[2],
            NewDepend {
                dependent: ObjectAddress::relation(100),
                referenced: ObjectAddress::constraint(101),
                deptype: DependType::Internal,
            }
        );
    }

    /// `index_depends` は `CatalogStore` が実際に書く `pg_depend` の行と一致する。
    #[test]
    fn index_depends_match_what_the_store_writes() {
        use crate::analyzer::query::{BoundCreateTable, BoundDdl, IndexConstraintKind};
        use crate::ddl::testkit::{Harness, col, create_table_ddl, key};
        let mut h = Harness::new();
        let mut ct: BoundCreateTable = create_table_ddl(
            "dp1",
            vec![
                col("a", 1, SqlType::INT4),
                col("b", 2, SqlType::INT4),
                col("c", 3, SqlType::TEXT),
            ],
        );
        ct.constraints = vec![
            key(IndexConstraintKind::PrimaryKey, None, &[1, 2]),
            key(IndexConstraintKind::Unique, None, &[3]),
        ];
        h.exec_w(BoundDdl::CreateTable(ct)).unwrap();
        h.commit();
        let t = h.table("dp1").unwrap();
        let snap = h.tc.cluster.txn_manager().snapshot(None, 0);
        for i in &t.indexes {
            let ni = NewIndex {
                oid: i.oid,
                name: i.name.clone(),
                namespace: i.namespace,
                owner: h.role,
                table_oid: t.oid,
                relfilenode: i.oid,
                columns: i
                    .columns
                    .iter()
                    .map(|c| NewIndexColumn {
                        name: String::new(),
                        column: c.clone(),
                        ty: SqlType::INT4,
                    })
                    .collect(),
                unique: true,
                primary: i.primary,
                constraint: i.constraint.as_ref().map(|c| NewConstraint {
                    oid: c.oid,
                    name: c.name.clone(),
                }),
                stats: BuildStats::default(),
            };
            let mut actual: Vec<NewDepend> = Vec::new();
            let mut sources = vec![ObjectAddress::relation(i.oid)];
            sources.extend(
                i.constraint
                    .iter()
                    .map(|c| ObjectAddress::constraint(c.oid)),
            );
            for src in sources {
                for r in h.db.catalog.references_of(&snap, src).unwrap() {
                    actual.push(NewDepend {
                        dependent: r.dependent,
                        referenced: r.referenced,
                        deptype: r.deptype,
                    });
                }
            }
            let mut expected = index_depends(&ni);
            let key = |d: &NewDepend| format!("{d:?}");
            actual.sort_by_key(key);
            expected.sort_by_key(key);
            assert_eq!(actual, expected, "index {}", i.name);
        }
    }

    #[test]
    fn regclass_constants_are_collected_once() {
        assert_eq!(collect_regclass_refs(&regclass(42)), vec![42]);
        let int = Expr::new(
            ExprKind::Literal(Datum::Oid(7)),
            SqlType::of(oid::OID),
            Span::default(),
        );
        assert!(
            collect_regclass_refs(&int).is_empty(),
            "an oid is not a regclass"
        );
        let int4 = Expr::new(
            ExprKind::Literal(Datum::Int4(1)),
            SqlType::INT4,
            Span::default(),
        );
        assert!(collect_regclass_refs(&int4).is_empty());
    }
}
