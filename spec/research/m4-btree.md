# yuzhu M4 設計調査: B+Tree インデックス、PRIMARY KEY / UNIQUE、シーケンスと SERIAL / IDENTITY

調査日: 2026-10-04。PostgreSQL は `REL_17_STABLE` ブランチのソースを取得して確認した（行番号は調査時点のもの。ブランチが進むとずれることがある）。
記号の意味: 【確認】は今回ソースを開いて確かめた事実、【記憶】は過去の知識に基づき今回は細部まで照合していない記述、【提案】は yuzhu 向けの推奨案。工数は S（〜2 日）/ M（〜1 週）/ L（2 週〜）の目安。

参照 URL の接頭辞: `PG = https://github.com/postgres/postgres/blob/REL_17_STABLE`

前提にした既存文書: `spec/research/m2-page-heap.md`（ページ形式・TID）、`m2-catalog.md`（予約 OID、pg_am）、`m3-wal.md`（WAL レコード形式、`MAX_BLOCK_REFS = 4`）、`m3-recovery.md`（REDO、FPW）、`m3-mvcc.md`（64 ビット xid、可視性）、`m3-tx-semantics.md` §7（シーケンスの方針）。

---

## 0. 結論（要約）

1. **nbtree をほぼそのまま写す**。8KB ページ、ブロック 0 がメタページ、各ページの special 領域（16 バイト）に左右リンク・レベル・フラグを置く。各ページに **high key**、各レベルに **right-link** を持つ Lehman & Yao 方式。M4 は単一ライターだが、読み手は分割と並行して走るので right-link による「右へ移動」は M4 から必須。
2. **キーは「ユーザーキー + ヒープ TID」で常に一意**にする（PG v4 の heapkeyspace と同じ）。重複キーは TID 順に並ぶ。これで分割点の扱い、一意性検査、将来の重複排除がすべて単純になる。
3. **インデックスタプルは PG の `IndexTupleData` と同じ並び**（TID 6 バイト + `t_info` 2 バイト + 任意の NULL ビットマップ + 列データ）。ピボット（内部ページの要素と high key）の符号化も PG と同じにして、**M4 では suffix truncation と重複排除（posting list）を実装しない**。ただし内部ページ先頭要素の「マイナス無限大」は必須。
4. **ページ分割の WAL は【提案】「1 回の挿入で変更した全ページを 1 レコードに入れる」方式**（PG は「分割レコード + 親への挿入レコード」の 2 段で、途中でクラッシュすると INCOMPLETE_SPLIT を後で直す）。yuzhu は分割の連鎖全体を 1 レコード・各ページの全画像で記録し、未完了分割という状態を作らない。そのため `MAX_BLOCK_REFS` を 4 → 32 に上げる。PG 方式（4 ブロックに収まる）は代替案として §5 に書く。
5. **削除は遅延**: M4 ではインデックスから項目を消さない（DELETE / UPDATE / アボートでもエントリは残り、ヒープ側の可視性判定で除外する）。ページ削除、LP_DEAD による掃除、空きページの再利用はすべて M5 の VACUUM。これにより M4 は「ページ削除がない」「TID が再利用されない」前提で読み手の手順を大幅に省ける。
6. **一意性検査は PG の `_bt_check_unique` の形を守る**（キーが入り得る最初の葉ページを排他ラッチしたまま、等しいキーの各 TID についてヒープを SnapshotDirty 相当で判定）。M4 は単一ライターなので「実行中の他トランザクションを待つ」分岐は起きないが、**API は `Conflict::WaitFor(xid)` を返せる形**にして M5 で待機を足す。
7. **比較は Rust のネイティブ比較関数で行い、カタログ（pg_opfamily / pg_opclass / pg_amop / pg_amproc）は 1 つの静的テーブルから生成して実体化**する。プランナはカタログを引かずに同じ静的テーブル由来のメモリ上の表を使う。テキストは C 照合（バイト順）のみ。
8. **CREATE INDEX はヒープ全走査 → メモリ上でソート → 葉から順に詰める一括構築**（PG の nbtsort と同じく葉 90%、内部 70%）。構築したページは全画像で WAL に記録する。外部ソートは後回し。
9. **PRIMARY KEY / UNIQUE は「一意インデックス + pg_constraint」**。名前（`t_pkey`、`t_a_key`）、エラー文言（`duplicate key value violates unique constraint "t_pkey"` / `Key (a)=(1) already exists.`、SQLSTATE 23505）を PG と完全に合わせる。
10. **シーケンスは 1 ページのリレーション（relkind 'S'）+ pg_sequence 行**。値の更新は MVCC を使わずその場で上書きし、WAL には 32 個先まで進めた状態を記録する（`SEQ_LOG_VALS = 32`）。SERIAL は「シーケンス + `DEFAULT nextval('t_a_seq'::regclass)` + NOT NULL + 所有関係」、IDENTITY は内部依存のシーケンス。
11. **インデックスオンリースキャンは M5 以降**（可視性マップが VACUUM で整備されてから）。

---

## 1. PostgreSQL nbtree の仕組み（調査結果）

### 1.1 Lehman & Yao と PG の差分（README）

`PG/src/backend/access/nbtree/README` 【確認】

- L&Y は各ページに **right-link** と **high key**（そのページに入り得るキーの上限）を追加する。降下中に「探しているキー > high key」なら、そのページは並行して分割されたので right-link を辿って右へ移る（何度でも）。これで読み手は親子ロックの連結（lock coupling）なしに降下できる。
- 内部ページの要素と葉の high key を **ピボットタプル**と呼ぶ。ピボットはナビゲーション専用で、ヒープを指さない。削除済みの値をキーに持っていてもよい。
- **ヒープ TID をタイブレーカー属性として扱うことで、全キーを一意にする**。L&Y はサブツリーの範囲を `Ki < v <= Ki+1` と定義し、Ki は厳密に小さい必要があるため。唯一の例外は葉の high key で、ページ最後の要素と完全に等しくてよい。
- PG は L&Y と違い、共有バッファを使うので **ページ単位の読み取りロック**を取る（ページを読む間だけ）。
- 前向きスキャンは right-link で進む。**後ろ向きスキャンのために left-link も持つ**。分割時は、分割するページの排他ロックを持ったまま元の右隣も排他ロックして left-link を直す（右方向なのでデッドロックしない）。
- スキャンは葉ページを見るとき、**一致する項目をまとめてローカルにコピー**してからロックを放す。項目は既存のページ境界を越えて動かない（分割は右へだけ移す）ので、取りこぼしも重複も起きない。スキャンは「読んだ時点の right-link」を覚えておき、そこへ進む（現在の right-link へ進むと分割で移った項目を二度読む）。
- ロックを持ったまま次のページをロックしてよいのは **右か上へ動くときだけ**（左・下は不可。デッドロック回避）。
- ルート分割: 普通のページと同じように分割し、両者を指す新しいルートを作ってメタページのルートポインタを書き換える。古いルートを読んだ探索者も right-link で正しく辿れる。
- 親への挿入で、降下時のスタックより上のレベル（その後ルートが分割された）が必要になったら、メタページから降り直して該当レベルを探す（各ページの level 番号で識別）。
- 親を探し直すときは、セパレータキーではなく **子のブロック番号が一致するダウンリンク**を探す（`_bt_getstackbuf`）。
- 可変長キーなので、ページあたりの件数は固定でない。分割は件数ではなく **バイト数が均等になる**ように選ぶ（新しい項目も計算に含める）。

### 1.2 ページ形式

`PG/src/include/access/nbtree.h` 【確認】

- special 領域 `BTPageOpaqueData`（`nbtree.h#L62-L69`）: `btpo_prev`（左隣、無ければ `P_NONE`=0）、`btpo_next`（右隣）、`btpo_level`（葉 = 0）、`btpo_flags`、`btpo_cycleid`（VACUUM サイクル ID）。計 16 バイト。
- フラグ（`#L76-L84`）: `BTP_LEAF` 1、`BTP_ROOT` 2、`BTP_DELETED` 4、`BTP_META` 8、`BTP_HALF_DEAD` 0x10、`BTP_SPLIT_END` 0x20、`BTP_HAS_GARBAGE` 0x40（非推奨）、`BTP_INCOMPLETE_SPLIT` 0x80、`BTP_HAS_FULLXID` 0x100。
- メタページ（ブロック 0、`BTREE_METAPAGE`）`BTMetaPageData`（`#L103-L119`）: `btm_magic`(=0x053162)、`btm_version`(=4)、`btm_root`、`btm_level`、`btm_fastroot`、`btm_fastlevel`、`btm_last_cleanup_num_delpages`、`btm_last_cleanup_num_heap_tuples`（非推奨）、`btm_allequalimage`。メタデータはページヘッダ直後（`PageGetContents`）。
- 行ポインタ番号: `P_HIKEY = 1`、`P_FIRSTKEY = 2`（`#L367-L368`）。**右端でないページは 1 番が high key**、データは 2 番から。右端ページは high key を持たず、データは 1 番から。
- **内部ページの最初のデータ要素は「マイナス無限大」**: `_bt_compare` は非葉ページの最初のデータ要素を常にスキャンキーより小さいとみなす（`PG/src/backend/access/nbtree/nbtsearch.c#L655-L680` のコメント）【確認】。v3 以降は実際に属性 0 個に切り詰めて保存する。
- 1 項目の最大サイズ `BTMaxItemSize`（`#L164-L168`）: 1 ページに 3 項目入るよう「(ページ − ヘッダと行ポインタ 3 個 − special) / 3 − TID 分」。8KB では **2704 バイト**（計算: (8192 − 40 − 16)/3 = 2712 → MAXALIGN_DOWN → − 8）。超えると `_bt_check_third_page` が SQLSTATE 54000 `index row size %zu exceeds btree version %u maximum %zu for index "%s"`（`nbtutils.c#L5082` 以降）【確認】。
- fillfactor（`#L199-L202`）: 既定 90（葉）、内部ページ 70、最小 10、単一値の葉 96。

### 1.3 インデックスタプル

`PG/src/include/access/itup.h#L35-L73` 【確認】

```
IndexTupleData (8 バイト)
  ItemPointerData t_tid   6 バイト (BlockIdData 2×u16 + OffsetNumber u16)
  uint16 t_info           bit15: NULL あり / bit14: 可変長あり / bit13: AM 定義 / bit0-12: タプル長
[NULL ビットマップ]       NULL ありのときだけ。INDEX_MAX_KEYS(32) ビット = 4 バイト
[列データ]                MAXALIGN した位置から、ヒープと同じ整列規則
```

- タプル長は 13 ビット（`INDEX_SIZE_MASK 0x1FFF`）なので 8191 が絶対上限。超えると `index row requires %zu bytes, maximum size is %zu`（`PG/src/backend/access/common/indextuple.c#L206-L210`）【確認】。
- nbtree はビット 13 を `INDEX_ALT_TID_MASK` として使い（`nbtree.h#L459-L466`）、立っていると `t_tid` は TID ではなく別の意味になる:
  - **ピボット**: `t_tid` のブロック番号部 = ダウンリンク（子のブロック番号）、オフセット部の下位 12 ビット = 残っているキー属性数、`BT_PIVOT_HEAP_TID_ATTR`(0x1000) = タプル末尾にヒープ TID がある。
  - **posting list**（重複排除）: `BT_IS_POSTING`(0x2000)。複数の TID をまとめた葉タプル。
- 通常の葉タプル（非ピボット）は `t_tid` = ヒープ TID そのもの。

### 1.4 探索と比較

- `_bt_search`（`nbtsearch.c#L96`）: メタページからルート（実際は fast root）を読み、各レベルで二分探索してダウンリンクを辿る。親の読み取りロックを放してから子をロックする（連結しない）【確認: `_bt_relandgetbuf` を使う】。
- `_bt_moveright`（`#L235`）: high key と比較し、`キー >= high key`（`nextkey` なら `>`）または削除・半死ページなら右へ。**書き込み目的で INCOMPLETE_SPLIT を見つけたらその場で分割を完了させる**（`_bt_finish_split`）【確認】。
- `_bt_compare`（`#L682`）の規則【確認】:
  - 属性ごとに比較関数（opclass の support proc 1）で比べ、DESC 列は結果を反転。NULL は値として順序付け（既定 NULLS LAST、`INDOPTION_NULLS_FIRST` で先頭）。
  - 全属性が等しく、タプル側が切り詰められて属性が少なければ「スキャンキー > タプル」（切り詰め = マイナス無限大）。
  - スキャンキーにヒープ TID（`scantid`）が無ければ 0（等しい）を返す（ただし前向きスキャンで切り詰めピボットと比べる特例あり）。
  - `scantid` があれば TID 同士を比較してタイを破る。
- 「挿入用スキャンキー」（全キー列 + scantid）と「検索用スキャンキー」（`WHERE` 由来の演算子付き条件）の 2 種類がある。`_bt_first`（`#L876`）は検索用キーから開始位置を求めるための挿入用キーを組み立てる。

### 1.5 挿入と一意性検査

`PG/src/backend/access/nbtree/nbtinsert.c` 【確認】

- `_bt_doinsert`（`#L102`）:
  1. 挿入用スキャンキーを作る。一意性検査をするなら、まず `scantid = NULL`（「そのキーが入り得る最初のページ」に着地させる）。
  2. **キーに NULL が含まれるなら一意性検査を飛ばす**（NULL は何とも等しくない。NULLS NOT DISTINCT でない場合）。
  3. `_bt_search_insert` で葉ページを排他ロックして得る。
  4. `_bt_check_unique` を呼ぶ。待つべき xid が返ったら、**ロックを放して `XactLockTableWait(xid)`（そのトランザクションの終了待ち）してから最初からやり直す**。
  5. 一意性が確定したら `scantid` に自分の TID を戻し、`_bt_findinsertloc` → `_bt_insertonpg`。
- 並行する同一キーの挿入をどう防ぐか（`#L163-L190` のコメント）: 「キーが入り得る最初のページ（TID = −∞）」の**排他ロックを、検査から挿入まで持ち続ける**。同じキーを入れようとする他者は同じページの排他ロックを取る必要があるので、検査は同時に 1 人しかできない。
- `_bt_check_unique`（`#L408`）:
  - `SnapshotDirty` を使う。等しいキーの各項目について、指す先のヒープタプル（HOT チェーン）を `table_index_fetch_tuple_check(..., &SnapshotDirty, &all_dead)` で調べる（`#L560`）。
  - 見つかったら: `SnapshotDirty.xmin`（挿入中のトランザクション）か `.xmax`（削除中のトランザクション）が有効なら、それを待つ xid として返す（`#L586-L597`）。
  - 有効でなければ確定した重複。ただし念のため、自分が入れようとしているヒープタプル自体が既に死んでいないか `SnapshotSelf` で確認（CREATE INDEX CONCURRENTLY のため）。生きていれば **SQLSTATE 23505** `duplicate key value violates unique constraint "%s"`、DETAIL `Key %s already exists.`、ErrorResponse にテーブル名・制約名（`errtableconstraint`）を付ける（`#L665-L673`）。
  - ヒープ側が全員にとって死んでいれば（`all_dead`）、その索引項目に LP_DEAD を立てる（ヒント扱い、`#L676-L691`）。
  - 等しいキーが右隣ページまで続くときは右へ進んで調べる（high key で続かないと判断できれば訪問しない）。
- 待ち方の種類: 通常は `XactLockTableWait`、`INSERT ... ON CONFLICT` の投機的挿入なら `SpeculativeInsertionWait`（`#L220-L227`）。

### 1.6 分割

- `_bt_findsplitloc`（`nbtsplitloc.c#L129`）: 左右のバイト数がほぼ均等になる点を基本とし、fillfactor（右端への連続挿入なら左を fillfactor まで詰める）、重複キーがページをまたがない点、suffix truncation で high key が短くなる点を考慮する【記憶: 戦略の詳細】。
- `_bt_split`（`nbtinsert.c#L1467`）: 左ページは一時ページに組み立ててから元ページへ戻す（`PageRestoreTempPage`）。新しい右ページを確保し、元の右隣の `btpo_prev` を書き換え、非葉なら子ページの INCOMPLETE_SPLIT を下ろす。左ページに `BTP_INCOMPLETE_SPLIT` を立てる。
- WAL（`#L1966-L2054`）【確認】: `XLOG_BTREE_SPLIT_L` / `_R`（新項目が左右どちらか）。ブロック参照は 0 = 元（左）ページ、1 = 新しい右ページ（`REGBUF_WILL_INIT`。中身はレコードに丸ごと含める）、2 = 元の右隣（left-link 更新）、3 = 子ページ（INCOMPLETE_SPLIT を下ろす。非葉のとき）。main data は `xl_btree_split { level, firstrightoff, newitemoff, postingoff }`（`nbtxlog.h#L153-L159`）。
- 親への挿入 `_bt_insert_parent`（`#L2099`）は **別の WAL レコード**（`XLOG_BTREE_INSERT_UPPER`、メタページ更新を伴えば `_META`）。親も満杯なら親の分割レコード、さらに上へ…と続く。
- ルート分割は `_bt_newlevel`（`#L2444`）: 新ルート（WILL_INIT）、左の子（INCOMPLETE_SPLIT を下ろす）、メタページの 3 ブロックで `XLOG_BTREE_NEWROOT`（`#L2569-L2593`）。
- **未完了分割**（README "WAL Considerations"）: 分割レコードと親挿入レコードの間でクラッシュすると、右ページへのダウンリンクが欠ける。探索は right-link で正しく動くが、その右ページがさらに分割されると親の挿入位置が見つからない。そこで左ページの INCOMPLETE_SPLIT を手掛かりに、**次に挿入しようとした人が親へのダウンリンクを足す**（`_bt_finish_split`、`#L2241`）。9.4 より前はリカバリ終了時に直していたが、複雑なのでやめた。
- WAL レコード種別一覧（`nbtxlog.h#L27-L43`）: `INSERT_LEAF` 0x00、`INSERT_UPPER` 0x10、`INSERT_META` 0x20、`SPLIT_L` 0x30、`SPLIT_R` 0x40、`INSERT_POST` 0x50、`DEDUP` 0x60、`DELETE` 0x70、`UNLINK_PAGE` 0x80、`UNLINK_PAGE_META` 0x90、`NEWROOT` 0xA0、`MARK_PAGE_HALFDEAD` 0xB0、`VACUUM` 0xC0、`REUSE_PAGE` 0xD0、`META_CLEANUP` 0xE0。

### 1.7 削除（M5 で必要になるもの）

README の "Deleting index tuples during VACUUM" 以降【確認】:

- 葉項目の削除（btbulkdelete）は**クリーンアップロック**（他にピンがない状態）を取ってから行う。これは「スキャンが葉ページのピンを持ったままヒープを訪問している間に、VACUUM がその TID のヒープ行ポインタを再利用する」ことを防ぐ連動（interlock）。
- VACUUM の物理順走査と並行分割の取りこぼしは `btpo_cycleid` で検出する。
- ページ削除は空になった葉からだけ、2 段階（半死: 親からダウンリンクを外す → 兄弟リンクから外して DELETED）。各段階が 1 つの WAL レコード。右端ページは削除しない（木の高さは減らない。代わりに fast root）。
- 削除済みページは、その時点のスナップショットがすべて消えるまで再利用できない（drain technique。削除時の次の xid を `BTDeletedPageData` に 64 ビット＝`FullTransactionId` で記録 → `BTP_HAS_FULLXID`）。
- 通常の挿入時にも、ページが満杯なら LP_DEAD 項目を消して分割を避ける「simple deletion」と、重複が多いときの「bottom-up deletion」がある（`_bt_simpledel_pass` `nbtinsert.c#L2812`、`_bt_bottomupdel_pass` `nbtdedup.c#L307`）。

### 1.8 一括構築（CREATE INDEX）

- `PG/src/backend/access/nbtree/nbtsort.c` の冒頭コメント【確認】: tuplesort でインデックスタプルをソートし、葉ページに順に詰める。ページが埋まるたびに親レベルへリンクを追加（必要なら親レベルを新設）。最後に各レベルの最終ページを閉じ、1 ページだけのレベルがルートになりメタページに登録する。**葉は fillfactor（既定 90%）、内部は 70%** まで詰める（満杯にすると最初の挿入から分割が連鎖するため）。
- PG17 では **bulk_write 機構（`storage/bulk_write.h`、`smgr_bulk_start_rel` / `smgr_bulk_write` / `smgr_bulk_finish`、`nbtsort.c#L1149-L1376`）でバッファキャッシュを経由せずに書き、ページを WAL に効率よく記録**する【確認: 呼び出し】。
- ヒープ走査（`heapam_index_build_range_scan`、`PG/src/backend/access/heap/heapam_handler.c#L1173`）は `HeapTupleSatisfiesVacuum` で各タプルを分類し、`DEAD` は索引しない、`LIVE` は索引して一意性検査対象、`RECENTLY_DEAD` は索引するが一意性検査対象外（HOT チェーンが壊れていれば `indcheckxmin`）、挿入中・削除中は他トランザクションなら待つ、などを決める（`#L1419-L1615`）【確認: 分岐の存在。細部は【記憶】】。一意性検査の対象外のタプルは別の spool（`btspool2`）に入れ、ソート時の重複検出に参加させない。
- ソート中に重複を見つけたら SQLSTATE 23505 `could not create unique index "%s"`、DETAIL `Key %s is duplicated.`（`PG/src/backend/utils/sort/tuplesortvariants.c#L1551-L1554`）【確認】。

### 1.9 演算子クラス

- btree の strategy 番号: 1 `<`、2 `<=`、3 `=`、4 `>=`、5 `>`。support 関数（`nbtree.h#L707-L712`）: 1 `BTORDER_PROC`（比較関数、int32 を返す。必須）、2 `BTSORTSUPPORT_PROC`、3 `BTINRANGE_PROC`（ウィンドウの RANGE 用）、4 `BTEQUALIMAGE_PROC`（重複排除可否）、5 `BTOPTIONS_PROC`【確認】。docs: <https://www.postgresql.org/docs/17/btree-behavior.html>、<https://www.postgresql.org/docs/17/xindex.html>【記憶: URL の節名】。
- **opfamily は型をまたぐ比較をまとめる単位**。例: `btree/integer_ops`（OID 1976）は int2/int4/int8 とその相互比較の演算子（pg_amop に 45 行）と比較関数（`btint4cmp`、`btint48cmp`、`btint42cmp` …）を含む。opclass は「ある入力型の既定の並べ方」で、`int4_ops`（OID 1978）、`int8_ops`（3124）、`text_ops`（3126）など【確認: `pg_opfamily.dat` / `pg_opclass.dat` / `pg_amop.dat` / `pg_amproc.dat`】。
- 主な btree opfamily の OID【確認】: `bool_ops` 424、`bpchar_ops` 426、`float_ops` 1970、`integer_ops` 1976、`numeric_ops` 1988、`oid_ops` 1989、`text_ops` 1994（text / varchar / name の opclass が属する）。`pg_am` の btree は 403。
- `pg_index`（OID 2610、`PG/src/include/catalog/pg_index.h`）の列: `indexrelid, indrelid, indnatts, indnkeyatts, indisunique, indnullsnotdistinct, indisprimary, indisexclusion, indimmediate, indisclustered, indisvalid, indcheckxmin, indisready, indislive, indisreplident, indkey(int2vector), indcollation(oidvector), indclass(oidvector), indoption(int2vector), indexprs, indpred`。`indoption` のビットは `INDOPTION_DESC` 1、`INDOPTION_NULLS_FIRST` 2（`#L87-L88`）【確認】。

### 1.10 シーケンス

`PG/src/backend/commands/sequence.c` 【確認】

- シーケンスは **1 行だけのリレーション**（relkind 'S'）。列は `last_value, log_cnt, is_called`。パラメータは `pg_sequence`（OID 2224: `seqrelid, seqtypid, seqstart, seqincrement, seqmax, seqmin, seqcache, seqcycle`）。
- 行は `xmin = FrozenTransactionId` で書き（`#L382-L387`）、以後 **MVCC を使わずその場で上書き**する。
- `SEQ_LOG_VALS = 32`（`#L58`）。`nextval_internal`（`#L703-L727`）は、`log_cnt` が足りないか、**ページ LSN が直近チェックポイントの REDO 位置以下（チェックポイント後の最初の nextval）**なら、32 個余分に進めた状態で WAL を書く（`XLOG_SEQ_LOG`、バッファは `REGBUF_WILL_INIT` でタプル全体を記録、`#L821-L850`）。WAL には「32 回先の状態」、ページには「実際の状態 + 残り log_cnt」を書く。→ **クラッシュ後に最大 32 個欠番**。
- WAL を書いた場合は `GetTopTransactionId()` で xid を割り当て、**コミット時に WAL フラッシュが起きる**ようにする（`#L798-L806`）。
- 上限超過は SQLSTATE 2200H `nextval: reached maximum value of sequence "%s" (%lld)`（`#L745-L749`）。`nextval()` / `setval()` は READ ONLY トランザクションで不可（`PreventCommandIfReadOnly`、`#L659`、25006）。
- ロールバックしても値は戻らない（docs: <https://www.postgresql.org/docs/17/functions-sequence.html>）。CACHE はセッションローカル（`elm->cached`）。

### 1.11 SERIAL と IDENTITY

- `PG/src/backend/parser/parse_utilcmd.c#L573-L668` 【確認】: `smallserial/serial2` → int2、`serial/serial4` → int4、`bigserial/serial8` → int8 に置き換え、`generateSerialExtraStmts` で `CREATE SEQUENCE` と `ALTER SEQUENCE ... OWNED BY t.col` を生成、列に `DEFAULT nextval('<schema>.<seq>'::regclass)` と `NOT NULL` を付ける。`serial[]` はエラー。「implicit sequence を作る」通知は DEBUG1（クライアントには出ない）。
- 所有関係の依存種別: SERIAL（OWNED BY）は `DEPENDENCY_AUTO`、IDENTITY は `DEPENDENCY_INTERNAL`（`sequence.c#L1600`）【確認】。
- IDENTITY: `GENERATED ALWAYS | BY DEFAULT AS IDENTITY [(seq options)]`、`pg_attribute.attidentity` が 'a' / 'd'。ALWAYS の列に明示値を INSERT すると SQLSTATE 428C9（`OVERRIDING SYSTEM VALUE` で回避）【記憶】。docs: <https://www.postgresql.org/docs/17/ddl-identity-columns.html>【記憶: ページ名】、<https://www.postgresql.org/docs/17/datatype-numeric.html#DATATYPE-SERIAL>。

---

## 2. yuzhu の推奨ページ形式

### 2.1 共通

- ページヘッダ・行ポインタは `m2-page-heap.md` §2.2–2.3 と同一（`pd_lsn` u64、チェックサムあり）。`pd_special = 8192 − 16 = 8176`。
- special 領域（16 バイト、LE）【提案】:

| オフセット（special 内） | サイズ | 名前 | 備考 |
|---|---|---|---|
| 0 | 4 | `btpo_prev` | 左隣。0 = なし（ブロック 0 はメタなので 0 を「なし」に使える） |
| 4 | 4 | `btpo_next` | 右隣。0 = なし（右端） |
| 8 | 4 | `btpo_level` | 葉 = 0 |
| 12 | 2 | `btpo_flags` | PG と同じビット割り当て（§1.2）。M4 で使うのは LEAF / ROOT / META のみ。他は予約 |
| 14 | 2 | `btpo_cycleid` | M4 は常に 0。M5 の VACUUM 用 |

- 読み込み時の検査（XX001 / XX002）: special の位置、未定義フラグ、`btpo_level` と LEAF の整合、メタページの magic。

### 2.2 メタページ（ブロック 0）

ページヘッダ直後（オフセット 24）から【提案】:

| オフセット | サイズ | 名前 | M4 の扱い |
|---|---|---|---|
| 24 | 4 | magic | 0x053162（PG と同じ値でよい） |
| 28 | 4 | version | yuzhu 独自の系列で 1 |
| 32 | 4 | root | ルートのブロック番号。空のインデックスは 0（= ルート未作成） |
| 36 | 4 | level | ルートのレベル |
| 40 | 4 | fastroot | M4 は root と同じ値を書く（ページ削除が無いので fast root は不要） |
| 44 | 4 | fastlevel | 同上 |
| 48 | 4 | last_cleanup_num_delpages | 0 |
| 52 | 1 | allequalimage | 重複排除を入れるまで false |

- `pd_lower` はメタデータの直後を指す（FPW の穴の省略が効くように。PG も同様【記憶】）。
- 空のインデックスは「メタページのみ」。最初の挿入でブロック 1 に葉兼ルートを作る（PG と同じく遅延作成【記憶】）。

### 2.3 葉ページと内部ページ

- 項目はキー順に行ポインタ配列へ並べる（途中挿入は行ポインタ配列をずらす。タプル本体は pd_upper から下へ詰める）。
- 右端でなければ行ポインタ 1 = high key、データは 2 から（`P_HIKEY`/`P_FIRSTKEY` の規約をそのまま採用）。
- 内部ページの最初のデータ要素はキー属性 0 個のピボット（マイナス無限大）。比較では内容を見ずに「必ず小さい」とする。

### 2.4 インデックスタプル【提案】

PG の符号化をそのまま採用する（将来の suffix truncation / posting list でディスク形式を変えずに済む）。

```
off 0  u32 LE  tid_block     非ピボット: ヒープのブロック番号 / ピボット: ダウンリンク（子のブロック番号）
off 4  u16 LE  tid_offset    非ピボット: ヒープの行ポインタ番号 / ピボット: 下位12ビット=キー属性数, 0x1000=末尾にヒープTIDあり
off 6  u16 LE  t_info        bit15 NULLあり, bit14 可変長あり, bit13 ALT_TID(=ピボット), bit0-12 タプル長
[off 8 4 バイト NULL ビットマップ]   NULL ありのときだけ（ビットが 1 = 非 NULL。ヒープと同じ向き）
列データ                      データ開始は 8 または 16（MAXALIGN）。各列の整列・符号化は m2-page-heap.md §2.5 と同じ
[末尾 6 バイト ヒープ TID]   ピボットで 0x1000 が立っているとき
```

- PG の `BlockIdData` は 2 個の u16（上位・下位）だが、yuzhu はヒープ側と合わせて u32 LE にする（バイト互換は狙わない方針、`m2-page-heap.md` §2.1）。
- **M4 のピボットは切り詰めない**: 分割時の high key と親のセパレータは「右ページ最初の項目のキー全列 + そのヒープ TID」を持つピボットにする。L&Y の不変条件（左ページの全項目 < high key ≤ 右ページの全項目）は満たされる。切り詰めは後で入れられる（ピボットの属性数フィールドがあるため）。
- タプル長の上限は PG と同じ `BTMaxItemSize` = 2704 バイト（8KB ページ）。超えたら 54000、メッセージも PG と同じ。**注意**: PG は大きな可変長値をインデックスタプル作成時に圧縮するので、PG では入る値が yuzhu では入らないことがある（yuzhu は TOAST / 圧縮なし）。テストではこの境界を突かない。
- インデックスに入れる値は、**列の型のまま**（ヒープと同じ符号化）。式インデックスは M4 対象外。

---

## 3. 比較関数と演算子クラス

### 3.1 比較の実装【提案】

- `fn compare(opfamily, left_type, right_type, a: &Datum, b: &Datum) -> Ordering` を **Rust のネイティブ実装**で持つ（fmgr 風の関数呼び出し層は作らない）。インデックスの列ごとに、作成時に解決した比較関数ポインタ（`fn(&[u8], &[u8]) -> Ordering` 相当）を relcache に持つ。
- 型ごとの注意:
  - **整数族**: int2/int4/int8 の相互比較は i64 に広げて比較（`btint48cmp` などと同じ結果）。
  - **float4/float8**: IEEE の比較ではなく **btree の全順序**（NaN は全ての非 NaN より大きく、NaN = NaN、−0 = +0）。`float8 = float4` の相互比較も族内で可能。
  - **text / varchar / name / bpchar**: C 照合のみ。`memcmp` → 短い方が小さい。bpchar は末尾空白を無視して比較（PG の `bpcharcmp`）【記憶】。テストを PG 実機で流す際は C ロケールの DB で比較する（`research-slt.md` の方針どおり）。
  - **numeric**: 符号・桁を考慮した比較（NaN が最大）。
  - **bool**: false < true。
  - **NULL**: 比較関数には渡さず、外側で `indoption` に従い先頭/末尾に置く。
- DESC 列は結果を反転し、NULLS FIRST/LAST は独立に扱う（`DESC` の既定は NULLS FIRST）。

### 3.2 カタログに何を実体化するか【提案】

| カタログ | M4 で入れる | 理由 |
|---|---|---|
| `pg_am` | 既存（heap 2, btree 403） | M2 で作成済み |
| `pg_index` | 必須 | `\d`、`\di`、ドライバ・ORM のメタデータ取得 |
| `pg_opfamily` | 対応型の btree 族のみ | `pg_opclass.opcfamily` の参照先。`\dAf` |
| `pg_opclass` | 対応型の btree 既定 opclass（`opcdefault = t`）。必要なら `text_pattern_ops` 等は後回し | `pg_index.indclass`、`CREATE INDEX ... (col int4_ops)` の名前解決、`\d` の表示 |
| `pg_amop` | 対応型（同型 + 族内の相互型）の strategy 1–5 | プランナが「どの演算子がインデックスで使えるか」を PG と同じ根拠で判断できる。`\dAo` |
| `pg_amproc` | support 1（比較関数）のみ。2, 4 は関数が無いので入れない（入れるなら pg_proc も要る） | `\dAp`。比較関数の pg_proc 行（`btint4cmp` など）も同時に入れる |

- **単一の静的テーブル**（Rust の `const` 配列）に「opfamily OID / 名前、opclass OID / 名前 / 入力型、(左型, 右型, strategy) → 演算子 OID、比較関数 OID → Rust 関数」を書き、**initdb のカタログ行生成とプランナ・エグゼキュータの両方をここから導く**。プランナは実行時にカタログを引かない（M4 はユーザー定義 opclass を作らないので、カタログとメモリ表が食い違うことはない）。
- OID は PG と同じ値を使う（§1.9 の値、および `pg_amop.dat` / `pg_amproc.dat` / `pg_operator.dat` の OID）。行の生成スクリプトは PG の `.dat` を手で写すか、開発時ツールで抽出する（ビルド時に PG ソースへ依存しない）。
- 範囲: M1 の型（bool、int2/4/8、float4/8、numeric、text、varchar、bpchar、name、oid）＋ M4 時点で入っている日付時刻型。それ以外の型の列に CREATE INDEX したら PG と同じく 42704 `data type %s has no default operator class for access method "btree"`【記憶: 文言】。

---

## 4. 探索・挿入・分割（yuzhu の手順）

### 4.1 ラッチと並行性（M4: 単一ライター + 複数リーダー）

- ページのラッチはバッファプールの `RwLock`（M2 の設計）をそのまま使う。
- **読み手**: メタページを読んでルートへ。各レベルで「子をラッチする前に親を放す」（PG と同じく連結しない）。子に着いたら high key と比較し、必要なら right-link で右へ（`_bt_moveright`）。葉では一致する項目の TID と「読んだ時点の right-link」をまとめてコピーし、ラッチを放してからヒープを訪問する。
- **書き手**（M4 は常に 1 人）: 降下は読み手と同じ（内部は共有ラッチ、葉だけ排他）。降下時のスタック（各レベルのブロック番号と、たどったダウンリンクの位置）を記録。分割で親へ上るときは子の排他ラッチを持ったまま親を排他ラッチする（上方向なのでデッドロックしない）。
- デッドロックしない根拠: 読み手は常に 1 ページしかラッチを持たない。書き手は「下 → 上」「左 → 右」の順にしか追加でラッチしない（PG README の規則と同じ）。M5 で書き手が複数になってもこの順序は保たれる。
- M4 では木を変更するのは書き手 1 人なので、**上昇時にスタックがずれることはない**。ただし M5 に備え、親を探し直す処理は PG の `_bt_getstackbuf`（子のブロック番号でダウンリンクを探し、無ければ右へ）と同じ形で書いておく【提案】。

### 4.2 挿入

1. ヒープに挿入（`m2-dml-exec.md` の流れ）→ 各インデックスに挿入（PG の `ExecInsertIndexTuples` と同じ順序）。
2. 一意インデックスなら §6 の検査。
3. 葉に空きがあれば項目を追加して WAL `BTREE_INSERT_LEAF`（ブロック 1 個 + 行ポインタ番号 + タプル）。
4. 空きが無ければ分割（§4.3）。
- UPDATE は（HOT が無いので）新しい版の TID で **全インデックスに新項目を入れる**。キー列が変わらなくても入れる（PG も非 HOT 更新では同じ）。
- 例外でトランザクションがアボートしても、入れた項目はそのまま（ヒープの xmin がアボートなので見えない）。

### 4.3 分割【提案】

- 分割点: 「左右のバイト数がほぼ等しい」を基本に、**右端ページへの挿入（連番キー）なら左を fillfactor（90%）まで詰める**単純な規則だけ入れる（SERIAL の主キーで木が半分スカスカになるのを防ぐため）。重複キーの考慮や suffix truncation の考慮は後回し。
- 手順（例外安全のため、ページを書き換える前に全部計算する）:
  1. 必要な新ページをすべて先に確保する（ファイル拡張。M4 は空きページ再利用が無いので常に末尾に追加）。確保が失敗したらここでエラーにでき、木は無傷。
  2. 影響するページ（左、新しい右、元の右隣、親、親の分割で生じる新ページ、…、新ルート、メタページ）の **新しい画像をメモリ上で組み立てる**。
  3. クリティカルセクション: 画像をバッファへコピー → dirty → WAL レコード 1 本を挿入 → 全ページの `pd_lsn` を設定 → ラッチ解放。
- 新しい右ページの right-link = 元の right-link、元ページの right-link = 新ページ、元の右隣の left-link = 新ページ。high key は §2.4 の規則。
- 親に「新しい右ページへのダウンリンク（セパレータ = 左ページの新しい high key）」を、左ページのダウンリンクの直後に入れる。親が満杯なら同様に分割して上へ。
- ルートが分割されたら新ルート（level + 1）を作り、メタページの root / level / fastroot / fastlevel を更新、旧ルートの `BTP_ROOT` を下ろす。

---

## 5. WAL

### 5.1 推奨: 1 回の挿入を 1 レコードにする【提案】

| レコード | ブロック参照 | 内容 | REDO |
|---|---|---|---|
| `BTREE_INSERT_LEAF` | 1（葉） | 行ポインタ番号 + タプル | 指定位置へ挿入（FPW があれば画像を復元） |
| `BTREE_SPLIT_CHAIN` | 2〜N（影響した全ページ） | 各ブロックの**全画像**（穴は省く） | 各ページを画像で置き換えるだけ |
| `BTREE_NEWPAGES` | 1〜32 | 一括構築したページの全画像 | 同上（§7） |
| `BTREE_META_INIT` | 1（メタ） | 空インデックスの作成 | メタページを初期化 |

- 分割は挿入 100 回程度に 1 回なので、全画像でも WAL 量の増加は 1 挿入あたり数百バイト程度（未計測。M4 のベンチで確認する）。差分形式（PG の `xl_btree_split`）への最適化は後からレコード種別を足せばよい。
- 利点: **未完了分割という状態が存在しない**ので、INCOMPLETE_SPLIT の検出・修復（`_bt_finish_split`）、リカバリ時の特別扱い、クラッシュ試験の分岐がすべて不要。木の検査器（§9）の不変条件も「全ページにダウンリンクがある」と強く書ける。
- 必要な変更: `m3-wal.md` の `MAX_BLOCK_REFS = 4` を **32 に上げる**（PG の `XLR_MAX_BLOCK_ID` と同じ上限）。木の高さ h のとき最悪 3h + 2 ブロック程度（各レベルで左・右・右隣、+ 新ルート + メタ）なので、h ≤ 10 まで収まる。8KB ページで h が 10 を超えるのは非現実的（int4 キーで 4 段でも数十億件【記憶: 概算】）。
- 欠点: 分割が親まで連鎖する間、下位ページのラッチを持ち続ける（PG は子の INCOMPLETE_SPLIT を下ろした時点で子を放せる）。M4 は書き手 1 人なので問題にならない。M5 でも分割の連鎖は稀で、読み手はラッチを 1 つしか持たないので影響は小さい。

### 5.2 代替: PG 方式（2 段レコード + INCOMPLETE_SPLIT）

- 分割レコード（ブロック 0 左、1 右、2 元の右隣、3 子）は 4 ブロックに収まり、`MAX_BLOCK_REFS = 4` のままでよい（`m3-wal.md` はこれを想定していた）。
- 代わりに、`BTP_INCOMPLETE_SPLIT` を立てる・下ろす処理、挿入時の `_bt_finish_split`、`_bt_moveright` での検出、未完了分割を作るクラッシュ試験が必要。工数 +M。
- **判断材料**: PG のコードをそのまま写せる安心感 vs. 状態数の少なさ。yuzhu はテストの網羅（決定的シミュレーションでのクラッシュ点の全探索、`m3-recovery.md` §9）を重視しているので、状態の少ない 5.1 を推奨。

### 5.3 ファイル作成と REDO の注意

- インデックスのファイル作成（CREATE INDEX）はヒープと同じく「作成をコミットまで保留削除に登録」（`m2-catalog.md` §4）。WAL にはファイル作成のレコード（SMGR_CREATE 相当、`m3-recovery.md` §7.4）を書く。
- REDO で対象ブロックがファイル末尾の外なら拡張してから画像を置く（`m3-wal.md` §末尾の方針どおり）。分割・構築レコードは全画像なので、拡張後の内容は確定する。

---

## 6. 一意性検査（MVCC 下）

### 6.1 M4 の手順【提案】

```
fn check_unique(index, key, my_tid, snapshot_ctx) -> Result<(), UniqueError>
  if key に NULL を含む && !nulls_not_distinct: return Ok   // PG と同じ
  葉 = 「key, TID=-∞」が入り得る最初の葉を排他ラッチ
  for 項目 in 葉から key と等しい項目を順に（high key を越えたら右隣へ。右隣は排他ラッチを追加取得）:
      match heap.satisfies_dirty(項目.tid):
        Invisible            => continue（必要なら全員にとって死んでいるかを覚える。M4 では何もしない）
        Visible{in_progress_xid: Some(x)} if x != 自分 => return Err(WaitFor(x))   // M4 では起きない
        Visible{..}          => 自分の挿入したヒープタプルがまだ生きているか確認（SnapshotSelf 相当）
                                 生きていれば 23505 を返す
  // ラッチを持ったまま挿入位置（key, my_tid）を探して挿入する
```

- `satisfies_dirty` は PG の `HeapTupleSatisfiesDirty`（`PG/src/backend/access/heap/heapam_visibility.c#L743`）を 64 ビット xid で写す。要点【記憶: 細部】: xmin がアボート → 見えない／xmin が自分 → xmax が無効なら見える、xmax が自分なら（自分で削除済み）見えない／xmin が実行中の他人 → 見える（`snapshot.xmin = xmin` を返す）／xmax がコミット済み → 見えない／xmax が実行中の他人 → 見える（`snapshot.xmax` を返す）。**コマンド ID は見ない**ので、同じ文で先に入れた行とも衝突する（PG と同じ）。
- **M4 で待機が起きない理由**: グローバル書き込みロックにより、書き込みトランザクションは同時に高々 1 つ（`m3-tx-semantics.md` §5）。実行中の他トランザクションが作ったヒープ版はインデックスに存在しない。したがって `WaitFor` は返らないが、**型としては用意しておき、M4 では `unreachable!` ではなく内部エラー（XX000）にする**。
- **M5 での追加**: `WaitFor(x)` を受けたらラッチを全部放し、x のトランザクションロック（M5 の行ロック基盤）で終了を待ち、降下からやり直す（PG `_bt_doinsert` の `goto search`）。「最初の葉の排他ラッチを検査から挿入まで持ち続ける」規則で並行挿入の競合を防ぐ。

### 6.2 PG と挙動を合わせるべき点（ユーザーに見える）

- **検査は行ごとに即時**（`indimmediate = true`、DEFERRABLE は M4 対象外）。そのため `UPDATE t SET id = id + 1` は物理順によって 23505 になる（PG でも同じ）。共有テストではこの結果に依存する書き方を避ける。
- 同じトランザクション内で `DELETE` → 同じキーで `INSERT` は成功する（旧版の xmax が自分 → dirty で見えない）。
- 同じキーへの UPDATE（キー以外の列を変更）は成功する（旧版は自分が削除済み）。
- アボートされた行と同じキーは入れられる。
- エラー: SQLSTATE 23505、`duplicate key value violates unique constraint "t_pkey"`、DETAIL `Key (a)=(1) already exists.`（複数列は `Key (a, b)=(1, x) already exists.`）。ErrorResponse に `s`（スキーマ）、`t`（テーブル）、`n`（制約名）フィールドを付ける。値の表示は各型の出力関数による（長い値の扱いなど細部は PG の `BuildIndexValueDescription` に従う【記憶】）。

### 6.3 SnapshotDirty で見えるが今のスナップショットでは見えない行

- Read Committed でも、自分のスナップショット取得後にコミットされた行と衝突すれば 23505 になる（PG と同じ。一意性は「コミット済みの最新状態」に対する制約）。M4 は単一ライターなので、自分のスナップショット取得後に他人がコミットすることはあり得ない（書き込みロック取得前のスナップショットを使っている場合は除く。`m3-tx-semantics.md` §5.1 で「書き込みロック取得後にスナップショットを取り直す」かどうか決めている側と整合させる）。

---

## 7. CREATE INDEX（一括構築）

### 7.1 手順【提案】

1. 構文: `CREATE [UNIQUE] INDEX [IF NOT EXISTS] [name] ON t [USING btree] (col [opclass] [ASC|DESC] [NULLS FIRST|LAST], ...)`。名前省略時は PG の命名（`t_a_idx`、衝突したら `t_a_idx1` …、`ChooseRelationName`）【記憶】。
2. グローバル書き込みロックを取る（DDL）。カタログに pg_class（relkind 'i'、relam 403）、pg_index、pg_attribute（インデックスの列）行を入れ、テーブルの `relhasindex = true`。
3. ヒープを全走査し、各タプルを分類:
   - xmin がアボート → 入れない。
   - それ以外（コミット済み・自分）→ 入れる。xmax がコミット済みでも、まだ古いスナップショットから見える可能性があるので入れる（PG の RECENTLY_DEAD と同じ扱い）。「全員にとって死んでいる」を判定する境界（oldest xmin）が M3 にあれば、それ未満で削除済みのものは入れなくてよい（最適化。M4 では全部入れて構わない）。
   - 一意性検査の対象は「今生きている版」（xmax が無効、またはアボート、または自分以外が削除中＝M4 では無い）だけ。削除済みの版は別リストに分けてソートし、重複検出に参加させない（PG の `btspool2`）。
4. `(key, tid)` で **メモリ上でソート**（`Vec::sort_by`）。一意なら隣接比較で重複を検出し、`could not create unique index "%s"` / `Key %s is duplicated.`（23505）。生きている版のリストと削除済み版のリストはマージしながら木に詰める。
5. 葉から順に詰める（葉 90%、内部 70%）。各レベルのページが埋まったら親レベルにピボットを追加。最後に 1 ページだけのレベルをルートにし、メタページを書く。
6. ページはバッファプール経由で新規ページとして書き、**32 ページずつ `BTREE_NEWPAGES`（全画像）で WAL に記録**する。PG17 の bulk_write のような「WAL を書かずに書いて fsync する」最適化は入れない（wal_level=minimal 相当が無いため）。
- メモリ上限: M4 は全件をメモリに載せる（`maintenance_work_mem` 相当の上限を超えたら 53200 で失敗させるか、黙って使うかは確認事項）。外部ソートは後回し（M）。
- `CONCURRENTLY`、式インデックス、部分インデックス（`WHERE`）、`INCLUDE`、`NULLS NOT DISTINCT` は 0A000（feature_not_supported）。

### 7.2 DROP INDEX

- `DROP INDEX [IF EXISTS] name [, ...]`。制約が所有するインデックスは PG と同じく 2BP01 `cannot drop index t_pkey because constraint t_pkey on table t requires it`（HINT 付き）【記憶: 文言】。ファイル削除はコミット時（保留削除）。

---

## 8. PRIMARY KEY / UNIQUE / シーケンス / SERIAL / IDENTITY

### 8.1 制約【提案】

- `PRIMARY KEY`（列制約・表制約）: 列に `attnotnull = true` を立て（PG17 は NOT NULL 制約行を作らない。PG18 から作る【記憶】）、一意インデックス `<table>_pkey` と `pg_constraint` 行（contype 'p'、`conindid` = インデックス、`conkey` = 列番号配列）を作る。`indisprimary = true`。テーブルに主キーは 1 つ（2 つ目は 42P16 `multiple primary keys for table "t" are not allowed`【記憶】）。
- `UNIQUE`: インデックス `<table>_<col>_key`（複数列は `t_a_b_key`）と contype 'u'。
- 依存関係: インデックス → 制約は `DEPENDENCY_INTERNAL`、制約 → テーブル列は `DEPENDENCY_AUTO`【記憶】。`pg_depend` を M4 で作る（SERIAL の所有にも要る）。
- NOT NULL 違反は既存（M1）の 23502 のまま。PK 列に NULL を入れたときのメッセージも 23502 `null value in column "a" of relation "t" violates not-null constraint`。
- 既存テーブルへの `ALTER TABLE ... ADD PRIMARY KEY / UNIQUE` は §7 の一括構築で作る（M4 で入れるかは確認事項）。

### 8.2 シーケンスの格納【提案】

- **PG と同じく 1 ページのリレーション**（relkind 'S'、ファイル `base/<db>/<relfilenode>`）。ページにはヒープタプル 1 個 `(last_value int8, log_cnt int8, is_called bool)` を `xmin = FrozenXid` で置き、special 領域にマジック（PG は `SEQ_MAGIC 0x1717`）。`SELECT * FROM t_a_seq` がヒープ走査でそのまま動く。
- パラメータは `pg_sequence` 行（§1.10）。`pg_class` 行、pg_type 行（シーケンスは行型を持つ【記憶】）。
- 更新は **ページの排他ラッチの下でその場上書き**（MVCC なし）。グローバル書き込みロックは取らない（`m3-tx-semantics.md` §7）。
- WAL: `SEQ_LOG`（ブロック 1 個、WILL_INIT、タプル全体）。`nextval` の判定は PG の `nextval_internal` をそのまま写す: `log_cnt < 必要数` または `!is_called` または「ページ LSN ≤ 直近チェックポイントの REDO 位置」なら、32 個先の状態を WAL に書き、ページには実際の状態と `log_cnt` を書く。REDO はタプルを置き換えるだけ。
- **コミット時のフラッシュ**: PG は xid を割り当ててコミット時のフラッシュを起こすが、yuzhu は xid 割り当てを書き込みロックと結びつけているので、代わりにトランザクションに「コミット時に、自分が書いた最後の WAL 位置までフラッシュする」フラグを立てる（`m3-tx-semantics.md` §7 の推奨どおり）。xid の無いトランザクションのコミットでもこのフラグがあればフラッシュする。これで「クライアントが受け取った値が、クラッシュ後に再度払い出される」ことを防ぐ。
- セッションローカル: CACHE 値の先取り（既定 1）、`currval`（未呼び出しなら 55000 `currval of sequence "%s" is not yet defined in this session`）、`lastval`（55000 `lastval is not yet defined in this session`）【記憶: 文言】。
- `setval(seq, v [, is_called])` も非トランザクション（WAL を書く）。上限・下限・CYCLE、`AS smallint|integer|bigint`（既定 bigint、SERIAL は integer）、上限到達 2200H。
- CREATE / DROP SEQUENCE、ALTER SEQUENCE（パラメータ変更）はトランザクショナル（カタログ変更として扱う）。PG の ALTER SEQUENCE はリレーションファイルを作り直す場合がある【記憶】が、yuzhu は M4 では「pg_sequence 行の更新 + 必要なら SEQ_LOG」で足りる。

### 8.3 SERIAL【提案】

- パーサ段階ではなく DDL 変換段階で、PG の `transformColumnDefinition` と同じ書き換え: 型を int2/int4/int8 に、`CREATE SEQUENCE t_a_seq AS <型>`（名前は `ChooseRelationName(table, col, "seq")`）、`DEFAULT nextval('t_a_seq'::regclass)`、`NOT NULL`、`pg_depend`（シーケンス → 列、AUTO）。
- `DROP TABLE` で所有シーケンスも消える（AUTO 依存）。`\d t` の Default 列に `nextval('t_a_seq'::regclass)` と出るには `pg_get_expr` と `regclass` の出力が要る（M4 の `\d` 対応と合わせる）。

### 8.4 IDENTITY【提案】

- `GENERATED {ALWAYS | BY DEFAULT} AS IDENTITY [(seq options)]`: シーケンスを内部依存（INTERNAL）で作り、`attidentity` に 'a'/'d'。DEFAULT 式は持たず、INSERT で値が省略されたら nextval。
- ALWAYS の列に明示値: 428C9 `cannot insert a non-DEFAULT value into column "a"`、DETAIL `Column "a" is an identity column defined as GENERATED ALWAYS.`、HINT `Use OVERRIDING SYSTEM VALUE to override.`【記憶: 文言】。`OVERRIDING {SYSTEM|USER} VALUE` も実装する（S）。
- `UPDATE` で ALWAYS 列を DEFAULT 以外にすると 428C9【記憶】。

---

## 9. インデックススキャンと検査

### 9.1 インデックススキャン（エグゼキュータ側の要点）

- 開始位置を求め（`_bt_first` 相当）、葉ごとに一致 TID をコピー、各 TID でヒープを読み、**スナップショットで可視性判定**して見える版だけ返す。インデックス自体に可視性情報は無い。
- UPDATE で同じキーの版が複数あっても、1 つのスナップショットから見える版は高々 1 つなので重複は出ない。
- M4 は「ページ削除なし・TID 再利用なし」なので、**スキャンが葉のピンを持ち続ける必要がない**（PG の VACUUM との連動は M5 で入れる）。ただし **M3 のヒープが行ポインタを再利用しないこと**（M5 の VACUUM まで）を前提にしている。M3 で HOT pruning や行ポインタの再利用を入れる場合は、この前提が崩れるので要再検討。
- 後ろ向きスキャン（`ORDER BY a DESC LIMIT n`）: left-link と「左へ移ったら右へ進んで元ページを指すページを探す」処理（README "Page deletion and backwards scans" の 1–3 段。削除が無いので 4 段目は不要）。工数 S。
- 検索条件として使えるのは、先頭列からの `=`, `<`, `<=`, `>`, `>=`（族内の相互型を含む）、`IS NULL` / `IS NOT NULL`、`BETWEEN`。`IN (...)` は M4 では使わない（PG17 は配列スキャンキーで使う）。プランナ・EXPLAIN 表示（`Index Scan using t_pkey on t` / `Index Cond: (a = 1)`）は別調査（M4 最適化）に委ねる。
- **インデックスオンリースキャン**は可視性マップ（全可視ビット）が要るので M5 の VACUUM 以降（docs: <https://www.postgresql.org/docs/17/indexes-index-only-scans.html>）。

### 9.2 木の検査器（amcheck 相当）【提案】

`bt_index_check` 相当を `yuzhu-storage` のテスト用関数として作り、全テストとクラッシュ試験の後に走らせる:

- ページ内の項目が `(key, tid)` で厳密に昇順、全項目 < high key（葉は ≤ も許す規則に注意）。
- 兄弟リンクの往復一致（`a.next = b ⇔ b.prev = a`）、同一レベル、右端のみ high key なし。
- 親のピボット区間と子の内容の整合、内部ページ先頭がマイナス無限大、全ページにダウンリンクがある（§5.1 を採用した場合）。
- インデックスの全 TID 集合 = ヒープの「索引されるべき版」集合（ヒープとの突き合わせ）。
- 加えて、`BTreeMap<(Key, Tid), ()>` をモデルにした性質テスト（proptest は外部クレートだがテスト用途なので可）。

---

## 10. 工数の目安

| 項目 | 工数 |
|---|---|
| ページ形式・タプル符号化・メタページ・検査器の骨格 | M |
| 比較関数（型ごと）と opclass 静的テーブル、カタログ行生成（pg_opfamily/opclass/amop/amproc/pg_proc の比較関数） | M |
| 探索・前向きスキャン・right-link（読み手）、後ろ向きスキャン | M（後ろ向き S を含む） |
| 挿入・分割・新ルート、WAL レコード（§5.1）と REDO | M〜L |
| 一意性検査（M4 版）と 23505 のメッセージ・フィールド | S |
| CREATE INDEX（一括構築、メモリ内ソート）/ DROP INDEX / pg_index・pg_class・pg_attribute | M |
| PRIMARY KEY / UNIQUE 制約、pg_constraint・pg_depend、命名規則 | M |
| シーケンス（リレーション、nextval/currval/lastval/setval、WAL、コミット時フラッシュ）、CREATE/ALTER/DROP SEQUENCE | M |
| SERIAL / IDENTITY の DDL 変換、OVERRIDING | S〜M |
| 試験（性質テスト、クラッシュ点の全探索、読み手と分割の並行試験、PG 実機との sqllogictest） | M |
| （任意）カタログのインデックス（予約 OID 2662 など）と CatalogTupleInsert の索引維持 | M |
| （後回し）suffix truncation / 重複排除 / LP_DEAD 掃除 / ページ削除 / 外部ソート | 各 M（M5 以降） |

合計の目安: 任意項目を除き L × 2〜3（6〜8 週相当、並列化で短縮可）。

---

## 11. M5 以降への配慮（M4 で予約しておくもの）

- special 領域の `btpo_cycleid`、フラグの HALF_DEAD / DELETED / HAS_FULLXID / INCOMPLETE_SPLIT のビットは予約して未使用。読み込み時に立っていたら（M4 では）破損として扱う。
- 一意性検査の `WaitFor(xid)`、親の探し直し（`_bt_getstackbuf` 相当）、`_bt_moveright` の「削除済みページなら右へ」分岐は M4 から書いておく。
- 削除済みページの xid は 64 ビット（yuzhu の xid がもともと 64 ビットなので PG の FullTransactionId と一致）。
- インデックスタプルの ALT_TID / posting ビットは PG と同じ意味で予約（重複排除を入れるときにディスク形式を変えない）。
- VACUUM 時のスキャンとの連動（クリーンアップロック、または PG の「ピンを持たないスキャンは `_bt_killitems` で LSN を確認する」方式）は M5 の設計課題。

---

## 12. 確認事項（ユーザーへの仮決め。QUESTIONS.md 候補）

1. **分割の WAL 方式**: 推奨は「1 挿入 = 1 レコード、全ページ画像」（§5.1、`MAX_BLOCK_REFS` を 32 に変更）。PG 方式（2 段 + INCOMPLETE_SPLIT）にするなら工数 +M。→ 推奨で進めてよいか。
2. **suffix truncation と重複排除を M4 で入れない**（ディスク形式だけ PG 互換にしておく）。インデックスが PG より大きくなる（特に重複の多い列）。
3. **M4 ではインデックスから項目を一切消さない**（LP_DEAD の掃除も M5）。DELETE / UPDATE を繰り返す負荷ではインデックスが膨らみ続ける。
4. **opclass は組み込み型の既定 opclass のみ**、`text_pattern_ops` などや `CREATE OPERATOR CLASS` は対象外。比較はネイティブ実装、カタログは静的テーブルから生成。
5. **CREATE INDEX はメモリ内ソートのみ**。大きなテーブルではメモリを使い切る可能性がある。上限を設けて 53200 で失敗させるか、上限なしにするか（推奨: 上限なし、外部ソートは M6 以降）。
6. **`CREATE INDEX CONCURRENTLY` は 0A000 にする**（単一ライターなので通常の CREATE INDEX として受け付ける案もある。PG 互換のテストで使われるなら後者）。
7. **カタログのインデックス（予約 OID）を M4 で作るか**。推奨: ユーザーインデックスを優先し、カタログ索引は M4 の最後か M5（カタログは小さく順次走査で足りる）。ただし OID カウンタ一周後の重複検査は索引前提（`m2-catalog.md` §4.3）なので、それまではキャッシュ全件走査で代用。
8. **`ALTER TABLE ADD PRIMARY KEY / UNIQUE` を M4 に含めるか**（推奨: 含める。CREATE INDEX の一括構築を流用でき S）。
9. **シーケンスのコミット時フラッシュをフラグ方式にする**（xid を割り当てない）。PG と違い、nextval だけのトランザクションは xid を持たない（`pg_current_xact_id_if_assigned()` などで差が見える可能性。未検証）。
10. **前提確認**: M3 のヒープは M5 の VACUUM まで行ポインタを再利用しない（HOT pruning を入れない）。この前提でインデックススキャンのピン保持を省いている。
11. **インデックスタプルの大きな値**: PG は圧縮で 2704 バイト超の値も入る場合があるが、yuzhu は圧縮しないので 54000 になる。差として許容し、テストでは境界を突かない。

---

## 13. 未検証事項（実装前に確認すること）

- `_bt_findsplitloc` の詳細な戦略（右端挿入の判定、重複の扱い）は【記憶】。§4.3 の単純規則で PG と性能特性が大きく変わらないか、ベンチで確認。
- 空インデックスのルート遅延作成、メタページの `pd_lower` 設定は【記憶】（`_bt_initmetapage` / `_bt_getroot` を要確認）。
- インデックス・シーケンス・制約の自動命名（`ChooseRelationName`、`ChooseConstraintName`）の衝突時規則、DROP INDEX の 2BP01 文言、IDENTITY の 428C9 文言、`currval` / `lastval` の 55000 文言は PG 17 実機で確認する。
- `HeapTupleSatisfiesDirty` の分岐の細部（特に xmin が自分・xmax が他人のケース）は M3 の可視性実装と照合する。
- `heapam_index_build_range_scan` の分類の細部（RECENTLY_DEAD と HOT チェーンの扱い）は yuzhu に HOT が無い前提で簡略化した。
- 全画像方式の WAL 量（§5.1）は未計測。
