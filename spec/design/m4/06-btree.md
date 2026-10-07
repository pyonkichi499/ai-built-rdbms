# yuzhu M4 設計 06: B+Tree とインデックス

M4 設計書の第 06 章（担当 B1・B2・H4）です。`00-contracts.md` の契約（特に §11.3 の opclass の表、§11.4 の `IndexHandle`、§13.1〜§13.5 の `TableStore` の追加・`IndexStore`・WAL・B+Tree のディスク形式、§15.1 の定数、§19 の M5 への予約）に従い、**B+Tree の実装者がこの章だけ読めば実装できる**粒度で、ページとタプルのバイト形式、挿入・分割・一意性検査・一括構築・スキャン・REDO・検査器の手順、比較関数と opclass の静的な表、ヒープへの追加（H4）を決めます。

- 要件定義: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- 前提（必読）: `00-contracts.md`、`spec/design/m2.md` §3.3〜§3.5・§6.3〜§6.6、`spec/design/m3.md` §3・§4.2〜§4.5・§5.1・§6.3〜§6.5、`QUESTIONS.md`
- 調査（根拠）: `spec/research/m4-btree.md`（以下「調査」）
- 正解の基準は **PostgreSQL 17**。PostgreSQL のソースは REL_17_STABLE を `PG:<path>` と略す。実機（17.11、`pageinspect` つき）で確かめたものには「（実測）」、確かめていないものには「（未検証）」と付ける。
- 他の章への参照は章のファイル名と概念名で書く（`07-catalog-ddl.md` の `build_from_heap` など）。
- 実装の入口: B1 は §5.2〜§5.7・§5.11・§6、B2 は §5.4・§5.8・§5.9・§6.2・§4.7、H4 は §4.2・§4.4・§5.12。

---

## 1. 範囲

### 1.1 この章が決めるもの

| 分類 | 内容 | 担当 |
|---|---|---|
| ディスク形式 | B+Tree のメタページ・葉・内部ページ、インデックスタプル、ピボット、high key、`BTREE_INSERT_LEAF` と `BTREE_PAGES` の WAL レコード（00 §13.5 の詳細化） | B1 |
| 木の操作 | 探索（右へ移動）、挿入、一意性検査、ページ分割と親への伝播、ルート分割、`init_index` | B1、B2（一意性検査） |
| 構築 | `IndexStore::build`（一括構築）、構築用のソート補助（`cmp_keys`） | B2 |
| スキャン | 前向き・後ろ向きスキャン、`ResolvedScanKeys` の意味（演算子ごとの境界、NULL、先頭列以外の範囲） | B2 |
| REDO | `BTREE_*` の REDO、`describe`（waldump 用） | B1 |
| 検査器 | 木の構造（I14）とヒープとの突き合わせ（I13）を検査する `check.rs` | B2 |
| 比較と opclass | 比較関数、`catalog/opclass.rs` の静的な表（全行）、`pg_proc` に要る比較関数の一覧 | B2 |
| ヒープへの追加 | `begin_scan_all`・`tuple_state`・`fetch_dirty`・`nblocks`、列の符号化の公開、`page.rs` の追加アクセサ | H4 |
| M3 の部品への依頼 | バッファプールに B+Tree 用のラッチ・拡張の口を足す（§4.3） | M3 の C |

### 1.2 この章が決めないもの

- カタログの行（`pg_index` ほか）、`CREATE INDEX` / `DROP INDEX` / PRIMARY KEY / UNIQUE の実行、`TRUNCATE`: `07-catalog-ddl.md`。この章は `IndexStore` の呼び出し口だけを提供する。
- `UniqueCheck` を渡す側（`insert_with_indexes`）と `IndexScan` ノード: `05-executor.md`。
- インデックス選択と `ResolvedScanKeys` を作る側: `04-planner-optimizer.md`。
- シーケンス: `08-sequence-serial.md`。numeric・日時・char(n) の符号化と比較の本体: `09-types-functions.md`。

### 1.3 M4 で作らないもの（ディスク形式は壊さずに予約する。00 §19）

項目の削除（VACUUM・LP_DEAD・「簡易削除」）、ページの削除・再利用、suffix truncation、重複排除（posting list）、`INCLUDE` 列、式・部分インデックス、Index Only Scan、Bitmap Scan、`btpo_cycleid`、fast root（`fastroot` は `root` と同じ値）、複数ライターの下での親の探し直し、外部ソート。

---

## 2. 決定

### 2.0 前提と、00 の決定のうちこの章が実現するもの

- **前提（M2・M3 から）**: 書き込むトランザクションは同時に 1 つ（単一ライターロック。M2 §6.7.1）なので、B+Tree を**書き換えるのは常に 1 人**で、読み手（複数）は並行して走る。ヒープは行ポインタを再利用せず（M2 §3.4）、ヒントビットを立てない（M3 D7）。WAL は REDO のみで、ページの変更は M3 規約 1 の形で書く（M3 §2）。M5 の複数ライターで変わる箇所（待ち、親の探し直し、結合ラッチ）は、M4 でも形だけ用意して印を付ける（§6.3）。
- 00 の決定との対応:

| 00 の決定 | この章での実現 |
|---|---|
| D-7（nbtree に準拠。suffix truncation・重複排除・項目の削除・ページ削除なし。ルートは CREATE INDEX の時点で作る。構造変更は全画像の 1 レコード `BTREE_PAGES`、通常の挿入は差分の `BTREE_INSERT_LEAF`） | §3（形式）、§5.3〜§5.7（挿入・分割・初期化）、D6-1〜D6-7、D6-11 |
| D-8（PRIMARY KEY / UNIQUE は一意インデックス。検査は行ごとに即時。`WaitFor` を返せる形で、M4 では起きない） | §5.4（一意性検査）、§4.4（`fetch_dirty`）、D6-12 |
| D-18（B+Tree の後ろ向きスキャンはインデックス順のソート省略のためにあり、プランナが使わない間も単体テストで検証する） | §5.9（`walk_left`）、§7.8、§7.10、D6-9 |
| D-19（スピルしない。CREATE INDEX のソートは予算の対象外） | §5.8（`build` は整列済みの入力を受け取る。ソートと全件のメモリは 07 の `build_from_heap`）、[06-Q15] |

### 2.1 調査と 00 の食い違い、調査どうしの食い違い

| # | 論点 | 調査（m4-btree） | 00 / 他の章 | 決定 |
|---|---|---|---|---|
| 1 | 空のインデックスのルート | メタの `root = 0`、最初の挿入で遅延作成（§2.2） | D-7: ルート（葉）を CREATE INDEX の時点で作る。ブロック 1 が最初のルート（§13.5）。07 の `EMPTY_INDEX_STATS` は `pages: 2` | **00 に従う**（[06-Q2]）。`init_index` が「メタ + 空のルート葉」を `BTREE_PAGES` で書く。遅延作成の分岐（挿入の途中でルートを作る処理）と、その REDO が要らない |
| 2 | WAL レコードの種類 | `BTREE_INSERT_LEAF` / `SPLIT_CHAIN` / `NEWPAGES` / `META_INIT` の 4 種（§5.1） | `BTREE_INSERT_LEAF`（0x00）と `BTREE_PAGES`（0x10、理由は main data の先頭 1 バイト）の 2 種（§13.4） | **00 に従う**。理由: `SPLIT_CHAIN`・`NEWPAGES`・`META_INIT` は「ブロックの全画像を置くだけ」で REDO が同一。理由の値は §3.7 |
| 3 | 分割の連鎖で要るブロック数 | 各レベルで左・右・右隣、+ 新ルート + メタで「3h + 2」。h ≤ 10 まで（§5.1） | 「2h + 3」、h > 14 で `54000`（§13.4） | **どちらも不正確**。各レベルで左・右・元の右隣の 3 ページ、最上位がルートなら右隣はなく新ルートとメタが加わるので、最悪は **3h + 1**（h = 木のレベル数）。静的な高さの上限は作らず、**必要なブロック数が `MAX_BLOCK_REFS`（32）を超えたときだけ `54000`**（[06-Q3]）。00 の記述の訂正を §11 に出す |
| 4 | ピボットの境界の向き | 「左ページの全項目 < high key ≤ 右ページの全項目」（§2.4） | 00 は向きを書かない | **調査どおり**（左の全項目 < 区切り ≤ 右の全項目）。PostgreSQL の nbtree は逆（区切りは左の非厳密な上限、右の厳密な下限。切り詰めたピボットの TID を 1 つ減らして保つ）。M4 は切り詰めないので PostgreSQL の「TID を減らす」操作が要らず、区切り = 右の最初の項目そのもの、で足りる。M5 で suffix truncation を入れても矛盾しない（§2.2 D6-1）（[06-Q1] ★） |
| 5 | 比較関数の形 | 列ごとに `fn(&[u8], &[u8]) -> Ordering`（バイト列を直接比べる）（§3.1） | `CmpFn = fn(&Datum, &Datum) -> Ordering`（§11.3） | **00 に従う**。タプルを `Datum` に復号してから比べる。文字列のコピーが発生するが M4 は性能を目標にしない（[06-Q18]）。比較の本体は `cmp_datum` に一本化されており、型ごとの規則を B+Tree に持ち込まない（00 §4.3 の 2） |
| 6 | 一意性検査の戻り値 | `Visible { in_progress_xid }` / `Conflict::WaitFor` を B+Tree が受ける（§6.1） | `DirtyResult::{Invisible, Visible, WaitFor(Xid)}`（§13.1）。M4 では `WaitFor` は起きず内部エラー | **00 に従う**（§5.4） |
| 7 | 一括構築で一意性検査の対象外の版を別に持つ | `btspool2` と同じ役割をどこが持つかは未定（§7.1） | 07 の D07-7: C1 が生きている版だけを調べ、`build` には `BuildUnique::No` | **07 に従う**。`BuildUnique::Yes` は「入力のすべてが生きている版」として隣接比較する簡易版で、C1 は使わない（テストと将来の経路用。§5.8） |
| 8 | 検査器の置き場所 | `yuzhu-storage` のテスト用関数 | `yuzhu-core/src/storage/btree/check.rs`（§18） | **00 に従う** |
| 9 | datetime の型をまたぐ比較 | `datetime_ops` に型をまたぐ演算子・関数を入れる（§3.2） | 09 の D-9-5: 型をまたぐ日時の比較演算子は作らない | **09 に従う**。`datetime_ops` は date・timestamp・timestamptz の**同じ型どうしだけ**。PostgreSQL の `pg_amop` より 30 行少ない（§4.7） |

### 2.2 この章で追加で決めたこと

| # | 論点 | 選択肢 | 決定 | 理由 |
|---|---|---|---|---|
| D6-1 | 区切りとピボットの境界の向き | (A) PostgreSQL と同じ（区切りは左の非厳密な上限）／(B) 区切り = 右の最初の項目（左の全項目 < 区切り ≤ 右の全項目） | **(B)**。internal ページの項目 `P_i` が支配する範囲は `[P_i, P_{i+1})`。探索キー `K` は「`P_i <= K` となる最後の `i`」の子へ降りる。high key `H` は「左ページの全項目はそれより厳密に小さい」上限で、`K >= H` なら右へ移る | (A) は切り詰めない完全なピボットでも「TID の最下位を 1 引く」特例が要る（TID の offset が 0 になりうる）。(B) は区切りが右の最初の項目の完全な複製で済み、検査器の不変条件も単純（§6.2）。切り詰めを入れた後も「切り詰めた属性は -∞」の規則だけで成り立つ |
| D6-2 | ピボットの形 | 切り詰める／切り詰めない | **切り詰めない**（D-7）。完全なピボット = 右の最初の葉タプルの複製 + 末尾にヒープ TID（6 バイト、`size = S + 8`）。内部ページの最初のデータ項目だけは属性 0 個の -∞ ピボット（8 バイト） | ピボットの大きさが `葉タプルの大きさ + 8` と定まり、「1 ページに最低 3 項目」の保証（`BT_MAX_ITEM_SIZE`）がそのまま成り立つ（§3.8） |
| D6-3 | high key の `t_tid.block` | 0／子のブロック番号 | **0**（使わない。検査器が 0 であることを確かめる） | バイト形式が決定的になりテストしやすい |
| D6-4 | 分割点 | PostgreSQL の `_bt_findsplitloc`（重複・切り詰めの考慮つき）／単純な規則 | **左右のバイト数が均等になる点**。ただし**右端の葉に最後の位置へ追記する挿入**のときは、左を `BT_FILLFACTOR_LEAF`（90%）まで詰める（右へ残りを送る）。内部ページと右端以外の葉は常に均等 | 連番キー（SERIAL の主キー）で木が半分空になるのを防ぐ。中ほどへ挿入するときに 90/10 で割ると右ページがほぼ空になるので、PostgreSQL（右端なら位置に関わらず fillfactor）と違い「最後の位置への追記」に限る（[06-Q5]） |
| D6-5 | `BT_MAX_ITEM_SIZE` の検査 | タプルを作った時点／ページへ入れるとき | **タプルを作った時点**（ラッチを取る前）。`MAXALIGN(タプルの大きさ) > 2704` なら `54000`。メッセージは PostgreSQL と同じ（`index row size 2712 exceeds btree version 4 maximum 2704 for index "x"`）。境界は実測（§7.2）と一致する | 検査が失敗してもページに触れない。一括構築も同じ関数を通る |
| D6-6 | 高さの上限 | 静的な上限（00: 14）／分割に要るブロック数で動的に | **動的**（§2.1 の 3）。分割の計画の段階で `3h + 1` 以下のブロック数を数え、`MAX_BLOCK_REFS` を超えるなら**何も変更せず** `54000`（`index "x" cannot be split further: too many levels`、yuzhu 独自の文言）。メタ・ページの `level` が 64 以上なら破損（`XX001`） | 実用上到達しない（int4 キーで高さ 4、最大長のキー（2692 バイト）でも 100 万件で 13 程度。全レベルが同時に満杯になる確率は無視できる） |
| D6-7 | 項目の削除 | する（LP_DEAD・簡易削除）／しない | **しない**。DELETE・UPDATE・中断したトランザクションの項目も残る。可視性はヒープで判定する | 項目の削除は WAL（`BTREE_DELETE`）とスキャンとの連動（クリーンアップロック）が要る。M5 の VACUUM と一緒に入れる。M3 のヒントビット（D7）がないので `LP_DEAD` のヒントも立てられない |
| D6-8 | スキャンのピンとラッチ | 葉にピンを持ち越す（PostgreSQL）／持ち越さない | **持ち越さない**（00 §13.2、M2 D10）。1 回の `scan_next` の中で、葉を共有ラッチして一致する項目を**まとめてコピー**し、ラッチとピンを外してから返す | ヒープの行ポインタを再利用しない（M2 §3.4）ので、古い TID が別の行を指すことがない。ピンの持ち越しが要らず、スキャンを持つ側（executor）がピンの管理をしなくてよい |
| D6-9 | 後ろ向きスキャンを M4 で実装する理由 | M5 に回す（調査 §9.1 の工数 S、プランナが使うのは M4 後半の任意項目 S）／M4 で作る | **M4 で作る**（00 D-18） | (1) 左リンク（`prev`）を M4 から全レベルで維持し検査するので、使われない経路を残すと壊れていても気づけない。(2) 「左へ移るときに分割と競合する」手順（`walk_left`）が並行処理の中で最も間違えやすく、形式を凍結する前に決定的なテストで確かめたい。(3) `ORDER BY a DESC LIMIT n` と `max()` の最適化（04 のソートの省略（任意項目）がインデックスの逆順走査で `Sort` を省ける。プランナが使わない間は単体テスト・性質テストで検証する |
| D6-10 | ラッチの取る順序 | M2 のヒープと同じ「ブロック番号の昇順」／木の順序（下 → 上、左 → 右） | **木の順序**。同じレベルでは左 → 右、レベルをまたぐときは下 → 上（子 → 親）、メタページは最後。読み手は同時に 1 ページしかラッチしない。このため M3 のバッファプールの debug 検査（昇順・`extend` はラッチなし）を B+Tree には適用しない口を足してもらう（§4.3、[06-Q7]） | PostgreSQL の README と同じ規則で、M5 の複数ライターでもデッドロックしない。昇順では「子（大きいブロック）を持ったまま親（小さいブロック）を取る」ができない |
| D6-11 | 構造変更の組み立て | ページを 1 つずつ書き換える／全ページの新しい画像をメモリで組み立ててから一括で書く（00 §4.3 の 3） | **後者**。(1) 計画（ページは変えない）→ (2) 新しいページの確保（`extend_tree`）→ (3) 画像の組み立て → (4) `CriticalSection` の中で画像をコピー・`BTREE_PAGES` を 1 本挿入・全ページに `set_lsn`。途中で失敗しうる処理を `page_mut()` の後に書かない | 未完了の分割という状態が存在しない。クラッシュ試験の分岐が増えない（調査 §5.1） |
| D6-12 | 一意性検査のラッチ | 最初の葉の排他ラッチを検査から挿入まで持つ（PostgreSQL）／検査中だけ | **検査から挿入まで持つ**（M4 は書き手が 1 人なので結果は同じだが、M5 の形に合わせる）。等値の連続が右の葉へ続くとき、右の葉は共有ラッチで読む（最初の葉の排他ラッチを持ったまま）。挿入先が最初の葉の右隣以降になるときは、最初の葉のラッチを外してから右へ移る（左 → 右の順を守る。M5 では結合ラッチにする。§5.4） | 並行する同じキーの挿入が同じ最初の葉の排他ラッチで直列になる |
| D6-13 | 比較関数 | 型ごとに関数を書く／`cmp_datum` に一本化 | **`cmp_datum` に一本化**。`AMPROCS` の `cmp` はすべて `cmp_datum`（09 の決定と同じ）。NULL と DESC・NULLS FIRST の規則は `types::cmp::cmp_with_nulls` と同じ結果になる `btree::cmp_column` で行い、**一致を単体テストで確かめる** | Sort・Unique・集約・B+Tree の順序が一致しないと、インデックス順のソート省略（04 のソートの省略（任意項目）が誤った結果を返す |
| D6-14 | 一括構築の方式 | 葉と上位のページを並行して作る（PostgreSQL の nbtsort）／**葉を全部作ってから、区切りの並びで上のレベルを作る** | **後者**。レベルごとにブロック番号が連続し、32 ページずつの `BTREE_PAGES` が常に連続したブロックになる | 構築中のページの確保（`extend`）が常に末尾への追加になる。区切りの並び（1 ページあたり 1 個）はメモリに載る |
| D6-15 | 破損の検出と SQLSTATE | PostgreSQL の `XX002`（INDEX_CORRUPTED）／`XX001` | **`XX001`**（00 §13.5）。メッセージは PostgreSQL に寄せる（`index "x" contains unexpected zero page at block 7`、HINT `Please REINDEX it.`） | `error.rs` に `XX002` がない。M4 に REINDEX もない |
| D6-16 | `fetch_dirty` / `tuple_state` で自分以外の実行中のトランザクション | 判定のために `TxnManager` の実行中一覧を渡す／単一ライターの前提で「自分以外の clog が実行中 = 中断」 | **後者**（M2 §6.5.3 の `satisfies_update` と同じ規則）。`InsertInProgress` / `DeleteInProgress` / `WaitFor` は M4 では返さない（型としては用意する） | 単一ライターロックを持つトランザクションだけがヒープを書くので、他に実行中の書き込みトランザクションはいない。M5 で実行中の判定を渡す口を足す（§4.4） |

---

## 3. ディスク上の形式

すべてリトルエンディアン。ページヘッダ（24 バイト）・行ポインタ・チェックサム・全 0 のページの扱いは M2 §3.3、§3.4 のまま変えない。列の値の符号化は M2 §3.6（と 00 §12.3 の追加分）をヒープと共有する。

### 3.1 リレーションのファイル

- 1 つのインデックスは 1 つのリレーション（`pg_class.relkind = 'i'`）で、main フォークだけを持つ。ファイルの置き場所・セグメントは表と同じ（M2 §3.1）。
- **ブロック 0 = メタページ、ブロック 1 以降 = 木のページ**。`init_index` が書いた直後は、ブロック 0（メタ）とブロック 1（空のルート葉）の 2 ブロック。ブロック番号は 0 を「なし」の意味に使える（ブロック 0 はメタで、木のページの兄弟・子にはならない）。
- 木のページは**追加するだけ**で、削除も再利用もしない（D6-7）。ブロックが足りなくなったら `BufferPool::extend` で末尾に足す。確保したが木に繋がらなかったページ（分割の途中で失敗したときなど）は全 0 のまま残る。害はない（検査器は全 0 のページを「孤児」として数える）。

### 3.2 ページ共通

ヒープページとの違いは `pd_special = 8176`（special 領域 16 バイト）だけ。

| 項目 | 値 |
|---|---|
| `pd_lower` / `pd_upper` | M2 §3.3 と同じ意味。`24 <= pd_lower <= pd_upper <= pd_special`。空の木のページは `24` / `8176` |
| `pd_special` | **8176**（`BT_SPECIAL_SIZE = 16`）。8 の倍数 |
| `pd_flags` | 0（`PD_*` は使わない） |
| `pd_pagesize_version` | `0x2001` |
| 項目に使える領域 | `8176 - 24 = 8152` バイト（`BT_PAGE_USABLE`）。1 項目の占有 = `MAXALIGN(長さ) + 4`（行ポインタ 4 バイト） |

**special 領域**（8176..8192）:

| special 内のオフセット | 型 | 名前 | 意味 |
|---|---|---|---|
| 0 | u32 | `prev` | 同じレベルの左隣のブロック。0 = なし（最左） |
| 4 | u32 | `next` | 同じレベルの右隣のブロック。0 = なし（**最右 = high key を持たない**） |
| 8 | u32 | `level` | 葉 = 0。親は子より 1 大きい。メタページは 0 |
| 12 | u16 | `flags` | 下表 |
| 14 | u16 | `cycleid` | M4 は常に 0（M5 の VACUUM 用） |

**flags**（値は PostgreSQL と同じ）:

| 名前 | 値 | M4 での扱い |
|---|---|---|
| `BTP_LEAF` | 0x0001 | 葉に立てる。`LEAF` と `level == 0` は同値 |
| `BTP_ROOT` | 0x0002 | 現在のルートに立てる（木に 1 ページだけ） |
| `BTP_DELETED` | 0x0004 | **予約**。立っていたら `XX001` |
| `BTP_META` | 0x0008 | ブロック 0 だけ |
| `BTP_HALF_DEAD` | 0x0010 | **予約**（`XX001`） |
| `BTP_SPLIT_END` | 0x0020 | **予約**（`XX001`） |
| `BTP_HAS_GARBAGE` | 0x0040 | 使わない（`XX001`） |
| `BTP_INCOMPLETE_SPLIT` | 0x0080 | **予約**（`XX001`）。M4 の分割は 1 レコードで完結するので立たない |
| `BTP_HAS_FULLXID` | 0x0100 | **予約**（`XX001`） |
| 上記以外のビット | | `XX001` |

許される組み合わせ: メタ = `META` のみ。葉 = `LEAF` または `LEAF \| ROOT`。内部 = `0` または `ROOT`（`level >= 1`）。

### 3.3 メタページ（ブロック 0）

ページヘッダの直後（オフセット 24）から 32 バイト。`pd_lower = 56`、`pd_upper = pd_special = 8176`（FPI の穴が効くように、`pd_lower` はメタデータの直後）。special の `flags = META`、`prev = next = level = 0`。

| オフセット | 型 | 名前 | M4 の値 |
|---|---|---|---|
| 24 | u32 | `magic` | `0x053162`（`BT_MAGIC`） |
| 28 | u32 | `version` | `1`（`BT_VERSION`。yuzhu 独自の系列。PostgreSQL の 4 とは別） |
| 32 | u32 | `root` | ルートのブロック番号（空のインデックスでも 1 以上） |
| 36 | u32 | `level` | ルートの `level`（葉だけの木は 0） |
| 40 | u32 | `fastroot` | **`root` と同じ値**（M4 に fast root はない） |
| 44 | u32 | `fastlevel` | `level` と同じ値 |
| 48 | u32 | `last_cleanup_num_delpages` | 0 |
| 52 | u8 | `allequalimage` | 0（重複排除を入れるまで偽） |
| 53 | `[u8; 3]` | 予約 | 0 |

読み込み時の検査（破れていたら `XX001`）: `magic`・`version`、`root != 0`、`root < nblocks`、`fastroot == root`、`fastlevel == level`、`level < 64`、special が上の組み合わせ。

### 3.4 行ポインタと項目の並び

- 行ポインタは `LP_NORMAL` だけ（`LP_DEAD` は M5。読んだら `XX001`）。**行ポインタの並びがキーの昇順**で、タプル本体は `pd_upper` から下へ、追加した順に詰める（途中への挿入は行ポインタの並びをずらすだけで、本体は動かさない）。削除がないので領域に穴はできない。
- 項目の種類:

| ページ | 行ポインタ 1 | 行ポインタ 2 以降 |
|---|---|---|
| 葉（`next != 0`、右端でない） | **high key**（完全なピボット、`block = 0`） | 葉タプル（昇順） |
| 葉（右端） | 葉タプル（昇順） | 葉タプル |
| 内部（右端でない） | **high key** | データ項目: 最初が **-∞ ピボット**（属性 0 個）、以降が完全なピボット（昇順） |
| 内部（右端） | -∞ ピボット | 完全なピボット |

- 「最初のデータ項目」の行ポインタ番号は `first_data_offset = if next != 0 { 2 } else { 1 }`（PostgreSQL の `P_FIRSTDATAKEY`）。
- **不変条件**（D6-1）: ページ内のデータ項目は `(キー列..., ヒープ TID)` の順に**厳密に**昇順（内部ページの -∞ ピボットを除く）。high key があれば、ページの全データ項目はそれより**厳密に小さい**。内部ページの項目 `P_i`（下りは子 `c_i`）について、`c_i` のサブツリーの全項目は `P_i <= x < P_{i+1}`（最後の項目は上限が親の high key、親が右端なら上限なし）。`c_i` の high key は `P_{i+1}`（同じバイト列から `block` だけ 0 にしたもの）に等しく、親の最後の項目の子の high key は親の high key に等しい。

### 3.5 インデックスタプル

```
オフセット  大きさ  内容
0           u32     t_tid.block   葉タプル: ヒープの TID のブロック番号 / ピボット: 子（ダウンリンク）のブロック番号（high key は 0）
4           u16     t_tid.offset  葉タプル: ヒープの行ポインタ番号 / ピボット: 下位 12 ビット = キー属性数、0x1000 = 末尾にヒープ TID を持つ
6           u16     t_info        bit15 = NULL を含む（0x8000）、bit14 = 可変長を含む（0x4000）、bit13 = ピボット（0x2000、ALT_TID）、bit0〜12 = タプルの大きさ
8           [u8;4]  NULL ビットマップ（NULL を含むときだけ。INDEX_MAX_KEYS = 32 ビット）。列 i が非 NULL なら バイト i/8 のビット i%8 が 1（ヒープと同じ）
8 または 16 ...     列データ（データの開始は NULL なしなら 8、NULL ありなら MAXALIGN(8 + 4) = 16）
```

- **大きさ**: `t_info` の下位 13 ビットは**タプル全体の大きさで、必ず 8 の倍数**（末尾を 0 で埋める。`MAXALIGN`）。行ポインタの `lp_len` もこの値。タプルを作る前に領域を 0 で埋めてから値を書く（読み出しは M2 §3.6 の「varlena の位置で 0 バイトなら整列の埋め草」に頼る）。
- **列データ**: ヒープと同じ符号化（M2 §3.6、00 §12.3）。各列の整列の基準は**タプルの先頭**（データの開始位置が 8 の倍数なので、ヒープの `t_hoff` と同じ関係）。varlena は 127 バイト未満なら 1 バイトヘッダ（整列なし）、以上なら 4 バイトヘッダ（4 バイト境界）。NULL の列は領域を取らない。TOAST・圧縮はない。`name` は 64 バイト固定のまま入る（PostgreSQL の `name_ops` は cstring で持つが、バイト互換は狙わない）。
- **大きさの上限**: `BT_MAX_ITEM_SIZE = 2704`（`MAXALIGN` 後の葉タプルの大きさ）。超えたら `54000`（D6-5、§5.3）。ピボットは葉タプル + 8 なので最大 2712。PostgreSQL の実測と一致する境界（§7.2）: 単独の text キーは 2692 バイトまで、`(int, text)` の text は 2688 まで、`(NULL, text)` は 2684 まで。
- **ピボット**（`t_info` の bit13 が立つ）:
  - **完全なピボット**: 葉タプルの全バイト（ヘッダの `t_tid` を除く。ビットマップ・列データ・埋め草）を複製し、**末尾に 8 バイトを足して**、その**最後の 6 バイト**に葉の `t_tid`（ヒープ TID。block u32 + offset u16）を置く。`t_info = (S + 8) \| 0x2000 \| (元の NULL・可変長のビット)`、`t_tid.block = ダウンリンク`（high key は 0）、`t_tid.offset = キー属性数 \| 0x1000`。大きさは常に `S + 8`（`S` = 元の葉タプルの大きさ）。
  - **-∞ ピボット**（内部ページの最初のデータ項目）: 8 バイトだけ。`t_tid.block = ダウンリンク`、`t_tid.offset = 0`（属性 0 個、TID なし）、`t_info = 8 \| 0x2000`。比較では内容を見ずに「どんな探索キーよりも小さい」。
  - M4 の完全なピボットのキー属性数は常にインデックスの列数（`ncols`）。属性数が少ないピボット（切り詰め）は M4 では作らない。読んだら（`0` 以外で `ncols` 未満なら）`XX001`。
- **posting list**（`BT_IS_POSTING = 0x2000` のオフセット側）や `t_info` のその他のビットは M4 では使わない（立っていたら `XX001`）。

### 3.6 例（バイト単位）

`CREATE TABLE t (a int4, b text); CREATE INDEX t_a_idx ON t (a);` のインデックス（リレーション `(1663, 5, 16390)`）。以下のページの `pd_checksum` は書き出し時に入る値で、メモリ上は 0。`pd_lsn` は例として `0x0000_0000_0100_0F20`。

**例 1: メタページ（ブロック 0）**。`0` でない部分だけ（残りは 0）:

| オフセット | 長さ | 内容 |
|---|---|---|
| 0 | 8 | `pd_lsn` = `20 0F 00 01 00 00 00 00` |
| 10 | 2 | `pd_flags` = `00 00` |
| 12 | 2 | `pd_lower = 56` = `38 00` |
| 14 | 2 | `pd_upper = 8176` = `F0 1F` |
| 16 | 2 | `pd_special = 8176` = `F0 1F` |
| 18 | 2 | `pd_pagesize_version` = `01 20` |
| 24 | 4 | `magic` = `62 31 05 00` |
| 28 | 4 | `version` = `01 00 00 00` |
| 32 | 4 | `root` = `01 00 00 00` |
| 36 | 4 | `level` = `00 00 00 00` |
| 40 | 4 | `fastroot` = `01 00 00 00` |
| 44 | 4 | `fastlevel` = `00 00 00 00` |
| 48 | 8 | `last_cleanup_num_delpages` 4 バイト + `allequalimage` と予約 4 バイト（すべて 0） |
| 8176 | 16 | special: `prev = 0`、`next = 0`、`level = 0`（各 `00 00 00 00`）、`flags = 0x0008` = `08 00`、`cycleid = 00 00` |

**例 2: 右端のルート葉（ブロック 1）に int4 キー 10、20、30（TID は (0,1)、(0,2)、(0,3)）**。タプルは 16 バイト（ヘッダ 8 + `int4` 4 + 埋め草 4）。

| オフセット | 長さ | 内容 |
|---|---|---|
| 0〜23 | 24 | ページヘッダ: `pd_lsn` = `40 0F 00 01 00 00 00 00`、`pd_lower = 36` = `24 00`、`pd_upper = 8128` = `C0 1F`、`pd_special` = `F0 1F`、版 `01 20` |
| 24 | 4 | 行ポインタ 1 = 8160 \| (1 << 15) \| (16 << 17) = `0x00209FE0` = `E0 9F 20 00` |
| 28 | 4 | 行ポインタ 2（オフセット 8144）= `D0 9F 20 00` |
| 32 | 4 | 行ポインタ 3（オフセット 8128）= `C0 9F 20 00` |
| 8128 | 16 | キー 30: `00 00 00 00` `03 00` `10 00` `1E 00 00 00` `00 00 00 00` |
| 8144 | 16 | キー 20: `00 00 00 00` `02 00` `10 00` `14 00 00 00` `00 00 00 00` |
| 8160 | 16 | キー 10: `00 00 00 00` `01 00` `10 00` `0A 00 00 00` `00 00 00 00` |
| 8176 | 16 | special: `prev = 0`、`next = 0`、`level = 0`、`flags = 0x0003`（`LEAF \| ROOT`）= `03 00`、`cycleid = 0` |

タプルのバイトの内訳: `t_tid.block`（4）、`t_tid.offset`（2）、`t_info`（2、`0x0010` = 大きさ 16）、キー（4）、埋め草（4）。

**例 3: 右端でない葉（高キーあり）**。キー 10、20、30 と high key = 完全なピボット（キー 40、ヒープ TID (0,4)）。`next = 2`。

| オフセット | 長さ | 内容 |
|---|---|---|
| 0〜23 | 24 | `pd_lower = 40` = `28 00`、`pd_upper = 8104` = `A8 1F`（ほかは例 2 と同じ形） |
| 24 | 16 | 行ポインタ 1〜4: high key（オフセット 8152、長さ 24）= `D8 9F 30 00`、キー 10（8136）= `C8 9F 20 00`、キー 20（8120）= `B8 9F 20 00`、キー 30（8104）= `A8 9F 20 00` |
| 8104 | 16 | キー 30 のタプル（例 2 と同じ） |
| 8120 | 16 | キー 20 |
| 8136 | 16 | キー 10 |
| 8152 | 24 | high key: `00 00 00 00`（`block = 0`）`01 10`（`offset = 0x1001` = 属性 1 個 \| TID あり）`18 20`（`t_info = 0x2018` = 大きさ 24 \| ピボット）`28 00 00 00`（キー 40）`00 00 00 00 00 00`（埋め草 6）`00 00 00 00 04 00`（ヒープ TID (0,4)） |
| 8176 | 16 | special: `prev = 0`、`next = 2` = `02 00 00 00`、`level = 0`、`flags = 0x0001` = `01 00`、`cycleid = 0` |

**例 4: 内部ページ（ブロック 3、`level = 1`、ルート）**。子は葉のブロック 1 と 2。2 番目の項目の区切りはキー 40・TID (0,4)。

| オフセット | 長さ | 内容 |
|---|---|---|
| 0〜23 | 24 | `pd_lower = 32` = `20 00`、`pd_upper = 8144` = `D0 1F` |
| 24 | 8 | 行ポインタ 1（-∞、オフセット 8168、長さ 8）= 8168 \| (1 << 15) \| (8 << 17) = `0x00109FE8` = `E8 9F 10 00`、行ポインタ 2（区切り、オフセット 8144、長さ 24）= `0x00309FD0` = `D0 9F 30 00` |
| 8144 | 24 | 区切り: `02 00 00 00`（`block = 2` = 子）`01 10`（キー属性 1 個 \| TID あり）`18 20`（`t_info = 0x2018`）`28 00 00 00`（キー 40）`00 00 00 00 00 00`（埋め草）`00 00 00 00 04 00`（TID (0,4)） |
| 8168 | 8 | -∞ ピボット: `01 00 00 00`（`block = 1` = 子）`00 00`（属性 0 個）`08 20`（`t_info = 0x2008` = 大きさ 8 \| ピボット） |
| 8176 | 16 | special: `prev = 0`、`next = 0`、`level = 1` = `01 00 00 00`、`flags = 0x0002`（`ROOT`）= `02 00`、`cycleid = 0` |

**例 5: text キー**。`CREATE INDEX ... ON t (b)` の葉タプル、キー `'hello'`、TID (1,2)。データは 1 バイトヘッダ `0D`（`(5 + 1) << 1 \| 1`）+ 5 バイト = 6 バイトで、オフセット 8..14。大きさは `MAXALIGN(14) = 16`、可変長のビット（0x4000）が立つ。

```
01 00 00 00  02 00  10 40  0D 68 65 6C 6C 6F  00 00
block=1      off=2  info=0x4010  hdr h e l l o  埋め草
```

**例 6: NULL を含むキー**。`(a int4, b text)` の索引にキー `(NULL, 'x')`、TID (0,5)。NULL があるのでビットマップ 4 バイト（a は NULL でビット 0 = 0、b は非 NULL でビット 1 = 1 なので `02 00 00 00`）、データの開始は 16。`t_info = 24 \| 0x8000 \| 0x4000 = 0xC018`。

```
00 00 00 00  05 00  18 C0  02 00 00 00  00 00 00 00  05 78  00 00 00 00 00 00
block=0      off=5  info   bitmap       埋め草(12..16)  hdr x   埋め草(18..24)
```

### 3.7 WAL レコード（rmgr = Btree = 4）

形式は M3 §3.3（レコードヘッダ・ブロック参照）に従う。`xid` は変更したトランザクション（`WriteCtx.xid`）。定数は 00 §13.4。

| info | 名前 | ブロック | メインデータ（4 バイト） |
|---|---|---|---|
| `0x00` | `BTREE_INSERT_LEAF` | blk0 = 葉。`STANDARD`、`HAS_DATA`（データ = 挿入するインデックスタプル全体、`lp_len` バイト）。FPW の対象なら画像が付き、データは付かない（`KEEP_DATA` は使わない） | `offnum: u16`（挿入後の行ポインタ番号。それ以降の行ポインタは 1 つ後ろへずれる）、予約 `u16`（0） |
| `0x10` | `BTREE_PAGES` | blk0..blkN-1（1 <= N <= 32）= ページ。**すべて `FORCE_IMAGE`**（`HAS_IMAGE`、穴があれば `HAS_HOLE`）。データなし。`WILL_INIT` は使わない | `reason: u8`、予約 `[u8; 3]` |
| `0x20` 以降 | 予約（M5: DELETE・VACUUM・UNLINK_PAGE ほか） | | 読んだら不正なレコード |

`reason`（`BTREE_PAGES`）:

| 値 | 名前 | ブロックの並び（block_id 順） | 書き手 |
|---|---|---|---|
| 1 | `INIT` | ちょうど 2 つ: ブロック 0（メタ）、ブロック 1（空のルート葉） | `init_index` |
| 2 | `SPLIT` | レベル 0 から順に、分割したページごとに「左（元のページ）、右（新）、元の右隣（あれば）」。最後に、ルートが分割されたなら「新ルート、メタ」、そうでなければ最後の区切りを受けた親 | `insert`（分割） |
| 3 | `BUILD` | 連続したブロックを昇順（最後のレコードにはメタ（ブロック 0）が最後に入る） | `build` |

デコーダの検査: `reason` が上の 3 値のどれか、`INIT` なら `nblocks == 2`、`PAGES` の全ブロックが `HAS_IMAGE`、`INSERT_LEAF` の `data_len == タプルの `t_info` の大きさ`。

**例 7: `BTREE_INSERT_LEAF`**（例 2 のページへキー 40・TID (0,4) を `offnum = 4` に挿入。FPW なし。CRC を除く）。

| オフセット | 長さ | 内容 |
|---|---|---|
| 0 | 4 | `tot_len = 76`（32 + 24 + 16 + 4）= `4C 00 00 00` |
| 4 | 4 | CRC |
| 8 | 8 | `prev` |
| 16 | 8 | `xid = 42` = `2A 00 00 00 00 00 00 00` |
| 24 | 4 | `rmgr = 4`、`info = 0x00`、`nblocks = 1`、`reserved = 0` = `04 00 01 00` |
| 28 | 4 | `main_len = 4` = `04 00 00 00` |
| 32 | 24 | `block_id = 0`、`flags = 0x02`、`fork = 0`、0 = `00 02 00 00`、`(1663, 5, 16390)` = `7F 06 00 00` `05 00 00 00` `06 40 00 00`、`block = 1` = `01 00 00 00`、`data_len = 16` = `10 00 00 00` |
| 56 | 16 | タプル: `00 00 00 00` `04 00` `10 00` `28 00 00 00` `00 00 00 00` |
| 72 | 4 | メインデータ: `offnum = 4`（`04 00`）、予約（`00 00`） |
| 76 | 4 | パディング（0）。次のレコードは +80 から |

FPW が付く場合（ページの LSN が REDO 点以前）は、ブロック参照の `flags = 0x09`（`HAS_IMAGE \| HAS_HOLE`）、`data_len = 0`、`hole_offset = pd_lower`、`hole_length = pd_upper - pd_lower`、画像（変更**後**のページ）が付き、タプルのデータは付かない。

**例 8: `BTREE_PAGES`（`INIT`）**（`init_index` が書く、メタ + 空のルート葉。CRC を除く）。メタの穴は `[56, 8176)`（長さ 8120、画像 72 バイト）、ルート葉の穴は `[24, 8176)`（長さ 8152、画像 40 バイト）。

| オフセット | 長さ | 内容 |
|---|---|---|
| 0 | 4 | `tot_len = 204`（32 + (24 + 4 + 72) + (24 + 4 + 40) + 4）= `CC 00 00 00` |
| 24 | 4 | `rmgr = 4`、`info = 0x10`、`nblocks = 2`、`reserved = 0` = `04 10 02 00` |
| 28 | 4 | `main_len = 4` |
| 32 | 24 | blk0: `block_id = 0`、`flags = 0x09`、`fork = 0`、0 = `00 09 00 00`、`(1663, 5, 16390)`、`block = 0`、`data_len = 0` |
| 56 | 4 | `hole_offset = 56` = `38 00`、`hole_length = 8120` = `B8 1F` |
| 60 | 72 | 画像 = ページの `[0, 56)` と `[8176, 8192)` を連結したもの |
| 132 | 24 | blk1: `01 09 00 00`、同じリレーション、`block = 1`、`data_len = 0` |
| 156 | 4 | `hole_offset = 24` = `18 00`、`hole_length = 8152` = `D8 1F` |
| 160 | 40 | 画像 = ページの `[0, 24)` と `[8176, 8192)` |
| 200 | 4 | メインデータ: `reason = 1`（`INIT`）、予約 3 バイト = `01 00 00 00` |
| 204 | 4 | パディング。次のレコードは +208 から |

**例 9: `BTREE_PAGES`（`SPLIT`、ルート葉の分割）**。int4 キーの連番 1..=408 のうち、407 件が入った右端のルート葉（ブロック 1。例 2 と同じ形で 407 項目）に 408 件目を追記するとき。右端の葉への追記なので左を 90% まで詰める（D6-4）: 左 = high key（キー 366）+ キー 1..=365（365 項目）、右 = キー 366..=408（43 項目）。新ページはブロック 2（右）とブロック 3（新ルート）。

| block_id | ブロック | 役割 | 内容 | `hole_offset` | `hole_length` | 画像の長さ |
|---|---|---|---|---|---|---|
| 0 | 1 | 左（元のルート。`ROOT` を外す） | high key + 365 項目、`prev = 0`、`next = 2`、`level = 0`、`flags = LEAF` | 1488 | 824 | 7368 |
| 1 | 2 | 右（新） | 43 項目、high key なし、`prev = 1`、`next = 0`、`level = 0`、`flags = LEAF` | 196 | 7292 | 900 |
| 2 | 3 | 新ルート | -∞ → 1、(366, TID) → 2、`level = 1`、`flags = ROOT` | 32 | 8112 | 80 |
| 3 | 0 | メタ | `root = 3`、`level = 1`、`fastroot = 3`、`fastlevel = 1` | 56 | 8120 | 72 |

`tot_len = 32 + 4 × 28 + (7368 + 900 + 80 + 72) + 4 = 8568`（8 の倍数）。この値（CRC を除いたレコード全体）を B1 の単体テストの固定値にする。ブロックの採番は PostgreSQL（実測: 左 1、右 2、新ルート 3）と同じ。

`describe`（`yuzhu-waldump` の表示。§5.11）の書式:

```
BTREE_INSERT_LEAF rel 1663/5/16390 blk 1 off 4 size 16
BTREE_PAGES reason=INIT blks [0 (meta), 1 (leaf root)]
BTREE_PAGES reason=SPLIT blks [1 (leaf L), 2 (leaf R), 3 (internal lvl 1 root), 0 (meta)]
```

### 3.8 容量の目安（実装の期待値。テストの固定値に使う）

- 項目に使える領域は `8152` バイト。int4 キーの葉タプル = 16 バイト（占有 20）、完全なピボット = 24（占有 28）、-∞ ピボット = 8（占有 12）。
- 右端の葉（high key なし）に int4 キーは **407 件**入る（`407 × 20 = 8140`）。408 件目で最初の分割（例 9）。連番を挿入し続けると葉は 365 件ずつ（`28 + 365 × 20 = 7328 <= 7336 = 8152 × 90 / 100`）。PostgreSQL（実測: 366 件、truncate した high key が 20 バイトのため）より 1 件少ない。
- 右端の内部ページ（high key なし）の子は最大 `1 + (8152 - 12) / 28 = 291`。
- **1 ページに最低 3 項目**: 葉タプルの最大 2704、ピボットの最大 2712。high key + データ 2 件が最大の大きさでも入る（`3 × (2712 + 4) = 8148 <= 8152`）。したがって、満杯のページを分割すると、左右ともに 1 件以上を残せる（§5.5）。

---

## 4. 型とトレイト（契約の具体化）

ここに書いた署名は、00 の署名（`IndexStore`、`TableStore` の追加、`CmpFn` などの opclass の型）に**従う**。足りないものを追加している。00 から変える点は §11 にまとめた。

### 4.1 定数

00 §15.1 の定数（`BT_MAX_ITEM_SIZE` ほか）は 00 のまま `storage/btree/mod.rs` に置き、`storage` から再エクスポートする。この章が足すもの:

```rust
// storage/btree/mod.rs
pub const BT_PAGE_USABLE: usize = 8152;          // BLCKSZ - 24 - BT_SPECIAL_SIZE
pub const BT_PIVOT_EXTRA: usize = 8;             // 完全なピボット = 葉タプル + 8（末尾に TID 6 バイト + 埋め草 2）
pub const BT_MAX_PIVOT_SIZE: usize = BT_MAX_ITEM_SIZE + BT_PIVOT_EXTRA;       // 2712
pub const BT_BUILD_BATCH_PAGES: usize = 32;      // 一括構築の 1 レコードのページ数（= MAX_BLOCK_REFS）
pub const BT_MAX_LEVEL: u32 = 64;                // これ以上の level は破損（XX001）
pub const BT_P_HIKEY: u16 = 1;

pub const INDEX_TUPLE_HEADER_SIZE: usize = 8;
pub const INDEX_SIZE_MASK: u16 = 0x1FFF;
pub const INDEX_ALT_TID_MASK: u16 = 0x2000;      // ピボット
pub const INDEX_VAR_MASK: u16 = 0x4000;          // 可変長の列を含む
pub const INDEX_NULL_MASK: u16 = 0x8000;         // NULL を含む（ビットマップあり）
pub const BT_PIVOT_HEAP_TID_ATTR: u16 = 0x1000;  // ピボットの t_tid.offset: 末尾にヒープ TID
pub const BT_PIVOT_NATTS_MASK: u16 = 0x0FFF;

pub const BTP_LEAF: u16 = 0x0001;
pub const BTP_ROOT: u16 = 0x0002;
pub const BTP_META: u16 = 0x0008;
pub const BTP_KNOWN_MASK: u16 = BTP_LEAF | BTP_ROOT | BTP_META;     // これ以外のビットが立っていたら XX001
```

`storage/btree/` のモジュール構成は 00 §4 のとおり: `mod.rs`（`BtCtx`、エラーの補助、`cmp_keys`）、`page.rs`（special・メタ・ページの検査）、`tuple.rs`（インデックスタプル）、`meta.rs`（メタページの読み書きと `init_index` の画像）、`search.rs`（比較・二分探索・降下）、`insert.rs`、`split.rs`、`wal.rs`、`scan.rs`、`build.rs`、`unique.rs`、`check.rs`。テスト用の補助は `storage/btree/testing.rs`（`#[cfg(test)]`）。

### 4.2 `storage/page.rs` への追加アクセサ（H4。既存のメソッドは変えない）

```rust
impl Page {
    /// special 領域を special_size バイト（8 の倍数）持つ空のページにする: 全体を 0 にし、
    /// pd_lower = 24、pd_upper = pd_special = 8192 - special_size、pd_pagesize_version = 0x2001。
    /// 08 の `SeqStore` も使う（08 のシーケンスのページ形式）。B+Tree は special_size = 16
    pub fn init_special(&mut self, special_size: usize);
    /// pd_special..8192。pd_special が範囲外なら空のスライス（検査は verify の責任）
    pub fn special_area(&self) -> &[u8];
    pub fn special_area_mut(&mut self) -> &mut [u8];
    /// 行ポインタ 1 つ分を引いた空き。free_space と違い MAX_HEAP_TUPLES_PER_PAGE を見ない
    pub fn free_space_unbounded(&self) -> usize;
    /// 行ポインタ off（1..=max_offset+1）に data を入れ、off 以降の行ポインタを 1 つ後ろへずらす。
    /// タプル本体は pd_upper から下へ MAXALIGN(len) だけ取り（領域を 0 で埋めてからコピー）、既存の本体は動かさない。
    /// 入らない（MAXALIGN(len) + 4 > pd_upper - pd_lower）、または off が範囲外なら None（ページは変えない）。
    /// MAX_HEAP_TUPLES_PER_PAGE の制限はない
    pub fn insert_item_at(&mut self, off: u16, data: &[u8]) -> Option<u16>;
    /// ページヘッダ（24 バイト）の直後から pd_lower までの生バイト（メタページのメタデータ）
    pub fn body(&self) -> &[u8];
    pub fn body_mut(&mut self) -> &mut [u8];
    /// pd_lower を書く（メタページが body を作るときだけ。範囲の検査は呼び出し側）
    pub fn set_lower(&mut self, lower: u16);
    /// special の内容と項目の並び（行ポインタの順）からページを丸ごと組み立てる（分割・構築用）。
    /// special.len() は 8 の倍数。項目は pd_upper から下へ並べた順に詰める。入りきらなければ None。pd_lsn と pd_checksum は 0
    pub fn build_with_items(special: &[u8], items: &[&[u8]]) -> Option<Box<Page>>;
}
```

- `item_id` / `item` は既存のまま使う（`lp_off + lp_len <= pd_special` の検査は `pd_special = 8176` に対して効く）。`add_item` は B+Tree では使わない（行ポインタの再利用なし・件数の上限があるヒープ用）。
- 単体テスト: `insert_item_at` の途中挿入（先頭・中間・末尾・満杯）、`build_with_items` と `insert_item_at` の結果が同じバイト列になること、`verify` を通ること。

### 4.3 バッファプールへの追加（M3 の C への依頼。`storage/buffer/mod.rs`）

M2・M3 のバッファプールの debug 検査は、(1) 同じリレーションではブロック番号の昇順にしかラッチしない、(2) `extend` はラッチを持たずに呼ぶ、を強制する（`track.rs`）。B+Tree の分割は「子を持ったまま親を取る」「右隣を取る」「ラッチを持ったまま新しいページを確保する」ので、どちらにも当たる。ラッチの順序（M2 §5.9 の 4 → 5: コンテンツラッチ → 拡張ロック）自体は破っていない。debug の検査だけを外す口を足してもらう（D6-10、[06-Q7]）。

```rust
// storage/buffer/mod.rs（追加。既存の read / write / extend は変えない）
impl PinnedBuffer {
    /// B+Tree 専用の共有ラッチ。read() と同じだが「同じリレーションのブロック番号の昇順」の debug 検査をしない。
    /// 同じフレームの二重ラッチの検査は残す。木の順序（下 → 上、左 → 右）は呼び出し側（btree/）が守る
    pub fn read_tree(&self) -> Result<PageReadGuard<'_>>;
    pub fn write_tree(&self) -> Result<PageWriteGuard<'_>>;
}
impl BufferPool {
    /// extend と同じ。ただし呼び出しスレッドがページのラッチを持っていてもよい（debug の assert_no_latches_for_extend をしない）。
    /// 拡張ロックの中で他のページのラッチを待たないので、デッドロックしない（追い出しの書き出しは try_read）
    pub fn extend_tree(self: &Arc<Self>, rel: RelFileLocator, fork: ForkNumber) -> Result<PinnedBuffer>;
}
```

- 実装は既存の `read` / `write` / `extend` から検査の呼び出しを 1 つ除くだけ（0.3 日）。ヒープは今までどおり `read` / `write` / `extend` を使い、検査が効き続ける。
- **ラッチの順序**（B+Tree。debug の検査はない。レビューで守る）:

| 規則 | 内容 |
|---|---|
| 同じレベル | 左 → 右 |
| レベルをまたぐ | 下 → 上（子 → 親）。親から子へ降りるときは、親のラッチを**外してから**子を取る（読み手・書き手とも） |
| メタページ | 木のページより**後**（ルート分割のときだけ排他で取る） |
| 新しく確保したページ | 誰からも見えないので、いつ取ってもよい |
| 読み手 | 同時に 1 ページしかラッチしない（葉ごとにコピーして外す）。ただし一意性検査の最中だけは、最初の葉の排他ラッチを持ったまま右の葉を共有で読む（左 → 右） |
| ヒープとの関係 | インデックスのラッチを持ったままヒープのラッチを取ってよい（一意性検査の `fetch_dirty`）。**ヒープのラッチを持ったままインデックスのラッチを取らない**（`insert_with_indexes` はヒープの挿入を終えてから索引へ入る） |

- 分割では 1 回に最大 `3h + 1`（§5.5）のページを同時にピンする。バッファプールは `shared_buffers >= 3h + 4` を想定する（+3 は、同じセッションが分割と同時に持つ他のピン（挿入中のヒープページ・メタなど）の余裕。レビュー対応 R-10: §9 と 11 の U31 を同じ式に直した）（クラッシュ試験の 8 フレームのプールでインデックスを使うワークロードは作らない。ワークロード 6 は 24 フレーム）。

### 4.4 ヒープへの追加（H4。`storage/mod.rs`、`storage/heap/`、`heap_store.rs`）

00 §13.1 の署名どおり `TableStore` に足し、`HeapStore` が実装する。

```rust
pub trait TableStore: Send + Sync + std::fmt::Debug {
    /* M2/M3 のメソッド */
    fn begin_scan_all(&self, rel: &RelHandle) -> Result<HeapScan>;
    fn tuple_state(&self, t: &HeapTuple, own: Option<Xid>) -> Result<TupleState>;
    fn fetch_dirty(&self, rel: &RelHandle, own: Option<Xid>, tid: Tid) -> Result<DirtyResult>;
    fn nblocks(&self, rel: &RelHandle) -> Result<u32>;
}
```

**`begin_scan_all`**: `begin_scan` と同じ（開始時点のブロック数で固定、1 ブロックずつ読んで `HeapTuple` にデコード）だが、`Snapshot` は `SnapshotAny`（`xmin == xmax == Xid::INVALID`、`xip` 空、`curcid = u32::MAX`、`own_xid: None`。`visibility::visible` が `snap.is_any()` で真を返す既存の取り決め）。`LP_NORMAL` のタプルを**すべて**（中断した挿入・コミット済みの削除・実行中のものを含めて）返す。`HeapTuple.row` は全ユーザー列、`xmax` はタプルが `xmax_invalid` なら `Xid::INVALID`（`decode_if_visible` の既存の規則）。`Snapshot::any()` のようなコンストラクタは足さず、`heap_store.rs` の中で構造体リテラルで作る（`txn/mod.rs` は触らない）。

**`tuple_state`**（`own` は呼び出し側のトランザクションの XID）:

```
xmin_kind(x):  x ∈ {BOOTSTRAP, FROZEN} → Committed
               Some(x) == own          → Own
               clog.status(x)          → Committed なら Committed、それ以外（Aborted、または InProgress のまま = クラッシュの残骸）は Aborted
tuple_state(t, own):
  match xmin_kind(t.xmin):
    Aborted   → InsertAborted
    Own | Committed →
      if t.xmax == INVALID                            → Live
      match xmax_kind(t.xmax)   (xmin_kind と同じ規則):
        Aborted                                       → Live          （xmax が中断した版は生きている）
        Own                                           → DeletedBySelf （自分が削除した。自分が挿入して自分が削除したものも）
        Committed                                     → DeadCommitted
```

- **自分が挿入して削除していない版は `Live`**（`InsertInProgress` ではない。07 の `build_from_heap` の前提）。`InsertInProgress(x)` / `DeleteInProgress(x)` は `x != own` の実行中のトランザクションのときの値だが、M4 は単一ライターで、書き込む他トランザクションがいないので**返さない**（D6-16。clog が `InProgress` のままの他者の XID は中断扱い）。M5 で実行中の判定（`TxnManager::is_in_progress`）を渡す口を足したら返す。
- `Err` を返すのは clog の I/O エラーなどだけ。XID 0 は `XX001`（M2 の規則）。

**`fetch_dirty`**（PostgreSQL の `SnapshotDirty` による `HeapTupleSatisfiesDirty` に相当。**コマンド ID は見ない**）:

```
fetch_dirty(rel, own, tid):
  tid.block >= nblocks、tid.offset が 0 か max_offset 超、LP_NORMAL でない → Invisible   （行ポインタは再利用しないので、ありえないが安全側。破損の検出ではない）
  ページを共有ラッチして TupleHeader を読む（ラッチを外してから clog を引く。ピンも持ち越さない）
  xmin:  Own → 続き / Committed → 続き / Aborted（中断・残骸）→ Invisible
  xmax:  無効（0 または XMAX_INVALID）→ Visible
         Own → Invisible（自分が削除済み）
         Committed → Invisible（コミット済みの削除）
         Aborted → Visible
```

- 戻り値の `WaitFor(Xid)` は M4 では返さない（型としては契約どおり）。**M5**: 実行中の他トランザクションを判別できるよう `fetch_dirty` に実行中の判定（`&dyn Fn(Xid) -> bool`）を足し、xmin / xmax が実行中なら `WaitFor` を返す。
- ヒープのラッチは `read_buffer` + `read()` の 1 回で、返す前に外す。インデックスのラッチを持ったまま呼ばれる（§4.3 の順序）。

**`nblocks`**: `pool.nblocks(rel.locator, ForkNumber::Main)`。

**列の符号化の公開**（`storage/heap/tuple.rs`。既存の `form_tuple` / `deform_tuple` はこれを呼ぶ形に直し、**ヒープの挙動・バイト列は変えない**）:

```rust
/// 1 つの非 NULL の値を、ディスク形式（整列の埋め草と varlena ヘッダを含む）で buf に追記する。
/// buf.len() は「タプルの先頭からのオフセット」を表す（整列の基準）。戻り値は可変長（varlena）の列なら true
/// （呼び出し側が HEAP_HASVARWIDTH / INDEX_VAR_MASK を立てる）。Datum::Null を渡したら Error::internal
pub fn encode_attr(buf: &mut Vec<u8>, attr: &AttrDesc, d: &Datum) -> Result<bool>;

/// 列データ領域の読み取りカーソル。bytes はタプル全体、start はデータの開始位置
#[derive(Debug)]
pub struct ColumnCursor<'a> { /* bytes, off */ }
impl<'a> ColumnCursor<'a> {
    pub fn new(bytes: &'a [u8], start: usize) -> ColumnCursor<'a>;
    /// 次の（非 NULL の）列を読む。壊れていれば XX001（"invalid tuple: ..."）
    pub fn read_attr(&mut self, attr: &AttrDesc) -> Result<Datum>;
    pub fn offset(&self) -> usize;
}
```

- `kind_of` に、00 §12.3 と 09 の「`tuple.rs` の `Kind` への追加（H4 に依頼）」の新しい型を足す（H4 の担当）: `Numeric`・`BpChar`・`Int2Vector`（varlena）、`Date`（i32、`Int4` と同じ配置）、`Timestamp` / `TimestampTz`（i64、`Int8` と同じ配置）、`regclass` / `regtype`（既存の `Oid`）。本体の変換は `types::numeric::{encode_numeric, decode_numeric}`、`types::bpchar::decode_bpchar`（09 が提供）を呼ぶだけ。`NullOnly` から `TIMESTAMPTZ` と `INT2_ARRAY` を外す。
- メッセージの `heap tuple` を `tuple` にする（インデックスタプルも使う）。XX001 のままで、テストは SQLSTATE だけを見る。

### 4.5 `storage/btree` の内部の型

```rust
// storage/btree/mod.rs
/// 1 回の操作が使う文脈。軽い値（参照だけ）で、操作ごとに作る
pub(crate) struct BtCtx<'a> { pub pool: &'a Arc<BufferPool>, pub wal: &'a Wal, pub index: &'a IndexHandle }
impl BtCtx<'_> {
    pub fn tag(&self, block: BlockNumber) -> BufferTag;                 // main フォーク
    /// XX001 `index "x" contains a corrupted page at block N`、DETAIL = detail、HINT `Please REINDEX it.`
    pub fn corrupted(&self, block: BlockNumber, detail: impl Into<String>) -> Error;
    /// XX001 `index "x" contains unexpected zero page at block N`、HINT `Please REINDEX it.`
    pub fn zero_page(&self, block: BlockNumber) -> Error;
}

/// 木の順序そのもの: キー列だけを、各列の cmp・DESC・NULLS FIRST で比べる（NULL どうしは等しい）。
/// 07 の build_from_heap のソートが使う。a.len() == b.len() == index.columns.len()
pub fn cmp_keys(index: &IndexHandle, a: &[Datum], b: &[Datum]) -> std::cmp::Ordering;
/// cmp_keys が Equal ならヒープ TID の昇順（ブロック、オフセット）
pub fn cmp_key_tid(index: &IndexHandle, a: &[Datum], a_tid: Tid, b: &[Datum], b_tid: Tid) -> std::cmp::Ordering;
```

```rust
// storage/btree/page.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BtSpecial { pub prev: BlockNumber, pub next: BlockNumber, pub level: u32, pub flags: u16, pub cycleid: u16 }
impl BtSpecial {
    pub fn read(page: &Page) -> Option<BtSpecial>;            // special 領域が 16 バイトでなければ None
    pub fn to_bytes(&self) -> [u8; 16];
    pub fn write(&self, page: &mut Page);
    pub fn is_leaf(&self) -> bool;  pub fn is_root(&self) -> bool;
    pub fn is_rightmost(&self) -> bool;                       // next == 0
    pub fn first_data_offset(&self) -> u16;                   // next != 0 なら 2、でなければ 1
}
/// ページを読んだ直後の検査（§6.3）。破れていれば XX001。expect_level が Some ならページの level と一致すること
/// （降下で「期待したレベルのページか」を確かめる。ROOT フラグは見ない）。全 0 のページは zero_page
pub(crate) fn validate_page(ctx: &BtCtx<'_>, page: &Page, block: BlockNumber, expect_level: Option<u32>) -> Result<BtSpecial>;
/// 項目（行ポインタ off）のバイト列。LP_NORMAL でなければ XX001
pub(crate) fn item<'p>(ctx: &BtCtx<'_>, page: &'p Page, block: BlockNumber, off: u16) -> Result<&'p [u8]>;

// storage/btree/meta.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BtMeta { pub root: BlockNumber, pub level: u32 }   // fastroot / fastlevel は root / level と同じ値なので持たない
impl BtMeta {
    pub fn read(ctx: &BtCtx<'_>, page: &Page) -> Result<BtMeta>;     // magic・version・fastroot == root・level < BT_MAX_LEVEL を検査
    pub fn to_page(&self) -> Box<Page>;                               // 例 1 のメタページ（pd_lsn = 0）
    pub fn write(&self, page: &mut Page);                             // body だけ書き換える
}
pub(crate) fn empty_root_leaf() -> Box<Page>;                         // 例 8 のブロック 1（flags = LEAF | ROOT）
```

```rust
// storage/btree/tuple.rs
/// インデックスタプルのバイト列（行ポインタが指す全体）。検査は validate で行い、アクセサは検査済みを前提にする
#[derive(Clone, Copy, Debug)]
pub struct IndexTuple<'a>(pub &'a [u8]);
impl<'a> IndexTuple<'a> {
    pub fn validate(bytes: &'a [u8], ncols: usize) -> std::result::Result<IndexTuple<'a>, &'static str>;   // 大きさ・ビット・ピボットの形
    pub fn size(&self) -> usize;                          // t_info の下位 13 ビット
    pub fn has_nulls(&self) -> bool;  pub fn has_varlena(&self) -> bool;  pub fn is_pivot(&self) -> bool;
    pub fn data_offset(&self) -> usize;                   // 8 または 16
    /// 葉タプル: ヒープ TID。ピボット: TID を持てば末尾の 6 バイト、持たなければ None（-∞ ピボット）
    pub fn heap_tid(&self) -> Option<Tid>;
    pub fn downlink(&self) -> BlockNumber;                // ピボットの t_tid.block
    pub fn pivot_natts(&self) -> usize;                   // ピボットの属性数（葉タプルなら ncols を返すために呼び出し側が使い分ける）
    pub fn is_minus_infinity(&self) -> bool;              // ピボットで属性数 0
}
/// 葉タプルを作る。MAXALIGN 後の大きさが BT_MAX_ITEM_SIZE を超えたら 54000（メッセージ・DETAIL・HINT・s/t/n は §6.4）。tid はメッセージ用
pub fn form_index_tuple(index: &IndexHandle, key: &[Datum], tid: Tid) -> Result<Vec<u8>>;
/// 葉タプルまたは完全なピボットから、先頭 n 列のキーを復号する（NULL は Datum::Null）。列ごとに ColumnCursor を使う
pub fn decode_key(index: &IndexHandle, t: &IndexTuple<'_>, n: usize) -> Result<Vec<Datum>>;
/// 完全なピボットを作る（葉タプル → 長さ S + 8、TID を末尾へ）。downlink は t_tid.block に入れる（high key は 0）
pub fn leaf_to_pivot(leaf: &[u8], downlink: BlockNumber, ncols: usize) -> Vec<u8>;
/// 完全なピボットの downlink だけを差し替えた複製
pub fn pivot_with_downlink(pivot: &[u8], downlink: BlockNumber) -> Vec<u8>;
pub fn minus_infinity_pivot(downlink: BlockNumber) -> [u8; 8];
```

```rust
// storage/btree/search.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rest { Low /* 残り（後ろの列と TID）が -∞ */, High /* +∞ */ }
/// 探索キー。cols は先頭 n 列（NULL は Datum::Null。n <= ncols）。tid が Some なら全列が揃っているときの最後のタイブレーク。
/// cols が全列でなく、または tid が None なら、残りは rest（Low = 同じ接頭辞のどのタプルよりも小さい、High = 大きい）として扱う。
/// したがって rest を使うキーは、どのタプルとも Equal にならない
#[derive(Clone, Copy, Debug)]
pub struct SearchKey<'a> { pub cols: &'a [Datum], pub tid: Option<Tid>, pub rest: Rest }

/// 1 列の比較（NULL どうしは Equal、NULL は nulls_first に従い先頭か末尾、DESC は非 NULL の比較だけを反転）。col.cmp を使う
pub(crate) fn cmp_column(col: &IndexKeyColumn, a: &Datum, b: &Datum) -> std::cmp::Ordering;
/// key を item と比べた結果（Less = key が小さい）。item が -∞ ピボットなら常に Greater。
/// 先頭の min(key.cols.len(), ncols) 列を順に復号して比べ、全部 Equal なら key.cols.len() < ncols のとき rest で決め、
/// 全列が揃っていれば tid（Some）で比べる。tid が None なら rest で決める
pub(crate) fn cmp_key_item(index: &IndexHandle, key: &SearchKey<'_>, item: &IndexTuple<'_>) -> Result<std::cmp::Ordering>;
/// 二分探索。データ項目の範囲 [first_data_offset, max_offset] の中で、
/// strict == false: key <= item となる最初の行ポインタ番号（下限）、strict == true: key < item となる最初の番号（上限）。なければ max_offset + 1
pub(crate) fn find_first(ctx: &BtCtx<'_>, page: &Page, block: BlockNumber, sp: &BtSpecial, key: &SearchKey<'_>, strict: bool) -> Result<u16>;
/// key がこのページの範囲に入るか（右端、または key < high key）。false なら右へ移る
pub(crate) fn page_covers(ctx: &BtCtx<'_>, page: &Page, block: BlockNumber, sp: &BtSpecial, key: &SearchKey<'_>) -> Result<bool>;

#[derive(Clone, Copy, Debug)]
pub struct StackEntry { pub block: BlockNumber, pub offset: u16 }          // 降りたページと、辿ったダウンリンクの行ポインタ番号
pub struct Descent { pub leaf: BlockNumber, pub stack: Vec<StackEntry> /* 上（ルート）から下の順 */, pub root_level: u32 }
/// メタを読み、ルートから葉まで降りる（§5.2）。ラッチ・ピンは返さない。葉のブロックは「key が属するはずの葉」で、
/// 呼び出し側がラッチして page_covers で確かめ、外れていれば右へ移る
pub(crate) fn descend(ctx: &BtCtx<'_>, key: &SearchKey<'_>) -> Result<Descent>;
/// ブロック start（レベル level）から、key が属するページまで右へ移って、その共有ラッチの下で f を呼ぶ。f の戻り値を返す
pub(crate) fn with_covering_page<R>(ctx: &BtCtx<'_>, start: BlockNumber, level: u32, key: &SearchKey<'_>,
        f: &mut dyn FnMut(&Page, BlockNumber, &BtSpecial) -> Result<R>) -> Result<R>;
```

```rust
// storage/btree/wal.rs
pub const BTREE_INSERT_LEAF: u8 = 0x00;   pub const BTREE_PAGES: u8 = 0x10;      // 00 §13.4
#[derive(Clone, Copy, PartialEq, Eq, Debug)] #[repr(u8)]
pub enum PagesReason { Init = 1, Split = 2, Build = 3 }
pub struct InsertLeafMain { pub offnum: u16 }
impl InsertLeafMain { pub fn encode(&self) -> [u8; 4]; pub fn decode(b: &[u8]) -> Result<Self>; }
pub struct PagesMain { pub reason: PagesReason }
impl PagesMain { pub fn encode(&self) -> [u8; 4]; pub fn decode(b: &[u8]) -> Result<Self>; }
/// 呼び出しは CriticalSection の中。guards の全ページを FORCE_IMAGE | STANDARD で登録して BTREE_PAGES を 1 本挿入し、
/// 全ページに set_lsn する（M3 規約 1 の (5)(6)）。登録順 = guards の順。guards.len() > MAX_BLOCK_REFS は 54000（呼び出し側が事前に防ぐ）
pub(crate) fn log_pages(wal: &Wal, w: &WriteCtx, reason: PagesReason, guards: &mut [(BufferTag, PageWriteGuard<'_>)]) -> Result<Lsn>;
/// 同じく BTREE_INSERT_LEAF（STANDARD + block_data(tuple) + main）を挿入し、guard に set_lsn する
pub(crate) fn log_insert_leaf(wal: &Wal, w: &WriteCtx, tag: BufferTag, guard: &mut PageWriteGuard<'_>, offnum: u16, tuple: &[u8]) -> Result<Lsn>;
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()>;            // recovery::dispatch が呼ぶ（§5.11）
pub fn describe(rec: &DecodedRecord) -> String;                           // wal/dump.rs が呼ぶ（§3.7 の書式）
```

### 4.6 `BtreeStore` と `IndexScan`

```rust
// storage/index_store.rs
#[derive(Debug)]
pub struct BtreeStore { pool: Arc<BufferPool>, wal: Arc<Wal> }       // リレーションごとのメモリ上の状態は持たない
impl BtreeStore { pub fn new(pool: Arc<BufferPool>, wal: Arc<Wal>) -> BtreeStore; }
impl IndexStore for BtreeStore { /* 00 §13.2 の署名どおり。各メソッドは下の関数へ委譲する薄い実装 */ }
```

| `IndexStore` のメソッド | 委譲先 | 担当 |
|---|---|---|
| `init_index` | `btree::meta::init_index`（§5.7） | B1 |
| `insert` | `btree::insert::insert` | B1 |
| `build` | `btree::build::build_index` | B2 |
| `begin_scan` / `scan_next` | `btree::scan::begin` / `btree::scan::next` | B2 |
| `nblocks` | `pool.nblocks(index.locator, Main)` | B1 |
| `unlink_storage` | `pool.drop_relation_buffers(locator)` → `smgr.unlink(locator)`（ヒープの `unlink_storage` と同じ。コミット時の unlink は `TableStore::unlink_storage` でも同じ結果） | B1 |

- **P0 が `index_store.rs` の全メソッドを委譲の形で置き、`scan.rs` / `build.rs` / `unique.rs` / `check.rs` はスタブ**（`Err(Error::not_supported(..))`、`check_unique` は `Ok(())`）にしておく。B1 は `index_store.rs` と B1 のファイルだけを編集する。
- `StorageStack`（M3）に `index: Arc<BtreeStore>`（`Arc<dyn IndexStore>` にも変換できる）を足す（A）。`StackConfig` は変えない。

```rust
// storage/btree/scan.rs
#[derive(Debug)]
pub struct IndexScan {
    index: IndexHandle,
    dir: ScanDirection,
    bounds: ScanBounds,                  // begin_scan が ResolvedScanKeys から作る（§5.9）
    state: ScanState,
    buf: std::collections::VecDeque<Tid>,   // 現在の葉から集めた一致項目（走査順）
}
#[derive(Debug)]
enum ScanState {
    NotStarted,
    /// 次に読む葉（前向き: 読んだ時点の右リンク）。None = 右端まで読んだ
    Forward { next: Option<BlockNumber> },
    /// 後ろ向き: 最後に読んだ葉（origin）と、その左リンク（prev）。None = 最左まで読んだ
    Backward { origin: BlockNumber, prev: Option<BlockNumber> },
    Done,
}
#[derive(Debug)]
struct ScanBounds { /* §5.9 */ }
pub(crate) fn begin(index: &IndexHandle, keys: &ResolvedScanKeys, dir: ScanDirection) -> Result<IndexScan>;
pub(crate) fn next(pool: &Arc<BufferPool>, scan: &mut IndexScan) -> Result<Option<Tid>>;
```

- `IndexScan` はピンもラッチも持たず（D6-8）、`Send` でなくてよい（接続のスレッドが持つ）。`begin_scan` は I/O をしない（境界の検査と比較関数の解決だけ。最初の降下は最初の `scan_next`）。

### 4.7 `catalog/opclass.rs` の静的な表（B2。全行）

00 §11.3 の型・関数をそのまま実装する。OID は PostgreSQL 17.11 の実機（`pg_opfamily` / `pg_opclass` / `pg_amop` / `pg_amproc`）から写した（実測）。**`pg_opclass` の OID のうち 10000 番台は initdb が自動で振った値**（PostgreSQL の版・ビルドでずれうる。yuzhu のカタログの中で閉じていれば動作に影響しない。テストは PostgreSQL と OID を比べない。[06-Q13]）。**`AMOPS` の演算子はすべて `builtin::OPERATORS` に実在させる**（07 の「静的な表から生成する行」。「09」と書いた行は 09 が追加する演算子。09 が追加しないなら、その行を `AMOPS` から外す）。

**`OPFAMILIES`**（8 行。`pg_am` の btree = 403）:

| OID | 名前 | | OID | 名前 |
|---|---|---|---|---|
| 424 | `bool_ops` | | 1976 | `integer_ops` |
| 426 | `bpchar_ops` | | 1988 | `numeric_ops` |
| 434 | `datetime_ops` | | 1989 | `oid_ops` |
| 1970 | `float_ops` | | 1994 | `text_ops` |

**`OPCLASSES`**（15 行）:

| OID | 名前 | 族 | 入力型（OID） | 既定 | 備考 |
|---|---|---|---|---|---|
| 10003 | `bool_ops` | 424 | bool（16） | ○ | |
| 10004 | `bpchar_ops` | 426 | bpchar（1042） | ○ | |
| 3122 | `date_ops` | 434 | date（1082） | ○ | |
| 3128 | `timestamp_ops` | 434 | timestamp（1114） | ○ | |
| 3127 | `timestamptz_ops` | 434 | timestamptz（1184） | ○ | |
| 10012 | `float4_ops` | 1970 | float4（700） | ○ | |
| 3123 | `float8_ops` | 1970 | float8（701） | ○ | |
| 1979 | `int2_ops` | 1976 | int2（21） | ○ | |
| 1978 | `int4_ops` | 1976 | int4（23） | ○ | |
| 3124 | `int8_ops` | 1976 | int8（20） | ○ | |
| 3125 | `numeric_ops` | 1988 | numeric（1700） | ○ | |
| 1981 | `oid_ops` | 1989 | oid（26） | ○ | |
| 3126 | `text_ops` | 1994 | text（25） | ○ | |
| 10044 | `varchar_ops` | 1994 | text（25） | × | `varchar` 列に明示できる |
| 10028 | `name_ops` | 1994 | name（19） | ○ | `pg_opclass.opckeytype` は PostgreSQL では `cstring`（2275）。yuzhu は 0（07 D07-17） |

- `text_pattern_ops` などの `*_pattern_ops`、`char_ops`、`oidvector_ops`、`tid_ops`、`xid8_ops` は入れない。これらの型に `CREATE INDEX` すると `42704`（`data type X has no default operator class for access method "btree"`、07）。

**`AMOPS`**（120 行 = 24 組 × strategy 5 つ。strategy は 1 `<`、2 `<=`、3 `=`、4 `>=`、5 `>`。表の 1 行 = 同じ（族、左の型、右の型）の 5 行を、`<`、`<=`、`=`、`>=`、`>` の順の演算子 OID で）:

| 族 | 左 | 右 | `<` | `<=` | `=` | `>=` | `>` | 備考 |
|---|---|---|---|---|---|---|---|---|
| 424 | 16 | 16 | 58 | 1694 | 91 | 1695 | 59 | |
| 426 | 1042 | 1042 | 1058 | 1059 | 1054 | 1061 | 1060 | 09 |
| 434 | 1082 | 1082 | 1095 | 1096 | 1093 | 1098 | 1097 | |
| 434 | 1114 | 1114 | 2062 | 2063 | 2060 | 2065 | 2064 | 09 |
| 434 | 1184 | 1184 | 1322 | 1323 | 1320 | 1325 | 1324 | 09 |
| 1970 | 700 | 700 | 622 | 624 | 620 | 625 | 623 | |
| 1970 | 700 | 701 | 1122 | 1124 | 1120 | 1125 | 1123 | |
| 1970 | 701 | 700 | 1132 | 1134 | 1130 | 1135 | 1133 | |
| 1970 | 701 | 701 | 672 | 673 | 670 | 675 | 674 | |
| 1976 | 21 | 21 | 95 | 522 | 94 | 524 | 520 | |
| 1976 | 21 | 23 | 534 | 540 | 532 | 542 | 536 | |
| 1976 | 21 | 20 | 1864 | 1866 | 1862 | 1867 | 1865 | |
| 1976 | 23 | 21 | 535 | 541 | 533 | 543 | 537 | |
| 1976 | 23 | 23 | 97 | 523 | 96 | 525 | 521 | |
| 1976 | 23 | 20 | 37 | 80 | 15 | 82 | 76 | |
| 1976 | 20 | 21 | 1870 | 1872 | 1868 | 1873 | 1871 | |
| 1976 | 20 | 23 | 418 | 420 | 416 | 430 | 419 | |
| 1976 | 20 | 20 | 412 | 414 | 410 | 415 | 413 | |
| 1988 | 1700 | 1700 | 1754 | 1755 | 1752 | 1757 | 1756 | |
| 1989 | 26 | 26 | 609 | 611 | 607 | 612 | 610 | |
| 1994 | 19 | 19 | 660 | 661 | 93 | 663 | 662 | |
| 1994 | 19 | 25 | 255 | 256 | 254 | 257 | 258 | |
| 1994 | 25 | 19 | 261 | 262 | 260 | 263 | 264 | |
| 1994 | 25 | 25 | 664 | 665 | 98 | 667 | 666 | |

- 備考が空の行の演算子は `builtin::OPERATORS`（M2）に実在することを確認済み。「09」の行（bpchar、timestamp、timestamptz の同じ型どうしの比較）は 09 の bpchar・日時の演算子の表にある。
- **PostgreSQL の `pg_amop` との差**: `datetime_ops` の型をまたぐ 6 組（date × timestamp、date × timestamptz、timestamp × date、timestamp × timestamptz、timestamptz × date、timestamptz × timestamp。30 行）を入れない（09 D-9-5。timestamp と timestamptz の相互の比較は `TimeZone` に依存し、`CmpFn` に環境を渡せない）。`date × timestamp` の相互の比較は TZ に依存しないが、09 がその演算子を作らないので入れない。09 [09-Q4] で作るなら、この表に 10 行と `AMPROCS` に 2 行（`date_cmp_timestamp` 2344、`timestamp_cmp_date` 2370）を足し、`CmpFn` は日付を午前 0 時のマイクロ秒に直して比べる（範囲外は ±infinity に飽和）。

**`AMPROCS`**（24 行。support 1 = 比較関数だけ。PostgreSQL は 2〜4 も持つが yuzhu に対応する関数がない）:

| 族 | 左 | 右 | `proc_oid` | 名前 | `cmp` |
|---|---|---|---|---|---|
| 424 | 16 | 16 | 1693 | `btboolcmp` | `cmp_datum` |
| 426 | 1042 | 1042 | 1078 | `bpcharcmp` | `cmp_datum` |
| 434 | 1082 | 1082 | 1092 | `date_cmp` | `cmp_datum` |
| 434 | 1114 | 1114 | 2045 | `timestamp_cmp` | `cmp_datum` |
| 434 | 1184 | 1184 | 1314 | `timestamptz_cmp` | `cmp_datum` |
| 1970 | 700 | 700 | 354 | `btfloat4cmp` | `cmp_datum` |
| 1970 | 700 | 701 | 2194 | `btfloat48cmp` | `cmp_datum` |
| 1970 | 701 | 700 | 2195 | `btfloat84cmp` | `cmp_datum` |
| 1970 | 701 | 701 | 355 | `btfloat8cmp` | `cmp_datum` |
| 1976 | 21 | 21 | 350 | `btint2cmp` | `cmp_datum` |
| 1976 | 21 | 23 | 2190 | `btint24cmp` | `cmp_datum` |
| 1976 | 21 | 20 | 2192 | `btint28cmp` | `cmp_datum` |
| 1976 | 23 | 21 | 2191 | `btint42cmp` | `cmp_datum` |
| 1976 | 23 | 23 | 351 | `btint4cmp` | `cmp_datum` |
| 1976 | 23 | 20 | 2188 | `btint48cmp` | `cmp_datum` |
| 1976 | 20 | 21 | 2193 | `btint82cmp` | `cmp_datum` |
| 1976 | 20 | 23 | 2189 | `btint84cmp` | `cmp_datum` |
| 1976 | 20 | 20 | 842 | `btint8cmp` | `cmp_datum` |
| 1988 | 1700 | 1700 | 1769 | `numeric_cmp` | `cmp_datum` |
| 1989 | 26 | 26 | 356 | `btoidcmp` | `cmp_datum` |
| 1994 | 19 | 19 | 359 | `btnamecmp` | `cmp_datum` |
| 1994 | 19 | 25 | 246 | `btnametextcmp` | `cmp_datum` |
| 1994 | 25 | 19 | 253 | `bttextnamecmp` | `cmp_datum` |
| 1994 | 25 | 25 | 360 | `bttextcmp` | `cmp_datum` |

- すべての行の `cmp` が `types::datum::cmp_datum`（D6-13）。`cmp_datum` は変種が型を表すので型をまたぐ比較（整数の幅違い、float4 × float8、text × name）も正しく動く（00 §12.2）。
- **`pg_proc` に要る行**（`builtin::PROCS` に `BuiltinProc`（カタログの行だけ）として B2 が足す。07 の「`pg_proc` に必要な比較関数」の表）: 上の表の 24 個（`proc_oid`、名前、`pronargs = 2`、引数の型 = 左・右、`prorettype = int4`、`proisstrict = t`、`provolatile = i`）。07 の「`pg_proc` に必要な比較関数」の表のうち、`datetime_ops` の型をまたぐ 6 個（2344、2357、2370、2526、2383、2533）は不要。

```rust
// catalog/opclass.rs（00 §11.3 の関数に、この章が足すもの）
/// 型の既定の opclass。入力型が一致する既定の opclass、なければバイナリ互換の型 → 入力型の対応
/// （varchar(1043) → text(25)、regclass(2205) / regtype(2206) / regproc(24) → oid(26)）で引く。なければ None
pub fn default_opclass(type_oid: Oid) -> Option<&'static OpClass>;
/// 列の型の族（default_opclass(type_oid).family）
pub fn family_of_type(type_oid: Oid) -> Option<Oid>;
/// Datum の変種から型の OID（Bool→16、Int2→21、Int4→23、Int8→20、Float4→700、Float8→701、Numeric→1700、
/// Text→25、BpChar→1042、Oid→26、Date→1082、Timestamp→1114、TimestampTz→1184）。それ以外は None
pub fn datum_type_oid(d: &Datum) -> Option<Oid>;
/// 索引の列（型 col_type）の値と、スキャンキーの値 d を比べる関数。comparator(族, 列の opclass の入力型, datum_type_oid(d)) を引く。
/// 引けなければ None（begin_scan が内部エラーにする）
pub fn column_comparator(col_type: Oid, d: &Datum) -> Option<CmpFn>;
/// pg_proc の行を作るための名前（AMPROCS の proc_oid → 名前）
pub fn cmp_proc_name(proc_oid: Oid) -> Option<&'static str>;
```

- `AMOPS` と `AMPROCS` は、上の組の表から展開する（`macro_rules!` か `const fn`。手書きで 120 行を並べない）。
- 単体テスト: §7.9。

### 4.8 `check.rs` の型

```rust
// storage/btree/check.rs
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckStats { pub levels: u32, pub pages: u32, pub leaf_pages: u32, pub items: u64, pub orphan_zero_pages: u32 }
/// 違反。rule は §6.2 の名前、block は該当するページ（メタや木全体なら None）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckViolation { pub block: Option<BlockNumber>, pub rule: &'static str, pub detail: String }
#[derive(Debug)]
pub enum CheckError { Violation(CheckViolation), Io(Error) }
/// 構造の検査（I14）。ピンもラッチも持ち越さない（1 ページずつ共有ラッチして写しを取る）
pub fn check_structure(pool: &Arc<BufferPool>, index: &IndexHandle) -> std::result::Result<CheckStats, CheckError>;
pub struct HeapCheck<'a> { pub heap: &'a dyn TableStore, pub rel: &'a RelHandle, pub own: Option<Xid>, pub check_unique_live: bool }
/// 構造の検査 + ヒープとの突き合わせ（I13）+ （check_unique_live なら）生きている版のキーの重複なし
pub fn check_against_heap(pool: &Arc<BufferPool>, index: &IndexHandle, hc: &HeapCheck<'_>) -> std::result::Result<CheckStats, CheckError>;
/// 全葉項目 (キー, TID) を木の順序で返す（テスト用）
pub fn dump_entries(pool: &Arc<BufferPool>, index: &IndexHandle) -> Result<Vec<(Vec<Datum>, Tid)>>;
```

---

## 5. 処理の流れ

### 5.0 全体の呼び出し関係

```
IndexStore::init_index   → meta::init_index → wal::log_pages(Init)
IndexStore::insert       → insert::insert
                              ├ tuple::form_index_tuple                                   （54000）
                              ├ search::descend → search::{find_first, page_covers}
                              ├ unique::check_unique → TableStore::fetch_dirty            （23505。最初の葉の排他ラッチを持つ）
                              └ insert::insert_into_leaf
                                    ├ （入る）wal::log_insert_leaf
                                    └ （入らない）split::split_and_insert → split::{choose_split, find_parent} → wal::log_pages(Split)
IndexStore::build        → build::build_index → build::level_pass（葉 → 上のレベル）→ wal::log_pages(Build)
IndexStore::begin_scan   → scan::begin（境界の検査と比較関数の解決。I/O なし）
IndexStore::scan_next    → scan::next → search::{descend, with_covering_page, find_first} / scan::walk_left
recovery::dispatch       → wal::redo（read_buffer_for_redo → 画像の復元 / insert_item_at）
テスト・クラッシュ試験   → check::{check_structure, check_against_heap, dump_entries}
```

### 5.1 ラッチと WAL の順序（M3 規約 1 との対応）

ページを書き換える操作は、すべて M3 §2 の規約 1 の形に固定する。

| 規約 1 の手順 | B+Tree での内容 |
|---|---|
| (1) 失敗しうる検査をすべて終える | タプルの形成（`54000`）、キーの個数の検査、分割の**計画**（ページは変えない）、必要なブロック数の検査（`54000`）、新しいページの確保（`extend_tree`。失敗しても木は無傷）、全ページの**新しい画像の組み立て**（メモリ上） |
| (2) 対象ページをピンして排他ラッチ | 木の順序（§4.3）。ガードがピンを借用するので、**ピンを先に `Vec` に作り終えてから**ガードの `Vec` を作る（`PageWriteGuard<'a>` は `'a` について共変なので、すでに持っている葉のガードも同じ `Vec` に move できる） |
| (3) `CriticalSection::enter` | 全ラッチを取り終え、確保・組み立て・検証が終わった後 |
| (4) `page_mut()` で書き換える | 画像の丸ごとコピー（`*g.page_mut() = *image`。失敗しない）、または `insert_item_at`（空きは確認済み。`None` なら `escalate`） |
| (5) `RecordBuilder` に登録して `wal.insert` | `log_insert_leaf` / `log_pages`（§4.5） |
| (6) 全ページに `set_lsn` | `log_*` の中 |
| (7)(8) ガード・セクションを Drop | |

- `wal.insert` の前後で、ページのラッチ以外のロックを持たない（規約 3）。ヒープのラッチも持たない。
- `page_mut()` を呼んだページは必ず `set_lsn` を呼ぶ（規約 2。呼び出しは `log_*` に閉じ込める）。

### 5.2 比較・探索・降下

**`cmp_key_item(key, item)`**（§4.5）: 項目がピボットで属性 0 個（-∞）なら `Greater`。それ以外は、項目のキーを先頭から 1 列ずつ復号しながら `cmp_column` で比べ、最初の非 `Equal` を返す。`key.cols` が全列より短く、すべて `Equal` のときは `key.rest`（`Low` → `Less`、`High` → `Greater`）。全列が揃っていれば、`key.tid` が `Some` ならヒープ TID（ブロック、オフセットの順）で比べ、`None` なら `key.rest`。項目のヒープ TID は、葉タプルならヘッダ、完全なピボットなら末尾の 6 バイト。**`rest` を使うキーはどの項目とも `Equal` にならない**（二分探索の境界が一意に決まる）。

**`find_first(page, key, strict)`**: データ項目 `[first_data_offset, max_offset]` で二分探索。`strict = false` は「`key <= item` となる最初の番号」（`cmp_key_item != Greater`）、`strict = true` は「`key < item` となる最初の番号」（`== Less`）。なければ `max_offset + 1`。

**`page_covers(page, key)`**: 右端なら真。そうでなければ high key（行ポインタ 1）と比べて `key < high key`（`cmp_key_item == Less`）なら真。偽ならページは key の範囲より左にあるので右へ移る（D6-1: `key >= high key` で右）。

**`descend(key)`**:

```
descend(ctx, key):
  1. メタ（ブロック 0）を共有ラッチして BtMeta を読み、外す。root、level = meta.root、meta.level
  2. stack = []; blk = root; lvl = level; hops = 0
  3. loop:
       ピン + read_tree。validate_page(page, blk, Some(lvl))?       // level の不一致・ROOT フラグは見ない（ルートは並行して分割されうる）
       while !page_covers(page, key):                                  // 右へ移動（L&Y）。ラッチは外してから次を取る
            blk = sp.next; ラッチとピンを外して blk を読み直す; hops += 1; hops > nblocks なら XX001（循環）
       if lvl == 0: return Descent { leaf: blk, stack, root_level: level }
       pos = find_first(page, key, strict = true)                      // key < item となる最初の項目
       off = pos - 1                                                   // その直前の項目 = P_i <= key となる最後。-∞ ピボットは常に <= なので off >= first_data_offset
       child = downlink(item(off))
       stack.push(StackEntry { block: blk, offset: off })
       ラッチとピンを外す; blk = child; lvl -= 1
```

- **ルートを読んだ後でルートが分割されても正しい**: 古いルートは左半分になり、`level` と `prev` / `next` を保つ。各レベルで `page_covers` が偽なら右へ移るので、新しい右半分へ辿り着く。メタを読む前に分割された場合は、新しいルートから降りる。
- 親のラッチを**外してから**子を取る（読み手・書き手とも。子が分割されても右リンクで追いつける）。
- `with_covering_page(start, level, key, f)`: ブロック `start` から、`page_covers` が真になるまで 1 ページずつ共有ラッチして右へ移り、真になったページの共有ラッチの下で `f` を呼ぶ。スキャンと検査が使う。書き手は排他ラッチで同じ手順を自分で行う（§5.3）。

### 5.3 挿入

```
insert(w, index, key, tid, check):                          // IndexStore::insert
  1. key.len() != ncols、ncols > INDEX_MAX_KEYS → Error::internal
  2. tuple = form_index_tuple(index, key, tid)?              // MAXALIGN 後の大きさ > BT_MAX_ITEM_SIZE なら 54000（ラッチを取る前。ページに触れない）
  3. do_check = index.unique && check が Check && key に NULL がない
        // UniqueCheck::Skip なら unique でも検査しない（内部の経路用）
  4. K_first = SearchKey { cols: key, tid: None, rest: Low }         // 等値の連続の先頭（TID が -∞）
     K_new   = SearchKey { cols: key, tid: Some(tid), rest: Low }
  5. d = descend(ctx, if do_check { &K_first } else { &K_new })?
  6. leaf = d.leaf
     loop:                                                           // 葉の排他ラッチ。結合しない右への移動（L&Y）
        pin = read_buffer(leaf)?; g = pin.write_tree()?; sp = validate_page(g.page(), leaf, Some(0))?
        if page_covers(g.page(), 探索キー) { break }
        leaf = sp.next; ガードとピンを外す                              // M4 では起きない（書き手は 1 人で、降下の後に木は変わらない）
  7. do_check なら unique::check_unique(ctx, &g, leaf, key, heap, rel, own)?        // g（最初の葉の排他ラッチ）を持ったまま（§5.4）
  8. 挿入先の葉を決める:
        if !page_covers(g.page(), K_new)                                // 等値の連続が右の葉へ続いていて、tid が high key 以上
            loop { next = sp.next; g とピンを外し、next を排他ラッチ（結合しない）; page_covers(K_new) なら break }
  9. insert_into_leaf(ctx, w, &pin, g, &d.stack, &tuple, &K_new)
  
insert_into_leaf(ctx, w, pin, g, stack, tuple, K_new):
  pos = find_first(g.page(), K_new, strict = false)                     // key <= item となる最初の項目の番号
  pos <= max_offset かつ cmp_key_item(K_new, item(pos)) == Equal → Error::internal("duplicate index entry")   // (key, tid) は一意。ページは変えない
  if g.page().free_space_unbounded() >= maxalign(tuple.len()):          // 入る
       cs = CriticalSection::enter(pool)
       g.page_mut().insert_item_at(pos, tuple).ok_or_else(|| cs.escalate(Error::internal("..."))) ?
       log_insert_leaf(wal, w, tag, &mut g, pos, tuple).map_err(|e| cs.escalate(e))?       // BTREE_INSERT_LEAF
       return Ok
  split_and_insert(ctx, w, pin, g, stack, tuple, pos)                   // §5.5
```

- **Rust の書き方**: ガード（`PageWriteGuard`）はピン（`PinnedBuffer`）を借用するので、手順 6〜9 の「右へ移る」ループでは、ガードを関数から返さず、**ループの本体の中で `insert_into_leaf` まで進める**（ピンを作り直すたびにガードも作り直す。`drop(g)` の後で次のピンを取る）。複数ページのガードを同時に持つ分割は §5.5 のようにピンを先に作る。
- UPDATE は新しい TID で全インデックスに項目を入れる（キー列が変わらなくても。05）。同じキーの項目が TID の順に並ぶ。
- 中断したトランザクションが入れた項目は残る（D6-7）。
- 複数のインデックスへの挿入の順序と、最初に報告される違反は 05 の `insert_with_indexes`（`TableDef.indexes` は OID の昇順 = PostgreSQL と同じ）が決める。

### 5.4 一意性検査

呼び出し元は §5.3 の 7。**最初の葉**（`K_first` が属する葉）を排他ラッチしたまま、等値のキーを持つ項目を順に調べる。

```
check_unique(ctx, first: &PageWriteGuard, first_blk, key, heap, rel, own) -> Result<()>:
  K = SearchKey { cols: key, tid: None, rest: Low }
  page = first.page(); blk = first_blk; guard_right = None
  loop:                                                              // ページごと
     sp = BtSpecial::read(page); pos = find_first(page, K, strict = false)      // key <= item の最初
     for off in pos ..= max_offset(page):
         item = IndexTuple(item(page, off))                          // 葉タプル（ピボットなら XX001）
         if cmp_keys(index, decode_key(item, ncols), key) != Equal { return Ok(()) }       // 等値の連続の終わり。以降は大きい
         match heap.fetch_dirty(rel, Some(own), item.heap_tid())? :
            Visible     → return Err(unique_violation)               // 23505（下）
            Invisible   → continue                                   // 中断・自分が削除済み・コミット済みの削除
            WaitFor(x)  → return Err(Error::internal("unique check would wait for transaction x"))      // M4 では起きない
     // ページの終わりまで等値が続いた（または pos がページの終わり）
     if sp.is_rightmost() { return Ok(()) }
     H = high key（行ポインタ 1）。cmp_keys(decode_key(H, ncols), key) > 0 なら return Ok(())            // 右のページの項目はすべて H 以上で、H のキーが key より大きい
     // H のキーが key と等しい: 右のページの最初の項目（= H）が同じキーを持つ。右へ
     next = sp.next; ピン + read_tree（first の排他ラッチは持ったまま）; page = そのページ; blk = next; K の pos は first_data_offset から
```

- **PostgreSQL の `_bt_check_unique` との対応**: 同じ構造（最初の葉を排他ラッチして検査から挿入まで持つ、等値の項目ごとにヒープを `SnapshotDirty` で調べる、等値が右の葉へ続けば右を読む）。違い: (1) `LP_DEAD` を立てない（D6-7）ので、死んだ版の項目も毎回ヒープを引く（[06-Q17]）。(2) `WaitFor` を内部エラーにする（M4 の単一ライター。M5 では「ラッチをすべて外し、そのトランザクションの終了を待って、降下からやり直す」）。(3) 自分が入れようとしているヒープタプルが生きているかの再確認（`CREATE INDEX CONCURRENTLY` 用）はしない。
- **`unique_violation`**: `Error::new(UNIQUE_VIOLATION, format!("duplicate key value violates unique constraint \"{}\"", index.name)).with_table(index.schema, index.table_name).with_constraint(index.name)`。**DETAIL は付けない**（`Key (a)=(1) already exists.` は 05 の `complete_unique_error` が、`TypeEnv` を使って補う。D5-16）。
- 同じトランザクションで DELETE してから同じキーを INSERT する、キー以外の列を UPDATE する、中断した行と同じキーを入れる、同じ文の中の 2 行目で衝突する、は PostgreSQL と同じ結果になる（§7.6 のテスト）。コマンド ID を見ないので、同じ文で先に入れた行とも衝突する。
- ヒープのラッチを取る（`fetch_dirty`）間、インデックスのラッチ（最初の葉の排他、右の葉の共有）を持つ。順序は §4.3 のとおり。
- 並行する同じキーの挿入が検査から挿入までの間に割り込めないのは、最初の葉の排他ラッチを持ち続けるから（M5 の複数ライターで効く。M4 では書き手が 1 人）。挿入先が右の葉になるとき（§5.3 の 8）は、最初の葉を外してから右へ移る。M5 では、右の葉の排他ラッチを取ってから最初の葉を外す（結合）に変える（[06-Q17] の M5 の項）。

### 5.5 分割と親への伝播

葉に入らないとき（§5.3 の `split_and_insert`）。入力: 葉の排他ガード `g`、降下の `stack`、挿入する項目 `tuple`、行ポインタ番号 `pos`。

**分割点の選択 `choose_split`**（ページ 1 枚について）:

```
入力: items = データ項目（元のページの項目に、新しい項目を位置 newpos に挿入したもの。内部ページの items[0] は -∞ ピボット）
      hk_right = 元のページの high key の大きさ（なければ None）、rightmost = 元のページが右端、is_leaf
cost(len) = MAXALIGN(len) + 4                                       // 1 項目の占有
sep_len(k) = if is_leaf { MAXALIGN(len(items[k])) + 8 } else { len(items[k]) }       // 左の high key になるピボットの大きさ
left(k)  = cost(sep_len(k)) + Σ_{i<k} cost(len(items[i]))
right(k) = (hk_right があれば cost(hk_right)) + Σ_{i>=k} cost(len(items[i]))
           - (内部ページなら cost(len(items[k])) - 12)               // 右の最初の項目は 8 バイトの -∞ ピボットになる
実行可能な k: 1 <= k <= n - 1 かつ left(k) <= BT_PAGE_USABLE かつ right(k) <= BT_PAGE_USABLE
追記の分割か: is_leaf && rightmost && newpos == n - 1
if 追記の分割:  k = 実行可能な k のうち left(k) <= BT_PAGE_USABLE * BT_FILLFACTOR_LEAF / 100 (= 7336) を満たす最大のもの（なければ実行可能な最小の k）
else:           k = 実行可能な k のうち |left(k) - right(k)| が最小のもの（同点は小さい k）
実行可能な k がなければ Error::internal（BT_MAX_ITEM_SIZE により起きない: 1 ページに high key + データ 2 件が必ず入る）
```

左ページ = `[high key = items[k] から作ったピボット（downlink 0）] ++ items[..k]`、右ページ = `[元の high key があれば] ++ items[k..]`（内部ページなら `items[k]` を -∞ ピボット（downlink はそのまま）に置き換える）。親へ上げる区切り = 同じピボットで downlink を右ページのブロック番号にしたもの。葉の区切りは `leaf_to_pivot(items[k])`、内部ページの区切りは `items[k]` の完全なピボットの複製（-∞ になる前のもの）。

**全体の手順**（D6-11）。`P_0` = 葉（ガード `g` を持っている）、`P_1, P_2, ...` = 親たち。

```
split_and_insert(ctx, w, pin, g, stack, tuple, pos):
  --- フェーズ 1: 計画（ページは変えない。g は持ったまま）---
  levels = []; cur = (P_0 のブロック、写し = g.page().clone(), 挿入位置 pos, 挿入する項目 tuple); stack は末尾（葉に近い親）から 1 つずつ取り出す。items 内の位置 newpos = ins_off - first_data_offset
  loop:                                                                       // レベル l = 0, 1, ...
     sp = cur の special
     items = cur の項目（high key を除く）に挿入する項目を位置 (ins_off - first_data_offset) に入れたもの
     if l >= 1 && 項目が cur に入る（free_space_unbounded >= MAXALIGN(len)）:
           levels.push(Absorb { cur, ins_off }); break                        // 最後の親。分割しない
     choice = choose_split(items, ...)                                        // 分割する
     Q = if sp.next != 0 { sp.next を共有ラッチして写しを取る（左 → 右）} else { None }
     levels.push(Split { cur, items, choice, Q })
     if sp.is_root():                                                          // stack が空 = ルート。ROOT フラグと一致しなければ内部エラー
           new_root = true; break
     entry = stack.pop() or Error::internal
     (parent, parent_blk, dl_off) = find_parent(ctx, entry, cur のブロック, sp.level + 1)     // 親を共有ラッチして写しを取る。ダウンリンクが無ければ右へ（§下）
     cur = (parent_blk, parent の写し, 挿入位置 dl_off + 1, 区切り（downlink は後で埋める）)
  必要なブロック数 = Σ_{Split} (2 + (Q があれば 1)) + (最後が Absorb なら 1) + (new_root なら 新ルート 1 + メタ 1)
  必要なブロック数 > MAX_BLOCK_REFS → Err 54000（何も変えていない）
  --- フェーズ 2: 確保 ---
  new_pins = (Split の数 + new_root ? 1 : 0) 個を pool.extend_tree(rel, Main) で確保（g を持ったままでよい）。ブロック番号 = pin.tag().block
       // 失敗したらここでエラー。木は無傷（確保済みのページは全 0 のまま残る）
  --- フェーズ 3: ラッチと検証 ---
  old_pins = [Q_0, 親_1, Q_1, 親_2, ..., 最後の親 / new_root なら メタ（ブロック 0）]   // 下 → 上、左 → 右の順（§4.3）
  guards = [g] ++ old_pins の write_tree() ++ new_pins の write_tree()                // 持っている g を先頭に move
  各 old ページについて、ガードのページの pd_lsn が計画で読んだ写しの pd_lsn と一致し、special.next も一致することを確かめる
       // M4 は書き手が 1 人なので常に一致。不一致なら Error::internal("btree page changed during split planning")
       //（M5: ガードをすべて外して insert を最初からやり直す）
  --- フェーズ 4: 画像の組み立て（メモリ上。Page::build_with_items / insert_item_at）---
  各 Split レベル l（左 = P_l、右 = R_l = new_pins[l]）:
       左: special { prev: P_l.prev, next: R_l, level, flags: P_l.flags & !ROOT }、項目 = [区切りのピボット（downlink 0）] ++ items[..k]
       右: special { prev: P_l, next: P_l.next, level, flags: P_l.flags & BTP_LEAF }、項目 = [元の high key] ++ items[k..]（内部は先頭を -∞ に）
       Q_l（あれば）: 写し + special.prev = R_l
       区切り（downlink = R_l）: 親のレベルへ渡す挿入項目
  最後が Absorb: 親の写しに insert_item_at(ins_off, 区切り)
  new_root: 新ルートのページ（special { prev 0, next 0, level = P_top.level + 1, flags ROOT }、項目 = [-∞ → P_top, 区切り → R_top]）、
            メタ = BtMeta { root: 新ルートのブロック, level: P_top.level + 1 }
  どれかの組み立てが None（入りきらない）→ Error::internal（CriticalSection の前なので Panic にしない）
  --- フェーズ 5: 適用（CriticalSection）---
  cs = CriticalSection::enter(pool)
  for (guard, 画像) in 対応づけ: *guard.page_mut() = *画像
  log_pages(wal, w, Split, &mut guards の順（§3.7 の block_id の並び）).map_err(|e| cs.escalate(e))?     // BTREE_PAGES 1 本。全ページに set_lsn
  ガードを Drop（取った順の逆）、cs を Drop
```

- **`find_parent(entry, child, level)`**（PostgreSQL の `_bt_getstackbuf`）: `entry.block` を共有ラッチして写しを取り、`entry.offset` の項目のダウンリンクが `child` ならそれ。違えば同じページの全データ項目を線形に探す。なければ**右隣（`next`）へ移って**繰り返す（降下の後にこの葉のダウンリンクを持つ親が右へ移った場合。M4 でも、一意性検査で右の葉へ移った挿入（§5.3 の 8）で、右の葉のダウンリンクが親の右隣にあることがある）。`next == 0` まで探して見つからなければ `XX000`（`could not find parent downlink`）。
- **ルートの検出**: `P_l` の `BTP_ROOT` が立っているときにルートの分割とする。`stack` の残りが空であることと一致しなければ内部エラー（M4。M5 では他の書き手がルートを分割した印なので、メタから降り直す）。
- **PostgreSQL との違い**: PostgreSQL は「分割」と「親への挿入」を別々のレコードにして未完了の分割（`INCOMPLETE_SPLIT`）を許す。yuzhu は分割の連鎖全体を 1 レコード（`BTREE_PAGES`）にするので、未完了の分割という状態が存在しない（D6-11）。代わりに、分割する間は葉から親までのラッチを同時に持つ。
- **確保した新ページの扱い**: 画像をコピーするまで誰からも見えない（どのページも指していない）ので、ラッチを取るタイミングは自由。`new_pins` は全 0 のページ（`init` なし）なので、画像で丸ごと上書きする。
- **失敗したときの状態**: フェーズ 1〜3 のどこで失敗しても（`54000`、確保の ENOSPC `53100`、I/O エラー）、木のページは 1 バイトも変わっていない。フェーズ 2 で確保したブロックが全 0 のまま残るだけ（検査器は `orphan_zero_pages` に数え、違反にしない）。フェーズ 5 の失敗は `escalate`（Panic）。

### 5.6 ルート分割

§5.5 の「`new_root`」の場合分け。結果（例 9 と同じ）: 元のルート（ブロック `r`）は左半分のまま（`ROOT` を外す）、右半分と新ルートは末尾に確保した 2 ブロック（`b`、`b + 1`）、メタの `root = b + 1`、`level = 旧 level + 1`、`fastroot` / `fastlevel` も同じ。ブロック番号の順序は PostgreSQL と同じ。新ルートの項目は `[-∞ → r, 区切り → b]`。

### 5.7 `init_index`

```
init_index(w, index):
  1. pool.nblocks(index.locator, Main) != 0 → Error::internal("index storage is not empty")
  2. meta_pin = pool.extend(rel, Main)?（ブロック 0）; root_pin = pool.extend(rel, Main)?（ブロック 1）      // ブロック番号が 0、1 でなければ内部エラー。extend はラッチを持たずに呼ぶ
  3. gm = meta_pin.write()?; gr = root_pin.write()?                                                       // 昇順
  4. cs = CriticalSection::enter(pool)
     *gm.page_mut() = *BtMeta { root: 1, level: 0 }.to_page(); *gr.page_mut() = *empty_root_leaf()
     log_pages(wal, w, Init, [(tag0, gm), (tag1, gr)]).map_err(|e| cs.escalate(e))?
```

- ファイルの作成（`SMGR_CREATE`）は呼び出し側（07 の `create_file`）。`init_index` は作成済みの空のファイルに書く。
- 空の表への PRIMARY KEY / UNIQUE は `init_index` だけ（`build` しない。07）。空のインデックスの `BuildStats` は `{ tuples: 0, pages: 2, levels: 0 }`（`levels` はルートの `level`、葉だけなら 0）。

### 5.8 一括構築（`build`）

`IndexStore::build(w, index, entries, unique)`（00 §13.2）。入力: `init_index` 済みで、まだ項目がないインデックス。`entries` は `(キー, TID)` を**木の順序**（`cmp_key_tid`）で整列したもの。整列は呼び出し側（07 の `build_from_heap`。`cmp_keys` を使う）。`unique` の意味:

| `BuildUnique` | 意味 |
|---|---|
| `No` | 重複を調べない。**07 はこれだけを使う**（生きている版だけの重複検査と DETAIL の組み立ては 07 の D07-7） |
| `Yes` | 入力のすべてが「生きている版」だとして、**隣接する項目**のキーが（NULL を含まず）全列 `Equal` なら `23505`（`could not create unique index "x"`、`s` / `t` / `n` つき、DETAIL なし）。テストと、死んだ版を持たない呼び出し元（将来の経路）用 |

**構築の手順**（D6-14。メモリには 1 ページ分の項目と、ページごとの区切り 1 つだけを持つ）:

```
build_index(ctx, w, index, entries, unique):
  0. 前提の検査: nblocks == 2 かつブロック 1 が項目のない葉（でなければ Error::internal）
  1. 葉のレベル（level 0）を作る: level_pass(items = entries.map(form_index_tuple), is_leaf = true, first_block = 1)
  2. 区切りの並び seps（葉のページ j = 1, 2, ... の最初の項目から作ったピボット。downlink = そのページのブロック）が
     空（葉が 1 ページ）→ そのページがルート。flags = LEAF | ROOT。メタは書き換えない（root = 1、level = 0）
     でなければ level 1 を作る: level_pass(items = [-∞(page 0 のブロック)] ++ seps, is_leaf = false, first_block = 次のブロック)
     … 区切りの並びが空になる（1 ページのレベルができる）まで繰り返す。そのページがルート（flags = ROOT）
  3. メタ: BtMeta { root: ルートのブロック, level: ルートの level } を最後のレコードに入れる

level_pass(items, is_leaf, first_block):                   // 1 つのレベルを左から右へ
  limit = BT_PAGE_USABLE * (is_leaf ? BT_FILLFACTOR_LEAF : BT_FILLFACTOR_INNER) / 100        // 7336 / 5706
  cur = []（ページに入れる項目）; used = 0; blk = first_block; prev = 0
  for item in items:                                        // 1 項目ごとに入力のイテレータが割り込みを確認する
     sz = cost(len(item))                                   // 内部ページの最初の項目（-∞ ピボット）は 12
     if cur.len() >= 2 && used + sz > limit:                // ページを閉じる。cur が 1 件のときは閉じない（2 件で 5416 <= 5706 なので、1 件のページに次が入らないことはない）
          x = cur.pop()                                     // 最後の項目を次のページの最初へ送る（PostgreSQL の nbtsort と同じ。high key の分の空きができる）
          hikey = (葉: leaf_to_pivot(x, 0)      / 内部: pivot_with_downlink(x, 0))
          emit_page(blk, cur, Some(hikey), prev, next = blk + 1)
          seps.push(葉: leaf_to_pivot(x, blk + 1) / 内部: pivot_with_downlink(x, blk + 1))      // 次のページへの区切り
          prev = blk; blk += 1
          cur = [x]（内部ページなら -∞ ピボット minus_infinity_pivot(x の downlink)）; used = cost(cur[0])
     cur.push(item); used += sz
  emit_page(blk, cur, None, prev, next = 0)                 // 最後のページ（右端）。このレベルの全体が 1 ページ（seps が空）なら ROOT

emit_page(blk, items, hikey, prev, next): Page::build_with_items で画像を作り（special { prev, next, level, flags: LEAF（葉のみ）| ROOT（このレベルが 1 ページのとき） }）、バッチに足す。
  バッチが BT_BUILD_BATCH_PAGES（32）枚になったら flush_batch。
flush_batch: バッチのブロック（連続）を用意する（ブロック 1 は既存の read_buffer、それ以外は pool.extend。ラッチは持たない）→ 各ページを write_tree()
  → CriticalSection → 画像をコピー → log_pages(Build) → set_lsn → ガードを外す
  最後のレコードには（ルートが 1 ページでなければ）メタ（ブロック 0）を最後に足す。ページが 32 枚で空きがなければ、メタだけの BTREE_PAGES（reason = Build）を別に書く
```

- **ブロック番号の採番**: 葉のページは 1, 2, 3, ...（ブロック 1 = `init_index` が作った空の葉を最初の葉として上書きする）、次のレベルは続きの番号。`pool.extend` が返すブロック番号が期待した値でなければ内部エラー（構築中は他に書き手がいない）。
- **充填率**: 葉 90%（7336 バイト）、内部ページ 70%（5706 バイト）。ページを閉じるときに最後の項目を次のページへ送るので、high key を足しても `used + 8 <= 7344 <= 8152` で必ず入る（`limit + 8 <= BT_PAGE_USABLE`）。
- **入力の検査**: 隣り合う項目が木の順序で昇順でなければ `Error::internal`（`cmp_key_tid` の `Greater`。同じ `(キー, TID)` も不可）。
- **`54000`**: `form_index_tuple` が項目ごとに検査する（メッセージは挿入と同じ。DETAIL の TID は `entries` の TID）。
- **WAL**: 構築のページは 32 枚ずつ `BTREE_PAGES`（`Build`）。ヒープの読み取り・ソートは WAL を書かない。クラッシュしたら CREATE INDEX のトランザクションは中断扱いで、ファイルは孤児として残る（M3 D15）。
- `BuildStats { tuples: 項目数, pages: 最終的な nblocks（メタを含む）, levels: ルートの level }`。
- **CREATE INDEX の入力の作り方（07 の `build_from_heap` との契約）**: `begin_scan_all` が返す各バージョンを `tuple_state` で分類する。

| `TupleState` | 索引に入れる | 一意検査の対象（生きている版） |
|---|---|---|
| `InsertAborted` | しない | — |
| `Live`（自分が挿入して削除していないものを含む） | する | する |
| `DeletedBySelf` | する | しない |
| `DeadCommitted` | する | しない |
| `InsertInProgress` / `DeleteInProgress` | M4 では返らない（内部エラー） | — |

  「索引に入れる」のは `xmin` が中断でないすべての版（古いスナップショット・同じトランザクションの後続の文が見うる版を索引から引けるように。PostgreSQL の `RECENTLY_DEAD` と同じ）。重複検査の対象は生きている版だけで、NULL を含むキーは対象外。

### 5.9 スキャン

**`ResolvedScanKeys` の意味**（00 §13.2。値は executor が評価済みで、`eq` の `None` は `IS NULL` のときだけ。NULL の値（`a = NULL`）は executor が 0 行にして B+Tree を呼ばない。05 / 04）。値の型は索引の列の型か、同じ opfamily の別の型（`int4` の列に `int8` の定数など）。型の違う比較は `begin_scan` が `column_comparator(列の型, 値)` で解決する（引けなければ内部エラー）。

| SQL の条件（先頭から `eq` が続き、次の列が範囲） | `ResolvedScanKeys` |
|---|---|
| `a = v` | `eq = [Some(v)]` |
| `a IS NULL` | `eq = [None]` |
| `a = v AND b = w` | `eq = [Some(v), Some(w)]` |
| `a > v` / `>= v` | `lower = Some((v, false / true))` |
| `a < v` / `<= v` | `upper = Some((v, false / true))` |
| `a BETWEEN l AND h` | `lower = Some((l, true))`、`upper = Some((h, true))` |
| `a = v AND b > w`（先頭列以外の範囲） | `eq = [Some(v)]`、`lower = Some((w, false))` |
| `a = v AND b IS NULL AND c < x` | `eq = [Some(v), None]`、`upper = Some((x, false))`（範囲の列は `eq.len()` 番目 = c） |
| `<>`、`IS NOT NULL`、`IN`、`OR`、`LIKE` | 作らない（04 D-18。`filter` が再評価する） |

- `lower` / `upper` は**値の向き**（`col > v` の `v`）。DESC の列では索引の並びが逆なので、`begin_scan` が「索引の並びでの最初の境界（`first`）と最後の境界（`last`）」に直す: ASC なら `first = lower`、`last = upper`。DESC なら `first = upper`、`last = lower`（包含の印はそのまま）。
- **NULL**: 範囲の列では NULL の項目はどの境界にも一致しない（`NULL > 5` は真にならない）。索引では NULL は「最大」（ASC・NULLS LAST なら末尾、`NULLS FIRST` なら先頭）。そこで NULL の領域を**走査から外す**: 下限だけの範囲（`a > 5`、NULLS LAST）は NULL の項目に達したところで打ち切る。NULLS FIRST の列で境界が `last` だけのとき（`a < 5`）は、開始位置を NULL の領域の後ろ（`(…, NULL, High)`）にして飛ばす。04 の「NULL を打ち切るのは 06 の最適化」はこれ。`filter` の再評価は残るので、打ち切りが多少漏れても結果は正しい。
- `eq` の `None`（`IS NULL`）の列では、NULL どうしを等しいとして比べる。

**`begin_scan`**: `eq.len() > ncols`、`lower` / `upper` があるのに `eq.len() >= ncols`、`eq` の `Some` が `Datum::Null` なら `Error::internal`。次の `ScanBounds` を作る（I/O なし）:

```rust
struct ScanBounds {
    prefix: Vec<Datum>,                    // eq（IS NULL は Datum::Null）
    prefix_cmp: Vec<CmpFn>,                // 列 j の値と prefix[j] を比べる関数（column_comparator。IS NULL の列は使わない）
    range_col: Option<usize>,              // = eq.len()（lower / upper があるときだけ）
    first: Option<ScanBound>, last: Option<ScanBound>,       // 索引の並びでの最初・最後の境界
}
struct ScanBound { value: Datum, inclusive: bool, cmp: CmpFn }
enum Placement { Before, In, After }       // 項目が範囲の手前・中・後
```

`classify(item_key) -> Placement`（索引の並びの中で）:

```
for j in 0..prefix.len():
    c = cmp_bound(col_j, prefix_cmp[j], item_key[j], prefix[j])         // NULL どうしは Equal。desc・nulls_first を反映した「索引の並びでの」順序
    c == Less → Before / Greater → After
if range_col is None → In
x = item_key[range_col]
if x is NULL → if col.nulls_first { Before } else { After }             // NULL の領域は範囲の外
if let Some(f) = first { c = cmp_bound(.., x, f.value); c == Less || (c == Equal && !f.inclusive) → Before }
if let Some(l) = last  { c = cmp_bound(.., x, l.value); c == Greater || (c == Equal && !l.inclusive) → After }
In
```

**開始位置**（`SearchKey`。`rest` が TID・残りの列の側）:

| 向き | 条件 | 開始キー（`find_first(.., strict = false)` = 最初の項目 `>= key`） |
|---|---|---|
| 前向き | `first` あり | `cols = prefix ++ [first.value]`、`rest = if first.inclusive { Low } else { High }` |
| 前向き | `first` なし、範囲の列があり NULLS FIRST | `cols = prefix ++ [Null]`、`rest = High`（NULL の領域を飛ばす） |
| 前向き | それ以外 | `cols = prefix`、`rest = Low` |
| 後ろ向き | `last` あり | `cols = prefix ++ [last.value]`、`rest = if last.inclusive { High } else { Low }`（この位置より**前**の最後の項目から後ろへ） |
| 後ろ向き | `last` なし、範囲の列があり NULLS LAST | `cols = prefix ++ [Null]`、`rest = Low`（NULL の領域の手前から） |
| 後ろ向き | それ以外 | `cols = prefix`、`rest = High` |

**前向き**（`scan_next`）:

```
next(scan):
  loop:
    if let Some(t) = scan.buf.pop_front() { return Some(t) }
    match scan.state:
      Done → return None
      NotStarted:
         key = 開始キー; d = descend(ctx, &key)?
         with_covering_page(d.leaf, 0, &key, |page, blk, sp| {                         // 共有ラッチ。f の中でコピーして外す
              pos = find_first(page, &key, strict = false)
              collect_forward(page, pos, &mut buf)  → { 項目を classify して In を buf に、After で done = true、Before は飛ばす }
              state = if done { Done } else { Forward { next: (sp.next != 0).then_some(sp.next) } }   // 読んだ時点の右リンク
         })
      Forward { next: Some(b) }: ピン + read_tree + validate_page(level 0) → collect_forward(page, first_data_offset, ..) → state を同様に更新
      Forward { next: None }: state = Done
```

**後ろ向き**:

```
      NotStarted:
         key = 開始キー; d = descend(ctx, &key)?
         with_covering_page(d.leaf, 0, &key, |page, blk, sp| {
              pos = find_first(page, &key, strict = false)                             // 最初の項目 >= key。その手前から後ろへ
              collect_backward(page, pos - 1, &mut buf)  → { 行ポインタ番号を first_data_offset まで降りながら classify: In を buf に、Before で done、After は飛ばす }
              state = if done { Done } else { Backward { origin: blk, prev: (sp.prev != 0).then_some(sp.prev) } }
         })
      Backward { origin, prev: Some(p) }:
         (page, blk) = walk_left(p, origin)?                                         // 下
         collect_backward(page, max_offset, ..); state = … { origin: blk, prev: その左リンク }
      Backward { prev: None }: Done

walk_left(start, origin):                                                           // origin の左隣を探す（PostgreSQL の _bt_walk_left。ページの削除がないので 4 段目は不要）
  blk = start
  loop (最大 nblocks 回):
     ピン + read_tree + validate_page(level 0)
     if sp.next == origin { return (このページ) }                                    // 見つかった: origin の左隣
     if sp.next == 0 { Err XX001（origin に辿り着けない = 破損）}
     blk = sp.next                                                                    // start が分割された。右へ寄って origin の手前を探す
```

- 後ろ向きの 1 ページ目で `pos - 1 < first_data_offset`（このページに範囲内の項目がない）なら、`collect_backward` は何も集めず、そのまま `Backward { origin: blk, prev }` に進む（前のページへ）。
- 1 回の `scan_next` で読む葉は 1 枚。集まった項目が 0 件でも、`Done` でなければ次の葉を読んで続ける（`loop`）。
- `scan_next` の呼び出しの間、ピンもラッチも持たない（D6-8）。`next` / `prev` / `origin` のブロック番号だけを覚える。
- **同じキーの項目は TID の昇順（前向き）・降順（後ろ向き）**。同じ入力に対して、前向きの結果は後ろ向きの結果の逆順に等しい（性質テスト）。

### 5.10 並行する分割の下でのスキャンの正しさ

書き手（M4 では 1 人）の分割は、関係するすべてのページを排他ラッチして 1 つのレコードで書く（§5.5）ので、読み手はページを「分割の前」か「後」のどちらかの状態で見る（途中の状態はない）。ページの削除・項目の削除・項目の左への移動はない。このとき:

1. **前向き**: 葉 `P` から一致する項目をすべて 1 回のラッチでコピーし、そのとき読んだ右リンク `N` を覚える。後から `P` が分割されて項目の一部が新しい `R`（`P → R → N`）へ移っても、コピー済みなので重複しない。`N`（古い右リンク）へ進むので `R` を読まないが、`R` の項目は `P` から移った項目（コピー済み）と、分割の後に入った新しい項目だけ。新しい項目は、スキャンのスナップショットから見える版のものではない（見える版の索引項目は、そのトランザクションがコミットする前 = スナップショットを取る前に入っているので、スキャンの開始時にはすでにある）。したがって取りこぼしも重複もない。`N` が先に分割されても、`N` はいまの左半分として読め、その右リンクを辿れば新しい右半分も読む。
2. **後ろ向き**: 葉 `P` から項目をコピーし、左リンク `L` と自分のブロック（`origin = P`）を覚える。`L` が後から分割されて `L → R_L → P`（`R_L` が新しい右半分）になると、覚えている `L` の `next` は `origin` ではない。`walk_left` は `L` から右へ `next` を辿り、`next == origin` のページ（`R_L`）を見つける。`R_L` の左リンクは `L`（分割後の左半分）なので、続けて `L` も読む。ページは消えず、新しいページは必ず既存の 2 ページの間に入る（右へ分割）ので、`start` から右へ辿れば必ず `origin` に着く（着かなければ破損）。`P` 自身が分割されても、`L.next == P` は変わらない。
3. **降下**: 親を外してから子を取る間に子が分割されても、子（左半分）の high key と比べて右へ移る（`page_covers`）。ルートが分割された場合も同じ（§5.2）。
4. **スキャンの出発点が分割された**: 最初の降下で得た葉が、ラッチを取り直す前に分割されても、`with_covering_page` が `page_covers` で右へ移る。
5. **自分の文が項目を足す**（`UPDATE ... WHERE a > 5` を `a` のインデックスで走査しながら、新しい版の項目を同じ索引に入れる。Halloween 問題）: スキャンがあとから新しい項目を見つけても、その TID のヒープタプルは `xmin` が自分で `cmin` が現在のコマンド以降なので、`TableStore::fetch` の可視性で捨てられる（M2 §6.6）。分割で自分のスキャンの位置が動いても、1〜2 が成り立つ。
6. **TID の再利用がない**: スキャンが集めた TID は、ラッチを外した後もヒープの同じ論理的な行を指す（行ポインタを再利用しない。M2 §3.4）。M5 の VACUUM が行ポインタを再利用するようになるときは、この前提（D6-8）を作り直す（クリーンアップロックか、PostgreSQL の LSN 確認）。

テスト: §7.8（単一スレッドで手順を刻んで割り込ませる）。

### 5.11 REDO、`describe`

```
redo(ctx, rec):                                                         // storage::btree::wal::redo。recovery::dispatch が RmgrId::Btree で呼ぶ
  match rec.info:
    BTREE_INSERT_LEAF:
        rec.blocks.len() == 1 でなければ Panic。m = InsertLeafMain::decode(&rec.main)?
        match read_buffer_for_redo(ctx, rec, 0)?:
           Restored | Done | NotFound → return Ok
           NeedsRedo(buf):
              g = buf.write()?                                           // REDO はクリティカルセクションを要さない（M3 §6.4.3）
              ページが btree の葉であること（BtSpecial::read、is_leaf）、1 <= m.offnum <= max_offset + 1、blocks[0].data の大きさ == タプルの t_info の大きさ を検査（破れたら Panic）
              g.page_mut().insert_item_at(m.offnum, &blocks[0].data) が None なら Panic
              g.set_lsn(rec.end.0)
    BTREE_PAGES:
        PagesMain::decode(&rec.main)?（reason が不明なら Panic）
        for id in 0..rec.blocks.len(): match read_buffer_for_redo(ctx, rec, id)? { Restored | NotFound → (), _ → Panic("BTREE_PAGES block without image") }
    その他の info → Panic（M5 の予約）
```

- `Restored` のページは画像で丸ごと上書きされ、LSN は `rec.end`。同じレコードのページ群が一貫するのは、WAL-before-data（レコードが永続化される前にどのページも書かれない）と、レコードの全ページが画像であることによる。レコードの一部のページだけがディスクに残ったクラッシュでも、REDO が残りを復元する。
- REDO の対象ブロックがファイルの末尾より先なら、`read_buffer_for_redo` が `extend_to` で伸ばす（M3 §6.4.3）。`NotFound`（ファイルがない）は invalid page として記録される（M3 D14）。
- **`describe(rec) -> String`**: §3.7 の書式。`BTREE_PAGES` は各画像の special（level、prev、next、flags）から役割を表示する。`wal/dump.rs`（W2）が `rmgr == Btree` のとき呼ぶ。
- クラッシュ試験で I13（ヒープとの突き合わせ）が成り立つ理由: ヒープの WAL がインデックスの WAL より先（同じ文の中で `insert_with_indexes` が先にヒープへ書く）なので、永続化された WAL の接頭辞に「インデックスの項目はあるがヒープのタプルがない」状態はない。逆（ヒープにあるが索引にない）は、そのトランザクションがコミットしていなければ（`xmin` が中断）起こりうる。コミットしたトランザクションの WAL はコミットレコードまですべて永続化されている。

### 5.12 H4 の分類・走査との関係（要約）

- 一意性検査は `fetch_dirty`（§4.4）、CREATE INDEX の入力は `begin_scan_all` + `tuple_state`（§4.4、§5.8）、検査器の I13 は同じ 2 つを使う（§6.2）。
- インデックススキャンの後のヒープ側（`TableStore::fetch` と可視性、`filter` の再評価）は 05 の `IndexScan` ノード。B+Tree は可視性を見ない。

---

## 6. モジュールごとの仕様

### 6.1 ファイルごとの責務

| ファイル | 責務 | 主な関数 | 担当 |
|---|---|---|---|
| `storage/btree/mod.rs` | `BtCtx`、エラーの補助（`corrupted`、`zero_page`）、定数、`cmp_keys` / `cmp_key_tid`、テスト用の割り込みフック（`#[cfg(test)]`） | §4.5 | B1 |
| `btree/page.rs` | `BtSpecial`、ページの検査 `validate_page`、項目の取り出し | `validate_page`、`item` | B1 |
| `btree/meta.rs` | `BtMeta`、`init_index`、`empty_root_leaf` | §5.7 | B1 |
| `btree/tuple.rs` | インデックスタプルの作成・復号・ピボット | `form_index_tuple`、`decode_key`、`leaf_to_pivot`、`pivot_with_downlink`、`minus_infinity_pivot`、`IndexTuple` | B1 |
| `btree/search.rs` | 比較（`cmp_column`、`cmp_key_item`）、`find_first`、`page_covers`、`descend`、`with_covering_page` | §5.2 | B1 |
| `btree/insert.rs` | `insert`、`insert_into_leaf` | §5.3 | B1 |
| `btree/split.rs` | `choose_split`、`split_and_insert`、`find_parent`、画像の組み立て | §5.5、§5.6 | B1 |
| `btree/wal.rs` | レコードのエンコード・デコード、`log_insert_leaf`、`log_pages`、`redo`、`describe` | §3.7、§5.11 | B1 |
| `storage/index_store.rs` | `BtreeStore`（`IndexStore` の実装。委譲だけ） | §4.6 | B1 |
| `btree/unique.rs` | `check_unique`、`unique_violation` | §5.4 | B2 |
| `btree/build.rs` | `build_index`（一括構築）、`BuildUnique::Yes` の重複検出 | §5.8 | B2 |
| `btree/scan.rs` | `IndexScan`、`begin`、`next`、`ScanBounds`、`classify`、`walk_left` | §5.9 | B2 |
| `btree/check.rs` | 木の検査器 | §6.2 | B2 |
| `catalog/opclass.rs` | 静的な表と関数 | §4.7 | B2 |
| `storage/page.rs`、`heap/*`、`heap_store.rs` | ヒープへの追加 | §4.2、§4.4 | H4 |
| `storage/buffer/*` | `read_tree`、`write_tree`、`extend_tree` | §4.3 | M3 の C |

- **`unsafe` は使わない**（ワークスペースの lint で forbid）。バイト形式を扱う `btree/{tuple,page,meta,wal}.rs` と `storage/page.rs` の追加分は、M3 規約 5 のとおり `#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]` を許す。
- **panic しない**: `unwrap` / `expect` は「到達しない」ことを直前の検査で保証できる箇所だけ（メッセージに理由）。到達しうる破損は `XX001`、設計上ありえない状態は `Error::internal`。
- **割り込み**: 1 回の `insert` は 1 ページ分の仕事（降下は階数ぶん）で、割り込みの確認は executor の行ごと。`build` と `check` は項目ごとに入力のイテレータ（呼び出し側）が確認する。`scan_next` は読んだ葉ごとに戻る。
- **ログ**: B+Tree は `tracing` を使わない（yuzhu-core に外部依存はない）。検査器の結果は `CheckViolation` で返す。

### 6.2 木の検査器（`check.rs`）

全テストの最後と、クラッシュ試験のリカバリの後に走らせる（I13、I14）。1 ページずつ共有ラッチして**写しを取り**、ラッチ・ピンを持ち越さない。ページを 1 枚ずつ別の時刻に読み、root と nblocks はメタを読んだ時点で固定するので、**書き手が止まっているとき（テストの最後、リカバリ後）にだけ使う**。書き手と並行すると、ルート分割で旧ルートの ROOT フラグが消える・ファイルが伸びるなどで偽の違反が出る（並行する読み手向けの許容は `descend` など §5.10 の経路だけ）。

**構造の検査 `check_structure`（I14）の規則**（`rule` の名前。違反は最初の 1 件で返す）:

| 規則 | 内容 |
|---|---|
| `meta.page` | ブロック 0 の special が `META` のみ、`prev = next = level = cycleid = 0` |
| `meta.magic` / `meta.version` | `magic`、`version` |
| `meta.root` | `1 <= root < nblocks`、ルートのページの `level == meta.level`、`fastroot == root`、`fastlevel == level` |
| `page.zero` | 木から辿れるページが全 0 でない |
| `page.special` | `pd_special == 8176`、`cycleid == 0` |
| `page.flags` | 予約・未知のビットがない（§3.2）。許される組み合わせだけ。`LEAF` ⇔ `level == 0` |
| `page.lp` | すべての行ポインタが `LP_NORMAL`、`lp_off` / `lp_len` がページ内で重ならない、`pd_lower <= pd_upper` |
| `page.item` | 各項目の `t_info` の大きさ == `lp_len`、8 の倍数、`<= BT_MAX_ITEM_SIZE`（ピボットは + 8）、ビットマップとデータの整合、キーが復号できる |
| `page.kind` | 葉は葉タプルだけ（high key を除く）、内部ページはピボットだけ。ピボットの属性数は 0（内部ページの最初のデータ項目だけ）または `ncols`。完全なピボットは TID を持つ |
| `page.first_pivot` | 内部ページの最初のデータ項目が -∞ ピボットで、それ以外にない |
| `page.order` | データ項目が `(キー, TID)` で厳密に昇順（内部ページは -∞ を除く） |
| `page.hikey` | high key の有無と `next != 0` が一致、`block == 0`、完全なピボット、全データ項目が high key より厳密に小さい |
| `link.level` | 同じレベルのチェーン（最左から `next` を辿る）のページの `level` がすべて同じ |
| `link.pair` | `a.next == b` ⇔ `b.prev == a`。最左の `prev == 0`、最右の `next == 0` |
| `link.cycle` | チェーンが循環せず、ブロック数以内で終わる |
| `tree.children` | 内部ページの子（データ項目のダウンリンク）を、そのレベルの全ページの順に並べたものが、1 つ下のレベルのチェーンと**完全に一致**（すべてのページにダウンリンクがある。欠けは `tree.downlink_missing`、余分は `tree.downlink_extra`）。子の `level` == 親の `level - 1`（`tree.downlink_level`） |
| `tree.bounds` | 子 `c_i` の全データ項目 `x` が `P_i <= x < P_{i+1}`（最初の子の下限・最後の子の上限は親の範囲を引き継ぐ。ルートは上下とも無限）（D6-1）。子の high key が `P_{i+1}`（`block` を 0 にした同じバイト列）と一致、最後の子は親の high key（親が右端なら high key なし）と一致（`tree.child_hikey`） |
| `tree.root_flag` | `BTP_ROOT` が立っているのがルートだけ |
| `tree.orphan` | ブロック 1 以降で、辿れないページは全 0 だけ（`orphan_zero_pages` に数える）。内容のあるページは違反 |

**ヒープとの突き合わせ（I13）の規則** `check_against_heap`（`check_structure` に加えて）:

```
entries = 全葉項目を走査順に (キー, TID) で集める。同じ TID が 2 つあれば heap.duplicate_tid
for t in begin_scan_all(rel):                                       // 全バージョン
    key = index.columns.map(|c| t.row[c.attnum - 1])
    state = tuple_state(t, own)
    match entries.remove(t.tid):
        Some(k): cmp_keys(index, k, key) != Equal → heap.key_mismatch                    // 中断した版でも、項目があればキーは一致する
        None:    state != InsertAborted → heap.missing_entry                             // 中断していない版は必ず索引にある
残った entries（ヒープに対応するタプルがない）→ heap.dangling_entry
check_unique_live && index.unique: state == Live かつキーに NULL がない版のキーが 2 つ以上同じ → unique.live_duplicate
```

I13 の定義（00 §18 の「ヒープの索引されるべき版」）= `xmin` が中断でないすべての版（`Live`・`DeletedBySelf`・`DeadCommitted`。§5.8 の表の「索引に入れる」と同じ）。

### 6.3 読み込み時の破損検出と M5 への予約

B+Tree がページを読むたびに（降下・スキャン・挿入・検査器）、`validate_page` で次を検査する。破れていたら `XX001`（D6-15）。

| 検出するもの | メッセージ（PostgreSQL に寄せる） |
|---|---|
| 全 0 のページ（`is_new`）が木のページとして現れた | `index "x" contains unexpected zero page at block N`、HINT `Please REINDEX it.` |
| special 領域が 16 バイトでない（`pd_special != 8176`） | `index "x" contains a corrupted page at block N`、DETAIL `unexpected special area size` |
| **予約・未知のフラグが立っている**（`DELETED`、`HALF_DEAD`、`SPLIT_END`、`HAS_GARBAGE`、`INCOMPLETE_SPLIT`、`HAS_FULLXID`、その他。M5 の予約。00 §13.5） | 同上、DETAIL `reserved flags 0x0004 are set` |
| `LEAF` と `level == 0` の不一致、期待したレベルと違う、`level >= BT_MAX_LEVEL` | DETAIL `unexpected level` |
| `cycleid != 0` | DETAIL `unexpected cycle id` |
| `LP_NORMAL` でない行ポインタ（`LP_DEAD`、`LP_REDIRECT`、`LP_UNUSED`） | DETAIL `unexpected line pointer flags` |
| 項目の `t_info`・大きさ・ピボットの形の不整合（属性数が 0 でも `ncols` でもない、posting のビット） | DETAIL `invalid index tuple` |
| メタの `magic` / `version` | `index "x" is not a btree` / `version mismatch in index "x": file version V, current version 1`（`XX001`） |
| 降下・右への移動・`walk_left` がブロック数を超えて循環 | DETAIL `sibling chain is cyclic` |

**M5 のために予約し、M4 では作らない・壊さないもの**（00 §19 に加えて）: `btpo_cycleid`、`DELETED` / `HALF_DEAD` / `INCOMPLETE_SPLIT` ほかのフラグ、`BTREE_*` の `0x20` 以降の info、ピボットの属性数が `ncols` 未満・posting のビット、`LP_DEAD`、`fastroot`・`fastlevel`、`UniqueCheck` の `WaitFor` と降下からのやり直し、`find_parent` の「ルートが分割されていたらメタから降り直す」分岐、`with_covering_page` の「削除済みのページなら右へ」分岐、`walk_left` の「`origin` が削除された」分岐、一意性検査と右への移動の結合ラッチ、`fetch_dirty` の実行中判定、フェーズ 3 の LSN の不一致の再試行。

### 6.4 エラーの一覧

| 状況 | SQLSTATE | メッセージ・フィールド |
|---|---|---|
| 一意性違反（`insert`） | `23505` | `duplicate key value violates unique constraint "<索引名>"`。`schema` = 索引のスキーマ、`table` = 表名、`constraint` = 索引名。DETAIL なし（05 が補う） |
| 一意性違反（`build` の `Yes`） | `23505` | `could not create unique index "<索引名>"`。フィールドは同じ。DETAIL なし（07 は `No` で自分で検出する） |
| タプルが大きすぎる（`insert`、`build`） | `54000` | `index row size 2712 exceeds btree version 4 maximum 2704 for index "<索引名>"`、DETAIL `Index row references tuple (0,2) in relation "<表名>".`、HINT `Values larger than 1/3 of a buffer page cannot be indexed.\nConsider a function index of an MD5 hash of the value, or use full text indexing.`（実測）。フィールドは同じ（`constraint` = 索引名）。サイズは `MAXALIGN` 後の値。タプルが 8191 バイトを超えるとき（ヒープの行の上限 8160 で到達しない）の `index row requires N bytes, maximum size is 8191` は作らない |
| 分割の連鎖が 32 ブロックを超える | `54000` | `index "<索引名>" cannot be split: too many levels`（yuzhu 独自。木は変えない） |
| 列数が `INDEX_MAX_KEYS`（32）を超える | `54011` | `cannot use more than 32 columns in an index`（実測。07 の `CREATE INDEX`。B+Tree は内部エラーで防ぐ） |
| 他トランザクションの待ちが必要（`WaitFor`） | `XX000` | `unique check would wait for transaction N`（M4 では起きない） |
| ページの破損 | `XX001` | §6.3 |
| 設計上ありえない状態（重複する `(キー, TID)`、親のダウンリンクが見つからない、計画と実際のページが違う、入力が整列していない、空きがあるはずなのに入らない） | `XX000` | 各所の文言 |
| ファイルの拡張の失敗（ENOSPC など） | `53100` / `58030` | smgr のエラーのまま |
| `CriticalSection` の中のエラー | `Severity::Panic` | `escalate`（M2 の規約） |

---

## 7. テスト

PostgreSQL 17 で確かめた期待値は「（実測）」。以下はすべて `cargo test`（debug。M2 のピン・ラッチの検査が効く）で通すもの。時間のかかるものは `#[ignore]`（夜間）とする。テストは**全部の最後に `check_structure`（I14）と `pool.pinned_frames() == 0`** を確かめる。

### 7.1 固定値（B1）

- §3.6 の例 1〜6（`BtMeta::to_page`、`Page::build_with_items`、`form_index_tuple` の出力がバイト単位で一致）。`leaf_to_pivot` の例 3・4 のピボット（24 バイト）、`minus_infinity_pivot`（8 バイト）。
- §3.7 の例 7・8・9（CRC を除く。`RecordBuilder` → `Wal::insert` → `decode_record`、`tot_len = 76 / 204 / 8568`）。例 9 は int4 キー 1..=408 を連番で挿入した 408 件目の WAL。
- 容量（§3.8）: int4 連番で 407 件目まで分割しない、408 件目で最初の分割、左 365・右 43、そのまま続けると葉は 365 件、右端の内部ページの子は最大 291。ブロックの採番（左 1、右 2、新ルート 3）。

### 7.2 `54000` の境界（B1。実測の PostgreSQL 17 と同じ値）

| 索引 | 入れる値 | 成功 | `54000`（`index row size 2712 exceeds ...`） |
|---|---|---|---|
| `(a text)` | `a` の長さ（非圧縮の値） | 2692 | 2693 |
| `(a int, b text)` | `a = 1`、`b` の長さ | 2688 | 2689 |
| `(a int, b text)` | `a = NULL`、`b` の長さ | 2684 | 2685 |

- 失敗した後: 索引のブロック数が変わっていない、ページの内容が変わっていない、`check_structure` が通る。DETAIL の TID と表名、`s` / `t` / `n` のフィールド。
- 境界の大きさのキー（2692 バイト）を 100〜300 件、ランダムな順・昇順・降順で挿入して、1 ページ 3 項目の分割（§3.8）と高さ 5 以上の木を作り、構造の検査と全件スキャンを通す。`build` でも同じ件数を構築して同じ結果になる。

### 7.3 性質テスト（proptest。B1 が挿入・スキャン、B2 が一括構築を足す）

- **モデル**: `BTreeMap<(モデルのキー, Tid), ()>`。モデルのキーの順序は**テストの中で独立に書く**（`btree` の比較関数を使わない）: スキーマごとに、列の値を「NULL かどうか」と値のタプルに写し、ASC / DESC・NULLS FIRST / LAST を反映した順序のラッパー型を作る。
- **スキーマ**: (1) `int4` ASC、(2) `text` DESC NULLS LAST、(3) `(int4, text)` ともに ASC・NULL を含む、(4) `int4` DESC NULLS FIRST、(5) `(int4, int4)`（先頭だけ重複が多い）、(6) `numeric`（`1.0` と `1.00` が等しい）、(7) `bpchar`（末尾の空白）。
- **操作列**: `Insert(キー, 新しい TID)`（昇順・降順・ランダム・重複の多い値の 4 つの生成器）、`Scan(境界, 向き)`（§7.10 の境界の全種類）、`CheckStructure`。
- **各操作列の終わり**（と途中の N 操作ごと）: `dump_entries` == モデルの全件、各 `Scan` == モデルを同じ述語で絞って（向きに合わせて）並べたもの、`check_structure`。
- 設定: CI は `cases = 256`（時間の上限 60 秒）、夜間は 8192。失敗時はシードと縮小後の操作列を出す。

### 7.4 分割の網羅（B1）

分割の分岐を**すべて**通す。大きさの違うキー（text の長さで項目数 / ページを 3〜400 に調整）で、ページが満杯の直前の状態を作る補助（`testing::fill_page_to_brink`）を使う。

| 軸 | 場合 |
|---|---|
| 葉の分割 | 右端の葉に末尾へ追記（90%）／右端の葉の途中へ挿入（均等）／右端でない葉（均等）。新しい項目が左に入る・右に入る・ちょうど `k` の位置 |
| 親 | 親に入る（Absorb）／親も分割（非ルート）／親の分割でルートが分割される |
| 連鎖 | 2 レベル・3 レベル・全レベルが同時に満杯（高さ 3〜5）。各レベルで元の右隣 `Q` がある・ない |
| ルート分割 | 葉のルート（高さ 1 → 2）、高さ 2 → 3、高さ 3 → 4 |
| 高さ | 幅の広いキー（1500 バイト）で高さ 4 以上、`#[ignore]` で int4 を 10 万件以上挿入して高さ 3 |
| ブロック数 | 観測した `BTREE_PAGES` の最大ブロック数 `<= 3h + 1`。全レベルの同時分割で `3h + 1` に届くものを少なくとも 1 件作る（WAL をデコードして数える） |
| 失敗 | `MAX_BLOCK_REFS` を小さくするテスト用の上書き（`#[cfg(test)]`）で `54000`: 木のページが 1 バイトも変わらない（全ページの `pd_lsn` とバイト列、`nblocks` は確保した分だけ増える）。`extend_tree` の ENOSPC（`SimVfs` の障害注入）でも木が変わらない |

- `choose_split` の単体テスト: ランダムな項目の大きさ（8〜2712）で、(1) 左右とも `<= 8152`、(2) 左右とも 1 件以上、(3) 追記の分割で左が `<= 7336` の最大の `k`、(4) 均等の分割で `|left - right|` が最小（全 `k` を総当たりで確かめる）、(5) 最大の大きさの項目ばかりのページで必ず実行可能な `k` がある。内部ページ（-∞ の 12 バイト）を含む。

### 7.5 REDO（B1）

- **REDO の同値性**: 操作列（挿入・分割・ルート分割）を実行しながら WAL を取り、チェックポイントを挟む。全ページ（`BufferPool` を flush して `smgr` から読む）の写しを取り、**空のディスク**（または分割の前の古いページ）の上で同じ WAL を REDO して、全ページが**バイト単位で一致**する。REDO を 2 回続けても同じ（冪等）。ページの LSN の判定（`Done`）が効く場合（古いページ）も同じ。
- FPW の有無: `BTREE_INSERT_LEAF` が画像あり・なしの両方、`BTREE_PAGES` は常に画像。ブロックがファイルの末尾より先（`read_buffer_for_redo` が伸ばす）。ファイルがない（`NotFound`）。
- 不正なレコード: 未知の info、`INSERT_LEAF` の `offnum` が範囲外・データの大きさが `t_info` と違う、`PAGES` の画像なしのブロック、`INIT` で `nblocks != 2` は `Severity::Panic`。
- `describe` の出力（§3.7 の書式）。

### 7.6 一意性（B2）

| ケース | 期待（実測の PostgreSQL 17 と同じ） |
|---|---|
| 同じキーを 2 回 | `23505`、`duplicate key value violates unique constraint "t_pkey"`、`s` / `t` / `n` |
| 複合キーで NULL を含む行が重複 | 成功（NULL は別物） |
| 同じトランザクションで DELETE → 同じキーの INSERT | 成功 |
| キー以外の列の UPDATE | 成功（旧版は自分が削除済み） |
| キーを変える UPDATE が既存の行と衝突 | `23505` |
| ROLLBACK した行と同じキー | 成功。クラッシュの残骸（clog が実行中のまま）も同じ |
| コミット済みの削除の後に再挿入 | 成功 |
| 同じ文の中の 2 行目が 1 行目と衝突 | `23505`（コマンド ID を見ない） |
| 等値の連続が複数の葉にまたがる（死んだ版 2000 件 + 生きている版 1 件。生きている版が先頭・中ほど・末尾の 3 通り） | 生きている版があれば `23505`、なければ成功。挿入先が右の葉になる（TID が high key 以上）ときも正しく入る |
| 値の等しさ | `numeric` の `1.0` と `1.00`、`bpchar` の `'a'` と `'a '`（どちらも実測で衝突し、DETAIL は新しい行の値 `(1.00)`、`(a  )` を出す） |
| 多列・DESC・NULLS FIRST の一意索引 | 順序に依存せず同じ結果 |
| `fetch_dirty` が `WaitFor` を返す（テスト用の偽のヒープ） | `XX000`。ページは変わらない |
| `UniqueCheck::Skip` | unique でも検査しない |

### 7.7 一括構築（B2）

- 件数: 0（何もしない）、1、1 ページに収まる最大、葉 2 ページ、`BT_BUILD_BATCH_PAGES` の境界（31・32・33・64・65 ページ）、メタが同じレコードに入る・入らない場合、多段（幅の広いキーで高さ 4）。
- **逐次挿入との同値**: 同じ入力を `build` した木と、1 件ずつ `insert` した木で、`dump_entries` と全種類のスキャンが同じ。構築の後に挿入を続けても構造が保たれる（分割が起きる）。
- 充填率: 葉の充填が 90% 以下（`<= 7336`）、内部が 70% 以下。最後の項目を次のページへ送る規則（キーの大きさ 1800 バイトで、閉じるページの `used + hikey > 8152` になりうる場合）、最大の大きさのキーだけの入力。
- `BuildUnique::Yes`: 重複で `23505`（`could not create unique index`、`s` / `t` / `n`）、NULL を含むキーの重複は成功、`No` は重複を通す。整列していない入力は `XX000`。
- `BuildStats` の値（`tuples`、`pages`、`levels`）、`BTREE_PAGES` の理由（`Build`）とブロックの連続性。`54000`。
- REDO の同値性（§7.5）を構築にも。クラッシュ点の全探索（§7.11）で、構築の途中のクラッシュの後にトランザクションが中断扱いになり索引が使われないこと。

### 7.8 並行する分割と読み手（B1・B2。単一スレッドで手順を刻んで割り込ませる）

`#[cfg(test)]` の割り込みフック（`btree/mod.rs`）を使う。降下・スキャン・`walk_left` の**ラッチを外した隙間**で、テストが与えた関数を同じスレッドで実行する（その中で `insert` を呼べる。隙間ではラッチもピンも持たないので再入できる）。

```rust
#[cfg(test)]
pub(crate) enum HookPoint {
    DescendBetweenLevels { level: u32 },      // 親を外して子を取る前
    DescendAfterMeta,                         // メタを読んだ後、ルートを読む前
    ScanBetweenLeaves,                        // 1 枚の葉をコピーして外した後、次の葉を読む前
    WalkLeftBeforeLatch { target: BlockNumber },   // 左リンクを覚えた後、そのページを読む前
}
#[cfg(test)] pub(crate) fn set_test_hook(f: Option<Box<dyn FnMut(HookPoint)>>);   // スレッドローカル
```

| シナリオ | 割り込みの内容 | 期待 |
|---|---|---|
| 前向きスキャンの途中 | `ScanBetweenLeaves` で、いま読み終えた葉と次の葉が分割されるよう挿入する（葉が右へ分割される） | スキャン開始時に存在した一致項目が**ちょうど 1 回ずつ**、昇順で返る。新しい項目はあってもなくてもよいが、重複はない |
| 前向き。ルートの分割 | `DescendAfterMeta` / `DescendBetweenLevels` でルートを分割する挿入 | 結果は同じ |
| 後ろ向き。左の葉が分割 | `WalkLeftBeforeLatch { target }` で `target`（覚えた左の葉）を分割する挿入 | 開始時の一致項目がちょうど 1 回ずつ、降順で返る（`walk_left` が右へ寄って分割後の右半分から読む） |
| 後ろ向き。自分が分割 | 1 枚目を読んだ後に、そのページを分割する挿入 | 同じ |
| 連続した分割 | 隙間ごとに分割を何度も起こす（ランダム） | 同じ |
| 降下の後の分割 | `DescendBetweenLevels { level: 0 }` で目的の葉を分割する | 挿入が右の葉へ移って成功（`page_covers`） |

- 検証は「開始時の集合」（スキャン前の `dump_entries`）と比べる。割り込みで増えた項目は「開始時の集合 ∪ 追加した項目」の範囲に収まることだけ確かめる。
- 別に、**複数スレッド**（書き手 1 + 読み手 3）で分割を繰り返しながらスキャンする試験を 1 つ置く（再現性はないが、実際のラッチの競合を通す。`cargo test` に入れてよい小さい規模。失敗したら刻み方の試験で再現する）。

### 7.9 opclass の表（B2）

- **表の完全性**: `OPFAMILIES` 8、`OPCLASSES` 15（族ごとに既定が型ごとに 1 つ）、`AMOPS` 120（24 組すべてに strategy 1〜5 がそれぞれ 1 行）、`AMPROCS` 24。
- **演算子の実在と意味**: `AMOPS` のすべての `operator` が `builtin::OPERATORS` に実在し、`(左, 右)` の型と名前（`<` `<=` `=` `>=` `>`）が strategy と一致する。各組について、ランダムな値の対で `OPERATORS` の関数の結果（`<` なら真）と `cmp` の結果（`Less`）が一致する（**カタログの演算子と B+Tree の順序が食い違わない**）。
- `AMPROCS` の `proc_oid` が `builtin::PROCS` に実在する（07 の `dangling_references` と二重に）。
- `default_opclass`: 各型（`varchar` → `text_ops`、`regclass` → `oid_ops`、`name` → `name_ops`）、`interval`・`xid` → `None`。`opclass_by_name("varchar_ops")`（入力型 text）、`opclass_by_oid`、`family_of_type`。
- `comparator`・`column_comparator`: `(int4 列, Int8 の値)`、`(float4 列, Float8 の値)`、`(name 列, Text の値)`、`(text 列, Text の値)`、引けない組（`date` 列に `Int4` の値）は `None`。
- **`cmp_column` と `types::cmp::cmp_with_nulls` の一致**: ASC / DESC × NULLS FIRST / LAST の 4 通りと、NULL を含む全組み合わせで同じ `Ordering`（D6-13。Sort・Unique とインデックスの順序が一致することの保証）。

### 7.10 スキャンの境界・NULL・DESC（B2）

- **条件の網羅**: スキーマ（`int4` ASC NULLS LAST、`int4` DESC NULLS FIRST、`(int4, int4)`、`(int4, text)` で text が DESC NULLS LAST）× 条件（`eq` が 0・1・2 列、`IS NULL`、`lower` / `upper` の包含・排他の全組み合わせ、`BETWEEN`、先頭列以外の範囲、`eq` に NULL の列を含む）× データ（NULL を含む・含まない、重複あり、空）× 向き。
- **オラクル**: 全項目を条件（範囲の列の NULL は一致しない。`IS NULL` は NULL どうしを一致させる）で絞り、木の順序で整列したもの。前向き == オラクル、後ろ向き == オラクルの逆順。
- `IS NULL` と範囲が同じ列に来る条件（`eq = [None]` の後の範囲）。
- **NULL の打ち切り**: 下限だけの範囲（`a > 5`、NULLS LAST）が NULL の領域に入る前に止まる（テスト用の「調べた項目数」で全走査より少ないことを確かめる）。NULLS FIRST の列の `a < 5` が NULL の領域を飛ばして始まる。
- 型の違う値: `int4` の列に `Int8` の定数、`float4` の列に `Float8`、`name` の列に `Text`、`varchar` の列に `Text`、`numeric` の `1.0` と `1.00`。`begin_scan` が解決できない型は `XX000`。
- 後ろ向きスキャンのページ境界: 範囲が葉の先頭・末尾にちょうど当たる、範囲がページをまたぐ、最初のページに範囲内の項目がない。
- 不正な `ResolvedScanKeys`（`eq` が列数より長い等）は `XX000`。

### 7.11 クラッシュ試験（`yuzhu-core/tests/crash_sim/`。R2 と B1）

00 §18 のワークロード 6 と不変条件 I13・I14。

- **ワークロード 6 `indexed_table`**: 表 `t(k int PRIMARY KEY, s text, v int)` + `UNIQUE INDEX (s)` + `INDEX (v DESC NULLS FIRST)`。1 本のスレッドで複数のセッションを順番に操作する（M3 §7.5）。操作: 連番とランダムなキーの INSERT（`s` は 700 バイトで分割を頻発させる）、非キー列の UPDATE、キーを変える UPDATE、DELETE、ROLLBACK するトランザクション、実行中のまま残すトランザクション、**既存の行に対する `CREATE INDEX`（33 ページ以上になる件数で、構築の 32 ページごとのレコードを通す）**、k 文ごとの `CHECKPOINT`。`shared_buffers = 24`（分割で最大 `3h + 1` ページをピンするので、8 フレームのワークロード 5 とは別にする）。
- **検査**: 各クラッシュ点（小さいワークロードは I/O の通し番号をすべて、大きいものはシードで選ぶ。`DropUnsynced`、`KeepAll`、`TornSectors { 512 / 4096 }`、`RandomSubset`）の後のリカバリで、M3 の I1〜I12 に加えて
  - **I14**: すべてのインデックスで `check_structure` が通る（分割の途中の状態が見えない = 原子性）。
  - **I13**: すべてのインデックスで `check_against_heap`（`check_unique_live = true`）が通る。
  - 確定したトランザクションの `(k, s, v)` がモデルと一致し、PRIMARY KEY・UNIQUE に重複がなく、リカバリ後に同じキーを INSERT すると `23505`。
  - リカバリ後にさらに挿入して再びクラッシュしても成り立つ（I10）。
- **変異テスト**（ハーネスが B+Tree の壊れを検出できること）:

| 変異 | 方法 | 検出されるべきもの |
|---|---|---|
| 分割の原子性を壊す | `DebugKnobs::btree_split_in_two_records`（任意。[06-Q16]。分割の連鎖を下位のページと上位のページの 2 本のレコードに分けて書く） + `DropUnsynced` | I14 |
| FPW を無効にする | 既存の `disable_full_page_writes` + `TornSectors`（`BTREE_PAGES` は `FORCE_IMAGE` なので影響を受けず、`BTREE_INSERT_LEAF` の葉が壊れる） | I14 または起動時の `XX001` |
| WAL-before-data を破る | 既存の `skip_wal_before_data` + `DropUnsynced` | I13 または I14 |
| REDO の LSN 判定を外す | 既存の `redo_ignore_page_lsn` | I14（`insert_item_at` の二重適用） |

### 7.12 H4（H4）

- `tuple_state` の全行（§4.4 の分類を、xmin・xmax が own / committed / aborted / 実行中のまま（クラッシュの残骸）の全組み合わせで）。自分が挿入して削除していない版は `Live`。
- `fetch_dirty` の全行（own・committed・aborted・残骸 × xmax なし・own・committed・aborted、範囲外の TID）。**コマンド ID を見ない**（同じ文の前の行と衝突しうる）。
- `begin_scan_all` が中断した挿入・コミット済みの削除・自分の削除を返す。開始後に増えたブロックを読まない。
- 列の符号化: `encode_attr` / `ColumnCursor` の往復（全型。NULL なし・可変長・整列の境界）、`form_tuple` / `deform_tuple` のバイト列が変わっていない（M2 の固定値の回帰）、インデックスタプルでも同じ。新しい型（numeric、bpchar、date、timestamp、timestamptz、int2vector、regclass）。
- `Page::{init_special, insert_item_at, build_with_items, body, special_area}`（§4.2）。

### 7.13 検査器の負のテスト（B2）

健全な木を作り、ページのバイトをバッファプールの中で壊して、**指定した規則**が返ることを確かめる（検査器が壊れを見逃さないことの保証）。

| 壊し方 | 返るべき `rule` |
|---|---|
| メタの `magic` / `version` / `root` / `fastroot` | `meta.magic` / `meta.version` / `meta.root` |
| 内部ページの 2 つの項目を入れ替える | `page.order` |
| 葉の項目を high key より大きくする | `page.hikey` |
| 葉の `next` を別のページにする（`prev` が合わない） | `link.pair` |
| 兄弟のチェーンを循環させる | `link.cycle` |
| 内部ページから 1 つのダウンリンクを消す（葉が親から辿れない） | `tree.downlink_missing`（または `tree.orphan`） |
| 子の high key の 1 バイトを変える | `tree.child_hikey` |
| 子の項目が親の区切りの範囲の外 | `tree.bounds` |
| 予約フラグ（`DELETED` ほか）を立てる | `page.flags` |
| `ROOT` フラグを 2 つ目のページに立てる | `tree.root_flag` |
| ページを全 0 にする（辿れる） | `page.zero` |
| 内容のある辿れないページ | `tree.orphan`。全 0 の辿れないページは違反ではない |
| 行ポインタを `LP_DEAD` にする | `page.lp` |
| ヒープのタプルを削除（物理）／索引の項目のキーを書き換える／索引から項目を消す | `heap.dangling_entry` / `heap.key_mismatch` / `heap.missing_entry` |

また `validate_page` の同じ壊れ方が、降下・スキャン・挿入で `XX001`（`index "x" contains a corrupted page at block N`）になること（検査器を通さない経路）。

### 7.14 共有の SQL テスト・再起動テスト（K に依頼。`tests/slt/m4/index/`、`tests/restart/m4/`）

PostgreSQL 17 に流して期待値を作る（M1〜M3 と同じ規則。表名はファイルごとの接頭辞）。

| ファイル | 内容 |
|---|---|
| `index/unique_violation.slt` | 23505 のメッセージと DETAIL（単一列・複合・text・date・numeric、長い値が切り詰められない）、`CREATE UNIQUE INDEX` の重複（`could not create unique index`、`Key (a)=(1) is duplicated.`）、NULL の重複は可 |
| `index/unique_txn.slt` | §7.6 の表のうち SQL で書けるもの（トランザクション内の DELETE → INSERT、ROLLBACK、UPDATE のキー変更の物理順に依存しない形） |
| `index/large_key.slt` | §7.2 の境界。**圧縮されない値**を使う: 乱数の 16 進文字列のリテラルをスクリプトで生成して slt に埋め込む（`repeat('a', 3000)` のような繰り返しは PostgreSQL が圧縮して入ってしまい、圧縮しない yuzhu は `54000` になる。[06-Q19]）。エラーは `statement error` で SQLSTATE とメッセージ（`index row size 2712 ...`）を照合 |
| `index/many_rows.slt` | 数万行の INSERT・UPDATE・DELETE の後に、等値・範囲・`ORDER BY`・`ORDER BY ... DESC LIMIT`（04 が後ろ向きを使う場合）の結果が表の全走査と一致（`enable_indexscan` の on / off で同じ結果） |
| `index/null_order.slt` | 索引の ASC / DESC・NULLS FIRST / LAST と、`WHERE a > 5`・`a < 5`・`a IS NULL`（結果のみ） |
| `restart/m4/index_persist.slt` | 索引つきの表を作り、停止・再起動・`kill -9` の後に等値・範囲の検索と一意性が保たれる |

### 7.15 性能の確認（任意。B1・B2）

M4 は性能を目標にしないが、退行に気づけるよう次を測って `PROGRESS.md` に残す（合格の基準は置かない）: 連番・ランダムの 100 万件の挿入の時間と WAL の量（1 挿入あたり）、`build` の時間、全件スキャンの時間、最大の大きさのキー（分割が頻発する場合）、pgbench の tpcb-like（D-25）での完走と TPS（死んだ版の項目が増え続けることの影響。[06-Q11]）。

---

## 8. 実装の分担と工数

各担当は自分の範囲のファイルだけを編集する（00 §17）。他の担当の範囲で直すべき点は依頼する。担当の範囲は 00 §17 のとおり（`B1`: `storage/btree/{mod,page,tuple,meta,search,insert,split,wal}.rs` と `storage/index_store.rs`、`B2`: `storage/btree/{scan,build,unique,check}.rs` と `catalog/opclass.rs`、`H4`: `storage/heap/*`・`heap_store.rs`・`page.rs`（追加のみ））。

| 担当 | 日数 | 内容 | 依存 |
|---|---|---|---|
| **H4** | 1.5 | `page.rs` の追加（§4.2）0.3、`tuple.rs` の公開と新しい型の符号化（§4.4）0.5、`begin_scan_all`・`tuple_state`・`fetch_dirty`・`nblocks` とテスト（§7.12）0.7 | M3 の D・C。numeric・bpchar の変換関数は T1・T2（なければ新しい型の分岐は `0A000` のままにして先へ進む） |
| **B1** | 6 | (1) `tuple.rs`・`page.rs`・`meta.rs`・`wal.rs`（`BTREE_PAGES`、`log_pages`、REDO、`describe`）・`init_index` と固定値のテスト 1.0。(2) `search.rs`（比較・二分探索・降下・`with_covering_page`）と割り込みフック 1.0。(3) `insert.rs`・`wal.rs` の `INSERT_LEAF` と REDO・`index_store.rs` 1.5。(4) `split.rs`（`choose_split`、計画・確保・組み立て・適用、ルート分割、`find_parent`）2.0。(5) 性質テスト・分割の網羅・REDO の同値性・`54000` 0.5 | A、H4、M3 の W1・W2・C（§4.3 の口）。B2 の `check.rs`（テストで使う） |
| **B2** | 5 | `opclass.rs`（表・関数・`pg_proc` の行・テスト）1.0 → `check.rs` と負のテスト 1.0 → `build.rs` 1.0 → `scan.rs`（境界・前向き・後ろ向き・`walk_left`）1.5 → `unique.rs` 0.5 | B1 の (1)・(2)・(3)、H4 |
| M3 の C | 0.3 | `read_tree`・`write_tree`・`extend_tree`（§4.3） | なし（M3 の実装中に入れてもらうのが最良） |
| R2・K | 11 章 | ワークロード 6 と I13・I14、`tests/slt/m4/index/*` | B1・B2 |

**進め方**:

1. A が型と P0 のスタブ（`index_store.rs` の委譲、`btree/*` のスタブ、`catalog/opclass.rs` の型と空の表）を置く。M3 の C が §4.3 の 3 メソッドを入れる。
2. **H4 が最初の 0.3 日で `page.rs` の追加**を出す（B1 が使う。08 の SeqStore も `init_special` を使う）。同時に B2 が `opclass.rs` の `OPFAMILIES` / `AMOPS` / `operator_strategy` / `comparator` を先に出す（L2 のインデックス選択（04）が使う）。
3. B1 が (1)（形式・`init_index`・WAL）を出す（約 1.5 日）。B2 は `opclass.rs` の残りの後、`check.rs`（B1 のテストが使うので早めに）、続けて `build.rs`。
4. B1 が (2)（探索）、(3)（挿入）、(4)（分割）を出す。B2 が `scan.rs`（B1 の探索の後）、`unique.rs`（B1 の挿入の後）。
5. 結合: 07 の `build_from_heap` と `ddl/index.rs`、05 の `insert_with_indexes` と `IndexScan` が実物の `BtreeStore` に繋がる。R2 のワークロード 6 と I13・I14。K の slt。

**B1・B2 の確認用の足場**: テスト用の `IndexHandle`（`storage/btree/testing.rs`、`#[cfg(test)]`）。列の型・DESC・NULLS FIRST・`unique` を指定すると、`cmp = cmp_datum` の `IndexKeyColumn` を持つ `IndexHandle` を作る（`catalog::opclass` に依存しない。全 opclass の `cmp` が `cmp_datum` なので同じ結果）。`TestStorage`（M3 の `StorageStack` + `Wal`）の上に `BtreeStore` を作る補助、偽のヒープ（`fetch_dirty` の結果を表で与える `FakeHeap`）。

---

## 9. 未検証の点（実装前に確かめるもの）

- **`PageWriteGuard<'a>` が `'a` について共変であること**（§5.1 の「葉のガードを `Vec` に move する」）。`RwLockWriteGuard<'a, T>` は `&'a RwLock<T>` を持つので共変のはずだが、コンパイルで確かめる。だめなら葉のガードを `Vec` の外に置き、`log_pages` に 2 つの並びを渡す形にする（§4.5 の署名は `&mut [(BufferTag, PageWriteGuard<'_>)]` のまま、内部で並べ替える）。
- **`Wal::insert` の `FORCE_IMAGE | STANDARD` の組み合わせ**（穴を省いた画像）が M3 の実装で動くこと（M3 §6.3.1 の記述どおり。M3 の W1 の実装後に確認）。`KEEP_DATA` は使わない。
- **M3 の C が §4.3 の 3 メソッドを受け入れること**（受け入れられない場合の代案: B+Tree のテストだけ `cfg(debug_assertions)` の検査を `track.rs` の `thread_local` のフラグで外す。本体の検査の外し方は C に任せる）。
- **PostgreSQL の `_bt_findsplitloc`（重複キー・suffix truncation の考慮）との性能特性の差**。D6-4 の単純な規則で、同じキーが大量に重なるときに右端以外の分割が偏らないか、ページ数・WAL 量・挿入速度（§7.15）。
- **WAL の量**（未計測）。例 9 のルート分割で 8.5KB、非ルートの葉の分割で左 7.3KB + 右 0.9KB + 親（葉が 365 件ずつの連番で約 30 バイト / 挿入、ランダムで約 80 バイト / 挿入の見積り）。全画像を避けて差分にするのは、後からレコードの種類を足せば入れられる（00 §13.4 の予約）。
- **`cmp_datum` が新しい変種（numeric、bpchar、日時）を持つこと**（09 の T3）と、`types::cmp::cmp_with_nulls` が `cmp_column` と一致すること（§7.9 のテストで検出する）。
- **実機（PostgreSQL 17.11）での確認が残っているもの**: `pg_amop` / `pg_amproc` の全行と OID（§4.7。今回、本表のとおり照合した。`pg_opclass` の 10000 番台の OID は版で変わりうる）、`CREATE INDEX` の `opclass` を型に合わない列へ指定したときのエラー（07）。
- **一意性検査で、等値の連続が非常に長い（死んだ版が数万件）ときの性能**。ヒープを 1 件ずつ引く（§5.4。[06-Q17]）。pgbench の `pgbench_branches` で顕著になりうる（[06-Q11]）。
- **ヒープの `fetch_dirty` の細部**（PostgreSQL の `HeapTupleSatisfiesDirty` の xmin が自分で xmax が他人、`HEAP_XMAX_LOCK_ONLY`、MultiXact）。M4 は単一ライターで行ロックがないので、これらの枝は起きない。M5 で M3 の可視性と照合する。
- **`TIMESTAMP` と `DATE` の型をまたぐ比較**（09 [09-Q4] を採用した場合のみ）: 日付 → マイクロ秒の飽和の境界。
- **後ろ向きスキャンの `walk_left` が PostgreSQL の `_bt_walk_left` と同じ結果になること**は、M4 では削除がないので、右へ寄って `next == origin` を探す形で足りる（§5.9）。M5 の削除（`origin` が削除される）の分岐は PostgreSQL のソースから写す（未読）。
- **`BufferPool` の大きさ**: 分割が最大 `3h + 1` ページをピンし、§4.3 は余裕を含めて `shared_buffers >= 3h + 4` を想定する。`shared_buffers >= 16`（M2 の下限）で `3h + 4 <= 16` を満たすのは **h <= 4**（最悪の連鎖は 13 ページ + 余裕 3）。高さ 5 の最悪の連鎖は 16 ページ（`3h + 1`）で 16 フレームを使い切り、余裕の 3 が足りない（19 要る）。高さ 5 以上は `int4` 主キーで 10^10 行台なので M4 の用途では起きない。起きたときは `no unpinned buffers available` の `XX000` になる（レビュー対応 R-10。以前の「16 で高さ 5 が入る」は `3h + 1` だけを数えた記述で、§4.3 の `3h + 4` と矛盾していた）。

---

## 10. 確認事項

ユーザーの不在中に仮決めしたことです。仮決めのままでよいか確認してください。ディスク形式に関わるもの（★）は、実装の前に決めるのが望ましいです。`11-tests-plan.md` が `M4-Q` の通し番号に振り直して集約します。

### m4-btree §12 の各項目の決着

| 調査 §12 | 論点 | 決着 |
|---|---|---|
| 1 | 分割の WAL 方式 | 推奨どおり「1 回の挿入 = 1 レコード、全ページ画像」（`BTREE_PAGES`）。`MAX_BLOCK_REFS = 32`（M3 D4）。ブロック数の式を `3h + 1` に訂正し、高さの上限は動的（[06-Q3] ★） |
| 2 | suffix truncation と重複排除 | M4 では入れない。ピボットは切り詰めず、ディスク形式は PostgreSQL に近い形で予約（[06-Q4] ★） |
| 3 | 項目を消さない | そのとおり（D6-7）。影響と将来の緩和は [06-Q11] |
| 4 | opclass は組み込み型の既定のみ | そのとおり（`text_pattern_ops` などは入れない）。ただし `varchar_ops`・`name_ops` は入れ、型をまたぐ日時の比較は入れない（[06-Q9] ★） |
| 5 | CREATE INDEX はメモリ内ソートのみ | 上限なし（D-19）。外部ソートは M6（[06-Q15]） |
| 6 | `CREATE INDEX CONCURRENTLY` | 通常の CREATE INDEX と同じ扱い（00 §3。07） |
| 7 | カタログのインデックス | 作らない（D-23。07） |
| 8 | `ALTER TABLE ADD PRIMARY KEY / UNIQUE` | 含める（D-12。07 が `build` を流用） |
| 9 | シーケンスのコミット時 flush | `wal_flush_upto`（D-9。08） |
| 10 | M3 のヒープが行ポインタを再利用しない前提 | 明記した（D6-8、§5.10 の 6）。M5 の VACUUM で再設計 |
| 11 | 大きな値の圧縮 | 圧縮しない。`54000` の境界を実測で確認した（§7.2）。PostgreSQL が圧縮して入る値（繰り返しの多い文字列）は yuzhu では入らない（[06-Q19]） |

### 項目

- **★[06-Q1] ピボットの境界の向き（左の全項目 < 区切り <= 右の全項目）**
  - 仮決め: D6-1。区切り（親のピボットと左ページの high key）= 右ページの最初の項目そのもの（完全なキー + TID）。`K >= high key` で右へ、`P_i <= K` となる最後の子へ降りる。PostgreSQL は逆（区切りは左の非厳密な上限）。
  - 理由: 切り詰めない M4 では、PostgreSQL の向きだと「TID の最下位を 1 引く」特例が要る（offset 0 になりうる）。調査 §2.4 の記述とも一致。検査器の不変条件が単純。M5 で suffix truncation を入れても、切り詰めた属性を -∞ とみなす規則で矛盾しない。
  - 変えたい場合の影響: 降下・moveright・`find_first` の `strict` の向き、`page_covers`、検査器の `tree.bounds` を反転する。ディスク形式（ピボットの TID の意味）が変わるので、実装の前なら半日、後なら initdb のやり直し。

- **★[06-Q2] 空のインデックスでもルート葉を作る（メタの `root = 1`）**
  - 仮決め: 00 D-7 のとおり。`init_index` が「メタ + 空のルート葉」の 2 ページを `BTREE_PAGES`（`INIT`）で書く。調査 §2.2 の「`root = 0` で遅延作成」はとらない。
  - 理由: 挿入の途中でルートを作る分岐と、その REDO が要らない。07 の `EMPTY_INDEX_STATS`（`pages: 2`）と一致。PostgreSQL（空のインデックスは 1 ブロック）との差は `pg_relation_size` に見える（比べるテストは作らない）。
  - 変えたい場合の影響: 遅延作成にするなら、`init_index` がメタだけを書き、最初の挿入がルートを確保する（`BTREE_PAGES` の理由を 1 つ足す）。07 の `EMPTY_INDEX_STATS` と `pg_class.relpages` の値が変わる。工数は +0.5 日。

- **★[06-Q3] 構造変更は全画像の 1 レコード `BTREE_PAGES`。ブロック数は `3h + 1`、静的な高さの上限は作らない**
  - 仮決め: D6-6、D6-11。必要なブロック数が 32 を超えたときだけ `54000`（独自の文言）。00 §13.4 の「`2h + 3`、h > 14 で `54000`」と調査 §5.1 の「`3h + 2`、h <= 10」はどちらも不正確（各レベルで左・右・元の右隣の 3 ページ）。
  - 理由: 未完了の分割という状態がなく、クラッシュ試験の分岐が増えない（調査 §5.1）。実用上の高さ（int4 で 4、最大長のキーで 13）では全レベルが同時に満杯になることはない。
  - 変えたい場合の影響: PostgreSQL 方式（分割と親への挿入を別レコードにして `INCOMPLETE_SPLIT` を許す）にするなら +M（`INCOMPLETE_SPLIT` の設定・解除、`_bt_finish_split` 相当、クラッシュ試験の状態の追加）。`MAX_BLOCK_REFS` は 4 のままでよくなる。

- **★[06-Q4] ピボットの形（切り詰めなし、葉タプル + 8 バイト、TID は末尾の 6 バイト、high key の `block = 0`、-∞ ピボットは 8 バイト）**
  - 仮決め: §3.5。完全なピボットは右の最初の葉タプルの複製 + 末尾 8 バイト（最後の 6 バイトに TID）。
  - 理由: 大きさが `S + 8` と定まり、`BT_MAX_ITEM_SIZE`（2704）で「1 ページに high key + データ 2 件」が保証できる。バイト形式が決定的でテストしやすい。PostgreSQL の `BTreeTupleGetHeapTID`（ピボットの TID はタプルの末尾）と同じ配置。
  - 変えたい場合の影響: 切り詰めを入れる（属性数 < `ncols` のピボット）には、`cmp_key_item` の「切り詰めた属性は -∞」の枝（読み取りはすでに予約）、分割点の選択（`_bt_findsplitloc` の切り詰めの考慮）、`BT_MAX_ITEM_SIZE` の再計算（切り詰めたピボットは小さくなるだけ）。ページ形式は変わらない。+3 日。

- **[06-Q5] 分割点の規則（バイト数が均等。右端の葉への末尾への追記だけ左を 90% まで詰める）**
  - 仮決め: D6-4。PostgreSQL は右端のページなら位置に関わらず fillfactor を目標にするが、yuzhu は「末尾への追記」に限る。重複キーの考慮と suffix truncation の考慮はしない。連番では葉が 365 件（89.5%）。
  - 理由: 規則が単純で決定的。中ほどへの挿入で右ページがほぼ空になるのを避ける。
  - 変えたい場合の影響: `choose_split` だけ（ディスク形式は変わらない）。PostgreSQL と同じにするなら `追記の分割か` の条件から `newpos` を外す（0.1 日）。性能の差は §7.15 で測る。

- **★[06-Q6] 一括構築の詰め方（葉を全部作ってから上のレベル、葉 90%・内部 70%、閉じるときに最後の項目を次のページへ送る、32 枚ずつ `BTREE_PAGES`）**
  - 仮決め: D6-14、§5.8。ブロック 1（`init_index` が作った空の葉）を最初の葉として上書きし、レベルごとにブロック番号が連続する。
  - 理由: ページの確保が常に末尾への追加で、WAL のレコードが連続したブロックになる。「最後の項目を次へ送る」で high key に必ず空きができる。
  - 変えたい場合の影響: PostgreSQL の nbtsort のように全レベルを並行して作ると、ブロック番号が飛び飛びになり（`read_buffer_zeroed` で間を伸ばす）、バッチ化が複雑になる（+1 日）。ページの充填率・ブロックの並びが変わるだけでディスク形式の定義は変わらない。

- **[06-Q7] バッファプールへの依頼（`read_tree` / `write_tree` / `extend_tree`）**
  - 仮決め: §4.3。M3 の debug 検査（ブロック番号の昇順、`extend` はラッチなし）を B+Tree には適用しない口を足してもらう（0.3 日）。
  - 理由: 木の順序（下 → 上、左 → 右）は昇順ではない。ラッチの順序（M2 §5.9）自体は破っていない。
  - 変えたい場合の影響: 昇順に合わせるには、分割で全ページを昇順にラッチし直す（計画 → 解放 → 昇順に再ラッチ → 検証）構造にする。一意性検査で持つ最初の葉のラッチを手放す必要があり、D6-12 が崩れる。+1 日。`extend` は分割の前に（ラッチを持たずに）必要な最大数を確保する手もあるが、使わないページが残る。

- **[06-Q8] 23505 の DETAIL は B+Tree が付けない**
  - 仮決め: 05 D5-16 のとおり、`IndexStore::insert` は `23505` と `s` / `t` / `n` だけを返し、DETAIL（`Key (a)=(1) already exists.`）は 05 の `complete_unique_error` が `TypeEnv` で補う。`build(.., Yes)` も DETAIL を付けない（07 は `No` で呼び、自分で検出して DETAIL を作る。D07-7）。
  - 理由: `IndexStore::insert` に `TypeEnv` がなく、日時のキーを B+Tree が出力できない。
  - 変えたい場合の影響: B+Tree が DETAIL を作るなら、`UniqueCheck::Check` と `BuildUnique::Yes` に `&TypeEnv` を足す（00 §13.2 の変更。05・07 に波及）。

- **★[06-Q9] `datetime_ops` に型をまたぐ行を入れない**
  - 仮決め: 09 D-9-5 に従い、`date`・`timestamp`・`timestamptz` の同じ型どうしだけ（`pg_amop` が PostgreSQL より 30 行、`pg_amproc` が 6 行少ない）。
  - 理由: `timestamp` と `timestamptz` の相互の比較は `TimeZone` に依存し、`CmpFn`（純粋な関数）に環境を渡せない。09 がその演算子を作らない。
  - 変えたい場合の影響: 09 [09-Q4] で演算子を作るなら、06 は `AMOPS` に最大 30 行と `AMPROCS` に最大 6 行を足し、`CmpFn` が環境を持てるようにする（`TimeZone` を使う 4 組は `CmpFn` の署名の変更を伴う）。date × timestamp だけなら 10 行と 2 行で、`CmpFn` は変わらない。+1.5 日。

- **[06-Q10] `fetch_dirty` / `tuple_state` は「自分以外の実行中のトランザクション」を持たない**
  - 仮決め: D6-16。clog が `InProgress` のままの他者の XID は中断（クラッシュの残骸）とみなす。`WaitFor` / `InsertInProgress` / `DeleteInProgress` は M4 では返さない。
  - 理由: 単一ライターロックを持つトランザクションだけがヒープを書く（M2 §6.5.3 の `satisfies_update` と同じ規則）。
  - 変えたい場合の影響: M5（複数ライター）で `fetch_dirty` に実行中の判定（`&dyn Fn(Xid) -> bool`）を足す。M4 の B+Tree の呼び出し側は変わらない。

- **[06-Q11] 項目を消さないことの影響**
  - 仮決め: D6-7。同じキーの UPDATE を繰り返すと、キーが同じ項目（死んだ版）が増え続ける。一意性検査と等値のスキャンは、その全部をヒープで確かめる。
  - 理由: 項目の削除は WAL（`BTREE_DELETE`）・スキャンとの連動・ヒントビット（M3 D7）が要り、M5 の VACUUM と一緒に入れる。
  - 変えたい場合の影響: pgbench の tpcb-like（`pgbench_branches` は 1 行を数万回更新する）で、更新ごとに検査が遅くなる（完走はする。D-25 の条件は満たすが TPS は低い）。緩和策: (a) 挿入のときに、満杯の葉の中の「全員にとって死んでいる」項目を削除してから分割する簡易削除（`BTREE_DELETE` を足す。+2〜3 日。M5 の VACUUM の部品）、(b) ヒントビットなしで `LP_DEAD` を立てる（FPI が要り不可）。

- **[06-Q12] `BuildStats.levels` はルートの `level`（葉だけの木は 0）**
  - 仮決め: 07 の `EMPTY_INDEX_STATS`（`levels: 0`）に合わせる。木のレベル数（葉だけで 1）ではない。
  - 理由: 00 は値の意味を書いていない。PostgreSQL の `bt_metap.level` と同じ。
  - 変えたい場合の影響: `levels` を使うのは 07 の `pg_class` の更新だけ（表示用）。

- **[06-Q13] `pg_opclass` の OID（`bool_ops` 10003 など 10000 番台）**
  - 仮決め: PostgreSQL 17.11 の実機の値を写す。テストは `pg_opclass.oid` を PostgreSQL と比べず、名前で結合して比べる（07 と同じ）。
  - 理由: PostgreSQL が initdb で自動採番する値で、版・ビルドでずれうる。yuzhu のカタログの中で閉じていれば動作に影響しない。
  - 変えたい場合の影響: なし（表の数字を変えるだけ。`builtin_hash` が変わるので initdb のやり直し）。

- **[06-Q14] ページの破損の SQLSTATE は `XX001`**
  - 仮決め: 00 §13.5 のとおり。PostgreSQL の btree の破損（`XX002` `INDEX_CORRUPTED`）とは違う。メッセージは PostgreSQL に寄せる。
  - 理由: `error.rs` に `XX002` がなく、M4 に REINDEX もない。
  - 変えたい場合の影響: `error.rs` に `INDEX_CORRUPTED` を足し、`ctx.corrupted` / `zero_page` の SQLSTATE を替えるだけ（0.1 日）。

- **[06-Q15] CREATE INDEX は全件をメモリに持つ**
  - 仮決め: D-19 のとおり。`yuzhu.query_mem_limit` の対象外で、`build` の入力（07 が整列した `Vec`）を使う。`build` 自身は 1 ページ分 + 32 ページのバッチ + 区切りの並びだけを持つ。
  - 理由: 外部ソートは M6。
  - 変えたい場合の影響: 上限を設けるなら 07 の `build_from_heap` が `53200` を返す（06 は変わらない）。

- **[06-Q16] 分割の原子性を壊す変異用の `DebugKnobs`（任意）**
  - 仮決め: §7.11。`DebugKnobs::btree_split_in_two_records`（既定は偽。分割の連鎖を 2 本の `BTREE_PAGES` に分ける）を A の `debug_knobs.rs` に足し、クラッシュ試験の変異テストで I14 が検出することを確かめる。任意（作らなくても B+Tree 自体は動く）。
  - 理由: 「1 レコードで書く」ことがクラッシュ試験で守られていることを、壊した実装で確かめる（M3 D29 と同じ考え方）。
  - 変えたい場合の影響: 作らないなら変異テストの 1 行を省く。B1 に +0.3 日（2 本に分ける経路）。

- **[06-Q17] 一意性検査: 最初の葉の保持と、等値の連続の全件確認**
  - 仮決め: §5.4。最初の葉の排他ラッチを検査から挿入まで持ち、等値のキーの項目をすべてヒープで確かめる（`LP_DEAD` なし）。挿入先が右の葉になるときは非結合で右へ移る（M4 は書き手が 1 人）。
  - 理由: M5 の形に合わせつつ、M4 は単純にする。
  - 変えたい場合の影響: M5 で (1) 右の葉の排他ラッチを取ってから最初の葉を外す（結合）、(2) `WaitFor` を受けたらラッチを外して待つ、(3) 簡易削除で死んだ項目を減らす。

- **[06-Q18] 比較は項目を `Datum` に復号してから行う**
  - 仮決め: 調査 §3.1 の `fn(&[u8], &[u8])`（バイト列を直接比べる）ではなく、00 の `CmpFn(&Datum, &Datum)` に従う。1 回の比較で必要な列だけを先頭から復号する。
  - 理由: 型ごとの比較の規則を `cmp_datum` に一本化する（00 §4.3 の 2）。numeric・bpchar・日時が増えても B+Tree が変わらない。
  - 変えたい場合の影響: 性能が問題になったら、整数・text だけバイト列の直接比較を足す（`cmp_key_item` の中の局所的な最適化。形式は変わらない）。

- **[06-Q19] PostgreSQL が圧縮して入る値が `54000` になる**
  - 仮決め: yuzhu は TOAST・圧縮がない。PostgreSQL は 512 バイトを超えるキーを圧縮してから `BT_MAX_ITEM_SIZE` を検査するので、繰り返しの多い 3KB の文字列は PostgreSQL では入り、yuzhu では `54000`。差として許容し、共有テストは圧縮されない値（乱数の 16 進文字列）だけを使う（§7.14）。
  - 理由: M2-Q11（TOAST なし）の帰結。
  - 変えたい場合の影響: 圧縮を入れるなら TOAST と一緒に M5 以降。

- **[06-Q20] `default_opclass` にバイナリ互換の解決を含める**
  - 仮決め: 07 の依頼（a）に応じて、`default_opclass` が `varchar` → `text_ops`、`regclass` / `regtype` / `regproc` → `oid_ops` まで返す。07 の `coercible_index_type` は不要になる（残っていても害はない）。
  - 理由: 型と opclass の対応を 1 か所にする。
  - 変えたい場合の影響: 07 の `resolve_opclass` が補うだけ（06 の関数を素の型の一致に戻す）。

---

## 11. 00 への変更提案

統合時に反映してください。この章の署名は、ここに書いた変更を含めた形で書いてある（含めない場合の代替を括弧に添えた）。

1. **§13.4・§13.5 のブロック数と高さの記述**: 「木の高さ h の構造変更は最大 2h + 3 ページ。h > 14 は `54000`」を、**「最大 3h + 1 ページ（各レベルで左・右・元の右隣の 3 ページ、最上位がルートなら右隣がなく新ルートとメタが加わる）。静的な高さの上限は作らず、分割に必要なブロック数が `MAX_BLOCK_REFS` を超えたときだけ `54000`」**に直す（[06-Q3]）。
2. **§13.5 の表**: 次を足す。「ピボット: 完全なピボット = 右の最初の葉タプルの複製 + 末尾 8 バイト（最後の 6 バイトにヒープ TID、大きさ `S + 8`）。high key の `t_tid.block = 0`。内部ページの最初のデータ項目は 8 バイトの -∞ ピボット（属性数 0）」「ピボットの境界の向き: 左ページの全項目 < 区切り <= 右ページの全項目（D6-1。PostgreSQL と逆）」「予約フラグ: `LEAF` / `ROOT` / `META` 以外のビットが立っていたら `XX001`（00 の列挙は例）」「`BTREE_PAGES` の `reason`: 1 = INIT、2 = SPLIT、3 = BUILD」「`BTREE_INSERT_LEAF` の main data: `offnum: u16` + 予約 `u16`」。
3. **§13.2 の `BuildUnique` と `BuildStats`**: `BuildUnique::Yes` を「入力のすべてが生きている版として、隣接する項目のキー（NULL を含まない）が全列等しければ `23505`（DETAIL なし）」と定義する。07 は `No` だけを使う。`BuildStats.levels` = ルートの `level`（葉だけで 0。[06-Q12]）。`IndexStore::insert` の `23505` は DETAIL を付けない（`schema` / `table` / `constraint` のみ。05 D5-16、[06-Q8]）。
4. **§11.3 の opclass の表**: `AMOPS` = 120 行（`datetime_ops` は同じ型どうしだけ。09 D-9-5）、`AMPROCS` = 24 行、`pg_proc` に要る比較関数 = 24 個（07 の表のうち日時の型をまたぐ 6 個は不要）。`OPCLASSES` = 15 行（`varchar_ops`・`name_ops` を含む）。関数を足す: `default_opclass` のバイナリ互換の解決、`family_of_type`、`datum_type_oid`、`column_comparator`、`cmp_proc_name`（§4.7）。`AMPROCS` の `cmp` はすべて `cmp_datum`（09 と同じ）。
5. **§11.4**: `storage::btree::cmp_keys(index, a, b)` と `cmp_key_tid(index, a, a_tid, b, b_tid)` を公開する（07 の `build_from_heap` のソート用。07 の依頼）。`IndexKeyColumn` は変えない（`cmp` は `comparator(opfamily, opclass の入力型, 同じ)`、族は `family_of_type` / `default_opclass` から引く）。
6. **§13.1 の `TableStore`**: `tuple_state` は M4 では `InsertInProgress` / `DeleteInProgress` を返さない（クラッシュの残骸は `InsertAborted` / `Live`）。`fetch_dirty` は M4 では `WaitFor` を返さない。`begin_scan_all` は `SnapshotAny` で全バージョンを返す。いずれも M5 で実行中の判定を足す（[06-Q10]）。
7. **§17 と §4（担当と範囲）**: (a) M3 の C に `PinnedBuffer::{read_tree, write_tree}` と `BufferPool::extend_tree` を足す（§4.3）。(b) H4 の範囲に `heap/tuple.rs` の `encode_attr`・`ColumnCursor` の公開と新しい型（`Numeric`・`BpChar`・`Date`・`Timestamp`・`TimestampTz`・`Int2Vector`・`regclass`・`regtype`）の符号化を足す（09 が依頼済み）。`storage/page.rs` に `init_special` ほかを足す（§4.2。08 も使う）。
8. **§15.1 の定数**: §4.1 の追加分（`BT_PAGE_USABLE`、`BT_PIVOT_EXTRA`、`BT_MAX_PIVOT_SIZE`、`BT_BUILD_BATCH_PAGES`、`BT_MAX_LEVEL`、`INDEX_*_MASK`、`BTP_*`）。
9. **§4（モジュール）**: `storage/btree/testing.rs`（`#[cfg(test)]`。テスト用の `IndexHandle`、`FakeHeap`、割り込みフック）を足す。
10. **`debug_knobs.rs`（A）**: 任意で `btree_split_in_two_records: bool` を足す（[06-Q16]）。
11. **§18**: ワークロード 6 の `shared_buffers = 24`（分割が最大 `3h + 1` ページをピンするため。8 フレームのワークロード 5 とは別）。

**他の章・担当への依頼**:

| 宛先 | 依頼 |
|---|---|
| `07-catalog-ddl.md`（C1） | (a) `build` は `BuildUnique::No` で呼ぶ（D07-7 のとおり）。(b) `pg_proc` に足す比較関数は 24 個（日時の型をまたぐ 6 個は不要。§4.7）。(c) `IndexHandle::from_def` の `cmp` は `catalog::opclass::comparator(opfamily, opclass の入力型, 同じ)`、`IndexDef.columns[i].opfamily` は `opclass_by_oid(opclass).family`。(d) `storage::btree::cmp_keys` / `cmp_key_tid` を使ってよい。(e) `pg_opclass.opckeytype` は 0（`name_ops` も）のままでよい。(f) `BtreeStore` はリレーションごとのメモリ上の状態を持たない（`unlink_storage` と TRUNCATE の新しい relfilenode への切り替えで無効化するものはない） |
| `05-executor.md`（X3） | `IndexStore::insert` の `23505` は `s` / `t` / `n` つき・DETAIL なし（D5-16）。`ResolvedScanKeys` の意味は §5.9（`eq` の `None` は `IS NULL` のみ、範囲の列の NULL は一致しない、NULL の打ち切りは B+Tree が行う）。`IndexScan` は `scan_next` の間ピン・ラッチを持たない。`FakeIndexStore` の順序は `cmp_keys`（木の順序）と同じ（`cmp_with_nulls` の列ごとの適用 + TID） |
| `04-planner-optimizer.md`（L2） | `operator_strategy(op, family)` は型をまたぐ演算子（`int4` の列と `int8` の定数など）でも `Some((strategy, 左の型, 右の型))`。`ResolvedScanKeys` の値は右の型のまま渡してよい（`begin_scan` が `column_comparator` で解決する）。`datetime_ops` は同じ型どうしだけなので、`timestamp_col < timestamptz_const` はインデックス条件にならない。後ろ向きスキャンは §5.9 |
| `09-types-functions.md`（T1〜T3） | `cmp_datum` が numeric・bpchar・日時を正しく比べること、`cmp_with_nulls` が `btree::cmp_column` と一致すること（§7.9 のテストが検出）。`types::numeric::{encode_numeric, decode_numeric}`、`types::bpchar::decode_bpchar` を H4 が呼ぶ。bpchar・timestamp・timestamptz の同じ型どうしの比較演算子（OID は §4.7 の表）を `OPERATORS` に入れる |
| `08-sequence-serial.md`（Q1） | `Page::init_special(special_size)` は §4.2 の名前と意味で H4 が提供する |
| M3 の W2・R | `wal/dump.rs` が `RmgrId::Btree` で `storage::btree::wal::describe` を呼ぶ。`recovery::dispatch` に `RmgrId::Btree => storage::btree::wal::redo`（00 §4.1）。`RmgrId::from_u8(4)` を `Some(Btree)` にする（A） |
| M3 の C | §4.3 の 3 メソッド |
| `11-tests-plan.md`（K、R2、Z） | §7.11 のワークロード 6・I13・I14・変異テスト、§7.14 の slt、§7.15 の測定。`yuzhu-fuzz-sql`（実体は `tests/tools/difftest`。C-5） は索引ありの表に対する等値・範囲の問い合わせを含める |
