//! 依存関係（`pg_depend`）と DROP の計画（`m4/07-catalog-ddl.md` §4.3、§5.8、§6.5）。
//!
//! PostgreSQL の `findDependentObjects` / `reportDependentObjects` を、`a`（AUTO）/ `i`（INTERNAL）/
//! `n`（NORMAL）の 3 種に絞って移したもの。`ddl/` と `08` のシーケンスが [`plan_drop`] を呼び、
//! 行の削除は `CatalogStore::drop_objects` が行う。

use std::collections::HashMap;

use super::RelKind;
use super::store::{CatalogStore, RelationRow};
use crate::deparse::ident::{quote_identifier, quote_qualified};
use crate::error::{Error, Result, sqlstate};
use crate::storage::smgr::RelFileLocator;
use crate::txn::Snapshot;
use crate::types::Oid;

/// 依存元・依存先のカタログの OID（`pg_depend.classid` / `refclassid`）。
pub mod classes {
    use crate::types::Oid;

    pub const RELATION: Oid = 1259;
    pub const CONSTRAINT: Oid = 2606;
    pub const ATTRDEF: Oid = 2604;
}

/// `(カタログ, OID, 列番号)`。列番号 0 はオブジェクト全体。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ObjectAddress {
    pub class_id: Oid,
    pub obj_id: Oid,
    pub obj_sub: i32,
}

impl ObjectAddress {
    /// `(pg_class, oid, 0)`。
    pub fn relation(oid: Oid) -> Self {
        ObjectAddress {
            class_id: classes::RELATION,
            obj_id: oid,
            obj_sub: 0,
        }
    }

    /// `(pg_class, oid, attnum)`。
    pub fn column(oid: Oid, attnum: i16) -> Self {
        ObjectAddress {
            class_id: classes::RELATION,
            obj_id: oid,
            obj_sub: i32::from(attnum),
        }
    }

    pub fn constraint(oid: Oid) -> Self {
        ObjectAddress {
            class_id: classes::CONSTRAINT,
            obj_id: oid,
            obj_sub: 0,
        }
    }

    pub fn attrdef(oid: Oid) -> Self {
        ObjectAddress {
            class_id: classes::ATTRDEF,
            obj_id: oid,
            obj_sub: 0,
        }
    }

    /// 列番号を無視した識別子（DROP の計画の集合のキー）。
    pub(crate) fn key_pair(self) -> (Oid, Oid) {
        (self.class_id, self.obj_id)
    }
}

/// `pg_depend.deptype`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DependType {
    /// `n`: 依存先を消すには依存元も消す必要がある（RESTRICT なら拒否、CASCADE なら連鎖）。
    Normal,
    /// `a`: 依存先が消えたら依存元も黙って消える。
    Auto,
    /// `i`: 依存元は依存先の一部。単独では消せず、依存先と一緒に消える。
    Internal,
}

impl DependType {
    pub fn code(self) -> char {
        match self {
            DependType::Normal => 'n',
            DependType::Auto => 'a',
            DependType::Internal => 'i',
        }
    }

    pub fn from_code(c: char) -> Option<DependType> {
        match c {
            'n' => Some(DependType::Normal),
            'a' => Some(DependType::Auto),
            'i' => Some(DependType::Internal),
            _ => None,
        }
    }
}

/// 書き込む依存 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDepend {
    pub dependent: ObjectAddress,
    pub referenced: ObjectAddress,
    pub deptype: DependType,
}

/// 読み取った依存 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependRow {
    pub dependent: ObjectAddress,
    pub referenced: ObjectAddress,
    pub deptype: DependType,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropBehavior {
    Restrict,
    Cascade,
}

/// 消すオブジェクトの種類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropKind {
    Table,
    Index,
    Sequence,
    Constraint,
    AttrDefault,
}

/// 消す 1 つのオブジェクト。
#[derive(Clone, Debug)]
pub struct DropItem {
    pub addr: ObjectAddress,
    pub kind: DropKind,
    /// `table t`、`index t_pkey`、`constraint t_pkey on table t`、
    /// `default value for column id of table t` など。
    pub description: String,
    /// Table / Index / Sequence のファイル。ほかは `None`。
    pub locator: Option<RelFileLocator>,
    /// Constraint / `AttrDefault` が属する表と列（AttrDefault の `atthasdef` を戻すため）。
    pub owner_table: Option<Oid>,
    pub attnum: Option<i16>,
}

#[derive(Clone, Debug, Default)]
pub struct DropPlan {
    /// 重複のない、消すオブジェクトすべて。ルートが先、その後は発見順。
    pub items: Vec<DropItem>,
    /// CASCADE で、ルートの閉包の外から連鎖して消えるオブジェクトの説明（NOTICE 用。発見順）。
    pub cascaded: Vec<String>,
}

/// DETAIL / NOTICE に並べる行数の上限（PostgreSQL の `MAX_REPORTED_DEPS`）。
const MAX_REPORTED_DEPS: usize = 100;

/// 閉包に入れるオブジェクトの集合（発見順を保つ）。
struct DropSet<'a> {
    store: &'a CatalogStore,
    snap: &'a Snapshot,
    visible: &'a [Oid],
    items: Vec<DropItem>,
    index: HashMap<(Oid, Oid), usize>,
    /// 未処理の（依存先を調べる）オブジェクト。
    work: Vec<ObjectAddress>,
}

impl DropSet<'_> {
    fn contains(&self, addr: ObjectAddress) -> bool {
        self.index.contains_key(&addr.key_pair())
    }

    /// 集合に加える。すでにあれば何もしない。実在しないオブジェクトは加えない（`false`）。
    /// 表を加えたときは、安全網として表の部品も直接のキーで加える（`pg_depend` の行が漏れていても
    /// 取りこぼさない。漏れは `check_catalog` が検出する）。
    fn add(&mut self, addr: ObjectAddress) -> Result<bool> {
        if self.contains(addr) {
            return Ok(true);
        }
        let Some(item) = self.make_item(addr)? else {
            return Ok(false);
        };
        let kind = item.kind;
        let obj_id = item.addr.obj_id;
        self.index.insert(addr.key_pair(), self.items.len());
        self.work.push(item.addr);
        self.items.push(item);
        if kind == DropKind::Table {
            let parts = self.store.table_parts(self.snap, obj_id)?;
            for oid in parts.constraints {
                self.add(ObjectAddress::constraint(oid))?;
            }
            for oid in parts.attrdefs {
                self.add(ObjectAddress::attrdef(oid))?;
            }
            for oid in parts.indexes {
                self.add(ObjectAddress::relation(oid))?;
            }
        }
        Ok(true)
    }

    fn make_item(&self, addr: ObjectAddress) -> Result<Option<DropItem>> {
        let whole = ObjectAddress { obj_sub: 0, ..addr };
        match addr.class_id {
            classes::RELATION => {
                let Some(row) = self.store.relation_row(self.snap, addr.obj_id)? else {
                    return Ok(None);
                };
                let kind = match row.kind {
                    RelKind::Table => DropKind::Table,
                    RelKind::Index => DropKind::Index,
                    RelKind::Sequence => DropKind::Sequence,
                };
                Ok(Some(DropItem {
                    addr: whole,
                    kind,
                    description: self.describe(whole)?,
                    locator: Some(row.locator),
                    owner_table: None,
                    attnum: None,
                }))
            }
            classes::CONSTRAINT => {
                let Some(def) = self.store.constraint_by_oid(self.snap, addr.obj_id)? else {
                    return Ok(None);
                };
                Ok(Some(DropItem {
                    addr: whole,
                    kind: DropKind::Constraint,
                    description: self.describe(whole)?,
                    locator: None,
                    owner_table: Some(def.table_oid),
                    attnum: None,
                }))
            }
            classes::ATTRDEF => {
                let Some((relid, attnum)) = self.store.attrdef_by_oid(self.snap, addr.obj_id)?
                else {
                    return Ok(None);
                };
                Ok(Some(DropItem {
                    addr: whole,
                    kind: DropKind::AttrDefault,
                    description: self.describe(whole)?,
                    locator: None,
                    owner_table: Some(relid),
                    attnum: Some(attnum),
                }))
            }
            other => Err(Error::internal(format!(
                "pg_depend refers to an unsupported catalog {other}"
            ))),
        }
    }

    fn describe(&self, addr: ObjectAddress) -> Result<String> {
        self.store.describe_object(self.snap, addr, self.visible)
    }
}

/// 依存の閉包を求める（`m4/07-catalog-ddl.md` §5.8.1）。RESTRICT で連鎖が必要なら `2BP01`。
/// ルートが内部依存で他のオブジェクトに要求されていれば `2BP01`（CASCADE でも同じ）。
/// `visible` は `describe_object` に渡す（名前空間を修飾する必要があるか）。
pub fn plan_drop(
    store: &CatalogStore,
    snap: &Snapshot,
    roots: &[ObjectAddress],
    behavior: DropBehavior,
    visible: &[Oid],
) -> Result<DropPlan> {
    let mut set = DropSet {
        store,
        snap,
        visible,
        items: Vec::new(),
        index: HashMap::new(),
        work: Vec::new(),
    };
    // 1. ルート。
    let mut root_keys = Vec::with_capacity(roots.len());
    for r in roots {
        if !set.add(*r)? {
            return Err(Error::internal(format!(
                "cannot drop object {} of catalog {}: it does not exist",
                r.obj_id, r.class_id
            )));
        }
        root_keys.push(r.key_pair());
    }
    // 2. 閉包（Auto / Internal は一緒に消す。Normal は後で判定する）。
    let mut normal: Vec<(ObjectAddress, ObjectAddress)> = Vec::new();
    close(&mut set, &mut normal)?;

    // 3. ルートの内部依存の検査（CASCADE でも同じ）。
    for r in roots {
        for row in store.references_of(snap, *r)? {
            if row.deptype == DependType::Internal && !set.contains(row.referenced) {
                let me = set.describe(ObjectAddress { obj_sub: 0, ..*r })?;
                let owner = set.describe(row.referenced)?;
                return Err(Error::new(
                    sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!("cannot drop {me} because {owner} requires it"),
                )
                .with_hint(format!("You can drop {owner} instead.")));
            }
        }
    }

    // 4. Normal 依存の処理。
    let mut cascaded: Vec<String> = Vec::new();
    let mut blocked: Vec<(ObjectAddress, ObjectAddress)> = Vec::new();
    let mut handled = 0usize;
    loop {
        let pending: Vec<_> = normal[handled..].to_vec();
        handled = normal.len();
        if pending.is_empty() {
            break;
        }
        for (d, x) in pending {
            if set.contains(d) {
                continue;
            }
            match behavior {
                DropBehavior::Restrict => blocked.push((d, x)),
                DropBehavior::Cascade => {
                    if set.add(d)? {
                        let idx = set.index[&d.key_pair()];
                        cascaded.push(set.items[idx].description.clone());
                    }
                }
            }
        }
        close(&mut set, &mut normal)?;
    }

    // 5. RESTRICT で残った依存。同じ（依存元）は 1 度だけ報告する。
    if !blocked.is_empty() {
        // 閉包に後から入ったものは問題にならない（集合の中の依存元）。
        let blocked: Vec<_> = blocked
            .into_iter()
            .filter(|(d, _)| !set.contains(*d))
            .collect();
        if !blocked.is_empty() {
            let message = if root_keys.len() == 1 {
                let idx = set.index[&root_keys[0]];
                format!(
                    "cannot drop {} because other objects depend on it",
                    set.items[idx].description
                )
            } else {
                "cannot drop desired object(s) because other objects depend on them".to_owned()
            };
            let mut lines = Vec::new();
            for (d, x) in &blocked {
                lines.push(format!(
                    "{} depends on {}",
                    set.describe(*d)?,
                    set.describe(*x)?
                ));
            }
            return Err(Error::new(sqlstate::DEPENDENT_OBJECTS_STILL_EXIST, message)
                .with_detail(report_lines(&lines))
                .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
    }

    Ok(DropPlan {
        items: set.items,
        cascaded,
    })
}

/// 作業キューが空になるまで、依存元を調べる。
fn close(set: &mut DropSet<'_>, normal: &mut Vec<(ObjectAddress, ObjectAddress)>) -> Result<()> {
    while let Some(x) = set.work.pop() {
        for row in set.store.dependents_of(set.snap, x)? {
            match row.deptype {
                DependType::Auto | DependType::Internal => {
                    set.add(row.dependent)?;
                }
                DependType::Normal => normal.push((row.dependent, row.referenced)),
            }
        }
    }
    Ok(())
}

/// DETAIL に並べる行（100 行まで。超えたら末尾に件数）。
fn report_lines(lines: &[String]) -> String {
    let mut out: Vec<String> = lines.iter().take(MAX_REPORTED_DEPS).cloned().collect();
    if lines.len() > MAX_REPORTED_DEPS {
        out.push(format!(
            "and {} other objects (see server log for list)",
            lines.len() - MAX_REPORTED_DEPS
        ));
    }
    out.join("\n")
}

/// CASCADE の NOTICE（`§5.8.2`）。連鎖がなければ `None`。
/// 1 件なら `(drop cascades to <説明>, None)`、2 件以上なら
/// `(drop cascades to N other objects, Some(DETAIL))`。
pub fn cascade_notice(plan: &DropPlan) -> Option<(String, Option<String>)> {
    match plan.cascaded.as_slice() {
        [] => None,
        [one] => Some((format!("drop cascades to {one}"), None)),
        many => {
            let lines: Vec<String> = many
                .iter()
                .map(|d| format!("drop cascades to {d}"))
                .collect();
            Some((
                format!("drop cascades to {} other objects", many.len()),
                Some(report_lines(&lines)),
            ))
        }
    }
}

impl CatalogStore {
    /// PostgreSQL の `getObjectDescription` に当たる文（`§5.8.2`）。`visible` に含まれる名前空間の
    /// 名前は修飾しない。`obj_sub > 0` は列（`column id of table t`）。
    pub fn describe_object(
        &self,
        snap: &Snapshot,
        obj: ObjectAddress,
        visible: &[Oid],
    ) -> Result<String> {
        let gone = |what: &str| {
            Error::internal(format!(
                "cannot describe {what} {}: it does not exist",
                obj.obj_id
            ))
        };
        match obj.class_id {
            classes::RELATION => {
                let row = self
                    .relation_row(snap, obj.obj_id)?
                    .ok_or_else(|| gone("relation"))?;
                let rel = self.qualified_name(snap, &row, visible)?;
                if obj.obj_sub > 0 {
                    let attnum = i16::try_from(obj.obj_sub)
                        .map_err(|_| Error::internal("pg_depend objsubid out of range"))?;
                    let col = self
                        .attribute_name(snap, row.oid, attnum)?
                        .ok_or_else(|| gone("column of relation"))?;
                    Ok(format!(
                        "column {} of {} {rel}",
                        quote_identifier(&col),
                        row.kind.noun()
                    ))
                } else {
                    Ok(format!("{} {rel}", row.kind.noun()))
                }
            }
            classes::CONSTRAINT => {
                let def = self
                    .constraint_by_oid(snap, obj.obj_id)?
                    .ok_or_else(|| gone("constraint"))?;
                let name = quote_identifier(&def.name);
                // 表の制約は `constraint c on table t`。
                let table = self.relation_row(snap, def.table_oid)?;
                match table {
                    Some(t) => {
                        let rel = self.qualified_name(snap, &t, visible)?;
                        Ok(format!("constraint {name} on {} {rel}", t.kind.noun()))
                    }
                    None => Ok(format!("constraint {name}")),
                }
            }
            classes::ATTRDEF => {
                let (relid, attnum) = self
                    .attrdef_by_oid(snap, obj.obj_id)?
                    .ok_or_else(|| gone("default value"))?;
                let row = self
                    .relation_row(snap, relid)?
                    .ok_or_else(|| gone("relation"))?;
                let rel = self.qualified_name(snap, &row, visible)?;
                let col = self
                    .attribute_name(snap, relid, attnum)?
                    .ok_or_else(|| gone("column of relation"))?;
                Ok(format!(
                    "default value for column {} of {} {rel}",
                    quote_identifier(&col),
                    row.kind.noun()
                ))
            }
            other => Err(Error::internal(format!(
                "cannot describe an object of catalog {other}"
            ))),
        }
    }

    /// 名前空間が見えていれば修飾しない名前、見えなければ `schema.name`。
    fn qualified_name(
        &self,
        snap: &Snapshot,
        row: &RelationRow,
        visible: &[Oid],
    ) -> Result<String> {
        if visible.contains(&row.namespace) {
            return Ok(quote_identifier(&row.name));
        }
        let nsp = self
            .namespace_name(snap, row.namespace)?
            .unwrap_or_else(|| row.namespace.to_string());
        Ok(quote_qualified(&nsp, &row.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::store::test_env::*;
    use crate::catalog::store::{
        EMPTY_INDEX_STATS, NewConstraint, NewIndex, NewSequence, NewTable, TableOidRequest,
    };
    use crate::catalog::{ColumnDef, SequenceParams};
    use crate::types::{SqlType, oid};

    const VISIBLE: &[Oid] = &[11, 2200];

    /// `o1(id serial primary key)` と、その SERIAL のシーケンスを既定値で使う `o2`・`o3`。
    struct Fixture {
        e: Env,
        o1: Oid,
        seq: Oid,
        o2: Oid,
        o3: Oid,
        o4: Oid,
    }

    fn serial_table(
        e: &Env,
        name: &str,
        own_seq: bool,
        default_seq: Option<Oid>,
    ) -> (Oid, Option<Oid>) {
        let oids_ = e
            .store
            .allocate_table_oids(
                &e.oids,
                &TableOidRequest {
                    n_attrdefs: 1,
                    n_checks: 0,
                    n_index_constraints: usize::from(own_seq),
                },
            )
            .unwrap();
        let seq = own_seq.then(|| e.store.get_new_relation_oid(&e.oids).unwrap());
        let target = seq.or(default_seq).unwrap();
        let mut col: ColumnDef = col(
            "id",
            1,
            SqlType::INT4,
            true,
            Some(&format!("nextval('{target}'::regclass)")),
        );
        col.attnum = 1;
        let mut spec = NewTable::plain(oids_.table, 2200, name, 10, vec![col]);
        spec.attrdef_oids = oids_.attrdefs.clone();
        spec.extra_depends.push(NewDepend {
            dependent: ObjectAddress::attrdef(oids_.attrdefs[0]),
            referenced: ObjectAddress::relation(target),
            deptype: DependType::Normal,
        });
        if own_seq {
            let (index, con) = oids_.index_constraints[0];
            spec.indexes.push(NewIndex {
                oid: index,
                name: format!("{name}_pkey"),
                namespace: 2200,
                owner: 10,
                table_oid: oids_.table,
                relfilenode: index,
                columns: vec![int4_col(1, false, false)],
                unique: true,
                primary: true,
                constraint: Some(NewConstraint {
                    oid: con,
                    name: format!("{name}_pkey"),
                }),
                stats: EMPTY_INDEX_STATS,
            });
        }
        write_table(e, &spec);
        if let Some(seq) = seq {
            let params = SequenceParams {
                type_oid: oid::INT4,
                start: 1,
                increment: 1,
                min: 1,
                max: i64::from(i32::MAX),
                cache: 1,
                cycle: false,
                owned_by: Some((oids_.table, 1)),
            };
            e.store
                .create_sequence(
                    &wctx(),
                    &snap(),
                    &NewSequence {
                        oid: seq,
                        name: format!("{name}_id_seq"),
                        namespace: 2200,
                        owner: 10,
                        params,
                        owned_by_deptype: Some(DependType::Auto),
                    },
                )
                .unwrap();
        }
        (oids_.table, seq)
    }

    fn fixture() -> Fixture {
        let e = env();
        let (o1, seq) = serial_table(&e, "o1", true, None);
        let seq = seq.unwrap();
        let (o2, _) = serial_table(&e, "o2", false, Some(seq));
        let (o3, _) = serial_table(&e, "o3", false, Some(seq));
        let (o4, _) = serial_table(&e, "o4", true, None);
        Fixture {
            e,
            o1,
            seq,
            o2,
            o3,
            o4,
        }
    }

    fn drop_tables(f: &Fixture, tables: &[Oid], b: DropBehavior) -> Result<DropPlan> {
        let roots: Vec<_> = tables.iter().map(|o| ObjectAddress::relation(*o)).collect();
        plan_drop(&f.e.store, &snap(), &roots, b, VISIBLE)
    }

    #[test]
    fn restrict_reports_the_default_values_that_use_the_sequence() {
        let f = fixture();
        let err = drop_tables(&f, &[f.o1], DropBehavior::Restrict).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(
            err.message,
            "cannot drop table o1 because other objects depend on it"
        );
        assert_eq!(
            err.detail.as_deref(),
            Some(
                "default value for column id of table o2 depends on sequence o1_id_seq\n\
                 default value for column id of table o3 depends on sequence o1_id_seq"
            )
        );
        assert_eq!(
            err.hint.as_deref(),
            Some("Use DROP ... CASCADE to drop the dependent objects too.")
        );
    }

    #[test]
    fn several_roots_use_the_plural_message() {
        let f = fixture();
        let err = drop_tables(&f, &[f.o1, f.o4], DropBehavior::Restrict).unwrap_err();
        assert_eq!(
            err.message,
            "cannot drop desired object(s) because other objects depend on them"
        );
    }

    #[test]
    fn dependents_inside_the_set_are_fine() {
        let f = fixture();
        let plan = drop_tables(&f, &[f.o1, f.o2, f.o3], DropBehavior::Restrict).unwrap();
        assert!(plan.cascaded.is_empty());
        // 3 tables + pkey index/constraint of o1 + sequence + 2 attrdefs + o2/o3 attrdefs; no duplicates
        let mut keys: Vec<_> = plan.items.iter().map(|i| i.addr).collect();
        let n = keys.len();
        keys.sort_by_key(|a| (a.class_id, a.obj_id));
        keys.dedup();
        assert_eq!(keys.len(), n);
        assert_eq!(plan.items[0].addr, ObjectAddress::relation(f.o1));
    }

    #[test]
    fn cascade_drops_the_default_values_and_keeps_the_columns() {
        let f = fixture();
        let plan = drop_tables(&f, &[f.o1], DropBehavior::Cascade).unwrap();
        assert_eq!(
            plan.cascaded,
            [
                "default value for column id of table o2",
                "default value for column id of table o3"
            ]
        );
        let (msg, detail) = cascade_notice(&plan).unwrap();
        assert_eq!(msg, "drop cascades to 2 other objects");
        assert_eq!(
            detail.unwrap(),
            "drop cascades to default value for column id of table o2\n\
             drop cascades to default value for column id of table o3"
        );
        f.e.store.drop_objects(&wctx(), &snap(), &plan).unwrap();
        // o2 and o3 survive and lose the default
        let o2 = f.e.store.load_table_def(&snap(), f.o2).unwrap().unwrap();
        assert!(o2.columns[0].default.is_none());
        assert_eq!(rows_mentioning(&f.e, f.o1), 0);
        assert_eq!(rows_mentioning(&f.e, f.seq), 0);
        let problems = crate::catalog::check::check_catalog_store(&f.e.store, &snap()).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn one_cascaded_object_gets_a_single_line_notice() {
        let f = fixture();
        // o3 first drops its default, so only o2 remains
        let plan = drop_tables(&f, &[f.o1, f.o3], DropBehavior::Cascade).unwrap();
        assert_eq!(plan.cascaded, ["default value for column id of table o2"]);
        let (msg, detail) = cascade_notice(&plan).unwrap();
        assert_eq!(
            msg,
            "drop cascades to default value for column id of table o2"
        );
        assert!(detail.is_none());
        assert!(cascade_notice(&DropPlan::default()).is_none());
    }

    #[test]
    fn owned_objects_go_quietly() {
        let f = fixture();
        let plan = drop_tables(&f, &[f.o4], DropBehavior::Restrict).unwrap();
        assert!(plan.cascaded.is_empty());
        let kinds: Vec<_> = plan.items.iter().map(|i| i.kind).collect();
        for k in [
            DropKind::Table,
            DropKind::Index,
            DropKind::Constraint,
            DropKind::AttrDefault,
            DropKind::Sequence,
        ] {
            assert!(kinds.contains(&k), "{k:?} missing from {kinds:?}");
        }
        assert_eq!(plan.items.iter().filter(|i| i.locator.is_some()).count(), 3);
    }

    #[test]
    fn an_index_owned_by_a_constraint_cannot_be_dropped_alone() {
        let f = fixture();
        let def = f.e.store.load_table_def(&snap(), f.o1).unwrap().unwrap();
        let pk = &def.indexes[0];
        let err = plan_drop(
            &f.e.store,
            &snap(),
            &[ObjectAddress::relation(pk.oid)],
            DropBehavior::Cascade,
            VISIBLE,
        )
        .unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::DEPENDENT_OBJECTS_STILL_EXIST);
        assert_eq!(
            err.message,
            "cannot drop index o1_pkey because constraint o1_pkey on table o1 requires it"
        );
        assert_eq!(
            err.hint.as_deref(),
            Some("You can drop constraint o1_pkey on table o1 instead.")
        );
    }

    #[test]
    fn a_plain_index_drops_alone() {
        let f = fixture();
        let idx = new_index(
            &f.e,
            f.o2,
            "o2_id_idx",
            vec![int4_col(1, false, false)],
            None,
        );
        f.e.store
            .create_index(&wctx(), &snap(), &idx, true)
            .unwrap();
        let plan = plan_drop(
            &f.e.store,
            &snap(),
            &[ObjectAddress::relation(idx.oid)],
            DropBehavior::Restrict,
            VISIBLE,
        )
        .unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].kind, DropKind::Index);
        assert_eq!(plan.items[0].description, "index o2_id_idx");
        f.e.store.drop_objects(&wctx(), &snap(), &plan).unwrap();
        assert_eq!(rows_mentioning(&f.e, idx.oid), 0);
        assert!(
            f.e.store
                .load_table_def(&snap(), f.o2)
                .unwrap()
                .unwrap()
                .indexes
                .is_empty()
        );
    }

    #[test]
    fn dropping_the_sequence_alone_is_blocked_by_the_defaults() {
        let f = fixture();
        let err = plan_drop(
            &f.e.store,
            &snap(),
            &[ObjectAddress::relation(f.seq)],
            DropBehavior::Restrict,
            VISIBLE,
        )
        .unwrap_err();
        assert_eq!(
            err.message,
            "cannot drop sequence o1_id_seq because other objects depend on it"
        );
        assert!(
            err.detail.unwrap().starts_with(
                "default value for column id of table o1 depends on sequence o1_id_seq"
            )
        );
    }

    #[test]
    fn descriptions_qualify_invisible_namespaces() {
        let f = fixture();
        let a = ObjectAddress::relation(f.o2);
        assert_eq!(
            f.e.store.describe_object(&snap(), a, VISIBLE).unwrap(),
            "table o2"
        );
        assert_eq!(
            f.e.store.describe_object(&snap(), a, &[11]).unwrap(),
            "table public.o2"
        );
        assert_eq!(
            f.e.store
                .describe_object(&snap(), ObjectAddress::column(f.o2, 1), VISIBLE)
                .unwrap(),
            "column id of table o2"
        );
        assert!(
            f.e.store
                .describe_object(&snap(), ObjectAddress::relation(99_999), VISIBLE)
                .is_err()
        );
        assert!(
            f.e.store
                .describe_object(
                    &snap(),
                    ObjectAddress {
                        class_id: 7,
                        obj_id: 1,
                        obj_sub: 0
                    },
                    VISIBLE
                )
                .is_err()
        );
    }

    #[test]
    fn the_safety_net_finds_parts_missing_from_pg_depend() {
        let f = fixture();
        // lose every dependency row of o4's primary key constraint and default
        for class in [classes::CONSTRAINT, classes::ATTRDEF] {
            let rows = f.e.store.table_parts(&snap(), f.o4).unwrap();
            let ids = if class == classes::CONSTRAINT {
                rows.constraints
            } else {
                rows.attrdefs
            };
            for id in ids {
                f.e.store
                    .delete_dependencies(
                        &wctx(),
                        &snap(),
                        crate::catalog::store::DependFilter::Dependent {
                            class_id: class,
                            obj_id: id,
                        },
                    )
                    .unwrap();
            }
        }
        let plan = drop_tables(&f, &[f.o4], DropBehavior::Restrict).unwrap();
        f.e.store.drop_objects(&wctx(), &snap(), &plan).unwrap();
        assert_eq!(rows_mentioning(&f.e, f.o4), 0);
    }

    #[test]
    fn a_missing_root_is_an_internal_error() {
        let f = fixture();
        assert!(
            plan_drop(
                &f.e.store,
                &snap(),
                &[ObjectAddress::relation(99_999)],
                DropBehavior::Restrict,
                VISIBLE
            )
            .is_err()
        );
    }

    #[test]
    fn long_detail_lists_are_truncated() {
        let lines: Vec<String> = (0..103).map(|i| format!("line {i}")).collect();
        let r = report_lines(&lines);
        assert_eq!(r.lines().count(), 101);
        assert!(r.ends_with("and 3 other objects (see server log for list)"));
    }

    #[test]
    fn address_and_type_helpers() {
        assert_eq!(ObjectAddress::column(5, 3).obj_sub, 3);
        for t in [DependType::Normal, DependType::Auto, DependType::Internal] {
            assert_eq!(DependType::from_code(t.code()), Some(t));
        }
        assert_eq!(DependType::from_code('x'), None);
        assert_eq!(classes::RELATION, 1259);
    }
}
