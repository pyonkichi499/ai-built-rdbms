# yuzhu M4 設計 10: EXPLAIN・deparse・COPY・psql / pgbench 互換

`00-contracts.md`（以下「00」）の担当 **E1**（EXPLAIN と deparse）、**O1**（COPY）、**J**（サーバ）、**S**（セッション・設定）の仕様です。この章だけ読めば実装できる粒度で書きます。署名と名前は 00 に従い、足りないものは追加しました。00 から変える必要があったものは末尾の「00 への変更提案」に集約しています。

- 前提: `00-contracts.md`（§6 式、§9.3 `ExplainNode`、§14.5 COPY、§15.4 設定）、`spec/design/m1.md`・`m2.md`・`m3.md`、`QUESTIONS.md`
- 調査（根拠）: `spec/research/m4-query.md` §8、`pg-compat-tools.md` §3.1・§3.2・§4・§5・§6
- 実機: PostgreSQL 17.11（`sandbox/pg.sh start`、`127.0.0.1:55432`）と psql 17.11 / pgbench 17.11。本文の「[実機]」は、この章を書くときに実際に PostgreSQL に流して確かめたもの。確かめていないものは「（未検証）」。
- PostgreSQL のソースは REL_17_STABLE。`PG:<path>` と略す。

---

## 1. 範囲

| 分類 | M4 で行うこと | 担当 |
|---|---|---|
| EXPLAIN | `EXPLAIN [ANALYZE] [VERBOSE] stmt` と `EXPLAIN (option [value], ...) stmt`。出力は FORMAT TEXT のみ。`COSTS`・`TIMING`・`SUMMARY` の切り替え。`ANALYZE` は実際に実行する（DML は実際に書く） | E1、L2、S |
| deparse | 式の逆変換。EXPLAIN の式表示と `pg_get_expr` が共有する。`format_type`、任意で `pg_get_constraintdef` と `pg_get_indexdef` | E1 |
| COPY | `COPY t [(cols)] FROM STDIN [WITH (...)]`（テキスト形式、Simple Query）。旧構文の `WITH NULL AS`、`DELIMITER` | O1、S、J |
| psql | `\dt` `\dn` `\di` `\l` が動く。`\dt+` `\d tbl` `\df` `\du` は任意 | S、E1、K |
| pgbench | `pgbench -i`（既定の手順 `dtgvp` と `-I dtGvp`）と組み込み tpcb-like の `-M simple`（`-c 4 -T 30`）の完走 | K（互換テスト）、全担当の機能 |
| 設定 | PostgreSQL 17 にあって意味を持たない GUC を受け付けて保存する（`INERT_GUCS`） | S |
| テスト | `tests/slt/m4/explain`、`copy`、`psql`、`tests/compat/{psql,pgbench}` | K |

**この章で対応しない（実行すると `0A000`）**: `FORMAT JSON|XML|YAML`、COPY の CSV・バイナリ・`TO`・ファイル・`PROGRAM`・`WHERE`・`ON_ERROR`、Extended Query 経由の COPY、`EXPLAIN` の `BUFFERS` `WAL` `SETTINGS` `MEMORY` `SERIALIZE` の出力（受け付けて無視する。§3.1）、`EXPLAIN` の対象が `SELECT` / `VALUES` / `INSERT` / `UPDATE` / `DELETE` 以外の文（構文エラー）。

---

## 2. 決定（この章で追加で決めたこと）

| # | 論点 | 選択肢 | 決定 | 理由 |
|---|---|---|---|---|
| D10-1 | `ExplainNode` を `PhysicalPlan` と同形にするか | 同形（00 §9.3 の文面）／PostgreSQL に表示されるノードだけの木（`Project`・`Filter` は親か子に吸収） | **表示用の木。同形にはしない。計測値との対応は `exec_id` で持つ** | PostgreSQL には `Project` ノードも `Filter` ノードもない（式は `Output:` と `Filter:` に出る）。`Hash`・`Subquery Scan` のように物理プランにない表示ノードもある。00 の「同形」は文字どおりには実現できない（00 への変更提案 1） |
| D10-2 | `ExplainNode` を作る場所 | 物理プランから後で（位置参照を名前に戻す）／物理化の途中で（`ColId` から名前を引ける） | **物理化の途中（`planner/explain_tree.rs`、L2）** | `PhysCol::Local(i)` には名前も修飾名もない。`ColumnArena` と論理プランがある物理化の時点で文字列にする |
| D10-3 | deparse の入力 | 物理プランの `PhysExpr`／論理の `LExpr`（`ColId`）／`Expr<C, Q>` に対して総称的に | **総称的（`ColumnNamer<C>`、`SubLinkRenderer<Q>`）** | EXPLAIN は `ColId` 版、`pg_get_expr` は `Var` 版を使う。式の書式の規則は 1 か所に置く |
| D10-4 | deparse の 2 つの書式 | 1 つ／EXPLAIN 用（計画後。定数畳み込み済み）と保存式用（計画前） | **2 つ（`DeparseMode::Plan` と `Stored`）** | PostgreSQL の出力が実際に違う（`i IN (1,2,3)` は EXPLAIN では `ANY ('{1,2,3}'::integer[])`、CHECK では `ANY (ARRAY[1, 2, 3])`。§4.2） |
| D10-5 | `pretty`（`pg_get_expr(..., true)`）を作るか | 作らない／作る | **作る**（括弧を減らす規則と CASE の字下げ） | psql の `\d tbl` が `pretty = true` で呼ぶ。`\d tbl` は任意だが、規則は小さく、非 pretty と部品を共有する |
| D10-6 | ANALYZE の出力 | PostgreSQL と完全に同じ（`Sort Method`・`Hash Buckets`・`Memory Usage` を含む）／必要な行だけ | **`actual` と `Rows Removed by ...` だけ。`Sort Method`・`Buckets`・`Batches`・`Memory Usage`・`Heap Fetches` は出さない** | 値が yuzhu の実装に依存して一致しない。テストは `COSTS OFF, TIMING OFF, SUMMARY OFF` の出力を比べる |
| D10-7 | `BUFFERS` `WAL` `SETTINGS` `MEMORY` `SERIALIZE` `GENERIC_PLAN` | `0A000`／受け付けて無視 | **受け付けて無視**。ただし PostgreSQL と同じ前提条件の検査（`WAL`・`SERIALIZE` は `ANALYZE` が必要など）は行う | pgAdmin や DBeaver の EXPLAIN が付ける。拒否すると使えない |
| D10-8 | `transaction_timeout`（PostgreSQL 17 の新 GUC） | M3 の `42704`／受け付けて保存だけ | **受け付けて保存だけ（強制しない）** | pg_dump 17 が接続直後に `SET transaction_timeout = 0` を送る（`pg-compat-tools.md` §2.6）。M3 の `m3.md` §1.2 の「`42704`」を上書きする（確認事項 [10-Q8]） |
| D10-9 | COPY の入力の処理単位 | 全部ためてから／CopyData 1 つごと | **CopyData 1 つごと。行の境界は CopyData の境界と無関係。保持するのは未完の 1 行だけ** | `pgbench -i -s 100` は 1000 万行。メモリは行単位 |
| D10-10 | COPY 中の再開 | Session が文の列を保持する／サーバが保持する | **Session が `PendingQuery`（残りの文と元の SQL）を保持する** | 「同じ `Query` メッセージの続きの文」（`COPY t FROM STDIN; SELECT 1`）を CopyDone の後に実行する必要がある（PostgreSQL と同じ。[実機]） |
| D10-11 | COPY の失敗の後のデータ | 受信し続けて捨てる／直ちに `E` と `Z` を返し、後から来る `d` `c` `f` を無視 | **後者（PostgreSQL と同じ）** | [実機] で確かめた。`d` / `c` / `f` はアイドル状態で無視される（PG:src/backend/tcop/postgres.c の `PostgresMain`） |
| D10-12 | COPY の CSV・バイナリ・`WHERE`・`ON_ERROR` | M4／M5 | **M5（`0A000`）** | 範囲外。構文だけは解析して持つ |
| D10-13 | COPY FREEZE | 凍結する／検査だけして通常の挿入 | **検査だけ**（00 D-30、`pg-compat-tools.md` §8 の 6） | 違いは他のトランザクションから早く見える点だけ |
| D10-14 | `\dt` の slt での確認 | 実際の psql が流す SQL を全文そのまま／LIKE で絞る | **両方**: slt（`tests/slt/m4/psql/`）は名前で絞った版、`tests/compat/psql/` は psql 17 の出力そのもの | slt の DB には他のテストの残りがあり、全件を返す問い合わせは比べにくい（M2-Q22） |

---

## 3. EXPLAIN

### 3.1 構文とオプション

**構文**（PG:src/backend/parser/gram.y の `ExplainStmt`）:

```
EXPLAIN [ANALYZE | ANALYSE] [VERBOSE] explainable_stmt        -- 旧構文。順序は ANALYZE → VERBOSE
EXPLAIN ( option [value] [, ...] ) explainable_stmt
explainable_stmt := SELECT ... | VALUES ... | INSERT ... | UPDATE ... | DELETE ... | ( select ) 
```

- `EXPLAIN VERBOSE ANALYZE ...` は構文エラー（`42601`、`syntax error at or near "analyze"`）[実機]。
- 括弧の中の **option 名**は任意の語（予約語の `analyze` `verbose` も可）。小文字に畳む。**value** は省略（= true）、`true` / `false` / `on` / `off`（語でも引用符つき文字列でも。大文字小文字を区別しない）、整数 `0` / `1`、または option によっては語（`FORMAT json`、`SERIALIZE text`）。
- 同じ option を複数回書いてもエラーにならず、**最後のものが有効**（`(costs, costs off)` は off、`(format text, format json)` は json）[実機]。
- `EXPLAIN` の対象が上のもの以外（`EXPLAIN CREATE TABLE`、`EXPLAIN EXPLAIN`、`EXPLAIN BEGIN`）は、その語の位置の構文エラー（`42601`）[実機]。

**AST**（`sql/ast.rs`。M1 の `Explain { analyze, verbose, .. }` を置き換える。S1 の担当）:

```rust
pub struct Explain {
    pub options: Vec<ExplainOption>,
    pub statement: Box<Statement>,
    pub span: Span,
}
pub struct ExplainOption {
    /// 小文字に畳んだ名前
    pub name: String,
    pub value: Option<ExplainValue>,
    /// 名前の位置（エラー位置に使う）
    pub name_span: Span,
}
pub enum ExplainValue {
    /// `true` `false` `on` `off`、引用符つきの文字列、引用符なしの語
    Word(String),
    Integer(i64),
    /// 小数など。ブール値としては常に不正
    Other(String),
}
```

旧構文は S1 が `ExplainOption { name: "analyze", value: None }` と `"verbose"` に直す（AST は 1 種類にする）。M1 のパーサは「知らない値は true」と読んでいる（`parse_explain_option_value`）が、**アナライザの検査に移す**。

**アナライザ**（`analyzer::analyze` の `Statement::Explain` の分岐が呼ぶ。分岐の口は P0 が置き、`resolve_options` は E1 が書く）:

```rust
// explain/mod.rs
/// option 列を解釈して ExplainOptions にする。エラーは §3.1 の表
pub fn resolve_options(options: &[ExplainOption]) -> Result<ExplainOptions>;
```

`ExplainOptions`（00 §7）は `{ analyze, verbose, costs, timing, summary }`。解釈の手順（PG:src/backend/commands/explain.c `ExplainQuery`）:

1. option を先頭から順に見る。名前が下のどれでもなければ `42601 unrecognized EXPLAIN option "x"`（**位置は名前**）。
2. 値の解釈（ブール値の option。`defGetBoolean`）: 値なし → true。語・文字列が `true` / `false` / `on` / `off`（大文字小文字を区別しない）なら対応する値。整数が `0` / `1` なら false / true。それ以外（`t`、`y`、`2`、`'maybe'`、小数）は **`42601 {名前} requires a Boolean value`**（位置なし。名前は書かれたとおり小文字）。`analyze t` も `analyze y` もエラー [実機]。
3. `format`: 語 `text` は受け付ける。`json` / `xml` / `yaml`（大文字小文字を区別しない）は **`0A000 EXPLAIN option FORMAT with value "json" is not supported yet`**。それ以外の語は **`22023 unrecognized value for EXPLAIN option "format": "foo"`**（位置は名前）[実機]。
4. `serialize`: `none` / `text` / `binary` 以外は `22023 unrecognized value for EXPLAIN option "serialize": "foo"`（位置は名前）[実機]。値なしは `text`。受け付けて無視する。
5. 全部読んだ後に、既定値と検査を行う（PostgreSQL と同じ順）:
   - `timing` が書かれていなければ `timing = analyze`、`summary` が書かれていなければ `summary = analyze`。`costs` の既定は true。
   - `timing` が**明示的に true**で `analyze` が false → **`22023 EXPLAIN option TIMING requires ANALYZE`** [実機]（`timing off` は可）。
   - `wal` が true で `analyze` が false → `22023 EXPLAIN option WAL requires ANALYZE` [実機]。`serialize` が `none` でなく `analyze` が false → `22023 EXPLAIN option SERIALIZE requires ANALYZE` [実機]。
   - `generic_plan` が true で `analyze` が true → **`0A000 EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together`** [実機]。`generic_plan` が true で `analyze` が false は受け付けて無視する（M4 にパラメータがないので通常の EXPLAIN と同じ）。
   - `buffers` `wal` `settings` `memory` `serialize` `generic_plan` の値は保持せず、出力にも影響させない（D10-7）。PostgreSQL は `SETTINGS`（既定値以外の設定があれば `Settings:` 行）、`MEMORY`（`Planning:` 節）、`SERIALIZE`（`Serialization:` 行）、`BUFFERS` を出す。yuzhu は出さない（既知の差。§3.12）。

**エラーの表**（PostgreSQL 17 [実機]）:

| 入力 | SQLSTATE | メッセージ | 位置 |
|---|---|---|---|
| `EXPLAIN (foo) SELECT 1` | `42601` | `unrecognized EXPLAIN option "foo"` | あり |
| `EXPLAIN (analyze foo) ...` | `42601` | `analyze requires a Boolean value` | なし |
| `EXPLAIN (analyze 2) ...` | `42601` | `analyze requires a Boolean value` | なし |
| `EXPLAIN (analyze true false) ...` | `42601` | `syntax error at or near "false"` | あり |
| `EXPLAIN (format foo) ...` | `22023` | `unrecognized value for EXPLAIN option "format": "foo"` | あり |
| `EXPLAIN (format json) ...` | `0A000`（yuzhu のみ。PostgreSQL は JSON を出す） | `EXPLAIN option FORMAT with value "json" is not supported yet` | あり |
| `EXPLAIN (timing on) ...` | `22023` | `EXPLAIN option TIMING requires ANALYZE` | なし |
| `EXPLAIN (analyze, generic_plan) ...` | `0A000` | `EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together` | なし |

**実機で確かめたオプションの組み合わせ**（`SELECT 1`）:

```
EXPLAIN (COSTS OFF) SELECT 1                                        →  Result
EXPLAIN SELECT 1                                                    →  Result  (cost=0.00..0.01 rows=1 width=4)
EXPLAIN (SUMMARY ON, COSTS OFF) SELECT 1                            →  Result
                                                                       Planning Time: 0.008 ms
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT 1      →  Result (actual rows=1 loops=1)
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY ON) SELECT 1       →  Result (actual rows=1 loops=1)
                                                                       Planning Time: 0.002 ms
                                                                       Execution Time: 0.002 ms
EXPLAIN (ANALYZE, COSTS OFF, TIMING ON, SUMMARY OFF) SELECT 1       →  Result (actual time=0.000..0.000 rows=1 loops=1)
EXPLAIN (ANALYZE, COSTS OFF) SELECT 1                               →  Result (actual time=0.000..0.001 rows=1 loops=1)
                                                                       Planning Time: 0.002 ms
                                                                       Execution Time: 0.003 ms
EXPLAIN ANALYZE VERBOSE SELECT 1                                    →  Result  (cost=0.00..0.01 rows=1 width=4) (actual time=0.000..0.000 rows=1 loops=1)
                                                                         Output: 1
                                                                       Planning Time: 0.001 ms
                                                                       Execution Time: 0.001 ms
```

- ノードの名前の後は、`COSTS` が on なら**空白 2 つ**で `(cost=...)`、続けて ANALYZE なら**空白 1 つ**で `(actual ...)`。`COSTS OFF` なら名前の後に**空白 1 つ**で `(actual ...)`。
- 結果は 1 列 `QUERY PLAN`（型 text、OID 25）、1 行が出力の 1 行。コマンドタグは `EXPLAIN`。

### 3.2 ExplainNode の拡張

00 §9.3 の `ExplainNode` を次のように置き換える（00 への変更提案 1）。名前は変えず、フィールドを足す。

```rust
// planner/physical.rs（A が型を置く。中身を作るのは L2、読むのは E1）
pub struct ExplainNode {
    /// "Seq Scan on t"、"Hash Join"、"HashAggregate" など。コストと actual は含まない
    pub title: String,
    /// 詳細行。出す順に並べる
    pub details: Vec<ExplainDetail>,
    /// VERBOSE のときだけ作る Output 行の各要素（"a", "(b + 1)" など）
    pub output: Vec<String>,
    /// 子。通常の子（"->  "）とラベルつきの子（InitPlan / SubPlan / CTE）を、出力する順に並べる
    pub children: Vec<ExplainChild>,
    /// 計測値の持ち主（PhysicalPlan の先行順の通し番号。§3.10）
    pub exec_id: usize,
    /// コスト欄の width（出力列の平均幅の和。§3.9）
    pub width: u32,
}
pub struct ExplainDetail {
    /// "Filter"、"Hash Cond"、"Sort Key" など
    pub label: &'static str,
    pub text: String,
    /// ANALYZE のとき、この行の直後に "Rows Removed by ..." を出す元
    pub removed: Option<RemovedRows>,
}
pub struct RemovedRows {
    pub label: &'static str,
    /// 合計する計測値（exec_id, どちらのカウンタか）。`Filter` の群では通常 1 つ
    pub sources: Vec<(usize, FilterCounter)>,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FilterCounter { Filter, JoinFilter }
pub struct ExplainChild { pub label: Option<String>, pub node: ExplainNode }
```

- `exec_id` の付け方: `PhysicalPlan` の先行順（根が 0、子は左から）で全ノードに通し番号を振る。続けて `PhysicalQuery.subplans[i].plan`（i の昇順）、最後に `PhysicalQuery.ctes[i]`（i の昇順）の木に、同じ規則で続きの番号を振る。**番号を振る関数は 1 つ**（`planner::physical::assign_exec_ids(&PhysicalQuery) -> ExecIds`）にして、L2 と `executor::build_instrumented`（§3.10）が共有する。
- 1 つの `ExplainNode` は物理ノードの「群」を表すことがある（§3.4）。`Filter` / `Project` が `X` を包む群の `exec_id` は**最も外側のノード**の番号。`Hash` のように物理ノードがないものは、中身（ビルド側の子）の番号を指す。
- `SubPlanDef.explain` と `PhysicalQuery.explain` は 00 のまま（`Option<ExplainNode>`）。

### 3.3 物理プラン → ExplainNode の対応表

`PhysicalPlan`（00 §9.2）の各ノードを、PostgreSQL の表示に対応させる。「群」とは、1 つの `ExplainNode` にまとまる物理ノードの並び。詳細行は PostgreSQL の出力順（上から）。式は §4 の deparse で文字列にする。**修飾**は §3.5 の 3 つの規則（O = Output、Q = スキャンの条件、U = 結合・集約・ソートの条件）のどれを使うか。

| 物理ノード | タイトル | 詳細行（この順） | 子 | 備考 |
|---|---|---|---|---|
| `SeqScan` | `Seq Scan on {rel}`（別名があれば `{rel} {alias}`） | `Filter: {filter}`（Q） | なし | `{rel}` は §3.6 |
| `IndexScan` | `Index Scan using {index} on {rel}`。`direction = Backward` は `Index Scan Backward using ...` | `Index Cond: {cond}`（Q。`keys` から §3.7）、`Filter: {filter}`（Q） | なし | キーが空（順序のためだけの走査）なら `Index Cond` を出さない |
| `FunctionScan` | `Function Scan on {func}`（別名があれば `{func} {alias}`）。VERBOSE は `pg_catalog.{func}` | VERBOSE のとき `Function Call: {func}({args})`。`Filter:`（外側の `Filter` 群、Q） | なし | 別名が関数名と同じなら付けない |
| `Values` | `Values Scan on "*VALUES*"`（別名があれば `Values Scan on {alias}`） | `Filter:`（Q） | なし | **INSERT の 1 行だけの VALUES で別名なし**は `Result`（詳細なし） |
| `Result` | `Result` | `One-Time Filter: {one_time_filter}`（U） | なし（FROM なし） | `exprs` は `Output` |
| `Filter` | （吸収） | 子の群の `Filter` 行として足す | 子 | 群の外側。§3.4 |
| `Project` | （吸収） | `Output:` を置き換える（§3.8） | 子 | 群の外側 |
| `Sort` | `Sort` | `Sort Key: k1, k2 [DESC] [NULLS FIRST\|LAST]`（U） | 子 | §3.7 |
| `Unique` | `Unique` | なし | 子 | |
| `Distinct` | `HashAggregate` | `Group Key: {出力列すべて}`（U） | 子 | PostgreSQL の `SELECT DISTINCT` と `UNION`（ALL なし）の表示に合わせる |
| `Limit` | `Limit` | なし | 子 | |
| `Materialize` | `Materialize` | なし | 子 | |
| `NestedLoopJoin` | `Nested Loop`（`kind` により `Nested Loop Left Join` `Nested Loop Full Join` `Nested Loop Semi Join` `Nested Loop Anti Join`） | `Join Filter: {join_filter}`（U。`removed` = JoinFilter） | outer、inner（表示の順） | |
| `NestedLoopParam` | `Nested Loop`（同上） | `Join Filter: {join_filter}`（U） | outer、inner | inner の `Index Cond` に外側の列が `u.a` の形で出る（§3.7） |
| `HashJoin` | `Hash Join`（`kind` と `build_is_left` により下の表） | `Hash Cond: {cond}`（U）、`Join Filter: {residual}`（U。`removed` = JoinFilter） | **プローブ側**、`Hash`（合成。子はビルド側） | |
| `Aggregate` | `Aggregate` | `Filter:`（外側の `Filter` 群が HAVING。U） | 子 | |
| `HashAggregate` | `HashAggregate` | `Group Key: {keys}`（U。キーの式そのもの）、`Filter:`（HAVING） | 子 | |
| `GroupAggregate` | `GroupAggregate` | `Group Key: {keys}`、`Filter:` | 子（`Sort`） | |
| `Append` | `Append` | なし | `inputs` の順 | |
| `HashSetOp` | `HashSetOp Intersect` / `HashSetOp Except`（`all` なら末尾に ` All`） | なし | 合成の `Append`（子は合成の `Subquery Scan on "*SELECT* 1"` と `... "*SELECT* 2"`。それぞれの子が left / right） | PostgreSQL の形に合わせる。PostgreSQL は `INTERSECT` で腕を並べ替えることがある（既知の差） |
| `CteScan` | `CTE Scan on {cte}`（別名があれば `{cte} {alias}`） | `Filter:`（Q） | なし | CTE の本体は §3.8 の位置に `CTE {name}` のラベルつきで出す |
| `Insert` | `Insert on {rel}` | なし | 入力の群 | RETURNING があれば `Output` |
| `Update` | `Update on {rel}` | なし | 入力の群 | |
| `Delete` | `Delete on {rel}` | なし | 入力の群 | |

**表示にだけ現れるノード**（物理ノードがなく、`exec_id` で別のノードの計測値を借りる）:

| 表示ノード | 出す条件 | 計測値 |
|---|---|---|
| `Hash` | `HashJoin` の 2 つ目の子として常に | ビルド側の子の `exec_id` |
| `Append`（合成） / `Subquery Scan on "*SELECT* n"`（合成） | `HashSetOp` の子 | `Append` は `HashSetOp` の 2 つの子の合計（`rows` を足す。`loops` は 1）、`Subquery Scan` はそれぞれの子 |

**`join` の種類とタイトル**（PostgreSQL 17 [実機] と一致させる）:

| `kind` | `build_is_left = false`（左がプローブ） | `build_is_left = true`（左がビルド） |
|---|---|---|
| `Inner` | `Hash Join` | `Hash Join` |
| `Left` | `Hash Left Join` | `Hash Right Join` |
| `Full` | `Hash Full Join` | `Hash Full Join` |
| `Semi` | `Hash Semi Join` | `Hash Right Semi Join` |
| `Anti` | `Hash Anti Join` | `Hash Right Anti Join` |

`build_is_left = true` のとき、表示の子の順は「右の子（プローブ）、`Hash`（左の子がビルド）」、`Hash Cond` は「プローブ側のキー = ビルド側のキー」の順に書く。`build_is_left = false` のときは「左の子（プローブ）、`Hash`（右の子がビルド）」。出力の列の並び（左 ++ 右）は物理プランのままで、`Output` の並びも変えない。PostgreSQL の `t RIGHT JOIN u` が `Hash Left Join`（`u` がプローブ、`t` がビルド）と表示されるのと、yuzhu が RIGHT を LEFT に直して `u` を左にしたときの表示が一致する [実機]。

**`NestedLoopJoin` / `NestedLoopParam` の `kind`**: `Inner` は `Nested Loop`、`Left` は `Nested Loop Left Join`、`Full`・`Semi`・`Anti` は同様に `Nested Loop Full Join` / `Semi Join` / `Anti Join`。

### 3.4 群（Filter・Project の吸収）の規則

`explain_tree.rs` は物理プランを根から再帰して `ExplainNode` を作る。

```rust
// planner/explain_tree.rs（L2）
pub struct ExplainBuildCtx<'a> {
    pub arena: &'a ColumnArena,
    pub verbose: bool,
    /// 文全体の範囲表の数（§3.5。explain::node::count_rtes）
    pub n_rtable: usize,
    pub type_env: &'a TypeEnv<'a>,
    pub catalog: &'a dyn CatalogReader,
    /// ColId → 表示（§3.8）。ノードを下から作るたびに足す
    pub names: HashMap<ColId, ColText>,
}
```

1. **`Filter { input, predicate }`**: `input` の群を作り、`predicate` を群の `Filter:` 行として、その群の PostgreSQL の位置に足す。`input` がすでに `Filter:` 行を持つ（`SeqScan.filter` など）なら `(a) AND (b)` ではなく **`AND` を 1 つの式にまとめて**から deparse する（`Filter: ((x) AND (y))`。PostgreSQL の述語の並びと同じ）。`removed` の `sources` は、外側の `Filter` ノードの `(exec_id, FilterCounter::Filter)` に、内側のノードが自分の `filter` を持つ場合はそのノードの `(exec_id, FilterCounter::Filter)` を加えたもの（2 つの述語を 1 つの `Filter:` 行にまとめるので、数も合計して `loops` で割る）。物理化が `Filter` を `SeqScan.filter` に畳み込めるときは内側の 1 つだけになる。
2. **`Project { input, exprs }`**: `input` の群を作り、群の `Output` を `exprs` の deparse に置き換える（群の出力列 = `exprs`）。群の外から見ると、`Project` の出力列が群の出力列。`ExplainNode.exec_id` は `Project` の番号。
3. 群の `exec_id` は最も外側のノード。`Filter` が `Project` を包んでも `Project` が `Filter` を包んでもよい。
4. **`Result`** は群にならない（`Result` ノードが表示される）。ただし `Project` が `Result`（FROM なし）の上にあるときは、`Project` を吸収せず `Result` の `exprs` を `Output` にする（物理化は FROM なしの `SELECT` を `Result { exprs }` にするので、実際にはこの形で来る）。
5. 子の並び（`children`）: **InitPlan / CTE のラベルつきの子**（その問い合わせ階層の根のノードだけが持つ）→ **通常の子**（表示の順。上の表）→ **SubPlan のラベルつきの子**（その式を表示したノードが持つ）。PostgreSQL と同じ [実機]（例: §3.11 の 8、9）。

### 3.5 修飾（`t.a` か `a` か）の 3 つの規則

PostgreSQL の `useprefix`（PG:src/backend/commands/explain.c）は表示する行によって違う [実機]。`explain_tree.rs` は次の表を実装する。`n_rtable` は文全体の範囲表（`Rte`）の数（JOIN の `Rte`・FROM 句の副問い合わせ・VALUES・CTE 参照・サブクエリ内のものを含み、**インライン展開で消えたものも数える**）。

| 規則 | 対象の行 | 修飾する条件 |
|---|---|---|
| **O** | `Output:` の各要素 | `n_rtable > 1` |
| **Q** | スキャンノードの `Filter:`・`Index Cond:`・`Recheck Cond:`、`Function Call:`、`Tid Cond:` | `verbose` |
| **U** | `Sort Key`・`Group Key`・`Hash Cond`・`Join Filter`・結合と集約の `Filter:`・`One-Time Filter` | `verbose || n_rtable > 1` |

例 [実機]（単一表）:

```
EXPLAIN (COSTS OFF, VERBOSE) SELECT b, count(*) FROM t GROUP BY b HAVING sum(a) > 5 AND b > 1;
HashAggregate
  Output: b, count(*)               ← O: n_rtable = 1 なので修飾しない
  Group Key: t.b                    ← U: verbose なので修飾する
  Filter: (sum(t.a) > 5)            ← U
  ->  Seq Scan on public.t
        Output: a, b, c
        Filter: (t.b > 1)           ← Q: verbose なので修飾する
```

- **外側の列を参照するパラメータ**（`NestedLoopParam` の内側の `Index Cond: (a = u.a)`、相関サブクエリの `Filter: (a = t.a)`）は、**`verbose` や `n_rtable` に関係なく常に修飾する**（`u.a`、`t.a`）[実機]。内側の自分の列は規則 Q に従う。
- 範囲表の数の数え方は `explain::node::count_rtes(&BoundStatement) -> usize`（E1 が書く。`Rte` の総数を再帰で数える）。**FROM 句の副問い合わせを引き上げた後の `SELECT * FROM (SELECT a, b FROM t WHERE b > 5) s` は `n_rtable = 2`（`s` と `t`）で、VERBOSE の `Output` は `t.a, t.b` になる** [実機]。
- PostgreSQL の `Subquery Scan on s` の `Filter` は修飾する（`Filter: (s.a > 1)`）[実機]。yuzhu は `Subquery Scan` を作らない（既知の差。§3.12）ので規則 Q のままでよい。

### 3.6 リレーション名・別名・インデックス名

- **スキャンの対象**（`Seq Scan on ...`、`Index Scan using i on ...`）: `quote_identifier(relname)`（§4.7）。`verbose` のときは `quote_identifier(schema).quote_identifier(relname)`（`public.t`）。別名があり、**リレーション名と違えば** ` {alias}` を続ける（`Seq Scan on public.t x`）[実機]。インデックス名は修飾しない（`using t_pkey`）。
- **同じ別名が 1 つの計画に 2 回以上出る**（CTE のインライン展開で同じ表が 2 回など）ときは、2 回目以降を `{名前}_{n}` にする（PostgreSQL の `set_rtable_names` と同じ。最初の出現は元の名前、2 回目は `_1`、3 回目は `_2`。`_1` がすでに使われていれば次の番号）。`Seq Scan on t` と `Seq Scan on t t_1`、条件の中でも `t_1.a` [実機]。
- **CTE スキャン**: `CTE Scan on c`、別名があれば `CTE Scan on c c1`。
- **`Values`**: 別名なしは `"*VALUES*"`（引用符つき。`Values Scan on "*VALUES*"`）。
- **FunctionScan**: `generate_series`。別名なしで関数名と同じなら別名を付けない。VERBOSE は `Function Scan on pg_catalog.generate_series g`、`Output: g`、`Function Call: generate_series(1, 10)` [実機]。

### 3.7 詳細行の書式

**Sort Key**（`Sort.keys`）: `k1, k2 DESC, k3 NULLS FIRST`。各キーは式の deparse（列なら列名、式なら `((b + 1))` のように**子の計算列は括弧で包む**。§3.8）。`descending` なら ` DESC`。`nulls_first` が既定（ASC は NULLS LAST、DESC は NULLS FIRST）と違うときだけ ` NULLS FIRST` / ` NULLS LAST` を付ける [実機]:

```
Sort Key: t.b NULLS FIRST, t.c DESC NULLS LAST, t.a DESC     ← ORDER BY b ASC NULLS FIRST, c DESC NULLS LAST, a DESC
Sort Key: b DESC, c                                          ← ORDER BY b DESC NULLS FIRST, c
```

**Group Key**: キーの式そのものを deparse（子の計算列を指すのではなく、元の式。括弧は式自身のもの）。`Group Key: (b + 1)`（Sort Key では `((b + 1))`）[実機]。複数は `, ` 区切り。

**Hash Cond**: `left_keys[i] = right_keys[i]` を 1 つずつ `(l = r)` にし、複数なら `AND` で連結して全体を括弧で包む: `((t.a = u.a) AND (t.c = u.x))`。1 つなら `(u.a = t.a)`。順序は「プローブ側のキー = ビルド側のキー」（§3.3）。等号の演算子は `key_types` の型の `=`。

**Join Filter**: 結合の `residual` / `join_filter`。

**Index Cond**（`IndexScanKeys`）: 各条件を `({列} {演算子} {値})` にして並べる。順序: `eq` の各キー（インデックスの列順）→ `lower` → `upper`。1 つなら条件そのまま、複数なら `AND` で連結して外側を括弧で包む。

| キー | 書式 |
|---|---|
| `Eq(e)` | `(a = {e})` |
| `IsNull` | `(a IS NULL)` |
| `lower`（含む / 含まない） | `(a >= e)` / `(a > e)` |
| `upper`（含む / 含まない） | `(a <= e)` / `(a < e)` |

```
Index Cond: (a = 3)
Index Cond: ((a >= 3) AND (a <= 5))
Index Cond: ((a > 3) AND (a < 5))
Index Cond: (a IS NULL)
Index Cond: (a = u.a)               ← NestedLoopParam。値は外側の列（常に修飾）
```

DESC のインデックス列でも条件の書式は同じ（演算子は列の論理的な向き）。

**One-Time Filter**: `Result.one_time_filter`。偽の定数なら `false`（`One-Time Filter: false`）。

**Rows Removed**: §3.10。

### 3.8 VERBOSE の Output 行と、列の表示（`ColText`）

`Output:` は `verbose` のときだけ作る。**各ノードの出力列**を、次のように文字列にして `, ` で連ねる。

```rust
// deparse/mod.rs（00 の ColumnNamer の具体化。§4.1）
pub struct ColText {
    /// 修飾の判断を済ませた文字列。列なら "t.a" か "a"、計算列ならその式の deparse
    pub text: String,
    /// true なら参照するとき `(` `)` で包む（計算列。PostgreSQL が子の式を展開するときの規則）
    pub wrap: bool,
}
```

- ベーステーブルの列: `ColText { text: 修飾規則に従った名前, wrap: false }`。
- **計算列**（`Project` の式、集約の結果、`GROUP BY` の式）: `ColText { text: 式の deparse, wrap: true }`。**上のノードがこの列を参照するとき**（`Output` の要素、`Sort Key`、`Hash Cond` など）は `(` + text + `)` と包む。式自身の括弧と合わせて二重になる [実機]:

```
Sort                                       ← ORDER BY x（x = b + 1）
  Output: a, ((b + 1))                     ← Sort の Output は子の列の参照: ((b + 1))
  Sort Key: ((t.b + 1))                    ← 参照: ((b + 1))
  ->  Seq Scan on public.t
        Output: a, (b + 1)                 ← 定義するノード（Project を吸収した群）は包まない
```

```
Sort
  Output: b, (count(*))                    ← 集約の結果を参照: (count(*))
  Sort Key: t.b
  ->  HashAggregate
        Output: b, count(*)                ← 集約ノード自身は定義: count(*)
        Group Key: t.b
```

- **定義するノード**: `Project` を吸収した群は `exprs` を、`Aggregate` / `HashAggregate` / `GroupAggregate` は `keys` の式と集約の式を、そのまま deparse（包まない）。
- **それ以外のノード**（`Sort`、`Limit`、`Hash`、結合、`Unique` など）の `Output` は、**子（結合は左 ++ 右）の出力列の参照**を並べたもの。
- スキャン（`SeqScan`、`IndexScan`、`FunctionScan`、`CteScan`、`Values`）: ユーザー列の全部（`Project` で刈り込まれていれば残った列）。`system_columns` は `ctid` などの名前を末尾に付ける（`Output: a, ctid`）。
- **DML**: `Insert on t` は `RETURNING` があるときだけ `Output:`（`returning` の式）。`Update` の入力のスキャンの `Output` は `{代入した列の新しい値の式}, ctid`（古い列の値は出さない。PostgreSQL 14 以降の形 [実機]）、`Delete` の入力のスキャンの `Output` は `ctid` だけ:

```
Update on public.t
  ->  Index Scan using t_pkey on public.t
        Output: (b + 1), ctid
        Index Cond: (t.a = 5)
Delete on public.t
  ->  Seq Scan on public.t
        Output: ctid
        Filter: (t.a > 500)
```

- 集約の式: `count(*)`、`sum(a)`、`count(DISTINCT b)`、`count(*) FILTER (WHERE (b > 2))`、`max((a + 1))`、`bool_and((b > 1))` [実機]（§4.4）。
- `Output:` に出さないノード: `Append`（PostgreSQL は出さない）、`Result` 以外で出力列が 0 個のもの。
- **`Inner Unique: true`**（結合ノードの VERBOSE の行）と、Hash の `Buckets`・`Batches` の行は出さない（既知の差）。

### 3.9 コスト欄

`costs` が true のとき、**すべてのノード**（InitPlan / SubPlan / CTE の子を含む）のタイトルの後に、空白 2 つと `(cost=0.00..0.00 rows=0 width={W})` を付ける。`W` は `ExplainNode.width`（`ExplainBuildCtx` が計算する）:

- ノードの出力列の型ごとの幅を足す。`bool` 1、`int2` 2、`int4`・`float4`・`date`・`oid`・`regclass` 4、`int8`・`float8`・`timestamp`・`timestamptz` 8、`numeric` 32、`text`・`varchar`・`name` と typmod なしの可変長 32、`varchar(n)` / `char(n)`（`n` 文字）は `n`（上限 `32 + (min(n, 1000) - 32) / 2`、`n <= 32` は `n`）。
- `Hash` は子と同じ。`Insert` / `Update` / `Delete` は 0。
- **値に意味はなく、PostgreSQL と一致させる必要もない**（`rows=0`、`cost=0.00..0.00`）。ツールが書式を解析して落ちないためのもの。テストは常に `COSTS OFF`（§8）。

### 3.10 ANALYZE と計測

**実行の流れ**（`Session` の `exec_explain`、S の担当）:

1. 内側の文を解析（`analyzer::analyze` の結果の `BoundExplain.inner`）し、`planner::plan`（`want_explain = true`）で `PhysicalQuery` を作る。ここまでを計って `planning_time` とする。
2. `analyze = false`: 実行せず、`PhysicalQuery.explain` を描画する。**DML でも書かない**（書き込みロックも取らない）。読み取り専用トランザクションでも `EXPLAIN INSERT ...` は通る [実機]。
3. `analyze = true`: 実際に実行する。**SELECT は結果の行を捨てる。INSERT / UPDATE / DELETE は実際に書く**（トランザクション内なら ROLLBACK で戻る。外ならその文の終わりにコミット）。読み取り専用トランザクションでの DML は `25006 cannot execute INSERT in a read-only transaction` [実機]。書き込みロックは通常の DML と同じ規則で取る（`write_statement_tag` が `Explain` の `analyze` かつ内側が DML のときに内側のタグを返す）。実行時間（`ExecutorStart` から `ExecutorEnd` まで。コミットは含まない）を `execution_time` とする。`statement_timeout` とキャンセルは通常と同じ。
4. 描画（`explain::render_plan`）して行を `QUERY PLAN` の `data_row` として送り、タグは `EXPLAIN`。

**計測**（`executor/instrument.rs`、E1）:

```rust
/// 文の間 1 つ。ノードごとの計測値（Rc<RefCell<..>> か Cell。ExecCtx と同じスレッドだけで使う）
pub struct Instrumentation { nodes: Vec<NodeCounters> }
#[derive(Default)]
pub struct NodeCounters {
    /// 現在のループ（rewind から次の rewind まで）の計測
    running: bool, cycle_total: Duration, cycle_first: Duration, cycle_rows: u64,
    /// InstrEndLoop で足し込んだ合計
    pub loops: u64, pub rows: u64, pub startup: Duration, pub total: Duration,
    /// スキャン・結合・Filter が内部で数える（FilterCounter::Filter / JoinFilter）
    pub removed_filter: u64, pub removed_join_filter: u64,
}
impl Instrumentation {
    pub fn new(n_nodes: usize) -> Self;
    /// 描画の前に全ノードで呼ぶ（running なら EndLoop）
    pub fn finish(&self);
    pub fn node(&self, id: usize) -> &NodeCounters;
    pub fn add_removed(&self, id: usize, counter: FilterCounter, n: u64);
}

/// 計測つきの Executor の木を作る。`build` と同じ形で、各ノードを Instrumented で包む
pub fn build_instrumented(q: &PhysicalQuery, plan: &PhysicalPlan, ids: &mut ExecIdCursor, instr: &Rc<Instrumentation>, timing: bool) -> BoxedExecutor;
pub struct Instrumented { /* inner: BoxedExecutor, id, instr, timing */ }
```

- `executor::build`（計測なし）の `Executor` に **`fn set_counters(&mut self, _id: usize, _instr: &Rc<Instrumentation>) {}`**（既定は何もしない）を足す（00 への変更提案 2）。`Filter`・`SeqScan`・`IndexScan`・`NestedLoopJoin`・`NestedLoopParam`・`HashJoin` の executor が override し、述語が false の行を数えて `instr.add_removed(id, counter, 1)` を呼ぶ。`SeqScan` / `IndexScan` / `Filter` と、結合の「`Filter`」群は `FilterCounter::Filter`、結合の `join_filter` / `residual` で落ちた行は `JoinFilter`。ANALYZE でないときは呼ばれない（`Option` を持ち、`None` なら数えない）。
- SubPlan / InitPlan / CTE の子の executor は、`SubPlanStates` が作る時点で `build_instrumented` を呼ぶ（`ExecCtx.instr: Option<Rc<Instrumentation>>` を足す。00 への変更提案 2）。
- **計測の意味**（PostgreSQL の `Instrumentation`、PG:src/backend/executor/instrument.c と同じ）:
  - `Instrumented::next`: `timing` なら呼び出しの前後で `Instant` を取り `cycle_total` に加える。最初の `next` が戻った時点で（行でも `None` でも）`running = true`、`cycle_first = cycle_total`。行が返れば `cycle_rows += 1`。
  - `Instrumented::rewind`: `running` なら EndLoop（下）してから `inner.rewind`。
  - EndLoop: `running` でなければ何もしない。`loops += 1; rows += cycle_rows; startup += cycle_first; total += cycle_total;` 各 `cycle_*` を 0、`running = false`。
  - `finish`: 全ノードで EndLoop。
  - **`loops` は「実際に `next` が呼ばれたループの数」**。1 回も `next` が呼ばれなかったノードは `loops = 0` で `(never executed)`。
- `timing = false`（`TIMING OFF`）なら `Instant` を取らない（`startup` `total` は 0 のまま。出力にも出さない）。

**描画**（`explain/format.rs`）の ANALYZE の部分:

```
{title}  (cost=...)  (actual time={startup_ms}..{total_ms} rows={rows} loops={loops})     ← timing
{title}  (cost=...)  (actual rows={rows} loops={loops})                                    ← timing off
{title}  (cost=...)  (never executed)                                                      ← loops = 0
```

- `startup_ms = startup.as_secs_f64() * 1000 / loops`、`total_ms = total... / loops`（**`loops` で割った 1 ループ平均**）。`{:.3}`。`rows = rows_total / loops` を **`{:.0}`**（四捨五入。PostgreSQL 17 は小数を出さない [実機]: `rows=2 loops=100`）。
- `Hash`（合成）など `SameAs` のノード: 対応する `exec_id` の値をそのまま使う。
- **`Rows Removed by Filter: N` / `Rows Removed by Join Filter: N`**: `ExplainDetail.removed` を持つ詳細行の**直後**に、同じ字下げで出す。`N = removed_* / loops`（`{:.0}`）。**`removed_* > 0` のときだけ**出す（`never executed` では出さない）[実機]:

```
Seq Scan on t (actual rows=400 loops=1)
  Filter: (b > 5)
  Rows Removed by Filter: 600
```

```
Nested Loop (actual rows=20 loops=1)
  Join Filter: (t.a < u.a)
  Rows Removed by Join Filter: 5980
  ->  Seq Scan on t (actual rows=100 loops=1)
        Filter: (b = 1)
        Rows Removed by Filter: 900
  ->  Materialize (actual rows=60 loops=100)
        ->  Seq Scan on u (actual rows=60 loops=1)
              Filter: (a < 3)
              Rows Removed by Filter: 1940
```

- `Planning Time: {:.3} ms` と `Execution Time: {:.3} ms` は、`summary` が true のとき、プランの行の後に（この順に）出す。`analyze = false` のときは `Planning Time` だけ [実機]。`summary = false` なら出さない。
- DML の最上位ノード（`Insert on t` など）の `rows` は **`RETURNING` の行数**（なければ 0）、子は通常の計測 [実機]: `Insert on u (actual rows=0 loops=1)` / `->  Result (actual rows=1 loops=1)`。`INSERT ... RETURNING` は `Insert on u (actual rows=1 loops=1)`。

### 3.11 実機で確かめた出力例（PostgreSQL 17.11）

表: `t(a int primary key, b int, c text)` 1000 行、`u(a int, x text)` 2000 行と `create index u_a_idx on u(a)`、`u2(b int)`。PostgreSQL 側は `set enable_bitmapscan = off; set enable_indexonlyscan = off; set enable_memoize = off; set enable_mergejoin = off;`（§8.2）。**yuzhu の出力がこれと一致するもの**は `✔`、**PostgreSQL のコスト選択で形が変わるもの**は `△`（yuzhu の期待を付記）。

**1. Result、Values、FunctionScan** ✔

```
EXPLAIN (COSTS OFF) SELECT 1 WHERE false;
Result
  One-Time Filter: false

EXPLAIN (COSTS OFF) VALUES (1,'a'),(2,'b');
Values Scan on "*VALUES*"

EXPLAIN (COSTS OFF, VERBOSE) VALUES (1,'a'),(2,'b');
Values Scan on "*VALUES*"
  Output: column1, column2

EXPLAIN (COSTS OFF) SELECT * FROM (VALUES (1),(2)) v(x) WHERE x > 1;
Values Scan on "*VALUES*"
  Filter: (column1 > 1)

EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM generate_series(1,10) g;
Function Scan on pg_catalog.generate_series g
  Output: g
  Function Call: generate_series(1, 10)

EXPLAIN (COSTS OFF) SELECT g FROM generate_series(1,10) g WHERE g > 3;
Function Scan on generate_series g
  Filter: (g > 3)
```

（`(VALUES ...) v(x)` の `Values Scan on "*VALUES*"` と `column1` は PostgreSQL が別名 `v` を引き上げた後の形。yuzhu は別名があるので `Values Scan on v` と `Filter: (x > 1)` を出す。既知の差。）

**2. 単一表のスキャンと VERBOSE** ✔

```
EXPLAIN (COSTS OFF) SELECT * FROM t WHERE b > 5;
Seq Scan on t
  Filter: (b > 5)

EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM t WHERE b > 5;
Seq Scan on public.t
  Output: a, b, c
  Filter: (t.b > 5)

EXPLAIN (COSTS OFF, VERBOSE) SELECT x.a FROM t x WHERE x.b = 1;
Seq Scan on public.t x
  Output: a
  Filter: (x.b = 1)
```

**3. インデックススキャン** ✔（PostgreSQL は `set enable_seqscan = off` を付けたとき）

```
EXPLAIN (COSTS OFF) SELECT * FROM u WHERE a = 3 AND x = 'y';
Index Scan using u_a_idx on u
  Index Cond: (a = 3)
  Filter: (x = 'y'::text)

EXPLAIN (COSTS OFF) SELECT * FROM u WHERE a BETWEEN 3 AND 5;
Index Scan using u_a_idx on u
  Index Cond: ((a >= 3) AND (a <= 5))

EXPLAIN (COSTS OFF) SELECT * FROM u WHERE a > 3 AND a < 5 AND x = 'q';
Index Scan using u_a_idx on u
  Index Cond: ((a > 3) AND (a < 5))
  Filter: (x = 'q'::text)

EXPLAIN (COSTS OFF) SELECT * FROM u WHERE a IS NULL;
Index Scan using u_a_idx on u
  Index Cond: (a IS NULL)

EXPLAIN (COSTS OFF) SELECT * FROM u ORDER BY a DESC;
Index Scan Backward using u_a_idx on u

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE a > 3 ORDER BY a LIMIT 5 OFFSET 2;
Limit
  ->  Index Scan using t_pkey on t
        Index Cond: (a > 3)

EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM u WHERE a = 3 AND x = 'y';
Index Scan using u_a_idx on public.u
  Output: a, x
  Index Cond: (u.a = 3)
  Filter: (u.x = 'y'::text)
```

（`ORDER BY a DESC` のインデックス順によるソートの省略は 00 D-18 の任意項目。yuzhu が省略しなければ `Sort` + `Seq Scan` になる。）

**4. 結合** ✔（`enable_hashjoin = off` で NestedLoop、通常は Hash Join）

```
EXPLAIN (COSTS OFF) SELECT * FROM t, u WHERE t.a = u.a AND t.b > 5 ORDER BY t.a;
Sort
  Sort Key: t.a
  ->  Hash Join
        Hash Cond: (u.a = t.a)
        ->  Seq Scan on u
        ->  Hash
              ->  Seq Scan on t
                    Filter: (b > 5)

EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM t, u WHERE t.a = u.a AND t.b > 5 ORDER BY t.a;
Sort
  Output: t.a, t.b, t.c, u.a, u.x
  Sort Key: t.a
  ->  Hash Join
        Output: t.a, t.b, t.c, u.a, u.x
        Inner Unique: true                      ← yuzhu は出さない
        Hash Cond: (u.a = t.a)
        ->  Seq Scan on public.u
              Output: u.a, u.x
        ->  Hash
              Output: t.a, t.b, t.c
              ->  Seq Scan on public.t
                    Output: t.a, t.b, t.c
                    Filter: (t.b > 5)

EXPLAIN (COSTS OFF) SELECT * FROM t JOIN u ON t.a = u.a AND u.x > t.c WHERE t.b = 1 AND (u.a > 3 OR t.b = 2);
Hash Join
  Hash Cond: (u.a = t.a)
  Join Filter: ((u.x > t.c) AND ((u.a > 3) OR (t.b = 2)))
  ->  Seq Scan on u
  ->  Hash
        ->  Seq Scan on t
              Filter: (b = 1)

EXPLAIN (COSTS OFF) SELECT * FROM t LEFT JOIN u ON t.a = u.a AND u.x > 'a';       -- enable_hashjoin = off, enable_indexscan = off
Nested Loop Left Join
  Join Filter: (t.a = u.a)
  ->  Seq Scan on t
  ->  Materialize
        ->  Seq Scan on u
              Filter: (x > 'a'::text)

EXPLAIN (COSTS OFF) SELECT * FROM t JOIN u ON t.a < u.a;                          -- enable_hashjoin = off, enable_indexscan = off
Nested Loop
  Join Filter: (t.a < u.a)
  ->  Seq Scan on u
  ->  Materialize
        ->  Seq Scan on t

EXPLAIN (COSTS OFF) SELECT * FROM t RIGHT JOIN u ON t.a = u.a;
Hash Left Join
  Hash Cond: (u.a = t.a)
  ->  Seq Scan on u
  ->  Hash
        ->  Seq Scan on t

EXPLAIN (COSTS OFF) SELECT * FROM t, u;
Nested Loop
  ->  Seq Scan on u
  ->  Materialize
        ->  Seq Scan on t
```

（`SELECT * FROM t JOIN u ON t.a < u.a` で PostgreSQL が外側を `u` にするのは、行数の見積もりによる。yuzhu の結合順は 04 の規則で、左に書いた表が外側になるのが基本で、一致しないことがある `△`。）

**5. インデックス付きネステッドループ（NestedLoopParam）** `△`

```
EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM t JOIN u ON t.a = u.a WHERE t.b = 1;    -- PG: enable_hashjoin = off
Nested Loop
  Output: t.a, t.b, t.c, u.a, u.x
  ->  Seq Scan on public.t
        Output: t.a, t.b, t.c
        Filter: (t.b = 1)
  ->  Index Scan using u_a_idx on public.u
        Output: u.a, u.x
        Index Cond: (u.a = t.a)               ← 外側の列（t.a）は常に修飾。内側の自分の列は規則 Q（verbose なので u.a）

EXPLAIN (COSTS OFF) SELECT * FROM t JOIN u ON t.a < u.a;                          -- PG: enable_hashjoin = off（enable_indexscan は on）
Nested Loop
  ->  Seq Scan on t
  ->  Index Scan using u_a_idx on u
        Index Cond: (a > t.a)
```

**6. 集約・DISTINCT・ソート・LIMIT** ✔

```
EXPLAIN (COSTS OFF) SELECT count(*), sum(a), avg(b)::int FROM t;
Aggregate
  ->  Seq Scan on t

EXPLAIN (COSTS OFF, VERBOSE) SELECT count(*), sum(a), avg(b)::int FROM t;
Aggregate
  Output: count(*), sum(a), (avg(b))::integer
  ->  Seq Scan on public.t
        Output: a, b, c

EXPLAIN (COSTS OFF) SELECT b, count(*) FROM t GROUP BY b HAVING sum(a) > 5 ORDER BY b;
Sort
  Sort Key: b
  ->  HashAggregate
        Group Key: b
        Filter: (sum(a) > 5)
        ->  Seq Scan on t

EXPLAIN (COSTS OFF, VERBOSE) SELECT b, count(*) FROM t GROUP BY b ORDER BY b;
Sort
  Output: b, (count(*))
  Sort Key: t.b
  ->  HashAggregate
        Output: b, count(*)
        Group Key: t.b
        ->  Seq Scan on public.t
              Output: a, b, c

EXPLAIN (COSTS OFF) SELECT DISTINCT b FROM t;                                      -- yuzhu: Distinct → HashAggregate
HashAggregate
  Group Key: b
  ->  Seq Scan on t

EXPLAIN (COSTS OFF) SELECT DISTINCT ON (b) b, c FROM t ORDER BY b, c;
Unique
  ->  Sort
        Sort Key: b, c
        ->  Seq Scan on t

SET enable_hashagg = off;
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM t GROUP BY b;
GroupAggregate
  Group Key: b
  ->  Sort
        Sort Key: b
        ->  Seq Scan on t

EXPLAIN (COSTS OFF) SELECT * FROM t ORDER BY b DESC NULLS FIRST, c LIMIT 3;
Limit
  ->  Sort
        Sort Key: b DESC, c
        ->  Seq Scan on t

EXPLAIN (COSTS OFF, VERBOSE) SELECT a, b+1 AS x FROM t ORDER BY x;
Sort
  Output: a, ((b + 1))
  Sort Key: ((t.b + 1))
  ->  Seq Scan on public.t
        Output: a, (b + 1)

EXPLAIN (COSTS OFF, VERBOSE) SELECT lower(c), upper(c) || 'x', length(c), c::varchar, a::text, now(), current_date, abs(-a) FROM t;
Seq Scan on public.t
  Output: lower(c), (upper(c) || 'x'::text), length(c), (c)::character varying, (a)::text, now(), CURRENT_DATE, abs((- a))
```

（`SELECT count(DISTINCT b) FROM t` は PostgreSQL では `Aggregate -> Sort -> Seq Scan` と表示される。yuzhu の `Aggregate` は DISTINCT を内部で処理するので `Aggregate -> Seq Scan` になる。既知の差。）

**7. 集合演算** ✔（`UNION ALL`・`EXCEPT`）`△`（`INTERSECT` の腕の順）

```
EXPLAIN (COSTS OFF) SELECT a FROM t UNION ALL SELECT a FROM u;
Append
  ->  Seq Scan on t
  ->  Seq Scan on u

EXPLAIN (COSTS OFF) SELECT a FROM t UNION SELECT a FROM u;
HashAggregate
  Group Key: t.a
  ->  Append
        ->  Seq Scan on t
        ->  Seq Scan on u

EXPLAIN (COSTS OFF) SELECT a FROM t EXCEPT ALL SELECT a FROM u;
HashSetOp Except All
  ->  Append
        ->  Subquery Scan on "*SELECT* 1"
              ->  Seq Scan on t
        ->  Subquery Scan on "*SELECT* 2"
              ->  Seq Scan on u

EXPLAIN (COSTS OFF) (SELECT a FROM t ORDER BY a LIMIT 3) UNION ALL SELECT a FROM u;
Append
  ->  Limit
        ->  Index Scan using t_pkey on t
  ->  Seq Scan on u
```

**8. サブクエリ（InitPlan / SubPlan）** ✔

```
EXPLAIN (COSTS OFF) SELECT * FROM t WHERE b = (SELECT max(a) FROM u2);          -- 非相関のスカラー
Seq Scan on t
  Filter: (b = (InitPlan 1).col1)
  InitPlan 1
    ->  Aggregate
          ->  Seq Scan on u2

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE b = (SELECT 1) + (SELECT 2);
Seq Scan on t
  Filter: (b = ((InitPlan 1).col1 + (InitPlan 2).col1))
  InitPlan 1
    ->  Result
  InitPlan 2
    ->  Result

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE b = (SELECT 1) ORDER BY a;            -- InitPlan は根の Sort が持つ
Sort
  Sort Key: t.a
  InitPlan 1
    ->  Result
  ->  Seq Scan on t
        Filter: (b = (InitPlan 1).col1)

EXPLAIN (COSTS OFF) SELECT a, (SELECT count(*) FROM u WHERE u.a = t.a) FROM t;   -- 相関のスカラー
Seq Scan on t
  SubPlan 1
    ->  Aggregate
          ->  Seq Scan on u
                Filter: (a = t.a)

EXPLAIN (COSTS OFF, VERBOSE) SELECT a, (SELECT count(*) FROM u WHERE u.a = t.a) FROM t;
Seq Scan on public.t
  Output: t.a, (SubPlan 1)
  SubPlan 1
    ->  Aggregate
          Output: count(*)
          ->  Seq Scan on public.u
                Output: u.a, u.x
                Filter: (u.a = t.a)

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE b = (SELECT a FROM u WHERE u.x = t.c LIMIT 1) OR a = 1;
Seq Scan on t
  Filter: ((b = (SubPlan 1)) OR (a = 1))
  SubPlan 1
    ->  Limit
          ->  Seq Scan on u
                Filter: (x = t.c)

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE a NOT IN (SELECT a FROM u WHERE u.x = t.c);        -- 相関の NOT IN
Seq Scan on t
  Filter: (NOT (ANY (a = (SubPlan 1).col1)))
  SubPlan 1
    ->  Seq Scan on u
          Filter: (x = t.c)

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE a NOT IN (SELECT a FROM u);                       -- 非相関の NOT IN: ハッシュ化
Seq Scan on t
  Filter: (NOT (ANY (a = (hashed SubPlan 1).col1)))
  SubPlan 1
    ->  Seq Scan on u

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE (a, b) IN (SELECT a, a FROM u) OR c = 'q';
Seq Scan on t
  Filter: ((ANY ((a = (hashed SubPlan 1).col1) AND (b = (hashed SubPlan 1).col2))) OR (c = 'q'::text))
  SubPlan 1
    ->  Seq Scan on u

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE b > ALL (SELECT a FROM u);
Seq Scan on t
  Filter: (ALL (b > (SubPlan 1).col1))
  SubPlan 1
    ->  Materialize                           ← yuzhu: SubPlan の根に Materialize を付けるかは 04 の規則
          ->  Seq Scan on u

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.a < t.a) OR b = 1;      -- 相関の EXISTS
Seq Scan on t
  Filter: (EXISTS(SubPlan 1) OR (b = 1))
  SubPlan 1
    ->  Seq Scan on u
          Filter: (a < t.a)

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.a < t.a) OR b = 1;
Seq Scan on t
  Filter: ((NOT EXISTS(SubPlan 1)) OR (b = 1))
  ...

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u) OR b = 1;                      -- 非相関の EXISTS は InitPlan の bool
Seq Scan on t
  Filter: ((InitPlan 1).col1 OR (b = 1))
  InitPlan 1
    ->  Seq Scan on u

EXPLAIN (COSTS OFF) SELECT t.a, (SELECT count(*) FROM u WHERE u.a = t.a) FROM t JOIN u2 ON t.b = u2.b;
Hash Join
  Hash Cond: (u2.b = t.b)
  ->  Seq Scan on u2
  ->  Hash
        ->  Seq Scan on t
  SubPlan 1                                   ← SubPlan は通常の子の後
    ->  Aggregate
          ->  Seq Scan on u
                Filter: (a = t.a)

EXPLAIN (COSTS OFF) SELECT * FROM t WHERE b IN (SELECT a FROM u) AND c = 'q';                    -- yuzhu: Semi 結合に書き換える（04）
Nested Loop Semi Join                         ← PG のこの例。yuzhu は Hash Semi Join になりうる
  ->  Seq Scan on t
        Filter: (c = 'q'::text)
  ->  Index Scan using u_a_idx on u
        Index Cond: (a = t.b)
```

- スカラー（`Scalar`）の SubPlan は `(SubPlan 1)`、InitPlan は `(InitPlan 1).col1`（出力の列番号。1 列なら `col1`）。`EXISTS` は SubPlan なら `EXISTS(SubPlan 1)`（`EXISTS(` の直後に `SubPlan 1`、空白なし）、InitPlan なら `(InitPlan 1).col1`。`Any` は `(ANY ({test}))`、`All` は `(ALL ({test}))` で、`test` の中の `SubLinkOutput(i)` を `(SubPlan n).col{i+1}`（ハッシュ化したものは `(hashed SubPlan n).col{i+1}`、InitPlan は `(InitPlan n).col{i+1}`）にする。`NOT IN` は `(NOT (ANY (...)))` [実機]。
- `n` は `PhysicalQuery.subplans` の添字 + 1（SubPlan と InitPlan が共通の番号。PostgreSQL は CTE と共通の番号を使うので、CTE があると 1 つずれる。既知の差）。
- **InitPlan のラベルつきの子**は、その問い合わせ階層（主問い合わせ、または 1 つの SubPlan の本体）の**根の `ExplainNode`** に、通常の子より前に置く。`SubPlan` のラベルつきの子は、**その式を表示したノード**に、通常の子より後に置く。`Strategy::Hashed` の SubPlan も同じ（ラベルが `SubPlan n` で、式の側が `hashed SubPlan n`）。
- InitPlan の根が `Aggregate` の `min` / `max` を PostgreSQL が `Limit -> Index Scan Backward` に書き換える最適化は yuzhu にない（既知の差）。

**9. CTE** ✔

```
EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT a, b FROM t WHERE b > 5) SELECT * FROM c WHERE a < 10;
CTE Scan on c
  Filter: (a < 10)
  CTE c
    ->  Seq Scan on t
          Filter: (b > 5)

EXPLAIN (COSTS OFF, VERBOSE) WITH c AS MATERIALIZED (SELECT a, b FROM t WHERE b > 5) SELECT * FROM c WHERE a < 10;
CTE Scan on c
  Output: c.a, c.b
  Filter: (c.a < 10)
  CTE c
    ->  Seq Scan on public.t
          Output: t.a, t.b
          Filter: (t.b > 5)

EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT a FROM t) SELECT * FROM c c1, c c2 WHERE c1.a = c2.a;
Hash Join
  Hash Cond: (c1.a = c2.a)
  CTE c
    ->  Seq Scan on t
  ->  CTE Scan on c c1
  ->  Hash
        ->  CTE Scan on c c2

EXPLAIN (COSTS OFF) WITH c AS NOT MATERIALIZED (SELECT a FROM t) SELECT * FROM c c1, c c2 WHERE c1.a = c2.a;
Hash Join
  Hash Cond: (t.a = t_1.a)
  ->  Seq Scan on t
  ->  Hash
        ->  Seq Scan on t t_1                ← 同じ別名の重複は t_1（§3.6）
```

- CTE の本体は、**`CteScan` の参照を含む問い合わせ階層の根の `ExplainNode`** に、`CTE {name}` のラベルつきの子として、InitPlan と同じ位置（通常の子より前。InitPlan と CTE が両方あれば番号順ではなく **CTE が先**）に 1 回だけ置く。2 つ目以降の `CTE Scan` は本体を出さない。

**10. DML** ✔

```
EXPLAIN (COSTS OFF) INSERT INTO u VALUES (1, 'a');
Insert on u
  ->  Result

EXPLAIN (COSTS OFF, VERBOSE) INSERT INTO u VALUES (1, 'a'), (2, 'b');
Insert on public.u
  ->  Values Scan on "*VALUES*"
        Output: "*VALUES*".column1, "*VALUES*".column2

EXPLAIN (COSTS OFF) INSERT INTO u SELECT a, c FROM t WHERE b = 1;
Insert on u
  ->  Seq Scan on t
        Filter: (b = 1)

EXPLAIN (COSTS OFF) UPDATE t SET b = b + 1 WHERE a = 5;
Update on t
  ->  Index Scan using t_pkey on t
        Index Cond: (a = 5)

EXPLAIN (COSTS OFF) UPDATE t SET b = u.a FROM u WHERE u.a = t.a AND u.x = 'z';       -- PG: §8.2 の設定のみ
Update on t
  ->  Nested Loop
        ->  Seq Scan on u
              Filter: (x = 'z'::text)
        ->  Index Scan using t_pkey on t
              Index Cond: (a = u.a)

EXPLAIN (COSTS OFF) UPDATE t SET b = (SELECT 1) WHERE a = 1;                         -- InitPlan は最上位の Update が持つ
Update on t
  InitPlan 1
    ->  Result
  ->  Index Scan using t_pkey on t
        Index Cond: (a = 1)

EXPLAIN (COSTS OFF) DELETE FROM t USING u WHERE t.a = u.a;
Delete on t
  ->  Hash Join
        Hash Cond: (u.a = t.a)
        ->  Seq Scan on u
        ->  Hash
              ->  Seq Scan on t

EXPLAIN (COSTS OFF, VERBOSE) INSERT INTO t VALUES (1000000, 1, 'z') RETURNING a, c;
Insert on public.t
  Output: t.a, t.c
  ->  Result
        Output: 1000000, 1, 'z'::text
```

**11. ANALYZE** ✔（`EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF)`。`Hash` の `Buckets` 行、`HashAggregate` の `Batches` 行、`Sort` の `Sort Method` 行は PostgreSQL が出し、yuzhu は出さない）

```
Seq Scan on t (actual rows=400 loops=1)
  Filter: (b > 5)
  Rows Removed by Filter: 600

Index Scan using t_pkey on t (actual rows=1 loops=1)
  Index Cond: (a = 5)

Limit (actual rows=3 loops=1)
  ->  Seq Scan on t (actual rows=3 loops=1)
        Filter: (b > 5)
        Rows Removed by Filter: 5

Nested Loop (actual rows=200 loops=1)                         ← enable_hashjoin = off
  ->  Seq Scan on t (actual rows=100 loops=1)
        Filter: (b = 1)
        Rows Removed by Filter: 900
  ->  Index Scan using u_a_idx on u (actual rows=2 loops=100)
        Index Cond: (a = t.a)

Seq Scan on t (actual rows=0 loops=1)
  Filter: ((ANY (a = (SubPlan 1).col1)) OR (b = 99999))
  Rows Removed by Filter: 1000
  SubPlan 1
    ->  Seq Scan on u (actual rows=0 loops=1000)
          Filter: (x = t.c)
          Rows Removed by Filter: 2000

Index Scan using t_pkey on t (actual rows=0 loops=1)
  Index Cond: (a < 0)
  Filter: (b = (InitPlan 2).col1)
  InitPlan 2
    ->  Result (never executed)
          ...

Insert on u (actual rows=0 loops=1)
  ->  Seq Scan on t (actual rows=100 loops=1)
        Filter: (b = 1)
        Rows Removed by Filter: 900
```

**補足（`Rows Removed` の数え方）**: `Seq Scan on t (actual rows=400 loops=1) / Rows Removed by Filter: 600` は `rows = 通った行数`、`removed = 落ちた行数`。ネステッドループの内側（`loops=100`）では**1 ループ平均**（`Rows Removed by Filter: 2000`、`loops=1000` のサブプランの例）。

### 3.12 PostgreSQL との既知の差（EXPLAIN）

| 差 | yuzhu | 備考 |
|---|---|---|
| コスト | `cost=0.00..0.00 rows=0 width=W` | §3.9 |
| プランの選び方 | 統計がないので 04 の規則（インデックスは使えるなら使う、結合順は書いた順など）。`Bitmap Heap Scan`・`Index Only Scan`・`Merge Join`・`Memoize`・`Gather`・`Incremental Sort`・`LockRows` を作らない | テストは PostgreSQL 側の `enable_*` で合わせる（§8.2） |
| `Subquery Scan` | FROM 句の副問い合わせ（引き上げられないもの）の `Subquery Scan on s` を作らない | 子が直接出る。`Filter` は子の側（または `Filter` 群として親側） |
| `Inner Unique`、`Buckets`、`Batches`、`Memory Usage`、`Sort Method`、`Heap Fetches` | 出さない | D10-6 |
| `Settings:`、`Planning:`（`MEMORY`）、`Serialization:`、`Buffers:` | 出さない | D10-7 |
| 式の簡約 | yuzhu の定数畳み込みは 04 の範囲。`NOT (b AND x > 1)` → `(NOT b) OR (x <= 1)` のような PostgreSQL の変形（否定の押し込み、`b OR b` の畳み込み、述語の並べ替え `b AND (i = 1) AND (t = 'a'::text)`）はしない | 式の文字列は同じ式なら同じ書式（§4） |
| InitPlan / SubPlan の番号 | CTE を数えない | |
| `INTERSECT` の腕の並び | 書いた順 | PostgreSQL はコストで並べ替える |
| `count(DISTINCT x)` | `Sort` を出さない | |
| `FORMAT JSON` など | `0A000` | |
| VERBOSE の `Output:` で関数名を修飾する場合 | ビルトインはすべて `pg_catalog` にあり、検索パスで見えるので修飾しない | `Function Scan on pg_catalog.generate_series` のタイトルだけが修飾 |

---

## 4. deparse（式の逆変換）

EXPLAIN の式表示と `pg_get_expr` が共有する。`deparse/` に置く（E1）。PostgreSQL の `ruleutils.c`（PG:src/backend/utils/adt/ruleutils.c）の規則を、M4 で作る式の範囲に限って写す。**この節の規則は、すべて PostgreSQL 17.11 に流して確かめた**（§4.9 に 150 件以上の対応表の抜粋）。

### 4.1 モジュールと API

```
deparse/
├── mod.rs        DeparseMode、DeparseOptions、ColText、ColumnNamer、SubLinkRenderer、DeparseCtx、deparse_expr
├── expr.rs       式の変種ごとの書式（§4.4〜§4.6）
├── literal.rs    定数（§4.3）
├── ident.rs      quote_identifier（§4.7）
├── typename.rs   format_type（§4.8）
└── stored.rs     pg_get_expr / pg_get_constraintdef / pg_get_indexdef（§4.8）
```

```rust
// deparse/mod.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeparseMode {
    /// 計画後の式（EXPLAIN）。定数畳み込み済みの形を前提にする
    Plan,
    /// 保存された式（pg_get_expr、制約の定義）。解析しただけで、畳み込んでいない
    Stored,
}

#[derive(Clone, Copy, Debug)]
pub struct DeparseOptions {
    pub mode: DeparseMode,
    /// PRETTYFLAG_PAREN: 括弧を最小にする（§4.6）。psql の \d が使う
    pub pretty_paren: bool,
    /// PRETTYFLAG_INDENT: CASE を複数行にする（§4.6）
    pub indent: bool,
}
impl DeparseOptions {
    /// EXPLAIN: 括弧は常に付ける、字下げなし（1 行）
    pub const EXPLAIN: DeparseOptions = DeparseOptions { mode: DeparseMode::Plan, pretty_paren: false, indent: false };
    /// pg_get_expr(.., pretty) と pg_get_constraintdef(.., pretty)。pretty = false でも indent は true（PostgreSQL の GET_PRETTY_FLAGS）
    pub const fn stored(pretty: bool) -> DeparseOptions {
        DeparseOptions { mode: DeparseMode::Stored, pretty_paren: pretty, indent: true }
    }
}

/// 列参照の表示（§3.8）
pub struct ColText { pub text: String, pub wrap: bool }

/// 列の型 C ごとに 1 つ実装する。修飾するかどうかの判断は実装が持つ（呼び出しごとに作る）
pub trait ColumnNamer<C> {
    fn name(&self, col: &C) -> Result<ColText>;
}

/// SubLink の副問い合わせ Q の表示用の名前。Stored では使わない（SubLink は Error::not_supported）
pub struct SubPlanLabel {
    /// "SubPlan 1" / "InitPlan 1"
    pub name: String,
    /// ハッシュ化した SubPlan（"(hashed SubPlan 1)"）
    pub hashed: bool,
    /// InitPlan か（"(InitPlan 1).col1" の形にする）
    pub init_plan: bool,
}
pub trait SubLinkRenderer<Q> {
    fn label(&self, q: &Q) -> Result<SubPlanLabel>;
}

pub struct DeparseCtx<'a, C, Q> {
    pub opts: DeparseOptions,
    pub namer: &'a dyn ColumnNamer<C>,
    pub sublinks: Option<&'a dyn SubLinkRenderer<Q>>,
    /// 定数の出力（DateStyle、extra_float_digits）に使う
    pub type_env: &'a TypeEnv<'a>,
    /// regclass 定数の名前、関数名の可視性に使う
    pub catalog: &'a dyn CatalogReader,
}

/// 式 1 つを文字列にする。根の式として扱う（§4.6 の「根の暗黙のキャストを隠す」は Stored のときこれだけ）
pub fn deparse_expr<C: Clone, Q: Clone>(e: &Expr<C, Q>, cx: &DeparseCtx<'_, C, Q>) -> Result<String>;
/// 式の並び（Group Key、Sort Key、Output）を要素ごとに文字列にする
pub fn deparse_list<C: Clone, Q: Clone>(es: &[Expr<C, Q>], cx: &DeparseCtx<'_, C, Q>) -> Result<Vec<String>>;
```

- 実装する `ColumnNamer`: `explain_tree.rs` の `ArenaNamer`（`ColId`、§3.8 の `names` を引く。L2 が書く）、`stored.rs` の `TableNamer`（`Var { rte: 0, col, levels_up: 0 }`。列名だけ。`quote_identifier` を通す。システム列も名前で）。
- `SubLinkRenderer<Box<LogicalSubquery>>` は `explain_tree.rs`（L2）が、物理化で決めた `SubPlanId` と `SubPlanStrategy` から作る。`SubLinkRenderer<SubPlanId>` が必要なら `PhysicalQuery` から作る（EXPLAIN は論理側の式を使うので M4 では不要）。

### 4.2 2 つの書式（`DeparseMode`）

PostgreSQL の出力が、計画の前後で違う点だけを `DeparseMode` で切り替える。それ以外（括弧、定数、演算子、関数）は同じ規則。

| 式 | `Plan`（EXPLAIN） | `Stored`（`pg_get_expr`） |
|---|---|---|
| `i IN (1,2,3)`（`InList`、全要素が定数でちょうど 2 個以上） | `(i = ANY ('{1,2,3}'::integer[]))` | `(i = ANY (ARRAY[1, 2, 3]))` |
| `i NOT IN (1,2,3)` | `(i <> ALL ('{1,2,3}'::integer[]))` | `(i <> ALL (ARRAY[1, 2, 3]))` |
| `t IN ('a','b')` | `(t = ANY ('{a,b}'::text[]))` | `(t = ANY (ARRAY['a'::text, 'b'::text]))` |
| `i IN (1, NULL)` | `(i = ANY ('{1,NULL}'::integer[]))` | `(i = ANY (ARRAY[1, NULL::integer]))` |
| 要素が 1 個 | `(i = 1)` / `(i <> 1)` | 同じ |
| 要素に定数でないものがある | `((i = 1) OR (i = bi))` / `NOT IN` は `((i <> 1) AND (i <> bi))` | 同じ |
| `t LIKE 'a!%' ESCAPE '!'`（定数どうし） | `(t ~~ 'a\%'::text)`（`like_escape` を計画時に畳み込む） | `(t ~~ like_escape('a!%'::text, '!'::text))` |
| 根の暗黙のキャスト | 隠さない（計画後は暗黙のキャストが定数に畳み込まれている） | **隠す**（`DEFAULT 1` の `bigint` 列は `1`） |

- 上のうち `Plan` 側の IN の配列定数は、**`InList` の全要素が `Literal`**（定数畳み込み後）のときだけ。M4 には配列型がないので、`'{...}'::{型}[]` の文字列は deparse が直接作る（§4.3）。
- `Stored` の `LIKE`（エスケープなし）は `(t ~~ 'a%'::text)` で同じ。`LIKE ... ESCAPE` の結果は `like_escape(...)` を関数呼び出しとして出す。**エスケープの畳み込み**（`Plan`）の規則は PostgreSQL の `like_escape`: エスケープ文字（空文字列なら「なし」）の直後の文字の前に `\` を置き、エスケープ文字自体は消す。エスケープ文字でない `\` は `\\` にする。パターンかエスケープが定数でなければ `Plan` でも `like_escape(pattern, escape)`。
- `ILIKE` は演算子 `~~*`、`NOT ILIKE` は `!~~*`。

### 4.3 定数（`Literal`）

`literal.rs`。PostgreSQL の `get_const_expr` に従う。型は **`Expr.ty`**（`SqlType`。oid と typmod）で決める（`Datum` の変種ではなく）。

1. **NULL**: `NULL::{型}`（`format_type_with_typemod`）。`NULL::integer`、`NULL::text`、`NULL::boolean`。`CASE` の `ELSE`（`else_result = None`）も `ELSE NULL::{結果の型}` と出す [実機]。
2. **型ごとの書式**（NULL でない値）:

| 型 | 書式 | 例 |
|---|---|---|
| `bool` | `true` / `false`（ラベルなし） | `true` |
| `int4` | 数字（負ならクォート + ラベル） | `5`、`'-1'::integer` |
| `numeric` | 出力が `0-9` で始まり `.` か `e` / `E` を含めばそのまま。それ以外（整数に見える、負、`NaN`、`Infinity`）はクォートして `::numeric` | `1.5`、`1.50`、`'1'::numeric`、`'-1.5'::numeric` |
| unknown 型（`SqlType::UNKNOWN`） | `'...'`（ラベルなし） | `'abc'` |
| それ以外すべて（`int2`、`int8`、`float4`、`float8`、`text`、`varchar`、`bpchar`、`date`、`timestamp`、`timestamptz`、`oid`、`regclass`、…） | `'出力文字列'::{型}` | `'5'::bigint`、`'1.5'::double precision`、`'abc'::text`、`'ab'::bpchar`、`'2020-01-01'::date`、`'2020-01-01 10:00:00'::timestamp without time zone`、`'Infinity'::double precision` |

- **文字列のクォート**（`simple_quote_literal`）: `'` を `''` に。`\` は**そのまま**（`standard_conforming_strings = on`。yuzhu は常に on）。改行・タブもそのまま（エスケープしない）[実機: `'a⏎b'::text`]。
- 出力文字列は `types::io::output_text(d, ty, type_env)`（`float8` は `extra_float_digits` の影響を受ける。PostgreSQL も同じ）。日時は `type_env.datetime`（DateStyle と TimeZone）に従う（PostgreSQL と同じ。ISO 以外の DateStyle ではそれに従った文字列になる）。`regclass` の定数は `catalog.relation_name(oid)`（`'tt_id_seq'::regclass`。検索パスで見えない名前空間のものは `'schema.name'::regclass`）。
- **ラベルの型名**は `format_type_with_typemod(oid, typmod)`（§4.8）。定数の typmod は通常 -1（`'ab'::character varying`、`'ab'::bpchar`）。
- **配列定数**（`Plan` の IN 用。`'{1,2,3}'::integer[]`）: 要素は各型の出力文字列。`array_out` のクォート規則を使う: 要素が空文字列、`{` `}` `,` `"` `\` または空白を含む、あるいは大文字小文字を区別せず `NULL` に等しいときは `"` で囲み、中の `"` と `\` を `\` でエスケープする。NULL 要素は `NULL`。区切りは `,`（空白なし）。型名は `format_type(要素の型)` に `[]` を付ける（`integer[]`、`text[]`、`bigint[]`、`numeric[]`、`date[]`、`character varying[]`）。
- `Stored` の `ARRAY[...]`: 各要素を通常の定数として §4.3 の規則で出し、`ARRAY[` と `]`、`, ` 区切り（`ARRAY['a'::text, 'b'::text]`、`ARRAY[1, NULL::integer]`）。

### 4.4 式の変種ごとの書式

「括弧」の列は**非 pretty**（`pretty_paren = false`）での、その式が自分で付ける括弧。pretty は §4.6。`E(x)` は式 x の deparse。

| `ExprKind` | 書式 | 備考 |
|---|---|---|
| `Literal` | §4.3 | |
| `Column` | `ColumnNamer::name`。`wrap` なら `(` `)` で包む | |
| `Operator`（2 項） | `(E(l) {op} E(r))` | `{op}` は `BuiltinOperator.name`（`+` `=` `<>` `||` `~~` `~` `!~*` など）。前後に空白 1 つ。ビルトインは `OPERATOR(pg_catalog.x)` の形にしない |
| `Operator`（前置） | `({op} E(a))` | `(- i)`（`-` の後に空白 1 つ）[実機] |
| `Function` | `{name}(E(a1), E(a2))` | `{name}` は `quote_identifier`（`"current_schema"()`、`"left"(...)` のようにキーワードは引用符。§4.7）。引数なしは `name()` |
| `Function`（SQL 構文の関数） | 下の表 | |
| `SessionValue` | `CURRENT_USER`、`SESSION_USER`、`USER`、`CURRENT_CATALOG`、`"current_schema"()` | `CurrentSchema` は関数形 [実機] |
| `Cast` | `(E(a))::{型}` | 引数に常に括弧（非 pretty）。`((i + 1))::text` と二重になる。型は `format_type_with_typemod(ty)` |
| `CoerceTypmod` | `(E(a))::{型（typmod つき）}` | `varchar(10)` への長さの強制。**暗黙（`explicit = false`）で根にあるとき `Stored` は隠す** |
| `And` | `(E(a) AND E(b) AND ...)` | 3 つ以上も 1 組の括弧 |
| `Or` | `(E(a) OR E(b) OR ...)` | |
| `Not` | `(NOT E(a))` | |
| `IsNull` / `IsNotNull` | `(E(a) IS NULL)` / `(E(a) IS NOT NULL)` | |
| `BoolTest` | `(E(a) IS TRUE)`、`IS NOT TRUE`、`IS FALSE`、`IS NOT FALSE`、`IS UNKNOWN`、`IS NOT UNKNOWN` | |
| `Case` | `CASE WHEN E(c) THEN E(r) [WHEN ...] ELSE E(e) END` | **EXPLAIN は 1 行**。`Stored` は複数行（§4.6）。`else_result = None` は `ELSE NULL::{型}` |
| `Coalesce` | `COALESCE(E(a), E(b), ...)` | 大文字 |
| `NullIf` | `NULLIF(E(l), E(r))` | 大文字 |
| `Like` | `(E(e) ~~ E(p))` / `!~~` / `~~*` / `!~~*` | `escape` つきは §4.2 |
| `InList` | §4.2 | |
| `Aggregate` | `{name}([DISTINCT ]E(a))` と、`filter` があれば ` FILTER (WHERE E(f))` | `count(*)`（引数なし）。`count(DISTINCT b)`、`sum(a)`、`max((a + 1))`、`bool_and((b > 1))`、`count(*) FILTER (WHERE (b > 2))` [実機] |
| `SubLink` | §3.11 の 8 | `Stored` では来ない（CHECK・DEFAULT に副問い合わせは書けず、解析が `0A000 cannot use subquery in check constraint` で弾く）。来たら `Error::internal` |
| `SubLinkOutput(i)` | `SubLinkRenderer` の名前 + `.col{i+1}` | `test` の中だけ |

**SQL 構文の関数**（`BuiltinFunction.name` がこの名前のものは、関数呼び出しの形ではなく SQL 構文の形で出す。PostgreSQL の `COERCE_SQL_SYNTAX`。[実機]）:

| 関数名 | 書式 |
|---|---|
| `current_date` | `CURRENT_DATE` |
| `current_timestamp` | `CURRENT_TIMESTAMP`（精度つきなら `CURRENT_TIMESTAMP(3)`） |
| `localtimestamp` | `LOCALTIMESTAMP`（同様） |
| `current_time` / `localtime` | 同様（M5） |
| `now`、`transaction_timestamp`、`statement_timestamp`、`clock_timestamp` | 通常の関数呼び出し（`now()`） |

`current_timestamp` などを `now()` と同じ `BuiltinFunction` の別名にすると、元の綴りが失われて `CURRENT_TIMESTAMP` が `now()` と出てしまう。**章 09 は、SQL 構文用に別の `BuiltinFunction` の行（名前が `current_timestamp` など）を持つこと**（他章への依頼。末尾）。

**型名のキャストで読み替えるもの**: `Cast` の対象が `bool` で `Function` でなく `InOut` の `'true'::boolean` のような定数は、定数の書式（§4.3）が先に使われる。

### 4.5 暗黙のキャスト

PostgreSQL は、演算子と関数の引数にある暗黙のキャストを**表示する**（`(1)::numeric`、`((v)::text = 'x'::text)`、`(f > (1.5)::double precision)`）。表示しないのは**式の根**にある暗黙のキャストだけ（`DEFAULT 1` の `bigint` 列は `1`、`DEFAULT 'a'` の `text` 列は `'a'::text`）[実機]。

- `Stored`: 根が `Cast { implicit: true }`、または暗黙の `CoerceTypmod` なら、その `expr` を（根として）deparse する。連続していれば繰り返す。それ以外の位置は隠さない。**そのため `Cast` に `implicit: bool` を持たせる**（00 への変更提案 3。アナライザの暗黙のキャストの挿入箇所が `true`、`CAST(x AS t)` / `x::t` が `false`）。
- `Plan`: 隠さない（計画後は定数の暗黙のキャストは定数に畳み込まれ、残るのは列へのキャストだけで、PostgreSQL もそれを表示する: `((v)::text = 'x'::text)`）。
- 混合幅の整数比較（`bi > 5`、`i > 2147483648`）は、PostgreSQL では `int84gt` などの演算子が選ばれてキャストが出ない。yuzhu の組み込み演算子表（章 09）に `int2`/`int4`/`int8` の混合幅の比較・算術演算子があれば同じ表示になる。なければ `Cast` が入って `(bi > '5'::bigint)`（`Plan`）や `(bi > (5)::bigint)`（`Stored`）になり一致しない（確認事項 [10-Q9]、章 09 への依頼）。

### 4.6 括弧と字下げ

**非 pretty**（`pretty_paren = false`。EXPLAIN と `pg_get_expr(.., false)`）:

- 自分で括弧を付ける式: `Operator`（2 項・前置）、`And` / `Or` / `Not`、`IsNull` / `IsNotNull`、`BoolTest`、`Like`、`InList`（`ANY` / `ALL` の形）、`Cast` の引数（`(E(a))::t`）。**どの位置でも付ける**（関数の引数の位置でも: `lower((t || 'a'::text))`）。
- 自分では付けない式: 定数、列、関数、`CASE`、`COALESCE`、`NULLIF`、集約。
- 根の式にも付く: `CHECK ((i > 0))`（`CHECK (` + `(i > 0)` + `)`）。

**pretty**（`pretty_paren = true`。`pg_get_expr(.., true)`、`pg_get_constraintdef(.., true)`。psql の `\d`）。式を「**自分では括弧を付けない**」で出し、**親が子を出すときに `is_simple(子, 親)` が偽なら括弧で包む**（PostgreSQL の `get_rule_expr_paren` と `isSimpleNode`）。親が子を `is_simple` で判断する位置は「演算子の引数、`AND`/`OR`/`NOT` の引数、`IS NULL` / `IS TRUE` の引数、`ANY` / `ALL` の両辺、キャストの引数」。**関数の引数・`COALESCE` の引数・`CASE` の各部分・`ARRAY[...]` の要素は括弧の判断をしない**（`COALESCE(i + 1, 2)`、`lower(t || 'a'::text)`）。根にも括弧は付かない。

`is_simple(node, parent)`:

| `node` | 判定 |
|---|---|
| 定数、列、`SubLinkOutput`、集約、関数、`COALESCE`、`NULLIF`、`CASE`、`SessionValue` | 常に真（括弧なし） |
| `Cast`（`CastMethod::Function`） | 真（関数形のキャスト: `i::bigint`、引数側で括弧: `(i + 1)::bigint`） |
| `Cast`（`Binary` / `InOut`）と `CoerceTypmod` | `is_simple(引数, このキャスト)`（引数が演算子などなら偽で、キャスト全体が括弧に入る: `((i + 1)::text) IS NULL`） |
| 2 項の算術演算子 `+ - * / %`（名前が 1 文字で、その 5 つ）で、**親も同じ種類の演算子** | 優先度で判定: 子が `* / %`・親が `+ -` なら真、子が `+ -`・親が `* / %` なら偽。**同じ優先度なら、子が親の左の引数のときだけ真**（`i - 1 - 2`、`i + (1 - 1)`、`i / (2 * 3)`） |
| `Operator`（上以外。比較、`||`、`~~`、前置の `-` など）、`Like` | 親が演算子（`Operator` / `Like`）なら偽。親が関数（キャスト・SQL 構文の関数でない）、`AND`/`OR`/`NOT`、`CASE`、`COALESCE`、`NULLIF`、集約、配列なら真。それ以外（`ANY`、キャスト、`IS NULL` など）は偽 |
| `IsNull` / `IsNotNull` / `BoolTest` / `SubLink` | 親が関数、`AND`/`OR`/`NOT`、`CASE`、`COALESCE`、`NULLIF`、集約、配列なら真。それ以外は偽 |
| `InList`（`ANY` の形）| 常に偽（`(i = ANY (ARRAY[1, 2])) AND b`） |
| `And` / `Or` / `Not` | 親が `And` / `Or` / `Not` のときだけ判定: 子 `And` は親 `And`・`Or` で真、親 `Not` で偽。子 `Or` は親 `Or` で真、親 `And`・`Not` で偽。子 `Not` は親 `And`・`Or` で真、親 `Not` で偽。それ以外の親は偽 |

- 上の表の「関数」の親は、`FuncExpr` のうち**キャストと SQL 構文の関数を除く**（それらの親では偽）。
- PostgreSQL の関数の引数・`COALESCE` の中の `BoolExpr` は括弧なし（`COALESCE(b AND b, true)`）[実機]。
- **`(a AND b) AND c` のようなネスト**は、`And` の入れ子が子 `And`・親 `And` で真なので `a AND b AND c` と平らに出る（非 pretty は `(a AND (b AND c))` と入れ子のまま）。

**CASE の字下げ**（`indent = true`）: PostgreSQL の `appendContextKeyword`。

```
CASE                      ← 直前に改行（先頭が "\n" で始まる）。字下げ = 現在の字下げ幅
    WHEN cond THEN res    ← 字下げ + 4。"WHEN " + E(cond) + " THEN " + E(result)
    ELSE e                ← 字下げ + 4
END                       ← 字下げ
```

実装: `indent_level`（最初 0）を持ち、`CASE` で「改行 + `indent_level` 個の空白 + `CASE`」を出して `indent_level += 4`、`WHEN` / `ELSE` ごとに「改行 + `indent_level` の空白 + キーワード」、`END` の前に `indent_level -= 4` して「改行 + 空白 + `END`」。**キーワードを出す直前に、バッファ末尾の空白を取り除く**（`removeStringInfoSpaces`）。単純な `CASE x WHEN v ...`（`CASE i WHEN 1 THEN ...`）は M1 で `x = v` の `WHEN` に展開されているので、`CASE i WHEN 1` の形には戻さない（`WHEN (i = 1)`）。ネストした `CASE` は内側で `indent_level` が 4 ずつ増える [実機]:

```
CHECK ((
CASE
    WHEN (
    CASE
        WHEN b THEN 1
        ELSE 2
    END > 1) THEN 'a'::text
    ELSE 'b'::text
END = 'a'::text))
```

（非 pretty。`CHECK ((` の後ろの改行は `CASE` の直前の改行。pretty は `CHECK (` + 改行 + `CASE`、`WHEN` の条件に括弧なし。）`indent = false`（EXPLAIN）は 1 行: `CASE WHEN (i > 1) THEN 'a'::text ELSE 'b'::text END`。

### 4.7 識別子（`quote_identifier`）

`ident.rs`。`settings::quote_identifier`（M1。キーワードを見ない）は置き換えず、deparse 用に別に持つ。PostgreSQL の `quote_identifier` と同じ: 次のどれかなら `"..."`（中の `"` は `""`）で囲む。

1. 空文字列、2. 先頭が `a-z` か `_` でない、3. 2 文字目以降に `a-z` `0-9` `_` 以外がある、4. `sql::token::keyword_category(name)` が `Unreserved` 以外（`ColName`・`TypeFuncName`・`Reserved`）。

これを、リレーション名、別名、列名、関数名、インデックス名、スキーマ名のすべてに使う（`"current_schema"()`、`"left"(...)`、`Seq Scan on "MyTable"`、`"*VALUES*"` は常に引用符つきの固定文字列）。

### 4.8 format_type と、保存式の経路

**`format_type(oid, typmod) -> String`**（`typename.rs`。SQL 関数 `format_type(oid, int4)` と、deparse のキャスト・定数のラベルが共有する。typmod が NULL または -1 は「指定なし」）[実機]:

| oid | typmod なし | typmod あり |
|---|---|---|
| 16 | `boolean` | |
| 18 | `"char"` | |
| 19 | `name` | |
| 20 / 21 / 23 | `bigint` / `smallint` / `integer` | |
| 22 | `int2vector` | |
| 25 | `text` | |
| 26 | `oid` | |
| 700 / 701 | `real` / `double precision` | |
| 1042 | `bpchar` | `character(n)`（typmod = n + 4。`format_type(1042, 5)` は `character(1)`） |
| 1043 | `character varying` | `character varying(n)`（`format_type(1043, 12)` は `character varying(8)`） |
| 1082 | `date` | |
| 1114 | `timestamp without time zone` | `timestamp(p) without time zone` |
| 1184 | `timestamp with time zone` | `timestamp(p) with time zone` |
| 1700 | `numeric` | `numeric(p,s)`（typmod - 4 の上位 16 ビットが p、下位が s。`format_type(1700, 655366)` は `numeric(10,2)`） |
| 2205 / 2206 | `regclass` / `regtype` | |
| 2278 | `void` | |
| 0 | `-` | |
| 配列型 | `{要素}[]`（`integer[]`） | |
| その他 | `pg_type.typname` を `quote_identifier` | |

**`pg_get_expr(pg_node_tree, oid [, bool])`**（`stored.rs`。SQL 関数。章 09 / 07 が関数の行を置き、実体はこれを呼ぶ）:

```rust
// deparse/stored.rs
/// 保存された式のテキスト（pg_attrdef.adbin、pg_constraint.conbin。M2-Q6 のとおり SQL のテキスト）を、
/// parse → analyze → deparse して返す。relid = 0 は列を持たない式
pub fn pg_get_expr(src: &str, relid: Oid, pretty: bool, catalog: &dyn CatalogReader, type_env: &TypeEnv<'_>) -> Result<String>;
```

1. `relid != 0` なら `catalog.table_by_oid(relid)`（なければ `NULL` を返す。PostgreSQL と同じ）。`sql::parse_expr(src)`（式 1 つ。M2 で DEFAULT・CHECK の再解析に使っている関数）。
2. 列の既定値（`pg_attrdef`）は、その列の型への代入キャスト（暗黙）をかけて解析する（`analyze_default(expr, column_ty)`。CREATE TABLE の DEFAULT の解析と同じ関数）。CHECK は `bool`。`Var { rte: RteId(0), col }` で表の列を参照する。
3. `deparse_expr(&bound, &DeparseCtx { opts: DeparseOptions::stored(pretty), namer: &TableNamer, .. })`。
4. 保存形式は変えない（00 D-21）。`pg_attrdef.adbin` に入っているのは今までどおりのテキスト。

```
x bigint DEFAULT 1                   →  1                                 （根の暗黙のキャストを隠す）
w varchar(5) DEFAULT 'ab'            →  'ab'::character varying
z text DEFAULT 'a'                   →  'a'::text
h int DEFAULT -1                     →  '-1'::integer
i int DEFAULT 1+2                    →  (1 + 2)           （pretty: 1 + 2）
n text DEFAULT 'a'||'b'              →  ('a'::text || 'b'::text)         （pretty: 'a'::text || 'b'::text）
p bigint DEFAULT (1)::bigint         →  (1)::bigint       （pretty: 1::bigint）
q int DEFAULT nextval('s1')          →  nextval('s1'::regclass)
t timestamp DEFAULT now()            →  now()
t2 timestamp DEFAULT current_timestamp  →  CURRENT_TIMESTAMP
```

**任意（`\d tbl`、M4 後半）**:

- `pg_get_constraintdef(oid [, bool])`: `pg_constraint` の行から。`contype = 'c'` → `CHECK (` + `pg_get_expr` の結果 + `)`（非 pretty `CHECK ((n > (0)::numeric))`、pretty `CHECK (n > 0::numeric)`）。`'p'` → `PRIMARY KEY (id)`、`'u'` → `UNIQUE (name)`（列名は `quote_identifier`、`, ` 区切り）。それ以外（`'f'`）は M5。
- `pg_get_indexdef(oid [, int4 [, bool]])`: 第 2 引数が 0（または省略）なら全体: `CREATE [UNIQUE] INDEX {名前} ON {リレーション} USING btree ({列, ...})`。リレーションは**非 pretty では常にスキーマ修飾**（`public.tt`）、pretty では検索パスで見えれば修飾しない（`tt`）。各列は `列名 [演算子クラス] [DESC] [NULLS FIRST|NULLS LAST]`: 演算子クラスは**その型の既定でないとき**だけ、`NULLS` は**既定と違うとき**だけ（ASC の既定は NULLS LAST、DESC の既定は NULLS FIRST。`n DESC` は NULLS FIRST を省略）。第 2 引数が 1 以上なら、その列の名前だけ [実機]:

```
CREATE UNIQUE INDEX tt_pkey ON public.tt USING btree (id)               pretty: ... ON tt USING btree (id)
CREATE INDEX tt_multi ON public.tt USING btree (n DESC, name text_pattern_ops)
CREATE UNIQUE INDEX tt_u2 ON public.tt USING btree (name, n DESC)
```

### 4.9 deparse の対応表（PostgreSQL 17.11 [実機]。そのままテストの入力にする）

`CHECK` 制約に入れて `pg_get_constraintdef(oid, false)` と `pg_get_constraintdef(oid, true)` を取ったもの（左から、SQL、非 pretty、pretty）。`Stored` のテストはこの表をそのまま使う（`tests/slt/m4/explain/deparse_stored.slt`。両方のターゲットで同じ結果）。

| SQL | 非 pretty | pretty |
|---|---|---|
| `i > 0` | `CHECK ((i > 0))` | `CHECK (i > 0)` |
| `i + 1 + 2 > 0` | `CHECK ((((i + 1) + 2) > 0))` | `CHECK ((i + 1 + 2) > 0)` |
| `i + 1 * 2 > 3` | `CHECK (((i + (1 * 2)) > 3))` | `CHECK ((i + 1 * 2) > 3)` |
| `(i + 1) * 2 > 3` | `CHECK ((((i + 1) * 2) > 3))` | `CHECK (((i + 1) * 2) > 3)` |
| `i - (1 - 2) > 0` | `CHECK (((i - (1 - 2)) > 0))` | `CHECK ((i - (1 - 2)) > 0)` |
| `i - 1 - 2 > 0` | `CHECK ((((i - 1) - 2) > 0))` | `CHECK ((i - 1 - 2) > 0)` |
| `-i < 0` | `CHECK (((- i) < 0))` | `CHECK ((- i) < 0)` |
| `i / (2 * 3) > 0` | `CHECK (((i / (2 * 3)) > 0))` | `CHECK ((i / (2 * 3)) > 0)` |
| `i % 2 * 3 > 0` | `CHECK ((((i % 2) * 3) > 0))` ※ | `CHECK ((i % 2 * 3) > 0)` |
| `i = bi + 1` | `CHECK ((i = (bi + 1)))` | `CHECK (i = (bi + 1))` |
| `t = 'abc'` | `CHECK ((t = 'abc'::text))` | `CHECK (t = 'abc'::text)` |
| `v = 'x'`（`varchar`） | `CHECK (((v)::text = 'x'::text))` | `CHECK (v::text = 'x'::text)` |
| `c = 'ab'`（`char(3)`） | `CHECK ((c = 'ab'::bpchar))` | `CHECK (c = 'ab'::bpchar)` |
| `t \|\| 'x' = 'yx'` | `CHECK (((t \|\| 'x'::text) = 'yx'::text))` | `CHECK ((t \|\| 'x'::text) = 'yx'::text)` |
| `lower(t) = 'abc'` | `CHECK ((lower(t) = 'abc'::text))` | `CHECK (lower(t) = 'abc'::text)` |
| `lower(t \|\| 'a') = 'x'` | `CHECK ((lower((t \|\| 'a'::text)) = 'x'::text))` | `CHECK (lower(t \|\| 'a'::text) = 'x'::text)` |
| `t LIKE 'a%'` | `CHECK ((t ~~ 'a%'::text))` | `CHECK (t ~~ 'a%'::text)` |
| `t NOT ILIKE 'a%'` | `CHECK ((t !~~* 'a%'::text))` | `CHECK (t !~~* 'a%'::text)` |
| `t LIKE 'a!%' ESCAPE '!'` | `CHECK ((t ~~ like_escape('a!%'::text, '!'::text)))` | 同じ（括弧のみ pretty） |
| `t ~ '^a'` | `CHECK ((t ~ '^a'::text))` | `CHECK (t ~ '^a'::text)` |
| `i IN (1,2,3)` | `CHECK ((i = ANY (ARRAY[1, 2, 3])))` | `CHECK (i = ANY (ARRAY[1, 2, 3]))` |
| `i NOT IN (1,2,3)` | `CHECK ((i <> ALL (ARRAY[1, 2, 3])))` | `CHECK (i <> ALL (ARRAY[1, 2, 3]))` |
| `i IN (1)` | `CHECK ((i = 1))` | `CHECK (i = 1)` |
| `i IN (1, bi)` | `CHECK (((i = 1) OR (i = bi)))` | `CHECK (i = 1 OR i = bi)` |
| `i IN (1, NULL)` | `CHECK ((i = ANY (ARRAY[1, NULL::integer])))` | `CHECK (i = ANY (ARRAY[1, NULL::integer]))` |
| `i BETWEEN 1 AND 5` | `CHECK (((i >= 1) AND (i <= 5)))` | `CHECK (i >= 1 AND i <= 5)` |
| `i NOT BETWEEN 1 AND 5` | `CHECK (((i < 1) OR (i > 5)))` | `CHECK (i < 1 OR i > 5)` |
| `t IS NULL` / `t IS NOT NULL` | `CHECK ((t IS NULL))` / `CHECK ((t IS NOT NULL))` | `CHECK (t IS NULL)` / `CHECK (t IS NOT NULL)` |
| `b IS NOT TRUE` | `CHECK ((b IS NOT TRUE))` | `CHECK (b IS NOT TRUE)` |
| `b` | `CHECK (b)` | `CHECK (b)` |
| `NOT b` | `CHECK ((NOT b))` | `CHECK (NOT b)` |
| `b AND i > 1` | `CHECK ((b AND (i > 1)))` | `CHECK (b AND i > 1)` |
| `(b AND i > 1) OR i < 0` | `CHECK (((b AND (i > 1)) OR (i < 0)))` | `CHECK (b AND i > 1 OR i < 0)` |
| `b AND (i > 1 OR i < 0)` | `CHECK ((b AND ((i > 1) OR (i < 0))))` | `CHECK (b AND (i > 1 OR i < 0))` |
| `NOT (b AND i > 1)` | `CHECK ((NOT (b AND (i > 1))))` | `CHECK (NOT (b AND i > 1))` |
| `NOT (i > 1)` | `CHECK ((NOT (i > 1)))` | `CHECK (NOT i > 1)` |
| `NOT b AND b` | `CHECK (((NOT b) AND b))` | `CHECK (NOT b AND b)` |
| `NOT (NOT b)` | `CHECK ((NOT (NOT b)))` | `CHECK (NOT (NOT b))` |
| `b AND (b AND b)` | `CHECK ((b AND (b AND b)))` | `CHECK (b AND b AND b)` |
| `(b OR b) AND (b OR b)` | `CHECK (((b OR b) AND (b OR b)))` | `CHECK ((b OR b) AND (b OR b))` |
| `(i > 0) = (bi > 0)` | `CHECK (((i > 0) = (bi > 0)))` | `CHECK ((i > 0) = (bi > 0))` |
| `(i + 1) IS NULL` | `CHECK (((i + 1) IS NULL))` | `CHECK ((i + 1) IS NULL)` |
| `(i + 1)::text IS NULL` | `CHECK ((((i + 1))::text IS NULL))` | `CHECK (((i + 1)::text) IS NULL)` |
| `COALESCE(b AND b, true) = b` | `CHECK ((COALESCE((b AND b), true) = b))` | `CHECK (COALESCE(b AND b, true) = b)` |
| `COALESCE(t, v, 'z') = 'q'` | `CHECK ((COALESCE(t, (v)::text, 'z'::text) = 'q'::text))` | `CHECK (COALESCE(t, v::text, 'z'::text) = 'q'::text)` |
| `NULLIF(i, 0) = 1` | `CHECK ((NULLIF(i, 0) = 1))` | `CHECK (NULLIF(i, 0) = 1)` |
| `i::bigint > 1` | `CHECK (((i)::bigint > 1))` | `CHECK (i::bigint > 1)` |
| `(i + 1)::bigint > 0` | `CHECK ((((i + 1))::bigint > 0))` | `CHECK ((i + 1)::bigint > 0)` |
| `i::bigint::text = t` | `CHECK ((((i)::bigint)::text = t))` | `CHECK (i::bigint::text = t)` |
| `i::numeric > 1.5` | `CHECK (((i)::numeric > 1.5))` | `CHECK (i::numeric > 1.5)` |
| `n > 1`（`numeric(10,2)`） | `CHECK ((n > (1)::numeric))` | `CHECK (n > 1::numeric)` |
| `n = 1.50` | `CHECK ((n = 1.50))` | `CHECK (n = 1.50)` |
| `f > 1`（`float8`） | `CHECK ((f > (1)::double precision))` | `CHECK (f > 1::double precision)` |
| `f > 'Infinity'` | `CHECK ((f > 'Infinity'::double precision))` | `CHECK (f > 'Infinity'::double precision)` |
| `i > -1` | `CHECK ((i > '-1'::integer))`（※ 負数リテラルは定数に畳まれる） | 同じ |
| `i > 2147483648` | `CHECK ((i > '2147483648'::bigint))` | `CHECK (i > '2147483648'::bigint)` |
| `i > 1::bigint` | `CHECK ((i > (1)::bigint))` | `CHECK (i > 1::bigint)` |
| `d > '2020-01-01'` | `CHECK ((d > '2020-01-01'::date))` | 同じ（括弧のみ pretty） |
| `ts > '2020-01-01 10:00'` | `CHECK ((ts > '2020-01-01 10:00:00'::timestamp without time zone))` | |
| `tz > '2020-01-01 10:00+00'` | `CHECK ((tz > '2020-01-01 10:00:00+00'::timestamp with time zone))` | |
| `ts > now()` | `CHECK ((ts > now()))` | |
| `tz > current_timestamp` | `CHECK ((tz > CURRENT_TIMESTAMP))` | |
| `d > current_date` | `CHECK ((d > CURRENT_DATE))` | |
| `ts::date = d` | `CHECK (((ts)::date = d))` | `CHECK (ts::date = d)` |
| `current_user = t` | `CHECK ((CURRENT_USER = t))` | |
| `current_schema() = t` | `CHECK (("current_schema"() = t))` | |
| `t = ''` | `CHECK ((t = ''::text))` | |
| `t = 'it''s'` | `CHECK ((t = 'it''s'::text))` | |
| `t = 'a\b'` | `CHECK ((t = 'a\b'::text))` | |
| `case when i > 1 then 'a' else 'b' end = t` | `CHECK ((⏎CASE⏎    WHEN (i > 1) THEN 'a'::text⏎    ELSE 'b'::text⏎END = t))` | `CHECK (⏎CASE⏎    WHEN i > 1 THEN 'a'::text⏎    ELSE 'b'::text⏎END = t)` |
| `case when i > 1 then 1 end = 1` | `... ELSE NULL::integer⏎END = 1))` | |

（⏎ は改行。※ の行は `i % 2 * 3` の `%` と `*` が同じ優先度で左結合なので、非 pretty は `(((i % 2) * 3) > 0)` と全部の組に括弧が付く。）

**EXPLAIN（`Plan`）の対応表**（`SELECT * FROM chk2 WHERE <式>` の `Filter:` 行。PostgreSQL が定数を畳み込んだ後の形）:

| SQL | Filter |
|---|---|
| `i + 1 * 2 > 3` | `((i + 2) > 3)`（畳み込み。yuzhu の畳み込みは 04 の範囲） |
| `i - (1 - 2) > 0` | `((i - '-1'::integer) > 0)` |
| `t = 'abc'` | `(t = 'abc'::text)` |
| `v = 'x'` | `((v)::text = 'x'::text)` |
| `t LIKE 'a!%' ESCAPE '!'` | `(t ~~ 'a\%'::text)` |
| `i IN (1,2,3)` | `(i = ANY ('{1,2,3}'::integer[]))` |
| `t IN ('a','b')` | `(t = ANY ('{a,b}'::text[]))` |
| `i IN (1, NULL)` | `(i = ANY ('{1,NULL}'::integer[]))` |
| `i IN (1, bi)` | `((i = 1) OR (i = bi))` |
| `n > 1` | `(n > '1'::numeric)` |
| `n > 1.5` | `(n > 1.5)` |
| `f > 1.5` | `(f > '1.5'::double precision)` |
| `f > 1e10` | `(f > '10000000000'::double precision)` |
| `f > 1e100` | `(f > '1e+100'::double precision)` |
| `bi > 5` | `(bi > 5)` |
| `bi > 2147483648` | `(bi > '2147483648'::bigint)` |
| `i > 2147483648` | `(i > '2147483648'::bigint)` |
| `i > -1` | `(i > '-1'::integer)` |
| `i > 1::bigint` | `(i > '1'::bigint)` |
| `d > '2020-01-01'` | `(d > '2020-01-01'::date)` |
| `case when i > 1 then 'a' else 'b' end = t` | `(CASE WHEN (i > 1) THEN 'a'::text ELSE 'b'::text END = t)` |
| `coalesce(t, v, 'z') = 'q'` | `(COALESCE(t, (v)::text, 'z'::text) = 'q'::text)` |
| `i::bigint > 1` | `((i)::bigint > 1)` |
| `(i + 1)::text = t` | `(((i + 1))::text = t)` |
| `b IS UNKNOWN` | `(b IS UNKNOWN)` |
| `tz > current_timestamp` | `(tz > CURRENT_TIMESTAMP)` |
| `current_schema() = t` | `("current_schema"() = t)` |

PostgreSQL が行う式の変形（`b OR b OR b` → `b`、`NOT (b AND x)` → `(NOT b) OR (x <= 1)`、述語の並べ替え）は yuzhu では行わず、**同じ式を同じ書式で出す**ことだけを一致させる。

---

## 5. COPY FROM STDIN

### 5.1 構文

```
COPY table_name [ ( column_name [, ...] ) ] FROM STDIN [ [ WITH ] ( option [ value ] [, ...] ) ]
COPY table_name [ ( column_name [, ...] ) ] FROM STDIN [ [ WITH ] legacy_option [ ... ] ]       -- 旧構文
legacy_option := BINARY | DELIMITER [ AS ] 'c' | NULL [ AS ] 'string' | CSV [ HEADER ] ...
```

**AST**（`sql/ast.rs`。S1 の担当）:

```rust
pub struct Copy {
    pub table: QualifiedName,          // [schema.]name
    pub columns: Vec<Ident>,           // 空 = 全列
    pub direction: CopyDirection,
    pub source: CopySource,
    pub options: Vec<CopyOption>,
    pub where_clause: Option<Expr>,
    pub span: Span,
}
pub enum CopyDirection { From, To }
pub enum CopySource { Stdin, File(String), Program(String) }      // To のときは Stdout / File / Program の意味
pub struct CopyOption { pub name: String /* 小文字 */, pub value: Option<CopyOptionValue>, pub name_span: Span }
pub enum CopyOptionValue { Word(String), String(String), Integer(i64), List(Vec<String>) /* force_not_null (a, b) */, Star }
```

- 旧構文は、`BINARY` → `format = binary`、`CSV` → `format = csv`、`DELIMITER [AS] 'c'` → `delimiter`、`NULL [AS] 's'` → `null`、`HEADER` → `header` の `CopyOption` に直す（AST は 1 種類）。`COPY BINARY t FROM ...`（`COPY` の直後の `BINARY`）も `format = binary`。`COPY t FROM STDIN, null` のような余分なものは構文エラー [実機]。
- `COPY (query) TO ...`、`COPY ... TO ...` は構文として読み、アナライザが `0A000`。
- `COPY` の対象のスキーマ修飾（`public.cp`、`ch10.public.cp`）は通常の名前解決。

**アナライザの検査**（`copy/mod.rs` の `analyze_copy`。O1 が書き、`analyzer::analyze` の `Statement::Copy` の分岐から呼ぶ（呼び出しの口は P0 が置く）。結果は `BoundStatement::Copy(BoundCopy)`）。順序は PostgreSQL の `ProcessCopyOptions` / `DoCopy` に合わせる。エラーはすべて [実機]（SQLSTATE まで確認）。

| 条件 | SQLSTATE | メッセージ |
|---|---|---|
| `direction = To`、`source` が `File` / `Program` | `0A000` | `COPY TO is not supported yet` / `COPY from a file is not supported` / `COPY from a program is not supported` （PostgreSQL は権限があればファイルを読む。yuzhu は常に拒否） |
| 対象が存在しない | `42P01` | `relation "nosuch" does not exist`（位置あり） |
| 対象がビュー / シーケンス（M4 でビューは作れないが `CREATE SEQUENCE` はある） | `42809` | `cannot copy to sequence "sq9"`（ビューは `cannot copy to view "v"`） |
| 列リストに存在しない列 | `42703` | `column "zz" of relation "cp" does not exist` |
| 列リストに同じ列が 2 回 | `42701` | `column "a" specified more than once` |
| option の名前が下の表にない | `42601` | `option "foo" not recognized`（位置は名前） |
| 同じ option を 2 回（`freeze on, freeze off`、`format text, format csv`） | `42601` | `conflicting or redundant options`（位置は 2 つ目の名前） |
| 読み取り専用トランザクション | `25006` | `cannot execute COPY FROM in a read-only transaction` |

**option**（PostgreSQL 17 の名前）:

| 名前 | 値 | 動作 | エラー |
|---|---|---|---|
| `format` | `text`（既定）、`csv`、`binary` | `text` のみ。`csv` / `binary` は `0A000`（`COPY format "csv" is not supported yet`）。`foo` は `22023 COPY format "foo" not recognized`（位置は値） | |
| `freeze` | ブール（省略 = true） | §5.6 | |
| `delimiter` | 1 バイトの文字列 | 既定はタブ | 1 バイトでない・空: `0A000 COPY delimiter must be a single one-byte character`（PostgreSQL は `0A000`）。改行・復帰: `22023 COPY delimiter cannot be newline or carriage return`。`\` `.` 英数字 `\r` `\n`: `22023 COPY delimiter cannot be "x"`（text 形式で使えないのは `\`、`.`、`0-9`、`a-z`、`A-Z`） |
| `null` | 文字列 | 既定は `\N` | 改行・復帰を含む: `22023 COPY null representation cannot use newline or carriage return`。区切り文字を含む: `22023 COPY delimiter character must not appear in the NULL specification` |
| `default` | 文字列 | そのフィールドは列の DEFAULT（PG16+） | `null` と同じ文字列: `0A000 NULL specification and DEFAULT specification cannot be the same`。区切り文字を含む: `22023 COPY delimiter character must not appear in the DEFAULT specification` |
| `header` | ブール | true なら**最初の 1 行を捨てる**（text 形式。PG17 は text でも受け付ける）。`match` は `0A000` | `header 'x'` は `42601 header requires a Boolean value or "match"` |
| `encoding` | 文字列 | `UTF8`（大文字小文字・`UTF-8` を区別しない）のみ。他の有効な名前は `0A000`、無効な名前は `22023 argument to option "encoding" must be a valid encoding name` | |
| `log_verbosity` | `default` / `verbose` / `terse` | 受け付けて無視。他の値は `22023 COPY LOG_VERBOSITY "foo" not recognized` | |
| `on_error` | `stop` / `ignore` | `stop` のみ。`ignore` は `0A000`。他は `22023 COPY ON_ERROR "foo" not recognized` | |
| `quote` `escape` `force_quote` `force_not_null` `force_null` | | CSV 専用。text 形式では `0A000 COPY QUOTE requires CSV mode`（名前は大文字。`FORCE_NOT_NULL` など） | |

`WHERE`（`where_clause`）は `0A000 COPY FROM ... WHERE is not supported yet`。

**`BoundCopy`**（00 §7 の名前。O1 が `copy/mod.rs` に定義）:

```rust
pub struct BoundCopy {
    pub table: Arc<TableDef>,
    /// 入力の i 番目のフィールドが入る列（attnum - 1）。列リストなしは 0..ncols
    pub columns: Vec<usize>,
    /// 全列の型（attnum 順）
    pub col_types: Vec<SqlType>,
    /// 列リストにない列の DEFAULT。lower_single_rel 済み（INSERT の BoundInsert.defaults と同じ作り方）。DEFAULT のない列は None
    pub defaults: Vec<Option<PhysExpr>>,
    pub checks: Vec<PhysCheck>,
    pub not_null: Vec<bool>,
    pub options: CopyOptions,
}
pub struct CopyOptions {
    pub delimiter: u8,
    /// 既定は b"\\N"
    pub null_string: Vec<u8>,
    pub default_string: Option<Vec<u8>>,
    pub header: bool,
    pub freeze: bool,
}
```

### 5.2 プロトコルの状態機械

メッセージ（PostgreSQL のプロトコル 3.0）:

```
サーバ → G  CopyInResponse   Int8 全体の形式（0 = text）、Int16 列数 n、Int16 × n 各列の形式（すべて 0）
クライアント → d  CopyData   Byte*
クライアント → c  CopyDone
クライアント → f  CopyFail   String（メッセージ）
```

```
                   Query("COPY t FROM STDIN; SELECT 1")
 Idle ───────────────────────────────────────────────────▶  CopyIn         （サーバは G を送り、RFQ を送らない）
                                                              │
        d (任意個)  ─────────────────────────────────────────▶│ 行を処理して挿入。エラーなら ↓
        H（Flush）・S（Sync）─────────────────────────────────▶│ 無視（状態も RFQ も変えない）
        c（CopyDone）─────────────────────────────────────────▶│ 残りを処理 → C "COPY n" → 続きの文を実行 → Z
        f（CopyFail）─────────────────────────────────────────▶│ E 57014 → Z
        X（Terminate）────────────────────────────────────────▶│ トランザクションを中断して接続を閉じる
        それ以外（Q、P、B、…）─────────────────────────────────▶│ E 08P01 + FATAL 08P01、接続を閉じる
                                                              ▼
 エラー（データの誤り、制約違反、キャンセル）は、その場で E と Z を送って Idle に戻る
```

- **エラーのとき**は直ちに `E`（ErrorResponse）と `Z`（ReadyForQuery。ブロック内なら状態 `E`）を送り、`Idle`（またはブロックの失敗状態）に戻る。その後に届く `d` / `c` / `f` は、**アイドル状態で無視する**（PostgreSQL と同じ。[実機]: 不正な行の後に `d` `d` `c` を送ると、`E` の 1 つと `Z` の 1 つだけが返る）。
- 同じ `Query` の続きの文（`COPY t FROM STDIN; SELECT 'after'`）は、CopyDone の後に実行する。[実機]: `C COPY 1`、`D after`、`C SELECT 1`、`Z I` の順。エラー（`f` を含む）の場合は続きの文を実行しない。
- `f` のエラー: **`57014`、`COPY from stdin failed: {クライアントのメッセージ}`、CONTEXT `COPY {table}, line {n}`**（`n` = 読み終えた行数 + 1）[実機]。メッセージが空なら `COPY from stdin failed`（コロンなし。未検証）。
- 想定外のメッセージ（CopyIn 中の `Q` など）: **`08P01 unexpected message type 0x51 during COPY from stdin`**（CONTEXT `COPY {table}, line {n}`）の ERROR に続けて、**`FATAL 08P01 terminating connection because protocol synchronization was lost`**。接続を閉じる [実機]。
- **COPY の開始前の失敗**（存在しない表、列、option、読み取り専用）は、`G` を送らず通常の ErrorResponse + Z [実機]。
- **FREEZE の検査は `G` を送った後**（`G` の直後に `E 55000` と `Z`。PostgreSQL の `BeginCopyFrom` が `G` を送り、続く `CopyFrom` が検査する順 [実機]）。
- 拡張クエリ（`P` `B` `E`）からの COPY は M5。

**サーバ → クライアントのメッセージ**（J）: `BackendMessage::CopyInResponse { format: u8, column_formats: &[i16] }`（タイプ `G`、長さ = 4 + 1 + 2 + 2n）。

**クライアント → サーバのメッセージ**（J）:

```rust
// protocol/messages.rs の FrontendMessage に追加
CopyData(Vec<u8>),          // 'd'
CopyDone,                   // 'c'
CopyFail(String),           // 'f'。本体は NUL 終端の文字列（UTF-8 でなければ空として扱う）
```

`FrontendMessage::Extended(ExtendedKind::Flush)`（`H`）は、**CopyIn 中は無視**、それ以外（アイドル）は M1 のまま（`0A000`）。`Sync`（`S`）は CopyIn 中は無視。アイドル状態では従来どおり ReadyForQuery。

### 5.3 Session の COPY 状態と「続きの文」の再開

```rust
// session.rs
struct PendingQuery {
    /// 元の SQL（エラー位置の解決に使う）
    sql: String,
    /// 同じ Query メッセージの文すべて
    stmts: Vec<Statement>,
    /// CopyDone の後に実行する次の文の添字
    next: usize,
}
struct CopyState {
    copy: copy::CopyIn,
    pending: PendingQuery,
}
// Session のフィールドに追加
copy: Option<Box<CopyState>>,
```

`run_statements` を、添字から始められる形に分ける:

```rust
fn run_statements(&mut self, sql: &str, parsed: Result<Vec<Statement>>, sink: &mut dyn ResultSink) -> io::Result<()> {
    /* パース結果を取り出して self.run_from(PendingQuery { sql: sql.to_owned(), stmts, next: 0 }, sink) */
}
/// pending.next 以降の文を実行する。COPY IN を始めたら self.copy を設定して戻る（RFQ もコミットもしない）
fn run_from(&mut self, pending: PendingQuery, sink: &mut dyn ResultSink) -> io::Result<()>;
```

- `run_from` の内容は M3 の `run_statements` のループ（`self.state == Failed` の検査、暗黙トランザクションの開始、`statement_timeout` の締め切りの設定、`exec_statement`、出力の送信、`command_complete`）と同じ。違いは次だけ。
  - `exec_statement` が `Ok` を返した結果が「COPY IN の開始」（`ExecOutcome::CopyIn(CopyIn)`。タグを返す代わりに）のとき: `sink.copy_in_response(0, &vec![0; ncols])`、`self.copy = Some(CopyState { copy, pending: PendingQuery { next: i + 1, .. } })` を設定し、**`statement_timeout` の締め切りを消さずに** `Ok(())` で戻る。暗黙トランザクションはコミットせず、`flush_parameter_status` もしない。
  - FREEZE の検査は、`copy_in_response` を送った後に `copy.check_freeze(&self.txn)` として行い、失敗したら通常のエラー処理（`report_error`。`self.copy = None`）。
- `exec_statement` の `Statement::Copy`: `write_statement_tag` が `Some("COPY FROM")`（書き込みロックを取る、読み取り専用の検査）。バリアの下で `analyze_copy` → `copy::begin`（`RelHandle` の組み立て）まで行い、`ExecOutcome::CopyIn` を返す。**ストレージバリアはここで手放す**（待ち中に持たない）。
- **各メッセージでのバリア**: `copy_data` / `copy_done` は、`exec_data_statement` の 1〜2、4〜5（書き込みロックは取得済み。共有バリア、スナップショット、`StatementCatalog`）をそのメッセージの間だけ行う。`ExecCtx.query` には `PhysicalQuery::empty()`（サブプランなし）を渡す。

```rust
impl Session {
    /// execute_simple / copy_done の後に、サーバが CopyIn の受信ループに入るか
    pub fn is_copying_in(&self) -> bool;
    /// 1 つの CopyData。完成した行を処理して挿入する。エラーなら report_error して self.copy = None（RFQ はサーバが送る）
    pub fn copy_data(&mut self, chunk: &[u8], sink: &mut dyn ResultSink) -> io::Result<()>;
    /// CopyDone: 残りを処理し、コマンドタグ COPY n を送り、pending の続きを run_from で実行する
    pub fn copy_done(&mut self, sink: &mut dyn ResultSink) -> io::Result<()>;
    /// CopyFail: 57014 で中断
    pub fn copy_fail(&mut self, message: &str, sink: &mut dyn ResultSink) -> io::Result<()>;
    /// サーバが読み取りの待ち（100 ミリ秒のタイムアウト）の度に呼ぶ。キャンセル・statement_timeout・停止要求を見て、
    /// 発生していれば report_error して self.copy = None にする
    pub fn copy_poll(&mut self, sink: &mut dyn ResultSink) -> io::Result<()>;
}
```

- `copy_data` の `chunk` が空でもよい（何もしない）。
- `copy_done` の手順: (1) `copy.finish(&mut ctx, &w)`（未完の最後の行を処理。§5.4）、(2) `self.txn.command_counter_increment()?`、(3) `statement_timeout` の締め切りを消す、(4) `sink.command_complete(&format!("COPY {n}"))`、(5) `let p = self.copy.take().pending;` `self.run_from(p, sink)`（続きの文があれば実行。最後に暗黙トランザクションのコミットと `flush_parameter_status`）。エラーのときは `report_error` して終わり（続きは実行しない）。
- **`terminate()`**（接続断）は `self.copy = None` にしてトランザクションを中断する。
- **`idle_timeout()`**: `self.copy.is_some()` のとき `None`（COPY 中は「アイドル」ではない）。`idle_in_transaction_session_timeout` もかからない。
- **`statement_timeout`**: COPY の開始から CopyDone / エラーまで有効。ソケットの読み取りは `copy_poll`（100 ミリ秒ごと）が締め切りを見るので、データが来ない間でも切れる。
- `transaction_status()` は COPY 中、ブロックの外なら `Idle`、ブロックの中なら `InBlock` のまま（RFQ を送らないので影響しない）。

### 5.4 テキスト形式の解析

`copy/text.rs`（行の読み取り。O1）。**入力はバイト列**で、行の境界と CopyData メッセージの境界は無関係（メッセージの途中で行が切れる、1 つのメッセージに複数行、`\r\n` が 2 つのメッセージにまたがる）。

```rust
pub enum Eol { Nl, CrNl, Cr }
pub struct LineReader {
    buf: Vec<u8>,
    /// buf の中で走査済みの位置（未完の行の再走査を避ける）
    scanned: usize,
    /// 最初の行の終端で決まる。以降の行は同じでなければならない
    eol: Option<Eol>,
    /// \. を見た後。以降のバイトは捨てる
    done: bool,
    /// 読み終えた行数
    lines: u64,
}
impl LineReader {
    pub fn new() -> Self;
    pub fn push(&mut self, chunk: &[u8]);
    /// 完成した次の行（終端を除く）を返す。at_eof は CopyDone を受けた後。None = データ不足（at_eof でなければ次の push を待つ）、または終了
    pub fn next_line(&mut self, at_eof: bool) -> Result<Option<Vec<u8>>>;
    /// 次に読む行の番号（1 始まり）= lines + 1。エラーの CONTEXT に使う
    pub fn line_no(&self) -> u64;
}
```

`next_line` の規則（PostgreSQL の `CopyReadLineText`（PG:src/backend/commands/copyfromparse.c）の非 CSV の部分。すべて [実機]）。`buf[scanned..]` を 1 バイトずつ見る:

1. **`\` の直後**:
   - 次のバイトがまだない → データ不足（`at_eof` なら `\` は行に残して次へ）。
   - `.`: さらに次のバイトを見る（なければデータ不足。`at_eof` なら下の「corrupt」）。
     - 次が行の終端（`\n`、`\r\n`、`\r`）なら**データの終わり**。`\.` の前のバイトが 1 バイト以上あれば、それを**最後の行として返す**（`22\tx\t5\.\n` は行 `22\tx\t5` を処理して終わる）。空なら行は返さず終わり。`done = true`。終端の種類が `eol` と食い違えば `22P04 end-of-copy marker does not match previous newline style`。`eol` が未定ならこの終端で決める（`\.\n` だけのデータも `COPY 0`）。
     - 次が行の終端でない（`\.x`）、またはデータの終わり（`\.` で CopyDone）なら `22P04 end-of-copy marker corrupt`。
   - それ以外: 次のバイトを読み飛ばす（`\` の後の改行がその行を終わらせない。エスケープの意味は §5.4 の 3 で）。
2. **`\n`**: 行の終端。`eol` が未定なら `Nl` に決める。`Nl` なら行を返す。`CrNl` / `Cr` なら `22P04 literal newline found in data`（HINT `Use "\n" to represent newline.`）。
3. **`\r`**: 次のバイトを見る（なければデータ不足。`at_eof` なら `Cr`）。`\n` が続けば `CrNl`（`eol` が `Nl` / `Cr` なら `22P04 literal carriage return found in data`、HINT `Use "\r" to represent carriage return.`）。続かなければ `Cr`（`eol` が `Nl` / `CrNl` なら同じエラー）。
4. その他のバイトは行の一部。
5. `at_eof` で未完のバイトが残っていれば、**終端なしの最後の行**として返す（`D 2\tx\t5` + CopyDone で `COPY 1` [実機]）。残りがなければ `None`。
6. 返した行ごとに `lines += 1`。**`done` の後のバイトは捨てる**（`10\tx\t5\n\.\n11\t...` の `11` 以降は挿入されない [実機]）。
7. **1 行の上限**: 未完の行のバイト数が 64 MiB（`COPY_MAX_LINE = 64 << 20`）を超えたら `54000 COPY line is too long`（PostgreSQL は 1GB。yuzhu の行は 8KB を超えられないので十分）。

**フィールドの分割と値の復元**（`copy/text.rs` の `split_fields`）:

```rust
pub enum Field { Null, Default, Value(Vec<u8>) }
/// 行を delimiter で分け、各フィールドを復元する。区切りの前の `\` は区切りをエスケープする（`\<TAB>`）
pub fn split_fields(line: &[u8], delim: u8, null: &[u8], default: Option<&[u8]>) -> Vec<RawField>;
pub struct RawField<'a> { pub raw: &'a [u8] }
pub fn unescape(raw: &[u8]) -> Vec<u8>;
```

1. 区切り文字で分ける。ただし `\` の次のバイトは区切りとして扱わない（`\` + 区切り文字は値の一部）。
2. 各フィールドの**生のバイト列**（エスケープを解く前）が `null_string` と等しければ `Null`、`default_string` と等しければ `Default`（`\N` の既定: 生のフィールドが `\N`。`\\N` は文字列 `\N`）。
3. それ以外は `unescape`: `\b` `\f` `\n` `\r` `\t` `\v` → 制御文字。`\` + 8 進 1〜3 桁 → その値（& 0xFF）。`\x` + 16 進 1〜2 桁 → その値（16 進がなければ `x` そのもの）。`\\` → `\`。それ以外の `\c` は `c` そのもの。行末の `\` はそのまま `\`。[実機]: `a\tb\x41\101\q` → `a<TAB>bAAq`。
4. 復元したバイト列が UTF-8 でなければ `22021 invalid byte sequence for encoding "UTF8": 0x{xx} ...`（不正なバイト列の先頭から 1〜4 バイト、小文字 16 進、空白区切り）。NUL バイト（`\0`）は `22021 invalid byte sequence for encoding "UTF8": 0x00`。
5. フィールド数が期待（`columns.len()`）と違えば `22P04`: 少ない → **`missing data for column "{最初に足りない列の名前}"`**、多い → **`extra data after last expected column`**。空行は 1 フィールド（空文字列）。

**値の変換**: 各列で `types::io::input_text(s, ty, type_env)` の後に、INSERT の代入と同じ typmod の強制（`varchar(n)` の長さ `22001`、`numeric(p,s)` の桁 `22003` など。INSERT の `CoerceTypmod` が呼ぶ関数と同じものを呼ぶ）。**入力関数のエラーは `22P02`**（`invalid input syntax for type integer: "abc"`）などのまま。

**エラーの CONTEXT**（`Error::with_context`。PostgreSQL の `CopyFromErrorCallback`）:

| 起きた場所 | CONTEXT |
|---|---|
| 列の値の変換（入力関数、typmod、UTF-8） | `COPY {table}, line {n}, column {col}: "{value}"`（NULL なら `column {col}: null input`） |
| 列数の過不足、行全体の問題（`22P04`）、NOT NULL・CHECK・一意制約・DEFAULT の評価 | `COPY {table}, line {n}: "{line}"` |
| 行の読み取り（`end-of-copy marker corrupt`、`literal newline found in data`、CopyFail） | `COPY {table}, line {n}`（行の中身なし） |

- `{table}` はスキーマなしのリレーション名（`COPY cp`）、`{n}` は処理中の行の番号（1 始まり。`header` の行も数える）、`{col}` は列名、`{value}` は復元後の値、`{line}` は行のバイト列（タブなど制御文字はそのまま）。
- **値・行が 100 バイトを超える**ときは、先頭 100 バイト（UTF-8 の文字境界で切る）に `...` を付ける（PostgreSQL の `limit_printout_length`。未検証: 細部）。
- [実機] の例:

```
ERROR:  22P04: missing data for column "c"
CONTEXT:  COPY cp, line 1: "3	y"
ERROR:  22P04: extra data after last expected column
CONTEXT:  COPY cp, line 1: "3	y	1	9"
ERROR:  22P02: invalid input syntax for type integer: "abc"
CONTEXT:  COPY cp, line 1, column c: "abc"
ERROR:  23514: new row for relation "cp" violates check constraint "cp_c_check"
DETAIL:  Failing row contains (5, y, 500).
CONTEXT:  COPY cp, line 1: "5	y	500"
ERROR:  23502: null value in column "a" of relation "cp" violates not-null constraint
DETAIL:  Failing row contains (null, y, 1).
CONTEXT:  COPY cp, line 1: "\N	y	1"
ERROR:  22P04: end-of-copy marker corrupt
CONTEXT:  COPY cp, line 2
ERROR:  22P04: literal newline found in data
CONTEXT:  COPY cp, line 2
ERROR:  57014: COPY from stdin failed: client aborted
CONTEXT:  COPY cp, line 2
```

### 5.5 INSERT 経路の再利用と処理の流れ

`copy/exec.rs`（O1）。1 行の処理（`CopyIn::process_line`）:

1. `split_fields`（§5.4）。`header` なら最初の 1 行（行番号 1）は捨てる。
2. 行バッファ `row: Vec<Datum>`（全列 `Datum::Null`）を作る。入力のフィールド i（`columns[i]` の列）を変換して入れる（`Null` → NULL のまま、`Default` → その列の `defaults[col]` を `eval_const`（なければ NULL）、`Value` → §5.4 の変換）。
3. 列リストにない列は `defaults[col]` を `eval_const`（`nextval` など。`ExecCtx.runtime` を通る）。DEFAULT がなければ NULL。
4. **NOT NULL と CHECK**: INSERT と同じ関数を使う。`executor/dml.rs`（X3）が `pub fn check_row(ctx: &ExecCtx<'_>, checks: &RowChecks<'_>, row: &[Datum]) -> Result<()>`（`RowChecks { not_null: &[bool], checks: &[PhysCheck], table: &TableDef }`）を公開する（00 への変更提案 4。`nodes/insert.rs` も同じ関数を呼ぶ）。エラーの形（`23502`・`23514` の MESSAGE、DETAIL `Failing row contains (...)`、`schema` / `table` / `column` / `constraint` フィールド）は INSERT と同一。
5. `executor::dml::insert_with_indexes(ctx, &rel, &w, &row)`（ヒープへの挿入 + 全インデックスへの挿入、一意性の検査 `23505`）。
6. 失敗したらエラーに CONTEXT（§5.4 の表）を付けて返す。`copy_data` が `report_error`（トランザクションを中断）する。
7. 行ごとに `ctx.check_interrupts()?`。

- **文全体の原子性**: エラーでトランザクションが中断されるので、その COPY の途中までの行は見えない（暗黙トランザクションなら ROLLBACK、ブロックなら失敗状態）。
- 行ごとに WAL を書く（`HEAP_INSERT`、M3）。ページ単位の `heap_multi_insert` 相当は M5。
- カタログ・トリガ・ルールはない。`RETURNING` もない。
- `copy_data` ごとの `command_counter_increment` はしない（同じ COPY の行は同じ `cid`。自分の行を見る検査は `fetch_dirty` が `cid` を見ないので問題ない）。`copy_done` で 1 回。

### 5.6 FREEZE

`FREEZE` を受け付ける（00 D-30）。PostgreSQL の前提条件だけ検査する（`CopyFrom` の冒頭）: 対象の表が**現在のトランザクションで作成または TRUNCATE されている**こと。M4 では `txn.pending_creates` に `table.locator` が含まれているかで判定する（CREATE TABLE と TRUNCATE が新しい relfilenode を `pending_creates` に積むこと。章 07 が保証する）。含まれなければ

```
ERROR:  55000: cannot perform COPY FREEZE because the table was not created or truncated in the current subtransaction
```

[実機]。通れば**凍結せず通常の xmin で挿入する**。`pgbench -i` は `begin; truncate ...; copy ... with (freeze on); commit` なので条件を満たす。

### 5.7 メモリと性能

- 保持するのは `LineReader.buf`（未完の 1 行 + 直近のチャンク）と 1 行分の `row` だけ。`pgbench -i -s 100`（1000 万行）でもメモリは一定。
- `copy_data` は 1 つのチャンク（psql は 8KB、pgbench は 1 行ごと〜数 KB）から取り出せる行をすべて処理してから戻る。1 チャンクあたりのバリアの取得は 1 回。
- `MemBudget`（`query_mem_limit`）の対象外。

### 5.8 キャンセルとタイムアウト

- `CancelRequest` が届くと `InterruptFlag` が立つ。サーバの COPY 受信ループは読み取りに 100 ミリ秒のタイムアウトを付け、タイムアウトのたびに `session.copy_poll(sink)` を呼ぶ。`copy_poll` は `interrupts.check()` を呼び、`Err`（`57014 canceling statement due to user request`、`statement timeout`）なら `report_error`（トランザクションの中断）して `self.copy = None`、`E` `Z` を送らせる。クライアントはその後もデータを送り続けるが、アイドル状態で無視される。
- 停止要求（`is_terminate_requested`）は M3 と同じ（`57P01`）。
- 行の処理の途中の `ctx.check_interrupts()` でも同じエラーが出る。

### 5.9 yuzhu-server の変更（J）

| ファイル | 変更 |
|---|---|
| `protocol/messages.rs` | `FrontendMessage::{CopyData, CopyDone, CopyFail}`、`BackendMessage::CopyInResponse` |
| `protocol/codec.rs` | `d` `c` `f` の読み取り（`f` は NUL 終端の文字列）、`G` の書き出し。最大メッセージ長の検査は既存のもの |
| `connection.rs` | `message_loop` に CopyIn の分岐（下）。`Sink` に `copy_in_response`。エラーの符号化に `s` `t` `c` `n` `W` を追加（下） |

`message_loop` の変更:

```rust
// Query の処理の後（M3 のコード）
FrontendMessage::Query(sql) => {
    session.execute_simple(&sql, &mut Sink::new(writer))?;
    if session.is_closing() { return writer.flush(); }
    if !session.is_copying_in() { ready_for_query(writer, session)?; }     // 追加
}
// ループの先頭の読み取りの前（追加）
if session.is_copying_in() {
    // 読み取りのタイムアウトを 100 ミリ秒にして、タイムアウトなら session.copy_poll(..) → 終わっていれば RFQ
}
// メッセージの振り分け（追加）
FrontendMessage::CopyData(b) if session.is_copying_in() => { session.copy_data(&b, &mut sink)?; if !session.is_copying_in() { ready_for_query(..)?; } }
FrontendMessage::CopyDone     if session.is_copying_in() => { session.copy_done(&mut sink)?;    if !session.is_copying_in() { ready_for_query(..)?; } }
FrontendMessage::CopyFail(m)  if session.is_copying_in() => { session.copy_fail(&m, &mut sink)?; ready_for_query(..)?; }
FrontendMessage::Extended(ExtendedKind::Flush) | FrontendMessage::Sync if session.is_copying_in() => {}      // 無視
FrontendMessage::CopyData(_) | CopyDone | CopyFail(_) => {}                    // アイドル状態: 無視（D10-11）
// CopyIn 中のそれ以外（Query、Terminate 以外の拡張クエリなど）
_ if session.is_copying_in() => { send_error(08P01, "unexpected message type 0x.. during COPY from stdin") + CONTEXT; send_fatal(08P01, "terminating connection because protocol synchronization was lost"); return Ok(()) }
```

- `Terminate` は M3 のまま（`session.terminate()` は `run_connection` が呼ぶので COPY も中断される）。
- `copy_done` が続きの文を実行して再び COPY IN を始める場合（`COPY a FROM STDIN; COPY b FROM STDIN`）は、`is_copying_in()` が真のままなので RFQ を送らず、次の `d` を待つ [実機: 2 つの COPY が順に動いた]。
- **ErrorResponse の追加フィールド**（00 D-27、§14.4）: `ErrorFields` に `schema` `table` `column` `constraint` `context` を足し、`write_core_error` が出す順は PostgreSQL と同じ **`S` `V` `C` `M` `D` `H` `P` `W`（context）`s`（schema）`t`（table）`c`（column）`n`（constraint）**。`NoticeResponse` も同じ関数。

### 5.10 psql の `\copy` と pgbench のデータ

- `\copy t FROM 'file'`（クライアント側）は、psql が `COPY t FROM STDIN` を送り、ファイルの内容を 8192 バイトずつ `d` で送り、最後に `c` を送る（`\.` 行は付けない）。
- `pgbench -i` は表ごとに `copy pgbench_branches from stdin with (freeze on)` を送り、行ごとに `d`（`1\t0\t\N\n`、tellers は `1\t1\t0\t\N\n`、accounts は `1\t1\t0\t\n`。filler は空文字列で `char(n)` が空白で埋まる）を送り、最後に `\.\n` を `d` で送ってから `c` を送る [pgbench.c L4964-L5110]。`\.\n` の後の `c` で `COPY n` が返る。

---

## 6. psql の `\dt` 系

### 6.1 psql 17 が送る SQL（実機で採取。`psql -E`）

psql は接続時に SQL を送らない。以下は psql 17.11 が `server_version = 17.x` に対して送った**全文**（空白・改行・大文字小文字もそのまま）。yuzhu は `server_version = 17.0`（M2-Q7）なので同じ文が来る。

**`\dt`**:

```sql
SELECT n.nspname as "Schema",
  c.relname as "Name",
  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index' WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' WHEN 'I' THEN 'partitioned index' END as "Type",
  pg_catalog.pg_get_userbyid(c.relowner) as "Owner"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
WHERE c.relkind IN ('r','p','')
      AND n.nspname <> 'pg_catalog'
      AND n.nspname !~ '^pg_toast'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;
```

**`\dt t*`**（パターンつき。`\dt public.*` は `AND n.nspname OPERATOR(pg_catalog.~) '^(public)$' COLLATE pg_catalog.default`、`\dtS` は `relkind IN ('r','p','t','s','')` で `n.nspname` の除外がない）:

```sql
SELECT n.nspname as "Schema",
  c.relname as "Name",
  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index' WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' WHEN 'I' THEN 'partitioned index' END as "Type",
  pg_catalog.pg_get_userbyid(c.relowner) as "Owner"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
WHERE c.relkind IN ('r','p','t','s','')
  AND c.relname OPERATOR(pg_catalog.~) '^(t.*)$' COLLATE pg_catalog.default
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;
```

**`\di`**:

```sql
SELECT n.nspname as "Schema",
  c.relname as "Name",
  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index' WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' WHEN 'I' THEN 'partitioned index' END as "Type",
  pg_catalog.pg_get_userbyid(c.relowner) as "Owner",
  c2.relname as "Table"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
     LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = c.oid
     LEFT JOIN pg_catalog.pg_class c2 ON i.indrelid = c2.oid
WHERE c.relkind IN ('i','I','')
      AND n.nspname <> 'pg_catalog'
      AND n.nspname !~ '^pg_toast'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;
```

**`\dn`**:

```sql
SELECT n.nspname AS "Name",
  pg_catalog.pg_get_userbyid(n.nspowner) AS "Owner"
FROM pg_catalog.pg_namespace n
WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
ORDER BY 1;
```

**`\l`**（M2 で対応済み。`\l` の SQL は 17 では `datlocale` と `daticurules` の列を使う）:

```sql
SELECT
  d.datname as "Name",
  pg_catalog.pg_get_userbyid(d.datdba) as "Owner",
  pg_catalog.pg_encoding_to_char(d.encoding) as "Encoding",
  CASE d.datlocprovider WHEN 'b' THEN 'builtin' WHEN 'c' THEN 'libc' WHEN 'i' THEN 'icu' END AS "Locale Provider",
  d.datcollate as "Collate",
  d.datctype as "Ctype",
  d.datlocale as "Locale",
  d.daticurules as "ICU Rules",
  CASE WHEN pg_catalog.array_length(d.datacl, 1) = 0 THEN '(none)' ELSE pg_catalog.array_to_string(d.datacl, E'\n') END AS "Access privileges"
FROM pg_catalog.pg_database d
ORDER BY 1;
```

**任意**（`\dt+` `\du` `\df`）:

```sql
-- \dt+ は \dt の SELECT に次の列を足し、WHERE は同じ
  CASE c.relpersistence WHEN 'p' THEN 'permanent' WHEN 't' THEN 'temporary' WHEN 'u' THEN 'unlogged' END as "Persistence",
  am.amname as "Access method",
  pg_catalog.pg_size_pretty(pg_catalog.pg_table_size(c.oid)) as "Size",
  pg_catalog.obj_description(c.oid, 'pg_class') as "Description"

-- \du
SELECT r.rolname, r.rolsuper, r.rolinherit,
  r.rolcreaterole, r.rolcreatedb, r.rolcanlogin,
  r.rolconnlimit, r.rolvaliduntil
, r.rolreplication
, r.rolbypassrls
FROM pg_catalog.pg_roles r
WHERE r.rolname !~ '^pg_'
ORDER BY 1;

-- \df
SELECT n.nspname as "Schema",
  p.proname as "Name",
  pg_catalog.pg_get_function_result(p.oid) as "Result data type",
  pg_catalog.pg_get_function_arguments(p.oid) as "Argument data types",
 CASE p.prokind
  WHEN 'a' THEN 'agg'
  WHEN 'w' THEN 'window'
  WHEN 'p' THEN 'proc'
  ELSE 'func'
 END as "Type"
FROM pg_catalog.pg_proc p
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
WHERE pg_catalog.pg_function_is_visible(p.oid)
      AND n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
ORDER BY 1, 2, 4;
```

**`\d tbl`**（任意。10 本。11 本目（外部キー）と 12 本目（トリガ）は表にフラグが立っているときだけ）: 1 表名の解決（`c.relname OPERATOR(pg_catalog.~) '^(tt)$' COLLATE pg_catalog.default AND pg_catalog.pg_table_is_visible(c.oid)`）、2 表の属性（`WHERE c.oid = '18400'` — **文字列リテラルと oid の比較**）、3 列（`format_type`、スカラーサブクエリの `pg_get_expr(d.adbin, d.adrelid, true)`、`pg_collation` と `pg_type` のサブクエリ、`attidentity`、`attgenerated`）、4 インデックス（`pg_get_indexdef(i.indexrelid, 0, true)`、`pg_get_constraintdef(con.oid, true)`、`LEFT JOIN pg_constraint con ON (conrelid = i.indrelid AND conindid = i.indexrelid AND contype IN ('p','u','x'))`）、5 CHECK、6 行レベルセキュリティ（`pol.polroles = '{0}'`、`array(select rolname ... = any (pol.polroles) ...)`）、7 拡張統計、8 出版物（UNION 3 本、`string_agg`、`generate_series`、`pr.prattrs::pg_catalog.int2[]`、`prattrs[s]`）、9・10 継承。**6 と 8 は配列型・`ANY`・`regclass` が解析できないと失敗する**ので、M4 の範囲では `\d tbl` は動かない部分がある（確認事項 [10-Q6]）。全文は `tests/compat/psql/` の期待出力の生成時に採取する（§8.3）。

### 6.2 必要な機能と、各テスト

`\dt` `\dn` `\di` `\l` が動くために、次が必要（持ち主の章を付記）。**どれか 1 つでも欠けると、psql は何も表示せずエラーを出す**。

| 機能 | 使う文 | 持ち主の章 | slt のテスト（`tests/slt/m4/psql/`） |
|---|---|---|---|
| `pg_class` の `relkind`（`"char"`）、`relowner`、`relam`、`relpersistence`、`relnamespace` に実データ。テーブルは `'r'`、インデックスは `'i'`、シーケンスは `'S'` | 全部 | 07 | `catalog_relkind.slt` |
| `pg_am`: `heap`（oid 2、`amtype = 't'`）と `btree`（403、`'i'`）の 2 行。**`pg_class.relam`**: テーブルは 2（heap）、インデックスは 403（btree）、シーケンスは 0 | `\dt` `\di` | 07 | `pg_am.slt` |
| `pg_index`（`indexrelid`、`indrelid`、…） | `\di` | 07 | `dt_di.slt` |
| `pg_namespace` と、`public` の所有者が `pg_database_owner`（oid 6171）であること（`\dn` の `Owner` 列が PostgreSQL 17 と同じ `pg_database_owner`） | `\dn` | 07（M2 の bootstrap の確認） | `dn.slt` |
| `pg_catalog.pg_get_userbyid(oid)`、`pg_catalog.pg_table_is_visible(oid)`、`pg_catalog.pg_encoding_to_char(int)`、`pg_catalog.array_length(anyarray, int)`、`pg_catalog.array_to_string`（`datacl` が NULL でも動く） | 全部 | 09 | `functions.slt` |
| `LEFT JOIN`（1 SELECT に 2〜4 個）、列の別名つきの SELECT 句、`ORDER BY 1,2` | 全部 | 03 | `dt_di.slt` |
| 単純な `CASE c.relkind WHEN 'r' THEN ... END`（`"char"` と unknown リテラルの比較） | `\dt` | 09 | `case_relkind.slt` |
| `c.relkind IN ('r','p','')`（`''` は `"char"` の空 = `\0`。`"char"` の入力 `''` を受け付ける）、`IN ('i','I','')`、`IN ('r','p','t','s','')` | `\dt` `\di` | 09 | `relkind_in.slt` |
| 正規表現 `!~` と `~`: `n.nspname !~ '^pg_toast'`（`name` と unknown/text）、`c.relname OPERATOR(pg_catalog.~) '^(t.*)$' COLLATE pg_catalog.default` | `\dt` `\dn` | 09（D-22 の手書きエンジン）、03（`OPERATOR(pg_catalog.~)` と `COLLATE pg_catalog.default` の構文。`COLLATE "default"` は無視して通す） | `regex_names.slt` |
| `<> 'pg_catalog'`、`!~ '^pg_'`（`name` 型の比較・演算子） | `\dn` | 09 | `name_ops.slt` |
| `pg_proc` の `pg_function_is_visible`（`\df`、任意） | `\df` | 09 | `df.slt`（任意） |

**slt の形**（`tests/slt/m4/psql/dt.slt` など）: psql の SQL をそのまま使うが、**自分の表だけに絞る**ため `WHERE` に `AND c.relname LIKE 'psql_t_%'` を足す（他のテストの残りに依存しない。M2-Q22）。結果の行（スキーマ、名前、種類、所有者）を PostgreSQL と yuzhu の両方で比べる。`tests/compat/psql/` が全文を psql 17 で流す（§8.3）。

**psql 側の確認**（`tests/compat/psql/`。§8.3）: 実機の psql 17 の `\dt` `\dn` `\di` `\l` の**出力そのもの**を、PostgreSQL と yuzhu で比べる。

### 6.3 任意の機能（M4 後半）

| コマンド | 追加で必要 | 備考 |
|---|---|---|
| `\dt+` | `pg_size_pretty(bigint)`、`pg_table_size(oid)`（ヒープのブロック数 × 8192。PostgreSQL は TOAST・FSM・VM を含むので Size 列は一致しない。テストは Size 列を伏せる）、`obj_description(oid, text)`（`pg_description` は空。章 07） | 関数は SQL で呼べればよい |
| `\d tbl` | §6.1 の 10 本が通る: 配列型（`oid[]`・`int2[]` の演算、`= ANY`、`array(select ...)`、添字）は M5 なので、**6 と 8 の本が通らない**。`\d tbl` を M4 で動かすには「配列のキャストと `ANY` の最小実装」が要る（確認事項 [10-Q6]）。それ以外は `pg_get_indexdef`、`pg_get_constraintdef`、`pg_get_expr`（pretty = true）、`format_type`、`regclass` / `regtype` のキャストと、中身が空のカタログ表（`pg_policy`、`pg_statistic_ext`、`pg_publication*`、`pg_inherits`）が要る | 動くのは「索引・CHECK・既定値の表示」まで |
| `\df` | `pg_get_function_result`、`pg_get_function_arguments`。ユーザー定義関数がない間は `pg_catalog` を除外するので 0 行 | 結果が 0 行でも解析を通ること |
| `\du` | `pg_roles`（ビューまたは仮想表）。M2 で対応済み | |

---

## 7. pgbench と互換テスト（`tests/compat/`）

### 7.1 `pgbench -i` が送る SQL と必要な機能

pgbench 17.11 に `log_statement = 'all'` の PostgreSQL へ流して採取した [実機]。`pgbench -i -s 1`（既定の手順 `dtgvp`）:

| 手順 | 文 | 必要な機能（持ち主の章） | 失敗したとき |
|---|---|---|---|
| `d` | `drop table if exists pgbench_accounts, pgbench_branches, pgbench_history, pgbench_tellers` | M1 | 中止 |
| `t` | `create table pgbench_history(tid int,bid int,aid    int,delta int,mtime timestamp,filler char(22))` | `timestamp`、`char(n)`（09） | 中止 |
| `t` | `create table pgbench_tellers(tid int not null,bid int,tbalance int,filler char(84)) with (fillfactor=100)`、`pgbench_accounts(aid    int not null,bid int,abalance int,filler char(84)) with (fillfactor=100)`、`pgbench_branches(bid int not null,bbalance int,filler char(88)) with (fillfactor=100)` | `WITH (fillfactor=N)`（検証して捨てる。07） | 中止 |
| `g` | `begin` | | |
| `g` | `truncate table pgbench_accounts, pgbench_branches, pgbench_history, pgbench_tellers` | TRUNCATE（複数表、トランザクション内。07） | 中止 |
| `g` | `copy pgbench_branches from stdin with (freeze on)`、`copy pgbench_tellers from stdin with (freeze on)`、`copy pgbench_accounts from stdin with (freeze on)` | **COPY FROM STDIN、`FREEZE`（§5）** | 中止（`unexpected copy in result` など） |
| `g` | `commit` | | |
| `v` | `vacuum analyze pgbench_branches`（`_tellers`、`_accounts`、`_history` の 4 本） | VACUUM / ANALYZE を何もせず成功（D-11。07）。**失敗すると pgbench は中止する** | 中止 |
| `p` | `alter table pgbench_branches add primary key (bid)`、`pgbench_tellers add primary key (tid)`、`pgbench_accounts add primary key (aid)` | `ALTER TABLE ... ADD PRIMARY KEY`（D-12。07）。インデックスの構築（06） | 中止 |

`-I dtGvp`（サーバ側でデータを生成）の `G` の手順（`g` の代わり。`begin` と `truncate` は同じ）[実機、`-s 2`]:

```sql
insert into pgbench_branches(bid,bbalance) select bid, 0 from generate_series(1, 2) as bid
insert into pgbench_tellers(tid,bid,tbalance) select tid, (tid - 1) / 10 + 1, 0 from generate_series(1, 20) as tid
insert into pgbench_accounts(aid,bid,abalance,filler) select aid, (aid - 1) / 100000 + 1, 0, '' from generate_series(1, 200000) as aid
```

（`FROM generate_series(1, N) AS bid` の **`AS bid` が関数の別名であり列名になる**（FROM 句の関数の別名規則。03）。`'' → char(84)` は INSERT の代入キャストで空白に埋まる。）

### 7.2 組み込みスクリプト tpcb-like（`-M simple`）の SQL と必要な機能

`pgbench -c N -T S` は、実行前に次を送る [実機。`pgbench -n` は VACUUM と TRUNCATE を送らない]:

| 文 | 失敗したとき |
|---|---|
| `select count(*) from pgbench_branches` | **中止**（スケールとして使う） |
| パーティションの確認: `select o.n, p.partstrat, pg_catalog.count(i.inhparent) from pg_catalog.pg_class as c join pg_catalog.pg_namespace as n on (n.oid = c.relnamespace) cross join lateral (select pg_catalog.array_position(pg_catalog.current_schemas(true), n.nspname)) as o(n) left join pg_catalog.pg_partitioned_table as p on (p.partrelid = c.oid) left join pg_catalog.pg_inherits as i on (c.oid = i.inhparent) where c.relname = 'pgbench_accounts' and o.n is not null group by 1, 2 order by 1 asc limit 1` | **続行する**（「パーティションなし」とみなす）。**ErrorResponse を返して接続が使えれば良い** [実機: 偽のサーバで、この問い合わせに `0A000` を返しても pgbench は次の文へ進んだ] |
| `vacuum pgbench_branches`、`vacuum pgbench_tellers`、`truncate pgbench_history` | 警告を出して続行（失敗しても可。ただし M4 では何もせず成功させる） |

**パーティション確認の問い合わせは `CROSS JOIN LATERAL`（M4 では `0A000`）を含むのでパースの時点で失敗する。これは想定どおりの動きで、pgbench は続行する。** pgbench のログに `pgbench: error: ERROR:  ...` と `(ignoring this error and continuing anyway)` が出るが、完走には影響しない [実機]。

本体（`-M simple`。`\set` は pgbench が値を計算し、`:aid` などをリテラルに置き換えてから送る）:

```sql
BEGIN;
UPDATE pgbench_accounts SET abalance = abalance + -400 WHERE aid = 4441;
SELECT abalance FROM pgbench_accounts WHERE aid = 4441;
UPDATE pgbench_tellers SET tbalance = tbalance + -400 WHERE tid = 7;
UPDATE pgbench_branches SET bbalance = bbalance + -400 WHERE bid = 1;
INSERT INTO pgbench_history (tid, bid, aid, delta, mtime) VALUES (7, 1, 4441, -400, CURRENT_TIMESTAMP);
END;
```

- 1 文ずつ別の Simple Query。`END` はコミット。`abalance + -400` の `+ -400` は、`-` が前置演算子か負の定数かを区別せず `int4 + int4` に解決できること。
- `CURRENT_TIMESTAMP`（`timestamptz`）を `timestamp` 列へ入れる代入キャスト（09。`INSERT` の代入でのみ許される）。
- `aid` への主キー検索（B+Tree がなければ全件走査で遅いが動く）、`UPDATE ... WHERE aid = N`。
- `-M extended` と `-M prepared` は Extended Query が要るので M5。`-c 4`: 単一ライター（M3）で直列化されるが正しく動く。

### 7.3 完走の条件（00 D-25）

次をすべて満たす。`tests/compat/pgbench/` が確かめる。

1. `pgbench -i -s 1 DB` が終了コード 0。続けて `select count(*) from pgbench_accounts` が 100000、`pgbench_branches` が 1、`pgbench_tellers` が 10、`pgbench_history` が 0。主キー（`\d` ではなく `select conname from pg_constraint where conrelid = 'pgbench_accounts'::regclass` が `pgbench_accounts_pkey`）がある。
2. `pgbench -i -I dtGvp DB` も同様（`-s 1`）。
3. `pgbench -c 4 -T 30 -M simple DB` が終了コード 0、出力に `number of failed transactions: 0 (0.000%)`。
4. 終了後の不変条件: `select (select sum(abalance) from pgbench_accounts), (select sum(tbalance) from pgbench_tellers), (select sum(bbalance) from pgbench_branches), (select sum(delta) from pgbench_history)` の 4 つがすべて等しい [実機: `-10385|-10385|-10385|-10385`]。`select count(*) from pgbench_history` = `number of transactions actually processed`。
5. サーバが PANIC・FATAL を出さず、`SIGTERM` で正常に止まる。

### 7.4 `tests/compat/` の構成と比較方法

```
tests/compat/
├── README.md        サーバが受けた文の記録の取り方（log_statement = 'all'）と、新しいツールへの対応の手順
├── run.sh           tests/compat/run.sh --target pg|yuzhu [suite ...]  （suite = psql | pgbench。省略は全部）
├── lib.sh           接続先の起動（PG は sandbox/pg.sh または tests/pg.sh、yuzhu は tests/yuzhu.sh）、DB の作成、正規化
├── psql/
│   ├── dt.sql  dn.sql  di.sql  l.sql  dtplus.sql(任意)  d_tbl.sql(任意)   psql -X に流すスクリプト（メタコマンドを含む）
│   └── expected/*.out                        PostgreSQL 17 で生成した出力（git に入れる）
├── copy/
│   ├── *.sql                                 COPY のデータを `\.` で含む psql スクリプト（text 形式の解析、エラーの CONTEXT、FREEZE、続きの文）
│   └── expected/*.out                        PostgreSQL 17 の出力（OID・時刻を正規化）
└── pgbench/
    ├── init.sh          -i -s 1、-i -I dtGvp -s 1 と、7.3 の 1〜2
    ├── run.sh           -c 4 -T 30 -M simple（CI では -T 10）と 7.3 の 3〜5
    └── invariants.sql   7.3 の 4
```

- **psql の比較**: `psql -X -q -d $DB -f psql/dt.sql` の標準出力を、正規化してから `expected/dt.out` と `diff` する。正規化: `\l` の `Name`・`Owner` 以外の列と `Access privileges`（yuzhu は M4 では GRANT がない）、`\dt+` の `Size`、OID、データベース名。**テーブルの内容は各スクリプトの先頭で `create table compat_*`、末尾で `drop` する**（他のスクリプトの影響を受けない。空の DB を作ってから流す: `createdb compat_psql`）。期待出力は `run.sh --target pg --update`（PostgreSQL に流して `expected/` を作り直す）で作る。
- **psql / pgbench の入手**: ホスト・コンテナでは `PATH` の `psql` / `pgbench`（17 系であること）。CI では `postgres:17` の docker イメージ内のものを使う。`PGHOST` `PGPORT` `PGUSER` で接続先を切り替える。
- **pgbench の比較**: 出力の数値（tps、レイテンシ）は比べない。終了コードと 7.3 の 1〜5 の SQL の結果だけ（PostgreSQL と yuzhu の両方で）。
- CI: yuzhu のジョブで `tests/compat/run.sh --target yuzhu` を実行する（`-T 10`）。PostgreSQL のジョブは期待出力が最新かの確認（`--target pg` が `expected/` と一致）。

---

## 8. テスト

### 8.1 共通の SQL テスト（`tests/slt/m4/`。K の担当。期待値は PostgreSQL 17 で確かめる）

| ディレクトリ / ファイル | 内容 | 実行するターゲット |
|---|---|---|
| `explain/options.slt` | §3.1 の option の解釈と、エラー（`unrecognized EXPLAIN option`、`requires a Boolean value`、`TIMING requires ANALYZE`、`unrecognized value for EXPLAIN option "format"`）。`EXPLAIN (COSTS OFF) SELECT 1` の出力 | 両方（`FORMAT json` の成功ケースは `skipif yuzhu`、`0A000` のケースは `onlyif yuzhu`） |
| `explain/nodes.slt` | §3.11 の 1〜10 の出力。PostgreSQL 側は §8.2 の設定を先頭で流して形をそろえる。yuzhu と PostgreSQL で一致するものは両方、差があるものは `onlyif yuzhu`（期待値を yuzhu 用に別に書く） | 両方 / yuzhu |
| `explain/verbose.slt` | §3.5〜3.8 の修飾と `Output:` | 両方（`Inner Unique` を含むものは `onlyif yuzhu` で別の期待値） |
| `explain/analyze.slt` | `EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF)`: `actual rows=N loops=L`、`Rows Removed by Filter`、`never executed`、DML が実際に書くこと（続く `SELECT count(*)`）、ROLLBACK で戻ること | 両方（`Sort Method` などを含むものは PostgreSQL 側の行を除いた別の期待値で `onlyif yuzhu`） |
| `explain/subplans.slt`、`explain/cte.slt`、`explain/dml.slt` | §3.11 の 8〜10 | 同上 |
| `explain/deparse_stored.slt` | §4.9 の表: 各 `CHECK` 式を `ALTER TABLE ... ADD CONSTRAINT` し、`pg_get_constraintdef(oid, false)` と `(oid, true)`、`pg_get_expr` を取る | **両方** |
| `explain/deparse_plan.slt` | §4.9 の `Plan` の表: `EXPLAIN (COSTS OFF) SELECT * FROM t WHERE <式>` の `Filter:` 行 | 両方（畳み込みや並べ替えが PostgreSQL と違う行は `onlyif yuzhu`） |
| `explain/format_type.slt` | `format_type` の表（§4.8） | 両方 |
| `copy/errors.slt` | **`G` の前に失敗する**もの: 存在しない表・列、`42701`、option の誤り（§5.1 の表）、読み取り専用（`25006`）、`COPY TO` / CSV の `0A000`（yuzhu のみ） | 両方 / yuzhu |
| `psql/dt.slt`、`dn.slt`、`di.slt`、`l.slt` | §6.1 の SQL を、自分の表（`psql_t_*`）に絞って流す。`catalog_relkind.slt`、`pg_am.slt`、`case_relkind.slt`、`relkind_in.slt`、`regex_names.slt`、`name_ops.slt`、`functions.slt`（§6.2 の各行） | 両方 |

- COPY のデータを送るテストは sqllogictest ではできない（`tests/compat/copy/`。§8.3）。
- EXPLAIN の出力は 1 行 1 結果行なので `query T` で比べる。時刻・メモリなどの値を含むものは使わない（`TIMING OFF, SUMMARY OFF`）。

### 8.2 PostgreSQL の側で形をそろえる設定

yuzhu が作らないノード（`Bitmap Heap Scan`、`Index Only Scan`、`Merge Join`、`Memoize`、`Gather`）を PostgreSQL が選ばないようにする。EXPLAIN の slt ファイルの先頭に次を置く（`skipif yuzhu` で PostgreSQL にだけ流す）:

```
skipif yuzhu
statement ok
SET enable_bitmapscan = off; SET enable_indexonlyscan = off; SET enable_memoize = off; SET enable_mergejoin = off; SET enable_tidscan = off; SET max_parallel_workers_per_gather = 0; SET jit = off
```

- テーブルは小さく、`ANALYZE` した後に流す（yuzhu では `ANALYZE` は何もしない）。インデックススキャンを強制したいときは `SET enable_seqscan = off`（yuzhu も `enable_seqscan` を読む）。NestedLoop を強制するときは `SET enable_hashjoin = off`。`GroupAggregate` は `SET enable_hashagg = off`。
- 一致しない形は、同じ文に `onlyif yuzhu` の期待値を別に書く。**一致させることが目的ではなく、書式（ノード名、詳細行の並び、字下げ、式の文字列）の一致が目的**。

### 8.3 互換テスト（`tests/compat/`。K の担当。構成は §7.4）

| スイート | 内容 |
|---|---|
| `psql/` | psql 17 の `\dt` `\dn` `\di` `\l`（と任意の `\dt+` `\d tbl`）の出力を、PostgreSQL と yuzhu で比べる。空の DB に `compat_*` の表（主キー、UNIQUE、通常のインデックス、複数スキーマ）を作ってから流す |
| `copy/` | psql スクリプトでデータを `\.` つきで流し、PostgreSQL と yuzhu の出力（`COPY n` と、エラーの `ERROR:` `CONTEXT:`）を比べる。ケース: `\t` `\N` `\\` `\x41` `\101` の復元、`\r\n` の行末、`\.` の途中終了（`22\tx\t5\.\n`）、`end-of-copy marker corrupt`、`literal newline found in data`、列数の過不足、列リスト、`NULL 'NA'` と `DELIMITER '\|'`、旧構文の `WITH NULL AS`、`COPY ...; SELECT` の続き、FREEZE の可否（`begin; truncate; copy ... freeze; commit`）、`HEADER`、NOT NULL・CHECK・UNIQUE・型変換のエラーの CONTEXT、`\copy t from file` |
| `pgbench/` | §7.3 |

### 8.4 Rust のテスト

| 対象 | 観点 |
|---|---|
| `deparse`（E1） | §4.9 の表の全行を `Stored`（非 pretty・pretty）と `Plan` で。`Bound` / `LExpr` を手で組み立てて文字列を比べる（アナライザなしの単体テスト）。定数（`'-1'::integer`、`1.50`、`'1'::numeric`、`NULL::integer`、配列定数のクォート `{"a b",NULL,"NULL"}`）。識別子のクォート（キーワード、大文字、`"` を含む名前）。`is_simple` の全組み合わせ（算術の優先度、`BoolExpr` どうし、`NullTest` の親、キャストの引数）。`CASE` の字下げ（入れ子 2 段）。`Plan` と `Stored` の違い（IN、LIKE ESCAPE、根の暗黙キャスト） |
| `explain/format.rs`（E1） | 手で組み立てた `ExplainNode` の木から行を作る。字下げ（通常の子、InitPlan / SubPlan / CTE のラベルつきの子、入れ子）、コスト欄の空白、`ANALYZE` の 3 つの形、`never executed`、`Rows Removed` の位置、`Planning Time` / `Execution Time`、`TIMING OFF` |
| `executor/instrument.rs`（E1） | `loops` と `rows` の平均（`rewind` を挟む）、`never executed`、`startup` / `total` の平均、`running` でないノードの `EndLoop` が何もしないこと |
| `planner/explain_tree.rs`（L2。E1 と共同） | §3.3 の表を 1 行 1 テスト（物理プランを手で作り、タイトル・詳細行・子の順を確かめる）。群（Filter・Project の吸収）、`build_is_left` の表示、InitPlan / SubPlan / CTE の置き場所、修飾の 3 つの規則（`n_rtable` と `verbose` の組み合わせ）、同じ別名の `_1` |
| `copy::text::LineReader`（O1） | **チャンク分割の同値性**: 同じ入力を、1 バイトずつ、ランダムな位置、全体を 1 つの `push` で渡して、同じ行の並び（とエラー）になること（乱数のシードを固定した性質テスト）。`\r\n` が 2 つのチャンクにまたがる、`\.` と終端が別のチャンク、`\` で終わるチャンク。§5.4 の規則 1〜7 を 1 つずつ |
| `copy::text::split_fields` / `unescape`（O1） | 表のとおりの復元（`\N`、`\\N`、`\b \f \n \r \t \v`、8 進 1〜3 桁、`\x` 1〜2 桁と `\xZ`、`\q`、末尾の `\`、`\<TAB>`）。NUL と不正な UTF-8（`22021`）。フィールド数の過不足 |
| `copy::exec`（O1） | `Session` なしで `CopyIn` を `TestCluster` のストレージに対して動かし、行数・DEFAULT・NOT NULL・CHECK・一意制約・CONTEXT の文字列を確かめる |
| `Session` の COPY（S） | 状態機械（§5.2 の図の全辺）: begin → data → done、`G` の列数、エラー後の `is_copying_in() == false`、`copy_fail` の 57014 と CONTEXT、続きの文（`COPY ...; SELECT 'after'`、`COPY a ...; COPY b ...`）、暗黙トランザクションと `BEGIN` の中、`terminate` での中断、`statement_timeout` を `copy_poll` が検出、`idle_timeout()` が COPY 中は `None`、FREEZE の可否、読み取り専用 |
| `yuzhu-server` のプロトコル（J。`yuzhu-server/tests/copy_protocol.rs`） | 実際の TCP で §5.2 の全辺: `G` のバイト列、`d` を細切れに、`c`、`f`、エラー後の `d` `c` が無視され `Z` が 1 つだけ返る、CopyIn 中の `Q` が `08P01` + FATAL、`H` / `S` が無視される、アイドル中の `d` `c` `f` が無視される、ErrorResponse の `W` `s` `t` `n` フィールド |
| `INERT_GUCS`（S） | 全項目に対して、既定値の `SHOW`、`SET` で bool / int（単位つき）/ real / enum の検証とエラー（§9 の表）、`RESET`、`SET LOCAL`、ロールバックでの復帰 |

---

## 9. 受け付けるだけの GUC

`settings.rs`（S の担当）に、**PostgreSQL 17 にあって yuzhu では意味を持たない設定**の一覧 `INERT_GUCS` を持つ。`SET` は値の形を検査して保存し、`SHOW` / `current_setting` / `pg_settings` はそれを返す。意味を持つもの（M1〜M3 の `SETTINGS`、00 §15.4 の `enable_seqscan` `enable_indexscan` `enable_hashjoin` `enable_nestloop` `enable_hashagg` `enable_sort` `enable_material`、`yuzhu.query_mem_limit`）はこの一覧に入れない。

```rust
// settings.rs
pub enum InertKind {
    Bool,
    /// 値の単位（基準の単位の整数で保存する）と範囲（基準の単位で）
    Int { unit: Unit, min: i64, max: i64 },
    Real { unit: Unit, min: f64, max: f64 },
    Enum(&'static [&'static str]),
    Str,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit { None, Kb, Block8Kb, Bytes, Ms, Sec }       // Block8Kb は 8kB 単位の整数
pub struct InertGuc {
    pub name: &'static str,
    pub default: &'static str,
    pub kind: InertKind,
}
pub static INERT_GUCS: &[InertGuc];
/// PostgreSQL に存在するが SET できない（postmaster / sighup / internal）名前。SET は 55P02、SHOW は既定値
pub static RESTART_ONLY_GUCS: &[&str];
```

**`SET name = value` の規則**（PostgreSQL 17 [実機]）:

| 種類 | 受け付ける値 | エラー（SQLSTATE `22023`） |
|---|---|---|
| `Bool` | `true` / `false` / `on` / `off` / `yes` / `no` / `1` / `0` と、一意に決まる接頭辞（`parse_bool`。M1 の `settings::parse_bool` を使う）。`SHOW` は `on` / `off` | `parameter "geqo" requires a Boolean value` |
| `Int` | 整数、または単位つきの文字列（`'5MB'`、`'1s'`、`'1h'`）。単位のある設定で単位なしの整数は基準の単位（`work_mem` は kB、`statement_timeout` は ms）。小数は不可（単位つきで割り切れれば可: `1.5MB` = 1536kB）。メモリの単位 `B` `kB` `MB` `GB` `TB`、時間の単位 `us` `ms` `s` `min` `h` `d` | 範囲外: `63 kB is outside the valid range for parameter "work_mem" (64 kB .. 2147483647 kB)`（単位のないものは `0 is outside the valid range for parameter "default_statistics_target" (1 .. 10000)`）。形が不正: `invalid value for parameter "work_mem": "abc"` |
| `Real` | 小数 | 範囲外: `-1 is outside the valid range for parameter "random_page_cost" (0 .. 1.79769e+308)`。形が不正: `invalid value for parameter "random_page_cost": "x"` |
| `Enum` | 一覧のどれか（大文字小文字を区別しない。引用符つき可）。`on` / `off` / `true` / `false` / `yes` / `no` / `1` / `0` を受け付ける enum（`synchronous_commit` など）は `on` / `off` との同義語を PostgreSQL の表に合わせる（M4 では `wal_compression`、`constraint_exclusion` の `on` / `off` と `true` / `false` / `yes` / `no`） | `invalid value for parameter "synchronous_commit": "foo"` + HINT `Available values: local, remote_write, remote_apply, on, off.` |
| `Str` | 任意の文字列（検査しない） | |

- **`SHOW` の表示**（`Int` / `Real` で単位があるもの）: 基準の単位の値を、**割り切れる最大の単位**で表示する。メモリ: `B` → `kB`（1024）→ `MB` → `GB` → `TB`（`1024kB` は `1MB`、`1500kB` は `1500kB`、`1.5MB` は `1536kB`）。時間: `us` → `ms`（1000）→ `s` → `min`（60）→ `h` → `d`（`vacuum_cost_delay = 0.5` は `500us`）。`Block8Kb`（`effective_cache_size`、`temp_buffers`）は 8kB 単位の整数で保存し、メモリの表示規則を適用する（`4GB`）。0 は `0`（単位なし）。
- **カスタム名**（`.` を含む名前。`my.custom`）は M1 のとおり任意（`is_custom`）。
- **未知の名前**: `42704 unrecognized configuration parameter "nosuch_guc"`。`enable_foo` のような `enable_` で始まる未知の名前も同じ。
- **`RESTART_ONLY_GUCS`**: `max_connections`、`shared_buffers` など PostgreSQL では `SET` できないもの（`SELECT name FROM pg_settings WHERE context IN ('postmaster', 'sighup', 'internal')` の 182 件。M1〜M3 の `ReadOnly` と同じ扱い）: `SET` は `55P02 parameter "max_connections" cannot be changed without restarting the server`（`internal` は `parameter "x" cannot be changed`）。実装は PostgreSQL 17 の一覧を `settings.rs` に静的に持つ（名前だけ。`SHOW` は M1〜M3 の `ReadOnly` の項目だけ値を返し、他は既定値を返す。値は `postgresql.conf` の既定）。
- `SET` の対象が `superuser` 文脈の項目でも、単一の管理者ロールだけなので常に許す。
- `transaction_timeout` は M3 の `42704` をやめ、`Int { Ms, 0, 2147483647 }` の保存だけにする（D10-8）。

**一覧**（`name = 既定値`。単位は基準の単位。PostgreSQL 17.11 の `pg_settings` から採取。`user` と `superuser` と `backend` の文脈のうち、M1〜M3 の `SETTINGS` と意味を持つものを除いた 168 件）:

**Bool**（60）:

```
allow_in_place_tablespaces=off  allow_system_table_mods=off  array_nulls=on  check_function_bodies=on  debug_pretty_print=on
debug_print_parse=off  debug_print_plan=off  debug_print_rewritten=off  enable_async_append=on  enable_bitmapscan=on
enable_gathermerge=on  enable_group_by_reordering=on  enable_incremental_sort=on  enable_indexonlyscan=on  enable_memoize=on
enable_mergejoin=on  enable_parallel_append=on  enable_parallel_hash=on  enable_partition_pruning=on  enable_partitionwise_aggregate=off
enable_partitionwise_join=off  enable_presorted_aggregate=on  enable_tidscan=on  escape_string_warning=on  event_triggers=on
exit_on_error=off  geqo=on  ignore_checksum_failure=off  ignore_system_indexes=off  jit=on
jit_debugging_support=off  jit_dump_bitcode=off  jit_expressions=on  jit_profiling_support=off  jit_tuple_deforming=on
lo_compat_privileges=off  log_connections=off  log_disconnections=off  log_duration=off  log_executor_stats=off
log_lock_waits=off  log_parser_stats=off  log_planner_stats=off  log_replication_commands=off  log_statement_stats=off
parallel_leader_participation=on  quote_all_identifiers=off  row_security=on  synchronize_seqscans=on  trace_notify=off
trace_sort=off  track_activities=on  track_counts=on  track_io_timing=off  track_wal_io_timing=off
transform_null_equals=off  update_process_title=on  wal_init_zero=on  wal_recycle=on  zero_damaged_pages=off
```

**Int**（54。`name = 既定 [単位] [最小..最大]`）:

```
backend_flush_after=0 [8kB 0..256]  client_connection_check_interval=0 [ms 0..2147483647]  commit_delay=0 [0..100000]
commit_siblings=5 [0..1000]  debug_discard_caches=0 [0..0]  default_statistics_target=100 [1..10000]
effective_cache_size=524288 [8kB 1..2147483647]  effective_io_concurrency=1 [0..1000]  from_collapse_limit=8 [1..2147483647]
geqo_effort=5 [1..10]  geqo_generations=0 [0..2147483647]  geqo_pool_size=0 [0..2147483647]  geqo_threshold=12 [2..2147483647]
gin_fuzzy_search_limit=0 [0..2147483647]  gin_pending_list_limit=4096 [kB 64..2147483647]  io_combine_limit=16 [8kB 1..32]
join_collapse_limit=8 [1..2147483647]  log_min_duration_sample=-1 [ms -1..2147483647]  log_min_duration_statement=-1 [ms -1..2147483647]
log_parameter_max_length=-1 [B -1..1073741823]  log_parameter_max_length_on_error=0 [B -1..1073741823]  log_temp_files=-1 [kB -1..2147483647]
logical_decoding_work_mem=65536 [kB 64..2147483647]  maintenance_io_concurrency=10 [0..1000]  maintenance_work_mem=65536 [kB 64..2147483647]
max_parallel_maintenance_workers=2 [0..1024]  max_parallel_workers=8 [0..1024]  max_parallel_workers_per_gather=2 [0..1024]
max_stack_depth=2048 [kB 100..2147483647]  min_parallel_index_scan_size=64 [8kB 0..715827882]  min_parallel_table_scan_size=1024 [8kB 0..715827882]
post_auth_delay=0 [s 0..2147]  scram_iterations=4096 [1..2147483647]  tcp_keepalives_count=9 [0..2147483647]
tcp_keepalives_idle=7200 [s 0..2147483647]  tcp_keepalives_interval=75 [s 0..2147483647]  tcp_user_timeout=0 [ms 0..2147483647]
temp_buffers=1024 [8kB 100..1073741823]  temp_file_limit=-1 [kB -1..2147483647]  transaction_timeout=0 [ms 0..2147483647]
vacuum_buffer_usage_limit=2048 [kB 0..16777216]  vacuum_cost_limit=200 [1..10000]  vacuum_cost_page_dirty=20 [0..10000]
vacuum_cost_page_hit=1 [0..10000]  vacuum_cost_page_miss=2 [0..10000]  vacuum_failsafe_age=1600000000 [0..2100000000]
vacuum_freeze_min_age=50000000 [0..1000000000]  vacuum_freeze_table_age=150000000 [0..2000000000]
vacuum_multixact_failsafe_age=1600000000 [0..2100000000]  vacuum_multixact_freeze_min_age=5000000 [0..1000000000]
vacuum_multixact_freeze_table_age=150000000 [0..2000000000]  wal_sender_timeout=60000 [ms 0..2147483647]
wal_skip_threshold=2048 [kB 0..2147483647]  work_mem=4096 [kB 64..2147483647]
```

**Real**（18）:

```
cpu_index_tuple_cost=0.005 [0..1.79769e+308]  cpu_operator_cost=0.0025 [0..1.79769e+308]  cpu_tuple_cost=0.01 [0..1.79769e+308]
cursor_tuple_fraction=0.1 [0..1]  geqo_seed=0 [0..1]  geqo_selection_bias=2 [1.5..2]  hash_mem_multiplier=2 [1..1000]
jit_above_cost=100000 [-1..1.79769e+308]  jit_inline_above_cost=500000 [-1..1.79769e+308]  jit_optimize_above_cost=500000 [-1..1.79769e+308]
log_statement_sample_rate=1 [0..1]  log_transaction_sample_rate=0 [0..1]  parallel_setup_cost=1000 [0..1.79769e+308]
parallel_tuple_cost=0.1 [0..1.79769e+308]  random_page_cost=4 [0..1.79769e+308]  recursive_worktable_factor=10 [0.001..1e+06]
seq_page_cost=1 [0..1.79769e+308]  vacuum_cost_delay=0 [ms 0..100]
```

**Enum**（19）:

```
backslash_quote=safe_encoding {safe_encoding,on,off}
compute_query_id=auto {auto,regress,on,off}
constraint_exclusion=partition {partition,on,off}
debug_logical_replication_streaming=buffered {buffered,immediate}
debug_parallel_query=off {off,on,regress}
default_toast_compression=pglz {pglz,lz4}
icu_validation_level=warning {disabled,debug5,debug4,debug3,debug2,debug1,log,notice,warning,error}
log_error_verbosity=default {terse,default,verbose}
log_min_error_statement=error {debug5,debug4,debug3,debug2,debug1,info,notice,warning,error,log,fatal,panic}
log_min_messages=warning {debug5,debug4,debug3,debug2,debug1,info,notice,warning,error,log,fatal,panic}
log_statement=none {none,ddl,mod,all}
password_encryption=scram-sha-256 {md5,scram-sha-256}
plan_cache_mode=auto {auto,force_generic_plan,force_custom_plan}
session_replication_role=origin {origin,replica,local}
stats_fetch_consistency=cache {none,cache,snapshot}
track_functions=none {none,pl,all}
wal_compression=off {pglz,lz4,zstd,on,off}
xmlbinary=base64 {base64,hex}
xmloption=content {content,document}
```

**Str**（17。検査なし）:

```
backtrace_functions=''  createrole_self_grant=''  default_table_access_method=heap  default_tablespace=''
default_text_search_config=pg_catalog.english  dynamic_library_path=$libdir  extension_destdir=''  lc_monetary=C  lc_numeric=C  lc_time=C
local_preload_libraries=''  output_plugin_libraries='pgoutput, test_decoding'  restrict_nonsystem_relation_kind=''
session_preload_libraries=''  temp_tablespaces=''  timezone_abbreviations=Default  wal_consistency_checking=''
```

- ツール別の確認: pg_dump の接続時（`synchronize_seqscans` `statement_timeout` `lock_timeout` `idle_in_transaction_session_timeout` `transaction_timeout` `row_security` `restrict_nonsystem_relation_kind`）と出力の先頭（`client_encoding` `standard_conforming_strings` `check_function_bodies` `xmloption` `client_min_messages` `default_tablespace` `default_table_access_method` `search_path`）、pgAdmin（`DateStyle` `client_min_messages` `bytea_output` `client_encoding`）、Rails（`standard_conforming_strings` `intervalstyle` `client_min_messages` `timezone`）、pgJDBC（`extra_float_digits` `application_name`）、Prisma（`server_version_num`）はすべて、M1〜M3 の `SETTINGS` かこの一覧にある。
- **`pg_settings` の仮想表**（M2）にこの一覧の行を足す（`name`、`setting`、`unit`、`category` は空でよい、`vartype`、`min_val`、`max_val`、`enumvals`、`context`、`boot_val`）。`psql` の `\dconfig` や pg_dump が読む。

---

## 10. セッションとサーバの変更（まとめ）

詳細は各節。担当ごとの一覧:

| 担当 | 変更 |
|---|---|
| **S**（`session.rs`、`settings.rs`、`engine.rs`、`testing.rs`） | `PendingQuery` / `CopyState`、`run_statements` → `run_from` の分割（§5.3）。`is_copying_in` / `copy_data` / `copy_done` / `copy_fail` / `copy_poll`（00 §14.5 + `copy_poll`）。`ResultSink::copy_in_response`（00 §14.5）。`exec_explain`（§3.10）。`write_statement_tag` に `Copy` と `Explain`。`idle_timeout()` の COPY 中の `None`。`INERT_GUCS`（§9）。`Settings::planner_settings` と `type_env`（00 §14.2）。`Statement::Copy` / `Statement::Explain` の振り分け |
| **J**（`yuzhu-server/`） | §5.9 の表。`CopyInResponse`、`CopyData` / `CopyDone` / `CopyFail` の読み書き、`message_loop` の COPY の分岐と読み取りのタイムアウト、ErrorResponse の追加フィールド（`s` `t` `c` `n` `W`）、`FrontendMessage::Unknown` の扱いは M3 のまま（CopyIn 中の想定外は `08P01`） |
| **E1** | `deparse/*`、`explain/{mod,format,node}.rs`、`executor/instrument.rs`、`pg_get_expr` などの SQL 関数の実体（関数の行は章 09 / 07） |
| **L2** | `planner/explain_tree.rs`（§3.3〜3.9 の表の実装） |
| **O1** | `copy/{mod,text,exec}.rs`、`analyze_copy` |
| **S1** | `Explain` / `Copy` の AST とパーサ（§3.1、§5.1） |

---

## 11. 実装の分担と工数

00 §17 の担当 E1・O1・S・J のこの章の部分。日数は AI の実装エージェント 1 本の粗い見積もり。

| 担当 | 日数 | 中身 | 依存 |
|---|---|---|---|
| **E1** | 5（pretty を含めて 6） | deparse 2.5（定数・演算子・キャスト・括弧・`CASE`・識別子・`format_type`・保存式の経路）、explain の整形 1（字下げ、コスト欄、ANALYZE の 3 形、ラベルつきの子）、instrument 1、`explain/node.rs`（タイトルの関数、`count_rtes`）0.5。テスト 含む | P0（`Expr<C,Q>`、`ExplainNode`）、L2（`explain_tree.rs` を並行して。E1 が `explain/node.rs` の関数と `ColumnNamer` を先に置く） |
| **L2**（explain_tree の部分） | 2（00 の L2 の 5 日とは別に、explain の作成に） | §3.3〜3.9 の表の実装、`ExplainBuildCtx`、`ArenaNamer` | E1 の `deparse`、物理化 |
| **O1** | 4 | `analyze_copy` 0.5、`LineReader` と `split_fields` 1.5、`CopyIn`（行の処理、defaults・checks・挿入）1、テスト 1 | P0、X3（`insert_with_indexes`、`check_row`）、S（`ResultSink`） |
| **S**（この章の分） | 3（00 の S の 4 日のうち） | COPY の状態と再開 1.5、`exec_explain` 0.5、`INERT_GUCS` 1 | A、P0、C1、O1 の `CopyIn` |
| **J** | 1.5 | プロトコルの追加、受信ループ、エラーのフィールド、プロトコルのテスト | S |
| **S1**（この章の分） | 1 | `Explain` / `Copy` の構文 | A |
| **K**（この章のテスト） | 3（00 の K の 10 日のうち） | `tests/slt/m4/{explain,copy,psql}`、`tests/compat/{psql,copy,pgbench}`、`lib.sh`、CI | なし（PostgreSQL で先に期待値を作る） |

- **進め方**: (1) A が `ExplainNode` の拡張（§3.2）と `Cast.implicit` を置く → (2) S1 の構文、E1 の `deparse`（`Stored`）と `format_type` を P0 と独立に始める（`Expr<C,Q>` が置かれてから）、O1 の `LineReader` と `split_fields`（アナライザなしで書ける）、K のテスト → (3) P0 のマージ後に E1 の explain、L2 の explain_tree、O1 の `CopyIn`、S の COPY 状態 → (4) J → (5) `tests/compat`（psql、pgbench）を通す。
- **クリティカルパス**: pgbench の完走（§7.3）は C1（`ALTER TABLE ADD PRIMARY KEY`、TRUNCATE、VACUUM の受け付け、`WITH (fillfactor)`）、T2（`char(n)`、`timestamp`）、B2（インデックス構築）、O1、S、J がそろってから。それまでは `pgbench -n -f custom.sql` で COPY なしの負荷を流せる（`pg-compat-tools.md` §3.2.3）。

---

## 12. 未検証の点（実装前に確かめるもの）

- PostgreSQL の `CopyFail` のメッセージが空のときの文言（`COPY from stdin failed` か `COPY from stdin failed: `）。
- COPY の `default` option と区切り文字の組み合わせの SQLSTATE と文言（`COPY delimiter character must not appear in the DEFAULT specification`）。
- `limit_printout_length` の細部（100 バイトで切る位置、`...` の付け方）。
- 同じ位置で `E` と `Z` を返す順序（PostgreSQL は `E` の直後に `Z` を送ると推定。クライアントが見える順序は同じ）。
- 非 pretty の `get_rule_expr_paren` に関する推定: 表 §4.6 の `is_simple` のうち、`BoolExpr` を `FuncExpr` 以外の親（`ScalarArrayOp` など）の下に置いたときの括弧、`CASE` の `WHEN` の位置の `BoolExpr`。実機で確かめたのは §4.9 の表のものだけ。
- `n_rtable`（§3.5）の数え方の端: FROM なしの文、RTE_RESULT になる副問い合わせ、`WITH` の中の `Rte`、`UPDATE ... FROM` の対象表。`SELECT 1 WHERE EXISTS (SELECT 1 FROM t)` のようなトップレベルに `Rte` がない文での `Output` の修飾。
- `Update` / `Delete` の VERBOSE の `Output`（§3.8）が PostgreSQL 17.11 の形で、`RETURNING` つきの `UPDATE` では `Output:` が増える形（`Output: t.a, ...`）。
- `pg_get_expr` の第 3 引数 `pretty` が `NULL` のときの扱い（`false` として）。
- `explain (analyze)` の `rows=` の丸め（`{:.0}` が PostgreSQL の `%.0f` と同じか。ちょうど .5 のとき）。
- 1 つの Query に `COPY` が複数ある文の `command_complete` の順序（実機では `COPY 0 COPY 0` が出たが `D` との交錯は未確認）。
- `SET default_transaction_read_only = on` の後の `EXPLAIN (ANALYZE) SELECT 1` が通ること（PostgreSQL は通った [実機]。yuzhu の `write_statement_tag` が SELECT で `None` を返すこと）。
- yuzhu のロール名: `\dt` の Owner 列、`\l` の Owner 列が PostgreSQL 側のテスト DB と同じロール名（`postgres`）になること（`tests/compat` の正規化で伏せるか、同じ名前にそろえる）。
- 章 09 が混合幅の整数演算子（`int84gt` など）と、SQL 構文の関数の行（`current_timestamp`）を持つこと（§4.4、§4.5）。
- `transaction_timeout` を受け付けることが M3 の `m3.md` §1.2・`tests/slt/m3` の既存のテストと食い違わないこと（`42704` を期待するテストがないか）。

---

## 13. 確認事項

ユーザーの不在中に仮決めしたことです。仮決めのままでよいか確認してください（`11-tests-plan.md` が `M4-Q` の通し番号に振り直します）。この章にディスク形式（★）に関わるものはありません。

- **[10-Q1] `ExplainNode` を PostgreSQL の表示用の木にする**（D10-1）。仮決め: `PhysicalPlan` と同形ではなく、`Project` / `Filter` を吸収し `Hash` などを足す。計測値との対応は `exec_id`。理由: PostgreSQL にない `Project` / `Filter` のノードを出さないため。変えたい場合の影響: 同形にすると、PostgreSQL と形の違う EXPLAIN になり、`tests/slt/m4/explain` の期待値の大半を yuzhu 専用にする必要がある。
- **[10-Q2] ANALYZE の出力を `actual` と `Rows Removed` だけにする**（D10-6）。仮決め: `Sort Method`・`Buckets`・`Batches`・`Memory Usage` を出さない。理由: 値が実装依存で一致しない。変えたい場合: 各ノードが追加の計測（ソートの種類、ハッシュ表のサイズ）を持つ必要があり、`executor/` の複数のノードに手が入る（約 1〜2 日）。
- **[10-Q3] `BUFFERS` `WAL` `SETTINGS` `MEMORY` `SERIALIZE` を受け付けて無視する**（D10-7）。理由: pgAdmin・DBeaver が付ける。変えたい場合: 出さないままで拒否（`0A000`）にすると、それらのツールの EXPLAIN が使えない。
- **[10-Q4] `FORMAT JSON` / `XML` / `YAML` を `0A000` にする**。仮決め: M4 はテキストのみ（要件の D-20）。理由: JSON は pgAdmin・DBeaver・explain.depesz.com が使うが、M4 の範囲外。変えたい場合: `ExplainNode` から JSON を出す整形を足す（約 2 日。構造は同じなので `format.rs` に出力関数を足すだけ）。
- **[10-Q5] COPY の失敗の後に即座に `E` と `Z` を返し、後続の `d` `c` `f` を無視する**（D10-11）。FREEZE の検査は `G` を送った後（PostgreSQL と同じ順）。理由: PostgreSQL と同じ。変えたい場合: 「CopyDone まで受信して捨ててから `E`」にすると、データを送らないクライアントがハングする。
- **[10-Q6] `\d tbl` は M4 の完了条件に入れない**（D-24 のとおり任意）。仮決め: 動くのは「索引・CHECK・既定値の表示」まで。`\d tbl` が送る 10 本のうち、配列型（`oid[]`・`int2[]`・`= ANY`・`array(select ...)`・添字）を使う 2 本（行レベルセキュリティと出版物）が M5 の配列の実装まで通らない。理由: 配列の最小実装は M5 の範囲（`00 §3`）。変えたい場合: 「配列リテラルのキャストと `ANY` の最小実装」を M4 に足す（約 5 日。章 09 の範囲）。
- **[10-Q7] COPY の CSV・バイナリ・`TO`・`WHERE`・`ON_ERROR` を M5 にする**（D10-12）。理由: M4 の目的（pgbench の初期化とリストア）に不要。変えたい場合: CSV は約 3 日、`TO STDOUT` は約 1〜2 日、`WHERE` は約 0.5 日。pg_dump のリストア（`psql -f` で流す出力は COPY FROM STDIN のテキスト形式）は M4 で通る。
- **[10-Q8] `transaction_timeout` を受け付けて保存だけにする**（D10-8）。仮決め: M3 の「`42704`」を上書き。理由: pg_dump 17 が接続直後に `SET transaction_timeout = 0` を送る。変えたい場合: `42704` のままだと pg_dump 17 が接続できない（M6 の対象だが、`psql -f` で流すダンプにも `SET transaction_timeout = 0;` が入る）。
- **[10-Q9] 混合幅の整数演算子の有無で、式の表示が変わる**。仮決め: 章 09 が `int2`/`int4`/`int8` の混合幅の比較・算術演算子を持つ前提で、`bi > 5` は `(bi > 5)` と出す。理由: PostgreSQL がそうする。変えたい場合: 演算子がなければ `Cast` が入って `(bi > '5'::bigint)`（`Plan`）になり、EXPLAIN の期待値が PostgreSQL と違う（機能には影響しない）。
- **[10-Q10] `pretty`（括弧の最小化）を作る**（D10-5）。理由: psql の `\d tbl` と、`pg_get_expr(.., true)` を使うツール（SQLAlchemy の reflection など）が使う。変えたい場合: 作らなければ `pretty = true` でも非 pretty の出力を返し、`\d` の `CHECK ((n > (0)::numeric))` が PostgreSQL（`CHECK (n > 0::numeric)`）と違う表示になる（約 1 日の節約）。
- **[10-Q11] pgbench のパーティション確認の失敗に頼る**（§7.2）。仮決め: `CROSS JOIN LATERAL` が `0A000` で失敗しても pgbench は続行する（偽のサーバで [実機] 確認）。理由: LATERAL は M4 の対象外。変えたい場合: pgbench の将来の版が失敗で中止するようになったら、`LATERAL` の最小実装か、この問い合わせだけを認識して空の結果を返す特例（`pg-compat-tools.md` §3.1 は推奨しない）が要る。
- **[10-Q12] COPY の 1 行の長さの上限を 64 MiB にする**（`54000`）。理由: yuzhu の行は 8KB 以内（TOAST なし）なので PostgreSQL の 1GB は不要で、メモリの使いすぎを防ぐ。変えたい場合: 値を変えるだけ。
- **[10-Q13] `tests/slt/m4/psql/` は自分の表に絞った問い合わせ、`tests/compat/psql/` は psql の出力そのもの**（D10-14）。理由: slt の DB は共有され、他のテストの表が残る（M2-Q22）。

---

## 14. 00 への変更提案と、他章への依頼

### 14.1 00 への変更提案（統合時に反映）

1. **`ExplainNode` の拡張**（§3.2）。00 §9.3 の `ExplainNode { title, details: Vec<(String, String)>, output, children }` を、`details: Vec<ExplainDetail>`、`exec_id: usize`、`width: u32` を持つ形に置き換える。`ExplainChild` は 00 のまま。`planner::physical::assign_exec_ids(&PhysicalQuery) -> ExecIds` を足す。00 の「`PhysicalPlan` と同形」の記述は「表示用の木」に改める。
2. **計測のための `Executor` と `ExecCtx` の追加**（§3.10）。`Executor::set_counters(&mut self, id: usize, instr: &Rc<Instrumentation>)`（既定は何もしない）、`ExecCtx.instr: Option<Rc<Instrumentation>>`、`executor::instrument::{Instrumentation, NodeCounters, Instrumented, build_instrumented}`。`PhysicalQuery::empty()`（COPY の `ExecCtx.query` 用。サブプランなし）。
3. **`ExprKind::Cast` に `implicit: bool` を足す**（§4.5）: `Cast { expr, method, implicit }`。アナライザが暗黙のキャストを挿入するときに `true`。`Stored` の deparse が根の暗黙のキャストを隠すために使う。
4. **`executor/dml.rs` に `check_row` を公開する**（§5.5）: `pub struct RowChecks<'a> { pub not_null: &'a [bool], pub checks: &'a [PhysCheck], pub table: &'a TableDef }`、`pub fn check_row(ctx: &ExecCtx<'_>, c: &RowChecks<'_>, row: &[Datum]) -> Result<()>`。INSERT のノードと COPY が共有する（X3 の担当。00 §10 の `insert_with_indexes` の隣）。
5. **`ResultSink` と `Session` の COPY の追加**（00 §14.5）に次を足す: `Session::copy_poll(&mut self, sink) -> io::Result<()>`、`PendingQuery`、`run_from`。`BackendMessage::CopyInResponse`、`FrontendMessage::{CopyData, CopyDone, CopyFail}`（00 §4 の yuzhu-server の記述に追記）。
6. **`BoundCopy` と `CopyOptions` の定義**（§5.1）。00 §7 の `pub struct BoundCopy { /* 10 */ }` を埋める。
7. **`transaction_timeout`**（§9、D10-8）。M3 の `42704` を、受け付けて保存だけに変える。00 §15.4 の `INERT_GUCS` の記述に「M1〜M3 の `SETTINGS` にない名前は `42704`、`RESTART_ONLY_GUCS` は `55P02`」を追記。
8. **`n_rtable` の計算**（§3.5）: `explain::node::count_rtes(&BoundStatement) -> usize` を E1 が持つ。`analyzer` に変更はない。

### 14.2 他章への依頼

| 宛先 | 依頼 |
|---|---|
| 02（P0、A） | 上の 1〜3、`PhysicalQuery::empty()`、`planner::physical::assign_exec_ids`。`Statement::Explain` の分岐（`explain::resolve_options` を呼ぶ）と `Statement::Copy` の分岐（`copy::analyze_copy` を呼ぶ）の口。 |
| 03（S1、N1〜N3） | `FROM generate_series(1, N) AS bid` の別名が列名になること（pgbench `-I G`）。`OPERATOR(pg_catalog.~)`、`COLLATE pg_catalog.default` の構文（`default` の照合順序名を無視して通す）。`Explain` の AST（§3.1）と `Copy` の AST（§5.1）。 |
| 04（L1、L2） | `explain_tree.rs` の実装（§3.3〜3.9）。`want_explain` のときだけ作る。`PhysicalQuery.subplans[i].explain` と `PhysicalQuery.explain`。FROM 句の関数・VALUES・CTE の `alias`（別名）を `LogicalPlan` から引けること。InitPlan の持ち主（その問い合わせ階層の根）が分かること。`SubPlanStrategy::Hashed` の `hashed SubPlan` の表示。 |
| 05（X1〜X3） | 上の 2、4。`set_counters` を `Filter`・`SeqScan`・`IndexScan`・結合の executor が実装すること。 |
| 07（C1） | `pg_class.relam`（テーブル 2、インデックス 403、シーケンス 0）、`pg_am`（`heap` と `btree`）、`pg_index`、`pg_description`（空）、`pg_roles`、`public` の所有者 `pg_database_owner`。TRUNCATE と CREATE TABLE が新しい relfilenode を `Transaction.pending_creates` に積むこと（§5.6 の FREEZE の検査）。`WITH (fillfactor = N)`、`ALTER TABLE ... ADD PRIMARY KEY`、複数表の TRUNCATE、VACUUM / ANALYZE を何もせず成功。`pg_get_expr` / `pg_get_constraintdef` / `pg_get_indexdef` / `format_type` の関数の行（`pg_node_tree` を受ける）。 |
| 09（T1〜T3） | `current_date` / `current_timestamp` / `localtimestamp` を `now` とは別の `BuiltinFunction` の行にする（§4.4）。`int2` / `int4` / `int8` の混合幅の比較・算術演算子（§4.5）。`"char"` の入力が `''`（空 = `\0`）を受け付けること。`pg_get_userbyid`、`pg_table_is_visible`、`pg_encoding_to_char`、`array_length`、`array_to_string`（`datacl` が NULL で動く）、`pg_size_pretty`、`pg_table_size`、`obj_description`、`pg_function_is_visible`（`\df` 用）。`name` 型の `~` `!~` `<>`。`regclass` 定数の `relation_name`。 |
| 11（K、Z） | §8 の全項目。`tests/compat/` の `run.sh --update`。EXPLAIN の slt の先頭の PostgreSQL 側の設定（§8.2）。 |

---
