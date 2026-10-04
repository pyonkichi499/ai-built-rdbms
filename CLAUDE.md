# CLAUDE.md

yuzhu（PostgreSQL ワイヤプロトコル互換 RDBMS をゼロから実装）に取り組む AI 向けのガイドです。

- 要件定義（必ず参照）: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- **ユーザーへの返答は日本語で行うこと。**

## 構成

- `spec/` 言語非依存の仕様 / `tests/` 共有 sqllogictest スイート / `impl/rust/` Rust 実装（Cargo ワークスペース）
- Rust の確認コマンド（`impl/rust/` で実行。CI と同じ）:
  `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`

## 重要ルール

- **コア部品は手書き**: SQL パーサ（再帰下降）、ストレージ、WAL、トランザクション、ワイヤプロトコル。
  外部クレートは周辺用途のみ（暗号、テスト、CLI 引数、ロギング、エラー型）。依存追加は慎重に。
- **`#![forbid(unsafe_code)]`** を全クレートで維持する（ワークスペース lint でも forbid）。
- **初日から PostgreSQL 互換の土台**を守る:
  - 型は OID + typmod で表現する
  - カタログは SQL で問い合わせ可能なテーブルとして持つ
  - すべてのエラーに SQLSTATE を付与する
  - 可視性は PostgreSQL の MVCC セマンティクスに従う
- **接続ごとに 1 スレッド・同期 I/O**（async ランタイムは使わない）。
- **Linux のみ**対応。
- **ファイル I/O は抽象化レイヤ経由**で行い、障害注入（fault injection）できるようにする。
- `tests/` のテストは**本物の PostgreSQL に対しても実行できる**こと（PostgreSQL が正解の基準）。
- 設計は `spec/design/` を参照。判断に迷う点は推奨案で進め、`QUESTIONS.md` に記録する。

## マイルストーン（詳細は要件定義を参照）

- M1: 繋がって動く（データはメモリ上）。プロトコル（Simple Query・trust）、手書きパーサ、基本型、CREATE TABLE / INSERT / 単一テーブル SELECT、NOT NULL・DEFAULT・CHECK、カタログスタブ、共通テストランナー
- M2: 永続化。8KB ページ、バッファプール、ヒープ、リレーションごとのファイル、カタログのテーブル化、UPDATE / DELETE、initdb
- M3: トランザクションと耐久性。MVCC（単一ライター + 複数リーダー、Read Committed）、コミットログ、REDO のみの WAL、チェックポイント、full page write
- M4: インデックスとクエリ。B+Tree、PRIMARY KEY / UNIQUE / SERIAL、JOIN、集約、サブクエリ、ルールベース最適化、EXPLAIN
- M5: 実用化。複数ライター、VACUUM、Repeatable Read、Extended Query、型の追加、FOREIGN KEY、SCRAM、CREATE DATABASE
- M6 以降: GRANT/REVOKE、TLS、グループコミット、コストベース最適化、pg_catalog の拡充
