# yuzhu M5 基本設計 09: CREATE DATABASE・DROP DATABASE・接続とデータベースの管理

M5 のゴールの 1 つは「複数のデータベースを持つ」ことです。この章は、(1) 接続のたびに行う検査と、データベースごとのセッションスコープのロック、(2) `CREATE DATABASE`（FILE_COPY 相当）、(3) `DBASE` rmgr（WAL のレコードと REDO）、(4) `DROP DATABASE`、(5) `pg_database` と `yz_datxid` の行の扱い、(6) クラッシュ試験の観点を、実装者がこの章だけ読めば実装できる粒度で決めます。

- 契約: `spec/design/m5/00-contracts.md`（以下「00」）。この章はそれに従う。従えない点は §11 に書く。決定 D30（CREATE DATABASE）、D31（孤児ディレクトリ）、D32（DROP DATABASE の順序）、D40（`Database(oid)` ロック）、D51（`yz_datxid`）、WAL は 00 §3.3（DBASE rmgr）、型は 00 §4.8。
- 前提の設計: `spec/design/m2.md` §6.9（接続・OID・`copy_database_dir`）、`m3.md`（WAL・チェックポイント・リカバリ・クラッシュ試験）、`spec/design/m4/00-contracts.md`（`DdlCtx`・`ddl/`）。
- 調査: `spec/research/m5-protocol-auth.md` §7（CREATE / DROP DATABASE と別 DB への接続）。**この章は、その調査の「【記憶】」を PostgreSQL 17.11 の実機（`sandbox/pg.sh start`。C ロケール、trust）で確かめ直した**。食い違った点は §0 に出典つきで書いた。
- 略号: **DB**。作業パッケージは DB-1〜DB-4（§8）。確認事項は `M5-DB-Q<n>`、決定は `DB-D<n>`。
- 根拠の記号: 【確認】PostgreSQL 17.11 の実機で確かめた（本章の作業中。日付 2026-10-05）、【記憶】未照合、【提案】yuzhu への推奨。`PG:<path>` は REL_17_STABLE のソース（本章では実機の `LOCATION:` 行で関数名と行を確かめた箇所だけを引く）。
- 関連する他の章との境界: ロックマネージャ・`BackendRegistry` は 01 章（LK）、ロール（`pg_authid`、`CREATE ROLE`、`rolconnlimit` の列）と認証は 08 章（AU）、`yz_datxid` の値を進める処理・VACUUM・clog の切り詰めは 03 章（VC）、Extended Query は 04 章（XQ）。

---

## 0. 決定（この章で扱う論点。調査間・既存設計との食い違いも）

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| DB-D1 | `Cluster::connect` の検査順 | (a) M2 §6.9.1 の順（DB → `datallowconn` → ロール）。00 §4.8 の列挙「3D000 / 55000 / 28000 / 53300」／(b) PostgreSQL 17 の順 | **(b)。ロール（28000）→ ロールの接続数（53300）→ データベースの存在（3D000）→ `Database(oid)` ロックの取得（CREATE / DROP DATABASE の完了を待つ）→ 行の取り直し（3D000）→ `datallowconn`（55000）→ データベースの接続数（53300）**。00 §4.8 の列挙は SQLSTATE の一覧であって検査順ではないと解釈する | 【確認】実機で、存在しないロールと存在しない DB の組は `role "nobody" does not exist`、ロールの接続数超過は存在しない DB でも `too many connections for role`、`template0` は superuser でも 55000。M2 の順は PostgreSQL と逆で、複数の条件が重なったときだけ結果が変わる |
| DB-D2 | ロールの存在・ログイン可否の検査を `connect` でも行うか | AU の `lookup_role`（認証の前）だけに任せる／`connect` でも行う | **`connect` でも行う（28000 の 2 種）** | `trust` ではロールを認証で確かめないので、存在しないロールの 28000 はここでしか出ない。認証と接続の間にロールが消える・NOLOGIN になる競合（TOCTOU）にも効く |
| DB-D3 | 接続数の数え方 | 登録の前に数える／**自分を先に登録してから「自分を含めた数 > 上限」で拒否**（PG の `CountUserBackends` / `CountDBBackends`） | **後者**。superuser は両方の上限から除外する | 【確認】`CONNECTION LIMIT 1` の DB は 1 本目が入り 2 本目が拒否される。`CONNECTION LIMIT 0` は superuser 以外を全員拒否。同時に 2 本が来て両方が拒否される過剰拒否は PostgreSQL にもある |
| DB-D4 | 「他のセッションが居ない」の判定と、新しい接続を待たせる仕組み | D40: 各セッションが `Database(oid)` を AccessShare でセッションスコープに持つ。CREATE / DROP は `try_acquire(AccessExclusive)` | **D40 のとおり。接続側は `acquire`（待つ）で AccessShare を取り、取れたら `pg_database` の行を取り直す（待っている間に DROP されていれば 3D000）** | 1 つのロックで「新しい接続を止める」と「他の接続があるか」の両方を表せる |
| DB-D5 | CREATE / DROP が他のセッションの退出を待つか | D40 の `try_acquire` を 1 回だけ（即失敗）／**100 ms 間隔で最長 5 秒繰り返す** | **後者（`ClusterOptions.database_busy_wait`。既定 5 秒。テストでは 300 ms など）。待っている間も `interrupts.check()`（キャンセル・`statement_timeout`・停止）を毎回行う** | 【確認】PostgreSQL は 5 秒待つ（`CountOtherDBBackends` の 50 回 × 100 ms）。実機では、別の接続が 6 秒のクエリを実行中に `CREATE DATABASE` を流すと 5 秒後に 55006、接続が切れた直後なら成功した。Django / Rails のテストランナーは「接続を閉じてすぐ DROP DATABASE」をするので、即失敗にするとサーバ側のセッション終了との競合で不安定になる。待ちは `acquire`（FIFO で待つ）ではなくポーリングにする: セッションスコープのロックを待つ `acquire` はデッドロック検出器に載り、「A が DB1 に居て DROP DB2、B が DB2 に居て DROP DB1」が 40P01 になる（PostgreSQL は 55006）。ポーリングなら待ち手が居ないので検出器に関与しない |
| DB-D6 | CREATE 中に持つロックとそのスコープ | トランザクションスコープ／**セッションスコープ** | **名前予約ロック（`Object { db: 0, class: 1262, obj }` Exclusive）、`Database(template)`、`Database(新 OID)` をすべて AccessExclusive / Exclusive の `LockScope::Session` で取り、文の最後（最後のチェックポイントの後）に RAII で解放する** | 文の途中で `commit_and_restart` が呼ばれ、トランザクションスコープのロックはそのコミットで解放される（00 §5.2）。最後のチェックポイントが終わるまでテンプレートの変更を止める必要がある（DB-D8 の REDO の正しさの前提） |
| DB-D7 | CREATE DATABASE の手順 | 調査 §7.1 と D30: チェックポイント → コピー → `DBASE_CREATE` → 行の挿入 → コミット → チェックポイント／PostgreSQL（最後のチェックポイントをコミットの前に置く【記憶】） | **D30 のとおり**。最後のチェックポイントの失敗は、コミット済みの DB が残るままエラーを返す（§5.3）。PostgreSQL と同じ「コミットの前」に移す案は確認事項 M5-DB-Q3 | D30 が決定済み。移すとチェックポイントの失敗でロールバックでき、PostgreSQL に近づく。影響は §10 |
| DB-D8 | `DBASE_CREATE` の REDO | 毎回消してコピーし直す（冪等）／コピーが済んでいれば何もしない | **毎回、`base/<dst>/` を消してから `base/<src>/` をコピーし直す。`src` が無ければ WARNING で飛ばす（起動を止めない）** | 調査 §7.1 の方針。REDO が走るのは「`DBASE_CREATE` の後・最後のチェックポイントの完了前のクラッシュ」だけで、その間テンプレートは DB-D6 のロックで変更されない。コピーは commit の前に fsync 済み（`sync = true`）なので、`src` が先に DROP されていても `dst` は無事 |
| DB-D9 | DROP DATABASE の順序（D32）と `DBASE_DROP` の `xid` | D32 のとおり／`DBASE_DROP` を行の削除と同じトランザクションの中に置く（PostgreSQL） | **D32 のとおり。行（`pg_database` と `yz_datxid`）の削除をコミット → バッファを捨てる → `DBASE_DROP` を WAL に入れて flush → ディレクトリを削除。`DBASE_DROP` の `xid` は 0（コミットの後の独立したレコード）** | `DBASE_DROP` をコミットの前に置くと、コミット前にクラッシュしたとき REDO が生きた DB のディレクトリを消す。コミットの後に置けば REDO が消すのは必ず「もう論理的に存在しない」DB |
| DB-D10 | オプションの受理範囲 | 調査 C-9 / C-11 / C-12 | **§6.2 の表。UTF8 と C ロケール（`C` / `POSIX` / 空）だけ受け付け、`STRATEGY` は 2 値とも FILE_COPY で実行、それ以外の既知のオプションは 0A000、未知は 42601、値の不正は 22023 / 42809 など PostgreSQL と同じ** | 「黙って無視しない」（00 §1.1） |
| DB-D11 | `pg_database` の列の拡張 | 00 §4.8 は「`DatabaseRow` に `conn_limit`・`owner` を足す」。調査は列の追加を示唆 | **列は増やさない。`pg_database` は M2 から PostgreSQL 17 と同じ 18 列（`datdba`・`datconnlimit`・`datfrozenxid` を含む。M2 §6.8 の表）を持つ。足すのは Rust の `DatabaseRow` の項目と、その読み書きのコードだけ** | 列を変えると `catalog_columns.slt` が壊れる |
| DB-D12 | `datfrozenxid` と `yz_datxid` の分担（D51） | DB が全部／VC が全部／分担 | **VC（03 §4.3 の `catalog/store_vac.rs`）: `yz_datxid` の読み書きの関数をすべて持つ（`insert_datxid` / `delete_datxid` / `datxid` / `min_datxid` / `set_datxid_inplace`。値を進めるのは HEAP2 INPLACE の上書き。VC は XID を持たないので MVCC の更新はできない）、値の規則（03 VC-D13）、全データベースの最小値の計算、clog の切り詰め。DB: `pg_database` の行の作成・削除と同じ `WriteCtx` で VC の `insert_datxid` / `delete_datxid` を呼ぶ（行の作成は initdb と CREATE DATABASE、削除は DROP DATABASE）。`store_db.rs` に `yz_datxid` の関数は作らない。値の規則: initdb は template1・postgres を 3、template0 を番兵 `DATFROZEN_PRISTINE`（`i64::MAX`）、CREATE DATABASE は新しい行を `min(テンプレートの行の値, TxnManager::oldest_xmin())`（03 C11）で作る** | D51 と 03 VC-D13 / C11 に統一した（レビュー対応 R-08。以前の「テンプレートの値をそのままコピー」と「MVCC の `set_datfrozenxid`」は、template0 の番兵を継承して clog の切り詰めの境界を実態より大きくする恐れがあった）。`yz_datxid` の関数の持ち主は 1 つ（VC）にする |
| DB-D13 | 孤児ディレクトリ（D31）の起動時の扱い | 消す（調査 C-10）／D31: 消さず WARNING | **D31。起動時に `base/` の数字だけの名前のディレクトリのうち `pg_database` に行の無いものを WARNING に出す。逆（行はあるがディレクトリが無い）も WARNING に出す（診断だけ。起動は止めない）** | M3 D15 と同じ方針。逆向きの検出は 1 行で済む |
| DB-D14 | `ALTER DATABASE` | M5 に入れない（00 §1.1 は触れていない）／**3 つのフラグのオプションだけ実装** | **`ALTER DATABASE name [WITH] { ALLOW_CONNECTIONS b \| CONNECTION LIMIT n \| IS_TEMPLATE b }...` だけ実装し、`RENAME` / `OWNER` / `SET` / `RESET` / `REFRESH COLLATION VERSION` / `SET TABLESPACE` は 0A000。DB-3 に +0.5 日** | `CREATE DATABASE ... IS_TEMPLATE true` を受け付けると、`ALTER DATABASE ... IS_TEMPLATE false` が無い限り二度と消せない（【確認】実機: `cannot drop a template database`）。罠を作らないための最小の追加。入れない場合は M5-DB-Q4 の代案（`IS_TEMPLATE true` を 0A000） |
| DB-D15 | `DROP DATABASE ... WITH (FORCE)` | 他の接続を切って実行（PG13 以降）／0A000 | **0A000**（00 §1.1。`pg_terminate_backend` と一緒に M6） | 接続の強制終了の仕組みが無い |
| DB-D16 | ブロック内の CREATE / DROP DATABASE の 25001 | 文の側で検査／session が `outside_block` を作る | **session が `DdlCtx.outside_block` で判定し、25001 を返す（00 §4.7）。複数の文を 1 つの Query に入れたときも 25001（PostgreSQL と同じ）** | 【確認】実機: `select 1; create database z4` も `create database z5; select 1` も 25001（暗黙のトランザクションブロックになるため）。BEGIN の中も同じ |
| DB-D17 | OID の採番 | `get_new_oid(PG_DATABASE)` だけ／**さらに `base/<oid>` が無いことを確かめる** | **後者**。`get_new_oid`（`SnapshotAny` 相当の走査。M2 §6.9.2）で行との重複を避け、`base/<oid>` が既にあれば（孤児）次の OID を取る | PostgreSQL の `check_db_file_conflict` と同じ。孤児ディレクトリ（D31）を残す以上必要 |
| DB-D18 | ファイルの破棄の置き場 | `dbase.rs` が `BufferPool` / `StorageManager` の内部に触る／持ち主の型に関数を足してもらう | **`BufferPool::drop_database_buffers`、`StorageManager::forget_database` / `vfs()`、`InvalidPages::forget_database` を持ち主に依頼する（§11）。`dbase.rs` はそれらと `Vfs` だけを使う** | 型の内部（フレーム表、開いているファイルの表）は持ち主のもの |

**調査の【記憶】との食い違い**（実機で確かめた結果）:

| 調査 §7 の記述 | 実機（PostgreSQL 17.11） | 本章の扱い |
|---|---|---|
| 存在しないエンコーディング名の SQLSTATE を 22023 系と推測 | `42704`、`foo is not a valid encoding name` | yuzhu は UTF8 以外を 0A000 にする（既知のエンコーディングと未知の名前を区別しない。M5-DB-Q7） |
| `drop` できないテンプレートの SQLSTATE は 55006 か 22023 | **`42809`**、`cannot drop a template database` | 42809 |
| テンプレートの使用中は即 55006 | **5 秒待ってから 55006**。DETAIL は `There is 1 other session using the database.` | DB-D5 |
| 接続の検査順は DB → ロール（M2） | **ロール → ロールの接続数 → DB** | DB-D1 |
| `datfrozenxid` は不明 | 実機は新しい DB の `datfrozenxid` / `datminmxid` が**テンプレートの値のコピー**（全 DB で 730 / 1）。yuzhu の `yz_datxid` は **`min(テンプレートの行の値, oldest_xmin())`**（03 VC-D13。R-08。通常のテンプレートではコピーと同じ） | §3.2 |
| `datacl` は不明 | 新しい DB の `datacl` は **NULL**（template1 / template0 の ACL は引き継がれない） | §3.2 |
| OWNER に別のロールを指定できるか | 非 superuser は自分以外を指定すると **`42501 must be able to SET ROLE "x"`** | §6.3 |

---

## 1. 範囲

### 1.1 対応するもの

| 項目 | 内容 |
|---|---|
| 接続 | `Cluster::connect(&ConnectRequest)` の検査（DB-D1）、`Database(oid)` のセッションスコープ保持（D40）、`datconnlimit` / `rolconnlimit`、`datallowconn`、`DatabaseHandle` のキャッシュと `forget_database` |
| `CREATE DATABASE` | §6.2 のオプション。FILE_COPY 相当（D30）。`DBASE_CREATE_FILE_COPY` の WAL と REDO。失敗時の後始末 |
| `DROP DATABASE [IF EXISTS] name` | D32 の順序。`DBASE_DROP` の WAL と REDO |
| `ALTER DATABASE` | 3 つのフラグのオプションだけ（DB-D14） |
| カタログ | `pg_database` の行の作成・削除・更新（`SharedCatalogStore`）、`yz_datxid`（OID 9802）の定義・initdb の行・作成・削除（DB-D12）、`DatabaseRow` の拡張 |
| 起動 | 孤児ディレクトリと「ディレクトリの無い行」の WARNING（DB-D13） |

### 1.2 対応しない（実行すると 0A000。黙って無視しない）

`DROP DATABASE ... WITH (FORCE)`、`ALTER DATABASE` の `RENAME TO` / `OWNER TO` / `SET ...` / `RESET ...` / `REFRESH COLLATION VERSION` / `SET TABLESPACE`、`COMMENT ON DATABASE`、`CREATE DATABASE` の UTF8 以外の `ENCODING`、C 以外の `LC_COLLATE` / `LC_CTYPE` / `LOCALE`（42809）、`LOCALE_PROVIDER` の `icu` / `builtin`、`ICU_LOCALE` / `ICU_RULES` / `BUILTIN_LOCALE` / `COLLATION_VERSION` / `OID` オプション、`TABLESPACE`（`pg_default` 以外は 42704）、データベースへの GRANT / REVOKE（`CONNECT` 権限の検査は M6）、`pg_stat_database`、`DROP ROLE` との競合の完全な検出（所有ロールの同時削除。M5-DB-Q12）。

### 1.3 この章が保証すること

1. `CREATE DATABASE` が成功を返した後は、コミットも新しい DB のファイルも永続化されている（クラッシュしても残る）。失敗した場合は、新しい DB の行も `base/<新 OID>/` も残らない（クラッシュを除く。クラッシュのときは §5.3 の状態表のとおり、ディレクトリだけが残りうる）。
2. `DROP DATABASE` が成功を返した後は、行は無く、`base/<oid>/` も無い。途中のクラッシュでは「行は無いがディレクトリが残る」状態（D31）だけが起こり、「行はあるがファイルが壊れている」状態は起こらない。
3. `CREATE DATABASE` の実行中と `DROP DATABASE` の実行中、対象の DB への新しい接続は、その文が終わるまで（成功でも失敗でも）待たされる。待たされた接続は、終わった後に最新の `pg_database` を見て続行・拒否される。
4. 接続中のセッションがあるテンプレートからのコピー、接続中のセッションがある DB の DROP は起こらない（55006。ただし DB-D5 のとおり最長 5 秒は退出を待つ）。
5. 他の DB のデータに影響しない。`CREATE` / `DROP` を実行している間も、他の DB は読み書きできる（コピー前のチェックポイントと、DROP でファイルを消す間の `exclusive_barrier` による短い待ちを除く）。

---

## 2. 構成

```
impl/rust/crates/yuzhu-core/src/
├── dbase.rs                     ★ DB の下位の仕組み。Cluster・session・ddl を知らない
│                                   （DBASE レコードの形式・挿入・REDO、ディレクトリの作成・削除、RAII のロックガード
│                                    HeldLock / SessionLocks、`lock_database_exclusive`、名前予約ロックのタグ、孤児の検出）
├── ddl/database.rs              ★ create_database / drop_database / alter_database（M4 の ddl/ に足す。D41）
├── analyzer/ddl_ext/database.rs ★ AST → BoundCreateDatabase など（オプションの検証。§6.2）
├── sql/parser/database.rs       ★ CREATE / DROP / ALTER DATABASE の構文（§6.1）
├── catalog/
│   ├── store_db.rs              ★ `impl SharedCatalogStore`（pg_database の読み書き。yz_datxid の行は VC の store_vac.rs の insert_datxid / delete_datxid を呼ぶ。§4.2・R-08）
│   ├── store.rs                 △ F0: `SharedCatalogStore` のフィールドを `pub(in crate::catalog)` に、`datxid` の RelHandle を足す。`DatabaseRow` の項目（§4.2）
│   ├── schema.rs                △ yz_datxid（OID 9802、共有）の定義（F0 が作る。03 C8。§11 の依頼 8）
│   └── rows.rs                  △ initdb の行（pg_database の 3 行に対応する yz_datxid の 3 行。値は template1・postgres = 3、template0 = 番兵。F0。03 VC-D13）
├── engine.rs                    △ connect（置き換え）、database_handle、forget_database、起動時の孤児検査、ClusterOptions.database_busy_wait
├── datadir.rs                   △ copy_database_dir_checked（キャンセル可能なコピー）
├── recovery.rs                  △ dispatch に RmgrId::Dbase => dbase::redo（F0 が口を作る）
├── storage/buffer/mod.rs        △ drop_database_buffers（持ち主に依頼。§11）
├── storage/smgr.rs              △ forget_database、vfs()（同上）
├── wal/redo.rs                  △ InvalidPages::forget_database（同上）
├── wal/dump.rs                  △ DBASE の整形（同上）
├── debug_knobs.rs               △ 変異テスト用の 4 つのスイッチ（§4.7。同上）
yuzhu-server/tests/database.rs   ★ 統合テスト（§7.3）
yuzhu-core/tests/crash_sim/database.rs ★ クラッシュ試験のワークロードと不変条件（§7.4。DB-4）
tests/slt/m5/database/*.slt      ★ 共有テスト（§7.1）
tests/restart/m5/database/*      ★ 再起動・クラッシュをまたぐテスト（§7.2）
```

**依存の方向**: `dbase.rs` は `txn`（`LockManager`・`TxnManager`）、`wal`、`storage`、`catalog` の型に依存し、`engine` / `session` / `ddl` に依存しない（チェックポイントを呼ぶ手順は `ddl/database.rs` が `ctx.cluster.checkpoint()` で行い、`dbase.rs` は呼ばない）。`ddl/database.rs` は `dbase`・`catalog::store_db`・`engine`（`DdlCtx.cluster`）を使う。`engine.rs` は `dbase`（ガードと孤児検査）を使う。

**章の規約**:

1. ファイル操作は `Vfs` だけを使う（M2 規約）。コピー・削除のどちらも、障害注入（`SimVfs`）の対象になる。
2. ロックガード（`HeldLock`、`SessionLocks`）の Drop は I/O をしない（メモリ上のロックの解放だけ）。
3. 待つ関数は必ず `WaitCtl`（キャンセル・`statement_timeout`）を通す（M5 規約 2）。ポーリングは `interrupts.check()` を 1 周期ごとに呼ぶ。
4. 新しい REDO は冪等（M3 I9）。クラッシュしてもう一度同じレコードを REDO しても結果が変わらない。
5. `commit_and_restart` をまたいで `DdlCtx` を持たない（借用が切れる）。必要なものは `Arc` で取り出しておき、後で `ctl.ddl_ctx()` を取り直す。

---

## 3. ディスク上の形式

### 3.1 DBASE の WAL レコード（rmgr = `RmgrId::Dbase`。00 §3.3）

M3 §3.3 のレコードヘッダ（32 バイト、すべてリトルエンディアン）。`nblocks = 0`（ブロック参照なし）。`info` の下位 4 ビットは 0。

| info | 名前 | `xid` | メインデータ（`main_len`） |
|---|---|---|---|
| `0x00` | `CREATE_FILE_COPY` | CREATE DATABASE を実行したトランザクションの XID | 8 バイト: `src_db_oid: u32`、`dst_db_oid: u32` |
| `0x10` | `DROP` | 0（DB-D9） | 4 バイト: `db_oid: u32` |

- 予約: `0x20 CREATE_WAL_LOG`（`STRATEGY = WAL_LOG` を本当に実装するとき。M6 以降）。それ以外の info は読み手が不正とみなす（`XX001`、Panic）。
- テーブルスペースは `pg_default`（1663）だけなので、レコードに表領域の OID は持たない。
- `CREATE_FILE_COPY` の REDO の意味は §5.4。

**例 1: `CREATE_FILE_COPY`**（`xid = 1234`、`src = 1`（template1）、`dst = 16384`）。CRC は M3 §3.4 と同じく除く。この例（CRC を除く）を `dbase.rs` の単体テストの固定値にする。

| オフセット | 長さ | 内容（バイト列） |
|---|---|---|
| 0 | 4 | `tot_len = 40`（`28 00 00 00`） |
| 4 | 4 | CRC |
| 8 | 8 | `prev` |
| 16 | 8 | `xid = 1234`（`d2 04 00 00 00 00 00 00`） |
| 24 | 4 | `rmgr = 7`、`info = 0x00`、`nblocks = 0`、予約 0（`07 00 00 00`） |
| 28 | 4 | `main_len = 8`（`08 00 00 00`） |
| 32 | 8 | `src = 1`（`01 00 00 00`）、`dst = 16384`（`00 40 00 00`） |

`end = start + 40`（8 の倍数なのでパディングなし）。

**例 2: `DROP`**（`db = 16384`、`xid = 0`）:

| オフセット | 長さ | 内容 |
|---|---|---|
| 0 | 4 | `tot_len = 36`（`24 00 00 00`） |
| 4 | 4 | CRC |
| 8 | 8 | `prev` |
| 16 | 8 | `xid = 0` |
| 24 | 4 | `rmgr = 7`、`info = 0x10`、`nblocks = 0`、予約 0（`07 10 00 00`） |
| 28 | 4 | `main_len = 4`（`04 00 00 00`） |
| 32 | 4 | `db = 16384`（`00 40 00 00`） |
| 36 | 4 | パディング（0。`tot_len` に含めない） |

`end = start + 40`。

### 3.2 新しい DB の `pg_database` の行

`CREATE DATABASE x [options]` が書く行（M2 §6.8 の 18 列）。【確認】実機の値と照合済み。

| 列 | 値 |
|---|---|
| `oid` | §5.3 手順 6 で採番 |
| `datname` | 名前 |
| `datdba` | `OWNER`（省略は実行したロールの OID） |
| `encoding` | 6（UTF8） |
| `datlocprovider` | `'c'` |
| `datistemplate` | `IS_TEMPLATE`（既定 false） |
| `datallowconn` | `ALLOW_CONNECTIONS`（既定 true） |
| `dathasloginevt` | false |
| `datconnlimit` | `CONNECTION LIMIT`（既定 -1） |
| `datfrozenxid` | **新しい `yz_datxid` の値（§3.3 の `min(テンプレートの行の値, oldest_xmin())`）の下位 32 ビット**（テンプレートが template1 などの通常の DB なら、テンプレートの値と同じになる。template0 から作るときは `oldest_xmin()` の下位 32 ビット。PG 実機の「全 DB が同じ値」とは違う既知の差。値そのものを比べる共有テストは入れない） |
| `datminmxid` | **テンプレートの `datminmxid` をそのままコピー** |
| `dattablespace` | 1663 |
| `datcollate`、`datctype` | `'C'`（`POSIX` を指定しても `'C'`。【確認】） |
| `datlocale`、`daticurules`、`datcollversion`、`datacl` | NULL（テンプレートの ACL は引き継がない。【確認】） |

### 3.3 `yz_datxid` の行（共有カタログ、OID 9802、`global/9802`。D51）

| 列 | 型 | 内容 |
|---|---|---|
| `datid` | oid | `pg_database.oid` |
| `datfrozenxid8` | int8 | 64 ビットの凍結境界。`pg_database.datfrozenxid` はその下位 32 ビット |

- initdb は template1（1）と postgres（5）に値 **3**（`Xid::FIRST_NORMAL`。制御ファイルの初期の `oldest_xid` と同じ）の行を作り、**template0（4）には番兵 `DATFROZEN_PRISTINE`（`i64::MAX`。03 VC-D13）の行を作る**（`rows.rs` の initdb の行は F0 が 03 の規則で作る）。`pg_database` の `datfrozenxid` 列（M2 の `FROZEN_XID = 3`）は 3 のまま（番兵は `yz_datxid` だけ）。
- CREATE DATABASE は、**新しい行を `min(テンプレートの行の値, TxnManager::oldest_xmin())` で作り**（テンプレートが番兵の template0 なら `oldest_xmin()`）、`pg_database` の行と同じトランザクションで VC の `insert_datxid` を呼んで挿入する。DROP DATABASE は同じトランザクションで VC の `delete_datxid` を呼んで削除する。コピーした番兵（i64::MAX）を持つ DB は作られない（レビュー対応 R-08）。
- 1 つの `pg_database` の行に `yz_datxid` の行がちょうど 1 つ対応する（不変条件 I-DB1。§7.4）。
- **template0 の扱いは 03 VC-D13 で決まった**（M5-DB-Q8 は解決）: template0 は接続できないので VACUUM が一度も動かない。入っているのはブートストラップの XID（`Xid::BOOTSTRAP` = 1）だけで通常の XID を含まない（clog を引かない）ので、番兵 `i64::MAX` にして全 DB の最小値の計算に影響させない。**その番兵をそのままコピーした DB を作ると、その DB の XID を clog の切り詰めが守れなくなる**ので、CREATE DATABASE は上のとおり `min(.., oldest_xmin())` で作る。

### 3.4 データディレクトリ

`base/<oid>/` は、そのデータベースのリレーションのファイル（`<relfilenode>`、`<relfilenode>.<segno>`、`_fsm` などのフォーク）と `YUZHU_VERSION` を持つ（M2 §3.1）。CREATE DATABASE はこのディレクトリの**全ファイルをバイト単位でコピー**する（形式は変えない）。サブディレクトリは無い前提（M2 の `copy_database_dir` と同じ）。

---

## 4. 共通の型（契約）

00 の §4.8 と食い違わない。足した項目は「（足す）」と書き、§11 に一覧する。

### 4.1 engine.rs（Cluster の接続周り）

```rust
impl Cluster {
    /// 認証の後に呼ぶ（00 §4.8）。検査順は §5.1（DB-D1）。失敗はすべて Severity::Fatal。
    /// 成功すると、Database(db_oid) を AccessShare で LockScope::Session に取り、BackendRegistry と LockManager に登録済み
    pub fn connect(&self, req: &ConnectRequest<'_>) -> Result<ConnectGrant>;

    /// oid のデータベースの DatabaseHandle。キャッシュにあればそれ、なければ pg_database の行を引いて作る。
    /// 行が無ければ 3D000 `database with OID %u does not exist`（Severity::Error。FATAL ではない。接続以外の呼び出し元用:
    /// autovacuum、VACUUM の全データベース巡回など）。ロックは取らない（呼び出し側が dbase::HeldLock で取る）
    pub fn database_handle(&self, oid: Oid) -> Result<Arc<DatabaseHandle>>;

    /// DROP DATABASE の後始末。キャッシュから外す。残っている Arc は呼び出し側が持つ分だけ（ロックを持つ間は接続が無い）
    pub fn forget_database(&self, oid: Oid);

    /// ClusterOptions.database_busy_wait（既定 5 秒）。dbase::lock_database_exclusive に渡す
    pub fn database_busy_wait(&self) -> Duration;
}

pub struct ClusterOptions { /* 既存の項目に加えて */ pub database_busy_wait: Duration /* （足す）既定 5 秒 */ }

/// 00 §4.8 の ConnectGrant に session_locks を足す。Drop の順序が意味を持つので、フィールドの並びを変えない
/// （db、role、session_locks、backend の順に Drop される）
pub struct ConnectGrant {
    pub db: Arc<DatabaseHandle>,
    pub role: RoleRow,
    pub session_locks: SessionLocks,         // （足す）Database(oid) の AccessShare を含む、このセッションのセッションスコープのロックの持ち主
    pub backend: BackendGuard,
}
```

- `Cluster` の `databases: Mutex<HashMap<Oid, Arc<DatabaseHandle>>>`（M2）はそのまま。`DatabaseHandle` を作るのは `database_handle` と `connect` の中だけで、`databases` の Mutex を持ったまま `LockManager` や I/O をしない（作る前に行を読み、Mutex の中では挿入だけ）。
- `Session::new(cluster, params)`（M2 と同じ呼び出し）は、`cluster.next_session_id()` で ID を採り、`ConnectRequest { session_id, pid: id as i32, interrupts, client_addr, .. }` を作って `connect` を呼び、返った `ConnectGrant` の `db`・`role`・`session_locks`・`backend` を `Session` の（この順の）フィールドに保持する。
- **`Session::terminate()` の順序**（01 章の持ち分だが、この章の前提）: (1) 実行中のトランザクションをアボートし、トランザクションスコープのロックを解放する、(2) `session_locks` を Drop（`release_all(id, Session)` → `unregister_backend`）、(3) `backend` を Drop。`Session` が `terminate` を呼ばれずに Drop されても、(2)(3) は Drop の順序で同じになる（`release_all(.., Session)` はすべてのスコープを解放する。00 §4.2）。

### 4.2 catalog（`store.rs` の型と `store_db.rs`）

```rust
// store.rs（F0 が項目を足す。読み取りだけ）
pub struct DatabaseRow {
    pub oid: Oid, pub name: String, pub allow_conn: bool, pub is_template: bool,    // M2
    pub conn_limit: i32,        // 00 §4.8: datconnlimit
    pub owner: Oid,             // 00 §4.8: datdba
    pub encoding: i32,          // （足す）
    pub collate: String, pub ctype: String,                                  // （足す）
    pub frozen_xid: u32, pub min_mxid: u32,                                  // （足す）datfrozenxid / datminmxid（32 ビットの列の値）
}

// store_db.rs（DB が持つ。SharedCatalogStore の別ファイルの impl。F0 がフィールドの可視性を pub(in crate::catalog) にする）
impl SharedCatalogStore {
    pub fn database_by_oid(&self, snap: &Snapshot, oid: Oid) -> Result<Option<DatabaseRow>>;
    /// すべての行。oid の昇順
    pub fn databases(&self, snap: &Snapshot) -> Result<Vec<DatabaseRow>>;
    /// 所有しているデータベースの名前（AU の DROP ROLE の DETAIL `owner of database %s`）
    pub fn databases_owned_by(&self, snap: &Snapshot, role: Oid) -> Result<Vec<String>>;

    /// pg_database と yz_datxid に 1 行ずつ挿入する（§3.2、§3.3）。同じ WriteCtx（同じ XID）で 2 行
    pub fn insert_database(&self, w: &WriteCtx, row: &NewDatabase) -> Result<()>;
    /// 両方の行を削除する。MVCC の削除で、別のトランザクションが同じ行を更新中なら
    /// XX000 `tuple concurrently updated`（00 §3.8。待たない: wait = None）
    pub fn delete_database(&self, w: &WriteCtx, snap: &Snapshot, oid: Oid) -> Result<()>;
    /// ALTER DATABASE の 3 つのフラグ。pg_database の行の MVCC 更新（旧版の xmax と新版）
    pub fn update_database_flags(&self, w: &WriteCtx, snap: &Snapshot, oid: Oid, f: &DatabaseFlagUpdate) -> Result<()>;

    // yz_datxid の読み書き（`datxid` / `min_datxid` / `set_datxid_inplace` / `insert_datxid` / `delete_datxid`）はここに作らない。
    // 03 §4.3 の `catalog/store_vac.rs`（VC）が持つ（レビュー対応 R-08）。`insert_database` / `delete_database` はその `insert_datxid` / `delete_datxid` を呼ぶ。
    // 値を進める VC-4 の `set_datxid_inplace`（HEAP2 INPLACE。pg_database.datfrozenxid の鏡も同時に書く）の呼び出し側は、
    // 同じ DB の VACUUM 同士の直列化に dbase::frozen_lock_tag(oid) の Exclusive（トランザクションスコープ）を取る（§6.5）
}

#[derive(Clone, Debug)]
pub struct NewDatabase {
    pub oid: Oid, pub name: String, pub owner: Oid,
    pub is_template: bool, pub allow_conn: bool, pub conn_limit: i32,
    pub frozen_xid: u32, pub min_mxid: u32,       // frozen_xid = frozen8 の下位 32 ビット。min_mxid はテンプレートの値
    pub frozen8: Xid,                             // min(テンプレートの yz_datxid の値, TxnManager::oldest_xmin())。テンプレートが番兵の template0 なら oldest_xmin()（03 VC-D13）
}
#[derive(Clone, Copy, Debug, Default)]
pub struct DatabaseFlagUpdate { pub allow_conn: Option<bool>, pub is_template: Option<bool>, pub conn_limit: Option<i32> }
```

- `insert_database` / `delete_database` は `scan_rel` の全走査でよい（`pg_database` は数行）。OID の採番は `store_db.rs` に `get_new_database_oid(&self, alloc: &OidAllocator, vfs: &dyn Vfs) -> Result<Oid>` を足す。M2 の `CatalogStore::get_new_oid` は共有カタログを拒否する（`!d.shared` の条件。`get_new_oid_needs_an_oid_column` のテストが `PG_DATABASE` の `Err` を確かめている）ので使えない。同じ手順（`alloc.next_raw()` → `FIRST_NORMAL_OBJECT_ID` 未満は捨てる → `snapshot_any()` で `pg_database` を走査して同じ `oid` の行が無いこと）に、DB-D17 の「`base/<oid>` が無いこと」の確認を足し、最大 `MAX_OID_ATTEMPTS` 回繰り返す（尽きたら `54000`）。

### 4.3 dbase.rs

```rust
pub const DBASE_CREATE_FILE_COPY: u8 = 0x00;
pub const DBASE_DROP: u8 = 0x10;
/// 既定の待ち時間（ClusterOptions.database_busy_wait の既定値）と間隔
pub const DEFAULT_DATABASE_BUSY_WAIT: Duration = Duration::from_secs(5);
pub const DATABASE_BUSY_POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DbaseRecord { CreateFileCopy { src: Oid, dst: Oid }, Drop { db: Oid } }
impl DbaseRecord {
    /// (info, メインデータ)。§3.1
    pub fn encode(&self) -> (u8, Vec<u8>);
    /// 長さ・info（下位 4 ビット）の不整合は Severity::Panic の XX001
    pub fn decode(info: u8, main: &[u8]) -> Result<DbaseRecord>;
}

/// WAL・バッファ・ファイル・バリアの部品（Cluster を知らずに済ませるため。CheckpointParts と同じ考え方）
#[derive(Clone, Copy, Debug)]
pub struct DbaseParts<'a> {
    pub pool: &'a BufferPool, pub smgr: &'a StorageManager, pub wal: &'a Wal, pub txn: &'a TxnManager,
}
/// CREATE_FILE_COPY を挿入する（flush しない。コミットの flush に含まれる）。失敗は Panic
pub fn log_create_file_copy(p: &DbaseParts<'_>, xid: Xid, src: Oid, dst: Oid) -> Result<Lsn>;
/// DROP を挿入して flush する（ディレクトリの削除より前でなければならない）。失敗は Panic
pub fn log_drop(p: &DbaseParts<'_>, db: Oid) -> Result<Lsn>;
/// REDO（§5.4）。recovery::dispatch が RmgrId::Dbase に対して呼ぶ
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()>;

pub fn database_dir(db: Oid) -> PathBuf;                                   // base/<db>
/// ディレクトリを（なければ何もせず）消し、base/ を sync_dir する
pub fn remove_database_dir(vfs: &dyn Vfs, db: Oid) -> Result<()>;
/// base/ の下の「10 進の数字だけの名前」のディレクトリの OID。それ以外の名前は無視する
pub fn list_database_dirs(vfs: &dyn Vfs) -> Result<Vec<Oid>>;
pub struct OrphanReport { pub dirs_without_row: Vec<Oid>, pub rows_without_dir: Vec<(Oid, String)> }
pub fn find_orphans(dirs: &[Oid], rows: &[DatabaseRow]) -> OrphanReport;    // 純関数（単体テストしやすいように分ける）

// --- ロックのタグ（LockTag::Object { db: 0, class: PG_DATABASE(1262), obj }）
/// 名前予約ロック。obj は名前の FNV-1a 64 ビットハッシュに最上位ビットを立てたもの
pub fn name_lock_tag(name: &str) -> LockTag;
/// datfrozenxid の更新用（VC）。obj = データベースの OID（最上位ビットは 0 なので name_lock_tag と衝突しない）
pub fn frozen_lock_tag(db: Oid) -> LockTag;

// --- RAII のロックガード（Drop は I/O をしない）
/// 1 つの（タグ、モード）を LockScope::Session で持つ。Drop で、まだ持っていれば release する
#[derive(Debug)]
pub struct HeldLock { /* Arc<LockManager>, BackendId, LockTag, LockMode */ }
impl HeldLock {
    pub fn acquire(locks: &Arc<LockManager>, id: BackendId, tag: LockTag, mode: LockMode, wait: &WaitCtl<'_>) -> Result<HeldLock>;
    pub fn try_acquire(locks: &Arc<LockManager>, id: BackendId, tag: LockTag, mode: LockMode) -> Option<HeldLock>;
}
/// 1 本の接続のセッションスコープのロック全体の持ち主。Drop で release_all(id, Session) → unregister_backend
#[derive(Debug)]
pub struct SessionLocks { /* Arc<LockManager>, BackendId */ }
impl SessionLocks { pub fn register(locks: &Arc<LockManager>, id: BackendId, pid: i32) -> SessionLocks; pub fn id(&self) -> BackendId; }

/// db の AccessExclusive を、他のセッションが居なくなるまで最長 wait_total、DATABASE_BUSY_POLL ごとに try_acquire して取る（DB-D5）。
/// 取れなければ 55006。`kind` で文言を変える。取れるまでの間、interrupts.check() を周期ごとに呼ぶ（57014 / 57P01）
pub fn lock_database_exclusive(
    locks: &Arc<LockManager>, backends: &BackendRegistry, me: BackendId, my_db: Oid,
    db: Oid, name: &str, kind: BusyKind, wait_total: Duration, interrupts: &InterruptFlag,
) -> Result<HeldLock>;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BusyKind { Source /* CREATE のテンプレート */, Target /* DROP の対象 */ }

/// 失敗時の後始末（§5.3）。disarm されなければ Drop で base/<oid>/ を消す（失敗は無視。起動時の WARNING が拾う）。
/// `commit_and_restart` で DdlCtx の借用が切れてもガードが生きるよう、Vfs は Arc で持つ
#[derive(Debug)]
pub struct CreatedDirGuard { /* Arc<dyn Vfs>, Oid, armed: bool */ }
impl CreatedDirGuard { pub fn new(vfs: Arc<dyn Vfs>, db: Oid) -> Self; pub fn disarm(&mut self); }
```

- `HeldLock::drop` は `locks.holds(id, tag, mode)` が真のときだけ `release` する（同じ（タグ、モード）を `Transaction` スコープでも持っているときに、スコープを取り違えない。この章のロックは Session スコープだけなので、`holds` が真なら自分のもの）。
- `lock_database_exclusive` の 55006 の DETAIL（【確認】単数形。複数形は PostgreSQL の `errdetail_plural` で（未検証））: 他のセッション数 `n = backends.count_in_db(db) − (my_db == db ? 1 : 0)`（0 になったら 1 にする）。`n == 1`: `There is 1 other session using the database.`、それ以外: `There are {n} other sessions using the database.`。メッセージは `Source` が `source database "{name}" is being accessed by other users`、`Target` が `database "{name}" is being accessed by other users`（00 §3.8）。
- 同じバックエンドが持つロックは衝突しない（00 §3.4）ので、自分が `Database(db)` の AccessShare を持っていても（自分がテンプレートに接続している）AccessExclusive は他のセッションが居なければ取れる。【確認】実機: template1 に接続して `CREATE DATABASE` は成功する。

### 4.4 sql・analyzer（AST と Bound）

```rust
// sql/ast.rs（F0 が変種を足す。DB が中身を埋める）
pub struct CreateDatabaseStmt { pub name: String, pub options: Vec<DatabaseOption> }
pub struct DropDatabaseStmt { pub name: String, pub if_exists: bool, pub force: bool }
pub struct AlterDatabaseStmt { pub name: String, pub action: AlterDatabaseAction }
pub enum AlterDatabaseAction {
    Options(Vec<DatabaseOption>),
    /// RENAME TO / OWNER TO / SET / RESET / REFRESH COLLATION VERSION / SET TABLESPACE（アナライザが 0A000）
    Unsupported(&'static str),
}
/// name は小文字化済み（`CONNECTION LIMIT` は "connection_limit"）。pos はエラー位置（先頭の文字からの位置）
pub struct DatabaseOption { pub name: String, pub value: DatabaseOptionValue, pub pos: usize }
pub enum DatabaseOptionValue {
    Default,                  // DEFAULT
    Ident(String),            // 引用符なしの識別子（小文字化済み）、または ON
    Str(String),              // '...'（そのまま）または "..."（そのまま）
    Bool(bool),               // TRUE / FALSE キーワード
    Int(i64),                 // 符号つきの整数
    Float(String),            // 小数・指数形式（整数でないので CONNECTION LIMIT では 42601）
}

// analyzer/bound.rs（F0 が変種を足す。DB が中身を決める）
pub struct BoundCreateDatabase {
    pub name: String, pub template: String,                  // 既定 "template1"
    pub owner: Option<String>,                               // None = 実行したロール
    pub conn_limit: i32, pub is_template: bool, pub allow_conn: bool,
}
pub struct BoundDropDatabase { pub name: String, pub if_exists: bool }
pub struct BoundAlterDatabase { pub name: String, pub flags: DatabaseFlagUpdate }
//   BoundDdl::CreateDatabase / DropDatabase    → ddl::execute_standalone（トランザクションを自分で閉じて開き直す。00 §4.7）
//   BoundDdl::AlterDatabase（足す）             → ddl::execute（通常のトランザクションの中。ブロック内でも可）
```

### 4.5 ddl/database.rs

```rust
/// execute_standalone の CreateDatabase / DropDatabase の変種から呼ばれる。コマンドタグ（"CREATE DATABASE" / "DROP DATABASE"）を返す。
/// DdlCtx.outside_block が偽なら debug_assert（session が 25001 を返してから呼ぶ。DB-D16）
pub fn create_database(ctl: &mut dyn TxnControl, b: BoundCreateDatabase) -> Result<String>;
pub fn drop_database(ctl: &mut dyn TxnControl, b: BoundDropDatabase) -> Result<String>;
/// ddl::execute の AlterDatabase の変種から呼ばれる
pub fn alter_database(ctx: &mut DdlCtx<'_>, b: BoundAlterDatabase) -> Result<String>;
```

### 4.6 他の持ち主の型に足してもらう関数（§11 に一覧）

```rust
// storage/buffer/mod.rs（M2 のレポート m2-buffer-io.md が予告していたもの。未実装）
impl BufferPool {
    /// db_oid のリレーションのバッファを、書かずにすべて捨てる。ピンされたものがあれば内部エラー（XX000）。drop_relation_buffers の db 版
    pub fn drop_database_buffers(&self, db_oid: Oid) -> Result<()>;
}
// storage/smgr.rs
impl StorageManager {
    /// db に属する、開いているファイル（rels）、pending_sync、pending_unlink を捨てる（ファイルは触らない）
    pub fn forget_database(&self, db: Oid) -> Result<()>;
    /// REDO（RedoCtx は vfs を持たない）と dbase.rs が使う
    pub fn vfs(&self) -> &Arc<dyn Vfs>;
}
// wal/redo.rs
impl InvalidPages { pub fn forget_database(&mut self, db: Oid); }      // PG の forget_invalid_pages_db
// datadir.rs（DB が持つ）
/// copy_database_dir と同じ。check をファイルごとと 1 MiB ごとに呼び、Err なら dst を消して Err を返す（キャンセルのため）。
/// 既存の copy_database_dir は check が常に Ok の呼び出し
pub fn copy_database_dir_checked(vfs: &dyn Vfs, src: Oid, dst: Oid, sync: bool, check: &mut dyn FnMut() -> Result<()>) -> Result<()>;
```

### 4.7 debug_knobs.rs（変異テスト用。既定はすべて無効。持ち主に足してもらう）

```rust
/// DB: コピー前のチェックポイントを省く
pub skip_pre_copy_checkpoint: bool,
/// DB: コピーの fsync（ファイルとディレクトリ）を省く（DBASE_CREATE の REDO が修復することを確かめる）
pub skip_copy_fsync: bool,
/// DB: DBASE_CREATE を WAL に入れない
pub skip_dbase_wal: bool,
/// DB: DROP でディレクトリの削除を行の削除のコミットより前に行う（D32 を破る）
pub drop_dir_before_commit: bool,
```

---

## 5. 処理の流れ

### 5.1 接続（`Cluster::connect`）

```
connect(req) -> Result<ConnectGrant>                       ※ すべてのエラーは Severity::Fatal
  snap = txn.take_snapshot(None, FIRST_COMMAND_ID)               登録する短命のスナップショット（`take_snapshot`。使い終えたら Drop。D12、LK-D9、R-02）。ストレージバリアは取らない
  1. role = shared.role_by_name(snap, req.user)
       None                → 28000 `role "x" does not exist`
       !role.can_login     → 28000 `role "x" is not permitted to log in`
  2. row_opt = shared.database_by_name(snap, req.database)       None でもまだ失敗しない（手順 3 のロールの接続数が先。DB-D1）
  3. backend = backends.register(BackendInfo { id: BackendId(req.session_id), pid, db_oid: row_opt の oid（None なら 0）,
                                               user_oid: role.oid, client_addr, interrupts })
     session_locks = SessionLocks::register(locks, id, pid)      以後、エラーで戻れば Drop で外れる
     if role.conn_limit >= 0 && !role.superuser && backends.count_of_role(role.oid) > role.conn_limit
                           → 53300 `too many connections for role "x"`
  4. row = row_opt                None → 3D000 `database "x" does not exist`
     Database(row.oid) を AccessShare・LockScope::Session で acquire する
       WaitCtl { lock_timeout: None, deadlock_timeout: 既定, interrupts: req.interrupts }
       CREATE / DROP DATABASE が AccessExclusive を持っている間はここで待つ（D30、D40）。
       待ちの中断: サーバの停止 → 57P01（FATAL）。この待ちは 00 規約 1 を満たす（ほかのロック・ラッチ・ピンを持たない）
  5. snap = txn.take_snapshot(None, FIRST_COMMAND_ID)                 待った間のコミットを見るため取り直す
     row2 = shared.database_by_oid(snap, row.oid)
       None または row2.name != row.name → 3D000 `database "x" does not exist`、DETAIL `It seems to have just been dropped or renamed.`
                                          （ロックは session_locks の Drop で外れる）
  6. !row2.allow_conn                    → 55000 `database "x" is not currently accepting connections`   （superuser でも拒否。【確認】）
  7. row2.conn_limit >= 0 && !role.superuser && backends.count_in_db(row2.oid) > row2.conn_limit
                                         → 53300 `too many connections for database "x"`
  8. db = database_handle_for(&row2)     databases の Mutex の中は挿入だけ
  Ok(ConnectGrant { db, role, session_locks, backend })
```

- 手順 3 は、自分を登録してから数える（DB-D3）ので、`CONNECTION LIMIT 1` の DB で 2 本目は 2 > 1 で拒否される。拒否されたセッションは `backend` の Drop で登録簿から外れるので、数えに残らない。
- `role` は手順 1 の行を使い続ける（手順 5 の間にロールが消えても、そのセッションは動き続ける。PostgreSQL と同じ）。
- この関数の中で `connect` が呼び出し元のスレッドを長く止めうるのは手順 4 だけ。呼び出し元のサーバは、`Session::new` の前に `InterruptFlag` を作ってサーバの登録簿に登録しておくこと（停止の通知が届くように。§11 の依頼 2）。

### 5.2 セッションの終了

§4.1 の `Session::terminate` の順序のとおり。`Database(oid)` の AccessShare は `session_locks` の Drop で外れる。外れた時点で、`lock_database_exclusive` のポーリングが次の周期に成功しうる。

### 5.3 CREATE DATABASE（`ddl::database::create_database`）

トランザクションの流れ: session が開始した暗黙のトランザクション T1（XID なし）→ 手順 12 で XID を取得して書く → 手順 13 の `commit_and_restart` でコミット → 新しい暗黙のトランザクション T2（空）→ 手順 14 のチェックポイント。

```
create_database(ctl, b)
  ─ フェーズ 1: 検査とロック（T1。WAL を書かない）
  1. ctx = ctl.ddl_ctx()
       owner = b.owner のロールを引く。無ければ 42704 `role "x" does not exist`。None なら ctx.role.oid
       **owner のロール OID に `catalog::roles::lock_role_shared(locks, me, owner.oid, wait)`（08 C9。AccessShare・トランザクションスコープ）を取り、取った後に `authid_by_oid` で存在を確かめ直す（無ければ 42704）**。
       `DROP ROLE` は同じ OID を AccessExclusive で取る（08 AU-D13）ので、所有者にするロールの同時削除を塞ぐ。ロックは T1 のコミット（手順 13 の `commit_and_restart`）で外れるが、その時点で `pg_database` の行（owner つき）が見え、`DROP ROLE` の `databases_owned_by` が拾う（レビュー対応 R-16）
       非 superuser が owner != ctx.role.oid → 42501 `must be able to SET ROLE "x"`
       conn_limit < -1 は analyzer で 22023 済み
       !(ctx.role.superuser || ctx.role.create_db) → 42501 `permission denied to create database`
     cluster = ctx.cluster、vfs = Arc::clone(&cluster.stack().vfs)、me = ctx.backend、interrupts = ctx.wait.interrupts
  2. name_lock = HeldLock::acquire(name_lock_tag(b.name), Exclusive, ctx.wait)            ← 同名の CREATE / DROP を直列化（待つ）
  3. snap = txn.take_snapshot(own = None, ..)                                                  ← ロックの後に取る（00 §5.1 の理由）
       tpl = shared.database_by_name(snap, b.template)        None → 3D000 `template database "x" does not exist`
       !tpl.is_template && !(superuser || tpl.owner == role.oid)
                                                              → 42501 `permission denied to copy database "x"`
       shared.database_by_name(snap, b.name).is_some()        → 42P04 `database "x" already exists`
  4. tpl_lock = lock_database_exclusive(.., tpl.oid, &tpl.name, Source, cluster.database_busy_wait(), interrupts)   → 55006
  5. tpl = shared.database_by_oid(txn.take_snapshot(..), tpl.oid)   待っている間に DROP / 更新された可能性
       None → 3D000 `template database "x" does not exist`。権限（手順 3 の 2 つ目）を新しい行でもう一度検査
  6. new_oid = shared.get_new_database_oid(oids, vfs)          get_new_oid(1262) で行との重複を避け、base/<oid> が無いものまで繰り返す（DB-D17）
     new_lock = HeldLock::try_acquire(Database(new_oid), AccessExclusive)   必ず取れる（誰も OID を知らない）。取れなければ内部エラー
  7. src8 = shared.datxid(snap, tpl.oid)                       None → XX001 `yz_datxid has no row for database %u`（03 §4.3）
     frozen8 = min(src8, txn_mgr.oldest_xmin())                番兵（template0。i64::MAX）は oldest_xmin() になる。oldest_xmin は単調に増えるので、ここで読んだ値は後で読むより小さいか等しい（安全側）
  8. cluster.checkpoint()                                      ← D30 の 1。Explicit（必ず実行する）。手順 4 の後なので、
                                                                 テンプレートの dirty ページはここですべてディスクに書かれ、
                                                                 以後テンプレートは（DB-D6 のロックで）変更されない
                                                                 DebugKnobs::skip_pre_copy_checkpoint で省ける
  ─ フェーズ 2: コピーと行の挿入（T1）
  9. w = ensure_xid(ctx)                                       txn.xid が None なら assign_xid（TxnManager）して Transaction に保存し、write_ctx()
 10. guard = CreatedDirGuard::new(Arc::clone(&vfs), new_oid)
     copy_database_dir_checked(vfs, tpl.oid, new_oid, sync = !knobs.skip_copy_fsync, &mut || interrupts.check())
                                                               ← 各ファイルと base/<new>/、base/ を fsync。キャンセル・タイムアウトは 57014
 11. log_create_file_copy(parts, w.xid, tpl.oid, new_oid)      ← DBASE_CREATE（flush しない）。skip_dbase_wal で省ける
 12. shared.insert_database(&w, NewDatabase { oid: new_oid, name, owner, is_template, allow_conn, conn_limit,
                                              frozen_xid: frozen8.0 as u32, min_mxid: tpl.min_mxid, frozen8 })
 13. guard.disarm()                                            ← commit_and_restart を呼ぶ「直前」に無効化する（コミットの結果が分からない
                                                                 状態でディレクトリを消すと、コミット済みの DB を壊す）
     drop(ctx)
     ctl.commit_and_restart()?                                 ← T1 のコミット（WAL を flush。D38 の順序）。失敗は Panic（クラスタを poison）
  ─ フェーズ 3: 最後のチェックポイント（T2。論理的には DB は存在する）
 14. ctl.ddl_ctx()?.cluster.checkpoint()?                      ← D30 の最後。REDO の範囲を DBASE_CREATE より後ろに進める（DB-D8 の前提を閉じる）
                                                                 失敗はそのエラーを返す（コミット済み。M5-DB-Q3）
 15. ロックを解放（new_lock → tpl_lock → name_lock の逆順に Drop）。待たされていた接続はここから進む
 16. Ok("CREATE DATABASE")
```

**失敗時**（手順 13 の `disarm` より前のエラー・キャンセル・`statement_timeout`）: `CreatedDirGuard` が `base/<new>/` を消す。T1 は session がアボートする。ロックは各 `HeldLock` の Drop で外れる。バッファは新 DB の分が無い（読み込んでいない）ので破棄不要。DBASE_CREATE が WAL にあってもよい（REDO は §5.4。アボートした XID のレコードでも REDO は無条件に実行されるので、クラッシュ後に孤児ディレクトリが再作成されうる。D31 の WARNING が拾う）。

**手順 13 より後のエラー**: ディレクトリは消さない（コミットされたかもしれない）。`commit_and_restart` の失敗は Panic（M3 §5.3）なので、再起動後のリカバリが状態を決める。手順 14 の失敗は、DB が存在するまま呼び出し元にエラーを返す。

**クラッシュ時の状態**（再起動後。DB-4 の期待値の表。`ok` = 通常の起動）:

| クラッシュの時点 | 再起動後の `pg_database` | `base/<new>/` | 備考 |
|---|---|---|---|
| 手順 8 のチェックポイントの中、またはその前 | 行なし | なし | 何も起きていない |
| 手順 10 のコピーの途中 | 行なし | 不完全（あるいは一部のファイル） | **孤児**。WARNING。同名の DB は作り直せる。新しい OID は孤児と重ならない（OID の先取りは制御ファイルに fsync 済みで、DB-D17 の `base/<oid>` の確認もある） |
| 手順 10 の完了後・手順 12 より前 | 行なし | 完全 | 孤児（完全） |
| 手順 11 の後・コミットが永続化される前 | 行なし（XID はコミットされていない） | 完全（`DBASE_CREATE` が WAL にあれば REDO が作り直す） | 孤児（完全） |
| コミットが永続化された後・手順 14 のチェックポイントの完了前 | **行あり**（`yz_datxid` の行も） | 完全（REDO が `DBASE_CREATE` でコピーし直す） | DB は使える。コピー元は DB-D6 のロックで変わっていない |
| 手順 14 の完了後 | 行あり | 完全 | 通常 |

### 5.4 DBASE の REDO（`dbase::redo`）

```
redo(ctx, rec):
  match DbaseRecord::decode(rec.info, &rec.main)?
  CreateFileCopy { src, dst }:
     1. pool.drop_database_buffers(dst)?; smgr.forget_database(dst)?; invalid.lock().forget_database(dst)
     2. if vfs.exists(base/<dst>)? { vfs.remove_dir_all(base/<dst>)?; vfs.sync_dir(base)? }       ← 毎回作り直す（冪等。途中のクラッシュも同じ）
     3. if !vfs.exists(base/<src>)? {
            WARNING "could not copy database %u to %u: source directory base/%u does not exist; skipping"   ← DB-D8
            return Ok(())
        }
        datadir::copy_database_dir(vfs, src, dst, sync = true)?                                       ← 失敗は Panic（REDO の失敗は Cluster::open の失敗）
  Drop { db }:
     1. pool.drop_database_buffers(db)?; smgr.forget_database(db)?; invalid.lock().forget_database(db)
     2. remove_database_dir(vfs, db)?                                                                ← なければ何もしない
```

- `RedoCtx` は `vfs` を持たないので `ctx.smgr.vfs()`（§4.6）を使う。REDO は単一スレッドで、`exclusive_barrier` は要らない。
- **`invalid.forget_database` が要る理由**: DROP された DB のページに触る古い WAL レコード（`DBASE_DROP` より前にある `HEAP_INSERT` など）は、ディレクトリが既に無いまま REDO されると「ファイルが無い」として `InvalidPages` に記録される（`FPI` 付きならファイルとディレクトリを作り直す。`smgr.create` は親ディレクトリを作る）。`DBASE_DROP` がそれらを `forget` して、最後の `check_empty`（M3 D14）を通す。PostgreSQL の `forget_invalid_pages_db` と同じ。
- **REDO が走る範囲**: `CREATE_FILE_COPY` は §5.3 の最後のチェックポイントの REDO 点より前になるので、「コミット後・最後のチェックポイントの完了前」のクラッシュのときだけ REDO される。`DROP` は §5.5 のとおり、ディレクトリの削除より先に永続化されるので、削除の途中のクラッシュで REDO が削除を完了させる。

### 5.5 DROP DATABASE（`ddl::database::drop_database`）

```
drop_database(ctl, b)
  ─ フェーズ 1: 検査とロック（T1）
  1. ctx = ctl.ddl_ctx()（DdlCtx.outside_block が真であること）
  2. name_lock = HeldLock::acquire(name_lock_tag(b.name), Exclusive, ctx.wait)
  3. snap = txn.take_snapshot(None, ..)（ロックの後）
       row = shared.database_by_name(snap, b.name)
         None → if b.if_exists { ctx.notices.push(NOTICE "database \"x\" does not exist, skipping"); return Ok("DROP DATABASE") }
                else 3D000 `database "x" does not exist`
  4. !(ctx.role.superuser || row.owner == ctx.role.oid)   → 42501 `must be owner of database x`
  5. row.is_template                                      → 42809 `cannot drop a template database`
  6. row.oid == ctx.db.oid                                → 55006 `cannot drop the currently open database`
  7. lock = lock_database_exclusive(.., row.oid, &row.name, Target, cluster.database_busy_wait(), interrupts)   → 55006
     （検査の順序は【確認】PG の `dropdb` のソース上の順（LOCATION の行番号）: 存在 → 所有者 → テンプレート → 現在の DB → 使用中）
  8. row = shared.database_by_oid(txn.take_snapshot(..), row.oid)   None → 3D000（手順 2 のロックがあるので通常は起きない。念のため）
  ─ フェーズ 2: 行の削除とコミット（D32 の 1）
  9. w = ensure_xid(ctx)
     shared.delete_database(&w, &snap, row.oid)               pg_database と yz_datxid の 2 行
     （DebugKnobs::drop_dir_before_commit なら、ここでフェーズ 3 の手順 11〜12 を先に行う: 変異試験用）
 10. drop(ctx); ctl.commit_and_restart()?                      ← ここで DB は論理的に消える。新しい接続はロックの手前で待っている
  ─ フェーズ 3: ファイルの破棄（T2。コミット済み）
 11. ctl.ddl_ctx()? を取り直し、cluster の部品から DbaseParts を作る
     { _b = txn.exclusive_barrier()?                           ← チェックポイントの書き出しとだけ排他（D10）
       pool.drop_database_buffers(oid)?                        書き出さずに捨てる（dirty でも）
       smgr.forget_database(oid)?
       log_drop(parts, oid)?                                   ← DBASE_DROP を挿入して flush（xid = 0。DB-D9）
       remove_database_dir(vfs, oid)                           ← remove_dir_all + sync_dir(base) }
 12. cluster.forget_database(oid)
 13. ロックを解放（lock → name_lock の逆順に Drop）。待たされていた接続は 3D000 になる
 14. Ok("DROP DATABASE")
```

- **手順 11 の失敗**: `Severity::Panic` はそのまま返す（クラスタが poison される）。それ以外の失敗（ディレクトリの削除の I/O エラーなど）は、行は既に消えているので WARNING（`could not remove database directory "base/N": ...`）を `ctx.notices` に積み、DROP は成功として返す（ディレクトリは孤児として残る。D31）。
- **コミットの前に何も消さない**ことが D32 の要点。手順 10 の前のクラッシュ・エラーでは、DB は無傷（バッファも捨てていない）。
- **DROP が使えない状態**: 手順 6 の「現在の DB」は `ctx.db.oid`（このセッションが接続している DB）と比べる。テンプレートの判定（手順 5）が先なので、template1 に接続して `DROP DATABASE template1` は `42809`（【確認】LOCATION の順序）。
- **クラッシュ時の状態**:

| クラッシュの時点 | 再起動後の `pg_database` | `base/<oid>/` |
|---|---|---|
| 手順 10 のコミットが永続化される前 | **行あり**（無傷） | 完全 |
| コミットの永続化後・`DBASE_DROP` の永続化前（手順 11 の前半） | 行なし | 完全（**孤児**。WARNING） |
| `DBASE_DROP` の永続化後・ディレクトリの削除の途中 | 行なし | REDO が削除を完了する（孤児なし）。`DBASE_DROP` は、最後のチェックポイントの REDO 点より後ろにある間だけ REDO される。手順 11 は `exclusive_barrier` の中なので、その間に始まったチェックポイントのバッファの書き出しは、ディレクトリの削除が終わるまで完了しない（REDO 点が `DBASE_DROP` より後ろに進んでも、完了したチェックポイントの時点でディレクトリは既に無い） |
| 手順 11 の完了後 | 行なし | なし |

### 5.6 起動時の孤児検査（`Cluster::open`）

REDO と終了チェックポイントの後（00 §5.4）、共有カタログを読めることを確かめた直後に行う。

```
rows = shared.databases(txn.take_snapshot(None, ..))
dirs = dbase::list_database_dirs(vfs)
report = dbase::find_orphans(&dirs, &rows)
for oid in report.dirs_without_row:
    warn("database directory \"base/{oid}\" has no row in pg_database (left over from an interrupted CREATE or DROP DATABASE); it is not removed")
for (oid, name) in report.rows_without_dir:
    warn("database \"{name}\" (OID {oid}) has a row in pg_database but its directory \"base/{oid}\" does not exist")
```

- どちらも WARNING だけで起動は止めない（D31）。ディレクトリは消さない。`base/` の下で数字だけの名前でないもの（`YUZHU_VERSION` など）は無視する。
- 孤児は起動のたびに同じ WARNING を出す。手で消すまで容量が残る（M5-Q10 と同じ。確認事項は M5-DB-Q9）。

### 5.7 ALTER DATABASE（`ddl::database::alter_database`。通常のトランザクションの中）

```
alter_database(ctx, b)
  1. name_lock = Object 名前予約ロックを Exclusive・LockScope::Transaction で取る（CREATE / DROP と直列化。コミットで外れる）
  2. snap = 新しいスナップショット。row = database_by_name   None → 3D000 `database "x" does not exist`
  3. !(superuser || row.owner == role.oid) → 42501 `must be owner of database x`   【確認】
  4. shared.update_database_flags(&w, &snap, row.oid, &b.flags)    w = ensure_xid
       ・同じ行を別のトランザクションが更新中（ALTER DATABASE の更新など。VC の `set_datxid_inplace` は yz_datxid の行を INPLACE で書くので pg_database の行の MVCC の更新とは衝突しない。R-08）→ XX000 `tuple concurrently updated`
  5. Ok("ALTER DATABASE")
```

- ブロック内で実行できる（【確認】実機）。ロールバックすれば元に戻る（MVCC の更新）。現在接続している DB に対しても実行できる（`IS_TEMPLATE` を false にするなど）。
- `ALLOW_CONNECTIONS false` にしても、既に接続しているセッションは切れない（PostgreSQL と同じ）。

### 5.8 ロックの順序

00 §5.3 に従う。この章が足すもの:

- フェーズ 1 のロックの取得順は **名前予約 → `Database(template)`（またはドロップ対象）→ `Database(新 OID)`**。この順で取るので、2 つの CREATE / DROP の間でロック待ちのサイクルは作れない（待つのは名前予約ロックだけ。`Database(..)` はポーリングで、待ち手にならない）。
- 名前予約ロックを持ったままポーリングの間（最長 5 秒）眠る。待つのは他の CREATE / DROP（同名）と、その名前の ALTER だけ。
- チェックポイント（フェーズ 1 手順 8、フェーズ 3 手順 14）は `LockManager` のロックを持ったまま呼ぶ。チェックポイントは `LockManager` を待たないので、規約 1（待つ前の禁止事項）に当たらない。ただし**ページのラッチ・ピン・コミットゲートを持たない**こと（`assert_no_pins` を各手順の前に呼ぶ）。
- フェーズ 3 手順 11 の `exclusive_barrier` は、`LockManager` のロックより後に取る（00 §5.3 の順 1 → 3）。
- 接続側（§5.1 手順 4）が待つのは `Database(oid)` だけ。ほかの何も持たない。

---

## 6. モジュールごとの仕様

### 6.1 パーサ（`sql/parser/database.rs`）

文法（【確認】実機の挙動に合わせた。キーワードは大文字小文字を区別しない）:

```
CREATE DATABASE name [ [ WITH ] option [ ... ] ]
   option := option_name [ = ] value  |  CONNECTION LIMIT [ = ] signed_integer
   option_name := 識別子（TEMPLATE、OWNER、ENCODING、LC_COLLATE、LC_CTYPE、LOCALE、STRATEGY、TABLESPACE、
                  ALLOW_CONNECTIONS、IS_TEMPLATE、LOCALE_PROVIDER、ICU_LOCALE、ICU_RULES、BUILTIN_LOCALE、
                  COLLATION_VERSION、OID、または未知の名前。未知は構文ではなくアナライザが 42601 にする）
   value := 符号つきの数値 | TRUE | FALSE | 識別子（引用符なしは小文字化）| '文字列' | "引用識別子" | DEFAULT
DROP DATABASE [ IF EXISTS ] name [ [ WITH ] ( FORCE [ , FORCE ... ] ) ]
ALTER DATABASE name [ WITH ] option [ ... ]            -- option は CREATE と同じ字句
ALTER DATABASE name { RENAME TO .. | OWNER TO .. | SET .. | RESET .. | REFRESH COLLATION VERSION | SET TABLESPACE .. }
```

- 値が必要。`create database x is_template`（値なし）は 42601 `syntax error at end of input`（【確認】）。`CONNECTION = 3`（`LIMIT` なし）は 42601。
- `OWNER` の値は識別子か文字列（`current_user` などのキーワードは 42601。【確認】`create database x owner current_user` は構文エラー）。`TEMPLATE true` は `TRUE` キーワードが文字列 `"true"` として扱われる（【確認】3D000 `template database "true" does not exist`）。
- 複数文の中で `CREATE DATABASE` が最初でも最後でも、構文は通り、25001 は session が出す（DB-D16）。
- 名前が空の引用識別子（`""`）は M1 の字句の規則どおり 42601（【確認】`zero-length delimited identifier`）。
- `DROP DATABASE` の `FORCE` はパーサが受け付け、アナライザが 0A000 にする。`DROP DATABASE a, b`、`DROP DATABASE IF EXISTS a, b` は 42601（【確認】）。

### 6.2 アナライザ（`analyzer/ddl_ext/database.rs`）: オプションの検証

`DatabaseOption` を `BoundCreateDatabase` / `BoundAlterDatabase` に変える。この段階で**カタログを引かない**（ロール・テンプレートの存在などは実行時。§5.3）。

**値の取り出し（PostgreSQL の `defGetString` / `defGetBoolean` / `defGetInt32` と同じ）**:

- 文字列: `Ident` / `Str` はそのまま、`Bool(b)` は `"true"` / `"false"`、`Int` は 10 進の文字列、`Default` は「指定なし」。
- 真偽: `Bool(b)`、`Int(0|1)`、`Ident`/`Str` の `true` / `false` / `on` / `off`（大文字小文字を区別しない）。`Str('f')` や `foo` は 42601 `<option> requires a Boolean value`（【確認】`allow_connections 'f'`）。
- 整数: `Int(n)` で `i32` に収まるもの。`Str('3')`、`Float`、`i32` を超える値は 42601 `connection_limit requires an integer value`（【確認】）。

| オプション | 受理する値 | それ以外 |
|---|---|---|
| `TEMPLATE` | 名前、`DEFAULT`（= template1） | — |
| `OWNER` | 名前、`DEFAULT`（= 実行したロール） | — |
| `ENCODING` | `UTF8`・`utf8`・`UTF-8`・`UNICODE`（大文字小文字を区別しない）、整数 `6`、`DEFAULT`（【確認】PostgreSQL は `unicode`・`utf-8`・`6` を受け付ける。Rails の `ENCODING = 'unicode'`、Django の `ENCODING 'UTF8'` が通る） | **0A000** `encoding "x" is not supported` / `encoding code 99 is not supported`（PostgreSQL は未知の名前に 42704 `x is not a valid encoding name`、別のエンコーディングには成功する。差は M5-DB-Q7） |
| `LC_COLLATE` / `LC_CTYPE` / `LOCALE` | `C`・`POSIX`・空文字列・`DEFAULT`。保存値は `C`（【確認】`POSIX` は `C` として保存される） | **42809** `invalid LC_COLLATE locale name: "x"`（`LC_CTYPE` / `LOCALE`（メッセージは `LC_COLLATE`）。PostgreSQL は OS に無いロケールに同じ SQLSTATE。HINT は付けない（ICU は無い）） |
| `LOCALE_PROVIDER` | `libc` | `icu` / `builtin` → **0A000** `locale provider "icu" is not supported`。未知 → **42P17** `unrecognized locale provider: x`（【確認】） |
| `ICU_LOCALE` / `ICU_RULES` / `BUILTIN_LOCALE` / `COLLATION_VERSION` / `OID` | — | **0A000** `option "x" is not supported` |
| `STRATEGY` | `wal_log`・`file_copy`（大文字小文字を区別しない。どちらも FILE_COPY で実行。D30） | **22023** `invalid create database strategy "x"`、HINT `Valid strategies are "wal_log" and "file_copy".`（【確認】） |
| `TABLESPACE` | `pg_default`、`DEFAULT` | **42704** `tablespace "x" does not exist`（`pg_global` も同じ。PostgreSQL は 22023 `cannot assign new default tablespace "pg_global"`。差は些細） |
| `ALLOW_CONNECTIONS` | 真偽 | 42601 |
| `CONNECTION LIMIT` | `-1` 以上の整数 | `< -1` → **22023** `invalid connection limit: -5`（【確認】）。整数でない → 42601 |
| `IS_TEMPLATE` | 真偽 | 42601 |
| 同じオプションの重複 | — | **42601** `conflicting or redundant options`（位置つき。【確認】） |
| 未知のオプション名 | — | **42601** `option "foo" not recognized`（位置つき。【確認】） |

- `ALTER DATABASE` は `ALLOW_CONNECTIONS` / `CONNECTION LIMIT` / `IS_TEMPLATE` だけ。それ以外の名前は 42601 `option "x" not recognized`（【記憶】）。`AlterDatabaseAction::Unsupported(what)` は **0A000** `ALTER DATABASE ... {what} is not supported yet`。
- `DROP DATABASE ... WITH (FORCE)` は **0A000** `DROP DATABASE ... WITH (FORCE) is not supported yet`。
- PostgreSQL は検査を「所有者 → 接続数 → 権限 → テンプレートの存在 → コピーの権限 → STRATEGY → ロケール → エンコーディング → 重複 → 使用中」の順に交互に行う（【確認】LOCATION の行番号）。yuzhu は値の検証を先にまとめて行うので、**複数のエラーが重なったとき**（例: 権限が無く、かつ STRATEGY が不正）だけ返るエラーが違う（既知の差。M5-DB-Q10）。共有テストには入れない。

### 6.3 権限

GRANT / REVOKE が無い M5 では、ロールの属性だけで判定する（調査 §5.2 と同じ考え方）:

| 操作 | 条件 | エラー |
|---|---|---|
| CREATE DATABASE | superuser または `create_db` | 42501 `permission denied to create database`（【確認】） |
| OWNER に自分以外を指定 | superuser だけ | 42501 `must be able to SET ROLE "x"`（【確認】PG17。M5 にロールの所属は無いので「自分以外」の判定だけ） |
| テンプレートが `datistemplate = false` | superuser か、そのテンプレートの所有者 | 42501 `permission denied to copy database "x"`（【確認】） |
| DROP / ALTER DATABASE | superuser か、その DB の所有者 | 42501 `must be owner of database x`（【確認】） |
| 接続（`CONNECT` 権限） | 検査しない（M6 の GRANT と一緒） | — |

役割の判定は AU が作る関数に集める（調査の `require_role_attr` 案）。DB 章は `ctx.role.superuser` / `ctx.role.create_db` / `ctx.role.oid` を直接読む（`RoleRow` の 3 項目は AU が足す）。

### 6.4 エラーの一覧（固定）

00 §3.8 に無い文言はこの表が固定する（§11 の依頼 11）。【確認】は PostgreSQL 17.11 の実機。

| 状況 | SQLSTATE | メッセージ（DETAIL） | 出る場所 |
|---|---|---|---|
| ロールが無い | 28000 | `role "x" does not exist`（FATAL） | connect |
| ロールがログイン不可 | 28000 | `role "x" is not permitted to log in`（FATAL） | connect |
| ロールの接続数 | 53300 | `too many connections for role "x"`（FATAL） | connect |
| DB が無い | 3D000 | `database "x" does not exist`（FATAL。待っている間に消えたときは DETAIL `It seems to have just been dropped or renamed.`） | connect |
| DB が接続を受けない | 55000 | `database "x" is not currently accepting connections`（FATAL） | connect |
| DB の接続数 | 53300 | `too many connections for database "x"`（FATAL） | connect |
| テンプレートが無い | 3D000 | `template database "x" does not exist` | CREATE |
| DB の重複 | 42P04 | `database "x" already exists` | CREATE |
| テンプレートの使用中 | 55006 | `source database "x" is being accessed by other users`（DETAIL §4.3） | CREATE |
| 対象の使用中 | 55006 | `database "x" is being accessed by other users`（DETAIL §4.3） | DROP |
| 現在の DB | 55006 | `cannot drop the currently open database` | DROP |
| テンプレートの DROP | 42809 | `cannot drop a template database` | DROP |
| DB が無い | 3D000 | `database "x" does not exist`（`IF EXISTS` は NOTICE `database "x" does not exist, skipping`。SQLSTATE 00000） | DROP / ALTER |
| ブロック内 | 25001 | `CREATE DATABASE cannot run inside a transaction block` / `DROP DATABASE cannot run inside a transaction block` | session |
| 権限 | 42501 | §6.3 の 4 つ | CREATE / DROP / ALTER |
| OWNER が無い | 42704 | `role "x" does not exist` | CREATE |
| オプション | §6.2 | §6.2 | analyzer |
| 更新競合 | XX000 | `tuple concurrently updated`（00 §3.8） | DROP / ALTER |
| `yz_datxid` に行が無い | XX001 | `yz_datxid has no row for database %u` | CREATE |

### 6.5 `datfrozenxid` と `yz_datxid` の分担（VC との境界）

| 操作 | 持ち主 | 内容 |
|---|---|---|
| 行の作成（initdb、CREATE DATABASE） | initdb の行: F0（`rows.rs`。値は 03 の規則）。CREATE DATABASE: DB が `pg_database` の行と同じ `WriteCtx` で VC の `insert_datxid` を呼ぶ | 値は **initdb: template1・postgres = 3、template0 = 番兵。CREATE DATABASE: `min(テンプレートの行の値, oldest_xmin())`**（03 VC-D13）。`pg_database.datfrozenxid` は下位 32 ビット |
| 行の削除（DROP DATABASE） | DB（VC の `delete_datxid` を呼ぶ） | `pg_database` の行と同じトランザクション |
| 値を進める | VC | `store_vac.rs` の `set_datxid_inplace`（HEAP2 INPLACE。`pg_database.datfrozenxid` の鏡も同時に）を呼ぶ。呼び出しは、`LockManager` の `frozen_lock_tag(oid)` を Exclusive（トランザクションスコープ）で取ってから（同じ DB の VACUUM 同士の更新競合を避ける。PostgreSQL の `LockDatabaseFrozenIds`）。DB は `frozen_lock_tag` を提供する |
| 全 DB の最小値の計算、clog の切り詰め | VC | `min_datxid` で読む。**自分が居る DB の行だけを進め、他の DB の行は読むだけ**（PostgreSQL の `vac_update_datfrozenxid` と同じ） |
| template0 の境界の進め方 | VC（VC-D13。M5-DB-Q8 は解決） | 番兵。§3.3 |

- 新しい DB の行が `yz_datxid` に入る前に clog の切り詰めが走っても安全である理由: 切り詰めの境界は全 DB の最小値で、新しい DB のページはテンプレートのコピー（テンプレートの行の値以上の XID しか含まない。テンプレートが番兵の template0 なら、clog を引く XID を含まない）。新しい行の値は `min(テンプレートの行の値, oldest_xmin())` なので、どちらのテンプレートでも新しい DB のページの XID を守る。コミットまでテンプレートの行は残るので、境界はテンプレートの値以下になる。DROP は境界を上げる方向にしか効かない。
- CREATE DATABASE と VACUUM の競合: テンプレートは DB-D6 のロックで VACUUM から守られる（VACUUM を動かすセッションは `Database(oid)` の AccessShare を持つ。**autovacuum（VC-6）のワーカーも、DB を触る前に `dbase::HeldLock::acquire(Database(oid), AccessShare, ..)` を取ること**。接続の上限には数えない）。

### 6.6 `DatabaseHandle` のキャッシュ

- キャッシュのキーは DB の OID。作るのは `connect` と `database_handle` だけ（§4.1）。`DatabaseHandle.name` は作った時点の名前（M5 に RENAME は無い。手順 5 の取り直しで名前の不一致を検出する）。
- `forget_database(oid)` は DROP の手順 12 だけが呼ぶ。キャッシュから外すだけで、`CatalogCache` を明示的に捨てる処理は要らない（`Arc` の最後の参照で落ちる）。OID は再利用されない（カウンタ。DB-D17）ので、古いハンドルが別の DB を指すことはない。
- `Cluster::invalidate_all_catalog_caches()`（M3 §5.3）は `databases` の全ハンドルを対象にする。CREATE / DROP DATABASE は他の DB のカタログを変えないので、呼ばない。`pg_database` / `yz_datxid` の読み取りにキャッシュは無い。

### 6.7 他の章との境界

| 相手 | 内容 |
|---|---|
| LK（01） | `LockManager::try_acquire` / `acquire` / `release_all`（Session スコープ）と `BackendRegistry` の `register` / `count_in_db` / `count_of_role` を使う。**前提**: (1) 同じバックエンドのロックは互いに衝突しない、(2) `try_acquire` は待ち行列の先行者を飛び越さずに取れるときだけ成功する（取れなければ false）、(3) `release_all(id, Session)` は全スコープを解放し、`unregister_backend` は解放済みの後に呼べる、(4) `BackendGuard` の Drop が `LockManager` からの登録解除も行う場合は、`SessionLocks::drop` の `unregister_backend` を外す（冪等であればそのままでよい）。D10（文の実行が `statement_barrier` を持たない）が入っていること（DB-2 の依存。§8） |
| AU（08） | `lookup_role`（認証の前）と `connect` の 28000 / 53300 の分担（DB-D2）。`RoleRow` に `create_db`・`conn_limit` を足す。`databases_owned_by` を DROP ROLE の依存検査に提供。接続のたびの `valid_until` 失効は AU の認証で扱う（`connect` は見ない）。サーバは `InterruptFlag` を `Session::new` の前に登録する（§11 の依頼 2） |
| VC（03） | §6.5。`TRUNCATE` や VACUUM が `Database(oid)` のロックを必要とする場面は無い（セッションが既に持っている） |
| XQ（04） | `CREATE DATABASE` / `DROP DATABASE` は Extended Query の `Execute` でも動くこと（`outside_block` は、暗黙のトランザクションで Sync までに BEGIN が無ければ真。XQ-2 が `execute_standalone` に振り分ける）。PREPARE は不可（PostgreSQL も不可。ユーティリティ文は PREPARE できない） |
| TS（10） | 共通の `TestCluster` / 分離性ランナー / `tests/restart` のランナーの上で §7 のテストを動かす。この章の共有テストが「既知の差」を出さないことの確認 |

---

## 7. テスト

### 7.1 共有テスト（`tests/slt/m5/database/`。PostgreSQL 17 でも通ること）

テストの DB 名は `m5db_` で始め、各ファイルの先頭で `DROP DATABASE IF EXISTS` を流し、末尾で消す（M2-Q22 の教訓）。ランナーは 1 ファイルを 1 つの DB（`postgres`）に流し、既定の接続は他の接続が無いので、`template1` / `postgres` を使う `CREATE DATABASE` は成功する（`m5db_in_use.slt` を除く）。

| ファイル | 内容 |
|---|---|
| `create_drop.slt` | `CREATE DATABASE m5db_a` → `pg_database` の行（`\l` と同じ問い合わせ。下の「`\l` の問い合わせ」）→ `DROP DATABASE m5db_a` → 行が無い。`DROP DATABASE IF EXISTS`（存在しない）は `statement ok`（NOTICE）。タグの確認は slt に出ないので Rust のテストで |
| `create_options.slt` | `WITH`・`=`・`DEFAULT` の各形、`TEMPLATE template0` / `template1`（文字列・識別子）、`OWNER postgres`、`ENCODING 'UTF8'` / `'utf8'` / `'UNICODE'` / `6`、`LC_COLLATE 'C' LC_CTYPE 'C'`、`LOCALE 'C'`、`STRATEGY wal_log` / `file_copy`、`TABLESPACE pg_default`、`CONNECTION LIMIT 5`、`ALLOW_CONNECTIONS false`、`IS_TEMPLATE true`（→ 行の列を確認 → `ALTER DATABASE .. IS_TEMPLATE false` → DROP）。**入れない**: `ENCODING 'SQL_ASCII'`、`LC_COLLATE 'en_US.UTF-8'`（OS に依存）、`C.UTF-8`、`COLLATION_VERSION`、`OID` |
| `create_errors.slt` | `42P04`（重複）、`3D000`（`TEMPLATE nonexistent`）、`22023`（`STRATEGY foo`、`CONNECTION LIMIT -5`）、`42601`（重複オプション `conflicting or redundant options`、未知のオプション、`CONNECTION LIMIT '3'`、`ALLOW_CONNECTIONS 'f'`）、`42704`（`OWNER nobody`）、`25001`（`BEGIN; CREATE DATABASE ..` → `ROLLBACK`） |
| `drop_errors.slt` | `3D000`、`55006`（`DROP DATABASE postgres`＝現在の DB）、`42809`（`DROP DATABASE template1`）、`25001`（ブロック内）。`WITH (FORCE)` は **yuzhu だけ**（`onlyif yuzhu` で 0A000。PostgreSQL は成功するので別ファイルにしない） |
| `alter_database.slt` | `ALLOW_CONNECTIONS` / `CONNECTION LIMIT` / `IS_TEMPLATE` が `pg_database` の列に反映される。ブロック内の `ALTER DATABASE` を `ROLLBACK` すると戻る。`3D000`（存在しない DB）。他の接続（`connection other`）からの見え方（コミット前は旧値、後は新値） |
| `in_use.slt` | `connection other` で `postgres` に接続したまま、既定の接続から `CREATE DATABASE m5db_x TEMPLATE postgres` が `55006`（PostgreSQL は 5 秒待つので、このファイルは遅い。`slow` として CI の別ジョブでもよい）。`connection other` を閉じた後は成功する |
| `catalog_l.slt` | `\l` の問い合わせ（下）の結果を、`WHERE d.datname LIKE 'm5db\_%'` で絞って確認する（所有者 `postgres`、`UTF8`、`libc`、`C`、`C`、`NULL`、`NULL`、`NULL` の ACL）。`pg_database` の `datconnlimit`・`datistemplate`・`datallowconn` の列。`oid` が 16384 以上でテンプレートより大きいこと |

**`\l` の問い合わせ**（【確認】psql 17 の `-E` の出力。M2 §1.1 の `\l` と同じ。`ORDER BY 1` に絞り込みを足す）:

```sql
SELECT d.datname AS "Name",
       pg_catalog.pg_get_userbyid(d.datdba) AS "Owner",
       pg_catalog.pg_encoding_to_char(d.encoding) AS "Encoding",
       CASE d.datlocprovider WHEN 'b' THEN 'builtin' WHEN 'c' THEN 'libc' WHEN 'i' THEN 'icu' END AS "Locale Provider",
       d.datcollate AS "Collate", d.datctype AS "Ctype", d.datlocale AS "Locale", d.daticurules AS "ICU Rules",
       CASE WHEN pg_catalog.array_length(d.datacl, 1) = 0 THEN '(none)'
            ELSE pg_catalog.array_to_string(d.datacl, E'\n') END AS "Access privileges"
FROM pg_catalog.pg_database d
WHERE d.datname LIKE 'm5db\_%'
ORDER BY 1;
```

### 7.2 再起動・クラッシュをまたぐテスト（`tests/restart/m5/database/`）

M3 の `tests/restart/m3/<シナリオ>/NN-*.slt`（`--restart` と `--crash`）と同じ形。PostgreSQL でも通る内容。

| シナリオ | フェーズ |
|---|---|
| `01-create-survives` | 1: `CREATE DATABASE m5r_a` と `m5r_b`、`m5r_b` を DROP。2（再起動 / `kill -9`）: `pg_database` に `m5r_a` だけある（`datconnlimit` などの列も）。`m5r_b` を作り直せる。`m5r_a` を DROP。3: どちらも無い |
| `02-drop-then-recreate` | 同名を DROP → CREATE → DROP → CREATE を繰り返してから再起動（OID が毎回変わること: `SELECT oid FROM pg_database` の比較は各フェーズ内で） |
| `03-alter-flags` | `ALTER DATABASE` の 3 つのフラグが再起動後も残る |
| `04-orphan-directory`（yuzhu のみ） | フェーズ 1 の後、`NN-*.after.sh` が `base/99999/`（空のディレクトリ）と `base/99998/YUZHU_VERSION` を作る。フェーズ 2 でサーバが起動し（WARNING。ログに `has no row in pg_database` が出る）、`CREATE DATABASE` が成功する。ディレクトリは消えていない |

### 7.3 Rust のテスト

**`yuzhu-core` の単体テスト**（`SimVfs`・`TestCluster`）:

- `dbase.rs`: `DbaseRecord` の encode / decode と §3.1 の固定値の往復、info の下位 4 ビットが立っていれば Panic、長さ違いは Panic。`name_lock_tag` が決定的で、`frozen_lock_tag` と衝突しない。`find_orphans`（純関数）。REDO: `CreateFileCopy` は dst が無い・不完全・完全のどれからでも同じ結果（バイト一致）、src が無ければ WARNING で戻り dst に触れない、`Drop` はディレクトリがあってもなくても成功し、`InvalidPages` の該当エントリを消す。`HeldLock` / `SessionLocks` の Drop がロックを残さない。
- `catalog/store_db.rs`: `insert_database` → `database_by_name` / `by_oid` / `databases`、2 行（pg_database と yz_datxid）が同時に入る・消える、`delete_database` の更新競合（`XX000`）、`update_database_flags`、`databases_owned_by`、（`yz_datxid` の読み書きと `set_datxid_inplace` の試験は 03 §7 の VC の担当。DB は `insert_database` / `delete_database` が `insert_datxid` / `delete_datxid` を呼ぶことと、CREATE DATABASE の `min(.., oldest_xmin())` を試験する）。
- `datadir.rs`: `copy_database_dir_checked` が `check` の Err でコピーを中断し dst を消す。
- `engine.rs` / `testing.rs` の `TestCluster`: `connect` の検査順（DB-D1 の 8 通りの組: 存在しないロール × 存在しない DB、NOLOGIN × template0 など）、接続数の上限（DB、ロール、superuser の免除、`CONNECTION LIMIT 0`）、`datallowconn`、ロックの解放（`Session` を Drop すると `lock_status` に `database` ロックが残らない）、`database_handle` / `forget_database`。

**`yuzhu-server/tests/database.rs`**（`Server` を同じプロセスで動かし `postgres` クレートで接続する。M3 `server.rs` と同じ作り。環境変数 `YUZHU_TEST_ADDR` を与えれば本物の PostgreSQL に向けられる部分は向ける）:

| テスト | 内容 |
|---|---|
| `create_and_connect` | `CREATE DATABASE`（`batch_execute` と Extended Query の `execute` の両方）→ 新しい DB に接続し `current_database()` を確認 → 表を作って INSERT → 元の DB に同名の表が無いこと |
| `new_database_is_isolated` | 2 つの DB に同名・別内容の表を作り、読み書きが混ざらない（バッファの `BufferTag` が DB ごとであること）。両方で `\d` 相当の `pg_class` の問い合わせが自分の表だけを返す |
| `template_content_is_copied` | template1 に表と行を作る → `CREATE DATABASE` → 新しい DB にその表と行がある。テンプレートの未チェックポイントの変更もコピーに含まれる（コピー前のチェックポイントの確認。`skip_pre_copy_checkpoint` を入れると落ちる）。後始末で template1 の表を消す |
| `template_in_use` | template1 に接続したままの別のセッションがあると `CREATE DATABASE` が `database_busy_wait`（このテストでは 300 ms）の後に 55006、DETAIL `There is 1 other session using the database.`。2 本なら複数形 |
| `drop_errors` | 存在しない（3D000、`IF EXISTS` は NOTICE と `DROP DATABASE` のタグ）、現在の DB、template0 / template1（42809）、他のセッションが居る（55006、DETAIL）、ブロック内（25001）、複数文の Query（25001）、`WITH (FORCE)`（0A000） |
| `drop_after_disconnect_races` | クライアントが接続を閉じた直後に別接続から `DROP DATABASE` を流す。`database_busy_wait` の間に成功する（これを 100 回繰り返しても失敗しない） |
| `connection_limits` | `CONNECTION LIMIT 1` / `0`、ロールの `CONNECTION LIMIT`、superuser の免除、超過で FATAL 53300 と文言、切断後に再接続できる |
| `allow_connections_false` | `ALLOW_CONNECTIONS false` と template0 が 55000（superuser でも） |
| `role_and_database_precedence` | DB-D1 の順序（存在しないロール＋存在しない DB → 28000、上限のロール＋存在しない DB → 53300、NOLOGIN ＋ template0 → 28000） |
| `connect_waits_during_create` | `SimVfs` の `FaultEffect::Delay`（`base/` の書き込みに 5 ms）で `CREATE DATABASE` を遅くし、その間に template1 へ別のスレッドが接続する。接続は CREATE の完了まで待たされ、完了後に成功する（接続の完了時刻 ≥ CREATE の完了時刻）。新しい DB への接続も同じ |
| `connect_to_dropped_database` | `DROP DATABASE` を（`FaultOp::Remove` の遅延で）遅くし、その間に対象への接続を始める。DROP の完了後に 3D000 と DETAIL `It seems to have just been dropped or renamed.` |
| `cancel_create_database` | コピーの遅延中にキャンセル（`CancelRequest`）→ 57014、`base/<新>/` が無く、同名で作り直せる |
| `restart_keeps_databases` | CREATE → サーバを再起動 → 接続して表のデータが読める。DROP → 再起動 → 接続は 3D000 |
| `permissions` | ロールを作り（AU-3 の後）、`CREATEDB` なしで 42501、`CREATEDB` ありで成功、他人の DB の DROP / ALTER が 42501、`OWNER` に他人を指定すると 42501。所有者のロールは DROP ROLE できない（2BP01、AU 章と共同） |
| `orphan_warning` | 起動前に `base/` に未登録のディレクトリを置いて起動し、WARNING が出る（ログを `tracing` のテスト用 subscriber で確認）。接続・CREATE は成功する |

**M5 の共通部品**: `TestCluster` に `database_busy_wait` と複数 `Session` を作る関数を足す（TS）。

### 7.4 クラッシュ試験（DB-4。`yuzhu-core/tests/crash_sim/database.rs`。M3 §7.5 の層 1 の仕組みに載せる）

**仕組み**: `TestCluster`（`SimVfs`、`background_checkpointer = false`、`shared_buffers = 16`、`wal_segment_size = 2 MiB`）。`database_busy_wait` は 0。ワークロードは 1 スレッドで複数の `Session` を順番に操作する（プロトコルを通さない）。

**ワークロード**（それぞれ「DB-X の文を実行している間の I/O」に `FaultRule { op: Any, nth: Some(N), effect: CrashFreeze }` を仕掛け、全 N を `DropUnsynced` と `KeepAll` の両方で網羅する。M3 §7.5 と同じ。大きいものはシードで `RandomSubset` / `TornSectors`）:

| 名前 | 内容 |
|---|---|
| `W-create` | template1 に表と行（`INSERT` を数十行、チェックポイント前の dirty を含める）を作り、`CREATE DATABASE x`。その後 `x` に接続して追記 |
| `W-create-from-user-db` | `CREATE DATABASE a` → `a` に表 → `CREATE DATABASE b TEMPLATE a`（`a` は `IS_TEMPLATE true`。コピー元がユーザー DB） |
| `W-drop` | `CREATE DATABASE x`・`x` に表と行 → `DROP DATABASE x` |
| `W-create-drop-create` | `CREATE x` → `DROP x` → `CREATE x`（同名・別 OID）。各ステップの前後にクラッシュ点を置く |
| `W-concurrent-other-db` | `postgres` DB で銀行振込（M3 のワークロード 1。合計一定）を続けながら、別のセッションが `CREATE` / `DROP DATABASE` を繰り返す（他の DB のデータが無傷であること。I-DB6） |
| `W-redo-crash` | 回復の最中（REDO の `CreateFileCopy` のコピーの途中）にもう一度クラッシュ（二重クラッシュ。`FaultRule` を回復の I/O に仕掛ける）。M3 のワークロード 06-double-crash と同じ作り |

**不変条件**（M3 の I1〜I12 に加えて。各ワークロードの後に検査する。`invariants.rs` に `check_databases` を足す）:

| ID | 内容 |
|---|---|
| I-DB1 | `pg_database` の各行に `yz_datxid` の行がちょうど 1 つあり、逆も成り立つ |
| I-DB2 | `pg_database` の各行について `base/<oid>/` が存在し、`pg_class` に載るリレーションのファイルがすべて存在する。`datallowconn` の DB に `Session` で接続でき、`pg_class` を読める |
| I-DB3 | 行の無い `base/<oid>/` は、§5.3 / §5.5 の状態表が許す状態（CREATE のコミット前・DROP のコミット後でディレクトリ削除前）のときだけ存在する。それ以外では存在しない（オラクルが「確定した CREATE / DROP」と「不明」を記録する） |
| I-DB4 | 確定した CREATE（`Ok` で戻ったもの）の行は残り、確定した DROP の行は無い。「不明」な文（クラッシュで戻ったもの）は、行あり・行なしのどちらも許すが、**行ありなら I-DB2 と I-DB5 を満たす** |
| I-DB5 | CREATE が確定した DB の内容は、コピー時点のテンプレートの内容と一致する（ユーザー表の行集合と `pg_class` の `relname` の集合をテンプレートの記録と比べる） |
| I-DB6 | 他の DB（銀行振込のモデル）の内容が、確定したトランザクションだけを反映した状態と一致する |
| I-DB7 | 回復後に、孤児があっても同名の DB を `CREATE DATABASE` できる。新しい OID は既存の行・ディレクトリのどれとも重ならない |
| I-DB8 | REDO の冪等: 同じクラッシュ後のディスクに対して、回復を途中で止めてもう一度回復した結果が、一度で回復した結果と同じ（`W-redo-crash`） |
| I-DB9 | 回復後の起動が、孤児の存在で失敗しない（WARNING だけ） |

**クラッシュ点の網羅**: 全 N の掃引に加え、次の名前つきの点は、通常実行で各手順の境界の I/O 番号を記録（`SimVfs::io_count()` をワークロードのフック位置で読む）して必ず試す: (1) 手順 8 のチェックポイントの直前・直後、(2) 最初のファイルのコピーの前後、(3) 最後のファイルの `sync` と `base/<new>` の `sync_dir` の間、(4) `DBASE_CREATE` の挿入と `pg_database` の挿入の間、(5) コミットレコードの `flush` の前後、(6) 最後のチェックポイントの前後（REDO 点の決定の前後、制御ファイルの更新の前後）、(7) DROP のコミットの前後、(8) `DBASE_DROP` の flush の前後、(9) `remove_dir_all` の最初のファイルの削除の後。

**変異テスト**（`mutation.rs`。ハーネスが壊れを検出できること。M3 §7.5 と同じく「既定のシード集合のうち少なくとも 1 つで検出する」）:

| 変異 | 方法 | 検出されるべき不変条件 |
|---|---|---|
| コピー前のチェックポイントなし | `skip_pre_copy_checkpoint`（クラッシュは不要） | I-DB5（テンプレートの最近の変更が新 DB に無い） |
| コピーの fsync なし | `skip_copy_fsync` + `DropUnsynced` | **検出されない**（`DBASE_CREATE` の REDO が修復する。これを肯定的なテストにする: 常に I-DB2・I-DB5 が成り立つ） |
| コピーの fsync なし + `DBASE_CREATE` なし | `skip_copy_fsync` + `skip_dbase_wal` + `DropUnsynced`（コミット後・最後のチェックポイント前のクラッシュ） | I-DB2 か I-DB5（ファイルが無い・空） |
| ディレクトリの削除をコミットの前に行う | `drop_dir_before_commit` + コミット前のクラッシュ | I-DB2（行はあるがディレクトリが無い） |
| `sync_dir` なし | `SimVfs::set_ignore_sync_dir(true)` + `DropUnsynced` | I-DB2（新しい DB のディレクトリが消える） |
| REDO の `invalid.forget_database` なし（実装を一時的に外す） | `W-drop` で `DBASE_DROP` より前のレコードがある状態でクラッシュ | 起動の失敗（`XX001 WAL contains references to invalid pages`） |

---

## 8. 実装の分担と工数

担当は **DB**（1 人）。00 §7 の WP の ID を使う。日数は AI の実装エージェント 1 本の日数（粗い見積もり）。

| ID | 内容 | 依存 | 日数 |
|---|---|---|---|
| **DB-1** | `Database(oid)` のセッションスコープ（`SessionLocks`・`HeldLock`・`lock_database_exclusive`）、`Cluster::connect` の置き換え（検査順 DB-D1、接続数 DB-D3）、`database_handle` / `forget_database`、`DatabaseRow` の項目と `store_db.rs` の読み取り側（`database_by_oid`・`databases`）、起動時の孤児検査、`ClusterOptions.database_busy_wait`、単体テスト | LK-1（`LockManager`）、LK-3 のうち `BackendRegistry`（小さいので先に切り出してもらう）、F0 | 2 |
| **DB-2** | `CREATE DATABASE`: パーサ・アナライザ（オプションの検証）、`store_db.rs` の書き込み側（`yz_datxid` の行は VC の `insert_datxid` を呼ぶ。schema・initdb の行は F0b-1 / VC-4。R-08）、所有者ロールの `lock_role_shared`（R-16）、`dbase.rs`（DBASE レコード・REDO・`CreatedDirGuard`・`copy_database_dir_checked`）、`ddl/database.rs::create_database`、`get_new_database_oid`、失敗時の後始末、共有テスト `create_*.slt`・`catalog_l.slt`、単体テスト | DB-1、F0（F0b-1・F0b-2。`execute_standalone`・`TxnControl`・`DdlCtx`・`BoundDdl` の変種・`RmgrId::Dbase`）、**LK-3（D10: 文の実行が `statement_barrier` を持たない。M3 の実装のままだと `CHECKPOINT` と同じく、文の中で `cluster.checkpoint()` を呼ぶと自分のバリアで待つ。LK-3 が入るまでは M3 の `CHECKPOINT` 文の経路と同じく「バリアを取る前」に処理する仮の入口で動かす）**、AU-3（`RoleRow` の `create_db`・`conn_limit`、ロールの名前引き） | 4 |
| **DB-3** | `DROP DATABASE`（手順・`DBASE_DROP`・バッファ・smgr の破棄・孤児）、`ALTER DATABASE`（3 オプション。+0.5）、`drop_database_buffers` などの他の持ち主への依頼の取りまとめ、共有テスト `drop_errors.slt`・`alter_database.slt`・`in_use.slt` | DB-1、DB-2 | 2.5 |
| **DB-4** | クラッシュ試験（§7.4）: ワークロード 6 本、不変条件 I-DB1〜I-DB9、名前つきクラッシュ点、変異テスト、再起動テスト（§7.2）、`yuzhu-server/tests/database.rs` の統合テスト（§7.3）の残り | DB-2、DB-3、TS-3（クラッシュ試験の基盤）、LK-3 | 2 |

合計 **10.5 日**（00 の見積もりの 10 日に ALTER DATABASE の +0.5 日）。DB-1 は LK-1 の完了後すぐ始められる。DB-2 の SQL 側（パーサ・アナライザ・`store_db.rs`・`dbase.rs` の単体）は F0 の後、LK-3 を待たずに進められる。クリティカルパスには載らない（00 §1.3）。

**ファイルの持ち主**（00 §8 のとおり）: `dbase.rs`、`ddl/database.rs`、`analyzer/ddl_ext/database.rs`、`sql/parser/database.rs`、`catalog/store_db.rs`、`engine.rs` のデータベース登録（`connect`・`database_handle`・`forget_database`・孤児検査）、`datadir.rs` のコピー。`yz_datxid` の `schema.rs` の定義と `rows.rs` の initdb の行は F0 が作り（03 C8。§11 の依頼 8。R-08）、読み書きの関数は VC の `store_vac.rs` が持つ。DB は `insert_database` / `delete_database` から呼ぶだけ。

---

## 9. 未検証の点（実装前に確かめること）

1. PostgreSQL の `CountOtherDBBackends`（`PG:src/backend/storage/ipc/procarray.c`）が 5 秒（50 回 × 100 ms）待つこと、その間 autovacuum のワーカーを `SIGTERM` で止めること【記憶】。5 秒は実機で動作を確認したが、ソースの定数は未確認。
2. DETAIL の複数形 `There are %d other sessions using the database.`（単数形は【確認】）。
3. PostgreSQL が最後のチェックポイントをコミットの前に置くこと（`PG:src/backend/commands/dbcommands.c` の `createdb` / `CreateDatabaseUsingFileCopy`）【記憶】。D30 と異なる（M5-DB-Q3）。
4. `LockManager::try_acquire` が待ち行列の先行者（待ち手）を考慮するか、`release_all(.., Session)` が全スコープを解放すること（LK 章で確認。§6.7 の前提 (2)(3)）。
5. `Vfs::remove_dir_all` の `SimVfs` での障害注入の粒度（再帰全体で 1 回の操作か、ファイルごとか）。§7.4 の名前つきクラッシュ点 (9) に影響する。ファイルごとの操作でなければ、削除の途中のクラッシュを作れない（`SimVfs` に「ディレクトリの全ファイルを列挙して 1 つずつ削除する」実装があるか確認）。
6. `flush_all_for_checkpoint` が「開始時点で dirty なページ」をすべて書くこと（M3 §5.6 の手順 4）。テンプレートのロックを取った後に開始するので、コピー前のチェックポイントでテンプレートの dirty ページが残らないはず。M3 の実装で確認する。`CheckpointKind::Explicit` が Periodic の「変更なしならスキップ」の対象外であること。
7. M2 の `CatalogStore::get_new_oid` は共有カタログを拒否する（`catalog/store.rs` の `!d.shared` の条件と `get_new_oid_needs_an_oid_column` のテスト。実装を読んで確認済み）。そのため `SharedCatalogStore::get_new_database_oid`（§4.2）を新設する。`MAX_OID_ATTEMPTS` と `FIRST_NORMAL_OBJECT_ID` は M2 の定数をそのまま使う。
8. `ALTER DATABASE` で受け付けないオプションを PostgreSQL が 42601 `option "x" not recognized` で拒否すること【記憶】。
9. 非 superuser が `OWNER = 自分` を指定したときに成功すること（【確認】`own1`・`own2` の 2 件で成功）。`OWNER` に別のロール（`SET ROLE` できる）の場合の挙動はロールの所属（M6）が要る。
10. `DROP DATABASE` の SQLSTATE `42809` と文言は実機で確認済み（テンプレート）。同じく「現在の DB」が先か「テンプレート」が先か（両方に当たる template1 に接続して `DROP DATABASE template1`）は、実機で `42809` が先であることを確認した。
11. PostgreSQL の「複数のエラーが重なったときの優先順位」の細部（§6.2 の最後）。
12. `Notice` の SQLSTATE `00000`（`DROP DATABASE IF EXISTS` の NOTICE。M2 のセッションの `Notice` の作り方に合わせる）。
13. 実機の `datfrozenxid` が全 DB で同じ（730）ことは、PostgreSQL が新しい DB にテンプレートの値をコピーする（`src_frozenxid`）ことと整合する。`PG:src/backend/commands/dbcommands.c` の読みは【記憶】。

---

## 10. 確認事項

仮決めのまま進める。変えたい場合の影響を添える。

| ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|
| M5-DB-Q1 | 接続の検査順は PostgreSQL 17 の順（ロール → ロールの接続数 → DB → ロック → 行の取り直し → 55000 → DB の接続数。DB-D1）。00 §4.8 の列挙と M2 §6.9.1 の順（DB → ロール）から変える | 実機で確認。存在しないロールと存在しない DB の組で結果が変わるだけ | M2 の順に戻すなら `connect` の手順 1 と 2 の入れ替え（テストの期待が変わる）。実利はほぼ無い |
| M5-DB-Q2 | CREATE / DROP DATABASE は他のセッションの退出を最長 5 秒（`database_busy_wait`）ポーリングで待つ（DB-D5）。D40 の `try_acquire` の即失敗を拡張 | 実機。ドライバのテストランナーが接続を閉じてすぐ DROP する | 即失敗にするなら `database_busy_wait = 0` にするだけ（コードは同じ）。接続を閉じた直後の DROP が不安定になる |
| M5-DB-Q3 | 最後のチェックポイントはコミットの後（D30 のまま）。失敗時は、コミット済みの DB が残ったままエラーを返し、REDO の窓（最後のチェックポイントの完了まで。その間にテンプレートが変更されてクラッシュすると、REDO のコピーが「後の」テンプレートを写す）が残る | D30 は決定済み。窓は 2 重の障害のときだけ | **最後のチェックポイントをコミットの前（手順 12 と 13 の間）に移す案**: チェックポイントの失敗でロールバックでき、PostgreSQL と同じになり、窓も閉じる。代償はほぼ無い（チェックポイントの後・コミットの前にクラッシュすると孤児が残るが、それは今の手順でもコミット前のクラッシュで起こる）。D30 の改訂を推奨するかは 00 の担当が決める。手順の順序の入れ替えだけで、REDO・テスト・状態表は 1 行ずつ変わる |
| M5-DB-Q4 | `ALTER DATABASE` は 3 つのフラグのオプションだけ実装する（DB-D14。+0.5 日）。`RENAME` / `OWNER` / `SET` などは 0A000 | `IS_TEMPLATE true` で作った DB を消す手段を残すため。`ALLOW_CONNECTIONS` と `CONNECTION LIMIT` は同じ更新関数で足りる | 入れないなら `IS_TEMPLATE true` を 0A000 にする（DB が undroppable になる罠を避ける）。00 §1.1 の範囲外の追加なので、不要なら DB-3 から −0.5 日 |
| M5-DB-Q5 | `DBASE_CREATE` の REDO で `src` が無ければ WARNING で飛ばす（DB-D8）。PostgreSQL は失敗（PANIC） | コピーは commit の前に fsync 済みで、`src` が先に DROP された窓（M5-DB-Q3 の窓と重なる）で起動不能にしないため | PANIC にすると、その窓の二重障害で起動できなくなる。`--no-sync` の initdb をした環境で `skip_copy_fsync` 相当（コピーが durable でない）のときは、`src` が無く `dst` も壊れていれば DB が壊れる |
| M5-DB-Q6 | `DBASE_DROP` の `xid` は 0（DB-D9）。コミットの後の独立したレコード | コミットの前に置くと、コミット前のクラッシュで REDO が生きた DB を消す | PostgreSQL のようにトランザクションの中（コミット前）に置くなら、PostgreSQL の「無効の印」（`datconnlimit = -2`）の方式が要る（調査 §7.2 の D32 の議論） |
| M5-DB-Q7 | `ENCODING` は UTF8 系（`UTF8`・`utf8`・`UTF-8`・`UNICODE`・`6`）だけ。他は 0A000（PostgreSQL は別のエンコーディングに成功し、未知の名前に 42704）。ロケールは `C` / `POSIX` / 空だけ（`POSIX` は `C` として保存）。他は 42809 | M1〜M5 が UTF8 と C ロケールしか扱わない（`LC_COLLATE` に応じた比較が無い） | `SQL_ASCII` を受け付けると、バイト列をそのまま通す型の扱いが要る（M6 以降） |
| M5-DB-Q8 | **解決済み（レビュー対応 R-08）**: template0 の `datfrozenxid8` は initdb で番兵 `DATFROZEN_PRISTINE`（i64::MAX。03 VC-D13）。CREATE DATABASE は `min(テンプレートの行の値, oldest_xmin())` で作る（番兵を継承しない）。以前の仮決め（initdb で 3、VC が決める）は廃止 | 接続できない DB は VACUUM が動かず、境界が永久に 3 のままになる | VC が template0 を除外せず境界に含めるなら、`template0` を内部の経路で VACUUM する仕組み（`datallowconn` を無視する autovacuum 相当）が要る。template0 から作った DB は値 3 のコピーで始まるので、その DB の全テーブルの VACUUM が一巡するまで clog の切り詰めは進まない（PostgreSQL も同じ。クラッシュ・正しさには影響しない） |
| M5-DB-Q9 | 孤児ディレクトリは消さず、起動のたびに WARNING（D31）。「行はあるがディレクトリが無い」も WARNING（診断だけ） | M3 D15 と同じ方針 | 消すなら、起動時に `base/<oid>` のうち行の無いものを `remove_dir_all`（調査 C-10 の案）。行が無いのは CREATE のコミット前のクラッシュか DROP のコミット後のクラッシュのいずれかで、どちらも消してよい状態だが、`pg_database` が壊れたときに生きたデータを消す危険がある |
| M5-DB-Q10 | オプションの値の検証をアナライザで先にまとめて行う。複数のエラーが重なったときだけ PostgreSQL と返るエラーが違う（§6.2） | 構造がシンプル（実行時にカタログを引くものと分離できる） | PostgreSQL の順序に合わせるなら、`BoundCreateDatabase` に「保留したエラー」を持たせて実行器の該当位置で投げる。複雑さに見合わない |
| M5-DB-Q11 | CREATE DATABASE は新しい DB の `Database(新 OID)` も最後のチェックポイントの後まで AccessExclusive で持つ（DB-D6）。PostgreSQL はコミット後すぐに接続を受ける | 「`CREATE DATABASE` が返れば、使えて永続している」を単純な規則にする | 外すなら手順 6 の `new_lock` を省く。コミット後・チェックポイント前の接続は動くが、REDO の窓（M5-DB-Q3）で新しい DB への書き込みと REDO のコピーが重なる（FPI で正しく回復する。§5.3 の根拠）。外す利点は、最後のチェックポイントの間に新しい DB に入れること |
| M5-DB-Q12 | **所有ロールの同時削除は塞ぐ（レビュー対応 R-16。以前の「検出しない」を改めた）**: `CREATE DATABASE ... OWNER r` は手順 1 で `catalog::roles::lock_role_shared`（08 C9。`Object { db: 0, class: 1260, obj: ロール OID }` の AccessShare）を取り、`DROP ROLE` は同じ OID を AccessExclusive で取る（08 AU-D13）。取った後でロールの存在を確かめ直す | 08 AU-D13・C9 が 09 の実装を前提にしていた。ロックは AU の関数 1 行で、+0.1 日（DB-2 に含める） | 取らないと、`DROP ROLE r` が所有者のいない DB を作る競合を許す（`pg_shdepend` が無いので `databases_owned_by` だけでは取りこぼす）。M4 の CREATE TABLE 側（08 C9）が未対応の間は既知の差（08 §9-17） |
| M5-DB-Q13 | autovacuum のワーカー（VC-6）は DB を触る前に `Database(oid)` の AccessShare を取り、接続の上限には数えない（§6.5） | CREATE / DROP DATABASE との排他を保つため | 取らないと、DROP / CREATE の最中に VACUUM が動き、ファイルを消した後のページ書き出しや、コピー中のテンプレートの変更が起こる。VC 章が実装時に従うこと |

**PostgreSQL との既知の差**（10 章が集める。共有テストには入れない）: (1) `ENCODING` / `LOCALE` の受理範囲（M5-DB-Q7）、(2) `DROP DATABASE ... WITH (FORCE)` が 0A000、(3) 最後のチェックポイントの位置（M5-DB-Q3）、(4) 複数のエラーが重なったときの優先順位（M5-DB-Q10）、(5) `STRATEGY = WAL_LOG` を FILE_COPY で実行、(6) 孤児ディレクトリを消さない（M5-DB-Q9）、(7) `OID` / `ICU_*` / `COLLATION_VERSION` / `LOCALE_PROVIDER icu` は 0A000、(8) `pg_global` を `TABLESPACE` に指定したときの SQLSTATE、(9) 新しい DB の接続が最後のチェックポイントの完了まで待たされる（M5-DB-Q11）、(10) 所有ロールの同時削除（M5-DB-Q12）。

---

## 11. 契約への変更依頼

00 を黙って変えない。次を 00（F0 と各持ち主）に依頼する。**変更でなく追加**のものと、**解釈の確認**のものを分ける。

**解釈・決定への依頼**

1. **00 §4.8 の検査順**: 「3D000 / 55000 / 28000 / 53300」は SQLSTATE の列挙であり検査順ではない、と解釈した。実際の順は §5.1（PostgreSQL 17 の順。M2 §6.9.1 の順から変更）。00 の本文にこの旨を 1 行足してほしい。
2. **`Session::new` と `InterruptFlag`**: `connect` は DB の `Database(oid)` ロックの取得で待ちうる（CREATE / DROP DATABASE の完了まで）。サーバの停止の通知が届くように、サーバ（`connection.rs`）は `Session::new` の**前**に `InterruptFlag` を作ってサーバの登録簿に登録し、`StartupParams`（または `Session::new` の引数）で渡す必要がある（今の M2 のサーバは `Session::new` の後に登録している）。`StartupParams` にも `client_addr: Option<IpAddr>` を足す（`BackendInfo`・`pg_locks` 用）。持ち主は AU（`connection.rs` の起動処理）と XQ。
3. **D40 の拡張**: 「`try_acquire`」を、最長 `database_busy_wait`（既定 5 秒）の 100 ms ポーリングにした（DB-D5）。D40 の文面に反映してほしい。
4. **WP の依存**: DB-2 に LK-3（D10）と AU-3（`RoleRow` の項目）を足す（§8）。00 §7 の表の DB-2 の依存は DB-1 だけ。DB-1 は LK-1 のほか `BackendRegistry`（LK-3 の一部）が要る。
5. **ALTER DATABASE の追加**（DB-D14。+0.5 日。M5-DB-Q4）。`Statement::AlterDatabase` / `BoundDdl::AlterDatabase`（`AlterDatabaseStmt` / `BoundAlterDatabase`）を F0 が足す。`BoundDdl::AlterDatabase` は `ddl::execute`（通常のトランザクション）。00 §1.1 の「対応しないもの」の「`ALTER TABLE` の大半」には含まれないが、`ALTER DATABASE` の記載が無い。

**00 の型への追加（互換。既存の署名は変えない）**

6. `ConnectGrant` に `session_locks: SessionLocks` を足す（フィールドの順序は §4.1）。`dbase::{SessionLocks, HeldLock}` を新設（LK の `BackendGuard` との役割分担は §6.7 の前提 (4)）。
7. `DatabaseRow` に `encoding`・`collate`・`ctype`・`frozen_xid`・`min_mxid` を足す（00 §4.8 は `conn_limit`・`owner` だけ）。`SharedCatalogStore` のフィールド（`storage`・`database` など）を `pub(in crate::catalog)` にし、`datxid: (Arc<TableDef>, RelHandle)` を足す（F0）。
8. **ファイルの持ち主**（レビュー対応 R-08 で改訂）: `yz_datxid`（OID 9802）の `catalog/schema.rs` の定義と `catalog/rows.rs` の initdb の行（値は 03 の規則: template1・postgres = 3、template0 = 番兵）は **F0 が作る**（03 C8 と同じ。00 §8 に反映した）。読み書きの関数は VC の `catalog/store_vac.rs`。DB はそれを呼ぶだけ。`ClusterOptions.database_busy_wait`（既定 5 秒）を足す。
9. **他の持ち主の型に関数を足す**（§4.6）: `BufferPool::drop_database_buffers`（`storage/buffer/mod.rs`。M5 の持ち主は VC）、`StorageManager::forget_database` と `vfs()`（`storage/smgr.rs`）、`InvalidPages::forget_database`（`wal/redo.rs`）、`wal/dump.rs` の DBASE の整形（`DBASE CREATE_FILE_COPY src=%u dst=%u`、`DBASE DROP db=%u`）。持ち主が決まっていない `smgr.rs`・`redo.rs`・`dump.rs` は F0 が足す。
10. `DebugKnobs` に 4 つのスイッチを足す（§4.7）。
11. **§3.8 の文言の追加**: §6.4 の表のうち 00 §3.8 に無いもの（`template database "x" does not exist`、`too many connections for role "x"` / `for database "x"`、`database "x" is not currently accepting connections`、`permission denied to create database` / `to copy database "x"`、`must be owner of database x`、`must be able to SET ROLE "x"`、`cannot drop a template database`（42809）、オプションの検証の文言（§6.2））。
12. **SQLSTATE の追加**: 00 §3.5 の一覧に `42809`（`WRONG_OBJECT_TYPE`）、`42501`、`42704`、`55000`、`3D000`、`28000` はいずれも `error.rs` に既にある。足りないのは `42P04`（`DUPLICATE_DATABASE`。00 §3.5 が F0 に割り当て済み）と `42P17`（`INVALID_OBJECT_DEFINITION`。`LOCALE_PROVIDER` 用。無ければ F0 が足す）だけ。

**他の章への依頼（参考）**

- **LK**: §6.7 の前提 (1)〜(4)。`Session::terminate` の順序（§4.1）。
- **AU**: `RoleRow` に `create_db`・`conn_limit`、`lookup_role` と `connect` の分担（DB-D2）、`databases_owned_by` の利用。
- **VC**: `store_vac.rs` の `set_datxid_inplace`・`min_datxid`・`insert_datxid`・`delete_datxid` の提供（§6.5。R-08）、template0 の扱い（番兵。M5-DB-Q8 は解決済み）、autovacuum のワーカーの `Database(oid)` ロック（M5-DB-Q13）。
- **XQ**: Extended Query の `Execute` で `CREATE DATABASE` / `DROP DATABASE` が `execute_standalone` を通ること（§6.7）。
- **TS**: `tests/slt/m5/database/` の追加（§7.1）、`tests/restart/m5/database/` と `NN-*.after.sh`（§7.2）、「既知の差」の集約（§10 の末尾）、`TestCluster` の拡張。
