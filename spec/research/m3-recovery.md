# M3 調査: チェックポイントとクラッシュリカバリ

調査日: 2026-10-04。対象は PostgreSQL REL_17_STABLE。

- 出典の表記: 【確認】は REL_17_STABLE のソース（`raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/...` から取得）を grep して確かめた点。【未確認】は記憶や一般知識に基づく点で、実装前に確かめる必要がある。【推論】は PostgreSQL の設計から筆者が導いた理屈で、ソースに明記されているわけではない。
- 行番号は版が変わるとずれるので、関数名で示す。
- 関連する M3 調査（WAL レコード形式、MVCC 可視性、バッファプール）とは重なる部分がある。この文書では「チェックポイント」「pg_control」「REDO ループ」「torn page」「clog の永続化」「クラッシュ時の xid の扱い」「クラッシュテスト」を扱う。

主なソース:

| ファイル | 内容 |
|---|---|
| [src/backend/postmaster/checkpointer.c](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/postmaster/checkpointer.c) | checkpointer プロセス。起動条件（時間・WAL 量・要求） |
| [src/backend/access/transam/xlog.c](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/xlog.c) | `CreateCheckPoint`、`CheckPointGuts`、`StartupXLOG`、`PerformRecoveryXLogAction`、pg_control の読み書き |
| [src/backend/access/transam/xlogrecovery.c](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/xlogrecovery.c) | `InitWalRecovery`、`PerformWalRecovery`（REDO ループ）、`ApplyWalRecord`、`ReadRecord` |
| [src/backend/access/transam/xloginsert.c](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/xloginsert.c) | `XLogRecordAssemble`（full page image を付けるかの判定）、`XLogSaveBufferForHint` |
| [src/backend/access/transam/xlogutils.c](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/xlogutils.c) | `XLogReadBufferForRedo(Extended)`、invalid page の追跡 |
| [src/include/catalog/pg_control.h](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/catalog/pg_control.h) | `ControlFileData`、`CheckPoint`、`DBState` |
| [src/common/controldata_utils.c](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/common/controldata_utils.c) | `update_controlfile` |
| [src/backend/access/transam/clog.c](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/clog.c) | コミットログ（pg_xact）、`CLOG_ZEROPAGE` / `CLOG_TRUNCATE`、`TrimCLOG` |
| [src/backend/access/transam/transam.c](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/transam.c) | `TransactionIdDidCommit` / `TransactionIdDidAbort` |
| [src/backend/access/transam/xact.c](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/xact.c) | `RecordTransactionCommit`、`xact_redo_commit` |

主なドキュメント:

- WAL の信頼性: <https://www.postgresql.org/docs/17/wal-reliability.html>
- WAL の設定（チェックポイント）: <https://www.postgresql.org/docs/17/wal-configuration.html>
- WAL の内部: <https://www.postgresql.org/docs/17/wal-internals.html>
- `full_page_writes` などの GUC: <https://www.postgresql.org/docs/17/runtime-config-wal.html>
- `data_sync_retry`: <https://www.postgresql.org/docs/17/runtime-config-error-handling.html>
- `pg_controldata`: <https://www.postgresql.org/docs/17/app-pgcontroldata.html>

---

## 1. 要点（先に結論）

1. **チェックポイントは PostgreSQL と同じ「ファジー」方式にする。** 先に redo ポイントを決め、その時点で dirty なページと clog をすべて書き出して fsync し、最後にチェックポイントレコードと制御ファイルを書く。書き出しの間も更新は止めない。M3 は単一ライターだが、M5 の複数ライターでもアルゴリズムは変わらない。
2. **制御ファイル `global/yuzhu_control` は 2 スロット方式にする。** 1 スロット 512 バイト以内、CRC32C と世代番号を付け、古い方のスロットに書いて fdatasync する。PostgreSQL は「512 バイトの書き込みはアトミック」という前提で上書きするが、yuzhu は障害注入テストで torn write を起こすので、その前提に頼らない方式にする。
3. **full page write（FPW）は PostgreSQL と同じ規則にする。** redo ポイント以降にページを初めて変更するとき（`page_lsn <= redo_lsn`）は、WAL レコードにページ全体のイメージを入れる。REDO でイメージを持つレコードに出会ったら、ディスク上のページが壊れていても丸ごと上書きする。この判定は WAL 挿入ロックの中で行う。
4. **リカバリは「制御ファイル → チェックポイントレコード → redo_lsn から WAL 末尾まで REDO → 終了チェックポイント → 接続受付」の順で行う。** WAL の末尾は「最初に検証に失敗したレコード」とする。末尾より後ろはゼロで埋め、後続のセグメントは消す。
5. **クラッシュ時に実行中だった xid には、何も書かない。** PostgreSQL と同じく「clog では IN_PROGRESS のままだが、実行中のトランザクション一覧にない xid は abort 扱い」という規則にする。ただし `next_xid` は、REDO で見たすべての xid より大きくする（xid を再利用しない）。
6. **clog（`pg_xact/`）は 2 ビット/xid のページファイルにし、チェックポイントで fsync する。** コミットの永続性は、あくまで WAL のコミットレコードが担う。clog ページの torn write は、ビットが一方向（0 → 1 または 2）にしか変わらないので無害になる【推論】。
7. **クラッシュテストは 2 層にする。** (a) プロセス内のシミュレーション: 障害注入 FS が「fsync されていない書き込みを捨てる・一部だけ残す・ページを破る」を決定的な乱数で行う。(b) 実プロセスの kill -9 ハーネス。どちらも「確認応答したコミットは残る」「部分的なトランザクションは見えない」「リカバリは冪等」などの不変条件を検査する。さらに FPW や fsync をわざと無効にしたときに、ハーネスが壊れを**検出できること**を確かめる（ハーネス自体のテスト）。

---

## 2. PostgreSQL のチェックポイント

### 2.1 checkpointer の起動条件

checkpointer は専用のプロセスで、次のいずれかで `CreateCheckPoint` を呼ぶ【未確認: 細部】。

- 前回から `checkpoint_timeout`（既定 5 分）が経った
- 前回の redo ポイント以降の WAL が `max_wal_size`（既定 1GB）に基づく閾値を超えた（`XLogCheckpointNeeded` → `RequestCheckpoint`）
- `CHECKPOINT` コマンド、シャットダウン、リカバリ終了、`CREATE DATABASE` などの要求

通常のチェックポイントは、`checkpoint_completion_target`（既定 0.9）に従ってページの書き出しを時間的に分散する。`CHECKPOINT_IMMEDIATE` の要求では分散しない。詳細: <https://www.postgresql.org/docs/17/wal-configuration.html>

### 2.2 CreateCheckPoint の手順

`xlog.c` の `CreateCheckPoint` は、概ね次の順で動く【確認: 関数の構成と `XLOG_CHECKPOINT_REDO`、`CheckPointGuts` の中身】【未確認: 細かいロック順】。

1. **redo ポイントを決める。**
   - シャットダウンのチェックポイントでは、他に WAL を書く者がいないので、現在の挿入位置をそのまま redo ポイントにする。
   - オンラインのチェックポイントでは、PostgreSQL 17 から **`XLOG_CHECKPOINT_REDO` レコードを挿入し、その開始位置を redo ポイントにする**（`XLogInsert(RM_XLOG_ID, XLOG_CHECKPOINT_REDO)`）【確認】。挿入と同時に共有メモリの `RedoRecPtr` を更新するので、これ以降に WAL を書くバックエンドは、新しい redo ポイントに基づいて FPW を判定する。（17 より前は、全 WAL 挿入ロックを取って挿入位置を読み、`RedoRecPtr` を更新していた。REDO レコードは WAL summarizer 向けに導入された。）
2. **その時点の nextXid、nextOid、oldestXid などを読み、`CheckPoint` 構造体に詰める。** redo ポイントの後に読むので、redo ポイント以前の WAL に現れる xid はすべて nextXid より小さい。
3. **`CheckPointGuts(redo, flags)` で、redo ポイント以前の変更をすべてディスクに書いて fsync する。** 中身は次の順【確認】:
   `CheckPointRelationMap` → `CheckPointReplicationSlots` → `CheckPointSnapBuild` → `CheckPointLogicalRewriteHeap` → `CheckPointReplicationOrigin` → **`CheckPointCLOG`** → `CheckPointCommitTs` → `CheckPointSUBTRANS` → `CheckPointMultiXact` → `CheckPointPredicate` → **`CheckPointBuffers`** → **`ProcessSyncRequests`** → `CheckPointTwoPhase`
   - `CheckPointBuffers`（`BufferSync`）は、開始時点で dirty なバッファに `BM_CHECKPOINT_NEEDED` を付け、それだけを書き出す。途中で新たに dirty になったバッファは対象外になる。書き出すときは、ページの LSN まで WAL を flush する（WAL の原則）。
   - `ProcessSyncRequests` は、書き出したファイル（バックエンドが追い出したページを含む）をまとめて fsync する。PostgreSQL は書き込みと fsync を分け、fsync を checkpointer に集めている。
4. **チェックポイントレコード（`XLOG_CHECKPOINT_ONLINE` または `XLOG_CHECKPOINT_SHUTDOWN`）を挿入し、flush する。** 中身は `CheckPoint` 構造体（`redo`、`ThisTimeLineID`、`fullPageWrites`、`nextXid`、`nextOid`、`nextMulti`、`oldestXid`、`oldestActiveXid` など）。
5. **pg_control を更新する。** `checkPoint`（チェックポイントレコードの位置）、`checkPointCopy`（構造体のコピー）、`state`（オンラインなら `DB_IN_PRODUCTION`、シャットダウンなら `DB_SHUTDOWNED`）を書き、fsync する。**この書き込みがチェックポイントの完了点**になる。ここより前に落ちれば、前回のチェックポイントからリカバリする。
6. **不要になった WAL セグメントを消すか再利用する。** 新しい redo ポイントより前のセグメントは要らない（PostgreSQL 11 以降は、2 つ前ではなく直前のチェックポイントだけを残す）【未確認: 11 での変更の詳細】。`pg_subtrans` も切り詰める。

### 2.3 なぜこれで正しいか

- redo ポイント R より前の WAL レコードが表す変更は、手順 3 ですべてディスクに永続化されている。だから R より前の WAL は REDO に要らない。
- R より後の変更は、手順 3 で書かれたかもしれないし、書かれていないかもしれない。REDO はページ LSN と比べて、適用済みのレコードを飛ばす（4.3 節）。だから二重適用にならない。
- 手順 3 の最中にページを書くと、そのページが torn になる可能性がある。しかし、そのページが R 以降に初めて変更されたときの WAL レコードには、FPW によってページ全体のイメージが入っている（第 5 節）。REDO はそのイメージで上書きするので、torn page は直る。
- 手順 5 より前に落ちた場合: pg_control は古いチェックポイントを指したままなので、そこから REDO する。古い redo ポイント以降の WAL はまだ消していない（手順 6 は手順 5 の後）。

### 2.4 シャットダウンとチェックポイント

- 正常なシャットダウンでは `XLOG_CHECKPOINT_SHUTDOWN` を書き、`state = DB_SHUTDOWNED` にする。起動時に `state` がこれで、かつチェックポイントレコードの後ろに WAL がなければ、REDO は要らない【未確認: 「後ろに WAL がない」の判定方法】。
- 起動時に `state` が `DB_IN_PRODUCTION` などなら、前回はクラッシュしたとみなしてリカバリする。

---

## 3. pg_control（制御ファイル）

### 3.1 PostgreSQL の構造

`pg_control.h` の `ControlFileData` の主なフィールド【確認: `state`、`checkPoint`、`checkPointCopy`、`crc` とサイズの定数】【未確認: 列挙は主要なものだけ】:

| フィールド | 意味 |
|---|---|
| `system_identifier` | initdb ごとの一意な ID（WAL ファイルとの対応確認に使う） |
| `pg_control_version`、`catalog_version_no` | 形式とカタログの版 |
| `state` | `DBState`: `DB_STARTUP`、`DB_SHUTDOWNED`、`DB_SHUTDOWNED_IN_RECOVERY`、`DB_SHUTDOWNING`、`DB_IN_CRASH_RECOVERY`、`DB_IN_ARCHIVE_RECOVERY`、`DB_IN_PRODUCTION` |
| `time` | 最終更新時刻 |
| `checkPoint` | 最後のチェックポイントレコードの WAL 位置 |
| `checkPointCopy` | そのチェックポイントレコードの中身（`redo` を含む） |
| `minRecoveryPoint` など | アーカイブリカバリ・スタンバイ用 |
| `blcksz`、`relseg_size`、`xlog_blcksz`、`xlog_seg_size` など | ビルド時の定数。不一致なら起動を拒否する |
| `data_checksum_version` | データページのチェックサムの有無 |
| `crc` | CRC32C |

- 構造体は **`PG_CONTROL_MAX_SAFE_SIZE`（512 バイト）以下**であることを静的アサートで保証している。ファイル自体は **`PG_CONTROL_FILE_SIZE`（8192 バイト）**にゼロで水増しして書く【確認】。
- 512 バイト以下にするのは、「1 セクタ（512 バイト）の書き込みはアトミック」という前提で、上書き更新しても torn にならないようにするため。CRC は破損の検出用で、壊れたら修復はできない（`pg_resetwal` で手で直す）【未確認: ヘッダのコメントの言い回し】。
- `update_controlfile`（`controldata_utils.c`）はファイルを開いて先頭から書き、`pg_fsync` する。一時ファイルとリネームは使わない【未確認: 細部】。

### 3.2 yuzhu の制御ファイル（推奨）

**ファイル**: `$PGDATA/global/yuzhu_control`。大きさは 8192 バイト固定。**スロット A をオフセット 0、スロット B をオフセット 4096 に置く。**

**スロットの形式**（リトルエンディアン、512 バイト以内。残りはゼロ）:

| オフセット | 型 | フィールド | 説明 |
|---|---|---|---|
| 0 | `[u8; 8]` | `magic` | `b"YUZHUCTL"` |
| 8 | u32 | `format_version` | 制御ファイル形式の版（M3 は 1） |
| 12 | u32 | `catalog_version` | カタログ形式の版 |
| 16 | u64 | `generation` | 書くたびに 1 増やす |
| 24 | u64 | `system_identifier` | initdb 時の乱数 |
| 32 | u32 | `state` | 0=Startup、1=ShutDown、2=ShuttingDown、3=InCrashRecovery、4=InProduction |
| 36 | u32 | `page_size` | 8192 |
| 40 | u32 | `wal_segment_size` | 例: 16 MiB |
| 44 | u32 | `flags` | bit0=full_page_writes、bit1=page_checksums |
| 48 | i64 | `time` | 更新時刻（UNIX 秒、表示用のみ） |
| 56 | u64 | `checkpoint_lsn` | 最後のチェックポイントレコードの LSN |
| 64 | u64 | `redo_lsn` | そのチェックポイントの redo ポイント |
| 72 | u64 | `next_xid` | 64 ビットの FullTransactionId（第 7 節） |
| 80 | u64 | `oldest_xid` | 参照されうる最古の xid（M5 の VACUUM と wraparound 用。M3 は 3 固定でよい） |
| 88 | u32 | `next_oid` | |
| 92 | u32 | `timeline` | M3 は 1 固定（将来のレプリケーション用に場所だけ確保） |
| 96 | u64 | `min_recovery_lsn` | M3 は 0（将来用） |
| ... | | 予約 | ゼロ |
| 508 | u32 | `crc32c` | オフセット 0〜507 の CRC32C |

**書き込み**: 2 つのスロットのうち `generation` が小さい方（または無効な方）に、`generation = 現在値 + 1` で書き、`fdatasync` する。成功して初めて「新しい制御ファイルになった」とみなす。

**読み込み**: 両スロットを読み、magic・CRC・版が正しいものの中で `generation` が大きい方を採用する。両方とも無効なら PANIC（`XX000`、または専用のエラー。起動しない）。

**この方式を選ぶ理由**:

- 512 バイトのアトミック性は、ディスクや FS の実装に依存する。yuzhu の障害注入 FS は、書き込みをセクタ未満や任意のバイト境界で破ることもある。2 スロットなら、どこで破れても「古い方」が必ず無傷で残る。
- リネーム方式（一時ファイル → fsync → rename → ディレクトリ fsync）でも安全だが、ディレクトリ操作を含むので障害モデルが複雑になる。2 スロットは「1 ファイル・2 か所の上書き」だけで済む。
- 工数はリネーム方式とほぼ同じ（0.5 日程度）。

**チェックポイントレコードの中身を制御ファイルにコピーしておく**（`redo_lsn`、`next_xid`、`next_oid`）のは PostgreSQL と同じ。WAL のチェックポイントレコードが読めないときの診断に役立つ。ただしリカバリの正本は WAL 側のレコードとし、両者が食い違えば PANIC にする。

**`pg_controldata` 相当**: `yuzhu-ctl controldata <dir>` のような小さな CLI を用意すると、クラッシュテストの失敗解析に便利。

---

## 4. PostgreSQL のリカバリ（StartupXLOG と REDO ループ）

### 4.1 全体の流れ

PostgreSQL 15 で REDO ループの大部分は `xlogrecovery.c` に移った【確認: ファイルの存在と `PerformWalRecovery`】。

1. **`StartupXLOG`**（xlog.c）が pg_control を読む（`ReadControlFile` で CRC と定数を検証）。
2. **`InitWalRecovery`**（xlogrecovery.c）が、`backup_label` や `recovery.signal` の有無、`state` から、リカバリが必要かを決める。`checkPoint` の位置からチェックポイントレコードを読む。**読めなければ PANIC** する（11 以降は 1 つ前のチェックポイントに戻らない）【未確認: 11 の変更】。
3. チェックポイントレコードの `nextXid`、`nextOid` などで共有状態を初期化し、`StartupCLOG` などで SLRU を準備する。
4. クラッシュリカバリなら、pg_control の `state` を `DB_IN_CRASH_RECOVERY` にして書く。
5. **`PerformWalRecovery`** が `redo` の位置から `ReadRecord` で 1 レコードずつ読み、`ApplyWalRecord` で適用する。
   - `ApplyWalRecord` はまず **`AdvanceNextFullTransactionIdPastXid(record->xl_xid)`** を呼び、レコードの xid が nextXid 以上なら nextXid を進める【確認】。
   - 次に、リソースマネージャ（rmgr）ごとの `rm_redo` を呼ぶ（heap、btree、xact、clog、smgr、xlog など）。
6. `ReadRecord` が無効なレコードを返したら、そこが WAL の末尾になる（4.2 節）。
7. **`PerformRecoveryXLogAction`**: スタンバイの昇格でなければ、**`RequestCheckpoint(CHECKPOINT_END_OF_RECOVERY | CHECKPOINT_IMMEDIATE | CHECKPOINT_WAIT)`** で終了チェックポイントを取り、完了を待つ【確認】。昇格のときは軽量な `CreateEndOfRecoveryRecord` だけで済ませ、チェックポイントは後で取る。
8. `TrimCLOG` などで SLRU の後始末をし、`state = DB_IN_PRODUCTION` にして接続を受け付ける。

### 4.2 WAL の末尾の判定

`XLogReaderValidatePageHeader` と `ValidXLogRecordHeader`、`ValidXLogRecord` で、次のどれかに当たれば「ここで WAL が終わり」とみなす【未確認: 関数名の正確さ】。

- ページヘッダの magic、`xlp_pageaddr`（そのページが WAL 上のどの位置か）が合わない。再利用したセグメントには古い WAL が残っているので、`xlp_pageaddr` で区別する。
- レコード長が 0 や不正な値、`xl_prev`（直前のレコードの位置）が合わない
- CRC32C が合わない（書き込みの途中で落ちた torn な WAL もここで弾かれる）

クラッシュリカバリでは「末尾」と「破損」を区別しない。途中の 1 レコードが壊れていれば、それ以降はすべて捨てる。fsync 済みの WAL が壊れないことは前提になっている。

### 4.3 ページを読むときの判定（XLogReadBufferForRedo）

`XLogReadBufferForRedoExtended` は、レコードが参照する各ブロックについて次を返す【確認: 戻り値の種類】。

| 戻り値 | 条件 | rmgr の動作 |
|---|---|---|
| `BLK_RESTORED` | レコードにそのブロックの full page image がある | イメージでページを上書き済み（ページ LSN = レコードの LSN）。何もしない |
| `BLK_DONE` | ディスク上のページの LSN ≥ レコードの LSN | 既に適用済みなので飛ばす |
| `BLK_NEEDS_REDO` | 上記以外 | レコードを適用し、ページ LSN をレコードの LSN にする |
| `BLK_NOTFOUND` | ファイルやブロックがない（後のレコードで truncate または drop された） | 飛ばす。invalid page として記録する |

- ブロックがファイルの末尾より先にあるときは、ゼロのページで延長する。
- `BLK_NOTFOUND` になったページは `log_invalid_page` で記録する。後で同じリレーションの drop や truncate のレコードを REDO したら、記録を消す。**リカバリの終わりに記録が残っていたら PANIC** する（WAL とファイルの食い違い、つまり本当の破損を意味する）【未確認: PANIC の条件の細部】。
- ページを初期化するレコード（`XLOG_HEAP_INIT_PAGE` フラグ付きの insert など）は、ディスクのページを読まずに初期化してから適用する。

---

## 5. torn page と full page write

### 5.1 問題

- PostgreSQL のページは 8KB だが、OS やディスクが保証するアトミックな書き込み単位は通常 512 バイトか 4KB。ページの書き込み中に電源が落ちると、前半が新しく後半が古いページが残る（torn page）。
- REDO-only の WAL のレコードは「このページのこのスロットにタプルを入れる」のような差分なので、土台のページが壊れていると正しく適用できない。ページ LSN も壊れているかもしれない。

### 5.2 PostgreSQL の解決策

- **redo ポイント以降にページを初めて変更するとき、その WAL レコードにページ全体のイメージ（FPI）を入れる。** 判定は `XLogRecordAssemble` の中で `needs_backup = (page_lsn <= RedoRecPtr)` として行う【確認】。
- この判定はロックの外で `GetFullPageWriteInfo` で得た `RedoRecPtr` を使う。そのため、`XLogInsertRecord` が挿入ロックの中で、**判定に使った最も古いページ LSN（`fpw_lsn`）が最新の `RedoRecPtr` 以下になっていないか**を確かめる。間に別のチェックポイントが redo ポイントを進めていたら、挿入せずに `InvalidXLogRecPtr` を返し、呼び出し側が組み立てからやり直す【確認: `fpw_lsn` の説明コメント】。
- REDO は、FPI を持つレコードではディスクのページを読まずにイメージで上書きする。そのページがどれだけ壊れていても直る。
- イメージはページの空き領域（`pd_lower`〜`pd_upper` の「穴」）を省いて保存する。`wal_compression` で圧縮もできる。
- **なぜ十分か**【推論、ドキュメントの説明と整合】: クラッシュ時に torn になりうるのは、最後の redo ポイント R 以降に書き出されたページだけ。R より前に書かれたページは、チェックポイントの fsync で完全に永続化されている。R 以降に書き出されたページは、R 以降に変更されたことがある（そうでなければ dirty にならない）。その最初の変更の WAL には FPI がある。そして REDO は R から始まるので、必ずその FPI を通る。
- `full_page_writes = off` は、torn write が起きないことが保証された FS（ZFS など）のための設定。ドキュメントは既定の on を強く推奨している（<https://www.postgresql.org/docs/17/runtime-config-wal.html#GUC-FULL-PAGE-WRITES>）。

### 5.3 ヒントビットとチェックサムの落とし穴

- PostgreSQL は、ヒントビット（タプルの xmin/xmax がコミット済みか abort 済みかのキャッシュ）を WAL なしで設定する。ページは dirty にするが、WAL レコードは出さない（`MarkBufferDirtyHint`）。
- **データチェックサムか `wal_log_hints` が有効なとき**は、ヒントビットだけの変更でも、redo ポイント後の初回なら `XLogSaveBufferForHint` で FPI を WAL に書く（`XLOG_FPI_FOR_HINT`）【確認: `XLogSaveBufferForHint` が `page_lsn <= RedoRecPtr` を判定している】。理由は次の通り: ヒントビットの変更だけで書き出したページが torn になると、チェックサムが合わなくなる。しかも WAL にそのページの記録がなければ、REDO で直せない。
- チェックサムは torn page を**検出**するだけで、直すのは FPW の役目。

### 5.4 yuzhu への推奨

- **FPW は M3 の最初から必須にする**（無効にするスイッチはテスト用にだけ用意する。第 9 節のハーネス検証に使う）。
- **判定は WAL 挿入ロックの中で行う。** yuzhu は M3 で WAL 挿入を 1 つの `Mutex` で直列化する想定なので、ロックの中で `redo_lsn` を読み、`page_lsn <= redo_lsn` ならイメージを付ける。PostgreSQL の「ロックの外で組み立てて、ロックの中で検証してやり直す」方式は、M5 以降に挿入を並列化するときに導入する。そのためにレコード組み立ての API は「(ページ参照, 差分データ) の並び」を受け取る形にしておき、イメージを付けるかどうかは WAL 層が決めるようにする。
- **ページを新しく作る操作（リレーションの延長、空ページへの最初の挿入）は、FPI か「ページ初期化」フラグ付きのレコードにする。** REDO はディスクのページを読まずに初期化する。
- **ページチェックサムは M3 で入れることを推奨する**（CRC32C、ページヘッダの 2 バイトまたは 4 バイト）。クラッシュテストで「FPW の届かない破損」を見つけるのに役立つ。入れる場合は、ヒントビットの扱いを PostgreSQL と同じにする（初回のヒントビット変更で FPI を出す）。ヒントビット自体を M3 で使うかは MVCC の調査で決める。使わないなら、この問題は起きない。
- FPI の「穴」の省略は M3 で入れてよい（簡単で WAL が大きく減る）。圧縮は後回し。

---

## 6. コミットログ（clog / pg_xact）の永続化

### 6.1 PostgreSQL の構造

- 1 トランザクションあたり 2 ビット。状態は `IN_PROGRESS = 0`、`COMMITTED = 1`、`ABORTED = 2`、`SUB_COMMITTED = 3`【未確認: 定数名】。8KB のページに 32768 個の xid が入る。
- SLRU（simple LRU）という小さな専用バッファで管理し、`pg_xact/` の下に 32 ページ（256KB）ずつのセグメントファイルとして置く【未確認: 32 ページ】。
- **SLRU のページにはページヘッダも LSN もなく、FPW の対象でもない。**

### 6.2 WAL レコード

| rmgr | レコード | 意味 |
|---|---|---|
| `RM_CLOG_ID` | `CLOG_ZEROPAGE` | 新しい clog ページをゼロで初期化した（xid の割り当てで新しいページにかかったとき、`ExtendCLOG` から）【確認: レコードの存在】 |
| `RM_CLOG_ID` | `CLOG_TRUNCATE` | 古い clog セグメントを消した（VACUUM による凍結の後） |
| `RM_XACT_ID` | `XLOG_XACT_COMMIT` | コミット。REDO（`xact_redo_commit`）が clog に COMMITTED を書く |
| `RM_XACT_ID` | `XLOG_XACT_ABORT` | abort。REDO が clog に ABORTED を書く |

**clog のビットを直接 WAL に記録するレコードはない。** clog はコミットと abort のレコードの REDO によって再構成される。

### 6.3 コミットの順序

`xact.c` の `RecordTransactionCommit` は次の順で動く【未確認: 細部。概略は確か】。

1. `XLOG_XACT_COMMIT` を挿入する。
2. `synchronous_commit` が on なら、`XLogFlush` でコミットレコードまで fsync する。
3. `TransactionIdCommitTree` で clog に COMMITTED を書く（共有バッファ上。ディスクにはまだ書かない）。
4. その後 `ProcArrayEndTransaction` で実行中の一覧から外す。ここで初めて他のセッションから見えるようになる。

- abort のレコードは flush しない。abort の情報が失われても、次の 6.5 節の規則で abort 扱いになるので害がない。
- 非同期コミットでは、clog ページを書き出す前にそのページに関係するコミットレコードの WAL を flush する必要がある。そのために clog は `group_lsn`（clog ページ内の xid のグループごとの最新コミット LSN）を持つ【確認: `group_lsn` の存在】。yuzhu は M3 で非同期コミットをしないので不要。

### 6.4 チェックポイントとの関係

- `CheckPointGuts` の `CheckPointCLOG` で、dirty な clog ページをすべて書き出して fsync する【確認: 呼び出し順】。redo ポイントより前のコミットレコードは、以後の REDO で再生されないからである。
- **clog ページの torn write が無害な理由**【推論】: clog のビットは IN_PROGRESS から COMMITTED または ABORTED へ一度だけ変わり、戻らない。チェックポイント後に clog ページを書いている途中で落ちると、ページの一部が古い内容のままになる。しかし、古い内容と新しい内容の違いは、すべて「チェックポイント後に決まった xid」のビットだけ。それらのコミットやアボートのレコードは redo ポイントより後にあるので、REDO が再び書く。チェックポイントより前に決まった xid のビットは、古い版と新しい版のどちらでも同じ値なので、どう破れても正しい。

### 6.5 クラッシュ時に実行中だった xid

- PostgreSQL は、クラッシュ時に実行中だった xid について、**clog に何も書かない**。
- `TransactionIdDidAbort` のコメントに「クラッシュで暗黙に abort されたトランザクションは、clog 上ではたいてい実行中のように見える。ほとんどの場合、`TransactionIdIsInProgress()` を先に確かめてから `TransactionIdDidCommit()` を使うべき」とある【確認】。
- 可視性の判定（`heapam_visibility.c`）は、(1) 実行中の一覧（procarray）にあれば実行中、(2) なければ clog で COMMITTED か、(3) どちらでもなければ abort、と判断する。リカバリ後の procarray は空なので、クラッシュ時に実行中だった xid はすべて (3) になる。
- abort と判断したときは、ヒントビット（`HEAP_XMIN_INVALID` など）を設定するので、次回から clog を見ない。
- `TrimCLOG` は、リカバリの終わりに、現在の clog ページのうち nextXid より後ろの部分をゼロにする【確認: 関数の存在】【未確認: 動作の細部】。xid は再利用されないので、本来は必要ないはずだが、念のための処理と思われる。
- **xid を再利用しないことの保証**【推論】: xid X を持つタプルがディスクにあるなら、WAL の原則により、そのタプルを書いた WAL レコード（xid X を持つ）が先に flush されている。そのレコードが redo ポイントより後なら、REDO の `AdvanceNextFullTransactionIdPastXid` が nextXid を X より大きくする。redo ポイントより前なら、チェックポイントレコードの nextXid が既に X より大きい（2.2 節の手順 2）。どちらでも、リカバリ後の nextXid は X より大きい。xid を割り当てただけで何も書かずに落ちたトランザクションの xid は、再利用されるかもしれないが、どこにも記録がないので害はない。

---

## 7. yuzhu のチェックポイント（推奨アルゴリズム）

### 7.1 前提（他の M3 文書と合わせる点）

- LSN は WAL の先頭からのバイト位置（u64）。ページヘッダに `page_lsn: u64` を持つ（M2 からの約束）。
- WAL レコードには `xid`（そのレコードを書いたトランザクション、なければ 0）を持たせる。
- xid は内部では 64 ビット（`FullTransactionId`）で扱い、制御ファイル・WAL では 64 ビットで書く。タプルヘッダに 32 ビットで持つか 64 ビットで持つかは MVCC の調査で決める。32 ビットなら PostgreSQL と同じく epoch を制御ファイルの `next_xid` の上位ビットから復元する。
- バッファプールは steal（未コミットの変更を含むページも追い出してよい）、no-force（コミット時にページを書かない）。xmin/xmax と clog で未コミットを不可視にするので、UNDO は要らない（`research-rust-db-arch.md` の推奨どおり）。

### 7.2 トリガ

M3 の checkpointer は専用のスレッド 1 本とする。

| トリガ | 既定値（推奨） | 備考 |
|---|---|---|
| 時間 | `checkpoint_timeout = 300s` | PostgreSQL と同じ |
| WAL 量 | redo ポイントから 256 MiB | M3 は `max_wal_size` を単純化して「redo ポイント以降の WAL がこれを超えたら」にする |
| `CHECKPOINT` コマンド | | スーパーユーザのみ（M3 は権限がないので全員）。完了を待って返す |
| シャットダウン | | シャットダウンのチェックポイント |
| リカバリの終了 | | 終了チェックポイント（第 8 節） |
| テスト用 | 任意のレコード数ごと | クラッシュテストで FPW を頻繁に起こすための設定 |

書き出しの時間的な分散（`checkpoint_completion_target`）は M3 では入れない。後で `BufferSync` の間に sleep を挟むだけで追加できる。

### 7.3 手順

```
fn create_checkpoint(kind: Online | Shutdown | EndOfRecovery):
  0. checkpoint_lock を取る（チェックポイントは同時に 1 つだけ）

  1. redo ポイントを決める
     Online:
       WAL 挿入ロックを取る
         redo_lsn = 現在の挿入位置
         CHECKPOINT_REDO レコードを挿入する（中身なし）
         shared.redo_lsn = redo_lsn   // 以後の FPW 判定はこの値を使う
       ロックを外す
     Shutdown / EndOfRecovery:
       （他に書き手がいないことを確認済み）redo_lsn = 現在の挿入位置
       shared.redo_lsn = redo_lsn

  2. next_xid, next_oid, oldest_xid を読む（xid 割り当てのロックの中で）

  3. clog の dirty ページをすべて書き出す
  4. バッファプールで、この時点で dirty なページに checkpoint_needed を付ける
     付けたページを 1 つずつ:
       content lock（共有）を取る
       WAL を page_lsn まで flush する（WAL の原則）
       ページのコピーを取る → チェックサムを計算 → ファイルに書く
       dirty を外す（コピーの後に再び dirty になったものは残す）
       そのファイルを「fsync が必要」な集合に入れる
  5. 「fsync が必要」な集合のファイルと clog ファイルをすべて fsync する
     （バックエンドが追い出しで書いたファイルも、追い出し時にこの集合に入れておく）
     fsync が失敗したら PANIC（9.6 節）

  6. CHECKPOINT レコード { redo_lsn, next_xid, next_oid, oldest_xid,
                          kind, full_page_writes } を挿入し、flush する
     checkpoint_lsn = その開始位置

  7. 制御ファイルを更新する
     { checkpoint_lsn, redo_lsn, next_xid, next_oid, oldest_xid,
       state = (Shutdown なら ShutDown、それ以外は InProduction) }
     を 2 スロット方式で書き、fdatasync する  ← ここがチェックポイントの完了点

  8. redo_lsn を含むセグメントより前の WAL セグメントを削除する
     （削除後に pg_wal ディレクトリを fsync。削除が失敗しても正しさには影響しない）
```

- **手順 1 で `CHECKPOINT_REDO` レコードを入れる理由**: redo ポイントにレコードを置くと、REDO が「redo_lsn から読み始める」とき、そこに必ず有効なレコードがあることが保証される。WAL リーダの実装とテストが単純になる。PostgreSQL 17 の方式とも一致する。
- **手順 4 でページのコピーを取ってから書く**のは、書き込み中にページが変わらないようにするため（PostgreSQL も共有ロックを持ったまま書く。チェックサムを計算するときはコピーを取る）。M3 では content lock を持ったまま書いてもよい。
- **M3 の単一ライターで「チェックポイント中は書き込みを止める」簡易版**も可能だが、推奨しない。止めても WAL 形式も FPW の規則も変わらないので、得られる単純さは小さい。逆に、M5 で必ずファジー方式に作り直すことになる。ファジー方式の追加の工数は 1〜2 日程度と見積もる。
- **追い出し（eviction）**: バックエンドがページを追い出すときも、WAL を page_lsn まで flush してから書く。そのファイルを fsync 要求の集合に入れる。追い出しのたびに fsync はしない。

### 7.4 新しいリレーションファイルと削除

- `CREATE TABLE` でファイルを作るときは、`SMGR_CREATE` レコードを WAL に書いてからファイルを作り、**親ディレクトリを fsync** する。REDO で `SMGR_CREATE` に出会ったら、ファイルがなければ作る。
- `DROP TABLE` のファイル削除は**コミット後**に行う（コミットレコードに削除するファイルの一覧を入れ、REDO でも削除する）。abort したら削除しない。PostgreSQL と同じ方式【未確認: `xl_xact_relfilelocators` の名前】。
- 作成したトランザクションが abort したファイル、コミット直後に落ちて削除しそこねたファイルは「孤児ファイル」として残る。PostgreSQL はこれを放置する。yuzhu は**リカバリの最後に、カタログにないファイルを消す**処理を入れてもよい（M3 で任意。工数は半日程度）。
- テーブルの延長（ファイルの末尾にページを足す）は、新しいページの初期化レコードが WAL にあれば十分で、ファイル長の fsync は要らない。REDO 時にファイルが短ければゼロで延長する。

---

## 8. yuzhu のリカバリ手順（推奨）

```
fn startup(data_dir):
  1. data_dir をロックする（postmaster.pid 相当のファイルと flock。二重起動を防ぐ）
  2. 制御ファイルを読む（2 スロットから有効で新しい方）
     magic / format_version / catalog_version / page_size / wal_segment_size を検証
     不一致なら起動を拒否（エラーメッセージを出して終了）
  3. checkpoint_lsn のレコードを読む
     読めない、CHECKPOINT レコードでない、redo_lsn が制御ファイルと食い違う → PANIC
  4. need_redo = !(state == ShutDown
                  && チェックポイントの kind == Shutdown
                  && checkpoint レコードの直後が WAL の末尾)
  5. need_redo なら:
     a. state = InCrashRecovery を制御ファイルに書く
     b. next_xid / next_oid をチェックポイントレコードから設定する
     c. redo_lsn から順にレコードを読む。各レコードについて:
        - next_xid = max(next_xid, record.xid + 1)
        - rmgr ごとの redo を呼ぶ（下の表）
        - 有効でないレコード（長さ・CRC・xl_prev・セグメントヘッダの不一致）に
          当たったら、そこを end_of_wal として止める
     d. invalid page の集合が空でなければ PANIC
     e. end_of_wal 以降を片付ける:
        - 現在のセグメントの end_of_wal 以降をゼロで埋めて fsync
        - それより後ろのセグメントファイルを削除し、pg_wal を fsync
     f. WAL の書き込み位置を end_of_wal にする
     g. 終了チェックポイント（kind = EndOfRecovery）を取る。完了を待つ
        → 制御ファイルの state = InProduction
  6. need_redo でなければ、WAL の書き込み位置をチェックポイントレコードの直後にする
  7. 孤児ファイルの掃除（任意）
  8. checkpointer スレッドを起動し、接続の受け付けを始める
```

**rmgr ごとの redo（M3 で必要なもの）**:

| レコード | redo の動作 |
|---|---|
| `CHECKPOINT_REDO` | 何もしない |
| `CHECKPOINT`（Online / Shutdown / EndOfRecovery） | next_oid を更新する。next_xid は max を取る |
| `NEXT_OID` | next_oid を更新する（OID を一定数ずつ先取りしたときに出す。PostgreSQL の `XLOG_NEXTOID` と同じ） |
| `FPI`（ヒントビット用などのイメージだけのレコード） | ページを上書きする |
| heap の insert / update / delete / lock / ページ初期化 | 4.3 節の規則（イメージがあれば上書き、`page_lsn >= lsn` なら飛ばす、それ以外は適用して `page_lsn = lsn`） |
| `COMMIT { xid, dropped_files }` | clog に COMMITTED。dropped_files を削除する（なければ無視） |
| `ABORT { xid }` | clog に ABORTED |
| `SMGR_CREATE { rel }` | ファイルがなければ作る |
| `SMGR_TRUNCATE { rel, nblocks }` | ファイルを切り詰める。invalid page の記録から、該当するものを消す |

**REDO の間の設計上の注意**:

- REDO は単一スレッドで、ページはバッファプールを通して読み書きする（追い出しも普通に起きてよい。WAL の原則は、REDO 中は「読んだ位置まで WAL が既にディスクにある」ので自動的に満たされる）。
- REDO 中にエラーが起きたら（レコードの形式が不正、ページの中身がレコードと合わない）、PANIC にする。黙って飛ばすと、データが静かに壊れる。
- **clog の扱い**: yuzhu は `CLOG_ZEROPAGE` レコードを**使わない**ことを推奨する。代わりに「clog ファイルの末尾より先、またはファイルがないページを読んだら、全部ゼロ（= IN_PROGRESS）として扱う」と決める。PostgreSQL が `CLOG_ZEROPAGE` を必要とするのは SLRU の作りによるもので、yuzhu の単純な実装では不要【推論】。clog の切り詰め（`CLOG_TRUNCATE` 相当）は M5 の VACUUM と一緒に入れる。
- **クラッシュ時に実行中だった xid**: PostgreSQL と同じく何も書かない。可視性の判定は「実行中の一覧にあれば実行中 → clog が COMMITTED ならコミット → それ以外は abort」の順にする。
  - 代案として、リカバリの終わりに「`[最後のチェックポイントの next_xid, 現在の next_xid)` の範囲で clog が IN_PROGRESS の xid を ABORTED に書き換える」こともできる。clog を見ただけで状態が決まるので、デバッグやテストの検査が楽になる。しかし、範囲が広いと遅い。また PostgreSQL の規則と二重になる。**推奨は PostgreSQL と同じ暗黙の abort**。テストの検査関数で同じ規則を実装すれば足りる。
- 終了チェックポイントは、PostgreSQL と同じく完了を待ってから接続を受け付ける。M3 では時間より単純さを優先する。

---

## 9. クラッシュテストの方法

### 9.1 障害モデル（何を想定するか）

Linux の FS とディスクについて、次を前提にする。障害注入 FS はこのモデルを忠実に再現する。

1. `fsync` / `fdatasync` が成功したファイルの内容は、電源断の後も残る。
2. fsync していない書き込みは、電源断の後に**残る・消える・一部だけ残る**のどれでもありうる。ファイル内の書き込みの順序も保証されない（後の書き込みだけが残ることもある）。
3. 1 回の `write` の中でも、セクタ（512 バイト）またはページ（4KB）単位で一部だけ残ることがある（torn write）。制御ファイルの検証には、さらに厳しく任意のバイト境界での破れも使う。
4. ファイルの作成・削除・リネームは、**親ディレクトリを fsync するまで**残るとは限らない。ファイル長の変化（延長・切り詰め）も、そのファイルの fsync まで残るとは限らない。
5. `fsync` が EIO を返したら、未書き込みのデータは失われたとみなす（dirty なページキャッシュが捨てられうる）。その後 fsync を再試行して成功しても、データが残った保証はない（PostgreSQL の "fsyncgate"。12 で `data_sync_retry = off` を既定にし、fsync 失敗で PANIC するようになった: <https://www.postgresql.org/docs/17/runtime-config-error-handling.html#GUC-DATA-SYNC-RETRY>）【未確認: 導入された版の細部】。
6. プロセスだけが落ちる（kill -9）場合は、OS のページキャッシュに書いた内容はすべて残る。

参考: Pillai et al., "All File Systems Are Not Created Equal: On the Complexity of Crafting Crash-Consistent Applications"（OSDI 2014）。アプリケーションがファイル操作の順序と永続性をどう誤るかの分類と、その検出ツール ALICE【未確認: 書誌の細部】。

### 9.2 ファイル I/O の抽象化レイヤ（CLAUDE.md の要件）

すべてのファイル I/O をトレイト経由にする。M2 の `DiskManager` もこれを使う。案:

```rust
pub trait Vfs: Send + Sync {
    fn open(&self, path: &Path, opts: OpenOptions) -> Result<Box<dyn VfsFile>>;
    fn remove(&self, path: &Path) -> Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    fn create_dir_all(&self, path: &Path) -> Result<()>;
    fn sync_dir(&self, path: &Path) -> Result<()>;   // ディレクトリの fsync
    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;
    fn exists(&self, path: &Path) -> Result<bool>;
}

pub trait VfsFile: Send + Sync {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize>;
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()>;  // 全部書くか、エラー
    fn sync_data(&self) -> Result<()>;   // fdatasync
    fn sync_all(&self) -> Result<()>;    // fsync
    fn set_len(&self, len: u64) -> Result<()>;
    fn len(&self) -> Result<u64>;
}
```

- 本番の実装 `OsVfs` は `std::fs` と `std::os::unix::fs::FileExt` で作る（`unsafe` 不要）。
- テスト用の実装 `SimVfs` はメモリ上で動く（9.3 節）。
- エラーは `std::io::Error` を yuzhu のエラー型に包む。fsync の失敗は呼び出し側で PANIC に変える（`58030 io_error` を出してから `std::process::abort` ではなく、サーバ全体を停止する経路を用意する。テストではパニックを捕まえて「クラッシュ」として扱う）。

### 9.3 層 1: プロセス内の決定的シミュレーション（主力）

**SimVfs の状態**: ファイルごとに `durable: Vec<u8>`（fsync 済みの内容）と `pending: Vec<WriteOp>`（fsync していない書き込み・set_len の列）を持つ。ディレクトリごとに、fsync 済みのエントリと未 fsync の作成・削除・リネームの列を持つ。読み込みは「durable に pending を順に適用した内容」を返す（OS のページキャッシュと同じ見え方）。

**クラッシュの起こし方**:

- I/O 操作に通し番号を振り、「N 番目の操作の直前（または直後）でクラッシュする」と指定する。小さなワークロードでは N を 0 から最後まで**全部試す**（網羅的な列挙）。大きなワークロードでは乱数で選ぶ。
- クラッシュしたら、エンジン（サーバの全スレッド）を止める。テストではエンジンをライブラリとして呼び、`SimVfs` が以後の I/O をすべてエラーにすることで止める。その後、エンジンのオブジェクトを捨てる。
- **クラッシュ後のディスクイメージの作り方**（ポリシーを乱数のシードで選ぶ）:
  - `DropAll`: pending をすべて捨てる
  - `KeepAll`: pending をすべて適用する（kill -9 と同じ）
  - `RandomSubset`: pending の各操作を独立に、確率 p で適用する（順序の入れ替わりも再現される）
  - `Torn`: 選んだ書き込みを 512 バイトか 4KB 単位で分け、一部のセクタだけ適用する。制御ファイルには任意のバイト境界での破れも使う
  - ディレクトリ操作も同様に、未 fsync のものを残すか捨てる
- 新しい `SimVfs`（クラッシュ後のイメージ）でエンジンを起動し、リカバリさせてから不変条件を検査する（9.5 節）。

**その他の注入**:

- 指定した操作で `EIO`・`ENOSPC` を返す。書き込みのエラーはそのトランザクションのエラーになるべきで、fsync のエラーは PANIC になるべき。それぞれ検査する。
- **リカバリ中のクラッシュ**: リカバリの I/O の N 番目でもう一度クラッシュさせ、再リカバリする。

**再現性**: 乱数はシード 1 つから決める。スレッドの実行順は決定的にならないので、層 1 のワークロードは**1 スレッドで複数セッションを順番に操作する**形にする（エンジンの `Session` API を直接呼ぶ。プロトコルは通さない）。これでスレッドのスケジューリングに左右されない。checkpointer もテストでは手動で呼ぶ（「k 文ごとにチェックポイント」）。失敗したらシードとポリシー、N を表示し、それだけで再現できるようにする。

**ワークロードの例**:

1. **銀行振込**: 口座 10 件、初期残高の合計は固定。各トランザクションで 2 口座間を移す。複数のトランザクションを交互に進め、一部は ROLLBACK、一部はクラッシュ時に実行中のままにする。
2. **追記ログ**: 各トランザクションが `(txn_id, seq)` の行を k 行挿入する。トランザクションごとの行数が 0 か k のどちらかであることを確かめる。
3. **更新の繰り返し**: 少数の行を何度も UPDATE し、同じページへの多数の変更とチェックポイントをまたがせる（FPW の経路を通す）。
4. **DDL の混在**: CREATE TABLE、DROP TABLE、ROLLBACK される CREATE TABLE。
5. **小さなバッファプール**（例: 8 フレーム）で、追い出しと steal を頻繁に起こす。チェックポイントの間隔も極端に短くする（例: 5 レコードごと）。

### 9.4 層 2: 実プロセスの kill -9 ハーネス

- `yuzhu-server` を一時ディレクトリのデータで起動し、複数の接続（`postgres` クレートの同期版を dev-dependency にする想定。依存の追加は要検討）から並行にワークロードを流す。
- クライアントは、**COMMIT の CommandComplete を受け取ったトランザクションだけ**を「確定」として、ハーネス側のファイルに記録する。COMMIT を送ったが応答前に接続が切れたものは「不明」として記録する。
- ランダムな時刻に `kill -9` し、再起動してから不変条件を検査する。これを数十〜数百回くり返す。
- kill -9 ではページキャッシュが残るので、確かめられるのは「プロセスのクラッシュ」だけ（障害モデルの 6）。電源断の再現は層 1 の役目とする。
- 任意の発展: Linux 限定なので、Jepsen の LazyFS（FUSE で「fsync されていないデータを捨てる」FS を作るツール）や、device-mapper の `dm-log-writes` を使えば、実プロセスでも電源断を再現できる。導入の工数が大きいので M3 では見送り、記録だけ残す【未確認: 各ツールの現状】。
- CI では、層 1 の固定シードのテスト（数秒〜数十秒）を `cargo test` で毎回動かす。長時間のランダム実行と層 2 は `#[ignore]` か別のジョブ（夜間）にする。

### 9.5 検査する不変条件

| # | 不変条件 | 検査の方法 |
|---|---|---|
| I1 | **永続性**: コミットが確認応答されたトランザクション（層 1 ではコミットの fsync が戻ったもの）は、リカバリ後にすべて見える | 確定トランザクションの書いた値が全部あるか |
| I2 | **原子性**: 部分的なトランザクションは見えない | 振込の合計が一定。追記ログの行数が 0 か k |
| I3 | **未確定のトランザクション**: 応答前に落ちたトランザクションは「全部見える」か「全部見えない」のどちらか | 「不明」のトランザクションについて I2 と同じ検査 |
| I4 | **モデルとの一致**: リカバリ後の全テーブルの内容が、モデル（`BTreeMap` で持つ確定済みの状態に、不明なトランザクションの任意の部分集合を加えたもの）のどれかと一致する | テストが持つ参照モデルと全件比較 |
| I5 | **xid を再利用しない**: リカバリ後の next_xid は、WAL とヒープに現れるすべての xid より大きい | ヒープを全走査して xmin/xmax の最大値と比べる |
| I6 | **クラッシュ時に実行中だった xid は abort 扱い**: その xid が書いたタプルは見えず、clog は COMMITTED になっていない | 実行中だった xid を記録しておき、clog と可視性を確かめる |
| I7 | **clog と WAL の一致**: 保持されている WAL にコミットレコードがある xid は、clog で COMMITTED。abort レコードがあれば ABORTED | WAL を読む検査ツールで比べる |
| I8 | **構造の健全性**: 全ページのヘッダ・チェックサム・line pointer が正しい。`page_lsn` は WAL の末尾以下。制御ファイルが有効で、チェックポイントレコードを指している。カタログにあるリレーションのファイルがすべて存在する | 整合性検査関数（`amcheck` の小さい版）を用意する |
| I9 | **冪等性**: リカバリの途中でクラッシュしても、再リカバリ後の論理的な内容は同じ。2 回続けてリカバリしても同じ | リカバリ中のクラッシュ注入。結果を比べる |
| I10 | **リカバリ後も動く**: リカバリ後に新しいトランザクションを実行し、再びクラッシュ・リカバリしても I1〜I9 が成り立つ | 「ワークロード → クラッシュ → リカバリ」を数回くり返す |
| I11 | **fsync の失敗**: fsync が EIO を返したら、サーバは PANIC で止まり、その後のリカバリで I1〜I8 が成り立つ | EIO の注入 |
| I12 | **WAL の末尾**: リカバリ後の WAL の書き込み位置の後ろに、古いレコードの残骸が有効なレコードとして読まれない | 2 回目のクラッシュで、古い残骸を誤って REDO しないことを確かめる（I10 で同時に検査） |

### 9.6 ハーネス自体のテスト（変異テスト）

不変条件の検査が正しく動いているかを確かめるため、わざと壊した設定で実行し、**失敗を検出できること**をテストにする。

| わざと壊すもの | 検出されるべき不変条件 |
|---|---|
| FPW を無効にする（`Torn` ポリシーで） | I4、I8 |
| コミット時の WAL の fsync をしない（`DropAll` ポリシーで） | I1 |
| WAL の原則を破る（ページの書き出し前に WAL を flush しない） | I2、I4 |
| チェックポイントで clog を fsync しない | I1 または I7 |
| 制御ファイルを 1 スロットで上書きする（任意のバイト境界の `Torn` で） | 起動不能を検出 |
| REDO で `page_lsn >= lsn` の判定を外す（二重適用） | I4 |
| ディレクトリの fsync をしない | 新しいテーブルが消える（I1、I8） |

これらがすべて検出できれば、ハーネスは少なくとも主要な誤りを見逃さないと言える。この表をそのまま Rust のテストにする（各変異をテスト用の設定フラグで有効にする）。

---

## 10. M5 以降への配慮

- **複数ライター**: チェックポイントの手順は変わらない。WAL 挿入の並列化に合わせて、FPW の判定を「ロックの外で組み立て、ロックの中で `fpw_lsn <= redo_lsn` を検証してやり直す」方式に変える（5.4 節）。
- **行ロック**: PostgreSQL は行ロック（xmax にロック専用のビットを立てる）も `XLOG_HEAP_LOCK` で WAL に記録する。タプルヘッダの変更なので、REDO で同じ状態にしないと、ページの内容が WAL と食い違うから【未確認: 理由の書き方】。yuzhu も M5 でロックのレコードを足す。rmgr の表に入れる場所を空けておく。
- **Repeatable Read**: リカバリには影響しない。
- **VACUUM と xid の凍結**: clog の切り詰めと `oldest_xid` の更新を WAL に記録する必要がある。制御ファイルの `oldest_xid` はそのために用意してある。
- **グループコミット**: コミットの手順（WAL の flush を待つ部分）だけが変わる。チェックポイントとリカバリには影響しない。非同期コミットを入れるなら、clog に `group_lsn` 相当が必要になる（6.3 節）。
- **レプリケーション・PITR**: 制御ファイルの `timeline`、`min_recovery_lsn` の場所だけ確保した。

---

## 11. 工数の目安（M3 のうち、この文書の範囲）

| 項目 | 目安 |
|---|---|
| `Vfs` トレイトと `OsVfs`、既存コードの置き換え | 1 日 |
| 制御ファイル（2 スロット、CRC、CLI 表示） | 0.5〜1 日 |
| checkpointer スレッドとファジーチェックポイント（clog、バッファの書き出し、fsync 集約、WAL セグメント削除） | 2〜3 日 |
| FPW（判定、イメージの穴の省略、REDO での上書き） | 1〜2 日 |
| リカバリ（REDO ループ、rmgr ごとの redo、WAL 末尾の片付け、invalid page、終了チェックポイント） | 3〜4 日 |
| `SimVfs` と層 1 のハーネス（ポリシー、網羅的な列挙、モデル、不変条件、変異テスト） | 4〜6 日 |
| 層 2 の kill -9 ハーネス | 1〜2 日 |

---

## 12. QUESTIONS.md に記録すべき判断

1. 制御ファイルを PostgreSQL と同じ 1 スロット上書きにせず、2 スロット方式にした（障害注入テストで任意のバイト境界の torn write を扱うため）。
2. `CLOG_ZEROPAGE` を使わず、「clog の未作成ページは全部ゼロ」と決めた。
3. クラッシュ時に実行中だった xid は、PostgreSQL と同じく clog に何も書かない（暗黙の abort）。
4. M3 のチェックポイントは、書き出しの時間的な分散をしない。
5. ページチェックサムを M3 で入れるかどうか（推奨は入れる。ヒントビットの扱いは MVCC の調査と合わせて決める）。
6. 層 2 のハーネス用に、同期版の PostgreSQL クライアントクレートを dev-dependency に追加するか（代案: `psql` をサブプロセスで呼ぶ）。
