# 再開用スナップショットと M2 進捗マップ

PROGRESS.md は最新 1 枚で上書きされるため、過去の状態が残らない。このファイルは「その時点の状態・次にやること・未解決・確認コマンド」を日付ごとに積み、M2 の進捗マップも持つ。

- 時刻はすべて JST（JST は括弧で補足）。新しいスナップショットは節 1 の先頭に積み、過去の記述は書き換えない。
- 自動更新されるのは節 2 の `AUTO:status` ブロックだけ（`tools/journal-snapshots.sh`）。それ以外は手書き。
- 数字には出所を添える。分からないことは「不明」と書く。

---

## 1. スナップショット（新しいものを上に積む）

### 2026-10-05 00:13 JST

| 項目 | 内容 | 出所 |
|---|---|---|
| HEAD / ブランチ | `88d1bc0`（2026-10-04 23:49:27 JST）/ `dev` | `git log`（節 2 参照） |
| M1 | 完了。slt `tests/slt/m1` は yuzhu・PostgreSQL 17 とも 37/37 通過。fmt / clippy / test 通過 | PROGRESS.md（2026-10-04 の記述）。この時刻に自分では再実行していない |
| M2 | 進行中。設計書 `spec/design/m2.md` は作成済み。実装 Workflow の最初の担当 A（基盤）が実行中。テストは slt 24 ファイル + 再起動テスト 6 シナリオを作成済み | `ls tests/slt/m2/*`（catalog 10、ddl 2、dml 7、psql 1、txn 3、types 1 = 24 ファイル）、`ls tests/restart`（6 ディレクトリ） |
| M3 以降 | 未着手（設計書 `m3.md` と調査のみ） | PROGRESS.md |
| 進行中の Workflow | `wf_0cc0a7be-171`（M2 実装。A-foundation が 23:50:14 JST に開始。この時刻で約 22 分経過、終了していない）。`wf_dc96a8cc-646`（journal 作成。本ファイルを含む）も実行中 | `subagents/workflows/wf_*/journal.jsonl`（`started` のみで `result` が無い） |
| 未コミット変更 | `git status --short` で 56 件（`git diff --stat` は追跡済み 24 ファイル、+1443 / -732 行）。主な場所は `yuzhu-core/src/storage`（11）、`executor`（9）、`catalog`（9）、`types`（4）、`analyzer`（3）。新規ディレクトリは `storage/vfs/`、`storage/buffer/`、`storage/heap/`、`txn/`、`util/`、`planner/`、`tools/`。M1 の `txn.rs`、`catalog/memory.rs`、`storage/memory.rs` は削除（ステージ済み）。`WORKLOG.md` も未追跡 | `git status --short` |
| コンパイル | 作業途中。`cargo check` が `yuzhu-server` で E0432 の 1 件で失敗。A の完了条件「crate 全体がコンパイルできる」は未達 | 翌00:12 JST ごろに `cargo check` を実行 |

**次にやること（3 件以内）**

1. A-foundation の完了を待ち、結果（スタブの網羅、コンパイル、CI）を確認する。通ったら B・C・E・F・H1・H2 を並列に起動する（m2.md §8 の進め方）。
2. Workflow が 1 本終わるたびに `tools/journal-all.sh` を再実行し、手書き欄（所見、KPT、手戻り）を足す。
3. 未承認のディスク形式 ★（M2-Q2、Q5、Q15）について、ユーザーの確認を取る。実装が進むほど変更コストが上がる。

**既知の未解決**

- 未承認の ★: 3 件。M2-Q2（タプルヘッダ 35 バイト）、M2-Q5（チェックサム常時有効）は QUESTIONS.md 35、38 行目。M2-Q15（制御ファイルの形式）は `spec/design/m2.md` 2141 行目。QUESTIONS.md に Q15 の ★ 行は無い（食い違い。要確認）。ユーザーの追認は記録上なし。
- yuzhu に対して未実行の M2 テスト: `tests/slt/m2` の 24 ファイルと `tests/restart` の 6 シナリオ。PostgreSQL 17 での通過確認の有無は PROGRESS.md に「残課題」とあり、完了の記録は見つからない（不明）。
- M2 の仮決め 21 件（M2-Q1〜Q21）は、ユーザーが確認した記録なし。
- PROGRESS.md は「実装は未着手」と書いており、実装 Workflow 開始後の実態（A が実行中）とずれている。更新は他の担当の範囲なので、ここでは触れない。

**確認コマンド**

```bash
# Rust（CI と同じ）
cd impl/rust && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

# slt（yuzhu）
tests/run.sh --target yuzhu

# slt（本物の PostgreSQL 17。sandbox 内）
sandbox/pg.sh start && tests/run.sh --target pg

# 状態の素材を更新
tools/journal-snapshots.sh
```

---

## 2. 直近の git 状態

使い方: スナップショットを書くとき、この内容を貼る素材にする。`tools/journal-snapshots.sh` が上書きする。

<!-- AUTO:status BEGIN -->
取得: 2026-10-07 21:45:29 JST / HEAD c3fe463 / ブランチ dev

git status --short: 235 件

```text
     34 impl/rust/crates/yuzhu-core/src/executor
     21 impl/rust/crates/yuzhu-core/src/analyzer
     16 impl/rust/crates/yuzhu-core/src/planner
     14 impl/rust/crates/yuzhu-core/src/catalog
     13 impl/rust/crates/yuzhu-core/src/types
     13 impl/rust/crates/yuzhu-core/src/sql
     12 impl/rust/crates/yuzhu-core/src/storage
      4 impl/rust/crates/yuzhu-core/src/wal
      2 impl/rust/crates/yuzhu-server/src/protocol
      1 tools/journal-workflows.sh
      1 tools/journal-verify.sh
      1 tools/journal-timeline.sh
      1 tools/journal-snapshots.sh
      1 tools/journal-readme.sh
      1 tools/journal-metrics.sh
```

```text
 M .github/workflows/nightly.yml
 M PROGRESS.md
 M QUESTIONS.md
 M WORKLOG.md
 M impl/rust/Cargo.lock
 M impl/rust/clippy.toml
 M impl/rust/crates/yuzhu-core/Cargo.toml
 M impl/rust/crates/yuzhu-core/src/analyzer/bound.rs
 M impl/rust/crates/yuzhu-core/src/analyzer/coerce.rs
 M impl/rust/crates/yuzhu-core/src/analyzer/ddl.rs
 M impl/rust/crates/yuzhu-core/src/analyzer/dml.rs
 M impl/rust/crates/yuzhu-core/src/analyzer/expr.rs
 M impl/rust/crates/yuzhu-core/src/analyzer/mod.rs
 M impl/rust/crates/yuzhu-core/src/analyzer/resolve.rs
 M impl/rust/crates/yuzhu-core/src/analyzer/scope.rs
 M impl/rust/crates/yuzhu-core/src/analyzer/select.rs
 M impl/rust/crates/yuzhu-core/src/analyzer/tests.rs
 M impl/rust/crates/yuzhu-core/src/catalog/builtin.rs
 M impl/rust/crates/yuzhu-core/src/catalog/cache.rs
 M impl/rust/crates/yuzhu-core/src/catalog/fake.rs
 M impl/rust/crates/yuzhu-core/src/catalog/mod.rs
 M impl/rust/crates/yuzhu-core/src/catalog/reader.rs
 M impl/rust/crates/yuzhu-core/src/catalog/rows.rs
 M impl/rust/crates/yuzhu-core/src/catalog/schema.rs
 M impl/rust/crates/yuzhu-core/src/catalog/store.rs
 M impl/rust/crates/yuzhu-core/src/debug_knobs.rs
 M impl/rust/crates/yuzhu-core/src/engine.rs
 M impl/rust/crates/yuzhu-core/src/error.rs
 M impl/rust/crates/yuzhu-core/src/executor/build.rs
 M impl/rust/crates/yuzhu-core/src/executor/eval.rs
 M impl/rust/crates/yuzhu-core/src/executor/mod.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/delete.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/distinct.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/filter.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/insert.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/limit.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/mod.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/project.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/result.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/seq_scan.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/sort.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/update.rs
 M impl/rust/crates/yuzhu-core/src/executor/nodes/values.rs
 M impl/rust/crates/yuzhu-core/src/lib.rs
 M impl/rust/crates/yuzhu-core/src/planner/mod.rs
 D impl/rust/crates/yuzhu-core/src/planner/plan.rs
 D impl/rust/crates/yuzhu-core/src/planner/simplify.rs
 M impl/rust/crates/yuzhu-core/src/recovery.rs
 M impl/rust/crates/yuzhu-core/src/session.rs
 M impl/rust/crates/yuzhu-core/src/settings.rs
 M impl/rust/crates/yuzhu-core/src/sql/ast.rs
 M impl/rust/crates/yuzhu-core/src/sql/parser/ddl.rs
 M impl/rust/crates/yuzhu-core/src/sql/parser/dml.rs
 M impl/rust/crates/yuzhu-core/src/sql/parser/expr.rs
 M impl/rust/crates/yuzhu-core/src/sql/parser/misc.rs
 M impl/rust/crates/yuzhu-core/src/sql/parser/mod.rs
 M impl/rust/crates/yuzhu-core/src/sql/parser/select.rs
 M impl/rust/crates/yuzhu-core/src/sql/parser/tests.rs
 M impl/rust/crates/yuzhu-core/src/storage/buffer/mod.rs
 M impl/rust/crates/yuzhu-core/src/storage/buffer/tests.rs
 M impl/rust/crates/yuzhu-core/src/storage/buffer/track.rs
 M impl/rust/crates/yuzhu-core/src/storage/heap/tuple.rs
 M impl/rust/crates/yuzhu-core/src/storage/heap/visibility.rs
 M impl/rust/crates/yuzhu-core/src/storage/heap_store.rs
 M impl/rust/crates/yuzhu-core/src/storage/mod.rs
 M impl/rust/crates/yuzhu-core/src/storage/page.rs
 M impl/rust/crates/yuzhu-core/src/storage/stack.rs
 M impl/rust/crates/yuzhu-core/src/testing.rs
 M impl/rust/crates/yuzhu-core/src/txn/manager.rs
 M impl/rust/crates/yuzhu-core/src/types/datum.rs
 M impl/rust/crates/yuzhu-core/src/types/funcs.rs
 M impl/rust/crates/yuzhu-core/src/types/io.rs
 M impl/rust/crates/yuzhu-core/src/types/mod.rs
 M impl/rust/crates/yuzhu-core/src/types/ops.rs
 M impl/rust/crates/yuzhu-core/src/types/regex.rs
 M impl/rust/crates/yuzhu-core/src/types/sys.rs
 M impl/rust/crates/yuzhu-core/src/wal/dump.rs
 M impl/rust/crates/yuzhu-core/src/wal/mod.rs
 M impl/rust/crates/yuzhu-core/src/wal/record.rs
 M impl/rust/crates/yuzhu-core/src/wal/writer.rs
 M impl/rust/crates/yuzhu-core/tests/crash_sim/invariants.rs
 M impl/rust/crates/yuzhu-core/tests/crash_sim/main.rs
 M impl/rust/crates/yuzhu-core/tests/crash_sim/model.rs
 M impl/rust/crates/yuzhu-core/tests/crash_sim/mutation.rs
 M impl/rust/crates/yuzhu-core/tests/crash_sim/workload.rs
 M impl/rust/crates/yuzhu-numeric/src/func.rs
 M impl/rust/crates/yuzhu-server/src/config.rs
 M impl/rust/crates/yuzhu-server/src/connection.rs
 M impl/rust/crates/yuzhu-server/src/protocol/codec.rs
 M impl/rust/crates/yuzhu-server/src/protocol/messages.rs
 M impl/rust/crates/yuzhu-server/tests/crash_kill9.rs
 M journal/README.md
 M journal/decisions.md
 M journal/metrics.md
 M journal/retrospective.md
 M journal/setbacks.md
 M journal/snapshots.md
 M journal/timeline.md
 M journal/workflows.md
 M spec/design/m4/06-btree.md
 M spec/design/m4/10-explain-copy-compat.md
 M spec/design/m4/99-questions.md
 M tests/README.md
 M tests/done-check.sh
 M tests/restart/m3/10-checksum-corrupt/01-prepare.after.sh
 M tests/restart/m3/10-checksum-corrupt/01-prepare.slt
 M tests/slt/m1/errors/guc_int_decimal_exponent.slt
 M tests/slt/m1/session/client_encoding_latin1.slt
 M tests/slt/m2/dml/update_errors.slt
 M tests/slt/m3/session/statement_timeout_fraction.slt
 M tests/slt/m4/KNOWN-DIFFS.md
 M tests/slt/m4/agg/basic.slt
 M tests/slt/m4/explain/analyze.slt
 M tests/slt/m4/explain/deparse_stored.slt
 M tests/slt/m4/explain/nodes.slt
 M tests/slt/m4/explain/verbose.slt
 M tests/slt/m4/seq/alter.slt
 M tests/tools/difffuzz/README.md
 M tests/tools/difffuzz/src/gen/dml.rs
 M tests/tools/difffuzz/src/gen/expr.rs
 M tests/tools/difffuzz/src/gen/expr_extra.rs
 M tests/tools/difffuzz/src/gen/mod.rs
 M tests/tools/difffuzz/src/gen/query.rs
 M tests/tools/difffuzz/src/gen/query_extra.rs
 M tests/tools/difffuzz/src/gen/query_r3.rs
 M tests/tools/difffuzz/src/gen/txn.rs
 M tests/tools/difffuzz/src/gen/txn_r3.rs
 M tests/tools/difffuzz/src/gen/types.rs
 M tests/tools/difffuzz/src/gen/values.rs
 M tests/tools/difffuzz/src/main.rs
 M tests/tools/difffuzz/src/session.rs
 M tests/tools/slttools/src/main.rs
 M tools/journal-all.sh
 M tools/journal-decisions.sh
 M tools/journal-lib.sh
 M tools/journal-metrics.sh
 M tools/journal-readme.sh
 M tools/journal-snapshots.sh
 M tools/journal-timeline.sh
 M tools/journal-verify.sh
 M tools/journal-workflows.sh
?? impl/rust/crates/yuzhu-core/core.2561288
?? impl/rust/crates/yuzhu-core/src/analyzer/agg.rs
?? impl/rust/crates/yuzhu-core/src/analyzer/cte.rs
?? impl/rust/crates/yuzhu-core/src/analyzer/ddl_constraint.rs
?? impl/rust/crates/yuzhu-core/src/analyzer/ddl_index.rs
?? impl/rust/crates/yuzhu-core/src/analyzer/from.rs
?? impl/rust/crates/yuzhu-core/src/analyzer/setop.rs
?? impl/rust/crates/yuzhu-core/src/analyzer/sublink.rs
?? impl/rust/crates/yuzhu-core/src/analyzer/tests_agg.rs
?? impl/rust/crates/yuzhu-core/src/analyzer/tests_from.rs
?? impl/rust/crates/yuzhu-core/src/analyzer/tests_sub.rs
?? impl/rust/crates/yuzhu-core/src/analyzer/tests_types.rs
?? impl/rust/crates/yuzhu-core/src/catalog/check.rs
?? impl/rust/crates/yuzhu-core/src/catalog/depend.rs
?? impl/rust/crates/yuzhu-core/src/catalog/names.rs
?? impl/rust/crates/yuzhu-core/src/catalog/naming.rs
?? impl/rust/crates/yuzhu-core/src/catalog/opclass.rs
?? impl/rust/crates/yuzhu-core/src/catalog/seq_params.rs
?? impl/rust/crates/yuzhu-core/src/copy/
?? impl/rust/crates/yuzhu-core/src/ddl/
?? impl/rust/crates/yuzhu-core/src/deparse/
?? impl/rust/crates/yuzhu-core/src/executor/agg.rs
?? impl/rust/crates/yuzhu-core/src/executor/dml.rs
?? impl/rust/crates/yuzhu-core/src/executor/instrument.rs
?? impl/rust/crates/yuzhu-core/src/executor/mem.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/aggregate.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/append.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/cte_scan.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/function_scan.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/group_aggregate.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/hash_aggregate.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/hash_join.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/hash_setop.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/index_scan.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/materialize.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/nested_loop.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/test_util.rs
?? impl/rust/crates/yuzhu-core/src/executor/nodes/unique.rs
?? impl/rust/crates/yuzhu-core/src/executor/seq.rs
?? impl/rust/crates/yuzhu-core/src/executor/subplan.rs
?? impl/rust/crates/yuzhu-core/src/explain/
?? impl/rust/crates/yuzhu-core/src/expr/
?? impl/rust/crates/yuzhu-core/src/planner/build.rs
?? impl/rust/crates/yuzhu-core/src/planner/check_const.rs
?? impl/rust/crates/yuzhu-core/src/planner/explain_tree.rs
?? impl/rust/crates/yuzhu-core/src/planner/index_select.rs
?? impl/rust/crates/yuzhu-core/src/planner/logical.rs
?? impl/rust/crates/yuzhu-core/src/planner/physical.rs
?? impl/rust/crates/yuzhu-core/src/planner/physicalize.rs
?? impl/rust/crates/yuzhu-core/src/planner/print.rs
?? impl/rust/crates/yuzhu-core/src/planner/rules/
?? impl/rust/crates/yuzhu-core/src/planner/size.rs
?? impl/rust/crates/yuzhu-core/src/planner/tests.rs
?? impl/rust/crates/yuzhu-core/src/planner/util.rs
?? impl/rust/crates/yuzhu-core/src/planner/validate.rs
?? impl/rust/crates/yuzhu-core/src/sql/parser/copy.rs
?? impl/rust/crates/yuzhu-core/src/sql/parser/ddl_index.rs
?? impl/rust/crates/yuzhu-core/src/sql/parser/seq.rs
?? impl/rust/crates/yuzhu-core/src/sql/parser/tests_query.rs
?? impl/rust/crates/yuzhu-core/src/sql/parser/tests_stmt.rs
?? impl/rust/crates/yuzhu-core/src/storage/btree/
?? impl/rust/crates/yuzhu-core/src/storage/index_store.rs
?? impl/rust/crates/yuzhu-core/src/storage/sequence.rs
?? impl/rust/crates/yuzhu-core/src/types/bpchar.rs
?? impl/rust/crates/yuzhu-core/src/types/cmp.rs
?? impl/rust/crates/yuzhu-core/src/types/datetime.rs
?? impl/rust/crates/yuzhu-core/src/types/hash.rs
?? impl/rust/crates/yuzhu-core/src/types/numeric.rs
?? impl/rust/crates/yuzhu-core/src/types/typmod.rs
?? impl/rust/crates/yuzhu-core/tests/plan_golden/
?? impl/rust/crates/yuzhu-core/tests/regex_corpus.rs
?? impl/rust/crates/yuzhu-server/tests/copy_protocol.rs
?? spec/design/m4-changes.md
?? tests/compat/
?? tests/data/
?? tests/gen/
?? tests/slt/m4/join/paren_subquery_leaf.slt
?? tests/slt/m4/plan_variants/
?? tests/slt/m4/seq/restart_rollback.slt
?? tests/slt/m4/seq/volatile_order.slt
?? tests/slt/m4/types/nullif_null_left_fold.slt
?? tests/slt/m4/types/review_sql_compat.slt
?? tests/tools/difffuzz/known-excludes.txt
?? tests/tools/difffuzz/src/compare.rs
?? tests/tools/difffuzz/src/gen/m4.rs
?? tests/tools/difffuzz/src/gen/m4_agg.rs
?? tests/tools/difffuzz/src/gen/m4_ddl.rs
?? tests/tools/difffuzz/src/gen/m4_index.rs
?? tests/tools/difffuzz/src/gen/m4_join.rs
?? tests/tools/difffuzz/src/gen/m4_setop.rs
?? tests/tools/difffuzz/src/gen/m4_subq.rs
?? tests/tools/difffuzz/src/gen/m4_tests.rs
?? tests/tools/slttools/src/planvar.rs
?? tools/gen_procs.sh
```

git log -n 5（JST）:

```text
c3fe463 2026-10-05 22:32:56 test: 差分ファジングで見つけた差分を修正し、回帰テストと使い方を追加
c9c1214 2026-10-05 18:11:37 test(m1,m3): 差分テストで見つけた挙動の回帰テストを追加し、PostgreSQL で通るよう修正
56fc5a0 2026-10-05 18:11:37 test(m4): M4 の slt・再起動・クラッシュ・isolation テストと検証ツールを追加
23fdc8b 2026-10-05 17:04:17 docs: M4・M5 の基本設計書を追加
828ae16 2026-10-05 16:32:25 feat(core): M3 を完了（WAL・チェックポイント・クラッシュリカバリ・トランザクション意味論）
```
<!-- AUTO:status END -->

---

## 3. M2 進捗マップ（手書き。Workflow ごとに更新）

状態の語彙: 未着手 / 実装済 / テスト通過 / レビュー済。ここでは「空のスタブだけ置かれた」ものを「未着手（スタブ）」と書く。
確認日時: 2026-10-05 00:12 JST。ファイルの有無は `git status --short`、行数は `wc -l`。担当割りは `spec/design/m2.md` §8（A〜K）。

| 設計の節 / 確認事項 | 担当 | 対応する新規ファイル（行数） | 状態 | 備考 |
|---|---|---|---|---|
| §4.1〜4.2、§4.4〜4.5 共通の型・トレイト、Cargo 依存（M2-Q6 signal-hook、proptest） | A 基盤 | `interrupt.rs`（37）、`util/sync.rs`（99）、`util/mod.rs`（4）、`storage/mod.rs`、`txn/mod.rs`（63）、`planner/plan.rs`（91）。`Cargo.toml` / `Cargo.lock` 変更 | 進行中（Workflow `wf_0cc0a7be-171`） | `cargo check` が `yuzhu-server` で失敗。M1 の `txn.rs`、`catalog/memory.rs`、`storage/memory.rs` は削除済み |
| §2 ★ファイルの空スタブ化 | A | 下記の各スタブ | 進行中 | 多くは数行〜数百行。どこまでが A の成果で、どこが他担当の範囲のスタブかは、A の結果が出るまで不明 |
| §4.3 VFS、障害注入（CLAUDE.md の規約） | B I/O 層 | `storage/vfs/mod.rs`（60）、`vfs/sim.rs`（138）、`vfs/local.rs`（56）、`util/crc32c.rs`（4） | 未着手（スタブ） | B はまだ起動していない（Workflow に B の `started` 行なし）。vfs の行数は A のスタブの可能性が高い（推測） |
| §4.3a、§3.1〜3.2 制御ファイル、データディレクトリ、M2-Q15★ | B | `control.rs`（97）、`datadir.rs`（82） | 未着手（スタブ） | 同上 |
| §3.3〜3.4 ページ、チェックサム、M2-Q5★ | C バッファ | `storage/page.rs`（109）、`storage/checksum.rs`（15）、`storage/smgr.rs`（151） | 未着手（スタブ） | C は最初に `storage/testing.rs` を置く必要あり（現在 4 行） |
| §4.3 バッファプール | C | `storage/buffer/mod.rs`（264）、`guard.rs`（62）、`track.rs`（5）、`clock.rs`（3）、`table.rs`（4）、`frame.rs`（4）、`storage/stack.rs`（35）、`storage/testing.rs`（4） | 未着手（スタブ） | `buffer/mod.rs` の 264 行は型・トレイトの宣言が中心と思われる（中身は未確認） |
| §3.5〜3.7 ヒープタプル、M2-Q2★、M2-Q11（TOAST なし） | D ヒープ | `storage/heap/{mod 9, tuple 57, scan 55, hio 4, visibility 4}`、`storage/heap_store.rs`（79） | 未着手（スタブ） | D は C の API と E の `Clog` が先に必要 |
| §3.8 コミットログ、M2-Q1 ROLLBACK 方式、M2-Q3 単一ライター | E トランザクション | `txn/clog.rs`（69）、`txn/manager.rs`（229） | 未着手（スタブ） | `manager.rs` が 229 行あり、A が型を置いた可能性。内容は未確認 |
| §4.8 チェックポイント、M2-Q4（正常停止のみ保証） | E | `checkpoint.rs`（68） | 未着手（スタブ） | `Cluster` は `checkpoint::run` を呼ぶだけの設計 |
| §6 システムカタログのテーブル化 | F カタログ | `catalog/{schema 47, rows 4, store 117, cache 53, reader 61}.rs`、`types/sys.rs`（71） | 未着手（スタブ） | `catalog/fake.rs`（130）は H1 向けのテスト用の偽物と思われる。担当割りに載っていない（不明） |
| M2-Q7（server_version 17.0）、PG17 のヘッダとの照合 | F / J | 該当ファイルなし | 未着手 | `catalog_columns.slt` を PG17 に流して確認する計画（m2.md 1834 行付近） |
| §5 エンジン、bootstrap（initdb） | G エンジン | `bootstrap.rs`（20）、`testing.rs`（5） | 未着手（スタブ） | B〜F に依存 |
| §4.7 Bound 型、SQL とアナライザ（UPDATE / DELETE） | H1 SQL | `analyzer/{bound, ddl, select}.rs` は変更中。新規ファイルなし | 未着手 | これらの変更が A によるものか不明 |
| §4.7 実行、`build()` の移設 | H2 実行 | `executor/build.rs`（70）、`executor/nodes/update.rs`（65）、`executor/nodes/delete.rs`（46） | 未着手（スタブ） | `build.rs` の移設は A の完了条件(2)。移設済みなら A の成果 |
| §5.4 CREATE / DROP の手順、`settings.rs` | I セッション | `session.rs` は変更中。新規なし | 未着手 | G と E に依存 |
| `shutdown.rs`、`bin/yuzhu-initdb.rs`、M2-Q6 SIGTERM | J サーバ | 新規なし（`yuzhu-server/Cargo.toml` のみ変更） | 未着手 | `yuzhu-server` が現在コンパイルできない |
| §7 テスト、再起動テスト、CI | K テスト | `tests/slt/m2/` 24 ファイル、`tests/restart/` 6 シナリオ（01-committed-dml、02-rollback、03-steal-rollback、04-create-rollback、05-drop-commit、06-uncommitted-at-stop） | 実装済（yuzhu では未実行） | コミット `88d1bc0`（23:49:27 JST）。PG17 での通過確認は不明。PROGRESS.md は dml 6 ファイルのみ記載で、実際は 24 で鮮度ずれ |
| 確認事項 M2-Q8（psql は `\l` まで）、M2-Q20（互換保証なし） | K / J | `tests/slt/m2/psql/l.slt` | 実装済（未実行） | |

### 確認事項・設計判断の状態（M2-Q1〜Q21、D1〜D15）

- M2-Q1〜Q21: 全件が仮決め。ユーザーの追認は記録なし。★は Q2、Q5、Q15 の 3 件。
- D1〜D15: 設計判断の台帳は `journal/decisions.md` を参照。本ファイルでは状態を持たない。
- 工数の目安（m2.md §8）: A 1.5、B 2.5、C 4、D 3、E 2.5、F 6、G 2、H1 2、H2 2、I 1.5、J 1.5、K 3 日（AI 1 本の日数換算。実績との比較は workflows.md で行う）。A の実績は、Workflow 完了後に追記する。

### 更新履歴（進捗マップ）

| 日時 (JST) | 更新内容 |
|---|---|
| 2026-10-05 00:12 | 初版。A が実行中。他担当は未起動。表中の行数は A が置いたスタブを含み、実装量ではない |
