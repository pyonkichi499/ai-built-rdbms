//! 外部キーの解析（`m5/07-foreign-key.md` §4.3・§6.6）。
//!
//! `bind_foreign_key` は `ForeignKeySpec` を `BoundForeignKey` にする。列の解決、一意インデックスの選択、
//! 型の分類（FK-D15）、`eq_ops`（§3.3）、自動命名（FK-D10）を行う。実行（`create_foreign_key`）は FK-1b が持つ。

use std::collections::HashSet;
use std::sync::Arc;

use crate::analyzer::bound::{BoundIndexConstraint, IndexConstraintKind};
use crate::catalog::builtin::CastContext;
use crate::catalog::fk::{FkAction, FkEqOps, FkMatch};
use crate::catalog::naming::{NameLookup, choose_constraint_name};
use crate::catalog::opclass::{AMOPS, default_opclass, family_of_type};
use crate::catalog::{CatalogReader, ColumnDef, RelKind, TableDef};
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{FkActionSpec, FkMatchSpec, ForeignKeySpec, Ident};
use crate::types::{Oid, SqlType, format_type, oid};

/// 解析済みの FK（CREATE TABLE と ALTER の両方）。名前は解析で決める。
#[derive(Clone, Debug)]
pub struct BoundForeignKey {
    pub name: String,
    /// 子の attnum（CREATE TABLE では列の並び順 + 1）。
    pub child_cols: Vec<i16>,
    pub parent: BoundFkParent,
    /// 親の attnum。空にならない（PK の省略は解析で解決）。書いた順のまま。
    pub parent_cols: Vec<i16>,
    pub match_type: FkMatch,
    pub on_update: FkAction,
    pub on_delete: FkAction,
    /// 子の attnum。`ON DELETE SET ... (cols)`。空 = FK の全列。
    pub set_cols: Vec<i16>,
    pub eq_ops: Vec<FkEqOps>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum BoundFkParent {
    /// 既存の表。`index` は FK が使う一意インデックス。
    Existing {
        oid: Oid,
        schema: String,
        name: String,
        index: Oid,
    },
    /// 作成中の表自身。`index_constraint` は `BoundCreateTable.constraints` の添字。
    SelfTable { index_constraint: usize },
}

/// `ALTER TABLE ... ADD FOREIGN KEY` の解析結果。
#[derive(Clone, Debug)]
pub struct BoundAddForeignKey {
    pub child_oid: Oid,
    pub child_schema: String,
    pub child_name: String,
    pub fk: BoundForeignKey,
}

/// `ALTER TABLE ... DROP CONSTRAINT` の解析結果。
#[derive(Clone, Debug)]
pub struct BoundDropConstraint {
    pub table_oid: Oid,
    pub schema: String,
    pub table: String,
    pub name: String,
    pub if_exists: bool,
    pub cascade: bool,
}

/// 子の対象。CREATE TABLE では、作成中の列と PK / UNIQUE の一覧から解決する。
#[derive(Clone, Copy, Debug)]
pub enum FkChild<'a> {
    Existing(&'a TableDef),
    New {
        schema: &'a str,
        name: &'a str,
        columns: &'a [ColumnDef],
        index_constraints: &'a [BoundIndexConstraint],
    },
}

/// 親の表の見え方（既存の表も作成中の表も同じ形で扱う）。
struct ParentView<'a> {
    columns: &'a [ColumnDef],
    /// 一意インデックス候補: (識別子, attnum の並び, 演算子族の並び)。
    uniques: Vec<(UniqueRef, Vec<i16>, Vec<Oid>)>,
    /// 列を省略したときの PK。
    primary: Option<usize>,
    display_name: String,
}

#[derive(Clone, Copy)]
enum UniqueRef {
    Index(Oid),
    Constraint(usize),
}

struct Lookup<'a>(&'a dyn CatalogReader);

impl NameLookup for Lookup<'_> {
    fn relation_exists(&self, _nsp: Oid, _name: &str) -> Result<bool> {
        Ok(false)
    }
    fn constraint_exists(&self, nsp: Oid, name: &str) -> Result<bool> {
        self.0.constraint_name_exists(nsp, name)
    }
}

fn namespace_of_schema(schema: &str) -> Oid {
    match schema {
        "pg_catalog" => 11,
        _ => 2200,
    }
}

fn convert_action(a: &FkActionSpec) -> (FkAction, Vec<&Ident>) {
    match a {
        FkActionSpec::NoAction => (FkAction::NoAction, Vec::new()),
        FkActionSpec::Restrict => (FkAction::Restrict, Vec::new()),
        FkActionSpec::Cascade => (FkAction::Cascade, Vec::new()),
        FkActionSpec::SetNull(c) => (FkAction::SetNull, c.iter().collect()),
        FkActionSpec::SetDefault(c) => (FkAction::SetDefault, c.iter().collect()),
    }
}

fn is_integer(t: Oid) -> bool {
    matches!(t, oid::INT2 | oid::INT4 | oid::INT8)
}

/// FK-D15: 子の型から親の型へ、キーとして比べられるか。
fn types_compatible(catalog: &dyn CatalogReader, child: Oid, parent: Oid) -> bool {
    if child == parent || (is_integer(child) && is_integer(parent)) {
        return true;
    }
    if let (Some(c), Some(p)) = (default_opclass(child), default_opclass(parent))
        && c.input_type == p.input_type
    {
        return true;
    }
    catalog
        .find_cast(child, parent)
        .is_some_and(|c| c.context == CastContext::Implicit)
}

fn norm_type(t: Oid) -> Oid {
    default_opclass(t).map_or(t, |c| c.input_type)
}

fn eq_op(family: Oid, left: Oid, right: Oid) -> Option<Oid> {
    AMOPS
        .iter()
        .find(|a| a.family == family && a.strategy == 3 && a.left == left && a.right == right)
        .map(|a| a.operator)
}

/// §3.3。`family` は親のインデックス列の演算子族。
fn eq_ops_for(family: Oid, parent_ty: Oid, child_ty: Oid) -> Result<FkEqOps> {
    let (p, f) = (norm_type(parent_ty), norm_type(child_ty));
    let missing = || {
        Error::internal(format!(
            "no equality operator for foreign key types {child_ty} and {parent_ty}"
        ))
    };
    let pp = eq_op(family, p, p).ok_or_else(missing)?;
    Ok(match eq_op(family, p, f) {
        Some(pf) => FkEqOps {
            pf,
            pp,
            ff: eq_op(family, f, f).unwrap_or(pp),
        },
        None => FkEqOps { pf: pp, pp, ff: pp },
    })
}

fn resolve_cols(names: &[Ident], columns: &[ColumnDef], span: Span) -> Result<Vec<i16>> {
    names
        .iter()
        .map(|n| {
            columns
                .iter()
                .find(|c| c.name == n.value)
                .map(|c| c.attnum)
                .ok_or_else(|| {
                    Error::new(
                        sqlstate::UNDEFINED_COLUMN,
                        format!(
                            "column \"{}\" referenced in foreign key constraint does not exist",
                            n.value
                        ),
                    )
                    .with_span(span)
                })
        })
        .collect()
}

fn same_set(a: &[i16], b: &[i16]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.contains(x))
}

fn column_of(columns: &[ColumnDef], attnum: i16) -> Result<&ColumnDef> {
    columns
        .iter()
        .find(|c| c.attnum == attnum)
        .ok_or_else(|| Error::internal(format!("attnum {attnum} not found")))
}

/// 親の表を解決する（§6.6 の 4〜6）。`Ok(None)` は作成中の表自身。
fn resolve_parent(
    catalog: &dyn CatalogReader,
    child: &FkChild<'_>,
    spec: &ForeignKeySpec,
) -> Result<Option<Arc<TableDef>>> {
    let parts = &spec.ref_table.parts;
    let name = spec.ref_table.name().value.as_str();
    let schema = (parts.len() >= 2).then(|| parts[parts.len() - 2].value.as_str());
    if let FkChild::New {
        schema: cs,
        name: cn,
        ..
    } = child
        && name == *cn
        && schema.is_none_or(|s| s == *cs)
    {
        return Ok(None);
    }
    let does_not_exist = || {
        Error::new(
            sqlstate::UNDEFINED_TABLE,
            format!("relation \"{name}\" does not exist"),
        )
        .with_span(spec.ref_table.span)
    };
    match catalog.relation_kind(schema, name)? {
        None => return Err(does_not_exist()),
        Some((_, RelKind::Index | RelKind::Sequence)) => {
            return Err(Error::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("referenced relation \"{name}\" is not a table"),
            )
            .with_span(spec.ref_table.span));
        }
        Some(_) => {}
    }
    let t = catalog.table(schema, name)?.ok_or_else(does_not_exist)?;
    if t.is_system_catalog() {
        return Err(Error::new(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            format!("permission denied: \"{}\" is a system catalog", t.name),
        )
        .with_span(spec.ref_table.span));
    }
    Ok(Some(t))
}

/// 制約名を決める（FK-D10。§6.6）。
fn choose_name(
    catalog: &dyn CatalogReader,
    child: &FkChild<'_>,
    spec: &ForeignKeySpec,
    child_col_names: &[&str],
    taken: &mut Vec<String>,
) -> Result<String> {
    let (schema, table) = match child {
        FkChild::Existing(t) => (t.schema.as_str(), t.name.as_str()),
        FkChild::New { schema, name, .. } => (*schema, *name),
    };
    if let Some(n) = &spec.name {
        let mut dup = taken.contains(&n.value);
        if let FkChild::Existing(t) = child {
            dup |= t.checks.iter().any(|c| c.name == n.value)
                || t.foreign_keys.iter().any(|f| f.name == n.value)
                || t.indexes
                    .iter()
                    .any(|i| i.constraint.as_ref().is_some_and(|c| c.name == n.value));
        }
        if dup {
            return Err(Error::new(
                sqlstate::DUPLICATE_OBJECT,
                format!(
                    "constraint \"{}\" for relation \"{table}\" already exists",
                    n.value
                ),
            )
            .with_span(spec.span));
        }
        taken.push(n.value.clone());
        return Ok(n.value.clone());
    }
    let nsp = match child {
        FkChild::Existing(t) => t.namespace,
        FkChild::New { .. } => namespace_of_schema(schema),
    };
    let joined = child_col_names.join("_");
    let set: HashSet<String> = taken.iter().cloned().collect();
    let name = choose_constraint_name(table, Some(&joined), "fkey", nsp, &Lookup(catalog), &set)?;
    taken.push(name.clone());
    Ok(name)
}

/// 解析の本体。順序とエラーは §6.6 の表。
#[allow(clippy::too_many_lines)]
pub fn bind_foreign_key(
    catalog: &dyn CatalogReader,
    child: FkChild<'_>,
    spec: &ForeignKeySpec,
    taken: &mut Vec<String>,
) -> Result<BoundForeignKey> {
    // 2, 3: DEFERRABLE / INITIALLY DEFERRED / NOT VALID。
    if spec.deferrable == Some(false) && spec.initially_deferred == Some(true) {
        return Err(Error::new(
            sqlstate::SYNTAX_ERROR,
            "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
        )
        .with_span(spec.span));
    }
    if spec.deferrable == Some(true) {
        return Err(
            Error::not_supported("DEFERRABLE constraints are not supported yet")
                .with_span(spec.span),
        );
    }
    if spec.initially_deferred == Some(true) {
        return Err(
            Error::not_supported("INITIALLY DEFERRED constraints are not supported yet")
                .with_span(spec.span),
        );
    }
    if spec.not_valid {
        return Err(
            Error::not_supported("NOT VALID foreign keys are not supported yet")
                .with_span(spec.span),
        );
    }
    let match_type = match spec.match_type {
        FkMatchSpec::Simple => FkMatch::Simple,
        FkMatchSpec::Full => FkMatch::Full,
        FkMatchSpec::Partial => {
            return Err(
                Error::not_supported("MATCH PARTIAL not yet implemented").with_span(spec.span)
            );
        }
    };

    // 4〜6: 親。
    let parent_table = resolve_parent(catalog, &child, spec)?;

    let child_columns: &[ColumnDef] = match &child {
        FkChild::Existing(t) => &t.columns,
        FkChild::New { columns, .. } => columns,
    };
    let child_table_name = match &child {
        FkChild::Existing(t) => t.name.clone(),
        FkChild::New { name, .. } => (*name).to_owned(),
    };

    // 親の見え方。
    #[allow(clippy::single_match_else)]
    let view = match &parent_table {
        Some(t) => ParentView {
            columns: &t.columns,
            uniques: t
                .indexes
                .iter()
                .filter(|i| i.unique)
                .map(|i| {
                    (
                        UniqueRef::Index(i.oid),
                        i.columns.iter().map(|c| c.attnum).collect(),
                        i.columns.iter().map(|c| c.opfamily).collect(),
                    )
                })
                .collect(),
            primary: t
                .indexes
                .iter()
                .filter(|i| i.unique)
                .position(|i| i.primary),
            display_name: t.name.clone(),
        },
        None => {
            let FkChild::New {
                columns,
                index_constraints,
                ..
            } = &child
            else {
                return Err(Error::internal("self parent without a new table"));
            };
            ParentView {
                columns,
                uniques: index_constraints
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let fams = c
                            .columns
                            .iter()
                            .map(|a| {
                                columns
                                    .iter()
                                    .find(|col| col.attnum == *a)
                                    .and_then(|col| family_of_type(col.ty.oid))
                                    .unwrap_or(0)
                            })
                            .collect();
                        (UniqueRef::Constraint(i), c.columns.clone(), fams)
                    })
                    .collect(),
                primary: index_constraints
                    .iter()
                    .position(|c| c.kind == IndexConstraintKind::PrimaryKey),
                display_name: child_table_name.clone(),
            }
        }
    };

    // 7: 列名。
    let child_cols = resolve_cols(&spec.columns, child_columns, spec.span)?;
    let mut parent_cols = resolve_cols(&spec.ref_columns, view.columns, spec.span)?;
    let mut chosen: Option<usize> = None;

    // 8: 親の列の省略 = PK。
    if spec.ref_columns.is_empty() {
        let Some(p) = view.primary else {
            return Err(Error::new(
                sqlstate::UNDEFINED_OBJECT,
                format!(
                    "there is no primary key for referenced table \"{}\"",
                    view.display_name
                ),
            )
            .with_span(spec.ref_table.span));
        };
        parent_cols.clone_from(&view.uniques[p].1);
        chosen = Some(p);
    }

    // 9, 10。
    if child_cols.len() != parent_cols.len() {
        return Err(Error::new(
            sqlstate::INVALID_FOREIGN_KEY,
            "number of referencing and referenced columns for foreign key disagree",
        )
        .with_span(spec.span));
    }
    for (i, a) in parent_cols.iter().enumerate() {
        if parent_cols[..i].contains(a) {
            return Err(Error::new(
                sqlstate::INVALID_FOREIGN_KEY,
                "foreign key referenced-columns list must not contain duplicates",
            )
            .with_span(spec.span));
        }
    }

    // 11: 一意インデックス。
    let chosen = match chosen {
        Some(c) => c,
        None => view
            .uniques
            .iter()
            .position(|(_, cols, _)| same_set(cols, &parent_cols))
            .ok_or_else(|| {
                Error::new(
                    sqlstate::INVALID_FOREIGN_KEY,
                    format!(
                        "there is no unique constraint matching given keys for referenced table \"{}\"",
                        view.display_name
                    ),
                )
                .with_span(spec.span)
            })?,
    };
    let (uref, index_cols, index_fams) = &view.uniques[chosen];

    // 名前は 12 より前に決める（12 のメッセージが名前を使う）。
    let child_names: Vec<&str> = child_cols
        .iter()
        .map(|a| column_of(child_columns, *a).map(|c| c.name.as_str()))
        .collect::<Result<_>>()?;
    let name = choose_name(catalog, &child, spec, &child_names, taken)?;

    // 12: 型と eq_ops。
    let mut eq_ops = Vec::with_capacity(child_cols.len());
    for (c, p) in child_cols.iter().zip(&parent_cols) {
        let cc = column_of(child_columns, *c)?;
        let pc = column_of(view.columns, *p)?;
        if !types_compatible(catalog, cc.ty.oid, pc.ty.oid) {
            taken.retain(|n| *n != name);
            return Err(Error::new(
                sqlstate::DATATYPE_MISMATCH,
                format!("foreign key constraint \"{name}\" cannot be implemented"),
            )
            .with_detail(format!(
                "Key columns \"{}\" and \"{}\" are of incompatible types: {} and {}.",
                cc.name,
                pc.name,
                format_type(SqlType {
                    oid: cc.ty.oid,
                    typmod: cc.ty.typmod
                }),
                format_type(SqlType {
                    oid: pc.ty.oid,
                    typmod: pc.ty.typmod
                }),
            ))
            .with_span(spec.span));
        }
        let pos = index_cols.iter().position(|a| a == p).unwrap_or(0);
        let family = index_fams
            .get(pos)
            .copied()
            .filter(|f| *f != 0)
            .or_else(|| family_of_type(pc.ty.oid))
            .unwrap_or(0);
        eq_ops.push(eq_ops_for(family, pc.ty.oid, cc.ty.oid)?);
    }

    // 13: SET NULL / SET DEFAULT の列。
    let (on_update, _) = convert_action(&spec.on_update);
    let (on_delete, set_names) = convert_action(&spec.on_delete);
    let mut set_cols = Vec::new();
    for n in set_names {
        let attnum = child_columns
            .iter()
            .find(|c| c.name == n.value)
            .map(|c| c.attnum)
            .filter(|a| child_cols.contains(a));
        let Some(a) = attnum else {
            return Err(Error::new(
                sqlstate::INVALID_COLUMN_REFERENCE,
                format!(
                    "column \"{}\" referenced in ON DELETE SET action must be part of foreign key",
                    n.value
                ),
            )
            .with_span(n.span));
        };
        set_cols.push(a);
    }

    let parent = match (&parent_table, uref) {
        (Some(t), UniqueRef::Index(index)) => BoundFkParent::Existing {
            oid: t.oid,
            schema: t.schema.clone(),
            name: t.name.clone(),
            index: *index,
        },
        (None, UniqueRef::Constraint(i)) => BoundFkParent::SelfTable {
            index_constraint: *i,
        },
        _ => return Err(Error::internal("inconsistent foreign key parent")),
    };
    Ok(BoundForeignKey {
        name,
        child_cols,
        parent,
        parent_cols,
        match_type,
        on_update,
        on_delete,
        set_cols,
        eq_ops,
        span: spec.span,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::{FakeCatalog, TableBuilder};
    use crate::sql::ast::{ObjectName, Statement, TableConstraintKind, TableElement};
    use crate::sql::parser::parse;

    fn fk_spec(sql: &str) -> ForeignKeySpec {
        let stmts = parse(sql).unwrap();
        let Statement::CreateTable(ct) = &stmts[0] else {
            panic!("not create table")
        };
        for e in &ct.elements {
            if let TableElement::Constraint(tc) = e
                && let TableConstraintKind::ForeignKey(s) = &tc.kind
            {
                return s.clone();
            }
        }
        panic!("no fk")
    }

    fn catalog() -> FakeCatalog {
        let mut cat = FakeCatalog::new("t");
        cat.add(
            &TableBuilder::new("p")
                .column_nn("id", SqlType::INT4)
                .primary_key(&["id"]),
        );
        cat.add(
            &TableBuilder::new("p2")
                .column_nn("a", SqlType::INT4)
                .column_nn("b", SqlType::INT4)
                .primary_key(&["a", "b"]),
        );
        cat.add(&TableBuilder::new("np").column("id", SqlType::INT4));
        cat.add(
            &TableBuilder::new("pt")
                .column_nn("id", SqlType::TEXT)
                .primary_key(&["id"]),
        );
        cat.add(
            &TableBuilder::new("pn")
                .column_nn("id", SqlType::NUMERIC)
                .primary_key(&["id"]),
        );
        cat.add(
            &TableBuilder::new("c")
                .column("id", SqlType::INT4)
                .column("pid", SqlType::INT8)
                .column("a", SqlType::INT4)
                .column("b", SqlType::INT4)
                .column("t", SqlType::TEXT),
        );
        cat
    }

    fn bind(sql: &str) -> Result<BoundForeignKey> {
        let cat = catalog();
        let c = cat.table(None, "c").unwrap().unwrap();
        bind_foreign_key(&cat, FkChild::Existing(&c), &fk_spec(sql), &mut Vec::new())
    }

    fn ddl(clause: &str) -> String {
        format!("create table x (q int, {clause})")
    }

    fn err(clause: &str) -> Error {
        bind(&ddl(clause)).unwrap_err()
    }

    #[test]
    fn pk_is_default_and_name_is_automatic() {
        let fk = bind(&ddl("foreign key (pid) references p")).unwrap();
        assert_eq!(fk.name, "c_pid_fkey");
        assert_eq!(fk.child_cols, vec![2]);
        assert_eq!(fk.parent_cols, vec![1]);
        assert!(matches!(fk.parent, BoundFkParent::Existing { .. }));
        // bigint 子 → int 親: pf = int48eq(15), pp = int4eq(96), ff = int8eq(410)。
        assert_eq!(
            fk.eq_ops,
            vec![FkEqOps {
                pf: 15,
                pp: 96,
                ff: 410
            }]
        );
    }

    #[test]
    fn actions_and_match_are_converted() {
        let fk = bind(&ddl(
            "foreign key (a, b) references p2 (a, b) match full on update cascade on delete set null (a)",
        ))
        .unwrap();
        assert_eq!(fk.match_type, FkMatch::Full);
        assert_eq!(fk.on_update, FkAction::Cascade);
        assert_eq!(fk.on_delete, FkAction::SetNull);
        assert_eq!(fk.set_cols, vec![3]);
    }

    #[test]
    fn column_order_is_a_set_match() {
        let fk = bind(&ddl("foreign key (b, a) references p2 (b, a)")).unwrap();
        assert_eq!(fk.parent_cols, vec![2, 1]);
    }

    #[test]
    fn text_and_numeric_ops() {
        let fk = bind(&ddl("foreign key (t) references pt")).unwrap();
        assert_eq!(
            fk.eq_ops,
            vec![FkEqOps {
                pf: 98,
                pp: 98,
                ff: 98
            }]
        );
        let fk = bind(&ddl("foreign key (a) references pn")).unwrap();
        assert_eq!(
            fk.eq_ops,
            vec![FkEqOps {
                pf: 1752,
                pp: 1752,
                ff: 1752
            }]
        );
    }

    #[test]
    fn explicit_name_is_kept_and_duplicate_taken_is_42710() {
        let cat = catalog();
        let c = cat.table(None, "c").unwrap().unwrap();
        let spec = fk_spec(&ddl("constraint k foreign key (pid) references p"));
        let mut taken = vec!["k".to_owned()];
        let e = bind_foreign_key(&cat, FkChild::Existing(&c), &spec, &mut taken).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DUPLICATE_OBJECT);
        let mut taken = Vec::new();
        let fk = bind_foreign_key(&cat, FkChild::Existing(&c), &spec, &mut taken).unwrap();
        assert_eq!(fk.name, "k");
    }

    #[test]
    fn auto_name_avoids_taken() {
        let cat = catalog();
        let c = cat.table(None, "c").unwrap().unwrap();
        let spec = fk_spec(&ddl("foreign key (pid) references p"));
        let mut taken = Vec::new();
        let a = bind_foreign_key(&cat, FkChild::Existing(&c), &spec, &mut taken).unwrap();
        let b = bind_foreign_key(&cat, FkChild::Existing(&c), &spec, &mut taken).unwrap();
        assert_eq!(
            (a.name.as_str(), b.name.as_str()),
            ("c_pid_fkey", "c_pid_fkey1")
        );
    }

    #[test]
    fn errors_follow_the_table() {
        let e = err("foreign key (pid) references nope");
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_TABLE);
        let e = err("foreign key (zz) references p");
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_COLUMN);
        assert_eq!(
            e.message,
            "column \"zz\" referenced in foreign key constraint does not exist"
        );
        let e = err("foreign key (pid) references np");
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        let e = err("foreign key (pid, a) references p");
        assert_eq!(e.sqlstate, sqlstate::INVALID_FOREIGN_KEY);
        let e = err("foreign key (a, b) references p2 (a, a)");
        assert_eq!(
            e.message,
            "foreign key referenced-columns list must not contain duplicates"
        );
        let e = err("foreign key (a) references p2 (a)");
        assert_eq!(
            e.message,
            "there is no unique constraint matching given keys for referenced table \"p2\""
        );
        let e = err("foreign key (a) references pn on delete set null (b)");
        assert_eq!(e.sqlstate, sqlstate::INVALID_COLUMN_REFERENCE);
    }

    #[test]
    fn incompatible_types_report_the_first_pair() {
        let e = err("foreign key (t) references p");
        assert_eq!(e.sqlstate, sqlstate::DATATYPE_MISMATCH);
        assert_eq!(
            e.message,
            "foreign key constraint \"c_t_fkey\" cannot be implemented"
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("Key columns \"t\" and \"id\" are of incompatible types: text and integer.")
        );
    }

    #[test]
    fn deferrable_and_initially_deferred_are_rejected() {
        let e = err("foreign key (pid) references p deferrable");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        let e = err("foreign key (pid) references p initially deferred");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        let e = err("foreign key (pid) references p not deferrable initially deferred");
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        let e = err("foreign key (pid) references p deferrable initially deferred");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert!(
            bind(&ddl(
                "foreign key (pid) references p not deferrable initially immediate"
            ))
            .is_ok()
        );
    }

    #[test]
    fn system_catalog_parent_is_42501() {
        let mut cat = catalog();
        let mut def = (*cat.table(None, "p").unwrap().unwrap()).clone();
        def.oid = 1259;
        def.name = "pg_class".to_owned();
        cat.put_table(Arc::new(def));
        let c = cat.table(None, "c").unwrap().unwrap();
        let spec = fk_spec(&ddl("foreign key (pid) references pg_class"));
        let e = bind_foreign_key(&cat, FkChild::Existing(&c), &spec, &mut Vec::new()).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
        assert_eq!(
            e.message,
            "permission denied: \"pg_class\" is a system catalog"
        );
    }

    #[test]
    fn self_reference_in_create_table_uses_the_new_constraint() {
        let cat = catalog();
        let spec = fk_spec("create table s (id int, pid int, foreign key (pid) references s (id))");
        let cols = vec![
            ColumnDef {
                name: "id".into(),
                attnum: 1,
                ty: SqlType::INT4,
                not_null: true,
                default: None,
                identity: None,
            },
            ColumnDef {
                name: "pid".into(),
                attnum: 2,
                ty: SqlType::INT4,
                not_null: false,
                default: None,
                identity: None,
            },
        ];
        let idx_cons = vec![BoundIndexConstraint {
            name: None,
            kind: IndexConstraintKind::PrimaryKey,
            columns: vec![1],
            options: Vec::new(),
            span: Span::default(),
        }];
        let child = || FkChild::New {
            schema: "public",
            name: "s",
            columns: &cols,
            index_constraints: &idx_cons,
        };
        let fk = bind_foreign_key(&cat, child(), &spec, &mut Vec::new()).unwrap();
        assert_eq!(fk.name, "s_pid_fkey");
        assert!(matches!(
            fk.parent,
            BoundFkParent::SelfTable {
                index_constraint: 0
            }
        ));
        // PK が無ければ 8。
        let spec2 = fk_spec("create table s (id int, pid int, foreign key (pid) references s)");
        let no_cons: Vec<BoundIndexConstraint> = Vec::new();
        let e = bind_foreign_key(
            &cat,
            FkChild::New {
                schema: "public",
                name: "s",
                columns: &cols,
                index_constraints: &no_cons,
            },
            &spec2,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::UNDEFINED_OBJECT);
        let _ = ObjectName {
            parts: Vec::new(),
            span: Span::default(),
        };
    }
}
