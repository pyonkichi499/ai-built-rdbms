//! Storage: VFS, storage manager, buffer pool, pages, heap and the
//! `TableStore` trait (`m2.md` §2, §4.3, §4.4).
//!
//! Dependency direction: `vfs` ← `smgr` / `page` / `checksum` ← `buffer` ←
//! `heap` ← `heap_store` ← `stack`.

pub mod buffer;
pub mod checksum;
pub mod heap;
pub mod heap_store;
pub mod page;
pub mod smgr;
pub mod stack;
pub mod testing;
pub mod vfs;

use std::sync::Arc;

pub use self::heap::scan::HeapScan;
use self::smgr::RelFileLocator;
use crate::catalog::{TableDef, builtin};
use crate::error::Result;
use crate::txn::{CommandId, Snapshot, Xid};
use crate::types::{Datum, Oid, Row, Tid, oid};

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
pub const CATALOG_VERSION_NO: u32 = 2_026_100_401;
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
}

impl RelHandle {
    pub fn from_table(def: &TableDef) -> RelHandle {
        RelHandle {
            oid: def.oid,
            locator: def.locator,
            desc: Arc::new(TupleDesc::from_table(def)),
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
    /// caller's job (`Transaction::pending_creates`).
    fn create_storage(&self, rel: RelFileLocator) -> Result<()>;
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
