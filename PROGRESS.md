# 進捗と再開手順

最終更新: 2026-10-05（M1〜M3 完了。次は M4）

## 完了したこと

- 要件定義書（Claude Docs）: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- 調査レポート: `spec/research/`（プロトコル、型、sqllogictest、Rust 製 DB の構成、M2〜M5 の事前調査）
- 設計書: `spec/design/m1.md`（契約は `m1-changes.md` で改訂）、`m2.md`、`m3.md`
- M1 実装: 手書きパーサ・アナライザ・実行・セッション・サーバ（`yuzhu-core`、`yuzhu-server`、`yuzhu-numeric`、`yuzhu-datetime`）
- M1 の確認結果（2026-10-04）:
  - `tests/run.sh --target yuzhu`: `tests/slt/m1` の 37 ファイル中 37 通過、失敗 0
  - `tests/run.sh --target pg`（PostgreSQL 17）: 37 ファイル中 37 通過、失敗 0
  - `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` すべて通過
- CI（fmt・clippy・test）、`README.md`、`CLAUDE.md`、claude-sandbox 用コンテナ

## M2 の状況（完了）

到達点: 8KB ページ、バッファプール、ヒープ、リレーションごとのファイル、カタログのテーブル化、UPDATE / DELETE、initdb、
xmin/xmax とコミットログによる ROLLBACK、停止チェックポイント、SIGTERM での正常停止、psql `\l` 用の pg_database 周り。
レビュー修正（checkpoint の毒状態拒否、snapshot_any、セッションの poison 再確認、smgr create の後始末、psql 互換の差異 7 件）を反映済み。

確認結果（2026-10-04）:

- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test`: すべて通過（yuzhu-core の単体テスト 466 件ほか、失敗 0）
- `tests/run.sh --target yuzhu tests/slt/m1 tests/slt/m2`: 61 ファイル中 61 通過
- `tests/run.sh --target yuzhu --restart`: 15 通過（m3 のシナリオは対象外）
- `tests/run.sh --target pg tests/slt/m1 tests/slt/m2`（PostgreSQL 17）: 61 中 61 通過。`--restart` は 15 通過

未解決事項:

- `cargo build --release` で `storage/buffer/track.rs` の `LatchRec` の `tag` / `mode` が未読という警告が 1 件ある（clippy -D warnings は通る）。
- 名前付き CHECK 制約の重複エラーの文言が PG と違う（`analyzer/ddl.rs`）。
- slt の一部（`m2/catalog/pg_attribute`・`constraint_attrdef`・`m2/ddl/drop_cleanup`）は末尾でテーブルを消さないため、使用済みの DB に流すと「already exists」で失敗する（新しい DB では通る）。
- poison の競合を再現する session レベルの回帰テストと、シグナル登録順の修正のテストは未追加。
- 実物の psql 17 で `\l` を流す確認は未実施（同等の SELECT を統合テストで確認）。

## M3 の状況（完了）

到達点: REDO のみの WAL（独自形式、`pg_wal/` のセグメント）、コミットログの永続化、ファジーチェックポイント、full page write、クラッシュリカバリ
（起動時に自動で REDO）、`yuzhu-waldump`、トランザクションの残りの意味論（READ ONLY、AND CHAIN、`SET TRANSACTION`、SAVEPOINT 系のエラー、
`statement_timeout` / `lock_timeout` / `idle_in_transaction_session_timeout` / `idle_session_timeout`、CancelRequest）、分離性テスト用の関数（`pg_sleep`、`pg_backend_pid`、`pg_isolation_test_session_is_blocked`）、
クラッシュ試験（プロセス内の障害注入 `crash_sim` と、実プロセスの `kill -9` 試験）。レビュー指摘（WAL・REDO・バリア・pg 互換・クラッシュ試験の網羅）を反映済み。

確認結果（2026-10-05）:

- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test`: すべて通過（yuzhu-core lib 676 件、crash_sim 49 件、pg_compat_review 6 件、yuzhu-server の統合テストほか。失敗 0）
- `tests/run.sh --target yuzhu tests/slt/m1 tests/slt/m2 tests/slt/m3`: 73 ファイル通過、失敗 0（新しいデータディレクトリで実行）
- `tests/run.sh --target yuzhu --restart`、`--crash`（kill -9 で落として起動し直す）: すべてのシナリオ通過
- `tests/run.sh --target pg`（PostgreSQL 17）: slt m1〜m3 の 73 ファイル通過、`--restart` と `--crash` も通過（`--crash` の yuzhu.only シナリオは pg では飛ばす）
- 分離性テスト（`yuzhu-isolation`）: PG は 10 件すべて通過。yuzhu は 8 件通過、`lost-update` と `write-skew-rr` が失敗（REPEATABLE READ が未対応のため。M5 の対象）

未解決事項:

- REPEATABLE READ / SERIALIZABLE は未対応（0A000）。SAVEPOINT も未実装（0A000）。どちらも M5。
- 定数畳み込みがないため、PG が plan 時に返す 22012（`UPDATE t SET a = 1/0` など）が実行時評価になる。READ ONLY のトランザクションでは 25006 が先に出る。
- `int4[]` の `||`（配列の連結）は未実装で 42883。CREATE TABLE の `int4[]` 列は 0A000（PG は受理。意図的な差）。
- 延期した unlink（排他バリアが取れないとき）は、他セッションの長い文がバリアを持つ間は残り、クラッシュでキューが失われると孤立ファイルが残る。`flush_deferred_unlinks` の並行シナリオの単体テストはない。
- 排他バリアの待ち（DROP のコミット、CREATE を含む ROLLBACK の unlink）は cancel / statement_timeout / terminate が効かない。読み手が途切れない間は unlink が遅れる。
- 1 トランザクションで作成・削除できるテーブルは約 8.7 万個まで（超えると 54000）。単体テストはあるが、8.7 万テーブルの実走はしていない。
- `synchronous_commit` の不正値のヒントの順は PG 17 の記憶に基づく（実機未確認）。
- `kill -9` 試験（層 2）は 20 ラウンド以上の夜間実行をしていない（`YUZHU_KILL9_ROUNDS=6` で確認）。
- 実物の psql での確認は未実施。

## 残課題

- M4（B+Tree、PRIMARY KEY / UNIQUE / SERIAL、JOIN、集約、サブクエリ、EXPLAIN）。設計書は未作成。
- 上の未解決事項。

## 再開手順

1. `QUESTIONS.md` を読む（仮決め事項の一覧）。
2. M4 の設計書（`spec/design/m4.md`）を作り、テスト作成と実装を並列に進める。
3. 各段階で `tests/run.sh --target yuzhu` と `--target pg` の両方を確認する。

## 環境メモ

- Rust 1.96.0、Docker あり、psql はホストに未インストール（必要なら postgres:17 コンテナ内の psql を使う）。
- sqllogictest ランナーは `cargo install sqllogictest-bin --locked --version 0.29.1`（`--locked` は必須）。
- ホストでは別件の langfuse 用 postgres:17 コンテナが動いている。yuzhu のテストで使うものではないので触らない。
