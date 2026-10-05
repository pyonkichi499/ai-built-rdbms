# yuzhu M4 設計 11: テスト全体・実装計画・全章の集約（11-tests-plan）

M4 の担当 **K**（共有テスト）、**R2**（クラッシュ試験の追加）、**Z**（差分ランダムテスト）の設計書であり、同時に M4 の**全章（01〜10）を突き合わせて集約する章**です。`00-contracts.md`（以下「00」）の署名と名前に従い、00 から変える必要があったものは末尾（§9）と §7.4 の表に集約しました。書き方は `00 §1.2` に従います。

- 前提（必読）: `00-contracts.md`、`01〜10` 章（この章は**それらを読んだうえで最後に**書いた）、`spec/design/m3.md` §7〜§10（書式の手本と、M3 のテスト基盤）、`tests/README.md`、`tests/run.sh`、`QUESTIONS.md`、`PROGRESS.md`
- 調査（根拠）: `spec/research/m4-query.md` §9・§10、`research-slt.md`、`pg-compat-tools.md` §6、`m4-btree.md` §9.2
- 正解の基準は PostgreSQL 17。実機（`sandbox/pg.sh start`、`127.0.0.1:55432`）で確かめたものは「（実測）」、確かめていないものは「（未検証）」と書く。
- **この章の執筆時点で `01-scope-decisions.md` は読めなかった**（他章と並行して書かれていて未完だった）。M4 の完了条件と範囲は 00 §2・§3・§17 と各章の「範囲」から組み立てた（§1.3）。01 の内容がこの章と食い違う場合は、範囲と完了条件は 01 が、テストの形はこの章が優先する。
- 他の章への参照は、章のファイル名と概念名で書く（`00 §1.2` の 6）。

---

## 1. 範囲

### 1.1 この章が決めるもの

| 分類 | 内容 |
|---|---|
| テスト全体 | 層と置き場所、`tests/slt/m4/` のディレクトリとファイルの全一覧（各章のテスト節の合成）、接頭辞の表、`onlyif` / `skipif` の規約、slt の lint、`plan_variants` の生成、`tests/restart/m4` と `tests/run.sh` の拡張、`tests/compat`（psql・pgbench・COPY）、isolation の追加、クラッシュ試験 層 1 のワークロード 6〜8・不変条件 I13〜I16・変異テスト、差分ランダムテスト、PostgreSQL 回帰テストの取り込み、Rust の単体テストの一覧（章ごと）、CI のジョブ、環境要件（tzdata ほか） |
| 実装の分担と工数 | 00 §17 の担当表を各章の分担表と突き合わせた**確定版**、ファイル所有の衝突の確認と共有ファイルの区画割り、依存の順序・並列度・最長経路、フェーズ分け |
| 未検証の点 | 全章の「未検証の点」を重複を除いて集約し、実装前に確かめる順に並べたもの |
| 確認事項 | 全章の確認事項を `M4-Q1` からの通し番号に振り直した集約（仮決め・理由・変えたい場合の影響）。`QUESTIONS.md` への転記の依頼 |
| 整合性レビュー | 章をまたぐ矛盾（署名・名前・OID・SQLSTATE・ファイル所有・担当不在）の洗い出しと決定、各章の「00 への変更提案」の採否表 |
| M5 以降の宿題 | 00 §19 と各章から集めた一覧 |

### 1.2 この章が決めないもの

- 各機能の仕様と、機能ごとのテストの**中身**（slt の個々の文と期待値）: 各章の「テスト」節。この章は**一覧と形式**を決め、中身は各章に従う。
- M4 の範囲そのもの: `01-scope-decisions.md` と 00 §3。
- `tests/` の既存の仕組み（sqllogictest-rs 0.29.1、`tests/run.sh`、`tests/pg.sh`、`tests/yuzhu.sh`、isolation ランナー、`tests/tools/difftest`）の作り直し。**拡張だけ**を決める。
- ディスク上の形式・Rust の型とトレイトの契約: この章にディスク形式（★）はない。型の契約はテスト用の部品（`LossyIndexStore`、不変条件の関数、`slttools`・`difftest` の CLI。§3.6.4、§3.7.3、§3.2.8）と、`tests/run.sh` / `restart` のファイル形式（§3.4.1）だけ。全章の ★ は §6.1 に集約した。

### 1.3 完了の判定（M4 を終えたと言える条件）

00 §2（D-24、D-25）と各章の「範囲」から、**テストで機械的に判定できる形**にした。1〜9 がすべて通ったときに M4 を完了とする（`PROGRESS.md` に結果を書く）。

| # | 条件 | 判定するもの | CI のジョブ（§3.10） |
|---|---|---|---|
| 1 | `cd impl/rust && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` が通る。`plan_golden`、proptest（256 ケース）、クラッシュ試験 層 1（固定シード）を含む | CLAUDE.md の確認コマンド | `rust`、`crash-sim` |
| 2 | `tests/run.sh --target pg` が `tests/slt`（m1〜m4）で通る（**期待値が PostgreSQL 17 で正しい**）。`tests/run.sh --target yuzhu` も通る（`slt-yuzhu` の `continue-on-error` を外す） | 共有 slt（約 120 ファイルを追加） | `slt-pg`、`slt-yuzhu` |
| 3 | `tests/run.sh --target {pg,yuzhu} --restart` と `--crash` が、`tests/restart` と `tests/restart/m3` と `tests/restart/m4` のすべてのシナリオで通る | 再起動・クラッシュをまたぐ永続性（インデックス・シーケンス・DDL） | `restart-pg`、`restart-yuzhu` |
| 4 | isolation の全 spec（M3 の 6 本 + M4 の 2 本）が pg と yuzhu で通る | MVCC と索引・シーケンスの可視性 | `isolation` |
| 5 | `tests/compat/run.sh --target {pg,yuzhu}` が通る: psql 17 の `\dt` `\dn` `\di` `\l` の出力が PostgreSQL と一致、`pgbench -i`（既定と `-I dtGvp`）と `pgbench -c 4 -T 30 -M simple` が完走し不変条件が成り立つ、COPY の psql スクリプトの出力が一致（00 D-24、D-25） | 周辺ツール互換 | `compat-pg`、`compat-yuzhu` |
| 6 | 差分ランダムテスト（§3.7）の**固定シード集合**で、yuzhu 対 PostgreSQL の差分が 0 件。差分が出たものは原因を `tests/tools/difftest/KNOWN.md` に書いた範囲だけを許す。夜間の長時間実行が連続 7 回で未解決の差分 0 件 | 結果の一致 | `fuzz-pr`、`nightly` |
| 7 | クラッシュ試験 層 1 のワークロード 1〜8 が全クラッシュ点で I1〜I16 を満たし、変異テストが**すべて検出される**（§3.6） | 耐久性と構造の原子性 | `crash-sim` |
| 8 | `EXPLAIN (COSTS OFF)` の書式の一致を確かめる slt（`explain/format.slt`、`explain/deparse_*.slt`）と `plan_variants`（`enable_*` を変えて同じ結果）が pg と yuzhu で通る | プランの書式と最適化の安全性 | `slt-pg`、`slt-yuzhu` |
| 9 | `QUESTIONS.md` に §6 の確認事項が転記され、`PROGRESS.md` が M4 完了の状態になっている | 運用 | — |

参考（合否の条件にしない）: pgbench の TPS・レイテンシ（`PROGRESS.md` に記録。`06-btree.md` §7.15）、PostgreSQL 回帰テストの取り込み結果（§3.8。差分の一覧を作るのが目的）。

---

## 2. 決定（この章で追加で決めたこと）

選択肢と理由を表にする。`D11-n` はこの章の決定番号。他章の食い違いの決定は §7.1 の `C-n`。

| # | 論点 | 選択肢 | 決定 | 理由 |
|---|---|---|---|---|
| D11-1 | 章をまたぐ食い違いの決め方 | (A) 常に 00 が正／(B) 後から書かれた章が正／(C) 次の優先順位 | **(C)**: (1) 観測できる挙動は PostgreSQL 17 の実機が正、(2) 実装の持ち主（ファイルを編集する担当）の章が、その実装の名前・署名・内部の型の正（例: `executor/subplan.rs` は 05）、(3) ディスク形式は定義の持ち主の章（06 / 07 / 08 / 09）、(4) 横断する契約（式の木、Bound、論理・物理プラン）は 00 と 02、(5) どの章も書いていないものは 00 | 00 は M4 の設計の**前に**書かれ、各章の実機調査で変更が必要になった。実装者が迷わないように決め方を固定する |
| D11-2 | slt のディレクトリ | 00 §18 の 15 ディレクトリのまま／足す | **`ddl/`、`mem/`、`z_final/` の 3 つを足す**。`index/` にあった TRUNCATE・VACUUM・reloptions・読み取り専用 DDL は `ddl/` へ。`catalog/consistency.slt` は `z_final/` へ（全スイートの最後に流すため。`tests/run.sh` はパスを C ロケールの昇順に流す） | 07 が「`consistency.slt` を全スイートの最後に」と依頼している。メモリ上限のテストは `onlyif yuzhu` 専用のため別ディレクトリにして見落とさない |
| D11-3 | PostgreSQL と yuzhu が違うものの書き方 | 自由／規約を決める | **規約（§3.2.1 の 6）**: `skipif yuzhu` は「M5 以降に作る」または「PostgreSQL にしかない機能」だけ、直前に `# KNOWN-DIFF: <ID> <理由>` のコメントを必須。`onlyif yuzhu` は yuzhu 専用の振る舞い（EXPLAIN の独自の期待値、メモリ上限、0A000）。両方を**対で**書く（PG 側 `skipif yuzhu`、yuzhu 側 `onlyif yuzhu`）。`KNOWN-DIFF` の一覧を lint が集計する | M1 の規約（`tests/README.md`）は「`skipif yuzhu` は一時回避」だけで、M4 は既知の差分（`interval`、`FULL JOIN ... ON true`、`LATERAL` など）が多い。入れた後に外し忘れないようにする |
| D11-4 | `plan_variants` の作り方 | 手で書く／テンプレートから生成してコミット／実行時に生成 | **テンプレート（`tests/gen/plan_variants/*.tpl`）から 2 段階で生成し、生成物をコミット**（§3.3）。期待値は PostgreSQL に `--override` で作る（yuzhu に使わない）。生成物が最新かを CI が確かめる | 同じ問い合わせ群を 10 通りの設定で流すので手書きは保守できない。期待値は 1 回だけ PostgreSQL で作り、全変種に同じ値を入れることで「設定を変えても結果が同じ」を検査できる |
| D11-5 | 再起動・クラッシュのシナリオの置き場所と指定 | `tests/restart/m4/` に混ぜる／`mode` ファイルで指定 | **`tests/restart/m4/<シナリオ>/` に置き、シナリオに `mode`（`restart` / `crash` / `both`）、フェーズに `NN-<名前>.mode`（その後の停止の方法）を置く**。`tests/run.sh --restart` は `restart` と `both`、`--crash` は `crash` と `both` を流す（§3.4） | 08 のシーケンスは正常停止とクラッシュで期待値が違う。08 の `seq-mixed-restart` は 1 つのシナリオの中でフェーズごとに停止の方法を変える |
| D11-6 | 差分ランダムテストの実体 | 00 のとおり新しい `yuzhu-fuzz-sql`（ワークスペースの bin）／**既存の `tests/tools/difftest` を M4 向けに仕上げて使う** | **後者**。00 の `yuzhu-fuzz-sql` は**作らない**（§7.1 の C-5）。00 の記述はすべて `tests/tools/difftest` と読み替える | `tests/tools/difftest`（約 4,300 行。独立した Cargo プロジェクト。TLP・最小化・再現手順・M4 レベルの JOIN / 集約 / 副問い合わせを持つ）がすでにある。言語非依存のテストツールは `tests/` に置く（CLAUDE.md）。00 の「依存は yuzhu-core のみ」は差分テストに不要。**ただし現在コミットされている状態はビルドが通らない**（`HANDOFF.md` の作業途中。Z の最初の作業） |
| D11-7 | PostgreSQL 回帰テストの取り込み | `tests/slt` に取り込む／別の場所と別のジョブ | **`tests/imported/pg_regress/`（`tests/slt` の外）に、スクリプトが生成した slt を置き、夜間ジョブだけで流す**（§3.8） | 数千文あり、`tests/run.sh` の既定（`tests/slt` 全体）に入れると毎回の CI が遅くなる。取り込みは「PostgreSQL と yuzhu の結果が一致した文だけ」を機械的に選ぶので手で保守しない |
| D11-8 | クラッシュ試験のワークロード | 00 のとおり 6（インデックス）と 7（シーケンス）／DDL を足す | **8 を足す**（DDL の混在。07 §8.5 の依頼）。不変条件 **I16**（カタログの整合。07 §8.3 の `check_catalog`）を足す。ワークロード 6 は `shared_buffers = 24`（06 §7.11） | 07 がワークロードと不変条件を依頼している。00 §18 の「6 と 7」は下限 |
| D11-9 | CI のゲート | 全ジョブを必須にする／yuzhu 側は当面 `continue-on-error` | **pg 側（期待値の検証）は常に必須**。yuzhu 側は M4 の**フェーズ 4 の開始（§4.4）から必須**にし、それまでは `continue-on-error: true` のまま（M3 の運用を引き継ぐ） | 実装が揃う前に yuzhu 側を必須にすると、毎回の CI が赤くなり意味を失う。期待値が PostgreSQL で正しいことは実装に依存しない |
| D11-10 | 環境 | 各担当が整える／この章で要件を決める | **要件を決める**（§3.11）: tzdata を `Dockerfile`（実行用）・`sandbox/Dockerfile`・CI に**明示的に**入れる、psql / pgbench は 17 系（PGDG の `postgresql-client-17`）、`TimeZone` と `DateStyle` を slt の先頭で明示する、PostgreSQL は `--locale=C --encoding=UTF8`（M1 から） | 09 §5.3 の依頼。実行用の `Dockerfile`（`debian:bookworm-slim`）は tzdata を一切インストールしておらず、ベースイメージに入っているかは保証がない（Debian の `tzdata` の優先度に依存。未検証）。入っていないと `Asia/Tokyo` などが `22023 time zone not recognized` になり、`SET TimeZone` と日時の入出力が UTC 以外で動かない。`sandbox/Dockerfile` は postgresql-17 の依存としてたまたま入る（09 §5.3） |
| D11-11 | K の分割 | 00 のとおり 1 担当 10 日／分割して並列 | **K1〜K4 の 4 担当に分け、全体で約 24 日**（§4.5）。ディレクトリを分けて互いのファイルを編集しない。全員が PostgreSQL だけで先に書ける | 00 の K は 10 日だが、各章が書いたテストの一覧（§3.2.2）は約 120 ファイル。10 日では収まらない。独立に書けるので分割で最長経路に乗らない |
| D11-12 | 新しい SQLSTATE の追記 | 各章が個別に足す／この章で一覧を確定 | **§7.2 の表（11 個）を確定**。`error.rs` の `sqlstate` への追記は A（00 の方針どおり、他の担当が追記してよい）。重複して足さない | 00 §15.3 の 6 個に、04（`54001`）、07（`42939`）、09（`22007` `22008` `22009`）の 5 個が加わる |
| D11-13 | slt の lint | 目視／スクリプトで機械的に | **`slttools lint`（`tests/tools/slttools`。Rust）で検査し、CI の `slt-lint` ジョブが走らせる**（§3.2.5、§3.2.8） | 約 120 ファイルを 4 人が書く。ファイル末尾の DROP、`rowsort` と `ORDER BY` の併用、接頭辞の衝突などの規約を人が見張れない。M2-Q22（後始末を忘れた）の再発を防ぐ。**ツールは Rust で書く**: sandbox のイメージには `python3` も `uv` も入っておらず（実測）、既存のテストツール（isolation・difftest）も Rust |
| D11-14 | M4 の isolation spec | 足さない／足す | **2 本**を足す（§3.4.3）: `index-reader-writer.spec`（索引走査と未コミットの挿入）、`seq-nonblocking.spec`（`nextval` が待たず、ロールバックでも戻らない） | M3 の isolation は書き込み同士の待ちだけ。M4 の索引とシーケンスは「読み手が書き手を待たない」「値が戻らない」が要点で、PostgreSQL と同じ結果になる |
| D11-15 | pgbench のクラッシュ試験 | 足さない／足す | **`tests/compat/pgbench/crash.sh` を足す**（§3.5.3）: `pgbench -i` と `-c 4 -T 5` の途中で `kill -9` → 再起動 → `sum(abalance) = sum(tbalance) = sum(bbalance) = sum(delta)`（コミットされたトランザクションの原子性）と主キー | 実プロセスの `kill -9` と、実際のクライアントと、索引つきの表を同時に通せる。不変条件が単純 |

---

## 3. テスト全体

### 3.1 テストの層と置き場所

| # | 層 | 置き場所 | 実行 | 対象 | PostgreSQL で検証 | 章 |
|---|---|---|---|---|---|---|
| T1 | Rust の単体・統合テスト | `impl/rust/crates/*/src/**`（`#[cfg(test)]`）、`yuzhu-core/tests/*.rs` | `cargo test` | 各モジュールの契約（§3.9 の一覧） | 一部（`yuzhu-numeric` / `yuzhu-datetime` の差分コーパスは PostgreSQL 17 から作成済み） | 02〜10 |
| T2 | プランナのスナップショット | `yuzhu-core/tests/plan_golden/` | `cargo test` | SQL → build → 各ルール → 物理（04 §11.1） | しない（yuzhu の設計どおりの期待値） | 04 |
| T3 | 共有の SQL テスト | `tests/slt/m4/<ディレクトリ>/*.slt` | `tests/run.sh --target pg\|yuzhu` | 結果と SQLSTATE（§3.2） | **する（正解の基準）** | 02〜10 |
| T3a | plan_variants | `tests/slt/m4/plan_variants/*.slt`（`tests/gen/` から生成） | 同上 | `enable_*` を変えても同じ結果（§3.3） | する | 04 |
| T4 | 再起動・クラッシュ | `tests/restart/m4/<シナリオ>/` | `tests/run.sh --restart` / `--crash` | 索引・シーケンス・DDL の永続性（§3.4） | する | 06〜08 |
| T5 | isolation | `tests/isolation/specs/` | `yuzhu-isolation` | 読み手と書き手、シーケンス（§3.4.3） | する | 11 |
| T6 | 互換 | `tests/compat/{psql,copy,pgbench}/` | `tests/compat/run.sh --target pg\|yuzhu` | psql の出力、COPY のプロトコル、pgbench の完走（§3.5） | する | 10 |
| T7 | クラッシュ試験 層 1 | `yuzhu-core/tests/crash_sim/` | `cargo test --release -p yuzhu-core --test crash_sim` | SimVfs の上の全クラッシュ点（§3.6） | しない（不変条件で判定） | 06〜08、11 |
| T8 | クラッシュ試験 層 2 | `yuzhu-server/tests/crash_kill9.rs`（`#[ignore]`） | 夜間 | 実プロセスの `kill -9`（08 §7.5 の `serial` を足す） | しない | 08 |
| T9 | 差分ランダムテスト | `tests/tools/difftest/` | `difftest run` | yuzhu 対 PostgreSQL、TLP、NoREC、プラン変種（§3.7） | する（PostgreSQL が相手） | 03〜06、09 |
| T10 | PostgreSQL 回帰テストの取り込み | `tests/imported/pg_regress/`（生成物）、`tests/tools/pgregress/` | 夜間 | 一致した文の再実行（§3.8） | する | 11 |
| T11 | 性能の確認 | `PROGRESS.md` に記録 | 手動 | 退行の検出（合否にしない） | — | 06 §7.15 |

### 3.2 共通の SQL テスト（`tests/slt/m4/`）

#### 3.2.1 規則（M1〜M3 の規則 `tests/README.md` に追加するもの）

M1〜M3 の規則（1 ファイル 1 機能、1 レコード 1 文、末尾で DROP、SET は RESET、`ORDER BY` か `rowsort`、SQLSTATE での照合、`--override` は PostgreSQL にだけ使う）はそのまま守る。M4 で足す:

1. **オブジェクトの接頭辞はディレクトリごとに決まっている**（§3.2.4）。ファイルで作ったテーブル・索引・シーケンスは、そのファイルの最後ですべて DROP する（`DROP TABLE` は所有するシーケンスと索引も消す。単独で作ったシーケンスは `DROP SEQUENCE`）。**`z_final/no_leftovers.slt`（全スイートの最後）が `oid >= 16384` のリレーションが 0 件であることを確かめる**ので、1 つでも残すと最後に落ちる。
2. **結果の順序が決まらない問い合わせには必ず `ORDER BY` か `rowsort` を付ける**（ハッシュ結合・ハッシュ集約・集合演算・`DISTINCT` は PostgreSQL でも不定）。`ORDER BY` を付けても同順位が残るときは第 2 キーを足す。`LIMIT` には全順序の `ORDER BY` を付ける。
3. **NULL を必ず混ぜる**（結合キー、集約の入力、`NOT IN` の集合、`FULL JOIN` の `COALESCE`、索引のキー、UNIQUE）。
4. **環境を先頭で固定する**（日時を使うファイル）: `SET TIME ZONE 'UTC'`、`SET DateStyle = 'ISO, MDY'`。ファイルの最後で `RESET`。`now()` `clock_timestamp()` `random()` の値は結果に出さない（`IS NOT NULL` と `pg_typeof` で確かめる）。
5. **型を確かめる**には `pg_typeof(expr)` を使う（`query` の型文字 `I` `T` `R` より強い）。numeric の結果は文字列（`T`）で比べる（`R` は scale の違いを見落とす）。浮動小数の `sum` / `avg` は足す順序で変わらない値（整数値、2 進で正確な小数）だけ。
6. **PostgreSQL と yuzhu が違うものの書き方**（D11-3）:
   - PostgreSQL で成功し yuzhu が `0A000` / `42883` などで失敗するもの（M5 以降の機能）は、PostgreSQL 側を `skipif yuzhu`、yuzhu 側を `onlyif yuzhu` + `statement error (SQLSTATE)` の**対**で書く。2 つとも直前に `# KNOWN-DIFF: KD-<n> <理由>` のコメントを付ける（`KD-<n>` は `tests/slt/m4/KNOWN-DIFFS.md` の ID。初期の一覧は §3.2.7）。
   - yuzhu だけの振る舞い（EXPLAIN の独自の期待値、`yuzhu.query_mem_limit`、`log_cnt`）は `onlyif yuzhu`。直前に `# YUZHU-ONLY: <理由>`。
   - PostgreSQL にしかない機能の確認（`FORMAT JSON` の成功など）は `skipif yuzhu`。
   - **理由なしの `skipif` / `onlyif` は lint が拒否する**（§3.2.5）。
7. **エラーは SQLSTATE（`statement error (23505)`）で照合する**。メッセージの照合は補助の短い部分一致だけ。`--override` が書いた `db error: ...` の形は SQLSTATE の形に直す（M2 の規則）。yuzhu 独自の文言（`0A000` の本文など）は SQLSTATE だけ。
8. **EXPLAIN のファイルの先頭**で PostgreSQL にだけ `SET enable_bitmapscan = off` ほか 7 つを流す（10 §8.2。`skipif yuzhu`）。`ANALYZE` した後に流す（yuzhu の `ANALYZE` は何もしない）。
9. **PostgreSQL が圧縮して入れる値**（繰り返しの多い 3KB の文字列）は索引のキーに使わない（yuzhu は TOAST も圧縮もなく `54000`。06-Q19）。大きなキーのテストは乱数の 16 進文字列を**スクリプトで生成して埋め込む**（`slttools large-keys`）。
10. **OID・`relpages`・`reltuples`・`pg_opclass.oid` は比べない**。カタログのテストは名前に結合して比べ、`WHERE oid > 16383` か名前で絞る（M2 の規則）。他のテストのテーブルが残っていない前提のファイル（`catalog/pg_depend.slt` など）が壊れないよう、1 の後始末を厳守する。
11. **yuzhu で期待値を作らない**。`--override` は PostgreSQL にだけ使い、差分をレビューしてからコミットする（`tests/README.md`）。
12. **新しいファイルは必ず `tests/run.sh --target pg` で通してから置く**。

#### 3.2.2 ディレクトリとファイルの一覧

「対象」は `両方` = pg と yuzhu の両方で同じ期待値、`両方*` = 一部に `onlyif yuzhu` / `skipif yuzhu` の対がある、`yuzhu` = yuzhu だけ。「担当」は §4.5 の K1〜K4。**各ファイルの中身（個々の文と期待値）は出典の章に従う**。出典の章に下書きがあるもの（03 の付録 A の 11 ファイルは PostgreSQL 17.11 で通過を確認済み）は「下書き」と付けた。

**`join/`（K1。接頭辞 `jn_`）**

| ファイル | 内容 | 対象 | 出典 |
|---|---|---|---|
| `cross_inner.slt` | INNER / CROSS / カンマ結合、自己結合（別名）、3 表、式での結合、NULL キーは一致しない、派生表との結合、`count(*)` の検算 | 両方 | 03 §6.3 |
| `using_natural.slt` | USING / NATURAL の併合列（位置・型・修飾）、FULL の `COALESCE`、複数列、連鎖、エラー 6 種 | 両方 | 03 付録 A-1（下書き） |
| `outer_and_names.slt` | ON と WHERE の違い、LEFT / RIGHT / FULL（等値 + 残り、NULL キー）、入れ子の外部結合、ON から見える名前と `42P01`、FULL JOIN の `0A000`（`ON true` は KD-2） | 両方* | 03 付録 A-2（下書き） |
| `from_items.slt` | 派生表・列別名・別名なし、VALUES、`generate_series`（別名が列名）、システム列、出力列名 | 両方* | 03 付録 A-3（下書き） |

**`agg/`（K1。`ag_`）**

| ファイル | 内容 | 対象 | 出典 |
|---|---|---|---|
| `basic.slt` | 集約の解決のエラー、禁止位置（WHERE / GROUP BY / LIMIT など）、未対応の集約・構文の `0A000` | 両方* | 03 付録 A-4（下書き）。**値の検査（結果型・NULL・空入力・DISTINCT・FILTER）は 05 の下の 7 ファイルに移し、重複させない**（§3.2.3） |
| `group_resolution.slt` | GROUP BY は入力列が先、ORDER BY は出力列が先、位置・定数のエラー、HAVING、DISTINCT / DISTINCT ON | 両方 | 03 付録 A-5（下書き） |
| `functional_dep.slt` | 主キーへの関数従属（複合主キー・UNIQUE は対象外）、HAVING だけの集約問い合わせ、副問い合わせの未グループ列 | 両方* | 03 付録 A-6（下書き） |
| `empty.slt` | 空入力（GROUP BY なしで 1 行、あれば 0 行）、`count` は 0 で他は NULL、`HAVING` だけ | 両方 | 05 §10.5 |
| `types.slt` | `sum(int2/int4)` = bigint、`sum(int8)` = numeric、`avg(int*)` = numeric、`avg(float)` = double、`sum(float4)` = real、43 行の `AGGREGATES` の結果型（`pg_typeof`）、`sum(bool)` の `42883` | 両方 | 05 §10.5 + 09 `agg_types.slt`（統合） |
| `avg_scale.slt` | `avg` の numeric の scale（05 §6.4 の表の値） | 両方 | 05 §10.5 |
| `minmax.slt` | text・char(n)・numeric・date・NaN・NULL のみ、等しいときの表示（`1.100`、09 D-9-12） | 両方 | 05、09 |
| `bool.slt` | `bool_and` / `bool_or` / `every` と NULL | 両方 | 05 |
| `distinct_filter.slt` | `count(DISTINCT)`、`sum(DISTINCT)`、`avg(DISTINCT)`、`FILTER`（全部落とす）、GROUP BY との組み合わせ | 両方 | 05 |
| `overflow.slt` | `sum(float8)` / `sum(float4)` / `avg(float8)` の `22003`（`sum(int4)` の `bigint out of range` は Rust の単体テスト） | 両方 | 05 |

**`subquery/`（K1。`sb_`。03 の下書きの `sq_` は機械的に置換する。§3.2.4）**

| ファイル | 内容 | 対象 | 出典 |
|---|---|---|---|
| `null_semantics.slt` | IN / NOT IN / ANY / ALL / EXISTS の三値論理と空集合、スカラー副問い合わせ（0 行・2 行の `21000`）、行値 IN、列数・型のエラー | 両方 | 03 付録 A-7（下書き）、05 §4.2.3 |
| `correlated.slt` | 2 段の相関、派生表を通り抜ける参照、ON・HAVING・ORDER BY・SET・DELETE の中、未グループ列、LIMIT の変数、CHECK / DEFAULT の禁止、外側の集約（KD-8）、**UNION の腕から外側の列を参照する**（`levels_up` の数え方の確認。§7.1 の C-3） | 両方* | 03 付録 A-8（下書き） |
| `row_in.slt` | 行値の `IN` / `NOT IN` / `= ANY` / `<> ALL`（NULL を含む）、その他の行値演算子は yuzhu で `0A000`（M4 後半・任意） | 両方* | 03 §6.3 |

**`setop/`（K1。`so_`）**: `union_types.slt`（型の決定・unknown・列名・外枠の ORDER BY / LIMIT・優先順位・ALL・VALUES の腕・エラー。03 付録 A-9、下書き）、`in_contexts.slt`（FROM の派生表・副問い合わせ・INSERT ... SELECT・CTE の本体・腕の中の GROUP BY / 副問い合わせ）。対象は両方。

**`cte/`（K1。`ct_`）**: `scope.slt`（前の CTE の参照、2 回の参照、列別名、同名、MATERIALIZED / NOT MATERIALIZED、WITH + 集合演算 / VALUES / TABLE、エラー。03 付録 A-10、下書き）、`recursive_syntax.slt`（`WITH RECURSIVE` の非再帰は通る。再帰とデータ変更 CTE は PostgreSQL だけ `skipif yuzhu`、yuzhu は `(0A000)`。KD-4）。対象は両方*。

**`dml/`（K1。`dm_`）**: `update_from.slt`（UPDATE ... FROM / DELETE ... USING の結合・複数一致（最初の値）・自己結合・派生表・VALUES・generate_series、SET の副問い合わせ、解析の順序。03 付録 A-11、下書き）、`insert_select.slt`（`INSERT ... SELECT` に結合・集約・集合演算・CTE・generate_series（`pgbench -i` の形）、unknown の列の代入、シーケンス・索引への DML の `42809`）、`returning.slt`（`RETURNING` の全ケース。**`RETURNING_ENABLED` が true になってから有効にする**（任意）。FROM 列を含むものは `0A000`。KD-26）。対象は両方*。

**`types/`（K3。`ty_`。09 §12.2。ファイルの先頭で `SET TIME ZONE 'UTC'` と `SET DateStyle = 'ISO, MDY'`）**

| ファイル | 内容 | 対象 |
|---|---|---|
| `numeric_basic.slt` | リテラルの型、四則と scale、`round` `trunc` `ceil` `floor` `abs` `sign` `mod` `div` `scale`、`round(2)` が float8、NaN / Infinity、`22012`、`1.10 = 1.1`、ORDER BY / GROUP BY / DISTINCT、型の混合 | 両方 |
| `numeric_typmod.slt` | `numeric(5,2)` 列への INSERT（丸め・`22003` と DETAIL）、`::numeric(7,2)`、`typmod_in` のエラー、`format_type`、`atttypmod` | 両方 |
| `numeric_cast.slt` | `float8 → numeric`、`numeric → int`（丸め・範囲外・NaN）、text との相互変換、`22P02` | 両方 |
| `bpchar.slt` | `char(n)` 列（パディング・`octet_length`・`length`・`22001`）、比較、`\|\|`、LIKE と `~`、`IN`、ORDER BY / GROUP BY / DISTINCT / `min` / `max`、UNIQUE、`pg_typeof('a'::char)` | 両方 |
| `datetime_basic.slt` | date / timestamp / timestamptz の入出力、タイムゾーンつき入力、比較、`date ± int`、`date - date`、`timestamp(0)` 列の丸め、`timestamptz` を `timestamp` 列へ代入、`min` / `max`。`timestamp - timestamp` と `interval` は KD-1 の対 | 両方* |
| `datetime_tz.slt` | `Asia/Tokyo` と `America/New_York` での出力、変換、DST の境界、`SHOW TimeZone`、`22023`、`SET DateStyle` | 両方 |
| `datetime_errors.slt` | `22007` `22008`（HINT つき）`22009` `22023` | 両方 |
| `regclass.slt` | `regclass` / `regtype` の入出力（`'pg_class'` `'1259'` `'-'`、`42P01` `42602` `42704`）、`oid::regclass`、`regclass + 1` の `42883`、`to_regclass`、`pg_typeof` | 両方 |
| `regex.slt` | `~` `~*` `!~` `!~*`（text / name / bpchar）、psql のパターン、NULL、`2201B`、`\1` と `(?=` の `0A000`（KD-15） | 両方* |
| `generate_series.slt` | 正順・逆順・step・境界・NULL・`22023`、`AS g` の列名、結合、SELECT 句での使用の `0A000`（KD-27） | 両方* |

**`catalog/`（K2。`cat_`。07 §8.1）**: `catalog_columns_m4.slt`（追加 9 カタログの列と型）、`pg_index.slt`、`pg_constraint_index.slt`、`pg_depend.slt`（§3.6 の表の全行を `deptype` ごとに集計）、`pg_class_kinds.slt`（`relkind` r / i / S、`relam`、`relhasindex`）、`pg_attribute_index.slt`（索引の列）、`opclass_catalog.slt`（行数は比べない）、`language.slt`、`catalog_closure.slt`（追加カタログの OID 参照が閉じている）、`table_is_visible.slt`。対象は両方。**07 の `consistency.slt` は `z_final/` へ、`psql_queries.slt` は `psql/` へ移す**（§3.2.3）。

**`constraint/`（K2。`cst_`。07 §8.1）**: `pk_basic.slt`（PRIMARY KEY の `23505` / `23502`、DELETE → 同じキーの INSERT、キー以外の UPDATE、ROLLBACK した行と同じキー、複数列）、`unique_basic.slt`（NULL の重複は可、型ごとのキー、UPDATE での衝突）、`names.slt`（自動名・衝突・統合・PK の名前の移し替え・長い名前）、`create_table_errors.slt`（`42P16` `42701` `42703` `42P07` `42710` `42704` `22023` `0A000`）、`alter_add.slt`（`ADD PRIMARY KEY` / `ADD CONSTRAINT UNIQUE`、重複と NULL が両方ある表で `23505` が先、`OWNER TO`）、`drop_table_dependencies.slt`（`2BP01` の DETAIL、`CASCADE` の NOTICE）、`unique_build_versions.slt`（死んだ版を重複と誤判定しない）。対象は両方。

**`index/`（K2。`idx_`。07 §8.1、06 §7.14）**

| ファイル | 内容 | 出典 |
|---|---|---|
| `create_index.slt` | 基本、`DESC` / `NULLS FIRST`、複数列、自動名、`IF NOT EXISTS`、`CONCURRENTLY`（ブロック内は `25001`）、`WITH (fillfactor)` | 07 |
| `create_index_errors.slt` | `42P01` `42703` `42809` `42501` `42P07` `54011` `42704` `42804` `0A000` `22023` `25001` | 07 |
| `opclass.slt` | opclass の明示、`xid` の索引の `42704`、`varchar` に `varchar_ops` / `text_ops`、`regclass` の索引 | 07 |
| `drop_index.slt` | `42704`、NOTICE、`42809` の HINT、`2BP01`（制約が所有する索引）、`CONCURRENTLY` のエラー | 07 |
| `index_dml.slt` | 索引つきの表への INSERT / UPDATE / DELETE の後、`enable_seqscan` の on / off で同じ結果（小さな表） | 07 |
| `index_tx.slt` | `BEGIN; CREATE INDEX; ROLLBACK`、`DROP INDEX` のロールバック、同じトランザクションで作った表への `ADD PRIMARY KEY` | 07 |
| `unique_violation.slt` | `23505` のメッセージと DETAIL（単一・複合・text・date・numeric、長い値を切り詰めない）、`CREATE UNIQUE INDEX` の重複、NULL は可 | 06 |
| `unique_txn.slt` | トランザクション内の DELETE → INSERT、ROLLBACK、UPDATE のキー変更 | 06 |
| `large_key.slt` | `BT_MAX_ITEM_SIZE` の境界（2692 / 2693 バイトなど）。圧縮されない値をスクリプトで生成（`slttools large-keys`） | 06 |
| `many_rows.slt` | 数万行の INSERT・UPDATE・DELETE の後、等値・範囲・`ORDER BY`・`ORDER BY ... DESC LIMIT` の結果が全走査と一致 | 06 |
| `null_order.slt` | 索引の ASC / DESC・NULLS FIRST / LAST と `a > 5`・`a < 5`・`a IS NULL`（結果のみ） | 06 |

**`ddl/`（K2。`ddl_`。07 §8.1 の `index/` から移した）**: `truncate.slt`（単一・複数表・`ONLY`・トランザクション内・索引つき・`RESTART IDENTITY`・`relfilenode` の変化）、`vacuum_analyze.slt`（`VACUUM` はブロック内で `25001`、`ANALYZE` は成功、オプション）、`reloptions.slt`（`WITH (fillfactor)` の範囲と `22023`）、`readonly_ddl.slt`（`BEGIN READ ONLY` の中の DDL は `25006`、VACUUM と ANALYZE は成功）。対象は両方。

**`seq/`（K2。`sq_` `sr_` `id_`。08 §7.1）**: `basic.slt`、`limits.slt`（`2200H` と `CYCLE`）、`setval_currval.slt`（`55000`）、`cache.slt`（2 接続の交互）、`readonly_txn.slt`（`25006`）、`rollback.slt`、`create_options.slt`（検証メッセージの全部）、`alter.slt`、`drop.slt`、`serial.slt`、`identity.slt`（`428C9`）、`catalog_rows.slt`、`select_from_seq.slt`（`42809`）、`log_cnt.slt`（`yuzhu` のみ）。対象は `log_cnt.slt` 以外は両方（`alter.slt` の「ロールバックしても `RESTART` が残る」は KD-11）。

**`explain/`（K3。`ex_`。10 §8.1、04 §11.7）**

| ファイル | 内容 | 対象 |
|---|---|---|
| `options.slt` | option の解釈とエラー（`unrecognized EXPLAIN option` ほか） | 両方* |
| `format.slt` | 結合・インデックスなしの単一表で、**プランの選び方に依らず PostgreSQL と同じになる**問い合わせ（`Seq Scan` + `Filter`、`Result` + `One-Time Filter`、`Values Scan`、`Sort`、`HashAggregate`、`Limit`、`Append`、`Unique`、`CTE Scan`、`InitPlan` / `SubPlan`）の `EXPLAIN (COSTS OFF)` | **両方** |
| `nodes.slt` | 10 §3.11 の 1〜10 の出力。PostgreSQL と一致するものは両方、プランが違うものは `onlyif yuzhu` で yuzhu の期待値 | 両方* |
| `verbose.slt` | 修飾と `Output:` | 両方* |
| `analyze.slt` | `EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF)`: `actual rows=N loops=L`、`Rows Removed by Filter`、`never executed`、DML が実際に書く、ROLLBACK で戻る | 両方* |
| `subplans.slt` `cte.slt` `dml.slt` | InitPlan / SubPlan / CTE / DML のプランの形 | 両方* |
| `deparse_stored.slt` | CHECK 式の `pg_get_constraintdef(oid, false/true)` と `pg_get_expr`（10 §4.9 の表） | **両方** |
| `deparse_plan.slt` | `EXPLAIN (COSTS OFF) SELECT ... WHERE <式>` の `Filter:` 行（10 §4.9） | 両方* |
| `format_type.slt` | `format_type` の表（10 §4.8） | 両方 |

**`copy/`（K3。`cp_`）**: `errors.slt`（`G` を送る**前**に失敗するもの: 存在しない表・列、`42701`、option の誤り、読み取り専用の `25006`、`COPY TO` / CSV の `0A000`（KD-19））。COPY のデータを送るテストは slt ではできない（`tests/compat/copy/`）。

**`psql/`（K3。`psql_`。10 §6.2）**: `dt.slt` `di.slt` `dn.slt` `ds.slt` `l.slt`（psql 17 の SQL をそのまま。`AND c.relname LIKE 'psql\_t\_%'` を足して他のテストの残りを除く）、`catalog_relkind.slt`、`pg_am.slt`、`case_relkind.slt`、`relkind_in.slt`、`regex_names.slt`、`name_ops.slt`、`functions.slt`、`df.slt`（任意）。対象は両方。

**`plan_variants/`（K3。`pv_`。§3.3）**: `join_inner.slt` `join_outer.slt` `join_semi_anti.slt` `agg.slt` `distinct_setop.slt` `subquery.slt` `index_scan.slt` `order_limit.slt` `dml.slt`（生成。両方）、`plan_shapes.slt`（`yuzhu` のみ。設定ごとに期待するノード名が EXPLAIN に出る）。

**`mem/`（K3。`mem_`）**: `limit.slt`（`yuzhu` のみ）。`SET yuzhu.query_mem_limit = '1MB'` のあと `generate_series(1, 1000000)` の ORDER BY・GROUP BY・自己結合・DISTINCT・集合演算・Materialize が `53200`（メッセージの先頭は `out of memory`）、上限を戻すと成功（05 §10.6）。

**`z_final/`（K4。接頭辞なし）**: `consistency.slt`（07 §8.3 の SQL 版の条件 1〜9。ユーザーオブジェクトに対して）、`no_leftovers.slt`（`pg_class` / `pg_attrdef` / `pg_constraint` / `pg_index` / `pg_depend` / `pg_sequence` の `oid >= 16384` の行が 0 件）。対象は両方。

合計 118 ファイル（`join` 4、`agg` 10、`subquery` 3、`setop` 2、`cte` 2、`dml` 3、`types` 10、`catalog` 10、`constraint` 7、`index` 11、`ddl` 4、`seq` 14、`explain` 11、`copy` 1、`psql` 13、`plan_variants` 10、`mem` 1、`z_final` 2）。

#### 3.2.3 重複と置き場所の整理（章の間で重なっていたもの）

| 重なり | 整理 |
|---|---|
| 09 の `types/agg_types.slt` と 05 の `agg/types.slt`（結果型） | `agg/types.slt` に統合（`types/` には置かない） |
| 09 の `types/psql_dt.slt`、07 の `catalog/psql_queries.slt`、10 の `psql/dt.slt` ほか | `psql/` に統合。10 §6.2 の表の `dt_di.slt` は `dt.slt` と `di.slt` に分ける。08 の `psql/ds.slt` も同じ場所 |
| 07 の `catalog/consistency.slt` | `z_final/consistency.slt` へ（D11-2） |
| 07 の `index/{truncate,vacuum_analyze,reloptions,readonly_ddl}.slt` | `ddl/` へ（D11-2） |
| 03 の `agg/basic.slt`（結果型・NULL・空入力・DISTINCT・FILTER）と 05 の `agg/{empty,types,distinct_filter}.slt` | 値の検査は 05 の側に置き、`basic.slt` は解決のエラー・禁止位置・`0A000` に絞る |
| 06 の `index/many_rows.slt` と 07 の `index/index_dml.slt` | 両方残す（規模が違う: 数万行 / 小さな表） |
| 04 の `explain/format.slt`（PostgreSQL と一致する単一表の EXPLAIN）と 10 の `explain/nodes.slt` | 両方残す。`format.slt` は**全行が両方の対象**（`onlyif yuzhu` を持たない）という性質で区別する |
| 06 の `restart/m4/index_persist.slt`（1 ファイルとして依頼） | シナリオ `tests/restart/m4/idx-persist/` に（再起動のテストは複数フェーズのファイル列。§3.4） |
| 08 の `tests/restart/m4/seq-*` と 07 の `tests/restart/m4/01-*`〜`09-*`（番号の付け方が違う） | 名前を §3.4.2 の表に統一し、番号は付けない |

#### 3.2.4 接頭辞の表

テーブル・索引・シーケンス・制約の名前は、置かれるディレクトリの接頭辞で始める。lint が検査する。

| ディレクトリ | 接頭辞 | 備考 |
|---|---|---|
| `join` `agg` `setop` `cte` `dml` | `jn_` `ag_` `so_` `ct_` `dm_` | 03 のとおり |
| `subquery` | **`sb_`** | 03 は `sq_` と書いたが、08 のシーケンス（`sq_`）と衝突する。03 の下書きは `sq_` → `sb_` に機械的に置換（C-7） |
| `types` `catalog` `constraint` `index` `ddl` | `ty_` `cat_` `cst_` `idx_` `ddl_` | 07・09 は例で `cat_i_` `nm_` `trc_` を使う。ディレクトリ接頭辞で始まっていればよい（`cat_i_t1`、`cst_nm_t2` などの二段目は自由） |
| `seq` | `sq_`（シーケンス）、`sr_`（SERIAL）、`id_`（IDENTITY） | 08 のとおり |
| `explain` `copy` `psql` `plan_variants` `mem` | `ex_` `cp_` `psql_` `pv_` `mem_` | 10 は `psql_t_` を使う |
| `tests/restart/m4/<シナリオ>` | `m4r_<シナリオの短縮>_` | M2 は `rs1_`〜、M3 は `cr1_`〜 |
| `tests/compat/*` | `compat_` | 10 のとおり |

#### 3.2.5 slt の lint（`slttools lint`。K4）

`tests/tools/slttools`（Rust の独立した Cargo プロジェクト。§3.2.8）の `slttools lint`。`tests/slt`、`tests/restart`、`tests/gen` の出力に対して走り、違反を `ファイル:行: 規則 ID: メッセージ` で出して終了コード 1。CI の `slt-lint` ジョブ（PostgreSQL もサーバも不要）。**M1〜M3 の既存ファイルにも走らせる**（最初の作業で違反を直す。M2-Q22 の後始末）。

| 規則 | 検査 |
|---|---|
| L01 後始末 | `CREATE TABLE` / `CREATE [UNIQUE] INDEX` / `CREATE SEQUENCE` の名前が、そのファイルの `DROP TABLE` / `DROP INDEX` / `DROP SEQUENCE`（`IF EXISTS` 可）に現れる（索引は表の DROP で足りる。SERIAL / IDENTITY のシーケンスも同様）。`z_final/` と `tests/restart`（最後のフェーズだけ）は例外の規則を別に持つ |
| L02 設定の復帰 | `SET <name>`（`LOCAL` なし、トランザクションの外）に対応する `RESET <name>` が後にある。`SET TIME ZONE` は `RESET TIME ZONE` |
| L03 接頭辞 | 作るオブジェクトの名前が §3.2.4 の接頭辞で始まる |
| L04 順序 | `rowsort` を付けた問い合わせの最上位に `ORDER BY` がない（あれば無意味）。複数行の結果で `ORDER BY` も `rowsort` もない問い合わせは警告（`LIMIT 1` と集約だけの問い合わせは除く） |
| L05 エラーの形 | `statement error` / `query error` が `(SQLSTATE)` かメッセージの正規表現を持つ。`db error:` を含まない |
| L06 既知の差 | `skipif yuzhu` / `onlyif yuzhu` の直前の行が `# KNOWN-DIFF: KD-<n> ...` または `# YUZHU-ONLY: ...`。`KD-<n>` が `KNOWN-DIFFS.md` にある。`KNOWN-DIFFS.md` の各 ID に使用箇所が 1 つ以上ある（使われなくなった差を消す） |
| L07 型文字列 | `query <型文字>` の文字数が、最初の期待行の空白区切りの列数と合う（値に空白を含む `T` 列がなければ） |
| L08 `statement count 0` | SELECT に `statement count 0` を付けない（`--override` が空の結果をこう書き換える。`query T` + 空の期待値に戻す。M2 の規則） |
| L09 不定な値 | `now()` `clock_timestamp()` `random()` `current_timestamp` を SELECT 句に直接置く問い合わせ（`pg_typeof(...)` / `IS NOT NULL` / `::date` で包まない） |
| L10 大きさ | 1 ファイルが 1,500 行を超える（警告）。`tests/slt/m1` 〜 `m3` は小数リテラル・JOIN などの範囲外の機能を使っていない（`tests/README.md` の旧規則を m1〜m3 にだけ適用。M4 は適用しない） |
| L11 生成物 | `tests/slt/m4/plan_variants/*.slt` が `slttools plan-variants check` と一致する（再展開で変わらない） |

#### 3.2.6 PostgreSQL の側で形をそろえる設定

EXPLAIN のファイルは 10 §8.2 のとおり（先頭に `skipif yuzhu` で `SET enable_bitmapscan = off; SET enable_indexonlyscan = off; SET enable_memoize = off; SET enable_mergejoin = off; SET enable_tidscan = off; SET max_parallel_workers_per_gather = 0; SET jit = off`）。テーブルは小さく、PostgreSQL では `ANALYZE` してから流す。インデックススキャンを強制するときは `SET enable_seqscan = off`、NestedLoop は `SET enable_hashjoin = off`、`GroupAggregate` は `SET enable_hashagg = off`。**目的は書式（ノード名・詳細行・字下げ・式の文字列）の一致であって、プランの一致ではない**。

#### 3.2.7 既知の差分の初期の一覧（`tests/slt/m4/KNOWN-DIFFS.md`。K4 が作り、K1〜K3 が使う）

各章が「PostgreSQL と違う」と明記したもの。ID は固定する（ファイルのコメントが参照する）。

| ID | 内容（yuzhu の振る舞い） | 出典 |
|---|---|---|
| KD-1 | `interval` / `time` / `timetz` は `0A000`。`timestamp - timestamp`、`ts ± '1 day'` は `42883`（PostgreSQL では動く） | 09-Q2 |
| KD-2 | `FULL JOIN ... ON true`（ハッシュ可能な等値のない FULL）は `0A000`。PostgreSQL は通す | 03-Q1、04-D10 |
| KD-3 | `LATERAL`（明示・暗黙）は `0A000` | 03-Q5 |
| KD-4 | `WITH RECURSIVE` の再帰、データ変更 CTE、`WITH ... INSERT/UPDATE/DELETE` は `0A000` | 03-Q6、03-Q7 |
| KD-5 | `IS [NOT] DISTINCT FROM` は `0A000`（M4 後半の任意項目で入れるかもしれない） | 03-Q12 |
| KD-6 | `FOR UPDATE` / `FOR SHARE` は `0A000` | 03 §8 |
| KD-7 | ウィンドウ関数、`GROUPING SETS` / `ROLLUP` / `CUBE`、集約内の `ORDER BY`、`string_agg` ほか未対応の集約は `0A000` | 00 §3、03-Q16 |
| KD-8 | 外側の問い合わせに属する集約（`(select max(t.a))`）は `0A000` | 03-Q4、02-Q4 |
| KD-9 | `ANY` / `ALL` の配列形、`ARRAY(SELECT ...)`、行値のその他の使い方、全行参照（`count(t)`）は `0A000` | 03-Q8、03-Q9、03-Q15 |
| KD-10 | `JOIN ... USING (...) AS j`、括弧つき JOIN の別名は `0A000` | 03-Q13 |
| KD-11 | `ALTER SEQUENCE` / `TRUNCATE ... RESTART IDENTITY` の状態はロールバックされない。`log_cnt` は ALTER のたびに 0 | 08-Q5、08-Q14 |
| KD-12 | `pg_typeof(1/0)` は `integer`（PostgreSQL は評価してエラー）。`timestamp(7)` の `WARNING` なし。DEFAULT の `'now'::timestamp` が使うたびの時刻 | 09-Q6、09-Q10、09-Q7 |
| KD-13 | 正規表現の後方参照・先読みは `0A000` | 09-Q5 |
| KD-14 | `pg_class.relpages` / `reltuples`、`relhasindex` の更新時機、`pg_type.typmodin` が 0 | 07-Q12、07-Q13、09 §3.2 |
| KD-15 | 圧縮されうる大きなキー（繰り返しの多い 2.7KB 以上）は `54000` | 06-Q19 |
| KD-16 | 索引の列に使えない型（`xid` `cid` `tid` `"char"` `int2vector`）は `42704`。`name` の索引列の `atttypid` が `name` | 07 D07-19、07-Q15 |
| KD-17 | `TEMP` / `UNLOGGED` シーケンス、`ALTER SEQUENCE RENAME` / `SET SCHEMA`、`ALTER TABLE` の `ADD PRIMARY KEY` / `UNIQUE` / `OWNER TO` 以外は `0A000` | 08-Q6、00 D-12 |
| KD-18 | `fillfactor = 50.5`（小数）は `22023`。`autovacuum_enabled` などその他の reloptions は `22023` | 07-Q8 |
| KD-19 | `COPY` の CSV・バイナリ・`TO`・`WHERE`・`ON_ERROR`、`EXPLAIN` の `FORMAT JSON` / `XML` / `YAML` は `0A000` | 10-Q4、10-Q7 |
| KD-20 | `\d tbl` の配列を使う 2 本の問い合わせは失敗する | 10-Q6 |
| KD-21 | `GROUP BY (SELECT ...)` の式を SELECT に書き直すと `42803`（PostgreSQL は構造の等しい副問い合わせを一致とみなす） | 03 §8、02-Q10 |
| KD-22 | 同じ `CREATE TABLE` が作るシーケンスの名前を DEFAULT に書くと `42P01`。同じ文の 2 つの暗黙のシーケンスの名前の衝突を避ける（上位互換） | 08-Q10、08-Q13 |
| KD-23 | 相関のある MATERIALIZED CTE、外側の列を参照する揮発性の CTE は `0A000` | 05-Q5、04-Q11、02-Q5 |
| KD-24 | `generate_series(numeric, ...)` / 日時版 / `generate_series(1, 10.5)` は `42883`。FROM 句の `generate_series` 以外の関数は `0A000` | 09、03-Q11 |
| KD-25 | `EXPLAIN` のプランの選び方（インデックスを小さな表でも使う、`Hash Right Join` の向きなど）と InitPlan の位置・番号 | 04 §2.2、04-Q7 |
| KD-26 | `RETURNING` に FROM / USING の列を使うと `0A000`（`RETURNING` 自体が M4 後半・任意） | 05-Q4、03-Q14 |
| KD-27 | SELECT 句の集合返却関数は `0A000` | 00 §3 |
| KD-28 | `transaction_timeout` は受け付けるだけで強制しない | 10-Q8 |
| KD-29 | `CREATE SCHEMA` / `CREATE DATABASE` は M5。compat テストは `postgres` データベースの `public` だけで動く | 00 §3（§7.1 の C-12） |

#### 3.2.8 テスト用ツール `tests/tools/slttools`（K4）

`tests/tools/difftest` と `tests/tools/isolation` と同じ形（`impl/rust` のワークスペースの外の独立した Cargo プロジェクト。`[workspace]` を空で持つ。依存は `postgres` と `clap` だけ）。**Rust で書く理由**: sandbox のイメージには `python3` も `uv` もなく（実測）、AI の実装エージェントが動く環境で走らせられない。

| サブコマンド | 内容 | 使う場所 |
|---|---|---|
| `slttools lint [paths...]` | §3.2.5 の規則 L01〜L11。終了コード 0 / 1 | CI の `slt-lint`、手元 |
| `slttools plan-variants base\|expand\|check` | §3.3.2 | K3、CI の `slt-lint`（`check`） |
| `slttools large-keys --size N --count M` | 圧縮されない（乱数の 16 進文字列の）キーの `INSERT` 文を出力（`index/large_key.slt` に埋め込む） | K2 |
| `slttools consistency` | `tests/slt/_include/consistency.slt.part` を各シナリオの最後のフェーズへコピーする（`include` が使えない場合） | K4 |
| `slttools pgregress import\|report` | §3.8 | K3、夜間 |

```rust
// tests/tools/slttools/src/main.rs（clap の derive。契約の形）
#[derive(clap::Parser)] #[command(name = "slttools")]
enum Cmd {
    /// §3.2.5。paths の既定は tests/slt tests/restart。違反があれば終了コード 1
    Lint { paths: Vec<PathBuf>, #[arg(long)] only: Vec<String> /* 規則 ID: L01 など */ },
    /// §3.3.2
    PlanVariants { #[command(subcommand)] action: PlanVariantsAction /* Base | Expand | Check */, #[arg(long)] family: Vec<String> },
    /// 圧縮されないキーの INSERT 文を標準出力へ（seed 固定）
    LargeKeys { #[arg(long)] size: usize, #[arg(long)] count: usize, #[arg(long, default_value_t = 1)] seed: u64, #[arg(long)] table: String },
    /// consistency.slt.part を tests/restart/m4 の各シナリオの最後のフェーズへコピー（--check は差分があれば終了コード 1）
    Consistency { #[arg(long)] check: bool },
    /// §3.8
    Pgregress { #[command(subcommand)] action: PgregressAction /* Import | Report */ },
}
```

ビルド: `cargo build --release --manifest-path tests/tools/slttools/Cargo.toml`（バイナリは `${CARGO_TARGET_DIR:-tests/tools/slttools/target}/release/slttools`）。`tests/README.md` に追記する。

### 3.3 `plan_variants`（`enable_*` を変えても同じ結果）

**目的**: 最適化のルールと実行ノードの組み合わせ（Hash Join / Nested Loop / Index Scan / HashAggregate / GroupAggregate / Materialize）を、**同じ問い合わせの結果が設定に依らず同じ**という不変条件で網羅する（`m4-query.md` §9.2、04 §11.4）。PostgreSQL にも同名の設定があるので、**同じファイルを PostgreSQL にも流して**期待値の正しさを確かめる。PostgreSQL では `enable_*` は「コストを足す」だけで、本当にそのプランになる保証はない（結果が同じなら問題ない）。yuzhu では 04 §9 の「他に選択肢があれば避ける」で、プランが実際に変わる。

#### 3.3.1 プロファイル

```text
プロファイル               SET する設定（RESET は各ブロックの最後）
default                    （なし）
no_hashjoin                enable_hashjoin = off
no_nestloop                enable_nestloop = off
no_hashjoin_no_nestloop    enable_hashjoin = off, enable_nestloop = off     # 等値結合は Nested Loop（04 §9）
no_indexscan               enable_indexscan = off
no_seqscan                 enable_seqscan = off
no_hashagg                 enable_hashagg = off
no_material                enable_material = off
no_sort                    enable_sort = off
combined                   enable_hashjoin = off, enable_hashagg = off, enable_indexscan = off
```

#### 3.3.2 テンプレートと生成

`slttools plan-variants`（§3.2.8）が、`tests/gen/plan_variants/<family>.tpl` から `tests/slt/m4/plan_variants/<family>.slt` を作る。**生成物をコミットし、期待値は PostgreSQL で 1 回だけ作る**（D11-4）。

```text
# <family>.tpl
-- family: join_inner            # ファイル名と接頭辞 pv_ji_ の元
-- profiles: default,no_hashjoin,no_nestloop,no_hashjoin_no_nestloop,no_indexscan,no_seqscan,combined
-- setup
CREATE TABLE pv_ji_t (a int PRIMARY KEY, b int, c text)
INSERT INTO pv_ji_t SELECT g, g % 7, 'x' || g FROM generate_series(1, 120) g
...
-- q: rowsort ITT
SELECT t.a, u.c FROM pv_ji_t t JOIN pv_ji_u u ON t.b = u.b WHERE t.a < 30
-- q: ordered IT
SELECT ... ORDER BY 1, 2
-- teardown
DROP TABLE pv_ji_t
```

```sh
slttools plan-variants base      # tests/gen/out/<family>.base.slt（default プロファイルだけ、期待値は空）を作る
SLT_EXTRA_ARGS=--override tests/run.sh --target pg tests/gen/out/<family>.base.slt     # PostgreSQL で期待値を埋める（yuzhu に使わない）
slttools plan-variants expand    # 期待値つきの base を全プロファイルに展開して tests/slt/m4/plan_variants/<family>.slt を書く
slttools plan-variants check     # コミット済みの生成物を、その中の default ブロックから再展開して一致を確かめる（PostgreSQL 不要。CI の slt-lint が呼ぶ）
```

展開の形（各プロファイルは、設定、全問い合わせ、RESET の順。期待値はすべて default と同じ）:

```text
statement ok
SET enable_hashjoin = off

query ITT rowsort
SELECT ...
----
(PostgreSQL が作った期待値)

statement ok
RESET enable_hashjoin
```

#### 3.3.3 問い合わせ群（ファイルごとの家族）

| ファイル | 問い合わせ群（各 15〜40 問。表は 200 行以下） |
|---|---|
| `join_inner.slt` | 内部結合（等値・複数キー・式・残りの条件・自己結合・3 表）、NULL キー、主キーと副インデックスの結合、直積 |
| `join_outer.slt` | LEFT / RIGHT / FULL（等値 + 残り）、ON と WHERE の違い、入れ子の外部結合、外部結合の内部結合化が効く WHERE（`u.d = 3`） |
| `join_semi_anti.slt` | `EXISTS` / `NOT EXISTS` / `IN` / `NOT IN`（NULL を含む）、相関 IN、ON の中の EXISTS、入れ子 |
| `agg.slt` | GROUP BY / HAVING / DISTINCT / `count(DISTINCT)` / FILTER、主キーへの関数従属、空入力、複数集約 |
| `distinct_setop.slt` | DISTINCT ON、UNION / INTERSECT / EXCEPT（ALL を含む）、集合演算の入れ子と ORDER BY |
| `subquery.slt` | スカラー・相関・NOT IN・ANY / ALL・FROM の副問い合わせ・CTE（参照 0 / 1 / 2 回、MATERIALIZED） |
| `index_scan.slt` | 主キーと副インデックスの等値・範囲・IS NULL・複合キーの先頭列、DESC / NULLS の索引、UPDATE / DELETE の後の索引走査 |
| `order_limit.slt` | `ORDER BY ... LIMIT`（索引順で満たせる / 満たせない）、OFFSET、後ろ向きスキャンになる `DESC LIMIT` |
| `dml.slt` | UPDATE ... FROM / DELETE ... USING / INSERT ... SELECT を実行して結果の表を比べる（トランザクションを ROLLBACK して表を元に戻す） |

**規則**: 結果の順序は完全に決める（`ORDER BY` の全キーか `rowsort`）。浮動小数・時刻・乱数は使わない。PostgreSQL が `enable_*` を変えても**エラーにならない**問い合わせだけ（FULL JOIN の `ON` はハッシュ可能な等値を持つものだけ。KD-2）。

#### 3.3.4 プランが実際に変わっていることの確認（`plan_shapes.slt`、`yuzhu` のみ）

PostgreSQL はコストで選ぶので、yuzhu で「設定を変えたのに同じプランだった」（変種テストが何も検査していない）ことは、結果の比較では分からない。`plan_variants/plan_shapes.slt`（`onlyif yuzhu`、手書き）が、プロファイルごとに `EXPLAIN (COSTS OFF)` に**期待するノード名が出る / 出ない**ことを確かめる（04 §11.4 の 2 の単体テストの slt 版）:

| 設定 | 確かめる EXPLAIN の行 |
|---|---|
| `enable_hashjoin = off` の等値結合 | `Hash Join` が出ない、`Nested Loop` が出る |
| 両方 off の等値結合 | `Nested Loop`（`Materialize` が内側） |
| `enable_hashagg = off` | `GroupAggregate` の下に `Sort` |
| `enable_indexscan = off` | `Index Scan` が出ない |
| `enable_seqscan = off`（候補あり） | `Index Scan using ...` |
| `enable_material = off` | `Nested Loop` の内側に `Materialize` が出ない |
| FULL JOIN（等値） | 設定に依らず `Hash Full Join` |

### 3.4 再起動・クラッシュをまたぐテスト（`tests/restart/m4/`）

#### 3.4.1 `tests/run.sh` の拡張（K4）

M2・M3 の仕組み（1 ファイル = 1 フェーズ、フェーズごとにランナーを起動し直し、フェーズの間でサーバを再起動または `kill -9`）を保つ。**追加するのは 3 つだけ**（D11-5）。

```text
tests/restart/m4/<シナリオ>/
├── mode                 # 1 行: restart | crash | both | mixed（なければ both）
├── yuzhu.args           # M2 のとおり（yuzhu のサーバの起動オプション。例: --shared-buffers 1MB）
├── yuzhu.only           # M3 のとおり（あれば pg では飛ばす）
├── NN-<名前>.slt        # フェーズ
├── NN-<名前>.after.sh   # M3 のとおり（そのフェーズの後、サーバが止まっている間に実行。yuzhu だけ）
└── NN-<名前>.mode       # 新規: このフェーズの「後」の停止の方法 restart | crash（mixed のシナリオだけが使う）
```

| `mode` | 意味 | 実行される場所 |
|---|---|---|
| `restart` | フェーズの間を fast shutdown → 起動で渡る | `tests/run.sh --restart` |
| `crash` | フェーズの間を `kill -9` → 起動（クラッシュリカバリ）で渡る | `tests/run.sh --crash` |
| `both` | 期待値が同じ。どちらでも通る | **両方**（`--restart` と `--crash`） |
| `mixed` | フェーズごとに `NN-<名前>.mode` で指定（08 の `seq-mixed-restart`） | `--crash` の中で 1 回だけ |

`tests/run.sh` の変更:

1. `--restart` の既定の探索先を `tests/restart`（直下のシナリオ）に加えて `tests/restart/m4`（`mode` が `restart` または `both` のシナリオ）にする。`--crash` の既定の探索先を `tests/restart/m3` に加えて `tests/restart/m4`（`crash` / `both` / `mixed`）にする。明示のパスの指定は従来どおり（`mode` を無視して全部流す）。
2. `restart_server` が呼び出し側から停止の方法を受け取る（`mixed` のシナリオは `.mode` を読む。それ以外はフラグ）。
3. `mode` ファイルの値の検査（不正な値は `exit 2`）。シナリオの先頭で `echo "== scenario $dir (N phases, mode=$m)"`。

PostgreSQL 側の `kill -9` は M3 の `tests/pg.sh crash`（docker なしでは `sandbox/pg.sh` の PostgreSQL の postmaster を kill -9 して `pg_ctl start`）。**注意（M3 から）**: 同じ PostgreSQL を他のエージェントが使っている最中に流さない。

#### 3.4.2 シナリオの一覧

テーブル名の接頭辞は `m4r_<短縮>_`。最後のフェーズで DROP する。**ユーザーオブジェクトの整合の確認**として、`both` / `crash` のシナリオの最後のフェーズは `consistency` の SQL（07 §8.3 の条件 1〜9）を流す（`tests/slt/_include/consistency.slt.part` を `include` する。`include` の相対パスの基準が未検証のため、使えなければ `slttools consistency`（§3.2.8）が各シナリオの最後のフェーズの末尾に機械的にコピーする）。

| シナリオ | `mode` | 内容 | 出典 |
|---|---|---|---|
| `idx-committed` | both | 表・PK・UNIQUE・`CREATE INDEX` をコミット → 再起動 → カタログ（`pg_index` `pg_constraint` `pg_depend`）、索引走査の結果、`\di` の SQL、重複の INSERT が `23505`、さらに INSERT・UPDATE・DELETE して再起動 → 結果が一致 | 07 §8.2 |
| `idx-rollback` | both | `BEGIN; CREATE INDEX; CREATE TABLE ... PRIMARY KEY; ROLLBACK` → 再起動 → カタログに残らない、同じ名前で再作成できる | 07 |
| `idx-drop-commit` | both | `DROP INDEX` をコミット → 再起動 → 索引がなく表のデータは無事、再作成できる | 07 |
| `truncate-commit` / `truncate-rollback` | both | 索引つきの表の TRUNCATE のコミット / ロールバック → 再起動 → 空 / 元のデータ。`enable_seqscan` の on / off の件数が一致 | 07 |
| `drop-table-cascade` | both | 他の表の DEFAULT が使うシーケンスを持つ表を `DROP TABLE ... CASCADE` → 再起動 → 全カタログの整合、残った表の `atthasdef = f` | 07 |
| `idx-persist` | both | 索引つきの表の等値・範囲・一意性が、停止・再起動・`kill -9` の後も保たれる | 06 §7.14 |
| `idx-steal-crash` | crash（`yuzhu.args`: `--shared-buffers 1MB`） | プールより大きい表（PK・UNIQUE・副索引つき）へ `INSERT ... SELECT generate_series` → `CHECKPOINT` → 全行 UPDATE（キーを変える UPDATE を含む）→ `kill -9` → リカバリ後、全走査と索引走査（`SET enable_seqscan = off`）の件数と合計が一致、重複の INSERT が `23505`。索引のページが steal された後のクラッシュを通す | **11 で追加** |
| `ddl-in-flight` | crash | 大きな表への未コミットの `CREATE INDEX` と `ALTER TABLE ADD PRIMARY KEY` の途中で `kill -9` → 索引がない、データ無事、同じ名前で再作成できる、整合 | 07 |
| `unique-after-crash` | crash | PRIMARY KEY 付きの表へ多数の INSERT / UPDATE をコミットして `kill -9` → 重複が `23505`、行数と索引走査の件数が一致 | 07 |
| `alter-add-commit-crash` | crash | `ALTER TABLE ADD PRIMARY KEY` をコミット直後に `kill -9` → 制約と索引が残り、重複が拒否される | 07 |
| `seq-clean` | restart | `nextval` ×3 → 再起動 → `4`。`CACHE 5` → `6`。`serial` → 次の `id = 3`。`setval` → `101`。`ALTER SEQUENCE ... RESTART WITH 7` → `7` | 08 §7.2 |
| `seq-crash` | crash | `nextval` ×3 → クラッシュ → `34`。`CACHE 5` → `38`。`serial` → `id = 34`。`setval(.., 100)` をコミット → クラッシュ → `101` | 08 |
| `seq-checkpoint-crash` | crash | `nextval` ×3 → `CHECKPOINT` → `nextval` → クラッシュ → `37`。ほか 2 つ | 08 |
| `seq-double-crash` | crash（3 回） | クラッシュのたびに 33 ずつ進む（`34` → `67` → `100`） | 08 |
| `seq-create-rollback` | crash | `BEGIN; CREATE SEQUENCE s; nextval; ROLLBACK` → クラッシュ → `s` がない。`CREATE SEQUENCE` をコミット → 次は `34`。`DROP SEQUENCE` → ない。`BEGIN; CREATE TABLE t (id serial); ...; ROLLBACK` → `t` も `t_id_seq` もない | 08 |
| `seq-mixed-restart` | mixed | `01-*.mode` = `restart`、`02-*.mode` = `crash`: 正常停止の後（`r1 = 4`、`r2 = 6`、`rt` の `id = 3`）に immediate 停止 → `r1 = 37`、`r2 = 43`、`rt` の `id = 36` | 08 |

#### 3.4.3 isolation の追加（`tests/isolation/specs/`。K4）

M3 の 6 本（`writer-waits-writer` ほか）に 2 本を足す（D11-14）。どちらも PostgreSQL で期待値を作る（M3 の規則）。**読み手が書き手を待たない**ことと、**値が戻らない**ことが要点で、M3 の既知の差（別の行への同時書き込みを待つ。m3-tx-semantics §5.6）に当たらない。

```text
# index-reader-writer.spec
setup    { CREATE TABLE iso_ix (id int PRIMARY KEY, v int); CREATE INDEX iso_ix_v ON iso_ix (v);
           INSERT INTO iso_ix SELECT g, g % 10 FROM generate_series(1, 200) g; }
teardown { DROP TABLE iso_ix; }
session w
step w_begin    { BEGIN; }
step w_ins      { INSERT INTO iso_ix SELECT g, g % 10 FROM generate_series(201, 3000) g; }
step w_upd      { UPDATE iso_ix SET v = v + 100 WHERE id <= 50; }
step w_commit   { COMMIT; }
step w_rollback { ROLLBACK; }
session r
step r_set      { SET enable_seqscan = off; }
step r_idx      { SELECT count(*), sum(v) FROM iso_ix WHERE v BETWEEN 0 AND 9; }
step r_pk       { SELECT count(*) FROM iso_ix WHERE id BETWEEN 1 AND 3000; }
permutation r_set w_begin w_ins w_upd r_idx r_pk w_commit r_idx r_pk
permutation r_set w_begin w_ins w_upd r_idx r_pk w_rollback r_idx r_pk
```

```text
# seq-nonblocking.spec
setup    { CREATE SEQUENCE iso_sq; CREATE SEQUENCE iso_sq5 CACHE 5; }
teardown { DROP SEQUENCE iso_sq; DROP SEQUENCE iso_sq5; }
session a
step a_begin { BEGIN; }
step a_next  { SELECT nextval('iso_sq'), nextval('iso_sq5'); }
step a_roll  { ROLLBACK; }
session b
step b_next  { SELECT nextval('iso_sq'), nextval('iso_sq5'); }
step b_last  { SELECT lastval(); }
permutation a_begin a_next b_next a_roll b_next b_last
```

期待: 2 つ目の `nextval` は待たず（`a_next` の後に `b_next` が即座に進む）、`a_roll` の後も値が戻らない（`iso_sq` は 1, 2, 3。`iso_sq5` は a が 1、b が 6、その次 7）。**`yuzhu-isolation` の待ちの判定（`--blocking-detection timeout`）が「待たなかった」を正しく判定すること**を、K が最初に確かめる（M3 の `reader-not-blocked.spec` と同じ形）。

### 3.5 互換テスト（`tests/compat/`。K4）

構成と比べ方は 10 §7.4・§8.3 のとおり。この章が決めるのは**実行の仕組み**と、10 の記述の食い違い（C-12）の整理。

```text
tests/compat/
├── README.md        サーバが受けた文の記録（log_statement = 'all'）と、新しいツールへの対応の手順
├── run.sh           tests/compat/run.sh --target pg|yuzhu [--update] [suite ...]（suite = psql | copy | pgbench。省略は全部）
├── lib.sh           接続先の起動（pg: sandbox/pg.sh か tests/pg.sh、yuzhu: tests/yuzhu.sh）、psql / pgbench の版の検査、正規化
├── psql/            dt.sql dn.sql di.sql l.sql（任意: dtplus.sql d_tbl.sql）と expected/*.out
├── copy/            *.sql（COPY のデータを \. で含む psql スクリプト）と expected/*.out
└── pgbench/         init.sh（-i -s 1 と -i -I dtGvp -s 1）、run.sh（-c 4 -T 30 -M simple。CI は -T 10）、invariants.sql、crash.sh（§3.5.3）
```

#### 3.5.1 実行の規則

- **使うデータベースは `postgres`（pg と yuzhu で同じ）**。10 §7.4 の `createdb compat_psql` と、`compat/psql` の「複数スキーマ」は **yuzhu の M4 では動かない**（`CREATE DATABASE` と `CREATE SCHEMA` は M5。KD-29。C-12）。テーブルは各スクリプトの先頭で `compat_*` を作り、末尾で DROP する。**他のテストの残りが `\dt` に混ざらないよう、`tests/compat/run.sh` は新しいサーバ（pg は `pg.sh stop && start`、yuzhu は `yuzhu.sh clean && start`）で流す**。
- **psql と pgbench は 17 系**でなければ失敗する（`lib.sh` が `psql --version` を検査する）。CI は PGDG の `postgresql-client-17` を入れる（§3.11）。psql は `-X -q`、`PGHOST` `PGPORT` `PGUSER` `PGDATABASE` で接続先を切り替える。
- **比べ方**: `psql` の標準出力を正規化して `expected/*.out` と `diff`。正規化は 10 §7.4 のとおり（`\l` の `Name` と `Owner` 以外、`Access privileges`、`\dt+` の `Size`、OID、時刻）。期待出力は `run.sh --target pg --update` が PostgreSQL に流して作り直す（レビューしてコミット）。
- **ロール名**: `\dt` の `Owner` が PostgreSQL 側と同じ（`postgres`）になるよう、PostgreSQL の起動を `POSTGRES_USER=postgres`（M1 から）にそろえる（10 §12 の未検証）。

#### 3.5.2 `copy/` のケース（10 §8.3）

`\t` `\N` `\\` `\x41` `\101` の復元、`\r\n` の行末、`\.` の途中終了、`end-of-copy marker corrupt`、`literal newline found in data`、列数の過不足、列リスト、`NULL 'NA'` と `DELIMITER '|'`、旧構文の `WITH NULL AS`、`COPY ...; SELECT` の続き、FREEZE の可否（`begin; truncate; copy ... freeze; commit`）、`HEADER`、NOT NULL・CHECK・UNIQUE・型変換のエラーの `CONTEXT`、`\copy t from file`。出力は `COPY n` とエラーの `ERROR:` `DETAIL:` `CONTEXT:`（OID と時刻は正規化）。

#### 3.5.3 pgbench のクラッシュ試験（`pgbench/crash.sh`。D11-15）

```text
1. pgbench -i -s 1                                  # 主キー付きの 4 表。COPY・TRUNCATE・ALTER TABLE ADD PRIMARY KEY を通る
2. pgbench -c 4 -T 5 -M simple &                    # 更新と INSERT の負荷
3. 1〜3 秒のランダムな時刻に kill -9（pg: tests/pg.sh crash。yuzhu: tests/yuzhu.sh crash）→ 起動を待つ
4. 検査（SQL）:
   a. select (select sum(abalance) from pgbench_accounts), (select sum(tbalance) from pgbench_tellers),
             (select sum(bbalance) from pgbench_branches), (select sum(delta) from pgbench_history)   → 4 つが等しい（原子性）
   b. select count(*) from pgbench_accounts → 100000（初期化の COPY が失われていない）
   c. select count(*) - count(distinct aid) from pgbench_accounts → 0（主キーの一意性）
   d. 主キーの索引走査と全走査の件数が一致（set enable_seqscan = off / on）
5. 手順 2〜4 を 5 回繰り返す（同じデータで続ける）
```

PostgreSQL にも同じ検査が通る（`kill -9` の後のリカバリで `4a` が成り立つ）。pgbench が接続切断で終了コード 1 になるのは想定内（`|| true`）。

### 3.6 クラッシュ試験 層 1（`yuzhu-core/tests/crash_sim/`。R2）

M3 §7.5 の仕組み（`TestCluster` が `SimVfs` の上で起動、1 スレッドで複数の `Session` を順に操作、I/O の通し番号 N でクラッシュ、`DropUnsynced` / `KeepAll` / `RandomSubset` / `TornSectors`、確定と不明の記録、`YUZHU_SIM_SEED` / `YUZHU_CRASH_AT`）をそのまま使い、**ワークロード 6〜8、不変条件 I13〜I16、変異テスト**を足す（00 §18 の 6・7 と I13〜I15 に 8 と I16 を足す。D11-8）。M3 の `crash_sim` はまだ足場（`invariants.rs` `model.rs` `mutation.rs` `workload.rs` は空）なので、**M3 の T が先に仕上がること**が R2 の前提。

#### 3.6.1 ワークロード

| # | 名前 | `shared_buffers` | 中身 | 出典 |
|---|---|---|---|---|
| 6 | `indexed_table` | 24 | `t(k int PRIMARY KEY, s text, v int)` + `UNIQUE INDEX (s)` + `INDEX (v DESC NULLS FIRST)`。連番・ランダムなキーの INSERT（`s` は 700 バイトで分割を頻発）、非キー列とキーの UPDATE、DELETE、ROLLBACK、実行中のまま残すトランザクション、**既存の行に `CREATE INDEX`（33 ページ以上。一括構築の 32 ページごとのレコードを通す）**、k 文ごとの `CHECKPOINT` | 06 §7.11 |
| 7 | `sequences` | 16 | `t7(id serial, who int)`（PK なし。重複を検査する）、`sq_a`（CACHE 1）、`sq_c`（CACHE 5）、`sq_i`（INCREMENT 3 START 10）、`sq_s`（`setval` 専用）。部品 A〜J（自動コミットの `nextval`、`serial` への INSERT、`BEGIN; nextval ×k; COMMIT`、ROLLBACK、**他セッションの未 flush の `SEQ_LOG` に依存する払い出し（F）**、`setval`、`CHECKPOINT`、`CREATE / DROP SEQUENCE`、`ALTER SEQUENCE RESTART`）。セッション 3 つ | 08 §7.4 |
| 8 | `ddl_mix` | 24 | `CREATE TABLE`（PK / UNIQUE）→ `INSERT` → `CREATE INDEX` → `UPDATE` → `DROP INDEX` → `TRUNCATE` → `DROP TABLE` を、コミットとロールバックを混ぜて繰り返す。`ALTER TABLE ADD PRIMARY KEY` と `serial` つきの表も混ぜる。k 文ごとの `CHECKPOINT` | 07 §8.5 |

**網羅**: 小さい版（I/O が数百回）はクラッシュ点 N を 0 から最後まで全部（`DropUnsynced` と `KeepAll`）。大きい版は固定シード（CI は 24 個、夜間は 480 個）で N と `RandomSubset` / `TornSectors { 512 / 4096 }` を選ぶ。**CI の合計時間は release ビルドで 120 秒以内を目安**（M3 の 60 秒に 3 ワークロード分）。

#### 3.6.2 不変条件（リカバリ後に `invariants.rs` の関数で検査。M3 の I1〜I12 に加えて）

| 不変条件 | 内容 | 定義の持ち主 |
|---|---|---|
| **I13** | すべてのインデックスで `check_against_heap(check_unique_live = true)` が通る: 索引の全項目の TID 集合 = ヒープの「索引されるべき版」（`xmin` が中断でないすべての版）。キーが一致し、`unique` の索引に生きている版のキーの重複がない | 06 §6.2 |
| **I14** | すべてのインデックスで `check_structure` が通る（分割の途中の状態が見えない = 原子性） | 06 §6.2 |
| **I15** | シーケンスは払い出した値を二度払い出さない: (1) `nextval` の結果 > 確定した最大、(2) 欠番の上限 `<= max_returned_any + (SEQ_LOG_VALS + cache) * increment`、(3) `t7.id` に重複なし、(4) `last_value` と `log_cnt` の形、(5) ファイルとカタログの整合、(6) ページの `verify` と `magic` | 08 §7.4 |
| **I16**（11 で追加） | カタログの整合: 07 §8.3 の `check_catalog`（条件 1〜9。リカバリ後。条件 10（孤児ファイル）は M3 の D15 によりクラッシュ後は検査しない）。加えて「コミットした DDL が見え、コミットしていないものが見えない」（ワークロード 8 のモデルとの一致） | 07 §8.3 |

全ワークロードの終わりで `pool.pinned_frames() == 0`。各検査器の**負のテスト**（壊れた木・カタログを見逃さない）は各章の単体テストにある（06 §7.13、07 §8.4）。

#### 3.6.3 変異テスト（`mutation.rs`。ハーネスが壊れを検出できることの確認）

各変異について「既定のシード集合のうち少なくとも 1 つで検出する」ことをテストにする（検出できなければテスト失敗）。**どの不変条件にも、それを検出する変異が最低 1 つある**ことを表で保証する。

| 変異 | 方法 | 検出されるべきもの | 出典 |
|---|---|---|---|
| FPW なし（M3 の既存） | `disable_full_page_writes` + `TornSectors`（`BTREE_PAGES` は `FORCE_IMAGE` で影響を受けず `BTREE_INSERT_LEAF` の葉が壊れる） | I14 または起動時の `XX001` | M3、06 |
| WAL-before-data を破る（既存） | `skip_wal_before_data` + `DropUnsynced` | I13 または I14 | M3、06 |
| REDO の LSN 判定を外す（既存） | `redo_ignore_page_lsn` | I14（`insert_item_at` の二重適用） | M3、06 |
| コミットで flush しない（既存） | `skip_commit_flush`（`finish_without_xid` も省く）+ `DropUnsynced`、部品 A のみ | I15（1 または 4） | M3、08 |
| 分割の原子性を壊す（任意） | `DebugKnobs::btree_split_in_two_records`（分割の連鎖を 2 本の `BTREE_PAGES` に分ける）+ `DropUnsynced` | I14 | 06-Q16 |
| PostgreSQL と同じ穴（シーケンス） | `DebugKnobs::seq_ignore_foreign_wal`（`SeqRun.wal_lsn` を自分が書いた分だけにする）+ `DropUnsynced`、部品 F | I15（1） | 08 |
| `SEQ_LOG` の REDO が LSN を見て飛ばす | `DebugKnobs::seq_redo_skip_if_page_newer` + チェックポイントをまたぐ | I15 または I8 | 08 |
| `force_log` を使わない | `DebugKnobs::seq_no_force_log` + `CHECKPOINT` + `DropUnsynced` | I15（1。重複） | 08 |
| **索引項目の取りこぼし**（11 で追加） | テスト専用の `LossyIndexStore`（`Arc<dyn IndexStore>` を包み、N 回に 1 回 `insert` を黙って捨てる。`StorageStack.index` の差し替えだけで済み、本番のコードに印を足さない）、ワークロード 6 | I13（`heap.missing_entry`） | 11 |
| **カタログの取りこぼし**（11 で追加） | リカバリ後に `CatalogStore` で `pg_depend` の行と `pg_index` の行を 1 つずつ消す（コミットする）→ `check_catalog` が各条件で検出する | I16（条件 2・4・5） | 07 §8.4、11 |

**M3 の `DebugKnobs` に足すもの**（A。既定は無効で、コマンドライン・設定ファイル・SQL からは変えられない。M3-Q13）: `btree_split_in_two_records`（任意）、`seq_ignore_foreign_wal`、`seq_redo_skip_if_page_newer`、`seq_no_force_log`。

**ハーネスへの要求**（M3 の T / `testing.rs` の持ち主へ）: (1) `TestCluster` がワークロードごとの `shared_buffers` を受け取る、(2) `StorageStack.index` を差し替えられる（`LossyIndexStore`）、(3) `pool()`（`check_structure` が `&Arc<BufferPool>` を取る）と、`IndexHandle` をカタログから引く補助、(4) ワークロードが SQL（索引・シーケンスの DDL を含む）を `Session` で流せる。ワークロード 7 の `shared_buffers = 16`、8 の `24` はこの章の仮決め（06 は 6 の 24 だけを決めている）。

#### 3.6.4 R2 が足す型と関数（契約の形）

署名の細部は M3 の T が作ったハーネスに合わせる。R2 が追加する公開の形は次のとおり（`yuzhu-core/tests/crash_sim/`）。

```rust
// invariants.rs（M3 の I1〜I12 の隣）
pub struct InvariantViolation { pub id: &'static str /* "I13" など */, pub detail: String }
/// I13 + I14: カタログにあるすべての索引（ユーザーテーブルの）に check_against_heap（check_unique_live = true）を走らせる
pub fn check_indexes(c: &TestCluster) -> Result<(), InvariantViolation>;
/// I15: 08 §7.4 の 6 条件。model は払い出した値の記録（確定・不明・ロールバックを含む最大値、setval / ALTER の下限）
pub fn check_sequences(c: &TestCluster, model: &SeqModel) -> Result<(), InvariantViolation>;
/// I16: 07 §8.3 の check_catalog（条件 1〜9）+ モデル（存在すべき DDL の結果）との一致
pub fn check_catalog_state(c: &TestCluster, model: &DdlModel) -> Result<(), InvariantViolation>;

// workload.rs
pub struct WorkloadSpec { pub name: &'static str, pub shared_buffers: usize, pub kind: WorkloadKind /* Small | Large */ }
pub fn indexed_table(seed: u64, kind: WorkloadKind) -> Box<dyn Workload>;     // ワークロード 6
pub fn sequences(seed: u64, kind: WorkloadKind) -> Box<dyn Workload>;         // 7
pub fn ddl_mix(seed: u64, kind: WorkloadKind) -> Box<dyn Workload>;           // 8

// mutation.rs
/// Arc<dyn IndexStore> を包み、insert を every 回に 1 回、Ok(()) を返して内側に渡さない（それ以外は委譲）
pub struct LossyIndexStore { inner: Arc<dyn IndexStore>, every: u64, count: AtomicU64 }
impl LossyIndexStore { pub fn new(inner: Arc<dyn IndexStore>, every: u64) -> Self; }
impl IndexStore for LossyIndexStore { /* 00 §13.2 の全メソッド。insert 以外は inner へ委譲 */ }
```

環境変数は M3 のまま（`YUZHU_SIM_SEED`、`YUZHU_CRASH_AT`）。失敗したら `seed`、`mode`、`N`、ワークロード名、**違反した不変条件の ID と `detail`** を出す。

### 3.7 差分ランダムテスト（00 の `yuzhu-fuzz-sql` = `tests/tools/difftest`）

**決定（D11-6、C-5）**: 00 §4 の `yuzhu-fuzz-sql`（ワークスペースの新しい bin）は**作らず**、`tests/tools/difftest` を M4 向けに仕上げて使う。

#### 3.7.1 現状（実際に確かめたこと）

- `tests/tools/difftest/`: 独立した Cargo プロジェクト（`impl/rust` のワークスペースの外。依存は `clap` と `postgres`）。約 4,300 行（`ast.rs` `case.rs` `db.rs` `generate.rs` `main.rs` `rng.rs` `schema.rs`）。サブコマンド `run` / `replay` / `gen`。自前の乱数（xoshiro256**）で**同じシードから同じ SQL**。機能レベル `m1` / `m4` / `m5`（`m4` は JOIN・集約・副問い合わせ、`m5` は小数リテラル）。TLP（`rows(Q) = rows(Q WHERE p) ⊎ rows(Q WHERE NOT p) ⊎ rows(Q WHERE p IS NULL)`）。失敗したケースの**最小化**と、そのまま流せる SQL スクリプト・`reproduce:` 行の出力。
- **ただし、コミットされている状態は `cargo check` が通らない**（実測: `src/db.rs` だけ書き換え済みで、`case.rs` / `main.rs` が未更新。`CmpOpts` の `eval_order` フィールドと `compare` の `Traits` 引数で 5 件のエラー）。`tests/tools/difftest/HANDOFF.md` が残りの作業（`ast.rs` の `zero_ambiguous` / `eval_order_sensitive`、`case.rs` の `Pair.inconclusive`、`main.rs` の `--timeout` と `--eval-order-errors`、テスト、README）を列挙している。**Z の最初の作業はこれを終えること**（0.5 日）。

#### 3.7.2 PostgreSQL を正解とする比較の規則（`HANDOFF.md` の知見を設計に取り込む）

| 規則 | 理由 |
|---|---|
| 結果は多重集合として比べる（`ORDER BY` が全出力列を並べるときだけ順序も比べる）。`LIMIT` は全順序の `ORDER BY` があるときだけ生成 | ハッシュ結合・集約の出力順は不定 |
| `-0` と `0` の入れ替わりは許容する（`DISTINCT` `GROUP BY` `min` `max` `ORDER BY .. LIMIT` スカラー副問い合わせ） | どちらが残るかが格納順やプランで変わる（実測。`HANDOFF.md` §1） |
| エラーは SQLSTATE を比べる（`--error-match exact\|class\|any`）。**「エラーが出るかどうか」が評価順で変わる式**（クラス 22 のエラーと行の結果、またはクラス 22 どうしでコードが違う）は inconclusive として数える（`--eval-order-errors skip`） | PostgreSQL は最上位の AND をコスト順に並べ替え、`x AND FALSE` を畳み込む。yuzhu は 04 の R1 で畳み込むが順序は違う（`HANDOFF.md` §2。`enable_*` を変えた実験で誤検出 4 件） |
| 文のタイムアウト（既定 10 秒）。超えたら `Outcome::Lost`、接続し直す | yuzhu がハングしても止まらないように（`HANDOFF.md` §3） |
| 参照側が C ロケールでなければ警告 | `'a' < 'B'`、`upper('é')` が変わる |
| 浮動小数の `sum` / `avg` は生成しない。float は NaN と境界値を入力に使うが演算の連鎖は短く | 加算順で結果が変わる |
| **生成する SQL は yuzhu が対応する範囲だけ**（`0A000` になるものは生成しない）。§3.2.7 の KD-1〜KD-27 の構文を生成器が避ける | 差分が「未対応」で埋まらないように |

#### 3.7.3 M4 で足す生成と検査（Z。4.5 日）

| 項目 | 内容 | 日数 |
|---|---|---|
| 仕上げ | `HANDOFF.md` の残作業。`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`（difftest 自身の単体テスト）を通す | 0.5 |
| スキーマ | `PRIMARY KEY` / `UNIQUE` / `CREATE INDEX`（単一・複合・`DESC` / `NULLS FIRST`）を持つ表。型に `numeric(p,s)` `char(n)` `date` `timestamp` を追加（浮動小数と同様、演算は誤差の出ないものだけ）。`serial` 列 | 1.0 |
| 問い合わせ | JOIN の連鎖（INNER / LEFT / RIGHT / FULL（等値）/ CROSS、USING / NATURAL）、`GROUP BY` / `HAVING`（主キーへの関数従属を含む）、`DISTINCT ON`、`NOT IN` / `IN` / `EXISTS` / スカラー / `ANY` / `ALL`（NULL を含む表。**相関あり、UNION の腕からの相関を含む**）、`UNION` / `INTERSECT` / `EXCEPT`（`ALL` の有無）、CTE（参照 0・1・2 回、`MATERIALIZED`）、`generate_series` | 1.0 |
| 検査（オラクル） | (a) **PostgreSQL との差分**（結果と SQLSTATE）、(b) **TLP**（WHERE / 集約の `min` `max` `count` `sum`（整数） / GROUP BY / HAVING / DISTINCT）、(c) **NoREC**: `SELECT count(*) FROM t WHERE p` と `SELECT sum(CASE WHEN p THEN 1 ELSE 0 END) FROM t`（`p` が NULL でも同じ）が一致、(d) **プラン変種**: 同じ問い合わせを `enable_*` のプロファイル（§3.3.1）ごとに yuzhu で流し結果が同じ、(e) **索引の有無**: 同じ表を索引つきとなしで作り（`CREATE INDEX` の後 / 前）、等値・範囲・ORDER BY の結果が同じ | 0.7 |
| DML 列 | `INSERT` / `UPDATE` / `DELETE`（`UPDATE ... FROM` を含む）をランダムな順で実行し、**一意制約の違反（`23505`）・NOT NULL・CHECK のエラーも含めて**両方に流し、各文の後の表の内容（`SELECT * ORDER BY`）と SQLSTATE を比べる。トランザクション（`BEGIN` / `ROLLBACK`）を混ぜる | 0.5 |
| 運用 | CI の `fuzz-pr`（固定シード）と夜間、失敗の成果物の保存（§3.7.4）、`KNOWN.md` | 0.3 |
| 合計 | | **4.5**（00 の 4 日より +0.5 = 仕上げ分） |

**difftest の CLI に足すもの**（契約。既存の `--level` `--seed` `--queries` `--enable` `--disable` `--error-match` `--out` はそのまま）:

```text
--level m4                 M4 の生成（既存の m4 に、下の機能を足したもの）
--enable  indexes,numeric,chars,dates,setops,ctes,distinct-on,quantified,correlated,dml   機能の個別の切り替え（既存の joins / aggregates / subqueries / tlp に加えて）
--oracle  diff,tlp,norec,plan,index      使う検査（既定は diff,tlp。norec / plan / index は yuzhu 単体の検査）
--plan-profiles default,no_hashjoin,...  --oracle plan のプロファイル（§3.3.1。既定は全部）
--dml-steps N              DML 列モード: 1 ラウンドあたりの文の数（既定 0 = 無効）
--timeout SECS             文のタイムアウト（既定 10）。HANDOFF.md の残作業
--eval-order-errors skip|report   評価順で変わるエラーの扱い（既定 skip）。HANDOFF.md の残作業
```

#### 3.7.4 実行

```sh
tests/pg.sh start; (cd impl/rust && cargo build --release -p yuzhu-server); tests/yuzhu.sh start --port 5433
cd tests/tools/difftest && cargo build --release
target/release/difftest run --level m4 \
  --ref  "host=127.0.0.1 port=55432 user=postgres dbname=postgres" \
  --test "host=127.0.0.1 port=5433  user=postgres dbname=postgres" \
  --seed 1 --queries 2000 --out /tmp/difftest-fail
```

- **固定シード（PR ごと。`fuzz-pr`）**: `--seed 1..8`、各 `--queries 2000`（合計 16,000 問い合わせ。2〜3 分）。`--level m4` と、DML 列のモード、プラン変種のモード。差分があれば失敗。
- **夜間（`nightly`）**: シードを日付から決め（`--seed $(date +%Y%m%d)`）、`--queries 200000`。失敗したシードをログに残す。連続 7 回で未解決の差分 0 件が完了条件の 1 つ（§1.3 の 6）。
- **失敗時**: 最小化され、`fail-<seed>-<round>-<n>.sql`（そのまま流せるスクリプト。先頭のコメントに `reproduce:` のコマンド、`reason:`、両サーバの結果）が `--out` に出る。CI は `--out` をアーティファクトとして保存する。**再現手順**: `difftest replay --ref ... --test ... fail-*.sql`（または `psql -f`）、元のラウンド全体は `reproduce:` の行（`--seed S --start-round R --rounds 1 --level m4 ...`）。
- **失敗の分類**（レビュー時）: (1) yuzhu のバグ → 修正して、最小化された SQL を `tests/slt/m4/` の該当ファイルにケースとして追加、(2) 未対応（`0A000`）→ 生成器が避けるようにする、(3) PostgreSQL の非決定性 → 比較の規則（§3.7.2）に足す、(4) 既知の差 → `tests/tools/difftest/KNOWN.md` に ID と原因を書く（生成器が避けられないものだけ）。

### 3.8 PostgreSQL 回帰テストの取り込み

**方針（D11-7）**: PostgreSQL 17 の `src/test/regress/sql/{select,select_distinct,select_distinct_on,select_having,select_implicit,join,aggregates,subselect,union,with,case,btree_index,numeric,char,varchar,date,timestamp,timestamptz,strings,int4,int8,float8,boolean,text,create_index,truncate,sequence,identity,copy}.sql` から、**yuzhu が対応する範囲の文だけ**を、`tests/imported/pg_regress/<元のファイル名>/` に slt として置く。手で選ばず、スクリプトが「PostgreSQL と yuzhu の結果が一致した文」を機械的に選ぶ。PostgreSQL License（出典とライセンスの表記を各ファイルの先頭に残す）。

```text
tests/tools/slttools/src/pgregress/  `slttools pgregress`（Rust。`postgres` クレートと psql）
├── README.md
├── import.rs                     <元の .sql> → 文の分割 → 実行 → 一致した文を slt として書く
├── deny.txt                      取り込まない構文の正規表現（lateral、over (、grouping sets、rollup、cube、with recursive、
│                                 interval、array[、::json、xml、\d メタコマンド、create function、create extension、inherits ...）
└── setup/                        test_setup.sql の変換（COPY ... FROM ファイル → \copy ... FROM STDIN の形）とデータファイル
tests/imported/pg_regress/<名前>/<連番>.slt   生成物（手で編集しない）
tests/imported/pg_regress/REPORT.md          一致 / 不一致 / 除外の件数と、不一致の文の一覧（yuzhu のバグ候補の作業リスト）
```

**取り込みの手順**（`slttools pgregress import`）:

1. **土台**: `test_setup.sql`（`tenk1` `onek` `int4_tbl` `road` ほか）を変換して流す。`COPY ... FROM :'filename'` は **`\copy ... FROM 'data/...'`（クライアント側のファイル）に書き換え**る（yuzhu の COPY は STDIN だけ。psql の `\copy` が STDIN に変える）。土台が yuzhu で作れない表（配列・interval・geometric 型の列）は、その列を除いた版を `setup/` に置く。
2. **文の分割**: `;` で区切る（文字列・ドル引用・コメント・`E''` を考慮）。psql のメタコマンドの行は飛ばす。
3. **除外**: `deny.txt` に一致する文、`create function` ほか `0A000` になる DDL。
4. **PostgreSQL に流して結果を取る**（行の結果は `query ... rowsort`、`ORDER BY` が最上位にあれば順序つき。エラーは SQLSTATE）。**非決定的な文**（`random()`、`now()`、`pg_sleep`、`ctid`、OID、`EXPLAIN` の出力、`pg_relation_size`）は除外。
5. **yuzhu に流して結果を比べる**。一致した文だけ slt に書く。**不一致は `REPORT.md` に文と両方の結果を書く**（yuzhu のバグか、未対応か、PostgreSQL の非決定性かを人が分類する）。
6. 土台の表を変える文（`CREATE TABLE` / `INSERT` / `UPDATE` の連鎖）は、元のファイルの**順序を保って**同じセッションで流す（後続の文がその結果に依存する）。1 つの元のファイル = 1 つの slt のディレクトリ（連番のファイル）。

**実行**: `tests/run.sh --target {pg,yuzhu} tests/imported`（夜間だけ。`tests/slt` の外なので既定の実行に入らない）。PostgreSQL では常に全件通る（取り込みの定義）。yuzhu で落ちたものは、(a) バグ → 直す、(b) 取り込みの前提の誤り（土台の違い）→ `deny.txt` / `setup/` を直して再生成、(c) 既知の差 → `deny.txt`。**合否の条件にしない**（§1.3）が、`REPORT.md` の不一致の件数の推移を `PROGRESS.md` に残す。工数 約 2 日（K3。00 の K の内数ではなく追加。§4.5）。

**SQLite の sqllogictest のコーパス**（`research-slt.md` §2.1。`select1-5`、`random/`）は M5 の宿題とする（§8）。

### 3.9 Rust の単体テストの一覧（章ごと）

各章の「テスト」節がテスト名の持ち主。ここは**どのモジュールにどんなテストが置かれるか**の索引（実装者が自分の範囲のテストを見落とさないための一覧）。

| 章 | 場所 | 主なテスト（章の節） |
|---|---|---|
| 02 | `expr/walk.rs` | `walk` の訪問順、`try_map` の往復・葉の変換漏れ、`same_as`、`lower_single_rel`（§6.1） |
| 02 | `planner/{logical,physical}.rs`、`analyzer/query.rs` | `validate` の各規則（L1〜L10、P1〜P11、B1〜B11）を 1 か所だけ壊して `XX000`、`width` / `children` / `uses_params`（§6.1） |
| 02 | `executor/**` | ノードごとの `rewind`、`rewind_reuses_or_rebuilds`、割り込み（葉は `next` ごと、長いループ）、`Update` の位置だけの入力、スタブの `0A000`（`stubs_return_not_supported`）（§6.2） |
| 02 | `planner/mod.rs`（P0-e の間だけ） | `new_path_matches_legacy`（旧経路と新経路の約 60 文の結果が一致）。通ったら旧経路ごと削除 |
| 03 | `sql/parser/tests_query.rs` | WITH / FROM の関数 / FILTER / ANY・ALL / COLLATE / OPERATOR / 行値 / GROUP BY の特殊形、位置、`0A000` の文言（§6.1） |
| 03 | `analyzer/tests_{from,agg,sub}.rs` | RTE と RteId、`levels_up`、USING / NATURAL、集約の解決・禁止位置・グループ化の検査・関数従属、SubLink の形、集合演算の型、CTE、出力名（§6.2） |
| 04 | `yuzhu-core/tests/plan_golden/cases/*.golden` | `scan` `const_fold` `join` `join_order` `sublink` `pullup` `outer_join` `pushdown` `agg` `setop` `cte` `dml` `explain_tree`（§11.1） |
| 04 | `planner/rules/*`、`planner/mod.rs` | ルールごとに変換する例としない例、R7（結合順序）、プラン変種（`enable_*` の組で期待するノードの有無）、`validate`、計画時間（8 表で release 50ms）と再帰の深さ（`54001`）（§11.2〜§11.6） |
| 05 | `executor/nodes/*`、`executor/{agg,mem,subplan,dml}.rs`、`types/hash.rs` | ノード単体（`FakeStore` / `FakeIndexStore` / `RowsExec` / `CountingExec` / `InterruptAfter`）、**結合アルゴリズムの相互比較**（HashJoin 全 kind × `build_is_left` × residual と NLJ と素朴な 3 重ループ。シード 200）、SubPlan の三値論理の全組み合わせ、DML のメッセージ、`MemBudget` の課金と解放、割り込み、`hash_datum` の性質（§10） |
| 06 | `storage/btree/**`、`storage/heap/**`、`catalog/opclass.rs` | 固定値（§3.6 のバイト例）、`54000` の境界（2692 / 2693）、proptest（`BTreeMap` モデル。7 スキーマ）、分割の網羅、REDO の同値性と冪等、一意性、一括構築と逐次挿入の同値、並行する分割と読み手（`HookPoint` で刻む + 複数スレッドの試験 1 つ）、opclass の表の完全性（8 / 15 / 120 / 24）、スキャンの境界・NULL・DESC、検査器の負のテスト、H4（§7） |
| 07 | `catalog/{rows,store,cache,reader,depend,naming,check}.rs`、`ddl/*`、`yuzhu-core/tests/ddl_m4.rs`、`ddl_error_fields.rs` | 行の生成、`load_table_def`、`plan_drop`、命名規則、`resolve_opclass`、DDL の統合テスト（ファイル数が元に戻る、ROLLBACK、TRUNCATE の旧ファイル）、エラーのフィールド（§8.4） |
| 08 | `storage/sequence.rs`、`catalog/seq_params.rs`、`executor/seq.rs`、`ddl/sequence.rs`、`analyzer/ddl.rs` | `plan_fetch`（参照実装との突き合わせ。数十万ケース）、ページとタプルの固定値、WAL・REDO（無条件上書き・冪等）、`SeqStore` の並行（N スレッド × M 回で重複なし）、`SeqSession` / `SeqRuntime`、`finish_without_xid`、`init_params` の全行、SERIAL / IDENTITY の解析（§7.3） |
| 09 | `types/{typmod,datum,hash,cmp,regex,sys,io,numeric,datetime,bpchar}.rs`、`storage/heap/tuple.rs`、`catalog/builtin.rs` | `typmod_in` / `apply_typmod`、`cmp_datum` の新分岐、`hash_datum` と `HashKey`、`cmp_with_nulls`、正規表現（§9.4 の全エラー・§9.2 の全行）、`regclass_in` / `regtype_in`、入出力の往復、ディスク形式のバイト例（numeric `ndigits` 59 の境界）、`OPERATORS` / `AGGREGATES` が実機の `pg_operator` / `pg_proc` と一致、**`numeric_bridge_corpus`**（`yuzhu-numeric` の約 3.5 万行のコーパスを `Datum` 経由でも流す）、橋渡しの差分テスト（日時）（§12.3） |
| 10 | `deparse/*`、`explain/*`、`copy/*`、`executor/instrument.rs`、`planner/explain_tree.rs`、`yuzhu-server/tests/copy_protocol.rs` | deparse（10 §4.9 の表の全行を `Stored`（pretty / 非 pretty）と `Plan`）、`format.rs`（字下げ・コスト欄・ANALYZE の 3 形・`never executed`）、instrument（`loops` / `rows` の平均）、`explain_tree`（10 §3.3 の表を 1 行 1 テスト）、COPY の `LineReader` の**チャンク分割の同値性**（性質テスト）、`split_fields` / `unescape`、`CopyIn`、`Session` の COPY の状態機械の全辺、プロトコル（実際の TCP）、`INERT_GUCS`（§8.4） |
| yuzhu-numeric / yuzhu-datetime | 既存の `tests/pg_corpus.rs` と `pg17_*.tsv`、`fixtures/zoneinfo` | **変更しない**（09 §12.1）。M4 では `yuzhu-core` 側の橋渡しの差分テストが同じコーパスを流す |

### 3.10 CI（`.github/workflows/ci.yml` と `nightly.yml`。K4）

M3 までのジョブ（`rust` `slt-pg` `slt-yuzhu` `docker` `restart-pg` `restart-yuzhu` `isolation` `crash-sim`）を**残し、次を足す・変える**。ゲートは D11-9: **pg 側は常に必須、yuzhu 側はフェーズ 4 の開始（§4.4）まで `continue-on-error: true`**。

| ジョブ | 内容 | ゲート | 目安の時間 |
|---|---|---|---|
| `rust` | 既存。M4 で `plan_golden`、proptest（256 ケース）が加わる | 必須 | 〜10 分 |
| `slt-lint`（新） | `slttools lint`（`tests/slt` `tests/restart`）、`slttools plan-variants check`。サーバ不要 | 必須 | 〜1 分 |
| `slt-pg` | `tests/run.sh --target pg`（m1〜m4）。**PostgreSQL の環境をそろえる**（`--locale=C --encoding=UTF8`、`TimeZone` は slt の先頭で固定）。最後に `z_final/` | 必須 | 〜5 分 |
| `slt-yuzhu` | `tests/run.sh --target yuzhu`（m1〜m4）。**サーバを `-c yuzhu.validate_plans=on` で起動する別のステップ**（02-Q7: `validate` を全 slt に効かせる）を足す | フェーズ 4 から必須 | 〜10 分 |
| `restart-pg` / `restart-yuzhu` | `--restart`（`tests/restart` + `tests/restart/m4` の restart / both）、`--crash`（`tests/restart/m3` + `tests/restart/m4` の crash / both / mixed） | pg 必須 / yuzhu はフェーズ 4 から | 〜10 分 |
| `isolation` | 既存（M3 の 6 本 + M4 の 2 本）。pg / yuzhu のマトリクス | pg 必須 | 〜3 分 |
| `compat-pg` / `compat-yuzhu`（新） | `tests/compat/run.sh --target pg\|yuzhu`（psql・copy・pgbench。pgbench は `-T 10`）。`postgresql-client-17` を入れる。pg は `expected/` が最新かの確認を兼ねる | pg 必須 / yuzhu はフェーズ 4 から | 〜10 分 |
| `crash-sim` | `cargo test --release -p yuzhu-core --test crash_sim`（ワークロード 1〜8、固定シード、変異テスト。**120 秒以内**） | 必須（M3 から） | 〜5 分 |
| `fuzz-pr`（新） | §3.7.4 の固定シード（pg と yuzhu を起動して `difftest run --level m4`） | pg 側のジェネレータの自己検査（pg 対 pg、差分 0）は必須 / yuzhu 対 pg はフェーズ 4 から | 〜10 分 |
| `docker` | 既存。**`tzdata` を入れたイメージでの smoke test** を足す（`SET TIME ZONE 'Asia/Tokyo'; SELECT now()` が成功） | 必須 | 〜5 分 |
| 夜間 `nightly.yml`（新） | (1) 層 1 の長時間ランダム実行（シード 480）と proptest 8192 ケース（`#[ignore]`）、(2) 層 2 の `cargo test -p yuzhu-server --test crash_kill9 -- --ignored`（`serial` 列つき。20 回 → 200 回）、(3) `difftest` を `--queries 200000`、(4) `pgbench -c 4 -T 300` と `crash.sh` を 20 回、(5) `tests/imported` の取り込みと実行、(6) 計画時間・性能の測定（`PROGRESS.md` に追記する数値を出す） | 参考（失敗は issue 化。連続 7 回 0 件が完了条件の 1 つ） | 〜2 時間 |

**M3 の CI への追加の要約**: (a) `slt-yuzhu` と `restart-yuzhu` と `isolation(yuzhu)` と `crash-sim` の対象に m4 が加わる、(b) 新しいジョブ `slt-lint` `compat-*` `fuzz-pr` `nightly`、(c) `slt-pg` に `z_final` が加わり、`slt-yuzhu` の起動に `-c yuzhu.validate_plans=on`、(d) `docker` ジョブに tzdata の確認、(e) `continue-on-error` の外し方（フェーズ 4 で yuzhu 側を外す。1 つずつ: slt → restart → isolation → compat → fuzz）。

### 3.11 環境要件

| 項目 | 要件 | 変更する場所 |
|---|---|---|
| **tzdata** | `Asia/Tokyo` と `America/New_York` を使う日時のテスト（09 §12.2）には `/usr/share/zoneinfo`（TZif）が要る。tzdata がないと地域名が `22023 time zone "..." not recognized`。**3 か所に明示する**: (1) `Dockerfile` の runtime ステージ（`apt-get install -y --no-install-recommends tzdata`。現在は何も入れていない）、(2) `sandbox/Dockerfile` の `apt-get install` の一覧（現在は postgresql-17 の依存としてたまたま入る）、(3) GitHub の `ubuntu-latest`（同梱。ジョブの先頭で `test -d /usr/share/zoneinfo/Asia` を確かめて失敗させる） | `Dockerfile`、`sandbox/Dockerfile`、`ci.yml`（K4。00 §17 の K の範囲に足す。§9 の P11-2） |
| tzdata の版 | PostgreSQL 17 が使う版と yuzhu が読む版の違いでゾーンの境界が変わりうる。テストは `Asia/Tokyo`（DST なし）と `America/New_York` の**最近の年**だけ（09 §14） | テストの規則 |
| psql / pgbench / pg_dump | **17 系**（`psql` が送る SQL が版で変わるため。yuzhu は 17.0 を名乗る）。コンテナ（sandbox）には 17.11 が入っている。CI は PGDG（`apt.postgresql.org`）の `postgresql-client-17`（`ubuntu-latest` 同梱のクライアントは 17 より古い可能性が高い。未検証） | `ci.yml`、`tests/compat/lib.sh` の版の検査 |
| PostgreSQL | `postgres:17`（C ロケール、UTF8、trust）。CI のサービスコンテナと `tests/pg.sh` / `sandbox/pg.sh`。`max_connections` の既定で足りる（`-c 4`） | M1〜M3 のまま |
| sqllogictest-bin | **0.29.1、`--locked`**（`tokio-postgres` の版でエラー文言が変わる） | M1 のまま |
| Python | **使わない**。sandbox のイメージには `python3` も `uv` もない（実測: `which python3 uv` が空）。テストの補助ツール（lint・plan_variants・大きなキーの生成・回帰テストの取り込み）は Rust の `tests/tools/slttools`（§3.2.8）。09 §12.1 の `gen_bpchar_corpus.py` / `gen_agg_corpus.py` も T2 / T3 が `psql` + シェルまたは Rust で書く（`sandbox/Dockerfile` に `python3` を足してもよい。その場合は 09 のままでよい） | `tests/tools/slttools`、`sandbox/Dockerfile`（任意） |
| Rust | `rust-toolchain.toml` の stable。`tests/tools/difftest` と `tests/tools/isolation` は独立した Cargo プロジェクトで、それぞれ別にビルドする（CI のキャッシュのワークスペースに両方を並べる） | `ci.yml` |
| ディスクとメモリ | pgbench `-s 1`（100,000 行）、`generate_series(1, 1000000)` のメモリ上限テスト（`yuzhu.query_mem_limit` を 1MB にして実行）、`idx-steal-crash`（プール 1MB で数万行）が動く程度。ジョブのメモリは 7GB（GitHub の既定）で足りる | — |
| ロケール・時刻の固定 | slt の先頭で `SET TIME ZONE 'UTC'` と `SET DateStyle = 'ISO, MDY'`。`LC_ALL=C`（`tests/run.sh` の `sort` はすでに C）。`PGTZ` は使わない | テストの規則（§3.2.1 の 4） |

---

## 4. 実装の分担と工数（全体）

各担当は**自分の範囲のファイルだけ**を編集する（M1〜M3 と同じ）。他の担当の範囲で直すべき点は自分では直さず依頼する。例外は `error.rs` の `sqlstate` への定数の追記だけ。作業中も crate 全体がコンパイルできる状態を保つ。この節は 00 §17 の担当表を、各章の分担表と突き合わせて確定した版（00 の表は M4 の最初の見積り。以後はこの節を正とする）。

### 4.1 担当表の確定

日数は AI の実装エージェント 1 本の日数（粗い見積もり）。「00」= 00 §17、「章」= 各章が自分の分担表に書いた日数、「確定」= この章の決定。00 の表の合計は **115.5 日**（00 の本文は「約 120 日」）。

| 担当 | 00 | 章の見積り | 確定 | 範囲（編集するファイル。00 から変わる点だけ太字） | 章 |
|---|---|---|---|---|---|
| **A 基盤** | 1.5 | 02: 1.5（02 の分は約 1.0。他章の型が加わる） | **2.0** | 00 §17 のとおり。**追加**: `Cast.implicit`（10）、`CastMethod::Env`（09）、`ExplainNode` の拡張（10、§7.1 C-1）、`TypeEnv.names` / `OidNames`（09）、`DebugKnobs` の 4 項目（06、08）、`sqlstate` の 11 定数（§7.2）、`PhysicalQuery::single` / `empty`、`BoundQuery::walk_exprs`（03）、`FnKind::Set`・`SessionValueKind`（09） | 02 |
| **P0 パイプライン移行** | 5 | 02: 5.0（P0-b 0.5、P0-c 1.8、P0-d 1.4、P0-e 1.3） | 5.0 | 00 のとおり。**段階ごとに担当を解放する**（02 §5.1: P0-a → S1・H4・B1・T・K、P0-b → B2・C1、P0-c → X1〜X3、P0-d → N1〜N3、P0-e → L1・L2・E1）。P0-b が `StorageStack.index` / `seq` と `Cluster::indexes()` / `sequences()` を足す（02-P14）。**`yuzhu-fuzz-sql/` のスタブは置かない**（D11-6）。`catalog/builtin.rs` の区画のコメントを置く（§4.2） | 02 |
| **S1 パーサ** | 3 | 03: 1.8、10: 1.0（07・08・09 の構文は章が日数を書いていない） | **5.0** | `sql/*` の M4 分すべて。内訳: 03 の分 1.8、10（EXPLAIN・COPY の構文）1.0、07（CREATE INDEX・ALTER TABLE・TRUNCATE・VACUUM・WITH オプション）1.0、08（シーケンス・IDENTITY・OVERRIDING）0.8、09（型名・日時キーワード・`SessionValueKind`）0.4（07・08・09 の分はこの章の見積り）。順序: 03 → 07 → 08 → 10 → 09 | 03、07〜10 |
| **N1 解析: FROM** | 4 | 03: 4 | **4.3** | 00 のとおり。**P0 の後の `analyzer/{expr,coerce}.rs` の持ち主**（T1・T2・T3・Q1 の依頼を受ける。+0.3）。`analyzer/dml.rs` の IDENTITY の規則の呼び出し（08 §4.9）と 42809（07） | 03 |
| **N2 解析: 集約** | 4 | 03: 4 | 4.0 | 00 のとおり（`agg.rs`、`select.rs` の `analyze_select`）。`analyze_query` の骨格は P0-d が置く | 03 |
| **N3 解析: サブクエリ** | 5 | 03: 5 | 5.0 | 00 のとおり（`sublink.rs` `setop.rs` `cte.rs`）。`select.rs` には触らない（§4.2） | 03 |
| **L1 論理プラン** | 6 | 04: 約 6.5（任意 0.3 を含む） | **6.2**（+0.3 任意） | `planner/{build,rules/*,util,print(論理),validate(論理),mod(plan・PlanTrace)}.rs`、`rules/testutil.rs`。`yuzhu-core/tests/plan_golden/` は L1 と L2 の共同 | 04 |
| **L2 物理化** | 5 | 04: 約 6.8（必須 5.9、任意 0.9）。10: explain_tree に 2 日 | **6.9**（+0.9 任意） | `planner/{physicalize,index_select,size,explain_tree,print(物理),validate(物理)}.rs`。explain_tree は 04 が 1.0、10 が 2.0 と見積もったので 2.0 を採る（必須 5.9 + 1.0）。任意: 内側 Index Scan の Nested Loop（0.5）、ソートの省略（0.4） | 04、10 |
| **X1 実行: 結合** | 5 | 05: 5 | 5.0 | 00 のとおり。**P0 の後の `executor/eval.rs` の持ち主**（SubLink の分岐。T2 の `Cast(Env)` と `session_value` は P0-c が実装済み） | 05 |
| **X2 実行: 集約** | 5 | 05: 5 | 5.0 | 00 のとおり | 05 |
| **X3 実行: 索引と DML** | 4 | 05: 4 | 4.0 | 00 のとおり（`dml.rs` の `RowChecker` / `RowBuilder` を O1 が使う。C-10） | 05 |
| **H4 ヒープ拡張** | 1.5 | 06: 1.5 | 1.5 | `storage/heap/*`、`heap_store.rs`、`page.rs`（**追加のみ**。`init_special` ほか。06 §4.2）。最初の 0.3 日で `page.rs` の追加を出す（B1・Q1 が使う） | 06 |
| **B1 B+Tree 本体** | 6 | 06: 6 | 6.0 | 00 のとおり | 06 |
| **B2 B+Tree 走査・構築** | 5 | 06: 5 | 5.0 | 00 のとおり。**最初に `opclass.rs` の表を出す**（L2・C1 が使う。06 §8 の進め方 2） | 06 |
| **M3 の持ち主への依頼** | — | 06: C 0.3 | **0.6** | M3 の C: `PinnedBuffer::{read_tree, write_tree}`、`BufferPool::extend_tree`（06 §4.3）、`page_mut_hint()` が呼んだ時点で dirty（08）。M3 の W2 / R: `wal/dump.rs` の `describe` 呼び出し、`recovery::dispatch` の `Btree` / `Seq`、`Wal::redo_lsn()` が起動直後から値を返す（06・08）。M3 の A: `DebugKnobs` の追加（A の内数） | 06、08 |
| **C1 カタログと DDL** | 7 | 07: 7.0 | **8.0** | 00 のとおり + `catalog/{naming,depend,check}.rs`、`ddl/vacuum.rs`、`analyzer/ddl_constraint.rs`（07）。**加えて `analyzer/ddl_index.rs`（新）: CREATE INDEX / DROP INDEX / DROP TABLE（behavior）/ TRUNCATE / VACUUM / ALTER TABLE の解析（AST → `Bound*`。07 §4.6 が型を決めたが、解析を書く担当が 00 にも 07 にもなかった。§7.3 の G-1）。+1.0** | 07 |
| **Q1 シーケンス** | 5 | 08: 5.0 | **5.3** | 00 のとおり + `catalog/seq_params.rs`、`executor/seq.rs`、`txn/manager.rs` の追加（`wal_flush_upto` ほか。00 §16 のとおり 08 が持ち主）。**`catalog/builtin.rs` の `sequence` 区画に `nextval` ほかの 5 行を足す**（09 に書かれていない。§7.3 の G-2）。`analyzer/ddl.rs` の `resolve_type_name` / `KNOWN_UNSUPPORTED_TYPES`（09 の依頼。ファイルの持ち主として。+0.3） | 08、09 |
| **T1 numeric** | 3 | 09: 3 | 3.0 | 00 のとおり | 09 |
| **T2 日時と char(n)** | 4 | 09: 4 | 4.0 | 00 のとおり。`settings.rs` の `DateTimeSettings` と `Settings::type_env`（S と共同。区画を分ける。§4.2） | 09 |
| **T3 関数・集約・正規表現** | 5 | 09: 5 | 5.0（+0.3 任意） | 00 のとおり。**`CatalogNames` は `catalog/reader.rs`（C1）ではなく新しい `catalog/names.rs` に置く**（09 は `reader.rs` の末尾と書いたが、そのファイルの持ち主は C1）。最初に `AGGREGATES` を出す（N2 が使う）。任意: `pg_size_pretty` `pg_table_size` `obj_description` `pg_function_is_visible`（`\dt+` `\df`。10 の依頼。G-3） | 09 |
| **E1 EXPLAIN と deparse** | 5 | 10: 5（pretty を含めて 6） | **6.0** | `explain/*`、`deparse/*`、`executor/instrument.rs`。`pg_get_constraintdef` / `pg_get_indexdef` の関数の行と本体（G-2）。**pretty を作る**（D10-5） | 10 |
| **O1 COPY** | 4 | 10: 4 | 4.0 | `copy/*`、`analyze_copy` | 10 |
| **S セッション** | 4 | 10: 3（COPY の状態 1.5、`exec_explain` 0.5、`INERT_GUCS` 1.0） | **4.5** | 00 のとおり + 07 §6.11（DDL の振り分け、`in_transaction_block`）、08（`SeqSession` の配線、`end_statement`）、09（`TypeEnv` の組み立て）、04・02（`PlannerSettings`・`yuzhu.validate_plans`）。10 の 3.0 + 約 1.5 | 02、04、07〜10 |
| **J サーバ** | 1.5 | 10: 1.5 | 1.5 | 00 のとおり | 10 |
| **R2 クラッシュ試験の追加** | 3 | 06・07・08 のワークロードと不変条件 | **5.0** | `yuzhu-core/tests/crash_sim/*`。ワークロード 6・7・8、I13〜I16、変異テスト（§3.6）。ハーネスの追加要求（§3.6.3）。内訳: WL6 + I13・I14 1.5、WL7 + I15 + 変異 1.5、WL8 + I16 1.0、ハーネスと変異の整備 1.0。**前提: M3 の T がハーネスを仕上げていること** | 06〜08、11 |
| **K 共有テスト** | 10 | 03: 3、10: 3（残りは章が日数を書いていない） | **24.0**（K1 3.0 + K2 7.5 + K3 7.0 + K4 6.5。§4.5） | `tests/slt/m4/*`、`tests/compat/*`、`tests/restart/m4/*`、`tests/isolation/specs/*`、`tests/tools/slttools/*`、`tests/gen/*`、`tests/imported/*`、`tests/run.sh`、`.github/workflows/*`、**`Dockerfile`・`sandbox/Dockerfile`（tzdata。D11-10）** | 02〜10、11 |
| **Z 差分ランダムテスト** | 4 | — | **4.5** | **`tests/tools/difftest/*`**（00 の `yuzhu-fuzz-sql/` ではない。D11-6） | 11 |
| 合計 | 115.5 | | **140.3**（任意を含めて 141.8） | | |

**増えた理由**（00 との差 +24.8 日）: K（00 の 10 日は各章が書いたテストの量に足りない。+14）、S1（07・08・10 の構文が加わった。+2）、R2（3 つのワークロード。+2）、L2（explain_tree。+1.9）、C1（索引・TRUNCATE・VACUUM の解析。+1）、E1（pretty。+1）、A（+0.5）、S（+0.5）、Z（difftest の仕上げ。+0.5）、N1・Q1・L1（+0.8）、M3 の持ち主への依頼（+0.6）。**最長経路は変わらない**（§4.3）ので、期間はほぼ 00 の見込みのまま（約 21〜23 日）。

### 4.2 ファイル所有の衝突の確認

00 §17 の範囲は互いに重ならない。各章が**ファイルを足し、他の担当へ依頼を書いた**ことで、次の重なりが生じた。決めた規則を表にする。**この表にないファイルは、00 §17（または §4.1 の確定表）の持ち主だけが編集する**。

| ファイル | 触る担当 | 規則 |
|---|---|---|
| `catalog/builtin.rs` | T1（numeric）、T2（日時・bpchar）、T3（集約・集合返却・regclass・sys）、Q1（シーケンス 5 行）、B2（B+Tree の比較関数 24 行）、E1（`pg_get_*`）、C1（`pg_table_is_visible` の 1 関数）、L1（`operator_by_oid` は T3 が足す） | **区画（region）で分ける**: P0-b が `// ===== region: <名前> (<担当>) =====` のコメントで空の区画を置く（`numeric (T1)` `datetime-bpchar (T2)` `agg-set-sys (T3)` `sequence (Q1)` `btree-procs (B2)` `explain-deparse (E1)` `misc (C1)`）。**各担当は自分の区画の中だけを編集する**。`PROCS`（`pg_proc` の行）は T3 の `tools/gen_procs.sh`（`psql` で PostgreSQL 17 から生成）が一括して作る。レビューは「他の区画を触っていないか」を見る |
| `types/ops.rs` | T1（numeric・float8 丸め・整数 `mod`）、T2（bpchar）、Q1（シーケンス関数 5 つ）、T3（`pg_typeof` ほか） | 区画で分ける（`catalog/builtin.rs` と同じ規則。P0-b が置く） |
| `types/io.rs` | T2 | T2 だけ（numeric の入出力は T1 の `types/numeric.rs` の関数を T2 が呼ぶ） |
| `types/mod.rs`、`types/datum.rs` | A（型）、T3（`cmp_datum`） | A が先。T3 は A のマージの後に `cmp_datum` の分岐だけを足す |
| `catalog/reader.rs` `cache.rs` `store.rs` | C1 | C1 だけ。**T3 の `CatalogNames` は `catalog/names.rs`（新）に置く**（C-17） |
| `analyzer/ddl.rs` | Q1（SERIAL・IDENTITY・シーケンスの解析）、**型名の処理（09）は Q1**。呼び出し側: C1 の `ddl_constraint.rs` と `ddl_index.rs`、N1（INSERT / UPDATE から IDENTITY の規則関数） | **Q1 が持ち主**。C1 は別ファイル（`ddl_constraint.rs`、`ddl_index.rs`）に書き、`analyze_create_table` の呼び出し 1 行だけ Q1 に依頼する |
| `analyzer/{expr,coerce,resolve}.rs` | P0（P0-d まで）、以後 N1。依頼元: T1（リテラル）、T2（日時の経路・`coerce_typmod`）、T3（`pg_typeof`・`"any"`）、N2・N3、Q1 | **P0-d のマージ後は N1 が持ち主**。依頼は N1 へ（0.3 日を見込んである）。09 §13.1 の「P0 / N 担当」を N1 に確定 |
| `analyzer/select.rs` | N2（`analyze_select`）。`analyze_query` の骨格は P0-d | N3 は触らない（`setop` `cte` `sublink` の関数の中だけ。03 §7 の N3 の「`analyze_query` の統合 0.5 日」は P0-d が置いた分岐の動作確認）。N3 が分岐に手を入れたいときは N2 に依頼（C-20） |
| `executor/eval.rs` | P0（P0-c）、以後 X1。依頼元: T2・E1・X2 | P0-c が `CastMethod::Env`・`apply_cast`・`session_value`・`eval_with_sub_row` まで実装する。以後 X1 |
| `executor/{mod,build}.rs` | A（型）、P0（骨格）。X1〜X3 は**触らない** | `build.rs` の `match` は P0-c が全変種を `nodes::<名前>::build` への 1 行で置く。X1〜X3 は自分のノードのファイルの `build` だけを書く |
| `executor/instrument.rs` | E1。各ノードの `set_counters`（X1〜X3 の各ノードのファイル） | E1 が型と `Instrumented` を持つ。X1〜X3 は自分のノードに `set_counters` を実装（C-2） |
| `types/hash.rs` | P0（P0-b の本実装: 現在の変種）、以後 X2 | X2 が numeric・bpchar・日時を足す |
| `settings.rs` | S（`INERT_GUCS`、`planner_settings`、`validate_plans`）、T2（`DateTimeSettings`、`type_env`、`SET TimeZone` / `DateStyle` の検証） | 関数ごとに分ける。S が先にマージし、T2 は関数の追加だけ |
| `session.rs`、`engine.rs`、`testing.rs` | S。P0（`plan` の呼び出しの差し替えだけ）、P0-b（`StorageStack` / `Cluster` の取得口） | S が持ち主。Q1・C1・O1・E1 は**依頼**（S の 4.5 日に配線を見込んである） |
| `storage/page.rs` | M3 の C（既存）、H4（追加のみ） | H4 は**メソッドの追加だけ**（既存を変えない） |
| `storage/buffer/*` | M3 の C | 06 の依頼（§4.1 の「M3 の持ち主への依頼」）を C が入れる |
| `txn/manager.rs` | M3 の E、Q1（`Transaction.wal_flush_upto` ほか。00 §16） | Q1 は M3 の E のマージの後に**フィールドと `finish_without_xid` の追加だけ** |
| `wal/dump.rs`、`recovery.rs` | M3 の W2 / R | B1・Q1 が各 2 行の依頼 |
| `debug_knobs.rs` | A | 4 項目を A が足す（§3.6.3） |
| `error.rs` | A | `sqlstate` への追記だけは誰でも（§7.2 の表の 11 個。重複して足さない） |
| `sql/ast.rs`、`sql/parser/*` | S1 | 07・08・09・10 は AST の**形を書いた**が、実装は S1 だけ |
| `tests/run.sh`、`.github/workflows/*`、`Dockerfile`、`sandbox/Dockerfile` | K4 | K の範囲に足す（D11-10） |
| `tests/slt/m4/<ディレクトリ>` | K1〜K3 | ディレクトリで分け、互いのディレクトリを編集しない。`KNOWN-DIFFS.md` は K4 が持ち、K1〜K3 は**ID の追加を K4 に依頼**（または自分の ID を末尾に追記だけ） |
| `yuzhu-core/tests/crash_sim/*` | R2（M3 の T が作ったハーネスを延長） | M3 の T の完了後 |
| `yuzhu-core/tests/plan_golden/` | L1 と L2 | `cases/` は観点ごとのファイルで分ける（04 §11.1）。`main.rs` `fixture.rs` は L1 |
| 新しいファイル（00 §4 にないもの） | — | `catalog/{seq_params(Q1),naming(C1),depend(C1),check(C1),names(T3)}.rs`、`executor/seq.rs`(Q1)、`ddl/vacuum.rs`(C1)、`analyzer/{ddl_constraint(C1),ddl_index(C1),tests_from(N1),tests_agg(N2),tests_sub(N3)}.rs`、`sql/parser/tests_query.rs`(S1)、`planner/{util(L1),size(L2),print(L1/L2),validate(L1/L2)}.rs`、`planner/rules/testutil.rs`(L1)、`storage/btree/testing.rs`(B1)、`types/typmod.rs`(T2)、`tests/tools/slttools/*`(K4)。**00 §4 に反映する**（§9 の提案 P11-2） |

### 4.3 依存の順序・並列度・最長経路

日を A の開始 = 0 として数える（各章の見積りと、02 §5.1 の「段階ごとの解放」、06 §8 の進め方に従った。担当が別のエージェントなので待ちが生じないとしたときの理想値）。

| 担当 | 開始 | 終了 | 開始の条件・順序 |
|---|---|---|---|
| A | 0 | 2.0 | なし |
| S1 | 0 | 5.0 | A の AST の型は S1 自身のファイル。03 → 07 → 08 → 10 → 09 の順（N1〜N3 が P0-d の後に使うので 03 を先に） |
| K1〜K4 | 0 | 〜21 | PostgreSQL だけで書ける。yuzhu での通過確認はフェーズ 3 から |
| Z | 0 | 〜14 | まず difftest の仕上げ（0.5）。PostgreSQL 対 PostgreSQL の自己検査で生成器を進め、yuzhu 対 PostgreSQL はフェーズ 3 から |
| P0-b / c / d / e | 2.0 / 2.5 / 4.3 / 5.7 | 2.5 / 4.3 / 5.7 / 7.0 | A の後。**M3 が `dev` に統合済みで、F・S が `eval.rs`・`session.rs` を変更中でないこと**（02 §7） |
| H4 | 2.0 | 3.5 | A。最初の 0.3 日で `page.rs` |
| B1 | 2.3 | 8.3 | H4 の `page.rs`、M3 の C の依頼 |
| B2 | 2.0 | 7.3 | `opclass.rs`（2.0〜3.0）→ `check.rs` → `build.rs` → `scan.rs` → `unique.rs`（B1 の各段の後） |
| T1 / T2 / T3 | 2.0 | 5.0 / 6.0 / 7.0 | A。T3 は `AGGREGATES` を最初（2.0〜2.5。N2 が使う） |
| E1（deparse） | 2.0 | 〜7.5 | A の `Expr<C,Q>`。`explain` の整形と `instrument` は P0-c の後。`explain_tree` との結合は L2 の後（〜14.0） |
| O1 | 2.0 | 8.2 | `LineReader` は独立。`CopyIn` は P0-d と X3 の `dml.rs` の後 |
| Q1 | 2.0 | 9.0 | `storage/sequence.rs` は独立（2.0〜3.5）。`analyzer/ddl.rs` は P0-d の後、`ddl/sequence.rs` は C1 の `CatalogStore` / `depend` の後 |
| C1 | 3.0 | 11.0 | P0-b と B2 の `opclass.rs`。`analyzer/ddl_index.rs` は P0-d の後 |
| X1 / X2 / X3 | 4.3 | 9.3 / 9.3 / 8.3 | P0-c。X2 は T1 の numeric（5.0）、X3 は B1・B2（実物は 8.3。それまで `FakeIndexStore`） |
| N1 / N2 / N3 | 5.7 | 10.0 / 9.7 / 10.7 | P0-d。N2 は T3 の `AGGREGATES`、N1 は C1 の `primary_key()`（N2 の関数従属） |
| L1 / L2 | 7.0 | 13.2 / 13.9 | P0-e。L2 のインデックス選択は B2 の `opclass.rs`（3.0 で済み） |
| S | 2.0 | 〜13.5 | `INERT_GUCS` と設定は独立。COPY の状態は O1 の `CopyIn`（7.2）の後、`exec_explain` は L2 / E1 の後、DDL の振り分けは C1 の後 |
| J | 3.0 | 〜14.0 | ErrorResponse のフィールドは独立。COPY のプロトコルは S の状態の後 |
| R2 | 8.3 | 〜13.5 | M3 の T のハーネス、B1（WL6）、Q1（WL7）、C1（WL8） |

**最長経路**: A（2.0）→ P0（5.0。〜7.0）→ L2（6.9。〜13.9）→ 結合（フェーズ 4。約 5 日）→ 完了判定（約 2 日）で**約 21〜23 日**（00 の 20〜25 日と同じ）。2 番目に長いのは P0 → N3（〜10.7）→ L1（〜13.2）、3 番目は B1（〜8.3）→ C1 / X3 の結合（〜11）→ R2（〜13.5）。

**並列度**: 最大で約 19 の担当が同時に動く（5.7〜8.3 日。L1・L2・N1〜N3・X1〜X3・B1・B2・C1・Q1・E1・O1・S・T3・K1〜K4・Z）。平均は 140 日 / 21 日で約 6.7。M3 の実装が `dev` に統合される前に P0 を始めない（02 §7）ことが、全体の開始の前提。

### 4.4 フェーズ分け

| フェーズ | 日 | 内容 | 終わりの条件（これが通らないと次へ進まない） |
|---|---|---|---|
| **0 足場** | 0〜2 | A が型と ★ のスタブ。K4 が `tests/run.sh` の拡張・lint・CI の骨組みを PostgreSQL に対して先に作る | 既存の `cargo test`・`tests/run.sh --target yuzhu`（m1〜m3）が変わらず通る |
| **1 基礎** | 2〜7 | P0-b〜e。並列に S1・H4・B1・B2（opclass）・T1〜T3・E1（deparse）・O1（`LineReader`）・Q1（`storage/sequence.rs`）・K1〜K4・Z | **`tests/slt/m1`〜`m3` が yuzhu で通る**（P0 の完了条件。02 §5.1 の各段階の条件）。`validate` が全テストで通る |
| **2 機能** | 4.3〜11 | X1〜X3、N1〜N3、L1、L2、C1、Q1、E1（explain）、O1（`CopyIn`）。**各担当は自分のディレクトリの slt を yuzhu で通す**（進捗は `tests/run.sh --target yuzhu tests/slt/m4/<dir>` で見える） | 各担当の単体テストが通る。結合のない範囲（式・型・単一表の索引なし）の slt が yuzhu で通る |
| **3 結合** | 10〜14 | S・J。索引の実物（X3 と C1 と B1・B2）、シーケンス（Q1 と S）、COPY（O1 と S と J）、EXPLAIN（L2・E1・S）が繋がる。R2 のワークロード、`tests/compat`、Z の yuzhu 対 PostgreSQL。**CI の `nightly` を開始** | `tests/slt/m4` の過半が yuzhu で通る。`pgbench -i` が完走する。`\dt` が動く |
| **4 完成** | 14〜21+ | 失敗の洗い出しと修正（各担当が自分のディレクトリ・機能を直す）。**CI の yuzhu 側を 1 つずつ必須にする**（slt → restart → isolation → compat → fuzz）。夜間を 7 回連続で通す | §1.3 の 1〜9 がすべて通る |

### 4.5 K・R2・Z の分担

**K1〜K4**（D11-11）。全員が PostgreSQL だけで先に書ける。ディレクトリを分け、互いのファイルを編集しない。

| 担当 | 範囲 | 日数 | 内訳 |
|---|---|---|---|
| **K1** | `tests/slt/m4/{join,agg,subquery,setop,cte,dml}`（24 ファイル） | 3.0 | 03 の下書き 11 ファイルを整える（`sq_` → `sb_`、`onlyif` / `skipif` の規約、PostgreSQL での再確認）+ 下書きのない 13 ファイル（05 の `agg/` 7、`cross_inner` `row_in` `in_contexts` `recursive_syntax` `insert_select` `returning`） |
| **K2** | `tests/slt/m4/{catalog,constraint,index,ddl,seq}`（46 ファイル）+ `tests/restart/m4` のシナリオ 17 個 | 7.5 | 07・06・08 のテスト節。`slttools large-keys`（K4）を使う |
| **K3** | `tests/slt/m4/{types,explain,copy,psql,plan_variants,mem}`（46 ファイル）+ `tests/gen/plan_variants/*.tpl` + `slttools plan-variants`（実体は K4 と共同）+ `tests/tools/slttools` の `pgregress` + `tests/imported` | 7.0 | types 1.0、explain・psql・copy 2.5、plan_variants と mem 1.5、pgregress の取り込み 2.0 |
| **K4** | `tests/run.sh` の拡張、`tests/compat/*`（psql・copy・pgbench・crash）、`tests/isolation/specs` の 2 本、`tests/tools/slttools`（`lint` `plan-variants` `large-keys` `consistency`）、`z_final/`、`KNOWN-DIFFS.md`、`tests/README.md`、`.github/workflows/*`、`Dockerfile` と `sandbox/Dockerfile` の tzdata | 6.5 | run.sh 0.5、compat 3.0、slttools 1.5、isolation 0.3、CI と Dockerfile 0.7、KNOWN-DIFFS と README 0.5 |

**K の最初の作業（K4。フェーズ 0）**: (1) 既存の `tests/slt/m1`〜`m3` と `tests/restart` に `slttools lint` を走らせて違反を直す（M2-Q22 の後始末。`z_final/no_leftovers.slt` の前提）、(2) `tests/run.sh` の `mode` 対応、(3) `tests/slt/m4/z_final/` と `KNOWN-DIFFS.md` の骨組み、(4) CI の `slt-lint` ジョブ。K1〜K3 は (1) の後に最初のファイルを置く。

**R2**（5.0）と **Z**（4.5）は §3.6・§3.7。Z は 00 §17 の依存（N1〜N3、X1〜X2）に関わらず、**difftest の仕上げ（0.5）と PostgreSQL 対 PostgreSQL の自己検査はフェーズ 0 から**始められる。

### 4.6 工数の合計と、見積りの根拠

| 区分 | 日数 |
|---|---|
| 基盤とパイプライン（A、P0） | 7.0 |
| 構文と解析（S1、N1〜N3） | 18.3 |
| プランナ（L1、L2） | 13.1 |
| 実行（X1〜X3） | 14.0 |
| ストレージ（H4、B1、B2、M3 への依頼） | 13.1 |
| カタログ・DDL・シーケンス（C1、Q1） | 13.3 |
| 型・関数（T1〜T3） | 12.0 |
| EXPLAIN・COPY・セッション・サーバ（E1、O1、S、J） | 16.0 |
| テスト（R2、K、Z） | 33.5 |
| **合計**（任意を除く） | **140.3**（§4.1 の表の合計と一致。任意の 1.5 日（L1 0.3 + L2 0.9 + T3 0.3）を含めると 141.8） |

見積りの粗さは M3 と同じ（AI の実装エージェント 1 本が、設計書だけを読んで書き、PostgreSQL / 共有テストで確かめるまでの日数）。**不確かさが大きいのは、P0（M1〜M3 のコードの作り直し）、L1・L2（ルール）、K（PostgreSQL での期待値の確認）**。

---

## 5. 未検証の点（実装前に確かめるもの。確かめる順）

各章の「未検証の点」を、重複を除いて集約し、**実装の前に確かめる順**に並べた。(A) は全担当の着手前、(B) はフェーズ 1 の前、(C) は各担当が自分の機能に着手するとき、(D) は結合・計測のとき。出典の章番号は各章の「未検証の点」の項目。「実測」= この章の執筆時に確かめたこと。

### (A) 着手前（全担当の前提。フェーズ 0）

| # | 項目 | 出典 | 確かめる人 |
|---|---|---|---|
| U1 | **M3 が `dev` に統合済みで、`executor/eval.rs`（M3 の F）・`session.rs`（M3 の S）の変更が止まっていること**。P0-c・P0-d は M3 の実装の最中に着手しない | 02 §7 | プロジェクトの管理者 |
| U2 | `tests/tools/difftest` のビルドが通らない（実測）。`HANDOFF.md` の残作業を終える | 11 §3.7.1 | Z（最初） |
| U3 | M3 の `crash_sim`（`invariants.rs` `model.rs` `mutation.rs` `workload.rs`）は足場だけ（実測）。ハーネスが R2 の要求（§3.6.3）を満たすか | 11 §3.6 | R2、M3 の T |
| U4 | sandbox のイメージに `python3` も `uv` もない（実測）。09 の `gen_bpchar_corpus.py` / `gen_agg_corpus.py` の書き方（Rust か `psql` + シェルか、イメージに `python3` を足すか） | 11 §3.11、09 §12.1 | T2・T3、プロジェクトの管理者 |
| U5 | tzdata: 実行用 `Dockerfile` は何も入れていない（実測）。`sandbox/Dockerfile` は postgresql-17 の依存としてたまたま入る。CI の `ubuntu-latest` には入っている。tzdata の版が PostgreSQL 17 の `postgres:17` と違うことによる境界のずれ | 09 §5.3、§14、11 | K4 |
| U6 | CI で PostgreSQL 17 の `psql` / `pgbench` を入手する方法（PGDG の `postgresql-client-17`。`ubuntu-latest` 同梱のクライアントの版は未検証） | 11 §3.11 | K4 |
| U7 | sqllogictest-rs の `include` の相対パスの基準（`tests/slt/_include/consistency.slt.part`）。使えなければ `slttools consistency` でコピー | 11 §3.4.2 | K4 |
| U8 | `Datum::as_str()` を `BpChar` に広げた影響: `Datum::Text` だけを想定している箇所（`planner/mod.rs` `executor/nodes/distinct.rs` `catalog/rows.rs` `catalog/store.rs`）の洗い出し | 09 §14 | A、P0（最初の作業） |

### (B) フェーズ 1 の前（基礎の契約。M3 の持ち主との接続）

| # | 項目 | 出典 | 確かめる人 |
|---|---|---|---|
| U9 | `Wal::redo_lsn()` が `open_at` / `finish_recovery` の直後から制御ファイルの REDO 点を返すこと（0 を返すと再起動後の最初の `nextval` が `SEQ_LOG` を書かない） | 08 §9 | M3 の W1・R、Q1 |
| U10 | `PageWriteGuard::page_mut_hint()` が**呼んだ時点で** dirty になること（08 の D8-7 の順序の前提。M2 版はガードの Drop 時） | 08 §9 | M3 の C |
| U11 | `PageWriteGuard<'a>` が `'a` について共変であること（葉のガードを `Vec` に move する）。だめなら 06 §9 の代案 | 06 §9 | B1（最初のコンパイル） |
| U12 | `Wal::insert` の `FORCE_IMAGE \| STANDARD`（穴を省いた画像）の組み合わせが M3 の W1 の実装で動くこと | 06 §9 | B1 |
| U13 | M3 の C が `read_tree` / `write_tree` / `extend_tree`（06 §4.3）を受け入れること。受け入れられない場合の代案は 06 §9 | 06 §9、06-Q7 | M3 の C、B1 |
| U14 | `TableStore::tuple_state` が自分の挿入（`xmin = own`）を `Live` と返すこと、`begin_scan_all` の `HeapTuple.row` が全ユーザー列を持つこと | 07 §10、06 §7.12 | H4 |
| U15 | `FnKind::Runtime` の strict の扱い（NULL 引数で呼ばれないこと）。引数つきの `Runtime` は `nextval` が初めて | 08 §9 | Q1、P0-c |
| U16 | `Page::init_special` の名前と形（06 と 08 の記述が一致していること） | 08 §9、06 §4.2 | H4、Q1 |
| U17 | 隠し設定 `yuzhu.validate_plans` を CI のサーバ起動オプション（`-c`）で渡せること | 02-Q7 | S、K4 |

### (C) 各担当が自分の機能に着手するとき（PostgreSQL 17 の実機・ソースで）

**カタログ・OID（B2・C1・T1〜T3）**

| # | 項目 | 出典 |
|---|---|---|
| U18 | `pg_opfamily` / `pg_opclass` / `pg_amop` / `pg_amproc` の全行と OID（`pg_opclass` の 10000 番台は版で変わりうる）。24 個の比較関数の `pg_proc` の行 | 06 §9・06-Q13、07 |
| U19 | 型をまたぐ日時の比較演算子の OID（2345〜2350、2358〜2363、2371〜2376、2384〜2389、2534〜2545。M4 では作らない） | 09 §14 |
| U20 | `pg_depend` の PostgreSQL の全行のうち、07 が書かない行（名前空間、CHECK の列）が `DROP` の挙動に影響しないこと | 07 §10 |
| U21 | `pg_type.typmodin` が 0 のままで `format_type` が正しく動くこと。`typarray` が 0 の行が psql の出力に影響しないこと | 09 §3.2 |
| U22 | 孤児ファイル（クラッシュで中断した DDL）を、起動時の掃除なしで OID 採番（`storage_exists` の確認）が避けること（M2-Q24） | 07 §10 |

**DDL・制約・シーケンス（C1・Q1）**

| # | 項目 | 出典 |
|---|---|---|
| U23 | `ALTER TABLE ... OWNER TO` を `pg_class` に対して実行したときの文言。`DROP` の `2BP01` の DETAIL が 100 行を超えるときの文面。`fillfactor = 50.5` の丸め（`rint`）。`deduplicate_items` の不正値。`autovacuum_enabled` などの扱い | 07 §10 |
| U24 | 空の表の索引の `relpages` / `reltuples`（実測 `1` / `0`。yuzhu は `2` / `0`）、`CREATE INDEX` の後の表の `relpages`、`VACUUM` が `relhasindex` を下ろす条件 | 07 §10 |
| U25 | 複数文の Simple Query の途中の `VACUUM` が `25001` になること（実測）を、yuzhu の暗黙のブロック（M3）が同じに扱うこと | 07 §10 |
| U26 | `CREATE INDEX IF NOT EXISTS` の名前の判定と列の検査の順序 | 07 §10 |
| U27 | PostgreSQL の穴（他トランザクションの未 flush の `SEQ_LOG` に依存した払い出し）と、`GetRedoRecPtr` と `MarkBufferDirty` の順序の狭い競合は、ソースの読みによる（`kill -9` では再現できない）。層 1 の変異テストで再現してから塞ぐ | 08 §9 |
| U28 | `ALTER SEQUENCE` を `OWNED BY` だけで呼んだときに状態を触らないこと。IDENTITY のオプション（`OWNED BY none` / `LOGGED`）。`VACUUM s` / `ANALYZE s` の WARNING の文言。`DROP SEQUENCE` の DETAIL の並び（OID 昇順） | 08 §9 |
| U29 | 一意性検査で等値の連続が非常に長いとき（死んだ版が数万件）の性能（`pgbench_branches`）。`_bt_findsplitloc` との性能特性の差。WAL の量（全画像方式） | 06 §9 |
| U30 | `fetch_dirty` の `HeapTupleSatisfiesDirty` の細部（xmin が自分で xmax が他人、`HEAP_XMAX_LOCK_ONLY`、MultiXact）は M4 では起きない。M5 で M3 の可視性と照合 | 06 §9 |
| U31 | `BufferPool` の大きさ: 分割が最大 `3h + 1` ページをピンする。`shared_buffers >= 16` で高さ 5 の最悪が入る。高さ 6 以上は `no unpinned buffers available` | 06 §9 |

**解析・プラン（N1〜N3・L1・L2・P0）**

| # | 項目 | 出典 |
|---|---|---|
| U32 | PostgreSQL 内部の `varlevelsup` の値そのもの（観測できる振る舞いは確認済み）。外側レベルの集約の細部（`count(*)` のように `Var` を含まないもの） | 02 §8、03 §8 |
| U33 | `42703` の「近い名前の HINT」が出る条件。`errorMissingRTE` の DETAIL・HINT の全分岐（RIGHT / FULL JOIN の左で HINT が付かないこと）。集合演算の列数エラーの位置。`WITH ... INSERT` の `levels_up` | 03 §8 |
| U34 | 同じ式の SubLink 同士の GROUP BY との一致（PostgreSQL は一致とみなす。yuzhu は `42803`。KD-21）。`COLLATE` を持つ式の ORDER BY の一致。`FOR UPDATE` を `0A000` にしてよいか（Django の `select_for_update`） | 03 §8、02 §8 |
| U35 | `DISTINCT ON` で `ORDER BY` がないときの NULL の位置。`Hash Right Join` が出る条件。EXPLAIN の `Index Cond` の複数述語の並び。同じ表を 2 度使うときの別名（`t_1`）の一般則 | 04 §13 |
| U36 | `EvalCtx::for_constant_folding` を 05 が提供できること（`planner` が `executor::eval` を 1 か所だけ呼ぶ依存の向きの例外。00 への変更提案 04-P3）。`catalog::builtin::operator_by_oid` の有無。`OperatorMeta.com` が新しい演算子の行にも入っていること | 04 §13 |
| U37 | `operator_strategy` が**交差型**（`int4 < int8`、`text = name`）で `Some` を返すこと。05 の `IndexScan` が「`Eq` の値が NULL なら 0 行」「自分が書いた新しい版を見ない」「`keys` が空なら全索引走査」であること。06 の `begin_scan` が下限だけの範囲で NULL の項目を返さないこと。後ろ向きスキャンの順序 | 04 §13 |
| U38 | `uses_params` が `SubLink` を含む部分木で常に true になる（02-D7）劣化の程度。05 の `free_params` が相関のある副問い合わせが `Materialize` を内側に持つ場合に取りこぼさないこと（Z の対象） | 02 §8、05 §12 |
| U39 | P0-e で `Update` の入力に常に `Project` が入ることによる単一表 UPDATE の性能（M3 と 1 万行で比較して記録）。8 表の結合の計画時間（release 50ms） | 02 §8、04 §11.6 |

**型・関数（T1〜T3）**

| # | 項目 | 出典 |
|---|---|---|
| U40 | `pg_typeof` が引数を評価するか。`regtype` の入力の構文エラーの文言。`timestamp(p)` の `WARNING`。`min` / `max` の等値の代表（float の `-0`、bpchar のパディング）。正規表現の ARE の細部（`\xhhh` の桁数、`[[.x.]]`、`(?x)`） | 09 §14 |
| U41 | M5 のプランキャッシュで日時リテラルの畳み込み結果が `TimeZone` / `DateStyle` に依存すること（M4 は毎回計画するので問題ない） | 09 §14 |

**実行・DML（X1〜X3）**

| # | 項目 | 出典 |
|---|---|---|
| U42 | `ExecRelCheck` が CHECK を名前順に評価すること（2 つの CHECK に違反する行で報告される名前）。`avg(float8)` の `float8_accum` でのオーバーフロー（`avg(1e200, -1e200)`）。`sum(float4)` の内部精度。`HashedSubPlan` の複数列のランダムな組み合わせ（Z）。`SelfModified` の `cmax == cid` が複数のコマンド ID で変わるか | 05 §12 |

**EXPLAIN・COPY・セッション（E1・O1・S・J）**

| # | 項目 | 出典 |
|---|---|---|
| U43 | `CopyFail` のメッセージが空のときの文言と SQLSTATE（`57014` と推定）、COPY の `default` option と区切り文字の組み合わせ、`FREEZE` の前提を満たさないときの SQLSTATE（`55000` と推定）、`\.` の後に続くデータ、1 つの Query に `COPY` が複数ある文の `command_complete` の順序、`E` と `Z` の送信の順序 | 10 §12、pg-compat §9 |
| U44 | `limit_printout_length`（100 バイトで切る位置）、`get_rule_expr_paren` の推定（`BoolExpr` の親による括弧）、`n_rtable` の数え方の端（FROM なし、RTE_RESULT、WITH の中、UPDATE ... FROM）、`Update` / `Delete` の VERBOSE の `Output`、`pg_get_expr` の `pretty` が NULL のとき、`explain (analyze)` の `rows=` の丸め（`{:.0}` と `%.0f`） | 10 §12 |
| U45 | `SET default_transaction_read_only = on` の後の `EXPLAIN (ANALYZE) SELECT 1` が通ること。`transaction_timeout` を受け付けることが M3 の既存の slt（`42704` を期待するもの）と矛盾しないこと。`\dt` の Owner 列のロール名が PostgreSQL 側と同じ（`postgres`）になること | 10 §12 |
| U46 | `pgbench` の `--max-tries` の既定値（1 と推定）。`\d tbl` で外部キー・トリガの問い合わせが送られる条件。pgbench のパーティション確認（`CROSS JOIN LATERAL`）が `0A000` で失敗しても続行すること（偽のサーバで確認済み。pgbench の将来の版では未検証） | pg-compat §9、10-Q11 |

### (D) 結合・計測のとき

| # | 項目 | 出典 |
|---|---|---|
| U47 | `tests/compat` の psql の出力が PostgreSQL と yuzhu で一致するまでの差（`\l` の列、`\dn` の所有者 `pg_database_owner`、`\dt` の `Owner`）の洗い出し | 10 §6、11 §3.5 |
| U48 | `plan_variants` で PostgreSQL が `enable_*` を変えても**エラーにならない**問い合わせだけが残っていること（FULL JOIN の `ON`）、`plan_shapes.slt` が各設定のノード名を満たすこと | 11 §3.3 |
| U49 | 差分ランダムテストの誤検出（評価順で変わるエラー、`-0`、タイムアウト）が `fuzz-pr` で 0 件になること。固定シード 1〜8 の実行時間 | 11 §3.7 |
| U50 | クラッシュ試験 層 1 の CI の実行時間（120 秒以内）。ワークロード 6 が `shared_buffers = 24` で分割を頻発させること | 11 §3.6、06 §7.11 |
| U51 | pgbench の tpcb-like の完走と TPS（死んだ版の項目が増え続けることの影響）。`CREATE INDEX` の時間と WAL の量 | 06 §7.15、06-Q11 |

---

## 6. 確認事項

ユーザーの不在中に仮決めしたことの**全章の集約**です。各章の `[章番号-Q番号]` を `M4-Q1` からの通し番号に振り直しました（順序: 02、03、04、05、06、07、08、09、10、11。01 は §6.3）。各項目に「仮決め」「理由」「変えたい場合の影響」を書きます。**★ はディスク形式に関わるもの**（実装の前に決めるのが望ましい。06-Q1・Q2・Q3・Q4・Q6・Q9、07-Q1・Q2、08-Q1・Q2・Q3、09-Q1 の 12 件）。章の間で食い違っていたものは「**→ C-n**」と書き、§7.1 の決定に従います。

### 6.1 各章の確認事項（通し番号）

**02 パイプラインの作り直し（M4-Q1〜Q10）**

- **M4-Q1 [02-Q1] 外部結合の NULL 側の `ColId` を再発行しない**: 仮決め: `Join` の NULL 側の列は同じ `ColId`。結合より上の `Column(c)` は null 拡張後の値を指す。理由: 再発行すると結合の上のすべての式の `ColId` を書き換える。PG の `varnullingrels` 相当は結合の位置から判定できる。影響: `Join.null_cols` を足す方式は build・押し下げ・外部結合の簡約・刈り込み・`validate` が増え約 +1.5 日。
- **M4-Q2 [02-Q2] 移行を下から行う（executor → analyzer → planner）**: 仮決め: 各段階の隣に一時アダプタ（`planner/legacy.rs`）。理由: アダプタが自明な構造変換で済む。影響: 上からは約 +1 日（旧型向けの `physicalize` が使い捨て）。
- **M4-Q3 [02-Q3] `uses_params()` は `SubLink` を含む部分木で常に true**: 仮決め: 00 の定義に足す。理由: `PhysExpr` の `SubLink` は `SubPlanId` だけで `Param` 参照が見えない。影響: `uses_params(&self, q)` に署名を変えると +0.5 日。**→ C-6**（executor の溜めた結果の再利用は 05 の `free_params` で決める。`uses_params()` は単体テスト用の保守的な判定として残る）。
- **M4-Q4 [02-Q4] 外側レベルの集約は `0A000`**: 仮決め: `(SELECT sum(t.a) FROM u) FROM t` の形は `0A000`。理由: PG は集約を外側の問い合わせに所属させる。必要性が低く複雑。影響: 集約の所属レベルを決めて外側の `has_agg` を立てる処理を 03・04 に足す約 +3 日（KD-8）。
- **M4-Q5 [02-Q5] 外側の列を参照する CTE は `0A000`**: 仮決め: `MATERIALIZED` でなくても `0A000`（参照 1 回の非 `MATERIALIZED` はインライン展開されるので出ない）。理由: 1 回だけ実行して共有する設計と `Param` による再実行は両立しない。影響: `CteStates` を `Param` の変化で作り直す約 +1.5 日（KD-23）。
- **M4-Q6 [02-Q6] 導出表の列名は内側の名前のまま、`Subquery Scan` ノードを持たない**: 仮決め: EXPLAIN の式が `s.x` でなく内側の名前で出る。理由: PG もプルアップされた導出表は内側の名前。影響: 論理・物理に `SubqueryScan` を足す約 +1.5 日（04・10 に波及）。
- **M4-Q7 [02-Q7] デバッグビルドで `validate` を常時実行し、隠し設定 `yuzhu.validate_plans`**: 仮決め: リリースは既定 off。`PlannerSettings.validate_plans`。理由: 全テストで不変条件が検査される。影響: 環境変数にする・常時 on にする（性能の計測が要る）。CI の `slt-yuzhu` は on で起動する。
- **M4-Q8 [02-Q8] Join RTE を指す `Var` を Bound に残さず、アナライザが展開する**: 仮決め: USING / NATURAL の併合列と `j.*` は式に展開。理由: `build` と `validate` が単純。影響: `build` が展開する方式は 03・04 に波及し約 +1 日。03-Q2 と同じ決定。
- **M4-Q9 [02-Q9] `levels_up` の数え方**: 02 の仮決め: 1 つの `BoundQuery`（本体が Values / SetOp でも）が 1 レベル。**→ C-3: 03 の D3-20（rtable を持つスコープだけを数える）を採る**。02 の §3.4.2 と `validate` の B1 を直す。理由: 03（生成側）と 04（消費側）が一致している。影響: 02 の方式に揃えると 03（N3）の書き直し約 +0.5 日と 04 の `scopes` の積み方の変更。
- **M4-Q10 [02-Q10] `SubLink` を含む式の `same_as` が常に不一致**: 仮決め: `GROUP BY (SELECT ...)` を SELECT に書き直すと `42803`（PG は通す。KD-21）。理由: 副問い合わせの木の等価判定が M4 の必要性に見合わない。影響: `BoundQuery` の構造的な等価判定を 03 に足す約 +1 日。

**03 パーサとアナライザ（M4-Q11〜Q27）**

- **M4-Q11 [03-Q1] FULL JOIN の `0A000` はプランナが出す**: 仮決め: アナライザは判定せず、プランナが「ハッシュ可能な等値がない FULL」を `0A000`（PG と同じ文言）。`ON true` の FULL は PG が通すが yuzhu は通さない（KD-2。**→ C-19**）。理由: PG もプランナが出す。影響: アナライザで判定するなら 04 と重複する分類を足す約 +0.5 日。
- **M4-Q12 [03-Q2] 結合の別名 `Var` は解析時に展開する**: 仮決め: Bound に Join RTE を指す `Var` は現れない。理由: GROUP BY の検査・出力列の由来・04 が単純。影響: 04 が展開する約 +1 日 + 約 +0.5 日。
- **M4-Q13 [03-Q3] 関数従属は主キーだけ。許された `Var` を `group_by` の末尾に足す**: 仮決め: `pg_depend` には記録しない（UNIQUE NOT NULL は対象外。実測）。理由: 実機の挙動。影響: 足さないなら 05 の `Aggregate` に代表行の保持（約 1 日）。
- **M4-Q14 [03-Q4] 外側のスコープに属する集約は `0A000`**: M4-Q4 と同じ決定（KD-8）。影響: 約 +2 日。
- **M4-Q15 [03-Q5] LATERAL は `0A000`。診断は PG と同じ**: 仮決め: 明示はパーサ、関数引数の暗黙の LATERAL はアナライザ。理由: LATERAL は M5〜M6。pgbench のパーティション確認は失敗してよい。影響: 参照を許して 04 が相関パラメータ付き NestedLoop で実行する 3〜4 日（KD-3）。
- **M4-Q16 [03-Q6] `WITH ... INSERT/UPDATE/DELETE` とデータ変更 CTE は `0A000`**: 仮決め: パーサが拒否。`INSERT ... SELECT` の中の WITH は動く。理由: `BoundUpdate` / `BoundDelete` に `ctes` の欄がない。影響: `ctes` を足し 04・05 に各 1 日（KD-4）。
- **M4-Q17 [03-Q7] `WITH RECURSIVE` の構文は受理し、再帰参照だけ `0A000`**: 理由: ORM が `RECURSIVE` を付けて非再帰の CTE を送る。影響: 全面的に `0A000` にするなら 0.1 日。
- **M4-Q18 [03-Q8] 行値は IN / ANY / ALL サブクエリの左辺だけ（M4 後半・任意）**: 理由: 00 §6.2 が `test` の形を決めている。影響: 受理しないなら 0.1 日 + 0.3 日が減る。
- **M4-Q19 [03-Q9] `ANY` / `ALL` の配列形は `0A000`**: 理由: 配列は M5。psql の `\d tbl` の 2 本が失敗（KD-9・KD-20）。影響: 最小の配列と `ANY(array)` の `Expr` 変種が要る 3 日（00 の変更）。
- **M4-Q20 [03-Q10] `COLLATE` は `"C"` / `"POSIX"` / `"default"` だけ受理して無視**: 理由: 照合順序は C だけ。psql が `COLLATE pg_catalog.default` を使う。`42P21` は検出しない。影響: 式に collation の欄が要る（M5 以降）。
- **M4-Q21 [03-Q11] FROM 句の関数は `FnKind::Set`（`generate_series`）だけ**: 理由: `FunctionScan` は集合返却だけ。影響: スカラー関数を 1 行の関数スキャンにする約 0.7 日（KD-24）。
- **M4-Q22 [03-Q12] `IS DISTINCT FROM` は `0A000` のまま**: 理由: 00 の `ExprKind` に変種がない。影響: `ExprKind::IsDistinctFrom` を足す（式の走査・評価・deparse で各 0.2 日。**M4 後半の任意項目として推奨**。KD-5）。
- **M4-Q23 [03-Q13] JOIN の別名（`USING ... AS j`、`(t JOIN u ON ..) AS j`）は `0A000`**: 理由: ORM がまれにしか生成しない。影響: 約 0.5 日（余力があれば入れてよい。KD-10）。
- **M4-Q24 [03-Q14] RETURNING は解析を実装し、`RETURNING_ENABLED` で解禁する**: 仮決め: 解禁は X3 と 04 の対応後。対象表の列だけ（KD-26）。理由: 00 D-26（M4 後半・任意）。影響: FROM の列を許すなら `BoundReturning` を作り直し 04 が入力に載せる 2 日。
- **M4-Q25 [03-Q15] 全行参照（`count(t)`、`select t from t`）は `0A000`**: 理由: 複合型が要る（M5 以降）。
- **M4-Q26 [03-Q16] 未対応の集約名は `0A000 aggregate function X is not supported yet`**: 仮決め: `string_agg` `array_agg` `stddev` ほか PG の組み込みで `AGGREGATES` にないもの（KD-7）。理由: `42883` と区別でき、M5 での追加が分かりやすい。
- **M4-Q27 [03-Q17] エラーの DETAIL / HINT は主要なものだけ**: 仮決め: 近い名前の HINT は出さない。理由: 編集距離の実装が要る。影響: 約 1 日。

**04 プランナとルールベース最適化（M4-Q28〜Q41）**

- **M4-Q28 [04-Q1] `ColId` をパススルーで引き継ぐ**: 仮決め: `Get` → `Project` の単純な参照 → `Aggregate` の group key は同じ `ColId`。計算列だけ新しい ID。理由: 押し下げ・派生表の展開・Semi 化・相関参照が付け替えなしでできる。影響: 「ノードごとに新しい ID」にすると R3 が置換表を持ち回り、L1・L2 の大半に影響。
- **M4-Q29 [04-Q2] 相関のある `IN` を `EXISTS` の形で結合化する**: 理由: PG 17 も Semi 結合にする（実測）。M4 は LATERAL がないので `EXISTS` の形に直せるものだけ。影響: 変換しないなら分岐を消すだけ（結果は同じ）。
- **M4-Q30 [04-Q3] 左結合の ON の中の `EXISTS` を引き上げない**: 影響: `try_pull` に右の子を足す約 +0.3 日。
- **M4-Q31 [04-Q4] 内側 Index Scan の Nested Loop の採否の定数**（`ROWS_PER_BLOCK_EST = 100`、`probe_cost`）: 理由: 統計がないので `nblocks` と述語の個数だけ。影響: 定数だけ（結果は変わらない）。
- **M4-Q32 [04-Q5] ビルド側は `estimate` の小さいほう、同点は右**: 影響: 定数・規則だけ。
- **M4-Q33 [04-Q6] ソートの省略は条件つき（M4 後半・任意）**: 仮決め: 述語で選んだ索引の順序で足りるとき、直上が `Limit` のとき、`enable_sort = off` のときだけ。理由: 全索引走査はヒープへのランダムアクセスが増える。影響: 04 §7.4 の (b) の条件。
- **M4-Q34 [04-Q7] InitPlan / SubPlan の EXPLAIN の位置と番号**: 仮決め: 式を持つノードの下、番号は物理化した順。PG は最上位ノード（KD-25）。影響: `assemble` の置き場所と採番だけ（`onlyif yuzhu` の期待値）。
- **M4-Q35 [04-Q8] `ExplainNode` に `plan_id`（と `width`）を足し、`PhysicalPlan` と同形にしない**: 仮決め: `Hash` の合成・`Filter` の併合・`Project` の透過。**→ C-1: 名前は `exec_id`（10 に従う）**。影響: 同形にすると EXPLAIN の見た目が PG から少し離れる（`onlyif yuzhu` の期待値だけ）。
- **M4-Q36 [04-Q9] リテラルのキャストを（Stable でも）計画時に畳む**: 理由: DateStyle・TimeZone・カタログは文の間は固定。影響: M5 の Extended Query でプランをキャッシュするなら設定が変わったときの再計画が要る。
- **M4-Q37 [04-Q10] プランをキャッシュしない**: M4 は文ごとに計画。M5 で再計画の仕組みを足す。
- **M4-Q38 [04-Q11] 外側の列を参照する共有 CTE はインライン、揮発性なら `0A000`**: 理由: M4 の executor は `CteScan` の再実行を持たない。影響: 共有を許すなら `CteScan` の作り直しと `uses_params` への CTE の算入。
- **M4-Q39 [04-Q12] 計画の再帰の深さの上限 `MAX_PLAN_DEPTH = 500`（`54001`）**: 影響: 定数だけ。スタックサイズ（J）と合わせる。
- **M4-Q40 [04-Q13] 従属列は group key に足す**: 理由: グループ内で一定なので意味が変わらず、追加の集約が要らない（03-Q3 と同じ）。
- **M4-Q41 [04-Q14] `EXISTS` を `ANY` に直してハッシュ化しない**: 仮決め: 引き上げられない相関 `EXISTS` は `Rescan`。PG は等値の相関ならハッシュ化 SubPlan。影響: `plan_sublink` に変換を足す約 +0.5 日。

**05 実行ノードと DML（M4-Q42〜Q51）**

- **M4-Q42 [05-Q1] メモリの課金は文の終わりまで保持し、`rewindable = false` のノードだけ枯渇で返す**: 理由: 二重計上を避ける複雑な管理を作らず、安全側に数える。影響: 厳密な追跡は `Executor` に `release` を足し全ノード変更（+1 日）。
- **M4-Q43 [05-Q2] 溜めた結果の再利用判定に `free_params` を使う（00 の `uses_params()` は使わない）**: 理由: `uses_params()` は `SubLink` の `SubPlanDef.params` を見られず、再利用を誤る。**→ C-6**。影響: `uses_params()` を使うなら `SubLink` を含むノードを保守的に「依存」にする（正しさは保たれるが遅い）。
- **M4-Q44 [05-Q3] 23505 の DETAIL を `dml.rs` が補う**: 仮決め: B+Tree は `23505` と `s` / `t` / `n` だけ（06-Q8 と同じ決定）。理由: `IndexStore::insert` に `TypeEnv` がない。影響: B+Tree が作るなら `IndexStore::insert` に `&TypeEnv` を足す（00 §13.2 の変更）。
- **M4-Q45 [05-Q4] RETURNING は対象表の列だけ**: 仮決め: FROM / USING の列を参照する RETURNING は `0A000`（KD-26）。影響: RETURNING の式を入力の `Project` に出す形に変える +1 日（02・04・X3）。
- **M4-Q46 [05-Q5] 相関のある MATERIALIZED CTE を拒否する**: 仮決め: planner が `0A000`。executor は内部エラー（KD-23）。影響: `CteSlot` に世代番号 +1 日。
- **M4-Q47 [05-Q6] `NestedLoopJoin` の outer は left、inner は right**: 理由: 「出力は左 ++ 右」を NLJ にも適用。影響: INNER の入れ替え用に `outer_is_right` を足す。
- **M4-Q48 [05-Q7] `HashSetOp` に UNION も実装する**: planner は使わない。影響: 削除してよい。
- **M4-Q49 [05-Q8] `HashAggregate` の出力はグループの初出順**: 理由: テストが安定する（PG は不定なので PG と比べる slt は `ORDER BY`）。影響: 順序を変えると yuzhu だけの期待値が変わる。
- **M4-Q50 [05-Q9] 相関のある副問い合わせの結果を、パラメータが前回と同じなら再利用する最適化はしない**: 理由: volatile 関数があると誤る。影響: planner が immutable / stable だけを `SubPlanDef` に持たせれば 1 エントリのキャッシュを足せる +0.5 日。
- **M4-Q51 [05-Q10] 計測の対応づけのための `ExplainNode.phys_id`**: **→ C-1・C-2: 採用しない（10 の `exec_id` と `set_counters` に統一）**。05 の仮決め: 合成ノードを持つ場合だけ必要。影響: 05 §9 の `BuildOptions.instrument` / `PlanScope` / `NodeKey` / `extra_stats` を 10 の方式に置き換える（X1〜X3 の各ノードが `set_counters` を実装。約 +0.3 日）。

**06 B+Tree（M4-Q52〜Q71）**

- **M4-Q52 [06-Q1] ★ ピボットの境界の向き（左の全項目 < 区切り <= 右の全項目）**: 仮決め: 区切り = 右ページの最初の項目そのもの。PG は逆。理由: 切り詰めない M4 では「TID の最下位を 1 引く」特例が要らず、検査器が単純。M5 の suffix truncation とも矛盾しない。影響: 降下・moveright・検査器を反転。ディスク形式（ピボットの TID の意味）が変わるので実装の前なら半日、後なら initdb のやり直し。
- **M4-Q53 [06-Q2] ★ 空のインデックスでもルート葉を作る（メタの `root = 1`）**: 仮決め: 00 D-7 のとおり（`init_index` がメタ + 空の葉の 2 ページを `BTREE_PAGES`（`INIT`）で書く）。理由: 挿入の途中でルートを作る分岐と REDO が要らない。影響: 遅延作成にすると `BTREE_PAGES` の理由が 1 つ増え、07 の `EMPTY_INDEX_STATS` が変わる（+0.5 日）。
- **M4-Q54 [06-Q3] ★ 構造変更は全画像の 1 レコード `BTREE_PAGES`。ブロック数は `3h + 1`、静的な高さの上限は作らない**: 仮決め: 必要なブロック数が 32 を超えたときだけ `54000`。00 §13.4 の `2h + 3` と調査の `3h + 2` は不正確（**→ C-13**）。理由: 未完了の分割という状態がなくクラッシュ試験の分岐が増えない。影響: PG 方式（`INCOMPLETE_SPLIT`）は +M。
- **M4-Q55 [06-Q4] ★ ピボットの形（切り詰めなし、葉タプル + 8 バイト、TID は末尾の 6 バイト、high key の `block = 0`、-∞ ピボットは 8 バイト）**: 理由: 大きさが `S + 8` と定まり、`BT_MAX_ITEM_SIZE` で「high key + データ 2 件」が保証できる。影響: 切り詰めを入れるのは +3 日（ページ形式は変わらない）。
- **M4-Q56 [06-Q5] 分割点の規則（バイト数が均等。右端の葉への末尾への追記だけ左を 90% まで詰める）**: 理由: 単純で決定的。影響: `choose_split` だけ（ディスク形式は変わらない）。
- **M4-Q57 [06-Q6] ★ 一括構築の詰め方（葉を全部作ってから上のレベル、葉 90%・内部 70%、32 枚ずつ `BTREE_PAGES`）**: 理由: 確保が常に末尾への追加で、レコードが連続したブロックになる。影響: nbtsort のように全レベル並行は +1 日（ブロック番号が飛ぶ）。
- **M4-Q58 [06-Q7] バッファプールへの依頼（`read_tree` / `write_tree` / `extend_tree`）**: 仮決め: M3 の debug 検査（ブロック番号の昇順）を B+Tree には適用しない口を足す（0.3 日）。影響: 昇順に合わせるには分割で全ページを昇順にラッチし直す +1 日。
- **M4-Q59 [06-Q8] 23505 の DETAIL は B+Tree が付けない**: M4-Q44 と同じ決定。影響: B+Tree が作るなら `UniqueCheck::Check` と `BuildUnique::Yes` に `&TypeEnv` を足す。
- **M4-Q60 [06-Q9] ★ `datetime_ops` に型をまたぐ行を入れない**: 仮決め: `date` `timestamp` `timestamptz` の同じ型どうしだけ（`pg_amop` が PG より 30 行、`pg_amproc` が 6 行少ない）。理由: 型をまたぐ比較は `TimeZone` に依存し `CmpFn`（純粋）に環境を渡せない。09 がその演算子を作らない。影響: 作るなら 06 に最大 30 行と `CmpFn` の署名変更（+1.5 日。09-Q4 と同時）。
- **M4-Q61 [06-Q10] `fetch_dirty` / `tuple_state` は「自分以外の実行中のトランザクション」を持たない**: 理由: 単一ライターなので clog が実行中の他者の XID は中断の残骸。影響: M5 で実行中の判定を足す（B+Tree の呼び出し側は変わらない）。
- **M4-Q62 [06-Q11] 項目を消さないことの影響**: 仮決め: 同じキーの UPDATE を繰り返すと死んだ版の項目が増え続け、一意性検査と等値スキャンが全部をヒープで確かめる。理由: 削除は WAL・スキャンとの連動・ヒントビットが要り M5 の VACUUM と一緒。影響: pgbench の完走はするが TPS が低い。緩和は簡易削除（+2〜3 日）。
- **M4-Q63 [06-Q12] `BuildStats.levels` はルートの `level`（葉だけの木は 0）**: 理由: 07 の `EMPTY_INDEX_STATS` に合わせる。影響: 表示（`pg_class`）だけ。
- **M4-Q64 [06-Q13] `pg_opclass` の OID（`bool_ops` 10003 など 10000 番台）**: 仮決め: PG 17.11 の実機の値を写す。テストは名前で結合して比べる。影響: なし（`builtin_hash` が変わるので initdb のやり直し）。
- **M4-Q65 [06-Q14] ページの破損の SQLSTATE は `XX001`**: 理由: `error.rs` に `XX002` がなく REINDEX もない。影響: `INDEX_CORRUPTED` を足す 0.1 日。
- **M4-Q66 [06-Q15] CREATE INDEX は全件をメモリに持つ**: 理由: 外部ソートは M6。影響: 上限を設けるなら 07 の `build_from_heap` が `53200` を返す。
- **M4-Q67 [06-Q16] 分割の原子性を壊す変異用の `DebugKnobs`（任意）**: 理由: 「1 レコードで書く」ことをクラッシュ試験で守る。影響: 作らないなら変異テストの 1 行を省く（B1 +0.3 日）。
- **M4-Q68 [06-Q17] 一意性検査: 最初の葉の保持と、等値の連続の全件確認**: 理由: M5 の形に合わせつつ M4 は単純に。影響: M5 で右の葉を取ってから最初の葉を外す・`WaitFor` で待つ・簡易削除。
- **M4-Q69 [06-Q18] 比較は項目を `Datum` に復号してから行う**: 理由: 型ごとの比較規則を `cmp_datum` に一本化（00 §4.3 の 2）。影響: 性能が問題なら整数・text だけバイト列比較を足す。
- **M4-Q70 [06-Q19] PostgreSQL が圧縮して入る値が `54000` になる**: 理由: M2-Q11（TOAST なし）の帰結。差として許容し、共有テストは圧縮されない値だけ（KD-15）。影響: 圧縮は TOAST と一緒に M5 以降。
- **M4-Q71 [06-Q20] `default_opclass` にバイナリ互換の解決を含める**: 仮決め: `varchar` → `text_ops`、`regclass` / `regtype` / `regproc` → `oid_ops`。理由: 型と opclass の対応を 1 か所に。影響: 07 の `resolve_opclass` が補うだけ。

**07 カタログと DDL（M4-Q72〜Q89）**

- **M4-Q72 [07-Q1] ★ カタログの追加と行の値**: 仮決め: 9 カタログ（`pg_index` `pg_depend` `pg_sequence` `pg_language` `pg_opfamily` `pg_opclass` `pg_amop` `pg_amproc` `pg_description`（空））。`reltype` は 0。`pg_depend` は §3.6 の表だけ。理由: 00 D-23。M4 に `DROP SCHEMA` と外部キーがない。影響: 名前空間と CHECK の列の依存を足すと書き込みが増える（後から足せる。`CATALOG_VERSION_NO` を上げて initdb）。
- **M4-Q73 [07-Q2] ★ `int2[]` / `int2vector` のディスク形式**: 仮決め: `Datum::Int2Vector`（varlena + `i16` LE。PG の配列ヘッダなし）。理由: M4 は 1 次元の `int2` 配列だけ。影響: M5 で PG の形式にすると `conkey` / `indkey` の読み直しと initdb。
- **M4-Q74 [07-Q3] 1 コマンドで同じカタログ行を 2 度更新しない**: 理由: `cid` が文の終わりでしか進まず、同じコマンドで挿入した行は見えない。影響: `command_counter_increment` を DDL の途中で呼べるようにする +0.5 日（M2 / M3 のコマンド ID の不変条件に触る）。
- **M4-Q75 [07-Q4] 一意索引の構築の重複検出は C1 が行う（`BuildUnique::No`）**: 理由: 死んだ版を索引に入れるので、隣接比較だけの検出は UPDATE した行を誤検出する。影響: `build` に `live: bool` を足す（06 の変更。工数は同じ）。
- **M4-Q76 [07-Q5] `OWNER TO` を実装する**: 仮決め: 00 D-12 の「何もしない」を変える（ロールを検証し `relowner` を更新）。理由: `\dt` の Owner が誤る。実装は小さい（+0.25 日）。影響: 何もしない（ロールの存在検査だけ）に戻せる。
- **M4-Q77 [07-Q6] `ANALYZE` はトランザクションブロックの中でも成功する**: 仮決め: 00 D-11 の「両方 `25001`」を変える（実測は `VACUUM` だけ `25001`）。影響: なし（PG と同じにしてあるだけ）。
- **M4-Q78 [07-Q7] DROP の依存: `pg_depend` の閉包と二重の網**: 影響: 直接のキーだけにすると `DROP TABLE ... CASCADE` の連鎖と M5 の外部キーを後で作り直す。
- **M4-Q79 [07-Q8] `WITH (...)` の reloptions は `fillfactor`（表・索引）と `deduplicate_items`（索引）だけ**: 仮決め: ほかは `22023`。`fillfactor=50.5` は `22023`（KD-18）。影響: `autovacuum_enabled` などを受け付けて捨てる名前の一覧 +0.1 日（M5 の pg_dump のリストアで必要になる可能性が高い）。
- **M4-Q80 [07-Q9] `TRUNCATE ... RESTART IDENTITY` のシーケンスの更新はトランザクショナルでない**: 理由: 08 のその場の上書き（M4-Q94 と同じ決定。KD-11）。影響: `SequenceStore::reset` を新しい relfilenode 方式にする（08 +0.5 日）。
- **M4-Q81 [07-Q10] `ALTER TABLE ... DROP CONSTRAINT` を M4 に入れない**: 仮決め: PRIMARY KEY の索引は `DROP TABLE` でしか消せない。影響: `DROP CONSTRAINT [IF EXISTS] name` は +0.5 日（CHECK の削除は +0.25 日）。
- **M4-Q82 [07-Q11] `CREATE INDEX CONCURRENTLY` / `DROP INDEX CONCURRENTLY` を通常と同じに動かす**: 理由: ORM のマイグレーションが付けても動く。影響: `0A000` にすると ORM が失敗する。
- **M4-Q83 [07-Q12] `relhasindex` を `DROP INDEX` で下ろさない**: 理由: PG は VACUUM が下ろす。M4 の VACUUM は何もしない。影響: 下ろすなら PG と違う値が見える（KD-14）。
- **M4-Q84 [07-Q13] 索引の `relpages` は空でも 2、`reltuples` は構築した件数。表の `relpages` / `reltuples` は更新しない**: 影響: `ANALYZE` が更新する（M5）。
- **M4-Q85 [07-Q14] 索引・シーケンスの名前を `table()` で引くと `None`**: 仮決め: DML / SELECT で索引を指定したときの `42809 cannot open relation "x"`（実測）を返すのが望ましい（アナライザ側 = N1 の作業）。
- **M4-Q86 [07-Q15] `name` 型の列の索引の `atttypid` は `name` のまま（PG は `cstring`）**: 影響: `\d` には影響しない（KD-16）。
- **M4-Q87 [07-Q16] 自動名を実行時に決める**: 仮決め: 00 の `BoundIndexConstraint.name: String` を `Option<String>` にする。理由: 同じ文の中の衝突の解決。影響: 解析時に決めるとアナライザが文内の名前を覚える責任が増える。
- **M4-Q88 [07-Q17] CHECK の自動名の衝突判定を名前空間の全制約に広げる**: 理由: PG の `ChooseConstraintName`。影響: 同じ表の中だけに戻せる（まれな差）。
- **M4-Q89 [07-Q18] クラッシュで中断した DDL の孤児ファイルを掃除しない**: 理由: M3 の D15、M3-Q5。影響: 大きな表への CREATE INDEX の途中のクラッシュで数百 MB の孤児ファイル。起動時の掃除は M5。

**08 シーケンスと SERIAL / IDENTITY（M4-Q90〜Q103）**

- **M4-Q90 [08-Q1] ★ シーケンスのページとタプルの形**: 仮決め: PG と同じ構造（special 8 バイト `SEQ_MAGIC = 0x1717`、タプル 1 個 57 バイト、`xmin = Xid::FROZEN`、`t_ctid = (0,1)`）。理由: `SELECT * FROM シーケンス` が通常のヒープ走査で動き、`xmin = 2` と `ctid = (0,1)` も PG と同じ。影響: 形を変えると別の実行ノードが要る（+1 日）。
- **M4-Q91 [08-Q2] ★ `SEQ_LOG` の形**: 仮決め: ブロック 1 個、`WILL_INIT`、タプル全体、メインデータなし。REDO は無条件に上書き。理由: PG と同じ。REDO が単純で FPW の対象外。影響: 差分レコードにすると REDO が LSN の順序に依存し FPW が要る（+1 日）。
- **M4-Q92 [08-Q3] ★ SERIAL の DEFAULT の保存形式**: 仮決め: `pg_attrdef.adbin` に `nextval('<oid>'::regclass)`。`pg_get_expr` が名前に戻す。利用者が書いた DEFAULT は M2 のとおり。理由: 名前の変更・検索パスの影響を受けない。影響: すべての DEFAULT を正規形にすると deparse に OID 形式のモード（+0.5 日）。
- **M4-Q93 [08-Q4] ROLLBACK では flush しない**: 理由: 中断した値は外から見えない前提（PG と同じ保証）。影響: 中断でも `finish_without_xid` 相当で flush する（工数ほぼ 0。I15 が強まる）。
- **M4-Q94 [08-Q5] `ALTER SEQUENCE` はその場で書き換える（状態はロールバックされない）**: 仮決め: `log_cnt` は常に 0。`TRUNCATE ... RESTART IDENTITY` も同じ（KD-11）。理由: 00 の `reset` と D-9。PG の方式は M5 の表ロックなしでは払い出しが失われうる。影響: 新しい relfilenode 方式（+2 日）は M5 のロックが要る。
- **M4-Q95 [08-Q6] `TEMPORARY` / `UNLOGGED` シーケンス、`ALTER SEQUENCE RENAME` / `SET SCHEMA` は `0A000`**: 影響: RENAME は +0.25 日（KD-17）。
- **M4-Q96 [08-Q7] 他のセッションの先取りを全部捨てる（`reset_generation`）**: 影響: シーケンスごとの世代にできる +0.25 日。
- **M4-Q97 [08-Q8] 払い出した値を覆う WAL の LSN をページの LSN で決める（PostgreSQL の穴を塞ぐ）**: 理由: PG の方式は他トランザクションの未 flush の `SEQ_LOG` に依存した払い出しがクラッシュで重複しうる。影響: 「自分が書いた分」に戻すと PG と同じ穴（層 1 の変異試験が検出する）。
- **M4-Q98 [08-Q9] ユーザーが書いた DEFAULT の `regclass` は書いたままのテキスト（遅延束縛）**: 理由: M2 の「DEFAULT は SQL テキスト」（Q-006）を変えない。影響: 検索パスを変えると別のシーケンスを引きうる（M4-Q92 の正規形で直る）。
- **M4-Q99 [08-Q10] 同じ `CREATE TABLE` が作るシーケンスの名前を DEFAULT に書くと `42P01`**: 理由: DEFAULT の解析を `ddl` の途中に移す必要があり価値が低い（KD-22）。影響: +0.5 日（07 の構造に影響）。
- **M4-Q100 [08-Q11] `pg_get_serial_sequence` は任意（M4 後半）**: 影響: 必須にすると +0.25 日。
- **M4-Q101 [08-Q12] IDENTITY の `SEQUENCE NAME` は採用、`OWNED BY` / `LOGGED` は `0A000`**: 理由: pg_dump の出力が使うのは `SEQUENCE NAME`。
- **M4-Q102 [08-Q13] 同じ文の 2 つの暗黙のシーケンスの名前の衝突を避ける**: 理由: PG の上位互換（KD-22）。影響: `taken` を空にすれば PG と同じ。
- **M4-Q103 [08-Q14] `ALTER SEQUENCE` のあとの `log_cnt` は常に 0**: 理由: PG の方式はクラッシュで払い出した値が戻りうる。影響: `SELECT * FROM s` の `log_cnt` が一致する代わりにその穴を持つ。

**09 型・関数・集約・正規表現（M4-Q104〜Q113）**

- **M4-Q104 [09-Q1] ★ numeric のディスク形式は 00 §12.3 の固定ヘッダ**: 仮決め: `ndigits` `weight` `sign` `dscale` + 桁。PG の short / long ヘッダ形式は採らない（調査との食い違いは 00 を採る）。理由: M2-Q2 でタプルヘッダが PG と違いサイズ互換の意味がない。影響: PG 形式にすると `encode_numeric` / `decode_numeric` の書き直し約 0.5 日と `tuple.rs` のテストの更新。
- **M4-Q105 [09-Q2] `interval` を M5 に回し、`timestamp - timestamp` を `42883` にする**: 理由: M4 の完了条件が使わず、`interval` の連鎖（`Datum`・比較・ハッシュ・ディスク形式・`IntervalStyle`・演算子・集約）が大きい（KD-1）。影響: 入れるなら約 +3 日（`extract` なども入れて約 +4 日）。
- **M4-Q106 [09-Q3] 日時リテラルはアナライザでなくプランナの畳み込みで評価する**: 理由: `analyze(stmt, catalog)` の署名に `TypeEnv` がない（署名変更は全担当に波及）。影響: `analyze` に `&TypeEnv` を足すと解析時に評価できる（呼び出しは `session.rs` と `testing.rs` だけ）。
- **M4-Q107 [09-Q4] 型をまたぐ日時の比較演算子を作らない**: 理由: `BuiltinOperator.func` が純粋関数。影響: `OpFn::{Pure, Env}` を足し 30 行を入れる（`ts_col < now()` が索引で使える。06-Q9 と同時。約 +1.5 日）。
- **M4-Q108 [09-Q5] 正規表現の後方参照と先読みは `0A000`**: 理由: 後方参照は線形時間で解けない（KD-13）。影響: ステップ数の上限つきバックトラック版の併設約 +2 日。
- **M4-Q109 [09-Q6] `pg_typeof` は引数を評価しない**: 理由: `BuiltinFn` が型を受け取らない（KD-12）。影響: 約 +0.5 日。
- **M4-Q110 [09-Q7] DEFAULT の `'now'::timestamp` は毎回評価される**: 理由: 保存時に畳むには deparse した定数を書き戻す必要（Q-006）。影響: 約 +1 日。
- **M4-Q111 [09-Q8] `TypeEnv.names` で reg* の名前表示を一本化する（00 の P-1）**: 理由: `regclass::text` が出力段以外でも動く必要。影響: 出力段だけだと `regclass::text` が数字になり `\d tbl` と ORM が壊れる。
- **M4-Q112 [09-Q9] `round(float8)` などの float8 版を入れる**: 理由: `round(2)` が PG では `double precision`。影響: 入れないと numeric を返して PG と型が違う（費用は約 0.3 日）。
- **M4-Q113 [09-Q10] `timestamp(p)` の p が 7 以上のときの `WARNING` を出さない**: 理由: アナライザに通知の経路がない（KD-12）。影響: アナライザの出力に `warnings` を足す約 0.5 日。

**10 EXPLAIN・deparse・COPY・互換（M4-Q114〜Q126）**

- **M4-Q114 [10-Q1] `ExplainNode` を PostgreSQL の表示用の木にする**: 仮決め: `PhysicalPlan` と同形にせず、`Project` / `Filter` を吸収し `Hash` を足す。計測は `exec_id`（**→ C-1 で全章の基準にする**）。影響: 同形にすると EXPLAIN の期待値の大半を yuzhu 専用にする。
- **M4-Q115 [10-Q2] ANALYZE の出力を `actual` と `Rows Removed` だけにする**: 理由: `Sort Method` `Buckets` `Memory Usage` は実装依存。影響: 各ノードが追加の計測を持つ約 1〜2 日。
- **M4-Q116 [10-Q3] `BUFFERS` `WAL` `SETTINGS` `MEMORY` `SERIALIZE` を受け付けて無視する**: 理由: pgAdmin・DBeaver が付ける。影響: `0A000` にするとそれらのツールの EXPLAIN が使えない。
- **M4-Q117 [10-Q4] `FORMAT JSON` / `XML` / `YAML` を `0A000` にする**: 影響: JSON を足す約 2 日（KD-19）。
- **M4-Q118 [10-Q5] COPY の失敗の後に即座に `E` と `Z` を返し、後続の `d` `c` `f` を無視する**: 理由: PG と同じ。影響: 「CopyDone まで受信して捨ててから `E`」はデータを送らないクライアントがハングする。
- **M4-Q119 [10-Q6] `\d tbl` は M4 の完了条件に入れない**: 仮決め: 動くのは索引・CHECK・既定値の表示まで。配列を使う 2 本が M5 まで通らない（KD-20）。影響: 配列の最小実装を M4 に足す約 5 日（09 の範囲）。
- **M4-Q120 [10-Q7] COPY の CSV・バイナリ・`TO`・`WHERE`・`ON_ERROR` を M5 にする**: 影響: CSV 約 3 日、`TO STDOUT` 約 1〜2 日、`WHERE` 約 0.5 日（KD-19）。
- **M4-Q121 [10-Q8] `transaction_timeout` を受け付けて保存だけにする**: 仮決め: M3 の「`42704`」を上書き。理由: pg_dump 17 が `SET transaction_timeout = 0` を送る（KD-28）。影響: `42704` のままだと pg_dump 17 と `psql -f` のダンプが通らない。
- **M4-Q122 [10-Q9] 混合幅の整数演算子の有無で式の表示が変わる**: 仮決め: `int2` / `int4` / `int8` の混合幅の演算子がある前提で `bi > 5` は `(bi > 5)`。**実測: 既存の `builtin.rs` にすでにある（45 行）ので 09 の追加は不要**。影響: なければ `Cast` が入って表示が PG と違う。
- **M4-Q123 [10-Q10] `pretty`（括弧の最小化）を作る**: 理由: psql の `\d tbl` と SQLAlchemy の reflection が使う。影響: 作らなければ `pretty = true` でも非 pretty の出力（約 1 日の節約）。
- **M4-Q124 [10-Q11] pgbench のパーティション確認の失敗に頼る**: 仮決め: `CROSS JOIN LATERAL` が `0A000` でも pgbench は続行（偽のサーバで確認）。影響: 将来の版が中止するようになったら LATERAL の最小実装が要る。
- **M4-Q125 [10-Q12] COPY の 1 行の長さの上限を 64 MiB にする（`54000`）**: 影響: 値を変えるだけ。
- **M4-Q126 [10-Q13] `tests/slt/m4/psql/` は自分の表に絞った問い合わせ、`tests/compat/psql/` は psql の出力そのもの**: 理由: slt の DB は共有され他のテストの表が残る（M2-Q22）。

### 6.2 この章（11）の確認事項（M4-Q127〜Q141）

- **M4-Q127 [11-Q1] 差分ランダムテストは新しい `yuzhu-fuzz-sql` ではなく既存の `tests/tools/difftest` を仕上げて使う**（D11-6、**→ C-5**）: 仮決め: 00 §4 の `yuzhu-fuzz-sql` は作らない。理由: 約 4,300 行の独立したツールがすでにあり、言語非依存のテストツールは `tests/` に置く（CLAUDE.md）。ただしコミットされた状態はビルドが通らない（実測）。影響: 00 のとおりワークスペースの `yuzhu-fuzz-sql` にするなら `tests/tools/difftest` の移動と `postgres` クレートのワークスペースへの追加（+0.5 日）。difftest を捨てると +約 6 日。
- **M4-Q128 [11-Q2] 工数の増加（115.5 → 140.3 日）と K の 4 分割**（D11-11）: 仮決め: K を K1〜K4（24 日）、S1 5 日、R2 5 日、L2 6.9 日、C1 8 日、E1 6 日。理由: 各章が書いたテスト・機能の量が 00 の見積りを超えた。影響: 最長経路は変わらない（約 21〜23 日）が、担当を絞ると期間が延びる。
- **M4-Q129 [11-Q3] slt のディレクトリに `ddl/` `mem/` `z_final/` を足す**（D11-2）: 理由: `consistency.slt` を最後に流す、`onlyif yuzhu` 専用の `mem/` を見落とさない。影響: 00 §18 の一覧に 3 つ足すだけ。
- **M4-Q130 [11-Q4] 既知の差分の規約**（D11-3）: 仮決め: `# KNOWN-DIFF: KD-<n>` を必須とし、`KNOWN-DIFFS.md`（初期 29 項目）と lint で管理する。理由: M4 は既知の差が多く、`skipif` の外し忘れを防ぐ。影響: 規約をやめると `skipif yuzhu` が増え続ける。
- **M4-Q131 [11-Q5] `plan_variants` はテンプレートから 2 段階で生成し、生成物をコミットする**（D11-4）: 理由: 同じ問い合わせ群を 10 通りの設定で流すので手書きは保守できない。期待値は PostgreSQL で 1 回だけ作る。影響: 手書きにすると保守が破綻する。実行時に生成するとランナーに依存する。
- **M4-Q132 [11-Q6] 再起動シナリオの `mode` ファイルと `NN-*.mode`、`tests/run.sh` の拡張**（D11-5、**→ C-9**）: 理由: 08 のシーケンスは正常停止とクラッシュで期待値が違い、`seq-mixed-restart` は 1 つのシナリオの中で停止の方法が変わる。影響: ディレクトリを `restart` / `crash` で分けるだけにすると mixed が書けない。
- **M4-Q133 [11-Q7] PostgreSQL 回帰テストの取り込みは `tests/imported/pg_regress/`（`tests/slt` の外）に生成し、夜間だけ流して合否にしない**（D11-7）: 理由: 数千文で毎回の CI が遅くなる。取り込みは一致した文だけを機械的に選ぶ。影響: 合否にすると PostgreSQL の挙動の細部への追従で M4 が終わらない。
- **M4-Q134 [11-Q8] クラッシュ試験にワークロード 8（DDL）と不変条件 I16（カタログの整合）、`LossyIndexStore` の変異を足す**（D11-8）: 理由: 07 の依頼。どの不変条件にも検出する変異が最低 1 つある状態にする。影響: 足さないと DDL の WAL の抜けを層 1 が検出しない。
- **M4-Q135 [11-Q9] CI のゲート: pg 側は常に必須、yuzhu 側はフェーズ 4 の開始から必須**（D11-9）: 理由: 実装前に yuzhu 側を必須にすると毎回赤くなる。影響: 早く必須にすると PR が通らない期間ができる。
- **M4-Q136 [11-Q10] 環境: tzdata を 3 か所に明示、psql / pgbench は 17 系、Python を使わず Rust のツール**（D11-10、D11-13）: 理由: 実行用 `Dockerfile` に tzdata がない、sandbox に `python3` と `uv` がない（実測）。影響: `sandbox/Dockerfile` に `python3` を足せば 09 の `.py` のままでよい（イメージの再ビルドが要る）。
- **M4-Q137 [11-Q11] compat テストは `postgres` データベースの `public` だけで動かす**（**→ C-12**、KD-29）: 理由: `CREATE DATABASE` / `CREATE SCHEMA` は M5。10 §7.4 の `createdb` と「複数スキーマ」は動かない。影響: M5 で `createdb` を使う構成に戻せる。
- **M4-Q138 [11-Q12] isolation の 2 本と `pgbench/crash.sh` を足す**（D11-14、D11-15）: 理由: 読み手が書き手を待たないこと・シーケンスが戻らないこと・実プロセスの `kill -9` で pgbench の不変条件（原子性）が保たれること。影響: 足さなくても完了条件は満たせるが、索引・シーケンスの結合の検証が薄くなる。
- **M4-Q139 [11-Q13] M4 の完了条件は §1.3 の 9 項目（01 が未読）**: 仮決め: 機械的に判定できる形。理由: 00 の D-24・D-25 と各章の範囲から組み立てた。影響: 01 が違う条件を定めたら 01 に従う。
- **M4-Q140 [11-Q14] 担当不在だった作業の割り当て**（§7.3 の G-1〜G-4）: 仮決め: CREATE INDEX / DROP INDEX / TRUNCATE / VACUUM / ALTER TABLE の解析（`analyzer/ddl_index.rs`）は C1 に +1.0 日、`nextval` 系の関数の行は Q1、`pg_get_*` の関数の行は E1、`\dt+` 用の関数は T3（任意）。理由: どの章も担当を書いていなかった。影響: 別の担当に移すと日数が移る。
- **M4-Q141 [11-Q15] 章間の食い違いの決め方と、その結果（C-1〜C-20）**（D11-1）: 仮決め: 実機 > 実装の持ち主の章 > ディスク形式の定義の持ち主 > 横断契約（00・02）> 00。理由: 実装者が迷わない。影響: 決定を変えたい食い違いは §7.1 の「変えたい場合」に書いた。

### 6.3 QUESTIONS.md への転記の依頼と、01 章の項目

**転記の依頼**: この章は `QUESTIONS.md` を編集しない（書くのは `spec/design/m4/` 配下の 1 ファイルだけ）。統合時に、次を `QUESTIONS.md` の末尾に**「M4 の設計で仮決めしたこと（`spec/design/m4/`）」**として転記してください。

1. §6.1 と §6.2 の項目を `M4-Q1`〜`M4-Q141` の ID のまま、各 3 点（仮決め・理由・変えたい場合の影響）で。ディスク形式に関わる ★（**M4-Q52・Q53・Q54・Q55・Q57・Q60・Q72・Q73・Q90・Q91・Q92・Q104** の 12 件）は先頭にまとめ、「実装の前に決めるのが望ましい」と添える。
2. 先頭に次の要約を置く（ユーザーが最初に見るもの）: (a) M4 の設計は 11 章（`spec/design/m4/00`〜`11`）。M4 は Q-010〜Q-014 を含む。Q-010（64 ビット XID）は M2 で実現済みで M4 の作業なし、Q-011（numeric）は D-3、Q-012（COPY・char(n)・timestamp）は D-4〜D-6 と D-25、Q-013（作り直し）は D-1・D-2、Q-014（MultiXact の簡易版）は M5。(b) 工数は約 140 日（00 の見積りより増加。最長経路は約 21〜23 日）。(c) 00 を変える提案が 102 件（各章 92 件 + この章 10 件）あり、§7.4 と §9 の表のとおり採否を決めた。(d) 章の間の食い違い 20 件を §7.1 で決めた。
3. 既存の項目への注記: M2-Q8（psql の `\dt` は M4）→ M4 で `\dt` `\dn` `\di` `\l` を完了条件に（D-24）。M3 の「`transaction_timeout` は `42704`」（`m3.md` §1.2）→ M4-Q121 で上書き。M2-Q9（`pg_get_expr` の正規形）→ D-21 で解決。M2-Q22（slt の後始末）→ `slttools lint` と `z_final/no_leftovers.slt` で解決。

**01 章の項目**: `01-scope-decisions.md` は執筆時点で読めなかった。01 に確認事項があれば、`[01-Qn]` のまま **M4-Q142 以降**に同じ形式で追加する（番号が既存の項目に影響しないよう末尾にした）。

---

## 7. 整合性レビュー

01 を除く全章（00、02〜10）を突き合わせた結果。**決め方は D11-1**（実機 > 実装の持ち主の章 > ディスク形式の定義の持ち主 > 横断契約（00・02）> 00）。00 は直さず、決定を「00 への変更提案の採否」（§7.4）と「各章を直す箇所」として残す。**この節で決めたことが各章の本文と食い違う場合は、この節が正**（実装者は該当の章の本文を、ここで決めた形に読み替える）。

### 7.1 章をまたぐ矛盾と決定（C-1〜C-20）

**C-1 `ExplainNode` の形と名前（00 / 02 / 04 / 05 / 10）**
- 食い違い: 00 §9.3 は「`PhysicalPlan` と同形・同じ子の順序」。02 §3.6.5 は「`children` の先頭 n 個が `plan.children()` と 1 対 1。`Hash` は `format.rs` が合成」。04（00 への提案 1）は「同形でない。`plan_id: Option<u32>` と `width`」。05（05-P4）は「`phys_id: Option<u32>`」。10（D10-1、10-P1）は「表示用の木。`exec_id: usize` と `width: u32`、`details: Vec<ExplainDetail>`、合成ノード（`Hash`、`Append`、`Subquery Scan`）は木に入れ、計測値は `exec_id` で借りる」。
- **決定: 10 の `ExplainNode`（`title` / `details: Vec<ExplainDetail>` / `output` / `children` / `exec_id` / `width`）を唯一の定義にする**。`plan_id`・`phys_id` は `exec_id` に統一し、合成ノードは中身のノードの `exec_id` を借りる。同形の約束（00 §9.3、02 §3.6.5）は廃止し「表示用の木」に改める。`PhysicalQuery.explain` と `SubPlanDef.explain` は 00 のまま。`ExplainNode` は物理化の最中に L2 が作る（02-D12、10-D10-2、04 §8 で一致）。
- 直す章: 00 §9.3、02 §3.6.5・§10-9、04 §8.1・§15-1、05 §9・§14（05-P4）。
- 変えたい場合: 同形にすると EXPLAIN の期待値の大半が yuzhu 専用になる（10-Q1）。

**C-2 EXPLAIN ANALYZE の計測の仕組みと通し番号（02 / 05 / 10）**
- 食い違い: 05 §9 は `BuildOptions.instrument: Option<Arc<InstrumentSink>>`、`instrument::wrap(inner, key)`、`NodeKey { scope: PlanScope, index }`（スコープごとの番号）、`Executor::extra_stats()`。10 §3.10 は `Instrumentation` + `NodeCounters`、`Executor::set_counters(id, &Rc<Instrumentation>)`、`ExecCtx.instr`、`executor::build_instrumented`、**全体で 1 つの `exec_id`**（根、`subplans` の昇順、`ctes` の昇順）。02 §3.6.4 の通し番号は 10 と同じ（先行順、根 → サブプラン → CTE）。05 は `PlanScope` を「10 が定義する」と書いたが 10 には定義がない。
- **決定: 10 の方式に統一する**。`Executor::set_counters`（既定は何もしない）、`ExecCtx.instr: Option<Rc<Instrumentation>>`、`Instrumented`、全体の `exec_id`（`planner::physical::assign_exec_ids`。L2 と executor が共有）。05 の `BuildOptions` / `PlanScope` / `NodeKey` / `extra_stats` / 05-P2 / 05-P4 は採らない。各ノードの `Rows Removed by ...` は 10 のとおり、`Filter`・`SeqScan`・`IndexScan`・`NestedLoopJoin`・`NestedLoopParam`・`HashJoin` が `set_counters` を実装して `instr.add_removed` を呼ぶ（X1〜X3 の各ノード +約 0.3 日）。`Sort Space Used` は出さない（10-D10-6）。`build_scoped`（05 §3.2）は 1 つの構築関数のまま、`instr` を `BuildEnv` が持つ。
- 直す章: 05 §3.1・§3.2・§9・§14。10 は変更なし。
- 変えたい場合: 05 の方式（スコープごとの番号）に統一すると 10 の `Instrumentation` が `Vec<Vec<NodeCounters>>` になり、`ExplainNode` がスコープを持つ。

**C-3 `levels_up` の数え方（02 / 03 / 04）**
- 食い違い: 02-D1 は「1 つの `BoundQuery`（本体が Values / SetOp でも）が 1 レベル。集合演算の腕・CTE 本体・導出表・副問い合わせ式はそれぞれ入れ子の `BoundQuery`」。03-D3-20 は「rtable を持つスコープ（`BoundSelect`、DML、Values の行）の入れ子だけを数え、`BoundQuery` は数えない（集合演算の腕・CTE 本体は兄弟）。`CteRef.levels_up` は `BoundQuery` を数える」。04 §4.1 は「`BoundSelect` の入れ子の深さ（導出表の `BoundQuery` も 1 段。副問い合わせの query も 1 段）」。
- **決定: 03 の D3-20 に統一する**（03 が生成し 04 が消費する。2 章が一致している）。02 の §3.4.2 と `BoundQuery::validate` の B1、00 §6.1 のコメントを 03 の定義に書き直す（02-Q9 の影響の見積りは 02 の書き直しのみ）。**集合演算の腕から外側の列を参照するケース**が両者で違う値になるので、`subquery/correlated.slt`（§3.2.2）と差分ランダムテストの相関副問い合わせ（§3.7.3）が確かめる。
- 直す章: 00 §6.1、02 §3.4.2・§6.1（B1）・§10-1。
- 変えたい場合: 02 に揃えると 03（N3）と 04 の `scopes` の積み方を直す（約 +0.5 日）。

**C-4 `min` / `max` の値が等しいときの代表（05 / 09）**
- 食い違い: 05 §6.2 は「比較して Less（Min）/ Greater（Max）のとき置き換える」（等しければ先の値を残す）。09 の D-9-12 は「等しいとき後に来た値を残す」（実測: `max(1.10, 1.1, 1.100)` = `1.100`。PG の `numeric_larger` が `cmp > 0 ? a : b`）。
- **決定: 09（実機）に従う**。`AggState::MinMax` は `cmp(v, cur)` が `Less`（Min は `<=`）/`Greater`（Max は `>=`）つまり**等しいときも置き換える**。`agg/minmax.slt`（`1.100`、float の `-0`、bpchar のパディング）が確かめる。
- 直す章: 05 §6.2。

**C-5 差分ランダムテストの実体（00 / 02 / 06 / 09 / 03 と、既存の `tests/tools/difftest`）**
- 食い違い: 00 §4・§17・§18 は新しい `yuzhu-fuzz-sql`（ワークスペースの bin。依存は `yuzhu-core` と `postgres`）、02 §5.4（P0-b）はそのスタブを置く、03・06・09 は「`yuzhu-fuzz-sql` に〜を含める」。実際には `tests/tools/difftest`（独立した Cargo プロジェクト。TLP・最小化・M4 レベルの JOIN / 集約 / 副問い合わせを持つ約 4,300 行）があり、**ビルドが通らない**（実測）。
- **決定（D11-6）: `yuzhu-fuzz-sql` は作らず、`tests/tools/difftest` を仕上げて M4 向けに拡張する**。03・06・09 の「`yuzhu-fuzz-sql` に含める」はすべて difftest の生成の範囲（§3.7.3）に取り込んだ。02 §5.4 の `yuzhu-fuzz-sql/` スタブの行と 00 §4 の `Cargo.toml` の members の変更を削除する。
- 直す章: 00 §4・§17・§18、02 §5.4。
- 変えたい場合: M4-Q127。

**C-6 溜めた結果を `rewind` で再利用してよいかの判定（00 / 02 / 05）**
- 食い違い: 00 §9.2 は `PhysicalPlan::uses_params()`（木のどこかに `PhysCol::Param`）。02-D7 は「`SubLink` を含めば true」を足して executor が `build` の時点でこれで決める。05 の D5-3 は `uses_params()` の代わりに executor 内部の `free_params(plan, query)`（`SubPlanDef` と `NestedLoopParam` の束縛を越えて自由な `ParamId` を求める）を使う。
- **決定: 05 の `free_params` を使う**（`uses_params()` は SubLink を越えられず、また NestedLoopParam が自分で束縛する Param を「依存」と数えて不要に作り直す）。`PhysicalPlan::uses_params()`（02 の定義: SubLink を含めば true）は残し、`build(plan)`（`PhysicalQuery` を持たない単体テスト用）と他の章のために使う。P0-c の `reusable` の判定は `free_params`。`PhysicalPlan::{children, exprs, bound_params}` は 05-P1 のとおり P0 が置く。
- 直す章: 02 §3.6.4・§3.7.1・02-D7・02-Q3、00 §9.2。

**C-7 テーブル名の接頭辞の衝突（03 / 08）**
- 食い違い: 03 §6.3 は副問い合わせのファイルの接頭辞に `sq_`、08 §7.1 はシーケンスのファイルに `sq_`。
- **決定**: 副問い合わせは `sb_`（§3.2.4）。03 の付録 A-7・A-8 の `sq_` は K1 が機械的に置換する。

**C-8 slt の置き場所の重なり**: §3.2.3 の表のとおり（`agg/types`、`psql/*`、`z_final/consistency`、`ddl/`、`explain/format` と `nodes`、`restart/m4` のシナリオ名）。

**C-9 再起動シナリオのモードと名前（06 / 07 / 08）**
- 食い違い: 07 は `01-index-committed`〜`09-*`（`--restart` と `--crash` の両方）、08 は `seq-clean` などで「期待値が違うのでディレクトリを分ける」と `seq-mixed-restart`（フェーズごとに停止の方法を変える）、06 は 1 つの `index_persist.slt`。`tests/run.sh` はシナリオ単位で `--restart` か `--crash`（M3）。
- **決定（D11-5）**: `mode` ファイルと `NN-*.mode`、名前の統一（§3.4）。

**C-10 executor の API 名の食い違い（02 / 05 / 10）**
- 食い違いと決定（実装を書く担当の章の名前を採る。D11-1）:

| 項目 | 02 | 05 | 10 | 決定 |
|---|---|---|---|---|
| `ExecCtx` の構築 | `ExecCtx::new(ExecEnv, txn, query)` | `QueryState::new(query, mem_limit, opts)` | — | **`ExecCtx::new(env, txn, query)`**（02）。`ExecEnv` に `mem_limit` と `instr`。`QueryState` は `new` の内部の補助 |
| `SubPlanStates` | `new(query)`、`take_exec` / `put_exec`、`SubPlanState`、`HashedSet` | `new(query, ..)`、`take` / `put_back`、`SubPlanSlot`、`SubPlanCache`、`HashedSubPlan` | — | **05**（X1 のファイル） |
| `AggState` | `new(&PhysAgg)`、`accumulate`、`finish` | `new(kind)`、`transition`、`result`、`AggSet` / `AggGroup` | — | **05**（X2 のファイル）。P0-c の `executor/agg.rs` のスタブは 05 の署名で置く |
| SubLink の評価 | `eval_with_sub` | `eval_with_sub_row` | — | **`eval_with_sub_row`**（05。`pub(crate)`） |
| NOT NULL / CHECK の検査 | — | `RowChecker::{new, check}`、`RowBuilder` | `RowChecks` + `check_row`（10-P4） | **`RowChecker` / `RowBuilder`**（05）。COPY（O1）もこれを使う |
| 問い合わせ全体の構築 | `ExecCtx::new` | `build(plan)`（単体テスト）+ `build_query(&PhysicalQuery, ..)` | `build_instrumented` | `build` と `build_query` を持ち、計測は `BuildEnv.instr`（C-2） |
| 空の `PhysicalQuery` | `PhysicalQuery::single(root, output)` | — | `PhysicalQuery::empty()` | 両方（`empty()` = 行を出さない `single`）。A が置く |

- 直す章: 02 §3.7.2・§3.7.3・§5.4（P0-c のスタブ）、10 §14.1-2・-4。

**C-11 `IndexStore::insert` の `23505` の DETAIL と `BuildUnique`（00 §13.2 / 05 / 06 / 07）**
- 4 章は互いに一致している（05-D5-16、06-Q8、07-D07-7）が、**00 §13.2 の記述と違う**ので明記する: `IndexStore::insert` は `23505` と `s` / `t` / `n` を付けて返し、**DETAIL（`Key (a)=(1) already exists.`）は `dml.rs` の `unique_violation_detail` が補う**。`BuildUnique::Yes` は「入力のすべてが生きている版として、隣接するキー（NULL を含まない）が全列等しければ `23505`（DETAIL なし）」で、C1 は使わず `No` で呼び、**重複の検出と DETAIL（`could not create unique index` / `Key (a)=(1) is duplicated.`）は C1 が書く**（死んだ版の誤検出を避ける）。
- 直す章: 00 §13.2（コメント）。

**C-12 compat テストの `createdb` と「複数スキーマ」（10 と 00 §3）**
- 食い違い: 10 §7.4・§8.3 は「空の DB を作ってから流す: `createdb compat_psql`」「複数スキーマ」。M4 の yuzhu には `CREATE DATABASE` も `CREATE SCHEMA` もない（00 §3、07、03）。
- **決定**: `postgres` データベースの `public` だけで流し、新しいサーバで流す（§3.5.1）。`\l` の差は正規化する。KD-29。
- 直す章: 10 §7.4・§8.3。変えたい場合: M5 で `createdb` を使う構成に戻せる。

**C-13 B+Tree の構造変更のブロック数（00 / 06 / 調査）**: 00 §13.4・§13.5 は「`2h + 3`、h > 14 で `54000`」、調査は「`3h + 2`、h <= 10」。**決定: 06 の `3h + 1`（各レベルで左・右・元の右隣）。静的な高さの上限は作らず、必要なブロック数が `MAX_BLOCK_REFS = 32` を超えたときだけ `54000`**。00 §13.4・§13.5 を直す（06 の変更提案 1）。クラッシュ試験のワークロード 6 は `shared_buffers = 24`。

**C-14 `ExprKind::Cast` の拡張（00 / 02 / 09 / 10）**: 00 §6.2 と 02 §3.1.2 は `Cast { expr, method }`。09-P3 は `CastMethod::Env(fn(&[Datum], &TypeEnv) -> Result<Datum>)`、10-P3 は `Cast { expr, method, implicit: bool }`。**決定: 両方採る**。A が `ExprKind::Cast { expr, method, implicit }` と `CastMethod::Env` を置き、`walk` / `try_map` / `same_as`（`implicit` は無視して比べる）/ `validate` / `deparse` が扱う。アナライザは暗黙のキャストを挿入するとき `implicit = true`。`Cast(Env)` と `FnKind::Runtime` の関数は定数畳み込みしない（09 D-9-4、04 R1 が従う）。

**C-15 `CURRENT_TIMESTAMP` などの表現（09 と 10）**: 10 §4.4 は「`current_timestamp` などを別の `BuiltinFunction` の行にしてほしい」、09 D-9-6 は `SessionValueKind::{CurrentDate, CurrentTimestamp { precision }, LocalTimestamp { precision }}`。**決定: 09**。10 の deparse は `SessionValue` の変種を `CURRENT_DATE` / `CURRENT_TIMESTAMP[(p)]` / `LOCALTIMESTAMP[(p)]` と出す（10 §4.4 の表の `SessionValue` の行に足す）。10 §14.2 の 09 への依頼（別の関数の行）は不要。

**C-16 DDL の細部で 00 を変えるもの（00 / 07）**: 00 の D-11 は「`VACUUM` / `ANALYZE` はブロック内で `25001`」、D-12 は「`OWNER TO` は何もしない」。07 は実測で `ANALYZE` はブロック内で成功、`VACUUM` だけ `25001`、`OWNER TO` はロールを検証して `relowner` を更新（07-P3・P4）。**決定: 07（実機）**。00 の D-11・D-12 を直す。

**C-17 `CatalogNames` の置き場所（09 と 00 §17）**: 09 §3.4 は `CatalogNames` を `catalog/reader.rs` の末尾に置くとしたが、そのファイルの持ち主は C1。**決定: 新しい `catalog/names.rs`（T3）**。

**C-18 `Settings::type_env` の署名（00 §14.2 と 09）**: 00 は `type_env(&self, zones)`、09-P4 は `now`・`DateTimeSettings`・`names` を足す。**決定: 09 の署名**（`Settings::type_env<'a>(&'a self, ds: &'a DateTimeSettings, zones: &'a ZoneDb, now: i64, names: Option<&'a dyn OidNames>) -> TypeEnv<'a>`）。S と T2 が `settings.rs` を関数で分ける。

**C-19 定数の ON の FULL JOIN（03 / 04 / 05）**: 03 は「PG は `ON true` の FULL を通す。04 / 05 が決める」、04-D10 は「FULL は Hash のみ（キーがなければ `0A000`）」、05 は「NLJ の FULL は内部エラー」。**決定: ハッシュ可能な等値がない FULL は `0A000`（PG と同じ文言）。`ON true` も含む（KD-2）。NLJ の FULL は作らない**。`join/outer_and_names.slt` が対で書く（PG 側 `skipif yuzhu`）。

**C-20 `analyze_query` の持ち主（03 の N2 / N3）**: 03 §7 は N2 に `select.rs`、N3 に「`analyze_query` の統合 0.5 日」を割り当てたが、`analyze_query` は `select.rs` にある。**決定**: `analyze_query` の骨格（WITH・集合演算・Values・Nested の振り分け）は P0-d が置き、N3 は `setop.rs` / `cte.rs` / `sublink.rs` の中だけを書く。N3 の 0.5 日は分岐の動作確認。`select.rs` は N2 だけが編集する（§4.2）。

### 7.2 署名・名前・OID・SQLSTATE の確認

**SQLSTATE（`error.rs` の `sqlstate` に追加するもの。11 個。D11-12）**: 00 §15.3 の 6 個と、他章が足した 5 個。重複して足さない（既存の `INVALID_ROW_COUNT_IN_LIMIT_CLAUSE` `2201W`、`INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE` `2201X` などは `error.rs` に既にある）。

| 定数 | コード | 出典 |
|---|---|---|
| `SEQUENCE_GENERATOR_LIMIT_EXCEEDED` | `2200H` | 00 §15.3、08 |
| `INVALID_REGULAR_EXPRESSION` | `2201B` | 00 §15.3、09 |
| `BAD_COPY_FILE_FORMAT` | `22P04` | 00 §15.3、10 |
| `DEPENDENT_OBJECTS_STILL_EXIST` | `2BP01` | 00 §15.3、07、08 |
| `DUPLICATE_ALIAS` | `42712` | 00 §15.3、03 |
| `GENERATED_ALWAYS` | `428C9` | 00 §15.3、05、08 |
| `INVALID_DATETIME_FORMAT` | `22007` | 09-P2 |
| `DATETIME_FIELD_OVERFLOW` | `22008` | 09-P2 |
| `INVALID_TIME_ZONE_DISPLACEMENT_VALUE` | `22009` | 09-P2 |
| `STATEMENT_TOO_COMPLEX` | `54001` | 04 §10（04-P9） |
| `RESERVED_NAME` | `42939` | 07-P10（S1 が足す） |

使われているのに追加しないもの: `42P21`（明示照合順序の衝突。03 は検出しない）、`XX002`（06-Q14 は `XX001`）。**0A000 の文言（`... is not supported yet`）の形**は 02 §5.4 のとおり M1 の書式。

**OID**: 00 §11.5・§12.1 と 06（`pg_opfamily` 8 行・`pg_opclass` 15 行・`pg_amop` 120 行・`pg_amproc` 24 行）、07（カタログ OID 9 個、`pg_proc` の比較関数）、08（`nextval` 1574、`currval` 1575、`setval` 1576 / 1765、`lastval` 2559、`pg_get_serial_sequence` 1665）、09（型 OID、集約 43 行、演算子・関数・キャスト）を突き合わせ、**食い違いは見つからなかった**（`pg_opfamily` の OID は 06 と 07 が一致: `bool_ops` 424、`bpchar_ops` 426、`float_ops` 1970、`integer_ops` 1976、`numeric_ops` 1988、`oid_ops` 1989、`text_ops` 1994、`datetime_ops` 434）。07 の `pg_proc` の比較関数の表は日時の型をまたぐ 6 個（`date_cmp_timestamp` ほか）を含むが、09-D-9-5 と 06-Q9 に従い**要らない**（24 個）。実機で確かめていない OID は U18・U19。

**署名の確認**: 00 の署名のうち他章が変えたもの（`ExecCtx`・`ExplainNode`・`Cast`・`Settings::type_env`・`PlanEnv`・`TypeEnv`・`BoundIndexConstraint`・`DdlCtx`・`SequenceHandle`・`Transaction`・`CatalogReader`・`TableStore::tuple_state` の契約）は §7.4 の表で採否を決めた。**名前の衝突は C-10 の表で解決**した。`HashKey` / `hash_datum` / `cmp_with_nulls` / `AggKind` / `BuiltinAggregate` / `IndexStore` / `ResolvedScanKeys` は 00・05・06・09 で一致している。

### 7.3 担当が決まっていなかった作業（G-1〜G-5）

| # | 作業 | 状況 | 決定 |
|---|---|---|---|
| G-1 | `CREATE INDEX` / `DROP INDEX` / `DROP TABLE`（`behavior`）/ `TRUNCATE` / `VACUUM` / `ALTER TABLE ... ADD / OWNER TO` の**解析**（AST → `Bound*`。名前の解決・`42809` / `42P01` / `42704` の判定・列と opclass の解決） | 07 §4.6 が `Bound*` の型と「呼び出し側の責任」の検査を書いたが、解析を書く担当は 00 §17 にも 07 にもなかった（07 は PK / UNIQUE の `ddl_constraint.rs` だけ） | **C1 が新しいファイル `analyzer/ddl_index.rs` に書く（+1.0 日。C1 は 8.0 日）**。`analyzer/ddl.rs`（Q1）には 1 行の呼び出しだけ依頼 |
| G-2 | 関数の行（`catalog/builtin.rs`）: `nextval` / `currval` / `setval`（2 つ）/ `lastval`（08 は T3 が足すと書いたが 09 の表にない）、`pg_get_constraintdef` / `pg_get_indexdef`（07 は 10、10 は 07 に依頼） | どの章も書いていない | **`nextval` 系 5 行は Q1（`sequence` 区画）。`pg_get_constraintdef` / `pg_get_indexdef` の行と本体は E1（`explain-deparse` 区画。`pg_get_expr` は既存）** |
| G-3 | `pg_size_pretty` / `pg_table_size` / `obj_description` / `pg_function_is_visible`（`\dt+` と `\df`。10 が 09 に依頼） | 09 の表にない（任意の機能） | T3（任意。+0.3 日）。`\dt+` `\df` は任意なので完了条件ではない |
| G-4 | tzdata（`Dockerfile`・`sandbox/Dockerfile`・CI）、psql / pgbench 17 の入手、Python の不在 | 09 が K に依頼、他は誰も書いていない | **K4**（D11-10。§3.11） |
| G-5 | `"char"` 型の入力 `''`（= `\0`。`relkind IN ('r','p','')`）と `name` 型の `~` `!~` `<>` | 10 が 09 に依頼。09 は `name ~ text`（639・640）の行を持つが `"char"` の `''` は書いていない | T3（`types/sys.rs`。`psql/relkind_in.slt` が確かめる。既存の実装で足りるかは未検証） |

また**依頼を受けた側が日数を見込んでいないもの**: 03 の N1（`expr.rs` / `coerce.rs` の依頼 +0.3 日）、08 の Q1（`analyzer/ddl.rs` の型名 +0.3 日）は §4.1 に加えた。

### 7.4 各章の「00 への変更提案」の採否

**採否の記号**: ○ = 採用、△ = 条件つき・修正して採用、× = 採用しない（理由と代替を書く）。同じ内容の提案は 1 行にまとめた。**00 は直さない**（この表を統合時に反映する）。

**02**（14 件）

| # | 提案（要点） | 採否 | 理由 |
|---|---|---|---|
| 02-1 | `Var.levels_up` のコメントを「`BoundQuery` ごとに 1 レベル」に | **×** | C-3。03 の D3-20（スコープだけを数える）に統一する。00 §6.1 の注は 03 の定義で書き直す |
| 02-2 | `try_map` の規則、`try_rewrite`・`any`・`contains_aggregate`・`contains_sublink`・`conjuncts`・`same_as`・`columns`・`Expr::{new, literal, ..}`・`Var::{with_levels_up, ..}` の追加 | ○ | 追加のみ |
| 02-3 | 論理・物理の `span` は変換元を保つ | ○ | 費用がなく診断に使える |
| 02-4 | §6.4 の表を 02 §3.2.1 に置き換える（物理の `SubLink.test` は常に `None`） | ○ | 04-6 と一致 |
| 02-5 | Join RTE を指す `Var` を Bound に残さない、`returns_rows`、`BoundQuery::validate` | ○ | 03-P3-1 と一致 |
| 02-6 | §8 に走査 API・`validate` を追加。`n_subplans_hint` は使わなくてよい | ○ | |
| 02-7 | `PlannerSettings.validate_plans`（隠し設定 `yuzhu.validate_plans`） | ○ | S が `settings.rs` に足す。CI の `slt-yuzhu` が on で起動（M4-Q7） |
| 02-8 | `uses_params` に「SubLink を含めば true」、`children` / `width` / `exprs_with_width` / `validate` / `single` | △ | C-6。`uses_params()` は残すが executor は 05 の `free_params`。ほかは採用 |
| 02-9 | `SubPlanId` / `ParamId` の採番（後行順）、`ExplainNode.children` の先頭が `plan.children()` と 1 対 1、計測の通し番号 | △ | 採番と通し番号は採用。**1 対 1 は不採用**（C-1） |
| 02-10 | `ExecEnv`、`ExecCtx::new`、`EvalCtx.type_env`、`eval_with_sub`、`rewind` は未開始のノードにも可 | △ | `eval_with_sub_row`（C-10）。ほかは採用 |
| 02-11 | §4 構成図に `analyzer/query.rs`・`planner/legacy.rs` は一時ファイルと注記 | ○ | |
| 02-12 | §17 の P0 を段階ごとに置く・解放する | ○ | §4.3 の日程の前提 |
| 02-13 | §16 の表に 02 の行を追記 | ○ | |
| 02-14 | `StorageStack.index` / `seq`、`Cluster::indexes()` / `sequences()` を P0-b が足す | ○ | |

**03**（11 件）

| # | 提案（要点） | 採否 | 理由 |
|---|---|---|---|
| P3-1 | `BoundExpr` の `Var` は Join RTE を指さない | ○ | 02-5 と同じ |
| P3-2 | `group_by` の注: 関数従属で許された `Var` が末尾に追加されうる | ○ | 04-Q13 と一致 |
| P3-3 | `BoundDistinct::On` の位置の意味（distinct の順序） | ○ | 04 が従う |
| P3-4 | `BoundQuery::walk_exprs`（深さつき走査）を A / P0 が足す | ○ | 03 §5.5.3・§5.12.2・04 が共有。A に追加（§4.1） |
| P3-5 | `levels_up` の数え方（3.2.1）を 00 に書く。Values の行は rtable が空の 1 つのスコープ | ○ | **C-3 の決定そのもの** |
| P3-6 | `BoundCte.col_aliases` の注 | ○ | |
| P3-7 | AST の表（00 §7.1）を 03 §3.1 の具体化に直す | ○ | S1 の仕様 |
| P3-8 | 追加の SQLSTATE は不要（`DUPLICATE_ALIAS` だけ） | △ | 04（`54001`）・07（`42939`）・09（3 個）が足す。§7.2 の 11 個 |
| P3-9 | `catalog::builtin::is_set_returning` | ○ | T3 |
| P3-10 | テスト用モジュール `analyzer/tests_*.rs`、`sql/parser/tests_query.rs` | ○ | §4.2 の新しいファイル |
| P3-11 | `analyzer/scope.rs` の `ExprKind`（M1）を `ParseExprKind` に改名 | ○ | P0。名前の衝突を避ける |

**04**（11 件）

| # | 提案（要点） | 採否 | 理由 |
|---|---|---|---|
| 04-1 | `ExplainNode` に `plan_id` と `width`、`PlanEnv.explain_verbose`、`PlanEnv: Clone + Copy` | △ | **`plan_id` は不採用**（C-1: `exec_id`）。`width`・`explain_verbose`・`Copy` は採用。「同形」の文言は「表示用の木」に改める |
| 04-2 | `ExplainNode.children` は印字する順（InitPlan / CTE が先、SubPlan が後） | ○ | 10 と一致 |
| 04-3 | 定数畳み込みのために `planner::rules::const_fold` が `executor::eval::eval_const` を 1 か所だけ呼ぶ。`EvalCtx::for_constant_folding` を 05 が提供 | △ | 依存の向き（`planner` → `executor`）の例外として認める（1 か所）。05 が `for_constant_folding` を持つ（U36）。代案: `expr/eval.rs` に純粋な評価器を移す（大きい。M5 の宿題） |
| 04-4 | ファイルの追加（`planner/{util,size,print,validate}.rs`、`rules/testutil.rs`）、`plan_golden` を L1・L2 共同 | ○ | §4.2 |
| 04-5 | `LogicalCte.inline` は常に `false`。`n_subplans_hint` は容量の見積りだけ | ○ | |
| 04-6 | 物理の `SubLink` の `test = None`、比較式は `SubPlanDef.test` | ○ | 02-4 と一致 |
| 04-7 | deparse の API はコールバック（`pg_get_expr` は Var → 列名、EXPLAIN は `ExplainNames`） | ○ | 10 の `ColumnNamer` / `SubLinkRenderer`（D10-3）と同じ考え。名前は 10 |
| 04-8 | `catalog::builtin::operator_by_oid` | ○ | T3 |
| 04-9 | SQLSTATE `STATEMENT_TOO_COMPLEX` `54001` | ○ | §7.2 |
| 04-10 | アナライザの前提の明文化（`levels_up`、`BETWEEN` の分解、`overriding`、集合演算の ORDER BY） | ○ | 03 と合わせる |
| 04-11 | `PhysicalPlan::Result` の意味（`Empty` もこの形） | ○ | 05 に伝える |

**05**（8 件）

| # | 提案（要点） | 採否 | 理由 |
|---|---|---|---|
| 05-P1 | `PhysicalPlan::{children, exprs, bound_params}`。`uses_params()` は残す | ○ | C-6。02 の `children` / `exprs_with_width` と統合して P0 が置く |
| 05-P2 | `Executor::extra_stats()` | **×** | C-2（10 の `set_counters`） |
| 05-P3 | `EvalCtx` に `type_env` と `params` | ○ | 04-3 の `for_constant_folding` も要る |
| 05-P4 | `ExplainNode.phys_id` | **×** | C-1（`exec_id`） |
| 05-P5 | `build_query`・`BuildOptions`・`QueryState` | △ | `build_query` は採用。`BuildOptions` は不採用（C-2）、`QueryState` は `ExecCtx::new` の内部（C-10） |
| 05-P6 | `IndexStore::insert` は変更なし（23505 の `detail` は付けても付けなくてもよい） | ○ | C-11 |
| 05-P7 | `NestedLoopJoin` の注: outer は left、inner は right | ○ | |
| 05-P8 | 04 への依頼（相関のある MATERIALIZED CTE を `0A000`、checks の整列は executor） | ○ | 04-Q11 と一致 |

**06**（11 件）

| # | 提案（要点） | 採否 | 理由 |
|---|---|---|---|
| 06-1 | 構造変更のブロック数 `3h + 1`、静的な高さの上限なし | ○ | C-13 |
| 06-2 | §13.5 の表の追加（ピボットの形・境界の向き・予約フラグ・`BTREE_PAGES` の `reason`・`BTREE_INSERT_LEAF` の main data） | ○ | ★（M4-Q52・Q55） |
| 06-3 | `BuildUnique::Yes` の定義、`BuildStats.levels`、`insert` の `23505` は DETAIL なし | ○ | C-11 |
| 06-4 | opclass の表（120 / 24 / 15 / 8）、関数の追加（`family_of_type` ほか） | ○ | ★（M4-Q60） |
| 06-5 | `storage::btree::cmp_keys` / `cmp_key_tid` の公開 | ○ | 07 の `build_from_heap` が使う（07-P8） |
| 06-6 | `tuple_state` は `InsertInProgress` / `DeleteInProgress` を返さない、`fetch_dirty` は `WaitFor` を返さない、`begin_scan_all` は全バージョン | ○ | 07-P7 と一致 |
| 06-7 | M3 の C に `read_tree` / `write_tree` / `extend_tree`、H4 の範囲に `encode_attr` / `ColumnCursor` の公開と新しい型の符号化 | ○ | §4.1 の M3 の持ち主への依頼 |
| 06-8 | 定数の追加（`BT_PAGE_USABLE` ほか） | ○ | |
| 06-9 | `storage/btree/testing.rs`（`#[cfg(test)]`） | ○ | |
| 06-10 | `DebugKnobs::btree_split_in_two_records`（任意） | ○ | §3.6.3 |
| 06-11 | ワークロード 6 の `shared_buffers = 24` | ○ | §3.6.1 |

**07**（11 件）

| # | 提案（要点） | 採否 | 理由 |
|---|---|---|---|
| 07-1 | `BoundIndexConstraint.name: Option<String>`、`options`、`span`。`BoundCreateTable` に `options` と `default_refs`。ほかの `Bound*` は 07 §4.6 を正とする | ○ | M4-Q87 |
| 07-2 | `DdlCtx` に `in_transaction_block` と `interrupts` | ○ | |
| 07-3 | D-11: `VACUUM` はブロック内で `25001`、`ANALYZE` は成功 | ○ | C-16 |
| 07-4 | D-12: `OWNER TO` はロールを検証し `relowner` を更新 | ○ | C-16 |
| 07-5 | `CatalogReader::constraint_by_oid` と `ConstraintDef` | ○ | E1 が使う |
| 07-6 | `pg_description` は空、`pg_language` は 3 行、`reltype = 0` | ○ | |
| 07-7 | `TableStore::tuple_state` の契約（自分が挿入して削除していない版は `Live`） | ○ | 06-6 と同じ |
| 07-8 | C1 は `BuildUnique::No` だけ。06 に `cmp_keys` の公開を依頼 | ○ | C-11、06-5 |
| 07-9 | モジュールの追加（`catalog/{naming,depend,check}.rs`、`ddl/vacuum.rs`、`analyzer/ddl_constraint.rs`） | ○ | §4.2。**`analyzer/ddl_index.rs` を足す**（G-1） |
| 07-10 | SQLSTATE `42939`（S1 が `RESERVED_NAME` として足す） | ○ | §7.2 |
| 07-11 | `int2vector`・`_int2` の行を 09 に要求 | ○ | 09 が持つ |

**08**（11 件）

| # | 提案（要点） | 採否 | 理由 |
|---|---|---|---|
| 08-1 | `SequenceHandle.name`、`SeqState: PartialEq + Eq`、`SeqRun.wal_lsn` の意味、`reset_generation`、`init` / `reset` の意味 | ○ | ★（M4-Q91、M4-Q97） |
| 08-2 | `Transaction.note_wal`、`started_at` の意味、`finish_without_xid` は XID なしの COMMIT のときだけ flush | ○ | |
| 08-3 | `catalog/seq_params.rs`・`executor/seq.rs`、`analyzer/ddl.rs` に `choose_relation_name` ほか | ○ | §4.2 |
| 08-4 | `BoundCreateSequence` / `BoundAlterSequence` / `BoundDropSequence` / `SeqOwner` / `OwnedByTarget` | ○ | |
| 08-5 | `CatalogReader::sequence_owned_by_column`（任意） | ○ | 任意 |
| 08-6 | `pg_sequence` の列、`pg_depend` に DEFAULT 式の regclass 定数への依存 | ○ | |
| 08-7 | `SEQ_LOG` の形式、定数 `SEQ_SPECIAL_SIZE` ほか | ○ | ★（M4-Q90・Q91） |
| 08-8 | 関数 5 行（OID 1574 ほか）と `pg_cast`（`text → regclass` ほか）、`regclass` リテラルは解析時に評価 | △ | 関数の行は **Q1 が足す**（G-2。08 は T3 と書いたが 09 にない）。`pg_cast` は 09 |
| 08-9 | M3 の持ち主への要求（`Page::init_special`、`page_mut_hint`、`Wal::redo_lsn()`、`dispatch`、`DebugKnobs`、`Session`） | ○ | U9・U10・U16、§4.1 |
| 08-10 | C1 への要求（`CatalogStore` のメソッド、`ddl/depend.rs` の API、CREATE TABLE への差し込み、DROP・TRUNCATE からの呼び出し、エラー） | ○ | 07 が受けている |
| 08-11 | テストの置き場所（`seq/`、`restart/m4/seq-*`） | △ | `restart/m4` の名前と `mode`（C-9） |

**09**（7 件）

| # | 提案（要点） | 採否 | 理由 |
|---|---|---|---|
| P-1 | `TypeEnv.names: Option<&dyn OidNames>`、`output_text_regproc` を廃止 | ○ | M4-Q111 |
| P-2 | SQLSTATE `22007` `22008` `22009` | ○ | §7.2 |
| P-3 | `CastMethod::Env` | ○ | C-14 |
| P-4 | `Settings::type_env` の引数（`DateTimeSettings`、`now`、`names`） | ○ | C-18 |
| P-5 | `FnKind::Set(SetFn)` | ○ | |
| P-6 | `SessionValueKind::{CurrentDate, CurrentTimestamp, LocalTimestamp}` | ○ | C-15 |
| P-7 | `oid::ANY = 2276` | ○ | |

**10**（8 件）

| # | 提案（要点） | 採否 | 理由 |
|---|---|---|---|
| 10-1 | `ExplainNode` の拡張（`details: Vec<ExplainDetail>`、`exec_id`、`width`、`assign_exec_ids`）、「同形」を「表示用の木」に | ○ | **C-1 の基準** |
| 10-2 | `Executor::set_counters`、`ExecCtx.instr`、`Instrumentation`、`build_instrumented`、`PhysicalQuery::empty()` | ○ | C-2、C-10 |
| 10-3 | `ExprKind::Cast` に `implicit: bool` | ○ | C-14 |
| 10-4 | `executor/dml.rs` に `RowChecks` + `check_row` | △ | 05 の `RowChecker` / `RowBuilder` を使う（C-10）。COPY（O1）が共有 |
| 10-5 | `ResultSink` / `Session` の COPY の追加（`copy_poll`、`PendingQuery`、`run_from`）、`BackendMessage::CopyInResponse` ほか | ○ | |
| 10-6 | `BoundCopy` と `CopyOptions` の定義 | ○ | |
| 10-7 | `transaction_timeout` は受け付けて保存だけ。`INERT_GUCS` | ○ | M4-Q121。M3 の `42704` を上書き（U45 で既存 slt との矛盾を確認） |
| 10-8 | `explain::node::count_rtes` | ○ | |

**11（この章）→ 00 への変更提案**は §9。

## 8. M5 以降の宿題の一覧

00 §19 と各章から集めた。**M4 では入れない、ただし壊さない**もの（00 §19 の予約）と、**M5 / M6 で足すもの**。出典を括弧に書く。

| 分類 | 内容 | 出典 |
|---|---|---|
| B+Tree（M5） | `cycleid`、`DELETED` / `HALF_DEAD` / `INCOMPLETE_SPLIT` のフラグ、`BTREE_*` の `0x20` 以降の info（DELETE・VACUUM・UNLINK_PAGE）、ピボットの ALT_TID / posting のビット、`UniqueCheck` の `WaitFor` と親の探し直し（`_bt_getstackbuf` 相当）、`_bt_moveright` の「削除済みなら右へ」、**項目の削除（LP_DEAD・簡易削除・VACUUM との連動、クリーンアップロック）**、suffix truncation・重複排除、`fetch_dirty` / `tuple_state` に実行中のトランザクションの判定、`walk_left` の削除ページの分岐 | 00 §19、06-Q10・Q11・Q17 |
| 実行（M5〜M6） | `IndexScan` の Index Only 化、`IN` / `OR` / `LIKE 'abc%'` のインデックス検索、マージ結合、スピル（`work_mem`）、Top-N、並列、`heap_multi_insert` 相当（COPY）、相関のある MATERIALIZED CTE（`CteStates` の作り直し）、相関副問い合わせの結果の再利用、`LATERAL`、`WITH RECURSIVE`、データ変更 CTE、ウィンドウ関数・`GROUPING SETS`、集約内の `ORDER BY`、`string_agg` / `array_agg`、外側レベルの集約、`IS DISTINCT FROM`（M4 後半に入れなければ） | 00 §19、03・05 |
| 型（M5） | 配列（`ANY(array)`、`ARRAY(SELECT ...)`、`oid[]` / `int2[]` の演算。`\d tbl` の完全な一致）、`interval` / `time` / `timetz`、`numeric` の `sqrt` `power` `ln`、`to_char`、`extract` / `date_trunc` / `age`、型をまたぐ日時の比較演算子（`OpFn::{Pure, Env}`）、正規表現の後方参照・先読み、`pg_typeof` の評価、`timestamp(p)` の `WARNING`、DEFAULT の `'now'` を保存時に畳む、`generate_series(numeric / 日時)` | 09-Q2・Q4・Q5・Q6・Q7・Q10、10-Q6 |
| DDL（M5） | `ALTER TABLE` の残り（`DROP CONSTRAINT`・`ADD COLUMN` など）、`CREATE VIEW`、FOREIGN KEY（`pgbench -I dtgvpf`）、`DEFERRABLE`、`TEMP` / `UNLOGGED` シーケンス、`ALTER SEQUENCE RENAME`、カタログのインデックス、起動時の孤児ファイルの掃除、`COMMENT ON`、`pg_depend` に名前空間・CHECK の列の依存、reloptions の保存と `autovacuum_enabled` などの受け付け、`CREATE SCHEMA` / `DROP SCHEMA` / `CREATE DATABASE` | 00 §19、07、08、09 |
| シーケンス | `ALTER SEQUENCE` の新しい relfilenode 方式（M5 の表ロック）、複数ライターでの `nextval`（00 §19: 排他ラッチだけで安全）、`DISCARD SEQUENCES` | 08-Q5・Q6 |
| COPY・EXPLAIN・ツール | COPY の CSV・バイナリ・`TO`・`WHERE`・`ON_ERROR`・Extended Query 経由、EXPLAIN の `FORMAT JSON` / `XML` / `YAML`・`BUFFERS` の出力、`\d tbl` の完全な一致、pg_dump（M6）、ORM（psycopg 3 / SQLAlchemy / Rails / Prisma）、`interval` を使う `pgbench` のスクリプト | 10-Q4・Q6・Q7、pg-compat |
| プランナ | コストベース最適化・統計（M6）、プランのキャッシュと再計画（Extended Query）、計画の再帰の深さ・計画時間の見直し、`EXISTS` のハッシュ化 SubPlan、左結合の ON の `EXISTS` の引き上げ、等値の推移 | 04-Q3・Q10・Q14 |
| テスト | **SQLite の sqllogictest のコーパス**（`select1-5`、`random/`）を PostgreSQL で `--override` して取り込む、PostgreSQL 回帰テストの取り込みの範囲拡大（M5 の機能を `deny.txt` から外す）、`tests/compat/{restore,drivers}`（pg_dump のリストア、psycopg2 / SQLAlchemy）、`yuzhu-isolation` の複数ライター、Jepsen / Elle 系の履歴検査、`planner` が `executor::eval` を呼ぶ依存の向きの解消（`expr/eval.rs`） | 11、00 §19、research-slt |
| 運用 | `m4-changes.md`（00 §20）の運用、`PROGRESS.md` の更新 | 00 §20 |

## 9. 00 への変更提案（この章の分）

**00 は直さず**、統合時に反映してもらう。署名と名前は変えず、足す・明確にするだけ。

| # | 場所 | 提案 | 理由 |
|---|---|---|---|
| P11-1 | §4（クレート構成）、§17、§18 | `yuzhu-fuzz-sql` を削除し、「差分ランダムテストは `tests/tools/difftest`（独立した Cargo プロジェクト）」に。`Cargo.toml` の members の変更を取り消す。02 §5.4 の `yuzhu-fuzz-sql/` スタブの行を削除 | C-5、D11-6。既存の約 4,300 行を使う |
| P11-2 | §17（担当表） | §4.1 の確定表（日数・K1〜K4・新しいファイルの持ち主・K の範囲に `Dockerfile` と `sandbox/Dockerfile`・C1 の `analyzer/ddl_index.rs`・Q1 の `builtin.rs` の `sequence` 区画・T3 の `catalog/names.rs`・N1 の `expr` / `coerce`・X1 の `eval.rs`）に置き換える。合計 115.5 日 → 140.3 日。§4.2 の共有ファイルの区画の規則を追記 | §4.1・§4.2 |
| P11-3 | §4（新しいファイル） | §4.2 の「新しいファイル」の行を追加（`catalog/{seq_params,naming,depend,check,names}.rs`、`executor/seq.rs`、`ddl/vacuum.rs`、`analyzer/{ddl_constraint,ddl_index,tests_*}.rs`、`planner/{util,size,print,validate}.rs`、`storage/btree/testing.rs`、`types/typmod.rs`、`tests/tools/slttools/*`） | 各章の提案の集約（07-9、08-3、04-4、06-9、03-P3-10） |
| P11-4 | §18（テストの置き場所） | `tests/slt/m4/` に `ddl/`・`mem/`・`z_final/` を足す。`tests/gen/`・`tests/imported/`・`tests/tools/slttools/`・`tests/compat/` の構成（§3.5）・`tests/restart/m4/<シナリオ>/{mode, NN-*.mode}`（§3.4）・isolation の 2 本（§3.4.3）・`KNOWN-DIFFS.md`（§3.2.7）を追記 | D11-2、D11-3、D11-5、D11-14 |
| P11-5 | §18（crash_sim） | ワークロード 8（DDL）、不変条件 I16（カタログの整合）、`LossyIndexStore` の変異、ワークロードごとの `shared_buffers`（6 は 24）、CI の時間の目安 | D11-8、06-11 |
| P11-6 | §15.3 | SQLSTATE の追加を §7.2 の 11 個に（00 の 6 個 + `22007` `22008` `22009` `54001` `42939`） | 09-P2、04-9、07-10 |
| P11-7 | §13・`debug_knobs.rs` | `DebugKnobs` に `btree_split_in_two_records`（任意）、`seq_ignore_foreign_wal`、`seq_redo_skip_if_page_newer`、`seq_no_force_log` | 06-10、08 |
| P11-8 | §2（決定の一覧） | 章間の食い違いの決め方（D11-1）と C-1〜C-20 の結果を、各章の本文の読み替えとして「§2 の末尾」に注記する | D11-1 |
| P11-9 | §3（範囲）の後 | 新しい節「M4 の完了の判定」として §1.3 の 9 項目を追加（01 が定める場合は 01 を優先） | §1.3 |
| P11-10 | §20 | 00 から変えた点は `spec/design/m4-changes.md` に記録する運用。この章の §7.4 で「△」「×」とした提案（02-1、02-8〜10、04-1、04-3、05-P2、05-P4、05-P5、08-8、10-4 ほか）は、各章の本文も読み替えが要る点として記録する | 00 §20 |
