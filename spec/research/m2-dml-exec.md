# M2 調査: UPDATE / DELETE と、executor・storage 間のインタフェース変更

対象: yuzhu M2（永続化）。M1 の契約は `spec/design/m1.md`。この文書は M2 基本設計の材料になる調査と推奨案です。

- 根拠にした PostgreSQL のソースは REL_17_STABLE。URL は `https://github.com/postgres/postgres/blob/REL_17_STABLE/<path>` の形で示します。
- 行番号は 2026-10 時点の REL_17_STABLE を取得して確認しました。今後ずれることがあります。
- **［未検証］** を付けた記述は、ソースや実機で確認していない推測です。

---

## 0. 結論（推奨案の要約）

1. **ROLLBACK と文の原子性は、M2 の時点で xmin/xmax と永続的なコミットログ（clog）を使う「可視性方式」で実現する**（案 B）。undo ログをヒープページに当てる方式（案 A）は採らない。案 A は M3 でほぼ全部捨てることになり、UPDATE の前イメージをメモリに溜めるので大きな UPDATE に耐えない。
2. PostgreSQL では、文が失敗するとトランザクション全体が中断（Failed）されます。セーブポイントがない限り、「文の原子性」は「トランザクションの原子性」と同じことになります。そのため案 B ならサブトランザクションは不要です。M1 の `statement_start` による部分 undo は廃止します。
3. **書き込みは単一ライターロック**で直列化する（M3 の「単一ライター + 複数リーダー」を前倒しする）。ロックは DML/DDL 文の開始時、**スナップショットを取る前に**取得し、トランザクションの終了まで持ちます。これで M2 では EvalPlanQual（並行更新の再評価）が要りません。
4. **Halloween 問題はコマンド ID（cmin/cmax と snapshot.curcid）で解く**。PostgreSQL の `HeapTupleSatisfiesMVCC` と同じ判定を移植します。`INSERT INTO t SELECT * FROM t` も同じ仕組みで正しくなります。
5. **スキャンカーソルは「ページ単位で可視タプルをデコードして `VecDeque` に溜め、ラッチもピンも `next()` をまたいで持たない」**。カーソルの状態は所有権を持つただのデータ（ライフタイムなし）とし、`next()` に `&dyn TableStore` を毎回渡します。
6. 実行ノードは `Update` と `Delete` を別々に作る（PostgreSQL の ModifyTable に相当）。入力行の末尾に `ctid`（`Datum::Tid`）を載せます。SET 式はすべて**旧行**に対して評価し、新行全体で NOT NULL と**全** CHECK を検査してから `storage.update()` を呼びます。
7. **M2 のクラッシュ時の保証は「正常停止した場合だけ」**。pg_control 相当の制御ファイルに `state` を持たせます。起動時に `IN_PRODUCTION` のままだったら、「正常に停止されていない」というエラーで起動を拒否します（強制起動のオプションは用意する）。COMMIT は fsync しません。
8. テスト: slt に UPDATE/DELETE と ROLLBACK を追加します。永続化は「フェーズ分割した slt + 再起動ハーネス」で試し、本物の PostgreSQL（コンテナの再起動）でも同じテストを流します。Rust 側では、可視性判定の真理値表、障害注入、モデルとの突き合わせテストを書きます。

| 項目 | 案 A（undo ログ） | 案 B（xmin/xmax + clog）推奨 |
|---|---|---|
| M2 の工数感 | 4〜6 人日 | 6〜8 人日 |
| M3 での手戻り | 大きい（5〜8 人日。undo 系をすべて捨て、DML とスキャンを書き直す） | 小さい（1〜2 人日。WAL レコードを足すだけで、可視性まわりは変えない） |
| 合計 | 9〜14 人日 | 7〜10 人日 |
| ダーティリード | 残る | M2 で解消（他のセッションには未コミットの行が見えない） |
| 大きな UPDATE | 前イメージをメモリに溜める（上限なし） | 追加のメモリは要らない |
| 領域の回収 | 物理削除なので回収しやすい | VACUUM（M5）まで回収しない（膨張する） |

工数はどれも粗い見積もり（1 人日 ≒ AI 実装エージェント 1 本が集中して進める 1 日分）です。

---

## 1. 前提の整理

### 1.1 M1 の現状（m1.md 3.4、5.3）

- `TableStore::scan()` は全件コピーを返す。`INSERT ... SELECT` で同じテーブルを読んでも壊れないのは、このコピーのおかげ。
- `Transaction { undo, statement_start }`。`UndoEntry` は `Inserted` / `CreatedTable` / `DroppedTable { def, rows }`。
- 文の実行は `Database` の 1 本の Mutex で直列化している。他のセッションからは未コミットの行が見える（ダーティリード）。

### 1.2 M2 で固定する前提（今回の依頼の条件）

- 8KB ページ、リレーションごとのファイル（1GB セグメント）、複数データベースを置けるディレクトリ構成。
- タプルヘッダには M2 から xmin / xmax / cid / ctid / infomask を持たせ、M3 で形式を変えない。
- ページヘッダには M2 から LSN を持たせる（M2 では常に 0）。
- WAL、full page write、チェックポイントは M3。
- ファイル I/O は抽象化レイヤ経由で、障害注入ができるようにする。

### 1.3 PostgreSQL の意味論で効いてくる点

- **トランザクションブロック内で文が失敗すると、ブロック全体が Failed になる**。以降は ROLLBACK まで 25P02 を返す（m1.md 4.1）。暗黙トランザクションでは、Query メッセージ全体を巻き戻す。
  → セーブポイント（SAVEPOINT / ROLLBACK TO）がない限り、「失敗した文だけを巻き戻し、トランザクションは続ける」場面はありません。SAVEPOINT は M5 までの範囲に入っていません。
- 読み取り専用のトランザクションには XID を割り当てない。最初の書き込みで遅延割り当てする（`GetNewTransactionId`、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/varsup.c>）。
- コマンド ID は、そのコマンドが実際に書き込んだときだけ進める（`CommandCounterIncrement` は `currentCommandIdUsed` を見る。<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/xact.c>）。

---

## 2. ROLLBACK と文の原子性: 案の比較

### 2.1 案 A: undo ログをヒープページに当てる（M1 方式の延長）

- `UndoEntry` に `Inserted { rel, tid }`、`Deleted { rel, tid }`、`Updated { rel, old_tid, new_tid }` を追加する。
- DELETE は、タプルをその場で物理削除できない。undo で元に戻すとき、同じ TID を他の挿入に再利用されると困るからです。そのため「削除予定」の印を付けておき、コミット時に本当に削除する。**結局、xmax の簡易版を作ることになります。**
- UPDATE は「旧版に削除予定の印を付ける + 新版を挿入する」。旧版をその場で上書きする方式にすると、前イメージ（旧タプルのバイト列）を undo に持つ必要があり、`UPDATE big SET x = x + 1` でテーブル全体の大きさのメモリを使う。
- Halloween 問題は「この文で挿入した TID の集合」を `HashSet` で持ち、スキャンで飛ばして解く（行数に比例するメモリが要る）。または、対象の TID を先に全部集めてから更新する。
- DDL のロールバック（DROP TABLE）には、ファイルをコミットまで消さずに残す仕組みが別に要る。
- **ダーティリードは残る。**
- **M3 での手戻り**: MVCC にするとき、undo の当て込み、削除予定の印、Halloween 用の集合、スキャン側の判定をすべて捨てます。DELETE / UPDATE / スキャン / ROLLBACK / 終了処理を書き直すことになります。ダーティリードを前提にしたテストがあれば、それも直します。

### 2.2 案 B: xmin/xmax + clog で可視性を判定する（推奨）

- タプルヘッダの xmin / xmax / cid を M2 から正しく埋める。トランザクションの状態は clog（2 ビット × XID）に記録する。
- **ROLLBACK は clog に「中断」と書くだけ**。ページには触りません。中断されたトランザクションのタプルは、xmin が中断済みなので見えなくなります。中断されたトランザクションの xmax は無視されます。
- 文の失敗は、PostgreSQL の意味論ではトランザクションの中断と同じ（1.3 節）なので、同じ仕組みで扱える。
- DDL もカタログの行（pg_class など）の xmin/xmax でロールバックされる。ファイルの作成と削除だけは、PostgreSQL の pending deletes と同じ仕組みで、トランザクションの終了時に処理する（4.5 節）。
- Halloween 問題はコマンド ID で解く（第 5 節）。
- 他のセッションには未コミットの行が見えない。M1 のダーティリードが M2 で解消します。
- **steal（未コミットの変更を含むページの追い出し）を許してよい**。再起動後も、clog を見れば未コミットのタプルを見えなくできます。PostgreSQL が UNDO なしの REDO-only で成り立っているのは、まさにこの性質のおかげです（`research-rust-db-arch.md` 4.2 の最後の項目と同じ結論）。
- **M3 での手戻り**: 可視性判定、スナップショット、DML の経路はそのまま使えます。足すのは、ヒープ操作と clog 更新の WAL レコード、ページ LSN の更新、REDO、制御ファイルの「正常停止でなければ起動拒否」をリカバリに置き換える部分です。

### 2.3 案 B に必要な部品（M2 で作るもの）

| 部品 | 内容 | 工数感 |
|---|---|---|
| XID の採番 | `next_xid: u32`（3 から。0 = Invalid、1 = Bootstrap、2 = Frozen。`transam.h` と同じ）。制御ファイルに上限を先取りして書く（4.4 節） | 0.5 人日 |
| clog | `pg_xact` と同じく 2 ビット × XID。8KB ページ単位のファイルで、バッファは小さな専用キャッシュ。ファイル I/O は抽象化レイヤ経由 | 1〜1.5 人日 |
| スナップショット | `Snapshot { xmin, xmax, xip: Vec<Xid>, curcid }`。単一ライターなので `xip` は高々 1 要素 | 0.5 人日 |
| 可視性判定 | `HeapTupleSatisfiesMVCC` の簡易移植（5.2 節）と真理値表のテスト | 1 人日 |
| combo CID | cmin と cmax を 1 つの `t_cid` に入れるための、トランザクションごとのメモリ上の表（`combocid.c`） | 0.5 人日 |
| 単一ライターロック | Mutex + Condvar。`lock_timeout` に対応 | 0.5 人日 |
| pending な作成・削除 | ファイルの作成と削除をトランザクションの終了時に処理する | 0.5 人日 |
| heap の delete / update | xmax・cmax・ctid の設定、新版の挿入、`TmResult` | 1〜1.5 人日 |

参照:
- `HeapTupleSatisfiesMVCC`: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/heapam_visibility.c>（1014 行付近の `if (HeapTupleHeaderGetCmin(tuple) >= snapshot->curcid) return false; /* inserted after scan started */`）
- clog: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/clog.c>
- combo CID: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/time/combocid.c>
- XID の特別な値: <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/transam.h>（31〜35 行）

### 2.4 推奨とその理由

**案 B を推奨します。**

- 合計の工数が小さい（M3 で捨てるコードがほぼない）。
- M3 で入れるのは WAL の部分だけになり、M3 の難しさ（REDO、full page write、チェックポイント）に集中できる。
- 「ROLLBACK 後に再起動しても行が戻ってこない」という永続化の正しさを、M2 のうちにテストで固められる（clog を永続化するから）。
- 案 A の弱点（前イメージのメモリ、TID の再利用、ダーティリード）が最初から出ない。

案 B の弱点と対策:
- **膨張**: 削除・更新された版と、中断された挿入は、VACUUM（M5）までページに残る。M2〜M4 の既知の制約として明記する。テストでは問題にならない大きさです。
- **XID の周回**: 32 ビット XID が 2^31 に近づいたら書き込みを拒否する（PostgreSQL の `xidStopLimit` 相当。メッセージは別途決める）。凍結（freeze）は VACUUM と一緒に M5 で入れる。
- **ヒントビット**: `HEAP_XMIN_COMMITTED` などのビットは定義しておくが、**M2 では設定しない**（毎回 clog を引く。clog はメモリ上にキャッシュする）。ヒントビットを WAL なしで書くことと、M3 のチェックサムや full page write との関係は、M3 で決める。

---

## 3. 並行性: 単一ライターロック

### 3.1 問題

案 B では、セッション 1 が BEGIN して行を UPDATE し、まだコミットしていない状態で、セッション 2 が同じ行を UPDATE しようとします。PostgreSQL なら、セッション 2 は行ロックを待ちます。待ちが解けたあと、Read Committed では EvalPlanQual で新しい版を評価し直します（`nodeModifyTable.c` の `TM_Updated` の処理）。これを M2 で実装するのは重い作業です。

### 3.2 推奨: トランザクション単位の単一ライターロック

- DML（INSERT/UPDATE/DELETE）と DDL を実行する文は、**文の開始時、スナップショットを取る前に**ライターロックを取る。ロックはトランザクションの終了（COMMIT/ROLLBACK、または暗黙トランザクションの終了）まで持つ。
- ロックを持っている間、他のトランザクションは書き込めない。したがって、スナップショットを取ってから自分が更新するまでの間に、他者の「コミット済みの更新」が割り込むことはなく、`TM_Updated` / `TM_BeingModified` は起きない。M2 の `heap_update` / `heap_delete` で返りうるのは `Ok` / `SelfModified` / `Invisible` だけになる（`Invisible` は不変条件の違反として内部エラーにする）。
- 待つのはロックを持っていないトランザクションだけで、ロックは 1 つしかないので**デッドロックは起きない**。
- `lock_timeout`（m1.md 5.4 で設定項目はすでにある）が 0 でなければ、時間切れで `55P03 lock_not_available`（`canceling statement due to lock timeout`）を返す。
- **M1 の文単位の Mutex との順序**: M2 でも文単位の Mutex を残す場合、**ライターロック → 文単位の Mutex** の順で取る。逆にすると、文単位の Mutex を持ったままライターロックを待ち、ロックを持つセッションの COMMIT が文単位の Mutex を取れず、デッドロックします。M2 で文単位の Mutex を外すかどうかは、バッファプールとカタログキャッシュがスレッドセーフになっているかで決める。外すなら M3 の「複数リーダー」を先に実現できる。
- Repeatable Read（M5）と複数ライター（M5）では、このロックを外し、タプル単位のロック、`TM_Updated` の処理、40001 を入れる。**M2 の `heap_update` の戻り値は PostgreSQL の `TM_Result` と同じ列挙にしておく**（M5 で呼び出し側を変えずに済む）。参照: `TM_Result` は <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/tableam.h>。

---

## 4. storage / txn のインタフェース（M1 の `TableStore` を置き換える）

### 4.1 型

```rust
/// ItemPointerData と同じく (ブロック番号, 行ポインタ番号)。offset は 1 始まり
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Tid { pub block: u32, pub offset: u16 }
pub type RowId = Tid;               // m1.md の「M2 で TID に置き換わる」

pub type Xid = u32;                 // TransactionId
pub type CommandId = u32;

/// PostgreSQL 16 以降の RelFileLocator と同じ 3 つ組
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RelFileLocator { pub spc_oid: Oid, pub db_oid: Oid, pub rel_number: Oid }

/// スキャンや更新に必要な、リレーションの情報一式（文の開始時に作る）
#[derive(Clone)]
pub struct RelHandle { pub locator: RelFileLocator, pub desc: Arc<TupleDesc> }

pub struct Snapshot {
    pub xmin: Xid,          // これより小さい XID は終了済み
    pub xmax: Xid,          // これ以上の XID は未来（見えない）
    pub xip: Vec<Xid>,      // 取得時点で実行中だった XID
    pub curcid: CommandId,  // 自トランザクションの、この cid 以降の変更は見えない
    pub own_xid: Option<Xid>,
}

/// 書き込みに必要な情報。xid は割り当て済みでなければならない
pub struct WriteCtx { pub xid: Xid, pub cid: CommandId }

/// PostgreSQL の TM_Result と同じ列挙
pub enum TmResult { Ok, Invisible, SelfModified, Updated, Deleted, BeingModified }
```

`TupleDesc` は列の型（`SqlType`）、`attlen`、`attalign`、NOT NULL の情報を持ち、タプルのエンコードとデコードに使う。タプル形式そのものは、別の調査（タプル・ページ形式）に従う。

### 4.2 `TableStore`

```rust
pub trait TableStore: Send + Sync {
    fn create_storage(&self, rel: RelFileLocator) -> Result<()>;
    fn unlink_storage(&self, rel: RelFileLocator) -> Result<()>;

    fn insert(&self, rel: &RelHandle, w: &WriteCtx, row: &Row) -> Result<Tid>;
    fn delete(&self, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid) -> Result<TmResult>;
    /// 旧版に xmax/cmax/ctid を設定し、新版を挿入する。Ok なら新版の TID も返す
    fn update(&self, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid, new_row: &Row)
        -> Result<(TmResult, Option<Tid>)>;

    fn begin_scan(&self, rel: &RelHandle, snap: Snapshot) -> Result<HeapScan>;
    fn scan_next(&self, scan: &mut HeapScan) -> Result<Option<(Tid, Row)>>;

    /// TID を指定して 1 行読む（M4 のインデックススキャン用。M2 ではテストだけで使う）
    fn fetch(&self, rel: &RelHandle, snap: &Snapshot, tid: Tid) -> Result<Option<Row>>;
}
```

- M1 の `delete_row`（undo 用）と、全件コピーを返す `scan` は廃止する。
- `update` を 1 回の呼び出しにまとめておくのは、M3 でここを「両方のページを番号順にラッチして WAL レコードを 1 つ書く」形に変えるため（PostgreSQL の `heap_update` は、旧ページと新ページのロックをブロック番号の順で取り、`XLOG_HEAP_UPDATE` を 1 つ書く。<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/heapam.c> 3348 行付近、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/hio.c> の `RelationGetBufferForTuple`）。M2 の内部実装は、「新版を挿入（新ページだけをラッチ）→ 旧ページをラッチして xmax・cmax・ctid を設定」の 2 段階でよい。クラッシュの保証がない（第 7 節）ので順序は問いません。
- 空き領域の探し方: M2 には FSM（空き領域マップ）がない。UPDATE の新版は、**旧版と同じページに入ればそこへ**、入らなければ最後のページへ、それも満杯ならリレーションを伸ばす。INSERT は最後のページか、伸ばす。HOT（同じページ内の更新を特別扱いする最適化）は入れない。ただし、ヘッダのビット（`HEAP_HOT_UPDATED`、`HEAP_ONLY_TUPLE`）の位置は予約しておく。
- 1 ページに収まらないタプルは `54000 program_limit_exceeded`、`row is too big: size %zu, maximum size %zu`（hio.c 535 行付近と同じ文言）。TOAST は範囲外。
- M1 のメモリ実装（`storage/memory.rs`）は廃止する。単体テストは、**ファイル抽象化レイヤのメモリ上の実装**（障害注入にも使うもの）の上でヒープを動かせば速い。

### 4.3 `Transaction`（M1 の undo ログ方式を置き換える）

```rust
pub struct Transaction {
    pub xid: Option<Xid>,              // 最初の書き込みで割り当てる
    pub cid: CommandId,                // 現在のコマンド ID（0 から）
    pub cid_used: bool,                // このコマンドが書き込んだか（CommandCounterIncrement の判定）
    pub combo_cids: ComboCidMap,       // (cmin, cmax) → combo id
    pub pending_creates: Vec<RelFileLocator>, // 中断したら unlink
    pub pending_unlinks: Vec<RelFileLocator>, // コミットしたら unlink
    pub writer_lock: Option<WriterLockGuard>, // RAII。Drop で解放
    pub catalog_dirty: bool,           // DDL をしたか（コミット時にカタログキャッシュを無効化する）
}
```

- `UndoEntry`、`statement_start`、`take_statement_undo` は削除する。
- 文の終わり（成功時）: `cid_used` なら `cid += 1`（`CommandCounterIncrement`）。同じトランザクションの次の文から、自分の変更が見えるようになる。
- コミット: `xid` があれば、clog に COMMITTED を書く → `pending_unlinks` を処理 → カタログキャッシュを無効化（`catalog_dirty` なら）→ ライターロックを解放。
- 中断: `xid` があれば、clog に ABORTED を書く（書かなくても、「実行中でなく、コミットされていない」XID は中断扱いになる。ただし明示的に書く）→ `pending_creates` を unlink → キャッシュを破棄 → ロックを解放。
- **XID は再利用してはいけない**。再起動後に同じ XID を使うと、クラッシュ前に中断扱いになっていたタプルが、新しいトランザクションのコミットで見えるようになってしまう。対策は 4.4 節。

### 4.4 XID と OID の採番の永続化（WAL なしで）

PostgreSQL は、OID を `VAR_OID_PREFETCH`（8192）個ずつ先取りして WAL に記録し、XID はチェックポイントと WAL から復元する（<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/varsup.c> 31 行、602 行）。M2 には WAL がないので、次のようにする。

- 制御ファイルに `next_xid_limit` と `next_oid_limit` を持たせる。上限に達したら、上限を例えば +1024 して、**制御ファイルを書いて fsync してから**採番を続ける。
- 起動時は `next_xid = next_xid_limit` から再開する（先取りした分は捨てる。PostgreSQL の OID と同じ考え方）。
- clog は `next_xid` を含むページまで伸ばしておく（PostgreSQL の `ExtendCLOG`）。

### 4.5 ファイルの作成と削除をトランザクションに従わせる

PostgreSQL の `RelationCreateStorage` / `RelationDropStorage` / `smgrDoPendingDeletes(isCommit)` と同じ方式にする（<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/catalog/storage.c> 121、206、657 行）。

- CREATE TABLE: ファイルをすぐ作り、`pending_creates` に積む。中断したら unlink する。
- DROP TABLE: カタログの行に xmax を付けるだけにして、ファイルは `pending_unlinks` に積む。コミット時に unlink する。
- M1 の `DroppedTable { def, rows }`（行を全部コピーして持つ）は不要になる。
- クラッシュ時に、作りかけのファイルや消し損ねたファイルが残りうる（孤児ファイル）。M2 では保証の範囲外とする（第 7 節）。PostgreSQL も孤児ファイルを自動では消さない。

---

## 5. Halloween 問題とコマンド ID

### 5.1 問題

`UPDATE t SET a = a + 1` をシーケンシャルスキャンで実行すると、新しい版がスキャンの先（後ろのページや、同じページの空き）に書かれ、それをまたスキャンが拾って更新し続けるおそれがある。`INSERT INTO t SELECT * FROM t` も同じ。M1 は全件コピーで避けていた。

### 5.2 解き方（PostgreSQL と同じ）

- 文ごとに `snapshot.curcid = txn.cid` とする。書き込むタプルには `cmin = txn.cid`（削除なら `cmax = txn.cid`）を入れる。
- 可視性判定は、自トランザクションが挿入したタプルについて **`cmin >= curcid` なら見えない**とする。したがって、この文が書いた新版は、この文のスキャンからは見えない。
- 自トランザクションが削除したタプルは、`cmax >= curcid` なら**まだ見える**（この文、またはそれより後のコマンドが削除したもの）。スナップショットの意味論としてはこれが正しい。

M2 の可視性判定（`HeapTupleSatisfiesMVCC` から、ロックだけの xmax、MultiXact、サブトランザクション、ヒントビットを除いた簡易版）:

```text
fn xid_visible_in_snapshot(x, snap):            // x がスナップショット時点でコミット済みか
    if x == FROZEN || x == BOOTSTRAP: return true
    if x >= snap.xmax || x in snap.xip:  return false
    return clog(x) == COMMITTED

fn tuple_visible(t, snap):
    if Some(t.xmin) == snap.own_xid:
        if t.cmin >= snap.curcid: return false           // この文以降の挿入
        if t.xmax == INVALID:     return true
        if Some(t.xmax) == snap.own_xid: return t.cmax >= snap.curcid
        return true                                       // M2 では起きない（単一ライター）
    if !xid_visible_in_snapshot(t.xmin, snap): return false
    if t.xmax == INVALID:                       return true
    if Some(t.xmax) == snap.own_xid:            return t.cmax >= snap.curcid
    if !xid_visible_in_snapshot(t.xmax, snap):  return true   // 削除者が未コミット・中断・未来
    return false
```

- clog に IN_PROGRESS と記録されていても、スナップショットの `xip` に入っておらず `xmax` より小さい XID は、クラッシュしたトランザクションなので中断扱いになる。上の判定は「`clog == COMMITTED` のときだけ見える」と書いてあるので、自然にそうなる。
- `cmin` と `cmax` を両方持つ必要があるのは「同じトランザクションが挿入して、さらに削除した」タプルだけ。PostgreSQL はこのとき `t_cid` に combo CID を入れ、`HEAP_COMBOCID` ビットを立てる（<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/htup_details.h> 129、195 行）。yuzhu も同じにすると、ヘッダは PostgreSQL と同じ 23 バイトに収まる。combo CID の表はトランザクションの中でしか意味を持たないので、永続化はいらない。
  - combo CID を避けたい場合は、cmin と cmax を別々のフィールド（+4 バイト）にしてもよい。ただし、**M2 で決めたら M3 以降は変えない**。推奨は PostgreSQL と同じ combo CID（工数は 0.5 人日程度）。

### 5.3 スキャン開始時のブロック数

PostgreSQL は、スキャン開始時に `rs_nblocks = RelationGetNumberOfBlocks(...)` でブロック数を固定する（heapam.c 431 行）。yuzhu も `begin_scan` でブロック数を記録し、スキャン中に伸びた分は読まない。**ただしこれだけでは Halloween 問題は解けない**（既存ページの空きに新版が入るから）。本筋の対策は cid。ブロック数の固定は、無駄な読み取りを減らすだけです。

### 5.4 SelfModified

M2 の単一テーブルの UPDATE / DELETE では、同じタプルを同じコマンドで 2 回更新することはない（スキャンは各タプルを 1 回しか返さない）。M4 で `UPDATE ... FROM`（結合）を入れると起こりうる。PostgreSQL は、`cmax == 現在の cid` なら「自分がすでに更新した」として黙って飛ばす（<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/nodeModifyTable.c> 2402 行付近の `/* Else, already updated by self; nothing to do */`）。yuzhu の Update / Delete ノードも、`SelfModified` を受けたら飛ばす（件数に数えない）実装にしておく。

---

## 6. スキャンカーソルと実行ノード

### 6.1 ヒープスキャンのカーソル

```rust
pub struct HeapScan {
    rel: RelHandle,
    snap: Snapshot,
    nblocks: u32,                    // begin_scan 時点のブロック数（5.3 節）
    next_block: u32,
    buf: VecDeque<(Tid, Row)>,       // 現在のページから取り出した可視タプル
}
```

- `scan_next` は、`buf` が空なら `next_block` のページを `with_page(pid, |page| ...)` で開く（ピン → 共有ラッチ → クロージャ → ラッチ解放 → アンピン）。その中で行ポインタを順に見て、可視性を判定し、**見えるタプルをすべて `Row` にデコードして `buf` に積み**、クロージャから出る。
- **ラッチもピンも `next()` をまたいで持たない**。上位の Update ノードが同じページの排他ラッチを取っても、自分自身とデッドロックしない（`research-rust-db-arch.md` 4.2 の落とし穴）。ピンを持たないので、ピンが漏れてバッファプールが枯渇することもない。テストでは、文が終わるたびに `pool.pinned_count() == 0` を確かめる。
- PostgreSQL のページ単位モード（`heap_prepare_pagescan` で可視タプルのオフセットを `rs_vistuples` に集め、ロックを外してから、ピンだけを持って読む。heapam.c 608 行、1064 行の `heapgettup_pagemode`）とほぼ同じ形。yuzhu はピンも外し、デコード済みの行を持つ点が違う。メモリは 1 ページ分の行だけ。
- デコードしてから Update ノードが更新するまでの間に、ページの中身が変わることはありうる。ただし M2 では、変更できるのは自トランザクションだけ（単一ライター）。`update` / `delete` はラッチを取ったあとで xmax を検査し直して `TmResult` を返すので、正しさは storage の側で守られる。
- カーソルは、所有権を持つただのデータ。`SeqScan` ノードの中に持たせ、`next(&mut self, ctx)` で `ctx.storage.scan_next(&mut self.scan)` を呼ぶ。executor にライフタイムは要らない（m1.md 5.3 の方針を維持できる）。

### 6.2 `ExecCtx` の変更

```rust
pub struct ExecCtx<'a> {
    pub catalog: &'a dyn CatalogReader,
    pub storage: &'a dyn TableStore,
    pub txn_mgr: &'a TxnManager,     // XID の採番、clog
    pub txn: &'a mut Transaction,
    pub snapshot: &'a Snapshot,      // 文の開始時に取る（Read Committed）
    pub session: &'a SessionInfo,
}
impl ExecCtx<'_> {
    /// XID がなければ割り当てて、WriteCtx を返す
    pub fn write_ctx(&mut self) -> Result<WriteCtx>;
}
```

- **Session 側の手順（DML 文）**: (1) 書き込む文ならライターロックを取る（未保持の場合）→ (2) `CatalogReader` を作る → (3) スナップショットを取る（`curcid = txn.cid`）→ (4) analyze / plan / build → (5) `next()` のループ → (6) 成功したら `CommandCounterIncrement`。
- 失敗したら、暗黙トランザクションでもブロックでも中断する（ブロックなら Failed 状態にし、ROLLBACK が来た時点で中断処理を行う。clog への ABORTED の書き込みはその時点でよい）。

### 6.3 プランと実行ノード

```rust
PhysicalPlan::SeqScan { table_oid, columns, emit_tid: bool }
PhysicalPlan::Update {
    table_oid,
    input: Box<PhysicalPlan>,             // SeqScan(emit_tid) [+ Filter]。行の末尾が ctid
    assignments: Vec<(usize /*列番号*/, UpdateSource)>,
    not_null: Vec<usize>, checks: Vec<BoundCheck>,
}
enum UpdateSource { Expr(BoundExpr) /* 代入キャストと typmod の適用済み */, Default(Option<BoundExpr>) }
PhysicalPlan::Delete { table_oid, input: Box<PhysicalPlan> }
```

- **TID の運び方**: `Datum::Tid(Tid)` を追加し（型は `tid`、OID 27）、`SeqScan` が `emit_tid` のときだけ行の末尾に付ける。PostgreSQL が `ctid` をジャンク列として ModifyTable に渡すのと同じ（`nodeModifyTable.c` の `ExecModifyTable`）。
  - おまけとして、システム列 `ctid`（tid、OID 27）・`xmin`（xid、OID 28）・`xmax`・`cmin`（cid、OID 29）・`cmax` を SELECT できるようにしておくと、デバッグや Rust のテストに便利。`tid` の出力形式は `(0,1)`。ただし値が PostgreSQL と一致する保証はないので、**slt には入れない**。M2 で必須ではない。
- **Update ノード**（入力 1 行ごと）:
  1. 末尾の `Datum::Tid` を取り出す。旧行 = 残りの列。
  2. SET の各式を**旧行に対して**評価する（`SET a = b, b = a` で入れ替えになる。PostgreSQL と同じ）。`DEFAULT` は DEFAULT 式を評価し、なければ NULL。
  3. 新行 = 旧行のコピーに、評価した値を上書きしたもの。代入キャストと typmod（22001・22003）は、アナライザが `BoundExpr` に組み込み済み（m1.md 5.2 の INSERT と同じ経路）。
  4. **NOT NULL を全列について検査**し、続いて**CHECK 制約をすべて評価**する（SET で変えた列に関係しない CHECK も評価する）。メッセージは INSERT と同じで、`Failing row contains (...)` には**新行**を出す。PostgreSQL の `ExecConstraints`（<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/executor/execMain.c> 1975 行。2040 行の not-null、2092 行の check のメッセージ）は、UPDATE でも全部の not-null 列と全部の CHECK を見る。［未検証: PostgreSQL 17 には、変更されていない列の NOT NULL 検査を省略する最適化はないと読んだが、実装の細部は確認していない。結果は同じ］
  5. `storage.update(rel, w, snap, tid, &new_row)`。`Ok` なら件数を +1、`SelfModified` なら飛ばす、それ以外は内部エラー。
  6. 入力が尽きたら `None` を返す。Session は CommandComplete `UPDATE n` を送る。
- **Delete ノード**: 末尾の TID を取り出し、`storage.delete`。タグは `DELETE n`。
- 制約の検査は heap を書く**前**に行う（PostgreSQL も `ExecUpdateAct` の中で `ExecConstraints` を先に呼ぶ）。違反したら、それまでに更新した行も含めて、トランザクションの中断で消える（文の原子性）。
- RETURNING は M2 の範囲外。構文は受け付けて `0A000` を返す（M1 の方針と同じ）。

### 6.4 アナライザ（UPDATE / DELETE）

| 項目 | 挙動 |
|---|---|
| 構文 | `UPDATE [ONLY] t [[AS] alias] SET col = expr|DEFAULT [, ...] [WHERE cond]`、`DELETE FROM [ONLY] t [[AS] alias] [WHERE cond]` |
| 複数列の形 | `SET (a, b) = (e1, e2)` は M2 で対応してよい（任意）。`(a, b) = (SELECT ...)` はサブクエリなので M4 |
| `UPDATE ... FROM`、`DELETE ... USING`、`WHERE CURRENT OF` | 構文は受け付けて `0A000`（FROM/USING は M4、カーソルは範囲外） |
| 存在しない SET 列 | `42703`、`column "x" of relation "t" does not exist`（parse_target.c 1067 行。位置付き） |
| 同じ列への 2 回の代入 | `42601`、`multiple assignments to same column "a"`（PostgreSQL ではリライタが出す。<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/rewrite/rewriteHandler.c> 1146 行） |
| システム列への代入 | `0A000`、`cannot assign to system column "ctid"`（parse_target.c 480 行） |
| WHERE の型 | bool でなければ `42804`、`argument of WHERE must be type boolean, not type integer`。WHERE の結果が NULL の行は対象外 |
| SET の値 | 代入キャスト（m1.md 5.2）。キャストできなければ `42804`（`column "a" is of type integer but expression is of type boolean`） |
| `SET t.a = 1` | PostgreSQL は `t.a` を複合型のフィールド代入と解釈してエラーにする［未検証: エラーコードと文言は実機で確認していない］。M2 では 42703 を返し、slt に入れる前に PostgreSQL で確かめる |

---

## 7. M2 のクラッシュ時の挙動と保証

### 7.1 保証すること・しないこと（利用者向けに明記する）

- **保証すること**: 正常停止（SIGTERM / SIGINT による fast shutdown、または管理用 API での停止）のあとに起動すれば、停止前にコミットしたデータはすべて残っており、コミットしていないデータは残っていない。
- **保証しないこと**: プロセスの異常終了、kill -9、OS のクラッシュ、電源断のあとのデータ。COMMIT は fsync しない（M3 の WAL まで）。
- **異常終了を検出したら起動を拒否する**: 黙って壊れたデータを読ませないため。

### 7.2 制御ファイル（pg_control に相当）

PostgreSQL の `DBState`（<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/catalog/pg_control.h> 92、97 行の `DB_SHUTDOWNED`、`DB_IN_PRODUCTION` など）に倣う。

- `global/pg_control`（ファイル名は PostgreSQL に合わせる。中身の形式は yuzhu 独自）に、`state`（`Shutdowned` / `InProduction`）、`next_xid_limit`、`next_oid_limit`、`catalog_version`、`page_size`、`segment_size`、CRC を持たせる。M3 で `checkpoint_lsn` などを足せるよう、版番号を入れておく。
- 起動時:
  1. 制御ファイルを読み、CRC を検査する。壊れていたら `FATAL`（`XX001 data_corrupted`）。
  2. `state == InProduction` なら、`FATAL: database system was not properly shut down; yuzhu M2 has no crash recovery`（文言は仮）を出して終了する。終了コードは 0 以外。
  3. 問題なければ `state = InProduction` を書いて fsync してから、接続を受け付ける。
- 強制起動 `--force-start-after-crash`（名前は仮）: 再起動を拒否されたままでは開発中に困るので用意する。使うと WARNING をログに出す。データの整合性は保証しない（clog が「コミット済み」と言っていても、そのデータページが書かれていない可能性がある）。
- 正常停止の手順: 待ち受けを止める → 各接続に停止を通知し、実行中のトランザクションを中断させる（クライアントには `FATAL 57P01 terminating connection due to administrator command`）→ 全接続のスレッドの終了を待つ → バッファプールの dirty ページをすべて書く → clog を書く → 全ファイルを fsync → 制御ファイルに `state = Shutdowned` を書いて fsync。**途中で I/O エラーが起きたら `state` は `InProduction` のまま**にし、次の起動で拒否されるようにする（障害注入でテストする）。
- シグナルの処理: Rust の標準ライブラリだけではシグナルを受け取れない（unsafe と libc が要る）。周辺用途として `signal-hook` クレートの追加を提案する（unsafe はクレートの内部に閉じているので、yuzhu の `forbid(unsafe_code)` には抵触しない）。依存の追加になるので、`QUESTIONS.md` に記録する。

### 7.3 M3 での変更点

制御ファイルの `InProduction` で起動を拒否する処理を、「チェックポイントから REDO する」処理に置き換える。データファイル、clog、タプル形式、可視性判定は変えない。

---

## 8. テスト計画

### 8.1 slt（`tests/slt/m2/`。PostgreSQL 17 で期待値を確かめる）

| ファイル | 内容 |
|---|---|
| `dml/update_basic.slt` | 1 列・複数列の SET、WHERE なし（全行）、WHERE に合う行がない（`statement count 0`）、`statement count n` でタグの件数を確認 |
| `dml/update_expr.slt` | `SET a = a + 1`、`SET a = b, b = a`（入れ替え）、CASE・関数・キャスト、`SET a = DEFAULT`、DEFAULT がない列への `DEFAULT`（NULL になる）、`SET (a, b) = (1, 2)`（対応する場合） |
| `dml/update_constraints.slt` | NOT NULL（23502）、CHECK（23514。SET していない列に関係する CHECK も評価される）、varchar(n) の超過（22001）、int のオーバーフロー（22003）、CHECK が NULL なら通る、**3 行の更新の 2 行目で違反したら 1 行も変わらない** |
| `dml/update_halloween.slt` | `UPDATE t SET a = a + 1`（1 回だけ増える。ページをまたぐ件数で）、`INSERT INTO t SELECT * FROM t`（ちょうど 2 倍になる）、`UPDATE t SET a = a * 2 WHERE a > 0` |
| `dml/update_errors.slt` | 42703、42601（同じ列への代入）、42804（WHERE の型、代入できない型）、42P01、0A000（RETURNING・FROM） |
| `dml/delete_basic.slt` | WHERE あり・なし、`WHERE NULL`（0 件）、削除してから再挿入、`statement count` |
| `txn/rollback_dml.slt` | BEGIN → UPDATE → SELECT（自分の変更が見える）→ ROLLBACK → 元に戻っている。DELETE も同じ。同じトランザクションで INSERT → UPDATE → DELETE（combo CID の経路）→ COMMIT / ROLLBACK |
| `txn/failed_block_dml.slt` | ブロック内で UPDATE が CHECK 違反 → 25P02 → ROLLBACK → それまでの変更が残っていない。暗黙トランザクション（1 つの Query の中で `UPDATE ...; SELECT 1/0;`）全体が巻き戻る［sqllogictest で 1 レコードに複数の文を書けるかは未検証。だめなら Rust の結合テストで書く］ |
| `txn/visibility_2conn.slt` | `connection` を使い、未コミットの UPDATE / DELETE / INSERT が他の接続から見えないこと、コミット後は見えること（ブロックしない操作だけ。`research-slt.md` の注意を参照） |

- 書き込みの待ち（ライターロック）はランナーが止まるので、slt では試さない（Rust の結合テストで書く）。
- 返る行の順序が決まらないものには、すべて `rowsort` を付ける（UPDATE で版の位置が変わり、PostgreSQL と yuzhu の物理的な順序が違ってくるため）。

### 8.2 再起動をまたぐ永続化テスト（言語非依存）

- 配置: `tests/restart/<シナリオ>/01-before.slt`、`02-after.slt`（必要なら `03-...`）。
- `tests/run.sh --target pg|yuzhu --restart-suite`: フェーズごとに slt を流し、その間でサーバを再起動する。
  - `yuzhu`: SIGTERM → プロセスの終了を待つ → 同じデータディレクトリで起動 → 待ち受けを待つ。
  - `pg`: `docker restart`（または `pg_ctl restart -m fast`）。**同じシナリオが本物の PostgreSQL でも通ることを CI で確かめる。**
- シナリオ:
  1. CREATE / INSERT / UPDATE / DELETE をコミット → 再起動 → 値がそのまま残っている。
  2. BEGIN → UPDATE → ROLLBACK → 再起動 → 元の値（clog の永続化の確認）。
  3. バッファプールより大きいテーブル（M2 では、テスト用にプールの大きさを設定で小さくできるようにする）を BEGIN → 全行 UPDATE → ROLLBACK → 再起動 → 元の値（steal されたページの未コミットの版が見えない）。
  4. BEGIN → CREATE TABLE → ROLLBACK → 再起動 → テーブルがなく、ファイルも残っていない（ファイルの確認は yuzhu だけ。Rust 側で）。
  5. DROP TABLE をコミット → 再起動 → ない。
  6. コミットしていないトランザクション（`connection` で別の接続を開いたまま）がある状態で停止 → 再起動 → その変更はない（停止時の中断処理の確認）［ランナーが再起動後の接続を張り直すかは未検証。張り直さないなら、フェーズごとにランナーを起動し直す形にする］。
  7. 複数のページと、1GB セグメントの境界: 実際に 1GB を書くのは CI で重いので、**セグメントの大きさをビルド時かテスト用の設定で小さくできるようにして**（例: 64 ページ）、Rust 側でセグメントをまたぐテストを書く。

### 8.3 Rust のテスト

- **可視性判定の真理値表**: xmin・xmax の状態（自分 / コミット済み / 中断 / 実行中 / 未来 / Frozen）× cid の大小の全組み合わせを、表形式でテストする。
- **ヒープ**: insert / delete / update と `TmResult`。同じページに入る更新と、別のページに行く更新。`t_ctid` の連鎖。`row is too big`。
- **カーソル**: `scan_next` のあと毎回 `pinned_count() == 0`。スキャンの途中で同じページを更新しても止まらない。スキャン開始後に伸びたブロックを読まない。
- **Halloween**: ノード単位で、UPDATE の入力に新版が現れないこと。
- **XID の採番**: `next_xid_limit` を超えたら制御ファイルが書かれる。再起動したら上限から再開する（XID を再利用しない）。
- **障害注入**（ファイル抽象化レイヤ）:
  - 正常停止中の write / fsync を失敗させる → `state` が `InProduction` のまま → 次の起動が拒否される。
  - 実行中の書き込みが EIO → 文がエラー（`58030 io_error`）になり、トランザクションは中断、サーバは落ちない。
  - kill -9 の模擬（メモリ上の FS で、fsync していない書き込みを捨てる）→ 次の起動が拒否される。
- **モデルとの突き合わせ（性質テスト）**: INSERT / UPDATE / DELETE / COMMIT / ROLLBACK / 正常再起動をランダムに並べた列を、`BTreeMap` のモデルと yuzhu の両方で実行し、各時点の SELECT の結果を比べる（乱数は自前か proptest。proptest は dev-dependency なので追加してよい）。
- **ライターロック**: 2 つのスレッドで、ロックを持つ側が COMMIT するまで、もう一方が待つこと。`lock_timeout` で 55P03 になること。
- **結合テスト**: `yuzhu-server` をスレッドで起動して `postgres` クレートで接続し、Server の停止 API → 同じディレクトリで再起動、という流れで 8.2 節のシナリオを Rust 側でも回す（CI で速く、ファイルの存在も確認できる）。

---

## 9. 実装の順序（並列化の目安）

| 順 | 作業 | 依存 | 工数感 |
|---|---|---|---|
| 1 | `Tid`・`Xid`・`Snapshot`・`TmResult`・新しい `TableStore` と `Transaction` の型（契約） | ページ・タプル形式の設計 | 0.5 人日 |
| 2a | clog・XID の採番・制御ファイル・ライターロック | 1 | 2 人日 |
| 2b | ヒープの delete / update / スキャンカーソルと可視性判定 | 1、ヒープの insert | 2.5 人日 |
| 2c | パーサ・アナライザの UPDATE / DELETE | 1 | 1.5 人日 |
| 2d | slt と再起動ハーネス（PostgreSQL で先に通す） | なし | 1.5 人日 |
| 3 | Update / Delete ノード、Session の手順（ロック → スナップショット → 実行 → CCI）、pending な作成・削除 | 2a〜2c | 1.5 人日 |
| 4 | 正常停止の手順、シグナル、障害注入テスト、モデルとの突き合わせテスト | 3 | 1.5 人日 |

2a〜2d は並列で進められる。

---

## 10. QUESTIONS.md に記録すべき仮決め

- M2 から xmin/xmax + clog で ROLLBACK を実現する（案 B）。M1 の undo ログは廃止する。
- M2 から単一ライターロックを入れる（ダーティリードが解消し、書き込みはトランザクション単位で直列化される）。
- M2 は正常停止だけを保証し、異常終了のあとは起動を拒否する（強制起動のオプションあり）。COMMIT は fsync しない。
- シグナル処理のために `signal-hook` を追加する。
- 削除・更新された版は VACUUM（M5）まで回収しない。
- システム列（`ctid`、`xmin` など）の SELECT は任意機能とし、slt には入れない。
