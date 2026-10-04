# M2 調査: ページとヒープタプルのディスク上の形式

M2（永続化）で導入する 8KB ページとヒープタプルの形式について、PostgreSQL 17（REL_17_STABLE）の実装を調べ、yuzhu で採用する具体的なバイト配置を推奨する。M3（WAL・MVCC）、M4（B+Tree）、M5（VACUUM・FSM・VM）で**ディスク形式を変えずに済むこと**を最優先の設計目標とする。

- 前提: `spec/design/m1.md`（`Datum`、`SqlType`、`TableStore`、`RowId`、undo ログ方式のトランザクション）
- 参照したソース（すべて REL_17_STABLE。ヘッダは実際に取得して値を確認した）:
  - `src/include/storage/bufpage.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/storage/bufpage.h>
  - `src/include/storage/itemid.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/storage/itemid.h>
  - `src/include/storage/itemptr.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/storage/itemptr.h>
  - `src/include/access/htup_details.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/htup_details.h>
  - `src/include/access/heaptoast.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/heaptoast.h>
  - `src/include/access/sysattr.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/sysattr.h>
  - `src/include/varatt.h` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/varatt.h>
  - `src/backend/access/heap/hio.c` <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/hio.c>
  - ドキュメント「Database Page Layout」 <https://www.postgresql.org/docs/17/storage-page-layout.html>
  - ドキュメント「TOAST」 <https://www.postgresql.org/docs/17/storage-toast.html>
  - ドキュメント「Free Space Map」 <https://www.postgresql.org/docs/17/storage-fsm.html>、「Visibility Map」 <https://www.postgresql.org/docs/17/storage-vm.html>
- 「（未検証）」と書いた箇所は、ソースを直接確認していない記憶ベースの記述である。

---

## 0. 推奨の要約

| 項目 | 推奨 |
|---|---|
| ページサイズ | 8192 バイト固定（コンパイル時定数 `BLCKSZ`） |
| バイト順 | すべてリトルエンディアン固定（Linux x86_64 / aarch64 のみ対象のため） |
| ページヘッダ | PostgreSQL と同じ 24 バイト・同じフィールド並び（`pd_lsn` は u64 一つにまとめる） |
| 行ポインタ | 4 バイト。PostgreSQL と同じ `lp_off:15 / lp_flags:2 / lp_len:15` を、u32 のビット位置として明示的に定義 |
| タプルヘッダ | PostgreSQL と同じ 23 バイト + NULL ビットマップ + パディング（`t_hoff` は 8 の倍数） |
| 列の配置 | PostgreSQL の `typalign` に従う。短い varlena（1 バイトヘッダ）は整列しない |
| varlena | PostgreSQL のリトルエンディアン形式と同じ（1B ヘッダ / 4B ヘッダ。圧縮・外部参照のタグは予約のみ） |
| チェックサム | 常に有効。`pd_checksum` に格納し、書き出し時に計算・読み込み時に検証（ブロック番号を混ぜる） |
| TOAST | M2 では実装しない。タプルが `MaxHeapTupleSize`（8160 バイト）を超えたら PostgreSQL と同じ 54000 エラー |
| FSM | M2 では作らない。リレーションごとのメモリ上の「挿入先ページのヒント」+ 末尾ページへの追加。FSM フォークは VACUUM と一緒に M5 |
| VM | M5。`PD_ALL_VISIBLE` ビットとフォーク名だけ予約 |
| TID | `(block: u32, offset: u16)`。offset は 1 始まり、0 は無効。`RowId(u64)` には `block << 16 | offset` で詰める |
| UPDATE / DELETE | M2 から PostgreSQL と同じ「xmax を立てる + 新版を追加して t_ctid で繋ぐ」方式。可視性判定だけ M2 用の簡易版にし、ROLLBACK は M1 と同じ undo ログで「フラグを戻す」 |
| システム列 | `ctid`, `xmin`, `cmin`, `xmax`, `cmax`, `tableoid` を `pg_attribute` に負の attnum で登録（M2 で SELECT 可能にするのは推奨・任意） |

---

## 1. PostgreSQL のページ形式（調査結果）

### 1.1 全体構造

ドキュメント「Database Page Layout」の通り、ヒープページは次の 5 領域からなる。

```
0                24          pd_lower        pd_upper          pd_special   8192
+----------------+-----------+---------------+-----------------+------------+
| PageHeaderData | ItemId... | (free space)  | ...tuples       | special    |
+----------------+-----------+---------------+-----------------+------------+
                 行ポインタは前から伸びる    タプルは後ろから伸びる
```

- ヒープページは special 領域を使わない（`pd_special == BLCKSZ`）。B-Tree は special 領域に左右兄弟のリンクなどを置く（`nbtree.h` の `BTPageOpaqueData`）。
- 行ポインタ（`ItemIdData`）の配列番号が、TID のオフセット番号（1 始まり）になる。タプルをページ内で移動（断片化の解消）しても、行ポインタの番号は変わらないので TID は安定する。これが「行ポインタによる間接参照」の存在理由である。

### 1.2 PageHeaderData（bufpage.h）

| オフセット | サイズ | フィールド | 意味 |
|---|---|---|---|
| 0 | 8 | `pd_lsn` (`PageXLogRecPtr` = `{uint32 xlogid; uint32 xrecoff;}`) | このページを最後に変更した WAL レコードの次のバイトの LSN。WAL 先行書き込み（ページを書く前に、その LSN までの WAL を fsync）の判定に使う |
| 8 | 2 | `pd_checksum` | ページのチェックサム（データチェックサム有効時） |
| 10 | 2 | `pd_flags` | `PD_HAS_FREE_LINES 0x0001`、`PD_PAGE_FULL 0x0002`、`PD_ALL_VISIBLE 0x0004`（有効ビットは `0x0007`） |
| 12 | 2 | `pd_lower` | 空き領域の先頭オフセット（= 行ポインタ配列の末尾） |
| 14 | 2 | `pd_upper` | 空き領域の末尾オフセット（= 最も前にあるタプルの先頭） |
| 16 | 2 | `pd_special` | special 領域の先頭オフセット |
| 18 | 2 | `pd_pagesize_version` | 上位バイト = ページサイズ（8192 = 0x2000）、下位バイト = レイアウト版数（`PG_PAGE_LAYOUT_VERSION 4`） |
| 20 | 4 | `pd_prune_xid` | ページ内で最も古い「刈り取り可能かもしれない」XID。0 なら無し。HOT pruning の起動判定用 |
| 24 | - | `pd_linp[]` | 行ポインタ配列 |

- `SizeOfPageHeaderData = offsetof(PageHeaderData, pd_linp) = 24`。
- `PageIsNew()` は `pd_upper == 0` で判定する。全ゼロのページは「未初期化の新しいページ」として扱われる（ファイル拡張直後のクラッシュでゼロページが残りうるため）。
- `pd_lsn` が 2 つの uint32 に分かれているのは、構造体を 8 バイト整列させずに済ませるための歴史的事情（bufpage.h のコメント）。

### 1.3 ItemIdData（itemid.h）

```c
typedef struct ItemIdData {
    unsigned lp_off:15,   /* タプルへのオフセット（ページ先頭から） */
             lp_flags:2,  /* 状態 */
             lp_len:15;   /* タプルのバイト長 */
} ItemIdData;
```

| `lp_flags` | 値 | 意味 |
|---|---|---|
| `LP_UNUSED` | 0 | 未使用。`lp_len = 0`。再利用可能 |
| `LP_NORMAL` | 1 | 使用中。`lp_len > 0` |
| `LP_REDIRECT` | 2 | HOT チェーンの転送（`lp_off` に転送先オフセット番号、`lp_len = 0`） |
| `LP_DEAD` | 3 | 死んでいる。領域を持つ場合も持たない場合もある |

- 15 ビットのオフセット・長さは最大 32767 なので、BLCKSZ は最大 32KB。
- C のビットフィールドの配置はコンパイラ依存であり、PostgreSQL のファイルは同じアーキテクチャ間でしか互換性がない。yuzhu は**ビット位置を明示的に定義する**（後述）。

### 1.4 HeapTupleHeaderData（htup_details.h）

| オフセット | サイズ | フィールド | 意味 |
|---|---|---|---|
| 0 | 4 | `t_xmin` | 挿入したトランザクションの XID |
| 4 | 4 | `t_xmax` | 削除（またはロック）したトランザクションの XID。0 = 無効 |
| 8 | 4 | `t_cid` / `t_xvac` | 挿入・削除したコマンド ID（両方ある場合はコンボ CID）。`t_xvac` は 9.0 以前の VACUUM FULL 用で現在は使われない |
| 12 | 6 | `t_ctid` (`ItemPointerData`) | 自分自身、またはより新しい版の TID |
| 18 | 2 | `t_infomask2` | 下位 11 ビット = 列数（`HEAP_NATTS_MASK 0x07FF`）、`HEAP_KEYS_UPDATED 0x2000`、`HEAP_HOT_UPDATED 0x4000`、`HEAP_ONLY_TUPLE 0x8000` |
| 20 | 2 | `t_infomask` | 下表 |
| 22 | 1 | `t_hoff` | ヘッダ長（NULL ビットマップとパディングを含む）。MAXALIGN（8）の倍数 |
| 23 | - | `t_bits[]` | NULL ビットマップ（`HEAP_HASNULL` のときだけ存在。1 ビット = 1 列、**1 が非 NULL**） |

- `SizeofHeapTupleHeader = offsetof(HeapTupleHeaderData, t_bits) = 23`。NULL がなければ `t_hoff = MAXALIGN(23) = 24`。列数が 1〜8 で NULL ありなら、23 + 1 = 24 でやはり 24。9 列以上で NULL があると 32 になる。
- `t_infomask` のビット:

| 名前 | 値 | 意味 |
|---|---|---|
| `HEAP_HASNULL` | 0x0001 | NULL の列がある（ビットマップあり） |
| `HEAP_HASVARWIDTH` | 0x0002 | 可変長の列がある |
| `HEAP_HASEXTERNAL` | 0x0004 | TOAST 外部参照の列がある |
| `HEAP_HASOID_OLD` | 0x0008 | 旧 WITH OIDS（PG12 で廃止） |
| `HEAP_XMAX_KEYSHR_LOCK` | 0x0010 | xmax は FOR KEY SHARE のロック保持者 |
| `HEAP_COMBOCID` | 0x0020 | `t_cid` はコンボ CID |
| `HEAP_XMAX_EXCL_LOCK` | 0x0040 | xmax は排他ロック保持者 |
| `HEAP_XMAX_LOCK_ONLY` | 0x0080 | xmax はロックのみ（削除ではない） |
| `HEAP_XMIN_COMMITTED` | 0x0100 | ヒントビット: xmin はコミット済み |
| `HEAP_XMIN_INVALID` | 0x0200 | ヒントビット: xmin はアボート済み（COMMITTED と両方立つと FROZEN） |
| `HEAP_XMAX_COMMITTED` | 0x0400 | ヒントビット: xmax はコミット済み |
| `HEAP_XMAX_INVALID` | 0x0800 | ヒントビット: xmax は無効またはアボート済み |
| `HEAP_XMAX_IS_MULTI` | 0x1000 | xmax は MultiXactId |
| `HEAP_UPDATED` | 0x2000 | この版は UPDATE で作られた |
| `HEAP_MOVED_OFF` / `HEAP_MOVED_IN` | 0x4000 / 0x8000 | 9.0 以前の VACUUM FULL 用（現在は使われない） |

- `t_ctid` のコメント（htup_details.h）: 新しいタプルを格納するときは自分自身の TID を入れ、UPDATE されたら新しい版の TID に書き換える。チェーンをたどるときは、たどった先の `t_xmin` が元の `t_xmax` と一致するかを確かめる必要がある（その間に VACUUM されて別のタプルが入っている可能性があるため）。
- `ItemPointerData` は `BlockIdData {uint16 bi_hi; uint16 bi_lo;}` + `OffsetNumber ip_posid (uint16)` の 6 バイト。ブロック番号を 2 つの u16 に割っているのは、構造体を 2 バイト整列で済ませるため（itemptr.h / block.h）。

### 1.5 サイズの上限

- `MaxHeapTupleSize = BLCKSZ - MAXALIGN(SizeOfPageHeaderData + sizeof(ItemIdData)) = 8192 - MAXALIGN(28) = 8192 - 32 = 8160`。
- `MaxHeapTuplesPerPage = (BLCKSZ - SizeOfPageHeaderData) / (MAXALIGN(SizeofHeapTupleHeader) + sizeof(ItemIdData)) = 8168 / 28 = 291`。
- `MaxHeapAttributeNumber = 1600`（`t_hoff` が uint8 に収まるための上限から来ている）。
- タプルが `MaxHeapTupleSize` を超えると `RelationGetBufferForTuple`（hio.c）が `ERRCODE_PROGRAM_LIMIT_EXCEEDED`（54000）、`row is too big: size %zu, maximum size %zu` を出す（TOAST を試みたあとでも収まらない場合）。

### 1.6 整列（alignment）

- `MAXALIGN` は 64 ビット Linux で 8。タプルはページ内で 8 バイト境界から始まり、`pd_upper` は `MAXALIGN(lp_len)` ずつ下がる。`lp_len` 自体はパディングを含まない実長。
- 列の値はそれぞれの型の `pg_type.typalign` に整列する（オフセットは `t_hoff` 位置からではなく**タプル先頭から**の値だが、`t_hoff` が 8 の倍数なので同じこと）:

| typalign | 境界 | M1 の型 |
|---|---|---|
| `c` (char) | 1 | bool, name |
| `s` (short) | 2 | int2 |
| `i` (int) | 4 | int4, float4, oid, text, varchar |
| `d` (double) | 8 | int8, float8 |

- 例外: varlena で 1 バイトヘッダ（short varlena）に詰められた値は**整列しない**（`heap_fill_tuple` / `att_align_datum`。読み出し側は、パディングバイトが 0 であり、1B ヘッダの先頭バイトは必ず非ゼロであることを利用して、`att_align_pointer` で「ここが 0 ならパディングなので整列位置まで進む」と判定する）（実装の詳細は heaptuple.c と tupmacs.h、要点はドキュメント「Database Page Layout」の最終段落に記載）。
- PostgreSQL は各列の固定オフセットを `attcacheoff` にキャッシュし、「最初の NULL または可変長列」までは O(1) でアクセスする（heaptuple.c `nocachegetattr`）。

### 1.7 varlena（varatt.h、リトルエンディアン版）

| 先頭バイトのパターン | 種類 | 長さ |
|---|---|---|
| `xxxxxxx1`（ただし `0x01` ちょうどを除く） | 1B ヘッダ（短い値） | `(b >> 1) & 0x7F`（ヘッダ 1 バイトを含む、最大 127 = `VARATT_SHORT_MAX`） |
| `00000001`（`0x01`） | 1B_E: TOAST 外部参照ポインタ | 次の 1 バイトがタグ |
| `xxxxxx00` | 4B ヘッダ・非圧縮 | `(u32 >> 2) & 0x3FFFFFFF`（ヘッダ 4 バイトを含む、最大 1GB） |
| `xxxxxx10` | 4B ヘッダ・インライン圧縮 | 同上 |

- データ長 + 1 ≤ 127（データ 126 バイト以下）なら 1B ヘッダに詰める（`VARATT_CAN_MAKE_SHORT`）。ヘッダ値は `(len << 1) | 1`。
- 長さの単位はバイト。text の内容はサーバ符号化（yuzhu は UTF-8 固定）のバイト列で、終端の NUL は無い。

### 1.8 TOAST

- `TOAST_TUPLE_THRESHOLD = MaximumBytesPerTuple(TOAST_TUPLES_PER_PAGE=4)` = `MAXALIGN_DOWN((8192 - MAXALIGN(24 + 4*4)) / 4)` = `MAXALIGN_DOWN(2038)` = **2032 バイト**（heaptoast.h）。タプルがこれを超えると、まず圧縮可能な列を圧縮し、それでも超えれば大きい列から TOAST テーブルへ追い出して、`TOAST_TUPLE_TARGET`（同じく 2032）以下を目指す。
- TOAST テーブルは `pg_toast.pg_toast_<oid>` という別リレーション（chunk_id, chunk_seq, chunk_data）と、そのインデックスを持つ。つまり TOAST の実装には **B+Tree（M4）が事実上の前提**になる。
- 圧縮は pglz（PostgreSQL 独自実装）または lz4（ビルド時オプション）。

### 1.9 FSM と VM

- FSM（`<relfilenode>_fsm`）: 各ヒープページの空き容量を 1 バイト（BLCKSZ/256 単位）で記録した 3 段の木。INSERT は FSM で十分な空きのあるページを探す。FSM は VACUUM が更新する（ドキュメント「Free Space Map」）。FSM は WAL で保護されない「ヒント」であり、壊れても正しさには影響しない（未検証: freespace.c の README 記述による）。
- VM（`<relfilenode>_vm`）: ページごとに all-visible / all-frozen の 2 ビット。VACUUM のスキップと index-only scan に使う。ページ側の `PD_ALL_VISIBLE` と対になる。
- PostgreSQL 自身も、小さいテーブル（4 ページ以下）では FSM を作らず全ページを試す最適化を v12 で入れた（未検証: `HEAP_FSM_CREATION_THRESHOLD`。その後 v12 リリース前に取り消された可能性もある）。

### 1.10 チェックサム

- データチェックサムは initdb 時の `--data-checksums` で有効化する（PostgreSQL 17 までは既定で無効。18 で既定有効に変わったとされる、未検証）。
- アルゴリズムは `src/include/storage/checksum_impl.h` の FNV-1a 派生（32 レーン並列、SIMD 化しやすい形）。`pd_checksum` を 0 とみなして計算し、**ブロック番号を混ぜて**から 16 ビットに畳む。これで「別の位置に書かれたページ」も検出できる <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/storage/checksum_impl.h>。
- 検証失敗時は `ERRCODE_DATA_CORRUPTED`（XX001）、`invalid page in block %u of relation %s`（bufmgr.c、未検証: 文言の細部）。全ゼロのページは検証を通す（`PageIsVerifiedExtended`）。

---

## 2. yuzhu の推奨ページ形式

### 2.1 方針

1. **フィールドの意味と並びは PostgreSQL と同一**にする。理由: (a) M3 以降で PostgreSQL のアルゴリズム（可視性判定、HOT、VACUUM、FPW）をそのまま写経的に移植できる、(b) `pageinspect` 相当の出力や文献の図がそのまま読める、(c) 将来の拡張点（special 領域、`pd_prune_xid`、ヒントビット）が既に用意されている。
2. **バイナリ互換は狙わない**。PostgreSQL のデータディレクトリを読むことは要件ではない。そのため、C のビットフィールドやエンディアン依存の部分は、yuzhu では**明示的なリトルエンディアンのビット配置**として定義する。
3. **安全な Rust**（`#![forbid(unsafe_code)]`）では、構造体をページに重ねる（transmute）ことはできない。ページは `[u8; 8192]` とし、`u16::from_le_bytes(page[12..14].try_into())` のようなアクセサ関数で読み書きする。整列は CPU 的には意味を持たない（コピーで読むため）が、**サイズ計算と互換のために PostgreSQL の規則に従う**（2.5 節）。

### 2.2 ページヘッダ（24 バイト）

| オフセット | サイズ | 名前 | 型 | yuzhu での扱い |
|---|---|---|---|---|
| 0 | 8 | `pd_lsn` | u64 LE | M2 では常に 0 を書く。M3 で WAL の LSN を入れる。PostgreSQL の 2×u32 ではなく u64 一つにする |
| 8 | 2 | `pd_checksum` | u16 LE | **M2 から常に計算**（2.7 節） |
| 10 | 2 | `pd_flags` | u16 LE | `HAS_FREE_LINES=0x0001`、`PAGE_FULL=0x0002`、`ALL_VISIBLE=0x0004`。M2 では `HAS_FREE_LINES` のみ使う可能性あり。未定義ビットが立っていたら破損とみなす |
| 12 | 2 | `pd_lower` | u16 LE | |
| 14 | 2 | `pd_upper` | u16 LE | |
| 16 | 2 | `pd_special` | u16 LE | ヒープは 8192。B+Tree（M4）は special 領域にページ種別・兄弟リンク・レベルを置く |
| 18 | 2 | `pd_pagesize_version` | u16 LE | `0x2000 \| YUZHU_PAGE_LAYOUT_VERSION`。版数は **1** から始める（PostgreSQL の 4 とは別系列。将来形式を変えたら上げる） |
| 20 | 4 | `pd_prune_xid` | u32 LE | M2 では 0。M3/M5 で HOT pruning に使う |
| 24 | 4×n | 行ポインタ配列 | | |

不変条件（読み込み時に検査し、破れていたら XX001）:

- `24 <= pd_lower <= pd_upper <= pd_special <= 8192`
- `pd_special` は 8 の倍数、`pd_pagesize_version` の上位バイトが 0x20
- 全ゼロのページは「新しいページ」として正当（`pd_upper == 0` で判定）。読んだ側は使う前に初期化する。

### 2.3 行ポインタ（4 バイト、u32 LE）

| ビット | 名前 | 意味 |
|---|---|---|
| 0–14 | `lp_off` | タプルのページ内オフセット |
| 15–16 | `lp_flags` | 0 = UNUSED、1 = NORMAL、2 = REDIRECT、3 = DEAD |
| 17–31 | `lp_len` | タプル長（パディングを含まない） |

- GCC/x86_64 での PostgreSQL のビットフィールド配置と同じ並びになるはずだが（未検証）、yuzhu ではこの表を正とする。
- M2 で使うのは UNUSED と NORMAL のみ。DEAD は ROLLBACK された INSERT の後始末と M5 の VACUUM、REDIRECT は HOT（M5 以降）。

### 2.4 ヒープタプルヘッダ（23 バイト + ビットマップ + パディング）

| オフセット | サイズ | 名前 | 型 | 備考 |
|---|---|---|---|---|
| 0 | 4 | `t_xmin` | u32 LE | |
| 4 | 4 | `t_xmax` | u32 LE | 0 = 無効 |
| 8 | 4 | `t_cid` | u32 LE | コマンド ID（コンボ CID は M3 以降、2.9 節） |
| 12 | 4 | `t_ctid.block` | u32 LE | PostgreSQL の bi_hi/bi_lo の 2 分割はしない |
| 16 | 2 | `t_ctid.offset` | u16 LE | |
| 18 | 2 | `t_infomask2` | u16 LE | 下位 11 ビット = 列数、上位ビットは PostgreSQL と同じ意味で予約 |
| 20 | 2 | `t_infomask` | u16 LE | ビットの値・意味は PostgreSQL と同一（1.4 節の表）。0x0008、0x4000、0x8000 は使わない（立っていたら破損） |
| 22 | 1 | `t_hoff` | u8 | 8 の倍数 |
| 23 | ⌈natts/8⌉ | `t_bits` | | `HASNULL` のときだけ。列 i（0 始まり）の非 NULL がバイト `i/8` のビット `i%8` に 1 |
| … | | パディング | | 0 で埋めて `t_hoff` まで |

- **列数（natts）をタプルごとに持つ**のは重要。`ALTER TABLE ADD COLUMN`（M2 以降のどこか）で既存タプルを書き換えずに済む（タプルの natts より後ろの列は NULL、または `pg_attribute.atthasmissing` の値として読む）。
- `MaxHeapAttributeNumber = 1600` を yuzhu でも CREATE TABLE の上限にする（超えたら 54011 `tables can have at most 1600 columns`）。

### 2.5 列データの符号化（M1 の型）

| 型 | OID | typlen | typalign | ディスク上の表現 |
|---|---|---|---|---|
| bool | 16 | 1 | c | 1 バイト、0x00 / 0x01 |
| int2 | 21 | 2 | s | i16 LE |
| int4 | 23 | 4 | i | i32 LE |
| int8 | 20 | 8 | d | i64 LE |
| float4 | 700 | 4 | i | IEEE 754 binary32 のビット列（`f32::to_bits`）LE。NaN のビットはそのまま保存（正規化しない） |
| float8 | 701 | 8 | d | IEEE 754 binary64 のビット列 LE |
| text | 25 | -1 | i | varlena（1.7 節と同じ LE 形式）+ UTF-8 バイト列 |
| varchar | 1043 | -1 | i | text と同じ。typmod（長さ制限）はデータには入れない |
| name | 19 | 64 | c | 64 バイト固定、NUL 終端・NUL 埋め（カタログ用。最大 63 バイト） |
| oid | 26 | 4 | i | u32 LE |

- これらの typlen / typalign は `pg_type` の PostgreSQL の値と一致させる（M2 で `pg_type` をテーブル化するとき、`typlen`、`typbyval`、`typalign`、`typstorage` の列にそのまま出す）。
- 配置アルゴリズム（書き込み。`heap_fill_tuple` 相当）:

```
off = 0                                   // データ領域（t_hoff 以降）の先頭からのオフセット
for 各列 attr:
    if 値が NULL: ビットマップの bit を 0 にして continue
    if attr.typlen == -1:
        if データ長 + 1 <= 127:           // short varlena
            buf[off] = ((データ長 + 1) << 1) | 1; off += 1; データをコピー
        else:
            off = align(off, attr.typalign); 4B ヘッダ ((データ長 + 4) << 2) を u32 LE で書く; データをコピー
    else:
        off = align(off, attr.typalign); 固定長の値を書く
```

- 読み出し（`heap_deform_tuple` 相当）では、varlena 列の位置で `buf[off] == 0` なら整列パディングとみなして `align(off, 'i')` まで進め、そうでなければ（1B ヘッダの先頭バイトは必ず奇数なので）その場で読む。PostgreSQL の `att_align_pointer` と同じ規則。
- ページ内の書き込み済み領域は必ず 0 で初期化してから値を書く（パディングが 0 であることが上記判定の前提）。
- 4B ヘッダの圧縮パターン（下位 2 ビット = 10）と 1B_E（0x01）は M2 では**書かない**。読んで出会ったら XX001（データ破損）として扱う。TOAST・圧縮を入れるときに意味を与える。
- 「なぜ整列を守るのか」: 安全な Rust ではコピーで読むので速度上の利点はないが、(1) タプルのサイズが PostgreSQL と一致し、`pg_column_size()` や「何行入ると何ページになるか」「row is too big の境界」が PostgreSQL と同じになる、(2) 実装コストは `align()` 関数と上の分岐だけ、の 2 点で採用を推奨する。整列なしの詰め込み形式にする案は、ディスクを数 % 節約する以外の利点が乏しい。

#### 例: `CREATE TABLE t (a int4, b text, c int8, d bool)` に `(1, 'hello', NULL, true)`

- NULL があるので `HASNULL`、text があるので `HASVARWIDTH`。xmax は無効なので `XMAX_INVALID`。
- `t_infomask = 0x0001 | 0x0002 | 0x0800 = 0x0803`、`t_infomask2 = 4`
- ビットマップ: 列 a, b, d が非 NULL → `0b0000_1011 = 0x0B`、1 バイト。23 + 1 = 24 → `t_hoff = 24`

| タプル内オフセット | バイト | 内容 |
|---|---|---|
| 0–3 | `xmin` | |
| 4–7 | `00 00 00 00` | xmax = 0 |
| 8–11 | `cid` | |
| 12–17 | 自分の TID | t_ctid |
| 18–19 | `04 00` | t_infomask2 |
| 20–21 | `03 08` | t_infomask |
| 22 | `18` | t_hoff = 24 |
| 23 | `0B` | NULL ビットマップ |
| 24–27 | `01 00 00 00` | a = 1（4 境界） |
| 28 | `0D` | b の 1B ヘッダ = (5+1)<<1 \| 1 |
| 29–33 | `68 65 6C 6C 6F` | "hello"（整列なし） |
| — | | c は NULL なので領域なし |
| 34 | `01` | d = true（1 境界） |

- `lp_len = 35`、ページ上の占有は `MAXALIGN(35) = 40` バイト + 行ポインタ 4 バイト。

### 2.6 TID と RowId

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct BlockNumber(pub u32);      // 0..=0xFFFF_FFFE。0xFFFF_FFFF = InvalidBlockNumber
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct OffsetNumber(pub u16);     // 1..=MaxHeapTuplesPerPage(291)。0 = Invalid
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Tid { pub block: BlockNumber, pub offset: OffsetNumber }
```

- M1 の `RowId(u64)` は `(block as u64) << 16 | offset as u64` で `Tid` と相互変換する。順序（`Ord`）が「ブロック順 → オフセット順」= 物理順になるので、M1 で「挿入順に読める」としていた性質（の近似）がそのまま残る。
- テキスト表現は PostgreSQL の `tid` 型と同じ `(block,offset)`（例 `(0,1)`）。`tid` 型（OID 27、typlen 6、typalign s）は M2 でシステム列 `ctid` を出すなら `pg_type` に追加する。
- B+Tree（M4）の葉は値 + `Tid` を持つ。TID は行ポインタ番号なので、ページ内の断片化解消（compaction）でも変わらない。変わるのは UPDATE で新しい版ができたときだけ（そのときは新しいインデックスエントリを入れる。HOT は M5 以降）。

### 2.7 チェックサム

- **M2 から常に有効**にする（オプションにしない）。障害注入テスト（ビット反転・書き込みの欠落・別ブロックへの書き込み）を M2 から検出できるようにするため。
- アルゴリズムの推奨: PostgreSQL の `checksum_impl.h` と同じ FNV-1a 派生を手書きで移植する（約 100 行、外部クレート不要、ブロック番号を混ぜる手順まで含めて文書化されている）。代替案は CRC32C の手書き（テーブル方式で約 30 行）で、同じくブロック番号を XOR してから 16 ビットに畳む。どちらでも要件は満たす。PostgreSQL 版を選べば仕様の根拠を外部に求められる。
- 計算対象: `pd_checksum` を 0 にした 8192 バイト全体 + ブロック番号（リレーション内の通し番号。セグメント内番号ではない）。結果が 0 にならないよう調整する（PostgreSQL は `(checksum % 65535) + 1`）。
- 計算はバッファプールがページをディスクへ書き出す直前、検証はディスクから読み込んだ直後に行う。メモリ上では `pd_checksum` を気にしない。
- 検証失敗は SQLSTATE XX001、`invalid page in block N of relation base/D/R` 相当。M2 では回復手段はない（M3 の FPW で、クラッシュ由来の破れたページは WAL から上書き復元される）。

### 2.8 ページへの挿入と空き領域の管理

- ページ内の挿入（`PageAddItem` 相当）:
  1. 必要量 = `MAXALIGN(len)` + （再利用できる UNUSED の行ポインタがなければ）4。
  2. `pd_upper - pd_lower >= 必要量` かつ行ポインタ数 < 291 なら入る。
  3. `pd_upper -= MAXALIGN(len)`、タプルをコピー、行ポインタを `{off: pd_upper, flags: NORMAL, len}` で設定、新規なら `pd_lower += 4`。
- M2 で UNUSED の行ポインタが生じるのは「ROLLBACK した INSERT の後始末」の場合のみ（2.9 節）。M2 では行ポインタの再利用を**しない**ことを推奨する（一度使ったオフセット番号は VACUUM まで再利用しない、という PostgreSQL の性質に合わせ、TID の再利用によるバグを避ける）。
- **M2 の挿入先ページの選択は FSM なしで十分**:
  - リレーションごとに、メモリ上の `insert_target: Option<BlockNumber>`（最後に挿入したページ）を持つ。
  - そのページに入らなければ、リレーション末尾に新しいページを追加（ファイル拡張）してそこに入れる。
  - 再起動後は「最終ページ」から始める。
  - 理由: M2 には VACUUM がないので、削除や UPDATE で空いた領域は**そもそも再利用できない**（xmax が立っただけのタプルは物理的には残る）。FSM が役立つのは VACUUM が空きを作れるようになってから。PostgreSQL も FSM の更新は VACUUM の仕事である。
  - FSM（`_fsm` フォーク）は M5 で VACUUM と同時に入れる。それまでの間、ファイル名の規則に**フォーク番号（main=0, fsm=1, vm=2, init=3）を含める設計**だけは M2 で決めておく（`<relfilenode>`、`<relfilenode>_fsm`、`<relfilenode>_vm`、セグメントは `.1`, `.2`, ...、1 セグメント = 131072 ブロック = 1GB）。
- 1 タプルの上限: シリアライズ後の長さが `MaxHeapTupleSize = 8160` を超えたら、ページを触る前にエラー（54000、`row is too big: size %zu, maximum size 8160`）。
- `fillfactor`（ヒープの既定 100）は M2 では実装しない。

### 2.9 M2 での INSERT / DELETE / UPDATE と ROLLBACK

M2 には MVCC もコミットログもないが、**タプルヘッダの使い方は M2 から PostgreSQL と同じ**にし、変えるのは「可視性の判定関数」と「アボートの実現方法」だけにする。これで M3 では判定関数を差し替え、undo ログを削除するだけで済み、ディスク形式は変わらない。

#### トランザクション ID とコマンド ID

- XID は u32。`InvalidTransactionId = 0`、`BootstrapTransactionId = 1`（initdb が作るカタログの行の xmin）、`FrozenTransactionId = 2`、通常の XID は 3 から（PostgreSQL の `transam.h` と同じ値）。
- M2 から**書き込みを行うトランザクションに XID を割り当てる**。カウンタは制御ファイル（`global/pg_control` 相当）に永続化し、PostgreSQL と同様に一定量（例 1024）先まで予約して書いておき、再起動時は予約値から再開する（クラッシュしても XID を再利用しない）。周回（wraparound）対策は M5 の VACUUM/凍結まで扱わない。
- コマンド ID（CID）はトランザクション内の文ごとに 0, 1, 2, ... と増やす（M1 の `statement_start` と同じ単位）。

#### 各操作（M2）

| 操作 | タプルへの操作 |
|---|---|
| INSERT | `t_xmin = 自 XID`、`t_xmax = 0`、`t_cid = 現 CID`、`t_ctid = 自 TID`、`t_infomask` に `XMAX_INVALID` |
| DELETE | 対象タプルに `t_xmax = 自 XID`、`t_cid = 現 CID`（2.9.1）、`XMAX_INVALID` を外す。`t_ctid` は自分のまま |
| UPDATE | 旧版に DELETE と同じ操作 + `t_ctid = 新版の TID`。新版は INSERT と同じ操作 + `HEAP_UPDATED`。新版は旧版と同じページに入ればそこへ、入らなければ通常の挿入先へ |

- `HEAP_HOT_UPDATED` / `HEAP_ONLY_TUPLE` は M2 では立てない（インデックスがないので HOT の意味がない。M4/M5 で HOT を入れるかは別途判断）。
- **ハロウィーン問題**: `UPDATE t SET x = x + 1` の SeqScan は、同じ文が末尾に追加した新版に出会う。これを読まないために、M2 の可視性判定にも CID の比較が必要（PostgreSQL の `HeapTupleSatisfiesMVCC` の「自トランザクションが挿入した行は `cmin < curcid` のときだけ見える」部分）。

#### 2.9.1 M2 の可視性判定（M3 で差し替える）

M2 は M1 と同じく「変更は即座に反映、ROLLBACK は undo ログで戻す」方式とし、文の実行はデータベース全体で直列化（M1 と同じ単一 Mutex）する前提で、次の判定を使う。

```text
visible_m2(tup, my_xid, cur_cid):
    if tup.infomask & XMIN_INVALID: return false                 // ROLLBACK された INSERT
    if tup.xmin == my_xid and tup.cid >= cur_cid
       and xmax が無効: return false                             // この文以降で自分が挿入した行
    if xmax が無効 (xmax == 0 or XMAX_INVALID): return true
    if tup.xmax == my_xid and tup.cid >= cur_cid: return true    // この文自身が削除した行（PostgreSQL と同じく自分の文からは見える）
    return false                                                 // 削除済み（他トランザクションの未コミット削除も含む = M1 と同じダーティな挙動）
```

- 同じトランザクションで挿入して削除した行は、PostgreSQL ならコンボ CID（cmin と cmax の組をバックエンドのメモリ上の表に置き、`t_cid` にはその番号を入れる）が必要になる。M2 の範囲（直列実行、カーソルなし）では、削除するときに `t_cid` を削除側の CID で上書きするだけで正しく判定できる（削除できた時点で「挿入は現 CID より前」が確定しているため）。`HEAP_COMBOCID` ビットは M3 以降で使う。
- 他トランザクションの未コミット変更が見えるのは M1 と同じ既知の制約（QUESTIONS.md Q-003）。M3 でスナップショット + コミットログ + ヒントビットの判定に差し替える。

#### 2.9.2 ROLLBACK（undo ログ）

M1 の `UndoEntry` を TID ベースに拡張する。

| UndoEntry | 巻き戻し操作 |
|---|---|
| `Inserted { rel, tid }` | タプルに `XMIN_INVALID` を立てる（推奨）。行ポインタを DEAD にする案もあるが、M3 の挙動（アボートしたタプルは VACUUM まで残る）に合わせる |
| `Deleted { rel, tid, old_cid }` | `t_xmax = 0`、`XMAX_INVALID` を立てる、`t_cid = old_cid` に戻す |
| `Updated { rel, old_tid, new_tid, old_cid }` | 旧版を `Deleted` と同様に戻し、`t_ctid` を自 TID に戻す。新版に `XMIN_INVALID` |

- M2 には WAL がないので、ROLLBACK もコミットも**クラッシュ時には保証されない**（コミット後に全ページを fsync する簡易的な耐久化を M2 で入れるかは別の調査項目）。クラッシュ後に「undo しきれていない行」が残りうる点は M2 の既知の制約として QUESTIONS.md に記録すべき。
- M3 では undo ログを撤去し、アボートは「コミットログに aborted を記録する」だけになる。`XMIN_INVALID` / `XMAX_INVALID` は本来の「ヒントビット」の役割に戻る。M2 で立てた値は M3 の判定でも同じ意味（xmin アボート済み / xmax 無効）として正しく解釈されるので、**M2 で作ったデータファイルを M3 で読める**。

### 2.10 システム列

`pg_attribute` に PostgreSQL と同じ負の attnum でシステム列を登録する（sysattr.h）。

| attnum | 名前 | 型 |
|---|---|---|
| -1 | `ctid` | tid (27) |
| -2 | `xmin` | xid (28) |
| -3 | `cmin` | cid (29) |
| -4 | `xmax` | xid (28) |
| -5 | `cmax` | cid (29) |
| -6 | `tableoid` | oid (26) |

- psql の `\d` などのカタログ問い合わせは `attnum > 0` で絞り込むので、登録しておくことが互換性上は安全側（登録しないと `SELECT * FROM pg_attribute` の結果件数が PostgreSQL と異なる）。
- `SELECT ctid, xmin, xmax FROM t` を M2 で使えるようにすると、UPDATE/DELETE のテストとデバッグに有用。ただし `tests/` の期待値は PostgreSQL と一致させる必要があり、xid の具体値は環境依存なので、テストでは `ctid` の値すら（PostgreSQL と完全に同じページ詰めにしない限り）比較に使えない点に注意。M2 では「アナライザがシステム列を解決できる」ところまでを推奨とし、tid/xid/cid 型の追加は任意とする。

---

## 3. TOAST を M2 でどうするか

| 案 | 内容 | 工数感 | 評価 |
|---|---|---|---|
| A（推奨） | TOAST なし。8160 バイトを超える行は 54000 エラー。2032 バイト超〜8160 バイトの行はそのまま格納 | 小（数十行） | M2 の範囲で十分。ディスク形式は varlena のタグを予約しているので後から追加できる |
| B | インライン圧縮のみ（pglz 相当を手書き） | 中（圧縮器の手書きと検証で数日相当） | 圧縮の効く値だけ救える。PostgreSQL との境界の一致は依然として得られない |
| C | 本格 TOAST（toast リレーション + チャンク + インデックス） | 大。B+Tree（M4）が前提 | M6 以降が妥当 |

- 案 A の互換性への影響: PostgreSQL では `repeat('x', 100000)` のような文字列も（圧縮や TOAST で）格納できるが、yuzhu ではエラーになる。`tests/` の共有スイートは**1 行が約 8KB を超えるデータを使わない**ことをルールにし、超える場合は yuzhu 側でスキップする印（`skipif yuzhu`）を付ける。
- カタログ側は `pg_class.reltoastrelid = 0`、`pg_type.typstorage` は PostgreSQL と同じ値（text は `x`）を入れておく（値は形式上のもので、M2 では挙動に影響しない）。
- 注意: カタログ行（例: 長い CHECK 式のテキストを持つ `pg_constraint` / `pg_attrdef` の行）も 8160 バイトの制限を受ける。M1 の「DEFAULT・CHECK 式を SQL テキストで保存」（Q-006）は通常この上限に届かないが、上限に達したら同じ 54000 になる。

---

## 4. M3 以降のための予約事項（チェックリスト）

| 将来の機能 | M2 で用意しておくもの |
|---|---|
| WAL・WAL 先行書き込み（M3） | `pd_lsn`（M2 では 0）。バッファプールの書き出し経路に「書く前に `pd_lsn` まで WAL を flush する」フックを置ける構造 |
| full page write（M3） | チェックポイント後の最初の変更を検出するため、ページ単位で `pd_lsn` を比較できること（上と同じ） |
| MVCC・コミットログ（M3） | xmin/xmax/cid/ctid/ヒントビット（M2 から実データとして書く）、XID の永続カウンタ |
| B+Tree（M4） | `pd_special` による special 領域、TID の安定性（行ポインタの間接参照） |
| HOT・pruning（M5） | `pd_prune_xid`、`LP_REDIRECT` / `LP_DEAD`、`HEAP_HOT_UPDATED` / `HEAP_ONLY_TUPLE` |
| VACUUM・FSM・VM（M5） | フォーク番号つきファイル名、`PD_ALL_VISIBLE`、`PD_HAS_FREE_LINES` |
| 凍結（M5） | `HEAP_XMIN_FROZEN`（= COMMITTED \| INVALID） |
| 行ロック（M5、SELECT FOR UPDATE） | `HEAP_XMAX_LOCK_ONLY` などのロック用ビット、`HEAP_XMAX_IS_MULTI` |
| ADD COLUMN | タプルごとの natts |
| TOAST・圧縮（M6 以降） | varlena の 1B_E と 4B 圧縮のタグ、`HEAP_HASEXTERNAL` |

---

## 5. 実装への提案（モジュール分割の素案）

- `storage/page.rs`: `Page`（`Box<[u8; 8192]>`）と、ヘッダのアクセサ、`init_heap_page()`、`add_item()`、`get_item(offset) -> &[u8]`、`item_id(offset)`、`verify()`（不変条件 + チェックサム）、`set_checksum(blkno)`。ヒープ・B+Tree で共通。
- `storage/heap/tuple.rs`: `HeapTupleHeader` の読み書き、`form_tuple(desc: &[AttrDesc], row: &[Datum]) -> Result<Vec<u8>>`、`deform_tuple(desc, bytes) -> Result<Row>`、`AttrDesc { typlen: i16, typalign: u8, typbyval: bool }`。型ごとの符号化はここに閉じる（`types` に依存してよいが、`types` は `storage` に依存しない）。
- `storage/heap/visibility.rs`: `visible_m2()`。M3 で `HeapTupleSatisfiesMVCC` 相当に置き換える場所を 1 か所に集める。
- `storage/heap/mod.rs`: `heap_insert` / `heap_delete` / `heap_update` / `HeapScan`（ページ単位でバッファをピンし、可視タプルを順に返すカーソル。M1 の `scan()` の全件コピーを置き換える）。
- テスト: `form_tuple` → `deform_tuple` の往復（NULL の組み合わせ、126/127 バイト境界の text、整列境界をまたぐ列順）を property test で（`proptest` 等のテスト用クレートは CLAUDE.md の許容範囲内）。ページを詰め切ったときの `add_item` の境界（291 タプル、8160 バイト）。チェックサムの 1 ビット反転検出。§2.5 の例のバイト列を固定値テストにする。

---

## 6. QUESTIONS.md に記録すべき仮決め（候補）

1. M2 は TOAST なし。8160 バイト超の行は 54000 エラー。共有テストは 8KB 超の行を使わない。
2. データチェックサムは常に有効（PostgreSQL と違い無効化できない）。アルゴリズムは PostgreSQL と同じ FNV-1a 派生。
3. M2 の DELETE/UPDATE は xmax を立てるだけで物理的には消さない。VACUUM（M5）までファイルは縮まず、空きも再利用されない。
4. M2 は WAL がないため、クラッシュ時の一貫性は保証しない（ROLLBACK 途中・コミット途中の状態が残りうる）。
5. ページ形式はフィールドの意味は PostgreSQL と同じだが、バイナリ互換ではない（リトルエンディアン固定、ビット配置を明示、`pd_lsn` は u64、レイアウト版数は yuzhu 独自の 1）。
