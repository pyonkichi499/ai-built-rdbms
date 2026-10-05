# yuzhu M5 基本設計 02: 行ロック・更新競合・MultiXact・EvalPlanQual・Repeatable Read・B+Tree の複数ライター（RW）

M5 のうち「同じ行・同じキーを複数のトランザクションが同時に書く」ときの振る舞いを決める章です。ヒープの更新競合（PostgreSQL の `heap_delete` / `heap_update` / `heap_lock_tuple` の `l1:` / `l2:` / `l3:` ループ）、行ロックの xmax と infomask の組み立て、メモリ上だけの MultiXact（Q-014）、Read Committed の EvalPlanQual（簡易版）、`SELECT ... FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE`、Repeatable Read、B+Tree の複数ライター対応（一意検査の待ち）、`RETURNING` の仕上げを扱います。

- 位置づけ: `00-contracts.md`（M5 の契約。以下「00」）の担当 **RW**。WP は **RW-1 から RW-6**、決定は D2・D6・D7・D8・D18・D44・D45・D46・D47、型は 00 §4.3〜§4.6、ディスク形式は 00 §3.7。00 に従う。従えない点は §11「契約への変更依頼」に書いた。
- 境界: ロックマネージャ（`LockManager`、デッドロック検出、`pg_locks`、リレーションロックの取得手順、`ProcArray`、コミット / アボートの順序）は `01-lock-txn.md`（LK）。外部キーの RI 検査は `07-foreign-key.md`（FK）。VACUUM・pruning・凍結は `03-vacuum.md`（VC）。Extended Query は `04-extended-query.md`（XQ）。この章は、それらが提供する口（`wait_for_xact`、`is_in_progress`、`oldest_xmin` など）を**使う側**として書く。
- 前提の設計: `spec/design/m1.md`、`m2.md`（§3.5 タプルヘッダ、§6.5.3 ヒープの操作、§6.6 可視性）、`m3.md`（§3.9 HEAP の WAL、§5.1 ヒープの変更、§5.9 ロックの順序）、`spec/design/m4/00-contracts.md`（M4 の契約。§6 式の木、§8〜§9 プラン、§10 executor、§13 ストレージ、§19 M5 のための予約）。M4 の章 01〜11 はまだ無い。M4 の B+Tree の章（`06-btree.md`）が確定したら、§6.9 の洗い出し表を突き合わせ直す。
- 調査（根拠）: `spec/research/m5-concurrency.md`（以下 `m5c`。§4〜§7、§11、§15 が主）、`m3-mvcc.md`、`m3-tx-semantics.md`、`m4-btree.md`、`m5-types-fk.md`（RI の使い方）。
- 迷ったら PostgreSQL 17 と同じにする。PostgreSQL のソースは REL_17_STABLE（`PG:<path>`）。
- 根拠の記号: 【確認】ソースまたは実機で確かめた、【記憶】未照合、【提案】yuzhu への推奨。**【実機】は、この章を書くために PostgreSQL 17.11（`sandbox/pg.sh start`、`pageinspect` と `pgrowlocks` 拡張つき）で測った事実**で、§3.3・§5・§7 に値を載せた。測った手順は §9.1 にまとめた。

---

## 0. 決定（この章で扱う論点。調査間・既存設計との食い違いも）

00 の決定 D2（SAVEPOINT は実装しないが拡張点を残す）、D6（MultiXact はメモリ上だけ。更新者は入れない）、D7（更新競合の待ちは heap 層）、D8（EPQ は簡易版）、D18・D44（B+Tree の複数ライターと `InsertOutcome`）、D45（`ExternParam`）、D46（RETURNING）、D47（B+Tree の WAL）は**そのまま使う**。この章で追加で決めたことを RW-D1〜RW-D20 に書く。

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| RW-D1 | 更新者の xmax に `HEAP_XMAX_EXCL_LOCK` を立てるか（00 §3.7 は「（未検証）」） | 立てる／立てない | **立てない**。更新者の xmax は `LOCK_ONLY` も `EXCL_LOCK` / `KEYSHR_LOCK` も持たず、キー列を変えた UPDATE と DELETE だけ infomask2 の `KEYS_UPDATED` を立てる | 【実機】`pageinspect` で、非キー列の UPDATE は旧版の infomask = `0x0100`（`XMIN_COMMITTED` のヒントだけ）、キー列の UPDATE と DELETE は infomask2 に `0x2000`。ロック保持者の xmax とビットの形が重ならない（§3.3） |
| RW-D2 | `satisfies_update` を何に基づいて判定するか（M2 §6.5.3 の実装は `committed_in_snapshot`＝スナップショット基準） | スナップショット基準／**現在の状態**（ProcArray と clog） | **現在の状態**。`xid_state(x) = Running / Committed / Aborted`（§5.1）。xmin の判定もスナップショットを使わない | PostgreSQL の `HeapTupleSatisfiesUpdate` は `TransactionIdIsInProgress` / `TransactionIdDidCommit` を直接引く。スナップショット基準のままだと、待った後に辿る「スナップショットより後にコミットされた新しい版」を `Invisible` と誤判定し、EPQ と RR の 40001 判定が壊れる。M2 §6.5.3 の「実行中は生の実行中一覧で判定する」とも一致する |
| RW-D3 | 更新者を含む MultiXact と、新しい版へのロックの引き継ぎ（PostgreSQL の `xmax_new_tuple`） | PostgreSQL と同じ／D6 | **D6 のとおり、更新者は MultiXact に入れない。新しい版の xmax は常に空**（`XMAX_INVALID`）。ロック保持者がいる行の UPDATE / DELETE は、保持者全員の終了を待ってから行う | 【実機】PostgreSQL は、ロックを持つ行を自分で UPDATE すると新しい版の xmax に「自分 + `KEYSHR` + `LOCK_ONLY`」を引き継ぐ（旧版の KEY SHARE を新版に守らせるため）。yuzhu は他者の KEY SHARE が残る行を UPDATE させない（待つ）ので引き継ぎが要らず、自分のロックは自分の XID が終われば意味を失う（可視性は変わらない） |
| RW-D4 | KEY SHARE と「コミット済みの非キー列 UPDATE」（`m5c` §4 は触れていない） | 他の強度と同じく `Updated` を返す／PostgreSQL と同じく旧版を受け入れる | **旧版を受け入れる。ロックは最新版に置く**（§5.4）。更新者の infomask2 に `KEYS_UPDATED` が無く、要求が `KeyShare` のときだけ。呼び出し側には `Ok` と「最新版なし」を返し、旧版のデータをそのまま使わせる | 【実機】RR で、他者が非キー列を UPDATE（コミット）した行に `FOR KEY SHARE` すると成功して旧版 `(2,2)` を返し、`FOR SHARE` / `FOR NO KEY UPDATE` / `FOR UPDATE` は 40001。外部キーの検査（FK の `FOR KEY SHARE`）が、RR で親の非キー列の更新に当たって偽の 40001 を出さないために要る。ロックを最新版に置けば、親の削除・キー更新を防ぐという目的は果たせる（旧版に印を付ける必要がない＝更新者を MultiXact に入れずに済む） |
| RW-D5 | ロック待ちの後の再判定 | PostgreSQL: xmax と infomask の変化を比べて `goto l1`／毎回 `satisfies_update` からやり直す | **毎回やり直す**。待った後は、ラッチを取り直して `satisfies_update` を最初から行う | 比べる状態を持たずに済み、「待った間に別の誰かが先に印を付けた」ケースが自然に含まれる。収束する（待った XID は終了済み）。更新で別ページに新版を置くときだけ、2 つのラッチを取り直した後に旧版の xmax / infomask を比べる（§5.3） |
| RW-D6 | `NOWAIT` の 55P03 を誰が作るか（`RelHandle` に名前がない） | heap が作る（`m5c` §5.1）／executor が作る | **executor（`LockRows`）が作る**。heap の `lock_tuple` は `RowWait::Skip` も `RowWait::Error` も待たずに `TmResult::WouldBlock` を返す。`LockRows` が `wait == Error` なら `could not obtain lock on row in relation "t"`（55P03）にする | heap にリレーション名を持ち込まない。`RowMark` に `table_name` を足す（§4.4。00 §4.6 への追加） |
| RW-D7 | EvalPlanQual の再評価の範囲 | 計画木の部分再実行（PostgreSQL）／D8 の簡易版 | **D8 の簡易版**。【実機】で確かめた 4 つの挙動に合わせる: (1) 非ロック対象のテーブルは**元の版のまま**（結合相手が後で変わっても再評価に使わない）、(2) 相関サブクエリも**文のスナップショット**で再実行する（相手の新しい版を見ない）、(3) 非相関サブクエリ（InitPlan）は**再実行せず結果を使い回す**、(4) 新しい版が「元の走査では条件に合わなかった行」に変わっても拾わない（§5.6、§5.7、§7.2） | `m5c` §6.4 の「未検証」の表を実機で閉じた |
| RW-D8 | `LockRows` の位置と LIMIT | Limit の上／下（`m5c` §6.3【記憶】） | **Limit の下、Sort の上**（PostgreSQL と同じ）。さらに最終の射影（Project）は LockRows の**上**に置く。LIMIT / OFFSET が数えるのは「ロックできて再評価を通った行」で、OFFSET で読み飛ばした行もロックされる | 【実機】`ORDER BY id LIMIT 1 FOR UPDATE` で先頭行が再評価で落ちると、次の行 `id=2` が返る。`ORDER BY v DESC FOR UPDATE` で並び替えの後に値が変わった行は、元の並びの位置のまま新しい値で返る。EPQ で差し替えた行から出力式を計算し直すため、射影を LockRows の上に置く（§6.5） |
| RW-D9 | `FOR` 句を受け付ける範囲（00 §4.6 は「SELECT の最上位だけ」） | PostgreSQL と同じ（入れ子の副問い合わせ・CTE の中でも可）／最上位だけ | **最上位の SELECT だけ**。入れ子（FROM の副問い合わせ・CTE・SubLink・集合演算の腕の中）の `FOR` は `0A000`。FROM の副問い合わせは、`OF` なしの `FOR` が及ぶ位置にあれば `0A000`、`OF` が別の表だけを指すなら可。使えない組み合わせの文言は【実機】で固定した（§6.4） | 入れ子の行ロックは計画の書き換え（副問い合わせの引き上げ）と結びつく。M4 の planner が副問い合わせを展開する前（Bound の段階）で拒否すれば、黙ってロックを落とす危険がない |
| RW-D10 | Repeatable Read の最初のスナップショットを取る時点（00 §5.1 d と D5 は「ロックの後」） | ロックの後／ロックの前 | **RR のトランザクションスナップショットだけは、最初のスナップショットを要する文の、リレーションロックの取得の前に取る**。Read Committed の文のスナップショットは 00 のとおりロックの後 | 【実機】RR で最初の文（SELECT でも UPDATE でも）がテーブルロックを待つとき、待った後に相手のコミットした行が**見えない**（スナップショットが待ちの前に取られている）。RC は見える。挙動は【実機】で確認。理由は、PostgreSQL の `exec_simple_query` が解析（テーブルロックの取得を含む）の前にスナップショットを取るため【記憶】。`LOCK TABLE` はスナップショットを取らないので、「RR で先に `LOCK TABLE` する」慣用句は同じ意味で動く |
| RW-D11 | 「最初のスナップショットを取った」の定義（`SET TRANSACTION ISOLATION LEVEL` の 25001 判定。M3 §5.2 e / §6.11.1 は「FROM のない SELECT は立てない（PG 17 の実測）」） | M3 の記述／実機 | **実機に合わせる**。【実機】`BEGIN; SELECT 1; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ` は 25001（`SELECT 1`、`VALUES`、`SELECT now()` でも同じ。Simple Query でも Extended でも同じ）。スナップショットを要しない文は、トランザクション制御（BEGIN / COMMIT / ROLLBACK / SAVEPOINT 系）、`SET` / `RESET` / `SHOW`、`LOCK TABLE`、`SET CONSTRAINTS`、`CHECKPOINT`、`LISTEN` / `NOTIFY` / `UNLISTEN`、`FETCH` だけ。ほかの文（EXPLAIN、PREPARE、DEALLOCATE、DECLARE、COPY、DDL、`CREATE ROLE` を含む）はすべて立てる（§6.7）。M3 の記述（M3 §5.2 e の「`SELECT 1` は立てない」）を訂正する | `m3-tx-semantics.md` の実機 #15 も `begin; select 1; set transaction isolation level repeatable read` が 25001 としている。M3 の設計と実装（`session.rs` の `txn_snapshot_taken`）が食い違っていた |
| RW-D12 | 読み取り専用トランザクションの `FOR` 句の 25006 の判定時点（00 §5.1 b は「生のパース木で判定」） | 生のパース木／解析後 | **解析後（実行の前）に、ロック対象のテーブルが 1 つ以上あるときだけ**。メッセージは最初のロック対象の強度で `cannot execute SELECT FOR UPDATE in a read-only transaction`（`NO KEY UPDATE` / `SHARE` / `KEY SHARE` も同形） | 【実機】`select 1 for update`、関数だけの FROM、`EXPLAIN select ... for update` は読み取り専用でもエラーにならない。`LIMIT 0` でもロック対象があればエラー。PostgreSQL の `ExecCheckXactReadOnly`（実行器の開始時）と同じ |
| RW-D13 | 40001 の 2 種の文言 | 00 §3.8 | 00 §3.8 のとおり。UPDATE / DELETE が `Updated` に当たると `concurrent update`、`Deleted` に当たると `concurrent delete`。**DELETE が（コミット済みの）更新に当たったときも `concurrent update`**、UPDATE が削除に当たったときは `concurrent delete`。`FOR` 句はどちらも `concurrent update` | 【実機】`rw-rr-conflict`（§7.2）の全組み合わせで確認。DETAIL・HINT は付かない |
| RW-D14 | B+Tree の待ち（D44 の具体化） | D44 | `IndexStore::insert` が `InsertOutcome::WaitFor(xid)` を返す。待つのは `executor::dml` の `insert_with_indexes` / `update_with_indexes`。待ちの前にラッチ・ピンはすべて外れている。再試行は「降下からやり直し」。一意検査の対象ページ（キーが入りうる最初の葉）の排他ラッチを検査から挿入まで保つ（§5.9、§6.9） | PostgreSQL の `_bt_doinsert` と同じ |
| RW-D15 | MultiXact ID の先取りの置き場所 | `create` の中／ラッチの外 | **ラッチの外**。`MultiXactTable::ensure_reserved()`（制御ファイルへ書く可能性がある）を、`lock_tuple` がページのラッチを取る前に呼ぶ。`create` はメモリ上の割り当てだけでラッチの下でも I/O をしない | M3 §5.9 / 00 §5.3 の「ページのラッチを持ったまま I/O しない」を守る |
| RW-D16 | カタログ行の更新競合 | `0A000`／XX000（PostgreSQL） | **XX000**。カタログの書き込みも `delete` / `update` を `wait: Some` で呼び、PostgreSQL の `simple_heap_update` / `simple_heap_delete` と同じ文言にする: `tuple concurrently updated`、`tuple concurrently deleted`、`tuple already updated by self`（§6.10。後ろ 2 つの文言は【記憶】: `PG:src/backend/access/heap/heapam.c` の `simple_heap_update` / `simple_heap_delete`） | 00 §3.8 の `tuple concurrently updated` を、削除側と自己更新側にも広げただけ |
| RW-D17 | SERIALIZABLE の文言 | M3 の実装（`transaction isolation level "serializable" is not supported yet` + HINT）／00 §3.8（`SERIALIZABLE isolation level is not supported yet`） | **00 §3.8 の文言**に統一する。`BEGIN` / `SET TRANSACTION` / `SET transaction_isolation` / `default_transaction_isolation` のどれでも 0A000 | 契約が文言を固定している。REPEATABLE READ の 0A000 は無くなる |
| RW-D18 | `UpdateOutcome` に `lockmode` を足す | 足さない（EPQ が自分で計算）／足す | **足す**（00 に無い追加）。`UpdateOutcome { result, new_tid, lockmode }`。`lockmode` は旧版と新しい行を比べてキー列が変わったかで決まる `Exclusive` / `NoKeyExclusive` | PostgreSQL の `heap_update` が `lockmode` を返すのと同じ。EPQ が最新版をロックする強度に使う |
| RW-D19 | RETURNING の範囲（D46） | M4 の欄だけ／仕上げる | 仕上げる範囲を §6.11 の表で固定する（FROM / USING の列、サブクエリ、システム列 `ctid` と `tableoid`、エラー文言、Extended Query の Describe） | M4 が終えた部分は省く |
| RW-D20 | `SET TRANSACTION` の規則と `COMMIT AND CHAIN` | — | 【実機】の規則を §6.7 に表で固定する。`AND CHAIN` は分離レベルと読み取り専用を引き継ぎ、トランザクションスナップショットは引き継がない | M3 §6.11.2 の補足 |

---

## 1. 範囲

### 1.1 この章で対応するもの

| 分類 | 内容 | WP |
|---|---|---|
| ヒープの更新競合 | `TableStore::delete` / `update` / `lock_tuple` の `l1:` / `l2:` / `l3:` ループ。`TmResult` の全分岐。待つ前にラッチもピンも外す。タプルロック（`LockTag::Tuple`）と XID の終了待ち。`crosscheck`（FK の RR 用） | RW-1 |
| xmax と infomask | `storage/heap/xmax.rs` に集約（00 規約 3）。単独のロック、MultiXact、更新者、読み取り（`XmaxState`）。`KEYS_UPDATED` と `TableDef.key_columns` | RW-1 |
| 行ロックの WAL | `HEAP_LOCK`（info `0x40`）の記録と REDO | RW-1 |
| 可視性 | `LOCK_ONLY` の xmax を無視、MultiXact の展開、`XMIN_FROZEN` の読み取り、`satisfies_update`（現在の状態基準）、一意検査用の `fetch_dirty` の規則 | RW-1 |
| MultiXact | `txn/multixact.rs`。メモリ上だけ。作成・展開・掃除・`next_multi` の先取り。起動後は旧 ID を終了扱い | RW-2 |
| 実行器 | UPDATE / DELETE の EPQ ループ（`RecheckSpec`、`LRecheck`）、`LockRows` ノード、`FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE [OF ...] [NOWAIT / SKIP LOCKED]` の構文・解析・計画、使えない組み合わせの 0A000 | RW-3 |
| Repeatable Read | トランザクションスナップショットの保持、40001（2 種の文言）、`SET TRANSACTION` の規則（25001）、`transaction_isolation` / `default_transaction_isolation`、READ ONLY との組み合わせ、カタログは最新（D12） | RW-4 |
| B+Tree の複数ライター | M4 の単一ライター前提の洗い出しと修正、`IndexStore::insert` の `InsertOutcome`、`insert_with_indexes` / `update_with_indexes` の待ちのループ | RW-5 |
| RETURNING の仕上げ | D46。同一コマンドでの二度更新（27000）、カタログ行の更新競合（XX000）を含む | RW-6 |
| M6 の拡張点 | `INSERT ... ON CONFLICT`、SAVEPOINT（`Snapshot::is_own` と `Transaction::owns_xid`）。M5 では作らず、触る箇所だけ §6.12 に挙げる | — |

### 1.2 この章で対応しないもの（0A000 を返す。黙って無視しない）

| 内容 | 返すもの |
|---|---|
| `SERIALIZABLE` | `0A000`（RW-D17） |
| 入れ子の `FOR UPDATE`（FROM の副問い合わせ・CTE・SubLink・集合演算の腕の中） | `0A000`（RW-D9） |
| 永続の MultiXact、更新者を含む MultiXact | なし（既知の差。§1.4） |
| `FOR UPDATE` と同時の `DECLARE CURSOR` / `WHERE CURRENT OF` | 構文自体が M5 に無い |
| `INSERT ... ON CONFLICT`、`MERGE` | 構文が M5 に無い（`0A000`） |
| `RETURNING old.* / new.*`（PostgreSQL 18） | 構文エラー（PostgreSQL 17 と同じ） |
| `RETURNING` のシステム列のうち `xmax` / `cmax` | `0A000`（§6.11） |
| 述語ロック・SSI・`pg_advisory_lock` | M6 以降 |

### 1.3 この章が保証すること

1. 同じ行を同時に更新・削除・ロックする複数のトランザクションは、PostgreSQL と同じ**順序で待ち合い**（FIFO。タプルロックで飢餓を防ぐ）、**更新の消失が起きない**（RC: 待った後に最新版で WHERE と SET を再評価。RR: 40001）。
2. 4 つの強度の行ロックの共存は、PostgreSQL の表（README.tuplock）と同じ。**例外は 2 つだけ**（§1.4）。
3. 行ロックの有無・ロック保持者の状態は、クラッシュ後に正しく扱われる（ロック保持者が終了していれば無効。MultiXact は全 ID が終了扱い）。
4. 一意インデックスへの同時挿入で、重複した行は入らない。他のトランザクションの未確定の項目に当たったら、その終了を待ってからやり直す。
5. B+Tree は、複数のライターが同時に挿入・分割しても壊れず、デッドロックしない（§6.9 の取得順序）。
6. `Repeatable Read` のトランザクションは、最初のスナップショットで一貫した読み取りをし、スナップショットより後にコミットされた更新・削除に当たる書き込みは 40001 になる。

### 1.4 既知の差（共有テストに入れない。`10-tests-plan.md` が集める）

| # | PostgreSQL | yuzhu M5 | 出る場面 |
|---|---|---|---|
| RW-K1 | 非キー列の UPDATE と KEY SHARE のロックは両立する（更新者 + ロック保持者を含む MultiXact） | **待つ**（D6、Q-014）。【実機】衝突行列の `(KEY SHARE 保持, 非キー UPDATE 要求)` と `(非キー UPDATE 中, KEY SHARE 要求)` の 2 セルだけが差（§3.4 の表） | FK の子の INSERT 中の親の非キー列 UPDATE。待たされる方向だけの差で、結果の値は変わらない |
| RW-K2 | 上の待ちが無いので起きないデッドロック | 起きうる。例: A が親行を KEY SHARE（FK 検査）→ B が親行の非キー UPDATE（A を待つ）→ A が同じ親行の非キー UPDATE（B を待つ）で 40P01 | FK と親行の更新が絡む並行処理 |
| RW-K3 | 入れ子の `FOR UPDATE`、FROM の副問い合わせへの `FOR UPDATE` が可 | 0A000（RW-D9） | 入れ子の副問い合わせでの行ロック |
| RW-K4 | `FOR UPDATE` が仮想リレーション（`pg_roles` は `pg_authid` の行をロック）に及ぶ | 仮想リレーションは**黙って無視**（`pg_locks` は PostgreSQL も関数のビューで無視。`pg_roles` だけ差） | 通常使わない |
| RW-K5 | 更新者の旧版に KEY SHARE を置くと旧版の xmax が MultiXact になる | 最新版にだけ置く（RW-D4）。観測できる差は無い（`pgrowlocks` の表示と xmax の値だけ） | `pgrowlocks`、`SELECT xmax` |
| RW-K6 | 行ロックのヒントビット（`XMAX_COMMITTED` など）を書く | 書かない（D13） | `pageinspect` |
| RW-K7 | RR の最初のスナップショットを Extended Query の Parse で取りうる | Bind の直前（Parse は取らず、Bind が RW-D10 の順序で取る） | RR の最初の文が Parse と Bind の間に他者のコミットを挟む場合 |
| RW-K8 | EPQ は計画木を再実行するので、外部結合の ON 条件も新しい版で再評価される（ON が偽になれば右側が NULL 拡張される） | **外部結合（LEFT / RIGHT）の ON 条件は再評価しない**（WHERE と内部結合の条件だけ。§6.5） | 外部結合を含む `UPDATE ... FROM` / `SELECT ... FOR UPDATE OF 外側の表` が、並行更新で ON の結果が変わる行に当たる場合 |
| RW-K9 | 行ロックの `xmax` 列の値が MultiXact の ID なら PostgreSQL の番号 | yuzhu の ID（`next_multi` から。起動のたびに増える） | `SELECT xmax FROM t` |

---

## 2. 構成

`★` は M5 で新規、`△` は M5 での変更。括弧内は持ち主。パスは `impl/rust/crates/yuzhu-core/src/` 以下。**ファイルの持ち主は 00 §8 に従う**。00 に載っていないファイルは §11 に挙げた。

```
yuzhu-core/src/
├── storage/
│   ├── mod.rs                 △ TableStore の delete / update / lock_tuple の署名、UpdateOutcome.lockmode、LockOutcome、RowWait、
│   │                            TmResult::expect_simple（RW-D16）、InsertOutcome、IndexStore::insert の署名
│   ├── heap_store.rs          △ HeapStore::{delete, update, lock_tuple} の l1 / l2 / l3 ループ、fetch_dirty、HeapStore::new の引数
│   ├── heap/
│   │   ├── xmax.rs            ★ xmax と infomask の組み立てと読み取りのすべて（00 規約 3）。純粋関数 plan_lock / plan_update など
│   │   ├── lock.rs            ★ タプルロックと XID 待ち（TupleLockGuard、wait_for_holders、条件付き待ち）
│   │   ├── visibility.rs      △ LOCK_ONLY の無視、XMIN_FROZEN、satisfies_update（xid_state 基準）、satisfies_dirty の規則
│   │   ├── wal.rs             △ HEAP_LOCK（info 0x40）の記録・REDO・describe
│   │   └── mod.rs             △ mod 宣言（F0 が口を作る）
│   └── btree/{insert,unique,search,split}.rs  △ 複数ライター対応（RW-5。M4 の持ち主 B1・B2 から RW が引き継ぐ）
├── txn/multixact.rs           ★ MultiXactTable
├── executor/
│   ├── dml.rs                 △ insert_with_indexes / update_with_indexes の待ちのループ、delete_row、crosscheck の通し
│   └── nodes/
│       ├── lock_rows.rs       ★ LockRows
│       ├── update.rs delete.rs △ EPQ ループ、RETURNING の行
│       ├── insert.rs          △ RETURNING
│       └── index_scan.rs seq_scan.rs △ FOR 句の下で ctid を出力する
├── planner/
│   ├── logical.rs physical.rs △ LockRows、LRowMark、LRecheck、RecheckSpec、RowMark
│   └── build.rs physicalize.rs △ FOR 句つき SELECT の組み立て順、Update / Delete の recheck、extra_cols
├── analyzer/{select,dml,bound}.rs  △ BoundSelect.locking、BoundLockingClause、FOR 句の検査と文言、RETURNING の解析
├── sql/{ast.rs, parser/select.rs, parser/dml.rs}  △ LockingClause（Statement の変種は F0。FOR 句の構文は RW）
├── catalog/{mod.rs, reader.rs}    （M4 のファイル。RW は直接編集しない）TableDef.key_columns / RelHandle.key_columns を F0 が足し、RW-1 が計算関数を書く（§11）
├── session/txn_ctl.rs         △ 分離レベル、スナップショットの保持、SET TRANSACTION の規則、TxnControl の実装
├── settings.rs                △ transaction_isolation / default_transaction_isolation（RR を許可）
└── util/sync_point.rs         ★ テスト用の同期点（Cargo の feature `sync-points` のときだけ有効。§7.5）
```

**依存の方向**（00 §2 の図に従う。この章で足す矢印だけ）:

- `storage::heap`（xmax・lock・wal を含む）は `txn::{lock, multixact, proc_array, clog}` に依存する（`LockManager`・`MultiXactTable`・`ProcArray` は `HeapStore::new` が `Arc` で受け取る）。`txn` は `storage` を `use` しない。
- `storage::heap::xmax` は**ページにも ProcArray にも触れない純粋関数**だけを持つ（`TupleHeader` と「実行中か」を返すクロージャを受け取る）。単体テストが容易で、VC・FK が読み取りの関数だけを呼べる。
- `storage::btree` は `LockManager` を使わない（D44）。

**この章の規約**（00 の規約 1〜8 に加えて）:

1. **待つ前に何も持たない**: `lock_tuple` / `delete` / `update` が `LockManager` の待ちに入るのは、ページのラッチもバッファのピンも外した後だけ。待った後は `read_buffer` からやり直し、行ポインタ番号でタプルを読み直す（RW-D5）。
2. **判定は現在の状態で**: `satisfies_update` / `fetch_dirty` / `plan_*` は `xid_state`（ProcArray → clog の順）で判定し、スナップショットを使わない（RW-D2）。可視性（`visible`）だけがスナップショットを使う。
3. **自分の XID の判定は `Snapshot::is_own` と `Transaction::owns_xid` を通す**（D2）。heap の関数が受け取る `WriteCtx.xid` との `==` を直接書く箇所は `xmax.rs` の 1 か所（`XmaxEnv::me`）に集める。
4. **結果は `TmResult` で返し、分岐は executor が行う**（D7）。heap は 40001・27000・55P03 を作らない（RW-D6）。作るのは「内部エラー」（`Invisible` を渡された、など）だけ。
5. **ヒントビットを書かない**（D13）。読むのは `XMAX_INVALID` と `XMIN_FROZEN`（両ビット）だけ。
6. 新しい WAL レコード（`HEAP_LOCK`）は M3 の規約 1 の形（検査 → ラッチ → `CriticalSection` → `page_mut` → `RecordBuilder` → `insert` → `set_lsn`）。

---

## 3. ディスク上の形式

タプルヘッダの形は M2 §3.5 のまま（35 バイト + NULL ビットマップ。`t_xmax` は u64）。M5 が新しく使うのは infomask / infomask2 のビットと、xmax に入れる値の規則だけ。**タプル本体のバイト列・ページの形・WAL のブロック参照の形は変えない**。制御ファイルの `next_multi` は 00 §3.7 のとおり（F0 が足す。§3.7）。

### 3.1 使うビット

| 名前 | 値 | 場所 | M5 の扱い |
|---|---|---|---|
| `HEAP_XMAX_KEYSHR_LOCK` | `0x0010` | infomask | ロックの強度（§3.3）。M2 の予約を使い始める |
| `HEAP_XMAX_EXCL_LOCK` | `0x0040` | infomask | 同上 |
| `HEAP_XMAX_SHR_LOCK` | `0x0050`（= EXCL \| KEYSHR） | infomask | `FOR SHARE` |
| `HEAP_XMAX_LOCK_ONLY` | `0x0080` | infomask | xmax がロックだけで、削除・更新ではない。**可視性は xmax を無視する** |
| `HEAP_XMIN_COMMITTED` | `0x0100` | infomask | 単独では書かず、読んでも無視（D13） |
| `HEAP_XMIN_INVALID` | `0x0200` | infomask | 同上 |
| `HEAP_XMIN_FROZEN` | `0x0300`（**両ビット**） | infomask | VC が書く。**読む**: xmin を「コミット済み・全員に見える」とする。片方だけ立っているものは無視 |
| `HEAP_XMAX_COMMITTED` | `0x0400` | infomask | 書かず、読んでも無視（D13） |
| `HEAP_XMAX_INVALID` | `0x0800` | infomask | **書く**（挿入時と、更新・削除・ロックの書き込みで xmax を置くとき外す）・**読む** |
| `HEAP_XMAX_IS_MULTI` | `0x1000` | infomask | xmax が `MultiXactId`。**必ず `LOCK_ONLY` と一緒**（D6。立っていて `LOCK_ONLY` がなければ `XX001`） |
| `HEAP_UPDATED` | `0x2000` | infomask | UPDATE の新しい版（M2 のまま） |
| `HEAP_KEYS_UPDATED` | `0x2000` | infomask2 | 更新者: キー列を変えた UPDATE と DELETE。ロック保持者: `FOR UPDATE`（強度 Exclusive）と、最強のメンバーが Exclusive の MultiXact |
| `HEAP_HOT_UPDATED` / `HEAP_ONLY_TUPLE` | `0x4000` / `0x8000` | infomask2 | 使わない（M2 のとおり。立っていたら破損） |

- 6 つのビット（`KEYSHR`・`EXCL`・`LOCK_ONLY`・`XMAX_COMMITTED`・`XMAX_INVALID`・`IS_MULTI`）を xmax の「状態ビット」と呼び、`xmax.rs` の定数 `XMAX_STATE_BITS`（M2 の `tuple.rs` の `HEAP_XMAX_LOCK_BITS` と同じ集合。ただし名前が誤解を招くので `xmax.rs` に別名で持つ）に集める。xmax を書き換えるときは infomask からこの集合を一括で外してから新しいビットを足す。
- `xmax == 0` と `XMAX_INVALID` は同じ意味（xmax なし）。`xmax != 0` で `XMAX_INVALID` が立っているものは、M2 の `TupleHeader::xmax_invalid` が「なし」と扱う。

### 3.2 xmax の状態（読み取り）

`xmax::decode_xmax(&TupleHeader) -> Result<XmaxState>`（§4.1）が返す状態と、見分け方:

| 状態 | 条件 | 意味 |
|---|---|---|
| `None` | `xmax == 0` または `XMAX_INVALID` | xmax なし |
| `Locker { xid, mode }` | `LOCK_ONLY` あり・`IS_MULTI` なし | 単独のロック保持者。`mode` は下の対応 |
| `LockerMulti(id)` | `LOCK_ONLY` あり・`IS_MULTI` あり | MultiXact（メンバーはメモリ上。§3.7）。infomask の強度ビットは「最強のメンバーの強度」の写し（読むときは使わず、`MultiXactTable::expand` の結果を使う） |
| `Updater { xid, keys_updated }` | `LOCK_ONLY` なし・`IS_MULTI` なし | 更新者または削除者。`keys_updated` は infomask2 の `KEYS_UPDATED` |
| （破損） | `IS_MULTI` あり・`LOCK_ONLY` なし／`LOCK_ONLY` あり・強度ビットがすべて 0 | `XX001` |

強度ビットと `LockTupleMode` の対応（00 §3.7 と同じ。【実機】で全部確認）:

| `LockTupleMode` | infomask の強度ビット | infomask2 の `KEYS_UPDATED` |
|---|---|---|
| `KeyShare`（`FOR KEY SHARE`） | `KEYSHR` `0x0010` | なし |
| `Share`（`FOR SHARE`） | `SHR` `0x0050` | なし |
| `NoKeyExclusive`（`FOR NO KEY UPDATE`） | `EXCL` `0x0040` | なし |
| `Exclusive`（`FOR UPDATE`） | `EXCL` `0x0040` | あり |

`LockTupleMode` の強さの全順序は `KeyShare < Share < NoKeyExclusive < Exclusive`（00 §4.3 の列挙の順）。強い方が、弱い方の衝突する相手をすべて含むので全順序で足りる（§3.4）。00 の定義に `PartialOrd, Ord` の derive を足す（§11）。

### 3.3 xmax に書く値（実機 17.11 の測定と yuzhu の値）

【実機】`pageinspect` の `heap_page_items` を、トランザクションの中（未コミット）と後で読んだ。**`HEAP_XMIN_COMMITTED`（`0x0100`）は PostgreSQL が読み取りのときに付けるヒントで、yuzhu は書かない**。**`HEAP_HOT_UPDATED` は PostgreSQL の HOT 更新が付けたもので、yuzhu は HOT を使わない**。この 2 つを除いた形が yuzhu の値。

| 操作（同じ XID `X`） | 旧版の xmax | 旧版の infomask の xmax 状態ビット | 旧版の infomask2 | 新しい版 |
|---|---|---|---|---|
| 挿入直後 | 0 | `0x0800`（XMAX_INVALID） | — | — |
| `FOR KEY SHARE` | `X` | `0x0090`（LOCK_ONLY + KEYSHR） | — | — |
| `FOR SHARE` | `X` | `0x00D0`（LOCK_ONLY + SHR） | — | — |
| `FOR NO KEY UPDATE` | `X` | `0x00C0`（LOCK_ONLY + EXCL） | — | — |
| `FOR UPDATE` | `X` | `0x00C0` | `KEYS_UPDATED` | — |
| 2 人が `FOR KEY SHARE` | MultiXactId | `0x1090`（IS_MULTI + LOCK_ONLY + KEYSHR） | — | — |
| 2 人が `FOR SHARE` | MultiXactId | `0x10D0` | — | — |
| `FOR KEY SHARE` + `FOR NO KEY UPDATE` | MultiXactId | `0x10C0`（最強のメンバー = NoKeyExclusive） | — | — |
| 非キー列の `UPDATE` | `X` | `0x0000`（ロック系のビットなし） | — | 新版: xmax = 0、infomask = `0x2800`（UPDATED + XMAX_INVALID） |
| キー列を変える `UPDATE` | `X` | `0x0000` | `KEYS_UPDATED` | 同上 |
| `DELETE` | `X` | `0x0000` | `KEYS_UPDATED` | — |
| 自分が `FOR SHARE` していた行を自分で `UPDATE` | `X`（変わらない） | `0x0000`（ロックのビットは消える） | 非キーなら無し | 新版: **PostgreSQL は xmax = `X` + KEYSHR + LOCK_ONLY（`0x2090`）を引き継ぐ。yuzhu は xmax = 0**（RW-D3） |
| 自分の `FOR KEY SHARE` → `FOR SHARE` / `FOR NO KEY UPDATE` / `FOR UPDATE` | `X`（変わらない） | 強い方のビットに書き換え（MultiXact にしない） | `FOR UPDATE` で `KEYS_UPDATED` | — |
| 自分の `FOR UPDATE` の後に `FOR KEY SHARE` | `X` | `FOR UPDATE` のまま | `KEYS_UPDATED` のまま | — |

- 同じ XID が自分のロックを強くするときは、PostgreSQL も MultiXact を作らずビットを書き換える【実機】。yuzhu も同じ（全順序の `max`）。
- 旧版の `t_ctid` は新しい版の TID、新しい版の `t_ctid` は自分自身（M2 のまま）。
- `t_cmax` はロック（`HEAP_LOCK`）では書き換えない（0 のまま）。更新・削除では `w.cid`。

### 3.4 行ロックの衝突表

README.tuplock（PG:src/backend/access/heap/README.tuplock）の表と、【実機】で測った行列（`SELECT ... FOR ...`、非キー列の `UPDATE`（`unk`）、キー列の `UPDATE`（`uk`）、`DELETE`（`del`）。行 = 先に持った操作が未コミット、列 = 後から要求。`WAIT` = 待つ）:

| 持つ \ 要求 | KEY SHARE | SHARE | NO KEY UPDATE | UPDATE | unk | uk | del |
|---|---|---|---|---|---|---|---|
| KEY SHARE | ok | ok | ok | WAIT | **ok（yuzhu: WAIT）** | WAIT | WAIT |
| SHARE | ok | ok | WAIT | WAIT | WAIT | WAIT | WAIT |
| NO KEY UPDATE | ok | WAIT | WAIT | WAIT | WAIT | WAIT | WAIT |
| UPDATE | WAIT | WAIT | WAIT | WAIT | WAIT | WAIT | WAIT |
| unk（非キー UPDATE 中） | **ok（yuzhu: WAIT）** | WAIT | WAIT | WAIT | WAIT | WAIT | WAIT |
| uk（キー UPDATE 中） | WAIT | WAIT | WAIT | WAIT | WAIT | WAIT | WAIT |
| del（DELETE 中） | WAIT | WAIT | WAIT | WAIT | WAIT | WAIT | WAIT |

- ロック同士（左上の 4×4）は README.tuplock の表と完全に一致。これが D6 の「ロック同士の共存は README.tuplock どおり」。
- 太字の 2 セルが RW-K1（Q-014 の差）。yuzhu では更新者がロック保持者と共存できない（更新者を MultiXact に入れない）ので、**更新者（非キー UPDATE を含む）とロック保持者は常に衝突する**。
- 衝突関数: `tuple_lock_conflicts(held: LockTupleMode, req: LockTupleMode) -> bool` = `(held, req)` が表の左上 4×4 の `WAIT`。`KeyShare` は `Exclusive` とだけ、`Share` は `NoKeyExclusive` と `Exclusive`、`NoKeyExclusive` は `Share` 以上、`Exclusive` は全部と衝突する。
- 更新・削除の要求は、同じ XID 以外の**すべての**保持者（更新者・どの強度のロック保持者も）と衝突する。

### 3.5 例: M2 §3.7 のタプルの xmax

M2 §3.7 のタプル（`(1, 'hello', NULL, true)`。`t_infomask = 0x0803`、`t_infomask2 = 4`、`xmin = 0x0100`）に、XID `0x0102` が次の操作をした後のヘッダのバイト（変わる所だけ。オフセットは M2 §3.5。リトルエンディアン）。この表はそのまま `xmax.rs` の単体テストの固定値にする。

| 操作 | xmax（8–15） | cmax（20–23） | infomask2（30–31） | infomask（32–33） | 新しい版の infomask |
|---|---|---|---|---|---|
| なし（挿入直後） | `00 ×8` | `00 00 00 00` | `04 00` | `03 08` | — |
| `FOR KEY SHARE` | `02 01 00 00 00 00 00 00` | `00 00 00 00` | `04 00` | `93 00`（`0x0093`） | — |
| `FOR SHARE` | `02 01 00 …` | `00 00 00 00` | `04 00` | `D3 00` | — |
| `FOR NO KEY UPDATE` | `02 01 00 …` | `00 00 00 00` | `04 00` | `C3 00` | — |
| `FOR UPDATE` | `02 01 00 …` | `00 00 00 00` | `04 20`（`0x2004`） | `C3 00` | — |
| MultiXactId 7（最強 `Share`） | `07 00 00 00 00 00 00 00` | `00 00 00 00` | `04 00` | `D3 10`（`0x10D3`） | — |
| MultiXactId 7（最強 `Exclusive`） | `07 00 …` | `00 00 00 00` | `04 20` | `C3 10` | — |
| 非キー列の `UPDATE`（cid 7） | `02 01 00 …` | `07 00 00 00` | `04 00` | `03 00` | `03 28`（`0x2803`） |
| キー列を変える `UPDATE` / `DELETE`（cid 7） | `02 01 00 …` | `07 00 00 00` | `04 20` | `03 00` | `03 28`（UPDATE のとき） |

計算: `0x0803 & !XMAX_STATE_BITS(= 0x1CD0) = 0x0003`、`FOR KEY SHARE` は `0x0003 | LOCK_ONLY 0x0080 | KEYSHR 0x0010 = 0x0093`、`FOR SHARE` は `0x0003 | 0x0080 | 0x0050 = 0x00D3`。

### 3.6 HEAP LOCK の WAL（HEAP rmgr、info `0x40`）

M3 §3.5 が予約した枠を使う（00 §3.3。`HEAP_LOCK: u8 = 0x40`。`INIT_PAGE`（`0x80`）は付けない）。ブロック参照は blk0 = 対象ページ（データなし。FPW の対象。M3 の `STANDARD`）。**メインデータは 24 バイト**（DELETE と同じ長さ。「変更後の値をそのまま載せる」形）:

| オフセット | 型 | フィールド | 内容 |
|---|---|---|---|
| 0 | u16 | `offnum` | 行ポインタ番号 |
| 2 | u16 | `infomask` | 変更後の値（xmax 状態ビット以外は元のまま） |
| 4 | u16 | `infomask2` | 変更後の値（natts と `KEYS_UPDATED`） |
| 6 | u8 | `flags` | 予約（0） |
| 7 | u8 | `lock_mode` | `LockTupleMode`（0 = KeyShare、1 = Share、2 = NoKeyExclusive、3 = Exclusive）。REDO は使わない（`wal::dump` 用）。MultiXact のときは最強のメンバーの強度 |
| 8 | u64 | `xmax` | 変更後の `t_xmax`（単独の XID、または `MultiXactId`） |
| 16 | u64 | 予約 | 0 |

- **REDO は値を書くだけ**: blk0 が `NeedsRedo` なら、`offnum` のタプルのヘッダの `xmax`・`infomask`・`infomask2` に書く（`cmax` と `ctid` は触らない）→ `set_lsn`。ロックマネージャにも MultiXact の表にも触れない（00 §3.3）。2 回 REDO しても同じ（冪等）。
- `lock_mode` と `flags` は読み手が検査しない（予約の値が 0 でなくても不正にしない）。`lock_mode > 3` は不正（`XX001`）。
- レコードの `xid` = `WriteCtx.xid`（ロックしたトランザクション）。コミット時の flush はこの XID のコミットレコードが担う（ロックだけでも XID を持つ。00 §5.1 d.3）。

**例**（`m3.md` §3.4 と同じ形。リレーション `(1663, 5, 16384)` のブロック 3、行ポインタ 5 番を XID `0x0102` が `FOR SHARE`、FPW なし）:

| オフセット | 長さ | 内容 |
|---|---|---|
| 0 | 4 | `tot_len = 80`（32 + 24 + 24） |
| 4 | 4 | CRC |
| 8 | 8 | `prev` |
| 16 | 8 | `xid = 0x0102` |
| 24 | 4 | `rmgr = 3`、`info = 0x40`、`nblocks = 1`、`reserved = 0` |
| 28 | 4 | `main_len = 24` |
| 32 | 24 | `block_id = 0`、`flags = 0x00`、`fork = 0`、0、`(1663, 5, 16384)`、`block = 3`、`data_len = 0` |
| 56 | 24 | メインデータ: `05 00`（offnum）、`D3 00`（infomask `0x00D3`）、`04 00`（infomask2）、`00`（flags）、`01`（lock_mode = Share）、`02 01 00 00 00 00 00 00`（xmax）、`00 ×8` |
| 80 | — | 次のレコードは +80 から（パディングなし） |

この例（CRC を除く）を `heap/wal.rs` の単体テストの固定値にする。`HEAP_LOCK` の REDO は「操作を実行してページの写しを取る → 同じ WAL を古いページに REDO → 一致」「2 回 REDO しても同じ」を M3 §7.4 の形で試験する。

### 3.7 MultiXact（メモリ上）と制御ファイル

- メンバー表は**ディスクに書かない**（D6、D36）。xmax に `IS_MULTI` と `MultiXactId`（u64）が残るだけ。
- 制御ファイルの `next_multi`（u64、オフセット 120。00 §3.7。`format_version = 3`。F0 が足す）は「**ここまでの ID は払い出した（または払い出しうる）**」という上限。単調に増え、下げない（停止チェックポイントでも下げない。`update_for_shutdown` も `next_multi` は下げない）。新しいクラスタでは 1（0 は無効な ID）。
- 起動時: `boundary = next = limit = control.next_multi`。**`boundary` 未満の ID はすべて「メンバー全員が終了」**（起動前の ID。クラッシュ後・再起動後に古い xmax が指す MultiXact は、メンバー全員が終了している）。
- 先取り: `MULTI_PREFETCH = 1024` 個ずつ `limit` を進め、`control.next_multi` に書いてから ID を払い出す（XID の `XID_PREFETCH` と同じ方式）。`limit - next < MULTI_RESERVE_LOW = 512` のときに `ensure_reserved()` が次の 1024 個を先に確保する（RW-D15。ラッチの外で呼ぶ）。
- 例: 新しいクラスタ（`next_multi = 1`）で起動 → `boundary = next = limit = 1`。最初の `lock_tuple` が `ensure_reserved()` で `limit = 1025` を制御ファイルに書く。ID 1, 2, … を払い出す。クラッシュして再起動すると `next_multi = 1025`、`boundary = next = limit = 1025`。xmax = 7 の MultiXact は 7 < 1025 なので `Dead`。以後の ID は 1025 から。

---

## 4. 共通の型（契約）

00 §4.3〜§4.6 のシグネチャは**変えない**。ここは、それを具体化し、00 に無いものを足す（追加は §11 に一覧）。

### 4.1 storage/heap/xmax.rs（RW-1。00 規約 3）

xmax と infomask の組み立て・読み取りは、このファイルの関数だけが行う。FK・B+Tree は読み取りの関数（`decode_xmax`、`lockers_all_finished`）だけを呼ぶ。**VC は読み取りに加えて、凍結と pruning のために `invalidate_xmax` と `with_xmin_frozen`（下）を呼ぶ**（VC が自分でビットを組み立てない。00 規約 3。レビュー対応 R-01）。

```rust
pub const HEAP_XMAX_SHR_LOCK: u16 = 0x0050;
pub const HEAP_XMIN_FROZEN: u16 = 0x0300;
/// KEYSHR | EXCL | LOCK_ONLY | XMAX_COMMITTED | XMAX_INVALID | IS_MULTI（§3.1）
pub const XMAX_STATE_BITS: u16 = 0x1CD0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XmaxState {
    None,
    Locker { xid: Xid, mode: LockTupleMode },
    LockerMulti(MultiXactId),
    Updater { xid: Xid, keys_updated: bool },
}
/// §3.2 の表。破損は XX001
pub fn decode_xmax(h: &TupleHeader) -> Result<XmaxState>;
pub fn xmin_is_frozen(h: &TupleHeader) -> bool;                     // infomask & 0x0300 == 0x0300、または xmin が BOOTSTRAP / FROZEN
pub fn xmax_is_lock_only(h: &TupleHeader) -> bool;                  // infomask & LOCK_ONLY != 0（単独・MultiXact とも）。可視性はこれが真なら xmax を無視する
/// xmax の生の値の組。ラッチを離す前後の変化の検出（§5.3）に使う
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawXmax { pub xmax: Xid, pub infomask: u16, pub infomask2: u16 }
impl RawXmax { pub fn of(h: &TupleHeader) -> RawXmax; }
/// (infomask に足すビット, infomask2 に足すビット)。§3.2 の表
pub fn lock_mode_bits(mode: LockTupleMode) -> (u16, u16);
pub fn mode_from_bits(infomask: u16, infomask2: u16) -> Result<LockTupleMode>;
/// §3.4。held と req の衝突
pub fn tuple_lock_conflicts(held: LockTupleMode, req: LockTupleMode) -> bool;

/// 判定に要る外部の状態。純粋関数にするためクロージャで受け取る（ProcArray と MultiXactTable に依存しない形でテストできる）
pub struct XmaxEnv<'a> {
    pub me: Xid,                                    // 自分の XID（WriteCtx.xid）。自分との比較はここだけ（RW 規約 3）
    pub is_running: &'a dyn Fn(Xid) -> bool,        // xid_state(x) == Running
    pub multi: &'a MultiXactTable,
}

/// 書き込む xmax の値一式。infomask / infomask2 は完成値（natts・HASNULL など xmax と無関係のビットは元のまま）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct XmaxWrite { pub xmax: Xid, pub cmax: Option<CommandId>, pub infomask: u16, pub infomask2: u16 }
impl XmaxWrite { pub fn apply(&self, h: &mut TupleHeader); }       // xmax / infomask / infomask2 と、Some なら cmax

pub enum LockDecision {
    /// 自分がすでに要求以上の強度を持つ。何も書かない（WAL も書かない）
    AlreadyHeld,
    Write(XmaxWrite),
    /// 衝突する保持者の XID（昇順・重複なし・空にならない。自分は含まない）。
    /// i_am_member: 自分がすでにこの行のロック保持者（MultiXact のメンバー）か。真ならタプルロックを取らずに待つ（§5.5）
    Wait { xids: Vec<Xid>, i_am_member: bool },
}
/// 行ロックの新しい xmax（PostgreSQL の compute_new_xmax_infomask の、更新者を含まない版）。決定表は §6.1
pub fn plan_lock(h: &TupleHeader, req: LockTupleMode, env: &XmaxEnv<'_>) -> Result<LockDecision>;

pub enum UpdateDecision { Proceed, Wait { xids: Vec<Xid>, i_am_member: bool } }
/// UPDATE / DELETE が xmax を置いてよいか。Locker / LockerMulti / 実行中の Updater は、自分以外の実行中の保持者すべてと衝突（D6）。
/// 自分が更新者の場合は呼ばれない（satisfies_update が SelfModified / Invisible にする）
pub fn plan_update(h: &TupleHeader, env: &XmaxEnv<'_>) -> Result<UpdateDecision>;

/// 更新者・削除者の xmax。ロック系のビットを外し、keys_updated なら KEYS_UPDATED、cmax = cid
pub fn stamp_updater(h: &TupleHeader, me: Xid, cid: CommandId, keys_updated: bool) -> XmaxWrite;

/// VC 用: 単独の Locker / LockerMulti のメンバーが全員終了しているか（終了していれば xmax を無効化してよい）。Updater / None は false
pub fn lockers_all_finished(h: &TupleHeader, env: &XmaxEnv<'_>) -> Result<bool>;

/// VC 用（凍結。03 §5.2.1）: xmax を無効化した完成値。xmax = Xid::INVALID、infomask の `XMAX_STATE_BITS`（KEYSHR | EXCL | LOCK_ONLY |
/// XMAX_COMMITTED | XMAX_INVALID | IS_MULTI）を落として `XMAX_INVALID` を立て、infomask2 の `KEYS_UPDATED` を落とす。cmax・ctid は変えない
/// （`XmaxWrite.cmax = None`）。呼んでよいのは、decode_xmax が Locker / LockerMulti（全員終了）/ 中断した Updater と判定したときだけ（判定は呼び出し側）
pub fn invalidate_xmax(h: &TupleHeader) -> XmaxWrite;
/// VC 用（凍結）: infomask に `HEAP_XMIN_FROZEN`（0x0300。両ビット）を立てた値。xmin の値は残す
pub fn with_xmin_frozen(infomask: u16) -> u16;

/// UPDATE が KEYS_UPDATED を立てるか。key_columns は TableDef.key_columns（attnum - 1 の順）。
/// 「保守的に変わったと判断する」: Datum の == が偽、または Float4 / Float8 のビット列が違えば変わった（PostgreSQL の datumIsEqual と同じ向き）
pub fn keys_changed(key_columns: &[bool], old: &[Datum], new: &[Datum]) -> bool;
```

### 4.2 storage（TableStore・TmResult・IndexStore の具体化）

```rust
// storage/mod.rs
// TmResult は M2 のまま（Ok / Invisible / SelfModified{cmax} / Updated{ctid, xmax} / Deleted{xmax} / BeingModified{xmax} / WouldBlock）。
// Updated / Deleted の xmax は更新者の XID。crosscheck の失敗は Updated { ctid: 自分の TID, xmax: Xid::INVALID }
// BeingModified は wait == None のときだけ返る（xmax は衝突する保持者の先頭）。WouldBlock は lock_tuple の Skip / Error だけ

#[derive(Clone, Copy, Debug)]
pub struct UpdateOutcome {
    pub result: TmResult,
    pub new_tid: Option<Tid>,
    /// 旧版と new_row を比べて決まる強度（キー列が変わったら Exclusive、変わらなければ NoKeyExclusive）。
    /// result が Updated のとき、EPQ が最新版をロックする強度に使う（RW-D18。00 に無い追加）
    pub lockmode: LockTupleMode,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowWait { Block, Skip, Error }          // 00 §4.5 のとおり。heap の中では Skip と Error は同じ（RW-D6）

#[derive(Clone, Debug)]
pub struct LockOutcome {
    pub result: TmResult,                        // Ok / SelfModified / Deleted / Updated / WouldBlock / Invisible
    /// follow_updates で最新版をたどってロックしたとき、その最新版（EPQ の入力）。それ以外は None
    pub latest: Option<HeapTuple>,
}

pub trait TableStore: Send + Sync + std::fmt::Debug {
    // 00 §4.5 の delete / update / lock_tuple。意味は §5.2〜§5.4
    // M4 §13.1 の fetch_dirty / tuple_state は §6.3 の規則に従って LOCK_ONLY と MultiXact を扱う
}

/// カタログの書き込み（simple_heap_update / simple_heap_delete 相当）が結果を検査する。RW-D16
#[derive(Clone, Copy, Debug)]
pub enum SimpleHeapOp { Update, Delete }
impl TmResult {
    /// Ok → Ok(())。Updated → XX000 "tuple concurrently updated"、Deleted → XX000 "tuple concurrently deleted"、
    /// SelfModified → XX000 "tuple already updated by self"、Invisible → XX000 "attempted to update|delete invisible tuple"、
    /// BeingModified / WouldBlock → Error::internal（wait: Some で呼べば起きない）
    pub fn expect_simple(self, op: SimpleHeapOp) -> Result<()>;
}

// IndexStore（00 §4.5。D44）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum InsertOutcome { Inserted, WaitFor(Xid) }       // WaitFor のときは何も挿入しておらず、ラッチもピンも外れている（M6 の ON CONFLICT が変種を足す。§6.12）
pub trait IndexStore: Send + Sync + std::fmt::Debug {
    fn insert(&self, w: &WriteCtx, index: &IndexHandle, key: &[Datum], tid: Tid, check: UniqueCheck<'_>) -> Result<InsertOutcome>;
}
```

- `HeapStore::new(pool, clog, wal, locks, procs, multixact, fsm)` は 00 §4.5 のとおり。heap は `locks: Arc<LockManager>`（タプルロックと XID 待ち）、`procs: Arc<ProcArray>`（`is_in_progress`）、`multixact: Arc<MultiXactTable>` を持つ。バックエンドは `WaitCtx.backend`（呼び出しごと）から取る。
- `TableDef.key_columns: Vec<bool>`、`RelHandle.key_columns: Arc<[bool]>`（00 §4.5）。M4 のファイルなので、F0 が欄を足し、計算関数 `TableDef::key_columns_from(indexes: &[Arc<IndexDef>], ncols: usize) -> Vec<bool>`（一意インデックス（`unique`）の `columns[].attnum` の和集合。M4 は部分・式インデックスが無い。DEFERRABLE も無い）を RW-1 が書く（§11）。

### 4.3 txn/multixact.rs（RW-2。00 §4.4 の具体化）

```rust
pub const MULTI_PREFETCH: u64 = 1024;
pub const MULTI_RESERVE_LOW: u64 = 512;

impl MultiXactTable {
    /// boundary = next = limit = control.next_multi（0 のときは 1。新しいクラスタ）
    pub fn new(control: Arc<ControlFileHandle>) -> MultiXactTable;      // Result を返さない（制御ファイルの値を読むだけ。01 §5.12 の組み立てが REDO の前に呼ぶ。R-06）
    /// limit - next < MULTI_RESERVE_LOW のとき limit を MULTI_PREFETCH 進めて control.next_multi に書く（I/O あり。ラッチの外で呼ぶ。RW-D15）。足りていれば何もしない
    pub fn ensure_reserved(&self) -> Result<()>;
    /// メンバーは xid ごとに 1 つ（同じ xid が複数なら強い方）に正規化し、xid 昇順に並べる。同じメンバー集合には同じ ID を返す。
    /// メモリ上の割り当てだけ（I/O なし。limit を超えたら Error::internal。ensure_reserved が防ぐ）
    pub fn create(&self, members: &[MultiMember]) -> Result<MultiXactId>;
    /// id < boundary、または表に無い（全員終了して掃除済み）→ Dead。あれば still_running で絞り、空なら Dead
    pub fn expand(&self, id: MultiXactId, still_running: &dyn Fn(Xid) -> bool) -> MultiXactState;
    /// コミット / アボートの手順 1 の最後（ProcArray から外した後）に呼ぶ。メンバーが全員終了したエントリを消す
    pub fn on_xact_end(&self, xid: Xid);
    pub fn boundary(&self) -> MultiXactId;
    pub fn len(&self) -> usize;                       // 表のエントリ数（掃除のテスト用）
}
```

`MultiMember` と `LockTupleMode` には `Hash`（`LockTupleMode` は `Ord` も）を derive する（00 §4.3・§4.4 への追加。§11 の依頼 4）。内部の形: `Mutex<Inner { next: u64, limit: u64, boundary: u64, map: HashMap<MultiXactId, Vec<MultiMember>>, by_set: HashMap<Vec<MultiMember>, MultiXactId>, by_xid: HashMap<Xid, Vec<MultiXactId>>, live: HashMap<MultiXactId, u32> }>`。`MultiXactTable` の Mutex は 00 §5.3 の葉のロック（ページのラッチの下で取ってよい。中で I/O をしない）。

### 4.4 解析・計画・実行（00 §4.6 の具体化）

```rust
// sql/ast.rs（FOR 句の構文は RW。Statement の変種は F0）
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum LockStrength { Update, NoKeyUpdate, Share, KeyShare }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum LockWaitPolicy { Block, NoWait, SkipLocked }
pub struct LockingClause { pub strength: LockStrength, pub of: Vec<ObjectName>, pub wait: LockWaitPolicy, pub span: Span }
// SelectStmt に locking: Vec<LockingClause>（00 §4.7）。LIMIT / OFFSET と FOR は順不同（PostgreSQL と同じ）

// analyzer/bound.rs（00 §4.6 のとおり。BoundLockingClause.rtes は解決済みの RteId の昇順・重複なし）
pub struct BoundLockingClause { pub strength: LockTupleMode, pub wait: RowWait, pub rtes: Vec<RteId> }
// BoundSelect.locking: Vec<BoundLockingClause>。同じ RTE を複数の句が指すときは、解析の最後に「RTE ごとに最強の強度・強い方の wait（`Error` > `Skip` > `Block`）」の 1 本にまとめる（PostgreSQL と同じ。§6.4）

// planner/logical.rs（00 §4.6 のとおり）
pub struct LRowMark { pub rel: RelHandle, pub tid: ColId, pub cols: Vec<ColId>, pub mode: LockTupleMode, pub wait: RowWait, pub table_name: String }   // table_name は追加（RW-D6）
pub struct LRecheck { pub qual: Option<LExpr>, pub new_values: Vec<LExpr>, pub extra_cols: Vec<ColId> }

// planner/physical.rs（00 §4.6 のとおり。RowMark に table_name を足す）
pub struct RecheckSpec { pub qual: Option<PhysExpr>, pub new_values: Vec<PhysExpr> }
pub struct RowMark { pub rel: RelHandle, pub tid_col: usize, pub cols: std::ops::Range<usize>, pub mode: LockTupleMode, pub wait: RowWait, pub table_name: String }
// PhysicalPlan::LockRows { input, marks: Vec<RowMark>, recheck: Option<PhysExpr> }
// PhysicalPlan::Update / Delete の recheck: Option<RecheckSpec>。入力の行の形（§6.5）:
//   Update: [対象表のユーザー列 n 個][ctid][代入式の値 k 個][extra 列 m 個]
//   Delete: [対象表のユーザー列 n 個][ctid][extra 列 m 個]
// RecheckSpec の式は、この入力の行の位置（PhysCol::Local(i)）で列を参照する。extra 列は recheck と RETURNING が使う、対象表以外の列

// executor/mod.rs（00 §4.6 のとおり。この章が使う ExecCtx の欄）
//   ctx.locks・ctx.procs（`dml::wait_for_xid` だけが使う。heap の待ちは heap 自身が持つ `Arc<LockManager>` で行う。R-05）、ctx.wait: WaitCtx、ctx.xact_snapshot: Option<&Snapshot>（RR のとき Some）、ctx.snapshot（文のスナップショット）
```

### 4.5 txn・session（RW-4。00 §4.3 の具体化）

```rust
// txn/mod.rs（LK が持つ。00 §4.3 のとおり）
//   IsolationLevel { ReadUncommitted, ReadCommitted, RepeatableRead }、uses_xact_snapshot()
//   Transaction.isolation / xact_snapshot: Option<RegisteredSnapshot> / owns_xid
//   Snapshot::is_own

// settings.rs（RW が transaction_isolation 系を持つ。00 §3.6）
pub enum Isolation { ReadUncommitted, ReadCommitted, RepeatableRead }      // M3 の ReadCommitted / ReadUncommitted に RepeatableRead を足す
impl Isolation {
    pub fn parse(setting: &str, value: &str) -> Result<Isolation>;          // "serializable" は 0A000（RW-D17）
    pub fn level(self) -> IsolationLevel;
    pub fn as_str(self) -> &'static str;                                    // "read uncommitted" | "read committed" | "repeatable read"
}

// session/txn_ctl.rs
/// RW-D11。この文は「最初のスナップショットを取った」ことになるか。BEGIN / COMMIT / ROLLBACK / SAVEPOINT 系、SET / RESET / SHOW、
/// LOCK TABLE、SET CONSTRAINTS、CHECKPOINT、LISTEN / NOTIFY / UNLISTEN、FETCH だけが false
pub fn statement_needs_snapshot(stmt: &Statement) -> bool;
impl Session {
    /// 00 §5.1 d の 4 を実装する。RC: take_snapshot（文の終わりまで登録）。RR: txn.xact_snapshot を（なければ）作り、その写しに
    /// curcid = txn.cid と own_xid = txn.xid を入れて返す。登録は xact_snapshot が保つ
    pub(crate) fn statement_snapshot(&mut self) -> Result<StatementSnapshot>;
    /// Extended Query の Parse が呼ぶ。スナップショットを要する文なら「最初のスナップショットを取った」印だけ立てる（RW-D11、RW-K7）
    pub fn note_snapshot_use(&mut self, stmt: &Statement);
}
/// 文の実行に使うスナップショット。RC は文の終わりまで登録を保つ `registered`、RR は `xact_snapshot` が保つので `None`
pub(crate) struct StatementSnapshot { pub snap: Snapshot, pub registered: Option<RegisteredSnapshot> }
```

### 4.6 executor::dml（RW-1・RW-5。M4 §10 の署名を拡張）

```rust
// executor/dml.rs
// 3 つの共有関数は INSERT・UPDATE・DELETE・COPY FROM・FK の RI の action（07）がすべて通る。署名は 07 §4.5 の依頼 R1 を受け入れて確定した
// （レビュー対応 R-05。00 §4.6 に同じ形を載せた）: M4 §10 の `update_with_indexes(ctx, rel, w, tid, new_row)` に `snap`・`old_row`・`crosscheck` を足す。
// crosscheck は**引数**のまま（ctx.ri から読まない）。通常の文は None、RI の action は ctx.ri.crosscheck() を自分で渡す。
/// ヒープに挿入し、各インデックスに項目を入れる。一意検査が WaitFor を返したら、ここで wait_for_xact して同じインデックスへの挿入をやり直す（D44）。
/// 戻り値・失敗時の扱いは M4 のまま。**成功したら `ri::after_insert(ctx, rel, tid, row)`（FK。`rel.ri.is_empty()` なら即戻り。07 §4.5）を呼ぶ**
pub fn insert_with_indexes(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, row: &[Datum]) -> Result<Tid>;
/// 旧版を更新し（wait = Some(&ctx.wait)）、新しい TID を全インデックスに入れる。result が Ok 以外ならインデックスには触れずそのまま返す。
/// snap: 更新対象の可視性に使うスナップショット（通常の文は ctx.snapshot（RR は xact_snapshot の写し）、RI の action は RI が取ったスナップショット）。
/// old_row: 旧版の行（`ri::after_update` が使う）。crosscheck は FK の RR 用（通常の UPDATE は None）。**result が Ok のとき `ri::after_update` を呼ぶ**
pub fn update_with_indexes(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid, old_row: &[Datum],
                           new_row: &[Datum], crosscheck: Option<&Snapshot>) -> Result<UpdateOutcome>;
/// delete（wait = Some(&ctx.wait)）。インデックスには触れない（M4 のとおり。項目の削除は VACUUM）。**result が Ok のとき `ri::after_delete` を呼ぶ**
pub fn delete_row(ctx: &mut ExecCtx<'_>, rel: &RelHandle, w: &WriteCtx, snap: &Snapshot, tid: Tid, old_row: &[Datum],
                  crosscheck: Option<&Snapshot>) -> Result<TmResult>;
/// 他トランザクションの終了を待つ（`ctx.locks.wait_for_xact(ctx.wait.backend, xid, &|x| ctx.procs.is_in_progress(x), ctx.wait.ctl)`）。executor が待つときはこれだけを使う。
/// `ctx.locks` と `ctx.procs`（00 §4.6 の `ExecCtx`）を使うのはこの関数だけで、heap（`TableStore`）は自分の `Arc<LockManager>` を持つ
pub fn wait_for_xid(ctx: &mut ExecCtx<'_>, xid: Xid) -> Result<()>;
```

---

## 5. 処理の流れ

### 5.1 `xid_state` と `satisfies_update`（現在の状態で判定する。RW-D2）

```text
xid_state(x) -> Running | Committed | Aborted               // HeapStore の private 関数
    if x == BOOTSTRAP || x == FROZEN:        return Committed
    if procs.is_in_progress(x):              return Running    // ★ 先に ProcArray を見る
    match clog.status(x):
        Committed  => Committed
        Aborted    => Aborted
        InProgress => Aborted                                   // ProcArray に無いのに clog が InProgress = クラッシュの残骸（M2 §6.6）
```

- **順序が要点**: コミットは「clog に Committed → ProcArray から外す」の順（00 §5.2 の手順 1）。だから「ProcArray に無い」を先に確かめれば、clog は確定している。逆順（clog を先に見る）だと、InProgress を見た直後にコミットされて ProcArray からも消え、「クラッシュの残骸」と誤認する。
- `xid_state` はページのラッチの下で呼ばれる。ProcArray と clog の内部ロックは 00 §5.3 の葉のロックで、ラッチの下で取ってよい。

```text
satisfies_update(h, self_tid, w, snap) -> TmResult            // 純粋に「いまの状態」で決める。xmax の中身の詳細は plan_* が見る
  1. xmin:
       xmin_is_frozen(h)                 → 次へ
       snap.is_own(h.xmin):  h.cmin >= snap.curcid → Invisible      // この文以降に自分が挿入した
       match xid_state(h.xmin): Committed → 次へ ／ Running | Aborted → Invisible
  2. xmax:  match decode_xmax(h)?
       None                              → Ok
       Updater { xid, .. }:
           snap.is_own(xid)              → h.cmax >= snap.curcid ? SelfModified{cmax} : Invisible   // 前のコマンドが更新済み
           match xid_state(xid): Running → BeingModified{xmax: xid}
                                 Committed → h.ctid != self_tid ? Updated{ctid: h.ctid, xmax: xid} : Deleted{xmax: xid}
                                 Aborted → Ok
       Locker { xid, .. }:
           snap.is_own(xid)              → BeingModified{xmax: xid}   // 自分のロック。plan_lock / plan_update が AlreadyHeld / Proceed にする
           xid_state(xid) == Running     → BeingModified{xmax: xid}
           それ以外（終了した保持者）    → Ok
       LockerMulti(id):  match multi.expand(id, running)
           Live(members) (空でない)      → BeingModified{xmax: members[0].xid}
           Dead                          → Ok
```

- `Invisible` は「このタプルは更新してはいけない」。呼び出し側（executor）は内部エラー（`attempted to update invisible tuple` など。XX000）にする。例外: `lock_tuple` が連鎖の途中で行ポインタが無かったときは `Deleted` を返す（§5.4）。
- 旧い M2 の実装（`committed_in_snapshot` でスナップショットの `xmax` / `xip` と比べていた）は、`xid_state` に置き換える。可視性（`visible`）はスナップショット基準のまま（§6.3）。

### 5.2 `HeapStore::delete`（`l1:` ループ）

```text
delete(rel, w, snap, tid, wait: Option<&WaitCtx>, crosscheck) -> Result<TmResult>
  tuplock = TupleLockGuard::none()                       // タプルロックを持ったら Drop で解放（エラー・中断でも）
  loop {                                                  // l1:
      buf = read_buffer(tag(rel.locator, tid.block))?; g = buf.write()?          // 排他ラッチ（ピンつき）
      h = locate(&g, tid)?  else return Invisible                                  // 行ポインタが NORMAL でない
      res = satisfies_update(&h, tid, w, snap)?
      match res {
          Ok => {}                                                                // 続けて書き込みへ
          BeingModified{..} => match plan_update(&h, &env)? {
              Proceed      => {}                                                  // 自分のロックだけ、または保持者が全員終了。続けて書き込みへ
              Wait{ xids, i_am_member } => {
                  let Some(wait) = wait else { return Ok(BeingModified{ xmax: xids[0] }) };
                  drop(g); drop(buf);                                             // ★ ラッチもピンも外す（規約 1）
                  wait_for_holders(wait, &mut tuplock, tid, rel, &xids, RowWait::Block, i_am_member)?;
                  continue;                                                       // ★ 取り直して最初から（RW-D5）
              }
          },
          other => return Ok(other),                                              // Invisible / SelfModified / Updated / Deleted
      }
      // ここまで来たら xmax を書いてよい（Ok、または自分のロックだけ / 保持者が終了）
      if let Some(cc) = crosscheck && !visible(&h, cc)? {                           // RR の FK 用。PostgreSQL と同じ
          return Ok(Updated{ ctid: h.ctid, xmax: Xid::INVALID });
      }
      cs = CriticalSection::enter(pool)
      write = stamp_updater(&h, w.xid, w.cid, /*keys_updated*/ true)                // DELETE は常に KEYS_UPDATED
      write.apply(&mut hdr); page_mut().item_mut(off) に書く
      page_mut().set_prunable(w.xid)                                                // VC-1 の `Page::set_prunable`（pd_prune_xid。03 §4.1。R-01）
      rec = RecordBuilder(Heap, HEAP_DELETE, w.xid); ... main = (offnum, infomask, infomask2, xmax, cmax)   // M3 §3.9 のまま
      end = wal.insert(rec)?; g.set_lsn(end)
      drop(g); drop(cs)
      drop(tuplock)                                                                 // ★ 印を付けた後にタプルロックを解放
      return Ok(TmResult::Ok)
  }
```

- `wait_for_holders`（§5.5）は、必要ならタプルロックを取り（取り済みなら取らない）、`xids` の各 XID の終了を待つ。キャンセル・`lock_timeout`・デッドロックは `?` で伝わり、`tuplock` の Drop がタプルロックを解放する。
- `Wait` の後の `continue` では、**待った XID は終了している**ので、次の周回の `satisfies_update` は `Ok`（中断・ロックだけ）か `Updated` / `Deleted`（コミット）を返す。別の誰かが先に印を付けていれば、また `BeingModified` になって待つ（RW-D5）。
- `Updated` / `Deleted` が返ったとき、RC の executor は EPQ（§5.6）、RR は 40001（§5.8）。
- DELETE のメインデータは M3 §3.9 のまま（変更後の `infomask`・`infomask2`・`xmax`・`cmax` を載せるので、ロック系ビットの消去も REDO で再現される）。

### 5.3 `HeapStore::update`（`l2:` ループ）

```text
update(rel, w, snap, tid, new_row, wait, crosscheck) -> Result<UpdateOutcome>
  data = form_tuple(&rel.desc, new_row, w, TupleFlags{updated: true})?               // 失敗しうる処理はここまで（M3 §5.1）
  tuplock = TupleLockGuard::none()
  loop {                                                                              // l2:
      buf = read_buffer(old block); g = buf.write()?
      h = locate(&g, tid)?  else return outcome(Invisible)
      keys_updated = keys_changed(&rel.key_columns, &old_key_values(&g, tid, &rel.desc)?, new_row)   // 旧タプルから key 列だけデコード
      lockmode = if keys_updated { Exclusive } else { NoKeyExclusive }
      res = satisfies_update(&h, tid, w, snap)?
      (res が Ok / Proceed になるまで §5.2 と同じ分岐。Wait のとき: ラッチ・ピンを外す → wait_for_holders → continue)
      crosscheck の検査（§5.2 と同じ）。失敗は outcome(Updated{ctid: h.ctid, xmax: INVALID}, lockmode)
      if hio::fits(g.page(), data.len()):                                              // 同じページに入る（M3 §5.1 の手順 3）
          cs: 新版を place_in_page → 旧版に stamp_updater(.., keys_updated) と ctid = 新版を書く
              set_prunable → HEAP_UPDATE（SAME_PAGE。M3 §3.9 のまま）→ set_lsn
          drop(tuplock); return Ok{ new_tid, lockmode }
      else:                                                                            // 別のページ（M3 §5.1 の手順 4〜7）
          seen = RawXmax::of(&h)                                                       // (xmax, infomask, infomask2)
          drop(g)                                                                      // 新版の置き場所を決める間はラッチを持たない
          target = hio::find_target(rel, data.len())?                                  // ピンだけ返る
          latch old と target をブロック番号の小さい順に排他ラッチ
          h2 = locate(&g_old, tid)?  else { release; continue }
          if RawXmax::of(&h2) != seen { release both; continue }                       // ★ 待たずにやり直す（M3 の「内部エラー」を置き換える）
          if target に data が入らない { release both; continue }
          cs: 新版を target に置く → 旧版に stamp_updater と ctid = 新版 → 旧版のページに set_prunable → HEAP_UPDATE（blk0 = target、blk1 = 旧） → 両方に set_lsn
          drop(tuplock); return Ok{ new_tid, lockmode }
  }
```

- **2 つのラッチの間に、他者が旧版の xmax を書き換える**ことは、M5 では起きる（複数ライター）。M3 §5.1 の手順 6 が「単一ライターなので変わらない。変わっていたら内部エラー」としていた箇所は、`RawXmax` の比較とやり直しに置き換える。比べるのは xmax・infomask・infomask2 の 3 つ（`satisfies_update` が `Ok` と判定したときの値）。**この比較に待ちは要らない**（待つのは先頭に戻ったときの `satisfies_update`）。
- `old_key_values` は旧タプルのバイト列から key 列だけを `deform` する（`key_columns` がすべて false なら何もしない）。`UpdateOutcome.lockmode` は result が `Ok` 以外でも埋める（EPQ が使う）。
- 新しい版は xmax = 0・`XMAX_INVALID`（`form_tuple` のとおり）。RW-D3。

### 5.4 `HeapStore::lock_tuple`（`l3:` ループ）

```text
lock_tuple(rel, w, snap, tid, mode, policy, follow_updates, wait) -> Result<LockOutcome>
  tuplock = TupleLockGuard::none();  cur = tid;  prior_xmax = None;  traversed = false;  all_nonkey = true
  loop {                                                                              // l3:
      multixact.ensure_reserved()?                                                    // ★ ラッチの外（RW-D15）
      buf = read_buffer(cur.block); g = buf.write()?
      h = locate(&g, cur)? else return Ok(traversed ? Deleted{xmax: prior_xmax} : Invisible)
      if let Some(px) = prior_xmax && !xmin_is_frozen(&h) && h.xmin != px {           // 行ポインタが再利用された: 連鎖の途中で消えた
          return Ok(Deleted{ xmax: px }) }
      res = satisfies_update(&h, cur, w, snap)?
      match res {
        Invisible              => return Invisible
        SelfModified{cmax}     => return SelfModified{cmax}                           // 呼び出し側（LockRows）は行を飛ばす
        Deleted{xmax}          => return Deleted{xmax}
        Updated{ctid, xmax}    => {                                                   // コミット済みの更新者
            let keyshare_nonkey = mode == KeyShare && !h.keys_updated();              // ★ RW-D4
            if !(follow_updates || keyshare_nonkey) { return Updated{ctid, xmax} }    // RR など。呼び出し側が 40001
            drop(g); drop(buf)
            prior_xmax = Some(xmax); cur = ctid; traversed = true
            all_nonkey &= keyshare_nonkey                                              // 途中でキー更新を踏んだら、latest を返す（EPQ の対象）
            continue                                                                   // 次の版へ（ラッチは持ち越さない）
        }
        Ok | BeingModified     => {
            match plan_lock(&h, mode, &env)? {
              AlreadyHeld      => (何も書かない) → 完了へ
              Write(w)         => cs: w.apply(hdr) を書く → set_prunable は呼ばない → HEAP_LOCK → set_lsn → 完了へ
              Wait{ xids, i_am_member } => {
                  drop(g); drop(buf)                                                  // ★ 待つ前に何も持たない
                  match wait_for_holders(wait, &mut tuplock, cur, rel, &xids, policy, i_am_member)? {
                      WouldBlock => return WouldBlock,     // Skip / Error: タプルロックか XID の待ちが条件付きで取れなかった（RW-D6）
                      Waited     => continue,              // 取れた（Block は待った）。取り直して最初から
                  }
              }
            }
        }
      }
      // 完了: g を解放してから
      drop(tuplock)
      return Ok(LockOutcome{ result: Ok,
                             latest: if traversed && !(mode == KeyShare && all_nonkey) { Some(decode(h, cur)) } else { None } })
  }
```

- `latest` に入れるのは、**ロックした最新版**（`HeapTuple`。ユーザー列・`xmin`・`xmax`・`cmin`・`cmax`・TID）。連鎖をたどったのが `KeyShare` で、たどった更新がすべて非キー更新（`all_nonkey`。RW-D4 の経路）なら `None` を返し、呼び出し側は**元の（旧版の）データをそのまま使う**（ロックは最新版に置いてある）。
- **`KeyShare` と非キー更新の経路（RW-D4）**: 連鎖の各版が「コミット済みの更新者で `KEYS_UPDATED` が無い」間はたどり続け、最後の版（xmax なし・ロックだけ・中断した更新者）にロックを置く。途中の版が `KEYS_UPDATED` 付きの更新者（キー列を変えた UPDATE）なら、その版で `Updated`（`follow_updates` でなければそのまま返す）。連鎖の先が DELETE なら `Deleted`。【実機】`update v; delete` の後の KEY SHARE は 40001、`update v; update v`（非キー 2 回）の後の KEY SHARE は旧版を返して成功、`update v; update id` の後は 40001。
- 連鎖の最新版が**更新者として実行中**なら、`satisfies_update` が `BeingModified` を返し、`plan_lock` は `Wait`（D6: ロック要求は更新者と常に衝突）。待った後に、コミットなら連鎖をさらにたどり、中断ならその版にロックを置く。
- `Invisible` は、最初の版（`traversed == false`）に対してだけ呼び出し側に返る。連鎖の途中は上のとおり `Deleted`。
- 同じバックエンドが連鎖の途中で自分の XID の版に出会うことは、「自分が更新した版」＝ `SelfModified` / `Invisible`（§5.1）で処理される。
- ロックだけの書き込みは `HEAP_LOCK`（§3.6）を 1 つ書く。**XID の割り当ては呼び出し側**（00 §5.1 d.3）で済んでいる（`w.xid` が有効）。

### 5.5 待ち: タプルロックと XID の終了待ち（`heap/lock.rs`）

README.tuplock のとおり、2 段で待つ（D7）: 同じ行を待つ者が複数いるとき、先着順に XID を待たせて飢餓を防ぐ。

```text
wait_for_holders(wait: &WaitCtx, tuplock: &mut TupleLockGuard, tid, rel, xids: &[Xid], policy, i_am_member: bool) -> Result<WaitResult>
    // 呼び出し時点: ラッチもピンも持っていない（規約 1。デバッグビルドは buffer/track.rs で検査する）
    if !i_am_member && !tuplock.held():                          // 自分がすでにロックを持つ行（昇格）ではタプルロックを取らない（README.tuplock。デッドロックの回避）
        tag = LockTag::Tuple{ db: rel.locator.db_oid, rel: rel.oid, block: tid.block, offset: tid.offset }
        match policy:
            Block        => locks.acquire(wait.backend, tag, Exclusive, Transaction, wait.ctl)?  ; tuplock.set(tag)
            Skip | Error => if !locks.try_acquire(wait.backend, tag, Exclusive, Transaction) { return WouldBlock } ; tuplock.set(tag)
    for x in xids:
        match policy:
            Block        => locks.wait_for_xact(wait.backend, x, &|x| procs.is_in_progress(x), wait.ctl)?
            Skip | Error => if !conditional_wait_for_xact(x) { return WouldBlock }
    return Waited

conditional_wait_for_xact(x) -> bool:                            // PostgreSQL の ConditionalXactLockTableWait
    loop { if !procs.is_in_progress(x) { return true }
           if locks.try_acquire(me, TransactionId(x), Share, Transaction) { locks.release(me, TransactionId(x), Share) ; continue }   // 取れた = 終了した（またはしている最中）。もう一度確かめる
           else { return false } }
```

- **`i_am_member`**: `plan_lock` / `plan_update` が `Wait { xids, i_am_member }` で返す（`LockerMulti` に自分が含まれるときに真。単独の `Locker` が自分なら待たないので真にはならない）。`WaitResult` は `Waited`（待った、または条件付きで取れた。呼び出し側は取り直してやり直す）と `WouldBlock`（Skip / Error で取れなかった）。
- `TupleLockGuard`: `Drop` で `locks.release(backend, tag, Exclusive)`。`delete` / `update` / `lock_tuple` の関数の最後（印を付けた後）に明示的に落とす。エラー（キャンセル・タイムアウト・デッドロック）の `?` でも Drop される。各バックエンドが同時に持つタプルロックは高々 1 つ。
- 【実機】3 人（A が更新中、B と C が同じ行を更新）の `pg_locks`: A は `transactionid` の ExclusiveLock、**B は `tuple` の ExclusiveLock（granted）と A の `transactionid` の ShareLock（待ち）、C は `tuple` の ExclusiveLock（待ち）**。A がコミットすると B が先に通り、B が印を付けてタプルロックを解放すると C は B の XID の ShareLock で待つ（`rw-tuple-queue` §7.2）。
- `lock_timeout` は `WaitCtl.lock_timeout`（1 回のロック待ちに対する時間）。行ロック待ちにも効く【実機】（`canceling statement due to lock timeout`）。

### 5.6 `Update` / `Delete` ノードの EvalPlanQual ループ（D8、RW-D7）

executor は `TmResult` を分岐する（D7）。入力行 `row` は §4.4 の形。

```text
apply_update(ctx, row, tid, new_row)          // Update ノードが入力行ごとに呼ぶ。Delete は new_row がなく delete_row を呼ぶ以外は同じ
  w = ctx.write_ctx()?;  cur_tid = tid;  new_row = new_row (enforce_constraints 済み)
  rr = ctx.xact_snapshot.is_some()
  loop {
      ctx.check_interrupts()?
      out = update_with_indexes(ctx, &rel, &w, ctx.snapshot, cur_tid, &old_user_cols(&row, n), &new_row, None)?
      match out.result {
        Ok            => { count += 1; RETURNING の行を作る（§6.11）; return }
        SelfModified{cmax} => { if cmax != w.cid { return Err(already_modified("updated" | "deleted")) /* 27000 */ } ; return /* 同じコマンドで更新済み。黙って飛ばす */ }
        Deleted{..}   => { if rr { return Err(40001 "could not serialize access due to concurrent delete") } ; return /* RC: 飛ばす */ }
        Updated{..}   => {
            if rr { return Err(40001 "could not serialize access due to concurrent update") }
            // RC: 最新版を探してロックし、再評価する
            lk = ctx.storage.lock_tuple(&rel, &w, ctx.snapshot, cur_tid, out.lockmode, RowWait::Block, /*follow*/ true, &ctx.wait)?
            match lk.result {
              Ok => {
                  latest = lk.latest.expect("follow_updates で Ok なら最新版がある")           // KeyShare の経路は FOR KEY SHARE だけなので、ここでは None にならない
                  if let Some(spec) = &recheck {
                      r2 = row.clone();  r2[0..n] = latest.row;  r2[n] = Tid(latest.tid)       // 対象表の部分だけ最新版に差し替え。他の列（結合相手）は元のまま
                      if let Some(q) = &spec.qual && !eval_pred(q, &r2, ctx)?.unwrap_or(false) { return /* 条件を満たさない。飛ばす */ }
                      if Update: new_row = latest.row に assigned の位置だけ eval(spec.new_values[i], &r2) で置き換えたもの;  enforce_constraints(new_row)?
                  } else if Update { /* recheck なし = 代入が定数だけ */ new_row = latest.row に定数の代入を当てたもの }
                  cur_tid = latest.tid;  continue                                              // 自分がロック済みなので、今度は update が Ok になる
              }
              Deleted{..}     => return /* RC: 最新版が削除された。飛ばす */
              SelfModified{cmax} => (上の SelfModified と同じ)
              _ => return Err(internal)
            }
        }
        Invisible     => return Err(internal "attempted to update invisible tuple")   // XX000
        BeingModified | WouldBlock => return Err(internal)                            // wait = Some なので起きない
      }
  }
```

- **再評価するのは「元の文の WHERE 句全体（と結合条件）」と「SET 式」**。`RecheckSpec.qual` と `new_values` が、元の Bound の式から作られた木（書き換え後ではない）。入力行の**他テーブルの列は元のまま**（RW-D7 (1)）。【実機】`UPDATE p ... FROM q WHERE ... AND q.k = 1` の最中に `q` の行が別トランザクションで変わっても、`q` は元の版のままで再評価され、`RETURNING q.k` も元の値。
- **SubLink を含む `qual` / `new_values`**: `eval_pred` が `SubPlan` を評価する。**相関サブクエリは差し替えた行 `r2` の値を引数に、文のスナップショット（`ctx.snapshot`）で再実行**する（`Rescan` 戦略は引数が変わるので自動的に作り直し）。**非相関サブクエリ（`InitOnce`・`Hashed`）は文の最初に評価した結果を使い回す**（再実行しない）。【実機】`rw-epq-initplan`: `WHERE id = 1 AND v = (SELECT min(v) FROM ip)` の最中に別トランザクションが `v` を 0 にしてコミットすると、`min` は元の 1 のままなので 0 行（再実行すると 1 行になる）。
- **`ExternParam`（`$n`。D45）を含む式**は `ctx.bind_params` で評価するので、再評価でも元と同じ値になる。
- **元の走査で拾わなかった行は拾わない**（RW-D7 (4)）。【実機】`rw-epq-nopickup`: `UPDATE ... WHERE v < 5` の最中に別トランザクションが `id = 3` を `v = 30 → 1` に更新しても、B は `id = 3` を更新しない。
- **27000**: `cmax != w.cid`（同じコマンドではなく後のコマンドでの更新）。M5 では起きない（トリガー・関数内更新が無い）が、PostgreSQL の分岐を残す。HINT `Consider using an AFTER trigger instead of a BEFORE trigger to propagate changes to other rows.`（M2 の `already_modified` のとおり）。
- `UPDATE ... FROM` で 1 つの対象行に複数の入力行が当たる場合、2 回目以降は `SelfModified`（`cmax == w.cid`）で黙って無視される（PostgreSQL と同じ。EPQ は通らない）。
- **結果の行（RETURNING）は再計算後の新しい行**から作る。

**`TmResult` と呼び出し側の動作**（heap は分岐を作らず、executor が決める。D7）:

| heap の結果 | `UPDATE` / `DELETE`（§5.6） | `SELECT ... FOR`（§5.7） |
|---|---|---|
| `Ok` | 行数 +1、RETURNING の行 | ロック済み。`latest` があれば EPQ |
| `Invisible` | XX000 `attempted to update invisible tuple`（`delete` は `delete`）。起きない | XX000 `attempted to lock invisible tuple` |
| `SelfModified { cmax }` | `cmax == w.cid` なら黙って飛ばす、そうでなければ 27000 | 行を飛ばす（同じコマンドまたは後のコマンドで更新済み） |
| `Updated { ctid, xmax }` | RC: `lock_tuple(follow)` → 再評価 → やり直し。**RR: 40001 `concurrent update`** | RC: `follow_updates` が `lock_tuple` の中で処理（呼び出し側には `Ok + latest`）。**RR: 40001 `concurrent update`** |
| `Deleted { xmax }` | RC: 飛ばす。**RR: 40001 `concurrent delete`** | RC: 行を飛ばす。**RR: 40001 `concurrent update`**（文言は update のまま。RW-D13） |
| `BeingModified` | 起きない（`wait = Some`） | 起きない |
| `WouldBlock` | 起きない | `Skip`: 行を飛ばす。`Error`: 55P03 `could not obtain lock on row in relation "t"` |

### 5.7 `LockRows` ノード（D8）

```text
LockRowsExec::next(ctx):
  'next_row: loop {
      row = input.next(ctx)?  or return None
      ctx.check_interrupts()?
      let mut replaced: Vec<(mark_index, HeapTuple)> = []
      'marks: for (i, m) in marks.iter().enumerate():
          tid = match row[m.tid_col] { Datum::Tid(t) => t, Null => continue /* 外部結合の NULL 側など */ }
          lk = ctx.storage.lock_tuple(&m.rel, &w, ctx.snapshot, tid, m.mode, m.wait, /*follow*/ !rr, &ctx.wait)?
          match lk.result:
            Ok            => if let Some(t) = lk.latest { replaced.push((i, t)) }
            WouldBlock    => if m.wait == Error { return Err(55P03 "could not obtain lock on row in relation \"{m.table_name}\"") }
                             else /* Skip */ { continue 'next_row }                                   // この行を捨てる（ほかの mark で取ったロックは残る）
            Deleted{..}   => if rr { return Err(40001 "could not serialize access due to concurrent update") } else { continue 'next_row }
            Updated{..}   => /* follow = false のとき = RR */ return Err(40001 "could not serialize access due to concurrent update")
            SelfModified{..} => continue 'next_row           // 同じコマンドまたは後のコマンドで更新済み。飛ばす（PostgreSQL の nodeLockRows と同じ）
            Invisible     => return Err(internal "attempted to lock invisible tuple")
      if replaced.is_empty(): return Some(row)
      // EPQ: 最新版に差し替えた行で WHERE（と結合条件）を再評価する
      r2 = row.clone();  for (i, t) in replaced { r2[marks[i].cols] = t.row;  r2[marks[i].tid_col] = Tid(t.tid) }
      if let Some(q) = &recheck && !eval_pred(q, &r2, ctx)? { continue }                               // 条件を満たさない。飛ばす（ロックは残る）
      return Some(r2)                                                                                  // 新しい版の行を返す
  }
```

- **強度ごとの要点**（【実機】で確認。`rw-nowait-skip`、`rw-multixact-share`、`rw-rr-conflict`）:
  - `FOR UPDATE NOWAIT` が他者の `FOR UPDATE` 行に当たる → 55P03。他者の `FOR SHARE` 行への `FOR SHARE NOWAIT` は成功（共有）、`FOR UPDATE NOWAIT` は 55P03。
  - `SKIP LOCKED`: ロックされた行が結果から消える。`ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED` は、先頭がロック中なら次の行を返す（LockRows が Limit の下にあるため）。`FOR SHARE SKIP LOCKED` は他者の `FOR SHARE` 行を飛ばさない。
  - 普通の SELECT（ロックなし）は行ロックに止められない。
  - RR で `FOR UPDATE SKIP LOCKED` / `NOWAIT` も、コミット済みの並行更新には 40001（待たない・飛ばさない）。
- **`NOWAIT` / `SKIP LOCKED` はリレーションロックの待ちには効かない**（行ロックだけ）【実機】。リレーションロックは文の最初に LK が普通に待つ。
- **行ロックを取った後で再評価に落ちた行のロックは残る**（PostgreSQL と同じ）。トランザクションの終了で解放される。
- 並べ替えは LockRows の**下**（Sort が先）なので、EPQ で値が変わった行は、元の並びの位置のまま新しい値で返る【実機】`rw-epq-for-update`: `ORDER BY v DESC` の最中に `v = 3 → 0` になった行は、`4, 0(←id=3), 2, 1` の位置のまま出る。

### 5.8 Repeatable Read の文の実行（RW-4）

00 §5.1 の d を、RR について次のように具体化する（RC は 00 のとおり）。

```text
文 s の実行（スナップショットを要する文 = statement_needs_snapshot(s)）:
  0. [RR で txn.xact_snapshot == None、かつ s がスナップショットを要する] ★ ロック取得の前に:
        txn.xact_snapshot = Some(mgr.take_snapshot(txn.xid, txn.cid))          // RW-D10。これが「最初のスナップショット」
     first_snapshot_set = true                                                   // RC・RR とも（RW-D11）。25001 判定に使う
  1〜3. 00 §5.1 d のとおり（リレーションロック、XID の割り当て）
  4. 実行用のスナップショット:
        RC: mgr.take_snapshot(txn.xid, txn.cid)（文の終わりまで登録）
        RR: base = txn.xact_snapshot の中の Snapshot;  snap = Snapshot{ curcid: txn.cid, own_xid: txn.xid, ..base.clone() }
            ctx.xact_snapshot = Some(&snap)                                      // crosscheck と 40001 の判定に使う
  5. カタログ: 常に「文ごとに取り直す最新のスナップショット」（mgr.take_snapshot(txn.xid, txn.cid)。**登録する（短命。文の終わりまで）**。D12、LK-D9、VC-D23。レビュー対応 R-02）
  ...
```

- **RR の `Updated` / `Deleted` は、必ず 40001**。スナップショットで見えた行が、スナップショットより後にコミットされた更新・削除に当たったという意味で、スナップショットとの比較は要らない（見えていた行の xmax がコミット済みなら、そのコミットは必ずスナップショットより後。`m5c` §7.1）。
- 相手が実行中ならその終了を待ち（RW-D5 のループ）、**アボートしたら続行**、コミットしたら 40001（【実機】`m5c` §2 の #4b、#4c）。相手が**ロックだけでコミット**したなら続行する（xmax が `LOCK_ONLY` でコミット済み＝ロックは終わったので `satisfies_update` が `Ok`）。
- **挿入（ファントム）はエラーにならない**。読むだけ・挿入だけの RR は失敗しない（【実機】`m5c` §2 の #4g、#13）。
- RR のスナップショットの登録（`RegisteredSnapshot`）は、トランザクションが終わるまで保つ（D11）。`oldest_xmin()` を止める（【実機】`m5c` §2 の #11）。
- `xact_snapshot` の登録を外すのは、コミット / アボートの最後（00 §5.2 の手順 5）。`COMMIT AND CHAIN` で次のトランザクションに引き継ぐのは分離レベル・読み取り専用・DEFERRABLE だけで、**スナップショットは引き継がない**（新しいトランザクションの最初の文で取り直す）。
- `RW-D10` の「ロックの前」は、RR の**最初のスナップショットだけ**（【実機】`rw-rr-first-snapshot`: RR の最初の `SELECT` / `UPDATE` が `LOCK TABLE` の待ちに入り、相手のコミットを待った後も、相手の挿入した行は見えない。RC は見える。`LOCK TABLE` を最初の文にすれば、`LOCK TABLE` がスナップショットを取らないので、その後の文のスナップショットは待ちの後になる）。

### 5.9 一意検査の待ち（`insert_with_indexes` / `update_with_indexes`。D44、RW-5）

```text
insert_with_indexes(ctx, rel, w, row):
    tid = storage.insert(rel, w, row)?                                         // ヒープに先に入れる（M4 のとおり）
    for idx in rel.indexes (OID 昇順):
        key = idx.key_of(row)
        loop {
            check = if idx.unique { UniqueCheck::Check{ heap: storage, rel, own_xid: w.xid } } else { Skip }
            match indexes.insert(w, idx, &key, tid, check)? {          // 重複は 23505（ここまでに済んだ項目は残る。文が失敗すればトランザクションが中断され、ヒープの版は見えない）
                Inserted      => break,
                WaitFor(x)    => wait_for_xid(ctx, x)?,                  // ラッチ・ピンは外れている。キャンセル・タイムアウト・デッドロックは ? で伝わる
            }                                                              // もう一度同じ引数で insert（降下からやり直し）
        }
    return tid

update_with_indexes(ctx, rel, w, snap, tid, old_row, new_row, crosscheck):
    out = storage.update(rel, w, snap, tid, new_row, Some(&ctx.wait), crosscheck)?
    if out.result != Ok { return out }                                          // Updated / Deleted / SelfModified …。インデックスには触れない
    new_tid = out.new_tid.unwrap()
    for idx in rel.indexes: (insert_with_indexes と同じ待ちのループで new_tid を入れる。キー列が変わらなくても入れる。M4)
```

- `UniqueCheck::Check` の中身（`fetch_dirty` による判定）の規則は §6.3。他トランザクションが**挿入中（xmin が実行中）**または**削除・更新中（xmax が実行中。ロックだけは除く）**のタプルに当たったら `WaitFor(その XID)`（xmin を優先）。確定した重複（コミット済みで生きている）は 23505 `duplicate key value violates unique constraint "t_pkey"` / DETAIL `Key (id)=(10) already exists.`。
- 待った後は降下からやり直すので、待ちの間に相手が中断していれば挿入でき（【実機】`rw-unique-wait` の 2 つ目）、コミットしていれば 23505 になる（1 つ目）。**3 人**（A が挿入中、B と C が同じキーを挿入）で A が中断すると、B が入り、C は B の XID を待って B のコミット後に 23505（3 つ目）。
- **デッドロックは検出器が拾う**。2 つのトランザクションが 2 つの一意キーを逆の順で挿入すると互いの XID を待つ（LK の `deadlock_timeout` 後に 40P01）。
- 待ち中に持っているのは、挿入済みのヒープ版（自分の XID が xmin、実行中）だけ。他のトランザクションは、別のインデックスの一意検査でその版に当たると自分の XID を待つ（これがデッドロックの原因になりうる）。
- **FK のフック（07 §4.5 の R1。レビュー対応 R-05 で受け入れた）**: 3 関数は、行の書き込みが成功した直後（`insert_with_indexes` は全インデックスへの挿入が済んだ後、`update_with_indexes` / `delete_row` は `TmResult::Ok` のとき）に `ri::after_insert` / `ri::after_update` / `ri::after_delete` を呼ぶ（`rel.ri.is_empty()` ならすぐ戻る）。INSERT・UPDATE・DELETE のノードは、入力を使い切ったら `ri::finish_statement(ctx)` を呼んでから `None` を返す（COPY FROM は M4 の `copy/` が CopyDone の処理の終わりで呼ぶ。07 R2）。`ri::*` は FK-2 が `executor/ri/` に書く。FK-2 が来るまでは何もしないスタブ（F0 が置く）。

### 5.10 クラッシュとリカバリ

- **ロックはクラッシュで消える**。ロックマネージャと MultiXact の表はメモリ上で、起動時に空。xmax に残ったロック保持者の XID は、clog が Committed でなく ProcArray にもないので `xid_state == Aborted`（`satisfies_update` が `Ok`、`plan_lock` は新しいロックで上書き）。MultiXact は `boundary` 未満なので `Dead`（§3.7）。
- **`HEAP_LOCK` は WAL に載せる**（M3 の full page write・チェックサムの前提を崩さないため。`m5c` §4.3）。REDO はヘッダの値を書くだけ。ロックだけをしたトランザクションの XID も `next_xid` の計算に入る（M3 の I5: ヒープの全走査の xmax の最大値を含む）。
- **`next_multi`**: 制御ファイルは「払い出しうる上限」を fsync してから ID を払い出す（§3.7）。クラッシュで `limit` まで使い切ったと見なされるだけで、再起動後に ID が衝突することはない。
- 更新・削除・ロックの WAL は 1 レコード 1 ページ（`HEAP_UPDATE` の 2 ページを除く）。torn page は FPW が防ぐ。
- 試験の観点は §7.4。

### 5.11 ロックの順序（00 §5.3 への追加）

| 順 | 規則 |
|---|---|
| 1 | ページのラッチ・ピンを持って `LockManager` の待ちに入らない。`wait_for_holders`・`wait_for_xid`・`TupleLockGuard` の取得は、ラッチ・ピンを外した後だけ（00 規約 1。デバッグビルドで検査） |
| 4 | ヒープの 2 ページのラッチは同じリレーションのブロック番号の小さい順（M3）。**ヒープのラッチ・ピンを持ったままインデックスのラッチを取らない**（B+Tree の一意検査は葉のラッチを持ったままヒープのページを共有ラッチで読む（`fetch_dirty`）ので、逆向きの待ちを作らない。§6.9 の B10） |
| 7（葉） | `MultiXactTable` の Mutex、`ProcArray` の読み取り、`Clog` の内部は、ページのラッチの下で取ってよい。`MultiXactTable` の中で I/O をしない（`ensure_reserved` をラッチの外で呼ぶ。RW-D15）。`LockManager` の Mutex は、ラッチを持たずに取る（`try_acquire` / `release` もラッチを外してから） |
| — | タプルロック（`LockTag::Tuple`）→ XID ロックの Share の順。タプルロックは印を付けた直後（ページのラッチを外した後）に解放する。**タプルロックを持ったまま別の行のタプルロックを取らない** |

---

## 6. モジュールごとの仕様

### 6.1 storage/heap/xmax.rs（RW-1）

#### `plan_lock` の決定表

`state = decode_xmax(h)`、`m = env.me`、`run(x) = env.is_running(x)`。書き込みの値は §3.2・§3.3 のビット（`Write(single)` = xmax は `m`、infomask は `(h.infomask & !XMAX_STATE_BITS) | LOCK_ONLY | 強度ビット`、infomask2 は `(h.infomask2 & !KEYS_UPDATED) | (強度が Exclusive なら KEYS_UPDATED)`、cmax は `None`）。

| state | 条件 | 結果 |
|---|---|---|
| `None` | — | `Write(single m, req)` |
| `Locker { xid == m, mode }` | `mode >= req` | `AlreadyHeld` |
| | `mode < req` | `Write(single m, req)`（MultiXact にしない。【実機】） |
| `Locker { xid != m, mode }` | `!run(xid)`（終了した保持者） | `Write(single m, req)`（古い xmax を上書き） |
| | `run(xid)` かつ `!conflicts(mode, req)` | `Write(multi { (xid, mode), (m, req) })`（MultiXact を作る） |
| | `run(xid)` かつ `conflicts(mode, req)` | `Wait { [xid], false }` |
| `LockerMulti(id)` | `expand(id) = Dead` | `Write(single m, req)` |
| | `Live(members)`: `others = members から m を除く`、`conflicting = others のうち conflicts(mode, req) のもの` が空でない | `Wait { conflicting の xid, i_am_member = members に m がいる }` |
| | `conflicting` が空・`others` が空（自分だけ） | 自分の強度が `req` 以上なら `AlreadyHeld`、そうでなければ `Write(single m, max)` |
| | `conflicting` が空・`others` が空でない | 自分の強度（なければ無し）が `req` 以上なら `AlreadyHeld`。そうでなければ `Write(multi { others, (m, max(自分, req)) })`（既存のメンバーはそのまま） |
| `Updater { xid == m }` | — | `Err(internal)`（`satisfies_update` が先に `SelfModified` / `Invisible` にする） |
| `Updater { xid != m }` | `run(xid)` | `Wait { [xid], false }`（D6: 更新者とロックは常に衝突） |
| | 終了していて中断 | `Write(single m, req)` |
| | 終了していてコミット | `Err(internal)`（`satisfies_update` が `Updated` / `Deleted` にする） |

- `Write(multi { members })` は `env.multi.create(members)` を呼び、xmax = その ID、infomask = `(h.infomask & !XMAX_STATE_BITS) | IS_MULTI | LOCK_ONLY | 最強のメンバーの強度ビット`、infomask2 = `(h.infomask2 & !KEYS_UPDATED) | (最強が Exclusive なら KEYS_UPDATED)`。【実機】`FOR KEY SHARE`×2 → `0x1090`、`FOR SHARE`×2 → `0x10D0`、`FOR KEY SHARE` + `FOR NO KEY UPDATE` → `0x10C0`（ヒント `0x0100` を除く）。
- 衝突の組は §3.4 の左上 4×4。自分の強度の昇格が自分以外のメンバーと衝突するとき（例: 2 人が `FOR SHARE`、1 人が `FOR UPDATE` に昇格）は `Wait`。両方が昇格しようとすれば互いを待ってデッドロックになる（PostgreSQL も同じ。【実機】`rw-deadlock` の 2 つ目: 先に待ち始めた方が 40P01）。
- メンバーの強度の昇格（`max`）は、同じ XID のメンバーを置き換える（1 つの xid につき 1 メンバー）。

#### `plan_update`

| state | 条件 | 結果 |
|---|---|---|
| `None` | — | `Proceed` |
| `Locker { xid == m }` | — | `Proceed`（自分のロックは、更新者の xmax に置き換わる） |
| `Locker { xid != m }` | `run(xid)` | `Wait { [xid], false }` |
| | 終了 | `Proceed` |
| `LockerMulti(id)` | `Dead` | `Proceed` |
| | `Live(members)`: `others = m を除く` が空 | `Proceed`（自分だけのロック） |
| | `others` が空でない | `Wait { others の xid, i_am_member = members に m がいる }` |
| `Updater { xid != m }` | `run(xid)` | `Wait { [xid], false }` |
| | 終了 | `Proceed` |
| `Updater { xid == m }` | — | `Err(internal)` |

- **`Proceed` は xmax を書き換えてよいという意味**。ロック保持者が（自分以外）すべて終了していること、または自分のロックだけであることを、**ラッチを持った状態で**確かめた結果。ラッチを持ったまま `stamp_updater` を書くので、確認から書き込みまで他者は割り込めない。

#### `stamp_updater` と `keys_changed`

- `stamp_updater(h, me, cid, keys_updated)` = `XmaxWrite { xmax: me, cmax: Some(cid), infomask: h.infomask & !XMAX_STATE_BITS, infomask2: (h.infomask2 & !KEYS_UPDATED) | (keys_updated ? KEYS_UPDATED : 0) }`。DELETE は `keys_updated = true`、UPDATE は `keys_changed(...)`。
- `keys_changed(key_columns, old, new)`: `key_columns[i]` が真の列について、`old[i] != new[i]`（`Datum` の `PartialEq`）、または両方が `Float4` / `Float8` でビット列（`to_bits`）が違えば真。**偽陰性（変わったのに偽）は許さない**が、偽陽性（実質同じなのに真）は許す（`KEYS_UPDATED` が立つ側＝より強い側に倒す）。`key_columns` がすべて偽なら常に偽。
- `TableDef::key_columns_from(indexes, ncols)`: 一意インデックス（`IndexDef.unique`）の `columns[].attnum - 1` の和集合。M4 には部分インデックス・式インデックス・DEFERRABLE が無いので、PostgreSQL の `INDEX_ATTR_BITMAP_KEY`（部分・式・非即時を除く）と一致する。**一意インデックスの作成・削除（`CREATE INDEX`・`DROP INDEX`・`ADD PRIMARY KEY`）でカタログキャッシュが無効になり、次の文の `RelHandle` に反映される**（`TableDef` を作り直す）。

#### 単体テスト

§3.5 の表（9 行）を固定値に。`plan_lock` は §6.1 の表の全行（状態 × 要求 4 強度 × 自分/他人 × 実行中/終了）。**衝突の組は【実機】の行列（§3.4）と、4×4 の全 16 組で一致**。`plan_update` は全行。`keys_changed` は -0.0 / +0.0、NaN、キー列でない列だけの変更。

### 6.2 storage/heap/lock.rs（RW-1）

- `TupleLockGuard { locks: Arc<LockManager>, backend: BackendId, tag: Option<LockTag> }`（`Drop` で解放）。`set(tag)`、`held()`。
- `wait_for_holders`（§5.5）と `conditional_wait_for_xact`。`WaitResult { Waited, WouldBlock }`。
- デバッグビルドでは、待ちに入る前に `buffer::track::assert_no_latches_or_pins()`（00 規約 1。`buffer/track.rs` の既存のスレッドローカルの表を使う）。
- `LockManager` の API は 00 §4.2 のものだけを使う: `acquire`・`try_acquire`・`release`・`wait_for_xact`。`wait_for_xact(id, xid, ..)` で `xid` が自分の XID のときは待たない（LK の責任。自分のロックを待つと自己デッドロックになるので、`plan_*` が自分を `Wait` に入れないことと二重に守る）。

### 6.3 storage/heap/visibility.rs（RW-1。VC と共有）

**`visible(h, snap)`（MVCC）の変更**（M2 §6.6 の疑似コードに対して）:

```text
visible(h, snap):
    if snap.is_any(): return true
    // xmin
    if xmin_is_frozen(h):            xmin は「コミット済み・全員に見える」（cmin も見ない）
    elif snap.is_own(h.xmin):        if h.cmin >= snap.curcid: return false
                                     if xmax_invalid(h) || xmax_is_lock_only(h): return true         // ★ ロックだけの xmax は無視
                                     if snap.is_own(h.xmax): return h.cmax >= snap.curcid
                                     return true
    elif !committed_in_snapshot(h.xmin, snap): return false
    // xmax
    if xmax_invalid(h) || xmax_is_lock_only(h): return true         // ★ 単独の LOCK_ONLY も MultiXact（IS_MULTI + LOCK_ONLY）も xmax を見ない。MultiXact の中身は展開しない
    if snap.is_own(h.xmax): return h.cmax >= snap.curcid
    return !committed_in_snapshot(h.xmax, snap)
```

- `xmax_is_lock_only(h) = h.infomask & HEAP_XMAX_LOCK_ONLY != 0`。**`XMIN_FROZEN` の分岐は VC が RW に依頼する範囲**（00 §8）。M5 では上のとおり入れておく（VC-4 が凍結を書き始めるまでは立たない）。
- `committed_in_snapshot` は M2 のまま（スナップショットの `xmax` / `xip` を先に見て、そのあとで clog。`BOOTSTRAP` / `FROZEN` は常にコミット済み）。**clog を見るのは必ずスナップショットの判定の後**（M2 §6.6、m3-mvcc §0）。
- `snap.own_xid == Some(x)` の直接比較は `Snapshot::is_own(x)` に置き換える（D2）。

**`satisfies_update`**: §5.1。

**`fetch_dirty(rel, own, tid) -> DirtyResult`**（M4 §13.1。一意検査用。コマンド ID を見ない。現在の状態基準）:

```text
葉のラッチを持ったまま呼ばれる。ヒープページを共有ラッチで読む（ピンつき。読み終えたら放す）
    LP が NORMAL でない（VACUUM が消した）  → Invisible
    // xmin
    xmin_is_frozen(h)                        → 次へ
    own == Some(h.xmin)                      → 次へ（自分の挿入。コマンド ID は見ない）
    match xid_state(h.xmin): Committed → 次へ ／ Aborted → Invisible ／ Running → WaitFor(h.xmin)
    // xmax（ロックだけは見ない）
    xmax_invalid(h) || xmax_is_lock_only(h)  → Visible
    own == Some(h.xmax)                      → Invisible（自分が削除・更新済み。同じトランザクションの DELETE → 同じキーの INSERT が通る）
    match xid_state(h.xmax): Running → WaitFor(h.xmax) ／ Committed → Invisible ／ Aborted → Visible
```

- **xmin の待ちを優先**する（PostgreSQL の `xwait = xmin が有効 ? xmin : xmax`）。
- 非キー更新中の他者の旧版（xmax が実行中の更新者）にも `WaitFor`（新版が同じキーを持つので、更新者がコミットすれば重複になるため。PostgreSQL と同じ）。
- `tuple_state(&HeapTuple, own) -> TupleState`（M4。CREATE INDEX の構築用）は、`LOCK_ONLY` の xmax を「xmax なし」として扱う（`Live`）。`DeleteInProgress(xid)` は更新者が実行中のときだけ。構築は Share ロックで書き手を止めているので、実行中の他人のタプルは本来見えないが、ロック側の保証が破れたときのために `InsertInProgress` / `DeleteInProgress` を返す（M4 のまま）。

### 6.4 FOR 句（構文・解析・エラー文言）（RW-3）

**構文**（PostgreSQL の `for_locking_clause`）:

```
FOR { UPDATE | NO KEY UPDATE | SHARE | KEY SHARE } [ OF table_name [, ...] ] [ NOWAIT | SKIP LOCKED ]
```

- `LIMIT` / `OFFSET` の前でも後ろでも書ける（【実機】`for update limit 0` も `limit 1 for update` も通る）。複数の句を並べられる（`FOR UPDATE OF a FOR SHARE OF b`）。
- `NOWAIT SKIP LOCKED` / `SKIP LOCKED NOWAIT` は構文エラー（42601。`syntax error at or near "skip"` など）【実機】。
- `OF` の名前は修飾なし。修飾すると 42601 `FOR UPDATE must specify unqualified relation names`【実機】（強度の語は句の強度）。

**解析**（`analyzer/locking.rs`。`BoundSelect.locking` を作る。最上位の SELECT だけ）。順に検査する。強度の語 `%s` は `UPDATE` / `NO KEY UPDATE` / `SHARE` / `KEY SHARE`。

| 順 | 条件 | SQLSTATE | メッセージ（【実機】PostgreSQL 17.11） |
|---|---|---|---|
| 1 | 集合演算（UNION / INTERSECT / EXCEPT）に `FOR` | 0A000 | `FOR %s is not allowed with UNION/INTERSECT/EXCEPT` |
| 2 | `DISTINCT`（`DISTINCT ON` も） | 0A000 | `FOR %s is not allowed with DISTINCT clause` |
| 3 | `GROUP BY` | 0A000 | `FOR %s is not allowed with GROUP BY clause` |
| 4 | `HAVING` | 0A000 | `FOR %s is not allowed with HAVING clause` |
| 5 | 集約関数（`count(*)` など） | 0A000 | `FOR %s is not allowed with aggregate functions` |
| 6 | ウィンドウ関数 | 0A000 | `FOR %s is not allowed with window functions` |
| 7 | 目的リストの集合を返す関数 | 0A000 | `FOR %s is not allowed with set-returning functions in the target list` |
| 8 | `OF` の名前が修飾付き | 42601 | `FOR %s must specify unqualified relation names` |
| 9 | `OF` の名前が FROM（この階層）に無い。表に別名があるのに元の名前で指した場合も | 42P01 | `relation "t" in FOR %s clause not found in FROM clause` |
| 10 | `OF` が関数・VALUES・WITH（CTE の参照）・JOIN を指す | 0A000 | `FOR %s cannot be applied to a function` / `... to VALUES` / `... to a WITH query` / `... to a join`（JOIN は【記憶】） |
| 11 | ロック対象がシーケンス（`OF` の有無によらず） | 42809 | `cannot lock rows in sequence "sq"` |
| 12 | ロック対象が外部結合の NULL 側 | 0A000 | `FOR %s cannot be applied to the nullable side of an outer join` |
| 13 | **yuzhu 独自**: FROM の副問い合わせがロックの対象（`OF` なしで及ぶ、または `OF` がその別名） | 0A000 | `FOR %s on a subquery in the FROM clause is not supported yet` |
| 14 | **yuzhu 独自**: 入れ子の `FOR`（FROM の副問い合わせ・CTE・SubLink・集合演算の腕の**内側**にある `FOR`。`INSERT ... SELECT` の最上位の SELECT に付く `FOR` は含まない） | 0A000 | `FOR %s in a subquery is not supported yet` |

- `INSERT ... SELECT ... FOR UPDATE` は PostgreSQL で可（【実機】）。yuzhu も、INSERT のソースの **最上位の SELECT** に `FOR` が付く形を許す（実行は `Insert` ← `Project` ← `LockRows` ← ...。DML ノードの入力であって、RW-D9 の「入れ子」ではない）。
- `OF` なしの対象は、この階層の `rtable` のうち「通常のテーブル」（`RteKind::Table`、仮想リレーションを除く）。`Function` / `Values` / `CteRef` / `Join` は**黙って無視**する（【実機】`select * from t, generate_series(1,2) g for update` は成功）。仮想リレーション（`pg_locks`・`pg_roles`）は無視（RW-K4）。**ロック対象が 0 個でも `FOR` は成功する**（`select 1 for update`、関数だけの FROM）【実機】。
- 同じ RTE を複数の句が指すときは、強度は最強、待ちの方針は `Error`（NOWAIT）＞ `Skip`（SKIP LOCKED）＞ `Block`【記憶: PG:analyze.c の `applyLockingClause`。`LockWaitBlock = 0 < LockWaitSkip < LockWaitError` の大きい方】。
- 並行して、**LK の `session/locking.rs`（生のパース木からリレーションロックの一覧を作る）がロック対象の判定を共有する**: `analyzer/locking.rs` に生の `SelectStmt` から「ロック対象の表参照（別名・テーブル名）」を返す関数 `locked_table_refs(&SelectStmt) -> Vec<TableRefId>` を置き、LK が呼ぶ（FOR の対象は RowShare、それ以外は AccessShare。00 §3.4）。解析前なので 9〜14 のエラーは出さず、対象に入りうるものを返すだけ。
- **25006**（RW-D12）: 解析の後、`BoundSelect.locking` を解決した結果ロック対象が 1 つ以上あり、トランザクションが読み取り専用なら、`cannot execute SELECT FOR UPDATE in a read-only transaction`（最初のロック対象の強度。`NO KEY UPDATE` / `SHARE` / `KEY SHARE` も同形）。`EXPLAIN SELECT ... FOR UPDATE` は実行しないのでエラーにならない【実機】。

### 6.5 計画（LockRows・recheck。RW-3）

**FOR 句つき SELECT の論理プランの組み立て順**（`planner/build.rs` の `build_select` に分岐を足す）:

```text
Project(visible targets)                                     最上位。EPQ で差し替えた行から出力式を計算し直す（RW-D8）
  Limit(limit, offset)                                       LockRows の上。LIMIT / OFFSET が数えるのは再評価を通った行
    LockRows(marks, recheck)
      Sort(keys)                                             ORDER BY があるときだけ。キーは入力（生の列）の式（LSortKey.expr = targets[key.target] の式）
        Filter(WHERE)  /  Join / Get ...                     ロック対象の Get は system_columns に Ctid を持つ。刈り込みはロック対象の全ユーザー列と ctid を残す
```

- 集約・DISTINCT・GROUP BY・ウィンドウは FOR と組み合わせられない（6.4）ので、`Project` の下に `Aggregate` などは来ない。**ORDER BY の resjunk 列は不要**（Sort のキーが式を直接持つ）。
- `LRowMark`: ロック対象の RTE ごとに 1 つ（RTE の出現順。ロックを取る順序もこれ）。`cols` は対象表のユーザー列の `ColId`（`attnum` 順）、`tid` は `Ctid` の `ColId`。`mode` / `wait` は `BoundLockingClause` を RTE ごとにまとめた値。
- `recheck` = **WHERE と内部結合の ON 条件を AND した式**（Bound の式から作る。述語の押し下げ・結合の並べ替えの前の形）。**外部結合（LEFT / RIGHT）の ON 条件は再評価に含めない**（RW-K8。EPQ で外側の行が差し替わっても、外部結合の結果の行の形は元のまま）。**書き換え後の木ではなく、元の Bound から作る**（00 §4.6）。
- `physicalize` は、`LRowMark.cols` の `ColId` を子の出力の位置の範囲 `RowMark.cols: Range<usize>`（連続していること。`Get` の出力は `attnum` 順のユーザー列が連続するので成り立つ）と `tid_col` に直す。結合を通っても、各表の列は連続して並ぶ（M4 の結合の出力は「左 ++ 右」）。
- **列の刈り込み**: `LRowMark.cols` と `tid`、`LockRows.recheck` が参照する列、`LRecheck.extra_cols` は、M4 の「使われている列」の集合に数える（刈り込みで落とさない）。

**UPDATE / DELETE の `recheck`**:

- `LRecheck.qual` = WHERE と内部結合の ON の AND（Bound の式から）。`new_values`（Update のみ）= `assignments` の式（Bound の `UpdateSource::Expr` / `Default`。`new_values` と同じ順）。
- `LRecheck.extra_cols` = `qual` / `new_values` / `returning` が参照する、対象表以外の列。`physicalize` は入力の末尾（代入式の値の後ろ）に隠し列として足す（§4.4 の行の形）。M4 の「`UPDATE ... FROM` で代入式が FROM の列を参照できる」ために Project が持っていた列は、`extra_cols` と別に重複して持ってよい（簡単さを優先）。
- `RecheckSpec` の式は、入力の行の位置（`PhysCol::Local`）で参照する（対象表の列は `0..n`、ctid は `n`、extra は `n + 1 + k ..`）。
- 生成の規則: `UPDATE` は常に `Some`（WHERE も代入式の再計算もありうる）。`DELETE` は WHERE か `USING` があるとき `Some`、無条件の `DELETE FROM t` は `None`（最新版を無条件に削除する）。

**EXPLAIN**（E1 への依頼）: `LockRows` ノードは `LockRows`（PostgreSQL と同じ。【実機】`LockRows` → `Seq Scan on t`）。詳細行は付けない。

### 6.6 実行ノード（RW-3）

- `executor/nodes/lock_rows.rs`: §5.7。`Executor::rewind` は `Error::internal`（LockRows は NestedLoop の内側に来ない）。`rows_affected` は 0。
- `executor/nodes/update.rs` / `delete.rs`: §5.6。M2 の `split_ctid` は、入力の形が変わった（extra 列がある）ので、`split_input(row, n, k, m)`（対象表の列 `n`、ctid、新しい値 `k`、extra `m`）に直す。`already_modified`（27000）は M2 のものを使う。
- `executor/nodes/seq_scan.rs` / `index_scan.rs`: `system_columns` に `Ctid` があるとき、タプルの TID を出力の対応する位置に `Datum::Tid` で出す。**FOR 句の下では、可視性判定はスナップショットのまま（行ロックは LockRows が別に取る）**。
- **`ExecCtx.snapshot`**: RC は文のスナップショット、RR はトランザクションスナップショットの写し（`curcid` と `own_xid` を更新したもの。§5.8）。`ExecCtx.xact_snapshot` が `Some` のとき RR。
- **`index_scan`** は、TID を集めた後にヒープを引いて可視性判定する（M4）。EPQ の最新版の取得（`lock_tuple` の連鎖）はインデックスを使わない（ctid をたどる）。新しい版の TID がインデックスに入っているかは関係ない。

### 6.7 Repeatable Read とセッション（RW-4）

**文のスナップショットと `first_snapshot_set`**: §5.8。`session/txn_ctl.rs` に `statement_needs_snapshot(stmt)` を置く。セッションの `first_snapshot_set`（M3 の `txn_snapshot_taken` の改名）は、トランザクションの開始で偽にし、スナップショットを要する文（RW-D11 の表）を**実行する前に**真にする。`SELECT 1` のように成功しても立つ。文が失敗してブロックが中断状態になったときは、以後の `SET TRANSACTION` は 25P02 になるので関係しない。

**`SET TRANSACTION` と関連コマンドの規則**（【実機】で確認。M3 §6.11.1 の補足と訂正）:

| 操作 | 結果 |
|---|---|
| `BEGIN; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ`（最初の文） | 成功。`SHOW transaction_isolation` は `repeatable read` |
| スナップショットを要する文の後に、**現在と同じ**レベルへ `SET TRANSACTION ISOLATION LEVEL` | 成功（検査は値が変わるときだけ） |
| スナップショットを要する文の後に、**違う**レベルへ | 25001 `SET TRANSACTION ISOLATION LEVEL must be called before any query` |
| `SELECT 1`、`VALUES (1)`、`SELECT now()`、`EXPLAIN SELECT 1`、`PREPARE`、`DECLARE`、DDL の後 | 上の 25001 の対象（スナップショットを要する文） |
| `SHOW` / `SET` / `RESET` / `LOCK TABLE` / `SET CONSTRAINTS` / `CHECKPOINT` / `LISTEN` / `NOTIFY` の後 | 成功（スナップショットを要しない） |
| 読み取り専用で、スナップショットの後に `SET TRANSACTION READ WRITE` | 25001 `transaction read-write mode must be set before any query` |
| スナップショットの後に `SET TRANSACTION READ ONLY` | 成功 |
| スナップショットの後に `SET TRANSACTION [NOT] DEFERRABLE` | 25001 `SET TRANSACTION [NOT] DEFERRABLE must be called before any query`（M3） |
| ブロックの外の `SET TRANSACTION` | WARNING `SET TRANSACTION can only be used in transaction blocks`（25P01）。何もしない（M3） |
| `BEGIN ISOLATION LEVEL SERIALIZABLE` / `SET TRANSACTION ... SERIALIZABLE` / `SET transaction_isolation = 'serializable'` / `default_transaction_isolation` | 0A000 `SERIALIZABLE isolation level is not supported yet`（RW-D17）。`BEGIN` が失敗したときはトランザクションは始まらない |
| `READ UNCOMMITTED` | 受け付ける。動作は RC。`SHOW` は `read uncommitted` |
| `SET transaction_isolation = '...'` | `SET TRANSACTION` と同じ検査（context は user） |
| `SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ`、`SET default_transaction_isolation = 'repeatable read'` | 成功。次のトランザクションから。`SHOW default_transaction_isolation` も同じ値 |
| `COMMIT AND CHAIN` / `ROLLBACK AND CHAIN` | 新しいトランザクションは同じ分離レベル・読み取り専用。**`first_snapshot_set` は偽に戻り、スナップショットは取り直す** |
| `START TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY` | 成功。書き込みは 25006 `cannot execute UPDATE in a read-only transaction`。`SELECT ... FOR UPDATE` も 25006（RW-D12） |
| 暗黙のトランザクション（`BEGIN` なし）で `default_transaction_isolation = 'repeatable read'` | その文だけ RR（スナップショットはその文のもの）。並行更新に当たれば 40001 |

- `SHOW transaction_isolation` / `current_setting('transaction_isolation')` は現在のトランザクションの値（トランザクションの外では既定）。`pg_settings` の `enumvals` は `{serializable,"repeatable read","read committed","read uncommitted"}` を返す（M3 の既存の定義に `serializable` を含めたまま。実際に選ぶと 0A000）。
- `Transaction.isolation` は `BEGIN` / 暗黙のトランザクションの開始時に `Isolation::level()` で入れる。`IsolationLevel::uses_xact_snapshot()`（RR だけ真）が、§5.8 の分岐を決める。

**40001 の文言**（RW-D13）は §5.6・§5.7・§5.8 に出てくる 2 つだけ: `could not serialize access due to concurrent update`、`could not serialize access due to concurrent delete`。SQLSTATE `40001`。エラーの後は通常のエラーと同じく、ブロック内ならトランザクションは中断状態（ReadyForQuery `E`）。

### 6.8 txn/multixact.rs（RW-2）

- **作成**: `create(members)`: メンバーを xid ごとに 1 つ（最強）に正規化し xid 昇順に並べる。`by_set` に同じ集合があればその ID を返す（ただし、すべてのメンバーが終了済みで掃除された集合は表から消えているので、新しい ID になる）。なければ `next` を払い出し（`next >= limit` なら `Error::internal("multixact id reservation exhausted")`。`ensure_reserved` が防ぐ）、`map`・`by_set`・`by_xid`・`live`（まだ終了していないメンバーの数）に登録。
- **展開**: `expand(id, still_running)`: `id.0 < boundary` または表に無ければ `Dead`。あれば、メンバーのうち `still_running(xid)` が真のものだけを残して `Live(...)`。空なら `Dead`。**展開は表を書き換えない**。
- **掃除**: `on_xact_end(xid)`: `by_xid[xid]` の各 ID について `live` を 1 減らし、0 になったら `map`・`by_set` から消す（`by_xid` の他のエントリの参照も消す）。表に残るのは「まだ実行中のメンバーがいる MultiXact」だけ。`on_xact_end` が呼ばれる前に、他のスレッドが `expand` で終了済みのメンバーを `still_running` で除外できる（呼び出しの順序に依存しない）。
- **メモリ**: 同じメンバー集合は 1 つの ID を共有するので、2 人が 100 万行を `FOR SHARE` しても 1 エントリ。長時間のトランザクションが多数の異なる集合を作る場合だけ膨らむ（上限は設けない。確認事項 M5-RW-Q9）。
- **再起動**: `boundary` 未満はすべて `Dead`（§3.7）。起動時に表は空。
- **並行**: Mutex は 1 本（短い臨界区間）。`create` / `expand` はページのラッチの下で呼ばれるので、中で他のロックを取らない。

単体テスト: 同じ集合は同じ ID、強度の正規化、`Dead` の 3 経路（`boundary` 未満・掃除済み・全員終了）、`on_xact_end` の掃除、`ensure_reserved` が制御ファイルを書く（`SimVfs` の書き込みを数える）、再起動で ID が衝突しない（`next_multi` を `ControlFileHandle` に書いて開き直し）。

### 6.9 B+Tree の複数ライター対応（RW-5。D18、D44、D47）

**前提**: M4 の `06-btree.md` は未着手。ここでの洗い出しは、00 §13・§19 と `m4-btree.md`（§1.1 Lehman-Yao、§4 ラッチ、§5.1 分割の WAL、§6.1 一意検査、§9.1 スキャン）に基づく。`06-btree.md` が確定したら各行を突き合わせ、食い違いは §11 に追記する。

**M4 の単一ライター前提の洗い出し**:

| # | M4 の前提（出典） | 複数ライターで起きること | RW-5 の対応 |
|---|---|---|---|
| B1 | 降下: 内部は共有ラッチ・葉だけ排他。降下の間に葉の範囲が変わらない（`m4-btree` §4.1「書き手は常に 1 人」） | 葉を排他ラッチするまでの間に、別のライターが葉を分割し、キーが右ページへ移る | 排他ラッチを取った後で high key と比べ、挿入キーが high key 以上なら**右へ移動**する（現在のページを放して右隣を排他ラッチ。右へだけ）。移った先でも繰り返す。移動した後に空きがあるかを判定し直す |
| B2 | 親へ上るときスタックはずれない（§4.1「M4 では木を変更するのは書き手 1 人なので」） | 降下の後に親が分割され、ダウンリンクが親の右隣に移る。ルートが分割され、スタックの上に新しいレベルができる | `find_parent`（`_bt_getstackbuf` 相当。M4 は「親を探し直す処理は PostgreSQL と同じ形で書く」と予約済み）: スタックの親候補を排他ラッチし、**子のブロック番号が一致するダウンリンク**を探す。無ければ右隣へ（右へだけ）。スタックが尽きた（ルートが変わった）ときは、メタページから降り直して目的のレベルのページを探す |
| B3 | 構造変更は `BTREE_PAGES`（全画像の 1 レコード、最大 32 ブロック）。影響する全ページのラッチを分割の間持つ。「下 → 上」「左 → 右」の順で取る（§4.1、§5.1） | 2 人が隣り合うページの分割を同時に始めて、互いのラッチを待つ（循環） | **全順序を決める**: `(レベルの昇順, 同じレベルでは左 → 右（右リンクの向き）)`、メタページは最後。分割の手順は常にこの順に増える方向でだけ追加のラッチを取る（下の「ラッチの取得順序」）。待たずに諦める（try-latch）は使わない |
| B4 | ルートの分割: 新ルートを作りメタページの `root` / `level` を更新（§4.3） | 2 人が同じルートを分割しようとする。ルートのラッチを取った時点で、すでに他者が分割していて、そのページはもうルートではない | ルートの排他ラッチを取った後、**ルートフラグとメタの `root` を、メタページの排他ラッチの下で確かめる**（ルート → メタの順）。すでにルートでなければ B2 の `find_parent` に切り替える（その上のレベルを探す） |
| B5 | 一意検査が他者の未確定項目に当たる「`WaitFor` は起きない」（§6.1） | 他トランザクションの挿入中・削除中のタプルに当たる | `InsertOutcome::WaitFor(xid)`。ラッチ・ピンをすべて外して返す。呼び出し側が待って**降下からやり直す**（§5.9） |
| B6 | 同じキーを同時に挿入する 2 つのトランザクション（§6.1 の「最初の葉の排他ラッチを検査から挿入まで持つ」） | 検査が互いの項目を見ずに通り、重複が入る | 下の「一意検査のラッチの規則」 |
| B7 | 新しいページは分割の前に先に確保する（§4.3 手順 1「ファイル拡張」） | 確保した後で他のライターが同じ葉を分割し、確保したページが不要になる／親の連鎖が伸びて足りなくなる | ページの確保は**ラッチを取った後**に、必要なぶんを数えてから `extend` する（リレーション拡張ロックはコンテンツラッチの後。M3 §5.9 の 4 → 5）。確保した後で計画を変える必要が出たら（B1・B2 のやり直し）、確保済みの空ページは孤児として残す（M6 で再利用）。孤児ページは `check.rs` が「どのレベルからも到達できない空ページ」として許す |
| B8 | 後ろ向きスキャンは left-link をたどり、「左へ移ったら右へ進んで元ページを指すページを探す」（§9.1） | 並行する分割で left-link が古い | M4 のまま（M4 も読み手は分割と並行して走るので、すでに必要）。スキャンはラッチを 1 つしか持たず、左へ移るときは現在のページを放してから取る |
| B9 | シーケンスの `fetch` は排他ラッチだけ（M4 §19） | — | 変更なし |
| B10 | 一意検査の `fetch_dirty` は葉のラッチを持ったままヒープを共有ラッチで読む（00 A7） | ヒープのページのラッチを持つ者が、インデックスのラッチを待つと循環する | **規則**: ヒープのラッチ・ピンを持ったままインデックスのラッチを取らない。M5 の呼び出し順（`insert_with_indexes` はヒープの挿入を終えて両方のラッチを外してから、インデックスへ）で満たす。VACUUM の第 2 段（`bulk_delete`）もヒープのラッチを持たずにインデックスを処理する（VC 章の規則に追記を依頼） |
| B11 | `unlink_storage` / `nblocks` | DROP INDEX・TRUNCATE と並行する DML | リレーションロック（AccessExclusive）で排他（LK）。B+Tree の内部では何もしない |
| B12 | 一括構築（`build`）は空のインデックスにだけ | 構築中の並行する DML | CREATE INDEX は Share ロックで書き手を止める（00 §3.4。LK） |

**ラッチの取得順序**（B3 の規則）:

- 順序 `O`: `(level 昇順, 同じ level では右リンクの向きに左 → 右)`、メタページ（ブロック 0）は最後。ページのブロック番号の大小は使わない（分割で新しい右ページは大きい番号になるが、順序上は元のページと右隣の間に入る）。
- **分割を行うライターが追加で取るラッチは、保持しているどのラッチよりも `O` で後ろ**にある: 元の右隣（同じ level で右）、親（level + 1）、親の右隣、…、ルート、メタ。したがって循環は作れない（保持している最大の要素より後ろだけを要求するので、待ちグラフに閉路がない）。
- 読み手は 1 つしかラッチを持たない（降下は親を放してから子を取る）ので、循環に加わらない。後ろ向きスキャンは左へ移るとき現在のページを放す。
- 一意検査は、キーが入りうる最初の葉 `P0` を保ったまま、等しいキーが続く右隣のページを共有でなく**排他**で順にラッチして調べる（`P0` → 右へ。順序 `O` に従う。調べ終えたら右隣は放す）。
- 取得の順序を破りうる唯一の箇所が B2 の「親を探して右へ移る」だが、これも親の level で右へだけ移る。親を探す間に保持するのは子（より前の `O`）と現在の親候補だけ。
- 保持の上限は WAL の `MAX_BLOCK_REFS = 32`（木の高さ `h` で最大 `2h + 3` ページ。M4 §13.4。`h > 14` は `54000`）。

**一意検査のラッチの規則**（B6。PostgreSQL の `_bt_doinsert` の要点）:

1. 検査を始める葉 `P0` = 「`(key, TID = -∞)` が入りうる最初の葉」。`P0` の排他ラッチを**検査から挿入先の確定まで**手放さない。
2. 検査は `P0` から等しいキーの項目を順に調べ、`P0` の high key のキー部分が `key` と等しければ右隣に進む（`P0` を保ったまま右隣をラッチ。調べ終えたら右隣を放す）。各項目に `fetch_dirty`（§6.3）。`WaitFor(x)` なら、すべてのラッチ・ピンを放して `InsertOutcome::WaitFor(x)` を返す。確定した重複は `23505`。
3. 検査を通ったら、挿入位置 `(key, 新しい TID)` を決める。挿入キーが `P0` の high key 以上なら右へ移動するが、**右隣のラッチを取ってから `P0` を放す**（カップリング）。
4. これで、同じキーを検査する他のライターは、(a) `P0` で待つ、(b) `P0` を通ったとしても、右隣で我々のラッチに止まる（`P0` の high key が同じキーで続くので右隣まで進む）か、(c) すでに我々の項目を見る、のどれかになり、**互いの未挿入の項目を見落とさない**。
5. 挿入が `BTREE_INSERT_LEAF`（葉に空きがある）なら葉のラッチだけ。分割なら B3 の順序で追加のラッチを取る。

**スキャンと VACUUM の連動**（VC 章）: `IndexScan` は葉ごとに TID をコピーしてラッチ・ピンを手放す（00 A7）。このため、スキャンが持つ MVCC スナップショットが horizon を止めること（D11）が、「見えていたタプルの行ポインタは再利用されない」の根拠。RW-5 はこの規則を変えない。

**単体テスト・並行試験**: §7.5。

### 6.10 カタログ行の更新競合と同一コマンドの二度更新（RW-6）

- カタログの書き込み（`catalog/store.rs` の `delete` / `update`。`drop_table` が pg_class・pg_attribute などの行を消す、VC が `relpages` などを更新する、DB 章が `pg_database` を更新する、など）は、`TableStore::delete` / `update` を **`wait: Some(&WaitCtx)`、`crosscheck: None`** で呼び、結果を `TmResult::expect_simple(op)`（§4.2）で検査する。待つので、相手が中断すれば成功し、コミットすれば PostgreSQL と同じ `XX000`（`tuple concurrently updated` / `tuple concurrently deleted` / `tuple already updated by self`）になる。M2 の `expect_deleted`（「単一ライターなので他の結果は破損か不具合」）は置き換える。
- そのため `CatalogStore` の書き込み系の関数に `wait: &WaitCtx<'_>` の引数が要る（M4 の C1 のファイル。§11 に変更依頼）。DDL の実行中の `WaitCtx` は `DdlCtx.wait`（00 §4.7）から作る。
- **同一コマンドでの二度更新（27000）**: §5.6。メッセージ `tuple to be updated was already modified by an operation triggered by the current command`（DELETE は `deleted`）、HINT `Consider using an AFTER trigger instead of a BEFORE trigger to propagate changes to other rows.`。`cmax == w.cid` なら黙って飛ばす（`UPDATE ... FROM` の多対一）、`cmax != w.cid` なら 27000。M5 では後者は起きないが分岐は残す。

### 6.11 RETURNING の仕上げ（RW-6。D46）

M4 が用意する欄（`BoundReturning { targets, columns }`、`PhysicalPlan::{Insert, Update, Delete}.returning: Option<Vec<PhysExpr>>`）の続き。**M4 が済ませたものは省く**（M4 の完了後に、この表の各行について M4 の実装を調べ、済んでいる行は RW-6 から外す）。

| 項目 | 内容 | PostgreSQL 17 の挙動（【実機】） |
|---|---|---|
| 行の形 | INSERT / UPDATE: **新しい行**。DELETE: **削除した行**。`returning` の式は `PhysCol::Local(i)`: `i < n` は対象表の行、`i >= n` は入力の extra 列（UPDATE ... FROM / DELETE ... USING の他テーブルの列。`LRecheck.extra_cols` に数える） | `UPDATE p ... FROM q RETURNING p.*, q.k`、`DELETE ... USING ... RETURNING rt.id, ru.k` が通る |
| 対象外の表の参照 | `INSERT ... RETURNING ru.k`（FROM がない） | 42P01 `missing FROM-clause entry for table "ru"` |
| 集約・ウィンドウ | `RETURNING count(*)` / `sum(v) over ()` | 42803 `aggregate functions are not allowed in RETURNING` / 42P20 `window functions are not allowed in RETURNING` |
| サブクエリ | `RETURNING *, (SELECT count(*) FROM rt), v * 2` | 通る。副問い合わせは**その文のスナップショット**で評価され、今挿入した行は見えない（`count` は 0）。`SubPlan` は `returning` の `PhysExpr` の中（`SubPlanId`） |
| システム列 | `RETURNING ctid`（新しい行の TID）、`tableoid`、`xmin`（= 自分の XID）、`cmin`（= `w.cid`） | `xmax` は 0（INSERT / UPDATE の新しい行）。**M5 は `ctid` と `tableoid`、`xmin`、`cmin` を許し、`xmax` / `cmax` は 0A000**（DELETE の `xmax` は自分の XID で意味が違うため） |
| `RETURNING *` の展開 | 対象表のユーザー列に続けて、`UPDATE ... FROM` / `DELETE ... USING` の表の列も展開する（RTE の出現順）。`rt.*` は対象表だけ | 【実機】`update rt ... from ru ... returning *` は `id | v | id | k`、`returning rt.*` は `id | v` |
| 列名 | `?column?` / 式の最後の列名 / `AS` | SELECT と同じ |
| EPQ との組み合わせ | 再評価で行が落ちたら RETURNING の行も出ない。再評価を通ったら**再計算後の新しい行** | `rw-epq-recheck` の `RETURNING` で確認（`UPDATE 0` なら 0 行、通れば `110`） |
| コマンドタグ | `INSERT 0 n` / `UPDATE n` / `DELETE n`（RETURNING があっても行数が n） | — |
| Extended Query | `Describe`（文・ポータル）が RETURNING の列の `RowDescription` を返す（`PhysicalQuery.output` が空でない DML）。DML は最初の `Execute` で最後まで実行して行を溜める（PostgreSQL の `PORTAL_ONE_RETURNING`）。行数制限があっても副作用は全部起きる | XQ-2 の責任。この章は `PhysicalQuery.output` と、DML ノードが行を返す口を満たす |
| 失敗 | 制約違反などで文が失敗したら、すでに作った RETURNING の行は捨てる（結果は返らない） | 文の原子性 |

- DML ノードは入力行ごとに RETURNING の行を **返してよい**（`Executor::next` が行を返す）。DML の副作用は `next` が呼ばれるたびに 1 行ぶん進む。**トップレベルの駆動は、ポータルが `PORTAL_ONE_RETURNING` なら全部読み切る**（XQ。Simple Query は全部読む）。
- RETURNING の式の評価は、新しい行を作った後（インデックスへの挿入・一意検査の後。PostgreSQL と同じ順序）。

### 6.12 M6 の拡張点（今は作らない。壊さない）

| 機能 | 触る箇所 | M5 で用意してあるもの |
|---|---|---|
| `INSERT ... ON CONFLICT` | (1) 一意検査の結果に「衝突した TID」を返す口（`UniqueCheck` に `Probe` を足し、`InsertOutcome` に `Conflict { tid }` を足す）、(2) 衝突した行を `lock_tuple(.., Exclusive / NoKeyExclusive, Block / Skip, follow = false)` でロックし、`Deleted` / `Updated` なら最初からやり直す、(3) `DO UPDATE` は EPQ の再評価と同じ形（`RecheckSpec` の `qual` を `WHERE`、`new_values` を `SET` に流用） | `lock_tuple` の全結果、`InsertOutcome` の列挙（`#[non_exhaustive]` を付けておく）、`fetch_dirty` の `WaitFor` と待ちのループ |
| SAVEPOINT（D2） | 「自分のトランザクションの XID か」の判定箇所をすべて `Snapshot::is_own` / `Transaction::owns_xid` に通す。サブトランザクションの XID の集合を持つ | **この章の自分の XID の比較は次の 6 か所に集約してある**: (1) `visible`（xmin・xmax）、(2) `satisfies_update`、(3) `fetch_dirty`（`own`）、(4) `XmaxEnv.me`（`plan_lock` / `plan_update` の「自分のロック」。サブトランザクションが入ると「自分のトップレベル XID の配下」になる）、(5) `MultiMember` の「自分」の置き換え（強度の昇格の同一性）、(6) `UniqueCheck::Check.own_xid`。さらにサブトランザクションの中断ではロックを戻す（PostgreSQL は中断したサブトランザクションの `LOCK_ONLY` の xmax を無効として扱う。yuzhu も `xid_state(sub) == Aborted` で同じ）。`wait_for_xact` は親（トップレベル）の XID を待つ（LK） |
| MultiXact の永続化（案 C） | `MultiXactTable` の内部（メンバー表の SLRU、WAL、VACUUM での切り詰め）、更新者をメンバーに入れる、`plan_lock` / `plan_update` の「更新者と常に衝突」の規則を PostgreSQL の表に戻す（RW-K1 が消える） | メンバーの強度を 4 種類のまま記録している（意味を変えない）。`KEYS_UPDATED` をディスクに PostgreSQL と同じ意味で書いている |
| `SERIALIZABLE`（SSI） | 述語ロックと rw 依存の検出。`IsolationLevel::Serializable` | RR の 40001 の経路と、読み取りの `xact_snapshot` を共有できる |
| HOT | `HEAP_HOT_UPDATED` / `HEAP_ONLY_TUPLE`、`LP_REDIRECT` | infomask2 のビットは予約のまま |

---

## 7. テスト

テストの基盤（分離性ランナーの拡張、CI、`tests/slt/m5/` の枠）は `10-tests-plan.md`（TS）。この章の機能のテストファイルは RW が書く。**共有テスト（slt・分離性 spec）は PostgreSQL 17 でも通ること**（00 §9）。RW-K1〜K9 の差が出るケースは共有に入れず、yuzhu 専用のディレクトリに置く。

### 7.1 共有の SQL テスト（`tests/slt/m5/`。単一接続と `connection` による複数接続）

sqllogictest は各レコードの完了を待つので、**ブロックしないケース**（結果だけが変わる・即時にエラーになる）だけを書く。ブロックするケースは 7.2。

| ファイル | 内容 |
|---|---|
| `rowlock/for_clause_syntax.slt` | 4 強度、`OF t1, t2`、`NOWAIT`、`SKIP LOCKED`、`LIMIT` の前後、複数の句（`FOR UPDATE OF a FOR SHARE OF b`）、`INSERT ... SELECT ... FOR UPDATE`、`SELECT 1 FOR UPDATE`、関数 / VALUES / CTE の参照が FROM にあっても `OF` なしなら無視、ロックなしと同じ結果。`FOR UPDATE NOWAIT SKIP LOCKED` は 42601 |
| `rowlock/for_clause_errors.slt` | §6.4 の表の 1〜12 を `statement error (SQLSTATE) メッセージ` で（DISTINCT、GROUP BY、HAVING、集約、ウィンドウ、集合演算、`OF` の修飾付き、`OF` が見つからない・別名、関数 / VALUES / WITH、シーケンス 42809、外部結合の NULL 側）。13・14 は `onlyif yuzhu` |
| `rowlock/for_clause_readonly.slt` | `START TRANSACTION READ ONLY` で `SELECT ... FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE` が 25006（メッセージの強度の語）。`SELECT 1 FOR UPDATE`、`FROM generate_series(...)`、`EXPLAIN SELECT ... FOR UPDATE` は通る。`LIMIT 0` でもロック対象があれば 25006 |
| `rowlock/for_lock_2conn.slt` | 接続 2 本。A が `FOR UPDATE` した行に、B の `FOR UPDATE NOWAIT` が 55P03 `could not obtain lock on row in relation "t"`、`SKIP LOCKED` はその行を除く（`ORDER BY id LIMIT 1` なら次の行）、普通の `SELECT` は読める。A が `FOR SHARE` なら B の `FOR SHARE NOWAIT` は成功、`FOR UPDATE NOWAIT` は 55P03。A が `FOR KEY SHARE` なら B の `FOR NO KEY UPDATE NOWAIT` は成功、`FOR UPDATE NOWAIT` は 55P03。B の `SET lock_timeout = '200ms'; UPDATE ...` は `canceling statement due to lock timeout`（55P03）。A のコミット後は B が通る |
| `txn/isolation_level_rr.slt` | §6.7 の表（`SHOW transaction_isolation`、25001 の各ケース、`SELECT 1` の後の 25001、`SHOW` / `SET` / `LOCK TABLE` の後は成功、READ WRITE の 25001、`COMMIT AND CHAIN` で分離レベルが続く、`SET SESSION CHARACTERISTICS`、`READ UNCOMMITTED` の表示、ブロック外の WARNING）。SERIALIZABLE の 0A000 は `onlyif yuzhu`（PostgreSQL は成功するため） |
| `txn/rr_snapshot_2conn.slt` | 接続 2 本。RR の `BEGIN` だけではスナップショットを取らず、最初の文で取る。以後 A のコミットは見えない（`count(*)` と合計が不変）。RC は見える。RR で読むだけ・挿入だけ・別の行の更新はエラーにならない。`COMMIT AND CHAIN` の次のトランザクションは新しいスナップショット |
| `txn/rr_conflict_2conn.slt` | 接続 2 本（A はコミット済み）。RR の B が、A が更新・削除した行に `UPDATE`（`concurrent update` / `concurrent delete`）、`DELETE`、`SELECT FOR UPDATE / SHARE / NO KEY UPDATE`（`concurrent update`）、`FOR KEY SHARE`（非キー更新なら**旧版を返して成功**、キー更新・削除なら 40001）。A が `FOR UPDATE` しただけでコミットした行は B が更新できる。40001 の後は `ROLLBACK` まで 25P02。メッセージは本章 §0 RW-D13 のとおり。**`rw-rr-conflict` の 15 permutation をそのまま SQL にしたもの** |
| `dml/returning_basic.slt` ほか（`returning_from_using.slt`、`returning_errors.slt`） | §6.11 の表。INSERT / UPDATE / DELETE の RETURNING（`*`、式、別名、サブクエリ、`ctid`・`tableoid`）、UPDATE ... FROM / DELETE ... USING の列の参照と `*` の展開、42803・42P20・42P01 |
| `dml/update_from_halloween.slt` | `UPDATE ... FROM` で 1 つの対象行に複数の入力行が当たっても 1 回だけ更新される（`UPDATE n`）。同じ文で同じ行を二度更新しない |

### 7.2 分離性テスト（`tests/isolation/specs/`。本物の PostgreSQL 17 で `.out` を生成）

M3 のランナー（`tests/tools/isolation`。isolationtester と同じ `.spec` と出力形式）で流す。yuzhu 側は `pg_isolation_test_session_is_blocked`（LK-5）でブロックを判定する。**この章の spec 候補は、PostgreSQL 17.11 に対して実際に流して結果を確かめた**（§9.1。以下の「期待」はその出力の要点）。`deadlock-*` は `deadlock_timeout` が 1 秒なので、各セッションの `setup` で `SET deadlock_timeout = '100ms'` にすると速い（yuzhu も誰でも SET できる。00 §3.6）。

**M3 の既存 spec** は、`lost-update`（RC 2 本・RR 2 本。M3 は RR が 0A000 だったので RR の 2 本は M5 から通る）、`write-skew-rr`（RR で write skew が起きる。M5 から通る）、`writer-waits-writer`（同じ行。M5 でも通る）を使い続ける。`writer-queue` と `lock-timeout` の「別の行の更新も待つ」前提の部分は、D39 のとおり TS が書き直す。

| ファイル（新規） | 内容と期待（【実機】で確認済み） |
|---|---|
| `epq-recheck.spec` | RC。A が行を更新（未コミット）、B が同じ行を更新して待ち、A のコミットで再評価。(1) `v = v * 10` は A の結果 11 を基に **110**（`RETURNING 1\|110`）。(2) A が `v = 99`、B の `WHERE id = 2 AND v = 20` は **0 行**（最終 `2\|99`）。(3) A が DELETE、B の UPDATE は **0 行**。(4) 同じく B の `DELETE ... AND v = 20` は **0 行**。(5) A が ROLLBACK なら B の DELETE は **1 行**（`2`）で行が消える |
| `epq-join-partner.spec` | `UPDATE ep SET v = v + 1 FROM eq WHERE ep.id = eq.id AND eq.k = 1 RETURNING ep.id, ep.v, eq.k`。A が `ep` の行を 50 にして未コミット → B 待ち → C が `eq.k = 9` を更新（ブロックしない）→ A コミット → B は `1\|51\|1`（**`eq` は元の版 `k = 1`**）、最終 `1\|51\|9`。`ep.v = 1` を条件にすると A のコミット後に 0 行。`EXISTS (SELECT 1 FROM eq WHERE ... k = 1)` も C が `k = 9` にした後で更新が通る（**相関サブクエリは文のスナップショット**） |
| `epq-initplan.spec` | `WHERE id = 1 AND v = (SELECT min(v) FROM ip)`。A が `v = 0` にしてコミットしても、B は **0 行**（InitPlan の結果 `min = 1` を再実行しない）。最終 `1\|0, 2\|2` |
| `epq-no-pickup.spec` | `UPDATE np SET v = v + 100 WHERE v < 5`。A が `id = 1` を +1、`id = 3` を `30 → 1` にして未コミット → B 待ち → A コミット。B は `id = 1, 2`（`1\|102, 2\|102`）だけ更新し、**元の走査で拾わなかった `id = 3` は拾わない**（最終 `3\|1`） |
| `epq-for-update.spec` | RC の `SELECT ... FOR UPDATE`。(1) `WHERE v < 5 ORDER BY id LIMIT 1 FOR UPDATE` で、A が先頭行を `v = 100` にしてコミットすると B は **`2\|2\|1`**（再評価で落ちた行の分、次の行が LIMIT を満たす）。(2) `WHERE v < 5 ORDER BY v DESC FOR UPDATE` で A が `id = 3` を `v = 0` にすると、B は **`4, 3(v=0), 2, 1` の順**（並べ替えは LockRows の前。値は新しい版）。(3) `WHERE id = 1 FOR UPDATE` は新しい版 `1\|100`。(4) `AND v = 1` を付けると 0 行 |
| `rr-conflict.spec` | RR の B（`BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*)`）の後、A が自動コミットで変更。15 permutation: B の `UPDATE`・`DELETE`・`FOR UPDATE`・`FOR NO KEY UPDATE`・`FOR SHARE NOWAIT`・`FOR UPDATE SKIP LOCKED` はすべて 40001 `could not serialize access due to concurrent update`（A が削除した行への UPDATE / DELETE だけ `concurrent delete`）。**`FOR KEY SHARE` は、A の非キー UPDATE（1 回でも 2 回でも）の後なら旧版 `2\|2` を返して成功**、A がキー列を更新・または更新後に削除した後なら 40001。A が `FOR UPDATE` しただけの行の B の UPDATE は成功 |
| `rr-first-snapshot.spec` | A が `LOCK TABLE fs IN ACCESS EXCLUSIVE MODE` して行を挿入（未コミット）。RR の B の最初の `SELECT` / `UPDATE` は待ち、A のコミット後も A の行が**見えない**（`SELECT` は `1\|1` のみ、`UPDATE ... WHERE id = 2` は 0 行）。**RC の B は見える**（`1, 2`）。B が RR で最初に `LOCK TABLE` した場合は、待ちの後の `SELECT` に A の行が**見える**（`LOCK TABLE` はスナップショットを取らない）。`BEGIN ISOLATION LEVEL REPEATABLE READ` の後・最初の `SELECT` の前に C がコミットした行は、B の最初の `SELECT` に見える（`BEGIN` ではスナップショットを取らない） |
| `nowait-skip-locked.spec` | A が `FOR UPDATE`（`id = 1`）: B の `FOR UPDATE NOWAIT` は 55P03 `could not obtain lock on row in relation "ns"`、`ORDER BY id FOR UPDATE SKIP LOCKED` は `2, 3, 4`、`ORDER BY id LIMIT 1 ... SKIP LOCKED` は `2`、普通の SELECT は 4 行、`SET lock_timeout = '200ms'; UPDATE` は `canceling statement due to lock timeout`。A が `FOR SHARE`（`id = 2`）: B の `FOR SHARE NOWAIT` は成功、`FOR UPDATE NOWAIT` は 55P03、`FOR UPDATE SKIP LOCKED` は `1, 3, 4`、`FOR SHARE SKIP LOCKED` は全 4 行 |
| `tuple-lock-queue.spec` | A が更新中、B・C が同じ行を更新して待つ。D の `pg_locks`（`locktype = 'tuple'`）は 2 行（ExclusiveLock の granted と待ち）。A のコミットで B → C の順に通り最終 111（`+1 +10 +100`）。A の ROLLBACK なら 110 |
| `deadlock-cross.spec` / `deadlock-share-upgrade.spec` | 交差更新（s1: `id=1` → `id=2`、s2: `id=2` → `id=1`）で、**先に待ち始めた s1** のステップが `<... completed>` で `ERROR: deadlock detected`。共有ロックの昇格（2 人が `FOR SHARE` → 2 人が UPDATE）も、先に待ち始めた方が 40P01 |
| `unique-insert-wait.spec` | A が `INSERT (10)`（未コミット）、B が同じキーを INSERT して待つ。A コミット → B は 23505 `duplicate key value violates unique constraint "uw_pkey"`、A ロールバック → B は成功。3 人（C も同じキー）で A がロールバックすると B が入り、C は B のコミット後に 23505 |
| `multixact-share.spec` | A・B が同じ行を `FOR SHARE`（共有）。C の `FOR UPDATE NOWAIT` は 55P03、`FOR SHARE NOWAIT` は成功。C の `FOR UPDATE`（待ち）は A・B の**両方**のコミット後に完了。別の行で A が `FOR KEY SHARE`、B が `FOR NO KEY UPDATE` は両立し、C の `FOR UPDATE NOWAIT` は 55P03 |
| `tuplelock-conflict.spec` | `FOR KEY SHARE / SHARE / NO KEY UPDATE / UPDATE` の 4×4（16 permutation）。要求側は `NOWAIT`。§3.4 の左上 4×4 と同じ（衝突の組だけ 55P03）。行列は `psql` で測定済み（spec としては未実行） |
| `lock-then-update.spec` | RR の B（`SELECT count(*)` で最初のスナップショットを取った後）。A が `FOR UPDATE` してコミットした行を B が UPDATE → **成功**（ロックだけの xmax は無視。`1\|11`）。A が `FOR UPDATE` したまま B の UPDATE は待ち、A のロールバックでもコミットでも B は**成功**（コミットしたのがロックだけなので 40001 にならない） |
| `read-write-unique.spec` | RR の B のスナップショット（`count(*) = 0`）の後に A が `INSERT (10)` を自動コミット。B の同じキーの INSERT は**即座に** 23505 `duplicate key value violates unique constraint "rwu_pkey"`（40001 ではない。一意性は最新のコミット済みの状態に対する制約）。別のキー `INSERT (11)` は成功 |
| `update-chain.spec` | A が行 1 を更新（未コミット）、B が全行の `UPDATE uc SET v = v + 100 RETURNING` で行 1 を待つ間に、C が行 2 を 2 回（別の文）更新して自動コミット → A のコミットで B は**行 2 の最終版**（`2 + 1 + 10`）を基に `2\|113`（`1\|102` と合わせて 2 行）。C が行 2 を更新してから DELETE した場合は、B は行 2 を飛ばして `1\|102` だけ（連鎖の先が削除 → RC は飛ばす） |

**PostgreSQL 本家の spec の移植候補**（`m5c` §15.2 の表を、この章の実装で見直した）:

| 本家の spec | 扱い |
|---|---|
| `eval-plan-qual` | 単一テーブルのステップだけを上の `epq-*` に再構成した。本家のファイルは結合・サブクエリ・CTE を含むので、そのまま移植するときは permutation ごとに PostgreSQL で期待を再生成し、外部結合を使うもの（RW-K8）は yuzhu 専用に分ける |
| `deadlock-simple` / `deadlock-hard` | `deadlock-cross` と同形（LK 章が `deadlock-hard` を持つ）。`deadlock-soft` / `deadlock-soft-2` は LK の確認事項（待ち行列の並べ替えを実装しないため差が出うる） |
| `tuplelock-conflict` | `tuplelock-conflict.spec`（上） |
| `tuplelock-update` / `tuplelock-upgrade-no-deadlock` | **差が出る**（RW-K1・K2）。yuzhu 専用 `tuplelock-upgrade-keyshare.spec` に、yuzhu の期待（KEY SHARE を持つ 2 人の片方が非キー UPDATE へ昇格すると待ち、両方が昇格すると 40P01）を書く |
| `skip-locked` / `nowait` / `skip-locked-2`〜`4` | `nowait-skip-locked.spec` に取り込み。本家の複雑なもの（複数テーブル・サブクエリ）は結合の EPQ に依存するか確かめて移す |
| `lock-update-delete` / `lock-committed-update` / `lock-update-traversal` | `lock-then-update.spec` と `update-chain.spec` |
| `read-write-unique` ほか `*-unique` | `read-write-unique.spec`、`unique-insert-wait.spec` |
| `fk-contention` / `fk-deadlock` / `fk-deadlock2` / `fk-partitioned-*` | FK 章。KEY SHARE と非キー UPDATE の衝突（RW-K1）で差が出る |
| `multixact-no-deadlock` | 本家は UPDATE（更新者）を MultiXact に入れる前提の部分を含む可能性があり、差が出うる（未確認）。差が出たら yuzhu 専用 |
| `vacuum-concurrent-drop` / `vacuum-conflict` | VC 章 |

### 7.3 yuzhu 専用の分離性 spec（`tests/isolation/yuzhu-only/`。yuzhu の期待を `.out`、PostgreSQL の結果を `.out.PG`（参考）で持つ）

| ファイル | 内容 |
|---|---|
| `keyshare-update-wait.spec` | RW-K1。s1 が `FOR KEY SHARE`、s2 が非キー列の `UPDATE`: **yuzhu は `<waiting ...>`、s1 のコミットで完了**（PostgreSQL は待たずに成功）。逆（s1 が非キー UPDATE 中に s2 が `FOR KEY SHARE`）も同様 |
| `keyshare-update-deadlock.spec` | RW-K2。s1 が `FOR KEY SHARE`、s2 が非キー UPDATE（待ち）、s1 が同じ行の非キー UPDATE → **yuzhu は先に待った s2 が 40P01**（PostgreSQL はデッドロックしない） |
| `for-update-nested.spec`（または slt の `onlyif yuzhu`） | RW-K3。入れ子の `FOR UPDATE` の 0A000 |
| `epq-outer-join.spec` | RW-K8。外部結合の ON が並行更新で偽になるケース。yuzhu の結果（ON を再評価しない）と PostgreSQL の結果を並べる |

### 7.4 Rust のテスト（`yuzhu-core`）

| 対象 | 内容 |
|---|---|
| `heap/xmax.rs` | §6.1。§3.5 の表の固定値、`plan_lock` の全行・4×4 の衝突、`plan_update`、`keys_changed`、`lockers_all_finished`、破損の検出（IS_MULTI で LOCK_ONLY なし、LOCK_ONLY で強度ビットなし） |
| `heap/visibility.rs` | `LOCK_ONLY`（単独・MultiXact）の xmax が可視性に影響しない、`XMIN_FROZEN`（両ビット）と片方だけ、自分の挿入・ロック・更新の組み合わせ、`satisfies_update` が**スナップショットより後にコミットされた版**を `Ok` と判定する（RW-D2 の回帰）、`fetch_dirty` の全分岐（§6.3） |
| `heap_store.rs`（SimVfs。1 スレッド） | `delete` / `update` / `lock_tuple` の各 `TmResult`（`Updated` の `ctid`、`Deleted`、`SelfModified`、`BeingModified`（`wait = None`）、`WouldBlock`）。ロックの強度の昇格が MultiXact を作らない、2 人の共有で MultiXact（`pgrowlocks` 相当の表示用に `multixact.expand`）、更新が別ページに置かれるときの旧版の xmax の変化の検出（2 つのラッチの間に別の操作を差し込む。`sync_point`） |
| `heap/wal.rs` | `HEAP_LOCK` の例（§3.6）の固定値、「操作 → ページの写し → 同じ WAL を REDO → 一致」、2 回 REDO、FPW あり・なし、`lock_mode > 3` の拒否 |
| `txn/multixact.rs` | §6.8 |
| `analyzer`（`locking.rs`） | §6.4 の表の全行（SQLSTATE と文言）、`OF` の別名、複数の句のまとめ、RTE の種類ごとの扱い、外部結合の NULL 側、`locked_table_refs` が LK の一覧と一致 |
| `planner`（プランのスナップショット `plan_golden/`） | FOR 句つき SELECT の形（`Project ← Limit ← LockRows ← Sort ← ...`）、`LRowMark` の `cols` の範囲、ロック対象の列が刈り込まれない、`RecheckSpec` の式（WHERE と内部結合の ON のみ）、`UPDATE ... FROM` の `extra_cols`、`DELETE FROM t`（無条件）の `recheck: None` |
| `executor`（`FakeStore` で `TmResult` を強制） | `Update` / `Delete` の EPQ ループの全分岐（`Updated` → `lock_tuple` → 再評価 → 再試行、`Deleted`、`SelfModified`（`cmax == cid` と `!=`）、RR の 40001 の 2 種）、`LockRows`（`WouldBlock` → NOWAIT のエラー・SKIP の飛ばし、`Deleted`、`Updated`（RR）、`SelfModified`、EPQ の差し替えと `recheck`、`recheck` に落ちてもロックは残る）。M2 の `FakeStore::force_result` の列を使う |
| `session` | §6.7 の表の全行、`statement_needs_snapshot` の全文種、RR のスナップショットが最初の文で作られロックの前（`LockManager` のスタブで順序を確かめる）、`xact_snapshot` の登録がトランザクション終了で外れる（`oldest_xmin` が進む）、`COMMIT AND CHAIN` |

**並行の試験**（`yuzhu-core/tests/rowlock.rs`。`Cluster` を共有する複数スレッド。待ちは `Session` の API で本物の `LockManager` を通る）:

| テスト | 内容 |
|---|---|
| 送金の不変条件 | 10 口座、8 スレッドが 2 口座を `UPDATE`（RC と RR）。RC は待ちとデッドロック（40P01）を再試行、RR は 40001 を再試行。最後に合計が一定。`SELECT ... FOR UPDATE` で 2 口座をロックしてから更新する版も（ロックの順序を口座番号順/逆順にして、逆順ならデッドロックを検出して再試行） |
| 飢餓 | 1 行に 1 つの長い更新と多数の待ち手。タプルロックで先着順に通る（完了順の記録と FIFO の一致） |
| `SKIP LOCKED` のキュー | 8 ワーカーが `ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED` で行を取って削除。各行が高々 1 回だけ処理され、最後にすべて処理される |
| ロックの種類 | 4 強度の全組み合わせを `NOWAIT` で並行に試し、§3.4 の行列と一致 |
| 一意検査 | 8 スレッドが同じ 100 個のキーを挿入。各キーはちょうど 1 つのスレッドが成功し、残りは 23505。`INSERT` と `DELETE` と `UPDATE`（キー列）を混ぜる |
| キャンセル | 待ち中に `InterruptFlag` を立てる、`lock_timeout`、`statement_timeout`、停止要求。タプルロックが解放される（`LockManager::lock_status` で残りがないこと） |
| デッドロックの被害者 | 行ロックのデッドロックで先に待った方が 40P01（DETAIL に `ShareLock on transaction`）。タプルロックが残らない |

**ストレス**（`#[ignore]`。夜間）: 上の送金を VACUUM と機会的 pruning と並行（VC 章のワークロード）、8〜32 スレッド、10 分。

### 7.5 B+Tree の並行試験（RW-5）

テスト用の**同期点**（`util/sync_point.rs`。Cargo の feature `sync-points` が有効なときだけ `sync_point!("name")` が働き、通常ビルドでは何もしない。`unsafe` なし）。テストが「この点に着いたら待つ」「別のスレッドが進むまで進めない」を指定する。点: `btree.descend.parent_released`（降下で親を放した後）、`btree.leaf.latched`（葉を排他ラッチした直後）、`btree.unique.checked`（検査を終えた後・挿入の前）、`btree.split.before_parent_latch`（分割で親を取る前）、`btree.root.split.root_latched`（ルート分割でルートを取った後）。

| テスト | 内容 |
|---|---|
| 決定的な競り合い（B1） | A が `btree.leaf.latched` で止まっている間に B が同じ葉を分割する → A は右へ移動して正しいページに挿入する（木の検査器が通る） |
| 決定的な競り合い（B2・B4） | A の降下の後・親を探す前に、B が親を分割する／ルートを分割する → A は `find_parent` で右へ／メタから降り直して成功する |
| 同じキーの同時挿入（B6） | A が `btree.unique.checked` で止まっている間に B が同じキーを検査する → B は A の挿入が済むまで `P0` で待つ。A が先にコミットなら B は 23505、A が中断なら B が入る。右隣へ進むケース（重複キーが複数ページにまたがる）も |
| 一意検査の待ち（B5） | `InsertOutcome::WaitFor` を単体で返す（相手を実行中のままにする）。待った後の再試行で挿入される |
| デッドロックしないこと（B3） | 幅の広いキーで分割を頻発させ、16 スレッドがランダムなキーを 10 万件挿入。ウォッチドッグで 60 秒以内に終わる。最後に検査器（`check.rs`: 全 TID 集合がヒープと一致、リンク・high key・ダウンリンクの整合）が通り、挿入したキーがすべてスキャンで見つかる |
| モデル | `BTreeMap<(Key, Tid), ()>` との照合（M4 の性質テストを複数スレッドに拡張）。スキャンしながらの挿入で、見つかる TID に重複・取りこぼしがない |
| 後ろ向きスキャン（B8） | 後ろ向きにスキャンしながら別のスレッドが分割する |
| 変異 | 「一意検査のラッチを挿入前に手放す」「右へ移動しない」「ラッチの順序を逆にする」を入れると、それぞれ重複・検査器の失敗・ウォッチドッグのタイムアウトで検出されること |

### 7.6 クラッシュ試験（RW。M3 の層 1 `crash_sim/` に追加）

M3 の層 1 は 1 スレッドで複数の `Session` を順に操作する（待たない操作だけ）ので、**待たずに済む行ロックのワークロード**を足す:

- ワークロード 8（行ロック）: セッション A・B・C が 20 行の表で、`FOR UPDATE` / `FOR SHARE` / `FOR KEY SHARE`（共有 2 人で MultiXact）/ `FOR UPDATE SKIP LOCKED` / `NOWAIT`（失敗を期待）を、COMMIT / ROLLBACK / 実行中のまま混ぜて実行し、チェックポイントを挟む。ロック保持者の更新（自分のロックの行の UPDATE）も含める。
- クラッシュ点: M3 と同じ（I/O の通し番号を全探索）。
- **不変条件**（M3 の I1〜I12 に加えて）:
  - **I-RW1**: 再起動後、**どの行も待たずに UPDATE できる**（実行中だったトランザクションのロックはすべて無効。xmax が `HEAP_LOCK` の XID を指していても `xid_state == Aborted`）。
  - **I-RW2**: 確定したトランザクションの更新はモデルと一致（I4）。ロックだけをしたトランザクションの確定の有無でデータが変わらない。
  - **I-RW3**: 再起動後に作る MultiXact の ID は、再起動前に払い出した ID より大きい（`next_multi` の単調性）。`IS_MULTI` の xmax を持つタプルがあっても、その MultiXact は `Dead`。
  - **I-RW4**: すべてのページの `verify`・行ポインタ・`page_lsn <= WAL の末尾`（I8）に、`HEAP_LOCK` で書いたヘッダの整合（`IS_MULTI` なら `LOCK_ONLY`、`LOCK_ONLY` なら強度ビットあり）を足す。
  - **I-RW5**: WAL を読んで `HEAP_LOCK` の REDO の結果と、クラッシュ前の（確定した）ページの値が一致する（REDO の冪等・FPW の効き）。
- **変異テスト**（`DebugKnobs` に F0 が足す。§11）: `skip_heap_lock_wal`（`HEAP_LOCK` を WAL に書かない）+ `TornSectors` → I-RW4 か I8 で検出。`multixact_ignore_boundary`（`boundary` の判定を外し、表にない ID を「実行中のメンバーが 1 人いる」として扱う。再起動後に古い MultiXact が生きて見える）→ 待ちが起きて I-RW1 で検出（10 章 §6.9 の #10。以前の 10 章の名前 `multixact_dead_as_live` はこの名前に統合した）。`skip_tuple_lock`（タプルロックを取らない）→ 並行試験の「飢餓」で検出。**10 章 §6.9 が要求する次の 4 つもこの章が持つ（レビュー対応 R-09）**: `skip_update_wait`（#7。更新・削除が `BeingModified` で待たずに続行する）→ 10 章 S1 の合計・`bal[i]`、分離性 `lost-update`。`rr_skip_serialization_check`（#8。RR で `Updated` / `Deleted` を 40001 にせず最新版を更新する）→ 分離性 `rr-update-conflict`、S1 の RR。`epq_skip_recheck`（#9。待った後に WHERE を再評価しない）→ 分離性 `epq-single`。`unique_skip_wait`（#11。一意検査が `WaitFor` で待たずに挿入する）→ 10 章 S2、分離性 `unique-wait`。各変異を「既定のシード集合の少なくとも 1 つで検出する」ことをテストにする（M3 §7.5）。
- 層 2（`kill -9`、`crash_kill9.rs`）: 銀行振込のワークロードに `SELECT ... FOR UPDATE` で 2 口座をロックしてから更新する形を足す（TS-3 と共同）。

### 7.7 CI

既存のジョブ（00 §7 の TS-1・TS-2・TS-4）に、この章の spec と slt と Rust のテストを足す。`sync-points` feature つきのビルドを別ジョブにする（B+Tree の並行試験）。

---

## 8. 実装の分担と工数

00 §7 の WP の ID を使う（枝番は分けるときだけ）。日数は AI の実装エージェント 1 本の粗い見積もり（00 §7 の値を分けただけ。合計 19 日）。

| ID | 内容 | 依存 | 日数 | ファイル（持ち主は 00 §8） |
|---|---|---|---|---|
| RW-1a | `xmax.rs`（`decode_xmax`、`plan_lock`、`plan_update`、`stamp_updater`、`keys_changed`、`lockers_all_finished`、定数）と単体テスト | F0 | 1.5 | `storage/heap/xmax.rs` |
| RW-1b | `visibility.rs`（`visible` の `LOCK_ONLY` / `XMIN_FROZEN`、`xid_state`、`satisfies_update`、`fetch_dirty` / `tuple_state` の規則）、`TableDef::key_columns_from` | RW-1a、LK-3（`ProcArray`） | 1 | `storage/heap/visibility.rs`、`heap_store.rs` |
| RW-1c | `delete` / `update` / `lock_tuple` の `l1`〜`l3`、`lock.rs`（`TupleLockGuard`、`wait_for_holders`）、`UpdateOutcome.lockmode`、`TmResult::expect_simple` | RW-1a、RW-1b、LK-3、RW-2（`MultiXactTable` の `create` / `expand` は RW-1c の前に最低限あること） | 2 | `storage/heap_store.rs`、`storage/heap/lock.rs`、`storage/mod.rs` |
| RW-1d | `HEAP_LOCK` の WAL と REDO、`describe`、`recovery::dispatch` の口への登録 | RW-1a、F0 | 0.5 | `storage/heap/wal.rs` |
| RW-2 | `MultiXactTable`（`create` / `expand` / `on_xact_end` / `ensure_reserved`）、`control.next_multi` の使用、コミット / アボートからの `on_xact_end` の呼び出し（LK-3 の手順に 1 行） | F0 | 2.5 | `txn/multixact.rs` |
| RW-3a | FOR 句の構文（`LockingClause`）と解析（`BoundLockingClause`、§6.4 の全エラー）、`locked_table_refs` を LK に渡す | F0、M4 の analyzer | 1.5 | `sql/parser/select.rs`、`analyzer/{select,locking,bound}.rs` |
| RW-3b | 計画（`LockRows`、`LRecheck`、`RecheckSpec`、刈り込みの規則、`extra_cols`、EXPLAIN の名前） | M4 の planner、RW-3a | 1.5 | `planner/{logical,physical,build,physicalize}.rs` |
| RW-3c | 実行（`LockRows`、`Update` / `Delete` の EPQ、`ctid` の出力、`NOWAIT` / `SKIP LOCKED`） | RW-1c、RW-3b | 2 | `executor/nodes/{lock_rows,update,delete,seq_scan,index_scan}.rs`、`executor/dml.rs`（`delete_row`） |
| RW-4 | `Isolation` / RR、`statement_snapshot`（ロックの前の取得）、`first_snapshot_set`、`SET TRANSACTION` の規則、SERIALIZABLE の文言、`COMMIT AND CHAIN` の整合 | RW-3c、LK-3（`RegisteredSnapshot`）、LK-4（ロックの手順との順序） | 2 | `session/txn_ctl.rs`、`settings.rs` |
| RW-5 | B+Tree の洗い出しと修正（B1〜B7）、`InsertOutcome`、`insert_with_indexes` / `update_with_indexes` の待ち、`sync_point`、並行試験 | RW-1c、M4 の B+Tree（B1・B2） | 3 | `storage/btree/{insert,unique,search,split}.rs`、`executor/dml.rs`、`util/sync_point.rs` |
| RW-6 | RETURNING の仕上げ（§6.11）、27000、カタログ行の更新競合の文言（`expect_simple` の適用は C1 と調整） | M4、RW-3c | 1.5 | `analyzer/{dml,select}.rs`、`executor/nodes/{insert,update,delete}.rs` |
| テスト | §7 の slt・spec・Rust・クラッシュ試験。各 WP の中に含める（上の日数に含む）。専用の見積もりは RW-1c と RW-3c に 0.5 日ずつ、RW-5 に 1 日（上の 3 に含む） | | — | `tests/slt/m5/*`、`tests/isolation/*`、`yuzhu-core/tests/*` |

- **着手の順序**: F0 の後、RW-1a・RW-1d・RW-2・RW-3a は互いに独立で並列に進められる（`LockManager` に依存しない純粋な部分）。RW-1b・RW-1c は LK-3（`ProcArray`）と LK-1（`LockManager`）が要る。クリティカルパス（00 §1.3）は **LK-3 → RW-1 → RW-3 → FK-2**。RW-3c が終わるまで FK-2 は EPQ とロックを使えないので、RW-1c の `lock_tuple`（KEY SHARE の経路を含む）を RW-1 の早い段階で FK に渡す。
- **M4 のマージ後に始めるもの**: RW-3b・RW-3c（M4 の planner・executor が前提）、RW-5（M4 の B+Tree）、RW-6（M4 の RETURNING の欄）。
- **レビューの観点**: (1) 待つ前にラッチ・ピンが無いこと（デバッグビルドの検査が通る）、(2) xmax / infomask を `xmax.rs` 以外で書いていないこと（`grep` で `infomask` への代入を確認）、(3) 自分の XID の比較が 6 か所に集約されていること（§6.12）、(4) 判定が `xid_state` 基準で、スナップショットを使っていないこと、(5) `HEAP_LOCK` の REDO が冪等。

---

## 9. 未検証の点（実装前に確かめること）

| # | 内容 | 出典・確かめ方 |
|---|---|---|
| 1 | `heap_lock_updated_tuple`（PostgreSQL が KEY SHARE のとき連鎖の各版にロックを置く）の細部。yuzhu は**最新版だけ**に置く（RW-D4）。連鎖の途中の版に他者が `KEY SHARE` を後から置こうとして衝突する余地がないこと（途中の版は更新者がコミット済みなので、誰も途中の版を更新・削除できない） | `PG:src/backend/access/heap/heapam.c` の `heap_lock_updated_tuple`。【記憶】 |
| 2 | `ExecCheckXactReadOnly` が、複数のロック対象の強度が違うとき**最初の**強度の語を使うか | `PG:src/backend/executor/execMain.c`、`CreateCommandName`。【記憶】 |
| 3 | 同じ RTE を複数の `FOR` 句が指すときの待ちの方針の合成（`Error > Skip > Block`） | `PG:src/backend/parser/analyze.c` の `applyLockingClause`。【記憶】。実機で `for update of t skip locked for share of t nowait` を試す |
| 4 | `FOR UPDATE OF <JOIN の別名>` の文言（`... cannot be applied to a join`） | 【記憶】 |
| 5 | M4 の B+Tree（`06-btree.md`）が、§6.9 の B1〜B7 の対応をすでに含んでいるか、ラッチの順序・一意検査の `P0` の保持の規則と食い違わないか | `06-btree.md` の確定後に突き合わせ |
| 6 | `LockManager::wait_for_xact` が、自分の XID（`xid == id の持ち主の XID`）を渡されたとき待たずに戻ること、`still_running` が偽ならすぐ戻ること（LK-1 の仕様） | `01-lock-txn.md` |
| 7 | 外部キーの RI 検査が使う `lock_tuple(KeyShare, follow_updates = false)` と、RR の `crosscheck` の組み合わせで、RW-D4 の「旧版を返す」経路が FK 章の期待と一致するか | `07-foreign-key.md` |
| 8 | `UPDATE` が `Invisible` を返す条件のうち、行ポインタが `LP_UNUSED` / `LP_DEAD` になった場合（VC の pruning 後）。連鎖をたどる前に `read_buffer` したページの行ポインタが再利用されていないか（`priorXmax` の検査で守る。VC の規則「スキャンのスナップショットが horizon を止める」で起きない見込み） | VC の実装後に並行試験で確認 |
| 9 | `MultiXactTable` の `by_set` が大きくなる場合の上限。多数の異なる集合を作る長いトランザクション | 負荷試験（確認事項 M5-RW-Q9） |
| 10 | `Snapshot::is_own` / `Transaction::owns_xid` の LK 側の実装が、`XmaxEnv.me`（トップレベルの XID）と整合すること | `01-lock-txn.md` |
| 11 | `FOR UPDATE` と `CREATE INDEX`（Share ロックで書き手を止める）・`TRUNCATE`（AccessExclusive）の並行（行ロックの `RowShare` との衝突） | LK-4 の統合試験 |
| 12 | Extended Query で Parse と Bind の間に RR の最初のスナップショットを取る時点の差（RW-K7）が実用上問題にならないか | XQ-7 のドライバ検証 |

### 9.1 付録: 実機測定の方法と結果の索引

- 環境: `sandbox/pg.sh start --port 55477`（PostgreSQL **17.11**、C ロケール、trust 認証）。拡張 `pageinspect`（`heap_page_items`）と `pgrowlocks` を `CREATE EXTENSION`。複数セッションは名前付きパイプで `psql` を操作する自作のシェル（待ちは 0.5〜1 秒の遅れで観察）。分離性 spec は `tests/tools/isolation`（`yuzhu-isolation --print`。PostgreSQL に対して出力を生成）。
- 測った事実と本章の節:

| 測定 | 節 |
|---|---|
| `pageinspect` で 4 強度のロック・非キー UPDATE・キー UPDATE・DELETE・MultiXact の infomask / infomask2 | §3.2、§3.3、RW-D1、RW-D3 |
| 自分のロックの強化（KEY SHARE → SHARE → NO KEY UPDATE → UPDATE）と、ロックした行を自分で UPDATE したときの旧版・新版の xmax | §3.3、RW-D3 |
| 衝突行列（保持 7 × 要求 7） | §3.4、RW-K1 |
| 3 人の更新待ちの `pg_locks`（`tuple` と `transactionid`） | §5.5 |
| RR の更新・削除・ロックと 40001 の 2 種の文言（15 通り）、KEY SHARE が旧版を返す・返さない条件 | RW-D4、RW-D13、§7.2 `rr-conflict` |
| RR の最初のスナップショットがテーブルロックの待ちの前（SELECT・UPDATE とも）、RC は後 | RW-D10 |
| `SELECT 1` 以降の 25001、スナップショットを要する文・要しない文の一覧（約 50 文）、`COMMIT AND CHAIN`、READ WRITE / READ ONLY の 25001 | RW-D11、§6.7 |
| `FOR` 句の 0A000 / 42P01 / 42601 / 42809 の文言（約 40 文）、`OF` の別名・修飾、`FOR UPDATE NOWAIT SKIP LOCKED` の構文エラー | §6.4 |
| 読み取り専用の 25006（強度の語、`select 1 for update` は通る、`EXPLAIN` は通る、`LIMIT 0` は 25006） | RW-D12 |
| LockRows の位置（`ORDER BY` + `LIMIT`、`ORDER BY v DESC`）、EPQ（WHERE の再評価、削除、結合の相手、相関サブクエリ、InitPlan、拾わない行） | RW-D7、RW-D8、§5.6、§5.7 |
| `NOWAIT` / `SKIP LOCKED` はリレーションロックの待ちに効かない | §5.7 |
| 一意検査の待ち（A コミット・A 中断・3 人） | §5.9、§7.2 |
| デッドロック（交差更新・共有ロックの昇格）で先に待った方が 40P01 | §7.2 |
| RETURNING（集約・ウィンドウ・FROM 句・`*` の展開・サブクエリが自分の挿入を見ない・`old` / `new` は 17 に無い） | §6.11 |

---

## 10. 確認事項（仮決め・理由・変えたい場合の影響）

ID は `M5-RW-Q<n>`（`10-tests-plan.md` が M5-Q の通し番号に集約する）。ディスク形式に関わるものには ★。

| ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|
| M5-RW-Q1 | 更新者の xmax に `EXCL_LOCK` / `LOCK_ONLY` を立てない。更新者は MultiXact に入れず、新しい版の xmax は常に空（RW-D1、RW-D3）★ | 【実機】の infomask のとおり。D6 | 新しい版に自分のロックを引き継ぐ形（PostgreSQL と同じ）にするなら、`stamp_updater` と UPDATE の WAL（新版の xmax / infomask）に値が増える。観測できる差は無い |
| M5-RW-Q2 | KEY SHARE がコミット済みの非キー更新に当たったら、**最新版にロックして旧版を返す**（RW-D4） | RR の FK 検査で偽の 40001 を出さない。更新者を MultiXact に入れずに済む | 他の強度と同じく `Updated` を返す（40001 / EPQ）にすると、RR の外部キー検査が親の非キー更新に当たって失敗する（FK の RR の 40001 が増える）。実装は 5 行減る |
| M5-RW-Q3 | RR の最初のスナップショットはロック取得の**前**（RW-D10）。00 §5.1 d・D5 の「ロックはスナップショットより先」は RC の文のスナップショットに限る | 【実機】PostgreSQL がそうしている | 後にすると、RR の最初の文がテーブルロックを待つ間に相手がコミットした行が見える（PostgreSQL と違う。共有の `rr-first-snapshot` が落ちる）。実装は簡単になる（00 の手順のまま） |
| M5-RW-Q4 | 「最初のスナップショットを取った」は、`SELECT 1` を含むほぼすべての文で立てる（RW-D11）。M3 §5.2 e・§6.11.1 の「FROM のない SELECT は立てない」を訂正 | 【実機】 | M3 の記述のままにすると、`BEGIN; SELECT 1; SET TRANSACTION ISOLATION LEVEL REPEATABLE READ` が成功してしまい PostgreSQL と違う（Rust の `session` の単体テストと slt が 1 本ずつ変わる。実装は M3 の `txn_snapshot_taken` を 1 か所直すだけ） |
| M5-RW-Q5 | 入れ子の `FOR UPDATE`・FROM の副問い合わせへの `FOR` は 0A000（RW-D9、RW-K3） | M4 の planner の副問い合わせの展開と結びつく。黙ってロックを落とすのを避ける | 対応するなら、FROM の副問い合わせの引き上げ（pull-up）の前に `FOR` の対象を内側の表へ伝播する処理（+M）。ORM（Django の `select_for_update`、SQLAlchemy の `with_for_update`）は最上位に付けるので影響は小さい |
| M5-RW-Q6 | EPQ で外部結合の ON 条件は再評価しない（RW-K8） | 計画木の部分再実行をしないため | 外側の行の差し替えで ON の結果が変わる場合に、右側を NULL 拡張し直す（`recheck` に ON の式と「右側の列の位置」を持たせる。+S〜M） |
| M5-RW-Q7 | RETURNING のシステム列は `ctid` / `tableoid` / `xmin` / `cmin` だけ（`xmax` / `cmax` は 0A000）（§6.11） | `xmax` は DELETE で意味が分かれる | 許すなら DELETE の旧版の `xmax`（自分の XID）・UPDATE / INSERT の新版の 0 を返すだけ（+S） |
| M5-RW-Q8 | 00 の契約に足したもの: `LockTupleMode` に `PartialOrd, Ord`、`UpdateOutcome.lockmode`、`RowMark` / `LRowMark` の `table_name`、`ExecCtx.procs`、`LockDecision::Wait` の形（`i_am_member`）、`InsertOutcome` の `#[non_exhaustive]`（§11） | EPQ の強度、NOWAIT の文言、待ちの `still_running` に要る | 足さない場合の代案: `lockmode` は executor が自分で計算する（旧タプルの読み直しが要る）、`table_name` は `RelHandle` に名前を足す（M4 のファイル） |
| M5-RW-Q9 | MultiXact の表に上限を設けない（§6.8） | 同じメンバー集合は 1 つの ID を共有し、全員終了で掃除されるため | 上限（例: 100 万エントリ）を超えたら `53200`（out of memory）にする。M6 の永続化まで保てば足りる |
| M5-RW-Q10 | 仮想リレーション（`pg_locks`・`pg_roles`）への `FOR UPDATE` は黙って無視（RW-K4） | PostgreSQL は `pg_locks` で同じ（関数のビュー）。`pg_roles` だけ差 | `42809` にする（`cannot lock rows in view`）。PostgreSQL の `pg_locks` とは違うが明示的 |
| M5-RW-Q11 | B+Tree の分割で確保したページが、別のライターに先を越されて不要になったら孤児として残す（B7）。M4 の検査器（`check.rs`）が孤児を許す | 確保をラッチの後にしても、親の連鎖が伸びる場合の取り直しは残る | 孤児を出さないために、ラッチを取る前に上限数を確保する案（無駄な確保が増える）か、未使用ページの再利用（M6 の VACUUM と一緒に） |
| M5-RW-Q12 | カタログ行の書き込みは `wait: Some` で呼び、`XX000` の文言を PostgreSQL と同じにする（RW-D16）。そのために M4 の `catalog/store.rs` の書き込み関数に `&WaitCtx` を足す | PostgreSQL の `simple_heap_update` と同じ | `wait: None` で呼ぶ（待たずに `BeingModified` を内部エラーにする）と、DDL 同士が稀に衝突したとき PostgreSQL なら待って成功するところで失敗する。引数の追加は不要になる |
| M5-RW-Q13 | `SERIALIZABLE` の文言は 00 §3.8 の `SERIALIZABLE isolation level is not supported yet`（RW-D17）。M3 の文言（`transaction isolation level "serializable" is not supported yet`、HINT つき）を置き換える | 契約が固定している | M3 の文言のままにする場合は 00 §3.8 を直す。slt は `onlyif yuzhu` でエラーの SQLSTATE だけを見るので影響は小さい |
| M5-RW-Q14 | Extended Query の RR の最初のスナップショットは Bind の直前（RW-K7）。PostgreSQL は Parse で取りうる | Parse はロックを取らない（D21）ので、取るならロックの前に取る形が自然 | Parse で取る形にすると、`Session::note_snapshot_use` に加えて `xact_snapshot` の作成も Parse に移る（XQ-2 の変更）。差は「Parse と Bind の間の他者のコミットが見えるか」だけ |
| M5-RW-Q15 | `ensure_reserved` の閾値（`MULTI_RESERVE_LOW = 512`、`MULTI_PREFETCH = 1024`） | ラッチの外で確保し、同時のバックエンド数（数十）が使い切らない余裕 | 接続数の上限を上げる（M6）ときは `MULTI_RESERVE_LOW` を接続数以上にする |

---

## 11. 契約への変更依頼

00 を黙って変えず、ここに挙げる。**依頼 1〜5 は 00 への追加・訂正**、**6〜9 は他章・M4 のファイルを持つ担当への依頼**、**10〜12 は M3 / M4 の記述の訂正**。

| # | 対象 | 内容 | 理由 |
|---|---|---|---|
| 1 | 00 §5.1 d（文の実行） | **RR の最初のスナップショットは、ロックの取得（d の 1〜2）の前に取る**（RC は 00 のまま）。`Session::statement_snapshot`（§4.5）に実装。00 §5.1 の「ロックはスナップショットより先」の注に「RC の文のスナップショットに限る」を足す | RW-D10。【実機】 |
| 2 | 00 §5.1 b | `FOR` 句つき SELECT の 25006 は解析後（ロック対象が 1 つ以上あるとき）に判定する。書き込む文（INSERT / UPDATE / DELETE 等）は 00 のまま（生のパース木） | RW-D12 |
| 3 | 00 §3.7（更新者の xmax） | 「PostgreSQL が更新者に `EXCL_LOCK` を立てるかは（未検証）」→ **立てない（【実機】確認済み）**に直す。新しい版の xmax は常に空（RW-D3）を足す | RW-D1、RW-D3 |
| 4 | 00 §4.3 / §4.5 / §4.6 | (a) `LockTupleMode` に `PartialOrd, Ord` を derive、(b) `UpdateOutcome` に `lockmode: LockTupleMode`、(c) `RowMark` と `LRowMark` に `table_name: String`、(d) `ExecCtx` に `procs: &'a Arc<ProcArray>`（`wait_for_xid` の `still_running` 用）、(e) `InsertOutcome` に `#[non_exhaustive]`、(g) `LockTupleMode` と `MultiMember` に `Hash`、(f) `UpdateOutcome` / `LockOutcome` の定義を 00 §4.5 に明記 | EPQ・NOWAIT・待ちに要る。互いに独立の追加で、既存の署名は変わらない |
| 5 | 00 §3.8 | SERIALIZABLE の文言を採用（M3 の文言を置き換える。RW-D17）。`tuple concurrently deleted` / `tuple already updated by self`（XX000）を行に足す。`SET TRANSACTION` の 25001 の文言に `transaction read-write mode must be set before any query`（【実機】確認済み）を足す | RW-D16、RW-D17 |
| 6 | F0（`catalog/mod.rs`、`catalog/reader.rs`、`storage/mod.rs` の `RelHandle`） | `TableDef.key_columns: Vec<bool>` と `RelHandle.key_columns: Arc<[bool]>` の欄を足す。計算関数 `TableDef::key_columns_from`（§4.2）は RW-1 が書き、`TableDef` を作る場所から呼ぶ | 00 §4.5 が要求。M4 のファイル |
| 7 | M4 C1（`catalog/store.rs`） | 書き込み系の関数（`drop_table` など、`delete` / `update` を呼ぶもの）に `wait: &WaitCtx<'_>` を足し、`expect_deleted` を `TmResult::expect_simple` に置き換える（RW-D16、§6.10）。`create_table` の `insert` は待たないので不要 | M5-RW-Q12 |
| 8 | F0（`DebugKnobs`、`control.rs`） | `DebugKnobs` に `skip_heap_lock_wal`、`multixact_ignore_boundary`、`skip_tuple_lock`、`skip_update_wait`、`rr_skip_serialization_check`、`epq_skip_recheck`、`unique_skip_wait` の 7 項目を足す（§7.6。名前は 10 章 §6.9 と一致。R-09）。`control.rs` の `update` / `update_for_shutdown` の単調性の検査に `next_multi` を加える（下げない） | 変異テストと §3.7 |
| 9 | LK（`01-lock-txn.md`） | (a) 00 §5.2 の手順 1 に「`multixact.on_xact_end(xid)` を ProcArray から外した後に呼ぶ」を明記、(b) LK-4 の「生のパース木からリレーションロックの一覧を作る」は、`FOR` 句の対象の判定に RW の `analyzer::locking::locked_table_refs(&SelectStmt)` を呼ぶ、(c) `wait_for_xact` は呼び出し元バックエンドの XID に対して待たないこと、(d) RR の最初のスナップショットを取る位置（依頼 1） | §6.4、§6.2 |
| 10 | M3 の設計（§5.2 e、§6.11.1）と実装（`session.rs` の `txn_snapshot_taken`） | 「FROM のない SELECT はスナップショットを取った扱いにしない」を訂正し、RW-D11 の表に従う（`SELECT 1` でも 25001）。RW-4 が直す | RW-D11 |
| 11 | M4 の B+Tree（`06-btree.md`、`storage/btree/*`） | §6.9 の B1〜B7 を M4 の実装の時点から入れておけるなら入れる（特に B2 の `find_parent` の「右へ移る・メタから降り直す」、B6 の `P0` のラッチの保持）。入れてあれば RW-5 は検証だけになり縮む。`check.rs` は孤児ページ（B7）を許す | D18、M5-RW-Q11 |
| 12 | VC（`03-vacuum.md`） | (a) `xmax::decode_xmax` / `lockers_all_finished` を読み取りに使う（`satisfies_vacuum` の `LOCK_ONLY` の扱い。`LockerMulti` は `expand` で）、(b) RW の `delete` / `update` と REDO が呼ぶのは **`Page::set_prunable(xid)`（`storage/page.rs`。`pd_prune_xid` の更新。VC-1 が定義する。03 §4.1）**。`prune::note_prunable` という名前は使わない（レビュー対応 R-01）、(c) `bulk_delete` がヒープのラッチ・ピンを持たずにインデックスを処理する（B10） | §5.2、§6.9 |
| 13 | 00 §4.4・01 §5.12（レビュー対応 R-06） | **`MultiXactTable::new(control) -> MultiXactTable`（`Result` を返さない）**に確定した。制御ファイルの値を読むだけで失敗しない。`Cluster::prepare` が REDO の前に作る（01 §5.12）。00 §4.4 と §4.3 の署名を直した | LK-3 と RW-2 が別々に実装して組み立てが合わなくなるのを防ぐ |
| 14 | 00 §4.6・07 R1（R-05） | `executor/dml.rs` の 3 関数の署名を `update_with_indexes(ctx, rel, w, snap, tid, old_row, new_row, crosscheck)`・`delete_row(ctx, rel, w, snap, tid, old_row, crosscheck)` に確定（§4.6）。`ri::after_*` と `ri::finish_statement` の呼び出しを受け入れた。`ExecCtx` に `procs` を足した（00 §4.6）。`ctx.locks` と `ctx.procs` は `wait_for_xid` だけが使う | FK-2・RW-3c・RW-5 が同じファイルを別形で実装するのを防ぐ |
| 15 | 00 §4.1・§8.2（R-01、R-09） | `xmax.rs` に `invalidate_xmax` と `with_xmin_frozen` を足した（§4.1。VC が使う）。`DebugKnobs` に `skip_update_wait`・`rr_skip_serialization_check`・`epq_skip_recheck`・`unique_skip_wait` を足す（§7.6。計 7 項目） | VC の凍結。10 章 §6.9 の変異スイッチ |

**00 に載っていない、この章が作る・直すファイル**: `analyzer/locking.rs`（新規。FOR 句の解析と `locked_table_refs`）、`util/sync_point.rs`（新規。テスト用の同期点）、`storage/heap/tuple.rs`（読み取りの定数の参照だけ。`HEAP_XMIN_FROZEN` などは `xmax.rs` に置き、`tuple.rs` は変えない）。いずれも 00 §8 の「RW の持ち物」の延長として扱う。

