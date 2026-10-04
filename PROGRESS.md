# 進捗と再開手順

最終更新: 2026-10-04（M1 実装ワークフロー・M2 設計ワークフロー・M3 調査ワークフローを並列で実行中）

## 完了したこと

- 要件定義書（Claude Docs）: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- 調査レポート 4 本: `spec/research/`（プロトコル、型システム、sqllogictest、Rust 製 DB の構成）
- M1 基本設計書: `spec/design/m1.md`（並列実装のための契約。分担は第 7 節）
- リポジトリの雛形: Cargo ワークスペース（`impl/rust/`、`yuzhu-core` と `yuzhu-server`）、CI（fmt・clippy・test）、`README.md`、`CLAUDE.md`、`.gitignore`
- 仮決めした事項の一覧: `QUESTIONS.md`

すべて未コミットです（`QUESTIONS.md` の Q-001 を参照）。

## 進行中

- 基盤（`yuzhu-core` の共通型。契約の変更は `spec/design/m1-changes.md`）・テスト（`tests/slt/m1/` の 37 ファイル。PostgreSQL 17 で全件通過）・サーバ（`yuzhu-server`）は完了。
- ワークフロー `m1-implement`: パーサ・アナライザ・実行・セッションを並列実装 → slt を回して担当ごとに修正（最大 8 周）→ 3 観点のレビューと修正 → 最終確認。
- ワークフロー `m2-design`: M2 の調査 4 本 → `spec/design/m2.md` → レビューと修正。
- ワークフロー `m3-research`: M3 の調査 4 本（`spec/research/m3-*.md`）。

## 再開手順

1. `QUESTIONS.md` の回答を反映する（特に Q-001 のコミット方針）。
2. 上の 3 つを並列で再開する（基盤・テスト・サーバ）。
3. 基盤が終わったら、パーサ・アナライザ・実行・セッションの 4 担当を並列で走らせる（設計書の第 7 節）。
4. 結合して `tests/run.sh --target yuzhu` を通す → M1 完了。

## 環境メモ

- Rust 1.96.0、Docker あり、psql はホストに未インストール（必要なら postgres:17 コンテナ内の psql を使う）。
- sqllogictest ランナーは `cargo install sqllogictest-bin --locked --version 0.29.1`（`--locked` は必須）。
- ホストでは別件の langfuse 用 postgres:17 コンテナが動いている。yuzhu のテストで使うものではないので触らない。
