# 進捗と再開手順

最終更新: 2026-10-07（M1〜M4 完了。次は M5）

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

## M4 の状況（完了。2026-10-07 JST）

到達点: B+Tree インデックス（`BTREE_PAGES` などの WAL、一括構築、検査器）、PRIMARY KEY / UNIQUE / SERIAL / IDENTITY とシーケンス、
JOIN（nested loop / hash）、集約、サブクエリ、CTE、集合演算、ルールベース最適化（定数畳み込みを含む）、EXPLAIN、
COPY、ALTER TABLE ADD PRIMARY KEY / UNIQUE、TRUNCATE、numeric・日時・正規表現などの型と関数の追加、psql の `\dt` `\di` `\ds` `\l` と `\d シーケンス`、
pgbench / psql / COPY のクライアント互換。設計は `spec/design/m4/`（読む順は README.md）。実装の規模は Rust 約 15.9 万行（`yuzhu-core` 約 14.3 万行）。

完了判定（`tests/done-check.sh` の条件 1〜9）はすべて PASS（2026-10-07 JST）:

| 条件 | 内容 | 結果 |
|---|---|---|
| 1 | `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace`（yuzhu-core lib 1789 件、crash_sim 88 件ほか） | PASS |
| 2 | `tests/run.sh` の `--target pg` / `--target yuzhu`（slt m1〜m4 の 252 ファイル）と `slttools lint` | PASS |
| 3 | `--restart` / `--crash`（pg・yuzhu） | PASS |
| 4 | isolation の全 spec（pg・yuzhu） | PASS |
| 5 | `tests/compat`（psql・COPY・pgbench の整合性とクラッシュ） | PASS |
| 6 | 差分ファジング（固定シード 1〜32 × 200 ケース、長時間 1001〜1004 × 10,000 ケース。除外の針は `tests/tools/difffuzz/known-excludes.txt`） | PASS |
| 7 | クラッシュ試験 層 1（変異テストを含む） | PASS |
| 8 | EXPLAIN の書式と plan_variants | PASS |
| 9 | QUESTIONS.md / PROGRESS.md の運用 | PASS |

注: 条件 2 は、最後の修正（slt の後始末と名前、`ALTER TABLE ADD UNIQUE` のエラー文言）のあとで単独に再実行して通した。条件 1・3〜9 は同じコードでの全体実行（2 回目）の結果。

完了時の最終確認で直したこと:

- `ALTER TABLE ... ADD UNIQUE (存在しない列)` のメッセージを `column "zz" named in key does not exist` にした（PG 17 で確認。`ADD PRIMARY KEY` は PG も `of relation` の文言）。
- `NULLIF(NULL, 式)` は右辺を評価せずに NULL へ畳む（PG と同じ。右辺が 22003 になる行で誤ってエラーにならない）。slt `types/nullif_null_left_fold.slt` を追加。
- slt の後始末（`seq/volatile_order`・`seq/restart_rollback`・`types/review_sql_compat` の DROP と接頭辞）、`join/paren_subquery_leaf` のエラー SQLSTATE、`tests/compat/copy/expected/errors_order.out` の余分な末尾の空行を直した。
- 差分ファジングの除外の針に、M5 以降の機能（`DO`、`CREATE TABLE ... AS`、REPEATABLE READ / SERIALIZABLE の `BEGIN`）と、float の -0 / 0 の同順位の非決定性を追加した。

未解決事項・制限（事実のみ。QUESTIONS.md の「M4 の完了時に追加した確認事項」も参照）:

- 同じキーの UPDATE を繰り返すとユニーク検査と等値スキャンが 2 乗で遅くなる（インデックスの項目を削除しない。設計 06 の D6-7 / M4-Q62）。簡易削除（+2〜3 日）を M4 に入れるかの判断が残っている。
- `shared_buffers` の下限は 512kB（64 フレーム）。64 フレームでの CREATE INDEX と大きなキーの INSERT は実サーバでは未確認。
- `check_structure`（B+Tree の検査器）は書き手が止まっているときだけ使える。
- 差分ファジングの除外: RETURNING、DO、CTAS、REPEATABLE READ / SERIALIZABLE、SAVEPOINT などは未実装（0A000）。`tests/slt/m4/KNOWN-DIFFS.md` に既知の差分一覧がある。
- 深い入れ子（500 腕の UNION、1000 項の OR など）の自動回帰テストはなく、手動で確認しただけ。パーサの深さ上限は 5000（超えると 54001）。
- 実物の psql 17 での `\d tbl`（テーブルの詳細）は M4 では動かない（M4-Q119）。
- 承認が要る ★12 件（ディスク形式）は QUESTIONS.md のとおり仮決めのまま。
- 共有の PostgreSQL（55432）は他の作業で込み合うと checkpointer 待ちで止まることがあった（完了判定は専用のインスタンスで実行した）。

## 残課題

- M5（複数ライター、VACUUM、Repeatable Read、Extended Query、型の追加、FOREIGN KEY、SCRAM、CREATE DATABASE、RETURNING、簡易削除など）。設計書は `spec/design/m5/` に途中まである。
- 上の未解決事項。

## 再開手順

1. `QUESTIONS.md` を読む（M4 の完了時に追加した確認事項は末尾）。
2. M5 の設計書（`spec/design/m5/`）を読み、テスト作成と実装を並列に進める。
3. 各段階で `tests/done-check.sh`（M4 の完了判定。M5 用に拡張する）、`tests/run.sh --target yuzhu` と `--target pg` の両方を確認する。

## 環境メモ

- Rust 1.96.0。ホストでは Docker、claude-sandbox のコンテナ内では `sandbox/pg.sh` で PostgreSQL 17 を使う。
- sqllogictest ランナーは `cargo install sqllogictest-bin --locked --version 0.29.1`（`--locked` は必須）。
- 日誌（`journal/`、`WORKLOG.md`）の時刻は JST（UTC+9）。`tools/journal-all.sh` が更新する。
