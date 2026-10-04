# yuzhu 共通テストスイート調査: sqllogictest ほか

調査日: 2026-10-04
対象: 言語に依存しない共通テストを PostgreSQL ワイヤプロトコル経由で、yuzhu の各実装と本物の PostgreSQL の両方に対して実行する方法

> 凡例: **[検証済]** は今回 scratchpad で実際にビルド・実行して確かめた事実（postgres:17 コンテナ + sqllogictest-bin 0.29.1）。**[ソース確認]** はソースコードを読んで確かめた事実。それ以外は公式ドキュメントからの引用、または筆者の知見（その旨を明記）。

---

## 0. 結論（TL;DR）

- **ランナーは `sqllogictest-rs` の CLI（`sqllogictest-bin` 0.29.1）を採用し、`--engine postgres`（simple）を使う。** simple エンジンは Simple Query プロトコルだけを使うので、M1（Simple Query のみ対応）にそのまま使える。**[検証済]**
- **最大の落とし穴:** `cargo install sqllogictest-bin` を `--locked` なしで実行すると tokio-postgres 0.7.18 で解決される。0.7.15 以降は `Display` がエラーチェーンを含まないため、エラーメッセージが `db error` だけになり、**`statement error <regex>` 形式のテストが全部壊れる**。**[検証済]** 対策は、`--locked` を付けるか GitHub Releases のビルド済みバイナリを使うこと（どちらも tokio-postgres 0.7.12）。そのうえで、エラーの期待値は **SQLSTATE 指定 `statement error (42P01)` を基本**にする。
- CLI の **型文字列（`query ITR` の部分）は検証されない**。simple エンジンは全列を `Any` として返し、CLI は常に true を返す column validator を使う。**[ソース確認]** 型までチェックしたい場合は、後で `sqllogictest` crate をライブラリとして使う小さな自前ランナーを作り、RowDescription の型 OID から型文字列を導いて strict に検証する（推奨ロードマップ参照）。
- 期待値は **本物の PostgreSQL（postgres:17 を digest 固定）に対して `--override` で生成し、レビューしてコミットする。** CI では、まず PG に対してスイート全体が通ること（期待値の正しさ）を確認し、次に yuzhu 実装に対して流す、という 2 段構えにする。
- SQLite の sqllogictest コーパスは「PG で通る部分集合をフィルタして後から取り込む」用途なら使える（DataFusion に前例あり）。pg_regress は psql 依存とシステムカタログ依存が強いので、M1 では使わない。後で RisingWave のように「psql 経由の regress ランナー」を流用・自作して部分集合を使うのが現実的。
- isolation テストは、PG の isolationtester が `pg_locks` 系の PG 固有関数でブロッキングを検出するため、yuzhu にはそのまま使えない。spec 形式（setup/session/step/permutation）だけ借りて、タイムアウトでブロッキングを判定する自前ランナーを後で作るのが現実的。

---

## 1. sqllogictest-rs（risinglightdb/sqllogictest-rs）

### 1.1 プロジェクトの状況

| 項目 | 値 | 出典 |
|---|---|---|
| 最新版 | `sqllogictest` / `sqllogictest-bin` / `sqllogictest-engines` すべて **0.29.1**（2026-02-13 リリース） | https://crates.io/crates/sqllogictest-bin , CHANGELOG |
| 累計 DL | `sqllogictest` 約 1,441 万、`sqllogictest-bin` 約 7.2 万 | crates.io API |
| GitHub | Star 234、最終 push 2026-02-14、open issue/PR 35 | https://github.com/risinglightdb/sqllogictest-rs |
| ライセンス | MIT OR Apache-2.0 | 同上 |
| 主な利用者 | RisingWave、DataFusion、Databend、RisingLight、CnosDB | README |
| ビルド済みバイナリ | linux x86_64/aarch64 (musl)、macOS、Windows を Releases で配布 | https://github.com/risinglightdb/sqllogictest-rs/releases |

直近の変更（CHANGELOG https://github.com/risinglightdb/sqllogictest-rs/blob/main/CHANGELOG.md より）:
- 0.29.1: `let` レコード（クエリ結果を変数に束縛する）
- 0.29.0 (2025-12): **`statement|query error (<SQLSTATE>)` による SQLSTATE 照合**
- 0.28.4: `<slt:ignore>`（出力の揮発部分をワイルドカード扱い）、`--skip <regex>`
- 0.28.0: `--partition-id/--partition-count`（CI シャーディング）
- 0.27.1: `$__DATABASE__` 変数

開発頻度は 2025〜2026 年初頭は月 1 回程度で、2026-02 以降は落ち着いている。ただし利用者が多く、フォーマットが安定しているのでリスクは低い。

### 1.2 ファイルフォーマット（sqllogictest-rs 方言）

SQLite オリジナル形式（後述）の上位互換。README の Cookbook と `tests/slt/*.slt`、`sqllogictest/src/parser.rs` から整理した。**[ソース確認]**

| レコード | 構文・意味 |
|---|---|
| コメント | `#` で始まる行 |
| statement 成功 | `statement ok` |
| 影響行数チェック | `statement count <n>`（CommandComplete タグの行数と比較） |
| statement 失敗 | `statement error`（メッセージは見ない）／`statement error <regex>`（1 行の regex。**部分一致** = `Regex::is_match`）／`statement error (<SQLSTATE>)`（完全一致）／`statement error` の後に `----` と複数行（trim 後の完全一致。終端は空行 2 つ） |
| query | `query <types> [sortmode] [label]` → SQL → `----` → 期待行 |
| 型文字列 | `I` 整数、`T` テキスト、`R` 浮動小数、それ以外は `?`（Any） |
| ソートモード | `nosort`（既定）、`rowsort`（行単位で文字列ソート）、`valuesort`（全値をばらしてソート） |
| query 失敗 | `query error ...`（statement error と同じ構文） |
| ハッシュ | `hash-threshold <n>`。値の数が n を超えると期待値は `N values hashing to <md5>` 形式になる |
| control | `control sortmode rowsort`（ファイル全体の既定ソート）、`control resultmode valuewise/rowwise`、`control substitution on/off` |
| include | `include <glob>`（例: `include ./common/*.slt.part`） |
| 条件 | `onlyif <label>` / `skipif <label>`（次の 1 レコードだけに効く）。ラベルは **エンジン名（`postgres`, `postgres-extended`）+ `--label` で指定した任意のラベル** |
| retry | `query I retry 3 backoff 5s` 等。**`--override` とは併用できない**（issue #275 が未解決） |
| 接続切替 | `connection <name>` を次レコードの直前に書く。**次の 1 レコードにだけ効く**。名前ごとに別の接続が遅延生成される。`default` が既定接続 |
| 変数置換 | `control substitution on` で `$VAR`, `${VAR:default}`（subst crate）、特殊変数 `$__TEST_DIR__`, `$__NOW__`, `$__DATABASE__` |
| let | `let id` → 1 行のクエリ。結果を変数に束縛する（0.29.1〜） |
| その他 | `system ok`（シェル実行）、`sleep 1s`、`halt`、`subtest <name>`、`<slt:ignore>`（期待値の一部をワイルドカード扱い） |

**期待値の比較方法 [ソース確認]**: `default_normalizer` は各行を `trim().split_ascii_whitespace().join(" ")` で正規化してから比較する。つまり、
- 列の区切り（`--override` はタブを出力）と、値の中の空白は区別されない。`'x y'` という 1 列と、`x` と `y` の 2 列は同じに見える。
- 値の前後の空白や連続した空白は失われる。

### 1.3 CLI（`sqllogictest` バイナリ）

主なオプション（`sqllogictest-bin/src/main.rs`）**[ソース確認]**:

```
sqllogictest [OPTIONS] <FILES/GLOBS>...
  -e, --engine <postgres|postgres-extended|mysql|external>   (既定 postgres)
  -h, --host (SLT_HOST, 既定 localhost)   -p, --port (SLT_PORT, 既定 5432)
  -d, --db (SLT_DB, 既定 postgres)  -u, --user (SLT_USER, 既定 postgres)
  -w, --pass (SLT_PASSWORD, 既定 postgres)  --options <pg options>
  --override        実際の出力で .slt を書き換える
  --format          .slt を整形
  --label <L>       onlyif/skipif 用のラベルを追加（複数指定可）
  -j, --jobs <N>    並列実行。ファイルごとに CREATE DATABASE / DROP DATABASE する
  --junit <file>    JUnit XML を出力
  --fail-fast, --skip <regex>, --partition-id/--partition-count
  --shutdown-timeout <secs>
  --external-engine-command-template "<cmd> {host} {port} {db} {user} {pass}"
```

- `-j` を使わない場合、全ファイルが同じ DB で直列に実行される。テスト同士の独立性はファイル側で確保する必要がある（ファイル冒頭でテーブルを作り、末尾で DROP する、テーブル名にファイルごとの接頭辞を付ける、など）。
- **`-j` はランナーが `CREATE DATABASE` / `DROP DATABASE` を発行する**ので、それらを実装するまで yuzhu には使えない。
- `external` エンジンは、任意のコマンドと stdin/stdout の JSON でやり取りする仕組み。他言語のクライアントでテストしたい場合の逃げ道になる。

### 1.4 postgres エンジンの接続と描画

#### 接続（tokio-postgres 経由）[ソース確認]
- `tokio_postgres::Config::connect(NoTls)`。sslmode は既定の `prefer` だが、`NoTls` は TLS に対応できないので **SSLRequest は送らない**（`connect_tls.rs`）。
- StartupMessage のパラメータは `client_encoding=UTF8`, `user`, `database`（加えて `--options` を指定した場合は `options`）。
- 認証は AuthenticationOk（trust）、cleartext、MD5、SCRAM-SHA-256 に対応。M1 の yuzhu は **AuthenticationOk を返すだけで接続できる**。
- 起動シーケンスで tokio-postgres が受け付けるのは `BackendKeyData` / `ParameterStatus` / `NoticeResponse` / `ReadyForQuery` / `ErrorResponse` だけ。それ以外のメッセージが来ると `unexpected message` になる。BackendKeyData と ParameterStatus は省略してよい（pid/secret は 0 になる）。
- **接続を閉じるとき**、エンジンは `cancel_token().cancel_query()` を呼ぶ。これは **新しい TCP 接続を張って CancelRequest（コード 80877102）を送る** という動作になる（0.28.2〜）。yuzhu はこれを受けて静かに切断できる必要がある。失敗しても警告ログが出るだけだが、サーバ側が CancelRequest をパースできずにハングすると、テストが終わらなくなる危険がある。`--shutdown-timeout` を付けておくと安全。

#### simple エンジン（`--engine postgres`）は Simple Query だけを使う → **Yes** [検証済]
- `sqllogictest-engines/src/postgres/simple.rs` は `client.simple_query(sql)` だけを呼ぶ。
- 実際に `log_statement=all` を有効にした postgres:17 のログでは、simple エンジンの接続は `LOG: statement: ...`（Simple Query）だけだった。extended エンジンの接続は `LOG: execute s0: ...`（Parse/Bind/Execute）だった。
- **M1（Simple Query のみ）にはそのまま使える。**

#### 値の描画（simple）[ソース確認・検証済]
- サーバが返した **テキスト形式の値をそのまま** 使う。PG の出力関数の結果がそのまま期待値になる。
  - bool → `t` / `f`
  - float8 → PG12 以降の shortest-exact 形式。`0.1`、`1e+20`、`0.3333333333333333`
  - numeric → スケールを保つ（`1.50`）
- SQL の NULL → `NULL`、空文字列 → `(empty)`。**文字列の `'NULL'` と SQL NULL は区別できない**。
- 型は全列 `DefaultColumnType::Any`。
- 結果が 0 行のときは `DBOutput::StatementComplete` を返す。ランナーは「query の期待値が空なら OK」と扱うので問題はない。逆に `statement ok` で SELECT を流しても許容される。
- 1 レコードに複数の文を書くと、最初の CommandComplete で打ち切られ、その時点までの結果だけが返る（後続文のエラーは simple_query が Err にする）。

#### extended エンジン（`--engine postgres-extended`）
- `prepare()` → `query_raw()` を使う。つまり Parse/Describe/Bind/Execute/Sync で、結果は **バイナリ形式で受け取り Rust 側で整形** する。
- 整形が PG と異なる [検証済]: float8 の `1e20` が simple では `1e+20`、extended では `100000000000000000000` になった。
- 対応する型は固定のリスト（int2/4/8, numeric, date, time, timestamp(tz), interval, bool, float4/8, text/varchar とそれらの配列など）。
- **simple と extended で期待値が変わる**ので、共通スイートは simple を正とし、extended は後のマイルストーンで別ジョブとして（必要なら `skipif postgres-extended` を付けて）追加するのがよい。

### 1.5 エラーメッセージ照合と tokio-postgres のバージョン問題 [検証済]

- ランナーは `e.to_string()` を regex と照合する。エラー文字列は tokio-postgres の `Display` 実装に依存する。
- tokio-postgres **0.7.15 (2025-10-08) の CHANGELOG**: "Stop including error chain in `Display` impl of `Error`"（https://crates.io/crates/tokio-postgres/0.7.15 ）。
- sqllogictest-rs の `Cargo.lock` は tokio-postgres **0.7.12** に固定されている。Releases のビルド済みバイナリにも 0.7.12 が入っている（バイナリの strings で確認）。
- 実測結果:

| ビルド | エラー文字列 | `statement error relation "nope" does not exist` | `statement error (42P01)` |
|---|---|---|---|
| `cargo install --locked`（0.7.12） | `db error: ERROR: relation "nope" does not exist` | PASS | PASS |
| `cargo install`（--locked なし、0.7.18） | `db error` | **FAIL** | PASS |

- `--override`（locked 版）が書き出す内容は、1 行の場合 `statement error db error: ERROR: relation "nope" does not exist`。DETAIL があると複数行になる:
  ```
  statement error
  ...
  ----
  db error: ERROR: duplicate key value violates unique constraint "p_pkey"
  DETAIL: Key (a)=(1) already exists.
  ```
- 期待値が単なる `statement error`（パターンなし）の場合、`--override` しても書き換えない。

**方針**: CI ではバイナリを固定する（Releases の tar.gz か `cargo install --locked --version 0.29.1`）。エラー期待値は SQLSTATE を主にする。メッセージ regex は短い部分文字列（例: `does not exist`）に留める。`db error: ERROR:` という接頭辞はクライアント実装に依存するので、期待値に含めない。

### 1.6 型文字列は CLI では検証されない [ソース確認・検証済]

- CLI は `default_column_validator`（常に true）を使う。simple エンジンも全列 `Any` を返す。そのため、`query I` で 3 列返しても PASS する（実測）。
- `--override` は、型検証が通れば元の型文字列を残す。つまり書いた型文字列は消えないが、正しいかどうかは誰もチェックしない。
- 列数もチェックされない。ただし、値の数が違えば期待行の比較で落ちる。

### 1.7 本物の PG での期待値生成（`--override`）

```
docker run -d --name pg -e POSTGRES_PASSWORD=postgres -p 5432:5432 \
  -e POSTGRES_INITDB_ARGS="--encoding=UTF8 --locale=C" postgres:17
sqllogictest -p 5432 --label pg 'tests/slt/m1/**/*.slt' --override
git diff tests/slt   # 差分をレビューしてからコミット
```

- `--override` は、期待値と一致したレコードは元のまま残し、不一致のものだけ実際の出力（タブ区切り）で書き換える。
- 運用ルール: **期待値の生成・更新は PG に対してだけ行う**。yuzhu に対して `--override` を実行することは禁止する。

---

## 2. その他の選択肢

### 2.1 SQLite オリジナルの sqllogictest とコーパス
- 仕様: https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki
  - 型 `I/T/R`。`R` は `printf("%.3f")`、NULL は `NULL`、空文字列は `(empty)`、制御文字は `@` で描画する。
  - ソートは nosort/rowsort/valuesort。hash-threshold は 10〜20 を推奨。
  - `skipif/onlyif` で DB ごとの差を吸収する。
- コーパスは数百万件規模のクエリ（`select1-5.test`、`random/`、`index/`、`evidence/` など）。GitHub ミラー: https://github.com/gregrahn/sqllogictest
- 元のハーネスは ODBC か SQLite 直結で、ワイヤプロトコルではない。
- **PG 方言エンジンへの適用性**:
  - 中身はほぼ SQL-92 の整数演算・集約・結合・サブクエリで、PG でも大部分は動く。
  - `R` 列の `%.3f` 描画を前提にしているので、sqllogictest-rs の simple エンジン（PG のテキストをそのまま使う）とは浮動小数の表記が合わない。
  - `/` の整数除算や `CAST` の扱いなど、DB ごとの差分ブロックがある。
  - そのまま流すのではなく、「PG に対して `--override` で期待値を作り直す」か、「PG で一致するファイルだけ選ぶ」という前処理が必要。
  - 前例: DataFusion が `datafusion-testing` リポジトリにクレンジング済みのコピーを持っている（https://github.com/apache/datafusion-testing/tree/main/data/sqlite , https://github.com/apache/datafusion/blob/main/datafusion/sqllogictest/README.md ）。
  - **用途**: M2 以降で SELECT・結合・集約が揃ったら、回帰の母集団として部分的に取り込む価値がある。巨大なので nightly ジョブ向き。

### 2.2 DuckDB / CockroachDB / Materialize の logic test 方言
- **DuckDB**（https://duckdb.org/docs/current/dev/sqllogictest/intro.html ）: `require`、`loop`/`foreach`、`mode`、`statement maybe`、複数接続などの独自拡張を持ち、C++ の unittest に内蔵されている。ワイヤプロトコル外で動くので、ランナーとしては使えない。テストの中身は DuckDB 方言が多い。
- **CockroachDB**（https://github.com/cockroachdb/cockroach/tree/master/pkg/sql/logictest ）: Go のテストハーネス内蔵。`# LogicTest:` 構成指定、`query ... colnames`、`user` 切替などがあり、PG 互換を目指しているので PG に近い SQL が多い。ただし CRDB 固有の出力（エラー文言、型表示）を前提にしている。以下は筆者の知見: ファイル単位でスタンドアロンには使えないが、テストケースの発想源にはなる。
- **Materialize**（https://github.com/MaterializeInc/materialize/tree/main/src/sqllogictest ）: Rust 製の自前ランナー。SQLite 形式と cockroach 形式の両方を読み、pgwire で自 DB に接続する。Materialize 固有の部分が多く、外部ツールとしては使いにくい。
- **RisingWave**: sqllogictest-rs の主要ユーザ。e2e テストを slt で管理している。
- 結論: ランナーとしては sqllogictest-rs が唯一の汎用・スタンドアロン・PG ワイヤ対応の選択肢。他の方言はテストケースの参考資料として扱う。

### 2.3 pg_regress（PostgreSQL 本体の回帰テスト）
- `src/test/regress/{sql,expected}/*.sql|.out` を **psql に流し、その出力テキストを diff する**方式（https://github.com/postgres/postgres/tree/master/src/test/regress ）。`make installcheck` で既存サーバにも接続できる。
- yuzhu に使う場合の障害:
  1. psql の出力整形（表の罫線、`(N rows)`）と psql メタコマンド（`\d`、`\set` など）に依存する。
  2. テストの多くがシステムカタログ、拡張、PL/pgSQL、プランナ出力、ロケール依存に依存する。
  3. テスト間に依存がある（`create_table` → `insert` → ...、schedule で順序が決まっている）。
  4. psql 自体は通常 Simple Query で動くが、`\d` などはカタログ照会を発行する。
- 前例: **RisingWave の `src/tests/regress`** は pg_regress を Rust で書き直したもの。psql を子プロセスで起動して PG と自 DB の両方に流し、`--@ ` コメントでクエリ単位に除外できる（https://github.com/risingwavelabs/risingwave/tree/main/src/tests/regress ）。
- **実現性**: M1 では不可。型・式・関数が揃う中期以降に、`boolean`, `int4`, `int8`, `float8`, `numeric`, `text`, `select`, `join`, `aggregates` などの .sql を **sqllogictest に変換して取り込む**のが最も現実的。PG に対して `--override` すれば期待値は自動で作れる。PostgreSQL License なので取り込みは問題ない（出典表記を残す）。

---

## 3. isolation テスト（後のマイルストーン向け）

### 3.1 PG isolationtester
- spec 形式（https://github.com/postgres/postgres/blob/master/src/test/isolation/README ）:
  ```
  setup { CREATE TABLE ... }
  teardown { DROP TABLE ... }
  session s1
  setup { BEGIN ISOLATION LEVEL SERIALIZABLE; }
  step s1r { SELECT ... }
  step s1c { COMMIT; }
  session s2
  ...
  permutation s1r s2r s1c s2c   # 省略すると全順列
  ```
  ブロッキングマーカー `step(*)`、`step(other)`、`step(other notices N)` がある。
- **プロトコル的な制約 [ソース確認]**（`isolationtester.c`）:
  - 接続時に `PQexecParams("SELECT set_config('application_name', ...)")` を実行する（Extended Query）。
  - ブロッキング検出は `PQprepare` + `PQexecPrepared` で `SELECT pg_catalog.pg_isolation_test_session_is_blocked($1, '{pids}')` を実行する。`pg_locks` の重量ロック待ちしか検出できない。
  - バックエンド pid は `PQbackendPID` で得る（BackendKeyData が必要）。
  - 以上から、isolationtester をそのまま yuzhu に使うには、Extended Query と PG 固有関数のエミュレーションが必要になる。非推奨。

### 3.2 sqllogictest-rs で代用できる範囲
- `connection <name>` で複数セッションを交互に操作できる。ただし **各レコードは完了を待ってから次に進む**ので、ブロックする文があるとランナーが止まる。「ブロックしない（結果だけが変わる）」タイプの anomaly テスト（read committed での non-repeatable read、snapshot isolation での write skew の検出など）は書ける。ロック待ちやデッドロックのテストは書けない。

### 3.3 推奨
- 後のマイルストーンで、**isolationtester の spec 形式を流用した自前ランナー**（Rust + tokio-postgres の simple_query、またはプロトコル直書き）を作る。
  - 各 step を非同期に送り、一定時間（例: 200ms）応答がなければ「waiting」と判定する。PG 固有関数は不要になる。
  - 同じ spec を PG に流して期待出力を生成する。
- テストケースの発想源: PG の `src/test/isolation/specs/*.spec`、Hermitage（https://github.com/ept/hermitage 、各分離レベルの anomaly を 2 セッションの SQL 手順で列挙したもの）。
- 以下は筆者の知見: さらに先では Jepsen/Elle 系の履歴検査も選択肢になる。

---

## 4. 推奨構成

### 4.1 ランナーの採用方針（段階的）
1. **M1: `sqllogictest-bin` 0.29.1 のビルド済みバイナリ（または `cargo install --locked --version 0.29.1 sqllogictest-bin`）+ `--engine postgres`。**
   - ラベル運用: エンジン名はどちらも `postgres` になるので、`--label pg` / `--label yuzhu`（必要なら `--label yuzhu-rust`）を付けて `onlyif yuzhu` / `skipif yuzhu` で差分を吸収する。ただし **`skipif yuzhu` は「未実装の一時回避」に限定**する。PG と挙動が違う部分は原則として yuzhu 側のバグとして扱う。
2. **M2 前後: リポジトリ内に薄い Rust 製ランナー `tools/slt-runner` を作る**（テストツールなので外部ライブラリ可）。`sqllogictest` crate をライブラリとして使い、自前の `AsyncDB` を実装する。
   - Simple Query の RowDescription の型 OID から型文字列を導き（int2/4/8→`I`、float4/8/numeric→`R`、text/varchar/bpchar/name→`T`、それ以外→`?`）、`strict_column_validator` で **型文字列と列数を検証**する。
   - エラー文字列を自前で整形する（例: `ERROR: <message>` + SQLSTATE）。tokio-postgres のバージョンに依存しなくなる。
   - DB の作成・破棄、タイムアウト、PG/yuzhu の切替を自前で制御できる。
   - .slt ファイルはそのまま使い回せる（フォーマットは同じ）。
3. 後のマイルストーン: `postgres-extended` 相当のジョブ（Extended Query 対応時）、SQLite コーパスや pg_regress の部分集合を変換した nightly ジョブ、isolation ランナー。

### 4.2 ディレクトリ構成案（モノレポ）
```
tests/
  slt/
    m1/                      # マイルストーンごと（累積: 実装 Mn は m1..mn を全部流す）
      startup/connect.slt
      select/literals.slt
      select/expressions.slt
      ddl/create_table.slt
      dml/insert.slt
      dml/select_where.slt
      errors/sqlstate.slt
    m2/
      join/...
      aggregate/...
    _include/                # include 用の共通部品（*.slt.part）
    imported/                # 後で: sqlite/, pg_regress/ 由来（出典・ライセンス表記付き）
  isolation/                 # 後で: *.spec
tools/
  slt-runner/                # 後で: 自前ランナー（Rust）
scripts/
  slt.sh                     # 対象（pg|yuzhu-rust|...）とポートを受け取って実行
.github/workflows/slt.yml
```
- 1 ファイル 1 機能・数十レコード程度にする。ファイル冒頭で使うテーブルを作り、末尾で DROP する（`-j` なしでは DB を共有するため）。テーブル名にはファイル固有の接頭辞を付ける（例: `dml_insert_t1`）。M1 で `DROP TABLE IF EXISTS` が未実装なら、末尾の DROP を必須にする。
- マイルストーン単位ではなく機能単位（`tests/slt/<feature>/`）に分け、マイルストーンを glob リストで管理する方法もある。今回は「実装 Mn は m1..mn を流す」という単調な運用が分かりやすいので、提案どおり `m1/<feature>/` を推奨する。

### 4.3 CI（GitHub Actions + サービスコンテナ）
- 参考: https://docs.github.com/en/actions/use-cases-and-examples/using-containerized-services/creating-postgresql-service-containers

```yaml
name: slt
on: [push, pull_request]
env:
  SLT_VERSION: v0.29.1
jobs:
  validate-on-postgres:          # 期待値そのものが本物の PG で正しいか
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:17       # 本番運用では digest 固定 (postgres:17@sha256:...)
        env:
          POSTGRES_PASSWORD: postgres
          POSTGRES_INITDB_ARGS: "--encoding=UTF8 --locale=C"
        ports: ["5432:5432"]
        options: >-
          --health-cmd "pg_isready -U postgres"
          --health-interval 2s --health-timeout 5s --health-retries 30
    steps:
      - uses: actions/checkout@v4
      - name: install sqllogictest (prebuilt, tokio-postgres 0.7.12 同梱)
        run: |
          curl -sSL https://github.com/risinglightdb/sqllogictest-rs/releases/download/${SLT_VERSION}/sqllogictest-bin-${SLT_VERSION}-x86_64-unknown-linux-musl.tar.gz | tar xz -C /usr/local/bin
      - run: sqllogictest -h localhost -p 5432 --label pg --junit slt-pg 'tests/slt/**/*.slt'

  test-yuzhu-rust:
    runs-on: ubuntu-latest
    needs: validate-on-postgres
    steps:
      - uses: actions/checkout@v4
      - run: cargo build --release -p yuzhu-server
      - run: ./target/release/yuzhu-server --port 5433 & 
      - run: for i in $(seq 30); do (echo > /dev/tcp/127.0.0.1/5433) 2>/dev/null && break; sleep 1; done
      - name: install sqllogictest
        run: curl -sSL .../sqllogictest-bin-${SLT_VERSION}-x86_64-unknown-linux-musl.tar.gz | tar xz -C /usr/local/bin
      - run: sqllogictest -p 5433 --label yuzhu --shutdown-timeout 5 --junit slt-yuzhu 'tests/slt/m1/**/*.slt'
```
- 実装が増えたら `test-yuzhu-<lang>` を matrix 化する。対象マイルストーンの glob も matrix 変数にする。
- PG のメジャーバージョン（17/18…）は固定する。上げるときは差分をレビューする独立した PR にする。

### 4.4 落とし穴チェックリスト
| 項目 | 内容・対策 |
|---|---|
| ランナーのバージョン | `--locked` かビルド済みバイナリを使う。そうしないとエラー文字列が `db error` だけになる（§1.5）。CI でバージョンを固定する |
| エラー照合 | SQLSTATE `(42P01)` を主にする。yuzhu は PG と同じ SQLSTATE を返すのが前提になるので、SQLSTATE 一覧を設計に入れる。regex は短い部分一致に留め、`db error: ERROR:` 接頭辞を含めない |
| 型文字列 | CLI では未検証（§1.6）。それでも I/T/R は正しく書いておく（将来の自前ランナーで strict 検証するため） |
| NULL / 空文字列 | `NULL` / `(empty)`。文字列 `'NULL'` や `'(empty)'` とは区別できないので、テストで使わない |
| 空白 | 正規化で空白が潰れる。末尾空白、連続空白、タブを含む値は `length()` や `quote_literal()`/`format('%L')` で間接的に検査する |
| 浮動小数 | simple は PG の float8out（PG12+、`extra_float_digits=1` 既定で shortest-exact）をそのまま比較する。yuzhu も同じアルゴリズム（Ryu 相当）で出力する必要がある。`1e+20`、`Infinity`、`NaN`、`-0` に注意。M1 では float を避けるか、`round()` / numeric キャストで回避する |
| numeric | スケールが保持される（`1.50`）。演算結果のスケール規則も PG と同じにする必要がある |
| bool | `t` / `f` |
| 順序 | ORDER BY がない結果は `rowsort` を付ける。テキストの ORDER BY はロケール依存なので、PG は `--locale=C` で initdb する（DataFusion の pg_compat も C collation を推奨）。yuzhu も既定はバイト順にする |
| 0 行の結果 | `query` で期待値が空なら PASS（StatementComplete 扱い） |
| 複数文 | 1 レコード 1 文にする（simple エンジンは最初の文の結果しか見ない） |
| 接続終了時の CancelRequest | yuzhu は新規接続で来る CancelRequest を処理するか、無視して切断する。`--shutdown-timeout` も付ける |
| 起動メッセージ | SSLRequest は来ない（NoTls）。StartupMessage → AuthenticationOk → (ParameterStatus/BackendKeyData 任意) → ReadyForQuery で十分。将来の psql 等のために `server_version`、`client_encoding`、`DateStyle`、`integer_datetimes`、`standard_conforming_strings` などの ParameterStatus も送るのが望ましい |
| `-j` | CREATE/DROP DATABASE が必要なので、M1 では使わない |
| `retry` | `--override` と併用できない（issue #275） |
| 環境依存 | TimeZone、DateStyle、lc_messages はセッション既定を固定する（PG 側は image 既定の UTC / ISO,MDY / 英語）。now() などの揮発値は `<slt:ignore>` で扱う |
| extended vs simple | 描画が違う（例: 1e20）。共通期待値は simple 基準にし、extended 用は別ジョブにするか `skipif postgres-extended` を付ける |

---

## 5. 今回の検証ログ（再現手順）
- `docker run -d -e POSTGRES_PASSWORD=postgres -p 55432:5432 postgres:17 -c log_statement=all`
- `cargo install --root A --locked sqllogictest-bin` と `cargo install --root B sqllogictest-bin` で 2 つのビルドを用意（どちらも 0.29.1。tokio-postgres はそれぞれ 0.7.12 / 0.7.18）
- probe.slt（rowsort、NULL、`(empty)`、float、numeric、bool、`statement count`、`statement error <regex>`、`statement error (42P01)`、`query error`）の結果:
  - locked + postgres → **OK**
  - unlocked + postgres → regex のエラー期待で **FAIL**（実際のメッセージが `db error` のみ）。SQLSTATE 版は PASS
  - postgres-extended → float8 `1e+20` が `100000000000000000000` になり FAIL
- PG のログで、simple エンジンの接続は `statement:`（Simple Query）だけ、extended は `execute s0:` だった。
- ビルド済みリリースバイナリ（v0.29.1 linux musl）に同梱されているのは tokio-postgres 0.7.12（`strings` で確認）。
- コンテナは削除済み。リポジトリは変更していない。

## 参考 URL
- sqllogictest-rs: https://github.com/risinglightdb/sqllogictest-rs （README、CHANGELOG、`sqllogictest-engines/src/postgres/simple.rs`、`extended.rs`、`sqllogictest-bin/src/main.rs`）
- crates: https://crates.io/crates/sqllogictest-bin , https://crates.io/crates/tokio-postgres
- tokio-postgres CHANGELOG（0.7.15 の Display 変更）: https://github.com/sfackler/rust-postgres/blob/master/tokio-postgres/CHANGELOG.md
- SQLite sqllogictest: https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki 、ミラー https://github.com/gregrahn/sqllogictest
- DataFusion sqllogictest（pg_compat、sqlite コーパス）: https://github.com/apache/datafusion/blob/main/datafusion/sqllogictest/README.md
- DuckDB: https://duckdb.org/docs/current/dev/sqllogictest/intro.html
- CockroachDB logictest: https://github.com/cockroachdb/cockroach/tree/master/pkg/sql/logictest
- Materialize sqllogictest: https://github.com/MaterializeInc/materialize/tree/main/src/sqllogictest
- RisingWave regress: https://github.com/risingwavelabs/risingwave/tree/main/src/tests/regress
- pg_regress: https://github.com/postgres/postgres/tree/master/src/test/regress
- isolationtester: https://github.com/postgres/postgres/tree/master/src/test/isolation
- Hermitage: https://github.com/ept/hermitage
- GitHub Actions PostgreSQL service containers: https://docs.github.com/en/actions/use-cases-and-examples/using-containerized-services/creating-postgresql-service-containers
