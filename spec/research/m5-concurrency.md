# yuzhu M5 調査: 複数ライターの同時実行制御（行ロック・待ち・EvalPlanQual・Repeatable Read・デッドロック・テーブルロック・VACUUM）

調査日: 2026-10-04。対象は PostgreSQL **REL_17_STABLE**。ソースは `raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/...` から取得して読んだ。行番号は取得時点のもので、ブランチの更新でずれることがある。実機確認は Docker の `postgres:17`（**PostgreSQL 17.11**）で行った（付録 A）。

前提（M3 までの決定。`spec/research/m3-mvcc.md`、`m3-tx-semantics.md`、`m3-wal.md`、`m3-recovery.md`、`m2-page-heap.md`、`m2-buffer-io.md`）:

- ヒープ + 8KB ページ。タプルヘッダに **64 ビット XID**（`xmin`/`xmax` は u64）、`cmin`/`cmax` は別フィールド、infomask のビット位置は PostgreSQL と同じ。
- M3 は「グローバル書き込みロック 1 本による単一ライター + 複数リーダー」「Read Committed のみ」。`heap_update`/`heap_delete` は最初から `TmResult` を返す API で、M3 では `Updated`/`Deleted`/`BeingModified` は内部エラー扱い。
- REDO のみの WAL、ファジーチェックポイント、full page write。WAL の HEAP rmgr に `0x40 LOCK`（M5）の枠を予約済み。clog の切り詰め（`CLOG_TRUNCATE`）は M5 の VACUUM と一緒に入れると決めてある。
- 接続ごとに 1 スレッド・同期 I/O、safe Rust のみ、コア部品は手書き。
- バッファプールは「ピン（`PinnedBuffer`）とコンテンツラッチ（`RwLock<PageBuf>`）を型で分ける」設計。`FrameHeader.cleanup_waiter` を M5 用に予約済み。

記号:

- 【確認】今回ソースまたはドキュメントを開いて確かめた事実
- 【実機】PostgreSQL 17.11 で実際に動かして確かめた事実（第 2 節の番号を付ける）
- 【記憶】過去の知識に基づく記述。今回は細部まで照合していない（**未検証**）
- 【提案】yuzhu への推奨（PostgreSQL の事実ではない）

工数感の目安: **S** = 1〜2 日、**M** = 3〜7 日、**L** = 1〜3 週間（実装 + 単体テスト。分離性テストの整備は別に数える）。

---

## 0. 主な出典

ソース（すべて REL_17_STABLE）:

| ファイル | 読んだ箇所 | URL |
|---|---|---|
| heapam.c | `heap_delete`（2855 行〜、ラベル `l1:` 2914 行）、`heap_update`（3348 行〜、`l2:` 3578 行）、`heap_lock_tuple`（4803 行〜、`l3:` 4847 行）、`heap_acquire_tuplock`（5529 行） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/heapam.c> |
| README.tuplock | 行ロックの 2 段構成、4 つのロック強度と衝突表、MultiXact、infomask | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/README.tuplock> |
| heapam_handler.c | `heapam_tuple_lock`（355 行〜、`TUPLE_LOCK_FLAG_FIND_LAST_VERSION` で ctid 連鎖をたどる） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/heapam_handler.c> |
| heapam_visibility.c | `HeapTupleSatisfiesUpdate`、`HeapTupleSatisfiesVacuum` | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/heapam_visibility.c> |
| htup_details.h | infomask のビット（194〜282 行） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/htup_details.h> |
| lockoptions.h | `LockClauseStrength`、`LockWaitPolicy`、`LockTupleMode` | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/nodes/lockoptions.h> |
| lockdefs.h | テーブルロックの 8 モード | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/storage/lockdefs.h> |
| lock.c | `LockConflicts[]`（衝突表） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/lmgr/lock.c> |
| lmgr.c | `XactLockTableInsert`（616 行）、`XactLockTableWait`（657 行） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/lmgr/lmgr.c> |
| proc.c | `ProcSleep`（1107 行、待ち行列への割り込み規則）、`CheckDeadLock` | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/lmgr/proc.c> |
| deadlock.c | `DeadLockCheck`（217 行）、hard/soft edge、`DeadLockReport`（1104・1132 行） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/lmgr/deadlock.c> |
| lmgr/README | ロックマネージャ全体 | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/lmgr/README> |
| nodeModifyTable.c | `ExecDelete`（1533〜1669 行）、`ExecUpdate`（2371〜2508 行）の `TM_Result` 分岐 | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeModifyTable.c> |
| nodeLockRows.c | `SELECT ... FOR UPDATE` の行ロック（182〜237 行） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeLockRows.c> |
| execMain.c | `EvalPlanQual`（2529 行） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/execMain.c> |
| executor/README | EvalPlanQual の説明 | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/README> |
| vacuumlazy.c | `lazy_scan_heap`（816 行、3 段階の説明）、`lazy_vacuum_all_indexes`、`lazy_vacuum_heap_rel`、`lazy_truncate_heap` | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/vacuumlazy.c> |
| pruneheap.c | `heap_page_prune_opt`（193 行、機会的 pruning の条件） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/pruneheap.c> |
| procarray.c | `ComputeXidHorizons`（1735 行）、`GetOldestNonRemovableTransactionId`（2005 行） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/ipc/procarray.c> |
| vacuum.c | `vac_update_datfrozenxid`、`vac_truncate_clog` | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/commands/vacuum.c> |
| README.HOT | HOT の仕組み | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/README.HOT> |
| xact.c | `RecordTransactionCommit`（1488 行: WAL を書いたトランザクションは fsync） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/xact.c> |
| guc_tables.c | `deadlock_timeout`（既定 1000ms）、`autovacuum_naptime`（60s）、`autovacuum_vacuum_threshold`（50）、`autovacuum_vacuum_scale_factor`（0.2） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/misc/guc_tables.c> |
| isolation_schedule | PostgreSQL の分離性テスト一覧（移植候補） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/test/isolation/isolation_schedule> |

ドキュメント（17）:

- 13.2 Transaction Isolation: <https://www.postgresql.org/docs/17/transaction-iso.html>
- 13.3 Explicit Locking（テーブルロック・行ロック・デッドロック）: <https://www.postgresql.org/docs/17/explicit-locking.html>
- SELECT の Locking Clause: <https://www.postgresql.org/docs/17/sql-select.html#SQL-FOR-UPDATE-SHARE>
- LOCK: <https://www.postgresql.org/docs/17/sql-lock.html>
- 20.12 Lock Management（`deadlock_timeout` など）: <https://www.postgresql.org/docs/17/runtime-config-locks.html>
- 24.1 Routine Vacuuming: <https://www.postgresql.org/docs/17/routine-vacuuming.html>
- VACUUM: <https://www.postgresql.org/docs/17/sql-vacuum.html>
- 65.7 Heap-Only Tuples (HOT): <https://www.postgresql.org/docs/17/storage-hot.html>
- 52.74 pg_locks: <https://www.postgresql.org/docs/17/view-pg-locks.html>

---

## 1. 結論（推奨の要約）

1. **ロックマネージャを 1 つ作り、すべての「トランザクションをまたぐ待ち」をそこに通す**【提案】。ロック対象（`LockTag`）は `Relation`、`Tuple`、`TransactionId`、（将来の）`Object` など。モードは PostgreSQL の 8 段階と同じ衝突表（第 3 節）。行の更新待ちは PostgreSQL と同じく「相手の XID に対する ShareLock 待ち」として表現する。こうすると、行ロック待ちとテーブルロック待ちが同じ待ちグラフに乗り、デッドロック検出が 1 か所で済み、`pg_locks` もそのまま作れる。**工数 M**。
2. **行ロックは PostgreSQL と同じく xmax + infomask で表す**。ロック強度は 4 つ（`FOR KEY SHARE` / `FOR SHARE` / `FOR NO KEY UPDATE` / `FOR UPDATE`）を構文・意味ともに受け付ける。**MultiXact は「メモリ上だけに置くロック専用版」（案 B）にする**【提案】。共有ロックを複数のトランザクションが同時に持てるが、「ロック保持者と更新者の同居」（`FOR KEY SHARE` 中の非キー列 UPDATE）は認めず衝突扱いにする。PostgreSQL 9.2 以前と同じ挙動で、M5 の FOREIGN KEY では「子の INSERT 中に親の非キー列 UPDATE が待たされる」差が出る。永続 MultiXact（案 C）は M6 以降。第 4 節。**工数 M**（案 C なら L）。
3. **`heap_update`/`heap_delete`/`heap_lock_tuple` は PostgreSQL の `l1:`/`l2:`/`l3:` ループをそのまま移植する**: `satisfies_update` → `BeingModified` なら「ページラッチを離す → タプルロック（公平性の順番取り）→ 相手 XID の終了待ち → ラッチを取り直す → xmax が変わっていたら最初から」。結果は `TmResult` で呼び出し側（実行器）に返し、分岐は実行器で行う（第 5 節）。**工数 M**。
4. **Read Committed の EvalPlanQual は「対象テーブルの最新版をロックしてから、元の結合行の他テーブル部分は固定したまま、WHERE と SET 式を評価し直す」簡易版にする**【提案】。PostgreSQL の EPQ がやっている「計画木の部分再実行」はしない。単一テーブルの UPDATE/DELETE と `SELECT ... FOR UPDATE` では PostgreSQL と結果が一致する。結合・サブクエリを含む場合の差は第 6.4 節。**工数 M**。
5. **Repeatable Read は「トランザクションの最初の文で取ったスナップショットを使い回す」+「`Updated`/`Deleted` が返ったら 40001」だけで PostgreSQL と同じになる**。メッセージは更新なら `could not serialize access due to concurrent update`、削除なら `... concurrent delete`（`SELECT FOR UPDATE` はどちらも `concurrent update`）【確認・実機 #4〜#4f】。相手がアボートしたら続行する（#4b）。**SERIALIZABLE は M5 でも 0A000 のまま**（SSI は対象外）【提案】。**工数 S**。
6. **デッドロック検出は PostgreSQL と同じ「`deadlock_timeout`（既定 1s）待ってから、待った本人が待ちグラフを調べ、循環があれば自分を 40P01 で中断する」**【確認・実機 #5】。被害者選択はしない（タイマーが先に切れた方が中断される）。soft edge（待ち行列の順序による待ち）は、PostgreSQL の「待ち行列の並べ替えで解消」を実装せず、**ProcSleep の割り込み規則だけ入れて、残った循環はデッドロックとして報告する**簡易版を推奨【提案】。**工数 M**。
7. **テーブルロックを入れ、M3 のグローバル書き込みロックを廃止する**。文ごとのモードは PostgreSQL と同じ（SELECT = AccessShare、INSERT/UPDATE/DELETE = RowExclusive、SELECT FOR UPDATE = RowShare、VACUUM = ShareUpdateExclusive、CREATE INDEX = Share、DROP/TRUNCATE/大半の ALTER = AccessExclusive）。名前解決は「解決 → ロック → カタログを最新で読み直して OID が同じか確認」（第 9 節）。`LOCK TABLE` も実装する。**工数 M**。
8. **VACUUM は M5 では手動（`VACUUM [VERBOSE] [table]`）+ 機会的 pruning（ページ単位の掃除）**。autovacuum は「閾値で起動する単一ワーカー」の簡易版を M5 の後半か M6 に回す（確認事項）。3 段階（ヒープ走査で pruning と死んだ TID の収集 → インデックスから削除 → ヒープの行ポインタを UNUSED に）を PostgreSQL どおりに作る（第 12 節）。**工数 L**。
9. **64 ビット XID なので周回対策（anti-wraparound）は不要**だが、**clog の切り詰めのために「凍結（WAL に記録するヒントビット確定）」と、テーブルごとの「これより古い XID はこのテーブルに残っていない」境界（`relfrozenxid` 相当）は必要**。ヒントビットは WAL に残らないので、凍結を WAL に記録しないとクラッシュ後に clog を引けなくなる（第 12.6 節）。**工数 M**。
10. **HOT は M5 では入れない**（M6 以降）。ただし pruning と行ポインタの `LP_REDIRECT`・infomask2 の `HEAP_HOT_UPDATED`/`HEAP_ONLY_TUPLE` はフォーマットで予約済みなので、後から足せる（第 13 節）。

---

## 2. 実機確認の結果一覧（PostgreSQL 17.11）

付録 A の最小ワイヤプロトコルクライアントで、2〜4 本の接続から文を送り、400ms 以内に応答がなければ「ブロック」と判定した。表の A/B/C は接続。

| # | 手順 | 結果 | 意味 |
|---|---|---|---|
| 1 | A: `begin; update t set v=v+1 where id=1` / B: 同じ UPDATE（ブロック）/ A: commit | B は `UPDATE 1`、最終値は 12（2 回分加算） | RC: 待った後、最新版に対して SET を再計算（更新消失なし） |
| 2 | A: `update t set v=99 where id=1`（未コミット）/ B: `update t set v=v+1 where v=10 returning *`（ブロック）/ A: commit | B は `UPDATE 0` | EPQ: 最新版で WHERE を再評価して外れた |
| 3 | A: `delete ... id=1`（未コミット）/ B: `update ... id=1`（ブロック）/ A: commit | B は `UPDATE 0` | RC: 相手が削除してコミットしたら黙って飛ばす |
| 4 | B: RR で `select * from t` / A: `update ... id=1`（自動コミット）/ B: `update ... id=1` | `ERROR 40001 could not serialize access due to concurrent update`、RFQ `E` | RR: スナップショット後のコミット済み更新に当たると 40001 |
| 4b | B: RR / A: `begin; update`（未コミット）/ B: update（ブロック）/ A: rollback | B は `UPDATE 1` で続行、commit 可 | RR: 相手がアボートしたら成功 |
| 4c | 4b と同じで A が commit | B は 40001（待った後で失敗） | |
| 4d | B: RR / A: `delete ... id=2` / B: `update ... id=2`、または `delete ... id=1` | どちらも `ERROR 40001 could not serialize access due to concurrent delete` | 削除に当たったときはメッセージが違う |
| 4e | B: `begin isolation level repeatable read`（文なし）/ A: update（自動コミット）/ B: update | B は `UPDATE 1`、A の結果（556）が見える | RR のスナップショットは BEGIN ではなく最初の文で取る |
| 4f | B: RR / A: update / B: `select ... for update` | `40001 ... concurrent update` | FOR UPDATE も同じ |
| 4g | B: RR / A: insert / B: count、同じ行の update | count は変わらず、update は `UPDATE 0`、commit 可 | 挿入（ファントム）は RR ではエラーにならない |
| 5 | A・B が交差して行を更新 | **先に待ち始めた A** が約 1 秒後に `ERROR 40P01 deadlock detected`、DETAIL `Process 91 waits for ShareLock on transaction 777; blocked by process 92.\nProcess 92 waits for ShareLock on transaction 776; blocked by process 91.`、HINT `See server log for query details.`。B は成功 | 行待ちは「相手 XID の ShareLock 待ち」。タイマーが先に切れた方が中断 |
| 5b | 両者 `set local deadlock_timeout='5s'` で 5 と同じ | 約 5.0 秒後に A が 40P01 | 検出は deadlock_timeout 経過後 |
| 6 | A: `select ... id=1 for update` / B: `... for update nowait` | `ERROR 55P03 could not obtain lock on row in relation "t"` | NOWAIT |
| 6 | 同上 / B: `select * from t order by id for update skip locked` | id=2,3 だけ返る | SKIP LOCKED |
| 6 | 同上 / B: `set lock_timeout='200ms'; update ... id=1` | `ERROR 55P03 canceling statement due to lock timeout` | 行待ちにも lock_timeout が効く |
| 6 | 同上 / B: `for share nowait` | 55P03（FOR UPDATE と FOR SHARE は衝突） | |
| 6 | 同上 / B: 普通の SELECT | ブロックしない | 行ロックは読み取りを止めない |
| 7 | A・B が同じ行を `for share` | 両方成功。C から `xmax` は `1`（MultiXact ID）。C の `for update nowait` は 55P03 | 共有ロックの複数保持（MultiXact） |
| 7 | A: `for key share` / C: `update set v=7`（非キー列） | **ブロックしない**（UPDATE 1） | KEY SHARE と NO KEY UPDATE は両立 |
| 7 | 同上 / C: `update set id=11`（主キー列） | ブロック。A の rollback 後に成功 | キー列の UPDATE は FOR UPDATE 相当 |
| 7 | A: `for update` / C: `for key share nowait` | 55P03 | |
| 7 | A: 非キー列 `update`（未コミット）/ C: `for key share nowait` | **成功**（行が返る） | 更新者 + KEY SHARE 保持者の同居（更新を含む MultiXact） |
| 7 | A: `delete`（未コミット）/ C: `for key share nowait` | 55P03 | DELETE は FOR UPDATE 相当 |
| 7b | A・B が `for share` → 両方が UPDATE | 先に待ち始めた A が 40P01 | 共有ロックの昇格はデッドロックになる |
| 8 | A: `begin; select count(*) from t` / B: `drop table t`（ブロック）/ C: `select count(*) from t` | **C もブロック**。`pg_locks` は A の AccessShare（granted）、B の AccessExclusive（待ち）、C の AccessShare（待ち）。A の commit 後、B は DROP 成功、C は `ERROR 42P01 relation "t" does not exist` | 待ち行列は FIFO で、後続の弱いロックも先行の強いロック待ちの後ろに並ぶ。待ち明けに名前を解決し直す |
| 8b | A: `begin; drop table u` / B: `select * from u`（ブロック）/ A: commit | B は 42P01 | |
| 8c | A: `begin; alter table u add column w int` / B: insert（ブロック）/ A: rollback | B は `INSERT 0 1` | |
| 8d | A・B が別テーブルを `lock table ... in access exclusive mode` し、互いのテーブルを SELECT | A が 40P01、DETAIL `Process 100 waits for AccessShareLock on relation 16465 of database 5; blocked by process 101. ...` | テーブルロックも同じ検出器 |
| 8e | `lock table t`（ブロック外） | `ERROR 25P01 LOCK TABLE can only be used in transaction blocks` | |
| 8f | A: `begin; select 1 from u` / B: `set lock_timeout='300ms'; truncate u` | `ERROR 55P03 canceling statement due to lock timeout` | TRUNCATE は AccessExclusive |
| 9 | `begin; vacuum t` | `ERROR 25001 VACUUM cannot run inside a transaction block`、RFQ `E` | |
| 9 | Simple Query で `select 1; vacuum t`、`vacuum t; vacuum u` | どちらも 25001（暗黙ブロックとみなされる。`select 1` の結果は返る） | |
| 9 | `vacuum`、`vacuum (verbose) u`、`vacuum full u`、`vacuum analyze u` | `VACUUM`。VERBOSE は NOTICE で `vacuuming "postgres.public.u"` と統計（`tuples: N removed, M remain, K are dead but not yet removable`、`removable cutoff: X` など） | |
| 9 | `vacuum nosuch` | `ERROR 42P01 relation "nosuch" does not exist` | |
| 9b | A: `begin; select * from u` / B: `vacuum u` | ブロックしない。`vacuum full u` はブロック | VACUUM は ShareUpdateExclusive、FULL は AccessExclusive |
| 10 | A: `begin; insert into t values (10,1)` / B: 同じキーで insert（ブロック）/ A: commit | B は `23505 duplicate key value violates unique constraint "t_pkey"`、DETAIL `Key (id)=(10) already exists.` | 一意検査は挿入者の XID の終了を待つ |
| 10 | 同上で A が rollback | B は `INSERT 0 1` | |
| 11 | A: RR で count / B: 50 行 delete → `vacuum (verbose) h` | `0 removed, 100 remain, 50 are dead but not yet removable`。A の commit 後は `50 removed` | 古いスナップショットが horizon を止める |
| 11b | A: RC で `begin; select ...`（文の後はアイドル）/ B: update → vacuum | `1 removed` | RC の文の外ではスナップショットを持たないので止めない |
| 11c | A: `begin; select pg_current_xact_id()`（XID を持つ）/ B: update → vacuum | `0 removed ... 1 are dead but not yet removable` | XID を持つだけで horizon を止める |
| 12 | A: `update set v=100`（未コミット）/ B: RC で `select v ... for update`（ブロック）/ A: commit | B は 100（新しい版）を返す。`... and v=0 for update` なら `SELECT 0` | FOR UPDATE も EPQ で再評価 |
| 13 | A: RR で sum / B: 全行 update / A: sum、insert、commit | sum は変わらず、insert も commit も成功 | 読むだけ・挿入だけの RR はエラーにならない |
| 14 | `begin; select 1; set transaction isolation level repeatable read` | `25001 SET TRANSACTION ISOLATION LEVEL must be called before any query` | M3 調査 #15 と同じ |

---

## 3. ロックマネージャ

### 3.1 PostgreSQL の構成【確認: lockdefs.h, lock.c, lmgr.c, lmgr/README】

- 「重量ロック」（heavyweight lock）を共有メモリのハッシュ表で管理する。ロック対象は `LOCKTAG`（リレーション、タプル、トランザクション ID、仮想トランザクション ID、オブジェクト、ページ、アドバイザリなど）。
- モードは 8 段階（lockdefs.h 36〜45 行）: `AccessShareLock`(1, SELECT) / `RowShareLock`(2, SELECT FOR UPDATE/SHARE) / `RowExclusiveLock`(3, INSERT/UPDATE/DELETE) / `ShareUpdateExclusiveLock`(4, VACUUM・ANALYZE・CREATE INDEX CONCURRENTLY) / `ShareLock`(5, CREATE INDEX) / `ShareRowExclusiveLock`(6) / `ExclusiveLock`(7) / `AccessExclusiveLock`(8, ALTER TABLE・DROP TABLE・VACUUM FULL・LOCK TABLE の既定)。
- 衝突表（lock.c `LockConflicts[]`）:

| 要求 \ 保持 | AS | RS | RX | SUX | S | SRX | X | AX |
|---|---|---|---|---|---|---|---|---|
| AccessShare | | | | | | | | ✕ |
| RowShare | | | | | | | ✕ | ✕ |
| RowExclusive | | | | | ✕ | ✕ | ✕ | ✕ |
| ShareUpdateExclusive | | | | ✕ | ✕ | ✕ | ✕ | ✕ |
| Share | | | ✕ | ✕ | | ✕ | ✕ | ✕ |
| ShareRowExclusive | | | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ |
| Exclusive | | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ |
| AccessExclusive | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ |

- 同じトランザクションが持つロック同士は衝突しない。
- 待ち行列はおおむね FIFO だが、`ProcSleep`（proc.c 1107 行）には「**自分がすでに持っているロックと衝突するモードを待っている先行者がいたら、その前に割り込む**」規則がある（コメント 1090 行付近）。これがないと、ロックの昇格で簡単にデッドロックする。
- **トランザクションの終了待ち**は、各トランザクションが XID を割り当てたときに自分の XID に対する `ExclusiveLock` を取り（`XactLockTableInsert`）、待つ側がその XID に `ShareLock` を要求する（`XactLockTableWait`、lmgr.c 657 行）ことで実現する。取れたらすぐ解放し、`TransactionIdIsInProgress` で本当に終わったかを確かめてループする（ProcArray に載ってからロック表に載るまでの隙間がありうるため）。
- 実機 #5 の DETAIL `waits for ShareLock on transaction 777` はこの仕組みの現れ。`pg_locks` の `locktype = 'transactionid'`。

### 3.2 yuzhu の設計【提案】

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LockTag {
    Relation { db: Oid, rel: Oid },
    Tuple { db: Oid, rel: Oid, block: u32, offset: u16 }, // 行ロックの順番取り（第 5.3 節）
    TransactionId(Xid),                                   // XID の終了待ち
    Object { db: Oid, class: Oid, obj: Oid },             // 将来: CREATE DATABASE の競合など
    // Advisory は M6 以降
}

#[repr(u8)]
pub enum LockMode { AccessShare = 1, RowShare, RowExclusive, ShareUpdateExclusive,
                    Share, ShareRowExclusive, Exclusive, AccessExclusive }

pub struct LockManager {
    inner: Mutex<LockTable>,            // まず 1 本。計測して必要なら 16 分割
}
struct LockTable {
    locks: HashMap<LockTag, LockEntry>,
    procs: Slab<ProcLockState>,          // バックエンドごと: 保持ロック、待っているロック、Condvar
}
struct LockEntry {
    granted: Vec<(BackendId, u16 /* 保持モードのビット集合 */, u32 /* 回数 */)>,
    wait_queue: VecDeque<Waiter>,        // FIFO + ProcSleep の割り込み規則
}
struct Waiter { backend: BackendId, mode: LockMode, wake: Arc<Condvar> }
```

- **バックエンドごとに `Condvar` を 1 つ**持ち、`LockTable` の `Mutex` と組にして待つ。ロックを解放した側が、待ち行列の先頭から「保持者と衝突しない」待ち手を順に付与して `notify_one` する（PostgreSQL の `ProcLockWakeup` 相当）。待つ側は `wait_timeout` で起きて、キャンセル・`lock_timeout`・`statement_timeout`・`deadlock_timeout` を確認する（M3 の書き込みロック待ちと同じ構造。`m3-tx-semantics.md` 第 5.3 節）。
- **XID の終了待ち**: XID を割り当てるとき（M3 で「最初の書き込み」と決めた時点）に `TransactionId(xid)` を `Exclusive` で取る。トランザクション終了時の解放は、**ProcArray から外した後**に行う（PostgreSQL の `CommitTransaction` でも `ProcArrayEndTransaction` の後に `ResourceOwnerRelease` でロックを外す【確認: m3-tx-semantics.md 第 5.2 節の調査】）。こうすれば、待ちから起きた側が取るスナップショットには相手の結果が必ず入る。
- 「取れたらすぐ解放し、ProcArray で終了を確かめる」ループも PostgreSQL どおりに入れる。yuzhu では XID の割り当てとロック表への登録を同じ関数で行うので隙間は小さいが、ProcArray の `Mutex` とロック表の `Mutex` を別にする以上、隙間はゼロではない【推論】。
- ロックの保持はトランザクション終了まで（テーブル・XID・行）。`Tuple` タグだけは取った直後に解放する（第 5.3 節）。
- **ロック表の大きさの上限は設けない**（`HashMap` が伸びるだけ）。PostgreSQL の `max_locks_per_transaction` と `53200 out of shared memory` は再現しない。GUC は受け付けて保存だけする【提案】。
- **fast-path ロック**（PostgreSQL が弱いリレーションロックをバックエンドローカルに持つ最適化）は入れない。必要になったら後で入れる。
- **ラッチ（ページの `RwLock`）を持ったままロックマネージャで待つことは禁止**する。PostgreSQL も `heap_update` 内で必ず `LockBuffer(buffer, BUFFER_LOCK_UNLOCK)` してから待つ【確認: heapam.c 3015〜3018 行】。デバッグビルドでは、スレッドローカルの「保持中のラッチ数」が 0 でないときにロック待ちに入ったら panic させる（`m2-buffer-io.md` のピン追跡と同じ仕組み）【提案】。

### 3.3 観測用ビューと関数【提案】

- `pg_locks`（`locktype`, `database`, `relation`, `page`, `tuple`, `transactionid`, `virtualxid`（NULL）, `pid`, `mode`, `granted`, `fastpath`（常に false）, `waitstart`）をロック表から生成する。実機 #8 のような検証がワイヤ越しでできる。
- `pg_blocking_pids(pid)` と `pg_isolation_test_session_is_blocked(pid, int[])`（M3 で書き込みロック用に作る予定のもの）を、ロック表の待ちグラフから計算するように置き換える。
- `pg_stat_activity.wait_event_type = 'Lock'`、`wait_event` は `relation` / `transactionid` / `tuple`。

工数: **M**（ロック表・待ち・解放・タイムアウト・pg_locks。デッドロック検出は別、第 10 節）。

---

## 4. 行ロックの表現と MultiXact

### 4.1 PostgreSQL の仕組み【確認: README.tuplock, htup_details.h, lockoptions.h】

- 行ロックは**タプルヘッダに書く**（ロック表に置くと数が無制限になるため）。ロックしたトランザクションの XID を `xmax` に入れ、infomask で「削除ではなくロックだけ」と区別する。
- 4 つの強度（`LockTupleMode`）:
  - `LockTupleKeyShare` = `FOR KEY SHARE`（外部キー検査が使う）
  - `LockTupleShare` = `FOR SHARE`
  - `LockTupleNoKeyExclusive` = `FOR NO KEY UPDATE`、および**キー列を変えない UPDATE**
  - `LockTupleExclusive` = `FOR UPDATE`、**キー列を変える UPDATE**、**DELETE**
- 衝突表（README.tuplock）:

| | UPDATE | NO KEY UPDATE | SHARE | KEY SHARE |
|---|---|---|---|---|
| UPDATE | ✕ | ✕ | ✕ | ✕ |
| NO KEY UPDATE | ✕ | ✕ | ✕ | |
| SHARE | ✕ | ✕ | | |
| KEY SHARE | ✕ | | | |

- 「キー列」は、外部キーの参照先になりうる一意インデックス（部分インデックス・式インデックスを除く）の列【記憶: `RelationGetIndexAttrBitmap(INDEX_ATTR_BITMAP_KEY)`。未検証】。実機 #7 で主キー列の UPDATE は KEY SHARE と衝突し、非キー列の UPDATE は衝突しないことを確認した。
- infomask のビット（htup_details.h 194〜282 行）: `HEAP_XMAX_KEYSHR_LOCK 0x0010`、`HEAP_XMAX_EXCL_LOCK 0x0040`、`HEAP_XMAX_LOCK_ONLY 0x0080`、`HEAP_XMAX_SHR_LOCK = EXCL | KEYSHR`（0x0050）、`HEAP_XMAX_IS_MULTI 0x1000`、infomask2 の `HEAP_KEYS_UPDATED 0x2000`。`FOR UPDATE` と `FOR NO KEY UPDATE` はどちらも `EXCL_LOCK` で、`KEYS_UPDATED` の有無で区別する。DELETE とキー列の UPDATE も `KEYS_UPDATED` を立てる。
- **複数のトランザクションが同じ行をロックする**（`FOR SHARE` を 2 人、`FOR KEY SHARE` 中に非キー列 UPDATE など）と、xmax を **MultiXactId** に置き換える。MultiXact は「メンバー XID + 各メンバーの強度（更新者かどうかを含む）」の配列で、`pg_multixact/offsets` と `members` の SLRU に永続化され、WAL にも記録される。更新者を含みうるので、クラッシュ後も読めなければならない（README.tuplock「MultiXacts」）。VACUUM が古い MultiXact を消し、`relminmxid`/`datminmxid` で境界を管理する。
- 実機 #7: `FOR SHARE` を 2 人が持つと、`xmax` は `1`（MultiXactId の 1 番）になった。

### 4.2 yuzhu の選択肢【提案】

| 案 | 内容 | PostgreSQL との差 | 工数 |
|---|---|---|---|
| A. MultiXact なし・共有ロックも排他扱い | `FOR SHARE`/`FOR KEY SHARE` を `FOR UPDATE` と同じ排他ロックとして実装 | 実機 #7 の「2 人で FOR SHARE」がブロックする。外部キーの子 INSERT 同士が親行で直列化される（同じ親を参照する INSERT が並列に走らない） | **S** |
| **B. メモリ上だけのロック専用 MultiXact** | 共有ロックの複数保持は MultiXactId（64 ビット）を xmax に入れ、メンバー表はメモリ（`HashMap<MultiXactId, Vec<(Xid, LockTupleMode)>>`）にだけ置く。**更新者を MultiXact に入れない**（ロック保持者がいる行の UPDATE/DELETE は、保持者全員の終了を待つ） | `FOR KEY SHARE` と非キー列 UPDATE が衝突する（実機 #7 の 2 か所: 非キー UPDATE がブロックする、更新中の行に `FOR KEY SHARE NOWAIT` が 55P03 になる）。外部キーでは「子の INSERT が開いている間、親の非キー列 UPDATE が待つ」。PostgreSQL 9.2 以前と同じ挙動【記憶: 9.3 で FOR KEY SHARE が導入された経緯】 | **M** |
| C. PostgreSQL 同等の永続 MultiXact | offsets/members の永続ストア、WAL レコード、VACUUM による切り詰め、`relminmxid` | 差なし | **L** |
| D. 「xmax は更新者、追加のロック保持者は TID をキーにしたメモリ上の表」 | 更新者は普通の XID として xmax に置き、同居するロック保持者だけを揮発の表に置く。クラッシュ後は保持者がすべて死んでいるので表が消えても正しい | 差なしにできる見込み | **M〜L**。PostgreSQL にない独自方式で、UPDATE で新しい版にロック保持者を引き継ぐ処理（PostgreSQL の `xmax_new_tuple`）なども自前で設計が要る。**未検証の設計** |

**B を推奨する。** 理由:

- ロック専用の MultiXact は、**クラッシュ後にはメンバー全員が終了している**ので、永続化しなくても正しい。再起動後に見つかった `IS_MULTI | LOCK_ONLY` の xmax は「無効（ロックなし）」と扱えばよい。永続化が要るのは更新者を含む場合だけで、B はそれを作らないことで C の大部分（SLRU、WAL、切り詰め）を省ける。
- 差は「待たされる」方向だけで、結果の値は変わらない（デッドロックが増える可能性はある）。共有テストには差の出るケースを入れない（入れるなら M6 以降のディレクトリ）。
- D は魅力的だが独自設計で検証コストが高い。M6 で C か D を選び直す。

B の詳細:

- `MultiXactId` は 64 ビット。**再起動前の ID と区別する**ため、起動時に「この起動で最初に払い出す ID」（`multi_boundary`）を記録し、それ未満の ID は死んでいるとみなす。払い出しカウンタは XID と同じく制御ファイルに「ここまで予約済み」を書いて単調性を保つ（`m2-page-heap.md` の XID の予約方式と同じ）。
- メンバー表からの削除: メンバー全員が終了したエントリは、参照されたときに怠惰に消すか、トランザクション終了時に「自分が入っている MultiXact」の一覧をたどって消す。後者を推奨（表が膨らまない）。
- 単独のロック保持者は普通の XID を xmax に入れる（`LOCK_ONLY` + 強度ビット）。これは PostgreSQL と同じで、永続的でも問題ない（クラッシュ後はアボート扱いの XID になる）。
- MultiXact を作るのは「既存のロック保持者（実行中）と両立するロックを、別のトランザクションが追加する」ときだけ。メンバーの強度は 4 種類のまま記録する（`FOR SHARE` と `FOR KEY SHARE` の区別を保つので、後で C に移るときにメンバーの意味が変わらない）。
- 可視性判定（`satisfies_mvcc`）は `LOCK_ONLY` なら xmax を無視するだけなので、MultiXact の中身を見ない。中身を見るのは `satisfies_update` と VACUUM だけ。

### 4.3 行ロックの WAL【提案】

- PostgreSQL は行ロックを `XLOG_HEAP_LOCK` で WAL に記録する【確認: m3-wal.md の rmgr 表】。yuzhu も予約済みの `HEAP 0x40 LOCK` を使い、「TID、新しい xmax、infomask、infomask2」を記録する（REDO は値を書き込むだけ）。
- 「ロックはクラッシュ後に意味がないので WAL に書かない」案もあるが、ページの内容と WAL がずれると full page write やチェックサム（採用する場合）の前提が崩れるので、PostgreSQL どおり記録する。
- ロックだけをしたトランザクションも XID を持ち、WAL を書いたのでコミット時に fsync する（xact.c 1488 行: `wrote_xlog && synchronous_commit > OFF` で `XLogFlush`）【確認】。M3 の「書き込み文で XID を割り当てる」規則で、`SELECT ... FOR UPDATE` は書き込み文に含めてある（`m3-tx-semantics.md` 第 5.1 節）。

---

## 5. heap_update / heap_delete / heap_lock_tuple の TM_Result 処理

### 5.1 PostgreSQL の流れ【確認: heapam.c heap_delete 2905〜3080 行】

```
l1:
  result = HeapTupleSatisfiesUpdate(tp, cid, buffer)   // バッファは排他ラッチ中
  if result == TM_Invisible: ERROR "attempted to delete invisible tuple"
  if result == TM_BeingModified && wait:
      xwait = 生の xmax; infomask = 現在の infomask      // ラッチを離す前に写す
      if IS_MULTI:
          if DoesMultiXactIdConflict(...):
              ラッチ解放
              if 自分がメンバーでない: heap_acquire_tuplock(LockTupleExclusive)   // 順番取り
              MultiXactIdWait(...)
              ラッチ再取得
              if xmax か infomask が変わった: goto l1
      elif xwait != 自分:
          ラッチ解放
          heap_acquire_tuplock(LockTupleExclusive)      // LockTuple: 公平性
          XactLockTableWait(xwait)                      // 相手の終了待ち
          ラッチ再取得
          if xmax か infomask が変わった: goto l1        // 誰かが先にロック/更新した
          UpdateXmaxHintBits(...)
      if xmax が無効（アボート）か LOCK_ONLY: result = TM_Ok
      elif ctid が自分を指していない: result = TM_Updated
      else: result = TM_Deleted
  if crosscheck && result == TM_Ok && !crosscheck で見える: result = TM_Updated   // RR の外部キー検査用
  if result != TM_Ok:
      tmfd = { ctid, xmax = 更新者 XID, cmax（SelfModified のとき） }
      ラッチとタプルロックを解放して return result
  ... 実際に削除（xmax を書く、WAL）...
```

- `heap_update`（3578 行〜）も同じ構造で、要求する強度が `LockTupleExclusive` か `LockTupleNoKeyExclusive`（キー列を変えるかどうか）になる。
- `heap_lock_tuple`（4847 行〜）はさらに `wait_policy`（`LockWaitBlock` / `LockWaitSkip` / `LockWaitError`）を見る。`LockWaitError` で取れなければ `55P03 could not obtain lock on row in relation "%s"`（5222・5260 行）、`LockWaitSkip` なら `TM_WouldBlock` を返して呼び出し側が行を飛ばす。
- `TM_SelfModified`: 同じトランザクションの後のコマンドが更新済み。実行器が `cmax == 現在の CID` なら黙って無視、そうでなければ `27000 tuple to be updated was already modified by an operation triggered by the current command`（nodeModifyTable.c 2399 行）。

### 5.2 yuzhu の API【提案】

M3 の `TmResult` を少し広げる。

```rust
pub enum TmResult {
    Ok,
    Invisible,
    SelfModified { cmax: CommandId },
    Updated { new_tid: Tid, xmax: Xid },   // 他者がコミット済みで更新した（ctid が別の版を指す）
    Deleted { xmax: Xid },                 // 他者がコミット済みで削除した
    BeingModified { xmax: XmaxInfo },      // wait = false のときだけ返す
    WouldBlock,                            // SKIP LOCKED 用
}

pub enum WaitPolicy { Block, Skip, Error }

impl Heap {
    pub fn delete(&self, tx: &mut Tx, tid: Tid, cid: CommandId, crosscheck: Option<&Snapshot>,
                  wait: bool) -> Result<TmResult, DbError>;
    pub fn update(&self, tx: &mut Tx, tid: Tid, new: &TupleData, cid: CommandId,
                  crosscheck: Option<&Snapshot>, wait: bool) -> Result<(TmResult, LockTupleMode), DbError>;
    pub fn lock_tuple(&self, tx: &mut Tx, tid: Tid, cid: CommandId, mode: LockTupleMode,
                      policy: WaitPolicy, follow_updates: bool) -> Result<(TmResult, Option<TupleData>), DbError>;
}
```

- 待ち（ロックマネージャ経由の XID 待ち）は `heap` 層の中で行う。理由: 「ラッチを離す → 待つ → 取り直して再確認」は xmax の値と密接に結びついており、外に出すと再確認を忘れる。
- 待ちが中断されたら（キャンセル、タイムアウト、デッドロック）`DbError` で返す。そのときタプルロック（`Tuple` タグ）も解放する。
- **xmax == 自分の XID かつ `LOCK_ONLY`**（`SELECT FOR UPDATE` した行を同じトランザクションで UPDATE）は待たずに `Ok`。PostgreSQL も `HeapTupleSatisfiesUpdate` が `TM_BeingModified` を返し、`xwait` が自分なので待たずに進む（heap_delete 3008 行の `else if (!TransactionIdIsCurrentTransactionId(xwait))` に入らず、3044 行付近の `HEAP_XMAX_IS_LOCKED_ONLY` 判定で `TM_Ok`）【確認】。
- ラッチを取り直した後は、**行ポインタ番号でタプルを読み直す**（Rust ではガードを離した時点で参照は消えるので自然にそうなる）。読み直したタプルの `xmin` が元と同じかも確かめる（行ポインタが VACUUM で再利用された場合の防御。ただし自分のスナップショットが horizon を止めているので起きないはず【推論】）。

### 5.3 タプルロック（順番取り）を入れるか【提案】

- README.tuplock によると、XID 待ちだけだと、相手が終了したときに待っていた全員が同時に起きて、誰が行を取るかが競争になり、特定の待ち手が永久に負け続ける（飢餓）。特に共有ロックが続々と来ると排他ロックが永遠に取れない。そこで `LockTuple()`（`Tuple` タグの重量ロック）で順番を決めてから XID を待ち、行に印を付けたら `UnlockTuple()` する。各バックエンドが同時に持つ/待つタプルロックは高々 1 つ。
- 例外: すでに弱いロックを持っていて強いロックへ昇格するときはタプルロックを取らない（取るとデッドロックする、README.tuplock）。
- **yuzhu でも入れる**。第 3 節のロックマネージャがあれば数十行で済み、待ち手の勝ち順（分離性テストの期待出力に現れる）が PostgreSQL と揃う。タプルロック待ちは `pg_locks` で `locktype = 'tuple'` として見える。

### 5.4 キー列の判定【提案】

- `update` は「新旧でキー列が変わったか」で `LockTupleExclusive` / `LockTupleNoKeyExclusive` を選び、`KEYS_UPDATED` を立てるかを決める。キー列 = そのテーブルの一意インデックス（主キー・UNIQUE 制約）の列の和集合。M4 の B+Tree が入っていれば、テーブル定義のキャッシュに「キー列ビットマップ」を持たせる。
- 案 B では `NoKeyExclusive` と `KeyShare` も衝突扱いなので、M5 の時点では区別しなくても挙動は変わらない。ただし `KEYS_UPDATED` は**ディスク上の値として PostgreSQL と同じ意味で書いておく**（M6 で案 C/D に移ったときに既存データの意味が変わらないように）。

工数: **M**（3 関数 + 待ち + タプルロック + WAL の LOCK レコード）。

---

## 6. Read Committed の EvalPlanQual（簡易版）

### 6.1 PostgreSQL の挙動【確認: nodeModifyTable.c 2408〜2502 行、heapam_handler.c 355 行〜、execMain.c 2529 行】

UPDATE で `table_tuple_update` が `TM_Updated` を返したとき（Read Committed）:

1. `table_tuple_lock(..., TUPLE_LOCK_FLAG_FIND_LAST_VERSION)` で **ctid 連鎖をたどって最新版を探し、その版をロックする**。たどる途中で、次の版の `xmin` が前の版の `xmax` と一致するかを確かめる（`priorXmax`）。最新版がまだ更新中なら待つ。
2. ロックできたら `EvalPlanQual()` を呼ぶ。これは計画木の対象テーブルの走査を「この 1 行だけを返す」ものに差し替えて、計画を再実行する。他のテーブル（結合相手）は、`FOR UPDATE` 指定がなければ**元の走査で得た同じ版**を使う（ROW_MARK_REFERENCE / ROW_MARK_COPY）【記憶: executor/README の EvalPlanQual の節。細部は未検証】。
3. 再実行が行を返さなければ（WHERE を満たさなくなった）その行は飛ばす（実機 #2 の `UPDATE 0`）。返したら、その出力で新しい行を作り直し（SET 式の再計算）、`goto redo_act` で更新をやり直す（実機 #1 の 12）。
4. ロックの結果が `TM_Deleted` なら黙って飛ばす（実機 #3）。`TM_SelfModified` なら第 5.1 節と同じ規則。

`SELECT ... FOR UPDATE`（nodeLockRows.c）も同じく、最新版をロックして EPQ で再評価し、通れば**新しい版を返す**（実機 #12）。

### 6.2 yuzhu の簡易版【提案】

実行器の UPDATE/DELETE/LockRows ノードに次のループを持たせる。

```
for each 入力行 r（対象テーブルの tid と値、結合相手の値を含む）:
  loop:
    res = heap.update(tid, new_values(r), ...)       // DELETE なら heap.delete
    match res:
      Ok             => 処理済み。次の入力行へ
      SelfModified   => cmax == 現在の CID なら無視、それ以外は 27000
      Deleted        => RR なら 40001 "concurrent delete"、RC なら飛ばす
      Updated{..}    =>
        RR なら 40001 "concurrent update"
        (lres, latest) = heap.lock_tuple(tid, mode, Block, follow_updates = true)  // 最新版を探してロック
        match lres:
          Ok      => r' = r の対象テーブル部分を latest で置き換えたもの
                     if WHERE(r') が真でない: 飛ばす（次の入力行へ）
                     tid = latest.tid; new_values = SET式(r')    // 再計算
                     continue loop                              // 今度は自分がロック済みなので Ok になる
          Deleted => 飛ばす
          SelfModified => 上と同じ規則
```

- **再評価するのは「元の文の WHERE 句全体」と「SET 式」**。結合相手の列の値は、元の入力行 `r` に入っていたものを固定して使う。これは PostgreSQL の「非ロック対象の結合相手は元の版を使う」と同じ意味になる（第 6.1 節 2）。
- 計画木の部分再実行は行わない。そのため、**WHERE 句に相関サブクエリがある場合は、サブクエリだけを最新版の値で再実行する**（文のスナップショットで）。PostgreSQL の EPQ もサブプランは EPQ 用の状態で再実行する【記憶。未検証】。
- 結合で 1 つの対象行に複数の入力行が当たる場合（`UPDATE ... FROM` の多対一）、2 回目以降は `SelfModified`（cmax == 現在の CID）で無視される。PostgreSQL と同じ。
- **`RETURNING`** は再計算後の新しい行で評価する。

### 6.3 SELECT FOR UPDATE / SHARE【提案】

- LockRows ノードは計画の最上位近く（`LIMIT` の下、`ORDER BY` の上）に置く【記憶: PostgreSQL の planner は LockRows を Limit の下に置く。未検証】。`LIMIT` 付きでは「ロックできて条件を満たした行」が LIMIT 件になるまで続ける。
- M5 では**ロック対象は FROM のベーステーブルすべて**（`FOR UPDATE OF t` の指定は受け付けて対象を絞る）。集約・`DISTINCT`・`GROUP BY`・`UNION` と組み合わせたら PostgreSQL と同じく `0A000 FOR UPDATE is not allowed with aggregate functions` など【記憶: メッセージの正確な文言は未検証】。
- `NOWAIT` → `heap.lock_tuple(..., WaitPolicy::Error)`、`SKIP LOCKED` → `WaitPolicy::Skip` で `WouldBlock` の行を飛ばす（実機 #6）。テーブルロックの取得では `NOWAIT` を見ない（PostgreSQL も行ロックだけ）【記憶】。
- 複数テーブルの結合で一部のテーブルの行が `Updated` だった場合: その行の最新版をロックして WHERE を再評価し、通ればその行の新しい値で出力する。他のテーブルの行は（すでにロック済みなら）そのまま。

### 6.4 PostgreSQL と結果が変わりうる点【提案・未検証】

| ケース | PostgreSQL | yuzhu 簡易版 |
|---|---|---|
| 単一テーブルの UPDATE/DELETE、SELECT FOR UPDATE | EPQ | **同じ** |
| `UPDATE t ... FROM u`、u は FOR UPDATE 対象外 | u は元の版で再評価 | **同じ**（入力行の u の値を固定） |
| WHERE に非相関サブクエリ（InitPlan） | InitPlan の結果は再利用 | 再利用すれば同じ。再実行すると差が出るので**再利用する** |
| WHERE に相関サブクエリ | EPQ の中でサブプランを再実行 | サブクエリを再実行。細部（再実行時に見えるもの）が一致するかは未検証 |
| 結合の両側が同時に更新された（`FOR UPDATE` で両方ロック） | 両方の最新版で再評価 | 1 行ずつ最新版にして再評価。結果は同じになるはずだが未検証 |
| 新しい版が「元の走査では条件に合わなかった行」に変わった | 拾わない（m3-tx-semantics の website の例） | **同じ**（元の走査で拾った行しか再評価しない） |

M3 で「既知の差」として扱った website の例（`m3-tx-semantics.md` 第 4.2 節）は、M5 でこの EPQ を入れると PostgreSQL と一致する（DELETE 0）。共有テストに入れられる。

工数: **M**（UPDATE/DELETE/LockRows の 3 ノード + ctid 連鎖の追跡 + 再評価）。

---

## 7. Repeatable Read

### 7.1 PostgreSQL の挙動【確認: snapmgr.c、nodeModifyTable.c、nodeLockRows.c / 実機 #4〜#4g, #13, #14】

- スナップショットはトランザクションの**最初の文**で 1 回だけ取る（BEGIN ではない、#4e）。
- UPDATE/DELETE/SELECT FOR UPDATE が「スナップショットで見えていた行の、スナップショット後にコミットされた更新・削除」に当たると `40001`。判定は単純で、`TM_Updated`/`TM_Deleted` が返ったら `IsolationUsesXactSnapshot()` なら即エラー（nodeModifyTable.c 2414・2505 行、nodeLockRows.c 225・234 行）。見えていた行の xmax がコミット済みなら、そのコミットは必ずスナップショット後なので、スナップショットとの比較は要らない。
- メッセージ: UPDATE/DELETE で `TM_Updated` → `could not serialize access due to concurrent update`、`TM_Deleted` → `... concurrent delete`。LockRows はどちらも `... concurrent update`。
- 相手が実行中なら待ち、アボートしたら続行、コミットしたら 40001（#4b, #4c）。ロックだけでコミットした相手なら続行（`lock-committed-update` 分離性テスト【確認: isolation_schedule に存在。中身は未読】）。
- 読むだけ・挿入だけでは失敗しない（#4g, #13）。
- 分離レベルは最初の文の前にだけ変更できる（#14）。

### 7.2 yuzhu【提案】

- M3 の決定（`m3-tx-semantics.md` 第 10.4 節）で REPEATABLE READ を 0A000 にしていたのを解除する。`Tx` に `isolation: IsolationLevel` を持たせ、`GetTransactionSnapshot` 相当で RR なら最初の 1 回だけ取って保持する。
- 第 6.2 節のループで `Updated`/`Deleted` が返ったら、RR なら上のメッセージで 40001。エラー後はトランザクションが Failed（RFQ `E`）になるのは通常のエラーと同じ。
- **SERIALIZABLE は M5 でも `0A000`**（黙って RR として動かさない。M3 と同じ方針）。PostgreSQL の SSI（述語ロック、rw 依存の検出）は L 以上の工数で、要件のマイルストーンにない。確認事項に挙げる。
- **カタログの読み取りは RR でもトランザクションのスナップショットを使わない**。第 9.3 節のとおり、カタログは常に最新のカタログ用スナップショットで読む。
- VACUUM の horizon を正しく止めるため、RR のスナップショットの xmin はトランザクション終了まで `BackendSlot.snapshot_xmin` に残す（M3 の設計どおり。実機 #11）。

工数: **S**。

---

## 8. 待ちの中断と観測

- 行ロック待ち・テーブルロック待ち・XID 待ちのすべてで、M3 の書き込みロック待ちと同じ中断条件を確認する（`m3-tx-semantics.md` 第 5.3 節）: CancelRequest → `57014 canceling statement due to user request`、`lock_timeout` → `55P03 canceling statement due to lock timeout`（実機 #6, #8f）、`statement_timeout` → `57014 canceling statement due to statement timeout`、停止要求 → `57P01`。
- `lock_timeout` は「1 回のロック待ちの時間」、`statement_timeout` は文全体【記憶: ドキュメントの記述。未検証】。
- `idle_in_transaction_session_timeout` は M3 で実装済みの想定。M5 では行ロックを持ったままのアイドルが他者を止める問題が増えるので、重要度が上がる。

---

## 9. テーブルロック

### 9.1 文ごとのロックモード【確認: lockdefs.h のコメント、ドキュメント 13.3.1 / 実機 #8〜#9b】

| 文 | モード | 備考 |
|---|---|---|
| SELECT（参照する全テーブル） | AccessShare | |
| SELECT FOR UPDATE/SHARE（対象テーブル） | RowShare | |
| INSERT / UPDATE / DELETE / MERGE（対象テーブル） | RowExclusive | 参照だけのテーブルは AccessShare |
| VACUUM（FULL なし）、ANALYZE | ShareUpdateExclusive | DML を止めない（#9b） |
| CREATE INDEX | Share | DML を止める。CONCURRENTLY は対象外 |
| CREATE TRIGGER、一部の ALTER TABLE | ShareRowExclusive | M5 では該当なしでよい |
| DROP TABLE、TRUNCATE、VACUUM FULL、ALTER TABLE（ADD COLUMN など大半） | AccessExclusive | |
| ALTER TABLE ADD FOREIGN KEY | 参照元・参照先とも ShareRowExclusive【記憶。未検証】 | M5 の FOREIGN KEY で確認 |
| LOCK TABLE（モード省略） | AccessExclusive | トランザクションブロック外では `25P01`（#8e） |

ロックはすべてトランザクション終了まで保持する。

### 9.2 待ち行列の性質【実機 #8】

- 待ち行列は FIFO。AccessShare（A が保持）→ AccessExclusive（B が待ち）→ AccessShare（C）の順だと、C は A と両立するのに B の後ろで待たされる。これを実装しないと、読み取りが続く限り DDL が永遠に取れない。
- 待ち明けの C は、名前を解決し直して `42P01` になる（DROP がコミットされたため）。

### 9.3 名前解決とロックの順序【提案。PostgreSQL の `RangeVarGetRelidExtended` に倣う】

PostgreSQL は「名前 → OID を解決 → OID でロック → 無効化メッセージを処理 → もう一度解決して OID が同じなら確定、違えばロックを外してやり直し」というループで、待っている間に DROP や RENAME が起きても正しい OID をロックする【記憶: namespace.c `RangeVarGetRelidExtended`。未検証】。

yuzhu では:

1. 意味解析の最初に、文が参照するリレーションの名前を**最新のカタログ用スナップショット**で解決する。
2. 文の種類に応じたモードで `Relation{db, oid}` をロックする（待つことがある）。
3. ロックを取れたら、カタログ用スナップショットを取り直して名前をもう一度解決する。OID が同じなら確定。違えば（待っている間に DROP/RENAME された）ロックを外して 1 に戻る。見つからなければ `42P01`。
4. すべてのロックを取ってから、**実行用のスナップショット**を取る（RC では文ごと、RR では最初の文だけ）。

- **カタログ用スナップショット**: カタログのヒープを読むときは、RC・RR にかかわらず「その時点の最新のスナップショット」を使う。PostgreSQL も `GetCatalogSnapshot` で同じことをしている【記憶: snapmgr.c。未検証】。カタログキャッシュの無効化（M3 で決めた「カタログ版数」）は、ロックを取った後に確認する。
- 複数のテーブルを参照する文では、ロックを取る順番は FROM 句に出てくる順（PostgreSQL も特に並べ替えない【記憶】）。順番の違いによるデッドロックは第 10 節の検出器で拾う。
- M3 の「スナップショット取得前にグローバル書き込みロックを取る」規則は、この「ロック → スナップショット」の順序に置き換わる。

### 9.4 M3 の仕組みからの置き換え【提案】

- **グローバル書き込みロックを廃止**する。書き込み文は対象テーブルに RowExclusive、DDL は AccessExclusive を取る。
- M3 の「DROP したファイルの削除を、古いスナップショットがなくなるまで遅らせる」仕組み（`m3-tx-semantics.md` 第 6.3 節）は、AccessExclusive により不要になる。DROP のトランザクションはコミットまで AccessExclusive を持つので、そのテーブルを読んでいる文はない。PostgreSQL と同じく「コミット時（ロック解放前）に unlink」にできる。ただし、RR のトランザクションが「DROP 前のスナップショット」で後からそのテーブルを読もうとしても、第 9.3 節によりカタログは最新で読むので `42P01` になり、ファイルには触れない。**遅延削除の仕組みは安全装置として残してもよい**（残すコストは小さい）。
- TRUNCATE は AccessExclusive の下で新しい relfilenode に付け替える（PostgreSQL と同じ。MVCC 的には安全でない操作であることもドキュメントどおり）。

### 9.5 DDL とカタログ行の同時更新【記憶・未検証】

- 2 つのセッションが同時に同じ名前で CREATE TABLE すると、PostgreSQL ではカタログの一意インデックスで `23505 duplicate key value violates unique constraint "pg_class_relname_nsp_index"` になることがある。yuzhu のカタログにも一意インデックス（M4 の B+Tree）があれば同じになる。なければ、名前の重複検査の前に `Object` タグ（名前空間 + 名前のハッシュ）をロックする簡易策がある。
- 異なるテーブルへの DDL は同じカタログ行を更新しないので、テーブルロックだけで足りる。同じ行を更新する稀なケース（`pg_database` の更新など）は PostgreSQL では `tuple concurrently updated`（XX000）になる。yuzhu ではカタログの更新にも `heap.update` を使い、`Updated` が返ったら同じ XX000 にする。

工数: **M**（ロック取得の差し込み、名前解決のループ、LOCK TABLE、グローバル書き込みロックの撤去）。

---

## 10. デッドロック検出

### 10.1 PostgreSQL の挙動【確認: proc.c, deadlock.c, guc_tables.c / 実機 #5, #5b, #7b, #8d】

- ロック待ちに入ったバックエンドは、`deadlock_timeout`（既定 1000ms、PGC_SUSET）のタイマーを仕掛けて眠る。タイマーが切れたら `CheckDeadLock` で**全パーティションのロックを取って**待ちグラフを調べる（`DeadLockCheck`、deadlock.c 217 行）。
- 待ちグラフの辺は 2 種類: **hard edge**（待っているモードと、保持されているモードが衝突）と **soft edge**（待ち行列で自分より前にいる、衝突するモードの待ち手）。soft edge だけを含む循環は、待ち行列の並べ替え（`TopoSort`）で解消を試み、解消できれば並べ替えて待ちを続ける。hard edge を含む循環、または並べ替えで解消できない循環は「デッドロック」。
- デッドロックなら、**検出した本人**（タイマーが切れたバックエンド）が `40P01 deadlock detected` で中断される。被害者の選択（コスト最小など）はしない。実機 #5 では先に待ち始めた A がエラーになった（A のタイマーが先に切れた）。
- DETAIL（deadlock.c 1104 行）: 循環の各辺について `Process %d waits for %s on %s; blocked by process %d.` を改行区切りで並べる。`%s on %s` は `ShareLock on transaction 777`、`AccessShareLock on relation 16465 of database 5` など。HINT `See server log for query details.`。サーバログには各プロセスの問い合わせ文が出る。

### 10.2 yuzhu【提案】

- 待ちに入ったら、`deadlock_timeout` 経過後に 1 回だけ検査する（PostgreSQL も 1 回【記憶。未検証】）。検査は `LockTable` の `Mutex` の中で行う（ロック表が 1 本なので「全パーティションのロック」は自動的に満たされる）。
- グラフ: 各待ち手から、(a) 衝突するモードを保持しているバックエンド（hard）、(b) 待ち行列で自分より前にいて衝突するモードを待っているバックエンド（soft）へ辺を張る。自分から出発して自分に戻る循環があるかを DFS で調べる。
- **soft edge の並べ替え（TopoSort）は実装しない**。代わりに、待ち行列に入るときに `ProcSleep` の割り込み規則（「自分がすでに持っているロックと衝突するモードを待っている先行者の前に入る」）を入れる。これで「ロックの昇格」による典型的な soft deadlock は待ち行列に入る時点で避けられる。それでも残った循環は（soft edge だけでも）デッドロックとして 40P01 にする。PostgreSQL なら並べ替えで救われるケースで、yuzhu だけ 40P01 になる可能性があるが、頻度は低いと見込む【推論。`deadlock-soft`、`deadlock-soft-2` の分離性テストで差を確認する】。
- 被害者は検出した本人。DETAIL は PostgreSQL と同じ書式で、pid には yuzhu のバックエンド ID（`BackendKeyData` で返したもの）を使う。テストでは pid が毎回違うので、DETAIL は比較対象にしない（sqllogictest はエラーのメッセージ本文を見る）。
- `deadlock_timeout` は SET 可能（PostgreSQL は superuser のみ。yuzhu M5 には権限がないので誰でも可）。

工数: **M**（グラフ構築・DFS・割り込み規則・DETAIL）。並べ替えまで入れるなら +M。

---

## 11. 複数ライター化で同時に必要になるもの（行ロック以外）

M5 の同時実行は行ロックだけでは成り立たない。各層の対応を列挙する。詳細設計は各層の調査で行う。

| 層 | M3 まで（単一ライター） | M5 で必要なこと | 工数 |
|---|---|---|---|
| ヒープへの挿入先の選択 | 末尾ページ + メモリ上のヒント | 複数ライターが同じページに挿入する。ページの書き込みラッチで直列化されるので正しさは保たれる。拡張（新ページ追加）は `RelFork.extension` のロックで直列化済み（`m2-buffer-io.md`）。FSM は VACUUM と一緒に入れる | S |
| WAL 挿入 | `Mutex` 1 本 | そのままで正しい（`m3-wal.md`）。計測後に分割 | 0 |
| clog | 単一ライター | コミット状態の書き込みは XID ごとに 2 ビット。ページ単位の `Mutex` で足りる | S |
| ProcArray / XID 割り当て | `Mutex` 1 本 | そのままで正しい（`m3-mvcc.md` 第 4.5 節） | 0 |
| B+Tree（M4） | 単一ライター前提で作った場合 | **複数ライターの同時挿入・分割に対応が要る**（Lehman-Yao の右リンク、またはラッチカップリング）。一意検査で、衝突する挿入者が実行中なら XID の終了を待って再検査（実機 #10）。**本調査の範囲外で、別の調査が要る** | L |
| シーケンス（M4） | 書き込みロックを取らない設計 | そのまま | 0 |
| チェックポイント | ファジー | そのまま | 0 |

---

## 12. VACUUM

### 12.1 PostgreSQL の構成【確認: vacuumlazy.c 816 行のコメント、pruneheap.c、procarray.c / 実機 #9〜#11c】

- **3 段階**（lazy_scan_heap のコメント）:
  1. ヒープを走査し、各ページを **pruning**（死んだタプルの領域を回収し、行ポインタを `LP_DEAD` にする。HOT 連鎖は `LP_REDIRECT` に）と **凍結**を行い、`LP_DEAD` の TID を集める（`TidStore`、上限 `maintenance_work_mem`）。
  2. **インデックス**から、集めた TID を指すエントリを消す（`lazy_vacuum_all_indexes`、各インデックス AM の `ambulkdelete`）。
  3. **ヒープの 2 回目の走査**で `LP_DEAD` を `LP_UNUSED` にする（`lazy_vacuum_heap_rel`）。「インデックスのエントリが `LP_UNUSED` の行ポインタを指すことは決してない」という不変条件を守るため、インデックスの削除が終わるまで再利用可能にしてはならない。インデックスのないテーブルは 1 回の走査で済む。
  - 最後に、末尾の空ページをファイルから切り詰める（`lazy_truncate_heap`。AccessExclusive を条件付きで取れたときだけ）。FSM と VM を更新する。
- **horizon**（どの XID より古い削除なら消してよいか）: `ComputeXidHorizons`（procarray.c 1735 行）で、全バックエンドの「XID」と「スナップショットの xmin」の最小値。実機 #11（RR のスナップショット）と #11c（XID だけ持っている）は止め、#11b（RC で文の外、XID なし）は止めない。
- **タプルの判定**（`HeapTupleSatisfiesVacuum`）【記憶: 詳細は未検証】: `DEAD`（xmin がアボート、または xmax がコミット済みの更新/削除で horizon より古い）、`RECENTLY_DEAD`（xmax はコミット済みだが horizon 以降。VERBOSE の「dead but not yet removable」）、`LIVE`、`INSERT_IN_PROGRESS`、`DELETE_IN_PROGRESS`。
- **機会的 pruning**（`heap_page_prune_opt`、pruneheap.c 193 行）: 通常の読み取りでページを開いたとき、`pd_prune_xid` が horizon より古く、かつ空き領域が `max(fillfactor の目標, BLCKSZ/10)` 未満なら、**cleanup lock を条件付きで**取れたときだけ pruning する。VACUUM を待たずにページ内の領域を回収できる（行ポインタは `LP_DEAD` として残る）。
- **ロック**: テーブルに ShareUpdateExclusive（DML と両立、VACUUM 同士と DDL は衝突。#9b）。ページの pruning には **cleanup lock**（排他ラッチ + 自分以外のピンがない）が要る。タプルを移動（デフラグ）するので、他者がピンだけ持ってタプルへのポインタを保持している可能性を排除するため。
- **トランザクションブロック内では実行不可**（`25001`、#9）。VACUUM はテーブルごとに独立したトランザクションで処理する【記憶】。
- **WAL**: pruning・凍結・行ポインタの解放はそれぞれ WAL に記録される（`XLOG_HEAP2_PRUNE_*` 系【記憶: 17 で prune/freeze のレコードが統合された。未検証】）。

### 12.2 yuzhu の VACUUM（M5）【提案】

```
VACUUM [ ( VERBOSE ) ] [ table [, ...] ]     -- 引数なしは現在の DB の全テーブル（カタログを含む）
```

1. トランザクションブロック内（暗黙ブロックを含む）なら `25001 VACUUM cannot run inside a transaction block`。
2. テーブルごとに: 短いトランザクションを開始し、ShareUpdateExclusive を取る（`VACUUM` が他の VACUUM と並ばないことも、この衝突で保証される）。
3. `horizon = ProcArray.oldest_xmin()`（全バックエンドの `xid` と `snapshot_xmin` の最小値。VACUUM 自身は XID を持たないのでこれに含まれない）。
4. **第 1 段**: 全ページを走査。各ページで書き込みラッチを取り、タプルを判定:
   - `DEAD` → 領域を回収し、行ポインタを `LP_DEAD` にする（インデックスがなければ直接 `LP_UNUSED`）。TID を集める。
   - `LIVE` などで xmin/xmax が確定しているもの → ヒントビットを立てる。xid < `freeze_cutoff`（第 12.6 節）なら凍結する。
   - ロック専用の xmax で保持者が全員終了しているもの → xmax を無効化（`XMAX_INVALID`）。
   - ページのデフラグ（タプルを詰めて空き領域を連続させる）。
   - この変更をまとめて 1 つの WAL レコード（`HEAP2 PRUNE` 相当）にする。
5. **第 2 段**: インデックスごとに「TID の集合に含まれるエントリを削除」（M4 の B+Tree に `bulk_delete(&TidSet)` を足す）。
6. **第 3 段**: 集めた TID の行ポインタを `LP_DEAD` → `LP_UNUSED`（WAL に記録）。ページの空き容量を FSM に記録。
7. 末尾の空ページの切り詰めは、AccessExclusive を `try`（待たない）で取れたときだけ行う。M5 では省略してもよい。
8. `pg_class.relpages`、`reltuples` を更新（ANALYZE がない間の統計代わり）。`relfrozenxid` 相当を更新（第 12.6 節）。
9. `VERBOSE` なら PostgreSQL に似た NOTICE を出す（`vacuuming "db.schema.table"`、`tuples: N removed, M remain, K are dead but not yet removable`、`removable cutoff: X`）。行の並びは一致させなくてよい（テストで比較しない）。

- **TID の集合**: M5 では `BTreeSet<Tid>` か、ページ番号ごとのビットマップを `Vec` で持つ。上限（`maintenance_work_mem` 相当）を超えたら第 2・3 段を一度実行して空にする。PostgreSQL と同じ構造なので、後で最適化できる。
- **cleanup lock は要らない見込み**【推論】: yuzhu のバッファプールは「ピン（'static）」と「ラッチ（借用ガード）」を型で分けており、ラッチを離した後にページ内のタプルへの参照を持ち続けることが型で禁止されている（タプルはコピーして取り出す）。したがってデフラグは書き込みラッチだけで安全。ただし「行ポインタ番号を覚えておいて後で読み直す」コードは、読み直した行ポインタが `LP_UNUSED` や別のタプルになっていないかを確認すること（第 5.2 節）。実装時に `m2-buffer-io.md` の `cleanup_waiter` を使うかを最終判断する。
- **VM（visibility map）は M5 では作らない**。VACUUM は毎回全ページを走査する（遅いが正しい）。VM を入れると、INSERT/UPDATE/DELETE のたびに `PD_ALL_VISIBLE` を WAL 付きで落とす必要が生じ、正しさのリスクが大きい。index-only scan もないので、得るものが少ない。
- **VACUUM FULL**: 新しい relfilenode に生きているタプルだけをコピーし、インデックスを作り直す（AccessExclusive）。M5 では**後回し**にして `0A000` を推奨（確認事項）。`VACUUM ANALYZE` は ANALYZE 部分を「`reltuples` の更新だけ」とするか、統計なしで成功させる（確認事項）。

### 12.3 機会的 pruning（M5 で入れる）【提案】

- 読み取り走査でページを開いたとき、`pd_prune_xid`（そのページで最も古い「削除/更新した XID」のヒント。M2 で 0 のまま予約済み）が horizon より古く、空き領域が少ないなら、書き込みラッチを `try_write` で取れたときだけ pruning（第 12.2 節 4 の DEAD 処理 + デフラグ）を行う。取れなければ諦める。
- `pd_prune_xid` は `heap.delete`/`heap.update` が xmax を書くときに `min(現在値, 自分の XID)` で更新する（PostgreSQL の `PageSetPrunable`）【記憶】。
- 効果: VACUUM を実行しなくても、UPDATE を繰り返すテーブルのページが無限に膨らまない（行ポインタは `LP_DEAD` として残るので、行ポインタ配列は VACUUM まで回収されない）。
- HOT がなくても有効（PostgreSQL 14 以降、HOT 以外の死んだタプルも pruning で `LP_DEAD` にする【記憶。未検証】）。

### 12.4 autovacuum【提案】

| 案 | 内容 | 工数 |
|---|---|---|
| A. 手動のみ（M5） | `VACUUM` 文だけ。テストや運用で明示的に実行 | 0 |
| **B. 簡易 autovacuum（M5 後半か M6）** | バックグラウンドスレッド 1 本が `autovacuum_naptime`（既定 60s）ごとに、テーブルごとの「死んだタプル数の推定」（DELETE/UPDATE の件数をメモリ上で数える）が `autovacuum_vacuum_threshold + autovacuum_vacuum_scale_factor × reltuples`（既定 50 + 0.2 × reltuples）を超えたテーブルを VACUUM する。統計は再起動で消えてよい | **S〜M** |
| C. PostgreSQL 同等 | ランチャー + 複数ワーカー、コストベースの遅延、統計の永続化、anti-wraparound | L |

**M5 は A + 機会的 pruning、B は M5 の終盤（時間があれば）か M6** を推奨。64 ビット XID なので anti-wraparound の強制 VACUUM は不要で、autovacuum がなくてもデータが失われることはない（膨らむだけ）。sqllogictest の結果は autovacuum の有無で変わらない（変わるのは `ctid` や `pg_stat_*` を見るテストだけ）。

### 12.5 HeapTupleSatisfiesVacuum の yuzhu 版【提案】

```
fn satisfies_vacuum(t, horizon) -> VacResult:
  xmin の状態（ヒント → clog の順）:
    アボート → Dead
    実行中   → InsertInProgress
    コミット済み → 続く
  xmax が無効 or アボート → Live
  LOCK_ONLY:
    単独 XID: 実行中なら Live（ロック中）、終了済みなら Live（xmax を消してよい印を付ける）
    MultiXact: メンバーに実行中がいれば Live、いなければ Live（xmax を消してよい）
  更新/削除の xmax:
    実行中 → DeleteInProgress
    コミット済み:
      xmax < horizon → Dead
      それ以外       → RecentlyDead
```

### 12.6 64 ビット XID での clog の切り詰めと凍結【提案】

64 ビット XID なので**周回は起きない**が、clog（XID ごとに 2 ビット）は XID とともに伸び続ける（1 億 XID で約 25MB、`m3-mvcc.md` 第 5.2 節）。clog の古い部分を消すには、「その範囲の XID の状態を、もう誰も clog に問い合わせない」ことを保証する必要がある。

- **ヒントビットは WAL に記録されない**（`m3-mvcc.md` 第 6 節）。VACUUM がヒントを立ててからクラッシュすると、ページはヒントのない古い版に戻りうる。その後で clog を消していたら、状態を判定できなくなる。
- したがって clog を消す前提として、VACUUM は対象の XID 範囲のタプルについて、**WAL に記録する形で状態を確定させる**必要がある。これが PostgreSQL の「凍結」に相当する。

yuzhu の凍結:

- `freeze_cutoff` = horizon（または `horizon - vacuum_freeze_min_age` 相当。M5 では horizon そのものを推奨。ページを何度も書かずに済ませる最適化は後回し）。
- `xmin < freeze_cutoff` でコミット済みのタプルに `HEAP_XMIN_FROZEN`（= `XMIN_COMMITTED | XMIN_INVALID`、PostgreSQL と同じビット組み合わせ）を立てる。xmin の値そのものは残す（`xmin` システム列や調査に使える）。
- `xmax` がアボート済み → `XMAX_INVALID` を立てる（値は 0 にしてもよい）。ロック専用で保持者が全員終了 → 無効化。コミット済みの更新/削除で horizon より古い → そもそも DEAD として回収済み。
- これらの変更を **WAL に記録する**（`HEAP2 FREEZE` 相当。PRUNE と同じレコードにまとめてよい）。
- 可視性判定で `XMIN_FROZEN` を見たら clog を引かずに「コミット済み・全員に見える」とする。

境界の管理:

- テーブルごとに「このテーブルには `relfrozenxid` 未満の未凍結 XID は残っていない」値を持つ。VACUUM がテーブル全体を走査し終えたら `freeze_cutoff` に進める。
- **カタログ上の置き場所**: PostgreSQL の `pg_class.relfrozenxid` は `xid` 型（32 ビット）。yuzhu は 64 ビットが必要なので、選択肢は (a) `pg_class.relfrozenxid` には下位 32 ビットを入れ（互換のため）、64 ビット値は yuzhu 独自のカタログ（例 `yz_relxid(relid oid, relfrozenxid8 xid8)`）に持つ、(b) `pg_class.relfrozenxid` の型を `xid8` にする（PostgreSQL と型が違う）、(c) 制御ファイルにテーブルごとの表を持つ。**(a) を推奨**（`SELECT * FROM pg_class` の型が PostgreSQL と同じ）。データベースごとの最小値（`pg_database.datfrozenxid` 相当）も同様。確認事項に挙げる。
- **clog の切り詰め**: 全データベースの全テーブルの `relfrozenxid8` の最小値 `oldest` を求め、`oldest` より前の clog セグメント（ファイル単位）を消す。手順は「制御ファイルの `oldest_xid`（`m3-recovery.md` で予約済み）を更新 → `CLOG_TRUNCATE` を WAL に記録して fsync → ファイルを消す」。可視性判定で `xid < oldest_xid` かつヒントなしのタプルに出会ったら、不変条件違反として `XX001`（data_corrupted）にする。
- 他のデータベースのテーブルも境界に関わるので、clog の切り詰めは「全データベースで VACUUM が一巡した後」にしか進まない。M5 の CREATE DATABASE と合わせて、`datfrozenxid` の最小値で判定する。
- `template0` のように接続できないデータベースの扱い（PostgreSQL は `datallowconn = false` のデータベースを initdb 時に凍結済みにしておく）【記憶】は CREATE DATABASE の調査で決める。

工数: **M**（凍結の WAL、境界のカタログ、clog の切り詰め、判定の追加）。

---

## 13. HOT（Heap-Only Tuples）

- PostgreSQL の HOT【確認: README.HOT 冒頭 / 記憶: 詳細】: UPDATE でインデックス列が変わらず、新しい版が**同じページに入る**場合、インデックスに新しいエントリを入れず、旧版から新版へ ctid 連鎖で辿らせる（旧版に `HEAP_HOT_UPDATED`、新版に `HEAP_ONLY_TUPLE`）。pruning で連鎖の先頭の行ポインタを `LP_REDIRECT` にして、途中の死んだ版を回収する。インデックスの肥大と VACUUM のインデックス処理を大きく減らす。
- **M5 では入れない**ことを推奨【提案】。理由: (1) インデックス走査が「HOT 連鎖をたどって、スナップショットで見える版を探す」処理を要し、M4 の B+Tree とヒープ取得の境界に手が入る、(2) pruning で `LP_REDIRECT` を扱う必要があり、VACUUM と機会的 pruning が複雑になる、(3) M5 は複数ライター・行ロック・VACUUM だけで十分に大きい。
- 後から足せるように予約済み: `LP_REDIRECT`（行ポインタの状態）、infomask2 の `HEAP_HOT_UPDATED 0x4000` / `HEAP_ONLY_TUPLE 0x8000`、`pd_prune_xid`、WAL の `HEAP 0x30 HOT_UPDATE`（`m3-wal.md`）。
- 工数（M6 以降）: **L**。

---

## 14. M3 から M5 への移行手順と工数

依存の順に並べる。

| 順 | 作業 | 工数 | 依存 |
|---|---|---|---|
| 1 | ロックマネージャ（LockTag、8 モード、FIFO + 割り込み規則、Condvar、タイムアウト、pg_locks） | M | なし |
| 2 | XID ロック（割り当て時に Exclusive、終了時に解放）と `xact_lock_table_wait` | S | 1 |
| 3 | テーブルロックの差し込み（名前解決のループ、文ごとのモード、LOCK TABLE）。グローバル書き込みロックの撤去 | M | 1 |
| 4 | デッドロック検出（deadlock_timeout、DFS、DETAIL） | M | 1 |
| 5 | `heap.delete/update/lock_tuple` の待ちループ、タプルロック、WAL の LOCK レコード、行ロックの infomask | M | 2 |
| 6 | ロック専用 MultiXact（メモリ上、案 B） | M | 5 |
| 7 | 実行器: TmResult の分岐、EvalPlanQual 簡易版、LockRows（FOR UPDATE/SHARE、NOWAIT、SKIP LOCKED） | M | 5 |
| 8 | Repeatable Read（スナップショットの保持、40001） | S | 7 |
| 9 | B+Tree の同時更新対応と一意検査の待ち（別調査） | L | 2 |
| 10 | VACUUM（3 段階、FSM、VERBOSE）+ 機会的 pruning | L | 3, 9 |
| 11 | 凍結・relfrozenxid 相当・clog の切り詰め | M | 10 |
| 12 | 簡易 autovacuum（任意） | S〜M | 10 |
| 13 | 分離性テストランナーの拡張と PostgreSQL の spec の移植（第 15 節） | M | 並行 |

合計の目安: **L が 2、M が 8〜9、S が 2〜3**。1 人で直列に進めると 2〜3 か月規模。1〜4 と 5〜8 と 9〜11 はある程度並列化できる。

---

## 15. テスト

### 15.1 分離性テストランナー

M3 で設計した「複数接続を開き、ステップを順に送り、ブロックしたかを `pg_isolation_test_session_is_blocked` で判定する」ランナー（`m3-tx-semantics.md` 第 12 節）を使う。PostgreSQL の isolation tester の spec 形式（`setup` / `session` / `step` / `permutation`）をそのまま読めるようにすると、PostgreSQL の spec を移植でき、期待出力を本物の PostgreSQL で生成し直せる【提案】。

### 15.2 移植候補の PostgreSQL 分離性テスト【確認: isolation_schedule に存在することを確認。中身は未読】

| spec | 内容（名前からの推測を含む） | M5 で期待一致 |
|---|---|---|
| `eval-plan-qual` | EPQ の各種ケース | 単一テーブルのケースは一致。結合・サブクエリは差の確認用 |
| `deadlock-simple`、`deadlock-hard` | 単純・複雑なデッドロック | 一致見込み |
| `deadlock-soft`、`deadlock-soft-2` | 待ち行列の並べ替えで解消されるケース | **差が出る可能性**（第 10.2 節） |
| `tuplelock-conflict` | 4 強度の衝突表 | 案 B では KEY SHARE × NO KEY UPDATE が差 |
| `tuplelock-update`、`tuplelock-upgrade-no-deadlock` | ロックと UPDATE、昇格 | 一部差の可能性 |
| `skip-locked`、`nowait` | SKIP LOCKED / NOWAIT | 一致見込み |
| `lock-update-delete`、`lock-committed-update` | ロック後の UPDATE/DELETE | 一致見込み |
| `read-write-unique` | 一意検査と RR | B+Tree 次第 |
| `fk-deadlock`、`fk-contention` | 外部キーとロック | 案 B では差（FOR KEY SHARE） |
| `multixact-no-deadlock` | MultiXact | 案 B で一致するか要確認 |
| `vacuum-concurrent-drop` | VACUUM と DROP の競合 | 一致見込み |

### 15.3 sqllogictest（単一接続）で書けるもの

- `VACUUM` の構文と 25001（`begin; vacuum t`、`select 1; vacuum t`）、`LOCK TABLE` の 25P01、`SET TRANSACTION ISOLATION LEVEL REPEATABLE READ` の受け付けと `SHOW transaction_isolation`、SERIALIZABLE の扱い（PostgreSQL と差が出るので yuzhu 専用ディレクトリ）。
- `VACUUM` 後もデータが変わらないこと（UPDATE/DELETE を大量に行い VACUUM、結果を比較）。

### 15.4 yuzhu 専用の Rust テスト

- ロックマネージャの性質テスト: ランダムな要求列で、付与されたロックが衝突表に反しないこと、FIFO が守られること、デッドロックがあれば必ず誰かが 40P01 になること。
- 同時実行のストレステスト: 複数スレッドが口座間送金（合計が不変）を RC と RR で繰り返し、40001/40P01 はリトライして、最後に合計が一致することを確認。VACUUM と機会的 pruning を並行に走らせる。
- 障害注入: VACUUM の各段階（pruning の WAL 後、インデックス削除後、LP_UNUSED 化の前後、clog 切り詰めの前後）でクラッシュさせ、再起動後にデータと clog が整合すること。再起動後にロック専用 MultiXact が「無効」と扱われること。

---

## 16. 確認事項（仮決めしたもの）

QUESTIONS.md に転記する候補。

1. **MultiXact**: M5 は「メモリ上だけのロック専用 MultiXact」（案 B）にする。`FOR SHARE` の複数保持はできるが、`FOR KEY SHARE` と非キー列 UPDATE が衝突する（PostgreSQL 9.2 以前と同じ挙動。外部キーの子 INSERT 中に親の非キー列 UPDATE が待たされる）。永続 MultiXact（案 C、工数 L）か独自方式（案 D、M〜L）は M6 以降で選び直す。案 A（共有ロックも排他、工数 S）にする手もある。
2. **SERIALIZABLE**: M5 でも `0A000` のまま（SSI は工数 L 以上で要件外）。PostgreSQL のように「受け付けて RR として動かす」ことはしない。
3. **EvalPlanQual は簡易版**: 計画木の部分再実行はせず、対象行の最新版で WHERE と SET を再評価する。単一テーブルでは PostgreSQL と同じ。結合・相関サブクエリを含む場合の一致は未検証。
4. **デッドロック検出の簡易化**: 待ち行列の並べ替え（soft deadlock の解消）を実装しない。PostgreSQL なら救われる稀なケースで yuzhu だけ 40P01 になる可能性がある（工数 +M で解消可能）。
5. **グローバル書き込みロックの廃止**: M5 で PostgreSQL と同じテーブルロックに置き換える。M3 の DROP の遅延削除は安全装置として残す。
6. **autovacuum**: M5 は手動 VACUUM + 機会的 pruning のみ。簡易 autovacuum（工数 S〜M）は M5 の終盤か M6。
7. **VACUUM FULL**: M5 では `0A000`（工数 M で後から実装可能）。`VACUUM ANALYZE` は ANALYZE 部分を `reltuples` の更新だけにして成功させる。
8. **VM（visibility map）**: M5 では作らない。VACUUM は毎回全ページを走査する。
9. **HOT**: M5 では入れない（M6 以降、工数 L）。
10. **64 ビットの relfrozenxid の置き場所**: `pg_class.relfrozenxid`（`xid` 型）には下位 32 ビットを入れ、64 ビット値は yuzhu 独自のカタログ（例 `yz_relxid`）に持つ。`pg_database.datfrozenxid` も同様。
11. **凍結の cutoff**: `vacuum_freeze_min_age` 相当の猶予を設けず、horizon より古いものはすべて凍結する（ページの書き込みは増えるが単純）。GUC は受け付けて保存だけする。
12. **ロック表の上限なし**: `max_locks_per_transaction` は受け付けるだけで、`53200 out of shared memory` は起きない。
13. **`deadlock_timeout` の権限**: PostgreSQL は superuser のみ SET 可能（PGC_SUSET）だが、M5 には権限の概念がないので誰でも SET できる（GRANT/REVOKE の M6 で揃える）。
14. **B+Tree の同時更新**: 複数ライター化で B+Tree の並行制御（右リンクなど）が必要になる。本調査の範囲外として、別途調査する。

---

## 17. 未検証事項（実装前に確かめること）

- `HeapTupleSatisfiesUpdate` と `HeapTupleSatisfiesVacuum` の全分岐（今回は heap_delete 側の処理と結果の使い方だけを精読した）。
- `heap_update` で旧版にロック保持者がいる場合の `xmax_new_tuple`（新しい版へのロック保持者の引き継ぎ）の正確な規則。案 D を検討するときに必須。
- EvalPlanQual がサブプラン（相関サブクエリ）と InitPlan をどう扱うか（executor/README と execMain.c の `EvalPlanQualStart` 以降）。
- LockRows ノードの計画上の位置（Limit との上下関係）と、`FOR UPDATE` が許されない構文の一覧・メッセージ。
- `RangeVarGetRelidExtended` の再解決ループの正確な手順、`GetCatalogSnapshot` の無効化の契機。
- `deadlock_timeout` 後の検査が 1 回だけか、再検査があるか（`CheckDeadLock` と `ProcSleep` のループ）。
- 「キー列」の正確な定義（部分インデックス・式インデックス・UNIQUE NULLS NOT DISTINCT の扱い）。
- PostgreSQL 17 の pruning/freeze の WAL レコード構成（17 で統合されたという記憶の確認）。
- `lock_timeout` と `statement_timeout` の測り方の違い（1 回の待ちか、文全体か）。
- ALTER TABLE ADD FOREIGN KEY のロックモード（M5 の FOREIGN KEY 調査で確認）。
- 分離性テストの spec（第 15.2 節）の中身。案 B と簡易デッドロック検出でどれが差になるかを、spec を読んで確定させる。

---

## 付録 A. 実機確認の方法

- `docker run -d --name yuzhu-m5conc-pg -e POSTGRES_HOST_AUTH_METHOD=trust -p 127.0.0.1:55499:5432 postgres:17`（PostgreSQL 17.11）。
- Python 3 の標準ライブラリだけで書いた最小のワイヤプロトコルクライアント（StartupMessage → Simple Query → ReadyForQuery までのメッセージを、ErrorResponse の SQLSTATE・DETAIL・HINT 込みで表示）を使った。libpq や psql は使っていない。
- 同時実行は 2〜4 本の接続を開き、ブロックしうる文は別スレッドで受信して、400ms 以内に ReadyForQuery が来なければ「ブロック」と判定した（調査用の簡易判定。第 15 節のランナーでは `pg_isolation_test_session_is_blocked` を使う）。デッドロックの経過時間は送信から応答までを計った。
- スクリプトは作業用ディレクトリに置いたもので、リポジトリには含めていない。
