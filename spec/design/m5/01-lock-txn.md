# yuzhu M5 基本設計 01: トランザクション基盤の再構成・ロックマネージャ・テーブルロック・デッドロック・観測（LK）

M5 の「複数ライター」の土台を作る章です。M3 の単一ライター（`begin_write`・`WriterGuard`・`writer_owner`・`is_blocked_by`）を廃止し、次の 5 つを作ります。

1. 8 モードのリレーションロックと XID ロックを持つ **`LockManager`**（待ち行列、割り込み規則、タイムアウト、セッションスコープ）
2. **デッドロック検出**（`deadlock_timeout` の後に、待った本人が検査する）
3. XID の採番・スナップショットの登録・horizon を持つ **`ProcArray`** と、それを使う **`TxnManager`** の再構成（コミットとアボートの順序、ロックなしの clog、縮小したストレージバリア）
4. 文ごとの**リレーションロックの取得手順**（生のパース木から要求を作り、解決→ロック→再解決する。`LOCK TABLE`、DDL のモード、名前予約ロック）
5. **観測**（仮想リレーション `pg_locks`、`pg_blocking_pids`、`pg_isolation_test_session_is_blocked` の置き換え）

行ロックと MultiXact は 02 章、VACUUM は 03 章の担当です。この章は、02（ヒープの待ち）・03（VACUUM のロック）・04（準備済み文の Bind）・07（FK のロック）・08（ロールのオブジェクトロック）・09（`Database` ロック）が呼ぶ側の口を固定します。

- 契約: `spec/design/m5/00-contracts.md`（以下「契約」）。この章は §1.2 の D5 / D9 / D10 / D11 / D12 / D13 / D21 / D33 / D37 / D38 / D40 / D41 / D48、§3.4、§4.1〜§4.3 と §4.10、§5.1〜§5.3 に従う。従えない点は §11 に挙げる。
- 前提の設計: `m3.md`（§5.3 コミット、§5.9 ロックの順序、§6.6 txn、§6.11 セッション）、`m4/00-contracts.md`（§10 `ExecCtx`、§14.1 `Transaction`、§14.6 `DdlCtx`）、`m2.md` §6.7（ストレージバリア）
- 調査（根拠）: `spec/research/m5-concurrency.md` の §3（ロックマネージャ）、§8（待ちの中断）、§9（テーブルロック）、§10（デッドロック）、§11（複数ライター化の影響）、§14（移行手順）、§15（テスト）。PostgreSQL のソースは REL_17_STABLE（`PG:<path>`）。
- 根拠の記号: **【確認】**ソースを読んだ、**【実機】**PostgreSQL 17.11 で動かして確かめた（付録 A の番号 E1〜E20）、**【提案】**yuzhu への推奨、**（未検証）**。
- 略号: 決定は `LK-D<n>`、確認事項は `M5-LK-Q<n>`、作業パッケージは `LK-1`〜`LK-5`（契約 §7）。

---

## 0. 決定（この章で扱う論点）

契約の D5 / D9 / D10 / D11 / D12 / D13 / D21 / D33 / D37 / D38 / D40 / D41 / D48 は「そのまま採用」とし、ここには**それらを具体化するときに決めたこと**と、調査・依頼文との食い違いの決定を書く。

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| LK-D1 | 待ちグラフの辺と被害者（D9 の具体化） | (a) hard edge のみ（この章の依頼文）／(b) hard + soft の検出だけで、待ち行列の並べ替えはしない（契約 D9、m5-concurrency §10.2）／(c) PostgreSQL の `TopoSort` まで | **(b)**。深さ優先で hard 辺（保持者）→ soft 辺（待ち行列で先行する衝突する待ち手）の順にたどり、**検査した本人に戻る**循環が見つかったら本人が 40P01 で中断する。本人を通らない循環は無視する（その循環の当事者が自分で検出する） | (a) だと、「A が `AccessShare` を持ち、B が `AccessExclusive` 待ち、C が B の後ろで `AccessShare` 待ちで、C の持つ別のロックを A が待つ」という循環（B→A hard、A→C hard、C→B soft）が見えず**永久に待つ**。実機 E9 でこの構成を作ると PostgreSQL は待ち行列を並べ替えて C を通し、エラーにならない。yuzhu は並べ替えず 40P01 にする（**既知の差**。M5-LK-Q1）。依頼文の「hard edge のみ」は「hard 辺の循環は必ず検出する・並べ替えない」と解釈した |
| LK-D2 | ロック表の構造 | 1 本の `Mutex` + バックエンドごとの `Condvar`／パーティション分割（PostgreSQL は 16 分割。m5-concurrency §3.2） | **1 本の `Mutex<LockTable>`**。待ち手ごとに `Arc<Condvar>`（同じ `Mutex` と組にする）。分割は M6（計測してから） | デッドロック検査が全体を見る必要がある（PG も全パーティションを取る）。M5 の接続数・文の頻度では 1 本で足りる見込み |
| LK-D3 | 待ち行列の規則 | 「FIFO + 割り込み規則」と契約 §4.2 は書く／PG17 の `LockAcquireExtended` / `ProcSleep` / `ProcLockWakeup` を読んで細部まで移植 | **後者（§5.1）**。(1) 同じ `(タグ, モード)` を既に持っていれば待ち行列を見ずに参照カウントを増やす、(2) 要求が**待ち手のモード**（`wait_mask`）と衝突するなら保持者と衝突しなくても待ち行列に入る、(3) 自分がそのロックの何かを持っていて、先行する待ち手のモードが自分の保持と衝突するなら**その前に入る**。このとき自分の要求が先行者と保持者に衝突しなければ**待たずに付与**し、先行者が自分の要求と衝突するモードを保持しているなら**待たずに 40P01**（早期デッドロック）、(4) 解放のたびに、先行する未付与の待ち手のモード（`ahead`）と保持者のどちらにも衝突しない待ち手を順に付与する | 契約の「割り込み規則を入れる」だけでは、(3) の即時付与と早期デッドロック、(4) の起こし方が決まらない。実機 E7・E8 で確認した |
| LK-D4 | 待ちの周期とタイムアウトの測り方 | 契約: 10〜50ms／条件変数に通知を足してキャンセルも起こす | **20ms 周期**（`LOCK_WAIT_POLL`）の `wait_timeout`。次の期限（`lock_timeout`、未実施の `deadlock_timeout`）までが 20ms より短ければそこまで。起きるたびに (1) 付与済みか、(2) `interrupts.check()`（停止 → キャンセル → `statement_timeout`）、(3) `lock_timeout`、(4) デッドロック検査、の順で見る。**`lock_timeout` は 1 回の `acquire` ごと**に測る（`wait_for_xact` の中の各 `acquire` も別の待ち） | キャンセルの遅延は最大 20ms で、`InterruptFlag` に起こす仕組みを足さずに済む。PG も `ProcSleep` ごとにタイマーを仕掛ける【確認】PG:src/backend/storage/lmgr/proc.c |
| LK-D5 | デッドロック検査の回数 | 待ちの間ずっと周期的に／PG と同じく 1 回 | **待ち始めから `deadlock_timeout` 経過した時点で 1 回だけ**。検査の前に付与されていれば検査しない | PG の `DEADLOCK_TIMEOUT` は 1 回だけ仕掛けるタイマー【確認】。待ちに入ることでできた循環は「最後に待ちに入った者」の検査で見つかる（§5.4）。実機 E14・E15（と m5-concurrency §2 の #5・#5b） |
| LK-D6 | `release` とスコープ | 契約の `release(id, tag, mode)` にはスコープ引数がない | **参照カウントはスコープごと**（`HoldCount { txn, session }`）。`release` は Transaction スコープのカウントを優先して 1 減らし、なければ Session スコープを減らす。`release_all(Transaction)` は Transaction のカウントだけを 0 にし、`release_all(Session)` は両方を 0 にする | `wait_for_xact`・タプルロックは「取ってすぐ解放」なので同じスコープで足りる。`Database` の AccessShare は Session スコープで接続の終わりまで残す（D40） |
| LK-D7 | XID の終了待ち | `acquire(Share)` を直接呼ぶ／`wait_for_xact`（契約 §4.2） | **`wait_for_xact`**。自分の XID を待つのは内部エラー。`still_running` は `LockManager` の `Mutex` を**外して**呼ぶ。2 周目からは 1ms 眠り、`interrupts.check()` を呼ぶ | PostgreSQL の `XactLockTableWait`【確認】PG:src/backend/storage/lmgr/lmgr.c。D37 により「実行中なのに XID ロックがない」隙間は作らないが、安全網として残す |
| LK-D8 | スナップショットの登録の持ち方 | バックエンドごとのスロット（M3 調査 §4.5）／登録簿（契約 D11） | **`ProcArray` が `BTreeMap<Xid, u32>`（xmin → 登録数）を持つ**。スナップショットの計算と登録は同じ `Mutex` の中で行い、`RegisteredSnapshot` の Drop が数を減らす。`oldest_xmin = min(実行中の最小 XID, 登録済みの最小 xmin, next_xid)` | 取得と登録の間に隙間があると、その間に horizon が追い越す（VACUUM が見えているはずの行を消す）。登録先をバックエンドごとにしないので、`Drop` が `BackendId` を要らない |
| LK-D9 | カタログ用スナップショットも登録するか | 契約 §3.1・§4.3: 登録しない／登録する | **登録する（短命）**。名前解決とアナライズに使うカタログ用スナップショットは `take_snapshot` で取り、使い終わったら（名前解決は 1 回ごと、アナライズは文の終わりで）Drop する。登録しない `TxnManager::snapshot()` は、`SnapshotAny` に準ずる内部の用途（VACUUM のテスト、`pg_current_snapshot`）だけに使う | 登録しないと、取得後に別のトランザクションが削除してコミットし、VACUUM がそのカタログのタプルを消すまでの間に、カタログを読む文が「あるはずの行」を失う（取得から読み取りまで数マイクロ秒〜ミリ秒の窓だが、起きると 42P01 などの偽のエラーになる）。PostgreSQL は `GetCatalogSnapshot` を登録している【確認】。03 章（VACUUM）の C3 も同じ結論に独立に達している。契約の用語表の「登録しない」を変える（§11-5） |
| LK-D10 | `Clog` のロックなし読み取り（D13 の具体化） | `status` は `RwLock` の読み + `AtomicU8`、`set_status` は `fetch_or`（契約） | `status` は `RwLock` の読みの中で 1 回 `load(Acquire)`。`set_status` は **`compare_exchange` のループ**で「現在 0（InProgress）のときだけ」立てる。ページの dirty フラグは**バイトを立てた後**に `store(true)` する | `fetch_or` だと `Committed` と `Aborted` の二重設定が `0b11`（予約値）になり、矛盾を検出できない（M3 §6.6.2 は矛盾を Panic としている） |
| LK-D11 | 文の失敗とロックの解放 | ブロックの中で失敗してもロックは `ROLLBACK` まで保持／失敗した時点で解放 | **失敗した時点で解放**（M2/M3 の `report_error` が既にその時点でアボートする。変えない）。`ROLLBACK` は何もしない | 【実機 E7】早期デッドロックで失敗した A のブロックは `25P02` の状態に入るが、待っていた B はその時点で `AccessExclusive` を得る |
| LK-D12 | 取得手順の検証（契約 §5.1 d の 1〜2 の補強） | 名前解決の結果をそのまま信じる／アナライズ後に「必要なロックを持っているか」を確かめる | **確かめる**。生のパース木で取ったロックの集合に対し、アナライザの出力（`Bound`）が参照するリレーションと必要なモード（§5.9 の規則を `Bound` に適用したもの）が含まれなければ、足りない分を取って**アナライズからやり直す**（最大 3 回。4 回目は `XX000`） | 名前の解決は 2 回行う（ロックのため、アナライズのため）。その間に、検索パスの手前のスキーマに同名の表が作られてコミットされると、2 回目は別の表を指す。PostgreSQL は 1 回しか解決しないのでこの問題がない。準備済み文の再アナライズ（D21）も同じ経路で守る |
| LK-D13 | 見つからない名前 | ロックの段階で 42P01 を出す（PostgreSQL の `RangeVarGetRelidExtended`）／ロックせず飛ばす | **飛ばす**。42P01 / `IF EXISTS` の NOTICE などは、アナライザ（と `ddl/`）が出す。ロックの段階の失敗は「待ちのエラー」と `LOCK TABLE` の 42809 だけ | 文言と位置（`ObjectName.span`）の出し方を 1 か所にする。取りこぼした表は LK-D12 の検証が拾う |
| LK-D14 | インデックスのロック | PostgreSQL は DML でインデックスにも `RowExclusive` / `AccessShare` を取る【実機 E1】／取らない | **取らない**。インデックスに対する DDL（`CREATE INDEX` は表の Share、`DROP INDEX` は表の `AccessExclusive`）は常に表のロックを先に取るので、表のロックで足りる。`pg_locks` にインデックスの行は出ない（**既知の差**。M5-LK-Q3） | ロックの数が半分になり、デッドロックの当事者も減る |
| LK-D15 | CREATE の名前の衝突 | PostgreSQL: カタログの一意インデックスで待ち、後発は `23505`（`pg_type_typname_nsp_index`。実機 E11）／M3 の「既知の差」: `42P07`／**名前予約ロック**（契約 §3.1） | **名前予約ロック**（`Object { db, class: 1259, obj: FNV-1a(名前空間 OID ‖ 名前) }` を Exclusive、トランザクション終了まで）を、CREATE が作る名前に取る（DROP は取らない）。取れた後にアナライザが存在を検査する。後発は先発のコミットを待ち、**`42P07`**（`IF NOT EXISTS` なら NOTICE）。先発がアボートすれば成功する。**未コミットの DROP がある間の CREATE** は、PostgreSQL と同じく待たずに `42P07`（アナライザが古い行を見る。実機 E20） | カタログに一意インデックスがない（M4 までのカタログはヒープだけ）。エラーの SQLSTATE は PostgreSQL と違う（**既知の差**。M5-LK-Q5） |
| LK-D16 | DROP の第 2 段 | 表のロックだけ／FK でつながる相手の表と、列が所有するシーケンスも | **`AccessExclusive` を相手にも取る**。表 T を `AccessExclusive` で取った後、(1) T が持つ FK の相手（T が参照する表、T を参照する表）、(2) T の列が所有する SERIAL / IDENTITY のシーケンスを、同じモードで取る。`ALTER TABLE ... DROP CONSTRAINT`（FK）は相手の表だけ | 【実機 E2】`DROP TABLE c`（c が p を参照）は p も `AccessExclusive`、`DROP TABLE p CASCADE` は c も。シーケンスは `nextval` が `RowExclusive` を取る（LK-D18）ので、取らないと DROP のコミットが使用中のファイルを消す |
| LK-D17 | `LOCK TABLE` の置き場所 | `ddl::execute` の中で取る／生のパース木の段階で取る | **生のパース木の段階（他の文と同じ経路）**で取り、`BoundDdl::LockTable` の実行は何もしない（取れていることを `debug_assert` で確かめてコマンドタグを返すだけ）。25P01 は**ロックより前**に session が返す。読み取り専用トランザクションでも実行できる（PG17 の実測。モードを問わない）。仮想リレーションの `LOCK TABLE` は成功して何もしない | 【実機 E6】`lock table nosuch;` をブロックの外で打つと 25P01（存在検査より前）。NOWAIT のメッセージはロックの段階で出す必要がある。PostgreSQL は読み取り専用でも通す |
| LK-D18 | シーケンス関数の実行時ロック | 実行時にはロックしない（M4 の「シーケンスはロックなし」）／`nextval` などが `RowExclusive` | **`nextval` / `currval` / `setval` は、呼び出しの先頭で `Relation(seq)` を `RowExclusive`（トランザクションスコープ）で取る**。`RuntimeInfo::nextval` の実装（session）が `LockManager::acquire` を呼ぶ。保持済みなら即座に戻る（LK-D3 の (1)） | 【実機 E3】PostgreSQL も `RowExclusive`。`ALTER SEQUENCE` は `ShareRowExclusive`、`DROP SEQUENCE` は `AccessExclusive` なので、これがないと実行中の `nextval` の下でファイルが消える（D10 の「ファイルはリレーションロックで守る」の穴） |
| LK-D19 | `pg_locks` の形 | PG の 16 列そのまま／yuzhu の行だけ | **16 列を PG17 と同じ名前・型・順序で持つ**（付録 A の E18）。行は**保持（`granted = true`）と待ち（`false`）**。`virtualxid` の行は作らない（`virtualxid` / `virtualtransaction` は常に NULL）。`fastpath` は常に `false`。`Database` ロックは `locktype = 'object'`（`database = 0`、`classid = 1262`、`objid = データベースの OID`）として出す。`waitstart` は待ち始めの時刻 | 共有テストは `locktype`・`relation`・`mode`・`granted` で絞って比べる（M5-LK-Q6） |
| LK-D20 | `pg_blocking_pids` の意味 | hard だけ／PG と同じ（hard + soft）【確認】PG:src/backend/utils/adt/lockfuncs.c | **PG と同じ**。待っていないバックエンドは空。`pg_isolation_test_session_is_blocked(pid, among)` は「`pid` の `pg_blocking_pids` が `among` と交わるか」（PG17 の `waitfuncs.c` と同じ。安全なスナップショット待ちは作らない）。登録のない `pid` は `false` | 分離性テスト（TS）が PostgreSQL と同じ spec を流せる |
| LK-D21 | `BackendRegistry` と `shutdown.rs` の登録簿、`LockManager` への登録の持ち主 | 1 つに統合／役割を分けて両方に登録（契約 §4.1）。`LockManager` への登録と解放を `BackendGuard` が行う（この章の初案）／09 章の `SessionLocks`（`dbase.rs`）が行う | **分ける**（§5.12）。`BackendRegistry`（core）は「誰がどのデータベース・ロールで繋がっているか」「pid → `BackendId`」を持つだけで、`BackendGuard` の Drop は登録簿から外すだけ。`Coordinator`（server）はソケットと停止要求を持つ。どちらも**同じ pid** と**同じ `Arc<InterruptFlag>`** をキーと中身にする。**`LockManager` への登録・Session スコープのロックの解放・登録解除は 09 章の `SessionLocks`（`register` / Drop）が行う**。LK は `LockManager` 側の保証（§4.2 の「意味の固定」）だけを決める | 停止と接続数の制限は別の関心事。09 章が既に `SessionLocks` を設計済みで、2 つの RAII が同じ登録を持つと二重登録になる。「接続が消えたらロックが消える」は `SessionLocks` の Drop（`release_all(Session)` → `unregister_backend`）と、その前のトランザクションの片付け（`XidGuard`）で守る |
| LK-D22 | 規約 1 のデバッグ検出（契約 §2 規約 1） | `LockManager` が `buffer/track.rs` を直接呼ぶ／フック経由 | **フック経由**（`txn::lock` は `storage` より下の層で、`track.rs` を `use` できない）。`txn::lock::set_wait_assertion(fn(WaitSite))`（`OnceLock`、デバッグビルドだけ）を `Cluster::open` が 1 回登録し、登録先は `storage::buffer::track::assert_may_wait`。**待ちに入りうる呼び出し（`acquire`・`wait_for_xact`）の入口で、待たなくても検査する**。検出対象はページのラッチ、許容を超えるピン、共有ストレージバリア、コミットゲート、`checkpoint_lock`。**`Mutex` 一般は検出できない**（std の `MutexGuard` は追跡できない） | 待たない呼び出しでも検査すると、競合が起きた日にだけ panic するバグを毎回のテストで捕まえられる |
| LK-D23 | ストレージバリアの縮小（D10 の具体化） | バリアを廃止／チェックポイントの書き出し（共有）とファイルの削除（排他）にだけ残す（契約） | **後者**。`statement_barrier()` の名前は契約どおり残すが、**持つのはチェックポイントのバッファ書き出しだけ**（`checkpoint.rs`）。Session は持たない。`exclusive_barrier()` はコミット / アボートの `unlink`（DROP・TRUNCATE・作成の取り消し）が持つ。M3 の「同じスレッドが共有バリアを再度取ると panic」の検査（`track::barrier_acquired`）は、チェックポイントのスレッドで使い続ける | 長い SELECT が走る間の DROP のコミットが、新しい文をすべて止める（書き手待ちの `RwLock`）問題の解消（D10） |
| LK-D24 | XID の割り当て時期 | PostgreSQL は最初のヒープ書き込みで遅延【実機 E5】／契約 §5.1 d-3: 書き込む文・FOR 句つき SELECT の**文の開始時** | **契約どおり文の開始時**。ただし**ロックを取った後**。書き込む文（DDL を含む）と FOR 句つきの SELECT だけが持ち、FOR 句のない読み取りと `LOCK TABLE` は XID を持たない | `ExecCtx` から `TxnManager` を呼ばずに済む。差: 何も書かない `UPDATE ... WHERE false` が XID を持ち、horizon を止める・`pg_locks` に `transactionid` の行が出る（M5-LK-Q4） |
| LK-D25 | `lock_timeout` と `statement_timeout` が同時に切れたとき | PG: 先に切れた方（同時なら `lock_timeout`）【確認】PG:src/backend/tcop/postgres.c の `ProcessInterrupts`／常に `interrupts.check()` を先にする | **`interrupts.check()` を先に**（停止 → キャンセル → `statement_timeout` → `lock_timeout`）。違いが出るのは両方が同じ 20ms の周期の中で切れた場合だけ | `InterruptFlag` の期限を読む口を増やさない。M5-LK-Q7 |
| LK-D26 | Drop の安全網 | `Transaction` が `LockManager` を持つ／`XidGuard` と `SessionLocks` だけ | **`XidGuard` の Drop**（未完了なら clog を `Aborted` にし、実行中一覧から外し、トランザクションスコープのロックを解放する）と、**`SessionLocks` の Drop**（09 章。両スコープの全ロックを解放し、`LockManager` から登録解除する）の 2 段。XID を持たないトランザクションの取りこぼしは後者が拾う。`Session` のフィールドは `txn` を `session_locks`・`backend` より**前**に宣言する（先に Drop される。DB 章 §5.1 の `ConnectGrant` の並びと同じ） | `Transaction` に `Arc<LockManager>` を持たせると、`Transaction::new()`（テストで多用）が引数を要る |
| LK-D27 | Repeatable Read の最初のスナップショットの時点 | 契約 §5.1 d・D5: ロックの後（この章の初案）／02 章 RW-D10: **ロックの前**（PostgreSQL は解析の前に取る【実機】） | **RW-D10 に従う**。RR のトランザクションスナップショットだけは、最初にスナップショットを要する文の**リレーションロックの取得の前**に `take_snapshot` して `Transaction.xact_snapshot` に入れる（§5.9 の手順 0'）。RC の文のスナップショットは契約どおりロックの後 | 02 章が実機（RR の最初の文がテーブルロックを待ったとき、待ちの後に相手のコミットした行が見えない）で決めた。LK の手順はそれを前に挟む |
| LK-D28 | 他章の要求の取り込み | 03 章 C5・C6・C12、04 章 C7・C12、08 章 C5、10 章 CR-1・CR-4 | 取り込む（§4.2・§4.4・§4.6・§5.9）: `LockManager::has_waiters`、`ProcArray::oldest_xmin_hint`（と `RunningXids` の実装は 03 章が `prune.rs` に書く）、`Clog::open(vfs, next_xid, oldest)` と `oldest()` の口、`TxnManager::registered_snapshot_count()`、`Session::lock_statement_relations`、`RelationResolver::check_access`（`pg_authid` の 42501）、`Session::backend_id()` / `backend_pid()` | 同じ口を 2 つの章が別々に作らない |

---

## 1. 範囲

### 1.1 この章で行うもの

| 分類 | 内容 | WP |
|---|---|---|
| ロックマネージャ | `LockTag` 5 種、8 モード、参照カウント、待ち行列、タイムアウト、キャンセル、セッションスコープ、`try_acquire`、`wait_for_xact`、`lock_status`、`blocking_backends` | LK-1 |
| デッドロック | `deadlock_timeout` の後に 1 回、待った本人が検査。40P01、DETAIL と HINT は PG17 と同じ書式 | LK-2 |
| トランザクション基盤 | `ProcArray`、`assign_xid` / `XidGuard`、`RegisteredSnapshot`、`oldest_xmin`、カタログ用スナップショット、ロックなしの `Clog`、バリアの縮小、コミット / アボートの順序、`finish_without_xid`、`BackendRegistry` | LK-3 |
| リレーションロック | 生のパース木からの要求、解決→ロック→再解決、文ごとのモード、`LOCK TABLE`、DDL のモード、名前予約ロック、`TxnControl` の口、検証 | LK-4 |
| 観測 | `catalog::virtual_rel`、`pg_locks`、`pg_blocking_pids`、`pg_isolation_test_session_is_blocked` の置き換え | LK-5 |
| 設定 | `deadlock_timeout`、`max_locks_per_transaction`、`lock_timeout` の意味変更、`log_lock_waits`（保存だけ） | LK-1、LK-4 |

### 1.2 この章で行わないもの

| 項目 | 扱い |
|---|---|
| 行ロック、MultiXact、更新競合の待ち（heap 内の `l1:`〜`l3:`）、タプルロックの**取り方** | 02 章。この章は `LockTag::Tuple` と `wait_for_xact` の口だけ作る |
| VACUUM・ANALYZE のロックの取り方 | 03 章。この章の `lock_relation`（§6.3）を呼ぶ |
| 準備済み文の Bind の手順 | 04 章。この章の `acquire_oids`（§4.6）を呼ぶ |
| アドバイザリロック（`pg_advisory_lock` など）、`pg_stat_activity`、fast-path ロック、ロックグループ（並列問い合わせ）、述語ロック | 対応しない。関数は `42883`（存在しない）のまま |
| `max_locks_per_transaction` の上限と `53200` | 起こさない（設定は `SHOW` だけ。§4.8） |
| 待ち行列の並べ替え（soft deadlock の解消） | しない（LK-D1。M5-LK-Q1） |
| autovacuum が DDL を妨げるときの自動キャンセル（PG の `DS_BLOCKED_BY_AUTOVACUUM`） | M6 |
| `LOCK TABLE` の継承（`ONLY` / `*`）の意味 | 構文は受け付けるが、継承がないので無視する |

### 1.3 保証すること

- **衝突しない**: 異なるバックエンドが同時に保持できるのは、衝突表（§4.1）で互いに衝突しないモードだけ。
- **到着順**: 保持者と衝突しなくても、**先行する待ち手のモードと衝突する要求は待ち行列の後ろに並ぶ**（FIFO。読み取りが続いても DDL が飢えない。実機 E8）。自分がそのロックを既に持つ場合の割り込みだけが例外（LK-D3）。
- **漏れない**: 接続が消えれば（09 章の `SessionLocks` の Drop）、そのバックエンドの全ロックが消える。トランザクションの終わりには、トランザクションスコープのロックが消える。
- **起きる**: ロックが解放される・待ち手が取り下げられる（キャンセル、タイムアウト、デッドロック）たびに、付与できる待ち手が必ず付与される。
- **待ちに入ることでできた循環は誰かが 40P01 になる**: 待ちに入ることで待ちグラフに循環ができれば、その当事者のうち最後に待ちに入った者が `deadlock_timeout` の後に 40P01 で中断する（§5.4。1 回きりの検査の限界は §9-2）。
- **horizon を追い越さない**: 登録されたスナップショット・実行中の XID より新しい境界を `oldest_xmin()` が返すことはない（LK-D8）。
- **コミットの後にだけ見える**: 待っていたセッションが起きた時点で、相手のコミットの結果（clog・カタログのキャッシュの無効化）が見える（D38。§5.8）。

---

## 2. 構成

契約 §2 の木のうち LK の持ち分を詳しくする。`★` 新規、`△` 変更、`✕` 削除。

```
impl/rust/crates/yuzhu-core/src/
├── backend.rs                      ★ BackendId、BackendInfo、BackendRegistry、BackendGuard（§4.5）
├── txn/
│   ├── mod.rs                      △ `Snapshot::is_own`、`IsolationLevel`、`LockTupleMode`（契約 §4.3）、再エクスポート
│   ├── lock/
│   │   ├── mod.rs                  ★ LockTag、LockMode、LockScope、WaitCtl、WaitCtx、LockManager（公開 API と待ちのループ）、set_wait_assertion
│   │   ├── table.rs                ★ LockTable（内部）: エントリ、保持者、待ち行列、request / grant / release / wake_waiters
│   │   ├── wait.rs                 ★ 待ちのループ（acquire の本体）、wait_for_xact、タイムアウトの計算
│   │   ├── deadlock.rs             ★ 待ちグラフ、循環の検出、DETAIL の組み立て、describe_tag
│   │   └── relation.rs             ★ lock_relation（解決→ロック→再解決）、lock_name、name_key（§6.3）
│   ├── proc_array.rs               ★ ProcArray: 実行中の XID、採番、スナップショットの登録簿（manager.rs から切り出す）
│   ├── snapshot.rs                 ★ RegisteredSnapshot
│   ├── manager.rs                  △ 複数ライター化: assign_xid、XidGuard、Transaction、commit / abort / finish_without_xid。✕ begin_write、WriterGuard、writer_owner、is_blocked_by
│   ├── clog.rs                     △ 読み取りをロックなしに（LK-D10）。truncate_before は VC
│   └── multixact.rs                （02 章）
├── storage/buffer/track.rs         △ ゲート・checkpoint_lock の追跡、assert_may_wait（§6.6。LK が追記する）
├── checkpoint.rs                   △ バリアの使い方（LK-D23）、ゲート・checkpoint_lock の追跡の呼び出し
├── session/
│   ├── locking.rs                  ★ 生のパース木 → 要求、取得、検証、準備済み文用の口、end-of-transaction のロック解放（§5.9、§5.8）
│   └── txn_ctl.rs                  （02 章が持つ。LK は §5.8 の 1 行の呼び出しだけを足す）
├── sql/parser/lock.rs              ★ LOCK TABLE の構文
├── analyzer/ddl_ext/lock_table.rs  ★ LockTableStmt → BoundDdl::LockTable
├── ddl/lock_table.rs               ★ BoundDdl::LockTable の実行（何もしない + 検証）、require_transaction_block
├── catalog/virtual_rel.rs          ★ VirtualRelation、registry()、PgLocks、bootstrap の行（§6.5）
├── executor/virtual_scan.rs        ★ PhysicalPlan::VirtualScan の Executor
├── debug_knobs.rs                  △ 変異テスト用のスイッチを足す（§6.7）
└── engine.rs                       △ Cluster が locks / backends / multixact を持つ。組み立ての順序（§5.12）
yuzhu-server/src/shutdown.rs        （変更なし。BackendRegistry との関係は §5.12）
```

**依存の方向**（契約 §2 のとおり）: `txn::lock` は `storage` を使わない（`Oid`・`Xid` と `BackendId` だけ）。`txn::proc_array`・`txn::snapshot` も同じ。`txn::manager` は `wal`・`txn::lock`・`txn::proc_array` に依存する。`session::locking` は `analyzer` と `catalog` を使う。`ddl::*` は `session` を `use` しないので、VACUUM・データベース・ロールの DDL が使うロック取得の部品は `txn::lock::relation` に置く（§6.3）。

**この章の規約**（契約 §2 の規約 1〜8 に加えて）:

1. `LockManager` の内部 `Mutex` を持ったまま、I/O・`Condvar` 以外の待ち・他の `Mutex` の取得をしない（葉ロック。例外は §5.11）。`still_running` などのコールバックは `Mutex` を外して呼ぶ。
2. 公開の待ち関数（`acquire`・`wait_for_xact`・`lock_relation`・`lock_name`）は必ず `&WaitCtl` を受け取る（契約 規約 2）。待たない口は `try_acquire` だけ。
3. `Drop` の中で I/O をしない。`Drop` で呼ぶのは `release_all`・`unregister_backend`・clog のメモリ上の更新だけ。内部の `Mutex` は `lock_ignore_poison` で取る。
4. 失敗しうる公開関数の `Mutex` は `util::sync::lock`（poison は `Severity::Panic`）、`release*` など失敗できない関数は `lock_ignore_poison`。
5. ロックのスコープは、トランザクションに紐づくもの（リレーション、XID、タプル、名前予約）は `Transaction`、接続に紐づくもの（`Database`）は `Session` を使う。迷ったら `Transaction`。

---

## 3. ディスク上の形式

**この章はディスク形式を変えない。** ロックは再起動で全部消える（クラッシュ後のロックマネージャは空。契約 §5.4）。WAL のレコードも制御ファイルの項目も足さない。

観測用の仮想リレーションだけが、`pg_class` と `pg_attribute` に行を持つ（initdb が書く）。

| `pg_class` の列 | `pg_locks` の値 | 備考 |
|---|---|---|
| `oid` | **9811**（契約 §3.2 の yuzhu 独自の範囲。08 章 C6 が `pg_roles` に 9810 を申請しており、「LK は 9811 以降」とされた。PG17.11 の実測は 12073 だが、システムビューの OID は版ごとに変わりうるので使わない。【実機 E18】） | 共有テストは OID に依存しない |
| `relname` / `relnamespace` / `relowner` | `pg_locks` / 11（`pg_catalog`）/ 10 | |
| `relkind` | `'v'` | `RelKind::Virtual`（§11-12） |
| `reltype` | 0 | 行型の `pg_type` は作らない（PG の 12075 に対応する行を持たない。既知の差で、`\d pg_locks` の列の表示には影響しない） |
| `relfilenode` / `relpersistence` / `relhasindex` ほか | 0 / `'p'` / false | ファイルを持たない |

`pg_attribute` は 16 行（§4.7 の列）。`pg_locks` に対する `INSERT` / `UPDATE` / `DELETE` / `TRUNCATE` / `DROP TABLE` のエラーは §6.5。

---

## 4. 共通の型（契約）

契約 §4.1〜§4.3、§4.10 の署名は**変えない**。ここでは (1) 契約の署名を再掲して意味と前提を固定し、(2) 契約が「追加してよい」とした項目・型を足す。足したものには「追加」と書く。契約と食い違う点は §11 にある。

### 4.1 `LockMode` の衝突表（`txn/lock/mod.rs`）

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum LockMode { AccessShare = 1, RowShare, RowExclusive, ShareUpdateExclusive,
                    Share, ShareRowExclusive, Exclusive, AccessExclusive }

/// 要求するモード（添字 = モード − 1）が衝突する保持モードのビット集合（ビット (m − 1) が m）
const CONFLICT_MASK: [u8; 8] = [0x80, 0xC0, 0xF0, 0xF8, 0xEC, 0xFC, 0xFE, 0xFF];

impl LockMode {
    pub const ALL: [LockMode; 8] = [/* AccessShare .. AccessExclusive */];             // 追加
    pub fn bit(self) -> u8 { 1 << (self as u8 - 1) }                                   // 追加
    pub fn conflicts(self, other: LockMode) -> bool { CONFLICT_MASK[self as usize - 1] & other.bit() != 0 }
    /// "AccessShareLock" … "AccessExclusiveLock"。DETAIL にも pg_locks.mode にも**そのまま**使う
    /// （契約 §4.2 のコメントは「Lock を除く」と読めるが、PG17 は両方とも "Lock" 付き。実機 E14・E18）
    pub fn pg_name(self) -> &'static str;
    /// LOCK TABLE の `IN <words> MODE`（例 "access share"、"share row exclusive"。小文字・空白 1 個に正規化済み）
    pub fn from_sql_words(words: &str) -> Option<LockMode>;                            // 追加
}
```

**8×8 の衝突表**（【確認】PG:src/backend/storage/lmgr/lock.c の `LockConflicts[]`。✕ = 衝突。表は対称で、実装の単体テストは下の `mask` 列を固定値として全 64 組を検査する。§7.1）

| 要求 ＼ 保持 | AccessShare (1) | RowShare (2) | RowExclusive (3) | ShareUpdateExclusive (4) | Share (5) | ShareRowExclusive (6) | Exclusive (7) | AccessExclusive (8) | `mask` |
|---|---|---|---|---|---|---|---|---|---|
| **AccessShare** | | | | | | | | ✕ | `0x80` |
| **RowShare** | | | | | | | ✕ | ✕ | `0xC0` |
| **RowExclusive** | | | | | ✕ | ✕ | ✕ | ✕ | `0xF0` |
| **ShareUpdateExclusive** | | | | ✕ | ✕ | ✕ | ✕ | ✕ | `0xF8` |
| **Share** | | | ✕ | ✕ | | ✕ | ✕ | ✕ | `0xEC` |
| **ShareRowExclusive** | | | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ | `0xFC` |
| **Exclusive** | | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ | `0xFE` |
| **AccessExclusive** | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ | ✕ | `0xFF` |

覚え方（テストの導出式にも使う）: `AccessShare` は `AccessExclusive` とだけ、`ShareUpdateExclusive` は自分自身・`Share` 以上と、`Share` は `RowExclusive` 以上（自分自身を除く）と、`Exclusive` は `AccessShare` 以外の全部と、`AccessExclusive` は全部と衝突する。

### 4.2 `LockTag`・`LockManager`（`txn/lock/mod.rs`）

契約 §4.2 の署名（`LockTag`、`LockScope`、`WaitCtl`、`WaitCtx`、`LockManager` の公開メソッド、`LockStatusRow`）をそのまま採用し、次を**追加**する。

```rust
impl LockTag {
    /// DETAIL の記述（付録 A の E14〜E16）。xid は外部表現（下位 32 ビット）で出す
    ///   Relation      → "relation {rel} of database {db}"
    ///   Tuple         → "tuple ({block},{offset}) of relation {rel} of database {db}"
    ///   TransactionId → "transaction {xid32}"
    ///   Database(oid) → "object {oid} of class 1262 of database 0"
    ///   Object        → "object {obj as u32} of class {class} of database {db}"
    pub fn describe(&self) -> String;
}

impl LockManager {
    /// 変異テスト用（§6.7）。new() は LockManager::with_knobs(DebugKnobs::default())
    pub fn with_knobs(knobs: DebugKnobs) -> Arc<Self>;
    /// バックエンドが保持している (タグ, モード)（診断・テスト用。待ち手は含まない）
    pub fn held_locks(&self, id: BackendId) -> Vec<(LockTag, LockMode)>;
    /// 待っていれば (タグ, モード, 待ち始めからの経過)
    pub fn waiting_on(&self, id: BackendId) -> Option<(LockTag, LockMode, Duration)>;
    /// id 以外のバックエンドが tag を待っているか（待ち行列に id 以外がいるか）。03 章の autovacuum の中断と末尾の切り詰めが使う（LK-D28）
    pub fn has_waiters(&self, id: BackendId, tag: LockTag) -> bool;
}

```

`LockStatusRow`（契約 §4.2）に次を**追加**する（`pg_locks` の列に必要）。

```rust
pub struct LockStatusRow {
    // 契約の項目: locktype, database, relation, page, tuple, transactionid, backend, pid, mode, granted
    pub classid: Option<Oid>,          // Database / Object: テーブルの OID（1262 / 1259 / 1260）
    pub objid: Option<Oid>,            // Database: データベースの OID。Object: obj の下位 32 ビット
    pub objsubid: Option<i16>,         // object のとき Some(0)
    pub waited: Option<Duration>,      // 待ち手（granted = false）のとき、待ち始めからの経過。pg_locks.waitstart の元
}
```

`LockTag` → `pg_locks` の列の対応:

| `LockTag` | `locktype` | `database` | `relation` | `page` | `tuple` | `transactionid` | `classid` | `objid` | `objsubid` |
|---|---|---|---|---|---|---|---|---|---|
| `Relation { db, rel }` | `relation` | `db`（共有カタログは 0） | `rel` | | | | | | |
| `Tuple { db, rel, block, offset }` | `tuple` | `db` | `rel` | `block` | `offset` | | | | |
| `TransactionId(xid)` | `transactionid` | | | | | `xid` の下位 32 ビット | | | |
| `Database(oid)` | `object` | 0 | | | | | 1262 | `oid` | 0 |
| `Object { db, class, obj }` | `object` | `db` | | | | | `class` | `obj` の下位 32 ビット | 0 |

**意味の固定**（契約 §4.2 の補足）:

- **登録**: `acquire` / `try_acquire` / `release*` / `holds` を呼べるのは `register_backend` 済みの `BackendId` だけ。未登録の `acquire` は `Error::internal("backend not registered with the lock manager")`、`try_acquire` は `false`、`release*` は何もしない。`register_backend` は接続ごとに 1 回（09 章の `SessionLocks::register`。同じ `id` の二重登録は内部エラー）、`unregister_backend` は `SessionLocks` の Drop が `release_all(id, Session)` の後に 1 回呼ぶ（LK-D21）。
- **スコープ**: `acquire(.., scope, ..)` は、そのスコープのカウントを 1 増やす。同じ `(タグ, モード)` を複数回取れる（Transaction と Session の両方でも）。
- **`release(id, tag, mode)`**: LK-D6 の規則で 1 減らす。持っていないモードの `release` は `debug_assert!` に失敗する（リリースビルドでは何もしない）。
- **`release_all(id, Transaction)`**: Transaction スコープのカウントだけ 0 にする。Session スコープのカウントは残す。**`release_all(id, Session)`**: 両方を 0 にする（接続の終わり）。どちらも、解放の後で影響を受けたタグごとに待ち手を起こす（§5.3）。
- **`holds(id, tag, mode)`**: 両スコープのどちらかのカウントが 1 以上なら `true`。待ち手としての要求は含まない。
- **`try_acquire`**: `acquire` と同じ判定（§5.1。割り込みで待たずに付与できる場合も `true`）を行い、待たなければならないなら**何も変えずに** `false`。早期デッドロックの場合も `false`。
- **`unregister_backend`**: 保持が残っていればデバッグビルドで panic（`std::thread::panicking()` のときは検査しない）。
- **`blocking_backends(id)`**: 待っていなければ空。待っていれば、(1) 待っているモードと衝突するモードを保持している**他の**バックエンド（hard）、(2) 待ち行列で自分より前にいて、待っているモードが自分の待っているモードと衝突するバックエンド（soft。(1) に含まれるものは除く）の順に、重複なしで返す（PostgreSQL の `pg_blocking_pids` と同じ【確認】PG:src/backend/utils/adt/lockfuncs.c）。既に付与された（`granted` が立った）が、まだ目覚めていない待ち手は「待っていない」として扱う。
- **`is_blocked_by(id, among)`**: `blocking_backends(id)` と `among` が交わるか。
- **`lock_status`**: ロック表の一貫したスナップショット（`Mutex` の中で作る）。行の並びは `(database, relation, page, tuple, transactionid, backend, mode, granted の降順)` の昇順で決定的にする。保持者は保持しているモードごとに 1 行、待ち手は 1 行。

### 4.3 待ちの引数（`WaitCtl`・`WaitCtx`）の作り方

契約 §4.2 の型をそのまま使う。Session は文の実行の前に次のように作る（`session/locking.rs`）。

```rust
impl Session {
    /// lock_timeout = 0 は None。deadlock_timeout は Settings の値（既定 1s）
    pub(super) fn wait_ctl(&self) -> WaitCtl<'_> {
        WaitCtl { lock_timeout: self.settings.lock_timeout().filter(|d| !d.is_zero()),
                  deadlock_timeout: self.settings.deadlock_timeout(),
                  interrupts: &self.interrupt }
    }
}
```

`WaitCtl.lock_timeout` は**1 回の `acquire` の待ち**の上限（LK-D4）。`statement_timeout` は `interrupts` の期限として効く（Session が文の開始時に `set_statement_deadline` を呼ぶ。M3 §5.2 a）。**ロックの待ちも `statement_timeout` の対象**（PostgreSQL と同じ。実機 E12）。

### 4.4 `ProcArray`・`TxnManager`・`RegisteredSnapshot`（`txn/`）

契約 §4.3 の署名を採用し、次を決める。

```rust
// txn/mod.rs
impl Snapshot {
    /// 自分のトランザクションの XID か。可視性判定は own_xid との == を直接書かない（D2）
    pub fn is_own(&self, x: Xid) -> bool { self.own_xid == Some(x) }
}

// txn/proc_array.rs（LK-3）
/// 実行中の XID、XID の採番、スナップショットの登録簿。内部は 1 本の Mutex（以下 P）。
/// P は葉ロック（§5.11）。中で I/O をしない（XID の先取りで制御ファイルを更新する以外）
#[derive(Debug)]
pub struct ProcArray { /* inner: Mutex<ProcState>、hint: AtomicU64（oldest_xmin_hint） */ }
// `RunningXids`（03 章 `storage/heap/prune.rs` のトレイト）の実装は 03 章が prune.rs に書く（trait はそちらにあり、`txn` は `storage` を use できない）。
// LK が用意するのは固有メソッド is_in_progress / next_xid / oldest_xmin / oldest_xmin_hint
#[derive(Debug)]
struct ProcState {
    next_xid: Xid,
    xid_limit: Xid,                         // この未満は制御ファイルに記録済み（M3 §6.7.3 の先取り）
    running: BTreeSet<Xid>,                 // 実行中（XID を持つ）トランザクション
    snap_xmins: BTreeMap<Xid, u32>,         // 登録済みスナップショットの xmin → 数
}
impl ProcArray {
    pub fn new(next_xid: Xid) -> Arc<ProcArray>;                       // StorageStack::new が呼ぶ
    pub fn is_in_progress(&self, xid: Xid) -> bool;
    /// min(running の最小, snap_xmins の最小, next_xid)（D11）。計算のたびに oldest_xmin_hint を fetch_max で進める
    pub fn oldest_xmin(&self) -> Xid;
    /// oldest_xmin の下限の写し（03 章 C5）。ロックなし（AtomicU64 の load）。単調に増える（新しいスナップショットの xmin は常に現在の horizon 以上なので、
    /// horizon は減らない）。P の中で、commit / abort の finish と RegisteredSnapshot の Drop のたびにも更新する（古すぎない下限にする）
    pub fn oldest_xmin_hint(&self) -> Xid;
    pub fn snapshot(&self, own: Option<Xid>, curcid: CommandId) -> Snapshot;                    // 登録しない
    pub fn take_snapshot(self: &Arc<Self>, own: Option<Xid>, curcid: CommandId) -> RegisteredSnapshot;
    pub fn next_xid(&self) -> Xid;
    pub fn xid_counters(&self) -> (Xid, Xid);                          // (next_xid, xid_limit)。チェックポイント用
    pub fn running_xids(&self) -> Vec<Xid>;                            // 追加。テスト・診断用
    /// TxnManager からだけ呼ぶ（crate 内）
    pub(crate) fn assign(&self, backend: BackendId, locks: &LockManager, clog: &Clog, control: &ControlFileHandle) -> Result<Xid>;
    pub(crate) fn finish(&self, xid: Xid, status: XidStatus, clog: &Clog) -> Result<()>;
}

// txn/snapshot.rs（LK-3）
/// 登録済みスナップショット。Drop で登録が外れ、horizon の計算から消える。Clone できない（登録は 1 つ）
#[derive(Debug)]
pub struct RegisteredSnapshot { /* Snapshot, Arc<ProcArray>, reg_xmin: Xid */ }
impl std::ops::Deref for RegisteredSnapshot { type Target = Snapshot; /* … */ }
impl RegisteredSnapshot {
    /// Repeatable Read の文用: 登録したスナップショットの xmin / xmax / xip を保ち、own_xid と curcid だけを差し替えた
    /// **登録されない**コピー。own_xid を差し替える理由: トランザクションの最初のスナップショットを取った後で XID を
    /// 割り当てると、自分の変更が見えなくなる。差し替えたコピーを使う間、元の RegisteredSnapshot を生かしておくこと（追加）
    pub fn for_statement(&self, own: Option<Xid>, curcid: CommandId) -> Snapshot;
}
impl Drop for RegisteredSnapshot { /* P の snap_xmins[reg_xmin] を 1 減らす。0 なら消す。I/O なし。lock_ignore_poison */ }
```

```rust
// txn/manager.rs（LK-3）
#[derive(Debug)]
pub struct TxnManager { /* procs: Arc<ProcArray>, clog, control, wal, locks: Arc<LockManager>, multixact: Arc<MultiXactTable>,
                           barrier: RwLock<()>, commit_gate: RwLock<()>, knobs: DebugKnobs */ }

impl TxnManager {
    /// 契約 §4.3 の引数に multixact を足す（§11-4）
    pub fn new(procs: Arc<ProcArray>, clog: Arc<Clog>, control: Arc<ControlFileHandle>, wal: Arc<Wal>,
               locks: Arc<LockManager>, multixact: Arc<MultiXactTable>, knobs: DebugKnobs) -> Arc<Self>;
    pub fn assign_xid(self: &Arc<Self>, backend: BackendId) -> Result<(Xid, XidGuard)>;
    pub fn take_snapshot(self: &Arc<Self>, own: Option<Xid>, curcid: CommandId) -> RegisteredSnapshot;
    pub fn snapshot(&self, own: Option<Xid>, curcid: CommandId) -> Snapshot;       // 登録しない（LK-D9）
    pub fn oldest_xmin(&self) -> Xid;
    pub fn is_in_progress(&self, xid: Xid) -> bool;
    pub fn commit(&self, xid: Xid, dropped: &[RelFileLocator]) -> Result<()>;      // §5.8 の手順 1。ロックは解放しない
    pub fn abort(&self, xid: Xid, created: &[RelFileLocator]) -> Result<()>;
    /// XID を持たないトランザクションの終わり（D48）。flush_upto != 0 なら wal.flush(flush_upto)。clog・実行中一覧は触らない
    pub fn finish_without_xid(&self, flush_upto: Lsn) -> Result<()>;               // M4 §14.1 の口。追加ではなく M4 の署名
    pub fn commit_gate_exclusive(&self) -> Result<GateWrite<'_>>;                  // M3 のまま
    pub fn statement_barrier(&self) -> Result<BarrierRead<'_>>;                    // チェックポイントの書き出しだけが持つ（LK-D23）
    pub fn exclusive_barrier(&self) -> Result<BarrierWrite<'_>>;                   // コミット / アボートの unlink が持つ
    pub fn procs(&self) -> &Arc<ProcArray>;                                        // 追加
    /// テスト用（04 章 C12）: 登録中のスナップショットの数
    #[cfg(any(test, feature = "testing"))] pub fn registered_snapshot_count(&self) -> usize;
    pub fn locks(&self) -> &Arc<LockManager>;
    pub fn multixact(&self) -> &Arc<MultiXactTable>;
    pub fn next_xid(&self) -> Xid;  pub fn xid_counters(&self) -> (Xid, Xid);  pub fn clog(&self) -> &Arc<Clog>;   // M3 のまま
}

/// 契約 §4.3 のとおり。Drop の安全網は「XID がまだ実行中一覧にいれば」だけを条件にする（commit / abort との握手は要らない）
#[derive(Debug)]
pub struct XidGuard { /* Arc<TxnManager>, BackendId, xid */ }
impl XidGuard { pub fn xid(&self) -> Xid; pub fn backend(&self) -> BackendId; }
impl Drop for XidGuard {
    /// 1. P の中で、running にいれば clog を Aborted（CAS）にして running から外す（メモリのみ）。いたなら WARNING
    ///    "transaction {xid} was dropped without commit or abort; aborted in memory"（M3 の WriterGuard と同じ文言）
    /// 2. 1 で外した場合に限り locks.release_all(backend, Transaction) と multixact.on_xact_end(xid)
    /// （正常な commit / abort の後なら 1 は何もせず、ロックは Session が §5.8 の手順で解放済み）
}

// Transaction（契約 §4.3。M3 の writer を guard に置き換える）
pub struct Transaction { pub xid: Option<Xid>, pub guard: Option<XidGuard>, pub isolation: IsolationLevel,
                         pub xact_snapshot: Option<RegisteredSnapshot>, /* M3・M4 の項目はそのまま */ }
impl Transaction { pub fn owns_xid(&self, x: Xid) -> bool { self.xid == Some(x) } }
```

**`Clog`**（`txn/clog.rs`。公開署名は M3 のまま。LK-D10）:

```rust
struct ClogPage { data: Box<[AtomicU8; PAGE_SIZE]>, dirty: AtomicBool }
pub struct Clog { vfs, pages: RwLock<HashMap<u64, ClogPage>>, flush_lock: Mutex<()> }
// status(xid): read(pages) の中でページを引き、data[byte].load(Acquire)。ページがメモリになければ read を外して読み込み、
//              write(pages) で「なければ入れる」。read のロックを持ったままディスクを読まない
// ensure_page_for(xid): P の中から呼ばれる。read で見て無ければ write で作る（32768 XID に 1 回だけ）
// set_status(xid, s): read(pages) の中で CAS ループ。現在の 2 ビットが 0 でなければ内部エラー（M3 と同じ）。成功したら dirty.store(true, Release)
// flush: 各ページで dirty.swap(false) → バイトを Acquire で読んで書く（書き込み中に立つビットは次の flush で拾う）
// truncate_before / sweep_old_segments / oldest(): VC-4 の持ち分（03 章 §6.5。write(pages) の中で落とす）。LK-3 は構造体に `oldest: AtomicU64` を置き、
//              `Clog::open(vfs, next_xid, oldest)` の署名にしておく（03 章 C6。LK-3b が先、VC-4 がその上に足す。同じファイルなので順序を守る）
```

### 4.5 `backend.rs`

契約 §4.1 の署名（`BackendId`、`BackendInfo`、`BackendRegistry::{new, register, count_in_db, count_of_role, by_pid, by_id, all}`、`BackendGuard`）を採用し、次を**追加**する。

```rust
impl BackendRegistry {
    /// 接続数の制限の検査と登録を 1 つの Mutex の中で行う（検査してから登録する間に別の接続が入らない）。
    /// admit が Err なら登録しない。AU・DB は register の後に数える（09 章 §5.1）か、これを使う（どちらでもよい）
    pub fn register_if(self: &Arc<Self>, info: BackendInfo, admit: &dyn Fn(&[BackendInfo]) -> Result<()>) -> Result<BackendGuard>;
}
impl BackendGuard { pub fn id(&self) -> BackendId; pub fn info(&self) -> BackendInfo; }
impl Drop for BackendGuard {
    /// 登録簿から外すだけ。LockManager には触れない（LockManager への登録と、Session スコープのロックの解放は
    /// 09 章の SessionLocks が持つ。LK-D21）
}
```

`BackendRegistry` は `LockManager` を知らない。`LockManager` の `register_backend` / `unregister_backend` / `release_all(Session)` は 09 章の `SessionLocks`（`register` と Drop）が呼ぶ。`pid → BackendId` の解決（`by_pid`）は `BackendRegistry`、ロックの待ちの状態は `LockManager` が持つ。

### 4.6 `session/locking.rs`（LK-4）

```rust
/// 生のパース木から作る 1 件のロック要求（§5.9）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockRequest { pub target: LockTarget, pub mode: LockMode, pub nowait: bool, pub lock_table: bool }
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockTarget {
    /// 既存のリレーション（表・シーケンス）。名前は検索パスで解決する。見つからなければ飛ばす（LK-D13）
    Relation { schema: Option<String>, name: String, span: Span },
    /// DROP INDEX: インデックスの名前を引き、持ち主の表をロックする
    IndexOwner { schema: Option<String>, name: String, span: Span },
    /// 名前予約ロック（CREATE が作る名前。mode は Exclusive）
    NewName { schema: Option<String>, name: String },
    /// 第 2 段（LK-D16）を取る印。直前の Relation / IndexOwner の解決結果に対して行う。
    /// constraint = None: DROP TABLE（その表の全 FK の相手と、列が所有するシーケンス）。Some(name): ALTER TABLE ... DROP CONSTRAINT name（name が FK のときだけ、その相手）
    DropPeers { constraint: Option<String> },
}

/// 生のパース木だけから決まる純粋な関数（Cluster なしでテストできる）。解析順に並べ、同じ要求は 1 つにまとめる
pub fn collect_lock_requests(stmt: &Statement) -> Vec<LockRequest>;

/// 名前引きの窓口。Session の実装は「呼ぶたびに最新のカタログ用スナップショット（自分の変更を含む。D12）を取り、
/// キャッシュの世代を見て読み直す」（取ったスナップショットはこの呼び出しの間だけ登録する。LK-D9）
pub trait RelationResolver {
    fn lookup(&mut self, schema: Option<&str>, name: &str) -> Result<Option<ResolvedRel>>;
    fn index_owner(&mut self, schema: Option<&str>, name: &str) -> Result<Option<ResolvedRel>>;
    /// CREATE の名前空間。schema が明示されて存在しなければ 3F000 `schema "x" does not exist`
    fn creation_namespace(&mut self, schema: Option<&str>) -> Result<Oid>;
    /// 第 2 段: rel が持つ FK の相手の表の OID（constraint が Some ならその制約が FK のときだけ）。constraint が None のときは、rel の列が所有するシーケンスの OID も含む（FK 章の `pg_constraint` / `pg_depend` と、M4 の `TableDef.identity_seqs` から）
    fn drop_peers(&mut self, rel: Oid, constraint: Option<&str>) -> Result<Vec<Oid>>;
    /// ロックを取る前に、解決のたびに 1 回呼ぶ（PG の RangeVarGetRelidExtended の callback と同じ位置）。権限などの検査。
    /// 08 章 C5: `pg_authid`（OID 1260）の SELECT はスーパーユーザー以外 42501。ほかは Ok(())。既定の実装は何もしない
    fn check_access(&mut self, rel: &ResolvedRel, mode: LockMode) -> Result<()> { Ok(()) }
    /// カタログの世代（`DatabaseHandle.cache.generation()`）。lock_relation が「待っている間に何かコミットされたか」を見る
    fn generation(&self) -> u64;
}
#[derive(Clone, Debug)]
pub struct ResolvedRel { pub oid: Oid, pub kind: RelKind, pub shared: bool, pub schema: String, pub name: String }

/// 取れたロック。LK-D12 の検証と、準備済み文（D19・D21）の「参照リレーションの一覧」に使う
#[derive(Clone, Debug, Default)]
pub struct LockSet { pub entries: Vec<(RelKey, LockMode)> }
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RelKey { pub db: Oid /* 共有カタログは 0 */, pub rel: Oid }

impl Session {
    /// §5.9 の手順 2。待ちのエラー（55P03 / 40P01 / 57014 / 57P01）、LOCK TABLE の 42809、3F000、check_access の 42501 を返す
    pub(super) fn acquire_statement_locks(&mut self, reqs: &[LockRequest]) -> Result<LockSet>;
    /// 04 章 C7 の口: `acquire_statement_locks(&collect_lock_requests(stmt))` の結果を (リレーションの OID, モード) の一覧にしたもの。
    /// Simple Query の文の実行・Bind・Describe（文）が共有する。名前の解決は毎回行う（OID でロックする最適化は M6）
    pub(super) fn lock_statement_relations(&mut self, stmt: &Statement) -> Result<Vec<(Oid, LockMode)>>;
    /// LK-D12 の検証: bound が要るロック（required_locks）のうち、満たされていないもの（§5.9 B の判定。LockManager::holds を直接引く）。
    /// 空でなければ acquire_oids で取り、アナライズからやり直す。04 章の Bind も同じ関数を使う
    pub(super) fn missing_locks(&self, bound: &BoundStatement) -> Vec<(RelKey, LockMode)>;
    /// OID で直接取る（名前解決しない）。missing_locks の結果を取るのに使う。04 章は M5 では名前で取る（lock_statement_relations）ので、準備済み文が LockSet を持つ最適化は M6
    pub(super) fn acquire_oids(&mut self, items: &[(RelKey, LockMode)]) -> Result<()>;
    /// Bound が参照するリレーションと必要なモード（§5.9 の規則）の一覧。LK-D12
    pub(super) fn required_locks(bound: &BoundStatement) -> Vec<(RelKey, LockMode)>;
    /// §5.8 の手順 4: トランザクションスコープのロックを全部解放し、xact_snapshot を外す
    pub(super) fn release_transaction_resources(&mut self);
    /// 10 章 CR-4: 分離性ランナーのアクタースケジューラが is_blocked_by を呼ぶのに使う
    pub fn backend_id(&self) -> BackendId;
    pub fn backend_pid(&self) -> i32;               // M3 から（pid = session_id の i32）
}
```

`RelKind` は M4 の列挙（`Table` / `Index` / `Sequence`）に `Virtual`（`'v'`）を足したもの（§11-12）。

### 4.7 仮想リレーション（`catalog/virtual_rel.rs`、契約 §4.10）

契約の `VirtualRelation` / `registry()` をそのまま使い、`VirtualCtx` の中身を決める。

```rust
pub struct VirtualCtx<'a> {
    pub cluster: &'a Cluster,
    pub db_oid: Oid,
    pub backend: BackendId,
    pub role: &'a RoleRow,
    pub runtime: &'a dyn RuntimeInfo,      // statement_timestamp()（waitstart の計算に使う）
}

#[derive(Debug)] pub struct PgLocks;       // oid 9811。rows() は cluster.lock_manager().lock_status() から作る
impl VirtualRelation for PgLocks { /* columns は下表 */ }
```

`pg_locks` の列（PG17 と同じ名前・型・順序。【実機 E18】）:

| # | 列 | 型（OID） | 値 |
|---|---|---|---|
| 1 | `locktype` | text (25) | §4.2 の表 |
| 2 | `database` | oid (26) | 同上 |
| 3 | `relation` | oid | 同上 |
| 4 | `page` | int4 (23) | `Tuple` の `block`（u32 は i32 に丸める。PG も int4） |
| 5 | `tuple` | int2 (21) | `Tuple` の `offset` |
| 6 | `virtualxid` | text | 常に NULL |
| 7 | `transactionid` | xid (28) | `Datum::Xid(xid.to_external())` |
| 8 | `classid` | oid | 同上 |
| 9 | `objid` | oid | 同上 |
| 10 | `objsubid` | int2 | 同上 |
| 11 | `virtualtransaction` | text | 常に NULL |
| 12 | `pid` | int4 | `BackendInfo.pid` |
| 13 | `mode` | text | `LockMode::pg_name()` |
| 14 | `granted` | bool (16) | |
| 15 | `fastpath` | bool | 常に false |
| 16 | `waitstart` | timestamptz (1184) | 待ち手のとき `statement_timestamp − waited`、保持者は NULL |

### 4.8 設定（`settings.rs`。契約 §3.6）

| 名前 | 型・既定 | 検査 | 備考 |
|---|---|---|---|
| `deadlock_timeout` | 時間・`1s`（内部は ms） | 1〜2147483647 ms。範囲外は `22023 0 ms is outside the valid range for parameter "deadlock_timeout" (1 ms .. 2147483647 ms)`（【実機 E19】。`{n} ms` の `{n}` は入力を ms に直した値）。単位なしの数は ms。`SHOW` は `1s`・`300ms`・`1500ms` の形（`statement_timeout` と同じ整形） | 誰でも `SET` できる（契約 §3.6）。`Settings::deadlock_timeout() -> Duration` を足す |
| `lock_timeout` | 時間・`0`（無効） | M3 のまま | **意味を変える**: 「1 回のロック待ち」（M3 は書き込みロックの待ちだけ）。`WaitCtl.lock_timeout` に渡す |
| `max_locks_per_transaction` | 整数・64 | **`SHOW` だけ**。`SET` は PG17 と同じく `55P02 parameter "max_locks_per_transaction" cannot be changed without restarting the server`（context = postmaster。【実機 E19】。契約 §3.6 の「保存だけ」を、`SET` が通らない形に具体化する） | `53200` は起こさない。値は固定の 64 |
| `log_lock_waits` | bool・`off` | 保存するだけ（`INERT_GUCS`。PG17 は superuser のみ `SET` 可だが、yuzhu に権限はない） | M5 にサーバログがない |

### 4.9 `RuntimeInfo` の追加（`executor/mod.rs`）

M3 の `is_blocked_by(pid, among)` は意味を保ち（実装を `LockManager` に委ねる）、次を**追加**する。M4 §14.3 の追加メソッドとは独立。

```rust
pub trait RuntimeInfo: std::fmt::Debug {
    /* M3: backend_pid, is_blocked_by, check_interrupts。M4: transaction_timestamp ほか */
    fn blocking_pids(&self, pid: i32) -> Vec<i32>;       // pg_blocking_pids(int4)。pg → BackendId → blocking_backends → pg
}
```

`pg_blocking_pids(int4) -> int4[]` は `FnKind::Runtime` の組み込み関数として `pg_proc` に足す（OID **2561**、引数 `{23}`、戻り値 `1007`、`provolatile = 'v'`、`proparallel = 's'`。【実機 E18】。`pg_isolation_test_session_is_blocked` は OID 3378 のまま）。戻り値は `Datum::Array`（`elem_type = 23`、要素なしなら `dims` が空）。

---

## 5. 処理の流れ

### 5.1 ロック要求の判定（`LockTable::request`。`txn/lock/table.rs`）

PostgreSQL 17 の `LockAcquireExtended`（【確認】PG:src/backend/storage/lmgr/lock.c）と `ProcSleep`（PG:src/backend/storage/lmgr/proc.c）を、ロック表 1 本の上に移植する。`acquire` も `try_acquire` もこの関数を呼ぶ（`dont_wait` の違いだけ）。**`Mutex` を持っている間に呼ぶ。**

```rust
enum Request { Granted, Queued, NotAvailable, Deadlock(Vec<Hop>) }

fn mask(m: LockMode) -> u8 { CONFLICT_MASK[m as usize - 1] }

fn request(&mut self, id: BackendId, tag: LockTag, mode: LockMode, scope: LockScope, dont_wait: bool) -> Request {
    let e = self.locks.entry(tag).or_default();

    // (1) 同じ (タグ, モード) を既に持っている（どちらのスコープでも）: 待ち行列を見ずにカウントを増やす（PG の locallock の nLocks > 0）
    if e.holder(id).is_some_and(|h| h.has(mode)) { e.holder_mut(id).add(mode, scope); return Granted; }

    // (2) 待ち手のモードと衝突するなら、保持者と衝突しなくても待ち行列に入る（FIFO）。そうでなければ保持者との衝突を調べる
    let queued_conflict = !self.knobs.lock_disable_fifo && mask(mode) & e.wait_mask(&self.backends) != 0;       // 変異スイッチ: §6.7
    let conflict = queued_conflict || e.check_conflicts(id, mode);                                         // check_conflicts は lock_ignore_conflicts で常に false
    if !conflict { e.grant(id, mode, scope); return Granted; }

    // (3) ProcSleep: 自分がこのロックを何か持っているときだけ、割り込む位置を探す
    let my_held = e.hold_mask(id);                       // 自分が保持しているモードのビット集合
    let mut insert_at = None;                            // None = 末尾
    if my_held != 0 && !e.queue.is_empty() && !self.knobs.lock_disable_breakin {
        let mut ahead = 0u8;                             // 自分より前に残る待ち手のモードの和
        for (i, &q) in e.queue.iter().enumerate() {
            let qmode = self.backends[&q].waiting_mode();
            if mask(qmode) & my_held != 0 {              // 「q は私を待っている」（q の要求が私の保持と衝突する）
                if mask(mode) & e.hold_mask(q) != 0 {    // 「私も q を待つ」（私の要求が q の保持と衝突する）→ 早期デッドロック
                    return Deadlock(vec![Hop { waiter: id, tag, mode }, Hop { waiter: q, tag, mode: qmode }]);
                }
                // q の前に入る。私の要求が先行者（ahead）と保持者のどちらとも衝突しないなら、待たずに付与する
                if mask(mode) & ahead == 0 && !e.check_conflicts(id, mode) { e.grant(id, mode, scope); return Granted; }
                insert_at = Some(i);
                break;
            }
            ahead |= qmode.bit();                        // q の前には入れない（q は私を待っていない）
        }
    }
    if dont_wait { return NotAvailable; }                // エントリが空なら消す
    e.enqueue(id, insert_at);                            // 待ち手の情報（タグ、モード、スコープ、開始時刻）は backends[id].waiting に置く
    Queued
}
```

- `check_conflicts(id, mode)`: `mask(mode)` の各モード m について、`granted[m]`（m を保持している**バックエンド数**）から「自分が m を保持していれば 1」を引いた値が 1 以上なら衝突。**同じバックエンドの保持同士は衝突しない**（契約 §4.2）。
- `wait_mask`: `queue` の各待ち手のモードのビットの和（待ち手は短いので毎回計算してよい）。
- `grant(id, mode, scope)`: 保持者がいなければ作り、そのモードが初めてなら `granted[mode] += 1`、スコープのカウントを 1 増やし、`backends[id].held` にタグを入れる。
- 早期デッドロックは待ち行列に入る前に返すので、呼び出し側は**何も取り下げない**。エラーの組み立ては §5.4。
- 実機で確認した振る舞い（付録 A）: E7（A が `AccessShare` を持ち B が `AccessExclusive` を待つとき、A の `RowExclusive` は**待たずに付与**される。A と B の両方が `AccessShare` を持ち B が `AccessExclusive` を待つとき、A の `AccessExclusive` は**即座に** 40P01）、E8（B の `AccessExclusive` の後ろの C の `AccessShare` は、A と衝突しなくても待つ）。

### 5.2 待ちのループ（`acquire`。`txn/lock/wait.rs`）

```rust
const LOCK_WAIT_POLL: Duration = Duration::from_millis(20);

pub fn acquire(&self, id, tag, mode, scope, wait: &WaitCtl<'_>) -> Result<()> {
    assert_may_wait(WaitSite::Acquire);                         // デバッグビルドだけ。待たなくても検査する（LK-D22）
    let mut t = lock(&self.inner)?;
    match t.request(id, tag, mode, scope, false) {
        Granted => return Ok(()),
        Deadlock(hops) => return Err(deadlock_error(&t, &hops)),   // §5.4
        Queued => {}
        NotAvailable => unreachable!(),
    }
    let cv = t.backend(id)?.cv.clone();                         // Arc<Condvar>
    let started = Instant::now();
    let deadlock_at = started + wait.deadlock_timeout;
    let timeout_at = wait.lock_timeout.map(|d| started + d);
    let mut deadlock_checked = false;
    loop {
        let now = Instant::now();
        let mut slice = LOCK_WAIT_POLL;
        if !deadlock_checked { slice = slice.min(deadlock_at.saturating_duration_since(now)); }
        if let Some(at) = timeout_at { slice = slice.min(at.saturating_duration_since(now)); }
        let (g, _) = wait_timeout(&cv, t, slice.max(Duration::from_millis(1)))?;
        t = g;
        // (1) 付与が最優先（解放者が付与・待ち行列からの除去・notify を済ませている）
        if t.take_granted(id) { return Ok(()); }
        // (2) 停止 → キャンセル → statement_timeout（InterruptFlag::check の優先順）
        if let Err(e) = wait.interrupts.check() { t.cancel_wait(id); return Err(e); }
        // (3) lock_timeout（1 回の acquire の待ち）
        if timeout_at.is_some_and(|at| Instant::now() >= at) { t.cancel_wait(id); return Err(lock_timeout_error()); }
        // (4) deadlock_timeout が過ぎたら、1 回だけデッドロックを検査する
        if !deadlock_checked && Instant::now() >= deadlock_at {
            deadlock_checked = true;
            if !self.knobs.deadlock_detection_off {
                if let Some(hops) = deadlock::find_cycle(&t, id) { t.cancel_wait(id); return Err(deadlock_error(&t, &hops)); }
            }
        }
    }
}
```

- `lock_timeout_error()` は `55P03 canceling statement due to lock timeout`（契約 §3.8）。`NOWAIT` の `55P03 could not obtain lock on relation "t"` は `try_acquire` の失敗を呼び出し側（`lock_relation`。§6.3）が組み立てる。
- `cancel_wait(id)`: `queue` から自分を外し、`waiting = None` にし、**そのタグで `wake_waiters` を呼ぶ**（先頭の待ち手が消えると後ろが付与できるようになる）。エントリが空なら消す。付与済み（`granted` が立っている）なら何もしない。
- (1) を (2) より先にするのは、付与と期限切れが同時に起きたとき、付与を捨てないため（取れたロックを持ったままエラーを返すとロックが漏れる）。
- `statement_timeout` と `lock_timeout` が同じ周期で両方切れたら `interrupts.check()` の側（`statement_timeout`）が勝つ（LK-D25）。
- ループは `LockManager` の `Mutex` を `Condvar::wait_timeout` の間だけ手放す。`interrupts.check()` は `InterruptFlag` の葉の `Mutex`（`deadline`）を `Mutex` を持ったまま取るが、`InterruptFlag` は何も取らないので順序は一方向（§5.11）。

### 5.3 解放と起こし（`release`・`release_all`・`wake_waiters`）

```rust
fn release_one(&mut self, id, tag, mode)  // Transaction スコープのカウントを優先して 1 減らす。両方 0 になったら granted[mode] -= 1、モードが空なら保持者を消す
fn release_scope(&mut self, id, scope)    // release_all の本体。id が held に持つタグごとに、スコープのカウントを 0 にする（Session は両方）

/// PG の ProcLockWakeup: 待ち行列を先頭から見て、(a) 先行する未付与の待ち手のモードと衝突せず、(b) 保持者とも衝突しない待ち手を付与する
fn wake_waiters(&mut self, tag: LockTag) {
    let Some(e) = self.locks.get_mut(&tag) else { return };
    let mut ahead = 0u8;
    for q in e.queue.clone() {
        let (qmode, qscope) = self.backends[&q].waiting_mode_scope();
        if mask(qmode) & ahead == 0 && !e.check_conflicts(q, qmode) {
            e.grant(q, qmode, qscope);                 // 付与は解放者がやる（待ち手は目覚めて確かめるだけ）
            e.queue.retain(|x| *x != q);
            self.backends[&q].set_granted();
            self.backends[&q].cv.notify_one();
        } else {
            ahead |= qmode.bit();                      // 付与できない待ち手は、後ろの待ち手を止める
        }
    }
    // 保持者も待ち手もいなければエントリを消す
}
```

- `release` は「解放 → そのタグで `wake_waiters`」、`release_all` は「全タグで解放 → 影響を受けた各タグで `wake_waiters`」。`Mutex` は 1 回の呼び出しの間ずっと持つ（途中で他者が割り込まない）。
- `notify_one` は付与した待ち手の `Condvar` にだけ送る（待ち手ごとに専用の `Condvar`）。
- 起こされた待ち手は §5.2 の (1) で `granted` を見て戻る。待ち手が既に期限切れで `cancel_wait` に入っていても、`granted` を先に見る（§5.2）ので取れたロックは漏れない。

### 5.4 デッドロック検出（`txn/lock/deadlock.rs`）

**検査の起動**: 待ち手が `deadlock_timeout` 待っても付与されなければ、その待ち手自身が、`Mutex` を持ったまま 1 回だけ `find_cycle` を呼ぶ（§5.2 の (4)）。

```rust
pub(super) struct Hop { pub waiter: BackendId, pub tag: LockTag, pub mode: LockMode }

/// start から出発して start に戻る循環を探す。見つかったら、循環上の各待ち手 (waiter, 待っているタグとモード) を start から順に返す
pub(super) fn find_cycle(t: &LockTable, start: BackendId) -> Option<Vec<Hop>> {
    let mut visited = HashSet::new();                    // 一度訪れたバックエンドは二度訪れない（PG の visitedProcs）
    let mut path = Vec::new();
    return if dfs(t, start, start, &mut visited, &mut path) { Some(path) } else { None };

    fn dfs(t, cur, start, visited, path) -> bool {
        if !visited.insert(cur) { return false; }        // 既に訪れた。start に戻ったのは下の for で判定済み（循環が start を通らない場合は無視）
        let Some(w) = t.backend(cur).waiting_not_granted() else { return false; };   // 待っていなければ出る辺がない
        path.push(Hop { waiter: cur, tag: w.tag, mode: w.mode });
        for b in blockers(t, cur, &w) {                  // hard 辺（保持者）→ soft 辺（先行する待ち手）の順
            if b == start { return true; }               // 循環が閉じた。path が循環そのもの
            if dfs(t, b, start, visited, path) { return true; }
        }
        path.pop();
        false
    }
}
```

- `blockers(t, cur, w)` は `blocking_backends` と同じ順序・同じ規則（§4.2）。**同じ `Vec` の並び順（保持者は登録順、待ち手は待ち行列順）で辿る**ので、同じ状態から同じ循環が決まる（DETAIL がテストで固定できる）。
- 被害者は**検査した本人**（`start`）。他の当事者は中断しない。本人の `cancel_wait` の後、本人のトランザクションがアボートして初めて他の当事者が進む（実機 E14: 40P01 を受けた A が失敗した文のアボートでロックを手放し、B が進む。LK-D11）。
- 早期デッドロック（§5.1 (3)）は 2 者の循環を直接返す。

**エラーの組み立て**（実機 E14〜E16・E7 で固定した書式。【確認】PG:src/backend/storage/lmgr/deadlock.c の `DeadLockReport`）:

```rust
fn deadlock_error(t: &LockTable, cycle: &[Hop]) -> Error {
    let detail = cycle.iter().enumerate().map(|(i, h)| {
        let next = &cycle[(i + 1) % cycle.len()];
        format!("Process {} waits for {} on {}; blocked by process {}.",
                t.pid_of(h.waiter), h.mode.pg_name(), h.tag.describe(), t.pid_of(next.waiter))
    }).collect::<Vec<_>>().join("\n");
    Error::new(sqlstate::DEADLOCK_DETECTED, "deadlock detected")
        .with_detail(detail)
        .with_hint("See server log for query details.")
}
```

固定する出力の例（`pid` と XID は実行ごとに違う。単体テストは正規表現 `Process \d+ waits for ... blocked by process \d+\.` と**行数・モード名・`describe` の書式**を固定する）:

```
# E14: 2 者、行の更新待ち（RW の heap が XID を待つ）
ERROR:  40P01: deadlock detected
DETAIL:  Process 212430 waits for ShareLock on transaction 802; blocked by process 212437.
Process 212437 waits for ShareLock on transaction 801; blocked by process 212430.
HINT:  See server log for query details.
CONTEXT:  while updating tuple (0,2) in relation "d1"      ← RW の heap が wait_for_xact の Err に付ける（LockManager は付けない）

# E15: 3 者
DETAIL:  Process 212430 waits for ShareLock on transaction 804; blocked by process 212437.
Process 212437 waits for ShareLock on transaction 805; blocked by process 212836.
Process 212836 waits for ShareLock on transaction 803; blocked by process 212430.

# E16: 2 者、リレーションロック（ブロック内の SELECT が LOCK TABLE 済みの表を待つ）
DETAIL:  Process 213835 waits for AccessShareLock on relation 16408 of database 5; blocked by process 213846.
Process 213846 waits for AccessShareLock on relation 16403 of database 5; blocked by process 213835.

# E7: 早期デッドロック（待たずに失敗。2 行とも同じタグ）
DETAIL:  Process 213898 waits for AccessExclusiveLock on relation 16403 of database 5; blocked by process 213905.
Process 213905 waits for AccessExclusiveLock on relation 16403 of database 5; blocked by process 213898.
```

- PostgreSQL は DETAIL の他に、サーバログへ各プロセスの問い合わせ文を書く。M5 にはサーバログがない（HINT の文言は PG と同じにしてある）。
- 実機 E16 では、PostgreSQL は**パース解析の中**でロックを待つため、エラーに `LINE 1: select * from b;` と位置が付く。yuzhu はロックを生のパース木の段階で取る（§5.9）ので、`LockRequest.target` が持つ `span` から `Error::with_span` で同じ位置を付けてよい（任意。共有テストは位置を比べない）。
- XID は DETAIL では**外部表現（下位 32 ビット）**で出す（`xid` 型と同じ。契約 D14 の 64 ビット XID でも `describe` は `to_external()`）。

**検査が見つけるもの・見つけないもの**:

- **誰が被害者になるか**: 循環が完成した後に最初に検査（待ち始めから `deadlock_timeout` 後）が走った当事者が循環を見つけ、その者が 40P01 になる。通常は先に待ち始めた者（実機 E14・E15 の A）。循環の完成より前に検査が済んでいた当事者は見つけられないが、**最後に待ちに入った当事者**の検査は必ず循環の完成後に走る。辺がまだ残っている限り、その者から始める深さ優先探索は到達できる全員を調べ、循環上の前の者から出る辺で `start` に戻るので、循環を見つける。そのため、循環の当事者が全員 `deadlock_timeout` を過ぎるまでには、少なくとも 1 人が 40P01 になる。
- PostgreSQL と同じ**1 回きりの検査**なので、PostgreSQL と同じ限界を持つ（待ち手が増えないまま辺だけが増える稀な場合は、次に誰かが待ちに入るまで見つからない。付与の規則で新しい保持者は待ち手と衝突するモードを持たないので、通常は起きない）。**未検証**（§9-2）。
- soft 辺を含む循環も 40P01 にする（LK-D1）。PostgreSQL なら並べ替えで解消する場合に差が出る（実機 E9。分離性テスト `deadlock-soft` は yuzhu 専用の期待）。

### 5.5 XID の終了待ち（`wait_for_xact`。`txn/lock/wait.rs`）

```rust
pub fn wait_for_xact(&self, id: BackendId, xid: Xid, still_running: &dyn Fn(Xid) -> bool, wait: &WaitCtl<'_>) -> Result<()> {
    assert_may_wait(WaitSite::WaitForXact);
    let tag = LockTag::TransactionId(xid);
    if self.holds(id, tag, LockMode::Exclusive) { return Err(Error::internal("wait_for_xact: waiting for own transaction")); }
    let mut first = true;
    loop {
        self.acquire(id, tag, LockMode::Share, LockScope::Transaction, wait)?;   // XID の持ち主が終わるまで待つ（持ち主は Exclusive）
        self.release(id, tag, LockMode::Share);                                  // 取れたらすぐ解放
        if !still_running(xid) { return Ok(()); }                                // Mutex を持たずに呼ぶ（P を取りうる）
        if !first { wait.interrupts.check()?; std::thread::sleep(Duration::from_millis(1)); }
        first = false;
    }
}
```

- `still_running` は `ProcArray::is_in_progress` を呼ぶ。コミット / アボートは「clog → 実行中一覧から外す → … → ロックの解放」の順（D38）なので、待ちから起きた時点では必ず `still_running(xid) == false` になり、1 周で終わる。2 周目に入るのは、XID ロックがまだ登録されていない（D37 で起きない）か、`XidGuard` の Drop が実行中一覧から外した直後だけ。
- XID の持ち主がいない・既に終わっていれば、`acquire` は即座に取れて 1 周で戻る。
- デッドロックの DETAIL には `ShareLock on transaction N`（`N` は外部表現）と出る。`CONTEXT`（`while updating tuple (0,2) in relation "t"` など）は呼び出し側（02 章の heap）が `Error::with_context` で付ける。`wait_for_xact` 自身は付けない。

### 5.6 XID の採番・スナップショット・horizon（`txn/proc_array.rs`、`txn/snapshot.rs`）

**`assign_xid`**（D37。契約 §4.3）:

```rust
// TxnManager
pub fn assign_xid(self: &Arc<Self>, backend: BackendId) -> Result<(Xid, XidGuard)> {
    let xid = self.procs.assign(backend, &self.locks, &self.clog, &self.control)?;
    Ok((xid, XidGuard { mgr: Arc::clone(self), backend, xid }))
}

// ProcArray::assign（P を取る）
let mut p = lock(&self.inner)?;
if p.next_xid >= p.xid_limit {                          // M3 §6.7.3: 先取り。失敗は Panic で xid_limit は変えない
    let new_limit = p.xid_limit.0 + XID_PREFETCH;
    control.update(|c| c.next_xid = c.next_xid.max(new_limit))?;        // P → 制御ファイルは許可された入れ子（M3 §5.9）
    p.xid_limit = Xid(new_limit);
}
let xid = p.next_xid;
clog.ensure_page_for(xid);                              // P → Clog の内部（M3 のまま）
// D37: 実行中一覧に載せる前に、同じ P の中で XID ロックを Exclusive で登録する（P → LockManager の内部 Mutex。§5.11 の例外）
if !locks.try_acquire(backend, LockTag::TransactionId(xid), LockMode::Exclusive, LockScope::Transaction) {
    return Err(Error::internal("transaction ID lock is already held"));    // 新しい XID に待ち手はいない。起きたらバグ
}
p.next_xid = Xid(xid.0 + 1);
p.running.insert(xid);
Ok(xid)
```

- 「実行中なのに XID ロックがない」状態が一瞬もないので、`wait_for_xact` は 1 周で終わる。
- 先取りが失敗した（制御ファイルの書き込みエラー）場合は `Severity::Panic` のまま返す（M3 と同じ）。XID は消費しない。

**スナップショット**:

```rust
// ProcArray::take_snapshot（P の中で、計算と登録を不可分に行う。LK-D8）
let mut p = lock_ignore_poison(&self.inner);            // 失敗できない（poison は panic ではなく続行）。take_snapshot は Result を返さない
let xmin = p.running.first().copied().map_or(p.next_xid, |f| f.min(p.next_xid));    // 自分の XID も running にいれば含める（PG と同じ）
let snap = Snapshot { xmin, xmax: p.next_xid,
                      xip: p.running.iter().copied().filter(|x| Some(*x) != own).collect(),    // 昇順。自分は除く
                      curcid, own_xid: own };
*p.snap_xmins.entry(xmin).or_insert(0) += 1;
RegisteredSnapshot { snap, procs: Arc::clone(self), reg_xmin: xmin }
```

- `snapshot()`（登録しない）は同じ計算で `snap_xmins` に触れない。**使ってよい場所**: 単体テストと `pg_current_snapshot` 風の診断だけ（LK-D9）。実行器・名前解決・アナライザは `take_snapshot` を使う。
- **Read Committed**: Session が文ごとに `take_snapshot` を呼び、文の終わりに Drop する。**Repeatable Read**: 最初にスナップショットを取る文で `take_snapshot` を呼んで `Transaction.xact_snapshot` に入れ、トランザクションの終わりまで保つ。各文は `xact_snapshot.for_statement(txn.xid, txn.cid)` の登録されないコピーを使う（RW-4。取り方と 40001 は 02 章）。ポータルのスナップショットは 04 章（`take_snapshot` の結果を閉じるまで持つ）。
- **カタログ用スナップショット**（D12。LK-D9）: 文ごとに、名前解決のたびと文の解析の前に `take_snapshot(self.txn.xid, self.txn.cid)`（自分の変更を含む）で取り、使い終えたら Drop する。カタログのキャッシュ（M2 §6.8.6）の世代の扱いは M2 のとおり。

**horizon**:

```rust
pub fn oldest_xmin(&self) -> Xid {                      // P の中
    let p = lock_ignore_poison(&self.inner);
    let running = p.running.first().copied().unwrap_or(p.next_xid);
    let reg = p.snap_xmins.keys().next().copied().unwrap_or(p.next_xid);
    running.min(reg).min(p.next_xid)
}
```

m5-concurrency §2 の #11・#11b・#11c（実機）との対応: RR のスナップショットは `xact_snapshot` を持つ間 horizon を止める。RC は文の外では登録がないので止めない。XID を持つトランザクションは `running` にいる間 horizon を止める。

### 5.7 ストレージバリアの縮小とチェックポイントとの関係（LK-D23）

| 使う者 | 取るもの | 目的 |
|---|---|---|
| チェックポイントのバッファ書き出し（`checkpoint.rs` 手順 4） | `statement_barrier()`（共有）。`track::barrier_acquired` / `barrier_released` で再入を検出 | 書き出し中に `unlink_storage` がファイルを消さない |
| コミット / アボートの `unlink`（DROP・TRUNCATE が置き換えたファイル、アボートで作成を取り消すファイル） | `exclusive_barrier()` | 上と対 |
| 文の実行（Session） | **何も取らない** | リレーションのロックが守る（DROP / TRUNCATE は `AccessExclusive` を**コミットまで**保持し、`unlink` はその後・ロックの解放の前に行う。§5.8。そのリレーションを読む文は、それ以前に取った `AccessShare` 以上のロックを持っているので、`AccessExclusive` は取れていない） |

- M3 の `assert_no_pins()`（文の終わり）は、スレッドローカルのピンの数を調べるだけなので、バリアの縮小の影響を受けない（呼ぶ位置は同じ。以前は「バリア解放の直後」だった）。**未検証**（§9-3）: チェックポイントのバッファ書き出し（別スレッド）が、別のセッションの `unlink`（排他バリアを待つ）と同時に走る間、`unlink` を含むコミットは書き出しの終わりまで待たされる。DROP / TRUNCATE のコミット遅延として既知の制約にする（M6 の課題）。
- `commit_gate`（`RwLock<()>`）と `checkpoint_lock` は M3 のまま。`track` に追跡を足す（§6.6）。

### 5.8 コミットとアボート（D38、D48。契約 §5.2 を詳細にする）

**`TxnManager::commit`**（`txn/manager.rs`。ロックは解放しない）:

```rust
pub fn commit(&self, xid: Xid, dropped: &[RelFileLocator]) -> Result<()> {
    let _gate = self.commit_gate.read()...;  track::gate_acquired();                  // M3 §6.6.4。ラッチを持たずに取る
    let ins = self.wal.insert(XACT_COMMIT { time, rels: dropped }, xid)?;
    if !self.knobs.skip_commit_flush { self.wal.flush(ins.end)?; }                     // 失敗は Panic
    self.procs.finish(xid, XidStatus::Committed, &self.clog)?;                         // P の中で「clog に Committed（CAS）→ running から外す」
    drop(_gate);  track::gate_released();
    self.multixact.on_xact_end(xid);                                                   // メンバー全員が終了した MultiXact を掃除（02 章）
    Ok(())
}
// abort も同じ形: XACT_ABORT を insert、created が空でなければ flush、procs.finish(xid, Aborted)、gate、multixact.on_xact_end
// finish_without_xid(flush_upto): flush_upto が 0 でなければ（knobs.skip_commit_flush でなければ）wal.flush(flush_upto)。ゲートは取らない
```

**Session の終わり**（`session/mod.rs` の `commit_transaction` / `rollback_transaction`。ここに LK が手順 4・5 を足す。`session/txn_ctl.rs` の持ち主は 02 章なので、LK が足すのは `release_transaction_resources()` の 1 行の呼び出しだけ）:

```
commit_transaction():
  if let Some(xid) = txn.xid:
     1. txn_mgr.commit(xid, &txn.pending_unlinks)            ← WAL → flush → clog → 実行中一覧。失敗は Panic（poison）
     2. txn.catalog_dirty なら cluster.invalidate_all_catalog_caches()      ← キャッシュの世代が進む
     3. pending_unlinks があれば exclusive_barrier の下で unlink_storage（失敗は WARNING）
  else if txn.wal_flush_upto != 0:
     1'. txn_mgr.finish_without_xid(txn.wal_flush_upto)      ← D48: シーケンスの WAL を flush してからロックを解放する
  4. release_transaction_resources():                         ← **最後**
       locks.release_all(backend, LockScope::Transaction)     ← XID ロックの解放で、待っていたセッションが起きる
       txn.xact_snapshot = None（登録が外れる）。txn.guard = None（XidGuard は何もしない: すでに running にいない）
  5. txn をリセット
rollback_transaction():
  if let Some(xid): 1. txn_mgr.abort(xid, &txn.pending_creates)   2. pending_creates を unlink
  3. release_transaction_resources()   4. リセット
```

- **ロックの解放が最後であること**が要点（D38）。待っていたセッションは目覚めた直後に `still_running(xid)`（`ProcArray` を見る）と、再解決（キャッシュの世代を見る）を行う。手順 1 と 2 が済んでいなければ、コミット前の状態を見てしまう。変異テスト `release_locks_before_commit_record`（§6.7）がこの順序を検査する。
- 読み取りだけのトランザクション（XID なし）も手順 4 を行う（リレーションロックを持っている）。WAL は書かない。
- **失敗した文**（M2/M3 の `report_error`）は、暗黙のトランザクションでもブロックの中でも `rollback_transaction()` をその場で呼ぶ。ブロックは `Failed` になるが、**ロックと XID はその時点で消える**（LK-D11。実機 E7）。`ROLLBACK` は何もしない。
- コミットの手順 1 が失敗（Panic）したら、Session は FATAL で終わり、`SessionLocks` の Drop が全ロックを解放する。`XidGuard` の Drop が clog をメモリ上で `Aborted` にするが、クラスタは poison 済みで以後の文は FATAL になり、再起動で REDO が正しい状態にする（M3 §5.3 と同じ扱い）。
- `TxnControl::commit_and_restart`（契約 §4.7。VACUUM・CREATE / DROP DATABASE が使う）は、`commit_transaction()` の手順 1〜5 を行い、**Session スコープのロック（`Database`）は残して**新しい暗黙のトランザクションを開始する。呼び出しの前後で `DdlCtx` を作り直す（スナップショット・catalog・XID が変わる）。実装は `session/txn_ctl.rs`（02 章）で、LK の要件はこの 1 行。

### 5.9 文のリレーションロックの取得（`session/locking.rs`。契約 §5.1 d の 1〜2 の詳細）

**全体の流れ**（契約 §5.1 d の 1〜8 に、検証と再試行の輪を足す。LK-D12）:

```
exec_data_statement(stmt):                              // SELECT / INSERT / UPDATE / DELETE / COPY / EXPLAIN / 表を対象にする DDL / LOCK TABLE
  0. LOCK TABLE がブロックの外なら 25P01（ロックより前。DdlCtx.outside_block と同じ判定）
  0'. [RR で txn.xact_snapshot == None、かつ文がスナップショットを要する] ★ ロックの前に（LK-D27。02 章 RW-D10・§5.8）:
         txn.xact_snapshot = Some(take_snapshot(txn.xid, txn.cid))   // 「最初のスナップショット」。待っている間もこれが horizon を止める
  1. reqs = collect_lock_requests(stmt)                  // 生のパース木だけから。解析順
  2. locked = acquire_statement_locks(&reqs)?            // 待つことがある。statement_timeout / lock_timeout / キャンセルが効く
     check_not_poisoned()                                // 待っている間に poison されたかもしれない
  3. 書き込む文、または FOR 句つきの SELECT で txn.xid が None なら TxnManager::assign_xid（ロックの後。LK-D24）
  4. for attempt in 0..4:                                // LK-D12 の輪
       snap  = RC: take_snapshot(txn.xid, txn.cid)（ロックの後。文の終わりまで登録）  ／  RR: xact_snapshot.for_statement(txn.xid, txn.cid)（登録されないコピー）
       cat   = take_snapshot(txn.xid, txn.cid)           // カタログ用（D12。登録する。LK-D9）
       bound = analyze(stmt, &StatementCatalog { snapshot: &cat, .. })?
       missing = required_locks(&bound) のうち locked.holds(..) でないもの
       if missing.is_empty(): break
       if attempt == 3: return Err(XX000 "could not lock all relations of the statement")
       acquire_oids(&missing)?; locked.extend(missing); スナップショットを Drop してやり直す
  5. plan → build → next() ループ（行ごとに check_interrupts）。結果は Output に溜める
  6. 文のスナップショットの登録を外す（RC）。リレーションロックと XID ロックは外さない（トランザクションの終わりまで）
  7. buffer::assert_no_pins()。成功なら CCI。暗黙のトランザクションの最後の文ならコミット（§5.8）
```

契約 §5.1 d の手順 2 の「解決 → ロック → 再解決」は `acquire_statement_locks` の中（下の C）、手順 4〜5 の「スナップショット」「カタログ用スナップショット」は上の 4 に対応する。**Read Committed ではロックはスナップショットより先**（D5）。`UPDATE` がテーブルロックを待った後で、待った後のスナップショットを取る。**Repeatable Read の最初のスナップショットだけは、PostgreSQL と同じくロックの前**（0'。02 章 RW-D10。実機: RR の最初の文がテーブルロックを待つと、待った後も相手のコミットした行は見えない）。

#### A. `collect_lock_requests` の規則

走査の順序は PostgreSQL の解析順（`parse_clause.c` / `analyze.c`）: **WITH → FROM（左から。JOIN は左・右の順。副問い合わせは再帰）→ 選択リスト → WHERE → GROUP BY → HAVING → ORDER BY → LIMIT / OFFSET の中の副問い合わせ**。集合演算は左の腕、右の腕の順。INSERT は対象の表を先に、UPDATE / DELETE は対象の表を先に取ってから FROM / USING に進む。同じ `(target, mode)` は最初の出現位置の 1 つにまとめる。AST の変種名は M4 の確定に合わせる（下の表は種類で書く）。

| 文 | 要求（この順） | 実機 |
|---|---|---|
| SELECT / VALUES / WITH / 集合演算 | 表・シーケンスは **AccessShare**。ただし FOR 句の対象は **RowShare**（下の「FOR 句」） | E1 |
| INSERT | 対象 **RowExclusive** → ソース問い合わせの表（AccessShare） | E1 |
| UPDATE | 対象 **RowExclusive** → FROM の表（AccessShare）→ SET・WHERE の式の副問い合わせ（AccessShare） | E1 |
| DELETE | 対象 **RowExclusive** → USING の表 → WHERE の副問い合わせ | E1 |
| `COPY t FROM` | t **RowExclusive**。`COPY t TO` は **AccessShare**、`COPY (query) TO` は query の表（AccessShare）。COPY の継続中もロックは文（トランザクション）の終わりまで | E1 |
| `EXPLAIN [ANALYZE] stmt` | 中の文と同じ | E1 |
| CREATE TABLE | `NewName`（表の名前）→ 明示された制約名（`CONSTRAINT x PRIMARY KEY`）の `NewName` → FK の参照先の表 **ShareRowExclusive**（自分自身を参照するなら不要） | E2 |
| DROP TABLE | 名前ごとに `Relation` **AccessExclusive** → `DropPeers { None }` | E2 |
| TRUNCATE | 表ごとに **AccessExclusive**（FK で参照されている表は 07・03 が `0A000`。D42） | E2 |
| CREATE INDEX | `Relation`（表）**Share** → `NewName`（インデックス名） | E2 |
| DROP INDEX | `IndexOwner`（持ち主の表）**AccessExclusive** | E2 |
| CREATE SEQUENCE | `NewName` | E3 |
| ALTER SEQUENCE | `Relation` **ShareRowExclusive** | E3 |
| DROP SEQUENCE | `Relation` **AccessExclusive** | E3 |
| `ALTER TABLE ... ADD PRIMARY KEY / UNIQUE` | `Relation` **AccessExclusive**（明示した制約名があれば `NewName`） | E2 |
| `ALTER TABLE ... ADD FOREIGN KEY` | 対象 **ShareRowExclusive** → 参照先 **ShareRowExclusive** | E2 |
| `ALTER TABLE ... DROP CONSTRAINT x` | 対象 **AccessExclusive** → `DropPeers { Some(x) }` | E2 |
| `ALTER TABLE ... OWNER TO` | **AccessExclusive**（M4 は何もしないが取る） | E2 |
| `LOCK TABLE` | 表ごとに指定のモード（省略は AccessExclusive）。`lock_table = true`、`nowait` | E4 |
| VACUUM / ANALYZE、ロール・データベースの DDL | **取らない**（D41。VC・AU・DB が `lock_relation` / `try_acquire` を自分で呼ぶ） | |
| BEGIN / COMMIT / SET / SHOW / CHECKPOINT / PREPARE / EXECUTE / DEALLOCATE / DISCARD | 取らない（EXECUTE の Bind は `acquire_oids`） | |

- **FOR 句**（PG の `isLockedRefname` / `transformLockingClause`）: `FOR ... ` に `OF` がなければ、**その SELECT の FROM の全表**（副問い合わせの FROM の中の表も、`pushed down` として再帰的に）が RowShare。`OF a, b` があれば、別名（なければ表名）が一致する FROM の項目の表だけ RowShare で、残りは AccessShare。`OF` が副問い合わせの別名なら、その中の FROM の全表が対象。式の中の副問い合わせ（EXISTS、IN、スカラー）の表は FOR の影響を受けず AccessShare（実機 E1: `select * from a where exists (select 1 from b) for update` は a が RowShare、b が AccessShare）。
- **取らないもの**: `RelKind::Index`・`RelKind::Virtual` の表（飛ばす。アナライザが 42809 などを返す。LOCK TABLE は §6.4）、`pg_class` などのカタログへの**内部の**読み取り（利用者の文が FROM に書いた場合は表と同じく AccessShare）。

#### B. `required_locks(bound)`（LK-D12 の検証）

`BoundStatement` から、参照するリレーションと必要なモードを A の規則で導く。`BoundSelect` の `RteKind::Table`（副問い合わせ・CTE の中も再帰）は AccessShare、`BoundSelect.locking`（RW）に含まれる `RteId` の表は RowShare。`BoundInsert.table` / `BoundUpdate.rtable[0]` / `BoundDelete.rtable[0]` は RowExclusive、`BoundUpdate.from` などの残りは AccessShare。`BoundDdl` は変種ごとに A の表のモード（`BoundDropTable` は各表 AccessExclusive、`BoundTruncate` は各表 AccessExclusive、`BoundCreateIndex` は表 Share、`BoundLockTable` は指定モード、…）。**満たされたかの判定**: 必要な `(key, mode)` は、同じ `key` で保持しているモード h のうち `mask(h) ⊇ mask(mode)`（h が衝突する相手の集合が mode のそれを含む。例: RowExclusive の保持は AccessShare の要求を満たす）ものが 1 つでもあれば満たされたとみなす（`LockManager::holds` を 8 モードについて呼ぶ）。満たされないものが `missing`。

#### C. `acquire_statement_locks`

```rust
fn acquire_statement_locks(&mut self, reqs: &[LockRequest]) -> Result<LockSet> {
    let wait = self.wait_ctl();
    let (locks, backend, db) = (self.cluster_locks(), self.backend_id(), self.db_oid());   // Arc<LockManager>、BackendId、現在のデータベースの OID
    let mut resolver = self.relation_resolver();                    // RelationResolver の実装（最新のカタログ用スナップショット）
    let mut set = LockSet::default();
    let mut last: Option<RelKey> = None;                            // DropPeers の対象
    for req in reqs {
        match &req.target {
            LockTarget::Relation { schema, name, .. } | LockTarget::IndexOwner { schema, name, .. } => {
                let is_index = matches!(req.target, LockTarget::IndexOwner { .. });
                let label = req.nowait.then(|| written_name(schema, name));          // NOWAIT のメッセージ用
                let key = lock_relation(&locks, backend, &wait,
                    &RelLockOpts { mode: req.mode, nowait_label: label.as_deref() },
                    &|| resolver.generation(),
                    &mut || {
                        let r = if is_index { resolver.index_owner(schema.as_deref(), name)? } else { resolver.lookup(schema.as_deref(), name)? };
                        let Some(r) = r else { return Ok(None) };                    // 見つからない → 飛ばす（LK-D13）
                        resolver.check_access(&r, req.mode)?;                        // 08 章 C5（pg_authid の 42501）。ロックの前
                        if req.lock_table { check_lockable(&r, name)? }              // 42809。仮想リレーションは None（no-op。LK-D17）
                        else if matches!(r.kind, RelKind::Index | RelKind::Virtual) { return Ok(None) }
                        Ok(Some((if r.shared { 0 } else { db }, r.oid)))
                    })?;
                if let Some((db, rel)) = key { set.entries.push((RelKey { db, rel }, req.mode)); last = Some(RelKey { db, rel }); }
            }
            LockTarget::NewName { schema, name } => {
                let ns = resolver.creation_namespace(schema.as_deref())?;           // 3F000
                lock_name(&locks, backend, &wait, db, ns, name)?;                   // Object { db, class: 1259, obj: name_key(ns, name) } を Exclusive
            }
            LockTarget::DropPeers { constraint } => {
                if let Some(k) = last {
                    for peer in resolver.drop_peers(k.rel, constraint.as_deref())? {
                        locks.acquire(backend, LockTag::Relation { db, rel: peer }, LockMode::AccessExclusive, LockScope::Transaction, &wait)?;
                        set.entries.push((RelKey { db, rel: peer }, LockMode::AccessExclusive));
                    }
                }
            }
        }
    }
    Ok(set)
}
```

- 1 件ずつ**取れたものを保持したまま**次を待つ（PG と同じ。途中の失敗はトランザクションのアボートが全部解放する）。
- 検索パスの解決は `RelationResolver::lookup` が現在の `search_path` で行う。`lookup` は呼ぶたびに新しいカタログ用スナップショットを取り、**使い終えたら Drop する**（LK-D9）。
- `lock_relation` の中で `resolve` は 1 回目（ロックの前）と、ロックを取った後に**世代が変わっていたとき**の 2 回目（§6.3）に呼ばれる。待ち明けに世代が変わっていれば、待たされた相手の DDL がコミットされており、そのコミットは**キャッシュの無効化の後**にロックを解放した（§5.8）ので、`resolve` は新しい状態を見る。
- **シーケンス関数の実行時ロック**（LK-D18）: `RuntimeInfo::nextval` / `currval` / `setval` の実装（session）は、シーケンスの操作の前に `locks.acquire(backend, Relation { db, rel: seq }, RowExclusive, Transaction, &wait)` を呼ぶ。保持済みなら即座に戻る（§5.1 (1)）。式の評価中はページのピン・ラッチを持たないので規約 1 を満たす（M3 §2 のとおり、スキャンは `next()` の間にピンを持ち越さない）。
- **Bind と Describe（文）**（04 章 C7。D21）: 準備済み文の SQL テキストの生のパース木に対して `lock_statement_relations`（上の 1〜2 と同じ関数）を毎回呼び、カタログの世代（準備済み文が記録している）を比べ、変わっていれば再アナライズしてから `missing_locks` で検証する（上の輪と同じ）。名前の解決は毎回行う（カタログのキャッシュが効く）。`acquire_oids` は検証で足りなかった分を取るときに使う。OID の一覧を準備済み文に持たせて名前解決を省く最適化は M6。

### 5.10 DDL のロックと名前予約（LK-D15、LK-D16）

- **名前予約ロック**: `Object { db, class: 1259, obj }`、`obj = name_key(namespace_oid, name)`（FNV-1a 64 ビット。§6.3）を Exclusive、トランザクションスコープで取る。CREATE が作る名前（表、明示したインデックス名・制約名、シーケンス）に取る（DROP には取らない。未コミットの DROP がある間の CREATE は、アナライザが古い行を見て待たずに 42P07 になる。PG も同じ。実機 E20）。**名前が同じなら（ハッシュが衝突する別の名前でも）直列になるだけ**で、エラーにはならない。ロックを取った後にアナライザが存在を検査するので、先発がコミットすれば後発は `42P07 relation "x" already exists`（`IF NOT EXISTS` は NOTICE）、先発がアボートすれば後発は成功する（PG は後発が `23505`。実機 E11。LK-D15、M5-LK-Q5）。
- **自動生成の名前**（`t_pkey`、`t_id_seq` など）はアナライザが解析の中で決めるので予約しない。同時に別のトランザクションが同じ名前を明示して作る稀な場合は、名前の重複が起きうる（**未検証**・既知の制限。§9-5）。
- **自分が作ったリレーション**には、他者が見えないのでロックしない（作成のトランザクションがコミットするまでアナライザからも見えない）。同じトランザクションの後続の文は通常どおり取る（保持済みでなければ待ちなしで取れる）。
- **DROP / TRUNCATE が消すファイル**は、コミットの手順 3（§5.8）で、その表の `AccessExclusive` を持ったまま `unlink` する。`unlink` とロックの解放の間に別の文が読み始めることはない。
- **デッドロックの組み合わせ**: DROP が表 → FK の相手の順、別のセッションが相手 → 表の順に取るとデッドロックする（PostgreSQL と同じ。検出される）。
- DDL とカタログ行の同時更新（`pg_class` の `relhasindex` などを同じ表に対して 2 つの DDL が更新する場合）は §9-1。

### 5.11 ロックの順序と、葉ロックの例外の証明（契約 §5.3）

契約 §5.3 の表は、M3 の順序を次のように変える。この章はその表のうち「葉ロックの例外（**`ProcArray` の `Mutex` → `LockManager` の内部 `Mutex`**）」を証明する。

**対象の葉ロック**: `L` = `LockManager.inner`、`P` = `ProcArray.inner`、`C` = `Clog.pages`（RwLock）、`K` = 制御ファイルの `Mutex`、`M` = `MultiXactTable` の `Mutex`、`D` = `InterruptFlag.deadline`、`R` = `BackendRegistry` の `Mutex`。

**取ってよい入れ子（A → B は「A を持ったまま B を取る」）**:

| 入れ子 | 由来 | 備考 |
|---|---|---|
| `P → K` | M3 §5.9（XID の先取り） | |
| `P → C` | M3 §5.9（`ensure_page_for`、`finish` の `set_status`） | |
| **`P → L`** | **D37（この章で足す）** | `assign` の `try_acquire` だけ。待たない（新しい XID に待ち手はいない）ので `L` の取得は短い |
| **`L → D`** | この章（§5.2） | `interrupts.check()` を `L` を持ったまま呼ぶ。`D` は何も取らない葉 |
| `commit_gate → P → C` | M3 §5.9 | `commit` / `abort` |

**証明**（取得順序の有向グラフに閉路がない）:

1. 上の表の辺は `P → {K, C, L}`、`L → D`、`gate → P` だけで、`{K, C, L, D}` から出る辺は `L → D` のほかにない。したがって順序 `gate < P < {K, C, L} < D`（`L < D`）が取れ、閉路はない。
2. **逆向きの辺がないこと**を、`L` の臨界区間の中身で確かめる。`L` を持つ間に呼ぶのは (a) `HashMap`・`VecDeque` の操作、(b) `Condvar::wait_timeout`（`L` を手放す）、(c) `Condvar::notify_one`（ブロックしない）、(d) `interrupts.check()`（`D` だけ）、(e) `Error` の組み立て（`BackendInfo` は `L` の表の中にある `pid` を使うだけで、`BackendRegistry` を呼ばない）、(f) `deadlock::find_cycle`（表を読むだけ）に限る。**`ProcArray`・`Clog`・制御ファイル・`MultiXactTable`・`Wal`・バッファプールには触れない**。`still_running`（`P` を取る）は `L` の外で呼ぶ（§5.5）。`wait_for_xact` の入口の `assert_may_wait` もスレッドローカルを読むだけで `L` の外。
3. `P` を持つ間に呼ぶ `LockManager` の関数は `try_acquire` だけで、`L` の臨界区間は 2 のとおり短く有限（他の `Mutex` を待たない）。`L` を持つスレッドが `P` を待つ経路はない（2）ので、`P` を持つスレッドが `L` を待ち続けることはない。
4. `XidGuard::drop` は `P`（clog の更新と実行中一覧の削除）の後で `P` を手放してから `L`（`release_all`）を取る。入れ子ではない。`RegisteredSnapshot::drop` は `P` だけ。`SessionLocks::drop`（09 章）は `L`（`release_all`、`unregister_backend`）だけを取り、`BackendGuard::drop` は `R` だけを取る（`BackendRegistry` は `LockManager` を呼ばない）。入れ子ではない。

**待ちの前の禁止事項（規約 1）の `L` 以外への影響**: `acquire` / `wait_for_xact` を呼ぶスレッドは、ページのラッチ・許容を超えるピン・共有バリア・コミットゲート・`checkpoint_lock` を持たない（§6.6 のフックで検出）。待たない関数（`try_acquire`・`release*`・`holds`・`lock_status`）は、ラッチを持ったまま呼んでよい（葉ロックの規則: 何を持っていても最後に取れる）。

### 5.12 接続・停止・組み立て（`BackendRegistry` と `shutdown.rs`）

**`Cluster` の組み立て順**（`engine.rs`。`Cluster::prepare`）:

現行の実装（`engine.rs` の `prepare`、`recovery::startup`、`StorageStack::new`）は「制御ファイルを開く → `recovery::startup` が `StorageStack` を作り、その中で REDO まで行う → `TxnManager::new` に `stack.clog` を渡す」順で、**`Clog` は `StorageStack::new` が作る**。M5 もこの形を保つ（03 §6.8・C13 と同じ。レビュー対応 R-18）。`HeapStore` が要る `LockManager` / `MultiXactTable` と、`Clog::open` に渡す `oldest_xid` は、**`StackConfig` の項目として `recovery::startup` に渡す**（`StorageStack::new(vfs, cfg, wal, next_xid)` の署名は 00 §4.5 のまま変えない）。

```
1. control = ControlFileHandle::open        （check_compatible、segment::remove_temp_files は M3 のまま）
2. locks = LockManager::with_knobs(knobs)   3. backends = BackendRegistry::new()
4. multixact = MultiXactTable::new(Arc::clone(&control))                   // 02 §4.3。Result を返さない（R-06）
5. stack_cfg = StackConfig { rel_seg_blocks, nframes, knobs,
                             locks: Arc::clone(&locks), multixact: Arc::clone(&multixact), oldest_xid: Xid(control.get().oldest_xid) }
6. outcome = recovery::startup(vfs, &control, &stack_cfg)                  // 内部で StorageStack::new(vfs, cfg, wal, next_xid):
                                                                           //   clog = Clog::open(vfs, next_xid, cfg.oldest_xid)（古いセグメントの掃除。03 §6.8）、
                                                                           //   procs = ProcArray::new(next_xid)、fsm、heap = HeapStore::new(pool, clog, wal, locks, procs, multixact, fsm)
                                                                           //   を作ってから REDO（REDO はロックマネージャにも MultiXact にも触れない）
7. stack = outcome.stack
8. txn = TxnManager::new(Arc::clone(&stack.procs), Arc::clone(&stack.clog), control, Arc::clone(&stack.wal), Arc::clone(&locks), Arc::clone(&multixact), knobs)
9. debug_assertions: txn::lock::set_wait_assertion(storage::buffer::track::assert_may_wait)（OnceLock。2 回目以降は無視）
```

`Clog` の所有者は `StorageStack`（1 つ）で、`TxnManager` は `stack.clog` の `Arc` を受け取る。`oldest` の渡し先は `StackConfig.oldest_xid` の 1 か所（制御ファイルの `oldest_xid`。0 のときは `Xid(3)`）。`Cluster` の組み立て順を 01 だけが別に持たない。

**接続**（手順は 09 章 §5.1 の `Cluster::connect` が持つ。LK が決めるのは `LockManager` に対する前提だけ）:

```
connect(req):                                                              ※ 09 章。検査の順序は DB-D1
  1. 検査: 28000（ロール）、3D000（データベース）、…
  2. backend = backends.register(BackendInfo { id: BackendId(req.session_id), pid: req.pid, .. })        // 登録簿だけ
     session_locks = SessionLocks::register(locks, id, pid)       // locks.register_backend(id, pid)。以後、エラーで戻れば Drop が release_all(Session) → unregister_backend
     接続数の検査（53300）                                          // register の後に数える。失敗なら両方の Drop が後始末する
  3. Database(db_oid) を AccessShare・LockScope::Session で acquire                // CREATE / DROP DATABASE が AccessExclusive を持つ間は待つ（D30・D40）
       WaitCtl { lock_timeout: None, deadlock_timeout: 既定, interrupts: req.interrupts }   // 待つ前の禁止事項（規約 1）: 他に何も持っていない
  4. Ok(ConnectGrant { db, role, session_locks, backend })                          // フィールドの順序が Drop の順序（DB 章）
```

`LockManager` に求める保証（09 章 §6.7 の前提 (1)〜(4) への回答）: (1) 同じバックエンドのロックは互いに衝突しない（LK-D3、§4.2）。(2) `try_acquire` は待ち行列の先行者を**飛び越さない**（§5.1 (2) の `wait_mask` 検査）。ただし、**自分がそのロックの何かを保持している**場合の割り込み（§5.1 (3)）は PostgreSQL と同じく許す（`CREATE DATABASE` の実行者が自分のテンプレートへの `AccessShare` を持ったまま `AccessExclusive` を `try_acquire` するときに、先行者の待ち手がいなければ成功する。待ち手がいる場合の割り込みは、その待ち手が自分の保持と衝突する場合だけ）。(3) `release_all(id, Session)` は**両スコープ**を解放し、その後の `unregister_backend` は保持なしで通る。(4) `BackendGuard` の Drop は `LockManager` の登録解除を行わない（LK-D21）ので、`SessionLocks::drop` の `unregister_backend` はそのままでよい。

**終了**（`Session` の終了と Drop）: `Session::terminate()` が (1) 未完了のトランザクションをアボートして `release_transaction_resources()`、(2) `session_locks` を Drop（`release_all(id, Session)` → `unregister_backend`）、(3) `backend` を Drop（登録簿から外す）、の順に行う。**`Session` のフィールドは `txn: Transaction` を `session_locks` と `backend` より前に、`session_locks` を `backend` より前に宣言する**（Rust は宣言順に Drop する。`terminate` を経ずに Drop されても、`XidGuard` が先に未完了のトランザクションを片付け、次に `SessionLocks` が残りのロックを解放する）。パニックのアンワインド中は `unregister_backend` の保持の検査を行わない。


**`BackendRegistry`（core）と `shutdown.rs` の `Coordinator`（server）の役割**:

| | `BackendRegistry` | `Coordinator`（`shutdown.rs`） |
|---|---|---|
| 目的 | 誰がどのデータベース・ロールで繋がっているか、pid ↔ `BackendId`、接続数の制限（`register_if`）、`pg_locks` / `pg_blocking_pids` の pid の解決 | 停止の段階（smart / fast / immediate）、停止要求の伝達（`InterruptFlag` の `request_terminate` と、ソケットの読み側の `shutdown`）、新規接続の締め切り |
| キー | pid（= `session_id` の `i32`） | 同じ pid |
| 中身 | `BackendInfo`（`Arc<InterruptFlag>` を含む） | `TcpStream` の複製と**同じ** `Arc<InterruptFlag>` |
| 登録の時期 | 認証の後、`Cluster::connect` の中 | 接続を受け付けた直後（認証の前。M2 §6.12） |
| 解除の時期 | `Session` の Drop（`BackendGuard`。`SessionLocks` の後） | 接続スレッドの終了（`Registration` の Drop）。**`BackendGuard` の後** |

- 同じ接続が両方に載るが、認証の前の接続は `Coordinator` だけに載り、`BackendRegistry` の接続数には数えない（PostgreSQL も認証後に数える）。
- smart 停止は `Coordinator` が空になるのを待つ。fast 停止は `interrupt_all` が `request_terminate` を呼び、待っているセッションは 20ms 以内に `acquire` から `57P01`（FATAL）で出る（§5.2 の (2)）。
- CancelRequest は `Coordinator`（pid + 秘密鍵）が `request_cancel` を呼ぶ。`BackendRegistry` は秘密鍵を持たない。

---

## 6. モジュールごとの仕様

### 6.1 `txn/lock/table.rs`（LK-1）

内部構造（実装の指針。`pub` にしない）。

```rust
#[derive(Debug, Default)]
struct LockTable {
    locks: HashMap<LockTag, LockEntry>,
    backends: HashMap<BackendId, BackendLockState>,
}
#[derive(Debug, Default)]
struct LockEntry {
    holders: Vec<Holder>,                  // 登録順（blocking_backends・DFS の順序の元。決定的）
    granted: [u32; 8],                     // モードごとの「保持しているバックエンド数」（holders から導ける冗長情報。debug_assert で照合）
    queue: VecDeque<BackendId>,            // 待ち手。先頭が最古（割り込みで途中に入ることがある）
}
#[derive(Debug)]
struct Holder { backend: BackendId, counts: [HoldCount; 8] }          // モードごと
#[derive(Clone, Copy, Debug, Default)]
struct HoldCount { txn: u32, session: u32 }                            // どちらかが 1 以上ならそのモードを保持
#[derive(Debug)]
struct BackendLockState {
    pid: i32,
    cv: Arc<Condvar>,
    waiting: Option<Waiting>,
    held: HashSet<LockTag>,                // 保持しているタグ（release_all の走査用。保持が 0 になったら外す）
}
#[derive(Debug)]
struct Waiting { tag: LockTag, mode: LockMode, scope: LockScope, since: Instant, granted: bool }
```

**不変条件**（`debug_assert!` と単体テストで検査する）:

| # | 不変条件 |
|---|---|
| I1 | 同じエントリの保持者のうち、**異なる**バックエンドのモード同士は衝突しない |
| I2 | `granted[m]` = モード m を保持している保持者の数。保持者のカウントがすべて 0 のモードは数えない |
| I3 | `queue` の各 `BackendId` は `backends[id].waiting.tag` がそのエントリで、`granted == false`。付与された待ち手は `queue` にいない（`waiting` は残り、`granted == true`） |
| I4 | 静止状態（どのスレッドも `Mutex` を持っていない）で `wake_waiters(tag)` を呼んでも何も付与されない（起こすべき待ち手が残っていない） |
| I5 | エントリが空（保持者も待ち手もいない）なら `locks` から消えている。`backends[id].held` はそのバックエンドが保持者になっているタグだけを含む |

- `wake_waiters` は §5.3 のとおり。解放・取り下げのたびに呼ぶ。
- 待ち手の登録は `request` が `Queued` を返すときに `backends[id].waiting = Some(..)` と `queue` への挿入を行う。`granted` の読み出しと下ろしは `take_granted(id)`（`granted` が真なら `waiting = None` にして `true`）。
- `register_backend(id, pid)`: `backends` に入れる（既にあれば内部エラー）。`unregister_backend(id)`: `held` が空でなければデバッグビルドで panic、`waiting` が `Some` でも panic（Session は待ちの最中に消えない）。

### 6.2 `txn/lock/deadlock.rs`（LK-2）

§5.4 のとおり。`find_cycle` と `deadlock_error`、`LockTag::describe`、`pid_of` を持つ。`blockers(t, cur, w)` は `blocking_backends` と同じ関数を共有する（同じ規則を 2 か所に書かない）。

### 6.3 `txn/lock/relation.rs`（LK-4。VC・DB・AU も使う）

`ddl::*` は `session` を使えないので、「解決 → ロック → 再解決」の輪と名前予約を、`catalog` にも `session` にも依存しない形でここに置く。

```rust
pub struct RelLockOpts<'a> {
    pub mode: LockMode,
    /// Some なら NOWAIT（待たない）。取れなければ 55P03 `could not obtain lock on relation "{label}"`。
    /// label は文に**書かれたとおりの名前**（スキーマを書いたなら "public.a"、書かなければ "a"。実機 E12）
    pub nowait_label: Option<&'a str>,
}

/// PostgreSQL の RangeVarGetRelidExtended に倣う（PG:src/backend/catalog/namespace.c）。
/// resolve は最新のカタログ（カタログ用スナップショット + 自分の変更。D12）で名前を引き、(db, rel)（db は共有カタログなら 0）を返す。
/// 見つからなければ Ok(None)（ロックしない。42P01 は呼び出し側）。generation はカタログのキャッシュの世代（DatabaseHandle.cache.generation()）
pub fn lock_relation(locks: &LockManager, backend: BackendId, wait: &WaitCtl<'_>, opts: &RelLockOpts<'_>,
                     generation: &dyn Fn() -> u64,
                     resolve: &mut dyn FnMut() -> Result<Option<(Oid, Oid)>>) -> Result<Option<(Oid, Oid)>> {
    loop {
        wait.interrupts.check()?;                                    // 輪が回り続けても中断できる
        let g0 = generation();
        let Some((db, rel)) = resolve()? else { return Ok(None) };
        let tag = LockTag::Relation { db, rel };
        match opts.nowait_label {
            None => locks.acquire(backend, tag, opts.mode, LockScope::Transaction, wait)?,
            Some(label) => if !locks.try_acquire(backend, tag, opts.mode, LockScope::Transaction) {
                return Err(Error::new(sqlstate::LOCK_NOT_AVAILABLE, format!("could not obtain lock on relation \"{label}\"")));
            },
        }
        if generation() == g0 { return Ok(Some((db, rel))); }        // 待っている間に何もコミットされていない（PG の inval_count）
        match resolve()? {
            Some(k) if k == (db, rel) => return Ok(Some((db, rel))),
            _ => { locks.release(backend, tag, opts.mode); }         // 待っている間に DROP / RENAME された。外してやり直す
        }
    }
}

/// 名前予約ロック（LK-D15）。Object { db, class: 1259 (pg_class), obj: name_key(namespace, name) } を Exclusive で取る
pub fn lock_name(locks: &LockManager, backend: BackendId, wait: &WaitCtl<'_>, db: Oid, namespace: Oid, name: &str) -> Result<()>;

/// FNV-1a 64 ビット（offset basis 0xcbf29ce484222325、prime 0x100000001b3）を、namespace の 4 バイト（リトルエンディアン）→ name の UTF-8 バイトの順に計算する。
/// 単体テストの固定値: (2200, "t") = 0x6c683d563e0069e3、(2200, "orders") = 0x85d5fa889e7f428c、(0, "") = 0x4d25767f9dce13f5
pub fn name_key(namespace: Oid, name: &str) -> u64;
```

- NOWAIT（`try_acquire`）の判定は `acquire` と同じ規則（§5.1）で、待たないだけが違う。NOWAIT のときも、ロックを取った直後に `generation` を比べて再解決する（解決してから取るまでの間に別のセッションが DDL をコミットしていた場合の保険）。
- 03 章（VACUUM）は表ごとに `lock_relation(.., ShareUpdateExclusive, ..)` を呼ぶ。09 章（CREATE / DROP DATABASE）・08 章（ロール）は `LockTag::Database` / `Object` を `LockManager` に直接取る（`lock_relation` は使わない）。

### 6.4 `LOCK TABLE`（`sql/parser/lock.rs`、`analyzer/ddl_ext/lock_table.rs`、`ddl/lock_table.rs`。LK-4）

**構文**（PG: `gram.y` の `LockStmt`）: `LOCK [ TABLE ] [ ONLY ] name [ * ] [, ...] [ IN lockmode MODE ] [ NOWAIT ]`。`lockmode` は `ACCESS SHARE` / `ROW SHARE` / `ROW EXCLUSIVE` / `SHARE UPDATE EXCLUSIVE` / `SHARE` / `SHARE ROW EXCLUSIVE` / `EXCLUSIVE` / `ACCESS EXCLUSIVE`。省略は `ACCESS EXCLUSIVE`。`ONLY` と `*` は受け付けて無視する（継承がない）。不正なモードは `42601 syntax error at or near "bogus"`（実機 E6）。

```rust
// sql/ast.rs（F0 が空の構造体を作る。LK が埋める）
pub struct LockTableStmt { pub tables: Vec<LockTableName>, pub mode: TableLockMode, pub nowait: bool, pub span: Span }
pub struct LockTableName { pub name: ObjectName, pub only: bool }
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TableLockMode { AccessShare, RowShare, RowExclusive, ShareUpdateExclusive, Share, ShareRowExclusive, Exclusive, AccessExclusive }

// analyzer/bound.rs
pub struct BoundLockTable { pub targets: Vec<LockTableTarget>, pub mode: LockMode, pub nowait: bool }
pub struct LockTableTarget { pub db: Oid /* 共有カタログは 0 */, pub rel: Oid, pub virtual_rel: bool }
```

- `sql::ast` は `txn::lock` を使えない（層）ので `TableLockMode` を別に持ち、アナライザが `LockMode` に直す。
- **アナライザ**（`ddl_ext::lock_table::analyze`）: 各名前を解決する。見つからなければアナライザの 42P01 `relation "x" does not exist`（スキーマを書いていれば `"s.x"`）。`RelKind::Index` / `Sequence` はロックの段階（`check_lockable`）で既に 42809 になっている（実機 E6）が、アナライザも同じ検査をする（準備済み文の再アナライズの経路）。
- **`check_lockable`**: `Table` → ロックする。`Virtual` → 成功して何もしない。`Index` → `42809 cannot lock relation "a_pkey"`、DETAIL `This operation is not supported for indexes.`。`Sequence` → 同じく DETAIL `This operation is not supported for sequences.`。メッセージの名前は**スキーマなしの名前**（PG の `rv->relname`）。
- **25P01**: `LockTable` は `ddl::lock_table::require_transaction_block(outside_block)` が `outside_block` のとき `25P01 LOCK TABLE can only be used in transaction blocks` を返す。session は §5.9 の手順 0 でロックの前に呼ぶ。`ddl::execute` の中でも呼ぶ（準備済み文の経路）。暗黙のトランザクションブロック（複数文の Simple Query）の中は `outside_block = false` なので成功する（PostgreSQL と同じ）。
- **読み取り専用トランザクション**でもすべてのモードで実行できる（PG17 の実測。E6）。`25006` は返さない。
- **`ddl::lock_table::execute`**: 各 `target`（`virtual_rel` でないもの）について `debug_assert!(locks.holds(backend, Relation { db, rel }, mode))` を確かめ、コマンドタグ `LOCK TABLE` を返す。ロックは §5.9 で取れている。
- `LOCK TABLE a, b` は書かれた順に取る（途中で待ち・失敗しうる）。2 つ目の失敗でトランザクションはアボートし、1 つ目も解放される。

### 6.5 観測（`catalog/virtual_rel.rs`、`executor/virtual_scan.rs`。LK-5）

- `registry()` は `&[&PgLocks]`（08 章が `PgRoles` を足す）。`find(oid) -> Option<&dyn VirtualRelation>`、`bootstrap_catalog_rows(db_oid)`（`pg_class` と `pg_attribute` の行。F0 が `bootstrap.rs` の口から呼ぶ）。
- **カタログ**: `CatalogReader::table(..)` は `relkind = 'v'` の行を読んで `TableDef { kind: RelKind::Virtual, columns, .. }` を返す（`locator` は使わない。`relfilenode = 0`）。アナライザは `RteKind::Table` として扱い、`planner::physicalize` が `PhysicalPlan::VirtualScan { rel_oid, columns }` にする。**インデックス・フィルタの押し下げ・ロックはしない**（フィルタは上の `Filter` が行う）。
- **`VirtualScan` の `Executor`**: 最初の `next` で `registry::find(oid).rows(&VirtualCtx)` を 1 回呼んで行を溜める（その文の中では一貫した行になる）。`rewind` は溜めた行を先頭から読み直す。`rows()` を呼ぶ時点で `LockManager` の `Mutex` を取る（`lock_status`）。ラッチ・ピンは持たない。
- **`pg_locks` の自分自身の行**: 仮想リレーションはロックしないので、`select * from pg_locks` の文自身の `AccessShare` の行は出ない（PG は `pg_locks` ビューに `AccessShare` を取る。既知の差。共有テストは `relation` で絞る）。
- **`virtualxid` / `virtualtransaction` の行・値**: 作らない / NULL（M5-LK-Q6）。
- **DML・DDL のエラー**（実機 E17）: `INSERT` / `UPDATE` / `DELETE` は **`55000`** `cannot insert into view "pg_locks"` / `cannot update view "pg_locks"` / `cannot delete from view "pg_locks"`、DETAIL `Views that do not select from a single table or view are not automatically updatable.`、HINT `To enable inserting into the view, provide an INSTEAD OF INSERT trigger or an unconditional ON INSERT DO INSTEAD rule.`（`updating` / `UPDATE`、`deleting` / `DELETE`）。`TRUNCATE` と `DROP TABLE` は **`42809`** `"pg_locks" is not a table`（`DROP TABLE` だけ HINT `Use DROP VIEW to remove a view.`）。契約 §4.10 の「42809」は INSERT / UPDATE / DELETE については PG と違うので 55000 にする（§11-11）。これらのエラーを返す関数 `virtual_rel::modify_error(rel_name, ModifyKind)` / `not_a_table_error(rel_name, is_drop)` はこの章が提供し、呼ぶのはアナライザの DML（`analyzer/dml.rs`。RW の持ち分）と DDL（`ddl/`。VC・M4 の持ち分）。
- **関数**: `pg_blocking_pids(int4)`（§4.9）。`pg_isolation_test_session_is_blocked(int4, int4[])` は M3 の署名のまま、本体を `runtime.is_blocked_by(pid, among)` に委ねる（`among` は `Datum::Array` から取り出す。NULL 要素があれば `XX000 array must not contain nulls`、PG と同じ）。
- **Session の `RuntimeInfo`**: `is_blocked_by(pid, among)` は `cluster.backends().by_pid` で `pid` と `among` を `BackendId` に直し（登録がなければ `false`）、`locks.is_blocked_by(id, &ids)`。`blocking_pids(pid)` は `blocking_backends` の結果を `BackendInfo.pid` に直す。

### 6.6 規約 1 の検出（`storage/buffer/track.rs`、`checkpoint.rs`、`txn/lock/mod.rs`。LK-3）

```rust
// txn/lock/mod.rs（デバッグビルドだけ）
#[derive(Clone, Copy, Debug)] pub enum WaitSite { Acquire, WaitForXact }
#[cfg(debug_assertions)] static ASSERT_MAY_WAIT: OnceLock<fn(WaitSite)> = OnceLock::new();
pub fn set_wait_assertion(f: fn(WaitSite));            // Cluster::open が 1 回呼ぶ。2 回目以降は無視（OnceLock::set の Err を捨てる）
fn assert_may_wait(site: WaitSite);                    // フックが登録されていれば呼ぶ。acquire / wait_for_xact の入口（Mutex を取る前）

// storage/buffer/track.rs（追記。thread_local の State に gate / checkpoint_lock / allowed_pins を足す）
pub fn gate_acquired();  pub fn gate_released();                        // コミットゲート（共有・排他）。TxnManager が呼ぶ
pub fn checkpoint_lock_acquired();  pub fn checkpoint_lock_released();  // checkpoint::run が呼ぶ
/// 待ちの対象のページのピンを許す。ガードが生きている間だけ allowed_pins = n（入れ子は最大値）。02 章の heap が待つ直前に使う
pub fn allow_pins(n: usize) -> AllowPinsGuard;
pub fn assert_may_wait(site: WaitSite);                // 違反なら panic（メッセージに保持中のものを全部出す）
```

`track::assert_may_wait` の検査（すべて満たさなければ panic。メッセージ例: `may not wait for a lock while holding: page latch ×1, pins ×3 (allowed 1), shared storage barrier`）:

| 検査 | 条件 |
|---|---|
| ページのラッチ | `latches_held() == 0` |
| ピン | `pins_held() <= allowed_pins`（既定 0） |
| 共有ストレージバリア | `barrier_held() == false`（チェックポイントのスレッドで使う。LK-D23） |
| コミットゲート | `gate_depth == 0` |
| `checkpoint_lock` | `checkpoint_lock_depth == 0` |

- **検出できないもの**: `Mutex` 一般（`ProcArray`・`Clog`・`Wal` の内部など）。std の `MutexGuard` は追跡できないので、これらは §5.11 の葉ロックの規則とレビューで守る。
- `acquire` / `wait_for_xact` は**待たなくても**入口で検査する（LK-D22）。`try_acquire`・`release*`・`holds` は検査しない（ラッチを持ったまま呼んでよい。§5.11）。
- 検査を試験するには、フックを登録した `Cluster`（`TestCluster`）の上で、ラッチ・ピン・バリア・ゲート・`checkpoint_lock` を持ったまま `acquire` を呼び、`#[should_panic]` で確かめる（`yuzhu-core/tests/lock_rules.rs`。§7.2）。

**`checkpoint.rs`**: 手順 4 の `statement_barrier()` はそのまま（LK-D23）。`checkpoint::run` の冒頭の `checkpoint_lock` の取得と解放に `track::checkpoint_lock_acquired` / `released`、`commit_gate_exclusive` の取得と解放に `track::gate_acquired` / `released` を足す。

### 6.7 変異テスト用のスイッチ（`debug_knobs.rs`。M3 §4.10 の続き）

`DebugKnobs` に次を足す（既定はすべて `false`。CLI・設定・SQL からは変えられない）。名前は 10 章 §6.9 の一覧（#2、#13〜#15）に合わせた（TS-D9、CR-5）。

| スイッチ | 効果 | 検出するテスト（§7.5） |
|---|---|---|
| `lock_ignore_access_exclusive`（10 章 #14） | `AccessExclusive` の要求・保持が他のモードと衝突しないものとして扱う（`check_conflicts` と `wait_mask` の判定） | 衝突表（`LockManager` 経由の 64 組）、分離性 `drop-while-reading`・`lock-table-queue` |
| `lock_ignore_conflicts` | `check_conflicts` と §5.1 (3) の判定が常に「衝突しない」（`LockMode::conflicts` は純関数なので変えない） | 衝突表（`LockManager` 経由）、I1 の性質テスト |
| `lock_disable_fifo` | §5.1 (2) の待ち手のモードとの衝突検査（`wait_mask`）を省く | FIFO（DDL の飢餓） |
| `lock_disable_breakin` | §5.1 (3) の割り込み規則を外す | ロックの昇格（A の `RowExclusive` が待たされ 40P01 になる）、早期デッドロック |
| `deadlock_detection_off`（10 章 #15） | §5.2 (4) の検査をしない（早期デッドロックは残る） | デッドロックの 2 者・3 者（40P01 が出ず、テストの期限切れで失敗する）、分離性 `deadlock-2way-row` |
| `release_locks_before_commit_record`（10 章 #13） | §5.8 の手順 4（ロックの解放）を手順 1（WAL の flush と clog）より前に行う | コミットとロックの順序（待っていたセッションが古い値を読む）の結合テスト、10 章の S1 |
| `vacuum_ignore_registered_snapshots`（10 章 #2。VC と共有） | `ProcArray::oldest_xmin` が `snap_xmins` を無視する（実行中の XID だけで計算する） | `oldest_xmin` の単体テスト、VACUUM との競合（03 章・10 章 S4 (e)） |


### 6.8 `settings.rs`（LK が足す分。契約 §8 の例外）

§4.8 の 4 項目。`deadlock_timeout` は `Settings::deadlock_timeout() -> Duration`、`INERT_GUCS` に `log_lock_waits` を足す。`max_locks_per_transaction` は読み取り専用の設定（`SHOW` は 64、`SET` は `55P02`）として、M3 の `wal_segment_size` などと同じ扱いにする。

---

## 7. テスト

分離性 spec の実体（ランナー、期待ファイルの生成、CI）は 10 章（TS）が持つ。この章は**何を確かめるか**を決める。

### 7.1 Rust の単体テスト（`txn/lock/tests.rs`。LK-1・LK-2）

| 対象 | 内容 |
|---|---|
| 衝突表 | §4.1 の `mask` 列を**固定値として**書き、8×8 の全 64 組で `conflicts` と照合する。さらに **`LockManager` 経由**で、全 64 組について「A が mode_a を保持した状態で、別のバックエンドの `try_acquire(mode_b)` が `!conflicts(mode_b, mode_a)` と一致する」ことを確かめる（`lock_ignore_conflicts` の検出）。対称性（`a.conflicts(b) == b.conflicts(a)`）。`AccessShare` は `AccessExclusive` とだけ衝突、`AccessExclusive` は全部と衝突、`ShareUpdateExclusive` は自分自身と衝突、`Share` は自分自身と衝突しない。`from_sql_words` の 8 つ |
| 参照カウント | 同じ `(タグ, モード)` を 3 回取り 2 回 `release` してもまだ保持、3 回で消える。Transaction と Session の両方で取り、`release_all(Transaction)` で Session 分だけ残る。`release_all(Session)` で全部消える。持っていないモードの `release` が `debug_assert!` で失敗する（`#[should_panic]`） |
| 同一バックエンド | 同じバックエンドが `AccessShare` と `AccessExclusive` を取れる。別のバックエンドは衝突する |
| 即付与 / 待ち | 衝突しなければ `acquire` は待たずに返る。衝突すれば解放まで待つ（別スレッド）。`try_acquire` は衝突で `false` |
| FIFO（実機 E8） | A が `AccessShare`、B が `AccessExclusive` を待ち、C が `AccessShare` を要求 → **C は待つ**。A の解放で B が付与され、C はまだ待つ。B の解放で C が付与される |
| 割り込み規則（E7） | A が `AccessShare`、B が `AccessExclusive` を待ち、A が `RowExclusive` を要求 → **待たずに付与**。B は A の解放まで待つ。A・B が `AccessShare` を持ち B が `AccessExclusive` を待ち、A が `AccessExclusive` を要求 → **即座に 40P01**（`deadlock_timeout` を待たない。経過時間 < 50ms で検査） |
| 起こし方 | 待ち行列 `[X: AccessExclusive（A の保持と衝突）, Y: AccessShare]` で、X が `lock_timeout` で取り下げると Y が起きる。保持者が全員解放すると、先頭の X だけが付与され Y は X の後ろで待つ |
| 中断 | キャンセル（別スレッドから `request_cancel`）→ 57014 `user request`、20ms 周期なので 200ms 以内に戻る。`lock_timeout` → 55P03 `canceling statement due to lock timeout`、期限の 20ms 以内に。`statement_timeout` → 57014 `statement timeout`。停止（`request_terminate`）→ `57P01`（FATAL）。いずれも**取り下げの後、待ち行列の後ろの待ち手が起きる**こと、ロックが漏れないこと（`held_locks` が元のまま） |
| 付与と中断の競合 | 付与の直後に `lock_timeout` が切れる設定で多数回回し、`Ok` を返したのに `held_locks` に無い、`Err` を返したのに `held_locks` にある、の両方が起きない（§5.2 の (1) が (2) より先であること） |
| デッドロック 2 者（XID） | A が XID 1 のロックを持ち、B が XID 2 のロックを持ち、A が XID 2 の `Share`、B が XID 1 の `Share` を待つ。`deadlock_timeout = 100ms`、B は A が待ち始めた約 30ms 後に待つ（循環が A の検査より前に完成する）。**先に待った A が 40P01**（`deadlock_timeout` の後）、B は A の中断後（A が `release_all` した後）に取れる。DETAIL は 2 行で `Process {pid} waits for ShareLock on transaction {xid32}; blocked by process {pid}.` の形、1 行目の pid が A、最後の行の `blocked by` が A |
| デッドロック 3 者・リレーション | 3 者の循環（DETAIL が 3 行）。リレーションロックの 2 者（`AccessShareLock on relation {rel} of database {db}`）。早期デッドロックの DETAIL（2 行とも同じ `relation`、モードは `AccessExclusiveLock`） |
| 偽陽性なし | 循環のない長い待ちの連鎖（A→B→C→D）では `deadlock_timeout` の後も 40P01 にならず、解放で順に進む。**ある 1 者が循環の外から待っている**場合（E→A、A→B→A の循環）は E の検査では 40P01 にならない（循環が E を通らない。A か B が自分で検出する） |
| soft 辺 | 実機 E9 の構成（B→A hard、A→C hard、C→B soft）で 40P01 になる（PG は並べ替えで解く。yuzhu の既知の差を固定するテスト。LK-D1） |
| `wait_for_xact` | 持ち主が `release_all` するまで待つ。`still_running` が `false` を返せば 1 周で戻る。2 周目は 1ms 眠る。自分の XID は内部エラー。`still_running` が `LockManager` の `Mutex` を持たずに呼ばれる（コールバックの中から `lock_status` を呼んでデッドロックしない） |
| `blocking_backends` / `is_blocked_by` | hard のみ、soft のみ（E8 の C が `{B}`）、両方。待っていなければ空。付与済みで未起床の待ち手は空 |
| `lock_status` | 保持（`granted = true`）と待ち（`false`、`waited` は `Some`）、タグごとの列の対応（§4.2 の表）、並びの決定性 |
| 名前予約 | `name_key` の固定値、`lock_name` の直列化（同じ名前は 2 つ目が待つ） |
| 性質テスト（`proptest`） | 小さいタグ集合（3 タグ）・バックエンド 4・モード 8 のランダムな `request` / `release` / `cancel_wait` の列を**1 スレッドで `LockTable` に直接**与え、各操作の後に I1〜I5 を検査する。さらに I4（`wake_waiters` の冪等性）と、「到着順に並ぶ待ち手の付与順が、衝突する対については入れ替わらない」ことを履歴から検査する |

### 7.2 結合テスト（`yuzhu-core/tests/lock_wait.rs`、`lock_rules.rs`、`yuzhu-server/tests/concurrency.rs`）

`TestCluster` の上で複数の `Session` を別スレッドで動かす（プロトコルを通す `concurrency.rs` は TS の共通部品を使う）。`deadlock_timeout` は 100ms にする。

| テスト | 内容 |
|---|---|
| 読み取りは DML を止めない | A: `BEGIN; SELECT * FROM t`、B: `INSERT INTO t ...`（待たない）、B: `DROP TABLE t` は待つ。A の `COMMIT` で B が進み、後続の C の `SELECT * FROM t` が（C が DROP の後ろに並んでいたなら）`42P01` |
| DROP を待つ読み取り（E8） | A が `AccessShare`、B の `DROP TABLE` が待ち、C の `SELECT` も B の後ろで待つ。A のコミットで B が成功し、C は `42P01`（待ち明けに再解決する） |
| 再解決（`lock_relation`） | `generation` を操作して、待ち明けに別の OID を指す場合に、取ったロックを外して引き直すこと |
| コミットとロックの順序（D38） | A が `UPDATE` してコミット、B がその行を待っていて起きた直後に A の更新が見える（`release_locks_before_commit_record` を有効にした変異で落ちる） |
| 失敗した文の即時解放（LK-D11） | A のブロックの中で早期デッドロックの 40P01 → A は `Failed` だが、待っていた B は A の `ROLLBACK` を待たずに進む |
| `LOCK TABLE` | 25P01（ブロックの外、`outside_block`）、暗黙のブロックの中は成功、42809（index・sequence。メッセージの名前はスキーマなし）、`NOWAIT` で `could not obtain lock on relation "a"` と `"public.a"`、読み取り専用で成功、`pg_locks` に載るモード |
| 名前予約 | A が `CREATE TABLE x`（未コミット）、B が `CREATE TABLE x` は待つ。A のコミットで B は `42P07`。A がアボートすれば B は成功。`IF NOT EXISTS` は NOTICE。`DROP TABLE x`（未コミット）の間の B の `CREATE TABLE x` は**待たずに** `42P07`（PG と同じ） |
| DROP の第 2 段（LK-D16） | FK の相手の表と所有シーケンスが `AccessExclusive` で取られる。相手を読むトランザクションがいれば DROP が待つ。`nextval` が `RowExclusive` を取り、`DROP SEQUENCE` が待つ |
| 検証（LK-D12） | 検索パスの手前に同名の表が作られた場合を人為的に作り、2 回目のアナライズが別の表を指したら、足りないロックを取って再アナライズすること（4 回で `XX000`） |
| スナップショットと horizon | RC は文の外で登録がない（`oldest_xmin` が進む）。RR の `xact_snapshot` が `oldest_xmin` を止め、`COMMIT` で外れる。XID を持つトランザクションも止める。`RegisteredSnapshot` の Drop で外れる。`for_statement` が `own_xid` を差し替える（RR の最初の文の後で XID を取っても自分の変更が見える） |
| ロックなしの `Clog` | 多数のスレッドが `status` を読みながら別のスレッドが `set_status` する。矛盾する二重設定は内部エラー。`flush` が部分的に更新されたバイトを書いてもコミット済みの値が失われない |
| 規約 1 の検出（`lock_rules.rs`） | ラッチ・許容を超えるピン・共有バリア・コミットゲート・`checkpoint_lock` のそれぞれを持ったまま `acquire` / `wait_for_xact` を呼ぶと panic（待たない状況でも）。`allow_pins(1)` の下でピン 1 つは許され、2 つは panic |
| 接続の終わり | 待ちの最中にサーバが停止（`request_terminate`）→ 20ms 以内に FATAL `57P01`。`SessionLocks` の Drop で全ロックが消える。パニックのアンワインドで `Session` を Drop してもロックが漏れず二重 panic しない |
| `pg_locks` / `pg_blocking_pids` | A が `LOCK TABLE`、B が待つ状態で、C の `SELECT ... FROM pg_locks`（`relation` で絞る）が `granted = true / false` の 2 行を返し、`pg_blocking_pids(B の pid)` が `{A の pid}`、`pg_isolation_test_session_is_blocked(B, '{A}')` が `true`、`'{C}'` なら `false`。DML は 55000、`TRUNCATE` は 42809 |
| 準備済み文の Bind | 04 章と共同: `acquire_oids` で取った後に世代が変わっていたら再アナライズする |

### 7.3 共有テスト（`tests/slt/m5/lock/`。PostgreSQL 17 で期待値を確かめる）

ファイル名は 10 章 §6.5.1 の一覧に合わせた（持ち主は LK）。

| ファイル | 内容 |
|---|---|
| `lock_table_modes.slt` | 1 接続。`BEGIN` の中で文を実行し、`pg_locks` を `pg_class` と結合して**表（`relkind = 'r'`）の `relation` 行のモード**を確かめる: SELECT（AccessShare）、FOR UPDATE / FOR SHARE / `OF a`、INSERT / UPDATE / DELETE（RowExclusive）、UPDATE ... FROM（対象 RowExclusive、FROM 側 AccessShare）、`INSERT ... SELECT`、COPY、`EXPLAIN UPDATE`、副問い合わせ、CREATE INDEX（Share）、TRUNCATE（AccessExclusive）、`ALTER TABLE ADD FOREIGN KEY`（両方 ShareRowExclusive）、`CREATE TABLE ... REFERENCES`（参照先 ShareRowExclusive）、`DROP TABLE`（FK の相手の AccessExclusive）、`LOCK TABLE` の 8 モードと複数の表・`ONLY`、`nextval` / `setval`（シーケンスに RowExclusive）、`ALTER SEQUENCE`（ShareRowExclusive）。インデックスの行は比べない（既知の差。LK-D14。KD-9） |
| `lock_table_errors.slt` | `LOCK TABLE` の構文（`TABLE` の省略、`ONLY`、`*`、`NOWAIT`、不正なモードの 42601）、ブロックの外の 25P01、`lock table nosuch` が**ブロックの外では 25P01・中では 42P01**、index・sequence の 42809（名前はスキーマなし）、読み取り専用で成功、`NOWAIT` のメッセージ（スキーマの有無）。`NOWAIT` が相手の保持で 55P03 になるのは 2 接続の `lock_timeout_table.slt` |
| `pg_locks_basic.slt` | `pg_locks` の 16 列の名前・型・順序（`pg_attribute` で）、自分の `relation` で絞った `locktype` / `mode` / `granted`、DML が 55000、`TRUNCATE` が 42809 |
| `pg_blocking_pids.slt` | `pg_blocking_pids(pg_backend_pid())` が `{}`、2 接続で A が `LOCK TABLE`、B が `lock_timeout` 付きで待つ前に C が `pg_blocking_pids` と `pg_isolation_test_session_is_blocked` を呼ぶ形（待ちが残らないよう B は `lock_timeout` で必ず終わる） |
| `deadlock_timeout_set.slt` | `SHOW deadlock_timeout` が `1s`、`SET deadlock_timeout = '300ms'` の `SHOW` が `300ms`、`SET deadlock_timeout = 0` が 22023、`SHOW max_locks_per_transaction` が 64、その `SET` が 55P02、`SET log_lock_waits = on` が成功 |
| `lock_timeout_table.slt` | `connection a`: `BEGIN; LOCK TABLE t`、`connection b`: `SET lock_timeout = '100ms'; SELECT * FROM t` が 55P03 `canceling statement due to lock timeout`、`SET lock_timeout = 0` と `statement_timeout = '100ms'` で 57014、`LOCK TABLE ... NOWAIT` が 55P03 `could not obtain lock on relation "t"` |

`deadlock` の `DETAIL` は pid と XID が毎回違うので共有テストでは比べない（sqllogictest は `statement error` のメッセージ本文だけを見る）。


### 7.4 分離性テスト（`tests/isolation/`。実体の追加は TS。D39）

10 章 §6.3・§6.4.1 が計画した spec のうち、LK の機能を確かめるものと、LK が追加を依頼するものを挙げる。

| spec | 内容 | 計画 | PG と一致 |
|---|---|---|---|
| `lock-table-queue`（共有） | E8（DROP の待ち行列、後続の SELECT は FIFO で待つ、待ち明けは 42P01）、E4・E6（25P01、`LOCK TABLE` の待ち）、`lock_timeout`・NOWAIT | 10 章 §6.4.1 | ○ |
| `deadlock-2way-row`（共有） | E14（2 者の行ロックのデッドロック。被害者は先に待った側。`SET deadlock_timeout` で順序を決める） | 10 章 | ○ |
| `deadlock-table`（共有） | E16（テーブルロックのデッドロック） | 10 章 | ○ |
| `drop-while-reading`（共有） | 読み取り中のテーブルの DROP / TRUNCATE を待つ（M3-Q20 の差が消える） | 10 章 | ○ |
| `idle-session-lock-release`（共有） | 接続の切断（FATAL）でリレーションロックと XID ロックが解放され、待っていた側が進む | 10 章 | ○ |
| `lock-timeout`（M3 の書き直し） | テーブルロックの `lock_timeout` と NOWAIT の permutation を足す | 10 章 §6.3 | ○ |
| `deadlock-simple`・`deadlock-hard`（移植） | 2 者・多者のデッドロック。出力は `ERROR:  deadlock detected` | 10 章 §6.4（完全一致の見込み） | ○ |
| `deadlock-soft`・`deadlock-soft-2`（移植） | PostgreSQL の待ち行列の並べ替えで解消されるケース。yuzhu は 40P01 | 10 章 §6.4（variant。KD-5） | **×**（`<name>.yuzhu-m5.out`） |
| `lock-table-modes`（**追加依頼**） | `LOCK TABLE` の 8 モードの衝突表を、`pg_isolation_test_session_is_blocked` で「待つ / 待たない」の全 64 組み合わせ | LK が依頼 | ○ |
| `deadlock-early`（**追加依頼**） | E7 の早期デッドロック（待ち 0 秒で 40P01）と、割り込みで即付与（A の `RowExclusive` が待たされない） | LK が依頼 | ○ |
| `commit-order`（**追加依頼**） | 待っていたセッションが、起きた直後に相手のコミットの結果を見る（D38。変異 `release_locks_before_commit_record` の検出） | LK が依頼 | ○ |
| `create-table-same-name`（**追加依頼**） | 同名の CREATE TABLE の同時実行。後発は先発のコミット後に 42P07（PG は 23505。KD-7） | LK が依頼 | **×**（yuzhu 専用） |
| `drop-create-same-name`（**追加依頼**） | 未コミットの DROP の間の CREATE は待たずに 42P07（E20）、DROP のコミット後の CREATE は成功 | LK が依頼 | ○ |

M3 の `writer-queue`・`writer-waits-writer` の書き直しは RW と TS が担当する（行ロックの待ちになるため）。10 章 §6.9 の変異 #13〜#15（`release_locks_before_commit_record`、`lock_ignore_access_exclusive`、`deadlock_detection_off`）の検出は、上の `commit-order`・`lock-table-queue`・`drop-while-reading`・`deadlock-2way-row` が担う。


### 7.5 変異テスト

各スイッチ（§6.7）について「既定のシード・構成のうち少なくとも 1 つで、対応するテストが**失敗する**こと」をテストにする（M3 §7.5 と同じ規則）。`deadlock_detection_off` と `lock_disable_breakin` は待ちの期限（テスト用に 2 秒）で打ち切り、失敗として報告する。

### 7.6 M3 の `TxnManager` のテストの書き直し（LK-3 の工数に含む）

M3 の `txn/manager.rs` の単体テスト（約 10 件。`m3.md` §7.4 の「txn」の行）と、`begin_write` を呼ぶテスト・ヘルパ（`checkpoint.rs` 2 か所、`engine.rs` 3 か所、`testing.rs` 1 か所、`session.rs` のほか。2026-10-05 時点の呼び出し数）を書き直す。

| 既存のテスト | 扱い |
|---|---|
| `xids_are_assigned_in_order_and_run`、`xid_prefetch_is_written_to_control_file`、`restart_discards_prefetched_xids`、`snapshot_contents`、`commit_of_unknown_xid_is_panic_error` | `begin_write(sid, None)` を `assign_xid(BackendId(sid))`（`ProcArray` / `LockManager` を組み立てるヘルパ `test_txn_env()`）に置き換えるだけ |
| `writer_lock_blocks_then_hands_over`、`writer_lock_timeout_is_55p03`、`double_begin_by_the_same_session_is_an_error` | **削除**。同じ性質は `LockManager` のテスト（§7.1 の FIFO、`lock_timeout`、保持済みの再取得）に移る |
| `dropping_a_writer_guard_aborts_in_memory`、`dropping_a_finished_guard_keeps_the_status` | `XidGuard` に読み替える。さらに「Drop でトランザクションスコープのロックが解放される」「正常な commit / abort の後の Drop は何もしない」を足す |
| `control_failure_during_prefetch_releases_the_writer` | 「XID を消費せず、XID ロックも登録されない」に読み替える |
| `barrier_shared_and_exclusive`、`exclusive_barrier_waits_for_statements` | バリアの意味が変わる（LK-D23）ので、`exclusive_barrier_waits_for_statements` を「チェックポイントの共有バリアが保たれている間、排他バリアが待つ」に書き直す |
| M3 の「コミットの順序」「ゲートとチェックポイントの並行」「FIFO」「待ち中の中断」「`is_blocked_by`」 | 前 2 つは `TxnManager` に残す。後ろ 3 つは `LockManager` のテスト（§7.1）に移す |

---

## 8. 実装の分担と工数

契約 §7 の WP の ID を使う。日数は AI の実装エージェント 1 本の見積もり。契約の LK の合計（13.5 日）の内訳:

| ID | 内容 | ファイル（持ち主は LK。契約 §8） | 依存 | 日数 |
|---|---|---|---|---|
| **LK-1** | `LockManager`: `LockMode` / 衝突表、`LockTag`、`LockTable`（request / grant / release / `wake_waiters`）、待ちのループ（20ms、`lock_timeout`、キャンセル）、`try_acquire`、`wait_for_xact`、`blocking_backends` / `is_blocked_by`、`lock_status`、`BackendId`・`BackendRegistry` の型（`backend.rs`） | `txn/lock/{mod,table,wait}.rs`、`backend.rs`、`txn/lock/tests.rs`（§7.1 のうちデッドロック以外） | F0 | 3 |
| **LK-2** | デッドロック検出: `find_cycle`、`deadlock_error`、早期デッドロック、`describe`、§7.1 のデッドロックの 2 者・3 者・偽陽性・soft | `txn/lock/deadlock.rs` | LK-1 | 1.5 |
| **LK-3** | **a**（1.5）`ProcArray`・`RegisteredSnapshot`・`oldest_xmin`・`Snapshot::is_own`・`StorageStack` への組み込み。**b**（1.5）`TxnManager` の再構成（`assign_xid` / `XidGuard`、`commit` / `abort` / `finish_without_xid`、`Transaction` の項目）、`Clog` のロックなし化、バリアの縮小、規約 1 の検出（`track.rs`・フック・`checkpoint.rs`）、`BackendRegistry` の実装と `Cluster` への組み込み。**c**（1）§7.6 のテストの書き直し（10 件と呼び出し側 7 か所）、変異スイッチ、`lock_rules.rs` | `txn/{proc_array,snapshot,manager,clog}.rs`、`txn/mod.rs`、`storage/buffer/track.rs`（追記）、`checkpoint.rs`（追記）、`debug_knobs.rs`（追記）、`engine.rs`（`locks`・`backends`・`multixact` の持たせ方） | LK-1 | 4 |
| **LK-4** | **a**（1.5）`session/locking.rs`: `collect_lock_requests`（純粋関数と単体テスト）、`acquire_statement_locks`、`RelationResolver` の Session 実装、`required_locks` と検証の輪、`acquire_oids`、`release_transaction_resources`、`txn/lock/relation.rs`。**b**（1）`LOCK TABLE`（パーサ・アナライザ・`ddl/lock_table.rs`・25P01・NOWAIT）。**c**（1）DDL のモードと名前予約・DROP の第 2 段（07 の FK の API と結合）、シーケンス関数の実行時ロック、`TxnControl` の口の要件確認（02 と）、結合テスト（§7.2）と slt（§7.3） | `session/locking.rs`、`txn/lock/relation.rs`、`sql/parser/lock.rs`、`analyzer/ddl_ext/lock_table.rs`、`ddl/lock_table.rs`、`settings.rs`（4 項目） | LK-3、M4 の analyzer・`ddl/` | 3.5 |
| **LK-5** | `catalog/virtual_rel.rs`（`PgLocks`、`bootstrap_catalog_rows`）、`executor/virtual_scan.rs`、`RelKind::Virtual` の通し（カタログ → アナライザ → planner）、`pg_blocking_pids`、`pg_isolation_test_session_is_blocked` の置き換え、DML・DDL のエラー関数、`pg_locks` の slt | `catalog/virtual_rel.rs`、`executor/virtual_scan.rs`、`catalog/builtin/runtime.rs` の 1 行 | LK-1、F0、M4 の planner | 1.5 |

- **他章の要求の取り込み**（LK-D28: `has_waiters`、`oldest_xmin_hint`、`registered_snapshot_count`、`Session::backend_id`、`RelationResolver::check_access`、`lock_statement_relations`、`missing_locks`）は各 WP に含める（合計 0.5 日分が増えるが、LK-1・LK-3・LK-4 の見積もりに吸収する。超えたら報告する）。
- **並列化**: LK-1 と LK-3a・LK-3b の `Clog` は独立（LK-1 は新規ファイルだけで、M4 の実装中に始めてよい。D49）。LK-2 は LK-1 の `LockTable` の型が決まれば並行できる。LK-4a の `collect_lock_requests`（純粋関数）は F0 の AST が済めば LK-3 を待たずに書ける。LK-5 の `PgLocks` 本体は LK-1 の後すぐに書けるが、planner への通しは M4 のマージ後。
- **クリティカルパス**への寄与: F0 → LK-1（3）→ LK-3（4）→ RW-1（契約 §1.3）。LK-3 の完了が 02 章の heap 側の組み立て（`HeapStore::new` の引数）の前提なので、**LK-3a・LK-3b を LK-3c より先に仕上げて 02 章に渡す**。
- カットライン（遅れたとき）: LK-5 の `pg_blocking_pids`（分離性ランナーは `pg_isolation_test_session_is_blocked` だけで足りる）、`pg_locks` の `waitstart`、変異スイッチの一部、`session/locking.rs` の検証の輪（LK-D12。デバッグビルドの `debug_assert!` だけにする）。落とせない: 衝突表、FIFO、割り込み規則、デッドロック、D38 の順序、規約 1 の検出、`Clog` のロックなし化。

---

## 9. 未検証の点（実装前に確かめること）

1. **DDL とカタログ行の同時更新**（m5-concurrency §9.5）。(a) 異なる表への DDL は同じカタログ行を更新しないが、**同じ表に対して互いに衝突しないロックで入る 2 つの DDL**（例: 2 つの `CREATE INDEX`。どちらも `Share`）は、同じ `pg_class` の行（`relhasindex`・`relpages`・`reltuples`）を更新しうる。PostgreSQL はこれらを in-place 更新（MVCC なし）にして競合を避ける。yuzhu が通常の `heap.update` で更新すると、後発が `Updated` を受けて `XX000 tuple concurrently updated`（契約 §3.8）になる。**確かめること**: M4 の `CREATE INDEX` が `pg_class` のどの列をどう更新するか。対策案: (i) PG と同じく in-place 更新にする（`heap_inplace_update` 相当。M4 / VC の持ち主へ依頼）、(ii) `CREATE INDEX` を表の名前予約風の追加ロック（`Object { class: 1259, obj: rel }`）で直列化する、(iii) 許容して共有テストに入れない。(b) `ANALYZE` / `VACUUM`（`ShareUpdateExclusive`）と `CREATE INDEX`（`Share`）は衝突するので直列になる。(c) `pg_database` の行（CREATE / DROP DATABASE）は 09 章。
2. **1 回きりの検査の限界**（§5.4）。「待ち手が増えないまま辺だけが増える」ことが、§5.1 の付与規則のもとで起こりうるかを、ランダムな schedule（§7.1 の性質テストに、検査を一度だけ行うモデルを足して）で確かめる。起きるなら、PostgreSQL も同じ限界を持つ前提で許容するか、検査を `deadlock_timeout` ごとに繰り返す。
3. **`assert_no_pins` とバリアの縮小**（LK-D23）。(a) 文の実行が共有バリアを持たなくなっても、`unlink`（排他バリア）の最中にチェックポイントのバッファ書き出しが対象ファイルに触れないこと（`drop_relation_buffers` の呼び出しがバリアの下にあること）、(b) `unlink` を含むコミットが長いバッファ書き出しの間待たされる実害の測定、(c) `track::barrier_acquired` をチェックポイントのスレッドで使い続けて、`exclusive_barrier` を同じスレッドが取らないこと。
4. **`lock_timeout` と `statement_timeout` の同時切れ**（LK-D25）。PG は先に切れた方を報告する（【確認】PG:src/backend/tcop/postgres.c。同時なら `lock_timeout`）。yuzhu は 20ms の周期の中でだけ `statement_timeout` を優先する。実機で差を測り、必要なら `InterruptFlag` に期限を読む口を足す（`interrupt.rs` の持ち主へ依頼）。
5. **自動生成の名前の予約**（§5.10）。`CREATE TABLE t (id serial primary key)` が決める `t_pkey`・`t_id_seq` が、別のトランザクションが同時に明示して作る同名のオブジェクトと衝突する場合。アナライザの `ChooseRelationName` が未コミットの行を見ないので、重複しうる。確かめる: M4 のシーケンス・インデックスの名前の決め方と、重複を検出する一意性の検査があるか。
6. **`pg_locks` の OID 9811 と `pg_roles` の 9810（08 章 C6）が、F0 の OID の表・`bootstrap` の固定値と重ならないこと**（契約 §3.2 の 9800〜9899 は `yz_relxid` 9801、`yz_datxid` 9802、`pg_roles` 9810、`pg_locks` 9811 で使用中）。
7. **`Statement` の AST の変種名**（M4・F0 の確定後）。§5.9 A の表は種類で書いてある。`collect_lock_requests` は AST を直接走査するので、`RangeVar` に相当する `TableRef::Table` / `ObjectName`、FOR 句（RW の AST）、`COPY`・`CREATE INDEX`・`ALTER TABLE` の変種名を実装時に合わせる。
8. **`RelationResolver` の FK の API**: 07 章（FK-1）の `pg_constraint` の読み方（`fk_peer_tables(rel, constraint)` に相当）と、M4 の `TableDef.identity_seqs` で所有シーケンスを引けること。
9. **`LockManager` の単一 `Mutex` の限界**: 接続数 100 前後・毎秒数万の文で、ロック表の競合が支配的にならないか（TS-2 の並行ストレスで測り、必要なら M6 でタグのハッシュによる分割を検討する。`find_cycle` は全体を見る必要がある）。
10. **`std::sync::Condvar` の挙動**: 待ち手ごとの `Condvar` を 1 本の `Mutex` と組にするのは許される（1 つの `Condvar` に複数の `Mutex` を使うのが禁止）。`wait_timeout` の偽の起床は §5.2 のループが吸収する（`granted` と期限を毎回確かめる）。

---

## 10. 確認事項

ユーザーの不在中に仮決めしたことです。ID は `M5-LK-Q<n>`（10 章が集める）。

| ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|
| M5-LK-Q1 | **デッドロックの検出は hard 辺と soft 辺の両方を辿り、待ち行列の並べ替えはしない**（LK-D1）。依頼文の「hard edge のみ」は、「hard 辺の循環は必ず検出し、並べ替えない」と解釈した。soft 辺を含む循環も検査した本人が 40P01 になる | hard 辺だけだと、soft 辺を含む循環が永久に待つ（実機 E9 の構成） | PG は並べ替えで解消するので、yuzhu だけ 40P01 になる場合がある（`deadlock-soft` 系は yuzhu 専用の期待）。並べ替えまで入れると +2〜3 日（PG の `DeadLockCheckRecurse` と `TopoSort` の移植） |
| M5-LK-Q2 | **カタログ用スナップショットも登録する**（LK-D9）。契約の「登録しない」を変える | 取得から読み取りまでの間に VACUUM がカタログのタプルを消す窓を作らない | 登録しない場合は、VACUUM がカタログ表を対象にしない（03 章）か、horizon に 1 文分の余裕を持たせる必要がある。登録のコストは `Mutex` の 2 回 |
| M5-LK-Q3 | **インデックスにはロックを取らない**（LK-D14）。`pg_locks` にインデックスの行が出ない | インデックスの DDL は常に表のロックを先に取る。ロックの数が減る | PG と同じにするなら、DML・SELECT がインデックスにも `RowExclusive` / `AccessShare` を取る（`required_locks` と `lock_status` の変更、+0.5 日）。共有テストが `pg_locks` の行数を比べる場合に差が出る |
| M5-LK-Q4 | **XID は文の開始時に割り当てる**（LK-D24）。PG は最初のヒープ書き込みで遅延する | `ExecCtx` から `TxnManager` を呼ばずに済む（契約 §5.1 d-3） | 何も書かない `UPDATE ... WHERE false` / 該当なしの `SELECT FOR UPDATE` が XID を持ち、horizon を止め、`pg_locks` に `transactionid` の行が出る。PG と同じ遅延割り当てにするには、書き込み時に XID を取る口を `ExecCtx` に足す（+1 日、02 章と調整） |
| M5-LK-Q5 | **同名の CREATE の同時実行は、後発が先発のコミットを待って `42P07`**（`IF NOT EXISTS` なら NOTICE）。PG は後発が `23505`（`pg_type_typname_nsp_index` / `pg_class_relname_nsp_index`）。名前予約ロックは CREATE だけが取る（LK-D15） | カタログに一意インデックスがない。M3-Q20 の既知の差の継続 | PG のエラーに合わせるなら、名前予約ロックの後の重複検出を `23505` + 上のインデックス名・DETAIL（`Key (typname, typnamespace)=(x, 2200) already exists.`）にする（+0.5 日。ただし `IF NOT EXISTS` の競合でも `23505` になる PG の挙動まで写すことになる） |
| M5-LK-Q6 | **`pg_locks` は `virtualxid` の行を作らず、`virtualtransaction` は NULL、`fastpath` は false、`Database` ロックは `object` の行として出す**（LK-D19） | yuzhu に仮想トランザクション ID がない | 共有テストが `pg_locks` の全行数・`virtualxid` を比べると差が出る（`relation` で絞れば出ない）。`virtualxid` の行を足すなら各トランザクションの開始時に `LockTag` を足す（+0.5 日） |
| M5-LK-Q7 | **`lock_timeout` と `statement_timeout` が同じ 20ms の周期の中で両方切れたら `statement_timeout` を報告する**（LK-D25）。PG は先に切れた方 | `InterruptFlag` の口を増やさない | テストで同時に切れる設定を使うと差が出る（共有テストに入れない）。合わせるなら `InterruptFlag::statement_deadline()` を足す（+0.3 日） |
| M5-LK-Q8 | **Repeatable Read の最初のスナップショットは、リレーションロックの前に取る**（LK-D27。02 章 RW-D10）。Read Committed の文のスナップショットはロックの後（契約 §5.1 d）。この章の初案（RR もロックの後）から変えた | 02 章が実機で、RR の最初の文がテーブルロックを待つと待ちの後も相手のコミットした行が見えないことを確かめ、PostgreSQL と同じにした。LK の手順（§5.9 の 0'）はそれを前に挟むだけ | ロックの後に取る（初案）と、RR の最初の文が待った後に新しいスナップショットで始まる（PG と違う）。RR 内の整合性は保たれる。変えるなら 02 章の RW-D10 と §5.9 の 0' を同時に直す |

| M5-LK-Q9 | **ロック表は 1 本の `Mutex`**（LK-D2） | デッドロック検査が全体を見る。M5 の規模で足りる見込み | 競合が見えたらタグのハッシュで 16 分割し、`find_cycle` は全分割を順に取る（+2 日、M6） |
| M5-LK-Q10 | **`LOCK TABLE` は読み取り専用トランザクションでも全モードで実行でき、仮想リレーションの `LOCK TABLE` は何もせず成功する**（LK-D17） | PG17 の実測（E6、E17） | 制限するなら `PreventCommandIfReadOnly` 相当を足す（PG 本家が許しているので差になる） |
| M5-LK-Q11 | **シーケンス関数（`nextval` / `currval` / `setval`）は実行時にシーケンスへ `RowExclusive` を取る**（LK-D18）。`DROP TABLE` は列が所有するシーケンスも `AccessExclusive` で取る（LK-D16） | D10 により、ファイルの削除はリレーションロックで守る。M4 の「シーケンスはロックなし」を、DROP の安全のために改める | `nextval` が DROP を待たせる / 待たされるようになる（PG と同じ）。取らないなら、使用中のシーケンスのファイルが DROP のコミットで消えて `nextval` が失敗する |
| M5-LK-Q12 | **待ちの周期は 20ms**（LK-D4）。キャンセル・停止・期限の遅延は最大 20ms | `InterruptFlag` に起こす仕組みを足さずに済む | 10ms に短縮すると CPU の起床が増える。即時にするには `InterruptFlag` から待ち手の `Condvar` を起こす口が要る（+1 日） |
| M5-LK-Q13 | **`max_locks_per_transaction` は `SHOW` だけ（64 固定）、`SET` は `55P02`**（PG17 と同じ） | PG17 の context が postmaster。契約 §3.6 の「保存だけ」を PG の挙動に合わせた | `SET` を通したい（保存だけ）なら `INERT_GUCS` に入れる。PG と差が出る |
| M5-LK-Q14 | **`pg_locks` の OID は 9811**（契約 §3.2 の 9800 番台。08 章が `pg_roles` に 9810）。PG17.11 の 12073 は使わない | システムビューの OID は版ごとに変わりうる。共有テストは OID に依存しない | PG と同じ OID にするなら 12073 を使う（`bootstrap` の固定値と PG の版への依存が増える） |


---

## 11. 契約への変更依頼

契約（`00-contracts.md`）に対して、次を依頼する。**章の中で黙って変えていない**。1〜4 は 02・F0 の担当に影響する。

1. **§4.2 `LockMode::pg_name` のコメント**: 「DETAIL と pg_locks.mode は後者の "Lock" を除いた形」は誤読を招く。PG17 は DETAIL も `pg_locks.mode` も `ShareLock` のように "Lock" を**付ける**（実機 E14・E18）。署名は変えない。コメントを「DETAIL にも pg_locks.mode にもそのまま使う」に直す。
2. **§4.2 `LockStatusRow` の項目追加**: `classid`・`objid`・`objsubid`・`waited`（§4.2）。`LockManager` の追加メソッド `with_knobs`・`held_locks`・`waiting_on`、`LockTag::describe`、`LockMode::{ALL, bit, from_sql_words}`。契約が「足りないものは追加してよい」とした範囲。
3. **§4.5 / §6 `StorageStack::new`**（レビュー対応 R-18 で改訂）: **署名は 00 §4.5 のまま `StorageStack::new(vfs, cfg, wal, next_xid)`**。`HeapStore::new` が要る `Arc<LockManager>`・`Arc<MultiXactTable>` と、`Clog::open` に渡す `oldest_xid` は `StackConfig`（`locks`・`multixact`・`oldest_xid` の 3 項目）で渡す（F0・C の担当。`recovery::startup` が `StackConfig` を受けるので REDO の前に `HeapStore` が組み立てられる）。`Clog` は `StorageStack::new` が作り（03 §6.8・C13）、`TxnManager` が `stack.clog` を使う。
4. **§4.3 `TxnManager::new` の引数に `multixact: Arc<MultiXactTable>` を足す**。`commit` / `abort` / `XidGuard::drop` が `multixact.on_xact_end` を呼ぶため。**`MultiXactTable::new` は `Result` を返さない（`control.get().next_multi` を読むだけ）形に確定した**（R-06。00 §4.4・02 §4.3 を同じ形に直した。02 の依頼 13）。
5. **§3.1 用語表・§4.3 のカタログ用スナップショット**: 「登録しない」を「**登録する（短命）**」に変える（LK-D9、M5-LK-Q2）。`TxnManager::snapshot()`（登録しない）は残し、使う場所を内部の診断とテストに限る。
6. **§4.3 `Snapshot` / `RegisteredSnapshot`**: `RegisteredSnapshot::for_statement(own, curcid)`（RR の文のスナップショット。XID を後から取る場合の `own_xid` の差し替え。§4.4）を足す。02 章（RW-4）が使う。
7. **§4.3 `Clog::set_status`**: 実装を `fetch_or` ではなく `compare_exchange` のループにする（LK-D10。公開署名と意味は M3 のまま）。契約の「`set_status` は `fetch_or`」の文言を直す。
8. **ファイルの持ち主（§8）**: LK が追記するファイルを足す — `storage/buffer/track.rs`（ゲート・`checkpoint_lock`・`allow_pins`・`assert_may_wait`）、`checkpoint.rs`（`track` の呼び出しと LK-D23 の確認）、`debug_knobs.rs`（§6.7 の 6 項目）、`txn/lock/relation.rs`（新規。§6.3）。いずれも追記だけ。
9. **§4.1 `BackendRegistry`**: `register_if`・`BackendGuard::{id, info}` を足す（§4.5。任意で使える）。`BackendGuard` の Drop は登録簿から外すだけで、`LockManager` への登録・解放は 09 章の `SessionLocks` が持つ（LK-D21）。`Session` のフィールドの宣言順（`txn`、`session_locks`、`backend` の順）を F0 の `session/mod.rs` に守らせる。
10. **§4.9 `RuntimeInfo`**: `blocking_pids(pid) -> Vec<i32>` を足す（§4.9）。`pg_proc` に `pg_blocking_pids`（OID 2561）の行。`catalog/builtin/runtime.rs` に LK が 1 行足す。
11. **§4.10 仮想リレーションへの DML のエラー**: 契約の「INSERT / UPDATE / DELETE は 42809」は、PG17 の実測（E17）と違う。**INSERT / UPDATE / DELETE は `55000`**（`cannot insert into view "pg_locks"` ほか。DETAIL・HINT つき）、`TRUNCATE` / `DROP TABLE` は `42809 "pg_locks" is not a table`。エラーを返す関数は LK が提供し（§6.5）、呼ぶのはアナライザの DML（RW）と DDL（VC・M4）。
12. **M4 の `RelKind`**: `Virtual`（`'v'`）を足す。`TableDef.kind` が `Virtual` のとき `locator` は使われない。`CatalogReader::table()` は `Virtual` を返し、`relation_kind()` も返す（M4 の `catalog/mod.rs`・`reader.rs`。F0 の担当）。`pg_locks` の OID は **9811**（契約 §3.2 の 9800 番台の申請。08 章 C6 が `pg_roles` に 9810 を申請済み。PG17.11 の実測 12073 は使わない）。
13. **07 章（FK）への依頼**: DROP の第 2 段が使う `fk_peer_tables(rel, constraint: Option<&str>) -> Vec<Oid>`（相手の表の OID）を `catalog/store_fk.rs` に用意すること（§4.6 `RelationResolver::drop_peers`）。M4 の `TableDef.identity_seqs` から所有シーケンスを引くのは LK 側。
14. **04 章（準備済み文）への依頼（レビュー対応 R-19 で改訂）**: Bind が使う口は `Session::lock_statement_relations(&Statement) -> Result<Vec<(Oid, LockMode)>>`（§4.6。04 章 C7。**名前で取る**。生のパース木から集めて解決 → ロック → 再解決）。`Session::acquire_oids(&[(RelKey, LockMode)])` と `required_locks(&BoundStatement)` は §5.9 の検証（LK-D12）と、将来の最適化のために用意する。**準備済み文が前回の `LockSet` を持って OID でロックする最適化は M6**（§4.6 の記述と一致。04 の `PreparedStmt`（§4.2）にも `LockSet` は無い）。以前この項は「準備済み文は `LockSet` を持つ。Bind は前回の `LockSet` を使う」と書いていたが、古いまま残っていた。
15. **08・09 章への依頼**: (a) 09 章 §5.1 の `connect` は `txn.snapshot(None, ..)`（登録しない）で共有カタログを読むと書いているが、LK-D9・03 章 C3 に従い `take_snapshot`（短命）にすること。(b) `SessionLocks` が `register_backend` を 1 回呼び、`Drop` が `release_all(Session)` → `unregister_backend` を行う前提（09 章 §6.7 (4)）に、LK の `LockManager` は合わせてある（§5.12）。(c) 08 章 C5 の `check_catalog_select` は `RelationResolver::check_access` から呼ぶ（§4.6）。(d) 08 章 C6 の OID は 9810、LK は 9811（`pg_locks`）。
16. **§6 `Session` の `commit_transaction` / `rollback_transaction`**: LK が足す部分は `release_transaction_resources()` の呼び出し 1 行（§5.8）。`session/txn_ctl.rs`（RW の持ち主）の `TxnControl` の実装は、`commit_and_restart` が §5.8 の手順 1〜5 を行い Session スコープのロックを残すことを満たす。
17. **03 章（VACUUM）の C3・C5・C6 の受け入れ**: C3（カタログ用スナップショットを `take_snapshot` で登録する）は LK-D9 と一致。C5 の (1) `LockManager::has_waiters`、(2) `ProcArray` の固有メソッド `oldest_xmin_hint`（ロックなしの `AtomicU64`。単調に増える）と `is_in_progress` / `next_xid` / `oldest_xmin` を LK-3 が用意し、`RunningXids` の `impl` は 03 章が `storage/heap/prune.rs` に書く（トレイトがそちらにあり、`txn` は `storage` を使えない）、(3) 共有リレーションの `LockTag::Relation` は `db = 0`（§4.2、`RelKey`）、(4) スナップショットの計算と登録を P の中で一度に行う（§5.6）。C6 の `Clog::open(vfs, next_xid, oldest)`・`oldest()` の口は LK-3 が構造体に用意し、`truncate_before` / `sweep_old_segments` / `status` の `XX001` は VC-4 が足す。**同じ `clog.rs` を触るので、LK-3b（ロックなし化）→ VC-4 の順**にする。
18. **04 章の C7・C12 の受け入れ**: `Session::lock_statement_relations(&Statement) -> Result<Vec<(Oid, LockMode)>>`（§4.6）と、テスト用の `TxnManager::registered_snapshot_count()`（§4.4）。C1（Bind が 1・2 を行い、SELECT のポータルだけ 3・4 も Bind で行う）は、§5.9 の手順を Bind が分割して呼べるよう、`acquire_statement_locks`・`missing_locks`・`assign_xid`・スナップショットの取得が独立した関数であれば満たせる。
19. **08 章の C5・C6 の受け入れ**: `RelationResolver::check_access`（`pg_authid` の 42501。ロックの前、解決のたび）、`pg_locks` の OID は 9811（`pg_roles` は 9810）。`virtual_rel::registry()` に `&PgRoles` を足すのは AU（1 行）。
20. **02 章（RW）への依頼**: §5.8 の手順 5「カタログ: `mgr.snapshot(txn.xid, txn.cid)`」は登録しないスナップショットなので、`take_snapshot`（短命）に直すこと（LK-D9、03 章 C3）。RR の最初のスナップショットをロックの前に取る（RW-D10）手順は LK が §5.9 の 0' に取り込んだ。RR の文のスナップショットの組み立ては `RegisteredSnapshot::for_statement`（§4.4）を使える。heap の待ち（`wait_for_holders` など）は、待つ前にページのピンを外すか、対象のページのピンを残すなら `storage::buffer::track::allow_pins(1)`（§6.6）の下で呼ぶこと（規約 1。デバッグビルドで検出される）。
21. **10 章（TS）への依頼**: (a) KD-9 を、「`pg_locks` は 16 列すべてを持つ（`virtualxid`・`virtualtransaction` は NULL、`fastpath` は false、`waitstart` は待ち手だけ）。行の差は、`virtualxid` の行がない・インデックスのロックの行がない・`select * from pg_locks` の文自身の `AccessShare` の行がない」に直す。(b) 新しい KD: **XID を文の開始時に割り当てる**（何も書かない `UPDATE ... WHERE false` が XID を持つ。M5-LK-Q4）。(c) KD-11 は「`SHOW` だけ（`SET` は PG17 と同じ 55P02）」に直す。(d) CR-1（`is_blocked_by` の意味）・CR-4（`Session::backend_id` / `backend_pid`）・CR-5（変異スイッチの名前）は受け入れた（§4.6、§6.7）。(e) 共有テストのファイル名は 10 章 §6.5.1 に合わせた（§7.3）。


---

## 付録 A. 実機確認の結果と方法（PostgreSQL 17.11）

方法: `sandbox/pg.sh start`（17.11、trust、C ロケール。ポートとデータディレクトリは作業用に変えた）。`psql -X -h 127.0.0.1` を最大 4 本、名前付きパイプで標準入力をつないで並行に動かし、出力をファイルに溜めた（`VERBOSITY=verbose` で SQLSTATE と位置を出す）。表のロックは `pg_locks` を `pg_class` と結合して読んだ。「待つ」の判定は応答が来ないこと、デッドロックの時間は文の送信から応答までの時刻で測った。スクリプトは作業ディレクトリのもので、リポジトリには含めない。ソースは REL_17_STABLE の `proc.c`・`lock.c`・`deadlock.c`・`lmgr.c`・`lockcmds.c`・`lockfuncs.c`・`waitfuncs.c`・`namespace.c`・`postgres.c` を読んだ。

| # | 手順 | 結果 | 使った決定 |
|---|---|---|---|
| E1 | 1 接続で `begin; <文>; select ... from pg_locks join pg_class` | SELECT は `AccessShare`（a と a_pkey）。`select * from a, b for update` は両方 `RowShare`、`for update of a` は a が `RowShare`・b が `AccessShare`、副問い合わせの中の表も `for update` が及ぶ、`where exists (select 1 from b) for update` の b は `AccessShare`。INSERT / UPDATE / DELETE の対象は `RowExclusive`、`UPDATE ... FROM b` の b は `AccessShare`、`INSERT ... SELECT` の元は `AccessShare`。`COPY a FROM` は `RowExclusive`、`COPY a TO` は `AccessShare`、`EXPLAIN UPDATE` は更新の文と同じ、CTE も `AccessShare`。FK のある子の INSERT は親が `RowShare`、親の UPDATE（キー）/ DELETE は子が `RowShare`。**インデックスにも同じモードで行が出る**（INSERT の対象のインデックスは出なかった）。XID は書く文（行に触れたとき）と `for update`（行を取ったとき）で出る | LK-D14、§5.9 A |
| E2 | DDL | `CREATE TABLE` は新しい表の `AccessExclusive`・`Share`、`pg_type` / `pg_namespace` の object ロックだけ（既存の表のロックなし）。`CREATE INDEX` は表が `Share`。`DROP TABLE` は対象の `AccessExclusive`、**`DROP TABLE c`（c が p を参照）は p も `AccessExclusive`**、`DROP TABLE p CASCADE` は c も。`TRUNCATE` は `AccessExclusive`（+ `Share`）。`ALTER TABLE ... ADD COLUMN` / `ADD PRIMARY KEY` / `RENAME` は `AccessExclusive`。**`ADD FOREIGN KEY` は子・参照先とも `ShareRowExclusive`**。`DROP CONSTRAINT`（FK）は c・p とも `AccessExclusive`。`ANALYZE` は `ShareUpdateExclusive`。`CREATE SEQUENCE` は新しいシーケンスだけ。**`CREATE TABLE ... REFERENCES p` は p が `ShareRowExclusive`**。`ALTER TABLE ... OWNER TO` は `AccessExclusive`。`DROP INDEX i` は表が `AccessExclusive` | LK-D16、§5.9 A |
| E3 | シーケンス | `nextval` / `setval` / `currval` は `RowExclusive`、`select * from s` は `AccessShare`、`ALTER SEQUENCE ... RESTART` は `ShareRowExclusive`、`DROP SEQUENCE` は `AccessExclusive` | LK-D18 |
| E4 | `LOCK TABLE` | モード省略は `AccessExclusive`。`lock table u, a in share mode` は両方 `Share`。`ONLY` は受け付ける。同じ表に `Share` の後 `AccessExclusive` を取ると両方持つ | §6.4 |
| E5 | XID の割り当て | `lock table u in access exclusive mode` は XID を取る（`LogAccessExclusiveLock`）。`share` / `row exclusive` は取らない。`update ... where false` は取らず、行に触れる `update` と `select ... for update` は取る | LK-D24 |
| E6 | `LOCK TABLE` のエラー | ブロックの外は **`25P01`**（`lock table nosuch;` も 25P01 が先）。index・sequence は `42809 cannot lock relation "a_pkey"`、DETAIL `This operation is not supported for indexes.`（sequence は `... for sequences.`）。`42P01`（`relation "nosuch" does not exist`）。`in bogus mode` は `42601`。**`read only` のトランザクションでも全モードで成功**。`NOWAIT` は `55P03 could not obtain lock on relation "a"`、スキーマを書くと `"public.a"` | LK-D17、§6.4 |
| E7 | 割り込みと早期デッドロック | A が `AccessShare`、B が `AccessExclusive` を待つ、A が `RowExclusive` を要求 → **待たずに付与**。C の `AccessShare` は待つ。A・B が `AccessShare` を持ち B が `AccessExclusive` を待つとき A が `AccessExclusive` を要求 → **0.5 秒以内（待たず）に A が 40P01**。DETAIL は 2 行（どちらも `AccessExclusiveLock on relation 16403 of database 5`）。A の失敗した文でトランザクションが中断してロックが外れ、B の `LOCK TABLE` が成功 | LK-D3、LK-D11、§5.1 |
| E8 | FIFO と soft な待ち | A が `AccessShare` を持ち B が `AccessExclusive` 待ち、C が `AccessShare` を要求 → **C も待つ**。`pg_blocking_pids`: C = `{B}`、B = `{A}` | LK-D3、LK-D20 |
| E9 | soft deadlock | A が a に `AccessShare`、C が b に `RowExclusive` を持ち、B が a に `AccessExclusive` を待ち、C が a に `AccessShare` を要求（B の後ろ）、A が b に `AccessExclusive` を要求（C を待つ）→ **エラーにならない**（C の `AccessShare` が待ち行列の並べ替えで付与される） | LK-D1 |
| E10 | 行の待ちとタプルロック | A が行を更新中、B が同じ行を更新（待つ）、C も同じ行を更新。`pg_locks`: B が `tuple (0,4)` の `ExclusiveLock`（granted）と `transactionid` の `ShareLock`（待ち）、C が `tuple` の `ExclusiveLock`（待ち）。`pg_blocking_pids`: B = `{A}`、C = `{B}` | LK-D20、02 章 |
| E11 | 同名の CREATE の同時実行 | A が `create table dup`（未コミット）、B が同じ名前 → B は待ち、A のコミットで **`23505 duplicate key value violates unique constraint "pg_type_typname_nsp_index"`**、DETAIL `Key (typname, typnamespace)=(dup, 2200) already exists.`。`IF NOT EXISTS` でも同じ。A がロールバックすれば B は成功。シーケンスと表の衝突は `pg_class_relname_nsp_index` | LK-D15 |
| E12 | タイムアウト・NOWAIT とリレーションロック | `lock_timeout` は `select * from a` の待ちに効く（**位置つき**）。`select ... for update nowait` / `skip locked` も**テーブルロックは待つ**（`lock_timeout` で 55P03）。`statement_timeout` は 57014 `canceling statement due to statement timeout` | LK-D4、LK-D25 |
| E13 | VACUUM / ANALYZE | `lock table u in share update exclusive mode` と `vacuum u` / `analyze u` は互いに待つ（待ち手のモードは `ShareUpdateExclusiveLock`）。`row exclusive` とは両立 | 03 章 |
| E14 | 2 者のデッドロック（行） | **先に待ち始めた A が 1 秒後に 40P01**。DETAIL 2 行（`Process … waits for ShareLock on transaction 802; blocked by process ….`）、HINT `See server log for query details.`、CONTEXT `while updating tuple (0,2) in relation "d1"` | §5.4 |
| E15 | 3 者のデッドロック | 先に待った A が 40P01。DETAIL 3 行（A → B → C → A） | §5.4 |
| E16 | リレーションロックのデッドロック | `lock table` した 2 つの表を互いに SELECT → 先に待った A が 40P01。DETAIL 2 行（`AccessShareLock on relation 16408 of database 5`）。`LINE 1: select * from b;` と位置つき | §5.4 |
| E17 | 仮想リレーション（ビュー）への操作 | `insert into pg_locks` は `55000 cannot insert into view "pg_locks"`、DETAIL `Views that do not select from a single table or view are not automatically updatable.`、HINT `To enable inserting into the view, ...`（update / delete も）。`truncate pg_locks` は `42809 "pg_locks" is not a table`、`drop table pg_locks` は同じ + HINT `Use DROP VIEW to remove a view.`。`begin; lock table pg_locks in access share mode` は成功 | §6.5 |
| E18 | `pg_locks` の形・OID・関数 | 16 列: `locktype text, database oid, relation oid, page int4, tuple int2, virtualxid text, transactionid xid, classid oid, objid oid, objsubid int2, virtualtransaction text, pid int4, mode text, granted bool, fastpath bool, waitstart timestamptz`。`pg_locks` の OID は 12073（reltype 12075）、`pg_roles` は 12000（システムビューの OID は版ごとに変わりうるので、yuzhu は使わず 9811 を申請する）。`pg_blocking_pids` = 2561（`{23}` → 1007、volatile、parallel safe）、`pg_isolation_test_session_is_blocked` = 3378（`{23, 1007}` → 16）。mode は `ShareLock`・`ExclusiveLock` のように `Lock` つき | LK-D19、§4.7 |
| E19 | 設定 | `set deadlock_timeout = 0` → `22023 0 ms is outside the valid range for parameter "deadlock_timeout" (1 ms .. 2147483647 ms)`。`'300ms'` の `SHOW` は `300ms`、`1500` は `1500ms`、既定は `1s`。`set max_locks_per_transaction` → `55P02 parameter "max_locks_per_transaction" cannot be changed without restarting the server`、`SHOW` は 64。`set log_lock_waits = on` は成功（17.11 の権限は superuser） | §4.8 |
| E20 | 未コミットの DROP と同名の CREATE | A が `begin; drop table dd;`（未コミット）、B が `create table dd(b int)` → **待たずに** `42P07 relation "dd" already exists`（`heap_create_with_catalog`。カタログ検索がまだ古い行を見る）。A がコミットした後も、その B の文は失敗のまま | LK-D15 |
