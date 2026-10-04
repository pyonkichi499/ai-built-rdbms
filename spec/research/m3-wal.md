# yuzhu M3 設計調査: WAL の形式とライター

調査日: 2026-10-04。PostgreSQL は `REL_17_STABLE` ブランチのソースを取得して確認した（行番号は調査時点のもの。ブランチが進むとずれることがある）。
記号の意味: 【確認】は今回ソースを開いて確かめた事実、【記憶】は過去の知識に基づき今回は細部まで照合していない記述、【提案】は yuzhu 向けの推奨案。

参照 URL の接頭辞: `PG = https://github.com/postgres/postgres/blob/REL_17_STABLE`

---

## 0. 結論（要約）

1. **LSN は WAL ストリーム上のバイト位置（u64）**。PostgreSQL と同じ。ページの `pd_lsn` には「そのページを最後に変更したレコードの**終端** LSN」を入れる。
2. **WAL はセグメントファイル（既定 16 MiB）の連なり**。ファイル名は `pg_wal/` 下の 16 桁 16 進のセグメント番号。PostgreSQL のような 8KB の WAL ページ構造と「ページをまたぐ継続レコード」は**採用しない**。代わりに「**レコードはセグメントをまたがない**」制約を置き、足りなければ SWITCH レコードで次のセグメントへ進む。
3. **レコードは 8 バイト境界から始まる「24 バイト固定ヘッダ + ブロック参照 0〜N 個 + メインデータ」**。構成要素は PostgreSQL の `XLogRecord` / `XLogRecordBlockHeader` / main data と同じ考え方で、エンコードを単純化する（各ブロックの画像とデータをヘッダの直後にインラインで置く）。
4. **CRC32C（Castagnoli）を手書き（テーブル駆動 slice-by-8）**で実装する。`unsafe` が要る SSE4.2 命令は使わない。
5. **full page write（FPW）は PostgreSQL と同じ判定**: 「ページの LSN ≤ 直近チェックポイントの REDO 位置」なら、そのレコードにページ全体の画像を含める。pd_lower〜pd_upper の「穴」は省く。圧縮は後回し。
6. **ライター API は「レコードを組み立てる Builder」+ `Wal::insert()`（終端 LSN を返す）+ `Wal::flush(lsn)`**。M3 は挿入用 Mutex 1 本と、フラッシュ用 Mutex 1 本（+ Condvar）で足りる。FPW 判定は挿入 Mutex の内側で行い、PostgreSQL の「判定し直し（リトライ）」ループを不要にする。
7. **WAL-before-data**: バッファプールがダーティページを書き出す唯一の関数の中で、必ず `wal.flush(page_lsn)` を先に呼ぶ（PostgreSQL の `FlushBuffer` → `XLogFlush` と同じ）。
8. **COMMIT の耐久性**: コミットレコード挿入 → `flush(終端 LSN)`（fdatasync 完了まで待つ）→ コミットログ（pg_xact 相当）更新 → 可視化 → クライアントへ `CommandComplete`。ABORT レコードはフラッシュを待たない。
9. **WAL の write / fsync が失敗したらプロセスを PANIC 終了**し、再起動時のクラッシュリカバリに任せる（PostgreSQL の fsync 失敗時の方針と同じ）。
10. **リカバリは REDO 位置から順に読み、ヘッダ長・xl_prev・CRC のどれかが不正な最初のレコードを「WAL の終わり」とみなす**。各ページで `レコード終端 LSN ≤ pd_lsn` なら適用済みとして飛ばす。

---

## 1. PostgreSQL の WAL の仕組み（調査結果）

### 1.1 基本ルール（README）

`PG/src/backend/access/transam/README` の "Write-Ahead Log Coding"（399 行目〜）【確認】

- WAL は、それが記述するデータページの変更より**先に**安定記憶装置に届かなければならない。各データページには、そのページに影響した最新の WAL レコードの LSN を記録する。バッファマネージャはダーティページを書き出す前に、**そのページの LSN まで WAL をフラッシュ**する。
- リプレイ時、ページの LSN がレコードの位置以上なら、その変更は適用済みと判定できる。
- 通常のレコードは「1 ページへの差分」だけを持つ（physiological ログ）。これはページ書き込みがアトミックな場合にしか成立しない。そこで、**チェックポイント後にそのページを初めて変更するレコードにはページ全体のコピーを含め**、リプレイ時は差分の再実行ではなくコピーの復元を行う。「初めての変更か」は、ページの旧 LSN が直近チェックポイントの REDO 位置（RedoRecPtr）より前かどうかで判定する。
- WAL を書く操作の一般形（README 437 行目〜）:
  1. 対象バッファをピンして排他ロック
  2. `START_CRIT_SECTION()`（以降のエラーは PANIC。空き容量の確認などはこの前に済ませる）
  3. バッファを変更
  4. `MarkBufferDirty()`（WAL 挿入**前**に行う）
  5. `XLogBeginInsert` / `XLogRegister*` でレコードを組み立て `XLogInsert`、戻り値で `PageSetLSN`
  6. `END_CRIT_SECTION()`
  7. バッファのロック解除

`heap_insert` はまさにこの形になっている（`PG/src/backend/access/heap/heapam.c#L2170-L2283`）【確認】: `START_CRIT_SECTION` → `RelationPutHeapTuple` → `MarkBufferDirty` → `XLogBeginInsert` → `XLogRegisterData(xl_heap_insert)` → `XLogRegisterBuffer(0, buffer, REGBUF_STANDARD)` → `XLogRegisterBufData(xl_heap_header + タプル本体)` → `XLogInsert(RM_HEAP_ID, info)` → `PageSetLSN(page, recptr)` → `END_CRIT_SECTION`。

### 1.2 LSN

- `XLogRecPtr` は 64 ビットの WAL 内バイト位置【確認】（`xlogdefs.h`）。セグメント番号 = `LSN / wal_segment_size`、セグメント内オフセット = `LSN & (wal_segment_size - 1)`（`PG/src/include/access/xlog_internal.h#L106-L118`）【確認】。
- `XLogInsert` が返すのは**レコードの終端位置**（`EndPos`）。`XLogInsertRecord` は `ProcLastRecPtr = StartPos; XactLastRecEnd = EndPos; return EndPos;`（`PG/src/backend/access/transam/xlog.c#L1075-L1086`）【確認】。サイズは `MAXALIGN` されてから予約されるため、終端は次のレコードの開始位置と一致する（`xlog.c#L1108` `ReserveXLogInsertLocation` 内 `size = MAXALIGN(size)`）【確認】。
- ページヘッダの先頭 8 バイトが `pd_lsn`（コメント: "next byte after last byte of xlog record for last change to this page"）（`PG/src/include/storage/bufpage.h#L155-L167`）【確認】。`PageXLogRecPtr` は `{uint32 xlogid; uint32 xrecoff;}` の 2 語で、ネイティブのエンディアン【確認】。

### 1.3 レコードの形式（xlogrecord.h）

`PG/src/include/access/xlogrecord.h` 【確認】

```
XLogRecord (固定 24 バイト, MAXALIGN 境界から開始)
  uint32 xl_tot_len   レコード全体の長さ
  uint32 xl_xid       トランザクション ID
  uint64 xl_prev      直前のレコードの開始 LSN
  uint8  xl_info      下位 4 ビットは汎用フラグ、上位 4 ビットはリソースマネージャ(rmgr)用
  uint8  xl_rmid      rmgr ID
  (2 バイトのパディング、0 で埋める)
  uint32 xl_crc       CRC32C
XLogRecordBlockHeader × 0..N   (各ヘッダは id バイトで始まる。アラインしない)
  uint8  id           ブロック参照番号 (0..32)
  uint8  fork_flags   下位 4 ビット=フォーク番号、上位=HAS_IMAGE/HAS_DATA/WILL_INIT/SAME_REL
  uint16 data_length  このブロックの差分データ長(画像は含まない)
  [XLogRecordBlockImageHeader: uint16 length, uint16 hole_offset, uint8 bimg_info]  HAS_IMAGE のとき
  [RelFileLocator (spcOid, dbOid, relNumber)]  SAME_REL でないとき
  BlockNumber
XLogRecordDataHeaderShort/Long  (id=255 なら 1 バイト長、254 なら 4 バイト長)
ブロック画像とブロックデータ (ブロック順)
メインデータ
```

- `XLogRecord` は 41 行目、`XLogRecordBlockHeader` は 103 行目、`XLogRecordBlockImageHeader` は 141 行目、フラグ定義は 198 行目付近、予約 ID（255/254/253/252）は 243 行目付近。
- **穴（hole）**: データページの pd_lower〜pd_upper は 0 なので、画像から取り除き CRC の対象にもしない。`BKPIMAGE_HAS_HOLE`、`BKPIMAGE_APPLY`（リプレイ時に画像を復元すべきか）、圧縮方式フラグ（PGLZ/LZ4/ZSTD）がある【確認】。
- **WILL_INIT**: リプレイがページを初期化し直す（＝旧内容に依存しない）ことを示す。この場合 FPW は不要【確認】（フラグ定義）【記憶】（`XLogRecordAssemble` で WILL_INIT のときに画像を取らない分岐）。
- 上限 `XLogRecordMaxSize = 1020 MiB`、ブロック参照は最大 33 個（`XLR_MAX_BLOCK_ID 32`）【確認】。

### 1.4 CRC

- 挿入側（`PG/src/backend/access/transam/xloginsert.c#L903-L906`）【確認】: ヘッダ以降（ブロックヘッダ、画像、データ）の CRC を先に計算し、`XLogInsertRecord` で最後に固定ヘッダの `xl_crc` より前の部分を加える【記憶: 加算箇所は xlog.c 内】。
- 検証側（`PG/src/backend/access/transam/xlogreader.c#L1210-L1214`）【確認】: `INIT_CRC32C` → 本体 `[SizeOfXLogRecord, xl_tot_len)` → ヘッダ `[0, offsetof(xl_crc))` → `FIN_CRC32C`。つまり CRC は「本体 → ヘッダ（CRC 欄を除く）」の順に計算する。
- アルゴリズムは CRC-32C（Castagnoli、反転多項式 0x82F63B78、初期値 0xFFFFFFFF、最後に全ビット反転）。x86 では SSE4.2、ARM では ARMv8 CRC 命令を使い、なければ slice-by-8 のソフトウェア実装（`src/port/pg_crc32c_sb8.c`）【記憶】。

### 1.5 WAL ページとセグメント

`PG/src/include/access/xlog_internal.h` 【確認】

- WAL ファイルは `XLOG_BLCKSZ`（既定 8KB）の WAL ページに分かれ、各ページ先頭に `XLogPageHeaderData { xlp_magic(0xD116), xlp_info, xlp_tli, xlp_pageaddr, xlp_rem_len }`。セグメント先頭ページだけは `XLogLongPageHeaderData`（`xlp_sysid`、`xlp_seg_size`、`xlp_xlog_blcksz` を追加）。
- レコードはページをまたいでよい。またいだ場合、次ページのヘッダに `XLP_FIRST_IS_CONTRECORD` を立て、`xlp_rem_len` に残りの長さを書く。セグメントもまたげる。
- `xlp_pageaddr` は「このページの LSN」で、**再利用（recycle）された古いセグメントの残骸を見分ける**のに使う【記憶】。
- ファイル名は `TTTTTTTTXXXXXXXXYYYYYYYY`（タイムライン 8 桁 + セグメント番号を上下 8 桁ずつ）（`XLogFileName`、166 行目）【確認】。セグメントサイズは 1MB〜1GB の 2 の冪、initdb 時に決める【確認】（範囲）。
- 新しいセグメントは既定（`wal_init_zero = on`）でゼロ埋めしてから使い、チェックポイント後に不要になったセグメントは既定（`wal_recycle = on`）で名前を変えて再利用する（`xlog.c#L128-L129`、`XLogFileInitInternal` 3182 行目）【確認】。

### 1.6 挿入（xloginsert.c / xlog.c）

- **組み立て**: `XLogBeginInsert` → `XLogRegisterBuffer(block_id, buf, flags)` / `XLogRegisterBufData` / `XLogRegisterData` → `XLogInsert(rmid, info)`（`xloginsert.c#L474`）。`XLogInsert` は `XLogRecordAssemble`（548 行目）でバイト列の連結リストを作り、`XLogInsertRecord` に渡す【確認】。
- **FPW の判定**（`xloginsert.c#L592-L647`）【確認】: `REGBUF_FORCE_IMAGE` なら必ず、`REGBUF_NO_IMAGE` なら取らない、それ以外は `doPageWrites` が真のとき `needs_backup = (page_lsn <= RedoRecPtr)`。画像を取らない場合だけ差分データを含める（`needs_data = !needs_backup`）。つまり **FPW を含むレコードでは、そのブロックの差分データは省かれる**（`REGBUF_KEEP_DATA` 指定時を除く）。
- **判定の競合対策**（`xlog.c#L846-L857`）【確認】: 組み立ては WAL 挿入ロックの外で行うため、ロック取得後に RedoRecPtr が進んでいて `fpw_lsn <= RedoRecPtr` になっていたら、`InvalidXLogRecPtr` を返して**組み立てからやり直す**。
- **並行挿入**: `NUM_XLOGINSERT_LOCKS = 8`（`xlog.c#L151`）。`ReserveXLogInsertLocation`（1108 行目）がスピンロック下で位置を予約し `xl_prev` を決め、`CopyXLogRecordToWAL`（1225 行目）が共有 WAL バッファへコピーする。コピーは複数プロセスが並行してよい【確認】。
- **書き出しと同期**: `XLogWrite`（2295 行目）が WAL バッファをファイルへ write し、`issue_xlog_fsync`（8731 行目）が `wal_sync_method` に従って fsync / fdatasync / O_DSYNC 等を行う【確認】。Linux の既定は `fdatasync`【記憶】。
- **XLogFlush**（2775 行目）【確認】: `record <= LogwrtResult.Flush` なら即 return。そうでなければ WALWriteLock を取り、取れるまで待つ間に他者がフラッシュしてくれたら終わる。これが**グループコミットの素地**（他のバックエンドの分もまとめて fsync される）【記憶: 詳細ロジック】。

### 1.7 ヒープの WAL レコード

`PG/src/include/access/heapam_xlog.h` 【確認】

| info | 値 | メインデータ | ブロック |
|---|---|---|---|
| `XLOG_HEAP_INSERT` | 0x00 | `xl_heap_insert { offnum u16, flags u8 }` | 0: 対象ページ。データ = `xl_heap_header { t_infomask2, t_infomask, t_hoff }` + タプル本体（固定ヘッダ部を除く） |
| `XLOG_HEAP_DELETE` | 0x10 | `xl_heap_delete { xmax u32, offnum u16, infobits_set u8, flags u8 }` | 0: 対象ページ（データなし） |
| `XLOG_HEAP_UPDATE` | 0x20 | `xl_heap_update { old_xmax, old_offnum, old_infobits_set, flags, new_xmax, new_offnum }` | 0: 新ページ（新タプル）、1: 旧ページ（別ページの場合） |
| `XLOG_HEAP_HOT_UPDATE` | 0x40 | 同上 | 同一ページ |
| `XLOG_HEAP_LOCK` | 0x60 | 行ロック（M5 で必要） | |
| `XLOG_HEAP_INIT_PAGE` | 0x80 | 上記に OR するフラグ。ページを初期化してから適用（= WILL_INIT） | |

- タプルの固定ヘッダ（xmin, cmin, ctid など）は WAL に載せず、リプレイ時にレコードの `xl_xid` と `FirstCommandId`、ブロック番号 + offnum から再構成する【記憶: `heap_xlog_insert`（REL_17 では `heapam.c#L10014`）の詳細】。
- UPDATE では旧タプルとの共通接頭辞・接尾辞を省く最適化（`XLH_UPDATE_PREFIX_FROM_OLD` 等）がある【確認: フラグ定義】。
- 適用側は `XLogReadBufferForRedo` で、`lsn <= PageGetLSN(page)` なら `BLK_DONE`（適用済み）、FPW を含めば画像を復元して `BLK_RESTORED`、そうでなければ `BLK_NEEDS_REDO`（`PG/src/backend/access/transam/xlogutils.c#L455`）【確認】。ここで `lsn` はレコードの**終端** LSN（`record->EndRecPtr`）【記憶】。

### 1.8 WAL-before-data（bufmgr）

`FlushBuffer`（`PG/src/backend/storage/buffer/bufmgr.c#L3863`）【確認】: バッファヘッダロック下で `recptr = BufferGetLSN(buf)` を読み、`if (buf_state & BM_PERMANENT) XLogFlush(recptr);` を呼んでから（3926 行目付近）ページを書く。コメントにも "This implements the basic WAL rule that log updates must hit disk before any of the data-file changes they describe do." とある。

ヒントビットのみの変更は `MarkBufferDirtyHint`（5051 行目）で扱い、データチェックサム有効または `wal_log_hints = on` のときだけ、チェックポイント後最初の変更で `XLogSaveBufferForHint`（`xloginsert.c#L1065`）が FPW を書く【確認: 関数の存在】【記憶: 条件の詳細】。チェックサムがなければ、ヒントビットのみの変更で torn page が起きても、どの組み合わせも正しいページなので問題にならない。

### 1.9 コミットの順序

`RecordTransactionCommit`（`PG/src/backend/access/transam/xact.c#L1304`）【確認】:
`XactLogCommitRecord(...)`（1431 行目）→ `XLogFlush(XactLastRecEnd)`（1491 行目、同期コミット時）→ `TransactionIdCommitTree(xid, ...)`（1497 行目、pg_xact 更新）。その後 `ProcArrayEndTransaction` で他セッションから見えるようになる【記憶】。**WAL の永続化 → コミットログ → 可視化** の順。

### 1.10 リカバリでの WAL の終わりの判定

`ValidXLogRecordHeader`（`xlogreader.c#L1137`）【確認】: `xl_tot_len < SizeOfXLogRecord` なら "invalid record length"、rmgr ID が範囲外なら不正、`xl_prev` が直前のレコード位置と一致しなければ "record with incorrect prev-link"。その後本体を読んで CRC を検証（`ValidXLogRecord`、1210 行目）。どれかに失敗した位置がクラッシュリカバリにおける WAL の終端になる【記憶: xlogrecovery.c 側の扱い】。

### 1.11 ドキュメント

- WAL の内部: <https://www.postgresql.org/docs/17/wal-internals.html>
- 信頼性（torn page、FPW、fsync）: <https://www.postgresql.org/docs/17/wal-reliability.html>
- `full_page_writes` / `wal_sync_method` / `wal_init_zero` 等: <https://www.postgresql.org/docs/17/runtime-config-wal.html>
- WAL 設定とチェックポイント: <https://www.postgresql.org/docs/17/wal-configuration.html>

---

## 2. yuzhu の WAL 設計（提案）

### 2.1 方針と単純化の一覧

| 項目 | PostgreSQL | yuzhu M3【提案】 | 理由 |
|---|---|---|---|
| LSN | u64 バイト位置 | 同じ | ページ LSN 比較・セグメント計算がそのまま使える |
| WAL ページ（8KB）と継続レコード | あり | **なし** | 読み書きの実装量が大きく減る。ページヘッダの役目（古い残骸の識別）は xl_prev とセグメントヘッダで代替 |
| レコードのセグメント越え | 可 | **不可**（SWITCH で次へ） | リーダーが 1 ファイル内で完結する。無駄は最大でも 1 レコード分 |
| タイムライン | あり | なし（ファイル名に含めない） | PITR/レプリケーションは範囲外。必要になったらセグメントヘッダに追加 |
| セグメントの再利用 | 既定で再利用 | **しない**（新規作成 + ゼロ埋め、古いものは削除） | 古い残骸の誤認を構造的に防ぐ。性能が問題になったら再利用を検討 |
| ブロック参照の符号化 | 全ヘッダ → 全データ、SAME_REL 省略 | **ヘッダの直後にそのブロックの画像・データをインライン**、リレーションは毎回書く | デコードが逐次で書ける。数バイトの無駄は許容 |
| タプルの固定ヘッダ | WAL から省いて再構成 | **タプル全体（ヘッダ込み）を載せる** | 再構成ロジックが不要。リプレイがバイト列のコピーで済む |
| UPDATE の接頭辞/接尾辞圧縮 | あり | なし | 最適化は後回し |
| FPW の穴の除去 | あり | **あり** | 実装が数行で、WAL 量が大きく減る |
| FPW の圧縮 | pglz/lz4/zstd | なし | 外部クレートが必要。後回し |
| 挿入の並行性 | 8 本の挿入ロック + 予約/コピー分離 | **Mutex 1 本** | 単一ライター。M5 の複数ライターでもまずはこのままで、計測後に分離 |
| FPW 判定 | ロック外で判定 + リトライ | **挿入 Mutex 内で判定** | リトライが不要になり正しさを保ちやすい |
| CRC | ハードウェア命令 | 手書きソフトウェア（slice-by-8） | `forbid(unsafe_code)` のため |

### 2.2 ディレクトリとセグメントファイル

```
<data_dir>/
  global/pg_control      制御ファイル（チェックポイント位置など。チェックポイント設計で詳述）
  pg_wal/
    0000000000000001     セグメント番号を 16 桁 16 進（大文字）
    0000000000000002
  pg_xact/               コミットログ（別資料）
  base/<db_oid>/<relfilenumber>
```

- セグメントサイズ `WAL_SEG_SIZE = 16 MiB`（定数。initdb 時の値を制御ファイルとセグメントヘッダに記録し、起動時に一致を確認）【提案】。
- LSN 0 は「無効」として使うため、最初のセグメント番号は 1 とし、最初のレコードは `LSN = 1 * WAL_SEG_SIZE + SEG_HEADER_SIZE` から始める（PostgreSQL も initdb 後の最初の LSN は 0 ではない）【提案】。
- **セグメントの作成**: 一時名（`pg_wal/xlogtemp.<pid>`）で作成 → 16 MiB ゼロ書き込み → セグメントヘッダ書き込み → `fsync(file)` → `rename` → `fsync(pg_wal ディレクトリ)`。ゼロ埋めしておけば、以後の追記は `fdatasync` だけで済む（ファイルサイズが変わらないためメタデータ同期が不要）【提案】。次のセグメントは現在のセグメントの使用量が半分を超えたら先に作っておくとよい（M3 では同期的に作ってもよい）。
- **削除**: チェックポイント完了後、新しい REDO 位置を含むセグメントより前のセグメントを削除する。

#### セグメントヘッダ（32 バイト、リトルエンディアン）【提案】

| オフセット | 型 | 内容 |
|---|---|---|
| 0 | u32 | magic `0x59575A31`（"YZW1" 相当の任意値） |
| 4 | u16 | WAL 形式のバージョン（1） |
| 6 | u16 | フラグ（予約、0） |
| 8 | u64 | system_identifier（initdb 時に生成。別クラスタの WAL 混入を検出） |
| 16 | u64 | セグメント番号（ファイル名と一致を確認） |
| 24 | u32 | セグメントサイズ |
| 28 | u32 | CRC32C（0〜27 バイト） |

### 2.3 レコードのバイト配置【提案】

すべてリトルエンディアン。各レコードは **8 バイト境界**から始まり、`tot_len` の後ろは次の 8 バイト境界まで 0 で埋める（パディングは `tot_len` に含めない）。

```
固定ヘッダ（24 バイト）
  off  0  u32  tot_len    ヘッダを含むレコード全体の長さ（パディングを除く）
  off  4  u32  crc        CRC32C。計算順は「本体 [24, tot_len) → ヘッダ [0, 4) と [8, 24)」
  off  8  u64  prev       直前のレコードの開始 LSN（最初のレコードは 0）
  off 16  u32  xid        トランザクション ID（無関係なら 0 = InvalidTransactionId）
  off 20  u8   rmgr       リソースマネージャ ID
  off 21  u8   info       上位 4 ビット: rmgr ごとの種別、下位 4 ビット: 汎用フラグ
  off 22  u8   nblocks    ブロック参照の個数（0..=MAX_BLOCK_REFS）
  off 23  u8   reserved   0
ブロック参照 × nblocks（順番に、アラインなし）
  u8   block_id     0 から連番（rmgr のリプレイ関数が意味を決める）
  u8   flags        0x01 HAS_IMAGE / 0x02 HAS_DATA / 0x04 WILL_INIT / 0x08 IMAGE_HAS_HOLE / 0x10 APPLY_IMAGE
  u8   fork         0 = main（M3 は main のみ。FSM/VM 用に予約）
  u8   reserved     0
  u32  db_oid       ┐ リレーションの物理ファイルを特定する RelFileId
  u32  rel_number   ┘ （テーブルスペースは M3 では持たない）
  u32  block_no
  [HAS_IMAGE のとき]
    u16 hole_offset  穴の開始位置（IMAGE_HAS_HOLE でなければ 0）
    u16 hole_length  穴の長さ（同上 0）
    u8[8192 - hole_length]  ページ画像（穴を除いたもの）
  [HAS_DATA のとき]
    u16 data_len
    u8[data_len]     rmgr 固有の差分データ
メインデータ
  u32  main_len
  u8[main_len]
```

- `tot_len` の下限は 24 + 4（nblocks = 0、main_len = 0）。上限は `MAX_RECORD_LEN = 1 MiB`【提案】。M3 の最大レコードは「ページ画像 2 枚 + タプル 1 個」程度で 20KB 弱なので十分。
- `MAX_BLOCK_REFS = 4`【提案】（ヒープの UPDATE で 2、将来の B+Tree 分割で 3〜4。必要になったら増やす）。
- `info` の下位 4 ビットの汎用フラグは M3 では未使用（PostgreSQL の `XLR_SPECIAL_REL_UPDATE` 等に相当する枠として予約）。
- **WILL_INIT のブロックには FPW を付けない**（リプレイがページをゼロから作るため）。
- **HAS_IMAGE のブロックは差分データを省く**（PostgreSQL と同じ）。リプレイは画像を復元して終わり。
  - 例外として、リプレイ関数が FPW の有無にかかわらず差分を必要とする場合（M4 の B+Tree など）に備え、`KEEP_DATA` 相当を Builder のオプションで用意しておく。
- `xid` を u32 にするのは PostgreSQL のタプルヘッダ（xmin/xmax が 32 ビット）に合わせるため。MVCC 設計で 64 ビット XID を採るなら、ここも u64 にしてヘッダを 32 バイトにする（**要調整: MVCC 担当の資料と突き合わせる**）。

#### 穴の判定

PostgreSQL の `REGBUF_STANDARD` と同じく、「標準レイアウトのページ」（ヘッダの `pd_lower`/`pd_upper` が妥当: `PAGE_HEADER_SIZE <= pd_lower <= pd_upper <= BLCKSZ`）なら `hole_offset = pd_lower`、`hole_length = pd_upper - pd_lower` とする。穴が 0 でなかったら（＝ページが壊れている／標準でない）穴なしで全体を載せる。リプレイ時は穴を 0 で埋めて 8192 バイトに戻す。

### 2.4 リソースマネージャとレコード種別（M3）【提案】

| rmgr | ID | info（上位 4 ビット） | 内容 |
|---|---|---|---|
| XLOG | 0 | 0x00 CHECKPOINT_SHUTDOWN / 0x10 CHECKPOINT_ONLINE | メインデータ: redo LSN、next_xid、next_oid、oldest_xid など（チェックポイント設計で確定） |
| | | 0x20 SWITCH | セグメントの残りを捨てて次のセグメントへ |
| | | 0x30 FPI | ページ画像だけのレコード（ヒント用・汎用） |
| | | 0x40 NOOP | テスト用 |
| XACT | 1 | 0x00 COMMIT / 0x10 ABORT | メインデータ: `xid u32`（ヘッダと同じ）、`commit_time i64`（µs, PG エポック）、`ndropped u32` + `RelFileId[]`（コミットで削除するファイル）、ABORT は作成済みで削除すべきファイル |
| HEAP | 2 | 0x00 INSERT / 0x10 DELETE / 0x20 UPDATE / 0x30 HOT_UPDATE（M4 以降）/ 0x40 LOCK（M5）/ 0x80 INIT_PAGE（フラグ） | 下記 |
| SMGR | 3 | 0x00 CREATE / 0x10 TRUNCATE | リレーションファイルの作成・切り詰め |
| CLOG | 4 | 0x00 ZEROPAGE | pg_xact の新しいページの初期化 |
| BTREE | 5 | （M4） | |
| SEQ | 6 | （M4、SERIAL 用） | |

ヒープのレコード（メインデータとブロックデータ）:

```
HEAP_INSERT   main:  offnum u16, flags u8
              blk0:  data = タプル全体のバイト列（HeapTupleHeader 込み）
              INIT_PAGE なら blk0 に WILL_INIT（新しく初期化したページへの最初の挿入）
HEAP_DELETE   main:  offnum u16, xmax u32, infomask_set u16, infomask_clear u16
              blk0:  データなし（画像なしなら差分は main だけで足りる）
HEAP_UPDATE   main:  old_offnum u16, old_xmax u32, old_infomask_set u16, new_offnum u16, flags u8
              blk0:  新ページ。data = 新タプル全体
              blk1:  旧ページ（新旧が同一ページなら省略）
              リプレイ: 旧タプルの xmax と infomask を設定し、t_ctid を (新ブロック, new_offnum) に。新タプルを new_offnum に置く
```

- カタログの変更（CREATE TABLE で pg_class などに行を足す）は、カタログもヒープなので HEAP レコードで記録される。DDL 専用のレコードは不要（SMGR_CREATE を除く）。
- リレーションファイルの作成は「SMGR_CREATE を挿入 → ファイル作成」の順。削除（DROP TABLE）はコミットレコードに対象を載せ、**コミットが永続化してから** unlink する（PostgreSQL の pending deletes と同じ考え方）【記憶: PG の詳細】。

### 2.5 ライター API（Rust）【提案】

```rust
// yuzhu-core/src/wal/mod.rs
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Lsn(pub u64);
impl Lsn { pub const INVALID: Lsn = Lsn(0); }

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RmgrId { Xlog = 0, Xact = 1, Heap = 2, Smgr = 3, Clog = 4 /* Btree = 5, Seq = 6 */ }

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RelFileId { pub db_oid: Oid, pub rel_number: u32 }

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BlockTag { pub rel: RelFileId, pub fork: u8, pub block: u32 }

bitflags_like! { pub struct RegFlags: u8 {
    const STANDARD   = 0x01; // 標準ページ。穴を省いてよい
    const WILL_INIT  = 0x02; // リプレイがページを初期化する。FPW 不要
    const FORCE_IMAGE= 0x04; // 常に画像を載せる（XLOG_FPI 用）
    const NO_IMAGE   = 0x08; // 画像を載せない（初期化直後で画像不要と分かっている等）
    const KEEP_DATA  = 0x10; // 画像を載せても差分データを残す
} }

/// XLogBeginInsert〜XLogRegister* に相当する組み立て器。スタック上に作って使い捨てる。
pub struct RecordBuilder<'a> {
    rmgr: RmgrId,
    info: u8,
    xid: Xid,
    blocks: SmallVec<[BlockRef<'a>; 4]>, // 自作の固定長配列でよい
    main: Vec<u8>,
}
struct BlockRef<'a> {
    id: u8, tag: BlockTag, flags: RegFlags,
    page: &'a Page,           // 呼び出し側が排他ロック中のページ（FPW 判定と画像コピーに使う）
    data: Vec<u8>,
}

impl<'a> RecordBuilder<'a> {
    pub fn new(rmgr: RmgrId, info: u8, xid: Xid) -> Self;
    pub fn register_block(&mut self, id: u8, tag: BlockTag, page: &'a Page, flags: RegFlags);
    pub fn block_data(&mut self, id: u8, bytes: &[u8]);
    pub fn main_data(&mut self, bytes: &[u8]);
}

pub struct Wal { /* 下記 2.6 */ }

impl Wal {
    /// レコードを WAL バッファに追加し、**終端 LSN**（= 次のレコードの開始位置）を返す。
    /// FPW の判定・画像コピー・prev の設定・CRC 計算は挿入 Mutex の内側で行う。
    /// 戻り値を、登録した全ページの pd_lsn に設定するのは呼び出し側の責任。
    pub fn insert(&self, rec: RecordBuilder<'_>) -> Result<Lsn>;

    /// upto までの WAL を write + fdatasync する。完了まで戻らない。
    /// 既に flushed_lsn() >= upto なら即座に戻る。
    pub fn flush(&self, upto: Lsn) -> Result<()>;

    pub fn insert_lsn(&self) -> Lsn;   // 次に挿入される位置
    pub fn flushed_lsn(&self) -> Lsn;  // AtomicU64 から読むだけ（ロック不要）
    pub fn redo_lsn(&self) -> Lsn;     // 直近チェックポイントの REDO 位置（FPW 判定用）

    /// チェックポイント開始時に呼ぶ。挿入 Mutex の下で insert_lsn を読み、それを新しい REDO 位置にする。
    pub fn begin_checkpoint(&self) -> Lsn;
    /// 現在のセグメントを閉じて次へ（SWITCH レコード）。テストとアーカイブ（将来）用。
    pub fn switch_segment(&self) -> Result<Lsn>;
}

/// リカバリ用の逐次リーダー
pub struct WalReader { /* vfs, 現在位置, 直前レコードの開始 LSN */ }
pub struct DecodedRecord { pub start: Lsn, pub end: Lsn, pub xid: Xid, pub rmgr: RmgrId,
                           pub info: u8, pub blocks: Vec<DecodedBlock>, pub main: Vec<u8> }
impl WalReader {
    pub fn open(vfs: Arc<dyn Vfs>, start: Lsn, expected_prev: Option<Lsn>) -> Result<Self>;
    /// 次のレコード。WAL の終わり（不正なレコードを含む）なら Ok(None)。I/O エラーは Err。
    pub fn next(&mut self) -> Result<Option<DecodedRecord>>;
    pub fn end_of_wal(&self) -> Lsn; // None を返した位置
}
```

ヒープ挿入の呼び出し例:

```rust
let mut guard = buffer_pool.fetch_write(tag)?;      // 1. ピン + 排他ロック
// 2. ここまでに空き容量確認など「失敗しうる処理」を済ませる（以降の失敗は PANIC）
let offnum = guard.page_mut().add_tuple(&tuple)?;   // 3. ページ変更（失敗しないことが前提）
guard.mark_dirty();                                 // 4. ダーティ化（WAL 挿入前）
let mut rec = RecordBuilder::new(RmgrId::Heap, HEAP_INSERT | init_flag, xid);
rec.register_block(0, tag, guard.page(), RegFlags::STANDARD | will_init);
rec.block_data(0, tuple.as_bytes());
rec.main_data(&HeapInsert { offnum, flags: 0 }.encode());
let lsn = wal.insert(rec).unwrap_or_else(|e| panic_shutdown(e)); // 5. WAL 挿入（失敗は PANIC）
guard.page_mut().set_lsn(lsn);                      //    pd_lsn を更新
drop(guard);                                        // 6. ロック解除
```

- **画像は「変更後」のページ**を取る（PostgreSQL も変更後のバッファを登録する）。リプレイは画像で上書きして終わりなので、差分の適用は不要。
- Rust にはクリティカルセクションがないので、手順 3〜5 の間で `?` を使わない（失敗しうる処理はすべて手順 2 より前に済ませる）規約にする。WAL 挿入の失敗（バッファ確保や I/O）は `panic_shutdown()` で**プロセスを終了**する。変更済みでログのないページがディスクに出るのを防ぐため。
- 呼び出しのたびにページ画像のコピー（最大 8KB）を挿入 Mutex 内で行う。単一ライターの M3 では問題にならない。

### 2.6 内部構造と並行性【提案】

```rust
pub struct Wal {
    vfs: Arc<dyn Vfs>,
    insert: Mutex<InsertState>,
    flush_lock: Mutex<FlushState>,   // write + fdatasync を行う者は 1 人
    flushed: AtomicU64,              // 永続化済みの LSN
    redo: AtomicU64,                 // FPW 判定用の REDO 位置（挿入 Mutex 下で更新）
    poisoned: AtomicBool,            // I/O 失敗後は全操作をエラーにする
}
struct InsertState {
    insert_pos: Lsn,                 // 次のレコードの開始位置（8 バイト境界）
    prev_start: Lsn,                 // 直前のレコードの開始位置（prev 欄に入れる）
    buf: Vec<u8>,                    // [buf_base, insert_pos) の未書き出し分
    buf_base: Lsn,
}
struct FlushState { written: Lsn, current_seg: Option<(u64, Box<dyn VfsFile>)> }
```

- **insert**: 挿入 Mutex を取る → `redo` と各ページの `pd_lsn` を比較して FPW を決める → バイト列を組み立て CRC 計算 → セグメントの残りに収まらなければ SWITCH レコード（残りが 24 バイト未満なら何も書かずにゼロのまま）を置いて `insert_pos` を次のセグメントの先頭 + 32 に進める → `buf` に追記 → `insert_pos` と `prev_start` を更新 → Mutex を外す → 終端 LSN を返す。
- `buf` が上限（例 `wal_buffers = 4 MiB`）を超えたら、挿入側で `flush` を待たない write（fsync なし）を起動してもよいが、M3 では**単に `flush(insert_pos)` を呼ぶ**だけで十分。
- **flush(upto)**: `flushed >= upto` なら return → `flush_lock` を取る → 再確認（待っている間に他者がフラッシュ済みなら return。**これだけで素朴なグループコミットになる**）→ 挿入 Mutex を短時間だけ取り `buf` の中身を `mem::take` で取り出し `buf_base` を進める → 挿入 Mutex を外す → セグメント境界で分けて `write_at` → 書いたセグメントごとに `sync_data` → `flushed.store(書いた末尾)` → `flush_lock` を外す。
  - 取り出すのは「その時点の insert_pos まで全部」。要求より多く永続化するのは問題ない。
  - **既に永続化済みのバイトを書き直さない**（新しいバイトだけを書く）。PostgreSQL は末尾の WAL ページ全体を書き直すが、yuzhu は WAL ページを持たないのでその必要がなく、書き直し中のクラッシュでコミット済みデータが壊れる心配がない。
- M5 の複数ライターでもこの構造のまま動く（挿入は Mutex で直列化される）。ボトルネックになったら PostgreSQL のように「位置の予約（短いロック）」と「バッファへのコピー（並行）」を分ける。
- M6 のグループコミットは `flush` に「少し待ってから fsync」（`commit_delay` 相当）を足す程度で済む。

### 2.7 耐久性のセマンティクス【提案】

- **COMMIT（XID を持つトランザクション）**: `wal.insert(XACT_COMMIT)` → `wal.flush(end_lsn)` → pg_xact に COMMITTED を書く（バッファ上）→ スナップショット用の実行中リストから外す → ロック解放 → `CommandComplete` を返す。PostgreSQL と同じ順序（1.9 節）。
  - flush の前に可視化すると、クラッシュ後に「読めたはずのデータが消える」ので順序は厳守。
- **XID を持たないトランザクション**（読み取りのみ）: コミットレコードを書かない（PostgreSQL も XID 未割り当てならレコードを書かない【記憶】）。
- **ABORT**: `XACT_ABORT` を挿入するがフラッシュは待たない。クラッシュで消えても、リカバリ後に「コミットレコードのない XID は中断」とみなすので問題ない（pg_xact の扱いは MVCC/コミットログ資料で確定）。
- **fsync / write の失敗**: `poisoned` を立て、サーバプロセスを終了する（PostgreSQL は `data_sync_retry = off` が既定で、データファイルの fsync 失敗も PANIC【記憶】）。Linux では fsync が一度失敗すると、ダーティだったページが黙って捨てられ、再試行の fsync が成功してしまうことがあるため、再試行は危険。
- **同期方式**: 事前にゼロ埋めしたファイルへの `pwrite` + `fdatasync`（PostgreSQL の Linux 既定と同じ【記憶】）。`O_DIRECT` や `O_DSYNC` は使わない。
- **synchronous_commit = off** は M3 では提供しない（将来追加する場合、pg_xact ページの書き出し前にそのページ上の最新コミットの LSN まで WAL をフラッシュする「グループ LSN」管理が必要になる【記憶】）。

### 2.8 ページ LSN の使い方【提案】

yuzhu のページヘッダも先頭 8 バイトを `pd_lsn: u64`（リトルエンディアン）にする（ヒープページの設計資料と合わせる）。用途は 4 つ:

1. **WAL-before-data**: バッファプールがダーティページを書く関数（追い出し・チェックポイント・将来のバックグラウンドライターがすべてここを通る）で、ページの共有ロック下で内容をコピーし、そのコピーの `pd_lsn` まで `wal.flush()` してから `write_at` する。コピーを取ってから書くので、書き込み中に他者がページを変更しても、書いた内容と flush した LSN が食い違わない。
2. **FPW の判定**: `insert` の中で `page.lsn() <= redo_lsn` なら画像を載せる。新しく初期化したページ（`pd_lsn = 0`）は `WILL_INIT` なので画像を載せない。
3. **リプレイの冪等性**: `record.end <= page.lsn()` ならそのブロックは適用済みとして飛ばす。適用したら `page.set_lsn(record.end)`。画像を復元した場合も `set_lsn(record.end)`。
4. **ファイル末尾を越えるブロック**: リプレイ時に対象ブロックがファイルの外にあれば、ゼロページで拡張してから適用する（WILL_INIT か画像があれば内容は確定する）。PostgreSQL の `XLogReadBufferExtended` と同様【記憶】。

ヒントビット（xmin/xmax のコミット済みフラグ）は WAL を書かずに設定し、ページを「ヒントのみダーティ」にする。**M3 ではデータページのチェックサムを持たない**（`pd_checksum = 0`）とすれば、ヒントのみの変更による torn page は無害で FPW も不要【提案】。チェックサムを入れるときに、PostgreSQL の `XLogSaveBufferForHint` 相当（チェックポイント後最初のヒント変更で `XLOG_FPI`）を追加する。

### 2.9 リカバリの読み取り【提案】

1. 制御ファイルからチェックポイントレコードの LSN を読み、そのレコードから REDO 位置を得る。
2. `WalReader::open(redo_lsn)` から順に `next()`。各レコードで次を検査し、どれかに失敗したらそこを WAL の終わりとする:
   - セグメントの残りが 24 バイト未満 → 次のセグメントの先頭 + 32 へ
   - `tot_len == 0`（ゼロ埋めのまま）→ WAL の終わり
   - `tot_len < 28` または `tot_len > MAX_RECORD_LEN` または セグメントからはみ出す → 終わり
   - `prev != 直前のレコードの開始 LSN` → 終わり
   - `rmgr` が未知、`nblocks > MAX_BLOCK_REFS`、ブロック参照の長さの合計が `tot_len` と合わない → 終わり
   - CRC 不一致 → 終わり
   - SWITCH レコード → 次のセグメントへ
   - 次のセグメントファイルがない、またはセグメントヘッダ（magic、system_id、seg_no、CRC）が不正 → 終わり
3. rmgr ごとのリプレイ関数で適用（2.8 節の LSN 判定）。XACT_COMMIT/ABORT で pg_xact を更新。
4. 終わりの位置から書き込みを再開する。**再開前に、終わりの位置からそのセグメントの末尾までをゼロで上書きして fdatasync する**【提案】。torn write で途中まで書かれたレコードの残骸を消し、以後のクラッシュで残骸を誤って有効なレコードと読む余地をなくす。
5. 全ダーティページを書き出し、シャットダウンチェックポイントを書いて通常運転へ（チェックポイント資料で確定）。

セグメントを再利用しないので、ゼロ埋め済みのファイルに現れる非ゼロのバイトは「今回のクラスタのこのセグメントの書き込み」だけになる。これと xl_prev・CRC の組み合わせで、PostgreSQL の `xlp_pageaddr` の役割を代替できる。

### 2.10 CRC32C の実装【提案】

- `yuzhu-core/src/util/crc32c.rs` に手書きする。反転多項式 `0x82F63B78`、初期値 `0xFFFF_FFFF`、最後に `!crc`。
- `const fn` でテーブルを生成する slice-by-8（8 × 256 × u32 = 8KB）。`unsafe` 不要、安全な Rust で 1〜2 GB/s 程度が見込める【記憶: 一般的な性能値。要計測】。8KB のページ画像 1 枚あたり数 µs で、fsync（ミリ秒単位）に比べ無視できる。
- テストベクタ: `crc32c(b"123456789") == 0xE306_9283`（RFC 3720 の CRC32C の標準チェック値）【記憶】。加えて「1 バイトずつ更新」と「一括」が一致すること、ランダム入力で bitwise 実装と一致することを proptest 等で確かめる。
- 外部クレート（`crc32c` など）は内部で `unsafe` の SIMD を使うものが多い。依存クレートの `unsafe` は `forbid` の対象外だが、WAL はコア部品なので手書きを推奨する。

### 2.11 障害注入テスト（WAL 部分）【提案】

ファイル I/O の抽象化レイヤ（`Vfs` / `VfsFile`）の障害注入実装で、次の観点を最低限カバーする。

| 観点 | 注入方法 | 期待 |
|---|---|---|
| コミット応答後のクラッシュ | `sync_data` 済みのバイトだけを残し、未同期の書き込みを捨てて再起動 | 応答済みのコミットはすべて残る |
| コミット応答前のクラッシュ | flush の途中（write 後・fsync 前）で停止 | そのトランザクションは「残る」か「消える」のどちらか。中途半端な状態はない |
| WAL の torn write | 未同期の書き込みを 512B / 4KB 単位でランダムに一部だけ残す | CRC で検出し、そこを終わりとして起動できる |
| データページの torn write | データファイルへの 8KB 書き込みを 4KB 単位で半分だけ残す | FPW でページが正しく復元される |
| fsync の失敗 | `sync_data` が `EIO` を返す | サーバが PANIC 終了し、再起動後に整合する |
| WAL-before-data 違反の検出 | テスト用フックで「データページの書き込み時に `page.lsn > flushed_lsn`」なら assert | 一度も発生しない |
| セグメント越え | 小さいセグメント（テスト時のみ 64KB などに変更可能にする）で大量挿入 | SWITCH を含めて正しく読める |
| リカバリの冪等性 | リカバリ途中でクラッシュさせ、再度リカバリ | 結果が同じ |

テスト時にセグメントサイズを小さくできるよう、`WAL_SEG_SIZE` は initdb のオプション（2 の冪、64KiB〜1GiB）で変えられるようにしておくと便利【提案】。

---

## 3. 未決事項（他の資料と突き合わせが必要）

- **XID の幅**（u32 か u64 か）: MVCC 資料の結論に合わせてヘッダの `xid` 欄を決める。
- **ページヘッダの配置**: `pd_lsn` を先頭 8 バイトに置くことをヒープページ資料と合わせる。`pd_checksum` を M3 で使わないことも合わせて確認する。
- **チェックポイントレコードの中身と制御ファイルの形式**: チェックポイント資料で確定する。
- **pg_xact ページの WAL 保護**: コミットログのページは CLOG_ZEROPAGE と XACT_COMMIT/ABORT のリプレイで再構築できるため FPW は不要という PostgreSQL の考え方を採るか【記憶】、コミットログ資料で確定する。
- **DROP TABLE のファイル削除のタイミング**: コミットレコードに載せてコミット永続化後に unlink する案（2.4 節）でよいか、カタログ設計と合わせる。
