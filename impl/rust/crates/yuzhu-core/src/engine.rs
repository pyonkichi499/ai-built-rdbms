//! `Database`: owner of the catalog, storage and server-wide defaults.
//!
//! M1 serializes statement execution with one `Mutex` around the shared
//! state (`m1.md` §3.4). On top of that, a database-wide *writer lock*
//! (single writer, as in `m2.md` D3 (c)) is held from a transaction's first
//! writing statement until the transaction ends, so that undoing one
//! session's uncommitted changes can never collide with another session's
//! changes (e.g. a re-created table name, or rows inserted into a table
//! whose CREATE is later rolled back).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use crate::catalog::memory::MemoryCatalog;
use crate::storage::memory::MemoryTableStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    /// The only database in M1. Default `"postgres"` (driver default).
    pub database_name: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        DatabaseConfig {
            database_name: "postgres".into(),
        }
    }
}

/// State shared by all sessions, guarded by `Database::lock`.
#[derive(Debug)]
pub struct DbState {
    pub catalog: MemoryCatalog,
    pub storage: MemoryTableStore,
}

#[derive(Debug)]
pub struct Database {
    config: DatabaseConfig,
    state: Mutex<DbState>,
    /// Session id holding the writer lock, if any.
    writer: Mutex<Option<u64>>,
    writer_released: Condvar,
    next_session_id: AtomicU64,
}

impl Database {
    pub fn new(config: DatabaseConfig) -> Arc<Database> {
        let state = DbState {
            catalog: MemoryCatalog::new(config.database_name.clone()),
            storage: MemoryTableStore::new(),
        };
        Arc::new(Database {
            config,
            state: Mutex::new(state),
            writer: Mutex::new(None),
            writer_released: Condvar::new(),
            next_session_id: AtomicU64::new(1),
        })
    }

    pub fn config(&self) -> &DatabaseConfig {
        &self.config
    }

    /// Locks the shared state for the duration of one statement.
    pub fn lock(&self) -> MutexGuard<'_, DbState> {
        // A poisoned lock means a statement panicked; the catalog and
        // storage maps are still structurally valid.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new identifier for a session (used as the writer lock owner).
    pub fn new_session_id(&self) -> u64 {
        self.next_session_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Acquires the writer lock for `session`, waiting while another
    /// session holds it. Re-acquiring by the owner is a no-op. Must not be
    /// called while holding [`Database::lock`].
    pub fn acquire_writer(&self, session: u64) {
        let mut owner = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(o) = *owner {
            if o == session {
                return;
            }
            owner = self
                .writer_released
                .wait(owner)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *owner = Some(session);
    }

    /// Releases the writer lock if `session` holds it.
    pub fn release_writer(&self, session: u64) {
        let mut owner = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if *owner == Some(session) {
            *owner = None;
            self.writer_released.notify_all();
        }
    }

    /// The session currently holding the writer lock.
    pub fn writer_owner(&self) -> Option<u64> {
        *self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
