# yuzhu M4 設計 08: シーケンスと SERIAL / IDENTITY

この章は M4 の担当 Q1 の設計書です。シーケンスの格納・WAL・REDO、`nextval` / `currval` / `lastval` / `setval`、`CREATE` / `ALTER` / `DROP SEQUENCE`、`SERIAL`、`GENERATED ... AS IDENTITY`、`OVERRIDING`、コミット時の WAL の flush を扱います。**契約は `00-contracts.md` が唯一の正**で、この章はその具体化です（食い違いは「決定」と「00 への変更提案」に書いた）。書き方は `00 §1.2` に従います。

- 前提（必読）: `00-contracts.md`（特に §11.1・§13.3・§13.4・§14.1・§14.3）、`m3.md`（コミットの順序、WAL を書くページ変更の形、REDO、チェックポイント）、`m2.md`（ページ、ヒープタプル、カタログ）
- 調査（根拠）: `spec/research/m4-btree.md` §1.10・§1.11・§8.2〜§8.4、`m3-tx-semantics.md` §7
- 正解の基準は PostgreSQL 17。PostgreSQL のソースは REL_17_STABLE を `PG:<path>` と略す。**エラー文言と SQLSTATE、状態の値（`last_value` / `log_cnt` / `is_called`）、クラッシュ後の値は、PostgreSQL 17.11 の実機で確かめた**（検証の方法は §9 の冒頭）。「（未検証）」と付けたものは確かめていない。
- 参照したソース: `PG:src/backend/commands/sequence.c`（`nextval_internal`、`do_setval`、`init_params`、`AlterSequence`、`process_owned_by`、`seq_redo`）、`PG:src/backend/parser/parse_utilcmd.c`（`generateSerialExtraStmts`、`transformColumnDefinition`）、`PG:src/backend/rewrite/rewriteHandler.c`（IDENTITY の規則）

---

## 1. 範囲

### 1.1 M4 で入れるもの

| 分類 | 内容 |
|---|---|
| 格納 | relkind `S` の 1 ページのリレーション。タプル 1 個（`last_value`・`log_cnt`・`is_called`）を MVCC を使わずにその場で書き換える。WAL は `SEQ` rmgr の `SEQ_LOG`（ページ全体を毎回作り直す） |
| 関数 | `nextval(regclass)`、`currval(regclass)`、`lastval()`、`setval(regclass, bigint)`、`setval(regclass, bigint, boolean)`。`CACHE n`、`CYCLE`、昇順・降順、`smallint` / `integer` / `bigint` の 3 種類 |
| DDL | `CREATE SEQUENCE [IF NOT EXISTS] name [AS type] [INCREMENT [BY] n] [MINVALUE n \| NO MINVALUE] [MAXVALUE n \| NO MAXVALUE] [START [WITH] n] [RESTART [[WITH] n]] [CACHE n] [[NO] CYCLE] [OWNED BY table.col \| NONE]`、`ALTER SEQUENCE [IF EXISTS] name options`（上と同じオプション。`OWNER TO` は何もしない）、`DROP SEQUENCE [IF EXISTS] name [, ...] [CASCADE \| RESTRICT]` |
| SERIAL | `smallserial` / `serial2`、`serial` / `serial4`、`bigserial` / `serial8`（CREATE TABLE の列の型）。暗黙のシーケンス `<表>_<列>_seq`、`NOT NULL`、`DEFAULT nextval(...)`、`OWNED BY`、`pg_depend` |
| IDENTITY | `GENERATED { ALWAYS \| BY DEFAULT } AS IDENTITY [(シーケンスのオプション)]`（CREATE TABLE の列）、`INSERT ... OVERRIDING { SYSTEM \| USER } VALUE`、`UPDATE` での `ALWAYS` 列の制約、`TRUNCATE ... RESTART IDENTITY` との連携 |
| 耐久性 | コミット時に、そのトランザクションが払い出した値を覆う WAL までを flush する（XID を持たないトランザクションでも）。クラッシュ後に欠番は最大 32 個（`CACHE` ならその分も）。**払い出した値を二度払い出さない（不変条件 I15）** |
| 見え方 | `SELECT * FROM シーケンス`（`last_value`・`log_cnt`・`is_called`）、psql の `\ds`、`\d シーケンス`、`pg_sequence`、`pg_class`（relkind `S`）、`pg_depend` |
| 任意 | `pg_get_serial_sequence(text, text)`（§4.5。M4 後半・任意） |

### 1.2 M4 で入れないもの（実行すると `0A000`）

| 機能 | 理由・メッセージ |
|---|---|
| `TEMPORARY` / `TEMP` / `UNLOGGED` シーケンス | 一時テーブルとログなしリレーションが M4 にない。`temporary sequences are not supported yet` / `unlogged sequences are not supported yet` |
| `ALTER SEQUENCE ... RENAME TO` / `SET SCHEMA` / `SET LOGGED` / `SET UNLOGGED` | `ALTER TABLE` と同じく M4 は `OWNER TO` だけ（D-12）。`ALTER SEQUENCE ... OWNER TO` はロールの存在を確かめて何もしない |
| `ALTER TABLE ... ADD COLUMN serial`、`ALTER TABLE ... ADD GENERATED`、`ALTER COLUMN ... SET GENERATED / RESTART` | `ALTER TABLE` の範囲外（D-12） |
| IDENTITY のオプションの `OWNED BY`、`LOGGED` / `UNLOGGED` | PostgreSQL は受け付けるが意味がない |
| `DISCARD SEQUENCES` / `DISCARD ALL` | `DISCARD` 自体が M4 にない。セッションの状態を捨てる口（`SeqSession::discard`）だけ用意する |
| `GRANT` / `REVOKE ... ON SEQUENCE`、`COMMENT ON SEQUENCE` | M6 |
| `pg_sequences`・`pg_sequence_last_value()`・`pg_sequence_parameters()`・`nextval(text)` の独立した関数 | 作らない。`nextval('x'::text)` は `text → regclass` の暗黙キャスト（`09-types-functions.md`）で動く |
| 複数ライターの下での `ALTER SEQUENCE` と `nextval` の排他 | テーブルロックがない（D16）。M5 |

### 1.3 他の章との境界

この章の担当 Q1 が編集するファイルは `00 §17` のとおり（`storage/sequence.rs`、`ddl/sequence.rs`、`analyzer/ddl.rs` の SERIAL / IDENTITY、`types/ops.rs` のシーケンス関数）。次を**追加で**持つ（`00 §4` に無いファイル。`00 への変更提案`）。

- `catalog/seq_params.rs`: シーケンスのパラメータの検証（PostgreSQL の `init_params`）。パーサ・アナライザ・`ddl` が共有する純粋関数
- `executor/seq.rs`: セッションが持つシーケンスの状態（`currval` の表、`lastval`、`CACHE` の先取り値）と、`RuntimeInfo` の 4 つのメソッドの実装本体
- `txn/manager.rs` の `Transaction` と `TxnManager::finish_without_xid`（`00 §16` で 08 の持ち主。`started_at` も 08 が足す）

他の担当に頼むことは §11 にまとめた。主なもの:

| 相手 | 頼むこと |
|---|---|
| `07-catalog-ddl.md`（C1） | `pg_sequence` と `pg_depend` のカタログ、`CatalogStore` のシーケンス用メソッド（§4.8）、`TableDef` の `sequence` / `identity_seqs` の読み込み、CREATE TABLE・DROP TABLE・TRUNCATE からこの章の関数を呼ぶ口、`DEFAULT` 式の中の `regclass` 定数への依存の記録、シーケンスに対する DROP TABLE / DROP INDEX / CREATE INDEX / TRUNCATE / ALTER TABLE のエラー |
| `03-parser-analyzer.md`（S1・N1） | 構文（§4.8 の AST）、INSERT / UPDATE の解析から `identity_insert_rule` / `identity_update_rule`（§4.9）を呼ぶこと、シーケンスへの INSERT / UPDATE / DELETE の拒否（`42809`） |
| `09-types-functions.md`（T3） | 関数の行（§4.5）、`regclass` の入力を解析時に評価すること、`text → regclass` のキャスト、`volatile` を畳み込まないこと |
| `10-explain-copy-compat.md`（O1・E1） | COPY が IDENTITY の暗黙の DEFAULT を使うこと、`\ds` と `\d シーケンス` が使う SQL、`regclass` 定数の逆変換 |
| M3 の持ち主（B・C・E・R・S） | `Page` の special 付き初期化、`page_mut_hint()` が呼んだ時点で dirty にすること、`Wal::redo_lsn()` が起動直後から制御ファイルの値を返すこと、`recovery::dispatch` への `RmgrId::Seq` の追加、`DebugKnobs` の追加、`Session` の文の終わりでの `Transaction` への反映 |

---

## 2. 決定（この章で追加で決めたこと）

### 2.1 決定の一覧

選択肢と出典を表にする。`D8-n` はこの章の決定番号。`00` の D-9（シーケンスの格納）・D-10（SERIAL / IDENTITY の書き換え）を具体化したもので、食い違いはない（§2.2 に調査や実機との食い違いを集めた）。

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| D8-1 | 格納 | PostgreSQL と同じ 1 ページのリレーション（m4-btree §8.2）／カタログの行に持つ | **前者**。ページは special 8 バイト（`SEQ_MAGIC`）、タプルは 1 個、`xmin = Xid::FROZEN`、`t_ctid = (0,1)`。MVCC は使わず、ページの排他ラッチの下でその場で書き換える | `SELECT * FROM シーケンス` がヒープ走査でそのまま動く。PostgreSQL の見え方（`ctid = (0,1)`、`xmin = 2`、`relpages = 1`）と一致する |
| D8-2 | ロックと XID | ライターロックを取り XID を割り当てる／取らない（m3-tx-semantics §7） | **取らない・割り当てない**。シーケンスごとの排他ラッチ（ページのコンテンツラッチ）だけ。`SELECT nextval(...)` は書き込む文として扱わない（M3 §5.2 d の分類のまま） | 取ると `SELECT nextval` が他のトランザクションの終了を待つ。XID をライターロックなしで割り当てると M3 の「実行中のライターは高々 1 つ」が崩れる |
| D8-3 | WAL | PostgreSQL と同じ `SEQ_LOG`（WILL_INIT、タプル全体、`REDO` は常に無条件に上書き）／差分レコード | **前者**。レコードの XID は 0（`init` と `reset` だけ呼び出し側の XID）。ブロックは 1 個、メインデータは空 | REDO が「最後の `SEQ_LOG` の状態を置くだけ」になり、順序と冪等性の議論が要らない。`WILL_INIT` は FPW の対象外なので M3 の FPW の経路とも干渉しない |
| D8-4 | `nextval` の計算 | PostgreSQL のループをそのまま写す／同値の閉じた式 | **閉じた式（O(1)）**（§4.3 `plan_fetch`）。PostgreSQL のループを写した参照実装を `#[cfg(test)]` に置き、乱数で突き合わせる（設計時に約 285 万ケースで一致を確認済み） | `CACHE 1000000` や `increment` が大きいときにループが長くなるのを避ける。振る舞い（`log_cnt`、WAL に書く値、上限の判定、`CYCLE`）は PostgreSQL と同値 |
| D8-5 | コミット時の flush | 自分が書いた WAL の LSN だけを flush する（PostgreSQL。`GetTopTransactionId()` で XID を割り当てて `XactLastRecEnd` を flush させる）／**払い出した値を覆う WAL の LSN（ページの LSN）まで flush する** | **後者**。`SeqRun.wal_lsn` は「その呼び出しの後のページの LSN」。`Transaction.wal_flush_upto` に最大値を覚え、コミットで flush する | PostgreSQL では、`log_cnt` の余りで払い出す 2 番目のトランザクションは WAL を書かないので何も flush せず、1 番目のトランザクションの未 flush の `SEQ_LOG` に依存したままコミットできる（クラッシュ後に値が重複しうる。§5.10）。ページの LSN は 1 つの原子的な読み取りで得られ、`wal.flush` は flush 済みなら何もしないので費用はほぼ 0 |
| D8-6 | ROLLBACK の flush | 中断でも flush する／しない（PostgreSQL と同じ） | **しない**。XID を持たない中断は何もせず、XID を持つ中断は M3 のまま（`pending_creates` があるときだけ flush） | 中断したトランザクションが見た値は外部に残らない前提（PostgreSQL と同じ保証）。変えたい場合は `[08-Q4]` |
| D8-7 | dirty と REDO 点の読み取りの順 | `GetRedoRecPtr()` を読んでから `MarkBufferDirty`（PostgreSQL）／**dirty を先に立ててから** `redo_lsn()` を読む | **後者**。ページの排他ラッチを取った直後に `page_mut_hint()`（呼んだ時点で dirty）→ `wal.redo_lsn()` の順 | 逆順だと、チェックポイントの REDO 点の決定と dirty ページの集合の取得がその間に入ったとき、ログなしで払い出した値がどのチェックポイントにも WAL にも覆われなくなりうる（§5.10。PostgreSQL のソースの読みから。実機での再現は未検証） |
| D8-8 | `ALTER SEQUENCE` のトランザクション性 | 新しい relfilenode を作ってトランザクショナルにする（PostgreSQL。`OWNED BY` 以外は常に作り直す）／その場で書き換える（00 の `reset`、m4-btree §8.2） | **その場で書き換える**。`pg_sequence` の行の更新はカタログなのでトランザクショナル、シーケンスの状態は**ロールバックされない**。常に `log_cnt = 0` にして `SEQ_LOG` を 1 本書く | 00 の `SequenceStore::reset` と D-9 に従う。新しい relfilenode は他のセッションの `nextval` を待たせる表ロック（M5）がないと、`ALTER` と同時の払い出しが失われる（重複の原因）。変えたい場合は `[08-Q5]` |
| D8-9 | `ALTER` / `TRUNCATE ... RESTART IDENTITY` の後の他セッションのキャッシュ | PostgreSQL は relfilenode の変化で捨てる／同じ効果を別の方法で | `SequenceStore::reset_generation()`（クラスタ全体で 1 つの `AtomicU64`、`reset` が +1）。セッションは文の最初に値を見て、変わっていたら**全シーケンスの先取り分を捨てる**（`cached = last`）。`currval` の状態は触らない | その場の書き換えでは relfilenode が変わらない。全部捨てても欠番が増えるだけで安全 |
| D8-10 | SERIAL の DEFAULT の保存 | 利用者が書いた形（`nextval('t_id_seq')`。名前で都度解決）／**OID 形式 `nextval('<oid>'::regclass)`** | **OID 形式**。`pg_attrdef.adbin` にこのテキストを保存し、`pg_get_expr` は parse → analyze → deparse（D-21）で `nextval('t_id_seq'::regclass)` に戻す | 検索パスや名前の変更に左右されない（PostgreSQL は regclass 定数を OID で持つ。実機で確認）。ユーザーが書いた DEFAULT は M2 のとおりテキストのまま保存する（`[08-Q9]`） |
| D8-11 | IDENTITY の DEFAULT | `pg_attrdef` に `nextval` を持つ／**DEFAULT を持たない** | **持たない**。`pg_attribute.attidentity`（`a` / `d`）と `pg_depend`（シーケンス → 列、`i`）だけ。`atthasdef = false`。解決は `TableDef.identity_seqs` | PostgreSQL と同じ（実機で `atthasdef = f`、`pg_attrdef` に行なし）。INSERT で「値の指定なし」を `OVERRIDING` と合わせて扱うため、DEFAULT 式が別にあると区別できない |
| D8-12 | パラメータの検証 | アナライザと `ddl` に別々に書く／1 つの純粋関数 | **`catalog/seq_params.rs` の `init_params`**。CREATE はアナライザ、ALTER は `ddl/sequence.rs`（実行時に現在の状態を読んでから）が呼ぶ | ALTER の `RESTART` と `MINVALUE` / `MAXVALUE` の検査は現在の `last_value` を使うので実行時にしかできない。検査の順序と文言を 1 か所に置く |
| D8-13 | セッションの状態 | `Session` に直接持つ／`executor/seq.rs` の `SeqSession` | **後者**。`RuntimeInfo` の 4 つのメソッドは `SeqRuntime`（`SeqSession` と `SequenceStore` と `CatalogReader` を借りる）に委ねる | S（`session.rs`）の担当が最小の追加で済む。単体で試験できる |
| D8-14 | 暗黙のシーケンスの名前 | PostgreSQL の `ChooseRelationName`（既存のカタログだけを見る）／同じ文の中で選んだ名前も避ける | **後者**（PostgreSQL の上位互換）。`make_object_name`（`analyzer/ddl.rs` に既にある）で `<表>_<列>_seq`、衝突したら `seq1`、`seq2` ... | PostgreSQL は長い列名で同じ文の 2 列が同じ名前になっても気づかない（ソースのコメントにある）。M4 では一致させる必要がない |
| D8-15 | `pg_get_serial_sequence` | 作らない／作る | **任意**（M4 後半）。仕様は §4.5 | ORM が使う。psql・pgbench は使わない |

### 2.2 調査・実機・00 との食い違い

| 論点 | 食い違い | 決定 |
|---|---|---|
| シーケンスの行型 | `m4-btree.md` §8.2 は「pg_type 行（シーケンスは行型を持つ【記憶】）」。PostgreSQL 17 実機は `pg_class.reltype = 0` で、`pg_type` に行なし | **行型を作らない**（`reltype = 0`）。調査の記述は誤り |
| `ALTER SEQUENCE` | `m4-btree.md` §8.2 は「リレーションファイルを作り直す場合がある【記憶】」。PostgreSQL 17 は `OWNED BY` 以外の**どのオプションでも常に**新しい relfilenode を作る（`PG:sequence.c` の `init_params` のコメントと `AlterSequence`。ロールバックで状態が戻ることを実機で確認） | 00 の `reset`（その場の書き換え）に従い、**差として明記**する（D8-8、`[08-Q5]`）。ロールバックしても `RESTART` の結果が残る |
| 書き込みロックと XID | `m4-btree.md` §8.2 と `m3-tx-semantics.md` §7 は一致（取らない。XID の代わりにフラグ） | 一致。ただしフラグが覆う範囲を D8-5 のとおり広げる |
| IDENTITY のメッセージ | `m4-btree.md` §8.4 は文言を【記憶】とする | 実機で確認した文言を §6.1 に書いた。`UPDATE` は `column "id" can only be updated to DEFAULT`（`428C9`） |
| `ALTER SEQUENCE` の後の `log_cnt` | PostgreSQL は `START` だけを変えたとき `log_cnt` を保ったまま新しいファイルに WAL を書く（`PG:init_params`: `log_cnt` を 0 にするのは増分・最大・最小・CYCLE・CACHE・RESTART を変えたとき）。その後クラッシュすると払い出した値が戻りうる | yuzhu は ALTER のたびに 0 にする（D8-8）。`SELECT * FROM seq` の `log_cnt` が PostgreSQL と違うことがある（slt では `log_cnt` を比べない） |
| 00 §13.3 の `SequenceHandle` | 名前を持たない。エラーメッセージ（`nextval: reached maximum value of sequence "x" (3)`）に名前が要る | `name: String` を足す（§11） |

---

## 3. ディスク上の形式

すべてリトルエンディアン。ページ・ヒープタプル・WAL のレコードの形式は `m2.md` §3、`m3.md` §3 のとおりで、ここでは**シーケンスに固有の値**だけを決める。

### 3.1 リレーションのファイル

- ユーザーが作るシーケンスの `RelFileLocator` は表と同じ `(1663, <db の OID>, RelFileNumber(pg_class.oid))`（`base/<db>/<oid>`）。マップされない。
- ファイルは**常に 1 ブロック**（8192 バイト）。ブロック 0 にだけタプルがある。フォーク（FSM など）は持たない。伸ばすのは `SequenceStore::init` の 1 回だけ。

### 3.2 ページ（ブロック 0）

ヘッダは `m2.md` §3.3 のとおり。シーケンス固有の値:

| 項目 | 値 |
|---|---|
| `pd_special` | **8184**（special 領域 8 バイト。`MAXALIGN(sizeof(u32))`）。`pd_special <= 8192`・8 の倍数の検査（`Page::verify`）を満たす |
| special 領域（8184..8192） | `magic: u32 = SEQ_MAGIC (0x1717)`（LE）、続く 4 バイトは 0 |
| `pd_lower` | 28（行ポインタ 1 個） |
| `pd_upper` | 8120（= `8184 - MAXALIGN(57)`） |
| 行ポインタ 1 | `lp_off = 8120`、`LP_NORMAL`、`lp_len = 57`。`u32 = 8120 \| (1 << 15) \| (57 << 17) = 0x00729FB8` |
| 行ポインタ 2 以降 | なし（シーケンスのタプルは 1 個で動かない。`add_item` の「行ポインタを再利用しない」規則と矛盾しない） |

`Page::init_special(special_size)`（`06-btree.md` が定義する special 付きの初期化。名前が違えばそちらに合わせる）は、全体を 0 にして `pd_lower = 24`、`pd_upper = pd_special = 8192 - special_size`、`pd_pagesize_version = 0x2001` を書く。そのあと `add_item` が `pd_upper` から下へタプルを置く。

### 3.3 タプル（57 バイト、`MAXALIGN` で 64 バイト）

`m2.md` §3.5 のヘッダ（35 バイト + パディング 5 = `t_hoff` 40）に続けて 3 列を置く。

| オフセット | 大きさ | 内容 |
|---|---|---|
| 0 | 8 | `t_xmin = 2`（`Xid::FROZEN`。M2 では予約で使われていなかった。M4 でシーケンスのタプルだけが使う。`committed_in_snapshot` は FROZEN を clog なしで COMMITTED とみなす。`m2.md` §6.6） |
| 8 | 8 | `t_xmax = 0` |
| 16 | 4 | `t_cmin = 0` |
| 20 | 4 | `t_cmax = 0` |
| 24 | 6 | `t_ctid = (0, 1)`（ブロック 0、オフセット 1） |
| 30 | 2 | `t_infomask2 = 3`（列数） |
| 32 | 2 | `t_infomask = 0x0800`（`HEAP_XMAX_INVALID` だけ。`HASNULL` なし、`HASVARWIDTH` なし） |
| 34 | 1 | `t_hoff = 40` |
| 35 | 5 | パディング（0） |
| 40 | 8 | `last_value`（`i64`。`int8` の整列 8 に合う） |
| 48 | 8 | `log_cnt`（`i64`） |
| 56 | 1 | `is_called`（`0` / `1`） |

- `SeqState` はこの 3 値。`t_xmin` を除いてヒープの通常のタプルと同じ形式なので、`SELECT * FROM シーケンス` は通常のヒープ走査（`HeapStore::begin_scan` と `deform_tuple`）で動く。可視性は `xmin = FROZEN`・`xmax` 無効で常に見える。`xmin` は `2`、`ctid` は `(0,1)` と見える（PostgreSQL 17 の実機と同じ）。
- 読み出し側（`storage/sequence.rs` の `read_state`）は次を検査し、破れていたら `XX001`: special の `magic == SEQ_MAGIC`（文言は PostgreSQL と同じ `bad magic number in sequence "%s": %08X`）、`max_offset() == 1`、`lp_len == 57`、`t_hoff == 40`、`t_infomask2 & 0x7FF == 3`、`is_called ∈ {0, 1}`。

### 3.4 例: 作ったばかりのシーケンス `(last_value, log_cnt, is_called) = (1, 0, false)`

`CREATE SEQUENCE s` の直後の 8192 バイト（`pd_lsn` は `SEQ_LOG` の終端 LSN。例では `L`）。

| オフセット | 長さ | 内容 |
|---|---|---|
| 0 | 8 | `pd_lsn = L` |
| 8 | 2 | `pd_checksum`（書き出し時に入る） |
| 10 | 2 | `pd_flags = 0` |
| 12 | 2 | `pd_lower`: `1C 00` |
| 14 | 2 | `pd_upper`: `B8 1F` |
| 16 | 2 | `pd_special`: `F8 1F` |
| 18 | 2 | `pd_pagesize_version`: `01 20` |
| 20 | 4 | `pd_prune_xid = 0` |
| 24 | 4 | 行ポインタ 1: `B8 9F 72 00` |
| 28 | 8092 | 0 |
| 8120 | 8 | `t_xmin`: `02 00 00 00 00 00 00 00` |
| 8128 | 8 | `t_xmax`: 0 |
| 8136 | 8 | `t_cmin`、`t_cmax`: 0 |
| 8144 | 6 | `t_ctid`: `00 00 00 00 01 00` |
| 8150 | 2 | `t_infomask2`: `03 00` |
| 8152 | 2 | `t_infomask`: `00 08` |
| 8154 | 1 | `t_hoff`: `28` |
| 8155 | 5 | 0（パディング） |
| 8160 | 8 | `last_value = 1`: `01 00 00 00 00 00 00 00` |
| 8168 | 8 | `log_cnt = 0` |
| 8176 | 1 | `is_called = 0` |
| 8177 | 7 | 0（`MAXALIGN` のパディング） |
| 8184 | 4 | `magic`: `17 17 00 00` |
| 8188 | 4 | 0 |

この表（`pd_lsn` と `pd_checksum` を除く）を `storage/sequence.rs` の単体テストの固定値にする。1 回目の `nextval` の後（`CACHE 1`）は `(1, 32, true)`、`last_value` のバイトは `01 00 ...`、`log_cnt` のバイトは `20 00 ...`、`is_called` は `01`。

### 3.5 WAL: `SEQ` rmgr の `SEQ_LOG`

`00 §13.4` の `SEQ_LOG = 0x00`、`RmgrId::Seq = 5`。

| 項目 | 値 |
|---|---|
| `rmgr` / `info` | `5` / `0x00`（`INIT_PAGE` のようなフラグは使わない。`WILL_INIT` はブロック参照のフラグ） |
| `xid` | `fetch`・`setval` は **0**（`Xid::INVALID`）。`init`・`reset` は呼び出し側の `WriteCtx.xid` |
| ブロック参照 | 1 個。`block_id = 0`、`fork = Main`、`block = 0`、フラグ `WILL_INIT \| HAS_DATA`（= `0x06`）、画像なし、`data_len = 57` |
| 差分データ | **ログに書く状態**のタプル全体（§3.3 の 57 バイト。`t_ctid`・`t_infomask` は最終値） |
| メインデータ | **空**（`main_len = 0`）。PostgreSQL は `xl_seq_rec` に relfilenode を入れるが、yuzhu はブロック参照が持つ |
| 書く箇所 | `SequenceStore::{init, fetch, setval, reset}`。排他ラッチの下、`CriticalSection` の中（M3 規約 1） |

**ログに書く状態とページの最終状態は別**。`fetch` が `SEQ_LOG` を書くときは、WAL には「あと 32 個進めた後の状態」`(last_value = next, log_cnt = 0, is_called = true)`、ページには「実際に払い出した最後の値と残りの先取り分」`(last, log, true)` を書く（§4.3）。`init`・`setval`・`reset` は両者が同じ（`log_cnt = 0`）。

### 3.6 例: 作ったばかりの `s`（`CACHE 1`）への最初の `nextval`

`rel = (1663, 5, 16400)`、WAL に書く状態 `(33, 0, true)`（`1 + 32`）。`xid = 0`、`start` は直前のレコードの `end`、`prev` はその直前の開始位置。

| オフセット | 長さ | 内容 |
|---|---|---|
| 0 | 4 | `tot_len = 113`（`32 + 24 + 57`）: `71 00 00 00` |
| 4 | 4 | CRC |
| 8 | 8 | `prev` |
| 16 | 8 | `xid = 0` |
| 24 | 4 | `rmgr = 5`、`info = 0x00`、`nblocks = 1`、`reserved = 0`: `05 00 01 00` |
| 28 | 4 | `main_len = 0` |
| 32 | 24 | `block_id = 0`、`flags = 0x06`、`fork = 0`、0、`spc = 1663`（`7F 06 00 00`）、`db = 5`、`rel = 16400`（`10 40 00 00`）、`block = 0`、`data_len = 57`（`39 00 00 00`） |
| 56 | 57 | タプル（`t_xmin = 2` ... `last_value = 33`: `21 00 00 00 00 00 00 00`、`log_cnt = 0`、`is_called = 1`） |
| 113 | 7 | パディング。次のレコードは `+120` から（`end = start + 120`） |

この例（CRC を除く）を `storage/sequence.rs` の単体テストの固定値にする。`yuzhu-waldump` の表示（`describe`）は `rmgr: Seq len: 113 xid: 0 lsn: ... desc: SEQ_LOG rel 1663/5/16400 blk 0: last_value 33 log_cnt 0 is_called t`。

### 3.7 REDO（`storage::sequence::redo`）

```text
redo(ctx, rec):
  rec.rmgr == Seq かつ (rec.info & 0xF0) == SEQ_LOG でなければ Panic XX001 "seq_redo: unknown op code {info}"
  rec.blocks.len() == 1、blocks[0] が will_init、画像なし、data.len() == SEQ_TUPLE_LEN、
  タプルのヘッダ（t_hoff == 40、natts == 3）が正しいことを検査する。違えば Panic
  match read_buffer_for_redo(ctx, rec, 0)? {
     NeedsRedo(buf) => {                                   // WILL_INIT は常にここへ来る（page_lsn の判定をしない。M3 §6.4.3）
        let mut local = Page::zeroed();
        local.init_special(SEQ_SPECIAL_SIZE); local の special に magic を書く
        local.add_item(&blocks[0].data) が Some(1) でなければ Panic（"seq_redo: failed to add item to page"）
        let mut g = buf.write()?;
        *g.page_mut() = local;                             // 8192 バイトを一度に置き換える
        g.set_lsn(rec.end.0);
     }
     Restored | Done | NotFound => {}                      // WILL_INIT では起きない（起きても何もしない）
  }
```

- **常に無条件に上書きする**ことが正しさの要点（§5.10）。ページに書かれている LSN がレコードより新しくても、REDO は最後の `SEQ_LOG` の状態（払い出した値より先）に置き換える。これで、ログなしで進んだ（ディスクに書かれたかもしれない）ページの状態は、必ず「最後の `SEQ_LOG` の状態以上」に戻る。
- ファイルがなければ `read_buffer_zeroed` が作る（M3 §6.4.3）。`invalid pages` には記録されない。後から来る `XACT_COMMIT` / `XACT_ABORT` の `rels` が消す。
- ページの形はテストで `init` の結果と一致させる。`describe(rec)` は `wal/dump.rs` が呼ぶ。

### 3.8 カタログの行

`pg_sequence`（OID 2224、DB ごと。`07-catalog-ddl.md` が `schema.rs` に定義する）の列は PostgreSQL 17 と同じ（実機で確認）:

| 列 | 型 | 内容 |
|---|---|---|
| `seqrelid` | oid | シーケンスの OID |
| `seqtypid` | oid | 21 / 23 / 20 |
| `seqstart`、`seqincrement`、`seqmax`、`seqmin`、`seqcache` | int8 | `SequenceParams` の `start`、`increment`、`max`、`min`、`cache` |
| `seqcycle` | bool | `cycle` |

すべて NOT NULL。`pg_sequence` は `SequenceParams.owned_by` を持たない（所有は `pg_depend`）。

`CREATE SEQUENCE` が書く `pg_class` の行（PostgreSQL 17 の実機の値）:

| 列 | 値 |
|---|---|
| `oid` = `relfilenode` | 新しい OID（マップされない） |
| `relname`、`relnamespace`、`relowner` | 名前、名前空間、`DdlCtx.role_oid` |
| `reltype`、`reloftype`、`relam`、`reltablespace`、`reltoastrelid`、`relrewrite` | **0**（`relam = 0`。表は 2） |
| `relpages` / `reltuples` / `relallvisible` | `1` / `1` / `0`（作った直後から） |
| `relhasindex`、`relisshared`、`relhasrules`、`relhastriggers`、`relhassubclass`、`relrowsecurity`、`relforcerowsecurity`、`relispartition` | false |
| `relpersistence` / `relkind` | `p` / **`S`** |
| `relnatts` / `relchecks` | `3` / `0` |
| `relispopulated` | true |
| `relreplident` | **`n`**（表は `d`） |
| `relfrozenxid` / `relminmxid` | **0** / 0（表は 3 / 1） |
| `relacl`、`reloptions`、`relpartbound` | NULL |

`pg_attribute` の行: ユーザー列 3 つとシステム列 6 つ（`m2.md` §6.8.3 の `SYSTEM_COLUMNS`）。ユーザー列は `last_value`（`atttypid = 20`、`attlen = 8`、`attbyval = true`、`attalign = 'd'`）、`log_cnt`（同じ）、`is_called`（`16`、`1`、true、`'c'`）。3 列とも `attnum` は 1〜3、`attnotnull = true`、`atthasdef = false`、`attstorage = 'p'`、`attcollation = 0`、`atttypmod = -1`、`attidentity` / `attgenerated` / `attcompression` は `'\0'`。

`pg_depend`（`00 §11.6`）:

| 依存元 | 依存先 | deptype | いつ |
|---|---|---|---|
| シーケンス | 表の列（`refobjsubid` = attnum） | `a` | SERIAL、`OWNED BY t.c` |
| シーケンス | 表の列 | `i` | IDENTITY |
| `pg_attrdef` の行 | シーケンス | `n` | 列の DEFAULT に `nextval('...'::regclass)` があるとき（`07-catalog-ddl.md` の一般機構。§5.4） |
| `pg_attrdef` の行 | 表の列 | `a` | `07-catalog-ddl.md`（M2 から変わらない考え方） |

PostgreSQL が全オブジェクトに付ける「名前空間への `n` の依存」（実機で見えるが）は M4 では**書かない**。

### 3.9 SERIAL の DEFAULT の保存形式

`pg_attrdef.adbin`（`pg_node_tree`。中身は SQL テキスト。`m2.md` §6.8.4）は `nextval('16421'::regclass)`（16421 はシーケンスの OID の 10 進数）。`regclass` の入力関数は**数字だけの文字列を OID としてそのまま受け取る**（存在を確かめない。PostgreSQL の `regclassin` と同じ。`09-types-functions.md` の要件）。型の変換（`bigint` → 列の型）は DEFAULT の解析（代入キャスト）が行う。

---

## 4. 型とトレイト（契約の具体化）

### 4.1 定数と値の型

```rust
// storage/sequence.rs（00 §15.1 の SEQ_LOG_VALS・SEQ_MAGIC を使う）
pub const SEQ_LOG: u8 = 0x00;                    // 00 §13.4
pub const SEQ_SPECIAL_SIZE: usize = 8;
pub const SEQ_TUPLE_LEN: usize = 57;             // lp_len
pub const SEQ_TUPLE_HOFF: usize = 40;
pub const SEQ_ITEM_OFFSET: u16 = 1;              // 行ポインタ番号（常に 1）

// storage/mod.rs（00 §13.3。★が追加）
#[derive(Clone, Debug)]
pub struct SequenceHandle {
    pub oid: Oid,
    /// エラーメッセージ（"nextval: reached maximum value of sequence \"%s\" (%d)"）用  ★追加
    pub name: String,
    pub locator: RelFileLocator,
    pub params: SequenceParams,
}
#[derive(Clone, Copy, Debug)]
pub struct SeqRun {
    /// 払い出した最初の値
    pub first: i64,
    /// 払い出した個数（1 以上、要求した count 以下。上限に当たると要求より少ない）。first + (count - 1) * increment が最後の値
    pub count: u32,
    pub increment: i64,
    /// この呼び出しの後のページの LSN（= 払い出した値を覆う SEQ_LOG の終端 LSN）。
    /// この呼び出しが WAL を書かなかった場合も、直近の SEQ_LOG（他のセッションが書いたものを含む）の終端
    pub wal_lsn: Lsn,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]      // ★PartialEq, Eq を追加
pub struct SeqState { pub last_value: i64, pub log_cnt: i64, pub is_called: bool }
```

`SequenceHandle::from_def` に当たるものは `executor/seq.rs` の関数 `handle_from_def(def: &TableDef) -> Result<SequenceHandle>`（`def.kind == Sequence` かつ `def.sequence` が `Some`、でなければ `Error::internal`）。`storage/mod.rs`（A の持ち主）は触らない。

### 4.2 `SequenceStore`（00 §13.3 の意味づけと追加）

```rust
pub trait SequenceStore: Send + Sync + std::fmt::Debug {
    /// 新しいファイル（SMGR_CREATE 済み）の 1 ブロック目を伸ばして初期化し、状態 (params.start, 0, false) で SEQ_LOG を書く。
    /// 返す Lsn は SEQ_LOG の終端。CREATE SEQUENCE の RESTART n は init の後に reset で行う
    fn init(&self, w: &WriteCtx, seq: &SequenceHandle) -> Result<Lsn>;
    /// 最大 count 個の値を払い出す（§4.3、§5.1）。ページの排他ラッチの下でその場上書き。ライターロックも XID も使わない
    fn fetch(&self, seq: &SequenceHandle, count: u32) -> Result<SeqRun>;
    /// 状態を (value, 0, is_called) にして SEQ_LOG を書く。範囲外は 22003。返す Lsn は SEQ_LOG の終端
    fn setval(&self, seq: &SequenceHandle, value: i64, is_called: bool) -> Result<Lsn>;
    /// 共有ラッチの下で状態を読む（ALTER SEQUENCE と試験用）
    fn read(&self, seq: &SequenceHandle) -> Result<SeqState>;
    /// ALTER SEQUENCE / TRUNCATE ... RESTART IDENTITY 用。新しい状態は
    ///   restart_with = Some(v) → (v, 0, false)、None → (現在の last_value, 0, 現在の is_called)
    /// 現在の状態を排他ラッチの下で読み直し、new_params の min / max の範囲にあることを確かめる（22023）。
    /// SEQ_LOG を書き、reset_generation を +1 する
    fn reset(&self, seq: &SequenceHandle, new_params: &SequenceParams, restart_with: Option<i64>) -> Result<Lsn>;
    /// 全セッションの先取り分を捨てさせる世代（reset が +1）。クラスタごとに 1 つ（再起動で 0 に戻ってよい）  ★追加
    fn reset_generation(&self) -> u64;
    /// バッファを捨ててファイルを消す。実際の削除は session の commit / abort が TableStore::unlink_storage で行うので、
    /// 通常は呼ばれない（契約の対称性のために実装する）
    fn unlink_storage(&self, seq: &SequenceHandle) -> Result<()>;
}

/// 実体。StorageStack::new が作って StorageStack.seq に入れる
#[derive(Debug)]
pub struct SeqStore { pool: Arc<BufferPool>, smgr: Arc<StorageManager>, wal: Arc<Wal>, reset_gen: AtomicU64 }
impl SeqStore { pub fn new(pool: Arc<BufferPool>, smgr: Arc<StorageManager>, wal: Arc<Wal>) -> SeqStore; }

/// SEQ rmgr の REDO と表示（recovery::dispatch と wal::dump が呼ぶ）
pub fn redo(ctx: &RedoCtx, rec: &DecodedRecord) -> Result<()>;
pub fn describe(rec: &DecodedRecord) -> String;
```

- `fetch` の `count` は `min(params.cache, u32::MAX)`。`CACHE` が 2^32 を超えても、払い出される値の列は同じで、セッションが一度に先取りする個数が 2^32 - 1 になるだけ。
- `SeqStore` は `Arc<Wal>` を持つ。**リカバリモードの `Wal` では `fetch` などは呼ばれない**（REDO は `redo` だけが書く）。

### 4.3 `plan_fetch`（`nextval` の計算。副作用なし）

PostgreSQL の `nextval_internal` の「`fetch` / `log` / `rescnt` の計算」と同値の純粋関数。`SeqStore::fetch` と単体テストが使う。

```rust
// storage/sequence.rs
pub(crate) struct FetchPlan {
    pub first: i64,
    pub count: u32,
    /// ページに書く最終状態。last_value = 払い出した最後の値、log_cnt = WAL が先に覆っている残りの個数、is_called = true
    pub page: SeqState,
    /// WAL に書く状態（あと SEQ_LOG_VALS 個進めた後）。None ならこの呼び出しでは SEQ_LOG を書かない
    pub log: Option<SeqState>,
}

/// st: 現在の状態、cache: 払い出す個数（>= 1）、force_log: ページの LSN <= REDO 点（チェックポイント後の最初の呼び出し）
pub(crate) fn plan_fetch(name: &str, p: &SequenceParams, st: SeqState, cache: i64, force_log: bool) -> Result<FetchPlan> {
    debug_assert!(cache >= 1 && p.increment != 0);
    let (incr, min, max, cache) = (i128::from(p.increment), i128::from(p.min), i128::from(p.max), i128::from(cache));
    let mut next = i128::from(st.last_value);
    // is_called = false なら last_value 自身が最初の値（先に 1 個と数える）
    let mut rescnt: i128 = if st.is_called { 0 } else { 1 };
    let mut fetch = cache - rescnt;
    // 先取りの要否: 残りの先取り分が足りない、初回、またはチェックポイント後の最初
    let logit = i128::from(st.log_cnt) < fetch || !st.is_called || force_log;
    let mut log = if logit { fetch += i128::from(SEQ_LOG_VALS); fetch } else { i128::from(st.log_cnt) };
    let (mut first, mut last) = (next, next);
    // 上限（降順なら下限）に当たるまでに incr を足せる回数
    let room = |n: i128| -> i128 {
        if incr > 0 { if n <= max { (max - n) / incr } else { 0 } }
        else if n >= min { (n - min) / -incr } else { 0 }
    };
    // 1 つも払い出せない（rescnt == 0）のに足せない: CYCLE なら折り返し先が最初の値、そうでなければ 2200H
    if rescnt == 0 && fetch > 0 && room(next) == 0 {
        if !p.cycle { return Err(limit_error(name, p)); }
        next = if incr > 0 { min } else { max };
        fetch -= 1; log -= 1; rescnt = 1; first = next; last = next;
    }
    let steps = fetch.min(room(next));            // 実際に進める回数。払い出しの途中で折り返さない（1 回の fetch は等差数列）
    let counted = steps.min(cache - rescnt);      // うち、払い出しに数える回数
    if counted > 0 && rescnt == 0 { first = next + incr; }
    if counted > 0 { last = next + counted * incr; }
    next += steps * incr;
    fetch -= steps; rescnt += counted; log -= counted;
    log -= fetch;                                 // 進めなかった分（上限に当たった）
    // 以降 i128 → i64 / u32 の変換は try_from で、失敗は Error::internal（到達しない）
    Ok(FetchPlan { first: .., count: .., page: SeqState { last_value: last, log_cnt: log, is_called: true },
                   log: logit.then_some(SeqState { last_value: next, log_cnt: 0, is_called: true }) })
}

fn limit_error(name: &str, p: &SequenceParams) -> Error {
    // 昇順: "nextval: reached maximum value of sequence \"{name}\" ({max})"、降順: "... minimum value ... ({min})"
    Error::new(sqlstate::SEQUENCE_GENERATOR_LIMIT_EXCEEDED, ..)
}
```

- 「`room` が 0」の判定は PostgreSQL の `(maxv >= 0 && next > maxv - incby) || (maxv < 0 && next + incby > maxv)` と同値（`next + incby > maxv`）。PostgreSQL の式は `i64` でオーバーフローしないように 2 通りに書いたもの。yuzhu は `i128` で計算するので 1 通りでよい。
- **検証済みの値**（PostgreSQL 17.11 の実機と一致。単体テストの固定値）:

| 状態 `(last_value, log_cnt, is_called)` と設定 | 結果の `first`・`count` | ページの新しい状態 | WAL の状態 |
|---|---|---|---|
| `(1, 0, f)`、`CACHE 1`、`INCREMENT 1` | 1・1 | `(1, 32, t)` | `(33, 0, t)` |
| `(1, 32, t)`（上の続き）、同じ設定 | 2・1 | `(2, 31, t)` | なし |
| `(1, 0, f)`、`CACHE 5` | 1・5 | `(5, 32, t)` | `(37, 0, t)` |
| `(5, 32, t)`、`CACHE 5` | 6・5 | `(10, 27, t)` | なし |
| `(10, 0, f)`、`CACHE 5`、`INCREMENT 2`、`START 10` | 10・5 | `(18, 32, t)` | `(82, 0, t)`（`10 + 36 * 2`。ページ側は実機と一致。WAL の値は計算） |
| `(3, 4, t)`、`CACHE 3`、`MAXVALUE 7` | 4・3 | `(6, 1, t)` | なし |
| `(6, 1, t)`、同上 | 7・1（上限で打ち切り） | `(7, 0, t)` | `(7, 0, t)` |
| `(7, 0, t)`、同上 | 2200H `nextval: reached maximum value of sequence "c3" (7)` | — | — |
| `(3, 30, t)`、`CACHE 1`、`force_log = true`（CHECKPOINT の後） | 4・1 | `(4, 32, t)` | `(36, 0, t)` |
| `(1, 0, f)`、`MAXVALUE 3` | 1・1 | `(1, 2, t)` | `(3, 0, t)` |
| `MINVALUE 1 MAXVALUE 2 CYCLE CACHE 5`、`(1, 0, f)` | 1・2（2 で打ち切り） | `(2, 0, t)` | `(2, 0, t)` |

### 4.4 セッション側の状態（`executor/seq.rs`）

PostgreSQL の `SeqTableData`（シーケンスごと）と `last_used_seq`（最後に `nextval` したシーケンス）に当たる。

```rust
// executor/seq.rs
#[derive(Clone, Copy, Debug, Default)]
struct SeqEntry {
    /// currval が定義済みか（nextval か setval(.., true) をこのセッションでした）
    last_valid: bool,
    /// 最後に返した値（currval。lastval の元）
    last: i64,
    /// 先取りした最後の値。last != cached なら先取りの残りがある
    cached: i64,
    /// 先取りしたときの increment
    increment: i64,
}

#[derive(Debug, Default)]
pub struct SeqSession {
    entries: HashMap<Oid, SeqEntry>,
    last_used: Option<Oid>,                 // lastval
    seen_reset_gen: u64,
    /// この文で払い出した値を覆う WAL の最大の LSN。文の終わりに session が Transaction.wal_flush_upto へ反映する
    pending_flush: Lsn,
    /// 文の中で引いた SequenceHandle（同じ文のカタログのスナップショットは変わらないので使い回す）
    handles: HashMap<Oid, SequenceHandle>,
}
impl SeqSession {
    pub fn new() -> SeqSession;
    /// 文の終わり（成功も失敗も）に session が呼ぶ。pending_flush を返して 0 に戻し、handles を捨てる
    pub fn end_statement(&mut self) -> Lsn;
    /// DISCARD SEQUENCES 相当（M4 では呼ぶ文がない）
    pub fn discard(&mut self);
}

/// RuntimeInfo の nextval / currval / lastval / setval の実装本体。文ごとに session が作る
pub struct SeqRuntime<'a> {
    pub state: &'a RefCell<SeqSession>,
    pub store: &'a dyn SequenceStore,
    pub catalog: &'a dyn CatalogReader,
    /// 現在のトランザクションが READ ONLY（transaction_read_only。M3 §6.11.1）
    pub read_only: bool,
}
impl SeqRuntime<'_> {
    pub fn nextval(&self, seq: Oid) -> Result<i64>;
    pub fn currval(&self, seq: Oid) -> Result<i64>;
    pub fn lastval(&self) -> Result<i64>;
    pub fn setval(&self, seq: Oid, value: i64, is_called: bool) -> Result<i64>;
}
```

手順は §5.1・§5.2。`RuntimeInfo`（`m3.md` §4.9、`00 §14.3`）を実装する session の構造体（M3 §6.11.6 の `SessionRuntime`）は、4 つのメソッドを `SeqRuntime` に委ねる（`#[derive(Debug)]` は手書き）。`Session` は `seq_state: RefCell<SeqSession>` を持ち、`ExecCtx` が `&mut self.txn` を借りている間も `&self.seq_state` を渡せるように、フィールドを分けて借りる。

### 4.5 関数の行と実装

`catalog/builtin.rs`（`09-types-functions.md` の T3 が行を足す）に次の行を入れる。OID と属性は PostgreSQL 17 の `pg_proc`（実機で確認）。実装関数は `types/ops.rs`（Q1）に置く。

| OID | 名前 | 引数 | 戻り値 | strict | `provolatile` | `proparallel` | prosrc | `FnKind` |
|---|---|---|---|---|---|---|---|---|
| 1574 | `nextval` | `regclass` | `int8` | t | **v** | u | `nextval_oid` | `Runtime(seq_nextval)` |
| 1575 | `currval` | `regclass` | `int8` | t | **v** | u | `currval_oid` | `Runtime(seq_currval)` |
| 1576 | `setval` | `regclass`, `int8` | `int8` | t | **v** | u | `setval_oid` | `Runtime(seq_setval2)` |
| 1765 | `setval` | `regclass`, `int8`, `bool` | `int8` | t | **v** | u | `setval3_oid` | `Runtime(seq_setval3)` |
| 2559 | `lastval` | なし | `int8` | t | **v** | u | `lastval` | `Runtime(seq_lastval)` |
| 1665 | `pg_get_serial_sequence`（任意） | `text`, `text` | `text` | t | s | s | `pg_get_serial_sequence` | `Context(..)` |

```rust
// types/ops.rs
pub fn seq_nextval(args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum>;        // Datum::Int8(rt.nextval(oid)?)
pub fn seq_currval(args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum>;
pub fn seq_setval2(args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum>;        // is_called = true
pub fn seq_setval3(args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum>;
pub fn seq_lastval(args: &[Datum], rt: &dyn RuntimeInfo) -> Result<Datum>;
```

- 引数の `regclass` は `Datum::Oid(u32)`。`strict` の関数の NULL 引数は評価器が呼ばずに NULL を返す（`nextval(NULL)`・`setval('s', NULL)` は NULL。実機で確認）。
- **volatile**: 計画段階の定数畳み込み（`04-planner-optimizer.md` の `const_fold`）は `FnKind::Runtime` と `volatile` を畳み込まない。行ごとに 1 回評価する（`SELECT nextval('s'), nextval('s')` は左から `1, 2`）。
- **`pg_get_serial_sequence(table, column)`（任意）**: 第 1 引数を `regclass` と同じ規則（引用符・小文字化・スキーマ修飾）で表に解決する（なければ `42P01 relation "x" does not exist`）。第 2 引数は列名（そのまま。なければ `42703 column "x" of relation "t" does not exist`）。その列を所有する（`pg_depend` の `a` / `i`）シーケンスがあれば、`quote_ident(schema) || '.' || quote_ident(name)`（例 `public.t_id_seq`）、なければ NULL。`CatalogReader::sequence_owned_by_column(&self, table: Oid, attnum: i16) -> Result<Option<Oid>>`（既定は `Ok(None)`）を足して引く（§11）。
- `text → regclass`（`pg_cast` 10109、関数 1079 `regclass(text)`、暗黙）と `varchar → regclass`（10110）、`oid → regclass`（10074、暗黙・バイナリ）、`int4 → regclass`（10078、暗黙・バイナリ）が必要（実機で確認。`nextval('s'::text)`・`nextval(17338)` が動く）。`09-types-functions.md` が持つ。
- `regclass` の**文字列リテラルからの変換は解析時に行う**（`nextval('nope')` は文の解析で `42P01 relation "nope" does not exist`、位置つき）。結果は `Literal(Datum::Oid)`（型 `regclass`）になる。IDENTITY の暗黙の DEFAULT と、DEFAULT 式の依存の記録（§5.4）がこの形を使う。

### 4.6 `Transaction` と `TxnManager`

```rust
// txn/manager.rs（00 §14.1。この章が足す）
pub struct Transaction {            // M2/M3 のフィールドに追加
    /// 非トランザクション的に書いた WAL（SEQ_LOG）のうち、このトランザクションが払い出した値を覆うものの終端 LSN の最大値。
    /// コミット時にここまで flush する。Lsn(0) = なし
    pub wal_flush_upto: Lsn,
    /// トランザクションの開始時刻（2000-01-01 からのマイクロ秒。PostgreSQL のエポック）。now() / transaction_timestamp() が返す。
    /// Transaction::new() は 0。session が BEGIN、または暗黙のトランザクションの最初の文の開始時に設定する（statement_timestamp と同じ時計）
    pub started_at: i64,
}
impl Transaction {
    /// 大きい方を残す
    pub fn note_wal(&mut self, lsn: Lsn) { if lsn > self.wal_flush_upto { self.wal_flush_upto = lsn; } }
}

impl TxnManager {
    /// XID を持たないトランザクションの COMMIT。flush_upto が Lsn(0) でなければ wal.flush(flush_upto)。
    /// コミットゲートは取らない（clog を触らない）。失敗は Severity::Panic（Wal が poison される）。
    /// DebugKnobs::skip_commit_flush のときは何もしない（変異試験）
    pub fn finish_without_xid(&self, flush_upto: Lsn) -> Result<()>;
}
```

- **XID を持つトランザクションのコミット**（M3 §5.3 の手順 1）は変えない。コミットレコードの終端 LSN は、このトランザクションが読んだページの LSN（= それより前に挿入されたレコードの終端）以上なので、コミットの flush が `wal_flush_upto` を覆う（`debug_assert!(commit_end >= txn.wal_flush_upto)`）。
- **XID を持たないトランザクションのコミット**: `session.rs` の `commit_transaction` の「`txn.xid` が `None`」の経路（現在は何もせず戻る）で、`cluster.txn_manager().finish_without_xid(txn.wal_flush_upto)?` を呼んでから `txn` を捨てる。読み取りだけのトランザクションで `wal_flush_upto == 0` なら I/O は起きない（M3 §5.3 の「XID なしは WAL を書かず fsync もしない」のまま）。
- **中断**（XID の有無によらず）は `wal_flush_upto` を捨てるだけ。flush しない（D8-6）。
- `COMMIT AND CHAIN` は新しい `Transaction` に引き継がない（値は 0 に戻る）。

### 4.7 パラメータの検証（`catalog/seq_params.rs`）

```rust
// catalog/seq_params.rs（Q1。パーサ・アナライザ・ddl が共有する純粋関数。依存は types と error だけ）
/// CREATE / ALTER SEQUENCE のオプションを文面どおりに保持したもの（検査前）。None = 指定なし
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeqOptions {
    pub as_type: Option<Oid>,                  // int2 / int4 / int8 の OID（アナライザが型名を解決した結果。違う型は init_params が 22023）
    pub increment: Option<i64>,
    pub min: Option<Option<i64>>,              // Some(None) = NO MINVALUE
    pub max: Option<Option<i64>>,              // Some(None) = NO MAXVALUE
    pub start: Option<i64>,
    pub restart: Option<Option<i64>>,          // Some(None) = RESTART（START の値に戻す）
    pub cache: Option<i64>,
    pub cycle: Option<bool>,
}
impl SeqOptions { pub fn is_empty(&self) -> bool; }

pub enum InitMode<'a> {
    Create,
    /// current = 変更前の pg_sequence の値、state = 変更前のシーケンスの状態
    Alter { current: &'a SequenceParams, state: SeqState },
}
pub struct InitOutcome {
    pub params: SequenceParams,        // 新しい値（owned_by は None で返す。呼び出し側が現在の値を引き継ぐ）
    pub state: SeqState,               // 新しい状態: CREATE は (start または RESTART の値, 0, false)。ALTER は RESTART なら (値, 0, false)、そうでなければ (現在の last_value, 0, 現在の is_called)
    pub restarted: bool,               // RESTART が指定された
}
/// PostgreSQL の init_params。検査の順序と文言は §6.2
pub fn init_params(opts: &SeqOptions, for_identity: bool, mode: InitMode<'_>) -> Result<InitOutcome>;
/// オプションの数値のテキスト（符号つき。"+5"、"-1"、"1.5"、"9223372036854775808"）を i64 に。
/// int8 の入力関数と同じ: 範囲外は 22003 `value "%s" is out of range for type bigint`、数字でなければ 22P02 `invalid input syntax for type bigint: "%s"`
pub fn parse_seq_int(text: &str) -> Result<i64>;
pub fn seq_type_name(type_oid: Oid) -> &'static str;      // "smallint" / "integer" / "bigint"
```

### 4.8 構文・Bound・カタログのストア

**AST**（`sql/ast.rs`。S1 が実装。`00 §7.1` の名前に、この章がフィールドを決める）:

```rust
pub struct CreateSequence { pub name: ObjectName, pub if_not_exists: bool, pub persistence: SeqPersistence, pub options: Vec<SeqOption>, pub span: Span }
pub enum SeqPersistence { Permanent, Temporary, Unlogged }                // Temporary / Unlogged は analyzer が 0A000
pub struct AlterSequence { pub name: ObjectName, pub if_exists: bool, pub action: AlterSequenceAction, pub span: Span }
pub enum AlterSequenceAction { Options(Vec<SeqOption>), OwnerTo(Ident), RenameTo(Ident), SetSchema(Ident) /* 後ろの 2 つは analyzer が 0A000 */ }
pub struct DropSequence { pub names: Vec<ObjectName>, pub if_exists: bool, pub cascade: bool, pub span: Span }
pub struct SeqOption { pub kind: SeqOptionKind, pub span: Span }
pub enum SeqOptionKind {
    As(TypeName),
    Increment(SeqNumber),
    MinValue(Option<SeqNumber>),        // None = NO MINVALUE
    MaxValue(Option<SeqNumber>),
    Start(SeqNumber),
    Restart(Option<SeqNumber>),         // RESTART [[WITH] n]
    Cache(SeqNumber),
    Cycle(bool),                        // CYCLE = true、NO CYCLE = false
    OwnedBy(ObjectName),                // OWNED BY NONE は 1 要素の名前 "none"（PostgreSQL と同じ）
    SequenceName(ObjectName),           // CREATE SEQUENCE では 42601。IDENTITY のオプションでは採用する
}
/// 数値は「符号 + 数字（小数点を含みうる）」のテキストのまま持つ（PostgreSQL の NumericOnly と同じ。式は書けない）。
/// 変換は parse_seq_int（大きすぎる整数や小数のエラーを PostgreSQL と同じ文言にするため）
pub struct SeqNumber { pub text: String, pub span: Span }

// 列の制約（ColumnConstraintKind に追加）。GENERATED ... AS (expr) STORED は M1 のまま 0A000
ColumnConstraintKind::Identity { when: GeneratedWhen, options: Vec<SeqOption> }
pub enum GeneratedWhen { Always, ByDefault }
// serial 系の型名は TypeName のまま（names = ["serial"] など）。アナライザが解釈する
// INSERT
Insert.overriding: Option<OverridingKind>        // OverridingKind::{System, User}。DEFAULT VALUES とは併用できない（構文エラー）
```

**Bound**（`analyzer/bound.rs`。`00 §7` の名前）:

```rust
pub struct BoundCreateSequence {
    pub schema: String,
    pub namespace: Oid,
    pub name: String,
    pub if_not_exists: bool,
    /// 既定値を埋めた最終値（init_params の結果。owned_by は None。所有は owner で表す）
    pub params: SequenceParams,
    /// CREATE 時の初期状態（RESTART n があれば last_value = n）。通常は (start, 0, false)
    pub initial: SeqState,
    pub owner: SeqOwner,
    /// IDENTITY の暗黙のシーケンス（pg_depend の種類が i、init_params のメッセージが identity 用）
    pub for_identity: bool,
}
pub enum SeqOwner {
    None,
    /// 既存の表の列（CREATE SEQUENCE ... OWNED BY t.c。種類は a）
    Column { table: Oid, attnum: i16 },
    /// 同じ CREATE TABLE が作る表の列（表の OID は実行時に決まる）。serial_default = true は SERIAL（種類 a、DEFAULT を付ける）、false は IDENTITY（種類 i）
    NewTableColumn { attnum: i16, serial_default: bool },
}
pub struct BoundAlterSequence {
    /// None = IF EXISTS で存在しなかった（NOTICE を出して終わる）
    pub target: Option<Arc<TableDef>>,
    pub missing_name: String,
    pub action: BoundAlterAction,
}
pub enum BoundAlterAction {
    Options { options: SeqOptions, owned_by: Option<OwnedByTarget> },
    /// OWNER TO（ロールの存在を確かめて何もしない）
    OwnerNoop,
}
pub enum OwnedByTarget { None, Column { table: Oid, attnum: i16 } }
pub struct BoundDropSequence {
    pub targets: Vec<Arc<TableDef>>,
    /// IF EXISTS で存在しなかった名前（NOTICE 用）
    pub missing: Vec<String>,
    pub cascade: bool,
}
```

`BoundCreateTable.sequences`（`00 §7`）は `Vec<BoundCreateSequence>`、各要素の `owner` は `NewTableColumn`。`ColumnDef`（`00 §11.1`）の `identity` を IDENTITY の列で設定する。SERIAL の列の `ColumnDef.default` は**アナライザでは `None`**（`ddl/table.rs` がシーケンスの OID を決めた後に §3.9 のテキストを入れる）。

**カタログのストア**（`07-catalog-ddl.md` が `catalog/store.rs` に実装する。この章が必要とする形）:

```rust
pub struct NewSequence { pub oid: Oid, pub namespace: Oid, pub name: String, pub owner: Oid, pub params: SequenceParams }
impl CatalogStore {
    /// pg_class（§3.8）、pg_attribute（3 + 6 行）、pg_sequence に行を入れる。ファイルは作らない（呼び出し側が先に作る）
    pub fn create_sequence(&self, w: &WriteCtx, snap: &Snapshot, s: &NewSequence) -> Result<()>;
    /// pg_sequence の行を更新（seqtypid、seqstart、seqincrement、seqmax、seqmin、seqcache、seqcycle）
    pub fn update_sequence_params(&self, w: &WriteCtx, snap: &Snapshot, oid: Oid, p: &SequenceParams) -> Result<()>;
    /// pg_sequence、pg_attribute、pg_class の行を消す（pg_depend は ddl::depend が消す）
    pub fn drop_sequence(&self, w: &WriteCtx, snap: &Snapshot, oid: Oid) -> Result<()>;
    /// DROP SEQUENCE ... CASCADE が消す DEFAULT: pg_attrdef の行を消し、pg_attribute.atthasdef を false にする
    pub fn drop_default(&self, w: &WriteCtx, snap: &Snapshot, table: Oid, attnum: i16) -> Result<()>;
}
```

`load_table_def` は `relkind = 'S'` の行から `TableDef { kind: Sequence, columns: 3 列, sequence: Some(SequenceParams) }` を作る。`owned_by` は `pg_depend` の `a` / `i`（シーケンスが依存元、`refobjsubid > 0`）から。`TableDef.identity_seqs` は表の `pg_depend`（依存先が表の列、依存元が relkind `S`、deptype `i`）から `(attnum, シーケンスの OID)` を作る。`ColumnDef.identity` は `pg_attribute.attidentity` から。

**`ddl/depend.rs`**（07 が持つ。この章が使う形。名前は 07 と突き合わせる）:

```rust
pub struct ObjAddr { pub classid: Oid, pub objid: Oid, pub objsubid: i32 }
pub enum DepType { Normal /* n */, Auto /* a */, Internal /* i */ }
pub fn record(ctx: &mut DdlCtx<'_>, dependent: ObjAddr, referenced: ObjAddr, ty: DepType) -> Result<()>;
/// referenced に依存する行（pg_depend の refclassid / refobjid が一致。objsubid は問わない）。OID 昇順
pub fn dependents_of(ctx: &DdlCtx<'_>, referenced: ObjAddr) -> Result<Vec<(ObjAddr, DepType)>>;
/// dependent が依存する先
pub fn dependencies_of(ctx: &DdlCtx<'_>, dependent: ObjAddr) -> Result<Vec<(ObjAddr, DepType)>>;
/// dependent の行のうち、依存先のクラスが ref_class で種類が ty のものを消す（OWNED BY の付け替え）
pub fn delete_dependencies(ctx: &mut DdlCtx<'_>, dependent: ObjAddr, ref_class: Oid, ty: DepType) -> Result<()>;
/// 人が読む説明: "default value for column a of table t"、"sequence s"
pub fn describe(ctx: &DdlCtx<'_>, obj: ObjAddr) -> Result<String>;
```

### 4.9 IDENTITY の規則（`analyzer/ddl.rs`。N1 が INSERT / UPDATE の解析から呼ぶ）

```rust
/// INSERT の 1 つの列について、指定の形
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GivenValue { Omitted /* 列リストにない */, DefaultKeyword /* VALUES の DEFAULT */, Value /* それ以外の式・SELECT の出力 */ }
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdentityInsert { /* 与えられた値を使う */ Given, /* nextval を使う */ Generate }

/// col.identity が None の列は呼ばない（普通の DEFAULT の規則）。表は §5.8
pub fn identity_insert_rule(col: &ColumnDef, given: GivenValue, overriding: Option<OverridingKind>) -> Result<IdentityInsert>;
/// UPDATE ... SET col = <式>。col.identity == Some(Always) で is_default_keyword == false なら 428C9
pub fn identity_update_rule(col: &ColumnDef, is_default_keyword: bool) -> Result<()>;
/// IDENTITY の暗黙の DEFAULT: nextval(<seq oid>::regclass) を列型に代入キャストしたもの。identity でない列は None
/// （INSERT の defaults、UPDATE の DEFAULT、COPY の列変換が使う。Var を含まない）
pub fn identity_default_expr(table: &TableDef, col: &ColumnDef, catalog: &dyn CatalogReader) -> Result<Option<BoundExpr>>;
```

---

## 5. 処理の流れ

### 5.1 `nextval`

```text
SeqRuntime::nextval(oid):
  1. h = handle(oid)?                       // §5.2 の handle。文のキャッシュ（SeqSession.handles）→ catalog.table_by_oid
  2. if read_only: 25006 "cannot execute nextval() in a read-only transaction"      （先取りの残りがあっても検査する。PG と同じ）
  3. s = state.borrow_mut()
     s.sync_generation(store.reset_generation())     // 世代が進んでいたら全エントリの cached = last
     e = s.entries.entry(oid).or_default()
  4. if e.last != e.cached:                          // 先取りの残り（CACHE）
        e.last += e.increment; s.last_used = Some(oid); return Ok(e.last)               // WAL・ラッチなし
  5. run = store.fetch(&h, min(h.params.cache, u32::MAX))?          // 失敗（2200H など）ならセッションの状態は変えない
  6. e.increment = run.increment; e.last = run.first; e.cached = run.first + (run.count - 1) * run.increment; e.last_valid = true
     s.last_used = Some(oid); s.pending_flush = max(s.pending_flush, run.wal_lsn)
  7. return Ok(run.first)
```

`SeqStore::fetch(h, count)`（PostgreSQL の `nextval_internal` の写し。M3 規約 1 の手順どおり）:

```text
fetch(h, count):
  1. buf = pool.read_buffer(BufferTag { rel: h.locator, fork: Main, block: 0 })?          // ピン
  2. g = buf.write()?                                                                      // 排他ラッチ（シーケンスごとの直列化はこれだけ）
  3. let _ = g.page_mut_hint();                                                            // 先に dirty にする（D8-7。M3: 呼んだ時点で dirty）
     st = read_state(g.page(), &h.name)?                                                   // magic などの検査（XX001）
     force_log = g.page().lsn() <= wal.redo_lsn().0                                        // チェックポイント後の最初の呼び出し
  4. plan = plan_fetch(&h.name, &h.params, st, i64::from(count), force_log)?               // ここまでで失敗しうる処理は終わり（2200H はここ）
  5. match plan.log {
       Some(logged) => {                                                                   // SEQ_LOG を書く
          cs = CriticalSection::enter(&pool)
          write_state(g.page_mut(), &plan.page)                                            // ページは最終状態
          rec = RecordBuilder::new(RmgrId::Seq, SEQ_LOG, Xid::INVALID)
          b = rec.register_block(tag, g.page(), RegFlags::WILL_INIT)
          rec.block_data(b, &seq_tuple_bytes(&logged))                                     // WAL は「あと 32 個先」の状態
          ins = wal.insert(rec).map_err(|e| cs.escalate(e))?
          g.set_lsn(ins.end.0)
          drop(cs)
       }
       None => write_state(g.page_mut_hint(), &plan.page),                                 // WAL なし。set_lsn もしない（M3 規約 2 の例外は page_mut_hint）
     }
  6. wal_lsn = Lsn(g.page().lsn())                                                          // ログを書かなかった場合も、直近の SEQ_LOG の終端
  7. return SeqRun { first: plan.first, count: plan.count, increment: h.params.increment, wal_lsn }
```

- **ラッチと WAL の順序**: ページの排他ラッチ → `wal.insert`（挿入 Mutex）の順（M3 §5.9 の 4 → 6b）。`wal.flush` はここでは呼ばない（コミットが呼ぶ）。コミットゲートも取らない。
- `plan_fetch` が `Err`（上限）のとき、ページは変えていない（`page_mut_hint()` は dirty を立てただけ。書き出しが 1 回増えるだけで害はない）。
- 並行する `nextval` は同じページの排他ラッチで直列化される。重複しない（ラッチの下で状態を読んで書く）。
- `CACHE n`（`n > 1`）: 1 回の `fetch` で n 個の等差数列を払い出し、セッションが覚える。他のセッションは次の値から（`last_value` は先取りの最後）。セッションが終わると残りは失われる（欠番）。

### 5.2 `currval` / `lastval` / `setval`

```text
handle(oid):                         // nextval / currval / setval / ALTER SEQUENCE が共有
  def = catalog.table_by_oid(oid)?
  None                      → Error::internal("could not open relation with OID {oid}")        // XX000。PG の実機の文言と一致
  Some(def) if def.kind != Sequence → 42809 `cannot open relation "{name}"`、DETAIL `This operation is not supported for {tables|indexes}.`
  Some(def)                 → SeqSession.handles に入れて SequenceHandle を返す

currval(oid):
  h = handle(oid)?
  e = entries.get(oid)
  !e.last_valid → 55000 `currval of sequence "{name}" is not yet defined in this session`
  return e.last

lastval():
  last_used が None → 55000 `lastval is not yet defined in this session`
  catalog.table_by_oid(last_used) が None（その後 DROP された）または Sequence でない → 同じ 55000（PG と同じ文言）
  return entries[last_used].last

setval(oid, value, is_called):
  h = handle(oid)?
  if read_only: 25006 "cannot execute setval() in a read-only transaction"
  lsn = store.setval(&h, value, is_called)?       // 範囲外は 22003 `setval: value {v} is out of bounds for sequence "{name}" ({min}..{max})`
  e = entries.entry(oid)
  if is_called { e.last = value; e.last_valid = true }          // is_called = false なら currval の状態は変えない
  e.cached = e.last                                             // 先取りを捨てる
  pending_flush = max(pending_flush, lsn)
  return value                                                  // lastval は変えない（PostgreSQL は setval では last_used_seq を更新しない）
```

- 実機の確認: `setval('s', 5, false)` の後の `currval('s')` は 55000、`setval('s', 5, true)` の後は 5。`setval` は `lastval()` を定義しない（ただし `lastval()` が指しているシーケンスに `setval` すると、その `last` は新しい値になる）。
- `currval` はロールバックしても戻らない。`SELECT currval(...)` は DROP 済みのシーケンスを名前で引けない（regclass の解決が `42P01`）。
- `SeqStore::setval` の手順: ラッチ → `page_mut_hint()` → `read_state`（magic の検査）→ 範囲検査 → `CriticalSection` → `write_state(g.page_mut(), (value, 0, is_called))` → `SEQ_LOG`（同じ状態）→ `set_lsn`。

### 5.3 コミットとアボート（XID なしの経路）

M3 §5.3 に次を足す。

```text
Session::after_statement（成功・失敗の両方。文ごと）:
  lsn = seq_state.borrow_mut().end_statement()
  txn.note_wal(lsn)

commit(txn):                                         ※ストレージバリアを持たない状態で呼ぶ
  if let Some(xid) = txn.xid: （M3 のとおり。コミットレコードの flush が wal_flush_upto を覆う）
  else:                                              ★ M4 で足す
     txn_mgr.finish_without_xid(txn.wal_flush_upto)?       // 0 なら何もしない。Panic はクラスタを poison
  （catalog_dirty は XID がないときは常に false）

abort(txn): 変更なし（wal_flush_upto は捨てる）
```

- 暗黙のトランザクションの `SELECT nextval('s')` の流れ: 文の実行（`fetch` が `SEQ_LOG` を書くことがある）→ `end_statement` → `note_wal` → 暗黙のコミット（`finish_without_xid` が flush）→ `CommandComplete` / `ReadyForQuery`。クライアントが `ReadyForQuery` を受け取った時点で、その値を覆う WAL は永続化されている。
- 複数文の Query（暗黙のブロック）は最後にまとめてコミットするので、flush は 1 回。
- 文が失敗した場合も `note_wal` する（値は払い出されたまま戻らない。ブロック内で失敗した後の `COMMIT` は `ROLLBACK` になるので flush は起きない）。

### 5.4 `CREATE SEQUENCE`

アナライザ（`analyzer/ddl.rs`。`analyze_create_sequence`）:

```text
  1. persistence が Temporary → 0A000 "temporary sequences are not supported yet"、Unlogged → "unlogged sequences are not supported yet"
  2. 名前: スキーマ修飾なしなら search_path の最初に存在する名前空間（CREATE TABLE と同じ関数）。pg_catalog → 42501 `permission denied to create "pg_catalog.{name}"`、DETAIL `System catalog modifications are currently disallowed.`
  3. 同名のリレーション（relation_kind(schema, name)）があり IF NOT EXISTS なら: NOTICE（SQLSTATE 42P07）`relation "{name}" already exists, skipping` を出して何もしない（タグは CREATE SEQUENCE）。
     オプションの検査より先（PostgreSQL の DefineSequence と同じ順序）。IF NOT EXISTS なしの 42P07 は手順 7
  4. options を SeqOptions にする。同じオプションが 2 回 → 42601 `conflicting or redundant options`（2 回目の span）。
     SequenceName → 42601 `invalid sequence option SEQUENCE NAME`（span つき）。
     As(TypeName) は型を解決する（存在しなければ 42704 `type "x" does not exist`、位置つき）。int2 / int4 / int8 でなければ init_params が 22023
     各 SeqNumber は parse_seq_int（22003 / 22P02。位置なし）
  5. owned_by（OwnedBy）: §5.5 の process_owned_by の検査（CREATE では pg_depend の種類は a）
  6. out = init_params(&opts, false, InitMode::Create)?
  7. 同名のリレーションがあれば 42P07 `relation "{name}" already exists`（PostgreSQL は DefineRelation で出すので、init_params の検査より後）
  8. BoundCreateSequence { params: out.params, initial: out.state, owner, for_identity: false }
```

`ddl/sequence.rs` の `create`（`DdlCtx`、ライターロックとストレージバリアを持った状態）:

```text
create(ctx, c) → "CREATE SEQUENCE":
  oid = ctx.db.catalog.get_new_relation_oid(ctx.cluster.oid_allocator())?
  create_with_oid(ctx, &c, oid, None)?

create_with_oid(ctx, c, oid, table_ref: Option<(Oid /* 表 */, i16)>):       // CREATE TABLE（ddl/table.rs）も、表の OID が決まった後でこれを呼ぶ
  1. locator = RelFileLocator { spc_oid: 1663, db_oid: ctx.db.oid, rel_number: RelFileNumber(oid) }
  2. w = ctx.txn.write_ctx()?
     ctx.cluster.storage().create_storage(&w, locator)?; ctx.txn.pending_creates.push(locator)       // SMGR_CREATE（M3 §5.4）
  3. h = SequenceHandle { oid, name: c.name.clone(), locator, params: c.params }
     lsn = ctx.cluster.seq().init(&w, &h)?                                    // SEQ_LOG（状態 (start, 0, false)）
     if c.initial != SeqState { last_value: c.params.start, log_cnt: 0, is_called: false }:         // CREATE ... RESTART n
        lsn = ctx.cluster.seq().reset(&h, &c.params, Some(c.initial.last_value))?
     ctx.txn.note_wal(lsn)
  4. ctx.db.catalog.create_sequence(&w, ctx.snapshot, &NewSequence { oid, namespace, name, owner: ctx.role_oid, params })?
  5. 所有: c.owner が Column{table, attnum}、または table_ref が Some なら
        depend::record(ctx, ObjAddr{pg_class, oid, 0}, ObjAddr{pg_class, table, attnum}, if c.for_identity { Internal } else { Auto })?
  6. ctx.txn.catalog_dirty = true
```

- **`DEFAULT` 式の依存の記録**（`07-catalog-ddl.md` の一般機構。この章が要求する）: 列の DEFAULT を `pg_attrdef` に書いた後、解析済みの DEFAULT 式を走査して、型が `regclass` の `Literal`（`nextval('x'::regclass)` の引数）ごとに `pg_attrdef` の行 → そのリレーションへ `n` の依存を記録する。SERIAL のテキスト（§3.9）も、利用者が書いた `DEFAULT nextval('s')` も同じ経路（PostgreSQL は DEFAULT 式の中の関係への依存をすべて記録する。実機で `DROP SEQUENCE` が `2BP01` になることを確認）。
- 失敗したら（文が失敗するのでトランザクションは中断される）、`pending_creates` のファイルは M3 §5.3 のとおり中断で消える。SEQ_LOG は残るが孤児のファイルへの記録で害はない（REDO は WILL_INIT でファイルを作り、後続の ABORT の `rels` が消す）。

### 5.5 `ALTER SEQUENCE`

アナライザ（`analyze_alter_sequence`）:

```text
  1. 名前を引く（relation_kind）。なければ: IF EXISTS → NOTICE（SQLSTATE 00000）`relation "{name}" does not exist, skipping`、target = None。なければ 42P01 `relation "{name}" does not exist`
  2. Sequence でなければ 42809 `cannot open relation "{name}"`、DETAIL `This operation is not supported for {tables|indexes}.`
  3. action: Options → §5.4 の 3、4 と同じ（SeqOptions と OwnedBy の解決）。OwnerTo → ロールの存在（42704 `role "x" does not exist`）。RenameTo / SetSchema → 0A000
```

`ddl/sequence.rs` の `alter`:

```text
alter(ctx, a) → "ALTER SEQUENCE":
  target が None なら NOTICE を積んで終わる
  def = a.target; h = handle_from_def(&def)?                        // h.params = 変更前の pg_sequence の値（owned_by を含む）
  Options { options, owned_by }:
    if !options.is_empty():
       st = seq.read(&h)?
       out = init_params(&options, false, InitMode::Alter { current: &h.params, state: st })?     // §6.2
       new = SequenceParams { owned_by: h.params.owned_by, ..out.params }
       lsn = seq.reset(&h, &new, if out.restarted { Some(out.state.last_value) } else { None })?             // 常に log_cnt = 0、reset_generation + 1
       catalog.update_sequence_params(&w, snap, def.oid, &new)?
       txn.note_wal(lsn)
    if let Some(ob) = owned_by: process_owned_by(ctx, &def, ob)?
    txn.catalog_dirty = true
  OwnerNoop: 何もしない
```

`process_owned_by`（PostgreSQL の `process_owned_by`。CREATE と ALTER が共有）:

**どちらの形でも先に**: ユーザーが書いた OWNED BY（`for_identity = false`）で、シーケンスが IDENTITY に所有されている（`dependencies_of` に `i` がある）なら `0A000 cannot change ownership of identity sequence`、DETAIL `Sequence "{seq}" is linked to table "{table}".`（`OWNED BY NONE` も同じ。実機で確認）。

| 入力 | 動作・エラー |
|---|---|
| `OWNED BY NONE` | `delete_dependencies(seq, pg_class, a)`（既存の所有を外す） |
| `OWNED BY [schema.]table.column` | 表を引く（なければ `42P01 relation "x" does not exist`）。表でない（シーケンス・インデックス）→ `42809 sequence cannot be owned by relation "{x}"`、DETAIL `This operation is not supported for {sequences|indexes}.`。シーケンスと表が別の名前空間 → `55000 sequence must be in same schema as table it is linked to`。列がない → `42703 column "{c}" of relation "{t}" does not exist`。OK なら既存の `a` の依存を外して新しい `a` を記録する。所有者の検査（`55000 sequence must have same owner as table it is linked to`）は M4 では 1 ロールなので起きない |

- `ALTER SEQUENCE ... AS type`: `init_params` が `min` / `max` を新しい型に合わせて調整する（§6.2。実機: `int` + `INCREMENT -1` を `AS smallint` にすると `min = -32768`、`max = -1`。最大が元の型の最大のままなら新しい型の最大に）。
- 状態が変わる ALTER（`!options.is_empty()`）は**すべて**`reset` を呼ぶ（PostgreSQL の「`OWNED BY` 以外は書き直す」と同じ範囲）。`RESTART` は `(値, 0, false)`、それ以外は `(現在の last_value, 0, 現在の is_called)`。
- `currval` の状態は変えない（PostgreSQL と同じ）。**他のセッションの先取りは `reset_generation` で捨てられる**（D8-9）。
- 実機で確かめた値: `START 10 INCREMENT 5 MINVALUE 5 MAXVALUE 100 CACHE 3` の `nextval` ×2 の後（`last_value = 20, log_cnt = 16`）に `ALTER SEQUENCE a1 INCREMENT 7` すると状態は `(20, 0, t)`、次の `nextval` は 27。`RESTART` で `(10, 0, f)`、`RESTART WITH 50` で `(50, 0, f)`。この状態で `MAXVALUE 40` は `22023 RESTART value (50) cannot be greater than MAXVALUE (40)`（現在の値を検査する）。

### 5.6 `DROP SEQUENCE`

アナライザ（`analyze_drop_sequence`）: 名前ごとに `relation_kind`。なければ `42P01 sequence "{name}" does not exist`（`IF EXISTS` なら NOTICE（SQLSTATE 00000）`sequence "{name}" does not exist, skipping` で `missing` に積む）。Sequence でなければ `42809 "{name}" is not a sequence`、HINT `Use DROP TABLE to remove a table.`（表）／`Use DROP INDEX to remove an index.`（インデックス）。

`ddl/sequence.rs` の `drop`（`DROP SEQUENCE a, b [CASCADE]`）:

```text
drop(ctx, d) → "DROP SEQUENCE":
  全対象について先に検査し、1 つでも失敗したら何も消さずにエラーにする（PostgreSQL は全対象の依存を集めて 1 回で報告する）:
    for t in d.targets:
       deps_of_self = depend::dependencies_of(t)       // このシーケンスが依存する先
       if deps_of_self に Internal がある:             // IDENTITY のシーケンス（CASCADE でも落とせない）
          2BP01 `cannot drop sequence {s} because column {c} of table {t} requires it`、HINT `You can drop column {c} of table {t} instead.`
       dependents = depend::dependents_of(ObjAddr{pg_class, t.oid, 0})                // 'n' の依存元 = pg_attrdef の行
       restricted なのに dependents が空でない:
          2BP01 `cannot drop sequence {s} because other objects depend on it`、
          DETAIL: dependents を OID 昇順に 1 行ずつ `{describe} depends on sequence {s}`（改行区切り。100 行を超えたら `and {N} other objects (see server log for list)`）、
          HINT `Use DROP ... CASCADE to drop the dependent objects too.`
  実行（検査を通ったもの）:
    for t in d.targets:
       for (obj, _) in dependents（CASCADE のとき）: catalog.drop_default(w, snap, 表, attnum)?; depend の行（その attrdef を依存元とするもの）を消す
       depend の行（t を依存元とするもの、依存先とするもの）を消す
       catalog.drop_sequence(w, snap, t.oid)?
       ctx.txn.pending_unlinks.push(t.locator)                     // ファイルはコミット時に消す（M3 §5.3 の手順 3）
    ctx.txn.catalog_dirty = true
  CASCADE で消したものがあれば NOTICE（SQLSTATE 00000）:
    1 個: `drop cascades to default value for column {c} of table {t}`
    2 個以上: `drop cascades to {N} other objects`、DETAIL: 各 `drop cascades to {describe}` を改行区切り
```

- 実機の確認: 2BP01 の DETAIL は OID 昇順（`default value for column a of table d1` → `b of table d2` → `c of table d2`）。IDENTITY のシーケンスは `CASCADE` でも `requires it`。
- `DROP TABLE`（`07-catalog-ddl.md`）は表の列が所有するシーケンス（`a` と `i`）も落とす。そのために `ddl/sequence.rs` が `pub(crate) fn drop_owned_by_table(ctx, table: Oid) -> Result<()>`（`dependents_of(表の列)` のうち relkind `S` で種類が `a` / `i` のものを、依存の検査なしに落とす。表の DEFAULT はこの表と一緒に消えるため）を公開し、`DROP TABLE` が呼ぶ。実機: `DROP TABLE t`（SERIAL / IDENTITY の列あり）はシーケンスを黙って消す（NOTICE なし）。
- `TRUNCATE ... RESTART IDENTITY`（`07-catalog-ddl.md`）は `pub(crate) fn restart_owned_by_table(ctx, table: Oid) -> Result<()>` を呼ぶ: 所有するシーケンス（`a` と `i`。SERIAL も含む。実機で確認）ごとに `reset(&h, &h.params, Some(h.params.start))`。`CONTINUE IDENTITY`（既定）は何もしない。

### 5.7 SERIAL の書き換え（アナライザ。`analyze_create_table` の列ごと）

PostgreSQL の `transformColumnDefinition` と同じ順序で書く。**シーケンスの作成は表の前**、`OWNED BY` と `DEFAULT` の確定は表の後。

```text
for (i, col) in ct.columns:                       // attnum = i + 1
  1. serial の判定: 型名が 1 要素（スキーマ修飾なし）で小文字化して
       smallserial / serial2 → int2、serial / serial4 → int4、bigserial / serial8 → int8（typmod は無視）。
     配列（serial[]）→ 0A000 `array of serial is not implemented`（位置つき）
     （M2 の KNOWN_UNSUPPORTED_TYPES から serial 系を外す）
  2. serial でなければ型を解決する（既存）。serial なら col の型 := int2 / int4 / int8
  3. serial なら制約の並びの**末尾**に「DEFAULT（シーケンス）」と「NOT NULL」を足した扱いにする（利用者の制約との衝突を PostgreSQL と同じ文言で検出するため）:
       name = choose_relation_name(schema, 表名, 列名, "seq")                    // §6.3
       sequences.push(BoundCreateSequence { name, params: init_params(as_type = 型, Create).params, owner: NewTableColumn { attnum, serial_default: true }, for_identity: false, .. })
  4. 制約を順に処理（saw_nullable / saw_default / saw_identity）:
       NULL        : saw_nullable && is_not_null → 42601 `conflicting NULL/NOT NULL declarations for column "{c}" of table "{t}"`。is_not_null = false
       NOT NULL    : saw_nullable && !is_not_null → 同じ 42601。is_not_null = true
       DEFAULT     : saw_default → 42601 `multiple default values specified for column "{c}" of table "{t}"`
       IDENTITY    : §5.8
       CHECK / PRIMARY KEY / UNIQUE … は M2・07 のまま
       各制約の後: saw_default && saw_identity → 42601 `both default and identity specified for column "{c}" of table "{t}"`
  5. 列の確定: serial なら not_null = true、default はアナライザでは None（ddl が §3.9 のテキストを入れる）
```

`ddl/table.rs`（07）の CREATE TABLE（シーケンスを含むとき）の手順（07 の手順に差し込む）:

```text
  A. 表の OID と locator を決める（07 のとおり）
  B. for c in bound.sequences: seq_oid[c] = get_new_relation_oid(..)           // 先に全部決める（DEFAULT のテキストに要る）
  C. 列の補正: serial の列 default = Some(BoundExprSource { expr_sql: format!("nextval('{seq_oid}'::regclass)") })、not_null = true
              identity の列 identity = Some(kind)、not_null = true（default は None）
  D. 07 の手順で表の行（pg_class、pg_attribute、pg_attrdef、pg_constraint）と PK / UNIQUE を作る。pg_attrdef の依存（attrdef → シーケンス n、attrdef → 列 a）は 07 が記録する（§5.4）
  E. for c in bound.sequences: sequence::create_with_oid(ctx, c, seq_oid[c], Some((表の OID, attnum)))?      // ファイル・SEQ_LOG・カタログ・シーケンス → 列の依存
```

- `CREATE TABLE IF NOT EXISTS` で表がすでにあるときは、シーケンスも作らない（実機で確認）。
- 実機で確認した値: `serial` の `pg_sequence` は `seqtypid = 23`、`seqmax = 2147483647`（`smallserial` は 21 と 32767、`bigserial` は 20 と 9223372036854775807）。`\d` の Default 列は `nextval('t_id_seq'::regclass)`、`pg_depend` はシーケンス → 列が `a`、`pg_attrdef` → シーケンスが `n`。
- 名前の例（実機）: `y_a_seq` と `y_a_seq1` が既にあれば `y_a_seq2`。列名に空白があれば `"x 17_a b_seq"`（引用符つきで表示される）。長い名前は `make_object_name` が 63 バイトに縮める（`aaaa…_bbbb…_seq`）。
- `CREATE TABLE ... (b int DEFAULT nextval('x18_a_seq'), a serial)` のように**同じ文が作るシーケンス**の名前を DEFAULT に書くと、PostgreSQL は成功する（シーケンスを先に作ってから DEFAULT を解析する）。yuzhu は DEFAULT を文の解析時に解析するので `42P01 relation "x18_a_seq" does not exist` になる（既知の差。`[08-Q10]`）。

### 5.8 IDENTITY

CREATE TABLE の列（アナライザ。§5.7 の手順 4 の IDENTITY）:

```text
  IDENTITY（制約 { when, options }）:
    saw_identity → 42601 `multiple identity specifications for column "{c}" of table "{t}"`
    列の型が int2 / int4 / int8 でなければ 22023 `identity column type must be smallint, integer, or bigint`
    options の SeqOption を SeqOptions にする。As があれば 42601 `conflicting or redundant options`（PostgreSQL は型を表す AS を先頭に足すため、利用者の AS が重複扱い。実機で確認）、
       OwnedBy / persistence 系は 0A000、SequenceName は採用（スキーマ修飾なしなら表と同じ名前空間）
    out = init_params(&opts with as_type = 列の型, for_identity = true, Create)?
    名前: SequenceName があればそれ、なければ choose_relation_name(schema, 表名, 列名, "seq")
    sequences.push(BoundCreateSequence { owner: NewTableColumn { attnum, serial_default: false }, for_identity: true, .. })
    col.identity = Some(when の IdentityKind)、saw_identity = true
    暗黙の NOT NULL: saw_nullable && !is_not_null → 42601 `conflicting NULL/NOT NULL declarations ...`。is_not_null = true
```

INSERT の規則（`identity_insert_rule`。**表の各 IDENTITY 列について、行ごと・列ごと**に適用する。PostgreSQL の `rewriteTargetListIU`。実機で確認）:

| 列の指定 | `OVERRIDING` なし | `OVERRIDING SYSTEM VALUE` | `OVERRIDING USER VALUE` |
|---|---|---|---|
| 列リストにない／`DEFAULT` | `Generate`（nextval） | `Generate` | `Generate` |
| 値あり（`GENERATED ALWAYS`） | **`428C9`** | `Given` | `Generate`（与えた値は捨てる） |
| 値あり（`GENERATED BY DEFAULT`） | `Given` | `Given` | `Generate` |

- `428C9` の文言: `cannot insert a non-DEFAULT value into column "{c}"`、DETAIL `Column "{c}" is an identity column defined as GENERATED ALWAYS.`、HINT `Use OVERRIDING SYSTEM VALUE to override.`
- 複数行の `VALUES` は行ごとに判定する（`(DEFAULT, 'x'), (3, 'y')` は 2 行目で `428C9`）。`INSERT ... SELECT` の出力列は「値あり」として扱う（`SELECT 9, 'sel'` は `OVERRIDING` なしで `428C9`、`OVERRIDING SYSTEM VALUE` で通る）。`NULL` は「値あり」なので、`OVERRIDING SYSTEM VALUE` や `BY DEFAULT` で明示的に `NULL` を入れると `23502`（`GENERATED ALWAYS` に `(null, 'n')` を `OVERRIDING SYSTEM VALUE` で入れても同じ）。
- IDENTITY でない表への `OVERRIDING` は効果がない（エラーにならない）。`INSERT ... DEFAULT VALUES` は `OVERRIDING` と併用できない（構文エラー）。
- 解析: `Generate` の列は `identity_default_expr` の式を `defaults[col]` に入れ、`column_map` はその列を `None`（値は DEFAULT）にする。`Given` は与えられた式のまま。

UPDATE の規則（`identity_update_rule`。**WHERE に当たる行がなくても解析時にエラー**）:

| SET の右辺 | `GENERATED ALWAYS` | `GENERATED BY DEFAULT` |
|---|---|---|
| `DEFAULT` | 通る（新しい `nextval`。`UpdateSource::Default(identity_default_expr)`） | 通る（同じ） |
| それ以外（`id = id` も） | **`428C9 column "{c}" can only be updated to DEFAULT`**、DETAIL `Column "{c}" is an identity column defined as GENERATED ALWAYS.` | 通る |

- `COPY`（`10-explain-copy-compat.md`）は IDENTITY の制限を**検査しない**（`GENERATED ALWAYS` の列にも COPY で値を入れられる。実機で確認）。列リストにない IDENTITY 列は `identity_default_expr` で `nextval` を使う。

### 5.9 他の文からの見え方

| 文 | 動作 |
|---|---|
| `SELECT * FROM s` | 通常のヒープ走査。列は `last_value`・`log_cnt`・`is_called`（`int8`・`int8`・`bool`）。結合・副問い合わせにも使える。`xmin = 2`、`ctid = (0,1)`、`tableoid::regclass = s`。`EXPLAIN` は `Seq Scan on s`（`width=17`） |
| `INSERT` / `UPDATE` / `DELETE` ... シーケンス | `42809 cannot change sequence "s"`（アナライザ。`03-parser-analyzer.md`） |
| `COPY s FROM` | `42809 cannot copy to sequence "s"`（`10-explain-copy-compat.md`） |
| `TRUNCATE s` | `42809 "s" is not a table`（07） |
| `CREATE INDEX ON s(...)` | `42809 cannot create index on relation "s"`、DETAIL `This operation is not supported for sequences.`（07） |
| `ALTER TABLE s ADD ...` | `42809 ALTER action ADD CONSTRAINT cannot be performed on relation "s"`、DETAIL `This operation is not supported for sequences.`（07） |
| `DROP TABLE s` | `42809 "s" is not a table`、HINT `Use DROP SEQUENCE to remove a sequence.`（07）。`DROP INDEX s` は `"s" is not an index`、同じ HINT |
| `VACUUM s` / `ANALYZE s` | M4 は何もしない（PostgreSQL は `WARNING: skipping "s" --- cannot vacuum non-tables or special system tables`）。07 が決める |
| psql `\ds` | `pg_class` の `relkind IN ('S','')` の行。`Schema`・`Name`・`Type = sequence`・`Owner`（`pg_get_userbyid(relowner)`）。**IDENTITY と SERIAL のシーケンスも並ぶ**。実機の SQL は `10-explain-copy-compat.md` が確かめる |
| psql `\d シーケンス` | `pg_class`（`relam` の LEFT JOIN は NULL）、`pg_sequence`（`format_type(seqtypid, NULL)`、`seqstart`、`seqmin`、`seqmax`、`seqincrement`、`CASE WHEN seqcycle THEN 'yes' ELSE 'no' END`、`seqcache`）、`pg_depend`（`Owned by: public.t.id` の行。`deptype IN ('a','i')`、`quote_ident`） |
| psql `\d 表` | Default 列に `nextval('t_id_seq'::regclass)`（`pg_get_expr`）。IDENTITY の列は `generated always as identity` / `generated by default as identity`（`attidentity`） |

### 5.10 クラッシュ後の挙動

**不変条件 I15（シーケンスは払い出した値を二度払い出さない）**: クライアントが `COMMIT`（または暗黙のトランザクションの終了）の確認を受け取ったトランザクションが `nextval` で受け取った値は、リカバリの後のどの `nextval` も、その値より先（昇順なら大きい値、降順なら小さい値）だけを返す。`CYCLE` の折り返しは除く。中断したトランザクションと、確認を受け取る前にクラッシュしたトランザクションの値は保証しない（PostgreSQL と同じ）。

**なぜ成り立つか**:

1. REDO は `SEQ_LOG` を**無条件に**ページに置く（§3.7）。リカバリ後の状態は、REDO 点より後の最後の `SEQ_LOG` の状態（払い出した値より最大 32 個先）以上になる。
2. REDO 点より前の `SEQ_LOG` は再生されないが、チェックポイントが（REDO 点の時点で dirty だった）ページを書き出す。**チェックポイント後の最初の `nextval` は必ず `SEQ_LOG` を書く**（`force_log`）ので、REDO 点より後にログなしで進んだページには、その前に REDO 点より後の `SEQ_LOG` がある。
3. D8-7 の順序（dirty を先に立てる → REDO 点を読む）が、「REDO 点の決定と dirty ページの集合の取得の間に入った更新」の穴を塞ぐ。
4. D8-5: 払い出した値を覆う `SEQ_LOG`（他のセッションが書いたものを含む）の終端 LSN が `wal_flush_upto` に入り、コミットがそこまで flush するので、確認を受け取った値は WAL に残っている。
5. 起動直後（クリーンな起動もリカバリの後も）の最初の `nextval` は `force_log`（`Wal::redo_lsn()` が起動直後の REDO 点を返すこと。FPW と同じ要件）。

**欠番**: クラッシュで最大 `SEQ_LOG_VALS = 32` 個（`CACHE n` なら先取りした未使用の分が加わる）。**PostgreSQL 17 の実機（`kill -9` と immediate 停止）で確かめた値**（yuzhu も同じになる。再起動テストの固定値）:

| 操作 | 次の `nextval` |
|---|---|
| `nextval` ×3（`CACHE 1`）→ クラッシュ | **34**（WAL の `1 + 32 = 33` の次） |
| `CACHE 5` で `nextval` ×2 → クラッシュ | **38**（WAL の `1 + 36 = 37` の次） |
| `nextval` ×3 → `CHECKPOINT` → `nextval`（4）→ クラッシュ | **37**（チェックポイント後の最初で `3 + 33 = 36` を記録） |
| `nextval` ×2 → `CHECKPOINT` → クラッシュ | **3**（欠番なし。ページが書き出され、REDO する `SEQ_LOG` がない） |
| `nextval` ×1 → `CHECKPOINT` → クラッシュ | **2**（同上） |
| `serial` 列に `INSERT` ×3 → クラッシュ → `INSERT` | `id = 34` |
| `nextval`（`r1` 1〜3、`CACHE 5` の `r2` 1〜2、`serial` の `rt` 2 行）→ **正常停止（smart）→ 再起動** → `nextval` | `r1 = 4`、`r2 = 6`（先取りの 3〜5 は失われる）、`rt` の `id = 3`（**欠番なし**。停止チェックポイントがページの実際の状態を書く） |
| 上の続き（それぞれ 1 回 `nextval` した後）→ **immediate 停止 → 再起動** → `nextval` | `r1 = 37`、`r2 = 43`、`rt` の `id = 36`（再起動後の最初の `nextval` が `force_log` で 33 先を記録するため） |

**ロールバック**: `nextval` はロールバックされない（`BEGIN; SELECT nextval('s'); ROLLBACK;` の後も進んだまま）。`CREATE SEQUENCE` はロールバックで消える（`pending_creates`）。REDO は後者の `SEQ_LOG` でファイルを作り、`ABORT` の `rels` が消す。

---

## 6. モジュールごとの仕様

### 6.1 エラーの一覧（SQLSTATE と文言。PostgreSQL 17 の実機）

`SEQUENCE_GENERATOR_LIMIT_EXCEEDED`（2200H）・`GENERATED_ALWAYS`（428C9）・`DEPENDENT_OBJECTS_STILL_EXIST`（2BP01）は `00 §15.3`。ほかは `error.rs` の既存の定数。位置（`LINE n`）は構文・解析のエラーに付ける。

| 状況 | SQLSTATE | メッセージ（DETAIL / HINT） |
|---|---|---|
| 昇順で最大値を超える | 2200H | `nextval: reached maximum value of sequence "s" (3)` |
| 降順で最小値を下回る | 2200H | `nextval: reached minimum value of sequence "s" (-2)` |
| `setval` が範囲外 | 22003 | `setval: value 32768 is out of bounds for sequence "l4" (1..32767)` |
| `currval` が未定義 | 55000 | `currval of sequence "s" is not yet defined in this session` |
| `lastval` が未定義（DROP 後も） | 55000 | `lastval is not yet defined in this session` |
| READ ONLY のトランザクション | 25006 | `cannot execute nextval() in a read-only transaction` / `cannot execute setval() in a read-only transaction` |
| 引数がシーケンスでない | 42809 | `cannot open relation "t"`、DETAIL `This operation is not supported for tables.`（インデックスは `indexes`） |
| 存在しない OID | XX000 | `could not open relation with OID 99999` |
| ページの magic が違う | XX001 | `bad magic number in sequence "s": 00000000` |
| 存在しない名前（`nextval('x')`） | 42P01 | `relation "x" does not exist`（位置つき。regclass の入力） |
| CREATE: 同名がある | 42P07 | `relation "s" already exists`。IF NOT EXISTS は NOTICE `relation "s" already exists, skipping` |
| CREATE: pg_catalog | 42501 | `permission denied to create "pg_catalog.zz"`、DETAIL `System catalog modifications are currently disallowed.` |
| オプションの重複 | 42601 | `conflicting or redundant options`（位置つき） |
| `SEQUENCE NAME`（CREATE） | 42601 | `invalid sequence option SEQUENCE NAME`（位置つき） |
| `AS` の型が違う | 22023 | `sequence type must be smallint, integer, or bigint`（IDENTITY は `identity column type must be smallint, integer, or bigint`） |
| 型が存在しない | 42704 | `type "nope" does not exist`（位置つき） |
| 数値が範囲外 | 22003 | `value "9223372036854775808" is out of range for type bigint` |
| 数値が整数でない | 22P02 | `invalid input syntax for type bigint: "1.5"`（`3.0` も同じ） |
| `INCREMENT 0` | 22023 | `INCREMENT must not be zero` |
| `MAXVALUE` / `MINVALUE` が型の範囲外 | 22023 | `MAXVALUE (40000) is out of range for sequence data type smallint` / `MINVALUE (-40000) is out of range ...` |
| `MINVALUE >= MAXVALUE` | 22023 | `MINVALUE (5) must be less than MAXVALUE (1)` |
| `START` が範囲外 | 22023 | `START value (0) cannot be less than MINVALUE (1)` / `START value (11) cannot be greater than MAXVALUE (10)` |
| `RESTART`（または ALTER 時の現在の値）が範囲外 | 22023 | `RESTART value (500) cannot be greater than MAXVALUE (100)` / `RESTART value (..) cannot be less than MINVALUE (..)` |
| `CACHE` が 0 以下 | 22023 | `CACHE (0) must be greater than zero` |
| OWNED BY: 構文 | 42601 | `invalid OWNED BY option`、HINT `Specify OWNED BY table.column or OWNED BY NONE.`（`OWNED BY` に 1 要素で `none` 以外） |
| OWNED BY: 表でない | 42809 | `sequence cannot be owned by relation "s1"`、DETAIL `This operation is not supported for sequences.` |
| OWNED BY: 別のスキーマ | 55000 | `sequence must be in same schema as table it is linked to` |
| OWNED BY: 列がない | 42703 | `column "nope" of relation "u3" does not exist` |
| IDENTITY のシーケンスに OWNED BY | 0A000 | `cannot change ownership of identity sequence`、DETAIL `Sequence "idt_id_seq" is linked to table "idt".` |
| ALTER: 存在しない | 42P01 | `relation "nope" does not exist`。IF EXISTS は NOTICE `relation "nope" does not exist, skipping` |
| DROP: 存在しない | 42P01 | `sequence "nope" does not exist`。IF EXISTS は NOTICE `sequence "nope" does not exist, skipping` |
| DROP: シーケンスでない | 42809 | `"tt" is not a sequence`、HINT `Use DROP TABLE to remove a table.` |
| DROP: 依存するものがある | 2BP01 | `cannot drop sequence ds because other objects depend on it`、DETAIL `default value for column a of table d1 depends on sequence ds`（複数は改行）、HINT `Use DROP ... CASCADE to drop the dependent objects too.` |
| DROP: IDENTITY のシーケンス | 2BP01 | `cannot drop sequence d3_id_seq because column id of table d3 requires it`、HINT `You can drop column id of table d3 instead.` |
| DROP CASCADE の通知 | 00000（NOTICE） | `drop cascades to default value for column id of table t`（複数は `drop cascades to 3 other objects` と DETAIL） |
| SERIAL: 配列 | 0A000 | `array of serial is not implemented` |
| 列の制約の衝突 | 42601 | `multiple default values specified for column "a" of table "x4"` / `conflicting NULL/NOT NULL declarations for column "a" of table "x5"` / `both default and identity specified for column "a" of table "x2"` / `multiple identity specifications for column "a" of table "x3"` |
| IDENTITY の INSERT | 428C9 | `cannot insert a non-DEFAULT value into column "id"`、DETAIL `Column "id" is an identity column defined as GENERATED ALWAYS.`、HINT `Use OVERRIDING SYSTEM VALUE to override.` |
| IDENTITY の UPDATE | 428C9 | `column "id" can only be updated to DEFAULT`、DETAIL `Column "id" is an identity column defined as GENERATED ALWAYS.` |
| シーケンスへの DML | 42809 | `cannot change sequence "s1"` |
| TEMP / UNLOGGED / RENAME / SET SCHEMA | 0A000 | `temporary sequences are not supported yet` など（§1.2） |

### 6.2 `init_params` の検査の順序と計算（`catalog/seq_params.rs`）

PostgreSQL の `init_params` の順序をそのまま写す（最初に出るエラーを一致させるため）。`isInit = (mode == Create)`。`seqform` は結果の `SequenceParams`、`data` は結果の `SeqState`。`reset_max` / `reset_min` は `AS` の型の変更で最大・最小を型に合わせ直すフラグ。

```text
前処理（Alter のみ）: seqform = *current、data = state（log_cnt は後で 0 にする）
Create なら data.log_cnt = 0

1. AS type（as_type が Some）:
     型が int2 / int4 / int8 でなければ 22023（for_identity で文言が違う）
     Alter なら: current.type_oid の最大が元の型の最大値と等しい → reset_max = true、最小が元の型の最小値と等しい → reset_min = true
     seqform.type_oid = 新しい型
   Create で as_type が None なら type_oid = int8
2. INCREMENT: Some(v) → v == 0 なら 22023。seqform.increment = v。Create で None なら 1
3. CYCLE: Some(b) → seqform.cycle = b。Create で None なら false
4. MAXVALUE: Some(Some(v)) → max = v。Some(None) または Create で None または reset_max → 昇順（increment > 0）または reset_max なら型の最大、降順なら -1
   型が int2 / int4 で max が型の範囲外 → 22023 `MAXVALUE (v) is out of range for sequence data type {smallint|integer}`
5. MINVALUE: Some(Some(v)) → min = v。Some(None) または Create で None または reset_min → 降順または reset_min なら型の最小、昇順なら 1
   範囲外 → 22023 `MINVALUE ...`
6. min >= max → 22023 `MINVALUE (min) must be less than MAXVALUE (max)`
7. START: Some(v) → start = v。Create で None なら 昇順は min、降順は max
   start < min → 22023 `START value (..) cannot be less than MINVALUE (..)`、start > max → `START value (..) cannot be greater than MAXVALUE (..)`
8. RESTART: Some(Some(v)) → data = (v, 0, false)、Some(None) → data = (start, 0, false)。Create で None → data = (start, 0, false)
   data.last_value < min → 22023 `RESTART value (..) cannot be less than MINVALUE (..)`、> max → `RESTART value (..) cannot be greater than MAXVALUE (..)`
   （Alter で RESTART がなくても、現在の last_value を同じ式で検査する。MIN / MAX を変えたときの整合性）
9. CACHE: Some(v) → v <= 0 なら 22023 `CACHE (v) must be greater than zero`。cache = v。Create で None なら 1
最後に data.log_cnt = 0（yuzhu は ALTER でも常に 0。D8-8）
```

実機の検証済みの値（`seq_params` の単体テストの固定値）:

| 入力 | 結果 |
|---|---|
| `create sequence e14 no minvalue maxvalue 5 increment -1` | 成功（`min = i64::MIN`、`max = 5`、`start = 5`） |
| `increment -1 start 5`（降順の既定の `max = -1`） | 22023 `START value (5) cannot be greater than MAXVALUE (-1)` |
| `as smallint start 40000` | 22023 `START value (40000) cannot be greater than MAXVALUE (32767)` |
| `as integer increment -1` | `min = -2147483648`、`max = -1`、`start = -1`（`\d` の Start `-1`） |
| `maxvalue 0 minvalue -5` | 成功（`start = -5`） |
| `start 3 restart 4` | `seqstart = 3`、状態 `(4, 0, f)` |
| ALTER: `AS smallint`（`max = 100`、型は bigint）| `max` は 100 のまま、型だけ 21 |
| ALTER: `int` + `INCREMENT -1` を `AS smallint` | `min = -32768`、`max = -1` |
| ALTER: 現在 `(50, 0, f)` で `MAXVALUE 40` | 22023 `RESTART value (50) cannot be greater than MAXVALUE (40)` |
| ALTER: `START 20`（`min` 1）の後 `MINVALUE 30` | 22023 `START value (20) cannot be less than MINVALUE (30)` |

### 6.3 `choose_relation_name`（`analyzer/ddl.rs`）

PostgreSQL の `ChooseRelationName(relname, colname, "seq", namespace)`。

```rust
/// 既存の make_object_name（名前を 63 バイトに縮める）で <name1>_<name2>_<label> を作り、
/// 同じ名前空間のリレーション（表・インデックス・シーケンス）にも taken（同じ文で選んだ名前と、作る表自身の名前）にもなければ採用。
/// あれば label を label1、label2 ... に変えて繰り返す（PostgreSQL の pass）
pub(super) fn choose_relation_name(catalog: &dyn CatalogReader, schema: &str, name1: &str, name2: Option<&str>,
                                   label: &str, taken: &HashSet<String>) -> Result<String>;
```

### 6.4 ファイルごとの内容

| ファイル | 内容 |
|---|---|
| `storage/sequence.rs` | §4.1〜§4.3 の定数・`SeqStore`・`plan_fetch`・`read_state` / `write_state` / `seq_tuple_bytes`・`redo`・`describe`。ページを触る関数はすべて M3 規約 1 の形（`init` は `CriticalSection` の中で `init_special`・magic・`add_item`・`SEQ_LOG`・`set_lsn`） |
| `executor/seq.rs` | §4.4 の `SeqSession` / `SeqRuntime` と `handle_from_def` |
| `types/ops.rs` | §4.5 の 5 つの関数（`oid_arg(args, i)` で `Datum::Oid` を取り出す小さな補助） |
| `catalog/seq_params.rs` | §4.7 と §6.2 |
| `analyzer/ddl.rs`（Q1 の範囲） | `analyze_create_sequence` / `analyze_alter_sequence` / `analyze_drop_sequence`、SERIAL・IDENTITY の処理（§5.7・§5.8）、`choose_relation_name`、`identity_insert_rule` / `identity_update_rule` / `identity_default_expr`、`process_owned_by` の解析側（表と列の解決・検査） |
| `ddl/sequence.rs` | §5.4〜§5.6 の `create` / `create_with_oid` / `alter` / `drop` / `drop_owned_by_table` / `restart_owned_by_table`、`process_owned_by` の実行側（`pg_depend` の更新） |
| `txn/manager.rs` | `Transaction.{wal_flush_upto, started_at}`・`note_wal`・`TxnManager::finish_without_xid`（§4.6） |
| `session.rs`（S。この章が頼む） | `seq_state: RefCell<SeqSession>`、文ごとの `SeqRuntime`、`end_statement` → `note_wal`、`commit_transaction` の XID なしの経路（§5.3）、`started_at` の設定 |
| `recovery.rs`（R。この章が頼む） | `dispatch` に `RmgrId::Seq => storage::sequence::redo` |
| `wal/dump.rs`（W2） | `RmgrId::Seq => storage::sequence::describe` |
| `storage/stack.rs`（C / 06） | `StorageStack.seq = Arc::new(SeqStore::new(pool, smgr, wal))` |
| `debug_knobs.rs`（A） | `seq_ignore_foreign_wal`、`seq_redo_skip_if_page_newer`、`seq_no_force_log`（いずれも `bool`。§7.4 の変異試験。§11） |

---

## 7. テスト

### 7.1 共通の SQL テスト（`tests/slt/m4/seq/`）

PostgreSQL 17 で期待値を確かめる（M1〜M3 と同じ規則。**PostgreSQL で通らないテストは置かない**）。名前は他のファイルと衝突しないよう接頭辞（`sq_`、`sr_`、`id_`）を付け、ファイルの最後で後始末（DROP）をする（M2-Q22）。`nextval` の後の `log_cnt` は PostgreSQL のチェックポイントのタイミングで変わりうるので、比べるのは `onlyif yuzhu` の 1 ファイルだけにする（作った直後の `log_cnt = 0` は両方で一致するので使ってよい）。

| ファイル | 内容（主な文と期待値。値は実機で確認済み） |
|---|---|
| `basic.slt` | 作った直後 `SELECT last_value, log_cnt, is_called` が `1, 0, f`。`nextval` ×2 が `1`, `2`、`currval` と `lastval` が `2`。`SELECT nextval('s') FROM generate_series(1,3)`。`SELECT nextval('s'), nextval('s')` は左から。`nextval(NULL)`・`setval('s', NULL)` は NULL。`pg_typeof(nextval(..))` は `bigint`。`nextval('s'::text)`・`nextval(oid)` |
| `limits.slt` | `MAXVALUE 3` で `1, 2, 3` の後 `2200H`（メッセージを照合）、エラーの後 `last_value = 3, is_called = t`、`currval` は 3。`MAXVALUE 3 CYCLE` で `1,2,3,1,2,3,1`。降順 `INCREMENT -1 MINVALUE -2 MAXVALUE 0 START 0` で `0,-1,-2` の後 `2200H`（minimum）。`smallint` の `setval(32767)` の後 `nextval` が 2200H、`setval(32768)` と `setval(0)` が 22003。`INCREMENT 4611686018427387904 MAXVALUE 9223372036854775807 CYCLE` の 4 回が `1, 4611686018427387905, 1, 4611686018427387905` |
| `setval_currval.slt` | `setval('s', 5, false)` の後 `currval` は 55000、`nextval` は 5。`setval('s', 7, true)` の後 `nextval` は 8、`currval` は 7 の後 8。`setval` は `lastval` を変えない。新しいセッション（`connection other`）で `currval` / `lastval` が 55000。DROP 後の `lastval` が 55000 |
| `cache.slt` | `CACHE 5` の 2 接続の交互の `nextval`（接続 A が 1、B が 6、A が 2〜5、A が 11）。`CACHE 3 MAXVALUE 7` の 2 接続（A = 1、B = 4、A = 2、A = 3、A = 7、B = 5、B = 6、B の次が 2200H。上限での打ち切り）。`INCREMENT 2 CACHE 5 START 10` の 2 回目が 12 |
| `readonly_txn.slt` | `BEGIN READ ONLY` の `nextval` と `setval` が 25006（`currval` は通る）。ブロックは Failed になり `25P02` |
| `rollback.slt` | `BEGIN; nextval ×2; ROLLBACK` の後も進む。`BEGIN; CREATE SEQUENCE; nextval; ROLLBACK` の後は `nextval` が 42P01。`CREATE TABLE ... serial` の ROLLBACK でシーケンスも消える |
| `create_options.slt` | §6.1 と §6.2 の CREATE の検証メッセージを全部（`statement error` に SQLSTATE かメッセージの一部）。`\d` 相当は `pg_sequence` の `seqtypid` / `seqstart` / `seqmin` / `seqmax` / `seqincrement` / `seqcache` / `seqcycle`。`AS int2/int4/int8/integer/smallint/bigint`、`START WITH`、`INCREMENT BY -1`、`NO MINVALUE`、`NO CYCLE`、`RESTART` |
| `alter.slt` | `RESTART`、`RESTART WITH`、`INCREMENT`、`MINVALUE` / `MAXVALUE` / `NO MAXVALUE`、`CACHE`、`CYCLE`、`START`、`AS` の型変更と `min` / `max` の調整、現在の値を超える `MAXVALUE`、`IF EXISTS`、存在しない・表への ALTER。`OWNED BY NONE` と `OWNED BY t.c`（所有の変更を `pg_depend` で確認）、別スキーマの 55000 |
| `drop.slt` | 存在しない（42P01）、`IF EXISTS`（NOTICE）、`DROP SEQUENCE tt`（42809 と HINT）、`DROP TABLE s`（42809）、複数対象、`DEFAULT nextval` を持つ表がある間の 2BP01（DETAIL が OID 順）、`CASCADE` の NOTICE（1 個と複数）と `DEFAULT` が消えること、IDENTITY のシーケンスは CASCADE でも 2BP01 |
| `serial.slt` | `serial` / `smallserial` / `bigserial` / `serial2` / `serial4` / `serial8` の列の型。`pg_sequence` の `seqtypid` / `seqmax`。`pg_get_expr(adbin, adrelid)` が `nextval('t_id_seq'::regclass)`。`pg_depend`（シーケンス → 列 `a`、`pg_attrdef` → シーケンス `n`）。名前の衝突（`y_a_seq` と `y_a_seq1` がある → `y_a_seq2`）、長い名前、引用符。`serial NULL` / `serial DEFAULT 5` / `serial[]` / 型付きの衝突のエラー。`DROP TABLE` でシーケンスが消える。`CREATE TABLE IF NOT EXISTS` が何も作らない。`INSERT` の `id` が 1, 2, ...、明示の値は消費しない。`lastval()` が `INSERT` の後に使える |
| `identity.slt` | `ALWAYS` / `BY DEFAULT`、オプション（`START` / `INCREMENT` / `MINVALUE` / `MAXVALUE` / `CACHE` / `CYCLE` / `SEQUENCE NAME`）、§5.8 の INSERT の表の全マス（428C9 の文言、`OVERRIDING SYSTEM / USER VALUE`、複数行 VALUES、`INSERT ... SELECT`、`NULL` の 23502）、UPDATE の全マス（`WHERE false` でもエラー、`id = id`）、`pg_attribute.attidentity` と `atthasdef = f`、`pg_attrdef` に行なし、`pg_depend` が `i`、型エラー（text / numeric）、`DROP SEQUENCE` の 2BP01、`TRUNCATE ... RESTART IDENTITY`（SERIAL も） |
| `catalog_rows.slt` | `pg_class`（`relkind = 'S'`、`relam = 0`、`relpages = 1`、`relreplident = 'n'`、`relfrozenxid = 0`、`reltype = 0`）、`pg_attribute`（3 列 + システム列）、`pg_type` に行がない、`pg_sequence` |
| `select_from_seq.slt` | `SELECT * FROM s`、`xmin = 2`・`ctid = (0,1)`、結合、`INSERT` / `UPDATE` / `DELETE` / `TRUNCATE` / `CREATE INDEX` / `COPY` / `ALTER TABLE ADD` のエラー（42809 の文言）、`DROP INDEX s` |
| `log_cnt.slt` | `onlyif yuzhu`。作った直後 `(1, 0, f)` → 1 回で `(1, 32, t)` → 2 回で `(2, 31, t)`。`CACHE 5` の 1 回目 `(5, 32, t)`。`setval` / `ALTER` の後は `log_cnt = 0` |
| `psql/ds.slt`（10 章と共有） | `\ds` が返す SQL（`relkind IN ('S','')`）の結果に SERIAL・IDENTITY・通常のシーケンスが並ぶ |

### 7.2 再起動・クラッシュをまたぐテスト（`tests/restart/m4/`）

M3 §7.2 の仕組み（`--restart` は smart 停止、`--crash` は `kill -9`）を使う。期待値が違うので**ディレクトリを分ける**。値は §5.10 の表（PostgreSQL 17 で確認済み）。PostgreSQL 側の `--crash` は本物の PostgreSQL の `kill -9`、yuzhu 側は `tests/yuzhu.sh crash`。

| シナリオ（ディレクトリ） | 実行 | 内容 |
|---|---|---|
| `seq-clean/` | `--restart` | `nextval` ×3 → 再起動 → `4`。`CACHE 5` の 2 回 → 再起動 → `6`。`serial` に 2 行 → 再起動 → 次の `id = 3`。`setval(.., 100)` → 再起動 → `101`。`ALTER SEQUENCE ... RESTART WITH 7` → 再起動 → `7` |
| `seq-crash/` | `--crash` | `nextval` ×3 → クラッシュ → `34`。`CACHE 5` で 2 回 → `38`。`serial` に 3 行 → `id = 34`。`setval(.., 100)` → コミット → クラッシュ → `101`（`setval` は `log_cnt = 0` を記録するので欠番なし） |
| `seq-checkpoint-crash/` | `--crash` | `nextval` ×3 → `CHECKPOINT` → `nextval` → クラッシュ → `37`。`nextval` ×2 → `CHECKPOINT` → クラッシュ → `3`。`nextval` ×1 → `CHECKPOINT` → クラッシュ → `2` |
| `seq-double-crash/` | `--crash` ×3 | `nextval` ×3 → クラッシュ → `nextval` が `34` → クラッシュ → `67` → クラッシュ → `100`（起動直後の最初の `nextval` が `force_log` で 33 先を記録するので、クラッシュのたびに 33 ずつ進む。実機で確認） |
| `seq-create-rollback/` | `--crash` | `BEGIN; CREATE SEQUENCE s; SELECT nextval('s'); ROLLBACK` → クラッシュ → `s` がない。`CREATE SEQUENCE` をコミット → `nextval`（1）→ クラッシュ → `s` があり次は 34。`DROP SEQUENCE` をコミット → クラッシュ → ない。`BEGIN; CREATE TABLE t (id serial); INSERT ...; ROLLBACK` → クラッシュ → `t` も `t_id_seq` もない |
| `seq-mixed-restart/` | `--restart` → `--crash` | 正常停止の後（`r1 = 4`、`r2 = 6`、`rt` の `id = 3`）に immediate 停止 → `r1 = 37`、`r2 = 43`、`rt` の `id = 36`（§5.10 の表の最後の 2 行） |

### 7.3 Rust の単体テスト

| 対象 | 内容 |
|---|---|
| `plan_fetch` | §4.3 の固定値の表。**参照実装との突き合わせ**: PostgreSQL のループを写した `fn plan_fetch_literal(..)`（`#[cfg(test)]`）と、乱数（昇順・降順、型の範囲 3 種、`CACHE` 1〜100、`CYCLE`、`is_called`、`log_cnt` 0〜40、`force_log`、`increment` が `i64` の極端な値）で数十万ケースを比べる（設計時に 285 万ケースで一致を確認済み）。エラーの文言（`maximum` / `minimum`）。`count` が `CACHE` より少なくなる（上限での打ち切り）。1 回の払い出しが等差数列であること（折り返しを含まない） |
| ページとタプル | §3.4 の固定値（`init` の結果と一致）。`read_state` の検査（magic、`lp_len`、`t_hoff`、`natts`、`is_called` の値）が `XX001`。`HeapStore` の `begin_scan` で `SELECT *` と同じ `(Int8, Int8, Bool)` が読める。`xmin` が 2、`ctid` が `(0,1)` |
| WAL | §3.6 の固定値（CRC を除く）。`describe` の表示。`SEQ_LOG` が 113 バイトで `HAS_IMAGE` を含まない |
| REDO | 空のページへの適用が `init` の結果と一致する。**既存のページの LSN がレコードより新しくても無条件に上書きする**。2 回適用しても同じ（冪等）。ファイルがなければ作る。不正なレコード（info、ブロック数、データ長、ヘッダ）が Panic。同じページに `SEQ_LOG` を 3 本適用すると最後の状態 |
| `SeqStore`（`SimVfs` の上の `StorageStack`） | `init` → `fetch` ×N の状態の遷移（§4.3 の表）、`setval` / `read` / `reset`（`restart_with` の有無、範囲外の 22023）、`reset_generation` の増加、2 回目以降の `fetch` が WAL を書かない（`wal.insert_lsn()` が進まない）、`wal.begin_checkpoint_online()` の直後の最初の `fetch` が `SEQ_LOG` を書く、**起動直後（`Wal::open_at` の後）の最初の `fetch` が `SEQ_LOG` を書く**、`SeqRun.wal_lsn` が他のスレッドが書いたログのページ LSN になる、ログなしの更新で dirty が立つ（`page_mut_hint` 直後）、`fetch` が `Err` のときページの内容が変わらない |
| 並行 | N スレッド × M 回の `fetch`（`CACHE 1`）で払い出された値が重複なく `1..=N*M` に一致する。`CACHE 5` で重複しない。チェックポイントが並行して走っても重複しない（`begin_checkpoint_online` と `flush_all_for_checkpoint` を挟む）。`fetch` と `SELECT * FROM s`（共有ラッチでの読み取り）の並行 |
| `SeqSession` / `SeqRuntime` | §5.1・§5.2 の手順: `currval` の 55000、`lastval`（`setval` で変わらない、DROP 後の 55000）、先取りの消費（`last != cached`）、`reset_generation` が進んだら先取りを捨てる、READ ONLY の 25006（先取りの残りがあっても）、`handle` の 42809 / XX000、文ごとの `handles` の破棄、`end_statement` が `pending_flush` を返して 0 に戻す |
| `finish_without_xid` / `Transaction` | `wal_flush_upto == 0` なら flush しない（`SimStats` の sync の回数）、`> 0` なら `wal.flush` を呼ぶ、`note_wal` が最大値を残す、`skip_commit_flush` で省ける、中断では flush しない。`Session` の暗黙のトランザクションの `SELECT nextval` でコミット時に 1 回 flush する（`SimVfs` の sync の順序で確認）。XID ありのコミットは追加の flush をしない |
| `init_params` | §6.1・§6.2 の表の全行（入力 → 結果またはエラーの SQLSTATE と文言）。`parse_seq_int`（`+5`、`-1`、`1.5`、`3.0`、`9223372036854775808`、前後の空白） |
| アナライザ | SERIAL の 6 つの型名、`serial[]`、制約の衝突の 4 つの文言、IDENTITY の型検査、`AS` 重複、`choose_relation_name`（衝突、長い名前、引用符、同じ文の中の衝突の回避）、`identity_insert_rule` の全 9 マス、`identity_update_rule`、`identity_default_expr`（列型への代入キャスト）、`CREATE SEQUENCE` の全検査（§6.1）、`OWNED BY` の解決、`DROP SEQUENCE` の `missing` |
| `ddl` | `create`（ファイル、SEQ_LOG、`pg_class` / `pg_attribute` / `pg_sequence` の行、`pending_creates`、`pg_depend`）、`alter`（`reset` の呼び出し、`pg_sequence` の更新、OWNED BY の付け替え、IDENTITY の所有の拒否）、`drop`（2BP01 の DETAIL の順序、CASCADE、`pending_unlinks`、IDENTITY の拒否）、CREATE TABLE（serial・identity）の全手順、`drop_owned_by_table`、`restart_owned_by_table`、ROLLBACK でファイルが消える |

### 7.4 クラッシュ試験 層 1（`yuzhu-core/tests/crash_sim/`。R2 が書く。ワークロード 7 とこの章の不変条件 I15）

`00 §18`。M3 §7.5 の仕組み（`SimVfs`、1 スレッドで複数の `Session`、確定と不明の記録、`YUZHU_SIM_SEED` / `YUZHU_CRASH_AT`）をそのまま使う。

**ワークロード 7（シーケンス）**: 次の部品をシードで選んで繰り返す。表 `t7(id serial, who int)`（PRIMARY KEY を付けない。重複を検査したいので）、シーケンス `sq_a`（`CACHE 1`）、`sq_c`（`CACHE 5`）、`sq_i`（`INCREMENT 3 START 10`）、`sq_s`（`setval` 専用）。セッションは 3 つ。

| 部品 | 内容 | モデルへの記録（`Ok` で戻ったときだけ） |
|---|---|---|
| A | 自動コミットの `SELECT nextval('sq_a')` | 確定 `max(sq_a) = 値` |
| B | `sq_c` と `sq_i` の自動コミット | 同上 |
| C | `INSERT INTO t7(who) VALUES (n)`（serial の DEFAULT） | 確定した行の `id` |
| D | `BEGIN; nextval ×k; COMMIT` | COMMIT が `Ok` なら確定、Panic なら不明（上限だけ更新） |
| E | `BEGIN; nextval; ROLLBACK` | 「任意の受け取り」だけ更新（確定にしない） |
| F | **他セッションの未 flush の `SEQ_LOG` に依存する払い出し**: セッション 1 が `BEGIN; SELECT nextval('sq_a')`（`SEQ_LOG` を書く。コミットしない）→ セッション 2 が自動コミットで `nextval('sq_a')`（`log_cnt` の余りで払い出す）→ セッション 2 の確定の後でクラッシュ（セッション 1 は開いたまま） | セッション 2 の値を確定 |
| G | `SELECT setval('sq_s', <現在の確定値 + 1000>)`（上向きだけ）を自動コミット | 確定の下限 `floor(sq_s)` |
| H | `k` 文ごとに `CHECKPOINT`（FPW と `force_log` の経路） | — |
| I | `CREATE SEQUENCE tmp_n; SELECT nextval('tmp_n'); ROLLBACK`、`CREATE SEQUENCE` と `DROP SEQUENCE` のコミット | カタログの存在だけ |
| J | `ALTER SEQUENCE sq_a RESTART WITH <確定の最大 + 100>`（上向きだけ）を自動コミット | 確定の下限 |

**不変条件 I15**（`invariants.rs` に `check_i15`。リカバリ後に新しいセッションで検査する）:

1. 各シーケンスについて、`SELECT nextval(..)` の結果 `v` が `v > confirmed_max`（昇順。`sq_s` と J の対象は `floor` も）。
2. **欠番の上限**: `v <= max_returned_any + (SEQ_LOG_VALS + cache) * increment`（`max_returned_any` は確定・不明・ロールバックを含めて、どのセッションにも返した最大値。クラッシュまでに誰も払い出していなければ 0）。
3. `t7.id` に重複がない（`SELECT count(*) - count(DISTINCT id) FROM t7` が 0）。確定した行の `id` はすべて存在する。
4. `read(seq)` の `last_value >= confirmed_max` 相当（`is_called = true` のとき）と、`log_cnt` が 0 以上（ページの形の検査）。
5. 存在すべきシーケンスのファイルがあり、DROP 済み・ロールバックされたものはカタログにない（M3 の I8 の延長）。
6. ページの `verify`（チェックサムと形）、`magic == SEQ_MAGIC`、`max_offset() == 1`。

**変異試験**（`mutation.rs`。M3 の表に追加。ハーネスが壊れを検出できること）:

| 変異 | 方法 | 検出されるべき不変条件 |
|---|---|---|
| 暗黙のコミットで flush しない | `skip_commit_flush`（`finish_without_xid` も省く）+ `DropUnsynced`、部品 A のみのワークロード | I15（1 または 4） |
| **PostgreSQL と同じ穴** | `DebugKnobs::seq_ignore_foreign_wal`（`SeqRun.wal_lsn` を「この呼び出しが自分で書いた `SEQ_LOG` の LSN（なければ 0）」にする）+ `DropUnsynced`、部品 F | I15（1） |
| REDO が LSN を見て飛ばす | `DebugKnobs::seq_redo_skip_if_page_newer`（`SEQ_LOG` の REDO に `page_lsn >= end` なら飛ばす判定を足す）+ チェックポイントをまたぐワークロード | I15 か I8 |
| `force_log` を使わない | `DebugKnobs::seq_no_force_log`（`force_log = false` 固定）+ `CHECKPOINT` と `DropUnsynced` | I15（1。重複） |

### 7.5 クラッシュ試験 層 2（`crash_kill9.rs`。J）

M3 の「追記ログ」のクライアントに `serial` 列（`id`）を足し、`COMMIT` の応答を受け取った行の `id` を記録する。再起動後に `id` の重複がなく、記録した `id` がすべて残り、`nextval` が記録した最大より大きいこと（I15。`kill -9` ではページキャッシュが残るので REDO の無条件上書きの検証になる。電源断は層 1）。

---

## 8. 実装の分担と工数（Q1）

`00 §17` の Q1（5 日）。依存: A（型）、C1（カタログのストアと `ddl/depend.rs`）。

| 作業 | 日数 | 依存 |
|---|---|---|
| `storage/sequence.rs`（`SeqStore`、`plan_fetch` と参照実装、ページ・タプル、REDO、`describe`）と単体テスト | 1.5 | A、M3 の C・D・W1（`CriticalSection`、`RecordBuilder`、`Wal`） |
| `catalog/seq_params.rs`（`init_params`、`parse_seq_int`）と単体テスト | 0.5 | なし |
| `executor/seq.rs`、`types/ops.rs`、`Transaction` / `TxnManager::finish_without_xid`、`session.rs` への依頼の整理 | 0.75 | A、S |
| `analyzer/ddl.rs`（SERIAL・IDENTITY、CREATE / ALTER / DROP SEQUENCE の解析、`choose_relation_name`、IDENTITY の規則関数） | 1.0 | P0（解析の足場）、S1（構文） |
| `ddl/sequence.rs`（create / alter / drop / 表との連携） | 0.75 | C1（`CatalogStore`、`depend`）、`storage/sequence.rs` |
| 結合（slt、再起動テスト、psql `\ds`）と R2（crash_sim）への仕様の引き渡し | 0.5 | K、R2 |

- 進め方: `plan_fetch`・`seq_params`・`SeqStore` は C1 を待たずに始められる（`SimVfs` の上で単体テスト）。解析と `ddl` は P0 と C1 の後。K（slt）と R2（ワークロード 7）は PostgreSQL・`SimVfs` に対して先に書ける。
- 他の担当への依頼（§11）は、この章を読んだ担当が自分の範囲に反映する。この章が編集してよいのは §6.4 の「この章」のファイルだけ。

---

## 9. 未検証の点

実機の確認は、`sandbox/pg.sh start` の PostgreSQL 17.11（`127.0.0.1:55432`）と、クラッシュの確認用に別のデータディレクトリ・別ポートで起動して `kill -9` / `pg_ctl -m immediate` で止めたインスタンスで行った。ソース（REL_17_STABLE の `sequence.c`・`parse_utilcmd.c`）は GitHub から取得して読んだ。

- **PostgreSQL の穴（D8-5）と狭い競合（D8-7）**: ソースの読みによる。`kill -9` ではページキャッシュの WAL が残るので、実機で再現できない（電源断が要る）。yuzhu は層 1 の変異試験（`seq_ignore_foreign_wal`）で再現してから塞ぐ。
- **`page_mut_hint()` が呼んだ時点で dirty になること**: 現在のコード（M2 版）はガードの Drop 時に dirty にする。M3 の `page_mut` は呼んだ時点（D11）。`page_mut_hint` も同じにする依頼（§11）が通らないと D8-7 の順序が成り立たない。C の実装を確かめる。
- **`Wal::redo_lsn()` が起動直後に制御ファイルの REDO 点を返すこと**: M3 §6.3.4 の FPW と同じ要件。0 を返す実装だと、再起動後の最初の `nextval` が `SEQ_LOG` を書かない。単体テスト（§7.3）で確かめる。
- **`Page::init_special` の名前と形**: `06-btree.md` が決める。
- **`FnKind::Runtime` の strict の扱い**: NULL 引数で呼ばれないこと（`eval` の共通の処理）を確かめる。現在のコードで `pg_backend_pid` が `Runtime`（引数なし）なので、引数つきの `Runtime` は初めて。
- **DROP の DETAIL が 100 行を超えるとき**の正確な文言（`and N other objects (see server log for list)`。ソースの記憶）。
- **`DROP SEQUENCE` の DETAIL の並びが OID 昇順であること**: 実機の 1 例（3 個）で確認しただけ。PostgreSQL は `pg_depend` の走査順で出す。
- **`CREATE SEQUENCE ... RESTART n` の `n`** が範囲外のときの検査の順序（`init_params` の写しで一致するはず）。
- **`ALTER SEQUENCE` を `OWNED BY` だけで呼んだとき**に PostgreSQL が `pg_sequence` を更新するだけで状態を触らないこと（ソースの読み。実機では `log_cnt` が変わらないことを未確認）。
- **IDENTITY のオプションの `OWNED BY` / `LOGGED`**: PostgreSQL は受け付ける（`owned by none` は成功を確認）。yuzhu は 0A000。
- `VACUUM s` / `ANALYZE s` の WARNING の文言（実機で確認したが、yuzhu の VACUUM は何もしないので出さない見込み。07 が決める）。
- psql 17 の `\ds` / `\d シーケンス` の SQL は実機で取得した（§5.9）が、yuzhu での実行は未確認。
- 電源断（fsync 済みのデータだけが残る）の下での挙動は層 1 の試験の結果による。

---

## 10. 確認事項

ユーザーの不在中に仮決めしたことです。ディスク形式に関わるもの（★）は、実装の前に決めるのが望ましいです。ID は `[08-Q番号]`。`11-tests-plan.md` が `M4-Q` の通し番号に振り直します。

- **★[08-Q1] シーケンスのページとタプルの形**
  - 仮決め: PostgreSQL と同じ構造。special 8 バイト（`SEQ_MAGIC = 0x1717`）、タプル 1 個（ヘッダ 35 バイト + `t_hoff` 40 + 17 バイト = 57 バイト）、`xmin = Xid::FROZEN`、`xmax` 無効、`t_ctid = (0,1)`。`pd_special = 8184`。
  - 理由: `SELECT * FROM シーケンス` が通常のヒープ走査で動き、`xmin = 2`・`ctid = (0,1)` も PostgreSQL と同じになる。M2 のページ・タプルの検査を変えずに済む。
  - 変えたい場合の影響: 形を変えると `storage/sequence.rs` と REDO の形式が変わり、`SELECT * FROM シーケンス` のために別の実行ノードが要る（+1 日）。M5 以降はディスクの互換が崩れる。
- **★[08-Q2] `SEQ_LOG` の形**
  - 仮決め: ブロック 1 個、`WILL_INIT`、差分データ = ログに書く状態のタプル全体（57 バイト）、メインデータなし、`xid` は `init` と `reset` だけ呼び出し側の XID、`fetch` と `setval` は 0。REDO は無条件に上書き。
  - 理由: PostgreSQL と同じ。REDO が単純で、FPW の対象外。M3 の `wal::dump` と `max_xid` の扱いに影響しない（0 は無視される）。
  - 変えたい場合の影響: 差分レコードにすると REDO が LSN の順序に依存し、torn ページの扱いに FPW が要る（+1 日、M3 の FPW の経路に触る）。
- **★[08-Q3] SERIAL の DEFAULT の保存形式**
  - 仮決め: `pg_attrdef.adbin` に `nextval('<oid>'::regclass)` のテキスト。`pg_get_expr` が名前に戻す（D-21）。利用者が書いた DEFAULT は M2 のとおり書いたままのテキスト。
  - 理由: シーケンスの名前の変更・検索パスの影響を受けない。PostgreSQL は regclass 定数を OID で持つ。
  - 変えたい場合の影響: すべての DEFAULT の保存形式を「regclass 定数だけ OID に直した正規形」にすると、`deparse` に OID 形式の出力モードが要り、利用者のテキストの書き換えが入る（+0.5 日、`10-explain-copy-compat.md` と 07 に波及）。
- **[08-Q4] ROLLBACK では flush しない**
  - 仮決め: コミットだけ flush する（PostgreSQL と同じ。ただし覆う範囲を D8-5 のとおり広げる）。中断した（確認を受け取った）トランザクションが見た値は、クラッシュ後に再び払い出されうる。
  - 理由: 中断した値は外から見えない前提。PostgreSQL と同じ保証。
  - 変えたい場合の影響: 中断でも `finish_without_xid` 相当で flush する（工数ほぼ 0、中断の経路が I/O と Panic の可能性を持つ）。不変条件 I15 が「COMMIT と ROLLBACK のどちらでも」に強まる。
- **[08-Q5] `ALTER SEQUENCE` はその場で書き換える（状態はロールバックされない）**
  - 仮決め: `pg_sequence` の行は MVCC でロールバックされるが、シーケンスの状態（`RESTART` の結果など）は戻らない。他のセッションには即座に見える。`log_cnt` は常に 0。`TRUNCATE ... RESTART IDENTITY` も同じ。
  - 理由: 00 の `SequenceStore::reset`（その場）と D-9。PostgreSQL の方式（新しい relfilenode、表ロックで `nextval` を待たせる）は M5 のテーブルロックなしでは払い出しが失われうる。
  - 変えたい場合の影響: 07 の「新しい relfilenode への付け替え」（TRUNCATE と同じ部品）を使って新しいファイルに状態を書く方式にできる（+2 日）。PostgreSQL と同じくロールバックで戻るが、`ALTER` の最中の他セッションの `nextval` が古いファイルに書き、コミット後に失われる（重複）。M5 の表ロックまでは `nextval` との競合の制限が要る。
- **[08-Q6] `TEMPORARY` / `UNLOGGED` シーケンス、`ALTER SEQUENCE RENAME` / `SET SCHEMA` は `0A000`**
  - 仮決め: 実装しない。`ALTER SEQUENCE OWNER TO` はロールの存在を確かめて何もしない。
  - 理由: 一時テーブルとログなしリレーションが M4 にない。D-12 の `ALTER TABLE` の範囲と合わせる。pg_dump の出力の復元は目標でない。
  - 変えたい場合の影響: RENAME は `pg_class` の行の更新だけで +0.25 日（SERIAL の DEFAULT は OID なので影響なし）。TEMP / UNLOGGED は M5 以降。
- **[08-Q7] 他のセッションの先取りを全部捨てる（`reset_generation`）**
  - 仮決め: `ALTER` / `RESTART IDENTITY` のたびに全セッションのすべてのシーケンスの先取りが捨てられる（欠番が増えるだけ）。
  - 理由: その場の書き換えでは PostgreSQL の「relfilenode の変化」に当たる手がかりがない。
  - 変えたい場合の影響: シーケンスごとの世代（`HashMap<Oid, u64>`）にできる（+0.25 日）。
- **[08-Q8] 払い出した値を覆う WAL の LSN をページの LSN で決める（PostgreSQL の穴を塞ぐ）**
  - 仮決め: `SeqRun.wal_lsn` は呼び出し後のページの LSN。D8-7 の順序も PostgreSQL と違う。どちらも観測できる SQL の挙動は変えず、クラッシュ後の重複だけを防ぐ。
  - 理由: PostgreSQL の方式だと、他のトランザクションの未 flush の `SEQ_LOG` に依存した払い出しがクラッシュで重複しうる。費用はほぼ 0。
  - 変えたい場合の影響: `wal_lsn` を「自分が書いた分」にすると PostgreSQL と同じになる（層 1 の変異試験が検出する穴を作る）。
- **[08-Q9] ユーザーが書いた DEFAULT の `regclass` は書いたままのテキスト（遅延束縛）**
  - 仮決め: `DEFAULT nextval('s')` は `nextval('s')` のまま保存し、使うたびに名前を解決する。依存（`pg_attrdef` → シーケンス）は作る時点の解析結果で記録する。
  - 理由: M2 の「DEFAULT は SQL テキスト」（Q-006）を変えない。
  - 変えたい場合の影響: 検索パスを変えると別のシーケンスを引きうる（PostgreSQL は OID で固定）。[08-Q3] の正規形で直る。
- **[08-Q10] 同じ `CREATE TABLE` が作るシーケンスの名前を DEFAULT に書くと `42P01`**
  - 仮決め: 解析時に DEFAULT を解析するので、同じ文で作るシーケンスは見えない（PostgreSQL は成功する）。
  - 理由: PostgreSQL の「シーケンスを作ってから DEFAULT を解析する」順序を再現するには、DEFAULT の解析を `ddl` の途中に移す必要がある。実用上の価値が低い。
  - 変えたい場合の影響: `ddl/table.rs` で DEFAULT の解析をシーケンスの作成後にする（+0.5 日、07 の構造に影響）。
- **[08-Q11] `pg_get_serial_sequence` は任意（M4 後半）**
  - 仮決め: M4 の完了条件に含めない。作るなら §4.5 の仕様。
  - 理由: ORM 向け。psql・pgbench は使わない。
  - 変えたい場合の影響: 必須にすると +0.25 日と `CatalogReader::sequence_owned_by_column`。
- **[08-Q12] IDENTITY の `SEQUENCE NAME` は採用、`OWNED BY` / `LOGGED` は `0A000`**
  - 仮決め: 上のとおり。
  - 理由: `SEQUENCE NAME` は PostgreSQL の `pg_dump` の出力が使う。ほかは意味がない。
  - 変えたい場合の影響: 小さい。
- **[08-Q13] 同じ文の 2 つの暗黙のシーケンスの名前の衝突を避ける**
  - 仮決め: PostgreSQL（気づかず 42P07）の上位互換。
  - 理由: 長い列名のときだけの差。
  - 変えたい場合の影響: `taken` を空にすれば PostgreSQL と同じ。
- **[08-Q14] `ALTER SEQUENCE` のあとの `log_cnt` は常に 0**
  - 仮決め: PostgreSQL は `START` だけを変えたとき `log_cnt` を保つが、yuzhu は 0。
  - 理由: PostgreSQL の方式は、その後のクラッシュで払い出した値が戻りうる（§2.2）。
  - 変えたい場合の影響: `SELECT * FROM s` の `log_cnt` が一致する代わりに、その穴を持つ。

---

## 11. 00 への変更提案

統合時に `00-contracts.md` に反映する。署名と名前は変えず、足すだけ。

1. **`§13.3`**: `SequenceHandle` に `name: String` を足す。`SeqState` に `PartialEq, Eq` を足す。`SeqRun.wal_lsn` の意味を「その呼び出しの後のページの LSN（払い出した値を覆う `SEQ_LOG` の終端）」と明記する。`SequenceStore` に `fn reset_generation(&self) -> u64;` を足す。`init` の初期状態は `(params.start, 0, false)`、`CREATE SEQUENCE ... RESTART n` は `init` の後の `reset`。`reset` は「RESTART なしなら現在の状態を保ち `log_cnt` を 0 にする」「現在の値を `new_params` の `min` / `max` で再検査する」と意味を決める（署名は変えない）。
2. **`§14.1`**: `Transaction` に `note_wal(&mut self, lsn: Lsn)` を足す。`started_at` の意味（PostgreSQL のエポックからのマイクロ秒、BEGIN または暗黙のトランザクションの最初の文の開始時に session が設定）。`TxnManager::finish_without_xid` は XID なしの **COMMIT** のときだけ flush する。中断は flush しない（`[08-Q4]`）。
3. **`§4`（ファイル構成）**: `catalog/seq_params.rs`（純粋関数。`catalog::{mod, builtin, opclass, schema}` と同じ層）と `executor/seq.rs` を足す。`analyzer/ddl.rs` に `choose_relation_name`・IDENTITY の規則関数を置く。
4. **`§7`**: `BoundCreateSequence`・`BoundAlterSequence`・`BoundDropSequence`・`SeqOwner`・`OwnedByTarget` のフィールドは §4.8 のとおり。AST（`CreateSequence` など）は §4.8。`BoundCreateTable.sequences[i].owner` は `SeqOwner::NewTableColumn`。
5. **`§11.1`**: `TableDef.sequence` の `owned_by` は `pg_depend`（`a` / `i`）から。`CatalogReader` に `fn sequence_owned_by_column(&self, table: Oid, attnum: i16) -> Result<Option<Oid>> { Ok(None) }`（任意の `pg_get_serial_sequence` 用）。
6. **`§11.5`**: `pg_sequence` の列（§3.8）。`pg_depend` の依存に「`DEFAULT` 式の中の `regclass` 定数 → `pg_attrdef` の行からそのリレーションへ `n`」を足す（`§11.6` の「列のデフォルト → シーケンス」の一般化）。
7. **`§13.4` / `§15.1`**: `SEQ_LOG` の形式（§3.5）。定数 `SEQ_SPECIAL_SIZE = 8`、`SEQ_TUPLE_LEN = 57`、`SEQ_TUPLE_HOFF = 40`。
8. **`§12.1` と関数の行**: `nextval` など 5 つの関数（OID 1574・1575・1576・1765・2559）は `volatile`、`FnKind::Runtime`。`pg_cast` に `text → regclass`（10109）、`varchar → regclass`（10110）、`oid ↔ regclass`、`int4 → regclass`（`regclass` の暗黙キャスト）。`regclass` の文字列リテラルは解析時に評価して `Literal` にする。
9. **`§17` の M3 の持ち主への要求**:
   - `Page::init_special(special_size)`（06）。
   - `PageWriteGuard::page_mut_hint()` が呼んだ時点で dirty にする（M3 の `page_mut` と同じ。D11）。
   - `Wal::redo_lsn()` が `open_at` / `finish_recovery` の直後から制御ファイルの `redo_lsn` を返す。
   - `recovery::dispatch` に `RmgrId::Seq`、`wal/dump.rs` に `describe` の呼び出し。
   - `DebugKnobs` に `seq_ignore_foreign_wal`・`seq_redo_skip_if_page_newer`・`seq_no_force_log`（いずれも `bool`、既定は無効。§7.4 の変異試験）。
   - `Session`: 文の終わりに `SeqSession::end_statement()` を `Transaction::note_wal` に反映、`commit_transaction` の XID なしの経路で `finish_without_xid`、`seq_state` の保持、`started_at` の設定。
10. **`§17` の C1（07）への要求**: §4.8 の `CatalogStore` のメソッド（`create_sequence`・`update_sequence_params`・`drop_sequence`・`drop_default`）、`ddl/depend.rs` の API（`record`・`dependents_of`・`dependencies_of`・`delete_dependencies`・`describe`）、CREATE TABLE の手順への差し込み（§5.7 の A〜E）、DROP TABLE から `drop_owned_by_table`、TRUNCATE から `restart_owned_by_table`、シーケンスに対する DROP TABLE / DROP INDEX / CREATE INDEX / TRUNCATE / ALTER TABLE のエラー（§5.9）、`DEFAULT` 式の `regclass` 定数への依存の記録（§5.4）。
11. **`§18`（テストの置き場所）**: `tests/slt/m4/seq/` の下のファイルは §7.1。再起動テストは `tests/restart/m4/seq-*`（§7.2）。
