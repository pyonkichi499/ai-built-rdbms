//! Tuple format (35-byte header, null bitmap, column encoding)
//! (`m2.md` §3.5, §3.6, §6.5.1).
//!
//! 担当 D が実装する。

#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]

use crate::error::{Error, Result};
use crate::storage::{TupleDesc, WriteCtx};
use crate::txn::{CommandId, Xid};
use crate::types::{Datum, Row, Tid};

/// Flags for [`form_tuple`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TupleFlags {
    /// Set `HEAP_UPDATED`.
    pub updated: bool,
}

/// Encodes a row. Fails with `54000` if the row is too big, `0A000` for a
/// NULL-only type with a value.
pub fn form_tuple(
    _desc: &TupleDesc,
    _row: &[Datum],
    _w: &WriteCtx,
    _flags: TupleFlags,
) -> Result<Vec<u8>> {
    Err(Error::not_supported("heap tuples are not implemented yet"))
}

/// Decodes a tuple. Damaged data gives `XX001`.
pub fn deform_tuple(_desc: &TupleDesc, _bytes: &[u8]) -> Result<Row> {
    Err(Error::not_supported("heap tuples are not implemented yet"))
}

/// The fixed part of a tuple header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TupleHeader {
    pub xmin: Xid,
    pub xmax: Xid,
    pub cmin: CommandId,
    pub cmax: CommandId,
    pub ctid: Tid,
    pub infomask2: u16,
    pub infomask: u16,
    pub hoff: u8,
}

impl TupleHeader {
    pub fn read(_bytes: &[u8]) -> Result<TupleHeader> {
        Err(Error::not_supported("heap tuples are not implemented yet"))
    }

    pub fn write(&self, _bytes: &mut [u8]) {
        // 担当 D が実装する。
    }
}
