# yuzhu M2 調査レポート: システムカタログのテーブル化と initdb（ブートストラップ）

M2 では、M1 のメモリ上のカタログ（`catalog/memory.rs` の `MemoryCatalog` と `catalog/builtin.rs` の静的な表）を、**ディスク上のヒープテーブルとして持つシステムカタログ**に置き換えます。この文書では、PostgreSQL 17 のブートストラップ、カタログの構成、キャッシュと無効化の仕組みを調べ、yuzhu が M2 で何をどう実装するかを推奨します。

- 前提: `spec/design/m1.md`（M1 の契約）、`spec/research/research-pg-protocol.md` §3（psql のメタコマンドが使うカタログ）
- 「確認済み」は REL_17_STABLE のソースを取得して読んだもの、**（未確認）** は記憶や推測によるものです。
- 工数の目安は「1 人の実装者が集中して作業した場合」の概算です。

---

## 0. 出典（REL_17_STABLE）

| 内容 | URL |
|---|---|
| カタログ定義（列・OID・BKI 属性） | <https://github.com/postgres/postgres/tree/REL_17_STABLE/src/include/catalog>（`pg_class.h`, `pg_attribute.h`, `pg_type.h`, `pg_namespace.h`, `pg_database.h`, `pg_authid.h`, `pg_proc.h`, `pg_cast.h`, `pg_operator.h`, `pg_attrdef.h`, `pg_constraint.h` と各 `.dat`） |
| OID の範囲 | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/access/transam.h>（`FirstGenbkiObjectId`=10000, `FirstUnpinnedObjectId`=12000, `FirstNormalObjectId`=16384。確認済み） |
| OID 割り当ての規約 | <https://www.postgresql.org/docs/17/system-catalog-initial-data.html>（§"OID Assignment"、ソースは `doc/src/sgml/bki.sgml`。確認済み） |
| BKI 形式とブートストラップ | <https://www.postgresql.org/docs/17/bki.html>、`src/backend/catalog/genbki.pl`、`src/backend/bootstrap/bootparse.y`、`bootstrap.c` |
| initdb の手順 | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/initdb/initdb.c>（`initialize_data_directory`, `subdirs[]`。確認済み） |
| relmapper | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/cache/relmapper.c>（`pg_filenode.map`、magic `0x592717`、`MAX_MAPPINGS`=64、CRC 付き、一時ファイル + rename で置き換え。確認済み） |
| キャッシュ無効化 | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/cache/inval.c>（冒頭コメント。確認済み）、`catcache.c`、`syscache.c`、`relcache.c`、`src/backend/storage/ipc/sinvaladt.c` |
| 固定記述子（nailed relations） | `relcache.c` の `formrdesc("pg_class" / "pg_attribute" / "pg_proc" / "pg_type" / "pg_database" / "pg_authid" / ...)`（確認済み） |
| OID カウンタ | `src/backend/access/transam/varsup.c` の `GetNewObjectId`、`src/backend/catalog/catalog.c` の `GetNewOidWithIndex` / `GetNewRelFileNumber`（確認済み） |
| ファイル配置 | <https://www.postgresql.org/docs/17/storage-file-layout.html> |
| 制御ファイル | `src/include/catalog/pg_control.h`（`PG_CONTROL_FILE_SIZE`=8192, `PG_CONTROL_VERSION`=1700。確認済み） |
| psql のメタコマンド | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/psql/describe.c>（`listTables`, `listAllDbs`, `printACLColumn`。確認済み） |

---

## 1. PostgreSQL の仕組み（要点）

### 1.1 genbki.pl と *.dat、ブートストラップモード

- カタログの列は `src/include/catalog/pg_*.h` の `CATALOG(...)` 構造体で定義される。初期データは `pg_*.dat`（Perl のハッシュ形式）にある。ビルド時に `genbki.pl` がこれらを読み、`postgres.bki`（BKI 言語のスクリプト）と、OID 定数のヘッダ（`pg_*_d.h`）を生成する。
- `.dat` では参照を名前で書ける（`BKI_LOOKUP`。例: `relnamespace => 'pg_catalog'`, `typinput => 'int4in'`）。genbki.pl が OID に変換する。
- initdb は `postgres --boot` を起動し、`postgres.bki` を流し込む。BKI の主な命令は `create <name> <oid> [bootstrap] [shared_relation] [rowtype_oid N] (cols...)`、`open`、`insert ( ... )`、`close`、`declare [unique] index`、`build indices`（docs: bki.html）。
- ブートストラップモードでは、トランザクション ID は `BootstrapTransactionId`(=1) を使う（transam.h で確認済み）。XID 1 と 2（Frozen）は「常にコミット済み」と扱われるので、ここで作った行はその後のどのスナップショットからも見える。
- カタログのインデックスは、すべての行を入れてから最後にまとめて作る（`build indices`）。

### 1.2 4 つのブートストラップカタログ（BKI_BOOTSTRAP）と固定記述子

- `pg_class`(1259)、`pg_attribute`(1249)、`pg_proc`(1255)、`pg_type`(1247) は `BKI_BOOTSTRAP` が付いている（確認済み）。`pg_class.dat` にはこの 4 つの行だけが書かれている。
- 鶏と卵の問題: 「pg_class を読むには pg_class の行が要る」。PostgreSQL は `relcache.c` の `formrdesc()` で、この 4 つと共有カタログ（`pg_database`, `pg_authid`, `pg_auth_members`, `pg_shseclabel`, `pg_subscription`）の記述子を **C のコードに埋め込んだ列定義（`Schema_pg_class` など、genbki が生成）から作る**（確認済み）。これらは "nailed"（キャッシュに釘付け）と呼ばれる。
- 起動を速くするため、relcache は `pg_internal.init`（relcache init file）にカタログの記述子を保存する。カタログが変わるとこのファイルを消す（inval.c 冒頭コメントで確認済み）。

### 1.3 oid と relfilenode、relmapper

- `pg_class.oid` はリレーションの論理的な ID、`pg_class.relfilenode` はディスク上のファイル名。普段は同じ値だが、`TRUNCATE`・`VACUUM FULL`・`CLUSTER`・一部の `ALTER TABLE` はファイルを作り直すので、relfilenode だけが変わる（docs: storage-file-layout.html）。
- 新しい relfilenode は `GetNewRelFileNumber()` が OID カウンタから取り、同じ名前のファイルが無いことを確かめる（catalog.c で確認済み）。
- **マップされたカタログ**（共有カタログと、pg_class/pg_attribute/pg_proc/pg_type とそのインデックスなど）は、`pg_class.relfilenode = 0` とし、実際のファイル番号を `pg_filenode.map` に置く。共有カタログの表は `global/pg_filenode.map`、DB ごとのものは `base/<dboid>/pg_filenode.map`。理由は、pg_class 自身のファイル番号を pg_class に書くと、それを読むために pg_class を読む必要が出るから。マップファイルは 512 バイト未満の固定長で CRC 付き、置き換えは一時ファイル + rename で行う（relmapper.c で確認済み）。
- それ以外のカタログ（pg_namespace など）は、普通に `pg_class.relfilenode` を見る。

### 1.4 OID の割り当て

| 範囲 | 用途（transam.h / bki.sgml で確認済み） |
|---|---|
| 1–9999 | `.dat` で手で割り当てる組み込みオブジェクト。リリース後は変えない（クライアントが決め打ちしてよい値）。8000–9999 は開発中のパッチ用 |
| 10000–11999 | genbki.pl が自動で割り当てる（`FirstGenbkiObjectId`）。ブートストラップ中に作られるものもここ。**版ごとに変わりうる** |
| 12000–16383 | initdb の後半（information_schema など）。12000 未満は "pinned"（削除不可） |
| 16384– | 通常のユーザーオブジェクト（`FirstNormalObjectId`）。OID カウンタが一周したら 16384 に戻る（varsup.c で確認済み） |

- カウンタが一周したあとの重複は、`GetNewOidWithIndex()` が対象カタログの OID インデックスを引いて避ける（catalog.c で確認済み）。
- OID カウンタの永続化: `GetNewObjectId` は 8192 個ずつ先取りして WAL（`XLOG_NEXTOID`）に記録する（**未確認**: 値 `VAR_OID_PREFETCH`=8192 は記憶による）。

### 1.5 キャッシュ（syscache / catcache / relcache）と無効化

- **catcache / syscache**: カタログの行を「インデックスのキー → タプル」でキャッシュする（`syscache.c` に約 80 種。例: `RELOID`, `RELNAMENSP`, `TYPEOID`, `ATTNUM`）。「無い」という結果（negative entry）もキャッシュする。
- **relcache**: リレーションごとの `RelationData`（列定義 `TupleDesc`、制約、DEFAULT、インデックス一覧など）。pg_class・pg_attribute・pg_attrdef・pg_constraint などから組み立てる。
- **無効化**（inval.c 冒頭コメントの要約。確認済み）:
  1. カタログ行を更新・削除しても、同じコマンドの中では古い行がまだ有効。だから更新した時点ではキャッシュを捨てず、**捨てるべきものの一覧を記録しておく**。
  2. コマンドの区切り（`CommandCounterIncrement`）で、自分のバックエンドのキャッシュから捨てる。
  3. コミットしたら、**コミットを記録した後で** SI（shared invalidation）キューに流して他のバックエンドに知らせる。
  4. アボートしたら、自分が入れた行・消した行に関係するエントリを自分のキャッシュから捨てる（他には何も送らない）。
  5. pg_class・pg_attribute・pg_index の行が変わったら、そのリレーションの relcache を捨てる。
- 他のバックエンドは、**ロックを取るたび**（`LockRelationOid` → `AcceptInvalidationMessages`）に SI キューを読む（**未確認**: lmgr.c の該当箇所は記憶による）。DDL は AccessExclusiveLock を取るので、DDL のコミット後に同じテーブルのロックを取った読み手は、必ず新しい定義を見る。
- カタログの読み取りは、トランザクションのスナップショットではなく**最新のカタログスナップショット**で行う（inval.c 冒頭コメントで確認済み）。

### 1.6 DEFAULT と CHECK の保存形式

- `pg_attrdef(oid, adrelid, adnum, adbin pg_node_tree)`。`pg_attribute.atthasdef = true` にする（確認済み）。
- `pg_constraint` の CHECK は `contype = 'c'`、式は `conbin pg_node_tree`（確認済み）。`pg_class.relchecks` に CHECK の個数を入れる。
- `adbin`/`conbin` は内部の木を `nodeToString` で文字列にしたもの。列は名前ではなく `attnum` で参照するので、列名を変えても壊れない。人が読む形は `pg_get_expr(adbin, adrelid)` や `pg_get_constraintdef(oid)` で逆変換して得る。
- 以前あった `adsrc`/`consrc`（ソーステキスト）は PostgreSQL 12 で削除された（**未確認**: 版数は記憶による）。
- NOT NULL は PG17 では `pg_attribute.attnotnull` だけで表す。`pg_constraint.h` には `CONSTRAINT_NOTNULL 'n'` が定義されているが、テーブルの NOT NULL を pg_constraint に入れるのは PG18 から（**未確認**）。

---

## 2. psql の `\dt` と `\l` が必要とするもの

`research-pg-protocol.md` §3.2 の内容を、describe.c（REL_17_STABLE）で再確認しました。

### 2.1 `\dt`（`listTables`）

- カタログ: `pg_class(oid, relname, relnamespace, relkind, relowner)`、`pg_namespace(oid, nspname)`。`\dt+` ではさらに `relpersistence`、`relam` と `pg_am(oid, amname)`（server_version 12 以上）、`pg_table_size`、`pg_size_pretty`、`obj_description`。
- 関数・演算子: `pg_get_userbyid(oid)`（OID 1642）、`pg_table_is_visible(oid)`（2079）、`text !~ text`（演算子 642、関数 `textregexne` 1256）。パターン付き（`\dt foo*`）では `OPERATOR(pg_catalog.~)`（641、`textregexeq` 1254）と `COLLATE pg_catalog.default`。
- SQL 機能: `LEFT JOIN`、`CASE`、`IN`、`ORDER BY 1,2`。**LEFT JOIN は M4 の範囲なので、M2 で `\dt` を動かすなら JOIN（少なくとも Nested Loop の LEFT JOIN）を前倒しする必要がある。**

### 2.2 `\l`（`listAllDbs`）

```sql
SELECT d.datname as "Name",
  pg_catalog.pg_get_userbyid(d.datdba) as "Owner",
  pg_catalog.pg_encoding_to_char(d.encoding) as "Encoding",
  CASE d.datlocprovider WHEN 'b' THEN 'builtin' WHEN 'c' THEN 'libc' WHEN 'i' THEN 'icu' END AS "Locale Provider",
  d.datcollate as "Collate", d.datctype as "Ctype",
  d.datlocale as "Locale",            -- sversion >= 17。15〜16 は d.daticulocale
  d.daticurules as "ICU Rules",       -- sversion >= 16
  CASE WHEN pg_catalog.array_length(d.datacl, 1) = 0 THEN '(none)'
       ELSE pg_catalog.array_to_string(d.datacl, E'\n') END AS "Access privileges"
FROM pg_catalog.pg_database d
ORDER BY 1;
```

- 関数: `pg_get_userbyid`、`pg_encoding_to_char(int4)`（1597）、`array_length(anyarray, int4)`（2176）、`array_to_string(anyarray, text)`（395）。
- `datacl` は `aclitem[]`。**yuzhu では M2 の間ずっと NULL にしておけば、`array_length`/`array_to_string` を strict 関数として登録するだけで済む**（strict なので引数が NULL なら本体を呼ばずに NULL を返す。本体は 0A000 を返す実装でよい）。`E'...'` リテラルは字句解析で対応が必要。
- **server_version によって参照する列が変わる**: 16 なら `daticulocale`、17 なら `datlocale`（確認済み）。PG17 の `pg_database` には `daticulocale` は無い。

### 2.3 server_version の見直し（Q-004 への提案）

- M1 で `server_version=16.0` にした理由は「17 以降の `\d` が新しいカタログ列を参照するため」でした。describe.c を grep したところ、`sversion >= 170000` の分岐があるのは `listAllDbs`（`\l`）、`listCollations`（`\dO`）、`describeSubscriptions`（`\dRs`）の 3 か所だけで、**`describeOneTableDetails`（`\d tbl`）には無い**（確認済み）。
- 正解の基準は postgres:17 で、調査も REL_17_STABLE を参照している。カタログを PG16 と PG17 で作り分けるのは混乱のもとになる。
- **推奨: M2 でカタログを PG17 の列構成に合わせ、`server_version` を `17.0` に上げる。** QUESTIONS.md に Q-004 の改訂として記録する。

### 2.4 M2 で目指す範囲

| メタコマンド | M2 | 必要なもの |
|---|---|---|
| `\dt`, `\dt+` | 動かす | 上記。LEFT JOIN の前倒しと、正規表現（`~`, `!~`）の最小実装 |
| `\l` | 動かす | 上記 |
| `\dn` | 動かす（**未確認**: クエリの中身は確認していない。pg_namespace と pg_get_userbyid、`!~ '^pg_'` 程度のはず） | |
| `\du` | 任意 | `pg_roles` ビュー。M2 ではビューが無いので、pg_authid から合成する仮想テーブルにする |
| `\d tbl` | M4 | pg_index・pg_inherits・pg_trigger など多数、サブクエリ、配列、regclass |

**正規表現について**: `\dt` は `!~ '^pg_toast'`、パターン付きは `~ '^(foo.*)$'` を使う。正規表現エンジンは「コア部品」の一覧に入っていないので、`regex` クレートを使う案と、`^ $ . * + ? [] () |` 程度を手で書く案（200〜400 行、1〜2 日）がある。推奨は**手書きの最小実装**（psql が生成するパターンは限られている。PostgreSQL の ARE と完全に同じにするのは難しいので、どちらの案でも差は出る）。QUESTIONS.md に記録する。

---

## 3. 推奨: M2 で持つカタログと列

### 3.1 方針

1. **OID・カタログ名・列名・列の順序は PostgreSQL 17 と同じにする。** 列は PG17 の定義を先頭から全部持つ（途中の列を飛ばさない）。こうすると `SELECT *` の結果が PG と同じ並びになり、あとで列を足してもディスク上の形式が変わらない。
2. yuzhu がまだ値を扱えない型の列（`aclitem[]`、`text[]`、`oid[]`、`int2[]`、`"char"[]`、`timestamptz`、`anyarray` など）は、**型だけ pg_type に登録し、値は常に NULL** とする（以下「NULL 専用列」）。入力関数は NULL 以外が来たら `0A000` を返す。
3. カタログの形式を変えたら **カタログバージョン（catversion）** を上げる。制御ファイルの値と合わなければ起動を拒否し、initdb のやり直しを求める（PostgreSQL の `CATALOG_VERSION_NO` と同じ運用）。M5 までは形式の変更を気にせず進められる。
4. 列の型のうち、M1 に無いものは次の通り。値を扱うものは M2 で実装する。

| 型 | OID（配列型） | M2 での扱い |
|---|---|---|
| `"char"` | 18（1002） | 値を扱う（relkind, relpersistence, typtype, contype など）。1 バイト。出力は文字そのもの |
| `regproc` | 24（1008） | 値を扱う（typinput, oprcode）。出力は関数名、入力は名前から OID を引く。M2 は出力だけでも可 |
| `tid` | 27（1010） | システム列 `ctid` 用 |
| `xid` | 28（1011） | 値を扱う（relfrozenxid, datfrozenxid, システム列 xmin/xmax） |
| `cid` | 29（1012） | システム列 cmin/cmax 用 |
| `oidvector` | 30（1013） | 値を扱う（proargtypes）。テキスト表現は空白区切りの OID 列（例 `23 23`） |
| `pg_node_tree` | 194（なし） | 値を扱う（adbin, conbin）。中身はテキスト（§6） |
| `aclitem` | 1033（1034） | NULL 専用 |
| `timestamptz` | 1184（1185） | NULL 専用（rolvaliduntil）。本物は M5 |
| `anyarray` | 2277（配列型なし） | NULL 専用（attmissingval）。疑似型 |

- `regclass`(2205)、`regtype`(2206)、`regnamespace`(4089) は `\d` 系で必要になる M4 まで先送りしてよい。

### 3.2 M2 で作るカタログ

**共有カタログ（`global/` に置く。全データベースで 1 つ）**

| カタログ | OID | M2 の初期行 | 備考 |
|---|---|---|---|
| `pg_database` | 1262 | template1(1)、template0(4)、postgres(5) | PG17 の列: `oid, datname, datdba, encoding, datlocprovider, datistemplate, datallowconn, dathasloginevt, datconnlimit, datfrozenxid, datminmxid, dattablespace, datcollate, datctype, datlocale, daticurules, datcollversion, datacl`。encoding=6（UTF8）、datlocprovider='c'、datcollate=datctype='C'、datlocale/daticurules/datcollversion/datacl は NULL |
| `pg_authid` | 1260 | OID 10 のスーパーユーザー（名前は initdb の `-U`、既定は OS のユーザー名。PG と同じ） | 列: `oid, rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, rolreplication, rolbypassrls, rolconnlimit, rolpassword, rolvaliduntil`。`pg_database_owner`(6171) などの既定ロールは M6 の GRANT まで省略可 |
| `pg_tablespace` | 1213 | pg_default(1663)、pg_global(1664) | 列: `oid, spcname, spcowner, spcacl, spcoptions`。`\l+` と `\db` のため。安価なので入れる |

**データベースごとのカタログ（`base/<dboid>/` に置く）**

| カタログ | OID | 初期行の出所 | 備考 |
|---|---|---|---|
| `pg_class` | 1259 | 全カタログの定義 | 列は PG17 の 33 列すべて（§3.3） |
| `pg_attribute` | 1249 | 全カタログの列定義 + システム列 | 列は PG17 の 26 列すべて。`attstattarget` から後ろは NULL 専用 |
| `pg_type` | 1247 | `builtin::TYPES` + 上の追加型 + 配列型 | 32 列。`typdefaultbin`/`typdefault`/`typacl` は NULL |
| `pg_namespace` | 2615 | pg_catalog(11)、pg_toast(99)、public(2200) | `nspowner` は 10。PG17 では public の所有者は `pg_database_owner` だが、そのロールを作らない間は 10 にする（差分として記録） |
| `pg_proc` | 1255 | `builtin::FUNCTIONS` + 演算子・キャスト・型入出力の実体関数 | §5.3 |
| `pg_operator` | 2617 | `builtin::OPERATORS` | |
| `pg_cast` | 2605 | `builtin::CASTS` | PG の `pg_cast.dat` は OID を手で振っていない（genbki が 10000 台を割り当てる。確認済み）。yuzhu も 10000〜11999 から振る |
| `pg_am` | 2601 | heap(2)、btree(403) | `\dt+` が JOIN する。btree の実体は M4 |
| `pg_attrdef` | 2604 | なし（ユーザーの DEFAULT） | §6 |
| `pg_constraint` | 2606 | なし（ユーザーの CHECK） | §6。PRIMARY KEY・UNIQUE（M4）、FOREIGN KEY（M5）もここに入る |

- **作らないもの**: `pg_index`（M4。B+Tree と同時）、`pg_depend`（M4 の SERIAL か M5 の FOREIGN KEY で必要になったとき）、`pg_description`、`pg_inherits`、`pg_trigger`、`pg_rewrite`、`pg_policy`、`pg_collation` など（`\d` 対応の M4 で、空のテーブルとしてまとめて作る）。
- **カタログのインデックス**は B+Tree と一緒に M4 で作る。OID は PG と同じものを予約しておく: `pg_class_oid_index` 2662、`pg_class_relname_nsp_index` 2663、`pg_attribute_relid_attnam_index` 2658、`pg_attribute_relid_attnum_index` 2659、`pg_type_oid_index` 2703、`pg_type_typname_nsp_index` 2704、`pg_namespace_nspname_index` 2684、`pg_namespace_oid_index` 2685、`pg_database_datname_index` 2671、`pg_database_oid_index` 2672、`pg_attrdef_adrelid_adnum_index` 2656、`pg_attrdef_oid_index` 2657、`pg_constraint_oid_index` 2667、`pg_constraint_conrelid_contypid_conname_index` 2665、`pg_proc_oid_index` 2690、`pg_operator_oid_index` 2688、`pg_cast_oid_index` 2660、`pg_cast_source_target_index` 2661（いずれも各 `.h` の `DECLARE_*_INDEX` で確認済み）。M2 はインデックスが無いので、キャッシュが外れたら順次走査する。カタログは小さいので問題にならない。

### 3.3 pg_class の値の決め方

| 列 | ユーザーテーブル | システムカタログ |
|---|---|---|
| `oid` | 新しく割り当てた OID（16384 以上） | PG と同じ固定 OID |
| `relname` / `relnamespace` | 名前 / 2200（public） | 名前 / 11 |
| `reltype` | **0**（M2 は行型を作らない。§7 参照） | ブートストラップ 4 カタログは PG と同じ 71/75/81/83、pg_database は 1248、pg_authid は 2842。ほかは 0 |
| `reloftype` | 0 | 0 |
| `relowner` | セッションユーザーの OID（M2 は常に 10） | 10 |
| `relam` | 2（heap） | 2 |
| `relfilenode` | 作成時に割り当てた番号（§4.3） | マップ対象は 0、それ以外は oid と同じ値 |
| `reltablespace` | 0（= データベースの既定） | 共有カタログは 1664、それ以外は 0 |
| `relpages` / `reltuples` / `relallvisible` | 0 / -1 / 0（統計は M6 のコストベース最適化で使う） | 同じ |
| `reltoastrelid` | 0（TOAST は作らない） | 0 |
| `relhasindex` | false（M4 で更新） | false |
| `relisshared` | false | 共有カタログは true |
| `relpersistence` | 'p' | 'p' |
| `relkind` | 'r' | 'r' |
| `relnatts` | 列の数（システム列は数えない） | 同じ |
| `relchecks` | CHECK 制約の数 | 0 |
| `relhasrules` / `relhastriggers` / `relhassubclass` / `relrowsecurity` / `relforcerowsecurity` | false | false |
| `relispopulated` | true | true |
| `relreplident` | 'd'（PG の通常テーブルの既定。**未確認**: カタログは 'n'） | 'n' |
| `relispartition` / `relrewrite` | false / 0 | 同じ |
| `relfrozenxid` / `relminmxid` | M2 は 3 / 1（PG の BKI 既定値。M3 で実際の XID） | 3 / 1 |
| `relacl` / `reloptions` / `relpartbound` | NULL | NULL |

---

## 4. データディレクトリの構成

### 4.1 推奨レイアウト

```
$YUZHU_DATA/
├── YUZHU_VERSION          データ形式の版（1 行のテキスト。PG の PG_VERSION に相当）
├── yuzhu.pid              二重起動防止のロックファイル（postmaster.pid に相当）
├── yuzhu.conf             設定（任意。M1 の config.rs の項目）
├── global/
│   ├── yuzhu_control      制御ファイル（8KB、CRC32C 付き。pg_control に相当）
│   ├── 1262               pg_database（共有カタログ。ファイル名 = relfilenode）
│   ├── 1260               pg_authid
│   └── 1213               pg_tablespace
├── base/
│   ├── 1/                 template1（データベース OID ごとのディレクトリ）
│   │   ├── YUZHU_VERSION
│   │   ├── 1259           pg_class
│   │   ├── 1249  1247  1255  2615  2605  2617  2601  2604  2606
│   │   ├── 16384          ユーザーテーブル（relfilenode）
│   │   ├── 16384.1        1GB を超えた 2 番目のセグメント
│   │   └── 16384_fsm      フォーク（名前だけ予約。FSM/VM は M5 の VACUUM で）
│   ├── 4/                 template0
│   └── 5/                 postgres
├── pg_wal/                予約（M3 の WAL。M2 は空のディレクトリを作るだけ）
└── pg_xact/               予約（M3 のコミットログ。M2 は空）
```

- パスは PostgreSQL の `relpath()` と同じ規則にする: 共有は `global/<relfilenode>`、DB ごとは `base/<dboid>/<relfilenode>`、セグメント番号 n≥1 は `.<n>`、フォークは `_fsm` / `_vm` / `_init`（docs: storage-file-layout.html）。テーブル空間（`pg_tblspc/`）は作らない。
- ストレージ層はファイルを **`RelFileLocator { spc_oid, db_oid, rel_number }`** で指す（PG17 の `RelFileLocator` と同じ考え方）。テーブルの OID でファイルを指してはいけない（TRUNCATE などで relfilenode が変わるため）。M1 の `Storage` トレイトは `table_oid` を引数に取っているので、M2 で `RelFileLocator` に変える（あるいは executor 側で oid → locator に変換する）。
- すべてのファイル操作（作成・削除・rename・fsync・ディレクトリの fsync・ロック）は、ファイル I/O の抽象化レイヤ（障害注入用）を通す。

### 4.2 制御ファイル `global/yuzhu_control`

| 項目 | 内容 |
|---|---|
| magic | 固定値（例 `b"YUZHUCTL"`） |
| control_version | 制御ファイル自体の形式の版 |
| catalog_version | catversion（形式を変えるたびに上げる。`YYYYMMDDN` 形式） |
| builtin_hash | `builtin.rs` の表から計算したハッシュ（§5.2） |
| system_identifier | initdb 時の乱数 u64（M3 で WAL ファイルとの照合に使う） |
| block_size / segment_blocks | 8192 / 131072（1GB）。違えば起動を拒否 |
| state | `ShutDowned` / `InProduction`（M2 では「正常に停止したか」の判定に使う） |
| next_oid | OID カウンタの「ここまでは使ってよい」上限（§4.3） |
| next_xid / checkpoint_lsn | M3 で使う。M2 では 0 を書いておく |
| crc32c | 上のすべての CRC |

- 書き込みは「一時ファイルに書く → fsync → rename → ディレクトリを fsync」とする。PostgreSQL は pg_control を上書きしているが（512 バイト以下なのでセクタ単位で原子的になる前提）、yuzhu は relmapper と同じ rename 方式の方が抽象化レイヤで障害注入しやすい。
- `crc32c` は手書き（50 行程度）か、周辺用途のクレートを使う。CLAUDE.md の「暗号」の扱いに近いので、どちらでもよい。

### 4.3 OID と relfilenode の割り当て

- OID カウンタはクラスタ全体で 1 つ（PG と同じ）。共有メモリ相当の `Mutex<OidCounter>` に置く。
- M2 は WAL が無いので、次の方式で永続化する: カウンタが制御ファイルの `next_oid` に達したら、`next_oid += 8192` にして制御ファイルを書き、fsync してから OID を渡す。起動時は `next_oid` から始める（使わなかった番号は捨てる）。クラッシュしても同じ OID を二度渡さない。M3 からは PG と同じく WAL に記録する方式に変えてよい。
- 一周したら 16384 に戻す。重複の確認は、M2 はカタログキャッシュ（全件をメモリに持てる）で、M4 からは OID インデックスで行う。
- 新しいテーブルの relfilenode は、同じカウンタから取った値にする（PG の `GetNewRelFileNumber` と同じ）。そのファイルが既にあれば次の値を取る。新しいテーブルは OID を relfilenode と同じ値にすると分かりやすい（PG も通常はそうなる。**未確認**: `heap_create_with_catalog` の流れは記憶による）。
- **マップされるカタログ**: PostgreSQL と同じく、共有カタログと pg_class/pg_attribute/pg_type/pg_proc は `pg_class.relfilenode = 0` とする。M2 では `pg_filenode.map` を作らず、**「マップ対象の relfilenode は oid と同じ」と決め打ちする関数 `relmap_lookup(oid)`** を用意する。カタログのファイルを作り直す操作（カタログへの VACUUM FULL、TRUNCATE）が必要になったら（M5 以降）、この関数を `pg_filenode.map` を読む実装に置き換える。それ以外のカタログ（pg_namespace など）は `relfilenode = oid` を pg_class に書く。

### 4.4 複数データベースへの備え

- 接続時の流れ: 起動パラメータの `database` を共有カタログ `pg_database` で引く → 無ければ `3D000` → `datallowconn = false`（template0）なら `55000`（`database "template0" is not currently accepting connections`。**未確認**: SQLSTATE は記憶による）→ `base/<dboid>/` のカタログを開く。
- `engine::Database` は「クラスタ」（共有カタログ、OID カウンタ、制御ファイル、バッファプール）と「データベース」（DB ごとのカタログキャッシュ）に分ける。M1 の「Database は 1 つだけ」という制約は M2 で外し、template1 と postgres の両方に接続できるようにする。
- CREATE DATABASE（M5）は、テンプレートの `base/<src>/` を `base/<new>/` にコピーして pg_database に行を入れる（PG の `STRATEGY = FILE_COPY` と同じ。PG15 以降の既定の `WAL_LOG` はブロックごとに WAL を書く方式）。initdb の template0 / postgres の作成も同じ処理で行うので、M2 の段階でこのコピー処理を作っておけば M5 は SQL の受け口を足すだけになる。

---

## 5. initdb

### 5.1 コマンド

`yuzhu-initdb -D <datadir> [-U <superuser>] [--no-sync]`（`yuzhu-server` クレートに 2 つ目のバイナリとして置く。CLI 引数のクレートは使ってよい）。エンコーディングは UTF8、ロケールは C に固定する（オプションを受けても C と UTF8 以外は拒否する）。

### 5.2 手順（PostgreSQL の `initialize_data_directory` に対応させたもの）

1. `datadir` が存在しないか空であることを確かめる。権限 0700 で作る。
2. サブディレクトリを作る: `global`、`base`、`base/1`、`pg_wal`、`pg_xact`。
3. 最上位の `YUZHU_VERSION` を書く（PG は最初に PG_VERSION を書く。確認済み）。
4. **ブートストラップ（template1 を作る）**。SQL は使わず、Rust のコードから直接ヒープに書く。
   - カタログの列定義は Rust の静的な表（`catalog/schema.rs`。genbki.pl が生成する `Schema_pg_class` などに相当）に持つ。サーバの起動時もこの表から記述子を作る（`formrdesc` に相当）ので、カタログを読むためにカタログを読む必要はない。
   - 各カタログのファイルを作る（共有は `global/`、ほかは `base/1/`）。
   - 行を入れる順序: pg_namespace → pg_authid → pg_tablespace → pg_am → pg_type → pg_proc → pg_operator → pg_cast → pg_class（全カタログ分）→ pg_attribute（全カタログの列と、システム列）→ pg_database（template1）。
   - タプルヘッダの `xmin` は `BootstrapTransactionId`（1）、`xmax` は 0、`cmin` は 0。M3 の可視性判定では XID 1 と 2 を常にコミット済みとして扱うので、M3 で何も変えなくてもこれらの行は見える。
   - pg_type / pg_proc / pg_operator / pg_cast の行は `builtin.rs` から作る（§5.3）。
5. バッファをすべて書き出し、各ファイルとディレクトリを fsync する（`--no-sync` のときは省く。テストを速くするため。PG の `--no-sync` と同じ）。
6. `base/1/YUZHU_VERSION` を書く。
7. template0（4）と postgres（5）を作る: `base/1/` を `base/4/`、`base/5/` にコピーし、pg_database に行を入れる（template0 は `datistemplate = true, datallowconn = false`、template1 は `datistemplate = true`、postgres はどちらも false。PG の `make_template0` / `make_postgres` に相当）。
8. OID カウンタを `FirstNormalObjectId`（16384）に設定する。
9. **最後に**制御ファイルを書く（state = `ShutDowned`）。制御ファイルがあることを「initdb が最後まで終わった」印にする。途中で失敗したらディレクトリを消す（PG と同じ。`--no-clean` は不要）。

### 5.3 builtin.rs をカタログの行に変換する

- M1 の `BuiltinType`/`BuiltinCast`/`BuiltinOperator`/`BuiltinFunction` は、**実行時の実体（関数ポインタ）とカタログの行の両方の出所**として残す。
- 変換に足りない情報があるので、M2 で次の項目を足す:
  - `BuiltinOperator.proc_oid`（`oprcode` に入れる実体関数の pg_proc OID。例: `int4pl` 177）、`oprkind`、`com`/`negate`（任意）
  - `BuiltinCast` の `CastMethod::Function` に関数の OID（`castfunc`）
  - `BuiltinFunction.volatility`（`provolatile`。`version()` は 's'、算術は 'i' など）と `prosrc`（Rust 側の実体の名前。PG の internal 言語の関数が `prosrc` に C の関数名を入れるのと同じ）
  - `BuiltinType` に `typalign`、`typstorage`、`typelem`（配列型用）、`typcollation`（text 系は 100、**未確認**）
- pg_proc に入れる関数: ユーザーが呼べる関数（`length` など）、演算子の実体（`int4pl` など）、キャスト関数、型の入出力関数（`int4in` / `int4out` など。`pg_type.typinput` から参照される）、M2 で足す psql 用の関数（`pg_get_userbyid` 1642、`pg_table_is_visible` 2079、`pg_encoding_to_char` 1597、`array_length` 2176、`array_to_string` 395、`textregexeq` 1254、`textregexne` 1256、`pg_get_expr` 1716、`format_type` 1081 など。OID は pg_proc.dat で確認済み）。
- PG に対応するものが無い yuzhu 独自のオブジェクト（あれば）は、10000〜11999 の範囲で yuzhu が固定で振る（PG でもこの範囲は版ごとに変わるので、クライアントが決め打ちしていない）。
- **実行時の名前解決は、M2 でも静的な表を引く**（今の `CatalogReader` の既定実装のまま）。カタログのテーブルは、SQL から見るための写しとして持つ。両者がずれないように、`builtin.rs` の表から計算したハッシュを制御ファイルに入れ、起動時に一致を確かめる（違えば「initdb をやり直してください」で起動を拒否する）。CREATE FUNCTION や CREATE TYPE を入れる段階で、名前解決を「静的な表 + カタログ」に広げる。
- 単体テストで、pg_type・pg_proc・pg_operator・pg_cast に入れる組み込みの行が postgres:17 の同じ OID の行と一致すること（`typname`, `typlen`, `oprname`, `oprleft`, `oprright`, `oprresult`, `castcontext` など）を確かめる slt を `tests/` に置くとよい（PG に対しても通る）。

---

## 6. DDL をカタログの行にする

### 6.1 CREATE TABLE

`CREATE TABLE t (a int4 NOT NULL DEFAULT 1 CHECK (a > 0), b text)` の場合:

1. 名前の重複を確かめる（同じ名前空間の pg_class に同名があれば `42P07`）。
2. OID を割り当てる（relfilenode も同じ値）。
3. ファイル `base/<db>/<relfilenode>` を作る。**アボートしたら消す**ものとして登録する（PG の `smgr` の pending delete と同じ考え方）。
4. pg_class に 1 行（§3.3。`relnatts = 2`、`relchecks = 1`）。
5. pg_attribute に、ユーザー列（attnum 1, 2）と**システム列**（`ctid` -1、`xmin` -2、`cmin` -3、`xmax` -4、`cmax` -5、`tableoid` -6。PG12 以降の構成、**未確認**: 番号は記憶による）を入れる。`attlen`/`attbyval`/`attalign`/`attstorage` は pg_type から写す。`a` は `attnotnull = true`、`atthasdef = true`。`attcollation` は text 系なら 100。
6. pg_attrdef に 1 行（`adrelid = t の oid, adnum = 1, adbin = 式`）。
7. pg_constraint に 1 行（`conname = 't_a_check', contype = 'c', connamespace = 2200, conrelid = t の oid, convalidated = true, conislocal = true, connoinherit = false, conkey = NULL（int2[] は NULL 専用。本来は {1}）, conbin = 式`）。
8. 無効化を登録する（§7）。

- DROP TABLE は逆の順で、pg_constraint（`conrelid` で探す）→ pg_attrdef → pg_attribute → pg_class の行を消し、ファイルは**コミットしたら消す**ものとして登録する。pg_depend が無い M2 では、従属するものを `conrelid`/`adrelid`/`attrelid` で直接探す。
- OID が 16384 未満のもの（システムカタログ）への DROP・INSERT・UPDATE・DELETE は拒否する（PG は `permission denied: "pg_class" is a system catalog`、`42501`。**未確認**: 文言は記憶による）。SELECT は許す。

### 6.2 DEFAULT と CHECK の式の保存（Q-006 の M2 版）

- 列名と型は PG に合わせる: `adbin`/`conbin`、型は `pg_node_tree`（194）。ただし**中身は yuzhu の SQL テキスト**とする（PG の nodeToString 形式にはしない。クライアントは `pg_get_expr` を通して読むので、中身の形式の違いは表に出ない）。
- 選択肢:

| 案 | 中身 | 工数 | 長所 / 短所 |
|---|---|---|---|
| A | 利用者が書いた式の部分文字列（AST の Span で切り出す） | 小（半日） | M1 のまま。`pg_get_expr` は原文を返すので、PG の出力（`(a > 0)`、`CHECK ((a > 0))`）と一致しない。列名の変更（ALTER TABLE RENAME COLUMN）で壊れる |
| B | アナライズ済みの式を逆変換（deparse）した正規形のテキスト | 中（2〜4 日） | `pg_get_expr` が PG と同じ出力を返せる（括弧の付け方とキャストの表記を PG の `ruleutils.c` に合わせる必要あり）。列名の変更時に書き直せば済む |
| C | 束縛済みの木（列は attnum で参照）を独自形式で直列化 | 大（4〜7 日） | PG と同じ考え方。列名の変更に強い。表示には B の逆変換が別に要る |

- **推奨: M2 は A**（M1 と同じでパース・アナライズを使い回せる）。`\d tbl` を動かす M4 で B に移る（catversion を上げる）。列名の変更を入れるのは B の後にする。
- 読み出し: テーブルの定義（`TableDef`）を作るときに、pg_attrdef と pg_constraint（`contype = 'c'`）の行から `ColumnDef.default` と `CheckDef` を作る。パースとアナライズは今と同じく使うたびに行う（M4 で TableDef にキャッシュしてもよい）。

### 6.3 M3 で DDL がトランザクションに乗る理由

- カタログの行は普通のヒープタプル（xmin/xmax/cmin を持つ）として書くので、M3 で MVCC の可視性判定を入れれば、**DDL のロールバックとコミット前の不可視**は自動的に成り立つ（他のトランザクションは、コミット前の pg_class の行が見えない）。
- そのためには、カタログの読み書きを**必ずユーザーテーブルと同じヒープ API（スナップショット付きの走査、`heap_insert`/`heap_delete`/`heap_update`）経由**にすること。カタログ専用の「直接書き換える」経路を作ってはいけない。M2 の時点で、カタログ走査の関数にスナップショットの引数（M2 は「すべて見える」のダミー）を付けておく。
- 例外はファイル操作とキャッシュ: ファイルの作成・削除はアボート・コミット時の後処理（pending delete）で、キャッシュは §7 の無効化で、トランザクションに合わせる。
- 同じトランザクションの中で CREATE TABLE の直後に INSERT できるように、文の区切りでコマンド ID を進め、自分の書いたカタログ行が次の文から見えるようにする（PG の `CommandCounterIncrement`）。

---

## 7. カタログキャッシュと無効化

### 7.1 構成

PostgreSQL はバックエンド（プロセス）ごとにキャッシュを持ち、SI キューで知らせ合います。yuzhu は 1 プロセス・接続ごとに 1 スレッドなので、**データベースごとの共有キャッシュ**にします。

```rust
/// データベースごとに 1 つ。全セッションで共有する。コミット済みの状態だけを持つ。
pub struct CatalogCache {
    inner: RwLock<CacheInner>,
    generation: AtomicU64,          // 無効化のたびに増やす
}
struct CacheInner {
    rel_by_oid: HashMap<Oid, Arc<TableDef>>,
    rel_by_name: HashMap<(Oid /*nsp*/, String), Option<Oid>>, // None = 無いことのキャッシュ
    namespaces: HashMap<String, Oid>,
}

/// セッションごと。自分のトランザクションが変更したオブジェクトを覚える。
pub struct SessionCatalogState {
    pending_inval: Vec<Inval>,      // コミットしたら共有キャッシュに流す
    touched: HashSet<Oid>,          // このトランザクションで変更した relation
    seen_generation: u64,           // 文の開始時に共有キャッシュの generation と比べる
}

pub enum Inval { Relation { rel_oid: Oid }, RelationName { nsp: Oid, name: String }, Namespace, All }
```

- `TableDef` は M1 と同じく不変で `Arc` で共有する。キャッシュが外れたら、pg_class → pg_attribute → pg_attrdef → pg_constraint を走査して作り、`Arc` を入れる。カタログ自身の `TableDef` は `schema.rs` の静的な表から作り、キャッシュから消さない（nailed）。
- 型・関数・演算子・キャストは M2 では静的な表を引くので、キャッシュは要らない（§5.3）。

### 7.2 M2 の動き（単純版）

- M2 は MVCC が無いので、DDL はデータベースごとの DDL ロック（`Mutex`）を取って直列に実行し、文が終わったら（M1 の undo でロールバックした場合も含めて）**すぐに**共有キャッシュから該当エントリを消し、`generation` を増やす。
- 実行中の文がテーブルを使っている間に DROP されないよう、文の実行中はテーブルの読み取りロックを持つ。M2 では「DDL はデータベース単位の書き込みロック、それ以外の文は読み取りロック」でよい（M3 の「単一ライター + 複数リーダー」の方針と矛盾しない）。

### 7.3 M3 以降の動き（PostgreSQL の inval.c と同じ考え方）

1. DDL がカタログの行を挿入・削除・更新したら、`pending_inval` に記録し、`touched` に入れる。共有キャッシュにはまだ触らない。
2. 自分のセッションでは、`touched` に入っている relation を**共有キャッシュから取らず**、自分のスナップショットでカタログを読んで作る（コミット前の自分の変更を見るため）。文の区切りで、このセッション用の一時的な `TableDef` を捨てる。
3. コミットしたら、**コミットログに記録した後で** `pending_inval` を共有キャッシュに適用し（エントリを消して `generation` を増やす）、`touched` を空にする。順序を逆にすると、他のセッションがコミット前の古い行でキャッシュを作り直してしまう。
4. アボートしたら、`pending_inval` と `touched` を捨てるだけ（共有キャッシュにはコミット済みの状態しか入っていないので、何もしなくてよい）。
5. 共有キャッシュにエントリを入れるときは、「作り始めたときの `generation`」と「入れるときの `generation`」が同じ場合だけ入れる（作っている最中に無効化が来た場合に、古い定義を入れてしまうのを防ぐ。PG の relcache が作り直しを行うのと同じ問題）。
6. 他のセッションは、文の開始時（テーブルのロックを取った後）に `generation` を見て、変わっていればプランのキャッシュ（M5 の Extended Query の準備済み文）を捨てる。PG の「ロックを取るたびに SI キューを読む」に相当する。
7. カタログはトランザクションのスナップショットではなく、文ごとに取り直す最新のスナップショットで読む（PG と同じ。Repeatable Read（M5）でも、テーブル定義は最新を見る）。

---

## 8. 名前空間と search_path

- `pg_catalog` は search_path に書かれていなくても**先頭で暗黙に探す**（PG と同じ）。だから `SELECT * FROM pg_class` はカタログを指し、`CREATE TABLE pg_class (...)` は public に作られる（ただし普通の参照では pg_catalog が先に見つかる）。
- M2 の search_path は `"$user", public` 固定でよい（`SET search_path` を受けるなら settings に置く）。`"$user"` という名前の名前空間は作らないので、実質は public だけ。
- `pg_table_is_visible(oid)` は「その relation の名前空間が search_path（暗黙の pg_catalog を含む）にあり、それより前の名前空間に同じ名前の relation が無い」ときに true を返す。
- `current_schema()` は search_path の中で最初に存在する名前空間（M1 の実装をカタログ参照に変える）。

---

## 9. M2 の作業の分け方と工数の目安

| 作業 | 内容 | 工数 |
|---|---|---|
| カタログの静的な定義 | `catalog/schema.rs`（全カタログの列定義、OID 定数）、追加の型（`"char"`, `xid`, `oidvector`, `regproc`, `pg_node_tree` と NULL 専用型） | 2〜3 日 |
| builtin.rs の拡張と行への変換 | proc_oid などの追加、pg_type/pg_proc/pg_operator/pg_cast の行の生成、PG との突き合わせテスト | 2〜3 日 |
| 制御ファイルとディレクトリ | `yuzhu_control`、`YUZHU_VERSION`、ロックファイル、パスの規則、OID カウンタ | 1〜2 日 |
| initdb | `yuzhu-initdb`、ブートストラップ、template0/postgres のコピー | 2 日 |
| カタログ経由の DDL | CREATE/DROP TABLE を行の挿入・削除にする、TableDef の組み立て、キャッシュと無効化（M2 版） | 3〜4 日 |
| 接続とデータベース | pg_database による接続先の解決、クラスタとデータベースの分離 | 1 日 |
| psql 対応 | `pg_get_userbyid` などの関数、`E'...'`、`OPERATOR(...)`、`COLLATE`、正規表現の最小実装、LEFT JOIN の前倒し | 4〜6 日（LEFT JOIN を含む） |

（ヒープ、ページ、バッファプールは別の調査の範囲。カタログはそれらの上に乗る。）

---

## 10. QUESTIONS.md に記録すべき判断

1. **Q-004 の改訂**: M2 で server_version を 17.0 に上げ、カタログを PG17 の列構成にする（§2.3）。
2. ユーザーテーブルの行型（pg_type の composite 型と `reltype`）を M2 では作らず `reltype = 0` にする。PG は全テーブルに行型を作るので差分になる。行型の値や `pg_type` からテーブル名を引くクライアントが必要になったら作る（§3.3）。
3. public の所有者を `pg_database_owner` ではなく 10 にする（§3.2）。
4. マップされるカタログの relfilenode を M2 では oid と同じに決め打ちし、`pg_filenode.map` は M5 以降（§4.3）。
5. 実行時の型・関数・演算子・キャストの名前解決は静的な表のまま。カタログは写しで、ずれはハッシュで検出する（§5.3）。
6. `adbin`/`conbin` の中身は SQL の原文（案 A）。M4 で deparse した正規形（案 B）に移る（§6.2）。
7. 正規表現は手書きの最小実装（§2.4）。
8. `\dt` のために LEFT JOIN を M4 から M2 に前倒しするか、`\dt` を M4 まで待つか（推奨は前倒し。Nested Loop だけなら 1〜2 日）。
