# Workflow 台帳

Workflow ごとの狙い、計画と実績、時間、コスト、成否を横並びにして、並列設計と見積もりの根拠にする。

- 時刻はすべて UTC（JST は +9 時間）。日付は断りが無ければ 2026-10-04。
- `<!-- AUTO:名前 BEGIN/END -->` の間は `tools/journal-workflows.sh`（`tools/journal-all.sh` 経由）が書き換える。それ以外は手書きで、日付見出しで積み、過去の記述は上書きしない。
- 情報源は Workflow のトランスクリプト（`/home/sandbox/.claude/projects/-home-hiroshi-work-private-github-ai-built-rdbms/<session>/subagents/workflows/wf_*/` の `journal.jsonl`、`agent-*.jsonl`、`agent-*.meta.json`）と、`.../<session>/workflows/scripts/*-wf_*.js` の `meta`。WORKLOG.md / `tools/worklog.sh` の担当別所要時間とは同じ元データで、一致を確認した（例: K-m2-tests 13m02s）。
- 数字の出所の注意: `journal.jsonl` に時刻は無い。開始・終了・所要は各 `agent-*.jsonl` の最初と最後の `timestamp`（秒単位）から出している。コストはトークン数のみで、金額は不明（ログに単価が無い）。

## 1. 台帳

<!-- AUTO:ledger BEGIN -->
> 自動生成(tools/journal-workflows.sh)。時刻はすべて UTC。出所: `/home/sandbox/.claude/projects/-home-hiroshi-work-private-github-ai-built-rdbms/*/subagents/workflows/wf_*/` の journal.jsonl と agent-*.jsonl、`workflows/scripts/*.js` の meta.phases。
> 最新ログ時刻: 2026-10-05 07:32:06 UTC。壁時計 = 担当ファイルの最小 timestamp から最大 timestamp まで(journal.jsonl 自体は時刻を持たない)。進行中の Workflow は最新ログ時刻までの暫定値。
> 最長/壁時計 = 最長担当の所要 ÷ 壁時計。並列効率 = 全担当の所要の合計 ÷ 壁時計(1.0 なら実質直列、大きいほど並列が効いている)。

### Workflow 一覧

| Workflow | 名前 | 状態 | 開始 (UTC) | 終了 (UTC) | 壁時計 | 担当数 (結果あり) | 最長担当 | 最長/壁時計 | 合計稼働 | 並列効率 |
|---|---|---|---|---|---|---|---|---|---|---|
| wf_fabaf03d-9d6 | m1-finish-and-m2-tests | 完了 | 2026-10-04 14:35:15 | 2026-10-04 14:49:17 | 14m02s | 13 (13) | K-m2-tests (13m02s) | 92.9% | 18m04s | ×1.29 |
| wf_0cc0a7be-171 | m2-implement | 進行中 | 2026-10-04 14:50:14 | 2026-10-04 21:57:14 (暫定) | 7h07m | 21 (20) | I-session (4h38m) | 65.3% | 10h08m | ×1.43 |
| wf_dc96a8cc-646 | journal-design-and-write | 完了 | 2026-10-04 14:59:52 | 2026-10-04 15:33:41 | 33m49s | 21 (21) | write:tools/journal-workflows.sh (19m45s) | 58.4% | 3h17m | ×5.83 |
| wf_4d730f24-348 | design-doc | 進行中 | 2026-10-04 20:53:24 | 2026-10-05 07:32:06 (暫定) | 10h38m | 16 (14) | outline (8h02m) | 75.5% | 20h28m | ×1.92 |
| wf_32cf6626-a0c | design-doc | 進行中 | 2026-10-04 20:53:36 | 2026-10-05 07:05:47 (暫定) | 10h12m | 16 (15) | outline (7h55m) | 77.7% | 16h49m | ×1.65 |
| wf_5e84e5ec-172 | m3-shared-tests | 完了 | 2026-10-04 20:53:49 | 2026-10-04 21:48:55 | 55m06s | 5 (5) | K:crash (30m13s) | 54.8% | 1h36m | ×1.75 |
| wf_b10ff45b-956 | m3-implement | 進行中 | 2026-10-04 21:59:20 | 2026-10-05 07:32:04 (暫定) | 9h32m | 30 (29) | A-foundation (6h56m) | 72.7% | 12h42m | ×1.33 |

### 計画(meta.phases)と実績

#### wf_fabaf03d-9d6 m1-finish-and-m2-tests (完了)

- 計画スクリプト: `<session>/workflows/scripts/m1-finish-and-m2-tests-wf_fabaf03d-9d6.js`
- 説明: M1 を緑にする（コンパイル修正→領域別 slt 修正→統合）と、M2 の slt テスト作成を並列実行
- journal.jsonl の行数: 27、agent-*.jsonl の数: 13、started の担当数: 13

| フェーズ | 計画での説明 | 実績の担当数 (結果あり) | 開始 (UTC) | 終了 (UTC) | 壁時計 | 備考 |
|---|---|---|---|---|---|---|
| Stabilize | cargo test のコンパイルエラーを直す | 1 (1) | 2026-10-04 14:35:15 | 2026-10-04 14:36:15 | 1m00s |  |
| Fix | slt 領域ごとに yuzhu を修正 | 10 (10) | 2026-10-04 14:36:15 | 2026-10-04 14:36:46 | 0m31s |  |
| Integrate | 全体確認・PROGRESS 更新・コミット | 1 (1) | 2026-10-04 14:36:46 | 2026-10-04 14:37:35 | 0m49s |  |
| M2 tests | tests/slt/m2 を PostgreSQL で検証しつつ作成 | 1 (1) | 2026-10-04 14:36:15 | 2026-10-04 14:49:17 | 13m02s |  |

#### wf_0cc0a7be-171 m2-implement (進行中)

- 計画スクリプト: `<session>/workflows/scripts/m2-implement-wf_0cc0a7be-171.js`
- 説明: M2（永続化）を spec/design/m2.md 第8節の担当表どおりに依存順で並列実装し、結合・レビューまで行う
- journal.jsonl の行数: 42、agent-*.jsonl の数: 21、started の担当数: 21

| フェーズ | 計画での説明 | 実績の担当数 (結果あり) | 開始 (UTC) | 終了 (UTC) | 壁時計 | 備考 |
|---|---|---|---|---|---|---|
| A foundation | 基盤とスタブ | 1 (1) | 2026-10-04 14:50:14 | 2026-10-04 15:20:13 | 29m59s |  |
| Parallel | B C D E F H1 H2 を並列実装 | 8 (8) | 2026-10-04 15:20:13 | 2026-10-04 16:04:34 | 44m21s |  |
| Assemble | G → I → J | 4 (3) | 2026-10-04 16:04:34 | 2026-10-04 21:24:00 | 5h19m | 進行中の担当あり |
| Integrate | slt m1+m2・再起動テストを通す | 1 (1) | 2026-10-04 21:24:00 | 2026-10-04 21:27:57 | 3m57s |  |
| Review | 3 観点レビューと修正 | 7 (7) | 2026-10-04 21:27:57 | 2026-10-04 21:57:14 | 29m17s |  |

#### wf_dc96a8cc-646 journal-design-and-write (完了)

- 計画スクリプト: `<session>/workflows/scripts/journal-design-and-write-wf_dc96a8cc-646.js`
- 説明: 振り返り用の作業日誌に何を書くべきかを複数観点で検討し、構成を決めて日誌を書く
- journal.jsonl の行数: 43、agent-*.jsonl の数: 21、started の担当数: 21

| フェーズ | 計画での説明 | 実績の担当数 (結果あり) | 開始 (UTC) | 終了 (UTC) | 壁時計 | 備考 |
|---|---|---|---|---|---|---|
| Propose | 観点別に記録項目を提案 | 4 (4) | 2026-10-04 14:59:52 | 2026-10-04 15:03:48 | 3m56s |  |
| Design | 提案を統合して日誌の構成を決める | 1 (1) | 2026-10-04 15:03:48 | 2026-10-04 15:09:12 | 5m24s |  |
| Write | 項目ごとに日誌を書く | 15 (15) | 2026-10-04 15:09:12 | 2026-10-04 15:28:59 | 19m47s |  |
| Review | 事実確認と抜け漏れチェック | 1 (1) | 2026-10-04 15:28:59 | 2026-10-04 15:33:41 | 4m42s |  |

#### wf_4d730f24-348 design-doc (進行中)

- 計画スクリプト: `<session>/workflows/scripts/design-doc-wf_4d730f24-348.js`
- 説明: 次マイルストーンの基本設計書を、章立て→章ごとの並列執筆→3観点レビュー→修正で作る（コードは触らない）
- journal.jsonl の行数: 32、agent-*.jsonl の数: 16、started の担当数: 16

| フェーズ | 計画での説明 | 実績の担当数 (結果あり) | 開始 (UTC) | 終了 (UTC) | 壁時計 | 備考 |
|---|---|---|---|---|---|---|
| Outline | 章立てと契約の骨子 | 1 (1) | 2026-10-04 20:53:24 | 2026-10-05 04:55:41 | 8h02m |  |
| Write | 章ごとに並列執筆 | 11 (10) | 2026-10-05 04:55:41 | 2026-10-05 06:53:02 | 1h57m | 進行中の担当あり |
| Review | 観点別レビュー | 3 (3) | 2026-10-05 06:53:02 | 2026-10-05 07:23:55 | 30m53s |  |
| Fix | 指摘の反映と索引 | 1 (0) | 2026-10-05 07:23:55 | 2026-10-05 07:32:06 | 8m11s | 進行中の担当あり |

#### wf_32cf6626-a0c design-doc (進行中)

- 計画スクリプト: `<session>/workflows/scripts/design-doc-wf_32cf6626-a0c.js`
- 説明: 次マイルストーンの基本設計書を、章立て→章ごとの並列執筆→3観点レビュー→修正で作る（コードは触らない）
- journal.jsonl の行数: 32、agent-*.jsonl の数: 16、started の担当数: 16

| フェーズ | 計画での説明 | 実績の担当数 (結果あり) | 開始 (UTC) | 終了 (UTC) | 壁時計 | 備考 |
|---|---|---|---|---|---|---|
| Outline | 章立てと契約の骨子 | 2 (1) | 2026-10-04 20:53:36 | 2026-10-05 05:08:19 | 8h14m | 進行中の担当あり |
| Write | 章ごとに並列執筆 | 10 (10) | 2026-10-05 05:08:19 | 2026-10-05 06:12:22 | 1h04m |  |
| Review | 観点別レビュー | 3 (3) | 2026-10-05 06:12:22 | 2026-10-05 06:26:26 | 14m04s |  |
| Fix | 指摘の反映と索引 | 1 (1) | 2026-10-05 06:26:26 | 2026-10-05 07:05:47 | 39m21s |  |

#### wf_5e84e5ec-172 m3-shared-tests (完了)

- 計画スクリプト: `<session>/workflows/scripts/m3-shared-tests-wf_5e84e5ec-172.js`
- 説明: M3 の共有テスト（slt・再起動/クラッシュ・分離性）を PostgreSQL 17 で検証しながら作る（K 担当）
- journal.jsonl の行数: 11、agent-*.jsonl の数: 5、started の担当数: 5

| フェーズ | 計画での説明 | 実績の担当数 (結果あり) | 開始 (UTC) | 終了 (UTC) | 壁時計 | 備考 |
|---|---|---|---|---|---|---|
| Write | 領域別に並列作成 | 4 (4) | 2026-10-04 20:53:49 | 2026-10-04 21:24:02 | 30m13s |  |
| Verify | PG17 での全件確認と整合 | 1 (1) | 2026-10-04 21:24:02 | 2026-10-04 21:48:55 | 24m53s |  |

#### wf_b10ff45b-956 m3-implement (進行中)

- 計画スクリプト: `<session>/workflows/scripts/m3-implement-wf_b10ff45b-956.js`
- 説明: M3（WAL・リカバリ・トランザクション特性）を m3.md 第8節の担当表どおり依存グラフで並列実装→領域別並列修正の統合→レビュー
- journal.jsonl の行数: 60、agent-*.jsonl の数: 30、started の担当数: 30

| フェーズ | 計画での説明 | 実績の担当数 (結果あり) | 開始 (UTC) | 終了 (UTC) | 壁時計 | 備考 |
|---|---|---|---|---|---|---|
| A foundation | 基盤とスタブ、m3.md の訂正 | 1 (1) | 2026-10-04 21:59:20 | 2026-10-05 04:55:42 | 6h56m |  |
| Build | W1 B F S → W2 C E → D → R → T J の依存グラフ | 12 (12) | 2026-10-05 04:55:42 | 2026-10-05 06:15:41 | 1h19m |  |
| Integrate | 全テスト → 失敗領域ごとに並列修正（最大4周） | 8 (8) | 2026-10-05 06:15:41 | 2026-10-05 06:27:17 | 11m36s |  |
| Review | 観点別レビューと修正 | 8 (8) | 2026-10-05 06:27:17 | 2026-10-05 07:24:36 | 57m19s |  |
| Finish | 日誌・PROGRESS 更新とコミット | 1 (0) | 2026-10-05 07:24:36 | 2026-10-05 07:32:04 | 7m28s | 進行中の担当あり |

### 結果スキーマの不揃い(result に passed キーが無い担当)

出所: journal.jsonl の `type==result` の `result` オブジェクトのキー。Workflow ごとに結果スキーマが違うため、passed 以外(done / items など)を使うものは全てここに載る。完了した担当だけが対象。

| Workflow | 担当 | フェーズ | result のキー |
|---|---|---|---|
| wf_0cc0a7be-171 | A-foundation | A foundation | done, issues, summary |
| wf_0cc0a7be-171 | B-impl | Parallel | done, issues, summary |
| wf_0cc0a7be-171 | C-impl | Parallel | done, summary |
| wf_0cc0a7be-171 | D-impl | Parallel | done, issues, summary |
| wf_0cc0a7be-171 | E-impl | Parallel | done, issues, summary |
| wf_0cc0a7be-171 | F-impl | Parallel | done, summary |
| wf_0cc0a7be-171 | H1-impl | Parallel | done, summary |
| wf_0cc0a7be-171 | H2-impl | Parallel | done, summary |
| wf_0cc0a7be-171 | D-retry | Parallel | done, issues, summary |
| wf_0cc0a7be-171 | G-engine | Assemble | done, summary |
| wf_0cc0a7be-171 | I-session | Assemble | done, summary |
| wf_0cc0a7be-171 | J-server | Assemble | done, summary |
| wf_0cc0a7be-171 | integrate-1 | Integrate | done, issues, summary |
| wf_0cc0a7be-171 | review:crash-safety | Review | findings |
| wf_0cc0a7be-171 | review:mvcc-concurrency | Review | findings |
| wf_0cc0a7be-171 | review:pg-compat | Review | findings |
| wf_0cc0a7be-171 | fix:pg-compat | Review | done, issues, summary |
| wf_0cc0a7be-171 | fix:mvcc-concurrency | Review | done, issues, summary |
| wf_0cc0a7be-171 | fix:crash-safety | Review | done, summary |
| wf_0cc0a7be-171 | final | Review | done, summary |
| wf_dc96a8cc-646 | propose:process-retro | Propose | items |
| wf_dc96a8cc-646 | propose:decisions | Propose | items |
| wf_dc96a8cc-646 | propose:ai-ops | Propose | items |
| wf_dc96a8cc-646 | propose:future-reader | Propose | items |
| wf_dc96a8cc-646 | design | Design | automation, files, structure |
| wf_dc96a8cc-646 | write:journal/README.md | Write | done, summary |
| wf_dc96a8cc-646 | write:journal/timeline.md | Write | done, summary |
| wf_dc96a8cc-646 | write:journal/decisions.md | Write | done, summary |
| wf_dc96a8cc-646 | write:journal/workflows.md | Write | done, summary |
| wf_dc96a8cc-646 | write:journal/setbacks.md | Write | done, summary |
| wf_dc96a8cc-646 | write:journal/metrics.md | Write | done, summary |
| wf_dc96a8cc-646 | write:journal/retrospective.md | Write | done, summary |
| wf_dc96a8cc-646 | write:journal/snapshots.md | Write | done, summary |
| wf_dc96a8cc-646 | write:tools/journal-lib.sh | Write | done, summary |
| wf_dc96a8cc-646 | write:tools/journal-all.sh | Write | done, summary |
| wf_dc96a8cc-646 | write:tools/journal-timeline.sh | Write | done, summary |
| wf_dc96a8cc-646 | write:tools/journal-workflows.sh | Write | done, summary |
| wf_dc96a8cc-646 | write:tools/journal-decisions.sh | Write | done, summary |
| wf_dc96a8cc-646 | write:tools/journal-metrics.sh | Write | done, summary |
| wf_dc96a8cc-646 | write:tools/journal-verify.sh | Write | done, summary |
| wf_dc96a8cc-646 | review | Review | done, summary |
| wf_4d730f24-348 | outline | Outline | chapters, contracts |
| wf_4d730f24-348 | write:02-pipeline-refactor.md | Write | done, summary |
| wf_4d730f24-348 | write:03-parser-analyzer.md | Write | done, summary |
| wf_4d730f24-348 | write:04-planner-optimizer.md | Write | done, summary |
| wf_4d730f24-348 | write:05-executor.md | Write | done, summary |
| wf_4d730f24-348 | write:06-btree.md | Write | done, summary |
| wf_4d730f24-348 | write:07-catalog-ddl.md | Write | done, summary |
| wf_4d730f24-348 | write:08-sequence-serial.md | Write | done, summary |
| wf_4d730f24-348 | write:09-types-functions.md | Write | done, summary |
| wf_4d730f24-348 | write:10-explain-copy-compat.md | Write | done, summary |
| wf_4d730f24-348 | write:11-tests-plan.md | Write | done, summary |
| wf_4d730f24-348 | review:consistency | Review | findings |
| wf_4d730f24-348 | review:implementability | Review | findings |
| wf_4d730f24-348 | review:pg-compat | Review | findings |
| wf_32cf6626-a0c | outline | Outline | chapters, contracts |
| wf_32cf6626-a0c | write:01-lock-txn.md | Write | done, summary |
| wf_32cf6626-a0c | write:02-row-lock-rr.md | Write | done, summary |
| wf_32cf6626-a0c | write:03-vacuum.md | Write | done, summary |
| wf_32cf6626-a0c | write:04-extended-query.md | Write | done, summary |
| wf_32cf6626-a0c | write:05-types-core.md | Write | done, summary |
| wf_32cf6626-a0c | write:06-types-datetime.md | Write | done, summary |
| wf_32cf6626-a0c | write:07-foreign-key.md | Write | done, summary |
| wf_32cf6626-a0c | write:08-auth-roles.md | Write | done, summary |
| wf_32cf6626-a0c | write:09-database-ddl.md | Write | done, summary |
| wf_32cf6626-a0c | write:10-tests-plan.md | Write | done, summary |
| wf_32cf6626-a0c | review:consistency | Review | findings |
| wf_32cf6626-a0c | review:pg-compat | Review | findings |
| wf_32cf6626-a0c | review:implementability | Review | findings |
| wf_32cf6626-a0c | fix-and-index | Fix | done, summary |
| wf_5e84e5ec-172 | K:slt | Write | done, issues, summary |
| wf_5e84e5ec-172 | K:crash | Write | done, issues, summary |
| wf_5e84e5ec-172 | K:isolation | Write | done, summary |
| wf_5e84e5ec-172 | K:ci | Write | done, issues, summary |
| wf_5e84e5ec-172 | K:verify | Verify | done, issues, summary |
| wf_b10ff45b-956 | A-foundation | A foundation | done, issues, summary |
| wf_b10ff45b-956 | W1 | Build | done, summary |
| wf_b10ff45b-956 | B | Build | done, summary |
| wf_b10ff45b-956 | F | Build | done, issues, summary |
| wf_b10ff45b-956 | S | Build | done, issues, summary |
| wf_b10ff45b-956 | W2 | Build | done, summary |
| wf_b10ff45b-956 | C | Build | done, issues, summary |
| wf_b10ff45b-956 | E | Build | done, issues, summary |
| wf_b10ff45b-956 | D | Build | done, issues, summary |
| wf_b10ff45b-956 | R | Build | done, summary |
| wf_b10ff45b-956 | T | Build | done, issues, summary |
| wf_b10ff45b-956 | J | Build | done, issues, summary |
| wf_b10ff45b-956 | S-finish | Build | done, summary |
| wf_b10ff45b-956 | test-1 | Integrate | allPass, failures, summary |
| wf_b10ff45b-956 | fix:crash:r1 | Integrate | done, issues, summary |
| wf_b10ff45b-956 | fix:isolation:r1 | Integrate | done, issues, summary |
| wf_b10ff45b-956 | fix:cargo:r1 | Integrate | done, summary |
| wf_b10ff45b-956 | test-2 | Integrate | allPass, failures, summary |
| wf_b10ff45b-956 | fix:cargo:r2 | Integrate | done, summary |
| wf_b10ff45b-956 | fix:isolation:r2 | Integrate | done, summary |
| wf_b10ff45b-956 | test-3 | Integrate | allPass, failures, summary |
| wf_b10ff45b-956 | review:wal-recovery | Review | findings |
| wf_b10ff45b-956 | review:txn-concurrency | Review | findings |
| wf_b10ff45b-956 | review:crash-test-quality | Review | findings |
| wf_b10ff45b-956 | review:pg-compat | Review | findings |
| wf_b10ff45b-956 | fix:txn-concurrency | Review | done, issues, summary |
| wf_b10ff45b-956 | fix:pg-compat | Review | done, issues, summary |
| wf_b10ff45b-956 | fix:crash-test-quality | Review | done, summary |
| wf_b10ff45b-956 | fix:wal-recovery | Review | done, issues, summary |

### first-pass 判定

条件: result.summary に「初回」「1周目」「修正は不要」「コード変更なし」のいずれかを含み、tool_use が 10 回以下(StructuredOutput 呼び出しを含む)。summary を持たない result(スキーマ違い)は判定できない。文字列一致による目安で、解釈は手書きで行う。

| Workflow | 担当 | tool_use | 一致した語 |
|---|---|---|---|
| wf_fabaf03d-9d6 | fix:ddl | 6 | 修正は不要 |
| wf_fabaf03d-9d6 | fix:constraints | 6 | 修正は不要 |
| wf_fabaf03d-9d6 | fix:expressions | 7 | 初回 |
| wf_fabaf03d-9d6 | fix:types | 6 | 修正は不要 |
| wf_fabaf03d-9d6 | fix:functions | 4 | 初回 / 修正は不要 |
| wf_fabaf03d-9d6 | fix:txn | 5 | 初回 |
| wf_fabaf03d-9d6 | fix:errors | 5 | 初回 |
| wf_dc96a8cc-646 | write:tools/journal-metrics.sh | 7 | 初回 |

### 進行中の担当(result 未着)

| Workflow | 担当 | フェーズ | 開始 (UTC) | 最終ログ (UTC) | ここまでの所要 |
|---|---|---|---|---|---|
| wf_0cc0a7be-171 | J-server | Assemble | 2026-10-04 20:47:55 | 2026-10-04 20:49:14 | 1m19s |
| wf_4d730f24-348 | write:01-scope-decisions.md | Write | 2026-10-05 04:55:41 | 2026-10-05 05:25:26 | 29m45s |
| wf_4d730f24-348 | fix-and-index | Fix | 2026-10-05 07:23:55 | 2026-10-05 07:32:06 | 8m11s |
| wf_32cf6626-a0c | outline | Outline | 2026-10-04 20:53:36 | 2026-10-05 04:49:33 | 7h55m |
| wf_b10ff45b-956 | finish | Finish | 2026-10-05 07:24:36 | 2026-10-05 07:32:04 | 7m28s |
<!-- AUTO:ledger END -->

読み方と補足（手書き）:

- `wf_fabaf03d`（M1 完成 + M2 テスト作成）: stabilize(1) → fix:* 10 領域並列 + K-m2-tests → integrate(1) の計 13 担当。壁時計は 14m02s（14:35:15〜14:49:17 UTC、JST 23:35〜23:49）で、K-m2-tests の 13m02s が支配した。WORKLOG.md の「約 14 分」と一致する。
- `wf_0cc0a7be`（M2 実装）: 計画は A → Parallel(B C D E F H1 H2) → Assemble(G → I → J) → Integrate → Review（`m2-implement-wf_0cc0a7be-171.js` の `meta.phases` と本体）。**進行中**。結果欄は完了後に追記する。担当数は「現時点で起動済み」の数で、最終的な数ではない。
- `wf_dc96a8cc`: この日誌を設計して書く Workflow（`journal-design-and-write`）。M2 実装と同時に走っているため、表に自動で載る。M2 の壁時計と並走していることに注意（同じマシンを使うので、所要時間の比較には交絡する）。
- 「結果要約」列は自動集計の `passed=true/false` の件数だけ。M2 実装の result は `done` キーなので、この欄は 0/0 になる。中身は第 4 節と各 Workflow の所見を見る。

## 2. 計画と実績の差

### wf_fabaf03d（M1 完成 + M2 テスト作成）

| フェーズ | 計画（meta.phases / script） | 実績 | 差・気づき |
|---|---|---|---|
| Stabilize | 1 担当。`cargo test` のコンパイルエラーを直す | 14:35:15〜14:36:15（1m00s）。10 ターン、tool_use 11 | プロンプトは `analyzer/tests.rs` 付近と書いていたが、実際に壊れていたのは `planner/mod.rs` のテスト `insert_plan_and_execution`（`BoundInsert` に `coercions: None` が無い）。clippy の `assert_is_empty` 9 件も直した（result.summary より） |
| Fix | slt 10 領域を並列。失敗した領域だけ `fix2:*` で再実行（最大 12 周） | 10 担当が 14:36:15 に同時開始、13〜31 秒で全員が passed:true | `fix2:*` は 1 本も起動していない（journal に該当 label が無い）。再実行の仕組みは未使用のまま |
| M2 tests | K-m2-tests。m2.md 第 8 節では**依存なし** | 14:36:15〜14:49:17（13m02s） | script 上、K は stabilize の結果を待ってから起動している（journal の順序と開始時刻 14:36:15 が一致）。依存が無いのに 1m00s 遅れて始まった。K が最長経路だったので、この 1 分は全体の壁時計にそのまま乗った |
| Integrate | fix の全結果を待って 1 担当 | 14:36:46〜14:37:35（49s）。fix:ddl（31s）の終了直後に開始 | K を待たずに走った。M1 の結果（yuzhu 37/37、PG17 37/37）は 14:37:35 に出ていたが、Workflow 全体の完了は 14:49:17 で、利用者に見えたのは約 14:50（WORKLOG.md）。約 12 分遅れて見えた |

M1 側だけの最長経路は stabilize 1m00s → fix 最長 31s → integrate 49s = **2m20s**（14:35:15〜14:37:35）。K が無ければ Workflow は約 2.3 分で終わっていた。

### wf_0cc0a7be（M2 実装）。2026-10-04 15:26 UTC 時点の途中経過

| フェーズ | 計画 | 実績（途中） | 差・気づき |
|---|---|---|---|
| A foundation | 1 担当。★ファイルのスタブ、第 4 節の型とトレイト | 14:50:14〜15:20:13（29m59s）で完了。85 ターン、tool_use 95、出力 111,384 トークン。コミット 610c121（15:19:58 UTC） | m2.md 第 8 節の A の見積もりは 1.5（AI 1 本の日数。粗い見積もり）。実時間は 30 分。以降の 7 担当すべてが A の完了待ちだったので、ここが最初のボトルネック |
| Parallel | B C D E F H1 H2 を並列。未完了は 1 回だけ `*-retry` で再実行 | 15:20:13 に 7 担当が同時開始。完了は未確認 | m2.md 第 8 節は「D は C と E の API が置かれ次第」と書くが、script は D も同時に起動する（A がトレイトとスタブを置いたため）。この前提が成り立つかは D の結果で判断する |
| Assemble | G → I → J の直列 | 未着手 | 依存順なので並列化できない。所要は 3 担当の合計になる |
| Integrate | 最大 3 ラウンド | 未着手 | |
| Review | 3 観点（crash-safety / mvcc-concurrency / pg-compat）の pipeline、各観点に修正担当、最後に最終確認 | 未着手 | |

- 開始時刻のずれ: WORKLOG.md は M2 実装の開始を「約 14:52」と書くが、A のトランスクリプトの最初の timestamp は 14:50:14（UTC）。約 2 分の差。どちらが正かは不明（WORKLOG は概算の可能性がある）。
- 完了後にこの節を更新する（実際の順序、遅れ、飛ばし、`*-retry` や統合の再ラウンドの有無）。

### wf_dc96a8cc（日誌の設計と執筆）。途中

- Propose の 4 担当（process-retro / decisions / ai-ops / future-reader）が 14:59:52 前後に並列開始、約 3m45s〜3m54s で完了。Design 1 担当が 15:03:48〜（5m24s）。Write の担当が 15:09:12 から並列で起動した。Review は未着手（2026-10-04 15:26 UTC 時点）。完了後に追記する。

## 3. 担当別の実績

<!-- AUTO:agents BEGIN -->
> 自動生成。出所: agent-*.jsonl。ターン = message.id ごとに 1 回と数えた assistant 応答の数。tool_use = tool_use ブロックの id の重複なし件数(StructuredOutput を含む)。エラー = `is_error:true` の tool_result の件数。
> トークンは message.usage の合計(message.id ごとに最後の行を採用)。入力 = input_tokens、出力 = output_tokens、cache読 = cache_read_input_tokens、cache作成 = cache_creation_input_tokens。モデル = message.model。
> 状態: 「完了」= journal に result あり、「進行中」= result 未着(所要・トークンは暫定)。passed は result.passed(キー無しは「-」)。

### wf_fabaf03d-9d6 m1-finish-and-m2-tests (完了)

| 担当 | フェーズ | 状態 | passed | 開始 (UTC) | 終了 (UTC) | 所要 | ターン | tool_use | エラー | 入力 | 出力 | cache読 | cache作成 | モデル | first-pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| stabilize | Stabilize | 完了 | true | 2026-10-04 14:35:15 | 2026-10-04 14:36:15 | 1m00s | 10 | 11 | 0 | 20 | 2833 | 281914 | 37274 | claude-sonnet-5-5 | - |
| K-m2-tests | M2 tests | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:49:17 | 13m02s | 52 | 54 | 3 | 104 | 78949 | 5068223 | 148928 | claude-sonnet-5-5 | - |
| fix:ddl | Fix | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:36:46 | 0m31s | 5 | 6 | 0 | 10 | 1077 | 132636 | 11441 | claude-sonnet-5-5 | first-pass |
| fix:insert | Fix | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:36:31 | 0m16s | 4 | 4 | 0 | 8 | 945 | 101544 | 10234 | claude-sonnet-5-5 | - |
| fix:constraints | Fix | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:36:33 | 0m18s | 5 | 6 | 0 | 10 | 1246 | 132147 | 11726 | claude-sonnet-5-5 | first-pass |
| fix:select | Fix | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:36:40 | 0m25s | 5 | 5 | 0 | 10 | 1304 | 131331 | 10890 | claude-sonnet-5-5 | - |
| fix:expressions | Fix | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:36:39 | 0m24s | 6 | 7 | 1 | 12 | 2006 | 160660 | 11713 | claude-sonnet-5-5 | first-pass |
| fix:types | Fix | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:36:29 | 0m14s | 4 | 6 | 0 | 8 | 882 | 101728 | 10337 | claude-sonnet-5-5 | first-pass |
| fix:functions | Fix | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:36:28 | 0m13s | 4 | 4 | 0 | 8 | 817 | 100134 | 9250 | claude-sonnet-5-5 | first-pass |
| fix:txn | Fix | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:36:30 | 0m15s | 5 | 5 | 0 | 10 | 834 | 130427 | 10309 | claude-sonnet-5-5 | first-pass |
| fix:session | Fix | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:36:34 | 0m19s | 5 | 5 | 0 | 10 | 1073 | 129526 | 10470 | claude-sonnet-5-5 | - |
| fix:errors | Fix | 完了 | true | 2026-10-04 14:36:15 | 2026-10-04 14:36:33 | 0m18s | 4 | 5 | 0 | 8 | 774 | 102273 | 10867 | claude-sonnet-5-5 | first-pass |
| integrate | Integrate | 完了 | true | 2026-10-04 14:36:46 | 2026-10-04 14:37:35 | 0m49s | 12 | 14 | 0 | 24 | 4700 | 373798 | 20336 | claude-sonnet-5-5 | - |

合計: ターン 121、tool_use 132、エラー 4、入力 242、出力 97440、cache読 6946341、cache作成 313775

### wf_0cc0a7be-171 m2-implement (進行中)

| 担当 | フェーズ | 状態 | passed | 開始 (UTC) | 終了 (UTC) | 所要 | ターン | tool_use | エラー | 入力 | 出力 | cache読 | cache作成 | モデル | first-pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A-foundation | A foundation | 完了 | - | 2026-10-04 14:50:14 | 2026-10-04 15:20:13 | 29m59s | 85 | 95 | 2 | 170 | 111384 | 17664412 | 311598 | claude-sonnet-5-5 | - |
| B-impl | Parallel | 完了 | - | 2026-10-04 15:20:13 | 2026-10-04 15:42:22 | 22m09s | 25 | 25 | 0 | 52 | 65626 | 2721103 | 142443 | claude-sonnet-5-5 | - |
| C-impl | Parallel | 完了 | - | 2026-10-04 15:20:13 | 2026-10-04 15:59:10 | 38m57s | 64 | 66 | 3 | 128 | 123852 | 10300714 | 219201 | claude-sonnet-5-5 | - |
| D-impl | Parallel | 完了 | - | 2026-10-04 15:20:13 | 2026-10-04 15:47:16 | 27m03s | 27 | 30 | 0 | 54 | 43306 | 2403235 | 361111 | claude-sonnet-5-5 | - |
| E-impl | Parallel | 完了 | - | 2026-10-04 15:20:13 | 2026-10-04 15:38:23 | 18m10s | 36 | 36 | 2 | 74 | 36406 | 3010040 | 89597 | claude-sonnet-5-5 | - |
| F-impl | Parallel | 完了 | - | 2026-10-04 15:20:13 | 2026-10-04 16:04:12 | 43m59s | 123 | 127 | 5 | 246 | 192111 | 27875230 | 359833 | claude-sonnet-5-5 | - |
| H1-impl | Parallel | 完了 | - | 2026-10-04 15:20:13 | 2026-10-04 15:33:07 | 12m54s | 45 | 48 | 1 | 90 | 29089 | 3966485 | 107931 | claude-sonnet-5-5 | - |
| H2-impl | Parallel | 完了 | - | 2026-10-04 15:20:13 | 2026-10-04 15:33:29 | 13m16s | 38 | 40 | 3 | 76 | 32586 | 4111389 | 131266 | claude-sonnet-5-5 | - |
| D-retry | Parallel | 完了 | - | 2026-10-04 16:04:12 | 2026-10-04 16:04:34 | 0m22s | 3 | 3 | 0 | 6 | 928 | 79239 | 12465 | claude-sonnet-5-5 | - |
| G-engine | Assemble | 完了 | - | 2026-10-04 16:04:34 | 2026-10-04 16:09:04 | 4m30s | 24 | 26 | 1 | 48 | 29933 | 2253345 | 113465 | claude-sonnet-5-5 | - |
| I-session | Assemble | 完了 | - | 2026-10-04 16:09:04 | 2026-10-04 20:47:55 | 4h38m | 63 | 70 | 0 | 126 | 40000 | 8044974 | 300780 | claude-sonnet-5-5 | - |
| J-server | Assemble | 進行中 | - | 2026-10-04 20:47:55 | 2026-10-04 20:49:14 | 1m19s | 5 | 6 | 0 | 10 | 1351 | 273386 | 66697 | claude-sonnet-5-5 | - |
| J-server | Assemble | 完了 | - | 2026-10-04 20:49:17 | 2026-10-04 21:24:00 | 34m43s | 35 | 41 | 1 | 70 | 49145 | 3950741 | 129685 | claude-sonnet-5-5 | - |
| integrate-1 | Integrate | 完了 | - | 2026-10-04 21:24:00 | 2026-10-04 21:27:57 | 3m57s | 35 | 42 | 3 | 70 | 13605 | 1854750 | 55831 | claude-sonnet-5-5 | - |
| review:crash-safety | Review | 完了 | - | 2026-10-04 21:27:57 | 2026-10-04 21:52:16 | 24m19s | 46 | 57 | 1 | 92 | 57606 | 6733617 | 409923 | claude-sonnet-5-5 | - |
| review:mvcc-concurrency | Review | 完了 | - | 2026-10-04 21:27:59 | 2026-10-04 21:50:41 | 22m42s | 40 | 41 | 0 | 80 | 53989 | 4000813 | 301859 | claude-sonnet-5-5 | - |
| review:pg-compat | Review | 完了 | - | 2026-10-04 21:27:59 | 2026-10-04 21:37:20 | 9m21s | 49 | 50 | 5 | 98 | 32481 | 4260944 | 105130 | claude-sonnet-5-5 | - |
| fix:pg-compat | Review | 完了 | - | 2026-10-04 21:37:20 | 2026-10-04 21:52:47 | 15m27s | 41 | 43 | 1 | 82 | 20805 | 2433784 | 100614 | claude-sonnet-5-5 | - |
| fix:mvcc-concurrency | Review | 完了 | - | 2026-10-04 21:50:41 | 2026-10-04 21:52:24 | 1m43s | 7 | 7 | 0 | 14 | 5369 | 224412 | 22316 | claude-sonnet-5-5 | - |
| fix:crash-safety | Review | 完了 | - | 2026-10-04 21:52:16 | 2026-10-04 21:54:39 | 2m23s | 22 | 23 | 0 | 44 | 10171 | 998752 | 42388 | claude-sonnet-5-5 | - |
| final | Review | 完了 | - | 2026-10-04 21:54:39 | 2026-10-04 21:57:14 | 2m35s | 14 | 15 | 0 | 28 | 8014 | 546405 | 30295 | claude-sonnet-5-5 | - |

合計: ターン 827、tool_use 891、エラー 28、入力 1658、出力 957757、cache読 107707770、cache作成 3414428

### wf_dc96a8cc-646 journal-design-and-write (完了)

| 担当 | フェーズ | 状態 | passed | 開始 (UTC) | 終了 (UTC) | 所要 | ターン | tool_use | エラー | 入力 | 出力 | cache読 | cache作成 | モデル | first-pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| propose:process-retro | Propose | 完了 | - | 2026-10-04 14:59:52 | 2026-10-04 15:03:43 | 3m51s | 6 | 6 | 0 | 12 | 4788 | 178751 | 48618 | claude-sonnet-5-5 | - |
| propose:decisions | Propose | 完了 | - | 2026-10-04 14:59:54 | 2026-10-04 15:03:37 | 3m43s | 4 | 5 | 1 | 8 | 3807 | 122839 | 22374 | claude-sonnet-5-5 | - |
| propose:ai-ops | Propose | 完了 | - | 2026-10-04 14:59:54 | 2026-10-04 15:03:48 | 3m54s | 7 | 8 | 0 | 14 | 6149 | 225289 | 21087 | claude-sonnet-5-5 | - |
| propose:future-reader | Propose | 完了 | - | 2026-10-04 14:59:54 | 2026-10-04 15:03:39 | 3m45s | 4 | 5 | 1 | 8 | 4417 | 114173 | 16205 | claude-sonnet-5-5 | - |
| design | Design | 完了 | - | 2026-10-04 15:03:48 | 2026-10-04 15:09:12 | 5m24s | 2 | 1 | 0 | 4 | 15666 | 18740 | 62373 | claude-sonnet-5-5 | - |
| write:journal/README.md | Write | 完了 | - | 2026-10-04 15:09:12 | 2026-10-04 15:19:48 | 10m36s | 10 | 10 | 1 | 20 | 10712 | 506184 | 56622 | claude-sonnet-5-5 | - |
| write:journal/timeline.md | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:20:43 | 11m29s | 13 | 14 | 0 | 26 | 17649 | 592226 | 37937 | claude-sonnet-5-5 | - |
| write:journal/decisions.md | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:26:54 | 17m40s | 29 | 30 | 0 | 56 | 28463 | 2223946 | 95537 | <synthetic>,claude-sonnet-5-5 | - |
| write:journal/workflows.md | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:28:16 | 19m02s | 29 | 31 | 0 | 58 | 32709 | 2178969 | 74046 | claude-sonnet-5-5 | - |
| write:journal/setbacks.md | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:25:14 | 16m00s | 28 | 27 | 0 | 54 | 25076 | 1744190 | 77472 | <synthetic>,claude-sonnet-5-5 | - |
| write:journal/metrics.md | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:21:16 | 12m02s | 16 | 13 | 0 | 32 | 15343 | 664817 | 45705 | claude-sonnet-5-5 | - |
| write:journal/retrospective.md | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:20:26 | 11m12s | 14 | 15 | 1 | 28 | 13204 | 674109 | 42134 | claude-sonnet-5-5 | - |
| write:journal/snapshots.md | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:21:50 | 12m36s | 9 | 13 | 1 | 18 | 9695 | 376689 | 43845 | claude-sonnet-5-5 | - |
| write:tools/journal-lib.sh | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:12:50 | 3m36s | 5 | 5 | 0 | 10 | 3859 | 151512 | 17886 | claude-sonnet-5-5 | - |
| write:tools/journal-all.sh | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:12:38 | 3m24s | 4 | 4 | 1 | 8 | 477 | 115165 | 16187 | claude-sonnet-5-5 | - |
| write:tools/journal-timeline.sh | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:16:29 | 7m15s | 8 | 9 | 0 | 16 | 5903 | 274474 | 26831 | claude-sonnet-5-5 | - |
| write:tools/journal-workflows.sh | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:28:59 | 19m45s | 17 | 18 | 1 | 34 | 28646 | 993442 | 66517 | claude-sonnet-5-5 | - |
| write:tools/journal-decisions.sh | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:22:04 | 12m50s | 20 | 18 | 1 | 40 | 15304 | 943212 | 54085 | claude-sonnet-5-5 | - |
| write:tools/journal-metrics.sh | Write | 完了 | - | 2026-10-04 15:09:14 | 2026-10-04 15:19:41 | 10m27s | 7 | 7 | 1 | 14 | 8640 | 246289 | 21176 | claude-sonnet-5-5 | first-pass |
| write:tools/journal-verify.sh | Write | 完了 | - | 2026-10-04 15:12:38 | 2026-10-04 15:16:28 | 3m50s | 6 | 7 | 0 | 12 | 5630 | 194953 | 26265 | claude-sonnet-5-5 | - |
| review | Review | 完了 | - | 2026-10-04 15:28:59 | 2026-10-04 15:33:41 | 4m42s | 21 | 22 | 1 | 42 | 10369 | 1295144 | 67970 | claude-sonnet-5-5 | - |

合計: ターン 259、tool_use 268、エラー 10、入力 514、出力 266506、cache読 13835113、cache作成 940872

### wf_4d730f24-348 design-doc (進行中)

| 担当 | フェーズ | 状態 | passed | 開始 (UTC) | 終了 (UTC) | 所要 | ターン | tool_use | エラー | 入力 | 出力 | cache読 | cache作成 | モデル | first-pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| outline | Outline | 完了 | - | 2026-10-04 20:53:24 | 2026-10-05 04:55:41 | 8h02m | 61 | 72 | 1 | 120 | 214325 | 18031983 | 1369998 | <synthetic>,claude-sonnet-5-5 | - |
| write:01-scope-decisions.md | Write | 進行中 | - | 2026-10-05 04:55:41 | 2026-10-05 05:25:26 | 29m45s | 54 | 69 | 0 | 106 | 66230 | 11564172 | 335101 | <synthetic>,claude-sonnet-5-5 | - |
| write:02-pipeline-refactor.md | Write | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 06:05:20 | 1h09m | 66 | 109 | 1 | 132 | 245971 | 18739859 | 680384 | claude-sonnet-5-5 | - |
| write:03-parser-analyzer.md | Write | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 06:03:28 | 1h07m | 183 | 203 | 3 | 366 | 385707 | 96829622 | 833044 | claude-sonnet-5-5 | - |
| write:04-planner-optimizer.md | Write | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 06:02:37 | 1h06m | 68 | 79 | 1 | 136 | 273642 | 19612257 | 744885 | claude-sonnet-5-5 | - |
| write:05-executor.md | Write | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 05:30:23 | 34m41s | 30 | 42 | 1 | 60 | 113504 | 4549246 | 416187 | claude-sonnet-5-5 | - |
| write:06-btree.md | Write | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 06:20:18 | 1h24m | 96 | 113 | 2 | 192 | 336631 | 42240675 | 1342110 | claude-sonnet-5-5 | - |
| write:07-catalog-ddl.md | Write | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 05:44:59 | 49m17s | 106 | 130 | 3 | 212 | 270553 | 35685211 | 571318 | claude-sonnet-5-5 | - |
| write:08-sequence-serial.md | Write | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 05:47:14 | 51m32s | 100 | 118 | 6 | 200 | 266251 | 35067324 | 1078240 | claude-sonnet-5-5 | - |
| write:09-types-functions.md | Write | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 05:31:38 | 35m56s | 79 | 89 | 2 | 158 | 152071 | 19689482 | 798387 | claude-sonnet-5-5 | - |
| write:10-explain-copy-compat.md | Write | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 05:47:33 | 51m51s | 90 | 95 | 5 | 180 | 257726 | 24657558 | 465255 | claude-sonnet-5-5 | - |
| write:11-tests-plan.md | Write | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 06:53:02 | 1h57m | 135 | 150 | 2 | 270 | 270450 | 61312454 | 3745839 | claude-sonnet-5-5 | - |
| review:consistency | Review | 完了 | - | 2026-10-05 06:53:02 | 2026-10-05 07:18:49 | 25m47s | 77 | 79 | 1 | 154 | 70857 | 26966252 | 556887 | claude-sonnet-5-5 | - |
| review:pg-compat | Review | 完了 | - | 2026-10-05 06:53:02 | 2026-10-05 07:23:55 | 30m53s | 151 | 152 | 3 | 302 | 128644 | 36414393 | 394293 | claude-sonnet-5-5 | - |
| review:implementability | Review | 完了 | - | 2026-10-05 06:53:02 | 2026-10-05 07:15:00 | 21m58s | 71 | 71 | 0 | 142 | 62984 | 14200485 | 337984 | claude-sonnet-5-5 | - |
| fix-and-index | Fix | 進行中 | - | 2026-10-05 07:23:55 | 2026-10-05 07:32:06 | 8m11s | 35 | 45 | 0 | 70 | 38335 | 5637152 | 229945 | claude-sonnet-5-5 | - |

合計: ターン 1402、tool_use 1616、エラー 31、入力 2800、出力 3153881、cache読 471198125、cache作成 13899857

### wf_32cf6626-a0c design-doc (進行中)

| 担当 | フェーズ | 状態 | passed | 開始 (UTC) | 終了 (UTC) | 所要 | ターン | tool_use | エラー | 入力 | 出力 | cache読 | cache作成 | モデル | first-pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| outline | Outline | 進行中 | - | 2026-10-04 20:53:36 | 2026-10-05 04:49:33 | 7h55m | 44 | 52 | 0 | 88 | 166670 | 11668869 | 800070 | claude-sonnet-5-5 | - |
| outline | Outline | 完了 | - | 2026-10-05 04:49:37 | 2026-10-05 05:08:19 | 18m42s | 65 | 75 | 1 | 130 | 108768 | 13002906 | 278972 | claude-sonnet-5-5 | - |
| write:01-lock-txn.md | Write | 完了 | - | 2026-10-05 05:08:19 | 2026-10-05 06:07:43 | 59m24s | 172 | 186 | 4 | 344 | 325658 | 75112509 | 719071 | claude-sonnet-5-5 | - |
| write:02-row-lock-rr.md | Write | 完了 | - | 2026-10-05 05:08:19 | 2026-10-05 06:12:22 | 1h04m | 140 | 158 | 5 | 280 | 313599 | 51014552 | 678031 | claude-sonnet-5-5 | - |
| write:03-vacuum.md | Write | 完了 | - | 2026-10-05 05:08:19 | 2026-10-05 06:02:50 | 54m31s | 82 | 103 | 2 | 164 | 305120 | 31467562 | 873176 | claude-sonnet-5-5 | - |
| write:04-extended-query.md | Write | 完了 | - | 2026-10-05 05:08:19 | 2026-10-05 05:56:00 | 47m41s | 89 | 104 | 2 | 178 | 251549 | 28410551 | 559282 | claude-sonnet-5-5 | - |
| write:05-types-core.md | Write | 完了 | - | 2026-10-05 05:08:19 | 2026-10-05 05:49:56 | 41m37s | 126 | 133 | 2 | 252 | 223024 | 36756481 | 524165 | claude-sonnet-5-5 | - |
| write:06-types-datetime.md | Write | 完了 | - | 2026-10-05 05:08:19 | 2026-10-05 05:35:12 | 26m53s | 62 | 73 | 6 | 122 | 111164 | 11888338 | 579093 | <synthetic>,claude-sonnet-5-5 | - |
| write:07-foreign-key.md | Write | 完了 | - | 2026-10-05 05:08:19 | 2026-10-05 05:43:34 | 35m15s | 56 | 65 | 2 | 112 | 164042 | 11678539 | 668416 | claude-sonnet-5-5 | - |
| write:08-auth-roles.md | Write | 完了 | - | 2026-10-05 05:08:19 | 2026-10-05 05:45:17 | 36m58s | 49 | 60 | 2 | 98 | 186603 | 8652435 | 640028 | claude-sonnet-5-5 | - |
| write:09-database-ddl.md | Write | 完了 | - | 2026-10-05 05:08:19 | 2026-10-05 05:46:26 | 38m07s | 58 | 74 | 3 | 116 | 175258 | 12015612 | 670621 | claude-sonnet-5-5 | - |
| write:10-tests-plan.md | Write | 完了 | - | 2026-10-05 05:08:19 | 2026-10-05 05:45:27 | 37m08s | 37 | 41 | 1 | 74 | 159166 | 6092980 | 764764 | claude-sonnet-5-5 | - |
| review:consistency | Review | 完了 | - | 2026-10-05 06:12:22 | 2026-10-05 06:21:07 | 8m45s | 87 | 89 | 1 | 174 | 57189 | 20498476 | 379965 | claude-sonnet-5-5 | - |
| review:pg-compat | Review | 完了 | - | 2026-10-05 06:12:23 | 2026-10-05 06:26:26 | 14m03s | 61 | 63 | 1 | 122 | 108685 | 14201534 | 389522 | claude-sonnet-5-5 | - |
| review:implementability | Review | 完了 | - | 2026-10-05 06:12:23 | 2026-10-05 06:23:33 | 11m10s | 64 | 66 | 2 | 128 | 76328 | 14028109 | 340245 | claude-sonnet-5-5 | - |
| fix-and-index | Fix | 完了 | - | 2026-10-05 06:26:26 | 2026-10-05 07:05:47 | 39m21s | 219 | 256 | 5 | 438 | 247650 | 86166173 | 644771 | claude-sonnet-5-5 | - |

合計: ターン 1411、tool_use 1598、エラー 39、入力 2820、出力 2980473、cache読 432655626、cache作成 9510192

### wf_5e84e5ec-172 m3-shared-tests (完了)

| 担当 | フェーズ | 状態 | passed | 開始 (UTC) | 終了 (UTC) | 所要 | ターン | tool_use | エラー | 入力 | 出力 | cache読 | cache作成 | モデル | first-pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| K:slt | Write | 完了 | - | 2026-10-04 20:53:49 | 2026-10-04 21:09:00 | 15m11s | 22 | 23 | 0 | 44 | 18300 | 1333617 | 88346 | claude-sonnet-5-5 | - |
| K:crash | Write | 完了 | - | 2026-10-04 20:53:49 | 2026-10-04 21:24:02 | 30m13s | 29 | 28 | 1 | 58 | 34136 | 2195801 | 85234 | claude-sonnet-5-5 | - |
| K:isolation | Write | 完了 | - | 2026-10-04 20:53:49 | 2026-10-04 21:09:51 | 16m02s | 31 | 33 | 2 | 64 | 22863 | 2383821 | 74495 | claude-sonnet-5-5 | - |
| K:ci | Write | 完了 | - | 2026-10-04 20:53:49 | 2026-10-04 21:04:04 | 10m15s | 13 | 14 | 1 | 26 | 7345 | 558578 | 38565 | claude-sonnet-5-5 | - |
| K:verify | Verify | 完了 | - | 2026-10-04 21:24:02 | 2026-10-04 21:48:55 | 24m53s | 28 | 30 | 2 | 56 | 14907 | 1351725 | 123522 | claude-sonnet-5-5 | - |

合計: ターン 123、tool_use 128、エラー 6、入力 248、出力 97551、cache読 7823542、cache作成 410162

### wf_b10ff45b-956 m3-implement (進行中)

| 担当 | フェーズ | 状態 | passed | 開始 (UTC) | 終了 (UTC) | 所要 | ターン | tool_use | エラー | 入力 | 出力 | cache読 | cache作成 | モデル | first-pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A-foundation | A foundation | 完了 | - | 2026-10-04 21:59:20 | 2026-10-05 04:55:42 | 6h56m | 54 | 60 | 1 | 108 | 58940 | 7371283 | 281414 | claude-sonnet-5-5 | - |
| W1 | Build | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 05:03:41 | 7m59s | 32 | 34 | 1 | 64 | 58239 | 3050982 | 124832 | claude-sonnet-5-5 | - |
| B | Build | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 05:13:19 | 17m37s | 36 | 49 | 2 | 74 | 30236 | 3189649 | 92722 | claude-sonnet-5-5 | - |
| F | Build | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 05:03:05 | 7m23s | 55 | 62 | 5 | 110 | 38315 | 5038197 | 110432 | claude-sonnet-5-5 | - |
| S | Build | 完了 | - | 2026-10-05 04:55:42 | 2026-10-05 05:05:04 | 9m22s | 61 | 68 | 1 | 122 | 64277 | 8373077 | 181976 | claude-sonnet-5-5 | - |
| W2 | Build | 完了 | - | 2026-10-05 05:03:41 | 2026-10-05 05:17:06 | 13m25s | 20 | 20 | 0 | 40 | 32769 | 1627497 | 93130 | claude-sonnet-5-5 | - |
| C | Build | 完了 | - | 2026-10-05 05:13:19 | 2026-10-05 05:30:11 | 16m52s | 56 | 57 | 0 | 112 | 58153 | 7414564 | 163149 | claude-sonnet-5-5 | - |
| E | Build | 完了 | - | 2026-10-05 05:13:19 | 2026-10-05 05:40:45 | 27m26s | 57 | 64 | 3 | 114 | 73609 | 8744707 | 353080 | claude-sonnet-5-5 | - |
| D | Build | 完了 | - | 2026-10-05 05:30:11 | 2026-10-05 05:43:25 | 13m14s | 39 | 42 | 1 | 78 | 40481 | 4161518 | 232645 | claude-sonnet-5-5 | - |
| R | Build | 完了 | - | 2026-10-05 05:43:25 | 2026-10-05 05:51:15 | 7m50s | 55 | 57 | 1 | 110 | 57673 | 6749198 | 156174 | claude-sonnet-5-5 | - |
| T | Build | 完了 | - | 2026-10-05 05:51:15 | 2026-10-05 06:06:25 | 15m10s | 55 | 56 | 1 | 110 | 99232 | 7597036 | 191187 | claude-sonnet-5-5 | - |
| J | Build | 完了 | - | 2026-10-05 05:51:15 | 2026-10-05 06:09:17 | 18m02s | 50 | 53 | 1 | 100 | 51421 | 5900951 | 143850 | claude-sonnet-5-5 | - |
| S-finish | Build | 完了 | - | 2026-10-05 06:09:17 | 2026-10-05 06:15:41 | 6m24s | 44 | 47 | 3 | 88 | 15516 | 3304493 | 78662 | claude-sonnet-5-5 | - |
| test-1 | Integrate | 完了 | - | 2026-10-05 06:15:41 | 2026-10-05 06:18:59 | 3m18s | 9 | 9 | 0 | 18 | 3772 | 324740 | 50412 | claude-sonnet-5-5 | - |
| fix:crash:r1 | Integrate | 完了 | - | 2026-10-05 06:19:00 | 2026-10-05 06:21:13 | 2m13s | 16 | 27 | 0 | 32 | 12780 | 753755 | 44318 | claude-sonnet-5-5 | - |
| fix:isolation:r1 | Integrate | 完了 | - | 2026-10-05 06:19:00 | 2026-10-05 06:20:09 | 1m09s | 7 | 7 | 0 | 14 | 2352 | 225099 | 21588 | claude-sonnet-5-5 | - |
| fix:cargo:r1 | Integrate | 完了 | - | 2026-10-05 06:19:00 | 2026-10-05 06:19:59 | 0m59s | 5 | 6 | 0 | 10 | 1367 | 140189 | 14439 | claude-sonnet-5-5 | - |
| test-2 | Integrate | 完了 | - | 2026-10-05 06:21:13 | 2026-10-05 06:23:26 | 2m13s | 8 | 9 | 0 | 16 | 3367 | 297840 | 29159 | claude-sonnet-5-5 | - |
| fix:cargo:r2 | Integrate | 完了 | - | 2026-10-05 06:23:26 | 2026-10-05 06:23:59 | 0m33s | 4 | 5 | 0 | 8 | 1548 | 106971 | 12661 | claude-sonnet-5-5 | - |
| fix:isolation:r2 | Integrate | 完了 | - | 2026-10-05 06:23:26 | 2026-10-05 06:24:30 | 1m04s | 7 | 8 | 1 | 14 | 2480 | 207770 | 17926 | claude-sonnet-5-5 | - |
| test-3 | Integrate | 完了 | - | 2026-10-05 06:24:30 | 2026-10-05 06:27:17 | 2m47s | 11 | 12 | 0 | 22 | 3496 | 449453 | 32488 | claude-sonnet-5-5 | - |
| review:wal-recovery | Review | 完了 | - | 2026-10-05 06:27:17 | 2026-10-05 07:18:17 | 51m00s | 140 | 145 | 2 | 280 | 179291 | 42433467 | 852920 | claude-sonnet-5-5 | - |
| review:txn-concurrency | Review | 完了 | - | 2026-10-05 06:27:17 | 2026-10-05 06:40:50 | 13m33s | 47 | 49 | 0 | 94 | 93107 | 7834447 | 265661 | claude-sonnet-5-5 | - |
| review:crash-test-quality | Review | 完了 | - | 2026-10-05 06:27:17 | 2026-10-05 07:01:48 | 34m31s | 83 | 86 | 1 | 166 | 82617 | 13168481 | 221903 | claude-sonnet-5-5 | - |
| review:pg-compat | Review | 完了 | - | 2026-10-05 06:27:17 | 2026-10-05 06:43:33 | 16m16s | 92 | 94 | 24 | 184 | 66968 | 12778709 | 196044 | claude-sonnet-5-5 | - |
| fix:txn-concurrency | Review | 完了 | - | 2026-10-05 06:40:50 | 2026-10-05 06:46:33 | 5m43s | 33 | 34 | 2 | 66 | 16094 | 1808941 | 76832 | claude-sonnet-5-5 | - |
| fix:pg-compat | Review | 完了 | - | 2026-10-05 06:43:33 | 2026-10-05 06:57:51 | 14m18s | 51 | 62 | 0 | 102 | 30488 | 3847006 | 91344 | claude-sonnet-5-5 | - |
| fix:crash-test-quality | Review | 完了 | - | 2026-10-05 07:01:48 | 2026-10-05 07:24:07 | 22m19s | 83 | 90 | 2 | 166 | 61648 | 9648961 | 151852 | claude-sonnet-5-5 | - |
| fix:wal-recovery | Review | 完了 | - | 2026-10-05 07:18:17 | 2026-10-05 07:24:36 | 6m19s | 8 | 14 | 0 | 16 | 4644 | 260070 | 21879 | claude-sonnet-5-5 | - |
| finish | Finish | 進行中 | - | 2026-10-05 07:24:36 | 2026-10-05 07:32:04 | 7m28s | 25 | 27 | 0 | 50 | 10617 | 1257289 | 54502 | claude-sonnet-5-5 | - |

合計: ターン 1243、tool_use 1353、エラー 52、入力 2488、出力 1255497、cache読 168066340、cache作成 4359181

<!-- AUTO:agents END -->

強調する値（手書き。出所は上の表）:

- **K-m2-tests（wf_fabaf03d）が突出している。** 13m02s、assistant 52 メッセージ（トランスクリプトの assistant 行は 100 行。1 メッセージが content ブロックごとに複数行に分かれるため）、tool_use 54 回、出力 78,949 トークン（行を重複込みで足すと 81,918。ユーザー向けの「約 100 ターン・出力約 8.2 万」はこの行ベースの値）。cache_read 5,068,223 トークン。同 Workflow の出力トークン合計 97,440 の約 81%、cache_read 合計 6,946,341 の約 73% を 1 担当が使った。
- **fix:\* は軽量。** 10 担当とも 13〜31 秒、assistant 4〜6 メッセージ、tool_use 4〜7 回、出力 774〜2,006 トークン。10 担当の出力合計は 10,958 トークンで、K の約 14%。
- **A-foundation（wf_0cc0a7be）が M2 で最大。** 29m59s、tool_use 95、出力 111,384 トークン、cache_read 17,664,412 トークン。1 担当で wf_fabaf03d 全体（13 担当）の出力 97,440 を上回る。
- エラー数（`is_error:true` の tool_result）は、K が 3、fix:expressions が 1、A-foundation が 2 など、少数。何が失敗したかはこの表からは分からない（手戻りの原因は setbacks.md で扱う）。
- 全担当のモデルは同一（第 6 節）なので、モデル差によるコスト差は無い。

## 4. 初回通過率と過剰分割の判定

<!-- AUTO:firstpass BEGIN -->
最終集計: 2026-10-04 15:28:03 UTC。判定基準: result.summary が正規表現 `初回|1 ?周目|最初の(実行|から)|修正は不要|コード変更なし|コードの修正は不要|コードは変更していない|コード修正はしていません|修正はしていない|実装の修正は不要|実装側のコードは修正していない` に一致し、かつ tool_use が 10 回以下の担当に「first-pass」を付ける（自動判定。内容の妥当性は下の手書き節で判断する）。

| wf | 担当 | tool_use | first-pass | result のキー（出現順） | passed キー | スキーマ |
|---|---|---|---|---|---|---|
| `wf_0cc0a7be` | A-foundation | 95 | 対象外 | done, summary, issues | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | propose:process-retro | 6 | 対象外 | items | 無し | 別スキーマ（passed / done 無し） |
| `wf_dc96a8cc` | propose:decisions | 5 | 対象外 | items | 無し | 別スキーマ（passed / done 無し） |
| `wf_dc96a8cc` | propose:ai-ops | 8 | 対象外 | items | 無し | 別スキーマ（passed / done 無し） |
| `wf_dc96a8cc` | propose:future-reader | 5 | 対象外 | items | 無し | 別スキーマ（passed / done 無し） |
| `wf_dc96a8cc` | design | 1 | 対象外 | structure, automation, files | 無し | 別スキーマ（passed / done 無し） |
| `wf_dc96a8cc` | write:journal/README.md | 10 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:journal/timeline.md | 14 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:journal/decisions.md | 30 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:journal/setbacks.md | 27 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:journal/metrics.md | 13 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:journal/retrospective.md | 15 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:journal/snapshots.md | 13 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:tools/journal-lib.sh | 5 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:tools/journal-all.sh | 4 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:tools/journal-timeline.sh | 9 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:tools/journal-decisions.sh | 18 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:tools/journal-metrics.sh | 7 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_dc96a8cc` | write:tools/journal-verify.sh | 7 | 対象外 | done, summary | 無し | done 系（passed 無し） |
| `wf_fabaf03d` | stabilize | 11 | - | passed, summary | true | 揃い |
| `wf_fabaf03d` | K-m2-tests | 54 | - | passed, summary, remaining | true | 揃い |
| `wf_fabaf03d` | fix:ddl | 6 | first-pass | summary, passed | true | キー順が異なる（先頭 summary） |
| `wf_fabaf03d` | fix:insert | 4 | first-pass | passed, summary | true | 揃い |
| `wf_fabaf03d` | fix:constraints | 6 | first-pass | passed, summary | true | 揃い |
| `wf_fabaf03d` | fix:select | 5 | first-pass | passed, summary | true | 揃い |
| `wf_fabaf03d` | fix:expressions | 7 | first-pass | passed, summary | true | 揃い |
| `wf_fabaf03d` | fix:types | 6 | first-pass | passed, summary | true | 揃い |
| `wf_fabaf03d` | fix:functions | 4 | first-pass | passed, summary | true | 揃い |
| `wf_fabaf03d` | fix:txn | 5 | first-pass | passed, summary | true | 揃い |
| `wf_fabaf03d` | fix:session | 5 | first-pass | passed, summary | true | 揃い |
| `wf_fabaf03d` | fix:errors | 5 | first-pass | passed, summary | true | 揃い |
| `wf_fabaf03d` | integrate | 14 | - | passed, summary | true | 揃い |

注: first-pass 判定は passed キーを持つ result（slt 修正系）だけを対象にする。スキーマは script の RESULT 定義に従い、M1 完成は passed / summary / remaining、M2 実装は done / summary / issues。passed キーの有無はキー名で見ており、キーの出現順は無関係（fix:ddl は summary が先頭だが passed も持つ）。
<!-- AUTO:firstpass END -->

事実（出所: 上の自動表と `wf_fabaf03d/journal.jsonl` の result）:

- fix:\* 10 担当は全員 `passed:true` で、summary に「初回／1 周目／最初の実行で全件通過」「コードは変更していない」の趣旨が書かれている。`fix2:*`（失敗時の再実行）は 1 本も走っていない。初回通過率は 10/10。
- 各担当が実行したのは自領域のディレクトリだけ（例: `tests/run.sh --target yuzhu --port 55xx tests/slt/m1/<area>`）。ポートは領域ごとに 5500〜5509 を割り当てていた。summary に `cargo test` の記載は無く、`cargo fmt --check` と `clippy -D warnings` のみ。M1 全体（37 ファイル）を通した検証は、後段の integrate が行った（yuzhu 37/37、PG17 37/37）。したがって「各担当の通過」は領域内の検証で、領域をまたぐ相互作用は見ていない。
- 実際に直されたものは、stabilize が直したコンパイルエラーと clippy 9 件だけだった。slt を通すための実装修正は 0 件。
- result スキーマの不揃い: `wf_fabaf03d` で `passed` キーを持たない担当は無い。ただし fix:ddl だけ result のキー順が `summary, passed` で、他は `passed, summary`（出力順の違いのみで、`passed:true` は持つ）。この差は自動表の「キー順が異なる」行で確認できる。当初の想定「fix:ddl だけ passed キー無し」は、実データでは**当たらない**（キーはあり、順序が違うだけ）。`wf_0cc0a7be` の result は `done / summary / issues` で、`passed` を持たない（script の RESULT 定義どおり）。したがって Workflow 間で「成功」を表すキーが `passed` と `done` に分かれる。横断集計では両方を見る必要がある。

判定（2026-10-04 時点）:

- **分割して良かったか: 結果としては過剰分割。** 10 領域の並列は最長 31 秒で終わったが、実装の修正が 0 件で、価値のあった作業は stabilize の 1 分だけだった。10 担当の合計稼働は約 3 分（fix:\* の所要を足した値）、tool_use は 53 回。integrate が同じ確認（M1 全体の slt）をもう一度行っているので、検証の層としても重複している。「分割した効果で速くなった」とは言えない（1 担当が `tests/run.sh --target yuzhu` を 1 回流す場合の所要は、実測が無いので不明）。
- **次回は 1 担当にまとめるか: 条件付きでまとめる。** 先に slt 全体を 1 回流す担当（またはスクリプト）を置き、失敗した領域がある場合だけ、その領域に `fix:*` を起動する。今回の script は `pipeline` で fix2 の再実行を持っていたが、最初の起動を領域数ぶん無条件に行ったのが過剰だった。
- 保留: 「検証が浅い可能性」は今回は顕在化しなかった（integrate が 37/37 を確認）。ただし領域別の通過だけでは領域間の問題を見逃しうるので、まとめる場合も統合の全体実行は残す。
- 次の判断材料: M2 の Integrate（`tests/run.sh --target yuzhu` の m1 + m2 全体）で領域別に失敗が出たら、今回のような領域分割が効く可能性がある。その結果で再評価する。

## 5. ボトルネック分析

全体の壁時計は最長経路で決まる。合計稼働 / 壁時計（並列効率）が 1.29x（wf_fabaf03d）と低いのは、1 担当（K）が 93% を占めたからで、並列度 11 は見かけだけだった。

- **wf_fabaf03d**: K-m2-tests（13m02s）が最長で、他の 12 担当は 2m20s 以内に終わった。K を分割できたか: tests/slt/m2 は 6 ディレクトリ（catalog 10、ddl 2、dml 7、psql 1、txn 3、types 1 = 24 ファイル）と tests/restart の 6 シナリオで構成されているので、構造上は領域別に分けられそうに見える。ただし K は PG17（127.0.0.1:55432）に対して期待値を作っており、共有の 1 インスタンスを複数担当が同時に使うと衝突しうる（実行後に OID 16384 以上のテーブルが残っていないことを確認する手順が K の summary にある）。分割して速くなるかは**未検証**。依存順で待たされた点は、K が stabilize の完了を待って 1 分遅れて始まったこと（依存が無いのに待った）。
- **wf_0cc0a7be**: A（29m59s）が全員の前提で、完全に直列の区間。Parallel の 7 担当は同時開始なので、この段階の所要は最も遅い担当で決まる。そのあと G → I → J は依存順の直列で、合計は 3 担当の和。Integrate は最大 3 ラウンド、Review は 3 観点の pipeline のあとに最終確認が 1 つあるので、後半は直列の区間が長い。実績が出たら、各フェーズの最長経路を計算して追記する。
- **M2 の B〜J への示唆**:
  - Parallel の最長は、m2.md 第 8 節の工数の目安が最も大きい F（6）か C（4）になる可能性がある。これは見積もりからの予想で、実績ではない。完了後に実測と照合する。
  - G 以降は並列化の余地が小さい。ここを短くするなら、A が置く契約（トレイトとスタブ）の精度を上げて、G の結合作業を軽くするしかない。
  - 所要時間の見積もりの根拠として、今回の実測（A が 30 分、K が 13 分、fix:\* が 13〜31 秒）を m2.md の「日数」と対応づける材料にする。AI 1 本の「日数」と壁時計の対応は、まだ 2 点（A、K）しか無い。

## 6. 実行環境の前提

<!-- AUTO:env BEGIN -->
最終集計: 2026-10-04 15:28:03 UTC（agent-*.jsonl の message.model、agent-*.meta.json の agentType / requestShape）。

| wf | モデル | agentType | requestShape | 担当数 |
|---|---|---|---|---|
| `wf_0cc0a7be` | claude-sonnet-5-5 | workflow-subagent | foreground | 8 |
| `wf_dc96a8cc` | claude-sonnet-5-5 | workflow-subagent | foreground | 20 |
| `wf_fabaf03d` | claude-sonnet-5-5 | workflow-subagent | foreground | 13 |
<!-- AUTO:env END -->

手書き（2026-10-04）:

- モデル: 全担当 `claude-sonnet-5-5`（上の自動表。agent-*.jsonl の `message.model`）。
- agentType: `workflow-subagent`、requestShape: `foreground`（agent-*.meta.json）。Claude Code のバージョンは jsonl の `version` で 2.1.289。
- コンテナ: `HOME=/home/sandbox`（claude-sandbox）。トランスクリプトも `/home/sandbox/.claude/projects/...` にある。
- PostgreSQL: 17.11（K-m2-tests の summary）、`sandbox/pg.sh start` で `127.0.0.1:55432`。
- ビルドの分離: wf_fabaf03d の fix:\* は領域別ポート（5500〜5509）でサーバを分けた。wf_0cc0a7be は共通ルールで担当ごとの `CARGO_TARGET_DIR`（`$HOME/cargo-target-<担当名>`）を指定している。いずれも作業ツリーは共有（担当外ファイルは最小限の修正）。
- 変更があれば、ここに日付付きで追記する（モデルの変更は所見にも書く）。

## 7. 所見（Workflow 終了ごとに 3〜5 行。日付見出しで積む）

### 2026-10-04 wf_fabaf03d 終了（M1 完成 + M2 テスト作成）

- 壁時計 14m02s のうち、M1 を緑にする経路は 2m20s。残りの約 11m42s は K-m2-tests を待った時間で、M1 の結果が利用者に見えるのも遅れた。
- 次に同じ形（短い経路と長い独立作業の同居）を組むなら、Workflow を 2 本に分けるか、短い経路の結果を先に報告する。K は依存が無いので最初から起動する。
- fix:\* の 10 並列は全員が初回通過・修正 0 件で、効果は確認できなかった（第 4 節）。次は slt 全体を先に 1 回流し、失敗した領域だけ起動する。
- integrate が報告したコミット b8c7ea3 は、reflog 上で `commit (amend)` により 0771041（同じ件名、14:37:21 UTC）に置き換わっており、どのブランチからも辿れない。報告の食い違いは setbacks.md で扱う。

### 2026-10-04 wf_0cc0a7be 途中メモ（15:26 UTC）

- A-foundation が 29m59s かかり、Parallel の 7 担当の開始が 15:20:13 まで遅れた。完了後に Parallel 以降の実績と所見を追記する。

### （wf_0cc0a7be 完了後に追記）

### （wf_dc96a8cc 完了後に追記）
