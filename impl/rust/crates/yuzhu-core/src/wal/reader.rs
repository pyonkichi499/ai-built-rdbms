//! `WalReader`: WAL を先頭から読み、終わりを判定する（`m3.md` §4.4、§6.3.5）。

use std::io;
use std::sync::Arc;

use super::record::{RecordError, decode_record};
use super::segment::{
    header_is_valid, next_segment_start, normalize, open_segment, seg_end, segment_exists,
};
use super::xlog::XLOG_SWITCH;
use super::{DecodedRecord, Lsn, MAX_RECORD_LEN, RECORD_HEADER_SIZE, RmgrId, WalConfig};
use crate::error::{Error, Result};
use crate::storage::vfs::{OpenMode, Vfs, VfsFile};

#[derive(Debug)]
pub struct WalReader {
    vfs: Arc<dyn Vfs>,
    cfg: WalConfig,
    pos: Lsn,
    prev_start: Option<Lsn>,
    include_switch: bool,
    end: Option<(Lsn, EndReason)>,
    last_start: Option<Lsn>,
    /// ヘッダ検査済みの現在のセグメント。
    current: Option<(u64, Arc<dyn VfsFile>)>,
}

/// WAL の終わりと判定した理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    ZeroLength,
    BadHeader(RecordError),
    Crc,
    PrevMismatch,
    CrossesSegment,
    MissingSegment(u64),
    BadSegmentHeader(u64),
    TooLong,
}

type SegmentOrEnd = std::result::Result<Arc<dyn VfsFile>, EndReason>;

impl WalReader {
    /// `start` から読む。最初のレコードの prev は検査しない（以後は直前のレコードの開始と一致すること）。
    pub fn open(vfs: Arc<dyn Vfs>, cfg: &WalConfig, start: Lsn) -> WalReader {
        WalReader {
            vfs,
            cfg: *cfg,
            pos: normalize(start, cfg.segment_size),
            prev_start: None,
            include_switch: false,
            end: None,
            last_start: None,
            current: None,
        }
    }

    /// 次のレコード。WAL の終わり（不正なレコードを含む）なら `Ok(None)`。`NotFound` 以外の I/O エラーは
    /// `Err`。SWITCH は返さずに次のセグメントへ進む（dump 用に `include_switch` を立てれば返す）。
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<DecodedRecord>> {
        if self.end.is_some() {
            return Ok(None);
        }
        let seg = self.cfg.segment_size;
        loop {
            self.pos = normalize(self.pos, seg);
            let pos = self.pos;
            let file = match self.segment_file(pos.segno(seg))? {
                Ok(f) => f,
                Err(reason) => return Ok(self.finish(pos, reason)),
            };
            let off = pos.seg_offset(seg);

            let mut head = [0u8; RECORD_HEADER_SIZE];
            if let Err(reason) = read_at(file.as_ref(), &mut head, off)? {
                return Ok(self.finish(pos, reason));
            }
            let tot_len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
            if tot_len == 0 {
                return Ok(self.finish(pos, EndReason::ZeroLength));
            }
            if tot_len < RECORD_HEADER_SIZE {
                return Ok(self.finish(pos, EndReason::BadHeader(RecordError::BadLength)));
            }
            if tot_len > MAX_RECORD_LEN {
                return Ok(self.finish(pos, EndReason::TooLong));
            }
            if pos.0 + tot_len as u64 > seg_end(pos, seg) {
                return Ok(self.finish(pos, EndReason::CrossesSegment));
            }
            let mut buf = vec![0u8; tot_len];
            buf[..RECORD_HEADER_SIZE].copy_from_slice(&head);
            let body_off = off + RECORD_HEADER_SIZE as u64;
            if let Err(reason) = read_at(file.as_ref(), &mut buf[RECORD_HEADER_SIZE..], body_off)? {
                return Ok(self.finish(pos, reason));
            }
            let rec = match decode_record(&buf, pos) {
                Ok(r) => r,
                Err(RecordError::BadCrc) => return Ok(self.finish(pos, EndReason::Crc)),
                Err(RecordError::BadPrev) => {
                    return Ok(self.finish(pos, EndReason::PrevMismatch));
                }
                Err(e) => return Ok(self.finish(pos, EndReason::BadHeader(e))),
            };
            if let Some(prev) = self.prev_start
                && prev_of(&buf) != prev.0
            {
                return Ok(self.finish(pos, EndReason::PrevMismatch));
            }
            self.prev_start = Some(pos);
            self.last_start = Some(pos);
            if rec.rmgr == RmgrId::Xlog && rec.info == XLOG_SWITCH {
                self.pos = next_segment_start(pos, seg);
                if self.include_switch {
                    return Ok(Some(rec));
                }
                continue;
            }
            self.pos = normalize(rec.end, seg);
            return Ok(Some(rec));
        }
    }

    /// 終わりを記録する（呼び出し側は `Ok(None)` を返す）。
    fn finish(&mut self, pos: Lsn, reason: EndReason) -> Option<DecodedRecord> {
        self.end = Some((pos, reason));
        None
    }

    /// 現在のセグメントを返す。ない・ヘッダ不正なら `Ok(Err(理由))`。
    fn segment_file(&mut self, segno: u64) -> Result<SegmentOrEnd> {
        if let Some((n, f)) = &self.current
            && *n == segno
        {
            return Ok(Ok(Arc::clone(f)));
        }
        self.current = None;
        let io_err =
            |e: &io::Error| Error::from_io(e, format!("could not open WAL segment {segno:016X}"));
        match segment_exists(self.vfs.as_ref(), segno) {
            Ok(true) => {}
            Ok(false) => return Ok(Err(EndReason::MissingSegment(segno))),
            Err(e) => return Err(io_err(&e)),
        }
        let file = match open_segment(self.vfs.as_ref(), segno, OpenMode::ReadOnly) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Err(EndReason::MissingSegment(segno)));
            }
            Err(e) => return Err(io_err(&e)),
        };
        let mut h = [0u8; 32];
        if read_at(file.as_ref(), &mut h, 0)?.is_err() || !header_is_valid(&h, &self.cfg, segno) {
            return Ok(Err(EndReason::BadSegmentHeader(segno)));
        }
        self.current = Some((segno, Arc::clone(&file)));
        Ok(Ok(file))
    }

    pub fn set_include_switch(&mut self, on: bool) {
        self.include_switch = on;
    }

    /// `None` を返した後で有効。次のレコードを書くべき位置（正規化済み）と理由。
    pub fn end_of_wal(&self) -> Option<(Lsn, EndReason)> {
        self.end.clone()
    }

    /// 最後に返した（SWITCH を含む）有効なレコードの開始 LSN。
    pub fn last_record_start(&self) -> Option<Lsn> {
        self.last_start
    }
}

fn prev_of(buf: &[u8]) -> u64 {
    u64::from_le_bytes(buf[8..16].try_into().unwrap_or([0; 8]))
}

/// ファイルの末尾を超えるなら `Ok(Err(終わりの理由))`、それ以外の I/O エラーは `Err`。
fn read_at(
    file: &dyn VfsFile,
    buf: &mut [u8],
    off: u64,
) -> Result<std::result::Result<(), EndReason>> {
    match file.read_exact_at(buf, off) {
        Ok(()) => Ok(Ok(())),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            Ok(Err(EndReason::BadHeader(RecordError::TooShort)))
        }
        Err(e) => Err(Error::from_io(&e, "could not read WAL segment")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::segment::segment_path;
    use super::super::xlog::XLOG_NOOP;
    use super::super::{MIN_WAL_SEGMENT_SIZE, SEG_HEADER_SIZE, Wal};
    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::vfs::SimVfs;
    use crate::txn::Xid;
    use crate::wal::RecordBuilder;

    fn cfg() -> WalConfig {
        WalConfig {
            segment_size: MIN_WAL_SEGMENT_SIZE,
            system_identifier: 7,
            full_page_writes: true,
            knobs: DebugKnobs::default(),
        }
    }

    fn noop(wal: &Wal, len: usize) -> Lsn {
        let mut b = RecordBuilder::new(RmgrId::Xlog, XLOG_NOOP, Xid(5));
        b.main_data(&vec![0xAB; len]);
        wal.insert(b).unwrap().start
    }

    fn setup() -> (Arc<SimVfs>, Arc<Wal>) {
        let vfs = Arc::new(SimVfs::new(1));
        let _ = vfs.create_dir_all(Path::new("pg_wal"));
        let wal = Wal::initialize(vfs.clone(), cfg()).unwrap();
        (vfs, wal)
    }

    fn first() -> Lsn {
        Lsn(u64::from(MIN_WAL_SEGMENT_SIZE) + SEG_HEADER_SIZE)
    }

    #[test]
    fn reads_records_in_order_and_ends_at_zero_length() {
        let (vfs, wal) = setup();
        let starts: Vec<Lsn> = (0..5).map(|i| noop(&wal, 10 + i * 13)).collect();
        wal.flush(wal.insert_lsn()).unwrap();
        let mut r = WalReader::open(vfs, &cfg(), first());
        for s in &starts {
            let rec = r.next().unwrap().unwrap();
            assert_eq!(rec.start, *s);
            assert_eq!(rec.main[0], 0xAB);
        }
        assert!(r.next().unwrap().is_none());
        assert_eq!(
            r.end_of_wal(),
            Some((wal.insert_lsn(), EndReason::ZeroLength))
        );
        assert_eq!(r.last_record_start(), starts.last().copied());
        assert!(r.next().unwrap().is_none());
    }

    #[test]
    fn start_in_the_middle_skips_prev_check() {
        let (vfs, wal) = setup();
        let starts: Vec<Lsn> = (0..3).map(|_| noop(&wal, 20)).collect();
        wal.flush(wal.insert_lsn()).unwrap();
        let mut r = WalReader::open(vfs, &cfg(), starts[1]);
        assert_eq!(r.next().unwrap().unwrap().start, starts[1]);
        assert_eq!(r.next().unwrap().unwrap().start, starts[2]);
        assert!(r.next().unwrap().is_none());
    }

    #[test]
    fn crosses_segments_via_switch() {
        let (vfs, wal) = setup();
        let big = 900 * 1024;
        let starts: Vec<Lsn> = (0..3).map(|_| noop(&wal, big)).collect();
        wal.flush(wal.insert_lsn()).unwrap();
        assert_eq!(starts[2].segno(MIN_WAL_SEGMENT_SIZE), 2);

        let mut r = WalReader::open(vfs.clone(), &cfg(), first());
        let got: Vec<Lsn> = std::iter::from_fn(|| r.next().unwrap().map(|x| x.start)).collect();
        assert_eq!(got, starts);

        let mut r = WalReader::open(vfs, &cfg(), first());
        r.set_include_switch(true);
        let mut n = 0;
        let mut switches = 0;
        while let Some(rec) = r.next().unwrap() {
            n += 1;
            if rec.info == XLOG_SWITCH && rec.rmgr == RmgrId::Xlog {
                switches += 1;
            }
        }
        assert_eq!(switches, 1);
        assert_eq!(n, 4);
    }

    #[test]
    fn missing_next_segment_ends_there() {
        let (vfs, wal) = setup();
        noop(&wal, 900 * 1024);
        noop(&wal, 900 * 1024);
        // 3 つ目は次のセグメントに入る。SWITCH を書かせるために入れて、そのセグメントを消す。
        let third = noop(&wal, 900 * 1024);
        wal.flush(wal.insert_lsn()).unwrap();
        vfs.remove_file(&segment_path(third.segno(MIN_WAL_SEGMENT_SIZE)))
            .unwrap();
        let mut r = WalReader::open(vfs, &cfg(), first());
        assert!(r.next().unwrap().is_some());
        assert!(r.next().unwrap().is_some());
        assert!(r.next().unwrap().is_none());
        let (lsn, why) = r.end_of_wal().unwrap();
        assert_eq!(why, EndReason::MissingSegment(2));
        assert_eq!(lsn, third);
    }

    #[test]
    fn corrupt_byte_is_crc_end() {
        let (vfs, wal) = setup();
        let a = noop(&wal, 40);
        let b = noop(&wal, 40);
        wal.flush(wal.insert_lsn()).unwrap();
        let f = vfs.open(&segment_path(1), OpenMode::ReadWrite).unwrap();
        f.write_all_at(&[0x55], b.seg_offset(MIN_WAL_SEGMENT_SIZE) + 40)
            .unwrap();
        let mut r = WalReader::open(vfs, &cfg(), a);
        assert_eq!(r.next().unwrap().unwrap().start, a);
        assert!(r.next().unwrap().is_none());
        assert_eq!(r.end_of_wal(), Some((b, EndReason::Crc)));
        assert_eq!(r.last_record_start(), Some(a));
    }

    #[test]
    fn bad_lengths_and_segment_header() {
        let (vfs, wal) = setup();
        let a = noop(&wal, 40);
        wal.flush(wal.insert_lsn()).unwrap();
        let f = vfs.open(&segment_path(1), OpenMode::ReadWrite).unwrap();
        f.write_all_at(&5u32.to_le_bytes(), a.seg_offset(MIN_WAL_SEGMENT_SIZE))
            .unwrap();
        let mut r = WalReader::open(vfs.clone(), &cfg(), a);
        assert!(r.next().unwrap().is_none());
        assert_eq!(
            r.end_of_wal().unwrap().1,
            EndReason::BadHeader(RecordError::BadLength)
        );

        f.write_all_at(
            &(2u32 << 20).to_le_bytes(),
            a.seg_offset(MIN_WAL_SEGMENT_SIZE),
        )
        .unwrap();
        let mut r = WalReader::open(vfs.clone(), &cfg(), a);
        assert!(r.next().unwrap().is_none());
        assert_eq!(r.end_of_wal().unwrap().1, EndReason::TooLong);

        // セグメント末尾をまたぐ長さ
        f.write_all_at(
            &(1u32 << 19).to_le_bytes(),
            u64::from(MIN_WAL_SEGMENT_SIZE) - 64,
        )
        .unwrap();
        let tail = Lsn(u64::from(MIN_WAL_SEGMENT_SIZE) * 2 - 64);
        let mut r = WalReader::open(vfs.clone(), &cfg(), tail);
        assert!(r.next().unwrap().is_none());
        assert_eq!(r.end_of_wal(), Some((tail, EndReason::CrossesSegment)));

        f.write_all_at(&[0xFF], 20).unwrap();
        let mut r = WalReader::open(vfs, &cfg(), a);
        assert!(r.next().unwrap().is_none());
        assert_eq!(r.end_of_wal(), Some((a, EndReason::BadSegmentHeader(1))));
    }

    #[test]
    fn foreign_cluster_segment_is_rejected() {
        let (vfs, wal) = setup();
        let a = noop(&wal, 40);
        wal.flush(wal.insert_lsn()).unwrap();
        let mut other = cfg();
        other.system_identifier = 8;
        let mut r = WalReader::open(vfs, &other, a);
        assert!(r.next().unwrap().is_none());
        assert_eq!(r.end_of_wal().unwrap().1, EndReason::BadSegmentHeader(1));
    }

    /// 壊れた flush のあとに、古い（有効な CRC の）レコードが新しいレコードのすぐ後ろに残る場合:
    /// 古いレコードの prev は新しい最終レコードの開始と食い違うので、読み取りはそこで終わる。
    #[test]
    #[allow(clippy::many_single_char_names)]
    fn stale_valid_record_after_a_rewritten_tail_is_a_prev_mismatch() {
        let (vfs, wal) = setup();
        let a = noop(&wal, 40);
        let b = noop(&wal, 40);
        let c = noop(&wal, 40);
        let d = noop(&wal, 40);
        wal.flush(wal.insert_lsn()).unwrap();
        let seg = MIN_WAL_SEGMENT_SIZE;

        // 別のディスクで、a のあとに b から d までを 1 つで埋める長さのレコード b' を作る。
        let want = d.0 - b.0;
        let mut found = None;
        for len in 0..400usize {
            let (v2, w2) = setup();
            let a2 = noop(&w2, 40);
            let b2 = noop(&w2, len);
            w2.flush(w2.insert_lsn()).unwrap();
            if w2.insert_lsn().0 - b2.0 == want {
                assert_eq!((a2, b2), (a, b));
                found = Some(v2);
                break;
            }
        }
        let v2 = found.expect("a record of the wanted length exists");
        let src = v2.open(&segment_path(1), OpenMode::ReadOnly).unwrap();
        let mut bytes = vec![0u8; usize::try_from(want).unwrap()];
        src.read_exact_at(&mut bytes, b.seg_offset(seg)).unwrap();
        let dst = vfs.open(&segment_path(1), OpenMode::ReadWrite).unwrap();
        dst.write_all_at(&bytes, b.seg_offset(seg)).unwrap();

        let mut r = WalReader::open(vfs, &cfg(), a);
        assert_eq!(r.next().unwrap().unwrap().start, a);
        let rewritten = r.next().unwrap().unwrap();
        assert_eq!(rewritten.start, b);
        assert_eq!(rewritten.end, d);
        // d は CRC の正しい古いレコード（prev は c）。
        assert!(r.next().unwrap().is_none());
        assert_eq!(r.end_of_wal(), Some((d, EndReason::PrevMismatch)));
        assert_eq!(r.last_record_start(), Some(b));
        let _ = c;
    }
    #[test]
    fn start_position_is_normalized() {
        let (vfs, wal) = setup();
        let a = noop(&wal, 40);
        wal.flush(wal.insert_lsn()).unwrap();
        let mut r = WalReader::open(vfs, &cfg(), Lsn(u64::from(MIN_WAL_SEGMENT_SIZE)));
        assert_eq!(r.next().unwrap().unwrap().start, a);
    }
}
