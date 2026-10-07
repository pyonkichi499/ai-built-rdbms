//! BTREE rmgr: `BTREE_INSERT_LEAF` / `BTREE_PAGES` の WAL、REDO、`describe`
//! （`m4/06-btree.md` §3.7、§5.11）。
//!
//! `BTREE_PAGES`（B1a）: 全ページの全画像（`FORCE_IMAGE`）を 1 レコードで書く。
//! `BTREE_INSERT_LEAF` の書き出し・REDO は B1b が `redo_insert_leaf` と `log_insert_leaf` を埋める。

use super::page::BtSpecial;
use super::{INDEX_SIZE_MASK, INDEX_TUPLE_HEADER_SIZE};
use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::WriteCtx;
use crate::storage::buffer::PageWriteGuard;
use crate::storage::page::Page;
use crate::storage::smgr::BufferTag;
use crate::wal::{
    DecodedRecord, Lsn, MAX_BLOCK_REFS, RecordBuilder, RedoBuffer, RedoCtx, RegFlags, RmgrId, Wal,
    read_buffer_for_redo,
};

/// 葉への 1 項目の挿入（差分。通常の FPW の対象）。
pub const BTREE_INSERT_LEAF: u8 = 0x00;
/// 構造変更・初期化・一括構築: 全ページの全画像（`FORCE_IMAGE`）。理由は main data の先頭 1 バイト。
pub const BTREE_PAGES: u8 = 0x10;
// 0x20 以降は M5 の予約（DELETE、VACUUM、UNLINK_PAGE など）。

/// `BTREE_PAGES` の `reason`（06 §3.7）。
pub const REASON_INIT: u8 = 1;
pub const REASON_SPLIT: u8 = 2;
pub const REASON_BUILD: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PagesReason {
    /// ちょうど 2 ブロック: メタ（0）、空のルート葉（1）。
    Init = REASON_INIT,
    Split = REASON_SPLIT,
    Build = REASON_BUILD,
}

impl PagesReason {
    pub fn from_u8(v: u8) -> Option<PagesReason> {
        match v {
            REASON_INIT => Some(PagesReason::Init),
            REASON_SPLIT => Some(PagesReason::Split),
            REASON_BUILD => Some(PagesReason::Build),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            PagesReason::Init => "INIT",
            PagesReason::Split => "SPLIT",
            PagesReason::Build => "BUILD",
        }
    }
}

/// `BTREE_PAGES` のメインデータ（4 バイト: `reason`、予約 3 バイト）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PagesMain {
    pub reason: PagesReason,
}

fn corrupt(what: &str) -> Error {
    Error::new(
        sqlstate::DATA_CORRUPTED,
        format!("invalid btree WAL record: {what}"),
    )
    .with_severity(Severity::Panic)
}

impl PagesMain {
    pub fn encode(&self) -> [u8; 4] {
        [self.reason as u8, 0, 0, 0]
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != 4 || b[1..] != [0, 0, 0] {
            return Err(corrupt("BTREE_PAGES main data has a bad length"));
        }
        PagesReason::from_u8(b[0])
            .map(|reason| PagesMain { reason })
            .ok_or_else(|| corrupt(&format!("unknown BTREE_PAGES reason {}", b[0])))
    }
}

/// `BTREE_PAGES` を 1 本挿入し、全ページに `set_lsn` する（M3 規約 1 の (5)(6)）。呼び出しは
/// `CriticalSection` の中。`guards` の全ページを `FORCE_IMAGE | STANDARD` で登録する（登録順 = `guards` の順）。
/// `guards.len() > MAX_BLOCK_REFS` は `54000`（呼び出し側が事前に防ぐ）。
pub(crate) fn log_pages(
    wal: &Wal,
    w: &WriteCtx,
    reason: PagesReason,
    guards: &mut [(BufferTag, PageWriteGuard<'_>)],
) -> Result<Lsn> {
    if guards.is_empty() {
        return Err(Error::internal("BTREE_PAGES without pages"));
    }
    if guards.len() > MAX_BLOCK_REFS {
        return Err(Error::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "index cannot be split: too many levels",
        ));
    }
    let end = {
        let mut rec = RecordBuilder::new(RmgrId::Btree, BTREE_PAGES, w.xid);
        for (tag, g) in guards.iter() {
            rec.register_block(*tag, g.page(), RegFlags::FORCE_IMAGE | RegFlags::STANDARD);
        }
        rec.main_data(&PagesMain { reason }.encode());
        wal.insert(rec)?.end
    };
    for (_, g) in guards.iter_mut() {
        g.set_lsn(end.0);
    }
    Ok(end)
}

/// `BTREE_INSERT_LEAF` のメインデータ（4 バイト: `offnum`、予約 2 バイト）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InsertLeafMain {
    /// 挿入後の行ポインタ番号。
    pub offnum: u16,
}

impl InsertLeafMain {
    pub fn encode(&self) -> [u8; 4] {
        let o = self.offnum.to_le_bytes();
        [o[0], o[1], 0, 0]
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != 4 || b[2..] != [0, 0] {
            return Err(corrupt("BTREE_INSERT_LEAF main data has a bad length"));
        }
        Ok(InsertLeafMain {
            offnum: u16::from_le_bytes([b[0], b[1]]),
        })
    }
}

/// `BTREE_INSERT_LEAF`（`STANDARD` + 差分データ = タプル全体 + メイン）を 1 本挿入し、`guard` に `set_lsn`
/// する。呼び出しは `CriticalSection` の中で、`guard` のページはタプルを入れた後（FPW の画像は変更後）。
pub(crate) fn log_insert_leaf(
    wal: &Wal,
    w: &WriteCtx,
    tag: BufferTag,
    guard: &mut PageWriteGuard<'_>,
    offnum: u16,
    tuple: &[u8],
) -> Result<Lsn> {
    let end = {
        let mut rec = RecordBuilder::new(RmgrId::Btree, BTREE_INSERT_LEAF, w.xid);
        let id = rec.register_block(tag, guard.page(), RegFlags::STANDARD);
        rec.block_data(id, tuple);
        rec.main_data(&InsertLeafMain { offnum }.encode());
        wal.insert(rec)?.end
    };
    guard.set_lsn(end.0);
    Ok(end)
}

// ----- REDO -----------------------------------------------------------------

/// `recovery::dispatch` が `RmgrId::Btree` で呼ぶ。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    match rec.info {
        BTREE_INSERT_LEAF => redo_insert_leaf(ctx, rec),
        BTREE_PAGES => redo_pages(ctx, rec),
        i => Err(corrupt(&format!("unknown info 0x{i:02X}"))),
    }
}

/// `BTREE_INSERT_LEAF` の REDO（06 §5.11）。
fn redo_insert_leaf(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    if rec.blocks.len() != 1 {
        return Err(corrupt("BTREE_INSERT_LEAF must have exactly 1 block"));
    }
    let m = InsertLeafMain::decode(&rec.main)?;
    match read_buffer_for_redo(ctx, rec, 0)? {
        RedoBuffer::Restored | RedoBuffer::Done | RedoBuffer::NotFound => Ok(()),
        RedoBuffer::NeedsRedo(buf) => {
            let mut g = buf.write()?;
            let data = &rec.blocks[0].data;
            let applied = check_insert_leaf(g.page(), m.offnum, data).and_then(|()| {
                g.page_mut()
                    .insert_item_at(m.offnum, data)
                    .map(|_| ())
                    .ok_or_else(|| corrupt("BTREE_INSERT_LEAF does not fit on the page"))
            });
            match applied {
                Ok(()) => g.set_lsn(rec.end.0),
                Err(e) => {
                    // ガードの「page_mut したら set_lsn」の検査を静かにする（起動が止まるので書き出されない）。
                    let lsn = g.page().lsn();
                    g.set_lsn(lsn);
                    return Err(e);
                }
            }
            Ok(())
        }
    }
}

fn check_insert_leaf(page: &Page, offnum: u16, data: &[u8]) -> Result<()> {
    let Some(sp) = BtSpecial::read(page) else {
        return Err(corrupt("BTREE_INSERT_LEAF target is not a btree page"));
    };
    if !sp.is_leaf() {
        return Err(corrupt("BTREE_INSERT_LEAF target is not a leaf"));
    }
    if offnum < 1 || offnum > page.max_offset() + 1 {
        return Err(corrupt("BTREE_INSERT_LEAF offnum is out of range"));
    }
    if data.len() < INDEX_TUPLE_HEADER_SIZE {
        return Err(corrupt("BTREE_INSERT_LEAF data is too short"));
    }
    let size = usize::from(u16::from_le_bytes([data[6], data[7]]) & INDEX_SIZE_MASK);
    if size != data.len() {
        return Err(corrupt("BTREE_INSERT_LEAF data size differs from t_info"));
    }
    Ok(())
}

fn redo_pages(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    let main = PagesMain::decode(&rec.main)?;
    let n = rec.blocks.len();
    if n == 0 || n > MAX_BLOCK_REFS {
        return Err(corrupt("BTREE_PAGES has a bad number of blocks"));
    }
    if main.reason == PagesReason::Init && n != 2 {
        return Err(corrupt("BTREE_PAGES INIT must have exactly 2 blocks"));
    }
    if rec.blocks.iter().any(|b| b.image.is_none()) {
        return Err(corrupt("BTREE_PAGES block without image"));
    }
    for id in 0..n {
        let id = u8::try_from(id).map_err(|_| corrupt("too many blocks"))?;
        match read_buffer_for_redo(ctx, rec, id)? {
            RedoBuffer::Restored | RedoBuffer::NotFound => {}
            _ => return Err(corrupt("BTREE_PAGES block without image")),
        }
    }
    Ok(())
}

// ----- describe ---------------------------------------------------------------

/// `BTREE_PAGES` の 1 ブロックの役割（画像の special から）。`count` は同じレベルの画像の数、`index` は
/// その中で何番目か。
fn role(sp: Option<BtSpecial>, is_meta: bool, index: usize, count: usize) -> String {
    if is_meta {
        return "meta".to_string();
    }
    let Some(sp) = sp else {
        return "unknown".to_string();
    };
    let kind = if sp.is_leaf() {
        "leaf".to_string()
    } else {
        format!("internal lvl {}", sp.level)
    };
    if sp.is_root() {
        return format!("{kind} root");
    }
    if count < 2 {
        return kind;
    }
    let suffix = match index {
        0 => "L",
        1 => "R",
        _ => "Q",
    };
    format!("{kind} {suffix}")
}

fn describe_pages(rec: &DecodedRecord) -> String {
    let reason = rec
        .main
        .first()
        .copied()
        .and_then(PagesReason::from_u8)
        .map_or("UNKNOWN", PagesReason::name);
    let specials: Vec<(bool, Option<BtSpecial>)> = rec
        .blocks
        .iter()
        .map(|b| {
            let sp = b.image.as_deref().and_then(BtSpecial::read);
            let meta = sp.is_some_and(|s| s.flags & super::BTP_META != 0);
            (meta, sp)
        })
        .collect();
    let level_of = |sp: &Option<BtSpecial>| sp.map(|s| s.level);
    let mut parts = Vec::with_capacity(rec.blocks.len());
    for (i, b) in rec.blocks.iter().enumerate() {
        let (meta, sp) = specials[i];
        let same: Vec<usize> = (0..specials.len())
            .filter(|&j| !specials[j].0 && level_of(&specials[j].1) == level_of(&sp))
            .collect();
        let index = same.iter().position(|&j| j == i).unwrap_or(0);
        let count = if reason == "SPLIT" { same.len() } else { 1 };
        parts.push(format!(
            "{} ({})",
            b.tag.block,
            role(sp, meta, index, count)
        ));
    }
    format!("BTREE_PAGES reason={reason} blks [{}]", parts.join(", "))
}

/// `wal::dump` が `RmgrId::Btree` で呼ぶ 1 行の説明（06 §3.7 の書式）。
pub fn describe(rec: &DecodedRecord) -> String {
    match rec.info {
        BTREE_INSERT_LEAF => {
            let off = rec
                .main
                .get(0..2)
                .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
            match rec.blocks.first() {
                Some(b) => format!(
                    "BTREE_INSERT_LEAF rel {}/{}/{} blk {} off {off} size {}",
                    b.tag.rel.spc_oid,
                    b.tag.rel.db_oid,
                    b.tag.rel.rel_number.0,
                    b.tag.block,
                    b.data.len()
                ),
                None => format!("BTREE_INSERT_LEAF off {off}"),
            }
        }
        BTREE_PAGES => describe_pages(rec),
        i => format!("UNKNOWN 0x{i:02X}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::btree::meta::{BtMeta, empty_root_leaf};
    use crate::storage::btree::testing::test_index;
    use crate::storage::btree::{BTP_LEAF, BTP_ROOT};
    use crate::storage::page::Page;
    use crate::storage::smgr::{ForkNumber, RelFileLocator, RelFileNumber};
    use crate::txn::Xid;
    use crate::types::SqlType;
    use crate::wal::{DecodedBlock, RmgrId};

    fn rec(info: u8, main: Vec<u8>, blocks: &[(u32, Option<Box<Page>>)]) -> DecodedRecord {
        DecodedRecord {
            start: Lsn(0x20_0020),
            end: Lsn(0x20_0080),
            xid: Xid(42),
            rmgr: RmgrId::Btree,
            info,
            blocks: blocks
                .iter()
                .enumerate()
                .map(|(i, (block, image))| DecodedBlock {
                    id: u8::try_from(i).unwrap(),
                    tag: BufferTag {
                        rel: RelFileLocator {
                            spc_oid: 1663,
                            db_oid: 5,
                            rel_number: RelFileNumber(16390),
                        },
                        fork: ForkNumber::Main,
                        block: *block,
                    },
                    will_init: false,
                    image: image.clone(),
                    data: vec![0; 16],
                })
                .collect(),
            main,
        }
    }

    fn leaf_page(flags: u16, level: u32) -> Box<Page> {
        let mut p = Box::new(Page::zeroed());
        p.init_special(16);
        BtSpecial {
            prev: 0,
            next: 0,
            level,
            flags,
            cycleid: 0,
        }
        .write(&mut p);
        p
    }

    #[test]
    fn pages_main_round_trip() {
        for r in [PagesReason::Init, PagesReason::Split, PagesReason::Build] {
            let m = PagesMain { reason: r };
            assert_eq!(PagesMain::decode(&m.encode()).unwrap(), m);
        }
        assert_eq!(
            PagesMain {
                reason: PagesReason::Init
            }
            .encode(),
            [1, 0, 0, 0]
        );
        assert!(PagesMain::decode(&[0, 0, 0, 0]).is_err());
        assert!(PagesMain::decode(&[4, 0, 0, 0]).is_err());
        assert!(PagesMain::decode(&[1, 0]).is_err());
        let e = PagesMain::decode(&[9, 0, 0, 0]).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
    }

    #[test]
    fn describe_formats() {
        assert_eq!(
            describe(&rec(BTREE_INSERT_LEAF, vec![4, 0, 0, 0], &[(1, None)])),
            "BTREE_INSERT_LEAF rel 1663/5/16390 blk 1 off 4 size 16"
        );
        let meta = BtMeta { root: 1, level: 0 }.to_page();
        assert_eq!(
            describe(&rec(
                BTREE_PAGES,
                vec![REASON_INIT, 0, 0, 0],
                &[(0, Some(meta.clone())), (1, Some(empty_root_leaf()))]
            )),
            "BTREE_PAGES reason=INIT blks [0 (meta), 1 (leaf root)]"
        );
        let l = leaf_page(BTP_LEAF, 0);
        let r = leaf_page(BTP_LEAF, 0);
        let root = leaf_page(BTP_ROOT, 1);
        assert_eq!(
            describe(&rec(
                BTREE_PAGES,
                vec![REASON_SPLIT, 0, 0, 0],
                &[(1, Some(l)), (2, Some(r)), (3, Some(root)), (0, Some(meta))]
            )),
            "BTREE_PAGES reason=SPLIT blks [1 (leaf L), 2 (leaf R), 3 (internal lvl 1 root), 0 (meta)]"
        );
        assert_eq!(describe(&rec(0x20, vec![], &[])), "UNKNOWN 0x20");
    }

    #[test]
    fn init_record_is_204_bytes() {
        // §3.6 例 8: tot_len = 204、ブロックの画像は 72 / 40 バイト（穴 8120 / 8152）。
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let w = t.write_ctx();
        let start = t.wal().insert_lsn();
        crate::storage::btree::meta::init_index(t.pool(), t.wal(), &w, &t.handle).unwrap();
        let recs = t.read_wal_from(start);
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.rmgr, RmgrId::Btree);
        assert_eq!(r.info, BTREE_PAGES);
        assert_eq!(r.main, vec![1, 0, 0, 0]);
        assert_eq!(r.blocks.len(), 2);
        assert!(
            r.blocks
                .iter()
                .all(|b| b.image.is_some() && b.data.is_empty())
        );
        assert_eq!(r.end.0 - r.start.0, 208, "align8(tot_len = 204)");
        assert_eq!(r.blocks[0].tag.block, 0);
        assert_eq!(r.blocks[1].tag.block, 1);
        assert_eq!(
            describe(r),
            "BTREE_PAGES reason=INIT blks [0 (meta), 1 (leaf root)]"
        );
    }

    #[test]
    fn redo_rejects_bad_records() {
        let t = test_index(&[(SqlType::INT4, false, false)], false);
        let ctx = t.redo_ctx();
        let meta = BtMeta { root: 1, level: 0 }.to_page();
        let img = |b: u32| (b, Some(meta.clone()));
        let bad = |r: DecodedRecord| {
            let e = redo(&ctx, &r).unwrap_err();
            assert_eq!(e.severity, Severity::Panic);
        };
        bad(rec(0x20, vec![], &[]));
        bad(rec(BTREE_PAGES, vec![9, 0, 0, 0], &[img(0)]));
        bad(rec(BTREE_PAGES, vec![REASON_INIT, 0, 0, 0], &[img(0)]));
        bad(rec(BTREE_PAGES, vec![REASON_SPLIT, 0, 0, 0], &[]));
        bad(rec(BTREE_PAGES, vec![REASON_SPLIT, 0, 0, 0], &[(0, None)]));
    }
}

#[cfg(test)]
mod redo_tests {
    use super::*;
    use crate::storage::btree::meta::init_index;
    use crate::storage::btree::testing::{TestIndex, test_index};
    use crate::storage::btree::tuple::{form_index_tuple, leaf_to_pivot, minus_infinity_pivot};
    use crate::storage::page::Page;
    use crate::storage::testing::TestStorage;
    use crate::types::{Datum, SqlType, Tid};

    fn same(a: &[Box<Page>], b: &[Box<Page>]) {
        assert_eq!(a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            assert!(x.0 == y.0, "block {i} differs");
        }
    }

    fn redo_all(target: &TestIndex, recs: &[DecodedRecord]) {
        let ctx = target.redo_ctx();
        for r in recs {
            redo(&ctx, r).unwrap();
        }
    }

    /// 分割前の古いページを持つディスクと空のディスクの両方で、同じ WAL の REDO が同じバイト列を作る。
    #[test]
    fn pages_redo_equivalence_and_idempotence() {
        let cols = [(SqlType::INT4, false, false)];
        let src = test_index(&cols, false);
        let start = src.wal().insert_lsn();
        init_index(src.pool(), src.wal(), &src.write_ctx(), &src.handle).unwrap();
        // 3 ブロックの別の構造変更（ルート分割に似た形）を 1 レコードで書く。
        let t = &src;
        let tid = |n| Tid {
            block: 0,
            offset: n,
        };
        let l = form_index_tuple(&t.handle, &[Datum::Int4(7)], tid(1)).unwrap();
        let sp = |prev, next, level, flags| {
            BtSpecial {
                prev,
                next,
                level,
                flags,
                cycleid: 0,
            }
            .to_bytes()
        };
        let left = Page::build_with_items(
            &sp(0, 2, 0, super::super::BTP_LEAF),
            &[&leaf_to_pivot(&l, 0, 1)],
        )
        .unwrap();
        let right = Page::build_with_items(&sp(1, 0, 0, super::super::BTP_LEAF), &[&l]).unwrap();
        let root = Page::build_with_items(
            &sp(0, 0, 1, super::super::BTP_ROOT),
            &[&minus_infinity_pivot(1), &leaf_to_pivot(&l, 2, 1)],
        )
        .unwrap();
        let meta = super::super::meta::BtMeta { root: 3, level: 1 }.to_page();
        {
            use crate::storage::buffer::CriticalSection;
            use crate::storage::smgr::ForkNumber;
            let pool = t.pool();
            let p2 = pool.extend(t.locator(), ForkNumber::Main).unwrap();
            let p3 = pool.extend(t.locator(), ForkNumber::Main).unwrap();
            let p1 = pool.read_buffer(t.ctx().tag(1)).unwrap();
            let p0 = pool.read_buffer(t.ctx().tag(0)).unwrap();
            let mut gs = vec![
                (p1.tag(), p1.write_tree().unwrap()),
                (p2.tag(), p2.write_tree().unwrap()),
                (p3.tag(), p3.write_tree().unwrap()),
                (p0.tag(), p0.write_tree().unwrap()),
            ];
            let cs = CriticalSection::enter(pool);
            for ((_, g), page) in gs.iter_mut().zip([&left, &right, &root, &meta]) {
                *g.page_mut() = (**page).clone();
            }
            log_pages(t.wal(), &t.write_ctx(), PagesReason::Split, &mut gs)
                .map_err(|e| cs.escalate(e))
                .unwrap();
        }
        let recs = src.read_wal_from(start);
        assert_eq!(recs.len(), 2);
        assert!(recs[1].end.0 > recs[1].start.0);
        assert_eq!(recs[1].blocks.len(), 4);
        let want = src.snapshot_pages();
        assert_eq!(want.len(), 4);

        // 空のディスクへ。
        let dst = test_index(&cols, false);
        redo_all(&dst, &recs);
        same(&want, &dst.snapshot_pages());
        // 冪等（2 回目は画像で上書きするだけ）。
        redo_all(&dst, &recs);
        same(&want, &dst.snapshot_pages());
        assert!(dst.snapshot_pages().iter().all(|p| p.lsn() > 0));

        // 古いページ（INIT 直後の状態）の上へ SPLIT だけを当てる。
        let old = test_index(&cols, false);
        redo_all(&old, &recs[..1]);
        redo_all(&old, &recs[1..]);
        same(&want, &old.snapshot_pages());
        let _ = TestStorage::new;
        src.assert_clean();
        dst.assert_clean();
        old.assert_clean();
    }
}

#[cfg(test)]
mod redo_insert_tests {
    use super::*;
    use crate::storage::btree::meta::init_index;
    use crate::storage::btree::testing::{Lcg, TestIndex, check_tree, put, test_index, tid_of};
    use crate::storage::page::Page;
    use crate::types::{Datum, SqlType};

    const INT: [(SqlType, bool, bool); 1] = [(SqlType::INT4, false, false)];

    fn same(a: &[Box<Page>], b: &[Box<Page>]) {
        assert_eq!(a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            assert!(x.0 == y.0, "block {i} differs");
        }
    }

    fn redo_all(target: &TestIndex, recs: &[DecodedRecord]) {
        let ctx = target.redo_ctx();
        for r in recs {
            redo(&ctx, r).unwrap();
        }
    }

    #[test]
    fn insert_leaf_main_round_trip() {
        let m = InsertLeafMain { offnum: 0x1234 };
        assert_eq!(m.encode(), [0x34, 0x12, 0, 0]);
        assert_eq!(InsertLeafMain::decode(&m.encode()).unwrap(), m);
        assert!(InsertLeafMain::decode(&[1, 0, 1, 0]).is_err());
        assert!(InsertLeafMain::decode(&[1, 0]).is_err());
    }

    /// 挿入・分割・ルート分割を含む操作列の WAL を、空のディスクと途中まで進んだディスクに REDO して、
    /// 全ページがバイト単位で一致する。2 回目の REDO（`Done`）も同じ（06 §7.5）。
    #[test]
    fn insert_and_split_redo_equivalence() {
        let src = test_index(&INT, false);
        init_index(src.pool(), src.wal(), &src.write_ctx(), &src.handle).unwrap();
        let mut r = Lcg(11);
        for (i, p) in r.permutation(1500).into_iter().enumerate() {
            put(
                &src,
                &[Datum::Int4(i32::try_from(p).unwrap())],
                tid_of(u32::try_from(i).unwrap()),
            )
            .unwrap();
        }
        // 大きなキーで内部ページも分割させる。
        let recs = src.read_wal_from(src.start);
        assert!(recs.iter().any(|x| x.info == BTREE_INSERT_LEAF));
        assert!(
            recs.iter()
                .any(|x| x.info == BTREE_PAGES && x.main[0] == REASON_SPLIT)
        );
        let want = src.snapshot_pages();
        check_tree(&src);

        let dst = test_index(&INT, false);
        redo_all(&dst, &recs);
        same(&want, &dst.snapshot_pages());
        redo_all(&dst, &recs);
        same(&want, &dst.snapshot_pages());
        assert!(dst.snapshot_pages().iter().all(|p| p.lsn() > 0));

        // 途中まで REDO 済みのディスク（古いページ）の上へ残りを当てる。
        let half = recs.len() / 2;
        let old = test_index(&INT, false);
        redo_all(&old, &recs[..half]);
        redo_all(&old, &recs[half..]);
        same(&want, &old.snapshot_pages());
        src.assert_clean();
        dst.assert_clean();
        old.assert_clean();
    }

    #[test]
    fn insert_leaf_redo_rejects_bad_records() {
        let t = test_index(&INT, false);
        init_index(t.pool(), t.wal(), &t.write_ctx(), &t.handle).unwrap();
        put(&t, &[Datum::Int4(1)], tid_of(1)).unwrap();
        let recs = t.read_wal_from(t.start);
        let good = recs
            .iter()
            .find(|r| r.info == BTREE_INSERT_LEAF)
            .unwrap()
            .clone();
        // 古いページ（INIT 直後）の上で壊したレコードを当てる。
        let bad = |f: &dyn Fn(&mut DecodedRecord)| {
            let dst = test_index(&INT, false);
            redo_all(&dst, &recs[..1]);
            let mut r = good.clone();
            f(&mut r);
            let e = redo(&dst.redo_ctx(), &r).unwrap_err();
            assert_eq!(e.severity, Severity::Panic);
        };
        bad(&|r| r.main = vec![9, 0, 0, 0]); // offnum が範囲外
        bad(&|r| r.main = vec![0, 0, 0, 0]);
        bad(&|r| r.blocks[0].data.truncate(8)); // t_info の大きさと違う
        bad(&|r| r.blocks.clear());
        bad(&|r| r.main = vec![1, 0]);
        // 正しいレコードは当たる。
        let dst = test_index(&INT, false);
        redo_all(&dst, &recs[..1]);
        redo(&dst.redo_ctx(), &good).unwrap();
        assert_eq!(dst.snapshot_pages()[1].max_offset(), 1);
    }
}
