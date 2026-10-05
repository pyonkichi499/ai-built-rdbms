//! 起動時のクラッシュリカバリの全手順と rmgr の振り分け（`m3.md` §4.7、§5.5、§6.8）。
//!
//! 担当 R が実装する。`dispatch` は A が置いた実装（rmgr ごとの REDO への振り分けだけ）、
//! `startup` はスタブ。

use std::sync::Arc;

use crate::control::ControlFileHandle;
use crate::error::{Error, Result};
use crate::storage::heap;
use crate::storage::smgr_wal;
use crate::storage::stack::{StackConfig, StorageStack};
use crate::storage::vfs::Vfs;
use crate::txn::{Xid, xact_wal};
use crate::types::Oid;
use crate::wal::{self, DecodedRecord, RedoCtx, RedoStats, RmgrId};

#[derive(Debug)]
pub struct StartupOutcome {
    pub stack: StorageStack,
    pub next_xid: Xid,
    pub next_oid: Oid,
    pub did_redo: bool,
    pub redo_stats: Option<RedoStats>,
}

/// `Cluster::open` の手順 4（§5.5）。制御ファイルは開いて検査済みのものを受け取る。
/// 担当 R が実装する。
pub fn startup(
    vfs: Arc<dyn Vfs>,
    control: &Arc<ControlFileHandle>,
    cfg: &StackConfig,
) -> Result<StartupOutcome> {
    let _ = (vfs, control, cfg);
    Err(Error::internal("recovery::startup: 担当 R が実装"))
}

/// rmgr の振り分け（ほかに置かない）。
pub fn dispatch(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    match rec.rmgr {
        RmgrId::Xlog => wal::xlog::redo(ctx, rec),
        RmgrId::Xact => xact_wal::redo(ctx, rec),
        RmgrId::Smgr => smgr_wal::redo(ctx, rec),
        RmgrId::Heap => heap::wal::redo(ctx, rec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::buffer::{BufferPool, NoWal};
    use crate::storage::smgr::StorageManager;
    use crate::storage::vfs::SimVfs;
    use crate::txn::clog::Clog;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    fn ctx() -> RedoCtx {
        let vfs: Arc<dyn Vfs> = Arc::new(SimVfs::new(1));
        let smgr = Arc::new(StorageManager::new(Arc::clone(&vfs), 1024));
        let pool = BufferPool::new(8, Arc::clone(&smgr), Arc::new(NoWal));
        let clog = Arc::new(Clog::open(vfs, Xid(3)).unwrap());
        RedoCtx {
            pool,
            smgr,
            ext: Box::new(clog),
            invalid: Mutex::new(wal::InvalidPages::default()),
            next_oid: AtomicU32::new(0),
            knobs: DebugKnobs::default(),
        }
    }

    fn rec(rmgr: RmgrId) -> DecodedRecord {
        DecodedRecord {
            start: wal::Lsn(0x20_0020),
            end: wal::Lsn(0x20_0040),
            xid: Xid(5),
            rmgr,
            info: 0,
            blocks: vec![],
            main: vec![],
        }
    }

    #[test]
    fn dispatch_routes_each_rmgr_to_its_owner() {
        let c = ctx();
        for (rmgr, owner) in [
            (RmgrId::Xlog, "xlog::redo"),
            (RmgrId::Xact, "xact_wal::redo"),
            (RmgrId::Smgr, "smgr_wal::redo"),
            (RmgrId::Heap, "heap::wal::redo"),
        ] {
            let e = dispatch(&c, &rec(rmgr)).unwrap_err();
            assert!(e.message.contains(owner), "{rmgr:?}: {}", e.message);
        }
    }
}
