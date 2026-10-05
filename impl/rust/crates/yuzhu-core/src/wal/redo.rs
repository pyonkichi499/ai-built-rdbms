//! REDO の骨格: `RedoCtx`、`read_buffer_for_redo`、invalid page の追跡、
//! REDO ループ（`m3.md` §4.4、§6.4）。
//!
//! `InvalidPages` は A が置いた実装。

#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use super::{DecodedRecord, Lsn, Wal, WalReader};
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::buffer::{BufferPool, PinnedBuffer, assert_no_pins};
use crate::storage::smgr::{BlockNumber, ForkNumber, RelFileLocator, StorageManager};
use crate::txn::Xid;

/// REDO 関数に渡す文脈。`recovery.rs` が作る。
///
/// `wal::redo` は `txn` より下の層なので `Clog` の型を名指しできない。上位の層の部品は
/// `ext` に型を消して入れる（`recovery.rs` が `Arc<Clog>` を入れ、`txn::xact_wal::redo` が
/// `downcast_ref::<Arc<Clog>>()` で取り出す。ほかの用途には使わない）。
pub struct RedoCtx {
    pub pool: Arc<BufferPool>,
    pub smgr: Arc<StorageManager>,
    pub ext: Box<dyn std::any::Any + Send + Sync>,
    pub invalid: Mutex<InvalidPages>,
    /// XLOG のチェックポイントレコードで進める。
    pub next_oid: AtomicU32,
    pub knobs: DebugKnobs,
}

impl std::fmt::Debug for RedoCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedoCtx")
            .field("invalid", &self.invalid)
            .field("next_oid", &self.next_oid)
            .field("knobs", &self.knobs)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum RedoBuffer {
    /// 画像で上書きし、LSN を設定済み。何もしなくてよい。
    Restored,
    /// `page_lsn >= record.end`。適用済み。
    Done,
    /// 適用が必要。`WILL_INIT` なら中身は 0（呼び出し側が初期化する）。
    NeedsRedo(PinnedBuffer),
    /// ファイルかブロックがない（invalid page として記録済み）。飛ばす。
    NotFound,
}

/// REDO ループ中の統計（`RedoCtx` に置き場がないのでスレッドローカルに持つ。REDO は単一スレッド）。
#[derive(Clone, Copy, Default)]
struct BlockCounters {
    restored: u64,
    done: u64,
}

thread_local! {
    static COUNTERS: std::cell::Cell<BlockCounters> = const { std::cell::Cell::new(BlockCounters { restored: 0, done: 0 }) };
}

fn bump(f: impl FnOnce(&mut BlockCounters)) {
    COUNTERS.with(|c| {
        let mut v = c.get();
        f(&mut v);
        c.set(v);
    });
}

/// PostgreSQL の `XLogReadBufferForRedoExtended` に相当する（§6.4.3）。
///
/// `WILL_INIT` のときの `NeedsRedo` は、呼び出し側がページを初期化し直す前提（ページの現在の中身は
/// 使ってはならない）。
pub fn read_buffer_for_redo(
    ctx: &RedoCtx,
    rec: &DecodedRecord,
    block_id: u8,
) -> Result<RedoBuffer> {
    let blk = rec.blocks.get(usize::from(block_id)).ok_or_else(|| {
        Error::internal(format!(
            "WAL record at {} has no block reference #{block_id}",
            rec.start
        ))
        .with_severity(Severity::Panic)
    })?;
    let tag = blk.tag;

    if let Some(image) = &blk.image {
        let buf = ctx.pool.read_buffer_zeroed(tag)?;
        {
            let mut g = buf.write()?;
            g.page_mut().0.copy_from_slice(&image.0);
            g.set_lsn(rec.end.0);
        }
        bump(|c| c.restored += 1);
        return Ok(RedoBuffer::Restored);
    }
    if blk.will_init {
        return Ok(RedoBuffer::NeedsRedo(ctx.pool.read_buffer_zeroed(tag)?));
    }
    if !ctx.smgr.exists(tag.rel, tag.fork)? || tag.block >= ctx.smgr.nblocks(tag.rel, tag.fork)? {
        lock_invalid(ctx).record(tag.rel, tag.fork, tag.block);
        return Ok(RedoBuffer::NotFound);
    }
    let buf = ctx.pool.read_buffer(tag).map_err(|e| {
        // FPW があれば起きないはずの破損: リカバリは続けられない。
        e.with_severity(Severity::Panic)
    })?;
    if !ctx.knobs.redo_ignore_page_lsn {
        let done = buf.read()?.lsn() >= rec.end.0;
        if done {
            bump(|c| c.done += 1);
            return Ok(RedoBuffer::Done);
        }
    }
    Ok(RedoBuffer::NeedsRedo(buf))
}

fn lock_invalid(ctx: &RedoCtx) -> std::sync::MutexGuard<'_, InvalidPages> {
    ctx.invalid
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// REDO 中に見つかった「ファイルやブロックがない」参照の記録（D14）。
#[derive(Debug, Default)]
pub struct InvalidPages {
    pages: HashMap<(RelFileLocator, ForkNumber), BTreeSet<BlockNumber>>,
}

impl InvalidPages {
    pub fn record(&mut self, rel: RelFileLocator, fork: ForkNumber, blk: BlockNumber) {
        self.pages.entry((rel, fork)).or_default().insert(blk);
    }

    /// リレーションごと消えた（unlink の REDO）。
    pub fn forget_relation(&mut self, rel: RelFileLocator) {
        self.pages.retain(|(r, _), _| *r != rel);
    }

    /// `nblocks` 以降が消えた（truncate の REDO）。
    pub fn forget_from(&mut self, rel: RelFileLocator, fork: ForkNumber, nblocks: BlockNumber) {
        if let Some(set) = self.pages.get_mut(&(rel, fork)) {
            set.retain(|b| *b < nblocks);
            if set.is_empty() {
                self.pages.remove(&(rel, fork));
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// 空でなければ `Severity::Panic`（XX001 "WAL contains references to invalid pages"、
    /// DETAIL に一覧）。
    pub fn check_empty(&self) -> Result<()> {
        if self.pages.is_empty() {
            return Ok(());
        }
        let mut lines: Vec<String> = self
            .pages
            .iter()
            .flat_map(|((rel, fork), blks)| {
                blks.iter().map(move |b| {
                    format!(
                        "rel {}/{}/{} fork {} blk {}",
                        rel.spc_oid, rel.db_oid, rel.rel_number.0, *fork as u8, b
                    )
                })
            })
            .collect();
        lines.sort();
        Err(Error::new(
            sqlstate::DATA_CORRUPTED,
            "WAL contains references to invalid pages",
        )
        .with_detail(lines.join("\n"))
        .with_severity(Severity::Panic))
    }
}

/// `assert_no_pins` を呼ぶ間隔（レコード数）。
const PIN_CHECK_INTERVAL: u64 = 1000;

/// REDO ループ（§5.5 の手順 e-3）。`dispatch` は `recovery.rs` の振り分け表。
///
/// 各レコードで `on_record` → `wal.note_replayed(end)` → `dispatch` の順に呼ぶ。`dispatch` のエラーは
/// そのまま返す。`reader` が `None` を返したところで終わる（終わりの位置は `reader.end_of_wal()`）。
/// `RedoStats::{start, last_start, end}` は読んだレコードがなければ 0。
pub fn run_redo(
    ctx: &RedoCtx,
    wal: &Wal,
    reader: &mut WalReader,
    dispatch: &dyn Fn(&RedoCtx, &DecodedRecord) -> Result<()>,
    on_record: &mut dyn FnMut(&DecodedRecord),
) -> Result<RedoStats> {
    COUNTERS.with(|c| c.set(BlockCounters::default()));
    let mut stats = RedoStats::default();
    while let Some(rec) = reader.next()? {
        if stats.records == 0 {
            stats.start = rec.start;
        }
        on_record(&rec);
        // dispatch 中のページ LSN は rec.end になる。evict 時の flush が通るよう先に進める。
        wal.note_replayed(rec.end);
        dispatch(ctx, &rec)?;
        stats.records += 1;
        stats.max_xid = stats.max_xid.max(rec.xid);
        stats.last_start = rec.start;
        stats.end = rec.end;
        if stats.records % PIN_CHECK_INTERVAL == 0 {
            assert_no_pins();
        }
    }
    assert_no_pins();
    let c = COUNTERS.with(std::cell::Cell::get);
    stats.fpi_restored = c.restored;
    stats.done_skipped = c.done;
    Ok(stats)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RedoStats {
    pub records: u64,
    pub fpi_restored: u64,
    pub done_skipped: u64,
    pub max_xid: Xid,
    pub start: Lsn,
    pub last_start: Lsn,
    pub end: Lsn,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::smgr::RelFileNumber;

    fn rel(n: u32) -> RelFileLocator {
        RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(n),
        }
    }

    #[test]
    fn empty_invalid_pages_pass() {
        assert!(InvalidPages::default().check_empty().is_ok());
    }

    #[test]
    fn remaining_invalid_pages_are_a_panic_with_detail() {
        let mut ip = InvalidPages::default();
        ip.record(rel(16384), ForkNumber::Main, 7);
        let e = ip.check_empty().unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        assert!(e.detail.as_deref().unwrap().contains("blk 7"));
    }

    #[test]
    fn forget_relation_and_forget_from() {
        let mut ip = InvalidPages::default();
        ip.record(rel(1), ForkNumber::Main, 3);
        ip.record(rel(1), ForkNumber::Main, 9);
        ip.record(rel(2), ForkNumber::Main, 0);
        ip.forget_from(rel(1), ForkNumber::Main, 5);
        assert!(ip.check_empty().is_err());
        ip.forget_from(rel(1), ForkNumber::Main, 3);
        ip.forget_relation(rel(2));
        assert!(ip.is_empty());
        ip.check_empty().unwrap();
    }
}

#[cfg(test)]
mod redo_tests {
    use std::path::Path;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::page::Page;
    use crate::storage::smgr::{BufferTag, RelFileNumber};
    use crate::storage::vfs::{SimVfs, Vfs as _};
    use crate::wal::xlog::{self, CheckpointRecord, CheckpointRecordKind, XLOG_FPI, XLOG_NOOP};
    use crate::wal::{
        DecodedBlock, MIN_WAL_SEGMENT_SIZE, RecordBuilder, RegFlags, RmgrId, WalConfig,
    };

    fn rel(n: u32) -> RelFileLocator {
        RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(n),
        }
    }

    fn cfg() -> WalConfig {
        WalConfig {
            segment_size: MIN_WAL_SEGMENT_SIZE,
            system_identifier: 3,
            full_page_writes: true,
            knobs: DebugKnobs::default(),
        }
    }

    struct Env {
        vfs: Arc<SimVfs>,
        ctx: RedoCtx,
        wal: Arc<Wal>,
    }

    fn env(knobs: DebugKnobs) -> Env {
        let vfs = Arc::new(SimVfs::new(9));
        vfs.create_dir_all(Path::new("pg_wal")).unwrap();
        vfs.create_dir_all(Path::new("base/5")).unwrap();
        let wal = Wal::open_for_recovery(vfs.clone(), cfg());
        let smgr = Arc::new(StorageManager::new(vfs.clone(), 131_072));
        smgr.set_recovery_mode(true);
        let pool = BufferPool::new(16, smgr.clone(), wal.clone(), DebugKnobs::default());
        let ctx = RedoCtx {
            pool,
            smgr,
            ext: Box::new(()),
            invalid: Mutex::new(InvalidPages::default()),
            next_oid: AtomicU32::new(100),
            knobs,
        };
        Env { vfs, ctx, wal }
    }

    /// C の recovery モード（`extend_to` が無いファイルを作る）が入るまでの代わり。
    fn make(e: &Env, n: u32) {
        e.ctx.smgr.create(rel(n), ForkNumber::Main).unwrap();
    }

    fn tag(n: u32, block: u32) -> BufferTag {
        BufferTag {
            rel: rel(n),
            fork: ForkNumber::Main,
            block,
        }
    }

    fn record(info: u8, blocks: Vec<DecodedBlock>) -> DecodedRecord {
        DecodedRecord {
            start: Lsn(0x20_0100),
            end: Lsn(0x20_0200),
            xid: Xid(7),
            rmgr: RmgrId::Xlog,
            info,
            blocks,
            main: vec![],
        }
    }

    fn block(t: BufferTag, image: Option<u8>, will_init: bool) -> DecodedBlock {
        DecodedBlock {
            id: 0,
            tag: t,
            will_init,
            image: image.map(|b| {
                let mut p = Page::zeroed();
                p.0[100] = b;
                Box::new(p)
            }),
            data: vec![],
        }
    }

    #[test]
    fn image_restores_and_creates_the_file() {
        let e = env(DebugKnobs::default());
        make(&e, 1);
        let rec = record(XLOG_FPI, vec![block(tag(1, 2), Some(0xEE), false)]);
        assert!(matches!(
            read_buffer_for_redo(&e.ctx, &rec, 0).unwrap(),
            RedoBuffer::Restored
        ));
        let b = e.ctx.pool.read_buffer(tag(1, 2)).unwrap();
        let g = b.read().unwrap();
        assert_eq!(g.0[100], 0xEE);
        assert_eq!(g.lsn(), 0x20_0200);
        drop(g);
        drop(b);
        assert!(e.ctx.invalid.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_block_is_recorded_then_forgotten() {
        let e = env(DebugKnobs::default());
        let rec = record(XLOG_NOOP, vec![block(tag(1, 0), None, false)]);
        assert!(matches!(
            read_buffer_for_redo(&e.ctx, &rec, 0).unwrap(),
            RedoBuffer::NotFound
        ));
        assert!(e.ctx.invalid.lock().unwrap().check_empty().is_err());
        e.ctx.invalid.lock().unwrap().forget_relation(rel(1));
        assert!(e.ctx.invalid.lock().unwrap().is_empty());
        assert!(read_buffer_for_redo(&e.ctx, &rec, 5).is_err());
    }

    #[test]
    fn page_lsn_decides_done_or_needs_redo() {
        for (ignore, expect_done) in [(false, true), (true, false)] {
            let knobs = DebugKnobs {
                redo_ignore_page_lsn: ignore,
                ..DebugKnobs::default()
            };
            let e = env(knobs);
            make(&e, 1);
            let img = record(XLOG_FPI, vec![block(tag(1, 0), Some(1), false)]);
            read_buffer_for_redo(&e.ctx, &img, 0).unwrap();
            let rec = record(XLOG_NOOP, vec![block(tag(1, 0), None, false)]);
            let r = read_buffer_for_redo(&e.ctx, &rec, 0).unwrap();
            assert_eq!(matches!(r, RedoBuffer::Done), expect_done, "{r:?}");
            drop(r);
            let mut newer = record(XLOG_NOOP, vec![block(tag(1, 0), None, false)]);
            newer.end = Lsn(0x20_0300);
            assert!(matches!(
                read_buffer_for_redo(&e.ctx, &newer, 0).unwrap(),
                RedoBuffer::NeedsRedo(_)
            ));
        }
    }

    #[test]
    fn will_init_needs_redo_on_a_new_block() {
        let e = env(DebugKnobs::default());
        make(&e, 2);
        let rec = record(XLOG_NOOP, vec![block(tag(2, 3), None, true)]);
        match read_buffer_for_redo(&e.ctx, &rec, 0).unwrap() {
            RedoBuffer::NeedsRedo(b) => assert!(b.read().unwrap().0.iter().all(|x| *x == 0)),
            other => panic!("{other:?}"),
        }
        assert_eq!(e.ctx.smgr.nblocks(rel(2), ForkNumber::Main).unwrap(), 4);
    }

    #[test]
    fn xlog_redo_advances_next_oid_and_restores_fpi() {
        let e = env(DebugKnobs::default());
        let mut cp = CheckpointRecord {
            redo: Lsn(1),
            next_xid: Xid(9),
            oldest_xid: Xid(3),
            next_oid: 500,
            kind: CheckpointRecordKind::Online,
            full_page_writes: true,
            time: 0,
        };
        let mk = |c: &CheckpointRecord| DecodedRecord {
            main: c.encode().to_vec(),
            info: c.info(),
            ..record(0, vec![])
        };
        xlog::redo(&e.ctx, &mk(&cp)).unwrap();
        assert_eq!(e.ctx.next_oid.load(Ordering::Acquire), 500);
        cp.next_oid = 50;
        cp.kind = CheckpointRecordKind::Shutdown;
        xlog::redo(&e.ctx, &mk(&cp)).unwrap();
        assert_eq!(e.ctx.next_oid.load(Ordering::Acquire), 500);
        xlog::redo(&e.ctx, &record(xlog::XLOG_NOOP, vec![])).unwrap();
        xlog::redo(&e.ctx, &record(xlog::XLOG_CHECKPOINT_REDO, vec![])).unwrap();
        make(&e, 3);
        xlog::redo(
            &e.ctx,
            &record(XLOG_FPI, vec![block(tag(3, 0), Some(4), false)]),
        )
        .unwrap();
        assert!(
            xlog::redo(
                &e.ctx,
                &record(XLOG_FPI, vec![block(tag(3, 0), None, false)])
            )
            .is_err()
        );
        assert!(xlog::redo(&e.ctx, &record(0x70, vec![])).is_err());
    }

    #[test]
    fn run_redo_replays_counts_and_stops_at_end() {
        let e = env(DebugKnobs::default());
        // WAL を書く側（書き込みモードの Wal）を別に用意し、同じ VFS を読む。
        let w = Wal::initialize(e.vfs.clone(), cfg()).unwrap();
        make(&e, 4);
        let t = tag(4, 0);
        let page = Page::zeroed();
        let mut b = RecordBuilder::new(RmgrId::Xlog, XLOG_FPI, Xid(11));
        b.register_block(t, &page, RegFlags::STANDARD | RegFlags::FORCE_IMAGE);
        w.insert(b).unwrap();
        for x in [5u64, 20, 8] {
            let mut b = RecordBuilder::new(RmgrId::Xlog, XLOG_NOOP, Xid(x));
            b.main_data(&[1, 2, 3]);
            w.insert(b).unwrap();
        }
        w.flush(w.insert_lsn()).unwrap();
        let first = Lsn(u64::from(MIN_WAL_SEGMENT_SIZE) + 32);
        let mut reader = WalReader::open(e.vfs.clone(), &cfg(), first);
        let mut seen = 0;
        let stats = run_redo(
            &e.ctx,
            &e.wal,
            &mut reader,
            &|ctx, rec| xlog::redo(ctx, rec),
            &mut |_| seen += 1,
        )
        .unwrap();
        assert_eq!(seen, 4);
        assert_eq!(stats.records, 4);
        assert_eq!(stats.fpi_restored, 1);
        assert_eq!(stats.max_xid, Xid(20));
        assert_eq!(stats.start, first);
        assert_eq!(Some(stats.last_start), reader.last_record_start());
        assert_eq!(reader.end_of_wal().unwrap().0, w.insert_lsn());
        assert_eq!(stats.end, w.insert_lsn());

        // dispatch 中に当該レコード末尾までの flush が要求されても通る（evict を想定）。
        let mut reader = WalReader::open(e.vfs.clone(), &cfg(), first);
        let wal = e.wal.clone();
        run_redo(
            &e.ctx,
            &e.wal,
            &mut reader,
            &|_, rec| {
                wal.flush(rec.end)?;
                assert!(wal.durable_lsn() >= rec.end);
                Ok(())
            },
            &mut |_| {},
        )
        .unwrap();

        let mut reader = WalReader::open(e.vfs.clone(), &cfg(), first);
        let err = run_redo(
            &e.ctx,
            &e.wal,
            &mut reader,
            &|_, _| Err(Error::internal("boom")),
            &mut |_| {},
        );
        assert!(err.is_err());
    }
}
