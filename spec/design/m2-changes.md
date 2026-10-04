# M2 契約からの変更記録

`m2.md` 第 4 節の契約から変えた点・足した点（担当 A）。

- `types::io`: 契約の `output_text_with(d, ty, regproc_name)` は、M1 の `output_text_with(d, ty, &OutputOpts)` と衝突するため、別名の `output_text_regproc(d, ty, opts, regproc_name)` として追加した。
- `catalog::fake::FakeCatalog`（`#[cfg(test)]`）を追加。M1 の `MemoryCatalog` の代わりに analyzer / planner / executor の単体テストが使う。F の `StatementCatalog` ができたら置き換えてよい。
- `storage::heap::scan::HeapScan::new` と、`#[cfg(test)]` の `from_tuples` / `pop_buffered` を追加（偽の `TableStore` 用）。
- `storage/mod.rs` に第 3.10 節の定数を置いた（`CATALOG_VERSION_NO`、`XID_PREFETCH` なども）。
- `analyzer::UpdateSource` は analyzer に置き、`planner::plan` から再エクスポートする（依存の向きのため）。
- `BoundFrom::Table` に `system_columns` を足した。
- `buffer::PoisonFlag` と `BufferPool::poison_flag()` を追加（`CriticalSection` が共有する）。
