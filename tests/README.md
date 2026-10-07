# tests/

言語非依存の共有テストスイート（[sqllogictest](https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki) 形式）と、
その実行スクリプトを置くディレクトリです。

- テストは PostgreSQL ワイヤプロトコル（Simple Query）経由で実行します。
  そのため、どの言語の yuzhu 実装に対しても、本物の PostgreSQL に対しても同じテストを流せます。
- **PostgreSQL 17（C ロケールで initdb したもの）が正解の基準**です。PostgreSQL で通らないテストは「期待値が間違っている」とみなし、置きません。

```
tests/
├── run.sh            スイートの実行スクリプト（--restart で再起動テスト、--crash で kill -9 のクラッシュテスト）
├── done-check.sh     M4 の完了判定（01 §3 の条件 1〜9）をローカルで一発実行し、合否を一覧で出す
├── pg.sh             検証用の PostgreSQL 17 コンテナを起動・停止・再起動（crash で kill -9）する
├── yuzhu.sh          yuzhu-server をテスト用に起動・停止・再起動（crash で kill -9）する
├── slt/
│   ├── m1/<機能>/*.slt   M1 の範囲のテスト（ddl, insert, constraints, select, expressions, types, functions, txn, session, errors）
│   ├── m2/<機能>/*.slt   M2 の範囲のテスト（dml, txn, ddl, catalog, types, psql）
│   ├── m3/<機能>/*.slt   M3 の範囲のテスト（txn, session, functions, checkpoint）
│   ├── m4/<機能>/*.slt   M4 の範囲のテスト（K1〜K4。KNOWN-DIFFS.md、z_final/ を含む）
│   └── _include/consistency.slt.part  カタログ整合の SQL の正本（slttools consistency がコピーする）
├── restart/
│   ├── <シナリオ>/NN-*.slt      再起動をまたぐテスト（--restart。フェーズごとにサーバを再起動する）
│   ├── m3/<シナリオ>/NN-*.slt   クラッシュ（kill -9）をまたぐテスト（--crash。yuzhu.only / yuzhu.args / NN-*.after.sh を置ける）
│   └── m4/<シナリオ>/NN-*.slt   M4 の再起動・クラッシュ（mode ファイルで restart | crash | both | mixed）
├── isolation/{specs,expected}    isolationtester 形式の spec と期待値
└── tools/
    ├── isolation/                spec のランナー（yuzhu-isolation）
    └── slttools/                slt の lint と consistency の同期（依存なしの Rust）
```

## 準備

ランナーは **sqllogictest-bin 0.29.1** を使います。`--locked` は必須です。付けないと tokio-postgres 0.7.15 以降で解決され、
エラーメッセージが `db error` だけになって、メッセージを照合するテストが壊れます（`spec/research/research-slt.md` §1.5）。

```sh
cargo install sqllogictest-bin --locked --version 0.29.1
```

## 実行方法

### 本物の PostgreSQL に対して（期待値の検証）

```sh
tests/pg.sh start                 # postgres:17 を 127.0.0.1:55432 で起動（コンテナ名 yuzhu-test-pg、trust 認証、C ロケール）
tests/run.sh --target pg          # tests/slt 以下（m1〜m3）をすべて実行
tests/pg.sh stop                  # コンテナを削除
# claude-sandbox のコンテナ内（docker なし）では tests/pg.sh の代わりに sandbox/pg.sh start|stop|restart|status を使う
```

`tests/pg.sh status` で状態を確認できます。ポートは `--port N` か環境変数 `PG_PORT` で変えられます。

### yuzhu に対して

```sh
cd impl/rust && cargo run --release -p yuzhu-server -- --port 5432 &
tests/run.sh --target yuzhu
```

### オプション

```
tests/run.sh --target pg|yuzhu [--host H] [--port N] [--user U] [--db D] [files or dirs...]
```

| オプション | 既定値 |
|---|---|
| `--host` | `127.0.0.1` |
| `--port` | pg なら `55432`、yuzhu なら `5432` |
| `--user` / `--db` | `postgres` / `postgres` |
| ファイル・ディレクトリ | `tests/slt`（m1〜m3。ディレクトリを渡すと、その下の `*.slt` をすべて実行） |
| `--restart` | 再起動テストを流す（下記）。ファイル・ディレクトリの既定は `tests/restart` |

- 例: `tests/run.sh --target yuzhu tests/slt/m1/select tests/slt/m1/ddl/create_table.slt`
- `--engine postgres`（Simple Query のみ）で実行し、`--label pg` または `--label yuzhu` を付けます。
- 環境変数 `SLT_BIN` でランナーのパスを、`SLT_EXTRA_ARGS` で追加の引数（例: `--fail-fast`）を渡せます。
- テストが途中で失敗すると、そのファイルのテーブルが残ることがあります。次の実行が `42P07` で失敗したら、
  PostgreSQL は `tests/pg.sh stop && tests/pg.sh start` で作り直し、yuzhu（M1 はメモリ上）は再起動してください。



### yuzhu の起動（M2 以降）

`tests/yuzhu.sh` が `yuzhu-initdb -U postgres --no-sync` と `yuzhu-server -D` をまとめて扱います（`pg.sh` と同じ形）。
バイナリは `impl/rust/target/release`（または `$CARGO_TARGET_DIR/release`、`$YUZHU_BIN_DIR`）から探します。

```sh
(cd impl/rust && cargo build --release -p yuzhu-server)
tests/yuzhu.sh start [--port 5432] [--data DIR] [--shared-buffers 1MB]   # データがなければ initdb してから起動
tests/yuzhu.sh crash                                                     # kill -9 して同じオプションで起動し直す（M3）
tests/yuzhu.sh restart                                                   # fast shutdown して同じオプションで起動し直す
tests/yuzhu.sh stop [smart|fast|immediate]                               # データは残す
tests/yuzhu.sh clean                                                     # 止めて、データごと消す
tests/run.sh --target yuzhu                                              # m1〜m3
```

状態（データ、ログ、pid）は `$YUZHU_STATE`（既定 `/tmp/yuzhu-test`）に置きます。

### 再起動をまたぐテスト（`tests/restart/`）

```sh
tests/run.sh --target pg --restart                          # tests/restart の全シナリオ
tests/run.sh --target yuzhu --restart tests/restart/02-rollback
```

- 1 つのシナリオは `tests/restart/<シナリオ>/NN-<名前>.slt` の列です。1 ファイルが 1 フェーズで、**フェーズごとにランナーのプロセスを起動し直し**、
  フェーズの間でサーバを再起動します（最後のフェーズのあとは再起動しません）。各フェーズは自分で接続するので、再起動後の再接続をランナーに頼りません。
- 再起動: `pg` は `tests/pg.sh restart`（docker があるとき。smart shutdown）、コンテナ内では `sandbox/pg.sh restart`。
  環境変数 `PG_RESTART_CMD` で差し替えられます。`yuzhu` は `tests/yuzhu.sh restart`（SIGINT の fast shutdown → 同じデータディレクトリで起動）。
- シナリオのディレクトリに `yuzhu.args` があれば、yuzhu ではそのシナリオの前にその内容でサーバを起動し直し（例: `--shared-buffers 1MB`）、
  終わったら既定のオプションに戻します。`03-steal-rollback` はバッファプールより大きいテーブルを作るために使います。
- フェーズの最後にコミットしていない変更を残したい場合は `connection other` を使います（ランナーが終わると接続が閉じ、その後に再起動されます）。
- テーブル名はシナリオごとに一意な接頭辞（`rs1_`〜`rs6_`）を付け、最後のフェーズで DROP します。

| シナリオ | 内容 |
|---|---|
| `01-committed-dml` | CREATE / INSERT / UPDATE / DELETE をコミット → 再起動 → 値が残っている（2 回再起動） |
| `02-rollback` | ROLLBACK した UPDATE / DELETE / INSERT → 再起動 → 元の値（コミットログの永続化） |
| `03-steal-rollback` | プールより大きいテーブルで全行 UPDATE → ROLLBACK → 再起動 → 元の値（steal されたページの未コミットの版が見えない） |
| `04-create-rollback` | BEGIN → CREATE TABLE → ROLLBACK → 再起動 → テーブルがない |
| `05-drop-commit` | DROP TABLE をコミット → 再起動 → テーブルもカタログの行もない |
| `06-uncommitted-at-stop` | 別の接続にコミットしていない変更を残して停止 → 再起動 → その変更はない |

### クラッシュをまたぐテスト（`tests/restart/m3/`、`--crash`）

```sh
tests/run.sh --target pg --crash                       # tests/restart/m3 の全シナリオ（フェーズの間で kill -9）
tests/run.sh --target yuzhu --crash tests/restart/m3/08-around-checkpoint
tests/run.sh --target pg --restart                     # tests/restart 直下だけ（m3 は含めない）
```

- `--crash` は `--restart` と同じ仕組みで、フェーズの間の停止を **kill -9** にする。停止チェックポイントも WAL の flush もなく、次の起動でクラッシュリカバリが走る。
  - `pg`: 環境変数 `PG_CRASH_CMD`、なければ `tests/pg.sh crash`（docker があれば `docker kill -s KILL` + `docker start`。なければ `sandbox/pg.sh` の PostgreSQL の postmaster と子プロセスを kill -9 して `pg_ctl start`）。
  - `yuzhu`: `tests/yuzhu.sh crash`（SIGKILL → 同じデータディレクトリ・同じオプションで起動 → 待ち受けを待つ）。
- シナリオのディレクトリの追加ファイル: `yuzhu.args`（M2 と同じ）、`yuzhu.only`（あれば pg では飛ばす）、
  `NN-<名前>.after.sh`（フェーズ NN のあと、サーバが止まっている間に実行。`$YUZHU_DATA` がデータディレクトリ。yuzhu だけ）。
- 別の接続の未コミットの変更は `connection other` で残す。yuzhu の M3 は書き込みが 1 本ずつなので、本体の接続のコミットを先に済ませてから `other` が書き始める。
- 集約は M4 なので、行数は `ORDER BY ... LIMIT 1 OFFSET N` で確かめる。テーブル名の接頭辞は `cr1_`〜`cr10_`。

| シナリオ | 内容 |
|---|---|
| `01-committed-dml` | CREATE / INSERT / UPDATE / DELETE をコミット → クラッシュ → 残る。リカバリ後に足した変更も次のクラッシュで残る |
| `02-uncommitted-hidden` | 別の接続の未コミットの INSERT / UPDATE / DELETE はクラッシュ後に見えず、行がロックされたままにならない |
| `03-drop-commit` | DROP TABLE をコミット → クラッシュ → テーブルもカタログの行もない。同名で作り直せて、それも次のクラッシュで残る |
| `04-create-rollback` | BEGIN → CREATE TABLE → INSERT → ROLLBACK → クラッシュ → ない |
| `05-checkpoint-large` | プールより大きい表（`yuzhu.args`: 1MB）を全行 UPDATE → CHECKPOINT → もう一度全行 UPDATE → クラッシュ → 最後の値 |
| `06-double-crash` | リカバリ直後に検査だけしてもう一度クラッシュ、さらに書いて 3 回目。未コミットのものが XID の再利用で見えない |
| `07-ddl-in-flight` | 未コミットの CREATE TABLE と DROP TABLE の途中でクラッシュ → 作りかけは無く、DROP しかけの表は中身ごと残る |
| `08-around-checkpoint` | チェックポイントの前後のコミット / ロールバック、チェックポイントをまたぐ未コミットのトランザクション、チェックポイントの後に作ったテーブル。リカバリ直後の CHECKPOINT |
| `09-large-values` | TOAST される大きな値と、1 トランザクションでの 1024 行のコミットが全部残る |
| `10-checksum-corrupt` | **yuzhu 専用**。チェックポイント後に止めて、ヒープのページを壊し（`01-prepare.after.sh`）、起動後に `XX001` で検出される。ほかの表は使え、壊れた表は DROP できる |

### M2 のテスト（`tests/slt/m2/`）の書き方

M1 の規則に加えて:

- **システム列の値（`xmin`、`ctid` など）は PostgreSQL と一致しない**ので比べません。`ORDER BY xmin::text::int8` のような大小関係と、
  新しいテーブルに順に INSERT したときの `ctid::text`（`(0,1)`、`(0,2)`、…）だけを使います（`dml/system_columns.slt`）。
- カタログのテストは、`attrelid` を名前から引く手段（`regclass`、サブクエリ）が M2 にないので、`attrelid > 16383`（ユーザーテーブルは OID 16384 以上）で絞ります。
  このため、**カタログを調べるファイルは、他のユーザーテーブルが存在しない状態**（前のファイルが DROP 済み）で流します。
  toast テーブルの列が混ざらないよう、そのファイルでは `text` 型の列を使いません（`varchar(n)` で行の最大長が約 2KB に収まれば toast テーブルは作られません）。
- 集約（`count(*)` など）、JOIN、サブクエリは M4 なので使いません。行数は `statement count N`、空の結果は `query T` + 空の期待値で確かめます。
- PostgreSQL が成功して yuzhu が `0A000` を返す機能は、`onlyif yuzhu` を付けて yuzhu だけで確かめます。
- `statement error` は SQLSTATE（`(23514)`）で照合します。`--override` は `db error: ...` の形で書き出すので、**override したあとは必ず SQLSTATE の形に直します**。
- `--override` は空の結果を `statement count 0` に書き換えます。`query T` + 空の期待値に戻します。
- カタログの `oid` 以外の列で比べる行は、yuzhu が持つものに限ります（`pg_am` なら `oid IN (2, 403)`、`pg_type` なら yuzhu が持つ型の OID）。
- カタログへの DML の拒否（`42501`）は PostgreSQL のスーパーユーザーでは成功するので、slt には入れません（Rust の結合テストで確かめる）。

### isolation テスト（ブロックする交互実行）

`tests/isolation/specs/*.spec` は PostgreSQL の isolationtester と同じ形式のテストで、
自作ランナー `tests/tools/isolation`（Simple Query のみ）で実行します。詳しくは
[tests/tools/isolation/README.md](tools/isolation/README.md) を参照してください。

```sh
cargo build --release --manifest-path tests/tools/isolation/Cargo.toml
${CARGO_TARGET_DIR:-tests/tools/isolation/target}/release/yuzhu-isolation --port 55432 tests/isolation/specs              # PostgreSQL
${CARGO_TARGET_DIR:-tests/tools/isolation/target}/release/yuzhu-isolation --port 5432 --blocking-detection timeout tests/isolation/specs  # yuzhu
```

### 差分ファジング（`tests/tools/difffuzz`）

本物の PostgreSQL 17 と yuzhu に同じ SQL を同じ順序で流し、行・コマンドタグ・SQLSTATE・エラーメッセージを突き合わせる。
領域は `expr` `types` `query` `dml` `txn`（M1〜M3）と `join` `agg` `subquery` `setop` `index` `ddl`（M4。多重集合比較、評価順で変わるエラーの別扱い、`enable_*` 変種と索引の有無の検査つき）。`--domain all` は case 番号で順に切り替える。外部クレートなしで、`psql` を子プロセスで呼ぶ。
乱数は自前なので、`(seed, case)` が決まれば SQL は常に同じ。

```sh
sandbox/pg.sh start                                  # PG: 127.0.0.1:55432（起動済みなら再利用）
YUZHU_STATE=/tmp/fz YUZHU_PORT=55433 tests/yuzhu.sh start   # 自分専用のデータディレクトリとポートで yuzhu を起動
cd tests/tools/difffuzz && cargo build --release
target/release/difffuzz --domain expr --seed 1 --cases 5000 --out /tmp/difffuzz.jsonl
target/release/difffuzz --domain expr --seed 1 --case 13 -v   # 差分の 1 ケースを再現
```

- 終了コード: 0 = 差分なし、1 = 差分あり、2 = 実行エラー。差分は JSON Lines（SQL、両者の出力、seed、case、シナリオ全体）。
- 差分が出たら最小の SQL に縮め、yuzhu を直し、`tests/slt/` に回帰テストを足す（PG でも通ること）。
- yuzhu 側が未対応（`0A000`）の文は `--skip-unsupported` で数えるだけにできる。メッセージの差は `--no-message` で無視できる。
- M4 の完了判定（`tests/done-check.sh` 条件 6）は `--domain all` を固定シード 1〜32（各 200 ケース = 約 3,600 文）と 1001〜1004（各 10,000 ケース = 約 18 万文）で流す。既知の差で他の差分が隠れるときだけ `tests/tools/difffuzz/known-excludes.txt` に針を書く。
- 詳細なオプションは `tests/tools/difffuzz/README.md`。

### CI

`.github/workflows/ci.yml` のジョブ:

- `slt-pg`: postgres:17 のサービスコンテナに対してスイートを流し、期待値が正しいことを確かめます。
- `slt-yuzhu`: yuzhu-server をビルド・起動して `tests/slt`（m1・m2・m3）を流します（実装が揃うまでは `continue-on-error: true`）。
- `restart-pg` / `restart-yuzhu`: `--restart` の全シナリオと、`--restart --crash tests/restart/m3`。
- `isolation`（pg / yuzhu のマトリクス）: `tests/tools/isolation` をビルドして `tests/isolation/specs` を流します。
- `crash-sim`: クラッシュ試験 層 1（`cargo test --release -p yuzhu-core --test crash_sim`。固定シード）。
- 夜間ジョブ（層 1 の長時間ランダム実行と層 2 の `cargo test -- --ignored`）は未追加です。

## テストの書き方

### 基本

- 1 ファイル 1 機能、`tests/slt/m1/<機能>/<内容>.slt` に置きます。
- **1 レコード 1 文**にします（トランザクションの意味論を確かめる場合だけ、`;` で区切った複数の文を 1 レコードに書きます）。
- **テーブル名はファイルごとに一意な接頭辞を付けます**（例: `select/order_by.slt` なら `sel_o_t`）。ランナーはすべてのファイルを同じデータベースで直列に実行するためです。
- ファイルで作ったテーブルは、**ファイルの最後で必ず DROP** します。
- SET した設定は、ファイルの最後までに RESET で元に戻します。
- 新しいテストを足したら、必ず `tests/run.sh --target pg` で通ることを確かめます。

### 期待値

- 結果は PostgreSQL のテキスト出力そのままです。NULL は `NULL`、空文字列は `(empty)`、bool は `t` / `f`。
- ランナーは各行の空白を正規化して比較します（連続した空白は 1 つになり、前後の空白は消えます）。
  空白そのものを確かめたいときは `length()` などで間接的に確かめます。文字列 `'NULL'` や `'(empty)'` は使いません。
- **順序が決まらない結果にだけ `rowsort` を付けます**。`ORDER BY` がある問い合わせには付けません。
  `rowsort` は行を**文字列として**並べ替えるので、`10` は `5` より前に、`NULL` は数字より後ろに来ます。
- 型文字列（`query ITR?` の部分）はランナーが検証しませんが、将来の自前ランナーで検証するので正しく書きます。
  整数は `I`、浮動小数点は `R`、文字列（text・varchar・unknown）は `T`、bool などそれ以外は `?` です。

### エラー

- エラーは **SQLSTATE で照合**します: `statement error (42P01)` / `query error (22012)`。
- メッセージの照合は補助として、短い部分一致に留めます（例: `statement error violates check constraint "a_lt_b"`）。
  `db error: ERROR:` のようなクライアント依存の接頭辞は書きません。

### PostgreSQL の挙動で注意する点

- **小数リテラル（`1.5`）は PostgreSQL では numeric 型**です。numeric は M1 の範囲外なので、float8 の値は
  `'1.5'::float8` のように文字列からキャストして書きます。`1e10` のような指数表記のリテラルも numeric なので使いません。
- `2147483648` 以上の整数リテラルは int8、`9223372036854775808` 以上は numeric になります（後者は使いません）。
- `version()` は環境によって変わるので、`SELECT version() LIKE 'PostgreSQL %'` の形でだけ確かめます。
  yuzhu の `version()` は `PostgreSQL 16.0 (yuzhu ...)` で始まる文字列を返します。
- `timezone` の既定値は環境によって違う（docker の postgres では `Etc/UTC`、yuzhu では `UTC`）ので、既定値は SHOW しません。
- `application_name` はランナーが設定しないので、既定値は空です。
- M1 の範囲外の機能（JOIN、集約、サブクエリ、UPDATE / DELETE（`tests/slt/m2` では使う）、PRIMARY KEY / UNIQUE、numeric・日付型など）は使いません。

### onlyif / skipif

- yuzhu 向けのラベルは `yuzhu`、PostgreSQL 向けは `pg` です（エンジン名 `postgres` は両方に付きます）。
- `skipif yuzhu` は「未実装の一時回避」に限り、理由をコメントで残します。PostgreSQL と挙動が違う部分は yuzhu のバグとして扱います。

### 期待値の生成

`--override` は**本物の PostgreSQL に対してだけ**使い、差分をレビューしてからコミットします。yuzhu に対して `--override` を使ってはいけません。

```sh
SLT_EXTRA_ARGS=--override tests/run.sh --target pg tests/slt/m1/select/new_test.slt
git diff tests/slt
```

## M3 のテストの実行方法

```sh
# 1. SQL テスト（m1〜m3）
tests/run.sh --target pg                                   # PostgreSQL 17 で期待値を確認
tests/run.sh --target yuzhu                                # yuzhu（m3/txn/savepoint_unsupported.slt は yuzhu 専用）

# 2. クラッシュをまたぐテスト（フェーズの間で kill -9 → 同じデータディレクトリで起動）
tests/run.sh --target pg --restart --crash tests/restart/m3
tests/run.sh --target yuzhu --restart --crash tests/restart/m3

# 3. 分離性テスト
cargo build --release --manifest-path tests/tools/isolation/Cargo.toml
${CARGO_TARGET_DIR:-tests/tools/isolation/target}/release/yuzhu-isolation --port 55432 tests/isolation/specs
${CARGO_TARGET_DIR:-tests/tools/isolation/target}/release/yuzhu-isolation --port 5432 --blocking-detection timeout tests/isolation/specs

# 4. クラッシュ試験 層 1（SimVfs。サーバ不要）
(cd impl/rust && cargo test --release -p yuzhu-core --test crash_sim)
# 失敗の再現: YUZHU_SIM_SEED=... YUZHU_CRASH_AT=... cargo test -p yuzhu-core --test crash_sim
# 層 2（kill -9、yuzhu-server/tests/crash_kill9.rs）: cargo test -p yuzhu-server --test crash_kill9 -- --ignored
```

- `--crash` は `--restart` と併用し、フェーズ間の再起動を `tests/yuzhu.sh crash` / `tests/pg.sh crash`（kill -9 → 起動 → 待つ）に置き換えます。
- PostgreSQL 側のクラッシュは `PG_CRASH_CMD`（なければ `tests/pg.sh crash`。docker なしなら `sandbox/pg.sh` で起動した PG を kill -9 して `pg_ctl start`）です。
- `tests/restart/m3/10-checksum-corrupt` は `yuzhu.only`（PG では飛ばす）です。`10-*.after.sh` が `$YUZHU_DATA` のヒープを壊します。
- **注意**: `$CARGO_TARGET_DIR` が設定されている環境では、isolation ランナーは `$CARGO_TARGET_DIR/release/` に出ます。
  古い `tests/tools/isolation/target/release/yuzhu-isolation` が残っていると `-- @cancel` 拡張が無く `cancel-wait` が 30 秒で失敗するので、必ずビルドし直します。
- PG にテーブルが残っていると `m2/catalog/*` や `m2/ddl/drop_cleanup` が失敗します。slt の前に PG をクリーンにします（`sandbox/pg.sh stop && sandbox/pg.sh start`）。
- 注意: `tests/pg.sh crash`（docker なし）は kill -9 を使うので、同じ PG を他のエージェントが使っている最中に流さないこと。

## M4 の追加（K4）

### 完了判定 `tests/done-check.sh`

```sh
tests/done-check.sh                  # 条件 1〜9 を順に実行し、最後に PASS / FAIL / MISSING の一覧を出す
tests/done-check.sh --only 2,4       # 条件を選ぶ（完了判定にはならない）
tests/done-check.sh --quick          # 差分テストの長時間実行を短くする（完了判定にはならない）
tests/done-check.sh --pg-only --skip 1,7   # yuzhu の手順を飛ばし、PostgreSQL だけでテスト自体を検証する（完了判定にはならない）
```

- 専用の PostgreSQL（`127.0.0.1:55443`）と yuzhu（`127.0.0.1:5443`）を起動し、終わりに止める。他のエージェントが使う 55432 / 5432 には触らない
  （環境変数 `DC_PG_PORT` `DC_YUZHU_PORT` `DC_PG_DATA` で変える。`--keep` で残す）。
- 条件ごとのログは `/tmp/yuzhu-done-check/<日時>/`。1 つでも FAIL / MISSING があれば終了コード 1。
- 条件 5（`tests/compat/run.sh`）と条件 8（`explain/format.slt`、`deparse_*.slt`、`plan_variants/`）は、ファイルがなければ MISSING。

### slt の lint `tests/tools/slttools`

```sh
cargo build --release --manifest-path tests/tools/slttools/Cargo.toml   # $CARGO_TARGET_DIR/release/slttools
slttools lint [--only L01,L02] [paths...]      # 既定は tests/slt と tests/restart。違反があれば終了コード 1（warning は 0）
slttools consistency [--add] [--check] [paths...]
```

- 実装済みの規則は L01〜L10（`11-tests-plan.md` §3.2.5）。L11（plan_variants の生成物）は未実装。
  L03 と L06 は m4（`tests/slt/m4`、`tests/restart/m4`）だけに適用する。
  L01 は、作成の直前 3 行以内に `# LINT-ALLOW: L01 理由` があれば免除する（コミットされない作成など。例: `tests/restart/06-uncommitted-at-stop`）。
  L02 は、エラーを期待する SET、`SET TRANSACTION` / `SET SESSION CHARACTERISTICS` / `SET CONSTRAINTS` を対象にしない。`RESET ALL` でも満たされる。
- `consistency` は `tests/slt/_include/consistency.slt.part` を `z_final/consistency.slt` と、`tests/restart/m4` の各シナリオの最後のフェーズへコピーする。
  最後のフェーズに入れるには、`slttools consistency --add tests/restart/m4/<シナリオ>`（`# >>> consistency` ... `# <<< consistency` のブロックが付く）。以後は引数なしで同期する。
- `z_final/no_leftovers.slt` は、スイート全体を**新しいサーバ**で流したときだけ通る（ユーザーオブジェクトが 0 件）。他のテストが同じ PostgreSQL を使っている最中は通らない。

### 再起動・クラッシュのシナリオの `mode`（`tests/restart/m4/`）

- `mode`（1 行。なければ `both`）: `restart` は `--restart` のみ、`crash` は `--crash` のみ、`both` は両方、`mixed` は `--crash` の中で 1 回だけ。
  フェーズごとの `NN-<名前>.mode`（`restart` | `crash`）が、そのフェーズの後の停止の方法を決める（`mixed` のシナリオ）。不正な値は終了コード 2。
- 既定の探索先: `--restart` は `tests/restart` 直下と `tests/restart/m4`、`--crash` は `tests/restart/m3` と `tests/restart/m4`。明示のパスを渡すと `mode` を無視して全部流す。
- B+Tree のシナリオ（K4）: `btree-grow`（both。昇順・降順・ばらばらの順の挿入で分割、キーを変える UPDATE、範囲 DELETE、ROLLBACK）、
  `btree-uncommitted-crash`（crash。`--shared-buffers 1MB`。分割を伴う未コミットの変更を残して kill -9）、`btree-mixed`（mixed。正常停止のあと kill -9）。接頭辞は `m4r_bg_` `m4r_bu_` `m4r_bm_` `m4r_`。
- 専用の PostgreSQL に対して流すには、`PG_DATA=... PG_PORT=N sandbox/pg.sh start` のあと `PG_DATA=... PG_PORT=N tests/run.sh --target pg --port N --crash`（再起動・kill -9 が同じインスタンスを指す）。

### isolation の追加

`index-reader-writer`（索引走査の読み手は書き手を待たず、ROLLBACK 後も値が戻る）、`seq-nonblocking`（`nextval` は待たず、ROLLBACK で戻らない）、
`index-unique-wait`（同じキーの INSERT は待ち合い、COMMIT なら 23505、ROLLBACK なら成功）。期待値は PostgreSQL 17 で作成。
yuzhu では `--variant yuzhu-m3` を付けて流す（Repeatable Read は M5）。

### M4 の slt（`tests/slt/m4/`）の構成と運用

- K1: `join` `agg` `subquery` `setop` `cte` `dml`（接頭辞 `jn_` `ag_` `sb_` `so_` `ct_` `dm_`）。
  `dml/returning.slt.pending` は、yuzhu が `RETURNING_ENABLED` を true にしてから `returning.slt` に改名して有効化する（run.sh は `*.slt` だけを拾う）。
  有効化すると `tests/slt/m2/dml/update_errors.slt` の `onlyif yuzhu` の `RETURNING` `0A000` が落ちるので、あわせて直す。
- K2: `catalog` `constraint` `index` `ddl` `seq`（接頭辞 `cat_` `cst_` `idx_` `ddl_` `sq_` `sr_` `id_`）と `tests/restart/m4` の idx-* / seq-* など。
  `seq/log_cnt.slt` は yuzhu 専用（`onlyif yuzhu`）なので PostgreSQL では検証していない。
- K3: `types` `explain` `copy` `psql`（接頭辞 `ty_` `ex_` `cp_` `psql_t_`）。`explain/nodes.slt` の join・集合演算のプラン形は PostgreSQL の選択のまま。
- K4: `z_final`、`KNOWN-DIFFS.md`（KD-30 は索引・制約の未対応オプション）、isolation、`tests/restart/m4/btree-*`。
- 未作成: `tests/compat/`（条件 5）、`plan_variants/`、`mem/limit.slt`、`slttools` の `plan-variants` / `large-keys` / `pgregress` と L11。
- 重複: カタログ整合の SQL は `_include/consistency.slt.part` が正本。both / crash のシナリオの最後のフェーズに入っている同じ内容のブロックは、`slttools consistency --check` で同期を確認する。
- PostgreSQL に対する確認の手順（クリーンなインスタンスで）:
  `PG_PORT=55460 PG_DATA=/tmp/x sandbox/pg.sh start` → `tests/run.sh --target pg --port 55460`（`z_final/no_leftovers.slt` まで通る）→
  `PG_DATA=/tmp/x tests/run.sh --target pg --port 55460 --restart` と `--crash` → `yuzhu-isolation --port 55460 tests/isolation/specs`。
  共有の 55432 は他のエージェントの表が残るので、`no_leftovers.slt` と `m2/catalog/pg_attribute.slt` などの `oid >= 16384` を数える系は通らないことがある。
