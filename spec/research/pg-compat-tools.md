# yuzhu 調査レポート（M5/M6）: 周辺ツール互換性 — psql メタコマンド、pgbench、pg_dump、ORM、GUI ツール

- 対象: psql 17 のメタコマンド（`\d` `\dt` `\di` `\l` `\dn` `\df` `\du`）、pgbench 17（初期化と組み込みスクリプト）、pg_dump 17（とその出力のリストア）、SQLAlchemy 2（psycopg 3 / psycopg2）、Prisma 6（`db pull`）、Rails ActiveRecord（main）、DBeaver（pgJDBC）、pgAdmin 4
- 前提: `CLAUDE.md`（マイルストーン）、`spec/design/m1.md`、`spec/research/m2-catalog.md`（psql の `\dt` `\l` と LEFT JOIN の前倒し）、`spec/research/m3-tx-semantics.md`（グローバル書き込みロック、セーブポイントは M3 の範囲外）、`spec/research/research-pg-protocol.md`（ドライバの起動処理）
- 表記: **[実機]** は PostgreSQL 17.11（`postgres:17` コンテナ、`log_statement=all`）でサーバログを取って確認したもの。**[ソース]** は REL_17_STABLE などのソースを読んで確認したもの。**[未検証]** は記憶や推測によるもので、実装前に確認が必要なもの。
- 工数: **S** = 1 日以内、**M** = 2〜5 日、**L** = 1 週間以上（1 人、テストを含む）。

---

## 0. 出典と確認方法

### 0.1 ソース（REL_17_STABLE ほか）

| 対象 | URL |
|---|---|
| pgbench | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/pgbench/pgbench.c>（組み込みスクリプト L780-816、`executeStatement` L1500、`tryExecuteStatement` L1516、スクリプト中の COPY 禁止 L3340、COPY による初期データ投入 L5018-5110、`GetTableInfo` L5381-5480、`internal_script_used` の判定 L7290、実行前の VACUUM L7337-7348） |
| psql メタコマンド | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/psql/describe.c>（`describeOneTableDetails` は各問い合わせが失敗すると `goto error_return` で表示全体を中止する。同ファイル中に 61 か所） |
| psql の `\copy` と COPY のデータ送信 | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/psql/copy.c>（`handleCopyIn` L510-700。`COPYBUFSIZ` 8192、`\.` 行の扱い） |
| pg_dump | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/pg_dump/pg_dump.c>（`PREPARE dumpFunc(pg_catalog.oid)` など SQL レベルの PREPARE/EXECUTE が 12 か所） |
| pg_dump の接続処理 | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/pg_dump/pg_backup_db.c> |
| プロトコル（COPY の流れ） | <https://www.postgresql.org/docs/17/protocol-flow.html#PROTOCOL-COPY> |
| プロトコル（メッセージ形式） | <https://www.postgresql.org/docs/17/protocol-message-formats.html> |
| COPY 文 | <https://www.postgresql.org/docs/17/sql-copy.html> |
| pgbench の文書 | <https://www.postgresql.org/docs/17/pgbench.html> |
| Rails | <https://github.com/rails/rails/blob/main/activerecord/lib/active_record/connection_adapters/postgresql_adapter.rb>（`configure_connection` L1164-1263、型の読み込み L1060-1080、PG 10 未満の拒否 L772） |
| pgAdmin 4 | <https://github.com/pgadmin-org/pgadmin4/blob/master/web/pgadmin/utils/driver/psycopg3/connection.py>（接続直後の処理 L530-660） |
| DBeaver | <https://github.com/dbeaver/dbeaver/tree/devel/plugins/org.jkiss.dbeaver.ext.postgresql/src/org/jkiss/dbeaver/ext/postgresql/model>（`PostgreDataSource.java` L192/L220/L443、`PostgreExecutionContext.java` L179/L187、`PostgreDatabase.java`） |

### 0.2 実機で取ったもの [実機]

一時コンテナ（`yuzhu-compat-research`、確認後に削除）で `log_statement=all` を有効にし、次を実行してサーバ側で受けた文をすべて記録した。

1. `pgbench -i -s 1`（既定の初期化手順 `dtgvp`）と `pgbench -i -I dtGvp`（サーバ側でデータを生成）
2. `pgbench -t 1 -c 1` を `-M simple`（既定）、`-M extended`、`-M prepared` の 3 通り
3. `psql -E` で `\l` `\dn` `\dt` `\di` `\df` `\d` `\d t1` `\du` `\dt+`（`t1` は SERIAL の主キー、UNIQUE、CHECK、DEFAULT、追加のインデックスを持つ表）
4. `pg_dump -t t1 -t pgbench_branches`
5. SQLAlchemy 2 + psycopg 3 / psycopg2 での接続、`inspect()` の各メソッド、`MetaData.reflect()`
6. `npx prisma@6 db pull`

---

## 1. 結論（要約）

1. **pgbench は M4 の終わりに動かせる**。必要なのは COPY FROM STDIN（テキスト形式）、`CREATE TABLE ... WITH (fillfactor=100)`、TRUNCATE、VACUUM（何もしない実装でよい）、`ALTER TABLE ... ADD PRIMARY KEY`、`count(*)`、`char(n)`、`timestamp` と `CURRENT_TIMESTAMP`。組み込みスクリプトの `\set` と `:var` は**クライアント側で展開される**ので、既定の `-M simple` なら Simple Query だけで動く [実機]。`-M extended` と `-M prepared` は Extended Query（M5）が必要。
2. **M3 の性能測定にも pgbench を使える**。`pgbench -n -f custom.sql` とすると、`GetTableInfo`（`count(*)` とパーティションの確認）も実行前の VACUUM も走らない [ソース L7290, L7337]。初期化は psql で SQL を流せばよい。
3. **COPY プロトコルは M4 に前倒しすることを推奨する**（FROM STDIN、テキスト形式のみ。工数 M）。pgbench、pg_dump の出力のリストア、psql の `\copy` が使う。COPY TO STDOUT とCSV 形式は M5、バイナリ形式は M6。
4. **psql の `\d tbl` は 12 本の問い合わせを順に送り、1 本でも失敗すると何も表示しない** [ソース]。pg_publication、pg_policy、pg_statistic_ext、pg_inherits などの**中身が空のカタログ表**を正しい列構成で用意し、UNION、スカラーサブクエリ、`ARRAY(SELECT ...)`、`= ANY(...)`、`::regclass`、`pg_get_indexdef` などの**逆パース関数**を揃える必要がある。目標は M4（JOIN とサブクエリが入る時点）。
5. **逆パース（PostgreSQL の ruleutils.c に当たるもの）が最大の山**。`pg_get_expr`、`pg_get_constraintdef`、`pg_get_indexdef`、`format_type` が psql、pg_dump、SQLAlchemy、Prisma のすべてで使われる。出力の文字列（例: `'x'::character varying`、`CHECK ((n > 0))`）を PostgreSQL と一致させる必要がある（工数 L）。
6. **ORM と GUI ツールはほぼすべて Extended Query を使う**。psycopg 3 はパラメータ付きの問い合わせで Extended Query を使い（SQLAlchemy の inspect は全部これ）、Prisma は名前付きの準備済み文（`s0`, `s1`, ...）、pgJDBC（DBeaver）は既定で Extended Query を使う [実機/ソース]。psycopg2 は Simple Query だけで済む [実機]。
7. **SQLAlchemy + psycopg 3 は接続直後に SAVEPOINT を送る**（hstore 型の検出、[実機]）。M3 の調査では SAVEPOINT を範囲外にしているため、M5 で入れないと接続そのものが失敗する可能性が高い（失敗時の挙動は **[未検証]**）。
8. **pg_dump は M6**。`SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY`、`LOCK TABLE`、約 50 個のカタログ表と `pg_roles`・`pg_settings` などのビュー、`unnest('{...}'::oid[])`、`WITH RECURSIVE`、`acldefault`、COPY TO STDOUT を使う [実機]。出力のリストア（`psql -f`）は M4〜M5 で一部動かせる。
9. **未知の GUC への SET を拒否しないこと**が、pg_dump の出力、pgAdmin、Rails を動かす前提になる。PostgreSQL に存在する GUC は「受け付けて保存するだけ」の項目として一覧に持つ（工数 S）。
10. **互換性テストは sqllogictest では書けない**（COPY のデータ、メタコマンドの出力）。`tests/compat/` に psql・pgbench・Python のスクリプトを置き、PostgreSQL と yuzhu の両方で実行して出力を比べる仕組みを M4 で作ることを推奨する（工数 M）。

---

## 2. ツールをまたいで必要になる基盤

各ツールの節（第 3 節）はここの記号（C1、P2 など）を参照する。

### 2.1 プロトコル

| 記号 | 項目 | 使うツール | 推奨時期 | 工数 |
|---|---|---|---|---|
| P1 | COPY FROM STDIN（CopyInResponse `G`、CopyData `d`、CopyDone `c`、CopyFail `f`）、テキスト形式 | pgbench `-i`、pg_dump の出力のリストア、psql `\copy ... from` | **M4（前倒し）** | M |
| P2 | COPY TO STDOUT（CopyOutResponse `H`、CopyData、CopyDone） | pg_dump、psql `\copy ... to` | M5 | S〜M |
| P3 | Extended Query（Parse/Bind/Describe/Execute/Sync/Close/Flush、名前なし・名前付きの文とポータル、ParameterDescription、パラメータの型推論） | psycopg 3、Prisma、pgJDBC/DBeaver、pgbench `-M extended/prepared`、Rails（pg gem の `exec_params`/`prepare`、**[未検証]**） | M5 | L |
| P4 | バイナリ形式の結果とパラメータ | Prisma（tokio-postgres 由来の可能性、**[未検証]**）、pgJDBC（一部の型、**[未検証]**） | M5 の後半〜M6 | M |
| P5 | COPY の CSV 形式、`HEADER`、`COPY (query) TO` | psql `\copy`、ETL ツール | M5 | S〜M |
| P6 | COPY のバイナリ形式 | pg_dump では使わない。一部のドライバ（psycopg 3 の `copy` の binary、**[未検証]**） | M6 | M |

### 2.2 SQL の構文と意味

| 記号 | 項目 | 使うツール | 推奨時期 | 工数 |
|---|---|---|---|---|
| Q1 | LEFT JOIN、カンマ区切りの FROM（暗黙の内部結合） | psql の全メタコマンド、すべての ORM | M2 で前倒し（`m2-catalog.md` §2.1）、M4 で完成 | M |
| Q2 | スカラーサブクエリ、`EXISTS`、`IN (SELECT ...)` | psql `\d tbl`、SQLAlchemy、Prisma | M4 | M |
| Q3 | UNION / UNION ALL | psql `\d tbl`（出版物の問い合わせ）、Rails（型の読み込み）、pg_dump | M4 | S |
| Q4 | `CROSS JOIN LATERAL (SELECT ...)` | pgbench の `GetTableInfo`（失敗しても続行するので必須ではない） | M6 | M |
| Q5 | `WITH` / `WITH RECURSIVE` | pg_dump、pgAdmin（ロールの所属）、Prisma（`WITH rawindex AS`） | WITH は M5、RECURSIVE は M6 | M |
| Q6 | 配列: 配列リテラル `'{0}'`、`ARRAY[...]`、`ARRAY(SELECT ...)`、`= ANY(array)`、添字 `a[s]`、`'{...}'::oid[]` | psql `\d tbl`、SQLAlchemy、pg_dump、pgAdmin | M5（型の追加と一緒に） | L |
| Q7 | 集合返却関数: FROM 句の `generate_series`/`unnest`（`AS t(col)` の列別名つき）、**SELECT 句の** `unnest(indkey)`/`generate_subscripts`（ProjectSet） | pgbench `-I G`、SQLAlchemy（主キーと UNIQUE の取得）、Prisma、pg_dump | FROM 句は M4、SELECT 句は M5 | M |
| Q8 | 正規表現 `~` `!~`、`OPERATOR(pg_catalog.~)`、`COLLATE pg_catalog.default` | psql | M2（`m2-catalog.md` の手書きの最小実装） | S〜M |
| Q9 | 集約 `count`、`array_agg(... ORDER BY ...)`、`string_agg`、`bool_and`、`min` | pgbench（`count(*)`）、SQLAlchemy、psql | count は M4、順序つき集約は M5 | M |
| Q10 | `TRUNCATE t1, t2, ...`（トランザクションの中で実行できること） | pgbench `-i` | M3（トランザクショナル DDL と一緒に）〜M4 | S〜M |
| Q11 | `CREATE TABLE ... WITH (fillfactor=N)`、`WITH (fillfactor='100')`（値が文字列の形） | pgbench、pg_dump の出力 | M4 | S |
| Q12 | `ALTER TABLE [ONLY] t ADD [CONSTRAINT n] PRIMARY KEY / UNIQUE (...)`、`ALTER COLUMN SET DEFAULT`、`OWNER TO` | pgbench、pg_dump の出力 | M4（OWNER TO は受け付けるだけ） | M |
| Q13 | `VACUUM [ANALYZE] t`、`ANALYZE t` | pgbench | M3〜M4 は「何もせず成功」、M5 で実装 | S |
| Q14 | `LOCK TABLE ... IN ACCESS SHARE MODE` | pg_dump | M5（M3 の単一ライターでは受け付けるだけでよい） | S |
| Q15 | `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY` | pg_dump | M5 | （RR の実装に含む） |
| Q16 | `SAVEPOINT` / `RELEASE` / `ROLLBACK TO` | SQLAlchemy + psycopg 3（接続時）、psql の `ON_ERROR_ROLLBACK`、Rails の入れ子トランザクション | **M5（マイルストーンに無いので追加を提案）** | M |
| Q17 | SQL レベルの `PREPARE name(type) AS ...` / `EXECUTE name(...)` | pg_dump（関数・型などを出力するとき） | M5（Extended Query と実装を共有） | S |
| Q18 | `CREATE SEQUENCE ... AS integer START WITH ... NO MINVALUE ... CACHE 1`、`ALTER SEQUENCE ... OWNED BY`、`setval` | pg_dump の出力 | M4（SERIAL と一緒に） | S〜M |

### 2.3 カタログ

`m2-catalog.md` で作る pg_class・pg_attribute・pg_type・pg_namespace・pg_proc・pg_database・pg_authid などに加えて、次が必要になる。

| 記号 | 項目 | 使うツール | 推奨時期 | 工数 |
|---|---|---|---|---|
| C1 | pg_am、pg_index、pg_constraint、pg_attrdef、pg_sequence、pg_depend | psql `\di`/`\d tbl`、全 ORM、pg_dump | M4（インデックスと制約の実装で中身が入る） | M |
| C2 | **中身が空のカタログ表**: pg_inherits、pg_partitioned_table、pg_policy、pg_statistic_ext、pg_publication、pg_publication_rel、pg_publication_namespace、pg_trigger、pg_rewrite、pg_extension、pg_description、pg_shdescription、pg_init_privs、pg_event_trigger、pg_foreign_data_wrapper、pg_foreign_server、pg_foreign_table、pg_ts_parser/template/dict/config、pg_transform、pg_default_acl、pg_subscription、pg_enum、pg_range、pg_collation（`default` だけ）、pg_language（`internal`/`sql`）、pg_opclass/pg_opfamily/pg_operator/pg_cast（builtin の内容） | psql `\d tbl`、pg_dump、Prisma、DBeaver | **M4**（列定義を PG17 と同じにした空の表を initdb で作る） | M |
| C3 | ビュー: pg_roles、pg_settings、pg_views、pg_seclabels、pg_stat_gssapi | psql `\du`、pg_dump、pgAdmin、DBeaver、Prisma | M2〜M4 は仮想表、ビューを実装したら置き換え（M5） | M |
| C4 | information_schema（少なくとも `columns`、`sequences`、`tables`） | Prisma、一部の GUI | M6 | L |
| C5 | pg_auth_members | pgAdmin | M6（GRANT/REVOKE と一緒に） | S |

**方針の提案**: C2 は「PG17 の列構成どおりの空の表」をまとめて作るのが最も安上がりである。M2 でカタログを表として持つ仕組みができていれば、1 表あたり列定義を書くだけで済む。`relhasrules` などのフラグは常に false でよい。

### 2.4 関数

| 記号 | 項目 | 使うツール | 推奨時期 | 工数 |
|---|---|---|---|---|
| F1 | `pg_get_userbyid`、`pg_table_is_visible`、`pg_function_is_visible`、`pg_encoding_to_char`、`array_length`、`array_to_string`、`obj_description`、`col_description` | psql、Prisma | M2〜M4 | S |
| F2 | **逆パース**: `pg_get_expr(pg_node_tree, oid[, bool])`、`pg_get_constraintdef(oid[, bool])`、`pg_get_indexdef(oid[, int, bool])`、`format_type(oid, int)`、`pg_get_function_result`/`pg_get_function_arguments`、`pg_get_viewdef`、`pg_get_triggerdef`、`pg_get_functiondef` | psql `\d tbl`/`\df`、pg_dump、SQLAlchemy、Prisma | format_type・pg_get_expr・pg_get_constraintdef・pg_get_indexdef は M4、残りは M5〜M6 | **L** |
| F3 | OID 別名型へのキャスト: `::regclass`、`::regtype`、`::regproc`、`::regprocedure`、`::regnamespace`、およびその逆（`regclass::text`）、`to_regtype` | psql、pg_dump、SQLAlchemy、Rails | regclass と regtype は M4、残りは M5 | M |
| F4 | `current_schemas(bool)`、`array_position`、`pg_get_serial_sequence`、`pg_relation_is_publishable`、`pg_table_size`、`pg_size_pretty`、`pg_database_size` | pgbench、psql `\dt+`、SQLAlchemy、DBeaver | M4〜M5 | S |
| F5 | `set_config`、`current_setting`、`pg_show_all_settings()`、`pg_is_in_recovery()`、`pg_backend_pid()`、`version()`、`session_user` | pg_dump、pgAdmin、DBeaver、Prisma | M2〜M4 | S |
| F6 | `quote_ident`、`quote_literal`、`json_build_object`、`acldefault`、`has_database_privilege`、`pg_options_to_table` | pg_dump、SQLAlchemy、pgAdmin | M5〜M6 | M |

### 2.5 型

| 記号 | 項目 | 使うツール | 推奨時期 | 工数 |
|---|---|---|---|---|
| T1 | `char(n)`（bpchar、OID 1042。空白の詰め物と比較時の末尾空白の無視） | **pgbench**、pg_dump の出力 | **M4（M5 から前倒し）** | S〜M |
| T2 | `timestamp`（1114）、`timestamptz`（1184）、`CURRENT_TIMESTAMP`、timestamptz → timestamp の代入キャスト | **pgbench**（`pgbench_history.mtime`） | **M4（M5 から前倒し）** | M |
| T3 | カタログ用の型: `name`（19）、`"char"`（18）、`oid`（26）、`int2vector`（22）、`oidvector`（30）、`pg_node_tree`（194）、`aclitem`（1033） | psql、ORM、pg_dump | name・"char"・oid は M2、残りは M4 | M |
| T4 | 配列型（`text[]`、`oid[]`、`int2[]`、`aclitem[]`、`anyarray` の出力） | psql、pg_dump、SQLAlchemy | M5 | L（Q6 と同じ作業） |
| T5 | `json`（114）、`jsonb` | SQLAlchemy（`json_build_object`） | M6 | M〜L |

### 2.6 GUC（SET で受け付ける設定項目）

ツールの多くは、接続直後やダンプの先頭で yuzhu が実装していない GUC を SET する。**PostgreSQL では未知の名前の SET は `42704`（`unrecognized configuration parameter`）で失敗し、pg_dump や pgAdmin はそこで中止する** [ソース: pg_backup_db.c、pgAdmin connection.py L535-549]。

推奨: `settings.rs` に「PG17 に存在するが yuzhu では意味を持たない項目」を一覧で持ち、値の検査だけして保存する（工数 S）。少なくとも次を入れる。

- pg_dump の接続時 [実機]: `DATESTYLE`、`INTERVALSTYLE`、`extra_float_digits`、`synchronize_seqscans`、`statement_timeout`、`lock_timeout`、`idle_in_transaction_session_timeout`、`transaction_timeout`、`row_security`、`restrict_nonsystem_relation_kind`（`pg_settings` を SELECT して set_config で設定する）
- pg_dump の出力の先頭 [実機]: `client_encoding`、`standard_conforming_strings`、`check_function_bodies`、`xmloption`、`client_min_messages`、`default_tablespace`、`default_table_access_method`、`search_path`（`set_config('search_path', '', false)` で**空**にするので、出力中の名前はすべてスキーマ修飾される）
- pgAdmin [ソース]: `DateStyle`、`client_min_messages`、`bytea_output`、`client_encoding`
- Rails [ソース]: `standard_conforming_strings`、`intervalstyle`（`iso_8601`）、`client_min_messages`、`timezone`
- pgJDBC [ソース、`research-pg-protocol.md` §4.3]: `extra_float_digits`、`application_name`
- Prisma [実機]: `current_setting('server_version_num')::integer` を読むので、`server_version_num` が `server_version` と矛盾しないこと

---

## 3. ツール別の要件

### 3.1 psql のメタコマンド [実機 + ソース]

psql は接続時に SQL を送らない（ParameterStatus を読むだけ）[実機]。メタコマンドの SQL は `server_version` によって変わる。以下は `server_version=17.x` のときのもの（`m2-catalog.md` §2.3 の推奨どおり 17 系を名乗る前提）。

| コマンド | 送る問い合わせ | 必要なもの | 時期 |
|---|---|---|---|
| `\l` | 1 本（pg_database） | F1、`CASE`、`E'\n'`、`datacl` を NULL にすれば配列関数は strict で済む（`m2-catalog.md` §2.2） | M2 |
| `\dn` | 1 本: `SELECT n.nspname, pg_get_userbyid(n.nspowner) FROM pg_namespace n WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema' ORDER BY 1` | F1、Q8。PG17 では `public` の所有者が `pg_database_owner` になる点に注意（出力を一致させるなら、そのロールを pg_authid に入れる） | M2 |
| `\dt` `\d` | 1 本: pg_class LEFT JOIN pg_namespace LEFT JOIN **pg_am** | Q1、Q8、F1、`c.relkind IN ('r','p','')` | M2（pg_am は 1 行 `heap` だけでよい） |
| `\dt+` | 上記 + `relpersistence`、`am.amname`、`pg_size_pretty(pg_table_size(c.oid))`、`obj_description(c.oid, 'pg_class')` | F1、F4、C2（pg_description） | M2〜M4 |
| `\di` | 上記 + LEFT JOIN pg_index + LEFT JOIN pg_class c2 | C1、Q1 | M4 |
| `\df` | 1 本: pg_proc LEFT JOIN pg_namespace、`pg_get_function_result`、`pg_get_function_arguments`、`pg_function_is_visible`、`CASE p.prokind` | F2 の関数署名部分のみ。ユーザー定義関数が無い間は、pg_catalog を除外するので結果は 0 行でよい | M4（関数定義は M6 以降の想定） |
| `\du` | 1 本: `pg_roles` の `rolname`, `rolsuper`, ..., `rolbypassrls` | C3 | M2（仮想表） |
| `\d tbl` | **12 本**（下記） | Q1〜Q3、Q6、C1、C2、F2、F3 | M4（配列が要る 2 本を除けば）。完全な一致は M5 |

`\d tbl` の 12 本 [実機]:

1. 名前の解決: `c.relname OPERATOR(pg_catalog.~) '^(t1)$' COLLATE pg_catalog.default AND pg_table_is_visible(c.oid)`
2. 表の属性: `relchecks, relkind, relhasindex, relhasrules, relhastriggers, relrowsecurity, relforcerowsecurity, false AS relhasoids, relispartition, '', reltablespace, CASE WHEN reloftype = 0 THEN '' ELSE reloftype::regtype::text END, relpersistence, relreplident, am.amname`、`WHERE c.oid = '16516'`（**文字列リテラルを oid と比較する**ので unknown → oid の暗黙変換が必要）
3. 列: `format_type`、スカラーサブクエリで `pg_get_expr(d.adbin, d.adrelid, true)`、pg_collation と pg_type を参照するサブクエリ、`attidentity`、`attgenerated`
4. インデックス: `pg_get_indexdef(i.indexrelid, 0, true)`、`pg_get_constraintdef(con.oid, true)`、`contype IN ('p','u','x')` を条件にした LEFT JOIN、`ORDER BY i.indisprimary DESC, c2.relname`
5. CHECK 制約: `pg_get_constraintdef(r.oid, true)`
6. 行レベルセキュリティ: `pol.polroles = '{0}'`（**配列リテラルとの比較**）、`array_to_string(array(select rolname from pg_roles where oid = any (pol.polroles) order by 1), ',')`
7. 拡張統計: `stxrelid::regclass`、`stxnamespace::regnamespace::text`、`'d' = any(stxkind)`
8. 出版物: 3 本の UNION、`pg_relation_is_publishable`、`string_agg`、`generate_series(0, array_upper(pr.prattrs::int2[], 1))`、`prattrs[s]`
9. 継承の親: `c.oid::regclass`、`ORDER BY inhseqno`
10. 継承の子/パーティション: `pg_get_expr(c.relpartbound, c.oid)`、`ORDER BY <bool 式>, c.oid::regclass::text`
11〜12. 外部キー（参照する側・される側）と トリガ（表にフラグがあるときだけ。**[未検証]**: 今回の `t1` ではフラグが立たず送られなかった）

**要点**: 6 と 8 は中身が空でも**解析を通る**必要がある。PostgreSQL は、テーブルが空でも式の型検査をするので、yuzhu でも配列型の列と演算子が解析できないと失敗する。抜け道として「サーバ側で問い合わせの形を認識して空の結果を返す」方法もあるが、PostgreSQL と違う動きになり、psql のバージョンが変わると壊れるので**推奨しない**。

出力の文字列も一致させる必要がある（例: `nextval('t1_id_seq'::regclass)`、`'x'::character varying`、`"t1_pkey" PRIMARY KEY, btree (id)`、`CHECK (n > 0)`）。互換性テスト（第 6 節）でこの表示を PostgreSQL と比べる。

### 3.2 pgbench

#### 3.2.1 初期化（`pgbench -i`、既定の手順 `dtgvp`）[実機]

| 手順 | 送る文 | 必要なもの | 失敗したとき | 時期 |
|---|---|---|---|---|
| d | `drop table if exists pgbench_accounts, pgbench_branches, pgbench_history, pgbench_tellers` | M1 で対応済み（NOTICE 4 件） | 中止 | M1 |
| t | `create table pgbench_history(tid int,bid int,aid int,delta int,mtime timestamp,filler char(22))` | **T1、T2** | 中止 | M4 |
| t | `create table pgbench_tellers(... filler char(84)) with (fillfactor=100)`（accounts、branches も同様） | **Q11**、T1 | 中止 | M4 |
| g | `begin` → `truncate table pgbench_accounts, pgbench_branches, pgbench_history, pgbench_tellers` → `copy pgbench_branches from stdin with (freeze on)` ×3 → `commit` | **Q10、P1**、`FREEZE` オプション | 中止（`unexpected copy in result`） | M4 |
| G（`-I` で指定したとき） | `insert into pgbench_accounts(aid,bid,abalance,filler) select aid, (aid - 1) / 100000 + 1, 0, '' from generate_series(1, 100000) as aid` など | Q7（FROM 句の generate_series、`AS aid` で関数名の別名が列名になる） | 中止 | M4 |
| v | `vacuum analyze pgbench_branches` ×4 | **Q13** | **中止**（`executeStatement` は失敗で `exit(1)`、L1500） | M4（何もしない実装） |
| p | `alter table pgbench_branches add primary key (bid)` ×3 | **Q12**、B+Tree | 中止 | M4 |
| f（任意） | `alter table pgbench_tellers add constraint ... foreign key (bid) references pgbench_branches` など | FOREIGN KEY | 中止 | M5 |

- COPY のデータ [ソース L4964-4990]: branches は `1\t0\t\N\n`、tellers は `1\t1\t0\t\N\n`、accounts は `1\t1\t0\t\n`（filler は**空文字列**なので char(84) の空白で埋まる）。最後に `\.\n` を CopyData として送り、`PQendcopy` で CopyDone を送る [ソース L5107-5110]。
- `with (freeze on)` は `server_version >= 14` のときだけ付ける [ソース L5018]。PostgreSQL では「同じ（サブ）トランザクションで作成または TRUNCATE された表」でないと FREEZE はエラーになる（`55000`、**[未検証]**: エラーコード）。pgbench は直前に TRUNCATE しているので条件を満たす。
- 初期化の手順を変えれば、VACUUM や主キーを省ける（`pgbench -i -I dtg`）。M4 の途中でも COPY の確認ができる。

#### 3.2.2 実行（組み込みスクリプト `tpcb-like`）[実機 + ソース]

実行前に、組み込みスクリプトを使うときだけ次を送る [ソース L7290, L7337-7348]。

1. `select count(*) from pgbench_branches` — **失敗すると中止**。結果をスケールとして使う（Q9）。
2. パーティションの確認（`CROSS JOIN LATERAL`、`current_schemas(true)`、`array_position`、pg_partitioned_table、pg_inherits、`GROUP BY 1, 2`）— **失敗しても「パーティションなし」とみなして続行する**（ソースのコメント: "We assume no partitioning on any failure"）。ErrorResponse を返して接続を保てばよい。
3. `vacuum pgbench_branches`、`vacuum pgbench_tellers`、`truncate pgbench_history` — `tryExecuteStatement` なので**失敗しても警告だけで続行**。`-n` で送らなくなる。

本体（`-M simple`、既定）[実機]:

```
BEGIN;
UPDATE pgbench_accounts SET abalance = abalance + 1868 WHERE aid = 12678;
SELECT abalance FROM pgbench_accounts WHERE aid = 12678;
UPDATE pgbench_tellers SET tbalance = tbalance + 1868 WHERE tid = 6;
UPDATE pgbench_branches SET bbalance = bbalance + 1868 WHERE bid = 1;
INSERT INTO pgbench_history (tid, bid, aid, delta, mtime) VALUES (6, 1, 12678, 1868, CURRENT_TIMESTAMP);
END;
```

- `\set aid random(1, 100000 * :scale)` などはクライアントが計算し、`:aid` を**リテラルに置き換えてから** Simple Query で送る。サーバは `\set` を見ない。
- `-M extended` は `PQsendQueryParams`（名前なしの文、パラメータの型は指定なし = 0）、`-M prepared` は `PQprepare` で `P_0` 〜 `P_6` を作り `PQsendQueryPrepared` を使う [実機/ソース L3112-3196]。`abalance + $1` の `$1` の型を推論する必要がある（P3）。
- 本体で必要なもの: UPDATE（M2）、主キーによる点検索（M4 の B+Tree が無いと全件走査になり非常に遅いが動く）、BEGIN/END、`CURRENT_TIMESTAMP`（timestamptz）を timestamp 列に入れる代入キャスト（T2）。
- 複数クライアント（`-c N`）: スケール 1 では全トランザクションが `bid = 1` の行を更新するので、行ロックの競合が最大になる。M3 のグローバル書き込みロックなら直列化されるだけで正しく動く。M5 の複数ライターでは、Read Committed の行ロック待ちと EvalPlanQual 相当の再評価（`m3-tx-semantics.md` §4）が要る。`--max-tries` を指定したときだけ `40001`/`40P01` を再試行する（**[未検証]**: 既定値は 1 で再試行しない）。
- スクリプト中で COPY は使えない（pgbench が `COPY is not supported in pgbench, aborting` で中止する、L3340）。

#### 3.2.3 M3 で pgbench を使う方法

組み込みスクリプトを使わなければ `GetTableInfo` は呼ばれない [ソース L7290]。

```sh
psql -f init.sql                       # 表の作成と INSERT ... SELECT か複数行 VALUES
pgbench -n -f bench.sql -c 4 -T 30     # -n で VACUUM と TRUNCATE を送らない
```

`bench.sql` を `int`/`text` と UPDATE/INSERT/SELECT だけで書けば、M3 の WAL・コミットの性能測定とクラッシュ試験の負荷生成に使える。**M3 の性能の基準として推奨する**（工数 S）。

### 3.3 pg_dump と、その出力のリストア

#### 3.3.1 pg_dump が送るもの [実機]

76 本の文を送った（`-t` で表を 2 つに絞った場合）。主なもの:

- 接続時: `set_config('search_path', '', false)`、`pg_is_in_recovery()`、第 2.6 節の SET、`SELECT set_config(name, 'view, foreign-table', false) FROM pg_settings WHERE name = 'restrict_nonsystem_relation_kind'`
- `BEGIN` → `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY`（Q15）
- 対象表の解決: `n.oid OPERATOR(pg_catalog.=) c.relnamespace`、`RESET search_path`
- カタログの全件読み込み: pg_roles、pg_extension、pg_depend（`refclassid = 'pg_extension'::regclass`）、pg_namespace（`acldefault('n', nspowner)`）、pg_class、pg_proc LEFT JOIN pg_init_privs、pg_type、pg_language、pg_operator、pg_am（`amhandler::regproc`）、pg_opclass、pg_opfamily、pg_ts_*、pg_foreign_*（`ARRAY(SELECT quote_ident(option_name) ... FROM pg_options_to_table(...))`）、pg_default_acl、pg_collation、pg_conversion、pg_cast（`NOT EXISTS (SELECT 1 FROM pg_range ...)`）、pg_transform、pg_inherits、pg_event_trigger（`unnest(evttags)`）、pg_partitioned_table、pg_index、pg_statistic_ext、pg_constraint、pg_trigger、pg_rewrite、pg_policy、pg_publication*、pg_subscription、pg_description、pg_seclabels、pg_sequence
- 列の取得: `FROM unnest('{16418,16516}'::pg_catalog.oid[]) AS src(tbloid) JOIN pg_attribute a ...`、`attmissingval`（anyarray）、`array_to_string(a.attoptions, ', ')`
- `WITH RECURSIVE w AS (...)`（マテリアライズドビューの依存関係）
- `LOCK TABLE public.pgbench_branches, public.t1 IN ACCESS SHARE MODE`（Q14）
- シーケンスの状態: `SELECT last_value, is_called FROM public.t1_id_seq`
- データ: `COPY public.t1 (id, name, n) TO stdout;`（P2）
- 関数・型・ドメインなどがあると `PREPARE dumpFunc(pg_catalog.oid) AS ...` / `EXECUTE dumpFunc('16384')` を使う [ソース]（Q17）

pg_dump 17 は 9.2 以上のサーバに対応する（**[未検証]**: 下限）。古い版の分岐を通らないよう 17 系を名乗るのがよい。

**結論**: pg_dump はカタログの網羅、配列、逆パース、RR、COPY TO をすべて必要とするため **M6** とする。M5 の終わりに `pg_dump --schema-only -t 表` から試すのが現実的。

#### 3.3.2 pg_dump の出力のリストア（`psql -f dump.sql`）[実機]

出力の SQL はリストアの互換性の目標として使いやすい。必要なもの:

- 先頭の SET（第 2.6 節）と `SELECT pg_catalog.set_config('search_path', '', false);`（以降の名前はすべて `public.t1` のように修飾される。組み込み関数は search_path が空でも pg_catalog から見つかること）
- `CREATE TABLE public.t (...) WITH (fillfactor='100');`、`CONSTRAINT t1_n_check CHECK ((n > 0))`
- `ALTER TABLE public.t OWNER TO postgres;`（受け付けるだけでよい）
- `CREATE SEQUENCE public.t1_id_seq AS integer START WITH 1 INCREMENT BY 1 NO MINVALUE NO MAXVALUE CACHE 1;`、`ALTER SEQUENCE ... OWNED BY public.t1.id;`、`ALTER TABLE ONLY public.t1 ALTER COLUMN id SET DEFAULT nextval('public.t1_id_seq'::regclass);`
- `COPY public.t1 (id, name, n) FROM stdin;` の後にデータ行と `\.`（psql が CopyData として送る、P1）
- `SELECT pg_catalog.setval('public.t1_id_seq', 1, false);`
- `ALTER TABLE ONLY ... ADD CONSTRAINT ... PRIMARY KEY (...)`、`CREATE INDEX t1_n ON public.t1 USING btree (n);`
- psql 17.6 以降の pg_dump は先頭と末尾に `\restrict <key>` / `\unrestrict <key>` を出力する [実機]。これは psql 側のメタコマンドで、サーバには届かない。

これらは M4 の範囲（SERIAL、PRIMARY KEY、COPY FROM）でほぼ揃う。**M4 の互換性目標の 1 つとして推奨する**。

### 3.4 SQLAlchemy 2（psycopg 3 / psycopg2）[実機]

接続時（ダイアレクトの初期化。psycopg 3 の場合）:

```
BEGIN
select pg_catalog.version()
select current_schema()
show transaction isolation level
show standard_conforming_strings
SAVEPOINT "_pg3_1"
SELECT typname AS name, oid, typarray AS array_oid, oid::regtype::text AS regtype, typdelim AS delimiter
  FROM pg_type t WHERE t.oid = to_regtype($1) ORDER BY t.oid      -- Extended Query, $1 = 'hstore'
RELEASE "_pg3_1"
ROLLBACK
```

- psycopg2 では SAVEPOINT の代わりに `SELECT t.oid, typarray FROM pg_type t JOIN pg_namespace ns ON typnamespace = ns.oid WHERE typname = 'hstore'` を Simple Query で送る。
- `version()` の文字列を正規表現で解析してサーバの版を決める（**[未検証]**: `PostgreSQL 17.0 ...` の形で始まらないと失敗する可能性）。M1 の `version()` の値をこの形にしておく。
- `show transaction isolation level` の `SHOW` の名前は空白を含む特殊形（`SHOW TRANSACTION ISOLATION LEVEL`）。

`inspect()` と `reflect()`（psycopg 3 では**すべて Extended Query**、psycopg2 では Simple Query）:

- `get_table_names`: pg_class JOIN pg_namespace、`relkind = ANY (ARRAY[$1::VARCHAR, ...])`、`pg_table_is_visible`
- `get_columns`: `format_type`、スカラーサブクエリで `pg_get_expr`、`json_build_object(...)`（IDENTITY 列のとき）、`pg_get_serial_sequence(CAST(CAST(attrelid AS REGCLASS) AS TEXT), attname)`、`pg_collation_is_visible`、LEFT OUTER JOIN pg_description
- `get_pk_constraint` / `get_unique_constraints`: **SELECT 句の** `unnest(pg_index.indkey)` と `generate_subscripts(pg_index.indkey, 1)`、`array_agg(CAST(attname AS TEXT) ORDER BY ord)`、`bool_and`、`CAST($2 AS REGCLASS)`（`'pg_constraint'` を regclass へ）
- `get_indexes`: pg_am、pg_opclass、`indoption`、`reloptions`、`pg_get_indexdef`
- `get_foreign_keys`: `pg_get_constraintdef(oid, true)` の**文字列を正規表現で解析する**（**[未検証]**: SQLAlchemy のソースで確認していない）。逆パースの出力形式が違うと外部キーが読めない。
- `reflect()` は加えて `current_setting($1::VARCHAR)`（`default_table_access_method`）を参照する。

**結論**: psycopg2 経由の単純な CRUD（ORM の `Session` で INSERT/SELECT/UPDATE）は M4 で動く見込み。psycopg 3 は M5（Extended Query と SAVEPOINT）。Alembic の自動生成などの完全な inspect は配列と SELECT 句の集合返却関数が要るので M5 の終わり〜M6。

### 3.5 Prisma 6（`prisma db pull`）[実機]

- すべて名前付きの準備済み文（`s0` 〜 `s13`）で送る。Extended Query（P3）が前提。
- 最初の 2 本: `SELECT version()`、`SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname = $1), version(), current_setting('server_version_num')::integer as numeric_version`
- 参照するもの: information_schema.columns、information_schema.sequences（C4）、pg_views、pg_enum、pg_extension、pg_proc（`pg_get_functiondef`）、pg_language、pg_index、pg_opclass、pg_am、pg_constraint（`pg_get_constraintdef`）、`obj_description`/`col_description`、`unnest`、`generate_subscripts`、`WITH rawindex AS (...)`、`::regproc`
- 実行時のクエリエンジン（Prisma Client）はバイナリ形式の結果を要求する可能性がある（**[未検証]**）。Prisma 7 では既定の接続方式が変わった（JavaScript のドライバアダプタ経由、**[未検証]**）。
- `prisma migrate` は勧告的ロック（`pg_advisory_lock`）を使う（**[未検証]**）。

**結論**: M6（information_schema とバイナリ形式）。

### 3.6 Rails ActiveRecord（main ブランチ）[ソース]

- PostgreSQL 10 未満は拒否する（L772）。17 系を名乗れば問題ない。
- 接続時: `parameter_status` を見て、値が違うものだけ `SET SESSION <name> TO <value>` を送る（`standard_conforming_strings`、`intervalstyle = iso_8601`、`client_min_messages = warning`、`timezone`）。M1 の ParameterStatus で `standard_conforming_strings=on`、`TimeZone=UTC` を送っているので、`intervalstyle` と `client_min_messages` の SET だけが届く見込み（**[未検証]**: `client_min_messages` は ParameterStatus に含まれないので必ず送られる）。
- `schema_search_path` の設定（`SET search_path`）と `SHOW search_path`（**[未検証]**: 取得方法）。
- 型の読み込み: `SELECT t.oid, t.typname, t.typelem, t.typdelim, t.typinput, r.rngsubtype, t.typtype, t.typbasetype FROM pg_type AS t LEFT JOIN pg_range AS r ON oid = rngtypid WHERE ...`。PG の版によって `t.typnamespace != 'pg_catalog'::regnamespace` が付き、複数の条件は UNION でつなぐ（L1060-1080）。released 版（7.x/8.0）では `t.typinput = 'array_in(cstring,oid,integer)'::regprocedure` を条件に使う（**[未検証]**: main では変わっている）。
- マイグレーション: `schema_migrations`、`ar_internal_metadata` の表、`pg_try_advisory_lock`（**[未検証]**）。
- pg gem は既定で `exec_params`（Extended Query）を使う（**[未検証]**）。

**結論**: M5（Extended Query、regnamespace/regprocedure、pg_range）。

### 3.7 pgJDBC と DBeaver [ソース]

- pgJDBC の接続時の処理は `research-pg-protocol.md` §4.3 を参照（`SET extra_float_digits = 3`、`SET application_name`。既定で Extended Query、同じ文を 5 回実行すると名前付きの準備済み文に切り替える `prepareThreshold=5`）。
- pgJDBC は未知の型 OID を見ると `pg_type` と `pg_namespace` を問い合わせる（`TypeInfoCache`、**[未検証]**: 問い合わせの中身）。
- DBeaver の接続時 [ソース]:
  - `SELECT current_database()`（ブートストラップ接続）
  - `SELECT db.oid,db.* FROM pg_catalog.pg_database db WHERE 1 = 1 AND datallowconn AND NOT datistemplate OR db.datname =?`（**`oid` と `*` を同時に指定**するので、`oid` 列を持つ表で `db.oid, db.*` が通ること）
  - `SELECT version()`
  - `SELECT reset_val FROM pg_settings WHERE name = 'search_path'`（C3 の pg_settings に `reset_val` 列が必要）
  - `SELECT current_schema(),session_user`
- ナビゲータを開くと pg_namespace、pg_class、pg_type、pg_am、pg_collation、pg_language、pg_event_trigger、pg_available_extensions、`pg_database_size(db.oid)` などを読む [ソース `PostgreDatabase.java`]。

**結論**: 接続だけなら M5（Extended Query + pg_settings）、ナビゲータの表示は M6。

### 3.8 pgAdmin 4 [ソース]

接続直後に次を **1 つの Simple Query** で送る（複数文。1 つでも失敗すると接続を閉じる）:

```
SET DateStyle=ISO; SET client_min_messages=notice;
SELECT set_config('bytea_output','hex',false) FROM pg_show_all_settings() WHERE name = 'bytea_output';
SET client_encoding='utf-8';
```

続いて `SELECT version()`、pg_database（`pg_encoding_to_char`、`has_database_privilege(db.oid, 'CREATE')`、`datistemplate`）、`server_version >= 120000` なら `pg_stat_gssapi WHERE pid = pg_backend_pid()`、ロール情報（`WITH RECURSIVE cte AS (...)` を `ARRAY(...)` に入れて `'pg_signal_backend' = ANY(...)`、pg_auth_members）。psycopg 3 を使うので、パラメータ付きの問い合わせは Extended Query になる。

- `client_encoding='utf-8'` のようにハイフン付きの別名を受け付ける必要がある（PostgreSQL は `utf-8` を `UTF8` と解釈する、**[未検証]**: yuzhu の M1 実装がこれを受けるか）。
- ブラウザツリーは版ごとの SQL テンプレート（数百本）を使う。対応する最低の版がある（**[未検証]**: 現行の pgAdmin は PG 13 以上を想定）。

**結論**: 接続は M6（WITH RECURSIVE、pg_auth_members、pg_stat_gssapi）。ツリーの表示は M6 以降の長期目標。

---

## 4. COPY プロトコルの設計提案（P1/P2）

### 4.1 メッセージの流れ（protocol-flow 文書の「COPY Operations」による）

**COPY FROM STDIN**:

1. クライアント: `Q`（`COPY t FROM STDIN ...`）
2. サーバ: `G` CopyInResponse（Int8 全体の形式 0=テキスト/1=バイナリ、Int16 列数、Int16×列数 各列の形式）
3. クライアント: `d` CopyData を任意個。**メッセージの境界は行の境界と一致しない**（psql は 8KB 単位で複数行をまとめ、バイナリ形式では `fread` の単位で送る [ソース copy.c L568-665]）。サーバはバイト列として連結して行に分ける。
4. クライアント: `c` CopyDone、または `f` CopyFail（エラーメッセージの文字列つき）
5. サーバ: 成功なら `C` CommandComplete（`COPY n`）、失敗なら `E` ErrorResponse。CopyFail のときは `57014`（`COPY from stdin failed: <メッセージ>`、**[未検証]**: SQLSTATE）。
6. Simple Query の場合、同じ `Q` に続く文があればそれを実行し、最後に `Z` ReadyForQuery。

- CopyIn の間に受けた Flush（`H`）と Sync（`S`）は無視する（文書に明記）。それ以外のメッセージは `08P01` protocol_violation とする（**[未検証]**: PostgreSQL の正確な挙動）。
- データ途中でエラーを検出しても、サーバは**CopyDone/CopyFail まで受信を続けて捨てる**必要がある（クライアントは送り続けるため）。Extended Query から始めた COPY でエラーが起きたら、その後 Sync まで捨てる。
- テキスト形式では `\.` だけの行をデータの終わりとして扱う。pgbench と psql はどちらもこの行を CopyData として送る [ソース pgbench.c L5107、copy.c L626-640]。それ以降のデータは捨てる（**[未検証]**: PG17 の細部。PG18 で CSV の `\.` の扱いが変わった）。

**COPY TO STDOUT**:

1. サーバ: `H` CopyOutResponse（形式は CopyInResponse と同じ）
2. サーバ: 1 行ごとに `d` CopyData（PostgreSQL は 1 メッセージ 1 行で送る）
3. サーバ: `c` CopyDone → `C` CommandComplete（`COPY n`）

### 4.2 テキスト形式の規則（sql-copy 文書）

- 区切りはタブ、行末は `\n`（`\r\n` も受け付ける）、NULL は `\N`
- バックスラッシュのエスケープ: `\b \f \n \r \t \v \\`、`\digits`（8 進 1〜3 桁）、`\xhh`（16 進 1〜2 桁）、それ以外の `\c` は c そのもの
- 列数の過不足は `22P04` bad_copy_file_format（`extra data after last expected column`、`missing data for column "x"`）
- 値の変換は各型の入力関数で行う（`22P02` など）。エラーには `CONTEXT: COPY t, line 3, column aid: "abc"` を付ける
- 出力: 各型の出力関数の結果に、タブ・改行・バックスラッシュなどのエスケープを施す

### 4.3 yuzhu での実装方針

- **プロトコル層**（`yuzhu-server/src/connection.rs`）: Session から「COPY IN を開始する」という結果を受け取り、`G` を送ってメッセージループを CopyIn 状態にする。CopyData のバイト列を Session に渡す。Session は `CopySink` を通して行を受け取り、内部で行に分けて INSERT 経路に流す。
  - `ResultSink` に `copy_in_start(format, ncols)`、`copy_out_start(...)`、`copy_out_row(bytes)` を足す（`m1.md` の契約の追加になるので `spec/design/m1-changes.md` に記録）。
- **実行層**: COPY FROM は INSERT と同じく、DEFAULT（列リストに無い列）、NOT NULL、CHECK、主キー/UNIQUE の検査、インデックスの更新を行う。トリガは無い。
- **トランザクションと MVCC**: COPY FROM は書き込み文としてグローバル書き込みロックを取る（`m3-tx-semantics.md` §5.1 にすでに記載）。1 行ごとに WAL を書くと遅いので、PostgreSQL の `heap_multi_insert`（XLOG_HEAP2_MULTI_INSERT）のように、ページ単位でまとめて 1 レコードにすることを M5 以降の最適化として検討する。
- **FREEZE**: 構文として受け付ける。PostgreSQL の前提条件（同じトランザクションで作成または TRUNCATE された表）の検査だけ行い、凍結は行わない（通常の xmin で挿入する）。違いは「他のトランザクションから早く見える」点だけで、pgbench の結果に影響しない。
- **メモリ**: CopyData を全部ためてから処理しない。行単位で流す（pgbench のスケール 100 で 1,000 万行）。

### 4.4 工数

| 項目 | 工数 |
|---|---|
| プロトコル層の CopyIn 状態（Simple Query のみ） | S |
| テキスト形式の解析（エスケープ、`\N`、`\.`、列リスト、エラーの CONTEXT） | M |
| COPY 文の構文（`FROM STDIN`、`TO STDOUT`、列リスト、`WITH (FORMAT text, FREEZE, DELIMITER, NULL)`、古い構文 `WITH NULL AS`） | S |
| COPY TO STDOUT（テキスト）、`COPY (query) TO STDOUT` | S〜M |
| CSV 形式（引用符、`HEADER`、`QUOTE`/`ESCAPE`/`FORCE_*`） | M |
| Extended Query 経由の COPY | S（P3 の後） |
| バイナリ形式（`PGCOPY\n\377\r\n\0` のヘッダ、各型の send/recv） | M（P4 の型の send/recv があれば S） |
| サーバ側のファイル `COPY t FROM '/path'` | 非推奨（権限モデルが要る。M6 以降に必要なら） |

---

## 5. 優先順位つきチェックリスト（マイルストーン別）

優先度: **A** = そのマイルストーンの完了条件に入れる、**B** = 入れたい、**C** = 余裕があれば。

### M2（永続化）

| 優先 | 項目 | 対象ツール | 記号 |
|---|---|---|---|
| A | `\dt` `\d`（一覧）`\l` `\dn` `\du` が動く（既存の `m2-catalog.md` の計画どおり） | psql | Q1、Q8、F1、C3 |
| A | `server_version` を 17 系にし、`server_version_num` GUC も対応させる | psql、Prisma、Rails | — |
| B | pg_am（`heap` 1 行）を作る（`\dt` が LEFT JOIN する） | psql | C2 |
| B | 未知の GUC を受け付けるための一覧を `settings.rs` に用意する | pg_dump 出力、Rails | 2.6 節 |

### M3（トランザクションと耐久性）

| 優先 | 項目 | 対象ツール | 記号 |
|---|---|---|---|
| A | `pgbench -n -f custom.sql` を性能測定と負荷生成に使う（組み込みスクリプトは使わない） | pgbench | 3.2.3 |
| B | TRUNCATE（複数表、トランザクション内、MVCC 上でロールバックできること） | pgbench | Q10 |
| B | `VACUUM` / `ANALYZE` を「何もせず成功」で受け付ける（トランザクションブロック内なら `25001`） | pgbench | Q13 |

### M4（インデックスとクエリ）

| 優先 | 項目 | 対象ツール | 記号 |
|---|---|---|---|
| A | **COPY FROM STDIN（テキスト形式、Simple Query）** | pgbench、リストア、`\copy` | P1 |
| A | `char(n)`、`timestamp`/`timestamptz`、`CURRENT_TIMESTAMP` を M5 から前倒し | pgbench | T1、T2 |
| A | `CREATE TABLE ... WITH (fillfactor=N)`（reloptions に保存。範囲外と未知の名前は `22023`） | pgbench、pg_dump 出力 | Q11 |
| A | `ALTER TABLE ... ADD PRIMARY KEY` / `ADD CONSTRAINT ... UNIQUE` | pgbench、pg_dump 出力 | Q12 |
| A | **`pgbench -i` と `pgbench -c 4 -T 30`（`-M simple`）が完走する** | pgbench | 3.2 |
| A | 中身が空のカタログ表（第 2.3 節の C2）を PG17 の列構成で作る | psql、pg_dump、ORM | C2 |
| A | 逆パース（`format_type`、`pg_get_expr`、`pg_get_constraintdef`、`pg_get_indexdef`）を PostgreSQL と同じ文字列で返す | psql、ORM | F2 |
| A | `\di` と `\d tbl` が表示できる（配列を使う 2 本は M5 で） | psql | 3.1 |
| B | pg_dump の出力（表、SERIAL、主キー、インデックス、COPY データ）を `psql -f` でリストアできる | pg_dump 出力 | 3.3.2 |
| B | `::regclass`、`::regtype`、文字列リテラルと oid の比較 | psql | F3 |
| B | 互換性テストの仕組み `tests/compat/`（第 6 節） | 全部 | — |
| C | FROM 句の `generate_series`（`pgbench -I dtGvp`） | pgbench | Q7 |

### M5（実用化）

| 優先 | 項目 | 対象ツール | 記号 |
|---|---|---|---|
| A | Extended Query（パラメータの型推論、名前付き文、ParameterDescription、RowDescription の Describe） | psycopg 3、pgJDBC、Prisma、pgbench `-M extended/prepared` | P3 |
| A | SAVEPOINT / RELEASE / ROLLBACK TO（**マイルストーンへの追加を提案**） | SQLAlchemy + psycopg 3、Rails、psql | Q16 |
| A | 配列型と配列の演算（`= ANY`、`ARRAY(SELECT)`、添字、`'{...}'::oid[]`） | psql `\d tbl`、SQLAlchemy、pg_dump | Q6、T4 |
| A | COPY TO STDOUT、CSV 形式、Extended Query 経由の COPY | pg_dump、`\copy` | P2、P5 |
| A | VACUUM の実装（M3〜M4 の何もしない実装を置き換える） | pgbench | Q13 |
| A | Repeatable Read と `SET TRANSACTION ... READ ONLY` | pg_dump | Q15 |
| B | SELECT 句の集合返却関数、`array_agg(... ORDER BY)`、`string_agg`、`bool_and` | SQLAlchemy | Q7、Q9 |
| B | ビュー（pg_roles、pg_settings などを本物のビューに置き換える）、`WITH`（非再帰） | 全部 | C3、Q5 |
| B | SQL レベルの PREPARE/EXECUTE、`LOCK TABLE` | pg_dump | Q14、Q17 |
| B | FOREIGN KEY（`pgbench -i -I dtgvpf`、`\d tbl` の参照の表示） | pgbench、psql | — |
| C | バイナリ形式の結果とパラメータ | Prisma、pgJDBC | P4 |

### M6 以降

| 優先 | 項目 | 対象ツール | 記号 |
|---|---|---|---|
| A | pg_dump（`--schema-only` から始めて全体へ）。acldefault、pg_depend の中身、`WITH RECURSIVE` | pg_dump | 3.3.1 |
| A | information_schema（`columns`、`tables`、`sequences`） | Prisma、GUI | C4 |
| B | pgAdmin の接続（pg_auth_members、`pg_show_all_settings`、`has_database_privilege`、pg_stat_gssapi） | pgAdmin | 3.8 |
| B | DBeaver のナビゲータ（pg_database_size、pg_available_extensions など） | DBeaver | 3.7 |
| B | `CROSS JOIN LATERAL`（pgbench のパーティション確認を正しく通す） | pgbench | Q4 |
| C | `json`/`jsonb`、`json_build_object` | SQLAlchemy | T5 |
| C | COPY のバイナリ形式 | ドライバ | P6 |

---

## 6. 互換性テストの進め方（提案）

`tests/` の sqllogictest は「同じ SQL を PostgreSQL と yuzhu に流して結果を比べる」ものだが、COPY のデータ、psql のメタコマンドの表示、pgbench の完走は書けない。次の構成を提案する（工数 M、M4 で作る）。

```
tests/compat/
├── run.sh                 --target postgres|yuzhu。postgres:17 コンテナの psql/pgbench/pg_dump を使う
├── psql/                  *.sql（\d 系のメタコマンドを含む）と期待出力。psql -X -E なしの表示を比べる
├── pgbench/               pgbench -i -s 1 && pgbench -t 100 -c 4 の完走と、終了後の不変条件
│                          （sum(abalance) = sum(tbalance) = sum(bbalance) = sum(delta)）を SQL で確認
├── restore/               PostgreSQL で作った pg_dump の出力を yuzhu に psql -f で流し、表の内容を比べる
└── drivers/               Python（psycopg2/psycopg 3/SQLAlchemy）のスクリプト。uv で依存を入れる
```

- 期待出力は PostgreSQL 17 で生成し、yuzhu の出力と diff する（OID の値など環境に依存する部分は置き換える）。
- サーバ側で受けた文の記録（本調査で使った `log_statement=all`）を、新しいツールに対応するときの調査手順として `tests/compat/README.md` に残す。
- pgbench の不変条件の確認は、M5 の複数ライターと分離レベルの試験にもなる。

---

## 7. 工数のまとめ

| 塊 | 主な中身 | 工数 | 時期 |
|---|---|---|---|
| COPY FROM STDIN（テキスト） | 4.4 節の上 3 行 | M（3〜5 日） | M4 |
| pgbench の残り | char(n)、timestamp、fillfactor、ALTER TABLE ADD PRIMARY KEY、TRUNCATE、VACUUM の受け付け | M（4〜6 日。B+Tree と SERIAL は M4 本体の工数に含めない） | M3〜M4 |
| 空のカタログ表 | 約 30 表の列定義と initdb への組み込み | M（2〜4 日） | M4 |
| 逆パース | format_type、pg_get_expr、pg_get_constraintdef、pg_get_indexdef（psql と同じ文字列） | L（1〜2 週間） | M4 |
| psql `\d tbl` の完成 | regclass 系、配列が要る 2 本 | M | M4〜M5 |
| 互換性テストの仕組み | 第 6 節 | M | M4 |
| GUC の一覧 | 2.6 節 | S | M2 |
| Extended Query | プロトコル、型推論、文とポータルの管理 | L | M5 |
| SAVEPOINT | サブトランザクション ID、MVCC の可視性、WAL | L（1〜2 週間） | M5 |
| 配列 | 型、入出力、演算子、関数、`ANY` | L | M5 |
| COPY TO / CSV | 4.4 節 | M | M5 |
| pg_dump | カタログの網羅、acldefault、依存関係、PREPARE、RR | L（2〜3 週間） | M6 |
| information_schema | ビューの定義 | L | M6 |
| pgAdmin/DBeaver | 個別の関数とビュー | M〜L | M6 |

---

## 8. 確認事項（仮決めした点）

ユーザーに確認したい点。回答があるまで推奨案で進め、`QUESTIONS.md` に記録する。

1. **COPY FROM STDIN を M4 に前倒しするか**（推奨: する）。pgbench の初期化、pg_dump の出力のリストア、psql の `\copy` で必要。代わりに `pgbench -i -I dtGvp`（サーバ側で `generate_series` を使って生成）でしのぐ案もあるが、COPY の方が使い道が広い。
2. **`char(n)` と `timestamp`/`timestamptz` を M5 から M4 に前倒しするか**（推奨: する）。pgbench の表定義に含まれる。前倒ししない場合は、M4 では `pgbench -f` の独自スクリプトだけで測る。
3. **SAVEPOINT をマイルストーンに追加するか**（推奨: M5 に追加）。SQLAlchemy + psycopg 3 が接続時に使い、Rails の入れ子トランザクションと psql の `ON_ERROR_ROLLBACK` も使う。
4. **M3 の性能の基準を `pgbench -n -f custom.sql` にするか**（推奨: する）。組み込みの TPC-B 風スクリプトは M4 の完了条件にする。
5. **VACUUM / ANALYZE を M3〜M4 では「何もせず成功」にするか**（推奨: する）。pgbench の初期化は VACUUM が失敗すると中止する。M5 で本物に置き換える。
6. **COPY の FREEZE は前提条件の検査だけ行い、凍結はしないでよいか**（推奨: それでよい）。
7. **psql の `\d tbl` を「サーバ側で問い合わせの形を見て空の結果を返す」方法で早期に動かすことはしない**（推奨: しない。中身が空のカタログ表と、正しい解析で対応する）。
8. **PostgreSQL に存在するが yuzhu では意味を持たない GUC を、受け付けて保存するだけにするか**（推奨: する。名前の一覧を持ち、一覧に無い名前は PostgreSQL と同じく `42704`）。
9. **互換性テスト `tests/compat/` を M4 で作るか**（推奨: 作る）。CI では postgres:17 と yuzhu の両方で実行する。
10. **ORM の対象の優先順位**（推奨: psycopg2/SQLAlchemy → psycopg 3 → pgJDBC → Rails → Prisma）。psycopg2 は Simple Query だけで済むので M4 で試せる。

---

## 9. 未検証事項の一覧

- `\d tbl` で外部キーとトリガの問い合わせが送られる条件（今回の表では送られなかった）
- COPY FREEZE の前提条件を満たさないときの SQLSTATE（`55000` と推定）
- CopyFail を受けたときの SQLSTATE（`57014` と推定）と、CopyIn 中に想定外のメッセージを受けたときの PostgreSQL の挙動
- PG17 で `\.` の後に続くデータの扱い（PG18 で CSV の扱いが変わった）
- pgbench の `--max-tries` の既定値（1 と推定）
- SQLAlchemy の `version()` 文字列の解析規則と、`get_foreign_keys` が `pg_get_constraintdef` の出力を正規表現で解析しているか
- SQLAlchemy + psycopg 3 で SAVEPOINT が失敗したときに接続そのものが失敗するか
- Prisma Client（実行時）がバイナリ形式の結果を要求するか、Prisma 7 での接続方式、`prisma migrate` の勧告的ロック
- Rails の released 版（7.x/8.0）での型の読み込み（`::regprocedure` の使用）、`SHOW search_path` の使用、pg gem の既定のプロトコル
- pgJDBC の `TypeInfoCache` の問い合わせの中身と、バイナリ転送を使う型
- pgAdmin の対応する最低の PostgreSQL の版、`client_encoding='utf-8'` を yuzhu の M1 実装が受け付けるか
- pg_dump 17 が対応するサーバの下限（9.2 と推定）
