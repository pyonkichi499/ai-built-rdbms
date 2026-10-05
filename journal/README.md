# journal/ 入口

yuzhu（PostgreSQL ワイヤプロトコル互換 RDBMS を AI エージェント群でゼロから作るプロジェクト）の作業日誌。
作成日: 2026-10-04。時刻はすべて **UTC**（JST は括弧で補足するだけ。git の +0900 と +0000 の混在による読み違いを防ぐため）。

## 1. 目的と使い方

- 目的: 後から「**何をいつどう判断し、何に時間がかかり、何がうまくいき/いかなかったか**」を振り返ること。
- 各ファイルは 2 層構造。
  - **自動生成ブロック**: `<!-- AUTO:名前 BEGIN -->` と `<!-- AUTO:名前 END -->` で囲まれた部分。`tools/journal-*.sh` が書き換える。
  - **手書きブロック**: それ以外。スクリプトは触らない。
- 読み方のおすすめ: まず `snapshots.md`（今どこか）→ `timeline.md`（経緯）→ `decisions.md`（判断）→ `workflows.md`（時間）→ `setbacks.md`（失敗）→ `retrospective.md`（解釈）。数値の推移は `metrics.md`。

## 2. 索引

| ファイル | 1 行要旨 |
|---|---|
| `README.md` | この入口。索引、更新手順、書き方の規則、用語集、spec の読書ガイド |
| `timeline.md` | UTC 統一の年表（コミット、Workflow、ユーザー介入、成果物） |
| `decisions.md` | 設計判断（D）と仮決め（Q、M2-Q）の台帳。ディスク形式★、スコープ変更、規約への例外 |
| `workflows.md` | Workflow 台帳（計画と実績、クリティカルパス、担当別コスト、初回通過率） |
| `setbacks.md` | 手戻り、誤報告、衝突、ドキュメントの鮮度ずれ、外れた前提 |
| `metrics.md` | 指標の推移表（コード行数、テスト件数、slt 通過率など。Workflow 区切りで 1 行追記） |
| `retrospective.md` | KPT と運用知見（プロンプト、分割粒度、テスト先行の効果） |
| `snapshots.md` | 再開用スナップショットと M2 進捗マップ |

種別と最終更新（`tools/journal-readme.sh` が生成。最終更新はファイルの mtime で、UTC）:

<!-- AUTO:index BEGIN -->
| ファイル | 種別 | 最終更新 |
|---|---|---|
| `journal/README.md` | 手書き中心 | 2026-10-04 15:33 UTC |
| `journal/timeline.md` | 自動 + 手書き | 2026-10-05 07:32 UTC |
| `journal/decisions.md` | 自動 + 手書き | 2026-10-05 07:32 UTC |
| `journal/workflows.md` | 自動 + 手書き | 2026-10-05 07:32 UTC |
| `journal/setbacks.md` | 自動 + 手書き | 2026-10-05 07:32 UTC |
| `journal/metrics.md` | 自動 + 手書き | 2026-10-04 15:33 UTC |
| `journal/retrospective.md` | 手書き中心 | 2026-10-04 15:20 UTC |
| `journal/snapshots.md` | 手書き中心 | 2026-10-05 07:32 UTC |
<!-- AUTO:index END -->

## 3. 更新手順

1. Workflow が 1 本終わったら `tools/journal-all.sh` を実行する（`--dry-run` なら差分を標準出力に出すだけで書き換えない）。
2. `git diff --stat journal/` で変更を確認する。
3. 各ファイルの**手書き欄**（所見、KPT、手戻りの原因と対処、スナップショットの「次にやること」、★の追認状況）を足す。
4. コミットは人間が行うか、別指示があるときだけ。cron や hook は仕掛けない。


M2 実装 Workflow（wf_0cc0a7be）完了時の手順（2026-10-04 検証時に確定）:

1. `tools/journal-metrics.sh --label "M2 実装完了" --run-tests` で slt 通過率と restart を計測する（`sandbox/pg.sh start` が先）。`tools/journal-all.sh` は snapshots と README 索引まで一括更新する（`--run-tests` も渡せる）。
2. `tools/worklog.sh <wf_0cc0a7be のトランスクリプトディレクトリ> "M2 実装"` の出力を確認する（WORKLOG.md への追記は別担当）。
3. 手書き欄を足す: workflows.md（計画と実績の差、Parallel/Assemble/Integrate/Review の実測）、setbacks.md（S-番号を継続。AUTO:reports の「履歴から外れている」ハッシュを確認）、decisions.md（M2 実装中の仮決め、★の追認）、retrospective.md（KPT）、snapshots.md（新スナップショットを節 1 の先頭に積む）、timeline.md 第 6 節の追記欄。
4. 注意: ブラケット `HEAD@{n}` のような番号付き参照は時間でずれるので書かない。metrics の `--label` を省くと「(ラベルなし)」行が増える。
注意: **AUTO ブロックは手で編集しない**（次回の実行で上書きされる）。直したいときは `tools/journal-*.sh` を直す。
この README の AUTO ブロック（索引、spec の行数）だけは `tools/journal-readme.sh` で単独更新できる。

## 4. 書き方の規則

- 手書きの追記は**日付見出し**（`## 2026-10-04` など）で積む。過去の記述は上書きしない。`PROGRESS.md` が最新 1 枚で上書きされる弱点を補うため。
- 判断には **Q-番号 / D-番号 / コミット hash / wf ID** を必ず付ける（例: `M2-Q2`、`m2.md D2`、`88d1bc0`、`wf_fabaf03d`）。
- **推測と事実を分ける**。推測には「推測:」を付け、不明なことは「不明」と書く。数字には出所（コマンド、ファイル）を添える。
- 時刻は UTC。JST を書くときは括弧書き。

## 5. 用語集

| 用語 | 説明 |
|---|---|
| M1〜M6 | マイルストーン（`CLAUDE.md`）。M1 繋がって動く（メモリ上）／M2 永続化（8KB ページ、バッファプール、ヒープ、initdb）／M3 トランザクションと耐久性（WAL、チェックポイント）／M4 インデックスとクエリ（B+Tree、JOIN、EXPLAIN）／M5 実用化（複数ライター、VACUUM、Extended Query、SCRAM）／M6 以降 GRANT、TLS など。なお M2 設計で MVCC・コミットログ・単一ライターを M2 に前倒しした（`m2.md` D1〜D3、`m3.md` D1） |
| slt / sqllogictest | SQL の入力と期待結果を書くテスト形式。`tests/slt/` に置き、`tests/run.sh --target yuzhu\|pg` で yuzhu と本物の PostgreSQL 17 の両方に流す（PostgreSQL が正解の基準） |
| SQLSTATE | 5 文字のエラーコード。yuzhu はすべてのエラーに付与する |
| typmod | 型修飾子（`varchar(10)` の 10 など）。型は OID + typmod で表す |
| OID | PostgreSQL のオブジェクト識別子。型、テーブルなどの ID |
| MVCC | 多版型同時実行制御。タプルの xmin/xmax とコミットログで可視性を決める |
| WAL | Write-Ahead Log。先行書き込みログ。yuzhu は REDO のみ（M3 で導入） |
| full page write | チェックポイント後の最初のページ更新で、ページ全体を WAL に書いて torn write に備える仕組み（M3） |
| チェックサム | ページの破損検出用の CRC。M2 から常に有効（`m2.md` D7、★M2-Q5） |
| Halloween 問題 | 自分が書いた新しい版を同じ文のスキャンが再び拾う問題。コマンド ID で防ぐ（`m2.md` §6 付近、テスト `tests/slt/m2/dml/update_halloween.slt`） |
| claude-sandbox | 承認なしで Claude Code を動かす Docker コンテナ（イメージ `yuzhu-sandbox`。`sandbox/README.md`）。`HOME` が `/home/sandbox`。PostgreSQL 17 は `sandbox/pg.sh start` で 127.0.0.1:55432 |
| Workflow | 複数のサブエージェントを並列・段階実行するスクリプト。トランスクリプトは `~/.claude/projects/.../subagents/workflows/wf_*/` |
| stabilize | M1 完成 Workflow の最初の担当。コンパイルを通す（`WORKLOG.md`） |
| fix:* | M1 完成 Workflow で並列に動いた、slt 10 領域ごとの修正担当（fix:functions、fix:types、fix:txn、fix:insert、fix:constraints、fix:errors、fix:session、fix:expressions、fix:select、fix:ddl） |
| K-m2-tests | M2 のテスト（slt と再起動テスト）を先行して作った担当。`m2.md` §8 の担当 K に対応 |
| 担当 A〜J（と K） | `m2.md` §8 の実装分担。A 基盤、B I/O 層、C バッファ、D ヒープ、E トランザクション、F カタログ、G エンジン、H1 SQL（パーサ、アナライザ）、H2 実行（プランナ、エグゼキュータ）、I セッション、J サーバ、K テスト。各担当は自分の範囲のファイルだけ編集する |
| Q-nnn | `QUESTIONS.md` の仮決め。Q-001〜002 作業の進め方、Q-003〜009 M1、Q-010〜014 先のマイルストーン |
| M2-Qn / M3-Qn | `m2.md` §9、`m3.md` §10 の確認事項。M2 は 21 件。`QUESTIONS.md` には重要なものだけ転記 |
| D-n | 設計書の「調査間の食い違いの決定」表の行番号。**文書ごとに D1 から振り直される**ので `m2.md D2` のように文書名を添える |
| C-nn | `spec/design/m1-changes.md` の、M1 契約からの実装時変更（C-01 から） |
| ★ | ディスク形式に関わる仮決め。実装後に変えるとやり直しが大きい。`QUESTIONS.md` で ★ が付くのは M2-Q2（タプルヘッダ 35 バイト）と M2-Q5（チェックサム）の 2 件（`grep -n '★' QUESTIONS.md`。32 行目は説明文）。`m2.md` §9 は M2-Q15 もディスク形式に関わるとするが、`QUESTIONS.md` には転記されていない。M3-Q2、Q3 の ★ は `m3.md` §10 側 |

## 6. spec 読書ガイド

`spec/README.md` はプレースホルダのまま（内容は「現在はプレースホルダです」のみ）。実質の仕様は `spec/design/` と `spec/research/`。

### 6.1 行数とサイズ（`wc` による自動生成）

<!-- AUTO:speclines BEGIN -->
| ファイル | 行数 | バイト |
|---|---:|---:|
| `spec/design/m1-changes.md` | 36 | 6026 |
| `spec/design/m1.md` | 488 | 37073 |
| `spec/design/m2-changes.md` | 11 | 1112 |
| `spec/design/m2.md` | 2250 | 218394 |
| `spec/design/m3.md` | 1776 | 150876 |
| `spec/research/m2-buffer-io.md` | 794 | 67761 |
| `spec/research/m2-catalog.md` | 446 | 48734 |
| `spec/research/m2-dml-exec.md` | 471 | 48927 |
| `spec/research/m2-page-heap.md` | 461 | 42588 |
| `spec/research/m3-mvcc.md` | 538 | 45723 |
| `spec/research/m3-recovery.md` | 570 | 61136 |
| `spec/research/m3-tx-semantics.md` | 491 | 62015 |
| `spec/research/m3-wal.md` | 474 | 46107 |
| `spec/research/m4-btree.md` | 480 | 62279 |
| `spec/research/m4-query.md` | 776 | 78889 |
| `spec/research/m5-concurrency.md` | 750 | 89406 |
| `spec/research/m5-protocol-auth.md` | 661 | 68504 |
| `spec/research/m5-types-fk.md` | 740 | 78250 |
| `spec/research/pg-compat-tools.md` | 552 | 57609 |
| `spec/research/research-pg-protocol.md` | 399 | 44378 |
| `spec/research/research-pg-types.md` | 474 | 41380 |
| `spec/research/research-rust-db-arch.md` | 516 | 54985 |
| `spec/research/research-slt.md` | 368 | 34200 |
<!-- AUTO:speclines END -->

分量の目安: `m2.md` は約 218KB（2250 行）、`m3.md` は約 149KB（1775 行）。**全部は通読せず、先に目次（`^## ` 見出し）と各冒頭の D 表（「調査間の食い違いと、この設計での決定」）を読む**。
`grep -n '^## ' spec/design/m2.md` で節の一覧が出る。M2 は §0 決定表、§3 ディスク形式、§4 契約（型、トレイト）、§5 処理の流れ、§6 モジュール仕様、§7 テスト、§8 担当分け、§9 確認事項、§10 レビュー対応。

### 6.2 design（設計書）

| ファイル | 要旨 | 依存 |
|---|---|---|
| `m1.md` | M1 基本設計。psql と主要ドライバが Simple Query で繋がり、メモリ上で CREATE TABLE / INSERT / SELECT が動くまでの契約 | 調査 `research-*.md` 4 本 |
| `m1-changes.md` | M1 実装中の契約変更の記録（C-01〜）。**`m1.md` を改訂する**ので、m1.md の後に読む | m1.md |
| `m2.md` | M2 基本設計。永続化（8KB ページ、ヒープ、バッファプール、カタログのテーブル化、initdb）。MVCC・単一ライターを前倒し | m1.md、m1-changes.md、`m2-*.md` と `m3-mvcc/wal/recovery.md` |
| `m3.md` | M3 基本設計。WAL、チェックポイント、リカバリ、クラッシュ試験。M2 からの差分として書かれている | m1.md、m1-changes.md、m2.md、`m3-*.md`、`m4-btree.md` §5 |

### 6.3 research（調査 18 本）

| ファイル | 要旨 |
|---|---|
| `research-pg-protocol.md` | M1 調査。Simple Query で psql とドライバを繋ぐためのワイヤプロトコル |
| `research-pg-types.md` | M1 調査。型 OID、キャスト、演算子解決、SQLSTATE |
| `research-rust-db-arch.md` | 既存 DB のコード構造と yuzhu-core のモジュール設計案、safe Rust のバッファプール |
| `research-slt.md` | 共通テストスイート。sqllogictest-bin 0.29.1 の採用、運用 |
| `m2-page-heap.md` | ページとヒープタプルのディスク形式、TOAST、FSM、チェックサム |
| `m2-buffer-io.md` | 障害注入つき I/O 抽象化、ストレージマネージャ、バッファプール |
| `m2-catalog.md` | システムカタログのテーブル化、initdb、データディレクトリ、キャッシュ |
| `m2-dml-exec.md` | UPDATE / DELETE、ROLLBACK の方式、executor と storage のインタフェース変更 |
| `m3-mvcc.md` | MVCC の可視性、トランザクション管理 |
| `m3-wal.md` | WAL の形式とライター |
| `m3-recovery.md` | チェックポイント、制御ファイル、リカバリ、クラッシュ試験 |
| `m3-tx-semantics.md` | ユーザーから見えるトランザクション意味論（PostgreSQL 17.11 の実機確認つき） |
| `m4-btree.md` | B+Tree、PRIMARY KEY / UNIQUE、シーケンスと SERIAL |
| `m4-query.md` | JOIN、集約、サブクエリ、集合演算、CTE、ルールベース最適化、EXPLAIN |
| `m5-concurrency.md` | 複数ライター、行ロック、EvalPlanQual、Repeatable Read、デッドロック、VACUUM |
| `m5-protocol-auth.md` | Extended Query、SCRAM-SHA-256、ロール、CREATE / DROP DATABASE |
| `m5-types-fk.md` | numeric、日付時刻、char(n)、bytea、uuid、配列、FOREIGN KEY |
| `pg-compat-tools.md` | psql メタコマンド、pgbench、pg_dump、ORM、GUI ツールの互換性（M5/M6） |

調査資料の注記の凡例は文書ごとに異なる（【確認】【記憶】【提案】、[実機] [ソース] [未検証] など）。「未検証」「未確認」「記憶」と書かれた記述は、実装前に確かめる前提のもの。

### 6.4 推奨する読み順

1. `CLAUDE.md`、`QUESTIONS.md`、`PROGRESS.md`（現状と仮決め）
2. `spec/design/m1.md` → `m1-changes.md`（改訂後の M1 契約）
3. `spec/design/m2.md` の §0 の D 表 → §1 範囲 → §3 ディスク形式 → 必要な節
4. `spec/design/m3.md` の §0 の D 表 → §1 範囲 → 必要な節
5. 根拠を確かめたいときだけ research。M2 なら `m2-page-heap.md`、`m2-buffer-io.md`、`m2-catalog.md`、`m2-dml-exec.md`。M3 なら `m3-*.md`。M4 以降は設計書が未作成なので `m4-*.md`、`m5-*.md`、`pg-compat-tools.md` を直接読む
6. 最初の 4 本（`research-*.md`）は M1 の背景。必要になったら読む

## 7. 他の情報源

- `WORKLOG.md`: Workflow の担当別所要時間（`tools/worklog.sh` で生成）
- Workflow のトランスクリプト: `/home/sandbox/.claude/projects/-home-hiroshi-work-private-github-ai-built-rdbms/*/subagents/workflows/wf_*/`（`journal.jsonl` と `agent-*.jsonl`。`jq` で読む）
- M1 完成 Workflow の ID は `wf_fabaf03d`。M2 実装 Workflow は進行中（ID は `timeline.md` / `workflows.md` を参照）
