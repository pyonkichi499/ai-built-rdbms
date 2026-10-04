# yuzhu M3 調査レポート: ユーザーから見えるトランザクションの意味論を PostgreSQL に合わせる

調査日: 2026-10-04
対象: PostgreSQL REL_17_STABLE（実機確認は `postgres:17` イメージの PostgreSQL 17.11）
前提: M3 は「単一ライター + 複数リーダー」「Read Committed のみ」「ヒープ + タプルごとの xmin/xmax」「REDO のみの WAL」「コミットごとに fsync」。複数ライターと行ロックは M5、Repeatable Read も M5、Serializable は対象外。

> 凡例
> - **[実機]**: PostgreSQL 17.11 に生のワイヤプロトコル（Simple Query）で接続して確認した事実。スクリプトは本文末尾の付録 A に要約する。
> - **[ソース]**: REL_17_STABLE のソースまたは公式ドキュメントで確認した事実。
> - **[未検証]**: 記憶や推測に基づく記述。実装前に確認すること。
> - **推奨**: yuzhu 向けの設計判断。`QUESTIONS.md` に記録すべき候補は第 13 節にまとめる（本レポートでは `QUESTIONS.md` 自体は変更しない）。

---

## 0. 主な出典

| 種別 | URL |
|---|---|
| 分離レベルと EvalPlanQual の説明（website の例） | https://www.postgresql.org/docs/17/transaction-iso.html |
| 明示的ロック（ロックモード、デッドロック） | https://www.postgresql.org/docs/17/explicit-locking.html |
| BEGIN / SET TRANSACTION / SAVEPOINT / SET | https://www.postgresql.org/docs/17/sql-begin.html 、 https://www.postgresql.org/docs/17/sql-set-transaction.html 、 https://www.postgresql.org/docs/17/sql-savepoint.html 、 https://www.postgresql.org/docs/17/sql-set.html |
| シーケンス関数（nextval はロールバックされない） | https://www.postgresql.org/docs/17/functions-sequence.html |
| タイムアウト系 GUC | https://www.postgresql.org/docs/17/runtime-config-client.html |
| Simple Query の複数文 | https://www.postgresql.org/docs/17/protocol-flow.html#PROTOCOL-FLOW-MULTI-STATEMENT |
| SQLSTATE 一覧 | https://www.postgresql.org/docs/17/errcodes-appendix.html |
| 待機イベント | https://www.postgresql.org/docs/17/monitoring-stats.html#WAIT-EVENT-TABLE |
| 暗黙トランザクションブロック、トランザクション状態機械 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/xact.c （`TBLOCK_IMPLICIT_INPROGRESS`、`BeginImplicitTransactionBlock`、`EndImplicitTransactionBlock`、`AssignTransactionId`、`PreventInTransactionBlock`） |
| トランザクション機構の README | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/README |
| Simple Query の処理本体 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/tcop/postgres.c （`exec_simple_query`、`IsTransactionExitStmt`、`ReportChangedGUCOptions` の呼び出し） |
| GUC のトランザクション終了処理と報告 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/misc/guc.c （`AtEOXact_GUC`、`ReportChangedGUCOptions`） |
| スナップショットの取得 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/time/snapmgr.c （`GetTransactionSnapshot`、`IsolationUsesXactSnapshot`） |
| 可視性判定 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/heapam_visibility.c （ファイル先頭のコメント、`HeapTupleSatisfiesMVCC`、`XidInMVCCSnapshot`） |
| EvalPlanQual の README | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/README （「EvalPlanQual (READ COMMITTED Update Checking)」節） |
| UPDATE/DELETE からの EvalPlanQual 呼び出し | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeModifyTable.c |
| リレーションファイルの遅延削除 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/catalog/storage.c （`pendingDeletes`、`RelationDropStorage`、`smgrDoPendingDeletes`） |
| シーケンス | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/commands/sequence.c （`SEQ_LOG_VALS`、`nextval_internal`、`GetTopTransactionId` の呼び出し） |
| コミットログの状態値 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/clog.h （`TRANSACTION_STATUS_SUB_COMMITTED` を含む 4 状態） |
| ロックマネージャ README | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/lmgr/README |
| isolationtester | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/test/isolation/README 、 https://github.com/postgres/postgres/blob/REL_17_STABLE/src/test/isolation/isolationtester.c 、 https://github.com/postgres/postgres/blob/REL_17_STABLE/src/test/isolation/specs/eval-plan-qual.spec |
| ブロック判定関数 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/waitfuncs.c （`pg_isolation_test_session_is_blocked`。PG17 で lockfuncs.c からこのファイルへ移っている） |

---

## 1. 結論（推奨の要約）

1. **スナップショットは文ごと**（Read Committed）。Simple Query の複数文も 1 文ごとに新しいスナップショットを取る。同じトランザクションの先行コマンドの変更は CommandId（cmin/cmax）で見せる。
2. **グローバル書き込みロックは「最初の書き込み文の開始時、その文のスナップショットを取る前」に取得し、トランザクション終了まで保持する**。BEGIN では取らない（psql やドライバは読むだけのトランザクションでも BEGIN を送るため）。どの文が書き込みかは**生のパース木の文の種類**で決め、意味解析（カタログ参照）より前に取得する。
3. 書き込みロックを「スナップショットより前」に取るので、**M3 では EvalPlanQual が不要**になる（並行する書き込みが存在しない）。代わりに、PostgreSQL と結果が変わるケース（docs の website の例など）がある。これは直列実行と同じ結果であり、M3 では許容し、M5 で行ロックと EvalPlanQual を入れて PostgreSQL に揃える。
4. **解放の順序**: WAL のコミットレコードを fsync → コミットログを「コミット済み」に → 実行中トランザクション一覧から外す → 書き込みロックを解放。この順を守らないと、次のライターが直前のコミットを見ないスナップショットで更新して更新消失（lost update）が起きる。
5. 二番目のライターは **FIFO で待つ**。待ちは `lock_timeout`（55P03）、`statement_timeout`（57014）、CancelRequest（57014）で中断できるようにする。**ロックが 1 つだけで、ほかに「保持したまま待つ」ものがないので、M3 ではデッドロックは起きない**。M3 ではリレーション単位のロック（テーブルロック）は入れない（入れるとデッドロックが起こりうるため。第 5.4 節）。
6. **トランザクショナル DDL** は、カタログをヒープに置き xmin/xmax で管理することで自然に実現できる。ファイルの作成と削除は PostgreSQL の `pendingDeletes` と同じ考え方で、作成はすぐ、削除はコミット後に遅延させる。yuzhu はテーブルロックを持たないので、さらに「そのファイルを読んでいる可能性のあるスナップショットがなくなるまで」削除を待たせる。
7. **25P02** の状態機械は M1 の設計をそのまま使う。MVCC になると、文の失敗はトランザクション全体を失敗させるだけでよく、**M1 の undo ログ（文単位の巻き戻し）は不要になる**。
8. **セーブポイントは M3 の範囲外**（`0A000`）。ただしコミットログを 2 ビット 4 状態にして「サブコミット済み」を予約し、将来サブトランザクションの xid を入れられる形にしておく。
9. **SET はトランザクショナル**（ROLLBACK や暗黙トランザクションの失敗で元に戻る）。SET LOCAL はトランザクション終了で戻る。**ParameterStatus は ReadyForQuery の直前に、最終的に値が変わった項目だけ送る**（M1 設計書の「CommandComplete の前に送る」は PostgreSQL と違う。第 10 節）。
10. **Simple Query の暗黙トランザクション**: 途中の `BEGIN` は**それまでの文も含めて**明示的ブロックに取り込む（区切りにはならない）。区切りになるのは COMMIT/ROLLBACK。M1 設計書の記述を直す必要がある（第 11 節）。
11. 分離性のテストは 3 層で行う: (a) sqllogictest の `connection` でブロックしない交互実行、(b) PostgreSQL の isolationtester と同じ spec 形式を読む自作の Rust ランナー（ブロック判定は `pg_isolation_test_session_is_blocked` を yuzhu にも実装）、(c) 銀行振替などの不変条件を使ったランダムな負荷テスト。

---

## 2. 実機確認の結果一覧（PostgreSQL 17.11）

すべて Simple Query で 1 行ずつ送信した結果。`RFQ` は ReadyForQuery のトランザクション状態。

| # | 送ったもの | PostgreSQL の応答 | yuzhu への含意 |
|---|---|---|---|
| 1 | `insert(1); select 1/0; insert(2)` | INSERT 0 1 → ERROR 22012 → RFQ I。その後 count=0。3 文目は実行されない | 1 つの Query が 1 つの暗黙トランザクション。エラー以降の文は捨てる |
| 2 | `insert(10); begin; insert(11); commit; insert(12); select 1/0` | 10 と 11 は残り、12 は消える | COMMIT が区切り。BEGIN 前の文も同じトランザクションに入る |
| 3 | `insert(40); begin; insert(41); rollback` | 40 も 41 も消える | **BEGIN は区切りではない**（先行文を取り込む） |
| 4 | `insert(20); commit; insert(21); select 1/0` | COMMIT で `WARNING 25P01 there is no transaction in progress` が出るが、20 はコミットされる。21 は消える | 暗黙ブロック中の COMMIT は警告付きでコミットする |
| 5 | `begin` → `select 1/0` → `select 1` / `show` / `set` → `commit` | 失敗後は RFQ E、`select`・`show`・`set` はすべて 25P02、`commit` はタグ `ROLLBACK` で RFQ I | M1 設計どおり |
| 6 | `begin; select 1/0` → `rollback; select 1` | ROLLBACK の後の SELECT は普通に動く | 失敗状態から脱出した後は同じ Query の残りを実行する |
| 7 | ブロック外の `commit` / `rollback`、ブロック内の `begin` | 25P01 / 25P01 / 25001 の WARNING（NoticeResponse） | M1 設計どおり |
| 8 | ブロック外の `set local work_mem='8MB'` | `WARNING 25P01 SET LOCAL can only be used in transaction blocks`、タグ SET、値は変わらない | M1 設計どおり |
| 9 | `begin; set search_path=foo; rollback` | search_path は元に戻る | **SET はトランザクショナル** |
| 10 | `set application_name='x1'; select 1/0` | 値は元に戻り、**ParameterStatus は一度も送られない** | ParameterStatus は RFQ 直前に最終値の差分だけ送る |
| 11 | `begin; set application_name='inblock'; set local application_name='loc'` → `commit` | 1 回目の RFQ 直前に `application_name=loc` だけ。commit 後の RFQ 直前に `application_name=inblock` | 同上 |
| 12 | ブロック外の `savepoint` / `rollback to` / `release`、複数文中の `savepoint` | すべて `ERROR 25P01 ... can only be used in transaction blocks` | セーブポイントを実装しなくても、このエラーの順序は合わせられる |
| 13 | `begin; savepoint s; select 1/0` → `release savepoint s` | 25P02（失敗状態で RELEASE は許されない） | 失敗状態で許されるのは COMMIT/ROLLBACK/ROLLBACK TO/PREPARE TRANSACTION のみ（[ソース] `IsTransactionExitStmt`） |
| 14 | `select 1; vacuum t` / `begin; vacuum t` | どちらも `ERROR 25001 VACUUM cannot run inside a transaction block` | 複数文の暗黙ブロックも「トランザクションブロック内」扱い |
| 15 | `begin; select 1; set transaction isolation level repeatable read` | `ERROR 25001 SET TRANSACTION ISOLATION LEVEL must be called before any query` | 分離レベルは最初の文（スナップショット取得）より前だけ変更可 |
| 16 | `begin read only; insert ...` | `ERROR 25006 cannot execute INSERT in a read-only transaction` | READ ONLY は安価に実装できる |
| 17 | `begin isolation level repeatable read; commit and chain; show transaction_isolation` | `repeatable read`（特性を引き継ぐ） | AND CHAIN は特性を引き継ぐ |
| 18 | ブロック外の `commit and chain` | `ERROR 25P01 COMMIT AND CHAIN can only be used in transaction blocks`（WARNING ではなく ERROR） | 素の COMMIT と違うので注意 |
| 19 | 同時 UPDATE（第 4 節） | B は待ち、A のコミット後に新しい版で再評価 | 第 4 節 |
| 20 | 同時 INSERT 同士 | **待たない** | M3 の yuzhu は待つ（PostgreSQL との差） |
| 21 | `lock_timeout=200ms` で行ロック待ち | `ERROR 55P03 canceling statement due to lock timeout` | 同じ SQLSTATE とメッセージを返す |
| 22 | `statement_timeout=200ms` で行ロック待ち | `ERROR 57014 canceling statement due to statement timeout` | 同上 |
| 23 | 2 セッションが互いの行を更新 | 片方に `ERROR 40P01 deadlock detected`（RFQ E） | M3 の yuzhu では起きない |
| 24 | ブロック内の CREATE TABLE + INSERT を別セッションから参照 | `ERROR 42P01 relation "ddl1" does not exist`。ROLLBACK 後は作った本人からも消える | トランザクショナル DDL |
| 25 | 読み取りトランザクションがテーブルを読んだ後の DROP TABLE | DROP は読み取り側の終了まで待つ。DROP の後ろに並んだ新しい SELECT も待ち、DROP のコミット後に 42P01 | M3 の yuzhu は待たない（テーブルロックなし） |
| 26 | 同名 CREATE TABLE を同時に | 後発は待ち、先発のコミット後に `ERROR 23505 duplicate key value violates unique constraint "pg_type_typname_nsp_index"` | M3 の yuzhu は書き込みロック待ちの後に 42P07 を返すことになる（差。テストで依存しない） |
| 27 | `begin; select nextval('sq'); rollback` → `select nextval('sq')` | 2 が返る（ロールバックされない） | 第 7 節 |
| 28 | `begin read only; select nextval('sq')` | `ERROR 25006 cannot execute nextval() in a read-only transaction` | M4 で合わせる |
| 29 | 読むだけのトランザクション中の `pg_current_xact_id_if_assigned()` | NULL。UPDATE 後は非 NULL | xid は最初の書き込みで遅延割り当て |
| 30 | `begin; delete from t; rollback` 後の `xmax` | 0 でない（中断した削除者の xid が残る） | 中断した xmax はコミットログで無視する。システム列の値はテストに出さない |
| 31 | 遅延 FK 違反で COMMIT 失敗 | `ERROR 23503 ...`、RFQ I（ロールバック済み） | COMMIT 時のエラーはロールバックして I を返す |
| 32 | `idle_in_transaction_session_timeout=300ms` で放置 | 次の送信で `FATAL 25P03 terminating connection due to idle-in-transaction timeout` と切断 | 書き込みロックを握ったまま放置されたときの救済策として実装する |
| 33 | 書き込み中のトランザクションがある間の SELECT | 待たずに古い版を返す | MVCC の基本 |
| 34 | `SELECT ... FOR UPDATE` 中の UPDATE | 待つ | yuzhu では FOR UPDATE/SHARE も「書き込み文」とみなす |
| 35 | 行ロック待ちのセッションを `pg_cancel_backend` | `ERROR 57014 canceling statement due to user request`、RFQ I | キャンセルの文言 |
| 36 | 行ロック待ち中の `pg_stat_activity` | `wait_event_type = Lock`、`wait_event = transactionid` | yuzhu も同じ値を出す |
| 37 | `pg_isolation_test_session_is_blocked(待ち側, '{持ち主}')`、`pg_blocking_pids(待ち側)` | `true`、`{持ち主}` | isolation ランナー用に実装する |
| 38 | `begin; checkpoint; rollback` | CHECKPOINT は成功する | CHECKPOINT はブロック内でも可 |
| 39 | `begin; rollback to savepoint nope` | `ERROR 3B001 savepoint "nope" does not exist`、RFQ E | 第 9 節 |
| 40 | `drop table t; create table t(id int); begin; insert ...; select 1/0` → `commit` | タグ ROLLBACK。DROP/CREATE も取り消され、元の t（1 行）が残る | BEGIN が先行の DDL も取り込む |
| 41 | `begin isolation level read uncommitted; show transaction_isolation` | `read uncommitted` | 名前は保持し、動作は Read Committed |

---

## 3. Read Committed の文ごとスナップショット

### 3.1 PostgreSQL の挙動

- **[ソース]** `GetTransactionSnapshot()`（snapmgr.c）は、`IsolationUsesXactSnapshot()` が偽（= Read Committed）のとき、呼ばれるたびに新しいスナップショットを取る。Repeatable Read 以上では最初の 1 回だけ取って使い回す。
- **[ソース]** `exec_simple_query`（postgres.c）は、Query 文字列をパースした後、**パース木 1 つごと**にスナップショットを取って解析・計画・実行する。したがって 1 つの Query 内の複数文でも、各文は直前の文の時点までにコミットされた変更を見る。
- **[実機]** 表の #19 前半: A が BEGIN 後に `select v` で 10 を見た後、B が自動コミットで 11 に更新すると、A の次の `select v` は 11 を返す。
- 1 つの文の実行中はスナップショットが固定される（文の途中で他者のコミットが見え始めることはない）。**[ソース]** https://www.postgresql.org/docs/17/transaction-iso.html の Read Committed の節。
- 自分のトランザクションの変更は、**先行するコマンド**のものだけが見える。同じ文が挿入した行は、その文自身からは見えない（UPDATE が自分の更新した版を再び更新し続ける「ハロウィーン問題」を防ぐ）。PostgreSQL はこれを CommandId（`cmin`/`cmax`）と `CommandCounterIncrement` で実現する。**[ソース]** transam/README、heapam_visibility.c。

### 3.2 yuzhu の推奨設計

```rust
pub struct Snapshot {
    pub xmin: Xid,            // これより小さい xid はすべて終了済み
    pub xmax: Xid,            // これ以上の xid は未来（見えない）
    pub xip: Vec<Xid>,        // 取得時点で実行中だった xid（M3 では高々 1 つだが Vec にしておく）
    pub curcid: CommandId,    // 自トランザクションの、このコマンドの番号
}
```

- M3 では実行中の書き込みトランザクションは高々 1 つなので `xip` は空か 1 要素だが、**M5 の複数ライターとサブトランザクション（`subxip`）に備えて一般形のままにする**。
- 可視性の判定順序は PostgreSQL と同じく「**スナップショットの `xip` を先に見て、次にコミットログを見る**」。**[ソース]** heapam_visibility.c 先頭のコメントは、コミットログへの記録が実行中一覧からの削除より先に起きるため、順序を逆にすると競合が起きると説明している（MVCC スナップショットでは `XidInMVCCSnapshot` を先に使う）。
- タプルヘッダには `xmin`、`xmax`、`cmin`、`cmax`（PostgreSQL は 1 フィールドを combo CID で共用するが、yuzhu は**別フィールドで持つ**のを推奨。ヘッダは 4 バイト大きくなるが、combocid.c 相当の仕組みが要らない）、`ctid`（更新後の版へのポインタ）、infomask（ヒントビットと、M5 用の「xmax はロックだけ」ビット）を置く。`ctid` の連鎖は M3 では使い道が少ないが、M5 の EvalPlanQual で「最新版をたどる」ために必須なので M3 から書く。
- 文の開始ごとに `curcid` を増やす。1 つのトランザクション内の文は増加する `CommandId` を持つ。

---

## 4. 同時 UPDATE と EvalPlanQual

### 4.1 PostgreSQL の挙動（Read Committed）

**[ソース]** executor/README の「EvalPlanQual (READ COMMITTED Update Checking)」: UPDATE/DELETE が、実行中または並行してコミットされたトランザクションによって変更されたタプルに出会ったら、その相手の終了を待つ。相手がコミットしたら**変更後の最新版**を取り直し、WHERE 条件を**その版に対して再評価**する。条件を満たせば最新版から新しい行を作って更新し、満たさなければその行を飛ばす。相手がロールバックしたら元の版をそのまま更新する。

**[実機]** 確認した 4 パターン（初期値 `t(id,v) = (1,10),(2,20),(3,30)`）:

| A（先に更新し、未コミット） | B（後から実行） | A の終わり方 | B の結果 |
|---|---|---|---|
| `update t set v=v+1 where id=1`（10→11） | `update t set v=v*10 where id=1 returning v` | COMMIT | 待った後 `120`（**A の結果 11 を基に計算**）、UPDATE 1 |
| `update t set v=0 where id=2` | `update t set v=-1 where v=20` | COMMIT | UPDATE 0（**最新版が WHERE を満たさない**） |
| `delete from t where id=3` | `update t set v=99 where id=3` | COMMIT | UPDATE 0 |
| `update t set v=500 where id=1` | `update t set v=v+1 where id=1 returning v` | ROLLBACK | `121`（元の版 120 を基に計算） |

**[実機]** ドキュメントにある「異常に見える」例: `website(id,hits) = (1,9),(2,10)`、A が `update website set hits=hits+1`（未コミット）、B が `delete from website where hits=10`。A のコミット後、**B は DELETE 0**。B のスナップショットでは hits=10 なのは id=2 だけで、その最新版は 11 になって条件を外れる。id=1 は最新版が 10 になったが、B の元のスキャンで条件に合わなかったので再評価されない。最終状態は `(1,10),(2,11)`。

### 4.2 M3 の yuzhu: 書き込みロックを先に取れば EvalPlanQual は不要

- 書き込み文は**スナップショットを取る前に**グローバル書き込みロックを取る（第 5.1 節）。ロックを取れた時点で、他の書き込みトランザクションはすべて終了しており、新しいスナップショットにはそのコミット結果がすべて含まれる。
- したがって、書き込み文がスキャン中に「実行中の他者が変えたタプル」や「スナップショット後にコミットされた他者の変更」に出会うことはない。**M3 では `heap_update` が `TM_Updated`/`TM_BeingModified` に相当する状態を返すことは理論上ない**。ただし実装には検査を残し、出会ったら内部エラー（`XX000`）にして、不変条件が壊れたことを検出できるようにする。
- 上の 4 パターンでは、M3 の yuzhu は B が文の開始時に待ち、A の終了後に新しいスナップショットで実行するので、**結果は PostgreSQL と同じになる**（120、UPDATE 0、UPDATE 0、121）。
- **結果が変わるケース**: website の例では、yuzhu の B は A のコミット後のスナップショット `(1,10),(2,11)` で実行するので、**id=1 を削除して DELETE 1** になる。これは「A の後に B を直列に実行した」のと同じ結果で、より厳しい（直感的な）結果だが、PostgreSQL とは違う。複数行を対象とし、他者の更新で条件に入ってくる行があるケースは、すべてこの種の差が出る。
- **推奨**: M3 ではこの差を許容し、仕様（`spec/`）に「M3 の既知の差」として明記する。共有テスト（`tests/`）には、この差に依存するケースを入れない（入れる場合は M5 以降のディレクトリに置く）。M5 で行ロック（xmax をロックに使う）と EvalPlanQual を実装したときに PostgreSQL と一致させる。

### 4.3 M5 に向けて M3 で用意しておくもの

- タプルの `ctid` 連鎖（更新時に旧版の `ctid` を新版に向ける）。
- infomask の「xmax はロックだけ」（`HEAP_XMAX_LOCK_ONLY` 相当）、「排他ロック」（`HEAP_XMAX_EXCL_LOCK` 相当）のビット位置を予約する。複数トランザクションによる共有ロック（MultiXact）は M5 以降で検討。**[ソース]** https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/htup_details.h
- 実行器の UPDATE/DELETE ノードを「対象タプルの更新が失敗したら結果を返す」形（`TM_Result` 相当の enum）にしておき、M5 で再評価の分岐を足せるようにする。
- 書き込みロックの待ちを「相手トランザクションの終了を待つ」抽象（PostgreSQL の `XactLockTableWait` 相当）として実装しておくと、M5 で「行の xmax の持ち主の終了を待つ」に流用できる。

---

## 5. グローバル書き込みロック

### 5.1 取得の時点

| 案 | 内容 | 評価 |
|---|---|---|
| A. BEGIN で取る | 明示的トランザクションは開始時点で取る | **不採用**。psql の `AUTOCOMMIT off`、pgJDBC の `setAutoCommit(false)`、psycopg の既定（暗黙に BEGIN を送る）などで、読むだけのトランザクションも書き込みを止めてしまう |
| B. 最初の書き込み文の開始時（スナップショット取得前） | 文の種類で判定し、意味解析より前に取る | **推奨** |
| C. 最初にタプルを変更するとき | スナップショット取得後、ヒープ書き込みの直前 | **不採用**。待っている間に前のライターがコミットすると、古いスナップショットで更新して更新消失が起きる（EvalPlanQual が必要になる） |

案 B の詳細:

- **書き込み文**とみなすもの: INSERT、UPDATE、DELETE、`SELECT ... FOR UPDATE/NO KEY UPDATE/SHARE/KEY SHARE`、すべての DDL（CREATE/DROP/ALTER/TRUNCATE）、将来の COPY FROM、MERGE。M4 の `nextval` は第 7 節のとおり書き込みロックを取らない。
- 判定は**生のパース木の文の種類**で行い、意味解析より前にロックを取る。意味解析はカタログを読むので、解析の後にロックを取ると、待っている間に他者の DDL がコミットされて解析結果が古くなる（PostgreSQL はテーブルロックを解析中に取って、無効化メッセージを処理してから解析し直す仕組みで防いでいる。yuzhu はロックを先に取ることで同じ問題を避ける）。
- 書き込みを含みうる関数呼び出し（`SELECT f()` で f が書き込む）は M3 には存在しない。将来ユーザー定義関数を入れるときは、関数の volatility で判定するか、関数内で初めて書くときに「ロックを取り、取得までに他者がコミットしていたら 40001 で失敗させる」方式を検討する（[未検証] の設計案）。
- **xid の割り当て**も同じ時点にする。PostgreSQL も xid は最初の書き込みで遅延割り当てする（**[実機]** #29、**[ソース]** `AssignTransactionId`）。読むだけのトランザクションは xid を持たず、WAL もコミットレコードも書かず、fsync もしない。
- Simple Query の暗黙ブロック（複数文）でも同じで、最初の書き込み文でロックを取り、Query の終了（暗黙トランザクションの終了）まで保持する。

### 5.2 解放の時点と順序

保持はトランザクション終了まで（文の終了では解放しない。行ロックと同じ寿命）。解放の手順:

1. コミットレコードを WAL に書き、fsync する（`synchronous_commit` 相当は常に on）。
2. コミットログ（pg_xact 相当）にコミット済みと記録する。
3. 実行中トランザクションの一覧（ProcArray 相当）から自分の xid を外す。以後に取られるスナップショットにはコミット済みとして見える。
4. 書き込みロックを解放し、待っている先頭のセッションを起こす。
5. CommandComplete（`COMMIT`）を返す。

3 と 4 の順が逆だと、次のライターがロックを取ってスナップショットを取ったとき、前のトランザクションがまだ「実行中」に見え、その変更を見ずに更新してしまう。ロールバックの場合は 1 の代わりに中断レコード（fsync 不要）を書き、2 で中断済みと記録する。

**[ソース]** PostgreSQL の `CommitTransaction`（xact.c）も同じ順序: `RecordTransactionCommit()`（その中で `XLogFlush` → `TransactionIdCommitTree` でコミットログ更新）→ `ProcArrayEndTransaction()` → `ResourceOwnerRelease(..., RESOURCE_RELEASE_LOCKS, ...)` でロック解放。

### 5.3 二番目のライターの待ち方

- **FIFO の待ち行列**にする（`Mutex<State>` + `Condvar`、整理券番号で順番を守る）。PostgreSQL のロック待ちもおおむね到着順（**[ソース]** lmgr/README の待ち行列の説明。厳密な順序の保証は [未検証]）。
- 待ちは `Condvar::wait_timeout` の短い間隔（例: 50ms）のループで行い、毎回次を確認する:
  - **CancelRequest**（別接続から届く。M1 で `BackendKeyData` を返しているので、M3 で受け付けを実装する）: `ERROR 57014 canceling statement due to user request`（**[実機]** #35。確認は `pg_cancel_backend` で行ったが、CancelRequest も同じ経路（SIGINT）でキャンセルされる）。
  - **`lock_timeout`**（0 は無制限）: `ERROR 55P03 canceling statement due to lock timeout`（**[実機]** #21）。
  - **`statement_timeout`**: `ERROR 57014 canceling statement due to statement timeout`（**[実機]** #22）。文全体の時間で測る。
  - サーバ停止要求: `FATAL 57P01 terminating connection due to administrator command`。
- 待ちが中断されたら、通常の文のエラーと同じ扱い: 明示的ブロック内なら失敗状態（RFQ E）、暗黙トランザクションならロールバック（RFQ I）。
- ロックを持っているセッションが接続を切ったら、そのトランザクションをロールバックしてロックを解放する（接続スレッドの終了処理、`Drop` で確実に行う）。
- ロックを持ったまま `idle in transaction` で放置されると、すべてのライターが止まる。**`idle_in_transaction_session_timeout`** を実装し、`FATAL 25P03 terminating connection due to idle-in-transaction timeout` で切断する（**[実機]** #32）。既定値は PostgreSQL と同じ 0（無効）。

### 5.4 デッドロックは起きないか

M3 で次の条件を守る限り、**デッドロックは起きない**:

1. トランザクションをまたいで「保持したまま他者を待つ」資源は、グローバル書き込みロックの 1 つだけ。
2. 書き込みロックを持つトランザクションは、他のトランザクションの何かを待たない（読み取り側は何も保持しないので待つ対象がない）。
3. バッファのラッチ（ページロック）は短時間だけ持ち、持ったまま書き込みロックやクライアントを待たない。WAL の fsync やチェックポイントは他トランザクションの終了を待たない。

このため M3 では 40P01 は発生せず、`deadlock_timeout` も意味を持たない（SET は受け付けて保存だけする）。

**テーブルロックを入れると壊れる例**: PostgreSQL のように「読むと AccessShareLock、DROP は AccessExclusiveLock」を入れると、

- R: `BEGIN; SELECT * FROM t;`（t の AccessShare を保持）
- W: `BEGIN; INSERT INTO u ...;`（書き込みロックを保持）→ `DROP TABLE t;`（R の AccessShare 待ち）
- R: `INSERT INTO t ...;`（W の書き込みロック待ち）→ **デッドロック**

となる。したがって M3 ではテーブルロックを入れず、DDL と読み取りの衝突は MVCC カタログとファイル削除の遅延で解決する（第 6.3 節）。M5 で行ロックとテーブルロックを入れるときに、待ちグラフによるデッドロック検出（`deadlock_timeout` 後に検出し 40P01）も入れる。

### 5.5 観測性（テストのため）

- `pg_stat_activity` の `wait_event_type`/`wait_event` に、書き込みロック待ちを `Lock` / `transactionid` として出す（**[実機]** #36: PostgreSQL で行の更新待ちはこの組み合わせになる）。
- `pg_isolation_test_session_is_blocked(pid int, interesting_pids int[]) returns bool` を実装する（第 12 節のテストランナーが使う）。**[実機]** #37: PG17 にこのシグネチャで存在し、行ロック待ちで `true` を返す。M3 の意味は「pid が書き込みロック待ちで、そのロックの持ち主が interesting_pids に含まれる」。
- `pg_blocking_pids(int)` も同じ情報から作れるので、余力があれば実装する。

### 5.6 PostgreSQL と見え方が変わる点（M3 の既知の差）

| 状況 | PostgreSQL | M3 の yuzhu |
|---|---|---|
| 別の行への同時 INSERT/UPDATE | 待たない（#20） | 後発は先発のトランザクション終了まで待つ |
| docs の website の例 | DELETE 0 | DELETE 1（直列実行と同じ） |
| 読み取り中のテーブルの DROP | DROP が待つ（#25） | 待たない。読み取り側の次の文は 42P01 |
| 同名 CREATE TABLE の同時実行 | 23505（pg_type の一意索引） | 42P07 `relation "x" already exists` |
| デッドロック | 40P01 | 起きない |

---

## 6. トランザクショナル DDL

### 6.1 PostgreSQL の挙動

- **[実機]** #24: ブロック内の CREATE TABLE は他のセッションから見えず、ROLLBACK で消える。#40: `drop table t; create table t(...); begin; ...` の後に失敗して ROLLBACK すると（BEGIN が先行文を取り込むため）DROP と CREATE ごと取り消され、元の t が中身ごと戻る。
- PostgreSQL のカタログは普通のヒープテーブルで、行に xmin/xmax を持つので、カタログの変更も MVCC で見え方が決まる。**[ソース]** https://www.postgresql.org/docs/17/catalogs-overview.html
- リレーションのファイル: CREATE TABLE は**その場でファイルを作り**、トランザクションが中断したら削除する。DROP TABLE は**コミット時にファイルを削除する**（それまでは残す）。どちらも `pendingDeletes` のリストに積んで、トランザクション終了時に `smgrDoPendingDeletes` で処理する。**[ソース]** storage.c。
- WAL: ファイル作成は専用の WAL レコード（`XLOG_SMGR_CREATE`）、削除はコミットレコードに「削除するリレーションの一覧」を載せ、REDO 時に削除する。**[ソース]** storage.c、xact.c（`xl_xact_commit` の relfilelocator 一覧）。クラッシュで中断したトランザクションが作ったファイルは**孤児として残る**（PostgreSQL の既知の制限。**[未検証]** PG17 時点でも自動掃除はない認識）。

### 6.2 yuzhu の推奨設計

- M2 でカタログをヒープテーブルにしてあるので、M3 で xmin/xmax を付ければトランザクショナル DDL はほぼ自動的に成り立つ。カタログの読み取りも**文のスナップショット**で行う（PostgreSQL はカタログ専用のスナップショットを使うが、M3 の yuzhu は書き込みロックで DDL が直列化されるので、文のスナップショットで十分）。
- カタログキャッシュ（メモリ上の TableDef など）は「カタログ版数」をコミットのたびに増やし、文の開始時に版数を比べて古ければ捨てる。DDL を実行したトランザクション自身は、自分の未コミットの変更を見る必要があるので、トランザクションローカルのキャッシュを別に持つか、DDL をしたトランザクションではキャッシュを使わない。
- ファイルの作成と削除は PostgreSQL と同じ「作成は即時 + 中断で削除、削除はコミット後」。WAL も同じ構造（作成レコード、コミットレコードに削除一覧）。
- 中断したトランザクションが作った孤児ファイルは、yuzhu では**起動時の REDO 完了後に、カタログに載っていないリレーションファイルを削除する**ことを推奨（PostgreSQL より一歩進むが、ユーザーには見えない改善）。

### 6.3 テーブルロックなしで DROP を安全にする

- M3 ではテーブルロックがないので、DROP TABLE のコミット時点で、まだそのテーブルを読んでいる文（DROP より前のスナップショットで走っているスキャン）がありうる。
- **推奨**: リレーションのファイルハンドルを `Arc` で共有し、削除はコミット後に「削除予定」リストへ移し、**どの実行中スナップショットの xmin も DROP の xid を超えたら**（= DROP 前のスナップショットがなくなったら）実際に unlink する。Read Committed では文ごとにスナップショットが替わるので、遅延は長くても実行中の 1 文ぶん。
- バッファプールに残ったそのリレーションのページは、unlink の前に破棄する（書き戻さない）。
- クラッシュした場合は、コミットレコードの削除一覧を REDO で処理するので、遅延中だったファイルも消える。

### 6.4 トランザクションブロック内で実行できない文

- **[実機]** #14: `VACUUM` は明示的ブロック内でも、複数文の暗黙ブロック内でも `ERROR 25001 VACUUM cannot run inside a transaction block`。**[ソース]** `PreventInTransactionBlock`（xact.c）が暗黙ブロックも「ブロック内」と判定する。
- yuzhu では M5 の VACUUM、CREATE DATABASE / DROP DATABASE で同じ扱いにする。M3 には該当する文がない（CHECKPOINT 文を入れる場合、PostgreSQL ではブロック内でも実行できる。**[実機]** #38）。

---

## 7. シーケンスと SERIAL の例外（M4 で実装、M3 では設計だけ予約）

- **[実機]** #27: `nextval` はロールバックされない。CREATE SEQUENCE 自体はトランザクショナル（`create sequence sq; begin; ...; rollback` で sq ごと消えた。第 11 節の BEGIN の取り込みによる）。
- **[ソース]** https://www.postgresql.org/docs/17/functions-sequence.html : nextval と setval は、トランザクションがロールバックしても取り消されない。そのため SERIAL の列には欠番が生じる。
- **[ソース]** sequence.c: シーケンスは 1 行だけのリレーションで、行は xmin を凍結（`FrozenTransactionId`）して書き、**MVCC を使わずにその場で上書き**する。WAL には `SEQ_LOG_VALS`（32）個先まで進めた値を記録し、32 回に 1 回だけ WAL を書く。このため**クラッシュ後に最大 32 個の欠番が生じる**（ユーザーに見える挙動）。
- **[ソース]** sequence.c: WAL を書く nextval は `GetTopTransactionId()` を呼んで xid を割り当てる。コメントによれば、トランザクションのコミット時に WAL を flush させるため（同期レプリケーションの待ちも含む）。
- **[実機]** #28: READ ONLY トランザクションでの nextval は 25006。

**推奨（M4）**:

- シーケンスごとに短時間の排他ラッチを持ち、**グローバル書き込みロックは取らない**（取ると `SELECT nextval(...)` が書き込みトランザクションの終了を待つことになり、PostgreSQL と大きく違う）。
- xid を割り当てる代わりに、トランザクションに「コミット時に WAL を flush する」フラグを立てる（yuzhu は xid の割り当てを書き込みロックと結びつけているため。書き込みロックなしで xid を割り当てると M3 の「実行中は高々 1 つ」の前提が崩れる）。
- `SEQ_LOG_VALS` の先行記録は PostgreSQL に合わせ、クラッシュ後の欠番の挙動も同じにする。
- `currval`/`lastval` はセッションローカル（ロールバックしても戻らない）。

---

## 8. エラー状態と 25P02

### 8.1 状態機械

M1 設計（`spec/design/m1.md` 第 4.1 節、`TransactionStatus { Idle, InBlock, Failed }`）は PostgreSQL と一致しており、M3 でもそのまま使う。内部状態としては暗黙ブロックを区別するため、次の 4 つを持つのがよい。

| yuzhu の内部状態 | PostgreSQL の blockState（xact.c） | RFQ |
|---|---|---|
| `Idle` | `TBLOCK_DEFAULT` / `TBLOCK_STARTED`（単一文） | I |
| `Implicit` | `TBLOCK_IMPLICIT_INPROGRESS` | （Query の途中だけ。RFQ 時点では存在しない） |
| `InBlock` | `TBLOCK_INPROGRESS` | T |
| `Failed` | `TBLOCK_ABORT` | E |

- **[ソース]** postgres.c の `IsTransactionExitStmt`: 失敗状態で受け付けるのは `COMMIT`、`ROLLBACK`、`PREPARE TRANSACTION`、`ROLLBACK TO SAVEPOINT` だけ。それ以外（SELECT、SHOW、SET、RELEASE SAVEPOINT など）は 25P02（**[実機]** #5、#13）。
- 失敗状態の COMMIT はタグ `ROLLBACK`（**[実機]** #5）。`PREPARE TRANSACTION` は yuzhu では対象外（`0A000`）。
- COMMIT の処理中にエラーが出た場合（M5 の遅延制約など）は、ロールバックしてエラーを返し、RFQ は I（**[実機]** #31）。M3 では COMMIT 時の fsync 失敗がこれに当たるが、PostgreSQL は fsync 失敗で PANIC する（`data_sync_retry = off` が既定。**[ソース]** https://www.postgresql.org/docs/17/runtime-config-error-handling.html ）。yuzhu も fsync 失敗はプロセス停止（クラッシュ扱い）にし、REDO で回復させる。クライアントからは接続断として見え、コミットされたかどうかは不明になる（PostgreSQL と同じ）。
- FATAL エラー（タイムアウトや管理者の停止）では接続を閉じ、トランザクションはロールバックする。

### 8.2 MVCC で M1 の undo ログが要らなくなる

- PostgreSQL には**文単位のロールバックがない**。ブロック内で文が失敗すると、トランザクション全体が失敗状態になり、その文の途中までの変更も含めて ROLLBACK で捨てられる。自動コミットの単一文も、失敗すればトランザクションごと中断される。
- MVCC では、中断したトランザクションの xid をコミットログに「中断」と記録するだけで、その xid が書いたタプルはすべて見えなくなる。したがって **M1 の undo ログ（`statement_start` 以降の巻き戻し）は M3 で削除できる**。
- ただし、メモリ上の状態（カタログキャッシュ、GUC、作成したファイルの一覧）はトランザクション終了時に元へ戻す処理が必要（GUC は第 10 節、ファイルは第 6 節）。
- セーブポイント（第 9 節）を入れると部分的なロールバックが必要になるが、それもサブトランザクションの xid を中断にすることで実現する（PostgreSQL と同じ）。undo ログは復活させない。

---

## 9. セーブポイント（M3 の範囲外）

**推奨**: M3 では実装せず、`SAVEPOINT`、`ROLLBACK TO SAVEPOINT`、`RELEASE SAVEPOINT` は次の順で判定する。

1. トランザクションブロック外（暗黙ブロックを含む）なら、PostgreSQL と同じ `ERROR 25P01 SAVEPOINT can only be used in transaction blocks`（各文の名前に置き換え。**[実機]** #12）。
2. ブロック内なら `ERROR 0A000 SAVEPOINT is not supported`（yuzhu 独自の文言）。トランザクションは失敗状態になる。
3. 失敗状態で `ROLLBACK TO SAVEPOINT` が来たら、名前のセーブポイントは存在しえないので、PostgreSQL と同じ `ERROR 3B001 savepoint "x" does not exist`（**[実機]** #39。PostgreSQL ではブロック内で存在しない名前を指定するとこのエラーで失敗状態になる）。

**影響（ドライバとツール）**:

- psql の `ON_ERROR_ROLLBACK`（既定 off）は、文ごとに暗黙のセーブポイントを送る。**[ソース]** https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/psql/common.c
- psycopg 3 の入れ子の `conn.transaction()`、Django の入れ子の `atomic()`、SQLAlchemy の `begin_nested()` は SAVEPOINT を使う。pgJDBC の `autosave`（既定 `never`）も使う。**[未検証]** 各ドライバの既定値は記憶に基づく。
- 共有テスト（`tests/`）では M3 のうちはセーブポイントを使わない。

**将来（M5 以降）に向けて M3 で予約するもの**:

- コミットログは **1 トランザクション 2 ビット、4 状態**（実行中、コミット済み、中断、サブコミット済み）にする。**[ソース]** clog.h の `TRANSACTION_STATUS_SUB_COMMITTED`。
- タプルの xmin/xmax にはサブトランザクションの xid が入りうる前提で可視性判定を書く（「自分のトランザクションか」の判定を `xid == my_xid` ではなく `is_my_xid(xid)` という関数にしておく）。
- 親 xid の対応表（pg_subtrans 相当）は M5 以降で追加する。スナップショットの `subxip` も同様。

---

## 10. SET / SET LOCAL と GUC のトランザクション性

### 10.1 PostgreSQL の挙動

- **[実機]** #9、#10: 通常の SET（SESSION）も**トランザクショナル**で、ブロックを ROLLBACK すると元に戻る。暗黙トランザクション（複数文の Query）が失敗した場合も戻る。
- **[実機]** #11: SET LOCAL はトランザクション終了（コミットでも）で、トランザクション内の SET SESSION の値に戻る。
- **[実機]** #8: ブロック外の SET LOCAL は WARNING 25P01 で無視される。
- **[実機]** #5: 失敗状態では SET も 25P02。
- **[ソース]** guc.c の `AtEOXact_GUC(isCommit, nestLevel)` が、トランザクション（とサブトランザクション）の入れ子ごとに保存した値のスタックを、コミットか中断かに応じて処理する。
- **[実機] [ソース]** **ParameterStatus は ReadyForQuery の直前に送られる**。postgres.c のメインループで `ReportChangedGUCOptions()` を `ReadyForQuery()` の直前に呼んでおり、報告対象の値が**最後に報告した値から変わっているものだけ**を送る。そのため #10 のように SET してすぐ戻った場合は何も送られず、#11 のように同じ Query 内で 2 回変えた場合は最終値だけが送られる。

### 10.2 M1 設計との食い違い

`spec/design/m1.md` 第 5.4 節は「`reported` な設定が SET/RESET で変わったら、CommandComplete の前に `sink.parameter_status` を呼ぶ」としているが、PostgreSQL 14 以降は上記のとおり **RFQ の直前にまとめて差分を送る**（**[未検証]** 14 で変わったという版数は記憶。PG17 の挙動は実機で確認済み）。ドライバは ParameterStatus をどの位置で受けても処理できるので M1 で害はないが、M3 で SET がトランザクショナルになると「送ったのに戻った」状態が生じるため、**M3 で RFQ 直前の差分送信に直す**のを推奨する。

### 10.3 yuzhu の推奨設計

- 設定ごとに「セッション値」と、トランザクション中の変更履歴（変更前の値のスタック）を持つ。トランザクション内で初めて変更するときに元の値を保存する。
- コミット時: SET の値は残し、SET LOCAL の値は保存しておいたトランザクション内のセッション値に戻す。中断時: どちらも保存しておいた元の値に戻す。
- RFQ の直前に、報告対象の各項目について「最後にクライアントへ送った値」と比べ、違うものだけ ParameterStatus を送る。
- 将来のセーブポイントに備え、保存スタックには入れ子の深さ（nestLevel）を持たせる。

### 10.4 トランザクションの特性（SET TRANSACTION など）

- `BEGIN [ISOLATION LEVEL ...] [READ ONLY | READ WRITE]`、`START TRANSACTION ...`、`SET TRANSACTION ...`、`SET SESSION CHARACTERISTICS AS TRANSACTION ...`、GUC の `transaction_isolation`、`default_transaction_isolation`、`transaction_read_only`、`default_transaction_read_only` を受け付ける。
- 分離レベルの変更は最初の文より前だけ（**[実機]** #15、25001）。
- **M3 での分離レベルの扱い（推奨）**: `READ COMMITTED` と `READ UNCOMMITTED`（PostgreSQL でも Read Committed として動く。ただし `SHOW transaction_isolation` は `read uncommitted` を返す。**[実機]** #41）を受け付ける。`REPEATABLE READ` と `SERIALIZABLE` は `ERROR 0A000`。`default_transaction_isolation` にこれらを SET しようとした場合も 0A000（黙って Read Committed で動かすと、利用者が強い分離を期待したまま誤動作するため）。M5 で REPEATABLE READ を受け付け、SERIALIZABLE の扱いは M5 で改めて決める。
- **READ ONLY は M3 で実装する**（安価で、書き込みロックを取らないことが保証される）。書き込み文は `ERROR 25006 cannot execute <文> in a read-only transaction`（**[実機]** #16）。
- `COMMIT AND CHAIN` / `ROLLBACK AND CHAIN` は特性を引き継いで新しいトランザクションを始める（**[実機]** #17）。ブロック外では WARNING ではなく ERROR 25P01（**[実機]** #18）。M3 で実装するのを推奨（実装は小さい）。

---

## 11. Simple Query の暗黙トランザクション

### 11.1 PostgreSQL の規則

**[ソース]** postgres.c `exec_simple_query` のコメント: 歴史的な理由で、1 つの Query に複数の SQL 文があるときは、明示的なトランザクション制御文で区切られない限り 1 つのトランザクションとして実行する。これを「暗黙トランザクションブロック」で表現する（文が 2 つ以上のときだけ `BeginImplicitTransactionBlock` を呼ぶ）。

具体的な規則（**[実機]** で確認）:

1. 文が 1 つなら普通の自動コミット。
2. 文が複数なら、最初の文の前に暗黙ブロックを開く。最後の文の後で閉じてコミットする。
3. **途中の BEGIN は、暗黙ブロックをそのまま明示的ブロックに格上げする**。BEGIN より前の文も同じトランザクションに入る（#3: `insert(40); begin; insert(41); rollback` で 40 も消える）。
4. 途中の COMMIT/ROLLBACK はそこでトランザクションを終え、次の文から新しい暗黙ブロックが始まる（#2）。明示的な BEGIN がない状態での COMMIT は `WARNING 25P01` を出すが、**それまでの文はコミットされる**（#4）。
5. エラーが起きたら、Query の残りの文は実行しない。暗黙ブロック中ならロールバックして RFQ I、明示的ブロック中なら失敗状態で RFQ E（#1、#5）。失敗状態からも、同じ Query の後続の ROLLBACK は受け付けられ、その後の文は実行される（#6。ただしエラーが起きた Query の残りは捨てられるので、これは「次の Query」での話）。
6. 各文はそれぞれ新しいスナップショットで実行する（第 3 節）。
7. 最後の文については、**CommandComplete の前に**トランザクションを閉じる（コミット時のエラーを CommandComplete の後に送らないため。**[ソース]** postgres.c の該当コメント）。
8. ブロック内で実行できない文（VACUUM など）は、暗黙ブロック中でも 25001（#14）。
9. 空の Query は EmptyQueryResponse の後 RFQ（状態は変えない）。

### 11.2 M1 設計との食い違い

`spec/design/m1.md` 第 4.1 節の「ただし Query の途中に BEGIN/COMMIT があれば、そこで区切る」は、BEGIN については不正確。**BEGIN は区切らず、先行文を取り込む**。M3 の実装（または M1 の修正）で規則 3 に直す。対応する slt テストを入れることを推奨する（PostgreSQL に対しても通る）:

```
statement ok
CREATE TABLE ib(x int)

statement ok
INSERT INTO ib VALUES (1); BEGIN; INSERT INTO ib VALUES (2); ROLLBACK

query I
SELECT count(*) FROM ib
----
0
```

（**[未検証]** sqllogictest-rs が 1 レコード内の複数文を 1 つの Simple Query として送るかはドライバ次第。`research-slt.md` で使うドライバの挙動を確認すること。）

---

## 12. 分離性テストの方法論（ワイヤ越し）

### 12.1 3 層の構成

| 層 | 道具 | 書けるもの | 実行対象 |
|---|---|---|---|
| (a) ブロックしない交互実行 | sqllogictest の `connection <name>`（`research-slt.md` 参照） | Read Committed の文ごとスナップショット、未コミット行が見えないこと、自分の変更が見えること、トランザクショナル DDL の可視性、ROLLBACK、SET のトランザクション性 | yuzhu と PostgreSQL の両方 |
| (b) ブロックする交互実行 | **自作の isolation ランナー（Rust）**。PostgreSQL の isolationtester の spec 形式を読む | 書き込みロック待ち、待ちの解除順、lock_timeout、キャンセル、（M5）EvalPlanQual、デッドロック | 両方。期待結果は実装ごとに持てるようにする |
| (c) 不変条件の負荷テスト | Rust のテストバイナリ（複数スレッド・複数接続） | 銀行振替（合計が常に一定）、読み取りが常に一貫したスナップショットを見ること、クラッシュ後の不変条件 | 主に yuzhu（PostgreSQL でも同じ不変条件が成り立つことを確認できる） |

### 12.2 (b) isolation ランナーの設計

- **spec 形式は PostgreSQL の isolationtester に合わせる**（**[ソース]** isolation/README）: `setup { ... }`、`teardown { ... }`、`session <name> { setup {...} step <name> { SQL } teardown {...} }`、`permutation <step> ...`。ステップの完了報告を遅らせるマーカー `(*)`、`(<他のステップ>)`、`(<他のステップ> notices <n>)` もサポートする。PostgreSQL 本体の `src/test/isolation/specs/*.spec`（例: `eval-plan-qual.spec`）の一部をそのまま流用できるようになる。
- **本物の isolationtester を yuzhu に向けるのは M5 以降**: isolationtester は接続時に `PQexecParams`、ブロック判定に `PQprepare`/`PQexecPrepared`（Extended Query）を使い、タイムアウト時に `PQcancelBlocking`（CancelRequest）を送る（**[ソース]** isolationtester.c）。M3 の yuzhu は Extended Query を持たないので、M3 では Simple Query だけで動く自作ランナーが必要。
- **ブロック判定**: ステップを送った後、制御用の接続で `SELECT pg_catalog.pg_isolation_test_session_is_blocked(<pid>, '{<全セッションの pid>}')` を問い合わせる（isolationtester と同じ方法）。pid は `BackendKeyData` から取る。PostgreSQL ではこの関数がそのまま使え、yuzhu では第 5.5 節で実装する。**時間だけで判定（例: 300ms 応答がなければブロックとみなす）するのは不安定なので避ける**。
- 待ちすぎた場合は CancelRequest を送り、さらに待っても終わらなければ失敗とする（isolationtester の `max_step_wait` と同じ考え方。既定 360 秒を、CI 向けに 30 秒程度に短くする）。
- 出力形式も isolationtester に合わせる（`step s1: <SQL>`、`<waiting ...>`、結果表、エラー）。期待結果は PostgreSQL に対して実行して生成し、M3 の yuzhu と結果が違うもの（第 5.6 節の差）は `expected/<name>.yuzhu-m3.out` のような実装別の期待ファイルで管理するか、M5 以降のディレクトリに置く。
- 置き場所の案: `tests/isolation/specs/*.spec`、`tests/isolation/expected/*.out`、ランナーは `impl/rust/` のワークスペースにテスト用クレートとして置く（言語非依存の spec と期待結果は `tests/`、ランナーは実装側）。

### 12.3 M3 で最初に書く分離性テストの候補

| 名前 | 層 | 内容 |
|---|---|---|
| rc-snapshot-per-statement | (a) | A が BEGIN 中に B がコミット → A の次の文で見える |
| uncommitted-invisible | (a) | B の未コミット INSERT は A から見えず、B 自身からは見える |
| ddl-visibility | (a) | ブロック内の CREATE TABLE は他から 42P01、ROLLBACK で消える |
| set-transactional | (a) | `BEGIN; SET ...; ROLLBACK` で元に戻る |
| implicit-block | (a) | 第 11 節の規則 1〜5（単一接続） |
| writer-waits-writer | (b) | A が UPDATE 中、B の UPDATE は待ち、A の COMMIT/ROLLBACK で再開して正しい値になる（第 4.1 節の 4 パターン。PostgreSQL と結果が一致する） |
| reader-not-blocked | (a) | A が UPDATE 中でも B の SELECT は待たずに古い値を返す |
| lock-timeout | (b) | `lock_timeout` で 55P03、その後のトランザクション状態（ブロック内なら E） |
| cancel-wait | (b) | 待ち中のステップを CancelRequest で 57014 |
| idle-in-tx-timeout | (b) | ロックを持ったまま放置すると FATAL 25P03 で切断され、待っていたライターが進む |
| bank-transfer | (c) | N 口座・M スレッドで振替と合計の読み取りを繰り返し、合計が常に一定 |

---

## 13. QUESTIONS.md に記録すべき仮決めの候補

（本タスクでは `QUESTIONS.md` を変更しない。設計書を書く段階で転記すること。）

1. グローバル書き込みロックは「最初の書き込み文の開始時、スナップショットと意味解析の前」に取得し、トランザクション終了まで保持する。BEGIN では取らない。
2. M3 では EvalPlanQual を実装しない。複数行が対象で他者の更新により条件に入る行があるケース（docs の website の例）は PostgreSQL と結果が違う（直列実行と同じ結果）。M5 で揃える。
3. M3 ではテーブルロックを入れない。読み取り中のテーブルの DROP は待たない。デッドロックは起きない。DROP のファイル削除は古いスナップショットがなくなるまで遅延する。
4. 同時 INSERT 同士も待たせる（PostgreSQL は待たない）。
5. セーブポイントは M3 では 0A000（ブロック外では PostgreSQL と同じ 25P01）。コミットログは 4 状態で予約する。
6. REPEATABLE READ / SERIALIZABLE の指定は M3 では 0A000（黙って Read Committed にしない）。READ UNCOMMITTED は Read Committed として受け付ける。
7. ParameterStatus を「RFQ 直前の差分送信」に変更する（M1 設計の修正）。
8. Simple Query の途中の BEGIN は先行文を取り込む（M1 設計の修正）。
9. タプルヘッダの cmin/cmax は別フィールドで持つ（combo CID を使わない）。
10. 中断したトランザクションの孤児ファイルを、起動時の REDO 完了後に掃除する。
11. M4 の nextval はグローバル書き込みロックを取らず、xid も割り当てず、「コミット時に WAL を flush する」フラグだけ立てる。
12. CancelRequest と `idle_in_transaction_session_timeout`、`lock_timeout` を M3 で実装する。

---

## 14. 未検証事項の一覧（実装前に確認すること）

- ロック待ちの待ち行列の公平性の厳密な規則（第 5.3 節）。
- クラッシュで残った孤児ファイルを PostgreSQL 17 が掃除しないこと（第 6.1 節）。
- ユーザー定義関数内の書き込みでのロック取得方式（第 5.1 節。将来の設計案）。
- 各ドライバの SAVEPOINT 利用の既定値（第 9 節）。
- ParameterStatus の送信位置が変わった版（PG14 という記憶。第 10.2 節）。
- sqllogictest-rs が複数文を含むレコードを 1 つの Simple Query で送るか（第 11.2 節）。

---

## 付録 A. 実機確認の方法

- `docker run -d --name yuzhu-txsem-pg -e POSTGRES_HOST_AUTH_METHOD=trust postgres:17`（PostgreSQL 17.11）。
- Python 3 の標準ライブラリだけで書いた最小のワイヤプロトコルクライアント（StartupMessage → Simple Query の送信 → RFQ までのメッセージを種類ごとに表示）を使った。ParameterStatus、NoticeResponse、ReadyForQuery の状態を直接観測するため、libpq や psql は使っていない。
- 同時実行の確認は、2〜3 本の接続を開き、ブロックしうる文はスレッドで送信して 300ms 後にまだ応答がないかでブロックを判定した（調査用の簡易判定。第 12 節のランナーでは使わない方法）。
- スクリプトは作業用ディレクトリに置いたもので、リポジトリには含めていない。
