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
| `--domain` | `expr` `types` `query` `dml` `txn` `all`（既定 all。case 番号で領域を順番に切り替える） |
| `--seed N` | シード（省略時は時刻から） |
| `--cases N` / `--start N` | case 番号 `start .. start+N` を実行（既定 1000 件、start 0） |
| `--case N` | その 1 ケースだけ再現（`--seed` と `--domain` は差分出力と同じ値を渡す） |
| `--pg` / `--yuzhu` | libpq の接続文字列（環境変数 `DIFFFUZZ_PG` / `DIFFFUZZ_YUZHU` でも可。既定は PG 55432、yuzhu 55433） |
| `--out FILE` | 差分を JSON Lines で追記（省略時は標準出力）。進捗と集計は標準エラー |
| `--no-message` | エラーメッセージの差を無視（SQLSTATE の差だけ見る） |
| `--skip-unsupported` | PG が成功し yuzhu が `0A000`（未対応）を返した文で、そのケースを打ち切って数えるだけにする |
| `--timeout-ms N` | 1 文あたりの待ち時間（既定 10000）。超えると `timeout` として差分になる |
| `-v` | 全文と両者の結果を標準エラーに出す |

`--domain all` のときの領域は `case % 5`（expr, types, query, dml, txn の順）で決まります。

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

## 後始末

テーブル名は `fz_<seed>_<case>_t<n>`。シナリオの最後に両方のサーバで `ROLLBACK;` と `DROP TABLE IF EXISTS ...` を流します。

## 生成器の拡張

`src/gen/` に領域ごとのモジュールがあります（`expr` `types` `query` `dml` `txn`、共通の部品は `mod.rs`、型ごとのリテラルは `values.rs`）。

1. `fn scenario(ctx: &mut Ctx)` を持つモジュールを作る（`ctx.rng` で乱数、`ctx.push(sql)` で文を積む）。
2. `gen/mod.rs` の `DOMAINS` と `generate` に 1 行足す。

式は `expr::expr(rng, scope, cls, depth)`（系統: Int / Num / Text / Bool）で作れます。列スコープを渡すと列参照も混ざります。
エラー文は `gen::error_stmt(ctx)`、`error_stmt` に項目を足せばエラーの種類を増やせます。
