# yuzhu 設計調査: 既存DBのコード構造と yuzhu-core のモジュール設計提案

調査日: 2026-10-04。GitHub 上の各リポジトリの HEAD (default branch) のツリーと主要ファイルを `gh api` で確認した。リンクは default branch を指しているため、将来ファイルが移動するとリンク切れになることがある。行番号は省略した。
記号の意味: 【確認】は今回ソースを開いて確かめた事実、【記憶】は過去の知識に基づく記述で、今回は細部まで照合していないもの。

---

## 0. 結論

- **クレートは `yuzhu-core` 1つ + モジュール分割で始める**。ただし、モジュール間の依存方向を後でクレートに切り出せる形 (DAG) に保つ。プロトコルは `yuzhu-server` 側に置く。分割を検討するのは M3 以降、ビルド時間が実際に問題になってからで遅くない。
- パイプラインは **AST → Bound (解決済み) ツリー → 論理プラン → 物理プラン → Executor ツリー**。M1 では「論理プラン = 物理プラン」(toydb 方式) でもよいが、`binder` だけは最初から独立させる。PostgreSQL 互換 (型解決、`$1` パラメータ、エラーコード) は binder がないと破綻する。
- Executor は **`trait Executor { fn next(&mut self, ctx: &mut ExecCtx) -> Result<Option<Row>>; }` + `Box<dyn Executor>`** にする。コンテキストは `next` の引数として毎回渡し、構造体には保持させない。こうすると、トランザクションやバッファプールの借用が executor の寿命と絡まない。
- バッファプールは **`Arc<Frame>` + `parking_lot`/`std` の `RwLock` で中身を守り、ピンカウントは RAII ガード (`ReadGuard`/`WriteGuard`) の Drop で減らす**。ガードはプール本体を借用せず `Arc` で所有する (BusTub の `ReadPageGuard` が `shared_ptr<FrameHeader>` を持つのと同じ構造)。Executor はページガードを `next()` 呼び出しをまたいで保持せず、「(page_id, slot) カーソル + 毎回ピン」にする。`unsafe` なしで成立する。

---

## 1. 各プロジェクトの調査

### 1.1 toydb (erikgrinaker/toydb), 2024年の全面書き直し後の版

教育用。分散 (Raft) + MVCC + SQL。yuzhu に最も近い参照先で、特に「単純さを優先する判断」が参考になる。

**モジュール構成 (単一クレート)【確認】**
- `src/sql/parser/{lexer,parser,ast}.rs`: 手書きの再帰下降と Pratt (優先順位上昇) 法
  - https://github.com/erikgrinaker/toydb/blob/main/src/sql/parser/parser.rs
  - https://github.com/erikgrinaker/toydb/blob/main/src/sql/parser/ast.rs
- `src/sql/planner/{planner,plan,optimizer}.rs`: AST から `Plan`/`Node` を作る。名前解決は planner 内の `Scope` が担い、独立した binder 段はない。
  - https://github.com/erikgrinaker/toydb/blob/main/src/sql/planner/plan.rs
  - https://github.com/erikgrinaker/toydb/blob/main/src/sql/planner/optimizer.rs (定数畳み込み、フィルタ押し下げ、インデックス選択、HashJoin 化などをノード書き換えで行う)
- `src/sql/execution/{executor,aggregator,join,session}.rs`
  - https://github.com/erikgrinaker/toydb/blob/main/src/sql/execution/executor.rs
- `src/sql/engine/{engine,local,raft}.rs`: SQL が要求するストレージ抽象 (`Engine`/`Transaction`/`Catalog` トレイト)
  - https://github.com/erikgrinaker/toydb/blob/main/src/sql/engine/engine.rs
- `src/sql/types/{value,schema,expression}.rs`
- `src/storage/{engine,bitcask,memory,mvcc}.rs`: KV エンジン抽象と、その上に載る MVCC
  - https://github.com/erikgrinaker/toydb/blob/main/src/storage/mvcc.rs
- `src/encoding/{keycode,bincode,format}.rs`: 順序を保つキーエンコーディング
- `src/raft/*`, `src/server.rs`, `src/client.rs`, `src/error.rs`

**パイプライン**: AST → `Plan` (ルートの文種別: `CreateTable`/`Insert`/`Select(Node)` など) と `Node` (`Scan`/`Filter`/`Projection`/`HashJoin`/`NestedLoopJoin`/`Aggregate`/`Order`/`Limit`/`Values`…) の enum ツリー → optimizer がノードを書き換え → executor が実行。論理プランと物理プランは区別しない。HashJoin や IndexLookup も同じ `Node` enum に入っている。

**Executor の形【確認】**: 独自の Volcano トレイトはなく、**標準の `Iterator` を使う**。
```rust
pub type Row = Vec<Value>;
pub type Rows = Box<dyn RowIterator>;
pub trait RowIterator: Iterator<Item = Result<Row>> + DynClone {}
```
`Executor<'a, T: Transaction> { txn: &'a T }` が `Node` を再帰的に辿り、`Rows` (イテレータアダプタの合成) を返す。Clone できるのは、NestedLoopJoin で右側を巻き戻すため (`dyn_clone`)。

**借用問題の解き方**: `Transaction` トレイトのメソッドはすべて `&self`。`scan()` は `Result<Rows>` を返し、**行の実体をイテレータが所有する** (MVCC の scan はエンジンの `Arc<Mutex<E>>` をロックし、バッファリングしてから返す)。したがってイテレータは txn を借用しない。`storage::mvcc::Transaction<E>` は `engine: Arc<Mutex<E>>` を持つ【確認】。ディスクページを扱わない (Bitcask = ログ構造 KV) ため、バッファプールの借用問題そのものが存在しない。この点で yuzhu とは前提が違う。

**値と行の表現【確認】**: `enum Value { Null, Boolean(bool), Integer(i64), Float(f64), String(String) }`, `enum DataType`, `Row = Vec<Value>`。カラム名は `Label { None, Unqualified, Qualified }`。

**カタログ**: `trait Catalog { create_table, drop_table, get_table, list_tables, must_get_table }`。`Transaction: Catalog` なので、**カタログ操作もトランザクショナル** (MVCC の KV に格納される)。PostgreSQL のシステムカタログと同じ考え方。

**エラー【確認】**: `enum Error { Abort, InvalidData(String), InvalidInput(String), IO(String), ReadOnly, Serialization }` という最小構成で、`Clone + Serialize` (Raft で送るため)。`errinput!` マクロで生成する。

**テスト**: goldenscript (`src/sql/testscripts/{queries,expressions,optimizers,schema,transactions,writes}`、`src/storage/testscripts/mvcc` など) によるスナップショット型のデータ駆動テスト。プランの表示、実行結果、KV 操作ログを期待値ファイルと比較する。yuzhu でも強く推奨したい方式。

---

### 1.2 RisingLight (risinglightdb/risinglight)

教育用の OLAP DB。ベクトル化実行、egg (e-graph) を使った最適化、async。

**構成 (単一クレート `src/`)【確認】**
- `parser/`: sqlparser-rs のラッパー
- `binder/{select,insert,create_table,expr,table,…}.rs`: AST を束縛済みの表現に変換する
  - https://github.com/risinglightdb/risinglight/tree/main/src/binder
- `planner/{mod,optimizer,cost,rules/*}.rs`: **egg の `define_language!` で、式とプランを一つの `Expr` 言語として表現**し、`RecExpr` に格納する。ルールは `rules/{plan,expr,agg,order,range}.rs`
  - https://github.com/risinglightdb/risinglight/blob/main/src/planner/mod.rs
- `executor/{table_scan,filter,projection,hash_join,hash_agg,order,top_n,…}.rs`
  - https://github.com/risinglightdb/risinglight/blob/main/src/executor/mod.rs
- `catalog/{root,schema,table,column,index,function}.rs`: `RootCatalog` が階層構造を持つ (Arc + Mutex)
- `storage/{memory,secondary,index}`: `Storage` トレイトの実装が2つ (インメモリとカラムナ)
- `array/`: カラムナ配列 `DataChunk`
- `types/`, `db.rs`

**パイプライン**: AST → binder (いまは egg の `RecExpr` を直接生成) → egg による等価飽和最適化 + コストベースの抽出 → executor builder → stream ツリー。

**Executor の形【確認】**:
```rust
pub type BoxedExecutor = BoxStream<'static, Result<DataChunk>>;
pub fn build(optimizer: Optimizer, storage: Arc<impl Storage>, plan: &RecExpr) -> BoxedExecutor
```
各 executor は `#[try_stream]` の async generator で、**`'static` の stream を `Arc<Storage>` で所有させる**。こうして借用問題を回避している。共有サブプランは `async_broadcast` で複数の購読者に配る。

**値と行**: カラムナの `DataChunk` (配列の束) と、スカラの `DataValue` enum。

**エラー**: モジュールごとに `binder/error.rs`、`executor/error.rs`、`storage/error.rs` を置き、`thiserror` で定義してトップで束ねる。

**テスト**: sqllogictest (`tests/sql/*.slt`) と planner test (プランのスナップショット)。

**yuzhu にとっての教訓**: binder を独立させる設計は真似る価値がある。egg、async、ベクトル化は yuzhu (同期、行指向、M1) には過剰。「'static な executor + Arc で所有」という手法は参考になる。

---

### 1.3 GlueSQL (gluesql/gluesql)

ストレージ差し替え可能な SQL ライブラリ。ストレージ抽象の設計が参考になる。

**ワークスペース構成【確認】**: `core/` (エンジン本体)、`storages/{memory,shared-memory,sled,redb,json,csv,parquet,file,git,mongo,redis,composite}-storage/`、`cli/`、`test-suite/`、`macros/`、`pkg/` (言語バインディング)。
- `core/src/{ast, parse_sql.rs, translate/, plan/, planner/, executor/, data/, store/, query_builder/, glue.rs}`
  - https://github.com/gluesql/gluesql/tree/main/core/src

**パイプライン**: sqlparser-rs の AST → **自前の `ast` に translate** (外部 AST をそのまま内部で使わず、自前の安定した AST に変換している点が重要) → `plan/` (主キー検出、インデックス選択、結合計画などの書き換え。AST を変換していく方式) → `executor/` が AST に近い構造を直接評価する。

**Executor の形【記憶+一部確認】**: Volcano の Executor ツリーは作らず、`executor/select.rs` などが async Stream (`futures::Stream`) のコンビネータで行を流す。`fetch()` が `GStore` から行の stream を得る。

**ストレージ抽象【確認】** (https://github.com/gluesql/gluesql/blob/main/core/src/store.rs):
```rust
pub trait Store    { fn fetch_schema(&self, ..); fn fetch_data(&self, table, key); fn scan_data<'a>(&'a self, table) -> Result<RowIter<'a>>; .. }
pub trait StoreMut { fn insert_schema(&mut self, ..); fn append_data(&mut self, ..); fn insert_data(..); fn delete_data(..); .. }
pub trait GStore: Store + Index + Metadata + CustomFunction {}
```
読み取り (`&self`) と書き込み (`&mut self`) をトレイトごと分け、さらに `Transaction`、`AlterTable`、`Index` などの機能を別トレイトに分けている (どれもデフォルト実装は「未サポート」エラー)。`Glue<T>` がストレージを所有し、実行時に `&mut` で渡す。

**値【確認】**: 大きな `enum Value { Bool, I8..I128, U8..U128, F32, F64, Decimal, Str, Bytea, Inet, Date, Timestamp, Time, Interval, Uuid, Map, List, Point, Null }`。`Row` は `Vec<Value>` または schemaless 用の Map。

**テスト**: `test-suite` クレートに共通テストを集め、**全ストレージ実装に対して同じテストを流す** (マクロで生成)。yuzhu で M1 (メモリ) と M2 (ディスク) の両方に同じ SQL テストを流す設計に直接使える。

---

### 1.4 Limbo / Turso (tursodatabase/turso)

SQLite の Rust による再実装。旧名 Limbo。現在のリポジトリは `tursodatabase/turso`。

**構成【確認】**: ワークスペース。主なもの:
- `sqlite/parser/src/{lexer,parser,ast}.rs`: 自前の手書きパーサ (以前は lemon 系の fork だった)
  - https://github.com/tursodatabase/turso/tree/main/sqlite/parser/src
- `core/translate/{select,insert,delete,planner,plan,logical,optimizer/,emitter/,main_loop/,expr/,…}.rs`: AST からプランを作り、**VDBE バイトコードへ emit** する
  - https://github.com/tursodatabase/turso/blob/main/core/translate/plan.rs
  - https://github.com/tursodatabase/turso/blob/main/core/translate/optimizer/mod.rs
- `core/vdbe/{mod,builder,insn,execute}.rs`: バイトコード VM
  - https://github.com/tursodatabase/turso/blob/main/core/vdbe/mod.rs
- `core/storage/{pager,page_cache,buffer_pool,btree,wal,sqlite3_ondisk,database}.rs`
  - https://github.com/tursodatabase/turso/blob/main/core/storage/pager.rs
  - https://github.com/tursodatabase/turso/blob/main/core/storage/btree.rs
- `core/mvcc/`: 実験的な MVCC (logical log)
- `core/schema.rs` (カタログ)、`core/types.rs` (`Value`、`ImmutableRecord`、`IOResult`)、`core/error.rs`
- `bindings/*`、`cli/`、`simulator`/`testing`/`fuzz`/`tlaplus` (決定論的シミュレーションと形式手法)、`postgres/` (PostgreSQL 互換レイヤの試み)

**パイプライン**: AST → `translate::plan` (SelectPlan など。結合順序の最適化とインデックス選択) → **Volcano ではなく SQLite と同じバイトコード**の `Program { insns: Vec<(Insn, usize)> }` → `Program::step()` が `StepResult` (Row/IO/Done/Busy) を返す。

**借用と I/O の扱い【確認】**:
- `pub type PageRef = Arc<Page>`。`Page` の内部は `PageInner { buffer: Option<Arc<Buffer>>, ... }`。`Pager` は `page_cache: Arc<RwLock<PageCache>>`、`buffer_pool: Arc<BufferPool>`、`wal: Option<Arc<dyn Wal>>`、`io: Arc<dyn IO>` を持つ。
- **すべての I/O を `IOResult<T> { Done(T), IO(..) }` 型で返し、再開可能なステートマシン (`return_if_io!` マクロ) にしている**。io_uring などの非同期 I/O のための設計で、同期 I/O の yuzhu には不要。
- 内部では `unsafe` を部分的に使っている (バッファ管理、FFI)。`forbid(unsafe_code)` の yuzhu はそのまま真似できない。

**値**: `enum Value` (Null/Integer/Float/Text/Blob) と、借用版の `ValueRef<'a>`、`ImmutableRecord` (SQLite レコード形式のバイト列をそのまま保持し、遅延デコードする)。**所有版と借用版の値を分けて、ゼロコピー読み出しを実現**している。

**テスト**: SQLite の TCL テスト互換、Python の互換テスト、`simulator` (決定論的シミュレーションテスト)、fuzz、Antithesis。

**yuzhu にとっての教訓**: B+Tree と pager の分離 (`btree.rs` が `Pager` を介してページを得る)、ページキャッシュとバッファプールの分離は参考になる。VDBE と非同期 I/O ステートマシンは採用しない。

---

### 1.5 DataFusion (planner / 論理プラン設計のみ)

- AST → 論理プラン: `datafusion/sql/src/{planner,statement,query,select}.rs` の `SqlToRel`。**binder と論理プランナが一体**で、名前解決と型付けは `ContextProvider` (カタログのトレイト) を引きながら `LogicalPlan` を直接構築する。
  - https://github.com/apache/datafusion/blob/main/datafusion/sql/src/planner.rs
  - https://github.com/apache/datafusion/blob/main/datafusion/sql/src/select.rs
- 論理プラン: `enum LogicalPlan { Projection(Projection), Filter(Filter), Join(Join), Aggregate(Aggregate), Sort, Limit, TableScan, Values, Dml, Ddl, ... }`。各ノードは `Arc<LogicalPlan>` の子と `DFSchemaRef` (出力スキーマ) を持つ。
  - https://github.com/apache/datafusion/blob/main/datafusion/expr/src/logical_plan/plan.rs
  - https://github.com/apache/datafusion/blob/main/datafusion/expr/src/logical_plan/builder.rs (`LogicalPlanBuilder`。テストや API から plan を組み立てるのに便利)
  - 式: https://github.com/apache/datafusion/blob/main/datafusion/expr/src/expr.rs (`Expr::Column(Column{relation, name})`。名前で列を参照し、物理化の時点でインデックスに解決する)
- 論理最適化: `trait OptimizerRule` の列を固定点まで適用する。`TreeNode` トレイト (transform_up/down) で汎用的に書き換える。
  - https://github.com/apache/datafusion/blob/main/datafusion/optimizer/src/optimizer.rs
- 物理化: `DefaultPhysicalPlanner` が `LogicalPlan` を `Arc<dyn ExecutionPlan>` に変換する
  - https://github.com/apache/datafusion/blob/main/datafusion/core/src/physical_planner.rs
  - https://github.com/apache/datafusion/blob/main/datafusion/physical-plan/src/execution_plan.rs (`fn execute(&self, partition, ctx: Arc<TaskContext>) -> Result<SendableRecordBatchStream>`)
- カタログ: `CatalogProvider → SchemaProvider → TableProvider` の3階層トレイト (https://github.com/apache/datafusion/blob/main/datafusion/catalog/src/schema.rs, https://github.com/apache/datafusion/blob/main/datafusion/catalog/src/table.rs)
- エラー: 単一の `DataFusionError` enum (`SqlError`/`Plan`/`SchemaError`/`Execution`/`NotImplemented`/`Internal`/`External(Box<dyn Error>)`…) と `plan_err!` などのマクロ。

**yuzhu への示唆**: 「論理プランは子を `Box`/`Arc` で持つ enum + 各ノードが出力スキーマを持つ」「最適化はルールの列」「`execute(&self, ctx)` が新しいストリームを返す (プランと実行状態を分ける)」の3点を取り入れる。`Expr::Column` を名前で参照する方式は柔軟だが、PostgreSQL 風にするなら binder の時点で `(rel_index, col_index)` に解決した方がよい (PostgreSQL の `Var` = varno/varattno と同じ)。

---

### 1.6 sqlparser-rs (apache/datafusion-sqlparser-rs), AST 設計の参照のみ

- https://github.com/apache/datafusion-sqlparser-rs/blob/main/src/ast/mod.rs (`Statement` enum、`Expr` enum)
- https://github.com/apache/datafusion-sqlparser-rs/blob/main/src/ast/query.rs (`Query { with, body: Box<SetExpr>, order_by, limit, .. }`, `SetExpr::{Select, Query, SetOperation, Values}`, `Select { projection, from: Vec<TableWithJoins>, selection, group_by, having, .. }`)
- https://github.com/apache/datafusion-sqlparser-rs/blob/main/src/ast/ddl.rs, https://github.com/apache/datafusion-sqlparser-rs/blob/main/src/ast/dml.rs
- https://github.com/apache/datafusion-sqlparser-rs/blob/main/src/tokenizer.rs, https://github.com/apache/datafusion-sqlparser-rs/blob/main/src/parser/mod.rs (式は Pratt 法の `parse_subexpr(precedence)`、優先順位は Dialect が返す)
- https://github.com/apache/datafusion-sqlparser-rs/blob/main/src/dialect/postgresql.rs

**参考にする点**
- `Query`/`SetExpr`/`Select` の3層構造。UNION、VALUES、サブクエリ、CTE を自然に表現できる。PostgreSQL の `SelectStmt` (gram.y) も、op/larg/rarg で集合演算を表す同じ構造をしている。
- `Ident { value, quote_style }`: 引用符の有無を保持する。PostgreSQL では、引用符なしの識別子を小文字に畳み込み、`"Foo"` はそのまま残す。**yuzhu ではレキサの段階で畳み込みを済ませ**、AST には正規化済みの名前と、エラー表示用の `Span` を持たせるのが簡単。
- AST ノードに `Span` (位置情報) を付けていく方向にある。PostgreSQL のエラー応答の `P` (position) フィールドを返すために、**yuzhu は最初からトークン位置 (バイトオフセット) を AST に持たせる**べき。

**真似しない点**: 多方言対応による巨大さ。AST が実質的に「全方言の和集合」になっていて、`Option` だらけ。yuzhu は PostgreSQL のサブセットだけで小さく作る。

---

### 1.7 CMU BusTub (C++), executor / catalog / buffer pool

**構成【確認】**: `src/{binder, planner, optimizer, execution, catalog, buffer, storage/{page,table,index,disk}, concurrency, recovery, type, common}` と `src/include/...`。

**パイプライン**: libpg_query (PostgreSQL のパーサを流用) → `binder/` (`BoundStatement`, `BoundExpression`, `BoundTableRef`) → `planner/` (`plan_select.cpp` などで `AbstractPlanNode` ツリーに変換) → `optimizer/` (ルールベース: `nlj_as_hash_join`, `seqscan_as_indexscan`, `sort_limit_as_topn`, `merge_filter_scan` など) → `execution/executor_factory.cpp` がプランノードから executor を生成。
- https://github.com/cmu-db/bustub/tree/main/src/binder
- https://github.com/cmu-db/bustub/tree/main/src/planner
- https://github.com/cmu-db/bustub/tree/main/src/optimizer
- https://github.com/cmu-db/bustub/blob/main/src/execution/executor_factory.cpp

**Executor【確認】** (https://github.com/cmu-db/bustub/blob/main/src/include/execution/executors/abstract_executor.h):
```cpp
class AbstractExecutor {
  explicit AbstractExecutor(ExecutorContext *exec_ctx);
  virtual void Init() = 0;
  virtual auto Next(std::vector<Tuple> *tuple_batch, std::vector<RID> *rid_batch, size_t batch_size) -> bool = 0;
  virtual auto GetOutputSchema() const -> const Schema & = 0;
};
```
**現行版の `Next` はバッチ版**になっている (以前は `Next(Tuple*, RID*)` の1行版)。プランノード (不変) と executor (状態を持つ) を分け、`Init()` で巻き戻しができる (NLJ の内側を再スキャンするため)。

**ExecutorContext【確認】** (https://github.com/cmu-db/bustub/blob/main/src/include/execution/executor_context.h): `GetTransaction()`、`GetCatalog()`、`GetBufferPoolManager()`、`GetLockManager()`、`GetTransactionManager()`、`GetLogManager()`。生ポインタで全 executor が共有する。Rust ではここが借用問題になるので、後述の「`next` の引数で渡す」方式で解く。

**バッファプール【確認】** (https://github.com/cmu-db/bustub/blob/main/src/include/storage/page/page_guard.h, https://github.com/cmu-db/bustub/blob/main/src/include/buffer/buffer_pool_manager.h):
- `ReadPageGuard`/`WritePageGuard` は `std::shared_ptr<FrameHeader>`、`shared_ptr<ArcReplacer>`、`shared_ptr<std::mutex> bpm_latch`、`shared_ptr<DiskScheduler>` を所有する。**ガードが BPM 本体への参照ではなく、共有所有の部品を持つ**。デストラクタ (`Drop()`) で、ラッチ解放 → ピンカウント減少 → replacer に evictable と通知する。
- これは Rust の `Arc<Frame>` + RAII ガードにほぼそのまま対応する。置換方式は LRU-K / ARC。

**カタログ** (https://github.com/cmu-db/bustub/blob/main/src/include/catalog/catalog.h): `TableInfo { schema, name, table_heap, oid }` と `IndexInfo` を `unordered_map<table_oid_t, unique_ptr<TableInfo>>` で持つ。非トランザクショナルでメモリ内のみ。
**値**: `Value` (TypeId + union) と `Tuple` (シリアライズ済みバイト列。`GetValue(schema, idx)` でデコード)。
**テスト**: gtest と SQLLogicTest 互換の `.slt` (`test/sql/*.slt`、`bustub-sqllogictest`)。

---

### 1.8 PostgreSQL 本体 (parser → analyzer → rewriter → planner → executor)

入口は `exec_simple_query()` (https://github.com/postgres/postgres/blob/master/src/backend/tcop/postgres.c):
1. **raw parse**: `pg_parse_query()` → `raw_parser()`。flex の `scan.l` と bison の `gram.y` で、`RawStmt` (生の構文木) を作る。この段ではカタログを参照しない。
   - https://github.com/postgres/postgres/blob/master/src/backend/parser/gram.y, https://github.com/postgres/postgres/blob/master/src/backend/parser/scan.l
   - ノード定義: https://github.com/postgres/postgres/blob/master/src/include/nodes/parsenodes.h
2. **analyze**: `parse_analyze_*()` → `transformStmt()`。カタログを参照して名前解決と型解決を行い、`Query` 構造体 (range table `rtable`、`targetList`、`jointree`、`Var{varno,varattno}`) を作る。
   - https://github.com/postgres/postgres/blob/master/src/backend/parser/analyze.c、`parse_expr.c`、`parse_clause.c`、`parse_target.c`、`parse_relation.c`、`parse_coerce.c` (暗黙キャストの解決)、`parse_func.c`、`parse_oper.c` (演算子の解決)
   - 式ノード: https://github.com/postgres/postgres/blob/master/src/include/nodes/primnodes.h
3. **rewrite**: `pg_rewrite_query()` → `QueryRewrite()`。ビューとルールを展開する。https://github.com/postgres/postgres/blob/master/src/backend/rewrite/rewriteHandler.c
4. **plan**: `pg_plan_queries()` → `planner()` → `standard_planner()` → `subquery_planner()` → `grouping_planner()`。Path (候補) を列挙してコストで選び、`create_plan()` で `Plan` ツリーにする。
   - https://github.com/postgres/postgres/blob/master/src/backend/optimizer/plan/planner.c, `createplan.c`, `optimizer/path/allpaths.c`, `costsize.c`
   - https://github.com/postgres/postgres/blob/master/src/include/nodes/plannodes.h
5. **execute**: Portal → `ExecutorStart/Run/Finish/End` (`execMain.c`)。`ExecInitNode()` が Plan ツリーと**並行する PlanState ツリー** (実行状態) を作り、`ExecProcNode()` で 1 タプルずつ pull する (Volcano)。
   - https://github.com/postgres/postgres/blob/master/src/backend/executor/execMain.c, https://github.com/postgres/postgres/blob/master/src/backend/executor/execProcnode.c, `nodeSeqscan.c`, `nodeHashjoin.c` など
   - https://github.com/postgres/postgres/blob/master/src/include/nodes/execnodes.h (`EState` が snapshot、range table、`es_output_cid` などの実行全体の状態を持つ。これが yuzhu の `ExecCtx` に相当する)

**その他の参考箇所**
- タプルの受け渡し: `TupleTableSlot` (https://github.com/postgres/postgres/blob/master/src/include/executor/tuptable.h)。virtual / heap / minimal の各スロット形式があり、遅延デコード (`slot_getattr`) する。
- 拡張クエリプロトコル: Parse/Bind/Execute が `exec_parse_message`/`exec_bind_message`/`exec_execute_message` (postgres.c) に対応し、plancache (`utils/cache/plancache.c`) を使う。**yuzhu は M1 から「パース結果 + パラメータ型 → 実行時に値を Bind」できる形にしておく**と、後の拡張プロトコル対応が楽になる。
- エラー: `ereport(ERROR, errcode(ERRCODE_...), errmsg(...))`。SQLSTATE の一覧は https://github.com/postgres/postgres/blob/master/src/backend/utils/errcodes.txt にある。
- バッファ: https://github.com/postgres/postgres/blob/master/src/backend/storage/buffer/bufmgr.c (`ReadBuffer` → pin、`LockBuffer` → content lock、`ReleaseBuffer`。**pin と latch を分けている**)
- heap と MVCC: https://github.com/postgres/postgres/blob/master/src/backend/access/heap/heapam.c, https://github.com/postgres/postgres/blob/master/src/backend/access/heap/heapam_visibility.c (`HeapTupleSatisfiesMVCC`)
- カタログキャッシュ: `utils/cache/{relcache,catcache,syscache}.c`
- テスト: `src/test/regress/sql/*.sql` と `expected/*.out` を pg_regress で突き合わせる。並行実行は `src/test/isolation` (spec ファイル)。

---

## 2. 比較表

| 項目 | toydb | RisingLight | GlueSQL | Turso | DataFusion | BusTub | PostgreSQL |
|---|---|---|---|---|---|---|---|
| パーサ | 手書き | sqlparser-rs | sqlparser-rs → 自前 AST | 手書き | sqlparser-rs | libpg_query | bison |
| binder 段 | なし (planner 内の Scope) | あり | なし (plan で変換) | translate 内 | SqlToRel に統合 | あり | analyze |
| 論理/物理の分離 | なし (Node 1種) | egg で統合 | なし | plan → bytecode | 分離 | plan と executor のみ | Path / Plan |
| 実行モデル | Iterator 合成 (`Box<dyn>`) | async stream, ベクトル | async stream | VDBE | async stream, ベクトル | Volcano (バッチ Next) | Volcano |
| 借用の解き方 | イテレータが行を所有 | `'static` + Arc | `&mut` ストレージを渡す | Arc + IOResult (一部 unsafe) | Arc<TaskContext> | (C++) 生ポインタ | (C) |
| 値 | 5種 enum | DataValue + 配列 | 大きな enum | Value / ValueRef | Arrow ScalarValue | Value + Tuple(bytes) | Datum + Slot |
| エラー | 単一 enum (6種) | モジュール別 thiserror | 単一 + サブ enum | 単一 enum | 単一 enum + マクロ | 例外 | SQLSTATE |
| テスト | goldenscript | slt + planner test | 共通 test-suite | 互換 + シミュレーション | slt | gtest + slt | pg_regress |

---

## 3. yuzhu-core への推奨

### 3.1 クレート構成: 1つの core クレート + 厳格なモジュール DAG

```
yuzhu/
├─ Cargo.toml              (workspace, [workspace.lints] で unsafe_code = "forbid")
├─ crates/yuzhu-core/      (lib)
└─ crates/yuzhu-server/    (bin: pgwire プロトコル, 接続ごとのスレッド, 設定, ログ)
```

**理由**
- **ビルド時間**: 数万行までは、単一クレートでもインクリメンタルビルドが十分速い。クレートを分けると並列ビルドの恩恵はあるが、実際に効いてくるのは依存 (外部クレート) が重い場合。yuzhu は外部依存を絞る方針のはずなので、効果は小さい。逆に分割すると、`pub` の範囲、型の置き場所、循環依存の解消といった判断コストが M1 の段階から発生する。
- **境界**: Rust でモジュールの境界を守るには、`pub(crate)` の徹底と、**下位レイヤが上位レイヤを `use` しない**というルールで足りる。ルールは `lib.rs` にコメントで明記し、テスト (`cargo modules` や grep の CI チェック) で監視できる。
- **プロトコルは server 側に置く**: pgwire のメッセージ形式は SQL エンジンの関心事ではない。core は「`Session::execute(sql) -> Result<Vec<QueryResult>>`」と「`prepare`/`bind`/`execute`」の API だけを公開する。将来 `yuzhu-protocol` (コーデックのみ) をクライアントやテストツールと共有したくなったら、その時点で切り出す (依存が少ないので切り出しやすい)。
- **他言語での再実装**: 再実装の容易さを決めるのは**モジュール境界のインターフェース (データ型) が言語非依存かどうか**であり、クレートの数ではない。AST、Bound、LogicalPlan を素直な enum/struct で定義し、ページ形式と WAL 形式を文書化しておくことの方が、はるかに効く。テストを SQL ファイル (slt や golden) で書けば、他言語の実装でもそのまま使い回せる。これが最大の資産になる。
- **分割を検討するタイミング**: (a) `cargo build` のインクリメンタルビルドが 10 秒を超えて開発の妨げになる、(b) storage だけをベンチマークや fuzz で独立して回したい、(c) 外部の利用者が出てくる。このいずれかに当たったら、`yuzhu-storage` (page/buffer/heap/btree/wal) から切り出す。DAG を守っていれば機械的に移動できる。

### 3.2 モジュール構成 (M1 から M5 まで維持できる形)

依存は下 → 上の一方向。下位は上位を知らない。

```
yuzhu-core/src/
├─ lib.rs              #![forbid(unsafe_code)]; 公開 API の再エクスポート
├─ error.rs            Error, ErrorKind, SqlState (PostgreSQL の SQLSTATE), Result<T>
├─ types/              [最下層] 型システム
│   ├─ mod.rs          DataType (Bool, Int2, Int4, Int8, Float8, Text, Varchar(n), Numeric?, ...), Oid の対応表
│   ├─ value.rs        Value enum, 比較 (3値論理), Hash
│   ├─ row.rs          Row (Vec<Value> の newtype)
│   ├─ cast.rs         キャスト規則 (暗黙/代入/明示)
│   └─ encoding.rs     [M2] Value/Row ⇄ バイト列 (タプル形式)
├─ sql/                [カタログ非依存] 字句・構文解析
│   ├─ token.rs, lexer.rs   Span 付きトークン, 識別子の畳み込み, $n パラメータ
│   ├─ ast.rs          Statement / Query / SetExpr / Select / Expr / TableRef / DataTypeName
│   └─ parser/         mod.rs, expr.rs (Pratt), select.rs, ddl.rs, dml.rs
├─ catalog/            カタログ抽象
│   ├─ mod.rs          TableId/ColumnId/IndexId, TableDef, ColumnDef, IndexDef, Schema
│   ├─ provider.rs     trait CatalogReader (binder/planner が使う読み取りビュー)
│   ├─ memory.rs       [M1] インメモリ実装
│   └─ system.rs       [M2+] システムテーブル (yz_class, yz_attribute) に格納するヒープ実装
├─ binder/             AST + カタログ → Bound ツリー (PostgreSQL の analyze に相当)
│   ├─ mod.rs, scope.rs (名前解決と RTE), expr.rs (型推論と coercion の挿入), select.rs, dml.rs, ddl.rs
│   └─ bound.rs        BoundStatement, BoundExpr { kind, ty }, ColumnRef { rel: usize, col: usize }
├─ planner/
│   ├─ logical.rs      LogicalPlan enum (Scan, Filter, Project, Join, Aggregate, Sort, Limit, Values, Insert, Update, Delete, CreateTable ...)
│   ├─ builder.rs      Bound → LogicalPlan
│   ├─ optimizer/      ルールの列 (M1 は定数畳み込みのみ, M3+ で述語押し下げ/インデックス選択)
│   ├─ physical.rs     PhysicalPlan enum (SeqScan, IndexScan, NestedLoopJoin, HashJoin, HashAggregate, Sort ...)
│   └─ explain.rs      EXPLAIN 出力 (golden テストにも使う)
├─ executor/
│   ├─ mod.rs          trait Executor, ExecCtx, build(plan) -> BoxedExecutor
│   ├─ eval.rs         式評価 (BoundExpr を物理化した Expr を Row で評価)
│   ├─ scan.rs, filter.rs, project.rs, nested_loop_join.rs, hash_join.rs, aggregate.rs, sort.rs, limit.rs, values.rs
│   └─ dml.rs, ddl.rs
├─ txn/                トランザクション
│   ├─ mod.rs          TxnId, Snapshot { xmin, xmax, xip }, Transaction (状態, 書き込み集合)
│   ├─ manager.rs      TransactionManager (採番, active 集合, commit/abort, clog)
│   └─ visibility.rs   is_visible(tuple_header, snapshot, clog)
├─ storage/
│   ├─ mod.rs          trait TableStore / IndexStore (heap と btree を executor から隠す抽象)
│   ├─ memory.rs       [M1] BTreeMap ベースの実装 (ただし xmin/xmax はここで既に持つ)
│   ├─ disk.rs         [M2] DiskManager (ページ単位の read/write, fsync)
│   ├─ page.rs         [M2] PageId, ページヘッダ, スロット付きページ (slotted page)
│   ├─ buffer.rs       [M2] BufferPool, Frame, ReadGuard/WriteGuard, 置換ポリシー
│   ├─ heap.rs         [M2] HeapFile (TID = (PageId, slot)), HeapScanCursor
│   └─ btree/          [M3] node.rs, tree.rs, cursor.rs
├─ wal/                [M2] REDO-only WAL
│   ├─ record.rs       WalRecord (ページ単位の物理 REDO / 論理 REDO), LSN
│   ├─ writer.rs       追記, group commit, fsync
│   └─ recovery.rs     REDO の再生
├─ engine.rs           Database (全体の所有者: catalog, txn manager, storage, wal を Arc で持つ)
└─ session.rs          Session (接続単位: 現在のトランザクション, prepared statements, 設定)
                       execute_simple(sql) / prepare / bind / execute
```

**依存方向**: `error, types` ← `sql` ← `catalog` ← `binder` ← `planner` ← `executor` ← `session/engine`。`storage, wal, txn` は `types, error` にだけ依存し、`executor` と `catalog` (システムテーブル実装) から呼ばれる。**`sql` (パーサ) はカタログにもストレージにも依存させない**。PostgreSQL の raw parser と同じで、fuzz や他言語への移植が容易になる。

**M1 から M5 への移行を楽にするための工夫**
- `storage::TableStore` トレイトを M1 から用意し、M1 はメモリ実装 (`memory.rs`)、M2 はヒープ実装にする。**MVCC のヘッダ (xmin/xmax) は M1 のメモリ実装から既に持たせる**。こうすると M2 で変わるのは「どこに置くか」だけで、可視性ロジックは変わらない。
- 同じ SQL テスト群を両方の実装に流す (GlueSQL の test-suite 方式)。
- 迷う場合はトレイトでなく enum (`enum StorageImpl { Memory(..), Disk(..) }`) でもよい。実装が2つで固定ならジェネリクスより単純で、コンパイルも速い。

### 3.3 Executor のトレイトとコンテキストの渡し方

**推奨形**
```rust
pub type BoxedExecutor = Box<dyn Executor>;

pub trait Executor {
    /// 出力スキーマ (列名と型)。RowDescription の生成に使う
    fn schema(&self) -> &OutputSchema;
    /// 1行取り出す。None で終端
    fn next(&mut self, ctx: &mut ExecCtx<'_>) -> Result<Option<Row>>;
    /// NLJ の内側などで巻き戻す。既定は未サポートエラー
    fn rewind(&mut self, ctx: &mut ExecCtx<'_>) -> Result<()> { Err(Error::internal("rewind unsupported")) }
}

pub struct ExecCtx<'a> {
    pub db: &'a Database,              // buffer pool, storage, wal, txn manager (内部は Arc/Mutex)
    pub txn: &'a mut Transaction,      // 書き込み集合, コマンドID, 状態
    pub snapshot: &'a Snapshot,        // 文 (Read Committed) またはトランザクション (Repeatable Read) 単位
    pub catalog: &'a dyn CatalogReader,// 文開始時点のカタログビュー
    pub params: &'a [Value],           // $1..$n
}
```

**このシグネチャを選ぶ理由**
1. **`ctx` を構造体に保持せず、`next` の引数で渡す**。executor 構造体が `&'a mut Transaction` を保持すると、ツリー全体にライフタイム `'a` が伝播する。さらに、親子の両方が同じ `&mut` を持てない (NLJ の左右、`INSERT ... SELECT` で読み取り側と書き込み側が同じ txn を使う) という問題にぶつかる。引数で渡せば、呼び出しの間だけ再借用され、**executor は `'static` (ライフタイムなし) の `Box<dyn Executor>` にできる**。BusTub の `ExecutorContext*` 共有に相当するものを、Rust の借用規則の範囲で表現する方法がこれ。
2. **`Box<dyn Executor>` か enum ディスパッチか**: M1 は `Box<dyn>` を推奨する。ノード種別を追加するときの変更が局所的 (ファイル1つ + builder 1か所) で済み、仮想呼び出しのコストは1行あたり数 ns で、I/O やハッシュに比べて無視できる。enum ディスパッチ (`enum ExecNode { Scan(ScanExec), .. }`) は、`match` の網羅性チェックが効く利点がある。ただ、ノードの追加ごとに巨大な match を触ることになる。性能が問題になったら、Volcano のままバッチ化 (`next_batch -> Vec<Row>`) する方が効果が大きい (BusTub もバッチ版に移行した)。
3. **標準の `Iterator` を使わない理由**: toydb のように `Iterator<Item = Result<Row>>` にすると、アダプタの合成で簡潔に書ける。しかし `Iterator::next(&mut self)` には外部コンテキストを渡せないので、txn やバッファプールをイテレータに捕獲させる必要が出る (toydb では行をバッファしてから返すことで回避している)。ディスクのヒープと B+Tree を扱う yuzhu では、**独自トレイト + コンテキスト引数**の方が素直。
4. **プランと実行状態を分ける**。PostgreSQL の Plan と PlanState、BusTub の PlanNode と Executor に倣う。`PhysicalPlan` は不変の enum とし、`executor::build(&PhysicalPlan, &ctx) -> BoxedExecutor` で状態を持つ executor ツリーを作る。prepared statement ではプランを保持して、`execute` のたびに build する。
5. **DML の executor**: `Insert`/`Update`/`Delete` も `next()` で子から行を引き、`ctx.txn` に書き込む。返り値は `RETURNING` があれば行、なければ件数のカウンタ。**Halloween 問題** (UPDATE が自分の更新した行を再び読む) は、MVCC のコマンド ID (`cmin`/`cmax`。PostgreSQL の `CommandId`) で解決する。読み取りスナップショットは、現在のコマンド ID より前の書き込みしか見ない。M1 から `Transaction { id, command_id }` を持たせておく。
6. **DDL は executor ツリーに入れず**、`session` 側で直接処理してもよい (toydb の `Plan::CreateTable`、PostgreSQL の utility 文が ProcessUtility で別経路になっているのと同じ)。

**コンテキストの流れ (1文)**
```
Session::execute(sql)
  ├─ sql::parse(sql)                       -> Vec<ast::Statement>      (カタログ不要)
  ├─ txn = self.current_txn or 暗黙に begin
  ├─ catalog_view = db.catalog().reader(&txn, &snapshot)
  ├─ binder::bind(&stmt, &catalog_view)    -> BoundStatement           (型と名前を確定)
  ├─ planner::plan(bound, &catalog_view)   -> PhysicalPlan
  ├─ exec = executor::build(&plan)
  ├─ let mut ctx = ExecCtx { db, txn: &mut txn, snapshot, catalog, params }
  ├─ while let Some(row) = exec.next(&mut ctx)? { sink.send(row) }  // sink = pgwire DataRow
  └─ 暗黙トランザクションなら commit (WAL の flush を待ってから応答)
```

### 3.4 値と行の表現

- `enum Value { Null, Bool(bool), Int2(i16), Int4(i32), Int8(i64), Float8(f64), Text(String), /* M3+: Numeric, Date, Timestamp, Bytea */ }`
  - PostgreSQL 互換のため、整数の幅を区別する (`int4 + int4 → int4`、オーバーフローは SQLSTATE 22003 のエラー)。toydb のように Integer 1種に寄せると、互換性テストで苦労する。
  - **型は `Value` ではなく、`BoundExpr.ty` と `OutputSchema` が持つ**。`Value::Null` は型を持たないので、型付きの NULL は式の型情報で表す。
  - 浮動小数点の比較と Hash は、PostgreSQL と同じく NaN を最大値、-0 = +0 として全順序の newtype を作る (ソート、ハッシュ結合、GROUP BY に必要)。
- `struct Row(Vec<Value>)`。M1 はこれで十分。M2 でディスクのタプルを読むときは、ガードを手放す前にデコードして `Row` を作る (所有権を持つコピー)。ゼロコピー (Turso の `ValueRef<'a>`、PostgreSQL の Slot) は最適化フェーズまで見送る。ガードの寿命と行の寿命が結びつき、借用地獄になるため。
- 式: binder の段階で列参照を `ColumnRef { rel_idx, col_idx }` に解決し、planner が物理化するときに「入力行のオフセット」に変換する (DataFusion の Column(name) → PhysicalExpr Column(index) と同じ)。

### 3.5 カタログの抽象

- binder と planner には `trait CatalogReader { fn table(&self, name: &QualifiedName) -> Result<Option<Arc<TableDef>>>; fn table_by_id(..); fn indexes_of(..); fn function(..)/operator(..) }` だけを見せる。
- M1: `MemoryCatalog` (`RwLock<HashMap<..>>`)。DDL はコミット時に反映する。M1 ではロックを取って即時反映し、ロールバックは未対応、としてもよい。
- M2 以降: **システムテーブル (ヒープ) に格納し、トランザクショナルにする** (PostgreSQL の pg_class/pg_attribute、toydb の `Transaction: Catalog` と同じ考え方)。読み取りはスナップショットで可視性を判定し、キャッシュ (`Arc<TableDef>`) は DDL のコミット時に無効化する。トレイトで抽象化しておけば、M1 から M2 で binder を変えずに済む。
- `TableDef` は `Arc` で共有し、不変にする。文の実行中にカタログが変わっても、実行中の文は自分の `Arc` を持ち続ける。DDL と DML の並行は M4 以降にロック (テーブル単位の共有/排他) で制御する。

### 3.6 エラー型の設計

```rust
pub struct Error { kind: ErrorKind, sqlstate: SqlState, message: String, detail: Option<String>, hint: Option<String>, position: Option<usize> }
pub enum ErrorKind { Syntax, Undefined, TypeMismatch, Constraint, Serialization, ReadOnly, Io, Corrupted, Internal, NotSupported }
pub struct SqlState([u8; 5]);  // 定数: SYNTAX_ERROR = "42601", UNDEFINED_TABLE = "42P01", UNIQUE_VIOLATION = "23505", SERIALIZATION_FAILURE = "40001", ...
pub type Result<T> = std::result::Result<T, Error>;
```
- **crate 全体で単一の `Error`** にする (toydb、DataFusion 方式)。モジュール別の enum (RisingLight 方式) は、境界ごとに変換が要って煩雑になる。ヘルパ関数 (`Error::syntax(pos, msg)`、`Error::undefined_table(name)`) か、マクロで生成する。
- pgwire の ErrorResponse (S/C/M/D/H/P フィールド) にそのまま写像できる形にしておく。
- `std::io::Error` は `From` で `Io` に変換する。`Corrupted` (チェックサム不一致) と `Internal` (不変条件違反) は `XX001`/`XX000` に対応する。**`panic!` や `unwrap` は不変条件違反に限り、ユーザ入力由来の異常は必ず `Error` で返す** (1接続の panic でサーバ全体を落とさない。スレッドごとに `catch_unwind` で接続だけを切る保険も入れる)。
- 外部依存を減らす方針なら、`thiserror` なしで手書きの `Display` で十分。

### 3.7 テスト戦略

1. **sqllogictest 形式 (`.slt`)** を主軸にする。`statement ok`、`query IT rowsort` などの書式。M1 のメモリ実装と M2 のディスク実装に同じファイルを流す。言語非依存なので、将来の他言語実装でも再利用できる。Rust の sqllogictest ランナーは小さいので自作も容易 (外部クレート `sqllogictest` を dev-dependency にするのも可)。
2. **golden (スナップショット) テスト**: パーサ (SQL → AST の Debug 出力)、binder、planner (EXPLAIN 出力) を `tests/golden/*.sql` と `*.out` で比較する (toydb の goldenscript、PostgreSQL の pg_regress と同じ考え方)。`UPDATE_GOLDEN=1` で期待値を更新できるようにする。
3. **PostgreSQL との差分テスト**: 同じ `.slt` を本物の PostgreSQL に流して期待値を作る (互換性を確認する最強の手段)。
4. **ユニットテスト**: page、btree、wal は `#[cfg(test)]` の性質テスト。proptest か自作の乱数で、B+Tree を `BTreeMap` と突き合わせる。
5. **クラッシュリカバリ (M2+)**: `DiskManager` をトレイト化してフォールト注入を可能にし、「WAL の任意の位置で切断 → 再生 → 不変条件を確認」を回す (Turso の simulator を小さくした版)。
6. **並行性 (M4+)**: PostgreSQL の isolation tester 風に、複数セッションの操作順序をスクリプトで記述する。

---

## 4. Rust の所有権とバッファプール: unsafe なしでの設計と落とし穴

### 4.1 推奨構造

```rust
pub struct BufferPool {
    frames: Vec<Arc<Frame>>,                  // 固定サイズ, 起動時に確保
    inner: Mutex<PoolInner>,                  // page_table: HashMap<PageId, FrameId>, free list, replacer
    disk: Arc<DiskManager>,
    wal: Arc<WalWriter>,                      // M2: 追い出し時に page_lsn まで flush (WAL の原則)
}
pub struct Frame {
    id: FrameId,
    pin_count: AtomicU32,
    meta: Mutex<FrameMeta>,                   // page_id, dirty
    data: RwLock<PageBuf>,                    // PageBuf = Box<[u8; PAGE_SIZE]>
}
pub struct ReadGuard  { frame: Arc<Frame>, pool: Arc<BufferPool>, guard: ??? }   // 問題あり (後述)
```

**問題: `RwLockReadGuard<'a, PageBuf>` は `&'a RwLock` を借用している**ので、同じ構造体に `Arc<Frame>` と、その Arc から借用したガードを同居させられない (自己参照構造体)。unsafe なしで解く方法は3つある。

1. **クロージャ方式 (最も単純で推奨)**
   ```rust
   impl BufferPool {
       pub fn with_page<R>(&self, pid: PageId, f: impl FnOnce(&Page) -> Result<R>) -> Result<R>;
       pub fn with_page_mut<R>(&self, pid: PageId, f: impl FnOnce(&mut Page) -> Result<R>) -> Result<R>;
   }
   ```
   pin → ロック → `f` → アンロック → unpin を内部で完結させる。ガードはスコープの外に出ない。B+Tree の lock coupling (親を持ったまま子を取る) は、クロージャのネストで書ける。M2 から M3 はこれで十分。
2. **ピンガードとラッチを分ける (PostgreSQL の pin/lock 分離)**: `PinGuard { frame: Arc<Frame>, pool: Arc<BufferPool> }` は `'static` で、Drop 時に unpin する。中身へのアクセスは `pin.read() -> RwLockReadGuard<'_, PageBuf>` で、**PinGuard を借用する短命のガード**として取る。PinGuard はカーソルに長期間保持できる (ページを追い出させない)。ラッチは `next()` の中でだけ取る。自己参照にならない。
3. `parking_lot` の `arc_lock` 機能による `ArcRwLockReadGuard` (Arc を所有するガード) を使う方法。unsafe は parking_lot 内部に閉じており、yuzhu 側の `forbid(unsafe_code)` には抵触しない。外部依存を許すなら最も BusTub に近い形で書ける。標準ライブラリだけで済ませるなら 1 か 2 を選ぶ。

推奨は 1 を基本とし、heap scan や B+Tree カーソルのように「位置を覚えたまま次へ」が必要な箇所だけ 2 を使う。

### 4.2 落とし穴

- **プール全体を `Arc<RwLock<BufferPool>>` にしない**。全ページアクセスが1本のロックで直列化され、さらに `with_page` の中で別ページを取ろうとしてデッドロックする。ロックの粒度は「page_table の Mutex (短時間だけ持つ)」と「フレームごとの RwLock」の2段にする。
- **page_table の Mutex を持ったままディスク I/O をしない**。追い出しと読み込みの間は、フレームを「I/O 中」状態にしてから Mutex を外す。単純さを優先するなら、M2 は Mutex を持ったまま I/O してもよいが、そう決めたことを TODO として明記する。
- **ロック順序を固定する**: page_table Mutex → フレームの meta → フレームの data RwLock。逆順は禁止する。B+Tree は親 → 子、左 → 右の順で取る。`std::sync::RwLock` は同じスレッドから再帰的に取るとデッドロックする (再入不可)。
- **`Rc`/`RefCell` を使わない**: thread-per-connection なので、共有データは `Send + Sync` でなければならない。`RefCell` の実行時借用エラー (panic) は、デッドロックよりも原因を追いにくい。
- **Executor にガードを持たせたまま `next()` から戻らない**。ラッチを持ったまま上位に戻ると、上位の executor (例: Insert) が同じページの書き込みラッチを取ろうとして自己デッドロックする。heap scan は「`(PageId, SlotId)` の位置だけ覚え、`next()` のたびに pin + ラッチ → 1行デコード → 解放」にする。ページあたりの行をまとめてバッファする (ページ単位でデコードして `VecDeque<Row>` に入れる) と、ラッチの回数も減る。
- **ピンが漏れるとバッファプールが枯渇する**。すべて RAII (Drop) で unpin し、手動の `unpin()` API は公開しない。テストでは、クエリ終了後に `pool.pinned_count() == 0` を assert する。
- **WAL の原則**: dirty ページを追い出す前に、`page_lsn` までの WAL を fsync する。ページヘッダに LSN を持たせることは M2 の最初から決めておく (REDO-only なら、REDO 時に `page_lsn >= record_lsn` のレコードをスキップする判定に使う)。REDO-only (no-steal) にするなら、**未コミットの変更を含むページを追い出さない**制約が必要になる。no-steal にするには、ピンかフラグで追い出しを防ぐ。プールが小さいと大きなトランザクションで詰まるので、仕様として上限を明記する。もう一つの選択肢は、PostgreSQL と同じく steal を許し、xmin/xmax と clog で未コミットのタプルを不可視にすること。この場合、UNDO なしで REDO-only が成立する (PostgreSQL がまさにこの方式)。**xmin/xmax の MVCC を採用するなら後者の方が自然で、no-steal 制約は不要**。この点は設計判断として明確にしておくべき。
- **カタログの `Arc<TableDef>` とバッファプールを循環参照させない** (`Database` → `Arc` で各コンポーネント。子から親へは引数で渡すか `Weak`)。
- **`&mut Transaction` とストレージの分離**: `Transaction` には ID、スナップショット、書き込み集合、コマンド ID だけを持たせ、ストレージへの参照は持たせない。ストレージ操作は `db.storage().insert(&mut txn, ...)` の形にする。こうすると `ExecCtx` 内で `db: &Database` と `txn: &mut Transaction` が独立して借用できる。
- **`Session` は 1 スレッドが所有する** (`!Sync` でよい)。`Database` は `Arc<Database>` で全スレッドが共有し、内部可変性は各コンポーネントの中に閉じる。
- **長い文の実行中のカタログ変更**: 文の開始時に `Arc<TableDef>` を確保し、DDL はテーブルロックで排他にする (M4)。M1 から M3 は「DDL はグローバル Mutex を取る」でよい。

---

## 5. マイルストーン別に何を入れるか

| モジュール | M1 (メモリ) | M2 (永続化) | M3 以降 |
|---|---|---|---|
| sql | lexer, parser, ast (Span 付き) | - | 構文の拡張 |
| binder | 名前と型の解決, $n パラメータ | - | サブクエリ, CTE |
| planner | Bound → Logical → Physical (ほぼ 1:1), 定数畳み込み | - | 述語押し下げ, インデックス選択, 結合順序 |
| executor | Scan, Filter, Project, NLJ, Agg, Sort, Limit, Values, DML | HeapScan カーソル | HashJoin, IndexScan |
| catalog | MemoryCatalog (trait 越し) | システムテーブル化 | トランザクショナル DDL |
| txn | TxnId, Snapshot, command_id, 可視性判定 | clog の永続化 | 分離レベル, SSI など |
| storage | memory 実装 (xmin/xmax 付き) | disk, page, buffer, heap | btree, VACUUM |
| wal | - | record, writer, recovery | checkpoint |

---

## 6. 参考リンク一覧 (主要なもの)

- toydb: https://github.com/erikgrinaker/toydb (アーキテクチャ解説 `docs/architecture/` もある)
- RisingLight: https://github.com/risinglightdb/risinglight
- GlueSQL: https://github.com/gluesql/gluesql
- Turso (旧 Limbo): https://github.com/tursodatabase/turso
- DataFusion: https://github.com/apache/datafusion
- sqlparser-rs: https://github.com/apache/datafusion-sqlparser-rs
- BusTub: https://github.com/cmu-db/bustub (CMU 15-445 の講義資料も参考になる)
- PostgreSQL: https://github.com/postgres/postgres (README: `src/backend/optimizer/README`, `src/backend/access/heap/README.tuplock`, `src/backend/storage/buffer/README`, `src/backend/access/transam/README`)
