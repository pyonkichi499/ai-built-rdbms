# 手戻り・つまずき台帳（setbacks）

うまくいかなかったこと（手戻り、誤報告、衝突、ドキュメントの鮮度ずれ、外れた前提）を、原因と対処つきで残す。

## 読み方と原則

- **エージェントの自己申告は鵜呑みにしない。** 報告（`result.summary`）は、git、ファイル、transcript の実状態と突き合わせてから記録する。この台帳の各エントリも、出所（コミット、ファイル、transcript の行）を明記する。
- **解釈（原因、再発防止、評価）は人間 / メインセッションが書く。** `AUTO:reports` ブロックは `tools/journal-verify.sh` が事実を並べるだけで、解釈はしない。手書き欄は自動更新で上書きされない。
- 時刻はすべて **JST（UTC+9）**。git の `+0900` と `+0000` の混在に注意（5e42f81 以前は +0900、以降は +0000。素の表示は JST に直して読む）。
- 数字には出所を示す。分からないことは「不明」と書く。
- 手書きの追記は日付見出しで積み、過去の記述は上書きしない。誤りがあれば訂正の追記をする。
- エントリの書式: 日付 JST / wf・担当・コミット / 何が起きたか / どう気づいたか / 原因 / 対処 / 再発防止 / 手戻りの規模。

出所の略記: `transcript` = `/home/sandbox/.claude/projects/-home-hiroshi-work-private-github-ai-built-rdbms/dd4de71f-ddb1-4c7d-acf0-ac44d49a764f/subagents/workflows/<wf>/`。`wf_fabaf03d` = M1 完成 + M2 テスト作成、`wf_0cc0a7be` = M2 実装（進行中）、`wf_dc96a8cc` = この journal を作る Workflow。

## 自動照合（報告ハッシュと実状態）

<!-- AUTO:reports BEGIN -->
生成: 2026-10-07 21:45:28 JST / 出所: 各 wf_*/journal.jsonl の type==result の result.summary、git、tests/slt/
対象 result 数: 294（出所: 各 journal.jsonl の type==result 行の合計）

### 報告ハッシュの存在確認（git cat-file -t）

| 状態 | ハッシュ | Workflow | agentId | label | git の応答 |
|---|---|---|---|---|---|
| 存在 | `610c121` | wf_0cc0a7be-171 | ac0a281336d65966c | A-foundation | commit（HEAD から到達可） |
| 存在 | `88d1bc0` | wf_0cc0a7be-171 | ac0a281336d65966c | A-foundation | commit（HEAD から到達可） |
| 存在 | `425ca04` | wf_0cc0a7be-171 | af4ffff0999eb323b | integrate-1 | commit（HEAD から到達可） |
| 存在 | `7daa421` | wf_0cc0a7be-171 | a410ccbd02f82ca59 | final | commit（HEAD から到達可） |
| 存在 | `ab02b68` | wf_0cc0a7be-171 | a410ccbd02f82ca59 | final | commit（HEAD から到達可） |
| **報告ハッシュ不在** | `1e200000` | wf_18ddb87d-acf | a1ed35c3719aa72f4 | N1b | fatal: Not a valid object name 1e200000 |
| **報告ハッシュ不在** | `ac44d49a764f` | wf_18ddb87d-acf | ac72f276f7a81b52b | L1-retry | fatal: Not a valid object name ac44d49a764f |
| **報告ハッシュ不在** | `dd4de71f` | wf_18ddb87d-acf | ac72f276f7a81b52b | L1-retry | fatal: Not a valid object name dd4de71f |
| 存在 | `c3fe463` | wf_383fcbbf-1a5 | ab206b119871cf5f0 | wrap | commit（HEAD から到達可） |
| 存在 | `11bcaeb` | wf_b10ff45b-956 | a8b4abfb9d633280e | A-foundation | commit（HEAD から到達可） |
| **報告ハッシュ不在** | `ac44d49a764f` | wf_b10ff45b-956 | ae9764ca0ddee5bb9 | R | fatal: Not a valid object name ac44d49a764f |
| **報告ハッシュ不在** | `dd4de71f` | wf_b10ff45b-956 | ae9764ca0ddee5bb9 | R | fatal: Not a valid object name dd4de71f |
| 存在 | `828ae16` | wf_b10ff45b-956 | a38d95b3b100aaafa | finish | commit（HEAD から到達可） |
| 存在 | `56fc5a0` | wf_b836e612-b0c | a82f0992afa15d0cb | K-verify | commit（HEAD から到達可） |
| 存在 | `c9c1214` | wf_b836e612-b0c | a82f0992afa15d0cb | K-verify | commit（HEAD から到達可） |
| **履歴から外れている** | `b8c7ea3` | wf_dc96a8cc-646 | a1c788b09d28a0ab3 | journal-verify.sh | commit（HEAD から到達不能。reflog にのみ残る可能性） |
| 存在 | `88d1bc0` | wf_dc96a8cc-646 | a43c34ddc3633abce | journal-metrics.sh | commit（HEAD から到達可） |
| **履歴から外れている** | `b8c7ea3` | wf_dc96a8cc-646 | a2efe58543601eb15 | retrospective.md | commit（HEAD から到達不能。reflog にのみ残る可能性） |
| 存在 | `5e42f81` | wf_dc96a8cc-646 | a68357732932a41f7 | timeline.md | commit（HEAD から到達可） |
| 存在 | `85a0e4e` | wf_dc96a8cc-646 | a68357732932a41f7 | timeline.md | commit（HEAD から到達可） |
| 存在 | `610c121` | wf_dc96a8cc-646 | a5d1a84027e3b9ed8 | metrics.md | commit（HEAD から到達可） |
| 存在 | `88d1bc0` | wf_dc96a8cc-646 | a5d1a84027e3b9ed8 | metrics.md | commit（HEAD から到達可） |
| 存在 | `610c121` | wf_dc96a8cc-646 | a2cdb2b2e599d292e | snapshots.md | commit（HEAD から到達可） |
| 存在 | `88d1bc0` | wf_dc96a8cc-646 | a2cdb2b2e599d292e | snapshots.md | commit（HEAD から到達可） |
| 存在 | `5e42f81` | wf_dc96a8cc-646 | a372c175258a5dcaa | journal-decisions.sh | commit（HEAD から到達可） |
| 存在 | `610c121` | wf_dc96a8cc-646 | a372c175258a5dcaa | journal-decisions.sh | commit（HEAD から到達可） |
| 存在 | `5e42f81` | wf_dc96a8cc-646 | a1dcd292ba618fcb7 | setbacks.md | commit（HEAD から到達可） |
| 存在 | `88d1bc0` | wf_dc96a8cc-646 | a1dcd292ba618fcb7 | setbacks.md | commit（HEAD から到達可） |
| **履歴から外れている** | `b8c7ea3` | wf_dc96a8cc-646 | a1dcd292ba618fcb7 | setbacks.md | commit（HEAD から到達不能。reflog にのみ残る可能性） |
| 存在 | `bc0cc78` | wf_dc96a8cc-646 | a1dcd292ba618fcb7 | setbacks.md | commit（HEAD から到達可） |
| **履歴から外れている** | `f2f3b32` | wf_dc96a8cc-646 | a1dcd292ba618fcb7 | setbacks.md | commit（HEAD から到達不能。reflog にのみ残る可能性） |
| 存在 | `610c121` | wf_dc96a8cc-646 | ac6f0415162fc36a4 | decisions.md | commit（HEAD から到達可） |
| **履歴から外れている** | `b8c7ea3` | wf_dc96a8cc-646 | ac6f0415162fc36a4 | decisions.md | commit（HEAD から到達不能。reflog にのみ残る可能性） |
| **履歴から外れている** | `b8c7ea3` | wf_dc96a8cc-646 | a195a4eef7f6dfc83 | workflows.md | commit（HEAD から到達不能。reflog にのみ残る可能性） |
| 存在 | `88d1bc0` | wf_dc96a8cc-646 | a0e11aa341516e958 | review | commit（HEAD から到達可） |
| **履歴から外れている** | `b8c7ea3` | wf_dc96a8cc-646 | a0e11aa341516e958 | review | commit（HEAD から到達不能。reflog にのみ残る可能性） |
| **履歴から外れている** | `b8c7ea3` | wf_fabaf03d-9d6 | aacb3834c4b5c0414 | integrate | commit（HEAD から到達不能。reflog にのみ残る可能性） |

件数: 到達可 24 / 履歴から外れている 8 / オブジェクト不在 5（出所: 上表）

### 「未実行」「未存在」を含む報告

- wf_18ddb87d-acf / ac4e3f44816dfb3a5 / T1: - tests/slt/m4/types の numeric 系 slt は、サーバをビルドしていないため未実行。
- wf_18ddb87d-acf / ae49511a74bfdc50f / T2-retry: T2 は一部完了です。コミットはしていません。担当ファイルの fmt と clippy は通り、単体テストも通りました（コピー上、yuzhu-core lib 815 件）。ただし now 系 4 関数と tz 依存の 8 関数、gen_bpchar_corpus の差分テストは未実装で、`tests/run.sh` の slt と PG 実機も未実行です。
- wf_18ddb87d-acf / ac139d724335a662a / P0-d: P0-d の実装は書き終えたが、テストを実行できていない。lib のビルドと lib の clippy では自分の範囲（analyzer、planner/legacy.rs、session.rs）に指摘は残っていない。`cargo test -p yuzhu-core --lib` は他担当のテストコードが通らず、いまも続いている（直近のログは ddl/ の警告まで。エラーが残っているかは未確認）。そのため analyzer/tests.rs、scope.rs と legac
- wf_18ddb87d-acf / ab9fa9753b2c1d074 / X1: - slt（tests/run.sh）は未実行。
- wf_18ddb87d-acf / ac89335c2efbbb287 / Q1b: - `session.rs` 経由の結合、`regclass` を使う経路（SERIAL の DEFAULT の評価）も未実行。
- wf_18ddb87d-acf / a1ed35c3719aa72f4 / N1b: - tests/slt/m4 は未実行。
- wf_18ddb87d-acf / aa5d41453c8f75d05 / P0-e: - `--restart` は tests/restart 直下と tests/restart/m3 が通った。tests/restart/m4 は対象外で未実行。
- wf_18ddb87d-acf / a5f2910146fffd52c / E1b: - 実行していないもの: 実サーバでの `EXPLAIN` / `EXPLAIN ANALYZE` と `tests/slt/m4/explain` は、L2 の `explain_tree` と Session の配線がないため未実行。
- wf_18ddb87d-acf / a9cc808d7ff4420fe / L1: - `cargo test`、clippy、`tests/run.sh --target yuzhu`（m1〜m3）は未実行。build.rs を書き直したので、planner/tests.rs の既存テストが通るか、physicalize の完成後に確認が必要。
- wf_18ddb87d-acf / a72ea1773407e9ca4 / S: - 未実行: tests/slt/m4、EXPLAIN の ANALYZE 実行、実サーバでの COPY（サーバ側が未対応）、PG との差分。
- wf_18ddb87d-acf / ac72f276f7a81b52b / L1-retry: L1 の実装は、R1〜R8、build の不変条件の検査、plan_golden の論理側まで書けた。ただし本物の作業ツリーでは cargo build が通らず、そのため `tests/run.sh --target yuzhu` の m1〜m3 は未実行で、完了条件は未達。コミットはしていない。
- wf_18ddb87d-acf / ac72f276f7a81b52b / L1-retry: - `tests/run.sh --target yuzhu` の m1〜m3 は未実行。physicalize が `explain_node` 未解決のため。
- wf_18ddb87d-acf / aa4b830f0f480a653 / R2a: - 共有ツリー本体では、`cargo fmt --check` と `cargo clippy -D warnings`、`cargo test` は未実行です。
- wf_18ddb87d-acf / aee96ac30de84d34e / Z: - nightly.yml: rust-cache の対象を difffuzz に変え、difffuzz ジョブ（日付シード、20,000 ケース、失敗の成果物保存）を追加した。この YAML は CI 上では未実行。
- wf_18ddb87d-acf / a8b4dd223c2c3d6f1 / test-1: M4 は未完了。実装担当 T2, L1, S, R2a, R2b の作業途中の状態で、パスしたのは fmt、crash_sim（層1）、isolation 13/13、PG 側 slt/restart/crash/isolation/EXPLAIN。失敗は下記。以下は実行ログ /tmp/cargo-m4-test/{test.log,slt.clean,restart.clean,crash.clean,iso.log} と /tmp/cargo-m4-test/dc/（do
- wf_18ddb87d-acf / a61ba9ccc95c60e5a / fix:isolation:r1: isolation は失敗していなかった。報告どおり修正は不要で、コードは変更していない。ポート 6317 の専用データディレクトリで yuzhu-server を起動して再実行すると、`yuzhu-isolation --variant yuzhu-m3 --blocking-detection timeout tests/isolation/specs` は 13/13 pass だった。終了させたのは自分が起動したサーバだけ。fmt と clippy は未実行（変更なし
- wf_18ddb87d-acf / a88e9c75e5ffeb20a / fix:done-check:r1:  - yuzhu 側の pgbench は未実行です。
- wf_18ddb87d-acf / a88e9c75e5ffeb20a / fix:done-check:r1:  - ビルドと clippy は未確認です。copy モジュールの単体テストは未実行です。
- wf_18ddb87d-acf / a513d4daeeda31f65 / fix:done-check:r3: 条件 6（差分ファジング）が 560 秒で打ち切られる原因は、実行時間と、固定名の衝突による並列化不能でした。`tests/done-check.sh --only 6 --quick` は約 3 分 4 秒で完走し PASS(QUICK) になりました。固定シード 1〜32（各 200 ケース）の差分は 0 件で、その後の long-QUICK（シード 1001、300 ケース）も ok でした。完了判定用の長時間版（1001〜1004 × 10,000 ケース）は未実行で
- wf_18ddb87d-acf / a3433970df7febeda / fix:ddl-catalog-crash: 2. lossy insert 変異が未実行(実在、修正済み): DebugKnobs に btree_lossy_insert_every を追加し、BtreeStore(with_knobs)が N 回に 1 回 insert を捨てるようにした。StorageStack::new が knobs を渡す。run_case の「startup」違反と detects_m4 の SKIPPED 握りつぶしを削除し、LossyIndexStore も削除した。変異の実行結果は
- wf_383fcbbf-1a5 / aa07beb16c1409cfa / fix:query:r2:3: fmt/clippy/全体 cargo test は最後まで通せていない。clippy が別担当の編集中の crates/yuzhu-numeric/src/func.rs（untracked）で失敗する。単文字変数名が 6 個あるという lint で、私の変更とは無関係。したがって yuzhu-core 以外の cargo test は未実行。そのファイルが直れば再実行できる。
- wf_383fcbbf-1a5 / af3d59794af1a23b1 / fix:txn:r2:4: PG17 の SHOW timezone は Etc/UTC（pg_settings の source は configuration file）。これは initdb がホストのシステムタイムゾーンを検出して postgresql.conf に書いた値で、環境依存。yuzhu の既定 UTC は不具合ではないため、直さず、.slt も追加していない（PG 上で安定して通らないため）。SET TIME ZONE 後の挙動は既存の m1/session/set_show.slt
- wf_383fcbbf-1a5 / a23d825308e82dc33 / fix:txn:r3:5: 修正せず、環境依存の差として報告のみ。PG 17.11 で再確認した結果、server_version_num=170011（Debian pgdg ビルド）、TimeZone=Etc/UTC。yuzhu は 170000（settings.rs:304）と UTC（settings.rs:333）の固定値。PG 側の値はマイナーバージョンと initdb が検出したホスト TZ に依存し、UTC と Etc/UTC は同じゾーン。.slt で揃えるなら比較時に正規化するか比
- wf_383fcbbf-1a5 / ab6629483a211411d / fix:txn:r3:2: DateStyle の衝突検出を実装した。PG 17 で 'ISO, SQL' が 22023 と DETAIL 'Conflicting "datestyle" specifications.' になることを再確認済み。修正は impl/rust/crates/yuzhu-core/src/settings.rs の normalize_datestyle。出力形式（ISO/SQL/Postgres/German）を複数指定した場合と、フィールド順（MDY/DMY/YMD）
- wf_5e84e5ec-172 / a28b7c143d8e3ec75 / K:crash: - yuzhu 側は未実行。M3 のバイナリがまだ無い。tests/yuzhu.sh の crash と after.sh フックは bash -n の構文確認のみ。
- wf_5e84e5ec-172 / ae4cb023a641ba574 / K:verify: - yuzhu 側は未実行。M3 のバイナリがまだ無い。run.sh、yuzhu.sh、pg.sh は bash -n の構文確認のみ。
- wf_b10ff45b-956 / a8b4abfb9d633280e / A-foundation: 担当 A の範囲を完了し、コミット 11bcaeb（日本語・署名なし・push なし）。`cd impl/rust && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` は通過（テスト 597 件成功・失敗 0、層 2 の足場 1 件は `#[ignore]`）。tests/run.sh や PG との slt は未実行。
- wf_b10ff45b-956 / a4a8082517fe75469 / E: - 単体テストのうち `--lib` の他モジュールは未実行。
- wf_b10ff45b-956 / a6c1abcff32cf975b / T: - 本物の作業ツリーでは cargo test --test crash_sim を未実行。
- wf_dc96a8cc-646 / a5661a45b8a3d5da7 / journal-all.sh: tools/journal-all.sh を作成しました（/home/hiroshi/work/private_github/ai-built-rdbms/tools/journal-all.sh、実行権限あり）。timeline、workflows、decisions、metrics、verify の順に tools/journal-<名前>.sh を bash で実行し、--dry-run と --run-tests を各スクリプトへ引き回します。1 つが失敗しても続行し
- wf_dc96a8cc-646 / a1c788b09d28a0ab3 / journal-verify.sh: 2. 「未実行」「未存在」を含む報告行を、Workflow、agentId、label 付きで並べる。
- wf_dc96a8cc-646 / a1c788b09d28a0ab3 / journal-verify.sh: - 「未実行」を含む報告は wf_fabaf03d の K-m2-tests の1件。yuzhu 実装に対して未実行とある。ほかに wf_fabaf03d の integrate と wf_dc96a8cc の journal-all.sh を作る担当（label は write:…/journal-all.sh）の2件の行も一致した。後者は、仕様の例示として「未実行」の語が文中に出ているだけの可能性がある（未確認）。
- wf_dc96a8cc-646 / a43c34ddc3633abce / journal-metrics.sh: - **重複の判定**: 同じ HEAD と同じ dirty ハッシュの行があれば追記しない。dirty の計算からは journal/ と `tools/journal-*.sh` を除く。`--run-tests` を付けたときだけは、同じ状態でも未実行の行が残っていれば追記する。
- wf_dc96a8cc-646 / a43c34ddc3633abce / journal-metrics.sh: - **テスト列**: `--run-tests` なしは「未実行(前回 …)」と書く。`--run-tests` ありでサーバが止まっていれば、その列は「未計測」にする。起動していれば `tests/run.sh --target …` を実行し、`[OK]` と `[FAILED]` のファイル数を数える。
- wf_dc96a8cc-646 / a43c34ddc3633abce / journal-metrics.sh:   - テスト列は未実行。
- wf_dc96a8cc-646 / a5d1a84027e3b9ed8 / metrics.md: - M2 の通過数は yuzhu が未実行、PG17 が slt 24/24 と restart OK です。この値は PROGRESS.md と指示の記述で、再計測していません。
- wf_fabaf03d-9d6 / aacb3834c4b5c0414 / integrate: tests/slt/m1 は yuzhu 37/37 通過・失敗0、PG17 でも 37/37 通過。cargo fmt --check / clippy -D warnings / cargo test も全て通過。PROGRESS.md を更新し、ローカルでコミット済み（push なし、AI署名なし）。コミットは b8c7ea3 と、その後の PROGRESS.md 修正 1 件。tests/slt/m2/dml の6ファイルも含めてコミットした。M2 テストは未実行。
- wf_fabaf03d-9d6 / ad7a8aff97e9b2bf8 / K-m2-tests: yuzhu の実装には触っていません。yuzhu に対しては未実行です。`yuzhu-server` や `yuzhu-initdb` がまだ無く、`tests/yuzhu.sh` は `bash -n` の構文確認と `status` だけ試しました。

該当行数: 38（出所: 上記）

### 実ファイルの状態（2026-10-07 21:45:28 JST 時点）

| 項目 | 値 | 出所 |
|---|---|---|
| tests/slt/m1 のファイル数 | 79 | `find tests/slt/m1 -type f` |
| tests/slt/m2 のファイル数 | 24 | `find tests/slt/m2 -type f` |
| tests/slt/m2/catalog | 10 | `find tests/slt/m2/catalog/ -type f` |
| tests/slt/m2/ddl | 2 | `find tests/slt/m2/ddl/ -type f` |
| tests/slt/m2/dml | 7 | `find tests/slt/m2/dml/ -type f` |
| tests/slt/m2/psql | 1 | `find tests/slt/m2/psql/ -type f` |
| tests/slt/m2/txn | 3 | `find tests/slt/m2/txn/ -type f` |
| tests/slt/m2/types | 1 | `find tests/slt/m2/types/ -type f` |
| tests/yuzhu.sh | あり | `test -e` |
| tests/pg.sh | あり | `test -e` |
| tests/run.sh | あり | `test -e` |
| sandbox/pg.sh | あり | `test -e` |
| impl/rust/crates/yuzhu-server | あり | `test -e` |
| impl/rust/crates/yuzhu-initdb | なし | `test -e` |
| HEAD | c3fe463 | `git rev-parse --short HEAD` |
| 作業ツリーの変更ファイル数 | 235 | `git status --short` |

※ 上記は事実の並置のみ。報告と実状態の食い違いの判断・原因・対処は手書き欄に書く。
<!-- AUTO:reports END -->

## 件数サマリ（2026-10-05 00:20 JST 時点、手書き）

| 区分 | 件数 | 出所 |
|---|---|---|
| 手戻り・つまずき（S-01〜S-02） | 2 | 下の記録 |
| 報告と実状態の食い違い（S-03〜S-04） | 2 | 下の記録 |
| 履歴・粒度の問題（S-05〜S-06） | 2 | 下の記録 |
| 並行編集の事故（S-07） | 1（M1）+ M2 は集計中 | 下の記録 |
| ドキュメント鮮度ずれ（S-08） | 1 | 下の記録 |
| 外れた前提（候補） | 1（未確定） | 下の記録 |

---

## 手戻り・つまずきの記録（日付別）

### 2026-10-04

#### S-01 stabilize の指示が指す壊れ箇所が違っていた（エージェントが自力で訂正）

- **日付 JST**: 2026-10-04 23:35:15〜23:36:15。出所: `wf_fabaf03d` の stabilize transcript `agent-a11eb309a4cd0237b.jsonl` の最初と最後の timestamp。
- **wf / 担当 / コミット**: wf_fabaf03d / stabilize / 修正は 0771041 に含まれる（`git show 0771041 --stat` に `planner/mod.rs` と `Cargo.toml`）。
- **何が起きたか**: 指示は「cargo test が yuzhu-core の lib test でコンパイルエラー（BoundInsert の初期化に coercions フィールドが無い。**analyzer/tests.rs** 付近 316 行目）」だった（transcript の最初の user メッセージ）。実際の壊れは `crates/yuzhu-core/src/planner/mod.rs` のテスト `insert_plan_and_execution` で、`BoundInsert` に `coercions: None` が無かった。`analyzer/tests.rs` は既に新契約に対応済みだった（stabilize の summary）。
- **どう気づいたか**: エージェントがコンパイルエラーの出力を読んで気づいた（summary の記述による）。
- **原因**: 指示を作った側が、エラー箇所のファイル名を確かめずに書いた（エラー出力の行番号 316 は合っていたが、ファイルを取り違えた、と推測されるが、実際の経緯は不明）。契約 `BoundInsert.coercions` の追加（m1-changes）にテスト側が追随していなかったことが、コンパイルエラーの直接の原因。
- **対処**: `planner/mod.rs` に `coercions: None` を追加。続いて clippy の `assert_is_empty`（pedantic）が 9 件（`executor/nodes/insert.rs`、`planner/mod.rs`、`sql/parser/tests.rs`）で出たため、`impl/rust/Cargo.toml` の `[workspace.lints.clippy]` に `assert_is_empty = "allow"` を足して対処した。lib テスト 151 件成功。
- **再発防止**: 不明（手書き予定）。案: 指示にはファイル名ではなく、エラー出力そのもの（`cargo test` の先頭 30 行）を渡す。
- **手戻りの規模**: 約 1 分、tool_use 11 回、is_error 0 回（transcript の集計）。指示の誤りが招いた無駄は小さい。ただし clippy の lint を workspace 全体で許可した判断は、規約（CLAUDE.md の clippy -D warnings）に対する例外であり、decisions.md 側の「規約への例外」にも載せる対象。

#### S-02 K-m2-tests の作業ミス（ツールエラー 3 回）と、担当外ファイルの変更

- **日付 JST**: 2026-10-04 23:36:33 / 23:42:49 / 23:44:42。出所: transcript `agent-ad7a8aff97e9b2bf8.jsonl` の `is_error:true` の tool_result（3 件）。
- **wf / 担当**: wf_fabaf03d / K-m2-tests（開始 23:36:15、終了 23:49:17、tool_use 54 回）。
- **何が起きたか**:
  1. 23:36:33 `sandbox/pg.sh` 関連のコマンドが exit 2（内容の詳細は未確認）。
  2. 23:42:49 `grep` が exit 2（yuzhu-server の config.rs を探した出力。ファイルが存在する場所の探索）。
  3. 23:44:42 `cd tests/restart` が「No such file or directory」で失敗し、続くヒアドキュメントの `cat > 01-committed-dml/...` が全て失敗（exit 127）。`tests/restart` を作る前に `cd` した。最終的には tests/restart に 16 ファイルが作られた（88d1bc0 の stat で確認）ので、作り直したと判断できる。
- **どう気づいたか**: ツールの exit code でエージェントが気づいた。
- **対処**: 作り直した（最終成果は 88d1bc0 に入った）。
- **担当外の変更**: summary に「`sandbox/pg.sh` に `restart` を追加した（担当外だが、コンテナ内で docker が使えないための最小限の追加）」とある。`git show 88d1bc0 --stat` でも `sandbox/pg.sh | 9 +-` を確認した。
- **未対応として自己申告された項目**: `.github/workflows/ci.yml` に M2 の yuzhu ジョブ、再起動テストのジョブ、psql 17 の `\l` ジョブを足していない（summary の「未対応」）。
- **原因 / 再発防止**: 不明（手書き予定）。
- **手戻りの規模**: エラー 3 回。うち 1 件は heredoc の連鎖失敗で、作り直しの時間は不明（transcript の時刻から 23:44:42 以降の数分と推測できるが、断定しない）。

### 報告と実状態の食い違い

#### S-03 integrate の報告コミット b8c7ea3 が、現在の履歴に無い（amend で置き換え）

- **日付 JST**: 2026-10-04 23:37:21〜23:37:35。出所: `git reflog`、integrate の transcript `agent-aacb3834c4b5c0414.jsonl`（開始 23:36:46、終了 23:37:35、tool_use 14 回）。
- **wf / 担当 / コミット**: wf_fabaf03d / integrate / b8c7ea3 → 0771041 → bc0cc78。
- **何が起きたか**: integrate の summary は「コミットは **b8c7ea3** と、その後の PROGRESS.md 修正 1 件」と報告した。`git log` に b8c7ea3 は無く、同じ作者時刻（23:37:21 JST）で同じタイトル「chore: M1 の結合確認と PROGRESS.md の更新」の **0771041** がある。
- **確認した事実**:
  - `git cat-file -t b8c7ea3` は `commit`（オブジェクトは残っている）。ただし HEAD からは到達できない。
  - `git reflog`（番号は新コミットのたびにずれるので省略。2026-10-05 00:30 JST 時点では HEAD@{4}→{3}→{2}）: `commit: chore: M1 の結合確認…`（b8c7ea3）→ `commit (amend): …`（0771041）→ `commit: docs: PROGRESS.md の M2 テスト作成状況を修正`（bc0cc78）。
  - つまり「履歴が書き直された可能性」ではなく、**integrate 自身が `git commit --amend --no-edit` で b8c7ea3 を 0771041 に置き換えた**ことが reflog と transcript で確認できる（transcript の Bash 呼び出しに `cat /tmp/tail.md >> PROGRESS.md; git add -A; git commit -q --amend --no-edit`）。
  - 報告した「その後の PROGRESS.md 修正」は bc0cc78（23:37:34）で、amend の約 13 秒後。
  - 同じことが 5e42f81 にも起きている。reflog に `commit: M1 実装・M2 設計までの成果物…`（f2f3b32）→ `commit (amend)`（5e42f81）。ただし、これを誰が（人間かエージェントか）amend したかは不明。
- **どう気づいたか**: journal-verify の報告ハッシュ照合と `git log` の突き合わせ。
- **原因**: 担当が amend したのに、報告には amend 前のハッシュを書いた。履歴の書き換えが、報告書式（コミットのハッシュ）を無効にした。
- **対処**: この台帳に記録した。コミットを辿るときは、報告のハッシュではなく、タイトルと時刻で引く（0771041）。b8c7ea3 は reflog が expire する（既定 90 日、到達不能は 30 日）まで残る。
- **再発防止**: 案: 統合担当には amend を禁止する。報告には最終の `git rev-parse --short HEAD` を書かせる。journal-verify が「履歴から外れたハッシュ」を検出する（実装済み）。
- **手戻りの規模**: 実害は小さい。追跡の手間だけ（0 回の作り直し）。なお、journal-verify に、amend 後も報告ハッシュが「存在」と判定されるため、到達可能性の判定を足した。

#### S-04 integrate が「M2 テストは未実行」としつつ、K が作成中のファイルまでコミットに含めた

- **日付 JST**: 2026-10-04 23:36:46〜23:37:35。出所: integrate の summary、`git show 0771041 --stat`、K-m2-tests の transcript。
- **wf / 担当 / コミット**: wf_fabaf03d / integrate と K-m2-tests / 0771041。
- **何が起きたか**: integrate の summary に「tests/slt/m2/dml の 6 ファイルも含めてコミットした。M2 テストは未実行。」とある。0771041 の stat は `tests/slt/m2/dml/` の 6 ファイル（delete_basic、update_basic、update_constraints、update_errors、update_expr、update_halloween）を含む。これらは並行して動いていた K-m2-tests（開始 23:36:15）が作成途中のもの。integrate は `git add -A` で作業ツリー全体を取り込んだ（transcript の Bash 呼び出しで確認）。
- **作成途中だった証拠**: コミット（23:37:21）の後に K が `update_errors.slt` を編集している（K の transcript で 23:37:36 の `sed -i`、23:37:41 の `perl -0pi`、23:37:47 の追記）。88d1bc0 の stat でも `tests/slt/m2/dml/update_errors.slt | 21 +-` と変更が入っている。つまり 0771041 に入った update_errors.slt は最終版ではない。
- **どう気づいたか**: コミットの stat と K の transcript の時刻の突き合わせ（今回の journal 作成時）。
- **原因**: 共有作業ツリーで、統合担当が `git add -A` を使った。「コミットするのは統合担当だけ」という規則は守られたが、他担当の作成途中の成果物を取り込む問題は、規則では防げない。
- **対処**: 88d1bc0（23:49:27）で K の最終版（m2 は 24 ファイル）をコミットした。
- **再発防止**: 案: 統合担当は担当範囲のパスを明示して `git add` する。または、並行する担当が全員終わるまでコミットを待つ。
- **手戻りの規模**: 履歴上に作成途中の版が残った（1 コミット）。作り直しは無し。

#### S-05 5e42f81 が「作業途中の部品を含む」一括コミット

- **日付 JST**: 2026-10-04 13:01:34。出所: `git show 5e42f81 --stat`（218 files changed, 173120 insertions）。
- **何が起きたか**: M1 の実装、M2 の設計、調査レポート、テスト、Dockerfile、CI などが 1 コミットになった。メッセージ本文には「途中で止めたもの（未完成）: spec/design/m3.md、yuzhu-numeric、yuzhu-datetime、tests/tools/difftest、tests/tools/isolation」とある。f2f3b32 を amend したもの（reflog）。
- **影響**: M1 の途中経過（どの部品がいつ・どの順で入ったか）が git から追えない。`git bisect`、`git blame` での時期の特定も、このコミット以前には使えない。時系列は Workflow の transcript と WORKLOG.md に頼る。
- **原因**: 不明（手書き予定）。事実としては、初回コミット（b9b8fc0、2026-10-04 01:02:06 JST）から 5e42f81 まで、約 12 時間分の作業が 1 コミットになった。
- **再発防止**: 案: 今後は Workflow 1 本ごと、または領域ごとにコミットする。journal の timeline.md を、git の代わりの経過記録として使う。
- **手戻りの規模**: 回数ではなく、情報の欠落。復元は不可能。

#### S-06 slt 領域別の fix:* 担当は、全員が「初回で通過」し、実質的な修正ゼロだった

- **日付 JST**: 2026-10-04 23:36:15〜23:36:46。出所: wf_fabaf03d の journal.jsonl の result.summary 10 件（fix:ddl、insert、constraints、select、expressions、types、functions、txn、session、errors）。
- **何が起きたか**: 10 件すべてが「初回 / 1 周目で通った」「コード変更なし」と報告している。所要は 13 秒〜31 秒、tool_use は 4〜7 回（transcript の集計。最長は fix:ddl の 31 秒）。stabilize の修正（S-01）が済んだ時点で、M1 の slt は既に通る状態だった。
- **評価**: 解釈は retrospective.md に置く。ここでは事実だけ記す。10 担当を並列に起動する分割は、結果的に過剰だった可能性がある（判断は人間）。
- **手戻りの規模**: 手戻りではなく、投入量に対する成果が小さかった事例。コスト（トークン）の数字は workflows.md を参照。

### 並行編集の衝突と回避策

#### S-07 共有作業ツリーでの並行編集

- **日付 JST**: 2026-10-04 23:36:15〜23:36:46（fix:* 10 担当の並列）、23:36:15〜23:49:17（K-m2-tests との重なり）、23:50:14 以降（wf_0cc0a7be、進行中）。
- **回避策（指示文にあるもの。integrate への指示から引用）**:
  - 他のエージェントが同じ作業ツリーを並行して編集している。担当外のファイルは必要最小限の修正にとどめる。
  - 編集は原子的（コンパイルが通る状態）にする。他人の編集中のエラーでビルドが壊れていたら、少し待って再試行する。
  - git commit / push はしない（統合担当だけがコミットする）。
  - 専用ポートを割り当てる（fix:* は 5500〜5509。ddl=5500、insert=5501、constraints=5502、select=5503、expressions=5504、types=5505、functions=5506、txn=5507、session=5508、errors=5509。出所: 各 summary のポート番号）。
- **M1（wf_fabaf03d）で確認できた事故**:
  - constraints 担当の summary に「起動したサーバ（pid 9379 とは別の、2 回目に起動したもの）は停止済み。ログは /tmp/claude-1000/s5502.log」とある。transcript では、最初に `nohup $B --port 5502 > $TMPDIR/s5502.log` で起動し、`$B` の解決に失敗した後、`/home/sandbox/cargo-target/release/yuzhu-server` を直接指定して `/tmp/claude-1000/s5502.log` に出す形で起動し直した。初回に起動した pid 9379 の停止を、summary は確認していない（「とは別の」と書いている）。**pid 9379 のプロセスが残ったかは、今回は確認できていない（不明）**。
  - expressions 担当で `Exit code 144` の tool_result が 1 回（23:36:28）。144 は 128+16 で、シグナル由来の終了と考えられるが、原因は未確認（不明）。
  - fix:* の後始末は、各担当の summary が「自分で起動したサーバは終了済み」と自己申告している。外部から確認した記録は無い。
  - 統合担当の `git add -A` による他担当成果物の取り込み（S-04）。
  - K-m2-tests が担当外の `sandbox/pg.sh` を変更（S-02）。
- **衝突の件数（Workflow ごと）**: 追記する欄。

| Workflow | 確認できた衝突・事故 | 出所 | 備考 |
|---|---|---|---|
| wf_fabaf03d（M1 完成 + M2 テスト） | 3 件（サーバ二重起動の疑い、`git add -A` の巻き込み、担当外ファイルの変更）。ビルド競合による失敗は 0 件（報告されていない） | S-02、S-04、本節 | 件数は報告と transcript から数えた。確認できていない事故は含まない |
| wf_0cc0a7be（M2 実装） | 進行中。集計は完了後 | - | A-foundation は 23:50:14 開始、最後の記録が 翌00:12:37。is_error は 2 件（どちらも `bash`（小文字）の呼び出しミス、23:55:17 と 翌00:12:34）。これは並行編集の衝突ではない |
| wf_dc96a8cc（この journal 作成） | 不明（完了後に追記） | - | journal/ と tools/ だけを書く規則 |

M2 は多数の担当が同じ yuzhu-core を触る（A → B/C/D/E/F/H1/H2 → G/I/J → 統合 → レビュー。出所: WORKLOG.md の全体の流れ）。**衝突は Workflow ごとにこの表へ追記する**。

### ドキュメント鮮度ずれの記録

#### S-08 PROGRESS.md の「M2 テストの作成状況」が実態とずれた（bc0cc78 で修正、その後も再びずれ）

- **日付 JST**: 2026-10-04 23:37:21（0771041）、23:37:34（bc0cc78）、23:49:27（88d1bc0）。出所: `git log -p PROGRESS.md`（相当）、`git show bc0cc78`、`git show HEAD:PROGRESS.md`。
- **何が起きたか**:
  1. 0771041 の PROGRESS.md は「`tests/slt/m2/` は `dml/update_basic.slt` と `dml/update_expr.slt` の 2 ファイルのみ」と書いた。同じコミットに入ったファイルは 6 つ（S-04）。**コミットの内容と、同じコミットの PROGRESS.md が食い違った**。
  2. 13 秒後の bc0cc78 で「`tests/slt/m2/dml/` に 6 ファイル（…）。`catalog`・`ddl`・`psql`・`txn`・`types` は空」に直した。この時点の記述は、コミットされたファイルとは一致する。ただし K の作成は続いており、完了時は 24 ファイル（K の summary と `find tests/slt/m2 -name '*.slt' | wc -l` = 24）。
  3. 88d1bc0（24 ファイルをコミット）では PROGRESS.md を更新しなかった。**HEAD の PROGRESS.md は、今も「dml/ に 6 ファイル」「catalog・ddl・psql・txn・types は空」と書いている**（`git show HEAD:PROGRESS.md | grep slt/m2` の 20 行目で確認）。実際は m2 が 24 ファイル（catalog 10、ddl 2、dml 7、psql 1、txn 3、types 1）で、`tests/restart` に 6 シナリオ 16 ファイルもある。「M2 のテストはまだ yuzhu に対して実行していない」だけは今も正しい。
- **どう気づいたか**: 6 と 24 の差を、PROGRESS.md と `find` の件数で突き合わせた。
- **原因**: PROGRESS.md は最新 1 枚で上書きされる。並行して動く担当の進捗を、統合担当の時点のスナップショットで書いたため、すぐに古くなった。
- **対処**: この台帳に記録した。PROGRESS.md 自体は journal の担当外なので編集していない（指示による）。
- **再発防止**: 案: PROGRESS.md の件数は手書きせず、`find` の結果を書く。journal の snapshots.md に日付つきで積む。
- **手戻りの規模**: 1 回の修正（bc0cc78）と、未修正の残り 1 件。

---

## エージェント報告と実状態の食い違い（一覧）

| # | 報告 | 実状態 | 出所 | 詳細 |
|---|---|---|---|---|
| 1 | integrate: 「コミットは b8c7ea3 と PROGRESS.md 修正 1 件」 | b8c7ea3 は amend で 0771041 に置換。HEAD から到達不能 | reflog、transcript | S-03 |
| 2 | integrate: 「M2 テストは未実行。dml の 6 ファイルも含めてコミット」 | K が作成中のファイルを `git add -A` で取り込んだ。update_errors.slt は後に変更された | 0771041、88d1bc0 | S-04 |
| 3 | 0771041 の PROGRESS.md: 「m2 は 2 ファイルのみ」 | 同じコミットに 6 ファイル | git show | S-08 |
| 4 | stabilize への指示: 「analyzer/tests.rs」 | 実際は planner/mod.rs | stabilize summary | S-01 |
| 5 | 各 fix:*: 「自分で起動したサーバは終了済み」 | 外部からの確認は無し。constraints は 2 回起動 | summary、transcript | S-07 |

## 外れた前提（decisions.md の「未検証」との突き合わせ）

- 現時点で数えた「未検証」の出現行数（`grep -c '未検証'`）: spec/design/m2.md が 35、spec/research の最大は pg-compat-tools.md の 25、m5-concurrency.md の 21。出所: 今回のコマンド結果。推移は decisions.md の `AUTO:unverified` を参照。
- **候補 1（未確定）: マップ対象カタログの範囲**。K-m2-tests の summary は、PostgreSQL 17.11 の実機で `pg_tablespace` も `relfilenode = 0` だったこと、共有カタログ（`pg_authid`、`pg_database`、`pg_tablespace`）の `reltablespace` が 1664 であることを報告した。一方 `spec/design/m2.md` 1836 行目は、マップされるカタログを「pg_class、pg_attribute、pg_type、pg_proc、pg_database、pg_authid」の 6 つとし、`pg_tablespace` を挙げていない（grep で確認）。`catalog/catalog_rows.slt` は PG の値をそのまま期待値にするため、実装が設計書どおりだとテストが落ちる。**これが外れた前提かどうかは、F 担当の実装結果を待たないと判断できない（不明）**。m2.md が修正されたかも未確認。
- 同じ summary が挙げた、yuzhu 側の確認事項（外れる可能性のある前提）: `dml/system_columns.slt` の ctid の期待値（`(0,1)` から順、更新後に `(0,6)`）、`--shared-buffers 1MB` が受理されること、`yuzhu-initdb` / `yuzhu-server` のコマンドライン。現状 `impl/rust/crates/yuzhu-initdb` は存在しない（`test -e` の結果、上の自動照合の表）。
- 他の「未検証」（m2.md 93 行目の pg_type の分類値は「記憶に基づく」、118 行目の OID など）の突き合わせは、M2 の実装とテストが PG と照合した後に行う。

## M2 の Integrate / Review の指摘（完了後に追記する枠）

M2 実装 Workflow（wf_0cc0a7be、進行中）が完了した後に、次の形で追記する。現時点で記録するものは無い。

| 指摘 # | 件数 | 元の担当 | 契約（spec/design/m2.md 第 4 節）のどこが曖昧だったか | 修正の規模 | 出所 |
|---|---|---|---|---|---|
| （未記入） | | | | | |

- 集計の観点: Integrate の指摘件数、Review の指摘件数、そのうち契約の曖昧さに由来するもの、担当境界（どの担当どうしのインターフェース）に由来するもの。
- データが揃うのは Review の後なので、境界ごとの詳細分析は今は行わない（枠のみ）。

## 追記ログ（日付見出しで積む。過去の記述は上書きしない）

### 2026-10-04（初回作成、翌00:20 JST 頃）

- S-01〜S-08 と、外れた前提の候補 1 を記録した。
- b8c7ea3 は「存在しない」ではなく、「存在するが HEAD から到達できない（amend で置換）」であることを確認し、S-03 に訂正して記録した。作業指示の前提（現在の git log に存在しない）は正しいが、「履歴が書き直された可能性」は reflog で確認済みの事実になった。
- PROGRESS.md の「6 ファイル」は、bc0cc78 の時点では正しく、その後 24 ファイルになってもその記述が残っている点が鮮度ずれの実態である（S-08）。
