//! Clock-sweep victim selection (`m2.md` §6.3 items 2, 11).
//!
//! `get_victim` returns a frame that is pinned once (by the caller), has no
//! tag, is clean and is not on the free list. Evicting a victim follows the
//! invariants of item 11: write while pinned, re-check under the partition
//! lock and the header mutex, remove the old tag, and only then let the
//! caller register the new tag.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::flush::FlushOutcome;
use super::frame::FrameId;
use super::{BufferPool, MAX_USAGE_COUNT};
use crate::error::{Error, Result, Severity};
use crate::util::sync::lock;

#[derive(Debug, Default)]
pub(super) struct ClockHand(AtomicUsize);

impl ClockHand {
    pub(super) fn next(&self, nframes: usize) -> usize {
        self.0.fetch_add(1, Ordering::Relaxed) % nframes
    }
}

impl BufferPool {
    pub(super) fn get_victim(&self) -> Result<FrameId> {
        loop {
            if let Some(fid) = lock(&self.free_list)?.pop() {
                let mut h = self.header(fid)?;
                debug_assert!(h.tag.is_none() && h.pin_count == 0);
                h.pin_count = 1;
                h.usage_count = 0;
                return Ok(fid);
            }
            if let Some(fid) = self.sweep()? {
                return Ok(fid);
            }
        }
    }

    /// One full clock-sweep. `Ok(None)` means "a candidate was lost to a
    /// concurrent change; try the free list again".
    fn sweep(&self) -> Result<Option<FrameId>> {
        let n = self.frames.len();
        let mut trycounter = n;
        loop {
            let fid = FrameId(u32::try_from(self.clock.next(n)).expect("nframes fits u32"));
            {
                let mut h = self.header(fid)?;
                if h.pin_count == 0 && h.tag.is_some() {
                    if h.usage_count > 0 {
                        h.usage_count -= 1;
                        trycounter = n;
                        continue;
                    }
                    h.pin_count = 1;
                } else if h.pin_count == 0 {
                    // Unmapped and unpinned: it is on (or about to be put on)
                    // the free list. Leave it to the free-list path.
                    return Ok(None);
                } else {
                    trycounter -= 1;
                    if trycounter == 0 {
                        return Err(Error::internal("no unpinned buffers available"));
                    }
                    continue;
                }
            }
            // We hold the only pin of a mapped, unused frame.
            if self.try_evict(fid)? {
                return Ok(Some(fid));
            }
            // Could not use this one (busy, unwritable, re-dirtied): count it
            // like a pinned frame so that the sweep terminates.
            trycounter -= 1;
            if trycounter == 0 {
                return Err(Error::internal("no unpinned buffers available"));
            }
        }
    }

    /// Makes the pinned, mapped frame `fid` unmapped and clean. On `Ok(false)`
    /// the pin was released and the frame left as it was.
    fn try_evict(&self, fid: FrameId) -> Result<bool> {
        let (tag, dirty) = {
            let h = self.header(fid)?;
            (h.tag.expect("mapped"), h.dirty)
        };
        if dirty {
            match self.flush_frame(fid, false, true) {
                Ok(FlushOutcome::Written | FlushOutcome::Clean) => {}
                Err(e) if e.severity == Severity::Panic => {
                    self.unpin(fid);
                    return Err(e);
                }
                // Busy, or a failed write that leaves the frame valid and
                // dirty (`BM_IO_ERROR`): look for another victim.
                Ok(FlushOutcome::Skipped) | Err(_) => {
                    self.unpin(fid);
                    return Ok(false);
                }
            }
        }
        let part = self.table.part(&tag);
        let mut map = lock(part)?;
        let mut h = self.header(fid)?;
        if h.pin_count != 1 || h.dirty || h.io_in_progress || h.tag != Some(tag) {
            drop(h);
            drop(map);
            self.unpin(fid);
            return Ok(false);
        }
        map.remove(&tag);
        h.clear_mapping();
        self.stats.evictions.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    /// Bumps the usage count when a page is pinned (capped).
    pub(super) fn bump_usage(usage: &mut u8) {
        if *usage < MAX_USAGE_COUNT {
            *usage += 1;
        }
    }
}
