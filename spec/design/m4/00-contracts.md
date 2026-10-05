# yuzhu M4 契約（00-contracts）

M4 設計書（`spec/design/m4/`）の**共通の契約**です。M4 は 14 個のファイル（この文書と 01〜11 章、98 レビュー対応、99 確認事項、README の索引）に分けて並列に書かれ、並列に実装されます。章をまたぐ名前・型・トレイト・定数・モジュール構成・OID・SQLSTATE・ディスク形式は、**この文書に書いたものが唯一の正**です。各章はこの文書に従い、足りないものは追加してよいが、ここにある署名と名前は変えません（変えたい場合は各章末尾の「00 への変更提案」に理由とともに書き、統合時に反映する）。

**統合版であること（レビュー対応 R-02）**: 各章の実機調査で 00 の初版から変える必要が生じたものは、`11-tests-plan.md` §7 の C-1〜C-20 と §7.4 の採否表で決めた。**この文書は、それらを反映した統合版**に直してある（§1.4 に反映の一覧）。反映の対象は、`yuzhu-fuzz-sql` の廃止、`ExplainNode`（表示用の木）、`levels_up` の数え方、B+Tree の構造変更のブロック数、VACUUM / ANALYZE・OWNER TO、DISTINCT ON の `Unique`、`copy_in_response` の型、`Cast.implicit` と `CastMethod::Env`、`Settings::type_env` の署名、担当表の確定版、依存の向きの例外。**それでも本文が食い違うときの優先順位は 11 §2 の D11-1（実機 > 実装の持ち主の章 > ディスク形式の定義の持ち主 > 横断契約（00・02）> 00）に従う**。M4 の範囲と完了条件は `01-scope-decisions.md` が正。

- 要件定義: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- 前提（必読）: `spec/design/m1.md`、`m1-changes.md`、`m2.md`、`m2-changes.md`、`m3.md`（M2 と M3 は**実装が進行中**。この文書は M3 の設計どおりに完成した状態を前提にする）、`QUESTIONS.md`、`PROGRESS.md`
- 調査（根拠）: `spec/research/m4-btree.md`、`m4-query.md`、`research-pg-types.md`、`pg-compat-tools.md`、`m5-types-fk.md`（numeric / 日時 / char(n) の節）、`m3-tx-semantics.md` §7（シーケンス）
- 正解の基準は **PostgreSQL 17**。PostgreSQL のソースは REL_17_STABLE を `PG:<path>` と略す。「（未検証）」と付けた記述は、ソースや実機で確かめていない。

---

## 1. 読み方と書き方の規則

### 1.1 章の構成と持ち主

| ファイル | 内容 | 主な担当（§17） |
|---|---|---|
| `00-contracts.md` | この文書 | — |
| `01-scope-decisions.md` | M4 の範囲、調査間の食い違いと決定、完了条件、保証 | — |
| `02-pipeline-refactor.md` | 式・解析結果・論理プラン・物理プラン・executor 契約の作り直し（Q-013）と段階的な移行手順 | A、P0 |
| `03-parser-analyzer.md` | 問い合わせの構文（JOIN・サブクエリ・WITH・集合演算・集約）とアナライザ | S1、N1、N2、N3 |
| `04-planner-optimizer.md` | 論理プランの構築、ルールベース最適化、物理プラン、インデックス選択 | L1、L2 |
| `05-executor.md` | 実行ノード、集約、サブプラン、メモリ予算、DML とインデックス維持 | X1、X2、X3 |
| `06-btree.md` | B+Tree、比較関数と演算子クラス、一意性検査、一括構築、WAL、検査器、ヒープへの追加 | B1、B2、H4 |
| `07-catalog-ddl.md` | カタログの追加、インデックス・制約の DDL、TRUNCATE、`ddl/` モジュール | C1 |
| `08-sequence-serial.md` | シーケンス、SERIAL、IDENTITY | Q1 |
| `09-types-functions.md` | numeric・日時・char(n)・regclass などの統合、正規表現、集約関数の表、関数 | T1、T2、T3 |
| `10-explain-copy-compat.md` | EXPLAIN と deparse、COPY FROM STDIN、psql の `\dt`、pgbench、互換テスト | E1、O1、J、S |
| `11-tests-plan.md` | テスト全体、実装の分担と工数、未検証の点、整合性レビュー（C-1〜C-30）、M5 への宿題 | K、Z |
| `98-review-response.md` | レビュー対応（指摘の実在確認、反映先、一部反映の理由） | — |
| `99-questions.md` | 確認事項の集約（仮決めの一覧。M4-Q1〜Q158） | — |
| `README.md` | 索引（章の一覧、読む順、実装の分担と工数の要約） | — |

### 1.2 書き方の規則（全章共通）

1. 言語は日本語。絵文字は使わない。コードブロックには言語識別子を付ける。
2. 構成は M3 設計書（`m3.md`）に倣う。各章は次の節を、その章に当てはまるものだけ、この順で持つ: 「範囲」「決定（この章で追加で決めたこと）」「ディスク上の形式」「型とトレイト（契約の具体化）」「処理の流れ」「モジュールごとの仕様」「テスト」「実装の分担と工数」「未検証の点」「確認事項」「00 への変更提案」。
3. 契約は Rust の型・トレイトの**シグネチャで具体的に**書く。擬似コードで済ませない。
4. 曖昧な点は推奨案で仮決めし、「確認事項」に「仮決め・理由・変えたい場合の影響」の 3 点で書く。項目の ID は `[章番号-Q番号]`（例 `[06-Q3]`）。`11-tests-plan.md` が `M4-Q1` からの通し番号に振り直して集約する。ディスク形式に関わるものには ★ を付ける。
5. 調査レポート同士や調査とこの文書が食い違う点は、章の「決定」に明示して決める。
6. 他の章への参照は**章のファイル名と概念名**で書く（例「`06-btree.md` の一意性検査」）。節番号では参照しない（節番号は書き手が決めるため）。この文書の節は `00 §N` と書いてよい。
7. PostgreSQL の挙動は、実機（`sandbox/pg.sh start` で `127.0.0.1:55432` に PostgreSQL 17 が起動する）で確認できるものは確認し、エラー文言・SQLSTATE を正確に書く。確認できなかったものは「（未検証）」と書く。
8. この文書の署名を「変えたい」と思ったら、まず別の表現で実現できないかを考える。変えると他の章すべてに波及する。

### 1.3 用語

| 用語 | 意味 |
|---|---|
| Bound | アナライザの出力（`analyzer::Bound*`）。列は `Var` で参照する |
| 論理プラン | `planner::logical::LogicalPlan`。列は `ColId`（問い合わせ内で一意）で参照する |
| 物理プラン | `planner::physical::PhysicalPlan`。列は行内の位置（`PhysCol::Local`）か実行時パラメータ（`PhysCol::Param`）で参照する |
| RTE | range table entry。FROM 句の 1 項目（表・副問い合わせ・VALUES・関数・JOIN・CTE 参照） |
| SubLink | 式の中に現れる副問い合わせ（スカラー、EXISTS、ANY/IN、ALL） |
| SubPlan / InitPlan | 物理プランの副問い合わせ。SubPlan は外側の行ごとに（パラメータを変えて）実行する。InitPlan は外側に依存せず 1 回だけ実行する |
| 構造変更 | B+Tree の分割・新ルート・一括構築のように複数ページを同時に変える操作。`BTREE_PAGES`（全画像の 1 レコード）で記録する |
| ライターロック | M2 の単一ライターロック（書き込むトランザクションは同時に 1 つ） |
| 担当 | 並列実装の区切り（§17）。章の書き手ではなく、実装者の単位 |

### 1.4 統合版への反映の一覧（レビュー対応 R-02）

11 §7 の決定を 00 の本文に直接反映した箇所。**ここに載っていない C-n と、11 §7.4 で「△」「×」とした提案は、11 の記述が正**（実装者は 00 の該当箇所を読み替える）。

| 決定 | 00 の反映箇所 |
|---|---|
| C-1（`ExplainNode` は表示用の木、`exec_id`・`width`） | §4 の `explain_tree.rs`、§9.3 |
| C-3（`levels_up` はスコープ単位） | §6.1 の `Var.levels_up` |
| C-5（`yuzhu-fuzz-sql` を作らない） | §4 の構成図・§4.2・§17 の Z・§18 |
| C-6（`free_params`。`uses_params()` は残す） | §4.3 の 5 |
| C-13（`3h + 1`） | §13.4 |
| C-14（`Cast.implicit`、`CastMethod::Env`） | §6.2 の `Cast` |
| C-16（`ANALYZE` はブロック内で成功、`OWNER TO` はロールを検証） | §2 の D-11・D-12 |
| C-18（`Settings::type_env` の署名） | §14.2 |
| C-23（DISTINCT ON の `Unique`） | §9.2 の `Unique` |
| C-24（結合の出力の並びと INL の入れ替え） | §9.2 の `NestedLoopJoin` のコメント |
| C-27（`check_interrupts` は入力 1 行ごと） | §4.3 の 4 |
| C-29（`copy_in_response` の列形式は `i16`） | §14.5 |
| C-30（依存の向きの例外 2 か所） | §4.1 |
| 11 §4.1（担当表の確定、追加ファイルの持ち主） | §4 の新しいファイル、§17、§18 |
| 11 の D11-2・D11-5・D11-8（slt のディレクトリ、restart の mode、ワークロード 8・I16） | §18 |
| 01（範囲・決定・完了条件・保証） | §2、§3、§17 の最後の注 |

**未反映（11 を正とする）**: C-2（計測の仕組み。05 の旧方式は 05 から削除済みで、00 の `Executor` に `set_counters` と `ExecCtx.instr` を足す 10 の提案が正）、C-4（`min` / `max` の等しいときの代表）、C-7〜C-10、C-12、C-15、C-17、C-19〜C-22、C-25、C-26、C-28、署名の細部（`HashKey`・`AggKind` ほか）。


---

## 2. 決定の一覧

各章の「決定」で根拠（選択肢と出典）を詳しく書く。ここは**章をまたいで効く決定の結論**だけを並べる。`D-n` は M4 の決定番号（`01-scope-decisions.md` が選択肢と理由を表にする）。

| # | 決定 | 関連する QUESTIONS |
|---|---|---|
| D-1 | 処理系列は AST → Bound（`Var`）→ 論理プラン（`ColId`）→ ルール → 物理プラン（`PhysCol`）→ Executor。式の木は `Expr<C, Q>` の**単一の定義**から 3 つの型別名（`BoundExpr`、`LExpr`、`PhysExpr`）を作る | Q-013 |
| D-2 | 副問い合わせは式の中に `SubLink { kind, test, query }` として持つ。ANY/ALL の比較は `test` に `SubLinkOutput(i)` を含む式として持ち、`IN` は `ANY`、`NOT IN` は `NOT ANY` | — |
| D-3 | numeric は既存の `yuzhu-numeric` クレートを `yuzhu-core` に統合する（`Datum::Numeric`）。新規実装はしない。ディスク形式は §12.3 | Q-011 |
| D-4 | 日時は既存の `yuzhu-datetime` クレートを統合する。M4 で入れる型は `date`、`timestamp`、`timestamptz`。`interval`、`time`、`timetz` は M5（キーワードは `0A000`） | Q-012 |
| D-5 | `char(n)`（bpchar、OID 1042）は `Datum::BpChar(String)` という別の変種で持つ。比較・ハッシュは末尾の空白を無視する | Q-012 |
| D-6 | COPY FROM STDIN（テキスト形式、Simple Query）を入れる。COPY TO、CSV、バイナリは M5 | Q-012 |
| D-7 | B+Tree は PostgreSQL の nbtree に準拠（8KB、メタページ、high key、right-link、TID をタイブレークに使う）。suffix truncation・重複排除・項目の削除・ページ削除は M4 で作らない。**ルート（葉）は CREATE INDEX の時点で作る**。構造変更は全画像の 1 レコード `BTREE_PAGES`、通常の挿入は差分の `BTREE_INSERT_LEAF` | — |
| D-8 | PRIMARY KEY / UNIQUE は「一意インデックス + `pg_constraint`」。検査は行ごとに即時。一意性検査の API は他トランザクションの待ち（`WaitFor`）を返せる形にするが、M4 では起きない | — |
| D-9 | シーケンスは relkind `S` の 1 ページのリレーション。MVCC を使わずその場で上書き。WAL は `SEQ` rmgr、`SEQ_LOG_VALS = 32`。コミット時の WAL flush は `Transaction.wal_flush_upto` で行う | — |
| D-10 | SERIAL / IDENTITY は解析段階で「シーケンス + DEFAULT + NOT NULL + 所有関係」に書き換える | — |
| D-11 | `TRUNCATE` は新しい relfilenode を作る方式（PostgreSQL と同じ。ロールバック可能）。`VACUUM` / `ANALYZE` は何もせず成功。**トランザクションブロック内で `25001` になるのは `VACUUM` だけ。`ANALYZE` は成功する**（実機。11 §7.1 の C-16、07-D） | — |
| D-12 | `ALTER TABLE` は `ADD [CONSTRAINT n] PRIMARY KEY / UNIQUE` と `OWNER TO`（**ロールを検証して `relowner` を更新する**。C-16）だけ。それ以外は `0A000` | — |
| D-13 | 結合は HashJoin（INNER / LEFT / RIGHT / FULL / SEMI / ANTI）、NestedLoopJoin（内側を Materialize）、NestedLoopParam（内側のインデックス検索）。マージ結合は作らない。RIGHT は論理プラン構築で LEFT に直す | — |
| D-14 | 集約は HashAggregate、Aggregate（GROUP BY なし）、GroupAggregate（`enable_hashagg = off` のとき、Sort + 逐次集約）。集約関数は `BuiltinAggregate` の静的な表で引く | Q-011 |
| D-15 | サブクエリは PostgreSQL と同じ範囲の書き換え（WHERE 最上位の AND にある `EXISTS` / `IN` / `NOT EXISTS` を SEMI / ANTI 結合に）と、残りは SubPlan / InitPlan / ハッシュ化 SubPlan | — |
| D-16 | CTE のインライン展開は **PostgreSQL と同じ規則**（非再帰・副作用なし・参照 1 回・`MATERIALIZED` なし）。それ以外は CteScan。`m4-query.md` の M4Q-4（全部インライン）は採用しない | — |
| D-17 | 最適化は決まった順に 1 回ずつ適用するルール列。統計は持たず、サイズの手がかりは計画時点の `nblocks` だけ | — |
| D-18 | インデックス選択はヒューリスティクス（一意で全列等値 > 等値の列数 > 先頭列の範囲）。`IN (...)`・`OR`・Index Only Scan・Bitmap Scan は M4 では行わない。**インデックス順によるソートの省略（前向き・後ろ向き）は M4 後半の任意項目（S）**。B+Tree の後ろ向きスキャンはそのためにあり、プランナが使わない間も単体テストで検証する | m4-query §2.1 の「M5」を前倒し |
| D-19 | スピルしない。クエリ単位のメモリ予算 `yuzhu.query_mem_limit`（既定 256MB）を超えたら `53200`。CREATE INDEX のソートは予算の対象外 | — |
| D-20 | EXPLAIN はテキスト形式のみ。`COSTS OFF` の出力は PostgreSQL と同じ書式（プランの選び方は一致しない）。コスト欄は `cost=0.00..0.00 rows=0 width=N` | — |
| D-21 | 式の逆変換（deparse）を `deparse/` に置き、EXPLAIN と `pg_get_expr` が共有する。`pg_get_expr` は保存テキストを parse → analyze → deparse して返す（保存形式は変えない。M2-Q9 の移行をこの方式で行う） | M2-Q9 |
| D-22 | 正規表現（`~`、`~*`、`!~`、`!~*`）は手書きのエンジン（`types/regex.rs`）。`regex` クレートは使わない | M2-Q8 |
| D-23 | カタログの追加は §11.5 の表。カタログ自体のインデックスは作らない | — |
| D-24 | psql 17 の `\dt`、`\dn`、`\di`、`\ds`、`\l` と `\d シーケンス` が動くことを M4 の完了条件にする（`\ds` と `\d シーケンス` は 10 §6.1 に採取した SQL。R-25）。`\dt+`、`\df`、`\du` は任意。**`\d tbl` は M4 では動かない任意項目**（10 §6.3。R-18・R-19） | M2-Q8 |
| D-25 | `pgbench -i` と、組み込みスクリプト（tpcb-like）の `-M simple` での完走（`-c 4 -T 30`）を M4 の完了条件にする | Q-012 |
| D-26 | `UPDATE ... FROM` / `DELETE ... USING` を入れる。`RETURNING`（INSERT / UPDATE / DELETE）は M4 の**後半・任意**（Bound と物理プランには欄を用意し、未実装の間は `0A000`） | — |
| D-27 | `Error` に `schema` / `table` / `column` / `constraint` / `context` を足し、ErrorResponse の `s` `t` `c` `n` `W` フィールドで送る | — |
| D-28 | `CATALOG_VERSION_NO` を上げる。M3 のデータディレクトリは使えない（initdb のやり直し。M2-Q20 の方針どおり） | M2-Q20 |
| D-29 | DDL の実行は `session.rs` から `ddl/` モジュールに移す | — |
| D-30 | `WITH (fillfactor = N)` は検証して捨てる（`reloptions` は保存しない）。`COPY ... WITH (FREEZE)` は受け付けて無視する | — |

**QUESTIONS.md の Q-010〜Q-014 の扱い**: Q-010（64 ビット XID）は M2 で実現済みで M4 に作業なし。Q-011（numeric）は D-3、Q-012（COPY・char(n)・timestamp）は D-4〜D-6 と D-25、Q-013（作り直し）は D-1・D-2。Q-014（MultiXact の簡易版）は M5 で、M4 は D-8 の `WaitFor` の口だけ用意する。

---

## 3. 範囲の要約

詳細は `01-scope-decisions.md`。契約が前提にする範囲だけをここに置く。

**M4 で入れる**:

| 分類 | 内容 |
|---|---|
| インデックス | `CREATE [UNIQUE] INDEX [CONCURRENTLY（通常と同じ扱い）] [IF NOT EXISTS] [name] ON t [USING btree] (col [opclass] [ASC\|DESC] [NULLS FIRST\|LAST], ...)`、`DROP INDEX [IF EXISTS] name [, ...]`、B+Tree のスキャン（前向き・後ろ向き、等値・範囲） |
| 制約 | `PRIMARY KEY`、`UNIQUE`（列制約・表制約・`ALTER TABLE ... ADD`）、`23505` のメッセージとフィールドを PostgreSQL と一致 |
| シーケンス | `CREATE / ALTER / DROP SEQUENCE`、`nextval` `currval` `lastval` `setval`、`serial` `smallserial` `bigserial`、`GENERATED { ALWAYS \| BY DEFAULT } AS IDENTITY`、`OVERRIDING { SYSTEM \| USER } VALUE` |
| 問い合わせ | JOIN 全種（ON / USING / NATURAL / カンマ）、FROM の副問い合わせ（別名・列別名）、集約（`count` `sum` `avg` `min` `max` `bool_and` `bool_or` `every`、`DISTINCT`、`FILTER`）、`GROUP BY` / `HAVING`（主キーへの関数従属を含む）、`DISTINCT ON`、サブクエリ（スカラー、`IN` / `NOT IN` / `EXISTS` / `ANY` / `ALL`、相関あり）、`UNION` / `INTERSECT` / `EXCEPT`（`ALL` を含む）、`WITH`（非再帰、`[NOT] MATERIALIZED`）、`generate_series(int4\|int8, ...)`（FROM 句） |
| DML | `UPDATE ... FROM`、`DELETE ... USING`、`TRUNCATE t [, ...]`、`COPY t [(cols)] FROM STDIN [WITH (...)]`、`RETURNING`（任意） |
| 型 | `numeric(p,s)`、`char(n)` / `bpchar`、`date`、`timestamp[(p)]`、`timestamptz[(p)]`、`regclass`、`regtype`、`int2vector`、1 次元の `int2[]`（カタログ用の最小限） |
| 関数 | `now()` `current_timestamp` `current_date` `localtimestamp` `transaction_timestamp()` `statement_timestamp()` `clock_timestamp()`、numeric の `round` `trunc` `ceil` `ceiling` `floor` `abs` `sign`、`pg_typeof`、`pg_get_expr`（正規形）、正規表現演算子 |
| EXPLAIN | `EXPLAIN [ANALYZE] [VERBOSE] [COSTS] stmt`、`EXPLAIN (option [value], ...) stmt` |
| 設定 | `enable_*`、`work_mem` 系、`yuzhu.query_mem_limit`、PostgreSQL にあって意味を持たない GUC を受け付けて保存するだけの一覧 |
| ツール | psql の `\dt` `\dn` `\di` `\ds` `\l` と `\d シーケンス`、pgbench の `-i` と tpcb-like（`-M simple`） |

**M4 では対応しない（実行すると `0A000`）**: `WITH RECURSIVE`、`LATERAL`、ウィンドウ関数、`GROUPING SETS` / `ROLLUP` / `CUBE`、`INSERT ... ON CONFLICT`、`MERGE`、`CREATE VIEW`、`ALTER TABLE` の上記以外、`DEFERRABLE` 制約、式インデックス・部分インデックス・`INCLUDE`・`NULLS NOT DISTINCT`、`interval` / `time` / `timetz`、配列型（int2vector と int2[] を除く）、`FOREIGN KEY`（M5）、`COPY TO` / CSV / バイナリ（M5）、`SELECT` 句の集合返却関数、`FOR UPDATE` / `FOR SHARE`、`TABLESAMPLE`、集約内の `ORDER BY`、`string_agg` / `array_agg`（M5）。

---

## 4. クレートとモジュールの構成

M3 の構成（`m3.md` 第 2 節）からの変更を示す。`★` は新規、`△` は変更、`✕` は削除。**M3 の時点でまだ存在しないファイル（`wal/*` など）は M3 の設計のまま存在するものとして扱う。**

```
impl/rust/
├── Cargo.toml                           （変更しない。差分ランダムテストは tests/tools/difftest。D11-6、C-5）
└── crates/
    ├── yuzhu-numeric/                   既存（変更しない。不足があれば章 09 が追加を提案する）
    ├── yuzhu-datetime/                  既存（同上）
    ├── （yuzhu-fuzz-sql は作らない。差分ランダムテストは tests/tools/difftest に既にある独立の Cargo プロジェクトを仕上げて使う。11 D11-6、C-5）
    ├── yuzhu-core/
    │   ├── Cargo.toml                   △ yuzhu-numeric、yuzhu-datetime をパス依存に追加
    │   └── src/
    │       ├── lib.rs                   △ モジュールの追加
    │       ├── error.rs                 △ §14.4（フィールド）、§15.3（SQLSTATE）
    │       ├── expr/                    ★ 式の木（§6）
    │       │   ├── mod.rs               Expr<C,Q>、ExprKind、ID の型、AggCall、SubLinkKind
    │       │   └── walk.rs              走査と書き換えの補助（walk、try_map）、lower_single_rel
    │       ├── deparse/                 ★ 式 → SQL テキスト（EXPLAIN と pg_get_expr が共有。章 10）。stored.rs の pg_get_expr は sql::parse_expr → analyzer → deparse を呼ぶ（§4.1 の例外 2）
    │       ├── types/
    │       │   ├── mod.rs               △ OID 定数（§12.1）、TypeEnv
    │       │   ├── datum.rs             △ Datum の変種（§12.2）、cmp_datum の拡張
    │       │   ├── io.rs                △ 新しい型の入出力、TypeEnv を受け取る版
    │       │   ├── ops.rs               △ 新しい型の演算
    │       │   ├── sys.rs               △ regclass、regtype、int2vector、int2[]
    │       │   ├── numeric.rs           ★ yuzhu-numeric の薄い橋渡し（演算・キャスト・ディスク形式）
    │       │   ├── datetime.rs          ★ yuzhu-datetime の薄い橋渡し
    │       │   ├── bpchar.rs            ★ char(n) の入出力・サイズ変換・比較
    │       │   ├── regex.rs             ★ 手書きの正規表現エンジン
    │       │   ├── hash.rs              ★ hash_datum、HashKey（ハッシュ結合・集約・DISTINCT・集合演算が共有）
    │       │   └── cmp.rs               ★ cmp_with_nulls（NULL の順序つきの比較。Sort・B+Tree・Unique が共有）
    │       ├── sql/                     △ 構文の追加（章 03、07、08、09、10 がそれぞれ受け持つ）
    │       ├── catalog/
    │       │   ├── mod.rs               △ TableDef・IndexDef・RelKind・ColumnDef の拡張、CatalogReader の追加（§11）
    │       │   ├── builtin.rs           △ 新しい型・演算子・関数・キャスト、BuiltinAggregate
    │       │   ├── opclass.rs           ★ opfamily / opclass / amop / amproc の静的な表（章 06）
    │       │   ├── schema.rs            △ カタログの追加（§11.5）
    │       │   ├── rows.rs              △ 初期行の追加
    │       │   ├── store.rs             △ インデックス・制約・シーケンス・依存関係の書き込み
    │       │   ├── seq_params.rs / naming.rs / depend.rs / check.rs / names.rs   ★ シーケンスのパラメータ（Q1）、自動命名（C1。make_object_name もここ）、依存関係（C1）、制約の検査（C1）、CatalogNames（T3）。11 §4.2
    │       │   ├── cache.rs             △ TableDef に indexes を含める
    │       │   └── reader.rs            △ CatalogReader の追加メソッド
    │       ├── analyzer/
    │       │   ├── mod.rs               △
    │       │   ├── bound.rs             △ BoundQuery、Rte、FromItem など（§7）
    │       │   ├── scope.rs             △ 複数の RTE、親スコープ
    │       │   ├── from.rs              ★ FROM 句・JOIN
    │       │   ├── agg.rs               ★ 集約・GROUP BY・HAVING・関数従属
    │       │   ├── sublink.rs           ★ サブクエリ式
    │       │   ├── setop.rs             ★ 集合演算
    │       │   ├── cte.rs               ★ WITH
    │       │   ├── select.rs / dml.rs / ddl.rs / expr.rs / resolve.rs / coerce.rs   △
    │       ├── planner/
    │       │   ├── mod.rs               △ plan() の入口（§9.1）
    │       │   ├── logical.rs           ★ LogicalPlan、ColumnArena（§8）
    │       │   ├── build.rs             ★ Bound → 論理プラン
    │       │   ├── rules/               ★ mod.rs、const_fold.rs、sublink.rs、subquery_pullup.rs、outer_join.rs、pushdown.rs、join_keys.rs、join_order.rs、prune.rs
    │       │   ├── physicalize.rs       ★ 論理プラン → 物理プラン
    │       │   ├── index_select.rs      ★ インデックス選択
    │       │   ├── physical.rs          △ 旧 plan.rs。PhysicalPlan、PhysicalQuery（§9）
    │       │   ├── explain_tree.rs      ★ 表示用の ExplainNode の構築（物理プランと同形ではない。章 10 と共同。C-1）
    │       │   └── util.rs / size.rs / print.rs / validate.rs   ★ 補助（04 §3.2、§3.6）。rules/testutil.rs は #[cfg(test)]
    │       ├── executor/
    │       │   ├── mod.rs               △ Executor に rewind、ExecCtx の拡張（§10）
    │       │   ├── build.rs / eval.rs   △ PhysExpr を評価する
    │       │   ├── mem.rs               ★ MemBudget
    │       │   ├── agg.rs               ★ AggState（AggKind ごと）
    │       │   ├── subplan.rs           ★ SubPlan / InitPlan の状態と評価
    │       │   ├── dml.rs               ★ insert_with_indexes、update_with_indexes（INSERT・UPDATE・COPY が共有）
    │       │   ├── instrument.rs        ★ EXPLAIN ANALYZE の計測
    │       │   ├── seq.rs               ★ nextval / currval / setval / lastval の実行（Q1。章 08）
    │       │   └── nodes/               △ 追加: nested_loop.rs、hash_join.rs、materialize.rs、aggregate.rs、hash_aggregate.rs、group_aggregate.rs、unique.rs、append.rs、hash_setop.rs、cte_scan.rs、function_scan.rs、index_scan.rs
    │       ├── storage/
    │       │   ├── mod.rs               △ TableStore の追加、IndexStore（§13）、RelHandle に indexes
    │       │   ├── page.rs              △ 追加のアクセサ（special 付きの初期化、途中への項目の挿入など。章 06）
    │       │   ├── heap/ / heap_store.rs △ fetch_dirty、生のスキャン、列の符号化の公開（H4。章 06）
    │       │   ├── btree/               ★ mod.rs、page.rs、tuple.rs、meta.rs、search.rs、insert.rs、split.rs、build.rs、scan.rs、unique.rs、check.rs、wal.rs
    │       │   ├── index_store.rs       ★ BtreeStore: IndexStore の実装
    │       │   └── sequence.rs          ★ SequenceStore の実装と SEQ rmgr の REDO（章 08）
    │       ├── wal/                     △ RmgrId::Btree = 4、Seq = 5。dump に両 rmgr
    │       ├── txn/                     △ Transaction に wal_flush_upto、started_at
    │       ├── ddl/                     ★ CREATE / DROP / ALTER / TRUNCATE などの実行（章 07、08）
    │       │   ├── mod.rs               DdlCtx、execute()
    │       │   ├── table.rs / index.rs / constraint.rs / sequence.rs / truncate.rs / depend.rs
    │       ├── copy/                    ★ COPY FROM STDIN（章 10）: mod.rs、text.rs（行の分割と値の復元）、exec.rs
    │       ├── explain/                 ★ EXPLAIN の整形（章 10）: mod.rs、format.rs、node.rs
    │       ├── settings.rs              △ §15.4 の項目
    │       ├── session.rs               △ 新しい文の振り分け、COPY の状態、RuntimeInfo の実装
    │       ├── bootstrap.rs / engine.rs △ 新しいカタログの initdb、StorageStack に IndexStore と SequenceStore
    │       └── testing.rs               △
    └── yuzhu-server/src/
        ├── protocol/                    △ CopyInResponse（G）、CopyData（d）、CopyDone（c）、CopyFail（f）、エラーの追加フィールド
        └── connection.rs                △ COPY の状態
```

### 4.1 依存の方向

M3 の図（`m3.md` 第 2 節）に次を差し込む。下位は上位を `use` しない。

```
error, types::{mod, datum, cmp, hash, regex, numeric, datetime, bpchar, io, ops, sys}, util, interrupt
  ← sql
  ← catalog::{mod, builtin, opclass, schema}
  ← expr                                   （BuiltinOperator などの型を使う）
  ← deparse                                （expr と catalog の名前を使う）
  ← storage::vfs ← control, datadir
  ← storage::{smgr, page, checksum} ← storage::buffer
  ← wal                                    （M3）
  ← storage::heap ← storage::btree ← storage::{index_store, sequence} ← storage::{heap_store, stack}
  ← txn
  ← catalog::{rows, store, cache, reader}
  ← analyzer ← planner::{logical, build, rules, physicalize, index_select, physical} ← executor
  ← ddl, copy, explain
  ← checkpoint, recovery, bootstrap, engine, session
```

- `planner::physical` は `executor` が使う。`executor` は `planner::logical` を知らない。
- **この図の向きの例外（2 か所だけ。レビュー対応 R-30）**。(1) **`planner::rules::const_fold` が `executor::eval::eval_const` を 1 か所だけ呼ぶ**（`EvalCtx::for_constant_folding` を使う。11 §7.4 の 04-3。`executor` は `planner::physical` を使うので `planner` ⇄ `executor` が相互参照になるが、使う型は `physical`（下位）と `eval`（式の評価）に限り、`executor::build` / `nodes` は `planner` の他の部分を `use` しない。解消は M5 以降に `expr/eval.rs` へ純粋な評価器を移す）。(2) **`deparse::stored`（`pg_get_expr` / `pg_get_constraintdef` / `pg_get_indexdef` の本体）が `sql::parse_expr` → `analyzer::analyze_*` → `deparse` を呼ぶ**（10 §4.8）。`deparse` の他の部分（式の逆変換本体）は `expr` と `catalog` だけに依存し、`stored.rs` だけが `analyzer` に依存する。このため `deparse::stored` は図の `analyzer` より後ろ（`ddl` / `explain` と同じ段）に置く。これ以外の逆向きの `use` は禁止（`cargo` の循環依存はクレート内なので検出されない。CI の `grep` ベースの検査を K4 が足す）。
- `expr` は `analyzer`・`planner`・`executor` のどれにも依存しない。`BoundExpr` / `LExpr` / `PhysExpr` の**型別名は定義する側のモジュール**（`analyzer::bound`、`planner::logical`、`planner::physical`）に置く。
- `storage::btree` は `catalog::opclass`（比較関数の静的な表）と `types::cmp` を使う。カタログの行（`catalog::store`）には依存しない。
- `recovery.rs` の `dispatch` に `RmgrId::Btree` → `storage::btree::wal::redo`、`RmgrId::Seq` → `storage::sequence::redo` を足す（持ち主は M3 の R。M4 の追加は B1 と Q1 が依頼する）。

### 4.2 外部依存

- `yuzhu-core` の外部クレート依存は**ゼロのまま**。内部のパス依存として `yuzhu-numeric` と `yuzhu-datetime` を足す（どちらも外部依存なし）。
- `yuzhu-server` は新しい依存を足さない。差分ランダムテスト（`tests/tools/difftest`）は独立の Cargo プロジェクトで、`postgres` クレートを使う（ワークスペースの外）。
- 正規表現のためのクレートは追加しない（D-22）。
- 日時のタイムゾーンは `yuzhu-datetime` が `/usr/share/zoneinfo`（TZif）を読む。tzdata がない環境では UTC と固定オフセットだけが使える。CI とコンテナ（`sandbox/`）に tzdata を入れること（章 09 と 11 が確認する）。

### 4.3 コーディング規約（M4 で追加）

M2・M3 の規約（`m2.md` §2、`m3.md` §2）を引き継ぎ、次を足す。

1. **式の木を型ごとに複製しない**。`Expr<C, Q>` に集約する。新しい式の種類は `expr/mod.rs` の `ExprKind` に 1 回だけ足し、3 つの層の取り扱い（Bound に現れてよいか、論理に、物理に）を 6.4 の表に書く。
2. **値の比較とハッシュは `cmp_datum` / `types::hash::hash_datum` に集約する**（`Datum` の変種が型を表すので、bpchar・numeric・日時も型を渡さずに正しく動く。§12.2）。比較・ハッシュ・等値判定に使う 2 値は**同じ型**にそろえてから渡す（整数の幅違いは可）。型をそろえるのは演算子解決とプランナ（結合キー・集合演算・ANY の `test`）の責任。NULL の順序は `types::cmp::cmp_with_nulls` を使い、各所で書き直さない。
3. **B+Tree と sequence のページ変更は M3 規約 1（WAL を書くページ変更の形）に従う**。特に構造変更は、必要な新ページをすべて確保してから、全ページの新しい内容をメモリ上で組み立て、`CriticalSection` の中で一度にバッファへ書き `BTREE_PAGES` を 1 本挿入する。途中で失敗しうる処理を `page_mut()` の後に書かない。
4. **executor のノードは行ごとに `ctx.check_interrupts()?` を呼ぶ**（ループの長い処理 — ハッシュ表の構築、ソート、集約 — を含む）。**入力を 1 行読むたびに 1 回**、葉に限らずループを持つ全ノードが呼ぶ（02 §3.7.4 の規則 2 もこれに合わせた。レビュー対応 R-11）。
5. **`rewind` では、パラメータに依存しない子の結果（ソート・ハッシュ表・Materialize）を溜め直さず読み直す**。依存するかどうかは、executor 内部の `free_params(plan, query)`（05 §3.2。`SubPlanDef` と `NestedLoopParam` の束縛を越えて自由な `ParamId` を求める）で、`executor::build` が各ノードを作る時点で決める（C-6）。`PhysicalPlan::uses_params()`（木のどこかに `PhysCol::Param` があるか。`SubLink` を含めば true）は残し、`build(plan)`（単体テスト用）と他の章が使う。
6. **ID のオーバーフロー**: `ColId` は u32、`RteId` は u16（1 つの SELECT の範囲表は 65535 まで）、`ParamId` / `SubPlanId` は u16。超えたら `54000`。
7. **panic しない**: `unwrap` / `expect` は「到達しない」ことを型や直前の検査で保証できる箇所に限り、メッセージに理由を書く。到達しうるものは `Error::internal`。

---

## 5. 処理の流れ（全体像）

```
Session::execute_simple(sql)
  parse → Statement
  ├─ トランザクション制御・SET・SHOW・CHECKPOINT・COPY の継続 … session が直接処理
  └─ 解析可能な文: （ストレージバリア共有 + スナップショット。M2 §5.2）
       analyzer::analyze(stmt, &catalog)      → BoundStatement            （Var 参照）
       ├─ BoundStatement::Ddl(..)             → ddl::execute(&mut DdlCtx, ..) → コマンドタグ
       ├─ BoundStatement::Copy(..)            → copy::begin(..)            → CopyIn 状態へ
       └─ それ以外（Select / Insert / Update / Delete / Explain）:
            planner::plan(&bound, &PlanEnv) → PhysicalQuery
              ├─ planner::build            Bound → LogicalQuery            （ColId 参照）
              ├─ planner::rules::run       固定順のルール
              └─ planner::physicalize      LogicalQuery → PhysicalQuery    （PhysCol 参照、インデックス選択）
            executor::build(&query.root) → BoxedExecutor
            next() ループ（ExecCtx に params / mem / subplans を持つ）
```

- 副問い合わせ（SubLink）は、Bound では `Box<BoundQuery>`、論理プランでは `Box<LogicalSubquery>`、物理プランでは `SubPlanId`（`PhysicalQuery.subplans` の添字）になる。
- CHECK・DEFAULT・COPY の列変換のように「1 つの表の行に対する式」は、`rte = 0` の `Var` で解析し、`expr::lower_single_rel` で `PhysExpr`（`PhysCol::Local(col)`）に直して評価する。

---

## 6. 式の木（`expr`）

### 6.1 ID の型

```rust
// expr/mod.rs
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)] pub struct RteId(pub u16);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)] pub struct ColId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)] pub struct ParamId(pub u16);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)] pub struct SubPlanId(pub u16);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)] pub struct CteId(pub u16);

/// システム列を指す Var.col の下限。Var.col >= SYSTEM_COL_BASE なら SystemColumn（col - SYSTEM_COL_BASE の添字）
pub const SYSTEM_COL_BASE: u16 = 0x8000;

/// Bound 層の列参照（PostgreSQL の Var）
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Var {
    pub rte: RteId,
    /// Rte.columns の 0 始まりの位置。システム列は SYSTEM_COL_BASE + SystemColumn の添字（Ctid=0, Xmin=1, Cmin=2, Xmax=3, Cmax=4, TableOid=5）
    pub col: u16,
    /// 0 = 同じスコープ。1 以上 = 外側のスコープ（相関参照）。**数えるのは「rtable を持つスコープ」の入れ子**:
    /// `BoundSelect`・DML・`BoundSetExpr::Values` の各行（rtable が空の 1 スコープ）。`BoundQuery` は数えない
    /// （集合演算の腕・CTE 本体は兄弟）。03 の D3-20 が正（C-3、レビュー対応 R-04）
    pub levels_up: u16,
}
impl Var {
    pub fn user(rte: RteId, col: u16) -> Var;
    pub fn system(rte: RteId, sc: SystemColumn) -> Var;
    pub fn system_column(&self) -> Option<SystemColumn>;
}

/// 物理層の列参照
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PhysCol {
    /// 評価中の行の位置
    Local(usize),
    /// 実行時パラメータ（相関サブクエリ・NestedLoopParam が外側の値を渡す）
    Param(ParamId),
}
```

### 6.2 式の木

```rust
#[derive(Clone, Debug)]
pub struct Expr<C, Q> { pub kind: ExprKind<C, Q>, pub ty: SqlType, pub span: Span }

#[derive(Clone, Debug)]
pub enum ExprKind<C, Q> {
    // ---- M1〜M3 から引き継ぐ変種（意味は m1.md、m1-changes.md のまま）----
    Literal(Datum),
    /// 列参照。C = Var（Bound）/ ColId（論理）/ PhysCol（物理）
    Column(C),
    Operator { op: &'static BuiltinOperator, args: Vec<Expr<C, Q>> },
    Function { func: &'static BuiltinFunction, args: Vec<Expr<C, Q>> },
    /// implicit: アナライザが暗黙のキャストを挿入したとき true（10-P3。deparse が根の暗黙のキャストを隠す）。same_as は無視して比べる。
    /// CastMethod には Env(fn(&[Datum], &TypeEnv) -> Result<Datum>) を足す（09-P3。定数畳み込みしない）。C-14
    Cast { expr: Box<Expr<C, Q>>, method: CastMethod, implicit: bool },
    CoerceTypmod { expr: Box<Expr<C, Q>>, explicit: bool },
    And(Vec<Expr<C, Q>>),
    Or(Vec<Expr<C, Q>>),
    Not(Box<Expr<C, Q>>),
    IsNull(Box<Expr<C, Q>>),
    IsNotNull(Box<Expr<C, Q>>),
    BoolTest { expr: Box<Expr<C, Q>>, test: BoolTestKind },
    Case { arms: Vec<(Expr<C, Q>, Expr<C, Q>)>, else_result: Option<Box<Expr<C, Q>>> },
    Coalesce(Vec<Expr<C, Q>>),
    NullIf { left: Box<Expr<C, Q>>, right: Box<Expr<C, Q>>, eq_op: &'static BuiltinOperator },
    Like { expr: Box<Expr<C, Q>>, pattern: Box<Expr<C, Q>>, escape: Option<Box<Expr<C, Q>>>, negated: bool, case_insensitive: bool },
    InList { expr: Box<Expr<C, Q>>, list: Vec<Expr<C, Q>>, eq_op: &'static BuiltinOperator, negated: bool },
    SessionValue(SessionValueKind),
    // ---- M4 の新しい変種 ----
    /// 集約の呼び出し。Bound にだけ現れる（論理プランへの変換で Aggregate ノードの出力列 Column(ColId) に置き換わる）
    Aggregate(Box<AggCall<C, Q>>),
    /// 副問い合わせ式。query の型は層ごとに違う（§6.4）
    SubLink { kind: SubLinkKind, test: Option<Box<Expr<C, Q>>>, query: Q },
    /// SubLink の test の中でだけ使う: 副問い合わせの現在の行の i 番目の出力列
    SubLinkOutput(u16),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubLinkKind {
    /// (SELECT ...) 1 行 1 列。0 行は NULL、2 行以上は 21000
    Scalar,
    /// EXISTS (SELECT ...)
    Exists,
    /// 「副問い合わせのある行について test が true」の三値論理 OR。`x IN (SELECT ...)` は Any（test = `x = SubLinkOutput(0)`）、`x NOT IN (...)` は Not(Any)
    Any,
    /// 「すべての行について test が true」の三値論理 AND。`x <> ALL (SELECT ...)` など
    All,
}

#[derive(Clone, Debug)]
pub struct AggCall<C, Q> {
    pub func: &'static BuiltinAggregate,
    /// count(*) は空
    pub args: Vec<Expr<C, Q>>,
    pub distinct: bool,
    pub filter: Option<Expr<C, Q>>,
}
```

- `Expr` に `Span` を持たせるのは M1 から変えない（エラー位置）。論理・物理の層では `Span::default()` でよい。
- `SubLink.test` は Any / All では必須（Scalar / Exists では None）。例: `a IN (SELECT x FROM u)` は `test = Operator(=, [Column(a), SubLinkOutput(0)])`。右辺に型変換が要るなら `Cast(SubLinkOutput(0))` のように test の中に含める。行値の `(a, b) IN (SELECT x, y ...)` は `And(a = Out0, b = Out1)`。
- `Expr::new(kind, ty, span)` のコンストラクタと、`ExprKind` ごとのヘルパ（`Expr::literal`、`Expr::and` など）は `expr/mod.rs` の持ち主が足してよい。

### 6.3 層ごとの型別名

```rust
// analyzer/bound.rs
pub type BoundExpr = Expr<Var, Box<BoundQuery>>;
pub type BoundExprKind = ExprKind<Var, Box<BoundQuery>>;
pub type BoundAggCall = AggCall<Var, Box<BoundQuery>>;

// planner/logical.rs
pub type LExpr = Expr<ColId, Box<LogicalSubquery>>;

// planner/physical.rs
pub type PhysExpr = Expr<PhysCol, SubPlanId>;
```

### 6.4 層ごとに現れてよい変種

| 変種 | Bound | 論理 | 物理 |
|---|---|---|---|
| `Column` | `Var`（`levels_up` > 0 は相関参照） | `ColId`（その LogicalPlan の子の出力にない ColId は外側の列＝相関参照） | `PhysCol::Local` / `PhysCol::Param` |
| `Aggregate` | 可（targets・having・order by の中） | 不可（`build` の途中だけ。最終形にはない） | 不可 |
| `SubLink` | `query: Box<BoundQuery>` | `query: Box<LogicalSubquery>` | `query: SubPlanId` |
| `SubLinkOutput` | `test` の中だけ | `test` の中だけ | `SubPlanDef.test` の中だけ |
| それ以外 | 可 | 可 | 可 |

### 6.5 走査と書き換えの補助（`expr/walk.rs`）

```rust
impl<C: Clone, Q: Clone> Expr<C, Q> {
    /// 先行順に訪れる。f が false を返したらその部分木の子は訪れない
    pub fn walk(&self, f: &mut dyn FnMut(&Expr<C, Q>) -> bool);
    /// 上から書き換える。f が Some(e) を返したらその部分木を e に置き換えて子へは降りない。
    /// None なら構造をそのまま保って子へ降りる。列の型 C → C2、副問い合わせの型 Q → Q2 の変換は f が行う
    pub fn try_map<C2, Q2>(&self, f: &mut dyn FnMut(&Expr<C, Q>) -> Result<Option<Expr<C2, Q2>>>) -> Result<Expr<C2, Q2>>;
}
/// rte = 0 の Var（1 つの表の行に対する式）を PhysCol::Local(col) に直す。SubLink・Aggregate・levels_up > 0 を含んだらエラー
pub fn lower_single_rel(e: &BoundExpr) -> Result<PhysExpr>;
```

`try_map` の f は葉（`Column`、`Aggregate`、`SubLink`）だけを扱えばよい。それ以外の変種の再構築は `try_map` が行う。`C` / `Q` を変える書き換えで f が葉を変換し忘れたら、`try_map` は `Error::internal`（葉の変換漏れ）を返す。

---

## 7. アナライザの出力（`analyzer::bound`）

M1/M2 の `BoundSelect` を `BoundQuery` / `BoundSelect` / `BoundSetExpr` に分け、FROM を範囲表（`Rte`）と結合の木（`FromItem`）にする。

```rust
// analyzer/bound.rs
pub enum BoundStatement {
    Select(Box<BoundQuery>),
    Insert(BoundInsert),
    Update(BoundUpdate),
    Delete(BoundDelete),
    Copy(BoundCopy),
    Explain(Box<BoundExplain>),
    Ddl(BoundDdl),
    Checkpoint,
}

/// WITH + 本体 + ORDER BY / LIMIT / OFFSET
pub struct BoundQuery {
    pub ctes: Vec<BoundCte>,
    pub body: BoundSetExpr,
    /// 本体の targets（Select）または出力列（Values / SetOp）の位置を指す。resjunk を指してもよい
    pub order_by: Vec<BoundSortKey>,
    pub limit: Option<BoundExpr>,
    pub offset: Option<BoundExpr>,
    /// 外に見える出力列（名前・型・元の表と attnum）
    pub columns: Vec<OutputColumn>,
}

pub enum BoundSetExpr {
    Select(Box<BoundSelect>),
    /// 単独の VALUES。各行は列型にそろえた式
    Values { rows: Vec<Vec<BoundExpr>>, types: Vec<SqlType> },
    /// 腕は ORDER BY / LIMIT を持てる（括弧つき）ので BoundQuery。
    /// left_coerce / right_coerce は、腕の出力列を共通型に直す式の並び。腕の出力行の i 番目を Var { rte: RteId(0), col: i, levels_up: 0 } で参照する（M1 の BoundInsert.coercions と同じ約束）。None = 型変換不要
    SetOp { op: SetOpKind, all: bool, left: Box<BoundQuery>, right: Box<BoundQuery>,
            left_coerce: Option<Vec<BoundExpr>>, right_coerce: Option<Vec<BoundExpr>>, types: Vec<SqlType> },
}

pub struct BoundSelect {
    /// このスコープの範囲表。JOIN 自体も RTE になる
    pub rtable: Vec<Rte>,
    /// FROM 句の項目（カンマ区切りごと）。空 = FROM なし
    pub from: Vec<FromItem>,
    /// WHERE。集約を含まない
    pub filter: Option<BoundExpr>,
    /// GROUP BY の式（位置番号・別名は解決済み）
    pub group_by: Vec<BoundExpr>,
    /// HAVING。集約を含んでよい
    pub having: Option<BoundExpr>,
    /// 集約・GROUP BY・HAVING のいずれかがあるか。group_by が空で true なら全体を 1 グループとする
    pub has_agg: bool,
    /// 先頭 n_visible 個が出力列、以降は resjunk（ORDER BY 用）。集約を含んでよい
    pub targets: Vec<BoundExpr>,
    pub n_visible: usize,
    pub distinct: BoundDistinct,
}

pub enum BoundDistinct { None, All, On(Vec<usize> /* targets の位置。ORDER BY の先頭と一致していることを検査済み */) }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BoundSortKey { pub target: usize, pub descending: bool, pub nulls_first: bool }

pub enum FromItem {
    /// Table / Subquery / Values / Function / CteRef の RTE
    Scan(RteId),
    Join { rte: RteId, kind: JoinType, left: Box<FromItem>, right: Box<FromItem>, on: Option<BoundExpr> },
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinType { Inner, Left, Right, Full, Cross }

pub struct Rte {
    pub kind: RteKind,
    /// 別名、なければ表名。別名なしの JOIN / 副問い合わせ / VALUES は None
    pub refname: Option<String>,
    /// `SELECT *` で展開される順の列。表なら attnum 順のユーザー列（システム列は含まない）
    pub columns: Vec<RteColumn>,
    pub span: Span,
}
pub struct RteColumn { pub name: String, pub ty: SqlType }

pub enum RteKind {
    Table { table: Arc<TableDef> },
    Subquery { query: Box<BoundQuery> },
    Values { rows: Vec<Vec<BoundExpr>> },
    /// FROM 句の関数呼び出し（generate_series）。call は Function の式
    Function { call: BoundExpr },
    /// columns の i 番目が子のどの列か（USING / NATURAL で併合した列が先頭に来る）
    Join { kind: JoinType, left: RteId, right: RteId, sources: Vec<JoinColSource> },
    /// levels_up は CTE を宣言した BoundQuery までの入れ子の深さ
    CteRef { levels_up: u16, cte: CteId },
}
pub enum JoinColSource { Left(u16), Right(u16), Coalesce(u16, u16) }

pub struct BoundCte { pub name: String, pub query: BoundQuery, pub materialize: CteMaterialize, pub col_aliases: Vec<String> }
pub enum CteMaterialize { Default, Always, Never }

pub struct OutputColumn { pub name: String, pub ty: SqlType, pub table_oid: Oid, pub attnum: i16 }   // M1 から変更なし
```

DML の Bound:

```rust
pub struct BoundInsert {
    pub table: Arc<TableDef>,
    pub source: Box<BoundQuery>,
    pub coercions: Option<Vec<BoundExpr>>,     // M1 と同じ約束（source の出力列を Var { rte: 0, col: i } で参照）
    pub column_map: Vec<Option<usize>>,
    pub defaults: Vec<Option<BoundExpr>>,      // 列型にそろえた式（Var を含まない。nextval などは含みうる）
    pub checks: Vec<BoundCheck>,               // rte 0 = 対象表
    pub overriding: Option<OverridingKind>,
    pub returning: Option<BoundReturning>,
}
pub struct BoundUpdate {
    pub rtable: Vec<Rte>,                      // rtable[0] = 対象表（RteKind::Table）。以降は FROM 句の RTE
    pub from: Vec<FromItem>,                   // UPDATE ... FROM の項目。対象表は含めない
    pub filter: Option<BoundExpr>,
    pub assignments: Vec<(usize /* attnum - 1 */, UpdateSource)>,   // 式は rtable 全体の Var を参照してよい
    pub checks: Vec<BoundCheck>,
    pub not_null: Vec<bool>,
    pub returning: Option<BoundReturning>,
}
pub struct BoundDelete {
    pub rtable: Vec<Rte>,                      // rtable[0] = 対象表
    pub from: Vec<FromItem>,                   // DELETE ... USING の項目
    pub filter: Option<BoundExpr>,
    pub returning: Option<BoundReturning>,
}
pub struct BoundCheck { pub name: String, pub expr: BoundExpr }       // rte 0 の Var だけ
pub struct BoundReturning { pub targets: Vec<BoundExpr>, pub columns: Vec<OutputColumn> }   // 対象表の Var だけ（M4）
pub enum UpdateSource { Expr(BoundExpr), Default(Option<BoundExpr>) }  // M2 から変更なし（式の Var が rtable を指す）
pub use crate::catalog::IdentityKind;
pub use crate::sql::ast::OverridingKind;      // System | User
```

DDL・COPY・EXPLAIN の Bound（名前だけここで固定。フィールドは持ち主の章が決める）:

```rust
pub enum BoundDdl {
    CreateTable(BoundCreateTable),            // 07・08
    DropTable(BoundDropTable),                // 07
    CreateIndex(BoundCreateIndex),            // 07
    DropIndex(BoundDropIndex),                // 07
    CreateSequence(BoundCreateSequence),      // 08
    AlterSequence(BoundAlterSequence),        // 08
    DropSequence(BoundDropSequence),          // 08
    AlterTableAddConstraint(BoundAlterTableAddConstraint),   // 07
    AlterTableOwner(BoundAlterTableOwner),    // 07（何もしない）
    Truncate(BoundTruncate),                  // 07
    Vacuum(BoundVacuum),                      // 07（VACUUM と ANALYZE。何もしない）
}
pub struct BoundCreateTable {                 // M2 のフィールドに追加
    pub schema: String, pub name: String, pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>, pub checks: Vec<CheckDef>,
    pub constraints: Vec<BoundIndexConstraint>,        // PRIMARY KEY / UNIQUE（07）
    pub sequences: Vec<BoundCreateSequence>,           // SERIAL / IDENTITY が作る暗黙のシーケンス（08）。owned_by は列
}
pub struct BoundIndexConstraint { pub name: String, pub kind: IndexConstraintKind, pub columns: Vec<i16 /* attnum */> }
pub enum IndexConstraintKind { PrimaryKey, Unique }
pub struct BoundCopy { /* 10 */ }
pub struct BoundExplain { pub options: ExplainOptions, pub inner: BoundStatement }
pub struct ExplainOptions { pub analyze: bool, pub verbose: bool, pub costs: bool, pub timing: bool, pub summary: bool }
```

アナライザの入口は変えない: `pub fn analyze(stmt: &Statement, catalog: &dyn CatalogReader) -> Result<BoundStatement>`。

### 7.1 AST の追加（名前だけ固定。フィールドは持ち主の章が決める）

M1 の AST（`sql/ast.rs`）は、JOIN（`TableRef::Join`）、副問い合わせ（`TableRef::Subquery`、`Expr::{InSubquery, Exists, Subquery}`）、集合演算（`QueryBody::SetOp`、`Nested`）、`DISTINCT ON`（`Distinct::On`）、`EXPLAIN`（`Explain`）、`IS DISTINCT FROM` をすでに持つが、パーサが `not_supported` で拒否しているものがある（`WITH`、`LATERAL`、`DISTINCT ON` など）。**実際の対応状況は実装を調べて確かめる**（S1 の最初の作業）。足すもの:

| 追加 | 持ち主の章 |
|---|---|
| `Query.with: Option<With>`、`With { ctes: Vec<Cte>, recursive: bool }`、`Cte { name, columns, materialized: Option<bool>, query, span }` | 03 |
| `TableRef::Function { name, args, alias, span }`（`generate_series`）、`TableRef::Values { query, alias, span }`（`(VALUES ...) AS t(a, b)`。`Subquery` で表せるなら不要） | 03 |
| `Expr::Function` に `filter: Option<Box<Expr>>`（`FILTER (WHERE ...)`）、`Expr::QuantifiedSubquery { expr, op, quantifier, query, span }`（`x op ANY\|ALL (SELECT ...)`）、`Expr::Collate`、スキーマ修飾の演算子 `OPERATOR(pg_catalog.~)` | 03、09 |
| `Statement::CreateIndex(CreateIndex)`、`DropIndex(DropIndex)`、`AlterTable(AlterTable)`、`Truncate(Truncate)`、`Vacuum(Vacuum)`（`VACUUM` と `ANALYZE`） | 07 |
| `Statement::CreateSequence(CreateSequence)`、`AlterSequence(AlterSequence)`、`DropSequence(DropSequence)`、列定義の `GENERATED ... AS IDENTITY`、`serial` 系の型名、`INSERT ... OVERRIDING` | 08 |
| `Statement::Copy(Copy)`、`Explain` のオプション（`ExplainOption { name, value }` の並び） | 10 |
| 型名: `numeric(p, s)`、`char(n)` / `character(n)` / `bpchar`、`date`、`timestamp[(p)] [WITH\|WITHOUT TIME ZONE]`、`timestamptz`、`regclass`、`regtype` | 09 |
| `CREATE TABLE` の `WITH (fillfactor = N)`、`PRIMARY KEY` / `UNIQUE`（M1 から構文はある。`0A000` をやめる） | 07 |
| `UPDATE ... FROM`、`DELETE ... USING`、`RETURNING`（M2 でも構文は受け付けて `0A000`） | 03、05 |


---

## 8. 論理プラン（`planner::logical`）

```rust
pub type LExpr = Expr<ColId, Box<LogicalSubquery>>;

pub struct LogicalSubquery { pub plan: LogicalPlan, pub output: Vec<ColId> }

/// 1 つの文の中で一意な列の台帳。副問い合わせ・CTE とも共有する
#[derive(Debug, Default)]
pub struct ColumnArena { /* Vec<ColumnInfo> */ }
pub struct ColumnInfo {
    pub name: String,
    /// 表示用の修飾名（"t"）。EXPLAIN と deparse が使う
    pub qualifier: Option<String>,
    pub ty: SqlType,
    /// ベーステーブルの列そのものなら (表の OID, attnum)
    pub origin: Option<(Oid, i16)>,
}
impl ColumnArena { pub fn add(&mut self, info: ColumnInfo) -> ColId; pub fn get(&self, id: ColId) -> &ColumnInfo; pub fn len(&self) -> usize; }

pub struct LogicalQuery {
    pub plan: LogicalPlan,
    pub arena: ColumnArena,
    /// 出力列（resjunk を除く可視列。plan の出力列の先頭）
    pub output: Vec<ColId>,
    pub columns: Vec<OutputColumn>,
    pub ctes: Vec<LogicalCte>,
    pub n_subplans_hint: usize,
}
pub struct LogicalCte { pub name: String, pub plan: LogicalPlan, pub output: Vec<ColId>, pub refs: u32, pub materialize: CteMaterialize, pub inline: bool }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinKind { Inner, Left, Full, Semi, Anti }       // RIGHT / CROSS は build が Left / Inner に直す
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetOpKind { Union, Intersect, Except }

pub struct LSortKey { pub expr: LExpr, pub descending: bool, pub nulls_first: bool }

pub enum LogicalPlan {
    /// ベーステーブルの走査。cols は attnum 順のユーザー列、system_columns は参照された分だけ
    Get { rel: RelHandle, table: Arc<TableDef>, alias: Option<String>, cols: Vec<ColId>, system_columns: Vec<(SystemColumn, ColId)> },
    Values { rows: Vec<Vec<LExpr>>, cols: Vec<ColId> },
    FunctionScan { func: &'static BuiltinFunction, args: Vec<LExpr>, alias: Option<String>, cols: Vec<ColId> },
    CteScan { cte: CteId, alias: Option<String>, cols: Vec<ColId> },
    Filter { input: Box<LogicalPlan>, predicate: LExpr },
    /// 出力 = exprs の並び（列の刈り込みもここで表す）
    Project { input: Box<LogicalPlan>, exprs: Vec<(ColId, LExpr)> },
    /// 出力 = left の出力 ++ right の出力（Semi / Anti は left の出力だけ）。on = None は直積
    Join { kind: JoinKind, left: Box<LogicalPlan>, right: Box<LogicalPlan>, on: Option<LExpr> },
    /// 出力 = group_by の列 ++ aggs の列（having は Filter として上に置く）
    Aggregate { input: Box<LogicalPlan>, group_by: Vec<(ColId, LExpr)>, aggs: Vec<(ColId, LAggCall)> },
    /// on = None: 全列で重複除去。Some: DISTINCT ON（入力は on の式で整列済み）
    Distinct { input: Box<LogicalPlan>, on: Option<Vec<LExpr>> },
    Sort { input: Box<LogicalPlan>, keys: Vec<LSortKey> },
    Limit { input: Box<LogicalPlan>, limit: Option<LExpr>, offset: Option<LExpr> },
    SetOp { op: SetOpKind, all: bool, left: Box<LogicalPlan>, right: Box<LogicalPlan>, cols: Vec<ColId>, left_cols: Vec<ColId>, right_cols: Vec<ColId> },
    /// FROM なし: 1 行。one_time_filter が偽なら 0 行
    Result { one_time_filter: Option<LExpr>, cols: Vec<ColId> },
    /// 定数畳み込みで 0 行と分かった部分
    Empty { cols: Vec<ColId> },
    Insert { table: Arc<TableDef>, rel: RelHandle, input: Box<LogicalPlan>, input_cols: Vec<ColId>,
             column_map: Vec<Option<usize>>, defaults: Vec<Option<PhysExpr>>, checks: Vec<PhysCheck>, not_null: Vec<bool>,
             returning: Option<Vec<PhysExpr>> },
    /// input の出力: 対象表のユーザー列 ++ ctid ++ 代入する列の新しい値（new_values と同じ順）。代入式は input の Project が計算する
    Update { table: Arc<TableDef>, rel: RelHandle, input: Box<LogicalPlan>, old_cols: Vec<ColId>, ctid: ColId,
             new_values: Vec<(usize /* attnum - 1 */, ColId)>, checks: Vec<PhysCheck>, not_null: Vec<bool>, returning: Option<Vec<PhysExpr>> },
    Delete { table: Arc<TableDef>, rel: RelHandle, input: Box<LogicalPlan>, old_cols: Vec<ColId>, ctid: ColId, returning: Option<Vec<PhysExpr>> },
}
pub type LAggCall = AggCall<ColId, Box<LogicalSubquery>>;
```

- DML ノードの `defaults` / `checks` / `returning` は、対象表の 1 行だけを見る式なので `PhysExpr`（`lower_single_rel` 済み）のまま持つ（論理プランで唯一 `PhysExpr` が現れる箇所）。
- `JoinKind::Semi` / `Anti` は利用者が直接書けない。サブクエリの書き換え（`04-planner-optimizer.md`）だけが作る。

---

## 9. 物理プラン（`planner::physical`）

### 9.1 入口

```rust
// planner/mod.rs
pub struct PlanEnv<'a> {
    pub catalog: &'a dyn CatalogReader,
    /// nblocks を問い合わせる（ビルド側の選択）
    pub storage: &'a dyn TableStore,
    pub settings: &'a PlannerSettings,
    pub type_env: &'a TypeEnv<'a>,        // 定数畳み込みでリテラルのキャストを評価する
    pub want_explain: bool,
}
pub fn plan(stmt: &BoundStatement, env: &PlanEnv<'_>) -> Result<PhysicalQuery>;

/// settings.rs の値から作る
#[derive(Clone, Debug)]
pub struct PlannerSettings {
    pub enable_seqscan: bool, pub enable_indexscan: bool, pub enable_hashjoin: bool, pub enable_nestloop: bool,
    pub enable_hashagg: bool, pub enable_sort: bool, pub enable_material: bool,
    pub query_mem_limit: usize,
}
```

### 9.2 物理プラン

```rust
pub type PhysExpr = Expr<PhysCol, SubPlanId>;
pub struct PhysCheck { pub name: String, pub expr: PhysExpr }     // 型は bool、NULL は通す

pub struct SortKey { pub expr: PhysExpr, pub descending: bool, pub nulls_first: bool }

pub struct PhysAgg {
    pub kind: AggKind, pub arg_types: Vec<SqlType>, pub args: Vec<PhysExpr>,
    pub distinct: bool, pub filter: Option<PhysExpr>, pub result: SqlType,
}

pub enum IndexScanKey { Eq(PhysExpr), IsNull }
pub struct RangeBound { pub expr: PhysExpr, pub inclusive: bool }
/// 先頭から eq.len() 個の列が等値（または IS NULL）、次の列に lower / upper の範囲
pub struct IndexScanKeys { pub eq: Vec<IndexScanKey>, pub lower: Option<RangeBound>, pub upper: Option<RangeBound> }
pub use crate::storage::ScanDirection;      // Forward | Backward（定義は storage/mod.rs。planner が storage に依存するのは許される向き）

pub enum PhysicalPlan {
    Result { exprs: Vec<PhysExpr>, one_time_filter: Option<PhysExpr> },
    Values { rows: Vec<Vec<PhysExpr>> },
    /// 出力 = ユーザー列（attnum 順）++ system_columns。filter は走査中に評価する
    SeqScan { rel: RelHandle, columns: Vec<SqlType>, system_columns: Vec<SystemColumn>, filter: Option<PhysExpr> },
    /// 出力の形は SeqScan と同じ。keys で TID を集め、ヒープを引いて可視性を判定し、filter で再評価する
    IndexScan { rel: RelHandle, index: IndexHandle, keys: IndexScanKeys, direction: ScanDirection,
                columns: Vec<SqlType>, system_columns: Vec<SystemColumn>, filter: Option<PhysExpr> },
    FunctionScan { func: &'static BuiltinFunction, args: Vec<PhysExpr> },
    Filter { input: Box<PhysicalPlan>, predicate: PhysExpr },
    Project { input: Box<PhysicalPlan>, exprs: Vec<PhysExpr> },
    Sort { input: Box<PhysicalPlan>, keys: Vec<SortKey> },
    /// 入力は整列済み。`key_cols`（入力の列位置。順不同）の値の組が前の行と違う最初の行だけ通す（DISTINCT ON）。
    /// 入力の Sort の先頭 key_cols.len() 個の key の集合が key_cols の式の集合と一致する（03 が 42P10 で保証し、04 §5.5 が
    /// `order ++ 未出の ON の式` で整列する）。ON の式が先頭に並ぶ保証も、Project で先頭に出す処理もない（レビュー対応 R-03）
    Unique { input: Box<PhysicalPlan>, key_cols: Vec<usize> },
    Distinct { input: Box<PhysicalPlan> },
    Limit { input: Box<PhysicalPlan>, limit: Option<PhysExpr>, offset: Option<PhysExpr> },
    Materialize { input: Box<PhysicalPlan> },
    /// 出力は `outer ++ inner`（Semi / Anti は outer だけ）。`outer` は論理プランの left で固定（05 D5-8）。内側 Index Scan のために
    /// INNER の左右を入れ替えるときは、planner が上に並べ直しの Project を置いて論理の左 ++ 右に戻す（04 §7.5.3、レビュー対応 R-14）
    NestedLoopJoin { kind: JoinKind, outer: Box<PhysicalPlan>, inner: Box<PhysicalPlan>, join_filter: Option<PhysExpr>, outer_width: usize, inner_width: usize },
    /// inner を params を設定して rewind し直す（インデックス付き NLJ）
    NestedLoopParam { kind: JoinKind, outer: Box<PhysicalPlan>, inner: Box<PhysicalPlan>, params: Vec<(ParamId, PhysExpr)>, join_filter: Option<PhysExpr>, outer_width: usize, inner_width: usize },
    /// build_is_left = true なら左がビルド側（出力の並びは変えず、内部でプローブ側とビルド側を入れ替える）
    HashJoin { kind: JoinKind, left: Box<PhysicalPlan>, right: Box<PhysicalPlan>, left_keys: Vec<PhysExpr>, right_keys: Vec<PhysExpr>,
               key_types: Vec<SqlType>, residual: Option<PhysExpr>, build_is_left: bool, left_width: usize, right_width: usize },
    /// 出力 = aggs の結果（GROUP BY なし。入力が空でも 1 行）。having は上の Filter
    Aggregate { input: Box<PhysicalPlan>, aggs: Vec<PhysAgg> },
    /// 出力 = keys ++ aggs の結果
    HashAggregate { input: Box<PhysicalPlan>, keys: Vec<PhysExpr>, key_types: Vec<SqlType>, aggs: Vec<PhysAgg> },
    /// 入力は keys で整列済み。出力は HashAggregate と同じ
    GroupAggregate { input: Box<PhysicalPlan>, keys: Vec<PhysExpr>, key_types: Vec<SqlType>, aggs: Vec<PhysAgg> },
    Append { inputs: Vec<PhysicalPlan> },
    HashSetOp { op: SetOpKind, all: bool, left: Box<PhysicalPlan>, right: Box<PhysicalPlan>, key_types: Vec<SqlType> },
    /// PhysicalQuery.ctes[cte] を最初の参照で全行溜め、各参照が自分のカーソルで読む
    CteScan { cte: usize },
    Insert { rel: RelHandle, input: Box<PhysicalPlan>, column_map: Vec<Option<usize>>, defaults: Vec<Option<PhysExpr>>,
             checks: Vec<PhysCheck>, not_null: Vec<bool>, table_name: String, returning: Option<Vec<PhysExpr>> },
    /// 入力の形: 対象表のユーザー列(n) ++ ctid ++ 新しい値(assigned.len())。新しい行 = 古い行の assigned[i].0 番目を入力の n + 1 + i 列目で置き換えたもの
    Update { rel: RelHandle, input: Box<PhysicalPlan>, n_user_cols: usize, assigned: Vec<(usize /* attnum - 1 */, usize /* 入力の列位置 */)>,
             checks: Vec<PhysCheck>, not_null: Vec<bool>, table_name: String, returning: Option<Vec<PhysExpr>> },
    /// 入力の形: 対象表のユーザー列 ++ ctid
    Delete { rel: RelHandle, input: Box<PhysicalPlan>, n_user_cols: usize, returning: Option<Vec<PhysExpr>> },
}
impl PhysicalPlan { pub fn uses_params(&self) -> bool; }   // 木のどこかに PhysCol::Param を含むか
```

- 旧 `Update` の `assignments: Vec<(usize, UpdateSource)>` は廃止し、代入式の評価を論理プランの Project に移した（`UPDATE ... FROM` で代入式が FROM 句の列を参照できるようにするため）。
- `Unique.key_cols` は入力の列位置（DISTINCT ON の式の位置。順不同。Project は挟まない。レビュー対応 R-03）。

### 9.3 問い合わせ全体と EXPLAIN 用の木

```rust
pub struct PhysicalQuery {
    pub root: PhysicalPlan,
    pub subplans: Vec<SubPlanDef>,
    /// MATERIALIZED の CTE（および参照が複数ある CTE）。CteScan { cte } が添字
    pub ctes: Vec<PhysicalPlan>,
    pub n_params: usize,
    /// 可視出力列
    pub output: Vec<OutputColumn>,
    /// want_explain のときだけ。**表示用の木**（root と同形とは限らない。Hash・Append の合成、Filter の併合、Project の透過。C-1）
    pub explain: Option<ExplainNode>,
}

pub struct SubPlanDef {
    pub plan: PhysicalPlan,
    pub kind: SubLinkKind,
    /// Any / All の比較式（SubLinkOutput を含む）
    pub test: Option<PhysExpr>,
    /// 実行前に外側の行から計算して ctx.params に設定する値
    pub params: Vec<(ParamId, PhysExpr)>,
    pub strategy: SubPlanStrategy,
    pub explain: Option<ExplainNode>,
}
pub enum SubPlanStrategy {
    /// 外側の行ごとに rewind して実行（相関あり）
    Rescan,
    /// 1 回だけ実行して結果を保持（相関なし。PostgreSQL の InitPlan）
    InitOnce,
    /// 非相関の ANY で test が等値のとき: 1 回だけ実行してハッシュ集合にする。probe_keys は外側の行に対して評価する式
    Hashed { probe_keys: Vec<PhysExpr>, build_keys: Vec<PhysExpr> },
}

/// EXPLAIN の 1 ノード。**表示用の木**（C-1。定義の正本は 10 §3.2）。`exec_id`（`planner::physical::assign_exec_ids` の先行順の
/// 通し番号。根、subplans の昇順、ctes の昇順）で executor::instrument の計測値と突き合わせる。InitPlan / CTE は
/// その問い合わせ階層の根のノードの子、SubPlan は式を表示したノードの子（04 §8、レビュー対応 R-06）
pub struct ExplainNode {
    /// "Seq Scan on t"、"Hash Join" など。コストと actual は含まない
    pub title: String,
    /// "Filter: (b > 5)" のような詳細行。出す順に並べる（`Rows Removed by ...` の元 `removed` を持つ）
    pub details: Vec<ExplainDetail>,
    /// VERBOSE の "Output:" 行の各要素
    pub output: Vec<String>,
    /// 通常の子とラベルつきの子（InitPlan / SubPlan / CTE）を、出力する順に並べる
    pub children: Vec<ExplainChild>,
    /// 計測値の持ち主。合成ノード（Hash など）は中身のノードの番号を借りる
    pub exec_id: usize,
    /// コスト欄の width
    pub width: u32,
}
// ExplainDetail / RemovedRows / FilterCounter は 10 §3.2
pub struct ExplainChild { pub label: Option<String> /* "InitPlan 1" など */, pub node: ExplainNode }
```

---

## 10. executor の契約

```rust
// executor/mod.rs
pub trait Executor {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>>;
    /// 先頭に戻す。ctx.params が変わっていることがある。
    /// パラメータに依存しない子（PhysicalPlan::uses_params() が false）が溜めた結果（Sort・HashAggregate・Materialize・ハッシュ表）は読み直すだけ。依存するなら作り直す
    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()>;
    fn rows_affected(&self) -> u64 { 0 }
}
pub type BoxedExecutor = Box<dyn Executor>;
pub fn build(plan: &PhysicalPlan) -> BoxedExecutor;

pub struct ExecCtx<'a> {
    pub catalog: &'a dyn CatalogReader,
    pub storage: &'a dyn TableStore,
    pub indexes: &'a dyn IndexStore,                 // ★
    pub txn: &'a mut Transaction,
    pub snapshot: &'a Snapshot,
    pub session: &'a SessionInfo,
    pub runtime: &'a dyn RuntimeInfo,                // M3
    pub interrupts: &'a InterruptFlag,
    pub query: &'a PhysicalQuery,                    // ★ subplans / ctes を引く
    pub params: Vec<Datum>,                          // ★ n_params 個。ParamId の添字
    pub mem: MemBudget,                              // ★
    pub subplans: SubPlanStates,                     // ★ SubPlan / InitPlan の Executor と結果
    pub ctes: CteStates,                             // ★
    pub type_env: &'a TypeEnv<'a>,                   // ★
}
impl ExecCtx<'_> {
    pub fn write_ctx(&mut self) -> Result<WriteCtx>;            // M2
    pub fn check_interrupts(&self) -> Result<()>;               // M3 の InterruptFlag::check
    pub fn eval_ctx(&self) -> EvalCtx<'_>;                      // ★
}

// executor/eval.rs
pub fn eval(expr: &PhysExpr, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Datum>;
pub fn eval_pred(expr: &PhysExpr, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Option<bool>>;
/// SubLink を含まない式（DEFAULT・CHECK の一部の経路）。EvalCtx だけで評価する
pub fn eval_const(expr: &PhysExpr, row: &Row, ctx: &EvalCtx<'_>) -> Result<Datum>;

// executor/mem.rs
#[derive(Debug)]
pub struct MemBudget { /* limit: usize, used: Cell<usize> */ }
impl MemBudget {
    pub fn new(limit: usize) -> Self;
    /// 超えたら 53200 (OUT_OF_MEMORY)。DETAIL に yuzhu.query_mem_limit を書く
    pub fn charge(&self, bytes: usize) -> Result<()>;
    pub fn release(&self, bytes: usize);
    pub fn used(&self) -> usize;
}
pub fn estimate_row_bytes(row: &Row) -> usize;

// executor/dml.rs（INSERT・UPDATE・COPY が共有）
/// ヒープに挿入し、各インデックスに項目を入れる（一意インデックスは検査つき）。失敗したら呼び出し側がトランザクションを中断する
pub fn insert_with_indexes(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid>;
/// 旧版を更新し、新しい TID を全インデックスに入れる（キー列が変わらなくても入れる。HOT なし）
pub fn update_with_indexes(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, tid: Tid, new_row: &[Datum]) -> Result<UpdateOutcome>;
```

`SessionInfo` は M2 のまま。`RuntimeInfo` の追加メソッドは §14.3。

---

## 11. カタログの契約

### 11.1 型の追加と変更（`catalog/mod.rs`）

```rust
/// pg_class.relkind
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelKind { Table /* 'r' */, Index /* 'i' */, Sequence /* 'S' */ }
impl RelKind { pub fn code(self) -> char; pub fn from_code(c: char) -> Option<RelKind>; }

/// pg_attribute.attidentity
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdentityKind { Always /* 'a' */, ByDefault /* 'd' */ }

pub struct ColumnDef {                // M2 のフィールドに追加
    pub name: String, pub attnum: i16, pub ty: SqlType, pub not_null: bool,
    pub default: Option<BoundExprSource>,
    pub identity: Option<IdentityKind>,        // ★
}

pub struct TableDef {                 // M2 のフィールドに追加
    /* oid, namespace, schema, name, kind, locator, columns, checks … */
    pub indexes: Vec<Arc<IndexDef>>,           // ★ OID 昇順。kind = Table のときだけ
    pub sequence: Option<SequenceParams>,      // ★ kind = Sequence のときだけ
    pub identity_seqs: Vec<(i16 /* attnum */, Oid /* シーケンスの OID */)>,   // ★ IDENTITY 列と暗黙のシーケンスの対応
}
impl TableDef {
    pub fn primary_key(&self) -> Option<&Arc<IndexDef>>;
    pub fn column_index(&self, name: &str) -> Option<usize>;      // M2
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexDef {
    pub oid: Oid, pub name: String, pub namespace: Oid, pub table_oid: Oid,
    pub locator: RelFileLocator,
    /// インデックスのキー列（INCLUDE なし。M4 は式・部分インデックスなし）
    pub columns: Vec<IndexColumn>,
    pub unique: bool, pub primary: bool,
    /// PRIMARY KEY / UNIQUE 制約が所有するインデックスなら、その制約
    pub constraint: Option<IndexConstraintRef>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexColumn { pub attnum: i16, pub opclass: Oid, pub opfamily: Oid, pub descending: bool, pub nulls_first: bool }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexConstraintRef { pub oid: Oid, pub name: String }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceParams {
    pub type_oid: Oid,                 // int2 / int4 / int8
    pub start: i64, pub increment: i64, pub min: i64, pub max: i64, pub cache: i64, pub cycle: bool,
    pub owned_by: Option<(Oid /* 表 */, i16 /* attnum */)>,
}
```

- 表・インデックス・シーケンスは `pg_class` で名前空間を共有する。同じ名前空間・同じ名前は `42P07`（relation already exists）。
- `CatalogReader::table()` が返すのは `Table` と `Sequence`（どちらも列とヒープを持ち SELECT できる）。`Index` は返さない。

### 11.2 `CatalogReader` の追加メソッド

```rust
pub trait CatalogReader: std::fmt::Debug {
    /* M2/M3 のメソッド */
    /// 表・インデックス・シーケンスのどれでも（名前空間は共有）。DDL の名前衝突、DROP INDEX、regclass の入力、42809 の判定に使う
    fn relation_kind(&self, schema: Option<&str>, name: &str) -> Result<Option<(Oid, RelKind)>>;
    fn index_by_name(&self, schema: Option<&str>, name: &str) -> Result<Option<Arc<IndexDef>>>;
    fn index_by_oid(&self, oid: Oid) -> Result<Option<Arc<IndexDef>>>;
    /// regclass の出力用。検索パスで見える名前空間なら修飾しない（PostgreSQL の regclassout と同じ）
    fn relation_name(&self, oid: Oid) -> Result<Option<String>>;
    /// 静的な表の既定実装
    fn aggregates_named(&self, name: &str) -> Vec<&'static BuiltinAggregate> { builtin::aggregates_named(name) }
}
```

### 11.3 集約関数と演算子クラスの静的な表

```rust
// catalog/builtin.rs
#[derive(Debug)]
pub struct BuiltinAggregate {
    /// pg_proc.oid（prokind = 'a'）
    pub oid: Oid,
    pub name: &'static str,
    pub args: &'static [Oid],        // count(*) は空
    pub result: Oid,
    pub kind: AggKind,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AggKind {
    CountStar, Count,
    SumInt2, SumInt4, SumInt8, SumFloat4, SumFloat8, SumNumeric,
    AvgInt2, AvgInt4, AvgInt8, AvgFloat4, AvgFloat8, AvgNumeric,
    Min, Max,                         // 引数の型の比較で動く（型ごとに BuiltinAggregate の行がある）
    BoolAnd, BoolOr,                  // every は BoolAnd の別名の行
}
pub fn aggregates_named(name: &str) -> Vec<&'static BuiltinAggregate>;
pub static AGGREGATES: &[BuiltinAggregate];

// catalog/opclass.rs
pub type CmpFn = fn(&Datum, &Datum) -> std::cmp::Ordering;      // NULL は渡されない。同じ型同士（整数の幅違いは可）
pub struct OpFamily { pub oid: Oid, pub name: &'static str }
pub struct OpClass { pub oid: Oid, pub name: &'static str, pub family: Oid, pub input_type: Oid, pub is_default: bool }
pub struct AmOp { pub family: Oid, pub left: Oid, pub right: Oid, pub strategy: u8 /* 1 <, 2 <=, 3 =, 4 >=, 5 > */, pub operator: Oid }
pub struct AmProc { pub family: Oid, pub left: Oid, pub right: Oid, pub support: u8 /* 1 = 比較関数 */, pub proc_oid: Oid, pub cmp: CmpFn }
pub static OPFAMILIES: &[OpFamily];  pub static OPCLASSES: &[OpClass];  pub static AMOPS: &[AmOp];  pub static AMPROCS: &[AmProc];
pub fn default_opclass(type_oid: Oid) -> Option<&'static OpClass>;
pub fn opclass_by_oid(oid: Oid) -> Option<&'static OpClass>;
pub fn opclass_by_name(name: &str) -> Option<&'static OpClass>;
/// (opfamily, 左の型, 右の型) の比較関数
pub fn comparator(family: Oid, left: Oid, right: Oid) -> Option<CmpFn>;
/// この演算子（pg_operator.oid）は、この opfamily の btree で使えるか。使えるなら (strategy, 左の型, 右の型)
pub fn operator_strategy(operator: Oid, family: Oid) -> Option<(u8, Oid, Oid)>;
```

- `initdb` は `OPFAMILIES` などから `pg_opfamily` / `pg_opclass` / `pg_amop` / `pg_amproc` の行を生成する。プランナと B+Tree は実行時にカタログを引かず、この静的な表を引く（M4 にはユーザー定義の opclass がない）。
- M4 で表に入れる opfamily（PG17 の名前）: `bool_ops`、`integer_ops`（int2・int4・int8）、`float_ops`（float4・float8）、`numeric_ops`、`text_ops`（text・varchar・name）、`bpchar_ops`、`oid_ops`、`datetime_ops`（date・timestamp・timestamptz）。OID は `pg_opfamily.dat` に合わせる。それ以外の型に CREATE INDEX すると `42704`（data type X has no default operator class for access method "btree"）。

### 11.4 `RelHandle` と `IndexHandle`（`storage/mod.rs`）

```rust
pub struct RelHandle {                 // M2 に追加
    pub oid: Oid, pub locator: RelFileLocator, pub desc: Arc<TupleDesc>,
    pub indexes: Arc<[IndexHandle]>,   // ★ TableDef.indexes の実行用の写し
}
impl RelHandle { pub fn from_table(def: &TableDef) -> RelHandle; }     // indexes も作る

#[derive(Clone, Debug)]
pub struct IndexHandle {
    pub oid: Oid, pub locator: RelFileLocator,
    pub schema: String, pub name: String, pub table_name: String,       // エラーメッセージ用
    pub unique: bool, pub primary: bool,
    pub columns: Arc<[IndexKeyColumn]>,
}
#[derive(Clone, Debug)]
pub struct IndexKeyColumn {
    pub attnum: i16, pub name: String, pub ty: SqlType, pub attr: AttrDesc,
    pub cmp: CmpFn, pub descending: bool, pub nulls_first: bool,
}
impl IndexHandle { pub fn from_def(def: &IndexDef, table: &TableDef) -> IndexHandle; }
```

### 11.5 追加するカタログ（`catalog/schema.rs`）

| カタログ | OID | 置き場所 | 中身 |
|---|---|---|---|
| `pg_index` | 2610 | DB ごと | 実データ（CREATE INDEX・制約が書く）。列は PG17 のヘッダと同じ（`indkey` は int2vector、`indclass` `indcollation` は oidvector、`indoption` は int2vector、`indexprs` `indpred` は NULL 専用） |
| `pg_depend` | 2608 | DB ごと | 実データ（§11.6） |
| `pg_sequence` | 2224 | DB ごと | 実データ（シーケンスのパラメータ） |
| `pg_opfamily` / `pg_opclass` / `pg_amop` / `pg_amproc` | 2753 / 2616 / 2602 / 2603 | DB ごと | 静的な表から生成。ユーザーは書けない |
| `pg_language` | 2612 | DB ごと | `internal`(12)、`c`(13)、`sql`(14) の 3 行。`pg_proc.prolang = 12` の参照を閉じる（M2-Q13 の宿題） |
| `pg_description` | 2609 | DB ごと | 空（`\dt+` が任意で使う。章 07 が入れるか決める） |

- `pg_aggregate`（2600）は作らない（D-23）。集約関数は `pg_proc` に `prokind = 'a'` の行として入れる。
- カタログのインデックス（OID 2662 など）は作らない。
- 新しい型（§12.1）と演算子・関数・キャスト・集約の行を `pg_type` / `pg_operator` / `pg_proc` / `pg_cast` に入れる。`builtin_hash` に新しい表を含める。
- `CATALOG_VERSION_NO` を M4 の最初の変更で上げる（形式 `YYYYMMDDNN`）。

### 11.6 依存関係（`pg_depend`）に記録するもの

| 依存元 | 依存先 | deptype | 意味 |
|---|---|---|---|
| インデックス（pg_class） | 表の列（pg_class + attnum） | `a`（AUTO） | 列を含む。表を落とせば消える |
| 制約（pg_constraint） | 表（pg_class） | `a` | |
| インデックス（制約が所有） | 制約 | `i`（INTERNAL） | `DROP INDEX t_pkey` は `2BP01` |
| シーケンス（SERIAL） | 表の列 | `a` | 表を落とせば消える。`OWNED BY` |
| シーケンス（IDENTITY） | 表の列 | `i` | |
| 列のデフォルト（pg_attrdef） | シーケンス | `n`（NORMAL） | `DROP SEQUENCE` は `2BP01` |
| 列のデフォルト（pg_attrdef） | 表の列 | `a` | |

`pg_depend` の列は PG17 のヘッダに合わせる（`classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype`）。

---

## 12. 型の契約

### 12.1 OID と性質

M2 の型に次を足す（OID は `pg_type.dat` と一致。`oid` モジュールの定数にする。`builtin.rs` にある `NUMERIC` `DATE` `INTERVAL` の定数は `types::oid` に移す）。

| 型 | OID | typlen | typalign | typcategory | typmod | `Datum` の変種 |
|---|---|---|---|---|---|---|
| `numeric` | 1700 | -1 | i | N | `((p << 16) \| s) + 4`。無指定は -1 | `Numeric(Box<yuzhu_numeric::Numeric>)` |
| `bpchar`（`char(n)`、`character(n)`） | 1042 | -1 | i | S | `n + 4`。`bpchar` と書くと -1（無制限） | `BpChar(String)`（空白で埋めた後の文字列） |
| `date` | 1082 | 4 | i | D | なし | `Date(yuzhu_datetime::Date)` |
| `timestamp` | 1114 | 8 | d | D | 小数秒の桁数 p（0〜6）。+4 しない | `Timestamp(yuzhu_datetime::Timestamp)` |
| `timestamptz`（M2 は NULL 専用 → 値を扱う） | 1184 | 8 | d | D（preferred） | 同上 | `TimestampTz(yuzhu_datetime::TimestampTz)` |
| `regclass` | 2205 | 4 | i | N | なし | `Oid(u32)` |
| `regtype` | 2206 | 4 | i | N | なし | `Oid(u32)` |
| `int2vector` | 22 | -1 | i | A | なし | `Int2Vector(Vec<i16>)`（1 次元の `int2[]`(1005) も同じ変種） |

- 型名の表示（`format_type`）: `numeric`、`numeric(10,2)`、`character(3)`、`character`（typmod -1 の bpchar は `bpchar`）、`date`、`timestamp without time zone`、`timestamp(3) without time zone`、`timestamp with time zone`、`regclass`、`regtype`、`int2vector`。
- `numeric` の配列型、日時の配列型などは作らない（配列は M5）。`pg_type.typarray` は 0。
- `interval`（1186）・`time`（1083）・`timetz`（1266）はキーワードとして来たら `0A000`（`type interval is not supported yet`）。

### 12.2 `Datum` と比較・ハッシュ

```rust
// types/datum.rs（M2/M3 の変種に加えて）
pub enum Datum {
    /* … */
    Numeric(Box<yuzhu_numeric::Numeric>),
    BpChar(String),
    Date(yuzhu_datetime::Date),
    Timestamp(yuzhu_datetime::Timestamp),
    TimestampTz(yuzhu_datetime::TimestampTz),
    Int2Vector(Vec<i16>),
}
```

- `cmp_datum` を拡張する: `Numeric` は `Numeric` の全順序（NaN が最大、`1.10 = 1.1`）、`BpChar` は末尾の空白を除いてバイト比較、日時は整数比較。**変種が型を表すので、型を渡さなくても正しく比較できる**。異なる変種どうし（`timestamp` と `timestamptz` など）の比較は演算子解決でキャストされるので起こらない。起きた場合の順序は M1 の「変種の順位」のまま（バグの検出用）。
- `types::hash::hash_datum(d: &Datum, state: &mut dyn std::hash::Hasher)`: `cmp_datum` が Equal を返す 2 値は同じハッシュになること（-0 と +0、NaN どうし、`1.10` と `1.1`、末尾空白の違う bpchar、整数の幅違い）。ハッシュ結合・ハッシュ集約・DISTINCT・集合演算・ハッシュ化 SubPlan のキーは、**プランナが左右の型をそろえた後の値**にこれを使う（整数と浮動小数のように `cmp_datum` が型をまたいで Equal になるが同じハッシュにならない組を作らない）。
- `types::hash::HashKey(pub Vec<Datum>)`: `Eq` は各要素を `cmp_datum == Equal`（NULL どうしも等しい）、`Hash` は `hash_datum`。結合キーの NULL は一致しないので、executor が NULL を含む組を `HashKey` に入れる前に弾く（外部結合のために別に保持する）。
- `types::cmp::cmp_with_nulls(a: &Datum, b: &Datum, descending: bool, nulls_first: bool) -> Ordering`: NULL を `nulls_first` に従って先頭か末尾に置き、DESC は非 NULL の比較だけを反転する。Sort・B+Tree・Unique が共有する。

### 12.3 ディスク上の符号化（ヒープ・インデックスタプル共通。M2 §3.6 の表に追加）★

| 型 | 符号化 |
|---|---|
| `numeric` | varlena（M2 の 1 バイト / 4 バイトヘッダ）+ 次のペイロード（リトルエンディアン）: `ndigits: u16`、`weight: i16`、`sign: u16`、`dscale: u16`、`digits: [u16; ndigits]`（各 0〜9999、10000 進）。`sign`: `0x0000` 正、`0x4000` 負、`0xC000` NaN、`0xD000` +Infinity、`0xF000` -Infinity。0 は `ndigits = 0, weight = 0, sign = 0`。NaN と ±Infinity は `ndigits = 0` |
| `bpchar` | varlena + UTF-8 のバイト列（空白で埋めた後。typmod の長さには文字数で揃える） |
| `date` | `i32` LE（2000-01-01 からの日数。`i32::MIN` = -infinity、`i32::MAX` = infinity） |
| `timestamp` / `timestamptz` | `i64` LE（2000-01-01 00:00:00 からのマイクロ秒。`timestamptz` は UTC。`i64::MIN` / `i64::MAX` が ∓infinity） |
| `regclass` / `regtype` | `u32` LE |
| `int2vector` | varlena + `i16` LE の並び（yuzhu 独自。PostgreSQL の配列ヘッダは持たない） |

`yuzhu-numeric` の `to_binary()`（Extended Query 用の BE 形式）はディスクには使わない。変換は `types/numeric.rs` に置く。

### 12.4 `TypeEnv`

```rust
// types/mod.rs
/// テキストの入出力が参照する設定。session が文ごとに作る
#[derive(Clone, Copy, Debug)]
pub struct TypeEnv<'a> {
    pub extra_float_digits: i32,
    /// 日時の入出力に必要（DateStyle・TimeZone）。None の文脈（initdb・一部のテスト）で日時型を入出力したら Error::internal
    pub datetime: Option<yuzhu_datetime::DateTimeEnv<'a>>,
}
impl Default for TypeEnv<'_> { /* extra_float_digits = 1, datetime = None */ }

// types/io.rs
pub fn input_text(s: &str, ty: SqlType, env: &TypeEnv<'_>) -> Result<Datum>;
pub fn output_text(d: &Datum, ty: SqlType, env: &TypeEnv<'_>) -> Option<String>;
```

M1 の `OutputOpts` と `output_text_with` は `TypeEnv` に置き換える（M2 の `output_text_regproc` は残す）。regclass の出力は executor の出力段が `CatalogReader::relation_name` で行う（regproc と同じ方式）。

---

## 13. ストレージの契約

### 13.1 `TableStore` の追加（`storage/mod.rs`）

```rust
pub trait TableStore: Send + Sync + std::fmt::Debug {
    /* M2/M3 のメソッド */
    /// 全バージョンの走査（可視性判定なし）。CREATE INDEX の構築用。HeapTuple.row は全ユーザー列
    fn begin_scan_all(&self, rel: &RelHandle) -> Result<HeapScan>;
    /// タプルの状態（clog を引く）。own は呼び出し側のトランザクションの XID
    fn tuple_state(&self, t: &HeapTuple, own: Option<Xid>) -> Result<TupleState>;
    /// SnapshotDirty 相当: 一意性検査用。コマンド ID は見ない
    fn fetch_dirty(&self, rel: &RelHandle, own: Option<Xid>, tid: Tid) -> Result<DirtyResult>;
    /// 計画時の大きさの手がかり
    fn nblocks(&self, rel: &RelHandle) -> Result<u32>;
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirtyResult {
    Invisible,                 // 見えない（中断した挿入、自分が削除済み、コミット済みの削除）
    Visible,                   // 生きている（コミット済み、または自分の挿入）
    WaitFor(Xid),              // 他のトランザクションが挿入中・削除中（M4 では起きない。起きたら内部エラー）
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TupleState { InsertAborted, Live, DeadCommitted, DeletedBySelf, InsertInProgress(Xid), DeleteInProgress(Xid) }
```

### 13.2 `IndexStore`（`storage/mod.rs`）

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScanDirection { Forward, Backward }
pub struct ResolvedScanKeys {
    /// 先頭から等値の列。None は IS NULL
    pub eq: Vec<Option<Datum>>,
    /// eq.len() 番目の列の範囲（値, 境界を含むか）
    pub lower: Option<(Datum, bool)>,
    pub upper: Option<(Datum, bool)>,
}
pub enum UniqueCheck<'a> {
    Skip,
    Check { heap: &'a dyn TableStore, rel: &'a RelHandle, own_xid: Xid },
}
pub enum BuildUnique { No, Yes }
#[derive(Debug, Default, Clone, Copy)]
pub struct BuildStats { pub tuples: u64, pub pages: u32, pub levels: u32 }

pub trait IndexStore: Send + Sync + std::fmt::Debug {
    /// 新しいインデックスのファイル（作成済み）に、メタページと空のルート葉を書く（BTREE_PAGES）
    fn init_index(&self, w: &WriteCtx, index: &IndexHandle) -> Result<()>;
    /// 項目を 1 つ入れる。key はインデックス列の値（NULL を含みうる）。unique かつ check が Check なら重複を検査する
    /// （NULL を含むキーは検査しない）。重複は 23505（`s` `t` `n` のフィールドを付けて返す。**DETAIL `Key (a)=(1) already exists.` は
    /// executor/dml.rs の unique_violation_detail が補う**。C-11）、他トランザクション待ちが必要なら内部エラー（M4）
    fn insert(&self, w: &WriteCtx, index: &IndexHandle, key: &[Datum], tid: Tid, check: UniqueCheck<'_>) -> Result<()>;
    /// 一括構築。init_index 済みの空のインデックスにだけ使える。entries は (key, tid) の昇順に整列済み。
    /// unique == Yes は「入力のすべてが生きている版」として、NULL を含まない隣接キーが全列等しければ 23505（DETAIL なし）。
    /// **C1（CREATE INDEX / ALTER TABLE ADD）は死んだ版の誤検出を避けるため常に No で呼び、重複の検出と DETAIL（`could not create unique index` /
    /// `Key (a)=(1) is duplicated.`）は C1 が書く**（C-11）。ページは 32 枚ずつ BTREE_PAGES で WAL に書く
    fn build(&self, w: &WriteCtx, index: &IndexHandle, entries: &mut dyn Iterator<Item = (Vec<Datum>, Tid)>, unique: BuildUnique) -> Result<BuildStats>;
    fn begin_scan(&self, index: &IndexHandle, keys: &ResolvedScanKeys, dir: ScanDirection) -> Result<IndexScan>;
    /// 次の TID。葉ごとに一致した TID をまとめてコピーし、ページのラッチ・ピンを持ち越さない
    fn scan_next(&self, scan: &mut IndexScan) -> Result<Option<Tid>>;
    fn nblocks(&self, index: &IndexHandle) -> Result<u32>;
    fn unlink_storage(&self, index: &IndexHandle) -> Result<()>;
}
/// storage/btree/scan.rs が定義する。ピンもラッチも持たない（M2 D10 と同じ方針）
#[derive(Debug)] pub struct IndexScan { /* … */ }
```

`StorageStack`（M3）に `index: Arc<dyn IndexStore>`（実体は `BtreeStore`）と `seq: Arc<dyn SequenceStore>` を足す。

### 13.3 `SequenceStore`（`storage/mod.rs`）

```rust
#[derive(Clone, Debug)]
pub struct SequenceHandle { pub oid: Oid, pub locator: RelFileLocator, pub params: SequenceParams }
#[derive(Clone, Copy, Debug)]
pub struct SeqRun { pub first: i64, pub count: u32, pub increment: i64, pub wal_lsn: Lsn }
#[derive(Clone, Copy, Debug)]
pub struct SeqState { pub last_value: i64, pub log_cnt: i64, pub is_called: bool }

pub trait SequenceStore: Send + Sync + std::fmt::Debug {
    /// 1 ページを初期化して SEQ_LOG を書く（ファイルは作成済み）
    fn init(&self, w: &WriteCtx, seq: &SequenceHandle) -> Result<Lsn>;
    /// 最大 count 個の値を払い出す（CACHE。上限・下限・CYCLE の検査を含む。2200H）。排他ラッチの下でその場上書き
    fn fetch(&self, seq: &SequenceHandle, count: u32) -> Result<SeqRun>;
    fn setval(&self, seq: &SequenceHandle, value: i64, is_called: bool) -> Result<Lsn>;
    fn read(&self, seq: &SequenceHandle) -> Result<SeqState>;
    /// ALTER SEQUENCE / TRUNCATE ... RESTART IDENTITY 用
    fn reset(&self, seq: &SequenceHandle, new_params: &SequenceParams, restart_with: Option<i64>) -> Result<Lsn>;
    fn unlink_storage(&self, seq: &SequenceHandle) -> Result<()>;
}
```

払い出した値を `currval` / `lastval` のためにセッションが覚える。`SeqRun.wal_lsn` を `Transaction.wal_flush_upto` に反映する（コミット時にそこまで WAL を flush する。D-9）。

### 13.4 WAL（`wal/mod.rs` への追加）

```rust
pub enum RmgrId { Xlog = 0, Xact = 1, Smgr = 2, Heap = 3, Btree = 4, Seq = 5 }

// storage/btree/wal.rs
pub const BTREE_INSERT_LEAF: u8 = 0x00;   // 葉への 1 項目の挿入（差分。通常の FPW の対象）
pub const BTREE_PAGES: u8 = 0x10;         // 構造変更・初期化・一括構築: 全ページの全画像（FORCE_IMAGE）。理由は main data の先頭 1 バイト
// 0x20 以降は M5 の予約（DELETE、VACUUM、UNLINK_PAGE など）

// storage/sequence.rs
pub const SEQ_LOG: u8 = 0x00;             // blk0 = シーケンスのページ（WILL_INIT、タプル全体）
```

- `BTREE_PAGES` のブロック数は最大 32（`MAX_BLOCK_REFS`）。木の高さ h の構造変更は最大 **3h + 1** ページ（各レベルで左・右・元の右隣、新ルート、メタ。06 §4.3、C-13）。**静的な高さの上限は作らず**、必要なブロック数が 32 を超えたときだけ `54000`。
- `wal::dump`（`yuzhu-waldump`）が Btree と Seq のレコードを人が読める形で出す。実装は各モジュールの `wal.rs` が `describe(rec) -> String` を提供し、`wal/dump.rs` が呼ぶ。
- M3 の `WAL_FORMAT_VERSION` は変えない（M3 のサーバが M4 の WAL を読むことは想定しない。D-28）。

### 13.5 B+Tree のディスク形式の要点（定義の持ち主は `06-btree.md`。ここは他章が前提にする値）★

| 項目 | 値 |
|---|---|
| ブロック 0 | メタページ。ブロック 1 が最初のルート（空のインデックスでも葉のルートを持つ） |
| special 領域 | 16 バイト（`pd_special = 8176`）: `prev: u32`、`next: u32`、`level: u32`、`flags: u16`、`cycleid: u16`。0 = なし |
| flags | `LEAF = 0x0001`、`ROOT = 0x0002`、`META = 0x0008`。`DELETED = 0x0004`、`HALF_DEAD = 0x0010`、`SPLIT_END = 0x0020`、`INCOMPLETE_SPLIT = 0x0080` は M5 の予約で、立っていたら `XX001` |
| メタ | `magic = 0x053162`、`version = 1`、`root`、`level`、`fastroot`、`fastlevel`（M4 は root / level と同じ）、`last_cleanup_num_delpages = 0`、`allequalimage = 0` |
| 行ポインタ | 右端でないページは 1 番が high key、データは 2 番から。右端ページはデータが 1 番から。内部ページの最初のデータ要素はキー属性 0 個（-∞） |
| インデックスタプル | 8 バイトヘッダ（`block: u32`、`offset: u16`、`t_info: u16`: bit15 NULL あり、bit14 可変長あり、bit13 ALT_TID（ピボット）、bit0〜12 タプル長）+ NULL ビットマップ（`INDEX_MAX_KEYS = 32` ビット = 4 バイト、NULL があるときだけ）+ 列データ（ヒープと同じ符号化。データ開始は MAXALIGN）。ピボットは `block` = 子のブロック番号、`offset` の下位 12 ビット = キー属性数、`0x1000` = 末尾にヒープ TID を持つ |
| 最大項目 | `BT_MAX_ITEM_SIZE = 2704`。超えたら `54000`（`index row size N exceeds btree version 4 maximum 2704 for index "x"`） |
| 充填率 | 葉 90、内部 70（一括構築）。右端の葉の分割は左を 90 まで詰める |
| 比較 | 全キー + ヒープ TID の順（TID がタイブレーク）。NULL は最大（ASC なら末尾、`NULLS FIRST` で先頭）。ピボットのキー属性数が少なければ「残りは -∞」 |

---

## 14. セッション・COPY・サーバ・エラーの契約

### 14.1 `Transaction`（`txn/manager.rs`）

```rust
pub struct Transaction {            // M2/M3 のフィールドに追加
    /// 非トランザクション的に書いた WAL（SEQ_LOG）の最後の終端 LSN。XID を持たないトランザクションでも、
    /// コミット時にここまで flush する。0 = なし
    pub wal_flush_upto: Lsn,        // ★
    /// トランザクション開始時刻（PostgreSQL のエポックからのマイクロ秒）。now() が返す
    pub started_at: i64,            // ★
}
```

M3 の `TxnManager::commit` / `abort` のうち XID を持たない経路に `finish_without_xid(flush_upto: Lsn) -> Result<()>` を足す（0 でなければ `wal.flush`）。

### 14.2 設定との対応

`Settings` から `PlannerSettings`（§9.1）と `TypeEnv`（§12.4）を作る関数を `settings.rs` に置く: `Settings::planner_settings(&self) -> PlannerSettings`、`Settings::type_env<'a>(&'a self, ds: &'a DateTimeSettings, zones: &'a ZoneDb, now: i64, names: Option<&'a dyn OidNames>) -> TypeEnv<'a>`（09-P4。C-18）。`ZoneDb` は `Cluster` が 1 つ持つ（`yuzhu_datetime::ZoneDb`）。

### 14.3 `RuntimeInfo` の追加（M3 の trait に足す）

```rust
pub trait RuntimeInfo: std::fmt::Debug {
    /* M3: backend_pid, is_blocked_by, check_interrupts */
    fn transaction_timestamp(&self) -> i64;     // now()
    fn statement_timestamp(&self) -> i64;
    fn clock_timestamp(&self) -> i64;
    fn datetime_env(&self) -> yuzhu_datetime::DateTimeEnv<'_>;
    /// シーケンス関数。25006（READ ONLY）、55000（currval / lastval が未定義）、2200H はここで返す
    fn nextval(&self, seq: Oid) -> Result<i64>;
    fn currval(&self, seq: Oid) -> Result<i64>;
    fn lastval(&self) -> Result<i64>;
    fn setval(&self, seq: Oid, value: i64, is_called: bool) -> Result<i64>;
}
```

### 14.4 `Error` の追加フィールド（`error.rs`）

```rust
pub struct Error {                  // M1 のフィールドに追加
    pub schema: Option<String>, pub table: Option<String>, pub column: Option<String>,
    pub constraint: Option<String>, pub context: Option<String>,
}
impl Error {
    pub fn with_table(self, schema: impl Into<String>, table: impl Into<String>) -> Self;
    pub fn with_column(self, column: impl Into<String>) -> Self;
    pub fn with_constraint(self, name: impl Into<String>) -> Self;
    pub fn with_context(self, ctx: impl Into<String>) -> Self;     // CONTEXT: COPY t, line 3, column a: "x"
}
```

`yuzhu-server` のエラー符号化は `s`（schema）`t`（table）`c`（column）`n`（constraint）`W`（context）を出す。23502（NOT NULL）・23514（CHECK）も M4 でこれらを付ける。

### 14.5 COPY（詳細は `10-explain-copy-compat.md`）

```rust
// session.rs の ResultSink に追加
/// column_formats はプロトコルの Int16 の並び（BackendMessage::CopyInResponse.column_formats: Vec<i16> と同じ型。10 §5.2）。
/// text 形式では全列 0。呼び出しは `sink.copy_in_response(0, &vec![0i16; ncols])`（レビュー対応 R-15）
fn copy_in_response(&mut self, format: u8 /* 0 = text */, column_formats: &[i16]) -> std::io::Result<()>;

impl Session {
    /// execute_simple が戻った後に、サーバが CopyData / CopyDone / CopyFail の受信ループに入るかを判定する
    pub fn is_copying_in(&self) -> bool;
    pub fn copy_data(&mut self, chunk: &[u8], sink: &mut dyn ResultSink) -> std::io::Result<()>;
    /// CopyDone: 残りを処理して CommandComplete("COPY n")、同じ Query の続きの文を実行する
    pub fn copy_done(&mut self, sink: &mut dyn ResultSink) -> std::io::Result<()>;
    pub fn copy_fail(&mut self, message: &str, sink: &mut dyn ResultSink) -> std::io::Result<()>;
}
```

プロトコル: サーバ → `G`（CopyInResponse）、クライアント → `d`（CopyData）* → `c`（CopyDone）または `f`（CopyFail）。COPY IN の間の `H`（Flush）と `S`（Sync）は無視する。

### 14.6 DDL の入口（`ddl/mod.rs`）

```rust
pub struct DdlCtx<'a> {
    pub cluster: &'a Cluster,
    pub db: &'a Arc<DatabaseHandle>,
    pub snapshot: &'a Snapshot,
    pub catalog: &'a dyn CatalogReader,
    pub txn: &'a mut Transaction,
    pub role_oid: Oid,
    pub notices: &'a mut Vec<Notice>,
    pub type_env: &'a TypeEnv<'a>,
}
/// コマンドタグ（"CREATE TABLE"、"CREATE INDEX"、"TRUNCATE TABLE"、"VACUUM" …）を返す
pub fn execute(ctx: &mut DdlCtx<'_>, ddl: BoundDdl) -> Result<String>;
```

---

## 15. 定数・SQLSTATE・設定

### 15.1 定数

```rust
pub const BT_MAX_ITEM_SIZE: usize = 2704;
pub const BT_SPECIAL_SIZE: usize = 16;
pub const BT_MAGIC: u32 = 0x053162;
pub const BT_VERSION: u32 = 1;
pub const BT_META_BLOCK: u32 = 0;
pub const BT_FIRST_ROOT_BLOCK: u32 = 1;
pub const BT_FILLFACTOR_LEAF: usize = 90;
pub const BT_FILLFACTOR_INNER: usize = 70;
pub const INDEX_MAX_KEYS: usize = 32;
pub const SEQ_LOG_VALS: i64 = 32;
pub const SEQ_MAGIC: u32 = 0x1717;
pub const DEFAULT_QUERY_MEM_LIMIT: usize = 256 << 20;
```

### 15.2 OID の方針

- 組み込みの型・演算子・関数・opfamily・opclass の OID は **PostgreSQL 17 の `.dat` と一致**させる。`.dat` から写せなかったものには「（未検証）」を付けて実装前に確かめる。
- ユーザーが作るオブジェクト（表・インデックス・シーケンス・制約・attrdef）は 16384 以上（M2 の `OidAllocator`）。
- 暗黙に作るオブジェクトの名前（PostgreSQL の `ChooseRelationName` / `ChooseConstraintName` と同じ）: PK `t_pkey`、UNIQUE `t_a_key` / `t_a_b_key`、`CREATE INDEX` の省略名 `t_a_idx`、SERIAL のシーケンス `t_a_seq`。衝突したら末尾に `1` `2` … を付ける。

### 15.3 SQLSTATE の追加（`error.rs` の `sqlstate`）

既存のもの（`UNIQUE_VIOLATION` `GROUPING_ERROR` `INVALID_COLUMN_REFERENCE` `INVALID_TABLE_DEFINITION` `OUT_OF_MEMORY` `CARDINALITY_VIOLATION` `PROGRAM_LIMIT_EXCEEDED` `OBJECT_NOT_IN_PREREQUISITE_STATE` `WRONG_OBJECT_TYPE` `DUPLICATE_OBJECT` `DUPLICATE_TABLE` `ACTIVE_SQL_TRANSACTION` など）はそのまま使う。追加:

```rust
pub const SEQUENCE_GENERATOR_LIMIT_EXCEEDED: SqlState = SqlState("2200H");
pub const INVALID_REGULAR_EXPRESSION: SqlState = SqlState("2201B");
pub const BAD_COPY_FILE_FORMAT: SqlState = SqlState("22P04");
pub const DEPENDENT_OBJECTS_STILL_EXIST: SqlState = SqlState("2BP01");
pub const DUPLICATE_ALIAS: SqlState = SqlState("42712");
pub const GENERATED_ALWAYS: SqlState = SqlState("428C9");
```

追加が必要になったら、M2・M3 と同じく `error.rs` の `sqlstate` モジュールへの定数の追記だけは持ち主以外が行ってよい。

### 15.4 設定項目（`settings.rs`）

| 名前 | 既定 | 備考 |
|---|---|---|
| `enable_seqscan` `enable_indexscan` `enable_hashjoin` `enable_nestloop` `enable_hashagg` `enable_sort` `enable_material` | `on` | プランナが読む（PostgreSQL と同じく「他の選択肢があれば避ける」。等値結合で hashjoin と nestloop が両方 off なら nestloop） |
| `enable_indexonlyscan` `enable_bitmapscan` `enable_mergejoin` `enable_tidscan` `enable_partitionwise_join` ほか | `on` | 受け付けて保存するだけ |
| `join_collapse_limit` `from_collapse_limit` | `8` | 保存するだけ |
| `work_mem` / `hash_mem_multiplier` / `maintenance_work_mem` | `4MB` / `2` / `64MB` | 保存するだけ（M6 のスピルの閾値） |
| `yuzhu.query_mem_limit` | `256MB` | 1 文の executor が溜める行の概算の上限。超えたら `53200` |
| `DateStyle` `TimeZone` `IntervalStyle` | M1 のまま | `TypeEnv.datetime` の元。`IntervalStyle` は保存するだけ |
| PostgreSQL 17 にあって意味を持たない GUC | PG の既定 | `settings.rs` の一覧（`INERT_GUCS`）で値の形だけ検査して保存する。`synchronize_seqscans` `row_security` `xmloption` `check_function_bodies` `default_tablespace` `default_table_access_method` `restrict_nonsystem_relation_kind` `transaction_timeout` `lc_messages` `lc_monetary` `lc_numeric` `lc_time` `default_text_search_config` ほか（一覧の確定は `10-explain-copy-compat.md`） |

---

## 16. M2・M3 の契約からの変更点（まとめ）

| 対象 | 変更 | 持ち主の章 |
|---|---|---|
| `BoundExpr` / `BoundExprKind` | `ColumnRef { index }` を `Column(Var)` に。`Expr<C, Q>` の別名に。`Aggregate` `SubLink` `SubLinkOutput` を追加 | 02 |
| `BoundSelect` / `BoundStatement::Select` | `BoundQuery` + `BoundSetExpr` + `BoundSelect`（範囲表・結合の木）に | 02、03 |
| `BoundInsert` / `BoundUpdate` / `BoundDelete` | source は `BoundQuery`。Update / Delete は rtable と from を持つ。`returning` | 02、03、05 |
| `BoundStatement` | `CreateTable` `DropTable` を `Ddl(BoundDdl)` に。`Copy`、`Explain` を追加 | 02、07 |
| `PhysicalPlan` | 式は `PhysExpr`。ノードの追加（§9.2）。`Update` の入力の形と `assignments` の廃止。`plan.rs` → `physical.rs` | 02、04 |
| `planner::plan` | 署名が `plan(&BoundStatement, &PlanEnv) -> Result<PhysicalQuery>` に | 04 |
| `Executor` | `rewind` を必須に。`ExecCtx` に `indexes` `query` `params` `mem` `subplans` `ctes` `type_env`。`eval` は `&mut ExecCtx` | 02、05 |
| `TableDef` / `ColumnDef` / `RelKind` / `RelHandle` | `indexes` `sequence` `identity`、`Index` `Sequence` の種別、`RelHandle.indexes` | 07、06 |
| `CatalogReader` | §11.2 のメソッド | 07 |
| `TableStore` | `begin_scan_all` `tuple_state` `fetch_dirty` `nblocks` | 06（H4） |
| `StorageStack` | `index` と `seq` | 06、08 |
| `Datum` | §12.2 の変種 | 09 |
| `types::io` | `OutputOpts` → `TypeEnv`。`input_text` に env | 09 |
| `cmp_datum` | 新しい変種の比較 | 09 |
| `RuntimeInfo` | §14.3 | 08、09 |
| `Transaction` | `wal_flush_upto` `started_at` | 08 |
| `Error` | §14.4 | 02（A） |
| `RmgrId` / `recovery::dispatch` | `Btree` `Seq` | 06、08 |
| `ResultSink` / `Session` | COPY IN | 10 |
| `session.rs` | CREATE / DROP の実行を `ddl/` に移す | 07、10 |
| `bootstrap.rs` | 新しいカタログと行の生成、`pg_language` | 07 |
| `settings.rs` | §15.4 | 10、04 |
| `CATALOG_VERSION_NO` | 上げる | 07 |
| yuzhu-server の protocol | Copy メッセージ、エラーの追加フィールド | 10 |

---

## 17. 担当（実装の区切り）と章との対応

各担当は**自分の範囲のファイルだけ**を編集する（M1〜M3 と同じ）。他の担当の範囲で直すべき点は自分では直さず依頼する。例外は `error.rs` の `sqlstate` への定数の追記だけ。作業中も crate 全体がコンパイルできる状態を保つ。

**この表は 11 §4.1 の確定表を反映した統合版**（レビュー対応 R-02。日数・範囲・追加ファイルの持ち主。初版の合計 115.5 日は **141.8 日**になった。内訳の増加は 11 §4.1）。共有ファイルの区画の規則は 11 §4.2。

**P0 が全員の足場を置く**: A が型を、P0 が ★ のファイルをすべて「関数の署名つきのスタブ」（中身は `Err(Error::not_supported(..))`）として置き、`analyzer/expr.rs` などの分岐点に呼び出しの口（`agg::analyze_agg_call`、`sublink::analyze_sublink` など）を足す。以降の担当はスタブの中身だけを書く。

| 担当 | 範囲（編集してよいファイル） | 依存 | 日数 | 章 |
|---|---|---|---|---|
| **A 基盤** | `expr/mod.rs`（型）、`error.rs`、`types/mod.rs`（OID・TypeEnv）、`types/datum.rs`（変種）、`catalog/mod.rs`（型）、`analyzer/bound.rs` と `planner/{logical,physical}.rs`（新しい型を別の名前で足す。旧型は P0 が置き換える）、`storage/mod.rs`（トレイト）、`wal/mod.rs`、`executor/mod.rs`（型）、`lib.rs`、各 `Cargo.toml`、`CATALOG_VERSION_NO`。追加: `Cast.implicit`・`CastMethod::Env`・`ExplainNode` の拡張・`TypeEnv.names`・`DebugKnobs` の追加・`sqlstate` の 11 定数・`PhysicalQuery::{single, empty}`・`BoundQuery::walk_exprs` | なし | 2.0 | 02 |
| **P0 パイプライン移行** | `analyzer/*`（`bound.rs` の置き換え、既存の単一表の解析を `Var` / `BoundQuery` に）、`planner/{mod,build,physicalize}.rs`（既存のノードだけ）、`executor/{mod,build,eval}.rs` と既存の `nodes/*`（`PhysExpr`・`rewind` 化）、`session.rs` の plan 呼び出し、★ のスタブ一式。`Filter`・`SeqScan` の `set_counters`。**完了条件**: `tests/slt/m1`〜`m3` が yuzhu で通る | A | 5.1 | 02 |
| **S1 パーサ** | `sql/*` の M4 分すべて（§7.1。問い合わせ・DDL・ユーティリティ・COPY・EXPLAIN・型名） | A（AST の型） | 5.0 | 03、07、08、09、10 |
| **N1 解析: FROM** | `analyzer/{from,scope}.rs`、`dml.rs` の FROM / USING、FROM 句の関数。P0 の後の `analyzer/{expr,coerce,resolve}.rs` の持ち主 | P0 | 4.3 | 03 |
| **N2 解析: 集約** | `analyzer/agg.rs`、`select.rs` の GROUP BY / DISTINCT ON / ORDER BY | P0 | 4 | 03 |
| **N3 解析: サブクエリ** | `analyzer/{sublink,setop,cte}.rs` | P0 | 5 | 03 |
| **L1 論理プラン** | `planner/{build,rules/*,util,print,validate}.rs`（論理の分） | P0 | 6.2 | 04 |
| **L2 物理化** | `planner/{physicalize,index_select,explain_tree,size}.rs`、`print` / `validate` の物理の分 | P0、B2（opclass の表） | 6.9 | 04、10 |
| **X1 実行: 結合** | `executor/subplan.rs`、`nodes/{nested_loop,hash_join,materialize}.rs`。P0 の後の `executor/eval.rs` の持ち主 | P0 | 5.3 | 05 |
| **X2 実行: 集約** | `executor/{agg,mem}.rs`、`types/hash.rs`、`nodes/{aggregate,hash_aggregate,group_aggregate,unique,append,hash_setop,cte_scan,function_scan}.rs` | P0、T1（numeric） | 5 | 05 |
| **X3 実行: インデックスと DML** | `executor/dml.rs`、`nodes/{index_scan,insert,update,delete}.rs` | P0、B1 | 4.1 | 05 |
| **H4 ヒープ拡張** | `storage/heap/*`、`heap_store.rs`、`page.rs`（追加のみ） | M3 の D、C | 1.5 | 06 |
| **B1 B+Tree 本体** | `storage/btree/{mod,page,tuple,meta,search,insert,split,wal}.rs`、`storage/index_store.rs` の `init_index` / `insert` | A、H4、M3 の W1・W2 | 6 | 06 |
| **B2 B+Tree 走査・構築** | `storage/btree/{scan,build,unique,check}.rs`、`catalog/opclass.rs` | B1 | 5 | 06 |
| **C1 カタログと DDL** | `catalog/{schema,rows,store,cache,reader,naming,depend,check}.rs`、`ddl/{mod,table,index,constraint,truncate,vacuum,depend}.rs`、`analyzer/{ddl_constraint,ddl_index}.rs`、`bootstrap.rs` | A、B2（opclass） | 8.2 | 07 |
| **Q1 シーケンス** | `storage/sequence.rs`、`catalog/seq_params.rs`、`executor/seq.rs`、`ddl/sequence.rs`、**`analyzer/ddl.rs` の持ち主**（SERIAL / IDENTITY、型名、07 からの依頼）、`types/ops.rs` と `catalog/builtin.rs` の `sequence` 区画（シーケンス関数）、`txn/manager.rs` の追加 | A、C1 | 5.8 | 08 |
| **T1 numeric** | `types/numeric.rs`、numeric の演算子・キャスト・関数の行 | A | 3 | 09 |
| **T2 日時と char(n)** | `types/{datetime,bpchar}.rs`、日時と bpchar の行、`TypeEnv` の組み立て（`settings.rs` の関数は S と共同） | A | 4 | 09 |
| **T3 関数・集約・正規表現** | `types/{regex,sys,cmp}.rs`、`catalog/{builtin(agg-set-sys 区画),names}.rs`、`regclass` / `regtype` | A | 5（任意 +0.3） | 09 |
| **E1 EXPLAIN と deparse** | `explain/*`、`deparse/*`、`executor/instrument.rs`、`pg_get_constraintdef` / `pg_get_indexdef` の行と本体 | P0、L2 | 6.0 | 10 |
| **O1 COPY** | `copy/*` | P0、X3 | 4 | 10 |
| **S セッション** | `session.rs`、`settings.rs`、`engine.rs`、`testing.rs` | A、P0、C1 | 4.5 | 10 |
| **J サーバ** | `yuzhu-server/` 全体 | S | 1.5 | 10 |
| **R2 クラッシュ試験の追加** | `yuzhu-core/tests/crash_sim/*` にワークロード 6〜8（B+Tree・シーケンス・DDL）、不変条件 I13〜I16、変異テスト | B1、Q1、C1、M3 の T | 5.0 | 11 |
| **K 共有テスト**（K1〜K4 に分割） | `tests/slt/m4/*`、`tests/compat/*`、`tests/restart/m4/*`、`tests/isolation/specs`、`tests/tools/slttools/*`、`tests/gen/*`、`tests/imported/*`、`tests/done-check.sh`、`tests/run.sh`、`.github/workflows/*`、`Dockerfile` と `sandbox/Dockerfile`（tzdata） | なし（PostgreSQL で先に通せる） | 24.3 | 11 |
| **Z 差分ランダムテスト** | **`tests/tools/difftest/*`**（`yuzhu-fuzz-sql/` は作らない。C-5） | N1〜N3、X1〜X2（最初の 0.5 日は不要） | 4.5 | 11 |
| M3 の持ち主への依頼 | `PinnedBuffer::{read_tree, write_tree}`、`extend_tree`、`page_mut_hint`、`wal/dump.rs`・`recovery::dispatch` の追加、`Wal::redo_lsn()` ほか（11 §4.1） | — | 0.6 | 06、08 |

- 進め方: (1) A → (2) P0 と、P0 に触れない担当（S1、H4、B1、T1〜T3、K）を並列に開始 → (3) P0 のマージ後に N1〜N3、L1、L2、X1〜X3、C1、E1、O1 を並列に → (4) B2 → L2（インデックス選択）・C1 → Q1 → S・J → (5) 結合して `tests/run.sh --target yuzhu`（m1〜m4）、再起動テスト、クラッシュ試験、`tests/compat`（psql・pgbench）、差分ランダムテストを通す → M4 完了。
- 工数は AI の実装エージェント 1 本の日数の粗い見積もり。**合計は 141.8 日**（任意を含めて 143.3 日）、A の後の最長経路は P0 → L2 → 結合 → 完了判定で約 21〜23 日（11 §4.3）。完了の判定は `01-scope-decisions.md` §3 の `tests/done-check.sh`（ローカル）。

---

## 18. テストの置き場所（共通の名前）

| 置き場所 | 内容 | 章 |
|---|---|---|
| `tests/slt/m4/{index,constraint,seq,join,agg,subquery,setop,cte,dml,types,copy,explain,psql,plan_variants,catalog,ddl,mem,z_final}/*.slt` | 共通の SQL テスト（PostgreSQL 17 で期待値を確かめる。M1〜M3 と同じ規則）。EXPLAIN とメモリ上限は `onlyif yuzhu`。`ddl/`・`mem/`・`z_final/` は 11 D11-2 | 各章、11 |
| `tests/compat/{run.sh,psql/,copy/,pgbench/}` | psql の `\dt` `\dn` `\di` `\ds` `\l` と `\d シーケンス` の出力、`pgbench -i` と tpcb-like の完走と不変条件を PostgreSQL と yuzhu で比べる | 10、11 |
| `tests/restart/m4/<シナリオ>/{mode, NN-*.mode}` | 再起動・クラッシュをまたぐテスト（インデックス・シーケンス・DDL）。`mode` は `restart` / `crash` / `both`（11 D11-5） | 06、07、08、11 |
| `yuzhu-core/tests/crash_sim/` | 層 1 のクラッシュ試験にワークロード 6（インデックス付き表。`shared_buffers = 24`）、7（シーケンス）、8（DDL）、不変条件 I13（インデックスの全 TID 集合 = ヒープの索引されるべき版）、I14（木の構造検査）、I15（シーケンスは払い出した値を二度払い出さない）、I16（カタログの整合） | 06、07、08、11 |
| `yuzhu-core/src/storage/btree/check.rs` | 木の検査器。全テストとクラッシュ試験の後に走らせる | 06 |
| `tests/tools/difftest/` | 差分ランダムテスト（PostgreSQL と yuzhu の結果を比べる。独立した Cargo プロジェクト。`yuzhu-fuzz-sql` は作らない。C-5）。完了条件は固定シード 1〜32 と長時間 4 本（01 §3）、CI は固定シード、夜間は多数 | 11 |
| `tests/done-check.sh`、`tests/tools/slttools/`、`tests/gen/`、`tests/imported/`、`tests/isolation/specs`（M4 の 2 本） | 完了判定のローカルスクリプト、slt の lint・生成、plan_variants のテンプレート、PostgreSQL 回帰の取り込み、isolation の追加 | 11 |
| `yuzhu-core/tests/plan_golden/` | プランナのスナップショットテスト（SQL → 最適化前の論理プラン → 各ルールの後 → 物理プラン） | 04 |

---

## 19. M5 以降のための予約（M4 で入れない、壊さない）

- B+Tree: `cycleid`、`DELETED` `HALF_DEAD` `INCOMPLETE_SPLIT` のフラグ、`BTREE_*` の `0x20` 以降の info、ピボットの ALT_TID / posting のビット、`UniqueCheck` の `WaitFor` と親を探し直す処理（`_bt_getstackbuf` 相当）、`_bt_moveright` の「削除済みなら右へ」分岐。
- 実行: `IndexScan` の Index Only 化、`IN` のインデックス検索、マージ結合、スピル（`work_mem`）、Top-N、並列 INSERT の `heap_multi_insert` 相当（COPY）。
- 型: 配列、`interval` / `time`、`numeric` の `sqrt` `power` `ln` など、`to_char`。
- DDL: `ALTER TABLE` の残り、`CREATE VIEW`、FOREIGN KEY（M5）、`DEFERRABLE`。
- `SequenceStore` の `fetch` は M5 の複数ライターでも排他ラッチだけで安全（ライターロックを取らない設計のため変更不要）。

---

## 20. 変更の記録

M4 の実装中に契約を変えたら `spec/design/m4-changes.md` に理由とともに記録する（M1・M2・M3 と同じ運用）。
