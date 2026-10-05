//! `Wal`: 挿入、flush、FPW の判定、REDO 点、リカバリモード（`m3.md` §4.3、§6.3.2〜§6.3.6）。
//!
//! 挿入 Mutex の中でエンコード・位置の決定・prev と CRC の確定を行い、WAL バッファ
//! （`buf_base` から始まる連続した LSN の範囲）に足す。`flush` は `flush_lock` の中でバッファを
//! 取り出し、セグメントごとに書いて `sync_data` する。ヘッダ領域は書かない。
//!
//! `flushed_lsn()` は flush した時点の（正規化済みの）挿入位置まで進む。レコード末尾の短い
//! 余白とセグメントヘッダは 0 のまま永続化済みとみなせるため。

#![allow(clippy::cast_possible_truncation, clippy::doc_markdown)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::record::{encode_record, seal_record};
use super::segment::{
    self, align8, create_segment, header_is_valid, normalize, seg_end, segment_exists, segment_path,
};
use super::{
    Inserted, Lsn, RecordBuilder, RmgrId, SEG_HEADER_SIZE, WAL_BUFFER_FLUSH_THRESHOLD, WalConfig,
};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::buffer::WalFlush;
use crate::storage::vfs::{OpenMode, Vfs, VfsFile};
use crate::txn::Xid;
use crate::util::sync::lock;

const INFO_CHECKPOINT_REDO: u8 = 0x20;
const INFO_SWITCH: u8 = 0x30;
const ZERO_CHUNK: usize = 1 << 20;

#[derive(Debug, Default)]
struct InsertState {
    /// 次のレコードの開始位置（正規化済み）。
    insert_pos: u64,
    /// 直前のレコード（SWITCH を含む）の開始位置。最初は 0。
    prev_start: u64,
    redo: u64,
    /// `buf` の先頭の LSN。
    buf_base: u64,
    buf: Vec<u8>,
}

impl InsertState {
    /// バッファを LSN `pos` まで 0 で延ばす。
    fn pad_to(&mut self, pos: u64) {
        let want = usize::try_from(pos - self.buf_base).expect("buffer offset fits in usize");
        if self.buf.len() < want {
            self.buf.resize(want, 0);
        }
    }
}

#[derive(Debug, Default)]
struct FlushState {
    /// 現在書いているセグメント。
    current: Option<(u64, Arc<dyn VfsFile>)>,
}

#[derive(Debug)]
pub struct Wal {
    vfs: Arc<dyn Vfs>,
    cfg: WalConfig,
    /// 書き込みモードか（false はリカバリモード）。
    writing: AtomicBool,
    insert: Mutex<InsertState>,
    flush_lock: Mutex<FlushState>,
    flushed: AtomicU64,
    redo: AtomicU64,
    replayed: AtomicU64,
    poisoned: AtomicBool,
}

fn panic_err(e: Error) -> Error {
    e.with_severity(Severity::Panic)
}

impl Wal {
    fn new(vfs: Arc<dyn Vfs>, cfg: WalConfig, writing: bool) -> Wal {
        Wal {
            vfs,
            cfg,
            writing: AtomicBool::new(writing),
            insert: Mutex::new(InsertState::default()),
            flush_lock: Mutex::new(FlushState::default()),
            flushed: AtomicU64::new(0),
            redo: AtomicU64::new(0),
            replayed: AtomicU64::new(0),
            poisoned: AtomicBool::new(false),
        }
    }

    fn check_config(cfg: &WalConfig) -> Result<()> {
        let s = cfg.segment_size;
        if !s.is_power_of_two()
            || !(super::MIN_WAL_SEGMENT_SIZE..=super::MAX_WAL_SEGMENT_SIZE).contains(&s)
        {
            return Err(Error::internal(format!("invalid WAL segment size: {s}")));
        }
        Ok(())
    }

    fn start_writing(&self, insert_pos: u64, prev: u64) -> Result<()> {
        let mut st = lock(&self.insert)?;
        st.insert_pos = insert_pos;
        st.prev_start = prev;
        st.redo = insert_pos;
        st.buf_base = insert_pos;
        st.buf.clear();
        self.redo.store(insert_pos, Ordering::Release);
        self.flushed.store(insert_pos, Ordering::Release);
        self.writing.store(true, Ordering::Release);
        Ok(())
    }

    /// initdb 用。セグメント 1 を作り、書き込みモードで開く（挿入位置 = `seg_size + 32`、prev = 0）。
    pub fn initialize(vfs: Arc<dyn Vfs>, cfg: WalConfig) -> Result<Arc<Wal>> {
        Self::check_config(&cfg)?;
        vfs.create_dir_all(std::path::Path::new(segment::WAL_DIR))
            .map_err(|e| Error::from_io(&e, "could not create WAL directory"))?;
        let file = create_segment(vfs.as_ref(), &cfg, 1)?;
        let wal = Wal::new(vfs, cfg, true);
        lock(&wal.flush_lock)?.current = Some((1, file));
        wal.start_writing(u64::from(cfg.segment_size) + SEG_HEADER_SIZE, 0)?;
        Ok(Arc::new(wal))
    }

    /// 正常停止後の起動。`insert_pos` = チェックポイントレコードの end（正規化する）、
    /// prev = その start。REDO 点は挿入位置にする（最初の変更は全ページ書き込みになる）。
    pub fn open_at(
        vfs: Arc<dyn Vfs>,
        cfg: WalConfig,
        insert_pos: Lsn,
        prev: Lsn,
    ) -> Result<Arc<Wal>> {
        Self::check_config(&cfg)?;
        let wal = Wal::new(vfs, cfg, true);
        wal.start_writing(normalize(insert_pos, cfg.segment_size).0, prev.0)?;
        Ok(Arc::new(wal))
    }

    /// クラッシュリカバリ用。insert は内部エラーを返し、`flush_to` は「REDO 済みの位置以下」だけを許す。
    pub fn open_for_recovery(vfs: Arc<dyn Vfs>, cfg: WalConfig) -> Arc<Wal> {
        Arc::new(Wal::new(vfs, cfg, false))
    }

    /// REDO ループが 1 レコード適用するたびに呼ぶ。
    pub fn note_replayed(&self, end: Lsn) {
        self.replayed.fetch_max(end.0, Ordering::AcqRel);
    }

    /// REDO の後。`end_of_wal` 以降のセグメントの残りを 0 で埋めて sync、後続のセグメントを消して
    /// `sync_dir` し、書き込みモードに切り替える（§6.3.6）。
    pub fn finish_recovery(&self, end_of_wal: Lsn, last_record_start: Lsn) -> Result<()> {
        if self.writing.load(Ordering::Acquire) {
            return Err(Error::internal("finish_recovery called in write mode"));
        }
        let seg = self.cfg.segment_size;
        let end = normalize(end_of_wal, seg);
        let segno = end.segno(seg);
        let vfs = self.vfs.as_ref();
        let io_err = |e: std::io::Error, what: &str| {
            panic_err(Error::from_io(
                &e,
                format!(
                    "could not {what} WAL segment {}",
                    segment::segment_file_name(segno)
                ),
            ))
        };
        if segment_exists(vfs, segno).map_err(|e| io_err(e, "stat"))? {
            let file = segment::open_segment(vfs, segno, OpenMode::ReadWrite)
                .map_err(|e| io_err(e, "open"))?;
            let mut hdr = [0u8; SEG_HEADER_SIZE as usize];
            let header_ok =
                file.read_exact_at(&mut hdr, 0).is_ok() && header_is_valid(&hdr, &self.cfg, segno);
            if end.seg_offset(seg) == SEG_HEADER_SIZE && !header_ok {
                // ヘッダが壊れた（または作りかけの）セグメント: 消して、書き込み時に作り直す。
                vfs.remove_file(&segment_path(segno))
                    .map_err(|e| io_err(e, "remove"))?;
            } else {
                let total = seg_end(end, seg);
                let mut off = end.0;
                let zeros = vec![0u8; ZERO_CHUNK];
                while off < total {
                    let n = usize::try_from((total - off).min(ZERO_CHUNK as u64))
                        .expect("chunk fits in usize");
                    file.write_all_at(&zeros[..n], off % u64::from(seg))
                        .map_err(|e| io_err(e, "write"))?;
                    off += n as u64;
                }
                file.sync_data().map_err(|e| io_err(e, "fsync"))?;
            }
        }
        segment::remove_segments_after(vfs, segno).map_err(panic_err)?;
        lock(&self.flush_lock)?.current = None;
        self.start_writing(end.0, last_record_start.0)
    }

    /// レコードを WAL バッファに足す。FPW の判定・画像のコピー・prev の設定・CRC の計算は挿入
    /// Mutex の中で行う。戻り値の end を、登録した全ページに `set_lsn` するのは呼び出し側の責任。
    /// 失敗は `Severity::Panic`。
    #[allow(clippy::needless_pass_by_value)]
    pub fn insert(&self, rec: RecordBuilder<'_>) -> Result<Inserted> {
        if !self.writing.load(Ordering::Acquire) {
            return Err(panic_err(Error::internal("WAL insert in recovery mode")));
        }
        if self.is_poisoned() {
            return Err(Self::poisoned_error());
        }
        let (ins, needs_flush) = {
            let mut st = lock(&self.insert).map_err(panic_err)?;
            let ins = self.insert_locked(&mut st, &rec).map_err(panic_err)?;
            (ins, st.buf.len() >= WAL_BUFFER_FLUSH_THRESHOLD)
        };
        if needs_flush {
            self.flush(ins.end)?;
        }
        Ok(ins)
    }

    fn poisoned_error() -> Error {
        Error::new(
            sqlstate::IO_ERROR,
            "WAL is unusable after an earlier write failure",
        )
        .with_severity(Severity::Panic)
    }

    fn full_page_writes(&self) -> bool {
        self.cfg.full_page_writes && !self.cfg.knobs.disable_full_page_writes
    }

    fn insert_locked(&self, st: &mut InsertState, rec: &RecordBuilder<'_>) -> Result<Inserted> {
        let seg = self.cfg.segment_size;
        let fpw = self.full_page_writes();
        let mut bytes = encode_record(rec, Lsn(st.redo), fpw)?;
        let tot = bytes.len() as u64;
        let mut pos = st.insert_pos;
        let remaining = seg_end(Lsn(pos), seg) - pos;
        if tot > remaining {
            let switch = if remaining >= super::RECORD_HEADER_SIZE as u64 {
                let sw = RecordBuilder::new(RmgrId::Xlog, INFO_SWITCH, Xid::INVALID);
                Some(encode_record(&sw, Lsn(st.redo), fpw)?)
            } else {
                None
            };
            if let Some(mut sw) = switch {
                seal_record(&mut sw, Lsn(st.prev_start));
                st.pad_to(pos);
                st.buf.extend_from_slice(&sw);
                st.prev_start = pos;
            }
            pos = seg_end(Lsn(pos), seg) + SEG_HEADER_SIZE;
            st.pad_to(pos);
        }
        seal_record(&mut bytes, Lsn(st.prev_start));
        st.pad_to(pos);
        st.buf.extend_from_slice(&bytes);
        let end = pos + align8(tot);
        st.pad_to(end);
        st.prev_start = pos;
        st.insert_pos = normalize(Lsn(end), seg).0;
        Ok(Inserted {
            start: Lsn(pos),
            end: Lsn(end),
        })
    }

    /// `upto` までを write + `sync_data` する。`flushed_lsn() >= upto` なら何もしない。
    pub fn flush(&self, upto: Lsn) -> Result<()> {
        if self.flushed.load(Ordering::Acquire) >= upto.0 {
            return Ok(());
        }
        if !self.writing.load(Ordering::Acquire) {
            return if upto.0 <= self.replayed.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(panic_err(Error::internal(format!(
                    "WAL flush request {upto} is beyond the replayed position during recovery"
                ))))
            };
        }
        if self.is_poisoned() {
            return Err(Self::poisoned_error());
        }
        let mut fs = lock(&self.flush_lock).map_err(panic_err)?;
        if self.flushed.load(Ordering::Acquire) >= upto.0 {
            return Ok(());
        }
        // A flush that held the lock before us may have failed and drained
        // the buffer; writing what is left would leave a hole in the WAL.
        if self.is_poisoned() {
            return Err(Self::poisoned_error());
        }
        let (base, bytes, target) = {
            let mut st = lock(&self.insert).map_err(panic_err)?;
            if upto.0 > st.insert_pos {
                return Err(Error::internal(format!(
                    "WAL flush request {upto} is beyond the insert position {}",
                    Lsn(st.insert_pos)
                )));
            }
            let base = st.buf_base;
            let bytes = std::mem::take(&mut st.buf);
            st.buf_base = base + bytes.len() as u64;
            (base, bytes, st.insert_pos)
        };
        match self.write_out(&mut fs, base, &bytes) {
            Ok(()) => {
                self.flushed.fetch_max(target, Ordering::AcqRel);
                Ok(())
            }
            Err(e) => {
                self.poisoned.store(true, Ordering::Release);
                Err(panic_err(e))
            }
        }
    }

    /// `[base, base + bytes.len())` をセグメントごとに書いて sync する。ヘッダ領域は書かない。
    fn write_out(&self, fs: &mut FlushState, base: u64, bytes: &[u8]) -> Result<()> {
        let seg = u64::from(self.cfg.segment_size);
        let mut off = 0usize;
        let mut lsn = base;
        while off < bytes.len() {
            let segno = lsn / seg;
            let seg_off = lsn % seg;
            let n = usize::try_from((bytes.len() - off) as u64).map_or(0, |rest| {
                rest.min(usize::try_from(seg - seg_off).unwrap_or(usize::MAX))
            });
            let skip = usize::try_from(SEG_HEADER_SIZE.saturating_sub(seg_off))
                .unwrap_or(0)
                .min(n);
            if skip < n {
                let file = self.current_file(fs, segno)?;
                let name = segment::segment_file_name(segno);
                file.write_all_at(&bytes[off + skip..off + n], seg_off + skip as u64)
                    .map_err(|e| {
                        Error::from_io(&e, format!("could not write to WAL segment {name}"))
                    })?;
                file.sync_data().map_err(|e| {
                    Error::from_io(&e, format!("could not fsync WAL segment {name}"))
                })?;
            }
            off += n;
            lsn += n as u64;
        }
        Ok(())
    }

    fn current_file(&self, fs: &mut FlushState, segno: u64) -> Result<Arc<dyn VfsFile>> {
        if let Some((n, f)) = &fs.current
            && *n == segno
        {
            return Ok(Arc::clone(f));
        }
        let file = match segment::open_segment(self.vfs.as_ref(), segno, OpenMode::ReadWrite) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                create_segment(self.vfs.as_ref(), &self.cfg, segno)?
            }
            Err(e) => {
                return Err(Error::from_io(
                    &e,
                    format!(
                        "could not open WAL segment {}",
                        segment::segment_file_name(segno)
                    ),
                ));
            }
        };
        fs.current = Some((segno, Arc::clone(&file)));
        Ok(file)
    }

    /// 次のレコードの開始位置（正規化済み）。リカバリモードでは `INVALID`。
    pub fn insert_lsn(&self) -> Lsn {
        if !self.writing.load(Ordering::Acquire) {
            return Lsn::INVALID;
        }
        lock(&self.insert).map_or(Lsn::INVALID, |st| Lsn(st.insert_pos))
    }

    pub fn flushed_lsn(&self) -> Lsn {
        Lsn(self.flushed.load(Ordering::Acquire))
    }

    /// WAL-before-data の検査用。リカバリモードでは読み込み済み（永続済み）の replayed も含める。
    pub fn durable_lsn(&self) -> Lsn {
        let f = self.flushed.load(Ordering::Acquire);
        if self.writing.load(Ordering::Acquire) {
            Lsn(f)
        } else {
            Lsn(f.max(self.replayed.load(Ordering::Acquire)))
        }
    }

    /// FPW の判定に使う REDO 点。
    pub fn redo_lsn(&self) -> Lsn {
        Lsn(self.redo.load(Ordering::Acquire))
    }

    /// オンラインのチェックポイント: 挿入 Mutex の中で `CHECKPOINT_REDO` を挿入し、その開始位置を
    /// REDO 点にする。
    pub fn begin_checkpoint_online(&self) -> Result<Lsn> {
        if !self.writing.load(Ordering::Acquire) {
            return Err(Error::internal("checkpoint in recovery mode"));
        }
        if self.is_poisoned() {
            return Err(Self::poisoned_error());
        }
        let mut st = lock(&self.insert).map_err(panic_err)?;
        let rec = RecordBuilder::new(RmgrId::Xlog, INFO_CHECKPOINT_REDO, Xid::INVALID);
        let ins = self.insert_locked(&mut st, &rec).map_err(panic_err)?;
        st.redo = ins.start.0;
        self.redo.store(ins.start.0, Ordering::Release);
        Ok(ins.start)
    }

    /// 停止・リカバリ終了のチェックポイント（他に書き手がいない）: 現在の挿入位置を REDO 点にする。
    pub fn begin_checkpoint_quiet(&self) -> Result<Lsn> {
        if !self.writing.load(Ordering::Acquire) {
            return Err(Error::internal("checkpoint in recovery mode"));
        }
        let mut st = lock(&self.insert).map_err(panic_err)?;
        st.redo = st.insert_pos;
        self.redo.store(st.insert_pos, Ordering::Release);
        Ok(Lsn(st.insert_pos))
    }

    /// REDO 点を含むセグメントより前のセグメントを消す。個々の削除の失敗は無視する
    /// （正しさに影響しない）。
    pub fn remove_segments_before(&self, redo: Lsn) -> Result<()> {
        segment::remove_segments_before(self.vfs.as_ref(), redo.segno(self.cfg.segment_size))
            .map(|_| ())
    }

    /// 現在の REDO 点からの WAL のバイト数（WAL 量の契機）。
    pub fn bytes_since_redo(&self) -> u64 {
        if !self.writing.load(Ordering::Acquire) {
            return 0;
        }
        lock(&self.insert).map_or(0, |st| st.insert_pos.saturating_sub(st.redo))
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    pub fn config(&self) -> &WalConfig {
        &self.cfg
    }
}

impl WalFlush for Wal {
    /// 書き込みモードは `flush(Lsn(lsn))`、リカバリモードは replayed 以下か検査するだけ。
    fn flush_to(&self, lsn: u64) -> Result<()> {
        self.flush(Lsn(lsn))
    }

    fn redo_ptr(&self) -> u64 {
        self.redo_lsn().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::page::Page;
    use crate::storage::smgr::{BufferTag, ForkNumber, RelFileLocator, RelFileNumber};
    use crate::storage::vfs::{CrashMode, FaultEffect, FaultOp, FaultPlan, FaultRule, SimVfs};
    use crate::wal::record::{decode_record, record_prev, record_tot_len};
    use crate::wal::{DecodedRecord, MAX_RECORD_LEN, RegFlags};
    use std::time::Duration;

    const SEG: u32 = super::super::MIN_WAL_SEGMENT_SIZE;
    const S: u64 = SEG as u64;

    fn cfg() -> WalConfig {
        WalConfig {
            segment_size: SEG,
            system_identifier: 42,
            full_page_writes: true,
            knobs: DebugKnobs::default(),
        }
    }

    fn setup() -> (Arc<SimVfs>, Arc<Wal>) {
        let vfs = Arc::new(SimVfs::new(7));
        let wal = Wal::initialize(Arc::clone(&vfs) as Arc<dyn Vfs>, cfg()).unwrap();
        vfs.sync_dir(std::path::Path::new("")).unwrap();
        (vfs, wal)
    }

    fn noop(len: usize) -> RecordBuilder<'static> {
        let mut r = RecordBuilder::new(RmgrId::Xlog, 0x50, Xid(0));
        r.main_data(&vec![0xA5; len]);
        r
    }

    /// 最小の読み手。(開始 LSN, レコード, prev) を返す。SWITCH も含める。
    fn read_all(vfs: &dyn Vfs, start: Lsn) -> Vec<(DecodedRecord, Lsn)> {
        let mut out = Vec::new();
        let mut pos = normalize(start, SEG);
        let mut prev_start: Option<Lsn> = None;
        loop {
            let Ok(f) = segment::open_segment(vfs, pos.segno(SEG), OpenMode::ReadOnly) else {
                break;
            };
            let mut hdr = [0u8; 32];
            f.read_exact_at(&mut hdr, 0).unwrap();
            assert!(header_is_valid(&hdr, &cfg(), pos.segno(SEG)));
            f.read_exact_at(&mut hdr, pos.seg_offset(SEG)).unwrap();
            let tot = record_tot_len(&hdr) as usize;
            if tot == 0 {
                break;
            }
            assert!(
                pos.0 + tot as u64 <= seg_end(pos, SEG),
                "record crosses segment"
            );
            let mut buf = vec![0u8; tot];
            f.read_exact_at(&mut buf, pos.seg_offset(SEG)).unwrap();
            let rec = decode_record(&buf, pos).unwrap();
            let prev = record_prev(&buf);
            if let Some(p) = prev_start {
                assert_eq!(prev, p, "prev chain broken at {pos}");
            }
            prev_start = Some(pos);
            let is_switch = rec.rmgr == RmgrId::Xlog && rec.info == INFO_SWITCH;
            let end = rec.end;
            out.push((rec, prev));
            pos = if is_switch {
                segment::next_segment_start(pos, SEG)
            } else {
                normalize(end, SEG)
            };
        }
        out
    }

    #[test]
    fn initialize_creates_segment_one() {
        let (vfs, wal) = setup();
        assert_eq!(wal.insert_lsn(), Lsn(S + 32));
        assert_eq!(wal.flushed_lsn(), Lsn(S + 32));
        assert_eq!(wal.redo_lsn(), Lsn(S + 32));
        assert_eq!(segment::list_segments(vfs.as_ref()).unwrap(), vec![1]);
        assert!(!wal.is_poisoned());
        assert_eq!(wal.bytes_since_redo(), 0);
    }

    #[test]
    fn insert_flush_and_read_back() {
        let (vfs, wal) = setup();
        let a = wal.insert(noop(10)).unwrap();
        let b = wal.insert(noop(100)).unwrap();
        assert_eq!(a.start, Lsn(S + 32));
        assert_eq!(a.end, Lsn(S + 32 + 48));
        assert_eq!(b.start, a.end);
        assert_eq!(b.end, Lsn(b.start.0 + 136));
        assert_eq!(wal.insert_lsn(), b.end);
        assert_eq!(wal.flushed_lsn(), Lsn(S + 32));
        wal.flush(b.end).unwrap();
        assert_eq!(wal.flushed_lsn(), b.end);
        // 2 回目は何もしない。
        wal.flush(a.end).unwrap();
        let recs = read_all(vfs.as_ref(), Lsn(S + 32));
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].1, Lsn(0));
        assert_eq!(recs[1].1, a.start);
        assert_eq!(recs[1].0.main.len(), 100);
        assert_eq!(recs[1].0.end, b.end);
    }

    #[test]
    fn flush_beyond_insert_pos_is_an_error() {
        let (_vfs, wal) = setup();
        let e = wal.flush(Lsn(S + 1000)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        assert!(!wal.is_poisoned());
    }

    #[test]
    fn flushed_data_survives_a_crash_and_unflushed_does_not_corrupt() {
        let (vfs, wal) = setup();
        let a = wal.insert(noop(10)).unwrap();
        wal.flush(a.end).unwrap();
        let _b = wal.insert(noop(10)).unwrap(); // flush しない
        let after = vfs.crash(CrashMode::DropUnsynced);
        let recs = read_all(&after, Lsn(S + 32));
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].0.start, a.start);
    }

    #[test]
    fn switch_record_when_tail_is_large_enough() {
        let (vfs, wal) = setup();
        // 末尾に 100 バイト残す。
        let mut remaining = S - (wal.insert_lsn().0 % S);
        while remaining > MAX_RECORD_LEN as u64 + 100 {
            wal.insert(noop(MAX_RECORD_LEN - 32)).unwrap();
            remaining = S - (wal.insert_lsn().0 % S);
        }
        let fill = remaining - 104; // 残り 104（8 の倍数）→ tot_len = remaining - 104
        let last = wal.insert(noop(fill as usize - 32)).unwrap();
        assert_eq!(S - (wal.insert_lsn().0 % S), 104);
        let big = wal.insert(noop(200)).unwrap();
        // SWITCH は last.end にあり、big は次のセグメントの先頭。
        assert_eq!(big.start, Lsn(2 * S + 32));
        wal.flush(big.end).unwrap();
        let recs = read_all(vfs.as_ref(), Lsn(S + 32));
        let sw = recs
            .iter()
            .find(|(r, _)| r.rmgr == RmgrId::Xlog && r.info == INFO_SWITCH)
            .expect("switch");
        assert_eq!(sw.0.start, last.end);
        assert_eq!(sw.1, last.start);
        let (r, prev) = recs.last().unwrap();
        assert_eq!(r.start, big.start);
        assert_eq!(*prev, sw.0.start);
        assert_eq!(segment::list_segments(vfs.as_ref()).unwrap(), vec![1, 2]);
    }

    #[test]
    fn no_switch_when_tail_is_shorter_than_a_header() {
        let (vfs, wal) = setup();
        let mut remaining = S - (wal.insert_lsn().0 % S);
        while remaining > MAX_RECORD_LEN as u64 + 100 {
            wal.insert(noop(MAX_RECORD_LEN - 32)).unwrap();
            remaining = S - (wal.insert_lsn().0 % S);
        }
        let last = wal.insert(noop(remaining as usize - 16 - 32)).unwrap();
        assert_eq!(last.end.0 % S, S - 16);
        // 正規化で次のセグメントの先頭に進む。
        assert_eq!(wal.insert_lsn(), Lsn(2 * S + 32));
        let next = wal.insert(noop(8)).unwrap();
        assert_eq!(next.start, Lsn(2 * S + 32));
        wal.flush(next.end).unwrap();
        let recs = read_all(vfs.as_ref(), Lsn(S + 32));
        assert!(
            recs.iter()
                .all(|(r, _)| !(r.rmgr == RmgrId::Xlog && r.info == INFO_SWITCH))
        );
        let (r, prev) = recs.last().unwrap();
        assert_eq!(r.start, next.start);
        assert_eq!(*prev, last.start);
    }

    #[test]
    fn record_ending_exactly_at_segment_end() {
        let (vfs, wal) = setup();
        let mut remaining = S - (wal.insert_lsn().0 % S);
        while remaining > MAX_RECORD_LEN as u64 + 100 {
            wal.insert(noop(MAX_RECORD_LEN - 32)).unwrap();
            remaining = S - (wal.insert_lsn().0 % S);
        }
        let last = wal.insert(noop(remaining as usize - 32)).unwrap();
        assert_eq!(last.end.0, 2 * S);
        assert_eq!(wal.insert_lsn(), Lsn(2 * S + 32));
        let next = wal.insert(noop(0)).unwrap();
        wal.flush(next.end).unwrap();
        assert_eq!(
            read_all(vfs.as_ref(), Lsn(S + 32)).last().unwrap().0.start,
            next.start
        );
    }

    #[test]
    fn many_records_across_segments_read_back_in_order() {
        let (vfs, wal) = setup();
        let mut ends = Vec::new();
        for i in 0..6000usize {
            ends.push(wal.insert(noop(i % 700)).unwrap());
        }
        wal.flush(wal.insert_lsn()).unwrap();
        assert_eq!(wal.flushed_lsn(), wal.insert_lsn());
        let recs = read_all(vfs.as_ref(), Lsn(S + 32));
        let normal: Vec<_> = recs
            .iter()
            .filter(|(r, _)| !(r.rmgr == RmgrId::Xlog && r.info == INFO_SWITCH))
            .collect();
        assert_eq!(normal.len(), 6000);
        for (i, (r, _)) in normal.iter().enumerate() {
            assert_eq!(r.start, ends[i].start);
            assert_eq!(r.main.len(), i % 700);
        }
        assert!(segment::list_segments(vfs.as_ref()).unwrap().len() >= 2);
    }

    #[test]
    fn oversize_record_is_rejected_without_state_change() {
        let (_vfs, wal) = setup();
        let before = wal.insert_lsn();
        let e = wal.insert(noop(MAX_RECORD_LEN)).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(wal.insert_lsn(), before);
        assert!(wal.insert(noop(10)).is_ok());
    }

    #[test]
    fn full_page_image_depends_on_redo_point() {
        let (vfs, wal) = setup();
        let mut page = Page::zeroed();
        page.init_heap();
        let tag = BufferTag {
            rel: RelFileLocator {
                spc_oid: 1663,
                db_oid: 5,
                rel_number: RelFileNumber(1),
            },
            fork: ForkNumber::Main,
            block: 0,
        };
        let mk = |wal: &Wal, page: &Page| {
            let mut r = RecordBuilder::new(RmgrId::Heap, 0x00, Xid(1));
            let id = r.register_block(tag, page, RegFlags::STANDARD);
            r.block_data(id, &[1, 2, 3]);
            wal.insert(r).unwrap()
        };
        // 新しいページ（lsn 0 <= redo）→ 画像あり。
        let a = mk(&wal, &page);
        page.set_lsn(a.end.0);
        // redo より後に変更済み → 画像なし。
        let b = mk(&wal, &page);
        page.set_lsn(b.end.0);
        // オンラインのチェックポイントで REDO 点が進む → 再び画像あり。
        let redo = wal.begin_checkpoint_online().unwrap();
        assert_eq!(wal.redo_lsn(), redo);
        assert!(redo >= b.end);
        let c = mk(&wal, &page);
        wal.flush(c.end).unwrap();
        let recs = read_all(vfs.as_ref(), Lsn(S + 32));
        let r: Vec<_> = recs.iter().map(|(r, _)| r).collect();
        assert!(r[0].blocks[0].image.is_some());
        assert!(r[1].blocks[0].image.is_none());
        assert_eq!((r[2].rmgr, r[2].info), (RmgrId::Xlog, INFO_CHECKPOINT_REDO));
        assert_eq!(r[2].start, redo);
        assert!(r[3].blocks[0].image.is_some());
        assert!(wal.bytes_since_redo() > 0);
    }

    #[test]
    fn disable_full_page_writes_knob() {
        let vfs = Arc::new(SimVfs::new(1));
        let mut c = cfg();
        c.knobs.disable_full_page_writes = true;
        let wal = Wal::initialize(vfs as Arc<dyn Vfs>, c).unwrap();
        let mut page = Page::zeroed();
        page.init_heap();
        let tag = BufferTag {
            rel: RelFileLocator {
                spc_oid: 1663,
                db_oid: 5,
                rel_number: RelFileNumber(1),
            },
            fork: ForkNumber::Main,
            block: 0,
        };
        let mut r = RecordBuilder::new(RmgrId::Heap, 0x00, Xid(1));
        r.register_block(tag, &page, RegFlags::STANDARD);
        let ins = wal.insert(r).unwrap();
        assert_eq!(ins.end.0 - ins.start.0, 56);
    }

    #[test]
    fn quiet_checkpoint_sets_redo_to_insert_pos() {
        let (_vfs, wal) = setup();
        wal.insert(noop(10)).unwrap();
        let r = wal.begin_checkpoint_quiet().unwrap();
        assert_eq!(r, wal.insert_lsn());
        assert_eq!(wal.redo_lsn(), r);
        assert_eq!(wal.bytes_since_redo(), 0);
        wal.insert(noop(10)).unwrap();
        assert_eq!(wal.bytes_since_redo(), 48);
    }

    #[test]
    fn automatic_flush_when_buffer_is_large() {
        let (_vfs, wal) = setup();
        let mut last = None;
        for _ in 0..5 {
            last = Some(wal.insert(noop(MAX_RECORD_LEN - 32)).unwrap());
        }
        // 4 MiB を超えた挿入で flush されている。
        assert!(wal.flushed_lsn() >= Lsn(S + 32 + 4 * MAX_RECORD_LEN as u64));
        assert!(wal.flushed_lsn() <= last.unwrap().end.max(wal.insert_lsn()));
    }

    #[test]
    fn segment_removal_keeps_the_redo_segment() {
        let (vfs, wal) = setup();
        for _ in 0..5 {
            wal.insert(noop(MAX_RECORD_LEN - 32)).unwrap();
        }
        wal.flush(wal.insert_lsn()).unwrap();
        let segs = segment::list_segments(vfs.as_ref()).unwrap();
        assert!(segs.len() >= 3, "{segs:?}");
        wal.remove_segments_before(Lsn(3 * S + 40)).unwrap();
        assert_eq!(segment::list_segments(vfs.as_ref()).unwrap()[0], 3);
    }

    #[test]
    fn a_waiting_flush_does_not_succeed_after_a_failed_flush() {
        let (vfs, wal) = setup();
        let a = wal.insert(noop(10)).unwrap();
        vfs.set_faults(FaultPlan {
            rules: vec![
                FaultRule {
                    op: FaultOp::Write,
                    path_prefix: None,
                    nth: Some(1),
                    probability: None,
                    effect: FaultEffect::Delay(Duration::from_millis(400)),
                },
                FaultRule {
                    op: FaultOp::Sync,
                    path_prefix: None,
                    nth: Some(1),
                    probability: None,
                    effect: FaultEffect::Error(std::io::ErrorKind::Other),
                },
            ],
        });
        let wa = Arc::clone(&wal);
        let ta = std::thread::spawn(move || wa.flush(a.end));
        std::thread::sleep(Duration::from_millis(100));
        let b = wal.insert(noop(10)).unwrap();
        let rb = wal.flush(b.end);
        assert!(ta.join().unwrap().is_err());
        assert!(rb.is_err(), "the waiting flush must not report success");
        assert!(wal.is_poisoned());
        assert!(wal.flushed_lsn() < b.end);
    }

    #[test]
    fn write_failure_poisons_the_wal() {
        let (vfs, wal) = setup();
        let a = wal.insert(noop(10)).unwrap();
        vfs.set_faults(FaultPlan {
            rules: vec![FaultRule {
                op: FaultOp::Any,
                path_prefix: None,
                nth: Some(1),
                probability: None,
                effect: FaultEffect::Error(std::io::ErrorKind::Other),
            }],
        });
        let e = wal.flush(a.end).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(e.sqlstate, sqlstate::IO_ERROR);
        assert!(wal.is_poisoned());
        assert_eq!(wal.insert(noop(1)).unwrap_err().severity, Severity::Panic);
        assert_eq!(wal.flush(a.end).unwrap_err().severity, Severity::Panic);
    }

    #[test]
    fn recovery_mode_semantics() {
        let (vfs, wal) = setup();
        let a = wal.insert(noop(10)).unwrap();
        let b = wal.insert(noop(10)).unwrap();
        wal.flush(b.end).unwrap();
        drop(wal);

        let rec = Wal::open_for_recovery(Arc::clone(&vfs) as Arc<dyn Vfs>, cfg());
        assert!(rec.insert(noop(1)).is_err());
        assert!(rec.flush_to(a.end.0).is_err());
        rec.note_replayed(a.end);
        rec.flush_to(a.end.0).unwrap();
        assert!(rec.flush_to(b.end.0).is_err());
        assert_eq!(rec.durable_lsn(), a.end);
        rec.note_replayed(b.end);
        rec.flush_to(b.end.0).unwrap();
        assert_eq!(rec.durable_lsn(), b.end);
        assert_eq!(rec.insert_lsn(), Lsn::INVALID);

        // 末尾を壊す（2 つ目のレコードがなかったことにして、0 で上書き）。
        rec.finish_recovery(a.end, a.start).unwrap();
        assert_eq!(rec.insert_lsn(), a.end);
        assert_eq!(rec.flushed_lsn(), a.end);
        let c = rec.insert(noop(24)).unwrap();
        assert_eq!(c.start, a.end);
        rec.flush(c.end).unwrap();
        let recs = read_all(vfs.as_ref(), Lsn(S + 32));
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[1].0.main.len(), 24);
        assert_eq!(recs[1].1, a.start);
    }

    #[test]
    fn finish_recovery_zeroes_tail_and_removes_later_segments() {
        let (vfs, wal) = setup();
        for _ in 0..5 {
            wal.insert(noop(MAX_RECORD_LEN - 32)).unwrap();
        }
        wal.flush(wal.insert_lsn()).unwrap();
        drop(wal);
        assert!(segment::list_segments(vfs.as_ref()).unwrap().len() >= 3);
        let rec = Wal::open_for_recovery(Arc::clone(&vfs) as Arc<dyn Vfs>, cfg());
        let end = Lsn(S + 32 + 100);
        rec.finish_recovery(end, Lsn(S + 32)).unwrap();
        assert_eq!(segment::list_segments(vfs.as_ref()).unwrap(), vec![1]);
        let f = segment::open_segment(vfs.as_ref(), 1, OpenMode::ReadOnly).unwrap();
        let mut buf = vec![0u8; (S - end.0 % S) as usize];
        f.read_exact_at(&mut buf, end.0 % S).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
        // 残したレコード（32 + 100 バイト目より前）は壊れていない。
        let mut head = [0u8; 4];
        f.read_exact_at(&mut head, 32).unwrap();
        assert_ne!(head, [0; 4]);
    }

    #[test]
    fn finish_recovery_recreates_a_bad_header_segment() {
        let vfs = Arc::new(SimVfs::new(3));
        vfs.create_dir_all(std::path::Path::new("pg_wal")).unwrap();
        // 作りかけ（ヘッダなしの 0 埋め）のセグメント 2。
        let f = vfs.open(&segment_path(2), OpenMode::CreateNew).unwrap();
        f.set_len(S).unwrap();
        let rec = Wal::open_for_recovery(Arc::clone(&vfs) as Arc<dyn Vfs>, cfg());
        rec.finish_recovery(Lsn(2 * S + 32), Lsn(S + 40)).unwrap();
        assert!(!segment_exists(vfs.as_ref(), 2).unwrap());
        let r = rec.insert(noop(8)).unwrap();
        rec.flush(r.end).unwrap();
        let recs = read_all(vfs.as_ref(), Lsn(2 * S + 32));
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].1, Lsn(S + 40));
    }

    #[test]
    fn open_at_normalizes_and_continues() {
        let (vfs, wal) = setup();
        let a = wal.insert(noop(10)).unwrap();
        wal.flush(a.end).unwrap();
        drop(wal);
        let wal = Wal::open_at(Arc::clone(&vfs) as Arc<dyn Vfs>, cfg(), a.end, a.start).unwrap();
        assert_eq!(wal.insert_lsn(), a.end);
        let b = wal.insert(noop(10)).unwrap();
        wal.flush(b.end).unwrap();
        let recs = read_all(vfs.as_ref(), Lsn(S + 32));
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[1].1, a.start);

        // セグメント末尾の短い余白は次のセグメントに正規化される。
        let w2 = Wal::open_at(vfs as Arc<dyn Vfs>, cfg(), Lsn(2 * S - 16), Lsn(S + 40)).unwrap();
        assert_eq!(w2.insert_lsn(), Lsn(2 * S + 32));
    }

    #[test]
    fn crash_at_every_io_during_inserts_leaves_a_valid_prefix() {
        // 各 I/O の直前でクラッシュしても、読めるレコード列は flush 済みの接頭辞を含み、prev の鎖は切れない。
        let mut total_ops = 0;
        for crash_at in 0..200u64 {
            let vfs = Arc::new(SimVfs::new(9));
            let wal = Wal::initialize(Arc::clone(&vfs) as Arc<dyn Vfs>, cfg()).unwrap();
            vfs.sync_dir(std::path::Path::new("")).unwrap();
            let base = vfs.op_count();
            vfs.set_faults(FaultPlan {
                rules: vec![FaultRule {
                    op: FaultOp::Any,
                    path_prefix: None,
                    nth: Some(crash_at + 1),
                    probability: None,
                    effect: FaultEffect::CrashFreeze,
                }],
            });
            let mut durable = Vec::new();
            for i in 0..6usize {
                let Ok(ins) = wal.insert(noop(i * 100)) else {
                    break;
                };
                if wal.flush(ins.end).is_ok() {
                    durable.push(ins.start);
                } else {
                    break;
                }
            }
            total_ops = total_ops.max(vfs.op_count() - base);
            let after = vfs.crash(CrashMode::DropUnsynced);
            let recs = read_all(&after, Lsn(S + 32));
            let starts: Vec<Lsn> = recs.iter().map(|(r, _)| r.start).collect();
            assert!(starts.len() >= durable.len(), "crash_at {crash_at}");
            assert_eq!(
                &starts[..durable.len()],
                &durable[..],
                "crash_at {crash_at}"
            );
        }
        assert!(total_ops > 0);
    }
}
