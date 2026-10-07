# difffuzz — 差分ファザー

本物の PostgreSQL 17（正解）と yuzhu に**同じ SQL を同じ順序で**流し、結果（行・コマンドタグ・エラーの SQLSTATE とメッセージ）を
正規化して突き合わせます。差分は SQL・両者の出力・シードつきで JSON Lines に出します。

- 独立した Cargo プロジェクト（外部クレートなし、std のみ）。接続は `psql` を子プロセスで呼びます（既定 `/usr/lib/postgresql/17/bin/psql`）。
- 乱数は自前（xorshift64\*）。`(seed, case)` が決まれば生成される SQL は常に同じです。生成はサーバの応答を見ません。
- 接続は文ごとではなく**シナリオ（複数文）単位で 1 セッション**（PG と yuzhu それぞれ psql を 1 つ起動）。
- 生成 SQL は PG で成功するもの約 7 割、エラーになるもの約 3 割を目安にしています（領域により前後します）。
- シナリオの最初の差分で打ち切ります（以降は両者の状態が食い違うため）。

## ビルドと実行

```sh
cd tests/tools/difffuzz
cargo build --release          # target/release/difffuzz

sandbox/pg.sh start            # PG: 127.0.0.1:55432（起動済みなら再利用）
tests/yuzhu.sh start --port 55433   # yuzhu（バイナリは YUZHU_BIN_DIR などで指定。tests/yuzhu.sh 参照）

target/release/difffuzz --domain expr --seed 1 --cases 5000 \
  --pg    "host=127.0.0.1 port=55432 user=postgres dbname=postgres" \
  --yuzhu "host=127.0.0.1 port=55433 user=postgres dbname=postgres" \
  --out /tmp/difffuzz.jsonl
```

終了コード: `0` = 差分なし、`1` = 差分あり、`2` = 実行エラー（接続できない等）。

### オプション

| オプション | 内容 |
|---|---|
| `--domain` | `expr` `types` `query` `dml` `txn`（M1〜M3）、`join` `agg` `subquery` `setop` `index` `ddl`（M4）、`all`（既定 all。case 番号で領域を順番に切り替える） |
| `--seed N` | シード（省略時は時刻から） |
| `--cases N` / `--start N` | case 番号 `start .. start+N` を実行（既定 1000 件、start 0） |
| `--case N` | その 1 ケースだけ再現（`--seed` と `--domain` は差分出力と同じ値を渡す） |
| `--pg` / `--yuzhu` | libpq の接続文字列（環境変数 `DIFFFUZZ_PG` / `DIFFFUZZ_YUZHU` でも可。既定は PG 55432、yuzhu 55433） |
| `--out FILE` | 差分を JSON Lines で追記（省略時は標準出力）。進捗と集計は標準エラー |
| `--no-message` | エラーメッセージの差を無視（SQLSTATE の差だけ見る） |
| `--skip-unsupported-legacy` | yuzhu が `0A000` を返した文を、M1〜M3 の領域（expr types query dml txn）だけ、PG のエラーの有無に関わらず数えるだけにする。M4 の領域の `0A000` は差分のまま（生成器が避けるはずの構文なので）。`done-check.sh` が使う |
| `--skip-unsupported` | PG が成功し yuzhu が `0A000`（未対応）を返した文で、そのケースを打ち切って数えるだけにする |
| `--ignore-trailing-space` | 行の各フィールドの末尾の空白を無視（M4 領域の読み取り文だけ。char(n) の詰め物の差を別扱いにして、ほかの差分を見るとき） |
| `--exclude NEEDLE` | NEEDLE を含む文を両方のサーバへ流さない（何度でも指定可。既知の差が他の差分を隠すとき。`done-check.sh` は `known-excludes.txt` から読む） |
| `--timeout-ms N` | 1 文あたりの待ち時間（既定 10000）。超えると `timeout` として差分になる |
| `-v` | 全文と両者の結果を標準エラーに出す |

`--domain all` のときの領域は `case % 11`（expr, types, query, dml, txn, join, agg, subquery, setop, index, ddl の順）で決まります。

## 差分の再現

出力の各行は次の形です。

```json
{"seed":7,"case":13,"domain":"types","step":7,"kind":"sqlstate","sql":"INSERT ...;",
 "pg":{"status":"error","sqlstate":"22003","message":"integer out of range"},
 "yuzhu":{"status":"error","sqlstate":"0A000","message":"..."},
 "scenario":["CREATE TABLE ...;", "...", "INSERT ...;"]}
```

- `kind`: `rows`（行が違う）/ `tag`（コマンドタグが違う）/ `sqlstate` / `message` / `yuzhu_error_pg_ok` / `yuzhu_ok_pg_error` / `status`（timeout・切断など）
- `scenario`: 差分が出た文までのシナリオ全体。そのまま 1 文ずつ両方に流せば手で再現できます。
- 同じ生成で再現: `difffuzz --domain <domain> --seed <seed> --case <case> -v`（`--domain all` で出した行は `domain` の値をそのまま渡す）。

## 比較の正規化

- psql は `-A -t -F '|'`、NULL は `<NULL>`、`VERBOSITY=verbose`。標準エラーは標準出力に合流させ、文ごとに `\warn @@END@@ n` で区切る。
- 行は文字列のまま比較（生成側が `ORDER BY 1, 2, ...` で順序を決定的にする）。タグは INSERT/UPDATE/DELETE/DDL/トランザクション文のみ。
- エラーは `ERROR:  <SQLSTATE>: <メッセージ>` の先頭行のみ。`DETAIL` / `HINT` / `LOCATION` / `LINE` とキャレット、`NOTICE` / `WARNING` は比較しない。
- 生成した文は 1 行で `;` 終わり、引用符・括弧が釣り合うようにしてあります（psql が文の終わりを見失うと待ちぼうけになるため）。

## M4 の領域（join / agg / subquery / setop / index / ddl）

11 §3.7.3 の表に対応します。表は `k` `a`（integer）を必ず持ち、ほかに `b`（bigint）`s`（text）`n`（numeric(6,2)）`c`（char(3)）`d`（date）`ts`（timestamp）`f`（boolean）を乱数で選びます
（列名と型が 1 対 1 なので NATURAL / USING が使えます）。主キー（単一・複合・serial）と UNIQUE を持つ表も混ざります。浮動小数・interval・タイムゾーンは使いません。

| 領域 | 生成するもの |
|---|---|
| `join` | INNER / LEFT / RIGHT / FULL（等値 + 残りの条件）/ CROSS、NATURAL・USING、2〜4 表の連鎖、派生表・`generate_series` との結合、WHERE の述語 |
| `agg` | GROUP BY / HAVING、主キーへの関数従属（主キーだけで GROUP BY）、DISTINCT ON、`count` `sum` `avg` `min` `max` `bool_and` `bool_or`、FILTER、DISTINCT 引数、42803 のエラー |
| `subquery` | IN / NOT IN / EXISTS / スカラー / ANY / ALL（NULL を含む表）、相関（2 段、UNION・INTERSECT の腕からの相関）、CTE（参照 0・1・2 回、MATERIALIZED）、派生表 |
| `setop` | UNION / INTERSECT / EXCEPT（ALL の有無）、括弧による結合順、集約・定数の腕、int4/int8 の混在、副問い合わせ・CTE の中 |
| `index` | PRIMARY KEY / UNIQUE / CREATE INDEX（単一・複合・DESC・NULLS FIRST）を持つ表への等値・範囲・IN・IS NULL・ORDER BY（LIMIT）・min/max、重複キーの INSERT（23505）、UPDATE / DELETE の後の全内容 |
| `ddl` | CREATE / DROP INDEX、ALTER TABLE ADD PRIMARY KEY / UNIQUE、TRUNCATE（RESTART IDENTITY など）、serial、DROP TABLE の連鎖（IF EXISTS / CASCADE）と `pg_class` / `pg_index` の突き合わせ |

生成しない構文は KD-1〜KD-27（`0A000` になるもの。LATERAL、再帰 CTE、`IS DISTINCT FROM`、FULL JOIN ON true、ウィンドウ関数、`ANY(array)` など）です。`gen/m4_tests.rs` が目印の文字列で確かめます。

### 比較の規則（11 §3.7.2）

M4 の読み取り文（`Ctx::push_q`）は次の規則で比べます（`src/compare.rs`）。

- 行は多重集合として比べる。ORDER BY が全出力列のときだけ順序も比べる（LIMIT は全順序のときだけ生成）。
- `-0` と `0`（`-0.00` など）の入れ替わりは許容する。
- 評価順で出る出ないが変わりうるエラー（クラス 21・22 のエラーと行の結果、またはクラス 21・22 どうしでコードが違う）は `inconclusive` として数えるだけで、差分にしない。
- 浮動小数の sum / avg は生成しない。

### 同じサーバ上の変種の検査（11 §3.7.3 の (d)(e)）

`pairs` に登録した 2 文は、PostgreSQL・yuzhu のそれぞれの中で結果が同じでなければなりません。差分の `kind` は `variant_pg` / `variant_yuzhu`、`paired` は基準にした文の添字です。

- プラン変種: 同じ問い合わせを `SET enable_hashjoin / nestloop / indexscan / seqscan / hashagg / material / sort = off`（と組み合わせ）の下で流す。FULL JOIN を含む文は hashjoin を切らない（KD-2）。
- 索引の有無: 同じデータの「索引・制約つきの表」と「何もない表」に同じ問い合わせを流す（`index` 領域）。

### PostgreSQL 対 PostgreSQL の自己検査

生成器自身の確認に、同じ PostgreSQL の別データベースを相手にして差分 0 を確かめます（テーブル名が衝突しないよう、別々のデータベースにします）。

```sh
psql "host=127.0.0.1 port=55432 user=postgres dbname=postgres" -c "CREATE DATABASE fz_a" -c "CREATE DATABASE fz_b"
difffuzz --domain join --seed 1 --cases 300 \
  --pg    "host=127.0.0.1 port=55432 user=postgres dbname=fz_a" \
  --yuzhu "host=127.0.0.1 port=55432 user=postgres dbname=fz_b"
```

## 後始末

テーブル名は `fz_<seed>_<case>_t<n>`。シナリオの最後に両方のサーバで `ROLLBACK;` と `DROP TABLE IF EXISTS ...` を流します。

## 生成器の拡張

`src/gen/` に領域ごとのモジュールがあります（`expr` `types` `query` `dml` `txn`、M4 は `m4.rs`（共通）と `m4_join` `m4_agg` `m4_subq` `m4_setop` `m4_index` `m4_ddl`、共通の部品は `mod.rs`、型ごとのリテラルは `values.rs`）。

1. `fn scenario(ctx: &mut Ctx)` を持つモジュールを作る（`ctx.rng` で乱数、`ctx.push(sql)` で文を積む）。
2. `gen/mod.rs` の `DOMAINS` と `generate` に 1 行足す。

式は `expr::expr(rng, scope, cls, depth)`（系統: Int / Num / Text / Bool）で作れます。列スコープを渡すと列参照も混ざります。
エラー文は `gen::error_stmt(ctx)`、`error_stmt` に項目を足せばエラーの種類を増やせます。
