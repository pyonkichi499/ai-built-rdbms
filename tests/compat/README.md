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
