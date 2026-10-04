# M1 契約（`m1.md`）からの変更点

`m1.md` 第 3 節のシグネチャに対して、実装中に加えた変更と追加を記録します。

## 基盤（yuzhu-core の共通型）

| # | 対象 | 変更 | 理由 |
|---|---|---|---|
| C-01 | `error::Error` | フィールド `cursor_byte: Option<u32>` と、`with_span(Span)`、`syntax_at(Span, msg)`、`resolve_position(&str)`、`with_severity` を追加した | パーサとアナライザはクエリ文字列を持たず、バイトオフセット（`Span`）しか知らない。バイト位置を `cursor_byte` に入れておき、session が `resolve_position(sql)` で 1 始まりの文字位置（`position`）に一括変換する。`with_span` は最初に設定された位置（最も内側）を優先する |
| C-02 | `error::Span` | `error.rs` に定義し、`crate::Span` と `sql::ast::Span` から再エクスポートした | sql と analyzer の両方から使うため、最下層に置いた |
| C-03 | `CatalogReader` | `casts_from` の代わりに `find_cast(source, target) -> Option<&'static BuiltinCast>` を採用した（契約で許されている代替形）。組み込みの検索（`type_by_*`、`find_cast`、`operators_named`、`functions_named`）には `catalog::builtin` の静的表を引く既定実装を付けた | 静的配列からソース型ごとのスライスを返すには並べ替えが必要になる。既定実装により、実装側は `table`、`table_by_oid`、`current_database` だけを書けばよい |
| C-04 | `BuiltinType` | `typtype`（`b`/`p`）と `array_oid` を追加し、`input`/`output` は関数名の文字列（`"int4in"` など）にした | 実際の変換は `types::io` が担う。M2 で `pg_type` に書き出すときに必要な列を持たせた |
| C-05 | `BoundExprKind::Cast` | `target` フィールドを持たない。変換先はノードの `ty` が表す。typmod（`varchar(n)` の長さ）は別ノード `CoerceTypmod { expr, explicit }` で適用する | 型を二重に持つと食い違いの原因になる。設計 5.2 の「typmod は別の段階で適用」をそのまま木の形にした |
| C-06 | `executor::SessionInfo` | `ExecCtx.session` の型を `executor` モジュールに定義した（`current_user`、`session_user`、`database`、`current_schema`） | 依存の方向（executor は session を `use` しない）を守るため |
| C-07 | `Executor` | 既定メソッド `rows_affected(&self) -> u64` を追加した | INSERT は行を返さないので、session が `INSERT 0 n` のタグを作るために件数を取り出す手段が必要 |
| C-08 | `BoundStatement::Select` | `Box<BoundSelect>` にした | clippy の `large_enum_variant` を避けるため |
| C-09 | `Datum` | `PartialEq` も derive した | テストと AST/Bound の比較のため。NaN は `!=`（Rust の意味）なので、SQL の比較には必ず `cmp_datum` を使う |
| C-10 | `Session::new` | 存在しない DB 名のエラーは `Severity::Fatal` を付けて返す | PostgreSQL は起動時のこのエラーを FATAL で送る |
| C-11 | `types::io::input_text` | `typmod` は見ない（`varchar(n)` の長さ検査をしない）。長さの適用は `types::ops::varchar_coerce(s, typmod, is_explicit)` で行う | PostgreSQL も unknown リテラルの変換では typmod -1 で入力関数を呼び、長さの強制は後段で行う。明示キャストの切り詰めと代入時の 22001 を一か所で扱える |
| C-12 | `sql::ast` | DEFAULT と CHECK の式は `SourceExpr { expr, text }` として、ソース上の文字列も持つ | カタログには SQL テキストで保存する（3.3）が、アナライザはクエリ文字列を受け取らない。パーサが該当部分をそのまま切り出して入れる |
| C-13 | `sql` | `parse_expr(sql) -> Result<Expr>` を追加した | カタログに保存した DEFAULT/CHECK のテキストを再パースするため |
| C-14 | `catalog::memory::MemoryCatalog`、`storage::memory::MemoryTableStore` | 基盤の段階で最小限の実装を置いた（担当者が拡張してよい） | 各担当が互いの完成を待たずにテストを書けるようにするため |

## 型の入出力（検証結果）

- float の出力は PostgreSQL 17 の Ryu 移植（`src/common/d2s.c`、`f2s.c`）と**完全に一致させた**。Rust 標準の最短表現とは次の 2 点が異なるため、独自に実装した（`types/float_fmt.rs`）。
  - PostgreSQL は `STRICTLY_SHORTEST = 0`（`acceptBounds = false`）でビルドしており、丸め区間の両端を含めない。そのため最短でない桁数になることがある（例: `-6.8659681752636816e+16`、float4 の `1.21636864e+08`）。
  - 小数部のない整数（float8 は 2^53 未満、float4 は 2^24 未満）は正確な整数値を出力する（`d2d_small_int`）。ちょうど中間の値は偶数へ丸める（`181705388398854.62`）。
- 指数表記に切り替わる境界: float8 は 10 進指数が `-4 ≤ e < 15` なら固定小数点（`1e14` → `100000000000000`、`1e15` → `1e+15`）。float4 は `-4 ≤ e < 6`（`123456` → `123456`、`1e6` → `1e+06`）。指数は最低 2 桁（`1e-05`）。
- 本物の PostgreSQL 17 で生成した約 3.8 万個の値（ランダム値、2 の冪、非正規化数、境界値）で一致を確認した。
- C の `strtod` が受け付ける 16 進の浮動小数点数（`'0x10'::float8`）には対応していない。

## 調査で見つかった設計書の誤りの修正（m3-tx-semantics.md より）

- **C-15 Query の途中の BEGIN**: 区切りにはならず、それより前の文も同じトランザクションに取り込む（第 4.1 節を修正）。PostgreSQL 17 の実機で確認済み。
- **C-16 ParameterStatus を送る時機**: CommandComplete の前ではなく、Query の処理の最後（ReadyForQuery の直前）に、値が変わった項目だけを送る（第 5.4 節を修正）。
