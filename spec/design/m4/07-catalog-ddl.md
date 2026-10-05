# yuzhu M4 設計 07: カタログと DDL

M4 設計書の第 07 章（担当 C1）です。`00-contracts.md` の契約（特に §11 カタログ、§13.2 `IndexStore`、§14.6 `DdlCtx`）に従い、**インデックスと制約のためのカタログの追加、`ddl/` モジュール、`TableDef` / `IndexDef` の読み込み、initdb の変更**を、実装者がこの章だけ読めば書ける粒度で定めます。

- 前提（必読）: `00-contracts.md`、`spec/design/m2.md`（§4.6、§5.4、§6.8）、`spec/design/m3.md`（§5.3、§5.4）、`QUESTIONS.md`、`PROGRESS.md`
- 調査（根拠）: `spec/research/m4-btree.md`（§7、§8）、`pg-compat-tools.md`（§2.2、§2.3、§3.1、§3.2）、`m2-catalog.md`
- 正解の基準は PostgreSQL 17。**この章の PostgreSQL の挙動の記述は、sandbox の PostgreSQL 17.11（`sandbox/pg.sh start`、`127.0.0.1:55432`）で実測したもの**です。実測できなかったものは「（未検証）」と付けました。PostgreSQL のソースは REL_17_STABLE を `PG:<path>` と略します。
- 他の章への参照は章のファイル名と概念名で書きます（`06-btree.md` の一意性検査、`08-sequence-serial.md` の `SequenceStore` など）。この章の書いている時点で 01〜06、08〜11 は並行して書かれており、読めませんでした。他章に期待していること（依頼）は、この章の末尾の「00 への変更提案」と「他章への依頼」にまとめました。

---

## 1. 範囲

### 1.1 この章が決めること

| 分類 | 内容 |
|---|---|
| カタログ | 追加する 9 個のカタログ（`pg_index`、`pg_depend`、`pg_sequence`、`pg_language`、`pg_opfamily`、`pg_opclass`、`pg_amop`、`pg_amproc`、`pg_description`）の全列と、既存カタログ（`pg_class`、`pg_attribute`、`pg_constraint`）に入る新しい行の値、`pg_depend` に記録する依存の一覧、静的な表から生成する行、`CATALOG_VERSION_NO` と `builtin_hash` |
| 読み込み | `load_table_def` が `TableDef.indexes` / `sequence` / `identity_seqs` を組み立てる手順、キャッシュ、`CatalogReader` の追加メソッドの実装、`CatalogStore` の追加メソッド |
| DDL | `CREATE TABLE`（PRIMARY KEY / UNIQUE / `WITH (fillfactor)` の組み込み）、`CREATE [UNIQUE] INDEX`、`DROP INDEX`、`ALTER TABLE ... ADD [CONSTRAINT n] PRIMARY KEY / UNIQUE`、`ALTER TABLE ... OWNER TO`、`TRUNCATE`、`VACUUM` / `ANALYZE`、`DROP TABLE`（依存の連鎖と `CASCADE` / `RESTRICT`） |
| 名前 | `ChooseRelationName` / `ChooseConstraintName` / `ChooseIndexName` の移植（実機で衝突規則を確認） |
| psql | `\dt` `\di` `\dn` `\ds` が必要とするカタログの値、`pg_table_is_visible` の対象 |
| テスト | `tests/slt/m4/{catalog,constraint,index}/`、再起動テスト、ファイルの後始末、カタログの整合検査 |

### 1.2 この章が決めないこと（持ち主）

- シーケンスそのもの（`SequenceStore`、`nextval` など）、`CREATE / ALTER / DROP SEQUENCE`、SERIAL / IDENTITY の書き換え: `08-sequence-serial.md`。ただし**シーケンスのカタログ行の書き方**（`pg_class` の relkind `S`、`pg_attribute`、`pg_sequence`、`pg_depend`）と DROP の依存の判定は、この章が `CatalogStore` / `catalog::depend` として提供し、08 が呼ぶ（§4.3、§4.7）。
- B+Tree のページ形式、`IndexStore` の実装、一意性検査（挿入時）、演算子クラスの静的な表（`catalog/opclass.rs`）、木の検査器: `06-btree.md`。この章はそれらを**呼ぶ**だけです。
- 構文（AST）と問い合わせのアナライザ: `03-parser-analyzer.md`、S1。ただし PRIMARY KEY / UNIQUE 制約の**解析**（名前の決め方を除く列の解決、重複の統合、NOT NULL の付与）はこの章が仕様を定め、`analyzer/ddl_constraint.rs` を C1 が書く（§6.9）。
- 式の逆変換（`pg_get_indexdef`、`pg_get_constraintdef`、`pg_get_expr`）: `10-explain-copy-compat.md`（E1）。この章は読み取りの API（`constraint_by_oid` など）を用意します。
- 新しい型・演算子・関数の行（numeric など）: `09-types-functions.md`。この章が 09 に要求するのは、opclass の参照を閉じるための行だけです（§3.8）。

### 1.3 M4 のこの部分が保証すること・しないこと

**保証する**: DDL は完全にトランザクショナル（ROLLBACK でカタログの行・ファイルがすべて消える。コミット後のクラッシュでも WAL の REDO で回復する）。PRIMARY KEY / UNIQUE の自動名、エラーの SQLSTATE と文言、`pg_index` / `pg_constraint` / `pg_depend` の値が PostgreSQL と一致する（OID と `relpages` / `reltuples` を除く）。

**保証しない**: カタログのインデックス、`ALTER TABLE` の上記以外（`DROP CONSTRAINT`、`ADD COLUMN` など）、式インデックス・部分インデックス・`INCLUDE`・`NULLS NOT DISTINCT`・`DEFERRABLE`、`CREATE INDEX CONCURRENTLY` の本来の意味（並行書き込みを許す構築。通常の `CREATE INDEX` と同じに動く）、VACUUM による `relhasindex` の更新、クラッシュで中断した DDL が作ったファイルの掃除（M3 の D15 と同じ）。

---

## 2. 決定（この章で追加で決めたこと）

`D07-n` はこの章の決定番号です（`00` の `D-n` とは別）。

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| D07-1 | DDL の実行の置き場所 | `session.rs` に書き足す（M2 の `exec_create_table`）／`ddl/` に移す（00 の D-29） | **`ddl/`**。session は `DdlCtx` を組み立てて `ddl::execute` を呼ぶだけ | session が DDL ごとに肥大するのを避ける。ライターロック・バリア・スナップショットは session の責任のまま（§5.0） |
| D07-2 | DROP の対象の求め方 | M2 の「`conrelid` / `adrelid` / `attrelid` で従属する行を直接探す」を拡張／`pg_depend` を真実の源にした閉包 | **`pg_depend` の閉包**（`catalog::depend::plan_drop`）。表自身の部品も `pg_depend` の行（`a` / `i`）で辿る（取りこぼし防止に、表の制約・既定値・索引は直接のキーでも拾う二重の網にする） | シーケンス・インデックス・制約・既定値・M5 の外部キーが同じ仕組みで消える。`2BP01` の DETAIL（「X depends on Y」）と `CASCADE` の NOTICE が同じ走査から出る。**取りこぼしの検出は、DROP 後のカタログ整合検査（§8.3）で行う** |
| D07-3 | 同じコマンド内のカタログ行の更新 | PostgreSQL のように `CommandCounterIncrement` を挟んで新しい行を更新する／**1 コマンドで同じ行を 2 度更新しない**。作る行は最初から最終形で書く | **後者** | yuzhu の `cid` は文の終わりでしか進まない。同じコマンドで挿入した行は自分のスナップショットから見えず、`update` が `Invisible` になる。例: `CREATE TABLE` の `relhasindex` は、`pg_class` の行を最初から `true` で挿入する。更新が要るのは、**前のコマンド**で作られた行（`CREATE INDEX` が既存の表の `relhasindex` を上げる、`TRUNCATE` が `relfilenode` を変える、など）だけ |
| D07-4 | 索引・制約の自動名を決める時機 | 解析時／実行時（PostgreSQL の `DefineIndex`） | **実行時**。アナライザは明示された名前だけを `Bound` に載せる（`BoundIndexConstraint.name: Option<String>`。00 への変更提案 1） | 名前の衝突判定は、同じ文で先に決めた名前と、カタログの現在の内容の両方を見る必要がある。実行時なら、DDL が自分のスナップショットで一度に判定できる。`make_object_name` などは analyzer と ddl が共有するので `catalog/naming.rs` に置く（依存の向き: `analyzer` は `ddl` を `use` できない） |
| D07-5 | `CREATE TABLE` 内の PRIMARY KEY / UNIQUE の重複 | そのまま全部作る／PostgreSQL の `transformIndexConstraints` と同じく統合する | **統合する**（実測: `unique(a), unique(a)` も、`primary key (a), unique (a)` も索引は 1 つ。§5.10.3） | pg_dump の出力や ORM が生成する DDL と互換にする |
| D07-6 | 索引の構築の順序 | カタログ行を先に書いて構築／**構築を先**にしてカタログ行を最後に書く | **構築が先** | `relpages` / `reltuples` に構築の結果（`BuildStats`）を 1 回の挿入で書ける（D07-3 により行の更新を避けられる）。失敗（`23505` など）してもトランザクションごと消える |
| D07-7 | 一意索引の構築での重複検出 | `IndexStore::build(.., BuildUnique::Yes)` に任せる（00 の記述）／**C1 が生きている版だけを調べ、`BuildUnique::No` で呼ぶ** | **後者** | `build` の入力 `(key, tid)` は「死んだ版（`DeadCommitted` / 自分が削除済み）」も含む（PostgreSQL の RECENTLY_DEAD。古いスナップショットが見る版を索引から引けるように）。隣接比較だけで検出する `Yes` では、UPDATE した行の旧版と新版を重複と誤判定する。PostgreSQL の `btspool2` と同じ役割を C1 が持つ（§5.2.1 の手順 4）。`BuildUnique::Yes` は C1 は使わない（06 のテスト用） |
| D07-8 | `CREATE INDEX CONCURRENTLY` / `DROP INDEX CONCURRENTLY` | 通常と同じに動かす／`0A000` | **通常と同じに動かす**。トランザクションブロックの中なら `25001`（実測の文言どおり） | 単一ライター（M2 D3）では並行して書く者がいない。ORM のマイグレーションが `CONCURRENTLY` を付けても動く（m4-btree §12 の 6） |
| D07-9 | `relhasindex` | `DROP INDEX` で最後の索引を消したら下ろす／PostgreSQL と同じく**下ろさない**（VACUUM が下ろす） | **下ろさない**。M4 の VACUUM は何もしないので、一度立てたら立ったまま | 実測: `DROP INDEX` の後も `t`、`VACUUM` の後に `f`。差が出るのは VACUUM の後だけで、psql は索引の一覧が空なら何も出さない |
| D07-10 | `ALTER TABLE ADD PRIMARY KEY` のエラーの順序 | NOT NULL 検査 → 重複／**重複（`23505`）→ NOT NULL（`23502`）** | **後者**（実測: NULL と重複が両方ある表では `could not create unique index` が先） | PostgreSQL は索引の構築（フェーズ 2）の後で、表の走査による NOT NULL の検証（フェーズ 3）を行う |
| D07-11 | `ALTER TABLE ... OWNER TO` | 00 の D-12「何もしない」／**ロールを検証し、`relowner` を表・索引・所有シーケンスに反映する** | **後者**（00 への変更提案 4） | 何もしないと、別のロールを指定しても `\dt` の Owner が変わらず誤った結果になる。実測: 所有者の変更は表の索引にも及ぶ。実装は `pg_class` の行の更新だけで小さい。ロールの存在は共有カタログで検査する（`42704 role "x" does not exist`） |
| D07-12 | `TRUNCATE` | 行を全部 DELETE する／新しい relfilenode を作る（00 の D-11）／**後者**。索引は新しいファイルに `init_index` で作り直す | **新 relfilenode** | ロールバックできる（旧ファイルはコミットまで残る）。大きな表でも速い。`CASCADE` は外部キーがない M4 では意味を持たないので受け付けて無視する |
| D07-13 | `VACUUM` / `ANALYZE` | どちらもトランザクションブロック内で `25001`（00 の D-11）／**`VACUUM` だけ `25001`、`ANALYZE` はブロック内で成功** | **後者**（00 への変更提案 3） | 実測: `BEGIN; ANALYZE; COMMIT` は成功、`BEGIN; VACUUM; ROLLBACK` は `25001 VACUUM cannot run inside a transaction block`。複数文の Simple Query（暗黙のブロック）の中の VACUUM も `25001` |
| D07-14 | `WITH (fillfactor = N)` | 保存する（`reloptions`）／**検証して捨てる**（00 の D-30） | **検証して捨てる**。表・索引とも範囲は 10〜100。値の形式と未知のパラメータのエラーは PostgreSQL に合わせる（§5.9） | `reloptions` を保存する価値は `\d+` と pg_dump だけ。B+Tree の充填率は定数（`BT_FILLFACTOR_LEAF`）のまま |
| D07-15 | 追加するカタログの行型 | 行型（`pg_type` の複合型）を作る／作らない | **作らない**（`reltype = 0`） | M2 の「ほかのカタログは 0」と同じ。実測では PostgreSQL は新しいカタログにも `reltype` を持つが（`pg_index` は 10007）、yuzhu の SQL から困らない |
| D07-16 | `pg_depend` に書く依存の範囲 | PostgreSQL と同じ全部／**この章 §3.6 の表だけ** | **表だけ**。名前空間への依存（`pg_class` → `pg_namespace` の `n`）、CHECK の式が参照する列への依存、`pinned`（`p`）は書かない | M4 に `DROP SCHEMA` がなく、組み込みオブジェクトは削除の対象にならない。CHECK は表への依存（`a`）だけにする（列ごとの依存は式の走査が要る。M5） |
| D07-17 | 索引の列が `name` 型のとき | PostgreSQL と同じく `pg_attribute.atttypid = cstring`（`opckeytype`）／`name` のまま | **`name` のまま** | `cstring` の型の扱いを B+Tree に持ち込まない。`opckeytype` は常に 0 |
| D07-18 | `CatalogReader::table()` が索引の名前に出会ったとき | 索引の `TableDef` を返す／`None`（00 の §11.1）／`42809` | **`Ok(None)`**（呼び出し側が `relation_kind` で区別する）。アナライザは `42809 cannot open relation "x"` DETAIL `This operation is not supported for indexes.`（実測）を返すのが望ましい（N1 への依頼） | `table()` の意味を「SELECT できるリレーション」に保つ |
| D07-19 | 索引の列に使える型 | 全部／`default_opclass` がある型と、バイナリ互換でその opclass が使える型 | **後者**: 表（00 §11.3 の opfamily）にある型に加えて、**`varchar` → `text_ops`、`regclass` / `regtype` / `regproc` → `oid_ops`**（実測: PostgreSQL では `varchar` の索引は `indclass = 3126`（text_ops）、`regclass` は `1981`（oid_ops））。それ以外（`xid`、`cid`、`int2vector`、`"char"`、`tid`）は `42704`（実測の文言）。**差分**: PostgreSQL は `"char"` / `tid` / `int2vector`（配列の opclass）にも索引を作れる | 配列と `"char"` の opclass は M4 の範囲外（00 §11.3） |
| D07-20 | `DROP TABLE` の `CASCADE` / `RESTRICT` | 既定のみ（RESTRICT）／構文と `CASCADE` の連鎖を実装 | **実装する**。M4 で連鎖が起きるのは「他の表の既定値が、この表の所有シーケンスを使っている」場合だけ（実測。§5.8） | `DROP TABLE ... CASCADE` を付ける ORM と pg_dump のリストア（`DROP TABLE IF EXISTS ... CASCADE`）で必要 |
| D07-21 | `ALTER TABLE ... ADD ... USING INDEX`、`DROP CONSTRAINT` | 実装する／`0A000` | **`0A000`**（00 の D-12）。ただし `DROP CONSTRAINT` は**実装が小さい**ので確認事項に挙げる（[07-Q10]） | PRIMARY KEY の索引は `DROP TABLE` でしか消せない状態になる |

### 2.1 調査・00 との食い違いと、この章での決定

| 食い違い | 決定 |
|---|---|
| `m4-btree.md` §7.1 は `CONCURRENTLY` を `0A000` にする（§12 の 6 で通常扱いも案に挙げる）。00 §3 は「通常と同じ扱い」 | 00 に従い**通常と同じ**（D07-8）。ブロック内の `25001` は実測の文言 |
| `pg-compat-tools.md` §5 は `WITH (fillfactor=N)` を「`reloptions` に保存」、00 の D-30 は「検証して捨てる」 | **検証して捨てる**（D07-14）。範囲外・未知の名前の `22023` は調査どおり |
| `m4-btree.md` §7.1 の「RECENTLY_DEAD も索引に入れる。一意性検査の対象外は別のリスト（`btspool2`）」と、00 §13.2 の `build(.., BuildUnique)`（隣接比較で検出） | **C1 が生きている版だけを調べ、`build` には `BuildUnique::No`**（D07-7） |
| `pg-compat-tools.md` §5 と 00 の D-11 は「VACUUM / ANALYZE はブロック内で `25001`」。実測は `VACUUM` だけ | **`VACUUM` だけ `25001`**（D07-13） |
| 00 の D-12 は `OWNER TO` を「何もしない」 | **ロールを検証して `relowner` を更新**（D07-11） |
| `m2.md` §6.8.3 は `pg_constraint.conkey` を「NULL（`int2[]` が NULL 専用のため）」 | M4 は `int2[]` の最小実装（00 §12.1）が入るので、**PRIMARY KEY / UNIQUE の `conkey` を値で埋める**。CHECK の `conkey` は NULL のまま（式の走査が要るため。M5） |
| 00 §11.6 の依存の表は列を略している。実測の PostgreSQL は名前空間への依存、`pg_attrdef` → 表の列、CHECK → 列などを持つ | **§3.6 の表を正とする**（D07-16）。書かない行は `DROP` の挙動に影響しない |
| `m4-btree.md` §7.1 は一意索引の構築のメモリ上限を確認事項にしている | 00 の D-19 に従い**上限なし**（CREATE INDEX のソートは `yuzhu.query_mem_limit` の対象外） |

---

## 3. ディスク上の形式（カタログの行）

カタログはヒープのテーブルです（M2 §6.8）。ここではカタログの**行の内容**を定めます。ヒープのタプルの形式は M2 §3.5〜§3.6 と 00 §12.3 のとおりです。**型と順序は PostgreSQL 17 のヘッダに合わせ、実機（`pg_attribute`）で確認しました。**

### 3.1 追加する 9 個のカタログの全列

すべて DB ごとのカタログで、共有カタログではありません（`shared = false`）。`mapped = false`（`pg_class.relfilenode = oid`）、`rowtype_oid = 0`（D07-15）です。`catalog/schema.rs` の `oids` と `CATALOGS` に足します。`CATALOGS` の並びは「共有カタログが最後」を保ち、既存の 10 個（`pg_class` … `pg_constraint`）の次に下の順で挿入します（共有の 3 個は末尾のまま）。

```rust
// catalog/schema.rs の oids に追加
pub const PG_INDEX: Oid = 2610;      pub const PG_DEPEND: Oid = 2608;     pub const PG_SEQUENCE: Oid = 2224;
pub const PG_LANGUAGE: Oid = 2612;   pub const PG_OPFAMILY: Oid = 2753;   pub const PG_OPCLASS: Oid = 2616;
pub const PG_AMOP: Oid = 2602;       pub const PG_AMPROC: Oid = 2603;     pub const PG_DESCRIPTION: Oid = 2609;
pub const LANGUAGE_INTERNAL: Oid = 12;  pub const LANGUAGE_C: Oid = 13;  pub const LANGUAGE_SQL: Oid = 14;
// INTERNAL_LANGUAGE は LANGUAGE_INTERNAL に改名する（pg_language が実在するので、参照の閉包の例外ではなくなる）
```

型の列の OID: `oid` 26、`int2` 21、`int4` 23、`int8` 20、`bool` 16、`char`（`"char"`）18、`name` 19、`text` 25、`regproc` 24、`int2vector` 22、`oidvector` 30、`pg_node_tree` 194、`aclitem[]` 1034。「NN」は NOT NULL。

**pg_index（2610、21 列）** — 索引 1 つにつき 1 行。

| # | 列 | 型 | NN | 備考 |
|---|---|---|---|---|
| 1 | `indexrelid` | oid | ○ | 索引の `pg_class.oid` |
| 2 | `indrelid` | oid | ○ | 表の `pg_class.oid` |
| 3 | `indnatts` | int2 | ○ | 列数（INCLUDE がないので `indnkeyatts` と同じ） |
| 4 | `indnkeyatts` | int2 | ○ | |
| 5 | `indisunique` | bool | ○ | |
| 6 | `indnullsnotdistinct` | bool | ○ | M4 は常に `f` |
| 7 | `indisprimary` | bool | ○ | |
| 8 | `indisexclusion` | bool | ○ | 常に `f` |
| 9 | `indimmediate` | bool | ○ | 常に `t`（DEFERRABLE がない） |
| 10 | `indisclustered` | bool | ○ | 常に `f` |
| 11 | `indisvalid` | bool | ○ | 常に `t` |
| 12 | `indcheckxmin` | bool | ○ | 常に `f` |
| 13 | `indisready` | bool | ○ | 常に `t` |
| 14 | `indislive` | bool | ○ | 常に `t` |
| 15 | `indisreplident` | bool | ○ | 常に `f` |
| 16 | `indkey` | int2vector | ○ | 表の attnum の並び（式インデックスの 0 はない） |
| 17 | `indcollation` | oidvector | ○ | 列ごと。`typcollation`（text・varchar・bpchar は 100、name は 950、それ以外は 0） |
| 18 | `indclass` | oidvector | ○ | 列ごとの opclass の OID |
| 19 | `indoption` | int2vector | ○ | 列ごと。bit0 = DESC、bit1 = NULLS FIRST |
| 20 | `indexprs` | pg_node_tree | | 常に NULL（`attcollation` 950） |
| 21 | `indpred` | pg_node_tree | | 常に NULL（`attcollation` 950） |

**pg_depend（2608、7 列）** — 全列 NOT NULL。

| # | 列 | 型 | 備考 |
|---|---|---|---|
| 1 | `classid` | oid | 依存元のカタログの OID（`pg_class` 1259、`pg_constraint` 2606、`pg_attrdef` 2604） |
| 2 | `objid` | oid | 依存元のオブジェクトの OID |
| 3 | `objsubid` | int4 | 依存元の列番号（0 = オブジェクト全体）。M4 の依存元は常に 0 |
| 4 | `refclassid` | oid | 依存先のカタログの OID |
| 5 | `refobjid` | oid | |
| 6 | `refobjsubid` | int4 | 依存先の列番号（0 = 全体、1 以上 = その列） |
| 7 | `deptype` | char | `n`（NORMAL）、`a`（AUTO）、`i`（INTERNAL） |

**pg_sequence（2224、8 列）** — 全列 NOT NULL。書くのは 08（`CatalogStore::create_sequence` 経由）。

| # | 列 | 型 | 備考 |
|---|---|---|---|
| 1 | `seqrelid` | oid | シーケンスの `pg_class.oid` |
| 2 | `seqtypid` | oid | `int2`（21）、`int4`（23）、`int8`（20）。`SequenceParams.type_oid` |
| 3〜7 | `seqstart` `seqincrement` `seqmax` `seqmin` `seqcache` | int8 | `SequenceParams` の `start` `increment` `max` `min` `cache` |
| 8 | `seqcycle` | bool | `cycle` |

**pg_language（2612、9 列）** — 3 行（§3.7）。

| # | 列 | 型 | NN |
|---|---|---|---|
| 1 | `oid` | oid | ○ |
| 2 | `lanname` | name | ○（`attcollation` 950） |
| 3 | `lanowner` | oid | ○ |
| 4 | `lanispl` | bool | ○ |
| 5 | `lanpltrusted` | bool | ○ |
| 6 | `lanplcallfoid` | oid | ○ |
| 7 | `laninline` | oid | ○ |
| 8 | `lanvalidator` | oid | ○ |
| 9 | `lanacl` | aclitem[] | |

**pg_opfamily（2753、5 列）**: `oid`、`opfmethod`（oid）、`opfname`（name、`attcollation` 950）、`opfnamespace`（oid）、`opfowner`（oid）。全列 NOT NULL。

**pg_opclass（2616、9 列）**: `oid`、`opcmethod`、`opcname`（name）、`opcnamespace`、`opcowner`、`opcfamily`、`opcintype`（以上 oid）、`opcdefault`（bool）、`opckeytype`（oid）。全列 NOT NULL。

**pg_amop（2602、9 列）**: `oid`、`amopfamily`、`amoplefttype`、`amoprighttype`（oid）、`amopstrategy`（int2）、`amoppurpose`（char）、`amopopr`（oid）、`amopmethod`（oid）、`amopsortfamily`（oid）。全列 NOT NULL。

**pg_amproc（2603、6 列）**: `oid`、`amprocfamily`、`amproclefttype`、`amprocrighttype`（oid）、`amprocnum`（int2）、`amproc`（regproc）。全列 NOT NULL。

**pg_description（2609、4 列）**: `objoid`（oid）、`classoid`（oid）、`objsubid`（int4）、`description`（text、`attcollation` 950）。全列 NOT NULL。**M4 は行を 1 つも入れない**（00 §11.5 が決めを C1 に任せた点。`obj_description()` は `pg_description` を引いて NULL を返す。`COMMENT ON` は M5 以降）。

追加するカタログに合わせて変えるもの:

- `schema::CATALOGS.len() == 22`。`catalog_by_name` で `pg_catalog.pg_index` などが引ける。`catalog_table_def` が返す `TableDef` は `indexes = vec![]`、`sequence = None`、`identity_seqs = vec![]`。
- `rows::initial_rows` の `match` に `PG_LANGUAGE`（3 行）、`PG_OPFAMILY`、`PG_OPCLASS`、`PG_AMOP`、`PG_AMPROC`（静的な表から生成。§3.7）を足す。`PG_INDEX`、`PG_DEPEND`、`PG_SEQUENCE`、`PG_DESCRIPTION` は空。`pg_class` / `pg_attribute` の初期行は `CATALOGS` を回して作るので自動的に増える（`relnatts` は列数、システム列 6 つを含む。`relhasindex` は `false`。**PostgreSQL ではカタログの `relhasindex` は `t` だが、yuzhu にはカタログの索引がない**）。
- `datum_matches_type`: `Datum::Int2Vector` は型 22（`int2vector`）と 1005（`int2[]`）に合う。

### 3.2 `pg_class` の行（表・索引・シーケンス）

M2 の `class_row(&ClassSpec)` を一般化する（`relkind` が `'r'` 固定、`relam` が heap 固定、`relhasindex = false` 固定だった点を直す）。

```rust
// catalog/rows.rs
pub struct ClassSpec<'a> {
    /* M2 のフィールド: oid, name, namespace, reltype, owner, relfilenode, reltablespace, is_shared, natts, nchecks, replident */
    pub kind: RelKind,        // ★ relkind と、下の既定値（relam、relfrozenxid、relminmxid）を決める
    pub has_index: bool,      // ★ relhasindex
    pub relpages: i32,        // ★
    pub reltuples: f32,       // ★
}
```

実測した値（PostgreSQL 17 の `t(a int primary key, b text unique, c int)`、`create index t_c on t (c desc nulls first)`、`create sequence`）:

| 列 | 表（`r`） | 索引（`i`） | シーケンス（`S`） |
|---|---|---|---|
| `relkind` | `r` | `i` | `S` |
| `relam` | 2（heap） | **403（btree）** | **0** |
| `relfilenode` | 新しい OID（= `oid`） | 新しい OID | 新しい OID |
| `reltype` / `reloftype` / `reltoastrelid` | 0 | 0 | 0 |
| `relhasindex` | 索引・制約を持つなら `t`（D07-3: 最初の挿入で最終値を書く）。**PostgreSQL と同じく `DROP INDEX` では下ろさない**（D07-9） | `f` | `f` |
| `relpages` / `reltuples` | `0` / `-1` | 構築後の `BuildStats.pages` / `tuples`。**CREATE TABLE の制約が作る空の索引は `2` / `0`**（`EMPTY_INDEX_STATS`: メタページとルート）。PostgreSQL は空の索引で `1` / `0`（差分。比べない） | `1` / `1`（08） |
| `relnatts` | ユーザー列数 | 索引の列数 | 3 |
| `relchecks` | CHECK の数 | 0 | 0 |
| `relreplident` | `d` | `n` | `n` |
| `relfrozenxid` / `relminmxid` | 3 / 1（M2 のまま） | **0 / 0** | **0 / 0** |
| `relpersistence` / `relispopulated` / `relrowsecurity` ほか | `p` / `t` / `f` | 同じ | 同じ |
| `relowner` | 作ったロール | 表の所有者 | 作ったロール |
| `relnamespace` | 表の名前空間 | **表の名前空間** | 作った名前空間 |

`TRUNCATE` は表と索引の `relfilenode` を新しい値にし、`relpages = 0`、`reltuples = -1`（実測: 索引も `0` / `-1` に戻る）にする。

### 3.3 `pg_attribute` の行

**表の列**（M2 のまま）に次を足す。`attidentity` は `ColumnDef.identity` から（`'a'` ALWAYS、`'d'` BY DEFAULT、なければ NUL）。`AttributeSpec` に `identity: Option<IdentityKind>` を足す。

**索引の列**（実測）: 索引の各キー列に 1 行。システム列（`ctid` など）は**持たない**。

| 列 | 値 |
|---|---|
| `attrelid` / `attnum` | 索引の OID / 1 から（キー列の順） |
| `attname` | `ChooseIndexColumnNames` の結果（元の列名。同じ列が重複したら `a`、`a1`、`a2` …。§5.10.2） |
| `atttypid` / `atttypmod` | **表の列の型と typmod**（`varchar(5)` なら `atttypmod = 9`、`numeric(5,2)` なら `327686`）。`name` 型の列は `name` のまま（D07-17） |
| `attlen` `attbyval` `attalign` `attstorage` | 型から（`attribute_row` が `builtin` の型表から写す） |
| `attnotnull` | **常に `f`**（PRIMARY KEY の索引でも `f`。実測） |
| `atthasdef` / `atthasmissing` / `attidentity` / `attgenerated` / `attisdropped` | `f` / `f` / NUL / NUL / `f` |
| `attislocal` / `attinhcount` / `attcompression` / `attcacheoff` | `t` / 0 / NUL / -1 |
| `attcollation` | 型の `typcollation`（100 / 950 / 0） |
| `attstattarget` ほか NULL 列 | NULL |

**シーケンスの列**（実測）: `last_value`（`int8`）、`log_cnt`（`int8`）、`is_called`（`bool`）の 3 列（すべて `attnotnull = t`、`atthasdef = f`）と、システム列 6 つ。書くのは `CatalogStore::create_sequence`（§4.3）。

### 3.4 `pg_index` の行

`CREATE [UNIQUE] INDEX t_c ON t (c DESC NULLS FIRST)` の例（実測）: `indexrelid`/`indrelid` は OID、`indnatts = indnkeyatts = 1`、`indisunique = f`、`indisprimary = f`、`indimmediate = t`、`indisvalid = indisready = indislive = t`、`indkey = 3`、`indcollation = 0`、`indclass = 1978`（int4_ops）、`indoption = 3`。

`indoption` の決まり（実測）:

| 指定 | `IndexColumn` | `indoption` |
|---|---|---|
| なし / `ASC` | `descending = false`、`nulls_first = false` | 0 |
| `DESC` | `descending = true`、**`nulls_first = true`** | 3 |
| `DESC NULLS LAST` | `true`、`false` | 1 |
| `ASC NULLS FIRST` | `false`、`true` | 2 |

アナライザが `NULLS` 句の省略を「`nulls_first = descending`」と解決してから `BoundIndexColumn` に載せる（§4.6）。`indoption` の復元は `descending = (v & 1) != 0`、`nulls_first = (v & 2) != 0`。

PRIMARY KEY の索引は `indisunique = t`、`indisprimary = t`。UNIQUE 制約の索引は `indisunique = t`、`indisprimary = f`。

### 3.5 `pg_constraint` の行（PRIMARY KEY / UNIQUE）

実測した `t1_pkey`（`primary key (a)`）と `t1_b_key`（`unique (b)`）:

| 列 | 値 |
|---|---|
| `oid` | 新しい OID。**索引の OID の次**に取る（実測: 索引 17412、制約 17413） |
| `conname` | 索引と同じ名前 |
| `connamespace` | 表の名前空間 |
| `contype` | `p`（PRIMARY KEY）、`u`（UNIQUE）。CHECK は `c`（M2 のまま） |
| `condeferrable` `condeferred` `convalidated` | `f` `f` `t` |
| `conrelid` / `contypid` | 表の OID / 0 |
| `conindid` | **索引の OID**（CHECK は 0） |
| `conparentid` / `confrelid` | 0 / 0 |
| `confupdtype` `confdeltype` `confmatchtype` | 空白（`' '`、M2 のまま） |
| `conislocal` / `coninhcount` | `t` / 0 |
| `connoinherit` | **`t`**（実測。CHECK は `f`のまま） |
| `conkey` | **`Datum::Int2Vector(attnum の並び)`**（`int2[]` 型の列。例 `{1,2}`）。CHECK は NULL のまま |
| `confkey` `conpfeqop` `conppeqop` `conffeqop` `confdelsetcols` `conexclop` `conbin` | NULL |

`conkey` の型の列（1005）に入る値は `Datum::Int2Vector`（00 §12.1）です。ヒープでの符号化は 00 §12.3（varlena + `i16` LE の並び。PostgreSQL の配列ヘッダはない）。SQL からの見え方は `{1,2}`（配列の出力形式。出力は 09）。

### 3.6 `pg_depend` に記録する依存

**00 §11.6 の表を、実測に基づいて列ごとに具体化**したものです。「依存元 → 依存先」の向きで、依存元が消えるときに依存先は残り、**依存先が消えるときに依存元が（deptype に応じて）一緒に消える・消せない**。`ObjectAddress` は `(classid, objid, objsubid)`。

| いつ | 依存元 | 依存先 | deptype | 実測 |
|---|---|---|---|---|
| CREATE TABLE（既定値のある列） | `pg_attrdef`(oid) | `pg_class`(表)、`objsubid` なし、`refobjsubid` = 列 | `a` | ○ |
| 同上。`nextval('s'::regclass)` のように既定値が `regclass` 定数でシーケンスを指すとき | `pg_attrdef`(oid) | `pg_class`(シーケンス)、`refobjsubid = 0` | `n` | ○ |
| CREATE TABLE（CHECK） | `pg_constraint`(oid) | `pg_class`(表)、`refobjsubid = 0` | `a` | **差分**: PostgreSQL は式が参照する列ごと（未検証。D07-16） |
| PRIMARY KEY / UNIQUE の制約 | `pg_constraint`(oid) | `pg_class`(表)、**キー列ごとに 1 行**（`refobjsubid` = attnum） | `a` | ○（`{1,2}` なら 2 行） |
| 同上の索引 | `pg_class`(索引) | `pg_constraint`(制約)、`refobjsubid = 0` | `i` | ○ |
| 通常の `CREATE INDEX` | `pg_class`(索引) | `pg_class`(表)、**キー列ごとに 1 行**（同じ列の重複は 1 行にまとめる） | `a` | ○（`(a, b)` は 2 行。表自体への行はない） |
| SERIAL のシーケンス（08） | `pg_class`(シーケンス) | `pg_class`(表)、`refobjsubid` = 列 | `a` | ○ |
| IDENTITY のシーケンス（08） | 同上 | 同上 | `i` | ○ |

書かないもの（D07-16）: `pg_class`（表・索引・シーケンス）→ `pg_namespace` の `n`（実測では PostgreSQL は書く）、`pg_type` の行型への依存（行型を作らない）、`pinned`。

**`pg_depend` の行の順序**（DETAIL の列挙順に効く）: 追加順（物理順）。`DependRow` を返す関数は物理順で返す。

### 3.7 静的な表から生成する行（`pg_opfamily` / `pg_opclass` / `pg_amop` / `pg_amproc` / `pg_language`）

静的な表（`catalog/opclass.rs`: `OPFAMILIES`、`OPCLASSES`、`AMOPS`、`AMPROCS`。持ち主は `06-btree.md`、00 §11.3）から `rows.rs` が行を作る。**カタログに書く行と、プランナ・B+Tree が実行時に引く静的な表は同じ元から作る**（ずれを防ぐ）。

```rust
// catalog/rows.rs（いずれも oid 昇順など、決まった順に並べてから行にする。builtin_hash が表の書き順に左右されないように）
fn opfamily_rows() -> Vec<Row>;   // OPFAMILIES を oid 順
fn opclass_rows() -> Vec<Row>;    // OPCLASSES を oid 順
fn amop_rows() -> Vec<Row>;       // AMOPS を (family, left, right, strategy) 順
fn amproc_rows() -> Vec<Row>;     // AMPROCS を (family, left, right, support) 順
fn language_rows() -> Vec<Row>;
```

| カタログ | 列 | 値 |
|---|---|---|
| `pg_opfamily` | `oid` `opfname` | `OpFamily.oid` `.name` |
| | `opfmethod` `opfnamespace` `opfowner` | **403**（btree）、11（pg_catalog）、10 |
| `pg_opclass` | `oid` `opcname` `opcfamily` `opcintype` `opcdefault` | `OpClass.oid` `.name` `.family` `.input_type` `.is_default` |
| | `opcmethod` `opcnamespace` `opcowner` `opckeytype` | 403、11、10、**0**（D07-17。PostgreSQL の `name_ops` は `cstring`） |
| `pg_amop` | `amopfamily` `amoplefttype` `amoprighttype` `amopstrategy` `amopopr` | `AmOp.family` `.left` `.right` `.strategy`（`i16`）`.operator` |
| | `oid` | **`oid::FIRST_GENBKI_OBJECT_ID`（10000）から、上の並び順に振る**（pg_cast と同じ。PostgreSQL の値は版ごとに変わるので比べない） |
| | `amoppurpose` `amopmethod` `amopsortfamily` | `s`（search）、403、0 |
| `pg_amproc` | `amprocfamily` `amproclefttype` `amprocrighttype` `amprocnum` `amproc` | `AmProc.family` `.left` `.right` `.support`（`i16`）`.proc_oid`（regproc） |
| | `oid` | 10000 から、上の並び順に振る |
| `pg_language` | 3 行 | 下表 |

**`pg_language` の 3 行**（実測の PostgreSQL 17 の値。`lanvalidator` は PostgreSQL では `fmgr_*_validator` の OID（2246〜2248）だが、yuzhu に対応する `pg_proc` の行がなく参照を閉じられないので 0 にする）:

| `oid` | `lanname` | `lanowner` | `lanispl` | `lanpltrusted` | `lanplcallfoid` / `laninline` / `lanvalidator` | `lanacl` |
|---|---|---|---|---|---|---|
| 12 | `internal` | 10 | `f` | `f` | 0 / 0 / 0 | NULL |
| 13 | `c` | 10 | `f` | `f` | 0 / 0 / 0 | NULL |
| 14 | `sql` | 10 | `f` | `t` | 0 / 0 / 0 | NULL |

これで `pg_proc.prolang = 12` が実在する行を指し、**M2 の参照の閉包の唯一の例外（`prolang`）がなくなる**。`rows::reference_columns()` に `(PG_PROC, "prolang", PG_LANGUAGE)` を足し、`dangling_references` のテストの例外の記述を消す。

**参照の閉包に加える列**（`reference_columns()`）:

| カタログ.列 | 参照先 |
|---|---|
| `pg_opfamily.opfmethod` / `opfnamespace` / `opfowner` | `pg_am` / `pg_namespace` / `pg_authid` |
| `pg_opclass.opcmethod` / `opcnamespace` / `opcowner` / `opcfamily` / `opcintype` | `pg_am` / `pg_namespace` / `pg_authid` / `pg_opfamily` / `pg_type` |
| `pg_amop.amopfamily` / `amoplefttype` / `amoprighttype` / `amopopr` / `amopmethod` | `pg_opfamily` / `pg_type` / `pg_type` / `pg_operator` / `pg_am` |
| `pg_amproc.amprocfamily` / `amproclefttype` / `amprocrighttype` / `amproc` | `pg_opfamily` / `pg_type` / `pg_type` / `pg_proc` |
| `pg_language.lanowner` | `pg_authid` |

したがって、**`AMOPS` に入れる行は、`builtin::OPERATORS` に実在する演算子だけ**（`pg_amop` の行数は PostgreSQL より少なくてよい。例: 09 が date と timestamp の相互の比較演算子を入れなければ、`datetime_ops` のその組は `AMOPS` に入れない）。`AMPROCS` の `proc_oid` は `builtin::PROCS`（`pg_proc` の行）に実在させる（§3.8）。`AMPROCS` に入れるのは support 1（比較関数）だけ（PostgreSQL は 2〜4 も持つ。差分。行数は比べない）。

### 3.8 `pg_type` / `pg_proc` / `pg_operator` / `pg_cast` への追加と `builtin_hash`

この章の行が参照するもののうち、**既存の表にまだないもの**を挙げる。行の追加そのものは `catalog/builtin.rs` の持ち主（T1〜T3、06）が行い、C1 は `dangling_references` のテストで漏れを検出する。

- `pg_type`: `int2vector`（22）と `_int2`（1005）。`pg_index.indkey` / `indoption`、`pg_constraint.conkey` の列の型。`oidvector`（30）、`pg_node_tree`（194）、`_aclitem`（1034）は M2 から。担当は 09（T3）。
- `pg_proc`（呼び出し可能な `BuiltinFunction` ではなく**カタログの行だけ**を持つ `BuiltinProc` を足す）: opclass の比較関数。名前と OID は実測の PostgreSQL 17 のもの。担当は 06（B2）。

| family | `pg_proc` に必要な比較関数（名前: OID） |
|---|---|
| `bool_ops` | `btboolcmp`: 1693 |
| `integer_ops` | `btint2cmp`: 350、`btint4cmp`: 351、`btint8cmp`: 842、`btint24cmp`: 2190、`btint28cmp`: 2192、`btint42cmp`: 2191、`btint48cmp`: 2188、`btint82cmp`: 2193、`btint84cmp`: 2189 |
| `float_ops` | `btfloat4cmp`: 354、`btfloat8cmp`: 355、`btfloat48cmp`: 2194、`btfloat84cmp`: 2195 |
| `numeric_ops` | `numeric_cmp`: 1769 |
| `text_ops` | `bttextcmp`: 360、`btnamecmp`: 359、`btnametextcmp`: 246、`bttextnamecmp`: 253 |
| `bpchar_ops` | `bpcharcmp`: 1078 |
| `oid_ops` | `btoidcmp`: 356 |
| `datetime_ops` | `date_cmp`: 1092、`timestamp_cmp`: 2045、`timestamptz_cmp`: 1314、`date_cmp_timestamp`: 2344、`date_cmp_timestamptz`: 2357、`timestamp_cmp_date`: 2370、`timestamp_cmp_timestamptz`: 2526、`timestamptz_cmp_date`: 2383、`timestamptz_cmp_timestamp`: 2533 |

- `pg_operator`: opclass に出てくる演算子（`=`、`<`、`<=`、`>=`、`>`）は 09 が行を入れる（date / timestamp / timestamptz の相互の比較を含むかは 09 が決める。含めなければ `AMOPS` からも外す）。
- `pg_cast`: この章は足さない。

**opclass の OID について**（実測）: `pg_opfamily` の OID は `bool_ops` 424、`bpchar_ops` 426、`float_ops` 1970、`integer_ops` 1976、`numeric_ops` 1988、`oid_ops` 1989、`text_ops` 1994、`datetime_ops` 434 と手で固定されているが、**`pg_opclass` の OID は一部（`int4_ops` 1978、`int2_ops` 1979、`int8_ops` 3124、`text_ops` 3126、`float8_ops` 3123、`numeric_ops` 3125、`oid_ops` 1981、`date_ops` 3122、`timestamptz_ops` 3127、`timestamp_ops` 3128）だけが固定で、`bool_ops`（10003）、`bpchar_ops`（10004）、`float4_ops`（10012）、`name_ops`（10028）、`varchar_ops`（10044）などは initdb が 10000 から自動で振った値**。値の決め方は 06 の持ち分で、**テストは `pg_opclass.oid` を PostgreSQL と比べない**（名前で結合して比べる）。

**`builtin_hash`**（M2 §6.8.5）: 対象に次の 5 つの**生成した行**を加える。静的な表（`OPFAMILIES` など）を変えた実行ファイルが古いデータディレクトリを黙って使うのを防ぐ。

```rust
// catalog/rows.rs
pub fn builtin_canonical_bytes() -> Vec<u8> {
    for catalog in [PG_TYPE, PG_PROC, PG_OPERATOR, PG_CAST,
                    PG_LANGUAGE, PG_OPFAMILY, PG_OPCLASS, PG_AMOP, PG_AMPROC] { /* M2 と同じ形式で追記 */ }
}
// put_datum は Datum::Int2Vector（タグ 16: 要素数 u32 + i16 LE の並び）などの新しい変種を扱う（A が変種を足したときに match が非網羅でコンパイルエラーになる）
```

### 3.9 `CATALOG_VERSION_NO` と initdb

- `CATALOG_VERSION_NO`（`storage/mod.rs`）は A が **M4 の最初の変更で上げる**（00 §11.5）。この章の変更（新しいカタログ、列の値、`builtin_hash` の対象）は M3 のデータディレクトリと非互換なので、**M3 の最後の値より大きい値にする**（形式 `YYYYMMDDNN`、目安 `2_026_100_701`）。制御ファイルの `catalog_version` が違えば起動を拒否し、initdb のやり直しを求める（M2-Q20、00 の D-28）。
- initdb の手順（M2 §5.8、M3 §5.8）は変えず、書く行が増えるだけ。`CatalogStore::bootstrap` は `CATALOGS` の全カタログのファイルを作り、`initial_rows` の行を入れる（新しい 9 カタログも同じ）。`pg_language` の 3 行、`pg_opfamily` 以下の生成行が入る。**template1 から template0 / postgres へのコピー**（`datadir::copy_database_dir`）で新しいファイルも増える。
- 起動時の検査（`builtin_hash` の一致）は、上のとおり対象が増える。

### 3.10 例: `CREATE TABLE t (id serial PRIMARY KEY, name text UNIQUE, n int CHECK (n > 0))` が書く行

OID の割り当て順（`CatalogStore::allocate_table_oids`。カウンタは 1 つで、`get_new_relation_oid` と `get_new_oid` が同じカウンタを使う）: 表 → `pg_attrdef`（列の順）→ CHECK の制約 → 索引制約ごとに（**索引、その制約**の順。PRIMARY KEY が先）→（その後、08 が）シーケンス。**PostgreSQL ではシーケンスが表より先に採番される**（実測: シーケンス 17645、表 17648）が、yuzhu はシーケンスの `owned_by` に表の OID が要るので表が先（OID の値は比べない）。例として 16384 から: `t` 16384、`id` の `pg_attrdef` 16385、`t_n_check` 16386、`t_pkey`（索引）16387、`t_pkey`（制約）16388、`t_name_key`（索引）16389、`t_name_key`（制約）16390、`t_id_seq` 16391。

| カタログ | 行 |
|---|---|
| `pg_class` | `t`（`r`、`relhasindex = t`、`relnatts = 3`、`relchecks = 1`、`relam = 2`）、`t_pkey`（`i`、`relam = 403`、`relnatts = 1`、`relpages = 2`、`reltuples = 0`）、`t_name_key`（`i`、同上）、`t_id_seq`（`S`、`relam = 0`、`relpages = 1`、`reltuples = 1`） |
| `pg_attribute` | `t` の `id`（`atthasdef = t`、`attnotnull = t`）、`name`（`attcollation = 100`）、`n`、システム列 6 つ。`t_pkey` の `id`（`attnotnull = f`）、`t_name_key` の `name`（`attcollation = 100`、`attstorage = x`）。`t_id_seq` の 3 列とシステム列 6 つ |
| `pg_attrdef` | `(16385, t, 1, "nextval('t_id_seq'::regclass)")` |
| `pg_index` | `t_pkey`: `indkey = 1`、`indclass = 1978`、`indcollation = 0`、`indoption = 0`、`indisunique = indisprimary = t`。`t_name_key`: `indkey = 2`、`indclass = 3126`、`indcollation = 100`、`indisunique = t`、`indisprimary = f` |
| `pg_constraint` | `t_n_check`（`c`、`conbin = 'n > 0'`）、`t_pkey`（`p`、`conindid = 16387`、`conkey = {1}`、`connoinherit = t`）、`t_name_key`（`u`、`conindid = 16389`、`conkey = {2}`） |
| `pg_depend` | `(pg_attrdef 16385) → (pg_class t, 1)` `a`、`(pg_attrdef 16385) → (pg_class t_id_seq, 0)` `n`、`(pg_constraint t_n_check) → (pg_class t, 0)` `a`、`(pg_constraint t_pkey) → (pg_class t, 1)` `a`、`(pg_class t_pkey) → (pg_constraint t_pkey)` `i`、`(pg_constraint t_name_key) → (pg_class t, 2)` `a`、`(pg_class t_name_key) → (pg_constraint t_name_key)` `i`、`(pg_class t_id_seq) → (pg_class t, 1)` `a`（08 が書く） |
| `pg_sequence` | `(16391, int4, 1, 1, 2147483647, 1, 1, f)`（08 が書く） |

---

## 4. 型とトレイト（契約の具体化）

00 §11（`RelKind`、`IndexDef`、`TableDef`、`CatalogReader` の追加メソッド、`RelHandle` / `IndexHandle`）と §14.6（`DdlCtx`）の署名は**変えない**。この節は、それを実装するための追加の型とメソッド、手順を定める。`catalog/mod.rs` と `analyzer/bound.rs` の**型の宣言は A が置く**（00 §17）。C1 は、この節の型をそのまま宣言するよう A に伝え、中身（メソッドの実装）を書く。`store.rs` / `depend.rs` の型（`NewTable`、`NewIndex`、`DropPlan` など）は C1 の持ち物。

### 4.1 `catalog/mod.rs` の追加

```rust
impl RelKind {
    pub fn code(self) -> char;                          // 'r' / 'i' / 'S'（00 §11.1）
    pub fn from_code(c: char) -> Option<RelKind>;
    /// エラーメッセージの名詞: "table" / "index" / "sequence"
    pub fn noun(self) -> &'static str;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConstraintKind { Check /* 'c' */, PrimaryKey /* 'p' */, Unique /* 'u' */ }

/// pg_constraint の 1 行（M4 は c / p / u だけ）。pg_get_constraintdef と DROP の説明が使う（§4.2 の constraint_by_oid）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstraintDef {
    pub oid: Oid, pub name: String, pub namespace: Oid, pub kind: ConstraintKind,
    pub table_oid: Oid,
    /// PRIMARY KEY / UNIQUE の索引（conindid）。Check は None
    pub index_oid: Option<Oid>,
    /// conkey（attnum の並び）。Check は空（conkey が NULL のため）
    pub columns: Vec<i16>,
    /// Check の式（conbin。SQL テキスト）
    pub check_sql: Option<String>,
}

impl TableDef {
    pub fn index_by_oid(&self, oid: Oid) -> Option<&Arc<IndexDef>>;
    pub fn index_by_name(&self, name: &str) -> Option<&Arc<IndexDef>>;
}
impl IndexDef {
    /// PRIMARY KEY / UNIQUE 制約が所有する索引か（constraint が Some）
    pub fn is_constraint_index(&self) -> bool;
    /// 列 i の indoption（bit0 = DESC、bit1 = NULLS FIRST）。pg_index.indoption の値
    pub fn indoption(&self, i: usize) -> i16;
    /// indoption から IndexColumn の descending / nulls_first を復元する
    pub fn flags_from_indoption(v: i16) -> (bool /* descending */, bool /* nulls_first */);
}
```

`ColumnDef.identity`（00 §11.1）は `attidentity` の値（`'a'` → `Always`、`'d'` → `ByDefault`、NUL → `None`）。`TableDef.indexes` は **OID の昇順**（挿入時の索引の更新順になる。`RelHandle::from_table` がそのまま写す）。

### 4.2 `CatalogReader` の実装（`StatementCatalog`、`catalog/reader.rs`）

00 §11.2 の 4 メソッドと既存メソッドを、次のように実装する。00 に**1 つ追加**する（`constraint_by_oid`。00 への変更提案 5）。

```rust
pub trait CatalogReader {
    /* 00 §11.2 のメソッド: relation_kind / index_by_name / index_by_oid / relation_name / aggregates_named */
    /// pg_get_constraintdef と \d tbl が使う。pg_constraint の 1 行（c / p / u）。なければ None
    fn constraint_by_oid(&self, oid: Oid) -> Result<Option<ConstraintDef>>;
}
```

内部の名前解決を、表・シーケンス・索引で共通の 1 本にする（M2 の `in_namespace` を置き換える）。

```rust
impl StatementCatalog<'_> {
    /// 名前空間 nsp の中の、この名前のリレーション（表・索引・シーケンスのどれでも）。
    /// 1. !bypass_cache: cache.get_relation_by_name(nsp, name) があればそれ
    /// 2. なければ db.catalog.lookup_relation_row(snapshot, nsp, name)（pg_class を 1 回走査）→ (oid, kind)
    /// 存在しないことはキャッシュしない（M2 §6.8.6）
    fn relation_in_namespace(&self, nsp: Oid, name: &str) -> Result<Option<(Oid, RelKind)>>;
    /// 修飾なしの名前: 22 個のシステムカタログ（釘付け）→ search_path の名前空間を順に（pg_catalog は暗黙に先頭）。
    /// "pg_catalog.x" と修飾されたらシステムカタログだけ
    fn resolve_relation(&self, schema: Option<&str>, name: &str) -> Result<Option<(Oid, RelKind)>>;
}
```

| メソッド | 実装 |
|---|---|
| `relation_kind(schema, name)` | `resolve_relation` の結果。システムカタログ（釘付け）は `(oid, RelKind::Table)` |
| `table(schema, name)` | `resolve_relation` → `Table` / `Sequence` なら `self.load(oid)`、`Index` なら **`Ok(None)`**（D07-18）、なければ `Ok(None)`。**検索パスの先にある名前空間の索引が、後ろの名前空間の同名の表を隠す**（PostgreSQL と同じ。M4 に `CREATE SCHEMA` がないので public だけ） |
| `table_by_oid(oid)` | `self.load(oid)`。**索引の OID には `None`**（`load` が `load_table_def` に委ねる。relkind `i` は `Ok(None)`） |
| `index_by_oid(oid)` | 1. `!bypass_cache` なら `cache.get_index_owner(oid)`（索引 → 表の OID）。2. なければ `db.catalog.index_owner(snapshot, oid)`（`pg_index.indrelid`。**`pg_class` の relkind を見ずに `pg_index` だけを引く**）。3. 表の `self.load(table_oid)` の `indexes` から OID が一致する `Arc<IndexDef>` を返す。表が見つからない・索引が含まれない（`indisvalid` が偽など）は `None` |
| `index_by_name(schema, name)` | `resolve_relation` → `Index` なら `index_by_oid`、それ以外は `None` |
| `relation_name(oid)` | `table_by_oid` か `index_by_oid` で `(namespace, name)` を得る。**名前空間が `visible_namespaces()` に含まれれば修飾せず、含まれなければ `schema.name`**（PostgreSQL の `regclassout`）。識別子の引用符（大文字・記号・予約語）は `types::sys` の `quote_identifier`（09）を通す |
| `visible_namespaces()` | M2 のまま |
| `constraint_by_oid(oid)` | `db.catalog.constraint_by_oid(snapshot, oid)`（キャッシュしない。`\d` でしか呼ばれない） |

`builtin::pg_table_is_visible`（`FnKind::Context`）は**索引とシーケンスも対象にする**（`\di` の `pg_catalog.pg_table_is_visible(c.oid)`）:

```rust
fn pg_table_is_visible(args: &[Datum], catalog: &dyn CatalogReader, _s: &SessionInfo) -> Result<Datum> {
    let Some(Datum::Oid(rel)) = args.first() else { /* 内部エラー */ };
    // 表・シーケンス → 索引の順に、(名前, OID) を得る。どちらでもなければ NULL（PostgreSQL と同じ）
    let name = match (catalog.table_by_oid(*rel)?, catalog.index_by_oid(*rel)?) {
        (Some(t), _) => t.name.clone(),
        (None, Some(i)) => i.name.clone(),
        (None, None) => return Ok(Datum::Null),
    };
    // 修飾なしの名前がこのリレーションを指すか（pg_catalog 先頭 → search_path の順。別の種類のリレーションが同名で先にあれば false）
    let found = catalog.relation_kind(None, &name)?;
    Ok(Datum::Bool(found.is_some_and(|(oid, _)| oid == *rel)))
}
```

### 4.3 `CatalogStore` の追加メソッド（`catalog/store.rs`）

M2 の `CatalogStore`（`create_table`、`drop_table`、`get_new_oid`、`get_new_relation_oid`、`allocate_child_oids`、`load_table_def`、`lookup_relation`）に次を足す・変える。**書き込みは常に `WriteCtx` と、そのコマンドのスナップショットを受け取る**（`TableStore::update` / `delete` がスナップショットを要る）。

```rust
// ---- 読み取り ----
/// pg_class の 1 行の主な列（kind は relkind から）
#[derive(Clone, Debug)]
pub struct RelationRow {
    pub oid: Oid, pub name: String, pub namespace: Oid, pub kind: RelKind, pub owner: Oid,
    pub locator: RelFileLocator, pub natts: i16, pub has_index: bool,
}
impl CatalogStore {
    /// M2 の lookup_relation を置き換える（M2 の戻り値 Option<Oid> を RelationRow に）。relkind が r / i / S 以外なら Error::corrupted
    pub fn lookup_relation_row(&self, snap: &Snapshot, nsp: Oid, name: &str) -> Result<Option<RelationRow>>;
    pub fn relation_row(&self, snap: &Snapshot, oid: Oid) -> Result<Option<RelationRow>>;
    /// 表（r）とシーケンス（S）の TableDef。索引（i）は Ok(None)。手順は §4.5
    pub fn load_table_def(&self, snap: &Snapshot, oid: Oid) -> Result<Option<TableDef>>;
    /// pg_index.indrelid。索引でなければ None
    pub fn index_owner(&self, snap: &Snapshot, index_oid: Oid) -> Result<Option<Oid>>;
    pub fn constraint_by_oid(&self, snap: &Snapshot, oid: Oid) -> Result<Option<ConstraintDef>>;
    /// 名前空間内に同名の制約があるか（PostgreSQL の ConstraintNameExists。ChooseRelationName の衝突判定に使う）
    pub fn constraint_name_exists(&self, snap: &Snapshot, nsp: Oid, name: &str) -> Result<bool>;
    /// この表の制約（c / p / u）の名前。ADD CONSTRAINT の重複検査（42710）に使う
    pub fn constraint_names_of(&self, snap: &Snapshot, relid: Oid) -> Result<Vec<String>>;
    /// この表の列が所有するシーケンス（pg_depend: 依存元が relkind S の pg_class、依存先が表、deptype a / i）の OID。TRUNCATE RESTART IDENTITY と OWNER TO が使う
    pub fn owned_sequences(&self, snap: &Snapshot, table_oid: Oid) -> Result<Vec<Oid>>;
    /// pg_depend: 依存先が referenced の行（物理順）。referenced.obj_sub == 0 は「そのオブジェクトの全列」を含む
    pub fn dependents_of(&self, snap: &Snapshot, referenced: ObjectAddress) -> Result<Vec<DependRow>>;
    /// pg_depend: 依存元が dependent の行（objsubid は問わない）
    pub fn references_of(&self, snap: &Snapshot, dependent: ObjectAddress) -> Result<Vec<DependRow>>;
    /// PostgreSQL の getObjectDescription に当たる文（§5.8.2 の表）。visible は visible_namespaces()（修飾の要否）
    pub fn describe_object(&self, snap: &Snapshot, obj: ObjectAddress, visible: &[Oid]) -> Result<String>;

// ---- OID ----
    /// 新しい relfilenode（TRUNCATE）。get_new_relation_oid と同じ（OID カウンタ + カタログと storage_exists の衝突確認）
    pub fn get_new_relfilenumber(&self, alloc: &OidAllocator) -> Result<Oid>;
    /// M2 の allocate_child_oids を置き換える（§5.1 手順 4）
    pub fn allocate_table_oids(&self, alloc: &OidAllocator, plan: &TableOidRequest) -> Result<TableOids>;

// ---- 書き込み（いずれも catalog_dirty は呼び出し側が立てる）----
    /// pg_class・pg_attribute（システム列を含む）・pg_attrdef・pg_constraint（CHECK）に加えて、
    /// spec.indexes の索引ごとに write_index_rows と同じ行（pg_class(i)・pg_attribute・pg_index・pg_constraint(p/u)・pg_depend）、
    /// 既定値・CHECK・列の pg_depend、spec.extra_depends を書く。ファイルは作らない
    pub fn create_table(&self, w: &WriteCtx, snap: &Snapshot, spec: &NewTable) -> Result<()>;
    /// 既存の表への索引: pg_class(i)・pg_attribute・pg_index・(constraint があれば pg_constraint と pg_depend)。
    /// mark_table_indexed が真なら表の pg_class.relhasindex を true にする（まだ false のときだけ update_class_row）
    pub fn create_index(&self, w: &WriteCtx, snap: &Snapshot, spec: &NewIndex, mark_table_indexed: bool) -> Result<()>;
    /// ALTER TABLE ADD PRIMARY KEY / UNIQUE: create_index（spec.constraint = Some、mark_table_indexed = true）に加えて、
    /// spec.primary なら key 列の pg_attribute.attnotnull を true にする（update_attribute。すでに true の列は更新しない）
    pub fn add_constraint(&self, w: &WriteCtx, snap: &Snapshot, spec: &NewIndex, table: &TableDef) -> Result<()>;
    /// シーケンス: pg_class(S)・pg_attribute（3 列 + システム列）・pg_sequence・pg_depend（params.owned_by と owned_by_deptype が Some のとき）。08 の ddl/sequence.rs が呼ぶ
    pub fn create_sequence(&self, w: &WriteCtx, snap: &Snapshot, spec: &NewSequence) -> Result<()>;
    /// ALTER SEQUENCE: pg_sequence の行を更新する（seqtypid・seqstart・seqincrement・seqmax・seqmin・seqcache・seqcycle。owned_by は pg_depend なので触らない）。
    /// 08 §5.5 が呼ぶ（レビュー対応 R-08。08 が C1 に求めていた `update_sequence_params`。`drop_sequence` / `drop_default` は
    /// `drop_objects`（DropKind::Sequence / AttrDefault）が行うので作らない）
    pub fn update_sequence_params(&self, w: &WriteCtx, snap: &Snapshot, oid: Oid, p: &SequenceParams) -> Result<()>;
    /// plan.items のオブジェクトの行をすべて消す（§6.3 の drop_objects の手順）。消したリレーションのファイルの場所を返す（呼び出し側が pending_unlinks に積む）。
    /// M2 の drop_table を置き換える。DROP INDEX もこれ（`plan_drop` の items が `DropKind::Index` 1 つ + その行）。専用の drop_index メソッドは作らない
    pub fn drop_objects(&self, w: &WriteCtx, snap: &Snapshot, plan: &DropPlan) -> Result<Vec<RelFileLocator>>;
    /// pg_class の 1 行を更新する。見つからない・Invisible・SelfModified（同じコマンドで 2 度目）は Error::internal（D07-3）
    pub fn update_class_row(&self, w: &WriteCtx, snap: &Snapshot, oid: Oid, patch: &ClassPatch) -> Result<()>;
    pub fn update_attribute(&self, w: &WriteCtx, snap: &Snapshot, relid: Oid, attnum: i16, patch: &AttributePatch) -> Result<()>;
    /// TRUNCATE: update_class_row(oid, relfilenode = new, relpages = 0, reltuples = -1)
    pub fn truncate_relation(&self, w: &WriteCtx, snap: &Snapshot, oid: Oid, new_relfilenode: Oid) -> Result<()>;
    pub fn record_dependency(&self, w: &WriteCtx, d: &NewDepend) -> Result<()>;
    pub fn record_dependencies(&self, w: &WriteCtx, ds: &[NewDepend]) -> Result<()>;      // 同じ (依存元, 依存先, deptype) の重複を除いて挿入
    /// pg_depend の行を消す。filter に当たる行をすべて（DropPlan の後始末と、08 の OWNED BY の付け替えが使う）
    pub fn delete_dependencies(&self, w: &WriteCtx, snap: &Snapshot, filter: DependFilter) -> Result<usize>;
}

#[derive(Clone, Copy, Debug)]
pub enum DependFilter {
    /// 依存元が (class_id, obj_id)（obj_sub は問わない）
    Dependent { class_id: Oid, obj_id: Oid },
    /// 依存先が (class_id, obj_id)（obj_sub は問わない）
    Referenced { class_id: Oid, obj_id: Oid },
    /// 依存元・依存先の両方を指定（行を 1 つ消す）
    Exact { dependent: ObjectAddress, referenced: ObjectAddress },
}

#[derive(Default, Clone, Debug)]
pub struct ClassPatch {
    pub relfilenode: Option<Oid>, pub relhasindex: Option<bool>, pub relpages: Option<i32>, pub reltuples: Option<f32>,
    pub relowner: Option<Oid>,
}
#[derive(Default, Clone, Debug)]
pub struct AttributePatch { pub not_null: Option<bool>, pub has_default: Option<bool> }
```

書き込みに渡す仕様の型:

```rust
/// M2 の NewTable に足す
pub struct NewTable {
    /* M2: oid, namespace, name, owner, columns（identity を含む）, checks, attrdef_oids, constraint_oids */
    /// PRIMARY KEY / UNIQUE 制約が所有する索引（PRIMARY KEY が先）。空の表なので init_index 済みで build は不要
    pub indexes: Vec<NewIndex>,
    /// 呼び出し側が組み立てる追加の依存（既定値 → シーケンスの n、08 のシーケンス → 列の a / i）
    pub extra_depends: Vec<NewDepend>,
}

pub struct NewIndex {
    pub oid: Oid, pub name: String, pub namespace: Oid, pub owner: Oid,
    pub table_oid: Oid,
    /// relfilenode（= oid。M4 は常に oid と同じ）
    pub relfilenode: Oid,
    pub columns: Vec<NewIndexColumn>,
    pub unique: bool, pub primary: bool,
    /// Some なら pg_constraint の行（contype p / u）と、制約 → 列の a、索引 → 制約の i を書く。None なら索引 → 列の a
    pub constraint: Option<NewConstraint>,
    /// relpages / reltuples（EMPTY_INDEX_STATS または build の結果）
    pub stats: BuildStats,
}
pub struct NewIndexColumn {
    /// 索引の attname（ChooseIndexColumnNames の結果）
    pub name: String,
    pub column: IndexColumn,
    /// 表の列の型（atttypid / atttypmod / attcollation の元）
    pub ty: SqlType,
}
pub struct NewConstraint { pub oid: Oid, pub name: String }

pub struct NewSequence {
    pub oid: Oid, pub name: String, pub namespace: Oid, pub owner: Oid, pub params: SequenceParams,
    /// SERIAL → a、IDENTITY → i。owned_by が Some のときだけ依存を書く
    pub owned_by_deptype: Option<DependType>,
}

/// CREATE TABLE が先に決める OID の要求（§5.1 の手順 4）
pub struct TableOidRequest { pub n_attrdefs: usize, pub n_checks: usize, pub n_index_constraints: usize }
pub struct TableOids {
    pub table: Oid, pub attrdefs: Vec<Oid>, pub checks: Vec<Oid>,
    /// 索引制約ごと（索引、制約）の順に取った OID
    pub index_constraints: Vec<(Oid /* index */, Oid /* constraint */)>,
}
pub const EMPTY_INDEX_STATS: BuildStats = BuildStats { tuples: 0, pages: 2, levels: 0 };
```

行を組み立てる関数（`catalog/rows.rs`）。`CatalogStore` のメソッドが呼ぶ（テストが列名で組み立てられるように `RowBuilder` を使う。M2 の方針）:

```rust
pub fn index_row(s: &IndexRowSpec<'_>) -> Row;          // pg_index（§3.4）
pub fn constraint_row(s: &ConstraintRowSpec<'_>) -> Row; // pg_constraint（c / p / u）。check_constraint_row はこの薄い包み
pub fn depend_row(d: &NewDepend) -> Row;                 // pg_depend
pub fn sequence_row(oid: Oid, p: &SequenceParams) -> Row; // pg_sequence
pub fn index_attribute_rows(s: &NewIndex) -> Vec<Row>;   // 索引の pg_attribute（システム列なし）
pub fn sequence_attribute_rows(relid: Oid) -> Vec<Row>;  // last_value / log_cnt / is_called + システム列 6 つ
```

#### 依存関係と DROP の計画（`catalog/depend.rs`）

`catalog/store.rs` が使い、`ddl/` と 08 が呼ぶ型と関数。`ddl/depend.rs` はこの上の「依存の組み立て」だけを持つ（§6.8）。

```rust
// catalog/depend.rs
pub mod classes { pub const RELATION: Oid = 1259; pub const CONSTRAINT: Oid = 2606; pub const ATTRDEF: Oid = 2604; }

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ObjectAddress { pub class_id: Oid, pub obj_id: Oid, pub obj_sub: i32 }
impl ObjectAddress {
    pub fn relation(oid: Oid) -> Self;                 // (pg_class, oid, 0)
    pub fn column(oid: Oid, attnum: i16) -> Self;      // (pg_class, oid, attnum)
    pub fn constraint(oid: Oid) -> Self;
    pub fn attrdef(oid: Oid) -> Self;
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DependType { Normal /* n */, Auto /* a */, Internal /* i */ }
impl DependType { pub fn code(self) -> char; pub fn from_code(c: char) -> Option<DependType>; }

#[derive(Clone, Debug)] pub struct NewDepend { pub dependent: ObjectAddress, pub referenced: ObjectAddress, pub deptype: DependType }
#[derive(Clone, Debug)] pub struct DependRow { pub dependent: ObjectAddress, pub referenced: ObjectAddress, pub deptype: DependType }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropBehavior { Restrict, Cascade }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropKind { Table, Index, Sequence, Constraint, AttrDefault }
/// 消す 1 つのオブジェクト
#[derive(Clone, Debug)]
pub struct DropItem {
    pub addr: ObjectAddress, pub kind: DropKind,
    /// "table t"、"index t_pkey"、"constraint t_pkey on table t"、"default value for column id of table t" など
    pub description: String,
    /// Table / Index / Sequence のファイル。ほかは None
    pub locator: Option<RelFileLocator>,
    /// Constraint / AttrDefault が属する表と列（AttrDefault の atthasdef を戻すため）
    pub owner_table: Option<Oid>, pub attnum: Option<i16>,
}
#[derive(Clone, Debug, Default)]
pub struct DropPlan {
    /// 重複のない、消すオブジェクトすべて。ルートが先、その後は発見順
    pub items: Vec<DropItem>,
    /// CASCADE で、ルートの閉包の外から連鎖して消えるオブジェクトの説明（NOTICE 用。発見順）
    pub cascaded: Vec<String>,
}
/// 依存の閉包を求める。RESTRICT で連鎖が必要なら Err(2BP01)、ルートが内部依存で他のオブジェクトに要求されていれば Err(2BP01)。
/// アルゴリズムは §5.8.1。visible は describe_object に渡す
pub fn plan_drop(store: &CatalogStore, snap: &Snapshot, roots: &[ObjectAddress], behavior: DropBehavior, visible: &[Oid]) -> Result<DropPlan>;
```

### 4.4 キャッシュ（`catalog/cache.rs`）

`TableDef` に `indexes` を含めたので、**索引の変更は表の `TableDef` の変更**として扱う。キャッシュの無効化の仕組み（コミット時に `invalidate_all`、世代番号、`bypass_cache`）は **M2 §6.8.6 のまま変えない**（DDL は `catalog_dirty` を立て、コミットで全データベースの全エントリを消す）。追加するのは索引の逆引きだけ。

```rust
struct CacheInner {
    by_oid: HashMap<Oid, Arc<TableDef>>,                      // 表・シーケンス
    /// namespace → 名前 → (OID, 種別)。表・シーケンス・索引（M2 の by_name は表だけだった）
    by_name: HashMap<Oid, HashMap<String, (Oid, RelKind)>>,
    /// 索引の OID → 表の OID
    index_owner: HashMap<Oid, Oid>,
    generation: u64,
}
impl CatalogCache {
    pub fn get_relation_by_name(&self, nsp: Oid, name: &str) -> Option<(Oid, RelKind)>;   // get_by_name を置き換える
    pub fn get_index_owner(&self, index_oid: Oid) -> Option<Oid>;
    /// def を by_oid に、def と def.indexes の全部を by_name と index_owner に入れる（世代の比較は M2 のまま）
    pub fn insert(&self, def: Arc<TableDef>, built_at_gen: u64);
}
```

- 索引の名前だけを先に引かれた場合（`DROP INDEX x`）は `by_name` に載っていないので、`relation_in_namespace` が `pg_class` を引き、`index_by_oid` が表を読み込んで `insert` する（以後はキャッシュに載る）。
- 「存在しない」ことはキャッシュしない（M2 のまま）。

### 4.5 `load_table_def` の手順（`catalog/store.rs`）

`TableDef` を、`pg_class`・`pg_attribute`・`pg_attrdef`・`pg_constraint` に加えて `pg_index`・`pg_depend`・`pg_sequence` から組み立てる。すべて**与えられたスナップショット**で読む。釘付けのシステムカタログは M2 のまま `catalog_table_def` を返す。

```
load_table_def(snap, oid) -> Option<TableDef>
  1. pg_class を oid で 1 行読む（なければ None）。relkind: 'r' → 表、'S' → シーケンス、'i' → Ok(None)、それ以外 → Error::corrupted
  2. 共通: name、namespace → schema 名、relfilenode / reltablespace → locator（db_oid は self.db_oid）
  3. 列: pg_attribute（attrelid = oid、attnum > 0、attisdropped でない）を attnum 順に。
     ColumnDef { name, attnum, ty = (atttypid, atttypmod), not_null = attnotnull, default（4）, identity = attidentity }
     行数が relnatts と違えば Error::corrupted（M2 のまま）
  4. 既定値: pg_attrdef（adrelid = oid）の (adnum → adbin)（M2 のまま）
  5. 'S' の場合は 6 へ。表の場合:
     a. CHECK: pg_constraint（conrelid = oid、contype = 'c'）を名前順（M2 のまま。relchecks と数が違えば corrupted）
     b. 索引: pg_index（indrelid = oid）の行を集め、その indexrelid の pg_class（1 回の走査で OID の集合に含まれる行を集める）から
        name / namespace / relfilenode / reltablespace を、pg_constraint（conrelid = oid、contype in ('p','u')）から conindid → (oid, name) を引き、
        IndexDef { oid = indexrelid, name, namespace, table_oid = oid, locator, unique = indisunique, primary = indisprimary,
                   constraint = conindid が一致する制約, columns }
        columns の i 番目 = IndexColumn { attnum = indkey[i], opclass = indclass[i],
                   opfamily = opclass_by_oid(indclass[i])?.family（静的な表。なければ Error::internal "unknown operator class"）,
                   (descending, nulls_first) = IndexDef::flags_from_indoption(indoption[i]) }
        indexprs / indpred が NULL でなければ Error::corrupted（M4 は式・部分索引を作らない）
        OID の昇順に並べる。indnatts と列数が違う、indkey の attnum が表の列にない → Error::corrupted
     c. identity_seqs: pg_depend（refclassid = pg_class、refobjid = oid、refobjsubid > 0、deptype = 'i'、classid = pg_class）の
        (objid = シーケンスの OID, refobjsubid = attnum)。attidentity のある列と 1 対 1 でなければ Error::corrupted
     d. 戻り値: kind = Table、indexes、sequence = None、identity_seqs
  6. 'S' の場合: pg_sequence（seqrelid = oid）から SequenceParams { type_oid = seqtypid, start, increment, min, max, cache, cycle,
     owned_by }。owned_by は pg_depend（classid = pg_class、objid = oid、refclassid = pg_class、deptype in ('a','i')）の
     (refobjid, refobjsubid)（なければ None）。columns は pg_attribute（last_value / log_cnt / is_called）、
     kind = Sequence、indexes = []、checks = []、identity_seqs = []
```

**コスト**: M2 は 1 つの表の読み込みで 4 つのカタログを順次走査した。M4 は表で `pg_index`、`pg_depend` を足した 6 つ（索引があればその `pg_class` の走査 1 回）。カタログは小さく、結果はキャッシュされるので許容する（カタログのインデックスは作らない。00 の D-23）。`pg_depend` は DDL のたびに増えるが、1 表あたり数行〜十数行（1 万表でも十万行台で、1 回の走査は数十 ms。**ミス時のコスト**であり、キャッシュが効くので、問題になれば M5 で `pg_depend` の索引を作る）。

### 4.6 Bound の型（この章が持つもの）

00 §7 の名前を使い、フィールドを確定する。`analyzer/bound.rs` の `BoundDdl` に属する（A が型を置き、C1 が中身を決める）。**00 から変えるのは `BoundIndexConstraint.name` と `BoundCreateTable` の 2 フィールドの追加**（00 への変更提案 1、2）。

```rust
pub struct BoundCreateTable {
    pub schema: String, pub name: String, pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,             // PRIMARY KEY の列は not_null = true に解決済み。identity は 08 が設定
    pub checks: Vec<CheckDef>,
    pub constraints: Vec<BoundIndexConstraint>,       // PRIMARY KEY が先。重複は統合済み（§6.9）
    pub sequences: Vec<BoundCreateSequence>,          // 08
    pub options: Vec<RelOption>,                      // ★ WITH (...)。ddl が検証する（§5.9）
    /// ★ 既定値の式が regclass 定数で指す既存のリレーション: (attnum, リレーションの OID)。解析済みの式から集める（§5.1 の手順 6）
    pub default_refs: Vec<(i16, Oid)>,
}
pub struct BoundIndexConstraint {
    /// 明示された CONSTRAINT 名。None なら実行時に ChooseIndexName で決める（D07-4）
    pub name: Option<String>,
    pub kind: IndexConstraintKind,                    // PrimaryKey | Unique
    pub columns: Vec<i16>,                            // attnum。重複なし
    pub options: Vec<RelOption>,                      // WITH (fillfactor) は索引のオプション
    pub span: Span,
}
/// WITH (name = value) の 1 項目。value は字面（文字列リテラルは引用符を外した中身、数値・識別子はそのまま）。値なしは None（"true" と同じ扱い）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelOption { pub namespace: Option<String>, pub name: String, pub value: Option<String> }

pub struct BoundCreateIndex {
    pub table: Arc<TableDef>,
    /// 明示された索引名（修飾なし。スキーマは常に表と同じ）。None なら ChooseIndexName
    pub name: Option<String>,
    pub unique: bool, pub if_not_exists: bool, pub concurrently: bool,
    /// アクセスメソッド名（小文字化済み）。既定は "btree"。btree 以外は ddl が 0A000 / 42704
    pub method: String,
    pub columns: Vec<BoundIndexColumn>,
    pub options: Vec<RelOption>,
}
pub struct BoundIndexColumn {
    pub attnum: i16,
    /// 明示された演算子クラス（修飾は pg_catalog だけ許す）。None なら既定
    pub opclass: Option<String>,
    pub descending: bool,
    /// NULLS FIRST / LAST の省略は「DESC なら先頭、ASC なら末尾」に解決済み（§3.4）
    pub nulls_first: bool,
    pub span: Span,
}

pub struct BoundDropTable { pub tables: Vec<Arc<TableDef>>, pub missing: Vec<String>, pub behavior: DropBehavior }
pub struct BoundDropIndex {
    pub indexes: Vec<Arc<IndexDef>>,                  // 重複は除去済み
    pub missing: Vec<String>,
    pub behavior: DropBehavior,
    pub concurrently: bool,
}

pub enum AlterTarget { Found(Arc<TableDef>), /* IF EXISTS で存在しなかった表の名前。NOTICE を出して何もしない */ Missing(String) }
pub struct BoundAlterTableAddConstraint { pub target: AlterTarget, pub constraint: BoundIndexConstraint }
pub struct BoundAlterTableOwner { pub target: AlterTarget, pub new_owner: OwnerSpec }
pub enum OwnerSpec { Name(String), CurrentUser, SessionUser }       // CURRENT_ROLE は CurrentUser と同じ扱い

pub struct BoundTruncate { pub tables: Vec<Arc<TableDef>>, pub restart_identity: bool, pub cascade: bool }

pub struct BoundVacuum {
    /// VACUUM（真）か、ANALYZE だけ（偽）か。VACUUM ANALYZE は vacuum = true、analyze = true
    pub vacuum: bool, pub analyze: bool,
    pub options: Vec<VacuumOption>,
    /// 空なら「全部」（何もしない）
    pub targets: Vec<VacuumTarget>,
}
pub struct VacuumOption { pub name: String, pub value: Option<String> }
pub struct VacuumTarget {
    pub name: String,
    /// 表なら Some。索引・シーケンスなら None（実行時に WARNING を出して飛ばす）。存在しなければアナライザが 42P01
    pub table: Option<Arc<TableDef>>,
    /// ANALYZE の列リスト（表の列に存在することは解析済み。42703）
    pub columns: Vec<String>,
}
```

アナライザの共通の検査（呼び出し側の責任、DDL の前）:

- `DROP TABLE` / `TRUNCATE` / `CREATE INDEX` / `ALTER TABLE` の対象名の解決は `relation_kind` で行い、**種類が違えば `42809`**: DROP TABLE の対象が索引なら `"x" is not a table`、HINT `Use DROP INDEX to remove an index.`、シーケンスなら HINT `Use DROP SEQUENCE to remove a sequence.`。DROP INDEX の対象が表なら `"x" is not an index`、HINT `Use DROP TABLE to remove a table.`。TRUNCATE は種類が違えば `"x" is not a table`（HINT なし。実測）。CREATE INDEX の対象がシーケンスなら `cannot create index on relation "x"` DETAIL `This operation is not supported for sequences.`、索引なら `cannot open relation "x"` DETAIL `This operation is not supported for indexes.`。ALTER TABLE ADD の対象がシーケンスなら `ALTER action ADD CONSTRAINT cannot be performed on relation "x"` DETAIL `This operation is not supported for sequences.`（索引は `... for indexes.`）。
- 存在しない名前: `DROP TABLE` は `table "x" does not exist`（42P01）、IF EXISTS なら `missing` に積む。`DROP INDEX` は `index "x" does not exist`（42704）。`TRUNCATE` / `CREATE INDEX` / `ALTER TABLE` は `relation "x" does not exist`（42P01）。`ALTER TABLE IF EXISTS` は `AlterTarget::Missing`。

### 4.7 `ddl/` の公開 API と `DdlCtx` の使い方

```rust
// ddl/mod.rs（00 §14.6 の署名のまま）
pub fn execute(ctx: &mut DdlCtx<'_>, ddl: BoundDdl) -> Result<String> {
    match ddl {
        BoundDdl::CreateTable(b) => table::create_table(ctx, &b),            // "CREATE TABLE"
        BoundDdl::DropTable(b) => table::drop_table(ctx, &b),                // "DROP TABLE"
        BoundDdl::CreateIndex(b) => index::create_index(ctx, &b),            // "CREATE INDEX"
        BoundDdl::DropIndex(b) => index::drop_index(ctx, &b),                // "DROP INDEX"
        BoundDdl::AlterTableAddConstraint(b) => constraint::add_constraint(ctx, &b),   // "ALTER TABLE"
        BoundDdl::AlterTableOwner(b) => constraint::alter_owner(ctx, &b),    // "ALTER TABLE"
        BoundDdl::Truncate(b) => truncate::truncate(ctx, &b),                // "TRUNCATE TABLE"
        BoundDdl::Vacuum(b) => vacuum::vacuum(ctx, &b),                      // "VACUUM" / "ANALYZE"
        BoundDdl::CreateSequence(b) => sequence::create_sequence(ctx, &b),   // 08
        BoundDdl::AlterSequence(b) => sequence::alter_sequence(ctx, &b),     // 08
        BoundDdl::DropSequence(b) => sequence::drop_sequence(ctx, &b),       // 08
    }
}
```

`ddl/vacuum.rs` を足す（00 §4 の `ddl/` の一覧は `table.rs / index.rs / constraint.rs / sequence.rs / truncate.rs / depend.rs`。VACUUM を `truncate.rs` に同居させず分けるだけで、00 の署名は変わらない）。

**`DdlCtx`（00 §14.6）のフィールドの使い方**と、ここで足す補助メソッド:

| フィールド | 使い方 |
|---|---|
| `cluster` | `cluster.storage()`（`TableStore`: `create_storage`、`begin_scan_all`、`tuple_state`、`unlink_storage` は session が呼ぶ）、**`cluster.stack().index`**（`IndexStore`: `init_index`、`build`）、`cluster.oid_allocator()`、`cluster.txn_manager()` |
| `db` | `db.catalog`（`CatalogStore`）、`db.shared`（`SharedCatalogStore`: ロールの名前解決）、`db.oid` |
| `snapshot` | この文のスナップショット（`curcid = txn.cid`）。**このコマンドで書いた行は見えない**（D07-3） |
| `catalog` | `StatementCatalog`（`relation_kind` など。**このコマンドで作ったオブジェクトは見えない**） |
| `txn` | `txn.write_ctx()`（`cid_used` を立てる）、`pending_creates`、`pending_unlinks`、`catalog_dirty` |
| `role_oid` | 所有者（`relowner`） |
| `notices` | NOTICE / WARNING を積む |
| `type_env` | 重複のキーの文字列化（`output_text`）に使う |

```rust
impl DdlCtx<'_> {
    /// ライターロックを持っていなければ内部エラー（VACUUM / ANALYZE 以外は session が先に取る。§5.0）
    fn write_ctx(&mut self) -> Result<WriteCtx>;
    /// create_storage（SMGR_CREATE を WAL に挿入してファイルを作る）+ pending_creates に積む。この順序を守る（作ってから積むと、積む前の失敗でファイルが残る）
    fn create_file(&mut self, w: &WriteCtx, locator: RelFileLocator) -> Result<()>;
    /// 消すファイルを pending_unlinks に積む（コミットで消える。ロールバックでは何も起きない）
    fn schedule_unlink(&mut self, locator: RelFileLocator);
    /// カタログを変えたことを記録する（txn.catalog_dirty = true）。DDL の最後に必ず呼ぶ
    fn mark_catalog_dirty(&mut self);
    fn notice(&mut self, severity: Severity, sqlstate: SqlState, message: String, detail: Option<String>);
    fn check_interrupts(&self) -> Result<()>;           // 長いループ（ヒープの全走査、ソート）が行ごとに呼ぶ（00 §4.3 の 4）
}
```

`DdlCtx` に **2 つのフィールドを足す**（00 への変更提案 2）: `in_transaction_block: bool`（明示ブロックまたは複数文の Simple Query の暗黙のブロック。`SET LOCAL` や SAVEPOINT の判定と同じ値。`CONCURRENTLY` と VACUUM の `25001` に使う）と、`interrupts: &'a InterruptFlag`（`check_interrupts` の元）。

`Notice` の構築は session.rs の `Notice::new`（現在は private）を `pub(crate)` にして使う（`with_detail` / `with_hint` を足す。S への依頼）。

---

## 5. 処理の流れ

### 5.0 すべての DDL に共通の前提

**session が `ddl::execute` を呼ぶまで**（M2 §5.2、M3 §5.2 のとおり。この章は変えない。S の作業）:

```
a. 生のパース木の種類で「書く文」を判定する（アナライズより前）。タグは下の表。書く文なら:
     読み取り専用のトランザクションなら 25006 `cannot execute <タグ> in a read-only transaction`（ライターロックを取る前に）
     txn.writer がなければ begin_write（XID を割り当てる）
b. ストレージバリア（共有）を取る。generation → snapshot → StatementCatalog → analyze
c. ddl::execute(&mut DdlCtx { .. }, bound)
d. バリアを解放 → CCI（cid を進める）→ 暗黙のトランザクションならコミット
```

| 文 | 「書く文」か | 読み取り専用での 25006 のタグ | ライターロック・XID |
|---|---|---|---|
| `CREATE TABLE` / `DROP TABLE` | はい | `CREATE TABLE` / `DROP TABLE` | 取る |
| `CREATE [UNIQUE] INDEX`（`CONCURRENTLY` を含む） | はい | `CREATE INDEX` | 取る |
| `DROP INDEX` | はい | `DROP INDEX` | 取る |
| `ALTER TABLE`（ADD CONSTRAINT / OWNER TO） | はい | `ALTER TABLE` | 取る |
| `TRUNCATE` | はい | `TRUNCATE TABLE`（実測） | 取る |
| `CREATE / ALTER / DROP SEQUENCE` | はい（08） | `CREATE SEQUENCE` など | 取る |
| **`VACUUM` / `ANALYZE`** | **いいえ**（実測: 読み取り専用のトランザクションでも成功） | — | **取らない**（XID を使わない） |

**DDL 関数の規約**:

1. **途中で失敗したら `Err` を返すだけ**。ローカルな取り消し（ファイルの削除、カタログの行の取り消し）は書かない。session が失敗した文のトランザクションを中断し（暗黙なら即、ブロックなら Failed → ROLLBACK）、**`pending_creates` のファイルをアボートの手順で消す**（M3 §5.3）。カタログの行・ヒープの変更は xmin が中断した XID なので誰にも見えない。ファイルを作る前にしか起きない失敗でも、作った後に起きる失敗でも、同じ。
2. **ファイルを作る順序**: `create_file`（`SMGR_CREATE` の WAL → ファイル作成 → `pending_creates` に積む。M3 §5.4）→ そのファイルに書く（`init_index` / `build` / ヒープの行）→ カタログの行。**ファイルがカタログより先**（カタログに行があるのにファイルがない状態を作らない）。
3. **`pending_unlinks` に積むのは、消すものが確定してから**（`drop_objects` の後）。コミットで消える（M3 §5.3 の手順 3。コミットレコードの `rels` に載る）。
4. **WAL を書く操作はすべて、既存の層が書く**（`create_storage`: `SMGR_CREATE`、`init_index` / `build`: `BTREE_PAGES`、カタログの行: `HEAP_INSERT` / `HEAP_UPDATE` / `HEAP_DELETE`）。DDL 自身は WAL のレコードを定義しない。
5. **`w = ctx.write_ctx()?` を DDL の最初の書き込みの直前に 1 度だけ取り**、同じ `WriteCtx` を使う（1 つのコマンドの `cid` は 1 つ）。
6. **同じコマンドで挿入した行は、このコマンドのスナップショットから見えない**（D07-3）。`ctx.catalog` / `ctx.snapshot` で「いま作った」オブジェクトを引かない。必要な情報（OID、`TableDef`、`IndexDef`）は手元の値から組み立てる。
7. **最後に `ctx.mark_catalog_dirty()`**（カタログを変える DDL。VACUUM / ANALYZE を除く）。
8. 長いループ（ヒープの全走査、ソート、重複の検出）は行ごとに `ctx.check_interrupts()?`。

### 5.1 CREATE TABLE（PRIMARY KEY / UNIQUE / SERIAL の組み込み）

M2 §5.4 の手順に、制約が所有する索引と依存を足す。**空の表なので索引は `init_index` だけ**（`build` はしない）。

```
create_table(ctx, b: &BoundCreateTable) -> "CREATE TABLE"
  1. reloptions の検証（§5.9）: validate_reloptions(Table, &b.options)? と、各 b.constraints[i].options を Index として。
     （PostgreSQL は解析のあとの最初の段階で検査する。構文エラー・列の重複より後、名前の衝突より前）
  2. nsp = db.catalog.namespace_oid(snap, &b.schema)?（なければ 3F000 `schema "x" does not exist`。M2 のまま）
  3. 名前の衝突: ctx.catalog.relation_kind(Some(&b.schema), &b.name)?（表・索引・シーケンスのどれでも）
       Some かつ if_not_exists → NOTICE `relation "t" already exists, skipping`（SQLSTATE 42P07）を積んで "CREATE TABLE" を返す
       Some → Err 42P07 `relation "t" already exists`
  4. OID: oids = db.catalog.allocate_table_oids(alloc, &TableOidRequest { n_attrdefs, n_checks, n_index_constraints })?
       （table → attrdef（列順）→ CHECK（b.checks の順）→ 索引制約ごとに（索引、制約）。シーケンスの OID は 08 が手順 6 で後から取る。§3.10）
  5. w = ctx.write_ctx()?
  4b. シーケンスの OID（b.sequences の順。08 §5.7 の B。レビュー対応 R-07。名前は解析時に 08 が `catalog::naming::choose_relation_name` で決め終えている）:
       seq_oids[i] = db.catalog.get_new_relation_oid(alloc)?（表・索引・制約の OID の後に取る）
       列の補正（08 §5.7 の C）: SERIAL の列の default = nextval('{seq_oid}'::regclass) のテキスト、not_null = true。IDENTITY の列は identity と not_null = true
       （この補正は create_table が b.columns のコピーに対して行う。手順 8 以降の columns は補正後）
  6. シーケンス（b.sequences の順。08）: for (i, s) in b.sequences.iter().enumerate():
       ddl::sequence::create_with_oid(ctx, s, seq_oids[i], Some((oids.table, attnum_of(s))))?
       （08 §5.4 の関数。ファイル作成（pending_creates）・SequenceStore::init（SEQ_LOG）・CatalogStore::create_sequence
        （pg_class(S)・pg_attribute・pg_sequence、`NewSequence.owned_by_deptype` が Some のときの「シーケンス → 列」の a / i の pg_depend）までを行う。
        表の行はまだないので pg_depend は表の OID を指すだけでよい。`create_sequence_for_table` という別の関数は作らない）
  7. 表のファイル: table_locator = (DEFAULTTABLESPACE_OID, db.oid, RelFileNumber(oids.table))。ctx.create_file(&w, table_locator)?
  8. 暫定の TableDef を作る（表の行がまだ見えないので、IndexHandle を作るためだけに使う）:
       provisional = TableDef { oid: oids.table, namespace: nsp, schema: b.schema, name: b.name, kind: Table, locator: table_locator,
                                columns: b.columns.clone(), checks: b.checks.clone(), indexes: vec![], sequence: None, identity_seqs: vec![] }
       （IndexHandle::from_def は列の名前・型しか見ないので、indexes と identity_seqs は空でよい）
  9. 索引制約ごと（b.constraints の順。PRIMARY KEY が先）:
       a. キー列の名前 colnames = choose_index_column_names(キー列の列名)、型 = 表の列の型
       b. 索引の名前 name_i:
            明示名 → 衝突を調べる: taken ∪ ctx.catalog.relation_kind(Some(schema), name) があれば Err 42P07 `relation "x" already exists`
            なし   → choose_index_name(&b.name, nsp, &colnames, primary, is_constraint = true, &taken)（§5.10。taken は手順 9 で決めた名前、表の名前、
                     シーケンスの名前、b.checks の名前の集合。いずれもカタログにまだ見えない名前）
            制約の名前 = name_i（PostgreSQL は索引と同じ名前）。taken に追加
       c. opclass: 各キー列で resolve_opclass(column.ty, None)?（§6.6。なければ 42704 `data type xid has no default operator class for access method "btree"`）
       d. IndexDef { oid = oids.index_constraints[i].0, name_i, namespace: nsp, table_oid, locator = (DEFAULTTABLESPACE_OID, db.oid, RelFileNumber(oid)),
                     columns（opclass、opfamily、descending = false、nulls_first = false）, unique: true, primary, constraint: Some(IndexConstraintRef { oid: oids.index_constraints[i].1, name: name_i }) }
       e. ctx.create_file(&w, d.locator)?、indexes.init_index(&w, &IndexHandle::from_def(&d, &provisional))?（メタページと空のルート葉。BTREE_PAGES）
 10. 追加の依存 extra_depends:
       - 手順 4b のシーケンス s（owned_by = 列 a）で、列 a に既定値がある（SERIAL）なら (attrdef(列 a の oid), relation(seq_oids[i]), 0, Normal)
       - b.default_refs の各 (attnum, rel_oid)（同じ組は 1 つに）: (attrdef(attnum の oid), relation(rel_oid), 0, Normal)
 11. db.catalog.create_table(&w, snap, &NewTable { oid, namespace, name, owner: ctx.role_oid, columns: b.columns.clone(), checks, attrdef_oids, constraint_oids,
                                                   indexes: [NewIndex { .. stats: EMPTY_INDEX_STATS }], extra_depends })?
       （pg_class の表の行は relhasindex = !indexes.is_empty() で最初から最終形。D07-3）
 12. ctx.mark_catalog_dirty(); Ok("CREATE TABLE")
```

- **PRIMARY KEY の列の `attnotnull`**: アナライザが `columns[i].not_null = true` に解決してあるので、`pg_attribute` に最初から `t` で入る（更新しない）。
- **`attidentity`**: `ColumnDef.identity`（08 が設定）から `pg_attribute` に入る。IDENTITY 列の NOT NULL も 08 が `not_null = true` にする（実測: `attnotnull = t`、`atthasdef = f`）。
- **失敗時**: 手順 6〜9 のファイルは `pending_creates` に載っているので、アボートで消える（§5.0 の 1）。

**WAL の並び**（コミットまで）: `SMGR_CREATE`（シーケンス）→ `SEQ_LOG`（08）→ `SMGR_CREATE`（表）→ `SMGR_CREATE`（索引）→ `BTREE_PAGES`（索引の初期化）→ `HEAP_INSERT` × カタログの行 → コミットレコード。

### 5.2 CREATE INDEX（ヒープ全走査 → ソート → 一括構築）

```
create_index(ctx, b: &BoundCreateIndex) -> "CREATE INDEX"
  1. concurrently && ctx.in_transaction_block → Err 25001 `CREATE INDEX CONCURRENTLY cannot run inside a transaction block`（実測の文言）。最初に判定する
  2. 対象の表の検査: b.table.kind == Table（シーケンスなら 42809。アナライザが済ませる）。
     b.table.is_system_catalog() → 42501 `permission denied: "pg_class" is a system catalog`（実測）
  3. アクセスメソッド: b.method == "btree" 以外:
       hash / gist / spgist / gin / brin → Err 0A000 `index access method "hash" is not supported yet`
       それ以外 → Err 42704 `access method "nosuch" does not exist`（実測）
  4. 列の検査（PostgreSQL の ComputeIndexAttrs の順。名前の衝突より前）:
       列数 > INDEX_MAX_KEYS（32）→ Err 54011 `cannot use more than 32 columns in an index`
       各列 c: opclass = resolve_opclass(table.columns[c.attnum - 1].ty, c.opclass.as_deref())?（§6.6）
               collation = builtin 型の typcollation（100 / 950 / 0）
  5. reloptions の検証: validate_reloptions(Index, &b.options)?（§5.9）
  6. 索引の名前:
       colnames = choose_index_column_names(列名の並び)    // 索引の attname。重複は a, a1, a2 …
       明示名: relation_kind(Some(table.schema), name) が Some → if_not_exists なら NOTICE `relation "x" already exists, skipping`（42P07）を積んで "CREATE INDEX"、
               そうでなければ Err 42P07 `relation "x" already exists`
       なし:   choose_index_name(&table.name, table.namespace, &colnames, primary = false, is_constraint = false, taken = {})（§5.10。IF NOT EXISTS で名前なしは構文エラー（03）。実測: `create index if not exists on t (a)` は 42601）
  7. 索引の OID と IndexDef を決める:
       oid = db.catalog.get_new_relation_oid(alloc)?、locator = (DEFAULTTABLESPACE_OID, db.oid, RelFileNumber(oid))
       d = IndexDef { oid, name, namespace: table.namespace, table_oid: table.oid, locator, columns, unique: b.unique, primary: false, constraint: None }
       handle = IndexHandle::from_def(&d, &table)
  8. w = ctx.write_ctx()?; ctx.create_file(&w, locator)?; indexes.init_index(&w, &handle)?
  9. 構築: stats = build_from_heap(ctx, &w, &table, &d, &handle)?        // §5.2.1
 10. カタログ: db.catalog.create_index(&w, snap, &NewIndex { .. stats }, mark_table_indexed = true)?
       （表の relhasindex が false のときだけ、前のコマンドで作られた表の pg_class の行を update_class_row で true にする）
 11. ctx.mark_catalog_dirty(); Ok("CREATE INDEX")
```

**IF NOT EXISTS の順序**（実測の推定: `index_create` の中で名前を調べる）: 手順 4（列・opclass の検査）が手順 6（名前）より先。列が存在しない（アナライザの 42703）、opclass がない（42704）エラーが、名前の重複による NOTICE より優先する。

#### 5.2.1 `build_from_heap`（CREATE INDEX と ALTER TABLE ADD が共有。`ddl/index.rs`）

```rust
/// ヒープ（可視性判定なし）を全走査し、索引に入れるべき (key, tid) を集めてソートし、IndexStore::build で一括構築する。
/// 戻り値は pg_class.relpages / reltuples に書く BuildStats
pub(crate) fn build_from_heap(ctx: &mut DdlCtx<'_>, w: &WriteCtx, table: &TableDef, index: &IndexDef, handle: &IndexHandle) -> Result<BuildStats>;
```

```
 1. rel = RelHandle::from_table(table)（table.indexes はこの走査に使わない）。own = ctx.txn.xid
 2. scan = storage.begin_scan_all(&rel)?; loop { t = storage.scan_next(&mut scan)?; ctx.check_interrupts()?;
      state = storage.tuple_state(&t, own)?
      分類（D07-7）:
        InsertAborted                                  → 索引に入れない
        Live                                           → 入れる（live = true）
        DeadCommitted | DeletedBySelf                  → 入れる（live = false。古いスナップショット・同じトランザクションの後続の文が見うる版）
        InsertInProgress(_) | DeleteInProgress(_)      → Err 内部エラー（他トランザクションの途中の版。単一ライターロックを持つので起きない。M5 で待ちに変える）
      key = index.columns.map(|c| t.row[c.attnum - 1].clone())        // NULL を含みうる
      entries.push(Entry { key, tid: t.tid, live }) }
 3. ソート: entries.sort_by(|a, b| cmp_entries(index, a, b))
       cmp_entries = 列ごとに types::cmp::cmp_with_nulls(a.key[i], b.key[i], col.descending, col.nulls_first) を順に、全部 Equal なら tid の昇順
       （B+Tree の木の順序そのもの。00 §13.5 の「比較: 全キー + ヒープ TID」。比較の関数は 06 が公開する `storage::btree::cmp_keys(handle, a, b)` があればそれを使う）
 4. 一意索引（index.unique）なら、生きている版だけの重複を調べる（D07-7）:
       prev_live: Option<&Entry> = None
       for e in &entries where e.live && !e.key.iter().any(Datum::is_null) {          // NULL を含むキーは重複とみなさない
           ctx.check_interrupts()?
           if let Some(p) = prev_live && p.key が e.key と cmp_datum が全列 Equal {
               Err 23505 `could not create unique index "<索引名>"`、DETAIL `Key (a, b)=(1, x) is duplicated.`、
               with_table(schema, table).with_constraint(索引名)                         // 実測: SCHEMA / TABLE / CONSTRAINT NAME が付く
           }
           prev_live = Some(e)
       }
       Key の値は types::io::output_text(d, ty, ctx.type_env)（長い値も切り詰めない。実測: 5000 文字がそのまま出る）。複数列は `(a, b)=(1, x)`
       報告するキーは「ソート順で最初に見つかった重複」。同じキーが 3 つ以上あっても 1 回。
 5. stats = indexes.build(w, handle, &mut entries.into_iter().map(|e| (e.key, e.tid)), BuildUnique::No)?
 6. Ok(stats)（BuildStats { tuples, pages, levels }。tuples は入れた件数（死んだ版を含む））
```

- **メモリ**: 全エントリを `Vec` に持つ。1 エントリは `Vec<Datum>` + `Tid` + `bool`。`yuzhu.query_mem_limit` の対象外（00 の D-19。大きな表の構築は M6 の外部ソート）。目安: 100 万行・int4 1 列で約 100MB 未満。
- **`tuple_state` の意味（06 の H4 への依頼）**: `own` に自分の XID を渡す。**自分が挿入して削除していない版は `Live`**（`InsertInProgress` ではない）。`InsertInProgress(x)` / `DeleteInProgress(x)` は `x != own` のときだけ。`xmax` が中断した XID の版は `Live` 扱い。
- **WAL**: 構築のページは `build` が 32 ページずつ `BTREE_PAGES` で書く（00 §13.5）。ヒープの読み取りは WAL を書かない。

### 5.3 DROP INDEX

```
drop_index(ctx, b: &BoundDropIndex) -> "DROP INDEX"
  1. b.concurrently:
       ctx.in_transaction_block → Err 25001 `DROP INDEX CONCURRENTLY cannot run inside a transaction block`
       b.indexes.len() + b.missing.len() > 1 → Err 0A000 `DROP INDEX CONCURRENTLY does not support dropping multiple objects`
       b.behavior == Cascade → Err 0A000 `DROP INDEX CONCURRENTLY does not support CASCADE`          （実測）
  2. missing の各名前: NOTICE（SQLSTATE 00000）`index "x" does not exist, skipping`
  3. roots = b.indexes.map(|i| ObjectAddress::relation(i.oid))
     plan = plan_drop(&db.catalog, snap, &roots, b.behavior, &visible)?
       制約が所有する索引（pg_depend: 索引 →（i）→ 制約）はルートの内部依存で、制約が集合の外なので次のエラーになる（CASCADE でも同じ。実測）:
         2BP01 `cannot drop index d1_pkey because constraint d1_pkey on table d1 requires it`
         HINT `You can drop constraint d1_pkey on table d1 instead.`
  4. NOTICE: plan.cascaded（索引には起きない。§5.8.2）
  5. w = ctx.write_ctx()?; locators = db.catalog.drop_objects(&w, snap, &plan)?    // pg_index・pg_attribute・pg_class・pg_depend の行
  6. for l in locators { ctx.schedule_unlink(l) }
  7. ctx.mark_catalog_dirty(); Ok("DROP INDEX")
```

- **表の `relhasindex` は下ろさない**（D07-9）。
- **ファイルはコミットで消える**: 同じトランザクションで ROLLBACK すれば何も消えない（ファイルは触っていない）。コミット後のクラッシュは、コミットレコードの `rels` を REDO が消す。
- 失敗（2BP01 など）はファイルを作っていないので後始末なし。

### 5.4 ALTER TABLE ... ADD [CONSTRAINT n] PRIMARY KEY / UNIQUE

```
add_constraint(ctx, b: &BoundAlterTableAddConstraint) -> "ALTER TABLE"
  0. b.target が Missing(name) → NOTICE `relation "name" does not exist, skipping`（42P01）、"ALTER TABLE" を返す
  1. table = Found(t)。t.kind != Table（アナライザが 42809）。t.is_system_catalog() → 42501 `permission denied: "pg_class" is a system catalog`
  2. c = &b.constraint。reloptions の検証（Index）
  3. PRIMARY KEY で t.primary_key().is_some() → Err 42P16 `multiple primary keys for table "t" are not allowed`（位置なし。実測）
  4. 索引の名前:
       明示名: relation_kind(Some(schema), name) が Some → Err 42P07 `relation "x" already exists`（実測: 既存の制約と同じ名前を指定しても、索引の名前の衝突として 42P07）
               加えて constraint_names_of(t.oid) に同名があれば Err 42710 `constraint "x" for relation "t" already exists`
       なし:   colnames = choose_index_column_names(キー列名)、choose_index_name(&t.name, t.namespace, &colnames, primary, is_constraint = true, taken = {})
               （既存の `a1_c_key` があれば `a1_c_key1`。同じ列の UNIQUE を 2 回 ADD すると 2 つ作られる。実測。CREATE TABLE と違って統合しない）
  5. opclass: 各キー列 resolve_opclass(ty, None)?（42704）
  6. OID: 索引の oid = get_new_relation_oid、制約の oid = get_new_oid(pg_constraint)（索引の次）。IndexDef { unique: true, primary, constraint: Some(..) }
  7. w = ctx.write_ctx()?; ctx.create_file(&w, locator)?; indexes.init_index(&w, &handle)?
  8. stats = build_from_heap(ctx, &w, &t, &d, &handle)?          // 重複 → 23505 `could not create unique index "a1_pkey"` DETAIL `Key (a)=(3) is duplicated.`（実測）
  9. PRIMARY KEY なら NOT NULL の検査（D07-10: 構築の後）:
       各キー列で、まだ attnotnull でない列について、スナップショット（ctx.snapshot）で見える行を storage.begin_scan(&rel, snap) で全走査し、
       その列が NULL の行があれば Err 23502 `column "b" of relation "a1" contains null values`
         with_table(schema, table).with_column(列名)                  // 実測: SCHEMA / TABLE / COLUMN NAME が付く（CONSTRAINT は付かない）
       すべての列を 1 回の走査で調べる（列ごとに走査しない）。すでに attnotnull の列は調べない
 10. db.catalog.add_constraint(&w, snap, &NewIndex { .. stats }, &t)?
       （pg_class(i)・pg_attribute・pg_index・pg_constraint・pg_depend、表の relhasindex = true、PRIMARY KEY なら attnotnull = true の更新）
 11. ctx.mark_catalog_dirty(); Ok("ALTER TABLE")
```

- **9 の走査の snapshot**: 単一ライターなので、この文のスナップショットで足りる（PostgreSQL は「最新のスナップショット」。同じ）。ただし**自分が前の文で挿入した行は見える**（`cmin < curcid`）。
- **ロールバック**: 8 の索引ファイルは `pending_creates` にあり、失敗でも ROLLBACK でも消える。9 で失敗した場合、索引のカタログ行は書いていないが、ファイルは作っていたので消える（§5.0 の 1）。
- `ADD CONSTRAINT ... USING INDEX`、`DEFERRABLE`、`NULLS NOT DISTINCT`、`INCLUDE`、`CHECK` / `FOREIGN KEY` の ADD、`DROP CONSTRAINT`、`ADD COLUMN` などは、アナライザが `0A000` にする（00 の D-12。文言の例: `ALTER TABLE ... ADD CONSTRAINT USING INDEX is not supported yet`）。

### 5.5 ALTER TABLE ... OWNER TO

```
alter_owner(ctx, b: &BoundAlterTableOwner) -> "ALTER TABLE"
  0. Missing(name) → NOTICE `relation "name" does not exist, skipping`、"ALTER TABLE"
  1. 新しい所有者の解決: OwnerSpec::Name(n) → db.shared.role_by_name(snap, &n)?（なければ Err 42704 `role "x" does not exist`。実測）。
       CurrentUser / SessionUser → ctx.role_oid。（"none" は構文の段階で 42939 `role name "none" is reserved`。03）
       存在するロールならどれでもよい（pg_database_owner のようにログインできないロールも可。実測）
  2. t.is_system_catalog() → 42501 `permission denied: "pg_class" is a system catalog`（未検証: OWNER TO での文言。ADD CONSTRAINT と TRUNCATE では実測）
  3. 対象 = 表 t、t.indexes のすべて、db.catalog.owned_sequences(snap, t.oid) のシーケンス（PostgreSQL は索引と所有シーケンスの所有者も変える。実測: 索引）
     各 oid について、現在の relowner が新しい値と違うものだけ update_class_row(oid, relowner = new)（D07-3: 1 行 1 回）
     （全部同じなら何も書かない。PostgreSQL も同じ所有者への変更はエラーにしない）
  4. 1 行でも更新したら ctx.mark_catalog_dirty()。"ALTER TABLE"
```

- 権限の検査（所有者または superuser でなければ `42501 must be owner of table t`）は M4 にない（ロールは superuser だけ。M5）。
- `pg_depend` の `pg_shdepend`（所有者の依存）は持たない。

### 5.6 TRUNCATE

```
truncate(ctx, b: &BoundTruncate) -> "TRUNCATE TABLE"
  1. tables = b.tables の oid の重複を除く（`TRUNCATE t, t` は 1 回。実測）。順序は最初の出現順
     各表: kind != Table → 42809 `"x" is not a table`（アナライザ）、is_system_catalog() → 42501 `permission denied: "pg_class" is a system catalog`（実測）
     （これらの検査をすべての表について済ませてから 2 へ。途中まで実行して失敗しない）
  2. w = ctx.write_ctx()?
  3. 各表 T について（T の TableDef はアナライザが読んだもの。このトランザクションが前の文で変えた定義も反映している）:
       a. 新しい relfilenode: new_oid = db.catalog.get_new_relfilenumber(alloc)?; new_loc = (T.locator.spc_oid, db.oid, RelFileNumber(new_oid))
          ctx.create_file(&w, new_loc)?
       b. 旧ファイル: ctx.schedule_unlink(T.locator)
       c. db.catalog.truncate_relation(&w, snap, T.oid, new_oid)?      // pg_class: relfilenode = new、relpages = 0、reltuples = -1
       d. T の索引ごと I（T.indexes の順）について a〜c と同じ（新しい relfilenode、旧ファイルを schedule_unlink）に加えて:
            handle = IndexHandle { locator: new_loc, ..IndexHandle::from_def(I, &T) }
            indexes.init_index(&w, &handle)?                  // 空の索引（メタページと空のルート葉。BTREE_PAGES）
            pg_class の更新は relfilenode / relpages / reltuples を 1 回の update_class_row で（truncate_relation）
  4. b.restart_identity: 各表 T について db.catalog.owned_sequences(snap, T.oid) の各シーケンスを
       ddl::sequence::restart_owned_by_table(ctx, T.oid)?（08 §5.6。表が所有するシーケンス（a / i）ごとに SequenceStore::reset(.., restart_with = Some(start))。
       `owned_sequences` の呼び出しは 08 の側が行うので、ここで 1 本ずつ restart する必要はない。レビュー対応 R-07）
     シーケンスの更新はトランザクショナルではなくその場の上書き（08 の方針。PostgreSQL は ROLLBACK で戻る。差分: [07-Q9]）
  5. b.cascade は無視する（外部キーがない）。b.restart_identity = false（CONTINUE IDENTITY）が既定
  6. ctx.mark_catalog_dirty(); Ok("TRUNCATE TABLE")
```

- **MVCC と並行**: 旧ファイルはコミットまで残り、ほかのセッションの実行中の文は旧ファイルを読み続ける。コミット後、旧ファイルは `exclusive_barrier` の下で消える（M3 §5.3 の手順 3）。**PostgreSQL は TRUNCATE が ACCESS EXCLUSIVE ロックで読み手を待たせるが、yuzhu は待たせない**（M2-Q14 と同じ差分）。TRUNCATE をコミットした後に始まる文は、新しい空のファイルを読む。
- **同じトランザクションで作った表を TRUNCATE**: 最初のファイルは `pending_creates` と `pending_unlinks` の両方に載るが、コミットで 1 度、アボートで 1 度消えるだけ（commit と abort は排他）。
- **ROLLBACK**: 新しいファイル（表・索引）は `pending_creates` なのでアボートで消える。旧ファイルとカタログの旧い行が生きている。
- **`pg_class` の更新**: 表の行は「前の文で作られた行」（このコマンドで挿入した行ではない）。同じ文で 2 度更新しない（手順 1 で重複を除く）。
- **シーケンスを所有する表の TRUNCATE ... CONTINUE IDENTITY**（既定）はシーケンスに触れない（実測: truncate の後も採番が続く）。
- 読み取り専用のトランザクションは session が 25006（§5.0）。

### 5.7 VACUUM / ANALYZE

「何もせず成功」（00 の D-11、D07-13）。ライターロック・XID を取らず、カタログも WAL も触らない。

```
vacuum(ctx, b: &BoundVacuum) -> "VACUUM" | "ANALYZE"
  1. b.vacuum && ctx.in_transaction_block → Err 25001 `VACUUM cannot run inside a transaction block`（実測。複数文の Simple Query の暗黙のブロックでも）
     ANALYZE（b.vacuum == false）はブロック内でも成功（実測）
  2. オプションの検査（validate_vacuum_options。名前は VACUUM では VACUUM のオプション、ANALYZE では ANALYZE のオプションの集合）:
       VACUUM のオプション: analyze, verbose, freeze, full, disable_page_skipping, skip_locked, index_cleanup, truncate, parallel, process_main, process_toast, skip_database_stats, only_database_stats, buffer_usage_limit
       ANALYZE のオプション: verbose, skip_locked, buffer_usage_limit
       未知の名前 → Err 42601 `unrecognized VACUUM option "foo"` / `unrecognized ANALYZE option "foo"`（実測。ANALYZE に `analyze` を渡しても同じ）
       真偽値のオプション（verbose, freeze, full, analyze, disable_page_skipping, skip_locked, truncate, process_main, process_toast, skip_database_stats, only_database_stats）の値が
         on/off/true/false/1/0 以外 → Err 42601 `verbose requires a Boolean value`（実測）。index_cleanup は auto/on/off、これ以外 → `index_cleanup requires a Boolean value`（実測）
       parallel: 整数で 0〜1024、範囲外 → Err 42601 `parallel workers for vacuum must be between 0 and 1024`（実測）。full かつ parallel > 0 → Err 0A000 `VACUUM FULL cannot be performed in parallel`（実測）
       オプションの意味は持たない（VERBOSE の INFO も出さない。差分）
  3. b.targets の各対象: table == None（索引・シーケンス）→ WARNING（SQLSTATE 01000）`skipping "x" --- cannot vacuum non-tables or special system tables`
       （ANALYZE のみ: `cannot analyze non-tables or special system tables`。実測）。システムカタログも表なので成功（何もしない）
     列リストがあるのに b.analyze == false → Err 0A000 `ANALYZE option must be specified when a column list is provided`（実測）
     （対象が存在しない 42P01 `relation "x" does not exist`、列がない 42703 `column "zz" of relation "v1" does not exist` はアナライザ）
  4. Ok(if b.vacuum { "VACUUM" } else { "ANALYZE" })    // VACUUM ANALYZE のタグは VACUUM（実測）
```

`pgbench -i` の `vacuum analyze pgbench_branches`（`-I v`）と、pgbench 開始時の `vacuum pgbench_branches`（`tryExecuteStatement` なので失敗しても続行）が通る。

### 5.8 DROP TABLE（依存の連鎖）

```
drop_table(ctx, b: &BoundDropTable) -> "DROP TABLE"
  1. b.missing の各名前: NOTICE（SQLSTATE 00000）`table "x" does not exist, skipping`（M2 のまま）
  2. 各表 t: t.is_system_catalog() → Err 42501 `permission denied: "pg_class" is a system catalog`（M2 のまま。実測の PostgreSQL と同じ文言）
  3. roots = b.tables.map(|t| ObjectAddress::relation(t.oid))（重複は除去済み。実測: `drop table t, t` は成功）
     plan = plan_drop(&db.catalog, snap, &roots, b.behavior, &visible)?                // §5.8.1。RESTRICT で外から依存されていれば 2BP01
  4. plan.cascaded を NOTICE にする（§5.8.2）
  5. w = ctx.write_ctx()?; locators = db.catalog.drop_objects(&w, snap, &plan)?
  6. for l in locators { ctx.schedule_unlink(l) }                                       // 表・索引・所有シーケンスのファイル
  7. ctx.mark_catalog_dirty(); Ok("DROP TABLE")
```

#### 5.8.1 `plan_drop` のアルゴリズム（`catalog/depend.rs`）

PostgreSQL の `findDependentObjects` / `reportDependentObjects` を、`a` / `i` / `n` の 3 種に絞って移したもの。`key(addr) = (class_id, obj_id)`（`obj_sub` は無視）。

```
plan_drop(store, snap, roots, behavior, visible) -> DropPlan
  set: 発見順の IndexMap<key, DropItem>。work: キュー。normal: Vec<(依存元, 依存先)>
  add(addr): すでに set にあれば何もしない。なければ DropItem を作って set に入れ、work に積む
       DropItem は種類で作る: pg_class の行 → relkind で Table / Index / Sequence（locator、name、namespace）。pg_constraint → Constraint（conrelid → owner_table）。
       pg_attrdef → AttrDefault（adrelid → owner_table、adnum → attnum）。description = describe_object(addr)
       表を add したときは安全網として、その表の部品も直接のキーで add する（pg_constraint の conrelid、pg_attrdef の adrelid、
       pg_index の indrelid が指す索引の pg_class）。pg_depend に行が漏れていても取りこぼさない（二重の網。漏れは整合検査 §8.3 が検出する）
  1. roots を add する
  2. 閉包: work が空になるまで x = work.pop() として store.dependents_of(snap, x) の各行 row について
          Auto | Internal → add(row.dependent)                                      // 一緒に消える（静かに）
          Normal          → normal.push((row.dependent, row.referenced))
  3. ルートの内部依存の検査（PostgreSQL は最初にこれを行う。CASCADE でも同じ）:
       for r in roots: for row in store.references_of(snap, r) where row.deptype == Internal:
          if key(row.referenced) が set にない →
             Err 2BP01 `cannot drop {desc(r)} because {desc(row.referenced)} requires it`、HINT `You can drop {desc(row.referenced)} instead.`
  4. Normal 依存の処理: normal の各 (d, x) で key(d) が set にないもの（集合の中の依存元は問題にならない）について
          RESTRICT → blocked.push((d, x))
          CASCADE  → add(d) して cascaded に description を追加。手順 2 のループを回し、新しい Normal 依存が出なくなるまで繰り返す
  5. blocked が空でなければ Err 2BP01:
          message: ルートが 1 つ → `cannot drop {desc(root)} because other objects depend on it`、複数 → `cannot drop desired object(s) because other objects depend on them`
          DETAIL: blocked の各 (d, x) を `{desc(d)} depends on {desc(x)}` の行（改行区切り。pg_depend の物理順。100 行まで。超えたら末尾に
                  `and {n} other objects (see server log for list)`（未検証: 100 の閾値と文言））
          HINT `Use DROP ... CASCADE to drop the dependent objects too.`
  6. Ok(DropPlan { items: set の値（ルートが先）, cascaded })
```

**実測した結果**（PostgreSQL 17。この結果がテストの期待値）:

| 状況 | 結果 |
|---|---|
| `drop table o1`（`o1` の SERIAL シーケンス `o1_id_seq` を `o2` の既定値 `nextval('o1_id_seq')` が使っている） | `ERROR: cannot drop table o1 because other objects depend on it` / `DETAIL: default value for column id of table o2 depends on sequence o1_id_seq` / `HINT: Use DROP ... CASCADE to drop the dependent objects too.`（`o2` と `o3` の 2 つなら DETAIL は 2 行、`o2`、`o3` の順） |
| `drop table o1, o4`（o4 は無関係の表） | `ERROR: cannot drop desired object(s) because other objects depend on them`（DETAIL は上と同じ） |
| `drop table o1, o2`（`o2` も消す） | 成功（集合の中の依存元は問題にならない） |
| `drop table o1 cascade` | 成功。`NOTICE: drop cascades to default value for column id of table o2`。`o2` の列 `id` は既定値を失う（`pg_attrdef` の行が消え、`pg_attribute.atthasdef = f`）。2 つなら `NOTICE: drop cascades to 2 other objects` / `DETAIL: drop cascades to default value for column id of table o2` + `drop cascades to default value for column id of table o3` |
| 所有シーケンス・索引・制約・自分の既定値 | 静かに一緒に消える（NOTICE なし） |

#### 5.8.2 説明の文（`describe_object`）

| 対象 | 文 |
|---|---|
| 表 / 索引 / シーケンス | `table t` / `index t_pkey` / `sequence t_id_seq`（名前空間が `visible_namespaces()` になければ `table s.t`） |
| 列（`obj_sub > 0`） | `column id of table t` |
| 制約（`pg_constraint`） | `constraint t_pkey on table t` |
| 既定値（`pg_attrdef`） | `default value for column id of table t` |

CASCADE の NOTICE: `cascaded` が 1 件なら `NOTICE: drop cascades to <説明>`（DETAIL なし）、2 件以上なら `NOTICE: drop cascades to N other objects`（N = 件数）+ DETAIL に各件の `drop cascades to <説明>` を改行区切り（100 件まで）。SQLSTATE は 00000。

### 5.9 `WITH (fillfactor)` と reloptions

D07-14: 検証して捨てる。`ddl/mod.rs` の関数で、CREATE TABLE（表・制約の索引）、CREATE INDEX、ALTER TABLE ADD の `options` に使う。

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelOptTarget { Table, Index }
/// 22023 で検査する。値は保存しない
pub fn validate_reloptions(target: RelOptTarget, opts: &[RelOption]) -> Result<()>;
```

規則（実測した文言。すべて SQLSTATE 22023、PostgreSQL の挙動は表の `WITH (fillfactor=..)`、索引の `WITH (fillfactor=..)` とも同じ）:

| 条件 | メッセージ |
|---|---|
| `namespace` が `toast` | どの名前でも `unrecognized parameter "foo"`（M4 は toast の項目を持たない） |
| `namespace` が `toast` 以外（`public.fillfactor`） | `unrecognized parameter namespace "public"` |
| 同じ名前が 2 回 | `parameter "fillfactor" specified more than once` |
| 未知の名前（表）: `fillfactor` 以外 / （索引）: `fillfactor`、`deduplicate_items` 以外 | `unrecognized parameter "foo"` |
| `fillfactor` が整数でない（`abc`、値なし = `true`、`50.5`） | `invalid value for integer option "fillfactor": abc`（値なしは `true`） |
| `fillfactor` が 10〜100 の外 | `value 5 out of bounds for option "fillfactor"` + DETAIL `Valid values are between "10" and "100".` |
| `deduplicate_items`（索引）が真偽値でない | `invalid value for boolean option "deduplicate_items": x`（未検証: 文言）。受け取った値は捨てる |

- 値の形: 整数リテラル（`100`）、文字列（`'50'`。pgbench は `with (fillfactor=100)`、pg_dump は `WITH (fillfactor='70')` の形を出すことがある）のどちらも受け付ける。**差分**: PostgreSQL は `50.5` も整数オプションとして受け付ける（実測: `CREATE TABLE`）。yuzhu は `invalid value for integer option "fillfactor": 50.5` にする（[07-Q8]）。
- PostgreSQL にある `autovacuum_enabled`、`toast_tuple_target`、`parallel_workers`、`user_catalog_table`、`oids`（`oids=true` は `0A000 tables declared WITH OIDS are not supported`）などは M4 では `unrecognized parameter`（[07-Q8]）。

### 5.10 名前の自動生成（`catalog/naming.rs`）

PostgreSQL の `makeObjectName` / `ChooseRelationName` / `ChooseConstraintName` / `ChooseIndexName` / `ChooseIndexNameAddition` / `ChooseIndexColumnNames` の移植（`indexcmds.c` と `heap.c` にある関数。ソースの場所は未検証）。M2 の `make_object_name` は実測（63 バイトへの切り詰め）と一致することを確認済み。**実測の結果と一致することを `naming` の単体テストと slt で確認する**。

```rust
// catalog/naming.rs（analyzer と ddl が共有する。M2 の analyzer/ddl.rs の make_object_name をここへ移す）
/// `name1_name2_label`。63 バイト（MAX_IDENTIFIER_LENGTH）を超えるときは長い方の名前を 1 文字ずつ短くする（M2 の実装のまま。UTF-8 の途中で切らない）
pub fn make_object_name(name1: &str, name2: Option<&str>, label: &str) -> String;

/// 索引の列名の並び（attname）。重複は origname + "1"、"2" … （PostgreSQL の ChooseIndexColumnNames）
pub fn choose_index_column_names(column_names: &[&str]) -> Vec<String>;
/// 列名を '_' でつないだ文字列。連結が 63 バイトに達したら打ち切る（ChooseIndexNameAddition）
pub fn choose_index_name_addition(index_column_names: &[String]) -> String;

/// 衝突の判定。実装は ddl の中の小さな構造体（StatementCatalog と CatalogStore を使う）で、解析側も同じ trait を満たす
pub trait NameLookup {
    fn relation_exists(&self, nsp: Oid, name: &str) -> Result<bool>;        // 表・索引・シーケンス
    fn constraint_exists(&self, nsp: Oid, name: &str) -> Result<bool>;      // pg_constraint
}
pub fn choose_relation_name(name1: &str, name2: Option<&str>, label: &str, nsp: Oid, is_constraint: bool,
                            lookup: &dyn NameLookup, taken: &HashSet<String>) -> Result<String>;
pub fn choose_constraint_name(name1: &str, name2: Option<&str>, label: &str, nsp: Oid,
                              lookup: &dyn NameLookup, taken: &HashSet<String>) -> Result<String>;
pub fn choose_index_name(table: &str, nsp: Oid, index_column_names: &[String], primary: bool, is_constraint: bool,
                         lookup: &dyn NameLookup, taken: &HashSet<String>) -> Result<String>;
```

#### 5.10.1 `ChooseRelationName`

```
choose_relation_name(name1, name2, label, nsp, is_constraint, lookup, taken):
    modlabel = label; pass = 0
    loop:
        relname = make_object_name(name1, name2, modlabel)
        衝突なし ⇔ !taken.contains(relname) && !lookup.relation_exists(nsp, relname)?
                      && (!is_constraint || !lookup.constraint_exists(nsp, relname)?)
        衝突なし → return relname
        pass += 1; modlabel = format!("{label}{pass}")           // "key1"、"key2"、"idx1" …（label の後ろに数字）
```

- `choose_constraint_name` も同じ形で、衝突の判定は `lookup.constraint_exists(nsp, name) || taken.contains(name)`（リレーションの名前は見ない）。CHECK の自動名（`t_a_check`、衝突で `t_a_check1`）は、**M2 では同じ表の中の名前だけを見ていた（`used`）。M4 では名前空間の全制約（`constraint_exists`）を見る**（PostgreSQL と同じ。`analyzer/ddl.rs` の CHECK の命名を `choose_constraint_name` に置き換える。`analyzer/ddl.rs` の持ち主 Q1 への依頼。小さい変更。レビュー対応 R-09）。
- `taken` は「同じ文の中で先に決めたが、まだカタログから見えない名前」（§5.1 の手順 9）。

#### 5.10.2 `ChooseIndexName` と列名

```
choose_index_name(table, nsp, index_column_names, primary, is_constraint, lookup, taken):
    if primary:      choose_relation_name(table, None, "pkey", nsp, true, lookup, taken)               // t_pkey
    else:            choose_relation_name(table, Some(choose_index_name_addition(index_column_names)),
                                          if is_constraint { "key" } else { "idx" }, nsp, is_constraint, lookup, taken)
                                                                                                         // t_a_key / t_a_b_idx
```

- `choose_index_column_names(["a","a"]) = ["a", "a1"]`、`["a","a1","a"] = ["a","a1","a2"]`: 衝突したら `origname + 1, 2, ...` を、既出の名前と衝突しなくなるまで。元の名前を切り詰めて（`63 - 数字の桁数` バイトまで）から数字を付ける。
- `choose_index_name_addition`: 各列名を `_` でつなぎ、**文字列の長さが 63 に達したらそこで打ち切る**（`strlcpy(buf + buflen, name, NAMEDATALEN - buflen)` の動き）。

#### 5.10.3 実測した例（slt とテストの期待値）

| DDL | 結果 |
|---|---|
| `create table t2 (a int, b int, c text, primary key (a, b), unique (b, c), unique (a))` | 索引 `t2_pkey`、`t2_b_c_key`、`t2_a_key`。`conkey` は `{1,2}`、`{2,3}`、`{1}`（PRIMARY KEY が先に作られる） |
| `create index on t2 (a desc)` / `(a desc nulls last)` / `(a nulls first)` / `(a)` / `(a, b)` / `(a)` | `t2_a_idx`（`indoption` 3）、`t2_a_idx1`（1）、`t2_a_idx2`（2）、`t2_a_idx3`（0）、`t2_a_b_idx`、`t2_a_idx4`。**列・向きが違っても名前は `t_a_idx` からの連番** |
| `create index on e1 (a, a)` | 索引名 `e1_a_a1_idx`、`pg_attribute` の列名は `a`、`a1`。`indkey = 1 1` |
| 同じ名前のリレーションがすでにある: `create table t3_a_key (x int); create table t3 (a int unique)` | `t3_a_key1` |
| 同じ名前の**制約**がすでにある: `create table t4 (a int, constraint t4_a_key check (a>0), unique (a))` | 索引と制約が `t4_a_key1`（索引の名前は制約の名前とも衝突しないようにする） |
| `create table t5 (a int, constraint x unique(a))` の後で `create table t6 (a int, constraint x unique (a))` | 42P07 `relation "x" already exists`（明示名は連番にしない） |
| `create table t7 (a int, unique (a), unique(a))` | 索引は 1 つ（`t7_a_key`）。重複は統合（§6.9） |
| `create table a5 (a int primary key, constraint u2 unique (a))` | 索引・制約は 1 つ。**名前は `u2`、contype は `p`**（PRIMARY KEY の名前がなかったので、後の UNIQUE の名前が移る） |
| `create table a5 (a int, constraint u1 unique (a), constraint p1 primary key (a))` | 1 つ。名前は `p1`、contype は `p` |
| `create table a5 (a int unique, b int, unique (a, b), unique (b, a), unique (a))` | `a5_a_key`、`a5_a_b_key`、`a5_b_a_key`（最後の `unique (a)` は 1 つ目に統合） |
| 長い名前: 60 文字の表 `aaa…a` + `primary key (b)` + `c int unique` + `unique (long_column_name_number_one, long_column_name_number_two)` + `create index on t (c)` | `aaa…a`（58 文字）`_pkey`、`aaa…a`（57 文字）`_c_key`、`aaa…a`（29 文字）`_long_column_name_number_one_l_key`（長い方から削るので表名と列名の部分が 29 文字ずつ）、`aaa…a`（57 文字）`_c_idx`（いずれも 63 文字） |
| `create index e1_a on e1 (a)`（明示名）の後に `create index e1_a on e1 (b)` | 42P07 |
| `create index if not exists e1_a on e1 (a)` | NOTICE `relation "e1_a" already exists, skipping` |
| ALTER TABLE ADD で `a1_c_key` がすでにある | `a1_c_key1`（ADD UNIQUE (c) を 2 回） |
| `create index on e1 (a, a, ..)` を 32 列 | `54011 cannot use more than 32 columns in an index`（33 列）。32 列は成功し、列名は `a`、`a1` … `a31` |

### 5.11 DDL ごとのファイル・WAL・後始末のまとめ

| DDL | 作るファイル（`pending_creates`） | 消すファイル（`pending_unlinks`） | WAL（コミットまで） | ROLLBACK / 失敗 | コミット後のクラッシュ |
|---|---|---|---|---|---|
| CREATE TABLE（PK / UNIQUE / SERIAL あり） | 表、各索引、各シーケンス | — | `SMGR_CREATE` ×(1 + 索引 + シーケンス)、`BTREE_PAGES`（索引ごと）、`SEQ_LOG`（08）、`HEAP_INSERT` × カタログの行 | アボートで作ったファイルを消す。行は xmin が中断した XID で見えない | WAL の REDO がファイルとページを再現（M3） |
| CREATE INDEX | 索引 | — | `SMGR_CREATE`、`BTREE_PAGES`（32 ページずつ + 初期化）、`HEAP_INSERT` × カタログの行、`HEAP_UPDATE`（表の `relhasindex`。上げるときだけ） | 同上 | 同上 |
| DROP INDEX | — | 索引 | `HEAP_DELETE` × カタログの行 | 何も消えない（ファイルは触っていない）。行の `xmax` が中断した XID になる | コミットレコードの `rels` を REDO が消す |
| ALTER TABLE ADD PK / UNIQUE | 索引 | — | CREATE INDEX と同じ + `HEAP_UPDATE`（PK の `attnotnull`） | 同上 | 同上 |
| ALTER TABLE OWNER TO | — | — | `HEAP_UPDATE`（`pg_class` の `relowner`） | 行の更新が見えなくなる | — |
| TRUNCATE | 表と各索引の新しいファイル | 表と各索引の旧ファイル | `SMGR_CREATE`、`BTREE_PAGES`（索引の初期化）、`HEAP_UPDATE`（`pg_class`） | 新しいファイルを消す。旧ファイルと旧い行が生きている | 旧ファイルをコミットレコードの `rels` が消し、新しいファイルは REDO が再現 |
| DROP TABLE | — | 表、各索引、所有シーケンス | `HEAP_DELETE` × カタログの行（+ CASCADE の `HEAP_UPDATE`: 他の表の `atthasdef`） | 何も消えない | `rels` を REDO が消す |
| VACUUM / ANALYZE | — | — | なし（XID も取らない） | — | — |

**クラッシュで中断した DDL**（コミットレコードが WAL にない）が作ったファイルは、孤児として残る（M3 の D15 と同じ。害はない。M3-Q5）。REDO は `SMGR_CREATE` と `BTREE_PAGES` を再現するので、孤児のファイルは「作りかけ」の内容を持ちうるが、どのカタログの行からも参照されない。

---

## 6. モジュールごとの仕様

### 6.1 `catalog/schema.rs`

- §3.1 の 9 カタログの `static PG_*_COLUMNS: &[CatalogColumn]` と `oids` の定数、`CATALOGS` への追加。`CatalogColumn` の型 OID と `not_null` は §3.1 の表。
- **PostgreSQL 17 の列を実機の `pg_attribute` と突き合わせる**ことが 1 つめのテスト（`tests/slt/m4/catalog/catalog_columns_m4.slt`: 9 カタログの `attname`、`format_type(atttypid, atttypmod)`、`attnotnull`、`attnum` を PostgreSQL と同じ結果にする。M2 の `catalog_columns.slt` と同じ方式）。
- 単体テスト: `CATALOGS.len() == 22`、`natts`（`pg_index` 21、`pg_depend` 7、`pg_sequence` 8、`pg_language` 9、`pg_opfamily` 5、`pg_opclass` 9、`pg_amop` 9、`pg_amproc` 6、`pg_description` 4）、共有カタログが末尾 3 つ、`mapped` が M2 のまま（新しい 9 個は `false`）。

### 6.2 `catalog/rows.rs`

- §3.2〜§3.7 の行の組み立て（`ClassSpec` の拡張、`index_row`、`constraint_row`、`depend_row`、`sequence_row`、`index_attribute_rows`、`sequence_attribute_rows`、`opfamily_rows` など）。すべて `RowBuilder::new(catalog_oid).set("列名", ..)` で書く（M2 の方針。列の綴りの誤りを単体テストが見つける）。
- `attribute_row` は `AttributeSpec.identity` を受け取り、`attidentity` を `Datum::Char(b'a' | b'd' | 0)` にする。`index_attribute_rows` は `attribute_row` を `not_null = false`、`has_default = false`、`catalog_column = false` で呼ぶ（索引の列は `attcollation` を型から取る）。
- `initial_rows` の拡張（§3.1）、`builtin_canonical_bytes` の対象の拡張（§3.8）、`reference_columns` の拡張（§3.7）。
- 単体テスト: (1) 全カタログの初期行が `datum_matches_type` を満たす、(2) `dangling_references(params)` が空（`prolang` の例外を消した状態で）、(3) `index_row` が PostgreSQL の実測値（§3.4）と一致する（`indoption` の 4 通り）、(4) `constraint_row` の `connoinherit`（p / u は `t`、c は `f`）、(5) `builtin_hash` が表の書き順に依らない、(6) `OPCLASSES` の `family` がすべて `OPFAMILIES` に実在する。

### 6.3 `catalog/store.rs`

- §4.3 の読み取り・書き込みのメソッド。**行の更新は `update_row_where`（内部の補助）に集約する**:

```rust
/// カタログ catalog_oid の、snap から見える行のうち pred が真の最初の 1 行を new_row(old) で置き換える。
/// 見つからない → Error::internal。TmResult が Ok 以外（Invisible / SelfModified / Updated ...）→ Error::internal（D07-3: 単一ライター、1 コマンド 1 回）
fn update_row_where(&self, w: &WriteCtx, snap: &Snapshot, catalog_oid: Oid,
                    pred: impl Fn(&Row) -> Result<bool>, new_row: impl Fn(&Row) -> Row) -> Result<()>;
```

- `create_table` は M2 の行（`pg_class`、`pg_attribute`、`pg_attrdef`、`pg_constraint`（CHECK））の書き方のまま、`spec.indexes` と依存を足す。**書く順序**: `pg_class`（表）→ `pg_attribute`（ユーザー列、システム列）→ `pg_attrdef` → CHECK の `pg_constraint` → 索引ごとに `write_index_rows`（`pg_class(i)` → `pg_attribute` → `pg_index` → 制約の `pg_constraint` → `pg_depend`）→ 表の依存（`pg_attrdef` → 列、CHECK → 表）→ `extra_depends`。順序に意味はない（どの行も同じコマンドで見えない）が、コミットされる行の物理順（`\di` などの出力の安定性）に効くので固定する。
- `write_index_rows(spec: &NewIndex)` は `create_table` と `create_index` が共有する。`pg_depend` は §3.6: `constraint: Some` なら (制約 → 表の各キー列、`a`) と (索引 → 制約、`i`)、`None` なら (索引 → 表の各キー列、`a`。同じ列は 1 行)。
- `drop_objects` の手順:

```
drop_objects(w, snap, plan):
  keys = plan.items の (class_id, obj_id) の集合
  for item in plan.items:
     Table:       pg_attribute（attrelid = oid）の行、pg_class の行を delete
     Index:       pg_index（indexrelid = oid）、pg_attribute、pg_class
     Sequence:    pg_sequence（seqrelid = oid）、pg_attribute、pg_class
     Constraint:  pg_constraint（oid）
     AttrDefault: pg_attrdef（oid）。owner_table が keys にない（= 列が生き残る）なら update_attribute(owner_table, attnum, has_default = false)
  pg_depend: classid/objid が keys にある行、または refclassid/refobjid が keys にある行を delete（1 回の走査）
  戻り値: Table / Index / Sequence の item.locator
  削除は「走査で TID を集めてから消す」（走査と更新を混ぜない。M2 の drop_table と同じ）。TmResult が Ok 以外は Error::internal
```

- `get_new_relation_oid` は M2 のまま（TRUNCATE の `get_new_relfilenumber` も同じ実装）。`allocate_table_oids` は M2 の `allocate_child_oids` を置き換える（§5.1 の手順 4 の順序）。
- `bootstrap` は `CATALOGS` を回すだけなので変更はない（新しい 9 カタログのファイルが作られ、初期行が入る）。
- 単体テスト（`FakeStore` で）: `create_table`（PK + UNIQUE + CHECK + 既定値）→ `load_table_def` が `indexes`、`checks`、`identity_seqs` を返す、`create_index`、`drop_objects`（PK 付きの表を消すと、全カタログからその OID の行が消える）、`update_class_row` の 2 度目が内部エラー、`owned_sequences`、`constraint_name_exists`。

### 6.4 `catalog/cache.rs` と `catalog/reader.rs`

§4.2、§4.4 のとおり。単体テスト: (1) 索引名で引いて表が読み込まれキャッシュされる、(2) `relation_kind` が表・索引・シーケンスを区別、(3) `table()` が索引の名前に `None`、(4) `index_by_oid` が `bypass_cache` でもキャッシュ済みでも同じ結果、(5) `pg_table_is_visible` が表・索引・シーケンス・存在しない OID（NULL）で正しい、(6) `relation_name` が検索パスにない名前空間を修飾する、(7) キャッシュの世代: 索引を作る DDL のコミットで `invalidate_all` され、古い `TableDef`（`indexes` が空）が戻らない。

### 6.5 `catalog/depend.rs`、`catalog/naming.rs`、`catalog/check.rs`

- `depend.rs`: §4.3 の型、§5.8.1 の `plan_drop`、§5.8.2 の `describe_object`。単体テスト: §5.8.1 の実測の表の全行（FakeStore に `o1`・`o2`・`o3` を作る）、内部依存（PK の索引を単独で消す）、`DROP TABLE o1, o2`（集合の中の依存元は問題にならない）。
- `naming.rs`: §5.10。単体テスト: §5.10.3 の表の全行（衝突の判定には `NameLookup` の偽物を使う）。`make_object_name` の 63 バイトの切り詰め（実測の 60 文字の表名の例）、UTF-8 の途中で切らない。
- `check.rs`（テスト用。`#[cfg(any(test, feature = "testing"))]`）: カタログの整合検査 `check_catalog(cluster: &Cluster, db: &DatabaseHandle, snap: &Snapshot) -> Result<Vec<String>>`（問題の一覧。空なら正常）。§8.3。

### 6.6 `ddl/index.rs`

CREATE INDEX（§5.2）、DROP INDEX（§5.3）、`build_from_heap`（§5.2.1）と、演算子クラスの解決:

```rust
/// 列の型と、明示された演算子クラス名から、使う演算子クラスを決める。戻り値は静的な表（catalog::opclass。06）の要素
pub(crate) fn resolve_opclass(ty: SqlType, requested: Option<&str>) -> Result<&'static OpClass> {
    match requested {
        Some(name) => {
            // "pg_catalog.int4_ops" は pg_catalog だけ許す。それ以外の修飾は存在しないものとして扱う
            let oc = opclass_by_name(name).ok_or_else(|| /* 42704 */
                Error::new(UNDEFINED_OBJECT, format!("operator class \"{name}\" does not exist for access method \"btree\"")))?;
            if !opclass_accepts(oc, ty.oid) {
                return Err(Error::new(DATATYPE_MISMATCH, format!("operator class \"{}\" does not accept data type {}", oc.name, format_type_name(ty.oid, None))));  // 42804
            }
            Ok(oc)
        }
        None => default_opclass(ty.oid)
            .or_else(|| coercible_index_type(ty.oid).and_then(default_opclass))
            .ok_or_else(|| Error::new(UNDEFINED_OBJECT,
                format!("data type {} has no default operator class for access method \"btree\"", format_type_name(ty.oid, None)))
                .with_hint("You must specify an operator class for the index or define a default operator class for the data type.")),   // 42704
    }
}
/// 索引の opclass としてバイナリ互換とみなす型（D07-19）。varchar → text、regclass / regtype / regproc → oid
fn coercible_index_type(ty: Oid) -> Option<Oid>;
fn opclass_accepts(oc: &OpClass, ty: Oid) -> bool { oc.input_type == ty || coercible_index_type(ty) == Some(oc.input_type) }
```

- `opclass_by_name("varchar_ops")` は入力型が `text` の opclass（PostgreSQL と同じ。`varchar` 列に指定できる）。`text_pattern_ops` などは静的な表にないので 42704。
- メッセージの型名は `format_type_name`（`integer`、`xid`、`character varying`）。`xid` の例は実測の文言そのもの。
- 単体テスト: 実測の全ケース（§5.2 の手順 4、§7.3 の表の opclass の行）、`varchar(5)` の索引が `indclass = text_ops`、`regclass` の索引が `oid_ops`。

### 6.6a `IndexHandle` の作成（`ddl/` が使う）

`IndexHandle::from_def(def, table)`（`storage/mod.rs`。持ち主は A / B1。00 §11.4）は、`def.columns[i].attnum` で `table.columns` を引いて `IndexKeyColumn { name, ty, attr, cmp, descending, nulls_first }` を作る。**`cmp` は `catalog::opclass::comparator(opfamily, ty_oid の opclass の入力型, 同じ)`**。C1 は `IndexDef.columns[i].opfamily` を `opclass_by_oid(opclass).family` で埋める責任を持つ（§4.5）。

### 6.7 `ddl/table.rs`、`constraint.rs`、`truncate.rs`、`vacuum.rs`

- `table.rs`: `create_table`（§5.1）、`drop_table`（§5.8）、暫定の `TableDef` の組み立て（§5.1 の手順 8）。
- `constraint.rs`: `add_constraint`（§5.4）、`alter_owner`（§5.5）。
- `truncate.rs`: `truncate`（§5.6）。
- `vacuum.rs`: `vacuum`（§5.7）、`validate_vacuum_options`。
- すべて 00 §14.6 の `DdlCtx` と §4.7 の補助メソッドだけを使う。**`session` や `Cluster` の内部を直接触らない**（`cluster.stack().index`、`cluster.storage()`、`cluster.oid_allocator()` の公開メソッドだけ）。

### 6.8 `ddl/depend.rs`

DDL が書く `pg_depend` の組み立て（`CatalogStore` の `create_table` / `create_index` が内部でも使う。重複を避けるため、**行の組み立ては `catalog/` の側に置き、ここは「何を書くか」の関数だけ**を持つ）。

```rust
/// CREATE TABLE の extra_depends（§5.1 の手順 10）: SERIAL のシーケンスに対する既定値の依存と、default_refs の依存
pub(crate) fn default_value_depends(attrdef_oids: &[(i16, Oid)], owned_sequences: &[(i16, Oid)], default_refs: &[(i16, Oid)]) -> Vec<NewDepend>;
/// 索引の依存（§3.6）。write_index_rows が使う
pub(crate) fn index_depends(index: &NewIndex) -> Vec<NewDepend>;
/// 解析済みの既定値の式から、regclass 定数が指すリレーションの OID を集める（BoundCreateTable.default_refs の元）。analyzer が呼ぶ
pub fn collect_regclass_refs(expr: &BoundExpr) -> Vec<Oid>;       // Literal(Datum::Oid) で ty.oid == REGCLASS のもの。重複なし
```

`collect_regclass_refs` は `expr/walk.rs` の `walk` を使い、`Column`・`SubLink` を含む式（既定値には現れない）は空の結果を返す（エラーにしない）。

### 6.9 `analyzer/ddl_constraint.rs`（PRIMARY KEY / UNIQUE の解析）

M2 の `analyze_create_table` は PRIMARY KEY / UNIQUE を `0A000` で拒否していた。M4 は、**ファイル `analyzer/ddl_constraint.rs`**（C1 が書く。`analyzer/ddl.rs` は Q1 と N が触るので衝突を避ける）の関数を `analyze_create_table` と `ALTER TABLE ADD` の解析から呼ぶ。

```rust
/// CREATE TABLE の要素（列定義の列制約と、表制約）から PRIMARY KEY / UNIQUE を集めて解決する。columns の PRIMARY KEY の列は not_null = true にする
pub(super) fn analyze_index_constraints(table_name: &str, elements: &[TableElement], columns: &mut [ColumnDef]) -> Result<Vec<BoundIndexConstraint>>;
/// ALTER TABLE ADD の 1 件。重複の統合はしない
pub(super) fn analyze_add_constraint(table: &TableDef, c: &TableConstraint) -> Result<BoundIndexConstraint>;
```

`analyze_index_constraints` の手順（PostgreSQL の `transformIndexConstraints` と同じ）:

1. **集める**: 要素を出現順に走査する。列定義の `PRIMARY KEY` / `UNIQUE`（キー列はその列 1 つ）と、表制約の `PRIMARY KEY (cols)` / `UNIQUE (cols)` を `Vec` に積む（名前は明示された `CONSTRAINT x` だけ。`WITH (..)` の `RelOption` も）。`DEFERRABLE` / `INITIALLY DEFERRED`、`NULLS NOT DISTINCT`、`INCLUDE (..)`、`USING INDEX TABLESPACE`、`EXCLUDE` は `0A000`（文言: `DEFERRABLE constraints are not supported yet`、`NULLS NOT DISTINCT is not supported yet`、`INCLUDE columns are not supported yet`、`exclusion constraints are not supported yet`）。`REFERENCES`（FOREIGN KEY）は M2 のまま `0A000`。
2. **列の解決**: 表制約のキー列名を `columns` から引く。なければ `42703 column "zz" named in key does not exist`（位置はその名前）。同じ列が 2 回なら `42701 column "a" appears twice in primary key constraint`（UNIQUE は `... in unique constraint`）。
3. **PRIMARY KEY は 1 つだけ**: 2 つ目（列制約でも表制約でも）は `42P16 multiple primary keys for table "t" are not allowed`（位置は 2 つ目）。PRIMARY KEY の各列は `columns[i].not_null = true`（明示の `NULL` は黙って上書き。PostgreSQL と同じ。`NOT NULL` との矛盾 `NULL` + `NOT NULL` は M2 のとおり 42601）。
4. **統合**（D07-5）: PRIMARY KEY を先頭に置き、残りを出現順に走査して、すでに残したどれかと**キー列の並びが同じ**なら捨てる。捨てるときは、残した側が無名で、捨てる側が名前を持っていれば**名前を残した側に移す**。`unique` は OR（PRIMARY KEY は常に unique）。**`WITH (..)` の options は残した側のものを使う**。
   例（実測）: `unique(a), unique(a)` → 1 つ。`primary key (a), unique (a)` → PRIMARY KEY 1 つ。`a int primary key, constraint u2 unique (a)` → PRIMARY KEY 1 つで**名前は `u2`**。`unique (a, b), unique (b, a)` は別（並びが違う）。
5. **名前の検査（解析で行えるもの。実測）**: 明示された制約名の重複は、組み合わせによって文言が違う。**CHECK どうし** → `42710 check constraint "c" already exists`（M2-Q23 の PostgreSQL の文言。M2 の実装は `constraint "c" for relation "t" already exists` なので、CHECK の重複検査と一緒にこの文言に直す）。**PRIMARY KEY / UNIQUE と CHECK**（どちらが先でも）→ `42710 constraint "c1" for relation "cc2" already exists`。**PRIMARY KEY / UNIQUE どうし**（列が違い統合されない）は解析では検査せず、実行時の索引の名前の衝突 `42P07 relation "c1" already exists`（実測）になる。
6. 結果を `Vec<BoundIndexConstraint>`（PRIMARY KEY が先）にして返す。

`analyze_add_constraint`: 列の解決（2）と `WITH` の取り出しだけ。PRIMARY KEY の `not_null` の更新は実行時（§5.4）。`USING INDEX` と `DEFERRABLE` などは `0A000`。

### 6.10 `bootstrap.rs` と initdb

- `bootstrap.rs` は `rows::builtin_hash()` を制御ファイルに書く（M2 のまま）。新しいカタログのファイルは `CatalogStore::bootstrap` が作る。
- `catalog::rows::dangling_references` が空であることを initdb のテストで確かめる（M2 のまま）。
- 起動時の `builtin_hash` 検査（M2 §6.8.5）は対象が増えただけ。

### 6.11 `session.rs` との接続（S の作業。この章の要求）

- `write_statement_tag(stmt)` に §5.0 の表の文を足す（`Statement::CreateIndex` → `"CREATE INDEX"`、`DropIndex` → `"DROP INDEX"`、`AlterTable` → `"ALTER TABLE"`、`Truncate` → `"TRUNCATE TABLE"`。`Vacuum` は `None`）。
- `exec_create_table` / `exec_drop_table` を削除し、`BoundStatement::Ddl(d)` を `ddl::execute(&mut DdlCtx { cluster, db, snapshot: &snap, catalog: &catalog, txn: &mut self.txn, role_oid, notices: &mut out.notices, type_env, in_transaction_block, interrupts }, d)` に置き換える。戻り値のコマンドタグをそのまま `CommandComplete` に使う。
- `in_transaction_block` は、**明示ブロックまたは暗黙のブロック（1 つの Simple Query に 2 文以上）の中**で真（`SET LOCAL` の判定と同じ値）。
- VACUUM / ANALYZE は書く文ではないので、ライターロックを取らず、`txn.xid` を作らない（暗黙のトランザクションは XID なしでコミットされ、WAL を書かない。M3 §5.3）。ストレージバリア（共有）は他の文と同じく取る。
- DDL が失敗したときの扱いは M2 / M3 のまま（文の失敗 = トランザクションの中断）。
- `Notice::new` を `pub(crate)` にし、`Notice::with_detail` / `with_hint` を足す（`ddl/` が NOTICE の DETAIL を作る）。

---

## 7. psql・pgbench とのカタログの互換、DDL のエラーの表

### 7.1 psql の `\dt` `\di` `\dn` `\ds` が必要とするカタログの値

psql 17 が送る SQL を `psql -E` で実機から採取した（`server_version` が 17 のときの文。M2-Q7）。各メタコマンドが yuzhu のカタログに要求するものを表にする。

**`\dt`**（M2 から動く。M4 では索引とシーケンスが増えても変わらない）:

```sql
SELECT n.nspname as "Schema", c.relname as "Name",
  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index'
       WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table'
       WHEN 'I' THEN 'partitioned index' END as "Type",
  pg_catalog.pg_get_userbyid(c.relowner) as "Owner"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
WHERE c.relkind IN ('r','p','')
      AND n.nspname <> 'pg_catalog' AND n.nspname !~ '^pg_toast' AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;
```

**`\di`**（M4 の追加）:

```sql
SELECT n.nspname as "Schema", c.relname as "Name", CASE c.relkind ... END as "Type",
  pg_catalog.pg_get_userbyid(c.relowner) as "Owner", c2.relname as "Table"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
     LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = c.oid
     LEFT JOIN pg_catalog.pg_class c2 ON i.indrelid = c2.oid
WHERE c.relkind IN ('i','I','')
      AND n.nspname <> 'pg_catalog' AND n.nspname !~ '^pg_toast' AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;
```

**`\ds`**: `\dt` と同じ形で `c.relkind IN ('S','')`、`am` の結合なし。**`\dn`**: `pg_namespace` と `pg_get_userbyid(n.nspowner)`、`n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'`（`public` の所有者が `pg_database_owner` と出る。M2 のまま）。

| 必要なもの | 満たし方 |
|---|---|
| `pg_class.relkind` が `r` / `i` / `S`、`relam` が 2 / 403 / 0（`pg_am` との LEFT JOIN は一致する行がなくても動くが、PostgreSQL と同じ値にする） | §3.2 |
| `pg_index.indexrelid` / `indrelid`（`\di` の Table 列） | §3.4。`LEFT JOIN` なので、`pg_index` に行がない索引は Table 列が空になる（行が必須） |
| `pg_table_is_visible(oid)` が**索引・シーケンスで真** | §4.2 の実装。`\dt` は `relkind IN ('r','p','')` で索引を除くが、`\di` は索引の可視性を要る |
| `pg_get_userbyid(relowner)` | M2。索引の `relowner` は表と同じ所有者（§3.2） |
| 正規表現 `!~`（`n.nspname !~ '^pg_toast'`） | M2 の最小実装、M4 の 09（手書きエンジン、D-22） |
| `ORDER BY 1,2`（C 照合の文字列比較） | M2 のまま |

期待される出力（`create table pl_t (a int primary key, b text unique); create index pl_i on pl_t (b);` の後の `\di`。PostgreSQL と yuzhu で同じ）:

```
                List of relations
 Schema |    Name    | Type  |  Owner   | Table
--------+------------+-------+----------+-------
 public | pl_i       | index | postgres | pl_t
 public | pl_t_b_key | index | postgres | pl_t
 public | pl_t_pkey  | index | postgres | pl_t
```

**`\d tbl`（任意。D-24）**の 4 本目（索引）が C1 に要求する値: `pg_index` の `indisprimary`、`indisunique`、`indisclustered`、`indisvalid`、`indisreplident`、`pg_constraint.conindid`・`contype`・`condeferrable`・`condeferred`、`pg_class.reltablespace`、`pg_get_indexdef(indexrelid, 0, true)`（`CREATE UNIQUE INDEX t1_pkey ON public.t1 USING btree (a)`。実測）、`pg_get_constraintdef(con.oid, true)`（`PRIMARY KEY (a)`、`UNIQUE (b)`）。後 2 つの関数は E1 が `CatalogReader::index_by_oid` と `constraint_by_oid` を使って書く（列の並び・`indoption`（`DESC`、`NULLS FIRST`）・opclass が既定でないときの `opclass 名` の表示は E1 が決める。この章はデータを提供する）。

### 7.2 pgbench `-i` の DDL

`pgbench -i`（既定の `dtgvp`）が送る DDL とこの章の対応（`pg-compat-tools.md` §3.2.1）:

| 手順 | 文 | この章の関係 |
|---|---|---|
| d | `drop table if exists pgbench_accounts, pgbench_branches, pgbench_history, pgbench_tellers` | §5.8（NOTICE 4 件）。M1 から動く |
| t | `create table pgbench_tellers(tid int not null, bid int, tbalance int, filler char(84)) with (fillfactor=100)` | §5.9（`fillfactor` の検証）、`char(n)` は 09 |
| g | `begin; truncate table pgbench_accounts, pgbench_branches, pgbench_history, pgbench_tellers; copy ... from stdin with (freeze on); commit` | §5.6（4 表の TRUNCATE、トランザクション内）。**TRUNCATE の後の COPY は、新しい relfilenode に書く**（COPY の `RelHandle` は TRUNCATE で更新された `TableDef` を使う。`catalog_dirty` により `bypass_cache` で読み直される） |
| v | `vacuum analyze pgbench_branches` ×4 | §5.7（何もしない） |
| p | `alter table pgbench_branches add primary key (bid)`（×3） | §5.4（`bid`・`tid`・`aid` に 100 万行の構築がありうる） |
| 開始時 | `vacuum pgbench_branches`、`truncate pgbench_history` | §5.7、§5.6（失敗しても続行されるが、成功する） |

### 7.3 DDL ごとのエラーと NOTICE の表

メッセージは実機（PostgreSQL 17.11）の ErrorResponse / NoticeResponse の文言どおり。「検」は検出する層（A = アナライザ、D = `ddl/`、C = `catalog/`）。位置（`P` フィールド）が付くものは、解析の層が `Span` を持つもの。

| 文 | 条件 | SQLSTATE | メッセージ（DETAIL / HINT） | 検 |
|---|---|---|---|---|
| CREATE TABLE | 同名のリレーション（表・索引・シーケンス） | 42P07 | `relation "t" already exists` | D |
| 〃 | `IF NOT EXISTS` で同名 | （NOTICE）42P07 | `relation "t" already exists, skipping` | D |
| 〃 | PRIMARY KEY が 2 つ | 42P16 | `multiple primary keys for table "t" are not allowed` | A |
| 〃 | キー列が 2 回 | 42701 | `column "a" appears twice in primary key constraint` / `... in unique constraint` | A |
| 〃 | 存在しないキー列 | 42703 | `column "zz" named in key does not exist` | A |
| 〃 | 明示した制約名が同名の索引・リレーション | 42P07 | `relation "x" already exists` | D |
| 〃 | 明示した制約名が、CHECK どうしで重複 | 42710 | `check constraint "c" already exists` | A |
| 〃 | 明示した制約名が、PRIMARY KEY / UNIQUE と CHECK で重複 | 42710 | `constraint "c1" for relation "cc2" already exists` | A |
| 〃 | 明示した制約名が、PRIMARY KEY / UNIQUE どうしで重複（列が違う） | 42P07 | `relation "c1" already exists` | D |
| 〃 | キーの型に opclass がない（`xid` など） | 42704 | `data type xid has no default operator class for access method "btree"` / HINT `You must specify an operator class for the index or define a default operator class for the data type.` | D |
| 〃 | `WITH (fillfactor=5)` | 22023 | `value 5 out of bounds for option "fillfactor"` / DETAIL `Valid values are between "10" and "100".` | D |
| 〃 | `WITH (foo=1)` | 22023 | `unrecognized parameter "foo"` | D |
| 〃 | `DEFERRABLE` / `NULLS NOT DISTINCT` / `INCLUDE` / `EXCLUDE` | 0A000 | §6.9 の文言 | A |
| CREATE INDEX | 表がない | 42P01 | `relation "nosuch" does not exist` | A |
| 〃 | 列がない | 42703 | `column "zz" does not exist` | A |
| 〃 | 対象がシーケンス | 42809 | `cannot create index on relation "ds1"` / DETAIL `This operation is not supported for sequences.` | A |
| 〃 | 対象が索引 | 42809 | `cannot open relation "t1_pkey"` / DETAIL `This operation is not supported for indexes.` | A |
| 〃 | システムカタログ | 42501 | `permission denied: "pg_class" is a system catalog` | D |
| 〃 | 同名のリレーション | 42P07 | `relation "e1_a" already exists` | D |
| 〃 | `IF NOT EXISTS` で同名 | （NOTICE）42P07 | `relation "e1_a" already exists, skipping` | D |
| 〃 | 名前なしの `IF NOT EXISTS` | 42601 | `syntax error at or near "on"`（03） | S1 |
| 〃 | 33 列以上 | 54011 | `cannot use more than 32 columns in an index` | D |
| 〃 | 存在しない opclass | 42704 | `operator class "nosuch_ops" does not exist for access method "btree"` | D |
| 〃 | opclass が列の型に合わない | 42804 | `operator class "text_ops" does not accept data type integer` | D |
| 〃 | 既定の opclass がない型 | 42704 | `data type xid has no default operator class for access method "btree"` / HINT（上と同じ） | D |
| 〃 | 存在しないアクセスメソッド | 42704 | `access method "nosuch" does not exist` | D |
| 〃 | `USING hash` など | 0A000 | `index access method "hash" is not supported yet`（yuzhu 独自の文言。PostgreSQL は成功する） | D |
| 〃 | 式・`WHERE`・`INCLUDE`・`NULLS NOT DISTINCT`・`COLLATE`・`TABLESPACE` | 0A000 | `index expressions are not supported yet`、`partial indexes are not supported yet`、`INCLUDE columns are not supported yet`、`NULLS NOT DISTINCT is not supported yet`、`collations are not supported yet`、`TABLESPACE is not supported yet`（yuzhu 独自の文言） | A |
| 〃 | 一意索引で重複 | 23505 | `could not create unique index "u1_ab"` / DETAIL `Key (a, b)=(1, x) is duplicated.`（`s` `t` `n` フィールド付き） | D |
| 〃 | `WITH (fillfactor=5)` | 22023 | CREATE TABLE と同じ | D |
| 〃 | `CONCURRENTLY` をブロック内で | 25001 | `CREATE INDEX CONCURRENTLY cannot run inside a transaction block` | D |
| 〃 | 読み取り専用のトランザクション | 25006 | `cannot execute CREATE INDEX in a read-only transaction` | S |
| DROP INDEX | 存在しない | 42704 | `index "nosuch" does not exist` | A |
| 〃 | `IF EXISTS` で存在しない | （NOTICE）00000 | `index "nosuch" does not exist, skipping` | A |
| 〃 | 対象が表 | 42809 | `"d1" is not an index` / HINT `Use DROP TABLE to remove a table.` | A |
| 〃 | 対象がシーケンス | 42809 | `"ds1" is not an index` / HINT `Use DROP SEQUENCE to remove a sequence.` | A |
| 〃 | 制約が所有する索引 | 2BP01 | `cannot drop index d1_pkey because constraint d1_pkey on table d1 requires it` / HINT `You can drop constraint d1_pkey on table d1 instead.`（`CASCADE` でも同じ） | C |
| 〃 | `CONCURRENTLY` をブロック内で | 25001 | `DROP INDEX CONCURRENTLY cannot run inside a transaction block` | D |
| 〃 | `CONCURRENTLY` で複数 / `CASCADE` | 0A000 | `DROP INDEX CONCURRENTLY does not support dropping multiple objects` / `... does not support CASCADE` | D |
| ALTER TABLE ADD | 表がない | 42P01 | `relation "nosuch" does not exist` | A |
| 〃 | `IF EXISTS` で表がない | （NOTICE）00000 | `relation "nosuch" does not exist, skipping` | A |
| 〃 | 対象がシーケンス / 索引 | 42809 | `ALTER action ADD CONSTRAINT cannot be performed on relation "ds1"` / DETAIL `This operation is not supported for sequences.`（索引は `... for indexes.`） | A |
| 〃 | システムカタログ | 42501 | `permission denied: "pg_class" is a system catalog` | D |
| 〃 | 2 つ目の PRIMARY KEY | 42P16 | `multiple primary keys for table "a1" are not allowed` | D |
| 〃 | 同名のリレーション（制約と同名の索引を含む） | 42P07 | `relation "u1" already exists` | D |
| 〃 | 同名の制約（CHECK など） | 42710 | `constraint "c4" for relation "cc4" already exists`（実測。UNIQUE の追加でも CHECK の追加でも同じ） | D |
| 〃 | キー列の重複 / 存在しない列 | 42701 / 42703 | CREATE TABLE と同じ（`... in unique constraint`） | A |
| 〃 | 重複するキー | 23505 | `could not create unique index "a1_pkey"` / DETAIL `Key (a)=(3) is duplicated.`（`s` `t` `n` 付き） | D |
| 〃 | PRIMARY KEY の列に NULL | 23502 | `column "b" of relation "a1" contains null values`（`s` `t` `c` 付き） | D |
| 〃 | `USING INDEX`・`DEFERRABLE`・`DROP CONSTRAINT`・`ADD COLUMN` など | 0A000 | `ALTER TABLE ... is not supported yet` の形（03 が文言を決める） | A |
| 〃 | 読み取り専用 | 25006 | `cannot execute ALTER TABLE in a read-only transaction` | S |
| OWNER TO | 存在しないロール | 42704 | `role "nosuch" does not exist` | D |
| 〃 | `none` | 42939 | `role name "none" is reserved` | S1 |
| TRUNCATE | 表がない | 42P01 | `relation "nosuch" does not exist` | A |
| 〃 | 対象が索引・シーケンス | 42809 | `"tr1_pkey" is not a table` | A |
| 〃 | システムカタログ | 42501 | `permission denied: "pg_class" is a system catalog` | D |
| 〃 | 読み取り専用 | 25006 | `cannot execute TRUNCATE TABLE in a read-only transaction` | S |
| VACUUM / ANALYZE | `VACUUM` がブロック内 | 25001 | `VACUUM cannot run inside a transaction block` | D |
| 〃 | 表がない | 42P01 | `relation "nosuch" does not exist` | A |
| 〃 | 索引・シーケンス | （WARNING）01000 | `skipping "v1_a" --- cannot vacuum non-tables or special system tables`（ANALYZE は `cannot analyze non-tables ...`） | D |
| 〃 | 未知のオプション | 42601 | `unrecognized VACUUM option "foo"` / `unrecognized ANALYZE option "foo"` | D |
| 〃 | 列リストだけ（`ANALYZE` なし） | 0A000 | `ANALYZE option must be specified when a column list is provided` | D |
| 〃 | 列がない | 42703 | `column "zz" of relation "v1" does not exist` | A |
| DROP TABLE | 表がない | 42P01 | `table "x" does not exist` | A |
| 〃 | `IF EXISTS` で表がない | （NOTICE）00000 | `table "x" does not exist, skipping` | A |
| 〃 | 対象が索引 | 42809 | `"d1_pkey" is not a table` / HINT `Use DROP INDEX to remove an index.` | A |
| 〃 | 対象がシーケンス | 42809 | `"ds1" is not a table` / HINT `Use DROP SEQUENCE to remove a sequence.` | A |
| 〃 | システムカタログ | 42501 | `permission denied: "pg_class" is a system catalog` | D |
| 〃 | 他のオブジェクトが依存 | 2BP01 | `cannot drop table o1 because other objects depend on it` / DETAIL `default value for column id of table o2 depends on sequence o1_id_seq` / HINT `Use DROP ... CASCADE to drop the dependent objects too.`（複数なら `cannot drop desired object(s) because other objects depend on them`） | C |
| 〃 | `CASCADE` で連鎖 | （NOTICE）00000 | `drop cascades to default value for column id of table o2`（2 件以上は `drop cascades to 2 other objects` + DETAIL） | C |

INSERT / UPDATE の一意制約違反（`23505 duplicate key value violates unique constraint "t_pkey"` / DETAIL `Key (a)=(1) already exists.`、`s` `t` `n` フィールド）は `06-btree.md` の一意性検査が返す。NOT NULL 違反（`23502`）と CHECK 違反は M2 のまま（00 の D-27 でフィールドが付く）。

---

## 8. テスト

共通の規則は M1〜M3 と同じ: `tests/slt/m4/` の期待値は **PostgreSQL 17 で先に通す**（`tests/run.sh --target pg`）。表名は**ファイルごとの接頭辞**（`cat_i_`、`cst_`、`idx_`、`trc_` など）を付け、**末尾で DROP して**同じ DB への再実行が通るようにする（M2-Q22 の教訓）。OID・`relpages`・`reltuples`・`pg_opclass.oid`・`pg_amop.oid` は比べない。`pg_class` などは `WHERE oid > 16383` か名前で絞る。

### 8.1 共通の SQL テスト（`tests/slt/m4/{catalog,constraint,index}/`）

**`catalog/`**:

| ファイル | 内容 |
|---|---|
| `catalog_columns_m4.slt` | 追加した 9 カタログの `attnum`・`attname`・`format_type(atttypid, atttypmod)`・`attnotnull` を `pg_attribute` から取って PostgreSQL と一致させる（`attcollation` も） |
| `pg_index.slt` | PRIMARY KEY / UNIQUE / 通常 / DESC・NULLS の 4 通り / 複数列 / `varchar(5)`（`indclass` が text_ops、`indcollation` が 100）/ `regclass`（oid_ops）/ `bpchar` / `numeric` / `date` / `timestamp` の `indkey`・`indclass`（名前に結合して比べる）・`indoption`・`indcollation`・`indisunique`・`indisprimary`・`indimmediate`・`indisvalid`・`indisready`・`indislive`・`indnatts`・`indnkeyatts`・`indexprs` が NULL |
| `pg_constraint_index.slt` | `contype`（p / u）、`conindid` と `pg_index.indexrelid` の結合、`conkey`、`connoinherit`、`conislocal`、`convalidated`、PRIMARY KEY の索引と制約が同名、CHECK との共存（`conindid = 0`） |
| `pg_depend.slt` | §3.6 の表の全行を、§3.10 の例の表に対して `pg_depend` から `deptype` ごとに集計し（`classid::regclass`、`refclassid::regclass`、`deptype`、`refobjsubid` の組の件数）、PostgreSQL と比べる。**差分のある行（名前空間への依存、CHECK）は `WHERE refclassid <> 'pg_namespace'::regclass` と `contype <> 'c'` で除く** |
| `pg_class_kinds.slt` | `relkind`（r / i / S）、`relam`（2 / 403 / 0）、`relhasindex`（作成で `t`、`DROP INDEX` の後も `t`）、`relnatts`、`relchecks`、`relreplident`（d / n / n）、索引の `relowner`、`relnamespace` |
| `pg_attribute_index.slt` | 索引の列: `attname`（`a`、`a1`、`a2`）、`atttypid` / `atttypmod`（`varchar(5)`: 9、`numeric(5,2)`: 327686、`char(3)`: 7、`timestamp(3)`: 3）、`attnotnull = f`、システム列がない（`attnum < 0` が 0 件）、`attcollation`。`name` 型の索引列の `atttypid` は差分なのでテストしない |
| `opclass_catalog.slt` | `pg_opfamily`（名前と `opfmethod`）、`pg_opclass`（`opcname`・`opcdefault`・`opcintype` を型名に結合）、`pg_amop` のうち `integer_ops` の `int4` 同士の 5 つの strategy、`pg_amproc` の support 1 の関数名（`btint4cmp` など）。**行数は比べない**（yuzhu は PostgreSQL より少ない） |
| `language.slt` | `pg_language` の 3 行（`oid`・`lanname`・`lanispl`・`lanpltrusted`）、`pg_proc.prolang` がすべて `pg_language` に存在する |
| `catalog_closure.slt` | 追加カタログの OID 参照が閉じている（`pg_opclass.opcfamily`、`pg_amop.amopopr`、`pg_amproc.amproc` など。§3.7 の表の各行を `NOT IN (SELECT oid FROM ...)` の件数 0 で）。M2 の `catalog_rows.slt` の方式 |
| `consistency.slt` | §8.3 の SQL 版（ユーザーオブジェクトのカタログの整合。複数の DDL の後、末尾の DROP の後に流す） |
| `psql_queries.slt` | §7.1 の `\dt`、`\di`、`\ds`、`\dn` の SQL を**そのまま**流す（`WHERE` に `AND c.relname LIKE 'pl\_%'` を足して他のテストの表を除く）。`\di` の結果が §7.1 の表と同じ |
| `table_is_visible.slt` | `pg_table_is_visible` が表・索引・シーケンスで `t`、存在しない OID（`0`）で NULL、`pg_class` の `relname` から引いた OID で全リレーションが `t` |

**`constraint/`**:

| ファイル | 内容 |
|---|---|
| `pk_basic.slt` | 列制約・表制約の PRIMARY KEY、重複の INSERT（`23505`）、NULL の INSERT（`23502`）、同じトランザクションでの DELETE → 同じキーの INSERT（成功）、同じキーへの UPDATE（キー以外。成功）、ROLLBACK した行と同じキー、複数列 |
| `unique_basic.slt` | UNIQUE の NULL（複数 NULL は成功）、複数列の NULL、`text` / `varchar` / `bpchar` / `numeric` / `date` / `timestamp` のキー、UPDATE での衝突 |
| `names.slt` | §5.10.3 の表の全行（自動名、衝突、統合、PRIMARY KEY の名前の移し替え、長い名前、`pg_class` と `pg_constraint` の名前の一致） |
| `create_table_errors.slt` | §7.3 の CREATE TABLE の行（42P16、42701、42703、42P07、42710、42704、22023、0A000） |
| `alter_add.slt` | `ADD PRIMARY KEY` / `ADD CONSTRAINT n UNIQUE`: 重複（`23505 could not create unique index` と DETAIL。複数列は `Key (a, b)=(1, x)`）、NULL（`23502 ... contains null values`）、**重複と NULL が両方ある表で `23505` が先**、2 つ目の PK（`42P16`）、同じ列の UNIQUE の 2 回の ADD（`_key`、`_key1`）、失敗した ADD の後に索引・制約の行が残らない、成功後の `attnotnull`、既存の行がある表への ADD、`IF EXISTS`、`OWNER TO`（自分、存在しないロール、`pg_database_owner`。索引の `relowner` も変わる） |
| `drop_table_dependencies.slt` | §5.8.1 の実測の表の全行（RESTRICT のエラーの DETAIL、`DROP TABLE o1, o4` の文言、`o1, o2` 同時、CASCADE の NOTICE と `o2.id` の既定値が消える）、`DROP TABLE` の後に PK 付きの表の全カタログの行が消える |
| `unique_build_versions.slt` | 実測の `u1` の例: 重複する版（`DELETE` した行、同じトランザクションの UPDATE 2 回）が重複と誤判定されない / 生きている版の重複は `could not create unique index` |

**`index/`**:

| ファイル | 内容 |
|---|---|
| `create_index.slt` | 基本、`DESC` / `NULLS FIRST`、複数列、自動名（`t_a_idx`、`t_a_idx1`）、`IF NOT EXISTS`、既存の行がある表、空の表、`CONCURRENTLY`（ブロックの外で成功、ブロック内で `25001`）、`WITH (fillfactor=..)` |
| `create_index_errors.slt` | §7.3 の CREATE INDEX の行（42P01、42703、42809、42501、42P07、54011、42704 ×3、42804、0A000 ×各種、22023、25001） |
| `opclass.slt` | `int4_ops` の明示、`text_ops` を整数に（42804）、存在しない opclass（42704）、`xid` の索引（42704 と HINT）、`varchar` に `varchar_ops` と `text_ops`、`regclass` の索引 |
| `drop_index.slt` | §7.3 の DROP INDEX の行（42704、NOTICE、42809 の 2 種の HINT、`2BP01` の HINT と CASCADE でも同じ、`CONCURRENTLY` の 3 つのエラー）、複数の索引、同じ名前を 2 回、`DROP INDEX` の後に再作成 |
| `index_dml.slt` | 索引の付いた表への INSERT / UPDATE / DELETE の後、`SET enable_seqscan = off`（yuzhu は索引走査、PostgreSQL も同じ結果）と既定で、同じ問い合わせが同じ結果を返す。既存の行がある表への CREATE INDEX の後の等値・範囲・ORDER BY。**同じ SQL を `onlyif yuzhu` なしで流せる**（結果の比較だけ。EXPLAIN は 11 の `plan_variants`） |
| `index_tx.slt` | `BEGIN; CREATE INDEX; ROLLBACK`（索引が残らない、同じ名前で再作成できる）、`BEGIN; DROP INDEX; ROLLBACK`（索引が残る）、`BEGIN; CREATE TABLE ... PRIMARY KEY; INSERT; CREATE INDEX; COMMIT`、同じトランザクションで作った表への ADD PRIMARY KEY |
| `truncate.slt` | §5.6: 単一、複数表、`TRUNCATE t, t`、`ONLY`、`CASCADE` / `RESTRICT`、トランザクション内（COMMIT / ROLLBACK）、TRUNCATE → INSERT → ROLLBACK、索引の付いた表（TRUNCATE の後に索引走査が空、INSERT で一意性検査が働く）、`RESTART IDENTITY` / `CONTINUE IDENTITY`（08 と共同）、エラー（存在しない表、索引・シーケンスを指定、`pg_class`）、`relfilenode` が変わる（`pg_class.relfilenode` の前後の比較）、読み取り専用のトランザクション（25006） |
| `vacuum_analyze.slt` | §5.7: `VACUUM`、`VACUUM v`、`VACUUM ANALYZE v`、`ANALYZE v`、`ANALYZE v (a)`、`BEGIN; ANALYZE; COMMIT`（成功）、`BEGIN; VACUUM; ROLLBACK`（25001）、索引・シーケンスへの WARNING、オプション（`(verbose)`、`(foo)`、`(parallel 2)` など）、`BEGIN READ ONLY; VACUUM; ANALYZE;`（成功） |
| `reloptions.slt` | §5.9 の表の全行（表・索引・PRIMARY KEY / UNIQUE の `WITH (fillfactor)`） |
| `readonly_ddl.slt` | `BEGIN READ ONLY` の中で CREATE TABLE / DROP TABLE / CREATE INDEX / DROP INDEX / ALTER TABLE / TRUNCATE が 25006（タグの文言）、VACUUM と ANALYZE は成功 |

slt の例（`names.slt` の一部。PostgreSQL と yuzhu で同じ結果）:

```
statement ok
CREATE TABLE nm_t2 (a int, b int, c text, PRIMARY KEY (a, b), UNIQUE (b, c), UNIQUE (a))

statement ok
CREATE INDEX ON nm_t2 (a DESC)

statement ok
CREATE INDEX ON nm_t2 (a DESC NULLS LAST)

query TT
SELECT c.relname, i.indoption::text FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'nm_t2'::regclass ORDER BY c.relname
----
nm_t2_a_idx 3
nm_t2_a_idx1 1
nm_t2_a_key 0
nm_t2_b_c_key 0 0
nm_t2_pkey 0 0

statement ok
DROP TABLE nm_t2
```

### 8.2 再起動をまたぐテスト（`tests/restart/m4/`、`--restart` と `--crash`）

M2 / M3 の方式（1 ファイル = 1 フェーズ。フェーズの間に fast shutdown か `kill -9`）。

| シナリオ | 内容 |
|---|---|
| `01-index-committed` | 表・PK・UNIQUE・`CREATE INDEX` をコミット → 再起動 → カタログ（`pg_index`、`pg_constraint`、`pg_depend`）、索引走査の結果、`\di` の SQL、重複の INSERT が `23505`、さらに INSERT・UPDATE・DELETE して再起動 → 結果が一致 |
| `02-index-rollback` | `BEGIN; CREATE INDEX; CREATE TABLE ... PRIMARY KEY; ROLLBACK` → 再起動 → カタログに残らない、同じ名前で再作成できる（名前が残っていない）、`SELECT` が通る |
| `03-drop-index-commit` | `DROP INDEX` をコミット → 再起動 → 索引がなく表のデータは無事、再作成できる |
| `04-truncate-commit` / `05-truncate-rollback` | TRUNCATE（索引付きの表）のコミット / ロールバック → 再起動 → 空 / 元のデータ、索引の一貫性（`enable_seqscan` の on / off の件数が一致） |
| `06-drop-table-cascade` | 他の表の既定値が使うシーケンスを持つ表を `DROP TABLE ... CASCADE` → 再起動 → 全カタログの整合（`consistency.slt` の SQL）、残った表の `atthasdef = f` |
| `07-ddl-in-flight`（`--crash`） | 大きな表への `CREATE INDEX`（未コミット）・`ALTER TABLE ADD PRIMARY KEY`（未コミット）の途中で `kill -9` → 再起動 → 索引がない、データ無事、同じ名前で再作成できる、`consistency.slt` が通る |
| `08-unique-after-crash`（`--crash`） | PRIMARY KEY 付きの表へ多数の INSERT / UPDATE をコミットして `kill -9` → 再起動 → 重複の INSERT が `23505`、行数と索引走査の件数が一致 |
| `09-alter-add-commit-crash`（`--crash`） | `ALTER TABLE ADD PRIMARY KEY` をコミット直後に `kill -9` → 再起動 → 制約と索引が残る（REDO が索引ファイルを再現する）、重複が拒否される |

### 8.3 カタログの整合検査（`catalog/check.rs` と `consistency.slt`）

DROP の取りこぼし（D07-2 の「二重の網」の前提）と、WAL の REDO 後の欠損を検出する検査。**Rust 版**（`check_catalog`）は下の条件をすべて確かめて問題の文字列を返し、全 Rust の統合テストの終わりと（M3 の）クラッシュ試験の各クラッシュ点で呼ぶ。**SQL 版**（`consistency.slt`）はユーザーオブジェクト（`oid >= 16384`）について条件 1〜9 を `SELECT ... WHERE ...` の件数 0 で書き、PostgreSQL にも流す。

| # | 条件 |
|---|---|
| 1 | すべての `pg_attribute.attrelid`、`pg_attrdef.adrelid`、`pg_constraint.conrelid`（≠ 0）、`pg_index.indrelid` と `indexrelid` が `pg_class` に存在する |
| 2 | `pg_index.indexrelid` の `pg_class` は `relkind = 'i'`、`indrelid` のは `'r'`。`relkind = 'i'` の行は必ず `pg_index` に行がある |
| 3 | `pg_constraint.conindid`（≠ 0）が `pg_index` に存在し、`pg_index.indrelid` と `conrelid` が一致する。`contype in ('p','u')` の制約の `conindid` は ≠ 0 |
| 4 | 制約が所有する索引（`conindid` の指す索引）には、`pg_depend` に（索引 → 制約、`i`）が**ちょうど 1 行**ある。制約には各キー列への（制約 → 表の列、`a`）がある |
| 5 | `pg_depend` の両端（`classid` / `refclassid` が `pg_class` / `pg_constraint` / `pg_attrdef` のとき）が実在する（`pg_namespace` への参照は除く） |
| 6 | `relkind = 'S'` の行は `pg_sequence` に行がある（逆も）。`pg_sequence.seqrelid` が `pg_class` に存在する |
| 7 | 表の `relhasindex` は、`pg_index` に行があれば `t`（逆は成り立たない: D07-9） |
| 8 | 表の `relnatts` が `pg_attribute`（`attnum > 0`、`attisdropped = f`）の件数に等しい。`relchecks` が CHECK の件数に等しい |
| 9 | 表の `pg_attribute.atthasdef = t` の列は `pg_attrdef` に行がある（逆も）。`attidentity` のある列は `pg_depend` に（シーケンス → 列、`i`）がある |
| 10（Rust のみ） | `pg_class.relfilenode`（≠ 0）の集合のファイルがすべて存在し、データディレクトリの `base/<db>/` に**ほかのファイルがない**（クラッシュ試験以外。孤児の検出。M3 の D15 により、クラッシュ後は「ほかのファイルがある」を許す） |

### 8.4 Rust のテスト

- **単体テスト**: §6.1〜§6.8 に列挙したもの（`rows`、`store`、`cache` / `reader`、`depend`、`naming`、`ddl/index`（`resolve_opclass`）、`ddl/depend`）。
- **`yuzhu-core/tests/ddl_m4.rs`（統合）**: `TestCluster`（M2 の `testing.rs`）に対して SQL を流す。(1) `CREATE TABLE`（PK / UNIQUE / CHECK / DEFAULT）→ `INSERT` → `DROP TABLE` の後、`rel_files(&tc)`（`base/<db>/` のファイル数）が元に戻る。(2) `ROLLBACK` した `CREATE TABLE`・`CREATE INDEX`・`ADD PRIMARY KEY` の後、同じ。(3) `TRUNCATE` のコミット後に旧ファイルが消え、ロールバックで新ファイルが消える（ファイル数と `pg_class.relfilenode`）。(4) `DROP INDEX` のコミットで索引のファイルだけが消える。(5) 同じトランザクションで `CREATE TABLE` → `TRUNCATE` → `DROP TABLE` → `COMMIT`（二重の unlink が起きない）。(6) 毎テストの終わりに `check_catalog` が空。
- **エラーのフィールド**（`yuzhu-core/tests/ddl_error_fields.rs`）: `23505`（`ALTER TABLE ADD` と `CREATE UNIQUE INDEX`）で `Error.table = Some("t")`、`schema = Some("public")`、`constraint = Some(索引名)`、`23502`（ADD PRIMARY KEY）で `column = Some("b")`（00 §14.4）。`yuzhu-server` の ErrorResponse の `s` `t` `n` `c` フィールドの符号化は J の試験。
- **PostgreSQL との列挙の突き合わせ**: `tests/compat/`（10 と 11）の psql の `\dt` `\di` `\dn` `\ds`（§7.1 の出力）。

### 8.5 クラッシュ試験への依頼（`yuzhu-core/tests/crash_sim/`、R2）

M3 の層 1 のクラッシュ試験に **DDL のワークロード**を足してもらう（R2 の担当。00 §18）: 繰り返し `CREATE TABLE`（PK / UNIQUE）→ `INSERT` → `CREATE INDEX` → `UPDATE` → `DROP INDEX` → `TRUNCATE` → `DROP TABLE` を、コミット / ロールバックを混ぜて実行する。各クラッシュ点の後のリカバリで、(1) `check_catalog`（条件 1〜9）、(2) 06 の木の検査器（I14）と「索引の全 TID 集合 = ヒープの索引されるべき版」（I13）、(3) コミットした DDL が見え、コミットしていないものが見えない、を確かめる。

---

## 9. 実装の分担と工数

**C1（カタログと DDL）: 7 日**（00 §17）。依存: A（型）、B2（`catalog/opclass.rs` の静的な表。B2 が遅れる間は、C1 は `FakeOpClasses`（int4_ops など数個）で単体テストを進め、実表への切り替えは 1 行）。

| # | 作業 | ファイル | 日数 |
|---|---|---|---|
| C1-1 | 追加カタログの定義、`ClassSpec` ほかの一般化、`index_row` / `constraint_row` / `depend_row` / `sequence_row`、生成行（`pg_language`、`pg_opfamily` など）、`builtin_hash` と `reference_columns` の拡張、列の突き合わせの slt | `catalog/schema.rs`、`rows.rs` | 1.0 |
| C1-2 | `load_table_def`（索引・シーケンス・identity）、`CatalogStore` の読み取り・書き込み（`create_table` 拡張、`create_index`、`add_constraint`、`update_class_row`、`update_attribute`、`truncate_relation`、`record_dependency`、`delete_dependencies`、`create_sequence`、`drop_objects`、`allocate_table_oids`）、単体テスト | `catalog/store.rs` | 2.0 |
| C1-3 | `StatementCatalog` の追加メソッド、キャッシュ、`pg_table_is_visible` の対象拡大 | `catalog/cache.rs`、`reader.rs`、`builtin.rs`（その 1 関数） | 0.75 |
| C1-4 | `catalog/depend.rs`（`plan_drop`、`describe_object`）、`naming.rs`（M2 の `make_object_name` の移動を含む）、`check.rs` | `catalog/depend.rs`、`naming.rs`、`check.rs` | 1.0 |
| C1-5 | `ddl/mod.rs`（`DdlCtx` の補助、`validate_reloptions`）、`ddl/index.rs`（CREATE / DROP INDEX、`build_from_heap`、`resolve_opclass`） | `ddl/mod.rs`、`index.rs` | 1.0 |
| C1-6 | `ddl/table.rs`（CREATE TABLE / DROP TABLE）、`constraint.rs`（ADD PK / UNIQUE、OWNER TO）、`truncate.rs`、`vacuum.rs`、`ddl/depend.rs` | `ddl/*.rs` | 0.75 |
| C1-7 | `analyzer/ddl_constraint.rs`（PK / UNIQUE の解析、統合）、`bootstrap.rs` の確認、`tests/slt/m4/{catalog,constraint,index}` のうち K が書かない分の補助、統合テスト | `analyzer/ddl_constraint.rs`、`bootstrap.rs`、`tests/` | 0.5 |
| | **合計** | | **7.0** |

**レビュー対応・統合での追加**（11 §4.1 の確定表が正）: `CatalogStore::update_sequence_params`（08 の ALTER SEQUENCE。R-08）+0.2 日、`analyzer/ddl_index.rs`（CREATE INDEX / DROP INDEX / DROP TABLE / TRUNCATE / VACUUM / ALTER TABLE の解析。11 §7.3 の G-1）+1.0 日で、**C1 は 8.2 日**。任意: `\d tbl` のための空のカタログ表（10 §6.3 の「C1b」。R-19）+1.0 日（完了条件ではない）。

**並列化**: C1-1 → C1-2 → C1-3 は直列（読み書きの API の土台）。C1-4（`plan_drop`、`naming`）は C1-1 の型ができれば並行できる。C1-5・C1-6・C1-7 は C1-2・C1-4 の後。**B2 が `IndexStore::build` / `init_index` を実装する前でも、C1 は `FakeIndexStore`（何もしない）で DDL の単体テストを書ける**（実際の索引を使う統合テストは B2 の後）。

**他の担当への依存**（C1 が待つ・待たれるもの）:

- 待つ: A（`RelKind::{Index, Sequence}`、`ColumnDef.identity`、`TableDef.indexes` / `sequence` / `identity_seqs`、`IndexDef` ほか、`Datum::Int2Vector`、`DdlCtx` の追加フィールド、`error::sqlstate::DEPENDENT_OBJECTS_STILL_EXIST`、`Error::with_table` ほか）、B2（opclass の静的な表）、H4（`begin_scan_all`、`tuple_state`）、B1（`init_index`）、S1（AST: `CreateIndex` ほか）。
- 待たれる: Q1（`CatalogStore::create_sequence`、`catalog::depend::plan_drop`、`ObjectAddress`）、S（`ddl::execute`、`DdlCtx`）、E1（`index_by_oid`、`constraint_by_oid`）、N1・N2・N3（`relation_kind`、`TableDef.indexes`）、L2（`TableDef.indexes` を索引選択に使う）、K（`tests/slt/m4/catalog`）。

---

## 10. 未検証の点

実装前に PostgreSQL 17 の実機かソースで確かめる。

- `pg_depend` の PostgreSQL の全行（名前空間への依存、CHECK の列依存、`pg_attrdef` → 列の `refobjsubid`）のうち、この章が書かないもの（D07-16）が、`DROP` の挙動に影響しないこと。
- `ALTER TABLE ... OWNER TO` を `pg_class`（システムカタログ）に対して実行したときの文言（§5.5）。
- `DROP` の `2BP01` の DETAIL が 100 行を超えるときの文言（`and N other objects (see server log for list)` の閾値と文面）。
- `deduplicate_items` の不正な値のメッセージ。`fillfactor = 50.5` を PostgreSQL が受け付ける丸めの規則（`rint`）。`WITH (autovacuum_enabled=false)` などの扱い。
- 空のテーブルに対する索引の `relpages` / `reltuples` の PostgreSQL の値（実測: `1` / `0`。yuzhu は `2` / `0`）。`CREATE INDEX` の後の**表**の `relpages` / `reltuples` の更新（実測: `5` / `1000`。yuzhu は更新しない）。
- `VACUUM` が `relhasindex` を `f` に戻す条件（実測: 索引が 0 件の表。yuzhu は何もしない）。
- 複数文の Simple Query の途中の `VACUUM` が `25001` になること（実測: `select 1; vacuum` は `VACUUM cannot run inside a transaction block`）を、yuzhu の暗黙のブロックの実装（M3）が同じに扱うこと。
- `begin_scan_all` の `HeapTuple.row` が全ユーザー列を持つこと、`tuple_state` が自分の挿入（`xmin = own`）を `Live` と返すこと（H4 / 06 の確認）。
- 長いキーの `Key (a)=(...) is duplicated.`: PostgreSQL は切り詰めない（実測: 5000 文字）が、`BuildIndexValueDescription` の `maxlen` の細部（SELECT 権限がないときの省略）は M4 の単一スーパーユーザーでは起きない。
- `CREATE INDEX` の `IF NOT EXISTS` の名前の判定と列の検査の順序（§5.2 の注）の実機での確認（`create index if not exists e1_a on e1 (zz)` の結果）。
- クラッシュ後の孤児ファイル（§5.11）を、起動時の掃除なしで M3 の `OID` の採番（`storage_exists` の確認）が避けること（M2-Q24）。

---

## 11. 確認事項

仮決めのままでよいか確認してください。★はディスク形式に関わるもの。IDは `11-tests-plan.md` が `M4-Q` の通し番号に振り直します。

- **★[07-Q1] カタログの追加と行の値**: 追加するカタログは 9 個（`pg_index`、`pg_depend`、`pg_sequence`、`pg_language`、`pg_opfamily`、`pg_opclass`、`pg_amop`、`pg_amproc`、`pg_description`。後者は空）。行型（`reltype`）は作らず 0、`pg_language.lanvalidator` は 0。`pg_depend` には PostgreSQL の全部ではなく §3.6 の表の依存だけを書く。理由: 00 の D-23 と M2 の方針。M4 に `DROP SCHEMA` や外部キーがなく、書かない依存は DROP の挙動に影響しない。**変えたい場合**: `pg_depend` に名前空間と CHECK の列の依存を足すと、書き込みが 1 表あたり数行増え、`pg_dump` の依存の並べ替えと完全に一致する。後から足せる（`CATALOG_VERSION_NO` を上げて initdb のやり直し）。
- **★[07-Q2] `int2[]` / `int2vector` のディスク形式**: `pg_index.indkey`・`indoption`、`pg_constraint.conkey` は `Datum::Int2Vector`（varlena + `i16` LE の並び。PostgreSQL の配列ヘッダなし。00 §12.3）。理由: M4 の範囲では 1 次元の `int2` の配列しか要らない。**変えたい場合**: M5 で配列型を入れるとき、配列の符号化を PostgreSQL の形式（ヘッダ + 次元 + NULL ビットマップ）に変えると、`conkey` / `indkey` の既存の行は読み直しが要り、`CATALOG_VERSION_NO` を上げて initdb のやり直し（M2-Q20 の方針どおり許容）。
- **[07-Q3] 1 コマンドで同じカタログ行を 2 度更新しない**（D07-3）。理由: yuzhu の `cid` は文の終わりでしか進まず、同じコマンドで挿入した行は見えない。PostgreSQL の `CommandCounterIncrement` を DDL の途中で挟む方式にすると、`TableStore` と `Snapshot` の契約（M2 の可視性）を変える。**変えたい場合**: `Transaction::command_counter_increment` を DDL の途中で呼べるようにする（`DdlCtx` が `snapshot` を取り直す）。M5 の複数ライターでも必要になる可能性がある（工数 +0.5 日、リスクは M2 / M3 のコマンド ID の不変条件）。
- **[07-Q4] 一意索引の構築の重複検出は C1 が行う**（D07-7）。`IndexStore::build` には常に `BuildUnique::No` を渡す。理由: 死んだ版を索引に入れるので、隣接比較だけの検出は UPDATE した行を誤検出する。**変えたい場合**: `build` の入力に `live: bool` を足し、`BuildUnique::Yes` のときは生きている版だけを比べる（06 の変更。C1 の `build_from_heap` の手順 4 が消える。工数は同じ）。
- **[07-Q5] `OWNER TO` を実装する**（D07-11）。00 の D-12 は「何もしない」。理由: 何もしないと `\dt` の Owner が誤る。実装は小さい（+0.25 日）。**変えたい場合**: 受け付けて何もしない（ロールの存在検査だけ）にする。M5 で一般ロールを入れるまでは、superuser しかいないので差は見えない。
- **[07-Q6] `ANALYZE` はトランザクションブロックの中でも成功する**（D07-13）。00 の D-11 は両方 `25001`。実測は `VACUUM` だけ `25001`。**変えたい場合**: なし（PostgreSQL と同じにしてあるだけ）。
- **[07-Q7] DROP の依存: `pg_depend` の閉包と二重の網**（D07-2）。**変えたい場合**: 表の部品を直接のキーだけで消す（M2 方式）にすると `pg_depend` の記録は要らなくなるが、`DROP TABLE ... CASCADE` の連鎖（他の表の既定値）と M5 の外部キーを後で作り直すことになる。
- **[07-Q8] `WITH (...)` の reloptions は `fillfactor`（表・索引）と `deduplicate_items`（索引）だけ**、ほかは `22023 unrecognized parameter`。`fillfactor=50.5` は 22023（PostgreSQL は受け付ける）。理由: M4 の範囲。**変えたい場合**: pg_dump が出す `autovacuum_enabled` などを受け付けて捨てる名前の一覧を足す（`ddl/mod.rs` の定数表。+0.1 日）。M5 の pg_dump のリストアで必要になる可能性が高い。
- **[07-Q9] `TRUNCATE ... RESTART IDENTITY` のシーケンスの更新はトランザクショナルでない**（08 のその場の上書き。PostgreSQL は ROLLBACK で戻る）。**変えたい場合**: 08 の `SequenceStore::reset` を新しい relfilenode を作る方式にする（TRUNCATE と同じ。08 の工数 +0.5 日）。
- **[07-Q10] `ALTER TABLE ... DROP CONSTRAINT` を M4 に入れない**（D07-21、00 の D-12）。PRIMARY KEY の索引は `DROP TABLE` でしか消せない。**変えたい場合**: `plan_drop` が制約をルートにできるので、`DROP CONSTRAINT [IF EXISTS] name [CASCADE | RESTRICT]` は +0.5 日（`pg_constraint` の行と所有する索引を消す。`relchecks` の更新が要る CHECK の削除は +0.25 日）。
- **[07-Q11] `CREATE INDEX CONCURRENTLY` / `DROP INDEX CONCURRENTLY` を通常と同じに動かす**（D07-8）。**変えたい場合**: `0A000` にする（ORM のマイグレーションが `CONCURRENTLY` を付けると失敗する）。
- **[07-Q12] `relhasindex` を `DROP INDEX` で下ろさない**（D07-9）。M4 の `VACUUM` は何もしないので、下ろされる機会がない。**変えたい場合**: `DROP INDEX` で下ろす（`pg_class` の更新が 1 回増える。PostgreSQL と違う値が見える）。または VACUUM でカタログを更新する（VACUUM が XID を取るようになる）。
- **[07-Q13] 索引の `relpages` は空でも 2、`reltuples` は構築した件数**（PostgreSQL は空で `1`、`0`）、**表の `relpages` / `reltuples` は更新しない**。**変えたい場合**: `ANALYZE` が更新する（M5）。
- **[07-Q14] 索引・シーケンスの名前を `table()` で引くと `None`**（D07-18）。DML / SELECT で索引を指定したときの SQLSTATE は PostgreSQL の `42809 cannot open relation "x"`（実測）を返すのが望ましいが、**アナライザ側（N1）の作業**。この章では DDL（DROP / CREATE INDEX / ALTER / TRUNCATE）だけ実測の文言にする。
- **[07-Q15] `name` 型の列の索引の `atttypid`** は `name` のまま（PostgreSQL は `cstring`）（D07-17）。`\d` の出力には影響しない。
- **[07-Q16] 自動名を実行時に決める**（D07-4）。00 の `BoundIndexConstraint.name: String` を `Option<String>` にする（00 への変更提案 1）。**変えたい場合**: 解析時に決める。同じ文の中の衝突（`unique (a, b)` と `unique (a_b)` が両方 `t_a_b_key`）の解決のために、解析が文の内の名前を覚える必要があり、アナライザの責任が増える。
- **[07-Q17] CHECK の自動名の衝突判定を名前空間の全制約に広げる**（PostgreSQL の `ChooseConstraintName`）。M2 は同じ表の中だけを見ていた。**変えたい場合**: 同じ表の中だけのまま（まれな差。2 つの表の名前と列名の組が同じ文字列になる場合だけ）。
- **[07-Q18] クラッシュで中断した DDL の孤児ファイル**を掃除しない（M3 の D15、M3-Q5）。大きな表への `CREATE INDEX` の途中でクラッシュすると、数百 MB の孤児ファイルが残りうる。**変えたい場合**: 起動時の孤児掃除（M5。カタログに relfilenode がないファイルを消す）。

---

## 12. 00 への変更提案と、他の章への依頼

### 12.1 00 への変更提案（統合時に反映する）

1. **§7 `BoundIndexConstraint` と `BoundCreateTable`**: `BoundIndexConstraint { name: Option<String>, kind, columns: Vec<i16>, options: Vec<RelOption>, span: Span }`（自動名は実行時。D07-4、[07-Q16]）。`BoundCreateTable` に `options: Vec<RelOption>` と `default_refs: Vec<(i16, Oid)>` を足す。そのほかの `BoundDdl` の各 `Bound*` の中身は **この章の §4.6** を正とする（`BoundCreateIndex`、`BoundDropTable`（`behavior` を追加）、`BoundDropIndex`、`BoundAlterTableAddConstraint`、`BoundAlterTableOwner`、`BoundTruncate`、`BoundVacuum`）。
2. **§14.6 `DdlCtx`**: `in_transaction_block: bool`（`CONCURRENTLY` と VACUUM の `25001`）と `interrupts: &'a InterruptFlag`（長いループの中断。00 §4.3 の 4）を足す。
3. **§2 D-11**: 「`VACUUM` / `ANALYZE` は何もせず成功（トランザクションブロック内は `25001`）」を「`VACUUM` はブロック内で `25001`、**`ANALYZE` はブロック内でも成功**（PostgreSQL 17 の実測）」に直す。
4. **§2 D-12**: 「`OWNER TO`（何もしない）」を「`OWNER TO`（ロールを検証し、表・索引・所有シーケンスの `relowner` を更新）」に直す（[07-Q5]）。
5. **§11.2 `CatalogReader`**: `fn constraint_by_oid(&self, oid: Oid) -> Result<Option<ConstraintDef>>` を足し、`ConstraintDef` / `ConstraintKind` を §11.1 に足す（この章 §4.1、§4.2）。`pg_get_constraintdef` と `\d` が使う。
6. **§11.5 追加するカタログ**: `pg_description` は**空**、`pg_language` は 3 行（`lanvalidator = 0`）と確定する。`pg_class` の `reltype` は 0。
7. **§13.1 `TableStore::tuple_state`**: 「`own` は呼び出し側のトランザクションの XID。**自分が挿入して削除していない版は `Live`**、自分が削除した版は `DeletedBySelf`。`InsertInProgress(x)` / `DeleteInProgress(x)` は `x != own` のときだけ」と契約を足す（§5.2.1）。
8. **§13.2 `IndexStore::build`**: C1 は `BuildUnique::No` でしか呼ばない（D07-7、[07-Q4]）。`BuildUnique::Yes` の意味は 06 が決める。06 に `storage::btree::cmp_keys(index: &IndexHandle, a: &[Datum], b: &[Datum]) -> Ordering`（木の順序そのもの）の公開を依頼する（C1 のソートが使う。なければ `cmp_with_nulls` を列ごとに使う）。
9. **§4 モジュール構成**: `catalog/naming.rs`（`make_object_name` を analyzer から移す。移すのは C1、`analyzer/ddl.rs` 側の削除と呼び出しは Q1。§12.2）、`catalog/depend.rs`、`catalog/check.rs`（テスト用）、`ddl/vacuum.rs`、`analyzer/ddl_constraint.rs`（C1 が書く。00 §17 の C1 の範囲に足す）を足す。
10. **§15.3 SQLSTATE**: 追加は不要（`DEPENDENT_OBJECTS_STILL_EXIST` は 00 にある）。ただし `42939`（`role name "none" is reserved`）は S1 が `RESERVED_NAME` として足す。
11. **§12.1 型**: この章は `int2vector`（22）・`_int2`（1005）の行を 09 に要求する（§3.8）。

### 12.2 他の章への依頼

| 宛先 | 依頼 |
|---|---|
| `06-btree.md`（B1・B2・H4） | (a) `catalog/opclass.rs` の `default_opclass(type_oid)` は、**`varchar` → `text_ops`、`regclass` / `regtype` / `regproc` → `oid_ops`** のバイナリ互換の解決まで含めてもよい（C1 は `resolve_opclass` の中で補っているので、06 が入れたら C1 の `coercible_index_type` を消す）。(b) `AMOPS` の演算子はすべて `builtin::OPERATORS` に実在させ、`AMPROCS` の関数は `builtin::PROCS` に実在させる（§3.7、§3.8 の 30 個の `pg_proc` の行）。(c) `BtreeStore` が**リレーションごとのメモリ上の状態**（ルートのキャッシュなど）を持つなら、`unlink_storage` と TRUNCATE での新しい relfilenode への切り替えで無効になること。持たない前提で C1 は書く（コミットの unlink は `TableStore::unlink_storage` 1 本。M3 §5.3）。(d) `IndexHandle::from_def` が `IndexDef.columns[i].opfamily` を使って比較関数を引くこと。(e) `begin_scan_all` が全ユーザー列を返し、`tuple_state` が §12.1 の 7 の契約であること。(f) `IndexStore::init_index` / `build` が WAL を書いた LSN まで、コミットの flush で永続化されること（M3 の通常のコミットの経路でよい）。 |
| `08-sequence-serial.md`（Q1） | (a) **`ddl::sequence::create_with_oid(ctx, &BoundCreateSequence, oid, Option<(Oid, i16)>) -> Result<()>`**（§5.1 の手順 6。OID は 07 が手順 4b で先に採る。名前は解析時に決まっている）と **`ddl::sequence::restart_owned_by_table(ctx, table_oid)`**（§5.6 の手順 4）を提供する（レビュー対応 R-07。以前の `create_sequence_for_table` / `restart_to_start` は 08 に存在しない名前だった。`pg_depend` の `ObjectAddress` / `NewDepend` / `DependType`、`CatalogStore::{record_dependency, delete_dependencies, dependents_of, references_of, describe_object}`、`plan_drop` / `drop_objects` は **07 の `catalog/depend.rs` と `CatalogStore`** を使い、08 が書いていた `ddl/depend.rs` の `record` / `dependents_of` / `dependencies_of` / `delete_dependencies` / `describe` は作らない。R-08）。(b) シーケンスのカタログの行は `CatalogStore::create_sequence`（§4.3）で書く（`pg_class`（`S`）、`pg_attribute`、`pg_sequence`、`pg_depend`）。(c) `DROP SEQUENCE` は `catalog::depend::plan_drop` を呼ぶ（SERIAL: `DETAIL: default value for column id of table o1 depends on sequence o1_id_seq`、IDENTITY: `cannot drop sequence s1_g_seq because column g of table s1 requires it`）。(d) SERIAL の列の既定値の `pg_depend`（`pg_attrdef` → シーケンス、`n`）は C1 が書く（`NewTable.extra_depends`）。08 は書かない。 |
| `03-parser-analyzer.md`（S1、N1〜N3） | (a) AST: `DropTable.cascade`、`DropIndex { names, if_exists, concurrently, cascade }`、`CreateIndex { name: Option<Ident>, table, unique, if_not_exists, concurrently, method: Option<Ident>, columns（式・opclass・`COLLATE`・ASC / DESC・NULLS）, include, options, where }`、`AlterTable { name, if_exists, only, action: AddConstraint(TableConstraint) \| OwnerTo(RoleSpec) \| Other }`、`Truncate { tables, restart_identity, cascade, only }`、`Vacuum { vacuum: bool, options: Vec<(Ident, Option<String>)>, targets: Vec<(ObjectName, Vec<Ident>)> }`、列制約・表制約の `PRIMARY KEY` / `UNIQUE` の `WITH (..)`・`DEFERRABLE`・`INCLUDE` など、`CREATE TABLE ... WITH (..)`。(b) `0A000` の文言（§6.9、§7.3）。(c) `resolve_table` を `relation_kind` で書き換え、索引・シーケンスの指定を §4.6 の `42809` にする（`analyzer/resolve.rs`。N1）。**(d)〜(f) は `analyzer/ddl.rs` の持ち主（11 §4.2 では Q1）の作業で、N1〜N3 ではない**（レビュー対応 R-09。以前はここに書いていたが N1〜N3 は `ddl.rs` を持たず日数もなかった。下の `08-sequence-serial.md` の行に移した）。(f) `DROP TABLE` / `DROP INDEX` / `TRUNCATE` / `VACUUM` / `ALTER TABLE` の解析は C1 の `analyzer/ddl_index.rs`（11 §7.3 の G-1）。 |
| `analyzer/ddl.rs` の持ち主 Q1（`08-sequence-serial.md` と共同。レビュー対応 R-09） | (d) CHECK の自動名を `catalog::naming::choose_constraint_name` に置き換える（M2 の `used` を廃止）。(e) `analyze_create_table` から `analyze_index_constraints`（C1 の `analyzer/ddl_constraint.rs`、§6.9）を呼び、解析済みの既定値から `collect_regclass_refs`（C1 の `ddl/depend.rs`）で `default_refs` を集める。(g) **`make_object_name` は C1 が `catalog/naming.rs` に移す**（C1 が作る）。Q1 は `analyzer/ddl.rs` からその定義を削除して `catalog::naming::make_object_name` を呼ぶ。暗黙のシーケンスの名前は `catalog::naming::choose_relation_name(表名, Some(列名), "seq", nsp, false, &lookup, &taken)`（`lookup` は `CatalogReader` を包む `NameLookup`、`taken` は同じ文の中で先に決めた名前）。08 が `analyzer/ddl.rs` に別の `choose_relation_name` を定義することはしない（二重定義の解消）。Q1 の日数に +0.5 日（11 §4.1）。 |
| `09-types-functions.md`（T1〜T3） | `int2vector` と `int2[]` の入出力（`{1,2}`、`1 2`）、`regclass` リテラルの入出力、`quote_identifier`、`format_type_name`、`pg_get_*` の関数。AMOPS に出てくる比較演算子（date / timestamp / timestamptz の相互を含めるかは 09 が決め、含めなければ 06 に伝えて `AMOPS` から外す）。 |
| `10-explain-copy-compat.md`（E1、O1、S） | (a) E1: `pg_get_indexdef` と `pg_get_constraintdef` は `CatalogReader::index_by_oid` / `constraint_by_oid` / `table_by_oid` で書く。(b) O1: `TRUNCATE` の後の COPY は、TRUNCATE で更新された `TableDef`（新しい `locator`）から `RelHandle` を作る（`catalog_dirty` で `bypass_cache` になり、読み直される）。(c) S: §6.11 の session の作業、`Notice::new` の `pub(crate)` 化。 |
| `11-tests-plan.md`（K、Z、R2） | §8 のテストの一覧を集約する。`consistency.slt`（SQL 版）を全スイートの最後に流すこと、クラッシュ試験の DDL のワークロード（§8.5）。 |
| `04-planner-optimizer.md`、`05-executor.md` | `TableDef.indexes` は OID の昇順で、`IndexColumn.opfamily` が埋まっている。インデックス選択（`index_select.rs`）は `TableDef.indexes` を使い、索引の有無に依存するプランは DDL のコミットで `TableDef` が入れ替わる（プランのキャッシュはない）。`insert_with_indexes` は `rel.indexes`（`TableDef.indexes` の写し）の順に索引を更新する。 |
