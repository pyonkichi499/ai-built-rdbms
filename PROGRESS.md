# 進捗と再開手順

最終更新: 2026-10-04（M1・M2 完了。次は M3）

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
- `tests/run.sh --target yuzhu` を引数なしで流すと m3 の slt も対象になり、未実装機能で 7 ファイルが失敗する（M3 の対象）。
- 実物の psql 17 で `\l` を流す確認は未実施（同等の SELECT を統合テストで確認）。

## 残課題

- M3（コミットログ・REDO のみの WAL・チェックポイント・full page write・クラッシュリカバリ）。設計書 `spec/design/m3.md` を参照。
- 上の未解決事項。

## 再開手順

1. `QUESTIONS.md` を読む（仮決め事項の一覧）。
2. `spec/design/m3.md` に沿って M3 のテスト作成と実装を並列に進める。
3. 各段階で `tests/run.sh --target yuzhu` と `--target pg` の両方を確認する。

## 環境メモ

- Rust 1.96.0、Docker あり、psql はホストに未インストール（必要なら postgres:17 コンテナ内の psql を使う）。
- sqllogictest ランナーは `cargo install sqllogictest-bin --locked --version 0.29.1`（`--locked` は必須）。
- ホストでは別件の langfuse 用 postgres:17 コンテナが動いている。yuzhu のテストで使うものではないので触らない。
