//! `Cluster` (the whole server instance) and `DatabaseHandle`
//! (`m2.md` §4.8). Replaces M1's `Database`.
//!
//! 担当 G が実装する。ここにあるのは型と公開 API の骨格で、起動・接続・
//! 停止の本体は未実装（`open` / `connect` / `shutdown` はエラーを返す）。

#![allow(clippy::unimplemented)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::catalog::cache::CatalogCache;
use crate::catalog::store::{CatalogStore, RoleRow, SharedCatalogStore};
use crate::checkpoint::CheckpointerHandle;
use crate::control::ControlFileHandle;
use crate::datadir::{OidAllocator, PidFile};
use crate::error::{Error, Result};
use crate::storage::TableStore;
use crate::storage::stack::StorageStack;
use crate::storage::vfs::Vfs;
use crate::txn::TxnManager;
use crate::types::Oid;

fn pending() -> Error {
    Error::not_supported("cluster is not implemented yet")
}

#[derive(Debug, Clone)]
pub struct ClusterOptions {
    pub data_dir: PathBuf,
    /// Number of buffer frames. Default 16384 (128MB).
    pub shared_buffers: usize,
    pub max_connections: u32,
    /// Default 300 seconds.
    pub checkpoint_timeout: Duration,
    pub ignore_unclean_shutdown: bool,
}

/// The whole cluster: storage, transactions, catalogs of every database.
#[derive(Debug)]
pub struct Cluster {
    #[allow(dead_code)]
    stack: StorageStack,
    #[allow(dead_code)]
    pid_file: Mutex<Option<PidFile>>,
    #[allow(dead_code)]
    control: Arc<ControlFileHandle>,
    #[allow(dead_code)]
    txn: Arc<TxnManager>,
    #[allow(dead_code)]
    oids: OidAllocator,
    #[allow(dead_code)]
    shared: Arc<SharedCatalogStore>,
    #[allow(dead_code)]
    checkpoint_lock: Mutex<()>,
    #[allow(dead_code)]
    checkpointer: Mutex<Option<CheckpointerHandle>>,
    #[allow(dead_code)]
    databases: Mutex<HashMap<Oid, Arc<DatabaseHandle>>>,
    poisoned: AtomicBool,
    next_session_id: AtomicU64,
}

impl Cluster {
    /// Starts the cluster (`m2.md` §5.1). The checkpointer thread gets a
    /// `Weak<Cluster>` and a stop channel (no reference cycle).
    pub fn open(_vfs: Arc<dyn Vfs>, _opts: ClusterOptions) -> Result<Arc<Cluster>> {
        Err(pending())
    }

    /// `3D000` / `55000` / `28000`, all FATAL.
    pub fn connect(&self, _database: &str, _user: &str) -> Result<(Arc<DatabaseHandle>, RoleRow)> {
        Err(pending())
    }

    /// Calls `checkpoint::run(.., CheckpointKind::Explicit)` (§5.6).
    pub fn checkpoint(&self) -> Result<()> {
        Err(pending())
    }

    /// Called after new connections are refused and all sessions have
    /// ended. Stops and joins the checkpointer first (§5.7).
    pub fn shutdown(&self) -> Result<()> {
        Err(pending())
    }

    /// Tests only: stops the checkpointer without writing anything and drops
    /// the cluster (pairs with `SimVfs::crash`).
    pub fn abandon(self: Arc<Self>) {
        unimplemented!("担当 G が実装")
    }

    pub fn poison(&self) {
        self.poisoned.store(true, Ordering::SeqCst);
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    pub fn txn_manager(&self) -> &Arc<TxnManager> {
        unimplemented!("担当 G が実装")
    }

    pub fn oid_allocator(&self) -> &OidAllocator {
        unimplemented!("担当 G が実装")
    }

    pub fn storage(&self) -> &Arc<dyn TableStore> {
        unimplemented!("担当 G が実装")
    }

    /// Allocates a session ID (passed to `begin_write`); called once by
    /// `Session::new`.
    pub fn next_session_id(&self) -> u64 {
        self.next_session_id.fetch_add(1, Ordering::Relaxed)
    }

    /// `invalidate_all` on every database's `CatalogCache` (§5.3).
    pub fn invalidate_all_catalog_caches(&self) {
        unimplemented!("担当 G が実装")
    }
}

/// Per-database state.
#[derive(Debug)]
pub struct DatabaseHandle {
    pub oid: Oid,
    pub name: String,
    pub catalog: CatalogStore,
    pub shared: Arc<SharedCatalogStore>,
    pub cache: CatalogCache,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vfs::SimVfs;

    #[test]
    fn open_is_not_implemented_yet() {
        let opts = ClusterOptions {
            data_dir: PathBuf::from("/nonexistent"),
            shared_buffers: 64,
            max_connections: 10,
            checkpoint_timeout: Duration::from_secs(300),
            ignore_unclean_shutdown: false,
        };
        let e = Cluster::open(Arc::new(SimVfs::new(1)), opts).unwrap_err();
        assert_eq!(e.sqlstate, crate::error::sqlstate::FEATURE_NOT_SUPPORTED);
    }
}
