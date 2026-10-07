//! `SeqStore`: `SequenceStore` の実装と `SEQ` rmgr の REDO・表示（`m4/08-sequence-serial.md` §3〜§5、
//! `m4/00-contracts.md` §13.3）。
//!
//! シーケンスは 1 ブロックのリレーションで、タプルを 1 個だけ持つ。MVCC は使わず、ページの排他ラッチの
//! 下でその場で書き換える。WAL は `SEQ_LOG`（ページ全体を作り直せるタプル全体。`WILL_INIT`）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::buffer::{BufferPool, CriticalSection};
use super::page::Page;
use super::smgr::{BufferTag, ForkNumber, StorageManager};
use super::{SeqRun, SeqState, SequenceHandle, SequenceStore, WriteCtx};
use crate::catalog::SequenceParams;
use crate::debug_knobs::DebugKnobs;
use crate::error::{Error, Result, Severity, sqlstate};
use crate::txn::Xid;
use crate::wal::{
    DecodedRecord, Lsn, RecordBuilder, RedoBuffer, RedoCtx, RegFlags, RmgrId, Wal,
    read_buffer_for_redo,
};

/// `SEQ_LOG` の先取り個数（00 §15.1）。
pub const SEQ_LOG_VALS: i64 = 32;
/// ページの special 領域の magic（00 §15.1）。
pub const SEQ_MAGIC: u32 = 0x1717;
/// WAL の `info`（00 §13.4）。blk0 = シーケンスのページ（`WILL_INIT`、タプル全体）。
pub const SEQ_LOG: u8 = 0x00;
pub const SEQ_SPECIAL_SIZE: usize = 8;
/// タプルの `lp_len`。
pub const SEQ_TUPLE_LEN: usize = 57;
pub const SEQ_TUPLE_HOFF: usize = 40;
/// 行ポインタ番号（常に 1）。
pub const SEQ_ITEM_OFFSET: u16 = 1;

/// `t_infomask`: `HEAP_XMAX_INVALID` だけ。
const SEQ_INFOMASK: u16 = 0x0800;
/// `t_infomask2`: 列数 3。
const SEQ_NATTS: u16 = 3;
/// `t_xmin`: `Xid::FROZEN`。
const SEQ_XMIN: u64 = 2;

// ----- タプルとページ ---------------------------------------------------------------

/// §3.3 の 57 バイトのタプル。
pub fn seq_tuple_bytes(st: &SeqState) -> [u8; SEQ_TUPLE_LEN] {
    let mut t = [0u8; SEQ_TUPLE_LEN];
    t[0..8].copy_from_slice(&SEQ_XMIN.to_le_bytes());
    // xmax・cmin・cmax は 0。t_ctid = (0, 1): bi_hi・bi_lo（24..28）は 0、posid（28..30）は 1。
    t[28..30].copy_from_slice(&SEQ_ITEM_OFFSET.to_le_bytes());
    t[30..32].copy_from_slice(&SEQ_NATTS.to_le_bytes());
    t[32..34].copy_from_slice(&SEQ_INFOMASK.to_le_bytes());
    t[34] = 40; // SEQ_TUPLE_HOFF
    write_values(&mut t, st);
    t
}

fn write_values(tuple: &mut [u8], st: &SeqState) {
    tuple[SEQ_TUPLE_HOFF..SEQ_TUPLE_HOFF + 8].copy_from_slice(&st.last_value.to_le_bytes());
    tuple[SEQ_TUPLE_HOFF + 8..SEQ_TUPLE_HOFF + 16].copy_from_slice(&st.log_cnt.to_le_bytes());
    tuple[SEQ_TUPLE_HOFF + 16] = u8::from(st.is_called);
}

fn corrupt(msg: String) -> Error {
    Error::corrupted(msg)
}

fn i64_at(b: &[u8], off: usize) -> i64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    i64::from_le_bytes(a)
}

/// ページからシーケンスの状態を読む。形が違えば `XX001`（§3.3）。
pub fn read_state(page: &Page, name: &str) -> Result<SeqState> {
    let special = page.special_area();
    let magic = special
        .get(..4)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if magic != SEQ_MAGIC {
        return Err(corrupt(format!(
            "bad magic number in sequence \"{name}\": {magic:08X}"
        )));
    }
    if page.max_offset() != 1 {
        return Err(corrupt(format!(
            "bad number of tuples in sequence \"{name}\": {}",
            page.max_offset()
        )));
    }
    let t = page
        .item(SEQ_ITEM_OFFSET)
        .map_err(|_| corrupt(format!("bad line pointer in sequence \"{name}\"")))?;
    if t.len() != SEQ_TUPLE_LEN {
        return Err(corrupt(format!(
            "unexpected tuple length {} in sequence \"{name}\"",
            t.len()
        )));
    }
    if usize::from(t[34]) != SEQ_TUPLE_HOFF {
        return Err(corrupt(format!(
            "unexpected t_hoff {} in sequence \"{name}\"",
            t[34]
        )));
    }
    let natts = u16::from_le_bytes([t[30], t[31]]) & 0x07FF;
    if natts != SEQ_NATTS {
        return Err(corrupt(format!(
            "unexpected number of columns {natts} in sequence \"{name}\""
        )));
    }
    let is_called = match t[SEQ_TUPLE_HOFF + 16] {
        0 => false,
        1 => true,
        v => {
            return Err(corrupt(format!(
                "bad is_called value {v} in sequence \"{name}\""
            )));
        }
    };
    Ok(SeqState {
        last_value: i64_at(t, SEQ_TUPLE_HOFF),
        log_cnt: i64_at(t, SEQ_TUPLE_HOFF + 8),
        is_called,
    })
}

/// 3 つの値をその場で書き換える（`read_state` を通った後のページに使う）。
fn write_state(page: &mut Page, st: &SeqState) -> Result<()> {
    let t = page
        .item_mut(SEQ_ITEM_OFFSET)
        .map_err(|_| corrupt("bad line pointer in sequence page".into()))?;
    if t.len() != SEQ_TUPLE_LEN {
        return Err(corrupt("unexpected tuple length in sequence page".into()));
    }
    write_values(t, st);
    Ok(())
}

/// 空のシーケンスのページ（§3.2、§3.4）を組み立てる。`init` と REDO が共有する。
fn build_page(tuple: &[u8]) -> Result<Page> {
    let mut page = Page::zeroed();
    page.init_special(SEQ_SPECIAL_SIZE);
    let sp = page.special_area_mut();
    if sp.len() != SEQ_SPECIAL_SIZE {
        return Err(Error::internal("sequence page: bad special area"));
    }
    sp[..4].copy_from_slice(&SEQ_MAGIC.to_le_bytes());
    if page.add_item(tuple) != Some(SEQ_ITEM_OFFSET) {
        return Err(Error::internal("failed to add item to sequence page"));
    }
    Ok(page)
}

// ----- plan_fetch -------------------------------------------------------------------

/// `nextval` の計算結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FetchPlan {
    pub first: i64,
    pub count: u32,
    /// ページに書く最終状態。
    pub page: SeqState,
    /// WAL に書く状態（あと `SEQ_LOG_VALS` 個進めた後）。`None` ならこの呼び出しでは書かない。
    pub log: Option<SeqState>,
}

fn limit_error(name: &str, p: &SequenceParams) -> Error {
    let (word, v) = if p.increment > 0 {
        ("maximum", p.max)
    } else {
        ("minimum", p.min)
    };
    Error::new(
        sqlstate::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
        format!("nextval: reached {word} value of sequence \"{name}\" ({v})"),
    )
}

fn narrow<T: TryFrom<i128>>(v: i128) -> Result<T> {
    T::try_from(v).map_err(|_| Error::internal("sequence value out of range in plan_fetch"))
}

/// PostgreSQL の `nextval_internal` の「`fetch` / `log` / `rescnt` の計算」と同値の純粋関数（§4.3）。
///
/// `cache >= 1`、`p.increment != 0`。`force_log` はページの LSN <= REDO 点。
pub(crate) fn plan_fetch(
    name: &str,
    p: &SequenceParams,
    st: SeqState,
    cache: i64,
    force_log: bool,
) -> Result<FetchPlan> {
    if cache < 1 || p.increment == 0 {
        return Err(Error::internal("plan_fetch: bad cache or increment"));
    }
    let (incr, min, max, cache) = (
        i128::from(p.increment),
        i128::from(p.min),
        i128::from(p.max),
        i128::from(cache),
    );
    let mut next = i128::from(st.last_value);
    // is_called = false なら last_value 自身が最初の値（先に 1 個と数える）。
    let mut rescnt: i128 = i128::from(!st.is_called);
    let mut fetch = cache - rescnt;
    let logit = i128::from(st.log_cnt) < fetch || !st.is_called || force_log;
    let mut log = if logit {
        fetch += i128::from(SEQ_LOG_VALS);
        fetch
    } else {
        i128::from(st.log_cnt)
    };
    let (mut first, mut last) = (next, next);
    // 上限（降順なら下限）に当たるまでに incr を足せる回数。
    let room = |n: i128| -> i128 {
        if incr > 0 {
            if n <= max { (max - n) / incr } else { 0 }
        } else if n >= min {
            (n - min) / -incr
        } else {
            0
        }
    };
    // 1 つも払い出せない（rescnt == 0）のに足せない: CYCLE なら折り返し先が最初の値、でなければ 2200H。
    if rescnt == 0 && fetch > 0 && room(next) == 0 {
        if !p.cycle {
            return Err(limit_error(name, p));
        }
        next = if incr > 0 { min } else { max };
        fetch -= 1;
        log -= 1;
        rescnt = 1;
        first = next;
        last = next;
    }
    // 実際に進める回数。1 回の fetch の中では折り返さない（等差数列）。
    let steps = fetch.min(room(next));
    // うち、払い出しに数える回数。
    let counted = steps.min(cache - rescnt);
    if counted > 0 && rescnt == 0 {
        first = next + incr;
    }
    if counted > 0 {
        last = next + counted * incr;
    }
    next += steps * incr;
    fetch -= steps;
    rescnt += counted;
    log -= counted;
    // 進めなかった分（上限に当たった）。
    log -= fetch;
    Ok(FetchPlan {
        first: narrow(first)?,
        count: narrow(rescnt)?,
        page: SeqState {
            last_value: narrow(last)?,
            log_cnt: narrow(log)?,
            is_called: true,
        },
        log: if logit {
            Some(SeqState {
                last_value: narrow(next)?,
                log_cnt: 0,
                is_called: true,
            })
        } else {
            None
        },
    })
}

// ----- SeqStore ---------------------------------------------------------------------

/// 実体。`StorageStack::new` が作って `StorageStack.seq` に入れる。
#[derive(Debug)]
pub struct SeqStore {
    pool: Arc<BufferPool>,
    smgr: Arc<StorageManager>,
    wal: Arc<Wal>,
    reset_gen: AtomicU64,
    knobs: DebugKnobs,
}

impl SeqStore {
    pub fn new(pool: Arc<BufferPool>, smgr: Arc<StorageManager>, wal: Arc<Wal>) -> SeqStore {
        SeqStore {
            pool,
            smgr,
            wal,
            reset_gen: AtomicU64::new(0),
            knobs: DebugKnobs::default(),
        }
    }

    /// 変異試験用のスイッチ（`seq_*`）を渡す。
    #[must_use]
    pub fn with_knobs(mut self, knobs: DebugKnobs) -> SeqStore {
        self.knobs = knobs;
        self
    }

    fn tag(seq: &SequenceHandle) -> BufferTag {
        BufferTag {
            rel: seq.locator,
            fork: ForkNumber::Main,
            block: 0,
        }
    }

    /// 排他ラッチの下で `SEQ_LOG` を 1 本書き、ページを `page_state` にする（M3 規約 1）。
    /// 呼び出し側は `g.page_mut_hint()` で先に dirty にしてある。返すのはレコードの終端。
    fn log_and_write(
        &self,
        tag: BufferTag,
        g: &mut super::buffer::PageWriteGuard<'_>,
        page_state: &SeqState,
        logged: &SeqState,
        xid: Xid,
    ) -> Result<Lsn> {
        let cs = CriticalSection::enter(&self.pool);
        write_state(g.page_mut(), page_state).map_err(|e| cs.escalate(e))?;
        let mut rec = RecordBuilder::new(RmgrId::Seq, SEQ_LOG, xid);
        let b = rec.register_block(tag, g.page(), RegFlags::WILL_INIT);
        rec.block_data(b, &seq_tuple_bytes(logged));
        let ins = self.wal.insert(rec).map_err(|e| cs.escalate(e))?;
        g.set_lsn(ins.end.0);
        Ok(ins.end)
    }
}

fn range_error(kind: &str, v: i64, bound: &str, limit: i64) -> Error {
    Error::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("{kind} value ({v}) cannot be {bound} ({limit})"),
    )
}

impl SequenceStore for SeqStore {
    fn init(&self, w: &WriteCtx, seq: &SequenceHandle) -> Result<Lsn> {
        let st = SeqState {
            last_value: seq.params.start,
            log_cnt: 0,
            is_called: false,
        };
        let buf = self.pool.extend(seq.locator, ForkNumber::Main)?;
        let tag = buf.tag();
        if tag.block != 0 {
            return Err(Error::internal(format!(
                "sequence \"{}\" already has {} blocks",
                seq.name, tag.block
            )));
        }
        let mut g = buf.write()?;
        let tuple = seq_tuple_bytes(&st);
        let page = build_page(&tuple)?;
        let cs = CriticalSection::enter(&self.pool);
        *g.page_mut() = page;
        let mut rec = RecordBuilder::new(RmgrId::Seq, SEQ_LOG, w.xid);
        let b = rec.register_block(tag, g.page(), RegFlags::WILL_INIT);
        rec.block_data(b, &tuple);
        let ins = self.wal.insert(rec).map_err(|e| cs.escalate(e))?;
        g.set_lsn(ins.end.0);
        Ok(ins.end)
    }

    fn fetch(&self, seq: &SequenceHandle, count: u32) -> Result<SeqRun> {
        let tag = Self::tag(seq);
        let buf = self.pool.read_buffer(tag)?;
        let mut g = buf.write()?;
        // 先に dirty にしてから REDO 点を読む（D8-7）。
        let _ = g.page_mut_hint();
        let st = read_state(g.page(), &seq.name)?;
        let force_log = !self.knobs.seq_no_force_log && g.page().lsn() <= self.wal.redo_lsn().0;
        let plan = plan_fetch(&seq.name, &seq.params, st, i64::from(count), force_log)?;
        let mut own = Lsn::INVALID;
        match plan.log {
            Some(logged) => {
                own = self.log_and_write(tag, &mut g, &plan.page, &logged, Xid::INVALID)?;
            }
            None => write_state(g.page_mut_hint(), &plan.page)?,
        }
        let wal_lsn = if self.knobs.seq_ignore_foreign_wal {
            own
        } else {
            Lsn(g.page().lsn())
        };
        Ok(SeqRun {
            first: plan.first,
            count: plan.count,
            increment: seq.params.increment,
            wal_lsn,
        })
    }

    fn setval(&self, seq: &SequenceHandle, value: i64, is_called: bool) -> Result<Lsn> {
        let tag = Self::tag(seq);
        let buf = self.pool.read_buffer(tag)?;
        let mut g = buf.write()?;
        let _ = g.page_mut_hint();
        read_state(g.page(), &seq.name)?;
        let p = &seq.params;
        if value < p.min || value > p.max {
            return Err(Error::new(
                sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
                format!(
                    "setval: value {value} is out of bounds for sequence \"{}\" ({}..{})",
                    seq.name, p.min, p.max
                ),
            ));
        }
        let st = SeqState {
            last_value: value,
            log_cnt: 0,
            is_called,
        };
        self.log_and_write(tag, &mut g, &st, &st, Xid::INVALID)
    }

    fn read(&self, seq: &SequenceHandle) -> Result<SeqState> {
        let buf = self.pool.read_buffer(Self::tag(seq))?;
        let g = buf.read()?;
        read_state(&g, &seq.name)
    }

    fn reset(
        &self,
        seq: &SequenceHandle,
        new_params: &SequenceParams,
        restart_with: Option<i64>,
    ) -> Result<Lsn> {
        let tag = Self::tag(seq);
        let buf = self.pool.read_buffer(tag)?;
        let mut g = buf.write()?;
        let _ = g.page_mut_hint();
        let cur = read_state(g.page(), &seq.name)?;
        let st = match restart_with {
            Some(v) => SeqState {
                last_value: v,
                log_cnt: 0,
                is_called: false,
            },
            None => SeqState {
                last_value: cur.last_value,
                log_cnt: 0,
                is_called: cur.is_called,
            },
        };
        if st.last_value < new_params.min {
            return Err(range_error(
                "RESTART",
                st.last_value,
                "less than MINVALUE",
                new_params.min,
            ));
        }
        if st.last_value > new_params.max {
            return Err(range_error(
                "RESTART",
                st.last_value,
                "greater than MAXVALUE",
                new_params.max,
            ));
        }
        let end = self.log_and_write(tag, &mut g, &st, &st, Xid::INVALID)?;
        self.reset_gen.fetch_add(1, Ordering::AcqRel);
        Ok(end)
    }

    fn reset_generation(&self) -> u64 {
        self.reset_gen.load(Ordering::Acquire)
    }

    fn unlink_storage(&self, seq: &SequenceHandle) -> Result<()> {
        self.pool.drop_relation_buffers(seq.locator)?;
        self.smgr.unlink(seq.locator)
    }
}

// ----- REDO と表示 ------------------------------------------------------------------

fn redo_panic(msg: impl Into<String>) -> Error {
    Error::new(sqlstate::DATA_CORRUPTED, msg).with_severity(Severity::Panic)
}

/// `recovery::dispatch` が `RmgrId::Seq` で呼ぶ（§3.7）。常に無条件にページを置き換える。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    if rec.rmgr != RmgrId::Seq || (rec.info & 0xF0) != SEQ_LOG {
        return Err(redo_panic(format!(
            "seq_redo: unknown op code {}",
            rec.info
        )));
    }
    let blk = match rec.blocks.as_slice() {
        [b] if b.will_init && b.image.is_none() && b.data.len() == SEQ_TUPLE_LEN => b,
        _ => {
            return Err(redo_panic(format!(
                "seq_redo: malformed SEQ_LOG record at {}",
                rec.start
            )));
        }
    };
    if usize::from(blk.data[34]) != SEQ_TUPLE_HOFF
        || (u16::from_le_bytes([blk.data[30], blk.data[31]]) & 0x07FF) != SEQ_NATTS
    {
        return Err(redo_panic(format!(
            "seq_redo: bad tuple header in SEQ_LOG record at {}",
            rec.start
        )));
    }
    if ctx.knobs.seq_redo_skip_if_page_newer
        && ctx.smgr.exists(blk.tag.rel, blk.tag.fork)?
        && blk.tag.block < ctx.smgr.nblocks(blk.tag.rel, blk.tag.fork)?
    {
        let buf = ctx.pool.read_buffer(blk.tag)?;
        if buf.read()?.lsn() >= rec.end.0 {
            return Ok(());
        }
    }
    match read_buffer_for_redo(ctx, rec, 0)? {
        RedoBuffer::NeedsRedo(buf) => {
            let local = build_page(&blk.data).map_err(|e| redo_panic(e.message))?;
            let mut g = buf.write()?;
            *g.page_mut() = local;
            g.set_lsn(rec.end.0);
        }
        RedoBuffer::Restored | RedoBuffer::Done | RedoBuffer::NotFound => {}
    }
    Ok(())
}

/// `wal::dump` が `RmgrId::Seq` で呼ぶ 1 行の説明（08 §3.6 の書式）。
pub fn describe(rec: &DecodedRecord) -> String {
    if rec.info != SEQ_LOG {
        return format!("UNKNOWN 0x{:02X}", rec.info);
    }
    let Some(b) = rec.blocks.first() else {
        return "SEQ_LOG".into();
    };
    let head = format!(
        "SEQ_LOG rel {}/{}/{} blk {}",
        b.tag.rel.spc_oid, b.tag.rel.db_oid, b.tag.rel.rel_number.0, b.tag.block
    );
    // タプル: ヘッダ 40 バイトの後に last_value(i64)、log_cnt(i64)、is_called(u8)。
    let d = &b.data;
    let (Some(lv), Some(lc), Some(ic)) = (
        d.get(SEQ_TUPLE_HOFF..SEQ_TUPLE_HOFF + 8),
        d.get(SEQ_TUPLE_HOFF + 8..SEQ_TUPLE_HOFF + 16),
        d.get(SEQ_TUPLE_HOFF + 16),
    ) else {
        return head;
    };
    let last_value = i64::from_le_bytes(lv.try_into().unwrap_or([0; 8]));
    let log_cnt = i64::from_le_bytes(lc.try_into().unwrap_or([0; 8]));
    format!(
        "{head}: last_value {last_value} log_cnt {log_cnt} is_called {}",
        if *ic == 0 { 'f' } else { 't' }
    )
}

#[cfg(test)]
#[allow(clippy::many_single_char_names, clippy::too_many_lines)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    use super::*;
    use crate::catalog::fake::{FakeCatalog, table_def};
    use crate::storage::smgr::{BufferTag, RelFileLocator, RelFileNumber};
    use crate::storage::testing::{TestStorage, test_rel};
    use crate::storage::vfs::CrashMode;
    use crate::storage::{RelHandle, TableStore};
    use crate::txn::Snapshot;
    use crate::types::{Datum, oid};
    use crate::wal::record::encode_record;
    use crate::wal::{DecodedBlock, InvalidPages, SEG_HEADER_SIZE, WalReader};

    fn params(
        start: i64,
        incr: i64,
        min: i64,
        max: i64,
        cache: i64,
        cycle: bool,
    ) -> SequenceParams {
        SequenceParams {
            type_oid: oid::INT8,
            start,
            increment: incr,
            min,
            max,
            cache,
            cycle,
            owned_by: None,
        }
    }

    fn st(last_value: i64, log_cnt: i64, is_called: bool) -> SeqState {
        SeqState {
            last_value,
            log_cnt,
            is_called,
        }
    }

    // ----- plan_fetch -----------------------------------------------------------

    #[test]
    fn plan_fetch_verified_values() {
        // (state, params, cache, force_log, first, count, page, log)
        type Case = (
            SeqState,
            SequenceParams,
            i64,
            bool,
            i64,
            u32,
            SeqState,
            Option<SeqState>,
        );
        let p1 = params(1, 1, 1, i64::MAX, 1, false);
        let p5 = params(1, 1, 1, i64::MAX, 5, false);
        let p5i2 = params(10, 2, 1, i64::MAX, 5, false);
        let c3 = params(1, 1, 1, 7, 3, false);
        let m3 = params(1, 1, 1, 3, 1, false);
        let cyc = params(1, 1, 1, 2, 5, true);
        let cases: Vec<Case> = vec![
            (
                st(1, 0, false),
                p1,
                1,
                false,
                1,
                1,
                st(1, 32, true),
                Some(st(33, 0, true)),
            ),
            (st(1, 32, true), p1, 1, false, 2, 1, st(2, 31, true), None),
            (
                st(1, 0, false),
                p5,
                5,
                false,
                1,
                5,
                st(5, 32, true),
                Some(st(37, 0, true)),
            ),
            (st(5, 32, true), p5, 5, false, 6, 5, st(10, 27, true), None),
            (
                st(10, 0, false),
                p5i2,
                5,
                false,
                10,
                5,
                st(18, 32, true),
                Some(st(82, 0, true)),
            ),
            (st(3, 4, true), c3, 3, false, 4, 3, st(6, 1, true), None),
            (
                st(6, 1, true),
                c3,
                3,
                false,
                7,
                1,
                st(7, 0, true),
                Some(st(7, 0, true)),
            ),
            (
                st(3, 30, true),
                p1,
                1,
                true,
                4,
                1,
                st(4, 32, true),
                Some(st(36, 0, true)),
            ),
            (
                st(1, 0, false),
                m3,
                1,
                false,
                1,
                1,
                st(1, 2, true),
                Some(st(3, 0, true)),
            ),
            (
                st(1, 0, false),
                cyc,
                5,
                false,
                1,
                2,
                st(2, 0, true),
                Some(st(2, 0, true)),
            ),
        ];
        for (i, (s, p, cache, force, first, count, page, log)) in cases.into_iter().enumerate() {
            let plan = plan_fetch("s", &p, s, cache, force).unwrap();
            assert_eq!(
                plan,
                FetchPlan {
                    first,
                    count,
                    page,
                    log
                },
                "case {i}"
            );
        }
    }

    #[test]
    fn plan_fetch_limit_errors() {
        let c3 = params(1, 1, 1, 7, 3, false);
        let e = plan_fetch("c3", &c3, st(7, 0, true), 3, false).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::SEQUENCE_GENERATOR_LIMIT_EXCEEDED);
        assert_eq!(
            e.message,
            "nextval: reached maximum value of sequence \"c3\" (7)"
        );
        let d = params(-1, -1, -2, -1, 1, false);
        let e = plan_fetch("d", &d, st(-2, 0, true), 1, false).unwrap_err();
        assert_eq!(e.sqlstate.0, "2200H");
        assert_eq!(
            e.message,
            "nextval: reached minimum value of sequence \"d\" (-2)"
        );
        // 初回（is_called = false）に上限を越えない。
        assert!(plan_fetch("d", &d, st(-1, 0, false), 1, false).is_ok());
    }

    #[test]
    fn plan_fetch_cycle_wraps_only_when_nothing_was_issued() {
        let p = params(1, 1, 1, 3, 2, true);
        let plan = plan_fetch("s", &p, st(3, 0, true), 2, false).unwrap();
        assert_eq!((plan.first, plan.count), (1, 2));
        let p = params(3, -1, 1, 3, 2, true);
        let plan = plan_fetch("s", &p, st(1, 0, true), 2, false).unwrap();
        assert_eq!((plan.first, plan.count), (3, 2));
    }

    #[test]
    fn plan_fetch_runs_are_arithmetic_progressions() {
        let p = params(1, 3, 1, 20, 100, false);
        let plan = plan_fetch("s", &p, st(1, 0, false), 100, false).unwrap();
        // 1, 4, ..., 19
        assert_eq!((plan.first, plan.count, plan.page.last_value), (1, 7, 19));
    }

    /// PostgreSQL の `nextval_internal` のループをそのまま写した参照実装。
    fn plan_fetch_literal(
        p: &SequenceParams,
        s: SeqState,
        cache: i64,
        force_log: bool,
    ) -> Option<(i64, u32, SeqState, Option<SeqState>)> {
        let (incby, maxv, minv) = (
            i128::from(p.increment),
            i128::from(p.max),
            i128::from(p.min),
        );
        let cache = i128::from(cache);
        let mut next = i128::from(s.last_value);
        let mut last = next;
        let mut result = next;
        let mut fetch = cache;
        let mut log = i128::from(s.log_cnt);
        let mut rescnt: i128 = 0;
        let mut logit = false;
        if !s.is_called {
            rescnt += 1;
            fetch -= 1;
        }
        if log < fetch || !s.is_called || force_log {
            log = fetch + i128::from(SEQ_LOG_VALS);
            fetch = log;
            logit = true;
        }
        while fetch > 0 {
            if incby > 0 {
                if next + incby > maxv {
                    if rescnt > 0 {
                        break;
                    }
                    if !p.cycle {
                        return None;
                    }
                    next = minv;
                } else {
                    next += incby;
                }
            } else if next + incby < minv {
                if rescnt > 0 {
                    break;
                }
                if !p.cycle {
                    return None;
                }
                next = maxv;
            } else {
                next += incby;
            }
            fetch -= 1;
            if rescnt < cache {
                log -= 1;
                rescnt += 1;
                last = next;
                if rescnt == 1 {
                    result = next;
                }
            }
        }
        log -= fetch;
        Some((
            i64::try_from(result).unwrap(),
            u32::try_from(rescnt).unwrap(),
            SeqState {
                last_value: i64::try_from(last).unwrap(),
                log_cnt: i64::try_from(log).unwrap(),
                is_called: true,
            },
            logit.then(|| SeqState {
                last_value: i64::try_from(next).unwrap(),
                log_cnt: 0,
                is_called: true,
            }),
        ))
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        /// `lo..=hi`（i128 で計算して幅の大きい範囲も扱う）。
        fn range(&mut self, lo: i64, hi: i64) -> i64 {
            let span = i128::from(hi) - i128::from(lo) + 1;
            let r =
                (u128::from(self.next()) << 64 | u128::from(self.next())) % span.cast_unsigned();
            i64::try_from(i128::from(lo) + i128::try_from(r).unwrap()).unwrap()
        }
    }

    #[test]
    fn plan_fetch_matches_the_literal_loop() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut errors = 0u32;
        let mut wraps = 0u32;
        for _ in 0..300_000 {
            let (tmin, tmax) = match rng.below(3) {
                0 => (i64::from(i16::MIN), i64::from(i16::MAX)),
                1 => (i64::from(i32::MIN), i64::from(i32::MAX)),
                _ => (i64::MIN, i64::MAX),
            };
            let asc = rng.below(2) == 0;
            let big = rng.below(8) == 0;
            let mag = if big {
                u64::try_from(rng.range(1, i64::MAX)).unwrap()
            } else {
                rng.below(50) + 1
            };
            let incr = if asc {
                i64::try_from(mag).unwrap()
            } else {
                -i64::try_from(mag).unwrap()
            };
            // 範囲を狭くして上限・下限・折り返しに当たりやすくする。
            let (min, max) = if rng.below(2) == 0 {
                let lo = rng.range(tmin, tmax.saturating_sub(1).max(tmin));
                let width = i64::try_from(rng.below(300) + 1).unwrap();
                (lo, lo.saturating_add(width).min(tmax))
            } else {
                (tmin.max(i64::MIN + 1), tmax)
            };
            if min >= max {
                continue;
            }
            let last_value = rng.range(min, max);
            let is_called = rng.below(2) == 0;
            let log_cnt = i64::try_from(rng.below(41)).unwrap();
            let cache = i64::try_from(rng.below(100) + 1).unwrap();
            let force = rng.below(4) == 0;
            let cycle = rng.below(3) == 0;
            let p = SequenceParams {
                type_oid: oid::INT8,
                start: min,
                increment: incr,
                min,
                max,
                cache,
                cycle,
                owned_by: None,
            };
            let s = st(last_value, log_cnt, is_called);
            let want = plan_fetch_literal(&p, s, cache, force);
            let got = plan_fetch("s", &p, s, cache, force);
            match (want, got) {
                (None, Err(e)) => {
                    errors += 1;
                    assert_eq!(e.sqlstate.0, "2200H");
                }
                (Some((first, count, page, log)), Ok(plan)) => {
                    assert_eq!(
                        plan,
                        FetchPlan {
                            first,
                            count,
                            page,
                            log
                        },
                        "p={p:?} s={s:?} cache={cache} force={force}"
                    );
                    if count > 1 {
                        // 1 回の払い出しは折り返さない。
                        let last = i128::from(first) + i128::from(count - 1) * i128::from(incr);
                        assert_eq!(last, i128::from(plan.page.last_value));
                    }
                    if (incr > 0 && first < last_value) || (incr < 0 && first > last_value) {
                        wraps += 1;
                    }
                    assert!(plan.page.log_cnt >= 0);
                }
                (w, g) => panic!("mismatch: literal {w:?} vs closed form {g:?} for {p:?} {s:?}"),
            }
        }
        assert!(errors > 1000, "errors: {errors}");
        assert!(wraps > 100, "wraps: {wraps}");
    }

    // ----- ページとタプル ----------------------------------------------------------

    fn handle(rel: u32, name: &str, p: SequenceParams) -> SequenceHandle {
        SequenceHandle {
            oid: rel,
            name: name.into(),
            locator: test_rel(rel),
            params: p,
        }
    }

    struct Rig {
        ts: TestStorage,
        store: SeqStore,
    }

    impl Rig {
        fn new() -> Rig {
            Rig::over(TestStorage::new())
        }

        fn over(ts: TestStorage) -> Rig {
            let store = SeqStore::new(
                Arc::clone(ts.pool()),
                Arc::clone(ts.smgr()),
                Arc::clone(ts.wal()),
            );
            Rig { ts, store }
        }

        fn create(&self, rel: u32, name: &str, p: SequenceParams) -> SequenceHandle {
            let h = handle(rel, name, p);
            self.ts.create_rel(h.locator).unwrap();
            self.store
                .init(
                    &WriteCtx {
                        xid: Xid(3),
                        cid: 0,
                    },
                    &h,
                )
                .unwrap();
            h
        }

        fn page_bytes(&self, h: &SequenceHandle) -> Vec<u8> {
            let buf = self.ts.pool().read_buffer(BufferTag {
                rel: h.locator,
                fork: ForkNumber::Main,
                block: 0,
            });
            let buf = buf.unwrap();
            let g = buf.read().unwrap();
            g.0.to_vec()
        }

        fn records(&self) -> Vec<DecodedRecord> {
            self.ts.wal().flush(self.ts.wal().insert_lsn()).unwrap();
            let cfg = *self.ts.wal().config();
            let start = Lsn(u64::from(cfg.segment_size) + SEG_HEADER_SIZE);
            let mut r = WalReader::open(Arc::new(self.ts.vfs.clone()), &cfg, start);
            let mut out = Vec::new();
            while let Some(rec) = r.next().unwrap() {
                out.push(rec);
            }
            out
        }

        fn seq_records(&self) -> Vec<DecodedRecord> {
            self.records()
                .into_iter()
                .filter(|r| r.rmgr == RmgrId::Seq)
                .collect()
        }
    }

    fn std_params() -> SequenceParams {
        params(1, 1, 1, i64::MAX, 1, false)
    }

    #[test]
    fn init_page_matches_the_documented_bytes() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        let b = r.page_bytes(&h);
        assert_eq!(b.len(), 8192);
        // pd_lsn と pd_checksum を除く（§3.4）。
        assert!(b[0..8].iter().any(|v| *v != 0), "pd_lsn is set");
        assert_eq!(&b[10..12], &[0, 0]);
        assert_eq!(&b[12..14], &[0x1C, 0x00]);
        assert_eq!(&b[14..16], &[0xB8, 0x1F]);
        assert_eq!(&b[16..18], &[0xF8, 0x1F]);
        assert_eq!(&b[18..20], &[0x01, 0x20]);
        assert_eq!(&b[20..24], &[0; 4]);
        assert_eq!(&b[24..28], &[0xB8, 0x9F, 0x72, 0x00]);
        assert!(b[28..8120].iter().all(|v| *v == 0));
        assert_eq!(&b[8120..8128], &[2, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&b[8128..8144], &[0; 16]);
        assert_eq!(&b[8144..8150], &[0, 0, 0, 0, 1, 0]);
        assert_eq!(&b[8150..8152], &[3, 0]);
        assert_eq!(&b[8152..8154], &[0x00, 0x08]);
        assert_eq!(b[8154], 40);
        assert_eq!(&b[8155..8160], &[0; 5]);
        assert_eq!(&b[8160..8168], &[1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&b[8168..8176], &[0; 8]);
        assert_eq!(b[8176], 0);
        assert_eq!(&b[8177..8184], &[0; 7]);
        assert_eq!(&b[8184..8188], &[0x17, 0x17, 0, 0]);
        assert_eq!(&b[8188..8192], &[0; 4]);
        r.ts.assert_clean();
    }

    #[test]
    fn first_nextval_bytes() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        let run = r.store.fetch(&h, 1).unwrap();
        assert_eq!((run.first, run.count, run.increment), (1, 1, 1));
        let b = r.page_bytes(&h);
        assert_eq!(&b[8160..8168], &[1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&b[8168..8176], &[32, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(b[8176], 1);
        assert_eq!(r.store.read(&h).unwrap(), st(1, 32, true));
    }

    #[test]
    fn wal_record_bytes_match_the_documented_example() {
        // §3.6: rel = (1663, 5, 16400)、WAL に書く状態 (33, 0, true)。
        let rel = RelFileLocator {
            spc_oid: 1663,
            db_oid: 5,
            rel_number: RelFileNumber(16400),
        };
        let page = Page::zeroed();
        let mut rec = RecordBuilder::new(RmgrId::Seq, SEQ_LOG, Xid::INVALID);
        let b = rec.register_block(
            BufferTag {
                rel,
                fork: ForkNumber::Main,
                block: 0,
            },
            &page,
            RegFlags::WILL_INIT,
        );
        rec.block_data(b, &seq_tuple_bytes(&st(33, 0, true)));
        let bytes = encode_record(&rec, Lsn(0), true).unwrap();
        assert_eq!(bytes.len(), 113);
        assert_eq!(&bytes[0..4], &[0x71, 0, 0, 0]);
        assert_eq!(&bytes[16..24], &[0; 8]);
        assert_eq!(&bytes[24..28], &[5, 0, 1, 0]);
        assert_eq!(&bytes[28..32], &[0; 4]);
        // block_id = 0、flags = WILL_INIT | HAS_DATA = 0x06、fork = 0。
        assert_eq!(&bytes[32..36], &[0, 0x06, 0, 0]);
        assert_eq!(&bytes[36..40], &[0x7F, 0x06, 0, 0]);
        assert_eq!(&bytes[40..44], &[5, 0, 0, 0]);
        assert_eq!(&bytes[44..48], &[0x10, 0x40, 0, 0]);
        assert_eq!(&bytes[48..52], &[0; 4]);
        assert_eq!(&bytes[52..56], &[0x39, 0, 0, 0]);
        assert_eq!(&bytes[56..113], &seq_tuple_bytes(&st(33, 0, true)));
        assert_eq!(&bytes[96..104], &[0x21, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(bytes[112], 1);
    }

    #[test]
    fn nextval_writes_a_113_byte_seq_log_without_image() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        r.store.fetch(&h, 1).unwrap();
        let recs = r.seq_records();
        assert_eq!(recs.len(), 2, "init and the first fetch");
        let last = &recs[1];
        assert_eq!(last.xid, Xid::INVALID);
        assert_eq!(last.info, SEQ_LOG);
        assert_eq!(last.end.0 - last.start.0, 120);
        assert_eq!(last.blocks.len(), 1);
        assert!(last.blocks[0].will_init);
        assert!(last.blocks[0].image.is_none());
        assert!(last.main.is_empty());
        assert_eq!(last.blocks[0].data, seq_tuple_bytes(&st(33, 0, true)));
        assert_eq!(recs[0].xid, Xid(3));
        assert_eq!(
            describe(last),
            "SEQ_LOG rel 1663/5/16400 blk 0: last_value 33 log_cnt 0 is_called t"
        );
        r.ts.assert_clean();
    }

    #[test]
    fn read_state_rejects_damaged_pages() {
        let tuple = seq_tuple_bytes(&st(1, 0, false));
        let good = build_page(&tuple).unwrap();
        assert_eq!(read_state(&good, "s").unwrap(), st(1, 0, false));
        let bad = |f: &dyn Fn(&mut Page)| {
            let mut p = build_page(&tuple).unwrap();
            f(&mut p);
            read_state(&p, "s").unwrap_err()
        };
        let e = bad(&|p| p.special_area_mut()[..4].fill(0));
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        assert_eq!(e.message, "bad magic number in sequence \"s\": 00000000");
        let e = bad(&|p| {
            let _ = p.add_item(&tuple);
        });
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        let e = bad(&|p| p.item_mut(1).unwrap()[34] = 24);
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        let e = bad(&|p| p.item_mut(1).unwrap()[30] = 2);
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        let e = bad(&|p| p.item_mut(1).unwrap()[56] = 2);
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        let mut short = Page::zeroed();
        short.init_special(SEQ_SPECIAL_SIZE);
        short.special_area_mut()[..4].copy_from_slice(&SEQ_MAGIC.to_le_bytes());
        short.add_item(&tuple[..56]).unwrap();
        assert_eq!(
            read_state(&short, "s").unwrap_err().sqlstate,
            sqlstate::DATA_CORRUPTED
        );
    }

    #[test]
    fn fetch_on_a_damaged_page_is_xx001() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        {
            let buf = r.ts.pool().read_buffer(SeqStore::tag(&h)).unwrap();
            let mut g = buf.write().unwrap();
            g.page_mut().special_area_mut()[..4].fill(0);
            g.set_lsn(g.page().lsn());
        }
        let e = r.store.fetch(&h, 1).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED);
        assert_eq!(e.message, "bad magic number in sequence \"s\": 00000000");
        r.ts.assert_clean();
    }

    #[test]
    fn heap_scan_reads_the_sequence_row() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        r.store.fetch(&h, 1).unwrap();
        let mut cat = FakeCatalog::new("yuzhu");
        let def = cat.add_sequence("s", std_params());
        // add_sequence は自分で OID を決めるので、同じファイルを指すように定義を作り直す。
        let mut def = (*def).clone();
        def.locator = h.locator;
        let rel = RelHandle::from_table(&def);
        let snap = Snapshot {
            xmin: Xid(3),
            xmax: Xid(10),
            xip: vec![],
            curcid: 0,
            own_xid: None,
        };
        let mut scan = r.ts.heap().begin_scan(&rel, &snap).unwrap();
        let t = r.ts.heap().scan_next(&mut scan).unwrap().unwrap();
        assert_eq!(
            t.row,
            vec![Datum::Int8(1), Datum::Int8(32), Datum::Bool(true)]
        );
        assert_eq!(t.xmin, Xid::FROZEN);
        assert_eq!((t.tid.block, t.tid.offset), (0, 1));
        assert!(r.ts.heap().scan_next(&mut scan).unwrap().is_none());
        drop(scan);
        r.ts.assert_clean();
        let _ = table_def;
    }

    // ----- SeqStore ----------------------------------------------------------------

    #[test]
    fn fetch_state_transitions() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        let before = r.ts.wal().insert_lsn();
        let a = r.store.fetch(&h, 1).unwrap();
        let after_first = r.ts.wal().insert_lsn();
        assert!(after_first > before);
        assert_eq!(a.wal_lsn, after_first);
        let b = r.store.fetch(&h, 1).unwrap();
        assert_eq!(b.first, 2);
        assert_eq!(r.store.read(&h).unwrap(), st(2, 31, true));
        // 2 回目は WAL を書かない。wal_lsn は直近の SEQ_LOG の終端。
        assert_eq!(r.ts.wal().insert_lsn(), after_first);
        assert_eq!(b.wal_lsn, a.wal_lsn);
        r.ts.assert_clean();
    }

    #[test]
    fn fetch_error_leaves_the_page_untouched() {
        let r = Rig::new();
        let h = r.create(16400, "c3", params(1, 1, 1, 7, 3, false));
        r.store.setval(&h, 7, true).unwrap();
        let before = r.page_bytes(&h);
        let e = r.store.fetch(&h, 3).unwrap_err();
        assert_eq!(e.sqlstate.0, "2200H");
        assert_eq!(before, r.page_bytes(&h));
        r.ts.assert_clean();
    }

    #[test]
    fn dirty_is_set_by_a_fetch_without_wal() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        r.store.fetch(&h, 1).unwrap();
        r.ts.pool().flush_all_for_checkpoint().unwrap();
        assert_eq!(r.ts.pool().dirty_frames(), 0);
        let lsn = r.ts.wal().insert_lsn();
        r.store.fetch(&h, 1).unwrap();
        assert_eq!(r.ts.wal().insert_lsn(), lsn);
        assert_eq!(r.ts.pool().dirty_frames(), 1);
        r.ts.assert_clean();
    }

    #[test]
    fn setval_read_and_range() {
        let r = Rig::new();
        let h = r.create(16400, "l4", params(1, 1, 1, 32767, 1, false));
        let lsn = r.store.setval(&h, 100, false).unwrap();
        assert_eq!(lsn, r.ts.wal().insert_lsn());
        assert_eq!(r.store.read(&h).unwrap(), st(100, 0, false));
        assert_eq!(r.store.fetch(&h, 1).unwrap().first, 100);
        r.store.setval(&h, 5, true).unwrap();
        assert_eq!(r.store.read(&h).unwrap(), st(5, 0, true));
        let e = r.store.setval(&h, 32768, true).unwrap_err();
        assert_eq!(e.sqlstate.0, "22003");
        assert_eq!(
            e.message,
            "setval: value 32768 is out of bounds for sequence \"l4\" (1..32767)"
        );
        assert!(r.store.setval(&h, 0, true).is_err());
        let recs = r.seq_records();
        assert_eq!(recs.last().unwrap().xid, Xid::INVALID);
        r.ts.assert_clean();
    }

    #[test]
    fn reset_with_and_without_restart() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        assert_eq!(r.store.reset_generation(), 0);
        r.store.fetch(&h, 1).unwrap();
        r.store.reset(&h, &h.params, None).unwrap();
        assert_eq!(r.store.read(&h).unwrap(), st(1, 0, true));
        assert_eq!(r.store.reset_generation(), 1);
        r.store.reset(&h, &h.params, Some(50)).unwrap();
        assert_eq!(r.store.read(&h).unwrap(), st(50, 0, false));
        assert_eq!(r.store.reset_generation(), 2);
        // 新しい範囲の外は 22023 で、状態も世代も変わらない。
        let mut narrow = h.params;
        narrow.max = 40;
        let e = r.store.reset(&h, &narrow, None).unwrap_err();
        assert_eq!(e.sqlstate.0, "22023");
        assert_eq!(
            e.message,
            "RESTART value (50) cannot be greater than MAXVALUE (40)"
        );
        narrow.min = 100;
        narrow.max = 200;
        let e = r.store.reset(&h, &narrow, Some(10)).unwrap_err();
        assert_eq!(
            e.message,
            "RESTART value (10) cannot be less than MINVALUE (100)"
        );
        assert_eq!(r.store.read(&h).unwrap(), st(50, 0, false));
        assert_eq!(r.store.reset_generation(), 2);
        r.ts.assert_clean();
    }

    #[test]
    fn first_fetch_after_a_checkpoint_logs() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        for _ in 0..3 {
            r.store.fetch(&h, 1).unwrap();
        }
        assert_eq!(r.store.read(&h).unwrap(), st(3, 30, true));
        r.ts.wal().begin_checkpoint_online().unwrap();
        let before = r.seq_records().len();
        r.store.fetch(&h, 1).unwrap();
        assert_eq!(r.seq_records().len(), before + 1);
        assert_eq!(r.store.read(&h).unwrap(), st(4, 32, true));
        assert_eq!(
            r.seq_records().last().unwrap().blocks[0].data,
            seq_tuple_bytes(&st(36, 0, true))
        );
        // その次は書かない。
        r.store.fetch(&h, 1).unwrap();
        assert_eq!(r.seq_records().len(), before + 1);
    }

    #[test]
    fn no_force_log_knob_skips_the_log() {
        let ts = TestStorage::new();
        let store = SeqStore::new(
            Arc::clone(ts.pool()),
            Arc::clone(ts.smgr()),
            Arc::clone(ts.wal()),
        )
        .with_knobs(DebugKnobs {
            seq_no_force_log: true,
            ..DebugKnobs::default()
        });
        let r = Rig { ts, store };
        let h = r.create(16400, "s", std_params());
        r.store.fetch(&h, 1).unwrap();
        r.ts.wal().begin_checkpoint_online().unwrap();
        let before = r.seq_records().len();
        r.store.fetch(&h, 1).unwrap();
        assert_eq!(r.seq_records().len(), before);
    }

    #[test]
    fn first_fetch_after_startup_logs() {
        let r = Rig::new();
        let rel = 16400;
        let h = handle(rel, "s", std_params());
        r.ts.create_rel_logged(h.locator).unwrap();
        r.store
            .init(
                &WriteCtx {
                    xid: Xid(3),
                    cid: 0,
                },
                &h,
            )
            .unwrap();
        r.store.fetch(&h, 1).unwrap();
        r.ts.wal().flush(r.ts.wal().insert_lsn()).unwrap();
        r.ts.pool().flush_all_for_checkpoint().unwrap();
        r.ts.smgr().sync_pending().unwrap();
        let ts2 = r.ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
        let r2 = Rig::over(ts2);
        assert_eq!(r2.store.read(&h).unwrap(), st(1, 32, true));
        let before = r2.ts.wal().insert_lsn();
        r2.store.fetch(&h, 1).unwrap();
        assert!(r2.ts.wal().insert_lsn() > before);
        assert_eq!(r2.store.read(&h).unwrap(), st(2, 32, true));
    }

    #[test]
    fn ignore_foreign_wal_knob_reports_only_own_writes() {
        let ts = TestStorage::new();
        let store = SeqStore::new(
            Arc::clone(ts.pool()),
            Arc::clone(ts.smgr()),
            Arc::clone(ts.wal()),
        )
        .with_knobs(DebugKnobs {
            seq_ignore_foreign_wal: true,
            ..DebugKnobs::default()
        });
        let r = Rig { ts, store };
        let h = r.create(16400, "s", std_params());
        assert!(r.store.fetch(&h, 1).unwrap().wal_lsn > Lsn(0));
        assert_eq!(r.store.fetch(&h, 1).unwrap().wal_lsn, Lsn(0));
    }

    #[test]
    fn unlink_storage_removes_the_file() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        r.store.unlink_storage(&h).unwrap();
        r.ts.assert_clean();
        assert_eq!(
            r.ts.pool()
                .nblocks(h.locator, ForkNumber::Main)
                .unwrap_or(0),
            0
        );
    }

    // ----- 並行 --------------------------------------------------------------------

    fn run_threads(cache: i64, with_checkpoints: bool) {
        let r = Rig::new();
        let h = r.create(16400, "s", params(1, 1, 1, i64::MAX, cache, false));
        let (n, m) = (4usize, 60usize);
        let got: Mutex<Vec<i64>> = Mutex::new(Vec::new());
        std::thread::scope(|sc| {
            for _ in 0..n {
                sc.spawn(|| {
                    let mut mine = Vec::new();
                    for i in 0..m {
                        let run = r.store.fetch(&h, u32::try_from(cache).unwrap()).unwrap();
                        for k in 0..i64::from(run.count) {
                            mine.push(run.first + k * run.increment);
                        }
                        if with_checkpoints && i % 7 == 0 {
                            r.ts.wal().begin_checkpoint_online().unwrap();
                            r.ts.pool().flush_all_for_checkpoint().unwrap();
                        }
                    }
                    got.lock().unwrap().extend(mine);
                });
            }
        });
        let mut v = got.into_inner().unwrap();
        v.sort_unstable();
        let total = v.len();
        v.dedup();
        assert_eq!(v.len(), total, "duplicates");
        // CACHE 1 なら隙間もない。
        if cache == 1 {
            assert_eq!(v, (1..=i64::try_from(n * m).unwrap()).collect::<Vec<_>>());
        }
        r.ts.assert_clean();
    }

    #[test]
    fn concurrent_fetches_never_duplicate() {
        run_threads(1, false);
        run_threads(5, false);
    }

    #[test]
    fn concurrent_fetches_with_checkpoints_never_duplicate() {
        run_threads(1, true);
        run_threads(5, true);
    }

    #[test]
    fn fetch_and_a_shared_read_run_together() {
        let r = Rig::new();
        let h = r.create(16400, "s", std_params());
        std::thread::scope(|sc| {
            sc.spawn(|| {
                for _ in 0..100 {
                    r.store.fetch(&h, 1).unwrap();
                }
            });
            sc.spawn(|| {
                for _ in 0..100 {
                    let s = r.store.read(&h).unwrap();
                    assert!(s.last_value >= 1 && s.log_cnt >= 0);
                }
            });
        });
        assert_eq!(r.store.read(&h).unwrap().last_value, 100);
    }

    // ----- REDO --------------------------------------------------------------------

    fn redo_ctx(ts: &TestStorage, knobs: DebugKnobs) -> RedoCtx {
        RedoCtx {
            pool: Arc::clone(ts.pool()),
            smgr: Arc::clone(ts.smgr()),
            ext: Box::new(()),
            invalid: Mutex::new(InvalidPages::default()),
            next_oid: AtomicU32::new(0),
            knobs,
        }
    }

    fn seq_rec(rel: u32, state: &SeqState, end: u64) -> DecodedRecord {
        DecodedRecord {
            start: Lsn(end - 120),
            end: Lsn(end),
            xid: Xid::INVALID,
            rmgr: RmgrId::Seq,
            info: SEQ_LOG,
            blocks: vec![DecodedBlock {
                id: 0,
                tag: BufferTag {
                    rel: test_rel(rel),
                    fork: ForkNumber::Main,
                    block: 0,
                },
                will_init: true,
                image: None,
                data: seq_tuple_bytes(state).to_vec(),
            }],
            main: vec![],
        }
    }

    fn page_of(ts: &TestStorage, rel: u32) -> Vec<u8> {
        let buf = ts
            .pool()
            .read_buffer(BufferTag {
                rel: test_rel(rel),
                fork: ForkNumber::Main,
                block: 0,
            })
            .unwrap();
        let g = buf.read().unwrap();
        g.0.to_vec()
    }

    #[test]
    fn redo_on_an_empty_file_matches_init() {
        let a = Rig::new();
        let h = a.create(16400, "s", std_params());
        let want = a.page_bytes(&h);
        let init_lsn = u64::from_le_bytes(want[0..8].try_into().unwrap());

        let b = TestStorage::new();
        b.create_rel(test_rel(16400)).unwrap();
        b.smgr().set_recovery_mode(true);
        let ctx = redo_ctx(&b, DebugKnobs::default());
        redo(&ctx, &seq_rec(16400, &st(1, 0, false), init_lsn)).unwrap();
        let got = page_of(&b, 16400);
        // pd_checksum はまだ入らない。pd_lsn は同じ。
        assert_eq!(&got[..8], &want[..8]);
        assert_eq!(&got[10..], &want[10..]);
        b.assert_clean();
    }

    #[test]
    fn redo_overwrites_a_newer_page_unconditionally() {
        let ts = TestStorage::new();
        ts.create_rel(test_rel(16400)).unwrap();
        ts.smgr().set_recovery_mode(true);
        let ctx = redo_ctx(&ts, DebugKnobs::default());
        redo(&ctx, &seq_rec(16400, &st(100, 0, true), 5000)).unwrap();
        // ページの LSN が新しくても、古いレコードで置き換える。
        redo(&ctx, &seq_rec(16400, &st(33, 0, true), 3000)).unwrap();
        let p = Page(page_of(&ts, 16400).try_into().unwrap());
        assert_eq!(read_state(&p, "s").unwrap(), st(33, 0, true));
        assert_eq!(p.lsn(), 3000);
        ts.assert_clean();
    }

    #[test]
    fn redo_is_idempotent_and_last_wins() {
        let ts = TestStorage::new();
        ts.create_rel(test_rel(16400)).unwrap();
        ts.smgr().set_recovery_mode(true);
        let ctx = redo_ctx(&ts, DebugKnobs::default());
        let recs = [
            seq_rec(16400, &st(1, 0, false), 1000),
            seq_rec(16400, &st(33, 0, true), 2000),
            seq_rec(16400, &st(65, 0, true), 3000),
        ];
        for r in &recs {
            redo(&ctx, r).unwrap();
        }
        let once = page_of(&ts, 16400);
        redo(&ctx, &recs[2]).unwrap();
        assert_eq!(once, page_of(&ts, 16400));
        let p = Page(once.try_into().unwrap());
        assert_eq!(read_state(&p, "s").unwrap(), st(65, 0, true));
    }

    #[test]
    fn redo_creates_a_missing_file() {
        let ts = TestStorage::new();
        ts.smgr().set_recovery_mode(true);
        let ctx = redo_ctx(&ts, DebugKnobs::default());
        redo(&ctx, &seq_rec(16401, &st(1, 0, false), 1000)).unwrap();
        assert!(ts.smgr().exists(test_rel(16401), ForkNumber::Main).unwrap());
        assert!(ctx.invalid.lock().unwrap().is_empty());
        ts.assert_clean();
    }

    #[test]
    fn redo_rejects_malformed_records() {
        let ts = TestStorage::new();
        ts.create_rel(test_rel(16400)).unwrap();
        ts.smgr().set_recovery_mode(true);
        let ctx = redo_ctx(&ts, DebugKnobs::default());
        let good = seq_rec(16400, &st(1, 0, false), 1000);
        let cases: Vec<(&str, DecodedRecord)> = vec![
            (
                "info",
                DecodedRecord {
                    info: 0x10,
                    ..good.clone()
                },
            ),
            (
                "rmgr",
                DecodedRecord {
                    rmgr: RmgrId::Heap,
                    ..good.clone()
                },
            ),
            (
                "no blocks",
                DecodedRecord {
                    blocks: vec![],
                    ..good.clone()
                },
            ),
            (
                "two blocks",
                DecodedRecord {
                    blocks: vec![good.blocks[0].clone(), good.blocks[0].clone()],
                    ..good.clone()
                },
            ),
            ("will_init", {
                let mut r = good.clone();
                r.blocks[0].will_init = false;
                r
            }),
            ("data len", {
                let mut r = good.clone();
                r.blocks[0].data.pop();
                r
            }),
            ("t_hoff", {
                let mut r = good.clone();
                r.blocks[0].data[34] = 24;
                r
            }),
            ("natts", {
                let mut r = good.clone();
                r.blocks[0].data[30] = 2;
                r
            }),
        ];
        for (what, rec) in cases {
            let e = redo(&ctx, &rec).unwrap_err();
            assert_eq!(e.severity, Severity::Panic, "{what}");
            assert_eq!(e.sqlstate, sqlstate::DATA_CORRUPTED, "{what}");
        }
        ts.assert_clean();
    }

    #[test]
    fn redo_skip_knob_keeps_a_newer_page() {
        let ts = TestStorage::new();
        ts.create_rel(test_rel(16400)).unwrap();
        ts.smgr().set_recovery_mode(true);
        let ctx = redo_ctx(&ts, DebugKnobs::default());
        redo(&ctx, &seq_rec(16400, &st(100, 0, true), 5000)).unwrap();
        let ctx = redo_ctx(
            &ts,
            DebugKnobs {
                seq_redo_skip_if_page_newer: true,
                ..DebugKnobs::default()
            },
        );
        redo(&ctx, &seq_rec(16400, &st(33, 0, true), 3000)).unwrap();
        let p = Page(page_of(&ts, 16400).try_into().unwrap());
        assert_eq!(read_state(&p, "s").unwrap(), st(100, 0, true));
    }

    /// 払い出した値は、クラッシュ後の REDO で最後の `SEQ_LOG` の状態（先）に戻る（§5.10）。
    #[test]
    fn crash_replay_never_goes_back() {
        let r = Rig::new();
        let h = handle(16400, "s", std_params());
        r.ts.create_rel_logged(h.locator).unwrap();
        r.store
            .init(
                &WriteCtx {
                    xid: Xid(3),
                    cid: 0,
                },
                &h,
            )
            .unwrap();
        for _ in 0..3 {
            r.store.fetch(&h, 1).unwrap();
        }
        r.ts.wal().flush(r.ts.wal().insert_lsn()).unwrap();
        r.ts.smgr().sync_pending().unwrap();
        let recs = r.records();
        let ts2 = r.ts.crash_and_reopen(CrashMode::DropUnsynced).unwrap();
        ts2.smgr().set_recovery_mode(true);
        let ctx = redo_ctx(&ts2, DebugKnobs::default());
        for rec in recs.iter().filter(|x| x.rmgr == RmgrId::Seq) {
            redo(&ctx, rec).unwrap();
        }
        ts2.smgr().set_recovery_mode(false);
        let r2 = Rig::over(ts2);
        assert_eq!(r2.store.read(&h).unwrap(), st(33, 0, true));
        assert_eq!(r2.store.fetch(&h, 1).unwrap().first, 34);
    }

    #[test]
    fn describe_reads_the_tuple() {
        let rec = seq_rec(16400, &st(33, 0, true), 0x20_0098);
        assert_eq!(
            describe(&rec),
            "SEQ_LOG rel 1663/5/16400 blk 0: last_value 33 log_cnt 0 is_called t"
        );
        let rec = DecodedRecord { info: 0x10, ..rec };
        assert_eq!(describe(&rec), "UNKNOWN 0x10");
    }
}
