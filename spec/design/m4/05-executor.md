# yuzhu M4 05: 実行ノードと DML（05-executor）

M4 の executor の契約です。式評価（`eval`）、サブプラン、物理ノードごとの仕様、集約、メモリ予算、DML（インデックス維持・RETURNING）、EXPLAIN ANALYZE の計測の差し込み口、テスト、実装の分担を定めます。`00-contracts.md` の署名と名前（特に §6 式の木、§9 物理プラン、§10 executor の契約、§12.2 比較・ハッシュ、§13 ストレージ）に従います。

- 前提（必読）: `00-contracts.md`、`spec/design/m2.md` §4.7・§5.5・§6.5、`m3.md` §5.1、`QUESTIONS.md`
- 調査（根拠）: `spec/research/m4-query.md` §5（集約）・§6（実行器）、`m4-btree.md` §6（一意性検査）、`m2-dml-exec.md` §5.4（SelfModified）
- 正解の基準は PostgreSQL 17。実機（`127.0.0.1:55432`）で確かめたものは「（検証済み）」、確かめていないものは「（未検証）」と書く。
- この章の担当は X1（結合・サブプラン）、X2（集約・メモリ・ハッシュ・単純ノード）、X3（インデックススキャンと DML）。既存ノードの `PhysExpr` 化と `rewind` 化は P0（`02-pipeline-refactor.md`）。

---

## 1. 範囲

### 1.1 この章で定めるもの

| 分類 | 内容 |
|---|---|
| 式評価 | `PhysExpr` の評価（`PhysCol::Param`、`SubLink`、`SubLinkOutput`）、サブプランの実行（Rescan / InitOnce / Hashed）、Any / All の三値論理 |
| 物理ノード | SeqScan（filter 付き）、IndexScan、FunctionScan、Result、Values、Filter、Project、Sort、Unique、Distinct、Limit、Materialize、NestedLoopJoin、NestedLoopParam、HashJoin、Aggregate、HashAggregate、GroupAggregate、Append、HashSetOp、CteScan |
| 集約 | `AggState`（`AggKind` ごとの初期値・遷移・結果型・オーバーフロー）、DISTINCT、FILTER |
| DML | `executor/dml.rs`（`insert_with_indexes` / `update_with_indexes`）、Insert / Update / Delete ノード、RETURNING、コマンドタグ、23505 / 23502 / 23514 のメッセージとフィールド |
| メモリ | `MemBudget`、課金と解放の規則、53200 |
| 計測 | EXPLAIN ANALYZE の計測ラッパーの差し込み口 |
| 部品 | `types/hash.rs`（`hash_datum`、`HashKey`。X2 の持ち主） |

### 1.2 この章で定めないもの

- 物理プランの木を作る規則、インデックス選択、ビルド側の選択、`PhysicalPlan` / `PhysExpr` の型の定義: `04-planner-optimizer.md`、`02-pipeline-refactor.md`
- B+Tree の探索・挿入・一意性検査の内部: `06-btree.md`（この章は `IndexStore` の呼び出し側）
- SERIAL / IDENTITY の書き換えとシーケンス: `08-sequence-serial.md`（executor は IDENTITY を知らない。D5-14）
- EXPLAIN の整形と deparse、`instrument.rs` の本体、COPY: `10-explain-copy-compat.md`
- 集約関数の表（`AGGREGATES`）と numeric の演算: `09-types-functions.md`、`yuzhu-numeric`

### 1.3 M4 では対応しない（`0A000` または起きない）

ディスクへのスピル、マージ結合、Index Only Scan、Bitmap Scan、並列実行、行ロック（FOR UPDATE）、トリガ、`ON CONFLICT`、`WITH RECURSIVE`、`LATERAL`、データ変更を含む `WITH`、ウィンドウ関数、`GROUPING SETS`、集約内の `ORDER BY`、他トランザクションの待ち（`TmResult::BeingModified` など。単一ライターなので起きない）。

---

## 2. 決定（この章で追加で決めたこと）

| # | 論点 | 決定 | 理由 |
|---|---|---|---|
| D5-1 | 実行モデル | M1 以来の Volcano（行ごとの `next`、`Row = Vec<Datum>`）のまま。ベクトル化しない | 契約が変わらず、M4 の範囲では性能目標（pgbench の小さな問い合わせ）に足りる |
| D5-2 | 全ノードの共通規則 | (a) 枯渇後の `next` は子を呼ばずに `None` を返し続ける（fused）。(b) `Err` を返した後のノードは再利用しない（文が中断する）。(c) `rewind` の後は先頭から再実行できる | NLJ の SEMI の早期終了、Limit の打ち切りで子が途中のまま残っても、親が `rewind` で確実に先頭へ戻せるようにする |
| D5-3 | 溜めた結果を `rewind` で再利用してよいか | `PhysicalPlan::uses_params()` ではなく、executor 内部の `free_params(plan, query)`（SubLink の副問い合わせと NestedLoopParam の束縛を越えて「外から値を受ける `ParamId` の集合」を求める）が空かどうかで決める。決定は `build_scoped` が各ノードを作る時点 | `uses_params()` は SubPlan の `SubPlanDef.params` を木の外に持つため SubLink を越えられず、また NestedLoopParam が自分で束縛する `Param` も「依存」と数える。前者は誤って再利用し（誤り）、後者は不要に作り直す（遅い）。00 の `uses_params()` は他の章のために残す（00 への変更提案 05-P1） |
| D5-4 | 式評価の可変性 | `eval(expr, row, &mut ExecCtx)`（00 §10）。SubLink 以外の評価は `EvalCtx` だけを使う。`EvalCtx` に `type_env` と `params` を足す | 00 の署名どおり。キャスト（`InOut`）が `TypeEnv` を要る。DEFAULT / CHECK / RETURNING は `eval_const`（`&ExecCtx` だけ）で足りる |
| D5-5 | `SubLinkOutput` の評価 | 評価器は「副問い合わせの現在の行」`sub_row: Option<&Row>` を持つ。`eval_with_sub_row(expr, row, sub_row, ctx)` を `pub(crate)` で提供し、`subplan.rs` が Any / All の `test` に使う | `test` は外側の行（`Column(Local)`）と副問い合わせの行（`SubLinkOutput`）の両方を参照する |
| D5-6 | InitOnce の範囲 | Scalar / Exists は 1 つの値を、Any / All は副問い合わせの全行（`Vec<Row>`）を保持する。どの種類でも 1 回だけ実行する | 00 は InitOnce の対象種類を限定していない。非相関で等値でない `x < ALL (SELECT ...)` も 1 回の実行で済ませる |
| D5-7 | Hashed の意味 | PostgreSQL の `ExecHashSubPlan` と同じ**正確な三値論理**（NULL を含む行は別に保持し、NULL を含む probe は「部分一致」を調べる）。単一キーでは 00 の簡約（一致 → true、不一致で NULL 行あり or lhs が NULL → NULL、それ以外 false、空集合は常に false）と一致する | 複数列 `(a, b) IN (SELECT x, y ...)` は簡約だと誤る。`(1, NULL) IN (SELECT 2, 5)` は false、`(1, 2) IN (SELECT 1, NULL)` は NULL（検証済み） |
| D5-8 | NestedLoopJoin の向き | `outer` = 論理プランの left、`inner` = right。出力は `outer ++ inner`。FULL は実行できない（`Error::internal`）。入れ替えが必要なら planner が Project で並べ直す | 00 の「出力は常に左 ++ 右」を NLJ にも当てはめる。RIGHT は planner が LEFT に直す |
| D5-9 | HashJoin の一般化 | 「probe 側の保存」と「build 側の保存」の 2 つの印で全種類（INNER / LEFT / FULL / SEMI / ANTI × `build_is_left`）を 1 つのアルゴリズムで扱う（§5.15）。SEMI / ANTI も `build_is_left = true` を実装する（planner は使わない） | 場合分けの重複を避け、結合アルゴリズムの相互比較テストで全組み合わせを網羅できる |
| D5-10 | 集約の共通部品 | `executor/agg.rs` の `AggSet`（FILTER → 引数評価 → NULL 除外 → DISTINCT → 遷移）を Aggregate / HashAggregate / GroupAggregate が共有する | 3 ノードで意味を揃える |
| D5-11 | HashAggregate の出力順 | グループの**初出順**。PostgreSQL の順序は不定なので、テストは必ず ORDER BY か rowsort | 決定的な出力でテストを安定させる |
| D5-12 | `avg(float)` | 入力順に `f64` で足し、`n` で割る（`float8_accum` の `Sx` と同じ）。結果は float8。`Sxx`（分散用）は持たないので、極端な値でのオーバーフローの有無が PostgreSQL と違いうる（未検証） | M4 に分散系の集約はない |
| D5-13 | HashSetOp に UNION も入れる | `op = Union` も実装する（planner は Append + Distinct を使うので到達しない）。データ構造が同じで 5 行で済む | 契約の `SetOpKind` を網羅する |
| D5-14 | IDENTITY・SERIAL | executor は知らない。DEFAULT 式（`nextval`）と `column_map` だけで動く。`OVERRIDING USER VALUE` は analyzer が `column_map` を `None` にして表す。ALWAYS への明示値（428C9）も analyzer が検出する | `08-sequence-serial.md` と D-10（解析段階の書き換え）に従う |
| D5-15 | DML の順序 | NOT NULL → CHECK（名前の昇順）→ ヒープ → インデックス（OID 昇順）。PostgreSQL の `ExecConstraints` → `heap_insert` → `ExecInsertIndexTuples` と同じ | 複数の違反があるときに最初に報告されるものを PostgreSQL に揃える（検証済み: PK と UNIQUE の両方に違反すると OID の小さい PK が報告される） |
| D5-16 | 23505 の DETAIL | `IndexStore::insert` は 23505 と `s` / `t` / `n` を付けたエラーを返す（`detail` は空でもよい）。`dml.rs` が `detail` が空なら `Key (a)=(1) already exists.` を補う。補う関数 `unique_violation_detail` は `dml.rs` が公開し、CREATE INDEX の経路（`could not create unique index`）は `06-btree.md` が自分で書く | `IndexStore::insert` が `TypeEnv` を受け取らないので、日時を含むキーの出力が B+Tree 側で決まらない |
| D5-17 | RETURNING の評価行 | INSERT / UPDATE は格納した**新しい行**（全ユーザー列）、DELETE は**削除した古い行**。式は `PhysCol::Local(i)` = 対象表の i 番目のユーザー列だけを参照する（00 §7、§8 の約束）。FROM 句の表の列を参照する RETURNING は analyzer が `0A000` にする | 00 の `BoundReturning` は対象表の Var だけ |
| D5-18 | RETURNING つき DML | ノードは遅延実行（`next` ごとに入力 1 行を処理して RETURNING の行を 1 つ返す）。session は `None` が返るまで必ず読み切る | PostgreSQL の ModifyTable と同じ。読み切らないと文が完了しない |
| D5-19 | CHECK 制約の評価順 | 名前の昇順（PostgreSQL の `ExecRelCheck` が名前順に並べた `ccname` の順に評価する。未検証: 並べ替えの箇所）。`RowChecker::new` が整列する | 複数の CHECK に違反する行でどの制約名が報告されるかを揃える |
| D5-20 | 相関のある MATERIALIZED CTE | 実行しない。`PhysicalQuery.ctes[i]` が `free_params` を持つ場合は、最初の `next` で `Error::internal`。planner が `0A000` で拒否する（確認事項 05-Q5） | 共有ストアを複数の `CteScan` が使うため、`rewind` ごとの作り直しが難しい。実用上まれ |
| D5-21 | `generate_series` | `int4`・`int8` の 2 引数・3 引数。NULL 引数は 0 行、`step = 0` は 22023、`step` の向きと逆なら 0 行、桁あふれは「そこで終わり」（エラーにしない。検証済み） | PostgreSQL と同じ |
| D5-22 | 課金の保持と解放 | 課金は文の終わりまで保持するのが原則。例外として (a) 再構築の前に自分の課金を返す、(b) `rewindable = false` のノードは入力を読み切った時点で自分の課金を返す（§8.3） | 実メモリより多めに数える（安全側）が、直列に並ぶ Sort・HashJoin の合計で上限を超えるのを避ける |

---

## 3. 型とトレイト（契約の具体化）

### 3.1 `Executor` と `ExecCtx`（00 §10 への追加点）

```rust
// executor/mod.rs
pub trait Executor {
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>>;
    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()>;
    fn rows_affected(&self) -> u64 { 0 }
    /// EXPLAIN ANALYZE 用の累計値。("Rows Removed by Filter", n) など。既定は空（00 への変更提案 05-P2）
    fn extra_stats(&self) -> Vec<(&'static str, u64)> { Vec::new() }
}

/// 00 §10 の ExecCtx のフィールドに加えて、この章が前提にするヘルパ
impl ExecCtx<'_> {
    pub fn param(&self, id: ParamId) -> Result<&Datum>;                 // 範囲外は Error::internal
    pub fn set_param(&mut self, id: ParamId, v: Datum) -> Result<()>;
}

/// 文の開始時に session が作る、問い合わせごとの状態
pub struct QueryState { pub params: Vec<Datum>, pub mem: MemBudget, pub subplans: SubPlanStates, pub ctes: CteStates }
impl QueryState {
    /// params = vec![Datum::Null; query.n_params]、mem = MemBudget::new(mem_limit)。
    /// subplans / ctes はスロットだけ作り、Executor は最初に使うときに build する
    pub fn new(query: &PhysicalQuery, mem_limit: usize, opts: &BuildOptions) -> QueryState;
}

// 式評価の文脈（M3 の EvalCtx に追加。05-P3）
#[derive(Clone, Copy)]
pub struct EvalCtx<'a> {
    pub session: &'a SessionInfo, pub catalog: &'a dyn CatalogReader, pub runtime: &'a dyn RuntimeInfo,
    pub type_env: &'a TypeEnv<'a>,      // ★
    pub params: &'a [Datum],            // ★ 単体テストでは空でよい
}
```

### 3.2 `build` と計測・再利用の判定

```rust
// executor/build.rs（P0 が足場を置き、各担当が自分のノードの分岐を埋める）
#[derive(Clone, Default)]
pub struct BuildOptions { pub instrument: Option<Arc<instrument::InstrumentSink>> }

/// 00 §10 の署名。subplans を持たない単体テスト用（SubLink は「パラメータ依存」とみなす）
pub fn build(plan: &PhysicalPlan) -> BoxedExecutor;
/// 本番の入口。root を `PlanScope::Root`、rewindable = false で build する
pub fn build_query(query: &PhysicalQuery, opts: &BuildOptions) -> BoxedExecutor;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanScope { Root, SubPlan(SubPlanId), Cte(usize) }       // instrument.rs と共有（10 章が定義）

pub(crate) struct BuildEnv<'a> { pub query: Option<&'a PhysicalQuery>, pub opts: &'a BuildOptions, /* 先行順の通し番号 Cell<u32> */ }
/// rewindable: このノードが（親または祖先によって）rewind されうるか
pub(crate) fn build_scoped(plan: &PhysicalPlan, scope: PlanScope, env: &BuildEnv<'_>, rewindable: bool) -> BoxedExecutor;

/// 外から値を受ける ParamId の集合。reads - bound。
///   reads : 木の式に現れる PhysCol::Param、および木の中の SubLink(id) について
///           SubPlanDef.params / test / strategy の probe_keys・build_keys の式の Param、
///           free_params(def.plan) から def.params の ParamId を除いたもの
///   bound : NestedLoopParam.params の ParamId（inner の中の reads から除く）
/// query = None のとき、SubLink を含む木は「空でない」とみなす（保守的）
pub(crate) fn free_params(plan: &PhysicalPlan, query: Option<&PhysicalQuery>) -> BTreeSet<ParamId>;
```

規則:

- 子を持つノードは `reuse = free_params(子).is_empty()` を作る時点で決めて保持する（D5-3）。
- `rewindable(子) = rewindable(親) || 子は NestedLoopJoin / NestedLoopParam の inner`。root は false。SubPlan / InitPlan / CTE の plan の root は true（`SubPlanStates` / `CteStates` が `rewindable = true` で build する）。
- `free_params` の実装に `PhysicalPlan::children()` / `exprs()` / `bound_params()` が要る（00 への変更提案 05-P1）。P0 が `physical.rs` に足せない場合は `build.rs` 内の `match` で書く。

### 3.3 サブプランと CTE の状態

```rust
// executor/subplan.rs（X1）
pub struct SubPlanStates { slots: Vec<SubPlanSlot> }
struct SubPlanSlot { exec: SlotExec, started: bool, cache: SubPlanCache }
enum SlotExec { Unbuilt, Idle(BoxedExecutor), Lent }
enum SubPlanCache {
    None,
    Scalar(Datum),                    // InitOnce の Scalar
    Exists(bool),                     // InitOnce の Exists
    Rows { rows: Vec<Row> },          // InitOnce の Any / All（全行を保持）
    Hashed(HashedSubPlan),
}
impl SubPlanStates {
    pub fn new(query: &PhysicalQuery, opts: &BuildOptions) -> SubPlanStates;
    fn take(&mut self, id: SubPlanId, query: &PhysicalQuery) -> Result<(BoxedExecutor, bool)>;   // Unbuilt なら build、Lent なら internal
    fn put_back(&mut self, id: SubPlanId, exec: BoxedExecutor, started: bool);
}
/// SubLink 式の評価（eval.rs から呼ぶ）。種類は SubPlanDef.kind
pub fn eval_sublink(id: SubPlanId, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Datum>;

pub struct HashedSubPlan {
    n_keys: usize,
    set: HashSet<HashKey>,            // キー列がすべて非 NULL の行
    null_rows: Vec<Vec<Datum>>,       // NULL を含むキー行
    full_rows: Vec<Vec<Datum>>,       // n_keys >= 2 のときだけ。set のキー行（NULL を含む probe の部分一致走査用）
}

// executor/nodes/cte_scan.rs（X2）
pub struct CteStates { slots: Vec<CteSlot> }
struct CteSlot { exec: SlotExec, rows: Vec<Row>, done: bool, free: bool /* free_params が空か */ }
impl CteStates { pub fn new(query: &PhysicalQuery, opts: &BuildOptions) -> CteStates; }
```

### 3.4 集約の部品

```rust
// executor/agg.rs（X2）
#[derive(Debug, Clone)]
pub enum AggState {
    CountStar(i64), Count(i64),
    SumInt2(Option<i64>), SumInt4(Option<i64>), SumInt8(Option<i128>),
    SumFloat4(Option<f32>), SumFloat8(Option<f64>), SumNumeric(Option<Numeric>),
    AvgInt { count: i64, sum: i128 },                 // AvgInt2 / AvgInt4 / AvgInt8
    AvgFloat { count: i64, sum: f64 },                // AvgFloat4 / AvgFloat8
    AvgNumeric { count: i64, sum: Numeric },
    MinMax { max: bool, cur: Option<Datum> },         // Min / Max
    BoolAnd(Option<bool>), BoolOr(Option<bool>),
}
impl AggState {
    pub fn new(kind: AggKind) -> AggState;
    /// 非 NULL の引数列（count(*) は空）で 1 回遷移する。NULL・FILTER・DISTINCT の判定は呼び出し側が済ませている
    pub fn transition(&mut self, args: &[Datum]) -> Result<()>;
    pub fn result(&self) -> Result<Datum>;           // 非消費。rewind での再利用のため
    pub fn heap_bytes(&self) -> usize;
}

pub struct AggSet { /* kinds、args、filter、distinct の写し */ }
pub struct AggGroup { pub states: Vec<AggState>, distinct: Vec<Option<HashSet<HashKey>>> }
impl AggSet {
    pub fn new(aggs: &[PhysAgg]) -> AggSet;
    pub fn new_group(&self) -> AggGroup;
    pub fn base_bytes(&self) -> usize;                                   // 1 グループの固定の見積り
    /// 1 入力行を全集約に流す。DISTINCT の新しい値だけ ctx.mem に課金する
    pub fn accumulate(&self, g: &mut AggGroup, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<()>;
    pub fn finish(&self, g: &AggGroup) -> Result<Vec<Datum>>;
}
```

### 3.5 ハッシュ（`types/hash.rs`。X2）

```rust
pub fn hash_datum(d: &Datum, state: &mut dyn std::hash::Hasher);
#[derive(Clone, Debug)] pub struct HashKey(pub Vec<Datum>);
impl PartialEq for HashKey { /* 各要素 cmp_datum == Equal。NULL どうしも等しい */ }
impl Eq for HashKey {}
impl std::hash::Hash for HashKey { /* 各要素に hash_datum */ }
```

`hash_datum` の正規化（`cmp_datum` が Equal を返す 2 値は同じハッシュ）:

| 変種 | 書き込み |
|---|---|
| Null | タグ 0 |
| Bool | タグ + u8 |
| Int2 / Int4 / Int8 | タグ 1 + `i64`（幅違いが等しい） |
| Float4 / Float8 | タグ 2 + `f64` のビット（Float4 は f64 に広げる）。`-0.0` は `0.0`、すべての NaN は正準な NaN のビット |
| Text | タグ 3 + バイト列 + 0xFF |
| BpChar | Text と同じ書き方で、**末尾の空白（U+0020）を除いてから**書く |
| Numeric | `yuzhu_numeric::Numeric` の `Hash`（`1.10` と `1.1` が等しい。クレートが正規化済み） |
| Date / Timestamp / TimestampTz | タグ + `i32` / `i64`（±infinity は番兵値のまま） |
| Oid / Xid / Cid / Char | 変種ごとのタグ + 値 |
| Tid | タグ + block + offset |
| OidVector / Int2Vector / Int4Array | タグ + 長さ + 要素（Int4Array の `None` は専用タグ） |
| Void | タグ |

整数と浮動小数のように型をまたぐ値は、プランナが型をそろえてから渡す（00 §4.3 の 2）。

### 3.6 `dml.rs` の公開面（X3）

```rust
// executor/dml.rs（INSERT・UPDATE・COPY が共有）
pub fn insert_with_indexes(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid>;
pub fn update_with_indexes(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, tid: Tid, new_row: &[Datum]) -> Result<UpdateOutcome>;

/// 挿入する行の組み立て（Insert ノードと COPY が共有）
pub struct RowBuilder { column_map: Vec<Option<usize>>, defaults: Vec<Option<PhysExpr>> }
impl RowBuilder {
    pub fn new(column_map: Vec<Option<usize>>, defaults: Vec<Option<PhysExpr>>) -> RowBuilder;
    /// 各表列 i について、column_map[i] = Some(j) なら input[j]、None なら defaults[i]（なければ NULL）を eval_const で評価
    pub fn build(&self, ctx: &ExecCtx<'_>, input: &Row) -> Result<Row>;
}
/// NOT NULL と CHECK の検査（Insert・Update・COPY が共有）
pub struct RowChecker { rel_oid: Oid, table_name: String, not_null: Vec<bool>, checks: Vec<PhysCheck> }
impl RowChecker {
    pub fn new(rel_oid: Oid, table_name: String, not_null: Vec<bool>, checks: Vec<PhysCheck>) -> RowChecker;  // checks を名前の昇順に整列（D5-19）
    pub fn check(&self, ctx: &ExecCtx<'_>, row: &Row) -> Result<()>;
}
pub fn unique_violation_detail(index: &IndexHandle, key: &[Datum], env: &TypeEnv<'_>) -> String;
pub fn failing_row_detail(table: &TableDef, row: &Row, env: &TypeEnv<'_>) -> String;   // M2 の failing_row を TypeEnv 対応に
```

---

## 4. 式評価（`executor/eval.rs`）

### 4.1 変更点

M2/M3 の `eval_expr` を `PhysExpr`（`Expr<PhysCol, SubPlanId>`）に移す。変更は次のとおり（P0 が機械的に行い、SubLink の分岐は X1 が `subplan::eval_sublink` に委ねる）。

```rust
pub fn eval(expr: &PhysExpr, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Datum>;
pub fn eval_pred(expr: &PhysExpr, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Option<bool>>;       // NULL = None
pub fn eval_const(expr: &PhysExpr, row: &Row, ctx: &EvalCtx<'_>) -> Result<Datum>;                 // SubLink は Error::internal
pub(crate) fn eval_with_sub_row(expr: &PhysExpr, row: &Row, sub_row: &Row, ctx: &mut ExecCtx<'_>) -> Result<Datum>;
```

評価器は `enum Env<'e, 'a> { Full(&'e mut ExecCtx<'a>), Const(&'e EvalCtx<'a>) }` と `sub_row: Option<&Row>` を持つ。関数呼び出しやキャストの直前に `EvalCtx` をその場で作る（`ctx.eval_ctx()`）ので、`&mut ExecCtx` と衝突しない。

| 式 | 評価 |
|---|---|
| `Column(PhysCol::Local(i))` | `row[i].clone()`。範囲外は `Error::internal` |
| `Column(PhysCol::Param(p))` | `ctx.params[p]` の複製。範囲外は `Error::internal` |
| `Aggregate` | `Error::internal`（物理式に現れない） |
| `SubLinkOutput(i)` | `sub_row[i]` の複製。`sub_row` が `None` なら `Error::internal` |
| `SubLink { query, .. }` | `Env::Full` なら `subplan::eval_sublink(query, row, ctx)`、`Env::Const` なら `Error::internal`（"subquery in a constant context"） |
| それ以外 | M1〜M3 のとおり（左から右、AND / OR は決定的な側で打ち切る三値論理、CASE / COALESCE は遅延評価、STRICT は NULL で呼ばない）。キャストの `InOut` は `ec.type_env` を使う |

### 4.2 SubLink の評価（`subplan.rs`）

```text
eval_sublink(id, row, ctx):
    query = ctx.query                          // &'a PhysicalQuery（参照を複製してから def を借りる。ctx の可変借用と衝突しない）
    def = &query.subplans[id]
    match def.strategy:
        InitOnce → eval_init_once(def, ..)
        Hashed   → eval_hashed(def, ..)
        Rescan   → eval_rescan(def, ..)
```

#### 4.2.1 take して戻す手順（全戦略共通）

`SubPlanStates` は Executor を `Idle` として持つ。使うときは `take` で所有権を取り出し、`ctx` を可変借用したまま `exec.next(ctx)` を呼び、終わったら（エラーの場合も）`put_back` する。`take` 時に `Lent` だったら `Error::internal`（副問い合わせが自分自身を評価している。起きない）。

```rust
fn with_subplan<R>(ctx: &mut ExecCtx<'_>, id: SubPlanId,
                   f: impl FnOnce(&mut BoxedExecutor, &mut bool /* started */, &mut ExecCtx<'_>) -> Result<R>) -> Result<R> {
    let (mut exec, mut started) = ctx.subplans.take(id, ctx.query)?;   // Unbuilt なら build_scoped(.., PlanScope::SubPlan(id), rewindable = true)
    let r = f(&mut exec, &mut started, ctx);
    ctx.subplans.put_back(id, exec, started);
    r
}
```

`started` は「`next` を 1 回でも呼んだか」。`started` なら次の実行の前に `rewind` する（`Exists` の早期終了などで途中のまま残るため）。

#### 4.2.2 Rescan（相関あり）

```text
eval_rescan(def, row, ctx):
    1. vals = [eval(e, row, ctx)? for (_, e) in def.params]        // 外側の行に対して評価（先に全部評価してから代入）
       for ((p, _), v) in zip(def.params, vals): ctx.set_param(p, v)
    2. with_subplan: if started { exec.rewind(ctx)? }; started = true     // params を設定してから rewind（再構築する子が params を読む）
    3. kind ごとに行を引く（§4.2.3）
```

実行のたびに副問い合わせを走らせる（1 要素の結果キャッシュは持たない。副問い合わせに volatile 関数があるときに誤るため。確認事項 05-Q9）。副問い合わせの行を読むループは行ごとに `ctx.check_interrupts()?`。

#### 4.2.3 種類ごとの処理

| 種類 | 手順 | NULL・境界 |
|---|---|---|
| Scalar | `next` → `None` なら NULL。`Some(r)` なら値 = `r[0]`。続けて `next` を呼び、`Some` が返れば `21000 more than one row returned by a subquery used as an expression` | 値が NULL のスカラーは NULL。0 行は NULL（検証済み） |
| Exists | `next` が `Some` なら true、`None` なら false。読み切らない | 副問い合わせの列の NULL は無関係 |
| Any | 各行 `r` について `t = eval_with_sub_row(test, row, r)`。true なら即 true を返す。NULL なら `saw_null = true`。最後に `saw_null` なら NULL、でなければ false | 行なしは false |
| All | 同様。false なら即 false。NULL なら `saw_null`。最後に `saw_null` なら NULL、でなければ true | 行なしは true |

三値論理の真理値表（行ごとの `test` の結果の並びに対する全体の結果）:

| 種類 | test の結果の並び | 結果 |
|---|---|---|
| Any | true を 1 つでも含む | true |
| Any | true がなく、NULL を含む | NULL |
| Any | すべて false、または行なし | false |
| All | false を 1 つでも含む | false |
| All | false がなく、NULL を含む | NULL |
| All | すべて true、または行なし | true |

`x IN (...)` は Any、`x NOT IN (...)` は `Not(Any)`（外側の `Not` は通常の式として NULL を保つ）。`x <> ALL (...)` は All。実機（検証済み、`t.a` = {1, 2, NULL, 2}）:

| 式 | 結果 |
|---|---|
| `1 IN (..)` | true |
| `5 IN (..)` | NULL |
| `5 NOT IN (..)` | NULL |
| `2 NOT IN (..)` | false |
| `NULL IN (空)` | false |
| `NULL IN (空でない)` | NULL |
| `5 <> ALL (..)` | NULL |
| `5 < ALL (..)` | false |
| `0 < ALL (空)` | true |

#### 4.2.4 InitOnce

1 回だけ実行して保持する（D5-6）。`def.params` は空。最初の評価で `with_subplan` を呼び、結果を `SubPlanCache` に入れる。2 回目以降は実行しない。

| 種類 | 保持するもの |
|---|---|
| Scalar | `Scalar(Datum)`（2 行目があれば 21000 でキャッシュしない） |
| Exists | `Exists(bool)` |
| Any / All | `Rows { rows }`。全行を `ctx.mem.charge(estimate_row_bytes)` して溜める。評価は §4.2.3 の Any / All を溜めた行に対して行う |

実行中にエラーが起きたらキャッシュしない（文が中断する）。

#### 4.2.5 Hashed

非相関の `x IN (SELECT ...)` / `(a, b) IN (SELECT ...)`（種類は Any だけ。それ以外が来たら `Error::internal`）。`test` は使わない（EXPLAIN 用）。

構築（最初の評価で 1 回）: 副問い合わせを読み切る。各行 `r` について `key = [eval(e, r, ctx) for e in build_keys]`（`r` に対して評価。`PhysCol::Local` = 副問い合わせの出力列）。

- すべて非 NULL → `set.insert(HashKey(key))`。`n_keys >= 2` なら `full_rows.push(key)`。
- NULL を含む → `null_rows.push(key)`。
- 各行を `ctx.mem.charge(estimate_row_bytes(key) + HASH_ENTRY_OVERHEAD)` で課金。行ごとに `check_interrupts`。

探索: `probe = [eval(e, row, ctx) for e in probe_keys]`（外側の行に対して評価）。

```text
if set.is_empty() && null_rows.is_empty():                       return false          // 空集合
if probe に NULL がない:
    if set.contains(HashKey(probe)):                             return true
    if null_rows のどれかが probe と部分一致:                       return NULL
    return false
else:                                                            // probe に NULL がある
    if set 側の行（n_keys == 1 なら set が空でないこと、>= 2 なら full_rows）または null_rows のどれかが部分一致: return NULL
    return false
```

「部分一致」: 全キー列 `i` について `probe[i]` が NULL、`row[i]` が NULL、または `cmp_datum(probe[i], row[i]) == Equal`（= 確定した不一致の列がない）。`n_keys == 1` では、probe が NULL なら集合が空でない限り部分一致、probe が非 NULL なら `null_rows` が空でなければ部分一致になる（00 の簡約と同じ）。複数列は `null_rows` / `full_rows` を線形走査する（PostgreSQL の `findPartialMatch` と同じ O(n)）。

実機（検証済み。`u` = {(2,5), (1,NULL)}）: `(1,NULL) IN (SELECT 2,5)` = false、`(1,NULL) IN (SELECT 1,5)` = NULL、`(1,NULL) IN (SELECT 2,NULL)` = false、`(NULL,NULL) IN (SELECT 1,1)` = NULL、`(1,2) IN (SELECT 1,NULL)` = NULL、`(3,2) IN (SELECT 1,NULL)` = false、`(1,NULL) IN (空)` = false、`(1,2) IN (SELECT x,y FROM u)` = NULL、`(2,5) IN (...)` = true、`(1,3) IN (...)` = NULL、`(3,3) IN (...)` = false。

---

## 5. ノードごとの仕様

### 5.0 共通の規約

- **行の形**: 各ノードの出力列の並びは 00 §9.2 のとおり。`concat(a, b)` は `a ++ b`、`nulls(n)` は NULL を n 個。
- **check_interrupts の位置**（00 §4.3 の 4）: (a) 走査ノードは `next` のたびに先頭で呼ぶ。(b) 入力を連続して読むループ（構築・ソート・集約・結合の内側ループ・スキップ・Filter の読み飛ばし）は、入力を 1 行読むたびに 1 回呼ぶ。(c) 副問い合わせの行を読むループは 1 行ごと。(d) DML は入力 1 行ごと。
- **メモリ**: 溜める行（ソート・ハッシュ表・Materialize・集約状態・DISTINCT の集合・CTE ストア・Hashed の集合）は `ctx.mem.charge` で課金する（§8）。
- **rewind の型**: ノードが「溜めた結果を持つ」とき、`reuse`（D5-3）が true なら読みの位置だけを先頭に戻し、子を `rewind` しない。false なら溜めた結果を捨て（課金を返し）、子を `rewind` して未構築に戻す。溜めた結果を持たないノードは、子を `rewind` して自分の状態を初期化する。
- **NULL**: 各節に書く。結合キー・DISTINCT・GROUP BY・集合演算の「等しい」は `HashKey` / `cmp_datum` の意味（NULL どうしは等しい）。結合キーだけは NULL が一致しない（§5.15）。
- **フィルタの数え方**: filter を持つノードは、落とした行数を `extra_stats` の `("Rows Removed by Filter", n)` として累計する（`rewind` でも 0 に戻さない）。

### 5.1 Result

- 入力: なし。出力: `exprs` の値 1 行（`exprs` が空なら 0 列の 1 行）。
- `one_time_filter` を最初に評価する（空の行に対して）。true でなければ 0 行。`exprs` も空の行に対して評価する（SubLink・Param を含みうるので `eval`）。
- `rewind`: 未実行に戻す（`one_time_filter` と `exprs` は再評価される。Param が変わりうるため）。

### 5.2 Values

- 出力: `rows[i]` の式を空の行に対して評価した値の並び。1 行ずつ遅延評価する。`rewind`: 添字を 0 に戻す（再評価する）。

### 5.3 SeqScan（filter 付き）

- 出力: ユーザー列 ++ `system_columns`（Ctid → `Datum::Tid`、Xmin / Xmax → `Datum::Xid(外部表現)`、Cmin / Cmax → `Datum::Cid`、TableOid → `Datum::Oid`）。M2 の `row_of` と同じ。
- アルゴリズム:

```text
next:
    loop:
        check_interrupts
        scan が None なら scan = ctx.storage.begin_scan(&rel, ctx.snapshot)
        t = ctx.storage.scan_next(&mut scan)?     None → return None
        row = row_of(t)
        if let Some(f) = filter:
            if eval_pred(f, &row, ctx)? != Some(true): removed += 1; continue     // NULL は落とす
        return Some(row)
```

- `filter` は出力行（ユーザー列 ++ システム列）に対して評価する。
- `rewind`: `scan = None`（次の `next` で新しい `begin_scan`。同じスナップショット・コマンド ID）。
- メモリ: 課金なし。

### 5.4 IndexScan

- 出力: SeqScan と同じ形（ユーザー列 ++ `system_columns`）。
- 状態: `scan: Option<IndexScan>`、`empty: bool`（キーに NULL があり 0 行と決まった）。

```text
resolve_keys(ctx) -> Option<ResolvedScanKeys>:          // None = 0 行
    eq = []
    for k in keys.eq:
        IndexScanKey::IsNull      → eq.push(None)
        IndexScanKey::Eq(e)       → v = eval(e, &[], ctx)?; if v is NULL → return None; eq.push(Some(v))
    lower = keys.lower.map(|b| (eval(b.expr, &[], ctx)?, b.inclusive))      // NULL なら return None
    upper = 同様
    Some(ResolvedScanKeys { eq, lower, upper })

next:
    loop:
        check_interrupts
        if scan is None:
            if empty: return None
            match resolve_keys(ctx)?:  None → { empty = true; return None }
                                       Some(k) → scan = Some(ctx.indexes.begin_scan(&index, &k, direction)?)
        tid = ctx.indexes.scan_next(&mut scan)?       None → return None
        t = ctx.storage.fetch(&rel, ctx.snapshot, tid)?       None（不可視・死んだ版）→ continue
        row = row_of(t)                                       // SeqScan と共有
        if let Some(f) = filter and eval_pred(f, &row, ctx)? != Some(true): removed += 1; continue
        return Some(row)
```

- キー式は空の行に対して評価する（定数・`Param`・InitPlan の値だけを含む。00 §9.2）。`col = NULL`、`col > NULL` は決して真にならないので 0 行（PostgreSQL と同じ）。`IS NULL` は `eq` に `None` を入れて B+Tree に NULL の項目を探させる。
- `filter` は planner が**元の述語すべて**（インデックスのキーに使ったものを含む）を入れる。executor は区別せず、そのまま再評価する（安全側。04 章の方針）。
- ヒープの可視性判定は `fetch` が行う（インデックスは MVCC を知らない）。UPDATE で旧版・新版の両方が索引されていても、可視な版だけが返る。同じ文の UPDATE が書いた新版はコマンド ID で見えない（Halloween 対策は M2 §5.5 と同じ）。
- 順序: `direction` どおりの索引順（キーが同じなら TID の順。Backward は逆）。
- `rewind`: `scan = None`、`empty = false`（次の `next` でキーを再評価して `begin_scan`。NestedLoopParam の inner では毎回 `Param` が変わる）。
- メモリ: 課金なし（B+Tree が葉ごとにまとめる TID は小さい）。`extra_stats`: `("Rows Removed by Filter", n)`。

### 5.5 FunctionScan（`generate_series`）

- 対象: `func.name == "generate_series"` の `(int4, int4)`、`(int4, int4, int4)`、`(int8, int8)`、`(int8, int8, int8)`。出力は 1 列（`int4` または `int8`）。ほかの関数は `build_scoped` の時点で `Error::internal`（最初の `next` で返す。planner が来させない）。
- 最初の `next` で `args` を空の行に対して評価する（`start`、`stop`、`step`。省略時の `step = 1`）。

```text
init:
    いずれかが NULL → 0 行（generate_series は STRICT）
    step == 0 → Err(22023 "step size cannot equal zero")
    cur = start
next:
    check_interrupts
    if (step > 0 && cur > stop) || (step < 0 && cur < stop): return None
    emit cur
    cur = cur + step        // int4 は i64 で計算（範囲を超えても i32 の stop と比較して終わる）。
                            // int8 は checked_add。None（桁あふれ）なら次の next で終了（エラーにしない）
```

- 検証済み: `generate_series(2147483646, 2147483647)` は 2 行、`generate_series(9223372036854775806, 9223372036854775807, 2)` は 1 行、`(1,3,-1)` は 0 行、`(3,1,-1)` は 3 行、`(NULL,3)` と `(1,3,NULL)` は 0 行、`(1,3,0)` は `22023 step size cannot equal zero`（int4・int8 とも）。
- `rewind`: 未初期化に戻す（引数を再評価する）。メモリ課金なし。

### 5.6 Filter

- 出力: 入力と同じ。`predicate` を入力行に対して評価し、true の行だけ返す（NULL・false は落とす）。落とした行数を `("Rows Removed by Filter", n)` に積む。
- `rewind`: 子を `rewind`。

### 5.7 Project

- 出力: `exprs[i]` を入力行に対して評価した値の並び。評価の順は `exprs` の順。`exprs` が空なら 0 列の行。
- `rewind`: 子を `rewind`。

### 5.8 Sort

- 入力を全部読んで `Vec<(Vec<Datum> /* キー */, Row)>` に溜め、整列してから返す。
- 比較: キーごとに `cmp_with_nulls(a, b, key.descending, key.nulls_first)`。**安定ソート**（`sort_by`）。PostgreSQL のソートは安定でないが、同順位の並びに依存するテストは書かない（ORDER BY の完全指定）。キー式は入力行を読んだ時点で 1 回評価する。
- 課金: 行ごとに `estimate_row_bytes(&row) + estimate_row_bytes(&key)`。
- `rewind`: `reuse` なら位置を 0 に戻す（行は複製して返す）。そうでなければ破棄して子を `rewind`。`rewindable = false` のとき、返す行は `std::mem::take` で取り出して複製を避け、最後の行を返した時点で課金を返す（D5-22）。
- NULL: キーの NULL は `nulls_first` に従って先頭か末尾（ASC の既定は NULLS LAST、DESC の既定は NULLS FIRST）。`extra_stats`: `("Sort Space Used (bytes)", 課金額)`。

### 5.9 Unique

- 入力は `key_cols`（入力の列位置）で整列済み。`prev_key: Option<Vec<Datum>>` を持ち、入力行の `key_cols` の値が `prev_key` と**異なる**最初の行だけ返す（等しさは `cmp_datum == Equal`、NULL どうしは等しい）。返した行の `key_cols` の値を `prev_key` にする。DISTINCT ON（各グループの最初の行）の実装。
- `rewind`: `prev_key = None`、子を `rewind`。課金なし。整列の仮定は検査しない（デバッグビルドで前の行より小さければ `debug_assert`）。

### 5.10 Distinct

- 行全体を `HashKey` にして `HashSet<HashKey>` に入れ、初めて見た行だけ返す（入力順を保つ）。NULL どうしは等しい。
- 課金: 新しいキーごとに `estimate_row_bytes(&row) + HASH_ENTRY_OVERHEAD`。
- `rewind`: 集合を空にして課金を返し、子を `rewind`（再利用はしない。Distinct は結果ではなく集合を持つため）。

### 5.11 Limit

- `limit` / `offset` を最初の `next` で空の行に対して評価する（OFFSET を先に評価）。NULL は「無制限」「0」。負は OFFSET が `2201X`（`OFFSET must not be negative`）、LIMIT が `2201W`（`LIMIT must not be negative`）。値は `i64`（planner が int8 にキャスト済み）。
- 読み飛ばし中も 1 行ごとに `check_interrupts`。残り 0 になったら子を呼ばずに `None`（PostgreSQL と同じく余分に読まない）。
- `rewind`: 評価済みの状態を捨て（式を再評価する）、子を `rewind`。

### 5.12 Materialize

- 遅延して溜める。`store: Vec<Row>`、`child_done: bool`、読み位置 `pos`。

```text
next:
    if pos < store.len(): pos += 1; return store[pos-1].clone()
    if child_done: return None
    check_interrupts
    match child.next(ctx)?: None → { child_done = true; return None }
                            Some(r) → charge(estimate_row_bytes(&r)); store.push(r.clone()); pos += 1; return Some(r)
rewind:
    if reuse: pos = 0          // 子は rewind しない。child_done = false なら、続きは子の現在位置から読む
    else: release(charged); store.clear(); child_done = false; pos = 0; child.rewind(ctx)
```

- NLJ の inner が途中で打ち切られ（SEMI の早期終了）、`pos = 0` に戻った後で最後まで読んでも、子の続きから正しく読み足せる。
- `rewindable = false` の Materialize は存在しうるが、末尾で課金を返さない（`store` の再読がありうるため）。

### 5.13 NestedLoopJoin

- 入力: `outer`（left）、`inner`（right）。出力: INNER / LEFT は `outer ++ inner`、SEMI / ANTI は `outer` だけ（D5-8）。FULL は `Error::internal`。
- `join_filter` は `outer ++ inner` の行に対して評価する（SEMI / ANTI でも連結した行に対して）。ON の残り条件であり、WHERE ではない（外部結合の意味が変わるため）。

```text
state: outer_row: Option<Row>, matched: bool, inner_dirty: bool   // inner_dirty = inner.next を 1 回でも呼んだ

next:
    loop:
        check_interrupts
        if outer_row is None:
            outer_row = outer.next(ctx)?;  None → return None（終了。以後 None）
            matched = false
            if inner_dirty: inner.rewind(ctx)?          // 外側の行ごとに内側を先頭から
        irow = inner.next(ctx)?; inner_dirty = true
        match irow:
          Some(i):
            joined = concat(outer_row, i)
            if let Some(f) = join_filter and eval_pred(f, &joined, ctx)? != Some(true): removed_join += 1; continue
            matched = true
            INNER, LEFT → return Some(joined)
            SEMI        → return Some(outer_row.take())                  // 最初の一致で外側の行を出して次へ（inner は途中のまま）
            ANTI        → outer_row = None; continue                      // 一致があれば出さない
          None:
            o = outer_row.take()
            LEFT and !matched → return Some(concat(o, nulls(inner_width)))
            ANTI and !matched → return Some(o)
            otherwise continue
```

- NULL: `join_filter` が NULL なら不一致（落とす）。ANTI は NULL を「一致」に数えない（`NOT EXISTS` の意味）。`NOT IN` は ANTI にならない（planner が変換しない）。
- `rewind`: `outer.rewind`、`outer_row = None`。`inner_dirty` は維持する（inner が途中の可能性があるので、次の外側の行で必ず `rewind`）。
- `extra_stats`: `("Rows Removed by Join Filter", n)`。課金なし（inner の Materialize が課金する）。

### 5.14 NestedLoopParam

§5.13 と同じ構造に、外側の行ごとの `params` の設定を足す。実装は 1 つの構造体（`params` が空なら NestedLoopJoin）。

```text
外側の行を得た直後:
    vals = [eval(e, &outer_row, ctx)? for (_, e) in params]
    for ((p, _), v) in zip(params, vals): ctx.set_param(p, v)?
    if inner_dirty: inner.rewind(ctx)?
```

- inner は `Param` に依存する（IndexScan のキー）ので、inner の子は `reuse = false` になり、`rewind` のたびに再構築される。
- パラメータが NULL なら inner の IndexScan は 0 行（§5.4）。LEFT なら NULL 拡張、ANTI なら出力。

### 5.15 HashJoin

入力: `left`、`right`、`left_keys`（left の行に対して評価）、`right_keys`（right の行に対して評価）、`residual`（`left ++ right` に対して評価）、`build_is_left`。出力: INNER / LEFT / FULL は `left ++ right`、SEMI / ANTI は `left`。`key_types` は実行時には使わない（デバッグビルドで評価した値の変種を検査してよい）。キーが空のときは全行が 1 つのバケットに入る（直積になる。planner は使わない）。

#### 5.15.1 役割と印

`B` = ビルド側（`build_is_left` なら left）、`P` = プローブ側。

| kind | build_is_left | probe 保存（PP） | build 保存（BP） | 出力 |
|---|---|---|---|---|
| Inner | どちらでも | なし | なし | 一致ごとに `left ++ right` |
| Left | false（P = left） | あり | なし | 一致ごと + P の不一致行を `P ++ nulls(right_width)` |
| Left | true（B = left） | なし | あり | 一致ごと + 終了後に B の不一致行を `B ++ nulls(right_width)` |
| Full | どちらでも | あり | あり | 一致ごと + P の不一致行（B 側を NULL）+ 終了後に B の不一致行（P 側を NULL） |
| Semi | false | 最初の一致で P を出す | なし | P の行（一致の有無 1 回だけ） |
| Semi | true | なし | あり | 終了後に B の一致した行を 1 回ずつ |
| Anti | false | 一致がなければ P を出す | なし | P の行 |
| Anti | true | なし | あり | 終了後に B の一致しなかった行 |

「一致」= キーが等しく（NULL を含むキーは決して一致しない）、`residual` が true（None なら常に true）。外部結合の不一致の判定は residual を含む。

#### 5.15.2 データ構造

```rust
struct BuildRow { row: Row, matched: bool }
rows: Vec<BuildRow>                       // BP の kind のときだけ、キーが NULL の行も保持する（終了後に不一致として出すため）
table: HashMap<HashKey, u32 /* bucket id */>
buckets: Vec<Vec<u32 /* rows の添字 */>>  // 挿入順。出力順を決定的にする
```

#### 5.15.3 アルゴリズム

```text
build（最初の next）:
    for r in B.next() 全部:
        check_interrupts
        key = [eval(e, &r, ctx) for e in B 側のキー式]
        if key に NULL: if BP の kind: rows.push(BuildRow{r, false}) ; charge; continue      // 一致できないが、不一致として出す
        idx = rows.len(); rows.push(..); table[HashKey(key)] → bucket に idx を追加
        charge(estimate_row_bytes(&r) + estimate_row_bytes(&key) + HASH_ENTRY_OVERHEAD)
    // 最適化（任意）: 構築側が空で kind が Inner、または Semi / Anti 以外の非 BP のとき、P を読まずに終了

probe（next）: 状態 = (cur: Option<Row>, bucket: Option<u32>, pos, cur_matched)
    loop:
        check_interrupts
        if cur is None:
            p = P.next()?;  None → 終了段へ
            key = [eval(e, &p, ctx) for e in P 側のキー式]
            bucket = if key に NULL { None } else { table.get(HashKey(key)) }
            cur = Some(p); pos = 0; cur_matched = false
        while let Some(b) = bucket and pos < buckets[b].len():
            i = buckets[b][pos]; pos += 1; check_interrupts
            joined = if build_is_left { concat(rows[i].row, p) } else { concat(p, rows[i].row) }
            if residual is Some and eval_pred(residual, &joined, ctx)? != Some(true): continue
            cur_matched = true; rows[i].matched = true
            match kind と build_is_left:
                Inner, Left, Full                → return Some(joined)
                Semi / Anti（build_is_left = false）→ break      // 一致が見つかったので残りは見ない
                Semi / Anti（build_is_left = true） → continue   // 印だけ付けて続ける（B の別の行にも当たりうる）
        // この P 行の bucket を見終えた
        p = cur.take()
        PP の kind で !cur_matched → return Some(P 側 ++ nulls)   // Left / Full / Anti（build_is_left=false のとき P ++ nothing）
        Semi（build_is_left=false）and cur_matched → return Some(p)
        continue

終了段（P が尽きた後）: i を 0.. で
    Left / Full で BP: rows[i].matched == false の行を NULL 拡張して返す（位置は B の左右に従う）
    Semi（build_is_left = true）: matched の行を返す
    Anti（build_is_left = true）: !matched の行を返す（キーが NULL の行を含む）
    その後 None
```

- 一致の印は Inner の `build_is_left` でも付けてよい（不要だが無害）。
- 出力順: probe の順、各 probe 行の中は bucket の順（= build の挿入順）。終了段は build の挿入順。
- NULL キー: ビルド側で NULL を含む行は table に入れない。プローブ側で NULL を含む行は bucket を引かない（不一致扱い。PP の kind なら不一致行として出る）。
- `rewind`: `reuse(B)` なら table と rows を保持し、`matched` をすべて false に戻し、P を `rewind`、probe の状態を初期化。`reuse(B)` が false なら構築し直す（課金を返し、B と P の両方を `rewind`）。
- メモリ: ビルド行と表の課金は上のとおり。`rewindable = false` なら終了段が終わった時点で課金を返す。`extra_stats`: `("Rows Removed by Join Filter", n)`（residual で落とした一致候補）。
- check_interrupts: 構築 1 行ごと、probe 1 行ごと、bucket の候補 1 つごと。

### 5.16 Aggregate（GROUP BY なし）

- 出力: 1 行 = `AggSet::finish` の値の並び。入力が空でも 1 行（`count` は 0、ほかは NULL。検証済み）。
- アルゴリズム: 全入力を `AggSet::accumulate`（行ごとに `check_interrupts`）してから 1 行を返す。2 回目の `next` は `None`。`rewind`: `reuse` なら結果を再び 1 回返す。そうでなければ状態を作り直して子を `rewind`。
- 課金: DISTINCT の集合の新しい値だけ（§8）。

### 5.17 HashAggregate

- 出力: `keys ++ aggs の結果`、グループごとに 1 行。入力が空なら 0 行（`keys` が空のときは planner が Aggregate を使う。空なら `Error::internal`）。
- データ構造: `groups: Vec<(Vec<Datum> /* key */, AggGroup)>`、`index: HashMap<HashKey, usize>`。出力順は groups の初出順（D5-11）。

```text
build（最初の next）:
    for row in child 全部:
        check_interrupts
        key = [eval(e, &row, ctx) for e in keys]            // NULL どうしは同じグループ
        g = index.get_or_insert(HashKey(key))               // 新規なら charge(estimate_row_bytes(&key) + aggs.base_bytes() + HASH_ENTRY_OVERHEAD)
        aggs.accumulate(&mut g, &row, ctx)
emit: groups[pos] から key ++ aggs.finish(g)
```

- `rewind`: `reuse` なら `pos = 0`。そうでなければ破棄（課金を返す）して子を `rewind`。`rewindable = false` なら最後のグループを返した時点で課金を返す。

### 5.18 GroupAggregate

- 入力は `keys` で整列済み（planner が Sort を置く）。出力は HashAggregate と同じ形・同じ内容（順序は入力の整列順）。ストリーミングで、課金は現在のグループの DISTINCT の集合だけ。

```text
next:
    if done: None
    loop:
        check_interrupts
        row = child.next()?
        None → if cur is Some: done = true; return Some(emit(cur)) else done = true; return None
        Some(row): k = keys 評価
            if cur is None: cur = new_group(k)
            elif k == cur.key（cmp_datum で等しい。NULL どうしは等しい）: （そのまま）
            else: out = emit(cur); cur = new_group(k); accumulate(row); return Some(out)
            accumulate(cur, row)
```

- 新しいグループへ移るときに DISTINCT の集合の課金を返す。入力が空なら 0 行。`rewind`: 状態を初期化し子を `rewind`。

### 5.19 Append

- `inputs` を順に最後まで読む。出力の列は全入力で同じ（型変換は planner が Project で済ませる）。`rewind`: `idx = 0`、各入力を `rewind`（未開始のものは何もしない）。課金なし。

### 5.20 HashSetOp

- 左を全部読み、次に右を全部読む。`entries: Vec<Entry { row: Row, left: u64, right: u64 }>`、`index: HashMap<HashKey, usize>`（キーは行全体。NULL どうしは等しい）。

```text
左の行: entry がなければ作る（charge(2 * estimate_row_bytes + HASH_ENTRY_OVERHEAD)）; left += 1
右の行: entry があれば right += 1。なければ、op が Union のときだけ作る（right = 1、left = 0）。それ以外は無視
出力（entries の初出順）。各 entry について出す個数:
    Intersect            : left > 0 && right > 0 → 1。 all → min(left, right)
    Except               : left > 0 && right == 0 → 1。 all → max(left - right, 0)
    Union（05-Q7）       : !all → 1（left + right > 0）。 all → left + right
```

- 検証済み: `{1,1,1,2,2,NULL,NULL} INTERSECT ALL {1,1,2,2,2,NULL}` は NULL×1、1×2、2×2。`EXCEPT ALL` は NULL×1、1×1。`EXCEPT` は空、`INTERSECT` は NULL、1、2。
- `rewind`: 両方の子が `reuse` なら出力位置を 0 に戻す。そうでなければ構築し直す。`rewindable = false` なら出力し終えた時点で課金を返す。

### 5.21 CteScan

- `ctx.ctes.slots[cte]` が共有ストア。各 `CteScan` は自分の読み位置 `pos` を持つ。

```text
next:
    slot = ctx.ctes.slots[cte]
    if pos < slot.rows.len(): pos += 1; return slot.rows[pos-1].clone()
    if slot.done: return None
    check_interrupts
    exec = take（Unbuilt なら build_scoped(ctx.query.ctes[cte], PlanScope::Cte(cte), rewindable = true)）; r = exec.next(ctx); put_back
    None → slot.done = true; None
    Some(r) → charge(estimate_row_bytes(&r)); slot.rows.push(r.clone()); pos += 1; Some(r)
rewind: pos = 0（共有ストアは触らない。他の CteScan の進み方に影響しない）
```

- 最初の参照で CTE を実行し、各参照が自分のカーソルで読む。2 つの `CteScan` が交互に `next` を呼んでも、先に進んだ方が溜め、遅れた方はストアから読む。`slot.free == false`（`ctes[cte]` が `Param` に依存）なら `Error::internal`（D5-20）。
- 課金は 1 回だけ（ストアに入れるとき）。解放しない。

---

## 6. 集約（`AggState`）

### 6.1 `AggSet::accumulate` の手順

集約 `a`（`PhysAgg`）ごとに、入力 1 行について次を行う。

1. `a.filter` があり `eval_pred != Some(true)` なら、この集約はこの行を飛ばす。
2. 引数を評価する（`count(*)` は引数なし）。
3. 引数のどれかが NULL なら飛ばす（M4 の集約はすべて引数について STRICT。`count(*)` は行を数える）。
4. `a.distinct` なら、グループ × 集約ごとの `HashSet<HashKey>` を引く。すでにあれば飛ばす。なければ入れて `ctx.mem.charge(estimate_row_bytes(args) + HASH_ENTRY_OVERHEAD)`。
5. `AggState::transition(args)`。

### 6.2 状態・遷移・結果

| `AggKind` | 状態 | 初期値 | 遷移（非 NULL の `v`） | 結果型・結果 | 非 NULL の入力が 0 件 |
|---|---|---|---|---|---|
| CountStar | `i64` | 0 | `+1`（FILTER を通った全行） | int8 | 0 |
| Count | `i64` | 0 | `+1` | int8 | 0 |
| SumInt2 | `Option<i64>` | None | `s + v as i64`（`checked_add`） | int8 | NULL |
| SumInt4 | `Option<i64>` | None | `s + v as i64`（`checked_add`） | int8 | NULL |
| SumInt8 | `Option<i128>` | None | `s + v as i128` | numeric（`i128` を小数点なしの numeric に。`Numeric::parse(&s.to_string())` でよい） | NULL |
| SumFloat4 | `Option<f32>` | None | `f32` の加算 | float4 | NULL |
| SumFloat8 | `Option<f64>` | None | `f64` の加算 | float8 | NULL |
| SumNumeric | `Option<Numeric>` | None | `checked_add` | numeric（入力の dscale の最大） | NULL |
| AvgInt（Int2 / Int4 / Int8） | `{count, sum: i128}` | 0, 0 | `count + 1`、`sum + v` | numeric: `numeric(sum) / numeric(count)`（§6.4） | NULL |
| AvgFloat（Float4 / Float8） | `{count, sum: f64}` | 0, 0.0 | `count + 1`、`sum + v as f64`（D5-12） | float8: `sum / count` | NULL |
| AvgNumeric | `{count, sum: Numeric}` | 0, 0 | `count + 1`、`checked_add` | numeric: `sum / count`（§6.4） | NULL |
| Min / Max | `Option<Datum>` | None | None なら `v`。そうでなければ `cmp_datum(v, cur)` が Less（Min）/ Greater（Max）のとき置き換える | 引数と同じ型 | NULL |
| BoolAnd | `Option<bool>` | None | `s && v`（None なら `v`） | bool | NULL |
| BoolOr | `Option<bool>` | None | `s \|\| v`（None なら `v`） | bool | NULL |

`every` は BoolAnd の別名の行。Min / Max は `cmp_datum` を使うので、text は C 照合のバイト順、NaN は最大、bpchar は末尾の空白を無視して比べる（返す値は元の値。検証済み: `min(char(3))` が `a`）。

### 6.3 オーバーフローと浮動小数

| 場合 | エラー |
|---|---|
| SumInt2 / SumInt4 の `i64` の加算が桁あふれ | `22003 bigint out of range` |
| SumFloat4 / SumFloat8 / AvgFloat の加算の結果が無限大で、2 つの入力はどちらも有限 | `22003 value out of range: overflow`（検証済み。`sum` と `avg` のどちらも） |
| numeric の加算・除算の範囲外 | `yuzhu_numeric::NumericError` を `Error`（22003 など）に変換（`types::numeric` の橋渡し関数） |
| SumInt8 / AvgInt の `i128` | 起きない（2^64 行以上が必要） |

### 6.4 `avg` の numeric 除算の scale 規則

`AvgInt` と `AvgNumeric` の結果は `Numeric::checked_div(sum, count)`（`numeric_div` と同じ。`select_div_scale` の結果の桁数で四捨五入）。PostgreSQL の規則（`numeric.c` の `select_div_scale`）:

```text
weight1, firstdigit1 = 被除数の先頭の base-10000 の桁の重みと値（0 なら 0, 0）
weight2, firstdigit2 = 除数の同様
qweight = weight1 - weight2
if firstdigit1 <= firstdigit2: qweight -= 1
rscale  = 16 - qweight * 4              // NUMERIC_MIN_SIG_DIGITS = 16、DEC_DIGITS = 4
rscale  = max(rscale, dscale1, dscale2, 0)    // NUMERIC_MIN_DISPLAY_SCALE = 0
rscale  = min(rscale, 1000)                   // NUMERIC_MAX_DISPLAY_SCALE
```

実機の照合値（検証済み。すべて `avg` の結果）:

| 入力 | 結果 | rscale |
|---|---|---|
| int4 {1, 2} | `1.5000000000000000` | 16 |
| int4 {1, 2, 4} | `2.3333333333333333` | 16 |
| int4 {1, 1, 1} | `1.00000000000000000000` | 20（3 <= 3 で qweight が下がる） |
| int4 {0, 1, 1} | `0.66666666666666666667` | 20（四捨五入） |
| int4 {1, 1, 2, 1000000} | `250001.000000000000` | 12（被除数の先頭桁 100 > 4、weight 差 1） |
| int8 {1, 2} | `1.5000000000000000` | 16 |
| float4 {1, 2} | `1.5`（float8） | — |
| int4 {2}（FILTER で 1 行） | `2.0000000000000000` | 16 |

`sum(float4)` の結果型は `real`、`sum(int8)` は `numeric`（`9223372036854775807 + 1` は `9223372036854775808`）。いずれも検証済み。

---

## 7. DML

### 7.1 `insert_with_indexes`

```text
insert_with_indexes(ctx, rel, w, row):
    1. tid = ctx.storage.insert(rel, w, row)?                      // 失敗（54000 の行サイズなど）はそのまま返す
    2. for index in rel.indexes.iter():                            // OID 昇順（TableDef.indexes と同じ）
           key  = index.columns.iter().map(|c| row[c.attnum - 1].clone())    // 式インデックスはない
           chk  = if index.unique { UniqueCheck::Check { heap: ctx.storage, rel, own_xid: w.xid } } else { UniqueCheck::Skip }
           ctx.indexes.insert(w, index, &key, tid, chk)
               .map_err(|e| complete_unique_error(e, index, &key, ctx.type_env))?
    3. Ok(tid)
```

- 一意性検査は B+Tree の中で行う（NULL を含むキーは検査しない）。他トランザクションの待ち（`WaitFor`）は M4 では `Error::internal`（06 章が返す）。
- **エラー時の扱い**: ヒープに入れた版と、それまでに成功したインデックスの項目は取り消さない。呼び出し元（ノード）が `Err` を返し、session がトランザクションを中断する。中断したトランザクションの版は `xmin` が中断扱いなので、可視性判定と一意性検査（`fetch_dirty` が `Invisible` を返す）の両方で無視される。したがって、半端な状態はどのトランザクションからも見えない。
- `complete_unique_error`: `e.sqlstate == UNIQUE_VIOLATION` で `e.detail` が空なら `with_detail(unique_detail)`、`table` / `constraint` が空なら `with_table(index.schema, index.table_name)` と `with_constraint(index.name)` を足す（D5-16）。

### 7.2 `update_with_indexes`

```text
update_with_indexes(ctx, rel, w, tid, new_row):
    out = ctx.storage.update(rel, w, ctx.snapshot, tid, new_row)?
    if out.result != TmResult::Ok: return Ok(out)                   // SelfModified など。インデックスには触らない
    new_tid = out.new_tid.ok_or_else(|| Error::internal("update returned Ok without a new TID"))?
    for index in rel.indexes: 上と同じ（key は new_row から。unique は検査つき）
    Ok(out)
```

- キー列が変わらなくても全インデックスに新しい TID の項目を入れる（HOT なし。00 §10）。一意インデックスで値が変わらないとき、検査は旧版（自分が削除済み: `fetch_dirty` が `Invisible`）に当たるだけで衝突しない（検証済み: 主キー以外の列の UPDATE は成功する）。
- 旧版の項目は消さない（VACUUM は M5）。インデックスのスキャンはヒープの可視性で旧版を捨てる。
- 失敗時の扱いは 7.1 と同じ。

### 7.3 一意違反（23505）のメッセージとフィールド

実機の出力（検証済み、`CREATE TABLE e5(a int PRIMARY KEY, ..., UNIQUE(u1, u2))`）:

| 項目 | 値 |
|---|---|
| SQLSTATE | `23505` |
| message | `duplicate key value violates unique constraint "e5_pkey"`（制約名 = インデックス名 `IndexHandle.name`） |
| DETAIL | `Key (a)=(1) already exists.`、複数列は `Key (u1, u2)=(1, p) already exists.` |
| `s`（schema） | `public`（`IndexHandle.schema`） |
| `t`（table） | `e5`（`IndexHandle.table_name`） |
| `n`（constraint） | `e5_pkey`（`IndexHandle.name`） |

- DETAIL の形式: `Key (` + 列名を `, ` で連結 + `)=(` + 値の出力テキストを `, ` で連結 + `) already exists.`。値は `io::output_text(d, ty, env)`（NULL は検査しないので現れない）。値は**切り詰めない**（実機: 100 文字の text がそのまま出る）。引用符もつけない。列名は `IndexKeyColumn.name`、型は `IndexKeyColumn.ty`。
- 複数の一意インデックスに違反するときは OID の小さい方（D5-15。検証済み）。
- 同じ文の中で先に入れた行とも衝突する（コマンド ID を見ない。検証済み: `INSERT INTO e7 VALUES (1),(1)`）。`UPDATE e7 SET a = a + 1`（行 1, 2）は物理順で `Key (a)=(2)` に当たる（検証済み）。
- ErrorResponse: `Error::with_table(schema, table).with_constraint(name)`。`Error.column` は付けない。

### 7.4 NOT NULL と CHECK（`RowChecker::check`）

行（挿入する行 / 更新後の新しい行）に対して、NOT NULL（attnum の順）→ CHECK（名前の昇順）を検査する。ヒープに書く前に行う。

| 違反 | SQLSTATE | message | DETAIL | フィールド |
|---|---|---|---|---|
| NOT NULL | `23502` | `null value in column "b" of relation "e5" violates not-null constraint` | `Failing row contains (2, null, 1, 1.50, 2, q).` | `s`、`t`、`c`（列名）。`n` なし |
| CHECK | `23514` | `new row for relation "e5" violates check constraint "e5_c_check"` | 同上 | `s`、`t`、`n`（制約名）。`c` なし |

（検証済み。`VERBOSITY verbose` で `SCHEMA NAME` / `TABLE NAME` / `COLUMN NAME` / `CONSTRAINT NAME` を確認）

- CHECK の結果が NULL なら通す。`eval_const`（`&ExecCtx`）で評価する。
- `Failing row contains (...)`: M2 の `failing_row` と同じ（NULL は `null`、各値は 64 バイトで切って `...`。numeric は dscale のまま `1.50`）。UPDATE では**新しい行**を出す。`io::output_text` に `TypeEnv` を渡す。
- schema と列の型はエラー時にだけ `ctx.catalog.table_by_oid(rel_oid)` で引く（M2 と同じ。正常系で `TableDef` を引かない）。

### 7.5 Insert ノード

- 入力: 子の行（analyzer が型をそろえた式で出す）。`column_map[i] = Some(j)` は表の i 列目 = 入力の j 列目、`None` は `defaults[i]`（なければ NULL）。

```text
next（1 回目）: loop:
    row_in = input.next()?  None → break
    check_interrupts
    row = builder.build(ctx, &row_in)?                // RowBuilder。DEFAULT は eval_const で評価（nextval は ctx.runtime 経由）
    checker.check(ctx, &row)?                          // NOT NULL → CHECK
    w = ctx.write_ctx()?
    insert_with_indexes(ctx, &rel, &w, &row)?
    count += 1
    if returning: return Some([eval_const(e, &row) for e in returning])     // D5-17、D5-18。ここで next を抜ける
done = true; None
```

- DEFAULT の評価は行ごと（`nextval` の値は行ごとに進む）。評価順は表の列順。
- `INSERT ... SELECT` が同じ表を読んでも、新しい行は読まれない（コマンド ID。M2 §5.5）。
- RETURNING なしのときは最初の `next` で全入力を処理し、`None` を返す。`rows_affected()` が件数。`rewind` は `Error::internal`（DML ノードは rewind されない）。

### 7.6 Update ノード

- 入力の形（00 §9.2）: 対象表のユーザー列（`n_user_cols` 個）++ ctid ++ 新しい値（`assigned.len()` 個）。

```text
per input row r:
    check_interrupts
    old = r[..n]; tid = r[n] as Datum::Tid（そうでなければ Error::internal）
    new = old.clone(); for (col, pos) in assigned: new[col] = r[pos].clone()
    checker.check(ctx, &new)?                                   // ヒープに触る前
    w = ctx.write_ctx()?
    out = update_with_indexes(ctx, &rel, &w, tid, &new)?
    match out.result:
        Ok                       → count += 1; if returning: emit [eval_const(e, &new)]
        SelfModified { cmax }    → if cmax == w.cid: 飛ばす（数えない・RETURNING も出さない）
                                    else Err(27000 "tuple to be updated was already modified by an operation triggered by the current command")
        その他（Invisible / Updated / Deleted / BeingModified / WouldBlock）→ Error::internal（M4 では単一ライターなので起きない）
```

- 代入式は planner の Project が評価済み（`SET a = b, b = a` は入れ替えになる。DEFAULT も Project が評価する）。
- `UPDATE ... FROM` で同じ対象行が複数回当たるとき（結合が複数の行を返す）、2 回目以降の `storage.update` は `SelfModified { cmax == w.cid }` を返すので**黙って飛ばす**。最初に当たった行の値が採用される（検証済み: `s1(id 1, 2)` と `s2(k, w)` = {(1,10),(1,20),(2,30)} で `v` は 10 と 30、`UPDATE` は 2 行）。2 回目の CHECK / NOT NULL の検査は PostgreSQL と同じくヒープの前に走る。
- `27000` の HINT は M2 の `already_modified` と同じ。

### 7.7 Delete ノード

- 入力の形: 対象表のユーザー列 ++ ctid。`storage.delete(rel, w, snapshot, tid)`:

| `TmResult` | 扱い |
|---|---|
| Ok | `count += 1`。RETURNING があれば古い行（`r[..n]`）に対して評価して返す |
| SelfModified { cmax } | `cmax == w.cid` なら飛ばす。違えば `27000 tuple to be deleted was already modified by an operation triggered by the current command` |
| その他 | `Error::internal` |

- `DELETE ... USING` で同じ行が複数回当たると 2 回目は飛ばす（検証済み）。インデックスには触らない（旧版の項目は残る）。

### 7.8 RETURNING とコマンドタグ

- RETURNING の式は対象表のユーザー列だけを参照する（D5-17）。評価した行を呼び出し元へ返す。出力の列の型と名前は `PhysicalQuery.output`。
- `rows_affected()`: DML ノードが数えた件数（`SelfModified` で飛ばした行を含まない）。session がコマンドタグにする: `INSERT 0 n`、`UPDATE n`、`DELETE n`。RETURNING があっても同じ。
- 文が `Err` なら件数は意味を持たない（トランザクションが中断する）。

---

## 8. メモリ予算（`executor/mem.rs`）

### 8.1 型

```rust
#[derive(Debug)]
pub struct MemBudget { limit: usize, used: Cell<usize>, peak: Cell<usize> }
impl MemBudget {
    pub fn new(limit: usize) -> Self;                  // 無制限は usize::MAX
    /// used + bytes > limit なら課金せずに 53200 を返す
    pub fn charge(&self, bytes: usize) -> Result<()>;
    pub fn release(&self, bytes: usize);               // saturating_sub
    pub fn used(&self) -> usize;
    pub fn peak(&self) -> usize;
}
pub const ROW_OVERHEAD: usize = 32;          // Vec のヘッダとアロケータの余白
pub const HASH_ENTRY_OVERHEAD: usize = 48;   // ハッシュ表のスロット・バケットの添字・印
pub fn estimate_datum_bytes(d: &Datum) -> usize;
pub fn estimate_row_bytes(row: &Row) -> usize;
```

### 8.2 見積りの定義

```rust
pub fn estimate_datum_bytes(d: &Datum) -> usize {
    std::mem::size_of::<Datum>() + match d {
        Datum::Text(s) | Datum::BpChar(s) => s.len(),
        Datum::Numeric(n) => std::mem::size_of::<Numeric>() + 2 * numeric_ndigits(n),
        Datum::Int2Vector(v) => 2 * v.len(),
        Datum::OidVector(v) => 4 * v.len(),
        Datum::Int4Array(v) => 8 * v.len(),
        Datum::Null | Datum::Bool(_) | /* ほかのヒープを持たない変種を列挙 */ _ => 0,
    }
}
pub fn estimate_row_bytes(row: &Row) -> usize { ROW_OVERHEAD + row.iter().map(estimate_datum_bytes).sum::<usize>() }
```

- `match` の最後の `_` は使わず、`Datum` の変種を全部列挙する（変種の追加でコンパイルエラーにして見積りの漏れを防ぐ）。上の `_` は記述の省略であって、実装では列挙する。
- `len()`（容量ではなく長さ）を使うので、同じ値は同じ見積りになる（テストが決定的）。

### 8.3 課金と解放

| ノード | 課金するもの | 解放 |
|---|---|---|
| Sort | 行ごとに `estimate_row_bytes(row) + estimate_row_bytes(key)` | 再構築の前。`rewindable = false` なら最後の行を返した時点 |
| Materialize | 溜めた行 | 再構築の前 |
| Distinct | 新しいキーごとに `estimate_row_bytes(row) + HASH_ENTRY_OVERHEAD` | `rewind` |
| HashJoin | ビルド行ごとに `estimate_row_bytes(row) + estimate_row_bytes(key) + HASH_ENTRY_OVERHEAD`（NULL キーの行は `estimate_row_bytes(row)` だけ） | 再構築の前。`rewindable = false` なら終了段の後 |
| HashAggregate | 新しいグループごとに `estimate_row_bytes(key) + AggSet::base_bytes() + HASH_ENTRY_OVERHEAD`。Min / Max は値を置き換えて大きくなったときの増分 | 再構築の前。`rewindable = false` なら最後のグループを返した後 |
| Aggregate / GroupAggregate | DISTINCT の集合の新しい値（`estimate_row_bytes(args) + HASH_ENTRY_OVERHEAD`）。Min / Max の増分 | GroupAggregate はグループの切り替え時。Aggregate は `rewind` |
| HashSetOp | 新しい entry ごとに `2 * estimate_row_bytes(row) + HASH_ENTRY_OVERHEAD` | 再構築の前。`rewindable = false` なら出力し終えた後 |
| CteScan | ストアに入れる行ごと | 解放しない（文の終わりまで） |
| InitOnce の Any / All | 溜めた行 | 解放しない |
| Hashed SubPlan | 構築した行ごとに `estimate_row_bytes(key) + HASH_ENTRY_OVERHEAD` | 解放しない |
| IndexScan・SeqScan・Filter・Project・Limit・Unique・Append・NLJ・FunctionScan・DML | なし | — |

- 課金は呼び出した時点で行う。失敗（`Err`）したらその行は表に入れない。
- 課金額は `usize` の加算で `saturating_add`。
- CREATE INDEX のソートと COPY は予算の対象外（D-19）。

### 8.4 53200

```rust
Error::new(sqlstate::OUT_OF_MEMORY, "out of memory")
    .with_detail(format!("Failed on request of {bytes} bytes: the query already holds {used} bytes and yuzhu.query_mem_limit is {limit} bytes."))
    .with_hint("Increase yuzhu.query_mem_limit or reduce the amount of data the query holds in memory.")
```

PostgreSQL の `out of memory` は `DETAIL: Failed on request of size N in memory context "..."` なので、形に合わせた。文言は PostgreSQL と一致しなくてよい（テストは SQLSTATE と先頭の message だけ見る）。

---

## 9. EXPLAIN ANALYZE の計測の差し込み口

`instrument.rs` の本体（`InstrumentSink`、`NodeCounters`、整形）は `10-explain-copy-compat.md`（E1）が書く。この章は「build が計測ラッパーを入れる」規約を定める。

1. `BuildOptions.instrument` が `Some` のとき、`build_scoped` は各ノードを作った直後に `instrument::wrap(inner, key)` で包む。`None` のときは何も包まない（通常の実行にオーバーヘッドを足さない）。
2. `key = NodeKey { scope: PlanScope, index: u32 }`。`index` は `PhysicalPlan` の**先行順**（親、子の順）の通し番号で、スコープ（`Root`、`SubPlan(id)`、`Cte(i)`）ごとに 0 から数える。`ExplainNode`（`PhysicalQuery.explain`、`SubPlanDef.explain`）は同形・同じ子の順序なので、同じ番号で突き合わせる。
3. ラッパーの `next` は、時間の計測（`TIMING` が有効のとき）、返した行数の加算、`None` を返した時点の記録を行い、`rewind` は `loops += 1`。`rows_affected` と `extra_stats` は内側に委ねる。
4. サブプランと CTE の Executor は遅延 `build` されるので、`SubPlanStates::new` / `CteStates::new` が `BuildOptions` を保持し、`build_scoped` に渡す。一度も実行されなかったノード（`loops = 0`）は `(never executed)` と出す（10 章）。
5. `ExplainNode` に実行ノードのない合成ノード（PostgreSQL の `Hash` など）を入れる場合は、`ExplainNode.phys_id: Option<u32>`（合成ノードは None）で対応づける（00 への変更提案 05-P4）。
6. EXPLAIN ANALYZE の DML は実際に実行する（PostgreSQL と同じ。トランザクションの扱いは session）。`rows_affected` と RETURNING は通常どおり。

追加の計測値は `extra_stats()` の名前で渡す。M4 で出すのは `Rows Removed by Filter`（SeqScan / IndexScan / Filter）、`Rows Removed by Join Filter`（NLJ / NLP / HashJoin の residual）、`Sort Space Used (bytes)`（Sort）。`MemBudget::peak()` は EXPLAIN ANALYZE の末尾の情報に使ってよい。

---

## 10. テスト

### 10.1 ノード単体（Rust。`FakeStore` と `ValuesExec` を入力にする）

M1/M2 の方式（`executor/nodes/test_util.rs` の `Fixture`）を踏襲する。`Fixture` を `ExecCtx` の新しいフィールド（`indexes`、`query`、`params`、`mem`、`subplans`、`ctes`、`type_env`）に対応させる（P0）。テスト用の補助:

- `FakeStore::fetch` を実装する（M2 では常に `Ok(None)`）。`fetch_dirty` / `tuple_state` / `begin_scan_all` / `nblocks` も実装する（X3）。
- `FakeIndexStore`（X3）: `Vec<(Vec<Datum>, Tid)>` を `cmp_with_nulls` で整列して持つ `IndexStore`。`insert` は一意検査（`FakeStore::fetch_dirty` で生きている行を判定）つきで、23505 を本番と同じ形（`s` / `t` / `n` 付き、DETAIL なし）で返す。`begin_scan` は `ResolvedScanKeys` で絞る。
- `ValuesExec` の代わりになる `RowsExec`（行の `Vec` を返すだけの `Executor`。`rewind` つき）と `CountingExec`（`next` / `rewind` の呼び出し回数を数える。再利用の検証に使う）、`InterruptAfter { n }`（n 行目で割り込みフラグを立てる）。

| 対象 | 観点 |
|---|---|
| Result / Values | `one_time_filter` が偽で 0 行。`rewind` で式を再評価（`Param` が変わった値になる） |
| SeqScan | filter、システム列との並び、`rewind`、removed の数 |
| IndexScan | 等値・範囲・IS NULL・Backward。NULL を含む等値キーで 0 行。`rewind` でキーの再評価。ヒープで不可視な TID（削除済み）を捨てる。filter の再評価。システム列 |
| FunctionScan | §5.5 の境界のすべて（桁あふれ、step 0、NULL、向きが逆） |
| Filter / Project | NULL の述語は落ちる。式の評価順 |
| Sort | 複数キー、ASC / DESC、NULLS FIRST / LAST の 4 通り、安定性、`rewind` の再利用（`CountingExec` で子の `next` が増えない）と再構築（`Param` 依存で子が `rewind` される） |
| Unique / Distinct | NULL どうしは等しい、bpchar の末尾空白、numeric の `1.10 = 1.1`、float の `-0` と NaN |
| Limit | NULL、負の値（2201X / 2201W）、OFFSET が行数を超える、LIMIT 0 で子を読まない、`rewind` で再評価 |
| Materialize | 遅延、途中で打ち切って `rewind` して続きを読み足す、`reuse` と再構築 |
| NLJ / NLP | 全 kind、`join_filter` の NULL、空の inner、空の outer、SEMI の早期終了後の `rewind`、NLP の params が inner の IndexScan に届く |
| HashJoin | §5.15.1 の表の全行 × residual の有無 × NULL キー × 重複キー × 空の側 |
| Aggregate | 空入力（count = 0、他は NULL）、FILTER、DISTINCT、NULL の除外 |
| HashAggregate / GroupAggregate | NULL のキーが 1 グループ、初出順、空入力は 0 行、GroupAggregate と HashAggregate が同じ結果 |
| Append / HashSetOp | §5.20 の数え方（INTERSECT / EXCEPT × ALL。検証済みの例を使う）、NULL の扱い |
| CteScan | 2 つの参照が交互に読む、1 回しか実行しない、`rewind` |
| SubPlan | §10.3 |
| Insert / Update / Delete | §10.4 |

### 10.2 結合アルゴリズムの相互比較

`tests` に決定的な擬似乱数（自前の xorshift。外部クレートなし）でデータを作り、**同じ入力に対する HashJoin（全 kind × `build_is_left` × residual の有無）と NestedLoopJoin と、テストコード内の素朴な 3 重ループの参照実装**の結果が（行の多重集合として）一致することを確かめる。データの条件: キーに NULL、キーの重複（0〜5 個）、片側が空、両側が空、residual が NULL を返す行、行数 0〜40。シードを 200 個。FULL は参照実装と HashJoin だけ（NLJ は FULL 不可）。SEMI / ANTI は NLJ と HashJoin（`build_is_left` 両方）。

### 10.3 サブプランと三値論理

- Any / All / Hashed の全組み合わせの表: lhs ∈ {1, 2, NULL}、副問い合わせの値の集合 ∈ {空, {1}, {2}, {NULL}, {1, NULL}, {2, NULL}, {1, 2}} × 演算 ∈ {`=`, `<>`, `<`}。Rescan・InitOnce・Hashed（`=` のみ）の 3 戦略の結果が、テストコード内の三値論理の参照実装と一致する。`NOT IN`（`Not(Any)`）も。
- 2 列の Hashed: §4.2.5 の検証済みの 11 件。
- Scalar: 0 行は NULL、2 行は 21000（Rescan と InitOnce の両方）。Exists は 1 行目で打ち切り、次の評価で `rewind` が呼ばれる（`CountingExec`）。
- InitOnce は副問い合わせを 1 回しか実行しない（外側 100 行で `next` の回数が副問い合わせ分だけ）。
- 入れ子（副問い合わせの中の副問い合わせで外々側の `Param` を使う）で、`free_params` により再利用が誤らない（外側の行ごとに結果が変わる）。
- take / put_back: エラー後でも Executor が戻る。`Lent` の再入で `Error::internal`。

### 10.4 DML

- Insert: column_map の欠けた列に DEFAULT、NOT NULL（列の順に最初の違反）、CHECK（NULL は通す、名前順で最初の違反が報告される）、23502 / 23514 / 23505 の message・DETAIL・`s` / `t` / `c` / `n` フィールド（§7.3、§7.4 の表の値を完全一致で比べる）。
- `insert_with_indexes`: 複数インデックスの OID 順（PK が先に報告される）、複数列の DETAIL、NULL を含むキーは衝突しない、同じ文の中での衝突、失敗後もそれまでの項目が残る（取り消さない）こと。
- `update_with_indexes`: キー列が変わらない UPDATE が一意インデックスで衝突しない、キーが他の行と衝突すれば 23505、`SelfModified` ではインデックスに触らない（`FakeIndexStore` の項目数が増えない）。
- Update: `FakeStore::force_result` で `SelfModified { cmax == cid }`（飛ばす・数えない・RETURNING なし）、`cmax != cid`（27000）、`Invisible`（内部エラー）。`UPDATE ... FROM` で同じ行が 2 回当たる入力。`SET a = b, b = a`（Project が評価するので Update ノードの入力で入れ替え済み）。
- Delete: 同様。RETURNING は古い行。
- RETURNING: Insert / Update は新しい行、Delete は古い行。件数と RETURNING の行数が一致する。
- 割り込み: 入力の途中で `request_terminate` すると DML ノードが `57P01`。

### 10.5 集約の PostgreSQL との照合

`tests/slt/m4/agg/*.slt`（K の担当。PostgreSQL で期待値を確かめる）に、この章の検証済みの値を含める。ORDER BY か rowsort を付ける。

| ファイル | 内容 |
|---|---|
| `agg/empty.slt` | 空入力（GROUP BY なしで 1 行、あれば 0 行）、`count` 0、他 NULL、`HAVING` だけの問い合わせ |
| `agg/types.slt` | `sum(int2/int4)` = bigint、`sum(int8)` = numeric、`9223372036854775807 + 1`、`avg(int*)` = numeric、`avg(float4/float8)` = double、`sum(float4)` = real、`pg_typeof` |
| `agg/avg_scale.slt` | §6.4 の表の値 |
| `agg/minmax.slt` | text、char(n)、numeric、date、NaN、NULL のみ |
| `agg/bool.slt` | `bool_and` / `bool_or` / `every` と NULL |
| `agg/distinct_filter.slt` | `count(DISTINCT)`、`sum(DISTINCT)`、`avg(DISTINCT)`、`FILTER` が全部落とす、GROUP BY との組み合わせ |
| `agg/overflow.slt` | `sum(float8)` / `sum(float4)` / `avg(float8)` の `22003 value out of range: overflow`。`sum(int4)` の `bigint out of range` は生成が重いので Rust の単体テスト（`AggState::transition` を直接呼ぶ） |

Rust の `AggState` 単体テストは、各 `AggKind` の初期値・遷移・NULL・結果型と、numeric の `avg` の rscale を §6.4 の表で確かめる。

### 10.6 メモリ上限

- 単体: `MemBudget::new(小さい値)` で、Sort・Materialize・HashJoin・HashAggregate・Distinct・HashSetOp・CteScan・Hashed のそれぞれが `53200` を返す。課金の合計が `used()` と一致する。再構築で課金が返る。`rewindable = false` で枯渇後に課金が返る（`used()` が 0 に戻る）。上限ちょうどは成功、1 バイト足りないと失敗。
- 結合した Sort と HashJoin が直列のとき、`rewindable = false` で上限を超えない（D5-22）。
- slt（`onlyif yuzhu`）: `SET yuzhu.query_mem_limit = '1MB'` のあと `generate_series(1, 1000000)` の ORDER BY・GROUP BY・自己結合が `53200`、上限を戻すと成功。メッセージの先頭は `out of memory`。

### 10.7 割り込み

各ノードについて、ループが入力を読むたびに `check_interrupts` を呼ぶことを、`InterruptAfter { n }` を子にして確かめる（n 行目で `request_terminate`。ノードが `57P01`（FATAL）を返し、構築ループ・読み飛ばしループ・内側ループの途中で止まること）。対象: Sort の構築、HashJoin の構築と probe と bucket の走査、HashAggregate の構築、Materialize、NLJ の内側、Limit の OFFSET の読み飛ばし、Filter の読み飛ばし、CteScan、Hashed SubPlan の構築、FunctionScan、DML。

### 10.8 `types/hash.rs`

`cmp_datum(a, b) == Equal` なら `hash_datum` が同じになる性質を、変種ごとの値の一覧（整数の幅違い、`-0.0` と `0.0`、NaN どうし、`1.10` と `1.1`、`'a'` と `'a  '`（bpchar）、日時、NULL）の全ペアで確かめる。`HashKey` の `Eq` と `Hash` が一致する。

---

## 11. 実装の分担と工数

00 §17 の担当に従う（各担当は自分のファイルだけを編集する）。

| 担当 | ファイル | 中身 | 日数 |
|---|---|---|---|
| X1 | `executor/subplan.rs`、`nodes/{nested_loop,hash_join,materialize}.rs` | SubLink の評価（§4.2）と `SubPlanStates`、NLJ / NLP、HashJoin（§5.15）、Materialize、結合の相互比較テスト | 5 |
| X2 | `executor/{agg,mem}.rs`、`types/hash.rs`、`nodes/{aggregate,hash_aggregate,group_aggregate,unique,append,hash_setop,cte_scan,function_scan}.rs`、Sort・Distinct への課金の追加 | 集約（§6）、`MemBudget`、ハッシュ、集合演算、CTE、`generate_series`、`CteStates` | 5 |
| X3 | `executor/dml.rs`、`nodes/{index_scan,insert,update,delete}.rs`、`test_util` の `FakeIndexStore` と `FakeStore::fetch` | §5.4、§7 | 4 |
| P0（02 章） | `executor/{mod,build,eval}.rs`、既存ノード（Result / Values / SeqScan（filter）/ Filter / Project / Sort / Distinct / Limit）の `PhysExpr` 化と `rewind` 化、`BuildOptions` / `QueryState` / `free_params` / `build_scoped` の足場、`ExecCtx` の拡張、`eval_with_sub_row` と `subplan::eval_sublink` のスタブ | 足場。X1〜X3 はそのスタブの中身を書く | （02 章に含まれる） |
| E1（10 章） | `executor/instrument.rs` | §9 | （10 章に含まれる） |

- 依存: X1・X2・X3 はすべて P0 に依存。X2 は T1（numeric の橋渡し）に、X3 は B1・B2（`IndexStore`）に依存する（未完成の間は `FakeIndexStore` で進める）。
- X1 と X2 は互いのファイルに触らない（`HashKey` は X2 の `types/hash.rs`。X1 は P0 の段階で置かれたスタブ（`HashKey` を `cmp_datum` で等しいかとハッシュの暫定実装）を使い、X2 の完成後に差し替わる）。

---

## 12. 未検証の点（実装前に確かめるもの）

- `ExecRelCheck` が CHECK 制約を名前順に評価すること（D5-19）。2 つの CHECK に違反する行で報告される制約名を実機で確かめる。
- `BuildIndexValueDescription` の値を切り詰めない挙動は 100 文字の text で確認したが、極端に長い値（数 KB）の扱いは未検証。
- `avg(float8)` の `float8_accum`（`Sxx` の計算）でオーバーフローする入力（D5-12）。`avg(1e200, -1e200)` が PostgreSQL でエラーになるか。
- `sum(float4)` の PostgreSQL の内部精度（f32 の加算をそのまま使うか）。小さな値の和で `f32` と `f64` の結果が違う入力を実機で確かめる。
- `HashedSubPlan` の複数列で、probe が NULL を含み `full_rows` を走査する場合の PostgreSQL との完全な一致（上の 11 件は一致。ランダムな組み合わせは差分ランダムテスト（Z）で確かめる）。
- 相関のある副問い合わせが `Materialize` を内側に持つ場合の `free_params` の取りこぼし（`SubPlanDef.explain` や `NestedLoopParam` の入れ子）。差分ランダムテストの対象にする。
- `Update` の `SelfModified` で `cmax == cid` の判定が、同じ文の中の複数のコマンド ID（`UPDATE` の中で `nextval` を呼ぶ関数など）で変わるか。M4 では 1 文 = 1 コマンド ID。
- `generate_series` に `int4` と `int8` が混ざる呼び出しは analyzer が解決する前提（この章は解決後の型だけを受ける）。

---

## 13. 確認事項

ユーザーの不在中に仮決めしたことです。各項目に「仮決め」「理由」「変えたい場合の影響」を書く。ディスク形式に関わるものはない（この章に ★ はない）。

- **[05-Q1] メモリの課金は文の終わりまで保持し、`rewindable = false` のノードだけ枯渇で返す**
  - 仮決め: D5-22。再構築の前と、`rewindable = false` のノードが読み切った時点でだけ課金を返す。
  - 理由: 親が保持している行の二重計上を避ける複雑な管理を作らず、安全側（実メモリより多め）に数える。直列の重いノード（HashJoin → Sort）で上限を超えにくくする。
  - 変えたい場合の影響: 厳密な追跡（ノードの `Drop` で返す）にするには `Executor` に `fn release(&mut self, ctx)` を足し、親が子の `release` を呼ぶ規約が必要。全ノードの変更（+1 日）。

- **[05-Q2] 溜めた結果の再利用判定に `free_params` を使う（00 の `uses_params()` は使わない）**
  - 仮決め: D5-3。
  - 理由: `uses_params()` は SubLink の `SubPlanDef.params` を見られず、再利用を誤る。誤りを避けるだけなら常に作り直せばよいが、非相関の副問い合わせを含む inner が外側の行ごとに作り直される（性能が悪い）。
  - 変えたい場合の影響: 00 の `uses_params()` を使うなら、SubLink を含むノードは保守的に「依存」にする（`free_params` の `query = None` と同じ）。正しさは保たれるが遅くなる。

- **[05-Q3] 23505 の DETAIL を `dml.rs` が補う**
  - 仮決め: D5-16。B+Tree（`06-btree.md`）は 23505 と `s` / `t` / `n` を付け、`detail` は付けなくてよい。付けた場合は `dml.rs` は上書きしない。
  - 理由: `IndexStore::insert` に `TypeEnv` がなく、日時の出力が B+Tree 側で決まらない。
  - 変えたい場合の影響: B+Tree 側で DETAIL を作るなら、`IndexStore::insert` に `&TypeEnv` を足す（00 §13.2 の変更。06 章と B1 / B2 に波及）。

- **[05-Q4] RETURNING は対象表の列だけ**
  - 仮決め: D5-17。FROM / USING の列を参照する RETURNING は `0A000`（analyzer）。
  - 理由: 00 の `BoundReturning` と `PhysicalPlan::Update/Delete.returning` が対象表の 1 行だけを見る式（`PhysExpr`）。
  - 変えたい場合の影響: `returning` の式を入力行（結合後の行）に対して評価できるよう、planner が RETURNING の式を入力の Project に出して Update / Delete が最後の列を使う形に変える（02・04 章と X3、+1 日）。

- **[05-Q5] 相関のある MATERIALIZED CTE を拒否する**
  - 仮決め: D5-20。planner が `0A000`（`correlated MATERIALIZED CTE is not supported`）にする。executor は内部エラー。
  - 理由: 共有ストアを `Param` の変化に合わせて作り直す仕組みが M4 の範囲に対して重い。実用上まれ。
  - 変えたい場合の影響: `CteSlot` に世代番号を持たせ、`SubPlan` の `rewind` ごとに世代を進める（X2、+1 日）。

- **[05-Q6] NestedLoopJoin の outer は left、inner は right**
  - 仮決め: D5-8。入れ替えは planner が Project で表す。
  - 理由: 00 の「出力は常に左 ++ 右」を NLJ にも適用した。
  - 変えたい場合の影響: INNER の NLJ で内側と外側を入れ替えたいとき（`outer_is_right`）を足すと `NestedLoopJoin` に 1 フィールドが増える。

- **[05-Q7] HashSetOp に UNION を実装する**
  - 仮決め: D5-13。planner は使わない。
  - 理由: データ構造が同じで実装が数行。契約の網羅。
  - 変えたい場合の影響: 削除してよい（`Error::internal` にする）。

- **[05-Q8] HashAggregate の出力はグループの初出順**
  - 仮決め: D5-11。
  - 理由: テストが安定する。PostgreSQL は不定なので、PostgreSQL と比べる slt は ORDER BY を付ける。
  - 変えたい場合の影響: 順序を変える（例: ハッシュ順）と、yuzhu だけの EXPLAIN テストや単体テストの期待値が変わる。

- **[05-Q9] 相関のある副問い合わせの結果を、パラメータが前回と同じなら再利用する最適化はしない**
  - 仮決め: 入れない（§4.2.2）。毎回 `rewind` して実行する。
  - 理由: 副問い合わせの中に volatile 関数（`random()`、`nextval`）があると誤る。判定には関数の volatility の情報が要る。
  - 変えたい場合の影響: planner が「副問い合わせが immutable / stable だけ」を `SubPlanDef` に持たせれば、X1 が 1 エントリの結果キャッシュを足せる（+0.5 日）。同じ外側の値が続くデータで速くなる。

- **[05-Q10] 計測の対応づけのための `ExplainNode.phys_id`**
  - 仮決め: 合成ノードを持つ場合だけ必要（§9 の 5）。
  - 理由: 00 は ExplainNode が PhysicalPlan と同形としているが、PostgreSQL の `Hash` ノードなどを出すなら同形でなくなる。
  - 変えたい場合の影響: 同形を厳守するなら 10 章が `Hash` ノードを `Hash Join` の詳細行（出力文字列）として偽装する必要がある（E1 の負担）。

---

## 14. 00 への変更提案

| # | 対象 | 提案 | 理由 |
|---|---|---|---|
| 05-P1 | `PhysicalPlan`（00 §9.2） | `children(&self) -> Vec<&PhysicalPlan>`、`exprs(&self) -> Vec<&PhysExpr>`（そのノード自身の式。filter・keys・params・returning など）、`bound_params(&self) -> Vec<ParamId>`（NestedLoopParam が束縛するもの）を足す。`uses_params()` は残す。executor は `free_params` を使う（D5-3） | `free_params` と先行順の通し番号（§9）の両方が木の走査を要する。`uses_params()` は SubLink を越えられない |
| 05-P2 | `Executor`（00 §10） | `fn extra_stats(&self) -> Vec<(&'static str, u64)> { Vec::new() }` を既定実装つきで足す | EXPLAIN ANALYZE の追加の計測値（Rows Removed by Filter など） |
| 05-P3 | `EvalCtx`（M3 §4.9 / 00 §10 の `eval_ctx`） | `type_env: &'a TypeEnv<'a>` と `params: &'a [Datum]` を足す | キャスト（`InOut`）が `TypeEnv` を要る。`eval_const` が `Param` を読めるようにする |
| 05-P4 | `ExplainNode`（00 §9.3） | `phys_id: Option<u32>` を足す（合成ノードは None） | 計測値との突き合わせ（§9 の 5） |
| 05-P5 | `executor::build`（00 §10） | `build_query(&PhysicalQuery, &BuildOptions)`、`BuildOptions`、`QueryState` を足す。`build(plan)` は単体テスト用に残す | `free_params` に `PhysicalQuery` が要る。計測の差し込み口 |
| 05-P6 | `IndexStore::insert`（00 §13.2） | 変更なし。ただし、23505 の `detail` は付けても付けなくてもよいと 06 章に伝える（D5-16） | 責任の分担 |
| 05-P7 | `PhysicalPlan::NestedLoopJoin`（00 §9.2） | 注記を足す: `outer` は論理プランの left、`inner` は right（D5-8） | 「出力は左 ++ 右」の解釈の統一 |
| 05-P8 | 04 章への依頼 | 相関のある MATERIALIZED CTE を `0A000` にする（D5-20）。`PhysicalPlan::Insert/Update/Delete.checks` の整列は executor が行うので、planner は順序を気にしなくてよい | 実行の前提 |
