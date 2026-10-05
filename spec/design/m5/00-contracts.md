# yuzhu M5 基本設計 00: 契約（共通の型・名前・モジュール構成・工程）

M5 のゴールは**実用化**です。複数の接続が同時に書き込み（行ロック・テーブルロック・デッドロック検出・Repeatable Read）、ドライバが既定のモード（Extended Query）で繋がり、パスワード認証（SCRAM-SHA-256）で守られ、複数のデータベースを持ち（CREATE / DROP DATABASE）、掃除（VACUUM）をしないと膨らみ続ける状態から抜けます。あわせて、型（numeric・日付時刻・char(n)・bytea・uuid・配列）と FOREIGN KEY を足します。

この文書は M5 の設計書全体（`01-` から `10-`）の**契約**です。M5 の設計書は章ごとに別の担当が並列に書き、実装も章ごとの担当が並列に進めます。章の間で食い違わないよう、クレートとモジュールの構成、用語、ID（OID・WAL・SQLSTATE・設定）の割り当て、主要な型とトレイトのシグネチャ、M3 / M4 の契約からの変更点、作業パッケージの一覧、ファイルの持ち主を、ここで先に固定します。**各章はこの文書に従う**。従えない事情が見つかったら、その章の最後の「契約への変更依頼」に書く（章の中で黙って変えない）。

- 要件定義: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- 前提の設計書: `spec/design/m1.md`、`m1-changes.md`、`m2.md`、`m3.md`、**`spec/design/m4/00-contracts.md`（M4 の契約。確定済み。M4 の章 01〜11 は並行して作成中）**。**M5 の契約は M4 の契約に従う。** M4 の名前・署名と食い違うところは §1.3 の突き合わせ表（A1〜A12）と §6 に書いてあり、M4 の章が後から変わったら、各章の担当は最初に突き合わせ直して「契約への変更依頼」に書く
- 調査資料（根拠）: `spec/research/m5-concurrency.md`（複数ライター・行ロック・RR・VACUUM）、`m5-protocol-auth.md`（Extended Query・SCRAM・ロール・CREATE DATABASE）、`m5-types-fk.md`（型・FOREIGN KEY）、`m3-mvcc.md`、`pg-compat-tools.md`、`m4-btree.md`、`m4-query.md`
- 迷ったら **PostgreSQL 17 と同じ挙動**を選ぶ。判断が分かれる点は推奨案で進め、各章の「確認事項」に「仮決め・理由・変えたい場合の影響」の形で書く。
- PostgreSQL のソースは REL_17_STABLE。`PG:<path>` は `https://github.com/postgres/postgres/blob/REL_17_STABLE/<path>` の略。「（未検証）」と付けた記述は、ソースや実機で確かめていない。

## 章の一覧

| 章 | ファイル | 略号 | 主題 |
|---|---|---|---|
| 00 | `00-contracts.md` | — | この文書 |
| 01 | `01-lock-txn.md` | LK | トランザクション基盤の再構成、ロックマネージャ、テーブルロック、デッドロック、観測 |
| 02 | `02-row-lock-rr.md` | RW | 行ロック、更新競合、MultiXact（簡易版）、EvalPlanQual、Repeatable Read、`FOR` 句、B+Tree の複数ライター対応（一意検査の待ち）、RETURNING の仕上げ |
| 03 | `03-vacuum.md` | VC | VACUUM、pruning、FSM、凍結、clog の切り詰め、TRUNCATE の M5 対応、B+Tree の項目削除、autovacuum |
| 04 | `04-extended-query.md` | XQ | Extended Query、パラメータ型推論、準備済み文、バイナリ形式の仕組み |
| 05 | `05-types-core.md` | TY | 型の枠組み（バイナリ I/O の型ごとの実体、`pg_type` の send / recv 列）、M4 が入れた numeric・char(n) の M5 分の穴埋め、bytea、uuid、配列 |
| 06 | `06-types-datetime.md` | TD | interval、time、タイムゾーンと DateStyle / IntervalStyle の完全対応、日時の関数、日時のバイナリ形式（date / timestamp / timestamptz の統合は M4） |
| 07 | `07-foreign-key.md` | FK | FOREIGN KEY |
| 08 | `08-auth-roles.md` | AU | SCRAM-SHA-256、pg_hba 相当、ロールの DDL |
| 09 | `09-database-ddl.md` | DB | CREATE DATABASE / DROP DATABASE、接続とデータベースの管理 |
| 10 | `10-tests-plan.md` | TS | テスト基盤、全体の工程と工数、未検証・確認事項の総覧 |
| 98 | `98-review-response.md` | — | レビュー対応（指摘の実在確認、反映先、却下の理由） |
| 99 | `99-questions.md` | — | 確認事項（仮決めの一覧と通し番号）。README.md が索引 |

---

## 1. 全体計画の見直しと決定

M5 の設計に入る前に、M1〜M4 の計画と 5 本の調査（M5 の 3 本、`pg-compat-tools.md`、`m4-*.md`）を通して見直した。その結果を、範囲（§1.1）、食い違いの決定（§1.2）、工程（§1.3）に書く。

### 1.1 M5 の範囲

| 分類 | M5 で行うもの | 章 |
|---|---|---|
| 複数ライター | グローバル書き込みロックの廃止。リレーションロック（8 モード）、XID ロック、行ロック（4 強度）、MultiXact（メモリ上だけ。Q-014）、デッドロック検出、`LOCK TABLE`、`SELECT ... FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE [OF] [NOWAIT / SKIP LOCKED]`、`pg_locks` | LK、RW |
| 分離レベル | Repeatable Read（40001）。Read Committed の EvalPlanQual（簡易版）。SERIALIZABLE は `0A000` のまま | RW |
| VACUUM | `VACUUM [(VERBOSE)] [table]`（M4 の「何もせず成功」を置き換える）、`ANALYZE`（`reltuples` / `relpages` の更新だけ）、機会的 pruning、FSM、凍結と clog の切り詰め、`TRUNCATE` の M5 対応（文そのものは M4 が実装済み）、B+Tree の項目削除。簡易 autovacuum は実装するが既定は無効 | VC |
| プロトコル | Extended Query（Parse / Bind / Describe / Execute / Close / Sync / Flush）、パラメータ型推論、バイナリ形式、`PREPARE` / `EXECUTE` / `DEALLOCATE` / `DISCARD` | XQ |
| 型 | **M4 が済ませるもの**: numeric、char(n)、date、timestamp、timestamptz、regclass / regtype、int2vector。**M5 で足すもの**: interval、time、タイムゾーンと DateStyle / IntervalStyle の完全対応、bytea、uuid、配列（1 次元・最小）、全型のバイナリ形式 | TY、TD |
| 制約 | FOREIGN KEY（MATCH SIMPLE / FULL、5 つの参照アクション、NOT DEFERRABLE のみ） | FK |
| 認証・ロール | SCRAM-SHA-256、`password`（平文）、`trust`、`reject`、`yuzhu_hba.conf`、`CREATE / ALTER / DROP ROLE / USER`（属性とパスワード） | AU |
| データベース | `CREATE DATABASE`（FILE_COPY 相当）、`DROP DATABASE`、**`ALTER DATABASE`（`ALLOW_CONNECTIONS`・`IS_TEMPLATE`・`CONNECTION LIMIT` の 3 オプション。09 DB-D14、+0.5 日）**、接続数の制限、別データベースへの接続 | DB |
| 付随（計画の穴を埋めるもの。§1.4） | `INSERT / UPDATE / DELETE ... RETURNING` の仕上げ（M4 は「後半・任意」で、Bound と物理プランに欄だけ用意する。D-26）、`COPY ... TO STDOUT` と CSV 形式（任意） | RW、XQ |

**M5 でも対応しないもの**: SAVEPOINT の実体（サブトランザクション）、SERIALIZABLE（SSI）、HOT、可視性マップとインデックスオンリースキャン、`VACUUM FULL`、インデックスのページ削除と再利用、DEFERRABLE な制約、MD5 認証、Unix ソケット、TLS、GRANT / REVOKE、`INSERT ... ON CONFLICT`、`ALTER TABLE` の大半（FK の ADD / DROP CONSTRAINT と、M4 の ADD PRIMARY KEY / UNIQUE を除く）、json / jsonb、enum、`DROP DATABASE ... WITH (FORCE)`、`pg_terminate_backend`、`pg_stat_activity`、numeric の `sqrt` / `exp` / `ln` / `power`、`to_char`、ユーザーテーブルの配列列、`timetz`、`CREATE SCHEMA`、`CREATE VIEW`、information_schema、pg_dump。実行すると `0A000` を返す（黙って無視しない）。

### 1.2 食い違いの決定（調査間・既存設計との）

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| D1 | M5 の範囲 | 依頼の 8 項目だけ／pg-compat-tools の M5 の A 項目（SAVEPOINT、配列、COPY TO、`LOCK TABLE`、RR）まで広げる | **依頼の 8 項目を主とし、§1.4 の「計画の穴」から RETURNING と COPY TO だけを足す** | 8 項目と付随項目で M5 は約 172.5 日分（§7。初版は 158 日。レビュー対応 R-20 で F0 の拡大・F1・各章の増分を足した）。SAVEPOINT は L で、足すと全体が崩れる |
| D2 | SAVEPOINT | M5 に追加（pg-compat §5。SQLAlchemy + psycopg 3 が接続時に使う）／M3-Q7 のまま `0A000`／M6 の先頭 | **M5 では実装しない（M6 の先頭候補）。ただし後から入れても作り直しにならないよう、「自分のトランザクションの XID か」の判定を `Snapshot::is_own` / `Transaction::owns_xid` に集める（§4.3）** | サブトランザクションは XID の採番・可視性・ロックの解放・clog・WAL のすべてに触れる。M5 の再構成が落ち着いてから入れる方が安い。ユーザーに見える影響は M5-Q1（§10.1）で扱う |
| D3 | SERIALIZABLE | 受け付けて RR として動かす（PostgreSQL の `READ UNCOMMITTED` と同様）／`0A000` | **`0A000` のまま**（m5-concurrency §7.2） | RR で動かすと write skew を黙って許し、SERIALIZABLE を前提にしたアプリが壊れる。SSI は M6 以降 |
| D4 | `server_version` | 16.0 のまま（m5-protocol-auth C-14）／17.0（M2 D11） | **17.0** | M2 で 17.0 に上げ済み。調査は M2 の前に書かれた |
| D5 | 書き込みの直列化 | グローバル書き込みロックを残す／廃止してテーブルロックと行ロック（m5-concurrency §9.4） | **廃止する。`TxnManager::begin_write` / `WriterGuard` / `writer_owner` / `is_blocked_by` を無くし、`assign_xid` / `XidGuard` と `LockManager` に置き換える** | 要件の「複数ライター」そのもの。M3 の `begin_write` は XID の採番と同じ関数だったので、XID の採番だけを残す |
| D6 | 行ロックと MultiXact | 案 A 排他のみ／案 B メモリ上のロック専用／案 C 永続／案 D（m5-concurrency §4.2）。Q-014 | **案 B。更新者（UPDATE / DELETE した XID）は MultiXact に入れない。ロック同士の共存は PostgreSQL の表（README.tuplock）どおり。更新者とロック保持者は常に衝突する** | Q-014 のとおり。結果は PostgreSQL 9.2 以前と同じで、FOREIGN KEY の子の INSERT 中に親の非キー列 UPDATE が待たされる。共有テストにはこの差が出るケースを入れない |
| D7 | 更新競合の待ち | 実行器で待つ／heap 層の中で待つ（m5-concurrency §5.2） | **heap 層の中で待つ**（PostgreSQL の `l1:` / `l2:` / `l3:` ループの移植）。待ちは `LockManager::wait_for_xact`。待つ前にページのラッチを必ず外す | 「ラッチを離す→待つ→取り直して xmax が変わっていたらやり直す」は heap の内部状態と一体 |
| D8 | EvalPlanQual | PostgreSQL と同じ計画木の部分再実行／簡易版（m5-concurrency §6.2） | **簡易版。UPDATE / DELETE / LockRows のノードが「入力行のうち対象テーブルの部分を最新版に差し替え、WHERE（と結合条件）全体と SET 式を評価し直す」。そのため planner は `RecheckSpec` を計画に載せる（§4.6）** | 単一テーブルでは PostgreSQL と一致。結合は元の入力行の他テーブル部分を固定する（PostgreSQL の ROW_MARK_REFERENCE と同じ意味） |
| D9 | デッドロック検出 | soft edge の並べ替えまで／hard + soft の検出だけ（m5-concurrency §10） | **`deadlock_timeout`（既定 1 秒）の後に待った本人が検査。soft edge の並べ替えはしない。`ProcSleep` の割り込み規則（保持ロックと衝突する先行者の前に入る）は入れる。被害者は検出した本人（40P01）** | PostgreSQL と同じ被害者の選び方。soft deadlock の稀なケースだけ差が出うる |
| D10 | ストレージバリア | 文の実行中は共有、DROP のコミットは排他（M2 §6.7.5）のまま／縮小 | **縮小する。文の実行は共有バリアを持たない。DROP / TRUNCATE が消すファイルは、そのリレーションの AccessExclusive ロックで守る。バリア（`RwLock`）は「チェックポイントのバッファ書き出し ⇄ ファイルの削除」だけに使う** | 今のままだと、長い SELECT が走っている間の DROP のコミットが、新しい文をすべて止める（書き手待ちの `RwLock`）。リレーションロックが入れば不要 |
| D11 | スナップショットと horizon | 登録なし／バックエンドごとに `xmin` を公開（m5-concurrency §12.1） | **スナップショットを `TxnManager` に登録する（`RegisteredSnapshot`。Drop で外れる）。`oldest_xmin() = min(実行中の XID, 登録済みスナップショットの xmin, next_xid)`。RR のスナップショットはトランザクション終了まで、ポータルのスナップショットはポータルを閉じるまで登録を保つ** | VACUUM が消してよい境界の計算。PG の実機 #11 / #11b / #11c と同じ結果になる |
| D12 | カタログの読み取りのスナップショット | RR でもトランザクションのスナップショット／常に最新（m5-concurrency §7.2、§9.3） | **カタログは常に「文ごとに取り直す最新のスナップショット（自分の変更を含む）」で読む。ユーザーテーブルの実体の読み取りだけが RR のスナップショットを使う。カタログ用スナップショットも `take_snapshot` で登録する（短命。名前解決は 1 回ごと、アナライズは文の終わりまで。LK-D9、VC-D23。レビュー対応 R-02）** | PostgreSQL の `GetCatalogSnapshot`（登録する）。RR で他者が作ったテーブルが 42P01 になるのを防ぐ。登録しないと、走査中のカタログの行を同時に走る VACUUM が除去する窓が開く（03 §5.3.5 の P4）。登録しない `TxnManager::snapshot()` は単体テストと `pg_current_snapshot` 風の診断だけに使う |
| D13 | ヒントビットと clog | M5 でヒントビットを入れる（m3 D7 の予告）／入れない | **入れない。`Clog` の読み取りをロックなしに近づける（`status` は `RwLock` の読み + `AtomicU8`、`set_status` は `compare_exchange` のループで「現在 0（InProgress）のときだけ」立てる。`fetch_or` だと Committed と Aborted の二重設定が予約値 `0b11` になり矛盾を検出できない。01 LK-D10。R-37）。ヒントビットは `XMAX_INVALID`（M2）と `XMIN_FROZEN`（凍結。両ビット）だけを読み書きする。`XMIN_COMMITTED` / `XMIN_INVALID` / `XMAX_COMMITTED` の単独のビットは M5 でも読まず書かない** | ヒントビットを書くにはページチェックサムのため FPI が要る（M3 D7）。clog の読みを速くするだけで同じ目的を達する |
| D14 | 64 ビットの凍結境界 | `pg_class.relfrozenxid` を `xid8` 型にする／32 ビット列は下位だけにして 64 ビットは別カタログ（m5-concurrency §12.6、M2 §6.7.2） | **別カタログ `yz_relxid`（OID 9801、データベースごと）に `(relid oid, relfrozenxid8 int8)` を持つ。`pg_class.relfrozenxid` には下位 32 ビットを書く（表示用）** | `tests/slt/m2/catalog/catalog_columns.slt` が `relfrozenxid` の型 `xid` を PostgreSQL と比べている。型を変えると共有テストが壊れる |
| D15 | VACUUM の範囲 | m5-concurrency §12 の全部（VM、末尾切り詰め、VACUUM FULL、HOT）／削る | **3 段階の VACUUM + 機会的 pruning + FSM + 凍結 + clog の切り詰め + 末尾の切り詰め（条件付き AccessExclusive）。VM・HOT・VACUUM FULL・インデックスのページ削除は M6** | 正しさのリスクが小さい範囲で、膨らみ続けない状態に届く |
| D16 | FSM | 作らない（M2-Q17）／M5 で作る | **作る。ヒープの FSM（`Fsm` フォーク）は WAL を書かず、読み込みは「チェックサム不正なら 0 のページとして扱う」（zero-on-error）。FSM は導出データで、VACUUM が作り直せる** | VACUUM が空けた領域を再利用しないと、テーブルが伸び続ける |
| D17 | autovacuum | M5 に入れる／M6 | **実装する（WP VC-6。カットライン上位）が、既定は無効（`autovacuum = off`）。M6 で soak 試験をしてから既定を有効にする** | 並行試験・クラッシュ試験の再現性を保つ |
| D18 | B+Tree の複数ライター | M4 の設計に任せる／M5 で洗い出す | **M5（WP RW-5）で、M4 の B+Tree の「単一ライター前提」を洗い出して直す。一意検査の `Conflict::WaitFor(xid)` は `wait_for_xact` で待って降下からやり直す。項目の削除（`bulk_delete`）は VC-3。ページの削除と再利用は M6** | 調査（m5-concurrency §11）が「別調査」と残した穴 |
| D19 | Extended Query の計画 | Bind ごとに計画／汎用プランを保存（m5-protocol-auth §2.8） | **Bind ごとに計画（planner）をやり直す（汎用プランは作らない）。解析結果（`Arc<BoundStatement>`）は、解析の鍵 `AnalysisKey`（カタログ世代・search_path・DateStyle・TimeZone）が変わらず、自分のトランザクションがカタログを変更していなければ使い回し、変わっていれば Bind（ロックの後）で再解析する。準備済み文は SQL テキスト・AST・宣言された型・結果の列・解析結果・解析の鍵を持つ。参照リレーションの一覧は持たず、Bind のたびに生のパース木から集めてロックする**（04 XQ-D6・XQ-D7・C14。レビュー対応 R-15で D19 の元の文面「Bind ごとにアナライズからやり直す」を改めた） | M4 の planner は軽く、パラメータ値で計画が変わる場面は少ない。解析（名前解決・型付け）は世代が同じなら結果が同じなので、ORM の同じ文を Bind するたびに繰り返さない |
| D20 | バイナリ形式 | 全型／一部 | **M1 の型と M5 で足す全型（numeric・日付時刻・bytea・uuid・bpchar・配列）の send / recv を作る。型ごとの形式は各型の章が決める** | tokio-postgres はパラメータも結果もバイナリ |
| D21 | 準備済み文とロック | Parse で取る／Bind（実行）で取る | **Bind で取る。Parse はロックを取らず、カタログの世代だけを記録する。Bind は「ロック→世代の比較→必要なら再アナライズ」の順** | PostgreSQL の `AcquireExecutorLocks` と同じ。Parse のロックは Sync で解放されうる |
| D22 | numeric・日付時刻の実装 | 新規に書く／既存クレートを統合する | **`yuzhu-numeric` と `yuzhu-datetime`（作成済み。PostgreSQL との差分コーパス試験つき）の `yuzhu-core` への統合は M4 が行う（M4 D-3、D-4。`Datum::Numeric` / `Date` / `Timestamp` / `TimestampTz`、`types/{numeric,datetime,bpchar}.rs`）。M5 は `interval`・`time` の統合、`TimeZone` / `DateStyle` / `IntervalStyle` の完全対応、関数、バイナリ形式を足す** | M4 が先に統合する（Q-011、Q-012）。M5 の作業はその上に載せる |
| D23 | numeric の数学関数 | M5／M6（m5-types-fk C-3） | **M6**。`yuzhu-numeric` に `sqrt` / `exp` / `ln` / `log` / `power` が無い。**numeric 版の行（OID は PG17）だけ `pg_proc` / `FUNCTIONS` に入れ、本体は `0A000 numeric <name> is not supported yet`（05 TY-D15。M4 の `^` と同じ）。numeric の引数（`sqrt(2.0)`）は float8 版に暗黙変換されず `0A000` になる（PG は numeric を返すので、float8 を黙って返すより安全）。整数・float8 の引数（`sqrt(2)`、`sqrt(2::float8)`）は従来どおり float8 版で動く**（レビュー対応 R-34。D23 の元の文面「numeric 以外の引数の `sqrt(2)` は float8 で動く」を、numeric リテラルは `0A000` と明記した。ユーザーに見える差なので 05 M5-TY-Q13、00 §10.1 の M5-Q17 に載せた） | 結果の桁数の規則が関数ごとに違い、使用頻度が低い |
| D24 | DateStyle / IntervalStyle | ISO だけ（m5-types-fk C-10）／クレートが対応する全形式 | **クレートが実装している全形式を受け付ける**（`ISO` / `SQL` / `Postgres` / `German`、`postgres` / `postgres_verbose` / `sql_standard` / `iso_8601`）。変更したら ParameterStatus を送る | 追加の工数がない |
| D25 | 配列 | 最小実装（m5-types-fk §7）／pg-compat の A 項目（`ARRAY(SELECT)`、添字、SELECT 句の SRF）まで | **ディスク形式は PostgreSQL の `ArrayType` と同じ（多次元に拡張できる）。M4 が `int2vector` と一緒に持つ「1 次元の `int2[]`（`Datum::Int2Vector`、yuzhu 独自の符号化）」は、M5 で `Datum::Array` に置き換える（`int2vector` だけが `Int2Vector` のまま残る。カタログの `int2[]` / `oid[]` 列の符号化が変わるので `catalog_version` を上げる。D35）。M5 の実装は 1 次元・要素はスカラー型すべて・NULL 要素あり。式は `ARRAY[...]`、`ARRAY(SELECT ...)`、`a[i]`（読み取り）、`= ANY` / `<> ALL`、`::T[]`、`array_length`、`cardinality`、`array_to_string`。`unnest` と SELECT 句の SRF、`array_agg`、スライス、代入は M6。**ただし例外が 1 つ: SELECT 句に SRF が 1 つだけで、ほかの出力列も FROM 句もない `SELECT srf(args)` は `SELECT * FROM srf(args)` に書き換えて受け付ける（05 TY-D17、TY-5c。+0.5 日）。psql 17 の `\d tbl`（FK のある表の `Referenced by`）が `pg_partition_ancestors` をこの形で使うため。レビュー対応 R-28**。ユーザーテーブルの配列列は `0A000`** | psql `\d tbl` に要る範囲（TY の確認事項で `\d` の問い合わせから確定する）。列を許すのはディスク形式が済んでいれば 1 行の変更 |
| D26 | FOREIGN KEY の実装方式 | PostgreSQL と同じ内部トリガー／実行器に組み込んだ RI（m5-types-fk §9.2） | **実行器に組み込む。`pg_trigger` には行を出さない（既知の差）。検査は文の終わりにキューを処理する** | トリガー機構は M5 の範囲外 |
| D27 | 並行下の FOREIGN KEY | 行ロック（案 a）／キー値ロック表（案 b） | **案 a。親の行に `FOR KEY SHARE`（D6 の簡易版）を取る** | D6 を採ったので自然に乗る |
| D28 | 認証方式 | m5-protocol-auth §4.7 | **`trust`、`reject`、`password`、`scram-sha-256`。MD5 は実装せず、hba の `md5` は `scram-sha-256` として扱う（PostgreSQL と同じ解釈）。`ALTER ROLE ... PASSWORD 'md5...'` は `0A000`** | PG18 で非推奨 |
| D29 | 暗号の依存 | core に直接／専用クレート | **新クレート `yuzhu-auth` に閉じ込める（`sha2`・`hmac`・`pbkdf2`・`base64`・`getrandom`・`subtle`・`stringprep`）。`yuzhu-core` は `yuzhu-auth`・`yuzhu-numeric`・`yuzhu-datetime` に path 依存するが、crates.io の外部クレートには直接依存しない** | CLAUDE.md は暗号を外部クレートの許容範囲とする。`gen_random_uuid()` と BackendKeyData の乱数も `yuzhu-auth` の乱数を使う |
| D30 | CREATE DATABASE | FILE_COPY／WAL_LOG（m5-protocol-auth §7.1） | **FILE_COPY 相当（チェックポイント→ディレクトリのコピー→`DBASE_CREATE` を WAL→行の挿入→コミット→チェックポイント）。`STRATEGY = WAL_LOG` は受け付けて FILE_COPY で実行する。テンプレートのデータベースへの新しい接続は、全体が終わるまで `Database(oid)` ロックで待たせる** | M2 の `copy_database_dir` を使える |
| D31 | 孤児ディレクトリ | 起動時に消す（m5-protocol-auth C-10）／消さない（M3 D15） | **消さない。起動時に `pg_database` に無い `base/<oid>/` を WARNING に出すだけ** | M3 の方針（誤って生きたデータを消す危険）と合わせる |
| D32 | DROP DATABASE の順序 | PostgreSQL（無効の印→ファイル→行）／yuzhu（行→ファイル）（m5-protocol-auth §7.2） | **行の削除をコミット→バッファを捨てる→`DBASE_DROP` を WAL→ディレクトリを削除**。途中のクラッシュはディレクトリが残るだけ（D31） | 行が先に消えるので「壊れた DB」の状態が要らない |
| D33 | 観測用の表 | ビューの仕組みを作る／仮想リレーション | **仮想リレーション（`catalog::virtual_rel`。relkind `v` の行を `pg_class` に持ち、読み取りは提供関数が行を作る）。M5 で使うのは `pg_locks` と `pg_roles`** | ビュー（ルール）の仕組みは M5 の範囲外 |
| D34 | ソースの分割 | 各担当が既存ファイルに足す／先に分割する | **先に機械的に分割する（WP F0a。M4 のマージ後）。各章が要る口は F0b-1（LK・RW・VC）と F0b-2（そのほか）で足し、M4 のファイルへの M5 の修正は F1 が 1 人で行う（R-20）。`session.rs` → `session/`、`catalog/builtin.rs` → `catalog/builtin/`。DDL・ユーティリティ文の実行は M4 が作る `ddl/`（D41）に足す** | `session.rs`（1,750 行。M4 後はさらに増える）と `builtin.rs`（1,970 行。M4 で numeric などが入って増える）に 10 人が同時に触ると衝突する |
| D35 | データディレクトリの互換 | 読む／initdb のやり直し（M2 D15、M3 D25） | **制御ファイルの `format_version` を 3 に上げ、`catalog_version` を上げる。M3 / M4 のクラスタは起動を拒否する** | M5 まで互換を保証しない方針（M2-Q20） |
| D36 | MultiXact ID の採番 | 別カウンタ／XID と共用 | **別カウンタ（64 ビット）。制御ファイルの `next_multi`（オフセット 120）に XID と同じ先取り方式で記録する。起動時の `multi_boundary` 未満の ID は「メンバー全員が終了済み」とみなす** | 再起動後に古い xmax の ID と新しい ID が衝突しないようにする |
| D37 | XID ロックの登録 | `assign_xid` の後／中 | **`proc` の Mutex の中で `TransactionId(xid)` を Exclusive で登録する（ロック順序の例外。§5.3）** | 実行中一覧に載ってからロックが載るまでの隙間を無くす |
| D38 | コミット時の順序 | M3 §5.3 に「ロック解放」を足す | **WAL の挿入と flush → clog → 実行中一覧から外す → キャッシュの無効化 → ファイルの削除 → ロックの解放。ロックの解放が最後** | 待っていたセッションが起きたときに、必ず相手の結果が見えるように |
| D39 | M3 の分離性 spec | そのまま使う／M5 の意味に直す | **`tests/isolation/specs` の M3 の spec のうち書き込みの直列化に依存するもの（`writer-queue`、`writer-waits-writer`、`lock-timeout` の一部）を、M5 の意味に書き直す。期待ファイルは PostgreSQL 17 で生成し直す** | PostgreSQL と同じ結果になる（M3-Q20 の既知の差が消える） |
| D40 | pg_database の接続数の管理 | 別の表を持つ／ロックマネージャ | **各セッションが `Database(db_oid)` を AccessShare でセッションスコープに保持する。CREATE DATABASE のテンプレートと DROP DATABASE の対象は、自分以外のセッションが居ないことを `try_acquire(.., AccessExclusive)` で確かめる（**最長 `database_busy_wait`（既定 5 秒）の 100ms ポーリング**。09 DB-D5。R-37）** | 新しい接続を止める仕組みと「他の接続がある」の判定を 1 つにできる |
| D41 | DDL・ユーティリティ文の置き場所 | M5 が `commands/` を新設（M4 の契約を読む前の案）／M4 D-29 の `ddl/`（`DdlCtx`、`ddl::execute(&mut DdlCtx, BoundDdl)`）に足す | **M4 の `ddl/` に足す。`commands/` は作らない。`LOCK TABLE`・`VACUUM` / `ANALYZE`・ロール・データベース・FK の ALTER は `BoundDdl` の変種として足し、アナライザ（`analyzer/ddl.rs`）が作る。`DdlCtx` に `backend`・`wait`・`session`・`role`・`outside_block` を足す（§4.7）。トランザクションを自分で閉じて開き直す 3 文（VACUUM、CREATE / DROP DATABASE）は `ddl::execute_standalone(&mut dyn TxnControl, ..)` を通す。`PREPARE` / `EXECUTE` / `DEALLOCATE` / `DISCARD` は Session の状態を触るので `ddl/` ではなく `session/prepared.rs` が直接処理する** | 同じ役割の入口を 2 つ作らない。M4 の `BoundStatement::Ddl` の経路にそのまま載る |
| D42 | `TRUNCATE` | M5 の VC-5 が実装（M4 の契約を読む前の案）／M4 D-11 が実装済み | **M4 が新しい relfilenode を作る方式で実装する（ロールバック可能）。M5 は、(1) AccessExclusive ロック、(2) 外部キーから参照されている表への TRUNCATE の拒否（**参照元がすべて同じ TRUNCATE に含まれていれば通し、含まれない参照元があれば `0A000`。PostgreSQL と同じ**。`TRUNCATE ... CASCADE` で文に含まれない参照元を巻き込む場合だけ `0A000`。判定は 07 の `ddl::constraint::check_truncate_fks`、呼ぶのは VC-5。07 FK-D18、03 VC-D17。レビュー対応 R-07 で「無条件に拒否」から改めた）、(3) `yz_relxid` と FSM の作り直し、(4) VACUUM との排他、を足す** | 文そのものを 2 回作らない |
| D43 | char(n) の `Datum` | `Datum::Text`（空白埋め。M4 の契約を読む前の案）／M4 D-5 の `Datum::BpChar(String)` | **`Datum::BpChar`（M4）。M5 は変えない** | 比較・ハッシュが型を渡さずに正しく動く（M4 §12.2） |
| D44 | 一意検査の待ち（M4 の口の使い方） | M4 の `DirtyResult::WaitFor` を内部エラーにする口をそのまま使う／戻り値で待ちを返す | **`IndexStore::insert` の戻り値を `Result<InsertOutcome>`（`Inserted` / `WaitFor(Xid)`）に変える。`WaitFor` のときは何も挿入していない。呼び出し側（`executor::dml::insert_with_indexes`）が `wait_for_xact` で待ち、ヒープの挿入を済ませたまま同じインデックスへの挿入をやり直す（PostgreSQL の `_bt_doinsert` の `xwait` ループと同じ）。ページのラッチは待つ前にすべて外れている** | 待つ前にラッチを持たない規約 1。B+Tree が `LockManager` を知らずに済む |
| D45 | 外部パラメータの式 | 別の式の木を作る／M4 の `Expr<C, Q>` の `ExprKind` に 1 変種足す | **`ExprKind::ExternParam(u16)`（`$n` の n − 1）を `expr/mod.rs` に 1 回だけ足す。Bound・論理・物理のすべての層に現れてよい。型は `Expr.ty`。M4 の `PhysCol::Param`（相関サブクエリ用）とは別物** | M4 の規約 1（式の木を複製しない） |
| D46 | RETURNING | M5 で新規に作る（M4 の契約を読む前の案）／M4 D-26 の「後半・任意、Bound と物理プランに欄、未実装なら `0A000`」の続き | **M4 が `BoundReturning`・`returning: Option<Vec<PhysExpr>>` の欄を用意する。M5（RW-6）は M4 が終えていない部分（UPDATE / DELETE の FROM / USING の列の参照、Extended Query の Describe、`LockRows` との組み合わせ）を仕上げる。M4 が完了していれば RW-6 は縮む** | 欄は M4 にある |
| D47 | B+Tree の WAL の info と配置 | `0xC0 VACUUM`（M4 の契約を読む前の案。m4-btree の予約）／M4 §13.4 は `0x00 INSERT_LEAF`、`0x10 PAGES`、`0x20` 以降を M5 の予約にした | **`BTREE_VACUUM = 0x20`（項目の削除。1 レコード・1 ページ）。`BTREE_DELETE` などは予約のまま（M6）。B+Tree のファイルは M4 の `storage/btree/`（`storage/index/` は作らない）。M5 の追加は `storage/btree/vacuum.rs`** | M4 の確定表が正 |
| D48 | XID を持たないトランザクションのコミット | M5 が `commit` の経路を作り直す／M4 の `TxnManager::finish_without_xid(flush_upto)` を残す | **残す。XID が無くても `wal_flush_upto`（シーケンスの WAL）まで flush してから、ロックを解放する（§5.2 の手順 1 の前）** | M4 D-9 |
| D49 | M5 の実装の開始時期 | M4 と並行して全部始める（M4 の契約を読む前の案）／M4 のマージ後に始める | **コードを動かす F0（`session.rs`・`builtin.rs` の分割と、各章が要る口。F0a・F0b-1・F0b-2。§7）は M4 のマージ後。M4 が触らない新規ファイル・新規クレートだけの作業（AU-1 の `yuzhu-auth`、TD-4 の `yuzhu-datetime` の `Time`、`LockManager` 単体（LK-1、LK-2。`txn/lock/` は新規）、TS-1a・TS-4a・TS-5a、DB の設計の検証）は M4 の実装中に始めてよい。XQ は先行リストに入れない（既存の `connection.rs`・`messages.rs`・`codec.rs` と、M4 の COPY が触るファイルに当たる。04 §8。レビュー対応 R-17）** | F0 は M4 が変更する 2 ファイルを動かす |
| D50 | M3 の SAVEPOINT 構文 | M5 で実装／M3 の `0A000` のまま | **M3 のまま（`0A000`。D2）** | D2 |
| D51 | データベースをまたぐ凍結境界（clog の切り詰め） | `pg_database.datfrozenxid`（32 ビット）だけに頼る（M2 §6.7.2 が不可とした）/ 共有カタログに 64 ビットの境界を持つ | **共有カタログ `yz_datxid`（OID 9802、`global/`、`(datid oid, datfrozenxid8 int8)`。`db_oid = 0`）。initdb が行を作り（template1・postgres = 3、**template0 = 番兵 `DATFROZEN_PRISTINE`（`i64::MAX`）**）、CREATE DATABASE が `min(テンプレートの行の値, TxnManager::oldest_xmin())` の行を作り（番兵を継承しない）、DROP DATABASE が消し、VACUUM が HEAP2 INPLACE の上書きで進める（03 VC-D13・C11、09 DB-D12。レビュー対応 R-08）。読み書きの関数は VC の `catalog/store_vac.rs` だけが持つ。`pg_database.datfrozenxid` には下位 32 ビットを書く。clog の切り詰めの境界 `oldest_xid = min(全データベースの datfrozenxid8, 登録済みスナップショットの xmin, 実行中の XID)`。template0（接続不可）も含める** | D14 と同じ理由。行の作成と削除は DB 章、進めるのは VC 章 |

### 1.3 工程

#### M4 の確定した契約との突き合わせ

`spec/design/m4/00-contracts.md`（以下「M4 契約」）を読んで確かめた結果。**M5 の各章は M4 契約の名前を正とする。** M4 の章（01〜11）が後から変えたら、各章の担当は最初にこの表と突き合わせ直し、食い違いを「契約への変更依頼」に書く。

| # | M4 契約の内容（出典） | M5 の扱い | 使う章 |
|---|---|---|---|
| A1 | 解析・計画・実行は「AST → Bound（`Var`）→ 論理プラン（`ColId`）→ 物理プラン（`PhysCol`）→ `Executor`」（M4 §5）。`PhysicalPlan::{Insert, Update, Delete}` が `RelHandle`（`indexes: Arc<[IndexHandle]>` を持つ）を持つ。`Executor::rewind` あり。`ExecCtx` は `&mut Transaction`、`&Snapshot`、`&dyn TableStore`、`&dyn IndexStore`、`params`、`mem` などを持つ（M4 §10） | **そのまま使う。** M5 は `ExecCtx` に項目を足し（§4.6）、`PhysicalPlan` に `LockRows`・`VirtualScan` を、`Update` / `Delete` に `recheck` を足す。論理プランにも `LockRows` と、`Update` / `Delete` の `recheck` を足す（RW-3） | RW、XQ、FK |
| A2 | 式の木は `Expr<C, Q>` の単一の定義（M4 §6）。`PhysCol::Param(ParamId)` は相関サブクエリ・`NestedLoopParam` 用 | **`ExprKind::ExternParam(u16)` を足す（D45）。** `PhysCol::Param` は使わない | XQ |
| A3 | B+Tree は `storage/btree/`。`IndexStore`（`init_index` / `insert` / `build` / `begin_scan` / `scan_next` / `nblocks` / `unlink_storage`）。`UniqueCheck::Check` と `DirtyResult::WaitFor`（M4 では起きない）。WAL は `BTREE_INSERT_LEAF = 0x00`、`BTREE_PAGES = 0x10`（全画像・最大 32 ブロック）、`0x20` 以降は M5 の予約。`DELETED` / `HALF_DEAD` / `INCOMPLETE_SPLIT` と `cycleid` を予約（M4 §13、§19）。`pg_index`・`pg_constraint`（contype `p` / `u`）・`pg_depend`・opclass の静的な表あり | **`IndexStore::insert` の戻り値を `InsertOutcome` に変える（D44）。`BTREE_VACUUM = 0x20`（D47）。`IndexStore::bulk_delete` を足す（VC-3）。ページの削除・再利用（フラグの使用）は M6** | RW、VC、FK |
| A4 | numeric は `yuzhu-numeric` を統合済み（`Datum::Numeric`、`types/numeric.rs`、ディスク形式は PostgreSQL の NumericVar と同じ 10000 進の `u16` 列。M4 §12.3）。日時は `date` / `timestamp` / `timestamptz` を統合済み。`interval` / `time` / `timetz` は M4 では `0A000`。`char(n)` は `Datum::BpChar`（D43）。`int2vector` と 1 次元の `int2[]` は `Datum::Int2Vector`（yuzhu 独自の符号化。M4 §12.3） | **numeric・char(n)・date・timestamp・timestamptz は M5 で作らない。** M5 は `interval`・`time`（TD）、bytea・uuid・配列（TY）、`int2[]` の `Array` への置き換え（D25）、全型のバイナリ形式を足す | TY、TD |
| A5 | `COPY ... FROM STDIN`（テキスト。Simple Query のみ）、`generate_series`（FROM 句）、`UPDATE ... FROM` / `DELETE ... USING`、シーケンスと SERIAL / IDENTITY、`CREATE INDEX`、`ALTER TABLE ADD PRIMARY KEY / UNIQUE`、`TRUNCATE`（新しい relfilenode）、`VACUUM` / `ANALYZE`（何もせず成功。`BoundDdl::Vacuum`）、EXPLAIN、正規表現、集約（`BuiltinAggregate`） | M5 は `VACUUM` / `ANALYZE` を本物に置き換え（VC）、`TRUNCATE` に M5 の対応を足し（D42）、COPY は Extended Query 経由と TO / CSV だけを足す（XQ-6） | XQ、VC |
| A6 | シーケンスの更新は MVCC を使わずページの排他ラッチの下でその場上書き（M4 D-9、§19）。`SeqRun.wal_lsn` を `Transaction.wal_flush_upto` に反映 | **変更なし。** 複数ライターでも安全（M4 §19 のとおり）。コミットでは `wal_flush_upto` まで flush する（D48） | RW、LK |
| A7 | `IndexScan` は葉ごとに一致した TID をコピーし、ピンもラッチも持ち越さない（M4 §13.2）。ヒープの行ポインタを再利用しない前提（M4 は VACUUM が何もしない） | **M5 はこの前提を、(1) VACUUM の 3 段階の順序（インデックスから消してから `LP_UNUSED`）、(2) スキャンの MVCC スナップショットが登録されていて horizon を止めること（見えていたタプルは消えない。D11）、(3) 一意検査の `fetch_dirty` は葉のラッチを持ったまま行うこと、で守る。** 証明は VC が書く | VC |
| A8 | `RETURNING` は M4 の後半・任意（D-26）。`BoundReturning`・`PhysicalPlan::{Insert, Update, Delete}.returning` の欄は M4 が用意する | **D46** | RW、XQ |
| A9 | DDL の実行は `ddl/`（`DdlCtx`、`ddl::execute(&mut DdlCtx, BoundDdl) -> Result<String>`。M4 §14.6）。`BoundStatement::Ddl(BoundDdl)`。M4 の `BoundDdl` に `Truncate`・`Vacuum`・`AlterTableAddConstraint` がある | **D41。** M5 は `BoundDdl` に変種を足す（§4.7） | LK、VC、AU、DB、FK |
| A10 | `Transaction` に `wal_flush_upto`・`started_at` を足す。`TxnManager::finish_without_xid`（M4 §14.1） | M5 は `guard`・`isolation`・`xact_snapshot` を足し、`writer` を `guard` に置き換える（§4.3）。`finish_without_xid` は残す（D48） | LK |
| A11 | `RmgrId = { Xlog 0, Xact 1, Smgr 2, Heap 3, Btree 4, Seq 5 }`（M4 §13.4） | `Heap2 = 6`、`Dbase = 7`（§3.3）。空きがあるので移さない | F0 |
| A12 | `CATALOG_VERSION_NO` を M4 が上げる。M3 のデータディレクトリは使えない（M4 D-28）。`settings.rs` に `INERT_GUCS`（受け付けて保存するだけの一覧）と `maintenance_work_mem`（保存するだけ）が入る。`DateStyle` / `TimeZone` は `TypeEnv.datetime` の元（`IntervalStyle` は保存するだけ。M4 §15.4） | M5 は `catalog_version` と `format_version` を更に上げる（D35）。`INERT_GUCS` に各章が自分の設定を足す。`maintenance_work_mem` は VC が読む。`IntervalStyle` と ParameterStatus を TD が効かせる | F0、各章 |

#### トラックと依存

```
F0a ─ F0b-1 ─┬─ F1（M4 のファイルの修正）
   │         ├─ LK-1 ─┬─ LK-2
   │         │        ├─ LK-5 ─ AU-3
   │         │        └─ LK-3 ─┬─ LK-4 ─┬─ VC-3 ─ VC-4 ─ VC-6
   │         │                 │        │      └─ VC-5 ─ FK-4
   │         │                 │        └─ RW-4 ─ FK-3
   │         │                 └─ RW-1b/1c ─┬─ RW-3c ─┬─ FK-2 ─ FK-3 / FK-4 / FK-5
   │         │                              │         └─ RW-4、RW-6
   │         │                              └─ RW-5 ─ VC-3 の結合
   │         ├─ RW-1a/1d、RW-2（F0b-1 だけに依存。RW-1c が RW-2 を待つ）
   │         ├─ VC-1 ─ VC-2 ─ VC-3
   │         └─ DB-1（LK-1、BackendRegistry）─ DB-2（LK-3、AU-3）─ DB-3 ─ DB-4
   └─ F0b-2（F0b-1 の後）─┬─ XQ-1 ─ XQ-2 ─ XQ-5 ─ XQ-7   （XQ-3 は M4 の analyzer と F1 が前提。XQ-4 は独立）
             ├─ RW-3a/3b（M4 の analyzer・planner）
             ├─ TY-1 ─┬─ TY-2..7（TY-5 の ArrayValue を先に FK-1 へ）  TD-1 ─ TD-2..5
             ├─ FK-1（TY-5 の ArrayValue）
             └─ AU-2        （AU-1 は新規クレートなので F0a と並行して先行。D49）
TS-1a・TS-4a・TS-5a は PostgreSQL だけで進められる。TS-1b・TS-2・TS-3・TS-4b・TS-5b は各トラックの完成に追従する
```

- **クリティカルパス**: F0a（2.5）→ F0b-1（1.5）→ LK-1（3）→ LK-3（4）→ RW-1b/1c（3。RW-1a・1d・RW-2 は LK の裏で済む）→ RW-3c（2）→ FK-2（6）→ FK-4（3）→ 結合・安定化（TS。5）で**約 30 日（6 週間）**。FK-1（TY-5 の `ArrayValue` 待ち）が FK-2 の開始に同じ日に効く第 2 の経路。並列度は平均約 6 本、ピーク 10〜13 本（10 章 §8.3）。初版は「RW-1 を 1 つの 5 日の WP として LK-3 の後に置く」と書いたため 32 日（FK-3 終端）だったが、RW-1a・1d・RW-2 を LK の裏に出した（02 §8 の枝番と、RW-1c が RW-2 に依存し RW-2 は F0 だけに依存するという 02 の依存に合わせた）ことと、FK-4 が FK-3 より長いことを反映した（レビュー対応 R-20）。
- **M4 の実装中に始めてよいもの（D49。新規ファイル・新規クレートだけに触る作業）**: AU-1（`yuzhu-auth`）、TD-4 のうち `yuzhu-datetime` への `Time` の追加、LK-1・LK-2（`txn/lock/` は新規。ただし `DebugKnobs` の項目は F0b-1 が足すまで仮置き）、TS-1a・TS-4a・TS-5a（PostgreSQL だけで完結）、XQ の純粋な部分（`RecvBuf` の枠、`ParamCtx` の単体、transcript の下書き。04 §8）。**M4 のマージ後に始めるもの**: F0a（`session.rs`・`builtin.rs` を動かす）、それに依存する残りすべて。**XQ-1・XQ-4 は既存の `connection.rs`・`messages.rs`・`codec.rs`・`types/` に触れ、M4 の COPY と衝突するので、先行リストに入れない**。**M4 の planner・analyzer・B+Tree・`ddl/` が要るもの**: RW-3、RW-5、FK、VC-3、XQ-2、XQ-3、AU-3、DB-2、F1。
- **カットライン**（遅れたら次の順で M6 に回す。ほかの章の契約を変えずに落とせる）: VC-6（autovacuum）→ XQ-6（COPY TO）→ TD-4（time 型）→ VC-4 の clog の切り詰め（凍結の記録と `yz_relxid` は残す）→ VC-5 の末尾の切り詰め。**TY-5 の `ARRAY(SELECT)` と添字は、psql の `\d tbl` に要る（05 TY-D14）ので落とさない**（05 の依頼 13。TY-5 の任意項目 `||`・`unnest`・`string_agg` → `md5` / `sha*` → `convert_to` / `convert_from` の順に落とす）。TY-5c（SELECT 句の SRF の最小対応）を落とすと FK のある表の `\d tbl` が動かない（07 の完了条件 9 を外す）。


### 1.4 計画の穴（どのマイルストーンにも割り当てられていない項目）

M1〜M5 の計画と調査を突き合わせて見つけた。「実用化」で必ず困るものから並べる。

| 項目 | 現状 | 推奨 |
|---|---|---|
| `INSERT / UPDATE / DELETE ... RETURNING` | M1・M2 が `0A000`。M4 は「後半・任意」で欄だけ用意する（D-26）。ORM（Rails・SQLAlchemy・Prisma・Django）が主キーの取得に毎回使う。m5-concurrency の EPQ と m5-protocol-auth の `PORTAL_ONE_RETURNING` は RETURNING を前提にしている | **M5 で仕上げる（WP RW-6。D46）**。M4 が完了していれば縮む |
| `TRUNCATE` | m3 §3.8 は「TRUNCATE 文は M5」と書いていたが、M4 D-11 が実装する（新しい relfilenode。ロールバック可能） | **M4 に任せる。M5 は D42 の 4 点だけ足す（VC-5）** |
| `COPY ... TO STDOUT`、CSV 形式 | pg-compat が M5 の A 項目（P2、P5）。依頼の 8 項目には無い | **XQ-6（任意。カットライン上位）** |
| SAVEPOINT | D2 | M6 の先頭。M5 は拡張点を残すだけ |
| `INSERT ... ON CONFLICT`（upsert） | どこにも無い。ORM が多用する | **M6**。M5 の行ロック（FOR UPDATE）と一意検査の待ち（RW-5）が前提になる。RW の章に「M6 で使う拡張点」を書く |
| `ALTER TABLE`（ADD / DROP COLUMN、RENAME など） | どこにも無い（M4 は ADD PRIMARY KEY / UNIQUE だけ） | M6。M5 の FK は ADD / DROP CONSTRAINT だけ |
| `CREATE SCHEMA` / `CREATE VIEW` / `CREATE TYPE` | どこにも無い | M6 |
| 未知の GUC の SET を受け付ける一覧 | pg-compat §2.6 が M2 の B 項目としている | M5 で新設定を足すときに一緒に一覧へ入れる（`settings.rs`。各章が自分の設定を追加） |
| `pg_terminate_backend` / `pg_cancel_backend` | どこにも無い | M6（DROP DATABASE FORCE と一緒） |

---

## 2. クレートとモジュールの構成

M3 の構成（`m3.md` 第 2 節）に **M4 の構成（`spec/design/m4/00-contracts.md` §4。`expr/`、`deparse/`、`ddl/`、`copy/`、`explain/`、`storage/btree/`、`storage/sequence.rs`、`planner/{logical,physical,build,rules,physicalize}`、`executor/nodes/*` など）が入った状態**を出発点にして、M5 での変更を示す。`★` は M5 で新規、`△` は M5 での変更、`✕` は削除。括弧内は持ち主の章。M4 のファイルは M4 の名前を正とする。

```
impl/rust/crates/
├── yuzhu-auth/                    ★ 新クレート（AU）。外部依存: sha2, hmac, pbkdf2, base64, getrandom, subtle, stringprep
│   └── src/ lib.rs, scram.rs（SCRAM-SHA-256 のサーバ側）, saslprep.rs, random.rs（OS の乱数）
├── yuzhu-numeric/                 既存。M5 での変更なし（M4 が統合済み）
├── yuzhu-datetime/                既存。`Time`（time without time zone）を足す（TD-4）。`interval` は既存
├── yuzhu-core/src/
│   ├── backend.rs                 ★ BackendId、BackendRegistry、BackendGuard（LK）
│   ├── error.rs                   △ SQLSTATE の追加（§3.5。F0）
│   ├── settings.rs                △ 設定項目の追加（§3.6。各章が自分の分を足す。`INERT_GUCS` は M4 が作る）
│   ├── expr/mod.rs                △ `ExprKind::ExternParam(u16)` を 1 変種足す（D45。XQ-3）
│   ├── types/
│   │   ├── datum.rs               △ Datum の変種を F0 がまとめて足す（§4.9）。`Int4Array` は `Array` に置き換え、M4 の `int2[]`（`Int2Vector`）も `Array` へ
│   │   ├── io.rs                  △ 入出力の振り分け（TY が持つ）。バイナリの振り分けは binary.rs
│   │   ├── binary.rs              ★ input_binary / output_binary の振り分け表（XQ-4）
│   │   ├── numeric.rs bpchar.rs datetime.rs   M4 が作成。M5 は send / recv（バイナリ形式）と M4 の穴埋めだけ足す（TY、TD）
│   │   ├── bytea.rs ★ uuid.rs ★ array.rs ★   （TY。型ごとの入出力・比較・send / recv）
│   │   └── interval.rs ★ time.rs ★            （TD。`datetime.rs` が大きくなるので分ける）
│   ├── sql/
│   │   ├── ast.rs                 △ Statement の変種を F0 がまとめて足す（§4.7）
│   │   ├── lexer.rs               △ `$n`（XQ-3）
│   │   └── parser/ lock.rs ★ vacuum.rs ★ role.rs ★ database.rs ★ prepare.rs ★ fk.rs ★    （各章。select.rs の FOR 句は RW）
│   ├── catalog/
│   │   ├── builtin/               △ builtin.rs を分割する（F0。M4 のマージ後）: mod.rs（表の結合）、core.rs（M1〜M3 の内容と M4 の集約・opclass 以外）、
│   │   │                            numeric.rs、datetime.rs（M4 が足した日時の行と M5 の interval / time）、text_misc.rs（bpchar・bytea・uuid）、array.rs、runtime.rs（FnKind::Runtime）
│   │   ├── virtual_rel.rs         ★ 仮想リレーション（LK。pg_locks。AU が pg_roles を足す）
│   │   ├── schema.rs              △ yz_relxid（VC）
│   │   ├── store.rs               M4 のまま。各章は `store_fk.rs`（pg_constraint の contype `f`）・`store_db.rs`（pg_database）・`roles.rs`（pg_authid）に `impl CatalogStore` を書く（§8）
│   │   └── roles.rs               ★ pg_authid の読み書き（AU）
│   ├── analyzer/                  △ params.rs ★（XQ-3）、ddl_ext/{lock_table,vacuum,role,database,foreign_key}.rs ★（各章の `BoundDdl` の組み立て。M4 の `ddl.rs` は振り分けの 1 行を F0 が足すだけ。§8）、dml.rs・select.rs・bound.rs（RW。FOR 句、RETURNING。`BoundSelect.locking`）
│   ├── planner/                   △ logical.rs・physical.rs・physicalize.rs・build.rs に `LockRows`、`recheck`（RW）、`VirtualScan`（LK）
│   ├── executor/
│   │   ├── dml.rs                 △ `insert_with_indexes` の `WaitFor` ループ、`update_with_indexes` の待ち・crosscheck（RW。M4 の X3 の後継）
│   │   ├── nodes/ lock_rows.rs ★ update.rs △ delete.rs △ insert.rs △ index_scan.rs △（`FOR` 句の下の実体取得）  （RW）
│   │   ├── ri/ mod.rs ★ queue.rs ★ check.rs ★ action.rs ★                            （FK）
│   │   └── virtual_scan.rs        ★ 仮想リレーションの走査（LK）
│   ├── storage/
│   │   ├── mod.rs                 △ `TableStore` の `delete` / `update` / `lock_tuple`、`IndexStore` の `InsertOutcome` / `bulk_delete`（RW、VC）
│   │   ├── fsm.rs                 ★ 空き領域マップ（VC-2）
│   │   ├── page.rs                △ LP_DEAD / LP_UNUSED の操作（VC）
│   │   ├── buffer/                △ read_buffer_zero_on_error（VC-2）
│   │   ├── heap/ lock.rs ★ xmax.rs ★ prune.rs ★ vacuum.rs ★ wal2.rs ★ ｜ mod.rs △ visibility.rs △ hio.rs △ wal.rs △
│   │   │        （lock.rs・xmax.rs・visibility.rs・mod.rs の更新競合は RW、HEAP_LOCK の wal.rs は RW、残りは VC。hio.rs は VC-2）
│   │   ├── btree/vacuum.rs        ★ 項目の削除（VC-3。BTREE_VACUUM の WAL）。M4 の `insert.rs`・`unique.rs` などの複数ライター対応の修正は RW-5
│   │   └── sequence.rs            M4 のまま（複数ライターでも変更なし）
│   ├── txn/
│   │   ├── lock/ mod.rs ★ table.rs ★ wait.rs ★ deadlock.rs ★                         （LK）
│   │   ├── multixact.rs           ★（RW-2）
│   │   ├── proc_array.rs          ★ ProcArray: 実行中の XID、XID の採番、スナップショットの登録簿（LK-3。manager.rs から切り出す）
│   │   ├── snapshot.rs            ★ RegisteredSnapshot（LK-3）
│   │   ├── manager.rs             △ 複数ライター化（LK-3）
│   │   └── clog.rs                △ ロックなしの読み取り（LK-3）、truncate_before（VC-4）
│   ├── vacuum/ mod.rs ★ driver.rs ★ autovacuum.rs ★                                  （VC）
│   ├── ddl/                       M4 が作る（D41）。M5 は次を足す
│   │   └── lock_table.rs ★（LK）, vacuum.rs ★・truncate.rs △（VC）, role.rs ★（AU）, database.rs ★（DB）, constraint.rs △（FK の ADD / DROP CONSTRAINT）, mod.rs △（`execute_standalone`、`DdlCtx` の項目）
│   ├── dbase.rs                   ★ CREATE / DROP DATABASE の手順、DBASE rmgr のレコードと REDO（DB）
│   ├── session/                   △ session.rs を分割する（F0）
│   │   └── mod.rs（Session、ResultSink、TxState）, simple.rs（execute_simple）, txn_ctl.rs（BEGIN など、分離レベル。RW。`TxnControl` の実装）,
│   │       locking.rs（文ごとのロック取得とスナップショット。LK）, extended.rs・prepared.rs（XQ。PREPARE / EXECUTE / DEALLOCATE / DISCARD も prepared.rs が処理する）
│   ├── engine.rs                  △ Cluster に locks / backends / multixact を持たせる（LK）、connect の分割（AU）、データベースの登録（DB）
│   ├── control.rs                 △ format_version 3、next_multi（F0）
│   ├── recovery.rs                △ REDO の振り分けに HEAP の LOCK、HEAP2、BTREE の VACUUM、DBASE を追加（F0 が口、VC・DB・RW が中身）
│   └── bootstrap.rs               △ 新しい型の行、yz_relxid、仮想リレーションの行（各章。F0 が口）
└── yuzhu-server/
    ├── src/ connection.rs △（XQ-1: メッセージループ、AU-2: 認証）, protocol/ △, auth.rs ★（AU）, hba.rs ★（AU）,
    │        shutdown.rs △（登録簿）, config.rs △, bin/yuzhu-initdb.rs △（--auth-host、--pwfile）
    └── tests/ extended.rs ★ auth.rs ★ database.rs ★ concurrency.rs ★                  （各章。TS が共通部品を持つ）
```

**依存の方向**（M4 契約 §4.1 の図に追加。下位は上位を `use` しない）:

```
error, types（numeric・datetime クレートを含む）, util, interrupt, debug_knobs, backend
  ← sql ← catalog::{mod, builtin, opclass, schema, virtual_rel}
  ← expr ← deparse
  ← storage::vfs ← control, datadir
  ← txn::{mod, clog, lock, multixact, proc_array, snapshot}   （storage の型に依存しない。LockTag は素の u32 / Oid で持つ）
  ← storage::{smgr, page, checksum} ← storage::buffer ← storage::fsm
  ← wal ← storage::heap（lock・xmax・prune・vacuum・wal2 を含む）← storage::btree ← storage::{index_store, sequence} ← storage::{heap_store, stack}
  ← txn::{manager, xact_wal}                          （Wal・LockManager で commit / abort を記録する）
  ← catalog::{rows, store, cache, reader, roles}
  ← analyzer ← planner ← executor（nodes・ri・virtual_scan）
  ← vacuum, dbase, ddl, copy, explain
  ← checkpoint, recovery, bootstrap, engine, session
```

- `txn::lock` と `txn::multixact` は `storage` を使わない。heap が `LockManager` と `MultiXactTable` を使う（`HeapStore::new` が `Arc` で受け取る）。`storage::btree` は `LockManager` を使わない（D44）。
- `executor::ri`（FK）は `TableStore`・`IndexStore`・`LockManager` を使う。ヒープの内部（ページ）には触れない。
- `ddl::*` は `catalog::store`・`storage`・`txn`・`dbase`・`vacuum` を使う。`session` が呼ぶ。`ddl::*` は `session` を `use` しない（必要な文脈は引数の `DdlCtx` / `TxnControl` で渡す。§4.7）。
- `yuzhu-core` は crates.io の外部クレートに直接依存しない（D29）。`yuzhu-server` は `yuzhu-auth` を使う。

**コーディング規約（M5 で追加。M2 の 10 項目、M3 の 5 項目に続く）**:

1. **待つ前の禁止事項**: `LockManager` の待ち（`acquire`・`wait_for_xact`）に入る前に、ページのラッチ・バッファのピン（待ちの対象のページ以外を含む）・コミットゲート・`checkpoint_lock`・`LockManager` 以外の Mutex を持っていてはいけない。デバッグビルドでは `buffer/track.rs` のスレッドローカルの表を見て panic する（m5-concurrency §3.2）。
2. **待つ関数は必ず `WaitCtl` を受け取る**。引数なしで待つ関数を作らない（キャンセル・`lock_timeout`・`statement_timeout`・`deadlock_timeout` が効かなくなる）。
3. **xmax と infomask の組み立ては `storage/heap/xmax.rs` の関数だけが行う**（RW が持つ。VC・FK は読み取りの関数を呼ぶ）。ビットの組み合わせを散らさない。
4. **FSM と統計は導出データ**: WAL を書かない。読み込みはチェックサム不正を 0 のページとして扱う。FSM を理由にした失敗は起こさない（見つからなければ拡張する）。
5. 新しい WAL レコードは M3 の規約 1 の形（検査→ラッチ→`CriticalSection`→`page_mut`→`RecordBuilder`→`insert`→`set_lsn`）。レコードの REDO は `recovery::dispatch` に振り分け表を足す（F0 が口を作る）。
6. 乱数は `yuzhu_auth::random`、現在時刻は `Clock` トレイト（TD）を通す。`SystemTime::now()` を直接呼ぶのはその 2 か所の実装だけ。
7. 文言と SQLSTATE は §3.5・§3.8 の表に合わせる。表に無いものを増やしたら、その章の「契約への変更依頼」に書く。
8. `#![forbid(unsafe_code)]`、`clippy::pedantic`、`missing_debug_implementations` は M2 のとおり。

---

## 3. 用語・ID・ディスク形式の方針

### 3.1 用語

| 用語 | 意味 | 注意 |
|---|---|---|
| リレーションロック | `LockTag::Relation` の重量ロック（8 モード）。「テーブルロック」とも言う | |
| XID ロック | `LockTag::TransactionId(xid)`。トランザクションが自分の XID に Exclusive を持ち、終了待ちの相手が Share を要求する | PostgreSQL の `XactLockTableWait` |
| 行ロック | タプルの `xmax` と infomask に書く 4 強度のロック（`FOR UPDATE` など） | ロックマネージャには載らない |
| タプルロック | `LockTag::Tuple` の重量ロック。同じ行を待つ者の順番取り（飢餓の防止）にだけ使い、行に印を付けたらすぐ解放する | 「行ロック」と取り違えない |
| 更新者 | `xmax` が UPDATE / DELETE の XID であるもの（`LOCK_ONLY` でない） | |
| ロック保持者 | `xmax`（または MultiXact のメンバー）が `FOR ...` のロックを持つ XID | |
| MultiXact | 複数のロック保持者を 1 つの ID にまとめたもの（メモリ上だけ。D6、D36）。`MultiXactId(u64)` | |
| horizon | VACUUM が「全員にとって死んでいる」と判断する境界 XID。`TxnManager::oldest_xmin()` | |
| 登録済みスナップショット | `TxnManager` に登録され、Drop まで horizon を止めるスナップショット（`RegisteredSnapshot`） | |
| カタログ用スナップショット | カタログの行を読むためだけに、文ごとに取り直す最新のスナップショット（D12） | **登録する（短命。`take_snapshot`。使い終えたら Drop）**。登録しない `snapshot()` は診断とテストだけ（R-02） |
| 仮想リレーション | 行を提供関数が作るリレーション（`pg_locks` など。D33） | |
| 名前予約ロック | CREATE の重複を防ぐ `LockTag::Object`（クラス = `pg_class` の OID、`obj` = 名前空間と名前のハッシュ） | |

### 3.2 OID の方針

- 型・関数・演算子・キャストの OID は **PostgreSQL 17 の `pg_type.dat` / `pg_proc.dat` / `pg_operator.dat` / `pg_cast.dat` と同じ値**にする（M2 §6.8.5）。値を記憶で書かない。各章の担当は実装前に .dat で確かめ、章の中の表には「.dat で確認済み」か「（未検証）」を付ける。
- yuzhu 独自のカタログと制約の OID は **9800〜9899** を使う（PostgreSQL が開発用に空けている 9000〜9999 の範囲）。M5 で使うのは `yz_relxid` = **9801**（データベースごと）、`yz_datxid` = **9802**（共有。D51）、`pg_roles` = **9810**（08 C6）、`pg_locks` = **9811**（01 §11-12。PG17.11 の実測 12073 は使わない）。`pg_auth_members` は PostgreSQL の **1261**（yuzhu 独自の範囲ではない）。ほかに必要になったら、その章の「契約への変更依頼」で申請する。
- データベース・ロールの OID はカウンタから採る（`CatalogStore::get_new_oid`。16384 以上）。`bootstrap` の固定値（template1 = 1、template0 = 4、postgres = 5、ブートストラップユーザー = 10）は変えない。
- `LockTag::Object` の `class` には、対象のカタログの OID（`pg_class` = 1259、`pg_database` = 1262、`pg_authid` = 1260）を使う。

### 3.3 WAL の割り当て（M3 §3.5 に足す。M4 の確定表が正）

| rmgr | ID | M5 で足すもの | 持ち主 |
|---|---|---|---|
| HEAP | 3 | `0x40 LOCK`（行ロックの記録）。M3 が予約した枠 | RW |
| BTREE | 4 | `0x20 VACUUM`（項目の削除。M4 §13.4 が `0x20` 以降を M5 の予約にした。D47） | VC |
| HEAP2 | **6** | `0x00 PRUNE_FREEZE`（pruning・凍結・xmax の無効化を 1 ページ 1 レコード）、`0x10 VACUUM_UNUSED`（`LP_DEAD` → `LP_UNUSED`）、`0x20 MULTI_INSERT`（予約。COPY の最適化）、**`0x30 INPLACE`（その場の上書き。統計と `yz_relxid` / `yz_datxid` の更新。03 VC-D3・C1。R-37）**、`0x40` 以降は予約 | VC |
| DBASE | **7** | `0x00 CREATE_FILE_COPY`（src と dst のデータベース OID）、`0x10 DROP` | DB |
| SMGR | 2 | 変更なし（TRUNCATE 文と VACUUM の末尾の切り詰めは既存の `TRUNCATE` / `CREATE` を使う） | VC |

- M4 の確定表は `RmgrId` を 0〜5（Btree = 4、Seq = 5）まで使う（M4 §13.4）ので、6・7 は空いている。**章の中では数値を書かず `RmgrId::Heap2` / `RmgrId::Dbase` と書く。**
- clog の切り詰めの WAL レコードは作らない。制御ファイルの `oldest_xid` を fsync してからファイルを消し、起動時にも `oldest_xid` 未満のファイルを消す（冪等）。`oldest_xid` 未満の XID に対する REDO の `set_status_redo` は何もしない（VC-4）。
- 行ロックの REDO は値を書くだけ。MultiXact の表は WAL に載せない（再起動後は全 ID が「メンバー全員が終了」）。

### 3.4 ロックのタグとモード

```text
LockTag::Relation { db, rel }                 リレーション（インデックスも）
LockTag::Tuple { db, rel, block, offset }     行の順番取り
LockTag::TransactionId(xid)                   XID の終了待ち
LockTag::Database(oid)                        データベースを使っているセッションと CREATE / DROP DATABASE
LockTag::Object { db, class, obj }            ロール、名前予約など
```

モードは PostgreSQL の 8 つ（`AccessShare` = 1 … `AccessExclusive` = 8）。衝突表は m5-concurrency §3.1 のとおり。`Tuple` は `Exclusive`、XID ロックは保持者が `Exclusive`・待ち手が `Share`、`Database` は接続中のセッションが `AccessShare`（セッションスコープ）・CREATE / DROP が `AccessExclusive`。**同じバックエンドが持つロック同士は衝突しない。**

文ごとのモード（LK が実装し、各章は従う）:

| 文 | 対象 | モード |
|---|---|---|
| SELECT | 参照するリレーション | AccessShare |
| SELECT ... FOR（4 強度とも） | FOR の対象のリレーション | RowShare |
| INSERT / UPDATE / DELETE | 対象のリレーション | RowExclusive（参照だけのテーブルは AccessShare） |
| VACUUM、ANALYZE | 対象 | ShareUpdateExclusive |
| CREATE INDEX（M4） | 対象のテーブル | Share |
| ALTER TABLE ... ADD FOREIGN KEY | 参照元と参照先 | ShareRowExclusive |
| DROP TABLE、TRUNCATE、ALTER TABLE ... DROP CONSTRAINT | 対象（子のテーブルも） | AccessExclusive |
| LOCK TABLE | 指定のモード（省略は AccessExclusive） | 指定どおり |
| FK の検査（内部） | 参照先 | RowShare。子への CASCADE / SET NULL / SET DEFAULT は RowExclusive |
| CREATE TABLE | 名前予約ロック | Exclusive |

### 3.5 SQLSTATE の追加（`error.rs` の `sqlstate`。F0 が一括で足す）

`SERIALIZATION_FAILURE` 40001、`DEADLOCK_DETECTED` 40P01、`INVALID_PASSWORD` 28P01、`DUPLICATE_DATABASE` 42P04、`OBJECT_IN_USE` 55006、`DUPLICATE_OBJECT` 42710（既存）、`DEPENDENT_OBJECTS_STILL_EXIST` 2BP01（M4 が追加済み）、`FOREIGN_KEY_VIOLATION` 23503、`INVALID_FOREIGN_KEY` 42830、`INVALID_BINARY_REPRESENTATION` 22P03、`INVALID_SQL_STATEMENT_NAME` 26000、`INVALID_CURSOR_NAME` 34000、`DUPLICATE_PREPARED_STATEMENT` 42P05、`DUPLICATE_CURSOR` 42P03、`UNDEFINED_PARAMETER` 42P02、`AMBIGUOUS_PARAMETER` 42P08、`INDETERMINATE_DATATYPE` 42P18、`PROTOCOL_VIOLATION` 08P01、`INVALID_DATETIME_FORMAT` 22007、`DATETIME_FIELD_OVERFLOW` 22008、`INVALID_TIME_ZONE_DISPLACEMENT_VALUE` 22009、`STATEMENT_TOO_COMPLEX` 54001、`TOO_MANY_CONNECTIONS` 53300、`RESERVED_NAME` 42939、`INVALID_PARAMETER_VALUE` 22023（M1 に無ければ）、`NUMERIC_VALUE_OUT_OF_RANGE` 22003（既存）、`ARRAY_SUBSCRIPT_ERROR` 2202E、`SUBSTRING_ERROR` 22011、`CHARACTER_NOT_IN_REPERTOIRE` 22021、`NAME_TOO_LONG` 42622、`INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION` 2201G、`CANNOT_COERCE` 42846、`WRONG_OBJECT_TYPE` 42809、`INVALID_OBJECT_DEFINITION` 42P17（05 §11-7、09 §11-12。無ければ F0b-1 が足す）。M4 が追加するもの（`2200H`、`2201B`、`22P04`、`2BP01`、`42712`、`428C9`。M4 §15.3）と重複して足さない。ほかに必要になった章は自分で足してよい（M2 §4.1 の例外）。

### 3.6 設定項目（`settings.rs`。各章が自分の分を足す）

| 名前 | 型・既定 | 章 | 備考 |
|---|---|---|---|
| `deadlock_timeout` | 時間・1s | LK | 誰でも SET できる（PostgreSQL は superuser のみ。M6 の GRANT で揃える） |
| `max_locks_per_transaction` | 整数・64 | LK | **読み取り専用（`SHOW` は 64、`SET` は `55P02`。PG17 と同じ）**。`53200` は起こさない（01 M5-LK-Q13。R-37） |
| `transaction_isolation` / `default_transaction_isolation` | 列挙・`read committed` | RW | `repeatable read` を許す。`serializable` は 0A000 |
| `maintenance_work_mem` | サイズ・64MB（M4 が「保存するだけ」で作る） | VC | M5 から VACUUM の TID 集合の上限として読む |
| `vacuum_freeze_min_age` ほか `vacuum_*` | 保存だけ | VC | 凍結は horizon までを常に凍結する（D14 の周辺） |
| `autovacuum`、`autovacuum_naptime`（60s）、`autovacuum_vacuum_threshold`（50）、`autovacuum_vacuum_scale_factor`（0.2）、`autovacuum_freeze_max_age`（200000000。03 C10） | `autovacuum` は **off** | VC | postmaster 設定（ファイルと引数。SET は不可）。PG にあって実装しない `autovacuum_*` は `INERT_GUCS` |
| `plan_cache_mode` | 保存だけ | XQ | |
| `password_encryption` | `scram-sha-256` | AU | `md5` は 0A000 |
| `scram_iterations` | 整数・4096 | AU | ParameterStatus で報告する（PG16 以降。**【実機】PG17.11 の起動時の ParameterStatus（14 個）に `scram_iterations=4096` が含まれることを確認済み**。レビュー対応 R-14） |
| `authentication_timeout` | 時間・60s | AU | postmaster 設定 |
| `TimeZone`、`DateStyle`、`IntervalStyle`、`timezone_abbreviations`（`Default` 固定）、`bytea_output`（`hex` / `escape`。値は大文字小文字を区別しない列挙。持ち主は TY。05 §11-10） | M4 が `TimeZone` と `DateStyle` を `TypeEnv.datetime` の元として持つ（`IntervalStyle` は保存だけ）。M5 は全形式を効かせる（D24） | TD、TY | SET したら ParameterStatus を送る（`TimeZone`・`DateStyle`・`IntervalStyle` は報告対象） |
| `extra_float_digits` | M1 のとおり | — | |

PostgreSQL に存在するが yuzhu では意味を持たない設定は「受け付けて保存だけ」にする（pg-compat §2.6）。一覧は `settings.rs` が持ち、各章は自分の設定をそこへ足す。

### 3.7 ディスク形式の変更点（まとめ。詳細は持ち主の章）

| 対象 | 変更 | 章 |
|---|---|---|
| 制御ファイル | `format_version` = 3。オフセット 120 に `next_multi`（u64。MultiXact の先取りの上限）。`oldest_xid`（オフセット 80）を M5 で使い始める。`catalog_version` を上げる | F0、VC |
| タプル（infomask） | 使うビット: `XMAX_KEYSHR_LOCK` 0x0010、`XMAX_EXCL_LOCK` 0x0040、`XMAX_LOCK_ONLY` 0x0080（`XMAX_SHR_LOCK` = 0x0050）、`XMAX_IS_MULTI` 0x1000、`XMAX_INVALID` 0x0800、`XMIN_FROZEN` = 0x0300（**両ビット**）、infomask2 の `KEYS_UPDATED` 0x2000。単独の `XMIN_COMMITTED` / `XMIN_INVALID` / `XMAX_COMMITTED` は M5 でも書かず、読んでも無視する（D13）。`HOT_UPDATED` / `ONLY_TUPLE` / `LP_REDIRECT` は使わない | RW、VC |
| 更新者の xmax | `LOCK_ONLY` を立てず、`EXCL_LOCK` / `KEYSHR_LOCK` も立てない。キー列を変えた UPDATE と DELETE は `KEYS_UPDATED` を立てる（M2 §6.5.3 のとおり）。**PostgreSQL も更新者に `EXCL_LOCK` を立てない（【実機】`pageinspect`。02 RW-D1。確認済み）**。新しい版の xmax は常に空（`XMAX_INVALID`。02 RW-D3） | RW |
| ロック保持者の xmax | 単独: その XID + `LOCK_ONLY` + 強度のビット（KeyShare = `KEYSHR_LOCK`、Share = `SHR_LOCK`、NoKeyExclusive = `EXCL_LOCK`、Exclusive = `EXCL_LOCK` + `KEYS_UPDATED`）。複数: `IS_MULTI` + `LOCK_ONLY` + `MultiXactId`（D6）。`LOCK_ONLY` の xmax は可視性判定で無視する | RW |
| 行ポインタ | `LP_DEAD`（領域なし）、`LP_UNUSED`（VACUUM の第 3 段で作る。再利用してよい）を使い始める。`pd_prune_xid` に「ページ内で最も古い削除 / 更新の XID」の下位 32 ビット（M2 §3.3）を書く | VC |
| FSM | `Fsm` フォーク（`<relfilenode>_fsm`）。1 ブロックのヒープにつき 1 バイト（空きを 32 バイト単位にした値）。WAL なし。形式は VC | VC |
| カタログ | `yz_relxid`（OID 9801、データベースごと）と `yz_datxid`（OID 9802、共有。D51）。`pg_class.relfrozenxid` と `pg_database.datfrozenxid` には下位 32 ビットを書く | VC、DB |
| 配列 | PostgreSQL の `ArrayType` と同じ varlena（`ndim`、`dataoffset`、`elemtype`、`dims`、`lbound`、NULL ビットマップ、要素）。M5 は 1 次元だけ作る。M4 の `int2[]`（yuzhu 独自の符号化）は置き換える（D25） | TY |
| numeric、日付時刻 | numeric・date・timestamp・timestamptz・bpchar は M4 §12.3 の符号化のまま（numeric は `ndigits u16, weight i16, sign u16, dscale u16, digits [u16]`）。M5 が足すのは time（i64 LE。0 時からのマイクロ秒）と interval（`time i64, day i32, month i32` の 16 バイト）、bytea（varlena + バイト列）、uuid（16 バイト） | TY、TD |
| pg_xact | 形式は変えない。`oldest_xid` 未満のセグメントを消す | VC |
| データベースのディレクトリ | `base/<oid>/` を CREATE DATABASE が作る。形式は変えない | DB |

### 3.8 エラーの文言（固定。PostgreSQL 17 と一字一句そろえる）

| 状況 | SQLSTATE | メッセージ |
|---|---|---|
| ロック待ちの時間切れ | 55P03 | `canceling statement due to lock timeout` |
| NOWAIT（リレーション） | 55P03 | `could not obtain lock on relation "t"` |
| NOWAIT（行） | 55P03 | `could not obtain lock on row in relation "t"` |
| デッドロック | 40P01 | `deadlock detected`。DETAIL は `Process %d waits for %s on %s; blocked by process %d.` を改行区切り、HINT は `See server log for query details.` |
| RR の更新競合 | 40001 | `could not serialize access due to concurrent update`（削除に当たったときは `... concurrent delete`。`FOR UPDATE` などはどちらも `... concurrent update`） |
| カタログ行の更新競合 | XX000 | `tuple concurrently updated` |
| 同じコマンドで二度更新 | 27000 | `tuple to be updated was already modified by an operation triggered by the current command`（M2 のとおり） |
| ブロック内で実行できない文 | 25001 | `VACUUM cannot run inside a transaction block`、`CREATE DATABASE cannot run inside a transaction block`、`DROP DATABASE cannot run inside a transaction block`（`TRUNCATE` と、VACUUM を伴わない `ANALYZE` はブロック内で可。03 VC-D15。同じバッチで先に Execute が完了していた Extended Query では `X cannot be executed within a pipeline`。04 §5.8） |
| カタログ行・自己更新（XX000） | XX000 | `tuple concurrently updated`、`tuple concurrently deleted`、`tuple already updated by self`（02 RW-D16） |
| LOCK TABLE をブロックの外で | 25P01 | `LOCK TABLE can only be used in transaction blocks` |
| 分離レベルの変更が遅い | 25001 | `SET TRANSACTION ISOLATION LEVEL must be called before any query`（`transaction read-write mode must be set before any query` も同じ形。【実機】02 §11-5）。「最初のスナップショットを取った」は `SELECT 1` を含むほぼすべての文で立つ（02 RW-D11） |
| SERIALIZABLE | 0A000 | `SERIALIZABLE isolation level is not supported yet`（yuzhu 独自。M3 の文言を置き換える。`BEGIN` / `SET TRANSACTION` / `SET transaction_isolation` / `default_transaction_isolation` のどれでも。02 RW-D17） |
| 外部キー違反（子） | 23503 | `insert or update on table "c" violates foreign key constraint "c_pid_fkey"` / DETAIL `Key (pid)=(3) is not present in table "p".` |
| 外部キー違反（親） | 23503 | `update or delete on table "p" violates foreign key constraint "c_pid_fkey" on table "c"` / DETAIL `Key (id)=(1) is still referenced from table "c".` |
| 認証失敗 | 28P01 | `password authentication failed for user "alice"`（FATAL） |
| ロールがログインできない | 28000 | `role "alice" is not permitted to log in` |
| データベースの重複 | 42P04 | `database "x" already exists` |
| 使用中 | 55006 | `source database "template1" is being accessed by other users`、`database "x" is being accessed by other users`、`cannot drop the currently open database` |
| 準備済み文 | 26000 / 42P05 / 34000 / 42P03 | `prepared statement "s" does not exist`（無名は `unnamed prepared statement does not exist`）/ `prepared statement "s" already exists` / `portal "p" does not exist` / **`cursor "p" already exists`（重複ポータル。【実機】文言は portal ではなく cursor。04 C4。R-29）** |
| 型推論 | 42P18 / 42P08 | `could not determine data type of parameter $1` / `inconsistent types deduced for parameter $1` |
| 再解析で結果の型が変わった | 0A000 | `cached plan must not change result type` |

そのほか（FK の 42830・2BP01、日付時刻の 22007 / 22008 / 22009 など）は持ち主の章が m5-types-fk.md の表から取って固定する。

---

## 4. 共通の型（契約）

ここに書いたシグネチャは**変えない**。足りないものは追加してよい。変更が必要になったら**各章の §11「契約への変更依頼」に理由とともに書く**（M1〜M3 の `m5-changes.md` への記録は使わない。レビュー対応 R-37 で運用を 1 つにした）。取り込んだ依頼は §6.3 の台帳に記録し、**取り込み済みの項目は 00 の本文を直してある**。台帳に「未取り込み」と書いたものだけ、章の記述が 00 に優先する。各章は自分の担当の型をここより詳しく書いてよいが、ここと食い違ってはいけない。M3 の契約からの変更は §6 にまとめた。

### 4.1 backend.rs（LK）

```rust
/// セッションごとの ID。Cluster::next_session_id() の値。ロックの持ち主を表す
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct BackendId(pub u64);

#[derive(Clone, Debug)]
pub struct BackendInfo {
    pub id: BackendId,
    pub pid: i32,                          // BackendKeyData の pid（Session::backend_pid）
    pub db_oid: Oid,
    pub user_oid: Oid,
    pub client_addr: Option<std::net::IpAddr>,
    pub interrupts: Arc<InterruptFlag>,
}

#[derive(Debug)]
pub struct BackendRegistry { /* Mutex<HashMap<BackendId, BackendInfo>> */ }
impl BackendRegistry {
    pub fn new() -> Arc<Self>;
    pub fn register(self: &Arc<Self>, info: BackendInfo) -> BackendGuard;      // Drop で外れる
    pub fn count_in_db(&self, db: Oid) -> usize;
    pub fn count_of_role(&self, role: Oid) -> usize;
    pub fn by_pid(&self, pid: i32) -> Option<BackendInfo>;
    pub fn by_id(&self, id: BackendId) -> Option<BackendInfo>;
    pub fn all(&self) -> Vec<BackendInfo>;
    pub fn register_if(self: &Arc<Self>, info: BackendInfo, admit: &dyn Fn(&[BackendInfo]) -> Result<()>) -> Result<BackendGuard>;   // 接続数の検査つき（01 §4.5・§11-9）
}
// BackendInfo.client_addr の出どころは StartupParams.client_addr（08 C3）。BackendGuard::{id, info} を足す。BackendGuard の Drop は登録簿から外すだけで、
// LockManager への登録・Session スコープのロックの解放は 09 の SessionLocks が持つ（01 LK-D21。R-37）
#[derive(Debug)] pub struct BackendGuard { /* Arc<BackendRegistry>, BackendId */ }
```

- yuzhu-server の `shutdown.rs` の登録簿（ソケットとキャンセル用の秘密鍵を持つ）は残す。サーバは両方に登録する。

### 4.2 txn::lock（LK）

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LockTag {
    Relation { db: Oid, rel: Oid },
    Tuple { db: Oid, rel: Oid, block: u32, offset: u16 },
    TransactionId(Xid),
    Database(Oid),
    Object { db: Oid, class: Oid, obj: u64 },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum LockMode { AccessShare = 1, RowShare, RowExclusive, ShareUpdateExclusive,
                    Share, ShareRowExclusive, Exclusive, AccessExclusive }
impl LockMode {
    pub fn conflicts(self, other: LockMode) -> bool;     // §3.4 の衝突表
    pub fn pg_name(self) -> &'static str;                // "AccessShareLock" など。DETAIL にも pg_locks.mode にも**そのまま使う**（PG17 は "Lock" を付ける。【実機】01 §11-1。R-37）
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LockScope { Transaction, Session }

/// 待ちの中断条件。待つ関数はすべてこれを受け取る（M5 規約 2）
#[derive(Clone, Copy, Debug)]
pub struct WaitCtl<'a> {
    pub lock_timeout: Option<Duration>,       // 1 回の待ちに対する時間
    pub deadlock_timeout: Duration,
    pub interrupts: &'a InterruptFlag,        // キャンセル・statement_timeout・停止
}
/// heap / index が待つときに渡す（誰が待つか + 中断条件）
#[derive(Clone, Copy, Debug)]
pub struct WaitCtx<'a> { pub backend: BackendId, pub ctl: &'a WaitCtl<'a> }

#[derive(Debug)]
pub struct LockManager { /* Mutex<LockTable> + バックエンドごとの Condvar（まず 1 本。計測して分割） */ }
impl LockManager {
    pub fn new() -> Arc<Self>;
    pub fn register_backend(&self, id: BackendId, pid: i32);
    pub fn unregister_backend(&self, id: BackendId);       // 保持が残っていればデバッグビルドで panic
    /// 取れるまで待つ。キャンセル → 57014、lock_timeout → 55P03、デッドロック → 40P01、停止 → 57P01
    pub fn acquire(&self, id: BackendId, tag: LockTag, mode: LockMode, scope: LockScope, wait: &WaitCtl<'_>) -> Result<()>;
    /// 待たずに試す。取れたら true（NOWAIT、CREATE / DROP DATABASE の「他の接続がない」の判定に使う）
    pub fn try_acquire(&self, id: BackendId, tag: LockTag, mode: LockMode, scope: LockScope) -> bool;
    pub fn release(&self, id: BackendId, tag: LockTag, mode: LockMode);
    /// Transaction: トランザクションスコープのロックだけ。Session: すべて（接続を閉じるとき）
    pub fn release_all(&self, id: BackendId, scope: LockScope);
    pub fn holds(&self, id: BackendId, tag: LockTag, mode: LockMode) -> bool;
    /// XactLockTableWait: xid の Share を取って即解放し、still_running(xid) が真なら繰り返す
    pub fn wait_for_xact(&self, id: BackendId, xid: Xid, still_running: &dyn Fn(Xid) -> bool, wait: &WaitCtl<'_>) -> Result<()>;
    pub fn blocking_backends(&self, id: BackendId) -> Vec<BackendId>;           // pg_blocking_pids
    pub fn is_blocked_by(&self, id: BackendId, among: &[BackendId]) -> bool;    // pg_isolation_test_session_is_blocked
    pub fn lock_status(&self) -> Vec<LockStatusRow>;                            // pg_locks
}
#[derive(Clone, Debug)]
pub struct LockStatusRow {
    pub locktype: &'static str,        // "relation" | "tuple" | "transactionid" | "database" | "object"
    pub database: Option<Oid>, pub relation: Option<Oid>, pub page: Option<u32>, pub tuple: Option<u16>,
    pub transactionid: Option<Xid>, pub backend: BackendId, pub pid: i32, pub mode: &'static str, pub granted: bool,
    // 01 §4.2 が足した項目（R-37）: pg_locks の 16 列を PG17 と同じにするため
    pub classid: Option<Oid>, pub objid: Option<Oid>, pub objsubid: Option<i16>, pub waited: Option<Duration>,
}
// LockManager の追加メソッド（01 §4.2・§11-2）: with_knobs(DebugKnobs)、has_waiters(id, tag) -> bool（03 C5）、held_locks、waiting_on。
// LockTag::describe、LockMode::{ALL, bit, from_sql_words}
```

- 待ち行列は FIFO。ただし `ProcSleep` の割り込み規則（自分がすでに持つモードと衝突する待ち手が先行者にいるときは、その前に入る）を入れる（D9）。
- 待ちの途中でも `LockManager` の Mutex を持ったまま I/O をしない。待ちは `Condvar::wait_timeout` の短い周期（10〜50ms）で、キャンセル・各タイムアウト・`deadlock_timeout` を確かめる。
- 同じ `BackendId` が持つロックは互いに衝突しない。同じ `(tag, mode)` を複数回取った場合は参照カウントで数える。

### 4.3 txn（LK、RW）

```rust
// txn/mod.rs
impl Snapshot {
    /// 自分のトランザクションの XID か（可視性判定は own_xid との == を直接書かず、これを使う。D2）
    pub fn is_own(&self, x: Xid) -> bool;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IsolationLevel { ReadUncommitted, ReadCommitted, RepeatableRead }     // SERIALIZABLE は作れない
impl IsolationLevel { pub fn uses_xact_snapshot(self) -> bool; }                // RepeatableRead だけ true

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LockTupleMode { KeyShare, Share, NoKeyExclusive, Exclusive }

// txn/snapshot.rs（LK-3）
/// 登録済みスナップショット。Drop で登録が外れ、horizon の計算から消える
#[derive(Debug)] pub struct RegisteredSnapshot { /* Snapshot, Arc<ProcArray>, reg_xmin: Xid */ }   // Deref<Target = Snapshot>。Drop は ProcArray の登録簿を 1 減らす（TxnManager を持たない。01 §4.4。R-37）
// impl RegisteredSnapshot { pub fn for_statement(&self, own: Option<Xid>, curcid: CommandId) -> Snapshot; }   // RR の文用: xmin / xmax / xip を保ち own_xid と curcid だけ差し替えた、登録されないコピー（01 §11-6）

// txn/manager.rs（LK-3）
/// トランザクションの XID を表すトークン（M3 の WriterGuard の後継）。
/// Drop の安全網: 終わっていなければメモリ上のアボート（clog に ABORTED、実行中一覧から削除、XID ロックと
/// トランザクションスコープのロックの解放）をする。I/O はしない（pending_creates のファイルは消せない。WARNING）
#[derive(Debug)] pub struct XidGuard { /* Arc<TxnManager>, BackendId, xid, finished */ }

impl TxnManager {
    pub fn new(procs: Arc<ProcArray>, clog: Arc<Clog>, control: Arc<ControlFileHandle>, wal: Arc<Wal>,
               locks: Arc<LockManager>, knobs: DebugKnobs) -> Arc<Self>;
    /// 最初の書き込みで呼ぶ（Transaction.xid が None のときだけ）。ライターロックは無い。
    /// proc の Mutex の中で XID を採り、実行中一覧に入れ、`TransactionId(xid)` を Exclusive で LockManager に登録する（D37）
    pub fn assign_xid(self: &Arc<Self>, backend: BackendId) -> Result<(Xid, XidGuard)>;
    pub fn take_snapshot(self: &Arc<Self>, own: Option<Xid>, curcid: CommandId) -> RegisteredSnapshot;
    /// 登録しない一瞬のスナップショット。使ってよいのは単体テストと `pg_current_snapshot` 風の診断だけ（LK-D9）。
    /// **カタログ用スナップショット（D12）・名前解決・アナライザ・実行器は `take_snapshot` を使う**（R-02）。RR の文のスナップショットは
    /// `RegisteredSnapshot::for_statement(own, curcid)`（xact_snapshot の写しに own_xid / curcid を入れる。01 §4.4）
    pub fn snapshot(&self, own: Option<Xid>, curcid: CommandId) -> Snapshot;
    /// min(実行中の XID, 登録済みスナップショットの xmin, next_xid)（D11）
    pub fn oldest_xmin(&self) -> Xid;
    pub fn is_in_progress(&self, xid: Xid) -> bool;
    /// §5.2 の手順 1（WAL → flush → clog → 実行中一覧）。ロックは解放しない（呼び出し側が最後に解放する。D38）
    pub fn commit(&self, xid: Xid, dropped: &[RelFileLocator]) -> Result<()>;
    pub fn abort(&self, xid: Xid, created: &[RelFileLocator]) -> Result<()>;
    pub fn commit_gate_exclusive(&self) -> Result<GateWrite<'_>>;              // M3 のまま
    pub fn statement_barrier(&self) -> Result<BarrierRead<'_>>;                // D10: チェックポイントの書き出しだけが持つ
    pub fn exclusive_barrier(&self) -> Result<BarrierWrite<'_>>;               // ファイルの削除が持つ
    pub fn locks(&self) -> &Arc<LockManager>;
    pub fn multixact(&self) -> &Arc<MultiXactTable>;
    // next_xid、xid_counters、clog は M3 のまま。begin_write、writer_owner、is_blocked_by は削除
}

#[derive(Debug)]
pub struct Transaction {
    pub xid: Option<Xid>,
    pub cid: CommandId, pub cid_used: bool,
    pub guard: Option<XidGuard>,                         // writer: Option<WriterGuard> の後継
    pub pending_creates: Vec<RelFileLocator>, pub pending_unlinks: Vec<RelFileLocator>,
    pub catalog_dirty: bool,
    pub isolation: IsolationLevel,
    pub xact_snapshot: Option<RegisteredSnapshot>,      // RR: 最初の文で取ったもの（トランザクション終了まで保持）
    // M3 が足した特性（read_only、deferrable、タイムアウト関連）はそのまま
}
impl Transaction {
    pub fn owns_xid(&self, x: Xid) -> bool;             // 今は self.xid == Some(x)。サブトランザクションが入ったら拡張（D2）
    // write_ctx、command_counter_increment は M3 のまま
}

// txn/clog.rs
impl Clog {
    // status は RwLock（ページ表）の読み + AtomicU8 の読みだけ。set_status は compare_exchange のループ（D13、01 LK-D10）
    // open(vfs, next_xid, oldest)・oldest()・status の xid < oldest の XX001・sweep_old_segments・set_status_redo の無視は 03 C6（LK-3b がロックなし化と同時に口を作り、VC-4 が切り詰めを足す）
    /// VC-4: oldest 未満のセグメントを消す（メモリからも落とす）。呼び出し側が先に control.oldest_xid を fsync する
    pub fn truncate_before(&self, oldest: Xid) -> Result<()>;
}
```

### 4.4 txn::multixact（RW-2）

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct MultiXactId(pub u64);                        // 0 は無効。tuple の xmax に IS_MULTI と一緒に入る

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MultiMember { pub xid: Xid, pub mode: LockTupleMode }     // 更新者は入れない（D6）

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MultiXactState { Live(Vec<MultiMember>) /* まだ実行中のメンバーだけ */, Dead /* 全員終了、または起動前の ID */ }

#[derive(Debug)]
pub struct MultiXactTable { /* Mutex<{ next, limit, boundary, map, by_xid }>, control */ }
impl MultiXactTable {
    pub fn new(control: Arc<ControlFileHandle>) -> Self;                  // boundary = next = limit = control.next_multi。Result を返さない（制御ファイルの値を読むだけ。R-06）
    pub fn create(&self, members: &[MultiMember]) -> Result<MultiXactId>; // 同じメンバー集合は同じ ID を返してよい。先取りは制御ファイルへ
    pub fn expand(&self, id: MultiXactId, still_running: &dyn Fn(Xid) -> bool) -> MultiXactState;
    pub fn on_xact_end(&self, xid: Xid);                                  // メンバー全員が終了したエントリを消す
}
```

### 4.5 storage（RW、VC）

```rust
// storage/mod.rs（TableStore の変更。M2 §4.4、M3 §4.5 に対して）
pub trait TableStore: Send + Sync + std::fmt::Debug {
    // create_storage、storage_exists、unlink_storage、insert、begin_scan、scan_next、fetch は M3 のまま
    /// wait が None なら待たずに BeingModified を返す（内部用）。Some なら PostgreSQL の l1: ループで待つ（D7）。
    /// crosscheck は FK の RR 用（PostgreSQL の crosscheck snapshot）
    fn delete(&self, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid,
              wait: Option<&WaitCtx<'_>>, crosscheck: Option<&Snapshot>) -> Result<TmResult>;
    fn update(&self, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid, new_row: &[Datum],
              wait: Option<&WaitCtx<'_>>, crosscheck: Option<&Snapshot>) -> Result<UpdateOutcome>;
    /// 行ロック。follow_updates が真なら ctid の連鎖をたどって最新版をロックし、latest に返す
    fn lock_tuple(&self, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid, mode: LockTupleMode,
                  policy: RowWait, follow_updates: bool, wait: &WaitCtx<'_>) -> Result<LockOutcome>;
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowWait { Block, Skip, Error }                  // FOR ... [SKIP LOCKED | NOWAIT]
#[derive(Clone, Debug)]
pub struct LockOutcome { pub result: TmResult, pub latest: Option<HeapTuple> }
```

- `TmResult` は M2 のまま（`Ok`、`Invisible`、`SelfModified{cmax}`、`Updated{ctid, xmax}`、`Deleted{xmax}`、`BeingModified{xmax}`、`WouldBlock`）。`Updated` / `Deleted` の `xmax` は更新者の XID。
- `TableDef` に `key_columns: Vec<bool>`（attnum − 1 の順。一意インデックスで部分・式でないものの列の和集合。M4 の `pg_index` から作る）を足し、`RelHandle` に `Arc` で写す。UPDATE がキー列を変えたかの判定（`KEYS_UPDATED`）に使う（RW-1）。
- `HeapStore::new(pool, clog, wal, locks: Arc<LockManager>, procs: Arc<ProcArray>, multixact: Arc<MultiXactTable>, fsm: Arc<FreeSpaceMap>)`。`ProcArray`（`txn/proc_array.rs`。LK-3 が `TxnManager` から切り出す）は、実行中の XID の一覧・XID の採番・登録済みスナップショットの登録簿を持ち、`is_in_progress` / `oldest_xmin` / `snapshot` を提供する。`StorageStack::new(vfs, cfg, wal, next_xid)` が `next_xid` から作って `HeapStore` と `TxnManager` に渡す（`TxnManager` を待たずに heap を組み立てられる）。**`HeapStore` が要る `LockManager`・`MultiXactTable` と、`Clog::open` に渡す `oldest_xid` は `StackConfig`（`locks`・`multixact`・`oldest_xid` の 3 項目）で渡し、`Clog` は `StorageStack::new` が作る（`TxnManager` は `stack.clog` を使う）。`Cluster::prepare` の組み立て順は 01 §5.12**（R-18）。`TableStore` に `inplace_update(&self, rel, tid, expect_xmin, patches) -> Result<InplaceOutcome>`（VC。03 C2）を足す。`UpdateOutcome { result, new_tid, lockmode }`・`LockOutcome { result, latest }` の定義は 02 §4.2。`LockTupleMode` に `PartialOrd, Ord, Hash`、`MultiMember` に `Hash`、`InsertOutcome` に `#[non_exhaustive]`、`RowMark` / `LRowMark` に `table_name: String`（02 §11-4。R-37）。
- FSM（VC-2）: `FreeSpaceMap::search(rel, needed) -> Result<Option<BlockNumber>>`、`record(rel, blk, free_bytes)`、`truncate(rel, nblocks)`。WAL なし。`hio.rs` が使う（M5 規約 4）。
- B+Tree（RW-5、VC-3。M4 §13.2 の `IndexStore` に対する変更。D44、D47）:

```rust
// storage/mod.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InsertOutcome { Inserted, WaitFor(Xid) }       // WaitFor のときは何も挿入していない（ラッチは外れている）

pub trait IndexStore: Send + Sync + std::fmt::Debug {
    // init_index、build、begin_scan、scan_next、nblocks、unlink_storage は M4 のまま
    /// UniqueCheck::Check で、同じキーの未確定の項目（挿入中・削除中の他トランザクション）に当たったら WaitFor(xid) を返す。
    /// 確定した重複は 23505。呼び出し側は wait_for_xact で待ち、同じ引数でもう一度呼ぶ
    fn insert(&self, w: &WriteCtx, index: &IndexHandle, key: &[Datum], tid: Tid, check: UniqueCheck<'_>) -> Result<InsertOutcome>;
    /// VACUUM の第 2 段（VC-3）。dead に含まれる TID を指す項目をすべて削除する。BTREE_VACUUM を 1 ページ 1 レコードで書く
    fn bulk_delete(&self, w: &WriteCtx, index: &IndexHandle, dead: &TidSet, wait: &WaitCtx<'_>) -> Result<BulkDeleteStats>;
}
#[derive(Debug, Default, Clone, Copy)]
pub struct BulkDeleteStats { pub pages_scanned: u32, pub tuples_removed: u64, pub tuples_remaining: u64 }
/// ソート済みの TID の集合（VACUUM が溜める。maintenance_work_mem が上限）
#[derive(Debug, Default)]
pub struct TidSet { /* Vec<Tid>。ブロック番号順 */ }
impl TidSet { pub fn contains(&self, t: Tid) -> bool; pub fn len(&self) -> usize; pub fn push(&mut self, t: Tid); }
```

- 一意検査の `fetch_dirty`（M4 §13.1）は、`DirtyResult::WaitFor(xid)` を返したら `InsertOutcome::WaitFor(xid)` にする（M4 では内部エラーだった箇所）。`UniqueCheck::Check` の `own_xid` は `Snapshot::is_own` に揃える（D2）。
- 複数ライターで壊れうる M4 の B+Tree の前提の洗い出し（RW-5）: 構造変更中の木を別のライターが降下する、親を探し直す（M4 §19 の `_bt_getstackbuf` 相当）、右隣への移動（`_bt_moveright`）、メタページの更新、ルートの分割、同じキーを同時に挿入する 2 つのトランザクション。`BTREE_PAGES` は全ページ画像なので、構造変更の 1 レコードの原子性は保たれる。M5 の VACUUM は葉の項目を消すだけで、ページの削除・併合はしない（M6）ので、M4 の「ページの削除フラグを予約」はそのまま予約のまま。

### 4.6 executor と planner（RW、FK）

M4 の型（`Expr<C, Q>`、`BoundSelect`、`LogicalPlan`、`PhysicalPlan`、`ExecCtx`。M4 §6〜§10）に足すものだけを書く。

```rust
// expr/mod.rs（D45）: ExprKind に 1 変種。Bound・論理・物理のすべてに現れてよい（M4 §6.4 の表の「それ以外」の行）
//   ExternParam(u16)       Extended Query の $n（0 始まりで n − 1）。型は Expr.ty。値は ExecCtx.bind_params[i]

// analyzer/bound.rs（RW-3）
pub struct BoundSelect { /* M4 の項目に加えて */ pub locking: Vec<BoundLockingClause> }
pub struct BoundLockingClause {
    pub strength: LockTupleMode, pub wait: RowWait,
    /// FOR ... OF t1, t2。空 = FROM 句の（副問い合わせの中を除く）すべてのテーブル RTE
    pub rtes: Vec<RteId>,
}

// executor/mod.rs
pub struct ExecCtx<'a> {
    // M4 §10 の項目（catalog、storage、indexes、txn、snapshot、session、runtime、interrupts、query、params、mem、subplans、ctes、type_env）に加えて
    pub locks: &'a Arc<LockManager>,             // dml::wait_for_xid だけが使う（heap の待ちは heap が持つ Arc<LockManager>。R-05）
    pub procs: &'a Arc<ProcArray>,               // 同上（wait_for_xact の still_running = procs.is_in_progress）
    pub wait: WaitCtx<'a>,                       // 行ロック・FK の検査・一意検査の待ち
    pub bind_params: &'a [Datum],                // ExprKind::ExternParam(i) が bind_params[i] を読む
    pub xact_snapshot: Option<&'a Snapshot>,     // Repeatable Read のときだけ Some（snapshot と同じもの。crosscheck と 40001 の判定に使う）
    pub ri: &'a mut RiQueue,                     // FK のイベントキュー（FK が型を持つ）
}

// executor/dml.rs（RW-1・RW-5 が持つ。INSERT・UPDATE・DELETE・COPY FROM・FK の RI の action が通る共有関数。02 §4.6 と 07 §4.5 の合意。R-05）
pub fn insert_with_indexes(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid>;         // 成功したら ri::after_insert
pub fn update_with_indexes(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid, old_row: &[Datum],
                           new_row: &[Datum], crosscheck: Option<&Snapshot>) -> Result<UpdateOutcome>;                  // Ok のとき ri::after_update
pub fn delete_row(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid, old_row: &[Datum],
                  crosscheck: Option<&Snapshot>) -> Result<TmResult>;                                                   // Ok のとき ri::after_delete
pub fn wait_for_xid(ctx: &mut ExecCtx<'_>, xid: Xid) -> Result<()>;
// crosscheck は引数（通常の文は None。RI の action は ctx.ri.crosscheck()）。INSERT・UPDATE・DELETE のノードは入力を使い切ったら ri::finish_statement(ctx) を呼ぶ

// planner/logical.rs（RW-3）。LogicalPlan に足す変種と、Update / Delete の欄
pub struct LRowMark { pub rel: RelHandle, pub tid: ColId, pub cols: Vec<ColId>, pub mode: LockTupleMode, pub wait: RowWait }
pub struct LRecheck {
    /// WHERE（と結合条件）の全体。元の Bound の filter から作る（書き換え後の木ではない）
    pub qual: Option<LExpr>,
    /// Update のみ。代入式（new_values と同じ順）
    pub new_values: Vec<LExpr>,
    /// qual / new_values が参照する、対象表以外の列。physicalize が入力の末尾に隠し列として足す。
    /// 刈り込み（prune）のルールはこれを「使われている列」に数える
    pub extra_cols: Vec<ColId>,
}
//   LogicalPlan::LockRows { input: Box<LogicalPlan>, marks: Vec<LRowMark>, recheck: Option<LExpr> }
//   LogicalPlan::Update / Delete に recheck: Option<LRecheck> を足す

// planner/physical.rs（RW-3、LK）
/// 更新競合（TmResult::Updated / BeingModified の後）に、最新版でもう一度評価し直す式。
/// 評価の対象は「ノードの入力行の、対象表の列（Update / Delete は先頭 n_user_cols 個と ctid、LockRows は各 mark の cols と tid_col）を
/// 最新版に差し替えた行」。他テーブルの列は元の入力行のまま固定する（PostgreSQL の ROW_MARK_REFERENCE と同じ意味。D8）
pub struct RecheckSpec {
    pub qual: Option<PhysExpr>,          // None = 再評価不要。偽になったらその行は飛ばす
    pub new_values: Vec<PhysExpr>,       // Update のみ。assigned と同じ順。差し替えた行で評価し直す
}
pub struct RowMark {
    pub rel: RelHandle, pub tid_col: usize, pub cols: std::ops::Range<usize>,
    pub mode: LockTupleMode, pub wait: RowWait,
}
pub enum PhysicalPlan {
    // M4 の Update { rel, input, n_user_cols, assigned, checks, not_null, table_name, returning } に recheck: Option<RecheckSpec> を足す
    // M4 の Delete { rel, input, n_user_cols, returning } に recheck: Option<RecheckSpec> を足す
    // Insert の returning は M4 の欄（RETURNING の仕上げは RW-6）
    /// Limit の下、Sort の上に置く（PostgreSQL の LockRows と同じ位置）。recheck は入力行の並びで評価する
    LockRows { input: Box<PhysicalPlan>, marks: Vec<RowMark>, recheck: Option<PhysExpr> },
    /// 仮想リレーションの走査（pg_locks、pg_roles）
    VirtualScan { rel_oid: Oid, columns: Vec<SqlType> },
}
```

- ノードの内部（EPQ のループ、`LockRows` の LIMIT との相互作用、`recheck.qual` に SubLink があるとき）は RW-3 が決める。ここで固定するのは、`recheck` が計画に載ること、再評価の行の作り方（入力行の対象表の部分だけを最新版に差し替える）、`ExecCtx` の項目。
- `FOR UPDATE` などは `SELECT` の最上位だけで受け付ける。DISTINCT・GROUP BY・集約・集合演算と一緒なら PostgreSQL と同じ `0A000`（`FOR UPDATE is not allowed with DISTINCT clause` などの文言は RW が実機で確かめて固定する）。
- FK の `RiQueue` とイベントの型は FK-2 が決める。`ExecCtx.ri` の口だけをここで固定する。

### 4.7 sql・ddl・session（F0、XQ、LK）

```rust
// sql/ast.rs: Statement に足す変種（F0 が中身の空の構造体つきで作り、各章が埋める）。M4 が持つ Vacuum / Analyze / Truncate / AlterTable は中身を拡張する
//   LockTable(LockTableStmt)        LK
//   Vacuum(VacuumStmt)  Analyze(AnalyzeStmt)                             VC（M4 のスタブにオプション（VERBOSE など）と対象表を足す）
//   Prepare(PrepareStmt)  ExecutePrepared(ExecuteStmt)  Deallocate(DeallocateStmt)  Discard(DiscardStmt)   XQ
//   CreateRole(..)  AlterRole(..)  DropRole(..)                          AU
//   CreateDatabase(..)  DropDatabase(..)                                 DB
//   AlterTable の action に AddForeignKey / DropConstraint を足す         FK
// SelectStmt に locking: Vec<LockingClause>（RW）。INSERT / UPDATE / DELETE の returning は M4 の欄（RW-6 が仕上げる）。
// Expr に Param { index: u16 }（XQ-3）。型名に bytea / uuid / `T[]` / `INTERVAL` / `TIME` のリテラル（TY、TD）
```

```rust
// analyzer/bound.rs: BoundDdl に足す変種（M4 §7 の列挙に追加。フィールドは持ち主の章が決める）
//   LockTable(BoundLockTable)                                             LK
//   Vacuum(BoundVacuum) / Analyze(BoundAnalyze)                           VC（M4 の BoundVacuum を拡張）
//   CreateRole(BoundCreateRole)  AlterRole(BoundAlterRole)  DropRole(BoundDropRole)   AU
//   CreateDatabase(BoundCreateDatabase)  DropDatabase(BoundDropDatabase)  DB
//   AlterTableAddForeignKey(BoundAddForeignKey)  AlterTableDropConstraint(BoundDropConstraint)   FK
// BoundCreateTable に foreign_keys: Vec<BoundForeignKey>（FK）。Truncate は M4 の変種（D42）
// PREPARE / EXECUTE / DEALLOCATE / DISCARD は BoundDdl にしない（Session が直接処理する。D41）

/// Extended Query 用の入口（XQ-3）。analyze の仲間で、`$n` の型を推論する。analyze(stmt, catalog) は変えない
/// declared は Parse メッセージで宣言された型の OID（0 = 未指定）。戻り値の Vec<Oid> は全パラメータの型（ParameterDescription）
pub fn analyze_with_params(stmt: &Statement, catalog: &dyn CatalogReader, declared: &[Oid]) -> Result<(BoundStatement, Vec<Oid>)>;
```

```rust
// ddl/mod.rs: M4 の DdlCtx に足す項目（M4 §14.6。D41）
pub struct DdlCtx<'a> {
    // M4: cluster、db、snapshot、catalog、txn、role_oid、notices、type_env
    pub backend: BackendId,
    pub wait: &'a WaitCtl<'a>,
    pub session: &'a SessionInfo,
    pub role: &'a RoleRow,
    pub password: PasswordPolicy,    // { encryption, scram_iterations }。session が Settings から文ごとに作る（08 C4。R-37）
    /// 暗黙のトランザクションで、この問い合わせの最初で唯一の文であるときだけ true
    /// （PostgreSQL の PreventInTransactionBlock。VACUUM / CREATE DATABASE / DROP DATABASE のブロック内判定は execute_standalone の前に session が行う）
    pub outside_block: bool,
}
// pub fn execute(ctx: &mut DdlCtx<'_>, ddl: BoundDdl) -> Result<String>      M4 のまま。**次の 3 変種（`Vacuum`・`CreateDatabase`・`DropDatabase`）は Err(Error::internal) にする**（`execute_standalone` が処理する）。`Analyze`（VACUUM を伴わないもの）は `execute` が外側のトランザクションの中で実行する（03 VC-D15・C4。R-37・R-29）

/// 文が自分でトランザクションを閉じて開き直す口（VACUUM はテーブルごとに、CREATE / DROP DATABASE はコミットの後の手順がある）。Session が実装する（session/txn_ctl.rs）
pub trait TxnControl {
    /// 現在の暗黙のトランザクションに対する DdlCtx（スナップショットと catalog はこの時点のもの）。commit_and_restart の後は作り直す
    fn ddl_ctx(&mut self) -> Result<DdlCtx<'_>>;
    /// 暗黙のトランザクションをコミットして新しい暗黙のトランザクションを開く（§5.2。**トランザクションスコープのロックが解放される。セッションスコープのロック（`Database(oid)` の AccessShare。D40）は残る**。01 §11-16、R-37）
    fn commit_and_restart(&mut self) -> Result<()>;
}
/// BoundDdl::{Vacuum, CreateDatabase, DropDatabase} の実行（VACUUM ANALYZE を含む。単独の ANALYZE は含まない）。コマンドタグを返す
pub fn execute_standalone(ctl: &mut dyn TxnControl, ddl: BoundDdl) -> Result<String>;
```

```rust
// session/mod.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format { Text = 0, Binary = 1 }

pub struct ColumnDesc { /* M2 の項目に加えて */ pub format: Format }       // M1〜M4 は常に Text

pub trait ResultSink {
    // 既存のメソッド（M4 の COPY 用の copy_in_response を含む）はそのまま
    /// 列ごとの形式に従ってエンコード済みの行（Extended Query）。既定の実装は全列 Text として data_row に流す
    fn data_row_raw(&mut self, values: &[Option<Vec<u8>>]) -> std::io::Result<()>;
}
```

```rust
// session/extended.rs（XQ-2）。Extended Query のメッセージ 1 つにつき 1 メソッド
pub struct StatementDescription { pub param_types: Vec<Oid>, pub columns: Option<Vec<ColumnDesc>> }   // None = NoData
pub enum ExecOutcome { Complete(String /* コマンドタグ */), Suspended, EmptyQuery, CopyIn /* XQ-6。Extended 経由の COPY FROM。04 C2 */ }
// ResultSink に copy_out_response / copy_out_data / copy_out_done を足す（既定の実装は Err(Unsupported)。04 C2）。Session に pending_parameter_status(&mut self) -> Vec<(String, String)>（04 C3）

impl Session {
    pub fn msg_parse(&mut self, name: &str, sql: &str, param_types: &[Oid]) -> Result<()>;
    pub fn msg_bind(&mut self, portal: &str, stmt: &str, param_formats: &[Format],
                    params: &[Option<&[u8]>], result_formats: &[Format]) -> Result<()>;
    pub fn msg_describe_statement(&mut self, name: &str) -> Result<StatementDescription>;
    pub fn msg_describe_portal(&mut self, name: &str) -> Result<Option<Vec<ColumnDesc>>>;
    pub fn msg_execute(&mut self, portal: &str, max_rows: u32, sink: &mut dyn ResultSink) -> Result<ExecOutcome>;
    pub fn msg_close_statement(&mut self, name: &str);                 // 存在しなくても成功
    pub fn msg_close_portal(&mut self, name: &str);
    /// Sync: 暗黙のトランザクションを閉じる（コミットの失敗は Err）。ReadyForQuery の状態は transaction_status()
    pub fn msg_sync(&mut self) -> Result<()>;
    /// ErrorResponse を送った直後に呼ぶ。暗黙のトランザクションを中断し、ポータルを破棄する
    pub fn msg_abort_implicit(&mut self);
}
```

- 状態機械（エラー後に Sync まで読み捨てる `SkipTillSync`）は yuzhu-server が持つ（§4.12）。Session は「エラーを返したら、その Execute の副作用は文の原子性で取り消されている」ことだけを保証する。
- Simple Query の `Q` を受けたら無名の文と無名のポータルを破棄する（`execute_simple` の先頭で行う）。

### 4.8 Cluster と認証・データベース（LK、AU、DB）

```rust
impl Cluster {
    pub fn lock_manager(&self) -> &Arc<LockManager>;
    pub fn backends(&self) -> &Arc<BackendRegistry>;
    pub fn multixact(&self) -> &Arc<MultiXactTable>;
    /// 認証の前に呼ぶ。存在しなければ None（存在を推測させないため、呼び出し側は偽のソルトで最後まで進む。AU）
    pub fn lookup_role(&self, user: &str) -> Result<Option<RoleAuthInfo>>;
    /// 認証の後に呼ぶ（M2 の connect を置き換える）。3D000 / 55000 / 28000 / 53300 を FATAL で返す。
    /// 成功すると Database(oid) を AccessShare でセッションスコープに取り、BackendRegistry に登録する
    pub fn connect(&self, req: &ConnectRequest<'_>) -> Result<ConnectGrant>;
    pub fn database_handle(&self, oid: Oid) -> Result<Arc<DatabaseHandle>>;
    /// DROP DATABASE の後始末（キャッシュされた DatabaseHandle を捨てる）
    pub fn forget_database(&self, oid: Oid);
}
pub struct RoleAuthInfo {
    pub oid: Oid, pub name: String, pub password: Option<String>,    // rolpassword（SCRAM の保存形式）
    pub valid_until: Option<i64>,                                    // UNIX エポックからのマイクロ秒
    pub can_login: bool, pub conn_limit: i32, pub superuser: bool,
}
pub struct ConnectRequest<'a> {
    pub database: &'a str, pub user: &'a str, pub client_addr: Option<std::net::IpAddr>,
    pub session_id: u64, pub pid: i32, pub interrupts: Arc<InterruptFlag>,
}
pub struct ConnectGrant { pub db: Arc<DatabaseHandle>, pub role: RoleRow, pub session_locks: SessionLocks, pub backend: BackendGuard }
// フィールドの順序が Drop の順序（session_locks が backend より前）。SessionLocks は dbase.rs（09 §6.7）。Session のフィールドも txn → session_locks → backend の順（01 LK-D26）
// RoleRow に create_db: bool、create_role: bool、conn_limit: i32 を足す。DatabaseRow に conn_limit: i32、owner: Oid、encoding・collate・ctype・frozen_xid・min_mxid を足す（09 §11-7。R-37）
// connect の検査の順序は、ロール（28000）→ ロールの接続数（53300）→ データベースの存在（3D000）→ Database ロック → 行の取り直し → datallowconn（55000）→ データベースの接続数（53300）（09 DB-D1。上の SQLSTATE の列挙は順序ではない。08 AU-D10、R-21）
```

- `Session::new(cluster, params)` は内部で `connect` を呼ぶ（M2 と同じ。テストはそのまま使える）。サーバは `lookup_role` → 認証 → `Session::new` の順に呼ぶ。

### 4.9 types（TY、TD、XQ-4）

```rust
// types/datum.rs（F0 が変種をまとめて足す。中身の型は TY / TD が実装する）
pub enum Datum {
    // 既存。M4 が Numeric(Box<yuzhu_numeric::Numeric>)、BpChar(String)、Date、Timestamp、TimestampTz、Int2Vector(Vec<i16>) を足している（M4 §12.2）
    // M5 で足すもの
    Time(i64 /* 0 時からのマイクロ秒 */), Interval(yuzhu_datetime::Interval),
    Bytea(Vec<u8>), Uuid([u8; 16]),
    Array(Box<ArrayValue>),          // M3 の Int4Array を置き換える。M4 の int2[]（Int2Vector）も Array にする（D25）。
                                     // pg_isolation_test_session_is_blocked の第 2 引数も Array。Int2Vector は int2vector 型だけに残る
}
/// PostgreSQL の ArrayType と同じ意味。ndim = dims.len()（空の配列は dims が空）。M5 が作るのは dims.len() <= 1 だけ。items は行優先の平坦な並び
pub struct ArrayDim { pub len: i32, pub lbound: i32 }
pub struct ArrayValue { pub elem_type: Oid, pub dims: Vec<ArrayDim>, pub items: Vec<Datum> /* NULL 要素は Datum::Null */ }

// types/binary.rs（XQ-4 が振り分け、型ごとの実体は各型のモジュール。**形は 04 §4.4 に統一した。レビュー対応 R-04**）
pub fn supports_binary(ty: SqlType) -> bool;                            // 05 §6.4 の表の型（aclitem・anyarray・record・cstring・internal と NULL 専用の配列は false）
pub fn output_binary(d: &Datum, ty: SqlType) -> Result<Vec<u8>>;        // PostgreSQL の *send と同じ形式。NULL は呼ばない
pub fn input_binary(buf: &[u8], ty: SqlType) -> Result<Datum>;          // *recv + RecvBuf::finish()。typmod は −1 として扱い、適用は呼び出し側（計画の CoerceTypmod。XQ-D11）
// 長さの過不足（PG の ReceiveFunctionCall と同じ。04 XQ-D10・05 §6.4）: **不足は 08P01 `insufficient data left in message`（RecvBuf の get_* が返す）、余りは 22P03 `incorrect binary data format`（input_binary が finish() で返す）**。
// 値の中身の不正（numeric の符号、配列の次元、time の範囲 22008 など）は型の binary_recv が返す。
pub struct RecvBuf<'a> { /* buf: &'a [u8], pos: usize。get_u8 / get_i16 / get_i32 / get_i64 / get_f32 / get_f64 / get_bytes(n) / get_rest() / remaining() / finish() */ }
// 各型のモジュール（bytea・uuid・array・numeric・bpchar・datetime・interval・time ほか）が公開する実体（05 §4.6、06 §4。名前と形を 1 つにする）:
//   pub fn binary_send(d: &Datum, ty: SqlType) -> Result<Vec<u8>>;
//   pub fn binary_recv(buf: &mut RecvBuf<'_>, ty: SqlType) -> Result<Datum>;     // ty.typmod は −1
// 配列は要素の codec（04 の BinaryCodec { send, recv }）を再帰で引く
```

- 型の入出力・比較・ハッシュ・キャスト・演算子・関数の登録は `catalog/builtin/` の対応するファイルと `types/` の対応するモジュールに置く。`pg_type` の `typreceive` / `typsend` / `typmodin` / `typmodout` 列と、対応する `pg_proc` の行は TY-1 が M2-Q13 の持ち越しとして埋める。
- 新しい型は、M4 §12.2 の規約どおり **`cmp_datum`・`types::hash::hash_datum` の対応**（`Interval` は PostgreSQL と同じ正規化した 3 要素の比較、`Bytea` はバイト列の辞書順、`Uuid` は 16 バイトの辞書順、`Time` は整数）と、**B+Tree の opclass の行**（`catalog/opclass.rs` の `OPFAMILIES` / `OPCLASSES` / `AMOPS` / `AMPROCS` に `bytea_ops`・`uuid_ops`・`time_ops`・`interval_ops`。OID は `pg_opfamily.dat` で確認）を足す。`id uuid PRIMARY KEY` を動かすため。配列と bytea 以外の配列要素型の opclass は作らない（`CREATE INDEX` は `42704`。M5-Q8）。
- `Array` の比較（`=`・`<>`）は PostgreSQL の `array_eq` と同じ（要素ごとの比較。NULL 要素どうしは等しい。未検証）。ハッシュと順序比較（`<` など）は M6。

### 4.10 catalog（LK、AU、VC）

```rust
// catalog/virtual_rel.rs（LK-5）
pub trait VirtualRelation: Send + Sync + std::fmt::Debug {
    fn oid(&self) -> Oid;
    fn name(&self) -> &'static str;
    fn columns(&self) -> &'static [CatalogColumn];
    fn rows(&self, ctx: &VirtualCtx<'_>) -> Result<Vec<Row>>;       // VirtualCtx: Cluster、現在のデータベース、権限
}
pub fn registry() -> &'static [&'static dyn VirtualRelation];       // pg_locks（LK）、pg_roles（AU）
// initdb が pg_class に relkind 'v' の行と pg_attribute の行を書く。アナライザは通常のテーブルと同じに扱い、
// planner が PhysicalPlan::VirtualScan にする。INSERT / UPDATE / DELETE は 55000 (object_not_in_prerequisite_state。`cannot insert into view "pg_locks"` など。DETAIL・HINT つき。【実機】PG17.11 のビューと同じ)、TRUNCATE / DROP TABLE は 42809 (`"pg_locks" is not a table`)、FOR UPDATE は黙って無視（02 RW-K4）。エラーを返す関数は LK が提供する（01 §6.5。R-10）

// catalog/schema.rs: yz_relxid（OID 9801、データベースごと、mapped でない）
//   relid oid NOT NULL, relfrozenxid8 int8 NOT NULL     ← CREATE TABLE と initdb が行を作り、VACUUM が進める

// yz_datxid（OID 9802、共有。db_oid = 0。D51）
//   datid oid NOT NULL, datfrozenxid8 int8 NOT NULL     ← initdb と CREATE DATABASE が行を作り、DROP DATABASE が消し、VACUUM が進める
```

### 4.11 yuzhu-auth（AU-1）

```rust
pub mod scram {
    pub const DEFAULT_ITERATIONS: u32 = 4096;
    /// "SCRAM-SHA-256$<iter>:<salt>$<StoredKey>:<ServerKey>"
    pub fn make_secret(password: &str, iterations: u32) -> Result<String, AuthError>;
    pub fn is_scram_secret(s: &str) -> bool;
    /// 平文パスワードの検証（hba の `password` 方式用）
    pub fn verify_plain_password(password: &str, secret: &str) -> bool;
    pub struct ServerExchange { /* 状態機械 */ }
    impl ServerExchange {
        /// secret が None（ロールが無い・パスワードが無い・SCRAM でない）なら、ユーザー名から決まる偽のソルトで進め、最後に失敗させる
        pub fn new(username: &str, secret: Option<&str>) -> Result<Self, AuthError>;
        pub fn client_first(&mut self, msg: &[u8]) -> Result<Vec<u8>, AuthError>;   // server-first-message
        pub fn client_final(&mut self, msg: &[u8]) -> Result<Vec<u8>, AuthError>;   // server-final-message（失敗は Err）
    }
}
pub mod random { pub fn fill(buf: &mut [u8]) -> std::io::Result<()>; }
```

### 4.12 yuzhu-server（XQ-1、AU-2）

```rust
// connection.rs
enum ExtState { Normal, SkipTillSync }      // エラー後は Sync か Terminate まで読み捨てる（Flush も無視）

// hba.rs
pub enum HbaMethod { Trust, Reject, Password, ScramSha256 }
pub struct HbaRules { /* Vec<HbaRule> */ }
impl HbaRules {
    pub fn parse(text: &str) -> Result<HbaRules, HbaError>;          // 起動時に失敗したら起動しない
    pub fn find(&self, addr: std::net::IpAddr, database: &str, user: &str) -> Option<HbaMethod>;  // 一致なしは 28000
}
```

- Flush（`H`）、Sync の応答（ReadyForQuery の後）、**ErrorResponse・NoticeResponse・CopyInResponse の後**で flush する（04 XQ-D13・C3。【実機】Flush なしでエラーが届く。R-37）。それ以外のメッセージでは flush しない。

---

## 5. 横断する処理の流れ

### 5.1 文の実行（M3 §5.2 のデータ文と DDL の部分を置き換える）

```
execute_simple(sql)
  0. interrupts.clear_cancel()。無名の文と無名のポータルを破棄する（XQ）
  ├─ 文ごとに:
  │   a. statement_timeout の期限を設定。Failed 状態の判定、BEGIN / COMMIT / ROLLBACK / SET / SHOW は M3 のまま
  │      （SET TRANSACTION ISOLATION LEVEL は RW。スナップショットを取った後は 25001）
  │   b. 書き込む文（生のパース木で判定）で読み取り専用のトランザクションなら 25006（ロックを取る前）。**`FOR` 句つき SELECT の 25006 は解析後（ロック対象が 1 つ以上あるときだけ。02 RW-D12。R-37）**
  │   c. PREPARE / EXECUTE / DEALLOCATE / DISCARD は Session が直接処理する（XQ。D41）。ロール・データベースの DDL と VACUUM は d の 1〜2 を行わず、
  │      analyze → BoundDdl → ddl::execute（トランザクションを自分で開閉する VACUUM、CREATE / DROP DATABASE は ddl::execute_standalone。§5.5）。
  │      DdlCtx.outside_block を渡す
  │   d. データ文と、リレーションを対象にする DDL（SELECT / INSERT / UPDATE / DELETE / CREATE TABLE / DROP TABLE / TRUNCATE / LOCK TABLE / ALTER TABLE / CREATE INDEX ...）:
  │       0'. Repeatable Read で、このトランザクションの最初のスナップショットを要する文（RW-D11 の `statement_needs_snapshot`）なら、
  │          **リレーションロックの前に** `xact_snapshot` を作る（`Session::statement_snapshot`。02 §4.5。LK-D27、RW-D10）。RC には 0' は無い
  │       1. 生のパース木から (リレーションの名前, LockMode) の一覧を作る（session/locking.rs。LK-4）
  │       2. 各リレーションを「カタログ用スナップショットで名前を解決 → LockManager::acquire → もう一度解決して
  │          OID が同じか確かめ、違えばロックを外してやり直す」。見つからなければ 42P01。複数あれば FROM の出現順
  │       3. 書き込む文、または FOR 句つきの SELECT で txn.xid が None なら TxnManager::assign_xid
  │       4. スナップショット: Read Committed は take_snapshot（文の終わりまで登録。**ロックの後**）。Repeatable Read は
  │          0' で作った txn.xact_snapshot の写し（`RegisteredSnapshot::for_statement`。curcid と own_xid を入れる。登録は xact_snapshot が保つ）
  │       5. catalog = StatementCatalog { snapshot: take_snapshot で取ったカタログ用スナップショット（自分の変更を含む。D12。登録する・文の終わりまで保持）, ... }
  │       6. analyze → （DDL なら ddl::execute）→ plan → build → next() ループ（行ごとに check_interrupts）。結果は Output に溜める
  │       7. 文のスナップショットの登録を外す。リレーションロックと XID ロックは外さない（トランザクションの終わりまで）
  │       8. buffer::assert_no_pins()。成功なら CCI。暗黙のトランザクションの最後の文ならコミット（§5.2）
  │       9. 失敗なら暗黙のトランザクションはアボート、ブロック内なら Failed。Severity::Panic は FATAL
  └─ M3 と同じ
```

- **Read Committed では、ロックはスナップショットより先**（D5、m5-concurrency §9.3）。UPDATE がテーブルロックを待った後で、待った後のスナップショットを取るため。**Repeatable Read の最初のスナップショットだけは、PostgreSQL と同じくロックの前（0'）**（【実機】RR の最初の文がテーブルロックを待っても、待った後に相手のコミットした行は見えない。RW-D10、LK-D27。D12 の注記: カタログ用スナップショットは RR でも文ごとに最新）。
- M3 の「書き込む文でライターロックを取る」手順は無い。M3 の `statement_barrier` を文の実行中に持つ手順も無い（D10）。
- `txn_snapshot_taken`（`SET TRANSACTION ISOLATION LEVEL` の 25001 判定）は RW-D11 の表に従う（M3 の「FROM のない SELECT は立てない」は誤りで、`SELECT 1` でも立てる）。
- Extended Query は、**Parse が `note_snapshot_use`（印だけ。RW-D11）**、**Bind が 0'〜2（RR の最初のスナップショット → ロック → 世代の比較 → 必要なら再アナライズ）に続けて、SELECT 系のポータル（`OneSelect`）だけ 3・4（XID の割り当て、スナップショット）も行い、DML のポータルは 3・4 を Execute で行う。DDL・COPY・ユーティリティ文は Bind で何もせず Execute が 0'〜6 をすべて行う**（XQ-D3、04 §5.5。レビュー対応 R-03）。

### 5.2 コミットとアボート（M3 §5.3 に、ロックの解放を足す。D38）

```
commit(txn)
  if let Some(xid) = txn.xid:
     1. txn_mgr.commit(xid, &pending_unlinks):  M3 の手順 1 のまま
          [commit_gate 共有] WAL のコミットレコードを挿入 → flush → proc の Mutex の中で clog に Committed、実行中一覧から外す
          → multixact.on_xact_end(xid)
     2. catalog_dirty なら cluster.invalidate_all_catalog_caches()
     3. pending_unlinks があれば exclusive_barrier の下で unlink_storage（ロックを持ったまま。チェックポイントの書き出しとだけ排他）
     4. locks.release_all(backend, LockScope::Transaction)      ← XID ロック（Exclusive）の解放で、待っていたセッションが起きる
     5. xact_snapshot の登録を外す。txn をリセット
abort(txn)
  1. txn_mgr.abort(xid, &pending_creates)   2. pending_creates を unlink  3. locks.release_all(.., Transaction)  4. 同上
```

- 4 が最後であることが要点。待っていたセッションは起きた直後に `still_running(xid)` を確かめ、clog を引く。1 より前に起こすと、コミット前の状態を見てしまう。
- XID のない読み取りだけのトランザクションも、手順 4・5 は行う（リレーションロックを持っている）。WAL は書かない。ただしシーケンスの WAL を書いたトランザクション（M4 の `Transaction.wal_flush_upto` が 0 でない）は、手順 4 の前に `TxnManager::finish_without_xid(wal_flush_upto)` で WAL を flush する（D48）。XID のあるトランザクションのコミットレコードの flush は、`wal_flush_upto` を含む（コミットレコードの LSN の方が大きい）。
- アボート時に、行ロック（xmax のロックビット）を戻す処理は要らない。ロック保持者が中断なら、他のセッションはそのロックを無効として扱う（clog が Committed でなく、実行中でもない）。

### 5.3 ロックの順序（M3 §5.9 を置き換える部分と、足す部分）

| 順 | ロック | 規則 |
|---|---|---|
| 1 | 重量ロック（`LockManager` で待つもの） | **待つ前に、ほかのロック・ラッチ・ピン・ゲートを持たない**（規約 1）。取得の順序は動的で、デッドロックは検出器が見つける。ライターロックは無い |
| 2 | `checkpoint_lock` | M3 のまま |
| 3 | ストレージバリア | D10: チェックポイントの書き出し（共有）とファイルの削除（排他）だけ。文の実行は持たない |
| 3a | コミットゲート | M3 のまま。持ったまま `LockManager` の待ちに入らない |
| 4 | コンテンツラッチ | M3 のまま（同じリレーションのブロック番号の小さい順）。B+Tree は下→上、左→右（m4-btree §4.1）。**FSM のページのラッチは、ヒープのページのラッチを持たずに取る** |
| 5〜6 | リレーション拡張ロック、マッピング表 | M3 のまま |
| 6a・6b | WAL の flush 用 / 挿入用 Mutex | M3 のまま |
| 7 | 葉のロック | M3 の一覧に `LockManager` の内部 Mutex、`MultiXactTable`、`ProcArray` の Mutex、`FreeSpaceMap` の内部を足す。例外（葉から葉へ取ってよい組）に **`ProcArray` の Mutex → `LockManager` の内部 Mutex**（XID ロックの登録。D37。待ちは起きない）を足す |

- 行ロックの待ちは、タプルロック（`Tuple` タグ）→ XID ロックの Share の順に、いずれもラッチを外してから取る（D7）。取ったタプルロックは、行に印を付けたらすぐ解放する。待ちが中断されたときも解放する。
- `LockManager` の待ちに入れる呼び出し元は、スレッドローカルの「保持中のラッチ数」が 0 であることを確かめる（デバッグビルドで panic）。

### 5.4 起動（M3 §5.5 に足す）

- 制御ファイルの `next_multi` で `MultiXactTable` の `boundary` を決める。`oldest_xid` 未満の `pg_xact` のセグメントが残っていれば消す。
- `pg_database` に無い `base/<oid>/` があれば WARNING を出す（D31）。
- REDO の振り分け（`recovery::dispatch`）に HEAP の `LOCK`、`HEAP2`、`BTREE` の `VACUUM`、`DBASE` を足す。REDO は値を書くだけで、ロックマネージャにも MultiXact にも触れない。`oldest_xid` 未満の XID の clog 設定は何もしない。
- クラッシュ後は、ロックマネージャも MultiXact の表も空。xmax にロック保持者の XID が残っていても、その XID は実行中でも Committed でもないので無効（アボートと同じ扱い）。
- サーバ（yuzhu-server）は `yuzhu_hba.conf` を読む。解析に失敗したら起動しない（AU）。

### 5.5 トランザクションを自分で開閉する文

`VACUUM`（`ANALYZE` オプション付きの `VACUUM ANALYZE` を含む）、`CREATE DATABASE`、`DROP DATABASE` はブロックの外（`DdlCtx.outside_block`。session が `execute_standalone` を呼ぶ前に検査し、違えば 25001）でなければならない。暗黙のトランザクションを session がまず開始してあるので、文の中で `TxnControl::commit_and_restart` によりそのトランザクションをコミットして新しいものを開く（VACUUM はテーブルごと）。**VACUUM を伴わない `ANALYZE` はブロックの中でも動き、外側のトランザクションの中で `ddl::execute` が実行する**（PG の `vacuum()` と同じ。03 VC-D15・C4。レビュー対応 R-29。初版の「ANALYZE もブロック内で 25001」を改めた）。`TRUNCATE`・`LOCK TABLE`・FK の DDL・ロールの DDL・`ALTER DATABASE` も通常のトランザクションの中で `ddl::execute` が動く。

---

## 6. M3 / M4 の契約からの変更（まとめ）

| 対象 | 変更 | 章 |
|---|---|---|
| `TxnManager::begin_write` / `WriterGuard` / `writer_owner` / `is_blocked_by` | 削除。`assign_xid` / `XidGuard` / `take_snapshot` / `oldest_xmin` を追加。`TxnManager::new` の引数が変わる | LK |
| `ProcArray` | `TxnManager` から切り出す。`StorageStack::new(vfs, cfg, wal, next_xid)` が作り、`stack.procs` に持つ。**`StackConfig` に `locks`・`multixact`・`oldest_xid` を足す**（R-18。01 §5.12） | LK |
| `WaitCtl` | `deadlock_timeout` を足す。`lock_timeout` の意味は「1 回のロック待ち」 | LK |
| `Transaction` | `writer` → `guard`、`isolation`、`xact_snapshot`、`owns_xid`（M4 の `wal_flush_upto`・`started_at` はそのまま） | LK、RW |
| `Snapshot` | `is_own()` を足し、可視性判定は `own_xid` を直接比べない（D2） | RW |
| `statement_barrier` | 文の実行は持たない。チェックポイントの書き出しだけが共有で持つ（D10） | LK |
| `Clog` | `status` / `set_status` をロックなしの読みと `compare_exchange` に（01 LK-D10）。`open(vfs, next_xid, oldest)`・`oldest()`（LK-3b）、`truncate_before`・`status` の `XX001`・`sweep_old_segments`（VC-4）。`set_status_redo` は `oldest_xid` 未満を無視 | LK、VC |
| `TableStore` | `delete` / `update` に `wait` と `crosscheck`。`lock_tuple` を追加。`HeapStore::new` の引数 | RW |
| `TableDef` / `RelHandle` | `key_columns` | RW |
| 可視性（`visibility.rs`） | `LOCK_ONLY` の xmax を無視、`XMIN_FROZEN` を「コミット済み・全員に見える」とする。`satisfies_update` が MultiXact を展開する | RW、VC |
| `RuntimeInfo` | `is_blocked_by` が `LockManager` に委ねる。`pg_isolation_test_session_is_blocked` はそのまま | LK |
| `Datum` | `Int4Array` と M4 の `int2[]` → `Array`（`ArrayValue` は `dims`）。`Time`・`Interval`・`Bytea`・`Uuid` を追加（numeric・date・timestamp・timestamptz・bpchar・int2vector は M4 のまま） | F0、TY、TD |
| `ResultSink` / `ColumnDesc` | `data_row_raw`、`format` | XQ |
| `Cluster::connect` | `lookup_role` と `connect(&ConnectRequest)` に分割。`ConnectGrant` | AU、DB |
| `ClusterOptions` | `autovacuum` など。`InitdbOptions` に `auth_host`・`pwfile` | VC、AU |
| `PhysicalPlan` / `LogicalPlan` / `BoundSelect` | `Update` / `Delete` に `recheck`（`returning` は M4 の欄）、`LockRows`、`VirtualScan`。論理プランに `LockRows`・`LRecheck`、`BoundSelect.locking`（§4.6） | RW、LK |
| `ExprKind` | `ExternParam(u16)` を足す（D45） | XQ |
| `IndexStore` | `insert` の戻り値が `InsertOutcome`、`bulk_delete` の追加（D44、D47） | RW、VC |
| `DdlCtx` / `BoundDdl` / `ddl::*` | `DdlCtx` に `backend`・`wait`・`session`・`role`・`outside_block`。`BoundDdl` に変種。`TxnControl`・`execute_standalone`（D41） | LK、VC、AU、DB、FK |
| `analyzer` | `analyze_with_params`（`analyze` は変えない） | XQ |
| M4 の `VACUUM` / `ANALYZE` / `TRUNCATE` | `VACUUM` / `ANALYZE` のスタブを本物に。`TRUNCATE` は M4 の実装に D42 を足す | VC |
| `ExecCtx` | `locks`、`wait`、`bind_params`、`xact_snapshot`、`ri` | RW、XQ、FK |
| 制御ファイル | `format_version` 3、`next_multi` | F0 |
| `session.rs` / `builtin.rs` | ディレクトリに分割 | F0 |
| `RmgrId` | `Heap2 = 6`、`Dbase = 7`（M4 の表は 0〜5 まで） | F0 |
| `TableStore` / `WAL` | `TableStore::inplace_update`、`HEAP2 INPLACE`（0x30）（03 C1・C2） | VC |
| `TxnManager::snapshot` | カタログ用スナップショットも `take_snapshot` で登録する。`snapshot()` は診断とテストだけ（R-02） | LK |
| `ConnectGrant` / `DatabaseRow` / `RoleRow` | `session_locks`、`DatabaseRow` の項目、`RoleRow` の `create_db`・`create_role`・`conn_limit`（R-37） | DB、AU |
| `ExecCtx` / `dml.rs` | `procs`、`update_with_indexes` / `delete_row` の `snap`・`old_row`・`crosscheck`（§4.6。R-05） | RW、FK |
| M3 の分離性 spec | 書き込みの直列化に依存する spec を書き直す（D39） | TS |
| `settings.rs` | §3.6 の項目。`TimeZone` などが SET で実際に効く | 各章 |
| `server_version` | 17.0 のまま（M2 の決定を再確認） | — |

### 6.3 各章からの変更依頼の取り込み台帳（レビュー対応 R-37）

各章の §11「契約への変更依頼」を 00 に取り込んだ結果。**✔ = 00 の本文を直した、→ = 持ち主の WP で実行する（00 は口だけ）、章 = 章の記述が正（00 は参照だけ）**。「R-nn」は 98-review-response.md の項目。

| 章 | 依頼 | 状態 |
|---|---|---|
| 01 LK | 1 `pg_name` の注、2 `LockStatusRow` の追加項目、9 `register_if` | ✔（§4.1・§4.2） |
| | 3 `StorageStack::new`、4 `TxnManager::new` と `MultiXactTable::new` | ✔ `StackConfig` に統一、`new` は `Result` なし（R-06、R-18。§4.4・§4.5） |
| | 5 カタログ用スナップショットの登録、6 `for_statement`、20 02 への依頼 | ✔（R-02。§3.1・§4.3・§5.1） |
| | 7 `Clog::set_status` | ✔ `compare_exchange`（§4.3） |
| | 8 持ち主の追記、12 `RelKind::Virtual`・OID 9811、16 `release_transaction_resources`、10 `blocking_pids` | ✔ §8.1、§3.2。`RelKind::Virtual` は F0b-1 |
| | 11 仮想リレーションへの DML | ✔ 55000（R-10。§4.10） |
| | 13 `fk_peer_tables` | ✔ 07 §4.2 に足した |
| | 14 準備済み文の `LockSet`、15 08・09 への依頼、17 03 の C3・C5・C6、18 04 の C7・C12、19 08 の C5・C6 | ✔ R-19、R-02、R-11 |
| | 21 10 章への依頼（KD-9、KD-11、XID の割り当て） | ✔ 10 §10.3 |
| 02 RW | 1 RR の最初のスナップショット、2 `FOR` の 25006、3 更新者の xmax、5 SQLSTATE と文言 | ✔（§3.7・§3.8・§5.1。R-03） |
| | 4 `LockTupleMode` の derive、`UpdateOutcome.lockmode`、`RowMark.table_name`、`ExecCtx.procs`、`InsertOutcome` | ✔（§4.5・§4.6。R-05） |
| | 6 `key_columns` | → F0b-1（欄）、RW-1b（`key_columns_from`） |
| | 7 `catalog/store.rs` の `wait` | → F1（§8.2） |
| | 8 `DebugKnobs` の 7 項目 | → F0b-1（R-09） |
| | 9 LK への依頼 | ✔ 01 が受け入れ済み |
| | 10 `txn_snapshot_taken` | → RW-4（R-03） |
| | 11 M4 の B+Tree | → RW-5（§8.1） |
| | 12 VC への依頼 | ✔ R-01（`set_prunable`、`invalidate_xmax`、`with_xmin_frozen`） |
| | 13（新）`MultiXactTable::new` の署名、14（新）`dml.rs` の署名 | ✔ R-06、R-05 |
| 03 VC | C1 `HEAP2 INPLACE`、C2 `inplace_update` | ✔（§3.3・§4.5） |
| | C3 カタログ用スナップショット | ✔ R-02 |
| | C4 ANALYZE のブロック内実行 | ✔ §5.5。宛先は RW（`txn_ctl.rs`。R-38） |
| | C5 `has_waiters`・`oldest_xmin_hint`・共有リレーションの `db = 0` | ✔ LK が受け入れ。`RunningXids` の `impl` は VC（R-11） |
| | C6 `Clog` の口、C13 起動 | ✔ §4.3・§5.4・R-18 |
| | C7 `DebugKnobs` | → F0b-1（6 項目。R-09） |
| | C8 持ち主、C10 設定 | ✔ §8.1、§3.6 |
| | C9 RW への依頼 | ✔ R-01 |
| | C11 `yz_datxid` の値の規則 | ✔ D51（R-08） |
| | C12 M2 / M3 の既存テスト | → VC-1（`page.rs`）、RW-1（REDO） |
| 04 XQ | C1 Bind の手順 | ✔ §5.1（R-03） |
| | C2 `ExecOutcome::CopyIn`、`copy_out_*` | ✔ §4.7 |
| | C3 `pending_parameter_status`、flush の規則 | ✔ §4.7・§4.12 |
| | C4 重複ポータルの文言 | ✔ §3.8（R-29） |
| | C5 バイナリの長さの過不足 | ✔ §4.9（R-04） |
| | C6 `ExecState`、C9 `ExternParam` の畳み込み | → F1（§8.2） |
| | C7 `lock_statement_relations` | ✔ 01 §4.6 が受け入れ |
| | C8 session の部品 | → F0b-2 |
| | C10 型ごとのバイナリ関数 | ✔ R-04 |
| | C11 WP の日数 | ✔ §7 |
| | C12 `registered_snapshot_count` | ✔ 01 §4.4 |
| | C13 `copy/*` | → XQ-6（§8 の例外） |
| | C14 D19、C15 `ExecCtx.procs` | ✔ R-15、R-05 |
| 05 TY | 1 `ExprKind` の変種、2 `FnKind::Env` | → F0b-2 |
| | 3・4・6 TY に許す編集と持ち主 | ✔ §8 |
| | 5 `yuzhu-auth` の `digest` | → AU-1（任意） |
| | 7 SQLSTATE | ✔ §3.5 |
| | 8 TY-5 の日数 | ✔ §7（6.5 日） |
| | 9 `int2_array` の集約 | → F1（M4 の `pg_constraint` を書く箇所） |
| | 10 `bytea_output` | ✔ §3.6 |
| | 11 バイナリ | ✔ R-04 |
| | 12 `ANY($1)` の推論 | → XQ-3 |
| | 13 カットライン | ✔ §1.3 |
| | 14 日時のバイナリ | ✔ 06 |
| | 15 numeric の sqrt、16 SELECT 句の SRF | ✔ D23、D25（R-34、R-28） |
| 06 TD | 1 持ち主、7 `ast.rs` | ✔ §8.1 |
| | 2 `ClusterOptions`（`clock`・`timezone`・`timezone_dir`）、3 `Session` の時刻、4 `Settings::new` の引数 | → F0b-1（ClusterOptions）、F0b-2（Session・Settings） |
| | 5・6 `analyzer/ddl.rs`・パーサ | → F1 |
| | 8・9 バイナリと `io.rs` | ✔ R-04 |
| | 10 CLI | 任意（AU・XQ の `config.rs`） |
| 07 FK | R1 `dml.rs` | ✔ R-05 |
| | R2 COPY の `finish_statement` | → F1 |
| | R3 `TableDef` / `RelHandle` / `CatalogReader` | → F0b-1（欄）、FK-1（中身） |
| | R4 `RiEnv` の session 実装 | → FK-2（口は F0b-2） |
| | R5 ロックの対象 | ✔ 01 §5.9 |
| | R6 DROP / TRUNCATE の連携 | → F1、VC-5（R-07） |
| | R7・R8 | → F1 |
| | R9 | → TS |
| | R10 `fk_skip_key_share` | → F0b-1 |
| 08 AU | C1 `store.rs` のアクセサ、C2 `mock_auth_nonce`、C3 `StartupParams.client_addr`、C7 `InitdbOptions` | → F0b-1・F0b-2 |
| | C4 `DdlCtx.password` | ✔ §4.7 |
| | C5 `check_catalog_select`、C6 OID | ✔ 01 が受け入れ、§3.2 |
| | C8 `Cargo.toml` | → F0a |
| | C9 所有者ロールのロック | → F1（CREATE TABLE）、DB-2（CREATE DATABASE。R-16） |
| | C10 `authentication_timeout` | → F0b-1 |
| | C11 検査の順序 | ✔ §4.8（R-21） |
| | C12 `tests/yuzhu.sh` | → TS-4 |
| | C13 `scram_skip_proof_check` | → F0b-1（R-09） |
| 09 DB | 1 検査順 | ✔ §4.8 |
| | 2 `InterruptFlag` を `Session::new` の前に | → AU-2（`connection.rs`） |
| | 3 D40 のポーリング | ✔ D40 |
| | 4 WP の依存 | ✔ §7 |
| | 5 `ALTER DATABASE` | ✔ §1.1、→ F0b-2（変種） |
| | 6・7 `ConnectGrant`、`DatabaseRow` | ✔ §4.8 |
| | 8 `yz_datxid` | ✔ R-08 |
| | 9 smgr / redo / dump の関数 | → F0b-2 |
| | 10 `DebugKnobs` | → F0b-1 |
| | 11 §3.8 の文言 | 章（09 §6.4 が正） |
| | 12 SQLSTATE | ✔ §3.5 |
| 10 TS | CR-1〜CR-13 | CR-3・CR-13 は ✔ §7・§1.3。CR-5 は R-09。ほかは 10 §11 |

---

## 7. 作業パッケージ（WP）

担当は章ごとに別のエージェントが並列に進める。各章の「実装の分担と工数」は、この表の ID と見積もりを詳しくして使う（ID は変えない。分けるときは `LK-3a` のように枝番を付ける）。工数は AI の実装エージェント 1 本の日数（粗い見積もり）。

| ID | 章 | 内容 | 依存 | 日数 |
|---|---|---|---|---|
| **F0a** | 00 | 基盤の機械的な部分（M4 のマージ後。D49）: `session/`・`catalog/builtin/` の分割、§2 の ★ ファイルをスタブで作り `mod` 宣言、`Cargo.toml`（path 依存）、CI が通ること | M4。最初に完了 | 2.5 |
| **F0b-1** | 00 | **LK・RW・VC が要る口**（F0 の「契約の口」の前半）: `DebugKnobs` の項目（各章の依頼分。LK 7・RW 7・VC 6・FK 1・AU 1・DB 4）、`TableDef.key_columns`・`RelHandle.key_columns`、`RelKind::Virtual`、`RmgrId::Heap2` / `Dbase` と `recovery::dispatch` の口、制御ファイル（`next_multi`・`mock_auth_nonce`・`format_version` 3・`catalog_version`、単調性の検査）、`StackConfig`（`locks`・`multixact`・`oldest_xid`。01 §5.12）、`ClusterOptions` の項目、SQLSTATE（§3.5）、`store.rs` のアクセサ（08 C1）、`yz_relxid` / `yz_datxid` の schema と initdb の行の口 | F0a | 1.5 |
| **F0b-2** | 00 | **XQ・TY・TD・FK・AU・DB が要る口**（後半）: `Datum` の変種、`Statement` / `BoundDdl` の変種（`AlterDatabase` を含む）、`ExprKind` の変種（`ExternParam`・配列の 4 変種）、`FnKind::Env`、`DdlCtx` の項目（`backend`・`wait`・`session`・`role`・`outside_block`・`password`）、`StartupParams.client_addr`、`InitdbOptions` と 3 か所の呼び出し側、`session` の `pub(super)` 関数 6 個と 1 行のフック（04 C8、03 の `modstat`）、`ddl/table.rs` の `insert_relxid` / `delete_relxid` の呼び出し、`ri::*` と `check_truncate_fks` のスタブ、`smgr` / `redo` / `dump` の関数（09 §11-9）、`testing.rs` の分割 | F0b-1 | 2.5 |
| **F1** | 00 | **M4 のファイルへの M5 の修正（M4 の担当が不在のため M5 が引き受ける。1 人が持つ）**: `catalog/store.rs` の書き込み系に `wait` と `expect_simple`（02 §11-7）、`ExecState` の `Default` と `take`（04 C6）、planner の `ExternParam` の畳み込み除外（04 C9）、`analyzer/ddl.rs` の `KNOWN_UNSUPPORTED_TYPES` と型修飾子（06 §11-5）、`sql/parser/{expr,misc}.rs` の日時の 1 行呼び出し（06 §11-6）、`ddl/{table,index,constraint,truncate}.rs` の FK のフック呼び出し（07 R6）、CREATE TABLE の所有者ロックの 1 行（08 C9）、COPY FROM の `finish_statement`（07 R2）。台帳は §8.2 | F0b-1 | 3 |
| LK-1 | 01 | `LockManager`（タグ、8 モード、FIFO と割り込み規則、Condvar、タイムアウト、セッションスコープ、`lock_status`） | F0b-1 | 3 |
| LK-2 | 01 | デッドロック検出（`deadlock_timeout`、待ちグラフ、DFS、DETAIL） | LK-1 | 1.5 |
| LK-3 | 01 | `ProcArray` の切り出し、`TxnManager` の複数ライター化（`assign_xid`、登録済みスナップショット、`oldest_xmin`、コミット / アボートの順序とロックの解放、clog のロックなし化（LK-3b。VC-4 の前）、バリアの縮小、`XidGuard`）、`BackendRegistry` | LK-1 | 4 |
| LK-4 | 01 | リレーションロックの取得手順（生のパース木からの一覧、解決→ロック→再解決、文ごとのモード、`LOCK TABLE`、名前予約ロック、DDL のモード） | LK-3、M4 | 3.5 |
| LK-5 | 01 | 観測: 仮想リレーションの仕組み、`pg_locks`、`pg_blocking_pids`、`pg_isolation_test_session_is_blocked` の置き換え | LK-1、F0b-1、M4 の planner | 1.5 |
| RW-1 | 02 | ヒープの更新競合。**枝番: RW-1a（`xmax.rs`・`invalidate_xmax`、1.5）・RW-1d（`HEAP_LOCK` の WAL、0.5）は F0b-1 だけに依存して先に進める。RW-1b（可視性、1）・RW-1c（`l1:`〜`l3:` ループ・`lock_tuple`、2）は LK-3・RW-2・F1 の後** | RW-1a・1d: F0b-1。RW-1b・1c: LK-3、RW-2、F1 | 5 |
| RW-2 | 02 | MultiXact（メモリ上。作成・展開・掃除、`next_multi`）。**F0b-1 だけに依存する**（RW-1c が RW-2 に依存する）。コミット / アボートからの `on_xact_end` の呼び出しは LK-3 の `manager.rs` の持ち物で、RW-2 は `MultiXactTable::on_xact_end` の実体だけを提供する | F0b-1 | 2.5 |
| RW-3 | 02 | 実行器: Update / Delete の EPQ（`RecheckSpec`、`LRecheck`）、`LockRows`、`FOR` 句の構文・解析・計画（枝番 RW-3a・3b は F0b-2・M4 だけに依存して先に進める。RW-3c は RW-1c の後）、NOWAIT / SKIP LOCKED | RW-1（3c）、F0b-2、M4 | 5 |
| RW-4 | 02 | Repeatable Read（スナップショットの保持、40001、`SET TRANSACTION` の規則、`transaction_isolation`、`statement_snapshot`） | RW-3、LK-3、LK-4 | 2 |
| RW-5 | 02 | B+Tree の複数ライター対応（M4 の単一ライター前提の洗い出しと修正、`IndexStore::insert` の `InsertOutcome`、`insert_with_indexes` の待ちのループ。D44） | RW-1、M4 | 3 |
| RW-6 | 02 | `INSERT / UPDATE / DELETE ... RETURNING` の仕上げ（M4 の欄の続き。D46） | M4、RW-3 | 1.5 |
| VC-1 | 03 | `satisfies_vacuum`、pruning、`pd_prune_xid`、機会的 pruning、HEAP2 の `PRUNE_FREEZE` の WAL と REDO | F0b-1 | 4 |
| VC-2 | 03 | FSM（形式、zero-on-error、`hio.rs` への組み込み、VACUUM での再構築） | VC-1 | 3 |
| VC-3 | 03 | VACUUM 本体（3 段階、TID 集合、B+Tree の `bulk_delete` と BTREE_VACUUM の WAL、`LP_UNUSED`、統計の更新、VERBOSE、`ANALYZE`）。**終了は RW-5 の終了より後（空の葉と一意検査のラッチの結合）** | VC-1、VC-2、LK-4、RW-5、M4 | 5 |
| VC-4 | 03 | 凍結、`yz_relxid` / `yz_datxid`、`relfrozenxid8` の進め方、clog の切り詰め（`oldest_xid`。LK-3b の後） | VC-3、LK-3、DB-1 | 4 |
| VC-5 | 03 | `TRUNCATE` への M5 の対応（D42。AccessExclusive、`yz_relxid` と FSM の作り直し、`check_truncate_fks` の呼び出し）、末尾の切り詰め | LK-4、VC-3 | 2 |
| VC-6 | 03 | 簡易 autovacuum（既定は無効） | VC-3、VC-5 | 2 |
| XQ-1 | 04 | サーバのメッセージループ（Parse〜Flush のメッセージ、`SkipTillSync`、flush の規則、COPY との関係） | F0b-2 | 2.5 |
| XQ-2 | 04 | Session の Extended API（文・ポータル、暗黙のトランザクション、Sync、行数制限と PortalSuspended、RETURNING の保持）。枝番 XQ-2a（2.5）・2b（2.5）。2b は RC の `statement_snapshot` で進め、RR は RW-4 の後に XQ-5 が結合 | XQ-1、M4 | 5 |
| XQ-3 | 04 | パラメータ型推論（`$n`、アナライザ、42P18 / 42P08） | M4、F1（planner の `ExternParam`） | 3.5 |
| XQ-4 | 04 | バイナリ I/O の振り分け（`binary.rs`）、既存型（M1〜M3）の send / recv | F0b-2 | 1.5 |
| XQ-5 | 04 | 準備済み文（世代と再解析、Bind でのロック）、`PREPARE` / `EXECUTE` / `DEALLOCATE` / `DISCARD`、RR の結合（RW-4） | XQ-2、LK-4（RR の結合 0.5 日だけ RW-4 の後） | 3 |
| XQ-6 | 04 | `COPY ... TO STDOUT`、CSV 形式、Extended 経由の COPY（任意）。`copy/*`（M4）の M5 の変更はこの WP が持つ | XQ-2、M4 | 3 |
| XQ-7 | 04 | ドライバの検証（tokio-postgres、psycopg 3、pgJDBC、node-postgres） | XQ-5、RW-4 | 3 |
| TY-1 | 05 | 型の枠組み（`Datum` の実体、`io.rs` の振り分け、`pg_type` の send / recv 列と `pg_proc` の行、キャスト表） | F0b-2 | 3 |
| TY-2 | 05 | numeric の穴埋め | TY-1、M4 | 1.5 |
| TY-3 | 05 | char(n) / bpchar の穴埋め | TY-1、M4 | 0.5 |
| TY-4 | 05 | bytea、uuid、`gen_random_uuid()` | TY-1 | 2 |
| TY-5 | 05 | 配列（ディスク形式、入出力、`ARRAY[]`、`ANY`、`::T[]`、関数、`ARRAY(SELECT)`、添字）、**TY-5c: SELECT 句の SRF の最小対応（TY-D17。+0.5）** | TY-1 | 6.5 |
| TY-6 | 05 | 上の型のバイナリ形式 | TY-1、XQ-4、TY-4、TY-5 | 2 |
| TY-7 | 05 | PostgreSQL との差分コーパス試験 | TY-2〜TY-5 | 2 |
| TD-1 | 06 | `interval` の統合 | F0b-2、M4、F1 | 2.5 |
| TD-2 | 06 | `TimeZone` / `DateStyle` / `IntervalStyle` の設定、`ZoneDb`、ParameterStatus、`AT TIME ZONE` | TD-1 | 2.5 |
| TD-3 | 06 | 演算子・関数（`now()` 系と `Clock`、`extract` / `date_part` / `date_trunc`、`age`、`make_*`、`justify_*`） | TD-1 | 4 |
| TD-4 | 06 | `time` 型（クレートへの追加と統合） | TD-1 | 2.5 |
| TD-5 | 06 | バイナリ形式、`DEFAULT 'now'` の扱い、差分コーパス試験 | TD-1、XQ-4 | 2 |
| FK-1 | 07 | DDL（構文、解析、`pg_constraint`（contype `f`）の行と `conkey` などの `Array`、`pg_depend`、名前、エラー） | TY-5（`ArrayValue` と `encode_array` を先に受け取る）、F0b-2、M4 | 4 |
| FK-2 | 07 | RI エンジン（イベントキュー、検査、5 つのアクション、MATCH、省略条件） | FK-1、RW-3 | 6 |
| FK-3 | 07 | 並行性（`FOR KEY SHARE`、RR の crosscheck と 40001） | FK-2、RW-4 | 2 |
| FK-4 | 07 | `ALTER TABLE ADD / DROP CONSTRAINT`、既存行の検査、DROP / TRUNCATE との連携 | FK-2、VC-5、F1 | 3 |
| FK-5 | 07 | テスト | FK-2 | 2 |
| AU-1 | 08 | `yuzhu-auth`（SCRAM、SASLprep、乱数）。**D49: 新規クレートだけなので M4 の実装中に始めてよい** | なし（F0a と並行） | 2.5 |
| AU-2 | 08 | サーバの認証フロー、`yuzhu_hba.conf`、`authentication_timeout`、initdb のオプション | AU-1、F0b-2 | 3 |
| AU-3 | 08 | ロールの DDL、`pg_authid` の書き込み、属性の検査、`pg_roles`、`Cluster::lookup_role` | AU-1、LK-5、F0b-2 | 3.5 |
| DB-1 | 09 | データベースの登録簿、`Database` ロック、接続数の制限、`Cluster::connect` の検査 | LK-1、LK-3 のうち `BackendRegistry`、F0b-1 | 2 |
| DB-2 | 09 | `CREATE DATABASE`（手順、DBASE の WAL と REDO、失敗時の後始末） | DB-1、LK-3（D10）、AU-3（`RoleRow`・`lock_role_shared`）、F0b-2 | 4 |
| DB-3 | 09 | `DROP DATABASE`、`ALTER DATABASE`（+0.5） | DB-1、DB-2 | 2.5 |
| DB-4 | 09 | クラッシュ試験 | DB-2、DB-3、TS-3a | 2 |
| TS-1 | 10 | 分離性ランナーの拡張と spec の移植・書き直し（TS-1a 2.5・1b 2.5） | LK-5、RW-3（1b）。1a は PostgreSQL だけ | 5 |
| TS-2 | 10 | 並行ストレス（TS-2a 1.5: ハーネスは LK-3 の後、S1（RC）は RW-3・RW-5 の後。TS-2b 2: RR、S2〜S7） | LK-3、RW-3、RW-5（2a）。RW-4、VC-3、DB-2（2b） | 3.5 |
| TS-3 | 10 | M5 のクラッシュ試験（VACUUM、MultiXact、FK の CASCADE、CREATE DATABASE。3a〜3e） | LK-5、VC-3、RW-2、DB-2、FK-2 | 4 |
| TS-4 | 10 | 互換・ドライバの CI ジョブ、SCRAM 構成のジョブ（4a 2.5・4b 1.5） | XQ-7、AU-2（4b）。4a は PostgreSQL だけ | 4 |
| TS-5 | 10 | `tests/slt/m5/` の共有テスト（5a 1.5・5b 3.5） | 各章 | 5 |

合計は約 **172.5 日**（M4 の契約を読む前の見積もり 165 日から、M4 が numeric・char(n)・日時・TRUNCATE・RETURNING の欄を先に作る分を引いた初版 158 日に、レビュー対応で次を足した: F0 の拡大 +4（2.5 → 6.5。F0a 2.5 + F0b-1 1.5 + F0b-2 2.5）、F1（M4 のファイルへの修正）+3、XQ-2 +1（04 C11）、TY-5 +1.5（05 §8。TY-5c の 0.5 を含む）、DB-3 +0.5（ALTER DATABASE）、TS +4.5（10 §8.1）。レビュー対応 R-20）。クリティカルパスは §1.3 と 10 章 §8.3。


---

## 8. 章の間の境界とファイルの持ち主

各担当は**自分の範囲のファイルだけ**を編集する。他の章のファイルを直す必要が出たら、自分では直さず、持ち主の章に依頼する（M1〜M3 と同じ）。**例外**は次のとおり（レビュー対応 R-36・R-38 で整理）:

- `error.rs` の `sqlstate` への定数の追記と、`settings.rs` への自分の設定の追記。
- **M4 のファイルへの修正**は、M4 の担当が F0 の開始時には存在しないので、**F1（M5 の WP。§7）が 1 人でまとめて行う**。各章は修正を「契約への変更依頼」に書き、F1 が §8.1 の台帳に従って実行する（衝突を避けるため、M5 の章の担当は M4 のファイルを直接編集しない）。`analyzer/ddl.rs` と `catalog/store.rs` は特に、自分の変種・自分のカタログの行を `analyzer/ddl_ext/<名前>.rs`・`catalog/store_<名前>.rs`（`impl CatalogStore` を別ファイルに書く）に置く。`analyzer/ddl.rs` の振り分けの 1 行（`ddl_ext::xxx::analyze` の呼び出し）と `store.rs` の private なフィールド・関数のアクセサ（08 C1）は F0b-1 が足す。
- **TY に許す小さな編集**（05 §11-3・§11-4）: `storage/heap/tuple.rs` の `Kind` に `Bytea` / `Uuid` / `Array` の分岐（各 1 型 1 分岐）、`analyzer/{resolve,coerce,expr}.rs` の配列の分岐（式の振り分けの 4 行、`is_polymorphic` の置き換え）、`sql/ast.rs` の `Expr` の配列の変種。
- **`copy/*`（M4）の M5 の変更は XQ-6 が持つ**（`copy/{to,csv}.rs` は新規、`copy/{mod,from}.rs` の CSV の分岐と `CopyOrigin`。04 C13）。COPY FROM の `ri::finish_statement` の呼び出し（07 R2）は F1。

### 8.1 持ち主の表

| ファイル / 領域 | 持ち主 | 備考 |
|---|---|---|
| `session/mod.rs`、`session/simple.rs`、`sql/ast.rs` の `Statement`、`types/datum.rs` の変種、`recovery.rs` の振り分け、`bootstrap.rs` の口 | F0（F0a が作り、F0b-1・F0b-2 が口を足す）。以後は該当の章が自分の行だけ足す | 衝突しやすい 5 か所。足すのは 1 つの変種 / 1 行ずつ |
| `session/locking.rs`、`txn/*`（`multixact.rs` を除く）、`backend.rs`、`ddl/lock_table.rs`、`analyzer/ddl_ext/lock_table.rs`、`catalog/virtual_rel.rs`、`executor/virtual_scan.rs`、`txn/clog.rs` の `open(vfs, next_xid, oldest)` の署名・`oldest()`・ロックなし化（LK-3b）、`engine.rs` の `Cluster::prepare` の組み立て（`locks`・`backends`・`multixact`。01 §5.12） | LK | `txn/clog.rs` は **LK-3b（ロックなし化・`open` の署名）→ VC-4（切り詰め）の順**。`session/txn_ctl.rs` は RW の持ち物で、LK は `release_transaction_resources()` の 1 行だけ足す（01 §11-16） |
| `txn/multixact.rs`、`storage/heap/{lock,xmax,visibility,mod,wal}.rs`、`executor/dml.rs`、`executor/nodes/{update,delete,lock_rows,insert,index_scan}.rs`、`planner` の `RecheckSpec`・`LRecheck`・`LockRows`、FOR 句の構文・解析（`analyzer/{select,dml,bound,locking}.rs`）、**`session/txn_ctl.rs`**（分離レベル、`TxnControl` の実装、ANALYZE のブロック内実行（03 C4）、`statement_snapshot`）、`storage/btree/{insert,unique,search,split}.rs` の複数ライター対応の修正と `IndexStore::insert` の `InsertOutcome` | RW | `visibility.rs` の `XMIN_FROZEN` の分岐と、`invalidate_xmax` / `with_xmin_frozen`（02 §4.1）は VC が RW に依頼する。`storage/btree/` の M4 のファイルの修正は RW-5 の範囲だけ（M4 の B+Tree の持ち主は M4 の B1・B2 で、M5 では RW が引き継ぐ）。`executor/dml.rs` の 3 関数の署名は §4.6（FK が `ri::*` のフックを受け取る形） |
| `storage/fsm.rs`、`storage/heap/{prune,vacuum,wal2,hio,inplace,modstat}.rs`、`storage/page.rs`、`storage/btree/vacuum.rs`、`vacuum/*`、`ddl/{vacuum,truncate}.rs`、`analyzer/ddl_ext/vacuum.rs`、`sql/parser/vacuum.rs`、`catalog/store_vac.rs`（`yz_relxid` / `yz_datxid` の読み書き。09 の CREATE / DROP DATABASE はここの `insert_datxid` / `delete_datxid` を呼ぶ）、`catalog/schema.rs` の `yz_relxid` / `yz_datxid` の定義と `catalog/rows.rs` の initdb の行の中身（F0b-1 が口を作り、VC-4 が 03 §3.4 の値の規則で書く）、`txn/clog.rs` の `truncate_before`・`status` の `XX001`・`sweep_old_segments`・`set_status_redo`（03 C6。LK-3b の後）、`yuzhu-server/src/config.rs` の autovacuum の 5 キー | VC | `ddl/truncate.rs` は VC が持つ。FK（07）は `check_truncate_fks` を提供するだけで `truncate.rs` を直接編集しない（VC-5 が呼ぶ） |
| `session/extended.rs`、`session/prepared.rs`、`analyzer/params.rs`、`types/binary.rs`、`sql/lexer.rs` の `$n`、`expr/mod.rs` の `ExternParam`、yuzhu-server の `connection.rs`・`protocol/*` のメッセージループ、`copy/*` の M5 の変更 | XQ | AU-2 は `connection.rs` の起動処理（認証）だけを足す |
| `types/{bytea,uuid,array,array_ops,typeinfo,md5,wire,typmod}.rs`、`types/{numeric,bpchar}.rs` の M5 の追加分（send / recv と穴埋め）、`types/{io,mod,hash}.rs`、`catalog/builtin/{numeric,text_misc,array,type_io}.rs`、`catalog/opclass.rs` の bytea・uuid の行、`analyzer/{polymorphic,array_expr,srf_rewrite}.rs`、`sql/parser/array.rs`、上の「TY に許す小さな編集」 | TY | M4 が作ったファイルは M4 の T1・T2・T3 の後継として TY が引き継ぐ。`types/typmod.rs` は M4 の T2 から TY へ |
| `types/{datetime,interval,time}.rs`、`catalog/builtin/datetime.rs`、`catalog/opclass.rs` の time・interval の行、`yuzhu-datetime/*`、`util/clock.rs`、`sql/parser/datetime.rs`、`settings.rs` の日時の設定 | TD | `analyzer/ddl.rs`・`sql/parser/{expr,misc}.rs` の 1 行の呼び出しは F1（06 §11-5・§11-6） |
| `executor/ri/*`、`sql/parser/fk.rs`、`analyzer/ddl_ext/foreign_key.rs`、`ddl/constraint.rs` の FK の部分、`catalog/{fk,store_fk}.rs`（`CatalogStore` の FK の行の読み書き。`store.rs` には足さない） | FK | `ddl/{table,index}.rs`・`ddl/constraint.rs` の PK / UNIQUE の部分への FK の呼び出し（07 R6）は F1。`executor/dml.rs` の `ri::after_*` の呼び出しは RW（§4.6） |
| `yuzhu-auth/*`、`yuzhu-server/src/{auth,hba}.rs`、`ddl/role.rs`、`analyzer/ddl_ext/role.rs`、`catalog/roles.rs`（ロールの存在・LOGIN・接続数の検査の関数を含む）、pg_roles、`engine.rs` の `Cluster::lookup_role` | AU | `Cluster::connect` の本体は DB（09 DB-1）が置き換える。AU は検査の関数（08 §5.1 手順 9 の a・b・e）を提供するだけで `connect` を編集しない |
| `dbase.rs`、`ddl/database.rs`、`analyzer/ddl_ext/database.rs`、`sql/parser/database.rs`、`catalog/store_db.rs`、`engine.rs` の `Cluster::connect`・`database_handle`・`forget_database`・孤児検査、`datadir.rs` のコピー | DB | `engine.rs` の `locks`・`backends` の持たせ方は LK。`smgr.rs`・`wal/redo.rs`・`wal/dump.rs` の関数（09 §11-9）は F0b-2 |
| `tests/*` の共通部品（分離性ランナー、`tests/compat/`、CI）、`yuzhu-core/src/testing.rs` の分割後の部品 | TS（`testing.rs` の分割は F0b-2） | 各章は自分の機能のテストファイル（slt・spec・Rust テスト）を書く |

### 8.2 M4 のファイルへの修正の台帳（F1 が実行する。R-36）

| M4 のファイル | 修正 | 依頼元 | 必要な WP |
|---|---|---|---|
| `catalog/store.rs` | 書き込み系の関数に `wait: &WaitCtx<'_>` を足し、`expect_deleted` を `TmResult::expect_simple` に置き換える | 02 §11-7（RW-D16） | RW-1c |
| `executor/mod.rs`（`ExecCtx` の状態） | `SubPlanStates: Default`、`CteStates: Default`、`MemBudget` を `take` / `replace` できる形、`ExecState` の束ね | 04 C6 | XQ-2b |
| `planner/{physicalize,rules}.rs`、`index_select` | `ExprKind::ExternParam` を「実行時に 1 回評価できる式」に含め、畳み込まない | 04 C9 | XQ-3 |
| `analyzer/ddl.rs` | `KNOWN_UNSUPPORTED_TYPES` から `time`・`interval` を外す、型修飾子の解決で `typmod_in`、`format_type` の `typmod_out`、DEFAULT の `fold_time_dependent_literals` | 06 §11-5 | TD-1 |
| `sql/parser/{expr,misc}.rs` | `AT` の枝、`INTERVAL` の枝、`SET TIME ZONE` の `INTERVAL` の枝を `sql/parser/datetime.rs` の関数への 1 行の呼び出しに置き換える | 06 §11-6 | TD-1、TD-2 |
| `ddl/{table,index}.rs`、`ddl/constraint.rs`（PK / UNIQUE の部分） | DROP TABLE の `fks_blocking_drop_tables`、DROP INDEX と PK / UNIQUE の DROP CONSTRAINT の `fks_depending_on_index` の呼び出し（2BP01 の DETAIL）。TRUNCATE は VC-5 が `check_truncate_fks` を呼ぶ | 07 R6 | FK-4 |
| `ddl/table.rs`（CREATE TABLE） | 所有者ロールの `lock_role_shared`（AccessShare）、`insert_relxid` / `delete_relxid` の呼び出し（後者は F0b-2 が済ませる） | 08 C9、03 C8 | AU-3、VC-4 |
| `copy/from.rs` | CopyDone の処理の終わりで `ri::finish_statement` を呼ぶ | 07 R2 | FK-2 |
| `catalog/{mod,reader}.rs`、`storage/mod.rs`（`RelHandle`） | `TableDef.foreign_keys` / `referenced_by`、`RelHandle.ri`、`CatalogReader` の FK の 3 メソッド（既定実装つき）。**型の欄は F0b-1 が足し、中身は FK-1** | 07 R3 | FK-1 |
| `deparse/`（`pg_get_constraintdef`）、`catalog/opclass.rs`（`integer_ops` のクロスタイプ） | contype `f` の分岐、`AMOPS` の行 | 07 R7・R8 | FK-1 |

F1 は 3 日で、M4 のマージ後に F0b-1 の完了を待って始める。各行は「必要な WP」の開始までに済ませる。M4 の担当がマージ後も残っていて、同じ修正をすでに済ませていれば、その行は不要になる。

---

## 9. 章の書き方

各章は M3 の設計書（`spec/design/m3.md`）の構成と書式に倣う。

```
# yuzhu M5 基本設計 NN: <主題>
（冒頭: ゴール、前提、参照する調査と設計、この章の略号）
## 0. 決定（この章で扱う論点。調査間の食い違いも）   表: # / 論点 / 選択肢（出典） / 決定 / 理由。ID は <略号>-D<n>
## 1. 範囲                                            対応するもの、しないもの（0A000 を返すもの）、保証すること
## 2. 構成                                            モジュールの木（★ △ ✕）、依存、章の規約
## 3. ディスク上の形式                                 ある章だけ。バイト単位の表と例
## 4. 共通の型（契約）                                 Rust のシグネチャ。00 の契約と食い違わない
## 5. 処理の流れ                                      手順（疑似コード）、ロックの順序、WAL と REDO
## 6. モジュールごとの仕様
## 7. テスト                                          共有テスト（slt / spec）、Rust のテスト、クラッシュ試験
## 8. 実装の分担と工数                                 §7 の WP の ID を使う。ファイルの持ち主、依存、見積もり
## 9. 未検証の点                                      実装前に確かめること（出典つき）
## 10. 確認事項                                       仮決め・理由・変えたい場合の影響。ID は M5-<略号>-Q<n>
## 11. 契約への変更依頼                                 00 と食い違う必要があるとき（無ければ「なし」）
```

- 根拠の記号: 【確認】ソースや実機で確かめた、【記憶】未照合、【提案】yuzhu への推奨。PostgreSQL のソースは `PG:<path>`。
- バイト形式は表で書き、**例を 1 つ以上**（実装者がそのまま単体テストの固定値にできる形）付ける。
- 「PostgreSQL と違う」点は、その章の確認事項と、`10-tests-plan.md` が集める「既知の差」に挙げる。共有テストには差が出るケースを入れない。
- 調査の推奨と食い違った箇所、M3 / M4 の設計と食い違った箇所は、決定表に出典つきで書く。

---

## 10. 契約レベルの確認事項と未検証

### 10.1 確認事項（ユーザーに確認したい主なもの）

仮決めのまま進める。各章の確認事項（`M5-<略号>-Q<n>`）は 10 章が集める。

| ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|
| M5-Q1 | SAVEPOINT は M5 に入れない（M6 の先頭候補。D2） | L の工数で M5 全体が崩れる | SQLAlchemy + psycopg 3 は接続時に SAVEPOINT を使うので、M6 まで繋がらない可能性がある（pg-compat §3.4。失敗時の挙動は未検証）。M5 に入れるなら +15〜25 日。契約は `is_own` / `owns_xid` で拡張点を残してある |
| M5-Q2 | RETURNING は M4 が欄を用意し、M5 が仕上げる（RW-6、+1.5 日。D46）。COPY TO / CSV も任意で追加（XQ-6、+3 日） | ORM が毎回使う。M4 は「後半・任意」 | M4 で完成していれば RW-6 は不要。入れないと Rails / SQLAlchemy / Prisma の INSERT が動かない |
| M5-Q3 | `INSERT ... ON CONFLICT` は M6 | L。行ロックと一意検査の待ち（M5）が前提 | M5 に入れると +10 日以上 |
| M5-Q4 | MultiXact はメモリ上だけ（Q-014）。FK の子の INSERT と親の非キー列 UPDATE が衝突し、デッドロックも PostgreSQL より増えうる（D6） | 永続化と WAL と切り詰めを避ける | 永続版（案 C）は +L（SLRU、WAL、VACUUM での切り詰め） |
| M5-Q5 | ヒントビットは M5 でも入れない。clog の読みをロックなしにして代える（D13） | FPI が要る | 入れるなら `XLOG_FPI_FOR_HINT` と `page_mut_hint` の運用が増える（+M） |
| M5-Q6 | `relfrozenxid` の 64 ビットは別カタログ `yz_relxid`（D14）。clog の切り詰めは、各データベースで全テーブルの VACUUM が一巡しないと進まない（template1 を含む） | 共有テストが `xid` 型を見ている | `xid8` 型の列にすると共有テスト（`catalog_columns.slt`）を直す必要がある |
| M5-Q7 | autovacuum は実装するが既定は無効（D17） | 並行試験の再現性 | 有効にすると膨張は自動で止まるが、試験の揺らぎの原因になりうる |
| M5-Q8 | 配列の列はユーザーテーブルで `0A000`（D25）。ディスク形式は PostgreSQL と同じ。bytea・uuid・time・interval の B+Tree の opclass は M5 で足す | M5 の範囲。`uuid PRIMARY KEY` は実用上必須 | 配列の列を許すなら比較・ハッシュ・B+Tree の opclass が要る（+M） |
| M5-Q9 | `SERIALIZABLE` は `0A000`（D3） | SSI は L 以上 | PostgreSQL の既定が RC で、SERIALIZABLE を明示するアプリだけが影響を受ける |
| M5-Q10 | 孤児の `base/<oid>/` は消さず WARNING だけ（D31） | M3 D15 と同じ方針 | 容量が残る |
| M5-Q11 | DateStyle / IntervalStyle はクレートが実装する全形式を受け付ける（D24） | 追加の工数がない | — |
| M5-Q12 | M3 の分離性 spec（書き込みの直列化に依存するもの）を書き直す（D39） | M5 で意味が変わる | M3 の期待ファイルは廃止 |
| M5-Q13 | DDL の置き場は M4 の `ddl/`（D41）。M5 は `commands/` を作らない。`BoundDdl` の変種と `DdlCtx` の項目を M5 が足す | 入口を 1 つにする | M5 だけ別の入口にすると、トランザクションの開閉・ロック取得・analyze の経路が 2 通りになる |
| M5-Q14 | M5 の実装は M4 のマージ後に始める（F0 の分割が M4 の触るファイルを動かすため。D49）。新規クレート・新規ファイルだけの作業は先行してよい | 衝突を避ける | 並行で始めると F0 の分割が M4 の差分とぶつかる（手戻りは機械的だが大きい） |
| M5-Q15 | M4 の `int2[]`（`Int2Vector`、yuzhu 独自の符号化）を M5 で `Array` に置き換える（D25）。カタログの `int2[]` / `oid[]` 列のディスク形式が変わる | PostgreSQL の `ArrayType` に一本化する | 置き換えないと、配列が 2 系統になる（`Int2Vector` 経由の列と `Array`）。`catalog_version` を上げるので M4 のデータディレクトリは使えなくなる（M2-Q20 のとおり許容済み） |
| M5-Q16 | `IndexStore::insert` の戻り値を `InsertOutcome` に変える（D44）。M4 の署名（`Result<()>`）から変わる | B+Tree が `LockManager` を知らずに一意検査の待ちを返せる | 戻り値を変えずに、B+Tree が `LockManager` と `WaitCtx` を受け取って内部で待つ案もある。ラッチを外してから待つ処理が B+Tree の内部に入り、`storage::btree` が `txn::lock` に依存する |

### 10.2 未検証の点（契約レベル）

- M4 の契約（`spec/design/m4/00-contracts.md`）との突き合わせは §1.3 の A1〜A12 で済ませた。M4 の章（01〜11）はまだ並行して書かれているので、特に `06-btree.md`（複数ライター前提、`UniqueCheck`）、`04-planner-optimizer.md`（`LockRows` を置ける位置、`Update` の入力の形）、`07-catalog-ddl.md`（`ddl/` の構成、`pg_constraint` の列）、`09-types-functions.md`（numeric・日時の範囲）が確定したら、各章の担当は最初に突き合わせ直す。
- `ProcArray` を切り出すと M2 / M3 の `TxnManager` の単体テストが多く書き直しになること（LK-3 の見積もりに含めた）。
- 更新者の xmax に PostgreSQL が `EXCL_LOCK` ビットを立てるか（§3.7）。
- ~~`scram_iterations` が PG17 の ParameterStatus の報告対象か。~~ **確認済み**（【実機】PG17.11 の起動時の ParameterStatus に含まれる。R-14）。
- 型 OID・関数 OID の値は各章が .dat で確かめる（§3.2）。
- 文の実行が `statement_barrier` を持たなくなることで、`assert_no_pins` やチェックポイントのバッファ書き出しの前提が崩れないか（LK-3 で確認）。
- カタログ用スナップショットを RR でも最新にする（D12）と、`StatementCatalog` のキャッシュの世代（M2 §6.8.6）の前提が崩れないか。
