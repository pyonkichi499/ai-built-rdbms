# difftest — 差分テストツール

本物の PostgreSQL（正解の基準）と yuzhu に**同じランダムな SQL** を Simple Query で流し、
結果と SQLSTATE を突き合わせるツールです。食い違いを見つけると自動で最小化し、
そのまま流せる SQL スクリプトとシードを出力します。

- 実装言語に依存しない道具として、`impl/rust` のワークスペースとは別の独立した Cargo プロジェクトにしています。
- 乱数生成器は自前（xoshiro256\*\*）なので、**同じシードからは常に同じ SQL** が生成されます（外部クレートの版に左右されません）。
- 生成はサーバの応答を一切見ないため、失敗があってもラウンド内の後続の SQL は変わりません。

## ビルド

```sh
cd tests/tools/difftest
cargo build --release
# バイナリ: target/release/difftest
```

## 使い方

```sh
# 参照用の PostgreSQL 17（C ロケール）を起動
tests/pg.sh start                      # 127.0.0.1:55432

# yuzhu を起動しておき（例: 127.0.0.1:5433）、差分テストを実行
target/release/difftest run \
  --ref  "host=127.0.0.1 port=55432 user=postgres dbname=postgres" \
  --test "host=127.0.0.1 port=5433 user=postgres dbname=postgres" \
  --queries 10000 --out /tmp/difftest-fail
```

接続文字列は libpq 形式でも URL 形式（`postgres://user@host:port/db`）でも構いません。
環境変数 `DIFFTEST_REF` / `DIFFTEST_TEST` でも指定できます。

終了コード: `0` = 差分なし、`1` = 差分あり、`2` = 実行エラー（接続できない等）。

### サブコマンド

| コマンド | 内容 |
|---|---|
| `run` | ランダムなケースを生成して両サーバで比較する（本体） |
| `replay FILE` | SQL スクリプトを両サーバで 1 文ずつ実行して比較する。`run` が出力した失敗スクリプトの再確認に使う。`-v` で全文の結果を表示 |
| `gen --seed S --round R` | シードが生成する SQL を表示するだけ（サーバ不要）。生成器のデバッグ用 |

### 主なオプション（`run`）

| オプション | 既定値 | 内容 |
|---|---|---|
| `--seed N` | 時刻から | シード。実行開始時に必ず表示される |
| `--queries N` | 10000 | SELECT の総数（TLP の分割クエリも数える）に達したら終了 |
| `--start-round R --rounds K` | 0 / 無制限 | ラウンド R から K ラウンドだけ実行（再現用） |
| `--queries-per-round N` | 50 | 1 ラウンド（= 1 つのスキーマとデータ）あたりのクエリ数 |
| `--level m1\|m4\|m5` | m1 | 機能レベル（後述） |
| `--enable a,b` / `--disable a,b` | | 機能を個別に有効化・無効化（`joins`, `aggregates`, `subqueries`, `decimal-literals`, `tlp`） |
| `--tlp-percent N` | 30 | TLP 検査を追加するクエリの割合（%） |
| `--max-tables` / `--max-cols` / `--max-rows` / `--max-depth` | 2 / 5 / 20 / 3 | スキーマ・データ・式の大きさ |
| `--error-match exact\|class\|any` | exact | エラーの照合方法。`exact` は SQLSTATE の完全一致、`class` は先頭 2 文字、`any` はどちらもエラーなら一致 |
| `--compare-names` | オフ | 出力列名（`?column?` や `coalesce` など）も比較する |
| `--max-failures N` | 5 | この数の失敗で打ち切る |
| `--no-minimize` / `--minimize-budget N` | / 2000 | 最小化の無効化 / 最小化での再実行回数の上限 |
| `--out DIR` | | 失敗スクリプトを `DIR/fail-<seed>-<round>-<n>.sql` に保存 |
| `--prefix P` | dt | 生成するテーブル名の接頭辞（`<P><実行ID>_<ラウンド>_<i>`） |
| `-v` | | 全文と両サーバの結果を表示 |

## 生成する SQL

### M1（`--level m1`）

- **スキーマ**: 型 `int2/int4/int8/float4/float8/bool/text/varchar(n)`（`integer`、`double precision`、`character varying(n)` などの別名も使う）。
  `NOT NULL`、`DEFAULT`、`CHECK`（列制約・表制約、名前付き・名前なし）。
- **データ**: 列リストあり／なし、`DEFAULT` キーワード、`DEFAULT VALUES`、複数行の `VALUES`。
  NULL、境界値（各整数型の最小・最大、`NaN`、`±Infinity`、`-0`、`1e308`、`5e-324` など）、
  わざと失敗する値（範囲外、`varchar(n)` より長い文字列、`'0x1F'` や `'1_000'` などの入力形式）も混ぜる。
  INSERT が失敗した場合も、両サーバで同じ SQLSTATE になることを確認する。
- **SELECT**: 算術（`+ - * / %`、単項 `+ -`）、比較（`= <> != < <= > >=`）、`AND/OR/NOT`、`IS [NOT] NULL`、
  `IS [NOT] TRUE/FALSE`、`[NOT] BETWEEN`、`[NOT] IN (...)`、`[NOT] LIKE`、`||`、`CASE`（単純形・検索形）、
  `COALESCE`、`NULLIF`、`CAST(x AS t)` と `x::t`、`length/lower/upper/abs`。
  `*`、列の別名、`t.col` / 表の別名、`DISTINCT`、`ORDER BY`（`ASC/DESC`、`NULLS FIRST/LAST`）、`LIMIT`、`OFFSET`。
- 小数リテラル（`1.5`）は PostgreSQL では numeric 型になるため、M1 では使わず `'1.5'::float8` の形にする。

### M4 以降

| 機能 | 内容 | レベル |
|---|---|---|
| `joins` | 2〜3 表の `JOIN ... ON` / `LEFT` / `RIGHT` / `FULL`（等値条件）/ `CROSS JOIN` / カンマ結合 | m4 |
| `aggregates` | `GROUP BY`、`HAVING`、`count(*)`、`count/sum/avg/min/max/bool_and/bool_or`、`DISTINCT` 付き集約 | m4 |
| `subqueries` | 相関のない `EXISTS`、`IN (SELECT ...)`、スカラーサブクエリ（`ORDER BY 1 LIMIT 1` 付き） | m4 |
| `decimal-literals` | 素の小数リテラル（numeric） | m5 |

浮動小数点の `sum/avg` は加算順で結果が変わるため生成しません。

## 比較の規則

- **結果**: 列数、行数、各セルのテキスト表現（PostgreSQL のテキスト出力そのまま）を比較する。
  - `ORDER BY` なし: 行の多重集合として比較する。
  - `ORDER BY` あり: 生成する `ORDER BY` は**全出力列**を並べるので、出力行の順序は一意に決まる。順序も含めて比較する。
    ただし `-0` と `0` は等しいものとして並ぶため、この 2 つの入れ替わりだけは許容する。
  - `LIMIT/OFFSET` は `ORDER BY` があるときだけ生成する。
- **エラー**: 両方がエラーなら SQLSTATE を比較する（`--error-match`）。片方だけエラーなら差分。
- **接続断**: テスト側の接続が切れた場合（クラッシュ等）も差分として扱い、再接続して続行する。

## TLP（Ternary Logic Partitioning）

クエリ `Q` と、ランダムな述語 `p` について、次が成り立つことを**テスト側サーバ単体で**検査します
（SQLancer の手法）。

```
rows(Q) = rows(Q WHERE p) ⊎ rows(Q WHERE NOT p) ⊎ rows(Q WHERE p IS NULL)
```

元の `WHERE w` がある場合は `w AND p` のように結合します。`DISTINCT` 付きなら和をとった後に重複を除きます。
4 つのクエリはそれぞれ参照側とも差分比較します。いずれかがエラーの場合は恒等式の検査を省きます。
参照側（PostgreSQL）で恒等式が破れた場合は「TLP violation (reference server)」として報告します
（生成器のバグを意味します）。集約クエリには適用しません。

## 最小化と出力

失敗したケース（スキーマ + INSERT + クエリ）を新しいテーブル名で再実行して再現を確認し、
同じ種類の失敗が続く限り次の順で縮めます。

1. INSERT 文を塊ごと削除 → 1 文ずつ削除、複数行 VALUES の行を削除
2. クエリの縮小（出力列・WHERE・HAVING・DISTINCT・LIMIT・OFFSET・ORDER BY・結合の削除、
   結合種別の単純化、別名の除去、式を部分式や単純なリテラルに置き換え）
3. 使われていない表・列の削除
4. 制約（CHECK・NOT NULL・DEFAULT）の削除
5. 挿入値を NULL に置き換え

出力は表名を `t0`, `t1`, ... に正規化した、そのまま流せるスクリプトです。例:

```sql
-- difftest seed=1 round=0
-- reproduce: difftest run --ref ... --test ... --seed 1 --start-round 0 --rounds 1 --level m1 ...
-- kind: query diff
-- reason: result rows differ
DROP TABLE IF EXISTS t0;
CREATE TABLE t0 (c0 real);
INSERT INTO t0 VALUES ('33.211e-2');
SELECT (c0 - ('689'::float8)) FROM t0;
-- failing statement:
--   SELECT (c0 - ('689'::float8)) FROM t0
-- reference: 1 row(s), columns [?column?]
--              ("-688.6678900122643")
-- test     : 1 row(s), columns [?column?]
--              ("-688.667890012264")
DROP TABLE IF EXISTS t0;
```

`difftest replay --ref ... --test ... fail-1-0-1.sql` で差分を再確認でき、`psql -f` でも流せます。
`reproduce:` の行のコマンドで、最小化前のラウンド全体を再現できます。

## 動作確認（PostgreSQL 対 PostgreSQL）

同じサーバ上の 2 つのデータベースを参照側・テスト側にして、差分が 0 件になることを確認しています。

```sh
docker exec <container> psql -U postgres -c 'CREATE DATABASE ref_db' -c 'CREATE DATABASE test_db'
target/release/difftest run --seed 1 --queries 10000 \
  --ref  "host=127.0.0.1 port=55432 user=postgres dbname=ref_db" \
  --test "host=127.0.0.1 port=55432 user=postgres dbname=test_db"
# difftest: 90 rounds, 10000 SELECTs (607 errored identically on both), 1386 TLP checks, ..., 0 failure(s)
```

検出と最小化の確認には、テスト側に `ALTER DATABASE test_db SET extra_float_digits = 0` を設定した
データベースを使うと、浮動小数点の出力の差分が見つかり、上の例のように最小化されます。

## 注意点

- yuzhu 側で同じ評価順序を採らない場合、1 つのクエリで複数のエラー（例: 22012 と 22003）が起こりうるときに
  SQLSTATE が食い違うことがあります。気になる場合は `--error-match class` を使ってください。
- 文のタイムアウトはありません。テスト側がハングすると difftest も止まります。
- テーブルは各ラウンドの最後に `DROP TABLE IF EXISTS` で削除します（失敗しても無視）。
  テーブル名には実行ごとの ID が入るため、DROP が未実装でも名前は衝突しません。
