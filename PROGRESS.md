# 進捗と再開手順

最終更新: 2026-10-04（M1 完了。M2 はテストの先行作成中）

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

## M2 の状況

- 設計書 `spec/design/m2.md` は作成済み。実装は未着手。
- テスト `tests/slt/m2/` は `dml/update_basic.slt` と `dml/update_expr.slt` の 2 ファイルのみ。`catalog`・`ddl`・`psql`・`txn`・`types` は空。
- M2 のテストはまだ yuzhu に対して実行していない（UPDATE / DELETE が未実装のため）。

## 残課題

- M2 のテストの残りを作成し、PostgreSQL 17 で通ることを確認する。
- M2 の実装（8KB ページ、バッファプール、ヒープ、リレーションごとのファイル、カタログのテーブル化、UPDATE / DELETE、initdb）。
- M3 以降は設計書 `spec/design/m3.md` と調査 `spec/research/` を参照。

## 再開手順

1. `QUESTIONS.md` を読む（仮決め事項の一覧）。
2. `spec/design/m2.md` に沿って M2 のテスト作成と実装を並列に進める。
3. 各段階で `tests/run.sh --target yuzhu` と `--target pg` の両方を確認する。

## 環境メモ

- Rust 1.96.0、Docker あり、psql はホストに未インストール（必要なら postgres:17 コンテナ内の psql を使う）。
- sqllogictest ランナーは `cargo install sqllogictest-bin --locked --version 0.29.1`（`--locked` は必須）。
- ホストでは別件の langfuse 用 postgres:17 コンテナが動いている。yuzhu のテストで使うものではないので触らない。
