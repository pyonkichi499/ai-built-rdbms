# tests/

言語非依存の共有テストスイート（[sqllogictest](https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki) 形式）と、
その実行スクリプトを置くディレクトリです。

- テストは PostgreSQL ワイヤプロトコル（Simple Query）経由で実行します。
  そのため、どの言語の yuzhu 実装に対しても、本物の PostgreSQL に対しても同じテストを流せます。
- **PostgreSQL 17（C ロケールで initdb したもの）が正解の基準**です。PostgreSQL で通らないテストは「期待値が間違っている」とみなし、置きません。

```
tests/
├── run.sh            スイートの実行スクリプト
├── pg.sh             検証用の PostgreSQL 17 コンテナを起動・停止する
└── slt/
    └── m1/<機能>/*.slt   M1 の範囲のテスト（ddl, insert, constraints, select, expressions, types, functions, txn, session, errors）
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
tests/run.sh --target pg          # tests/slt/m1 以下をすべて実行
tests/pg.sh stop                  # コンテナを削除
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
| ファイル・ディレクトリ | `tests/slt/m1`（ディレクトリを渡すと、その下の `*.slt` をすべて実行） |

- 例: `tests/run.sh --target yuzhu tests/slt/m1/select tests/slt/m1/ddl/create_table.slt`
- `--engine postgres`（Simple Query のみ）で実行し、`--label pg` または `--label yuzhu` を付けます。
- 環境変数 `SLT_BIN` でランナーのパスを、`SLT_EXTRA_ARGS` で追加の引数（例: `--fail-fast`）を渡せます。
- テストが途中で失敗すると、そのファイルのテーブルが残ることがあります。次の実行が `42P07` で失敗したら、
  PostgreSQL は `tests/pg.sh stop && tests/pg.sh start` で作り直し、yuzhu（M1 はメモリ上）は再起動してください。

### isolation テスト（ブロックする交互実行）

`tests/isolation/specs/*.spec` は PostgreSQL の isolationtester と同じ形式のテストで、
自作ランナー `tests/tools/isolation`（Simple Query のみ）で実行します。詳しくは
[tests/tools/isolation/README.md](tools/isolation/README.md) を参照してください。

```sh
cargo build --release --manifest-path tests/tools/isolation/Cargo.toml
tests/tools/isolation/target/release/yuzhu-isolation --port 55432 tests/isolation/specs              # PostgreSQL
tests/tools/isolation/target/release/yuzhu-isolation --port 5432 --blocking-detection timeout tests/isolation/specs  # yuzhu
```

### CI

`.github/workflows/ci.yml` に 2 つのジョブがあります。

- `slt-pg`: postgres:17 のサービスコンテナに対してスイートを流し、期待値が正しいことを確かめます。
- `slt-yuzhu`: yuzhu-server をビルド・起動してスイートを流します（実装が揃うまでは `continue-on-error: true`）。

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
- M1 の範囲外の機能（JOIN、集約、サブクエリ、UPDATE / DELETE、PRIMARY KEY / UNIQUE、numeric・日付型など）は使いません。

### onlyif / skipif

- yuzhu 向けのラベルは `yuzhu`、PostgreSQL 向けは `pg` です（エンジン名 `postgres` は両方に付きます）。
- `skipif yuzhu` は「未実装の一時回避」に限り、理由をコメントで残します。PostgreSQL と挙動が違う部分は yuzhu のバグとして扱います。

### 期待値の生成

`--override` は**本物の PostgreSQL に対してだけ**使い、差分をレビューしてからコミットします。yuzhu に対して `--override` を使ってはいけません。

```sh
SLT_EXTRA_ARGS=--override tests/run.sh --target pg tests/slt/m1/select/new_test.slt
git diff tests/slt
```
