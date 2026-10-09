//! HEAP2 rmgr: `PRUNE_FREEZE` / `VACUUM_UNUSED` / `INPLACE` の形式と REDO
//! （`m5/03` §3.5、VC-1）。
//!
//! `recovery::dispatch` が `RmgrId::Heap2` で [`redo`] を呼ぶ。`info`: `PRUNE_FREEZE` 0x00、
//! `VACUUM_UNUSED` 0x10、`MULTI_INSERT` 0x20（予約。未実装）、`INPLACE` 0x30（00 §3.3）。
//! すべて `xid = INVALID`。画像（FPI）が付いたブロックは REDO が画像を復元するだけで、
//! そのレコードのメインデータもブロックデータも使わない。

use super::prune::{FreezeEntry, PrunePlan, apply_plan};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::buffer::{BufferPool, CriticalSection, PageWriteGuard};
use crate::storage::page::{LpFlags, Page};
use crate::storage::smgr::{BlockNumber, BufferTag, ForkNumber, RelFileLocator};
use crate::txn::Xid;
use crate::wal::{
    DecodedRecord, Lsn, RecordBuilder, RedoBuffer, RedoCtx, RegFlags, RmgrId, Wal,
    read_buffer_for_redo,
};

pub const HEAP2_PRUNE_FREEZE: u8 = 0x00;
pub const HEAP2_VACUUM_UNUSED: u8 = 0x10;
pub const HEAP2_MULTI_INSERT: u8 = 0x20;
pub const HEAP2_INPLACE: u8 = 0x30;

/// `PRUNE_FREEZE` のメインデータの長さ。
pub const PRUNE_MAIN_LEN: usize = 16;
/// `VACUUM_UNUSED` のメインデータの長さ。
pub const VACUUM_UNUSED_MAIN_LEN: usize = 8;
/// `INPLACE` のメインデータのヘッダの長さ。
pub const INPLACE_HEADER_LEN: usize = 8;
const FREEZE_ENTRY_LEN: usize = 8;
const FREEZE_CLEAR_XMAX: u16 = 0x0001;

/// ディスク上のレコードとページの食い違い。リカバリは続けられないので PANIC。
fn corrupt(what: &str) -> Error {
    Error::new(
        sqlstate::DATA_CORRUPTED,
        format!("invalid heap2 WAL record: {what}"),
    )
    .with_severity(Severity::Panic)
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn len16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

// ---------------------------------------------------------------------------
// PRUNE_FREEZE
// ---------------------------------------------------------------------------

/// `PRUNE_FREEZE` のメインデータ（16 バイト）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PruneMain {
    pub ndead: u16,
    pub nunused: u16,
    pub nfrozen: u16,
    /// 予約（0）。
    pub flags: u16,
    /// 適用後の `pd_prune_xid`（0 = なし）。
    pub prune_xid: u32,
}

impl PruneMain {
    pub fn of(plan: &PrunePlan) -> PruneMain {
        PruneMain {
            ndead: len16(plan.dead.len()),
            nunused: len16(plan.unused.len()),
            nfrozen: len16(plan.frozen.len()),
            flags: 0,
            prune_xid: plan.prune_xid,
        }
    }

    pub fn encode(&self) -> [u8; PRUNE_MAIN_LEN] {
        let mut b = [0u8; PRUNE_MAIN_LEN];
        b[0..2].copy_from_slice(&self.ndead.to_le_bytes());
        b[2..4].copy_from_slice(&self.nunused.to_le_bytes());
        b[4..6].copy_from_slice(&self.nfrozen.to_le_bytes());
        b[6..8].copy_from_slice(&self.flags.to_le_bytes());
        b[8..12].copy_from_slice(&self.prune_xid.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<PruneMain> {
        if b.len() != PRUNE_MAIN_LEN {
            return Err(corrupt("PRUNE_FREEZE main data has a bad length"));
        }
        Ok(PruneMain {
            ndead: u16_at(b, 0),
            nunused: u16_at(b, 2),
            nfrozen: u16_at(b, 4),
            flags: u16_at(b, 6),
            prune_xid: u32_at(b, 8),
        })
    }

    /// ブロックデータの長さ。
    pub fn data_len(&self) -> usize {
        2 * (usize::from(self.ndead) + usize::from(self.nunused))
            + FREEZE_ENTRY_LEN * usize::from(self.nfrozen)
    }
}

/// `PRUNE_FREEZE` のブロックデータ: `dead`、`unused`（`u16` の昇順）、`frozen`（8 バイトずつ）。
pub fn encode_plan_data(plan: &PrunePlan) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        2 * (plan.dead.len() + plan.unused.len()) + FREEZE_ENTRY_LEN * plan.frozen.len(),
    );
    for off in plan.dead.iter().chain(&plan.unused) {
        out.extend_from_slice(&off.to_le_bytes());
    }
    for e in &plan.frozen {
        out.extend_from_slice(&e.offnum.to_le_bytes());
        out.extend_from_slice(&e.infomask.to_le_bytes());
        out.extend_from_slice(&e.infomask2.to_le_bytes());
        let flags = if e.clear_xmax { FREEZE_CLEAR_XMAX } else { 0 };
        out.extend_from_slice(&flags.to_le_bytes());
    }
    out
}

fn ascending(v: impl Iterator<Item = u16>) -> bool {
    let mut prev = 0u16;
    for x in v {
        if x == 0 || x <= prev {
            return false;
        }
        prev = x;
    }
    true
}

/// 長さと並びの検査つきの復号。食い違えば PANIC 級の `XX001`。
pub fn decode_plan(main: &PruneMain, data: &[u8]) -> Result<PrunePlan> {
    if data.len() != main.data_len() {
        return Err(corrupt(&format!(
            "PRUNE_FREEZE block data is {} bytes, the main data says {}",
            data.len(),
            main.data_len()
        )));
    }
    if main.flags != 0 {
        return Err(corrupt("PRUNE_FREEZE has unknown flags"));
    }
    let nd = usize::from(main.ndead);
    let nu = usize::from(main.nunused);
    let offs: Vec<u16> = (0..nd + nu).map(|i| u16_at(data, 2 * i)).collect();
    let (dead, unused) = offs.split_at(nd);
    let base = 2 * (nd + nu);
    let mut frozen = Vec::with_capacity(usize::from(main.nfrozen));
    for i in 0..usize::from(main.nfrozen) {
        let o = base + FREEZE_ENTRY_LEN * i;
        let flags = u16_at(data, o + 6);
        if flags & !FREEZE_CLEAR_XMAX != 0 {
            return Err(corrupt("a freeze entry has unknown flags"));
        }
        frozen.push(FreezeEntry {
            offnum: u16_at(data, o),
            infomask: u16_at(data, o + 2),
            infomask2: u16_at(data, o + 4),
            clear_xmax: flags & FREEZE_CLEAR_XMAX != 0,
        });
    }
    if !ascending(dead.iter().copied())
        || !ascending(unused.iter().copied())
        || !ascending(frozen.iter().map(|e| e.offnum))
    {
        return Err(corrupt("PRUNE_FREEZE offsets are not strictly ascending"));
    }
    if dead.iter().any(|d| unused.contains(d)) {
        return Err(corrupt("PRUNE_FREEZE dead and unused offsets overlap"));
    }
    Ok(PrunePlan {
        dead: dead.to_vec(),
        unused: unused.to_vec(),
        frozen,
        prune_xid: main.prune_xid,
        base_prune_xid: 0,
    })
}

// ---------------------------------------------------------------------------
// VACUUM_UNUSED
// ---------------------------------------------------------------------------

/// `VACUUM_UNUSED` のメインデータ（8 バイト）。
pub fn encode_vacuum_unused_main(nunused: usize) -> [u8; VACUUM_UNUSED_MAIN_LEN] {
    let mut b = [0u8; VACUUM_UNUSED_MAIN_LEN];
    b[0..2].copy_from_slice(&len16(nunused).to_le_bytes());
    b
}

/// `VACUUM_UNUSED` のブロックデータ（`u16` の昇順）。
pub fn encode_vacuum_unused_data(offsets: &[u16]) -> Vec<u8> {
    offsets.iter().flat_map(|o| o.to_le_bytes()).collect()
}

fn decode_vacuum_unused(main: &[u8], data: &[u8]) -> Result<Vec<u16>> {
    if main.len() != VACUUM_UNUSED_MAIN_LEN {
        return Err(corrupt("VACUUM_UNUSED main data has a bad length"));
    }
    let n = usize::from(u16_at(main, 0));
    if u16_at(main, 2) != 0 || data.len() != 2 * n {
        return Err(corrupt(
            "VACUUM_UNUSED block data does not match the main data",
        ));
    }
    let offs: Vec<u16> = (0..n).map(|i| u16_at(data, 2 * i)).collect();
    if !ascending(offs.iter().copied()) {
        return Err(corrupt("VACUUM_UNUSED offsets are not strictly ascending"));
    }
    Ok(offs)
}

/// 第 3 段のページ変更: 各行ポインタを `LP_UNUSED` にし、末尾の `LP_UNUSED` を外す
/// （`finish_unused`。タプルは動かさない）。`pd_prune_xid` は変えない。通常の経路と REDO が同じ関数を使う。
///
/// `LP_DEAD` でないもの（`LP_NORMAL` ほか）はエラー。`LP_UNUSED`（二度目の REDO で、末尾が
/// 外れて範囲外になったものを含む）は冪等として許す。
pub fn apply_vacuum_unused(page: &mut Page, offsets: &[u16]) -> Result<()> {
    for &off in offsets {
        if off > page.max_offset() {
            continue;
        }
        let id = page
            .item_id(off)
            .map_err(|e| corrupt(&format!("line pointer {off}: {e}")))?;
        match id.flags {
            LpFlags::Dead | LpFlags::Unused => page.mark_unused(off),
            f => {
                return Err(corrupt(&format!(
                    "VACUUM_UNUSED names line pointer {off}, which is {f:?}"
                )));
            }
        }
    }
    page.finish_unused();
    Ok(())
}

/// `VACUUM_UNUSED` を適用して WAL に書く（排他ラッチを持った呼び出し側が呼ぶ）。
/// 呼び出し側が全部 `LP_DEAD` であることを確かめてから呼ぶ。
pub fn log_vacuum_unused(
    pool: &BufferPool,
    wal: &Wal,
    rel: RelFileLocator,
    blk: BlockNumber,
    guard: &mut PageWriteGuard<'_>,
    offsets: &[u16],
) -> Result<Lsn> {
    let tag = BufferTag {
        rel,
        fork: ForkNumber::Main,
        block: blk,
    };
    let cs = CriticalSection::enter(pool);
    apply_vacuum_unused(guard.page_mut(), offsets).map_err(|e| cs.escalate(e))?;
    let end = {
        let mut rec = RecordBuilder::new(RmgrId::Heap2, HEAP2_VACUUM_UNUSED, Xid::INVALID);
        let b = rec.register_block(tag, guard.page(), RegFlags::STANDARD);
        rec.block_data(b, &encode_vacuum_unused_data(offsets));
        rec.main_data(&encode_vacuum_unused_main(offsets.len()));
        wal.insert(rec).map_err(|e| cs.escalate(e))?.end
    };
    guard.set_lsn(end.0);
    Ok(end)
}

// ---------------------------------------------------------------------------
// INPLACE
// ---------------------------------------------------------------------------

/// その場の上書きの 1 か所。`byte_off` はタプルの先頭（`t_xmin` の位置）からのバイト数。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InplaceBytes {
    pub byte_off: u16,
    pub bytes: Vec<u8>,
}

/// `INPLACE` のメインデータ: 8 バイトのヘッダ（`offnum u16`、`npatch u16`、予約 `u32`）の後に
/// `npatch` 個の `{ byte_off u16, len u16, bytes[len] }`（パディングなし）。
pub fn encode_inplace_main(offnum: u16, patches: &[InplaceBytes]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        INPLACE_HEADER_LEN + patches.iter().map(|p| 4 + p.bytes.len()).sum::<usize>(),
    );
    out.extend_from_slice(&offnum.to_le_bytes());
    out.extend_from_slice(&len16(patches.len()).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    for p in patches {
        out.extend_from_slice(&p.byte_off.to_le_bytes());
        out.extend_from_slice(&len16(p.bytes.len()).to_le_bytes());
        out.extend_from_slice(&p.bytes);
    }
    out
}

pub fn decode_inplace_main(b: &[u8]) -> Result<(u16, Vec<InplaceBytes>)> {
    if b.len() < INPLACE_HEADER_LEN || u32_at(b, 4) != 0 {
        return Err(corrupt("INPLACE main data has a bad header"));
    }
    let offnum = u16_at(b, 0);
    let npatch = usize::from(u16_at(b, 2));
    let mut pos = INPLACE_HEADER_LEN;
    let mut patches = Vec::with_capacity(npatch);
    for _ in 0..npatch {
        if pos + 4 > b.len() {
            return Err(corrupt("INPLACE patch header is truncated"));
        }
        let byte_off = u16_at(b, pos);
        let len = usize::from(u16_at(b, pos + 2));
        pos += 4;
        if pos + len > b.len() {
            return Err(corrupt("INPLACE patch bytes are truncated"));
        }
        patches.push(InplaceBytes {
            byte_off,
            bytes: b[pos..pos + len].to_vec(),
        });
        pos += len;
    }
    if pos != b.len() {
        return Err(corrupt("INPLACE main data has trailing bytes"));
    }
    Ok((offnum, patches))
}

/// タプルの中のバイトを書き換える。通常の経路と REDO が同じ関数を使う。`offnum` が `LP_NORMAL`
/// でなければ、またはパッチがタプルの外に出ればエラー。
pub fn apply_inplace(page: &mut Page, offnum: u16, patches: &[InplaceBytes]) -> Result<()> {
    let id = page
        .item_id(offnum)
        .map_err(|e| corrupt(&format!("line pointer {offnum}: {e}")))?;
    if id.flags != LpFlags::Normal {
        return Err(corrupt(&format!(
            "INPLACE names line pointer {offnum}, which is {:?}",
            id.flags
        )));
    }
    let item = page
        .item_mut(offnum)
        .map_err(|e| corrupt(&format!("line pointer {offnum}: {e}")))?;
    for p in patches {
        let start = usize::from(p.byte_off);
        let end = start + p.bytes.len();
        if end > item.len() {
            return Err(corrupt("INPLACE patch lies outside the tuple"));
        }
        item[start..end].copy_from_slice(&p.bytes);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// REDO
// ---------------------------------------------------------------------------

/// `f` を、REDO が要るブロック `id` に適用し、ページの LSN を設定する。失敗は PANIC。
fn redo_block(
    ctx: &RedoCtx,
    rec: &DecodedRecord,
    id: u8,
    f: impl FnOnce(&mut Page) -> Result<()>,
) -> Result<()> {
    if let RedoBuffer::NeedsRedo(buf) = read_buffer_for_redo(ctx, rec, id)? {
        let mut g = buf.write()?;
        if let Err(e) = f(g.page_mut()) {
            // 起動が止まりページは書かれないが、ガードの「page_mut の後は set_lsn」検査は黙らせる。
            let lsn = g.page().lsn();
            g.set_lsn(lsn);
            return Err(e.with_severity(Severity::Panic));
        }
        g.set_lsn(rec.end.0);
    }
    Ok(())
}

fn check_shape(rec: &DecodedRecord, main_len: Option<usize>) -> Result<()> {
    if rec.blocks.len() != 1 {
        return Err(corrupt(&format!(
            "{} block references at {}",
            rec.blocks.len(),
            rec.start
        )));
    }
    if let Some(n) = main_len
        && rec.main.len() != n
    {
        return Err(corrupt(&format!("main data of {} bytes", rec.main.len())));
    }
    if rec.blocks[0].will_init {
        return Err(corrupt("WILL_INIT is not used by HEAP2"));
    }
    Ok(())
}

/// HEAP2 rmgr の REDO。失敗を飲み込まない。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    if rec.rmgr != RmgrId::Heap2 {
        return Err(corrupt("HEAP2 redo called for a record of another rmgr"));
    }
    match rec.info {
        HEAP2_PRUNE_FREEZE => {
            check_shape(rec, Some(PRUNE_MAIN_LEN))?;
            let main = PruneMain::decode(&rec.main)?;
            redo_block(ctx, rec, 0, |p| {
                // 画像が付いたブロックは復元されるだけで、ここには来ない。
                let plan = decode_plan(&main, &rec.blocks[0].data)?;
                apply_plan(p, &plan)
            })
        }
        HEAP2_VACUUM_UNUSED => {
            check_shape(rec, Some(VACUUM_UNUSED_MAIN_LEN))?;
            redo_block(ctx, rec, 0, |p| {
                let offs = decode_vacuum_unused(&rec.main, &rec.blocks[0].data)?;
                apply_vacuum_unused(p, &offs)
            })
        }
        HEAP2_INPLACE => {
            check_shape(rec, None)?;
            let (offnum, patches) = decode_inplace_main(&rec.main)?;
            redo_block(ctx, rec, 0, |p| apply_inplace(p, offnum, &patches))
        }
        HEAP2_MULTI_INSERT => Err(corrupt("MULTI_INSERT is not implemented yet")),
        k => Err(corrupt(&format!("unknown HEAP2 info {k:#x}"))),
    }
}

/// `yuzhu-waldump` 用の 1 行の説明。
pub fn describe(rec: &DecodedRecord) -> String {
    match rec.info {
        HEAP2_PRUNE_FREEZE => match PruneMain::decode(&rec.main) {
            Ok(m) => format!(
                "PRUNE_FREEZE ndead: {}, nunused: {}, nfrozen: {}, prune_xid: {}",
                m.ndead, m.nunused, m.nfrozen, m.prune_xid
            ),
            Err(_) => "PRUNE_FREEZE (malformed)".to_string(),
        },
        HEAP2_VACUUM_UNUSED => match decode_vacuum_unused(
            &rec.main,
            rec.blocks.first().map_or(&[][..], |b| &b.data[..]),
        ) {
            Ok(o) => format!("VACUUM_UNUSED nunused: {}", o.len()),
            // 画像が付くとブロックデータは省かれる。
            Err(_) if rec.main.len() == VACUUM_UNUSED_MAIN_LEN => {
                format!("VACUUM_UNUSED nunused: {}", u16_at(&rec.main, 0))
            }
            Err(_) => "VACUUM_UNUSED (malformed)".to_string(),
        },
        HEAP2_INPLACE => match decode_inplace_main(&rec.main) {
            Ok((off, p)) => format!("INPLACE off: {off}, npatch: {}", p.len()),
            Err(_) => "INPLACE (malformed)".to_string(),
        },
        HEAP2_MULTI_INSERT => "MULTI_INSERT".to_string(),
        k => format!("UNKNOWN 0x{k:02X}"),
    }
}
