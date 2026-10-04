# M4 調査: クエリ処理（JOIN・集約・サブクエリ・集合演算・CTE・ルールベース最適化・EXPLAIN）

M4（インデックスとクエリ）のうち、**クエリ処理の側**（B+Tree 本体以外）について、PostgreSQL 17（REL_17_STABLE）の挙動と実装を調べ、yuzhu で採用する範囲・データ構造・アルゴリズム・テスト方針を推奨する。B+Tree のページ形式・並行制御・PRIMARY KEY / UNIQUE / SERIAL は別の調査の担当とし、ここでは「プランナがインデックスをどう選ぶか」と「インデックススキャンの実行ノードがどう見えるか」だけを扱う。

- 前提として読んだもの: `CLAUDE.md`、`spec/design/m1.md`・`m1-changes.md`・`m2.md`、`spec/research/research-rust-db-arch.md`・`research-pg-types.md`・`research-slt.md`・`m2-*.md`・`m3-*.md`、現在の実装（`impl/rust/crates/yuzhu-core/src/{analyzer,planner,executor}`）。
- 表記: 【検証済み】は PostgreSQL 17.11（`postgres:17` コンテナ、C ロケール）で実際に SQL を流して確認したもの。【ソース確認】は REL_17_STABLE のソースで関数の存在と位置を確認したもの（行番号は 2026-10 時点の REL_17_STABLE）。【未検証】は記憶や推論によるもので、実装前に確かめる必要がある。
- 工数: **S** = 1〜2 日、**M** = 3〜5 日、**L** = 1〜2 週間（AI エージェント 1 体が集中して実装し、slt を通すまで）。

---

## 0. 要約（推奨）

1. **パイプラインを「Bound → 論理プラン → 最適化 → 物理プラン → Executor」に分ける**。M1 の「Bound から直接 PhysicalPlan」は JOIN の並べ替えや述語の押し下げに耐えない。論理プランの列は**クエリ内で一意な列 ID（`ColId`）**で参照し、行内のオフセットへの変換は物理化の最後に一度だけ行う（CockroachDB の optimizer、DataFusion の Column→PhysicalExpr と同じ考え方）。これが M4 最大の契約変更になる。
2. **M4 で入れる SQL**: JOIN 全種（INNER / LEFT / RIGHT / FULL / CROSS、ON / USING / NATURAL、カンマ結合）、FROM 句の副問い合わせ、集約（count / sum / avg / min / max / bool_and / bool_or / every、`DISTINCT` 付き、`FILTER`）、GROUP BY / HAVING（主キーへの関数従属を含む）、DISTINCT ON、サブクエリ（スカラー、IN / NOT IN / EXISTS / ANY / ALL、相関あり）、UNION / INTERSECT / EXCEPT（ALL 含む）、非再帰の WITH、`UPDATE ... FROM` / `DELETE ... USING`、EXPLAIN（テキスト形式、`COSTS OFF` が PG と一致）。
3. **後回し**: WITH RECURSIVE（M5）、LATERAL（M5〜M6）、ウィンドウ関数・GROUPING SETS / ROLLUP / CUBE（M6 以降）、ディスクへのスピル（外部ソート、Grace ハッシュ結合、ハッシュ集約のスピル。M6）、マージ結合（M6）、コストベース最適化（M6）。
4. **結合アルゴリズム**: 等値条件があればハッシュ結合（INNER / LEFT / RIGHT / FULL / SEMI / ANTI の全種）、なければネステッドループ（内側を Materialize）。インデックス付きネステッドループは M4 の後半に「内側の結合キーに一意インデックスがある」場合だけのルールとして入れる。
5. **メモリ**: M4 はスピルしない。ハッシュ表・ソート・集約の合計がクエリあたりの上限（新設定 `yuzhu.query_mem_limit`、既定 256MB を提案）を超えたら `53200 out_of_memory` で文をエラーにする。`work_mem` は SET / SHOW できるようにしておき、M6 のスピル導入時に意味を持たせる。
6. **集約の結果型は PostgreSQL と完全に一致させる**（`sum(int4)` = int8、`sum(int8)` = numeric、`avg(int*)` = numeric、`avg(float*)` = float8、`count` = int8）。**numeric は M5 の予定だが、最小限の numeric（演算と入出力）を M4 に前倒しすることを推奨**する。独立したモジュールなので並列化しやすい。間に合わない間は `sum(int8)` / `avg(整数)` を `0A000` にする（float8 で代用しない）。
7. **サブクエリ**: まず「素朴な再実行」で全種類を正しく動かし（相関パラメータ付きの SubPlan、非相関は InitPlan で 1 回だけ実行）、その上に PostgreSQL と同じ範囲の書き換え（WHERE の最上位 AND にある `EXISTS` / `IN` → セミ結合、`NOT EXISTS` → アンチ結合）を載せる。`NOT IN` は NULL の意味論のためアンチ結合にしない（PG も同じ）。一般的な非相関化（集約を含む相関サブクエリの展開）は M6 以降。
8. **最適化ルール**（固定順のパス）: 定数畳み込み → サブクエリの結合化 → 外部結合の内部結合化 → 述語の押し下げ → 結合キーの抽出 → 結合順序（構文順 + 直積を避ける貪欲法）→ 列の刈り込み → 物理化（インデックス選択、ハッシュ/NLJ の選択、ビルド側の選択）。統計は使わず、サイズの手がかりは**計画時点のページ数（`smgr::nblocks`）**だけにする（PG も `estimate_rel_size` で実ページ数を使っている）。
9. **EXPLAIN**: ノード名・詳細行（`Hash Cond:`、`Filter:`、`Sort Key:` など）・インデントを PG のテキスト形式と同じにする。コストの数値は合わない前提で、テストは `EXPLAIN (COSTS OFF)` を使い、しかもプランの選択が PG と一致する保証はないので **`onlyif yuzhu`** にする。式の逆変換（deparse）は `pg_get_expr` の正規形（M2-Q9）と共通の部品にする。
10. **テスト**: 結果を比べる slt は PG で検証する（`ORDER BY` か `rowsort` を必ず付ける）。PG と同じ名前の `enable_hashjoin` / `enable_nestloop` / `enable_indexscan` / `enable_seqscan` / `enable_hashagg` を実装し、**同じ問い合わせを設定を変えて流す slt** で各実行ノードを網羅する（PG にも同じ設定があるので、PG に対しても通る）。さらに、PG を正解とする**差分ランダムテスト**（SQLancer の TLP 方式）を M4 の後半に入れる。

---

## 1. 現状（M1/M2 の構造）と M4 で変わるもの

### 1.1 現在の構造（実装を読んで確認）

- `analyzer::BoundSelect { from: BoundFrom, filter, targets, columns, distinct: bool, order_by, limit, offset }`。`BoundFrom` は `None | Table | Values` の 1 つだけ。
- 列参照は `BoundExprKind::ColumnRef { index }`（**入力行の中の位置**）。スコープ（`analyzer/scope.rs`）は `rel: Option<ScopeRel>` で FROM 項目は最大 1 つ。
- `planner::PhysicalPlan` は `Result / Values / SeqScan / Filter / Project / Sort / Distinct / Limit / Insert`（M2 で `Update / Delete` と `SeqScan.system_columns` が増える）。式は `BoundExpr` をそのまま持つ。
- 式の評価は `eval(expr: &BoundExpr, row: &Row, ctx: &ExecCtx) -> Result<Datum>`（`&ExecCtx`、共有参照）。
- 小数リテラル（`1.5`）は `0A000 type numeric is not supported yet`（`analyzer/expr.rs`）。

### 1.2 M4 で必要になる契約の変更（まとめ）

| 変更 | 理由 | 影響範囲 |
|---|---|---|
| `BoundSelect.from` を FROM 項目の木（JOIN を含む）にし、`group_by` / `having` / `aggregates` / `distinct_on` を足す。問い合わせ全体を `BoundQuery { ctes, body: BoundSetExpr, order_by, limit, offset }` にする | JOIN・集合演算・CTE | analyzer、planner |
| 列参照を `ColumnRef { index }` から `ColumnRef { rte: RteId, col: u16, levels_up: u16 }` にする（PG の `Var { varno, varattno, varlevelsup }`） | 複数の FROM 項目と相関サブクエリ。入力行のオフセットは並べ替えで変わる | analyzer 全体、M2 の UPDATE / DELETE / CHECK |
| 新しい論理プラン（`planner/logical.rs`）と、物理プラン用の式 `PhysExpr`（列はオフセット、相関は `Param`、サブクエリは `SubPlan` 番号） | §4 | planner、executor |
| 式の評価に `&mut` の文脈を渡す（`eval(expr, row, &mut EvalCtx)`） | 相関サブクエリの評価中に子の Executor を動かす（`next(&mut ExecCtx)` が要る） | executor 全体 |
| `ExecCtx` にパラメータ領域（`params: Vec<Datum>`）と、メモリ予算（`mem: &MemBudget`）を足す | 相関パラメータ、メモリ上限 | executor |
| `Executor::rewind`（または「プランから作り直す」）を必須にする | NLJ の内側、相関サブクエリの再実行 | executor 全体 |

CHECK 制約や DEFAULT のように「1 つの表の行に対する式」は、`rte = 0` 固定の `ColumnRef` として今と同じ経路で評価できる（物理化で `col` がそのままオフセットになる）。

---

## 2. M4 の範囲の推奨

### 2.1 機能ごとの判断

| 機能 | 推奨 | 工数 | 根拠・備考 |
|---|---|---|---|
| INNER / CROSS / カンマ結合 | **M4** | M | 必須 |
| LEFT / RIGHT / FULL OUTER JOIN | **M4** | M | psql の `\dt` が LEFT JOIN を使う（m2-catalog §2.4）。RIGHT は左右を入れ替えて LEFT にする。FULL はハッシュ結合のみ（PG も等値条件がない FULL は `0A000`。§3.2） |
| ON / USING / NATURAL | **M4** | S | 名前解決の規則が細かい（§3.2） |
| FROM 句の副問い合わせ（派生表） | **M4** | S | 集約結果の結合などで多用。PG16 以降は別名を省略できる【検証済み: `select * from (select 1)` が通る】 |
| 集約関数（count / sum / avg / min / max / bool_and / bool_or / every） | **M4** | M | 結果型は §5 |
| `count(DISTINCT x)` など集約内の DISTINCT | **M4** | S | グループごとのハッシュ集合 |
| `FILTER (WHERE ...)` | **M4** | S | 評価時に条件を見るだけ |
| 集約内の ORDER BY（`string_agg(x, ',' ORDER BY x)`）、`string_agg`、`array_agg` | M5 | S〜M | `string_agg` 自体は簡単だが、順序付き集約はソートを要する。配列型が M5 |
| GROUP BY / HAVING | **M4** | M | 位置番号・式・別名、主キーへの関数従属（§3.4） |
| GROUPING SETS / ROLLUP / CUBE | M6 以降 | M | 利用は分析系に限られる |
| DISTINCT ON | **M4** | S | PG 独自だがアプリで使われる。Sort + Unique で実装できる |
| スカラーサブクエリ、EXISTS、IN / NOT IN、ANY / ALL | **M4** | M〜L | 素朴な再実行 + 一部の書き換え（§6.5） |
| 行値の IN（`(a, b) IN (SELECT ...)`） | **M4**（任意） | S | 行コンストラクタの比較が要る。後回しでもよい |
| UNION / INTERSECT / EXCEPT（ALL を含む） | **M4** | M | §3.6 |
| WITH（非再帰） | **M4** | S | 基本はインライン展開（§3.7） |
| `WITH ... MATERIALIZED` | **M4** | S | 1 回だけ実行して共有する CteScan |
| WITH RECURSIVE | M5 | M | WorkTable と RecursiveUnion。木構造の問い合わせで需要はある |
| データ変更を含む WITH（`WITH x AS (DELETE ... RETURNING)`） | M6 以降 | M | RETURNING が前提 |
| LATERAL | M5〜M6 | M | 相関サブクエリの仕組み（パラメータ付き再実行）を流用できる |
| ウィンドウ関数 | M6 以降 | L | 範囲外。構文だけ受け付けて `0A000` |
| `UPDATE ... FROM` / `DELETE ... USING` | **M4** | S | 結合の上に既存の Update / Delete を載せる。同じ行の 2 回目の更新は `SelfModified` として飛ばす（m2-dml-exec §5 の通り） |
| `generate_series(int4, int4)` / `generate_series(int8, int8)`（FROM 句の関数） | **M4**（推奨） | S | 大きなデータを使うテスト（ハッシュ結合のメモリ上限、多数のグループ）を短く書ける。PG と同じ |
| EXPLAIN（テキスト形式、`COSTS OFF`、`VERBOSE`） | **M4** | M | §8 |
| EXPLAIN ANALYZE | **M4**（任意） | S | 実行回数と行数だけ（時間は出すが比較しない） |
| EXPLAIN (FORMAT JSON / YAML / XML) | M6 以降 | S | ツール向け |
| インデックススキャン（等値・範囲） | **M4** | M | B+Tree の調査と連携。§7.3 |
| インデックス付きネステッドループ結合 | **M4 後半**（任意） | M | §7.4 |
| インデックス順を使ったソートの省略 | M5 | S | 正しさに影響しない最適化 |
| マージ結合 | M6 | M | ハッシュ結合で代替できる |
| ディスクへのスピル（外部ソート、Grace ハッシュ結合、ハッシュ集約のスピル） | M6 | L | M4 はメモリ上限で文をエラーにする（§7.5） |
| コストベース最適化、統計（ANALYZE、pg_statistic） | M6 | L | 要件定義の M6 |

### 2.2 推奨する M4 の内部順序

1. 契約の変更（§1.2）と論理プラン・物理プランの骨組み（**最初に終わらせる。並列作業の前提**）
2. 並列に進められるもの（§10 の分担）: JOIN、集約、サブクエリ、集合演算と CTE、EXPLAIN と deparse、最適化ルール、numeric の最小核、slt
3. インデックススキャン（B+Tree の完成待ち）
4. psql の `\dt`（LEFT JOIN・正規表現 `~`）、`\d tbl`（m2-catalog §2 の一覧。JOIN とサブクエリと `regclass` が要る）

---

## 3. アナライザ（名前解決と意味の検査）

### 3.1 範囲表（range table）とスコープ

PostgreSQL のアナライザは、FROM 句の項目をすべて範囲表 `rtable` に並べ（RTE。JOIN 自体も 1 つの RTE になる）、名前空間（`p_namespace`）に「どの RTE の列が、修飾なし・修飾ありで見えるか」を積む（【ソース確認】`transformFromClauseItem` parse_clause.c:1056、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/parser/parse_clause.c>）。yuzhu も同じ構造にする。

```rust
pub struct RteId(pub u16);

pub enum RteKind {
    Table { table: Arc<TableDef> },
    Subquery { query: Box<BoundQuery> },           // 派生表、インライン展開した CTE
    Values { rows: Vec<Vec<BoundExpr>> },
    Function { func: &'static BuiltinFunction, args: Vec<BoundExpr> }, // generate_series
    Join { kind: JoinKind, left: RteId, right: RteId,
           /// USING / NATURAL で併合した列。FULL JOIN では COALESCE(l, r)
           merged: Vec<MergedColumn> },
    CteRef { cte: CteId },                          // MATERIALIZED の CTE
}

pub struct Rte {
    pub kind: RteKind,
    pub refname: String,        // 別名、なければ表名。JOIN は名前なし（`AS j` があれば名前付き）
    pub columns: Vec<RteColumn>,// 名前と型
}
```

スコープは「名前空間の項目の列」と「親スコープへのリンク」を持つ。列名の解決は、内側のスコープから外側へ向かって探し、見つかった階層の差が `levels_up` になる（相関参照）。

### 3.2 JOIN の名前解決（PG の挙動）

| 項目 | PG の挙動 | 根拠 |
|---|---|---|
| `SELECT *` の列順 | 結合の左 → 右の順。USING / NATURAL の併合列は**先頭に 1 回だけ**、その後に左の残り、右の残り | 【検証済み】`select * from t join u using (a)` → `a, b, e, c` |
| FULL JOIN の USING 列 | 併合列は `COALESCE(t.a, u.a)`。`t.a` / `u.a` と修飾すれば元の列（NULL になりうる） | 【検証済み】`select a, t.a, u.a from t full join u using (a)` で `3, NULL, 3` |
| 修飾なしの曖昧な列 | `42702 column reference "a" is ambiguous`（位置付き） | 【検証済み】 |
| USING の列が片側にない | `42703 column "zz" specified in USING clause does not exist in left table`（右なら `right table`） | 【検証済み】 |
| USING の列名が片側に 2 つ以上 | `42702 common column name "a" appears more than once in left table` | 【検証済み】 |
| NATURAL で共通列がない | 直積になる（エラーではない） | 【検証済み】`t natural join p` が 6 行 |
| 同じ参照名が 2 回 | `42712 table name "u" specified more than once`（別名の重複も同じ） | 【検証済み】 |
| ON の型 | bool でなければ `42804 argument of JOIN/ON must be type boolean, not type integer` | 【検証済み】 |
| ON の中の集約 | `42803 aggregate functions are not allowed in JOIN conditions` | 【検証済み】 |
| ON から見える表 | その JOIN の左右にある表だけ（後ろのカンマ結合の表は見えない）。`t join u on t.a = p.id, p` → `42P01 missing FROM-clause entry for table "p"`【検証済み】。名前空間にはあるが参照できない位置の場合は `invalid reference to FROM-clause entry for table "x"`（同じ 42P01）【未検証: 発生条件】 | parse_clause.c |
| FULL JOIN の条件 | 等値（ハッシュかマージが可能な条件）でなければ `0A000 FULL JOIN is only supported with merge-joinable or hash-joinable join conditions` | 【検証済み】`t full join u on t.a < u.a` |
| USING の型 | 左右の列の共通型（`select_common_type`）で比較する。等値演算子が見つからなければ `42883` | 【検証済み】`int2` と `text` で `42883 operator does not exist: smallint = text` |
| 存在しない修飾名 | `42P01 missing FROM-clause entry for table "t"` | 【検証済み】集合演算の ORDER BY で `t.a` を書いたとき |

USING の型付けの詳細は `transformJoinUsingClause`（parse_clause.c:308）と `buildMergedJoinVar`（同ファイル）【ソース確認: 前者のみ】。

### 3.3 集約の検査

PostgreSQL は `parseCheckAggregates`（【ソース確認】parse_agg.c:1131、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/parser/parse_agg.c>）で、集約やグループ化がある問い合わせについて、ターゲット・HAVING・ORDER BY の中の列参照が GROUP BY に含まれるか、集約の引数の中にあるかを `check_ungrouped_columns`（同 1328）で検査する。

| 項目 | PG の挙動【検証済み】 |
|---|---|
| グループ化されていない列 | `42803 column "t.a" must appear in the GROUP BY clause or be used in an aggregate function`（位置付き） |
| 集約の入れ子 | `42803 aggregate function calls cannot be nested` |
| WHERE の中の集約 | `42803 aggregate functions are not allowed in WHERE` |
| GROUP BY の位置番号が範囲外 | `42P10 GROUP BY position 5 is not in select list` |
| GROUP BY のない HAVING | 全体を 1 グループとして扱う（`select count(*) from t having count(*) > 1` → 1 行） |
| 空の入力に対する集約（GROUP BY なし） | 1 行を返す。`count` は 0、ほかは NULL |
| 空の入力に対する集約（GROUP BY あり） | 0 行 |
| `select 1 having false` | 0 行（集約なしでも HAVING があれば集約問い合わせになる） |
| `GROUP BY ()` | 全体を 1 グループ |
| 主キーへの関数従属 | `GROUP BY id`（`id` が主キー）なら同じ表の他の列を参照できる（`select id, v from p group by id` が通る） |

関数従属は PG 9.1 からの機能で、ORM（Rails など）の生成する SQL に現れる。主キーは M4 で入るので、**M4 で対応する**（S）。判定は「GROUP BY が、ある表の主キーの全列を単純な列参照として含む」場合だけでよい（PG も主キーのみ。UNIQUE 制約は対象外【未検証: UNIQUE + NOT NULL でも不可であること】）。PG はこの依存関係を `pg_depend` に記録して主キーの DROP を防ぐが、yuzhu は M4 では記録しない（確認事項 M4Q-7）。

### 3.4 GROUP BY と ORDER BY の名前の解決順（PG の癖）

**GROUP BY は入力列を優先し、ORDER BY は出力列の別名を優先する**。PG の `findTargetlistEntrySQL92`（【ソース確認】parse_clause.c:2006）のコメントの通りで、GROUP BY の単純な名前は「FROM の列として解決できればそれ、できなければ出力列の別名」になる。

- 【検証済み】`select a as b, count(*) from t group by b`（`t` に列 `b` がある）→ GROUP BY の `b` は `t.b` に解決され、`a` がグループ化されていないので `42803`。
- 【検証済み】`select a+0 as zz, count(*) from t group by zz` → 別名 `zz` に解決されて通る。

M1 で実装済みの ORDER BY の解決（出力列の別名が先）とは逆なので、共通化するときに取り違えないこと。

### 3.5 DISTINCT と DISTINCT ON

- `SELECT DISTINCT` で ORDER BY の式が出力列にない → `42P10 for SELECT DISTINCT, ORDER BY expressions must appear in select list`【検証済み】（M1 で実装済み）。
- `DISTINCT ON (exprs)` の式は ORDER BY の先頭と一致しなければならない → `42P10 SELECT DISTINCT ON expressions must match initial ORDER BY expressions`【検証済み】。ORDER BY がなければ任意の 1 行（PG では実装依存の行）。`transformDistinctOnClause`（【ソース確認】parse_clause.c:3069）。
- 実装: ORDER BY（DISTINCT ON の式を先頭に）で Sort → 先頭キーが変わった最初の行だけを通す `Unique { prefix_len }`。ORDER BY がない DISTINCT ON はテストで結果が不定になるので、slt では必ず ORDER BY を付ける。

### 3.6 集合演算（UNION / INTERSECT / EXCEPT）

PG の `transformSetOperationTree`（【ソース確認】analyze.c:2003、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/parser/analyze.c>）の規則:

- 優先順位: `INTERSECT` が `UNION` / `EXCEPT` より強い。同順位は左結合。括弧で囲んだ副問い合わせは ORDER BY / LIMIT を持てる。
- 列数が違う → `42601 each UNION query must have the same number of columns`（INTERSECT / EXCEPT も同様の文言）【検証済み】。
- 各列の型は左右の `select_common_type` で決まる（research-pg-types §3.4）。**左から順に 2 つずつ**解決する点に注意。`select 1 union select 'a'` は unknown が int4 に決まり `22P02 invalid input syntax for type integer: "a"`【検証済み】。`select 1 union select 2::bigint` は bigint【検証済み】。
- 出力列の名前は最初の枝のもの。
- 集合演算全体に付いた ORDER BY は、出力列の名前・位置番号しか参照できない（`order by t.a` → `42P01 missing FROM-clause entry for table "t"`）【検証済み】。式も不可（`ORDER BY a+1` → `0A000 invalid UNION/INTERSECT/EXCEPT ORDER BY clause`【検証済み】）。
- 括弧なしの `... LIMIT 1 UNION ...` は構文エラー（`42601 syntax error at or near "union"`）【検証済み】。

実装（物理）:

| 演算 | 物理プラン | 意味 |
|---|---|---|
| UNION ALL | `Append` | 連結 |
| UNION | `Append` → `HashAggregate`（全列をキー、集約なし）= `Distinct` | 重複除去（NULL 同士は等しい） |
| INTERSECT [ALL] / EXCEPT [ALL] | `HashSetOp { op, all }`：各行に「左/右」の印を付けて Append し、行ごとに左の個数 `nl` と右の個数 `nr` を数える | INTERSECT: `nl>0 && nr>0` なら 1 行、ALL なら `min(nl,nr)` 行。EXCEPT: `nl>0 && nr==0` なら 1 行、ALL なら `max(nl-nr,0)` 行 |

PG の `nodeSetOp.c` と同じ数え方（<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeSetOp.c>）【ソース確認: ファイルのみ】。出力の順序は不定なので slt は ORDER BY か rowsort にする。

### 3.7 WITH（CTE）

- PG12 以降、非再帰の CTE は「参照が 1 回」「`MATERIALIZED` でない」「副作用がない（SELECT のみ、volatile 関数なし）」ならインライン展開され、それ以外は 1 回だけ実行して CTE Scan で共有する（【ソース確認】`SS_process_ctes` subselect.c:884、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/plan/subselect.c>。docs: <https://www.postgresql.org/docs/17/queries-with.html#QUERIES-WITH-CTE-MATERIALIZATION>）。
  - 【検証済み】`with x as (select * from t) select * from x where a = 1` は `Seq Scan on t / Filter: (a = 1)`（インライン）。`MATERIALIZED` を付けると `CTE Scan on x / Filter / CTE x -> Seq Scan on t`。
- yuzhu の推奨: **`MATERIALIZED` 以外はすべて（参照が 2 回以上でも）インライン展開する**。M4 の時点で volatile 関数（`random()` など）がなければ、結果は同じになる。EXPLAIN の形だけが PG と違う（参照 2 回以上で PG は CTE Scan）。`MATERIALIZED` は `CteScan`（最初の参照で全行をメモリに溜め、各参照は自分のカーソルで読む）。
- 名前の重複 → `42712 WITH query name "x" specified more than once`【検証済み】。後の CTE は前の CTE を参照できる【検証済み】。同じ CTE を 2 回参照できる（`from x a, x b`）【検証済み】。
- `WITH RECURSIVE` は M5（構文は受け付けて `0A000`）。

### 3.8 サブクエリ（アナライザ側）

| 項目 | PG の挙動【検証済み】 |
|---|---|
| スカラーサブクエリが 2 行以上 | 実行時に `21000 more than one row returned by a subquery used as an expression` |
| スカラーサブクエリが 0 行 | NULL |
| スカラーサブクエリの列が 2 つ以上 | `42601 subquery must return only one column`（位置付き） |
| `IN (SELECT ...)` の列数が合わない | `42601 subquery has too many columns`（少ない場合は `42601 subquery has too few columns`【検証済み】） |
| `EXISTS (SELECT)`（列なし） | 通る |
| `x IN (SELECT ...)` の NULL | 一致がなく、サブクエリに NULL があれば NULL。`NULL IN (空でない集合)` は NULL |
| `x NOT IN (空集合)` | true |
| 相関参照 | 外側の列を `levels_up >= 1` の ColumnRef として解決（`(select e)` のような外側の列だけの式も可） |
| LIMIT の中のサブクエリ | 許される（`limit (select 1)`） |

`IN` / `ANY` / `ALL` は「`x op ANY (subquery)`」に正規化する（`IN` = `= ANY`、`NOT IN` = `NOT (= ANY)`、`<> ALL` = `NOT IN`）。演算子は `x` とサブクエリの列の型で通常の演算子解決（research-pg-types §3）をする。

---

## 4. 論理プランと物理プラン（データ構造）

### 4.1 方針

research-rust-db-arch.md §2 の推奨（AST → Bound → 論理プラン → 物理プラン → Executor）を M4 で実現する。比較:

| 案 | 内容 | 長所 | 短所 |
|---|---|---|---|
| A: 論理と物理を分けない（toydb、BusTub） | 1 種類の Plan を書き換える | 少ないコード | 並べ替えや押し下げのたびに行のオフセットを付け直す必要があり、バグの温床 |
| B: 論理（列 ID で参照）と物理（オフセットで参照）を分ける（DataFusion、CockroachDB） | 最適化は論理プランで行い、最後に物理化 | 書き換えが列の位置に依存しない。EXPLAIN も物理から作れる | 2 種類の enum と変換が要る（+1〜2 日） |
| C: PG 方式（Query → Path 候補 → Plan） | Path を列挙してコストで選ぶ | 将来のコストベースに直結 | M4 には過剰 |

**推奨は B**。M6 でコストベースにするときは、論理プランから複数の物理候補を作る段を足せばよい（C に近づける）。

### 4.2 列 ID（`ColId`）

```rust
/// 1 つの問い合わせの中で一意な列の識別子。論理プランの全ノードの出力列に付く
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ColId(pub u32);

pub struct ColumnMeta { pub id: ColId, pub name: String, pub ty: SqlType, pub nullable: bool }

/// 列 ID の発行と、各列の型・名前（EXPLAIN 用に "t.a" の表示名も）を持つ
pub struct ColumnArena { cols: Vec<ColumnInfo> }
```

- ベーステーブルの走査は、表の各列に新しい `ColId` を発行する（同じ表を 2 回参照すれば別の ID）。
- `Project` の計算列、集約の結果、集合演算の出力、外部結合で NULL になりうる側の列も新しい ID を持つ。
- 論理プランの式 `LExpr` は `Column(ColId)` で列を参照する。`ColumnRef { rte, col, levels_up }` から `ColId` への対応は Bound → 論理の変換（builder）で引く。
- 物理化では、各ノードの子の出力が `Vec<ColId>`（列の並び）なので、`ColId → オフセット` の表を作って `PhysExpr::Column(usize)` に変換する。

### 4.3 論理プラン

```rust
pub enum LogicalPlan {
    Get { rel: RelHandle, table: Arc<TableDef>, cols: Vec<ColId>, system_columns: Vec<(SystemColumn, ColId)> },
    Values { rows: Vec<Vec<LExpr>>, cols: Vec<ColId> },
    TableFunction { func: &'static BuiltinFunction, args: Vec<LExpr>, cols: Vec<ColId> }, // generate_series
    Filter { input: Box<LogicalPlan>, predicate: LExpr },
    /// 出力 = exprs の並び（列の刈り込みもここで表す）
    Project { input: Box<LogicalPlan>, exprs: Vec<(ColId, LExpr)> },
    Join { kind: JoinKind, left: Box<LogicalPlan>, right: Box<LogicalPlan>, on: LExpr },
    Aggregate { input: Box<LogicalPlan>, group_by: Vec<(ColId, LExpr)>, aggs: Vec<(ColId, AggCall)> },
    Distinct { input: Box<LogicalPlan>, on: Option<Vec<ColId>> },   // None = 全列
    Sort { input: Box<LogicalPlan>, keys: Vec<LSortKey> },
    Limit { input: Box<LogicalPlan>, limit: Option<LExpr>, offset: Option<LExpr> },
    SetOp { op: SetOpKind, all: bool, left: Box<LogicalPlan>, right: Box<LogicalPlan>,
            cols: Vec<ColId>, left_cols: Vec<ColId>, right_cols: Vec<ColId> },
    CteScan { cte: CteId, cols: Vec<ColId> },
    /// 1 行だけ出す（FROM なし）。one_time_filter が false なら 0 行
    Result { one_time_filter: Option<LExpr> },
    Empty { cols: Vec<ColId> },                                      // 定数畳み込みで偽になった部分
    Insert { .. }, Update { .. }, Delete { .. },
}

pub enum JoinKind { Inner, Left, Full, Semi, Anti }   // RIGHT は builder で左右を入れ替えて Left に
pub enum SetOpKind { Union, Intersect, Except }

pub enum LExpr {
    Column(ColId),
    Literal(Datum, SqlType),
    /// 相関参照（外側の問い合わせの列）。サブクエリの中だけに現れる
    OuterColumn(ColId),
    Operator { op: &'static BuiltinOperator, args: Vec<LExpr> },
    Function { .. }, Cast { .. }, CoerceTypmod { .. }, And(..), Or(..), Not(..), /* M1 の式と同じ種類 */
    /// サブクエリ式。kind = Scalar | Exists | Any { op, lhs } | All { op, lhs }
    Subquery { kind: SubqueryKind, plan: Box<LogicalPlan>, correlated: Vec<ColId> },
}

pub struct AggCall {
    pub func: &'static BuiltinAggregate,  // pg_aggregate の行に相当（§5）
    pub args: Vec<LExpr>,                 // count(*) は空
    pub distinct: bool,
    pub filter: Option<LExpr>,
}
```

- RIGHT JOIN は、builder で `Left` にして左右を入れ替える（出力列の順序は列 ID で持っているので、最後の Project で元の順序に戻るだけ）。EXPLAIN の表示が PG と違ってもよい（PG は `Hash Right Join` などを出す【検証済み】。PG はハッシュ結合でビルド側を選ぶ都合で Right を使う）。
- SEMI / ANTI はユーザーが直接書けない。サブクエリの書き換え（§6.5）だけが作る。

### 4.4 物理プラン

```rust
pub enum PhysicalPlan {
    // 既存: Result（one_time_filter を追加）, Values, SeqScan, Filter, Project, Sort, Limit, Insert, Update, Delete
    IndexScan { rel: RelHandle, index: IndexHandle, bounds: ScanBounds, direction: ScanDirection,
                columns: Vec<SqlType>, system_columns: Vec<SystemColumn>, filter: Option<PhysExpr> },
    FunctionScan { func: &'static BuiltinFunction, args: Vec<PhysExpr> },
    NestedLoopJoin { kind: JoinKind, outer: Box<PhysicalPlan>, inner: Box<PhysicalPlan>, join_filter: Option<PhysExpr> },
    /// 内側をパラメータ付きで再実行（インデックス付き NLJ、LATERAL）
    NestedLoopParam { kind: JoinKind, outer: Box<PhysicalPlan>, inner: Box<PhysicalPlan>,
                      params: Vec<(ParamId, PhysExpr)>, join_filter: Option<PhysExpr> },
    HashJoin { kind: JoinKind, probe: Box<PhysicalPlan>, build: Box<PhysicalPlan>,
               probe_keys: Vec<PhysExpr>, build_keys: Vec<PhysExpr>, key_types: Vec<SqlType>,
               residual: Option<PhysExpr>, build_is_left: bool },
    Materialize { input: Box<PhysicalPlan> },
    /// GROUP BY なしの集約（必ず 1 行）
    Aggregate { input: Box<PhysicalPlan>, aggs: Vec<PhysAgg>, having: Option<PhysExpr> },
    HashAggregate { input: Box<PhysicalPlan>, keys: Vec<PhysExpr>, aggs: Vec<PhysAgg>, having: Option<PhysExpr> },
    /// 並べ替え済みの入力から、先頭 prefix_len 列が変わった最初の行だけ通す（DISTINCT ON）
    Unique { input: Box<PhysicalPlan>, prefix_len: usize },
    Append { inputs: Vec<PhysicalPlan> },
    HashSetOp { op: SetOpKind, all: bool, left: Box<PhysicalPlan>, right: Box<PhysicalPlan> },
    CteScan { cte: CteId },
    /// EXPLAIN 用に名前を保つだけの透過ノード（派生表 "Subquery Scan on s"）。実行時は子をそのまま返す
    SubqueryScan { alias: String, input: Box<PhysicalPlan>, filter: Option<PhysExpr> },
}

pub struct PhysicalQuery {
    pub root: PhysicalPlan,
    pub init_plans: Vec<InitPlan>,   // 非相関のスカラー/EXISTS サブクエリ。最初に評価されたとき 1 回だけ実行
    pub sub_plans: Vec<SubPlanDef>,  // 相関サブクエリ（行ごとに再実行）、ハッシュ化 SubPlan
    pub ctes: Vec<PhysicalPlan>,     // MATERIALIZED の CTE
    pub n_params: usize,
}
```

- 「Plan は不変、Executor は状態を持つ」（research-rust-db-arch §3.3 の 4）を守る。prepared statement（M5）は `PhysicalQuery` を保持して実行のたびに `build` する。
- `PhysExpr` は `BoundExpr` と同じ種類の式に、`Column(usize)`、`Param(ParamId)`、`SubPlan(SubPlanId)`、`InitPlan(InitPlanId)` を足したもの。M1 の `BoundExpr` を評価している `eval.rs` を `PhysExpr` 用に移す（機械的な変更）。

---

## 5. 集約関数と結果型

### 5.1 PostgreSQL の定義（【検証済み】PG17.11 の `pg_aggregate` / `pg_proc` を問い合わせた結果）

| 集約 | pg_proc OID | 結果型 | 状態の型 | 備考 |
|---|---|---|---|---|
| `count(*)` | 2803 | int8 | int8 | `int8inc` |
| `count("any")` | 2147 | int8 | int8 | NULL は数えない |
| `sum(int2)` | 2109 | **int8** | int8 | |
| `sum(int4)` | 2108 | **int8** | int8 | `int4_sum`。int8 を超えることは事実上ない |
| `sum(int8)` | 2107 | **numeric** | internal | `int8_avg_accum` → `numeric_poly_sum`。`9223372036854775807 + 1` も正しく `9223372036854775808` になる【検証済み】 |
| `sum(float4)` | 2110 | float4 | float4 | |
| `sum(float8)` | 2111 | float8 | float8 | |
| `sum(numeric)` | 2114 | numeric | internal | |
| `avg(int2)` | 2102 | **numeric** | int8[] | `int2_avg_accum` → `int8_avg` |
| `avg(int4)` | 2101 | **numeric** | int8[] `{count,sum}` | `int4_avg_accum` → `int8_avg` |
| `avg(int8)` | 2100 | **numeric** | internal | `numeric_poly_avg` |
| `avg(float4)` | 2104 | **float8** | float8[] | |
| `avg(float8)` | 2105 | float8 | float8[] | |
| `avg(numeric)` | 2103 | numeric | internal | |
| `min/max(int2/int4/int8/float4/float8/text/numeric)` | 2133/2132/2131/2135/2136/2145/2146（min）、2117/2116/2115/2119/2120/2129/2130（max） | 引数と同じ | 同じ | varchar は text の版に解決される（バイナリ互換） |
| `bool_and` / `bool_or` / `every` | 2517 / 2518 / 2519 | bool | bool | |
| `stddev` / `variance`（整数） | — | numeric | — | M5 以降 |

出典: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/catalog/pg_aggregate.dat>【ソース確認】、docs <https://www.postgresql.org/docs/17/functions-aggregate.html>。

`avg` の表示例【検証済み】:

| 問い合わせ | 結果 |
|---|---|
| `avg` of int4 `{1, 2}` | `1.5000000000000000` |
| `avg` of int4 `{1, 2, 4}` | `2.3333333333333333` |
| `avg` of int4 `{1, 1, 1}` | `1.00000000000000000000` |
| `avg` of int8 `{10, 20}` | `15.0000000000000000` |
| `avg` of float8 `{1.5, 2.5}` | `2` |

小数点以下の桁数は numeric の除算 `numeric_div` の `select_div_scale`（【ソース確認】numeric.c:9831、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/numeric.c>）で決まり、「有効桁数を最低 16 桁（`NUMERIC_MIN_SIG_DIGITS`）確保する」規則なので、商の大きさによって桁数が変わる（`1.000...`（20 桁）と `1.5000000000000000`（16 桁）の違い）。**float8 での近似や「常に 16 桁」では一致しない**。

### 5.2 numeric をどうするか（判断）

`sum(int8)`・`avg(整数)`・小数リテラル（`1.5`。M1 では `0A000`）のすべてが numeric に依存する。

| 案 | 内容 | 工数 | 評価 |
|---|---|---|---|
| A: numeric の最小核を M4 に前倒し | `Datum::Numeric`（PG の `NumericVar` と同じ base-10000 の桁配列 + weight + sign + dscale、NaN / ±Infinity）、加減乗除・剰余・比較・ハッシュ、テキスト入出力、int/float との相互キャスト、小数リテラル、`round` | **M〜L**（5〜8 日） | **推奨**。PG の numeric.c のアルゴリズムをそのまま移植すれば、表示も含めて一致させられる。他のモジュールに依存しない独立した部品なので、並列の担当 1 つに切り出せる。`numeric(p,s)` の typmod とディスク形式（varlena）も同時に決める必要がある（M2 の列の符号化に追加） |
| B: M5 まで `0A000` | `sum(int8)` / `avg(整数)` / `avg(int8)` を `0A000 type numeric is not supported yet` にする | S | A が間に合わないときの暫定。テストは `avg(x::float8)` や `sum(x)::int8`（不可。sum が numeric なので）を避けて書く必要があり、アプリ互換性が下がる |
| C: float8 で代用 | 結果型を float8 にする | S | **不可**。RowDescription の型 OID・表示（`1.5` と `1.5000000000000000`）がともに PG と違い、黙って間違う |

推奨: **A を M4 の並列トラックとして実施し、それまでの間は B**。M5 の「型の追加」に残るのは `numeric(p,s)` の細部（`numeric_in` の typmod 適用、`trunc`、`power`、`sqrt`、`ln` などの関数群）になる。

### 5.3 集約の実装

- 状態は集約ごとの enum（`AggState::Count(i64) | SumInt(i64) | SumNumeric(Numeric) | AvgInt { count: i64, sum: i128 } | SumFloat8(f64) | AvgFloat8 { n, sum, sumsq? } | MinMax(Option<Datum>) | BoolAnd(Option<bool>) ...`）。PG のトランジション関数と同じ意味（strict なトランジション関数は NULL 入力を飛ばす。初期値 NULL の min/max は最初の非 NULL 値で初期化）。
- `sum(int8)` / `avg(int8)` は PG と同じく 128 ビット整数（`i128`）で足してから numeric にする（`numeric_poly_*`。PG は `HAVE_INT128` のとき int128 を使う【未検証: 17 での条件】）。
- `sum(float8)` は足す順序で結果が変わる。PG と同じ値を保証できないので、slt では誤差の出ない値（整数値や 2 進で正確な小数）だけを使う。`avg(float8)` は PG の `float8_accum` が `N, Sx, Sxx` を Youngs-Cramer 法で更新する【未検証: 詳細】。M4 では単純な和でよいが、誤差の出るテストは置かない。
- オーバーフロー: `sum(int4)` は int8 の状態で足すので実質起きない。`int8` の範囲を超えたら `22003 bigint out of range`。
- 集約内の DISTINCT: グループごとに `HashSet<Vec<Datum>>` を持ち、初めて見た値だけをトランジションに渡す。
- `FILTER (WHERE cond)`: cond が true の行だけトランジションに渡す。
- 結果の行順: HashAggregate の出力は不定（【検証済み】PG でも `NULL, 2, 1` のような順になる）。

---

## 6. 実行器

### 6.1 Executor トレイトの変更

```rust
pub trait Executor {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>>;
    /// 先頭に戻す。params が変わった後に呼ばれることがある（相関サブクエリ、NestedLoopParam）
    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()>;
}

pub struct ExecCtx<'a> {
    // M2/M3 の項目（catalog, storage, txn, snapshot, session, interrupts）
    pub params: &'a mut [Datum],        // 相関パラメータ、NestedLoopParam のパラメータ
    pub mem: &'a MemBudget,             // §6.6
    pub subplans: &'a mut SubPlanStates,// SubPlan / InitPlan の Executor と結果のキャッシュ
}
```

- 式の評価は `eval(expr: &PhysExpr, row: &Row, ctx: &mut ExecCtx) -> Result<Datum>` にする。`SubPlan` を評価するとき、`ctx.subplans` から該当する Executor を取り出し（`Option::take` で一時的に所有権を移す。`unsafe` 不要）、パラメータを `ctx.params` に設定して `rewind` → `next` を回し、終わったら戻す。
- `rewind` の既定実装は作らず、全ノードで実装する。走査ノードは「カーソルを先頭に」、Sort / HashAggregate / Materialize / HashJoin のビルド側は「溜めた結果を読み直す」（パラメータに依存しない場合）か「作り直す」（依存する場合）。どちらにするかはプラン作成時に `depends_on_params` で決める（PG の `chgParam` と同じ発想）。

### 6.2 ネステッドループ結合

- 外側の 1 行ごとに内側を `rewind` して最後まで読む。内側が SeqScan なら毎回ヒープを読み直すことになるので、PG と同様に内側に `Materialize`（メモリ上に溜める）を置く（【検証済み】PG も `Nested Loop / -> Materialize`）。内側が Values や小さな Materialize 済みのものなら不要。
- 種類ごとの処理:
  - INNER: `join_filter` が true の組を出す。
  - LEFT: 外側の行に一致が 1 つもなければ、内側の列を NULL にして出す。
  - SEMI: 最初の一致で外側の行を出して次へ。ANTI: 一致がなければ出す。
  - FULL: ネステッドループでは対応しない（PG も等値条件のない FULL はエラー。§3.2）。
- MVCC: 内側の再走査は同じスナップショット・同じコマンド ID で行うので、文の途中で見える行が変わることはない（m2-dml-exec の Halloween 対策と同じ仕組み）。

### 6.3 ハッシュ結合

- **ビルド側**を全部読んでハッシュ表 `HashMap<HashKey, Vec<Row>>`（M4 は Rust の標準 HashMap でよい。ハッシュ関数は自前の型別関数で `HashKey` を作る）を作り、**プローブ側**を 1 行ずつ流す。
- キーの型: 左右の結合キーを**共通の型に揃えてから**ハッシュする（`int4 = int8` なら int4 側を int8 にキャストした式をキーにする）。PG はハッシュ演算子族の中で異なる型同士でもハッシュ値が一致するよう作っている（`hashint4` と `hashint8` の互換）が、yuzhu は「共通型に揃える」方が単純で間違いにくい。等値演算子が `oprcanhash` を持つ組み合わせだけをハッシュ結合の対象にする（research-pg-types §3）。
- 正規化: float の `-0` と `+0`、NaN 同士を等しくハッシュする（`cmp_datum` と一致させる）。text は C 照合なのでバイト列そのまま。
- NULL のキーは決して一致しない（ビルド時に表へ入れない。ただし RIGHT/FULL のために「一致しなかったビルド行」としては残す）。
- 種類ごと（ビルド = 右、プローブ = 左の場合）:

| 種類 | プローブ行に一致あり | プローブ行に一致なし | プローブ終了後 |
|---|---|---|---|
| INNER | 組を出す | 何もしない | — |
| LEFT | 組を出す | 右を NULL で出す | — |
| RIGHT（ビルド側が保存側） | 組を出し、ビルド行に「一致した」印 | 何もしない | 印のないビルド行を、左を NULL で出す |
| FULL | 組を出し印を付ける | 右を NULL で出す | 印のないビルド行を出す |
| SEMI | 最初の一致で 1 回だけ出す | 何もしない | — |
| ANTI | 何もしない | 出す | — |

- `residual`（等値以外の残りの条件。`t.a = u.a AND t.b < u.b` の後半）は、キーが一致した組に対して評価し、true のときだけ「一致」とみなす（外部結合の意味として正しい）。
- ビルド側の選択: 論理的に LEFT JOIN なら右をビルド（LEFT のまま）か左をビルド（RIGHT 型の処理）のどちらでもよい。M4 は「**推定サイズ（ページ数）が小さい方をビルド**」にする（§7.2）。SEMI / ANTI はサブクエリ側をビルドに固定する。
- 空のビルド側の最適化（INNER / SEMI なら即終了）は PG にもある。M4 は任意。

### 6.4 ハッシュ集約・集合演算

- `HashAggregate`: `HashMap<Vec<Datum>(キー), Vec<AggState>>`。入力を読み切ってから出力する。NULL のキー同士は同じグループ（GROUP BY の意味）。
- `Aggregate`（GROUP BY なし）: 状態 1 組。入力が空でも 1 行出す。
- `HashSetOp`: §3.6 の数え方。
- HAVING は集約結果の行に対するフィルタとして同じノードで評価する（PG の EXPLAIN でも `HashAggregate` の `Filter:` として出る【検証済み】）。

### 6.5 サブクエリの実行と書き換え

**素朴な実行（全種類の土台）**

| 種類 | 物理表現 | 実行 |
|---|---|---|
| 非相関スカラー | InitPlan | 最初に必要になったとき 1 回だけ実行して値をキャッシュ。2 行以上なら `21000` |
| 非相関 EXISTS | InitPlan | 1 行目が来たら true（LIMIT 1 相当） |
| 非相関 `x op ANY (sub)` | ハッシュ化 SubPlan（op が等値でハッシュ可能な場合）または SubPlan | ハッシュ化: サブクエリの結果を 1 回だけ読んでハッシュ集合にし、NULL を含んだかを記録。各行で `x` を引く。一致 → true、不一致で集合に NULL あり or `x` が NULL → NULL、それ以外 false（PG の `ExecHashSubPlan`【ソース確認】nodeSubplan.c:101） |
| 相関あり（全種） | SubPlan | 外側の行ごとにパラメータを設定して `rewind` → 実行（PG の `ExecScanSubPlan`【ソース確認】nodeSubplan.c:204） |

**書き換え（PG と同じ範囲に限る）**

PG は `pull_up_sublinks`（【ソース確認】prepjointree.c:459、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/prep/prepjointree.c>）で、**WHERE（または JOIN/ON）の最上位の AND 項**にある次のものを結合に変える:

- `x IN (SELECT ...)`（= `= ANY`）で、サブクエリが外側を参照しない → セミ結合（`convert_ANY_sublink_to_join`【ソース確認】subselect.c:1258）
- `EXISTS (SELECT ... WHERE 相関条件)` → セミ結合、`NOT EXISTS` → アンチ結合（`convert_EXISTS_sublink_to_join` 同 1375）。サブクエリの WHERE の相関条件が結合条件になる。
- 【検証済み】`where a in (select a from u)` と `where exists (select 1 from u where u.a = t.a)` は `Hash Semi Join`、`not exists` は `Hash Anti Join`。
- `NOT IN` は変換しない（サブクエリに NULL があると結果が NULL になるため、アンチ結合と意味が違う）。【検証済み】PG は `Filter: (NOT (ANY (a = (hashed SubPlan 1).col1)))`。
- 相関するスカラーサブクエリ（`(select count(*) from u where u.a = t.a)`）は変換せず SubPlan として行ごとに実行する【検証済み】。

yuzhu も同じ規則にする。セミ結合のビルド側はサブクエリ側。重複があっても外側の行は 1 回しか出ない（SEMI の意味）。

**一般的な非相関化**（集約を含む相関サブクエリを GROUP BY + 外部結合に書き換える。Neumann & Kemper "Unnesting Arbitrary Queries", BTW 2015）は M6 以降。M4 の素朴な再実行は O(外側の行数 × 内側のコスト) だが、内側にインデックスがあれば実用上は許容できる。

### 6.6 メモリ上限（スピルは後回し）

- PG の既定は `work_mem = 4MB`、`hash_mem_multiplier = 2`（ハッシュ系は 8MB）【検証済み: SHOW】。超えると、ハッシュ結合はバッチ分割（`ExecChooseHashTableSize`【ソース確認】nodeHash.c:675）、ハッシュ集約は PG13 以降スピル（`hash_agg_enter_spill_mode`【ソース確認】nodeAgg.c:1882）、ソートは外部ソートに切り替わり、**エラーにはならない**。
- yuzhu M4 の推奨: スピルせず、**クエリ単位のメモリ予算**（`MemBudget`。Sort・HashJoin・HashAggregate・Materialize・HashSetOp・CteScan・ハッシュ化 SubPlan が溜める行の概算バイト数を加算）を超えたら `53200 out of memory`（detail に `yuzhu.query_mem_limit` を超えたことを書く）で文を失敗させる。
- 新しい設定 `yuzhu.query_mem_limit`（既定 256MB、セッションで変更可）。`work_mem` / `hash_mem_multiplier` は PG と同じ名前・既定値で SET / SHOW できるようにし、M4 では使わない（M6 のスピルの閾値になる）。
- M6 のスピル（Grace ハッシュ結合、外部マージソート、ハッシュ集約のパーティション分割）の一時ファイルは、障害注入のため VFS（M2 の `Vfs` トレイト）経由で `base/pgsql_tmp/` に作る（PG と同じ置き場所）。

---

## 7. ルールベース最適化

### 7.1 ルールの一覧と順序

固定点まで繰り返す方式（DataFusion）ではなく、**決まった順に 1 回ずつ**適用する（デバッグしやすい。必要になったら一部を繰り返す）。各ルールは `fn(LogicalPlan, &mut OptCtx) -> Result<LogicalPlan>`。

| 順 | ルール | 内容 | 工数 | PG の対応箇所 |
|---|---|---|---|---|
| 1 | 定数畳み込み | 列も相関参照も含まない immutable な式を計算する。`AND`/`OR` の true/false/NULL の簡約、`CASE` の定数条件、`WHERE false` → `Empty`（PG の `One-Time Filter: false`【検証済み】）、`a = 1 + 2` → `a = 3`【検証済み】 | S | `eval_const_expressions`【ソース確認】clauses.c:2262 |
| 2 | サブクエリの結合化 | §6.5 の書き換え | M | `pull_up_sublinks` |
| 3 | 派生表・CTE の展開 | 単純な派生表（集約・LIMIT・DISTINCT・集合演算を含まない）を親に溶かす | S | `pull_up_subqueries`【ソース確認】prepjointree.c:940 |
| 4 | 外部結合の内部結合化 | LEFT JOIN の右側の列に対する strict な条件（`u.a = 1`、`u.a IS NOT NULL`）が上の WHERE にあれば INNER に。FULL は片側/両側で LEFT/INNER に。【検証済み】`t left join u on true where u.a = 1` が `Nested Loop`（内部結合）になる | S | `reduce_outer_joins`【ソース確認】prepjointree.c:2938 |
| 5 | 述語の押し下げ | WHERE の AND 項を、参照する列がそろう最も下へ。INNER の ON と WHERE は同じ扱い。外部結合では保存側の WHERE 条件は保存側へ下げてよいが、NULL 側へは下げない（ON の条件は NULL 側へ下げてよい）。グループキーだけを参照する HAVING の条件は WHERE へ。派生表の中へも下げる（集約の下へはグループキーだけの条件のみ）| M | `distribute_qual_to_rels`【ソース確認】initsplan.c:2196 |
| 6 | 結合キーの抽出 | 結合条件のうち `左の式 = 右の式`（両辺がそれぞれ片側だけを参照し、等値演算子がハッシュ可能）をキーに、残りを residual に | S | `hash_inner_and_outer`（joinpath.c）【未検証: 関数名】 |
| 7 | 結合順序 | §7.2 | S〜M | `standard_join_search`【ソース確認】allpaths.c:3411 |
| 8 | 列の刈り込み | 上で使わない列を Project で落とす（ハッシュ表・ソートのメモリを減らす） | S | `build_base_rel_tlists` など |
| 9 | 物理化 | インデックス選択（§7.3）、結合アルゴリズムとビルド側の選択、DISTINCT ON の Sort + Unique 化、Sort + Limit（Top-N は任意） | M | `create_plan` |

**定数畳み込みとエラー**: PG は畳み込みの途中でエラーになる式（`1/0`）があれば、実行されない分岐でも計画時にエラーにする（【検証済み】`select case when a > 0 then 1 else 1/0 end from t` → `22012`。docs <https://www.postgresql.org/docs/17/sql-expressions.html#SYNTAX-EXPRESS-EVAL> にも記載）。対象の行が 0 行でも（`... from t where false`）計画時にエラーになる【検証済み】。yuzhu も同じにする（畳み込みのエラーをそのまま返す）。

### 7.2 結合順序（統計なし）

- 明示的な `JOIN` の木は、外部結合を含む限り**順序を変えない**（PG の `join_collapse_limit = 8` の範囲では内部結合だけの木は平坦化して並べ替える）。
- 内部結合とカンマ結合だけからなる部分（PG の「結合の島」）は平坦化し、**構文順に、すでに選んだ集合と結合条件でつながる表を優先して**つないでいく貪欲法にする（直積を避ける）。つながる表がなければ構文順の次の表と直積。
- サイズの手がかり: 統計は持たないが、**計画時点の実ページ数**（smgr の nblocks）は安く取れる。PG も `estimate_rel_size`（【ソース確認】plancat.c:1065）で実ページ数を使っている。ハッシュ結合のビルド側は「ページ数 × 押し下げたフィルタがあれば 1/3 などの係数」で小さい方を選ぶ。結合順序には使わない（M4 では予測可能さを優先）。
- M6 のコストベース化で、PG と同じく動的計画法（`join_search_one_level`【ソース確認】joinrels.c:73）にする。

### 7.3 インデックス選択（簡単なヒューリスティクス）

- 対象: 押し下げ後に表へ付いている AND 項のうち、`列 op 定数式`（定数・パラメータ・InitPlan の値。左右逆も可）で、op が B+Tree の演算子（`= < <= > >=`、`BETWEEN` は 2 つに分解、`IS NULL` は B+Tree が NULL を持つなら）で、列がインデックスの**先頭から連続した**キー列であるもの。
- 選び方（上から優先）:
  1. 一意インデックスの全キー列が等値で指定されている
  2. 等値で指定されたキー列の数が多いもの
  3. 先頭キー列に範囲条件があるもの
  4. それ以外は SeqScan
  - 同点ならインデックスの OID が小さい方（決定的にするため）。
- **表が小さくてもインデックスを使う**（統計がないので）。PG は小さい表では SeqScan を選ぶ【検証済み: 3 行の表で `Seq Scan on u / Filter: (a = 1)`】ので、EXPLAIN は PG と一致しない。結果は同じ。
- インデックススキャンはヒープを引いて MVCC の可視性を判定する（Index Only Scan は可視性マップのある M5 以降）。インデックス条件で使った述語も、型変換や NULL の扱いの誤りに備えてヒープ行で再評価してよい（PG の recheck とは意味が違う。安全側）。
- `enable_indexscan = off` / `enable_seqscan = off` は PG と同じ名前の設定で、「他の選択肢があれば使わない」という意味にする（PG はコストを大きくするだけだが、結果は同じ）。

### 7.4 インデックス付きネステッドループ（M4 後半、任意）

- 条件: 内部結合・LEFT・SEMI・ANTI で、内側がベーステーブルで、結合キーの列に**一意インデックス**がある（外部キー → 主キーの結合）。外側が「フィルタ付き」または「ページ数が内側より少ない」。
- 物理: `NestedLoopParam { params: [外側のキー式] }` + 内側 `IndexScan { bounds: 列 = Param }`。
- これがないと「小さな絞り込み結果と大きなマスタ表の結合」でマスタ表全体のハッシュ表を作ることになるが、正しさには影響しない。

### 7.5 PG の設定との対応

| 設定 | PG の既定 | yuzhu M4 |
|---|---|---|
| `enable_hashjoin` / `enable_nestloop` / `enable_mergejoin` | on | hashjoin / nestloop を実装（mergejoin は受け付けるだけ）。off のとき他に選択肢があれば使わない。**等値結合で両方 off なら nestloop を使う**（PG と同じく完全には禁止しない） |
| `enable_hashagg` / `enable_sort` | on | hashagg off なら Sort + GroupAggregate（M4 で GroupAggregate を作るなら）。作らないなら受け付けるだけ |
| `enable_seqscan` / `enable_indexscan` / `enable_bitmapscan` / `enable_indexonlyscan` | on | 上記 |
| `enable_material` | on | NLJ の内側の Materialize を省く |
| `join_collapse_limit` / `from_collapse_limit` | 8 / 8 | 受け付けるだけ |
| `work_mem` / `hash_mem_multiplier` | 4MB / 2 | 受け付けるだけ（§6.6） |

---

## 8. EXPLAIN

### 8.1 PG のテキスト形式（【検証済み】PG17.11 の出力）

```
Sort
  Sort Key: t.a
  ->  Hash Join
        Hash Cond: (t.a = u.a)
        ->  Seq Scan on t
              Filter: (b > 5)
        ->  Hash
              ->  Seq Scan on u
```

- 結果は列 `QUERY PLAN`（text）の 1 行 1 タプル。
- インデントの規則（`ExplainNode`【ソース確認】explain.c:1367、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/commands/explain.c>）: 根のノード名は 0 桁。詳細行はノード名の位置 + 2。子ノードは「親の位置 + 2」に `->  ` を置き、子のノード名は親の位置 + 6 から。子の詳細行は親の位置 + 8。
- コストは既定で `  (cost=26.74..26.76 rows=6 width=76)` がノード名の後に付く。`COSTS OFF` で消える。
- 主なノード名と詳細行【検証済み】:

| yuzhu の物理ノード | PG の表示 | 詳細行 |
|---|---|---|
| SeqScan | `Seq Scan on t`（別名があれば `Seq Scan on t x`） | `Filter: (...)` |
| IndexScan | `Index Scan using u_a_idx on u`（逆順は `Index Scan Backward using ...`） | `Index Cond: (a = 1)`、`Filter:` |
| HashJoin | `Hash Join` / `Hash Left Join` / `Hash Right Join` / `Hash Full Join` / `Hash Semi Join` / `Hash Anti Join`。ビルド側の子は `Hash` ノード | `Hash Cond: (t.a = u.a)`、`Join Filter:`（residual） |
| NestedLoopJoin | `Nested Loop`（`Nested Loop Left Join` など） | `Join Filter: (t.a < u.a)` |
| Materialize | `Materialize` | |
| Aggregate | `Aggregate` | `Filter:`（HAVING） |
| HashAggregate | `HashAggregate` | `Group Key: a`、`Filter: (count(*) > 1)` |
| Sort | `Sort` | `Sort Key: a`（`DESC`、`NULLS FIRST` を付記） |
| Unique | `Unique` | |
| Limit | `Limit` | |
| Append | `Append` | |
| HashSetOp | `HashSetOp Intersect` / `HashSetOp Except` / `... All` | 子は `Subquery Scan on "*SELECT* 1"` |
| Result | `Result` | `One-Time Filter: false` |
| Values | `Values Scan on "*VALUES*"` | |
| CteScan | `CTE Scan on x` | 親の下に `CTE x` → 子プラン |
| SubqueryScan | `Subquery Scan on s` | |
| FunctionScan | `Function Scan on generate_series` | |
| Insert / Update / Delete | `Insert on t` / `Update on t` / `Delete on t` | |
| InitPlan / SubPlan | ノードの詳細行の後に `InitPlan 1` / `SubPlan 1` → 子プラン。参照側は `(InitPlan 1).col1`、`(hashed SubPlan 1).col1`、`(SubPlan 1)` | PG17 で表記が変わった（PG16 までは `$0`）【検証済み: 17 の表記】 |

- 式の表示は PG の `ruleutils.c` の deparse と同じ規則にする: 比較などの二項演算は括弧で囲む（`(a = 1)`）。リテラルは型付きで出る場合がある（`'p'::text`、`'{1,2}'::integer[]`）。結合があると列を `t.a` のように修飾し、単一表なら修飾しない（`Filter: (b > 5)` と `Hash Cond: (t.a = u.a)`）。`VERBOSE` では常に修飾し、`Output:` 行が付き、`Seq Scan on public.t` になる【検証済み】。
- **この deparser を `pg_get_expr` / `pg_get_constraintdef` の正規形（M2-Q9、`adbin` を正規形に移す件）と共通の部品にする**。M4 で `\d tbl` に対応するときに必要になる。

### 8.2 yuzhu での方針

- ノード名・詳細行・インデント・式の表記を PG と同じにする（S〜M。deparser が大半）。
- コスト: yuzhu にはコストモデルがないので、既定（`COSTS ON`）では PG と同じ書式で `(cost=0.00..0.00 rows=0 width=W)` を出す（W は型の平均幅。PG の `get_typavgwidth` 相当の固定値）【提案。値に意味はない】。ツールが書式を解析して落ちないようにするため。テストでは常に `COSTS OFF`。
- RIGHT JOIN は yuzhu では LEFT に変換するので、PG が `Hash Right Join` を出す場面で yuzhu は `Hash Left Join`（左右逆）を出しうる。PG もコストで選ぶので、表示の一致は目標にしない。
- `EXPLAIN ANALYZE`: 各ノードの実行回数（loops）と出力行数を数え、`(actual time=... rows=N loops=L)` と `Planning Time:` / `Execution Time:` を出す。時間は比較できないので、テストは `EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF)` を使う。PG の出力は `Aggregate (actual rows=1 loops=1)`（ノード名の後に空白 1 つ）で、Planning Time / Execution Time の行は出ない【検証済み】。DML の EXPLAIN ANALYZE は実際に書き込む（PG と同じ）。
- EXPLAIN 自体のパース: `EXPLAIN [ANALYZE] [VERBOSE] stmt` と `EXPLAIN (option [value], ...) stmt`。知らないオプションは `42601 unrecognized EXPLAIN option "x"`（位置付き）【検証済み】。

---

## 9. テスト戦略

### 9.1 slt（PG で検証する結果テスト）

配置案（`tests/slt/m4/`）:

```
join/{inner,left,right,full,cross,using,natural,nested,self,errors}.slt
agg/{count,sum_avg,min_max,bool,distinct,filter,group_by,having,empty,errors,functional_dep}.slt
subquery/{scalar,exists,in,not_in_null,any_all,correlated,from,errors}.slt
setop/{union,intersect,except,precedence,types,errors}.slt
cte/{basic,materialized,errors}.slt
distinct_on.slt
dml/{update_from,delete_using}.slt
index/{eq,range,null,mvcc}.slt          … 結果だけを確かめる（プランは見ない）
plan_variants/*.slt                       … §9.2
psql/{dt,d_table}.slt                     … psql の \dt / \d が発行する SQL をそのまま流す
explain/*.slt                             … onlyif yuzhu（§9.3）
```

規則（M1 の規則に追加）:

- **結果の行順が決まらない問い合わせには必ず `ORDER BY` か `rowsort` を付ける**。ハッシュ結合・ハッシュ集約・集合演算の出力順は PG でも不定【検証済み: 結合結果の順序が実行ごとに違って見える例あり】。`ORDER BY` を付けても同順位の行が残る場合（`order by count(*) desc` だけなど）は第 2 キーを足す。
- NULL を含むデータを必ず混ぜる（結合キーの NULL、集約の NULL、NOT IN の NULL、FULL JOIN の COALESCE）。
- 浮動小数の `sum` / `avg` は、足す順序で結果が変わらない値だけを使う。
- 型の表示を確かめる: `pg_typeof(sum(a))`、`pg_typeof(avg(a))` のように型名で書くと、slt の列型文字（`I`/`R`/`T`）より強く検証できる。
- numeric を使うテスト（`avg(int)` など）は、numeric の最小核が入るまで yuzhu で `skipif yuzhu` にせず、**ファイルを分けて**（`agg/numeric_results.slt`）実行対象から外す。`skipif` を乱用すると、入れたあとに外し忘れる。
- エラーは SQLSTATE で照合する（§3 の表）。

### 9.2 実行ノードの網羅: PG と同じ設定で計画を変える

PG にも `enable_hashjoin` などがあるので、**同じファイルを PG と yuzhu の両方に流せる**のが利点。

```
statement ok
set enable_hashjoin = off

query ITT rowsort
select t.a, t.e, u.c from t left join u on t.a = u.a
----
...

statement ok
reset enable_hashjoin
```

- 1 つの問い合わせ群を「既定」「hashjoin off」「nestloop off（等値結合ならハッシュ）」「indexscan off」「hashagg off」で流すファイルを作る。結果はすべて同じでなければならない。ファイルの重複を避けるため、共通部分をテンプレートから生成するスクリプト（`tests/gen/plan_variants.py` など）を置いてもよい【提案】。
- PG では `enable_*` は禁止ではなくコストの加算なので、PG 側で本当にそのプランになったかは保証されない（結果が同じなら問題ない）。

### 9.3 EXPLAIN とプランのテスト

- EXPLAIN の出力は PG とプランの選び方が違うので、**slt では `onlyif yuzhu`** にする（ランナーは `--label yuzhu` を付ける。tests/run.sh 確認済み）。中身は yuzhu の期待するプラン（ルールが効いていることの確認）。
- 加えて、Rust 側にプランナのスナップショットテスト（toydb の goldenscript 方式。research-rust-db-arch §1.1）を置く: 「SQL → 論理プラン（最適化前）→ 論理プラン（各ルールの後）→ 物理プラン」をテキストで期待値ファイルと比較する。dev-dependency は自由なので `insta` などを使ってよい。
- PG の EXPLAIN と一致させたい部分（ノード名、詳細行の書式、式の deparse）は、PG と同じプランになる単純な問い合わせ（単一表の Filter、`WHERE false`、`select 1`、`values`）だけを PG でも流して確かめる（`EXPLAIN (COSTS OFF)`）。

### 9.4 PG の回帰テストの取り込み

research-slt.md §3 の通り、PG の `src/test/regress/sql/{join,aggregates,subselect,union,with}.sql` から、M4 の範囲の問い合わせを選んで slt に変換する（PostgreSQL License。出典を残す）。期待値は PG に流して作る（`--override`）。工数 M（選別が主）。

### 9.5 PG を正解とする差分ランダムテスト（M4 後半）

- SQLancer の **TLP（Ternary Logic Partitioning）** と **NoREC** の考え方（Rigger & Su, OOPSLA 2020 / ESEC/FSE 2020）を使う: ランダムな表とデータ、ランダムな結合・集約・サブクエリの問い合わせを生成し、(1) PG と yuzhu の結果（ソート済み）を比べる、(2) yuzhu 単体で「`Q` の結果 = `Q WHERE p` ∪ `Q WHERE NOT p` ∪ `Q WHERE p IS NULL`」を確かめる。
- PG はすでに Docker で動かしているので、(1) が最も強い。生成器は Rust の dev-only バイナリ（`impl/rust/crates/yuzhu-fuzz-sql`）にし、シード固定で CI では少数、夜間に多数回す【提案】。
- 生成する範囲は yuzhu の対応範囲に限る（numeric が入るまで avg は除くなど）。工数 M。

### 9.6 メモリ上限のテスト

`generate_series` で大きな表を作り、`set yuzhu.query_mem_limit = '1MB'` で `53200` になることを確かめる。PG にはこの設定がないので `onlyif yuzhu`。

---

## 10. 工数の見積もりと並列分担

### 10.1 見積もり

| 作業 | 工数 | 依存 |
|---|---|---|
| 契約の変更（Bound の木、RTE、ColumnRef、論理/物理の骨組み、PhysExpr、eval の `&mut`、rewind） | M | なし。**最初に** |
| アナライザ: JOIN（ON / USING / NATURAL、スコープ、エラー） | M | 契約 |
| アナライザ: 集約・GROUP BY・HAVING・関数従属・DISTINCT ON | M | 契約 |
| アナライザ: サブクエリ・集合演算・CTE | M | 契約 |
| 実行: NLJ・HashJoin（全種）・Materialize | M | 契約 |
| 実行: Aggregate・HashAggregate・Unique・HashSetOp・Append・CteScan | M | 契約 |
| 実行: SubPlan / InitPlan / ハッシュ化 SubPlan | M | 契約 |
| 最適化ルール（§7.1 の 1〜8） | M〜L | 論理プラン |
| 物理化・インデックス選択・インデックス付き NLJ | M | B+Tree |
| EXPLAIN + deparser（`pg_get_expr` と共通） | M | 物理プラン |
| numeric の最小核（§5.2 案 A） | M〜L | なし（独立） |
| `UPDATE ... FROM` / `DELETE ... USING` | S | JOIN |
| `generate_series`（FunctionScan） | S | 契約 |
| メモリ予算と `53200` | S | 実行 |
| slt（§9.1〜9.3、PG で検証）| M〜L | なし（先行して書ける） |
| PG 回帰テストの取り込み | M | slt の土台 |
| 差分ランダムテスト | M | JOIN・集約の完成 |
| psql の `\dt` / `\d tbl` | M | JOIN、サブクエリ、正規表現、`regclass` |

合計: 1 人で直列なら 8〜11 週間程度。下の分担で並列にすれば、契約の変更（約 1 週間）の後、3〜4 週間程度【見積もり】。

### 10.2 並列分担の案（M1/M2 と同じ「基盤を先に固めて担当を並べる」方式）

| 担当 | 範囲 | 依存 |
|---|---|---|
| **Q0 基盤** | §1.2 の契約変更、`planner/logical.rs`・`physical.rs`・`PhysExpr` の型定義、builder の骨組み（単一表で M1/M2 の slt が全部通る状態まで） | なし |
| Q1 結合 | JOIN のアナライザ、Join の builder、NLJ / HashJoin / Materialize、`UPDATE ... FROM` | Q0 |
| Q2 集約 | 集約のアナライザ、Aggregate / HashAggregate、DISTINCT ON / Unique、集約関数の表 | Q0 |
| Q3 サブクエリ・集合演算・CTE | アナライザ、SubPlan / InitPlan、Append / HashSetOp / CteScan、§6.5 の書き換え | Q0 |
| Q4 最適化・EXPLAIN | §7 のルール、物理化、EXPLAIN、deparser | Q0（Q1〜Q3 のノードが揃うごとに対応） |
| Q5 numeric | §5.2 案 A | なし（Q0 と同時に開始可） |
| Q6 テスト | slt（PG で検証）、plan_variants の生成、回帰テストの取り込み、差分ランダムテスト | なし（Q0 と同時に開始可） |
| Q7 インデックス | IndexScan、インデックス選択、インデックス付き NLJ | Q0、B+Tree |

---

## 11. 確認事項（仮決めした点）

- **M4Q-1 論理プランと物理プランの分離**: M4 で「Bound → 論理プラン（列 ID で参照）→ 物理プラン（オフセットで参照）」に分けます。M1/M2 の `ColumnRef { index }` を `ColumnRef { rte, col, levels_up }` に変える大きな契約変更になります（M1/M2 の analyzer・planner・executor に手が入ります。基盤担当で 1 週間程度）。
- **M4Q-2 numeric の前倒し**: `sum(bigint)` と `avg(整数)` の結果型が numeric のため、numeric の最小核（演算・入出力・キャスト・小数リテラル）を M5 から M4 に前倒しすることを推奨します（+5〜8 日。並列の担当 1 つ）。前倒ししない場合、これらの集約と小数リテラルは M5 まで「未対応（0A000）」エラーになります。float8 で代用する案は、型と表示が PostgreSQL と違って黙って間違うので採りません。
- **M4Q-3 ディスクへのスピルは M6**: M4 ではハッシュ結合・ソート・集約をすべてメモリ上で行い、上限（新設定 `yuzhu.query_mem_limit`、既定 256MB）を超えたら `53200 out of memory` で文を失敗させます。PostgreSQL は失敗せずにディスクを使うので、大きな問い合わせで挙動が違います。
- **M4Q-4 CTE はすべてインライン展開**: `MATERIALIZED` を付けない限り、2 回以上参照される CTE もインライン展開します（PostgreSQL は 2 回以上なら 1 回だけ実行して共有）。結果は同じで、EXPLAIN の表示と性能だけが違います。
- **M4Q-5 WITH RECURSIVE と LATERAL は M5 以降**: どちらも構文は受け付けて「未対応（0A000）」を返します。ウィンドウ関数と GROUPING SETS は M6 以降です。
- **M4Q-6 EXPLAIN の互換の程度**: ノード名・詳細行・インデント・式の表記は PostgreSQL と同じにしますが、プランの選び方（インデックスを使うか、どちらをハッシュ表にするか）は統計がないため一致しません。コストの数値は意味のない値（0.00）を同じ書式で出します。EXPLAIN のテストは yuzhu 専用（`onlyif yuzhu`）にします。
- **M4Q-7 GROUP BY の主キーへの関数従属**: M4 で対応します（ORM が使うため）。PostgreSQL はこの依存関係を `pg_depend` に記録して主キーの削除を防ぎますが、yuzhu の M4 では記録しません（主キーを削除すると、その後の問い合わせがエラーになるだけです）。
- **M4Q-8 インデックスは小さい表でも使う**: 統計がないので、使えるインデックスがあれば常に使います。PostgreSQL は小さい表では順次走査を選びます。結果は同じで、EXPLAIN と性能だけが違います。
- **M4Q-9 FULL JOIN の制限**: PostgreSQL と同じく、等値条件のない FULL JOIN は「未対応（0A000）」にします（ネステッドループで実装すれば動かせますが、PostgreSQL がエラーにするものを通す意味は薄いため）。
- **M4Q-10 generate_series の追加**: テストで大きなデータを簡単に作るため、`generate_series(int, int)` を M4 で入れます（PostgreSQL にある関数で、FROM 句に書きます）。
- **M4Q-11 psql の `\d tbl`**: M4 で `\dt` と `\d tbl` を動かす前提にしています（M2-Q8 の継続）。`\d tbl` にはこのレポートの JOIN・サブクエリに加え、`regclass`、正規表現、`pg_get_expr` の正規形（M2-Q9）などが要ります。M4 の終盤に回します。

---

## 参考資料

PostgreSQL REL_17_STABLE のソース（関数の位置は【ソース確認】で確認したもの）:

- パーサ・アナライザ: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/parser/parse_clause.c>（`transformFromClauseItem` 1056、`transformJoinUsingClause` 308、`findTargetlistEntrySQL92` 2006、`transformDistinctOnClause` 3069）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/parser/parse_agg.c>（`parseCheckAggregates` 1131、`check_ungrouped_columns` 1328）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/parser/analyze.c>（`transformSetOperationTree` 2003）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/parser/parse_cte.c>（`transformWithClause` 110）
- プランナ: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/prep/prepjointree.c>（`pull_up_sublinks` 459、`pull_up_subqueries` 940、`reduce_outer_joins` 2938）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/plan/subselect.c>（`make_subplan` 162、`SS_process_ctes` 884、`convert_ANY_sublink_to_join` 1258、`convert_EXISTS_sublink_to_join` 1375）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/util/clauses.c>（`eval_const_expressions` 2262）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/plan/initsplan.c>（`deconstruct_jointree` 734、`distribute_qual_to_rels` 2196）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/path/allpaths.c>（`standard_join_search` 3411）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/path/joinrels.c>（`join_search_one_level` 73）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/optimizer/util/plancat.c>（`estimate_rel_size` 1065）
- 実行器: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeHash.c>（`ExecChooseHashTableSize` 675）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeHashjoin.c>、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeNestloop.c>、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeAgg.c>（`hash_agg_enter_spill_mode` 1882）、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeSetOp.c>、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeSubplan.c>（`ExecHashSubPlan` 101、`ExecScanSubPlan` 204）
- EXPLAIN: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/commands/explain.c>（`ExplainNode` 1367）、式の表示 <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/ruleutils.c>
- 集約と numeric: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/catalog/pg_aggregate.dat>、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/adt/numeric.c>（`int8_avg` 6854、`select_div_scale` 9831）

PostgreSQL 17 のドキュメント:

- 結合: <https://www.postgresql.org/docs/17/queries-table-expressions.html>
- 集約関数: <https://www.postgresql.org/docs/17/functions-aggregate.html>
- サブクエリ式: <https://www.postgresql.org/docs/17/functions-subquery.html>
- 集合演算: <https://www.postgresql.org/docs/17/queries-union.html>
- WITH: <https://www.postgresql.org/docs/17/queries-with.html>
- DISTINCT ON: <https://www.postgresql.org/docs/17/sql-select.html#SQL-DISTINCT>
- 式の評価順と定数畳み込み: <https://www.postgresql.org/docs/17/sql-expressions.html#SYNTAX-EXPRESS-EVAL>
- EXPLAIN: <https://www.postgresql.org/docs/17/sql-explain.html>、<https://www.postgresql.org/docs/17/using-explain.html>
- プランナの設定（`enable_*`、`work_mem`）: <https://www.postgresql.org/docs/17/runtime-config-query.html>、<https://www.postgresql.org/docs/17/runtime-config-resource.html>

その他:

- T. Neumann, A. Kemper, "Unnesting Arbitrary Queries", BTW 2015（一般的な非相関化）
- M. Rigger, Z. Su, "Finding Bugs in Database Systems via Query Partitioning", OOPSLA 2020（TLP）／ "Detecting Optimization Bugs in Database Engines via Non-Optimizing Reference Engine Construction", ESEC/FSE 2020（NoREC）。SQLancer: <https://github.com/sqlancer/sqlancer>
- DataFusion の論理プランと最適化ルール、toydb の optimizer、BusTub の optimizer: research-rust-db-arch.md §1 を参照
