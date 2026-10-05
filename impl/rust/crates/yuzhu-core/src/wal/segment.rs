//! WAL セグメントのファイル名、ヘッダ、作成（0 埋め + rename）、削除
//! （`m3.md` §3.1、§3.2）。LSN の位置計算（`seg_end`、`normalize`）もここに置く。

#![allow(clippy::cast_possible_truncation, clippy::doc_markdown)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Lsn, RECORD_HEADER_SIZE, SEG_HEADER_SIZE, WAL_FORMAT_VERSION, WalConfig};
use crate::error::{Error, Result};
use crate::storage::vfs::{OpenMode, Vfs, VfsFile};
use crate::util::crc32c::crc32c;

pub const WAL_DIR: &str = "pg_wal";
pub const SEG_MAGIC: [u8; 4] = *b"YZWL";
const ZERO_CHUNK: usize = 1 << 20;

// ----- 位置の計算 -------------------------------------------------------------

/// 8 バイト境界に切り上げる。
pub fn align8(n: u64) -> u64 {
    (n + 7) & !7
}

/// セグメント `segno` の先頭 LSN。
pub fn seg_start(segno: u64, seg_size: u32) -> Lsn {
    Lsn(segno * u64::from(seg_size))
}

/// `lsn` を含むセグメントの末尾（排他的）。
pub fn seg_end(lsn: Lsn, seg_size: u32) -> u64 {
    (lsn.segno(seg_size) + 1) * u64::from(seg_size)
}

/// 次のレコードを置ける位置にする。セグメント末尾までの残りが `RECORD_HEADER_SIZE` 未満、
/// またはヘッダ領域の中なら、次（または同じ）セグメントのヘッダの直後。
pub fn normalize(lsn: Lsn, seg_size: u32) -> Lsn {
    if seg_end(lsn, seg_size) - lsn.0 < RECORD_HEADER_SIZE as u64 {
        Lsn(seg_end(lsn, seg_size) + SEG_HEADER_SIZE)
    } else if lsn.seg_offset(seg_size) < SEG_HEADER_SIZE {
        Lsn(seg_start(lsn.segno(seg_size), seg_size).0 + SEG_HEADER_SIZE)
    } else {
        lsn
    }
}

/// `lsn` を含むセグメントの次のセグメントの、最初のレコード位置。
pub fn next_segment_start(lsn: Lsn, seg_size: u32) -> Lsn {
    Lsn(seg_end(lsn, seg_size) + SEG_HEADER_SIZE)
}

// ----- ファイル名 -------------------------------------------------------------

/// 大文字 16 進 16 桁。
pub fn segment_file_name(segno: u64) -> String {
    format!("{segno:016X}")
}

pub fn segment_path(segno: u64) -> PathBuf {
    Path::new(WAL_DIR).join(segment_file_name(segno))
}

pub fn temp_path(segno: u64) -> PathBuf {
    Path::new(WAL_DIR).join(format!("xlogtemp.{segno}"))
}

/// ファイル名から番号を得る。セグメントの名前でなければ `None`。
pub fn parse_segment_name(name: &str) -> Option<u64> {
    if name.len() != 16
        || !name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
    {
        return None;
    }
    u64::from_str_radix(name, 16).ok()
}

// ----- ヘッダ -----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegHeader {
    pub system_identifier: u64,
    pub segno: u64,
    pub segment_size: u32,
}

pub fn encode_header(h: &SegHeader) -> [u8; SEG_HEADER_SIZE as usize] {
    let mut b = [0u8; SEG_HEADER_SIZE as usize];
    b[0..4].copy_from_slice(&SEG_MAGIC);
    b[4..6].copy_from_slice(&WAL_FORMAT_VERSION.to_le_bytes());
    // 6..8 flags = 0
    b[8..16].copy_from_slice(&h.system_identifier.to_le_bytes());
    b[16..24].copy_from_slice(&h.segno.to_le_bytes());
    b[24..28].copy_from_slice(&h.segment_size.to_le_bytes());
    let crc = crc32c(&b[..28]);
    b[28..32].copy_from_slice(&crc.to_le_bytes());
    b
}

/// マジック・バージョン・フラグ・CRC を検査してデコードする。
pub fn decode_header(b: &[u8]) -> Option<SegHeader> {
    if b.len() < SEG_HEADER_SIZE as usize || b[0..4] != SEG_MAGIC {
        return None;
    }
    let version = u16::from_le_bytes([b[4], b[5]]);
    let flags = u16::from_le_bytes([b[6], b[7]]);
    let crc = u32::from_le_bytes(b[28..32].try_into().ok()?);
    if version != WAL_FORMAT_VERSION || flags != 0 || crc != crc32c(&b[..28]) {
        return None;
    }
    Some(SegHeader {
        system_identifier: u64::from_le_bytes(b[8..16].try_into().ok()?),
        segno: u64::from_le_bytes(b[16..24].try_into().ok()?),
        segment_size: u32::from_le_bytes(b[24..28].try_into().ok()?),
    })
}

/// ヘッダが `segno` のこのクラスタのものとして正しいか。
pub fn header_is_valid(b: &[u8], cfg: &WalConfig, segno: u64) -> bool {
    decode_header(b)
        == Some(SegHeader {
            system_identifier: cfg.system_identifier,
            segno,
            segment_size: cfg.segment_size,
        })
}

// ----- 作成・オープン・削除 -----------------------------------------------------

/// セグメントを開く（なければ `NotFound`）。
pub fn open_segment(vfs: &dyn Vfs, segno: u64, mode: OpenMode) -> io::Result<Arc<dyn VfsFile>> {
    vfs.open(&segment_path(segno), mode)
}

pub fn segment_exists(vfs: &dyn Vfs, segno: u64) -> io::Result<bool> {
    vfs.exists(&segment_path(segno))
}

/// §3.2 の手順でセグメントを作り、読み書きで開いて返す。すでに正式な名前のファイルが
/// あれば何もせずそれを開く（呼び出し側は存在しないと分かっているときだけ使う）。
pub fn create_segment(vfs: &dyn Vfs, cfg: &WalConfig, segno: u64) -> Result<Arc<dyn VfsFile>> {
    let io_err = |e: io::Error, what: &str| {
        Error::from_io(
            &e,
            format!("could not {what} WAL segment {}", segment_file_name(segno)),
        )
    };
    let tmp = temp_path(segno);
    let dst = segment_path(segno);
    if vfs.exists(&tmp).map_err(|e| io_err(e, "stat"))? {
        vfs.remove_file(&tmp).map_err(|e| io_err(e, "remove"))?;
    }
    let f = vfs
        .open(&tmp, OpenMode::CreateNew)
        .map_err(|e| io_err(e, "create"))?;
    let zeros = vec![0u8; ZERO_CHUNK.min(cfg.segment_size as usize)];
    let total = u64::from(cfg.segment_size);
    let mut off = 0u64;
    while off < total {
        let n = (total - off).min(zeros.len() as u64) as usize;
        f.write_all_at(&zeros[..n], off)
            .map_err(|e| io_err(e, "write"))?;
        off += n as u64;
    }
    let header = encode_header(&SegHeader {
        system_identifier: cfg.system_identifier,
        segno,
        segment_size: cfg.segment_size,
    });
    f.write_all_at(&header, 0).map_err(|e| io_err(e, "write"))?;
    f.sync_all().map_err(|e| io_err(e, "fsync"))?;
    vfs.rename(&tmp, &dst).map_err(|e| io_err(e, "rename"))?;
    vfs.sync_dir(Path::new(WAL_DIR))
        .map_err(|e| io_err(e, "fsync directory of"))?;
    Ok(f)
}

/// `pg_wal/` にあるセグメントの番号（昇順）。
pub fn list_segments(vfs: &dyn Vfs) -> Result<Vec<u64>> {
    let entries = vfs
        .read_dir(Path::new(WAL_DIR))
        .map_err(|e| Error::from_io(&e, "could not read WAL directory"))?;
    let mut out: Vec<u64> = entries
        .iter()
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(parse_segment_name)
        })
        .collect();
    out.sort_unstable();
    Ok(out)
}

/// 起動時: 作成途中の `xlogtemp.*` を消す。
pub fn remove_temp_files(vfs: &dyn Vfs) -> Result<()> {
    let entries = vfs
        .read_dir(Path::new(WAL_DIR))
        .map_err(|e| Error::from_io(&e, "could not read WAL directory"))?;
    for p in entries {
        let is_tmp = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("xlogtemp."));
        if is_tmp {
            vfs.remove_file(&p)
                .map_err(|e| Error::from_io(&e, "could not remove temporary WAL file"))?;
        }
    }
    Ok(())
}

/// 番号が `segno` より小さいセグメントを消し、`sync_dir` する。個々の失敗は無視する
/// （正しさに影響しない。WARNING 相当）。消した個数を返す。
pub fn remove_segments_before(vfs: &dyn Vfs, segno: u64) -> Result<usize> {
    let mut removed = 0;
    for n in list_segments(vfs)? {
        if n < segno && vfs.remove_file(&segment_path(n)).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        let _ = vfs.sync_dir(Path::new(WAL_DIR));
    }
    Ok(removed)
}

/// `segno` より大きいセグメントをすべて消し、`sync_dir` する（リカバリ終了時。失敗はエラー）。
pub fn remove_segments_after(vfs: &dyn Vfs, segno: u64) -> Result<usize> {
    let mut removed = 0;
    for n in list_segments(vfs)? {
        if n > segno {
            vfs.remove_file(&segment_path(n))
                .map_err(|e| Error::from_io(&e, "could not remove WAL segment"))?;
            removed += 1;
        }
    }
    if removed > 0 {
        vfs.sync_dir(Path::new(WAL_DIR))
            .map_err(|e| Error::from_io(&e, "could not fsync WAL directory"))?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::vfs::SimVfs;

    const SEG: u32 = super::super::MIN_WAL_SEGMENT_SIZE;

    fn cfg() -> WalConfig {
        WalConfig {
            segment_size: SEG,
            system_identifier: 42,
            full_page_writes: true,
            knobs: DebugKnobs::default(),
        }
    }

    fn vfs() -> SimVfs {
        let v = SimVfs::new(1);
        v.create_dir_all(Path::new(WAL_DIR)).unwrap();
        v
    }

    #[test]
    fn names_round_trip() {
        assert_eq!(segment_file_name(1), "0000000000000001");
        assert_eq!(segment_file_name(0xAB), "00000000000000AB");
        assert_eq!(parse_segment_name("00000000000000AB"), Some(0xAB));
        assert_eq!(parse_segment_name("00000000000000ab"), None);
        assert_eq!(parse_segment_name("xlogtemp.3"), None);
        assert_eq!(parse_segment_name("0000000000000001x"), None);
    }

    #[test]
    fn header_round_trip_and_checks() {
        let h = SegHeader {
            system_identifier: 42,
            segno: 7,
            segment_size: SEG,
        };
        let b = encode_header(&h);
        assert_eq!(decode_header(&b), Some(h));
        assert!(header_is_valid(&b, &cfg(), 7));
        assert!(!header_is_valid(&b, &cfg(), 8));
        let mut other = cfg();
        other.system_identifier = 43;
        assert!(!header_is_valid(&b, &other, 7));
        for i in 0..32 {
            let mut c = b;
            c[i] ^= 1;
            assert!(!header_is_valid(&c, &cfg(), 7), "byte {i}");
        }
    }

    #[test]
    fn normalize_skips_short_tails_and_headers() {
        let s = u64::from(SEG);
        assert_eq!(normalize(Lsn(s + 32), SEG), Lsn(s + 32));
        assert_eq!(normalize(Lsn(2 * s), SEG), Lsn(2 * s + 32));
        assert_eq!(normalize(Lsn(2 * s - 32), SEG), Lsn(2 * s - 32));
        assert_eq!(normalize(Lsn(2 * s - 24), SEG), Lsn(2 * s + 32));
        assert_eq!(normalize(Lsn(s), SEG), Lsn(s + 32));
        assert_eq!(seg_end(Lsn(s + 100), SEG), 2 * s);
        assert_eq!(next_segment_start(Lsn(s + 100), SEG), Lsn(2 * s + 32));
        assert_eq!(align8(111), 112);
        assert_eq!(align8(112), 112);
    }

    #[test]
    fn create_writes_zero_filled_segment_with_header() {
        let v = vfs();
        let f = create_segment(&v, &cfg(), 1).unwrap();
        assert_eq!(f.size().unwrap(), u64::from(SEG));
        let mut b = vec![0u8; 64];
        f.read_exact_at(&mut b, 0).unwrap();
        assert!(header_is_valid(&b, &cfg(), 1));
        assert!(b[32..].iter().all(|&x| x == 0));
        assert!(!v.exists(&temp_path(1)).unwrap());
        assert_eq!(list_segments(&v).unwrap(), vec![1]);
    }

    #[test]
    fn create_replaces_a_stale_temp_file() {
        let v = vfs();
        v.open(&temp_path(2), OpenMode::CreateNew).unwrap();
        create_segment(&v, &cfg(), 2).unwrap();
        assert_eq!(list_segments(&v).unwrap(), vec![2]);
    }

    #[test]
    fn remove_before_after_and_temp() {
        let v = vfs();
        for n in 1..=4 {
            create_segment(&v, &cfg(), n).unwrap();
        }
        v.open(&temp_path(9), OpenMode::CreateNew).unwrap();
        remove_temp_files(&v).unwrap();
        assert!(!v.exists(&temp_path(9)).unwrap());
        assert_eq!(remove_segments_before(&v, 3).unwrap(), 2);
        assert_eq!(list_segments(&v).unwrap(), vec![3, 4]);
        assert_eq!(remove_segments_after(&v, 3).unwrap(), 1);
        assert_eq!(list_segments(&v).unwrap(), vec![3]);
    }

    #[test]
    fn crash_after_create_keeps_segment() {
        let v = vfs();
        v.sync_dir(Path::new("")).unwrap();
        create_segment(&v, &cfg(), 1).unwrap();
        let after = v.crash(crate::storage::vfs::CrashMode::DropUnsynced);
        assert_eq!(list_segments(&after).unwrap(), vec![1]);
    }
}
