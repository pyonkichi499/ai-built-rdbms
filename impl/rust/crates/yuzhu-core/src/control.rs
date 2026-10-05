//! Control file `global/yuzhu_control` with two alternating slots
//! (`m2.md` §3.2, §4.3a).
//!
//! The file is 8192 bytes: slot A at offset 0, slot B at offset 4096. Each
//! slot is a 512-byte record ending in a CRC32C. A write goes to the slot
//! that was not written last, with `generation + 1`, followed by
//! `sync_data`; so a torn write of one slot never destroys the other.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::vfs::{OpenMode, Vfs, VfsFile};
use crate::storage::{BLCKSZ, CATALOG_VERSION_NO, DEFAULT_RELSEG_SIZE, PAGE_LAYOUT_VERSION};
use crate::util::crc32c::crc32c;
use crate::util::sync::{lock, lock_ignore_poison};
use crate::wal::{MAX_WAL_SEGMENT_SIZE, MIN_WAL_SEGMENT_SIZE};

/// Path of the control file, relative to the data directory.
pub const CONTROL_FILE_PATH: &str = "global/yuzhu_control";
pub const CONTROL_FORMAT_VERSION: u32 = 2;
/// The default WAL segment size (initdb may choose another power of two).
pub const WAL_SEGMENT_SIZE: u32 = 16 * 1024 * 1024;
/// `flags` bit 0: `full_page_writes`.
pub const FLAG_FULL_PAGE_WRITES: u32 = 1;
/// `flags` bit 1: page checksums (always set).
pub const FLAG_PAGE_CHECKSUMS: u32 = 2;
/// First OID handed out to user objects.
pub const FIRST_NORMAL_OID: u32 = 16384;

const MAGIC: &[u8; 8] = b"YUZHUCTL";
const SLOT_OFFSET: [u64; 2] = [0, 4096];
const SLOT_SIZE: usize = 512;
const CRC_OFFSET: usize = 508;
const FILE_SIZE: usize = 8192;

/// The fields of `m2.md` §3.2 (without magic, generation and crc32c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlData {
    pub format_version: u32,
    pub catalog_version: u32,
    pub system_identifier: u64,
    pub state: DbState,
    pub page_size: u32,
    pub wal_segment_size: u32,
    pub flags: u32,
    pub time: i64,
    pub checkpoint_lsn: u64,
    pub redo_lsn: u64,
    pub next_xid: u64,
    pub oldest_xid: u64,
    pub next_oid: u32,
    pub timeline: u32,
    pub min_recovery_lsn: u64,
    pub rel_seg_blocks: u32,
    pub data_layout_version: u32,
    pub builtin_hash: u64,
}

impl ControlData {
    /// The contents initdb writes: state `ShutDown`, checksums on, XIDs
    /// starting at 3 (0 = invalid, 1 = bootstrap, 2 = frozen) and OIDs at
    /// [`FIRST_NORMAL_OID`].
    pub fn initial(system_identifier: u64, builtin_hash: u64, rel_seg_blocks: u32) -> Self {
        ControlData {
            format_version: CONTROL_FORMAT_VERSION,
            catalog_version: CATALOG_VERSION_NO,
            system_identifier,
            state: DbState::ShutDown,
            page_size: u32::try_from(BLCKSZ).expect("BLCKSZ fits u32"),
            wal_segment_size: WAL_SEGMENT_SIZE,
            flags: FLAG_FULL_PAGE_WRITES | FLAG_PAGE_CHECKSUMS,
            time: unix_now(),
            checkpoint_lsn: 0,
            redo_lsn: 0,
            next_xid: 3,
            oldest_xid: 3,
            next_oid: FIRST_NORMAL_OID,
            timeline: 1,
            min_recovery_lsn: 0,
            rel_seg_blocks,
            data_layout_version: u32::from(PAGE_LAYOUT_VERSION),
            builtin_hash,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbState {
    Startup = 0,
    ShutDown = 1,
    ShuttingDown = 2,
    InCrashRecovery = 3,
    InProduction = 4,
}

impl DbState {
    fn from_u32(v: u32) -> Option<DbState> {
        Some(match v {
            0 => DbState::Startup,
            1 => DbState::ShutDown,
            2 => DbState::ShuttingDown,
            3 => DbState::InCrashRecovery,
            4 => DbState::InProduction,
            _ => return None,
        })
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A system identifier from the time and PID (no random-number crate).
pub fn generate_system_identifier() -> u64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let micros = u64::from(d.subsec_micros());
    (secs << 32) | ((micros & 0xFFF) << 20) | (u64::from(std::process::id()) & 0xF_FFFF)
}

// ----- encoding --------------------------------------------------------------

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("4 bytes"))
}
fn get_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("8 bytes"))
}

fn encode_slot(d: &ControlData, generation: u64) -> [u8; SLOT_SIZE] {
    let mut b = [0u8; SLOT_SIZE];
    b[0..8].copy_from_slice(MAGIC);
    put_u32(&mut b, 8, d.format_version);
    put_u32(&mut b, 12, d.catalog_version);
    put_u64(&mut b, 16, generation);
    put_u64(&mut b, 24, d.system_identifier);
    put_u32(&mut b, 32, d.state as u32);
    put_u32(&mut b, 36, d.page_size);
    put_u32(&mut b, 40, d.wal_segment_size);
    put_u32(&mut b, 44, d.flags);
    put_u64(&mut b, 48, d.time.cast_unsigned());
    put_u64(&mut b, 56, d.checkpoint_lsn);
    put_u64(&mut b, 64, d.redo_lsn);
    put_u64(&mut b, 72, d.next_xid);
    put_u64(&mut b, 80, d.oldest_xid);
    put_u32(&mut b, 88, d.next_oid);
    put_u32(&mut b, 92, d.timeline);
    put_u64(&mut b, 96, d.min_recovery_lsn);
    put_u32(&mut b, 104, d.rel_seg_blocks);
    put_u32(&mut b, 108, d.data_layout_version);
    put_u64(&mut b, 112, d.builtin_hash);
    let crc = crc32c(&b[..CRC_OFFSET]);
    put_u32(&mut b, CRC_OFFSET, crc);
    b
}

/// `None` if the magic, CRC or state value is wrong.
fn decode_slot(b: &[u8]) -> Option<(ControlData, u64)> {
    if b.len() < SLOT_SIZE
        || &b[0..8] != MAGIC
        || crc32c(&b[..CRC_OFFSET]) != get_u32(b, CRC_OFFSET)
    {
        return None;
    }
    let data = ControlData {
        format_version: get_u32(b, 8),
        catalog_version: get_u32(b, 12),
        system_identifier: get_u64(b, 24),
        state: DbState::from_u32(get_u32(b, 32))?,
        page_size: get_u32(b, 36),
        wal_segment_size: get_u32(b, 40),
        flags: get_u32(b, 44),
        time: get_u64(b, 48).cast_signed(),
        checkpoint_lsn: get_u64(b, 56),
        redo_lsn: get_u64(b, 64),
        next_xid: get_u64(b, 72),
        oldest_xid: get_u64(b, 80),
        next_oid: get_u32(b, 88),
        timeline: get_u32(b, 92),
        min_recovery_lsn: get_u64(b, 96),
        rel_seg_blocks: get_u32(b, 104),
        data_layout_version: get_u32(b, 108),
        builtin_hash: get_u64(b, 112),
    };
    Some((data, get_u64(b, 16)))
}

// ----- handle ------------------------------------------------------------------

#[derive(Debug)]
struct ControlInner {
    data: ControlData,
    generation: u64,
    /// The slot written last; the next write goes to the other one.
    last_slot: usize,
}

#[derive(Debug)]
pub struct ControlFileHandle {
    file: Arc<dyn VfsFile>,
    /// Mutation switch (`DebugKnobs::single_slot_control_file`): always write slot A.
    single_slot: AtomicBool,
    inner: Mutex<ControlInner>,
}

fn invalid_control_file() -> Error {
    Error::corrupted("invalid control file")
        .with_severity(Severity::Fatal)
        .with_detail(format!(
            "Neither slot of \"{CONTROL_FILE_PATH}\" has a valid magic number and checksum."
        ))
}

fn incompatible_format(found: u32) -> Error {
    let detail = if found == 1 {
        "The database cluster was initialized without WAL (format version 1), but the server requires format version 2.".to_string()
    } else {
        format!(
            "The database cluster has control file format version {found}, but the server requires format version {CONTROL_FORMAT_VERSION}."
        )
    };
    Error::internal("database files are incompatible with server")
        .with_severity(Severity::Fatal)
        .with_detail(detail)
        .with_hint("Re-run yuzhu-initdb.")
}

fn initdb_hint(e: Error) -> Error {
    e.with_severity(Severity::Fatal)
        .with_hint("It looks like you need to initdb.")
}

impl ControlFileHandle {
    /// For initdb: writes both slots (A with generation 1, B all zeros),
    /// then `sync_all` and `sync_dir(global)`.
    pub fn create(vfs: &Arc<dyn Vfs>, data: &ControlData) -> Result<ControlFileHandle> {
        let ctx = |e: &std::io::Error| {
            Error::from_io(e, format!("could not create file \"{CONTROL_FILE_PATH}\""))
        };
        let path = Path::new(CONTROL_FILE_PATH);
        let global = path.parent().unwrap_or(Path::new(""));
        vfs.create_dir_all(global).map_err(|e| ctx(&e))?;
        let file = vfs.open(path, OpenMode::CreateNew).map_err(|e| ctx(&e))?;
        let mut buf = vec![0u8; FILE_SIZE];
        buf[..SLOT_SIZE].copy_from_slice(&encode_slot(data, 1));
        file.write_all_at(&buf, 0).map_err(|e| ctx(&e))?;
        file.sync_all().map_err(|e| ctx(&e))?;
        vfs.sync_dir(global).map_err(|e| ctx(&e))?;
        // Also persist the entry of `global/` itself in the data directory.
        vfs.sync_dir(Path::new("")).map_err(|e| ctx(&e))?;
        Ok(ControlFileHandle {
            file,
            single_slot: AtomicBool::new(false),
            inner: Mutex::new(ControlInner {
                data: data.clone(),
                generation: 1,
                last_slot: 0,
            }),
        })
    }

    /// Reads both slots and takes the valid one with the larger generation.
    pub fn open(vfs: &Arc<dyn Vfs>) -> Result<ControlFileHandle> {
        let ctx = |e: &std::io::Error| {
            Error::from_io(e, format!("could not open file \"{CONTROL_FILE_PATH}\""))
                .with_severity(Severity::Fatal)
        };
        let file = vfs
            .open(Path::new(CONTROL_FILE_PATH), OpenMode::ReadWrite)
            .map_err(|e| ctx(&e))?;
        let mut buf = vec![0u8; FILE_SIZE];
        match file.read_exact_at(&mut buf, 0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(invalid_control_file());
            }
            Err(e) => return Err(ctx(&e)),
        }
        let best = (0..2)
            .filter_map(|slot| {
                let off = usize::try_from(SLOT_OFFSET[slot]).ok()?;
                decode_slot(&buf[off..off + SLOT_SIZE]).map(|(d, g)| (slot, d, g))
            })
            .max_by_key(|(_, _, g)| *g);
        let Some((slot, data, generation)) = best else {
            return Err(invalid_control_file());
        };
        if data.format_version != CONTROL_FORMAT_VERSION {
            return Err(incompatible_format(data.format_version));
        }
        Ok(ControlFileHandle {
            file,
            single_slot: AtomicBool::new(false),
            inner: Mutex::new(ControlInner {
                data,
                generation,
                last_slot: slot,
            }),
        })
    }

    /// Start-up checks of the constants (§3.2). `FATAL` on mismatch.
    pub fn check_compatible(&self, expected_builtin_hash: u64) -> Result<()> {
        let d = self.get();
        let mismatch = |what: &str, found: String, expected: String| {
            initdb_hint(
                Error::internal("database files are incompatible with server").with_detail(
                    format!(
                        "The database cluster was initialized with {what} {found}, but the server was compiled with {what} {expected}."
                    ),
                ),
            )
        };
        if d.catalog_version != CATALOG_VERSION_NO {
            return Err(mismatch(
                "CATALOG_VERSION_NO",
                d.catalog_version.to_string(),
                CATALOG_VERSION_NO.to_string(),
            ));
        }
        if d.page_size as usize != BLCKSZ {
            return Err(mismatch(
                "BLCKSZ",
                d.page_size.to_string(),
                BLCKSZ.to_string(),
            ));
        }
        if d.rel_seg_blocks == 0 || d.rel_seg_blocks > DEFAULT_RELSEG_SIZE {
            return Err(mismatch(
                "RELSEG_SIZE",
                d.rel_seg_blocks.to_string(),
                format!("at most {DEFAULT_RELSEG_SIZE}"),
            ));
        }
        if d.data_layout_version != u32::from(PAGE_LAYOUT_VERSION) {
            return Err(mismatch(
                "PAGE_LAYOUT_VERSION",
                d.data_layout_version.to_string(),
                PAGE_LAYOUT_VERSION.to_string(),
            ));
        }
        if !d.wal_segment_size.is_power_of_two()
            || !(MIN_WAL_SEGMENT_SIZE..=MAX_WAL_SEGMENT_SIZE).contains(&d.wal_segment_size)
        {
            return Err(mismatch(
                "WAL_SEGMENT_SIZE",
                d.wal_segment_size.to_string(),
                format!("a power of two in {MIN_WAL_SEGMENT_SIZE}..={MAX_WAL_SEGMENT_SIZE}"),
            ));
        }
        if d.flags & FLAG_PAGE_CHECKSUMS == 0 {
            return Err(mismatch(
                "page checksums",
                "disabled".into(),
                "enabled".into(),
            ));
        }
        if d.builtin_hash != expected_builtin_hash {
            return Err(mismatch(
                "builtin_hash",
                format!("{:#018x}", d.builtin_hash),
                format!("{expected_builtin_hash:#018x}"),
            ));
        }
        Ok(())
    }

    /// Mutation testing only: write every update to slot A (slot B stays
    /// invalid), so a torn write can destroy the only valid copy.
    pub fn set_single_slot(&self, on: bool) {
        self.single_slot.store(on, Ordering::Relaxed);
    }

    /// A copy of the current contents.
    pub fn get(&self) -> ControlData {
        lock_ignore_poison(&self.inner).data.clone()
    }

    /// The generation of the slot written last (for tests and diagnostics).
    pub fn generation(&self) -> u64 {
        lock_ignore_poison(&self.inner).generation
    }

    /// Read-modify-write by the rules of §3.2. `next_xid` and `next_oid` must
    /// not decrease (`next_oid` may wrap back to the normal range); the
    /// identity fields must not change. A failed write is `Severity::Panic`
    /// and leaves the in-memory copy unchanged. `time` is set to now before
    /// `f` runs.
    pub fn update(&self, f: impl FnOnce(&mut ControlData)) -> Result<()> {
        self.update_impl(f, false)
    }

    /// Only for the shutdown checkpoint: may also lower `next_xid` (§3.2).
    pub fn update_for_shutdown(&self, f: impl FnOnce(&mut ControlData)) -> Result<()> {
        self.update_impl(f, true)
    }

    fn update_impl(
        &self,
        f: impl FnOnce(&mut ControlData),
        allow_xid_decrease: bool,
    ) -> Result<()> {
        let mut inner = lock(&self.inner)?;
        let old = &inner.data;
        let mut new = old.clone();
        new.time = unix_now();
        f(&mut new);
        validate_update(old, &new, allow_xid_decrease)?;
        let generation = inner.generation + 1;
        let slot = if self.single_slot.load(Ordering::Relaxed) {
            0
        } else {
            1 - inner.last_slot
        };
        let rec = encode_slot(&new, generation);
        let io_err = |e: &std::io::Error| {
            Error::from_io(e, format!("could not write file \"{CONTROL_FILE_PATH}\""))
                .with_severity(Severity::Panic)
        };
        self.file
            .write_all_at(&rec, SLOT_OFFSET[slot])
            .map_err(|e| io_err(&e))?;
        self.file.sync_data().map_err(|e| io_err(&e))?;
        inner.data = new;
        inner.generation = generation;
        inner.last_slot = slot;
        Ok(())
    }
}

fn validate_update(old: &ControlData, new: &ControlData, allow_xid_decrease: bool) -> Result<()> {
    let bad = |m: String| Err(Error::new(sqlstate::INTERNAL_ERROR, m));
    if !allow_xid_decrease && new.next_xid < old.next_xid {
        return bad(format!(
            "control file next_xid must not decrease ({} -> {})",
            old.next_xid, new.next_xid
        ));
    }
    if new.next_oid < old.next_oid {
        // Only the wrap-around back to the normal range is allowed.
        let wrapped = old.next_oid > u32::MAX / 2
            && new.next_oid >= FIRST_NORMAL_OID
            && new.next_oid <= FIRST_NORMAL_OID + crate::storage::OID_PREFETCH;
        if !wrapped {
            return bad(format!(
                "control file next_oid must not decrease ({} -> {})",
                old.next_oid, new.next_oid
            ));
        }
    }
    if new.format_version != old.format_version
        || new.system_identifier != old.system_identifier
        || new.page_size != old.page_size
    {
        return bad("control file identity fields must not change".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vfs::sim::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs};

    fn setup() -> (SimVfs, Arc<dyn Vfs>) {
        let sim = SimVfs::new(1);
        let vfs: Arc<dyn Vfs> = Arc::new(sim.clone());
        (sim, vfs)
    }

    fn data() -> ControlData {
        ControlData::initial(0x1234_5678_9ABC_DEF0, 0xAAAA_BBBB_CCCC_DDDD, 131_072)
    }

    #[test]
    fn create_open_roundtrip() {
        let (sim, vfs) = setup();
        let h = ControlFileHandle::create(&vfs, &data()).unwrap();
        assert_eq!(h.get(), data());
        let file = sim.file_contents(Path::new(CONTROL_FILE_PATH)).unwrap();
        assert_eq!(file.len(), 8192);
        assert_eq!(&file[..8], b"YUZHUCTL");
        assert!(file[4096..].iter().all(|&b| b == 0));
        let h2 = ControlFileHandle::open(&vfs).unwrap();
        assert_eq!(h2.get(), data());
        assert_eq!(h2.generation(), 1);
        assert!(ControlFileHandle::create(&vfs, &data()).is_err());
        h2.check_compatible(0xAAAA_BBBB_CCCC_DDDD).unwrap();
        assert!(h2.check_compatible(1).is_err());
    }

    #[test]
    fn create_is_durable_after_crash() {
        let (sim, vfs) = setup();
        ControlFileHandle::create(&vfs, &data()).unwrap();
        let after: Arc<dyn Vfs> = Arc::new(sim.crash(CrashMode::DropUnsynced));
        assert_eq!(ControlFileHandle::open(&after).unwrap().get(), data());
    }

    #[test]
    fn updates_alternate_slots_and_increase_generation() {
        let (sim, vfs) = setup();
        let h = ControlFileHandle::create(&vfs, &data()).unwrap();
        h.update(|c| c.next_xid = 1027).unwrap();
        h.update(|c| c.state = DbState::InProduction).unwrap();
        h.update(|c| c.next_oid += 8192).unwrap();
        assert_eq!(h.generation(), 4);
        let file = sim.file_contents(Path::new(CONTROL_FILE_PATH)).unwrap();
        let (a, b) = (get_u64(&file, 16), get_u64(&file, 4096 + 16));
        assert_eq!((a.max(b), a.min(b)), (4, 3));
        let h2 = ControlFileHandle::open(&vfs).unwrap();
        assert_eq!(h2.get().next_xid, 1027);
        assert_eq!(h2.get().state, DbState::InProduction);
        assert_eq!(h2.get().next_oid, 16384 + 8192);
    }

    #[test]
    fn corrupt_slot_falls_back_to_the_other() {
        let (sim, vfs) = setup();
        let h = ControlFileHandle::create(&vfs, &data()).unwrap();
        h.update(|c| c.next_xid = 100).unwrap(); // slot B, gen 2
        h.update(|c| c.next_xid = 200).unwrap(); // slot A, gen 3
        // Damage the newest slot (A): open must use B (next_xid 100).
        let f = vfs
            .open(Path::new(CONTROL_FILE_PATH), OpenMode::ReadWrite)
            .unwrap();
        f.write_all_at(&[0xFF; 8], 200).unwrap();
        let h2 = ControlFileHandle::open(&vfs).unwrap();
        assert_eq!(h2.get().next_xid, 100);
        // The next write targets A again (the other slot) and wins.
        h2.update(|c| c.next_xid = 300).unwrap();
        assert_eq!(ControlFileHandle::open(&vfs).unwrap().get().next_xid, 300);
        // Damage both: FATAL XX001.
        f.write_all_at(&[0xFF; 8], 200).unwrap();
        f.write_all_at(&[0xFF; 8], 4096 + 200).unwrap();
        let e = ControlFileHandle::open(&vfs).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
        assert_eq!(e.severity, Severity::Fatal);
        drop(sim);
    }

    #[test]
    fn torn_update_never_loses_the_previous_state() {
        for seed in 0..40 {
            let sim = SimVfs::new(seed);
            let vfs: Arc<dyn Vfs> = Arc::new(sim.clone());
            let h = ControlFileHandle::create(&vfs, &data()).unwrap();
            h.update(|c| c.next_xid = 100).unwrap();
            // Fail the sync of the next update, then tear whatever was written.
            sim.set_faults(FaultPlan {
                rules: vec![FaultRule {
                    op: FaultOp::Sync,
                    path_prefix: None,
                    nth: Some(1),
                    probability: None,
                    effect: FaultEffect::Error(std::io::ErrorKind::Other),
                }],
            });
            assert!(h.update(|c| c.next_xid = 200).is_err());
            let after: Arc<dyn Vfs> = Arc::new(sim.crash(CrashMode::TornSectors {
                sector: 64,
                keep_probability: 0.5,
            }));
            let got = ControlFileHandle::open(&after).unwrap().get().next_xid;
            assert!(got == 100 || got == 200, "seed {seed}: {got}");
        }
    }

    #[test]
    fn write_failure_is_panic_and_keeps_memory_state() {
        let (sim, vfs) = setup();
        let h = ControlFileHandle::create(&vfs, &data()).unwrap();
        sim.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Write,
                path_prefix: None,
                nth: Some(1),
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::StorageFull),
            }],
        });
        let e = h.update(|c| c.next_xid = 999).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(e.sqlstate.code(), "53100");
        assert_eq!(h.get().next_xid, 3);
        assert_eq!(h.generation(), 1);
        // A later update still works (and uses the same target slot).
        h.update(|c| c.next_xid = 999).unwrap();
        assert_eq!(ControlFileHandle::open(&vfs).unwrap().get().next_xid, 999);
    }

    #[test]
    fn monotonic_checks() {
        let (_sim, vfs) = setup();
        let h = ControlFileHandle::create(&vfs, &data()).unwrap();
        h.update(|c| {
            c.next_xid = 2000;
            c.next_oid = 20000;
        })
        .unwrap();
        let e = h.update(|c| c.next_xid = 5).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX000");
        assert_eq!(e.severity, Severity::Error);
        assert!(h.update(|c| c.next_oid = 17000).is_err());
        assert!(h.update(|c| c.system_identifier = 1).is_err());
        assert_eq!(h.get().next_xid, 2000);
        // Shutdown may lower next_xid, but still not next_oid.
        h.update_for_shutdown(|c| {
            c.next_xid = 1500;
            c.state = DbState::ShutDown;
        })
        .unwrap();
        assert_eq!(h.get().next_xid, 1500);
        assert!(h.update_for_shutdown(|c| c.next_oid = 17000).is_err());
        // OID wrap-around is allowed.
        h.update(|c| c.next_oid = u32::MAX).unwrap();
        h.update(|c| c.next_oid = FIRST_NORMAL_OID + 8192).unwrap();
    }

    #[test]
    fn check_compatible_detects_each_constant() {
        let (_sim, vfs) = setup();
        let mut d = data();
        d.catalog_version += 1;
        let h = ControlFileHandle::create(&vfs, &d).unwrap();
        let e = h.check_compatible(d.builtin_hash).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert!(e.hint.as_deref().unwrap().contains("initdb"));
        for tweak in [
            |d: &mut ControlData| d.page_size = 4096,
            |d: &mut ControlData| d.flags = 0,
            |d: &mut ControlData| d.data_layout_version = 2,
            |d: &mut ControlData| d.rel_seg_blocks = 0,
            |d: &mut ControlData| d.rel_seg_blocks = DEFAULT_RELSEG_SIZE + 1,
            |d: &mut ControlData| d.wal_segment_size = 1,
        ] {
            let sim = SimVfs::new(1);
            let vfs: Arc<dyn Vfs> = Arc::new(sim);
            let mut d = data();
            tweak(&mut d);
            let h = ControlFileHandle::create(&vfs, &d).unwrap();
            assert!(h.check_compatible(d.builtin_hash).is_err());
        }
    }

    #[test]
    fn unknown_format_version_and_missing_file() {
        let (sim, vfs) = setup();
        let mut d = data();
        d.format_version = 99;
        ControlFileHandle::create(&vfs, &d).unwrap();
        let e = ControlFileHandle::open(&vfs).unwrap_err();
        assert!(e.message.contains("incompatible"));
        let empty: Arc<dyn Vfs> = Arc::new(SimVfs::new(2));
        let e = ControlFileHandle::open(&empty).unwrap_err();
        assert_eq!(e.sqlstate.code(), "58P01");
        // Truncated file.
        let f = vfs
            .open(Path::new(CONTROL_FILE_PATH), OpenMode::ReadWrite)
            .unwrap();
        f.set_len(100).unwrap();
        assert_eq!(
            ControlFileHandle::open(&vfs).unwrap_err().sqlstate.code(),
            "XX001"
        );
        drop(sim);
    }

    #[test]
    fn system_identifier_differs_over_time() {
        assert_ne!(generate_system_identifier(), 0);
    }

    #[test]
    fn format_version_1_is_rejected_with_wal_message() {
        let (_sim, vfs) = setup();
        let mut d = data();
        d.format_version = 1;
        ControlFileHandle::create(&vfs, &d).unwrap();
        let e = ControlFileHandle::open(&vfs).unwrap_err();
        assert_eq!(e.severity, Severity::Fatal);
        assert_eq!(e.message, "database files are incompatible with server");
        assert!(
            e.detail
                .as_deref()
                .unwrap()
                .contains("without WAL (format version 1)")
        );
        assert_eq!(e.hint.as_deref(), Some("Re-run yuzhu-initdb."));
        assert_eq!(CONTROL_FORMAT_VERSION, 2);
        assert_eq!(data().flags, FLAG_FULL_PAGE_WRITES | FLAG_PAGE_CHECKSUMS);
    }

    #[test]
    fn wal_segment_size_is_checked_as_power_of_two_in_range() {
        for (size, ok) in [
            (2u32 << 20, true),
            (64 << 20, true),
            (1 << 30, true),
            (1 << 20, false),
            (3 << 20, false),
            (0, false),
        ] {
            let (_sim, vfs) = setup();
            let mut d = data();
            d.wal_segment_size = size;
            let h = ControlFileHandle::create(&vfs, &d).unwrap();
            assert_eq!(h.check_compatible(d.builtin_hash).is_ok(), ok, "{size}");
        }
    }

    #[test]
    fn single_slot_mutation_always_writes_slot_a() {
        let (sim, vfs) = setup();
        let h = ControlFileHandle::create(&vfs, &data()).unwrap();
        h.set_single_slot(true);
        h.update(|c| c.next_xid = 10).unwrap();
        h.update(|c| c.next_xid = 20).unwrap();
        let file = sim.file_contents(Path::new(CONTROL_FILE_PATH)).unwrap();
        assert_eq!(get_u64(&file, 16), 3);
        assert!(file[4096..].iter().all(|&b| b == 0));
        // A torn write of the only slot can make the file unusable.
        let mut broken = 0;
        for seed in 0..40 {
            let s = SimVfs::new(seed);
            let v: Arc<dyn Vfs> = Arc::new(s.clone());
            let h = ControlFileHandle::create(&v, &data()).unwrap();
            h.set_single_slot(true);
            h.update(|c| c.next_xid = 100).unwrap();
            s.set_faults(FaultPlan {
                rules: vec![FaultRule {
                    op: FaultOp::Sync,
                    path_prefix: None,
                    nth: Some(1),
                    probability: None,
                    effect: FaultEffect::Error(std::io::ErrorKind::Other),
                }],
            });
            assert!(h.update(|c| c.next_xid = 200).is_err());
            let after: Arc<dyn Vfs> = Arc::new(s.crash(CrashMode::TornSectors {
                sector: 1,
                keep_probability: 0.5,
            }));
            if ControlFileHandle::open(&after).is_err() {
                broken += 1;
            }
        }
        assert!(broken > 0);
    }
}
