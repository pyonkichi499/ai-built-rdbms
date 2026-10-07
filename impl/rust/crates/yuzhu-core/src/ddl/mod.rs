//! DDL の実行（`m4/07-catalog-ddl.md` §4.7、`m4/08-sequence-serial.md` §5、`m4/00-contracts.md` §14.6）。
//!
//! session は解析済みの `BoundDdl` を [`execute`] に渡す。ここは変種をファイルごとの関数へ振り分けるだけで、
//! 中身は C1（`table` `index` `constraint` `truncate` `vacuum` `depend`）と Q1（`sequence`）が書く。
//!
//! どの DDL も「途中で失敗したら `Err` を返すだけ」（07 §5.0 の 1）。ローカルな取り消しは書かない。
//! 作ったファイルは `pending_creates` に載っているので、session がアボートで消す。

pub mod constraint;
pub mod depend;
pub mod index;
pub mod sequence;
pub mod table;
pub mod truncate;
pub mod vacuum;

use std::sync::Arc;

use crate::analyzer::query::{BoundDdl, RelOption};
use crate::catalog::CatalogReader;
use crate::catalog::naming::NameLookup;
use crate::engine::{Cluster, DatabaseHandle};
use crate::error::{Error, Result, Severity, SqlState, sqlstate};
use crate::interrupt::InterruptFlag;
use crate::session::Notice;
use crate::storage::smgr::{DEFAULTTABLESPACE_OID, RelFileLocator, RelFileNumber};
use crate::storage::{IndexStore, TableStore, WriteCtx};
use crate::txn::{Snapshot, Transaction};
use crate::types::{Oid, TypeEnv};

/// DDL 関数に渡す文脈（00 §14.6。07 §4.7 の 2 フィールド `in_transaction_block` と `interrupts` を含む）。
pub struct DdlCtx<'a> {
    pub cluster: &'a Cluster,
    pub db: &'a Arc<DatabaseHandle>,
    pub snapshot: &'a Snapshot,
    pub catalog: &'a dyn CatalogReader,
    pub txn: &'a mut Transaction,
    pub role_oid: Oid,
    pub notices: &'a mut Vec<Notice>,
    pub type_env: &'a TypeEnv<'a>,
    /// 明示ブロックまたは複数文の Simple Query の暗黙のブロックの中か（`CONCURRENTLY` と VACUUM の `25001`）。
    pub in_transaction_block: bool,
    pub interrupts: &'a InterruptFlag,
}

impl std::fmt::Debug for DdlCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DdlCtx")
            .field("role_oid", &self.role_oid)
            .field("in_transaction_block", &self.in_transaction_block)
            .finish_non_exhaustive()
    }
}

impl DdlCtx<'_> {
    /// ライターロックを持っていなければ内部エラー。
    pub fn write_ctx(&mut self) -> Result<WriteCtx> {
        self.txn.write_ctx()
    }

    /// `SMGR_CREATE` を WAL に挿入してファイルを作り、`pending_creates` に積む（この順序を守る。07 §4.7）。
    pub fn create_file(&mut self, w: &WriteCtx, locator: RelFileLocator) -> Result<()> {
        check_rel_limit(self.txn.pending_creates.len(), 1)?;
        self.cluster.storage().create_storage(w, locator)?;
        self.txn.pending_creates.push(locator);
        Ok(())
    }

    /// 消すファイルを `pending_unlinks` に積む（コミットで消える）。
    pub fn schedule_unlink(&mut self, locator: RelFileLocator) {
        self.txn.pending_unlinks.push(locator);
    }

    /// 消すファイルをまとめて積む。1 つのコミットレコードに入りきらないなら `54000`（積まない）。
    pub fn schedule_unlinks(&mut self, locators: &[RelFileLocator]) -> Result<()> {
        check_rel_limit(self.txn.pending_unlinks.len(), locators.len())?;
        self.txn.pending_unlinks.extend_from_slice(locators);
        Ok(())
    }

    /// 表の本体（メインフォーク）のファイル番号が `rel` のデフォルト表領域のファイルの場所。
    pub fn locator_for(&self, rel: Oid) -> RelFileLocator {
        RelFileLocator {
            spc_oid: DEFAULTTABLESPACE_OID,
            db_oid: self.db.oid,
            rel_number: RelFileNumber(rel),
        }
    }

    /// 名前の自動生成（`catalog::naming`）が使う衝突の判定。この文のスナップショットで `pg_class` /
    /// `pg_constraint` を引く（同じ文で作る名前は呼び出し側の `taken` が持つ）。
    pub fn name_lookup(&self) -> CatalogNames<'_> {
        CatalogNames {
            db: self.db,
            snap: self.snapshot,
        }
    }

    pub fn heap(&self) -> &dyn TableStore {
        &**self.cluster.storage()
    }

    pub fn index_store(&self) -> &dyn IndexStore {
        &*self.cluster.stack().index
    }

    /// カタログを変えたことを記録する。DDL の最後に必ず呼ぶ。
    pub fn mark_catalog_dirty(&mut self) {
        self.txn.catalog_dirty = true;
    }

    pub fn notice(
        &mut self,
        severity: Severity,
        sqlstate: SqlState,
        message: String,
        detail: Option<String>,
    ) {
        self.notices.push(Notice {
            severity,
            sqlstate,
            message,
            detail,
            hint: None,
        });
    }

    /// 長いループ（ヒープの全走査、ソート）が行ごとに呼ぶ。
    pub fn check_interrupts(&self) -> Result<()> {
        self.interrupts.check()
    }
}

/// `NameLookup`（`catalog::naming`）の実装。`pg_class` と `pg_constraint` を引く。
#[derive(Debug)]
pub struct CatalogNames<'a> {
    db: &'a DatabaseHandle,
    snap: &'a Snapshot,
}

impl NameLookup for CatalogNames<'_> {
    fn relation_exists(&self, nsp: Oid, name: &str) -> Result<bool> {
        Ok(self
            .db
            .catalog
            .lookup_relation_row(self.snap, nsp, name)?
            .is_some())
    }

    fn constraint_exists(&self, nsp: Oid, name: &str) -> Result<bool> {
        self.db.catalog.constraint_name_exists(self.snap, nsp, name)
    }
}

/// 1 つのコミット / アボートのレコードに載せられるリレーションの数の上限（session と同じ。`54000`）。
fn check_rel_limit(pending: usize, adding: usize) -> Result<()> {
    if pending + adding > crate::txn::xact_wal::MAX_RELS_PER_RECORD {
        return Err(Error::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many relations created or dropped in one transaction",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelOptTarget {
    Table,
    Index,
}

/// `WITH (...)` を検査する（`m4/07-catalog-ddl.md` §5.9。D07-14: 検証して捨てる）。`22023`。
pub fn validate_reloptions(target: RelOptTarget, opts: &[RelOption]) -> Result<()> {
    let bad = |m: String| Error::new(sqlstate::INVALID_PARAMETER_VALUE, m);
    // 名前空間（PostgreSQL は先にすべての項目の名前空間を調べる）。
    for o in opts {
        match o.namespace.as_deref() {
            None => {}
            Some("toast") => {
                return Err(bad(format!("unrecognized parameter \"{}\"", o.name)));
            }
            Some(ns) => {
                return Err(bad(format!("unrecognized parameter namespace \"{ns}\"")));
            }
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for o in opts {
        let name = o.name.as_str();
        let known = matches!(
            (target, name),
            (_, "fillfactor") | (RelOptTarget::Index, "deduplicate_items")
        );
        if !known {
            return Err(bad(format!("unrecognized parameter \"{name}\"")));
        }
        if seen.contains(&name) {
            return Err(bad(format!(
                "parameter \"{name}\" specified more than once"
            )));
        }
        seen.push(name);
        let value = o.value.as_deref().unwrap_or("true");
        if name == "fillfactor" {
            let v: i64 = value.trim().parse().map_err(|_| {
                bad(format!(
                    "invalid value for integer option \"{name}\": {value}"
                ))
            })?;
            if !(10..=100).contains(&v) {
                return Err(
                    bad(format!("value {v} out of bounds for option \"{name}\""))
                        .with_detail("Valid values are between \"10\" and \"100\"."),
                );
            }
        } else if parse_bool(value).is_none() {
            return Err(bad(format!(
                "invalid value for boolean option \"{name}\": {value}"
            )));
        }
    }
    Ok(())
}

/// PostgreSQL の `parse_bool`（前方一致を許す）。
pub(crate) fn parse_bool(value: &str) -> Option<bool> {
    let v = value.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    let prefix = |full: &str, min: usize| v.len() >= min && full.starts_with(v.as_str());
    if prefix("true", 1) || prefix("yes", 1) || v == "on" || v == "1" {
        Some(true)
    } else if prefix("false", 1) || prefix("no", 1) || v == "off" || v == "of" || v == "0" {
        Some(false)
    } else {
        None
    }
}

/// コマンドタグ（`CREATE TABLE`、`CREATE INDEX`、`TRUNCATE TABLE`、`VACUUM` ...）を返す。
pub fn execute(ctx: &mut DdlCtx<'_>, ddl: BoundDdl) -> Result<String> {
    match ddl {
        BoundDdl::CreateTable(b) => table::create_table(ctx, &b),
        BoundDdl::DropTable(b) => table::drop_table(ctx, &b),
        BoundDdl::CreateIndex(b) => index::create_index(ctx, &b),
        BoundDdl::DropIndex(b) => index::drop_index(ctx, &b),
        BoundDdl::AlterTableAddConstraint(b) => constraint::add_constraint(ctx, &b),
        BoundDdl::AlterTableAddCheck(b) => constraint::add_check(ctx, &b),
        BoundDdl::AlterTableOwner(b) => constraint::alter_owner(ctx, &b),
        BoundDdl::Truncate(b) => truncate::truncate(ctx, &b),
        BoundDdl::Vacuum(b) => vacuum::vacuum(ctx, &b),
        BoundDdl::CreateSequence(b) => sequence::create_sequence(ctx, &b),
        BoundDdl::AlterSequence(b) => sequence::alter_sequence(ctx, &b),
        BoundDdl::DropSequence(b) => sequence::drop_sequence(ctx, &b),
    }
}

/// DDL の単体テストの足場。`TestCluster`（SimVfs）の上で、session を通さずに `ddl::execute` を直接呼ぶ。
/// session と同じ順序（書く文はライターロック → 文のスナップショット → DDL → CCI → コミット / アボート）を踏む。
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use crate::analyzer::query::{BoundCreateTable, BoundIndexConstraint, IndexConstraintKind};
    use crate::catalog::depend::DropBehavior;
    use crate::catalog::reader::StatementCatalog;
    use crate::catalog::{CheckDef, ColumnDef, TableDef};
    use crate::testing::TestCluster;
    use crate::txn::{WaitCtl, WriterGuard};
    use crate::types::SqlType;

    pub(crate) struct Harness {
        pub(crate) tc: TestCluster,
        pub(crate) db: Arc<DatabaseHandle>,
        pub(crate) role: Oid,
        pub(crate) txn: Transaction,
        pub(crate) notices: Vec<Notice>,
        pub(crate) intr: InterruptFlag,
        pub(crate) in_block: bool,
        session_id: u64,
    }

    impl Harness {
        pub(crate) fn new() -> Harness {
            let tc = TestCluster::new();
            let (db, role) = tc.cluster.connect("postgres", "postgres").unwrap();
            let session_id = tc.cluster.next_session_id();
            Harness {
                tc,
                db,
                role: role.oid,
                txn: Transaction::new(),
                notices: Vec::new(),
                intr: InterruptFlag::default(),
                in_block: false,
                session_id,
            }
        }

        /// ライターロックを取り、XID を割り当てる（書く文の前。07 §5.0）。
        pub(crate) fn begin(&mut self) {
            let (xid, guard): (_, WriterGuard) = self
                .tc
                .cluster
                .txn_manager()
                .begin_write(
                    self.session_id,
                    &WaitCtl {
                        lock_timeout: None,
                        interrupts: &self.intr,
                    },
                )
                .unwrap();
            self.txn = Transaction::new();
            self.txn.xid = Some(xid);
            self.txn.writer = Some(guard);
        }

        /// 1 つの文として `f` を流す。成功したら CCI（cid を進める）。
        pub(crate) fn run<R>(&mut self, f: impl FnOnce(&mut DdlCtx<'_>) -> Result<R>) -> Result<R> {
            let snap = self
                .tc
                .cluster
                .txn_manager()
                .snapshot(self.txn.xid, self.txn.cid);
            let search_path = vec!["public".to_owned()];
            let catalog = StatementCatalog {
                db: &self.db,
                snapshot: &snap,
                gen_at_snapshot: self.db.cache.generation(),
                bypass_cache: true,
                search_path: &search_path,
            };
            let env = TypeEnv::default();
            let mut ctx = DdlCtx {
                cluster: &self.tc.cluster,
                db: &self.db,
                snapshot: &snap,
                catalog: &catalog,
                txn: &mut self.txn,
                role_oid: self.role,
                notices: &mut self.notices,
                type_env: &env,
                in_transaction_block: self.in_block,
                interrupts: &self.intr,
            };
            let out = f(&mut ctx)?;
            self.txn.command_counter_increment().unwrap();
            Ok(out)
        }

        pub(crate) fn exec(&mut self, ddl: BoundDdl) -> Result<String> {
            self.run(|ctx| execute(ctx, ddl))
        }

        /// 書く文: ライターロックがなければ取ってから流す。
        pub(crate) fn exec_w(&mut self, ddl: BoundDdl) -> Result<String> {
            if self.txn.xid.is_none() {
                self.begin();
            }
            self.exec(ddl)
        }

        pub(crate) fn commit(&mut self) {
            let mut txn = std::mem::replace(&mut self.txn, Transaction::new());
            let xid = txn.xid.expect("no transaction");
            self.tc
                .cluster
                .txn_manager()
                .commit(xid, &txn.pending_unlinks)
                .unwrap();
            if txn.catalog_dirty {
                self.tc.cluster.invalidate_all_catalog_caches();
            }
            txn.writer = None;
            for l in &txn.pending_unlinks {
                self.tc.cluster.storage().unlink_storage(*l).unwrap();
            }
        }

        pub(crate) fn rollback(&mut self) {
            let mut txn = std::mem::replace(&mut self.txn, Transaction::new());
            let xid = txn.xid.expect("no transaction");
            self.tc
                .cluster
                .txn_manager()
                .abort(xid, &txn.pending_creates)
                .unwrap();
            txn.writer = None;
            for l in &txn.pending_creates {
                self.tc.cluster.storage().unlink_storage(*l).unwrap();
            }
        }

        /// いまのトランザクション（または何も始めていなければコミット済みの状態）から見える表。
        pub(crate) fn table(&self, name: &str) -> Option<Arc<TableDef>> {
            self.reader(|c| c.table(Some("public"), name).unwrap())
        }

        pub(crate) fn relation_kind(&self, name: &str) -> Option<(Oid, crate::catalog::RelKind)> {
            self.reader(|c| c.relation_kind(Some("public"), name).unwrap())
        }

        pub(crate) fn reader<R>(&self, f: impl FnOnce(&StatementCatalog<'_>) -> R) -> R {
            let snap = self
                .tc
                .cluster
                .txn_manager()
                .snapshot(self.txn.xid, self.txn.cid);
            let search_path = vec!["public".to_owned()];
            let catalog = StatementCatalog {
                db: &self.db,
                snapshot: &snap,
                gen_at_snapshot: self.db.cache.generation(),
                bypass_cache: true,
                search_path: &search_path,
            };
            f(&catalog)
        }

        /// ファイルがある（消したファイルは D13 により 0 バイトで残る）。
        pub(crate) fn file_exists(&self, l: RelFileLocator) -> bool {
            self.tc.cluster.storage().storage_exists(l).unwrap()
        }

        /// ファイルが消えている（D13: 0 バイトの残骸を含む。ブロックを持つ索引のファイルだけが意味のある判定）。
        pub(crate) fn file_removed(&self, l: RelFileLocator) -> bool {
            !self.file_exists(l)
                || self
                    .tc
                    .cluster
                    .stack()
                    .smgr
                    .nblocks(l, crate::storage::smgr::ForkNumber::Main)
                    .unwrap()
                    == 0
        }

        /// 全カタログの、この OID を指す行の数（見えている行。DROP の取りこぼしの検査）。
        pub(crate) fn rows_mentioning(&self, oid: Oid) -> usize {
            use crate::catalog::rows::column_index;
            use crate::catalog::schema::oids;
            use crate::types::Datum;
            let snap = self
                .tc
                .cluster
                .txn_manager()
                .snapshot(self.txn.xid, self.txn.cid);
            let n = |cat: Oid, cols: &[&str]| {
                let idx: Vec<usize> = cols.iter().map(|c| column_index(cat, c)).collect();
                self.db
                    .catalog
                    .scan(&snap, cat, |r| {
                        Ok(idx.iter().any(|i| r[*i] == Datum::Oid(oid)))
                    })
                    .unwrap()
                    .len()
            };
            n(oids::PG_CLASS, &["oid"])
                + n(oids::PG_ATTRIBUTE, &["attrelid"])
                + n(oids::PG_ATTRDEF, &["adrelid", "oid"])
                + n(oids::PG_CONSTRAINT, &["conrelid", "conindid", "oid"])
                + n(oids::PG_INDEX, &["indrelid", "indexrelid"])
                + n(oids::PG_SEQUENCE, &["seqrelid"])
                + n(oids::PG_DEPEND, &["objid", "refobjid"])
        }

        /// `check_catalog` の結果（空なら整合している）。
        pub(crate) fn check(&self) -> Vec<String> {
            let snap = self
                .tc
                .cluster
                .txn_manager()
                .snapshot(None, crate::txn::FIRST_COMMAND_ID);
            crate::catalog::check::check_catalog(&self.tc.cluster, &self.db, &snap).unwrap()
        }
    }

    pub(crate) fn col(name: &str, attnum: i16, ty: SqlType) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null: false,
            default: None,
            identity: None,
        }
    }

    pub(crate) fn create_table_ddl(name: &str, columns: Vec<ColumnDef>) -> BoundCreateTable {
        BoundCreateTable {
            schema: "public".into(),
            name: name.into(),
            if_not_exists: false,
            columns,
            checks: Vec::new(),
            constraints: Vec::new(),
            sequences: Vec::new(),
            options: Vec::new(),
            default_refs: Vec::new(),
        }
    }

    pub(crate) fn key(
        kind: IndexConstraintKind,
        name: Option<&str>,
        cols: &[i16],
    ) -> BoundIndexConstraint {
        BoundIndexConstraint {
            name: name.map(str::to_owned),
            kind,
            columns: cols.to_vec(),
            options: Vec::new(),
            span: crate::error::Span::default(),
        }
    }

    pub(crate) fn check_def(name: &str, sql: &str) -> CheckDef {
        CheckDef {
            name: name.into(),
            expr_sql: sql.into(),
            no_inherit: false,
        }
    }

    pub(crate) fn drop_ddl(tables: Vec<Arc<TableDef>>, behavior: DropBehavior) -> BoundDdl {
        BoundDdl::DropTable(crate::analyzer::query::BoundDropTable {
            tables,
            missing: Vec::new(),
            behavior,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use crate::error::sqlstate;

    fn opt(name: &str, value: Option<&str>) -> RelOption {
        RelOption {
            namespace: None,
            name: name.into(),
            value: value.map(str::to_owned),
        }
    }

    fn reloption_error(target: RelOptTarget, opts: &[RelOption]) -> (String, Option<String>) {
        let e = validate_reloptions(target, opts).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INVALID_PARAMETER_VALUE);
        (e.message.clone(), e.detail.clone())
    }

    #[test]
    fn reloptions_accept_the_documented_forms() {
        use RelOptTarget::{Index, Table};
        validate_reloptions(Table, &[]).unwrap();
        validate_reloptions(Table, &[opt("fillfactor", Some("70"))]).unwrap();
        validate_reloptions(Table, &[opt("fillfactor", Some(" 100 "))]).unwrap();
        validate_reloptions(Index, &[opt("fillfactor", Some("10"))]).unwrap();
        validate_reloptions(
            Index,
            &[
                opt("fillfactor", Some("50")),
                opt("deduplicate_items", Some("off")),
            ],
        )
        .unwrap();
        validate_reloptions(Index, &[opt("deduplicate_items", None)]).unwrap();
    }

    #[test]
    fn reloptions_errors_match_postgresql() {
        use RelOptTarget::{Index, Table};
        let ns = |ns: &str, name: &str| RelOption {
            namespace: Some(ns.into()),
            name: name.into(),
            value: Some("1".into()),
        };
        assert_eq!(
            reloption_error(Table, &[ns("toast", "foo")]).0,
            "unrecognized parameter \"foo\""
        );
        assert_eq!(
            reloption_error(Table, &[ns("public", "fillfactor")]).0,
            "unrecognized parameter namespace \"public\""
        );
        assert_eq!(
            reloption_error(
                Table,
                &[opt("fillfactor", Some("50")), opt("fillfactor", Some("60"))]
            )
            .0,
            "parameter \"fillfactor\" specified more than once"
        );
        assert_eq!(
            reloption_error(Table, &[opt("foo", Some("1"))]).0,
            "unrecognized parameter \"foo\""
        );
        // `deduplicate_items` は索引だけ。
        assert_eq!(
            reloption_error(Table, &[opt("deduplicate_items", Some("on"))]).0,
            "unrecognized parameter \"deduplicate_items\""
        );
        assert_eq!(
            reloption_error(Table, &[opt("fillfactor", Some("abc"))]).0,
            "invalid value for integer option \"fillfactor\": abc"
        );
        assert_eq!(
            reloption_error(Table, &[opt("fillfactor", Some("50.5"))]).0,
            "invalid value for integer option \"fillfactor\": 50.5"
        );
        assert_eq!(
            reloption_error(Table, &[opt("fillfactor", None)]).0,
            "invalid value for integer option \"fillfactor\": true"
        );
        let (m, d) = reloption_error(Table, &[opt("fillfactor", Some("5"))]);
        assert_eq!(m, "value 5 out of bounds for option \"fillfactor\"");
        assert_eq!(
            d.as_deref(),
            Some("Valid values are between \"10\" and \"100\".")
        );
        assert_eq!(
            reloption_error(Index, &[opt("fillfactor", Some("101"))]).0,
            "value 101 out of bounds for option \"fillfactor\""
        );
        assert_eq!(
            reloption_error(Index, &[opt("deduplicate_items", Some("x"))]).0,
            "invalid value for boolean option \"deduplicate_items\": x"
        );
    }

    #[test]
    fn parse_bool_follows_postgresql() {
        for v in ["t", "true", "TRUE", "y", "yes", "on", "1"] {
            assert_eq!(parse_bool(v), Some(true), "{v}");
        }
        for v in ["f", "false", "n", "no", "off", "of", "0"] {
            assert_eq!(parse_bool(v), Some(false), "{v}");
        }
        for v in ["", "x", "2", "tru3", "o"] {
            assert_eq!(parse_bool(v), None, "{v}");
        }
    }

    #[test]
    fn helpers_touch_the_transaction() {
        let mut h = Harness::new();
        h.begin();
        let loc = RelFileLocator {
            spc_oid: 1663,
            db_oid: h.db.oid,
            rel_number: RelFileNumber(16384),
        };
        h.run(|ctx| {
            ctx.mark_catalog_dirty();
            ctx.schedule_unlink(loc);
            ctx.notice(
                Severity::Notice,
                sqlstate::DUPLICATE_TABLE,
                "relation \"t\" already exists, skipping".into(),
                None,
            );
            assert!(ctx.check_interrupts().is_ok());
            assert_eq!(ctx.locator_for(16384), loc);
            Ok(())
        })
        .unwrap();
        assert!(h.txn.catalog_dirty);
        assert_eq!(h.txn.pending_unlinks, vec![loc]);
        assert_eq!(h.notices.len(), 1);
        h.rollback();
    }

    #[test]
    fn schedule_unlinks_respects_the_record_limit() {
        let mut h = Harness::new();
        h.begin();
        let loc = |n: u32| RelFileLocator {
            spc_oid: 1663,
            db_oid: h.db.oid,
            rel_number: RelFileNumber(n),
        };
        let many: Vec<RelFileLocator> = (0..=crate::txn::xact_wal::MAX_RELS_PER_RECORD)
            .map(|n| loc(u32::try_from(n).unwrap()))
            .collect();
        let e = h.run(|ctx| ctx.schedule_unlinks(&many)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(h.txn.pending_unlinks.is_empty());
        h.rollback();
    }

    #[test]
    fn writes_need_the_writer_lock() {
        let mut h = Harness::new();
        let e = h.run(|ctx| ctx.write_ctx().map(|_| ())).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
    }
}
