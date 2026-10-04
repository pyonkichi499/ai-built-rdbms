# yuzhu M3 調査: MVCC の可視性とトランザクション管理

調査日: 2026-10-04。対象は PostgreSQL **REL_17_STABLE** ブランチ。ソースは `raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/...` から取得して読んだ。行番号は取得時点のもので、ブランチの更新でずれることがある。

記号の意味:

- 【確認】今回ソースを開いて確かめた事実
- 【記憶】過去の知識に基づく記述。今回は細部まで照合していない（未検証）
- 【提案】yuzhu への推奨（PostgreSQL の事実ではない）

参照した主なファイル（すべて REL_17_STABLE）:

| ファイル | URL |
|---|---|
| heapam_visibility.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/heapam_visibility.c> |
| snapshot.h | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/utils/snapshot.h> |
| snapmgr.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/time/snapmgr.c> |
| procarray.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/storage/ipc/procarray.c> |
| xact.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/xact.c> |
| combocid.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/time/combocid.c> |
| htup_details.h | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/htup_details.h> |
| clog.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/clog.c> |
| transam.h | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/transam.h> |
| varsup.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/transam/varsup.c> |
| heapam.c | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/heapam.c> |
| tableam.h | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/tableam.h> |

ドキュメント:

- 13 章 Concurrency Control: <https://www.postgresql.org/docs/17/mvcc.html>
- 13.2 Transaction Isolation: <https://www.postgresql.org/docs/17/transaction-iso.html>
- 24.1.5 Preventing Transaction ID Wraparound Failures: <https://www.postgresql.org/docs/17/routine-vacuuming.html#VACUUM-FOR-WRAPAROUND>
- 65.6 Database Page Layout: <https://www.postgresql.org/docs/17/storage-page-layout.html>
- 9.27 System Information Functions（`pg_current_xact_id` 等）: <https://www.postgresql.org/docs/17/functions-info.html>
- 5.6 System Columns（`xmin` `xmax` `cmin` `cmax` `ctid`）: <https://www.postgresql.org/docs/17/ddl-system-columns.html>

---

## 0. 結論（yuzhu M3 への推奨）

1. **XID は 64 ビット（`Xid(u64)`）にし、タプルヘッダにもそのまま 64 ビットで格納する**【提案】。周回（wraparound）対策の凍結（freeze）は不要になる。ただし pg_xact（コミットログ）を切り詰めるために、M5 の VACUUM で「この XID 未満はすべて確定済み」という境界を進める仕組みは必要（第 5 節）。外部に見せる `xmin` 列（型 `xid`, OID 28）は下位 32 ビット、`pg_current_xact_id()`（型 `xid8`）は 64 ビットそのものを返す。
2. **可視性判定は PostgreSQL の `HeapTupleSatisfiesMVCC` を、サブトランザクション・MultiXact・`HEAP_MOVED` を除いて忠実に移植する**（第 3 節にアルゴリズム全文）。判定順は「自分の XID か → スナップショットで実行中か → clog でコミット済みか」で、**clog を見るのは必ずスナップショット判定の後**。
3. **スナップショットは `{ xmin, xmax, xip（ソート済み）, curcid, 自分の XID }`**。取得は ProcArray の `Mutex` 内で行う。Read Committed では文ごとに取り直す。
4. **単一ライターは「最初の書き込み時に XID を割り当て、同時にグローバルなライターロックを取ってトランザクション終了まで保持する」**方式にする【提案】。DML 文では**ライターロック取得 → スナップショット取得**の順にする。こうすれば M3 では他トランザクションとの更新競合（`TM_Updated` など）が原理的に起きない。それでも `heap_update`/`heap_delete` は最初から PostgreSQL と同じ `TM_Result` を返す API にしておき、M5 で行ロック待ちと EvalPlanQual 相当を足すだけで済むようにする。
5. **cmin と cmax はヘッダに別々のフィールドで持つ**（combo CID は使わない）【提案】。4 バイト増えるが、バックエンドごとの対応表と、そのバグの余地がなくなる。
6. **ヒントビット（`XMIN_COMMITTED` など）はフォーマット上は最初から定義し、M3 で設定もする**。ただし WAL には記録しない（PostgreSQL のチェックサム無効時と同じ）。ページチェックサムを採用するなら `wal_log_hints` 相当の対応が必要（第 6 節）。
7. **トランザクション状態機械は PostgreSQL の TBlockState を、サブトランザクションと PREPARE を除いて縮小したもの**を使う（第 7 節）。文の失敗はトランザクション全体を Failed にするので、**文単位の undo もサブトランザクションも要らない**。`SAVEPOINT` は M3 では `0A000`。
8. **コミット順序は「コミットレコードを WAL に書く → fsync → clog に COMMITTED → ProcArray から外す」**。この順序が可視性判定の正しさの前提になる（第 4.3 節）。チェックポイントとの競合を避けるため、コミットレコード挿入から clog 更新までをチェックポイントの開始と排他にする（PostgreSQL の `DELAY_CHKPT_START` 相当）。

---

## 1. PostgreSQL のタプルヘッダと MVCC の基本

### 1.1 ヘッダのフィールド【確認: htup_details.h】

```c
typedef struct HeapTupleFields
{
	TransactionId t_xmin;		/* inserting xact ID */
	TransactionId t_xmax;		/* deleting or locking xact ID */
	union
	{
		CommandId	t_cid;		/* inserting or deleting command ID, or both */
		TransactionId t_xvac;	/* old-style VACUUM FULL xact ID */
	}			t_field3;
} HeapTupleFields;
```

このほかに `t_ctid`（6 バイト。更新後の新しい版の TID。最新版なら自分自身）、`t_infomask2`（列数と HOT 関連）、`t_infomask`、`t_hoff` がある。ヘッダは計 23 バイト【記憶。65.6 節の表で 23 バイトと記載】。

`t_infomask` の可視性関連ビット【確認】:

| ビット | 値 | 意味 |
|---|---|---|
| `HEAP_XMAX_KEYSHR_LOCK` | 0x0010 | xmax は KEY SHARE ロック保持者 |
| `HEAP_COMBOCID` | 0x0020 | t_cid は combo CID |
| `HEAP_XMAX_EXCL_LOCK` | 0x0040 | xmax は排他ロック保持者 |
| `HEAP_XMAX_LOCK_ONLY` | 0x0080 | xmax は「ロックだけ」で削除者ではない |
| `HEAP_XMIN_COMMITTED` | 0x0100 | xmin はコミット済み（ヒント） |
| `HEAP_XMIN_INVALID` | 0x0200 | xmin はアボート済み（ヒント） |
| `HEAP_XMIN_FROZEN` | 0x0300 | 上の 2 つの両方 = 凍結済み |
| `HEAP_XMAX_COMMITTED` | 0x0400 | xmax はコミット済み（ヒント） |
| `HEAP_XMAX_INVALID` | 0x0800 | xmax は無効かアボート済み（ヒント） |
| `HEAP_XMAX_IS_MULTI` | 0x1000 | xmax は MultiXactId |
| `HEAP_UPDATED` | 0x2000 | UPDATE で作られた版 |
| `HEAP_MOVED_OFF/IN` | 0x4000/0x8000 | 9.0 より前の VACUUM FULL 用（互換のためだけに残る） |

### 1.2 INSERT / DELETE / UPDATE がヘッダに何を書くか【記憶。heapam.c の heap_insert / heap_delete / heap_update の概要】

- INSERT: `xmin = 自分の XID`, `cmin = 現在のコマンド ID`, `xmax = 0`（`HEAP_XMAX_INVALID`）。
- DELETE: 対象タプルに `xmax = 自分の XID`, `cmax = 現在のコマンド ID`。ヒントビット `XMAX_COMMITTED/INVALID` とロックビットはクリアする。
- UPDATE: 古い版に DELETE と同じ印を付け、`t_ctid` を新しい版の TID に向ける。新しい版は INSERT と同じ印に加えて `HEAP_UPDATED`。
- ROLLBACK: **何もしない**。clog に ABORTED を書くだけ。残ったタプルはアボート済み XID を持つので不可視になり、後で VACUUM が回収する。

### 1.3 コマンド ID と combo CID【確認: xact.c, combocid.c】

- コマンド ID（CID）は 32 ビット。トランザクション内で「タプルを書き換えた文」が終わるたびに `CommandCounterIncrement` で 1 増える。**書き換えに使われていなければ増やさない**（`currentCommandIdUsed`）。上限を超えると `54000 program_limit_exceeded`（"cannot have more than 2^32-2 commands in a transaction"）。
- スナップショットの `curcid` より小さい CID の変更だけが見える。これが **同じ文の中で自分が挿入した行を自分のスキャンが見ない**（Halloween 問題の防止。`INSERT INTO t SELECT * FROM t` が無限に増えない）根拠になる。
- PostgreSQL は 8.3 から cmin と cmax を 1 つのフィールドに重ねている。同じトランザクションが挿入して削除した場合だけ、バックエンドローカルな配列の添字（combo CID）を格納して `(cmin, cmax)` を引く。CID は元のトランザクションが終われば不要なので、他のバックエンドから見える必要はない（combocid.c 冒頭コメント）。

---

## 2. スナップショット

### 2.1 SnapshotData【確認: snapshot.h】

MVCC スナップショットに関係するフィールド:

```c
TransactionId xmin;   /* all XID < xmin are visible to me */
TransactionId xmax;   /* all XID >= xmax are invisible to me */
TransactionId *xip;   /* 取得時点で実行中の XID。xmin <= xip[i] < xmax */
uint32 xcnt;
TransactionId *subxip; int32 subxcnt; bool suboverflowed;  /* サブトランザクション用 */
CommandId curcid;     /* in my xact, CID < curcid are visible */
```

### 2.2 GetSnapshotData【確認: procarray.c】

- `ProcArrayLock` を**共有**モードで取る。
- `xmax = latestCompletedXid + 1`（「完了した最大の XID」の次）。
- `xmin` は `xmax` で初期化し、自分の XID があればそれも考慮する。
- 全バックエンドの XID を走査し、XID なし（読み取り専用）、自分自身、`xid >= xmax`、論理デコーディング中や VACUUM 中のものを除いて `xip` に入れ、`xmin` を最小値に更新する。
- **自分の XID は `xip` に入れない**。自分の変更は CID で判定するため。
- 同時に `MyProc->xmin` を設定し、VACUUM が回収してよい境界（horizon）の計算に使わせる【記憶】。

### 2.3 XidInMVCCSnapshot【確認: snapmgr.c】

```
xid <  xmin → 実行中ではない（false）
xid >= xmax → 実行中とみなす（true）
それ以外    → xip に含まれていれば実行中（サブトランザクション処理を除く）
```

### 2.4 Read Committed と Repeatable Read の違い【確認: snapmgr.c GetTransactionSnapshot】

`IsolationUsesXactSnapshot()`（Repeatable Read 以上）ならトランザクション最初のスナップショットを使い回し、Read Committed なら `GetTransactionSnapshot` が呼ばれるたびに（実質的に文ごとに）`GetSnapshotData` を呼び直す。Simple Query で複数の文を 1 メッセージに入れた場合も、文ごとに新しいスナップショットになる【記憶。postgres.c の exec_simple_query は parsetree ごとにスナップショットを設定する】。

---

## 3. 可視性判定アルゴリズム

### 3.1 HeapTupleSatisfiesMVCC の構造【確認: heapam_visibility.c】

サブトランザクション・MultiXact・`HEAP_MOVED` を除いて骨格だけ抜き出すと、次のとおり（コメントは筆者）。

```
if !XMIN_COMMITTED:
    if XMIN_INVALID: return false
    if xmin == 自分（TransactionIdIsCurrentTransactionId）:
        if cmin >= curcid: return false          # この文の開始後に挿入
        if XMAX_INVALID: return true
        if XMAX_LOCK_ONLY: return true
        if xmax != 自分: SetHint(XMAX_INVALID); return true   # 削除したサブトランザクションがアボート
        return cmax >= curcid                     # この文の開始後に削除なら見える
    elif XidInMVCCSnapshot(xmin): return false    # 挿入者は実行中
    elif TransactionIdDidCommit(xmin): SetHint(XMIN_COMMITTED, xmin)
    else: SetHint(XMIN_INVALID); return false     # アボートかクラッシュ
else:
    if !XMIN_FROZEN && XidInMVCCSnapshot(xmin): return false  # ヒント上はコミット済みでも、自分のスナップショットでは実行中

# ここまで来たら挿入はコミット済みで見える
if XMAX_INVALID: return true
if XMAX_LOCK_ONLY: return true
if !XMAX_COMMITTED:
    if xmax == 自分: return cmax >= curcid
    if XidInMVCCSnapshot(xmax): return true       # 削除者は実行中
    if !TransactionIdDidCommit(xmax): SetHint(XMAX_INVALID); return true
    SetHint(XMAX_COMMITTED, xmax)
else:
    if XidInMVCCSnapshot(xmax): return true
return false                                      # 削除はコミット済み
```

重要な注意点（ファイル冒頭コメント）【確認】:

- **clog（`TransactionIdDidCommit`）より先に「実行中か」を調べる**。xact.c は clog に記録してから ProcArray の XID を消すので、その隙間では「実行中」と「コミット済み」が両方真になる。clog だけを見ると、後で取ったスナップショットでは実行中扱いの XID をコミット済みとして扱ってしまう。
- MVCC スナップショットでは `TransactionIdIsInProgress` の代わりに `XidInMVCCSnapshot` を使う。
- `TransactionIdDidAbort` は使わない。クラッシュ時に実行中だったトランザクションは clog 上 ABORTED と記録されないので、「コミットしていない かつ 実行中でない ⇒ アボート」と消去法で判定する。

### 3.2 yuzhu の可視性判定（M3 確定版）【提案】

yuzhu はサブトランザクションも MultiXact も持たないので、次のように単純化できる。M5 で行ロック（`FOR UPDATE`）と複数ライターが入っても、`XMAX_LOCK_ONLY` の分岐はすでに入っているので構造は変わらない。

```rust
/// 戻り値は「このスナップショットから見えるか」。
/// ヒントビットを立てた場合は hint_dirty を true にする（呼び出し側がページを dirty にする）。
pub fn satisfies_mvcc(
    h: &mut TupleHeader,
    snap: &Snapshot,
    clog: &Clog,
    hint_dirty: &mut bool,
) -> bool {
    // ---- 挿入側 ----
    if !h.infomask.contains(XMIN_COMMITTED) {
        if h.infomask.contains(XMIN_INVALID) {
            return false;
        }
        if Some(h.xmin) == snap.current_xid {
            if h.cmin >= snap.curcid {
                return false; // この文の開始後に自分が挿入した
            }
            if h.infomask.contains(XMAX_INVALID) || h.xmax == Xid::INVALID {
                return true;
            }
            if h.infomask.contains(XMAX_LOCK_ONLY) {
                return true;
            }
            // サブトランザクションがないので、自分が挿入した行の xmax は自分以外ありえない
            // （M5 で他者がロックだけ掛けるケースは LOCK_ONLY で上で抜けている）
            debug_assert_eq!(Some(h.xmax), snap.current_xid);
            return h.cmax >= snap.curcid; // この文の開始後に削除したなら見える
        }
        if snap.is_running(h.xmin) {
            return false;
        }
        match clog.status(h.xmin) {
            XactStatus::Committed => set_hint(h, XMIN_COMMITTED, hint_dirty),
            _ => {
                // Aborted または InProgress（＝クラッシュで残ったもの）
                set_hint(h, XMIN_INVALID, hint_dirty);
                return false;
            }
        }
    } else if !h.is_xmin_frozen() && snap.is_running(h.xmin) {
        return false;
    }

    // ---- 削除側（ここに来たら挿入はコミット済みで、スナップショットから見える）----
    if h.infomask.contains(XMAX_INVALID) || h.xmax == Xid::INVALID {
        return true;
    }
    if h.infomask.contains(XMAX_LOCK_ONLY) {
        return true;
    }
    if !h.infomask.contains(XMAX_COMMITTED) {
        if Some(h.xmax) == snap.current_xid {
            return h.cmax >= snap.curcid;
        }
        if snap.is_running(h.xmax) {
            return true;
        }
        if clog.status(h.xmax) != XactStatus::Committed {
            set_hint(h, XMAX_INVALID, hint_dirty);
            return true;
        }
        set_hint(h, XMAX_COMMITTED, hint_dirty);
    } else if snap.is_running(h.xmax) {
        return true;
    }
    false
}

impl Snapshot {
    /// PostgreSQL の XidInMVCCSnapshot と同じ。
    pub fn is_running(&self, xid: Xid) -> bool {
        if xid < self.xmin { return false; }
        if xid >= self.xmax { return true; }
        self.xip.binary_search(&xid).is_ok()
    }
}
```

補足:

- `h.xmax == Xid::INVALID` の判定を `XMAX_INVALID` と並べているのは、yuzhu ではヒントビットを WAL に載せないため、リカバリ後のページで `XMAX_INVALID` が立っていない新規タプルがありうるから。**INSERT 時に `XMAX_INVALID` を立てる**のは PostgreSQL と同じ【記憶: heap_prepare_insert】で、これは WAL レコードの再生でも同じ値が入る（ヒントではなくタプル本体の一部として記録される）ので、実際には二重の保険になる。
- clog が `InProgress` を返すのに、スナップショット上は実行中でない XID は「クラッシュで残ったもの」。PostgreSQL と同じく消去法でアボート扱いにする。リカバリ完了時に明示的に ABORTED を書き込むかは任意（第 4.4 節）。
- ヒントビットを立ててよいのは「その XID の状態が確定したと分かった」ときだけ。`XMIN_COMMITTED` なら、PostgreSQL はさらに「コミットレコードが fsync 済み」を確認する（`SetHintBits` の `XLogNeedsFlush(commitLSN)` 判定）【確認】。yuzhu M3 は同期コミットだけで、clog への記録は fsync の後なので、clog が Committed を返した時点でこの条件は常に満たされる。非同期コミットを入れるときに同じ判定を追加する。

### 3.3 UPDATE / DELETE 用の判定: satisfies_update【提案。PostgreSQL の HeapTupleSatisfiesUpdate と TM_Result に倣う】

PostgreSQL は更新対象のタプルを見つけたあと、`HeapTupleSatisfiesUpdate` で最新状態を調べ、`TM_Result`（`TM_Ok`, `TM_Invisible`, `TM_SelfModified`, `TM_Updated`, `TM_Deleted`, `TM_BeingModified`, `TM_WouldBlock`）を返す【確認: tableam.h, heapam_visibility.c】。`TM_BeingModified` なら `XactLockTableWait` で相手の終了を待つ【確認: heapam.c heap_delete / heap_update 内】。Read Committed では相手がコミットして `TM_Updated` になったら、ctid チェーンをたどって最新版に WHERE を再評価する（EvalPlanQual）【記憶】。

yuzhu は M3 から同じ列挙型を持つ。

```rust
pub enum TmResult {
    Ok,
    Invisible,
    SelfModified { cmax: CommandId },
    Updated { new_tid: Tid, xmax: Xid },
    Deleted { xmax: Xid },
    BeingModified { xmax: Xid },
}
```

- M3 で実際に返るのは `Ok`、`Invisible`、`SelfModified` だけ（第 4.2 節の理由）。`Updated`/`Deleted`/`BeingModified` が返ったら M3 では内部エラー（`XX000`）にする。M5 でここに待機と EvalPlanQual を差し込む。
- `SelfModified` は、同じ文の中で同じ行を 2 回更新しようとしたとき（結合で 1 行に複数行が当たる UPDATE ... FROM など）に起きる。PostgreSQL は `cmax == 現在の CID` なら 2 回目を黙って無視し、トリガー経由などで後の CID なら `27000` の "tuple to be updated was already modified by an operation triggered by the current command" を返す【記憶】。yuzhu M3 にはトリガーがないので「無視」だけ実装すればよい。

---

## 4. トランザクション管理

### 4.1 XID の割り当て【確認: xact.c GetCurrentTransactionId / AssignTransactionId】

PostgreSQL は XID を**最初に必要になった時点で**割り当てる（読み取り専用トランザクションは XID を持たない）。yuzhu も同じにする。読み取りだけのトランザクションは ProcArray を汚さず、他者のスナップショットの `xip` も増やさない。

### 4.2 単一ライターの実現方法【提案】

```rust
pub struct TxnManager {
    procarray: Mutex<ProcArray>,
    writer: WriterLock,          // M3: グローバルに 1 つ。M5 で行ロック + XID 待ちに置き換える
    clog: Clog,
    ckpt_gate: RwLock<()>,       // DELAY_CHKPT_START 相当（第 4.3 節）
}
```

- 書き込みを含む文（INSERT/UPDATE/DELETE/DDL）の**解析後・スナップショット取得前**に、まだ XID がなければ `writer` を取り、XID を割り当てる。`writer` はトランザクション終了（COMMIT/ROLLBACK）まで保持する。
- 読み取り専用の文は `writer` を取らない。スナップショットだけで動くので、ライター実行中でも読める（複数リーダー）。
- **ロック → スナップショットの順**にする理由: 逆にすると、B がスナップショットを取ったあと A のコミット待ちでブロックし、A が削除・更新した行を古いスナップショットで見つけて上書きしてしまう（lost update）。ロックを先に取れば、B のスナップショットには A のコミットがすべて含まれ、以後 B が終わるまで他のライターは動かないので、B から見える行に他者の xmax が付くことはない。
- Repeatable Read（M5）ではトランザクションのスナップショットを最初の文で取るので、この順序は保証できない。そのため M5 では `Updated`/`Deleted` で `40001 could not serialize access due to concurrent update` を返す処理が必須になる。これは複数ライターと同時に入れる。
- `writer` は `Mutex<()>` + ガードを `Transaction` に持たせる形で実装できる。ガードの寿命をトランザクションに合わせるため、`parking_lot::ArcMutexGuard` のような所有型ガードか、`Mutex<Option<Xid>>` + `Condvar` の自前実装を使う（`std::sync::MutexGuard` は借用なので構造体に入れにくい）。外部クレートを増やさないなら後者。

### 4.3 コミットとアボートの順序【確認: xact.c CommitTransaction / RecordTransactionCommit, procarray.c】

PostgreSQL の CommitTransaction は `RecordTransactionCommit()` → `ProcArrayEndTransaction()` の順で、RecordTransactionCommit の中は「`DELAY_CHKPT_START` を立てる → コミットレコードを WAL に挿入 → `XLogFlush` → `TransactionIdCommitTree`（clog 更新）→ `DELAY_CHKPT_START` を下ろす」。その後、ロックの解放やファイルの削除（`smgrDoPendingDeletes`）が続く。

yuzhu M3 のコミット手順【提案】:

```
1. ckpt_gate.read() を取る
2. WAL に COMMIT レコード（xid, 終了時刻, 削除予定のリレーション）を挿入
3. WAL を fsync（M3 は毎コミット。M6 でグループコミット）
4. clog[xid] = COMMITTED（ページはメモリ上で更新。永続化はチェックポイントと WAL 再生に任せる）
5. ckpt_gate を離す
6. procarray から xid を外し、latest_completed を更新（Mutex 内）
7. DROP TABLE などで削除予定のファイルを消す（pending deletes）
8. writer ロックを離す
```

- 4 → 6 の順序が第 3.1 節の「先に実行中か、後で clog」の前提。逆にすると、スナップショットに含まれないのに clog が InProgress の XID が生まれ、アボート扱いされてしまう。
- 1・5 の `ckpt_gate`: チェックポイントは REDO 開始位置を決める前に `ckpt_gate.write()` を取る。これがないと「REDO 位置より前にコミットレコードがあるのに、チェックポイントが書き出した clog にはまだ COMMITTED が入っていない」状態になり、クラッシュ後にそのコミットが失われる【記憶: DELAY_CHKPT_START の目的。xact.c のコメントに基づく理解】。
- ROLLBACK は WAL に ABORT レコードを書き（fsync は不要【記憶: PostgreSQL はアボートレコードを flush しない】）、clog に ABORTED、ProcArray から外す。タプルは触らない。
- CREATE TABLE で作ったファイルはアボート時に消し、DROP TABLE のファイルはコミット後に消す（pending deletes）。これはカタログの MVCC と整合させるために M3 で必要。

### 4.4 コミットログ（clog）【確認: clog.c / 記憶】

- PostgreSQL は XID ごとに 2 ビット（`CLOG_BITS_PER_XACT 2`）で IN_PROGRESS(0) / COMMITTED / ABORTED / SUB_COMMITTED を持つ【確認】。8KB ページ 1 枚で 32768 個【計算】。
- clog の更新自体は WAL に記録されず、コミット/アボートレコードの再生で復元される。新しいページを使い始めるときだけ `CLOG_ZEROPAGE` を記録する【記憶】。
- yuzhu【提案】: `pg_xact/` に 256KB 程度のセグメントファイルを置き、ページ番号は `xid / 32768`（u64）。数ページの LRU キャッシュ（SLRU 相当）を持ち、ファイル I/O は障害注入レイヤ経由。チェックポイントで dirty ページを書いて fsync。状態の値は PostgreSQL と同じ 0〜3 を使う（SUB_COMMITTED は使わない）。
- クラッシュ時に実行中だった XID は clog 上 IN_PROGRESS のまま残る。消去法でアボート扱いになるので書き換えなくてもよいが、リカバリ完了時に「リカバリ開始時点で終了レコードがない XID」を ABORTED にしておくと、デバッグと将来の clog 切り詰めが楽になる【提案】。

### 4.5 ProcArray とスナップショット取得【提案】

```rust
pub struct ProcArray {
    next_xid: Xid,                    // 次に割り当てる XID。WAL とチェックポイントで永続化
    latest_completed: Xid,
    running: BTreeSet<Xid>,           // XID を持つ実行中トランザクション
    backends: Slab<BackendSlot>,      // 接続ごと。VACUUM の horizon 計算用
}
pub struct BackendSlot {
    xid: Option<Xid>,
    snapshot_xmin: Option<Xid>,       // このバックエンドが保持するスナップショットの最小 xmin
}

pub struct Snapshot {
    pub xmin: Xid,
    pub xmax: Xid,
    pub xip: Arc<[Xid]>,              // ソート済み。自分の XID は含まない
    pub curcid: CommandId,
    pub current_xid: Option<Xid>,     // 自分の XID（サブトランザクションがないので 1 つで足りる）
}
```

取得手順（Mutex 内）:

```
xmax = latest_completed + 1
xip  = running のうち、自分以外で xmax 未満のもの（BTreeSet なので既にソート済み）
xmin = min(xip ∪ {xmax} ∪ {自分の xid})   # PostgreSQL と同じく自分の XID も xmin の計算には入れる
backends[me].snapshot_xmin を（未設定なら）xmin にする
```

- PostgreSQL と同じく `xmax = latest_completed + 1` にする。XID は割り当て順にコミットされるとは限らないので、`latest_completed + 1` 以上で `next_xid` 未満の XID は「実行中」とみなされる（xmax 以上なので）。`next_xid` を xmax にしてその範囲の running を全部 xip に入れても正しいが、PostgreSQL と同じ値にしておくと `pg_current_snapshot()` の出力が一致する。
- 割り当て（`next_xid` を進めて `running` に入れる）と終了（`running` から外して `latest_completed` を更新）は同じ Mutex で行う。M3 はライターが 1 つなので競合はほぼないが、M5 でもこの Mutex 1 つで正しい（PostgreSQL も ProcArrayLock 1 つ。性能対策の CSN 方式などは不要）。
- `pg_current_snapshot()`（xid8 版の `xmin:xmax:xip,...` テキスト）はこの構造体をそのまま出せばよい【記憶: 9.27 節】。

### 4.6 コマンド ID の運用【提案】

- `Transaction { xid: Option<Xid>, cid: CommandId, cid_used: bool, ... }`。CID は 0 から（PostgreSQL の `FirstCommandId = 0`【記憶】）。
- 書き込みを行ったら `cid_used = true`。文の終わり（と、DDL の途中でカタログ変更を後続処理に見せたいとき）に `command_counter_increment()` を呼び、`cid_used` なら `cid += 1`。`u32::MAX`（InvalidCommandId）に達したら `54000`。
- 文の開始時に `snapshot.curcid = txn.cid` を設定する。Read Committed では文ごとにスナップショットを取り直すので自然にそうなる。Repeatable Read（M5）ではスナップショットを再利用しつつ `curcid` だけ更新する（PostgreSQL の `SnapshotSetCommandId`）。

---

## 5. XID の周回（wraparound）と 64 ビット XID

### 5.1 PostgreSQL の現状【確認/記憶】

- タプルの XID は 32 ビット。比較は modulo-2^32（`TransactionIdPrecedes`）で、約 20 億トランザクション先までしか前後を判定できない。そのため VACUUM で古いタプルを凍結（`HEAP_XMIN_FROZEN`、9.4 より前は xmin を `FrozenTransactionId = 2` に書き換え）しなければならない【確認: transam.h に `BootstrapTransactionId 1`, `FrozenTransactionId 2`, `FirstNormalTransactionId 3`】。
- 周回が近づくと `xidVacLimit`（強制 autovacuum）→ `xidWarnLimit`（警告）→ `xidStopLimit`（新規 XID 割り当てを拒否）と段階的に制限する【確認: varsup.c GetNewTransactionId】。
- 内部的には 12 以降 `FullTransactionId`（epoch 32 ビット + xid 32 ビットの 64 ビット）があり、`latestCompletedXid` や `nextXid` は 64 ビットで管理されている【確認: transam.h, procarray.c】。ディスク上のタプルは 32 ビットのまま。
- タプルヘッダを 64 ビット XID にするパッチ（ページ単位の基準値 + 32 ビットのオフセット方式など）は議論されているが、17 には入っていない【記憶】。

### 5.2 yuzhu の決定: 64 ビット XID をタプルに直接持つ【提案】

| 案 | 内容 | 長所 | 短所 | 工数感 |
|---|---|---|---|---|
| A. 32 ビット + 凍結 | PostgreSQL と同じ | ヘッダが小さい。ディスク形式が PostgreSQL に近い | 周回の比較、凍結、緊急 VACUUM、xidStopLimit を全部作る必要。バグるとデータが消える | 大（M5 の VACUUM が重くなる） |
| B. ページに基準値 + タプルに 32 ビット | Postgres Pro 系の 64 ビット XID パッチの方式 | ヘッダが小さく周回なし | 基準値をずらす処理（ページ内の全タプル書き換え）が複雑 | 中〜大 |
| **C. タプルに 64 ビット** | `xmin`/`xmax` を u64 で格納 | 単純。比較は普通の `<`。周回は事実上起きない（毎秒 100 万 XID でも約 58 万年） | ヘッダが 8 バイト増える | **小** |

**C を推奨する。** 教育的・実験的な実装で、ヘッダ 8 バイトの増加より、周回処理を書かずに済む単純さと安全性のほうが価値が大きい。

C でも残る課題:

- **clog の切り詰め**: clog は XID とともに伸び続ける（1 億 XID で約 25MB）。M5 の VACUUM で「境界 `frozen_horizon` 未満の XID を持つタプルには、すべて確定したヒントビットが立っている（アボートしたタプルは回収済み）」状態を作り、境界未満の clog を消せるようにする。可視性判定では `xid < frozen_horizon` なら clog を引かずにコミット済みとみなせる（凍結と同じ役割）。M3 では切り詰めなしでよい。
- **外部表現**: システム列 `xmin`/`xmax` の型は `xid`（OID 28、32 ビット）なので下位 32 ビットを返す。`pg_current_xact_id()` と `pg_current_snapshot()` は `xid8`（OID 5069）/`pg_snapshot`（OID 5038）で 64 ビットをそのまま返せる【記憶: OID 値は pg_type.dat で要確認】。`txid_current()` は int8 で同じ値。PostgreSQL の `xid8` は epoch を含む 64 ビットなので、yuzhu の XID と意味が一致する。
- XID の初期値: PostgreSQL と同じく 0 = 無効、1 = ブートストラップ（initdb が作るカタログ行）、2 = 凍結（予約。使わない）、3 から通常の XID。

---

## 6. ヒントビット

### 6.1 PostgreSQL の扱い【確認: heapam_visibility.c SetHintBits / 記憶】

- 可視性判定のついでに `XMIN_COMMITTED` などを立て、`MarkBufferDirtyHint` でページを dirty にする。WAL には記録しない。
- データチェックサムが有効か `wal_log_hints = on` の場合、チェックポイント後に初めてヒントだけでページを dirty にするときは full page image を WAL に書く（`XLogSaveBufferForHint`）【記憶】。チェックサムがあると、ヒントビットの違いだけで破れたページ（torn page）もチェックサム不一致になるため。
- チェックサムなしなら、破れたページは「ヒントビットが立った版と立っていない版の混在」にしかならず、どちらも正しい内容なので問題ない。

### 6.2 yuzhu への推奨【提案】

- M3 から実装する。clog 参照がほぼなくなり、走査が速くなる。clog のページキャッシュが小さくても済む。
- WAL には記録しない。ページは「dirty（ヒントのみ）」として扱う。
- **ページチェックサムを採用するなら**、チェックポイント後に初めてヒントだけで dirty にしたときに full page image を WAL に書く（PostgreSQL の `wal_log_hints` 相当）。採用しないなら何もしなくてよい。どちらにするかは WAL の調査（`m3-wal` 系）の結論と合わせて決める。
- 注意: ヒントビットを書き込むには、ページに対して読み取りロックではなく少なくとも「ヒント書き込みを許すロック」が要る。yuzhu のバッファプールが `RwLock<Page>` の場合、読み取りガードでは書けない。選択肢は (a) 走査時に書き込みロックを取る（並列度が落ちる）、(b) ヒントだけ `AtomicU16` などで別に持つ、(c) 読み取りロック中に判定だけし、立てるべきヒントを覚えておいて、後で短時間だけ書き込みロックを取って反映する（try_write に失敗したら諦める）。**(c) を推奨**。PostgreSQL もヒントの設定は「できればする」扱いで、失敗しても正しさに影響しない。M3 で迷うなら、最初はヒントを立てない実装でも正しく動く（clog を毎回引くだけ）。

---

## 7. トランザクション状態機械

### 7.1 PostgreSQL【確認: xact.c】

2 層になっている。

- 低レベルの `TransState`: `TRANS_DEFAULT`, `TRANS_START`, `TRANS_INPROGRESS`, `TRANS_COMMIT`, `TRANS_ABORT`, `TRANS_PREPARE`
- クライアントから見たブロック状態 `TBlockState`: `TBLOCK_DEFAULT`, `TBLOCK_STARTED`（単一文の暗黙トランザクション）, `TBLOCK_BEGIN`, `TBLOCK_INPROGRESS`, `TBLOCK_IMPLICIT_INPROGRESS`（Simple Query の複数文で暗黙 BEGIN した状態）, `TBLOCK_PARALLEL_INPROGRESS`, `TBLOCK_END`, `TBLOCK_ABORT`, `TBLOCK_ABORT_END`, `TBLOCK_ABORT_PENDING`, `TBLOCK_PREPARE`, それにサブトランザクション用の `TBLOCK_SUB*` 9 個。

### 7.2 yuzhu【提案】

サブトランザクション、PREPARE、並列ワーカーを除き、遷移の中間状態（`BEGIN`/`END`/`ABORT_END`/`ABORT_PENDING`）は「文の処理後に確定する」関数呼び出しで表現して、状態としては持たない。

```rust
pub enum BlockState {
    Idle,                 // TBLOCK_DEFAULT
    Implicit,             // TBLOCK_STARTED / TBLOCK_IMPLICIT_INPROGRESS（Query メッセージ単位）
    InBlock,              // TBLOCK_INPROGRESS（BEGIN 済み）
    Failed,               // TBLOCK_ABORT（エラー後、ROLLBACK 待ち。25P02 を返す）
}

pub struct Transaction {
    pub state: BlockState,
    pub isolation: IsolationLevel,     // M3 は ReadCommitted のみ。M5 で RepeatableRead
    pub xid: Option<Xid>,              // 遅延割り当て
    pub cid: CommandId,
    pub cid_used: bool,
    pub snapshot: Option<Snapshot>,    // RC では文ごとに作り直す
    pub writer_guard: Option<WriterGuard>,  // M3 の単一ライター。M5 で消える
    pub pending_deletes: Vec<PendingDelete>,// コミット後/アボート後に消すファイル
    pub read_only: bool,
}
```

M1 の `Transaction`（undo ログ方式、`spec/design/m1.md` 3.4 節）からの移行:

- undo ログは廃止。ROLLBACK は clog に ABORTED を書くだけになる。
- M1 の「文が失敗したらその文だけ undo」は不要になる。PostgreSQL では、ブロック内で文が失敗するとトランザクション全体が Failed になり、ROLLBACK するしかないから（`SAVEPOINT` がない限り）。暗黙トランザクションでは Query メッセージ全体がアボートされる。M1 設計書 4.1 節の意味論はそのまま使える。
- ダーティリード（M1 の既知の制約）は MVCC で解消する。

---

## 8. カタログと MVCC【提案】

- カタログもヒープテーブルなので、同じ可視性判定に従う。DDL はトランザクションの中で行え、コミットまで他のセッションから見えず、ROLLBACK で取り消せる（PostgreSQL と同じ）。
- 自分のトランザクション内では、DDL の直後に `command_counter_increment()` を呼んで、同じトランザクションの後続の文から新しい定義が見えるようにする。
- カタログのキャッシュ（リレーションキャッシュ）を持つ場合、他のセッションのコミット済み DDL を反映する仕組み（PostgreSQL の invalidation message 相当）が要る。M3 は単一ライターなので、「カタログを変更したトランザクションがコミットしたらグローバルな世代番号を上げ、各セッションは文の開始時に世代番号を見てキャッシュを捨てる」で十分。
- DROP TABLE とテーブルを読んでいる他セッションの競合は、PostgreSQL ではリレーションロック（AccessExclusiveLock）で防いでいる【記憶】。M3 では「DDL は writer ロックに加えて、テーブル単位の `RwLock` を排他で取る。読み取りは文の間だけ共有で取る」程度の簡易なリレーションロックを入れるのが安全。詳細はロックの設計で決める。

---

## 9. M5（複数ライター）への道筋

M3 の構造を変えずに、次を追加すれば複数ライターになる【提案】。

1. `writer` ロックを廃止する。
2. 各トランザクションは自分の XID に対する「待ち合わせ口」（`Arc<(Mutex<bool>, Condvar)>`）を ProcArray に登録する。PostgreSQL の XactLockTableWait（自分の XID に対する排他ロック）に相当。
3. `heap_update`/`heap_delete` で `satisfies_update` が `BeingModified { xmax }` を返したら、ページロックを離して xmax の終了を待ち、再判定する。デッドロック検出（待ちグラフ）も必要。
4. Read Committed で `Updated` が返ったら、ctid をたどって最新版を取り、WHERE を再評価する（EvalPlanQual の簡易版）。Repeatable Read なら `40001`。
5. `SELECT ... FOR UPDATE/SHARE` は `xmax` に自分の XID と `XMAX_LOCK_ONLY | XMAX_EXCL_LOCK` を立てる。共有ロックを複数で持つには MultiXact が要るが、まず `FOR UPDATE`（排他）だけにして、共有ロックは後回しにできる。そのため **`XMAX_IS_MULTI` のビットはフォーマットで予約だけしておく**。
6. VACUUM の horizon は `min(全バックエンドの snapshot_xmin, running の最小値)`。M3 から `BackendSlot.snapshot_xmin` を維持しておけば、そのまま使える。

M3 の可視性判定（第 3.2 節）は、2〜5 の追加後もそのまま正しい。他人の xmax が実行中なら「まだ削除されていない」と判定するだけで、ロックだけの xmax は `XMAX_LOCK_ONLY` の分岐で見える扱いになる。

---

## 10. yuzhu のタプルヘッダ案【提案】

```rust
/// ヒープタプルのヘッダ（ディスク上、リトルエンディアン）
pub struct TupleHeader {
    pub xmin: Xid,          // u64  挿入した XID
    pub xmax: Xid,          // u64  削除・更新・ロックした XID（0 = なし）
    pub cmin: CommandId,    // u32  挿入した CID（挿入トランザクションの中でだけ意味がある）
    pub cmax: CommandId,    // u32  削除した CID（削除トランザクションの中でだけ意味がある）
    pub ctid: Tid,          // 6 バイト（block u32 + offset u16）。更新後の版を指す。最新なら自分
    pub infomask2: u16,     // 列数（11 ビット）+ 将来の HOT 用ビット
    pub infomask: u16,      // 下表
    pub hoff: u8,           // データ開始オフセット（NULL ビットマップを含む、8 バイト境界）
}                           // 計 35 バイト + NULL ビットマップ → 8 バイト境界に揃える
```

`infomask` は PostgreSQL と同じビット位置を使う（`HAS_NULL 0x0001`, `HAS_VARWIDTH 0x0002`, `XMAX_EXCL_LOCK 0x0040`, `XMAX_LOCK_ONLY 0x0080`, `XMIN_COMMITTED 0x0100`, `XMIN_INVALID 0x0200`, `XMAX_COMMITTED 0x0400`, `XMAX_INVALID 0x0800`, `XMAX_IS_MULTI 0x1000`（予約）, `UPDATED 0x2000`）。`COMBOCID` と `MOVED_*` は使わない（予約）。ビット位置を揃えておくと、PostgreSQL の資料や `pageinspect` の出力と見比べやすい。

cmin と cmax を分ける理由（第 0 節 5）: combo CID は「挿入と削除が同じトランザクション」のときだけ必要な、ヘッダを 4 バイト削るための工夫。yuzhu は 64 ビット XID ですでにヘッダが大きいので、4 バイトの節約より単純さを取る。将来ディスク形式を詰めるときに combo CID を入れても、可視性判定のコードは `cmin()`/`cmax()` のアクセサ越しにしておけば変わらない。

ディスク形式に関わる決定なので、`QUESTIONS.md` に「64 ビット XID と cmin/cmax の分離により、タプルヘッダは PostgreSQL（23 バイト）より大きい 35 バイト」と記録することを勧める。

---

## 11. テストの観点

`tests/` の sqllogictest は PostgreSQL でも通る必要があるので、MVCC の検証は主に 2 つに分ける。

- **sqllogictest（単一接続で書けるもの）**: ROLLBACK で INSERT/UPDATE/DELETE/CREATE TABLE/DROP TABLE が消える、Failed 状態で 25P02、`INSERT INTO t SELECT * FROM t` が 1 回分だけ増える（Halloween）、同じトランザクションで挿入して削除した行が見えない、UPDATE を同じ文で 2 回当てても 1 回分。sqllogictest-bin の複数接続機能（`connection` ディレクティブ）【記憶: 0.29 で使えるか要確認】が使えれば、Read Committed の「他セッションの未コミット行が見えない」「コミット後の次の文では見える」もここで書ける。
- **Rust の結合テスト（yuzhu 専用）**: 可視性判定の表駆動テスト（xmin/xmax の状態 × スナップショット × CID の全組み合わせを PostgreSQL のアルゴリズムと突き合わせる）、スナップショットの xmin/xmax/xip の性質テスト（ランダムな開始・終了列に対して「スナップショット取得時にコミット済みの XID だけが見える」ことを確認）、障害注入でコミット手順の各段階（WAL 挿入後、fsync 後、clog 更新後）でクラッシュさせ、再起動後に「fsync が終わったコミットは残り、それ以外は消える」ことの確認。

---

## 12. 未検証の点（実装前に確かめるとよいもの）

- `pg_snapshot` と `xid8` の型 OID（5038, 5069）。`pg_type.dat` で確認する。
- PostgreSQL のアボートレコードが flush されないこと（`RecordTransactionAbort` を読む）。
- UPDATE で `TM_SelfModified` のときの PostgreSQL の挙動（同じ CID なら無視、後の CID なら 27000）。`nodeModifyTable.c` を読む。
- sqllogictest-bin 0.29.1 の複数接続サポートの書式。
- `exec_simple_query` が複数文の Query で文ごとにスナップショットを取ること（`postgres.c`）。
- チェックポイントと `DELAY_CHKPT_START` の関係の詳細（`xlog.c` の `CreateCheckPoint` と `GetVirtualXIDsDelayingChkpt`）。
