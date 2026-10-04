# yuzhu M2 設計調査: バッファプールとファイル I/O 層（safe Rust）

対象: M2（永続化）のストレージ最下層。具体的には、ファイル I/O の抽象化（障害注入つき）、ストレージマネージャ（PostgreSQL の smgr / md.c に相当）、バッファプール（clock-sweep、ピン、コンテンツラッチ）の 3 つ。M3 の WAL・full page write・チェックポイントを後から足せる形にすることを前提にする。

- 前提資料: `spec/design/m1.md`（M1 の契約）、`spec/research/research-rust-db-arch.md` 第 4 節（unsafe なしのバッファプール）
- PostgreSQL の参照先は REL_17_STABLE。主に次のファイルを実際に読んで確認した（2026-10 時点の REL_17_STABLE ブランチ）。
  - `src/backend/storage/buffer/bufmgr.c` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/buffer/bufmgr.c>
  - `src/backend/storage/buffer/freelist.c` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/buffer/freelist.c>
  - `src/include/storage/buf_internals.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/storage/buf_internals.h>
  - `src/backend/storage/buffer/README` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/buffer/README>
  - `src/backend/storage/smgr/md.c` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/smgr/md.c>
  - `src/backend/storage/sync/sync.c` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/sync/sync.c>
  - `src/include/storage/bufpage.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/storage/bufpage.h>
  - `src/include/common/relpath.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/common/relpath.h>
- 「（未検証）」と書いた箇所は、ソースを読んで確かめていない記憶ベースの記述。

---

## 0. 結論（推奨の要約）

1. **3 層に分ける**: `vfs`（`Vfs` / `VfsFile` トレイト。本番用 `LocalVfs` と障害注入用 `SimVfs`）→ `smgr`（`StorageManager`。relfilenode → 1GB セグメントのファイル群、ブロック単位の read / write / extend / truncate / unlink / sync）→ `buffer`（`BufferPool`）。上位（heap、カタログ、将来の B+Tree）は `buffer` だけを使い、`smgr` を直接触らない（例外は initdb の一括書き込みと、M3 の WAL REDO の一部）。
2. **外部クレートは使わない**。`std::sync::{Mutex, RwLock, Condvar}`、`std::os::unix::fs::FileExt`（`read_exact_at` / `write_all_at`）で足りる。どれも safe API で、`#![forbid(unsafe_code)]` と両立する。
3. **ピンとラッチを型で分ける**（PostgreSQL の pin / content lock の分離をそのまま型にする）。`PinnedBuffer`（`Arc<BufferPool>` とフレーム番号を持つ `'static` な値。Drop で unpin）と、それを借用する短命の `PageReadGuard<'_>` / `PageWriteGuard<'_>`。自己参照構造体は生じない。単純な用途には、これを包んだクロージャ版 `with_page` / `with_page_mut` を用意する。
4. **フレームのロックは 2 つ**: 小さな `Mutex<FrameHeader>`（タグ、ピン数、usage_count、dirty、I/O 中など。**葉ロック**＝持ったまま他のロックを取らない）と、ページ本体の `RwLock<PageBuf>`（コンテンツラッチ）。マッピング表は 16 分割の `Mutex<HashMap<BufferTag, FrameId>>`。
5. **置換は PostgreSQL と同じ clock-sweep**（usage_count の上限 5、全フレームを一巡して見つからなければ `no unpinned buffers available` エラー）。初期状態と DROP 後のフレームは free list から取る。
6. **I/O 中はマッピング表のロックを持たない**。フレームに `io_in_progress` を立ててロックを外し、待つ側は `Condvar` で待つ（PostgreSQL の `StartBufferIO` / `BM_IO_IN_PROGRESS` 相当）。
7. **ページヘッダは PostgreSQL と同じ 24 バイト**（先頭 8 バイトが `pd_lsn`、次の 2 バイトが `pd_checksum`）。M2 から LSN 欄を確保し、**チェックサムは M2 から常に有効**にする。WAL のない M2 でも、torn write を読み込み時に検出できる（`XX001 data_corrupted`）。
8. **書き戻しは M2 から steal / no-force**: dirty ページは追い出し時、チェックポイント時（定期と `CHECKPOINT` 文）、正常終了時に書く。コミット時には書かない。M2 はクラッシュ安全ではない（直近のチェックポイント以降の変更は失われ、torn page も起こりうる）と仕様に明記する。M3 で WAL を足すと、`FlushBuffer` 相当の場所に「page LSN まで WAL を fsync してから書く」を 1 行足すだけで済む構造にしておく。
9. **fsync 失敗は PANIC 扱い**（PostgreSQL 12 以降の既定 `data_sync_retry = off` と同じ）。`StorageManager` を「壊れた」状態にして以後の書き込みをすべて拒否し、サーバを止める。再試行はしない。
10. **障害注入は `SimVfs` で行う**。ファイルごとに「永続化済みの内容」と「未 sync の書き込み」を分けて持ち、`crash()` で未 sync の書き込みを全部捨てる・セクタ単位で一部だけ残す（torn write）・全部残す、を選べるようにする。fsync エラー、EIO、ENOSPC、short write は、パス・操作・回数で指定するルールで注入する。乱数はシード固定の自前 SplitMix64 で、再現性を保証する。
11. **ピン漏れは型と計数の二重で防ぐ**。手動の `unpin()` は公開しない。スレッドローカルのピン数（`PinnedBuffer` は `!Send`）を文の終わりに検査し、テストでは 0 でなければ panic、本番ビルドでは PostgreSQL と同じ `buffer refcount leak` の WARNING を出して強制解放する。同じスレッドが同じフレームを二重にラッチしようとしたら、デバッグビルドでハングではなく panic させる。

---

## 1. PostgreSQL の構造（参照モデル）

### 1.1 ストレージマネージャ（smgr / md.c）

- リレーションの物理的な実体は **relfilenode**（PG 16 以降の名前では `RelFileLocator { spcOid, dbOid, relNumber }`）で識別する。OID（`pg_class.oid`）とは別物で、`TRUNCATE` や `VACUUM FULL` で付け替わる。
- 1 つのリレーションは **フォーク**（`MAIN_FORKNUM=0`、`FSM_FORKNUM=1`、`VISIBILITYMAP_FORKNUM=2`、`INIT_FORKNUM=3`）ごとに別ファイルになる（`relpath.h` の `ForkNumber` と `forkNames[]`）。ファイル名は `base/<dbOid>/<relNumber>`、`..._fsm`、`..._vm`、`..._init`。共有カタログ（`pg_database` など）は `global/<relNumber>`。
- 各フォークは **1GB（`RELSEG_SIZE` = 131072 ブロック × 8KB）ごとのセグメント**に分かれる。2 つ目以降は `<path>.1`、`<path>.2` …（md.c 冒頭のコメント）。
- 主な関数（md.c）: `mdcreate`、`mdexists`、`mdextend`（1 ブロック追加）、`mdzeroextend`（PG 16 以降。複数ブロックをまとめて 0 で伸ばし、大きい場合は `posix_fallocate` を使う）、`mdreadv` / `mdwritev`（PG 17 でベクタ化）、`mdnblocks`、`mdtruncate`、`mdunlink`、`mdimmedsync`、`mdregistersync`。
- 書き込みのたびに fsync はしない。`register_dirty_segment` が「このセグメントはチェックポイントで fsync が必要」という要求を checkpointer に送り（sync.c）、チェックポイント時にまとめて fsync する。
- **`mdunlink` は main フォークの最初のセグメントをすぐには消さない**。長さ 0 に切り詰め、削除要求を次のチェックポイント後に回す。目的は、チェックポイント前に同じ relfilenumber が再割り当てされ、クラッシュ後の WAL 再生で古いリレーションの削除記録が新しいリレーションのファイルを消してしまう事故を防ぐこと（md.c の `mdunlink` 直前のコメント、255〜268 行付近）。
- エラー文言の例（md.c）: `could not extend file "%s": %m` + ヒント `Check free disk space.`、`could not read blocks %u..%u in file "%s": read only %zu of %zu bytes`。

### 1.2 バッファマネージャ（bufmgr.c / freelist.c / buf_internals.h）

- **`BufferTag = { spcOid, dbOid, relNumber, forkNum, blockNum }`**（`buf_internals.h` の `struct buftag`）。
- バッファごとに `BufferDesc` があり、状態は 1 本の 32 ビット atomic に詰めてある: 参照数（ピン）、usage_count、フラグ `BM_LOCKED`（ヘッダのスピンロック）、`BM_DIRTY`、`BM_VALID`、`BM_TAG_VALID`、`BM_IO_IN_PROGRESS`、`BM_IO_ERROR`、`BM_JUST_DIRTIED`、`BM_PIN_COUNT_WAITER`、`BM_CHECKPOINT_NEEDED`、`BM_PERMANENT`。
- ページ本体の排他は **content lock（LWLock、共有 / 排他）**。ピンとは別物で、「ピン = 追い出させない」「content lock = 中身を読む・書く権利」。content lock は必ずピンを持った状態で取る（`src/backend/storage/buffer/README` の "Buffer Access Rules"）。
- マッピング表（タグ → バッファ番号）は `NUM_BUFFER_PARTITIONS`（既定 128）個のパーティションに分けた LWLock で保護する（README）。
- 置換は **clock-sweep**（freelist.c の `StrategyGetBuffer`）。ピンされていなければ usage_count を 1 減らし、0 のものを選ぶ。usage_count はピンのたびに増え、上限は `BM_MAX_USAGE_COUNT = 5`。全バッファを数えるカウンタ `trycounter = NBuffers` が、usage_count を減らすたびに戻り、0 になったら `elog(ERROR, "no unpinned buffers available")`（freelist.c 353 行付近）。
- 追い出し（PG 16 以降の `GetVictimBuffer`）: 候補をピンし、dirty なら content lock を共有で取って `FlushBuffer` で書く。その後 `InvalidateVictimBuffer` で、**旧タグのパーティションロック → ヘッダロック**の順に取り、「ピン数が 1 のまま、かつ dirty でない」ことを再確認してから旧マッピングを消す。誰かが割り込んでピンしていたら諦めて別の候補を探す。新タグの登録は別に行い、同じタグを同時に読み込もうとした別プロセスに先を越されていたら、自分の候補を返して相手のバッファを使う（bufmgr.c `BufferAlloc`）。
- `FlushBuffer`（bufmgr.c 3863 行付近）: ヘッダロック中にページ LSN を読み、`BM_JUST_DIRTIED` を落とし、**`XLogFlush(recptr)` で WAL をページ LSN まで fsync してから**書く。共有 content lock しか持っていないので、ヒントビットを書き換えられる可能性があり、チェックサムは `PageSetChecksumCopy` でページのコピーに対して計算して、そのコピーを書く。
- チェックポイント（`BufferSync`）: 開始時点で dirty なバッファに `BM_CHECKPOINT_NEEDED` を立て、それだけを書く（チェックポイント中に新たに dirty になったものは対象外にし、終わらなくなるのを防ぐ）。書く順は `ckpt_buforder_comparator` でタグ順に並べ、連続 I/O にする。
- ピン漏れ検出: バックエンドごとの `PrivateRefCountArray`（と溢れた分のハッシュ表）でピンを数え、リソースオーナーの解放時に残っていれば `elog(WARNING, "buffer refcount leak: %s", ...)`（bufmgr.c 3654 行付近）。
- VACUUM の pruning は「ピン数が自分の 1 つだけ」の状態を要求する **cleanup lock**（`LockBufferForCleanup`）を使う。M5 で必要になる。
- リレーションの拡張は relation extension lock（`LockRelationForExtension`）で直列化する（`ExtendBufferedRelShared`）。

### 1.3 fsync 失敗の扱い

2018 年の「fsyncgate」以降、PostgreSQL は fsync の失敗を再試行しない。Linux では fsync が EIO を返したあと、失われた dirty ページがクリーン扱いになり、再度の fsync が成功してしまうことがあるため。fd.c の `data_sync_elevel()` は、GUC `data_sync_retry` が off（既定）なら ERROR を PANIC に格上げする。md.c の `register_dirty_segment` などで `ereport(data_sync_elevel(ERROR), ...)` が使われていることは確認した。`data_sync_retry` の説明: <https://www.postgresql.org/docs/17/runtime-config-error-handling.html>（fd.c 側の実装は未確認）

---

## 2. レイヤ構成とモジュール配置

```
yuzhu-core/src/storage/
├── mod.rs        TableStore など既存の境界 + 再エクスポート
├── memory.rs     M1 のメモリ実装（M2 以降もテスト用に残す）
├── vfs/
│   ├── mod.rs    Vfs / VfsFile トレイト、OpenMode
│   ├── local.rs  LocalVfs（std::fs）
│   └── sim.rs    SimVfs（障害注入、#[cfg(any(test, feature = "sim"))] ではなく常にビルドする。結合テストと yuzhu-server のテストから使うため）
├── smgr.rs       StorageManager, RelFileLocator, ForkNumber, BlockNumber, セグメント管理, pending sync
├── page.rs       PageBuf, PageHeader の読み書き, チェックサム
├── buffer/
│   ├── mod.rs    BufferPool の公開 API
│   ├── frame.rs  Frame, FrameHeader
│   ├── table.rs  分割マッピング表
│   ├── clock.rs  clock-sweep と free list
│   └── guard.rs  PinnedBuffer, PageReadGuard, PageWriteGuard, ピン追跡
├── checkpoint.rs チェックポイント手順（M2: ページ書き出し + fsync + 制御ファイル）
└── heap/ ...     （別の調査の範囲）
```

依存の向きは `heap → buffer → smgr → vfs` の一方向。`buffer` は `smgr` を `Arc<StorageManager>` で持つ。M3 の WAL は `buffer` から見て「フラッシュ要求の受け口」というトレイトとして注入する（§6）。`buffer` が `wal` モジュールに直接依存しないようにする。

---

## 3. I/O 抽象化層（vfs）

### 3.1 トレイト

```rust
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// 既存ファイルを読み書きで開く。なければ NotFound。
    ReadWrite,
    /// 新規作成（既にあれば AlreadyExists）。O_CREAT | O_EXCL 相当。
    CreateNew,
    /// 読み取り専用。
    ReadOnly,
}

/// ファイルシステム全体の操作。データディレクトリ配下のパスだけを扱う。
pub trait Vfs: Send + Sync + std::fmt::Debug {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Arc<dyn VfsFile>>;
    fn exists(&self, path: &Path) -> io::Result<bool>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// 同じディレクトリ内での原子的な置き換え（制御ファイルの更新に使う）。
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>; // DROP DATABASE（M5）用
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    /// ディレクトリの fsync。ファイルの作成・削除・rename を永続化するのに必要。
    fn sync_dir(&self, path: &Path) -> io::Result<()>;
}

/// 開いたファイル。位置を持たない（pread / pwrite 相当）ので &self で並行に使える。
pub trait VfsFile: Send + Sync + std::fmt::Debug {
    /// buf.len() バイトちょうど読む。EOF を跨げば UnexpectedEof。
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    /// buf 全体を書く（short write は内部で続きを書くか、エラーにする）。
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    /// 切り詰め・伸長（伸ばした部分は 0）。
    fn set_len(&self, len: u64) -> io::Result<()>;
    /// fdatasync 相当。
    fn sync_data(&self) -> io::Result<()>;
    /// fsync 相当（サイズ変更を伴ったあとはこちら）。
    fn sync_all(&self) -> io::Result<()>;
}
```

設計上の判断:

- **`io::Result` を返し、SQLSTATE への変換は `smgr` で行う**。VFS は SQL を知らない層にしておく。M3 の WAL ファイル、コミットログ（clog）、制御ファイルも同じ VFS を通すので、障害注入がすべての永続化経路に効く。
- **位置なし API（`read_exact_at` / `write_all_at`）だけにする**。`std::os::unix::fs::FileExt` がそのまま safe に提供している（Linux のみ対応という前提とも合う）。`Seek` 状態を共有しないので、`&self` で複数スレッドから同時に使える。
- `Arc<dyn VfsFile>` を返すのは、smgr がセグメントのハンドルをキャッシュし、ロックを外したあとも I/O に使えるようにするため。
- O_DIRECT は使わない（アライメント済みバッファの確保に unsafe か外部クレートが要る）。PostgreSQL も既定ではページキャッシュ経由（`debug_io_direct` は開発用）。`posix_fadvise` や `posix_fallocate` も std にないので使わない。拡張は 0 埋めの書き込みで行う。
- ファイルディスクリプタの上限: PostgreSQL は `max_files_per_process` と仮想 FD（fd.c の VFD の LRU）で管理している。yuzhu の M2 はテーブル数が小さい前提で、開いたセグメントはリレーションを閉じるまで保持し、上限管理は後回しにする（`QUESTIONS.md` に記録する候補）。

### 3.2 LocalVfs（本番用）

```rust
#[derive(Debug, Default)]
pub struct LocalVfs;

#[derive(Debug)]
struct LocalFile { file: std::fs::File }

impl VfsFile for LocalFile {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(&self.file, buf, off)
    }
    fn write_all_at(&self, buf: &[u8], off: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(&self.file, buf, off)
    }
    fn sync_data(&self) -> io::Result<()> { self.file.sync_data() }
    // ...
}
// sync_dir は std::fs::File::open(dir)?.sync_all()（Linux ではディレクトリを開いて fsync できる）
```

### 3.3 SimVfs（障害注入用）

テスト専用だが本体クレートに置き、結合テストからも使えるようにする。メモリ上の疑似ファイルシステムで、**「OS のページキャッシュにはあるが、まだディスクに届いていない」状態を再現する**のが要点。

```rust
#[derive(Debug, Clone)]
pub struct SimVfs { inner: Arc<Mutex<SimState>> }

#[derive(Debug)]
struct SimState {
    files: HashMap<PathBuf, SimFile>,
    /// 未 sync のディレクトリ操作（作成・削除・rename）。sync_dir で確定する。
    pending_dir_ops: Vec<DirOp>,
    durable_dirs: BTreeSet<PathBuf>,
    faults: FaultPlan,
    rng: SplitMix64,        // シード固定。外部クレート不要（10 行で書ける）
    op_counter: u64,        // 全 I/O 操作の通し番号（「n 回目の write で失敗」に使う）
    dead: bool,             // crash() 後に古いハンドルから来た I/O はすべて失敗させる
    stats: SimStats,
}

#[derive(Debug, Clone)]
struct SimFile {
    durable: Vec<u8>,                 // 最後の sync 時点の内容
    current: Vec<u8>,                 // 読み出しで見える内容
    unsynced: Vec<(u64, u64)>,        // 未 sync の書き込み範囲 (offset, len)。セクタ単位に丸める
    durable_len: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum CrashMode {
    /// 未 sync の書き込みをすべて失う（もっとも厳しい通常ケース）。
    DropUnsynced,
    /// 未 sync の書き込みを、セクタ（既定 512B）単位でランダムに一部だけ残す。
    /// 8KB ページの一部だけが新しい = torn page を作る。
    TornSectors { sector: u32, keep_probability: f64 },
    /// 未 sync の書き込みもすべて残す（OS は生きていて、プロセスだけ死んだ場合）。
    KeepAll,
}

impl SimVfs {
    pub fn new(seed: u64) -> Self;
    pub fn set_faults(&self, plan: FaultPlan);
    /// 「電源断」。現在の状態を CrashMode に従って永続化済みの状態へ巻き戻した、
    /// 新しい SimVfs を返す。自分自身は dead になり、以後の I/O は EIO。
    pub fn crash(&self, mode: CrashMode) -> SimVfs;
    pub fn stats(&self) -> SimStats; // 書き込み回数、fsync 回数など（性能テストの指標にも使う）
}

#[derive(Debug, Clone, Default)]
pub struct FaultPlan { pub rules: Vec<FaultRule> }

#[derive(Debug, Clone)]
pub struct FaultRule {
    pub path_contains: Option<String>,   // 例: "base/5/16384" や "pg_wal"
    pub op: FaultOp,                     // Read / Write / SyncData / SyncAll / SetLen / Open / Remove / Rename / SyncDir
    pub trigger: Trigger,                // Nth(n) / EveryNth(n) / Probability(p) / Always / AfterOpCount(n)
    pub effect: FaultEffect,
}

#[derive(Debug, Clone)]
pub enum FaultEffect {
    Error(io::ErrorKind),        // EIO 相当は io::ErrorKind::Other、ENOSPC は StorageFull
    /// 先頭 n バイトだけ書いてからエラー（short write + ENOSPC の再現）。
    ShortWrite { bytes: usize, then: io::ErrorKind },
    /// Linux の fsync 失敗の再現: エラーを返し、さらに未 sync の書き込みを捨てつつ
    /// 「sync 済み」とみなす（次の fsync は成功してしまう）。fsyncgate の挙動そのもの。
    FsyncFailAndForget,
    /// 読み出したバイト列の 1 ビットを反転（チェックサム検出のテスト）。
    BitFlipOnRead,
    /// 指定時間ブロックする（デッドロック・タイムアウト系のテスト）。
    Delay(std::time::Duration),
}
```

ディレクトリ操作の永続性: ファイルの作成・削除・rename は、**親ディレクトリを `sync_dir` するまで、クラッシュ時に巻き戻りうる**ものとして扱う（ext4 の実挙動より厳しいが、POSIX が保証するのはここまで）。これで「新しいリレーションファイルを作ったのにディレクトリを fsync していない」バグをテストで捕まえられる。

### 3.4 クラッシュの再現方法（テストの型）

yuzhu はシングルプロセス・マルチスレッドなので、「プロセスを kill する」代わりに次のようにする。

```rust
let vfs = SimVfs::new(seed);
let db = Database::open(Arc::new(vfs.clone()), opts)?;
run_workload(&db)?;
// シャットダウン処理を通さずに捨てる
let after = vfs.crash(CrashMode::TornSectors { sector: 512, keep_probability: 0.5 });
drop(db); // 残ったスレッドが I/O しても、旧 vfs は dead なので EIO になるだけ
let db2 = Database::open(Arc::new(after), opts)?; // 起動処理（M3 以降はリカバリ）
check_invariants(&db2)?;
```

このため、**`Drop` 実装の中でフラッシュや fsync をしてはいけない**（Drop で書き出すと「クラッシュ」がクラッシュにならない）。終了処理は必ず明示的な `Database::shutdown()` で行う。これはコーディング規約として `spec/design/m2.md` に書くべき項目。

---

## 4. ストレージマネージャ（smgr）

### 4.1 識別子

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct RelFileNumber(pub u32);          // PostgreSQL と同じ 32 ビット（Oid と同じ空間から割り当てる）

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct RelFileLocator {
    pub spc_oid: Oid,          // 1663 = pg_default, 1664 = pg_global
    pub db_oid: Oid,           // 共有カタログは 0
    pub rel_number: RelFileNumber,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
#[repr(u8)]
pub enum ForkNumber { Main = 0, Fsm = 1, VisibilityMap = 2, Init = 3 }

pub type BlockNumber = u32;
pub const INVALID_BLOCK_NUMBER: BlockNumber = u32::MAX;  // PostgreSQL の InvalidBlockNumber = 0xFFFFFFFF
pub const MAX_BLOCK_NUMBER: BlockNumber = u32::MAX - 1;
pub const BLCKSZ: usize = 8192;
pub const RELSEG_SIZE: u32 = 131_072;                      // 1GB / 8KB

/// バッファプールのキー。PostgreSQL の BufferTag と同じ 5 要素。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct BufferTag {
    pub rel: RelFileLocator,
    pub fork: ForkNumber,
    pub block: BlockNumber,
}
```

- `Ord` を導出しておくと、チェックポイントでタグ順に並べられる（PostgreSQL の `ckpt_buforder_comparator` と同じ効果）。
- パス: `spc_oid == 1664` なら `global/<rel>`、`1663` なら `base/<db_oid>/<rel>`。フォーク接尾辞は `""`、`"_fsm"`、`"_vm"`、`"_init"`。セグメント番号が 1 以上なら `.<segno>`。これで M5 の CREATE DATABASE は「`base/<新OID>/` を作ってテンプレートからコピー」で実装できる。ユーザー定義テーブルスペース（`pg_tblspc/`）は対象外だが、`spc_oid` を最初から持たせておくのでレイアウトの変更は要らない。
- M2 で使うのは Main フォークだけ。FSM と VM のフォーク番号は、ファイル名の互換と M5 の VACUUM のために予約しておく。

### 4.2 StorageManager

```rust
#[derive(Debug)]
pub struct StorageManager {
    vfs: Arc<dyn Vfs>,
    data_dir: PathBuf,
    /// 開いているリレーションフォーク。ロックは短時間だけ持つ（I/O 中は持たない）。
    rels: Mutex<HashMap<(RelFileLocator, ForkNumber), Arc<RelFork>>>,
    /// 前回のチェックポイント以降に書き込まれ、fsync が必要なセグメント。
    pending_sync: Mutex<BTreeSet<SegmentId>>,
    /// fsync が一度でも失敗したら true。以後の書き込みはすべて拒否する。
    broken: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct SegmentId { pub rel: RelFileLocator, pub fork: ForkNumber, pub segno: u32 }

#[derive(Debug)]
struct RelFork {
    /// セグメントのハンドルとキャッシュしたブロック数。
    state: Mutex<RelForkState>,
    /// リレーション拡張の直列化（PostgreSQL の relation extension lock 相当）。
    extension: Mutex<()>,
}

#[derive(Debug)]
struct RelForkState {
    segments: Vec<Arc<dyn VfsFile>>,
    nblocks: BlockNumber, // キャッシュ。yuzhu は単一プロセスなので、共有キャッシュが常に正しい
}

impl StorageManager {
    pub fn new(vfs: Arc<dyn Vfs>, data_dir: PathBuf) -> Self;

    pub fn create(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<()>;  // 空のセグメント 0 を作り、親ディレクトリを sync_dir
    pub fn exists(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<bool>;
    pub fn nblocks(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<BlockNumber>;

    pub fn read_block(&self, tag: BufferTag, buf: &mut PageBuf) -> Result<()>;
    /// 既存ブロックの上書き。pending_sync に登録する（fsync はしない）。
    pub fn write_block(&self, tag: BufferTag, buf: &PageBuf) -> Result<()>;
    /// 末尾に 1 ブロック追加。呼び出し側が extension ロックを持つこと（ExtensionGuard で強制）。
    pub fn extend(&self, g: &ExtensionGuard<'_>, buf: &PageBuf) -> Result<BlockNumber>;
    /// n ブロックを 0 で追加（mdzeroextend 相当）。
    pub fn zero_extend(&self, g: &ExtensionGuard<'_>, n: u32) -> Result<BlockNumber>;
    pub fn lock_extension(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<ExtensionGuard<'_>>;

    /// nblocks に切り詰める。呼び出し側が先にバッファを破棄しておくこと（§5.8）。
    pub fn truncate(&self, rel: RelFileLocator, fork: ForkNumber, nblocks: BlockNumber) -> Result<()>;
    /// 全フォーク・全セグメントを削除し、pending_sync から除く。
    pub fn unlink(&self, rel: RelFileLocator) -> Result<()>;
    /// 1 リレーションを即座に fsync（initdb や、WAL を通さない一括作成用。mdimmedsync 相当）。
    pub fn immedsync(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<()>;
    /// チェックポイント用: pending_sync を全部 fsync する。失敗したら broken にして Err。
    pub fn sync_pending(&self) -> Result<()>;
    /// リレーションのハンドルを閉じる（DROP 後や FD 節約用）。
    pub fn close(&self, rel: RelFileLocator);
}
```

振る舞いの要点:

- **ブロック番号 → (セグメント番号, オフセット)** は `segno = blk / RELSEG_SIZE`、`offset = (blk % RELSEG_SIZE) as u64 * BLCKSZ as u64`。
- **`nblocks` のキャッシュ**: PostgreSQL は複数プロセスなので毎回 `lseek(SEEK_END)` で長さを調べる（キャッシュはリカバリ中のみ）。yuzhu は 1 プロセスで全ファイルを所有するので、`RelForkState.nblocks` を信頼できる。開いた時点で、セグメントを順に開いて「最後以外は ちょうど 1GB」を検査して求める。最後以外のセグメントが 1GB 未満なら `XX001 data_corrupted`（PostgreSQL の mdnblocks も同様の不整合を検査する）。
- **ファイル末尾が中途半端**（ブロック境界でない）なら、末尾の端数は無視して切り捨て扱いにし、WARNING を出す。M2 ではクラッシュ時の extend 途中で起こりうる。
- **読み込みが EOF を跨ぐ**場合は `58030 io_error`（PostgreSQL の文言 `could not read blocks %u..%u in file "%s": read only %zu of %zu bytes`）。
- **ENOSPC** は `53100 disk_full`、メッセージ `could not extend file "%s": No space left on device`、ヒント `Check free disk space.`。extend で ENOSPC が出たら、`set_len` で元の長さに戻してから返す（中途半端なブロックを残さない）。
- **pending_sync**: `write_block` / `extend` のたびに `SegmentId` を登録する。`unlink` と `truncate` は対象セグメントを除く（PostgreSQL の「forget fsync requests」）。チェックポイントは `sync_pending` で全部 fsync する。
- **fsync 失敗**: `broken` を立て、エラー（Severity::Panic、SQLSTATE `58030`）を返す。上位はこれを受けたら全セッションを FATAL で切ってサーバを止める。再起動後は M2 では「最後のチェックポイントまで戻る」、M3 以降は WAL リカバリ。**同じ fsync を再試行しない**。
- **ファイル作成・削除の永続化**: `create` の後と `unlink` の後に親ディレクトリを `sync_dir` する。PostgreSQL は削除のディレクトリ fsync を省くが（チェックポイント後に消すので不要）、yuzhu の M2 は単純に毎回する。

### 4.3 DROP と relfilenumber の再利用

- CREATE TABLE → ROLLBACK ならファイルは abort 時に消す。DROP TABLE なら **コミット時に**消す（PostgreSQL の `RelationDropStorage` / `smgrDoPendingDeletes`。<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/catalog/storage.c>）。この「保留中の削除リスト」はトランザクションに持たせる（txn 側の設計）。
- relfilenumber の再利用事故（§1.1）は M3 で WAL 再生が入ってから問題になる。M2 では即座に削除してよいが、**M3 で PostgreSQL と同じ「最初のセグメントを 0 バイトに切り詰め、次のチェックポイント後に削除」を入れる**ことをあらかじめ決めておく。新しい relfilenumber は OID カウンタから取り、同名のファイルが既にあれば次を取る（PostgreSQL の `GetNewRelFileNumber` と同じ考え方）。

---

## 5. バッファプール

### 5.1 データ構造

```rust
pub const MAX_USAGE_COUNT: u8 = 5;          // BM_MAX_USAGE_COUNT
const NUM_PARTITIONS: usize = 16;           // 2 の冪。PostgreSQL は 128

pub struct PageBuf(pub [u8; BLCKSZ]);       // Box<PageBuf> で確保する（スタックに 8KB を置かない）

#[derive(Debug)]
pub struct BufferPool {
    frames: Box<[Frame]>,                                     // 起動時に固定数を確保。Arc<Frame> は不要
    partitions: [Mutex<HashMap<BufferTag, FrameId>>; NUM_PARTITIONS],
    free_list: Mutex<Vec<FrameId>>,
    clock_hand: AtomicUsize,
    smgr: Arc<StorageManager>,
    wal: Arc<dyn WalFlush>,                                   // M2 は NoWal（何もしない）
    checkpoint_lock: Mutex<()>,                               // チェックポイントは同時に 1 つ
    stats: BufferStats,                                       // ヒット数、読み込み数、追い出し時の書き込み数（AtomicU64）
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FrameId(u32);

#[derive(Debug)]
struct Frame {
    header: Mutex<FrameHeader>,     // 葉ロック: 持ったまま他のロックを取らない
    io_done: Condvar,               // header の Mutex と組で使う
    content: RwLock<Box<PageBuf>>,  // コンテンツラッチ
}

#[derive(Debug)]
struct FrameHeader {
    tag: Option<BufferTag>,         // None = 未使用（BM_TAG_VALID の否定）
    valid: bool,                    // BM_VALID: 中身が読み込み済み
    dirty: bool,                    // BM_DIRTY
    io_in_progress: bool,           // BM_IO_IN_PROGRESS
    io_error: bool,                 // BM_IO_ERROR
    checkpoint_needed: bool,        // BM_CHECKPOINT_NEEDED
    pin_count: u32,
    usage_count: u8,
    cleanup_waiter: bool,           // M5: cleanup lock 待ち（BM_PIN_COUNT_WAITER）
}
```

- PostgreSQL は状態を 1 本の atomic に詰めてスピンロックで扱うが、yuzhu は `Mutex<FrameHeader>` にする。safe Rust で CAS ループを正しく書くより単純で、std の Mutex は競合のないときは futex を呼ばないので十分速い。性能が問題になったら、`pin_count` と `usage_count` だけを `AtomicU32` に詰める最適化を後で行える（API は変わらない）。
- `frames` は `Box<[Frame]>` で、`Arc<Frame>` にしない。ガードは `Arc<BufferPool>` とフレーム番号を持てば足り、フレームごとの `Arc` の参照数操作が要らない。
- コンテンツは `RwLock<Box<PageBuf>>`。`std::sync::RwLock` は再入不可で、Linux の実装は書き込み待ちがいると新しい読み込みを待たせる（同じスレッドで読み込みロックを二重に取ると、間に書き込み待ちが入ったときにデッドロックする）。二重取得の検出は §5.7。

### 5.2 ロックの順序（デッドロック回避の規則）

| レベル | ロック | 規則 |
|---|---|---|
| 1 | `checkpoint_lock` | チェックポイントだけが取る |
| 2 | コンテンツラッチ（`Frame.content`） | ピンを持っているときだけ取れる（型で強制）。複数取るときは §5.6 の順序 |
| 3 | `RelFork.extension` | ラッチを持ったまま取ってよい（heap の「最後のページが満杯 → 拡張」） |
| 4 | マッピング表のパーティション | 一度に 1 つだけ。持ったままディスク I/O をしない |
| 5 | `Frame.header`（葉） | 持ったまま他のロックを一切取らない。持ったまま I/O しない |
| 6 | `smgr` 内部の `rels` / `RelForkState` / `pending_sync` | 葉。I/O は Arc でハンドルを取り出してからロックの外で行う |

- 上の段から下の段へだけ取る。逆向きは禁止。
- **葉ロックを持ったまま待たない**: `io_done.wait(header_guard)` は header の Mutex を手放して待つので規則に反しない。
- **追い出しで victim のコンテンツラッチを取ってもデッドロックしない**: victim はピン数 0 で選ばれ、ラッチはピンを持つ者しか取れない（型で保証）。追い出し側は自分で victim をピンしてからラッチを取るので、その時点で他者がラッチを持っている可能性は「victim を選んだ直後に誰かがピンしてラッチを取った」場合だけで、そのときは §5.4 の再確認で victim を諦める。追い出し側は victim のラッチを `try_read` で取り、取れなければ別の victim を探すようにすれば、待ちそのものが発生しない。

### 5.3 ピンとガード（自己参照を避ける型）

```rust
/// ピン。'static で、カーソルや Executor の中に保持できる。Drop で unpin。
#[derive(Debug)]
pub struct PinnedBuffer {
    pool: Arc<BufferPool>,
    frame: FrameId,
    tag: BufferTag,
    _not_send: PhantomData<*const ()>,   // !Send / !Sync（スレッドローカルのピン追跡のため。unsafe は不要）
}

impl PinnedBuffer {
    pub fn tag(&self) -> BufferTag;
    pub fn block(&self) -> BlockNumber;
    pub fn read(&self) -> PageReadGuard<'_>;                    // 共有ラッチ（LockBuffer(BUFFER_LOCK_SHARE)）
    pub fn write(&self) -> PageWriteGuard<'_>;                  // 排他ラッチ（BUFFER_LOCK_EXCLUSIVE）
    pub fn try_write(&self) -> Option<PageWriteGuard<'_>>;      // ConditionalLockBuffer
    /// M5: 自分以外のピンがなくなるまで待って排他ラッチを取る（LockBufferForCleanup）。
    pub fn write_for_cleanup(&self) -> PageWriteGuard<'_>;
}

impl Clone for PinnedBuffer { /* pin_count を 1 増やす（IncrBufferRefCount） */ }
impl Drop for PinnedBuffer  { /* pin_count を 1 減らす。cleanup_waiter がいれば通知 */ }

/// 共有ラッチ。PinnedBuffer を借用するので、ピンより長生きできない。
pub struct PageReadGuard<'a> {
    pin: &'a PinnedBuffer,
    guard: RwLockReadGuard<'a, Box<PageBuf>>,
}
impl Deref for PageReadGuard<'_> { type Target = Page; /* 型付きビュー */ }

/// 排他ラッチ。最初に可変参照を取った時点で dirty を立てる。
pub struct PageWriteGuard<'a> {
    pin: &'a PinnedBuffer,
    guard: RwLockWriteGuard<'a, Box<PageBuf>>,
    dirtied: bool,
}
impl PageWriteGuard<'_> {
    pub fn page(&self) -> &Page;
    pub fn page_mut(&mut self) -> &mut Page;    // dirtied = true
    /// M3: WAL レコードを書いたあとに呼ぶ（PageSetLSN）。
    pub fn set_lsn(&mut self, lsn: Lsn);
}
impl Drop for PageWriteGuard<'_> {
    // dirtied なら header を短時間ロックして dirty = true（MarkBufferDirty）。
    // Drop::drop はフィールドの drop より先に走るので、dirty を立てる時点ではまだラッチを持っている。
}
```

- `PinnedBuffer` は所有権を持つだけで何も借用しないので `'static`。ラッチは `&'a PinnedBuffer` を借用する短命のガードで、**同じ構造体にオーナーとそれから借用したガードを同居させない**。これが `research-rust-db-arch.md` §4.1 の方式 2。
- dirty の記録を `PageWriteGuard` の Drop でまとめて行うのは、「変更したのに `mark_dirty` を忘れる」バグを型で防ぐため。PostgreSQL では `MarkBufferDirty` を明示的に呼ぶが、呼び忘れはデータ消失の典型的な原因になる。
  - ただし M3 では順序が重要になる（PostgreSQL の規則: 排他ラッチ → 変更 → `MarkBufferDirty` → `XLogInsert` → `PageSetLSN` → ラッチ解放。`src/backend/access/transam/README` の "Write-Ahead Log Coding"）。yuzhu では「ラッチ解放の直前に dirty を立てる」ので、WAL 挿入より後に dirty が立つ。ラッチを持っている間はフラッシュできない（フラッシュには共有ラッチが要る）ので、この順序の違いは問題にならない、と考えている（未検証: M3 の設計時にチェックポイントの開始処理との競合を再確認すること）。
- `Page` は `PageBuf` の上に型付きアクセサ（`lsn()`、`lower()`、`upper()` など）を被せた `#[repr(transparent)]` の新型。バイト列の解釈は明示的に `u16::from_le_bytes` などで行い、`transmute` やポインタキャストは使わない。

クロージャ版（単純な用途用）:

```rust
impl BufferPool {
    pub fn with_page<R>(self: &Arc<Self>, tag: BufferTag, f: impl FnOnce(&Page) -> Result<R>) -> Result<R> {
        let pin = self.read_buffer(tag)?;
        let g = pin.read();
        f(&g)
    }
    pub fn with_page_mut<R>(self: &Arc<Self>, tag: BufferTag, f: impl FnOnce(&mut Page) -> Result<R>) -> Result<R>;
}
```

### 5.4 公開 API

```rust
impl BufferPool {
    pub fn new(nframes: usize, smgr: Arc<StorageManager>, wal: Arc<dyn WalFlush>) -> Arc<Self>;

    /// 既存ブロックをピンして返す（ReadBuffer）。ラッチは取らない。
    pub fn read_buffer(self: &Arc<Self>, tag: BufferTag) -> Result<PinnedBuffer>;
    /// ディスクから読まず、ゼロ初期化したページとしてピンする（RBM_ZERO_AND_LOCK 相当。M3 の REDO で FPI を適用するときに使う）。
    pub fn read_buffer_zeroed(self: &Arc<Self>, tag: BufferTag) -> Result<PinnedBuffer>;
    /// リレーションを 1 ブロック伸ばし、新しいページをピンして返す（ExtendBufferedRel）。
    /// 内部で smgr の extension ロックを取り、ディスクには 0 埋めページを書いてから返す。
    pub fn extend(self: &Arc<Self>, rel: RelFileLocator, fork: ForkNumber) -> Result<PinnedBuffer>;
    pub fn nblocks(&self, rel: RelFileLocator, fork: ForkNumber) -> Result<BlockNumber>;

    /// チェックポイントの前半: 開始時点で dirty なページをすべて書く（BufferSync）。
    pub fn flush_all_for_checkpoint(&self) -> Result<FlushStats>;
    /// リレーションのバッファを書かずに捨てる（DROP / TRUNCATE 用。DropRelationBuffers）。
    /// ピンが残っていたら internal error（呼び出し側がテーブルロックで排他していること）。
    pub fn drop_relation_buffers(&self, rel: RelFileLocator, fork: ForkNumber, from_block: BlockNumber) -> Result<()>;
    pub fn drop_database_buffers(&self, db_oid: Oid) -> Result<()>;      // M5: DROP DATABASE
    pub fn flush_relation_buffers(&self, rel: RelFileLocator) -> Result<()>; // CREATE DATABASE のコピー前など

    // 観測・テスト用
    pub fn pinned_frames(&self) -> usize;
    pub fn debug_pins(&self) -> Vec<(BufferTag, u32)>;
    pub fn stats(&self) -> BufferStatsSnapshot;
}
```

### 5.5 read_buffer の手順

```
read_buffer(tag):
  p = partition(hash(tag))
  lock p
  if let Some(fid) = map.get(tag):
      lock header(fid)
      pin_count += 1; usage_count = min(usage_count + 1, 5)
      unlock header; unlock p
      wait_io(fid)            # io_in_progress なら io_done で待つ。io_error なら自分で読み直す
      return PinnedBuffer
  unlock p

  fid = get_victim()          # §5.6。victim は pin_count = 1（自分のピン）で返る。dirty なら書いてある
  # 旧タグの削除（InvalidateVictimBuffer）
  if victim has old tag:
      lock partition(old); lock header
      if pin_count != 1 || dirty: unlock; unpin victim; retry from top
      map.remove(old); header.tag = None; valid = false
      unlock header; unlock partition(old)
  # 新タグの登録
  lock p
  if map.contains(tag):        # 別スレッドが先に読み込み始めた
      unlock p; return victim to free list (unpin); goto top
  map.insert(tag, fid)
  lock header: tag = Some(tag); io_in_progress = true; usage_count = 1; unlock header
  unlock p

  # ロックの外で I/O
  r = smgr.read_block(tag, content.write())   # まだ誰もラッチを取れない（valid = false）
  verify_page(r)                               # チェックサムとヘッダの検査（§5.9）
  lock header: io_in_progress = false; valid = ok; io_error = !ok; notify_all; unlock
  if error: map から外すかは §5.10。PinnedBuffer を drop してエラーを返す
```

- パーティションのロックは一度に 1 つだけ持つ。PG 15 以前の `BufferAlloc` は新旧 2 つのパーティションを番号順に同時に取っていたが、PG 16 以降は旧タグの削除と新タグの登録を分けており（`InvalidateVictimBuffer`）、yuzhu もこちらに倣う。同時に 1 つしか持たないので、パーティション間のデッドロックが原理的に起きない。
- 読み込み中のフレームのコンテンツには、他のスレッドは `wait_io` を抜けるまでラッチを取りに来ない。読み込み側はコンテンツの書き込みラッチを短時間取ってから I/O するので、規則上の問題もない。

### 5.6 clock-sweep（get_victim）

```
get_victim():
  if let Some(fid) = free_list.pop(): pin it; return fid
  trycounter = nframes
  loop:
    fid = clock_hand.fetch_add(1) % nframes
    lock header(fid)
    if pin_count == 0:
        if usage_count > 0: usage_count -= 1; trycounter = nframes
        else:
            pin_count = 1   # 自分でピンして確保
            dirty = header.dirty; unlock header
            if dirty: flush_frame(fid)?   # 失敗したら unpin してエラーを返す
            return fid
    else if trycounter -= 1 == 0:
        return Err(Error::internal("no unpinned buffers available"))  # PG と同じ文言、SQLSTATE XX000
    unlock header
```

- `flush_frame` で victim のラッチを `try_read` で取れなかったら、unpin して次を探す（§5.2）。
- `trycounter` の扱いは freelist.c の `StrategyGetBuffer` と同じ（usage_count を減らしたら戻す）。プールが全部ピンされていれば、最大 6 周程度でエラーになり、無限ループしない。
- **プールの最小サイズ**: 1 セッションが同時に持つピンの最大数（heap の UPDATE で旧ページ・新ページ、将来の B+Tree の降下で高さ分）× 最大接続数より十分大きくする。PostgreSQL は `shared_buffers` に「16 以上、かつ max_connections の 2 倍以上」の下限を設けている（未検証: `guc_tables.c` で確認していない）。yuzhu は起動時に `nframes >= 16 * max_connections` を検査してエラーにする、を推奨する。
- 大きなテーブルの全件走査がプールを汚す問題（PostgreSQL は `BufferAccessStrategy` のリングバッファで回避）は、M2 では対処しない。`read_buffer` に `strategy: Option<&mut Ring>` を足す余地を残しておく（M4 以降）。

### 5.7 デッドロックと誤用の検出（デバッグビルド）

スレッドローカルの状態で、Rust の型では防げない誤用を panic にして見えるようにする。`thread_local!` と `Cell` / `RefCell` はスレッド内に閉じているので、`Send + Sync` の原則（`RefCell` を共有しない）に反しない。

```rust
thread_local! {
    static HELD: RefCell<HeldState> = RefCell::new(HeldState::default());
}
#[derive(Default)]
struct HeldState {
    pins: HashMap<(usize /*pool id*/, FrameId), u32>,   // このスレッドが持つピン
    latches: Vec<(FrameId, LatchMode)>,                  // 取得順
}
```

- **同じフレームを二重にラッチしようとしたら panic**（`with_page` の中で同じページの `with_page_mut` を呼ぶ、など）。`RwLock` の再入は黙ってハングするので、これがないと原因の特定に時間がかかる。
- **ラッチを持ったまま `read_buffer` を呼んだら、許可リストにない場合は警告**（追い出しの I/O が長時間ラッチを延ばすため）。heap の UPDATE（旧ページの排他ラッチを持ったまま新ページを取る）と B+Tree の降下は明示的に許可する API を通す。
- **複数ページのラッチ順序**: 同じリレーション内ではブロック番号の小さい順に取る（PostgreSQL の `RelationGetBufferForTuple` は、UPDATE で旧ページと新ページの両方をロックするとき、ブロック番号の小さい方から取る。`src/backend/access/heap/hio.c`）。B+Tree は親 → 子、左 → 右。`HeldState.latches` で順序違反を検出する。
- 本番ビルドではこれらの検査を `cfg(debug_assertions)` で外す。ただしピン数の計数（次節）は本番でも残す（安価なので）。

### 5.8 ピン漏れの検出

- **手動の `unpin()` は公開しない**。ピンは `PinnedBuffer` の Drop でしか外れない。
- `PinnedBuffer` を `!Send` にしたので、ピンは作ったスレッドで必ず外れる。スレッドローカルのピン数（`HELD.pins`）が正しく保てる。M1 の `Executor` トレイトは `Send` を要求していないので、Executor にピンを持たせても問題ない（`impl/rust/crates/yuzhu-core/src/executor/mod.rs` で確認）。
- **文の終わりで検査する**: `session` が文を実行し終えたら（成功・失敗とも）`buffer::assert_no_pins()` を呼ぶ。
  - テストビルド（`cfg(test)` または `debug_assertions`）: 残っていれば、タグの一覧つきで panic。
  - 本番ビルド: PostgreSQL と同じく `buffer refcount leak` の WARNING をログに出す（Executor ツリーの Drop で必ず外れるはずなので、実際には起きない想定）。
- **エラー時の解放**: Rust の `?` によるエラー伝播でも Drop が走るので、PostgreSQL のリソースオーナーに相当する仕組みは不要。panic の場合も、巻き戻し（unwind）で Drop が走る。セッションのスレッドで `catch_unwind` する場合、ラッチの RwLock が poison される点に注意する（次項）。
- **poison の扱い**: ページの書き込みラッチを持ったまま panic すると、ページが書きかけのまま残っている可能性がある。PostgreSQL ならクリティカルセクション中のエラーは PANIC になる。yuzhu では「poison されたコンテンツラッチを検出したら、サーバ全体を停止する」を推奨する（M2 は WAL がないので、書きかけのページを書き戻されると壊れる）。読み込みラッチの poison は無視してよい。
- **全体の検査**: テストのたびに、最後に `pool.pinned_frames() == 0` を assert する。sqllogictest ランナーで M2 のスイートを流すときも、サーバのデバッグ用フラグで各文の後に検査する。

### 5.9 ページの検査とチェックサム

- ページヘッダは PostgreSQL の `PageHeaderData`（`bufpage.h`）と同じ配置にする: `pd_lsn`（8）、`pd_checksum`（2）、`pd_flags`（2）、`pd_lower`（2）、`pd_upper`（2）、`pd_special`（2）、`pd_pagesize_version`（2、値は `8192 | 4`。`PG_PAGE_LAYOUT_VERSION = 4`）、`pd_prune_xid`（4）の 24 バイト。バイト順はリトルエンディアンに固定する（PostgreSQL はネイティブ順だが、yuzhu の対象である x86_64 / aarch64 Linux ではどちらも同じ）。heap ページの中身は別の調査で決める。
- **チェックサムは M2 から常に計算する**。PostgreSQL のアルゴリズム（FNV-1a を 32 本並列に回し、ブロック番号を混ぜて 16 ビットに畳む。`src/include/storage/checksum_impl.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/storage/checksum_impl.h>）をそのまま手で実装する。100 行に満たない。アルゴリズムの詳細はこの調査では読んでいない（未検証）。
  - 書き込み時: 共有ラッチを持った状態でページを**スレッドローカルの 8KB の作業領域にコピー**し、コピーにチェックサムを入れて書く（PostgreSQL の `PageSetChecksumCopy` と同じ。共有ラッチでは元のページを書き換えられない）。
  - 読み込み時: PostgreSQL の `PageIsVerifiedExtended` と同じく、**全バイト 0 のページは正しいものとして受け入れる**（拡張途中でクラッシュした場合など）。ヘッダの整合性（`pd_lower <= pd_upper <= pd_special <= 8192`、版数）とチェックサムを検査し、失敗したら `XX001 data_corrupted`、メッセージ `invalid page in block %u of relation %s`（bufmgr.c 1556 行付近と同じ文言）。
  - 意義: WAL も full page write もない M2 で torn write が起きたとき、黙って壊れたデータを返さずに検出できる。M3 の full page write の単体テスト（「FPW を無効にすると torn page がチェックサムで検出される」）の土台にもなる。
- チェックサムがあると、M3 でヒントビットだけを変更したページを書くときに PostgreSQL は full page image が要る（`wal_log_hints` / `XLogSaveBufferForHint`）。この扱いは §6.3。

### 5.10 I/O エラーのときのフレームの状態

- 読み込みエラー: PostgreSQL はバッファを `BM_IO_ERROR` のまま残し、次に来た者が読み直す。yuzhu はより単純に、**マッピング表からタグを外し、フレームを free list に戻す**。待っていた他スレッドは `wait_io` から抜けたあと、タグが外れていることに気づいて最初からやり直す（自分で読み直す）。
- 書き込みエラー（追い出し時・チェックポイント時）: ページは dirty のまま残し、エラーを返す。追い出しなら、その文がエラーになる。fsync ではない `write` の EIO は PANIC にしない（PostgreSQL も write の失敗は ERROR）。チェックポイントは失敗として終わり、次回に再試行する。

### 5.11 DROP / TRUNCATE

1. テーブルロック（M2 は DDL のグローバル Mutex）で他のセッションのアクセスを止める。
2. `drop_relation_buffers(rel, fork, from_block)`: 全フレームを走査し、該当タグのものをマッピング表から外して free list に戻す。dirty でも書かない。ピンが残っていたら internal error（ロックの取り忘れのバグ）。
3. `smgr.truncate` / `smgr.unlink`。

順序を逆にすると、切り詰めたあとで dirty ページが書き戻され、ファイルが元の長さに戻ってしまう。PostgreSQL も `smgrtruncate` の中でバッファを先に捨てている（未検証: PG 17 の `smgrtruncate` が `DropRelationBuffers` を呼ぶ位置）。

---

## 6. 書き戻しの方針（M2 と M3）

### 6.1 M2: steal / no-force、チェックポイントで確定

| 契機 | 動作 |
|---|---|
| 追い出し | victim が dirty なら同期的に書く（fsync はしない。`pending_sync` に登録） |
| チェックポイント（定期、`CHECKPOINT` 文、正常終了時） | §6.2 の手順で全 dirty ページを書き、fsync し、制御ファイルを更新 |
| コミット | **何もしない**（ページを書かない） |
| バックグラウンドライタ | M2 では作らない |

- **M2 はクラッシュ安全ではない**。直近のチェックポイント以降のコミットは失われ、追い出しで書かれた途中状態のページ（未コミットの行を含むものもある）が残り、torn page も起こりうる。M2 の要件は「再起動しても（正常終了なら）データが残る」と解釈し、クラッシュ時の整合性は M3 で保証すると仕様に明記する。torn page はチェックサムで検出されるので、少なくとも黙って壊れたデータを返すことはない。
- 代替案として「コミットごとに全 dirty ページを書いて fsync する（force）」も検討したが、採用しない。(a) 書いている途中でクラッシュすれば結局 torn page で壊れる、(b) M3 で捨てるコードになる、(c) 遅い。テストで永続化を確かめたいときは、PostgreSQL にもある `CHECKPOINT` 文を発行すればよい（本物の PostgreSQL でも同じテストが動く）。
- **steal（未コミットの変更を含むページの追い出し）を最初から許す**。M3 で PostgreSQL と同じ「xmin/xmax と clog で未コミットのタプルを見えなくする、UNDO なしの REDO-only」を採用するなら、no-steal の制約は不要になる（`research-rust-db-arch.md` §4.2 の結論と同じ）。M2 でもピンやフラグで追い出しを禁止する仕組みを作らない。
- M2 のアボートの扱い（M1 の undo ログを続けるか、xmin + clog に移るか）は heap / txn 側の設計の問題で、バッファプールはどちらでも変わらない。

### 6.2 チェックポイントの手順（M2）

```
checkpoint():
  lock checkpoint_lock
  1. 全フレームを走査し、valid && dirty のものに checkpoint_needed = true を立て、タグを集める（BufferSync の前半）
  2. タグ順にソートする
  3. 各フレーム: checkpoint_needed が立っていれば（途中で追い出しにより書かれていれば落ちている）
       ピン → 共有ラッチ（ここは待ってよい） → header: dirty = false, checkpoint_needed = false
       → コピー + チェックサム → smgr.write_block → ラッチ解放 → unpin
  4. smgr.sync_pending()                       # 失敗したら PANIC 扱い（§4.2）
  5. 制御ファイル（global/pg_control 相当）を更新: 一時ファイルに書いて sync → rename → sync_dir
       中身: チェックポイントの時刻、次の OID、次の XID、（M3）REDO 開始 LSN、CRC
```

- 手順 3 で dirty を落とすのはラッチを取った後。共有ラッチを持っている間は誰もページを変更できない（変更には排他ラッチが要る）ので、PostgreSQL の `BM_JUST_DIRTIED` に相当する仕組みは要らない。M3 でヒントビットを共有ラッチで書き換えることにした場合は必要になる（§6.3 では採らない）。
- 「開始時点で dirty なページだけ」を書くことで、書き込みが絶えないワークロードでもチェックポイントが終わる。
- PostgreSQL は `pg_control` を rename せずに上書きする（512 バイト以下なのでセクタ単位の書き込みが原子的という前提、CRC 付き）。yuzhu は rename 方式の方が SimVfs でのテストがしやすいので、そちらを推奨する（`pg_control` の扱いの詳細は未検証）。
- 定期実行: 専用のチェックポインタスレッドを 1 本作り、`checkpoint_timeout`（既定 5 分、PostgreSQL と同じ）ごとに実行する。M2 では WAL 量による契機（`max_wal_size`）はない。
- 正常終了: 新規接続の受付を止め、全セッションの終了を待ち、チェックポイントを実行してから終了する。

### 6.3 M3 で足すもの（M2 で用意しておく場所）

```rust
pub type Lsn = u64;   // PostgreSQL の XLogRecPtr と同じ 64 ビット

/// バッファプールから見た WAL。M2 は NoWal。
pub trait WalFlush: Send + Sync + std::fmt::Debug {
    /// lsn までの WAL が永続化されるまで待つ（XLogFlush）。
    fn flush_to(&self, lsn: Lsn) -> Result<()>;
    /// 直近のチェックポイントの REDO 開始位置（full page write の要否判定用）。
    fn redo_ptr(&self) -> Lsn;
}
#[derive(Debug)]
pub struct NoWal;
impl WalFlush for NoWal {
    fn flush_to(&self, _: Lsn) -> Result<()> { Ok(()) }
    fn redo_ptr(&self) -> Lsn { 0 }
}
```

| M3 で入る規則 | M2 で用意しておくこと |
|---|---|
| **WAL-before-data**: dirty ページを書く前に `wal.flush_to(page.lsn())`（PostgreSQL の `FlushBuffer` 内の `XLogFlush`） | `flush_frame` を 1 か所にまとめ、そこで `wal.flush_to` を呼ぶ（M2 は no-op）。ページヘッダの `pd_lsn` |
| **full page write**: チェックポイント後の最初の変更で、ページ全体を WAL に載せる（`page.lsn() <= redo_ptr` なら FPI） | `PageWriteGuard` から現在の LSN が読める。`WalFlush::redo_ptr` |
| **REDO**: `page.lsn() >= record.lsn` ならスキップ。FPI はページを丸ごと置き換える | `read_buffer_zeroed`（読まずにゼロページを用意） |
| **チェックポイント**: REDO 開始 LSN を記録 → BufferSync → fsync → チェックポイントレコード → 制御ファイル | §6.2 の手順をそのまま拡張 |
| **relfilenumber 再利用の防止** | §4.3 |
| **ヒントビット** | 下記 |

ヒントビット（`t_infomask` の `HEAP_XMIN_COMMITTED` など）について: PostgreSQL は**共有ラッチのままヒントビットを書き換える**（`MarkBufferDirtyHint`）。safe Rust の `RwLock<PageBuf>` では、読み込みガードからページを書き換えられない。選択肢は次の 3 つ。

1. **ヒントビットを書くときだけ `try_write` を試み、取れたら書く**（取れなければ書かずに次の機会に回す。ヒントビットは最適化なので書かなくても正しい）。推奨。共有ラッチを手放してから `try_write` するので、その間にページが変わりうる点に注意する（スロットを読み直す）。
2. ページを `[AtomicU8; 8192]` 相当で持つ。safe で書けるが、すべてのページアクセスが atomic になって遅く、コードも読みにくい。不採用。
3. ヒントビットを使わず、clog を毎回引く（clog のキャッシュを速くする）。M3 の最初はこれでもよい。

チェックサム付きで「ヒントビットだけの変更」をどう書き戻すか（PostgreSQL は `wal_log_hints` 相当で FPI を出す）は、M3 の WAL 設計で決める。暫定案は「ヒントビットだけの変更ではページを dirty にしない（他の理由で書かれるときに一緒に永続化されればよい）」。これなら FPI なしでも torn page の危険が増えない（未検証: M3 設計時に再確認すること）。

---

## 7. エラーと SQLSTATE

| 状況 | SQLSTATE | Severity | メッセージ（PostgreSQL に合わせる） |
|---|---|---|---|
| read / write の EIO | `58030` io_error | Error | `could not read blocks %u..%u in file "%s": %m` / `could not write block %u in file "%s": %m` |
| ファイルがない | `58P01` undefined_file | Error | `could not open file "%s": No such file or directory` |
| 末尾を超えた読み込み | `58030` | Error | `could not read blocks %u..%u in file "%s": read only %zu of %zu bytes` |
| ENOSPC | `53100` disk_full | Error | `could not extend file "%s": No space left on device`、ヒント `Check free disk space.` |
| fsync 失敗 | `58030` | **Panic** | `could not fsync file "%s": %m` |
| チェックサム・ヘッダ不正 | `XX001` data_corrupted | Error | `invalid page in block %u of relation %s` |
| 空きフレームがない | `XX000` internal_error | Error | `no unpinned buffers available` |
| ラッチの poison | `XX000` | Panic | （yuzhu 独自）`buffer content lock poisoned` |

`error.rs` の `sqlstate` に `IO_ERROR`、`UNDEFINED_FILE`、`DISK_FULL`、`DATA_CORRUPTED` を追加する。SQLSTATE の値は PostgreSQL の付録 A（<https://www.postgresql.org/docs/17/errcodes-appendix.html>）による（値は記憶による。未検証）。

---

## 8. テスト計画

| 種類 | 内容 |
|---|---|
| smgr の単体 | セグメント境界（ブロック 131071 と 131072）の読み書き、extend / truncate / unlink 後の nblocks、セグメント 1GB の検査（テスト用に `RELSEG_SIZE` を小さくできる設定を用意する。PostgreSQL も `--with-segsize-blocks` でビルド時に変えられる） |
| バッファプールの単体 | 小さなプール（4 フレーム）での追い出し、全ピン時の `no unpinned buffers available`、usage_count による選択、同じタグの同時読み込みで I/O が 1 回だけ起きること（`SimStats` で数える） |
| 並行性 | N スレッドがランダムなページを読み書きし、各ページのカウンタの最終値が一致すること。デバッグビルドの二重ラッチ・順序違反検出を有効にして回す。`loom` は外部クレート（テスト用途なら可）だが、std の同期プリミティブを差し替える必要があるので M2 では見送る |
| ピン漏れ | すべてのテストの最後に `pinned_frames() == 0`。エラーで途中終了した文の後にもピンが残らないこと |
| 障害注入 | write の EIO で文がエラーになりサーバは生きていること、fsync 失敗でサーバが停止し再試行しないこと、ENOSPC の extend でファイル長が戻ること、ビット反転で `XX001` |
| クラッシュ | `CHECKPOINT` → 追加の変更 → `crash(DropUnsynced)` → 再起動で、チェックポイント時点のデータがそのまま読めること。`TornSectors` では、読み込みでチェックサムエラーが検出されるか、正しいデータが読めるかのどちらかで、壊れたデータを黙って返さないこと（M2 で保証するのはここまで。M3 で「コミット済みは必ず残る」に強化） |
| 再現性 | 失敗したケースのシードをログに出し、`YUZHU_SIM_SEED=...` で再実行できるようにする |

---

## 9. 決めておくべき点（`QUESTIONS.md` の候補）

1. **M2 はクラッシュ安全でない**ことを要件として許容してよいか（推奨: 許容。M3 で WAL により解消）。
2. **チェックサムを常に有効**にするか（推奨: 有効。PostgreSQL の既定は initdb 時に無効だが、PG 18 から既定で有効になる予定と記憶している。未検証）。
3. 開いているファイル数の上限管理（VFD 相当）を M2 で作るか（推奨: 作らない。テーブル数の上限を想定しない設計だけ守る）。
4. バッファプールの既定サイズ（推奨: 128MB = 16384 フレーム。PostgreSQL の `shared_buffers` の既定と同じ）。
5. `pg_control` 相当の更新を rename 方式にするか（推奨: rename 方式）。

---

## 10. 参考

- PostgreSQL バッファマネージャ README: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/buffer/README>
- WAL のコーディング規則（`src/backend/access/transam/README`）: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/README>
- PostgreSQL ドキュメント「データベースのファイルレイアウト」: <https://www.postgresql.org/docs/17/storage-file-layout.html>
- PostgreSQL ドキュメント「データベースページのレイアウト」: <https://www.postgresql.org/docs/17/storage-page-layout.html>
- PostgreSQL ドキュメント「信頼性と WAL」（torn page と full page write）: <https://www.postgresql.org/docs/17/wal-reliability.html>
- fsyncgate の経緯（PostgreSQL Wiki）: <https://wiki.postgresql.org/wiki/Fsync_Errors>
- CMU BusTub のバッファプール（C++ の参考実装）: <https://github.com/cmu-db/bustub>
