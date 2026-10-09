# tests/compat

周辺ツール（psql 17、pgbench 17）の互換テスト。設計は `spec/design/m4/11-tests-plan.md` §3.5、`10-explain-copy-compat.md` §7〜§8。

```sh
tests/compat/run.sh --target pg    --port 55432 [--quick] [psql|copy|pgbench]
tests/compat/run.sh --target yuzhu --port 5432  [--quick]
tests/compat/run.sh --target pg --update psql copy      # expected/ を PostgreSQL の出力で作り直す（差分をレビューしてコミット）
```

- サーバは**新しい状態**で起動してから流す（`\dt` に他のテストの残りが混ざる）。pg は `sandbox/pg.sh stop && start`、yuzhu は `tests/yuzhu.sh clean && start`。
  `tests/done-check.sh` の条件 5 が自動でやる。
- データベースは `postgres` の `public` だけ（`CREATE DATABASE` / `CREATE SCHEMA` は M5。KD-29）。表は各スクリプトが `compat_*` を作って最後に DROP する。
- `psql/*.sql`（`\dt` `\dn` `\di` `\ds` `\d シーケンス` `\l`）と `copy/*.sql`（COPY のデータを `\.` つきで流す）は、
  `psql -X -q` の標準出力・標準エラーを正規化（`lib.sh` の `normalize`）して `expected/*.out` と比べる。`\l` は Name と Owner の 2 列だけを比べる。
- `pgbench/{init,run,crash}.sh` は終了コードと不変条件（`invariants.sql`）だけを検査する。`crash.sh` は pgbench の最中にサーバを kill -9 する
  （pg は `tests/pg.sh crash`（`PG_DATA` が起動したデータディレクトリを指すこと）、yuzhu は `tests/yuzhu.sh crash`）。
- 新しいツールへの対応: `log_statement = 'all'` の PostgreSQL にツールを流して、送る文を記録し、`psql -E` / `pgbench` の出力と見比べる。
- psql / pgbench は 17 系でなければ失敗する（`psql` が送る SQL が版で変わるため）。

## drivers スイート（ドライバのシナリオ）

設計は `spec/design/m5/10-tests-plan.md` §6.10.1・§6.10.2。

```sh
tests/compat/run.sh --target pg    --port 55432 --suite drivers [--driver tokio-postgres|psycopg] [--auth trust|scram]
tests/compat/run.sh --target yuzhu --port 5432  --suite drivers --driver psycopg
```

- 各ドライバは `drivers/<名前>/run.sh` を持ち、1 シナリオ 1 行 `RESULT <ID> <ok|FAIL|KD> <詳細>` を標準出力に出す。
  `run.sh` が「ドライバ・シナリオ ID・結果」の表にまとめ、失敗が 1 つでもあれば終了コード 1。
  `--driver` を省略すると `drivers/*/run.sh` が存在する全ドライバを流す。カンマ区切りで複数指定もできる（`--driver a,b`）。
- 接続先は `PGHOST` / `PGPORT` / `PGUSER` / `PGDATABASE` / `PGPASSWORD`、`TARGET`（pg | yuzhu）、`AUTH`（trust | scram）で渡す。
  `--auth scram` は PGPASSWORD の既定が `yuzhu-test-pw`（`sandbox/pg.sh start --auth scram` の postgres のパスワード）。
  サーバを SCRAM で起動するのは呼び出し側（`--auth` は接続側の期待を切り替えるだけ）。
- シナリオは言語側のアサーション（値の往復、SQLSTATE、行数）で書く。PostgreSQL の結果との diff は取らない。
  **同じコードを PostgreSQL 17 に流して全部通ること**が先（`--target pg`）。
- KD（既知の未対応）に当たるシナリオは、`--target yuzhu` では指定の SQLSTATE で失敗することを期待して `KD` と表示し、
  `--target pg` では成功を期待する。yuzhu で成功してしまったら FAIL（M6 などで解消したら期待を反転する）。
  現在の KD シナリオ: `C6`（psycopg の名前つきカーソル。KD-22）、`C4-nested`（入れ子のトランザクション = SAVEPOINT。KD-4）、
  `C-copy`（COPY。XQ-6 が入るまで 0A000）。
- 依存は固定版: tokio-postgres は `drivers/tokio-postgres/Cargo.lock`（`cargo --locked`。独立した Cargo プロジェクトで、yuzhu のワークスペースには入れない）、
  psycopg は `drivers/psycopg/uv.lock`（`uv run --frozen`。Python 3.13、`psycopg[binary]==3.2.13`）。
  **ネットワークが要る**ので、claude-sandbox（オフライン）では流さない（CI とネットワークのあるローカルだけ）。
- 現在のシナリオ ID: C1 接続（trust / SCRAM + 誤パスワード）、C2 型の往復（psycopg はテキスト / バイナリの両方向、tokio-postgres はバイナリ）、
  C2-infer（tokio-postgres: `prepare` の `params()` と結果列の型 OID）、C3 準備済み文の再利用、C4 トランザクション、C5 SQLSTATE とフィールド、
  C6 行数制限（tokio-postgres: `query_portal` と `max_rows`）、C7 パイプライン / バッチ、C8 8 並列の送金、C9 キャンセル、C10 失敗後の復帰。
- 未実装: pgJDBC・node-postgres（別担当）、`--suite auth`（`auth/run.sh` が無ければ FAIL を出す）。
