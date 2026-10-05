//! Writing a frame to disk (`m2.md` §6.3 items 4, 8, 9, 12; WAL-before-data `m3.md` §6.5.3). Every write of
//! a buffer goes through [`BufferPool::flush_frame`].

use std::cell::RefCell;
use std::sync::TryLockError;
use std::sync::atomic::Ordering;

use super::BufferPool;
use super::frame::FrameId;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::BLCKSZ;
use crate::storage::checksum::page_checksum;
use crate::util::sync::{lock, wait};

thread_local! {
    /// The 8KB work area the page is copied to before the checksum is put
    /// in and the block is written (`PageSetChecksumCopy`).
    static WORK: RefCell<Box<[u8; BLCKSZ]>> = RefCell::new(Box::new([0; BLCKSZ]));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlushOutcome {
    /// The page was written.
    Written,
    /// There was nothing to write (not dirty, or someone else wrote it).
    Clean,
    /// Not done: I/O in progress elsewhere or the latch was busy (only when
    /// the caller asked not to wait).
    Skipped,
}

pub(super) fn latch_poisoned() -> Error {
    Error::new(sqlstate::INTERNAL_ERROR, "buffer content lock poisoned")
        .with_severity(Severity::Panic)
}

impl BufferPool {
    /// Writes the frame if it is dirty. The caller holds a pin.
    ///
    /// * `wait_io`: wait for another writer of this frame; otherwise return
    ///   `Skipped` (eviction must not wait: see the deadlock note in
    ///   `clock.rs`).
    /// * `try_latch`: take the shared latch with `try_read`; otherwise block.
    ///
    /// A write failure keeps the frame valid and dirty with `io_error` set.
    /// A poisoned latch or a poisoned cluster is a `Severity::Panic` error
    /// and nothing is written.
    pub(super) fn flush_frame(
        &self,
        fid: FrameId,
        wait_io: bool,
        try_latch: bool,
    ) -> Result<FlushOutcome> {
        let frame = self.frame(fid);
        // 1. Claim the I/O.
        let tag = {
            let mut h = lock(&frame.header)?;
            loop {
                if !h.io_in_progress {
                    break;
                }
                if !wait_io {
                    return Ok(FlushOutcome::Skipped);
                }
                h = wait(&frame.io_done, h)?;
            }
            if !h.valid || !h.dirty {
                return Ok(FlushOutcome::Clean);
            }
            let Some(tag) = h.tag else {
                return Ok(FlushOutcome::Clean);
            };
            if self.poison.is_set() {
                return Err(Error::new(
                    sqlstate::INTERNAL_ERROR,
                    "cluster is poisoned: refusing to write buffers",
                )
                .with_severity(Severity::Panic));
            }
            h.io_in_progress = true;
            h.just_dirtied = false;
            tag
        };

        // 2.-5. Copy under the shared latch, then write without any lock.
        let result = WORK.with(|w| -> Result<Option<u64>> {
            let mut work = w.borrow_mut();
            let lsn = {
                let latch = if try_latch {
                    match frame.content.try_read() {
                        Ok(g) => g,
                        Err(TryLockError::WouldBlock) => return Ok(None),
                        Err(TryLockError::Poisoned(_)) => return Err(self.poisoned_latch()),
                    }
                } else {
                    frame.content.read().map_err(|_| self.poisoned_latch())?
                };
                work.copy_from_slice(&latch.0);
                latch.lsn()
            };
            if !self.knobs.skip_wal_before_data {
                self.wal.flush_to(lsn)?;
            }
            if work.iter().any(|&b| b != 0) {
                let sum = page_checksum(&work, tag.block);
                work[8..10].copy_from_slice(&sum.to_le_bytes());
            }
            self.smgr.write_block(tag, &work).map(|()| Some(lsn))
        });

        // 6. Finish.
        let mut written_lsn = None;
        let mut h = lock(&frame.header)?;
        h.io_in_progress = false;
        let outcome = match result {
            Ok(Some(lsn)) => {
                written_lsn = Some(lsn);
                if !h.just_dirtied {
                    h.dirty = false;
                }
                h.io_error = false;
                self.stats.writes.fetch_add(1, Ordering::Relaxed);
                Ok(FlushOutcome::Written)
            }
            Ok(None) => Ok(FlushOutcome::Skipped),
            Err(e) => {
                if e.severity != Severity::Panic {
                    h.io_error = true;
                    self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
                }
                Err(e)
            }
        };
        drop(h);
        frame.io_done.notify_all();
        if let Some(lsn) = written_lsn {
            // WAL-before-data (`m3.md` §6.5.3): a violation must not go unnoticed.
            assert!(
                !self.knobs.assert_wal_before_data || lsn <= self.wal.flushed_ptr(),
                "WAL-before-data violated: wrote block {} of {:?} with page LSN {lsn} beyond the flushed WAL {}",
                tag.block,
                tag.rel,
                self.wal.flushed_ptr()
            );
        }
        outcome
    }

    pub(super) fn poisoned_latch(&self) -> Error {
        self.poison.set();
        latch_poisoned()
    }
}
