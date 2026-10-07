//! 不変条件 I1〜I12 の検査関数（m3-recovery §9.5、`m3.md` §7.5）。
//!
//! 検査は 2 段階。
//!
//! 1. クラッシュ後・リカバリ前（[`pre_recovery_scan`]）: 制御ファイルと WAL を独立の実装で読み、
//!    コミット・アボートのレコード（I7）、WAL の末尾、ページの `page_lsn <= WAL の末尾`（I2）を調べる。
//! 2. リカバリ後（[`check_after_recovery`]）: clog と WAL の一致（I7）、実行中だった xid（I6）、
//!    全テーブルの内容とモデルの一致（I1・I2・I3・I4）、`next_xid`（I5）、ページ構造・制御ファイル・
//!    カタログのファイル（I8）。
//!
//! 違反は [`Violation`] で返し、`inv` に番号を入れる（変異テストが「どの不変条件で検出したか」を見る）。
//! I9〜I12 はクラッシュを重ねる側（`main.rs`）が、この関数をくり返し呼んで確かめる。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use yuzhu_core::catalog::RelKind;
use yuzhu_core::catalog::check::{check_catalog, check_orphan_files};
use yuzhu_core::executor::seq::handle_from_def;
use yuzhu_core::storage::btree::BT_SPECIAL_SIZE;
use yuzhu_core::storage::btree::check::{
    CheckError, HeapCheck, check_against_heap, check_structure as btree_check_structure,
};
use yuzhu_core::storage::page::{LpFlags, Page};
use yuzhu_core::storage::sequence::{SEQ_LOG_VALS, SEQ_SPECIAL_SIZE, read_state};
use yuzhu_core::storage::smgr::{BufferTag, ForkNumber};
use yuzhu_core::storage::vfs::{OpenMode, SimVfs, Vfs};
use yuzhu_core::storage::{BLCKSZ, heap::tuple::TupleHeader};
use yuzhu_core::storage::{IndexHandle, RelHandle};
use yuzhu_core::testing::{run_sql, user_relation_defs};
use yuzhu_core::txn::Xid;
use yuzhu_core::txn::clog::XidStatus;
use yuzhu_core::txn::xact_wal::{XACT_ABORT, XACT_COMMIT};
use yuzhu_core::wal::xlog::CheckpointRecord;
use yuzhu_core::wal::{Lsn, RmgrId, WalConfig, WalReader};
use yuzhu_core::{Cluster, DebugKnobs, Session, StartupParams};

use crate::model::{
    DdlModel, DdlRel, Model, SeqModel, SeqOp, Table, Tables, TxnLog, find_candidate_with,
    has_missing,
};

/// 違反した不変条件。
#[derive(Clone, Debug)]
pub(crate) struct Violation {
    /// `"I1"`、`"I4"`、`"I8"`、`"startup"` など。
    pub(crate) inv: &'static str,
    pub(crate) msg: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.inv, self.msg)
    }
}

pub(crate) fn violation(inv: &'static str, msg: impl Into<String>) -> Violation {
    Violation {
        inv,
        msg: msg.into(),
    }
}

pub(crate) type V<T> = Result<T, Violation>;

/// クラッシュ後・リカバリ前の WAL とディスクについて分かったこと。
#[derive(Clone, Debug, Default)]
pub(crate) struct PreScan {
    /// REDO 開始点から読めた COMMIT / ABORT レコードの xid。
    pub(crate) commits: BTreeSet<u64>,
    pub(crate) aborts: BTreeSet<u64>,
    /// WAL に現れた最大の xid。
    pub(crate) max_wal_xid: u64,
    /// 有効なレコードが終わる位置。
    pub(crate) wal_end: u64,
}

fn as_dyn(vfs: &SimVfs) -> Arc<dyn Vfs> {
    Arc::new(vfs.clone())
}

/// リレーションのファイル（`global/NNN`、`base/DB/NNN`、`.N` つきを含む）。フォーク
/// （`_fsm` など）は含めない。
fn relation_files(vfs: &SimVfs) -> V<Vec<PathBuf>> {
    let list = |p: &str| -> V<Vec<PathBuf>> {
        vfs.read_dir(Path::new(p))
            .map_err(|e| violation("I8", format!("cannot list {p}: {e}")))
    };
    let mut dirs = vec![PathBuf::from("global")];
    for d in list("base")? {
        dirs.push(d);
    }
    let mut files = Vec::new();
    for d in dirs {
        let Ok(entries) = vfs.read_dir(&d) else {
            continue;
        };
        for f in entries {
            let name = f
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            let stem = name.split('.').next().unwrap_or_default();
            let numeric = !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit());
            let seg_ok = name
                .split_once('.')
                .is_none_or(|(_, s)| s.chars().all(|c| c.is_ascii_digit()));
            if numeric && seg_ok {
                files.push(f);
            }
        }
    }
    Ok(files)
}

/// ファイルの全ページ。`(ブロック番号, ページ)`。ブロック番号はセグメントをまたいだ通し番号ではなく
/// セグメント内の番号（検査にはチェックサムの計算にだけ使うので、セグメント番号を足す）。
fn read_pages(vfs: &SimVfs, path: &Path, rel_seg_blocks: u32) -> V<Vec<(u32, Page)>> {
    let seg: u32 = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| e.parse().ok())
        .unwrap_or(0);
    let f = vfs
        .open(path, OpenMode::ReadOnly)
        .map_err(|e| violation("I8", format!("cannot open {}: {e}", path.display())))?;
    let size = f
        .size()
        .map_err(|e| violation("I8", format!("cannot stat {}: {e}", path.display())))?;
    let mut out = Vec::new();
    for i in 0..(size / BLCKSZ as u64) {
        let mut page = Page::zeroed();
        f.read_exact_at(&mut page.0, i * BLCKSZ as u64)
            .map_err(|e| violation("I8", format!("cannot read {}: {e}", path.display())))?;
        let blkno = seg * rel_seg_blocks + u32::try_from(i).unwrap_or(u32::MAX);
        out.push((blkno, page));
    }
    Ok(out)
}

/// 実行時に作ったページと、REDO で作ったページの物理的な一致（PostgreSQL の
/// `wal_consistency_checking` 相当）。`runtime` は障害なしで動かしたあと全ページを書き出した
/// ディスク、`redone` は同じ WAL をリカバリして全ページを書き出したディスク。
///
/// 比べるのは、ページの `lsn`・`lower`・`upper`・`special`、各行ポインタの状態と長さ、各タプルの
/// 内容（ヒントビット `XMIN/XMAX_COMMITTED|INVALID` は実行時にしか立たないので無視）。
/// 行ポインタの位置（`off`）と空き領域の中身は比べない。索引のページ（special 領域あり）は項目をバイト単位で比べ、
/// シーケンスのページは内容を比べない（WAL に載らない払い出しで実行時のほうが進む。I15 が調べる）。
pub(crate) fn compare_physical(runtime: &SimVfs, redone: &SimVfs, rel_seg_blocks: u32) -> V<()> {
    const HINTS: u16 = 0x0100 | 0x0200 | 0x0400 | 0x0800;
    let bad = |at: &str, what: String| violation("IP", format!("{at}: {what}"));
    for f in relation_files(runtime)? {
        let a = read_pages(runtime, &f, rel_seg_blocks)?;
        let b: std::collections::BTreeMap<u32, Page> = if redone.exists(&f).unwrap_or(false) {
            read_pages(redone, &f, rel_seg_blocks)?
                .into_iter()
                .collect()
        } else {
            std::collections::BTreeMap::new()
        };
        for (blkno, pa) in a {
            if pa.is_new() {
                continue;
            }
            let at = format!("{} block {blkno}", f.display());
            let Some(pb) = b.get(&blkno).filter(|p| !p.is_new()) else {
                return Err(bad(
                    &at,
                    "the page exists at runtime but not after REDO".into(),
                ));
            };
            for (name, x, y) in [
                ("lsn", pa.lsn(), pb.lsn()),
                ("lower", u64::from(pa.lower()), u64::from(pb.lower())),
                ("upper", u64::from(pa.upper()), u64::from(pb.upper())),
                ("special", u64::from(pa.special()), u64::from(pb.special())),
            ] {
                if x != y {
                    return Err(bad(&at, format!("{name} differs: runtime {x}, REDO {y}")));
                }
            }
            let special_len = pa.special_area().len();
            if pa.special_area() != pb.special_area() {
                return Err(bad(&at, "the special area differs".into()));
            }
            // シーケンスのページ: 実行時は WAL に載らない払い出し（キャッシュ）の分だけ内容が先に進む。
            // 内容は I15 が調べるので、ここでは LSN・各オフセット・special だけを比べる。
            if special_len == SEQ_SPECIAL_SIZE {
                continue;
            }
            // 索引のページ: 項目をバイト単位で比べる（ヒントビットなどの実行時だけの差はない）。メタページは本体も。
            let is_index = special_len == BT_SPECIAL_SIZE;
            if is_index && pa.max_offset() == 0 && pa.body() != pb.body() {
                return Err(bad(&at, "the page body differs (meta page)".into()));
            }
            for off in 1..=pa.max_offset() {
                let (ia, ib) = (
                    pa.item_id(off).map_err(|e| bad(&at, e.to_string()))?,
                    pb.item_id(off).map_err(|e| bad(&at, e.to_string()))?,
                );
                if ia.flags != ib.flags || ia.len != ib.len {
                    return Err(bad(
                        &at,
                        format!("item {off}: line pointer differs: runtime {ia:?}, REDO {ib:?}"),
                    ));
                }
                if ia.flags != LpFlags::Normal {
                    continue;
                }
                if is_index {
                    let (ta, tb) = (
                        pa.item(off).map_err(|e| bad(&at, e.to_string()))?,
                        pb.item(off).map_err(|e| bad(&at, e.to_string()))?,
                    );
                    if ta != tb {
                        return Err(bad(&at, format!("item {off}: index tuple differs")));
                    }
                    continue;
                }
                let (ta, tb) = (
                    pa.item(off).map_err(|e| bad(&at, e.to_string()))?,
                    pb.item(off).map_err(|e| bad(&at, e.to_string()))?,
                );
                let (ha, hb) = (
                    TupleHeader::read(ta).map_err(|e| bad(&at, e.message))?,
                    TupleHeader::read(tb).map_err(|e| bad(&at, e.message))?,
                );
                let same = ha.xmin == hb.xmin
                    && ha.xmax == hb.xmax
                    && ha.cmin == hb.cmin
                    && ha.cmax == hb.cmax
                    && ha.ctid == hb.ctid
                    && ha.infomask2 == hb.infomask2
                    && ha.infomask & !HINTS == hb.infomask & !HINTS
                    && ha.hoff == hb.hoff
                    && ta[35..] == tb[35..];
                if !same {
                    return Err(bad(
                        &at,
                        format!("item {off}: tuple differs: runtime {ha:?}, REDO {hb:?}"),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn wal_config(c: &yuzhu_core::control::ControlData) -> WalConfig {
    WalConfig {
        segment_size: c.wal_segment_size,
        system_identifier: c.system_identifier,
        full_page_writes: c.flags & yuzhu_core::control::FLAG_FULL_PAGE_WRITES != 0,
        knobs: DebugKnobs::default(),
    }
}

/// 制御ファイルと、REDO 開始点からの WAL を読む。ページの `page_lsn` が WAL の末尾を超えて
/// いないこと（WAL 先行の原則、I2）も調べる。リカバリ前のディスク（`crash` が返したもの）に対して呼ぶ。
pub(crate) fn pre_recovery_scan(vfs: &SimVfs) -> V<PreScan> {
    let dyn_vfs = as_dyn(vfs);
    let control = yuzhu_core::control::ControlFileHandle::open(&dyn_vfs)
        .map_err(|e| violation("startup", format!("control file unreadable: {}", e.message)))?;
    let data = control.get();
    let cfg = wal_config(&data);
    let mut scan = PreScan {
        ..PreScan::default()
    };
    let mut reader = WalReader::open(Arc::clone(&dyn_vfs), &cfg, Lsn(data.redo_lsn));
    loop {
        let rec = reader
            .next()
            .map_err(|e| violation("startup", format!("WAL unreadable: {}", e.message)))?;
        let Some(rec) = rec else { break };
        if rec.xid.is_normal() {
            scan.max_wal_xid = scan.max_wal_xid.max(rec.xid.0);
        }
        if rec.rmgr == RmgrId::Xact {
            match rec.info {
                XACT_COMMIT => {
                    scan.commits.insert(rec.xid.0);
                }
                XACT_ABORT => {
                    scan.aborts.insert(rec.xid.0);
                }
                _ => {}
            }
        }
    }
    scan.wal_end = reader.end_of_wal().map_or(data.redo_lsn, |(l, _)| l.0);

    // WAL 先行の原則: 壊れていないページの LSN が、読める WAL の末尾より先を指してはならない。
    for f in relation_files(vfs)? {
        for (blkno, page) in read_pages(vfs, &f, data.rel_seg_blocks)? {
            if page.verify(blkno).is_ok() && page.lsn() > scan.wal_end {
                return Err(violation(
                    "I2",
                    format!(
                        "{} block {blkno}: page_lsn {} is beyond the end of the WAL {} (WAL-before-data broken)",
                        f.display(),
                        page.lsn(),
                        scan.wal_end
                    ),
                ));
            }
        }
    }
    Ok(scan)
}

/// クラッシュ前に調べておくこと。
#[derive(Clone, Debug, Default)]
pub(crate) struct PreCrash {
    /// クラッシュの時に実行中だったトランザクションの xid（I6）。不明なトランザクションがあれば
    /// その xid と区別できないので空にする。
    pub(crate) in_progress: Vec<u64>,
}

/// 実行中の xid を集める（`abandon` の前に呼ぶ）。
pub(crate) fn pre_crash(cluster: &Arc<Cluster>, has_unknown: bool) -> PreCrash {
    if has_unknown {
        return PreCrash::default();
    }
    let mgr = cluster.txn_manager();
    let next = mgr.next_xid().0;
    PreCrash {
        in_progress: (Xid::FIRST_NORMAL.0..next)
            .filter(|x| mgr.is_in_progress(Xid(*x)))
            .collect(),
    }
}

/// リカバリ後の全検査。成功したら、次の続きのモデル（不明なものを確定した形）を返す。
///
/// 最後に `checkpoint` を呼ぶ（ページをすべてディスクへ出してから I5・I8 を調べる）。
pub(crate) fn check_after_recovery(
    cluster: &Arc<Cluster>,
    vfs: &SimVfs,
    model: &Model,
    unknown: &[TxnLog],
    pre: &PreScan,
    crash: &PreCrash,
    extra: fn(&Tables) -> Result<(), String>,
) -> V<(Model, Vec<String>)> {
    check_clog_vs_wal(cluster, pre)?;
    check_in_progress_aborted(cluster, crash)?;

    let actual = dump_tables(cluster)?;
    let actual_ddl = if model.ddl.enforce {
        Some(catalog_relations(cluster)?)
    } else {
        None
    };
    let (mut new_model, applied) =
        match find_candidate_with(model, unknown, &actual, actual_ddl.as_ref()) {
            Ok(found) => found,
            Err(nc) => {
                let msg = nc.msg;
                let inv = if nc.catalog_only {
                    "I16"
                } else if has_missing(&model.tables, &actual) {
                    "I1"
                } else {
                    "I4"
                };
                let extra_msg = extra(&actual)
                    .err()
                    .map_or(String::new(), |e| format!(" [also: {e}]"));
                return Err(violation(inv, format!("{msg}{extra_msg}")));
            }
        };
    extra(&actual).map_err(|e| violation("I2", e))?;
    check_catalog_state(cluster, &new_model.ddl)?;
    check_indexes(cluster)?;
    for (name, v) in check_sequences_observe(cluster, &new_model.seq)? {
        new_model.seq.apply(&SeqOp::Confirm { name, value: v });
    }

    cluster.checkpoint().map_err(|e| {
        violation(
            "I8",
            format!("checkpoint after recovery failed: {}", e.message),
        )
    })?;
    check_structure(cluster, vfs, pre)?;
    check_no_pins(cluster)?;
    Ok((new_model, applied))
}

/// I7: WAL にコミットレコードのある xid は clog で COMMITTED、アボートレコードなら ABORTED。
fn check_clog_vs_wal(cluster: &Arc<Cluster>, pre: &PreScan) -> V<()> {
    let clog = cluster.txn_manager().clog();
    for (set, want, name) in [
        (&pre.commits, XidStatus::Committed, "commit"),
        (&pre.aborts, XidStatus::Aborted, "abort"),
    ] {
        for x in set {
            let got = clog
                .status(Xid(*x))
                .map_err(|e| violation("I7", format!("clog lookup of {x}: {}", e.message)))?;
            if got != want {
                return Err(violation(
                    "I7",
                    format!("xid {x} has a {name} record in the WAL but clog says {got:?}"),
                ));
            }
        }
    }
    Ok(())
}

/// I6: クラッシュ時に実行中だった xid は COMMITTED になっていない。
fn check_in_progress_aborted(cluster: &Arc<Cluster>, crash: &PreCrash) -> V<()> {
    let clog = cluster.txn_manager().clog();
    for x in &crash.in_progress {
        let got = clog
            .status(Xid(*x))
            .map_err(|e| violation("I6", format!("clog lookup of {x}: {}", e.message)))?;
        if got == XidStatus::Committed {
            return Err(violation(
                "I6",
                format!("xid {x} was still running at the crash but is COMMITTED"),
            ));
        }
    }
    Ok(())
}

/// ユーザーテーブル（`public`）の全内容。第 1 列を整数のキーとする。
pub(crate) fn dump_tables(cluster: &Arc<Cluster>) -> V<Tables> {
    let mut s = Session::new(
        Arc::clone(cluster),
        StartupParams {
            user: "postgres".into(),
            database: "postgres".into(),
            application_name: None,
            options: Vec::new(),
        },
    )
    .map_err(|e| {
        violation(
            "I8",
            format!("cannot connect after recovery: {}", e.message),
        )
    })?;
    let mut q = |sql: &str| -> V<Vec<Vec<String>>> {
        let out = run_sql(&mut s, sql);
        if let Some(e) = out.errors.first() {
            return Err(violation("I8", format!("{sql}: {}", e.message)));
        }
        Ok(out.text_rows())
    };
    let names = q("SELECT relname FROM pg_class WHERE relkind = 'r' AND relnamespace = 2200")?;
    let mut tables = Tables::new();
    for n in names {
        let name = n[0].clone();
        let rows = q(&format!("SELECT * FROM {name}"))?;
        let mut t = Table::new();
        for r in rows {
            let key: i64 = r[0]
                .parse()
                .map_err(|_| violation("I8", format!("{name}: non-integer key {:?}", r[0])))?;
            if t.insert(key, r[1..].to_vec()).is_some() {
                return Err(violation("I4", format!("{name}: key {key} appears twice")));
            }
        }
        tables.insert(name, t);
    }
    Ok(tables)
}

/// I5 と I8: 全ページの構造、`next_xid`、制御ファイルとチェックポイントレコード、カタログのファイル。
/// ページがすべてディスクにある状態（チェックポイントの後）で呼ぶ。
fn check_structure(cluster: &Arc<Cluster>, vfs: &SimVfs, pre: &PreScan) -> V<()> {
    let control = cluster.control().get();
    let wal = cluster.wal();
    let flushed = wal.flushed_lsn().0;
    let next_xid = cluster.txn_manager().next_xid().0;
    let mut max_xid = pre.max_wal_xid;

    for f in relation_files(vfs)? {
        for (blkno, page) in read_pages(vfs, &f, control.rel_seg_blocks)? {
            let at = format!("{} block {blkno}", f.display());
            page.verify(blkno)
                .map_err(|e| violation("I8", format!("{at}: {e}")))?;
            if page.is_new() || page.upper() == 0 {
                continue;
            }
            if page.lsn() > flushed {
                return Err(violation(
                    "I8",
                    format!("{at}: page_lsn {} > the WAL end {flushed}", page.lsn()),
                ));
            }
            // 索引・シーケンスのページは special 領域を持つ（ヒープのタプルではない）。
            if usize::from(page.special()) != BLCKSZ {
                continue;
            }
            for off in 1..=page.max_offset() {
                let id = page
                    .item_id(off)
                    .map_err(|e| violation("I8", format!("{at} item {off}: {e}")))?;
                if id.flags != LpFlags::Normal {
                    continue;
                }
                let bytes = page
                    .item(off)
                    .map_err(|e| violation("I8", format!("{at} item {off}: {e}")))?;
                let h = TupleHeader::read(bytes)
                    .map_err(|e| violation("I8", format!("{at} item {off}: {}", e.message)))?;
                for x in [h.xmin, h.xmax] {
                    if x.is_normal() {
                        max_xid = max_xid.max(x.0);
                    }
                }
            }
        }
    }
    if max_xid >= next_xid {
        return Err(violation(
            "I5",
            format!(
                "next_xid {next_xid} is not above the largest xid {max_xid} in the heap or WAL"
            ),
        ));
    }

    // 制御ファイルがチェックポイントレコードを指している。
    let dyn_vfs = as_dyn(vfs);
    let cfg = *wal.config();
    let mut reader = WalReader::open(dyn_vfs, &cfg, Lsn(control.checkpoint_lsn));
    let rec = reader
        .next()
        .map_err(|e| violation("I8", format!("checkpoint record unreadable: {}", e.message)))?
        .ok_or_else(|| violation("I8", "the control file points past the end of the WAL"))?;
    if rec.start.0 != control.checkpoint_lsn || rec.rmgr != RmgrId::Xlog {
        return Err(violation(
            "I8",
            format!(
                "the control file points at {}, which is not a checkpoint record",
                control.checkpoint_lsn
            ),
        ));
    }
    let ck = CheckpointRecord::decode(&rec)
        .map_err(|e| violation("I8", format!("bad checkpoint record: {}", e.message)))?;
    if ck.redo.0 != control.redo_lsn {
        return Err(violation(
            "I8",
            format!(
                "checkpoint record redo {} != control redo {}",
                ck.redo.0, control.redo_lsn
            ),
        ));
    }

    check_catalog_files(cluster, vfs)
}

/// カタログにあるリレーションのファイルがすべて存在する。
fn check_catalog_files(cluster: &Arc<Cluster>, vfs: &SimVfs) -> V<()> {
    let mut s = Session::new(
        Arc::clone(cluster),
        StartupParams {
            user: "postgres".into(),
            database: "postgres".into(),
            application_name: None,
            options: Vec::new(),
        },
    )
    .map_err(|e| violation("I8", format!("cannot connect: {}", e.message)))?;
    let db = run_sql(
        &mut s,
        "SELECT oid FROM pg_database WHERE datname = 'postgres'",
    );
    let db_oid = db
        .text_rows()
        .first()
        .map(|r| r[0].clone())
        .ok_or_else(|| violation("I8", "pg_database has no row for postgres"))?;
    let rels = run_sql(
        &mut s,
        "SELECT relname, relfilenode FROM pg_class WHERE relkind = 'r' AND relnamespace = 2200",
    );
    if let Some(e) = rels.errors.first() {
        return Err(violation("I8", format!("pg_class: {}", e.message)));
    }
    for r in rels.text_rows() {
        let path = PathBuf::from(format!("base/{db_oid}/{}", r[1]));
        if !vfs.exists(&path).unwrap_or(false) {
            return Err(violation(
                "I8",
                format!("table {} has no file {}", r[0], path.display()),
            ));
        }
    }
    Ok(())
}

// ----- M4: I13〜I16（11 §3.6.2）---------------------------------------------------

/// 11 §3.6.4 の名前。
pub(crate) type InvariantViolation = Violation;

fn describe_check(e: CheckError, index: &IndexHandle) -> String {
    match e {
        CheckError::Violation(v) => format!(
            "index {} on {}: {} (block {:?}): {}",
            index.name, index.table_name, v.rule, v.block, v.detail
        ),
        CheckError::Io(e) => format!("index {}: {}", index.name, e.message),
    }
}

/// I13 + I14: ユーザーテーブルのすべての索引に、構造の検査（I14）と、ヒープとの突き合わせ・生きている版の
/// キーの重複なし（I13。`check_unique_live = true`）を走らせる。検査した索引の数を返す。
pub(crate) fn check_indexes_counted(cluster: &Arc<Cluster>) -> V<usize> {
    let defs = user_relation_defs(cluster, "postgres")
        .map_err(|e| violation("I8", format!("cannot read the catalog: {}", e.message)))?;
    let pool = &cluster.stack().pool;
    let mut n = 0;
    for def in defs.iter().filter(|d| d.kind == RelKind::Table) {
        let rel = RelHandle::from_table(def);
        for idx in rel.indexes.iter() {
            btree_check_structure(pool, idx)
                .map_err(|e| violation("I14", describe_check(e, idx)))?;
            let hc = HeapCheck {
                heap: &**cluster.storage(),
                rel: &rel,
                own: None,
                check_unique_live: true,
            };
            check_against_heap(pool, idx, &hc)
                .map_err(|e| violation("I13", describe_check(e, idx)))?;
            n += 1;
        }
    }
    Ok(n)
}

pub(crate) fn check_indexes(c: &Arc<Cluster>) -> Result<(), InvariantViolation> {
    check_indexes_counted(c).map(|_| ())
}

/// すべてのワークロードの終わり: ピンされたバッファがない。
pub(crate) fn check_no_pins(cluster: &Arc<Cluster>) -> V<()> {
    let n = cluster.stack().pool.pinned_frames();
    if n == 0 {
        Ok(())
    } else {
        Err(violation("pins", format!("{n} buffer(s) are still pinned")))
    }
}

/// カタログにある `public` の関係（表・索引・シーケンス）。
pub(crate) fn catalog_relations(cluster: &Arc<Cluster>) -> V<BTreeMap<String, DdlRel>> {
    let defs = user_relation_defs(cluster, "postgres")
        .map_err(|e| violation("I16", format!("cannot read the catalog: {}", e.message)))?;
    let names: BTreeMap<u32, String> = defs.iter().map(|d| (d.oid, d.name.clone())).collect();
    let mut out = BTreeMap::new();
    for d in &defs {
        match d.kind {
            RelKind::Table => {
                out.insert(d.name.clone(), DdlRel::table());
                for i in &d.indexes {
                    out.insert(
                        i.name.clone(),
                        DdlRel {
                            kind: 'i',
                            parent: Some(d.name.clone()),
                            unique: i.unique,
                            primary: i.primary,
                        },
                    );
                }
            }
            RelKind::Sequence => {
                let parent = d
                    .sequence
                    .as_ref()
                    .and_then(|p| p.owned_by)
                    .and_then(|(t, _)| names.get(&t).cloned());
                out.insert(
                    d.name.clone(),
                    DdlRel {
                        kind: 'S',
                        parent,
                        unique: false,
                        primary: false,
                    },
                );
            }
            RelKind::Index => {}
        }
    }
    Ok(out)
}

/// 07 §8.3 の `check_catalog`（条件 1〜9 と、10 の「ファイルが存在する」。孤児は見ない）。
pub(crate) fn check_catalog_problems(cluster: &Arc<Cluster>) -> V<()> {
    let (db, _) = cluster
        .connect("postgres", "postgres")
        .map_err(|e| violation("I16", format!("cannot connect: {}", e.message)))?;
    let snap = cluster.txn_manager().snapshot(None, 0);
    let problems = check_catalog(cluster, &db, &snap)
        .map_err(|e| violation("I16", format!("check_catalog failed: {}", e.message)))?;
    if problems.is_empty() {
        return Ok(());
    }
    let n = problems.len();
    let mut shown: Vec<_> = problems.into_iter().take(5).collect();
    if n > 5 {
        shown.push(format!("... ({n} problems)"));
    }
    Err(violation("I16", shown.join("; ")))
}

/// 障害なしで最後まで走ったあとの孤児ファイル（07 §8.3 の条件 10 の後半）。ROLLBACK・DROP・TRUNCATE の後始末
/// （延期した unlink とチェックポイントの `finish_pending_unlinks`）を済ませてから、`pg_class` にどれにも対応しない
/// ファイルがないことを調べる。クラッシュ後は孤児を許す（D15）ので、この検査は障害なしの経路だけで呼ぶ。
pub(crate) fn check_no_orphans(cluster: &Arc<Cluster>) -> V<()> {
    let mut s = Session::new(
        Arc::clone(cluster),
        StartupParams {
            user: "postgres".into(),
            database: "postgres".into(),
            application_name: None,
            options: Vec::new(),
        },
    )
    .map_err(|e| violation("I16", format!("cannot connect: {}", e.message)))?;
    // 延期した unlink は、次の文の終わりに実行される。
    run_sql(&mut s, "SELECT 1");
    run_sql(&mut s, "SELECT 1");
    drop(s);
    // 1 回目のチェックポイントが「前のサイクル」の unlink を、2 回目が今のものを片づける。
    for _ in 0..2 {
        cluster
            .checkpoint()
            .map_err(|e| violation("I16", format!("checkpoint failed: {}", e.message)))?;
    }
    let (db, _) = cluster
        .connect("postgres", "postgres")
        .map_err(|e| violation("I16", format!("cannot connect: {}", e.message)))?;
    let snap = cluster.txn_manager().snapshot(None, 0);
    let problems = check_orphan_files(cluster, &db, &snap)
        .map_err(|e| violation("I16", format!("check_orphan_files failed: {}", e.message)))?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(violation("I16", problems.join("; ")))
    }
}

/// I16: `check_catalog` + モデル（`enforce` のとき）との一致。
pub(crate) fn check_catalog_state(
    c: &Arc<Cluster>,
    model: &DdlModel,
) -> Result<(), InvariantViolation> {
    check_catalog_problems(c)?;
    if model.enforce
        && let Some(m) = model.mismatch(&catalog_relations(c)?)
    {
        return Err(violation(
            "I16",
            format!("the catalog differs from the model: {m}"),
        ));
    }
    Ok(())
}

fn seq_key(v: i64, inc: i64) -> i128 {
    if inc >= 0 {
        i128::from(v)
    } else {
        -i128::from(v)
    }
}

/// I15 の検査。各シーケンスの `nextval` の結果を返す（呼び出し側がモデルへ確定として足す）。
pub(crate) fn check_sequences_observe(
    cluster: &Arc<Cluster>,
    model: &SeqModel,
) -> V<Vec<(String, i64)>> {
    if !model.enforce {
        return Ok(Vec::new());
    }
    let bad = |m: String| violation("I15", m);
    let defs = user_relation_defs(cluster, "postgres")
        .map_err(|e| violation("I15", format!("cannot read the catalog: {}", e.message)))?;
    for (n, t) in &model.seqs {
        if t.created
            && !defs
                .iter()
                .any(|d| d.kind == RelKind::Sequence && d.name == *n)
        {
            return Err(bad(format!("(5) the sequence {n} is missing")));
        }
    }
    let pool = &cluster.stack().pool;
    let mut s = Session::new(
        Arc::clone(cluster),
        StartupParams {
            user: "postgres".into(),
            database: "postgres".into(),
            application_name: None,
            options: Vec::new(),
        },
    )
    .map_err(|e| bad(format!("cannot connect: {}", e.message)))?;
    let mut observed = Vec::new();
    for def in defs.iter().filter(|d| d.kind == RelKind::Sequence) {
        let name = &def.name;
        let handle = handle_from_def(def).map_err(|e| bad(e.message))?;
        let (inc, cache, start) = (
            handle.params.increment,
            handle.params.cache.max(1),
            handle.params.start,
        );
        // (6) と (4): ページの形、magic、`log_cnt`。
        let st = {
            let nblocks = pool
                .nblocks(def.locator, ForkNumber::Main)
                .map_err(|e| bad(format!("(5) {name}: {}", e.message)))?;
            if nblocks != 1 {
                return Err(bad(format!("(6) {name} has {nblocks} blocks")));
            }
            let buf = pool
                .read_buffer(BufferTag {
                    rel: def.locator,
                    fork: ForkNumber::Main,
                    block: 0,
                })
                .map_err(|e| bad(format!("(6) {name}: {}", e.message)))?;
            let g = buf
                .read()
                .map_err(|e| bad(format!("(6) {name}: {}", e.message)))?;
            read_state(&g, name).map_err(|e| bad(format!("(6) {name}: {}", e.message)))?
        };
        if st.log_cnt < 0 || st.log_cnt > SEQ_LOG_VALS {
            return Err(bad(format!("(4) {name}: log_cnt = {}", st.log_cnt)));
        }
        let track = model.seqs.get(name);
        if st.is_called
            && let Some(a) = track.and_then(|t| t.above)
            && seq_key(st.last_value, inc) < seq_key(a.farthest(inc), inc)
        {
            return Err(bad(format!(
                "(4) {name}: last_value {} is behind the confirmed value {}",
                st.last_value,
                a.farthest(inc)
            )));
        }
        // (1) と (2)。
        let out = run_sql(&mut s, &format!("SELECT nextval('{name}')"));
        if let Some(e) = out.errors.first() {
            return Err(bad(format!("nextval({name}) failed: {}", e.message)));
        }
        let v: i64 = out
            .text_rows()
            .first()
            .and_then(|r| r[0].parse().ok())
            .ok_or_else(|| bad(format!("nextval({name}) returned no number")))?;
        if let Some(t) = track {
            if let Some(a) = t.above
                && seq_key(v, inc) <= seq_key(a.farthest(inc), inc)
            {
                return Err(bad(format!(
                    "(1) {name}: nextval = {v} but {} was already confirmed",
                    a.farthest(inc)
                )));
            }
            if let Some(r) = t.restart
                && seq_key(v, inc) < seq_key(r.farthest(inc), inc)
            {
                return Err(bad(format!(
                    "(1) {name}: nextval = {v} is below the restart value {}",
                    r.farthest(inc)
                )));
            }
        }
        let base = track
            .and_then(|t| t.any)
            .map_or(start.saturating_sub(inc), |a| a.farthest(inc));
        // PostgreSQL の上限 (32 + cache) に、クラッシュで途切れた払い出し 1 回分の余裕を足す。
        let limit =
            seq_key(base, inc) + i128::from(SEQ_LOG_VALS + cache + 1) * i128::from(inc.abs());
        if seq_key(v, inc) > limit {
            return Err(bad(format!(
                "(2) {name}: nextval = {v} skips too far (largest returned {base}, cache {cache})"
            )));
        }
        observed.push((name.clone(), v));
    }
    // (3) `serial` の表の `id`。
    if let Some(t7) = &model.t7
        && defs
            .iter()
            .any(|d| d.kind == RelKind::Table && d.name == t7.table)
    {
        let out = run_sql(&mut s, &format!("SELECT id FROM {}", t7.table));
        if let Some(e) = out.errors.first() {
            return Err(bad(format!("(3) {}: {}", t7.table, e.message)));
        }
        let mut seen = BTreeSet::new();
        for r in out.text_rows() {
            let id: i64 = r[0]
                .parse()
                .map_err(|_| bad(format!("(3) non-integer id {:?}", r[0])))?;
            if !seen.insert(id) {
                return Err(bad(format!("(3) {}: id {id} appears twice", t7.table)));
            }
        }
        if let Some(m) = t7.ids.iter().find(|i| !seen.contains(i)) {
            return Err(bad(format!(
                "(3) {}: the committed id {m} is gone",
                t7.table
            )));
        }
    }
    Ok(observed)
}

/// I15（11 §3.6.4 の形）。
pub(crate) fn check_sequences(
    c: &Arc<Cluster>,
    model: &SeqModel,
) -> Result<(), InvariantViolation> {
    check_sequences_observe(c, model).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Op;
    use crate::model::find_candidate_with;
    use yuzhu_core::storage::vfs::CrashMode;
    use yuzhu_core::testing::{TestCluster, TestClusterOptions};

    fn fresh() -> TestCluster {
        TestCluster::with_options(TestClusterOptions::crash_sim(5)).unwrap()
    }

    #[allow(clippy::unnecessary_wraps)]
    fn no_extra(_: &Tables) -> Result<(), String> {
        Ok(())
    }

    fn sql(tc: &TestCluster, q: &str) {
        let mut s = tc.session("postgres").unwrap();
        let out = run_sql(&mut s, q);
        assert!(out.is_ok(), "{q}: {:?}", out.errors);
    }

    /// 1 回クラッシュ・リカバリして、全検査を通す。
    fn recover_and_check(
        tc: TestCluster,
        model: &Model,
        unknown: &[TxnLog],
    ) -> (V<(Model, Vec<String>)>, TestCluster) {
        let crash = pre_crash(&tc.cluster, !unknown.is_empty());
        let (vfs, options) = tc.crash(CrashMode::DropUnsynced);
        let pre = pre_recovery_scan(&vfs).expect("pre-recovery scan");
        let tc = TestCluster::start_on(vfs, options).expect("recovery");
        let r = check_after_recovery(&tc.cluster, &tc.vfs, model, unknown, &pre, &crash, no_extra);
        (r, tc)
    }

    #[test]
    fn a_fresh_cluster_passes_every_check() {
        let tc = fresh();
        let (r, _tc) = recover_and_check(tc, &Model::default(), &[]);
        let (m, applied) = r.expect("invariants");
        assert!(m.tables.is_empty());
        assert!(applied.is_empty());
    }

    #[test]
    fn committed_data_is_found_and_matched() {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t (k int, c1 text)");
        sql(&tc, "INSERT INTO t VALUES (1, 'a')");
        let mut model = Model::default();
        model.apply(&TxnLog {
            label: "c".into(),
            ops: vec![
                Op::Create("t".into()),
                Op::Insert {
                    table: "t".into(),
                    key: 1,
                    row: vec!["a".into()],
                },
            ],
        });
        let (r, tc) = recover_and_check(tc, &model, &[]);
        let (m, _) = r.expect("invariants");
        assert_eq!(m, model);
        assert_eq!(dump_tables(&tc.cluster).unwrap(), model.tables);
    }

    #[test]
    fn an_unknown_transaction_may_have_happened_or_not() {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t (k int, c1 text)");
        let mut model = Model::default();
        model.apply(&TxnLog {
            label: "c".into(),
            ops: vec![Op::Create("t".into())],
        });
        // The row was committed, but the model was told "unknown".
        sql(&tc, "INSERT INTO t VALUES (7, 'x')");
        let unknown = vec![TxnLog {
            label: "u".into(),
            ops: vec![Op::Insert {
                table: "t".into(),
                key: 7,
                row: vec!["x".into()],
            }],
        }];
        let (r, _tc) = recover_and_check(tc, &model, &unknown);
        let (m, applied) = r.expect("invariants");
        assert_eq!(applied, vec!["u".to_string()]);
        assert_eq!(m.tables["t"].len(), 1);
    }

    #[test]
    fn a_missing_committed_row_is_reported_as_i1() {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t (k int, c1 text)");
        let mut model = Model::default();
        model.apply(&TxnLog {
            label: "c".into(),
            ops: vec![
                Op::Create("t".into()),
                Op::Insert {
                    table: "t".into(),
                    key: 1,
                    row: vec!["never written".into()],
                },
            ],
        });
        let (r, _tc) = recover_and_check(tc, &model, &[]);
        assert_eq!(r.unwrap_err().inv, "I1");
    }

    #[test]
    fn an_extra_row_is_reported_as_i4() {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t (k int, c1 text)");
        sql(&tc, "INSERT INTO t VALUES (1, 'a')");
        let mut model = Model::default();
        model.apply(&TxnLog {
            label: "c".into(),
            ops: vec![Op::Create("t".into())],
        });
        let (r, _tc) = recover_and_check(tc, &model, &[]);
        assert_eq!(r.unwrap_err().inv, "I4");
    }

    #[test]
    fn a_wrong_clog_is_reported_as_i7() {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t (k int)");
        let pre_scan = {
            let (vfs, options) = tc.crash(CrashMode::KeepAll);
            let pre = pre_recovery_scan(&vfs).unwrap();
            (vfs, options, pre)
        };
        let (vfs, options, mut pre) = pre_scan;
        let tc = TestCluster::start_on(vfs, options).unwrap();
        // Pretend the WAL had a commit record for a transaction that aborted.
        let aborted = tc.cluster.txn_manager().next_xid().0;
        tc.cluster
            .txn_manager()
            .clog()
            .set_status(Xid(aborted), XidStatus::Aborted)
            .unwrap();
        pre.commits.insert(aborted);
        let e = check_clog_vs_wal(&tc.cluster, &pre).unwrap_err();
        assert_eq!(e.inv, "I7");
    }

    #[test]
    fn a_running_xid_that_committed_is_reported_as_i6() {
        let tc = fresh();
        let x = tc.cluster.txn_manager().next_xid().0;
        tc.cluster
            .txn_manager()
            .clog()
            .set_status(Xid(x), XidStatus::Committed)
            .unwrap();
        let crash = PreCrash {
            in_progress: vec![x],
        };
        assert_eq!(
            check_in_progress_aborted(&tc.cluster, &crash)
                .unwrap_err()
                .inv,
            "I6"
        );
        assert!(check_in_progress_aborted(&tc.cluster, &PreCrash::default()).is_ok());
    }

    #[test]
    fn page_lsn_beyond_the_wal_is_reported_as_i2() {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t (k int)");
        sql(&tc, "INSERT INTO t VALUES (1)");
        tc.cluster.checkpoint().unwrap();
        let (vfs, _options) = tc.crash(CrashMode::KeepAll);
        // Stamp an absurd LSN into a catalog page and fix its checksum.
        let files = relation_files(&vfs).unwrap();
        let target = files
            .iter()
            .find(|f| f.starts_with("base"))
            .expect("a relation file")
            .clone();
        let f = vfs.open(&target, OpenMode::ReadWrite).unwrap();
        let mut page = Page::zeroed();
        f.read_exact_at(&mut page.0, 0).unwrap();
        page.set_lsn(u64::MAX / 2);
        let sum = yuzhu_core::storage::checksum::page_checksum(&page.0, 0);
        page.set_checksum(sum);
        f.write_all_at(&page.0, 0).unwrap();
        assert_eq!(pre_recovery_scan(&vfs).unwrap_err().inv, "I2");
    }

    #[test]
    fn relation_files_skip_other_forks_and_control_files() {
        let tc = fresh();
        let files = relation_files(&tc.vfs).unwrap();
        assert!(!files.is_empty());
        for f in files {
            let name = f.file_name().unwrap().to_str().unwrap().to_owned();
            assert!(
                name.chars().all(|c| c.is_ascii_digit() || c == '.'),
                "{name}"
            );
        }
    }

    #[test]
    fn violations_display_their_invariant() {
        assert_eq!(violation("I4", "boom").to_string(), "I4: boom");
    }
    // ----- M4: I13〜I16 -----

    /// 索引のファイルのブロック `block` を読み、`edit` で書き換えてチェックサムを付け直す。
    fn edit_index_page(
        vfs: &SimVfs,
        tc_defs: &[(String, yuzhu_core::storage::smgr::RelFileLocator)],
        index: &str,
        block: u32,
        edit: &dyn Fn(&mut Page),
    ) {
        let (_, loc) = tc_defs.iter().find(|(n, _)| n == index).expect("index");
        let path = yuzhu_core::storage::smgr::relpath(*loc, ForkNumber::Main, 0);
        let f = vfs.open(&path, OpenMode::ReadWrite).unwrap();
        let mut page = Page::zeroed();
        let off = u64::from(block) * BLCKSZ as u64;
        f.read_exact_at(&mut page.0, off).unwrap();
        edit(&mut page);
        let sum = yuzhu_core::storage::checksum::page_checksum(&page.0, block);
        page.set_checksum(sum);
        f.write_all_at(&page.0, off).unwrap();
    }

    fn index_locators(
        tc: &TestCluster,
    ) -> Vec<(String, yuzhu_core::storage::smgr::RelFileLocator)> {
        tc.relation_defs("postgres")
            .unwrap()
            .iter()
            .flat_map(|d| d.indexes.iter().map(|i| (i.name.clone(), i.locator)))
            .collect()
    }

    fn indexed() -> TestCluster {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t (k int PRIMARY KEY, v int UNIQUE)");
        for i in 1..=20 {
            sql(&tc, &format!("INSERT INTO t VALUES ({i}, {})", i + 100));
        }
        tc
    }

    #[test]
    fn healthy_indexes_pass_i13_and_i14() {
        let tc = indexed();
        assert_eq!(check_indexes_counted(&tc.cluster).unwrap(), 2);
        check_indexes(&tc.cluster).unwrap();
        check_no_pins(&tc.cluster).unwrap();
        // 索引のない表だけなら 0 件。
        assert_eq!(check_indexes_counted(&fresh().cluster).unwrap(), 0);
    }

    #[test]
    fn a_broken_tree_is_reported_as_i14() {
        let tc = indexed();
        tc.cluster.checkpoint().unwrap();
        let locs = index_locators(&tc);
        let (vfs, options) = tc.crash(CrashMode::KeepAll);
        // メタページの magic を壊す。
        edit_index_page(&vfs, &locs, "t_v_key", 0, &|p| {
            p.body_mut()[0..4].fill(0xFF);
        });
        let tc = TestCluster::start_on(vfs, options).unwrap();
        let v = check_indexes(&tc.cluster).unwrap_err();
        assert_eq!(v.inv, "I14", "{v}");
    }

    #[test]
    fn the_lossy_insert_knob_is_reported_as_i13() {
        let mut opts = TestClusterOptions::crash_sim(5);
        opts.knobs.btree_lossy_insert_every = 4;
        let tc = TestCluster::with_options(opts).unwrap();
        sql(&tc, "CREATE TABLE t (k int PRIMARY KEY)");
        for i in 1..=12 {
            sql(&tc, &format!("INSERT INTO t VALUES ({i})"));
        }
        let v = check_indexes(&tc.cluster).unwrap_err();
        assert_eq!(v.inv, "I13", "{v}");
        assert!(v.msg.contains("heap.missing_entry"), "{v}");
    }

    #[test]
    fn a_missing_index_entry_is_reported_as_i13() {
        let tc = indexed();
        tc.cluster.checkpoint().unwrap();
        let locs = index_locators(&tc);
        let (vfs, options) = tc.crash(CrashMode::KeepAll);
        // ルート葉（ブロック 1）の最後の項目を落とす（構造は正しいまま）。
        edit_index_page(&vfs, &locs, "t_v_key", 1, &|p| {
            let n = p.max_offset();
            let special = p.special_area().to_vec();
            let items: Vec<Vec<u8>> = (1..n).map(|o| p.item(o).unwrap().to_vec()).collect();
            let refs: Vec<&[u8]> = items.iter().map(Vec::as_slice).collect();
            let lsn = p.lsn();
            *p = *Page::build_with_items(&special, &refs).expect("rebuild");
            p.set_lsn(lsn);
        });
        let tc = TestCluster::start_on(vfs, options).unwrap();
        let v = check_indexes(&tc.cluster).unwrap_err();
        assert_eq!(v.inv, "I13", "{v}");
        assert!(v.msg.contains("heap.missing_entry"), "{v}");
    }

    #[test]
    fn catalog_consistency_and_the_ddl_model() {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t (k int PRIMARY KEY, v int UNIQUE)");
        sql(&tc, "CREATE SEQUENCE sq");
        let mut m = Model {
            ddl: DdlModel::enforcing(),
            ..Model::default()
        };
        m.apply(&TxnLog {
            label: "ddl".into(),
            ops: vec![
                Op::Create("t".into()),
                Op::Ddl(crate::model::DdlOp::CreateIndex {
                    name: "t_pkey".into(),
                    table: "t".into(),
                    unique: true,
                    primary: true,
                }),
                Op::Ddl(crate::model::DdlOp::CreateIndex {
                    name: "t_v_key".into(),
                    table: "t".into(),
                    unique: true,
                    primary: false,
                }),
                Op::Ddl(crate::model::DdlOp::CreateSeq {
                    name: "sq".into(),
                    owner: None,
                }),
            ],
        });
        check_catalog_state(&tc.cluster, &m.ddl).unwrap();
        // モデルにない索引がカタログにある / モデルにあるものが無い。
        let mut extra = m.ddl.clone();
        extra.rels.remove("t_v_key");
        let v = check_catalog_state(&tc.cluster, &extra).unwrap_err();
        assert_eq!(v.inv, "I16");
        assert!(v.msg.contains("t_v_key"), "{v}");
        let mut missing = m.ddl.clone();
        missing.rels.insert("ghost".into(), DdlRel::table());
        assert_eq!(
            check_catalog_state(&tc.cluster, &missing).unwrap_err().inv,
            "I16"
        );
        // 強制しないモデルは一致を求めない（check_catalog だけ）。
        check_catalog_state(&tc.cluster, &DdlModel::default()).unwrap();
    }

    #[test]
    fn a_broken_catalog_is_reported_as_i16() {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t (k int PRIMARY KEY)");
        check_catalog_problems(&tc.cluster).unwrap();
        for d in [
            crate::mutation::CatalogDamage::DependRow,
            crate::mutation::CatalogDamage::IndexRow,
        ] {
            let tc = fresh();
            sql(&tc, "CREATE TABLE t (k int PRIMARY KEY)");
            check_catalog_problems(&tc.cluster).unwrap();
            assert!(crate::mutation::damage_catalog(&tc.cluster, d).unwrap());
            let v = check_catalog_problems(&tc.cluster).unwrap_err();
            assert_eq!(v.inv, "I16", "{d:?}: {v}");
        }
    }

    #[test]
    fn unknown_ddl_must_apply_atomically() {
        // 表の内容は同じでも、索引の有無が候補と合わなければ I16。
        let mut m = Model {
            ddl: DdlModel::enforcing(),
            ..Model::default()
        };
        m.apply(&TxnLog {
            label: "c".into(),
            ops: vec![Op::Create("t".into())],
        });
        let unknown = vec![TxnLog {
            label: "u".into(),
            ops: vec![Op::Ddl(crate::model::DdlOp::CreateIndex {
                name: "i".into(),
                table: "t".into(),
                unique: false,
                primary: false,
            })],
        }];
        let actual_t = m.tables.clone();
        let mut with = m.ddl.rels.clone();
        // 索引なし = 不明を適用しない候補と一致。
        assert!(find_candidate_with(&m, &unknown, &actual_t, Some(&with)).is_ok());
        with.insert(
            "i".into(),
            DdlRel {
                kind: 'i',
                parent: Some("t".into()),
                unique: false,
                primary: false,
            },
        );
        let (_, applied) = find_candidate_with(&m, &unknown, &actual_t, Some(&with)).unwrap();
        assert_eq!(applied, vec!["u".to_string()]);
        with.insert("zzz".into(), DdlRel::table());
        let e = find_candidate_with(&m, &unknown, &actual_t, Some(&with)).unwrap_err();
        assert!(e.catalog_only, "{}", e.msg);
    }

    fn seq_model() -> SeqModel {
        let mut sm = SeqModel::enforcing();
        sm.apply(&SeqOp::Create("sq".into()));
        sm
    }

    #[test]
    fn sequences_pass_and_the_observed_values_are_returned() {
        let tc = fresh();
        sql(&tc, "CREATE SEQUENCE sq");
        sql(&tc, "SELECT nextval('sq')");
        sql(&tc, "SELECT nextval('sq')");
        let mut sm = seq_model();
        sm.apply(&SeqOp::Confirm {
            name: "sq".into(),
            value: 2,
        });
        let obs = check_sequences_observe(&tc.cluster, &sm).unwrap();
        assert_eq!(obs, vec![("sq".to_string(), 3)]);
        // 強制しないモデルは何もしない。
        assert!(
            check_sequences_observe(&tc.cluster, &SeqModel::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_repeated_or_skipped_sequence_value_is_reported_as_i15() {
        let tc = fresh();
        sql(&tc, "CREATE SEQUENCE sq");
        // 確定した最大が 5 なのに次の値が 1: 二度払い出し（I15 の 1）。
        let mut sm = seq_model();
        sm.apply(&SeqOp::Confirm {
            name: "sq".into(),
            value: 5,
        });
        let v = check_sequences(&tc.cluster, &sm).unwrap_err();
        assert_eq!(v.inv, "I15");
        assert!(v.msg.contains("(1)") || v.msg.contains("(4)"), "{v}");
        // 返した最大が 0 なのに次の値が 1000 に飛ぶ（欠番の上限。I15 の 2）。
        let tc = fresh();
        sql(&tc, "CREATE SEQUENCE sq START 1000");
        let mut sm = seq_model();
        sm.returned("sq", 0);
        let v = check_sequences(&tc.cluster, &sm).unwrap_err();
        assert!(v.msg.contains("(2)"), "{v}");
        // 作成が確定しているのにシーケンスが無い（5）。
        let tc = fresh();
        let v = check_sequences(&tc.cluster, &seq_model()).unwrap_err();
        assert!(v.msg.contains("(5)"), "{v}");
    }

    #[test]
    fn duplicate_serial_ids_are_reported_as_i15() {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t7 (id int, who int)");
        sql(&tc, "INSERT INTO t7 VALUES (1, 1)");
        let mut sm = SeqModel::enforcing();
        sm.track_serial_table("t7");
        sm.apply(&SeqOp::T7Id(1));
        check_sequences(&tc.cluster, &sm).unwrap();
        sql(&tc, "INSERT INTO t7 VALUES (1, 2)");
        let v = check_sequences(&tc.cluster, &sm).unwrap_err();
        assert!(v.msg.contains("(3)") && v.msg.contains("twice"), "{v}");
        // 確定した id が消えた。
        sm.apply(&SeqOp::T7Id(9));
        sql(&tc, "DELETE FROM t7 WHERE who = 2");
        let v = check_sequences(&tc.cluster, &sm).unwrap_err();
        assert!(v.msg.contains("gone"), "{v}");
    }

    #[test]
    fn a_pinned_buffer_is_reported() {
        let tc = fresh();
        sql(&tc, "CREATE TABLE t (k int)");
        let buf = tc.pool().read_buffer(BufferTag {
            rel: tc
                .relation_defs("postgres")
                .unwrap()
                .first()
                .map_or_else(|| panic!("no relation"), |d| d.locator),
            fork: ForkNumber::Main,
            block: 0,
        });
        // 表がなければ読めないので、あれば検査する。
        if let Ok(b) = buf {
            assert_eq!(check_no_pins(&tc.cluster).unwrap_err().inv, "pins");
            drop(b);
        }
        check_no_pins(&tc.cluster).unwrap();
    }
}
