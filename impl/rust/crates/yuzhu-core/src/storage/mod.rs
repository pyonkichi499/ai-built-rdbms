//! Storage: VFS, storage manager, buffer pool, pages, heap and the
//! `TableStore` trait (`m2.md` §2, §4.3, §4.4).
//!
//! Dependency direction: `vfs` ← `smgr` / `page` / `checksum` ← `buffer` ←
//! `heap` ← `heap_store` ← `stack`.

pub mod btree;
pub mod buffer;
pub mod checksum;
pub mod heap;
pub mod heap_store;
pub mod index_store;
pub mod page;
pub mod sequence;
pub mod smgr;
pub mod smgr_wal;
pub mod stack;
pub mod testing;
pub mod vfs;

use std::sync::Arc;

pub use self::heap::scan::HeapScan;
use self::smgr::RelFileLocator;
use crate::catalog::{IndexDef, SequenceParams, TableDef, builtin};
use crate::error::{Error, Result};
use crate::txn::{CommandId, Snapshot, Xid};
use crate::types::{CmpFn, Datum, Oid, Row, SqlType, Tid, cmp_datum, oid};
use crate::wal::Lsn;

// ----- constants (`m2.md` §3.10) ------------------------------------------

pub const BLCKSZ: usize = 8192;
pub const MAXALIGN: usize = 8;
/// 1GB / 8KB.
pub const DEFAULT_RELSEG_SIZE: u32 = 131_072;
pub const SIZE_OF_PAGE_HEADER: usize = 24;
pub const SIZE_OF_HEAP_TUPLE_HEADER: usize = 35;
/// `BLCKSZ - MAXALIGN(24 + 4)`.
pub const MAX_HEAP_TUPLE_SIZE: usize = 8160;
/// `(8192 - 24) / (MAXALIGN(35) + 4)`.
pub const MAX_HEAP_TUPLES_PER_PAGE: usize = 185;
pub const MAX_HEAP_ATTRIBUTE_NUMBER: usize = 1600;
pub const PAGE_LAYOUT_VERSION: u8 = 1;
pub const CATALOG_VERSION_NO: u32 = 2_026_100_601;
/// How many XIDs are reserved in the control file at a time.
pub const XID_PREFETCH: u64 = 1024;
/// How many OIDs are reserved in the control file at a time (same as
/// PostgreSQL's `VAR_OID_PREFETCH`; unverified).
pub const OID_PREFETCH: u32 = 8192;

// ----- tuple descriptors ---------------------------------------------------

/// `pg_attribute.attalign`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Char = 1,
    Short = 2,
    Int = 4,
    Double = 8,
}

#[derive(Clone, Debug)]
pub struct AttrDesc {
    pub type_oid: Oid,
    pub len: i16,
    pub align: Align,
    pub byval: bool,
}

impl AttrDesc {
    /// Builds the descriptor of a column of type `type_oid` from the type
    /// tables (`typlen`, `typbyval`, `typalign`).
    pub fn from_type(type_oid: Oid) -> AttrDesc {
        let (len, byval) = match builtin::type_by_oid(type_oid) {
            Some(t) => (t.typlen, t.typbyval),
            None => fallback_len_byval(type_oid),
        };
        AttrDesc {
            type_oid,
            len,
            align: type_align(type_oid),
            byval,
        }
    }
}

/// `(typlen, typbyval)` of the M2 system types that `builtin::TYPES` may not
/// list yet; unknown types are treated as varlena.
fn fallback_len_byval(type_oid: Oid) -> (i16, bool) {
    match type_oid {
        oid::CHAR => (1, true),
        oid::REGPROC | oid::XID | oid::CID => (4, true),
        oid::TID => (6, false),
        oid::TIMESTAMPTZ => (8, true),
        oid::ACLITEM => (16, false),
        _ => (-1, false),
    }
}

/// `pg_type.typalign` (`m2.md` §1.3).
fn type_align(type_oid: Oid) -> Align {
    match type_oid {
        oid::BOOL | oid::CHAR | oid::NAME | oid::UNKNOWN => Align::Char,
        oid::INT2 | oid::TID => Align::Short,
        oid::INT8
        | oid::FLOAT8
        | oid::TIMESTAMPTZ
        | oid::ACLITEM
        | oid::ACLITEM_ARRAY
        | oid::ANYARRAY => Align::Double,
        _ => Align::Int,
    }
}

#[derive(Clone, Debug)]
pub struct TupleDesc {
    pub attrs: Vec<AttrDesc>,
}

impl TupleDesc {
    /// One attribute per column, in attnum order.
    pub fn from_table(def: &TableDef) -> TupleDesc {
        TupleDesc {
            attrs: def
                .columns
                .iter()
                .map(|c| AttrDesc::from_type(c.ty.oid))
                .collect(),
        }
    }
}

/// Everything needed to open a relation, built at the start of a statement.
#[derive(Clone, Debug)]
pub struct RelHandle {
    pub oid: Oid,
    pub locator: RelFileLocator,
    pub desc: Arc<TupleDesc>,
    /// `TableDef.indexes` の実行用の写し（M4）。
    pub indexes: Arc<[IndexHandle]>,
}

impl RelHandle {
    /// `indexes` も作る。
    pub fn from_table(def: &TableDef) -> RelHandle {
        RelHandle {
            oid: def.oid,
            locator: def.locator,
            desc: Arc::new(TupleDesc::from_table(def)),
            indexes: def
                .indexes
                .iter()
                .map(|i| IndexHandle::from_def(i, def))
                .collect(),
        }
    }
}

/// 索引のキー列 1 つの実行用の情報。
#[derive(Clone, Debug)]
pub struct IndexKeyColumn {
    pub attnum: i16,
    pub name: String,
    pub ty: SqlType,
    pub attr: AttrDesc,
    /// 比較関数。B2 が `catalog::opclass::comparator` に切り替えるまでは `cmp_datum`。
    pub cmp: CmpFn,
    pub descending: bool,
    pub nulls_first: bool,
}

/// 索引を開くのに必要なものすべて（文の開始時に作る）。
#[derive(Clone, Debug)]
pub struct IndexHandle {
    pub oid: Oid,
    pub locator: RelFileLocator,
    /// エラーメッセージ用。
    pub schema: String,
    pub name: String,
    pub table_name: String,
    pub unique: bool,
    pub primary: bool,
    pub columns: Arc<[IndexKeyColumn]>,
}

impl IndexHandle {
    /// `def`（`table` の索引）から作る。キー列の `attnum` が表にない定義は壊れているので、
    /// その列は型 `unknown` のテキスト扱いにする（呼び出し側が先に検査している前提）。
    pub fn from_def(def: &IndexDef, table: &TableDef) -> IndexHandle {
        let columns = def
            .columns
            .iter()
            .map(|c| {
                let col = table.columns.iter().find(|tc| tc.attnum == c.attnum);
                let ty = col.map_or(SqlType::UNKNOWN, |tc| tc.ty);
                IndexKeyColumn {
                    attnum: c.attnum,
                    name: col.map_or_else(String::new, |tc| tc.name.clone()),
                    ty,
                    attr: AttrDesc::from_type(ty.oid),
                    cmp: cmp_datum,
                    descending: c.descending,
                    nulls_first: c.nulls_first,
                }
            })
            .collect();
        IndexHandle {
            oid: def.oid,
            locator: def.locator,
            schema: table.schema.clone(),
            name: def.name.clone(),
            table_name: table.name.clone(),
            unique: def.unique,
            primary: def.primary,
            columns,
        }
    }
}

// ----- table access --------------------------------------------------------

/// What a write needs to know.
#[derive(Clone, Copy, Debug)]
pub struct WriteCtx {
    pub xid: Xid,
    pub cid: CommandId,
}

/// PostgreSQL's `TM_Result` (`src/include/access/tableam.h`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TmResult {
    Ok,
    Invisible,
    SelfModified { cmax: CommandId },
    Updated { ctid: Tid, xmax: Xid },
    Deleted { xmax: Xid },
    BeingModified { xmax: Xid },
    WouldBlock,
}

#[derive(Clone, Copy, Debug)]
pub struct UpdateOutcome {
    pub result: TmResult,
    pub new_tid: Option<Tid>,
}

/// One row returned by a scan, with its system column values.
#[derive(Clone, Debug)]
pub struct HeapTuple {
    pub tid: Tid,
    pub xmin: Xid,
    pub xmax: Xid,
    pub cmin: CommandId,
    pub cmax: CommandId,
    /// User columns only; columns beyond the tuple's `natts` are NULL.
    pub row: Row,
}

pub trait TableStore: Send + Sync + std::fmt::Debug {
    /// Creates the file. Tying the creation to the transaction is the
    /// caller's job (`Transaction::pending_creates`). `w.xid` is the XID of
    /// the `SMGR_CREATE` WAL record (`m3.md` §4.5, §4.11).
    fn create_storage(&self, w: &WriteCtx, rel: RelFileLocator) -> Result<()>;
    /// Whether the file exists (a 0-byte leftover of D13 counts).
    fn storage_exists(&self, rel: RelFileLocator) -> Result<bool>;
    /// Drops buffers and the insertion hint, then `smgr.unlink` (D13).
    fn unlink_storage(&self, rel: RelFileLocator) -> Result<()>;

    fn insert(&self, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid>;
    fn delete(&self, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid) -> Result<TmResult>;
    /// Sets xmax / cmax / ctid on the old version and inserts the new one in
    /// one call (one WAL record in M3).
    fn update(
        &self,
        rel: &RelHandle,
        w: &WriteCtx,
        snap: &Snapshot,
        tid: Tid,
        new_row: &[Datum],
    ) -> Result<UpdateOutcome>;

    fn begin_scan(&self, rel: &RelHandle, snap: &Snapshot) -> Result<HeapScan>;
    fn scan_next(&self, scan: &mut HeapScan) -> Result<Option<HeapTuple>>;
    /// Reads one row by TID (index scans in M4; tests in M2).
    fn fetch(&self, rel: &RelHandle, snap: &Snapshot, tid: Tid) -> Result<Option<HeapTuple>>;

    /// 全バージョンの走査（可視性判定なし）。CREATE INDEX の構築用。`HeapTuple.row` は全ユーザー列。
    /// 既定は未実装（H4 が `HeapStore` に実装する）。
    fn begin_scan_all(&self, _rel: &RelHandle) -> Result<HeapScan> {
        Err(Error::not_supported(
            "full heap scans are not supported yet",
        ))
    }

    /// タプルの状態（clog を引く）。`own` は呼び出し側のトランザクションの XID。
    /// 既定は未実装（H4 が実装する）。
    fn tuple_state(&self, _t: &HeapTuple, _own: Option<Xid>) -> Result<TupleState> {
        Err(Error::not_supported("tuple_state is not supported yet"))
    }

    /// `SnapshotDirty` 相当: 一意性検査用。コマンド ID は見ない。既定は未実装（H4 が実装する）。
    fn fetch_dirty(&self, _rel: &RelHandle, _own: Option<Xid>, _tid: Tid) -> Result<DirtyResult> {
        Err(Error::not_supported("fetch_dirty is not supported yet"))
    }

    /// 計画時の大きさの手がかり（ブロック数）。既定は未実装（H4 が実装する）。
    fn nblocks(&self, _rel: &RelHandle) -> Result<u32> {
        Err(Error::not_supported("nblocks is not supported yet"))
    }
}

/// `fetch_dirty` の結果（一意性検査用）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirtyResult {
    /// 見えない（中断した挿入、自分が削除済み、コミット済みの削除）。
    Invisible,
    /// 生きている（コミット済み、または自分の挿入）。
    Visible,
    /// 他のトランザクションが挿入中・削除中（M4 では起きない。起きたら内部エラー）。
    WaitFor(Xid),
}

/// タプルの状態（`TableStore::tuple_state`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TupleState {
    InsertAborted,
    Live,
    DeadCommitted,
    DeletedBySelf,
    InsertInProgress(Xid),
    DeleteInProgress(Xid),
}

// ----- indexes (`m4/00-contracts.md` §13.2) -------------------------------

/// 索引走査の向き。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScanDirection {
    Forward,
    Backward,
}

/// 評価済みの索引走査キー。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedScanKeys {
    /// 先頭から等値の列。`None` は IS NULL。
    pub eq: Vec<Option<Datum>>,
    /// `eq.len()` 番目の列の範囲（値, 境界を含むか）。
    pub lower: Option<(Datum, bool)>,
    pub upper: Option<(Datum, bool)>,
}

/// `IndexStore::insert` の一意性検査の指定。
#[derive(Clone, Copy, Debug)]
pub enum UniqueCheck<'a> {
    Skip,
    Check {
        heap: &'a dyn TableStore,
        rel: &'a RelHandle,
        own_xid: Xid,
    },
}

/// `IndexStore::build` の一意性検査の指定。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuildUnique {
    No,
    Yes,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub tuples: u64,
    pub pages: u32,
    pub levels: u32,
}

/// 索引走査の状態（実体は `storage/btree/scan.rs`）。ピンもラッチも持たない（M2 D10 と同じ方針）。
/// 単体テスト用の `for_test` / `fake_tids`（`executor/nodes/test_util.rs` が使う）も scan.rs にある。
pub use self::btree::scan::IndexScan;

pub trait IndexStore: Send + Sync + std::fmt::Debug {
    /// 新しい索引のファイル（作成済み）に、メタページと空のルート葉を書く（`BTREE_PAGES`）。
    fn init_index(&self, w: &WriteCtx, index: &IndexHandle) -> Result<()>;

    /// 項目を 1 つ入れる。`key` は索引列の値（NULL を含みうる）。一意索引で `check` が `Check` なら
    /// 重複を検査する（NULL を含むキーは検査しない）。重複は 23505（`s` `t` `n` を付けて返す。
    /// DETAIL は `executor/dml.rs` の `unique_violation_detail` が補う。11 §7.1 の C-11）。
    /// 他トランザクションの待ちが必要なら内部エラー（M4）。
    fn insert(
        &self,
        w: &WriteCtx,
        index: &IndexHandle,
        key: &[Datum],
        tid: Tid,
        check: UniqueCheck<'_>,
    ) -> Result<()>;

    /// 一括構築。`init_index` 済みの空の索引にだけ使える。`entries` は (key, tid) の昇順。
    fn build(
        &self,
        w: &WriteCtx,
        index: &IndexHandle,
        entries: &mut dyn Iterator<Item = (Vec<Datum>, Tid)>,
        unique: BuildUnique,
    ) -> Result<BuildStats>;

    fn begin_scan(
        &self,
        index: &IndexHandle,
        keys: &ResolvedScanKeys,
        dir: ScanDirection,
    ) -> Result<IndexScan>;

    /// 次の TID。葉ごとに一致した TID をまとめてコピーし、ページのラッチ・ピンを持ち越さない。
    fn scan_next(&self, scan: &mut IndexScan) -> Result<Option<Tid>>;

    fn nblocks(&self, index: &IndexHandle) -> Result<u32>;

    fn unlink_storage(&self, index: &IndexHandle) -> Result<()>;
}

// ----- sequences (`m4/00-contracts.md` §13.3) -----------------------------

#[derive(Clone, Debug)]
pub struct SequenceHandle {
    pub oid: Oid,
    /// エラーメッセージ（`nextval: reached maximum value of sequence "%s" (%d)`）用。
    pub name: String,
    pub locator: RelFileLocator,
    pub params: SequenceParams,
}

/// `SequenceStore::fetch` が払い出した値の連なり。
#[derive(Clone, Copy, Debug)]
pub struct SeqRun {
    pub first: i64,
    pub count: u32,
    pub increment: i64,
    /// この呼び出しの後のページの LSN（払い出した値を覆う `SEQ_LOG` の終端。他のセッションが書いた
    /// ものを含む）。コミット時にここまで flush する（`Transaction.wal_flush_upto` へ反映する）。
    pub wal_lsn: Lsn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqState {
    pub last_value: i64,
    pub log_cnt: i64,
    pub is_called: bool,
}

pub trait SequenceStore: Send + Sync + std::fmt::Debug {
    /// 1 ページを初期化して `SEQ_LOG` を書く（ファイルは作成済み）。
    fn init(&self, w: &WriteCtx, seq: &SequenceHandle) -> Result<Lsn>;

    /// 最大 `count` 個の値を払い出す（CACHE。上限・下限・CYCLE の検査を含む。2200H）。
    /// 排他ラッチの下でその場上書きする。
    fn fetch(&self, seq: &SequenceHandle, count: u32) -> Result<SeqRun>;

    fn setval(&self, seq: &SequenceHandle, value: i64, is_called: bool) -> Result<Lsn>;

    fn read(&self, seq: &SequenceHandle) -> Result<SeqState>;

    /// ALTER SEQUENCE / TRUNCATE ... RESTART IDENTITY 用。
    fn reset(
        &self,
        seq: &SequenceHandle,
        new_params: &SequenceParams,
        restart_with: Option<i64>,
    ) -> Result<Lsn>;

    /// 全セッションの先取り分を捨てさせる世代（`reset` が +1。クラスタごとに 1 つ）。
    fn reset_generation(&self) -> u64;

    fn unlink_storage(&self, seq: &SequenceHandle) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::table_def;
    use crate::catalog::{ColumnDef, TableDef};
    use crate::types::SqlType;

    fn col(name: &str, attnum: i16, ty: SqlType) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            attnum,
            ty,
            not_null: false,
            default: None,
            identity: None,
        }
    }

    fn def() -> TableDef {
        table_def(
            16384,
            "t",
            vec![
                col("a", 1, SqlType::INT4),
                col("b", 2, SqlType::TEXT),
                col("c", 3, SqlType::INT8),
                col("d", 4, SqlType::BOOL),
                col("e", 5, SqlType::INT2),
                col("f", 6, SqlType::of(oid::TID)),
            ],
            vec![],
        )
    }

    #[test]
    fn tuple_desc_matches_pg_type_properties() {
        let d = TupleDesc::from_table(&def());
        let got: Vec<_> = d.attrs.iter().map(|a| (a.len, a.align, a.byval)).collect();
        assert_eq!(
            got,
            vec![
                (4, Align::Int, true),
                (-1, Align::Int, false),
                (8, Align::Double, true),
                (1, Align::Char, true),
                (2, Align::Short, true),
                (6, Align::Short, false),
            ]
        );
    }

    #[test]
    fn rel_handle_copies_table_identity() {
        let t = def();
        let h = RelHandle::from_table(&t);
        assert_eq!(h.oid, t.oid);
        assert_eq!(h.locator, t.locator);
        assert_eq!(h.desc.attrs.len(), 6);
    }

    #[test]
    fn constants_are_consistent() {
        assert_eq!(DEFAULT_RELSEG_SIZE as usize * BLCKSZ, 1 << 30);
        assert_eq!(MAX_HEAP_TUPLE_SIZE, BLCKSZ - 32);
        assert_eq!(
            (BLCKSZ - SIZE_OF_PAGE_HEADER) / (40 + 4),
            MAX_HEAP_TUPLES_PER_PAGE
        );
    }
}
