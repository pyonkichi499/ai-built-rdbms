# 引継ぎ資料（2026-10-04 時点）

Docker コンテナ内での作業に移行するため、ユーザーの指示ですべての作業を止めました。この資料と `QUESTIONS.md` を読めば、作業を再開できます（`PROGRESS.md` は古いので、こちらを優先してください）。

## 1. プロジェクトの概要

- **yuzhu**: PostgreSQL のワイヤプロトコルと互換のクライアント/サーバ型 RDB をゼロから作るプロジェクト。名前は『活俠傳』の郁竹が由来。まず Rust で実装し、その後は Go・C/C++・Python 以外の言語でも作り直す。
- **要件定義書**（Claude Docs）: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- **AI 向けのルール**: `CLAUDE.md`（コアは自作、`unsafe` 禁止、PostgreSQL 互換の土台、接続ごとに 1 スレッド、Linux のみ、など）
- **マイルストーン**: M1 メモリ上で動く → M2 永続化 → M3 WAL・リカバリ → M4 インデックス・JOIN・集約 → M5 実用化（複数ライター、Extended Query、認証など）→ M6 以降

## 2. ユーザーの作業スタイル（必ず守る）

- 返答は日本語で書く。
- 確認を取らずに進める。判断が必要な点は推奨案で仮決めし、`QUESTIONS.md` に追記する。
- 時間効率を最優先する。コストは気にせず、ワークフローやサブエージェントで可能な限り並列化する。先のマイルストーンの調査・設計も前倒しで並列に走らせる。
- ヒアリングが必要なときは一問一答で聞き、選択肢には工数感を添える。
- コミットはローカルの `dev` ブランチに、マイルストーンの区切りごとに入れてよい。push はしない。
- コマンドを実行するたびに許可を求めないで済むよう、コンテナ内で作業する方針になった。

## 3. 現在の状態

### git

- すべての成果物を、ローカルの `dev` ブランチにコミット済み（作業途中の部品も含む）。push はしていない。`main` は最初のコミットのまま。
- コンテナでは `dev` ブランチから作業を続けること。

### 完了したもの

| 成果物 | 場所 | 状態 |
|---|---|---|
| 調査レポート | `spec/research/` | M1 向け 4 本（`research-*.md`）、M2 向け 4 本（`m2-*.md`）、M3 向け 4 本（`m3-*.md`）、M4 向け 2 本、M5 向け 3 本、互換ツール 1 本（`pg-compat-tools.md`） |
| M1 基本設計 | `spec/design/m1.md`、`spec/design/m1-changes.md`（契約の変更記録 C-01〜C-16） | 完了 |
| M2 基本設計 | `spec/design/m2.md`（約 2,250 行。12 担当で並列に実装できるよう分担を定義） | 完了（3 観点のレビューを反映済み） |
| 共通 SQL テスト | `tests/slt/m1/`（37 ファイル・952 件）、`tests/run.sh`、`tests/pg.sh`、`tests/README.md` | 本物の PostgreSQL 17 で全件通過 |
| M1 の Rust 実装 | `impl/rust/`（`yuzhu-core`、`yuzhu-server`。約 2.2 万行） | 下記を参照 |
| Docker イメージ | `Dockerfile`、`.dockerignore`、README の「Docker」節 | `docker build` は通り、コンテナ内の psql で `select 1` が返ることを確認済み。ホストから公開ポートへの TCP 接続は通らなかった（原因は未調査） |
| CI | `.github/workflows/ci.yml` | fmt・clippy・test、slt を PostgreSQL に流すジョブ、slt を yuzhu に流すジョブ（`continue-on-error`）、Docker のビルド。CI 上ではまだ一度も動かしていない |

### M1 の実装

- 実装ワークフローは最後まで完走した。
  - 4 担当（パーサ・アナライザ・実行・セッション）が並列に実装。
  - 結合の 1 周目で、共通テスト 37 ファイルがすべて yuzhu で通った。
  - そのあと「正しさ」「堅牢性」「設計」の 3 観点でレビューし、指摘を修正。
  - 最後に最終確認を走らせた。
- **最終確認の結果（レビュー修正の後も全件通るか）は、まだ読んでいない**。再開したらまず次を実行して確かめること。
  - `impl/rust/` で `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
  - `tests/pg.sh start` で PostgreSQL を起動し、`tests/run.sh --target pg` を実行する（期待値の確認）。
  - yuzhu-server を起動し、`tests/run.sh --target yuzhu --port <port>` を実行する。
- 実装担当が報告した既知の制限:
  - 小数リテラル（`1.5`）は numeric 型なので、0A000 を返す（numeric は M4 に前倒しする予定）。
  - 定数畳み込みを再現していないため、空の表に対する `SELECT 1/0 FROM t` が、PostgreSQL ならエラーのところを 0 行で返す。
  - 演算子のエラー位置が、PostgreSQL と違い式の先頭を指す。
  - 拡張クエリを断るときに、トランザクション状態を変えていない（M5 で拡張クエリを実装するときに直す）。
  - 自動生成する CHECK 名の重複検査が、同じテーブルの中だけになっている。
- 設計書から変更した点（ParameterStatus は ReadyForQuery の直前に、値が変わった項目だけ送る。Query の途中の BEGIN は区切りにならない）が実装に反映されているかは、未確認。

### 途中で止めたもの（中途半端なファイルが残っている可能性あり）

1. **M3・M4・M5 の設計ワークフロー**: M3 の設計書（`spec/design/m3.md`）を書いている途中で止めた。ファイルがあっても未完成として扱い、作り直すこと。M4 と M5 の設計書は未着手。
   - 手順: 各マイルストーンについて、調査レポートと前のマイルストーンの設計書をもとに草稿を書く → 3 観点（正しさと前の形式との整合、PostgreSQL 互換、実装しやすさと契約の明確さ）でレビューする → 改訂する。
2. **独立部品のワークフロー**（作る → PostgreSQL と突き合わせて意地悪くレビューする → 直す）: どれも作っている途中で止めた。存在するファイルは未完成として扱い、作り直すこと。
   - `impl/rust/crates/yuzhu-numeric`: PostgreSQL 互換の任意精度数値（M4 に前倒しする予定）。
   - `impl/rust/crates/yuzhu-datetime`: date・timestamp・timestamptz・interval。タイムゾーンは tzdata を自前で読む。
   - `tests/tools/difftest`: PostgreSQL と yuzhu にランダムなクエリを流して差分を見つけるツール。
   - `tests/tools/isolation`: PostgreSQL の isolation spec 形式を読む、同時実行テストのランナー。
   - **注意**: numeric と datetime は `impl/rust/Cargo.toml` の `members` に追加されているかもしれない。中途半端な状態だとワークスペース全体のビルドが壊れるので、ビルドが通らなければ、まず `members` から外すこと。

### 動いているもの

- バックグラウンドのワークフローとエージェントは、すべて停止した。
- Docker では、作業中のエージェントが PostgreSQL のコンテナ（`yuzhu-*` という名前）を立てていた可能性がある。`docker ps -a` で確認し、残っていれば削除すること。**`langfuse-*` のコンテナは別件なので触らない**。

## 4. 再開手順（推奨順）

1. リポジトリをコンテナに持ち込み（マウントまたは clone）、`dev` ブランチに切り替える。
2. コンテナに必要なもの: Rust 1.96（`rust-toolchain.toml` で stable を指定）、`cargo install sqllogictest-bin --locked --version 0.29.1`（`--locked` は必須）、テストの期待値を確かめるための PostgreSQL 17。コンテナ内では Docker が使えない可能性が高いので、`tests/pg.sh` の代わりに PostgreSQL 17 を直接入れるか、別のコンテナを立てて接続先を指定すること（`tests/run.sh --target pg --host ... --port ...`）。initdb は `--locale=C --encoding=UTF8` で行い、trust 認証にする。
3. 上の「M1 の実装」の確認コマンドを実行し、M1 が全件通っていることを確かめる。通っていなければ直す。
4. M1 の完了としてコミットする（`dev` ブランチ）。
5. 次の 3 つを並列に始める。
   - **M2 の実装**: `spec/design/m2.md` の分担表（12 担当）に従い、ワークフローで並列に実装する。M1 と同じく「実装 → slt を回して担当ごとに修正 → レビュー → 最終確認」の流れにする。
   - **M3・M4・M5 の設計のやり直し**: 途中で止めたワークフローを、同じ手順でもう一度走らせる。
   - **独立部品 4 つの作り直し**: 上の 4 つ。
6. `QUESTIONS.md` に、ユーザーの回答待ちの仮決めが並んでいる（Q-001〜Q-014 と、M2 の確認事項の抜粋）。ユーザーから回答があれば反映する。

## 5. 主なファイル

| ファイル | 内容 |
|---|---|
| `HANDOFF.md` | この資料 |
| `QUESTIONS.md` | 仮決めした事項（ユーザーの回答待ち） |
| `CLAUDE.md` | AI 向けのルールとマイルストーン |
| `spec/design/m1.md`、`m1-changes.md`、`m2.md` | 基本設計（並列実装の契約） |
| `spec/research/*.md` | 調査レポート（各設計の根拠） |
| `tests/README.md` | 共通テストの実行方法と書き方のルール |
| `impl/rust/` | Rust 実装（Cargo ワークスペース） |
