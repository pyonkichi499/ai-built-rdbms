# yuzhu M5 基本設計 03: VACUUM・pruning・FSM・凍結・clog の切り詰め・autovacuum

この章（略号 **VC**）のゴールは、更新と削除で**膨らみ続ける状態から抜ける**ことです。死んだ行バージョンの領域を回収し（pruning）、行ポインタとインデックスの項目を片付け（VACUUM の 3 段階）、空いた領域を挿入が再利用し（FSM）、古い XID を凍結して clog を切り詰め、テーブルの末尾を縮め、必要なら裏で自動的に行う（簡易 autovacuum。既定は無効）。M2 から守ってきた「**行ポインタ番号（TID）を再利用しない**」という前提がこの章で外れる。M4 の B+Tree はその前提で作られているので、外しても壊れないことの証明（00 の A7）をこの章の 5.3.5 で行う。

- 要件定義: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>
- 前提の設計書: `spec/design/m5/00-contracts.md`（以下「**00**」。決定 D11・D13・D14・D15・D16・D17・D42・D47・D51、WAL は 00 §3.3、型は 00 §4.5、WP は 00 §7 の VC-1〜VC-6）、`m2.md`（ページ・ヒープ・smgr・バッファ）、`m3.md`（WAL・REDO・チェックポイント・クラッシュ試験）、`spec/design/m4/00-contracts.md`（B+Tree、`ddl/`、`DdlCtx`）。**章の間の境界**: ロックのモードは 01 章（LK）、xmax の読み取り関数・可視性の凍結の扱い・タプルロックは 02 章（RW）、`yz_datxid` の行の作成と削除は 09 章（DB）、FOREIGN KEY の参照先の検出は 07 章（FK）。
- 調査資料（根拠）: `spec/research/m5-concurrency.md` §12（VACUUM）・§13（HOT）・§14（工程）、`m3-mvcc.md` §5（凍結と clog）、`m4-btree.md` §3（VACUUM との連動）、`pg-compat-tools.md`（pgbench の VACUUM）
- 迷ったら **PostgreSQL 17 と同じ挙動**を選ぶ。PostgreSQL のソースは REL_17_STABLE で、`PG:<path>` は `https://github.com/postgres/postgres/blob/REL_17_STABLE/<path>` の略。根拠の記号は 【確認】ソースや実機で確かめた、【記憶】未照合、【提案】yuzhu への推奨。「（未検証）」と付けた記述は実装前に確かめる（第 9 節）。
- この章だけ読めば実装できるように、バイト形式（第 3 節）、シグネチャ（第 4 節）、手順と証明（第 5 節）、エラーの SQLSTATE（第 6 節）、テスト（第 7 節）を書く。

---

## 0. 決定

### 0.1 調査間・既存設計との食い違いと、この章の決定

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| VC-D1 | pruning・デフラグ・`LP_UNUSED` の設定に cleanup lock（排他ラッチ + 他者のピンなし）が要るか | 要る（PG。m4-btree §3 の btbulkdelete の連動も同じ）／要らない（m5-concurrency §12.2 の見込み。m2-buffer-io は `cleanup_waiter` を予約）／ピン数を条件に足す | **作らない。排他ラッチだけで行う**。ピン数は見ない。`cleanup_waiter` は使わない（予約のまま）。正しさは次の 2 つで守る: (a) ページの中身への参照をラッチの外へ持ち出さない（`HeapScan`・`IndexScan` はタプルと TID をコピーする。M2 D10・M4 §13.2 の既存規約）、(b) TID を覚えて後で読み直す呼び出し元は、そのタプルが**自分の登録済みスナップショットで可視**であることに頼る（5.3.5 の証明） | cleanup lock は「ピンだけ持ってページ内のポインタを保持している者」を排除する仕組みで、yuzhu にそういう者はいない。待ちの仕組み（起床、飢餓）を作る工数も要らない |
| VC-D2 | VACUUM が XID を持つか | 持つ／持たない（m5-concurrency §12.2。PG も同じ） | **持たない**。`WriteCtx { xid: Xid::INVALID, cid: 0 }`、WAL のヘッダの `xid` は 0 | XID を持つと自分の XID が horizon を止める（D11）。コミットレコードも要らない |
| VC-D3 | 統計（`reltuples` / `relpages`）と凍結境界の更新方法 | 通常の UPDATE（新しい版を作る）／その場の上書き（PG の `heap_inplace_update`） | **その場の上書き**。新しい WAL レコード `HEAP2 INPLACE`（0x30。00 §3.3 は 0x30 以降を予約としているので **第 11 節で契約変更を依頼する**）と `TableStore::inplace_update`（同）を足す | (1) ANALYZE はトランザクションブロックの中で動く（VC-D15）。通常の UPDATE ではユーザーのトランザクションに XID と行の版を持ち込み、ROLLBACK で統計が戻る。(2) VACUUM が XID を持てない（VC-D2）。(3) pg_class の版が増えず、カタログのキャッシュの無効化も要らない。(4) 並行して同じ行を更新する者はいない（VACUUM / ANALYZE / DDL はすべて ShareUpdateExclusive と衝突する） |
| VC-D4 | horizon の範囲 | データベースごと（PG は他のデータベースのバックエンドの xmin を無視する）／クラスタ全体（00 D11 の `oldest_xmin()`） | **クラスタ全体**。00 のとおり | 登録簿を 1 つにできる。別のデータベースの長いトランザクションが VACUUM を止める点だけ PG と違う（M5-VC-Q1） |
| VC-D5 | 凍結の境界 | `vacuum_freeze_min_age` 相当の余裕を持つ（PG）／常に horizon まで（00 §3.6） | **常に horizon まで**（`freeze_cutoff = horizon`。`vacuum_freeze_min_age` などは保存するだけ）。凍結は xmin に `XMIN_FROZEN`（両ビット。値は残す）、xmax の無効化。clog の切り詰めがなるべく進むようにする | 00 のとおり。書き込みの量が増える代わりに、余裕を持つ実装は cutoff を引き下げるだけで足せる（M5-VC-Q10） |
| VC-D6 | pruning が行ポインタを残すか | `LP_DEAD`（領域だけ回収。PG）／`LP_REDIRECT`（HOT）／直接 `LP_UNUSED` | **`LP_DEAD`（タプルの領域は即回収し、行ポインタだけ残す）**。インデックスのないテーブルを VACUUM するときだけ直接 `LP_UNUSED` にする（1 パス）。HOT・`LP_REDIRECT` は使わない（D15） | インデックスの項目が `LP_UNUSED` の行ポインタを指すことは決してない、という不変条件（A7） |
| VC-D7 | 機会的 pruning の起動場所 | 読み取りでページを開くとき（PG）のみ／加えて「挿入が入らなかったとき」 | **スキャンと TID での取得でページを開くとき（`try_write`。取れなければ諦める）**、および **`hio` が排他ラッチを持ったまま「入らない」と判断したとき**（そのラッチで pruning して再判定） | 後者はラッチの取り直しが要らず、更新の多いページの領域をちょうど必要なときに回収できる |
| VC-D8 | FSM の構造 | PG の 3 段の最大値の木／固定 2 段（根 + 葉）／作らない（M2-Q17） | **固定 2 段**。根のページ（FSM ブロック 0）が葉ごとの最大値を持ち、葉のページがヒープ 1 ブロックにつき 1 バイト。1 リレーションあたり約 66,585,600 ブロック（約 508 GiB）まで。超えた先は FSM を使わず拡張する。WAL なし、zero-on-error（D16） | 実装が小さく、検索は 2 ページの走査で済む。3 段にするのは M6 |
| VC-D9 | `pd_prune_xid` | 64 ビットを持つ（ページヘッダに入らない）／下位 32 ビットのヒント（M2 §3.3） | **下位 32 ビットのヒント**。上位は `next_xid` から復元する。VACUUM は見ない | ヒントが外れても pruning が遅れるだけで、正しさに影響しない |
| VC-D10 | 行ポインタの再利用 | M2 §3.4 のとおり再利用しない／VACUUM が `LP_UNUSED` にしたものを挿入が再利用する | **再利用する**。`Page::add_item` は最小の `LP_UNUSED` を使う。`PD_HAS_FREE_LINES` は「`LP_UNUSED` が 1 つ以上ある」ことと常に一致させる（hint ではなく、変更のたびに再計算する）。REDO は `Page::add_item_at(offnum, ..)` を使う | A7。PG の `PageAddItem` と同じ選び方なので、共有テストで `ctid` の再利用が一致する |
| VC-D11 | インデックスの空になった葉 | ページ削除・併合（M6）／残す | **残す**。`bulk_delete` は項目を消すだけ。空の葉を読み書きできることは M4 の B+Tree の持ち主（RW-5 が引き継ぐ）が保証し、VC のテストで確かめる | D18・D47 |
| VC-D12 | `yz_relxid` の対象 | 全リレーション／ヒープ（relkind `r`）だけ | **ヒープのテーブルだけ**（カタログ・共有カタログ・`yz_relxid` / `yz_datxid` 自身を含む。インデックス・シーケンスは持たない）。行の欠落は fail-closed（その表の境界を 3 とみなし、clog を切り詰めない。WARNING） | 00 D14。インデックスとシーケンスのページには clog に依存する XID がない |
| VC-D13 | template0 の凍結境界（00 D51 の「template0 も含める」の具体化） | 接続できないので境界が進まない（PG は autovacuum が回る）／initdb が「未変更」の印を付ける | **`yz_datxid` の template0 の行は番兵 `DATFROZEN_PRISTINE`（`i64::MAX`）**。「initdb 以後だれも書けない（接続不可で、`CREATE DATABASE` のコピー元になるだけ）ので、clog を引く XID を持たない」という意味で、全データベースの最小を取る計算に影響しない。`CREATE DATABASE`（09 章）は新しい行を `min(コピー元の値, oldest_xmin())` で作る | template0 は誰も VACUUM できない。番兵がなければ clog が永久に切り詰められない。複製した先は通常のデータベースになる |
| VC-D14 | clog の切り詰めの WAL | `CLOG_TRUNCATE` を WAL に書く（m5-concurrency §12.6）／書かない（00 §3.3） | **書かない**（00 のとおり）。制御ファイルの `oldest_xid` を fsync してからファイルを消す。`Clog::status` は `xid < oldest_xid` を `XX001` にして、消えたページを 0（実行中）として読む事故を防ぐ | 冪等で、起動時にも同じ掃除をする |
| VC-D15 | ANALYZE（VACUUM を伴わないもの）をトランザクションブロックの中で許すか | 00 §5.5 と M4 D-11 は VACUUM と同じく 25001／PG の `vacuum.c` は「ANALYZE（VACUUM なし）はどちらでも動く」 | **許す。外側のトランザクションの中で動き、`execute_standalone` を通らない（`ddl::execute` の `BoundDdl::Analyze`）**。`VACUUM ANALYZE` は 25001。**第 11 節で契約変更を依頼する** | PG17 と同じ（`PG:src/backend/commands/vacuum.c` の `vacuum()` のコメント。【記憶】確認は第 9 節）。VC-D3 の理由 (1) |
| VC-D16 | ANALYZE の範囲 | `pg_statistic` まで（M6 以降）／`reltuples` と `relpages` だけ | **`reltuples` と `relpages` だけ**（`relallvisible` は 0）。列リストは受け付けて存在だけ検査する。`pg_statistic` と `pg_stats` は作らない | プランナは M4 でルールベース。コストベース最適化は M6 以降 |
| VC-D17 | TRUNCATE の M5 対応 | 00 D42 | D42 の 4 点: (1) AccessExclusive（LK-4 が文ごとに取る。ここでは `debug_assert`）、(2) 外部キーから参照されている表の拒否（**07 章の `ddl::constraint::check_truncate_fks(ctx, rels, cascade)` を呼ぶだけ。参照元が同じ TRUNCATE に全部含まれていれば通す（PG と同じ。07 FK-D18）。含まれていない参照元があれば 0A000、`CASCADE` で参照元が文に含まれていない場合も 0A000**。文言・DETAIL・HINT・自己参照は 07 §6.7 が決める。レビュー対応 R-07。00 D42 も同じに直した）、(3) `yz_relxid` の行の作り直し（トランザクションの中の通常の UPDATE。ロールバックで戻る）と FSM の作り直し（新しい relfilenode には FSM のファイルがない）、(4) VACUUM との排他（ロックのモードの衝突で済む。追加の仕組みはない） | 00 D42 の方針（文そのものを 2 回作らない）に従う |
| VC-D18 | 末尾の切り詰め | M6 に回す／条件付き AccessExclusive（D15） | **実装する**。PG と同じ閾値（空きが 1000 ブロック以上、または 1/16 以上）、AccessExclusive は `try_acquire` を 50ms 間隔で最大 5 秒（`ClusterOptions.vacuum_truncate_lock_timeout`）、待ち手が現れたら中断。切り詰めは `exclusive_barrier` の下（D10） | カットライン: VC-6、clog の切り詰め、末尾の切り詰めの順に M6 へ |
| VC-D19 | M2-Q24 の残骸ファイル | VACUUM が回収／起動時掃除／何もしない | **起動時掃除（リカバリの後、接続の受け付けの前）。大きさが 0 バイトで、そのデータベースの `pg_class` のどの行の relfilenode にも当たらない主フォークのファイルだけを消す**。0 バイトでないものは WARNING に出すだけ（00 D31 と同じ方針） | 起動時は実行中のトランザクションがなく、コミット済みの `pg_class` が正しい。VACUUM 時の判定は、別のトランザクションが作成中のファイルを孤児と取り違えるので使えない |
| VC-D20 | autovacuum の統計 | PG の pgstat（永続）／メモリ上の更新件数 | **スレッドローカルに貯めて文の終わりに共有の表へ足す件数**（UPDATE / DELETE した件数）。再起動で消える。対象選択に `autovacuum_freeze_max_age` を足す（clog の切り詰めを進めるため） | ホットパスに共有ロックを入れない |
| VC-D21 | VACUUM の文の区切り | 1 つのトランザクション／テーブルごと | **テーブルごとに `TxnControl::commit_and_restart`**。datfrozenxid の更新と clog の切り詰めは全テーブルの後に 1 回 | 00 §5.5。ロックと登録済みスナップショットがテーブルごとに外れる |
| VC-D22 | TID の集合の上限 | `maintenance_work_mem` | **`maintenance_work_mem` バイト（下限 1MB）を 1 TID 8 バイトで割った個数**。溢れたら第 2・3 段を実行して空にする | 00 §3.6 |
| VC-D23 | カタログ用スナップショットを登録するか | 登録しない（00 D12・§4.3 の `snapshot()`）／文の間は登録する | **文の間 `StatementCatalog` が登録済みスナップショットを持つ（`take_snapshot`）。00 の `snapshot()`（登録しない）は、スキャンが残らない一瞬の用途に限る**。**第 11 節で契約変更を依頼する（重要）** | 登録しないスナップショットで走査中のカタログの行を、同時に走る VACUUM が「全員にとって死んでいる」と判断して消すと、文の途中で行が消える（PG の `GetCatalogSnapshot` は登録する） |
| VC-D24 | VERBOSE の出力の重大度 | NOTICE（m5-concurrency §2 の記述）／INFO | **INFO**（SQLSTATE 00000。`client_min_messages` に関係なく届く） | PG の `vacuumlazy.c` は `INFO`（【記憶】。第 9 節） |

### 0.2 この章で決めないこと

- ロックの取得手順とモード（`ShareUpdateExclusive` ほか）の衝突表: LK（01 章）。この章は「何をどのモードで取るか」だけを書く。
- xmax の読み取り関数（更新者かロック保持者か、MultiXact の展開）と、可視性判定での `XMIN_FROZEN` の扱い: RW（02 章）。この章は「RW が次を提供する」という前提を 4.4 に書く。
- FOREIGN KEY で参照されている表の TRUNCATE の拒否（`check_truncate_fks`。参照元の検出と文言）: FK（07 章 §4.6・§6.7）。
- `CREATE DATABASE` / `DROP DATABASE` が `yz_datxid` の行を作る・消す手順: DB（09 章）。この章は行の値の規則（VC-D13）だけを要求する。

---

## 1. 範囲

### 1.1 対応するもの

| 分類 | 内容 |
|---|---|
| 文 | `VACUUM [(option, ...)] [table [(col, ...)], ...]`、旧形式 `VACUUM [FULL] [FREEZE] [VERBOSE] [ANALYZE] [table ...]`、`ANALYZE [(option, ...)] [table [(col, ...)], ...]`、`ANALYZE [VERBOSE] ...`。オプション: `VERBOSE`・`ANALYZE`・`FREEZE`（受け付けるだけ。常に horizon まで凍結する）・`SKIP_LOCKED`・`INDEX_CLEANUP {ON\|OFF\|AUTO}`・`TRUNCATE`・`ONLY_DATABASE_STATS`・`SKIP_DATABASE_STATS`、受け付けて無視するもの（`DISABLE_PAGE_SKIPPING`・`PROCESS_MAIN`・`PROCESS_TOAST`・`PARALLEL n`・`BUFFER_USAGE_LIMIT x`）。各オプションの後ろに真偽値（`true`/`false`/`on`/`off`/`1`/`0`）を置ける。コマンドタグは `VACUUM` / `ANALYZE` |
| pruning | `satisfies_vacuum`、`HEAP2 PRUNE_FREEZE`（1 ページ 1 レコード）、`pd_prune_xid`、機会的 pruning（スキャン・TID での取得・`hio`） |
| FSM | `Fsm` フォーク、`FreeSpaceMap`、`hio` への組み込み、VACUUM での再構築 |
| VACUUM 本体 | 3 段階（ヒープ走査、`IndexStore::bulk_delete`、`LP_UNUSED`）、TID の集合（`maintenance_work_mem`）、統計の更新、VERBOSE、末尾の切り詰め、テーブルごとのコミット |
| 凍結と clog | 凍結（`XMIN_FROZEN`、xmax の無効化）、`yz_relxid`（OID 9801）、`yz_datxid`（OID 9802）、`pg_class.relfrozenxid` / `pg_database.datfrozenxid` の下位 32 ビットの鏡、制御ファイルの `oldest_xid`、`Clog::truncate_before`、起動時の掃除、`oldest_xid` 未満の REDO の無視 |
| TRUNCATE | 00 D42 の 4 点（VC-D17） |
| autovacuum | 単一スレッドの簡易版（既定は無効。VC-6） |
| 付随 | 行ポインタの再利用（`LP_UNUSED`）、M2-Q24 の残骸ファイルの起動時掃除（VC-D19） |

### 1.2 対応しない（実行すると 0A000 を返す。黙って無視しない）

- `VACUUM FULL`（`VACUUM (FULL)` を含む）: `0A000` `VACUUM FULL is not supported yet`（yuzhu 独自の文言）。
- 可視性マップ・HOT・`LP_REDIRECT`・インデックスのページ削除と再利用・並列 VACUUM・コストベースの遅延（`vacuum_cost_delay` ほか）・`pg_stat_*` / `pg_stat_progress_vacuum`・TOAST の VACUUM（TOAST がない）。設定は受け付けて保存するだけ（`INERT_GUCS`）。
- 権限の検査（所有者または `MAINTAIN`）: GRANT / REVOKE は M6。M5 では全ロールが全テーブルを VACUUM できる（既知の差。`vacuum_is_permitted` の呼び口だけ置く）。
- `TRUNCATE ... CASCADE`: `0A000` `TRUNCATE ... CASCADE is not supported yet`（yuzhu 独自）。

### 1.3 保証すること

- **VACUUM と並行して動く文は、MVCC の結果を変えない**: どの時点のスナップショットから見ても、VACUUM の前後で可視な行の集合は同じ（V2）。
- **インデックスの項目は、`LP_UNUSED` の行ポインタを決して指さない**（V1。クラッシュ後も）。
- **clog の切り詰めの後に、clog を引く必要のあるタプルが残らない**（V3）。
- **FSM はいつ壊れても正しさに影響しない**（V4。壊れたら 0 のページとして読み、VACUUM が作り直す）。
- 各ページの変更は 1 つの WAL レコードで原子的。VACUUM がどこで止まっても（キャンセル・エラー・クラッシュ）、再実行で続きができる（冪等）。

---

## 2. 構成

00 の §2 の木のうち VC の持ち分を再掲し、この章で足す・変えるものを示す。`★` 新規、`△` 変更。

```
impl/rust/crates/yuzhu-core/src/
├── storage/
│   ├── page.rs               △ 行ポインタの操作（mark_dead / mark_unused）、add_item の再利用、add_item_at、free_space、
│   │                           repair_fragmentation、set_prunable、FSM ページの初期化（6.1）
│   ├── fsm.rs                ★ FreeSpaceMap（3.3、5.7、6.2）
│   ├── buffer/mod.rs         △ read_buffer_zero_on_error（6.3）。持ち主は C だが VC-2 の範囲だけ足す（00 §2 の △）
│   ├── heap/
│   │   ├── prune.rs          ★ satisfies_vacuum、PrunePlan、scan_page、apply_plan、機会的 pruning（5.1、5.2）
│   │   ├── vacuum.rs         ★ VACUUM のページ単位の操作（第 1 段と第 3 段。5.3）
│   │   ├── wal2.rs           ★ HEAP2 の PRUNE_FREEZE / VACUUM_UNUSED / INPLACE の形式と REDO（3.5）
│   │   ├── inplace.rs        ★ その場の上書き（固定長の列。5.8）
│   │   ├── modstat.rs        ★ 更新件数のスレッドローカル集計（5.9）
│   │   ├── hio.rs            △ FSM を使った挿入先の選択、入らないときの機会的 pruning（6.4）
│   │   └── scan.rs           △ 機会的 pruning の呼び出し（6.4。00 §8 の表に行がないので 第 11 節で依頼）
│   ├── heap_store.rs         △ fetch での機会的 pruning、inplace_update、更新件数の記録、HeapStore::env()
│   └── btree/vacuum.rs       ★ IndexStore::bulk_delete の実体と BTREE_VACUUM の形式・REDO（5.4）
├── txn/clog.rs               △ oldest の保持と status の検査、truncate_before、起動時の掃除、set_status_redo の無視（6.5。ロックなし化は LK-3）
├── vacuum/                   ★ 新規
│   ├── mod.rs                  VacuumOptions、VacuumEnv、vacuum_relation、analyze_relation、結果の型（4.2）
│   ├── driver.rs               文の振り分け（対象の列挙、テーブルごとの区切り）、datfrozenxid の更新、clog の切り詰め（5.5）
│   ├── autovacuum.rs           autovacuum のスレッドと対象選択（5.9）
│   ├── stats.rs                ModStats（共有の件数表）と対象選択の純関数（5.9）
│   └── orphan.rs               起動時の残骸掃除（5.10）
├── ddl/
│   ├── vacuum.rs             ★ BoundDdl::{Vacuum, Analyze} の実行（5.3、6.7）
│   └── truncate.rs           △ D42 の 4 点（5.6）
├── analyzer/ddl_ext/vacuum.rs ★ BoundVacuum / BoundAnalyze の組み立て（6.7）
├── sql/parser/vacuum.rs      ★ 構文（6.7。00 §8 の表に行がないので 第 11 節で依頼）
├── catalog/
│   ├── schema.rs             △ yz_relxid、yz_datxid の定義（3.4）
│   └── store_vac.rs          ★ impl CatalogStore / SharedCatalogStore（VACUUM に要る読み書き。4.3）
├── settings.rs               △ maintenance_work_mem の読み取り関数、INERT_GUCS への vacuum_* の追加、autovacuum の読み取り専用の値（6.8）
├── engine.rs                 △ autovacuum スレッドの起動と停止、起動時掃除の呼び出し、ClusterOptions の項目（6.8）
├── bootstrap.rs / catalog/rows.rs △ initdb が yz_relxid と yz_datxid の行を書く（3.4。口は F0）
└── recovery.rs               △ HEAP2 と BTREE_VACUUM の振り分け（口は F0。中身はここの REDO 関数）
impl/rust/crates/yuzhu-server/src/config.rs  △ autovacuum の 5 つの設定（6.8）
```

**依存の方向**（00 §2 の図に従う）: `storage::page` ← `storage::buffer` ← `storage::fsm` ← `storage::heap`（`prune`・`vacuum`・`wal2`・`inplace`・`modstat`）← `storage::btree::vacuum` ← `catalog::store_vac` ← `vacuum` ← `ddl::vacuum`・`engine`。`storage::heap::prune` は `txn::{clog, proc_array, multixact}` を使う（`txn` は `storage` の型に依存しない）。`vacuum` は `Cluster`・`DatabaseHandle` を引数で受け取り、`session` を `use` しない。

**この章の規約**（00 §2 の 8 項目に足す）:

1. **VACUUM のページ操作は排他ラッチだけで行う**（VC-D1）。ラッチを持ったままページの外の参照を作らない。
2. **ページを変える関数は「計画を作る（読み取りだけ。`scan_page`）」と「計画を適用する（`apply_plan`。通常の経路と REDO が同じ関数を使う）」に分ける**。REDO が通常の経路と同じバイト列を作ることを単体テストで確かめる（7.2）。
3. **FSM と統計は導出データ**（00 規約 4）。FSM の失敗は `Severity::Panic` 以外ならヒントなしとして続行する。統計の更新の失敗は WARNING にして VACUUM は続ける。
4. **凍結と xmax の無効化の「値の組み立て」は RW の `xmax.rs` の関数を呼ぶ**（00 規約 3）。WAL には結果の値（絶対値）を載せ、REDO は値を書くだけにする。
5. **XID を持たない WAL**: VACUUM の WAL のヘッダの `xid` は `Xid::INVALID`。REDO の `max_xid` の追跡は 0 を無視する。

---

## 3. ディスク上の形式

### 3.1 ページと行ポインタ（M2 §3.3、§3.4 に対する変更）

**行ポインタの状態遷移**（番号は TID のオフセット。`LP_REDIRECT` は M5 でも使わない）:

```text
                 add_item（最小の LP_UNUSED、なければ末尾に追加）
   LP_UNUSED ───────────────────────────────────────────────▶ LP_NORMAL
       ▲                                                           │
       │ 第 3 段（HEAP2 VACUUM_UNUSED）                              │ pruning（HEAP2 PRUNE_FREEZE）
       │ または、インデックスのないテーブルの VACUUM の第 1 段       │ タプルの領域を回収する
       │ （PRUNE_FREEZE の unused）                                 ▼
       └───────────────────────────────────────────────────── LP_DEAD
```

- **正準形**: `LP_UNUSED` と `LP_DEAD` は `lp_off = 0`、`lp_len = 0`。違えば `Page::item_id` が `PageError::BadItem`（`XX001`）にする（M2 の検査に足す）。
- `LP_DEAD` はインデックスの項目が残っているかもしれない印。**`LP_UNUSED` に変えてよいのは、その TID を指すインデックスの項目がすべて消えた後だけ**（5.3.5）。
- **`PD_HAS_FREE_LINES`（`pd_flags` の bit0）は「`LP_UNUSED` の行ポインタが 1 つ以上ある」ことと常に一致させる**（VC-D10）。行ポインタの状態を変える操作（`add_item`・`add_item_at`・`apply_plan`・`vacuum_unused`）が最後に再計算する。`verify` は検査しない（hint）が、クラッシュ試験の整合性検査（V7）が一致を確かめる。`PD_PAGE_FULL`・`PD_ALL_VISIBLE` は M5 でも立てない。
- 行ポインタ配列は縮みうる: `repair_fragmentation` と `vacuum_unused` は、**末尾の連続した `LP_UNUSED` を配列から外す**（`pd_lower` を 4 ずつ戻す）。そのため **TID のオフセットが `max_offset` を超えることは「その行はない」の正常な形**（`LP_DEAD` の後ろは外さない）。TID を覚えて後で読み直す呼び出し元（RW のタプルロックの待ちの後、`fetch`）は、範囲外と `LP_NORMAL` でない行ポインタを「消えた」として扱うこと。
- `MAX_HEAP_TUPLES_PER_PAGE`（185）は行ポインタ配列の長さの上限のまま。`LP_UNUSED` を再利用するので、タプル数が 185 を超えることはない。

**`pd_prune_xid`**（M2 §3.3 の予約を使い始める）:

| 項目 | 規則 |
|---|---|
| 意味 | ページ内で**最も古い、まだ回収できない削除・更新の XID** の下位 32 ビット（PG の `PageSetPrunable`）。0 = ヒントなし |
| 書く者 | (1) ヒープの `delete` / `update`（RW の関数が xmax を書くときに `Page::set_prunable(xid)`: 現在値が 0 か、`xid` の下位 32 ビットがより小さいとき更新。比較は符号付き 32 ビットの差で行う（PG の `TransactionIdPrecedes`））、(2) pruning（`apply_plan` が `PrunePlan.prune_xid` を書く。値は 5.2 の規則） |
| 読む者 | 機会的 pruning の起動条件だけ。**VACUUM は見ない**（ヒントが外れても VACUUM は全ページを処理する） |
| 64 ビットへの復元 | `restore(h32, next_xid)`: `full = (next_xid & !0xFFFF_FFFF) \| h32`、`full > next_xid` なら `full -= 2^32`。`next_xid` から 2^32 以上古い XID は誤って「新しい」と復元されうるが、pruning が起こらないだけで害はない |
| REDO | `HEAP DELETE` / `HEAP UPDATE` の REDO（M3 §6.4.4）は、旧版の xmax を書くときに `set_prunable(xmax)` も行う（RW。第 11 節に依頼）。`PRUNE_FREEZE` の REDO はレコードの値を書く |

**ページの整理（`Page::repair_fragmentation`。決定的）**: `apply_plan` と REDO が同じ関数を呼び、同じ入力から同じバイト列を作る。

```text
repair_fragmentation(page):
  1. LP_NORMAL の行ポインタを (offnum, lp_off, lp_len) で集め、lp_off の降順に並べる
  2. new_upper = pd_special。順に: size = MAXALIGN(lp_len)、new_upper -= size、
     先頭 lp_len バイトを元の位置から copy_within（重なってよい。移動先 >= 移動元なので、降順なら未処理のタプルを上書きしない）してから、
     パディング [new_upper + lp_len, new_upper + size) を 0 にする、
     行ポインタの lp_off = new_upper
  3. [pd_lower, new_upper) を 0 で埋め、pd_upper = new_upper
  4. 末尾の連続した LP_UNUSED を外す（pd_lower -= 4 ずつ）。そのあと [pd_lower, pd_upper) を 0 で埋める
  5. PD_HAS_FREE_LINES = (LP_UNUSED が 1 つ以上)
```

- 領域とパディングを 0 で埋めるのは、REDO と通常の経路でページのバイト列を完全に一致させるため（クラッシュ試験とページ比較のテストが使う）。
- 例（M2 §3.7 のタプル 3 個。`lp_len = 51`、各 56 バイト）。初期状態は `pd_lower = 36`、`pd_upper = 8024`、行ポインタ 1〜3 は `LP_NORMAL` で `lp_off` が 8136・8080・8024（バイト列 `C8 9F 66 00`・`90 9F 66 00`・`58 9F 66 00`）。タプル 2 の xmax がコミット済みで horizon より古いので pruning すると、行ポインタ 2 は `LP_DEAD`（`00 80 01 00`）、タプル 3 が 8080 に詰め直され（行ポインタ 3 = `90 9F 66 00`）、`pd_lower = 36`、`pd_upper = 8080`（`90 1F`）。その後の第 3 段で行ポインタ 2 が `LP_UNUSED`（`00 00 00 00`）になり、末尾ではないので外れず、`pd_flags = 0x0001`。この 3 つの状態はそのまま単体テストの固定値にする。

### 3.2 タプルの凍結（M2 §3.5 の infomask の使い方に対する変更）

00 §3.7 のとおり。この章が書くのは次の 2 つだけ。

| 変更 | 値 |
|---|---|
| 凍結（xmin） | `t_infomask \|= HEAP_XMIN_COMMITTED \| HEAP_XMIN_INVALID`（= 0x0300。**両ビット**）。`t_xmin` の値は残す。単独のビットは M5 でも書かない。**`xmin` システム列は凍結済みなら 2（`FrozenTransactionId`）を返す**（PG の `HeapTupleHeaderGetXmin`。RW が `HeapTuple.xmin` に反映する。第 11 節で依頼） |
| xmax の無効化 | `t_xmax = 0`、`t_infomask` の `XMAX_KEYSHR_LOCK \| XMAX_EXCL_LOCK \| XMAX_LOCK_ONLY \| XMAX_IS_MULTI \| XMAX_COMMITTED` を落として `XMAX_INVALID` を立てる、`t_infomask2` の `KEYS_UPDATED` を落とす。`t_cmax`・`t_ctid` はそのまま（RW の `xmax.rs` の関数が組み立てる。00 規約 3） |

- 凍結済みの xmin は clog を引かず「コミット済みで全員に見える」。XID が `Xid::BOOTSTRAP`（1）・`Xid::FROZEN`（2）のタプルは凍結済みと同じ扱い（凍結の対象にしない。M2 のとおり clog を引かない）。
- `XMIN_FROZEN` と `HEAP_XMIN_COMMITTED` / `XMIN_INVALID` の片方だけが立ったタプルは、M5 でも読んでも無視する（00 D13）。

### 3.3 FSM（`Fsm` フォーク）

**ファイル**: `<relfilenode>_fsm`（M2 §3.1。セグメントの規則は main と同じ）。リレーションごとに 1 つ。ヒープのテーブルだけが持つ（インデックスは M5 で FSM を使わない）。ファイルは最初の `record` で作る（`smgr.create`。WAL なし）。リレーションを消せば `unlink` が全フォークを消す（M2 D13）。

**ページの形**: 8192 バイト。標準のページヘッダを持つので、バッファプールの検証とチェックサムがそのまま使える（`Page::init_fsm`）。

| オフセット | 大きさ | 名前 | 値 |
|---|---|---|---|
| 0 | 8 | `pd_lsn` | 常に 0（WAL を書かない） |
| 8 | 2 | `pd_checksum` | 書き出し時に入る（M2 §6.3 の `flush_frame`） |
| 10 | 2 | `pd_flags` | 0 |
| 12 | 2 | `pd_lower` | 32 |
| 14 | 2 | `pd_upper` | 8192 |
| 16 | 2 | `pd_special` | 8192 |
| 18 | 2 | `pd_pagesize_version` | `0x2001`（`01 20`） |
| 20 | 4 | `pd_prune_xid` | 0 |
| 24 | 1 | `kind` | 1 = 根、2 = 葉 |
| 25 | 1 | 予約 | 0 |
| 26 | 2 | `next_slot` | 次の検索を始める位置（u16 LE。負荷の分散用のヒント）。根では葉の番号、葉ではスロット番号 |
| 28 | 4 | 予約 | 0 |
| 32 | 8160 | `data[8160]` | 下記 |

- **カテゴリ**: 1 バイトで「空き容量 / 32（切り捨て）」。`avail_to_cat(free) = min(free / 32, 255)`（`free` は `Page::free_space()` で、行ポインタ 1 つ分を引いた値）。要求側は `needed_to_cat(n) = min(ceil(n / 32), 255)`。要求側を切り上げ、記録側を切り捨てるので、**カテゴリが要求以上なら実際の空きは要求以上**（`needed` が 8160 を超えるときだけ例外。`hio` が実際のページで確かめる）。
- **葉のページ**（FSM ブロック `1 + L`、`L = 0, 1, …`）: `data[j]` がヒープのブロック `L * 8160 + j` のカテゴリ。記録のないブロックは 0（空きなし）。
- **根のページ**（FSM ブロック 0）: `data[L]` が葉 `L` の `data` の最大値（上限。古くて大きすぎることはあるが、小さすぎない）。
- **容量**: 根が 8160 の葉を持ち、葉が 8160 ブロックを持つ。`FSM_MAX_BLOCKS = 8160 × 8160 = 66,585,600` ブロック（約 508 GiB）。これ以上のブロックは FSM に載らず（`record` は何もせず、`search` は返さない）、`hio` は拡張する。
- **全 0 のページ**（拡張直後、または zero-on-error の結果）は「`kind` 未設定・全カテゴリ 0」。最初の書き込みで `kind` を設定する。
- 変更は `page_mut_hint()`（M3 の `set_lsn` なしの検査から外れる。ヒントの変更）で行い、WAL は書かない。torn write はチェックサム不一致になり、zero-on-error で 0 のページとして読まれる。**FSM のページが WAL・REDO の対象になることはない**。

**例**: ヒープが 3 ブロックで、空きが順に 8164（空のページ）・0（満杯）・100 バイトのとき、葉 0 の `data` は `FF 00 03 00 …`（`8164/32 = 255`、`0`、`100/32 = 3`）、根の `data[0]` は `FF`。葉のページの先頭 40 バイトは次のとおり（チェックサムは `XX XX`）:

```text
00..07  00 00 00 00 00 00 00 00        pd_lsn
08..09  XX XX                          pd_checksum
10..11  00 00                          pd_flags
12..13  20 00   14..15  00 20   16..17  00 20   pd_lower=32, pd_upper=8192, pd_special=8192
18..19  01 20                          pd_pagesize_version
20..23  00 00 00 00                    pd_prune_xid
24..31  02 00 00 00 00 00 00 00        kind=2（葉）, 予約, next_slot=0, 予約
32..35  FF 00 03 00                    data[0..3]
```

### 3.4 `yz_relxid`（OID 9801）と `yz_datxid`（OID 9802）

どちらも `catalog/schema.rs` に定義する通常のヒープのカタログで、`pg_class` と `pg_attribute` に行を持つ（CLAUDE.md「カタログは SQL で問い合わせ可能」）。

| カタログ | OID | 置き場所 | 列（順序どおり。すべて NOT NULL） | 行の意味 |
|---|---|---|---|---|
| `yz_relxid` | 9801 | データベースごと（`base/<db>/9801`。マップしない: `relfilenode = 9801`） | `relid oid`、`relfrozenxid8 int8` | ヒープのテーブル 1 つにつき 1 行。`relfrozenxid8` が `relid` の凍結境界 |
| `yz_datxid` | 9802 | 共有（`global/9802`。`RelFileLocator` の `db_oid = 0`、`reltablespace = 1664`、`relisshared = true`） | `datid oid`、`datfrozenxid8 int8` | データベース 1 つにつき 1 行。`datfrozenxid8` がそのデータベースの凍結境界 |

- `pg_class` の行: `relnamespace = 11`（`pg_catalog`）、`relkind = 'r'`、`relpersistence = 'p'`、`relnatts = 2`、`reltype = 0`（`pg_type` に行は作らない）、`relhasindex = false`（M5 はこの 2 つにインデックスを作らない）。`pg_attribute` の行は通常の列 2 つとシステム列。2 つのカタログの行は `psql` の `\d` や共有テストの対象にならない（比較する共有テストは作らない）。
- **値の型**: `int8` に `Xid.0 as i64` を入れる（XID は 2^63 に達しない）。表示用の鏡: `pg_class.relfrozenxid = Xid::to_external(relfrozenxid8)`、`pg_database.datfrozenxid = Xid::to_external(datfrozenxid8)`（下位 32 ビット。番兵の行は `3` のまま）。
- **`relfrozenxid8` の意味**（不変条件 R1・R2）: テーブル T の `relfrozenxid8 = F` なら、T のすべてのタプルについて、R1: `xmin < F` ならば `xmin` が `BOOTSTRAP` / `FROZEN`、または `XMIN_FROZEN` が立っている。R2: `xmax < F` かつ `xmax` が有効ならば、そのタプルは存在しない（pruning で除去済み）か、`xmax` は無効化済み。つまり **`F` 未満の XID について clog を引く必要がない**。
- **`datfrozenxid8` の意味**: そのデータベースの全ヒープのテーブルの `relfrozenxid8` の下限（以下）。番兵 `DATFROZEN_PRISTINE = Xid(i64::MAX as u64)` は VC-D13。
- **行のライフサイクル**:

| 契機 | `yz_relxid` | `yz_datxid` | 実行者 |
|---|---|---|---|
| initdb | schema の全ヒープのカタログ（`yz_relxid` 自身を含む）に `(oid, 3)` | `(1, 3)`（template1）、`(4, DATFROZEN_PRISTINE)`（template0）、`(5, 3)`（postgres） | VC が `rows.rs` に初期行を足す（口は F0） |
| `CREATE TABLE` | `(oid, 作成したトランザクションの XID)` を挿入（トランザクションの中の通常の INSERT） | — | VC が `store_vac::insert_relxid` を提供し、F0 が `ddl/table.rs` の呼び出しを足す |
| `DROP TABLE` | 行を削除（通常の DELETE） | — | 同上（`delete_relxid`） |
| `TRUNCATE` | 行を更新（通常の UPDATE）して `relfrozenxid8 = 実行したトランザクションの XID`（新しいファイルには凍結が要るタプルがない） | — | VC（`ddl/truncate.rs`） |
| VACUUM | その場の上書き（5.5、5.8） | 同（5.5、5.8） | VC |
| `CREATE DATABASE` | コピーしたファイルにそのまま入っている（各テーブルの値は元のまま） | 行を挿入: `min(コピー元の値, oldest_xmin())` | DB（09 章）。この規則は VC が要求する |
| `DROP DATABASE` | — | 行を削除 | DB |

- **行の欠落**（`pg_class` にあるヒープのテーブルに `yz_relxid` の行がない）: `datfrozenxid8` の計算でその表を `Xid(3)` とみなし（clog を切り詰めない。fail-closed）、WARNING `yz_relxid has no row for table "x"`（ログと通知）を出す。VACUUM は行を補充しない（補充には XID が要り、VACUUM は XID を持たない。VC-D2）。欠落は CREATE TABLE の呼び出しの漏れという不具合の印で、テストで検出する（7.2）。

### 3.5 WAL（00 §3.3 の割り当ての詳細）

`RmgrId::Heap2`（6）の info は上位 4 ビット（`HEAP_INIT_PAGE` 相当のフラグは使わない）。`BTREE`（4）の `0x20`。すべて `xid = Xid::INVALID`。ブロック参照は `RegFlags::STANDARD`（M3 の FPW の規則がそのまま効く。REDO 点の後の最初の変更に画像が付く）。画像が付いたときのブロックデータは省かれる（M3 §3.3）。**画像が付いたブロックは REDO が画像を復元するだけで、そのレコードのメインデータもブロックデータも使わない**（画像はそのレコードの変更を含む）。画像がないときは、メインデータとブロックデータの両方で `apply_plan` を行う。

| rmgr | info | 名前 | ブロック | 持ち主 |
|---|---|---|---|---|
| Heap2 | `0x00` | `PRUNE_FREEZE` | blk0 = ヒープのページ（ブロックデータあり） | `heap/wal2.rs` |
| Heap2 | `0x10` | `VACUUM_UNUSED` | blk0 = ヒープのページ（ブロックデータあり） | 同 |
| Heap2 | `0x30` | `INPLACE`（**契約変更の依頼**。`0x20 MULTI_INSERT` は予約のまま） | blk0 = ヒープのページ（メインデータだけ） | 同 |
| Btree | `0x20` | `VACUUM` | blk0 = 葉のページ（ブロックデータあり） | `btree/vacuum.rs` |

**`PRUNE_FREEZE`**（1 ページ 1 レコード。計画が空なら書かない）:

| | オフセット | 型 | 名前 | 内容 |
|---|---|---|---|---|
| メイン（16 バイト） | 0 | u16 | `ndead` | `LP_DEAD` にする行ポインタの数 |
| | 2 | u16 | `nunused` | `LP_UNUSED` にする行ポインタの数（現在 `LP_NORMAL` でも `LP_DEAD` でもよい） |
| | 4 | u16 | `nfrozen` | 凍結（と xmax の無効化）するタプルの数 |
| | 6 | u16 | `flags` | 0（予約） |
| | 8 | u32 | `prune_xid` | 適用後の `pd_prune_xid`（0 = なし） |
| | 12 | u32 | 予約 | 0 |
| ブロックデータ | 0 | `[u16; ndead]` | `dead` | オフセット番号（昇順） |
| | | `[u16; nunused]` | `unused` | 同（昇順。`dead` と重ならない） |
| | | `[FreezeEntry; nfrozen]` | `frozen` | 8 バイトずつ（下表）。オフセット番号の昇順 |

`FreezeEntry`（8 バイト）: `offnum u16`、`infomask u16`（適用後の絶対値）、`infomask2 u16`（同）、`flags u16`（bit0 = `clear_xmax`: `t_xmax` を 0 にする）。

REDO は `apply_plan` と同じ関数（3.1 の `repair_fragmentation` を含む）で、`dead` → `unused` → `frozen` → `repair_fragmentation` → `pd_prune_xid = prune_xid` の順に行う。`dead` の行ポインタは `LP_NORMAL`（`LP_DEAD` でも許す: 冪等）、`unused` は `LP_NORMAL` か `LP_DEAD`、`frozen` は `LP_NORMAL` でなければならない。違えば `Severity::Panic`（M3 §6.4 と同じ。ページとレコードの食い違い）。

**例**: ページの行ポインタ 2 と 5 を `LP_DEAD` にし、行ポインタ 3 のタプル（`infomask = 0x0803`、`infomask2 = 4`）を凍結し、`pd_prune_xid = 756` にする。

```text
メイン  : 02 00  00 00  01 00  00 00  F4 02 00 00  00 00 00 00        （16 バイト）
ブロック: 02 00 05 00                                                  dead = [2, 5]
         03 00  03 0B  04 00  00 00                                    frozen[0] = {offnum 3, infomask 0x0B03, infomask2 4, flags 0}
```

**`VACUUM_UNUSED`**: メイン 8 バイト（`nunused u16`、`flags u16` = 0、予約 `u32`）、ブロックデータ `[u16; nunused]`（昇順）。REDO: 各行ポインタを `LP_UNUSED` にし（`LP_DEAD` でなければ `Panic`。`LP_UNUSED` は冪等として許す）、`Page::finish_unused()`（末尾の `LP_UNUSED` を外し、`PD_HAS_FREE_LINES` を再計算。タプルは動かさない）。`pd_prune_xid` は変えない。例: 行ポインタ 2 と 5 → メイン `02 00 00 00 00 00 00 00`、ブロックデータ `02 00 05 00`。

**`INPLACE`**（その場の上書き）: メインデータは 8 バイトのヘッダ（`offnum u16`、`npatch u16`、予約 `u32`）の後に、`npatch` 個の `{ byte_off u16, len u16, bytes[len] }`（パディングなし）。`byte_off` はタプルの先頭（`t_xmin` の位置）からのバイト数。REDO: `offnum` のタプルが `LP_NORMAL` で、`byte_off + len` がタプルの長さ以内であることを確かめて（違えば `Panic`）、バイトを書く。ブロックデータはない（画像が付いても、メインデータは REDO に使わない。画像がページの最新の状態を持つ）。例: `pg_class` の行（`t_hoff = 40`。NULL ビットマップあり）の `relpages` を 1、`reltuples` を 50.0 にする。`relpages` は先頭から 136、`reltuples` は 140 の位置（`oid` 40、`relname` 44〜107、`relnamespace` 108、`reltype` 112、`reloftype` 116、`relowner` 120、`relam` 124、`relfilenode` 128、`reltablespace` 132。実装は列の整列から計算し、この値を単体テストの固定値にする）。

```text
メイン: 07 00  02 00  00 00 00 00   88 00 04 00  01 00 00 00   8C 00 04 00  00 00 48 42      offnum=7, 2 つのパッチ
```

**`Btree VACUUM`（`0x20`）**: blk0 = 葉のページ。メイン 8 バイト（`ndeleted u16`、`flags u16` = 0、予約 `u32`）、ブロックデータ `[u16; ndeleted]`（削除する行ポインタの**削除前の**オフセット番号、昇順）。REDO: 行ポインタとタプルを削除して詰める（PG の `PageIndexMultiDelete` 相当。後ろの行ポインタの番号は前に詰まる）。例: 3・7・8 を消す → メイン `03 00 00 00 00 00 00 00`、ブロックデータ `03 00 07 00 08 00`。**右端でない葉の 1 番（high key）は決して消さない**（M4 §13.5）。

- `yuzhu-waldump`（M3 の `wal::dump`）のために、`wal2::describe(rec) -> String` と `btree::vacuum::describe(rec) -> String` を提供する（M4 §13.4 の規約）。

### 3.6 制御ファイルと `pg_xact`

- 制御ファイルの `oldest_xid`（オフセット 80。M2 §3.2 の予約）を使い始める。意味: **これより小さい XID の clog は引かれない（引いたら `XX001`）**。値は単調に増える。initdb は 3。チェックポイントレコードの `oldest_xid`（M3 §3.6）にはチェックポイントが読んだ制御ファイルの値を入れる（情報用。リカバリは制御ファイルの値を使う）。
- `pg_xact` のセグメントは 1 ファイル 1,048,576 個の XID（32 ページ × 32768）。**セグメント `s` を消してよい条件は `(s + 1) × 1,048,576 <= oldest_xid`**（そのセグメントのすべての XID が `oldest_xid` より小さい）。
- 順序（5.5）: WAL の flush → 制御ファイルの `oldest_xid` を fsync → メモリ上の `oldest` を更新 → ファイルを消す → `pg_xact/` の `sync_dir`。起動時は制御ファイルの値を読んで、残っている古いセグメントを消す（冪等）。

---

## 4. 共通の型（契約）

00 の §4.5（`FreeSpaceMap`、`TidSet`、`IndexStore::bulk_delete`）と食い違わない。足りないものは追加してよい。00 と違うものは 第 11 節に挙げる。

### 4.1 storage

```rust
// storage/page.rs（VC。M2 §6.4 の Page に足す・変える）
impl Page {
    /// 行ポインタを LP_DEAD（lp_off = 0、lp_len = 0）/ LP_UNUSED（同）にする。PD_HAS_FREE_LINES は呼び出し側が最後に直す
    pub fn mark_dead(&mut self, off: u16);
    pub fn mark_unused(&mut self, off: u16);
    /// 変更: 最小の LP_UNUSED を再利用する。なければ末尾に足す。入らなければ None。終わりに PD_HAS_FREE_LINES を再計算する
    pub fn add_item(&mut self, data: &[u8]) -> Option<u16>;
    /// REDO 用。off が LP_UNUSED か max_offset + 1 のときだけ置ける。違えば false（REDO が Panic にする）
    pub fn add_item_at(&mut self, off: u16, data: &[u8]) -> bool;
    /// 変更（PageGetHeapFreeSpace）: 新しいタプルに使える最大の MAXALIGN 済みの大きさ。
    /// LP_UNUSED があれば pd_upper - pd_lower、なければ行ポインタ 1 つ分を引く。行ポインタが 185 個あって LP_UNUSED もなければ 0
    pub fn free_space(&self) -> usize;
    pub fn has_unused_item(&self) -> bool;
    /// 3.1 の手順。決定的。失敗は PageError::BadItem（壊れたページ）
    pub fn repair_fragmentation(&mut self) -> std::result::Result<(), PageError>;
    /// 末尾の LP_UNUSED を外し、PD_HAS_FREE_LINES を再計算する（タプルは動かさない）
    pub fn finish_unused(&mut self);
    /// PageSetPrunable。現在値が 0、または xid の下位 32 ビットが現在値より小さい（符号付き 32 ビットの差が負）ときだけ書く
    pub fn set_prunable(&mut self, xid: Xid);
    pub fn set_prune_xid(&mut self, v32: u32);
    /// 64 ビットに復元（3.1）。0 は None
    pub fn prune_xid_full(&self, next_xid: Xid) -> Option<Xid>;
    pub fn init_fsm(&mut self, kind: FsmPageKind);            // 3.3 のヘッダ。data は 0
    pub fn fsm_kind(&self) -> Option<FsmPageKind>;            // 全 0 のページ（kind = 0）は None
    pub fn fsm_data(&self) -> &[u8];  pub fn fsm_data_mut(&mut self) -> &mut [u8];   // 8160 バイト
}
// item_id() は LP_DEAD / LP_UNUSED の lp_off と lp_len が 0 でなければ BadItem（正準形）
```

```rust
// storage/fsm.rs（VC-2）。00 §4.5 の 3 関数に、バッチと検査用を足す
pub const FSM_CAT_STEP: usize = 32;
pub const FSM_SLOTS_PER_PAGE: usize = 8160;
pub const FSM_MAX_BLOCKS: u64 = 66_585_600;                 // 8160 * 8160
pub fn avail_to_cat(free: usize) -> u8;                       // min(free / 32, 255)
pub fn needed_to_cat(needed: usize) -> u8;                    // min(ceil(needed / 32), 255)
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum FsmPageKind { Root = 1, Leaf = 2 }

#[derive(Debug)]
pub struct FreeSpaceMap { /* pool: Arc<BufferPool> */ }
impl FreeSpaceMap {
    pub fn new(pool: Arc<BufferPool>) -> FreeSpaceMap;
    /// needed（MAXALIGN 済みのタプルの大きさ。Page::free_space と同じ単位）以上の空きがあるとされるブロック。
    /// なければ None。FSM が読めない・存在しないときも None（FSM を理由に失敗しない。Severity::Panic だけ伝える）
    pub fn search(&self, rel: RelFileLocator, needed: usize) -> Result<Option<BlockNumber>>;
    /// ブロックの空き（Page::free_space の値）を記録する。FSM_MAX_BLOCKS 以上のブロックは何もしない
    pub fn record(&self, rel: RelFileLocator, blk: BlockNumber, free_bytes: usize) -> Result<()>;
    /// 同じ葉に属するものを 1 回のラッチでまとめて書く（VACUUM 用。内部でブロック番号順に整列する）
    pub fn record_batch(&self, rel: RelFileLocator, entries: &[(BlockNumber, usize)]) -> Result<()>;
    /// nblocks 以降のカテゴリを 0 にし、根を直し、FSM のファイルを葉 ceil(nblocks / 8160) までに切り詰める（WAL なし）
    pub fn truncate(&self, rel: RelFileLocator, nblocks: BlockNumber) -> Result<()>;
    pub fn category(&self, rel: RelFileLocator, blk: BlockNumber) -> Result<u8>;      // テストと V4 の検査用
}
```

```rust
// storage/buffer/mod.rs（VC-2。C の範囲に足す 1 関数）
impl BufferPool {
    /// FSM のページ用。ブロックがなければ Ok(None)。ヘッダ・チェックサムの検査に失敗したページは 0 のページとして
    /// 有効にして返す（WARNING をログ。dirty にしない。次の記録で正しい内容になる）。I/O エラーはそのまま Err
    pub fn read_buffer_zero_on_error(self: &Arc<Self>, tag: BufferTag) -> Result<Option<PinnedBuffer>>;
}
```

```rust
// storage/heap/prune.rs（VC-1）。HeapTupleSatisfiesVacuum は RW の visibility.rs ではなくここに置く
/// **`impl RunningXids for ProcArray` は VC-1 が `storage/heap/prune.rs` に書く**（トレイトがここにあり、`txn` は `storage` を `use` できないため。01 LK-D28・§4.4・§11-17 の返答に合わせた。レビュー対応 R-11）。LK-3 は `ProcArray` の固有メソッド（`is_in_progress`・`next_xid`・`oldest_xmin`・`oldest_xmin_hint`）だけを用意し、impl はそれを呼ぶ 1 行ずつ。VC-1 は LK-3 を待たず、テストはフェイクで進める
pub trait RunningXids: std::fmt::Debug {
    fn is_in_progress(&self, x: Xid) -> bool;
    fn next_xid(&self) -> Xid;
    fn oldest_xmin(&self) -> Xid;
    fn oldest_xmin_hint(&self) -> Xid;        // 下限の写し。ロックなし。単調に増える
}
#[derive(Clone, Copy, Debug)]
pub struct HeapEnv<'a> { pub clog: &'a Clog, pub procs: &'a dyn RunningXids, pub multixact: &'a MultiXactTable }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VacTuple { Dead, RecentlyDead, Live, InsertInProgress, DeleteInProgress }
/// 5.1。horizon は oldest_xmin()
pub fn satisfies_vacuum(env: &HeapEnv<'_>, t: &TupleHeader, horizon: Xid) -> Result<VacTuple>;

#[derive(Clone, Copy, Debug)]
pub struct PruneParams {
    pub horizon: Xid,
    /// Some なら、この XID 未満のコミット済みの xmin を凍結し、この XID 未満の古い xmax を無効化する（VACUUM）。None は凍結しない（機会的）
    pub freeze_cutoff: Option<Xid>,
    /// LP_DEAD にする代わりに直接 LP_UNUSED にする（インデックスのないテーブルの VACUUM。既存の LP_DEAD も LP_UNUSED にする）
    pub mark_unused_now: bool,
    /// ANALYZE を外側のトランザクションで動かすときの自分の XID（自分が挿入した行を live として数える）
    pub own_xid: Option<Xid>,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FreezeEntry { pub offnum: u16, pub infomask: u16, pub infomask2: u16, pub clear_xmax: bool }

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrunePlan {
    pub dead: Vec<u16>, pub unused: Vec<u16>, pub frozen: Vec<FreezeEntry>,
    pub prune_xid: u32,           // 適用後の pd_prune_xid（下位 32 ビット。0 = なし）
}
impl PrunePlan { pub fn is_empty(&self) -> bool; }                   // 空でも prune_xid がページと違えば is_empty = false

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageCounts {
    pub live: u32,                // LIVE + DELETE_IN_PROGRESS（+ 自分の INSERT_IN_PROGRESS）
    pub recently_dead: u32,       // 「dead but not yet removable」
    pub insert_in_progress: u32,
    pub made_dead: u32,           // この計画で領域を回収する（LP_DEAD / LP_UNUSED にする）タプル
    pub frozen: u32,
    pub lpdead_total: u32,        // 適用後の LP_DEAD の数（既にあったものを含む）
}
pub struct PageScan { pub plan: PrunePlan, pub counts: PageCounts, pub lpdead_after: Vec<u16> }

/// 読み取りだけ（共有ラッチでも呼べる）。計画と数え上げを返す
pub fn scan_page(env: &HeapEnv<'_>, page: &Page, params: &PruneParams) -> Result<PageScan>;
/// 通常の経路と REDO の共通の適用（3.5 の順序）。ページを変える。呼び出し側は CriticalSection と WAL を担当する
pub fn apply_plan(page: &mut Page, plan: &PrunePlan) -> Result<()>;

/// WAL を書いてページに適用する（M3 §5.1 の規約 1）。排他ラッチを持った呼び出し側が呼ぶ。計画が空なら何もしない
pub fn prune_and_log(pool: &BufferPool, wal: &Wal, rel: RelFileLocator, blk: BlockNumber,
                     guard: &mut PageWriteGuard<'_>, plan: &PrunePlan) -> Result<Option<Lsn>>;
/// 機会的 pruning（VC-D7）。5.2.3 の条件を確かめ、行うなら guard で行う。変更したら true
pub fn prune_opt(env: &HeapEnv<'_>, pool: &BufferPool, wal: &Wal, rel: RelFileLocator, blk: BlockNumber,
                 guard: &mut PageWriteGuard<'_>, horizon: Xid, next_xid: Xid) -> Result<bool>;
```

```rust
// storage/heap/vacuum.rs（VC-3）。VACUUM のページ単位の操作
#[derive(Debug)]
pub struct VacuumPageResult {
    pub counts: PageCounts,
    pub lpdead: Vec<u16>,         // 適用後の LP_DEAD の行ポインタ（mark_unused_now なら空）。TidSet に入れる
    pub free_bytes: usize,        // 適用後の Page::free_space
    pub last_lsn: Option<Lsn>,
}
/// 第 1 段の 1 ページ: 排他ラッチ → scan_page → prune_and_log。is_new のページは何もしない（free_bytes = 8164）
pub fn vacuum_scan_page(env: &HeapEnv<'_>, pool: &Arc<BufferPool>, wal: &Wal, rel: RelFileLocator, blk: BlockNumber,
                        params: &PruneParams) -> Result<VacuumPageResult>;
/// 第 3 段の 1 ページ: 排他ラッチ → 全部 LP_DEAD であることを確かめて LP_UNUSED → VACUUM_UNUSED。戻り値は (適用後の free_space, lsn)
pub fn vacuum_unused_page(pool: &Arc<BufferPool>, wal: &Wal, rel: RelFileLocator, blk: BlockNumber, offsets: &[u16]) -> Result<(usize, Lsn)>;
```

```rust
// storage/heap/wal2.rs（VC-1）
pub const HEAP2_PRUNE_FREEZE: u8 = 0x00;  pub const HEAP2_VACUUM_UNUSED: u8 = 0x10;  pub const HEAP2_INPLACE: u8 = 0x30;
pub struct PruneMain { pub ndead: u16, pub nunused: u16, pub nfrozen: u16, pub flags: u16, pub prune_xid: u32 }
impl PruneMain { pub fn encode(&self) -> [u8; 16]; pub fn decode(b: &[u8]) -> Result<Self>; }
pub fn encode_plan_data(plan: &PrunePlan) -> Vec<u8>;                          // ブロックデータ
pub fn decode_plan(main: &PruneMain, data: &[u8]) -> Result<PrunePlan>;       // 長さの検査（食い違えば Panic 級の XX001）
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()>;                // 3 種類の振り分け（recovery::dispatch から呼ぶ）
pub fn describe(rec: &DecodedRecord) -> String;                               // yuzhu-waldump 用
```

```rust
// storage/heap/inplace.rs（VC-3）と TableStore への追加（00 §4.5 の TableStore に足す 1 メソッド）
#[derive(Clone, Debug)] pub struct InplacePatch { pub attnum: usize /* 1 始まり */, pub value: Datum }   // Int4 / Int8 / Float4 / Oid / Xid だけ
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum InplaceOutcome { Updated, TupleGone }
pub trait TableStore {
    /// 固定長・非 NULL の列の値を、タプルの中で上書きする（PG の heap_inplace_update）。WAL は HEAP2 INPLACE。
    /// TupleGone: tid が LP_NORMAL でない／xmin が expect_xmin と違う／xmax が有効（ロックだけは可）。MVCC は無関係（版を作らない）
    fn inplace_update(&self, rel: &RelHandle, tid: Tid, expect_xmin: Xid, patches: &[InplacePatch]) -> Result<InplaceOutcome>;
}
```

```rust
// storage/heap/modstat.rs（VC-6）。ホットパスは共有ロックを取らない
pub fn note_modified(rel: RelFileLocator, n: u32);                // スレッドローカルに足す。HeapStore の delete / update が成功したときに呼ぶ
pub fn drain() -> Vec<(RelFileLocator, u64)>;                     // このスレッドの分を取り出して空にする

// storage/heap/hio.rs（VC-2。M2 の InsertHints と insert_tuple を置き換える部分）
#[derive(Debug)]
pub struct HioCtx { /* pool, fsm: Arc<FreeSpaceMap>, hints: InsertHints, heap env への参照 */ }
impl HioCtx {
    /// 候補のページを pin して返す（ラッチなし）。順序は 5.7.2 の「ヒント → FSM → 最後のページ → 拡張」。tried は「入らなかった」ブロック
    pub fn find_target(&self, rel: RelFileLocator, tuple_len: usize, tried: &[BlockNumber]) -> Result<PinnedBuffer>;
    /// 排他ラッチを持った呼び出し側が「入らない」と判断したとき。ページの実際の空きを FSM に記録する
    pub fn note_no_fit(&self, rel: RelFileLocator, blk: BlockNumber, free_space: usize);
    /// 入ったとき（挿入先のヒントを更新）
    pub fn note_used(&self, rel: RelFileLocator, blk: BlockNumber);
    /// 排他ラッチを持ったまま、領域が足りないページで機会的 pruning を試みる（VC-D7）。変更したら true
    pub fn try_prune_for_space(&self, rel: RelFileLocator, blk: BlockNumber, guard: &mut PageWriteGuard<'_>) -> Result<bool>;
    pub fn forget(&self, rel: RelFileLocator);                    // unlink_storage から（M2 §6.5.2 と同じ）
}
```

```rust
// storage/btree/vacuum.rs（VC-3）。IndexStore::bulk_delete の実体は BtreeStore が持ち、ここに処理を置く
pub fn bulk_delete(store: &BtreeStore, w: &WriteCtx, index: &IndexHandle, dead: &TidSet, wait: &WaitCtx<'_>) -> Result<BulkDeleteStats>;
pub const BTREE_VACUUM: u8 = 0x20;
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()>;
// TidSet（00 §4.5）に足す
impl TidSet {
    pub fn with_capacity_bytes(bytes: usize) -> TidSet;           // 8 バイト / TID
    pub fn is_full(&self) -> bool;                                // len + MAX_HEAP_TUPLES_PER_PAGE > 容量
    pub fn clear(&mut self);
    /// ブロックごとにまとめたオフセット（昇順）
    pub fn groups(&self) -> impl Iterator<Item = (BlockNumber, &[Tid])> + '_;
}
// push は (block, offset) の昇順でしか呼ばない（呼び出し側の保証。デバッグビルドで検査）。contains は二分探索
```

### 4.2 vacuum（`vacuum/mod.rs`、`analyzer/bound.rs`）

```rust
// analyzer/bound.rs に置く（analyzer は vacuum より下の層。vacuum は再エクスポートする）
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum IndexCleanup { Auto, On, Off }
#[derive(Clone, Debug)]
pub struct VacuumOptions {
    pub verbose: bool, pub analyze: bool, pub freeze: bool, pub skip_locked: bool,
    pub index_cleanup: IndexCleanup, pub truncate: bool,
    pub only_database_stats: bool, pub skip_database_stats: bool,
    pub tid_capacity: Option<usize>,       // テスト用。None = maintenance_work_mem から
}
impl Default for VacuumOptions { /* すべて false、index_cleanup = Auto、truncate = true、tid_capacity = None */ }
#[derive(Clone, Debug)]
pub struct BoundVacuumTarget { pub oid: Oid, pub schema: String, pub name: String, pub columns: Vec<i16> /* attnum。ANALYZE の列リスト */ }
pub struct BoundVacuum { pub options: VacuumOptions, pub targets: Vec<BoundVacuumTarget> }      // targets が空 = データベース全体
pub struct BoundAnalyze { pub verbose: bool, pub skip_locked: bool, pub targets: Vec<BoundVacuumTarget> }
// BoundDdl::Vacuum(BoundVacuum) / BoundDdl::Analyze(BoundAnalyze)。VacuumOptions.analyze が真の Vacuum は ANALYZE も行う
```

```rust
// vacuum/mod.rs（VC-3）
pub struct VacuumEnv<'a> {
    pub cluster: &'a Cluster,
    pub db: &'a Arc<DatabaseHandle>,
    pub backend: BackendId,
    pub wait: &'a WaitCtl<'a>,
    pub interrupts: &'a InterruptFlag,
    pub notices: &'a mut Vec<Notice>,           // VERBOSE（INFO）と WARNING
    pub lock_scope: LockScope,                  // SQL: Transaction（commit_and_restart で外れる）。autovacuum: Session（自分で release）
    pub own_xid: Option<Xid>,                   // 外側のトランザクションで動く ANALYZE だけ
    pub is_autovacuum: bool,                    // 真なら待ち手が現れたら中断する（5.9）
}
#[derive(Clone, Debug, Default)]
pub struct IndexVacStats { pub index_oid: Oid, pub pages: u32, pub tuples_removed: u64, pub tuples_remaining: u64 }
#[derive(Clone, Debug)]
pub struct VacuumRelStats {
    pub rel_pages_start: u32, pub rel_pages_end: u32, pub pages_scanned: u32,
    pub tuples_deleted: u64, pub new_live_tuples: u64, pub recently_dead_tuples: u64,
    pub frozen_tuples: u64, pub frozen_pages: u32, pub lpdead_items: u64, pub pages_with_lpdead: u32,
    pub index_scans: u32, pub index_stats: Vec<IndexVacStats>,
    pub removable_cutoff: Xid, pub old_relfrozenxid: Xid, pub new_relfrozenxid: Xid,
    pub truncated_to: Option<u32>, pub last_lsn: Lsn,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkipReason { NotATable(char), Gone, LockNotAvailable, CancelledByWaiter }
pub enum VacuumRelOutcome { Done(VacuumRelStats), Skipped(SkipReason) }

/// 1 テーブルの VACUUM（5.3）。ShareUpdateExclusive を取るところから。Transaction スコープなら解放は呼び出し側のコミット
pub fn vacuum_relation(env: &mut VacuumEnv<'_>, relid: Oid, opts: &VacuumOptions) -> Result<VacuumRelOutcome>;
/// ANALYZE（5.8）。relpages と reltuples だけ
pub fn analyze_relation(env: &mut VacuumEnv<'_>, relid: Oid, columns: &[i16], verbose: bool, skip_locked: bool) -> Result<()>;
/// 5.5: データベースの凍結境界を進め、進んだら clog を切り詰める。VACUUM の最後と autovacuum の 1 データベースごとに呼ぶ
pub fn update_datfrozenxid_and_truncate_clog(env: &mut VacuumEnv<'_>) -> Result<ClogTruncateOutcome>;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClogTruncateOutcome { pub datfrozenxid_advanced: bool, pub oldest_xid: Xid, pub segments_removed: u32 }
pub const DATFROZEN_PRISTINE: Xid = Xid(i64::MAX as u64);
pub const REL_TRUNCATE_MINIMUM: u32 = 1000;     // PG の REL_TRUNCATE_MINIMUM
pub const REL_TRUNCATE_FRACTION: u32 = 16;      // PG の REL_TRUNCATE_FRACTION
```

```rust
// ddl/vacuum.rs（VC-3）。ddl::execute_standalone（00 §4.7）が呼ぶ
pub fn execute_vacuum(ctl: &mut dyn TxnControl, v: BoundVacuum) -> Result<String>;         // "VACUUM"
/// ANALYZE（VACUUM なし）。外側のトランザクションの中で動く（VC-D15）。ddl::execute の BoundDdl::Analyze の分岐から
pub fn execute_analyze(ctx: &mut DdlCtx<'_>, a: BoundAnalyze) -> Result<String>;           // "ANALYZE"
```

### 4.3 catalog（`catalog/store_vac.rs`、VC-3・VC-4）

```rust
#[derive(Clone, Debug)]
pub struct VacRelInfo {
    pub oid: Oid, pub name: String, pub schema: String, pub relkind: char, pub is_shared: bool,
    pub locator: RelFileLocator, pub relpages: i32, pub reltuples: f32,
}
pub struct RelxidMin { pub min: Xid, pub missing: Vec<String> }       // 行がないテーブルの名前（3.4 の fail-closed）
impl CatalogStore {
    /// relkind 'r' の全リレーション（共有カタログを含む）。oid 昇順
    pub fn list_vacuum_relations(&self, snap: &Snapshot) -> Result<Vec<VacRelInfo>>;
    /// relkind を問わず 1 件（VACUUM idx の WARNING 用）
    pub fn vacuum_relation_info(&self, snap: &Snapshot, oid: Oid) -> Result<Option<VacRelInfo>>;
    pub fn relxid(&self, snap: &Snapshot, relid: Oid) -> Result<Option<Xid>>;
    pub fn min_relxid(&self, snap: &Snapshot) -> Result<RelxidMin>;
    pub fn insert_relxid(&self, w: &WriteCtx, relid: Oid, xid: Xid) -> Result<()>;                 // CREATE TABLE（M4 の ddl/table.rs が呼ぶ）
    pub fn delete_relxid(&self, w: &WriteCtx, snap: &Snapshot, relid: Oid) -> Result<()>;          // DROP TABLE
    pub fn update_relxid(&self, w: &WriteCtx, snap: &Snapshot, relid: Oid, xid: Xid) -> Result<()>; // TRUNCATE（通常の UPDATE）
    /// その場の上書き（5.8）。pg_class の行の relpages / reltuples（と、frozen が Some なら relfrozenxid の下位 32 ビット）と、
    /// frozen が Some なら yz_relxid の行。TupleGone（並行する DDL が行を作り替えた）は Ok(()) で諦める（WARNING をログ。統計は助言で、凍結境界は進まないだけで安全）
    pub fn set_relstats_inplace(&self, snap: &Snapshot, relid: Oid, relpages: i32, reltuples: f32, frozen: Option<Xid>) -> Result<()>;
}
impl SharedCatalogStore {
    pub fn datxid(&self, snap: &Snapshot, db: Oid) -> Result<Option<Xid>>;
    /// 全データベースの最小。番兵は大きい値なので最小に影響しない。pg_database にあって yz_datxid に行がないデータベースは Xid(3) とみなす（fail-closed。WARNING）
    pub fn min_datxid(&self, snap: &Snapshot) -> Result<Xid>;
    pub fn set_datxid_inplace(&self, snap: &Snapshot, db: Oid, v: Xid) -> Result<()>;   // pg_database.datfrozenxid の鏡も同時に
    pub fn insert_datxid(&self, w: &WriteCtx, db: Oid, v: Xid) -> Result<()>;           // CREATE DATABASE（09 章）が呼ぶ
    pub fn delete_datxid(&self, w: &WriteCtx, snap: &Snapshot, db: Oid) -> Result<()>;  // DROP DATABASE（09 章）が呼ぶ
}
```

### 4.4 他の章に前提とするもの

| 章 | 前提とする関数・性質 | 使う場所 |
|---|---|---|
| LK（01） | `TxnManager::{oldest_xmin, take_snapshot, snapshot, exclusive_barrier, finish_without_xid}`、`ProcArray` の固有メソッド `is_in_progress`・`next_xid`・`oldest_xmin`・`oldest_xmin_hint`（`RunningXids` の `impl` は VC-1 が `prune.rs` に書く。4.1。R-11）、`LockManager::{acquire, try_acquire, release, release_all}`（00 §4.2・§4.3）。**足してほしいもの**: `LockManager::has_waiters(id, tag) -> bool`（自分が持つタグを待っている者がいるか）と `ProcArray::oldest_xmin_hint() -> Xid`（`oldest_xmin` の下限の写し。単調に増える。ロックなしで読める）。共有リレーションの `LockTag::Relation` は `db = 0` | 5.3、5.6、5.9、機会的 pruning |
| RW（02） | `storage/heap/xmax.rs`（02 §4.1）の読み取りと無効化: `decode_xmax(&TupleHeader) -> Result<XmaxState>`（`None` / `Locker { xid, mode }` / `LockerMulti(MultiXactId)` / `Updater { xid, keys_updated }`）、`lockers_all_finished(&TupleHeader, &XmaxEnv) -> Result<bool>`、**`invalidate_xmax(&TupleHeader) -> XmaxWrite`（xmax を無効にし、XMAX 系のビットを落として `XMAX_INVALID` を立て、`KEYS_UPDATED` を落とした完成値。3.2 の「xmax の無効化」）**、**`with_xmin_frozen(infomask: u16) -> u16`（`XMIN_FROZEN` の両ビットを立てた値）**。後 2 つは 02 §4.1 に足した（レビュー対応 R-01。02 §11 の依頼 12 の (d)）。`classify_xmax` という名前は使わない。可視性で `XMIN_FROZEN` を最初に見る。`HeapTuple.xmin` は凍結済みなら `Xid::FROZEN`。`delete` / `update` と REDO が `Page::set_prunable` を呼ぶ。TID の読み直しで「範囲外・`LP_NORMAL` でない」を消えたものとして扱う。`add_item` の戻り値が再利用された番号になりうる | 5.1、5.2、3.2 |
| M4（B1・B2・C1） | `IndexHandle`、`RelHandle.indexes`、B+Tree のページ操作（葉の項目を読む・行ポインタを消して詰める関数。なければ `btree/vacuum.rs` に書く）、`catalog.indexes_of(table_oid)` に当たる関数（M4 07 章の名前に従う）、M4 の `ddl/table.rs`（CREATE / DROP TABLE） | 5.3、5.4 |
| FK（07） | **`ddl::constraint::check_truncate_fks(ctx: &DdlCtx<'_>, rels: &[Oid], cascade: bool) -> Result<()>`**（07 §4.6。07 の依頼 R6 が VC-5 に頼んでいる口。`FkRef` と `fk_constraints_referencing` は作らない。レビュー対応 R-07）。FK-4 が差し込むまでは常に `Ok(())` を返すスタブ（F0 が置く） | 5.6 |
| DB（09） | **`CREATE DATABASE` が `pg_database` の行と同じ `WriteCtx` で `insert_datxid(w, new_oid, min(shared.datxid(snap, tpl)?, TxnManager::oldest_xmin()))` を呼び、`DROP DATABASE` が `delete_datxid` を呼ぶ**（VC-D13 の値の規則。09 §3.3・DB-D12・§6.5 は R-08 でこの形に合わせた。`yz_datxid` の読み書きの関数は VC の `store_vac.rs` だけが持つ。09 は `store_db.rs` に作らない）。`CREATE DATABASE` が `datallowconn = false` のテンプレートを許すかは 09 章 | 3.4、5.10 |
| F0 | `Heap2 = 6`、`recovery::dispatch` の振り分けの口、`DatabaseHandle` / `Cluster` から `StorageStack` の `heap` / `fsm` を取る口、`ClusterOptions` の項目、`DebugKnobs` の項目（第 11 節） | |

---

## 5. 処理の流れ

### 5.1 `satisfies_vacuum`（VC-1）

PG の `HeapTupleSatisfiesVacuum`（PG:src/backend/access/heap/heapam_visibility.c）の yuzhu 版。ヒントビットを使わず、MultiXact はメモリ上だけ（00 D6）、HOT なし。

```text
xid_state(env, x, horizon) -> Running | Committed | Aborted:
  if !x.is_normal()            : Committed          // BOOTSTRAP / FROZEN
  if x >= horizon && env.procs.is_in_progress(x) : Running   // 実行中一覧を clog より先に見る（コミットは clog → 一覧から外す の順。00 D38）
  match env.clog.status(x)? { Committed => Committed, _ => Aborted }   // 一覧になく clog が IN_PROGRESS のもの（クラッシュの残り）は中断
  // x < horizon なら終了済みなので一覧を引かない（horizon は実行中の XID の最小以下）

satisfies_vacuum(env, t, horizon):
  // xmin
  if t.xmin_frozen() || !t.xmin.is_normal()  : （コミット済みとして続ける）
  else match xid_state(t.xmin):
      Aborted   => return Dead
      Running   => return InsertInProgress
      Committed => 続ける
  // xmax（RW の decode_xmax。02 §4.1）
  match decode_xmax(t)?:
      XmaxState::None       => Live
      Locker{..} | LockerMulti(_) => Live             // ロックだけの xmax は行の生死に関係しない（古ければ 5.2.1 が無効化する）
      Updater{xid: x, ..} => match xid_state(x):
          Running   => DeleteInProgress
          Aborted   => Live                           // 中断した更新者の xmax は無効（5.2.1 が無効化する）
          Committed => if x < horizon { Dead } else { RecentlyDead }
```

- 呼び出し側が自分の XID（`own_xid`、ANALYZE の外側のトランザクション）を持つときは、`InsertInProgress` で `t.xmin == own_xid` のものを `Live` として数える（PG の ANALYZE と同じ）。それ以外の用途（pruning・VACUUM）は `own_xid = None`。
- `xmax` が有効なのに `XMAX_IS_MULTI` だけで `LOCK_ONLY` がないタプルは `decode_xmax` が `XX001` にする（更新者は MultiXact に入らない。00 D6）。
- 真理値表（単体テストの項目）:

| xmin | xmax | 結果 |
|---|---|---|
| 中断（`xid_state = Aborted`） | 何でも | `Dead` |
| 実行中 | なし | `InsertInProgress` |
| コミット済み（凍結を含む） | なし / ロックだけ | `Live` |
| コミット済み | 更新者が実行中 | `DeleteInProgress` |
| コミット済み | 更新者が中断（クラッシュの残りを含む） | `Live` |
| コミット済み | 更新者がコミット済みで `< horizon` | `Dead` |
| コミット済み | 更新者がコミット済みで `>= horizon` | `RecentlyDead` |
| 凍結 | 更新者がコミット済みで `< horizon` | `Dead`（xmin が clog にもう引けなくても判定できる） |

### 5.2 pruning

#### 5.2.1 計画を作る（`scan_page`。読み取りだけ）

ページの全行ポインタを 1 から `max_offset` まで順に見る。各行ポインタ（`item_id` が `BadItem` なら `XX001`）:

| 行ポインタ | 処理 |
|---|---|
| `LP_UNUSED` | 何もしない |
| `LP_REDIRECT` | `XX001`（M5 は作らない） |
| `LP_DEAD` | `counts.lpdead_total += 1`。`mark_unused_now` なら `plan.unused` に入れる。そうでなければ `lpdead_after` に入れる（既存の `LP_DEAD` も TID の集合に入れるため） |
| `LP_NORMAL` | `satisfies_vacuum` を呼ぶ（下記） |

`LP_NORMAL` の結果ごと:

| 結果 | 処理 |
|---|---|
| `Dead` | `made_dead += 1`。`mark_unused_now` なら `plan.unused` に、そうでなければ `plan.dead` と `lpdead_after` に入れる。凍結はしない |
| `RecentlyDead` | `recently_dead += 1`。`prunable = min(prunable, xmax)`。凍結の対象にはなる |
| `Live` | `live += 1` |
| `InsertInProgress` | 自分の XID なら `live`、そうでなければ `insert_in_progress`。凍結しない |
| `DeleteInProgress` | `live += 1`。`prunable = min(prunable, xmax)` |

**凍結の計画**（`freeze_cutoff = Some(c)`、`Dead` と `InsertInProgress` を除く各タプル。`FreezeEntry` を作る）:

1. xmin: `xmin` が通常の XID で、凍結済みでなく、`xmin < c` なら `infomask |= XMIN_FROZEN`（`xmin < c <= horizon` なので終了済みで、`Dead` でなかったからコミット済み）。
2. xmax（RW の `decode_xmax` / `lockers_all_finished` / `invalidate_xmax`。いずれも 02 §4.1）:
   - `Locker { xid: x, .. }` で `x < c`: 保持者は終了済み。xmax を無効化する。
   - `LockerMulti(id)` で `lockers_all_finished(t, env)` が真（`env.is_running(x) = x >= horizon && procs.is_in_progress(x)`、`env.multi = &MultiXactTable`。中身は `multixact.expand(id, ..)` が `Dead`）: 無効化する。
   - `Updater { xid: x, .. }` で `x < c` かつ `xid_state(x) = Aborted`: 無効化する（中断した更新者）。
   - それ以外（実行中のロック・更新者、`c` 以上の XID）は触らない。
3. 1 の `with_xmin_frozen` と 2 の `invalidate_xmax` が返した `xmax` / `infomask` / `infomask2` の完成値を `FreezeEntry`（絶対値。`clear_xmax = true`）にする（VC は xmax と infomask のビットを自分で組み立てない。00 規約 3）。変更がなければ `FreezeEntry` を作らない。

`plan.prune_xid`: `prunable`（`RecentlyDead` と `DeleteInProgress` の xmax の最小。なければ 0）の下位 32 ビット。`counts.frozen = plan.frozen.len()`。

`plan.dead` / `unused` / `frozen` は昇順で互いに重ならない（`frozen` の行ポインタは `LP_NORMAL` のままのもの）。`PrunePlan::is_empty()` は 3 つの配列が空で、`prune_xid` がページの現在の値と同じとき真。

#### 5.2.2 計画を適用して WAL に書く（`prune_and_log`）

M3 §5.1 の規約 1（検査 → ラッチ → `CriticalSection` → `page_mut` → `RecordBuilder` → `insert` → `set_lsn`）。排他ラッチを持った呼び出し側が呼ぶ。

```text
prune_and_log(pool, wal, rel, blk, guard, plan):
  if plan.is_empty(): return None
  // ここまでに失敗しうる処理（scan_page）は終わっている
  cs = CriticalSection::enter(pool)
    apply_plan(guard.page_mut(), plan).map_err(|e| cs.escalate(e))?          // 3.5 の順序。REDO と同じ関数
    rec = RecordBuilder::new(Heap2, HEAP2_PRUNE_FREEZE, Xid::INVALID)
    b = rec.register_block(BufferTag{rel, Main, blk}, guard.page(), RegFlags::STANDARD)
    rec.block_data(b, encode_plan_data(plan)); rec.main_data(PruneMain{..}.encode())
    ins = wal.insert(rec).map_err(|e| cs.escalate(e))?
    guard.set_lsn(ins.end.0)
  Some(ins.end)
```

- `register_block` は適用後のページを渡す（M3 の FPW の規約。画像はこの後の状態）。
- `apply_plan` の失敗（`BadItem`、`dead` が `LP_NORMAL` でない等）は壊れたページ。クリティカルセクションの中なので `Panic` になる。検査は `scan_page` が済ませている（計画はこのラッチの下で作った）ので、通常は起こらない。
- `unused` の項目は適用時に `LP_NORMAL` でも `LP_DEAD` でもよい。

#### 5.2.3 機会的 pruning（`prune_opt`）

呼ぶ場所と条件は PG の `heap_page_prune_opt`（PG:src/backend/access/heap/pruneheap.c）に倣う。

| 場所 | 手順 |
|---|---|
| **スキャン**（`HeapScan::next_tuple` が 1 ページを読むとき）と **TID での取得**（`HeapStore::fetch`） | ピン → 共有ラッチ → `candidate(page)` を確かめる。真なら共有ラッチを外して `try_write`。取れなければ諦めて共有ラッチを取り直して続ける。取れたら `candidate` をもう一度確かめて `prune_opt`、ガードを外して共有ラッチを取り直して続ける |
| **`hio`**（`HioCtx::try_prune_for_space`） | 排他ラッチを持ったまま「入らない」と判断したとき、同じラッチで `candidate` を確かめて `prune_opt` し、`fits` をもう一度判定する |

```text
candidate(page, procs, next_xid) -> Option<Xid /* horizon */>:
  if page.is_new() or page.prune_xid() == 0:                          return None
  if page.free_space() >= PRUNE_MIN_FREE (= 819 = BLCKSZ / 10):       return None        // PG: max(fillfactor の目標, BLCKSZ/10)
  x = page.prune_xid_full(next_xid)
  h = procs.oldest_xmin_hint()                                        // ロックなし。古いが下限として安全（horizon は単調に増える。5.3.5 の P3）
  if x < h: return Some(h)
  h = procs.oldest_xmin()                                             // proc の Mutex を取る。ページを開くたびに呼ばないよう、上の 2 つを満たしたときだけ
  if x < h: Some(h) else None

prune_opt(.., guard, horizon, next_xid):
  scan = scan_page(env, guard.page(), PruneParams { horizon, freeze_cutoff: None, mark_unused_now: false, own_xid: None })?
  prune_and_log(.., &scan.plan)         // 5.2.2。変更したら true
```

- リカバリ中（REDO）とクラスタが poison されているときは呼ばない。ブロック番号が `nblocks` 以上のページは対象外。
- **horizon の条件**: `oldest_xmin()` は登録済みスナップショットを含むので、**自分の走査のスナップショットで可視なタプルは `Dead` にならない**（5.3.5）。機会的 pruning は凍結しない（`freeze_cutoff = None`）。
- `PRUNE_MIN_FREE` の 819 バイトは、PG の既定（fillfactor 100）で `minfree = max(0, BLCKSZ / 10)` になることから（【記憶】。第 9 節）。

### 5.3 VACUUM 本体（VC-3）

#### 5.3.1 全体

```text
vacuum_relation(env, relid, opts) -> VacuumRelOutcome:
  A. 準備
     tag = Relation { db: is_shared ? 0 : db_oid, rel: relid }
     skip_locked ? try_acquire(backend, tag, ShareUpdateExclusive, env.lock_scope) （取れなければ WARNING
                      `skipping vacuum of "t" --- lock not available`、Skipped(LockNotAvailable)）
                 : acquire(.., env.wait)?                     // 待つ前にラッチ・ピン・ゲートを持たない（00 規約 1）
     info = { snap = take_snapshot(); catalog.vacuum_relation_info(&snap, relid) }   // snap はここで外れる。VACUUM は登録済みスナップショットを持たずに走る
     info == None            → Skipped(Gone)（WARNING `skipping vacuum of "t" --- relation no longer exists`）
     info.relkind != 'r'     → WARNING `skipping "t" --- cannot vacuum non-tables or special system tables`、Skipped(NotATable)
     rel      = RelHandle（インデックスの一覧つき。M4 の関数）
     nindexes = rel.indexes.len()
     horizon  = txn.oldest_xmin()                              // ここから先の判定は、すべてこの値で行う
     cutoff   = horizon                                        // 凍結の境界（VC-D5）
     old_frozen = relxid(relid)（行がなければ Xid(3)。3.4 の fail-closed。補充はしない）
     nblocks0 = pool.nblocks(rel.locator, Main)                // 走査はこのブロックまで（後から増えたページは新しいタプルだけ）
     VERBOSE なら INFO `vacuuming "db.schema.rel"`
  B. 第 1 段（ヒープ走査と pruning・凍結）
     params   = PruneParams { horizon, freeze_cutoff: Some(cutoff), mark_unused_now: nindexes == 0, own_xid: None }
     do_index = nindexes > 0 && opts.index_cleanup != Off
     dead     = TidSet::with_capacity_bytes(opts.tid_capacity.unwrap_or(max(maintenance_work_mem, 1MB)))
     for blk in 0..nblocks0:
         env.interrupts.check()?                               // キャンセル・statement_timeout・停止
         if env.is_autovacuum && blk % 64 == 0 && locks.has_waiters(backend, tag): return Skipped(CancelledByWaiter)
         r = vacuum_scan_page(heap_env, pool, wal, rel.locator, blk, &params)?       // 排他ラッチ（VC-D1）。5.3.2
         統計を足す。free_batch.push((blk, r.free_bytes))；1024 個たまったら fsm.record_batch
         if do_index: for off in r.lpdead: dead.push(Tid { block: blk, offset: off })
         if dead.is_full(): index_and_heap_pass(..)            // 第 2・3 段（途中。5.3.3、5.3.4）
  C. 残りの第 2・3 段
     if !dead.is_empty(): index_and_heap_pass(..)
     fsm.record_batch（残り）
  D. 末尾の切り詰め（opts.truncate。5.6.2）。切り詰めたら rel_pages_end を更新
  E. 統計と凍結境界（5.8、5.5）
     catalog.set_relstats_inplace(snap_now, relid, rel_pages_end, new_live_tuples as f32, Some(max(old_frozen, cutoff)))
     各インデックス: set_relstats_inplace(index_oid, pages, tuples_remaining as f32, None)
     yz_relxid に行がなければ relfrozenxid8 は書かない（補充しない。3.4）
  F. VERBOSE の出力（5.8）。VacuumRelOutcome::Done(stats)（stats.last_lsn は呼び出し側が flush の対象にする）
```

- `ShareUpdateExclusive` は `DdlCtx` / `TxnControl` の暗黙のトランザクションのスコープ（`LockScope::Transaction`）で取る。`commit_and_restart` で全部外れる（00 §5.5）。
- VACUUM は **XID を持たない**（VC-D2）ので、`WriteCtx` は `{ xid: Xid::INVALID, cid: 0 }`。
- **インデックスの一覧は VACUUM の間変わらない**: CREATE INDEX（Share）・DROP INDEX（テーブルの AccessExclusive）・ADD CONSTRAINT は ShareUpdateExclusive と衝突する。インデックスのリレーション自体にはロックを取らない。
- `index_cleanup = Off` のとき、`LP_DEAD` はそのまま残る（第 2・3 段を行わない）。インデックスのないテーブルでは `Off` を無視して 1 パスで `LP_UNUSED` にする（PG と同じ）。
- 第 1 段のあいだ別のトランザクションが動き続けるので、`vacuum_scan_page` は各ページで独立に正しい（ページごとの原子的な WAL）。

#### 5.3.2 第 1 段のページ処理（`vacuum_scan_page`）

```text
vacuum_scan_page(env, pool, wal, rel, blk, params):
  buf = pool.read_buffer(tag(rel, blk))?;  g = buf.write()?             // ブロッキングの排他ラッチ。ピン数は見ない（VC-D1）
  if g.page().is_new(): return { counts: 0, lpdead: [], free_bytes: 8164, last_lsn: None }   // 書かない。hio が使うときに初期化する
  scan = scan_page(env, g.page(), params)?
  lsn  = prune_and_log(pool, wal, rel, blk, &mut g, &scan.plan)?
  free = g.page().free_space();  drop(g);  drop(buf)                     // FSM を更新する前にラッチを外す（00 §5.3）
  VacuumPageResult { counts: scan.counts, lpdead: scan.lpdead_after, free_bytes: free, last_lsn: lsn }
```

- 排他ラッチの間に clog の読み込み（I/O）が起こりうる（可視性判定の通常の経路と同じ。リーフのロックだけを持っていれば I/O は許される）。
- 凍結と pruning は **1 つの WAL レコード**（`PRUNE_FREEZE`）。凍結だけのページ、pruning だけのページ、両方のページがある。

#### 5.3.3 第 2 段（インデックスから消す）

```text
index_and_heap_pass(env, rel, indexes, dead: &mut TidSet, stats, free_batch):
  w = WriteCtx { xid: INVALID, cid: 0 }
  for idx in indexes:                                         // 順序は pg_index の順
      s = index_store.bulk_delete(&w, idx, dead, &WaitCtx{ backend, ctl: env.wait })?       // 5.4
      stats.index_stats[idx]: tuples_removed は全周の合計、tuples_remaining は最後の周の値、pages は終了時の nblocks(index)
      （index_scans == 0 のインデックスは reltuples = new_live_tuples、relpages = nblocks(index) とする）
  stats.index_scans += 1
  // 第 3 段へ
```

`bulk_delete` は `dead` に含まれる TID を指す**すべての**項目を削除する。1 つのインデックスの途中で失敗・キャンセルしたら、`dead` を捨てて VACUUM を中断する（ヒープは `LP_DEAD` のまま。5.3.6）。

#### 5.3.4 第 3 段（`LP_DEAD` → `LP_UNUSED`）

```text
  for (blk, tids) in dead.groups():                           // ブロックの昇順
      env.interrupts.check()?
      (free, lsn) = vacuum_unused_page(pool, wal, rel.locator, blk, offsets(tids))?
      free_batch.push((blk, free));  stats.lpdead_items += tids.len();  stats.last_lsn = lsn
  dead.clear()

vacuum_unused_page(pool, wal, rel, blk, offsets):
  buf = read_buffer; g = buf.write()
  各 off が LP_DEAD であることを確かめる。違えば XX001 "dead item identifier {off} of block {blk} is not dead"
     （LP_DEAD は pruning か VACUUM だけが作り、再利用される経路は第 3 段だけなので、起こらない。起きたら破損）
  cs: 各 off を mark_unused → finish_unused（末尾の外し、PD_HAS_FREE_LINES）→ HEAP2_VACUUM_UNUSED（blk0 = ページ。ブロックデータ = offsets）→ set_lsn
  free = page.free_space()
```

- 第 3 段はタプルを動かさない（領域は第 1 段で回収済み）。ページの空き容量は、末尾の行ポインタが外れたぶん（4 バイトずつ）増えるだけ。FSM には改めて記録する。

#### 5.3.5 順序の正しさの証明（00 の A7）

M4 の B+Tree の読み手は「葉ごとに一致した TID をコピーし、ページのラッチもピンも持ち越さない」（M4 §13.2）。M2 までは行ポインタを再利用しなかったので、持ち越した TID は常に元のタプルか `LP_DEAD` を指した。M5 で `LP_UNUSED` の再利用を入れると、持ち越した TID が**別のタプル**を指しうる。次の 3 つの命題が成り立てば安全。

- **P0（pruning は可視性を変えない）**: `Dead` と判定されたタプルは、登録済みのどのスナップショット `S` でも不可視（V2 の根拠）。xmin が中断なら誰にも見えない。xmax がコミット済みで `x < horizon` なら、P3（下記）により `x < horizon <= S.xmin` なので、`S` から見て削除済み。
- **P1（項目が存在する間）**: インデックスの項目 E がタプル `t` のために作られ、葉にまだ存在するなら、その TID の行ポインタ `p` は `t` を指すか、`t` が除去された後の `LP_DEAD` であり、`LP_UNUSED` でも別のタプルでもない。
- **P2（走査が TID を持ち越す間）**: 走査 X が葉から TID `p` をコピーし、その後 E が消えて `p` が再利用されても、X が `p` に見つけるタプルは X のスナップショットで**不可視**（見つかるのが `LP_UNUSED` / `LP_DEAD` なら飛ばす）。

**前提**（他章が守ること。第 11 節にも挙げる）:

- **P3（horizon は単調で、登録済みスナップショットを止める）**: `oldest_xmin()` は実行中の XID、登録済みスナップショットの `xmin`、`next_xid` の最小（00 D11）。スナップショットの取得と登録は `proc` の Mutex の中で一度に行う（LK-3）ので、あとから取ったスナップショットの `xmin` は、それ以前に計算されたどの horizon 以上になる。したがって horizon は単調に増え、**登録済みのスナップショット `S` が生きている間、`horizon <= S.xmin`**。
- **P4**: 走査（SELECT、UPDATE / DELETE の対象の検索、EPQ の前の取得）が使うスナップショットは、走査の開始前に登録され、走査が終わるまで外れない（D11）。**カタログの走査も同じ**（VC-D23）。
- **P5**: 一意検査の `fetch_dirty` は、葉のラッチを持ったまま行う（M4 §13.1。RW-5）。
- **P6**: インデックスへの項目の挿入は、そのタプルの挿入トランザクションが実行中の間（INSERT / UPDATE の文の中）にだけ起こる。

**P1 の証明**。`LP_UNUSED` になる経路は 2 つしかない。(i) インデックスのないテーブルの VACUUM（`mark_unused_now`）: インデックスの項目が存在しない（CREATE INDEX は ShareUpdateExclusive と衝突し、VACUUM の間に現れない）。(ii) 第 3 段: `p` が `dead`（TID の集合 S）に入っていて、**S に対する第 2 段がすべてのインデックスで完了した後**にだけ `LP_UNUSED` にする（5.3.1 の順序）。第 2 段は S の TID を持つすべての項目を、葉の排他ラッチの下で、右リンクの順に全葉を巡って削除する。第 2 段の完了後に `p` を指す項目が `t` のために新しく作られることはない: `p` の `t` が `Dead` と判定されたのは、(a) xmin が中断（そのトランザクションは終了済みで、P6 により以後 `t` の項目は作られない）か、(b) xmax がコミット済みで horizon 未満（`t` の xmin はコミット済みで、`t` の挿入文は完了している）のどちらかで、どちらも `t` の項目はもう作られない。また `LP_DEAD` の `p` に別のタプルは置かれない（`add_item` は `LP_UNUSED` だけを使う）。したがって `LP_UNUSED` になった時点で `t` のための項目は存在せず、P1 が成り立つ。□

**P2 の証明**。X のスナップショット `S` は X の開始前に登録され（P4）、X の間は外れない。X が葉 `L` から `p` をコピーした時点を `c` とする。X が `p` を読むとき `p` が `LP_UNUSED` または `LP_DEAD` なら、X はそれを「ない」として飛ばす。`p` に別のタプル `t'`（挿入トランザクション `Y`）が入っているのは、E が削除され（第 2 段）、`p` が `LP_UNUSED` になり（第 3 段）、そのあとで `t'` が挿入された場合だけで、**第 2 段の削除は `c` より後でなければ X は E を得られない**ので、`t'` の挿入は `c` より後、したがって `S` の取得より後である。このとき `t'` は `S` で不可視:

- `Y` が `S` の取得後に開始した: `Y >= S.xmax`。
- `Y` が `S` の取得時に実行中だった: `Y` は `S.xip` にあり、`S` では未コミット。
- `Y` が X 自身（`Y == S.own_xid`）: `t'` の `cmin >= S.curcid`（`S` の取得後のコマンド。カーソルの途中で同じトランザクションが挿入した行も、`curcid` 以降なので不可視）。
- `Y` が `S` の取得前にコミット済み: `t'` は `S` の取得後に挿入されたので起こらない。

また、`p` が元のタプル `t` のままなら、X は普通に可視性を判定する。□

**一意検査**: P5 により、葉のラッチを持っている間は E が存在し、P1 から `p` は `t_E` か `LP_DEAD`。`LP_DEAD` なら衝突しない（dead）として扱う。

**`ctid` の連鎖をたどる者**（RW の `lock_tuple` の `follow_updates`、EPQ）: 連鎖の起点は自分のスナップショットで可視だったタプル `o`（`xmax = U`、`U` がコミットした後に追う）。新しい版 `n` が除去されるには `n` の xmax `Z` が `Z < horizon <= S.xmin <= U < Z`（`Z` は `U` の後）となる必要があり、矛盾する。したがって `n` は `LP_UNUSED` にならない。`o` 自身は除去・再利用されうるが、RW は「連鎖の先の `xmin` が直前の `xmax` と等しいこと」を確かめるので、再利用された行を取り違えない（RW の検査。未検証）。

**P3 の補足（登録しないスナップショット）**: 登録されていないスナップショットで走査すると P4 が成り立たず、同時に走る VACUUM が走査中の行を除去しうる。00 D12 の「カタログ用スナップショットは登録しない」は、**スキャンが残らない一瞬の用途に限る**（VC-D23。第 11 節）。ページ単位の走査（`HeapScan`）は 1 ページ分をコピーして返すので、ページ内の整合は保たれるが、ページをまたぐ走査の間は行が消えうる。

**クラッシュ**: 第 1 段の WAL（`PRUNE_FREEZE`）と第 2 段（`BTREE_VACUUM`）と第 3 段（`VACUUM_UNUSED`）はそれぞれ 1 ページ 1 レコードで原子的。順序は WAL の順序でもある（第 3 段の `VACUUM_UNUSED` が WAL にあるなら、その前の `BTREE_VACUUM` も WAL にあり、REDO で再生される）。クラッシュで第 2 段の途中で止まれば `LP_DEAD` が残るだけで、次の VACUUM が既存の `LP_DEAD` を含めて集め直し（5.2.1 の `lpdead_after`）、第 2 段はすでに消えた項目を見つけなくても成功する（冪等）。

#### 5.3.6 失敗・キャンセル・クラッシュの後の状態

| 止まった場所 | ヒープ | インデックス | 次の VACUUM |
|---|---|---|---|
| 第 1 段の途中 | 処理済みのページは pruning・凍結済み。`LP_DEAD` あり | 項目は残っている | 全ページをもう一度処理（処理済みのページは空の計画） |
| 第 2 段の途中 | `LP_DEAD` あり | 一部のインデックス・葉から項目が消えている | `LP_DEAD` を集め直して第 2 段をやり直す（消えた項目は見つからなくてよい） |
| 第 3 段の途中 | 一部のページが `LP_UNUSED`、残りは `LP_DEAD` | 第 2 段は完了 | 残りを同様に |
| 統計の更新の前 | すべて正しい | | `relfrozenxid8` が進んでいないだけ（次の VACUUM が進める） |

どの場合も、**V1・V2・V3 は破れない**（`relfrozenxid8` は第 1 段が全ページを処理し終えた後でしか進めない）。

### 5.4 B+Tree の項目の削除（`IndexStore::bulk_delete`。VC-3）

```text
bulk_delete(store, w, index, dead, wait) -> BulkDeleteStats:
  meta を読む（共有ラッチ）→ root。最も左の葉まで下りる（各ページ: 共有ラッチで子のブロック番号を取り、ラッチを外してから子を開く。
      最も左の子は分割で変わらない: 分割は右側に新しいページを作る）
  p = 最も左の葉
  while p != 0:
      buf = pin(p); g = buf.write()                       // 排他ラッチ（葉は 1 ページずつ。ほかのページのラッチを持たない）
      assert 葉（flags & LEAF）
      first_data = (右端でない) ? 2 : 1                    // 右端でない葉の 1 番は high key（M4 §13.5）。消さない
      victims = [off for off in first_data..=max_offset if dead.contains(heap_tid(item(off)))]
      tuples_remaining += (max_offset - first_data + 1) - victims.len();  tuples_removed += victims.len()
      if !victims.is_empty():
          cs: delete_items(page, &victims)                // 行ポインタとタプルを削除して詰める
              rec = RecordBuilder::new(Btree, BTREE_VACUUM, Xid::INVALID); register_block(.., STANDARD)
              rec.block_data(b, victims as u16 LE); rec.main_data(8 バイト); wal.insert → set_lsn
      next = special.next;  pages_scanned += 1
      drop(g); drop(buf)                                   // ラッチを外してから次のページへ（ラッチは持ち越さない）
      wait.ctl.interrupts.check()?
      p = next
  stats
```

- **右リンクの順に最も左の葉から右へ**巡る。分割は右側にページを作るだけなので（M4 は未完了の分割を作らない。ページの併合・削除は M6）、**項目は常に右へしか動かない**。したがって、巡回の途中でページが分割されても、動いた項目は後で訪ねるページに入り、見落とさない（すでに訪ねたページから動いた項目は 2 回目の訪問で見る。冪等）。
- 右リンクを読んでからラッチを外すので、次のページが分割されていても、読んだ時点の右リンクの先（分割後の左半分）から続ければ右半分に到達する。
- **1 ページ 1 レコード**。`delete_items` と `BTREE_VACUUM` の REDO は同じ関数を使う。
- **空になった葉は残す**（VC-D11）。葉のすべての項目が消えてもページは削除しない。スキャンと挿入は空の葉（データの行ポインタが 0 個）を正しく扱わなければならない（M4 の B1・B2 の持ち主 = RW-5 が引き継ぐ。VC のテスト 7.2 で確かめる）。
- 内部ページには触れない（内部ページの項目は子のブロック番号で、ヒープの TID ではない）。葉のデータの項目（ピボットでないもの）だけを見る。
- `BulkDeleteStats.tuples_remaining` は、インデックスの `pg_class.reltuples`（5.8）に使う。
- 戻り値のあとの `pg_class` の更新は VACUUM 本体が行う（5.3.1 の E）。

### 5.5 凍結境界・`datfrozenxid8`・clog の切り詰め（VC-4）

#### 5.5.1 `relfrozenxid8` の進め方

VACUUM は 5.3.1 の手順で、**全ページを処理し終えた後**に、`relfrozenxid8 = max(old, cutoff)` を書く（E）。`cutoff = horizon`（開始時）。次が成り立つので R1・R2（3.4）が保たれる。

- **R1**: 全ページを処理した。各ページで、`xmin < cutoff` のコミット済みのタプルは凍結され、`Dead` は除去された。その後に挿入されたタプルの xmin は、挿入時の `next_xid` 以上（`next_xid >= horizon` を最初に確かめた時点より後）。実行中だった XID は `horizon` 以上なので、`xmin < cutoff` のタプルをそれらが作ることはない。
- **R2**: `xmax < cutoff` で有効な xmax は、`Updater` なら `Committed`（`Dead` として除去）か `Aborted`（無効化）。`LockOnly` は無効化。実行中の更新者・ロックは `horizon` 以上。
- 並行する DML が走査の後ろ側のページで `xmax` を書くことがあるが、その XID は実行中だったので `horizon` 以上で、R2 に影響しない。

書き込みは**その場の上書き**（`set_relstats_inplace`）。`HEAP2 INPLACE` の WAL は、**`wal.flush` で永続化してから** clog の切り詰め（5.5.3）に進む。

#### 5.5.2 `datfrozenxid8` の更新（`vac_update_datfrozenxid`）

`update_datfrozenxid_and_truncate_clog` の前半。VACUUM の文の最後に 1 回、autovacuum は 1 データベースの処理の最後に 1 回呼ぶ（`VACUUM (SKIP_DATABASE_STATS)` は呼ばない）。

```text
update_datfrozenxid_and_truncate_clog(env):
  snap = take_snapshot()                                       // この関数の中だけ
  mn = catalog.min_relxid(&snap)?                              // pg_class（relkind 'r'）と yz_relxid の突き合わせ。行がないテーブルは Xid(3) とみなして missing に入れる
  new = min(mn.min, txn.oldest_xmin(), txn.next_xid())         // 実行中の XID・スナップショットを超えて進めない（保険）
  cur = shared.datxid(&snap, db_oid)?.unwrap_or(Xid(3))
  advanced = new > cur
  if advanced: shared.set_datxid_inplace(&snap, db_oid, new)?  // yz_datxid の行と pg_database.datfrozenxid の下位 32 ビット（その場の上書き）
  return 5.5.3 の結果       // datfrozenxid8 が進まなくても、ほかのデータベースが進んでいれば floor は進みうるので、毎回評価する（進まなければ何もしない）
```

- `datfrozenxid8` は**進むだけ**（`new <= cur` なら書かない）。
- `mn.missing` が空でなければ WARNING（ログと通知）を出す。

#### 5.5.3 clog の切り詰め（`vac_truncate_clog`）

```text
  mind = shared.min_datxid(&snap)?                             // 全データベースの datfrozenxid8 の最小（template0 の番兵は大きい値）
  floor = min(mind, txn.oldest_xmin())                         // 00 D51。登録済みスナップショットと実行中の XID を超えない
  floor = max(floor, control.oldest_xid)                       // 単調
  if floor <= control.oldest_xid: return（何もしない）
  1. wal.flush(wal.insert_lsn())?                              // 凍結・INPLACE のレコードを永続化（5.5.1）
  2. control.update(|c| c.oldest_xid = max(c.oldest_xid, floor))?   // fsync。失敗は Panic
  3. clog.truncate_before(floor)?                              // 6.5
  4. segments_removed を返す。ログ "pg_xact truncated before {floor}"
```

**なぜ安全か**: clog を読み書きする者は、(a) タプルの xmin / xmax の状態を引く者（可視性判定・`satisfies_update`・`satisfies_vacuum`）と、(b) 自分の XID をコミット・アボートする実行中のトランザクションと REDO の 2 種類だけ。(b) の XID は実行中なので `floor <= oldest_xmin()` により `floor` 以上（REDO は 5.5.4 で無視する）。(a) は、どのデータベースのどのテーブルにも、`floor` 未満の XID を持つ未凍結の xmin や有効な xmax が残っていない（`floor <= datfrozenxid8 <= 各テーブルの relfrozenxid8`、R1・R2）ので、`floor` 未満の XID を引く機会がない。**どこかが破れて引いてしまったら**、`Clog::status` が `xid < oldest` を `XX001` にするので、黙って「実行中」と読むことはない（V3 の検出）。

**順序の理由**: 1 → 2 → 3。2 より前に消すと、クラッシュで凍結の WAL が失われたときに clog も無い。2 の後にクラッシュしても、起動時にもう一度 3 と同じ掃除をする（冪等）。`oldest_xid` を先に上げて `Clog.oldest`（メモリ）を更新してからファイルを消すのは、消している途中に別のスレッドが読んでも `XX001` で止まるようにするため。

#### 5.5.4 起動と REDO

- 起動（00 §5.4 に足す）: `Clog::open(vfs, next_xid, control.oldest_xid)`。直後に `sweep_old_segments()`（`pg_xact/` を列挙し、3.6 の条件に当たるセグメントを消す。冪等）。
- REDO: `set_status_redo(xid, ..)` は `xid < oldest` なら何もしない（`Ok(())`）。`oldest_xid` の前進は WAL に載らないので、REDO の途中で制御ファイルの値は変わらない（リカバリ開始時の値で固定）。凍結・pruning・INPLACE の REDO は clog を引かない。
- チェックポイントの `clog.flush()`（M3 §5.6）と `truncate_before` は、`Clog` の `flush_lock` で直列化する（切り詰めたセグメントを古い内容で書き直さないため。6.5）。

#### 5.5.5 全データベースの一巡の条件

`oldest_xid` が進むのは、**接続できる全データベースの全ヒープのテーブルが、少なくとも 1 回 VACUUM された後**（`datfrozenxid8` が進むため）。`template1` と `postgres` を含む。template0 は番兵（VC-D13）。新しく作ったデータベースは、コピー元の値から始まる（09 章）。自動で一巡させるのは autovacuum（5.9）。手動なら各データベースで `VACUUM`（引数なし）を実行する（M5-VC-Q4）。

### 5.6 TRUNCATE と末尾の切り詰め（VC-5）

#### 5.6.1 `TRUNCATE`（00 D42。VC-D17）

文そのもの（新しい relfilenode を作る方式）は M4。`ddl/truncate.rs` に足すのは次の 4 点。

1. **AccessExclusive**: LK-4 が文ごとに取る（00 §3.4）。`ddl/truncate.rs` は先頭で `debug_assert!(locks.holds(backend, tag, AccessExclusive))`。待つ相手の VACUUM はその間完了する（ShareUpdateExclusive と衝突）。autovacuum が相手のときは `has_waiters` で中断する（5.9）。
2. **FOREIGN KEY で参照されている表の拒否**: 対象の集合 `T`（文に並べたテーブル）を渡して **`check_truncate_fks(ctx, &T, cascade)`（07 §4.6）を呼ぶ**（AccessExclusive を取った後。1 の直後）。`Err` はそのまま返す。判定は 07 が持つ: 参照元が `T` にすべて含まれていれば通す（PG と同じ。FK-D18）。
   - 含まれない参照元があれば `0A000` `cannot truncate a table referenced in a foreign key constraint`、DETAIL `Table "c" references "p".`、HINT `Truncate table "c" at the same time, or use TRUNCATE ... CASCADE.`（PG と同じ。HINT の `CASCADE` は yuzhu では 0A000 になる）。自己参照（参照元 = 参照先）は許す。参照元が `T` に含まれない `TRUNCATE ... CASCADE` は `0A000` `TRUNCATE ... CASCADE is not supported`（07 §6.7）。
3. **`yz_relxid` の行**: 各テーブルについて `update_relxid(w, snap, relid, w.xid)`（トランザクションの中の通常の UPDATE。ロールバックで戻る）。`pg_class.relpages = 0`、`reltuples = -1`、`relfrozenxid` の鏡の更新は M4 の `truncate.rs` が行う `pg_class` の UPDATE に含める（M4 の関数に `relfrozenxid` の引数を足す。未検証: M4 07 章の実装）。**FSM**: 新しい relfilenode には FSM のファイルがない（最初の `record` で作る）。古いファイルはコミットで `unlink` される（全フォーク。M2 D13）。`HioCtx::forget(old_locator)` を `unlink_storage` から呼ぶ。
4. **VACUUM との排他**: ロックのモードの衝突で済む。追加の仕組みはない。

#### 5.6.2 末尾の切り詰め（VC-D18。VACUUM の D）

```text
try_truncate_tail(env, rel, nblocks_now):
  nonempty = nblocks_now の手前から後ろ向きに、行ポインタが 1 つもない（max_offset == 0）か is_new のページが続く限りを数えて、
             最後に中身のあるページの次のブロック番号（= 切り詰め後のブロック数）を求める   ※共有ラッチ。5.6.3
  possibly_freeable = nblocks_now - nonempty
  if possibly_freeable == 0 or !(possibly_freeable >= REL_TRUNCATE_MINIMUM || possibly_freeable >= nblocks_now / REL_TRUNCATE_FRACTION): return
  // AccessExclusive を条件付きで取る（待たない）。取れるまで 50ms 間隔で最大 vacuum_truncate_lock_timeout（既定 5 秒）
  deadline = now + timeout
  loop:
     if try_acquire(backend, tag, AccessExclusive, env.lock_scope): break          // 自分が持つ ShareUpdateExclusive からの格上げ。待たない
     if now >= deadline: INFO（verbose）`"t": stopping truncate due to conflicting lock request`; return
     env.interrupts.check()?;  sleep(50ms)
  // 取れた。ここから先、ほかのトランザクションはこのリレーションに入れない
  nblocks_locked = pool.nblocks(...)                                          // 取るまでに増えたかもしれない
  nonempty = count_nondeletable_pages(env, rel, nblocks_locked, deadline)?      // 5.6.3。再計算（ロックの下）
  if nonempty >= nblocks_locked: release(backend, tag, AccessExclusive); return          // 空きがなくなっていた
  _b = txn.exclusive_barrier()?                                               // チェックポイントの書き出しと排他（00 D10）
  smgr_wal::log_and_truncate(wal, pool, smgr, Xid::INVALID, rel, Main, nonempty)?     // WAL を flush してから、バッファを捨てて切り詰める（M3 §6.5.2）
  fsm.truncate(rel, nonempty)?                                                // WAL なし
  hio.forget(rel)                                                             // 挿入先のヒント
  stats.truncated_to = Some(nonempty); rel_pages_end = nonempty
  release(backend, tag, AccessExclusive)                                      // 切り詰めた直後に解放する（PG の lazy_truncate_heap と同じ。ShareUpdateExclusive は残る）
```

**5.6.3 `count_nondeletable_pages`**: 後ろ向きに、ブロックを 1 つずつ共有ラッチで見る。`is_new` か `max_offset == 0`（すべて `LP_UNUSED` で末尾が外れている）なら「空」、それ以外（`LP_NORMAL` または `LP_DEAD` が 1 つでもある）は「中身あり」で止まる。32 ブロックごとに `env.interrupts.check()` を行い、`env.is_autovacuum` なら `has_waiters` も見て、待ち手がいれば中断（切り詰めを諦める）。切り詰める範囲に `LP_DEAD` があるページは「中身あり」なので、インデックスの項目が切り詰めたブロックを指すことはない（第 3 段の後、そのページは空）。

- **インデックスの項目と切り詰め**: 第 3 段で `LP_UNUSED` になったページだけが空になる。その TID を指す項目はすでに消えている（P1）。AccessExclusive の下なので、切り詰めの間に挿入・走査は入らない。
- **WAL**: `SMGR_TRUNCATE`（M3 §3.8）。REDO は M3 のとおり（`pool.drop_relation_buffers_from` → `smgr.truncate` → `invalid.forget_from`）。切り詰めの前のブロックへの `PRUNE_FREEZE` / `VACUUM_UNUSED` の REDO がファイルの短い状態で `NotFound` を記録しても、後ろの `SMGR_TRUNCATE` で消える（M3 D14）。
- **FSM のファイル**: `fsm.truncate` は WAL を書かない（導出データ）。切り詰めたブロックを指す FSM のエントリが残っても、`hio` が `blk >= nblocks` を捨てる（5.7）。

---

### 5.7 FSM と挿入先の選択（VC-2）

#### 5.7.1 `FreeSpaceMap`

3.3 の形式。`pool.read_buffer_zero_on_error`（6.3）でページを読む。**FSM の失敗は呼び出し側の文を失敗させない**（00 規約 4）: `Severity::Panic` 以外のエラーは WARNING をログに出して「FSM なし」として扱う。

```text
search(rel, needed):
  cat = needed_to_cat(needed)
  repeat 最大 3 回:
    root = read(FSM ブロック 0)。ブロックがない／kind が Root でない → return None
    g = root.read(); i = data を next_slot から循環して、data[i] >= cat の最初の位置。なければ return None; drop(g)
    leaf = read(FSM ブロック 1 + i)。なければ root.data[i] = 0 に直して continue
    g = leaf.read(); j = data を next_slot から循環して、data[j] >= cat の最初の位置
    見つかった: blk = i * 8160 + j; drop(g); try_write でヒントを更新（leaf.next_slot = j + 1、root.next_slot = i。取れなければ省く）; return Some(blk)
    見つからない（根が古い）: m = max(leaf.data); drop(g); 根を write ラッチして root.data[i] = m（hint の書き込み）; continue
  None

record(rel, blk, free):                                          // free = Page::free_space() の値
  if blk >= FSM_MAX_BLOCKS: return
  cat = avail_to_cat(free);  L = blk / 8160;  j = blk % 8160
  FSM のファイルと、ブロック 0..=1+L を用意する（なければ smgr.create(Fsm)、pool.extend を必要な数だけ。競合して多めに伸びてもよい）
  leaf = read(1 + L); g = leaf.write(); kind が None なら init_fsm(Leaf)
  if data[j] == cat: return                                      // 変化なし
  data[j] = cat（page_mut_hint）; m = max(data); drop(g)         // 葉のラッチを外してから根へ（同時に持たない）
  root = read(0); g = root.write(); kind が None なら init_fsm(Root); root.data[L] = m（page_mut_hint）

truncate(rel, nblocks):                                          // 呼び出し側が exclusive_barrier を持つ。FSM のファイルがなければ何もしない
  L0 = nblocks / 8160; r = nblocks % 8160
  r != 0 なら葉 L0 の data[r..] を 0 にし、最大を根の data[L0] に。葉 > L0（r == 0 なら >= L0）はすべて根の data を 0 に
  pool.drop_relation_buffers_from(rel, Fsm, 1 + ceil(nblocks / 8160)); smgr.truncate(rel, Fsm, 1 + ceil(nblocks / 8160))   // WAL なし
```

- **整合性は「根 ≥ 葉の最大」だけ**を目標にする（根が古く大きいのは許す。小さいと空きを見逃すだけ）。`record` は葉と根を別々のラッチで更新するので、並行する `record` で根が一時的に古くなりうるが、`search` が直し、VACUUM が全ブロックを記録し直す。
- **ラッチの順序**（00 §5.3 の 4）: FSM のページのラッチは、**ヒープのページのラッチを持たずに**取る。したがって `hio` は、ヒープのページのラッチ（とピン）を外してから `note_no_fit` / `search` を呼ぶ。VACUUM は `vacuum_scan_page` が戻った後（ヒープのラッチなし）に `record_batch` を呼ぶ。FSM の根と葉を同時に持つこともない。
- FSM のページの変更は `page_mut_hint` で行う（WAL なし。M3 の `set_lsn` なしの検査の対象外）。クラッシュで torn になっても、チェックサム不一致が zero-on-error で 0 のページになるだけ（V4）。

#### 5.7.2 `hio` の流れ

`find_target`（ピンだけを返し、ラッチは取らない。M3 §6.7 の UPDATE の手順 4 と同じ口）:

```text
find_target(rel, tuple_len, tried):
  need = MAXALIGN(tuple_len);  nblocks = pool.nblocks(rel, Main)
  if tried.len() >= 8: return pool.extend(rel, Main)                       // 何度も入らなかったら拡張する
  1. ヒント（直近に使ったページ）: Some(b)、b < nblocks、b ∉ tried → pin して返す
  2. FSM: 最大 8 回 { b = fsm.search(rel, need)（失敗は None）; None → break;
                      b >= nblocks → fsm.record(rel, b, 0); continue;（切り詰めや異常の残りを捨てる）
                      b ∈ tried → continue; それ以外 → pin して返す }
  3. 最後のページ: nblocks > 0 かつ nblocks - 1 ∉ tried → pin して返す（PG の「FSM に何もなければ最後のページを試す」。FSM のない表で新しいページを作り続けないため）
  4. pool.extend(rel, Main)
```

挿入（M3 §5.1 の `insert` の手順 2・3 をこれに置き換える。D / RW の持ち物の実装が従う）:

```text
tried = []
loop:
  pin = hio.find_target(rel, len, &tried)?;  blk = pin.tag().block;  g = pin.write()?
  if fits(g.page(), len): break                                           // is_new は入る
  if hio.try_prune_for_space(rel, blk, &mut g)? && fits(g.page(), len): break     // 同じ排他ラッチで pruning して再判定（VC-D7）
  free = g.page().free_space(); drop(g); drop(pin)                        // FSM の前にヒープのラッチとピンを外す
  hio.note_no_fit(rel, blk, free); tried.push(blk)
（入った）CriticalSection で add_item → WAL INSERT → set_lsn（M3 §5.1）。ガードを外してから hio.note_used(rel, blk)
```

- `fits` は `Page::free_space() >= MAXALIGN(len)`（M2 の `fits` をそのまま使えるように `free_space` の意味を 4.1 のとおりに変えた）。
- UPDATE で新版を別のページに置くとき（M3 §5.1 の 4〜7）は、`tried = [旧ページ]` で `find_target` し、2 つのページを番号順にラッチした後で「入らない」と分かったら両方のラッチを外して `note_no_fit` し、`tried` に足して `find_target` からやり直す。
- **FSM がなくても正しく動く**: `search` が `None` を返し続けても、ヒント → 最後のページ → 拡張で挿入できる（V4）。

#### 5.7.3 VACUUM での再構築

- 5.3.1 の B・C で、走査した**すべてのブロック**の空き（pruning 後の `Page::free_space`）を `record_batch` で書く（変化の有無に関わらず）。これで、FSM が壊れていた・古かった・全 0 だった場合も、VACUUM の後は実際の空きを表す。
- 第 3 段で変わった空き（行ポインタの末尾の外れ）も、`vacuum_unused_page` の戻り値で記録し直す。
- 末尾の切り詰めのあと `fsm.truncate`（5.6.2）。

### 5.8 統計の更新（その場の上書き）・ANALYZE・VERBOSE

#### 5.8.1 `TableStore::inplace_update`（`storage/heap/inplace.rs`）

```text
inplace_update(heap, rel, tid, expect_xmin, patches):
  tid.block >= nblocks なら TupleGone
  buf = read_buffer; g = buf.write()
  行ポインタが LP_NORMAL でなければ TupleGone（範囲外を含む）。hdr = TupleHeader::read
  hdr.xmin != expect_xmin、または xmax が有効でロックだけでない → TupleGone
  各パッチ: (byte_off, len) = fixed_column_range(&rel.desc, item_bytes, attnum)?     // 列の整列から計算。列が NULL・可変長・値の型が合わないなら内部エラー
            value を LE のバイト列にする（Int4 / Oid / Xid(u32) は 4 バイト、Float4 は to_bits、Int8 は 8 バイト）
  cs: item_mut にバイトを書く → HEAP2_INPLACE（blk0 = ページ STANDARD、メイン = 3.5 の形式）→ set_lsn
  Updated
```

- `fixed_column_range` は `deform_tuple` と同じ走査で、先行する列の位置（整列と可変長のヘッダ）を数えて、対象列の開始位置と長さを返す。**ヘッダ（`t_xmin` の位置）からのバイト数**で、`HeapTupleHeader` の 35 バイトを含む。
- MVCC は関係しない（新しい版を作らない）。他のトランザクションが同じ行を読んでいても、固定長の値の上書きなので、読み手は共有ラッチの下で古い値か新しい値のどちらかを見る。

#### 5.8.2 `set_relstats_inplace`

```text
set_relstats_inplace(snap, relid, relpages, reltuples, frozen):
  (tid, xmin) = pg_class を snap で走査して oid = relid の行を探す。なければ（テーブルが消えた）Ok(())
  patches = [relpages: Int4, reltuples: Float4, relallvisible: Int4(0)] + (frozen なら relfrozenxid: Xid(下位 32 ビット))
  inplace_update(pg_class, tid, xmin, &patches) → TupleGone なら WARNING をログに出して Ok(())（統計は助言。凍結境界は進まないだけで安全）
  frozen が Some(f) なら yz_relxid を走査して relid の行を探し、inplace_update(yz_relxid, tid, xmin, [relfrozenxid8: Int8(f)])
  （yz_relxid に行がなければ書かない。WARNING）
```

インデックスの `pg_class` の行（`relpages`、`reltuples`）も同じ関数（`frozen = None`）で更新する。

#### 5.8.3 ANALYZE（`analyze_relation`。VC-D16）

```text
analyze_relation(env, relid, columns, verbose, skip_locked):
  ShareUpdateExclusive を取る（5.3.1 の A と同じ。外側のトランザクションの中なら、そのトランザクションの終わりまで持つ）
  relkind != 'r' → WARNING `skipping "t" --- cannot analyze non-tables or special system tables`
  horizon = txn.oldest_xmin()
  for blk in 0..nblocks: 共有ラッチ → scan_page(env, page, PruneParams { horizon, freeze_cutoff: None, mark_unused_now: false, own_xid: env.own_xid })
        live += counts.live;  dead += counts.made_dead + counts.recently_dead          // 計画は使わない（ページを変えない）
  set_relstats_inplace(snap, relid, nblocks, live as f32, None)
  verbose なら INFO `analyzing "schema.rel"` と
       `"t": scanned N of N pages, containing L live rows and D dead rows; L rows in sample, L estimated total rows`
```

- `pg_statistic` は作らない。列リストは**存在の検査だけ**（アナライザが 42703 にする）。`VACUUM ANALYZE` は VACUUM の統計（`new_live_tuples` と `rel_pages_end`）をそのまま使い、2 回目の走査はしない（定義が同じ。VERBOSE の `analyzing` の行は VACUUM の結果から作る）。
- 外側のトランザクションで動くので、**そのトランザクションが挿入した行は live に数える**（`own_xid`）。`ROLLBACK` しても統計は戻らない（その場の上書き。PG と同じ）。

#### 5.8.4 VERBOSE の出力（INFO。VC-D24）

1 テーブルにつき、開始時に `INFO:  vacuuming "db.schema.rel"`、切り詰めたら `INFO:  table "rel": truncated N to M pages`、最後に 1 つのメッセージ（改行を含む）。例（PG:src/backend/access/heap/vacuumlazy.c の `heap_vacuum_rel` の出力に倣う。【記憶】。行の並びと文言の細部は共有テストで比較しない）:

```text
INFO:  vacuuming "postgres.public.t"
INFO:  finished vacuuming "postgres.public.t": index scans: 1
pages: 0 removed, 1 remain, 1 scanned (100.00% of total)
tuples: 50 removed, 50 remain, 0 are dead but not yet removable
removable cutoff: 748, which was 0 XIDs old when operation ended
new relfrozenxid: 748, which is 745 XIDs ahead of previous value
frozen: 1 pages from table (100.00% of total) had 50 tuples frozen
index scan needed: 1 pages from table (100.00% of total) had 50 dead item identifiers removed
index "t_pkey": pages: 2 in total, 0 newly deleted, 0 currently deleted, 0 reusable
system usage: CPU: user: 0.00 s, system: 0.00 s, elapsed: 0.00 s
```

| 行 | 値の出どころ |
|---|---|
| `pages:` | removed = `rel_pages_start - rel_pages_end`、remain = `rel_pages_end`、scanned = `pages_scanned`、% は `scanned / rel_pages_start` |
| `tuples:` | `tuples_deleted`、`new_live_tuples`、`recently_dead_tuples` |
| `removable cutoff:` | `removable_cutoff`（**64 ビットの値**を出す。PG は 32 ビット）、「N XIDs old」は `next_xid - cutoff` |
| `new relfrozenxid:` | `new_relfrozenxid > old_relfrozenxid` のときだけ |
| `frozen:` | `frozen_pages`、`frozen_tuples` |
| `index scan needed:` / `index scan not needed:` | `lpdead_items > 0` かインデックスの周が 1 回以上あれば needed。`pages_with_lpdead` と `lpdead_items` |
| `index "x":` | `IndexVacStats.pages`（`newly deleted` / `currently deleted` / `reusable` は 0。ページの削除は M6） |

`VACUUM (VERBOSE)` でなければ何も出さない。WARNING（テーブルの読み飛ばしなど）は VERBOSE に関係なく出す。

### 5.9 簡易 autovacuum（VC-6。既定は無効。00 D17、VC-D20）

#### 5.9.1 構成

```rust
// vacuum/stats.rs
#[derive(Clone, Debug)]
pub struct AutovacuumConfig {
    pub enabled: bool,               // 既定 false（00 D17）
    pub naptime: Duration,           // 60 秒
    pub vacuum_threshold: u64,       // 50
    pub vacuum_scale_factor: f64,    // 0.2
    pub freeze_max_age: u64,         // 200_000_000（XID の数。clog の切り詰めを進めるための条件。PG の anti-wraparound の再解釈）
}
#[derive(Debug, Default)]
pub struct ModStats { /* Mutex<HashMap<(Oid /* db */, u32 /* relfilenode */), u64>> */ }
impl ModStats {
    pub fn absorb(&self, drained: Vec<(RelFileLocator, u64)>);        // 文の終わりに Session が modstat::drain() の結果を渡す（1 行。S の範囲。第 11 節）
    pub fn dead_since_vacuum(&self, db: Oid, relfilenode: u32) -> u64;
    pub fn reset(&self, db: Oid, relfilenode: u32);
}
/// 対象選択（純関数。単体テストの対象）
pub fn needs_vacuum(cfg: &AutovacuumConfig, info: &VacRelInfo, n_dead: u64, relfrozenxid8: Xid, next_xid: Xid) -> bool;
//   n_dead > vacuum_threshold + vacuum_scale_factor * max(reltuples, 0)   または   next_xid - relfrozenxid8 > freeze_max_age
```

```rust
// vacuum/autovacuum.rs
pub struct AutovacuumHandle { /* stop: Arc<AtomicBool>, join: JoinHandle<()>, interrupts: Arc<InterruptFlag> */ }
pub fn spawn(cluster: Weak<Cluster>, cfg: AutovacuumConfig) -> AutovacuumHandle;     // Cluster::open がリカバリと起動時掃除の後に呼ぶ
impl AutovacuumHandle { pub fn stop(self); }                                          // stop を立て、実行中の VACUUM を interrupts で止め、join する
```

#### 5.9.2 動き

```text
launcher（1 本のスレッド。Weak<Cluster> を upgrade できなければ終わる）:
  loop { naptime だけ眠る（1 秒ごとに stop を確かめる）; 起きたら
         for db in pg_database のうち datallowconn = true の行（template0 を除く）: worker_pass(db) }

worker_pass(db_oid):
  handle = cluster.database_handle(db_oid)?
  backend = BackendId(cluster.next_session_id()) を BackendRegistry と LockManager に登録（pid = -1 - id。user = ブートストラップユーザー。終わりに外す）
  snap = take_snapshot(); rels = catalog.list_vacuum_relations(&snap); relxids = 各 relxid; drop(snap)
  for r in rels:
      stop なら return
      if !needs_vacuum(cfg, &r, mod_stats.dead_since_vacuum(db_oid, r.locator.rel_number), relxid(r), next_xid): continue
      env = VacuumEnv { lock_scope: Session, is_autovacuum: true, own_xid: None, .. }
      opts = VacuumOptions { skip_locked: true, truncate: true, .. }
      match vacuum_relation(&mut env, r.oid, &opts):
          Ok(Done(s)) => { wal.flush(s.last_lsn); mod_stats.reset(db_oid, r.locator.rel_number); ログ "automatic vacuum of table \"db.schema.rel\": …" }
          Ok(Skipped(_)) => 次へ
          Err(e) if e は 57014（stop による）=> return;  Err(e) => ログ（WARNING）して次へ。Severity::Panic なら cluster.poison()
      release(backend, tag, ShareUpdateExclusive)（Session スコープなのでテーブルごとに明示的に外す）
  update_datfrozenxid_and_truncate_clog(&mut env)
```

- **カウンタ**: `HeapStore::delete` / `update` が成功するたびに `modstat::note_modified(locator, 1)`（スレッドローカル。共有ロックを取らない）。`Session` が文の終わりに `cluster.mod_stats().absorb(modstat::drain())` を呼ぶ。件数は UPDATE と DELETE の行数で、ロールバックされた分も数える（過大評価でよい）。**再起動で消える**（M5-VC-Q7）。
- **中断**: `vacuum_relation` は第 1 段の 64 ブロックごとと末尾の切り詰めの待ちの間、`is_autovacuum` なら `locks.has_waiters(backend, tag)` を見て、**ほかの文がこのテーブルのロックを待っていたら** `Skipped(CancelledByWaiter)` で止める（PG が autovacuum をキャンセルするのと同じ目的。DDL や TRUNCATE が長く待たされないように）。途中までの変更は有効（`LP_DEAD` は次の VACUUM が拾う）。止めたテーブルのカウンタは `reset` しない。
- 手動の VACUUM とは `ShareUpdateExclusive` が衝突するので並行しない。待たずに飛ばす（`skip_locked`）。
- autovacuum は `maintenance_work_mem` を使う。コストベースの遅延はしない（I/O を使い切る。M6）。
- 停止: `Cluster::shutdown` は、チェックポインタを止める前に `AutovacuumHandle::stop` を呼ぶ。`stop` は `interrupts` に停止を立てて実行中の VACUUM を 57014 で終わらせ、join する。
- `autovacuum` 設定が有効でも、`ClusterOptions.autovacuum.enabled` が偽（テスト）なら起動しない。

### 5.10 共有カタログ・template データベース・起動時の残骸掃除

#### 5.10.1 共有カタログ（`pg_database`・`pg_authid`・`pg_tablespace`・`yz_datxid`）

- **VACUUM（引数なし）は共有カタログも対象にする**（`list_vacuum_relations` が `relisshared` の行を含める。PG と同じ）。ロックの `LockTag::Relation` は `db = 0`（共有リレーションはデータベースをまたいで衝突する。第 11 節で LK に依頼）。ヒープの `RelFileLocator` は `db_oid = 0`（M2 §4.3）なので、バッファ・FSM の置き場所は同じ 1 つ。
- 共有カタログの `yz_relxid` の行は**各データベースに 1 行ずつ**ある（各データベースの `pg_class` が共有カタログの行を持つのと同じ）。どのデータベースから VACUUM しても、そのデータベースの行を進める。`datfrozenxid8` の計算は各データベースの `yz_relxid` の最小なので、共有カタログの凍結境界も「全データベースで一巡する」条件に含まれる。
- horizon はクラスタ全体（VC-D4）なので、共有カタログに特別な計算は要らない。

#### 5.10.2 template データベース

| データベース | 接続 | 凍結境界 | 扱い |
|---|---|---|---|
| `template1` | 可（`datallowconn = t`） | 通常 | 通常のデータベースと同じ。VACUUM しないと clog が切り詰められない（5.5.5） |
| `postgres` | 可 | 通常 | 同上 |
| `template0` | 不可 | **番兵 `DATFROZEN_PRISTINE`** | VACUUM できない。initdb 以後だれも書けないので clog を引く XID がない（VC-D13）。autovacuum も対象にしない |
| `CREATE DATABASE` の新しいデータベース | 通常は可 | `min(コピー元の値, oldest_xmin())`（09 章）。各テーブルの `yz_relxid` はコピー元のまま | 通常のデータベースと同じ |

`datallowconn = false` で作ったデータベース（`ALLOW_CONNECTIONS false`）の境界は、コピー元の値のまま動かない（VACUUM できない）ので、clog の切り詰めを止める（既知の制約。M5-VC-Q4）。

#### 5.10.3 起動時の残骸掃除（M2-Q24。VC-D19。`vacuum/orphan.rs`）

リカバリと終了チェックポイントの後、接続の受け付けとバックグラウンドのスレッドの起動の前に、`Cluster::open` が呼ぶ。この時点で実行中のトランザクションはなく、コミット済みの `pg_class` が正しい。

```text
sweep_orphans(cluster):
  snap = txn.snapshot(None, 0)                                   // 実行中がないので、コミット済みの行がすべて見える
  for db in pg_database の全行（datname に関わらず）:
      dir = base/<db.oid>/;  存在しなければ飛ばす（孤児ディレクトリは 00 D31 の WARNING。DB の持ち物）
      live = そのデータベースの pg_class の全行について、relfilenode != 0 ならその値、0（マップ）なら oid の集合
      if live.len() < カタログの数（13 + M4 以降の分）: WARNING "pg_class of database X looks incomplete; skipping orphan sweep" で飛ばす
      for 各エントリ（名前が 10 進数だけのもの。"_fsm"・".N" 付きは対象外）:
          n = 数値;  if n ∈ live: 次へ
          size == 0 なら remove_file(dir/n)（あれば dir/n_fsm も）し、ログ "removed leftover empty file base/<db>/<n>"
          size > 0 なら count += 1
      count > 0 なら WARNING "found {count} orphan relation files with data in base/{db}/ (not removed)"
  global/: 共有カタログの live は template1 の pg_class の relisshared の行から。同様
  sync_dir（消したディレクトリ）
```

- 消すのは**大きさが 0 バイトで、主フォークで、どの `pg_class` の行にも当たらないファイルだけ**。0 バイトでないものは消さない（M3 D15 と 00 D31 の方針。コミット済みの DROP のあと、ファイルの削除の前にクラッシュしたものなど。M3-Q5）。
- クラッシュで中断した `CREATE TABLE` のファイル（SMGR_CREATE の REDO が作り直した 0 バイトのもの）と、M2 D13 の「消せなかった残骸」がこれで消える。
- 失敗（`read_dir`・`remove_file` のエラー）は WARNING で続行する。起動は止めない。

### 5.11 ロックの順序（00 §5.3 に足す）

| 順 | 取るもの | VC の規則 |
|---|---|---|
| 1 | 重量ロック | `ShareUpdateExclusive`: 待つ（`acquire`）。そのとき**ラッチ・ピン・ゲート・バリアを持たない**。`AccessExclusive`: `try_acquire` だけ（待たない。デッドロックしない）。AccessExclusive は切り詰めの直後に `release` する |
| 3 | `exclusive_barrier` | 末尾の切り詰めの `log_and_truncate`・`fsm.truncate` の間だけ。AccessExclusive を得た後に取る |
| 4 | コンテンツラッチ | ヒープのページは同時に 1 つ（VACUUM の 1 ページ処理）。FSM のページは、ヒープのページのラッチを持たずに取り、根と葉を同時に持たない。B+Tree の葉は 1 ページずつ（`bulk_delete`） |
| 7 | 葉のロック | ページのラッチを持ったまま `procs.is_in_progress`・`clog.status`・`multixact.expand` を呼んでよい（いずれも葉のロックで、I/O は持たずに行う。clog のページの読み込みは Mutex の外） |

- VACUUM はコミットゲートを取らない（XID を持たず、コミットしない）。
- 1 ページの変更の WAL の挿入 Mutex はラッチを持ったまま取る（M3 §5.9 の 6b）。flush（`log_and_truncate`・5.5.3 の `wal.flush`）はラッチ・ゲートを持たずに行う。

---

## 6. モジュールごとの仕様

### 6.1 `storage/page.rs`（VC-1）

- **`add_item`**（M2 §6.4 の「`LP_UNUSED` は再利用しない」を変える）:

```text
add_item(data):
  n = max_offset;  size = MAXALIGN(len);  lower, upper = pd_lower, pd_upper
  slot = PD_HAS_FREE_LINES が立っていれば、1..=n の最初の LP_UNUSED。なければ None
  need = size + (slot.is_none() ? 4 : 0)
  if need > upper - lower、または（slot.is_none() && n >= MAX_HEAP_TUPLES_PER_PAGE）、または len > 0x7FFF: return None
  new_upper = upper - size;  [new_upper, upper) を 0 で埋めて data をコピー
  行ポインタ { off: new_upper, flags: Normal, len } を slot（なければ lower の位置に追加して pd_lower += 4）に書く;  pd_upper = new_upper
  PD_HAS_FREE_LINES = has_unused_item()（再計算）
  return Some(slot の番号、または n + 1)
```

- **`add_item_at(off, data)`**（REDO 用）: `off == n + 1`（追加。`n < 185`）か、`1 <= off <= n` で `LP_UNUSED` のときだけ置く。それ以外は `false`。領域の検査と最後の `PD_HAS_FREE_LINES` の再計算は `add_item` と同じ。**同じ入力のページに対して `add_item` が返した番号で `add_item_at` を呼ぶと、バイト列が同じになる**（単体テスト。ヒープの INSERT / UPDATE の REDO の一致の根拠）。
- **`free_space`**: 4.1 のとおり。M2 の「`max_offset >= 185` なら 0」は「`LP_UNUSED` がなく `max_offset >= 185` なら 0」に変わる。
- **`item_id`**: `LP_UNUSED` / `LP_DEAD` で `lp_off != 0 || lp_len != 0` なら `BadItem`（正準形。3.1）。`LP_REDIRECT` は `item_id` が返すが、ヒープの読み手（`HeapStore` と `scan_page`）は `XX001` にする。
- **`set_prunable(xid)`**: `pd_prune_xid` が 0、または `xid` の下位 32 ビットが現在値より小さい（PG の `TransactionIdPrecedes` と同じ、符号付き 32 ビットの差が負）ときだけ書く。`next_xid` を引数に取らない（ヒープの `delete` / `update` と REDO が `xid` だけで呼べるように）。
- **`prune_xid_full`**: 3.1 の復元。
- **`init_fsm` / `fsm_kind` / `fsm_data`**: 3.3。`init_fsm` は `init_heap` と同じ 24 バイトのヘッダを作り、`pd_lower = 32`、`kind`、`data` を 0 にする。`verify` はこの形（`24 <= lower <= upper <= special`、`(lower - 24) % 4 == 0`）を通る。

### 6.2 `storage/fsm.rs`（VC-2）

- 3.3 と 5.7.1。ファイルの作成: `smgr.create(rel, ForkNumber::Fsm)`（`exists` で確かめてから。競合して `create` が「すでにある」で失敗したら無視する）。作成は WAL に書かない。**ファイルがあるのにリレーションの主フォークがない状態**にはしない（`record` は主フォークがある `rel` にだけ呼ばれる）。
- `FreeSpaceMap` は状態を持たない（バッファプールだけ）。`Arc<FreeSpaceMap>` を `HeapStore::new` が受け取る（00 §4.5）。
- 共有リレーション（`db_oid = 0`）の FSM は `global/<relfilenode>_fsm`。

### 6.3 `storage/buffer`（VC-2。C の範囲に足す）

`read_buffer_zero_on_error(tag)`:

```text
  if tag.block >= smgr.nblocks(tag.rel, tag.fork)?: return Ok(None)         // 存在しないブロック
  get_buffer の経路（lookup → register → load_frame）で、load_frame の verify が失敗したとき、
     abort_load せずに latch を 0 で埋め、valid = true、dirty = false にして続ける。WARNING をログ
     （"invalid page in block N of relation PATH; treating as empty"）
  smgr.read_block の I/O エラーはそのまま Err
```

- ページを 0 にしても dirty にしないので、書き換えるまでディスクの不正な内容は残る（次の `record` が書く）。FSM のページ以外には使わない（`fork == Fsm` を debug_assert）。

### 6.4 `storage/heap`（VC-1・VC-2）

| ファイル | 変更 |
|---|---|
| `hio.rs` | 4.1 の `HioCtx`。M2 の `insert_tuple` と `InsertHints` の使い方を、5.7.2 の `find_target` + 呼び出し側のラッチ・判定・配置に置き換える（配置と WAL は heap_store の `insert` / `update`。D の持ち物）。`try_prune_for_space` は `candidate` → `prune_opt`（5.2.3）。`HioCtx::new(pool, fsm, heap_env 用の clog / procs / multixact, wal)` |
| `scan.rs`（第 11 節で依頼） | `HeapScan::next_tuple` が 1 ページを読むとき、5.2.3 の手順で機会的 pruning を行う。共有ラッチ → `candidate` が真なら外して `try_write` → `prune_opt` → 共有ラッチを取り直す。**取り直した後のページの内容でそのページを走査する**（ブロック数は変わらない）。`HeapEnv` と `next_xid` を引数に足す |
| `heap_store.rs`（同） | `fetch` で同じ機会的 pruning（`candidate` が真のときだけ）。`delete` / `update` が成功したら `modstat::note_modified(rel.locator, 1)`。`HeapStore::env()` を足す。`unlink_storage` で `hio.forget(rel)`。`TableStore::inplace_update` の実装（5.8.1）。**RW の `delete` / `update` が xmax を書くとき `Page::set_prunable(xid)` を呼ぶ**（RW の範囲。第 11 節） |
| `wal.rs`（RW と共有。第 11 節） | ヒープの `INSERT` / `UPDATE` の REDO は `add_item` ではなく `add_item_at(offnum, data)` を使い、`false` なら `Panic`。`DELETE` / `UPDATE` の REDO は旧版の xmax を書くとき `set_prunable(xmax)` も行う |
| `visibility.rs`（RW） | 先頭で `XMIN_FROZEN` を見る（3.2）。`HeapTuple.xmin` は凍結済みなら `Xid::FROZEN` |
| `scan_page` が読むヘッダ | `TupleHeader::read` が `XX001` にする「使わないビット」の検査に、`XMIN_COMMITTED` / `XMIN_INVALID` の単独のビットは含めない（M2 のとおり読んでも無視する）。`LP_DEAD` / `LP_UNUSED` / `LP_REDIRECT` の行ポインタの `item()` は `BadItem`（`normal_range` が `Normal` だけを許す。M2 のとおり） |

- **スキャンの結果の `ctid`・TID が再利用される影響**: `HeapScan` と `fetch` は TID を返すだけで、持ち越さない。UPDATE / DELETE は TID を持ち越すが、そのタプルは自分のスナップショットで可視だったものなので、5.3.5 の P2 により除去・再利用されない。

### 6.5 `txn/clog.rs`（VC-4。ロックなし化は LK-3）

```rust
impl Clog {
    /// 変更: oldest は制御ファイルの oldest_xid
    pub fn open(vfs: Arc<dyn Vfs>, next_xid: Xid, oldest: Xid) -> Result<Clog>;
    pub fn oldest(&self) -> Xid;                                   // AtomicU64
    /// 追加の検査: xid が通常の XID で xid < oldest なら Err(XX001)
    ///   "transaction {xid} status is no longer available" DETAIL "The commit log was truncated before transaction {oldest}."
    pub fn status(&self, xid: Xid) -> Result<XidStatus>;
    /// 00 §4.3 の署名どおり。呼び出し側（5.5.3）が先に制御ファイルの oldest_xid を fsync する
    pub fn truncate_before(&self, oldest: Xid) -> Result<()>;
    /// 起動時: 自分の oldest 未満のセグメントのファイルを消す（truncate_before のファイル削除の部分だけ。冪等）。消した数を返す
    pub fn sweep_old_segments(&self) -> Result<u32>;
}
```

- `truncate_before(oldest)`: (1) `flush_lock` を取る（`flush` と直列化する。切り詰めたセグメントを `flush` が書き直さないため）、(2) `oldest <= self.oldest` なら何もしない、(3) `self.oldest.store(oldest)`（**ファイルを消す前に**メモリを更新する。以後 `status` が `XX001` を返す）、(4) メモリ上のページのうち、そのページの最後の XID が `oldest` 未満のもの（dirty でも）を捨てる、(5) `pg_xact/` を `read_dir` し、セグメント番号 `s`（16 進 12 桁）が `(s + 1) × 1,048,576 <= oldest` のファイルを `remove_file`（`NotFound` は無視）、(6) 1 つでも消したら `pg_xact/` を `sync_dir`、(7) 消した数をログ。失敗は `Severity::Panic` にしない（`ERROR`。次の起動時の掃除が拾う）。
- `set_status_redo(xid, ..)`: `xid` が通常で `xid < oldest` なら `Ok(())`（何もしない）。
- `ensure_page_for` / `set_status` は変えない（実行中の XID は `oldest` 以上）。
- 通常の XID 以外（`BOOTSTRAP` / `FROZEN`）は `status` が従来どおり `Committed`（`oldest` の検査より先）。

### 6.6 B+Tree（`storage/btree/vacuum.rs`、VC-3）

- 手順は 5.4。`BtreeStore` の `IndexStore::bulk_delete` は `btree::vacuum::bulk_delete` に委ねる。`BTREE_VACUUM` の WAL は `btree/wal.rs`（M4 の B1。定数だけ）から `btree::vacuum::redo` に振り分ける（`recovery::dispatch` の口は F0）。
- 葉の項目を読む関数・`delete_items(page, &[u16])`（行ポインタとタプルの削除と詰め）は M4 の B+Tree のページ操作の持ち主（B1）に依頼するか、無ければ `vacuum.rs` に書く。**通常の経路と REDO が同じ関数を使う**。
- `IndexStore` を実装する試験用のフェイク（`MemIndexStore` など）の `bulk_delete` は、`Err(Error::not_supported(..))`（`0A000`）でよい。
- 7.2 で確かめること: 空の葉を `insert`・`begin_scan` が正しく扱う（M4 のテストへの追加を RW に依頼する）。

### 6.7 SQL: 構文・解析・実行（VC-3）

**構文**（`sql/parser/vacuum.rs`。00 §8 の表に行がないので 第 11 節で依頼）:

```text
vacuum_stmt  := VACUUM [ '(' option [, ...] ')' | legacy_flag ... ] [ target [, ...] ]
analyze_stmt := ANALYZE [ '(' option [, ...] ')' | VERBOSE ] [ target [, ...] ]
legacy_flag  := FULL | FREEZE | VERBOSE | ANALYZE                                   // 順不同。ANALYZE は VACUUM ANALYZE
option       := name [ boolean | integer | identifier ]                              // name は大文字小文字を区別しない
target       := qualified_name [ '(' column [, ...] ')' ]
boolean      := TRUE | FALSE | ON | OFF | 1 | 0（'on' などの文字列リテラルも可）
```

AST（`sql/ast.rs`。F0 が空の構造体で置き、VC が中身を足す）:

```rust
pub struct VacuumStmt { pub options: Vec<VacOptAst>, pub targets: Vec<VacTargetAst>, pub with_analyze_keyword: bool }
pub struct AnalyzeStmt { pub options: Vec<VacOptAst>, pub targets: Vec<VacTargetAst> }
pub struct VacOptAst { pub name: String, pub value: Option<VacOptValue> }
pub enum VacOptValue { Bool(bool), Int(i64), Ident(String) }
pub struct VacTargetAst { pub table: QualifiedName, pub columns: Vec<Ident> }
```

**解析**（`analyzer/ddl_ext/vacuum.rs`）— ロックは取らず、名前を OID にするだけ:

| 検査 | SQLSTATE とメッセージ |
|---|---|
| 未知のオプション | `42601` `unrecognized VACUUM option "x"`（ANALYZE は `unrecognized ANALYZE option "x"`） |
| `FULL`（真） | `0A000` `VACUUM FULL is not supported yet` |
| `INDEX_CLEANUP` の値が `on` / `off` / `auto` 以外（真偽値として読めない値） | **`42601` `index_cleanup requires a Boolean value`**（【実機】PG17.11。`defGetBoolean` 経由。`TRUNCATE foo` も同じ形で `42601` `truncate requires a Boolean value`。09 §6.2 と同じ規則。レビュー対応 R-23） |
| `PARALLEL n` が 0〜1024 以外 | **`42601`** `parallel workers for vacuum must be between 0 and 1024`（【実機】`parallel -1`。1000 は通る） |
| `BUFFER_USAGE_LIMIT x` が 0 でも 128kB〜16777216kB でもない | `22023` `BUFFER_USAGE_LIMIT option must be 0 or between 128 kB and 16777216 kB`（【実機】。値は検査だけして使わない） |
| 列リストがあるのに `ANALYZE` がない（VACUUM） | **`0A000`** `ANALYZE option must be specified when a column list is provided`（【実機】PG17.11。`ExecVacuum` の `ERRCODE_FEATURE_NOT_SUPPORTED`。レビュー対応 R-23） |
| テーブルが存在しない | `42P01` `relation "x" does not exist`（スキーマがなければ `3F000`） |
| 列が存在しない | `42703` `column "c" of relation "t" does not exist` |
| `ONLY_DATABASE_STATS` と対象テーブルの併用 | **`0A000`** `ONLY_DATABASE_STATS cannot be specified with a list of tables`（【実機】PG17.11） |

**実行**（`ddl/vacuum.rs`）:

```text
execute_vacuum(ctl, v):                            // ddl::execute_standalone から（00 §4.7）
  if !ctl.ddl_ctx()?.outside_block: Err(25001 "VACUUM cannot run inside a transaction block")      // session が先に検査する。二重に確かめる
  targets = v.targets が空なら、list_vacuum_relations（oid 昇順）から作る（ONLY_DATABASE_STATS ならなし）
  for t in targets:
      ctx = ctl.ddl_ctx()?                         // この時点の暗黙のトランザクションの文脈
      env = VacuumEnv { cluster: ctx.cluster, db: ctx.db, backend: ctx.backend, wait: ctx.wait, notices: ctx.notices,
                        interrupts, lock_scope: Transaction, own_xid: None, is_autovacuum: false }
      out = vacuum_relation(&mut env, t.oid, &v.options)?
      Done(s) なら ctx.txn.wal_flush_upto = max(ctx.txn.wal_flush_upto, s.last_lsn)           // コミットで flush される（00 D48）
      ctl.commit_and_restart()?                    // コミット（XID なし）→ ロック解放 → 新しい暗黙のトランザクション。登録済みスナップショットも外れる
  skip_database_stats でなければ: env を作り直して update_datfrozenxid_and_truncate_clog
  Ok("VACUUM")

execute_analyze(ctx, a):                           // ddl::execute の BoundDdl::Analyze。ユーザーのトランザクションの中
  targets（空ならデータベース全体）の各 t について analyze_relation(&mut env { lock_scope: Transaction, own_xid: ctx.txn.xid }, ..)
  Ok("ANALYZE")
```

- テーブルごとにコミットするので、**途中のエラー・キャンセルでも、終わったテーブルの結果は残る**（PG と同じ）。
- `VACUUM` は `READ ONLY` のトランザクションの中では動かない（25001 が先）。`default_transaction_read_only = on` の暗黙のトランザクションで動くかは PG で確かめる（未検証。第 9 節）。
- `ANALYZE`（単独）は `analyze_relation` が自分でも `ShareUpdateExclusive` を取る（Session の `locking.rs` が文の前に取っていても、参照カウントなので問題ない）。
- コマンドタグ: `VACUUM`、`ANALYZE`。

### 6.8 設定・エンジン・サーバ

- **`settings.rs`**（VC の行だけ）: `Settings::maintenance_work_mem_bytes(&self) -> usize`（M4 が保存する値を読む。下限 1 MiB に切り上げる）。`INERT_GUCS` に次を足す（値の形だけ検査して保存する）: `vacuum_freeze_min_age`、`vacuum_freeze_table_age`、`vacuum_multixact_freeze_min_age`、`vacuum_multixact_freeze_table_age`、`vacuum_failsafe_age`、`vacuum_multixact_failsafe_age`、`vacuum_cost_delay`、`vacuum_cost_limit`、`vacuum_cost_page_hit`、`vacuum_cost_page_miss`、`vacuum_cost_page_dirty`、`vacuum_buffer_usage_limit`。
- **autovacuum の設定**（`autovacuum`、`autovacuum_naptime`、`autovacuum_vacuum_threshold`、`autovacuum_vacuum_scale_factor`、`autovacuum_freeze_max_age`）は postmaster 設定: `SHOW` は読み取り専用の値（`Cluster::server_settings`）を返し、`SET` は `55P02` `parameter "autovacuum" cannot be changed now`（PG と同じ文言）。PG にあって yuzhu が実装しない `autovacuum_*`（`autovacuum_max_workers`、`autovacuum_analyze_*`、`autovacuum_vacuum_cost_*`、`autovacuum_vacuum_insert_*` ほか）は `INERT_GUCS`（保存だけ）。
- **`ClusterOptions`**（`engine.rs`。F0 が項目の口を作る）:

```rust
pub struct ClusterOptions { /* 既存の項目に加えて */
    pub autovacuum: AutovacuumConfig,                 // 既定: enabled = false
    pub vacuum_truncate_lock_timeout: Duration,       // 既定 5 秒（末尾の切り詰めの AccessExclusive を待つ上限。テストは短くする）
}
```

- **`yuzhu-server/src/config.rs`**: TOML のキー `autovacuum`（bool）、`autovacuum_naptime`（秒。1 以上）、`autovacuum_vacuum_threshold`（0 以上）、`autovacuum_vacuum_scale_factor`（0〜100 の実数）、`autovacuum_freeze_max_age`（1 以上）と、コマンドライン `--autovacuum`（フラグ。TOML より優先）。範囲外は起動を拒否する（既存の検査の流儀）。
- **`Cluster::open`**: `StorageStack::new` が `Clog::open(.., control.oldest_xid)` を呼び、直後に `sweep_old_segments` → REDO → 終了チェックポイント → **`sweep_orphans`（5.10.3）** → チェックポインタ → `autovacuum::spawn`（有効なら）。`Cluster::shutdown`: `AutovacuumHandle::stop` → チェックポインタの停止 → 停止チェックポイント。

### 6.9 エラーとメッセージの一覧（この章が返すもの）

| 状況 | SQLSTATE | メッセージ |
|---|---|---|
| ブロック内の `VACUUM` | `25001` | `VACUUM cannot run inside a transaction block`（00 §3.8） |
| `VACUUM FULL` | `0A000` | `VACUUM FULL is not supported yet` |
| `TRUNCATE` が FK に参照されている | `0A000` | `cannot truncate a table referenced in a foreign key constraint` / DETAIL `Table "c" references "p".` / HINT `Truncate table "c" at the same time, or use TRUNCATE ... CASCADE.` |
| `TRUNCATE ... CASCADE` | `0A000` | `TRUNCATE ... CASCADE is not supported yet` |
| 未知のオプション・存在しない表・列 | `42601` / `42P01` / `42703` | 6.7 の表 |
| 表でないものの VACUUM / ANALYZE | WARNING（01000） | `skipping "x" --- cannot vacuum non-tables or special system tables`（ANALYZE は `cannot analyze non-tables or special system tables`） |
| `SKIP_LOCKED` でロックが取れない | WARNING | `skipping vacuum of "t" --- lock not available`（ANALYZE は `skipping analyze of ...`） |
| 並行して消えた表 | WARNING | `skipping vacuum of "t" --- relation no longer exists` |
| キャンセル・`statement_timeout` | `57014` | `canceling statement due to user request` / `... statement timeout` |
| clog の切り詰めた範囲の XID を引いた | `XX001` | `transaction N status is no longer available` |
| 第 3 段で `LP_DEAD` でない行ポインタ | `XX001` | `dead item identifier {off} of block {blk} is not dead` |
| ページの破損 | `XX001` | M2 のとおり |
| `yz_relxid` に行がない表 | WARNING（ログと通知） | `yz_relxid has no row for table "x"` |
| `SET autovacuum = ...` | `55P02` | `parameter "autovacuum" cannot be changed now` |

---

## 7. テスト

### 7.1 共有テスト（`tests/slt/m5/vacuum/`。PostgreSQL 17 でも通る）

TS-5 が集める。**期待値は PostgreSQL 17 で確かめる**（M1〜M3 と同じ規則）。共有テストは、HOT が絡む `UPDATE` の後の `ctid` に依存しない（yuzhu は HOT を使わない。`DELETE` と `INSERT` だけで書く）。PG 側は `autovacuum = off` で起動する（`tests/pg.sh` の設定。TS に依頼）。

| ファイル | 内容 |
|---|---|
| `vacuum_basic.slt` | `VACUUM`、`VACUUM t`、`VACUUM (VERBOSE) t`、`VACUUM (ANALYZE) t`、`VACUUM ANALYZE t`、`ANALYZE`、`ANALYZE t`、`ANALYZE t (a)`、`VACUUM (SKIP_LOCKED, TRUNCATE false, INDEX_CLEANUP off) t` が成功（コマンドタグ）。`VACUUM nosuch` が `42P01`。`ANALYZE t (nosuch)` が `42703`。`VACUUM idx`（インデックス）が WARNING で成功 |
| `vacuum_in_block.slt` | `BEGIN; VACUUM t` が `25001`（その後 `ROLLBACK`）。1 つの Query に複数の文（`select 1; vacuum t` と `vacuum t; vacuum u`）が `25001`。**`BEGIN; ANALYZE t; COMMIT` が成功**（PG17 で確かめる。VC-D15） |
| `vacuum_reuse_ctid.slt` | `CREATE TABLE t (id int, v text)`、100 行を挿入、偶数の `id` を `DELETE`、`VACUUM t`、新しい行を挿入して `SELECT ctid, id` が `LP_UNUSED` の再利用（最小の空き番号から）と一致。インデックスのないテーブルと、`PRIMARY KEY` のあるテーブルの両方 |
| `vacuum_stats.slt` | `SELECT relpages, reltuples FROM pg_class WHERE relname = 't'` が、作成直後（`0`、`-1`）、`ANALYZE` 後（`1`、100）、`DELETE` 50 行 + `VACUUM` 後（`1`、50）、`TRUNCATE` 後で PG と一致 |
| `vacuum_snapshot_2conn.slt` | `connection a`: `BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM h`、`connection b`: 50 行を `DELETE` → `VACUUM h` → `INSERT` して `ctid` を確かめる（**再利用されない**。PG 実機 #11）→ `connection a`: `COMMIT` → `connection b`: `VACUUM h` → `INSERT`（**再利用される**） |
| `vacuum_rc_idle_2conn.slt` | `connection a`: Read Committed で `BEGIN; SELECT ...`（文の後はアイドル。XID なし）、`connection b`: `DELETE` → `VACUUM` → `INSERT`（**再利用される**。PG 実機 #11b） |
| `vacuum_xid_held_2conn.slt` | `connection a`: `BEGIN; INSERT INTO other VALUES (1)`（XID を持つ）、`connection b`: `DELETE` → `VACUUM` → `INSERT`（**再利用されない**。PG 実機 #11c と同じ結果になる形） |
| `vacuum_index.slt` | `PRIMARY KEY` のある表で `INSERT` / `DELETE` / `VACUUM` / 同じキーの `INSERT` を繰り返し、`SET enable_seqscan = off` のインデックススキャンと逐次スキャンの結果が一致（A7 の確認。PG と同じ結果） |
| `truncate_rollback.slt` | `BEGIN; TRUNCATE t; ROLLBACK` の後に行が残る。`TRUNCATE` 後の `INSERT` と `VACUUM` が動く |
| `vacuum_full_unsupported.slt`（**yuzhu だけ**。`--target pg` では流さない） | `VACUUM FULL t` と `VACUUM (FULL) t` が `0A000`。`TRUNCATE a, b CASCADE` が `0A000`（FK 章の後） |

### 7.2 Rust のテスト（`yuzhu-core`）

| 対象 | 内容 |
|---|---|
| `page`（VC-1） | `add_item` の再利用（最小の `LP_UNUSED`、`PD_HAS_FREE_LINES` の再計算、185 個の上限）、`add_item_at` と `add_item` の一致（同じ入力 → 同じバイト列。proptest）、`free_space` の 3 つの場合、`repair_fragmentation`（proptest: タプルのバイト列と行ポインタ番号が保たれる、`pd_upper` が最大、穴と末尾のパディングが 0、末尾の `LP_UNUSED` が外れる、`LP_DEAD` は外れない、2 回適用しても同じ）、3.1 の例の 3 つの状態（固定値）、`item_id` の正準形 |
| `satisfies_vacuum`（VC-1） | 5.1 の真理値表の全行（フェイクの `RunningXids` と実 `Clog`）、クラッシュの残り（clog が `IN_PROGRESS` で実行中でない）、`LockOnly(Single / Multi)`、凍結済みの xmin、`xid_state` の順序（一覧 → clog。clog を先に見る変異で失敗することも確認） |
| `scan_page` / `apply_plan`（VC-1） | 各結果の組み合わせのページ（`Dead` / `RecentlyDead` / `Live` / 進行中）から作った計画、凍結の 4 通り（xmin のみ、xmax の無効化の 3 通り）、`prune_xid` の最小、`mark_unused_now`（既存の `LP_DEAD` も `unused`）、空の計画、`own_xid` の数え方 |
| WAL（VC-1・VC-3） | 3.5 の例の固定値（エンコード・デコードの往復）、長さの不整合の検出、**REDO のバイト一致**（操作を実行してページの写しを取る → 空のページ（または古いページ）に同じ WAL を REDO → 写しと一致。`PRUNE_FREEZE`・`VACUUM_UNUSED`・`INPLACE`・`BTREE_VACUUM`、2 回 REDO しても同じ）、画像（FPI）が付く場合と付かない場合 |
| 機会的 pruning（VC-1） | `candidate` の各条件、`try_write` が取れないとき（別スレッドが共有ラッチを持つ）に諦める、スキャンと `fetch` と `hio` の 3 か所で動く、リカバリ中は動かない、**スキャンが可視なタプルを消さない**（登録済みスナップショットと並行する削除 + pruning。P0） |
| FSM（VC-2） | `avail_to_cat` / `needed_to_cat` の境界（0、31、32、8160、8164、8200）、`record` → `search`、`next_slot` の循環、第 2 葉（ブロック 8160 以上）、`FSM_MAX_BLOCKS` 以上は無視、根が古いときの修正、`truncate`、ファイルがない・全 0・チェックサム不正・`kind` 不正で `search` が `None`（zero-on-error。失敗しない）、`record_batch`、3.3 の例の固定値、FSM の WAL が 1 つも書かれないこと |
| `hio`（VC-2） | ヒント → FSM → 最後のページ → 拡張の順、`tried` の除外、`blk >= nblocks` のエントリを捨てる、FSM を削除・破壊しても挿入が成功してテーブルが膨らまない（FSM なしで最後のページが使われる）、入らないページで `try_prune_for_space` が領域を作って挿入が入る、VACUUM で空いた領域を挿入が再利用してファイルが伸びない（`nblocks` が変わらない） |
| `TidSet`（VC-3） | `push` の順序、`contains`、`groups`、`is_full`、容量の小さい設定で第 2・3 段が複数周する |
| `inplace_update`（VC-3） | 固定長・非 NULL 以外の列で内部エラー、`TupleGone` の 3 条件、`pg_class` の行の `relpages` / `reltuples` / `relfrozenxid` の上書きと REDO、同じ行を読んでいるスレッドが古い値か新しい値のどちらかだけを見る |
| `bulk_delete`（VC-3） | 空の葉が残る、右端でない葉の high key が消えない、全部消して空になったインデックスに `insert` / `begin_scan` が動く、**巡回中に分割が起きても見落とさない**（別スレッドが挿入し続ける）、`BTREE_VACUUM` の REDO、統計 |
| `vacuum_relation`（VC-3） | 3 段階の順序（`SimVfs` の操作と WAL の並びで確認: `PRUNE_FREEZE` → `BTREE_VACUUM` → `VACUUM_UNUSED`）、`LP_DEAD` の集め直し（途中でキャンセルしたあとの再実行）、`index_cleanup = Off`、インデックスなしの 1 パス、`maintenance_work_mem` が小さいときの複数周、`SKIP_LOCKED`、表でないものの WARNING、並行して消えた表、`vacuum_scan_page` の排他ラッチ中に別スレッドの INSERT / UPDATE が走る、統計（`relpages`・`reltuples`・インデックス）、VERBOSE の出力（キーワードの存在。行の並びは比較しない） |
| horizon（VC-3） | PG 実機 #11 / #11b / #11c の 3 通りを `TxnManager` と `ProcArray` で再現（RR のスナップショットが止める、RC の文の外では止めない、XID を持つだけで止める）。`removable_cutoff` と `tuples_deleted` / `recently_dead_tuples` の値。**登録されていないスナップショットでは止まらないことを確かめる試験（P4 の違反の再現）**を 1 本入れ、VC-D23 の理由を残す |
| A7（VC-3） | 次のインターリーブを `Barrier` で決定的に作る: (1) 走査 X が葉から TID をコピー、(2) VACUUM が第 2・3 段を完了、(3) 別のトランザクションが同じ TID に新しい行を挿入、(4) X がその TID を読む → X のスナップショットで不可視（飛ばされる）。さらに `vacuum_skip_index_pass`（第 2 段を省く）で `LP_UNUSED` を指す項目が残り V1 が破れることを確かめる |
| 凍結・clog（VC-4） | `relfrozenxid8` の単調増加、R1・R2 の検査関数（全ページを走査して違反を数える）、`yz_relxid` の行の欠落が fail-closed（`min_relxid` が `Xid(3)`）、`datfrozenxid8` の前進のみ、番兵（template0）が最小に影響しない、`CREATE DATABASE` の規則（`min(src, horizon)`。09 章と合わせる）、`truncate_before`（セグメントの境界、`oldest` ちょうど、メモリのページの破棄、`flush` との直列化）、`status` の `XX001`、`set_status_redo` の無視、起動時の掃除、制御ファイルの `oldest_xid` の fsync の順序（`SimVfs` の操作の順）。XID を跳ばす試験用の口（`TxnManager::test_skip_xids(n)`。`testing.rs`。T が提供）で 1 セグメント（1,048,576）を越えさせて、実際にファイルが消えることを確かめる |
| TRUNCATE（VC-5） | FK の参照先を `0A000`（FK-1 が入るまではフェイクの `check_truncate_fks`。07 の `check_truncate_fks` が返すエラーをそのまま返すこと）、同じ文で参照元も並べれば成功、自己参照、`CASCADE` の `0A000`、`yz_relxid` の更新がロールバックで戻る、FSM のファイルが新しい relfilenode にない |
| 末尾の切り詰め（VC-5） | 閾値（999 ブロックの空きでは切らず 1000 で切る、1/16）、`AccessShare` を持つトランザクションがあると切らずに `vacuum_truncate_lock_timeout` で諦める、待ち手がいると autovacuum が中断する、切り詰めの後の `nblocks`・FSM・挿入先のヒント、`SMGR_TRUNCATE` の REDO、`LP_DEAD` が残るページは切らない |
| 起動時掃除（VC-5） | 0 バイトの孤児を消す、0 バイトでない孤児は残して WARNING、`pg_class` の行がある 0 バイトのファイル（空のテーブル）は消さない、マップされたカタログ（`relfilenode = 0`）のファイル、`_fsm` と `.1` は対象外、`pg_class` が不完全なら飛ばす、`SimVfs` の障害 |
| autovacuum（VC-6） | `needs_vacuum` の境界（閾値、`reltuples = -1`、`freeze_max_age`）、`ModStats` の合算とスレッドローカルの `drain`、スレッドの起動・停止・実行中の VACUUM の中断、`skip_locked` で手動の VACUUM と並ばない、待ち手で中断、小さい `naptime` で実際にテーブルが片付く（統合） |
| SQL（VC-3） | 構文（オプションの全形、旧形式、真偽値の書き方）、解析のエラー（6.7 の表）、`execute_vacuum` が `TxnControl` の fake で「テーブルごとに `commit_and_restart`」「最後に 1 回 datfrozenxid」、`execute_analyze` がブロック内で動く、`outside_block` が偽なら 25001 |
| 設定（VC-6） | `config.rs` の 5 つのキーと範囲、`SHOW autovacuum`、`SET autovacuum` が 55P02 |

### 7.3 分離性テスト（`tests/isolation/specs/`。TS-1 のランナーで PostgreSQL にも流す）

VACUUM 自体は長く止められないので、`LOCK TABLE ... IN SHARE UPDATE EXCLUSIVE MODE` で VACUUM と同じロックを再現する。

| spec | 内容 |
|---|---|
| `vacuum-lock-conflict.spec` | A: `BEGIN; LOCK TABLE t IN SHARE UPDATE EXCLUSIVE MODE`。B: `VACUUM t`（**ブロック**）、`VACUUM (SKIP_LOCKED) t`（待たずに WARNING で成功）、C: `INSERT INTO t`（ブロックしない）、D: `DROP TABLE t`（ブロック）。A の `COMMIT` で B が進む |
| `vacuum-dml-concurrent.spec` | A が `INSERT` / `DELETE` を繰り返す間に B が `VACUUM`。B の完了後、A のコミットした行がすべて見える |

### 7.4 並行ストレス（TS-2 と合わせる。`yuzhu-server/tests/concurrency.rs`）

送金の不変条件（合計一定）の負荷に、**VACUUM のループ**と **主キーのインデックススキャンで残高を読む読み手**と RR の読み手を同時に走らせる。確かめること: 合計が常に一定（RC・RR とも）、インデックススキャンの結果と逐次スキャンの結果が一致（A7）、エラーは `40001` / `40P01` / `55P03` の予期されたものだけ。テーブルの `nblocks` が有界（FSM と pruning が効いて膨らみ続けない）。

### 7.5 クラッシュ試験（層 1。TS-3 と `crash_sim/`）

M3 §7.5 のハーネス（`SimVfs`、1 スレッドで複数セッション）に、次の**ワークロード 6**を足す: 小さい表（主キーあり）に対する `INSERT` / `UPDATE` / `DELETE` と `VACUUM` / `VACUUM (TRUNCATE)` / `CHECKPOINT` の混在。XID を跳ばして clog の切り詰めまで通すバリエーション。クラッシュ点は I/O の通し番号の全網羅（小さいワークロード）。M3 の不変条件 I1〜I12 に加えて、次の **V1〜V8** を検査関数（`invariants.rs`）にする:

| # | 不変条件 | 方法 |
|---|---|---|
| V1 | **インデックス → ヒープ**: 全インデックスの全葉の項目の TID について、`block < nblocks` で、行ポインタが `LP_UNUSED` でも範囲外でもない | B+Tree を全葉走査してヒープを引く |
| V2 | **MVCC の結果が変わらない**: モデルとの一致（I4）に加え、VACUUM の前後で同じスナップショットから見た全表の内容が同じ | 登録済みスナップショットで VACUUM の前後を読み比べる |
| V3 | **clog の不変条件**: `LP_NORMAL` のタプルについて、`xmin < oldest_xid` なら `xmin` が 1・2 か `XMIN_FROZEN`。有効な xmax が `oldest_xid` 未満のものがない（ロックだけでも）。`Clog::status` の `XX001` が 1 度も出ない | 全ヒープを走査して検査。ワークロード中の `XX001` の件数を数える |
| V4 | **FSM に依存しない**: `_fsm` のファイルを削除する・ランダムな内容にする・0 にしても、全表の内容が同じで、以後の挿入が成功する | クラッシュの後の検査の一部として、コピーしたイメージに適用 |
| V5 | **境界の単調性と上限**: `relfrozenxid8` ≤ 表の最小の未凍結 XID、`datfrozenxid8` ≤ 各表の `relfrozenxid8`、`control.oldest_xid` ≤ 番兵を除く各データベースの `datfrozenxid8`。再起動をまたいで `oldest_xid` が減らない | 全表走査と制御ファイル |
| V6 | **WAL と制御ファイルの順序**: `oldest_xid` が進んでいるなら、その値を決めた根拠の凍結の WAL（`PRUNE_FREEZE`・`INPLACE`）が永続化済み | 変異テスト（下）で検出できることを確かめる |
| V7 | **ページの形**: `verify` が通る、`LP_DEAD` / `LP_UNUSED` が正準形、`PD_HAS_FREE_LINES` が `LP_UNUSED` の存在と一致、`repair_fragmentation` が冪等 | 全ページ |
| V8 | **REDO の決定性**: 各クラッシュ点の WAL を 2 回 REDO しても同じページ | I9 と同時に検査 |

**変異テスト**（M3 §7.5 の `DebugKnobs`。00 D29。第 11 節で 6 項目を依頼。**名前は 10 章 §6.9 と一致させた。レビュー対応 R-09**）:

| 変異（10 章 §6.9 の #） | 方法 | 検出されるべき不変条件 |
|---|---|---|
| `vacuum_skip_index_pass`（#1） | 第 2 段を省いて第 3 段へ進む（`LP_UNUSED` がインデックスの削除より先になる） | V1（`LP_UNUSED` を指す項目）、A7 の試験 |
| `vacuum_ignore_registered_snapshots`（#2。**LK §6.7 が `ProcArray::oldest_xmin` に実装する共有のスイッチ**。以前この章が持っていた `vacuum_ignore_horizon` を統合した。VC は別のスイッチを作らない） | horizon を実行中の XID だけで計算する（登録済みスナップショットを無視。`next_xid` まで進む） | V2（RR の読み手が行を失う） |
| `freeze_skip_wal`（#3） | 凍結をページに適用するが WAL を書かない + `DropUnsynced` + clog の切り詰め | V3（`XX001` または未凍結の検出）、V6 |
| `truncate_clog_before_control_fsync`（#4） | 制御ファイルの `oldest_xid` を fsync する前に `pg_xact` のセグメントを消す | V3、I19（クラッシュ後に `XX001` または起動の失敗） |
| `relfrozenxid_too_high`（#5） | `yz_relxid` の `relfrozenxid8` を実際より進めて書く | I20（凍結していない XID が clog の切り詰めで引けなくなる） |
| `fsm_trust_corrupt`（#6） | FSM のチェックサム不正を 0 のページとして扱わず、そのまま信じる | I18 |
| `clog_truncate_ignore_floor`（#19） | `floor` を `next_xid` にして切り詰める | V3（`XX001`） |

各変異について「既定のシード集合のうち少なくとも 1 つで検出する」ことをテストにする（M3 と同じ）。

### 7.6 クラッシュ試験（層 2。`yuzhu-server/tests/crash_kill9.rs`）

銀行振込・追記ログのクライアントに、`VACUUM`（引数なし）を周期的に流す接続を足す。0.5〜3 秒のランダムな時刻に `kill -9` → 再起動 → 不変条件（I1〜I4、V1・V3）を SQL で検査（`VACUUM` を再実行して成功すること、`SELECT count(*)` が一致すること、`pg_xact` の古いセグメントが消えていても読めること）。

### 7.7 CI

- yuzhu の slt ジョブに `tests/slt/m5/vacuum/` を足す（`vacuum_full_unsupported.slt` は yuzhu だけ）。PG 側のジョブも流して期待値を確かめる。
- 分離性ジョブに `vacuum-*.spec`。
- 夜間: 層 1 のワークロード 6 の長時間ランダム、層 2、ストレス（7.4）。

---

## 8. 実装の分担と工数

00 §7 の WP の ID を使う。工数は AI の実装エージェント 1 本の日数（粗い見積もり）。**合計 20 日**（VC-1 4、VC-2 3、VC-3 5、VC-4 4、VC-5 2、VC-6 2）。

| WP | 内容 | 持つファイル | 依存 | 日数 |
|---|---|---|---|---|
| **VC-1** | `satisfies_vacuum`、`scan_page` / `apply_plan` / `prune_and_log`、`pd_prune_xid`、機会的 pruning（`candidate`・`prune_opt`・`scan.rs` と `fetch` のフック）、`Page` の操作（`add_item` の再利用、`add_item_at`、`repair_fragmentation`、`free_space`、`set_prunable`、正準形）、`HEAP2 PRUNE_FREEZE` / `VACUUM_UNUSED` の形式と REDO、ヒープの `INSERT` / `UPDATE` の REDO の `add_item_at` 化、`modstat.rs` | `storage/page.rs`、`storage/heap/{prune,wal2,modstat}.rs`、`scan.rs`・`heap_store.rs` の該当箇所 | F0（スタブ）。LK-3 は待たない（`RunningXids` のフェイク）。RW-1 の `decode_xmax` / `invalidate_xmax` / `with_xmin_frozen`（02 §4.1）は最初にスタブで合わせる | 4 |
| **VC-2** | FSM（形式、`FreeSpaceMap`、`read_buffer_zero_on_error`）、`hio.rs` の組み込み（`HioCtx`、`find_target`、`try_prune_for_space`）、VACUUM での再構築の口（`record_batch`・`truncate`） | `storage/fsm.rs`、`storage/buffer/mod.rs`（1 関数）、`storage/heap/hio.rs`、`heap_store.rs`（挿入・更新の組み込み。D / RW と調整） | VC-1 | 3 |
| **VC-3** | VACUUM 本体（3 段階、`TidSet`、`bulk_delete` と `BTREE_VACUUM`、統計の更新（`inplace_update`・`set_relstats_inplace`）、`VACUUM_UNUSED` の呼び出し、VERBOSE、ANALYZE、構文・解析・`execute_vacuum` / `execute_analyze`、`vacuum_relation` の末尾の切り詰めを除く部分） | `vacuum/{mod,driver}.rs`、`storage/heap/{vacuum,inplace}.rs`、`storage/btree/vacuum.rs`、`catalog/store_vac.rs`、`ddl/vacuum.rs`、`analyzer/ddl_ext/vacuum.rs`、`sql/parser/vacuum.rs` | VC-1、VC-2、LK-4（ロックの取得手順）、M4（B+Tree・`ddl/`・`BoundVacuum` のスタブ）、RW-5（空の葉と一意検査のラッチ） | 5 |
| **VC-4** | 凍結の記録（VC-3 が書く計画の凍結部分の仕上げ）、`yz_relxid` / `yz_datxid`（schema、initdb の初期行、`store_vac` の読み書き）、`relfrozenxid8` の進め方、`datfrozenxid8`、clog の切り詰め（`oldest_xid`、`Clog` の変更、起動時掃除、REDO の無視）、可視性の凍結（RW に依頼した分の結合） | `catalog/schema.rs`、`catalog/store_vac.rs`（`yz_*` の部分）、`txn/clog.rs`（VC の範囲）、`vacuum/driver.rs`（5.5）、`engine.rs`・`recovery.rs`・`bootstrap.rs` の該当箇所 | VC-3、LK-3（`oldest_xmin`・`Clog` のロックなし化）、DB-1（`yz_datxid` の行の規則）、F0 | 4 |
| **VC-5** | `TRUNCATE` の M5 対応（D42 の 4 点と FK 拒否の呼び口）、末尾の切り詰め（5.6.2・5.6.3）、起動時の残骸掃除（5.10.3） | `ddl/truncate.rs`、`vacuum/{mod,orphan}.rs`、`engine.rs` の呼び出し | LK-4、VC-3 | 2 |
| **VC-6** | 簡易 autovacuum（スレッド、`ModStats`、`needs_vacuum`、設定、`config.rs`、停止） | `vacuum/{autovacuum,stats}.rs`、`engine.rs`、`yuzhu-server/src/config.rs` | VC-3、VC-5（`has_waiters` の使い方） | 2 |

- **進め方**: F0 の後、VC-1 を最初に始める（LK・RW を待たずに済む）。VC-2 は VC-1 の `Page` の操作が済めば並行できる。VC-3 の B+Tree の `bulk_delete` は M4 の B+Tree の完成後。VC-4 の `yz_relxid` の schema と initdb の初期行は、VC-3 と並行して先に書ける（F0 の口に足す）。
- **他の章への依頼（00 §8 のとおり、自分では直さない）**: LK に `has_waiters`・`oldest_xmin_hint`・共有リレーションの `db = 0`、RW に 4.4 の表の項目、F0 に `Heap2`・`DebugKnobs` の 6 項目・`ClusterOptions` の項目・`ddl/table.rs` の `insert_relxid` / `delete_relxid` の呼び出し・`Session` の `modstat` の flush、TS に `tests/pg.sh` の `autovacuum = off`、T に `test_skip_xids`、DB に `yz_datxid` の行の規則。
- **カットライン**（00 §1.3。ほかの章の契約を変えずに落とせる順）: **VC-6**（autovacuum）→ **VC-4 の clog の切り詰め**（`yz_relxid` と凍結の記録、`datfrozenxid8` は残す。`oldest_xid` は 3 のまま）→ **VC-5 の末尾の切り詰め**（5.6.2。D42 の 4 点と起動時掃除は残す）。いずれも M6 に回す。落としても V1〜V8 のうち該当しないものは破れない。

---

## 9. 未検証の点（実装前に確かめるもの）

PostgreSQL 17（実機または REL_17_STABLE のソース）で確かめる。

1. **`ANALYZE`（VACUUM なし）がトランザクションブロックの中で動くこと**（VC-D15。`vacuum.c` の `vacuum()` のコメント「ANALYZE (without VACUUM) can run either way」の記憶）と、その場合の `reltuples` のロールバックの有無（その場の上書き）。
2. **VERBOSE の重大度が `INFO` であること**と、5.8.4 の各行の文言（`vacuumlazy.c` の `heap_vacuum_rel`）。
3. `heap_page_prune_opt` の条件（`minfree = max(fillfactor の目標, BLCKSZ / 10) = 819`、`PageIsFull`）と、PG17 でも `GlobalVisTestIsRemovableXid` による判定であること。
4. 定数: `REL_TRUNCATE_MINIMUM`（1000）、`REL_TRUNCATE_FRACTION`（16）、`VACUUM_TRUNCATE_LOCK_WAIT_INTERVAL`（50ms）、`VACUUM_TRUNCATE_LOCK_TIMEOUT`（5000ms）。
5. メッセージと SQLSTATE: `skipping "x" --- cannot vacuum non-tables or special system tables`、`skipping vacuum of "t" --- lock not available`、`ANALYZE option must be specified when a column list is provided` の SQLSTATE、`INDEX_CLEANUP` / `PARALLEL` の不正値の文言、`ONLY_DATABASE_STATS` と表の併用。
6. **`xmin` システム列が、凍結済みなら 2（`FrozenTransactionId`）を返すこと**（PG の `HeapTupleHeaderGetXmin`。【記憶】）。
7. `TRUNCATE` 直後の `pg_class.relpages = 0`、`reltuples = -1`。作成直後と VACUUM / ANALYZE 後の値（小さい表で正確になること）。
8. `default_transaction_read_only = on` のセッションで `VACUUM` が動くか。
9. **`ctid` の再利用が PG と一致すること**（`vacuum_reuse_ctid.slt` を PG に流す。最小の `LP_UNUSED` から再利用すること、`PageTruncateLinePointerArray` が第 3 段で末尾の `LP_UNUSED` を外すこと）。
10. `vac_update_datfrozenxid` が前進だけを行うこと（5.5.2）と、`datfrozenxid` / `relfrozenxid` の表示の仕様。
11. **M4 の B+Tree の `insert` と `begin_scan` が、データの行ポインタが 0 個の葉を正しく扱うこと**（M4 の実装前は未確認。6.6）。
12. RW が `ctid` の連鎖をたどるとき「連鎖の先の `xmin` が直前の `xmax` と等しい」ことを確かめること（5.3.5。RW の実装前は未確認）。
13. M4 の `ddl/truncate.rs` の `pg_class` の UPDATE が `relfrozenxid` を含められること（5.6.1）。
14. 起動時の残骸掃除が、M4 以降のカタログ（`pg_index`・`pg_depend`・`pg_sequence` など）の数を含めて、`pg_class` の不完全さを正しく判定できること（5.10.3 の「カタログの数」は M4 の `CATALOGS` の長さから取る）。
15. 登録しないスナップショットの使い道（00 の `snapshot()`）が、VC-D23 の「スキャンが残らない一瞬の用途」に本当に収まっていること（LK-3・F0 のコードの確認）。

---

## 10. 確認事項

仮決めして進めた点です。仮決めのままでよいか確認してください。ディスク形式に関わるもの（★）は実装の前に決めるのが望ましいです。

| ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|
| M5-VC-Q1 | horizon（VACUUM が消してよい境界）は**クラスタ全体**（VC-D4）。別のデータベースで長いトランザクションがあると、このデータベースの VACUUM もその分しか消せない（PG はデータベースごと） | 登録簿が 1 つで済む（00 D11） | データベースごとにするには、`RegisteredSnapshot` と実行中の XID に `db_oid` を持たせて `oldest_xmin_for_db` を作る（+1〜2 日。LK-3 の変更。共有カタログは全体のまま） |
| ★M5-VC-Q2 | 統計と凍結境界は**その場の上書き**（`HEAP2 INPLACE`）。00 §3.3 の予約枠（0x30）を使う（VC-D3） | ANALYZE がブロック内で動き、VACUUM が XID を持たない | 通常の UPDATE にするには、VACUUM が XID を取る（horizon を自分で止めないための工夫が要る）、ANALYZE を 25001 にする、カタログのキャッシュの無効化が増える |
| M5-VC-Q3 | `ANALYZE`（VACUUM なし）は**ブロックの中で動く**（00 §5.5 と M4 D-11 は 25001 としていた。VC-D15） | PG17 の `vacuum()` と同じ（未検証 1） | 25001 にすれば 00 のまま。ユーザーに見える差は、ブロック内の ANALYZE が失敗すること |
| M5-VC-Q4 | **clog は、接続できる全データベース（template1・postgres を含む）の全テーブルが VACUUM されるまで切り詰められない**。template0 は番兵で免除（VC-D13）。`ALLOW_CONNECTIONS false` で作ったデータベースは、コピー元の値のまま永久に切り詰めを止める。autovacuum が既定で無効なので、`VACUUM` を各データベースで手動で実行する必要がある | 64 ビット XID なので周回はなく、clog が伸びるだけ（1 億 XID で約 25MB） | 全データベースを自動で回すのは autovacuum（既定は M6 で有効化）。`datallowconn = false` の扱いを変えるなら、内部の VACUUM の仕組み（autovacuum のワーカーと同じ）を非接続のデータベースにも使う（+1 日） |
| M5-VC-Q5 | **起動時に、0 バイトで `pg_class` に行がない主フォークのファイルを消す**（M2-Q24。VC-D19）。0 バイトでない孤児は WARNING だけ（00 D31 と同じ） | 起動時は実行中のトランザクションがなく安全。M3 D15 の「消さない」を 0 バイトに限って緩める | 消さなければ、残骸は 0 バイトのファイルとして残るだけ（OID の採番が避ける）。大きい孤児を消すなら、カタログの照合の確かさを上げてからにする（M6） |
| M5-VC-Q6 | FSM は 2 段で、**1 リレーションあたり約 508 GiB まで**。超えた部分は FSM を使わず末尾に追加する（VC-D8） | 実装が小さい | 3 段にすれば PG と同じ容量（+1 日。ディスク形式を変えるので initdb のやり直し） |
| M5-VC-Q7 | autovacuum は実装するが**既定は無効**（D17）。更新件数の統計は**メモリ上だけで、再起動で消える**。再起動の直後は、`autovacuum_freeze_max_age` の条件以外では対象にならない（VC-D20） | 並行試験の再現性。統計の永続化は M6 | 統計を永続化するには、定期的にファイルに書く（+1〜2 日）。有効にして soak 試験をしてから既定を変える（M6） |
| M5-VC-Q8 | VACUUM はテーブルごとにコミットし、途中のエラーでも終わったテーブルの結果は残る（00 §5.5。PG と同じ） | | — |
| M5-VC-Q9 | `VACUUM FULL`・可視性マップ・HOT・インデックスのページ削除・コストベースの遅延は M6（D15）。`VACUUM FULL` は `0A000` | 範囲 | VACUUM FULL は新しい relfilenode へのコピー + インデックスの作り直し（AccessExclusive）で +3〜5 日 |
| M5-VC-Q10 | **凍結は常に horizon まで**（VC-D5）。最初の VACUUM では、行の入っている全ページが書き換わり、チェックポイントの後なら全ページの画像が WAL に載る（大きいテーブルの最初の VACUUM の WAL ≒ テーブルの大きさ） | clog の切り詰めがなるべく進む（00 §3.6） | `vacuum_freeze_min_age` 相当（cutoff を horizon より XID の数だけ古くする）にすれば、更新の多い行の凍結を省ける。clog の切り詰めがその分遅れるだけ（+0.5 日） |
| ★M5-VC-Q11 | **行ポインタ（TID）の再利用が M5 で始まる**（M2 §3.4 の「再利用しない」を覆す）。`ctid` を行の識別に使うアプリは、VACUUM の後に別の行の `ctid` と重なりうる（PG と同じ） | 領域の回収に必要。安全性は 5.3.5 | — |
| M5-VC-Q12 | VERBOSE の出力は PG の形に似せるが、**行の並びと数字の細部は PG と同じにしない**（64 ビットの XID を出す、`index` の削除ページの数は 0）。重大度は INFO | 共有テストで比較しない | — |
| M5-VC-Q13 | **権限の検査をしない**（全ロールが全テーブルを VACUUM / ANALYZE / TRUNCATE できる）。GRANT は M6 | M5 の範囲 | M6 で `vacuum_is_permitted` に所有者・`MAINTAIN` の検査を入れる |
| M5-VC-Q14 | cleanup lock を作らない（VC-D1）。正しさは 5.3.5 の証明と「ページの中身への参照をラッチの外へ持ち出さない」規約に依存する | 待ちの仕組みが要らず、VACUUM が飢えない | 将来、ラッチの外へ参照を持ち出す実装（例: ピンを持ったままタプルを参照し続けるスキャン）を入れるなら、`cleanup_waiter`（予約済み）を使う cleanup lock が要る |
| M5-VC-Q15 | `TRUNCATE ... CASCADE` は `0A000`（VC-D17） | FK の CASCADE は FK 章の範囲外 | PG と同じにするには、参照元のテーブルを再帰的に集める（+0.5 日。FK-4 と同時） |
| M5-VC-Q16 | VACUUM は大きなテーブルを走査するとバッファプールを使い切る（PG の VACUUM 用のリングバッファがない） | M5 の範囲 | リングバッファは M6（`BUFFER_USAGE_LIMIT` は受け付けて無視する） |

---

## 11. 契約への変更依頼

00（および M3 / M4 の契約）と食い違う、または 00 の表に行がないもの。**章の中で黙って変えない**。依頼先の章または F0 に渡す。

| # | 対象 | 変更 | 理由 | 依頼先 |
|---|---|---|---|---|
| C1 | 00 §3.3 の WAL の表 | `HEAP2` の `0x30` を **`INPLACE`**（その場の上書き）として VC が使う。`0x20 MULTI_INSERT` は予約のまま | VC-D3。ANALYZE がブロック内で動く・VACUUM が XID を持たない | F0（`wal2.rs` の定数と `recovery::dispatch`） |
| C2 | 00 §4.5 の `TableStore` | `inplace_update(&self, rel, tid, expect_xmin, patches) -> Result<InplaceOutcome>` と `InplacePatch`・`InplaceOutcome` を足す | 同上。カタログの `CatalogStore` が `dyn TableStore` を使うため | F0（トレイト）、RW（`HeapStore` の実装は VC） |
| **C3** | 00 D12・§4.3 の `TxnManager::snapshot` | **カタログの走査（`StatementCatalog`）が持つスナップショットは `take_snapshot` で登録する**。登録しない `snapshot()` は、スキャンが残らない一瞬の用途に限る | VC-D23。登録しないと、走査中のカタログの行を同時の VACUUM が除去しうる（5.3.5 の P4）。ページごとの走査（`HeapScan`）は 1 ページをコピーするが、ページをまたぐ間に行が消える | LK（LK-3、LK-4）、F0 |
| C4 | 00 §5.5・§4.7・M4 D-11 | `ANALYZE`（VACUUM なし）は**トランザクションブロックの中でも動き、外側のトランザクションで実行する**（`ddl::execute` の `BoundDdl::Analyze`）。`execute_standalone` が通すのは `Vacuum`（`ANALYZE` オプション付きを含む）・`CreateDatabase`・`DropDatabase` | VC-D15 | **RW（`session/txn_ctl.rs` は RW の持ち物。00 §8.1。レビュー対応 R-38 で宛先を LK から直した）**、F0（`ddl/mod.rs` の振り分け） |
| C5 | 00 §4.2・§4.3 | (1) `LockManager::has_waiters(id: BackendId, tag: LockTag) -> bool`、(2) `ProcArray` が固有メソッド `is_in_progress`・`next_xid`・`oldest_xmin`・`oldest_xmin_hint` を持つ（**`RunningXids` の `impl` は VC-1 が `storage/heap/prune.rs` に書く。LK は固有メソッドだけ。01 §11-17 と一致。R-11**）。`oldest_xmin_hint` は `oldest_xmin` の下限の写し（単調に増え、ロックなしで読める）、(3) **共有リレーションの `LockTag::Relation` は `db = 0`**、(4) スナップショットの取得と登録を `proc` の Mutex の中で一度に行う（5.3.5 の P3） | autovacuum の中断、機会的 pruning、共有カタログの VACUUM の排他 | LK |
| C6 | 00 §4.3 の `Clog` | `Clog::open(vfs, next_xid, oldest)`、`oldest()`、`status` の `xid < oldest` の `XX001`、`sweep_old_segments`、`set_status_redo` の無視（6.5） | VC-D14 | LK（ロックなし化と同時） |
| C7 | 00 D29 の `DebugKnobs` | **6 項目**: `vacuum_skip_index_pass`、`freeze_skip_wal`、`truncate_clog_before_control_fsync`、`relfrozenxid_too_high`、`fsm_trust_corrupt`、`clog_truncate_ignore_floor`（7.5。既定はすべて無効）。`vacuum_ignore_horizon` は作らず、LK の `vacuum_ignore_registered_snapshots`（01 §6.7。10 章 #2）を使う（R-09） | 変異テスト。名前は 10 章 §6.9 に合わせた | F0 |
| C8 | 00 §8 のファイルの持ち主 | VC が持つ: `sql/parser/vacuum.rs`、`storage/heap/{inplace,modstat}.rs`、`catalog/store_vac.rs`、`vacuum/{stats,orphan}.rs`、`scan.rs` と `heap_store.rs` の機会的 pruning・`modstat`・`inplace_update` の箇所、`yuzhu-server/src/config.rs` の autovacuum の 5 キー。**F0 が足す**: `ddl/table.rs` の `insert_relxid` / `delete_relxid` の呼び出し、`bootstrap.rs` / `rows.rs` の初期行（3.4）、`Session` の文の終わりの `cluster.mod_stats().absorb(modstat::drain())`、`ClusterOptions` の `autovacuum`・`vacuum_truncate_lock_timeout` | 00 の表に行がない | F0 |
| C9 | RW（02）への依頼（00 §8 の RW の持ち物） | 4.4 の表: `invalidate_xmax` / `with_xmin_frozen`（`decode_xmax` と `lockers_all_finished` は 02 §4.1 にある。`classify_xmax` は使わない。レビュー対応 R-01）、`visibility.rs` の `XMIN_FROZEN`、`HeapTuple.xmin` が凍結済みなら `Xid::FROZEN`、`delete` / `update` / REDO の `Page::set_prunable`、`heap/wal.rs` の INSERT / UPDATE の REDO の `add_item_at` 化、TID の読み直しで「範囲外・`LP_NORMAL` でない」を消えたものとして扱う、`ctid` の連鎖の `xmin == 直前の xmax` の検査 | 行ポインタの再利用と凍結 | RW |
| C10 | 00 §3.6 の設定 | `autovacuum_freeze_max_age`（既定 200,000,000）を足す。`vacuum_*`・PG にあって実装しない `autovacuum_*` は `INERT_GUCS` | VC-D20 | F0 |
| C11 | 00 D51・09 章 | `yz_datxid` の行の値: initdb は template1・postgres を 3、**template0 を番兵 `i64::MAX`**（`DATFROZEN_PRISTINE`）。`CREATE DATABASE` は新しい行を `min(コピー元の値, TxnManager::oldest_xmin())` で作る。`DROP DATABASE` が行を削除する。行の読み書きは `SharedCatalogStore` の `insert_datxid` / `delete_datxid`（4.3） | VC-D13 | DB |
| C12 | M2 §3.4・§6.4、M3 §6.4.4（既存のテスト） | `Page::add_item` が `LP_UNUSED` を再利用する、`free_space` の意味、`item_id` の正準形の検査、ヒープの INSERT / UPDATE の REDO が `add_item_at` を使う。M2 の「一度使った番号は再利用しない」を前提にした単体テスト（`add_item` の番号）は書き直す | VC-D10 | VC（`page.rs`）、D / RW（`wal.rs` の REDO） |
| C13 | 00 §5.4 の起動 | `Clog::open` に `oldest_xid` を渡して古いセグメントを消す、リカバリの後に `sweep_orphans`（5.10.3）、チェックポイントレコードの `oldest_xid` に制御ファイルの値を入れる | VC-D14、VC-D19 | F0（`engine.rs`・`checkpoint.rs`・`recovery.rs`） |
