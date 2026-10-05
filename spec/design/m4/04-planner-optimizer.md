# 04 プランナとルールベース最適化

`spec/design/m4/` の 1 章です。`00-contracts.md` の署名・名前に従います（足りないものは追加し、食い違いは末尾「00 への変更提案」に書きます）。

- 担当: **L1**（論理プランの構築 `build.rs`、ルール `rules/*`、共通部品 `util.rs`、プランの印字 `print.rs`）と **L2**（物理化 `physicalize.rs`、インデックス選択 `index_select.rs`、サイズの手がかり `size.rs`、EXPLAIN の木 `explain_tree.rs`）
- 前提（必読）: `00-contracts.md` §6（式の木）、§7（Bound）、§8（論理プラン）、§9（物理プラン）、§11.3（opclass の表）、`spec/research/m4-query.md` §4・§6.5・§7・§8.2、`m4-btree.md` §9.1、現在の `planner/`（M1 の `plan_select`）
- 表記: 「（検証済み）」は PostgreSQL 17.11（`sandbox/pg.sh start`、C ロケール）に SQL を流して確かめたもの。「（未検証）」は確かめていないもの。PostgreSQL のソースは `PG:<path>`（REL_17_STABLE）と書く。
- 他章との境界: 実行は `05-executor.md`、EXPLAIN の整形（インデント・`COSTS`・`ANALYZE`）と式の逆変換（deparse）は `10-explain-copy-compat.md`、Bound の作り方は `03-parser-analyzer.md`、B+Tree の比較関数と `operator_strategy` は `06-btree.md`。この章は「Bound を受け取り、`PhysicalQuery`（と `ExplainNode`）を返すまで」。

---

## 1. 範囲

### 1.1 この章が決めること

| 分類 | 内容 |
|---|---|
| 入口 | `planner::plan(&BoundStatement, &PlanEnv) -> Result<PhysicalQuery>` と、テスト用の `plan_traced` |
| build | Bound → 論理プラン。範囲表の列を `ColId` に付け替え、JOIN・集約・DISTINCT・ORDER BY・LIMIT・集合演算・CTE・サブクエリ式・INSERT / UPDATE / DELETE（と RETURNING）を `LogicalPlan` にする |
| ルール | 固定順に 1 回ずつ: 定数畳み込み → サブクエリの結合化 → 派生表の展開 → 外部結合の内部結合化 → 述語の押し下げ → 結合キーの正規化 → 結合順序 → 列の刈り込み |
| 物理化 | 論理プラン → 物理プラン。インデックス選択、結合方法とビルド側の選択、集約・DISTINCT ON・集合演算・CTE・SubPlan の分類、`ParamId` の割り当て、DML |
| EXPLAIN の木 | 物理化と同時に `ExplainNode` を作る（`PlanEnv.want_explain` のとき） |
| 設定 | `PlannerSettings`（`enable_*`）の意味 |
| テスト | `plan_golden`、ルール単体、結合順序、プラン変種、計画時間 |

### 1.2 この章が決めないこと

- 式の評価・各ノードの実行・メモリ予算（`05-executor.md`）。この章は「`PhysicalPlan` の各ノードが何を持つか」までを決め、その実行の意味は 00 §9.2 のコメントと 05 に従う。
- EXPLAIN の整形と deparse の実装（`10-explain-copy-compat.md`）。この章は `ExplainNode` に何を入れるかを決める。
- Bound の作り方、`levels_up` の数え方、関数従属の検査（`03-parser-analyzer.md`）。この章は 00 §7 の Bound を入力として仮定する（§4.1 に仮定の一覧）。
- B+Tree の探索と比較関数（`06-btree.md`）。この章は `ResolvedScanKeys` に載る形の `IndexScanKeys` を作る。
- コストベースの選択（M6）。M4 のプランナは統計を持たない（D-17）。

### 1.3 M4 で作る・作らない

| 作る | 作らない（M5 以降。実行すると `0A000` または計画しない） |
|---|---|
| `Get` の走査: Seq Scan、Index Scan（等値・範囲・`IS NULL`、前向き・後ろ向き） | Index Only Scan、Bitmap Scan、`IN (...)` / `OR` / `LIKE 'abc%'` のインデックス検索 |
| 結合: Hash Join（INNER / LEFT / RIGHT 型 / FULL / SEMI / ANTI）、Nested Loop（内側 Materialize）、Nested Loop + 内側の Index Scan | マージ結合、並列 |
| 集約: Aggregate、HashAggregate、GroupAggregate（`enable_hashagg = off`） | ソート済み入力の GroupAggregate の再利用、MIN/MAX のインデックス化 |
| DISTINCT / DISTINCT ON（Hash / Sort + Unique） | Top-N ソート |
| サブクエリ: Semi / Anti 結合化、InitPlan、Rescan の SubPlan、ハッシュ化 SubPlan | `EXISTS` → ハッシュ化 `ANY` への変換、LATERAL、相関 CTE の共有 |
| CTE: インライン / CteScan | WITH RECURSIVE |
| インデックス順によるソートの省略（後半・任意 S） | — |

---

## 2. 決定（この章で追加で決めたこと）

D-13、D-15〜D-19（00 §2）は前提とし、ここに書かないものを決める。ID は `04-D<n>`。

| # | 決定 | 理由・選択肢・出典 |
|---|---|---|
| 04-D1 | **`ColId` は「列の値の識別子」とする**。パススルーする列は `ColId` を引き継ぐ: `Get` の列 → `Project` の単純な列参照（`Column(c)` の式）→ `Aggregate` の単純な列の group key。1 つの `Project` の出力に同じ `ColId` が 2 回出るときは、2 回目から新しい `ColId` を発行する（`select a, a from t`）。計算した列・集約の結果・集合演算の出力・外部結合の NULL 側のコピーは作らず、新しい `ColId` は計算列にだけ発行する | 述語の押し下げ・派生表の展開・Semi 化・相関サブクエリの外側の列の参照が、`ColId` の付け替え（置換表）なしでできる。「Project ごとに新しい ID を発行」する案は、相関参照（`SubLink` の中の外側の列）が `Project` / `Aggregate` をまたぐたびに置換が要り、00 §6.4 の「子の出力にない `ColId` は外側の列」の約束と相性が悪い。**ID の一意性は「定義の一意性」**（同じ ID を 2 つの異なる値に使わない）であって、「ノードの出力に一度しか現れない」ことではない |
| 04-D2 | **派生表・インライン CTE は `Project` の層として build が作り、ルール R3（派生表の展開）が層を取り除く**。FROM の副問い合わせの出力列は、内側の問い合わせの出力 `ColId` をそのまま使う（新しい ID も `Project` も作らない） | PostgreSQL の `pull_up_subqueries` は問い合わせ構造の併合だが、論理プランでは「式の層（`Project`）を消して、参照を置換する」で同じ効果が得られる。集約を含む派生表も `Project` だけは消せる |
| 04-D3 | **`RIGHT JOIN` は build で左右を入れ替えて `Left` にする。元の列順に戻す `Project` は作らない** | すべての消費者が列を `ColId` で参照し、列の位置を意味に使うのは物理化の「`ColId` → 位置」の表だけ。出力列の順序は最上位の `Project`（SELECT の目的リスト）が決める。00 §8 の `Join` の出力は「left の出力 ++ right の出力」なので、入れ替えた結果は「元の right の出力 ++ 元の left の出力」になるだけ |
| 04-D4 | **CTE のインライン規則（D-16）は `inline(cte) = refs ≥ 1 ∧ ¬recursive ∧ is_select ∧ ¬volatile ∧ ((materialize = Default ∧ refs = 1) ∨ materialize = Never)`**。`refs = 0` の CTE は計画しない。共有する CTE（上の式でインラインしないもの）の計画が外側の列を参照する（相関）ときは次の 3 通り（レビュー対応 R-05。02 §3.6.3 の P8、05 D5-20、11 の M4-Q5・M4-Q46・KD-23 と一致させた）: (a) **`MATERIALIZED` の明示**があれば `0A000`（参照 1 回でも）、(b) **揮発性**を含み共有が必要なら `0A000`、(c) それ以外（`materialize = Default` で参照が複数、非揮発）は**インラインする**（共有しなくても結果は同じで、`CteScan` の再実行を executor に持たせずに済む）。共有する CTE は `LogicalQuery.ctes` に入れ、`LogicalCte.inline` は常に `false`、`refs` は `CteScan` の個数 | PG:`SS_process_ctes`（subselect.c）と同じ条件。M4 では `recursive = false`、`is_select = true`、データ変更 CTE なし。相関 CTE の共有は PostgreSQL にもあるが、`CteScan` の再実行（params の変化）を M4 の executor に持たせない。したがって `PhysicalQuery.ctes[i]` は外側の `Param` を使わない（02 P8、05 D5-20 の前提）。(a) の `MATERIALIZED` は「1 回だけ評価して共有する」意味を持つので、相関するなら共有できず `0A000` にする（インラインへの黙った読み替えはしない。KD-23） |
| 04-D5 | **サブクエリの結合化（R2）の範囲**: `WHERE`（と `HAVING`、DML の WHERE）の最上位 AND と、**内部結合の ON** の最上位 AND にある `EXISTS` / `NOT EXISTS` / `IN`（= `Any`）。左結合の ON 側や `OR` の下は変換しない。`NOT IN`（`Not(SubLink Any)`）は変換しない。**相関のある `IN` は、相関のある `EXISTS` と同じ形のサブクエリに限り変換する**（`x IN (SELECT e FROM ... WHERE w)` を `EXISTS (SELECT 1 FROM ... WHERE w AND x = e)` と同じに扱う） | PostgreSQL 17.11 は相関のある `IN` も Semi 結合にする（検証済み、§2.2）。PostgreSQL は内部で LATERAL を使うが、M4 は LATERAL を持たないので、`EXISTS` の形に直せるものだけを対象にする |
| 04-D6 | **結合キーの抽出（R6）は `Join.on` を「等値キー… ++ 残り…」の正規形に並べ直すこと**。キーを持つ別のデータ構造は作らず、物理化が同じ関数 `split_on` で取り直す。結合順序（R7）は新しく作る `Join` の `on` を同じ関数で正規化する | `LogicalPlan::Join`（00 §8）に鍵の欄がない。R7 の後で木の形が変わるので、物理化は常に取り直す必要がある |
| 04-D7 | **結合順序（R7）は貪欲法**: 内部結合の「島」を平坦化し、構文順の最初の葉から始め、すでに選んだ集合と結合条件でつながる葉を優先（等値でつながるもの > 他の条件でつながるもの > つながらない（直積））、同順位は構文順。外部結合・Semi・Anti は動かさない。統計もサイズも使わない（予測可能さを優先） | `m4-query.md` §7.2。M6 のコストベース化（動的計画法）で置き換える |
| 04-D8 | **サイズの手がかり**（ハッシュ結合のビルド側、内側 Index Scan の採否）は `size::estimate(&LogicalPlan) -> f64`（「ブロック相当の重み」）。`Get` は `nblocks`（0 は 1 とみなす）、述語 1 つにつき ×1/3（等値は ×0.1）、一意インデックスの全列等値は 0.01。定数は §3.6 | `m4-query.md` §7.2。PostgreSQL も `estimate_rel_size` で実ページ数を使う。値の意味は暫定で、**変えても結果は変わらない**（プランの形だけ変わる） |
| 04-D9 | **インデックス選択のスコアは `(一意で全列等値, 等値の列数, 範囲の境界の数)` の辞書順の最大、同点は `IndexDef.oid` の昇順**。選んだインデックスで使った述語も `filter` に残して再評価する | D-18。再評価は PostgreSQL の recheck とは意味が違い、「B+Tree の比較と SQL の演算子の意味が食い違った場合」と「NULL の境界」の安全弁 |
| 04-D10 | **結合方法の選択**（優先順）: (1) 内側 Index Scan の Nested Loop（§7.5 の条件）、(2) Hash Join（ハッシュ可能な等値キーがあるとき）、(3) Nested Loop（内側 Materialize）。`enable_*` は「他に選択肢があれば避ける」。FULL は Hash のみ（キーがなければ `0A000`） | D-13。PostgreSQL の `FULL JOIN is only supported with merge-joinable or hash-joinable join conditions` と同じ SQLSTATE・文言（検証済み） |
| 04-D11 | **SubPlan の分類**: 自由な列がなければ `InitOnce`（`Any` で `test` が等値の連言ならハッシュ化 `Hashed`）、あれば `Rescan`。`SubPlanDef.params` は自由な列の `ColId` 昇順 | D-15。外側の列だけでなく祖父母の列も「自由な列」として params にする（再実行のたびに値が変わりうる） |
| 04-D12 | **`ExplainNode` の木は `PhysicalPlan` と同形とは限らない**。`Hash`（Hash Join のビルド側）と `Append`（`HashSetOp` の入力）は合成ノード、`Filter` は親ノードの詳細行に併合、`Project` は子のノードに透過。計測値との対応づけは `ExplainNode.exec_id`（10 §3.2 の定義が正。名前は `plan_id` ではない。11 §7.1 の C-1） | PostgreSQL のテキスト形式（`Hash Cond:` の下に `Hash` ノード、`HashAggregate` の `Filter:`）に合わせるため |
| 04-D13 | **計画時にエラーにするもの**: 定数畳み込みで起きたエラー（`1/0` → `22012` など。使われない分岐でも、PostgreSQL と同じ範囲で）、FULL JOIN の条件（`0A000`）、ID のオーバーフロー（`54000`）、計画の再帰の深さ（`54001`）。型・名前の誤りはアナライザの責任で、プランナは `Error::internal` | PostgreSQL は `eval_const_expressions` の中でエラーにする（検証済み、§6.1） |
| 04-D14 | **ルールは `RULES` の表の順に 1 回ずつ適用する**（D-17）。各ルールは `fn(&mut LogicalQuery, &RuleCtx) -> Result<()>`。各ルールの後に `PlanTrace::logical` を呼べる | `plan_golden`（§11.2）が各ルールの後のプランを撮れるように |

### 2.1 処理系列

```
plan(stmt, env)
  ├─ build::build_statement      BoundStatement → LogicalQuery            （§5）
  ├─ rules::run                  RULES を順に 1 回ずつ                      （§6）
  └─ physicalize::physicalize    LogicalQuery → PhysicalQuery（+ ExplainNode）（§7、§8）
```

### 2.2 PostgreSQL 17 との差の一覧

**プランの選び方は PostgreSQL と一致しない**（統計もコストもない）。次の差は意図したもので、**結果は一致する**。EXPLAIN のテストは `onlyif yuzhu`（D-20）。

| 項目 | PostgreSQL 17 | yuzhu M4 |
|---|---|---|
| 選択の根拠 | コストと統計 | 規則とヒューリスティクス（`nblocks` と述語の個数だけ） |
| スキャン | Seq / Index / Index Only / Bitmap / Tid | Seq / Index。Index は使えれば常に使う（小さな表でも。検証済み: PG は 3 行の表で Seq Scan、M4Q-8）。`IN` / `OR` / `LIKE` は使わない（検証済み: `a IN (1,2,3)` は PG では `= ANY ('{1,2,3}')` の Filter） |
| 結合方法 | Hash / Merge / Nested Loop（Memoize など） | Hash / Nested Loop / 内側 Index Scan の Nested Loop |
| 結合順序 | 動的計画法（`join_collapse_limit = 8`） | 貪欲法・構文順（§6.7）。外部結合は動かさない |
| 等値の推移 | `t.a = 5 AND t.a = u.a` から `u.a = 5` を導く（EquivalenceClass） | 導かない（任意拡張 §6.5.4 で定数の推移だけ） |
| Semi / Anti | `LEFT JOIN` の ON の中の `EXISTS` も引き上げる。相関のある `IN` は LATERAL。`EXISTS` を `ANY` に直してハッシュ化 SubPlan にする（検証済み: `exists(...) or b = 1` が `hashed SubPlan`）。JOIN_UNIQUE の Unique 化 | 内部結合の ON と WHERE のみ。相関 `IN` は `EXISTS` の形で。`EXISTS` のハッシュ化なし（相関 `EXISTS` が引き上げられなければ `Rescan`）。Unique 化なし |
| `EXISTS` の中の `GROUP BY`（集約なし） | 取り除いて Semi 結合にする（検証済み: `exists (select 1 from u where u.a = t.a group by u.a)` が `Hash Semi Join`。`HAVING` つきは `SubPlan`） | 変換しない（`Rescan` の SubPlan）。結果は同じ |
| 左結合 + `IS NULL` | アンチ結合に認識することがある | しない |
| `HAVING`（集約なし・GROUP BY なし） | `Aggregate` の `Filter` に残したまま WHERE にも写す（検証済み） | 写さない。HAVING に残し、定数の偽は R1 で `Empty` |
| `LIMIT 0` | そのまま `Limit`（検証済み） | 同じ（畳まない） |
| 同一述語の重複 | 取り除く（検証済み: `a = 1 AND a = 1` → `Filter: (a = 1)`） | R1 で取り除く |
| `count(DISTINCT x)` | Sort + Aggregate（検証済み） | 集約ごとのハッシュ集合（05）。プランは `Aggregate` だけ |
| `DISTINCT` | `HashAggregate` か `Unique` + `Sort` | `PhysicalPlan::Distinct`（EXPLAIN は `HashAggregate` + `Group Key:`） |
| 派生表 | 平坦化できないものは `Subquery Scan on s` | ノードを出さない（`Project` の層は透過）。集合演算の枝の `*SELECT* 1` も出さない |
| 同じ表の 2 度目の別名 | `t_1`（検証済み: `Insert on t / Seq Scan on t t_1`） | 出さない |
| InitPlan の位置 | そのクエリレベルの最上位ノードの下 | 同じ（その問い合わせ階層の根の `ExplainNode` の子。主問い合わせ・各 CTE の本体・各 SubPlan の本体がそれぞれ 1 つの階層。§8、レビュー対応 R-06）。SubPlan（`Rescan` / `Hashed`）は式を表示したノードの下 |
| SubPlan の番号 | 内側が先 | 物理化した順（子ノードが先） |
| `Hash` ノード | あり | EXPLAIN にだけ合成する（04-D12） |
| `MIN` / `MAX` | インデックスの先頭を読む最適化 | なし |
| インデックス順によるソートの省略 | コストで判断 | 条件つき（§7.4。M4 後半・任意） |
| `LIMIT` を子に伝える（`tuple_fraction`） | あり | なし（`Limit` の下は全行を処理する。ソートの省略だけ例外） |

---

## 3. 型とトレイト（契約の具体化）

### 3.1 入口（`planner/mod.rs`、L1）

00 §9.1 の `PlanEnv` と `PlannerSettings` はそのまま。`PlanEnv` は参照だけなので `Clone, Copy` を付け、EXPLAIN VERBOSE の指定を足す（00 への変更提案 1）。

```rust
#[derive(Clone, Copy)]
pub struct PlanEnv<'a> {
    pub catalog: &'a dyn CatalogReader,
    pub storage: &'a dyn TableStore,
    pub settings: &'a PlannerSettings,
    pub type_env: &'a TypeEnv<'a>,
    pub want_explain: bool,
    /// ★ 追加: EXPLAIN VERBOSE。`Output:` 行を作るか、表名のスキーマ修飾・列名の修飾を付けるかを決める。want_explain が false なら無視
    pub explain_verbose: bool,
}

pub fn plan(stmt: &BoundStatement, env: &PlanEnv<'_>) -> Result<PhysicalQuery> {
    match stmt {
        BoundStatement::Select(_) | BoundStatement::Insert(_) | BoundStatement::Update(_) | BoundStatement::Delete(_) => {
            let mut lq = build::build_statement(stmt, env)?;
            rules::run(&mut lq, env, &mut NoTrace)?;
            physicalize::physicalize(lq, env)
        }
        // EXPLAIN は内側の文を want_explain = true で計画する（ANALYZE の実行・整形は 10 と session）
        BoundStatement::Explain(e) => plan(&e.inner, &PlanEnv { want_explain: true, ..*env }),
        BoundStatement::Copy(_) | BoundStatement::Ddl(_) | BoundStatement::Checkpoint =>
            Err(Error::internal("utility statements are not planned")),
    }
}

/// plan_golden とデバッグ用: 各段階の論理プランと最後の物理プランを受け取る
pub trait PlanTrace {
    /// stage: "build" と RULES の名前（"const_fold" など）
    fn logical(&mut self, stage: &str, q: &LogicalQuery);
    fn physical(&mut self, q: &PhysicalQuery);
}
pub struct NoTrace;      // 何もしない実装
pub fn plan_traced(stmt: &BoundStatement, env: &PlanEnv<'_>, trace: &mut dyn PlanTrace) -> Result<PhysicalQuery>;
```

- ネストした `Explain`（`EXPLAIN EXPLAIN ...`）はパーサが拒否するので起きない。起きたら `Error::internal`。
- `PhysicalQuery.output` は `LogicalQuery.columns`（SELECT は `BoundQuery.columns`、RETURNING 付き DML は `BoundReturning.columns`、RETURNING なしの DML は空）。

### 3.2 共通部品（`planner/util.rs`、L1。L2 も使う）

```rust
pub type ColSet = std::collections::BTreeSet<ColId>;          // 反復順が決まる（プランを決定的にする）

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Volatility { Immutable, Stable, Volatile }

// ---- 式 ----
/// 最上位の And を平坦化して conjunct の列にする。リテラル true は捨てる（空なら空）。and_all の逆
pub fn conjuncts(e: LExpr) -> Vec<LExpr>;
pub fn and_all(parts: Vec<LExpr>) -> Option<LExpr>;           // 空 → None、1 個 → そのまま、2 個以上 → And
/// 参照する ColId。SubLink の中の「自由な列」（plan_free_cols）を含む。SubLinkOutput は含まない
pub fn expr_refs(e: &LExpr) -> ColSet;
pub fn expr_volatility(e: &LExpr) -> Volatility;               // §3.2.1
pub fn contains_sublink(e: &LExpr) -> bool;
/// 構造の等価（span は無視。Literal は同じ変種で cmp_datum == Equal。SubLink を含むものは常に false）
pub fn expr_eq(a: &LExpr, b: &LExpr) -> bool;
/// Column(c) を map[c] に置き換える（SubLink の中の自由な列も置き換える）
pub fn substitute(e: &LExpr, map: &std::collections::HashMap<ColId, LExpr>) -> LExpr;
/// cols がすべて NULL のとき、e が NULL になる（NULL 伝播）。§6.4.1
pub fn nulls_out(e: &LExpr, cols: &ColSet, catalog: &dyn CatalogReader) -> bool;
/// cols がすべて NULL のとき、e が NULL か false になる（その行が WHERE / ON で落ちる）。§6.4.1
pub fn rejects_null(e: &LExpr, cols: &ColSet, catalog: &dyn CatalogReader) -> bool;
/// e を型 to にそろえる。同じ型ならそのまま、違えば implicit キャスト（catalog.find_cast）を包む。なければ None
pub fn coerce_expr(e: LExpr, to: SqlType, catalog: &dyn CatalogReader) -> Option<LExpr>;

// ---- プラン ----
pub fn plan_output(p: &LogicalPlan) -> Vec<ColId>;             // ノードの出力列（順序つき）。§3.2.2
pub fn plan_free_cols(p: &LogicalPlan) -> ColSet;              // p の中で参照され、p の中のどのノードの出力にも定義されない ColId
/// ノード直下の式・子・サブクエリを取り出す（ルールの走査の部品）
pub fn children_mut(p: &mut LogicalPlan) -> Vec<&mut LogicalPlan>;
pub fn exprs_mut(p: &mut LogicalPlan) -> Vec<&mut LExpr>;
/// 式の中の SubLink の LogicalSubquery（直下だけ。入れ子は呼び出し側が再帰する）
pub fn subqueries_mut(e: &mut LExpr) -> Vec<&mut LogicalSubquery>;
/// 子から先（post-order）に変換する。ノードの式の中の SubLink の plan にも降りる
pub fn transform_up(p: LogicalPlan, f: &mut dyn FnMut(LogicalPlan) -> Result<LogicalPlan>) -> Result<LogicalPlan>;

// ---- 結合条件 ----
pub struct EquiKey { pub left: LExpr, pub right: LExpr, pub op: &'static BuiltinOperator, pub key_type: SqlType }
pub struct SplitOn { pub keys: Vec<EquiKey>, pub residual: Vec<LExpr> }
/// on の conjunct を等値キーと残りに分ける（§6.6）。left / right はそれぞれの子の出力列
pub fn split_on(on: Option<&LExpr>, left: &ColSet, right: &ColSet, catalog: &dyn CatalogReader) -> SplitOn;
```

#### 3.2.1 `expr_volatility`

| 式 | 揮発性 |
|---|---|
| `Function { func }` | `catalog::builtin::proc_by_oid(func.oid).volatility`（`'i'` / `'s'` / `'v'`）。`None`（表にない）は Volatile |
| `Operator { op }` | `builtin::operator_meta(op.oid)` の `proc_oid` を `proc_by_oid` に渡す。どちらかが `None` なら Volatile |
| `Cast { method }` | `CastMethod::Function` なら `builtin::find_cast` の `func_oid` の揮発性。`Binary` は Immutable。`InOut` は Stable（日時・regclass などの入力は DateStyle / TimeZone / カタログに依存するため） |
| `CoerceTypmod` | Immutable |
| `SessionValue` | Stable |
| `SubLink` | 中の plan に Volatile な式があれば Volatile。なければ Stable（サブクエリは行を読むので Immutable ではない） |
| `Literal`、`Column`、`And` など | 子の最大。`Literal` と `Column` は Immutable |

結果は式の部分木の最大。`FnKind::Runtime`（`nextval`、`pg_sleep`）は `PROCS` の `provolatile` が `'v'` であることに頼る（`08`・`09` が行を正しく入れる。入れ忘れると畳み込みが誤るので、テストで「`FnKind::Runtime` の関数は Volatile」を固定する、§11.5）。

#### 3.2.2 `plan_output`（ノードごとの出力列）

| ノード | 出力 |
|---|---|
| `Get` | `cols` ++ `system_columns` の ColId |
| `Values`、`FunctionScan`、`CteScan`、`Result`、`Empty` | `cols` |
| `Filter`、`Sort`、`Limit`、`Distinct` | 子の出力 |
| `Project` | `exprs` の ColId の並び |
| `Join` | Inner / Left / Full: left ++ right。Semi / Anti: left |
| `Aggregate` | `group_by` の ColId ++ `aggs` の ColId |
| `SetOp` | `cols` |
| `Insert` / `Update` / `Delete` | 空（RETURNING は物理側で処理する） |

### 3.3 ルールの枠（`planner/rules/mod.rs`、L1）

```rust
pub struct RuleCtx<'a> { pub env: &'a PlanEnv<'a> }
pub type RuleFn = fn(&mut LogicalQuery, &RuleCtx<'_>) -> Result<()>;

/// 適用順。名前は PlanTrace と plan_golden の見出しに使う
pub static RULES: &[(&str, RuleFn)] = &[
    ("const_fold",      const_fold::apply),
    ("sublink",         sublink::apply),
    ("subquery_pullup", subquery_pullup::apply),
    ("outer_join",      outer_join::apply),
    ("pushdown",        pushdown::apply),
    ("join_keys",       join_keys::apply),
    ("join_order",      join_order::apply),
    ("prune",           prune::apply),
];

pub fn run(q: &mut LogicalQuery, env: &PlanEnv<'_>, trace: &mut dyn PlanTrace) -> Result<()> {
    trace.logical("build", q);
    for (name, f) in RULES { f(q, &RuleCtx { env })?; trace.logical(name, q); }
    Ok(())
}

/// テスト用: 名前で指定したルールだけ適用する（差分テスト §11.4）
pub fn run_only(q: &mut LogicalQuery, env: &PlanEnv<'_>, names: &[&str]) -> Result<()>;
```

- 各 `apply` は `q.plan`、`q.ctes[i].plan`、式の中の入れ子の `LogicalSubquery.plan` のすべてに適用する。入れ子の `LogicalSubquery` の根では「出力として必要な列」が `LogicalSubquery.output`、`q.plan` の根では `q.output` と根のノードが使う列（resjunk）になる。根の扱いが違うルール（R3 と R8）はそこを個別に書く。
- 再帰の深さは `MAX_PLAN_DEPTH`（§3.9）を超えたら `54001`。`transform_up` と各ルールの再帰関数が数える。

### 3.4 build（`planner/build.rs`、L1）

```rust
pub fn build_statement(stmt: &BoundStatement, env: &PlanEnv<'_>) -> Result<LogicalQuery>;

struct Builder<'a> {
    env: &'a PlanEnv<'a>,
    arena: ColumnArena,
    /// SELECT のスコープの積み（Var.levels_up は「積みの上から数えて何段目か」）
    scopes: Vec<SelectScope>,
    /// BoundQuery ごとの CTE の枠（RteKind::CteRef.levels_up は積みの上から数えた段）
    cte_frames: Vec<CteFrame<'a>>,
    /// 共有する CTE（LogicalQuery.ctes になる）
    ctes: Vec<LogicalCte>,
    n_sublinks: usize,
    depth: usize,
}
struct SelectScope {
    /// rte_cols[rte.0][i] = その RTE の i 番目の列を表す式（Column(ColId)。USING の併合列は Coalesce 式）
    rte_cols: Vec<Vec<LExpr>>,
    /// システム列: Get が発行した ColId
    sys_cols: std::collections::HashMap<(RteId, SystemColumn), ColId>,
}
enum CteState { Inline, Shared(Option<CteId> /* 最初の参照で build して Some にする */), Unreferenced }
struct CteBinding<'q> { cte: &'q BoundCte, refs: u32, state: CteState }
/// BoundQuery 1 つにつき 1 枠。scope_depth は枠を積んだときの scopes.len()（CTE の本体を build するとき scopes をここまで切り詰める。§5.7）
struct CteFrame<'q> { scope_depth: usize, bindings: Vec<CteBinding<'q>> }

/// build_query / build_select の結果
struct Built {
    /// 根の plan。出力列は「可視列 ++ resjunk」
    plan: LogicalPlan,
    /// 可視列（plan の出力の先頭）
    output: Vec<ColId>,
    columns: Vec<OutputColumn>,
}

impl Builder<'_> {
    fn build_query(&mut self, q: &BoundQuery) -> Result<Built>;                       // WITH、本体、ORDER BY、LIMIT
    fn build_select(&mut self, s: &BoundSelect, order: &[BoundSortKey]) -> Result<Built>;
    fn build_from_item(&mut self, item: &FromItem, s: &BoundSelect) -> Result<LogicalPlan>;
    fn build_rte_leaf(&mut self, id: RteId, rte: &Rte) -> Result<LogicalPlan>;        // Table / Subquery / Values / Function / CteRef
    fn lower(&mut self, e: &BoundExpr) -> Result<LExpr>;                              // Var → ColId、SubLink → LogicalSubquery
    fn lower_post_agg(&mut self, e: &BoundExpr, agg: &mut AggState) -> Result<LExpr>; // §5.4
    fn new_col(&mut self, name: &str, qualifier: Option<&str>, ty: SqlType, origin: Option<(Oid, i16)>) -> Result<ColId>;  // u32 を超えたら 54000
}
```

### 3.5 物理化（`planner/physicalize.rs`、L2）

```rust
pub fn physicalize(q: LogicalQuery, env: &PlanEnv<'_>) -> Result<PhysicalQuery>;

struct Physicalizer<'a> {
    env: &'a PlanEnv<'a>,
    arena: &'a ColumnArena,
    subplans: Vec<SubPlanDef>,
    ctes: Vec<PhysicalPlan>,
    cte_index: std::collections::HashMap<CteId, usize>,
    next_param: u32,                                   // u16 を超えたら 54000
    size: size::SizeCtx<'a>,                           // nblocks のキャッシュ
    /// want_explain のときだけ（§3.8、§8）
    names: Option<explain_tree::ExplainNames<'a>>,
    /// 木（根・各 CTE・各 SubPlan）ごとのメモ。物理ノードを作るたびに、作った順（後順）に足す。作っている木の添字は tree_stack の先頭
    notes: Vec<Vec<explain_tree::NodeNote>>,
    tree_stack: Vec<usize>,
    /// 今の節点の式の中で出会った SubPlan（式を降ろし終えたらメモの subplans に移す）
    pending_subplans: Vec<SubPlanId>,
    depth: usize,
}

/// 物理化したノードと、その出力列（ColId の並び）。列の位置は layout から引く
struct Phys { plan: PhysicalPlan, layout: Vec<ColId> }          // EXPLAIN 用の文字列は notes に積む（§8）
type ParamMap = std::collections::HashMap<ColId, ParamId>;     // 自由な列 → パラメータ

/// 式の降ろし先の文脈
struct LowerCx<'c> {
    layout: &'c [ColId],                               // 評価する行の列（PhysCol::Local の位置）
    params: &'c ParamMap,                                  // 自由な列 → パラメータ
}
impl Physicalizer<'_> {
    fn phys(&mut self, p: &LogicalPlan, cx_params: &ParamMap) -> Result<Phys>;
    fn lower(&mut self, e: &LExpr, cx: &LowerCx<'_>) -> Result<PhysExpr>;     // Column → Local / Param、SubLink → SubPlanId
    fn plan_sublink(&mut self, kind: SubLinkKind, test: Option<&LExpr>, sub: &LogicalSubquery, cx: &LowerCx<'_>) -> Result<SubPlanId>;
    fn ensure_layout(&mut self, p: Phys, want: &[ColId]) -> Result<Phys>;     // 違えば Project を足す（なければ Error::internal）
}
```

- `lower` は `Expr::try_map`（00 §6.5）で葉だけを書き換える。`Column(c)`: `cx.layout` に `c` があれば `PhysCol::Local(最初の位置)`、なければ `cx.params[c]` の `PhysCol::Param`、どちらもなければ `Error::internal("unbound column")`。`SubLink` は `plan_sublink` で `SubPlanId` にし、式の `SubLink` ノードは `{ kind, test: None, query: id }`（`test` は `SubPlanDef.test` が持つ。§7.7）。
- `ParamId` は問い合わせ全体で 1 つの採番（`PhysicalQuery.n_params` = 発行数）。

### 3.6 サイズの手がかり（`planner/size.rs`、L2）

```rust
pub const ROWS_PER_BLOCK_EST: f64 = 100.0;     // 1 ブロックの行数の見立て（INL の採否にだけ使う）
pub const FILTER_FACTOR: f64 = 1.0 / 3.0;      // 等値以外の述語 1 つの係数
pub const EQ_FACTOR: f64 = 0.1;                // `col = 定数式` 1 つの係数
pub const MIN_FILTER_FACTOR: f64 = 1.0 / 27.0; // 述語が多くても係数の積はこれより小さくしない
pub const POINT_LOOKUP_WEIGHT: f64 = 0.01;     // 一意インデックスの全列等値（1 行）
pub const MIN_WEIGHT: f64 = 0.01;

pub struct SizeCtx<'a> { env: &'a PlanEnv<'a>, nblocks: std::cell::RefCell<std::collections::HashMap<Oid, u32>> }
impl SizeCtx<'_> {
    /// 「ブロック相当の重み」。下表の規則（04-D8）。nblocks の失敗（storage のエラー）は伝える
    pub fn estimate(&self, p: &LogicalPlan) -> Result<f64>;
    pub fn nblocks(&self, rel: &RelHandle) -> Result<u32>;      // キャッシュつき。0 ブロックの表は呼び出し側が max(.., 1) とする
}
```

`estimate` の規則:

| ノード | 重み |
|---|---|
| `Get` | `max(nblocks, 1)` |
| `Filter { input, p }` | `estimate(input)` × （conjunct ごとに、`col = 定数式` なら `EQ_FACTOR`、他は `FILTER_FACTOR`）の積（下限 `MIN_FILTER_FACTOR`）。入力が `Get` で、一意インデックスの全列が等値のとき（`index_select::is_point_lookup`）は `POINT_LOOKUP_WEIGHT` |
| `Project`、`Sort`、`Distinct`、`CteScan`（CTE の計画の重み） | 入力の重み |
| `Limit` | 定数の limit n があれば `min(入力, max(n / ROWS_PER_BLOCK_EST, MIN_WEIGHT))`、なければ入力 |
| `Aggregate` | group_by なし: `MIN_WEIGHT`。あり: `入力 × 0.5` |
| `Join` | Inner: `max(左, 右)`（on なしの直積は `左 × 右`）、Left: 左、Full: `左 + 右`、Semi / Anti: `左 × 0.5` |
| `SetOp` | `Union`: 左 + 右、`Intersect`: `min(左, 右)`、`Except`: 左 |
| `Values` | `max(行数 / ROWS_PER_BLOCK_EST, MIN_WEIGHT)` |
| `FunctionScan` | 引数が定数の `generate_series(a, b)` は `max((b - a + 1) / ROWS_PER_BLOCK_EST, MIN_WEIGHT)`、他は 1 |
| `Result`、`Empty` | `MIN_WEIGHT` |
| DML | 入力の重み |

### 3.7 インデックス選択（`planner/index_select.rs`、L2）

```rust
/// 述語から取り出した、インデックスで使える条件
#[derive(Clone, Debug)]
pub struct IndexQual {
    pub attnum: i16,
    pub kind: QualKind,
    /// 値の式（定数式: このスキャンの列を含まない。パラメータ・InitPlan・外側の列を含んでよい）。IsNull では None
    pub value: Option<LExpr>,
    /// 述語の列（conjuncts の添字）
    pub conj: usize,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QualKind { Eq, IsNull, Lt, Le, Ge, Gt }        // strategy 3 / IS NULL / 1 / 2 / 4 / 5

/// 選んだインデックスと、その使い方
#[derive(Clone, Debug)]
pub struct IndexPick {
    /// `RelHandle.indexes` の添字（TableDef.indexes と同じ OID 昇順）
    pub index: usize,
    pub eq: Vec<IndexQual>,                 // 先頭の列から連続（Eq か IsNull）
    pub lower: Option<IndexQual>,           // eq.len() 番目の列の下限（Gt / Ge）
    pub upper: Option<IndexQual>,           // 同上限（Lt / Le）
    pub unique_full: bool,
    pub direction: ScanDirection,
}
impl IndexPick { pub fn score(&self) -> (u8, usize, usize); }      // (unique_full, eq.len(), 境界の数)

pub struct ScanRequest<'a> {
    pub table: &'a TableDef,
    pub rel: &'a RelHandle,
    /// Get の列（attnum 順）の ColId
    pub cols: &'a [ColId],
    /// 述語（Filter の conjunct。元の順）
    pub conjuncts: &'a [LExpr],
    /// 内側 Index Scan（NestedLoopParam）のとき、外側の列。これらを含む式は「値の式」として使える
    pub outer_cols: &'a ColSet,
    /// ソートの省略を試すとき、要求する順序（§7.4）
    pub want_order: Option<&'a [OrderKey]>,
}
pub struct OrderKey { pub col: ColId, pub descending: bool, pub nulls_first: bool }

pub fn extract_quals(req: &ScanRequest<'_>, catalog: &dyn CatalogReader) -> Vec<(usize /* index */, Vec<IndexQual>)>;
/// settings（enable_*）を適用して、使うなら Some。None は Seq Scan
pub fn choose_scan(req: &ScanRequest<'_>, settings: &PlannerSettings, catalog: &dyn CatalogReader) -> Option<IndexPick>;
/// 一意インデックスの全列が定数の等値か（size::estimate が使う）。NULL を含む IS NULL は数えない
pub fn is_point_lookup(table: &TableDef, conjuncts: &[LExpr], catalog: &dyn CatalogReader) -> bool;
```

### 3.8 EXPLAIN の木（`planner/explain_tree.rs`、L2。10 と共同）

```rust
/// 列名・パラメータ名・SubPlan の表記を引く。deparse（10）のコールバックの実体
pub struct ExplainNames<'a> {
    arena: &'a ColumnArena,
    /// Aggregate・Project が定義した列の表示（`count(*)`、`(a + 1)`）。ColumnInfo.origin が None の列で使う
    display: std::collections::HashMap<ColId, String>,
    /// NestedLoopParam の ParamId → 外側の式の表示（`t.a`）
    params: std::collections::HashMap<ParamId, String>,
    verbose: bool,
    /// 結合・ソート・集約の式に表名を付けるか: 走査の葉が 2 つ以上、または verbose
    prefix_upper: bool,
}
impl ExplainNames<'_> {
    pub fn new(arena: &ColumnArena, lq_leaf_count: usize, verbose: bool) -> ExplainNames<'_>;
    /// 物理式（layout で Local を ColId に戻す）を表示用の文字列にする。scan_qual = true なら走査の述語の規則（verbose のときだけ修飾）
    pub fn expr(&self, e: &PhysExpr, layout: &[ColId], scan_qual: bool, subplans: &[SubPlanDef]) -> String;
    pub fn col(&self, c: ColId, qualified: bool) -> String;
}
```

- `deparse`（10）が提供する関数の形は「列の名前の引き方と SubLink の表記をコールバックで受け取る」ものと仮定する（00 への変更提案 7）。ここでは `ExplainNames` がコールバックの実体で、deparse の出力文字列は 10 が決める。
- `NodeNote`（物理ノード 1 つ分の文字列）と `assemble`（物理プランから `ExplainNode` の木を組み立てる）は §8。

### 3.9 定数

```rust
pub const MAX_PLAN_DEPTH: usize = 500;        // 再帰の深さ（ルール・build・物理化とも）。超えたら 54001 "stack depth limit exceeded"
pub const MAX_JOIN_ISLAND: usize = 64;        // これを超える内部結合の島は並べ替えない（構文順のまま）
```

---

## 4. 処理の流れ

```
session
  analyze(stmt) → BoundStatement
  plan(&bound, &PlanEnv { catalog, storage, settings, type_env, want_explain, explain_verbose })
    1. build_statement
         Builder::new → scopes / cte_frames を積みながら Bound を再帰
         → LogicalQuery { plan, arena, output, columns, ctes, n_subplans_hint }
    2. rules::run                         RULES を上から 1 回ずつ（各ルールは q.plan と q.ctes[*].plan と入れ子のサブクエリに適用）
    3. physicalize
         共有 CTE を先に物理化（ctes[i]）→ 本体 → 根で出力列の並びをそろえる（ensure_layout）
         SubLink は式を降ろすときに plan_sublink で SubPlanDef にする（入れ子の SubLink は内側が先）
         → PhysicalQuery { root, subplans, ctes, n_params, output, explain }
  executor::build(&query.root)
```

- `LogicalQuery.n_subplans_hint` は build が数えた SubLink の個数で、物理化が `subplans` の容量に使うだけ。
- 計画は文ごとに作り直す（M4 にプランのキャッシュはない）。したがって、リテラルのキャストの畳み込み（§6.1）が `DateStyle` / `TimeZone` に依存してよい。M5 の Extended Query でプランをキャッシュするときは、設定が変わったら再計画すること（04-Q10）。
- `plan` が返す `Err` は、いずれも文の実行前（`EXPLAIN` を含む）。部分的な結果は返さない。

### 4.1 前提（他章の約束）

| 前提 | 持ち主 | 食い違ったとき |
|---|---|---|
| `Var.levels_up` は **rtable を持つスコープの入れ子の深さ**で数える（03 の D3-20。11 §7.1 の C-3）。スコープになるのは `BoundSelect`、DML、**`BoundSetExpr::Values` の各行（rtable が空の 1 スコープ。`select (values (t.a)) from t` の `t.a` は `levels_up = 1`）**。`BoundQuery` そのものは数えない（集合演算の腕・CTE 本体は兄弟）。FROM の派生表の `BoundSelect` と `SubLink` の query は 1 段。LATERAL がないので同じ階層の FROM 兄弟は見えない（レビュー対応 R-04） | 03 | `Builder.scopes` の積み方だけ変える |
| `RteKind::CteRef.levels_up` は **CTE を宣言した `BoundQuery` までの入れ子の深さ**（00 §7）。`cte` は宣言した `BoundQuery.ctes` の添字 | 03 | `cte_frames` の引き方だけ変える |
| `BETWEEN` は `a >= x AND a <= y` に分解されて Bound に来る。`IN (リスト)` は `InList`、`x IN (SELECT ...)` は `SubLink::Any` | 03 | 分解されない場合は R1 の前に build が分解する |
| 集約のレベル: `BoundExprKind::Aggregate` は**それを含む `BoundSelect` の集約**（外側のレベルの集約を内側のサブクエリに書く `select (select sum(t.a)) from t` は 03 が `0A000`）。build は `Aggregate` を `BoundSelect.targets` / `having` の中でだけ見る | 03 | 見つけたら `Error::internal` |
| 集約問い合わせの目的リスト・HAVING・ORDER BY が GROUP BY に含まれない列を参照するとき、**関数従属（主キー）で許されたもの以外は 03 が `42803` にしている**。build は残った「GROUP BY の式と一致しない列参照」を従属列とみなして group key に足す（§5.4） | 03 | — |
| `BoundInsert.overriding`（`OVERRIDING SYSTEM / USER VALUE`）と IDENTITY の `428C9` は 03・08 が `column_map` / `defaults` に反映済み。プランナは `overriding` を見ない | 03、08 | — |
| `BoundSetExpr::SetOp` の `ORDER BY` / `LIMIT` は出力列の位置と定数・パラメータだけを参照する（相関する列の参照は 03 が `0A000`） | 03 | — |
| 演算子解決は済んでいる: 比較・等値の `Operator` は引数の型に合った演算子で、必要なキャストは式に入っている（00 §4.3 規約 2） | 03 | — |

---

## 5. build（Bound → 論理プラン、L1）

### 5.1 全体

```
build_statement(stmt)
  Select(q)  → Builder::build_query(q)                         → LogicalQuery { plan, output, columns, ctes, ... }
  Insert(i)  → §5.9.1
  Update(u)  → §5.9.2
  Delete(d)  → §5.9.3
  Explain / Copy / Ddl / Checkpoint → plan() がここへ渡さない（Error::internal）
```

プランの例（印字の書式は §11.1。`#n:名前` が `ColId`）。表は `t(a, b, c)`、`u(a, d, e)`。`select t.c, count(*) from t join u on t.a = u.a where u.d > 1 group by t.c order by 2 desc` の build 直後:

```
Sort [#7 DESC NULLS FIRST]                           ← ORDER BY 2 は可視の 2 番目（#7）を指す
  Project [#3:t.c, #7:count(*)]                      ← パススルーなので ID を引き継ぐ（04-D1）
    Aggregate group=[#3:t.c] aggs=[#7 := count(*)]   ← group key は Column(#3) なので出力の ID も #3
      Filter (#5:u.d > 1)
        Join Inner on (#1:t.a = #4:u.a)
          Get t [#1:t.a #2:t.b #3:t.c]
          Get u [#4:u.a #5:u.d #6:u.e]
```

- **すべての `Builder` の関数は `depth` を 1 つ増やして再帰し、`MAX_PLAN_DEPTH` を超えたら `54001`**。
- 新しい `ColId` は `new_col` だけが発行する（`u32` を超えたら `54000`: `too many columns in query`）。`ColumnInfo.name` は列名、`qualifier` は表の別名（なければ表名）、`origin` はベーステーブルの列だけ `Some((表の OID, attnum))`。

### 5.2 `Var` から `ColId` への写像と、範囲表の葉

`Builder::lower(e: &BoundExpr) -> Result<LExpr>` は `Expr::try_map`（00 §6.5）で葉を書き換える。

| 葉 | 変換 |
|---|---|
| `Column(Var { rte, col, levels_up })` | `scope = scopes[len - 1 - levels_up]`（範囲外は `Error::internal`）。`col < SYSTEM_COL_BASE` なら `scope.rte_cols[rte][col]`、そうでなければ `Column(scope.sys_cols[(rte, SystemColumn)])`。**`levels_up > 0` のとき ColId は外側の問い合わせの列のまま**（自由な列。00 §6.4） |
| `SubLink { kind, test, query }` | §5.8 |
| `Aggregate` | 集約の外（`lower` から直接）で出会ったら `Error::internal`（`lower_post_agg` の中だけ） |
| その他の変種（`Operator` `Function` `Cast` `And` `Case` など） | 構造をそのまま保つ。`ty` と `span` も写す |

範囲表の葉（`build_rte_leaf`）:

| `RteKind` | 論理プラン | `rte_cols[rte]` |
|---|---|---|
| `Table { table }` | `Get { rel: RelHandle::from_table(table), table, alias, cols, system_columns }`。`cols` は attnum 順の各列に `new_col(列名, Some(refname), 型, Some((table.oid, attnum)))`。`alias` は `refname != table.name` のときだけ `Some(refname)`。`system_columns`: その SELECT（と、ネストしたサブクエリの相関参照）が使うシステム列だけを、`Ctid, Xmin, Cmin, Xmax, Cmax, TableOid` の順に `new_col(名前, Some(refname), 型, None)` で作る。使うシステム列は **build の前に Bound を走査して**集める（`collect_system_columns(&BoundSelect) -> BTreeSet<(RteId, SystemColumn)>`: 目的・WHERE・GROUP BY・HAVING・結合の ON・FROM の関数引数・UPDATE の代入式と、サブクエリ式の中の `levels_up` が自分を指す参照）。UPDATE / DELETE の対象表は常に `Ctid` を足す | `Column(cols[i])` |
| `Subquery { query }` | `build_query(query)` の `plan`。**新しい ID も `Project` も作らず**、内側の出力列をそのまま使う。内側に resjunk（ORDER BY 用の余分な列）があれば、可視列だけに絞る `Project`（パススルー）を上に置く | `Column(built.output[i])`。列数が `Rte.columns` と違えば `Error::internal` |
| `Values { rows }` | `Values { rows: 各式を lower, cols }`。`cols` は `new_col("column{i+1}"（`Rte.columns[i].name`）, refname, 型, None)` | `Column(cols[i])` |
| `Function { call }` | `call` は `Function { func, args }` の式。`FunctionScan { func, args: lower(args), alias, cols }`。`cols` は `Rte.columns` の列ごとに 1 つ（`generate_series` は 1 列） | `Column(cols[i])` |
| `CteRef` | §5.7 | 同 |
| `Join` | 葉ではない（§5.3） | §5.3 |

### 5.3 JOIN（`FromItem`）

```rust
fn build_from_item(&mut self, item: &FromItem, s: &BoundSelect) -> Result<LogicalPlan> {
    match item {
        FromItem::Scan(id) => self.build_rte_leaf(*id, &s.rtable[id.0 as usize]),
        FromItem::Join { rte, kind, left, right, on } => {
            let l = self.build_from_item(left, s)?;
            let r = self.build_from_item(right, s)?;
            self.fill_join_columns(*rte, s)?;                       // rte_cols[rte] を埋める（下表）。ON より先
            let on = on.as_ref().map(|e| self.lower(e)).transpose()?;   // ON は左右の RTE の列を指す
            Ok(match kind {
                JoinType::Inner            => Join { kind: Inner, left: l, right: r, on },
                JoinType::Cross            => Join { kind: Inner, left: l, right: r, on: None },
                JoinType::Left             => Join { kind: Left,  left: l, right: r, on },
                JoinType::Right            => Join { kind: Left,  left: r, right: l, on },   // 04-D3
                JoinType::Full             => Join { kind: Full,  left: l, right: r, on },
            })
        }
    }
}
```

- `from` の項目（カンマ区切り）は左から `Join { kind: Inner, left: それまで, right: 次の項目, on: None }` の左深い木にする。FROM なしは `Result { one_time_filter: None, cols: vec![] }`。
- **`fill_join_columns`**: `RteKind::Join { sources }` の `i` 番目の列を、`JoinColSource` から作る式にする。

| `JoinColSource` | 式 |
|---|---|
| `Left(c)` | `coerce(rte_cols[left_rte][c], rte.columns[i].ty)` |
| `Right(c)` | `coerce(rte_cols[right_rte][c], rte.columns[i].ty)` |
| `Coalesce(l, r)` | `Coalesce([coerce(左の列), coerce(右の列)])`（型は `rte.columns[i].ty`） |

  `coerce` は `util::coerce_expr`（型が同じならそのまま、違えば implicit キャスト。なければ `Error::internal`）。USING の併合列はこの式が参照ごとに展開される（`FULL JOIN ... USING (a)` の `a` は `COALESCE(t.a, u.a)`、`t.a` / `u.a` と修飾すれば元の列。検証済み: `select a, t.a, u.a from t full join u using (a)` が `3, NULL, 3` の行を返す）。
- **RIGHT を LEFT にしても元の列順に戻す `Project` は作らない**（04-D3）。最上位の `Project` が目的リストの順に並べる。
- 結合の ON の中の `SubLink`（`ON EXISTS (...)` など）は `Join.on` の式にそのまま入る。R2 が内部結合のものを引き上げる。
- **JOIN の ON と WHERE は build では区別して持つ**（`Join.on` と `Filter`）。内部結合では同じ意味なので、R5 が同じ扱いにする。

### 5.4 集約（`has_agg`）

`BoundSelect.has_agg` が true のとき、WHERE の `Filter` の上に次を作る。

```rust
struct AggState {
    input_cols: ColSet,                         // 集約の入力（FROM + WHERE）の出力列
    group: Vec<(ColId, LExpr)>,                 // group_by（従属列が足されることがある）
    aggs: Vec<(ColId, LAggCall)>,
}
```

1. **group key**: `s.group_by` の各式を `lower` する。結果が `Column(c)` で、`c ∈ input_cols` かつ `c` がまだ使われていなければ、出力の ID は `c`（04-D1）。そうでなければ新しい ID（名前は式の列名か `?column?`）。`expr_eq` で等しい式は 1 つにまとめる（`GROUP BY a, a`）。
2. **目的リスト・HAVING の書き換え `lower_post_agg(e)`**（`try_map` の上から下。ノードごとに次の順で判定する）:

   | ノード | 処理 |
   |---|---|
   | `Aggregate(call)` | `call` の引数と FILTER を **`lower`（集約の入力側）** する。`(func.oid, distinct, args, filter)` が `expr_eq` で等しい既存の集約があればその ID、なければ `new_col(func.name, None, node.ty, None)` で新しい ID を発行して `aggs` に足す。置き換え先は `Column(その ID)`。入れ子の集約は 03 が `42803` にしている |
   | `SubLink` | `lower` する（§5.8）。内側の plan の自由な列 `plan_free_cols` のうち `input_cols` に属するものを調べ、`group` のどの key の ID にも `Column(c)` の key にもなければ、**従属列として `(c, Column(c))` を `group` に足す**。置き換えはせず子へも降りない |
   | 集約も SubLink も含まないノード | `le = lower(node)`。`group` のどれかと `expr_eq` なら `Column(その key の ID)`。**葉の `Column(c)` で `c ∈ input_cols` なのに key にない**ときは従属列として `(c, Column(c))` を `group` に足し、`Column(c)`。`c ∉ input_cols` の列（外側の問い合わせの列）はそのまま |
   | それ以外 | 構造を保って子へ降りる |

   - 「従属列を group key に足す」のは、主キーへの関数従属で 03 が許した列（`select id, v from p group by id`）のため。同じグループの中で値が 1 つなので、結果は変わらない。足した key の ID は元の列の ID（04-D1）で、その後のノードからは普通の列として見える。
   - 目的リストに `Aggregate` も group key もないノード（`select 1, count(*) ...` の `1`）は、そのまま式になる。
3. すべての式を書き換えたあとで `Aggregate { input, group_by: group, aggs }` を作る。`group_by` なしで `aggs` も空の `Aggregate`（`select 1 having false`）も作る（1 行を出す。D-14）。
4. HAVING があれば `Filter { input: Aggregate, predicate: post(having) }`。

### 5.5 SELECT の残り（Project・DISTINCT・ORDER BY）

```
plan = FROM（§5.3）
plan = Filter(plan, lower(WHERE))                         WHERE があれば
plan = 集約（§5.4）                                        has_agg のとき
Project:  exprs = targets（resjunk を含む全部）。ID は 04-D1
    ・各 target の式 e（集約なら post-agg）:  e が Column(c) で、c ∈ plan_output(入力) かつこの Project でまだ使っていなければ id = c、
      そうでなければ new_col(名前, None, e.ty, None)（名前: 可視列は BoundQuery.columns[i].name、resjunk は "?column?"）
    ・output = 先頭 n_visible 個の ID
DISTINCT:
    None   : order があれば Sort(plan, keys)
    All    : Distinct { on: None }（可視列だけの Project の上。resjunk があれば Error::internal）→ order があれば Sort
    On(pos): Sort(plan, keys = order ++ pos[k..]（k = order の先頭から ON の式に一致している個数。03 §3.2.4。未出の ON の式は昇順 NULLS LAST）、order が空なら pos の順に昇順 NULLS LAST)
             → Distinct { on: Some(各 pos の Column(ID)) }（入力は Sort 済み。Sort は 1 回だけ。レビュー対応 R-03）
Sort の keys: BoundSortKey { target, descending, nulls_first } → LSortKey { expr: Column(Project の target 番目の ID), .. }
LIMIT / OFFSET: Limit { limit: lower(limit), offset: lower(offset) }（build_query で本体の上に。Select / Values / SetOp 共通）
```

- `LogicalQuery.output` は可視列の ID。根の `plan` の出力は「可視列 ++ resjunk」（00 §8）。resjunk を落とす `Project` は論理プランに置かず、物理化の根（`finish_root`、§7.1）が置く。ただし FROM の派生表と集合演算の枝では build が可視列だけの `Project` を置く（§5.2、§5.6）。
- `ORDER BY` を指す target が resjunk（ORDER BY の式が目的リストにないとき）でも、Sort は `Project` の出力 ID を参照するので追加の処理は要らない。
- `DISTINCT ON` で ORDER BY がないとき PostgreSQL も ON の式で整列する（検証済み: `select distinct on (b) b, a from t` が `Unique` + 整列）。ON の式が一致しない `ORDER BY` は 03 が `42P10` にしている。**ORDER BY が ON の式の一部だけのとき**（`distinct on (a, b) ... order by a`）は、03 が `BoundDistinct::On` の `positions` に未出の ON の式（`b`）を足してくる（03 §3.2.4。PG の `distinct_pathkeys` が `sort_pathkeys` の先頭に一致する）ので、Sort の keys は `order ++ positions[k..]`（`a, b`）の 1 つ。Sort の先頭 `positions.len()` 個の key の集合は ON の式の集合と一致する（03 が `42P10` で保証する）ので、Unique が比べる隣接する行は ON の値が等しいものがすべて連続する。`order` に ON にない項目があるとき（`order by a, c` + `on (a)`）は `k = positions.len()` で、足す式はない。

### 5.6 VALUES と集合演算

- `BoundSetExpr::Values { rows, types }`: `Values { rows: lower, cols: new_col("column{i+1}", None, types[i], None) }`、`output = cols`。**各行は `rtable` が空の 1 スコープとして `scopes` に積んで `lower` する**（03 §3.2.1・D3-20。`select (values (t.a)) from t` の `t.a` は `levels_up = 1` で、積まないと `scopes[len - 1 - levels_up]` が 1 段外にずれて誤った列を引くか範囲外になる。レビュー対応 R-04）。積むスコープは `rte_cols` が空で、`Var` はこのスコープを `levels_up = 0` では引かない（rtable が空なので出てこない）。ORDER BY / LIMIT があれば上に Sort / Limit（Sort の keys は `Column(cols[target])`）。
- `BoundSetExpr::SetOp { op, all, left, right, left_coerce, right_coerce, types }`:
  1. `l = build_query(left)`、`r = build_query(right)`。それぞれ resjunk があれば可視列だけの `Project` を上に置く。
  2. `left_coerce` が `Some(exprs)` のとき、`Var { rte: RteId(0), col: i }` を `Column(l.output[i])` に読み替える一時の `SelectScope`（`rte_cols[0] = l.output の Column`）を積んで `exprs` を `lower` し、`Project { plan: l.plan, exprs: [(new_col(名前, None, 型, None), 式)] }` を上に置く。`left_cols` はその新しい ID。`None` のとき `left_cols = l.output`。右も同じ。
  3. `SetOp { op, all, left, right, cols, left_cols, right_cols }`。`cols` は新しい ID（型は `types[i]`、名前は `left` の可視列の名前）。`output = cols`。
  4. 全体の ORDER BY / LIMIT は `cols` の位置を指す。
- `SetOpKind`（論理）は 00 §8 のとおり `Union` / `Intersect` / `Except`（`all` は別の欄）。

### 5.7 CTE

`build_query` は **BoundQuery ごとに `cte_frames` に 1 枠を積む**（CTE がなくても。`levels_up` の数え方を合わせるため）。枠には宣言したときの `scopes.len()`（`scope_depth`）と、`CteBinding` の並び（`BoundQuery.ctes` と同じ順）を持つ。

1. **参照の数え方**: `refs[i] = count_cte_refs(q, i)`。`q` の本体（`BoundQuery.body`）の中の `RteKind::CteRef { levels_up: 0, cte: i }` と、`q.ctes[j].query` の中の `levels_up: 1`、さらに入れ子の `BoundQuery`（派生表・サブクエリ式・集合演算の枝・CTE 本体）に入るたびに 1 を足した `levels_up` の参照を、`BoundQuery` と式を全部走査して数える（`bound_walk`）。
2. **インラインの判定**（04-D4）:

   ```rust
   fn decide(cte: &BoundCte, refs: u32, vol: Volatility, correlated: bool) -> Result<CteState> {
       let inline = refs >= 1
           && vol != Volatility::Volatile                 // 本体の式すべてで（演算子・関数・キャストの揮発性。§3.2.1）。D-16 の「副作用なし」
           && ((cte.materialize == CteMaterialize::Default && refs == 1) || cte.materialize == CteMaterialize::Never);
       Ok(match (refs, inline) {
           (0, _) => CteState::Unreferenced,
           (_, true) => CteState::Inline,
           // 相関する共有 CTE（04-D4 の (a)〜(c)）。MATERIALIZED の明示と揮発性は 0A000、それ以外は共有せずインライン
           _ if correlated && (cte.materialize == CteMaterialize::Always || vol == Volatility::Volatile) =>
               return Err(Error::not_supported(format!(                          // 0A000（§10）
                   "WITH query \"{}\" that references an outer query level is not supported", cte.name))),
           _ if correlated => CteState::Inline,                                  // Default で参照が複数、非揮発
           _ => CteState::Shared(None),
       })
   }
   ```

   `vol` は `BoundCte.query` の全式を走査して最大の揮発性（`Volatile` ならインラインしない。`random()` を 1 回だけ評価する意味を守る）。`CteMaterialize::Always` は `MATERIALIZED` の明示（00 §7 の `BoundCte.materialize`）。`correlated` は本体に `levels_up` がその CTE の宣言より外を指す `Var` があるか。`recursive`・`is_select`・データ変更 CTE は M4 では常に false / true / なし。
3. **参照の変換**（`RteKind::CteRef { levels_up, cte }` → 論理プラン）: `binding = cte_frames[len - 1 - levels_up][cte]`。
   - `Inline`: `binding.cte.query` を**宣言した位置の文脈で**その場で `build_query` する（`cte_frames` を宣言した枠まで、`scopes` を `scope_depth` まで一時的に切り詰め、終わったら戻す。CTE の本体は参照位置の外側のスコープを見ない）。参照ごとに別々に build するので、2 回参照すれば別々の ID の別々のプランになる。`col_aliases` は名前だけに影響する。`rte_cols` は `Column(built.output[i])`。
   - `Shared`: 最初の参照のときだけ宣言の文脈で `build_query` し、`LogicalCte { name, plan, output, refs, materialize, inline: false }` を `self.ctes` に足す（CteId = 足した位置）。`CteScan { cte, alias, cols }`（`cols` は参照ごとに新しい ID で、型は CTE の出力の型）。
   - `Unreferenced`: 来ない。
4. 共有 CTE の計画は、CTE の本体が参照している別の共有 CTE が先に `self.ctes` に入る（再帰で先に完了するため）。executor は最初の `CteScan` で遅延評価する（00 §9.2）ので順序は意味を持たない。

### 5.8 サブクエリ式（`SubLink`）

`lower` が `SubLink { kind, test, query }` に出会ったとき:

1. `built = build_query(query)`。**現在の `scopes` を積んだまま**呼ぶ（入れ子のスコープから外側の列を `levels_up` で引く）。
2. `kind = Scalar` なら `built.output.len() == 1`、`Any` / `All` なら `test` の `SubLinkOutput(i)` の `i < built.output.len()` を確かめる（違えば `Error::internal`。03 が `42601` にしている）。
3. `test` は `lower` する（`SubLinkOutput(i)` はそのまま残す。00 §6.4）。
4. `SubLink { kind, test, query: Box::new(LogicalSubquery { plan: built.plan, output: built.output }) }`。`n_sublinks += 1`。

### 5.9 DML

#### 5.9.1 INSERT

```
src = build_query(i.source)                                   // 可視列に絞る
input_cols = src.output                                       // coercions があれば
  Some(exprs): 一時スコープ（RteId(0) の列 → src.output）で exprs を lower し、Project { src, [(new_col, e)] } の ID
Insert {
  table: i.table, rel: RelHandle::from_table(&i.table), input: src.plan（Project 込み）, input_cols,
  column_map: i.column_map, defaults: i.defaults を lower_single_rel（Var を含まない式。nextval などは含む）,
  checks: i.checks を lower_single_rel（PhysCheck { name, expr }）、
  not_null: i.table.columns の not_null, returning: i.returning を lower_single_rel（Some のとき）
}
```

`lower_single_rel` は 00 §6.5。`Insert` の `defaults` / `checks` / `returning` が `PhysExpr` で論理プランに現れる唯一の箇所（00 §8）。`columns` は RETURNING があればその列、なければ空。

#### 5.9.2 UPDATE

`rtable[0]` が対象表。

```
target = build_rte_leaf(RteId(0))                              // Get（Ctid を必ず足す）
plan   = target ⋈ from の項目（§5.3 の左深い木。on なし）    // UPDATE ... FROM
plan   = Filter(plan, lower(filter))
Project { plan, exprs: [
     (old_col_i, Column(old_col_i)) …（対象表の全ユーザー列。パススルー）,
     (ctid, Column(ctid)),
     (new_k, 代入式) …（assignments の順）
] }
Update { table, rel, input: Project, old_cols, ctid, new_values: [(attnum - 1, new_k)], checks, not_null, returning }
```

- 代入式: `UpdateSource::Expr(e)` は `lower(e)`（`rtable` 全体の `Var` を参照してよい）。`UpdateSource::Default(Some(e))` は `lower(e)`、`Default(None)` は `Literal(NULL)`（列の型）。`new_k` は `new_col(列名, None, 列の型, None)`。
- `checks` / `returning` は対象表の**新しい行**に対する `lower_single_rel`（`rte = 0` の `Var` → `PhysCol::Local(列番号)`）。
- `UPDATE ... FROM` で 1 つの対象行に複数の行が結合すると、同じ行の 2 回目の更新が来る。**`SelfModified` として飛ばすのは executor の責任**（m2-dml-exec §5、05）。プランナは重複を除かない。
- 対象表は `Get` の「ユーザー列 ++ ctid」を必ず出す。R8（刈り込み）は `Update` の `old_cols` / `ctid` / `new_values` を常に必要な列として扱う（§6.8）。

#### 5.9.3 DELETE

`UPDATE` と同じ。`Project { plan, [old_cols のパススルー, (ctid, Column(ctid))] }` → `Delete { table, rel, input, old_cols, ctid, returning }`。`DELETE ... USING` の項目は `from`。

### 5.10 まとめ: build の不変条件（テストで検査する。§11.5）

1. 根の `plan` の出力に `output` が先頭から含まれる。
2. どの `Project` の出力にも同じ `ColId` が 2 回現れない。どの `Join` の左右の出力も互いに素。
3. 式の中の `Column(c)` は、そのノードの子の出力にあるか、外側の列（そのプランのどのノードの出力にも定義されない）。自由な列は `SubLink` の中にだけある。
4. `Aggregate` の式（`LExpr::Aggregate`）は最終形にない。
5. `Join.kind` は Inner / Left / Full（Semi / Anti は R2 の後）。

---

## 6. ルール（`planner/rules/*`、L1）

固定順に 1 回ずつ（04-D14）。各ルールは `q.plan`、`q.ctes[i].plan`、式の中の入れ子の `LogicalSubquery.plan` のすべてに適用する。書き方は 1 ルール 1 表（目的・前提条件・変換・正しさの条件・PostgreSQL の対応関数・例）と、必要なものは手順。

共通の注意（04-D1 の帰結）: **`ColId` はパススルーで引き継がれるので、「ある ID を式で置き換える」操作（`substitute`）は、その ID を定義したノードの上の、再定義されるまでの範囲にだけ適用する**。R3 がこの範囲を持ち回る（§6.3）。R5 の置換は押し下げる 1 つの述語に対してだけ行うので局所的。

### 6.1 R1 定数畳み込み（`const_fold.rs`）

| 項目 | 内容 |
|---|---|
| 目的 | 定数の式を計画時に評価し、恒真・恒偽の述語を取り除き、`WHERE false` の部分を `Empty` にする |
| 適用先 | すべてのノードの式（`Filter.predicate`、`Project.exprs`、`Join.on`、`Sort.keys`、`Limit` の式、`Values.rows`、`FunctionScan.args`、`Aggregate` の group_by・集約の引数・FILTER、`Result.one_time_filter`）と、式の中の `SubLink` の `test` と plan。`Insert` / `Update` / `Delete` の `PhysExpr` の欄（defaults・checks・returning）は対象外（実行時に評価する） |
| 前提条件 | なし（build の直後） |
| 変換 | 下の「畳む式の範囲」と「プランの簡約」 |
| 正しさの条件 | 畳むのは**列・`SubLink`・`SessionValue` を含まず、Immutable な関数・演算子だけ**の部分木、または**リテラルのキャスト**。評価中のエラー（`22012` など）は文のエラーにする（PostgreSQL と同じ。使われない分岐でも、下の `And` / `Or` / `Case` / `Coalesce` の規則に従った範囲では計画時にエラー） |
| PG の対応関数 | `eval_const_expressions`（PG:src/backend/optimizer/util/clauses.c）、`simplify_function`、`evaluate_expr`、`negate_clause`（prepqual.c） |
| 例 | `where a = 1 + 2` → `a = 3`。`where false and a = 1/0` → `Empty`。`select 1/0 where false` → `22012` |

#### 6.1.1 畳む式の範囲

評価は `executor::eval::eval_const(&PhysExpr, &Row::new(), &EvalCtx::for_constant_folding(catalog, type_env))`（05。00 への変更提案 3）。畳める部分木は列を含まないので、`PhysExpr` への変換（`Column` が現れたら畳まない）はその場で行う。`EvalCtx` の `session` / `runtime` は Pure の関数しか呼ばないので参照されない。

| 式 | 規則 |
|---|---|
| `Operator` / `Function` | 子を先に畳む。すべての引数がリテラルで、`expr_volatility` が Immutable、関数なら `FnKind::Pure` のとき評価して `Literal(結果, e.ty)`。strict で引数に NULL があれば呼ばずに `Literal(NULL)`。Stable・Volatile・`FnKind::Context` / `Runtime` は畳まない（`now()`・`nextval()`・`current_user`）。部分的に畳んだ式は残す |
| `Cast` / `CoerceTypmod` | 子を畳み、**子がリテラルなら**キャストの揮発性に関わらず評価する（`'2020-01-01'::date`、`'t'::regclass`。DateStyle・TimeZone・カタログは文の間は変わらない）。変換エラーは `22P02` などで返す |
| `And(args)` | **前から順に**各引数を畳み、リテラル true は捨て、**リテラル false が出たらそこで結果を `Literal(false)` にして残りの引数を処理しない**（検証済み: `select 1 where false and 1/0 = 1` はエラーなし）。リテラル NULL は残す。入れ子の `And` は平坦化。同じ（`expr_eq`）非 Volatile の引数は 1 つにする。0 個 → true、1 個 → その引数 |
| `Or(args)` | 同様（true で打ち切る。検証済み: `select 1 where true or 1/0 = 1` はエラーなし、`a = 1 or 1/0 = 1` は `22012`） |
| `Not(x)` | 子を畳み、リテラルなら計算。`Not(Not(y))` → `y`。`Not(Operator{op, [a,b]})` で `op` が strict かつ `operator_meta(op.oid).negate != 0` なら negator の演算子に（`not (a > 1)` → `a <= 1`。検証済み） |
| `IsNull` / `IsNotNull` / `BoolTest` | 子がリテラルなら計算 |
| `Case { arms, else }` | 前から順に: 条件を畳む。リテラル false / NULL の腕は**結果も畳まずに**捨てる。リテラル true の腕が来たら、その結果を畳んで `else` に置き換えて**残りの腕と else を処理しない**（検証済み: `case when true then 1 else 1/0 end` は 1。`case when a > 0 then 1 else 1/0 end` は `22012`）。腕が 0 個になれば `else`（なければ `Literal(NULL)`） |
| `Coalesce(args)` | 前から順に: リテラル NULL は捨て、リテラルの非 NULL が来たら（それが先頭ならその値を結果に、先頭でなければ最後の引数として残し）**以降を処理しない**（検証済み: `coalesce(1, 1/0)` は 1）。1 個になればその引数、0 個なら `Literal(NULL)` |
| `NullIf` / `Like` / `InList` | 子を畳み、**全部がリテラル**なら評価 |
| `SubLink` | `test` と plan の中を畳む。plan が `Empty`（R1 の簡約で）になったら: `Exists` → false、`Any` → false、`All` → true、`Scalar` → NULL（集約を含む plan は `Empty` にならない） |
| その他 | 子を畳む |

エラーには、評価した式の `span` が空でなければ位置を付ける（`Error::with_position`）。

#### 6.1.2 プランの簡約

| ノード | 条件 | 結果 |
|---|---|---|
| `Filter` | 述語がリテラル true | 入力 |
| `Filter` | 述語がリテラル false / NULL | `Empty { cols: plan_output(入力) }` |
| `Filter { input: Result { one_time_filter: None }, p }` | （FROM なしの `WHERE`） | `Result { one_time_filter: Some(p), cols }` |
| `Join { kind: Inner, on }` | `on` がリテラル true | `on = None` |
| `Join { kind: Inner / Semi }` | `on` が false / NULL、または**どちらかの子が `Empty`** | `Empty { cols: plan_output(Join) }` |
| `Join { kind: Left }` | 左が `Empty` | `Empty` |
| `Join { kind: Anti }` | 左が `Empty` | `Empty` |
| `Join { kind: Anti }` | 右が `Empty` | 左（`on` を捨てる） |
| `Join { kind: Inner, left, right: Result { one_time_filter: None, cols: [] } }`（と左右逆） | FROM なしの派生表との直積 | もう片方 |
| `Project` / `Sort` / `Distinct` / `Limit` | 入力が `Empty` | `Empty`（`Project` の cols は `exprs` の ID） |
| `Aggregate` | group_by が空でなく、入力が `Empty` | `Empty`。**group_by が空の `Aggregate` は `Empty` にしない**（0 行でも 1 行出す。D-14） |
| `Result` | `one_time_filter` が false / NULL | `Empty { cols }` |
| `Limit` | limit が 0 | **畳まない**（検証済み: PostgreSQL も `Limit` のまま） |

`Empty` は物理化で `PhysicalPlan::Result { exprs: [], one_time_filter: Some(false) }` になる（PostgreSQL の `Result` + `One-Time Filter: false`。検証済み）。

### 6.2 R2 サブクエリの結合化（`sublink.rs`）

| 項目 | 内容 |
|---|---|
| 目的 | `WHERE` の `EXISTS` / `IN` を Semi 結合に、`NOT EXISTS` を Anti 結合にして、行ごとの SubPlan をなくす |
| 適用先 | `Filter.predicate` の最上位 AND の項。`Join { kind: Inner }.on` の最上位 AND の項（変換できる形のものを `Join` の上の `Filter` に移してから同じ規則を適用する。内部結合の ON と WHERE は同じ意味）。`HAVING`・DML の WHERE も `Filter` なので対象 |
| 前提条件 | 項が次のいずれか: `SubLink { kind: Exists, .. }`、`Not(SubLink { kind: Exists })`、`SubLink { kind: Any, test: Some(_) }`。`NOT IN`（`Not(SubLink Any)`）、`ALL`、`Scalar`、`OR` の下、左結合の ON は対象外 |
| 変換 | `Filter { input, p ∧ sub }` → `Filter { Join { Semi / Anti, left: input, right: サブクエリ側, on }, p }`。サブクエリ側と `on` の作り方は下の手順 |
| 正しさの条件 | Semi / Anti は左の行を最大 1 回だけ出す（重複なし）。`Exists` の存在は、目的リスト・ORDER BY・DISTINCT・（定数 ≥ 1 の）LIMIT に影響されない。`Any` は「test が true の行が 1 つでもある」。いずれも `WHERE` で使われる限り NULL と false を区別しなくてよい |
| PG の対応関数 | `pull_up_sublinks`（PG:src/backend/optimizer/prep/prepjointree.c）、`convert_EXISTS_sublink_to_join`、`convert_ANY_sublink_to_join`、`simplify_EXISTS_query`（subselect.c） |
| 例 | `select * from t where exists (select 1 from u where u.a = t.a)` → `Join Semi on (u.a = t.a)`。`... where a in (select a from u where u.d = t.b)` → `Join Semi on ((t.b = u.d) AND (t.a = u.a))`（PostgreSQL 17.11 と同じ形。検証済み） |

#### 6.2.1 手順

`Filter { input, predicate }` に対して（`input` の出力を `avail` とする）、`predicate` の各 conjunct `c` を前から順に `try_pull(c, sides)` する。`sides` は変換した Semi / Anti を積み重ねてよい側の並びで、`Filter` の直下では `[Side { plan: input, avail }]` の 1 つ。変換できなかった項は `Filter` に残す。

```rust
struct Side<'p> { plan: &'p mut LogicalPlan, avail: ColSet }

enum Flat {
    /// 本体を右側にし、ON を作る
    Exists { right: LogicalPlan, conds: Vec<LExpr> },
    /// 内側が相関なしの IN: サブクエリ全体を不透明な右側にする
    OpaqueAny { right: LogicalPlan, test: LExpr },
}

fn try_pull(c: LExpr, sides: &mut [Side<'_>], ctx: &RuleCtx<'_>) -> Result<Result<(), LExpr>> /* Err(c) = 変換せず返す */;
```

1. **形の判定**: `c` が `SubLink { Exists }` → `(Semi, 存在検査)`、`Not(SubLink { Exists })` → `(Anti, 存在検査)`、`SubLink { Any, test, query }` → `(Semi, 比較つき)`。それ以外は変換せず返す。
2. **どの側に積むか**: `R = expr_refs(c) ∩ (すべての side.avail の和)`。`R ≠ ∅` で、`R ⊆ side.avail` となる最初の側を選ぶ。なければ（両方の側を参照、または avail を参照しない）変換せず返す。**Anti 結合の右側（ON の中の入れ子の変換。§6.2.2）は左の側を選べない**（左の行を絞る操作は Anti では意味が変わる）。
3. **サブクエリの形の解析（読み取りだけ）** `peel(plan) -> Option<Peeled>`:
   - `Limit` を**最初に**、続けて `Sort` と `Distinct { on: None }` を（どの順でも）取り除く。`Limit` が許されるのは `Exists` で、`offset = None` かつ `limit` が `Literal(Int8 n)`（`n ≥ 1`）のとき。それ以外の `Limit`、`Distinct { on: Some }` は失敗。
   - 次の `Project { exprs }` を取り除いて `exprs` を覚える（なければ恒等）。
   - 次の `Filter { predicate }` があれば取り除き、`conjuncts(predicate)` を `where_` とする。
   - 残りを `from_tree` とする。**`from_tree` の根が `Aggregate` / `SetOp` / `Distinct` / `Limit` / `Sort` / `Result` / `Empty` なら失敗**（本体そのものが集約・集合演算・FROM なし）。`from_tree` が走査の葉（`Get` / `Values` / `FunctionScan` / `CteScan`）を 1 つも含まなければ失敗。
   - 失敗しなければ `Peeled { from_tree, where_, proj }`。
4. **条件**:
   - `Exists`: `peel` が成功し、`plan_free_cols(from_tree) ∩ avail = ∅`（相関する参照は WHERE の項の中だけ）、`where_` の項に Volatile がなく、`expr_refs(where_ の項) ∩ avail ≠ ∅`（相関がなければ変換しない: InitPlan のほうがよい。PostgreSQL も同じ。検証済み: `exists (select 1 from u where u.d = 3)` は `Result` + `One-Time Filter: (InitPlan 1).col1`）。
   - `Any` で相関なし（`plan_free_cols(query.plan) ∩ avail = ∅`）: `test` に Volatile がなく、`expr_refs(test) ∩ avail ≠ ∅` なら **`OpaqueAny`**（サブクエリ全体が右側。集約・LIMIT・集合演算があってもよい。検証済み: `a in (select a from u limit 5)` は `Hash Semi Join` + `Limit`）。
   - `Any` で相関あり: `Limit` なしで `peel` が成功し、`from_tree` に自由な列がなく、`where_` と、使う `proj` の式（`SubLinkOutput(i)` が参照する `output[i]` に対応する式）に Volatile がなく、`proj` の式に `SubLink` がないこと（`EXISTS` 形に直す）。`expr_refs(test') ∩ avail ≠ ∅`（`test'` は下）。
5. **変換**（条件が満たされたとき）:
   - `Exists`: `right = from_tree`、`conds = where_`。
   - `OpaqueAny`: `right = query.plan`、`test' = substitute(test, SubLinkOutput(i) → Column(query.output[i]))`、`on = test'`。
   - 相関あり `Any`: `test' = substitute(test, SubLinkOutput(i) → proj の output[i] に対応する式)`、`right = from_tree`、`on = test' ∧ where_`。
   - `*side.plan = Join { kind, left: take(*side.plan), right, on: and_all(conds) }`。`Anti` で `NOT EXISTS` のとき `kind = Anti`。
6. **ON の中の入れ子の変換**（§6.2.2）。

#### 6.2.2 入れ子

PostgreSQL は引き上げた ON の中の `SubLink` もさらに引き上げる（`pull_up_sublinks_qual_recurse`）。yuzhu も、作った `Join { Semi / Anti, left, right, on }` の `on` の項に `try_pull` を再帰的に適用する。`sides = [Side { left, avail: plan_output(left) }, Side { right, avail: plan_output(right) }]`（Anti は右だけ）。変換できなかった項は `on` に残す（左右の両方を参照する入れ子の `SubLink` は PostgreSQL と同じく残り、`Rescan` の SubPlan になる）。

この再帰は**変換を外側から先に**行い、内側のサブクエリはそのあとで（変換後の木の一部として）再帰して処理する。

#### 6.2.3 変換しないもの（PostgreSQL との差。§2.2）

- `LEFT JOIN` の ON の中の `EXISTS`（PostgreSQL は右側に積む）。`OR` の下、`NOT IN`、`ALL`、スカラーサブクエリ。
- 相関のない `EXISTS` は InitPlan のまま。`EXISTS` のハッシュ化 SubPlan への変換（`convert_EXISTS_to_ANY`）はしない。
- `Exists` の `from_tree` が外側の列を参照する（ON や FROM の関数引数の中）もの。

### 6.3 R3 派生表の展開（`subquery_pullup.rs`）

| 項目 | 内容 |
|---|---|
| 目的 | 派生表・インライン CTE の `Project` の層を取り除き、中の `Filter` / `Join` / 走査を親の木の一部にする（述語の押し下げと結合順序が派生表をまたげるようにする） |
| 適用先 | `Project` ノードで、親が `Join`（任意の種類・側）・`Filter`・`Aggregate`・`Project` のもの。**根の `Project`**（問い合わせ・サブクエリ・CTE の本体の最上位）と、親が `Sort` / `Limit` / `Distinct` / `SetOp` / `Insert` / `Update` / `Delete` の `Project` は対象外 |
| 前提条件 | 取り除く `Project` P の式のどれにも Volatile がなく、`SubLink` がない。P が外部結合の NULL 側（祖先に `Left` の右の子、または `Full` の子がある。同じ plan の根まで見る）にあるときは、式の各々が `Column(_)`、または `nulls_out(式, P の入力の出力列)`（入力がすべて NULL なら NULL になる式。`a + 1` など。定数・`coalesce`・`case` は不可）|
| 変換 | P を P の入力で置き換える。P の出力 ID `(id, e)` を置換表に入れ、**P より上の、`id` が再定義されるまでの式**の `Column(id)` を `e` で置き換える |
| 正しさの条件 | Volatile でなければ、式を参照の数だけ複製しても結果は同じ（評価回数が変わるだけ）。NULL 側の定数は NULL 拡張されないので、上の条件で除く。`SubLink` を複製すると SubPlan が増えるので除く |
| PG の対応関数 | `pull_up_subqueries`、`is_simple_subquery`、`pull_up_simple_subquery`（prepjointree.c）。PostgreSQL は集約・ソート・LIMIT・DISTINCT・集合演算を含む派生表を平坦化しないが、yuzhu は `Project` の層だけを消すので、**集約を含む派生表の `Project` も消える**（`Aggregate` は残り、結合順序の葉になる） |
| 例 | `select s.a from (select a, b + 1 as x from t where b > 1) s where s.x = 3` → `Project[a] ( Filter (b + 1 = 3) ( Filter (b > 1) ( Get t ) ) )`（このあと R5 が `Filter` を 1 つにして `Get` の述語にする） |

**手順**（置換の範囲を局所にするため、下から上へ置換表を返す）:

```rust
type Subst = HashMap<ColId, LExpr>;
/// 戻り値の Subst は「この部分木の出力に見える ID のうち、取り除いた Project の式で置き換えるべきもの」
fn pull(plan: LogicalPlan, parent: Parent, nullable: bool, ctx: &RuleCtx<'_>) -> Result<(LogicalPlan, Subst)>;
enum Parent { None, Join, Filter, Aggregate, Project, Other }
```

1. 子に再帰する。子の `parent` はこのノードの種類、`nullable`: `Join { Left }` の右の子と `Join { Full }` の両方の子は `true`、それ以外は継承。子から返った置換表を合わせる（ID は互いに素）。
2. **このノードの式**（`Filter.predicate`、`Join.on`、`Project.exprs`、`Aggregate` の式、`Sort.keys` など。式の中の入れ子のサブクエリの自由な列を含む）に、合わせた置換表を `substitute` で適用する。
3. このノードが取り除ける `Project` なら、**子を返し**、戻りの置換表 = 合わせた表 ∪ このノードの `(id, 式)`（式はステップ 2 で置換済み）。
4. そうでなければ戻りの置換表:
   - `Filter` / `Sort` / `Limit` / `Distinct`: 合わせた表（出力がそのまま上へ通る）。
   - `Join`（Inner / Left / Full）: 合わせた表。`Join`（Semi / Anti）: 左の子の表だけ。
   - `Project`（取り除かないもの）/ `Aggregate` / `SetOp` / `Get` などの葉 / DML: **空**。この境界の上に見える ID は、この境界自身が定義したもの（パススルーで再定義した ID を含む。ステップ 2 で式は置換済み）だけだから。
5. `apply` は各根（`q.plan`、CTE、入れ子のサブクエリ）に `pull(根, Parent::None, false)` を呼ぶ（戻りの置換表は捨てる）。

### 6.4 R4 外部結合の内部結合化（`outer_join.rs`）

| 項目 | 内容 |
|---|---|
| 目的 | 上の述語が NULL 拡張された行を必ず落とすなら、外部結合を内部結合に（`Full` は `Left` か `Inner` に）して、述語の押し下げと結合順序の自由度を増やす |
| 適用先 | `Join { kind: Left / Full }` |
| 前提条件 | その結合の出力に対する「上の述語」`quals` のうち、`rejects_null(q, NULL 側の出力列)` を満たすものがある（§6.4.1） |
| 変換 | `Left` → `Inner`（右が NULL 側）。`Full`: 左の列を拒否（`rl`。右だけの行が落ちる）→ 左が保存側の `Left`、右の列を拒否（`rr`。左だけの行が落ちる）→ 左右を入れ替えた `Left`（右が保存側）、両方 → `Inner`（レビュー対応 R-27: 以前の表は向きが逆だった） |
| 正しさの条件 | NULL 拡張された行（NULL 側の全列が NULL）が上の述語で必ず落ちるなら、その行はなくても結果が同じ |
| PG の対応関数 | `reduce_outer_joins`、`reduce_outer_joins_pass2`（prepjointree.c）、`find_nonnullable_rels`（clauses.c） |
| 例 | `select * from t left join u on t.a = u.a where u.d = 3` → `Join Inner`（検証済み: PG は `Hash Join`）。`... where u.d is null` はそのまま（`IS NULL` は NULL を拒否しない。検証済み: `Hash Left Join` + `Filter: (u.d IS NULL)`） |

**手順**（上から下へ `quals` を持ち回る）:

```rust
fn reduce(plan: LogicalPlan, quals: &[LExpr], ctx) -> Result<LogicalPlan>;
```

- `Filter { input, p }`: `quals' = quals ++ conjuncts(p)` で `input` に再帰。
- `Join { Inner }`: `quals' = quals ++ conjuncts(on)` で左右に再帰。
- `Join { Semi }`: 左は `quals`、右は `conjuncts(on)`（ON を満たさない右の行は一致しないので落ちてよい）。`Join { Anti }`: 左は `quals`（**`on` は使わない**）、右は `conjuncts(on)`。
- `Join { Left, l, r, on }`: `rejects = quals.iter().any(|q| rejects_null(q, cols(r)))`。`true` なら `Inner` に変えて `Inner` の規則（`quals' = quals ++ conjuncts(on)`）。`false` なら左に `quals`、右に `conjuncts(on)`（ON を満たさない右の行は一致しない）。
- `Join { Full, l, r, on }`: `rl = rejects(cols(l))`（上の述語が**左の列が NULL の行**＝右だけの行を落とす）、`rr = rejects(cols(r))`（**右の列が NULL の行**＝左だけの行を落とす）。`rl ∧ rr` → `Inner`。**`rl` → 左の列が NULL の行（右だけの行）が落ちるので、残るのは一致行と左だけの行 = 左が保存側の `Left`**。**`rr` → 右の列が NULL の行（左だけの行）が落ちるので、残るのは一致行と右だけの行 = 右が保存側 = 左右を入れ替えた `Left`（`Right` 相当）**。どれもなければ左右に `conjuncts(on)` を渡さず `quals` も渡さない（空）。変換した場合は変換後の種類の規則で再帰。PostgreSQL の `reduce_outer_joins` も「左の rel に strict な述語 → `JOIN_LEFT`、右の rel に strict → `JOIN_RIGHT`」で、`t FULL JOIN u ON t.a = u.a WHERE u.d = 3` は `u.d` が右の列なので `rr`（右が保存側の `Left`。`u` だけにある行が残る）になる（レビュー対応 R-27）。
- `Project` / `Aggregate` / `Distinct` / `Sort` / `Limit` / `SetOp` / DML / 葉 / 式の中のサブクエリの plan: `quals = []` で子に再帰（上の述語は、これらの下の行を落とすとは限らない・列の意味が変わる）。

#### 6.4.1 `nulls_out` と `rejects_null`

`cols` がすべて NULL のとき…

| 式 | `nulls_out(e, cols)`（e が NULL になる） | `rejects_null(e, cols)`（e が NULL か false になる） |
|---|---|---|
| `Column(c)` | `c ∈ cols` | （boolean の列）`c ∈ cols` |
| `Literal(NULL)` | true | true |
| `Operator` / `Function`（strict）、`Cast`、`CoerceTypmod` | いずれかの引数が `nulls_out` | 同左 |
| `Operator` / `Function`（strict でない）、`Coalesce`、`Case`、`NullIf` | false | false |
| `IsNotNull(x)` | false | `nulls_out(x)` |
| `IsNull(x)`、`BoolTest` | false | false |
| `Not(x)` | `nulls_out(x)` | `nulls_out(x)`（`NOT NULL` は NULL） |
| `And(args)` | false | いずれかの引数が `rejects_null` |
| `Or(args)` | false | すべての引数が `rejects_null` |
| `InList { expr, list }`、`Like { expr, pattern }` | `nulls_out(expr)` ∨ `nulls_out(pattern)`（Like）| 同左（`InList` は `expr` が NULL なら NULL） |
| `SubLink`、`SubLinkOutput`、`Aggregate` | false | false |

関数・演算子の strict は `BuiltinFunction.strict` / 演算子の `provolatile` と同じ `PROCS` の `strict`（`proc_by_oid(operator_meta(op.oid).proc_oid).strict`）。

### 6.5 R5 述語の押し下げ（`pushdown.rs`）

| 項目 | 内容 |
|---|---|
| 目的 | `Filter` の述語と結合の ON の述語を、列がそろう最も下のノードへ移す（走査の述語・インデックス選択・結合のビルド側の縮小のため）。`HAVING` のうち集約を使わない項をグループ化の下へ、派生表の述語を中へ |
| 適用先 | すべてのノード（根と入れ子の plan のそれぞれ） |
| 前提条件 | 述語が Volatile でない（Volatile な項は元の `Filter` の位置に残し、移動しない。PostgreSQL も Volatile な述語を派生表の中へ押し下げない） |
| 変換 | `push(plan, preds)`（下の手順） |
| 正しさの条件 | 結合の種類ごとの表（§6.5.1）。述語は元の順序を保つ。評価順の入れ替えで、行を落とす述語が先に評価されて副作用のないエラーが変わることはある（PostgreSQL も述語の評価順を保証しない） |
| PG の対応関数 | `distribute_qual_to_rels`（PG:src/backend/optimizer/plan/initsplan.c）、`subquery_planner` の HAVING → WHERE（planner.c）、`qual_is_pushdown_safe`（allpaths.c） |
| 例 | `select * from t join u on t.a = u.a where t.b = 1 and u.d = 2 and t.c = u.e` → `Join (t.a = u.a ∧ t.c = u.e) ( Filter (t.b = 1) (Get t), Filter (u.d = 2) (Get u) )` |

```rust
/// preds: この plan の出力に対して成り立つべき述語（非 Volatile の conjunct。元の順）
fn push(plan: LogicalPlan, preds: Vec<LExpr>, ctx) -> Result<LogicalPlan>;
fn wrap(plan: LogicalPlan, preds: Vec<LExpr>) -> LogicalPlan;     // 空なら plan、あれば Filter { plan, and_all(preds) }
```

ノードごとの規則（各項の参照列 `refs = expr_refs(p)`、`out = plan_output(…)`。**`refs` のうち出力に定義される列は `out` との交わりで見る。出力に定義されない列（外側の列・パラメータ）は無視する**）:

| ノード | 規則 |
|---|---|
| `Filter { input, p }` | `conjuncts(p)` を、Volatile でないものは `preds` に足して `push(input, …)`、Volatile なものは結果の上に `Filter` として残す |
| `Get` / `Values` / `FunctionScan` / `CteScan` | `wrap(plan, preds)` |
| `Result { one_time_filter, .. }` | `preds` を `one_time_filter` に AND で足す（`Result` に列はないので、述語は定数・パラメータだけ） |
| `Empty` | そのまま |
| `Project { input, exprs }` | `refs ∩ ids(exprs)` の各 ID の式が Volatile でなく `SubLink` を含まない述語は、`substitute(p, id → 式)` して `input` へ押し下げる（`Project` の上に残らない）。そうでない述語は `Project` の上に `Filter` で残す |
| `Aggregate { group_by, aggs, input }` | **`group_by` が空なら何も押し下げない**（`having false` を下へ押すと 0 行の入力から 1 行が出てしまう。検証済み: PostgreSQL は `select count(*) from t having false` を `Aggregate` + `Filter: false`（HAVING に残し、同じ述語を WHERE にも写す: 下の `Result` + `One-Time Filter: false`）にする。yuzhu は写さず HAVING に残すだけ。`Filter(false)` は R1 で `Empty` になる）。空でないとき、`refs` が `group_by` の ID だけの述語（集約の結果の ID を含まない）は `substitute(p, group id → group の式)` して `input` へ。式が Volatile なら押し下げない。残りは上に `Filter`（これが HAVING） |
| `Distinct { on: None }` | 全部 `input` へ（行ごとの決定的な述語なので、前でも後でも同じ） |
| `Distinct { on: Some }` | 押し下げない（どの行が残るかが変わる） |
| `Sort` | 全部 `input` へ |
| `Limit` | 押し下げない。`Filter { Limit { push(input, []) } }` |
| `SetOp { cols, left_cols, right_cols }` | `refs ⊆ cols` の述語を `substitute(p, cols[i] → left_cols[i])` で左の枝へ、`right_cols` で右の枝へ**両方に**押し下げる（`UNION` / `INTERSECT` / `EXCEPT` の `ALL` あり・なしとも、述語が行の値だけで決まるので成り立つ）。`SetOp` 自身には残さない |
| `Insert` / `Update` / `Delete` | `push(input, [])` |
| `Join` | §6.5.1 |

押し下げる前に、式の中のサブクエリの plan にも `push(plan, [])` を適用する。

#### 6.5.1 結合の種類ごと

`L = plan_output(left)`、`R = plan_output(right)`。

| 種類 | 上から来た述語（`preds`） | `on` の述語 |
|---|---|---|
| **Inner** | `preds ++ conjuncts(on)` をまとめて分類: `refs ∩ (L ∪ R)` が `L` だけ → 左へ、`R` だけ → 右へ、**列を 1 つも参照しない（定数・パラメータだけ）→ 左へ**、それ以外（両方を参照）→ この結合の `on` に残す | （上と一緒に分類） |
| **Left**（保存側 = 左） | `refs ⊆ L` → 左へ。それ以外は `Join` の上に `Filter` で残す（NULL 側へは押し下げない。R4 を通っても残ったもの） | `refs ⊆ R` → **右へ**（ON を満たさない右の行は一致しないだけ）。`refs ⊆ L` は**左へ移さず** `on` に残す（左の行は ON を満たさなくても NULL 拡張で出力される）。両方を参照 → `on` に残す |
| **Full** | 押し下げない（`Join` の上に `Filter`） | 押し下げない（`on` に残す） |
| **Semi** | `refs ⊆ L` → 左へ（出力は左だけ） | `refs ⊆ R` → 右へ。`refs ⊆ L` → **左へ**（ON を満たさない左の行は出力されないので、左の行を先に絞ってよい）。両方 → `on` |
| **Anti** | `refs ⊆ L` → 左へ | `refs ⊆ R` → 右へ。`refs ⊆ L` → `on` に残す（ON を満たさない左の行は**出力される**ので左を絞れない）。両方 → `on` |

押し下げ後、`Join` の子は `push(left, 左へ行く述語)`、`push(right, 右へ行く述語)`、`on` は残りの述語の `and_all`。

#### 6.5.2 Semi / Anti の押し下げ

R2 が作った Semi / Anti は `Filter` の入力の根（内部結合の島の上）に置かれる。左に述語を押し下げたあと、**左が `Join { Inner }` または `Join { Left }` で、`on` の左の列の参照（`expr_refs(on) ∩ L`）がその結合の片側の出力だけに収まる**なら、Semi / Anti をその側へ沈める（再帰）。`Left` は左（保存側）の子にだけ沈める。沈めた先の側の述語押し下げと結合順序は R7 が扱う（沈めた Semi / Anti は島の葉の一部になる）。正しさ: Semi / Anti は左の行を落とすだけで、列を足さず、ON が参照しない側の行数を変えない。

#### 6.5.3 HAVING → WHERE

build が作る `Filter(having) → Aggregate` の `Filter` は、上の `Aggregate` の規則で、グループキーだけを参照する項が `Aggregate` の下へ移る。移らない項（集約の結果を参照する項）が `HAVING` として残る。

#### 6.5.4 任意拡張: 定数の推移（L1 に余力があれば）

`Join { Inner }` の左右で、`Column(c) = 定数式` の述語（演算子が同じ型の `=` で、`c` の型と定数式の型が同じ）と、`on` の等値の項 `Column(c) = Column(d)`（同じ型の `=`）があれば、`Column(d) = 定数式` を `d` の側へ足す。インデックス選択（`t.id = 5 AND t.id = q.pid` の `q.pid`）に効く。PostgreSQL の EquivalenceClass の一部。実装しなくても結果は同じ。

### 6.6 R6 結合キーの正規化（`join_keys.rs`）

| 項目 | 内容 |
|---|---|
| 目的 | `Join.on` を「等値キー… ++ 残り…」の順に並べ、キーを `左の式 = 右の式` の向きにそろえる（R7 が結合の連結を見分け、物理化が `HashJoin` のキーを取るため） |
| 適用先 | すべての `Join` の `on` |
| 前提条件 | `on` が `Some` |
| 変換 | `split_on(on, L, R)` の結果から `and_all(keys の Operator ++ residual)` を作る。キーは `Operator { op: キーの演算子, args: [左の式, 右の式] }`（向きが逆なら交換子の演算子に取り替える） |
| 正しさの条件 | 項の集合は変わらない（並べ替えと、等値の両辺の入れ替えだけ） |
| PG の対応関数 | `hash_inner_and_outer`、`select_mergejoin_clauses`、`generate_hashjoin_paths`（PG:src/backend/optimizer/path/joinpath.c）、`clause_sides_match_join`（restrictinfo.c） |
| 例 | `on (u.a = t.a and t.b < u.d)` で `left = t`、`right = u` → `on (t.a = u.a and t.b < u.d)` |

**`split_on(on, left, right, catalog) -> SplitOn`**: `on` の各 conjunct `c` について:

1. `c` が `Operator { op, args: [a, b] }` で、`op.name == "="` かつ `builtin::operator_merge_hash(op.oid).1`（`oprcanhash`）が true。
2. `ra = expr_refs(a) ∩ (left ∪ right)`、`rb` も同様。`ra ≠ ∅ ∧ ra ⊆ left ∧ rb ≠ ∅ ∧ rb ⊆ right` なら `(a, b)`、`ra ⊆ right ∧ rb ⊆ left`（両方とも非空）なら `(b, a)` で、このとき演算子を**交換子**に取り替える（`builtin::operator_meta(op.oid).com`。0 なら**キーにしない**。`com` の演算子は `builtin::operator_by_oid`。なければ L1 が `catalog::builtin` に足す。00 への変更提案 8）。
3. `a`・`b` のどちらにも Volatile と `SubLink` がない。
4. **キーの型 `key_type`**: 左の式の型 `A` と右の式の型 `B`。`A == B` ならその型。違えば、`A → B` の implicit キャスト（`builtin::find_cast(A, B)` の `context == Implicit`）があれば `B`、なければ `B → A` があれば `A`、どちらもなければキーにしない（整数の幅違い・int と float・date と timestamp はこれで決まる。00 §4.3 規約 2: ハッシュ・比較に使う 2 値は同じ型にそろえる）。
5. 通ったものを `keys`、それ以外を `residual`（元の順）に。

### 6.7 R7 結合順序（`join_order.rs`）

| 項目 | 内容 |
|---|---|
| 目的 | 内部結合の島の葉を並べ替えて、直積（結合条件のない結合）を避ける |
| 適用先 | 根が `Join { kind: Inner }` で、その親が内部結合でない島（最大の内部結合の部分木）。外部結合・Semi・Anti はその島の葉の一部（不透明な単位）として動かさない |
| 前提条件 | 島の葉が 2 つ以上、`MAX_JOIN_ISLAND`（64）以下 |
| 変換 | 下の貪欲法 |
| 正しさの条件 | 内部結合は可換・可結合。述語は、参照する葉がすべてそろった最初の結合に置く。島の出力列の集合は変わらない（順序は変わりうる） |
| PG の対応関数 | `standard_join_search`（PG:src/backend/optimizer/path/allpaths.c）、`join_search_one_level`（joinrels.c）、`have_relevant_joinclause` |
| 例 | `from a, b, c where a.x = c.x and b.y = c.y` → `((a ⋈ c) ⋈ b)`（構文順の `(a × b) × c` は直積を作る） |

**手順**:

1. **平坦化**: 根から `Join { Inner }` を左右に分解して、葉 `L[0..n)` を**構文順**（左から右）に集め、すべての `on` の `conjuncts` を `conds` に集める。葉は「`Join { Inner }` でないノード」（`Filter { Get }`、派生表の plan、外部結合の部分木、Semi / Anti など）。
2. 各葉の出力列から `owner: ColId → 葉の番号`。`conds[k]` ごとに `rels(k) = { owner[c] | c ∈ expr_refs(conds[k]) }`（出力に定義されない外側の列は無視）と、`is_equi(k)`（`split_on` の `keys` に入る等値の項）。
3. **貪欲法**:

   ```
   chosen = {0};  plan = L[0];  remaining = {1, .., n-1}
   while remaining ≠ ∅:
       for j in remaining（構文順）:
           rank(j) = 0  if ∃ k: j ∈ rels(k) ∧ rels(k) \ {j} ⊆ chosen ∧ rels(k) ∩ chosen ≠ ∅ ∧ is_equi(k)   // 等値でつながる
                   = 1  if ∃ k: 同じ条件で等値でない                                                       // 他の条件でつながる
                   = 2  otherwise                                                                          // つながらない（直積）
       j* = rank が最小の j のうち最初（構文順）
       placed = { k | まだ置いていない ∧ rels(k) ⊆ chosen ∪ {j*} }（rels(k) = ∅ の項は最初の結合に置く）
       plan = Join { Inner, left: plan, right: L[j*], on: and_all(placed の項) }   // on は join_keys::normalize_on で正規化
       chosen += j*;  remaining -= j*
   ```
4. 島が 1 つの結合（葉が 2 つ）でも手順を通す（`rank` による左右の入れ替えはしない。最初の葉が左のまま。ハッシュ結合のビルド側の選択は物理化）。
5. 左深い木だけを作る。葉の単位はそのまま（葉の中の島は別に並べ替える。入れ子の島は R7 の再帰で処理）。

- **統計もサイズも使わない**（04-D7）。したがって、同じ問い合わせはデータに関わらず同じ順序になる。
- 島が `MAX_JOIN_ISLAND` を超えたら、並べ替えずに構文順のまま（`on` の正規化だけ行う）。

### 6.8 R8 列の刈り込み（`prune.rs`）

| 項目 | 内容 |
|---|---|
| 目的 | 上で使わない列を落とし、集約でも使われない集約の呼び出しを取り除き、ハッシュ表・ソート・Materialize が溜める行を細くする |
| 適用先 | 根と入れ子の plan のそれぞれ。根の「必要な列」: `q.plan` は `q.output`、`q.ctes[i].plan` は `ctes[i].output`、入れ子の plan は `LogicalSubquery.output`、`Insert` / `Update` / `Delete` は `input_cols` / `old_cols ++ [ctid] ++ new_values の ID` |
| 前提条件 | — |
| 変換 | `prune(plan, required: ColSet)`（下の表） |
| 正しさの条件 | 必要な列は落とさない。`Aggregate` の group key は（その ID が必要でなくても）落とさない（グループの単位が変わる） |
| PG の対応関数 | `build_base_rel_tlists`、`create_scan_plan` の使用列だけを出す処理（setrefs.c の tlist の削り方） |
| 例 | `select t.c from t join u on t.a = u.a` → `Get t` の上に `Project[a, c]`、`Get u` の上に `Project[a]` |

| ノード | `required` から子の必要な列 |
|---|---|
| `Project { exprs }` | `required` に入る ID の式だけを残し（残りを捨てる。0 個でもよい）、子の必要な列 = 残した式の `expr_refs` |
| `Filter { p }` | `required ∪ expr_refs(p)` |
| `Join` | 左 = `(required ∪ expr_refs(on)) ∩ L`、右 = `(…) ∩ R`。Semi / Anti も同じ（右の出力は上に出ないが ON に使う） |
| `Aggregate` | `required` に入らない集約の呼び出しを捨てる。group key は残す。子 = group の式と、残した集約の引数・FILTER の `expr_refs` |
| `Distinct { on: None }` | 子の出力の**すべて**（重複除去の単位が変わる） |
| `Distinct { on: Some(e) }` | `required ∪ expr_refs(e)` |
| `Sort { keys }` | `required ∪ expr_refs(keys)` |
| `Limit` | `required ∪ expr_refs(limit, offset)` |
| `SetOp` | 左 = `left_cols` の全部、右 = `right_cols` の全部（集合演算は全列で比較する） |
| `Get` | 刈り込めない（`Get` は全ユーザー列を出す。00 §8）。**刈り込み用の `Project` を足す**（下） |
| `Values` / `FunctionScan` / `CteScan` / `Result` / `Empty` | 葉。何もしない |
| `Insert` / `Update` / `Delete` | 子の必要な列 = 上に書いた根の必要な列 |
| 式の中の入れ子の plan | `required = LogicalSubquery.output` |

**刈り込み用の `Project`**: `Get`（または `Filter { Get }`）の親が `Join` / `Sort` / `Distinct` のときだけ、`required ∩ cols(Get)` が `Get` の全列の真部分集合なら、その `Get`（`Filter` があればその上）の上に `Project { exprs: 必要な列のパススルー }`（元の順）を置く。親が `Project` / `Aggregate` / `Filter` のときは足さない（その親が行を細くする、または溜めない）。システム列（`ctid` など）は `required` にあればパススルーする。

- 刈り込み用の `Project` は物理化で `PhysicalPlan::Project` になる。`Filter { Get }` は走査の `filter` に融合し、その上の `Project` は残る（`Project` は走査の出力を細くするだけ）。

---

## 7. 物理化（`planner/physicalize.rs`、`index_select.rs`、`size.rs`、L2）

### 7.1 骨格

```rust
pub fn physicalize(q: LogicalQuery, env: &PlanEnv<'_>) -> Result<PhysicalQuery> {
    let mut p = Physicalizer::new(env, &q.arena, count_leaves(&q));
    for cte in &q.ctes { /* 共有 CTE の plan を phys → ensure_layout(cte.output) → p.ctes に積む。CteId → 添字 */ }
    let root = p.phys_root(&q.plan, &q.output)?;          // phys + finish_root
    Ok(PhysicalQuery { root: root.plan, subplans: p.subplans, ctes: p.ctes, n_params: p.next_param as usize,
                       output: q.columns, explain: root.explain })
}
```

- 共有 CTE は `LogicalQuery.ctes` の順に物理化する（`CteId` = 添字。`PhysicalPlan::CteScan { cte }` の添字は `PhysicalQuery.ctes` と同じ並び）。CTE の plan も根と同様に最後に `ensure_layout(cte.output)` をそろえる。
- **`finish_root(phys, output)`**: `phys.layout == output` ならそのまま。そうでなければ（resjunk を落とす・並びを変える）`Project { exprs: output の各 ID の Local の位置 }`。`output` に同じ ID が 2 回ある場合も、位置を引くので動く。DML は出力なし。
- `phys(plan, params) -> Result<Phys>` が `LogicalPlan` を再帰する（`params: &HashMap<ColId, ParamId>` は、この部分木の式で自由な列を `PhysCol::Param` に降ろす対応表。根では空）。深さは `MAX_PLAN_DEPTH` で `54001`。
- `Phys.layout` は物理ノードの出力列（ColId の並び）。各物理ノードの出力の並びは次のとおり（`Project` で並べ替えない限り 00 §9.2 のコメントの形）:

| 論理 | 物理 | 出力（layout） |
|---|---|---|
| `Get`（+ 直上の `Filter`）| `SeqScan` / `IndexScan`（§7.3） | `cols ++ system_columns の ID` |
| `Filter`（`Get` の直上以外）| `Filter` | 入力 |
| `Values` | `Values { rows }` | `cols` |
| `FunctionScan` | `FunctionScan { func, args }` | `cols` |
| `CteScan` | `CteScan { cte: 添字 }` | `cols`（CTE の出力と位置で対応） |
| `Project` | `Project { exprs }`（恒等なら省略。`Result` の直上なら `Result { exprs, one_time_filter }` に併合） | `exprs` の ID |
| `Join` | §7.5 | 左 ++ 右（Semi / Anti は左）。Inner の入れ替え（§7.5.3）は `Project` で左 ++ 右に戻す（00 §9.2、05 D5-8。レビュー対応 R-14） |
| `Aggregate` | §7.6.1 | `group_by` の ID ++ `aggs` の ID |
| `Distinct { on: None }` | `Distinct` | 入力 |
| `Distinct { on: Some }` | `Unique { key_cols }`（§7.6.2） | 入力 |
| `Sort` | `Sort { keys }`（§7.4 で省略されることがある） | 入力 |
| `Limit` | `Limit { limit, offset }` | 入力 |
| `SetOp` | §7.6.3 | `cols` |
| `Result` | `Result { exprs: [], one_time_filter }` | `cols`（M4 では空） |
| `Empty` | `Result { exprs: [], one_time_filter: Some(false) }` | `cols`（行を出さない） |
| `Insert` / `Update` / `Delete` | §7.8 | （なし） |

**恒等な `Project` の省略**: `exprs` がすべて `Column(Local(i))`（`i` は並びの位置）で、数が子の出力の数と等しいとき省略し、`layout` は `Project` の ID とする。

### 7.2 式を降ろす（`lower`）

`Physicalizer::lower(e, cx)` は §3.5 のとおり。補足:

- `Column(c)` の探索は、`cx.layout` に `c` が**複数ある**ときは最初の位置。結合の `join_filter` / `residual` は `layout = 左 ++ 右`。`HashJoin` のキーは `left_keys` が左の layout、`right_keys` が右の layout。インデックスのキーの値・`Limit` の式・`Values` の行・`FunctionScan` の引数は `layout = []`（列を含むと `Error::internal`）。
- `SubLink` を含む式の降ろしは `plan_sublink`（§7.7）を呼ぶ。入れ子の SubLink は内側が先に `subplans` に入る。

### 7.3 走査の選択（`phys_scan`）

`Get` と、その直上の `Filter`（R5 が 1 つにまとめた述語）を 1 つの走査にする。

```rust
fn phys_scan(&mut self, get: &GetNode, preds: Vec<LExpr> /* conjuncts。元の順 */,
             want_order: Option<&[OrderKey]>, outer: Option<&OuterScan>, params: &ParamMap) -> Result<(Phys, Option<ScanInfo>)>;
```

**手順**:

1. `layout = cols ++ system_columns の ID`。`req = ScanRequest { table, rel, cols, conjuncts: &preds, outer_cols（通常は空）, want_order }`。
2. **候補述語の抽出** `extract_quals`（インデックスごと、キー列ごと）。述語 `c`（`conjuncts` の添字 `i`）が次のいずれかのとき、インデックスの列 `col`（`index.columns[j].attnum`、`opfamily = index.columns[j].opfamily`）に対する `IndexQual` になる:
   - `Operator { op, args: [x, y] }`:
     1. `x` が**列式**（`Column(c)`、`c ∈ cols`（ユーザー列）、`c` の attnum が `col`、または `Cast { method: Binary }` で包んだもの）で、`y` が**定数式**（このスキャンの列 ID を含まず、`expr_volatility != Volatile`。パラメータ・InitPlan・外側の列・`outer_cols` を含んでよい）のとき: `catalog::opclass::operator_strategy(op.oid, opfamily)` が `Some((s, lt, rt))` で、`x.ty.oid == lt` かつ `y.ty.oid == rt` なら、`s` を `QualKind`（1 → `Lt`、2 → `Le`、3 → `Eq`、4 → `Ge`、5 → `Gt`）にして `value = y`。
     2. `y` が列式で `x` が定数式のとき: 交換子 `com = operator_meta(op.oid).com`（0 なら不可）の演算子について `operator_strategy(com, opfamily)` を引き、型は交換した引数で同様に調べる。
     3. `<>` や B+Tree の族にない演算子は `operator_strategy` が `None` で、候補にならない。
   - `IsNull(列式)`: `QualKind::IsNull`（`value = None`）。`IsNotNull` は使わない。
   - `And` の中身は `conjuncts` で平坦化済み。`Or`、`InList`、`Like`、`Not`、`SubLink` は候補にしない（D-18）。
3. **インデックスごとの使い方（`IndexPick`）**:
   - `eq`: インデックスの先頭の列から順に、その列に `Eq` または `IsNull` の候補があれば**最初の 1 つ**を取って次の列へ。なければそこで止める。
   - `lower` / `upper`: 止めた列（`eq.len()` 番目）に、`Gt` / `Ge` があれば最初の 1 つ（`lower`）、`Lt` / `Le` があれば最初の 1 つ（`upper`）。
   - `eq` が空で `lower` も `upper` もなければそのインデックスは使えない。
   - `unique_full = index.unique ∧ eq.len() == index.columns.len() ∧ eq にIsNull がない`（NULL は一意性に反しない）。
4. **スコア**: `score = (unique_full as u8, eq.len(), lower と upper の数)` を辞書順に比べて最大を選ぶ（D-18「一意で全列等値 > 等値の列数 > 先頭列の範囲」）。**同点は `IndexDef.oid` が小さいほう**（`rel.indexes` は OID 昇順なので、先に見たほうを残す）。
5. **`enable_*`**: 候補があるとき、`use_index = settings.enable_indexscan || !settings.enable_seqscan`（`enable_indexscan = off` で `enable_seqscan = on` のときだけ Seq Scan を選ぶ。両方 off なら Index Scan。PostgreSQL の「無効なパスにはコストを足すだけ」と同じ結果になる）。候補がなければ Seq Scan（どちらの設定でも）。
6. **出力**:
   - Seq Scan: `SeqScan { rel, columns: 表の列の型, system_columns: Get の system_columns の種別, filter: lower(and_all(preds)) }`。
   - Index Scan: `IndexScan { rel, index: rel.indexes[pick.index].clone(), keys: IndexScanKeys { eq: pick.eq → IndexScanKey::Eq(lower(value)) か IsNull, lower: pick.lower → RangeBound { expr: lower(value), inclusive: kind == Ge }, upper: 同様（inclusive: kind == Le） }, direction: pick.direction, columns, system_columns, filter: lower(and_all(preds)) }`。**使った述語も `filter` に残す**（04-D9）。値の式は `layout = []` の文脈で降ろす（`Param` は `params` と NLJ の外側の列）。
   - `Eq` の値が実行時に NULL になる場合（パラメータ・InitPlan の結果が NULL）、**0 行を返すのは executor の責任**（`ResolvedScanKeys.eq` の `None` は `IsNull` のときだけ。05・06）。範囲の境界が NULL のときも 0 行。
   - `ScanInfo { used: Vec<usize> /* 使った述語の添字 */, index_name, backward }` を返す（EXPLAIN の `Index Cond:` / `Filter:` の分割に使う。§8）。

境界条件:

- `preds` が空でインデックスの要求順序もなければ Seq Scan。
- 同じ列に `a = 1 AND a = 2` のように複数の等値があるとき、最初の 1 つをキーにし、残りは `filter` が再評価する（結果は 0 行）。
- システム列（`ctid` など）・式・複数の表にまたがる述語は候補にならない。
- B+Tree は NULL を最大として持つ。下限だけの範囲（`a > 5`）は NULL の項目にも到達しうるが、`filter` の再評価（NULL > 5 は NULL で落ちる）が正しさを守る。NULL を打ち切るのは 06 の最適化。

### 7.4 ソートの省略（M4 後半・任意 S）

`Sort { input, keys }` について、入力が `Project*` と `Filter*` を通って `Get` に至り、**すべての `keys[i].expr` が `Column(c)` で、`c` を `Project` のパススルーをたどって `Get` の列（`cols`）に戻せる**とき、インデックスの順序でソートを省略できる。

```rust
/// 順序要求 keys がインデックス pick の走査順で満たされるか。満たせば走査の向き
pub fn order_satisfied(index: &IndexDef, eq_cols: &[i16], keys: &[OrderKey], cols: &[(ColId, i16 /* attnum */)]) -> Option<ScanDirection>;
```

1. `keys[i]` の `col` を attnum に直す。`eq_cols`（等値で固定された列）に含まれる attnum のキーは常に満たされるので飛ばす。
2. 残りのキーが、`eq.len()` 番目以降のインデックスの列と順に一致する（attnum が等しい）。
3. 一致した各キーについて、`(descending, nulls_first)` が `(index.columns[j].descending, .nulls_first)` と**同じなら Forward、両方反転なら Backward**。全キーで同じ向きでなければ不可。
4. 満たすなら向きを返す。

**適用の条件**（どれかを満たすとき。M4 は「ソートを省くためだけの全インデックス走査」を無条件にはしない）:

- (a) 述語から選んだ `pick`（§7.3）がすでにあり、その `pick` の走査順が `order_satisfied`（等値で固定された列を `eq_cols` に）→ そのまま使い、`Sort` を省く。
- (b) `Sort` の**直上が `Limit`**（定数の limit）、または `enable_sort = off` のとき: `enable_indexscan` が on（または `enable_seqscan` が off）なら、`order_satisfied` となるインデックスを全部調べ、スコア（§7.3 の 4）が最大のもの（述語が使えなければ全インデックス走査 `keys` なし）を選ぶ。
- どちらも満たさなければ `Sort` を残す。

`Sort` を省く場合も、`Limit` / `Project` / `Filter` は残る。`Backward` の走査は 06 が検証する（M4 では、後ろ向きスキャンの単体テストが主で、プランナはこの任意項目でだけ使う。D-18）。

### 7.5 結合（`phys_join`）

`Join { kind, left, right, on }`。`L = plan_output(left)`、`R = plan_output(right)`、`split = split_on(on, L, R)`。

```rust
enum Algo { IndexNl { swap: bool, pick: IndexPick }, Hash, Nl }
fn choose_join_algo(&self, kind: JoinKind, left: &LogicalPlan, right: &LogicalPlan, split: &SplitOn) -> Result<Algo>;
```

**選択手順**（04-D10）:

1. `kind == Full`: `split.keys` が空なら `Err(0A000, "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions")`（PostgreSQL と同じ文言。検証済み）。キーがあれば `Hash`（`enable_hashjoin` の設定に関わらず。他に選択肢がない）。
2. `settings.enable_nestloop ∧ settings.enable_indexscan` のとき、**内側 Index Scan の Nested Loop**（§7.5.3）を試す。成功すれば `IndexNl`。
3. そうでなければ:

   | `keys` あり? | `enable_hashjoin` | `enable_nestloop` | 結果 |
   |---|---|---|---|
   | あり | on | 任意 | `Hash` |
   | あり | off | on | `Nl` |
   | あり | off | off | `Nl`（**等値結合で両方 off なら nestloop**。D-13、00 §15.4） |
   | なし | 任意 | 任意 | `Nl`（キーがなければ他に手がない。`enable_nestloop = off` でも） |

#### 7.5.1 Hash Join

```rust
PhysicalPlan::HashJoin {
    kind, left, right,
    left_keys:  keys.map(|k| lower(coerce_expr(k.left,  k.key_type), left_cx)),      // 左の行
    right_keys: keys.map(|k| lower(coerce_expr(k.right, k.key_type), right_cx)),     // 右の行
    key_types:  keys.map(|k| k.key_type),
    residual:   and_all(split.residual).map(|e| lower(e, 左 ++ 右の layout)),
    build_is_left, left_width, right_width,
}
```

- **ビルド側の選択**: `wl = estimate(left)`、`wr = estimate(right)`（§3.6）。`Semi` / `Anti`: `build_is_left = false`（サブクエリ側 = 右）。`Inner` / `Left` / `Full`: `build_is_left = wl < wr`（**同点は右をビルド**）。`Left` で `build_is_left = true` のとき、保存側（左）の一致しなかった行をプローブの後で出す処理は executor（05）。EXPLAIN の名前は `Hash Right Join`（§8）。
- `residual` には、キーにならなかった項（等値でない条件、片側だけを参照する ON の項、型がそろわなかった等値）が入る。**外部結合では residual が「一致」の判定に使われる**（一致しなければ NULL 拡張）。
- 結合キーの値が NULL の行は一致しない（05）。キーの型は `key_type` にそろえた値で `hash_datum`（00 §12.2）。
- `left_width` / `right_width` は左右の `layout.len()`。

#### 7.5.2 Nested Loop

```rust
PhysicalPlan::NestedLoopJoin { kind, outer: left, inner, join_filter: and_all(全 conjuncts(on)) を lower, outer_width, inner_width }
```

- `inner` は、`settings.enable_material ∧ !inner.uses_params() ∧ inner が Materialize / Values / Result / CteScan のいずれでもない` なら `Materialize { input }` で包む（外側の行ごとの再走査を、メモリに溜めた結果の読み直しにする。検証済み: PG も `Nested Loop` + `Materialize`）。`uses_params()` が true（相関するサブクエリの内側など）は再実行が必要なので包まない。
- 結合の条件は `on` の全項を `join_filter` に（Semi / Anti も同じ）。Left のとき、`join_filter` を満たさない外側の行は内側を NULL にして出す（05）。
- `kind == Full` は来ない（手順 1）。

#### 7.5.3 NestedLoopParam（内側 Index Scan）の条件 `try_inl`

`Inner` / `Left` / `Semi` / `Anti` で、次をすべて満たすとき。向きは、まず内側 = 右（`swap = false`）。`kind == Inner` のときだけ、だめなら内側 = 左（`swap = true`: **論理の左右を入れ替えて**、`outer = 右`、`inner = 左` で作る）も試す。`Left` / `Semi` / `Anti` は左が外側で固定。

**出力の並び（レビュー対応 R-14）**: 物理の `NestedLoopParam` / `NestedLoopJoin` は常に `outer ++ inner` を出し、`outer` は論理プランの left（00 §9.2「結合の出力は左 ++ 右。入れ替えは内部の事情」、05 D5-8・05-P7、02 §3.6.1）。`swap = true` のときは `NestedLoopParam { outer: 論理の右, inner: IndexScan(論理の左) }` の出力が右 ++ 左になるので、**その上に論理の左 ++ 右の順に並べ直す `Project`（`Column(Local(i))` だけ）を必ず置く**。これで `Phys.layout` は swap の有無によらず左 ++ 右になり、後続のルール・`validate`・`ensure_layout` は結合の向きを知らなくてよい。Project の費用は出力行ごとの列の並べ替え 1 回で、EXPLAIN では透過（§8.2）。内側が左のときの `Index Cond` の外側の列は、表示では外側の列（右側の表の列）として `Param` の規則で修飾される（10 §3.5）。

1. 内側の論理プランが `Get` または `Filter { Get }`（R5 が押し下げた内側だけの述語を持つ）。内側の表にインデックスがある。
2. `outer_cols = plan_output(外側)`。内側の走査の述語 = `内側の Filter の conjuncts ++ conjuncts(on)`。`choose_scan` を `outer_cols` つきで呼ぶ（外側の列を含む式が「値の式」になる。§7.3 の 2）。
3. 選ばれた `pick` が、**外側の列を参照する値を持つ述語を 1 つ以上使っている**（そうでなければ結合のためのインデックス走査ではない）。
4. **サイズの条件**: `probes = estimate(外側) × ROWS_PER_BLOCK_EST`、`probe_cost = 3.0（unique_full）/ 6.0（eq が 1 つ以上）/ 12.0（範囲だけ）`、`inner_blocks = max(nblocks(内側の表), 1)`。`probes × probe_cost < inner_blocks`。

成功したら:

```rust
PhysicalPlan::NestedLoopParam {
    kind, outer: phys(外側),
    inner: IndexScan { keys: pick（値の式は外側の列 → ParamId）, filter: 内側だけの述語 ++ 使った結合の述語（再評価）, .. },
    params: 外側の列のうち値の式が参照するものごとに (新しい ParamId, Column(外側の layout の位置)),
    join_filter: 使わなかった結合の述語（外側の列を参照するものなど）の and_all,
    outer_width, inner_width,
}
```

- 内側の `IndexScan` の値の式の中の外側の列は、新しい `ParamId` に降ろす（`params` の写像に追加して内側を物理化する）。
- Left / Semi / Anti では、内側の走査の `filter` が ON の条件を表す（満たさない内側の行は一致しない）ので、意味は変わらない。
- executor は外側の行ごとに `params` を `ctx.params` に設定し、内側を `rewind` する（00 §10。内側は `uses_params() == true` なので溜め直す）。

### 7.6 集約・DISTINCT・集合演算

#### 7.6.1 集約

```
Aggregate { input, group_by, aggs }:
  group_by が空:       Aggregate { input, aggs }
  group_by あり:
     enable_hashagg:   HashAggregate { input, keys, key_types, aggs }
     そうでなければ:   GroupAggregate { input: Sort { input, keys: group の式の昇順 NULLS LAST }, keys, key_types, aggs }
```

- `keys` は group の式を入力の layout で降ろしたもの。`key_types` は式の型。
- `PhysAgg { kind: func.kind, arg_types: 引数の型, args: 降ろした引数, distinct, filter: 降ろした FILTER, result: SqlType::of(func.result) }`。
- `enable_hashagg = off` でも `enable_sort = off` でも、集約には Sort が必要なので Sort を使う（他に手がない）。
- 出力の並び: `keys ++ aggs`（HashAggregate / GroupAggregate は group_by の ID ++ aggs の ID）。HAVING は `Filter`（上）。

#### 7.6.2 DISTINCT / DISTINCT ON

- `Distinct { on: None }` → `PhysicalPlan::Distinct { input }`。
- `Distinct { on: Some(exprs) }`: 入力は build が作った `Sort`（keys の先頭が ON の式）。`exprs` はすべて `Column(id)`（build の約束）で、`key_cols = 各 id の入力の layout の位置`。`Unique { input, key_cols }`。入力の Sort の keys の先頭 `exprs.len()` 個の集合が `exprs` の集合と一致しなければ `Error::internal`（keys は `order ++ 未出の ON の式`（§5.5）なので、ON の式が **keys の先頭にあるとは限らない**が、先頭 `exprs.len()` 個の集合としては一致する。同じ ON の値の行が隣接する条件。レビュー対応 R-03）。`key_cols` は入力の列位置の並びで、順序は問わない（Project を挟んで先頭に出すことはしない）。

#### 7.6.3 集合演算

`SetOp { op, all, left, right, cols, left_cols, right_cols }`。両方の枝を物理化し、`ensure_layout(left_cols)` / `ensure_layout(right_cols)` で出力の並びをそろえる。

| `op` / `all` | 物理プラン |
|---|---|
| `Union` / `all = true` | `Append { inputs }`。枝が `Union ALL` の `SetOp` ならその枝を平坦化して 1 つの `Append` に |
| `Union` / `all = false` | `Distinct { input: Append { inputs } }`。入れ子の `Union`（`ALL` でも）は平坦化して 1 つの `Append` にする（外側が重複除去するので結果は同じ） |
| `Intersect` / `Except`（`all` あり・なし） | `HashSetOp { op, all, left, right, key_types: cols の型 }` |

出力の layout は `cols`（`Append` の出力は枝の並びのまま。`ID` は `cols` に読み替える）。

#### 7.6.4 その他

- `Sort { input, keys }`: `SortKey { expr: lower(key.expr, 入力の layout), descending, nulls_first }`。`enable_sort` は集約・DISTINCT ON のソートの有無を変えないので無視する（§7.4 の例外を除く）。
- `Limit { input, limit, offset }`: 式は `layout = []` で降ろす（`Param` / InitPlan を含みうる）。`limit` が負なら実行時に `2201W`、`offset` が負なら `2201X`（05）。
- `Values { rows }`: 各式を `layout = []` で降ろす。`FunctionScan { func, args }`: 同様。
- `Result { exprs, one_time_filter }`: `one_time_filter` は `layout = []`。

### 7.7 SubPlan と CTE

#### 7.7.1 SubPlan の分類（`plan_sublink`）

`SubLink { kind, test, query }` を降ろすとき（04-D11）:

1. `free = plan_free_cols(query.plan)`。`free` の各列は `cx`（`layout` か `params`）で解決できなければならない（`Error::internal`）。
2. **内側の `params` 写像**: `free` の各 `f`（`ColId` 昇順）に新しい `ParamId` を発行（`u16` を超えたら `54000`）。`SubPlanDef.params = [(pid, lower(Column(f), cx))]`（外側の文脈で降ろす。`f` が外側でもパラメータなら `Column(Param(外側の pid))`）。
3. 内側の plan を、この写像だけを `params` に持つ文脈で物理化し、`ensure_layout(query.output)`。
4. **戦略**:
   - `free` が空:
     - `kind == Any` で `test` が**ハッシュ化できる**なら `Hashed { probe_keys, build_keys }`。
     - それ以外は `InitOnce`。
   - `free` が空でない: `Rescan`。
5. **ハッシュ化できる `test`**: `conjuncts(test)` の**すべて**が `Operator { op, args: [a, b] }` で、`op.name == "="` かつ `oprcanhash`、片側（`a` か `b`）が `SubLinkOutput(i)`（`Cast { Binary / Function }` で包んでよい）、もう片側が `SubLinkOutput` を含まない式（外側の行の式）のとき。キーの型は `split_on` と同じ規則（implicit キャストで 2 つを同じ型に。そろわなければハッシュ化しない）。`probe_keys = 外側の式（キー型にキャスト、cx で降ろす）`、`build_keys = Column(Local(i))`（内側の出力行の `i` 番目をキー型にキャスト）。
6. `SubPlanDef { plan, kind, test: test を lower（SubLinkOutput は残す）, params, strategy, explain }` を `subplans` に足し、`SubPlanId = 添字`（`u16` を超えたら `54000`）。式の中の `SubLink` ノードは `SubLink { kind, test: None, query: id }`。

- `SubPlanDef.test` の中の `PhysCol::Local` は**サブクエリを含む式を評価する側の行**（外側の行）の位置で、`SubLinkOutput(i)` は内側の現在の行の `i` 番目。`Hashed` の `probe_keys` は外側の行、`build_keys` は内側の行に対して評価する（05）。
- `Scalar` の「2 行以上で `21000`」、`Any` / `All` の三値論理、`Hashed` の NULL の扱いは executor（05）。
- 同じ `SubLink` が式の複製（R3 の置換）で 2 度現れることはない（R3 は `SubLink` を含む式を複製しない）。

#### 7.7.2 CTE

- `CteScan { cte, alias, cols }`: `cte_index[cte]` の添字で `PhysicalPlan::CteScan { cte }`。layout = `cols`。CTE の plan は `PhysicalQuery.ctes[添字]`（根と同じ手順で、`ensure_layout(cte.output)` をそろえる）。
- EXPLAIN では CTE の plan を `CTE x` の子として、根のノード（のうち式を持つ最初のノード）にぶら下げる（§8）。

### 7.8 DML

| 論理 | 物理 |
|---|---|
| `Insert { table, rel, input, input_cols, column_map, defaults, checks, not_null, returning }` | `Insert { rel, input: ensure_layout(phys(input), input_cols), column_map, defaults, checks: 名前（バイト列）順に並べ替え, not_null, table_name: table.name, returning }` |
| `Update { table, rel, input, old_cols, ctid, new_values, checks, not_null, returning }` | `Update { rel, input: ensure_layout(phys(input), old_cols ++ [ctid] ++ new_values の ID), n_user_cols: old_cols.len(), assigned: new_values の (attnum - 1, n + 1 + i), checks（名前順）, not_null, table_name, returning }` |
| `Delete { table, rel, input, old_cols, ctid, returning }` | `Delete { rel, input: ensure_layout(phys(input), old_cols ++ [ctid]), n_user_cols, returning }` |

- CHECK の評価順は名前のバイト列順（M1 の `plan_insert` / `plan_update` と同じ。PostgreSQL と同じ）。
- 入力の走査は通常の `Get`（Seq / Index）。UPDATE / DELETE の対象表のインデックス走査は、更新した行（新しい版）が同じ走査に現れない（コマンド ID の可視性。M2 の Halloween 対策）ことに依る。**この前提は 05 の IndexScan が `snapshot` のコマンド ID で新しい版を見ないことで満たされる**（未検証。§13）。
- `RETURNING` が `Some` で `PhysicalQuery.output` に列がある。

---

## 8. ExplainNode の構築（`planner/explain_tree.rs`、L2。整形は 10）

`PlanEnv.want_explain` が true のときだけ作る（false なら `PhysicalQuery.explain` / `SubPlanDef.explain` は `None` で、メモも作らない）。**構築と同時に 1 ノードずつ作る**と、物理プランの木の形（`Hash` ノードの合成など）と一致しないので、次の 2 段にする。

1. **メモ（`NodeNote`）**: 物理ノードを作るたびに（`Project` の省略など、ノードを作らなかったときは作らない）、その木（根・各 CTE・各 SubPlan のそれぞれ）の `Vec<NodeNote>` に**作った順（子が先の後順）**で 1 つ足す。
2. **組み立て（`assemble`）**: 物理プランが完成した後で、物理プランを先行順にたどり、メモを後順の添字で引いて `ExplainNode` の木を作る（物理プラン変種ごとの規則は下表）。ここで合成ノード・併合を行う。

```rust
pub struct NodeNote {
    pub title: String,                       // "Seq Scan on t"
    pub details: Vec<(String, String)>,      // ("Filter", "(b > 5)")
    pub output: Vec<String>,                 // VERBOSE の Output: 行。explain_verbose のときだけ入れる
    /// この節点の式の中に現れた SubPlan / InitPlan（SubPlanId）。式を降ろした順
    pub subplans: Vec<SubPlanId>,
}
/// 戻り値: 根の木と、SubPlanDef ごとの木（subplans と同じ添字）
pub fn assemble(q: &PhysicalQuery, root_notes: &[NodeNote], cte_notes: &[Vec<NodeNote>], sub_notes: &[Vec<NodeNote>])
    -> (ExplainNode, Vec<ExplainNode>);
```

### 8.1 `exec_id`（`ExplainNode` の計測値との対応。11 §7.1 の C-1・C-2）

`ExplainNode.exec_id: usize`（10 §3.2 の定義が正。以前の `plan_id: Option<u32>` は廃止）は、その行が表示する `PhysicalPlan` のノードの**先行順の通し番号**（`executor::instrument` の計測値と突き合わせる）。数え方は `planner::physical::assign_exec_ids`（10 §3.2。L2 と executor が共有する 1 つの関数）: `PhysicalQuery.root` の木を先行順（子は `PhysicalPlan` のフィールド順: `NestedLoop*` は `outer` → `inner`、`HashJoin` は `left` → `right`（`build_is_left` に関係なく）、`Append` は `inputs` の順）に 0 から、続けて `subplans[i].plan`（i の昇順）、最後に `ctes[i]`（i の昇順）の各木を連続して数える。合成ノード（`Hash`、`HashSetOp` の下の `Append` と `Subquery Scan`）は中身のノードの `exec_id` を借りる。併合した `Filter` の行は**最も外側のノード（`Filter`）の番号**、透過する `Project` を含む群も最も外側の番号（10 §3.2）。

`ExplainNode` を `PhysicalPlan` と同形にする案（`Hash`・`Append` を合成せず `Filter` / `Project` を独立のノードで出す）は採らない（C-1）。変えたい場合は `assemble` の合成・併合の規則を無効にする（10 と調整。04-Q8）。

### 8.2 物理ノードごとの規則

「走査の述語」は走査（`Seq Scan` / `Index Scan` / `Function Scan` などの `filter`）の述語で、列名は `explain_verbose` のときだけ表名で修飾する。それ以外（結合・ソート・集約）の式は `prefix_upper`（走査の葉が 2 つ以上、または `explain_verbose`）のとき修飾する（PostgreSQL と同じ。`show_scan_qual` と `show_upper_qual` の違い。検証済み: 単一表では `Filter: (b > 5)`、結合では `Hash Cond: (t.a = u.a)` だが結合の中の走査の `Filter: (d = 3)` は修飾なし、`Group Key: t.a` は派生表があると修飾あり）。

| `PhysicalPlan` | `title` | `details`（ラベル: 内容） | 子 |
|---|---|---|---|
| `Result` | `Result` | `One-Time Filter: <式>`（`one_time_filter` があるとき） | なし |
| `Values` | `Values Scan on "*VALUES*"` | — | なし |
| `SeqScan` | `Seq Scan on <表>[ <別名>]` | `Filter: <述語>`（`filter` があるとき） | なし |
| `IndexScan` | `Index Scan[ Backward] using <索引名> on <表>[ <別名>]` | `Index Cond: <使った述語>`（§8.3）、`Filter: <使わなかった述語>` | なし |
| `FunctionScan` | `Function Scan on <関数名>[ <別名>]` | — | なし |
| `Filter` | （子のノードに併合）| 子の `details` の末尾に `Filter: <述語>`。子が走査（`Seq Scan` 等）のときは走査の述語の規則、それ以外は上位の規則 | 子 |
| `Project` | （子のノードに透過）| `explain_verbose` のとき子の `output` を `Project` の式の表示で置き換える | 子 |
| `Sort` | `Sort` | `Sort Key: <k1>, <k2>…`（各キー: 式、`descending` なら ` DESC`、`nulls_first != descending` なら ` NULLS FIRST` / ` NULLS LAST`。検証済み: `a DESC NULLS LAST, b NULLS FIRST, c DESC`） | 子 |
| `Unique` | `Unique` | — | 子 |
| `Distinct` | `HashAggregate` | `Group Key: <出力のすべての列>` | 子 |
| `Limit` | `Limit` | — | 子 |
| `Materialize` | `Materialize` | — | 子 |
| `NestedLoopJoin` / `NestedLoopParam` | `Nested Loop[ Left Join / Semi Join / Anti Join]`（Inner は接尾辞なし） | `Join Filter: <join_filter>`（あるとき） | `[outer, inner]` |
| `HashJoin` | `Hash[ Join / Left Join / Right Join / Full Join / Semi Join / Anti Join]`（`kind == Left` かつ `build_is_left` のとき `Right Join`） | `Hash Cond: (<probe キー> = <build キー>) AND …`（`build_is_left` でなければ `(左キー = 右キー)`、そうなら `(右キー = 左キー)`）、`Join Filter: <residual>` | `[probe, 合成ノード Hash { children: [build] }]`。probe = `build_is_left ? right : left` |
| `Aggregate` | `Aggregate` | — | 子 |
| `HashAggregate` | `HashAggregate` | `Group Key: <keys>` | 子 |
| `GroupAggregate` | `GroupAggregate` | `Group Key: <keys>` | 子 |
| `Append` | `Append` | — | `inputs` |
| `HashSetOp` | `HashSetOp Intersect` / `HashSetOp Except`（`all` なら ` All`） | — | `[合成ノード Append { children: [left, right] }]` |
| `CteScan` | `CTE Scan on <CTE 名>[ <別名>]` | — | なし |
| `Insert` / `Update` / `Delete` | `Insert on <表>` / `Update on <表>` / `Delete on <表>` | — | 子（入力） |

- 表名: 通常は `table.name`、`explain_verbose` のとき `<スキーマ>.<表>`。別名は `Get.alias`（`Some` のときだけ）。
- `Output:`（`explain_verbose`）: そのノードの `layout` の各 ID の表示（常に表名修飾）。`Project` の透過で置き換える。`Insert` / `Update` / `Delete` は出さない。
- **`SubPlan` / `InitPlan`**（レビュー対応 R-06。10 §3.4 の 5・§3.11 の 8 と一致。PostgreSQL は InitPlan をそのクエリレベルの最上位ノードに付ける）: メモの `subplans` の各 `SubPlanId`（添字 `id`）を `ExplainChild { label: Some("InitPlan {id+1}" または "SubPlan {id+1}"), node: assemble した SubPlanDef の木 }` にして付ける。付ける先は戦略で違う。
  - `strategy == InitOnce`（InitPlan）: **そのメモを持つノードが属する問い合わせ階層の根の `ExplainNode`** に、**普通の子の前**（PostgreSQL の `InitPlan` は詳細行の直後）に付ける。問い合わせ階層とは、`assemble` が受け取る木 1 本ずつ（主問い合わせの `root_notes`、`cte_notes[i]` の各 CTE の本体、`sub_notes[j]` の各 SubPlan の本体）。したがって `assemble` は、各木の根を知っている（木ごとに呼ぶ）ので「階層の根が分かること」（10 §14.2 の依頼）は満たされる。DML は `Insert` / `Update` / `Delete` のノードが根（未検証）。根が併合・透過のため子の群と同一視される場合は、その群の最も外側の `ExplainNode`（`assemble` が返す木の根）に付ける。
  - `Rescan` / `Hashed`（SubPlan）: その式を**表示したノード**に、**普通の子の後**（検証済み: `SubPlan 2` は `Hash` の子の後に出る）。
  - 同じ根に複数付くときの順: CTE（添字順）、InitPlan（`SubPlanId` 昇順）、普通の子、SubPlan（`SubPlanId` 昇順）。
- **CTE**: 根のノードの子の先頭に `ExplainChild { label: Some("CTE <名前>"), node }` を、共有 CTE の添字順に足す（PostgreSQL の `CTE x` は根の `InitPlan` 扱い。検証済み）。
- **併合・透過のときの `subplans`**: 併合した `Filter` と透過する `Project` のメモの `subplans`（その式の中の SubPlan）は、併合先・透過先のノードのものとして扱う（ラベルつきの子はそのノードに付く）。
- 式の表示: `ExplainNames::expr`（§3.8）が `PhysExpr` を 10 の deparse に渡す。SubLink の表記は PostgreSQL 17 と同じ: スカラー `InitOnce` → `(InitPlan 1).col1`、`Rescan` → `(SubPlan 1)`、`Exists` の `InitOnce` → `(InitPlan 1).col1`、`Exists` の `Rescan` → `EXISTS(SubPlan 1)`、`Any` → `(ANY (a = (SubPlan 1).col1))`、`Hashed` → `(ANY (a = (hashed SubPlan 1).col1))`、`All` → `(ALL (a <> (SubPlan 1).col1))`（検証済み）。`NOT IN` は `Not` で包まれて `(NOT (ANY (...)))`。`Param`: `NestedLoopParam` のパラメータは外側の式の表示、サブクエリのパラメータは外側の列の表示（`ExplainNames.params`）。

### 8.3 `Index Cond`

`IndexPick` の使った述語を、**キー列の順に `eq`、続けて `lower`、`upper`** で並べる。各述語は `(<列> <演算子> <値の式>)`（演算子は `=` `<` `<=` `>=` `>`。`IsNull` は `(<列> IS NULL)`）。2 個以上は `(<p1> AND <p2>)` と 1 組の括弧で囲む。使わなかった述語（`conjuncts` のうち使った添字以外、元の順）が `Filter`。一方 `filter` の物理の欄には全述語が入っている（04-D9）ので、**`ScanInfo.used` から取り直す**（`filter` の文字列から引き算はしない）。

### 8.4 列名と式の表示（`ExplainNames`）

- 列 `c` の表示: `arena.get(c)` が `origin: Some`（ベーステーブルの列）なら `name`、修飾するときは `<qualifier>.<name>`。`origin: None` の列（集約・計算列）は `display[c]`（登録されていれば。`Aggregate` を物理化するとき `count(*)` / `sum(t.a)` の表示を登録、計算した `Project` の列は式の表示 `(a + 1)` を登録）、なければ `name`。
- `prefix_upper` は物理化の最初に論理プランの走査の葉（`Get`・`FunctionScan`・`Values`・`CteScan`。根・CTE・入れ子のサブクエリの全部）を数えて決める。
- `Hash Cond` などの 2 つの辺は、それぞれ左右の `layout` で表示する。

---

## 9. `PlannerSettings` と `enable_*` の意味

PostgreSQL の `enable_*` は「その種類のパスにコストを足す」だけで、他に選択肢がなければ使われる。M4 は**「他に選択肢があれば避ける。なければ使う」**とする（結果は設定に依らず同じ。プランだけが変わる）。

| 設定 | off のときの意味（M4） | 他に選択肢がないとき |
|---|---|---|
| `enable_seqscan` | 使えるインデックスの候補があれば Index Scan を使う（`enable_indexscan` が on のとき。両方 off なら Index Scan） | 候補がなければ Seq Scan |
| `enable_indexscan` | 使えるインデックスの候補があっても Seq Scan（`enable_seqscan` が on のとき）。内側 Index Scan の Nested Loop（§7.5.3）とソートの省略（§7.4）も無効 | — |
| `enable_hashjoin` | 等値キーがあっても Hash Join を避けて Nested Loop | FULL JOIN は Hash Join のまま |
| `enable_nestloop` | Nested Loop を避ける（等値キーがあれば Hash Join）。内側 Index Scan の Nested Loop も無効 | キーのない結合は Nested Loop。**等値結合で `enable_hashjoin` と `enable_nestloop` が両方 off なら Nested Loop**（00 §15.4） |
| `enable_hashagg` | `GroupAggregate`（`Sort` + 逐次集約） | GROUP BY なしの `Aggregate` は変わらない |
| `enable_sort` | `Sort` を避ける: インデックス順で満たせるソートの省略を `Limit` なしでも行う（§7.4 の (b)） | 集約・DISTINCT ON のソートは省けない |
| `enable_material` | Nested Loop の内側に `Materialize` を足さない | — |
| `query_mem_limit` | プランナは読まない（executor が使う） | — |

- `enable_indexonlyscan`・`enable_bitmapscan`・`enable_mergejoin`・`enable_tidscan` などは `PlannerSettings` に持たない（受け付けて保存するだけ。00 §15.4）。
- 各 `enable_*` の組み合わせで**結果が同じ**ことが `plan_variants` のテストの主題（§11.4）。

---

## 10. エラー

プランナが返すエラー。型・名前・構文のエラーは 03 が返し、プランナには来ない。

| 状況 | SQLSTATE | 文言 | 時点 |
|---|---|---|---|
| 定数畳み込みの評価エラー（`1/0`、`2147483647 + 1`、`'abc'::int`） | 評価した演算のもの（`22012` `22003` `22P02` など） | PostgreSQL と同じ（`division by zero` など） | R1 |
| FULL JOIN の条件にハッシュ可能な等値がない | `0A000` | `FULL JOIN is only supported with merge-joinable or hash-joinable join conditions` | 物理化 |
| 共有が要る CTE が外側の列を参照している（`MATERIALIZED` の明示または揮発性。非 `MATERIALIZED` で非揮発ならインライン。04-D4） | `0A000` | `WITH query "x" that references an outer query level is not supported`（yuzhu の文言） | build |
| `ColId` が `u32` を超えた | `54000` | `too many columns in query` | build |
| `ParamId` / `SubPlanId` が `u16` を超えた | `54000` | `too many parameters in query` / `too many subqueries in query` | 物理化 |
| 再帰が `MAX_PLAN_DEPTH` を超えた | `54001` | `stack depth limit exceeded` | build・ルール・物理化 |
| `nblocks` などストレージのエラー | そのまま | — | 物理化 |
| 契約違反（束縛されていない `ColId`、`Aggregate` が残った、`Scalar` の出力が 2 列など） | `XX000` | `Error::internal`（メッセージに ID とノード名） | どこでも |

- `54001` の SQLSTATE 定数 `STATEMENT_TOO_COMPLEX = "54001"` を `error.rs` の `sqlstate` に追記する（00 §15.3 の「追記だけは持ち主以外が行ってよい」に従う。00 への変更提案 9）。
- 計画のエラーでは、トランザクションは Failed になる（実行前でも通常のエラー。M3 の規則）。

---

## 11. テスト

置き場所は 00 §18 のとおり: `yuzhu-core/tests/plan_golden/`（スナップショット）、各モジュールの `#[cfg(test)] mod tests`（ルール単体）、`tests/slt/m4/plan_variants/`（K。結果の比較）、`tests/slt/m4/explain/`（E1。`onlyif yuzhu`）。

### 11.1 plan_golden（スナップショットテスト）

**目的**: 「SQL → build 直後 → 各ルールの後 → 物理」をテキストで固定し、ルールの効果とプランの形の退行を見つける。期待値は PostgreSQL ではなく yuzhu の設計どおり（レビューで確かめる）。

```
yuzhu-core/tests/plan_golden/
├── main.rs            runner。cases/*.golden を読み、実行して比べる。UPDATE_GOLDEN=1 で書き換える
├── fixture.rs         -- schema の DDL（CREATE TABLE / CREATE [UNIQUE] INDEX）から TableDef / IndexDef を作る（実パーサ + 解析。ストレージなし）。nblocks は -- nblocks の値を返す TableStore の偽物
└── cases/*.golden
```

**ケースファイルの形式**（1 ファイルに複数ケース。`-- case <名前>` で区切る）:

```
-- case join_filter_pushdown
-- schema
create table t(a int4, b int4, c text);
create table u(a int4, d int4, e text);
-- nblocks
t = 10
u = 100
-- settings
enable_hashjoin = on
-- sql
select t.c from t join u on t.a = u.a where u.d = 3
-- build
Project [#3:t.c]
  Filter (#5:u.d = 3)
    Join Inner on (#1:t.a = #4:u.a)
      Get t [#1:t.a #2:t.b #3:t.c]
      Get u [#4:u.a #5:u.d #6:u.e]
-- const_fold
(unchanged)
-- sublink
(unchanged)
-- subquery_pullup
(unchanged)
-- outer_join
(unchanged)
-- pushdown
Project [#3:t.c]
  Join Inner on (#1:t.a = #4:u.a)
    Get t [#1:t.a #2:t.b #3:t.c]
    Filter (#5:u.d = 3)
      Get u [#4:u.a #5:u.d #6:u.e]
-- join_keys
(unchanged)
-- join_order
(unchanged)
-- prune
Project [#3:t.c]
  Join Inner on (#1:t.a = #4:u.a)
    Project [#1:t.a #3:t.c]
      Get t [#1:t.a #2:t.b #3:t.c]
    Project [#4:u.a]
      Filter (#5:u.d = 3)
        Get u [#4:u.a #5:u.d #6:u.e]
-- physical
Project [@1]
  HashJoin Inner build=right keys=[(@0 = @0)] residual=-
    Project [@0 @2]
      SeqScan t filter=-
    Project [@0]
      SeqScan u filter=(@1 = 3)
```

- `-- sql` は 1 つの文。`-- settings` は省略可（`enable_*` と `yuzhu.*` を `PlannerSettings` に反映）。
- 各段階は、**前の段階から変わらなければ `(unchanged)`**。
- 論理プランの印字（`planner/print.rs`、L1）: 1 ノード 1 行、子は 2 スペース下げる。列は `#<ColId>:<修飾名>`（`ColumnInfo` の `qualifier.name`。修飾なしは `name`）。`Project` はパススルーを `#id:名前`、それ以外を `#id := 式` で並べる。`Aggregate group=[…] aggs=[#7 := count(*)]`、`Join <種類> on (…)`、`Distinct`、`Sort [#7 DESC NULLS FIRST]`、`Limit limit=… offset=…`、`SetOp Union all=false cols=[…] left=[…] right=[…]`、`Values rows=N`、`Result one_time_filter=…`、`Empty`、`CteScan x [#…]`。式の中のサブクエリは、式の文字列に `{SubLink Exists}` のように書き、そのノードの子として `~ ` で始まる行（サブクエリの plan を 1 段下げて）で続ける。
- 物理プランの印字（`planner/print.rs`）: ノード名と主なフィールド（`SeqScan <表> filter=…`、`IndexScan <表> using <索引> keys=[eq: …; lower: …; upper: …] dir=fwd|bwd filter=…`、`HashJoin <種類> build=left|right keys=[(…)] residual=…`、`NestedLoopJoin` / `NestedLoopParam params=[$0 := @1]`、`HashAggregate keys=[…] aggs=[…]` など）。式は `@<位置>`（`Local`）、`$<ParamId>`（`Param`）、`subplan#<id>`。`PhysicalQuery` の最後に `subplans:` と `ctes:` を続ける。
- 比べ方: 行ごとの完全一致。差があれば unified diff を出して失敗。`UPDATE_GOLDEN=1` で期待値を更新（レビューで差分を確認する）。

**ケースの網羅**（`cases/` のファイル。各ファイルは下の観点を 1 ケース以上持つ）:

| ファイル | 観点 |
|---|---|
| `scan.golden` | Seq Scan、等値・範囲・IS NULL のインデックス選択、複数インデックスの選び分け（スコアと OID）、述語を再評価する `filter`、`enable_seqscan` / `enable_indexscan` |
| `const_fold.golden` | §11.2 R1 の表 |
| `join.golden` | 内部・外部・USING・NATURAL・FULL・RIGHT、ビルド側の選択（`nblocks` を変えた 2 ケース）、Nested Loop + Materialize、`enable_hashjoin` / `enable_nestloop`、内側 Index Scan |
| `join_order.golden` | 直積を避ける並べ替え（チェーン・スター・直積しかない）、外部結合をまたぐ島 |
| `sublink.golden` | Semi / Anti、相関 IN、`NOT IN`、OR の下、ON の中、入れ子 |
| `pullup.golden` | 派生表の展開、集約を含む派生表、外部結合の NULL 側 |
| `outer_join.golden` | Left / Full の簡約 |
| `pushdown.golden` | 結合の種類ごと、HAVING、派生表、集合演算 |
| `agg.golden` | HashAggregate / GroupAggregate / Aggregate、従属列、DISTINCT、DISTINCT ON、HAVING |
| `setop.golden` | Union / Union ALL / Intersect / Except、平坦化 |
| `cte.golden` | インライン、共有（複数参照・MATERIALIZED）、NOT MATERIALIZED で複数参照 |
| `dml.golden` | INSERT … SELECT、UPDATE（FROM あり）、DELETE（USING あり）、インデックス走査の対象 |
| `explain_tree.golden` | 同じ問い合わせの `ExplainNode`（タイトル・詳細・子）をテキストにしたもの（`-- explain` 段）。`exec_id` つき |

### 11.2 ルール単体のテスト

プランを手で組み立てて（`planner/rules/testutil.rs` のヘルパ: `get(name, ncols)`、`join(kind, l, r, on)`、`filter`、`col(id)`、`lit`、`op("=", a, b)` など）、1 つのルールを適用して印字を比べる。**各ルールに、変換する例と変換してはいけない例の両方**を置く。

| ルール | ケース（変換する ○ / しない ×） |
|---|---|
| R1 const_fold | ○ `1 + 2` → 3、`'a' \|\| 'b'`、リテラルのキャスト（`'2020-01-01'::date`）、`false AND x` → false、`true OR 1/0 = 1` → true（エラーなし）、`coalesce(1, 1/0)` → 1、`case when true then 1 else 1/0 end` → 1、`a = null` → `Empty`、`a > 1 OR true` → フィルタ消滅、`not (a > 1)` → `a <= 1`、同じ述語の重複、`Filter(false)` → `Empty`、`Empty` の伝播（Inner / Left / Semi / Anti、group_by ありの Aggregate）、`Exists(Empty)` → false / `Any` → false / `All` → true。× `now()`・`random()`・`nextval()`・`current_user` は畳まない、`a + 1`（列）、group_by なしの `Aggregate(Empty)` は 1 行を残す、`LIMIT 0` は畳まない。エラー: `1/0` → `22012`、`false AND 1/0 = 1` はエラーなし、`a = 1 OR 1/0 = 1` は `22012`、`case when a > 0 then 1 else 1/0 end` は `22012`、`select 1/0 where false` は `22012` |
| R2 sublink | ○ 相関 EXISTS / NOT EXISTS、`LIMIT 1` つき EXISTS、`ORDER BY` / `DISTINCT` つき EXISTS、IN（相関なし・`LIMIT` つき・集約つき: `OpaqueAny`）、相関 IN（`EXISTS` の形）、`a < ANY (...)`、行値 IN、内部結合の ON の EXISTS、複数の EXISTS の連鎖、入れ子の EXISTS（内側が外側の列を参照 → 右側に積む / 左右の両方を参照 → 残る）、HAVING の EXISTS、UPDATE / DELETE の WHERE。× 相関なしの EXISTS（InitPlan のまま）、`NOT IN`、`ALL`、`OR` の下、LEFT JOIN の ON、集約を含む EXISTS（`select count(*) from u where u.a = t.a`）、**GROUP BY を持つ EXISTS（PostgreSQL 17 は変換する。検証済み。yuzhu は `from_tree` の根が `Aggregate` なので変換しない: §2.2）**、`LIMIT 5` つき EXISTS、`OFFSET` つき、volatile を含む WHERE、`from_tree` が外側の列を参照 |
| R3 subquery_pullup | ○ 単純な派生表、集約を含む派生表の `Project` の除去、入れ子の派生表、インライン CTE、`Project` の連鎖、INNER の NULL 側でない位置、NULL 側で `Column` だけ・`a + 1`（strict）。× volatile を含む式、`SubLink` を含む式、NULL 側で定数・`coalesce`・`case`、根の `Project`、親が `Sort` / `Limit` / `Distinct` / `SetOp`、**ID の再定義**（パススルーの `Project` が同じ ID を再定義した後ろの参照は置換しない: `select x from (select a+1 as x from t) s order by x` で `Sort` のキーが `Project` の出力 `#x` を指したまま） |
| R4 outer_join | ○ `left join … where u.d = 3`、`… where u.d is not null`、`… where u.d > 1 or u.d < 0`（Or は全項が拒否）、`not (u.d is null)`、`full join … where t.a = 1`（→ Left）、`where u.a = 1`（→ 入れ替えた Left）、両方 → Inner、入れ子の外部結合の連鎖（上の述語が下を簡約）。× `where u.d is null`、`where coalesce(u.d, 0) = 0`、`where u.d = 3 or u.d is null`、述語が `Project` / `Aggregate` をまたぐ位置、Anti の ON は使わない |
| R5 pushdown | ○ Inner の単一側・両側・定数だけの述語、Left の左への WHERE と右への ON（左だけの ON は残る）、Semi の左だけの ON、Anti の左だけの ON は残る、Aggregate の下へ（group key だけ）、集約の述語は残る、group_by なしの Aggregate は何も下ろさない、Distinct / Sort の下へ、Limit の下へは下ろさない、`DISTINCT ON` の下へ下ろさない、SetOp の両枝へ、Project をまたぐ置換、Volatile の述語は動かない、`SubLink` を含む述語、Semi / Anti の沈め込み。× Left の NULL 側へ WHERE、Full |
| R6 join_keys | ○ 左右の入れ替え（交換子）、型の違う等値（int4 = int8）、複数キー、キーと残りの並び、片側だけの式（`t.a + 1 = u.a`）。× `<`・`<>`、`IS NOT DISTINCT FROM`、両辺が同じ側、定数との比較、`oprcanhash` でない演算子、Volatile を含む辺、交換子がない演算子 |
| R7 join_order | §11.3 |
| R8 prune | ○ 結合の両側の `Get` に刈り込み `Project`、集約の使われない呼び出しの除去、`Aggregate` の group key は残す、ORDER BY の resjunk が `Sort` で必要、Distinct は全列、SetOp は全列、DML の必要な列。× 親が `Project` / `Aggregate` / `Filter` のときは足さない、全列が必要なら足さない |

### 11.3 結合順序のテスト（R7）

| ケース | 期待 |
|---|---|
| チェーン `a ⋈ b ⋈ c ⋈ d`（`a.x = b.x and b.y = c.y and c.z = d.z`）を構文順で / 逆順で / 入れ替えて書く | 直積なし。構文順の最初の葉から始まり、つながる葉を構文順で |
| スター（`f` と `d1..d4`、結合は `f.k_i = d_i.k`）で `from d1, d2, f, d3, d4` | `d1` から始まり、`d2` は直積になるので `f` が先（rank 0 = 等値でつながる）、続いて `d2`, `d3`, `d4` |
| 直積しかない（条件なし）3 表 | 構文順のまま（rank 2 の最初） |
| 等値でつながる葉と、`<` だけでつながる葉 | 等値の葉が先（rank 0 < 1） |
| 外部結合をまたぐ: `(a ⋈ b) left join c on … ⋈ d` | 外部結合は動かず、島は `(a ⋈ b)` と `d` を含む上の島で別々に並べる |
| 述語の置き場所: 3 表にまたがる述語 | 3 表がそろう最初の結合の `on` |
| 島の葉が 65 個 | 並べ替えない（構文順のまま）。`on` は正規化される |
| 同点（rank が同じ葉が複数） | 構文順の最初 |
| 結果の同値性 | 並べ替え前後で同じ行集合（実行して比べる。X の実装後） |
| 計画時間（§11.6） | 8 表のチェーン / スター / クリーク |

### 11.4 プラン変種テスト

PostgreSQL にも同名の設定があるので、**同じ slt を PostgreSQL と yuzhu の両方に流せる**（結果が同じことを確かめる）。

1. **slt**（K。`tests/slt/m4/plan_variants/*.slt`）: 1 つの問い合わせ群を「既定」「`enable_hashjoin = off`」「`enable_nestloop = off`」「両方 off」「`enable_indexscan = off`」「`enable_seqscan = off`」「`enable_hashagg = off`」「`enable_material = off`」「`enable_sort = off`」で流し、結果が同じ（`rowsort` か `ORDER BY` つき）。共通部分は生成スクリプトで作ってよい。問い合わせ群は、内部・左・右・FULL（等値）・SEMI（EXISTS / IN）・ANTI（NOT EXISTS）・集約（GROUP BY / HAVING / DISTINCT）・DISTINCT ON・集合演算・サブクエリ（スカラー・相関・NOT IN）・主キーと副インデックスを使う範囲検索・`ORDER BY ... LIMIT`（インデックス順）。
2. **単体**（`planner/mod.rs` の tests）: 同じ問い合わせ群を `PlannerSettings` の組（上の 9 + 全 on）で `plan` し、**期待する物理ノードの種類が含まれるか**を確かめる（`enable_hashjoin = off` の等値結合に `HashJoin` がない、両方 off の等値結合に `NestedLoopJoin`、`enable_hashagg = off` に `GroupAggregate` + `Sort`、`enable_indexscan = off` に `IndexScan` がない、`enable_seqscan = off` で候補があれば `IndexScan`、`enable_material = off` の `NestedLoopJoin` の内側に `Materialize` がない、FULL JOIN は設定に依らず `HashJoin`）。
3. **差分（ルールの on / off）**（単体。X が揃ったら有効化）: 小さなランダムなデータで、`rules::run_only` に空・全部・各ルールを 1 つずつ外したもの・各ルールだけを渡して作ったプランの実行結果が同じ（行の多重集合）。Z（差分ランダムテスト）は PostgreSQL との比較を受け持つ。

### 11.5 不変条件の検査（`planner/validate.rs`、L1 + L2）

`#[cfg(any(test, debug_assertions))]` で、`rules::run` の**各ルールの後**に論理プランを、`physicalize` の後に物理プランを検査し、違反は `Error::internal`（テストでは panic）。

- 論理: §5.10 の 1〜5 と、`Join` の左右の出力が互いに素、`Project` / `Aggregate` の出力に ID の重複がない、`Semi` / `Anti` は R2 より前に現れない、`LogicalQuery.output` が根の出力に含まれる、**自由な列**（plan の中のどのノードの出力にも定義されない ID）は、`q.plan` と `q.ctes[i].plan` の根には**ない**（最上位では全部定義される）、式の中の `SubLink` の plan にだけある。R3 の置換の後に、置き換えられた ID が参照として残っていない（上位の再定義を除く）。
- 物理: `PhysCol::Local(i)` が評価する行の幅の範囲内、`Param(p)` の `p < n_params`、`SubLink.query < subplans.len()`、`CteScan.cte < ctes.len()`、`HashJoin` の `left_keys.len() == right_keys.len() == key_types.len()` と各キーの型が `key_types` と同じ、`Update.input` の出力幅が `n_user_cols + 1 + assigned.len()`、`Unique.key_cols` が入力の幅の範囲内、`Insert.input` の幅が `column_map` の参照の最大 + 1 以上、`PhysicalQuery.output` の幅が `root` の出力と同じ（SELECT）。
- `FnKind::Runtime` の関数が `expr_volatility` で Volatile になること（`catalog::builtin` の表を全件走査するテスト）。

### 11.6 計画時間

- 8 表の内部結合（チェーン・スター・クリーク）で `plan` が **release で 50 ms、debug で 1 秒**以内（`#[test]` で測り、超えたら失敗。CI の機械の揺れを見て閾値は調整してよい）。
- 16 表のチェーンが 1 秒以内（debug）で、結果が構文順より悪くならない（直積なし）。
- 述語 1,000 個の `AND` / `IN` リストでスタックが溢れない・時間が線形に近い。
- 入れ子の深さ（サブクエリを 600 段、`WHERE` に 600 段の入れ子の式）で `54001` を返し、panic しない。

### 11.7 PostgreSQL と一致を確かめる EXPLAIN

D-20 の「書式は PostgreSQL と同じ」を守るため、**プランの選び方に依らず同じになる問い合わせ**（結合なし・インデックスなし・単一表）だけは、同じ `EXPLAIN (COSTS OFF)` を PostgreSQL にも流して一致を確かめる（`tests/slt/m4/explain/format.slt`、K と E1。`onlyif yuzhu` を付けない行）: `Seq Scan` + `Filter`（定数畳み込み後）、`where false` / `where a = null` の `Result` + `One-Time Filter: false`、`select 1` の `Result`、`values` の `Values Scan on "*VALUES*"`、`Sort` + `Sort Key`（DESC・NULLS FIRST・LAST）、`HashAggregate` + `Group Key` + `Filter`（HAVING）、`Aggregate`、`Limit`、`Append`（UNION ALL）、`HashAggregate`（UNION）、`HashSetOp Intersect` / `Except All`（検証済みの構造。`Subquery Scan` が入る点だけ違うので行を除いて比べる）、`Unique` + `Sort`（DISTINCT ON）、`CTE Scan` + `CTE x`、`InitPlan 1`（非相関のスカラー）、`SubPlan 1`（相関のスカラー）。

---

## 12. 実装の分担と工数（L1・L2）

00 §17 のとおり、L1 と L2 は P0 の後に並列に始める。**境界**: `planner/logical.rs` と `planner/physical.rs` の型は A が置く。L1 は `build.rs`、`rules/*`、`util.rs`、`print.rs`（論理プランの印字）、`validate.rs`（論理の部分）、`mod.rs` の `plan` / `PlanTrace`。L2 は `physicalize.rs`、`index_select.rs`、`size.rs`、`explain_tree.rs`、`print.rs` の物理プランの印字、`validate.rs` の物理の部分。`util.rs` は L1 の持ち物で、L2 が足してほしい関数は L1 に依頼する（`split_on`・`coerce_expr` は L1 が最初の日に置く）。

| 担当 | 作業 | 必須 / 任意 | 日数 |
|---|---|---|---|
| **L1** | `util.rs`（`expr_refs`、`substitute`、`expr_eq`、`plan_output`、`plan_free_cols`、揮発性、`nulls_out`、`coerce_expr`、`split_on`、走査の部品） | 必須 | 1.0 |
| | `build.rs`: 葉・JOIN・集約・SELECT の残り・集合演算・CTE・SubLink | 必須 | 2.0 |
| | `build.rs`: INSERT / UPDATE / DELETE（`UPDATE ... FROM`）、システム列の事前走査 | 必須 | 0.5 |
| | R1 const_fold（`eval_const` との接続、畳み込みの規則、`Empty` の伝播） | 必須 | 0.7 |
| | R2 sublink（`try_pull`、形の解析、入れ子） | 必須 | 0.8 |
| | R3 pullup、R4 outer_join、R5 pushdown | 必須 | 0.9 |
| | R6 join_keys、R7 join_order、R8 prune | 必須 | 0.6 |
| | `print.rs`（論理）、`validate.rs`（論理）、ルール単体のテスト、plan_golden の runner・fixture | 必須 | 1.0 |
| | R5 の任意拡張（定数の推移） | 任意 | 0.3 |
| | L1 の小計 | | 約 6.5（00 は 6） |
| **L2** | `physicalize.rs` の骨格（`phys`、`lower`、`finish_root`、`ensure_layout`、Project の省略）、走査以外の単純なノード、DML | 必須 | 1.2 |
| | `index_select.rs`（候補の抽出、`IndexPick`、スコア、`enable_*`）、`size.rs` | 必須 | 1.0 |
| | 結合（Hash・Nested Loop・Materialize・FULL のエラー・ビルド側）、集約、DISTINCT ON、集合演算 | 必須 | 1.3 |
| | SubPlan の分類（InitOnce / Rescan / Hashed）、`ParamId`、CteScan | 必須 | 0.8 |
| | `explain_tree.rs`（メモ、`assemble`、`ExplainNames`、タイトル・詳細） | 必須 | 1.0 |
| | 内側 Index Scan の Nested Loop（`try_inl`） | 任意（後半） | 0.5 |
| | ソートの省略（`order_satisfied`、(a)(b)） | 任意（S） | 0.4 |
| | `print.rs`（物理）、`validate.rs`（物理）、plan_golden の物理側ケース | 必須 | 0.6 |
| | L2 の小計 | | 約 6.8（00 は 5） |

- L2 の必須分は約 5.9 日で、00 の 5 日に近い。任意の 2 項目（`try_inl` と `sort` 省略）は、遅れたら M4 の後半に回す。`try_inl` を後回しにしても、結合は Hash か Nested Loop（Materialize）で結果は同じ。
- L2 の `index_select` は B2 の `catalog::opclass`（`operator_strategy`、`comparator`）が要る。B2 の最初の成果物（`OPFAMILIES` / `AMOPS` と `operator_strategy`）が出るまでは、L2 は Seq Scan と結合・集約・SubPlan・DML を進め、最後にインデックス選択を入れる（00 §17 の進め方 (4)）。
- E1（`explain/*`、`deparse/*`）との接点: `ExplainNames::expr` が呼ぶ deparse の関数の形（§3.8）、`ExplainNode.exec_id` と `width`、`NodeNote` と `assemble` の出力。E1 の最初の日にこの章の §3.8・§8 を共有する。
- X1〜X3（executor）との接点: `PhysicalPlan` の各ノードの意味（00 §9.2 のコメント）、§7.3 の「`Eq` の値が NULL なら 0 行」、§7.7 の `SubPlanDef.test` の評価の文脈、`NestedLoopParam` の `params`、`Hashed` の `probe_keys` / `build_keys`、`plan_id` の数え方（§8.1）。

---

## 13. 未検証の点（実装前に確かめるもの）

- `EvalCtx::for_constant_folding(catalog, type_env)` を 05 が提供できること。`EvalCtx` に `type_env` を足すこと（00 は `ExecCtx.type_env` だけ。§6.1.1、00 への変更提案 3）。
- `catalog::builtin::operator_by_oid` の有無（なければ追加。`OPERATORS` の線形探索でよい）。`OperatorMeta.com` が M4 の新しい演算子（numeric・日時・bpchar）の行にも入っていること。`proc_by_oid(operator_meta(oid).proc_oid)` で全演算子の `provolatile` が引けること。
- `catalog::opclass::operator_strategy(op, family)` が**交差型**（`int4 < int8`、`date < timestamp`、`text = name`）で `Some((strategy, left, right))` を返すこと（06）。`lt` が列の側の型、`rt` が値の側の型であること。
- アナライザが `a BETWEEN x AND y` を `a >= x AND a <= y` にして Bound に渡すこと、`IN (リスト)` が `InList` で来ること（§4.1）。`Var.levels_up` と `CteRef.levels_up` の数え方（§4.1）。
- 05 の `IndexScan` が、**`Eq` の値が NULL なら 0 行**を返すこと、UPDATE / DELETE の対象表の走査で自分が書いた新しい版を見ないこと（コマンド ID。M2 の Halloween 対策が IndexScan でも効くこと）、`keys` が全部空（`eq` も範囲もなし）のとき全インデックス走査になること（§7.4）。
- 06 の `begin_scan` が、下限だけの範囲で NULL の項目を返さない（または返してもよいが、`filter` で落ちる）こと。後ろ向きスキャンが `(descending, nulls_first)` を反転した順に返すこと。
- `PhysicalPlan::Project` が `Result` の直上に併合された形（`Result { exprs, one_time_filter }`）を 05 が受け付けること（M1 の `Result { exprs }` と同じ）。
- 05 の `HashJoin` が `kind == Left` かつ `build_is_left` の処理（保存側がビルド側）を実装すること。`Semi` / `Anti` の `build_is_left = false` 固定を前提にしてよいこと。
- `Hashed` の `SubPlanDef.test` が `probe_keys` / `build_keys` と矛盾しないこと（キー型へのキャストを `probe_keys` / `build_keys` が持つので `test` は NULL 判定のための元の式）。
- PostgreSQL の挙動で未確認のもの:
  - `DISTINCT ON` で `ORDER BY` がないときの並び（ON の式の昇順であること。検証済みのプランは `Unique` + 整列だが、NULL の位置は未確認）。
  - `Hash Right Join` が出る条件（左が小さいときに PostgreSQL がどちらを選ぶか。yuzhu は `wl < wr`）。検証済みなのは名前の形（`Hash Right Join`、probe が最初の子）だけ。
  - EXPLAIN の `Index Cond` の複数述語の並び（元の WHERE の順か索引の列の順か）。
  - 同じ表を 2 度使うときの別名（`t_1`）の付け方の一般則（M4 では出さない）。
  - `Aggregate` の `Filter` と `HashAggregate` の `Filter` の位置（検証済み: 詳細行の末尾）。

---

## 14. 確認事項

ユーザーの不在中に仮決めしたことです。仮決めのままでよいか確認してください。ID は `[04-Q<n>]`（`11-tests-plan.md` が `M4-Q<n>` に振り直す）。ディスク形式に関わるもの（★）はこの章にはありません。

- **[04-Q1] ColId をパススルーで引き継ぐ**（04-D1）。
  - 仮決め: `Get` の列 → `Project` の単純な列参照 → `Aggregate` の単純な列の group key は同じ `ColId`。計算列だけ新しい ID。
  - 理由: 述語の押し下げ・派生表の展開・Semi 化・相関参照が ID の付け替えなしでできる。
  - 変えたい場合の影響: 「ノードごとに新しい ID」にすると R3 が置換表を木全体に持ち回る必要があり、相関サブクエリの外側の列の参照が `Project` / `Aggregate` をまたぐたびに写像が要る。L1・L2 の大半に影響（build・R3・R5・物理化）。
- **[04-Q2] 相関のある `IN` を `EXISTS` の形で結合化する**（04-D5）。
  - 仮決め: PostgreSQL 17 は相関 `IN` も Semi 結合にする（検証済み）。yuzhu は LATERAL を持たないので、`EXISTS` の形に直せるサブクエリ（集約・LIMIT・集合演算なし）だけを変換し、他は `Rescan` の SubPlan。
  - 変えたい場合の影響: 変換しないなら `try_pull` の相関あり `Any` の分岐を消すだけ（結果は同じ。性能のみ）。
- **[04-Q3] 左結合の ON の中の `EXISTS` を引き上げない**。
  - 仮決め: 内部結合の ON と WHERE のみ。左結合の ON の `EXISTS` は SubPlan のまま（PostgreSQL は右側に積む）。
  - 変えたい場合の影響: R2 の `try_pull` の `sides` に「右の子」を足す。R5 の Left の `on` 規則と合わせる。+0.3 日。
- **[04-Q4] 内側 Index Scan の Nested Loop の採否の定数**（`ROWS_PER_BLOCK_EST = 100`、`probe_cost = 3 / 6 / 12`）。
  - 仮決め: §7.5.3 の式。外側が（絞り込まれて）小さく、内側が大きいときだけ選ぶ。
  - 理由: 統計がないので、`nblocks` と述語の個数だけで決める。pgbench の 3 つの更新・1 つの SELECT は単一表で、結合の性能には影響しない。
  - 変えたい場合の影響: 定数だけ。結果は変わらない。
- **[04-Q5] ビルド側は `estimate` の小さいほう、同点は右**。
  - 仮決め: `estimate`（`nblocks` と述語の個数の係数）で決める。
  - 変えたい場合の影響: 定数・規則だけ。結果は変わらない。
- **[04-Q6] ソートの省略は条件つき**（M4 後半・任意）。
  - 仮決め: 述語で選んだインデックスの順序で足りるとき、`Sort` の直上が `Limit` のとき、`enable_sort = off` のときだけ。「ソートを省くためだけの全インデックス走査」は `Limit` なしでは行わない（ヒープへのランダムアクセスが増えて遅くなりうる）。
  - 変えたい場合の影響: §7.4 の (b) の条件。結果は変わらない。
- **[04-Q7] InitPlan / SubPlan の EXPLAIN の位置と番号**（レビュー対応 R-06 で 10 に合わせた）。
  - 仮決め: InitPlan は PostgreSQL と同じくその問い合わせ階層の根の `ExplainNode` の子、SubPlan は式を表示したノードの子。番号は物理化した順（子が先。PostgreSQL は内側が先で、CTE と番号を共有するので一致しない）。
  - 変えたい場合の影響: `assemble` の置き場所と採番だけ。`onlyif yuzhu` のテストの期待値（`SubPlan` の番号の差は既知の差）。
- **[04-Q8] `ExplainNode` を表示用の木にし、`exec_id`（と `width`）を足す**（00 への変更提案 1。11 §7.1 の C-1 で `plan_id` から `exec_id` に統一）。
  - 仮決め: PostgreSQL の見た目（`Hash` の合成ノード、`Filter` の併合、`Project` の透過）を作るため、`ExplainNode` を `PhysicalPlan` と同形にしない。計測との対応は `exec_id`。
  - 変えたい場合の影響: 00 が採らなければ、`ExplainNode` を `PhysicalPlan` と同形にし（`Hash` なし、`Filter` と `Project` を独立のノードで出す）、EXPLAIN の見た目が PostgreSQL から少し離れる（`onlyif yuzhu` のテストの期待値だけの問題）。
- **[04-Q9] リテラルのキャストを（Stable でも）計画時に畳む**。
  - 仮決め: `'2020-01-01'::date` や `'t'::regclass` は DateStyle・TimeZone・カタログが文の間は固定なので畳む（PostgreSQL も、パース時に入力関数を評価する）。
  - 変えたい場合の影響: M5 の Extended Query でプランをキャッシュするとき、設定が変わったら再計画する必要がある（04-Q10 と同じ根拠）。畳まない場合はキャストの式が残り、インデックス選択の「定数式」の判定を Stable に広げる必要がある（既に広げてある）。
- **[04-Q10] プランをキャッシュしない**。M4 は文ごとに計画する。M5 の Extended Query で Parse / Bind をまたいでプランを使い回すなら、`TypeEnv` の設定・カタログの版（インデックスの追加）が変わったときに再計画する仕組みを足す。
- **[04-Q11] 外側の列を参照する CTE: `MATERIALIZED` の明示と揮発性は `0A000`、それ以外の共有はインライン**（04-D4。レビュー対応 R-05 で 02-Q5・05-Q5 と一致させた）。
  - 仮決め: PostgreSQL は相関 CTE も共有できるが、M4 は executor の `CteScan` の再実行を持たない。共有が意味を持つ（1 回だけ評価する）場合だけ `0A000`、結果が同じになる場合（非 `MATERIALIZED` かつ非揮発）はインラインで通す。
  - 変えたい場合の影響: 共有を許すなら 05 の `CteScan` が `rewind` で作り直すこと、`uses_params()` に CTE を含めることが要る。
- **[04-Q12] 計画の再帰の深さの上限 `MAX_PLAN_DEPTH = 500`**（`54001`）。
  - 仮決め: 接続スレッドのスタックで安全な値として。PostgreSQL の `max_stack_depth` に相当する。
  - 変えたい場合の影響: 定数だけ。スタックサイズ（J）と合わせる。
- **[04-Q13] 従属列は group key に足す**（§5.4）。
  - 仮決め: 主キーへの関数従属で許された列は、`Aggregate` の group key に足して出力に通す（PostgreSQL は最初の行の値を返す）。
  - 変えたい場合の影響: 03 が許す範囲（主キーの全列がグループ化されているときの同じ表の列）が広がったら、この処理が自然に追従する。
- **[04-Q14] `EXISTS` を `ANY` に直してハッシュ化しない**。
  - 仮決め: 相関 `EXISTS` が引き上げられないとき（`OR` の下など）は `Rescan` の SubPlan。PostgreSQL は等値の相関ならハッシュ化 SubPlan にする（検証済み）。
  - 変えたい場合の影響: `plan_sublink` に「`Exists` の相関が等値の連言なら `Any` + `Hashed` に変換」を足す（+0.5 日）。結果は同じ。

---

## 15. 00 への変更提案

統合時に反映してほしいもの。番号は本文の参照番号。

1. **`ExplainNode` に `exec_id: usize` と `width: u32` を足す**（C-1 で `plan_id` から変更）。`exec_id` は §8.1（物理プランの先行順の通し番号。合成ノードは中身の番号を借りる）。`width` は `COSTS` の `width=N`（そのノードの出力列の型の幅の和。`explain::type_width(&SqlType)` の表は 10 が決める）。`PlanEnv` に `explain_verbose: bool` を足す（§3.1）。`PlanEnv` に `#[derive(Clone, Copy)]`。00 §9.3 の「PhysicalPlan と同じ形・同じ子の順序の木」を「`exec_id` で物理プランのノードと対応づけられる木」に直す。
2. **`ExplainNode` の子の順序**: `children` は印字する順（`InitPlan` / `CTE` が先、`SubPlan` が後）。
3. **定数畳み込みのために `executor::eval` を計画から呼ぶ**。`planner::rules::const_fold` が `executor::eval::eval_const` を使う（00 §4.1 の依存の向きの例外。1 か所だけ）。05 は `EvalCtx::for_constant_folding(catalog: &dyn CatalogReader, type_env: &TypeEnv) -> EvalCtx<'_>`（`session` はダミー、`runtime` は `NullRuntime`）と、`EvalCtx` への `type_env` の追加を提供する。
4. **ファイルの追加**: `planner/util.rs`、`planner/size.rs`、`planner/print.rs`、`planner/validate.rs`、`planner/rules/testutil.rs`（`#[cfg(test)]`）。持ち主は L1（util・print の論理・validate の論理）と L2（size・print の物理・validate の物理）。`yuzhu-core/tests/plan_golden/` を L1・L2 の共同にする。
5. **`LogicalCte.inline` は常に `false`**（インラインする CTE は `ctes` に入れない。04-D4）。`LogicalQuery.n_subplans_hint` は物理化の `subplans` の容量の見積もりだけ。
6. **`SubPlanDef.test` と `SubLink` ノード**: 物理の式の `SubLink { kind, test, query }` は `test = None` とし、`Any` / `All` の比較式は `SubPlanDef.test` に置く（§7.7.1）。`SubPlanDef.test` の `Local` は評価する側の行の位置、`SubLinkOutput(i)` は内側の現在の行。
7. **deparse（10）の API の形**: 列の表示と SubLink の表記をコールバックで受け取る（`pg_get_expr` は Var → 列名、EXPLAIN は `ExplainNames`）。例: `deparse::expr<C, Q>(e: &Expr<C, Q>, cb: &DeparseCallbacks<'_, C, Q>) -> String`。この章はコールバックの実体（`ExplainNames`）だけを持つ。
8. **`catalog::builtin::operator_by_oid(oid: Oid) -> Option<&'static BuiltinOperator>`** を足す（交換子の引き当て。§6.6、§7.3）。
9. **SQLSTATE `STATEMENT_TOO_COMPLEX = "54001"`** を `error.rs` の `sqlstate` に足す（§10）。
10. **アナライザの前提の明文化**（03 と合わせる。§4.1）: `levels_up` の数え方、`CteRef.levels_up` の数え方、`BETWEEN` の分解、`BoundInsert.overriding` の反映済み、集合演算の ORDER BY / LIMIT が列を参照しない。
11. **`PhysicalPlan::Result` の意味**: `exprs` を 1 行出し、`one_time_filter` が false（または NULL）なら 0 行（00 §9.2 のとおりだが、`Empty` もこの形になることを 05 に伝える）。
