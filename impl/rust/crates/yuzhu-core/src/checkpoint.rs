//! The checkpoint procedure and the checkpointer thread (`m2.md` §4.8,
//! §5.6). This module is the only owner of the procedure; `Cluster` just
//! calls [`run`].

use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::control::{ControlFileHandle, DbState};
use crate::datadir::OidAllocator;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::buffer::BufferPool;
use crate::storage::smgr::StorageManager;
use crate::txn::TxnManager;
use crate::txn::clog::Clog;
use crate::util::sync::lock;

/// Borrowed pieces a checkpoint needs.
#[derive(Debug)]
pub struct CheckpointParts<'a> {
    pub pool: &'a BufferPool,
    pub smgr: &'a StorageManager,
    pub clog: &'a Clog,
    pub txn: &'a TxnManager,
    pub control: &'a ControlFileHandle,
    pub oids: &'a OidAllocator,
    pub checkpoint_lock: &'a Mutex<()>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckpointKind {
    Periodic,
    Explicit,
    /// Writes the exact `next_xid` and sets `state` to `ShutDown` (§6.7.3).
    /// The others write the look-ahead limit and leave `state` alone.
    Shutdown,
}

/// Runs one checkpoint (§5.6). One checkpoint runs at a time.
///
/// The caller must not hold the storage barrier (shared or exclusive) nor
/// the `proc` / control file mutexes; the writer lock does not matter.
///
/// An error from the buffer flush or from `sync_pending` is already
/// `Severity::Panic` (they own the fsync policy); this function returns it
/// as is. The control file is not touched in that case, so a failed
/// shutdown checkpoint never marks the cluster `ShutDown`.
#[allow(clippy::similar_names)]
pub fn run(parts: &CheckpointParts<'_>, kind: CheckpointKind) -> Result<()> {
    let _serial = lock(parts.checkpoint_lock)?;

    // After a failed write or fsync the dirty/pending sets were already
    // discarded, so a retry would find nothing to write and look durable.
    // Refuse (also for Shutdown, so the state never becomes ShutDown).
    if parts.pool.is_poisoned() || parts.smgr.is_broken() {
        return Err(Error::new(
            sqlstate::IO_ERROR,
            "refusing to checkpoint: a previous write or fsync failed",
        )
        .with_severity(Severity::Panic));
    }
    run_locked(parts, kind).inspect_err(|e| {
        if e.severity == Severity::Panic {
            parts.pool.poison_flag().set();
        }
    })
}

#[allow(clippy::similar_names)]
fn run_locked(parts: &CheckpointParts<'_>, kind: CheckpointKind) -> Result<()> {
    // 1-3. Write the pages that are dirty now. The shared barrier keeps a
    // DROP cleanup from removing files under the flush.
    {
        let _barrier = parts.txn.statement_barrier()?;
        parts.pool.flush_all_for_checkpoint()?;
    }
    // 4-5. The commit log, then the data files.
    parts.clog.flush()?;
    parts.smgr.sync_pending()?;

    // 6. Read the counters first: the `proc` and OID mutexes are released
    // before the control file mutex is taken (§5.9).
    let (next_xid, xid_limit) = parts.txn.xid_counters();
    let next_oid = parts.oids.limit();

    // 7. Record them. `max` keeps the control file from moving backwards
    // if a pre-allocation raced with us (not needed for Shutdown, which
    // may lower `next_xid` on purpose).
    match kind {
        CheckpointKind::Periodic | CheckpointKind::Explicit => {
            parts.control.update(|c| {
                c.next_xid = c.next_xid.max(xid_limit.0);
                c.next_oid = c.next_oid.max(next_oid);
            })?;
        }
        CheckpointKind::Shutdown => {
            parts.control.update_for_shutdown(|c| {
                c.next_xid = next_xid.0;
                c.next_oid = next_oid;
                c.state = DbState::ShutDown;
            })?;
        }
    }

    // 8. Leftovers of dropped relations; the commit already happened, so a
    // failure is only a warning.
    if let Err(e) = parts.smgr.finish_pending_unlinks() {
        warn(&format!(
            "checkpoint could not remove files of dropped relations: {}",
            e.message
        ));
    }
    Ok(())
}

#[allow(clippy::print_stderr)]
fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}

/// Starts the checkpointer thread. `tick` borrows the parts for each run
/// (it upgrades a `Weak<Cluster>`) and returns false when the cluster is
/// gone, which ends the thread. Errors are `tick`'s to log.
pub fn spawn_checkpointer(
    interval: Duration,
    tick: Box<dyn Fn() -> bool + Send>,
) -> CheckpointerHandle {
    let (stop, rx) = mpsc::channel::<()>();
    let join = std::thread::Builder::new()
        .name("checkpointer".to_string())
        .spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(interval) {
                if !tick() {
                    break;
                }
            }
        })
        .expect("could not spawn the checkpointer thread");
    CheckpointerHandle { stop, join }
}

#[derive(Debug)]
pub struct CheckpointerHandle {
    stop: mpsc::Sender<()>,
    join: JoinHandle<()>,
}

impl CheckpointerHandle {
    /// Asks the thread to stop and waits for it (a checkpoint in progress
    /// finishes first). A thread that panicked is ignored.
    pub fn stop_and_join(self) {
        let _ = self.stop.send(());
        let _ = self.join.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    #[test]
    fn checkpointer_ticks_and_stops() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = Arc::clone(&n);
        let h = spawn_checkpointer(
            Duration::from_millis(10),
            Box::new(move || {
                n2.fetch_add(1, Ordering::SeqCst);
                true
            }),
        );
        let start = Instant::now();
        while n.load(Ordering::SeqCst) < 3 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(n.load(Ordering::SeqCst) >= 3);
        h.stop_and_join();
        let after = n.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(n.load(Ordering::SeqCst), after);
    }

    #[test]
    fn stop_does_not_wait_for_the_interval() {
        let h = spawn_checkpointer(Duration::from_secs(3600), Box::new(|| true));
        let start = Instant::now();
        h.stop_and_join();
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn thread_ends_when_tick_returns_false() {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = Arc::clone(&n);
        let h = spawn_checkpointer(
            Duration::from_millis(5),
            Box::new(move || {
                n2.fetch_add(1, Ordering::SeqCst);
                false
            }),
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(n.load(Ordering::SeqCst), 1);
        h.stop_and_join(); // must not hang
    }

    #[test]
    fn panicking_tick_does_not_break_stop() {
        let h = spawn_checkpointer(Duration::from_millis(5), Box::new(|| panic!("boom")));
        std::thread::sleep(Duration::from_millis(50));
        h.stop_and_join();
    }

    // ---- run() ----

    use crate::control::ControlData;
    use crate::storage::buffer::NoWal;
    use crate::storage::vfs::{SimVfs, Vfs};
    use crate::txn::Xid;
    use crate::txn::clog::XidStatus;
    use std::path::Path;

    fn control_data() -> ControlData {
        ControlData {
            format_version: 1,
            catalog_version: 1,
            system_identifier: 1,
            state: DbState::InProduction,
            page_size: 8192,
            wal_segment_size: 0,
            flags: 0,
            time: 0,
            checkpoint_lsn: 0,
            redo_lsn: 0,
            next_xid: 3,
            oldest_xid: 3,
            next_oid: 16384,
            timeline: 1,
            min_recovery_lsn: 0,
            rel_seg_blocks: 131_072,
            data_layout_version: 1,
            builtin_hash: 0,
        }
    }

    struct Rig {
        sim: SimVfs,
        pool: Arc<BufferPool>,
        smgr: Arc<StorageManager>,
        clog: Arc<Clog>,
        txn: Arc<TxnManager>,
        control: Arc<ControlFileHandle>,
        oids: OidAllocator,
        lock: Mutex<()>,
    }

    impl Rig {
        fn new() -> Rig {
            let sim = SimVfs::new(11);
            for d in ["pg_xact", "global"] {
                sim.create_dir_all(Path::new(d)).unwrap();
            }
            let vfs: Arc<dyn Vfs> = Arc::new(sim.clone());
            let control = Arc::new(ControlFileHandle::create(&vfs, &control_data()).unwrap());
            let smgr = Arc::new(StorageManager::new(Arc::clone(&vfs), 131_072));
            let pool = BufferPool::new(16, Arc::clone(&smgr), Arc::new(NoWal));
            let clog = Arc::new(Clog::open(vfs, Xid(3)).unwrap());
            let txn = Arc::new(TxnManager::new(Arc::clone(&clog), Arc::clone(&control)));
            let oids = OidAllocator::new(Arc::clone(&control));
            Rig {
                sim,
                pool,
                smgr,
                clog,
                txn,
                control,
                oids,
                lock: Mutex::new(()),
            }
        }

        fn parts(&self) -> CheckpointParts<'_> {
            CheckpointParts {
                pool: &self.pool,
                smgr: &self.smgr,
                clog: &self.clog,
                txn: &self.txn,
                control: &self.control,
                oids: &self.oids,
                checkpoint_lock: &self.lock,
            }
        }
    }

    #[test]
    fn periodic_checkpoint_flushes_clog_and_records_limits() {
        let r = Rig::new();
        let (x, g) = r.txn.begin_write(1, None).unwrap();
        r.txn.commit(x).unwrap();
        drop(g);
        run(&r.parts(), CheckpointKind::Periodic).unwrap();
        let c = r.control.get();
        assert_eq!(c.next_xid, r.txn.xid_counters().1.0);
        assert_eq!(c.state, DbState::InProduction);
        assert!(r.sim.exists(Path::new("pg_xact/000000000000")).unwrap());
        let vfs: Arc<dyn Vfs> = Arc::new(r.sim.clone());
        let reopened = Clog::open(vfs, Xid(4)).unwrap();
        assert_eq!(reopened.status(x).unwrap(), XidStatus::Committed);
    }

    #[test]
    fn shutdown_checkpoint_writes_exact_counters_and_state() {
        let r = Rig::new();
        let (x, g) = r.txn.begin_write(1, None).unwrap();
        r.txn.commit(x).unwrap();
        drop(g);
        run(&r.parts(), CheckpointKind::Shutdown).unwrap();
        let c = r.control.get();
        assert_eq!(c.next_xid, r.txn.next_xid().0);
        assert_eq!(c.state, DbState::ShutDown);
        assert_eq!(c.next_oid, r.oids.limit());
    }

    #[test]
    fn failed_clog_sync_is_panic_and_leaves_control_alone() {
        use crate::error::Severity;
        use crate::storage::vfs::{FaultEffect, FaultOp, FaultPlan, FaultRule};
        let r = Rig::new();
        let (x, g) = r.txn.begin_write(1, None).unwrap();
        r.txn.commit(x).unwrap();
        drop(g);
        let before = r.control.get();
        r.sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Sync,
                path_prefix: Some("pg_xact".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        let e = run(&r.parts(), CheckpointKind::Shutdown).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(r.control.get(), before);
    }

    #[test]
    fn checkpoint_is_refused_after_a_failed_clog_flush() {
        use crate::error::Severity;
        use crate::storage::vfs::{FaultEffect, FaultOp, FaultPlan, FaultRule};
        let r = Rig::new();
        let (x, g) = r.txn.begin_write(1, None).unwrap();
        r.txn.commit(x).unwrap();
        drop(g);
        r.sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Sync,
                path_prefix: Some("pg_xact".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        assert!(run(&r.parts(), CheckpointKind::Explicit).is_err());
        r.sim.set_faults(FaultPlan::default());
        let before = r.control.get();
        let e = run(&r.parts(), CheckpointKind::Shutdown).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(r.control.get(), before);
        assert_ne!(r.control.get().state, DbState::ShutDown);
    }

    #[test]
    fn checkpoint_is_refused_after_a_failed_data_fsync() {
        use crate::error::Severity;
        use crate::storage::smgr::{
            BufferTag, DEFAULTTABLESPACE_OID, ForkNumber, RelFileLocator, RelFileNumber,
        };
        use crate::storage::vfs::{FaultEffect, FaultOp, FaultPlan, FaultRule};
        let r = Rig::new();
        let rel = RelFileLocator {
            spc_oid: DEFAULTTABLESPACE_OID,
            db_oid: 5,
            rel_number: RelFileNumber(16384),
        };
        r.smgr.create(rel, ForkNumber::Main).unwrap();
        r.smgr.extend(rel, ForkNumber::Main).unwrap();
        let tag = BufferTag {
            rel,
            fork: ForkNumber::Main,
            block: 0,
        };
        r.smgr.write_block(tag, &[1u8; 8192]).unwrap();
        r.sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Sync,
                path_prefix: Some("base".into()),
                nth: None,
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        assert!(run(&r.parts(), CheckpointKind::Explicit).is_err());
        r.sim.set_faults(FaultPlan::default());
        assert!(r.smgr.is_broken());
        let e = run(&r.parts(), CheckpointKind::Shutdown).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_ne!(r.control.get().state, DbState::ShutDown);
    }
}
