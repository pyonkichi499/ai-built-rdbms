//! CREATE / DROP DATABASE の手順と DBASE rmgr（DB）。
//!
//! `recovery::dispatch` が `RmgrId::Dbase` で `redo` を呼ぶ（F0b-1 の口）。
//! `info`: `CREATE_FILE_COPY` 0x00、`DROP` 0x10（00 §3.3、09 章 §3.1）。
//! この層は `Cluster`・session・ddl を知らない（チェックポイントを呼ぶ手順は `ddl::database` が行う）。

use crate::error::{Error, Result, Severity, sqlstate};
use crate::storage::buffer::BufferPool;
use crate::storage::smgr::StorageManager;
use crate::txn::{TxnManager, Xid};
use crate::util::sync::lock_ignore_poison;
use crate::wal::{DecodedRecord, Lsn, RecordBuilder, RedoCtx, RmgrId, Wal};

pub const DBASE_CREATE_FILE_COPY: u8 = 0x00;
pub const DBASE_DROP: u8 = 0x10;

/// DBASE の WAL レコード（09 §3.1）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DbaseRecord {
    CreateFileCopy { src: Oid, dst: Oid },
    Drop { db: Oid },
}

fn corrupt(message: String) -> Error {
    Error::new(sqlstate::DATA_CORRUPTED, message).with_severity(Severity::Panic)
}

impl DbaseRecord {
    /// `(info, メインデータ)`。
    pub fn encode(&self) -> (u8, Vec<u8>) {
        match *self {
            DbaseRecord::CreateFileCopy { src, dst } => {
                let mut v = Vec::with_capacity(8);
                v.extend_from_slice(&src.to_le_bytes());
                v.extend_from_slice(&dst.to_le_bytes());
                (DBASE_CREATE_FILE_COPY, v)
            }
            DbaseRecord::Drop { db } => (DBASE_DROP, db.to_le_bytes().to_vec()),
        }
    }

    /// 長さ・`info`（下位 4 ビット）の不整合は `Severity::Panic` の `XX001`。
    pub fn decode(info: u8, main: &[u8]) -> Result<DbaseRecord> {
        if info & 0x0F != 0 {
            return Err(corrupt(format!(
                "invalid dbase WAL record: info 0x{info:02X} has low bits set"
            )));
        }
        let u32_at =
            |i: usize| u32::from_le_bytes([main[i], main[i + 1], main[i + 2], main[i + 3]]);
        match info {
            DBASE_CREATE_FILE_COPY if main.len() == 8 => Ok(DbaseRecord::CreateFileCopy {
                src: u32_at(0),
                dst: u32_at(4),
            }),
            DBASE_DROP if main.len() == 4 => Ok(DbaseRecord::Drop { db: u32_at(0) }),
            DBASE_CREATE_FILE_COPY | DBASE_DROP => Err(corrupt(format!(
                "invalid dbase WAL record: info 0x{info:02X} with {} bytes of main data",
                main.len()
            ))),
            _ => Err(corrupt(format!(
                "invalid dbase WAL record: unknown info 0x{info:02X}"
            ))),
        }
    }
}

/// WAL・バッファ・ファイル・バリアの部品（`Cluster` を知らずに済ませるため）。
#[derive(Clone, Copy, Debug)]
pub struct DbaseParts<'a> {
    pub pool: &'a BufferPool,
    pub smgr: &'a StorageManager,
    pub wal: &'a Wal,
    pub txn: &'a TxnManager,
}

fn log_record(p: &DbaseParts<'_>, xid: Xid, rec: DbaseRecord) -> Result<Lsn> {
    let (info, main) = rec.encode();
    let mut b = RecordBuilder::new(RmgrId::Dbase, info, xid);
    b.main = main;
    Ok(p.wal.insert(b)?.end)
}

/// `CREATE_FILE_COPY` を挿入する（flush しない。コミットの flush に含まれる）。失敗は Panic。
pub fn log_create_file_copy(p: &DbaseParts<'_>, xid: Xid, src: Oid, dst: Oid) -> Result<Lsn> {
    log_record(p, xid, DbaseRecord::CreateFileCopy { src, dst })
}

/// `DROP` を挿入して flush する（ディレクトリの削除より前でなければならない）。`xid` は 0（DB-D9）。
pub fn log_drop(p: &DbaseParts<'_>, db: Oid) -> Result<Lsn> {
    let lsn = log_record(p, Xid::INVALID, DbaseRecord::Drop { db })?;
    p.wal.flush(lsn)?;
    Ok(lsn)
}

#[allow(clippy::print_stderr)]
fn warn(msg: &str) {
    eprintln!("WARNING:  {msg}");
}

fn redo_failure(e: Error) -> Error {
    e.with_severity(Severity::Panic)
}

/// DBASE rmgr の REDO（09 §5.4）。冪等。失敗は Panic（`Cluster::open` の失敗）。
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()> {
    debug_assert_eq!(rec.rmgr, RmgrId::Dbase);
    match DbaseRecord::decode(rec.info, &rec.main)? {
        DbaseRecord::CreateFileCopy { src, dst } => {
            let vfs = ctx.smgr.vfs();
            let src_exists = vfs.exists(&database_dir(src)).map_err(|e| {
                redo_failure(Error::new(
                    sqlstate::IO_ERROR,
                    format!("could not stat \"{}\": {e}", database_dir(src).display()),
                ))
            })?;
            // src が無ければ dst には触れない（コピーは commit の前に fsync 済みで、dst は無事なはず。DB-D8）。
            if !src_exists {
                warn(&format!(
                    "could not copy database {src} to {dst}: source directory base/{src} does not exist; skipping"
                ));
                return Ok(());
            }
            forget_in_memory(ctx, dst)?;
            // 毎回作り直す（途中のクラッシュも同じ結果になる）。
            remove_database_dir(&**vfs, dst).map_err(redo_failure)?;
            crate::datadir::copy_database_dir(&**vfs, src, dst, true).map_err(redo_failure)
        }
        DbaseRecord::Drop { db } => {
            forget_in_memory(ctx, db)?;
            remove_database_dir(&**ctx.smgr.vfs(), db).map_err(redo_failure)
        }
    }
}

fn forget_in_memory(ctx: &RedoCtx, db: Oid) -> Result<()> {
    ctx.pool.drop_database_buffers(db).map_err(redo_failure)?;
    ctx.smgr.forget_database(db).map_err(redo_failure)?;
    lock_ignore_poison(&ctx.invalid).forget_database(db);
    Ok(())
}

/// ディレクトリを（なければ何もせず）消し、`base/` を `sync_dir` する。
pub fn remove_database_dir(vfs: &dyn Vfs, db: Oid) -> Result<()> {
    let dir = database_dir(db);
    let io = |what: &str, e: std::io::Error| {
        Error::new(
            sqlstate::IO_ERROR,
            format!("could not {what} \"{}\": {e}", dir.display()),
        )
    };
    if !vfs.exists(&dir).map_err(|e| io("stat", e))? {
        return Ok(());
    }
    vfs.remove_dir_all(&dir).map_err(|e| io("remove", e))?;
    vfs.sync_dir(Path::new("base"))
        .map_err(|e| io("fsync the parent of", e))
}

/// 名前予約ロックの `class`（`pg_database`）。
const DATABASE_CLASS: Oid = 1262;
const NAME_LOCK_FLAG: u64 = 1 << 63;

/// 名前予約ロック。同名の CREATE / DROP / ALTER DATABASE を直列にする。
/// `obj` は名前の FNV-1a 64 ビットハッシュに最上位ビットを立てたもの。
pub fn name_lock_tag(name: &str) -> LockTag {
    LockTag::Object {
        db: 0,
        class: DATABASE_CLASS,
        obj: crate::catalog::roles::fnv1a64(name.as_bytes()) | NAME_LOCK_FLAG,
    }
}

/// `datfrozenxid` の更新用（VC）。`obj` はデータベースの OID（最上位ビットが 0 なので `name_lock_tag` と衝突しない）。
pub fn frozen_lock_tag(db: Oid) -> LockTag {
    LockTag::Object {
        db: 0,
        class: DATABASE_CLASS,
        obj: u64::from(db),
    }
}

/// 失敗時の後始末（09 §5.3）。`disarm` されなければ、呼び出し側が [`CreatedDirGuard::remove_now`] で
/// `base/<oid>/` を消す。Drop では I/O をしない（残った場合は起動時の WARNING が拾う）。
#[derive(Debug)]
pub struct CreatedDirGuard {
    vfs: Arc<dyn Vfs>,
    db: Oid,
    armed: bool,
}

impl CreatedDirGuard {
    pub fn new(vfs: Arc<dyn Vfs>, db: Oid) -> Self {
        CreatedDirGuard {
            vfs,
            db,
            armed: true,
        }
    }

    /// コミットの結果が分からなくなる直前に呼ぶ。以後 `remove_now` は何もしない。
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// 無効化されていなければディレクトリを消す（失敗は無視）。
    pub fn remove_now(&mut self) {
        if std::mem::take(&mut self.armed) {
            let _ = remove_database_dir(&*self.vfs, self.db);
        }
    }
}

// ===== DB-1: ロックのガード・孤児の検出（09 章 §4.3、§5.6） =====

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::backend::{BackendId, BackendRegistry};
use crate::catalog::store::DatabaseRow;
use crate::interrupt::InterruptFlag;
use crate::storage::vfs::Vfs;
use crate::txn::lock::{LockManager, LockMode, LockScope, LockTag, WaitCtl};
use crate::types::Oid;

/// 既定の待ち時間（`ClusterOptions.database_busy_wait` の既定値）と間隔。
pub const DEFAULT_DATABASE_BUSY_WAIT: Duration = Duration::from_secs(5);
pub const DATABASE_BUSY_POLL: Duration = Duration::from_millis(100);

/// 1 つの（タグ、モード）を `LockScope::Session` で持つ。Drop で、まだ持っていれば解放する
/// （I/O はしない）。
#[derive(Debug)]
pub struct HeldLock {
    locks: Arc<LockManager>,
    id: BackendId,
    tag: LockTag,
    mode: LockMode,
}

impl HeldLock {
    /// 取れるまで待つ（キャンセル・タイムアウト・デッドロックは `LockManager::acquire` のとおり）。
    pub fn acquire(
        locks: &Arc<LockManager>,
        id: BackendId,
        tag: LockTag,
        mode: LockMode,
        wait: &WaitCtl<'_>,
    ) -> Result<HeldLock> {
        locks.acquire(id, tag, mode, LockScope::Session, wait)?;
        Ok(HeldLock {
            locks: Arc::clone(locks),
            id,
            tag,
            mode,
        })
    }

    /// 待たずに試す。
    pub fn try_acquire(
        locks: &Arc<LockManager>,
        id: BackendId,
        tag: LockTag,
        mode: LockMode,
    ) -> Option<HeldLock> {
        locks
            .try_acquire(id, tag, mode, LockScope::Session)
            .then(|| HeldLock {
                locks: Arc::clone(locks),
                id,
                tag,
                mode,
            })
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        if self.locks.holds(self.id, self.tag, self.mode) {
            self.locks.release(self.id, self.tag, self.mode);
        }
    }
}

/// 1 本の接続のセッションスコープのロック全体の持ち主。
/// Drop で `release_all(id, Session)` → `unregister_backend`。
#[derive(Debug)]
pub struct SessionLocks {
    locks: Arc<LockManager>,
    id: BackendId,
}

impl SessionLocks {
    /// `LockManager` にバックエンドを登録する。
    pub fn register(locks: &Arc<LockManager>, id: BackendId, pid: i32) -> SessionLocks {
        locks.register_backend(id, pid);
        SessionLocks {
            locks: Arc::clone(locks),
            id,
        }
    }

    pub fn id(&self) -> BackendId {
        self.id
    }
}

impl Drop for SessionLocks {
    fn drop(&mut self) {
        self.locks.release_all(self.id, LockScope::Session);
        self.locks.unregister_backend(self.id);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BusyKind {
    /// CREATE DATABASE のテンプレート。
    Source,
    /// DROP DATABASE の対象。
    Target,
}

/// `db` の `AccessExclusive` を、他のセッションが居なくなるまで最長 `wait_total`、
/// [`DATABASE_BUSY_POLL`] ごとに `try_acquire` して取る（DB-D5）。取れなければ 55006。
/// 待つ間、周期ごとに `interrupts.check()` を呼ぶ。待ち手にならないのでデッドロック検出器に載らない。
#[allow(clippy::too_many_arguments)]
pub fn lock_database_exclusive(
    locks: &Arc<LockManager>,
    backends: &BackendRegistry,
    me: BackendId,
    my_db: Oid,
    db: Oid,
    name: &str,
    kind: BusyKind,
    wait_total: Duration,
    interrupts: &InterruptFlag,
) -> Result<HeldLock> {
    let tag = LockTag::Database(db);
    let start = Instant::now();
    loop {
        if let Some(held) = HeldLock::try_acquire(locks, me, tag, LockMode::AccessExclusive) {
            return Ok(held);
        }
        interrupts.check()?;
        let elapsed = start.elapsed();
        if elapsed >= wait_total {
            let own = usize::from(my_db == db);
            let n = backends.count_in_db(db).saturating_sub(own).max(1);
            let detail = if n == 1 {
                "There is 1 other session using the database.".to_owned()
            } else {
                format!("There are {n} other sessions using the database.")
            };
            let message = match kind {
                BusyKind::Source => {
                    format!("source database \"{name}\" is being accessed by other users")
                }
                BusyKind::Target => format!("database \"{name}\" is being accessed by other users"),
            };
            return Err(Error::new(sqlstate::OBJECT_IN_USE, message).with_detail(detail));
        }
        std::thread::sleep(DATABASE_BUSY_POLL.min(wait_total.saturating_sub(elapsed)));
    }
}

pub fn database_dir(db: Oid) -> PathBuf {
    Path::new("base").join(db.to_string())
}

/// `base/` の下の「10 進の数字だけの名前」のディレクトリの OID（昇順）。それ以外の名前は無視する。
pub fn list_database_dirs(vfs: &dyn Vfs) -> Result<Vec<Oid>> {
    let entries = match vfs.read_dir(Path::new("base")) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::new(
                sqlstate::IO_ERROR,
                format!("could not read directory \"base\": {e}"),
            ));
        }
    };
    let mut oids: Vec<Oid> = entries
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|n| n.parse::<Oid>().ok())
        .collect();
    oids.sort_unstable();
    Ok(oids)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct OrphanReport {
    pub dirs_without_row: Vec<Oid>,
    pub rows_without_dir: Vec<(Oid, String)>,
}

/// ディレクトリと `pg_database` の行の食い違い（純関数）。
pub fn find_orphans(dirs: &[Oid], rows: &[DatabaseRow]) -> OrphanReport {
    OrphanReport {
        dirs_without_row: dirs
            .iter()
            .copied()
            .filter(|d| rows.iter().all(|r| r.oid != *d))
            .collect(),
        rows_without_dir: rows
            .iter()
            .filter(|r| !dirs.contains(&r.oid))
            .map(|r| (r.oid, r.name.clone()))
            .collect(),
    }
}

#[cfg(test)]
mod record_tests {
    use super::*;
    use crate::debug_knobs::DebugKnobs;
    use crate::storage::smgr::{DEFAULTTABLESPACE_OID, ForkNumber, RelFileLocator, RelFileNumber};
    use crate::storage::testing::TestStorage;
    use crate::storage::vfs::OpenMode;
    use crate::wal::record::encode_record;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    fn ctx_for(ts: &TestStorage) -> RedoCtx {
        RedoCtx {
            pool: Arc::clone(ts.pool()),
            smgr: Arc::clone(ts.smgr()),
            ext: Box::new(()),
            invalid: Mutex::new(crate::wal::InvalidPages::default()),
            next_oid: AtomicU32::new(0),
            knobs: DebugKnobs::default(),
        }
    }

    fn decoded(rec: DbaseRecord) -> DecodedRecord {
        let (info, main) = rec.encode();
        DecodedRecord {
            start: Lsn(0),
            end: Lsn(0),
            xid: Xid(7),
            rmgr: RmgrId::Dbase,
            info,
            blocks: Vec::new(),
            main,
        }
    }

    fn put(ts: &TestStorage, path: &str, bytes: &[u8]) {
        let f = ts
            .vfs
            .open(Path::new(path), OpenMode::ReadWrite)
            .or_else(|_| ts.vfs.open(Path::new(path), OpenMode::CreateNew))
            .unwrap();
        f.write_all_at(bytes, 0).unwrap();
        f.sync_all().unwrap();
    }

    fn make_src(ts: &TestStorage, db: Oid) {
        ts.vfs.create_dir_all(&database_dir(db)).unwrap();
        put(ts, &format!("base/{db}/1259"), &[1u8; 100]);
        put(ts, &format!("base/{db}/1259_fsm"), &[2u8; 10]);
        put(ts, &format!("base/{db}/YUZHU_VERSION"), b"1\n");
    }

    fn contents(ts: &TestStorage, path: &str) -> Vec<u8> {
        let f = ts.vfs.open(Path::new(path), OpenMode::ReadOnly).unwrap();
        let mut buf = vec![0u8; usize::try_from(f.size().unwrap()).unwrap()];
        f.read_exact_at(&mut buf, 0).unwrap();
        buf
    }

    #[test]
    fn encode_decode_round_trip() {
        for rec in [
            DbaseRecord::CreateFileCopy { src: 1, dst: 16384 },
            DbaseRecord::Drop { db: 16384 },
        ] {
            let (info, main) = rec.encode();
            assert_eq!(DbaseRecord::decode(info, &main).unwrap(), rec);
        }
    }

    #[test]
    fn decode_rejects_bad_records_with_a_panic() {
        let (_, main) = DbaseRecord::CreateFileCopy { src: 1, dst: 2 }.encode();
        for (info, bytes) in [
            (DBASE_CREATE_FILE_COPY | 0x01, main.clone()),
            (DBASE_CREATE_FILE_COPY, main[..7].to_vec()),
            (DBASE_CREATE_FILE_COPY, vec![0; 9]),
            (DBASE_DROP, vec![0; 8]),
            (0x20, vec![0; 4]),
        ] {
            let e = DbaseRecord::decode(info, &bytes).unwrap_err();
            assert_eq!(
                (e.sqlstate, e.severity),
                (sqlstate::DATA_CORRUPTED, Severity::Panic)
            );
        }
    }

    #[test]
    fn fixed_bytes_match_the_design_examples() {
        // 例 1: CREATE_FILE_COPY（xid = 1234、src = 1、dst = 16384）。CRC と prev は 0。
        let (info, main) = DbaseRecord::CreateFileCopy { src: 1, dst: 16384 }.encode();
        let mut b = RecordBuilder::new(RmgrId::Dbase, info, Xid(1234));
        b.main_data(&main);
        let bytes = encode_record(&b, Lsn(0), true).unwrap();
        let mut want = vec![0x28, 0, 0, 0, 0, 0, 0, 0];
        want.extend_from_slice(&[0; 8]);
        want.extend_from_slice(&[0xd2, 0x04, 0, 0, 0, 0, 0, 0]);
        want.extend_from_slice(&[7, 0x00, 0, 0]);
        want.extend_from_slice(&[8, 0, 0, 0]);
        want.extend_from_slice(&[1, 0, 0, 0, 0x00, 0x40, 0, 0]);
        assert_eq!(bytes, want);
        // 例 2: DROP（db = 16384、xid = 0）。tot_len = 36。
        let (info, main) = DbaseRecord::Drop { db: 16384 }.encode();
        let mut b = RecordBuilder::new(RmgrId::Dbase, info, Xid::INVALID);
        b.main_data(&main);
        let bytes = encode_record(&b, Lsn(0), true).unwrap();
        assert_eq!(bytes.len(), 36);
        assert_eq!(&bytes[..4], &[0x24, 0, 0, 0]);
        assert_eq!(&bytes[24..28], &[7, 0x10, 0, 0]);
        assert_eq!(&bytes[28..], &[4, 0, 0, 0, 0x00, 0x40, 0, 0]);
    }

    #[test]
    fn lock_tags_are_deterministic_and_do_not_collide() {
        assert_eq!(name_lock_tag("a"), name_lock_tag("a"));
        assert_ne!(name_lock_tag("a"), name_lock_tag("b"));
        for db in [0, 1, 5, 16384, u32::MAX] {
            assert_ne!(frozen_lock_tag(db), name_lock_tag("a"));
            assert_ne!(frozen_lock_tag(db), LockTag::Database(db));
        }
        let LockTag::Object { obj, .. } = name_lock_tag("anything") else {
            panic!()
        };
        assert_ne!(obj & (1 << 63), 0);
    }

    #[test]
    fn redo_create_file_copy_is_idempotent_from_any_state() {
        let ts = TestStorage::new();
        let ctx = ctx_for(&ts);
        make_src(&ts, 1);
        let rec = decoded(DbaseRecord::CreateFileCopy { src: 1, dst: 9 });
        // dst が無い。
        redo(&ctx, &rec).unwrap();
        assert_eq!(contents(&ts, "base/9/1259"), vec![1u8; 100]);
        assert_eq!(contents(&ts, "base/9/YUZHU_VERSION"), b"1\n".to_vec());
        // 完全な dst（もう一度）。
        redo(&ctx, &rec).unwrap();
        // 不完全な dst（ファイルが足りず、余計なファイルがある）。
        ts.vfs.remove_dir_all(Path::new("base/9")).unwrap();
        ts.vfs.create_dir_all(Path::new("base/9")).unwrap();
        put(&ts, "base/9/1259", &[9u8; 3]);
        put(&ts, "base/9/stray", b"x");
        redo(&ctx, &rec).unwrap();
        let names: Vec<_> = ts
            .vfs
            .read_dir(Path::new("base/9"))
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert_eq!(contents(&ts, "base/9/1259"), vec![1u8; 100]);
    }

    #[test]
    fn redo_create_file_copy_without_src_leaves_dst_alone() {
        let ts = TestStorage::new();
        let ctx = ctx_for(&ts);
        ts.vfs.create_dir_all(Path::new("base/9")).unwrap();
        put(&ts, "base/9/1259", &[5u8; 4]);
        redo(
            &ctx,
            &decoded(DbaseRecord::CreateFileCopy { src: 1, dst: 9 }),
        )
        .unwrap();
        assert_eq!(contents(&ts, "base/9/1259"), vec![5u8; 4]);
        // dst も無いときは何も作らない。
        redo(
            &ctx,
            &decoded(DbaseRecord::CreateFileCopy { src: 1, dst: 10 }),
        )
        .unwrap();
        assert!(!ts.vfs.exists(Path::new("base/10")).unwrap());
    }

    #[test]
    fn redo_drop_removes_the_directory_and_invalid_pages() {
        let ts = TestStorage::new();
        let ctx = ctx_for(&ts);
        make_src(&ts, 9);
        let rel = RelFileLocator {
            spc_oid: DEFAULTTABLESPACE_OID,
            db_oid: 9,
            rel_number: RelFileNumber(1259),
        };
        ctx.invalid.lock().unwrap().record(rel, ForkNumber::Main, 3);
        assert!(!ctx.invalid.lock().unwrap().is_empty());
        let rec = decoded(DbaseRecord::Drop { db: 9 });
        redo(&ctx, &rec).unwrap();
        assert!(!ts.vfs.exists(Path::new("base/9")).unwrap());
        assert!(ctx.invalid.lock().unwrap().is_empty());
        // ディレクトリが無くても成功する（冪等）。
        redo(&ctx, &rec).unwrap();
    }

    #[test]
    fn remove_database_dir_syncs_the_parent() {
        let ts = TestStorage::new();
        make_src(&ts, 9);
        ts.vfs.sync_dir(Path::new("base")).unwrap();
        remove_database_dir(&ts.vfs, 9).unwrap();
        let after = ts.vfs.crash(crate::storage::vfs::CrashMode::DropUnsynced);
        assert!(!after.exists(Path::new("base/9")).unwrap());
    }

    #[test]
    fn created_dir_guard_removes_only_while_armed() {
        let ts = TestStorage::new();
        make_src(&ts, 9);
        let vfs: Arc<dyn Vfs> = Arc::new(ts.vfs.clone());
        let mut g = CreatedDirGuard::new(Arc::clone(&vfs), 9);
        g.disarm();
        assert!(!g.is_armed());
        g.remove_now();
        assert!(ts.vfs.exists(Path::new("base/9")).unwrap());
        let mut g = CreatedDirGuard::new(vfs, 9);
        drop(CreatedDirGuard::new(Arc::new(ts.vfs.clone()), 9)); // Drop は何もしない
        assert!(ts.vfs.exists(Path::new("base/9")).unwrap());
        g.remove_now();
        assert!(!ts.vfs.exists(Path::new("base/9")).unwrap());
    }

    #[test]
    fn held_lock_and_session_locks_leave_nothing_behind() {
        let locks = crate::txn::lock::LockManager::with_knobs(DebugKnobs::default());
        let id = BackendId(77);
        {
            let sl = SessionLocks::register(&locks, id, 1);
            let h =
                HeldLock::try_acquire(&locks, sl.id(), LockTag::Database(5), LockMode::AccessShare)
                    .unwrap();
            assert!(locks.holds(id, LockTag::Database(5), LockMode::AccessShare));
            drop(h);
            assert!(!locks.holds(id, LockTag::Database(5), LockMode::AccessShare));
            let leaked =
                HeldLock::try_acquire(&locks, id, name_lock_tag("x"), LockMode::Exclusive).unwrap();
            std::mem::forget(leaked);
            assert!(locks.holds(id, name_lock_tag("x"), LockMode::Exclusive));
        }
        assert!(!locks.holds(id, name_lock_tag("x"), LockMode::Exclusive));
    }
}
