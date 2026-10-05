//! SMGR rmgr: リレーションファイルの作成・切り詰めの WAL と REDO
//! （`m3.md` §3.8、§4.5、§6.5.2）。
//!
//! 担当 C が実装する。ここにあるのは A が置いたスタブ（API は §4.5 のとおり）。

#![allow(dead_code, clippy::needless_pass_by_value)]

use super::buffer::BufferPool;
use super::smgr::{BlockNumber, ForkNumber, RelFileLocator, StorageManager};
use crate::error::{Error, Result};
use crate::txn::Xid;
use crate::wal::{DecodedRecord, RedoCtx, Wal};

pub const SMGR_CREATE: u8 = 0x00;
pub const SMGR_TRUNCATE: u8 = 0x10;

/// 作成を WAL に記録してからファイルを作る（WAL の flush はしない。§6.5.2）。
pub fn log_and_create(
    wal: &Wal,
    smgr: &StorageManager,
    xid: Xid,
    rel: RelFileLocator,
    fork: ForkNumber,
) -> Result<()> {
    let _ = (wal, smgr, xid, rel, fork);
    Err(Error::internal("smgr_wal::log_and_create: 担当 C が実装"))
}

/// 切り詰めを WAL に記録して flush してから、バッファを捨てて切り詰める。
pub fn log_and_truncate(
    wal: &Wal,
    pool: &BufferPool,
    smgr: &StorageManager,
    xid: Xid,
    rel: RelFileLocator,
    fork: ForkNumber,
    nblocks: BlockNumber,
) -> Result<()> {
    let _ = (wal, pool, smgr, xid, rel, fork, nblocks);
    Err(Error::internal("smgr_wal::log_and_truncate: 担当 C が実装"))
}

/// SMGR rmgr の REDO。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    let _ = (ctx, rec);
    Err(Error::internal("smgr_wal::redo: 担当 C が実装"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_codes_are_distinct_rmgr_types() {
        assert_ne!(SMGR_CREATE, SMGR_TRUNCATE);
        // 上位 4 ビットだけを使う（§3.3）。
        assert_eq!(SMGR_CREATE & 0x0F, 0);
        assert_eq!(SMGR_TRUNCATE & 0x0F, 0);
    }
}
