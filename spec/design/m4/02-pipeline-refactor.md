# yuzhu M4 パイプラインの作り直し（02-pipeline-refactor）

M4 の最初の作業（QUESTIONS.md の Q-013）です。JOIN・副問い合わせ・集約を載せるために、M1〜M3 の「Bound から直接 PhysicalPlan を作る」「列は入力行の位置（`ColumnRef { index }`）」「`eval` は `&ExecCtx`」「Executor に `rewind` がない」を作り直します。この章は `00-contracts.md` の §5〜§10（式・解析結果・論理プラン・物理プラン・executor）の持ち主です。**署名は 00 のとおり変えず**、その**意味・不変条件・検証・移行手順**を決めます。

- 要件定義: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- 必読（読んだもの）: `00-contracts.md`（全部）、`spec/design/m1.md` §5.2・§5.3、`m1-changes.md`、`m2.md` §4.7・§5.2・§5.5、`m3.md` §4.9、`spec/research/m4-query.md` §1・§3・§4・§6、現在の実装（`impl/rust/crates/yuzhu-core/src/{analyzer,planner,executor,session.rs}`。M2・M3 が並行して進行中なので読むだけ）
- 担当: A（型の追加）、P0（移行）。00 §17 のとおり。
- PostgreSQL の確認は 17.11（`sandbox/pg.sh start` の `127.0.0.1:55432`）。実機で確かめたものには「（確認済み）」、ソースの記憶や推論によるものには「（未検証）」と書く。
- 用語は 00 §1.3 に従う。他章への参照は章のファイル名と概念名で書く（`03-parser-analyzer.md` のスコープ解決、`04-planner-optimizer.md` の述語の押し下げ、など）。

---

## 1. 範囲と動機

### 1.1 範囲

| この章が決めること | 他章が決めること |
|---|---|
| `Expr<C, Q>` の意味（`walk` / `try_map` の厳密な規則、層ごとに現れてよい変種、検証） | 各式の型付けとエラー（`03-parser-analyzer.md`）、関数・演算子（`09-types-functions.md`） |
| Bound の構造（`BoundQuery` / `BoundSelect` / `Rte` / `FromItem` / `JoinColSource`）の意味と評価順序、`Var` と `levels_up` の規則 | 名前解決・集約の検査・GROUP BY の規則（`03-parser-analyzer.md`） |
| 論理プランの出力列、`ColId` の発行・再利用・命名の規則、`ColumnArena` | Bound から論理プランへの変換の手順、ルール（`04-planner-optimizer.md`） |
| 物理プランの出力レイアウト・幅・式の文脈行、`SubPlanDef` と `ParamId` の割り当て、`ExplainNode` の形 | 物理化のアルゴリズム、インデックス選択（`04`）、EXPLAIN の整形（`10-explain-copy-compat.md`） |
| `Executor` の契約（`next` / `rewind`）、`ExecCtx`、SubPlan の状態遷移、`check_interrupts` の規則 | 結合・集約・ハッシュ表などノードの中身（`05-executor.md`） |
| session から executor までの呼び出し順、INSERT / UPDATE / DELETE の変換、CHECK / DEFAULT / RETURNING の変換 | COPY・DDL の呼び出し（`10`、`07`） |
| 検証関数、段階的な移行手順、★ スタブ一式 | — |

### 1.2 動機: `ColumnRef { index }` が成り立たない理由

M1〜M3 の `BoundExprKind::ColumnRef { index }` は「評価中の 1 本の入力行の何番目か」です。FROM が 1 表なら入力行は表の行そのものなので成り立ちました。M4 では次の理由で成り立ちません。

| # | 場面 | `index` で破綻する理由 |
|---|---|---|
| 1 | JOIN | 入力行は「左の行 ++ 右の行」。`a JOIN b JOIN c` を `(a JOIN c) JOIN b` に並べ替える（`04` の結合順序）と、`b` の列の `index` がすべてずれる。`u.c > 5` を `u` の走査へ押し下げると、同じ式の `index` が「結合後の 3 番目」から「`u` の 1 番目」に変わる。列の刈り込みでも変わる |
| 2 | 相関副問い合わせ | 副問い合わせの式は外側の行の列を参照する。`index` は「どの行の」を表せない（M1 の `eval(expr, row)` は行が 1 本だけ） |
| 3 | 自己結合 | `t JOIN t` の 2 つの `t` を区別するために、アナライザが結合の組み方（左深い木など）に依存した `index` を振ることになり、プランナの並べ替えと衝突する |
| 4 | USING / NATURAL / FULL JOIN | 併合列 `COALESCE(l.a, r.a)` は、元のどの列でもない |
| 5 | UPDATE ... FROM / DELETE ... USING | 代入式が FROM の列を参照する。M2 の `Update` ノードが `assignments` を旧行 1 本に対して評価する形では書けない |
| 6 | 実行側 | `eval` が `&ExecCtx`（共有参照）なので、式の評価中に子 Executor を動かせない（SubPlan）。Executor に `rewind` がなく、NLJ の内側と相関副問い合わせを再実行できない。`PhysicalPlan` が `BoundExpr` を持つので実行時パラメータ（`Param`）を表せない |

M1〜M3 の構造と M4 の構造の対応:

| 項目 | M1〜M3 | M4（00 §5〜§10） |
|---|---|---|
| 列参照 | `ColumnRef { index }`（入力行の位置） | Bound: `Var`（`rte` `col` `levels_up`）、論理: `ColId`（文の中で一意）、物理: `PhysCol`（`Local(位置)` か `Param`） |
| 解析結果 | `BoundSelect`（FROM は 1 表か VALUES） | `BoundQuery`（`ctes` + 本体 + ORDER BY / LIMIT）。範囲表 `Rte` と結合の木 `FromItem` |
| プラン | Bound → `PhysicalPlan`（直接） | Bound → `LogicalPlan` → ルール → `PhysicalPlan` |
| 式 | `BoundExpr` 1 種（M1 の struct + enum） | `Expr<C, Q>` 1 定義 + 3 つの型別名（`BoundExpr` `LExpr` `PhysExpr`） |
| UPDATE | `Update` ノードが `assignments` を評価する | 入力の `Project` が新しい値を計算し、`Update` ノードは位置だけを見る |
| executor | `next` だけ。`eval(&ExecCtx)` | `next` + `rewind`。`eval(&mut ExecCtx)`。`Param` と SubPlan |

### 1.3 非目標

- 最適化ルールの中身、結合順序、インデックス選択（`04`）。この章は、ルールが守るべき不変条件と、ルールが使う走査 API だけを置く。
- 結合・集約・ハッシュ表などノードの実装（`05`）。この章は `next` / `rewind` の契約だけを置く。
- LATERAL。`Var.levels_up` と `RteKind` はその表現を妨げないが、M4 は LATERAL を `0A000` にする（`03`）。
- `WITH RECURSIVE`、ウィンドウ関数、prepared statement。`PhysicalQuery` を不変にする方針（M5 で再利用できる）だけは守る。
- 式のコンパイル（クロージャ化）などの性能最適化。評価は木の再帰のまま（M1 と同じ）。

---

## 2. 決定

### 2.1 D-1: 処理系列と式の型（案 A / B / C と、「`Expr<C, Q>` 1 定義」案の比較）

`m4-query.md` §4.1 は、列参照を論理プランでは `ColId`、物理プランでは位置にする案 B を推奨した。そこでは `LExpr` と `PhysExpr` が別の enum になる。00 は式の定義を 1 つにまとめた（案 D）。比較:

| 観点 | A: 論理と物理を分けない | B: 分ける（式は層ごとに別の型）`m4-query` の推奨 | C: PostgreSQL 方式（Path を列挙して選ぶ） | **D: B + 式は `Expr<C, Q>` 1 定義（採用）** |
|---|---|---|---|---|
| 式の変種の定義 | 1 | 3（Bound・LExpr・PhysExpr） | 3 | 1 |
| 式を扱うコード（`walk`、等価判定、定数畳み込み、deparse、`eval`）の組数 | 1 | 3（変種を足すたびに 3 か所） | 3 | 1（C と Q の違いは葉の 3 変種だけ） |
| 並べ替え・押し下げ・刈り込み | 位置の付け直しが要る。バグの温床 | `ColId` なので位置は動かない | 同 B | 同 B |
| 層の変換 | なし | 手書きの変換関数 3 つ | | `try_map` 1 つ（葉だけを書く） |
| 層ごとに「現れてよい変種」の保証 | 不要 | 型で保証 | 型で保証 | **型では保証できない**。`validate`（§3.3）で保証する |
| M6 のコストベース最適化 | プラン全体の書き直し | 論理から複数の物理候補を作る段を足す | 直結 | B と同じ |
| 相関副問い合わせの書き換え（EXISTS → セミ結合） | — | 外側の列を別変種（`OuterColumn`）にすると、書き換えのたびに式を作り直す | | `Column(ColId)` に統一するので、式はそのまま、サブプランの置き場所だけを変える |
| 追加の工数（案 A に対して） | 0 | +2〜3 日 | +10 日以上 | B より約 1 日少ない（式のコードが 1 組） |
| 判定 | × | △ | ×（M4 には過剰） | ○ |

D の欠点と緩和:

1. 型パラメータでシグネチャが長くなる。型別名（`BoundExpr` `LExpr` `PhysExpr`）で隠す。
2. `Clone` / `Debug` の境界が `C` と `Q` に要る。`Q` に入る `Box<BoundQuery>` / `Box<LogicalSubquery>` / `SubPlanId` はすべて `Clone + Debug`（`BoundQuery` は `Arc<TableDef>` を持つが `Arc` は `Clone`）。
3. 層に現れない変種（たとえば物理の `Aggregate`）が型に残る。§3.2 の不変条件表と `validate` で保証し、00 §4.3 の規約 1（新しい変種は 1 回だけ足し、3 つの層の取り扱いを表に書く）で運用する。
4. 外側の列を `Column(ColId)` に統一したので、「この `ColId` は外側の列か」は式の形から局所的には分からない。`LogicalPlan::outer_refs` と `validate`（L2）で判定する。

**論理プランの外側参照を別変種にしない理由**（`m4-query.md` §4.3 は `OuterColumn(ColId)` を別に持った）: 副問い合わせの書き換え（`WHERE` の最上位 AND にある `EXISTS` / `IN` → セミ結合、`04`）では、相関参照が通常の結合条件の列に変わる。別変種だと書き換えのたびに式を作り直す。1 変種なら、サブプランを結合の右側に移し、条件に使う式をそのまま `on` に入れるだけで済む。

### 2.2 D-1: `Var` / `ColId` / `PhysCol` の使い分け

| | `Var`（Bound） | `ColId`（論理） | `PhysCol`（物理） |
|---|---|---|---|
| 指すもの | 範囲表の (RTE, 列, 外側へのレベル) | 文の中で一意な列の台帳（`ColumnArena`）の添字 | 評価中の行の位置、または実行時パラメータ |
| 安定なもの | SQL の字面（名前解決の結果） | プランの形（並べ替え・押し下げ・刈り込み・書き換えで変わらない） | 実行時の行の形 |
| 作る層 | アナライザ | `build`（Bound → 論理）が発行。ルールは作らない（§3.5.2 の例外を除く） | `physicalize` |
| 使う層 | `build` | ルール、`physicalize` | `executor` |
| 副問い合わせの相関 | `levels_up > 0` | 子の出力にない `ColId`（外側のスコープの列） | `Param(ParamId)` |
| 根拠 | PG の `Var { varno, varattno, varlevelsup }` | DataFusion・CockroachDB の列 ID | PG の `Param` と `OUTER_VAR` / `INNER_VAR` の代わりに「結合の出力は常に左 ++ 右」の約束で位置を決める |

`ColId` を「`(RteId, col)` から機械的に作る」ことはしない。`Var` は FROM 句の構文上の位置を指すが、`ColId` は計算の結果（`Project` の式、集約の結果、集合演算の出力）も指すため。

### 2.3 D-2: 副問い合わせを `Q` 型パラメータで持つ理由

`SubLink { kind, test, query: Q }` の `query` だけが層ごとに違う。Bound では `Box<BoundQuery>`、論理では `Box<LogicalSubquery>`、物理では `SubPlanId`。

1. 変わるのは「木の中身」であって「式の形」ではない。`Q` をパラメータにすると、層の変換が `try_map` 1 つで閉じる（`SubLink` を受けた変換関数が `query` を変換する）。
2. 物理では木に埋め込まず `SubPlanId` にする。`PhysicalPlan` は不変の木で、副問い合わせは `PhysicalQuery.subplans` に平らに置く。理由は 3 つ: (a) Executor は状態を持つので、不変の式の木に持たせられない（「Plan は不変、Executor は状態を持つ」。`m4-query.md` §4.4）。(b) `eval` が `&mut ExecCtx` を取り、`SubPlanStates` から子 Executor を `take` して動かす方式にすると、`unsafe` も内部可変性も要らない（§3.7.3）。(c) 非相関の InitPlan・ハッシュ化 SubPlan の結果を Executor 側に保持できる。
3. EXPLAIN が `InitPlan n` / `SubPlan n` を別ラベルの子として出せる（`SubPlanDef` が戦略と ID を持つ）。

### 2.4 D-1: 集約を式の変種（`Aggregate`）にする理由

`sum(a) / count(*) + 1`、`HAVING sum(a) > (SELECT ...)`、`ORDER BY max(b)` のように、集約は任意の式の入れ子の中に現れる。「`BoundSelect` に集約の一覧を別に持ち、式からは添字で参照する」形にすると、アナライザが式を作りながら添字を割り当て、`build` が式を書き換える二重の作業になる。式の変種にしておけば、アナライザは普通の式の木を作り、`build` が `Aggregate` ノードの出力列 `Column(ColId)` に置き換える（`try_map` の葉の変換）。置き換えた後の論理プランには `Aggregate` が残らない（L4 で検証）。PostgreSQL の `Aggref` も式の変種である。

### 2.5 D-2: `SubLinkOutput` で ANY / ALL・行値 IN を表す理由

`m4-query.md` §4.3 は `Subquery { kind: Any { op, lhs } }` のように演算子と左辺をフィールドで持った。00 は「比較式 `test` を式として持ち、副問い合わせの出力列を `SubLinkOutput(i)` で参照する」形にした。

| 表したいもの | `test` |
|---|---|
| `a IN (SELECT x ...)` | `Operator(=, [a, SubLinkOutput(0)])` |
| `a NOT IN (SELECT x ...)` | `Not(SubLink { Any, test = (a = Out0) })` |
| `a < ALL (SELECT x ...)` | `SubLink { All, test = (a < Out0) }` |
| 右辺の型変換が要る（`int4 = ANY (int8 の列)`） | `Operator(=, [Cast(a), Out0])` など。演算子解決の結果をそのまま式にできる |
| 行値 `(a, b) IN (SELECT x, y ...)` | `And([a = Out0, b = Out1])` |

フィールドで持つ案は、型変換・行値・複数列を別のフィールドで足すことになる。式にしておけば、`eval` は 1 回のコードで済み（行ごとに `test` を評価して三値論理で畳むだけ）、ハッシュ化 SubPlan の可否は `test` の形（`= SubLinkOutput` の連言か）から物理化が判定でき、deparse（EXPLAIN の `ANY (a = (hashed SubPlan 1).col1)`）も式の deparse で足りる。

### 2.6 この章で追加で決めたこと

| # | 論点 | 決定 | 理由 |
|---|---|---|---|
| 02-D1 | `levels_up` の数え方 | **1 つの `BoundQuery`（本体が Select / Values / SetOp のどれでも）が 1 レベル**。`BoundUpdate` / `BoundDelete` の本体も 1 レベル。導出表・副問い合わせ式・集合演算の腕・CTE の本体は、それぞれ入れ子の `BoundQuery`（§3.4.2） | PG の `varlevelsup` と同じ数え方のはず（観測できる振る舞いは確認済み。内部の値は未検証。§8）。スコープの積み方（`analyze_query` ごとに 1 つ積む）と一致して実装が単純 |
| 02-D2 | Join RTE を指す `Var` | **Bound に残さない**。アナライザが `JoinColSource` に従って展開する（§3.4.3）。`build` は Join RTE を知らなくてよい | PG は planner で `flatten_join_alias_vars` する（未検証: ソースの記憶）。M4 は早く（解析時に）展開して、`build` と `validate`（B3）を単純にする |
| 02-D3 | `try_map` の葉 | `Column` / `Aggregate` / `SubLink` は **f が必ず変換する**（00 のとおり）。`walk` は `SubLink` の `query` に降りない。型を変えない書き換え用に `try_rewrite` を足す（§3.1.4） | 00 の署名は変えず、同じ型の中の書き換え（ColId の置換など）が「葉を clone して返す f」を毎回書かずに済むようにする |
| 02-D4 | `ColId` の定義 | 各 `ColId` は**ちょうど 1 つのノードが定義する**。同じ ID を再び出力に並べるだけの `(id, Column(id))` はパススルーで、定義ではない。1 つのノードの出力に同じ `ColId` を 2 回並べない（`SELECT b, b` の 2 つ目は新しい ID） | ルールが「どのノードがこの列を作るか」を一意に引けるようにする。書き換え（`substitute`）が全体で安全になる |
| 02-D5 | 外部結合の NULL 側 | `ColId` を**再発行しない**。結合より上で参照される `Column(c)` は「null 拡張後の値」を指す（§3.5.4） | 再発行すると、結合の上の式をすべて書き換える必要が出る。PG は `varnullingrels` で表すが、M4 は結合の位置から判定する |
| 02-D6 | SubPlan と Param の割り当て | `SubPlanId` は **SubLink の出現ごと**に 1 つ（同じ `LogicalSubquery` が複数の式に複製されたら別 ID）。`ParamId` は (SubPlan, 外側の `ColId`) ごとに 1 つ。**入れ子の副問い合わせが、さらに外側の列を参照するときは、Param を再束縛せず外側の `ParamId` をそのまま使う**（§3.6.3） | 各 `SubPlanId` の文脈行が 1 つに決まる。Param の束縛元（`NestedLoopParam.params` か `SubPlanDef.params`）が 1 つに決まる |
| 02-D7 | `uses_params()` | 木のどこかに `PhysCol::Param` があるか、**`SubLink` が 1 つでもあれば true**（保守的）。00 の定義に「SubLink を含めば true」を足す | `PhysExpr` の `SubLink` は `SubPlanId` だけで、副問い合わせの Param 参照が式の木から見えない。偽の true は再作成が増えるだけで正しさは保たれる。偽の false は古い結果を返す |
| 02-D8 | SubPlan の戦略 | 外側の列を参照しない（`outer_refs` が空）→ スカラー・EXISTS は `InitOnce`、等値の `ANY` でハッシュ可能なら `Hashed`、他の `ANY` / `ALL` は `InitOnce`（行を溜めて `test` を行ごとに評価）。外側を参照する → `Rescan`。`InitOnce` と `Hashed` は**最初の評価で遅延実行**する（実行しない分岐のエラーを出さない） | PG の InitPlan / SubPlan と同じ振る舞い |
| 02-D9 | 外側レベルの集約 | `(SELECT sum(t.a) FROM u) FROM t` のように、集約の引数の `Var` がすべて外側（`levels_up > 0`）の集約は `0A000`（`outer-level aggregate functions are not supported yet`）。引数に `Var` がない（`count(*)`、`sum(1)`）か、レベル 0 の `Var` を含む集約はそのレベルの集約 | PG は外側の問い合わせの集約として扱う（確認済み: 上の例は `u` が 2 行のとき `21000` になる。集約が外側の問い合わせに属し、副問い合わせが `u` の行ごとに外側の集約値を返す、と解釈できる）。M4 では必要性が低い |
| 02-D10 | 物理の `SubLink.test` | 物理の式の `SubLink { test }` は常に `None`。比較式は `SubPlanDef.test` に置く（00 §9.3 のとおり）。論理・Bound では式の `test` に持つ | 二重に持たない。`SubLinkOutput` は `SubPlanDef.test` の中だけに現れる |
| 02-D11 | 検証の規約 | `LogicalQuery::validate` / `PhysicalQuery::validate` を、**デバッグビルドでは `plan()` が各段階の直後に必ず実行**する。リリースビルドでは隠し設定 `yuzhu.validate_plans`（既定 off）で有効にできる（slt を実機サーバで流すとき用） | 全テストが `cargo test`（デバッグビルド）で走るので、全テストで不変条件が検査される（§3.3） |
| 02-D12 | `ExplainNode` の作り方 | `physicalize` の**最中に**（`LExpr` と `ColumnArena` が手元にあるうちに）作る。物理プランから後で作ることはしない | `PhysCol::Local(i)` は列名を持たない。EXPLAIN の式は列名で出す |
| 02-D13 | 論理 `Empty` の物理形 | `Result { exprs: 型つきの NULL × 列数, one_time_filter: Some(false) }` | 物理に `Empty` ノードを足さない（PG も `Result` + `One-Time Filter: false`） |
| 02-D14 | 移行の順序 | **下から**（executor → analyzer → planner）。各段階の隣に一時的なアダプタを置く（§5） | アダプタが自明な構造変換だけで済む。上から（analyzer → planner → executor）だと `physicalize` を旧型向けと新型向けの 2 回書くことになる |
| 02-D15 | DML の入力の形 | `build` が `Project` で作る（§3.5.5）。M2 の `Update.assignments` は廃止 | 00 §9.2 のとおり。UPDATE ... FROM で代入式が FROM の列を参照できる |
| 02-D16 | `ExecCtx` の作り方と `rewind` | `ExecCtx::new(env, txn, query)` を足す。`rewind` は**未開始のノードにも呼べ**、副作用は「初期状態に戻す」だけ | `ExecCtx { .. }` のリテラルが session・テストに散らばるのを避ける。`Append` などが子を一律に `rewind` できる |
| 02-D17 | ハッシュ化 SubPlan の NULL | `Hashed` は NULL を含まないキーの完全一致だけを表で引き、**探索キーに NULL がある、または表の行に NULL を含むキーがあるときは `test` を行ごとに評価する経路に落とす**（§3.7.3） | 三値論理を単一の規則で正しくする。多列でも同じ規則 |
| 02-D18 | LIMIT / OFFSET の式 | レベル 0 の `Var` を含まない（`42P10 argument of LIMIT must not contain variables`、確認済み）。外側の `Var`（`levels_up > 0`）と副問い合わせは可（`limit (select 1)` と、`select (select 1 from u limit t.a) from t` の解析はどちらも通る。確認済み） | PG と同じ |

---

## 3. 型とトレイト（契約の具体化）

00 §6〜§10 の署名は変えない。ここでは、署名だけでは決まらない**意味・順序・不変条件**と、00 に無い**補助 API**（追加のみ）を決める。コードブロックに出てくる署名は、00 にあるものの再掲ではなく、この章で足すものだけである（再掲は 00 を正とする）。

### 3.1 `expr`（`expr/mod.rs`、`expr/walk.rs`）

#### 3.1.1 ID と `Var`

| 型 | 意味 | 範囲 |
|---|---|---|
| `RteId(u16)` | `BoundSelect.rtable` の添字（`BoundUpdate` / `BoundDelete` では `rtable` の添字）。`RteId(0)` は DML では対象表 | 超えたら `54000`（`too many range table entries`） |
| `ColId(u32)` | `ColumnArena` の添字。1 つの文（副問い合わせ・CTE を含む）の中で一意 | `ColumnArena::add` は `Result` を返さない（u32 を超える前にメモリが尽きる） |
| `ParamId(u16)` | `ExecCtx.params` の添字 | 超えたら `54000`（`too many parameters in query`） |
| `SubPlanId(u16)` | `PhysicalQuery.subplans` の添字 | 超えたら `54000`（`too many subplans in query`） |
| `CteId(u16)` | Bound では**宣言した `BoundQuery.ctes` の添字**。論理では `LogicalQuery.ctes` の添字（`build` が宣言順に通し番号で付け替える） | |

`Var.col` の符号化は 00 §6.1 のとおり（`>= SYSTEM_COL_BASE` ならシステム列）。追加するヘルパ:

```rust
impl Var {
    /// levels_up を n に直した Var（副問い合わせの外側参照を作る）
    pub fn with_levels_up(self, n: u16) -> Var;
    pub fn is_system(&self) -> bool;
    /// 現在のレベルの Var か
    pub fn is_local(&self) -> bool;            // levels_up == 0
}
```

#### 3.1.2 `ExprKind` の変種ごとの規則

`walk` / `try_map` / `same_as` / `validate` / deparse が共通に使う「子の順序」と「`ty` の決め方」の表。**子の順序は字面の左から右**で、`SubPlanId` / `ParamId` の割り当てと deparse の安定性がこれに依存する。

| 変種 | 子（この順序） | `ty` | 備考 |
|---|---|---|---|
| `Literal(d)` | なし | リテラルの型。`Datum::Null` でも具体的な型（`unknown` は残さない） | |
| `Column(c)` | なし（葉） | 列の型（Bound: `Rte.columns[col].ty`、論理: `ColumnInfo.ty`、物理: 元の `ColumnInfo.ty`） | `ty` は 3 層で同じ値を持ち回る |
| `Operator { op, args }` | `args` | `op.result` | |
| `Function { func, args }` | `args` | `func.result` | |
| `Cast { expr, method }` | `expr` | キャスト先 | typmod は `CoerceTypmod` |
| `CoerceTypmod { expr, explicit }` | `expr` | `expr.ty`（typmod だけ変わる） | |
| `And(v)` / `Or(v)` | `v` | bool | 2 個以上。0 個・1 個は作らない（`Expr::and_all` を使う） |
| `Not(e)` / `IsNull(e)` / `IsNotNull(e)` | `e` | bool | |
| `BoolTest { expr, test }` | `expr` | bool | |
| `Case { arms, else_result }` | `arms` の (条件, 結果) を順に、最後に `else_result` | 結果の共通型 | |
| `Coalesce(v)` | `v` | 共通型 | 1 個以上 |
| `NullIf { left, right, eq_op }` | `left`、`right` | `left.ty` | |
| `Like { expr, pattern, escape, .. }` | `expr`、`pattern`、`escape` | bool | |
| `InList { expr, list, .. }` | `expr`、`list` | bool | |
| `SessionValue(k)` | なし | 値ごと | |
| `Aggregate(call)` | `call.args`、`call.filter` | `call.func.result` | **葉扱い**（§3.1.4）。Bound にだけ現れる |
| `SubLink { kind, test, query }` | `test`（`query` は**含めない**） | Scalar: 出力列の型、他: bool | **葉扱い**。`query` の中身へは `walk` も `try_map` も降りない |
| `SubLinkOutput(i)` | なし | 副問い合わせの i 番目の出力列の型 | `test` の中だけ |

#### 3.1.3 `walk`

```rust
pub fn walk(&self, f: &mut dyn FnMut(&Expr<C, Q>) -> bool);
```

- 先行順（親 → 子）。各ノードで `f` を呼び、`false` なら**そのノードの子には降りない**（兄弟には進む）。根でも同じ。
- `SubLink` では `test` に降りるが、`query` には降りない。副問い合わせの中の式を調べたい呼び出し側は、層ごとの専用関数を使う（論理: `LogicalPlan::outer_refs`、Bound: `BoundQuery` の `validate`）。
- `Aggregate` では `args` に降り、次に `filter` に降りる。

追加するヘルパ（`any` 以降は `walk` の上に作る）:

```rust
impl<C, Q> Expr<C, Q> {
    pub fn new(kind: ExprKind<C, Q>, ty: SqlType, span: Span) -> Self;
    pub fn literal(d: Datum, ty: SqlType) -> Self;                 // span = Span::default()
    pub fn column(c: C, ty: SqlType) -> Self;
    pub fn null_of(ty: SqlType) -> Self;                            // 型つきの NULL
    pub fn bool_lit(b: bool) -> Self;
    /// 0 個 → bool_lit(true)、1 個 → そのまま、2 個以上 → And（子の And は 1 段に平らにする）。定数畳み込みはしない
    pub fn and_all(parts: Vec<Self>) -> Self;
}
impl<C: Clone, Q: Clone> Expr<C, Q> {
    /// 部分木のどこかで pred が true か（SubLink の query には降りない）
    pub fn any(&self, pred: &mut dyn FnMut(&Self) -> bool) -> bool;
    pub fn contains_aggregate(&self) -> bool;
    pub fn contains_sublink(&self) -> bool;
    /// AND の最上位の項。And なら平らにした各項、そうでなければ自分 1 つ
    pub fn conjuncts(&self) -> Vec<&Self>;
}
impl<C: Clone + PartialEq, Q: Clone> Expr<C, Q> {
    /// 構造の等価（span は無視）。M1 の same_expr の一般化。Literal の float はビット比較、Operator / Function は oid、
    /// Cast は method の種類、Aggregate は func.oid・distinct・args・filter で比べる。SubLink は常に false（保守的）
    pub fn same_as(&self, other: &Self) -> bool;
    /// 出現順・重複ありの Column の一覧（SubLink の query の中は含まない）
    pub fn columns(&self) -> Vec<C>;
}
```

`same_as` が `SubLink` で常に false になる帰結: `GROUP BY (SELECT ...)` の式を SELECT に書き直すと、アナライザは一致を見つけられず `42803` にする（PG は通す。M4 では許容。§8）。

#### 3.1.4 `try_map` と `try_rewrite`

```rust
pub fn try_map<C2, Q2>(&self, f: &mut dyn FnMut(&Expr<C, Q>) -> Result<Option<Expr<C2, Q2>>>) -> Result<Expr<C2, Q2>>;
```

厳密な規則（上から下へ、先行順）:

1. 各ノードでまず `f(node)` を呼ぶ。`Ok(Some(e))` なら、**そのノードの部分木を `e` に置き換え、子には降りない**（`e.ty` と `e.span` は `e` のものをそのまま使う）。
2. `Ok(None)` のとき:
   - **構造を保つ変種**（`Operator` `Function` `Cast` `CoerceTypmod` `And` `Or` `Not` `IsNull` `IsNotNull` `BoolTest` `Case` `Coalesce` `NullIf` `Like` `InList`）は、子を §3.1.2 の順に `try_map` で再帰的に変換して**同じ変種を再構築**する。`op` `func` `method` `eq_op` `explicit` `negated` `case_insensitive` `test` などの非子フィールド、`ty`、`span` は複製する。
   - **`C` にも `Q` にも依存しない葉**（`Literal` `SessionValue` `SubLinkOutput`）は複製する。
   - **葉扱いの 3 変種**（`Column` `Aggregate` `SubLink`）は、`f` が `None` を返したら **`Error::internal("try_map: 葉 Column が f で変換されなかった")`**（変種名は実際のもの）を返す。型パラメータが変わる変換で、これらを変換し忘れると別の層の式に別の層の列が混ざる。それを黙って通さないための規則である。
3. エラーは最初のものを返し、以降は降りない。

`SubLink` を受けた `f` は、`test` と `query` の両方を変換して 1 つの `SubLink` を返す責任を持つ。`test` の変換には、呼び出し側の再帰関数（たとえば `build` の `lower_expr(&mut self, e, scope)`）を `f` の中から呼べばよい（`f` が自分自身を呼ぶ必要はない）。`Aggregate` を受けた `f` は、`Column` に置き換える（Bound → 論理）か、`args` / `filter` を自分で変換して返す。

型を変えない書き換え（`ColId` の置換、`Var` の付け替え）用に、00 の署名に**足す**:

```rust
impl<C: Clone, Q: Clone> Expr<C, Q> {
    /// 型を変えない書き換え。f が None を返した葉（Column / Aggregate / SubLink）は clone してそのまま使う。
    /// None を返した Aggregate は args / filter に、SubLink は test に降りる（query は clone）
    pub fn try_rewrite(&self, f: &mut dyn FnMut(&Self) -> Result<Option<Self>>) -> Result<Self>;
}
```

#### 3.1.5 `lower_single_rel`

```rust
pub fn lower_single_rel(e: &BoundExpr) -> Result<PhysExpr>;
```

`rte = 0` の `Var`（1 つの表の行に対する式。CHECK・DEFAULT・COPY の列変換・RETURNING）を `PhysCol::Local(col)` に直す。`col` は表のユーザー列の位置（`attnum - 1`）で、評価する行は「その表のユーザー列を attnum 順に並べた行」。

| 入力 | 結果 |
|---|---|
| `Var { rte: 0, col: i, levels_up: 0 }`（`i` はユーザー列） | `Column(Local(i))` |
| `Var { rte != 0 }`、`levels_up > 0`、システム列 | `Error::internal`（アナライザが先に拒否しているはず） |
| `SubLink`、`Aggregate` | `Error::internal`（同上。CHECK・DEFAULT の中の副問い合わせは、アナライザが PG と同じ `0A000 cannot use subquery in check constraint` / `cannot use subquery in DEFAULT expression` で拒否する。確認済み） |
| `SubLinkOutput` | `Error::internal` |
| それ以外 | 構造をそのまま `PhysExpr` に（`ty` `span` を保つ） |

同じ「単一の行」の約束を使う文脈がもう 2 つある（どちらも `Var { rte: RteId(0), col: i, levels_up: 0 }`）が、行の意味が違う。

| 文脈 | 行の意味 | 変換する人 |
|---|---|---|
| CHECK・DEFAULT（Var なし）・RETURNING・COPY の列変換 | 対象表の行（ユーザー列） | `lower_single_rel` |
| `BoundInsert.coercions`、`BoundSetExpr::SetOp` の `left_coerce` / `right_coerce` | 副問い合わせ（腕）の出力行の i 番目 | `build`（`Column(出力列の ColId)` に置換。`lower_single_rel` は使わない） |

### 3.2 層ごとの不変条件

00 §6.4 の表を拡張する。**この表と §3.3 の検証規則を、新しい変種や新しいノードを足すたびに更新する**（00 §4.3 の規約 1）。

#### 3.2.1 変種が現れてよい場所

| 変種 | Bound | 論理 | 物理 |
|---|---|---|---|
| `Literal` `Operator` `Function` `Cast` `CoerceTypmod` `And` `Or` `Not` `IsNull` `IsNotNull` `BoolTest` `Case` `Coalesce` `NullIf` `Like` `InList` `SessionValue` | 可 | 可 | 可 |
| `Column` | `Var`（B1〜B3） | `ColId`（L2） | `Local` / `Param`（P1、P2） |
| `Aggregate` | `targets` と `having` の式の中だけ。`AggCall` の `args` / `filter` の中には現れない（B4） | **不可**（L4。`build` の途中で `Column` に置き換わる） | **不可**（P5） |
| `SubLink` | `Box<BoundQuery>`。`test` は Any / All で必須、Scalar / Exists で `None`（B5） | `Box<LogicalSubquery>`。`test` は同じ規則（L5） | `SubPlanId`。**`test` は常に `None`**（P4。比較式は `SubPlanDef.test`） |
| `SubLinkOutput(i)` | `SubLink.test` の中だけ。`i` < 副問い合わせの可視出力列数（B5） | 同じ（L5） | `SubPlanDef.test` の中だけ。`i` < 副問い合わせプランの幅（P4） |

#### 3.2.2 Bound の不変条件（B）

| # | 不変条件 |
|---|---|
| B1 | `Var.levels_up` ≤ その `Var` を含むスコープの外側にあるレベルの数。参照先のレベルは `rtable` を持つ（本体が Select の `BoundQuery`、`BoundUpdate`、`BoundDelete`）。`BoundQuery` の本体が Values / SetOp のレベルは `rtable` を持たないので、そのレベルを指す `Var` は無い |
| B2 | `Var.rte` < 参照先の `rtable.len()`。`Var.col` < `Rte.columns.len()`（ユーザー列）。システム列（`col >= SYSTEM_COL_BASE`）は `RteKind::Table` にだけ |
| B3 | `Var` は **Join RTE を指さない**（02-D2） |
| B4 | `Aggregate` は `BoundSelect.targets` / `having` だけに現れ、`AggCall` の `args` / `filter` の中・`filter`・`group_by`・`rtable` の式・`on`・`limit`・`offset` の中には現れない。`Aggregate` の `args` の `Var` が**すべて**外側（`levels_up > 0`）の集約は無い（02-D9）。`has_agg` は「`Aggregate` がある、または `group_by` が空でない、または `having` がある」と一致する |
| B5 | `SubLink.test` は Any / All でのみ `Some`。`SubLinkOutput` は `test` の中だけで、`i` < `query.columns.len()` |
| B6 | `BoundSortKey.target` < `targets.len()`（Select）または < 出力列数（Values / SetOp）。`BoundDistinct::On` の位置 < `targets.len()`。`columns.len() == n_visible`（Select）。`n_visible <= targets.len()` |
| B7 | `rtable` の非 Join の RTE は、`from` の木の `Scan` のどこかにちょうど 1 回ずつ現れる（DML の `rtable[0]`（対象表）は `from` に現れない）。Join RTE は `FromItem::Join` の `rte` にちょうど 1 回ずつ現れる |
| B8 | `BoundQuery.limit` / `offset` はレベル 0 の `Var` を含まない（02-D18）。`Aggregate` を含まない |
| B9 | `BoundInsert.defaults` と `UpdateSource::Default` の式は、`Var` / `SubLink` / `Aggregate` を含まない。`BoundCheck.expr` と `BoundReturning.targets` は `Var { rte: 0, levels_up: 0 }`（システム列を除く）だけを含み、`SubLink` / `Aggregate` を含まない（M4。RETURNING の副問い合わせは `0A000`） |
| B10 | `left_coerce` / `right_coerce` / `coercions` は `Var { rte: 0, col: i, levels_up: 0 }`（`i` < 腕の出力列数）だけを含み、`SubLink` / `Aggregate` を含まない。長さは `types.len()` / 対象列数に等しい |
| B11 | 非 LATERAL の導出表（`RteKind::Subquery`）の `query` と、CTE の本体から、その FROM 句を持つ SELECT の `rtable`（兄弟の RTE）を指す `Var`（導出表の中の `levels_up = 1`）が無い。LATERAL は `0A000` なので、このレベルは外側の参照先にならない |

#### 3.2.3 論理プランの不変条件（L）

| # | 不変条件 |
|---|---|
| L1 | 各 `ColId` は `arena` の範囲内で、**ちょうど 1 つのノードが定義する**（`LogicalPlan::defines`、02-D4）。プランは木で、同じ部分木が 2 か所に現れない（ルールは産出する列を持つ部分木を複製しない） |
| L2 | ノードの式の `Column(c)` は、そのノードの**子の出力列**（`output_cols`）にあるか、そのプランが入っている `LogicalSubquery` の**外側のスコープの列**（外側の式が参照できる列）でなければならない。`Join.on` は左右の子の出力の和集合を見る（Semi / Anti でも右を見る）。`Limit` の式は子の出力を見ない（外側の列と定数だけ） |
| L3 | 1 つのノードの `output_cols()` に重複がない。`Get` / `Values` / `FunctionScan` / `CteScan` / `SetOp` / `Result` / `Empty` の `cols` の長さと型が、それぞれの定義（表・行・関数・CTE の出力・`left_cols` / `right_cols`）と一致する |
| L4 | `LExpr` の中に `Aggregate` が無い。`Aggregate` ノードの `group_by` / `aggs` の式は子の出力を見る |
| L5 | `SubLink.test` は Any / All でのみ `Some`。`SubLinkOutput(i)` は `test` の中だけで、`i` < `LogicalSubquery.output.len()`。`LogicalSubquery.output` の各 `ColId` は `plan.output_cols()` に含まれる（Exists では空でよい） |
| L6 | `Join` の `kind` に Right / Cross が無い。Semi / Anti の出力は左だけ。`SetOp` の `left_cols.len() == right_cols.len() == cols.len()` で、対応する列の型が等しい（型変換は腕の上の `Project` が済ませている） |
| L7 | `Distinct.on` の式は子の出力を見る。`Sort.keys` の式は子の出力を見る |
| L8 | `Insert` / `Update` / `Delete` は根にだけ現れる。`Update.input.output_cols() == old_cols ++ [ctid] ++ new_values の ColId`、`Delete.input.output_cols() == old_cols ++ [ctid]`（§3.5.5） |
| L9 | `LogicalQuery.output` は `plan.output_cols()` と一致する（DML の根は空）。`ctes[i].output` は `ctes[i].plan.output_cols()` の部分列 |
| L10 | `CteScan` の `cte` は `ctes` の範囲内で、`cols.len() == ctes[cte].output.len()`、対応する列（位置 i どうし）の型が等しい。`inline == false` の CTE の `refs` は `CteScan` の出現数と一致する。`inline == true` の CTE を指す `CteScan` は残らない |

#### 3.2.4 物理プランの不変条件（P）

| # | 不変条件 |
|---|---|
| P1 | `PhysCol::Local(i)` の `i` < その式の**文脈行の幅**（§3.6.2 の表）。文脈行が空のフィールド（`Result` `Values` `Limit` `FunctionScan` の引数、`IndexScan` のキー、`Insert.defaults`）に `Local` は無い |
| P2 | `PhysCol::Param(p)` の `p` < `PhysicalQuery.n_params`。かつ、その式が評価される時点で `p` が**束縛済み**である。束縛するのは、祖先の `NestedLoopParam.params`（その `inner` の木の中だけ）、または、祖先の SubLink の `SubPlanDef.params`（その副問い合わせプランの中だけ）。同じ `ParamId` を束縛するものは文全体で**1 つだけ**（02-D6） |
| P3 | `SubLink.query` の `SubPlanId` は `subplans` の範囲内。各 `SubPlanId` は文全体（根・全副問い合わせ・全 CTE）でちょうど 1 回だけ参照される。`SubPlanDef.kind` は参照する `SubLink.kind` と同じ。参照は循環しない |
| P4 | 式の `SubLink.test` は `None`。`SubPlanDef.test` は kind が Any / All のときだけ `Some`。`SubLinkOutput(i)` は `SubPlanDef.test` の中だけで `i` < 副問い合わせプランの幅。`Hashed` は kind = Any、`test` が等値の連言で `probe_keys.len() == build_keys.len()`、`params` が空。`InitOnce` は `params` が空。`Hashed` と `InitOnce` の副問い合わせプランは、**外側で束縛された `Param` を使わない**（検証は、束縛済みの集合を空にして P2 を適用する。プランの中の `NestedLoopParam` が束縛する `Param` は使ってよい。なお `uses_params()` は使わない。それは内部で束縛された `Param` も数える保守的な判定のため）。`Rescan` は制限なし |
| P5 | `ExprKind::Aggregate` が無い |
| P6 | 幅の整合: `NestedLoopJoin` / `NestedLoopParam` の `outer_width == outer.width()`、`inner_width == inner.width()`（`HashJoin` は `left_width` / `right_width`）。`Append` の全入力が同じ幅。`HashSetOp` は左右が同じ幅。`HashJoin` の `left_keys.len() == right_keys.len() == key_types.len()`。`Unique.key_cols` の各値 < 入力の幅。`Insert` の `column_map` の `Some(i)` < 入力の幅。`Update` の入力の幅 = `n_user_cols + 1 + assigned.len()`、`assigned` の入力位置 = `n_user_cols + 1 + k`。`Delete` の入力の幅 = `n_user_cols + 1`。`SeqScan` / `IndexScan` の `columns.len()` = 表のユーザー列数 |
| P7 | `Values` は 1 行以上で全行が同じ幅。`NestedLoopJoin` / `NestedLoopParam` の `kind` が `Full` でない |
| P8 | `CteScan.cte` < `ctes.len()`。`ctes[i]` のプランは、外側で束縛された `Param` を使わない（束縛済みの集合を空にして P2 を適用する。CTE は 1 回だけ実行して共有する） |
| P9 | `IndexScanKeys`: `eq.len() <= index.columns.len()`。`lower` / `upper` があるなら `eq.len() < index.columns.len()`。キーの式に `Local` が無い |
| P10 | `Insert` / `Update` / `Delete` は根にだけ現れる |
| P11 | `PhysicalQuery.root.width() == output.len()`（DML は RETURNING の式の数。無ければ 0） |

### 3.3 検証関数（`validate`）

```rust
// planner/logical.rs
impl LogicalQuery {
    /// L1〜L10。違反は Error::internal("invalid logical plan [L2] ...") (XX000)
    pub fn validate(&self) -> Result<()>;
}
impl LogicalPlan {
    /// outer: このプランが外側から参照してよい列（LogicalSubquery の中では、SubLink が置かれたスコープの列）。根では空
    pub fn validate(&self, arena: &ColumnArena, outer: &BTreeSet<ColId>) -> Result<()>;
}

// planner/physical.rs
impl PhysicalQuery {
    /// P1〜P11。違反は Error::internal("invalid physical plan [P1] ...") (XX000)
    pub fn validate(&self) -> Result<()>;
}
impl PhysicalPlan {
    pub fn validate(&self, cx: &PhysValidateCx<'_>) -> Result<()>;
}
pub struct PhysValidateCx<'a> { pub query: &'a PhysicalQuery, /* 束縛済みの ParamId の集合、親の文脈行の幅など */ }

// analyzer/query.rs（最終的には bound.rs）
impl BoundQuery {
    /// B1〜B11。違反は Error::internal("invalid bound query [B3] ...") (XX000)
    pub fn validate(&self) -> Result<()>;
}
```

規約:

1. **デバッグビルド（`cfg!(debug_assertions)`）では、次の直後に必ず実行する**: `analyzer::analyze` が返す直前（`BoundQuery::validate`。DML は各 `BoundQuery` と本体）、`planner::build` の直後、`rules::run` の各ルールの直後、`physicalize` の直後。`cargo test` はデバッグビルドなので、**全テスト（単体・統合・クラッシュ試験・サーバのテスト）で不変条件が検査される**。
2. リリースビルドでは実行しない。ただし隠し設定 `yuzhu.validate_plans`（既定 off。`PlannerSettings.validate_plans`。00 への変更提案）を on にすると、リリースビルドでも同じ箇所で実行する。`tests/run.sh --target yuzhu` のサーバに `-c yuzhu.validate_plans=on` を付けて、共有 slt の全体で不変条件を検査する（K と S が行う）。
3. 検証の失敗は**バグ**であり、ユーザーのエラーではない。`Error::internal`（`XX000`）に、違反した規則の ID（`L2` など）と、ノードの種類、関係する `ColId` / `ParamId` を書く。ルールの作者が原因を特定できる粒度にする。
4. `validate` は**純粋関数**（副作用なし、カタログも見ない）。単体テストでは、わざと壊したプランを作って各規則が検出することを確かめる（§6）。
5. 規則の追加は、新しいノード・変種を足す変更と**同じコミット**で行う。

### 3.4 Bound（`analyzer::bound`）

フィールドの形は 00 §7 のとおり。ここでは意味を決める。

#### 3.4.1 `BoundQuery` と `BoundSelect` の評価順序

`BoundQuery` は「WITH + 本体 + ORDER BY / LIMIT / OFFSET」、`BoundSelect` は本体が Select のときの「FROM から targets まで」。**論理的な評価順序**は次のとおり（PostgreSQL の SELECT と同じ）。番号の隣に、その段を持つ構造体のフィールドを書く。

| 段 | 処理 | フィールド |
|---|---|---|
| 1 | 行の生成: `from` の各項目を結合の木として評価し、カンマ区切りの項目どうしは直積にする。FROM なしは 1 行（0 列） | `BoundSelect.rtable`、`from` |
| 2 | 絞り込み | `BoundSelect.filter`（集約を含まない） |
| 3 | グループ化: `group_by` の式でグループに分ける。`has_agg` かつ `group_by` が空なら全体を 1 グループ（入力が 0 行でも 1 行を出す） | `group_by`、`has_agg`、（集約は `targets` / `having` の `Aggregate`） |
| 4 | グループの絞り込み | `having`（集約を含んでよい） |
| 5 | 出力式の計算: `targets` の全要素（可視列 + resjunk）。この段で `Aggregate` は集約結果を指す | `targets`、`n_visible` |
| 6 | 整列 | `BoundQuery.order_by`（`target` で `targets` の位置を指す。resjunk を指してよい） |
| 7 | 重複除去 | `BoundSelect.distinct`（`All` は可視列の全体、`On` は `targets` の位置。`On` の式は ORDER BY の先頭と一致している（アナライザが検査済み）ので、整列済みの入力から先頭キーが変わった最初の行を取る） |
| 8 | 行数の制限 | `BoundQuery.limit` / `offset`（OFFSET を先に適用） |
| 9 | resjunk の除去: 先頭 `n_visible` 列だけを残す | `n_visible` |

- 7 の `distinct` は `BoundSelect` にあり、6 と 8 は `BoundQuery` にある。**`build` は 5 → 6 → 7 → 8 → 9 の順に 3 つの構造体をまたいで積む**（`Project`（全 targets）→ `Sort` → `Distinct` → `Limit` → `Project`（可視列））。
- `DISTINCT`（`All`）では、ORDER BY の式がすべて可視列に含まれる（`42P10`）ので、resjunk は無い。
- 本体が Values / SetOp の `BoundQuery` には 1〜5 と 7 が無い。`order_by` の `target` は出力列の位置で、resjunk は無い。
- `order_by` は PG と同じく**出力列の別名を先に**、GROUP BY の単純な名前は**入力列を先に**解決する（`m4-query.md` §3.4。アナライザの規則）。

`BoundQuery` のフィールド:

| フィールド | 意味 |
|---|---|
| `ctes` | WITH 句の CTE（宣言順）。`CteRef { levels_up, cte }` が参照する。後の CTE は前の CTE を参照できる。**宣言しただけでは実行しない**（参照から使われる。インライン展開するか 1 回だけ実行するかは `build` / ルール） |
| `body` | `Select` / `Values` / `SetOp` |
| `order_by` / `limit` / `offset` | 上の 6・8。`limit` / `offset` は int8 型の式。レベル 0 の `Var` を含まない（B8） |
| `columns` | 外に見える出力列。`name` `ty` `table_oid` `attnum`。`table_oid` / `attnum` は、出力式が `Var` のとき、その `Var` をたどった先の元の表の列（Table → (表の OID, attnum)。Subquery / CteRef → 内側の `query.columns[i]`。Join → `sources` 経由。Values / Function / それ以外の式 → 0, 0）。RowDescription の元の表情報になる |

`BoundSelect` のフィールド（00 §7 を補う）:

| フィールド | 意味 |
|---|---|
| `rtable` | このレベルの範囲表。**インデックス = `RteId`**。JOIN も RTE |
| `from` | FROM のカンマ区切りの項目。空 = FROM なし |
| `group_by` | GROUP BY の式（位置番号と別名は解決済み）。**`group_by` に無い `Var` が `targets` / `having` に現れてよいのは、その `Var` の表の主キーの全列が `group_by` に単純な列参照で含まれるとき（関数従属）だけ**（`03`）。`build` はそのような `Var` を、追加のグループキー（`Aggregate.group_by` の末尾）として足す。グループの分割は変わらない |
| `targets` | 可視列の式 + resjunk |
| `distinct` | `None` / `All` / `On(位置の並び)` |

#### 3.4.2 スコープと `levels_up`

**レベルを作るもの**（02-D1）: 入れ子の各 `BoundQuery`（導出表 `RteKind::Subquery`、副問い合わせ式 `SubLink.query`、集合演算の腕 `left` / `right`、CTE の本体 `BoundCte.query`）と、`BoundUpdate` / `BoundDelete` の本体。`BoundInsert.source` は外側のスコープを持たない独立した問い合わせ（`levels_up` は常に 0 から始まる）。

`Var.levels_up = k` は「`Var` を含む式が置かれたレベルから、外側へ `k` 個目のレベルの `rtable`」を指す。本体が Values / SetOp のレベルは `rtable` を持たないが、レベルとしては数える。

```sql
-- t(a, b), u(a, c), v(x)
SELECT (SELECT count(*) FROM u WHERE u.a = t.a) FROM t
--  内側の t.a = Var { rte: RteId(0), col: 0, levels_up: 1 }   （外側の SELECT の rtable[0] = t）

SELECT (SELECT (SELECT t.a + u.a FROM v LIMIT 1) FROM u LIMIT 1) FROM t
--  最も内側の t.a = Var { rte: 0, col: 0, levels_up: 2 }、u.a = Var { rte: 0, col: 0, levels_up: 1 }

SELECT (SELECT 1 FROM (SELECT t.a) s) FROM t
--  導出表の中の t.a = Var { rte: 0, col: 0, levels_up: 2 }
--  （導出表自身が 1 レベル、その外の副問い合わせが 1 レベル。PG 17 で通る。確認済み）
--  `FROM t, (SELECT t.a) s` のように兄弟の t を指すのは 42P01
--  "invalid reference to FROM-clause entry for table "t"" + HINT "To reference that table, you must mark this subquery with LATERAL."（確認済み）

SELECT (SELECT a FROM u UNION SELECT t.a) FROM t
--  右の腕の中の t.a: 腕の BoundQuery（レベル 0）→ 集合演算を本体に持つ BoundQuery（SubLink.query。レベル 1。rtable を持たない）
--  → 外側の SELECT（レベル 2）の順なので levels_up = 2。rtable を持たない中間のレベルも数える
--  PG 17 で通る（確認済み）

SELECT (VALUES (t.a)) FROM t
--  Values を本体に持つ BoundQuery がレベル 0（rtable なし）、外側の SELECT がレベル 1 なので、t.a = Var { rte: 0, col: 0, levels_up: 1 }
--  PG 17 で通る（確認済み）
```

- アナライザのスコープ（`Scope` の積み重ね）は、`analyze_query` ごとに 1 つ積み、`Var` の `levels_up` は「積んだスコープの末尾から何個戻ったか」にする。
- `build` は、同じ数え方でスコープの対応表（`Var` → `ColId`）のスタックを持つ（§3.5.6）。
- 結合キーや GROUP BY の式を `Var` の等価で比べるとき、`levels_up` が違う `Var` は別物。

#### 3.4.3 `Rte`・`FromItem`・`JoinColSource`

```rust
pub struct Rte { pub kind: RteKind, pub refname: Option<String>, pub columns: Vec<RteColumn>, pub span: Span }
```

| `RteKind` | `refname` | `columns` | 備考 |
|---|---|---|---|
| `Table` | 別名、なければ表名 | 表のユーザー列（attnum 順）。`AS t(x, y)` の列別名を適用した名前 | システム列は `columns` に含めない。`Var.col >= SYSTEM_COL_BASE` で参照 |
| `Subquery { query }` | 別名（PG 16 以降は無くてもよい。確認済み: `SELECT * FROM (SELECT 1 AS x)` が通る）。無ければ `None` | `query.columns` の名前（列別名があれば置き換え）と型 | 非 LATERAL |
| `Values { rows }` | 別名、無ければ `"*VALUES*"` | `column1`…（列別名があれば置き換え）と各列の共通型 | FROM 句の `(VALUES ...) AS v(a, b)` |
| `Function { call }` | 別名、無ければ関数名 | `generate_series` なら 1 列（引数が int4 なら int4、int8 を含むなら int8）。名前は列別名、無ければ関数名 | `call` は `Function` の式（`Var` を含まない。LATERAL 以外） |
| `Join { kind, left, right, sources }` | `AS j` があれば `j`、無ければ `None` | 結合の可視出力列（下記） | `Var` はこの RTE を指さない（B3）。名前解決・`SELECT *` の展開・`j.*` のためにある |
| `CteRef { levels_up, cte }` | 別名、無ければ CTE の名前 | CTE の出力列（列別名を適用） | `levels_up` は CTE を宣言した `BoundQuery` までの入れ子の深さ（`Var.levels_up` と同じ数え方） |

`FromItem`:

- `Scan(rte)`: Table / Subquery / Values / Function / CteRef の RTE 1 つ。
- `Join { rte, kind, left, right, on }`: `rte` はこの結合の Join RTE。`kind` は `Inner` / `Left` / `Right` / `Full` / `Cross`。`on` は結合条件（bool 型）。`Cross` と、共通列が無い `NATURAL` は `on: None`（`Inner` の `on: None` も直積）。USING / NATURAL の条件（`l.a = r.a` の連言。型が違えば演算子解決の結果に従う）は**アナライザが `on` に合成する**。
- カンマ区切りの項目どうしは `BoundSelect.from` の要素（直積）。`build` が左から順に `Join(Inner, on = None)` で畳む。

**`JoinColSource` と USING / NATURAL / FULL**: Join RTE の `columns` は `SELECT *` で展開される順で、USING / NATURAL で併合した列が**先頭に 1 回だけ**来て、その後に左の残り、右の残りが来る。`sources[i]` は `columns[i]` がどこから来るかを示す。

```rust
pub enum JoinColSource { Left(u16), Right(u16), Coalesce(u16, u16) }   // 子の Rte.columns の添字
```

併合列の値は、結合の種類で決まる（確認済み: PG 17 の実測）。

| 結合 | 併合列（USING / NATURAL） | 例（`t(a, b)`, `u(a, c)`） |
|---|---|---|
| INNER | 左の列（`Left(j)`） | `t JOIN u USING (a)`: `columns = [a, b, c]`、`sources = [Left(0), Left(1), Right(1)]` |
| LEFT | 左の列（`Left(j)`） | `t LEFT JOIN u USING (a)`: 同じ。`SELECT a` は `t.a`（`u` に一致が無くても `t.a` の値） |
| RIGHT | **右の列**（`Right(j)`） | `t RIGHT JOIN u USING (a)`: `sources = [Right(0), Left(1), Right(1)]`。`SELECT a` は `u.a` |
| FULL | `Coalesce(l, r)` | `t FULL JOIN u USING (a)`: `sources = [Coalesce(0, 0), Left(1), Right(1)]` |

`t FULL JOIN u USING (a)` に対する展開（アナライザが `Var` の代わりに式を作る。**`Var` は Join RTE を指さない**）:

| SQL | 結果の式 |
|---|---|
| `SELECT a` | `Coalesce([Var(t.a), Var(u.a)])`（1, 2, 3 → `t.a` は 1, 2, NULL、`u.a` は NULL, 2, 3 のとき、`a` は 1, 2, 3） |
| `SELECT t.a, u.a` | `Var(t.a)`、`Var(u.a)`（元の列。NULL になりうる） |
| `SELECT *` | `[Coalesce(t.a, u.a), Var(t.b), Var(u.c)]` |
| `SELECT j.*`（`... AS j`） | 同上 |

- 左右の型が違う（`t(a int4)` と `w(a int2)`）とき、併合列の型は `select_common_type` の結果で、`Rte.columns[i].ty` がそれを持つ。展開した式は、必要な側に `Cast` を付ける（`Coalesce([Var(t.a), Cast(Var(w.a) → int4)])`）。INNER / LEFT / RIGHT でも、選ばれた側の列の型が併合型と違えば `Cast` を付ける。`int2` と `int8` の USING の併合列は `bigint`（確認済み）。
- `NATURAL JOIN` は共通名の列を USING とみなす。共通列が 1 つも無ければ `on: None` の `Inner`（直積。確認済み）。
- この展開があるので、`build` は Join RTE を知らなくてよい。`JoinColSource` は、アナライザが名前解決と `*` の展開に使い、`BoundQuery::validate`（B3）が `Var` が Join RTE を指していないことを検査するときに使う。

#### 3.4.4 DML の Bound

- `BoundUpdate` / `BoundDelete`: `rtable[0]` が対象表（`RteKind::Table`）、`from` は FROM / USING の項目（対象表を含まない）。`filter` と `assignments` の式は `rtable` 全体の `Var` を参照してよい（`UPDATE t SET b = u.c FROM u WHERE t.a = u.a`。確認済み: 通る）。**`SET` の全式は、更新前の行（と FROM の行）に対して評価する**（`SET a = b, b = a` は入れ替え）。`UpdateSource::Default(Some(e))` の `e` は `Var` を含まない。
- `BoundInsert`: `source` は独立した `BoundQuery`（`INSERT ... VALUES` は本体が Values、`DEFAULT VALUES` は FROM なしで targets が空の Select）。`column_map[table 列] = Some(source の列位置)`。`coercions: Some(c)` のとき、`source` は型変換前の値を出し、`c[i]` が `Var { rte: 0, col: i }`（B10）を使って `i` 番目の出力列を対象列の型に直す式。`defaults` / `checks` / `returning` は B9。`overriding` はアナライザが使い切る（`08-sequence-serial.md`）。
- RETURNING の評価に使う行は、INSERT は挿入した行（DEFAULT 適用後）、UPDATE は更新後の行、DELETE は削除した行（いずれも対象表のユーザー列）。`UPDATE ... FROM` の RETURNING が FROM の列を参照するのは M4 では `0A000`（アナライザ）。

#### 3.4.5 `BoundStatement` の補助

```rust
impl BoundStatement {
    /// 行を返す文か（RowDescription を送るか）。Select と Explain は true、Insert / Update / Delete は returning が Some のときだけ
    pub fn returns_rows(&self) -> bool;
}
```

`SELECT FROM t`（列が 0 個）は行を返す文だが `output` が空なので、`output.is_empty()` では判定できない。session はこれを使う。

### 3.5 論理プラン（`planner::logical`）

#### 3.5.1 全ノードの出力列の並び

`LogicalPlan::output_cols()` の定義。**物理プランの出力の並びと 1 対 1 に対応する**（`physicalize` が `ColId → 位置` の表を作るときの唯一の根拠）。

| ノード | 出力列 | 新しく定義する `ColId`（`defines`） |
|---|---|---|
| `Get` | `cols` ++ `system_columns` の `ColId`（`system_columns` の並び順） | `cols`、`system_columns` の `ColId`（すべて新規。表を 2 回参照すれば別の ID） |
| `Values` | `cols` | `cols` |
| `FunctionScan` | `cols`（`generate_series` は 1 列） | `cols` |
| `CteScan` | `cols` | `cols`（CTE 自身のプランの出力とは別の新しい ID） |
| `Filter` | `input` の出力 | なし |
| `Project` | `exprs` の `ColId` の並び | `expr` が `Column(id)`（同じ `id`）でないものの `ColId` |
| `Join`（Inner / Left / Full） | `left` の出力 ++ `right` の出力 | なし |
| `Join`（Semi / Anti） | `left` の出力だけ | なし |
| `Aggregate` | `group_by` の `ColId` の並び ++ `aggs` の `ColId` の並び | `group_by` のうち `Column(id)`（同じ `id`）でないもの、`aggs` のすべて |
| `Distinct` / `Sort` / `Limit` | `input` の出力 | なし |
| `SetOp` | `cols` | `cols` |
| `Result` | `cols`（FROM なしは空） | `cols` |
| `Empty` | `cols`（置き換えた部分木の出力列と同じ ID を使ってよい。§3.5.2 の例外） | `cols` |
| `Insert` / `Update` / `Delete` | 空 | なし |

#### 3.5.2 `ColId` の発行・再利用の規則（02-D4）

1. **発行できるのは `build` だけ**（ルールは新しい `ColId` を作らない）。例外: ルールが `Project` や `Join` を新しく挿入するとき、その `Project` が計算する新しい式（たとえば結合キーの型そろえの `Cast`）に新しい `ColId` が要るなら、`ColumnArena::add` で発行してよい。`Empty` に置き換えるルール（定数畳み込み）は、置き換える部分木の `output_cols()` と同じ `ColId` を `cols` に使う（`defines` はそのままで L1 を保つ。置き換えた部分木は消えるので重複しない）。
2. 同じ値を再び並べるだけの `(id, Column(id))` は**パススルー**で、`id` を再定義しない（`SELECT b FROM t` の `Project` は `b` の `ColId` をそのまま使う）。`Filter` / `Sort` / `Distinct` / `Limit` / `Join` が新しい ID を作らないのも同じ理由。
3. 同じ `ColId` を 1 つのノードの出力に 2 回並べない。`SELECT b, b` の 2 つ目は `(新しい id, Column(b の id))`（コピーを定義する）。`GROUP BY a, a` なども同様に、2 つ目以降は新しい ID。
4. 導出表（`FROM (SELECT ...) s`）の列は、内側のプランの出力 `ColId` をそのまま使う（`Project` を挟まない）。列別名は `Rte.columns` の名前にだけ効き、`ColumnInfo` は内側の名前を保つ（EXPLAIN は PG と違う名前になりうるが、結果は同じ）。インライン展開した CTE（参照が 1 回）も同じ。参照が 2 回以上の CTE は `CteScan` が新しい ID を発行する。
5. `Aggregate` のグループキーが `Var` そのもの（`GROUP BY t.a`）なら、その列の `ColId` を再利用する（パススルー）。式（`GROUP BY a + 1`）は新しい ID。集約の結果は常に新しい ID。
6. 副問い合わせ（`LogicalSubquery`）の中も同じ台帳（`ColumnArena`）から発行する。

#### 3.5.3 `ColumnArena` の命名規則（EXPLAIN・deparse が使う）

`ColumnInfo { name, qualifier, ty, origin }` の決め方。EXPLAIN と deparse は `qualifier.name`（`qualifier` があるとき）か `name` を出す（修飾するかどうか、引用符の付け方は `10-explain-copy-compat.md`）。

| 列の由来 | `name` | `qualifier` | `origin` |
|---|---|---|---|
| `Get` のユーザー列 | `Rte.columns[i].name`（列別名適用後） | `Rte.refname`（別名、無ければ表名） | `Some((表の OID, attnum))` |
| `Get` のシステム列 | `ctid` `xmin` `cmin` `xmax` `cmax` `tableoid` | `Rte.refname` | `None` |
| `Values` | `column1`…（列別名があれば置き換え） | 別名、無ければ `"*VALUES*"` | `None` |
| `FunctionScan` | 列別名、無ければ関数名 | 別名、無ければ関数名 | `None` |
| `CteScan` | CTE の列名（列別名適用後） | 参照の別名、無ければ CTE の名前 | `None` |
| `Project` の計算列 | 対象列の名前（`FigureColname` の結果。`?column?` を含む）。resjunk の ORDER BY 式は `?column?` | `None` | 式が単純な列参照なら元の `origin`、他は `None` |
| `Aggregate` の集約結果 | 関数名（`count` `sum` …） | `None` | `None` |
| `Aggregate` の式のグループキー | `?column?` | `None` | `None` |
| `SetOp` の出力 | 最初の（左の）腕の出力列の名前 | `None` | `None` |
| USING の併合列（`Coalesce` を計算する `Project`） | 併合した列の名前 | `None` | `None` |
| `Result` / `Empty` の列 | 対応する出力列の名前 | `None` | `None` |

DML の RETURNING の列名は `BoundReturning.columns` が持つ（`ColumnArena` を使わない）。

#### 3.5.4 外部結合で NULL になる側の扱い（02-D5）

`Join { kind: Left | Full }` の NULL になりうる側（Left なら右、Full なら両側）の列は、**`ColId` を再発行しない**。結合より上のノードの式で `Column(c)` が現れたら、それは「結合の出力行での `c` の値」（一致しない行では NULL に拡張された値）を指す。

- 結合の下（NULL 側の入力の中）の `Column(c)` は拡張前の値。同じ `ColId` でも、結合の下と上で意味が異なる位置がある。
- ルール（`04`）が式を結合をまたいで動かすときは、次を守る: **結合より上にある、NULL 側の列を参照する述語は、その結合の NULL 側の入力へ押し下げない**（`left join` の `where u.c is null` を `u` の走査に下げると、一致しない行を捨ててしまう）。ON 条件の NULL 側だけを見る述語は NULL 側の入力に下げてよい。外部結合の内部結合化（NULL 側の列に対する strict な述語が上にあるとき）は `04` の規則。
- `validate` は ID の出所（L2）しか見ず、null 拡張の意味は検査しない。意味の誤りはルールの単体テストと slt が拾う（§6）。

#### 3.5.5 DML ノードの入力の形

| ノード | `input` の出力（`output_cols()`） | 作り方（`build`） |
|---|---|---|
| `Insert` | `input_cols` を含む。`input_cols` は挿入元の列（`column_map` が指す位置の順、可視列だけ）。`physicalize` が必要なら `Project` で `input_cols` の並びにそろえる | `source` のプランの上に、`coercions` があれば `Project`（対象列の型へ）を積む |
| `Update` | `old_cols`（対象表のユーザー列、attnum 順）++ `[ctid]` ++ `new_values` の `ColId`（`new_values` と同じ順） | `Project(Filter(Join(Get(対象表, system_columns に Ctid), from の項目...)))`。`Project.exprs` = 各 `old_cols` のパススルー、`ctid` のパススルー、各代入の `(新しい ID, 代入式)`。`Default(None)` は `null_of(列型)`、`Default(Some(e))` は `e` |
| `Delete` | `old_cols` ++ `[ctid]` | `Project(Filter(Join(Get(...), using の項目...)))`。`Project.exprs` はパススルーのみ |

- `Update` ノード自身は式を持たない（位置だけ）。`checks` / `not_null` / `returning` は「新しい行（ユーザー列）」に対する `PhysExpr`（論理プランで唯一 `PhysExpr` が現れる箇所。00 §8）。CHECK は名前の昇順（バイト順）に並べて持つ（M2 の `plan_insert` / `plan_update` と同じ）。
- `Get` の `system_columns` に `Ctid` を必ず入れ、`WHERE` が参照したシステム列（`xmin` など）もここに入る。`Project` が落とす。
- 自己 INSERT ... SELECT・UPDATE の Halloween 問題は、M2 のとおりコマンド ID（スナップショットの `curcid`）で防ぐ。プランの形は関係しない。

#### 3.5.6 相関参照の解決（`build` が守る規則）

`build` は、`Var` → `ColId` の**対応表のスタック**を持つ（§3.4.2 のレベルと同じ数え方）。`Var { levels_up: k }` は、スタックの末尾から `k` 個戻ったレベルの表で引く。**表は、その `SubLink` が評価される「段」の列の対応表**を使う。

- `WHERE`・結合条件・FROM 句の中の `SubLink`: 行の生成の段の表（全 RTE の列 → `Get` などの `ColId`）。
- グループ化より後（`targets` / `having` / ORDER BY）の `SubLink`: **グループ化後の表**。外側の `Var` がグループキー（`group_by` の単純な列参照、または関数従属で足した列）なら、そのグループキーの `ColId`。そうでない列への外側参照は、アナライザが `42803 subquery uses ungrouped column "t.b" from outer query` で拒否している（確認済み）。
- 集約を含むレベルの `targets` の書き換え: 各部分木が `group_by[i].same_as` に一致すれば `Column(グループキーの ColId)`、`Aggregate` は `Column(集約の ColId)`（同じ呼び出しは `same_as` で 1 つにまとめる）、どちらでもない `Var` は関数従属のグループキー。

`LogicalPlan::outer_refs` は、このスタックを使わずに、完成した論理プランから「自分では定義せず参照している列」を導く（§3.5.7）。

#### 3.5.7 補助 API（`planner/logical.rs`。A が置く）

ルール（`04`）と `physicalize` と `validate` が共通に使う走査。

```rust
impl LogicalPlan {
    pub fn output_cols(&self) -> Vec<ColId>;                     // §3.5.1
    /// このノードが定義する列（§3.5.1 の右の列）
    pub fn defines(&self) -> Vec<ColId>;
    /// 子のプラン。左から右（Join / SetOp は left, right）。DML は input
    pub fn children(&self) -> Vec<&LogicalPlan>;
    pub fn children_mut(&mut self) -> Vec<&mut LogicalPlan>;
    /// このノードが直接持つ式（子のプランは含まない。SubLink の中のプランには降りない）
    pub fn exprs(&self) -> Vec<&LExpr>;
    pub fn exprs_mut(&mut self) -> Vec<&mut LExpr>;
    /// この木が自分では定義せずに参照している列: 全ノードの式の Column と、式の中の SubLink の query の outer_refs から、
    /// この木の中で定義された ColId を引いたもの
    pub fn outer_refs(&self) -> BTreeSet<ColId>;
}
impl LogicalSubquery { pub fn outer_refs(&self) -> BTreeSet<ColId>; }     // plan.outer_refs()
impl LExpr {
    /// 式が参照する ColId（SubLink の query が外側から参照する分を含む）
    pub fn free_cols(&self, out: &mut BTreeSet<ColId>);
    /// Column(c) を map[c] に置き換える。SubLink の query の中の外側参照も置き換える。
    /// ColId は文の中で一意（L1）なので、query の中の c は常に外側の c を指し、全体で置換して安全
    pub fn substitute(&self, map: &HashMap<ColId, LExpr>) -> Result<LExpr>;
}
```

### 3.6 物理プラン（`planner::physical`）

#### 3.6.1 全ノードの出力レイアウトと幅

`PhysicalPlan::width(&self, ctes: &[PhysicalPlan]) -> usize`（`ctes` は `CteScan` の幅を引くために要る）。

| ノード | 出力行 | 幅 |
|---|---|---|
| `Result` | `exprs` の値 | `exprs.len()` |
| `Values` | 各行の式の値 | `rows[0].len()` |
| `SeqScan` / `IndexScan` | ユーザー列（attnum 順）++ `system_columns` の値（並び順） | `columns.len() + system_columns.len()` |
| `FunctionScan` | 関数の戻り値 1 列（`generate_series`） | 1 |
| `Filter` / `Sort` / `Unique` / `Distinct` / `Limit` / `Materialize` | 入力と同じ | 入力の幅 |
| `Project` | `exprs` の値 | `exprs.len()` |
| `NestedLoopJoin` / `NestedLoopParam` | Inner / Left: **outer ++ inner**（outer が結合の左、inner が右）。Semi / Anti: outer だけ | `outer_width + inner_width`（Semi / Anti は `outer_width`） |
| `HashJoin` | Inner / Left / Full: **left ++ right**。Semi / Anti: left だけ。`build_is_left` は出力の並びに影響しない | `left_width + right_width`（Semi / Anti は `left_width`） |
| `Aggregate` | 集約の結果（`aggs` の並び） | `aggs.len()` |
| `HashAggregate` / `GroupAggregate` | `keys` の値 ++ `aggs` の結果 | `keys.len() + aggs.len()` |
| `Append` | 各入力と同じ（全入力が同じ幅） | `inputs[0]` の幅 |
| `HashSetOp` | 左（= 右）と同じ | 左の幅 |
| `CteScan` | `ctes[cte]` の出力 | `ctes[cte]` の幅 |
| `Insert` / `Update` / `Delete` | `returning` の式の値（無ければ行を出さない） | `returning` の式の数、無ければ 0 |

- `NestedLoopJoin` / `NestedLoopParam` では、**outer が左、inner が右**で固定（外部結合の意味のため。入れ替えない）。LEFT 結合の NULL 拡張は inner の幅（`inner_width`）の NULL を右に足す。`kind = Full` は無い（HashJoin だけ）。
- 論理 `Join` を物理化するときの `ColId → 位置` の表は、Join では `left.output_cols() ++ right.output_cols()`（`HashJoin` の `left_keys` は左だけ、`right_keys` は右だけの表）。
- `Empty` の物理形は `Result { exprs: 型つき NULL × 列数, one_time_filter: Some(false) }`（02-D13）。

#### 3.6.2 式の文脈行（`Local(i)` が指す行）

`PhysicalPlan::exprs_with_width(&self, ctes) -> Vec<(&PhysExpr, usize)>` が、ノードが**直接持つ式**とその文脈行の幅を返す（`validate` と `uses_params` が共有する唯一の情報源）。

| ノード | フィールド | 文脈行 | 幅 |
|---|---|---|---|
| `Result` | `exprs`、`one_time_filter` | 空行 | 0 |
| `Values` | 各行の各式 | 空行 | 0 |
| `SeqScan` | `filter` | 走査の出力行 | `columns.len() + system_columns.len()` |
| `IndexScan` | `keys` の式 | 空行 | 0 |
| | `filter` | 走査の出力行 | 同上 |
| `FunctionScan` | `args` | 空行 | 0 |
| `Filter` | `predicate` | 入力行 | 入力の幅 |
| `Project` | `exprs` | 入力行 | 入力の幅 |
| `Sort` | `keys[i].expr` | 入力行 | 入力の幅 |
| `Limit` | `limit` / `offset` | 空行 | 0 |
| `NestedLoopJoin` | `join_filter` | outer ++ inner（Semi / Anti でも） | `outer_width + inner_width` |
| `NestedLoopParam` | `params[i].1` | outer の行 | `outer_width` |
| | `join_filter` | outer ++ inner | 同上 |
| `HashJoin` | `left_keys` | left の行 | `left_width` |
| | `right_keys` | right の行 | `right_width` |
| | `residual` | left ++ right | `left_width + right_width` |
| `Aggregate` / `HashAggregate` / `GroupAggregate` | `keys`、`aggs[i].args`、`aggs[i].filter` | 入力行 | 入力の幅 |
| `Insert` | `defaults` | 空行 | 0 |
| | `checks`、`returning` | 対象表の行 | 表のユーザー列数（`column_map.len()`） |
| `Update` | `checks`、`returning` | 更新後の行 | `n_user_cols` |
| `Delete` | `returning` | 削除した行 | `n_user_cols` |
| SubLink の `SubPlanDef` | `params[i].1`、`strategy` の `probe_keys` | SubLink が置かれたノードの、その式の文脈行 | その式の文脈の幅 |
| | `test`（`SubLinkOutput` を含む） | 文脈行（`Local`）+ 副問い合わせの現在の行（`SubLinkOutput`） | 文脈の幅 + 副問い合わせプランの幅 |
| | `Hashed.build_keys` | 副問い合わせプランの出力行 | 副問い合わせプランの幅 |

#### 3.6.3 `SubPlanDef` と `ParamId` の割り当て

`SubPlanDef` の各フィールドの意味: `plan`（副問い合わせの物理プラン。幅は `LogicalSubquery.output.len()`）、`kind`、`test`（Any / All のみ）、`params`（実行前に**文脈行から計算して `ctx.params` に設定する値**）、`strategy`、`explain`。

`physicalize` が SubLink 1 つ（`LogicalSubquery q`、評価される文脈の「`ColId → PhysCol`」の表 `scope`）を物理化する手順:

```text
outer = q.outer_refs()                         // BTreeSet<ColId>（昇順）
inner_scope = {}                               // q.plan を物理化するときの「外側の ColId → PhysCol」
params = []
for c in outer:
    match scope[c]:
        Local(i)  → pid = alloc_param()?        // 新しい ParamId（54000 を超えたら）
                    params.push((pid, Column(Local(i)) : ty(c)))
                    inner_scope[c] = Param(pid)
        Param(p)  → inner_scope[c] = Param(p)    // 入れ子: 外側が束縛済みの Param をそのまま使う（再束縛しない）
        なし      → Error::internal("outer ColId not in scope")   // L2 違反
plan = physicalize(q.plan, outer_scope = inner_scope)
strategy = if outer.is_empty() { InitOnce か Hashed（04 が判定） } else { Rescan }
id = alloc_subplan()?                           // 新しい SubPlanId。plan と test の物理化の後（02-D6）
```

- **`SubPlanId` / `ParamId` の採番順は、物理化が子を先に物理化してからノード自身の式を物理化する順（後行順）**で、0 から付ける。決定的で、plan_golden（`04`）が安定する。EXPLAIN の表示番号（`SubPlan 1`）への対応は `10` が決める。
- 副問い合わせの中の `NestedLoopParam` が束縛する `ParamId` も同じ採番器から取る（文全体で一意）。
- `PhysicalQuery.n_params` は採番した `ParamId` の数。
- **CTE の本体**（`PhysicalQuery.ctes[i]`）は外側の列を参照しない（P8。参照する CTE は `MATERIALIZED` でも `0A000`、`03`）。

#### 3.6.4 `uses_params`・`children`・番号付け

```rust
impl PhysicalPlan {
    /// 00 のとおり。木のどこかのノードの式に PhysCol::Param がある、または SubLink が 1 つでもある（02-D7）。
    /// NestedLoopParam.params の式の中の Param も数える
    pub fn uses_params(&self) -> bool;
    /// 子のプラン。実行の呼び出し順 = EXPLAIN の表示順 = 番号付けの順:
    /// 単項: [input]、NestedLoop*: [outer, inner]、HashJoin: [probe, build]（build_is_left なら [right, left]、そうでなければ [left, right]）、
    /// Append: inputs の順、HashSetOp: [left, right]、葉: []。SubPlan と CTE は含めない
    pub fn children(&self) -> Vec<&PhysicalPlan>;
    pub fn width(&self, ctes: &[PhysicalPlan]) -> usize;               // §3.6.1
    pub fn exprs_with_width(&self, ctes: &[PhysicalPlan]) -> Vec<(&PhysExpr, usize)>;   // §3.6.2
}
impl PhysExpr { pub fn uses_params(&self) -> bool; }
impl PhysicalQuery {
    /// テスト・EXPLAIN 用: 副問い合わせも CTE も持たない問い合わせ
    pub fn single(root: PhysicalPlan, output: Vec<OutputColumn>) -> PhysicalQuery;
}
```

**計測のための通し番号**: 先行順（`children()` の順に降りる）で、根のプランに 0 から、次に `subplans[0].plan`、`subplans[1].plan`…、最後に `ctes[0]`、`ctes[1]`… の順に続けて振る。`executor::instrument`（`10`）は `build` が Executor を作る順にこの番号を割り当てる。

#### 3.6.5 `ExplainNode` が `PhysicalPlan` と同形であること

- `ExplainNode.children` の**先頭 `plan.children().len()` 個**は `plan.children()` と同じ順序・1 対 1（`label` は `None`）。その後ろに、このノードの式が持つ SubLink（`label = "InitPlan n"` / `"SubPlan n"`）と、根なら CTE（`label = "CTE x"`）の子が付く。
- PG の `Hash` ノード（`Hash Join` の build 側の子の間に出る）は、`explain/format.rs` が `HashJoin` の build 側の子を印字するときに**合成**する。`ExplainNode` の木には入れない（同形を保つ）。
- `ExplainNode` は物理化の最中に作る（02-D12）。`physicalize` は `LExpr` と `ColumnArena` を持っているので、`deparse` で列名つきの式の文字列を作れる。`planner/explain_tree.rs` は物理化の各関数から呼ばれる部品（ノードの `title` / `details` / `output` を作る）で、`explain` を持たない（`want_explain = false`）ときは何もしない。

### 3.7 Executor の契約

署名は 00 §10 のとおり。ここでは意味を決める。

#### 3.7.1 `next` / `rewind` の意味

共通の規則:

- `next` は行を 1 つ返し、尽きたら `Ok(None)`。尽きた後に `next` を呼んでも `Ok(None)`（再び読み始めない）。
- `rewind` は**初期状態に戻す**。`ctx.params` が前回から変わっていることがある。`rewind` は**未開始のノードにも呼べ**、その場合は何もしなくてよい（02-D16）。`rewind` 自体は行を読まない（重い処理は次の `next` まで遅らせる）。
- ノードが**溜める系**（Sort・Materialize・Aggregate・HashAggregate・HashJoin の build 側・HashSetOp）のとき、`rewind` で**溜めた結果を読み直すか、作り直すか**を、`reusable` で決める。`reusable` は、**`executor::build` が各ノードを作る時点で、そのノード（自分の式を含む部分木）の `PhysicalPlan::uses_params()` が false か**で決める。HashJoin の build 側は、build 側の子の `uses_params()` と build 側のキー式の `PhysExpr::uses_params()` が両方 false か。
- 作り直すときは、溜めたバイト数を `ctx.mem.release` してから子を `rewind` する。ノードの `Drop` では `release` しない（問い合わせの終わりに `MemBudget` ごと捨てる）。
- `reusable == true` で読み直すとき、ノードの外から見える結果（行の並びを含む）は前回と同じでなければならない。

| ノード | 持つ状態 | `next` | `rewind` |
|---|---|---|---|
| `Result` | 出力済みか | 初回に `one_time_filter` を評価し、真なら `exprs` を評価して 1 行（偽 / NULL なら 0 行） | 出力済み = false（式は再評価される） |
| `Values` | 次の行の位置 | 位置の行の式を評価して返す（SubLink を含みうる） | 位置 = 0 |
| `SeqScan` | `Option<HeapScan>` | `check_interrupts`。初回に `begin_scan(rel, snapshot)`。行を読み `filter` が真のものだけ返す | `scan = None`（次の `next` で同じスナップショットで先頭から） |
| `IndexScan` | スキャン、評価済みのキー | `check_interrupts`。初回に `keys` を評価（`Param` を読む）→ `begin_scan` → TID ごとにヒープを引き可視性判定 → `filter`。`direction` に従う | `scan = None`、キーを捨てる（次の `next` で再評価） |
| `FunctionScan` | 現在値 | `check_interrupts`。初回に `args` を評価して `generate_series` の状態を作る | 状態を捨てる（`args` を再評価） |
| `Filter` | なし | 子の `next` を、`predicate` が真になるまで繰り返す | 子を `rewind` |
| `Project` | なし | 子の行を `exprs` で変換 | 子を `rewind` |
| `Sort`（溜める） | 整列済みの行、位置 | 初回に子を読み切り（行ごとに `check_interrupts`）、キー式を評価して整列（安定ソート） | `reusable` なら位置 = 0。そうでなければ溜めた行を捨て（`release`）、子を `rewind`、次の `next` で作り直す |
| `Unique` | 前の行のキー | 先頭 `key_cols` が前の行と変わった最初の行だけ返す | 前のキー = なし、子を `rewind` |
| `Distinct` | 見た行の集合 | 初めて見る行だけ返す（NULL どうしは等しい） | 集合を空にし（`release`）、子を `rewind` |
| `Limit` | 残り skip / emit | 初回に offset → limit の順に評価（式は `Param` を読みうる）。0 件になったら子を読まず `None` | 状態を捨てる（式を再評価）、子を `rewind` |
| `Materialize`（溜める） | 行、位置 | 初回に子を読み切って溜める（`check_interrupts`・`charge`） | `reusable` なら位置 = 0。そうでなければ破棄して子を `rewind` |
| `NestedLoopJoin` | 現在の outer 行、一致したか | outer を 1 行取るたびに、**inner を `rewind`**（未開始なら何もしない。02-D16）してから内側を読み、`join_filter` を評価。Semi は最初の一致で次の outer へ | outer 行 = なし。outer を `rewind`（inner は次の outer 行で `rewind` される） |
| `NestedLoopParam` | 現在の outer 行、一致したか | outer を 1 行取るたびに、`params` の式を outer 行で**全部評価してから**`ctx.params` に設定し、inner を `rewind`、`join_filter` を評価 | 同上 |
| `HashJoin`（build 側を溜める） | ハッシュ表、build 行ごとの一致印、probe の状態、未一致の build 行の出力段階 | 初回に build 側を全部読んで表を作る（NULL を含むキーは表に入れない。外部結合のために別に持つ）。probe 行ごとに表を引く。RIGHT / FULL 相当は probe 終了後に一致印のない build 行を出す | build 側が `reusable` なら**表は残し**、一致印・probe の状態・出力段階だけ初期化。そうでなければ表を捨てて build 側を `rewind`。probe 側は常に `rewind` |
| `Aggregate`（溜める） | 結果の 1 行、出力済みか | 初回に子を読み切って 1 行（入力が空でも 1 行） | `reusable` なら出力済み = false。そうでなければ結果を捨て、子を `rewind` |
| `HashAggregate`（溜める） | グループの表、出力位置 | 初回に子を読み切って表を作り、出力 | `reusable` なら出力位置 = 0。そうでなければ破棄して子を `rewind` |
| `GroupAggregate` | 現在のグループ | 子は `keys` で整列済み。グループが変わったら出力（溜めない） | 状態を捨て、子を `rewind` |
| `Append` | 現在の入力の添字 | 入力を順に | 添字 = 0、**全入力を `rewind`**（未開始のものは何もしない） |
| `HashSetOp`（溜める） | 件数の表、出力位置 | 左右を読み切って数え、`op` / `all` に従って出力 | `reusable` なら出力位置 = 0。そうでなければ作り直し |
| `CteScan` | `Arc<Vec<Row>>`、位置 | 初回に `ctx.ctes` から行を得る（最初の `CteScan` が CTE のプランを実行して溜める。`charge`）。各 `CteScan` が自分の位置で読む | 位置 = 0（CTE は作り直さない。P8） |
| `Insert` / `Update` / `Delete` | 件数、完了 | 入力を読み切って書く（RETURNING があれば行を返す） | `Error::internal("rewind is not supported for DML nodes")`（根にだけ現れる） |

`executor::build` は、`PhysicalPlan` の各ノードを上の表のノードに 1 対 1 で対応させる（`PhysicalPlan::Unique` → `UniqueExec` のように）。M1〜M3 のノードも同じ表に従う（`DistinctExec` は M1 の実装を `rewind` つきに直すだけ）。

#### 3.7.2 `ExecCtx` の所有と借用

```rust
// executor/mod.rs に足すもの（ExecCtx のフィールドは 00 §10）
pub struct ExecEnv<'a> {
    pub catalog: &'a dyn CatalogReader, pub storage: &'a dyn TableStore, pub indexes: &'a dyn IndexStore,
    pub snapshot: &'a Snapshot, pub session: &'a SessionInfo, pub runtime: &'a dyn RuntimeInfo,
    pub interrupts: &'a InterruptFlag, pub type_env: &'a TypeEnv<'a>,
    /// yuzhu.query_mem_limit
    pub mem_limit: usize,
}
impl<'a> ExecCtx<'a> {
    /// params を n_params 個の NULL で、mem を mem_limit で、subplans / ctes を query から作る
    pub fn new(env: ExecEnv<'a>, txn: &'a mut Transaction, query: &'a PhysicalQuery) -> ExecCtx<'a>;
}
// EvalCtx に type_env を足す（日時・数値の入出力が TypeEnv を使う）
pub struct EvalCtx<'a> { pub session: &'a SessionInfo, pub catalog: &'a dyn CatalogReader,
                         pub runtime: &'a dyn RuntimeInfo, pub type_env: &'a TypeEnv<'a> }
```

| フィールド | 型 | 所有 | 注意 |
|---|---|---|---|
| `catalog` `storage` `indexes` `session` `runtime` `interrupts` `type_env` `snapshot` | 共有参照 | 借用（session が作る） | ライフタイム `'a` は文の実行の間 |
| `txn` | `&'a mut Transaction` | 借用 | 書き込みは `write_ctx()` 経由だけ |
| `query` | `&'a PhysicalQuery` | 借用 | `let q: &'a PhysicalQuery = ctx.query;` で参照そのものを `Copy` すると、`ctx` の可変借用と独立に `q.subplans[i]` を引ける（SubPlan の評価で使う） |
| `params` | `Vec<Datum>` | `ctx` が所有 | `ParamId` の添字。`NestedLoopParam` と SubPlan の評価が書く |
| `mem` | `MemBudget` | `ctx` が所有 | 内部は `Cell`（`&self` で `charge` できる） |
| `subplans` | `SubPlanStates` | `ctx` が所有 | §3.7.3。`take` / `put` で子 Executor を借りる |
| `ctes` | `CteStates` | `ctx` が所有 | 各 CTE の行を `Arc<Vec<Row>>` で保持 |

- `Executor` の構造体はライフタイムを持たない（M1 から変えない）。`ctx` は `next` / `rewind` の引数で毎回渡す。
- `executor::build(&query.root)` は、プランの式を**複製して**ノードに持たせる（M1 と同じ。M5 の prepared statement では `Arc<PhysicalQuery>` から作る）。
- `eval(expr, row, ctx: &mut ExecCtx)`: 関数・演算子の呼び出しでは `ctx.eval_ctx()`（共有参照）を一時的に作る。`SubLink` だけが `ctx` を可変に使う。
- 副問い合わせの `test` の評価用に、`eval` の内部は「外側の行 `row`」と「副問い合わせの現在の行 `sub: Option<&Row>`」を持つ（`SubLinkOutput(i)` は `sub[i]`。`sub` が無いときに現れたら `Error::internal`）。公開関数:

```rust
// executor/eval.rs
pub fn eval(expr: &PhysExpr, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Datum>;
pub fn eval_pred(expr: &PhysExpr, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Option<bool>>;
pub fn eval_const(expr: &PhysExpr, row: &Row, ctx: &EvalCtx<'_>) -> Result<Datum>;     // SubLink・Param が現れたら Error::internal
/// executor/subplan.rs が使う: SubLinkOutput を解決できる評価
pub(crate) fn eval_with_sub(expr: &PhysExpr, row: &Row, sub: &Row, ctx: &mut ExecCtx<'_>) -> Result<Datum>;
```

#### 3.7.3 SubPlan の評価（`executor/subplan.rs`）

```rust
pub struct SubPlanStates { slots: Vec<SubPlanSlot> }                 // 添字 = SubPlanId
struct SubPlanSlot { exec: Option<BoxedExecutor> /* None = 実行中（take 済み） */, state: SubPlanState }
enum SubPlanState {
    Rescan,
    InitOnce(Option<InitValue>),                                      // None = まだ実行していない
    Hashed(Option<HashedSet>),
}
pub enum InitValue { Scalar(Datum), Exists(bool), Rows(Vec<Row>) }
pub struct HashedSet { set: HashSet<HashKey>, rows: Vec<Row>, null_rows: Vec<usize> /* キーに NULL を含む行の添字 */ }

impl SubPlanStates { pub fn new(query: &PhysicalQuery) -> SubPlanStates; /* 各 SubPlanDef.plan から executor::build */ }
/// SubLink 1 つを評価する。row は SubLink が置かれた式の文脈行
pub fn eval_sublink(id: SubPlanId, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Datum>;
```

子 Executor の借用手順（`eval` が `&mut ExecCtx` を取り、`unsafe` を使わない方法）:

```rust
pub fn eval_sublink(id: SubPlanId, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Datum> {
    let q: &PhysicalQuery = ctx.query;                       // &'a。ctx の借用と独立
    let def = &q.subplans[id.0 as usize];
    let mut exec = ctx.subplans.take_exec(id)?;              // 実行中（再入）なら Error::internal("SubPlan re-entered")
    let r = run_strategy(def, id, &mut exec, row, ctx);       // 以下の遷移
    ctx.subplans.put_exec(id, exec);                         // 成功・失敗どちらでも戻す
    r
}
```

**状態遷移**（`strategy` ごと。`kind` は `def.kind`、`test` は `def.test`）:

| 戦略 | 状態 | 評価 |
|---|---|---|
| `Rescan` | なし | ① `def.params` の式を `row` で**すべて評価してから**`ctx.params` に設定する。② `exec.rewind(ctx)`。③ 下の「種類ごとの畳み込み」で結果を出す |
| `InitOnce` | `None` → `Some(InitValue)` | 初回（`None`）: `params` は空。`exec.rewind(ctx)` して実行し、Scalar → `Scalar(値)`（0 行は NULL、2 行目があれば `21000`）、Exists → `Exists(bool)`、Any / All → `Rows(全行)`（`charge`）を保持して `Some`。2 回目以降: 保持した値を返す（Any / All は `Rows` を `test` で畳む） |
| `Hashed` | `None` → `Some(HashedSet)` | 初回: 実行して全行を `rows` に溜め、`build_keys`（副問い合わせの行に対する式）がすべて非 NULL の行を `set` に入れる。NULL を含む行の添字を `null_rows` に入れる（`charge`）。評価: `probe_keys` を `row` で評価。**全部非 NULL**なら `set` を引き、見つかれば true。見つからず `null_rows` が空なら false。見つからず `null_rows` があるとき、または `probe_keys` に NULL があるとき（表が空なら false）は、該当する行（`null_rows` の行、または全行）で `test` を `eval_with_sub` して三値で畳む（下） |

**種類ごとの畳み込み**（`Rescan` の ③ と、`InitOnce` の `Rows`、`Hashed` のフォールバック。いずれも 1 行ずつ `next` して行う）:

| `kind` | 結果 |
|---|---|
| `Scalar` | 最初の行の列 0（0 行なら NULL）。**2 行目が存在したら `21000 more than one row returned by a subquery used as an expression`**（2 行目を読んで確かめる。確認済み） |
| `Exists` | 1 行でも出たら true（**最初の行で止める**。残りは読まない。次の評価の `rewind` で捨てる） |
| `Any` | 各行で `test` を `eval_with_sub(row, sub)`。`true` が出たら**その場で true を返す**。NULL が 1 つでもあれば保持。最後まで true が無ければ、NULL があれば NULL、無ければ false。0 行は false |
| `All` | 各行で `test`。`false` が出たら**その場で false を返す**。NULL があれば保持。最後まで false が無ければ、NULL があれば NULL、無ければ true。0 行は true |

三値の確認（PG 17 実機）: `1 <> ALL (SELECT NULL)` は NULL、`1 = ANY (SELECT NULL)` は NULL、`1 = ANY (SELECT 1 UNION ALL SELECT NULL)` は true、`2 <> ALL (SELECT 1 UNION ALL SELECT NULL)` は NULL、`1 <> ALL (空)` は true、`NULL = ANY (空)` は false。`NOT IN` は `Not(Any)` なので、この三値の否定がそのまま `NOT IN` の意味になる（`x NOT IN (NULL を含む集合)` は一致が無ければ NULL）。

- 実行途中で止めた Executor（Exists・Any・All の早期終了）は、次の評価の `rewind` で捨てる。
- `exec` を `take` している間（`None`）に同じ `SubPlanId` を評価しようとしたら再入で、`Error::internal`（P3 により、正しいプランでは起きない）。
- InitOnce の `Rows` と Hashed の `rows` は、問い合わせの終わりまで保持する（`MemBudget` の対象。上限を超えたら `53200`）。

#### 3.7.4 `check_interrupts` の置き場所

`ctx.check_interrupts()` は `InterruptFlag::check`（停止・キャンセル・`statement_timeout`）。次の規則で置く。

1. **葉のノード**（`SeqScan` `IndexScan` `FunctionScan` `Values` `Result` `CteScan`）は、`next` の**呼び出しごとに 1 回**呼ぶ。
2. **子の `next` を 1 回の反復ごとに呼ぶループ**（`Filter` が条件に合わない行を読み捨てるループ、`Sort` / `Materialize` / `HashAggregate` / `HashJoin` の build の読み込み、`Distinct` / `Unique` / `Limit` の skip）は、葉が検査するので、ループ側は呼ばなくてよい。
3. **子の `next` を呼ばずに回る長いループ**（溜めた行の上を回るもの。`NestedLoopJoin` の inner が `Materialize` から読み直される内側ループ、`HashJoin` の同じキーの連鎖を回るループ、`HashAggregate` / `Sort` の出力、SubPlan の `Rows` / `Hashed` の行を回るループ）は、**反復ごとに**呼ぶ。
4. DML ノードは入力 1 行ごとに呼ぶ（M2 のとおり）。
5. 式の評価（`eval`）の中では呼ばない（`pg_sleep` などは `runtime.check_interrupts` を使う。M3）。
6. 検査に失敗したら（`57014` / `57P01`）、状態を壊さずにそのまま `Err` を返す（文全体が中断される）。

テスト: ノードの種類ごとに「大きな入力に対して `request_cancel` した状態で `next` を 1 回呼ぶと、その `next` の中で `57014` を返す」ことを確かめる共通のハーネス（§6）。

#### 3.7.5 `MemBudget`（`05-executor.md` が詳細を決める。ここは rewind との接点だけ）

- 溜める系ノードは、行を溜めるたびに `charge(estimate_row_bytes(row))`。作り直し・破棄の前に溜めた合計を `release`。
- `reusable` で読み直すときは `charge` も `release` もしない。
- `Hashed` / `InitOnce` / `CteScan` の溜めた行は、文の終わりまで解放しない。

---

## 4. 処理の流れ

### 4.1 session から executor までの呼び出し順

00 §5 を `Session::run_under_barrier`（`m2.md` §5.2 の手順 3〜6）の中身として詳しくする。ストレージバリア（共有）・スナップショット・`StatementCatalog` の組み立て（手順 3〜5）は M2・M3 のまま。

```text
run_under_barrier(stmt):
  generation = db.cache.generation(); snap = txn_mgr.snapshot(txn.xid, txn.cid); catalog = StatementCatalog { .. }       // M2 §5.2 の 3〜5
  bound = analyzer::analyze(stmt, &catalog)?                     // BoundStatement。デバッグビルドでは BoundQuery::validate を実行（B1〜B11）
  match bound:
    Checkpoint                         → Error::internal（バリアの外で処理済み）
    Ddl(d)                             → ddl::execute(&mut DdlCtx, d)（M4 の途中までは CreateTable / DropTable を session が処理。§5）
    Copy(c)                            → copy::begin(..)（10）
    Explain(e)                         → explain::run(..)（10。内部で下の plan を want_explain = true で呼ぶ）
    Select / Insert / Update / Delete  → 以下
  ① type_env = settings.type_env(zones);  ps = settings.planner_settings()
  ② env = PlanEnv { catalog: &catalog, storage: &**cluster.storage(), settings: &ps, type_env: &type_env, want_explain: false }
  ③ query: PhysicalQuery = planner::plan(&bound, &env)?
        ├ planner::build::build(&bound, &env) → LogicalQuery               // Bound → 論理（Var → ColId）。debug: LogicalQuery::validate
        ├ planner::rules::run(logical, &env)  → LogicalQuery               // 固定順のルール。各ルールの後に debug: validate
        └ planner::physicalize::physicalize(logical, &env) → PhysicalQuery // ColId → PhysCol、SubPlanId / ParamId の採番、ExplainNode。debug: PhysicalQuery::validate
  ④ if bound.returns_rows() { out.columns = Some(column_descs(&query.output, &catalog)) }          // RowDescription
  ⑤ exec = executor::build(&query.root)
  ⑥ ctx = ExecCtx::new(ExecEnv { catalog: &catalog, storage, indexes, snapshot: &snap, session: &info, runtime, interrupts, type_env: &type_env,
                                 mem_limit: settings.query_mem_limit() }, &mut self.txn, &query)
  ⑦ while let Some(row) = exec.next(&mut ctx)?:  行を output_text で文字列にして out.rows に積む（row.len() == query.output.len() は P11 で保証済み）
  ⑧ タグ: Select → "SELECT n"（n は out.rows.len()）、Insert → "INSERT 0 n"、Update → "UPDATE n"、Delete → "DELETE n"（n = exec.rows_affected()）
  （以降は M2 §5.2 の 7〜10: バリア解放 → assert_no_pins → command_counter_increment / コミット or アボート）
```

- P0 の間は、`settings.rs`（S の担当）に触れずに、`TypeEnv::default()`、`PlannerSettings::default()`、`DEFAULT_QUERY_MEM_LIMIT`（00 §15.1）を使う。S が `Settings::{type_env, planner_settings, query_mem_limit}` に置き換える。
- 手順 ⑤⑥ の `query`・`exec`・`ctx` の宣言順は `query` → `exec` → `ctx`（`ctx` が `&query` を借りる。`exec` はライフタイムを持たないので `query` を借りない）。
- `column_descs` は `&[OutputColumn]` を受け取る形に変える（M2 は `&BoundSelect`）。`ty.oid == unknown` は text として報告する（M1 のまま）。
- 結果の行は `out.rows` に溜めてから、バリアを解放した後に sink へ送る（M2 §2 規約 10 のまま）。
- 副問い合わせ（SubPlan）・CTE は `ExecCtx` の `subplans` / `ctes` が持つ。session は意識しない。
- PostgreSQL と同じく、**1 つの文の中のすべての副問い合わせは同じスナップショット・同じコマンド ID** で読む（`ExecCtx.snapshot` を共有する）。

### 4.2 SELECT の 3 段階の例

テーブル `t(a int4, b int4)`、`u(a int4, c int4)`。

**例 1: 単一表**（M1 から続く形。P0 の完了条件が通す範囲）

```sql
SELECT b, a + 1 AS c FROM t WHERE a > 1 ORDER BY b DESC LIMIT 2
```

```text
-- Bound
BoundQuery {
  ctes: [], limit: Some(Literal 2), offset: None,
  order_by: [ { target: 0, descending: true, nulls_first: true } ],
  columns: [ b (table_oid = t, attnum = 2), c ],
  body: Select(BoundSelect {
    rtable: [ Rte { Table t, refname "t", columns [a:int4, b:int4] } ],
    from: [ Scan(RteId(0)) ],
    filter: Operator(>, [Var(0,0,0), Literal 1]),
    group_by: [], having: None, has_agg: false,
    targets: [ Var(0,1,0), Operator(+, [Var(0,0,0), Literal 1]) ], n_visible: 2,
    distinct: None })
}

-- 論理プラン（build の直後）。#n は ColId。b は Get の #1 を Project がそのまま通す（パススルー）。c は新しい #2
Limit limit=2
  Sort [#1 DESC NULLS FIRST]
    Project [#1, #2 := (#0 + 1)]
      Filter (#0 > 1)
        Get t [#0 a, #1 b]

-- 物理プラン。@n は PhysCol::Local(n)
Limit limit=2
  Sort [@0 DESC NULLS FIRST]
    Project [@1, (@0 + 1)]
      Filter (@0 > 1)
        SeqScan t              -- 出力 @0 a, @1 b
```

P0 の物理化は `Filter` を `SeqScan.filter` に畳まず、M1 の形（`Filter` ノード）を保つ。畳み込みは `04` の仕事。`SELECT * FROM t` の恒等 `Project` の省略（M1 の `is_identity`）と、FROM なしの `Project(Result)` を `Result { exprs }` にする畳み込みは P0 の物理化が行う。

**例 2: 結合**（`ColId` と位置の違い）

```sql
SELECT t.a, u.c FROM t JOIN u ON t.a = u.a WHERE u.c > 5
```

```text
-- Bound: rtable = [t, u, Join{ Inner, left: 0, right: 1, sources: [Left(0), Left(1), Right(0), Right(1)] }]、from = [ Join { rte: 2, Inner, Scan(0), Scan(1), on: (Var(0,0,0) = Var(1,0,0)) } ]
-- 論理（build の直後）
Project [#0, #3]
  Filter (#3 > 5)
    Join Inner on (#0 = #2)
      Get t [#0 a, #1 b]
      Get u [#2 a, #3 c]

-- 論理（ルールの後: 述語の押し下げ・結合キーの抽出・列の刈り込み。04）
Project [#0, #3]
  Join Inner on (#0 = #2)
    Project [#0]                                    -- 刈り込み: t の b（#1）は上で使わない
      Get t [#0 a, #1 b]
    Filter (#3 > 5)                                 -- 押し下げ: u だけを参照する述語は u の側へ
      Get u [#2 a, #3 c]

-- 物理
Project [@0, @2]                                    -- 結合の出力は左 ++ 右: @0 t.a, @1 u.a, @2 u.c
  HashJoin Inner left_keys=[@0] right_keys=[@0]     -- left_width 1、right_width 2
    Project [@0]
      SeqScan t                                     -- 出力 @0 a, @1 b
    Filter (@1 > 5)
      SeqScan u                                     -- 出力 @0 a, @1 c
```

**例 3: 相関副問い合わせ**（`Var` の `levels_up`、`ColId` の外側参照、`Param`）

```sql
SELECT a FROM t WHERE b > (SELECT max(c) FROM u WHERE u.a = t.a)
```

```text
-- Bound（内側の t.a は Var { rte: 0, col: 0, levels_up: 1 }）
Filter: Operator(>, [Var(0,1,0), SubLink { Scalar, test: None, query: BoundQuery { Select max(c) FROM u WHERE u.a = Var(0,0,1) } }])

-- 論理。#0 は外側の t.a。副問い合わせのプランは #0 を自分では定義しない（outer_refs = {#0}）
Project [#0]
  Filter (#1 > SubLink Scalar { Aggregate [#4 := max(#3)] ← Filter (#2 = #0) ← Get u [#2 a, #3 c]; output = [#4] })
    Get t [#0 a, #1 b]

-- 物理
Project [@0]
  Filter (@1 > SubPlan#0)
    SeqScan t
SubPlan#0 { kind: Scalar, strategy: Rescan, params: [$0 := @0], test: None }
  Aggregate [max(@1)]
    Filter (@0 = $0)                                -- $0 は PhysCol::Param(ParamId(0))
      SeqScan u [a, c]
```

### 4.3 INSERT ... SELECT・UPDATE・DELETE の変換

**INSERT**

| 構文 | Bound | 論理 | 物理 |
|---|---|---|---|
| `INSERT INTO t VALUES (1, 'a'), (2, 'b')` | `source`: 本体が `Values { rows, types }`。`column_map` | `Insert { input: Values { rows: LExpr, cols: [#0, #1] }, input_cols: [#0, #1], .. }` | `Insert { input: Values { rows }, column_map, .. }` |
| `INSERT INTO t SELECT a, b FROM s` | `source`: 本体が Select（targets は対象列の型に直し済み）。`coercions: None` | `Insert { input: Project(Get s), input_cols: .. }` | `Insert { input: Project(SeqScan s), .. }` |
| `... SELECT ... ORDER BY / DISTINCT / LIMIT` | `coercions: Some(c)`。`source` は型変換前の値 | `Insert { input: Project[c の各式](Limit(Sort(..))) }`（型変換は `Limit` の上。PG と同じ） | 同左 |
| `INSERT INTO t DEFAULT VALUES` | 本体が FROM なし・targets なしの Select。`column_map` がすべて `None` | `Insert { input: Result { cols: [] }, input_cols: [] }` | `Insert { input: Result { exprs: [] } }` |

`defaults` / `checks` / `returning` は §4.4 で `PhysExpr` にして `Insert` ノードに持たせる。CHECK は名前のバイト順に並べる（M2）。

**UPDATE**（M2 の `Update.assignments` を廃止し、代入式を入力の `Project` に移す。§3.5.5）

```sql
UPDATE t SET b = u.c + 1 FROM u WHERE t.a = u.a
```

```text
-- 論理（t の Get は system_columns に ctid を持つ。#0 a, #1 b, #2 ctid）
Update { old_cols: [#0, #1], ctid: #2, new_values: [(1 /* b = attnum 2 - 1 */, #5)] }
  Project [#0, #1, #2, #5 := (#4 + 1)]
    Filter (#0 = #3)
      Join Inner on None
        Get t [#0 a, #1 b] sys[ctid #2]
        Get u [#3 a, #4 c]

-- 物理（Update ノードは位置だけを見る。入力の幅 = n_user_cols(2) + 1 + assigned.len()(1) = 4）
Update { n_user_cols: 2, assigned: [(1, 3)] }       -- 新しい行 = 入力の先頭 2 列のコピーの [1] を、入力の [3] で置き換えたもの
  Project [@0, @1, @2, (@4 + 1)]                    -- 結合の出力は t.a, t.b, t.ctid, u.a, u.c
    ...
```

- 新しい値は**更新前の行と FROM の行**から計算する（`SET a = b, b = a` は入れ替え）。`Update` は入力の先頭 `n_user_cols` 列を旧行として複製し、`assigned` の位置だけを入力の列で置き換えて新しい行にする。NOT NULL → CHECK（新しい行に対して。名前の順）→ `storage.update` の順序は M2 §5.5 のまま。
- 結合で同じ対象行が複数回入力に現れたとき（`UPDATE ... FROM` の多対一）、2 回目の更新は `SelfModified { cmax == 現在の cid }` になり、**飛ばして件数に数えない**（M2 のまま。PG と同じ）。
- `WHERE` が参照した `xmin` などのシステム列は `Project` が落とす（M2 §4.7 と同じ約束）。
- FROM なしの単一表 UPDATE（M2 の形）では、物理化が `Project[@0.., @n, 新しい値の式]` を `SeqScan` の上に置く（恒等部分の省略はしない。新しい値の列が要るため）。

**DELETE**

```text
Delete { old_cols: [#0, #1], ctid: #2 }
  Project [#0, #1, #2]       -- 単一表で WHERE がシステム列を参照しないなら、物理化が恒等 Project を省く（SeqScan が user 列 ++ [ctid] を出す）
    Filter (..)
      Get t [#0 a, #1 b] sys[ctid #2]
```

`DELETE ... USING u` は `Join(Get t, Get u)` の上に `Filter` と `Project`。重複する対象行は UPDATE と同じく 2 回目が飛ばされる。

### 4.4 CHECK・DEFAULT・RETURNING の `rte = 0` の `Var` → `PhysExpr`

| 式 | 解析 | 変換 | 評価 |
|---|---|---|---|
| CHECK（`BoundCheck`） | `table_checks`。`rte = 0` の `Var`（表の行）。結果は bool、NULL は通す | `build` が `lower_single_rel` で `PhysCheck` に。名前のバイト順に整列 | `Insert` / `Update` ノードが、挿入・更新後の行（ユーザー列）に対して `eval`（`&mut ctx`） |
| DEFAULT（`defaults[i]`、`UpdateSource::Default`） | `Var` を含まない（B9）。`nextval()` などは含みうる | `lower_single_rel`（`Var` が無いので構造の複製）。`Default(None)` は型つきの NULL | `Insert`: 列が省略されたとき、空行に対して評価。`Update`: 入力の `Project` が評価 |
| RETURNING（`BoundReturning`） | 対象表の `Var`（`rte = 0`）だけ（B9。M4 の後半・任意） | `lower_single_rel` | `Insert` / `Update` / `Delete` ノードが、挿入した行・更新後の行・削除した行に対して `eval` し、結果を行として返す |
| COPY の列変換（`10`） | `rte = 0` の `Var`（COPY で読んだ行の列） | `lower_single_rel` | COPY 実行が行ごとに `eval_const` / `eval` |

- 保存された DEFAULT / CHECK のテキストを解析し直す経路（`analyze_column_default` / `analyze_table_checks`）は M2・M3 のまま。戻り値の `BoundExpr` が `Expr<Var, Box<BoundQuery>>` に変わるだけ。
- CHECK・DEFAULT の中の副問い合わせは PG と同じ `0A000`（アナライザ）。CHECK のシステム列の参照は `42P10 system column "ctid" reference in check constraint is invalid`（確認済み。M2 の分岐点のまま）。

---

## 5. モジュールごとの仕様: 移行手順（A と P0）

### 5.1 方針

- **5 つの段階**（P0-a〜P0-e）に分け、各段階を 1 つのレビュー可能な変更（1 コミット、または `dev` 上の短い列）にする。**各段階の終わりで、次のすべてが通る**: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`（`impl/rust/`）、`tests/run.sh --target yuzhu tests/slt/m1 tests/slt/m2 tests/slt/m3`、`tests/run.sh --target yuzhu --restart`。`--target pg` の結果は変わらない（テストは変更しない）。
- 順序は**下から**（02-D14）: executor → analyzer → planner。各段階は 1 つの層だけを置き換え、隣の層とは**一時的なアダプタ**（`planner/legacy.rs`）でつなぐ。

```text
旧  AST → analyzer（旧 Bound）           → planner（旧 PhysicalPlan）                       → executor（旧）
a   型だけ追加（新旧が共存。動作は変わらない）
c   AST → analyzer（旧 Bound）           → planner（旧）→ [legacy::to_physical]               → executor（新: PhysExpr・rewind）
d   AST → analyzer（新 Bound）→ [legacy::from_bound] → planner（旧）→ [legacy::to_physical]   → executor（新）
e   AST → analyzer（新 Bound）           → planner（新: build → rules → physicalize）          → executor（新）       ※ legacy 削除
```

- アダプタが自明な構造変換で済む（旧の `ColumnRef { index }` ↔ 新の `Local(index)` / `Var { rte: 0, col }` は 1 対 1）。上から（analyzer → planner → executor）の順だと、論理プランを旧 `PhysicalPlan` に変換するアダプタが `physicalize` の二重実装になる。
- 新旧の型の共存: 新しい Bound の型は `analyzer/query.rs`（`analyzer::query::{BoundQuery, BoundSelect, BoundStatement, BoundInsert, BoundUpdate, BoundDelete, BoundExpr, ..}`）に置く（旧 `analyzer::bound` と名前が衝突するため）。新しい物理プランは `planner/physical.rs`（旧 `planner/plan.rs` の `PhysicalPlan` と別のモジュール。`planner/mod.rs` の `pub use plan::*` は P0-e まで残す）。P0-e で `query.rs` を `bound.rs` に、`physical.rs` を `plan.rs` の後継にする（`mod` 宣言と `use` の機械的な書き換え）。
- アダプタ（`planner/legacy.rs`）は P0-e で削除する。**P0-d と P0-e の間、旧 planner のコード（`plan.rs`、`plan_select` など）を消さない**（P0-e の失敗時に P0-d の状態へ戻せるように）。
- `m4-changes.md`（00 §20）に、各段階で 00 から変えた点を記録する。

**段階が解放する担当**（00 §17 の「P0 のマージ後に並列」を段階ごとに前倒しする）:

| 段階 | 解放される担当 |
|---|---|
| P0-a | S1（パーサ。AST の型）、H4、B1、T1〜T3、K（P0 に触れない担当は A の後すぐ） |
| P0-b | B2（opclass の表を埋める）、C1（`ddl/` と `catalog/` の足場） |
| P0-c | X1、X2、X3（物理プランの新しい型・`ExecCtx`・ノードのスタブが揃う） |
| P0-d | N1、N2、N3（`analyzer/{from,agg,sublink,setop,cte}.rs` のスタブと `Var` / `BoundQuery`） |
| P0-e | L1、L2、E1（`build` / `physicalize` / `rules/*` のスタブと検証関数） |

### 5.2 段階

#### P0-a: 型の追加（担当 A。動作は変わらない。1.5 日）

- **変更ファイル**: `expr/{mod,walk}.rs`（§3.1 のすべて: `Expr` `ExprKind` ID 型 `Var` `PhysCol` `AggCall` `SubLinkKind`、`walk` / `try_map` / `try_rewrite` / `same_as` / `columns` / `conjuncts` / `lower_single_rel`）、`analyzer/query.rs`（00 §7 の新しい Bound の型。`BoundQuery::validate`）、`planner/logical.rs`（00 §8 の型、`ColumnArena`、§3.5.7 の走査 API、`LogicalQuery::validate`）、`planner/physical.rs`（00 §9 の型、`width` / `children` / `uses_params` / `exprs_with_width` / `PhysicalQuery::single`、`PhysicalQuery::validate`）、`executor/{mem,subplan}.rs` の型だけ（`MemBudget` は本実装）、`error.rs`（00 §14.4 のフィールドと §15.3 の SQLSTATE）、`lib.rs`（`pub mod expr;`）、`types/mod.rs`（`TypeEnv` の型）、`Cargo.toml`。
- **作業**: 型と、純粋関数（走査・検証）の本実装と単体テスト。既存コードは**触らない**（新しい型は使われない）。
- **完了条件**: 既存のテストがそのまま通る。§6.1 の `expr` / `validate` の単体テストが通る。
- **戻し方**: コミットを `git revert`（未使用の型を消すだけ）。

#### P0-b: 依存のない ★ スタブ（P0。0.5 日）

- 内容は §5.4 の「P0-b」の表。コンパイルが通る空の足場で、既存コードのどこからも呼ばれない。
- 追記だけの小さな変更として、`storage/stack.rs` の `StorageStack` に `index: Arc<dyn IndexStore>`（実体は `BtreeStore` のスタブ）と `seq: Arc<dyn SequenceStore>` を足し、`engine.rs` の `Cluster` に `indexes()` / `sequences()` の取得口を足す（M4 の `StorageStack` の拡張は本来 C1・S の担当だが、P0-c の `ExecEnv.indexes` に要る。00 への変更提案 14）。
- **完了条件**: 全テスト。§6.1 の `stubs_return_not_supported`（スタブが `0A000` と所定のメッセージを返す）。
- **戻し方**: `git revert`。

#### P0-c: executor を `PhysExpr`・`rewind` に（P0。1.8 日）

- **変更ファイル**: `executor/{mod,build,eval}.rs`、`executor/nodes/*`（既存 11 ノード + ★ スタブ 12 ファイル）、`session.rs`（`ExecCtx::new`・`legacy::to_physical` の呼び出しだけ）、`planner/legacy.rs`（新規）、各ノード・`eval` の単体テストのヘルパ。
- **作業**:
  1. `ExecCtx` の拡張（`indexes` `query` `params` `mem` `subplans` `ctes` `type_env`）、`ExecEnv`、`ExecCtx::new`、`EvalCtx.type_env`（§3.7.2）。`Executor::rewind` を必須にする（既定実装を作らない）。
  2. `eval` を `PhysExpr` と `&mut ExecCtx` に。`Column(Local(i))` は `row[i]`（範囲外は `Error::internal`）、`Column(Param(p))` は `ctx.params[p]`、`SubLink` は `subplan::eval_sublink`（スタブ）、`Aggregate` と、`sub` が無いときの `SubLinkOutput` は `Error::internal`。`eval_const`（`EvalCtx` だけ）を足し、旧 `eval_expr` / `eval_bool` はこれに統合する。既存の式の評価規則（三値論理・遅延評価・strict）は変えない。
  3. 既存 11 ノードを `PhysExpr` に移し、§3.7.1 の表のとおり `rewind` を実装し、§3.7.4 の `check_interrupts` の規則に合わせる。`SortExec` は `types::cmp::cmp_with_nulls` を使う。`DistinctExec` は M1 の実装のまま（`rewind` を足すだけ。X2 が `HashKey` に切り替える）。
  4. `Update` / `Delete` / `Insert` のノードを新しい形（`Update { n_user_cols, assigned }`、入力の位置だけを見る。`returning: None` のとき行を返さない）にする。`InsertExec` / `UpdateExec` は `executor/dml.rs` の `insert_with_indexes` / `update_with_indexes` を呼ぶ。
  5. `executor::build` に新しい `PhysicalPlan` の全変種を足す（`NestedLoopJoin` などは `UnsupportedExec`。§5.4 の P0-c）。
  6. `planner/legacy.rs`: `pub fn to_physical(old: &plan::PhysicalPlan) -> Result<physical::PhysicalPlan>`（旧 `BoundExpr` の `ColumnRef { index }` → `Column(Local(index))`、他の変種は 1 対 1。旧 `Update { assignments }` は、入力の上に `Project[@0..@n-1, @n, 代入式…]`（旧行に対して評価する式。`Default(Some(e))` は `e`、`Default(None)` は型つき NULL）を足した新しい `Update` に変換。`Insert.defaults` / `checks` を `PhysExpr` に）と、`pub fn to_query(p: PhysicalPlan, columns) -> PhysicalQuery`（`subplans` / `ctes` は空）。
  7. `session.rs`: 旧 `planner::plan` の結果を `legacy::to_physical` に通し、`ExecCtx::new` で `ctx` を作る。テストの足場（`executor/nodes/mod.rs` の `Fixture`）は、`PhysicalQuery::single(plan, ..)` を作って `ExecCtx::new` に渡す形に直し、`n_params` を指定できるようにする（`Param` を使うテスト用）。
- **完了条件**: §3.3 の `PhysicalQuery::validate` を、`legacy::to_physical` の出力に対して（デバッグビルドで）毎回実行して通る。既存の全テストが通る（テストのヘルパ `col` / `lit` / `op` は `PhysExpr` を作るように書き換える）。`rewind` と割り込みの新しいテスト（§6.2）が通る。
- **戻し方**: `git revert`（`legacy.rs` も一緒に消える。session は元の `executor::build(&plan)`）。

#### P0-d: analyzer を `Var` と `BoundQuery` に（P0。1.4 日）

- **変更ファイル**: `analyzer/{mod,scope,select,dml,expr,coerce,resolve,ddl}.rs`、`analyzer/tests.rs`、`analyzer/{from,agg,sublink,setop,cte}.rs`（★）、`planner/legacy.rs`、`session.rs`（`Ddl(..)` の分岐）。
- **作業**:
  1. 式を `Expr<Var, Box<BoundQuery>>` に（`ColumnRef { index }` → `Column(Var)`）。`visit` → `Expr::walk`、`same_expr` → `Expr::same_as`、`contains_column_ref` → 「レベル 0 の `Var` を含むか」の判定（LIMIT・OFFSET。02-D18）、`sole_column_ref` → `columns()` を使った書き直し。
  2. `Scope` を `ScopeStack`（フレームの列。末尾が現在のレベル。P0 では長さ 1 か 2）に。フレームは `rels: Vec<ScopeRel>`（各 `ScopeRel` が `RteId` を持つ）。名前解決の結果は `Var { rte, col, levels_up }`。システム列は `Var::system(rte, sc)`（旧 `used_system` の記録は不要。`build` が `Var` から集める）。
  3. `analyze_query` → `BoundQuery`（本体が Select / Values。単独の `VALUES` は `BoundSetExpr::Values { rows, types }`）。`BoundSelect` は `rtable`（Table の RTE 1 つ、または FROM なしで空）、`from`、`targets`、`n_visible`。`output_column` は RTE から `table_oid` / `attnum` を決める。`resolve_unknowns`・`coercions` の約束（`Var { rte: 0, col: i }`）は §3.4.4 のとおり。
  4. `BoundStatement` を 00 §7 の形に（`CreateTable` / `DropTable` は `Ddl(BoundDdl::CreateTable)` / `Ddl(BoundDdl::DropTable)` に包む。他の `BoundDdl` の変種は A が空の構造体で置く）。`BoundCheck` / `defaults` は `Var { rte: 0 }`。
  5. ★ スタブ（§5.4 の P0-d）と**分岐点の口**: `select.rs` の FROM の match → `from::analyze_from_clause`、`expr.rs` の集約の判定 → `agg::analyze_agg_call`、`expr.rs` の `Expr::{InSubquery, Exists, Subquery}` → `sublink::analyze_sublink`、`select.rs` の `QueryBody::SetOp` → `setop::analyze_set_operation`、`group_by` / `having` / `Distinct::On` → `agg::analyze_grouping`。**エラーのメッセージ・SQLSTATE・位置は現在と同じ**（既存テストを変えずに通すため）。
  6. `planner/legacy.rs`: `pub fn from_bound(s: &query::BoundStatement) -> Result<analyzer::BoundStatement>`（新しい Bound を旧 `BoundStatement` へ。`Var { rte: 0, col }` → `ColumnRef { index: col }`、システム列は旧 M2 の約束（出現順に `natts + 位置`、`system_columns` に記録）に直す。`BoundQuery` は単一の Select / Values だけを旧 `BoundSelect` に畳む）。
  7. デバッグビルドで `analyze` の出力に `BoundQuery::validate` を実行する。
- **完了条件**: 既存の `analyzer/tests.rs` が、期待値の**形**（`BoundSelect` の構造を見る数か所）の書き換えだけで通る。メッセージと SQLSTATE を見るテストは書き換えなし。全テスト。
- **戻し方**: `git revert`（P0-c の状態に戻る）。

#### P0-e: planner を `build` → `rules` → `physicalize` に（P0。1.3 日）

- **変更ファイル**: `planner/{mod,build,physicalize,logical,physical}.rs`、`planner/rules/*`（★）、`planner/{index_select,explain_tree}.rs`（★）、`session.rs`、旧 `planner/{plan.rs,legacy.rs}`・旧 Bound の型・`analyzer/bound.rs` の旧部分の**削除**、`analyzer/query.rs` → `bound.rs`。
- **作業**:
  1. `build`（Bound → 論理）: 既存ノード分（`Get` `Values` `Result` `Filter` `Project` `Sort` `Distinct`（All）`Limit` `Insert` `Update` `Delete`）。`Var` → `ColId` の対応表のスタック（§3.5.6）、`Get.system_columns` の収集（`Var` のシステム列から。DML の対象表は `Ctid` を必ず入れる）、`lower_single_rel` で `PhysCheck` / `defaults` / `returning` を作る、`Update` の入力の `Project`（§3.5.5）、resjunk の除去（§3.4.1 の 5〜9）。それ以外（JOIN・集約・副問い合わせ・集合演算・DISTINCT ON・WITH）の分岐は `not_supported`（§5.4 の P0-e）。
  2. `rules::run`: 恒等のルール列（スタブ）。
  3. `physicalize`: `ColId → 位置` の表で `LExpr` → `PhysExpr`。M1 の形を保つ畳み込み 3 つ（恒等 `Project` の省略、`Project(Result)` → `Result { exprs }`、`Empty` → `Result { one_time_filter: Some(false) }`）。`Update` / `Delete` / `Insert` を 00 §9.2 の形に。`SeqScan.filter` は常に `None`。
  4. `plan(&BoundStatement, &PlanEnv) -> Result<PhysicalQuery>`（00 §9.1）: デバッグビルドでは `build` の後・各ルールの後・`physicalize` の後に `validate`。`PlannerSettings` は `Default`（すべて true、`query_mem_limit` = 256MB）。`want_explain` が true なら `ExplainNode` を作る口は用意する（P0 は `not_supported`）。
  5. `session.rs`: `planner::plan(&bound, &env)`、`column_descs(&query.output, ..)`、`bound.returns_rows()`。旧経路の呼び出しを消す。
  6. **差分テスト（この段階の間だけ）**: テスト内で旧経路（`legacy::from_bound` → 旧 planner → `legacy::to_physical`）と新経路を、`analyzer/tests.rs` と M1〜M3 の slt に出てくる SELECT / INSERT / UPDATE / DELETE の代表（約 60 文）に対して実行し、結果の行が一致することを確かめる（`planner::tests::new_path_matches_legacy`）。通ったら、旧経路・`legacy.rs`・旧 Bound・旧 `plan.rs` を削除し、このテストも削除する。
  7. 旧 `planner/mod.rs` の単体テストを、新しい経路（`build` → `physicalize` → `build_exec`）に移す。テスト名は維持する（§5.3）。
- **完了条件**: 全テスト。`LogicalQuery::validate` / `PhysicalQuery::validate` が全ての planner テストで走って通る。`tests/run.sh --target yuzhu`（m1〜m3）が通る。差分テストが通った後で削除が済んでいる。
- **戻し方**: 削除前なら、session の呼び出しを旧経路に戻す 1 か所の変更（`legacy` が残っているため）。削除後は `git revert` で P0-d の状態へ。

### 5.3 M1〜M3 のテストが守るリスク

各リスクについて、**どの段階で**どのテストが守るか。P0 のレビュアは、段階ごとに該当テストを確認する。

| # | リスク | 守るテスト（名前） | 段階 |
|---|---|---|---|
| R1 | Halloween（自分が書いた新しい版を同じ文のスキャンが読む）。`Update` の入力の形が変わる | slt `m2/dml/update_halloween`、`m1/insert/insert_select`（自分自身への INSERT ... SELECT）。単体 `update_without_where_does_not_rescan_new_versions`、`insert_plan_and_execution`。session `same_transaction_ddl_then_dml_and_update_halloween` | c、e |
| R2 | UPDATE の `SET` が旧行に対して評価される（`SET a = b, b = a` が入れ替え） | slt `m2/dml/update_expr`。単体 `set_expressions_see_the_old_row` | c、e |
| R3 | 制約検査の順序（ヒープを書く前、CHECK は名前のバイト順、NOT NULL の後） | slt `m2/dml/update_constraints`、`m1/constraints/{check,not_null,atomicity}`。単体 `check_violation_is_found_before_writing_and_in_name_order`、`not_null_violation_leaves_the_row`、`check_violation_and_null_passes`、`failing_row_clips_long_values` | c、e |
| R4 | システム列（`ctid` `xmin` …）の位置と、UPDATE / DELETE の入力から `WHERE` 用の列が落ちること | slt `m2/dml/system_columns`。単体 `system_columns_in_where_are_dropped_before_update`、analyzer `system_columns`、session `user_function_error_and_system_columns` | d（`Var` の符号化）、e（`Get.system_columns` の収集） |
| R5 | resjunk・ORDER BY・DISTINCT・LIMIT の積み順 | slt `m1/select/{order_by,distinct,limit_offset,values,basic,where}`。単体 `filter_order_by_resjunk_limit`、`distinct_after_sort_and_values`、`select_star_skips_projection`、`fromless_select_is_a_result_node`、`removes_duplicates_keeping_order`、`limit_offset`、`negative_counts`、`null_ordering_defaults_and_overrides`、`multi_key_stable_bytes_and_nan` | c（ノード）、e（積み順） |
| R6 | 式の評価（三値論理・遅延評価・strict・LIKE・IN の NULL） | slt `m1/expressions/*`、`m1/functions/*`、`m1/types/*`。単体（eval）`three_valued_logic`、`null_tests_and_bool_tests`、`case_and_coalesce_are_lazy`、`in_list_null_semantics`、`like_matching`、`casts_typmod_and_session_values` | c |
| R7 | エラーの SQLSTATE・メッセージ・位置 | slt `m1/errors/sqlstate`、`m1/constraints/*`。analyzer の全テスト（`names_and_scopes` `order_by_rules` `limit_offset` `insert_targets` `update_errors` `delete_basic_and_errors` など）。`tests/pg_compat_review.rs`（`error_messages_use_pg_type_names`、`hidden_table_name_reference_and_ddl_without_position`） | d |
| R8 | 割り込み（`57P01` / `57014`）が文の途中で効く | 単体 `stops_on_shutdown_request`、`interrupts_stop_dml`、`write_ctx_and_interrupts`。session `interrupt_stops_a_statement_with_fatal`、`timeouts_and_cancel`。slt `m3/session/statement_timeout` | c |
| R9 | システムカタログのスキャンと出力（regproc の名前、型の出力、多数のシステム型） | slt `m2/catalog/*`（`pg_class` `pg_attribute` `pg_proc_operator_cast` `pg_type` `catalog_columns` …）、`m2/psql/l`、`m2/types/system_types`。`pg_compat_review.rs::psql_l_access_privileges_query_runs`。単体 `regproc_output_uses_function_names`、`context_functions_see_catalog_and_session` | c（出力段）、e |
| R10 | 書き込みロック・WAL・コミット・再起動をまたぐ DML の経路 | slt `m2/txn/*`、`m3/txn/*`、`m3/checkpoint/*`。`tests/restart/*`（`01-committed-dml` … `06-uncommitted-at-stop`、`m3/*`）。session `writer_lock_is_exclusive_and_readers_see_only_committed`、`rollback_undoes_data_and_ddl`、`data_survives_clean_restart_and_checkpoint_crash`。`yuzhu-core/tests/crash_sim`（ワークロードの UPDATE / DELETE）。`yuzhu-server/tests/{server,crash_kill9}.rs` | c、e |
| R11 | RowDescription（`table_oid` / `attnum` / 型）と `unknown` → text | session `column_descriptions`、`select_without_table_and_show_server_values`。slt `m1/types/cast`、`m1/expressions/unknown_literals` | d（`OutputColumn` の由来）、e（`column_descs(&query.output)`） |
| R12 | `INSERT ... SELECT` の型変換の位置（ORDER BY / DISTINCT / LIMIT の上） | slt `m1/insert/insert_select`、`m1/insert/{values,defaults}`。analyzer `insert_coercion`、`insert_select`、`insert_targets`、`stored_defaults_and_checks` | d、e |
| R13 | 未対応構文が黙って通らず `0A000` になる（M4 の機能が揃うまで） | analyzer `unsupported_statements`、`functions`（`count(*)` が `0A000`）。slt `m1/errors/sqlstate` | d、e（スタブのメッセージを変えない） |
| R14 | 新しい不変条件（`validate`）が既存の全クエリで成り立つ | 上のすべてのテスト（デバッグビルドで `validate` が走る） | c（物理）、d（Bound）、e（論理・物理） |

### 5.4 ★ スタブ一式（段階ごと）

規約:

- スタブは**コンパイルが通る署名**を持ち、中身は `Err(Error::not_supported(format!("{what} is not supported yet")))`（`0A000`）。M1 の `not_supported(what, span)` と同じ書式で、**位置（`with_span`）も付ける**（呼び出し側が持つ span を渡す）。`what` が複数形のときは `are`（`aggregate functions are not supported yet`、`subqueries are not supported yet` のように、**現在のアナライザの文言は変えない**）。
- 戻り値が `Result` でない関数のスタブは、**現在の `Datum` の変種に対して正しい最小実装**を置く（`panic` / `unimplemented!` / `todo!` は使わない。00 §4.3 の規約 7）。
- 何もしなくても結果が正しい関数（ルール、インデックス選択）のスタブは、エラーでなく**恒等**にする。
- 署名のうち「（署名は NN が確定）」と書いたものは、章 NN の書き手が確定した署名に合わせて P0 が置く。

##### P0-b: 依存のない ★

| ファイル | スタブの中身 | メッセージ | 分岐点（呼ぶ側） |
|---|---|---|---|
| `storage/btree/{mod,page,tuple,meta,search,insert,split,build,scan,unique,check,wal}.rs` | `mod.rs` に子モジュールの宣言と、00 §15.1 の定数（`BT_*`、`INDEX_MAX_KEYS`）、`wal::{BTREE_INSERT_LEAF, BTREE_PAGES}`。他は空（モジュールのコメントだけ） | — | `storage/mod.rs` の `pub mod btree;` |
| `storage/index_store.rs` | `pub struct BtreeStore;` と `impl IndexStore for BtreeStore`（00 §13.2 の全メソッド）。すべて `Err(not_supported("B+Tree indexes"))` | `B+Tree indexes are not supported yet` | `StorageStack.index`（P0 が `StorageStack::new` で `Arc::new(BtreeStore)`）。呼ぶのは X3 の `insert_with_indexes` と C1 の CREATE INDEX。**P0 の間は `RelHandle.indexes` が空なので呼ばれない** |
| `storage/sequence.rs` | `pub struct SequenceStoreImpl;` と `impl SequenceStore`（00 §13.3 の全メソッド）→ `not_supported`。`SEQ_LOG` などの定数 | `sequences are not supported yet` | `StorageStack.seq`。呼ぶのは Q1 |
| `ddl/mod.rs` | `DdlCtx`（00 §14.6）、`pub fn execute(ctx: &mut DdlCtx<'_>, ddl: BoundDdl) -> Result<String>`: `CreateTable` / `DropTable` は `Error::internal("executed by the session until C1 moves it")`、他は変種ごとの `not_supported` | `CREATE INDEX is not supported yet`、`DROP INDEX …`、`CREATE SEQUENCE …`、`ALTER SEQUENCE …`、`DROP SEQUENCE …`、`ALTER TABLE ... ADD CONSTRAINT …`、`ALTER TABLE ... OWNER TO …`、`TRUNCATE …`、`VACUUM …` | session の `Ddl(d)` 分岐（P0-d で追加。`CreateTable` / `DropTable` は session が直接処理し続ける） |
| `ddl/{table,index,constraint,sequence,truncate,depend}.rs` | 空 | — | `ddl::execute` |
| `copy/{mod,text,exec}.rs` | `pub fn begin(copy: &BoundCopy) -> Result<()>`（署名は 10 が確定） | `COPY is not supported yet` | session の `Copy(c)` 分岐（S） |
| `explain/{mod,format,node}.rs` | `pub fn run(..) -> Result<..>`（署名は 10 が確定） | `EXPLAIN is not supported yet`（現在のアナライザの文言と同じ） | session の `Explain(e)` 分岐（S） |
| `deparse/mod.rs` | `deparse_lexpr(e: &LExpr, arena: &ColumnArena, opts: &DeparseOpts) -> Result<String>`、`deparse_bound(e: &BoundExpr, cx: &DeparseCx<'_>) -> Result<String>`（署名は 10 が確定） | `deparse is not supported yet` | `pg_get_expr`（C1・T3）、`explain_tree`（L2） |
| `types/{numeric,datetime,bpchar,regex}.rs` | 空（モジュールのコメントだけ） | — | T1・T2・T3 が埋める |
| `types/hash.rs` | `pub fn hash_datum(d: &Datum, state: &mut dyn Hasher)`、`pub struct HashKey(pub Vec<Datum>)`（`Eq` / `Hash`）。現在の変種に対する本実装（`DistinctExec::key_of` と同じ正規化: 整数は i64、float は NaN と -0 を正規化） | — | X2 が `DistinctExec` を `HashKey` に切り替える |
| `types/cmp.rs` | `pub fn cmp_with_nulls(a: &Datum, b: &Datum, descending: bool, nulls_first: bool) -> Ordering`（00 §12.2。本実装 10 行） | — | `SortExec`（P0-c で切り替える） |
| `catalog/opclass.rs` | 00 §11.3 の型、空の `static`（`OPFAMILIES` `OPCLASSES` `AMOPS` `AMPROCS`）、`default_opclass` / `opclass_by_oid` / `opclass_by_name` / `comparator` / `operator_strategy` は `None` | — | B2 が表を埋める |
| `catalog/builtin.rs` | `BuiltinAggregate`、`AggKind`、`AGGREGATES: &[BuiltinAggregate] = &[]`、`aggregates_named` は空の `Vec` | — | T3 |
| `catalog/mod.rs`（`CatalogReader`） | 00 §11.2 の 5 メソッドの既定実装（`relation_kind` `index_by_name` `index_by_oid` `relation_name` は `Ok(None)`、`aggregates_named` は静的な表） | — | C1 が `StatementCatalog` で上書き |
| `yuzhu-fuzz-sql/` | `Cargo.toml`（ワークスペースの member）と `src/main.rs`（`eprintln!` して終了コード 2） | — | Z |

##### P0-c: executor

| ファイル | スタブの中身 | メッセージ | 分岐点（呼ぶ側） |
|---|---|---|---|
| `executor/mem.rs` | 本実装（`MemBudget`、`estimate_row_bytes`。00 §10。超過は `53200` `out of memory`、DETAIL に `yuzhu.query_mem_limit`） | — | 溜める系ノード（X1・X2） |
| `executor/agg.rs` | `pub struct AggState;` `impl AggState { pub fn new(agg: &PhysAgg) -> Result<AggState>; pub fn accumulate(&mut self, args: &[Datum]) -> Result<()>; pub fn finish(&self) -> Result<Datum>; }` | `aggregate functions are not supported yet` | `Aggregate` / `HashAggregate` / `GroupAggregate` ノード（X2） |
| `executor/subplan.rs` | §3.7.3 の型。`SubPlanStates::new` / `take_exec` / `put_exec` は本実装。`eval_sublink` だけスタブ | `subqueries are not supported yet`（現在の analyzer の文言と同じ） | `eval` の `ExprKind::SubLink` 分岐（P0-c で足す）。X1 が `eval_sublink` を埋める |
| `executor/dml.rs` | `insert_with_indexes` / `update_with_indexes`（00 §10）の**本実装**（`storage.insert` / `storage.update` を呼ぶだけ。`rel.indexes` が空の間は正しい） | — | X3 が索引の維持を足す |
| `executor/instrument.rs` | `pub struct Instrument;`（空） | — | E1 |
| `executor/nodes/{nested_loop,hash_join,materialize,aggregate,hash_aggregate,group_aggregate,unique,append,hash_setop,cte_scan,function_scan,index_scan}.rs` | 各ファイルに `pub(super) fn build(plan: &PhysicalPlan) -> BoxedExecutor`。中身は `Box::new(UnsupportedExec::new("<ノード名>"))`。`UnsupportedExec`（`nodes/mod.rs`）の `next` / `rewind` は `Err(not_supported)` | `Nested Loop is not supported yet`、`Hash Join …`、`Materialize …`、`Aggregate …`、`HashAggregate …`、`GroupAggregate …`、`Unique …`、`Append …`、`HashSetOp …`、`CTE Scan …`、`Function Scan …`、`Index Scan …`（ノード名は PG の EXPLAIN の表記） | `executor::build` の match（P0-c で足す）。X1〜X3 が各ファイルの `build` を本実装に置き換える |

##### P0-d: analyzer

| ファイル | スタブの中身 | メッセージ（現在のものと同じ） | 分岐点（呼ぶ側） |
|---|---|---|---|
| `analyzer/from.rs` | `impl Analyzer<'_> { pub(super) fn analyze_from_clause(&self, from: &[TableRef], scopes: &mut ScopeStack) -> Result<(Vec<Rte>, Vec<FromItem>)> }`。**単一の `TableRef::Table` の解析をここに移す**。それ以外はエラー | `JOIN is not supported yet`、`subquery in FROM is not supported yet`、`JOIN (more than one table in FROM) is not supported yet`（位置つき） | `select.rs` の `analyze_select`（FROM の match を置き換える）。`dml.rs` の `UPDATE ... FROM` / `DELETE ... USING` の `0A000` は N1 が `analyze_from_clause` に置き換える |
| `analyzer/agg.rs` | `analyze_agg_call(&self, call: &ast::Expr, cx: &ExprCtx<'_>) -> Result<BoundExpr>`、`analyze_grouping(&self, s: &ast::Select, ..) -> Result<GroupingInfo>`（`GroupingInfo` は N2 が決める。P0 は空の構造体） | `aggregate functions are not supported yet`、`GROUP BY is not supported yet`、`HAVING is not supported yet`、`SELECT DISTINCT ON is not supported yet` | `expr.rs` の `Expr::Function`（現在 `AGGREGATES.contains` で `0A000`）、`select.rs` の `group_by` / `having` / `Distinct::On` |
| `analyzer/sublink.rs` | `analyze_sublink(&self, e: &ast::Expr, cx: &ExprCtx<'_>) -> Result<BoundExpr>` | `subqueries are not supported yet` | `expr.rs` の `Expr::{InSubquery, Exists, Subquery}` |
| `analyzer/setop.rs` | `analyze_set_operation(&self, q: &ast::Query, scopes: &mut ScopeStack) -> Result<BoundQuery>` | `UNION/INTERSECT/EXCEPT is not supported yet` | `select.rs` の `QueryBody::SetOp` |
| `analyzer/cte.rs` | `analyze_with(&self, with: &ast::With, scopes: &mut ScopeStack) -> Result<Vec<BoundCte>>` | `WITH is not supported yet`（現在はパーサが `0A000`。AST に `With` が足された後（S1）の分岐点） | `select.rs` の `analyze_query` |

##### P0-e: planner

| ファイル | スタブの中身 | メッセージ | 分岐点（呼ぶ側） |
|---|---|---|---|
| `planner/build.rs` | `pub fn build(stmt: &BoundStatement, env: &PlanEnv<'_>) -> Result<LogicalQuery>`（既存ノード分は本実装）。内部の分岐点: `build_from_item`（`FromItem::Join`）、`build_rte`（`Subquery` / `Function` / `CteRef`）、`build_group`（`has_agg` / `group_by`）、`build_set_op`、`lower_sublink`（`SubLink` / `Aggregate`）、`build_distinct_on`、`build_with`（`ctes` が非空） | `JOIN is not supported yet`、`subquery in FROM …`、`function in FROM …`、`aggregate functions …`、`UNION/INTERSECT/EXCEPT …`、`subqueries …`、`SELECT DISTINCT ON …`、`WITH …` | L1 が各関数を本実装にする |
| `planner/rules/{mod,const_fold,sublink,subquery_pullup,outer_join,pushdown,join_keys,join_order,prune}.rs` | `rules::run(q: LogicalQuery, env: &PlanEnv<'_>) -> Result<LogicalQuery>` が、各ルールの `pub fn apply(q: LogicalQuery, env: &PlanEnv<'_>) -> Result<LogicalQuery>` を `04` の順に呼ぶ。各 `apply` は**恒等**（`Ok(q)`） | — | L1 |
| `planner/physicalize.rs` | `pub fn physicalize(q: LogicalQuery, env: &PlanEnv<'_>) -> Result<PhysicalQuery>`（既存ノード分は本実装）。内部の分岐点: `physicalize_join`、`physicalize_aggregate`、`physicalize_setop`、`physicalize_cte_scan`、`physicalize_function_scan`、`physicalize_sublink` | build.rs と同じ文言 | L2 |
| `planner/index_select.rs` | `pub fn choose_index(rel: &RelHandle, preds: &[PhysExpr], env: &PlanEnv<'_>) -> Result<Option<IndexChoice>>`（**`Ok(None)` = SeqScan**。署名は 04 が確定）。`pub struct IndexChoice { pub index: IndexHandle, pub keys: IndexScanKeys, pub direction: ScanDirection, pub used: Vec<usize> }` | — | `physicalize` の `Get` 分岐 |
| `planner/explain_tree.rs` | `pub fn explain_node(..) -> Result<ExplainNode>`（署名は 04・10 が確定） | `EXPLAIN is not supported yet` | `physicalize` の各関数（`want_explain` のとき） |

---

## 6. テスト

この章の変更は**外から見える振る舞いを変えない**（P0 の完了条件は M1〜M3 の全テストが通ること）。新しいテストは、新しい契約（式の木・検証・`rewind`・割り込み・SubPlan の状態遷移）を単体で固定する。共有 slt（`tests/slt/m4/`）の追加は K の担当で、この章では足さない。

### 6.1 単体テスト（A、P0）

**`expr`（`expr/walk.rs` の `#[cfg(test)]`）**

| テスト | 確かめること |
|---|---|
| `walk_visits_preorder_in_child_order` | §3.1.2 の表の順序（`Case` は条件・結果の組を順に、最後に `else`。`Like` は `expr`・`pattern`・`escape`。`Aggregate` は `args`、`filter`）。全変種を 1 つずつ含む式で、訪問順の `Vec` を比べる |
| `walk_prune_and_sublink_boundary` | `f` が `false` を返した部分木の子を訪れない。`SubLink` は `test` に降り、`query` には降りない（`query` に目印のノードを入れて、訪問されないことを確かめる） |
| `try_map_roundtrip_changes_column_type` | `Expr<u32, ()>` → `Expr<String, ()>` のように `C` を変える変換で、構造・`ty`・`span`・非子フィールドが保たれる（全変種） |
| `try_map_replaces_without_descending` | `f` が `Some` を返したノードの子に降りない。置き換えた式の `ty` が使われる |
| `try_map_leaf_not_converted_is_internal_error` | `Column` / `Aggregate` / `SubLink` を変換しない `f` で、それぞれ `XX000`（メッセージに変種名） |
| `try_map_copies_type_independent_leaves` | `Literal` `SessionValue` `SubLinkOutput` は `f` が `None` でも複製される |
| `try_rewrite_keeps_leaves_and_descends_test` | `None` の葉は clone。`SubLink` の `test` に降り、`Aggregate` の `args` / `filter` に降りる |
| `same_as_ignores_spans_and_compares_float_bits` | span 違いは等しい。`-0.0` と `0.0`、NaN の扱い（ビット比較）。`SubLink` は常に不一致 |
| `and_all_and_conjuncts` | 0・1・多数、入れ子の `And` の平坦化。`conjuncts` の返す順序 |
| `lower_single_rel_cases` | §3.1.5 の表の全行（`rte != 0`、`levels_up > 0`、システム列、`SubLink`、`Aggregate`、`SubLinkOutput` は `XX000`） |

**論理プラン（`planner/logical.rs`）**

| テスト | 確かめること |
|---|---|
| `output_cols_and_defines_per_node` | §3.5.1 の表を、ノードの種類ごとに表駆動で確かめる（パススルーの `Project`、重複を避けるコピー、`Aggregate` のグループキーの再利用） |
| `outer_refs_of_nested_subqueries` | 2 段の入れ子の副問い合わせで、中間のプランの `outer_refs` が内側の外側参照を含む（§3.6.3 の「入れ子は継承」の前提） |
| `substitute_reaches_into_subqueries` | `substitute` が `SubLink` の `query` の中の外側参照も置き換える |
| `validate_rejects_each_rule`（L1〜L10 のそれぞれ） | 有効なプランをテスト用のビルダ（`planner/testing.rs`、`#[cfg(test)]`）で作り、1 か所だけ壊して、`XX000` とメッセージの規則 ID を確かめる。例: L1 = 同じ `ColId` を 2 つの `Get` が定義、L2 = `Filter` が子にない `ColId` を参照、L3 = `Project` の出力に同じ ID が 2 回、L4 = `LExpr` に `Aggregate`、L5 = `SubLinkOutput` が `test` の外、L8 = `Update.input` の出力が `old_cols ++ [ctid] ++ new_values` と違う |

**物理プラン（`planner/physical.rs`）**

| テスト | 確かめること |
|---|---|
| `width_per_node` | §3.6.1 の表（Semi / Anti は左だけ、`CteScan` は `ctes` の幅、DML の `returning`） |
| `children_order_matches_execution_and_display` | `HashJoin` は `build_is_left` の両方で `[probe, build]`。`NestedLoop*` は `[outer, inner]` |
| `exprs_with_width_per_node` | §3.6.2 の表（`Update.checks` は `n_user_cols`、`HashJoin.residual` は `left_width + right_width`、`Result` は 0） |
| `uses_params_cases` | `Param` を含む式、`NestedLoopParam.params` の中の `Param`、`SubLink` を含む部分木（true）、どちらも無い（false） |
| `validate_rejects_each_rule`（P1〜P11 のそれぞれ） | 上と同じ方式。P2 は「束縛されていない `Param`」「同じ `ParamId` を 2 か所が束縛」、P3 は「同じ `SubPlanId` を 2 回参照」「`kind` の不一致」、P4 は「物理の `SubLink.test` が `Some`」「`Hashed` の条件違反」、P6 は幅の不一致の各種 |

**Bound（`analyzer/query.rs`）**: `bound_validate_rejects_each_rule`（B1〜B11）。たとえば B3 = Join RTE を指す `Var`、B4 = `Aggregate` が `filter` に、B8 = `limit` にレベル 0 の `Var`。

**スタブ**: `stubs_return_not_supported`（表駆動。§5.4 の全スタブが `0A000` と所定のメッセージを返す。メッセージは文字列で固定）。P0-b・c・d・e で表に行を足す。

### 6.2 executor のテスト（P0。X1〜X3 が拡張する）

| テスト | 確かめること |
|---|---|
| ノードごとの `rewind_replays_same_rows` | 既存 11 ノード（と、足されるノード）で、途中まで読んで `rewind` し、最初から読み直した結果が前回と同じ |
| `rewind_reuses_or_rebuilds_by_uses_params` | 溜める系ノード（`Sort` など）の下に、読み込み回数を数えるテスト用の子（`CountingScan`）を置く。`uses_params() == false` なら `rewind` 後に子が再実行されない（読み直し）、`true`（`Param` を含むキー式や子）なら再実行される。`ctx.params` を変えると、`true` の場合だけ結果が変わる |
| `rewind_before_start_is_noop` | 未開始のノードに `rewind` を呼んでから読んでも、通常と同じ結果（02-D16） |
| `dml_rewind_is_internal_error` | `Insert` / `Update` / `Delete` の `rewind` は `XX000` |
| `every_leaf_checks_interrupts_per_next` | 葉ノードの種類ごとに、キャンセルを要求した状態で `next` を 1 回呼ぶと `57014` |
| `long_loops_check_interrupts` | §3.7.4 の規則 3 のループ（溜めた行の上を回るもの）に、大きな入力とキャンセル要求を与えると、その `next` の中で `57014` |
| `eval_param_and_misplaced_kinds` | `Column(Param(p))` は `ctx.params[p]`、範囲外は `XX000`。`Aggregate` と、`sub` が無い `SubLinkOutput` は `XX000` |
| `update_node_uses_positions_only` | `Update { n_user_cols, assigned }` が、入力の先頭 `n_user_cols` 列を旧行として複製し、`assigned` の位置だけを入力の列で置き換える。幅が合わない入力は `XX000`（M2 の `malformed_input_rows_are_internal_errors` の後継） |
| SubPlan の状態遷移（X1 が `eval_sublink` を実装した後。P0 では `#[ignore]` の枠だけ置く） | `InitOnce` が 1 回だけ実行される（読み込み回数）、実行しない分岐では実行されない（遅延）、`Rescan` が外側の行ごとに実行される、`Scalar` の 2 行目で `21000`、`Exists` が最初の行で止まる（子の読み込み回数）、`Hashed` の三値のフォールバック（§3.7.3 の 6 つの組。PG 17 で確認済みの値）、再入で `XX000` |

### 6.3 planner のテスト

- P0-e: 旧 `planner/mod.rs` の単体テストを新しい経路に移す（名前は維持。§5.3）。**すべてのテストが `LogicalQuery::validate` と `PhysicalQuery::validate` を通る**（`plan()` がデバッグビルドで実行する）。
- P0-e の間だけ: `new_path_matches_legacy`（§5.2）。
- **plan_golden の書式の予告**（詳細・ルールごとのスナップショットは `04-planner-optimizer.md`）: 次の形のテキストを `tests/plan_golden/*.golden` と比べる。`LogicalQuery` / `PhysicalQuery` に `impl Display` を A が置く（形式の確定は 04）。

  ```text
  -- sql
  SELECT b, a + 1 AS c FROM t WHERE a > 1 ORDER BY b DESC LIMIT 2
  == logical (build)
  Limit limit=2
    Sort [#1 DESC NULLS FIRST]
      Project [#1, #2 := (#0 + 1)]
        Filter (#0 > 1)
          Get t [#0 a, #1 b]
  == after const_fold  (unchanged)
  == physical
  Limit limit=2
    Sort [@0 DESC NULLS FIRST]
      Project [@1, (@0 + 1)]
        Filter (@0 > 1)
          SeqScan t
  ```

  規則: ノードは 1 行 `名前 キー=値`、子は 2 スペースずつ字下げ。論理の列は `#<ColId>`（`Get` の列は `#id 列名`）、物理は `@<位置>`、`Param` は `$<ParamId>`、副問い合わせは `SubPlan#<id>`。演算子は中置、関数は `name(args)`。`span`・OID・型の OID は出さない（安定のため）。各ルールの前後で変わらなかったときは `(unchanged)`。

### 6.4 共有テストと CI

- P0 は slt を足さない。M1〜M3 の slt（`tests/slt/m1`〜`m3`、`tests/restart`）が全段階で通ることが完了条件。
- デバッグビルドの `cargo test` で `validate` が全テストに効く（02-D11）。CI に、リリースビルドのサーバを `-c yuzhu.validate_plans=on` で起動して共有 slt（m1〜）を流すジョブを足す（K と S。M4 の途中から）。

---

## 7. 実装の分担と工数

担当は 00 §17 のとおり（A と P0）。この章の分の工数（AI の実装エージェント 1 本の日数。粗い見積もり）:

| 担当 | 段階 | 日数 | 中身 |
|---|---|---|---|
| A | P0-a | 1.5（00 §17 の A の 1.5 日のうち、この章の分は約 1.0 日。残りは他章の型） | `expr`（型・`walk`・`try_map`・`try_rewrite`・`same_as`・`lower_single_rel`）0.4、Bound の型と `validate` 0.2、論理の型・走査 API・`validate` 0.2、物理の型・`width` / `children` / `uses_params` / `validate` 0.2 |
| P0 | P0-b | 0.5 | 依存のない ★ スタブ（§5.4 の P0-b）、`stubs_return_not_supported` の枠 |
| P0 | P0-c | 1.8 | `eval` の `PhysExpr` 化と `&mut ExecCtx`、既存 11 ノードの `rewind` と割り込み規則、`Update` / `Delete` / `Insert` の新しい形、`ExecCtx::new`、`legacy::to_physical`、テストのヘルパの書き換え、★ スタブ（executor） |
| P0 | P0-d | 1.4 | `Var` 化、`ScopeStack`、`BoundQuery`、`output_column`、`legacy::from_bound`、`analyzer/tests.rs` の形の書き換え、★ スタブ（analyzer）と分岐点の口 |
| P0 | P0-e | 1.3 | `build`（既存ノード）、`physicalize`（既存ノード）、`plan()` の組み立てと検証、`session.rs`、差分テスト、旧コードの削除と名前の付け替え、★ スタブ（planner） |
| 合計 | | 6.5（A 1.5 + P0 5.0） | |

- **段階ごとの解放**（§5.1）により、00 §17 の「P0 のマージ後に N1〜N3・L1・L2・X1〜X3・E1 を並列に」を前倒しできる。P0 の開始を 0 日目とすると、X1〜X3 は 2.3 日目から、N1〜N3 は 3.7 日目から、L1・L2・E1 は 5.0 日目から。A の後の最長経路（P0 → L1/L2 → E1/S → 結合）は変わらない。
- P0 の開始前に、**M3 が `dev` に統合済み**であること（00 は M3 の設計どおりに完成した状態を前提にする）。M3 の実装が `executor/eval.rs`（F）・`session.rs`（S）を変更中の間は、P0-c・P0-d に着手しない（衝突を避ける）。
- P0 の各段階の完了報告に、§5.3 のリスク表のテストの結果（通過）と、`m4-changes.md` への 00 からの変更の記録を含める。
- 同じファイルを 2 人が触る必要が出たら、そのファイルの持ち主に依頼する（M3 §8 と同じ）。例外は `error.rs` の `sqlstate` への定数の追記だけ。

---

## 8. 未検証の点（実装前に確かめるもの）

- PG の内部の `varlevelsup` の値そのもの。実機で観測できる振る舞い（導出表・集合演算の腕・`VALUES` が外側の列を参照できること、導出表が兄弟の列を参照できないこと、`42P01 invalid reference to FROM-clause entry` の文言）は確認済み。§3.4.2 の `levels_up` の数え方は PG の `parse_sub_analyze` の親子関係から導いたもので、数値は未検証。
- 外側レベルの集約（02-D9）の PG の細かい挙動（確認は 1 ケース: `select (select sum(t.a) from u) from t` が `21000`）。`count(*)` のように `Var` を含まない集約、ネストした副問い合わせでの所属レベルは未検証。M4 は `0A000` にするので影響は小さい。
- EXPLAIN の `SubPlan n` / `InitPlan n` の番号の付け方（PG は `SubPlan` の `plan_id`、作成順）。§3.6.3 の後行順の採番との対応は `10` が決める。
- `uses_params` が `SubLink` を含む部分木で常に true になる（02-D7）ことによる性能の劣化（NLJ の内側に副問い合わせがあると、内側の `Materialize` が毎回作り直される）。実際の影響は、4 件の代表的な問い合わせの実行時間で、X1 が確かめる。
- 行値の `IN`（`(a, b) IN (SELECT x, y ...)`）。現在のパーサは `row constructors` を `0A000` にしている（`sql/parser/expr.rs`）。S1 が行値コンストラクタを AST に足す必要がある（`03` と S1 で確認）。
- `ORDER BY` / `GROUP BY` の式に `SubLink` を含むときの一致（`same_as` が常に不一致）。PG は一致とみなす。M4 では `42803` になりうる（§3.1.3）。
- P0-e で `Update` の入力に常に `Project` が入ることによる、単一表 UPDATE の性能への影響（M2 は旧行をそのまま渡していた）。数%のはず。P0-e で、1 万行の UPDATE の時間を M3 と比べて記録する。
- `LogicalQuery.n_subplans_hint`（00 §8）の用途。`Vec::with_capacity` のためだけで、実装者の判断で使わなくてよい。

---

## 9. 確認事項

ユーザーの不在中に仮決めしたことです。仮決めのままでよいか確認してください。この章にはディスク形式に関わるもの（★）はありません。`11-tests-plan.md` が `M4-Q` の通し番号に振り直して集約します。

- **[02-Q1] 外部結合の NULL 側の `ColId` を再発行しない（02-D5）**
  - 仮決め: `Join` の NULL 側の列は同じ `ColId` のまま。結合より上の `Column(c)` は null 拡張後の値を指す（§3.5.4）。
  - 理由: 再発行すると、結合の上のすべての式（`Project`、`Filter`、`Sort`）の `ColId` を書き換える必要がある。PG の `varnullingrels` に相当する情報は結合の位置から判定できる。
  - 変えたい場合の影響: `Join` が NULL 側の列ごとに新しい `ColId` を発行する（`Join.null_cols: Vec<(ColId, ColId)>` のような対応表）。`build`・押し下げ・外部結合の内部結合化・刈り込み・`validate` が増え、約 1.5 日。式の意味が局所的に決まる利点はある。
- **[02-Q2] 移行を下から行う（02-D14）**
  - 仮決め: executor → analyzer → planner の順。各段階の隣に一時的なアダプタ（`planner/legacy.rs`）を置く。
  - 理由: アダプタが自明な構造変換で済む。上からだと `physicalize` を旧型向けと新型向けに 2 回書く。各段階で 1 つの層のテストだけが書き換わる。
  - 変えたい場合の影響: 上から行うと約 +1 日。1 段階の変更の大きさは小さくなる（analyzer だけ → planner だけ → executor だけ）が、旧型向けの `physicalize` が使い捨てになる。
- **[02-Q3] `uses_params()` は `SubLink` を含む部分木で常に true（02-D7）**
  - 仮決め: 00 の定義（`Param` を含むか）に「`SubLink` を含めば true」を足す。署名は変えない。
  - 理由: `PhysExpr` の `SubLink` は `SubPlanId` だけで、副問い合わせの `Param` 参照が式の木から見えない。偽の true は再作成が増えるだけで正しさは保たれる。
  - 変えたい場合の影響: `uses_params(&self, q: &PhysicalQuery)` に署名を変え、`SubPlanDef` を再帰して調べる（`executor::build` も `query` を受け取る）。約 +0.5 日。非相関の InitOnce を含む部分木の再作成が減る。
- **[02-Q4] 外側レベルの集約は `0A000`（02-D9）**
  - 仮決め: `(SELECT sum(t.a) FROM u) FROM t` の形は `0A000 outer-level aggregate functions are not supported yet`。
  - 理由: PG は集約を外側の問い合わせに所属させる（確認済み）。必要性が低く、アナライザの集約の検査と `build` の段の扱いが複雑になる。
  - 変えたい場合の影響: 集約の所属レベルを `levels_up` の最小値で決め、外側の `BoundSelect` の `has_agg` を立てる処理を `03`（N2）と `build` に足す。約 +3 日。
- **[02-Q5] 外側の列を参照する CTE は `0A000`（P8）**
  - 仮決め: CTE の本体が外側の問い合わせの列を参照する（副問い合わせの中の `WITH`）場合は、`MATERIALIZED` でなくても `0A000`。`PhysicalQuery.ctes` のプランは外側で束縛された `Param` を使わない。
  - 理由: CTE を 1 回だけ実行して共有する設計（`CteScan`）と、`Param` による再実行は両立しない。非 `MATERIALIZED` で参照が 1 回の CTE はインライン展開されるので、実用上はほとんど出ない。
  - 変えたい場合の影響: `CteStates` を `Param` の変化で作り直す状態遷移に拡張する（約 +1.5 日）。
- **[02-Q6] 導出表の列名は内側の名前のまま、`Subquery Scan` ノードを持たない**
  - 仮決め: `FROM (SELECT ...) s` の列の `ColId` は内側のものを使い、`ColumnInfo` の名前も内側のまま（§3.5.2）。EXPLAIN の式が `s.x` ではなく `t.a` のように出る。`m4-query.md` §4.4 の EXPLAIN 専用の透過ノード `SubqueryScan` は物理プランに入れない（00 §9.2 のとおり）。
  - 理由: PG もプルアップされた導出表では内側の名前で出す。透過ノードは実行に不要で、EXPLAIN のためだけに論理・物理の両方にノードが増える。
  - 変えたい場合の影響: 論理に `SubqueryScan { alias, input, cols }`（新しい `ColId` を定義して名前を持つ）、物理に透過ノードを足す。約 +1.5 日（`04`・`10` にも波及）。
- **[02-Q7] デバッグビルドでの検証の常時実行と、隠し設定 `yuzhu.validate_plans`（02-D11）**
  - 仮決め: デバッグビルドでは `plan()` が各段階の直後に `validate` を実行する。リリースビルドでは既定 off、`yuzhu.validate_plans = on` で有効。`PlannerSettings` に `validate_plans: bool` を足す（00 への変更提案）。
  - 理由: 全テストで不変条件が検査される。slt を実機サーバで流すときだけ有効にできる。
  - 変えたい場合の影響: 環境変数にする、常時 on にする、など。常時 on は性能への影響（プランの大きさに比例）があり、計測が要る。
- **[02-Q8] Join RTE を指す `Var` を Bound に残さず、アナライザが展開する（02-D2）**
  - 仮決め: `USING` / `NATURAL` の併合列と `j.*` は、アナライザが `JoinColSource` に従って式に展開する。
  - 理由: `build` と `validate` が単純になる（`build` は Join RTE を知らない）。PG は planner で `flatten_join_alias_vars` するが、M4 では 1 回の展開で足りる。
  - 変えたい場合の影響: `Var` が Join RTE を指してよいことにして、`build` が展開する（`JoinColSource` を使う。型の違いの `Cast` を `build` が作るために、カタログの型変換の表が `build` に要る）。`03` と `04` に波及し、約 +1 日。
- **[02-Q9] `levels_up` の数え方（02-D1）**
  - 仮決め: 1 つの `BoundQuery`（本体が Values / SetOp で `rtable` を持たなくても）が 1 レベル。
  - 理由: アナライザのスコープの積み方（`analyze_query` ごとに 1 つ）と一致し、PG の `varlevelsup` とも一致するはず（§8 の未検証）。
  - 変えたい場合の影響: `rtable` を持つレベルだけを数える方式にすると、`Var` の値は小さくなるが、アナライザのスコープの積み方とずれる。`03`（N3）の書き直し。約 +0.5 日。
- **[02-Q10] `SubLink` を含む式の `same_as` が常に不一致（§3.1.3）**
  - 仮決め: `GROUP BY (SELECT ...)` の式を SELECT に書き直すと `42803`（PG は通す）。
  - 理由: 副問い合わせの木の等価判定は、`BoundQuery` と `LogicalSubquery` の比較を実装する必要があり、M4 の必要性に見合わない。
  - 変えたい場合の影響: `BoundQuery` の構造的な等価判定（`span` を無視する）を `03` に足す。約 +1 日。

---

## 10. 00 への変更提案

00 の**署名は変えない**。次は追加と意味の明確化で、統合時に 00 に反映してほしい。

| # | 00 の場所 | 提案 | 理由 |
|---|---|---|---|
| 1 | §6.1 `Var.levels_up` のコメント | 「0 = 同じ SELECT のスコープ。1 以上 = 外側の SELECT」を「**0 = 同じ `BoundQuery`（または DML 本体）のレベル。k = 外側へ k 個目のレベル。本体が Values / SetOp のレベルも数える**」に（02-D1） | 数え方を 1 つに固定する |
| 2 | §6.5 `try_map` | 規則（葉扱いの 3 変種、`f` が `None` のときの再構築）を本章 §3.1.4 のとおりに明記。`try_rewrite`、`any`、`contains_aggregate`、`contains_sublink`、`conjuncts`、`same_as`、`columns`、`Expr::{new, literal, column, null_of, bool_lit, and_all}`、`Var::{with_levels_up, is_system, is_local}` を追加 | 追加のみ |
| 3 | §6.2 `Expr.span` の注記 | 「論理・物理の層では `Span::default()` でよい」を「変換元の span を保つ（`try_map` が複製するので費用はない）」に | エラーの診断に使える |
| 4 | §6.4 の表 | 本章 §3.2.1 の表に置き換える（物理の `SubLink.test` は常に `None`。比較式は `SubPlanDef.test`。02-D10） | 二重に持たない |
| 5 | §7 `JoinColSource` | Join RTE を指す `Var` は Bound に残さない（02-D2）。展開の規則（§3.4.3）。`BoundStatement::returns_rows()` を追加。`BoundQuery::validate` を追加 | `build` の単純化 |
| 6 | §8 | `LogicalPlan::{output_cols, defines, children, children_mut, exprs, exprs_mut, outer_refs}`、`LogicalSubquery::outer_refs`、`LExpr::{free_cols, substitute}`、`LogicalQuery::validate`、`LogicalPlan::validate` を追加。`LogicalQuery.n_subplans_hint` は使わなくてよい | ルールと `physicalize` の共通の走査 |
| 7 | §9.1 | `PlannerSettings` に `validate_plans: bool`（隠し設定 `yuzhu.validate_plans`。既定 `cfg!(debug_assertions)`）を追加（[02-Q7]） | 検証の運用 |
| 8 | §9.2 | `uses_params` の定義に「`SubLink` を含めば true」を追加（02-D7）。`PhysicalPlan::{children, width, exprs_with_width}`、`PhysExpr::uses_params`、`PhysicalPlan::validate`、`PhysicalQuery::{single, validate}` を追加。`Values` は 1 行以上。論理 `Empty` は `Result { one_time_filter: Some(false) }` に物理化する | 検証・計測の共通部品 |
| 9 | §9.3 | `SubPlanId` / `ParamId` の採番規則（§3.6.3: 出現ごと・後行順・外側の `Param` の継承）、`ExplainNode.children` の先頭は `plan.children()` と 1 対 1（§3.6.5）、計測の通し番号（§3.6.4）を追記 | `10` と `05` の前提 |
| 10 | §10 | `ExecEnv`、`ExecCtx::new`、`EvalCtx.type_env`、`eval_with_sub`（内部）を追加。`Executor::rewind` は未開始のノードにも呼べる（02-D16）。`next` / `rewind` の表（§3.7.1）、`check_interrupts` の規則（§3.7.4）を追記 | `05` の前提 |
| 11 | §4 構成図 | `analyzer/query.rs` と `planner/legacy.rs` は P0 の一時ファイル（P0-e で消える）と注記。`planner/plan.rs` は `physical.rs` に置き換わる | 移行手順 |
| 12 | §17 P0 の行 | 「P0 が全員の足場を置く」を、段階ごと（P0-b: 依存のない ★、P0-c: executor、P0-d: analyzer、P0-e: planner）に置く、に。段階ごとの解放（§5.1）を追記 | クリティカルパスの前倒し |
| 13 | §16 の表 | 02 の行に、`Executor` の `rewind` の意味、`Update` / `Delete` の入力の形、`BoundQuery::validate` を追記 | 変更点の網羅 |
| 14 | §4 の `storage/stack.rs`・`engine.rs`、§17 | `StorageStack.index` / `seq` と `Cluster::indexes()` / `sequences()` を P0-b が（スタブの実装で）足す。`bootstrap.rs` は C1、`engine.rs` の本実装は S が引き継ぐ | `ExecEnv.indexes` を P0-c から使うため |
