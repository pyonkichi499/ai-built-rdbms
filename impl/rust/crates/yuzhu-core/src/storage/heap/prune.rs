//! プルーニング: `satisfies_vacuum`、計画の作成（`scan_page`）と適用（`apply_plan`）、
//! WAL 付きの `prune_and_log`、機会的 pruning（`m5/03` §5.1、§5.2、VC-1）。
//!
//! 排他ラッチだけで行う（VC-D1）。ページを変える処理は「計画を作る（読み取りだけ）」と
//! 「計画を適用する（通常の経路と REDO が同じ `apply_plan`）」に分ける。

use super::tuple::TupleHeader;
use super::wal2::{HEAP2_PRUNE_FREEZE, PruneMain, encode_plan_data};
use super::xmax::{
    XmaxEnv, XmaxState, decode_xmax, invalidate_xmax, lockers_all_finished, with_xmin_frozen,
    xmin_is_frozen,
};
use crate::error::{Error, Result};
use crate::storage::buffer::PinnedBuffer;
use crate::storage::buffer::{BufferPool, CriticalSection, PageReadGuard, PageWriteGuard};
use crate::storage::page::{LpFlags, Page, PageError};
use crate::storage::smgr::{BlockNumber, BufferTag, ForkNumber, RelFileLocator};
use crate::txn::Xid;
use crate::txn::clog::{Clog, XidStatus};
use crate::txn::multixact::MultiXactTable;
use crate::txn::proc_array::ProcArray;
use crate::wal::{Lsn, RecordBuilder, RegFlags, RmgrId, Wal};

/// 機会的 pruning を起こす空きの下限（`BLCKSZ / 10`。PG の `minfree`）。これ以上空いていれば何もしない。
pub const PRUNE_MIN_FREE: usize = 819;

// ---------------------------------------------------------------------------
// RunningXids / HeapEnv
// ---------------------------------------------------------------------------

/// 実行中の XID の一覧への窓口。`txn` は `storage` を `use` できないので、トレイトはここに置き、
/// `ProcArray` への `impl` も VC-1 が書く（`m5/03` §4.1、R-11）。
pub trait RunningXids: std::fmt::Debug + Send + Sync {
    /// `x` が実行中か（一覧にあるか）。
    fn is_in_progress(&self, x: Xid) -> bool;
    fn next_xid(&self) -> Xid;
    /// 登録済みスナップショットを含む horizon（Mutex を取る）。
    fn oldest_xmin(&self) -> Xid;
    /// `oldest_xmin` の下限の写し。ロックなし。単調に増える。
    fn oldest_xmin_hint(&self) -> Xid;
}

/// `ProcArray` の固有メソッドへの 1 行ずつの委譲（`m5/03` §4.1、LK-D28）。
impl RunningXids for ProcArray {
    fn is_in_progress(&self, x: Xid) -> bool {
        ProcArray::is_in_progress(self, x)
    }

    fn next_xid(&self) -> Xid {
        ProcArray::next_xid(self)
    }

    fn oldest_xmin(&self) -> Xid {
        ProcArray::oldest_xmin(self)
    }

    fn oldest_xmin_hint(&self) -> Xid {
        ProcArray::oldest_xmin_hint(self)
    }
}

/// 可視性（vacuum 用）の判定に要る外部の状態。
#[derive(Clone, Copy, Debug)]
pub struct HeapEnv<'a> {
    pub clog: &'a Clog,
    pub procs: &'a dyn RunningXids,
    pub multixact: &'a MultiXactTable,
}

// ---------------------------------------------------------------------------
// satisfies_vacuum
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum XidState {
    Running,
    Committed,
    Aborted,
}

/// XID の終了状態（5.1）。実行中の一覧を clog より先に見る（コミットは clog → 一覧から外す
/// の順。00 D38）。`x < horizon` なら終了済みなので一覧は引かない。
fn xid_state(env: &HeapEnv<'_>, x: Xid, horizon: Xid) -> Result<XidState> {
    if !x.is_normal() {
        return Ok(XidState::Committed);
    }
    if x >= horizon && env.procs.is_in_progress(x) {
        return Ok(XidState::Running);
    }
    Ok(match env.clog.status(x)? {
        XidStatus::Committed => XidState::Committed,
        // 一覧になく clog が IN_PROGRESS のもの（クラッシュの残り）は中断。
        XidStatus::Aborted | XidStatus::InProgress => XidState::Aborted,
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VacTuple {
    Dead,
    RecentlyDead,
    Live,
    InsertInProgress,
    DeleteInProgress,
}

/// `HeapTupleSatisfiesVacuum`（5.1）。`horizon` は `oldest_xmin()`。
pub fn satisfies_vacuum(env: &HeapEnv<'_>, t: &TupleHeader, horizon: Xid) -> Result<VacTuple> {
    if !xmin_is_frozen(t) {
        match xid_state(env, t.xmin, horizon)? {
            XidState::Aborted => return Ok(VacTuple::Dead),
            XidState::Running => return Ok(VacTuple::InsertInProgress),
            XidState::Committed => {}
        }
    }
    match decode_xmax(t)? {
        // ロックだけの xmax は行の生死に関係しない（古ければ 5.2.1 が無効化する）。
        XmaxState::None | XmaxState::Locker { .. } | XmaxState::LockerMulti(_) => {
            Ok(VacTuple::Live)
        }
        XmaxState::Updater { xid, .. } => Ok(match xid_state(env, xid, horizon)? {
            XidState::Running => VacTuple::DeleteInProgress,
            // 中断した更新者の xmax は無効（5.2.1 が無効化する）。
            XidState::Aborted => VacTuple::Live,
            XidState::Committed if xid < horizon => VacTuple::Dead,
            XidState::Committed => VacTuple::RecentlyDead,
        }),
    }
}

// ---------------------------------------------------------------------------
// 計画
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct PruneParams {
    pub horizon: Xid,
    /// `Some` なら、この XID 未満のコミット済みの xmin を凍結し、この XID 未満の古い xmax を無効化する
    /// （VACUUM）。`None` は凍結しない（機会的）。
    pub freeze_cutoff: Option<Xid>,
    /// `LP_DEAD` にする代わりに直接 `LP_UNUSED` にする（インデックスのないテーブルの VACUUM）。
    /// 既存の `LP_DEAD` も `LP_UNUSED` にする。
    pub mark_unused_now: bool,
    /// ANALYZE を外側のトランザクションで動かすときの自分の XID（自分が挿入した行を live として数える）。
    pub own_xid: Option<Xid>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FreezeEntry {
    pub offnum: u16,
    /// 適用後の絶対値。
    pub infomask: u16,
    pub infomask2: u16,
    pub clear_xmax: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrunePlan {
    pub dead: Vec<u16>,
    pub unused: Vec<u16>,
    pub frozen: Vec<FreezeEntry>,
    /// 適用後の `pd_prune_xid`（下位 32 ビット。0 = なし）。
    pub prune_xid: u32,
    /// 計画を作ったときのページの `pd_prune_xid`。`is_empty` が「変わらない」を判定するために持つ
    /// （`Default` は 0 で、`prune_xid` の既定と一致する）。
    pub base_prune_xid: u32,
}

impl PrunePlan {
    /// 3 つの配列が空で、`prune_xid` がページの現在の値と同じとき真。
    pub fn is_empty(&self) -> bool {
        self.dead.is_empty()
            && self.unused.is_empty()
            && self.frozen.is_empty()
            && self.prune_xid == self.base_prune_xid
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageCounts {
    /// `LIVE` + `DELETE_IN_PROGRESS`（+ 自分の `INSERT_IN_PROGRESS`）。
    pub live: u32,
    /// 「dead but not yet removable」。
    pub recently_dead: u32,
    pub insert_in_progress: u32,
    /// この計画で領域を回収する（`LP_DEAD` / `LP_UNUSED` にする）タプル。
    pub made_dead: u32,
    pub frozen: u32,
    /// 適用後の `LP_DEAD` の数（既にあったものを含む）。
    pub lpdead_total: u32,
}

#[derive(Clone, Debug)]
pub struct PageScan {
    pub plan: PrunePlan,
    pub counts: PageCounts,
    /// 適用後の `LP_DEAD` の行ポインタ（昇順。`mark_unused_now` なら空）。
    pub lpdead_after: Vec<u16>,
}

fn page_err(e: PageError) -> Error {
    Error::corrupted(format!("invalid page in a heap relation: {e}"))
}

/// 読み取りだけ（共有ラッチでも呼べる）。計画と数え上げを返す（5.2.1）。
pub fn scan_page(env: &HeapEnv<'_>, page: &Page, params: &PruneParams) -> Result<PageScan> {
    let mut out = PageScan {
        plan: PrunePlan::default(),
        counts: PageCounts::default(),
        lpdead_after: Vec::new(),
    };
    if page.is_new() {
        return Ok(out);
    }
    out.plan.base_prune_xid = page.prune_xid();
    let mut prunable: Option<Xid> = None;
    let mut note_prunable = |x: Xid| prunable = Some(prunable.map_or(x, |p| p.min(x)));

    for off in 1..=page.max_offset() {
        let id = page.item_id(off).map_err(page_err)?;
        match id.flags {
            LpFlags::Unused => {}
            LpFlags::Redirect => {
                return Err(Error::corrupted(format!(
                    "unexpected LP_REDIRECT at offset {off} of a heap page"
                )));
            }
            LpFlags::Dead => {
                if params.mark_unused_now {
                    out.plan.unused.push(off);
                } else {
                    out.counts.lpdead_total += 1;
                    out.lpdead_after.push(off);
                }
            }
            LpFlags::Normal => {
                let hdr = TupleHeader::read(page.item(off).map_err(page_err)?)?;
                let verdict = satisfies_vacuum(env, &hdr, params.horizon)?;
                match verdict {
                    VacTuple::Dead => {
                        out.counts.made_dead += 1;
                        if params.mark_unused_now {
                            out.plan.unused.push(off);
                        } else {
                            out.plan.dead.push(off);
                            out.lpdead_after.push(off);
                            out.counts.lpdead_total += 1;
                        }
                        continue;
                    }
                    VacTuple::RecentlyDead => {
                        out.counts.recently_dead += 1;
                        note_prunable(hdr.xmax);
                    }
                    VacTuple::Live => out.counts.live += 1,
                    VacTuple::InsertInProgress => {
                        if params.own_xid == Some(hdr.xmin) {
                            out.counts.live += 1;
                        } else {
                            out.counts.insert_in_progress += 1;
                        }
                        continue;
                    }
                    VacTuple::DeleteInProgress => {
                        out.counts.live += 1;
                        note_prunable(hdr.xmax);
                    }
                }
                if let Some(cutoff) = params.freeze_cutoff
                    && let Some(e) = plan_freeze(env, &hdr, off, cutoff, params.horizon)?
                {
                    out.plan.frozen.push(e);
                }
            }
        }
    }
    out.plan.prune_xid = prunable.map_or(0, Xid::to_external);
    out.counts.frozen = u32::try_from(out.plan.frozen.len()).unwrap_or(u32::MAX);
    // `lpdead_after` was filled in line pointer order, so it is ascending.
    Ok(out)
}

/// 凍結の計画（5.2.1）。`Dead` と `InsertInProgress` 以外のタプルについて呼ぶ。
fn plan_freeze(
    env: &HeapEnv<'_>,
    h: &TupleHeader,
    off: u16,
    cutoff: Xid,
    horizon: Xid,
) -> Result<Option<FreezeEntry>> {
    let (mut infomask, mut infomask2, mut clear_xmax) = (h.infomask, h.infomask2, false);
    let invalidate = match decode_xmax(h)? {
        XmaxState::None => false,
        XmaxState::Locker { xid, .. } => xid < cutoff,
        // Every member has finished (an unknown or old MultiXactId expands to `Dead`).
        XmaxState::LockerMulti(_) => {
            let is_running = |x: Xid| x >= horizon && env.procs.is_in_progress(x);
            let xenv = XmaxEnv {
                me: Xid::INVALID,
                is_running: &is_running,
                multi: env.multixact,
            };
            lockers_all_finished(h, &xenv)?
        }
        XmaxState::Updater { xid, .. } => {
            xid < cutoff && xid_state(env, xid, horizon)? == XidState::Aborted
        }
    };
    if invalidate {
        let w = invalidate_xmax(h);
        infomask = w.infomask;
        infomask2 = w.infomask2;
        clear_xmax = h.xmax != Xid::INVALID;
    }
    if !xmin_is_frozen(h) && h.xmin < cutoff {
        infomask = with_xmin_frozen(infomask);
    }
    if infomask == h.infomask && infomask2 == h.infomask2 && !clear_xmax {
        return Ok(None);
    }
    Ok(Some(FreezeEntry {
        offnum: off,
        infomask,
        infomask2,
        clear_xmax,
    }))
}

/// 計画を適用する。通常の経路と REDO が同じ関数を使う（3.5 の順序）:
/// `dead` → `unused` → `frozen` → `repair_fragmentation` → `pd_prune_xid`。
///
/// REDO の冪等性のため、`dead` は既に `LP_DEAD` でもよく、`unused` は既に `LP_UNUSED`
/// （末尾が外れて範囲外になったものを含む）でもよい。`frozen` は `LP_NORMAL` でなければならない。
/// ページを変える。呼び出し側は `CriticalSection` と WAL を担当する。
pub fn apply_plan(page: &mut Page, plan: &PrunePlan) -> Result<()> {
    for &off in &plan.dead {
        match page.item_id(off).map_err(page_err)?.flags {
            LpFlags::Normal | LpFlags::Dead => page.mark_dead(off),
            f => return Err(bad_state("dead", off, f)),
        }
    }
    for &off in &plan.unused {
        if off > page.max_offset() {
            continue;
        }
        match page.item_id(off).map_err(page_err)?.flags {
            LpFlags::Normal | LpFlags::Dead | LpFlags::Unused => page.mark_unused(off),
            f @ LpFlags::Redirect => return Err(bad_state("unused", off, f)),
        }
    }
    for e in &plan.frozen {
        let id = page.item_id(e.offnum).map_err(page_err)?;
        if id.flags != LpFlags::Normal {
            return Err(bad_state("frozen", e.offnum, id.flags));
        }
        let item = page.item_mut(e.offnum).map_err(page_err)?;
        let mut h = TupleHeader::read(item)?;
        h.infomask = e.infomask;
        h.infomask2 = e.infomask2;
        if e.clear_xmax {
            h.xmax = Xid::INVALID;
        }
        h.write(item);
    }
    page.repair_fragmentation().map_err(page_err)?;
    page.set_prune_xid(plan.prune_xid);
    Ok(())
}

fn bad_state(what: &str, off: u16, f: LpFlags) -> Error {
    Error::corrupted(format!(
        "cannot apply the prune plan: {what} entry at offset {off} is {f:?}"
    ))
}

/// WAL を書いてページに適用する（M3 §5.1 の規約 1）。排他ラッチを持った呼び出し側が呼ぶ。
/// 計画が空なら何もしない。
pub fn prune_and_log(
    pool: &BufferPool,
    wal: &Wal,
    rel: RelFileLocator,
    blk: BlockNumber,
    guard: &mut PageWriteGuard<'_>,
    plan: &PrunePlan,
) -> Result<Option<Lsn>> {
    if plan.is_empty() {
        return Ok(None);
    }
    // 変異テスト用: 凍結を WAL に書かない（ページには適用する）。
    let logged;
    let logged_plan = if wal.config().knobs.freeze_skip_wal && !plan.frozen.is_empty() {
        logged = PrunePlan {
            frozen: Vec::new(),
            ..plan.clone()
        };
        &logged
    } else {
        plan
    };
    let tag = BufferTag {
        rel,
        fork: ForkNumber::Main,
        block: blk,
    };
    let cs = CriticalSection::enter(pool);
    apply_plan(guard.page_mut(), plan).map_err(|e| cs.escalate(e))?;
    let end = {
        let mut rec = RecordBuilder::new(RmgrId::Heap2, HEAP2_PRUNE_FREEZE, Xid::INVALID);
        let b = rec.register_block(tag, guard.page(), RegFlags::STANDARD);
        rec.block_data(b, &encode_plan_data(logged_plan));
        rec.main_data(&PruneMain::of(logged_plan).encode());
        wal.insert(rec).map_err(|e| cs.escalate(e))?.end
    };
    guard.set_lsn(end.0);
    Ok(Some(end))
}

// ---------------------------------------------------------------------------
// 機会的 pruning
// ---------------------------------------------------------------------------

/// 機会的 pruning の起動条件（5.2.3）。真なら使う horizon を返す。
///
/// `procs.oldest_xmin()` は proc の Mutex を取るので、安い条件と `oldest_xmin_hint`
/// （ロックなしの下限）を満たしたときだけ呼ぶ。
pub fn candidate(page: &Page, procs: &dyn RunningXids, next_xid: Xid) -> Option<Xid> {
    if page.is_new() || page.prune_xid() == 0 || page.free_space() >= PRUNE_MIN_FREE {
        return None;
    }
    let x = page.prune_xid_full(next_xid)?;
    let hint = procs.oldest_xmin_hint();
    if x < hint {
        return Some(hint);
    }
    let h = procs.oldest_xmin();
    (x < h).then_some(h)
}

/// 機会的 pruning（VC-D7）。`candidate` を確かめたうえで呼ぶ。変更したら `true`。
/// 凍結はしない（`freeze_cutoff = None`）。
#[allow(clippy::too_many_arguments)]
pub fn prune_opt(
    env: &HeapEnv<'_>,
    pool: &BufferPool,
    wal: &Wal,
    rel: RelFileLocator,
    blk: BlockNumber,
    guard: &mut PageWriteGuard<'_>,
    horizon: Xid,
    _next_xid: Xid,
) -> Result<bool> {
    let params = PruneParams {
        horizon,
        freeze_cutoff: None,
        mark_unused_now: false,
        own_xid: None,
    };
    let scan = scan_page(env, guard.page(), &params)?;
    Ok(prune_and_log(pool, wal, rel, blk, guard, &scan.plan)?.is_some())
}

/// スキャンと TID での取得が機会的 pruning に使う部品。
#[derive(Clone, Copy, Debug)]
pub struct PruneCtx<'a> {
    pub env: HeapEnv<'a>,
    pub wal: &'a Wal,
}

/// ページを読む直前の機会的 pruning（5.2.3 の表の「スキャン・TID での取得」）。
///
/// 共有ラッチ `guard` を受け取り、`candidate` が偽ならそのまま返す。真なら共有ラッチを外して
/// `try_write` する。取れなければ諦め、取れたら `candidate` を確かめ直して `prune_opt` する。
/// どちらでも共有ラッチを取り直して返す（呼び出し側は返されたガードの内容を読む）。
pub fn prune_before_read<'a>(
    ctx: &PruneCtx<'_>,
    pool: &BufferPool,
    pinned: &'a PinnedBuffer,
    guard: PageReadGuard<'a>,
) -> Result<PageReadGuard<'a>> {
    if pool.is_poisoned() || guard.prune_xid() == 0 || guard.free_space() >= PRUNE_MIN_FREE {
        return Ok(guard);
    }
    let next_xid = ctx.env.procs.next_xid();
    if candidate(&guard, ctx.env.procs, next_xid).is_none() {
        return Ok(guard);
    }
    drop(guard);
    if let Some(mut w) = pinned.try_write()?
        && let Some(horizon) = candidate(w.page(), ctx.env.procs, next_xid)
    {
        let tag = pinned.tag();
        prune_opt(
            &ctx.env, pool, ctx.wal, tag.rel, tag.block, &mut w, horizon, next_xid,
        )?;
    }
    pinned.read()
}

#[cfg(test)]
mod tests;
