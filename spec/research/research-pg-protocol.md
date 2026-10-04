# yuzhu M1 調査レポート: PostgreSQL ワイヤプロトコル（Simple Query のみ）で psql と主要ドライバを接続させるために必要なもの

調査日: 2026-10-04
対象: PostgreSQL プロトコル 3.0（ドキュメントの current は PG18）、psql/libpq REL_17_STABLE、tokio-postgres master、psycopg 3 master、pgJDBC master、node-postgres master

> 凡例: 「事実」はソースコードやドキュメントで確認したもの。「推奨」は yuzhu 向けの設計判断。

---

## 0. 主な出典

| 種別 | URL |
|---|---|
| プロトコルの流れ | https://www.postgresql.org/docs/current/protocol-flow.html |
| メッセージ形式 | https://www.postgresql.org/docs/current/protocol-message-formats.html |
| エラー/通知フィールド | https://www.postgresql.org/docs/current/protocol-error-fields.html |
| libpq 接続パラメータ | https://www.postgresql.org/docs/current/libpq-connect.html |
| SQLSTATE 一覧 | https://www.postgresql.org/docs/current/errcodes-appendix.html |
| libpq ParameterStatus の処理 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/interfaces/libpq/fe-exec.c (`pqSaveParameterStatus`) |
| libpq 接続状態機械 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/interfaces/libpq/fe-connect.c |
| psql 起動処理 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/psql/startup.c |
| psql バージョン警告 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/psql/command.c (`connection_warnings`, `SyncVariables`) |
| psql メタコマンドの SQL | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/psql/describe.c |
| psql の BEGIN 自動送信、is_superuser 等 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/bin/psql/common.c |
| psql の列揃え | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/fe_utils/print.c (`column_type_alignment`) |
| GUC_REPORT の定義 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/misc/guc_tables.c |
| CommandComplete タグの生成 | https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/tcop/cmdtag.c (`BuildQueryCompletionString`) |
| tokio-postgres | https://github.com/sfackler/rust-postgres/tree/master/tokio-postgres/src (`connect_raw.rs`, `connect_tls.rs`, `simple_query.rs`, `query.rs`, `prepare.rs`, `error/mod.rs`) |
| psycopg 3 | https://github.com/psycopg/psycopg/tree/master/psycopg/psycopg (`_cursor_base.py`, `_connection_base.py`, `_encodings.py`, `types/datetime.py`) |
| pgJDBC | https://github.com/pgjdbc/pgjdbc/tree/master/pgjdbc/src/main/java/org/postgresql (`core/v3/ConnectionFactoryImpl.java`, `core/v3/QueryExecutorImpl.java`, `core/v3/SimpleParameterList.java`, `PGProperty.java`, `jdbc/PgConnection.java`, `jdbc/TypeInfoCache.java`) |
| node-postgres | https://github.com/brianc/node-postgres/tree/master/packages (`pg/lib/client.js`, `pg/lib/connection.js`, `pg/lib/query.js`, `pg-protocol/src/serializer.ts`) |

---

## 1. 起動シーケンス

### 1.1 最初のパケット（型バイトなし）の判定

最初のパケットには型バイトがなく、`Int32 長さ（自身を含む）` + `Int32 コード` という形をしている。コードを見て次のように分ける（protocol-message-formats）。

| コード | 意味 | 長さ |
|---|---|---|
| `196608` (0x00030000) | StartupMessage v3.0 | 可変 |
| `196610` (0x00030002) | StartupMessage v3.2（PG18 以降） | 可変 |
| `80877103` (1234<<16 \| 5679) | SSLRequest | 8 |
| `80877104` (1234<<16 \| 5680) | GSSENCRequest | 8 |
| `80877102` (1234<<16 \| 5678) | CancelRequest | 16（v3.0。v3.2 では可変長の鍵） |

**SSL/GSS を断る方法（事実）**: 暗号化しない場合は、平文で 1 バイト `'N'` を返し、同じソケットで次のパケットを待つ。ドキュメントには「`N` の後は通常の StartupMessage を送り、暗号化なしで続行する。あるいは `N` の後に GSSENCRequest を送って GSSAPI 暗号化を試してもよい（その逆も同様）」とある。したがってサーバは **「SSLRequest → N → GSSENCRequest → N → StartupMessage」の順でも受け付けられるループ**にしておく必要がある。
- 注意: `'N'` を返す前に、ソケット上に余分なバイトが届いていないことを確認する。PostgreSQL 本体は、SSLRequest の直後にバイトが積まれている場合（バッファ詰め込み攻撃。CVE-2021-23214 系）を拒否している。M1 では TLS を実装しないので実害はない。
- **PG17 の direct SSL**: `sslnegotiation=direct` を指定すると、クライアントは SSLRequest を送らずにいきなり TLS ClientHello（先頭バイト 0x16）を送ってくる（ALPN は "postgresql"）。libpq の既定は `sslnegotiation=postgres` なので通常は来ない。もし来たら、先頭 4 バイトを長さとして読むとおかしな値になる。M1 では「先頭バイトが 0x16 なら切断する」で十分。

**各クライアントの既定動作（事実）**

| クライアント | SSLRequest | GSSENCRequest | `N` を受けたとき |
|---|---|---|---|
| libpq（psql, psycopg3） | `sslmode=prefer` が既定なので**送ってくる**（SSL 付きでビルドされた libpq の場合） | `gssencmode=prefer` が既定だが、**Kerberos の資格情報キャッシュがある場合だけ**送る | 平文で続行（`fe-connect.c` の `SSLok == 'N'` 分岐） |
| tokio-postgres | 既定は `SslMode::Prefer`。ただし `NoTls` を渡した場合は `tls.can_connect()` が false なので**送らない**（`connect_tls.rs`） | 未対応 | `'S'` 以外なら、Require のときはエラー、それ以外は平文で続行 |
| pgJDBC | 既定は `sslmode=prefer`。送ってくる | 既定 `gssEncMode=allow` なので通常は送らない | prefer/allow なら平文で続行。require 以上なら "The server does not support SSL." |
| node-postgres | 既定 `ssl: false` なので**送らない** | 未対応 | ssl を設定していると、`N` で**エラー**になり、フォールバックしない（`connection.js`: "The server does not support SSL connections"） |

### 1.2 StartupMessage

```
Int32 len, Int32 196608, { CString name, CString value }*, Byte 0
```
- `user` は必須。`database` を省略した場合は user と同じ名前になる。`options`（非推奨。`-c name=value` 形式）、`replication`、`_pq_.*`（プロトコル拡張）がある。それ以外の名前は GUC の初期値として扱われる。
- 実際に送られてくるパラメータ（事実）
  - psql（libpq）: `user`, `database`, `application_name`/`fallback_application_name=psql`, `client_encoding`（psql は TTY のとき `"auto"` を渡す。libpq がそれをロケールから解決し、**UTF8 や、C ロケールなら SQL_ASCII など**を送る。`startup.c` L266、`fe-connect.c` L1841）
  - tokio-postgres: `client_encoding=UTF8`, `user`, `database`, `options`, `application_name`
  - pgJDBC: `user`, `database`, `client_encoding=UTF8`, `DateStyle=ISO`, `TimeZone=<JVM の TZ>`、さらに `assumeMinServerVersion>=9.0` を指定したときだけ `application_name`、`search_path`（currentSchema 指定時）、`options`
  - node-postgres: `user`, `database`, `client_encoding=UTF8`（`serializer.ts` で常に付く）、`application_name`、`options`、`statement_timeout` 等（指定時のみ）
  - psycopg3（libpq）: 既定では `client_encoding` を送らない。その場合サーバ既定値（= server_encoding）が使われる
- **推奨**: 知らないパラメータは無視するか保存するだけにする。`client_encoding` は UTF8/UTF-8/unicode/SQL_ASCII を受け付け、内部はすべて UTF8 として扱う。報告する値はクライアントの要求どおり返すのが無難（後述）。
- v3.2（196610）が来た場合: 本来は NegotiateProtocolVersion（`'v'`、Int32 newest minor=0、Int32 未認識オプション数、名前一覧）を返して 3.0 にダウングレードする。ただし libpq の既定 `max_protocol_version` は **3.0**（PG18 の libpq でも同じ。libpq-connect ドキュメント）なので、M1 では「3.0 以外の major は拒否し、minor>0 なら NegotiateProtocolVersion(0) を返す」で足りる。`_pq_.` で始まるオプションも、同じメッセージで「未認識」として返す。

### 1.3 認証（trust）と起動完了

ドキュメント（protocol-flow, Start-up）による順序は次のとおり。
```
S→C  AuthenticationOk        'R' Int32(8) Int32(0)
S→C  ParameterStatus × N     'S' Int32 len CString name CString value
S→C  BackendKeyData          'K' Int32(12) Int32 pid Int32 secret   (v3.0 は鍵 4 バイト)
S→C  ReadyForQuery           'Z' Int32(5) Byte1('I')
```
- PostgreSQL 本体は ParameterStatus を送ってから BackendKeyData を送る。ドキュメントでは BackendKeyData を先に列挙しているが、**どちらの順序でもクライアントは受け付ける**（libpq、tokio-postgres の `read_info`、pgJDBC はいずれもループで処理する）。本体と同じ順序にしておくのが安全。
- BackendKeyData を省略した場合: tokio-postgres は pid/secret を 0 のまま扱って続行する（`connect_raw.rs`）。それでも **libpq の PQcancel や psql の Ctrl-C のために必ず送ること**を推奨する。
- 認証失敗やデータベースが存在しない場合は、ErrorResponse（S=FATAL, C=28000 / 3D000 など）を返して切断する。
- 起動中に NoticeResponse を送ることも許されている（tokio-postgres は保留して後で配送する）。

### 1.4 ParameterStatus: クライアントが読む値と、無いと壊れるもの

サーバが自動で報告するパラメータ（GUC_REPORT。PG17 の `guc_tables.c` で確認）:
`application_name, client_encoding, DateStyle, default_transaction_read_only(14+), in_hot_standby(14+), integer_datetimes, IntervalStyle, is_superuser, scram_iterations(16+), server_encoding, server_version, session_authorization, standard_conforming_strings, TimeZone`。PG18 では `search_path` が加わった。

| パラメータ | 推奨値 | 誰がどう使うか / 欠けたり不正だったりすると何が起きるか（事実） |
|---|---|---|
| `server_version` | `"16.0"` など（後ろに `" (yuzhu 0.1.0)"` を付けてもよい） | **libpq** は `sscanf("%d.%d.%d")` で `sversion` に変換する。無い場合は 0 になる（`fe-exec.c` L1149-）。**psql** は sversion を見てバナーを出し、`sversion < 90200` やサーバの major が psql より新しいと `WARNING: psql major version X, server major version Y. Some psql features might not work.` を出す（`command.c` L3957）。さらに `describe.c` の \d 系 SQL は **sversion によって参照する列が変わる**（例: \l は 17 以上なら `datlocale`、15〜16 なら `daticulocale`、16 以上なら `daticurules`）。**pgJDBC** は `parseServerVersionStr` で数値化し、`runInitialQueries` で 12 未満なら `SET extra_float_digits = 3`（9.0 未満なら `= 2`）を送る。既定設定では `SET application_name = 'PostgreSQL JDBC Driver'` も送る（後述）。psycopg は `PQserverVersion` を参照する。**必須**。 |
| `server_encoding` | `UTF8` | psql/libpq は表示に使う程度。pgJDBC はマップに保存するだけ。入れておく。 |
| `client_encoding` | `UTF8`（クライアントの要求を返す） | **pgJDBC: UTF8/UTF-8 以外なら接続失敗**（"The server's client_encoding parameter was changed to {0}. The JDBC driver requires client_encoding to be UTF8 for correct operation."）。**psycopg3**: この値から Python のコーデックを決める。無い場合は utf-8 にフォールバックする。**libpq**: 未知の名前なら SQL_ASCII 扱い。`PQescapeString` 等に影響する。**必須**。 |
| `DateStyle` | `ISO, MDY` | **pgJDBC: `ISO` で始まらないと接続失敗**。psycopg3 は date/timestamp のテキストを解釈するのに使う。無い場合は `ISO, DMY` とみなす。未知の値だと `InterfaceError("unexpected DateStyle")`（`types/datetime.py`）。**必須**。 |
| `integer_datetimes` | `on` | **pgJDBC: `on`/`off` 以外の値だと "Protocol error. Session setup failed."**。無い場合は問題ない。binary 形式の timestamp 解釈に影響する。入れておく。 |
| `standard_conforming_strings` | `on` | **pgJDBC: `on`/`off` 以外の値だと接続失敗**。リテラルのエスケープ方法を決める。libpq/psql も、psql の字句解析（`'\'` の扱い）と `PQescapeString` に使う。無いと libpq は off（`std_strings=false`）と解釈する。**必須（on）**。 |
| `TimeZone` | `UTC`、またはクライアントが送った値 | pgJDBC と psycopg が timestamptz の変換に使う。入れておく。 |
| `IntervalStyle` | `postgres` | psycopg3: interval のテキストを解釈するとき、`postgres` 以外だと失敗することがある（無い場合は "unknown" 扱い）。入れておく。 |
| `is_superuser` | `on` | psql のプロンプト `%#`（`#` か `>` か）に使う（`common.c` L2221）。 |
| `session_authorization` | 接続ユーザ名 | psql のプロンプト `%n`。 |
| `application_name` | クライアントの値を返す（空でもよい） | pgJDBC はマップに保存する。 |
| `default_transaction_read_only` / `in_hot_standby` | `off` / `off` | libpq で `target_session_attrs=read-write` などを指定したとき、**これが無いと `SHOW transaction_read_only` や `SELECT pg_catalog.pg_is_in_recovery()` を追加で送ってくる**（`fe-connect.c` L3992-4076）。報告しておけば追加クエリは来ない。 |
| `scram_iterations` | `4096`（任意） | SCRAM を使うときだけ意味がある。trust なら不要。 |

さらに、**SET でこれらの値を変えたときは、次の ReadyForQuery の直前に ParameterStatus を再送する**のが PostgreSQL の動作。pgJDBC は `SET client_encoding` / `SET DateStyle` の結果をこれで検知して判断している。

### 1.5 CancelRequest

- 新しい TCP 接続で、`Int32(16) Int32(80877102) Int32 pid Int32 secret` が送られてくる。サーバは**何も返さずに接続を閉じる**（ドキュメントの仕様。応答がないので、キャンセルが成功したかどうかはクライアントには分からない）。pid と secret が一致すれば、該当セッションの実行中クエリに割り込む。中断されたクエリは ErrorResponse `57014 canceling statement due to user request` を返す。
- 一致しない場合は黙って無視する。
- M1 推奨: pid/secret の受理と照合までは実装する。実行中の中断は「クエリ実行ループでフラグを確認する」程度でよく、M1 では未実装（何もしない）でも致命的ではない。

---

## 2. Simple Query の応答

### 2.1 基本形

`'Q' Int32 len CString query` に対して、文ごとに次のいずれかを返す。
- 行を返す文: `RowDescription('T')` → `DataRow('D')` × n → `CommandComplete('C')`
- 行を返さない文: `CommandComplete`
- 空文字列や空白、`;` だけの場合: `EmptyQueryResponse('I')`（`postgres.c`: parsetree が空なら `NullCommand`）
- エラー: `ErrorResponse('E')`。その時点で残りの文はすべて実行しない
- 最後に**必ず 1 回だけ** `ReadyForQuery('Z')`（エラーのときも送る）
- NoticeResponse('N') と ParameterStatus('S') はいつ挟んでもよい

### 2.2 RowDescription('T')

```
Int16 列数, 各列: CString 名前, Int32 テーブルOID, Int16 列番号, Int32 型OID, Int16 typlen, Int32 typmod, Int16 フォーマット
```
- テーブル列でない場合（式など）はテーブル OID=0、列番号=0。M1 では**常に 0/0 でも動く**。pgJDBC の `ResultSetMetaData.getTableName`/`isNullable` 系はこの値で pg_attribute を引くので、0 なら問い合わせ自体を省略する。
- Simple Query の結果は**フォーマットが常に 0（テキスト）**。
- typmod は「指定なし」なら -1。varchar(n) は n+4、numeric(p,s) は ((p<<16)|s)+4。
- 主な型 OID と typlen（pg_type.dat の値）:

| 型 | OID | typlen | テキスト表現の例 |
|---|---|---|---|
| bool | 16 | 1 | `t` / `f` |
| bytea | 17 | -1 | `\x0a0b` (hex) |
| name | 19 | 64 | |
| int8 | 20 | 8 | `123` |
| int2 | 21 | 2 | |
| int4 | 23 | 4 | |
| text | 25 | -1 | |
| oid | 26 | 4 | |
| json | 114 | -1 | |
| float4 / float8 | 700 / 701 | 4 / 8 | `1.5`, `NaN`, `Infinity`（PG12 以降は最短往復表現） |
| unknown | 705 | -2 | |
| bpchar / varchar | 1042 / 1043 | -1 | |
| date / time | 1082 / 1083 | 4 / 8 | `2026-10-04` |
| timestamp / timestamptz | 1114 / 1184 | 8 / 8 | `2026-10-04 12:34:56`, `...+00` |
| interval | 1186 | 16 | `1 day 02:00:00` |
| numeric | 1700 | -1 | |
| uuid | 2950 | 16 | |
| jsonb | 3802 | -1 | |

- psql は型 OID が int2/4/8、float4/8、numeric、oid、xid、xid8、cid、money の列を**右寄せ**にする（`print.c` の `column_type_alignment`）。それ以外の OID でも表示はできる。
- 未知の OID の扱い: psql、simple_query を使う tokio-postgres、node-postgres（pg-types で未知 OID は文字列）は問題なく扱える。

### 2.3 DataRow('D')

`Int16 列数, 各列: Int32 長さ(NULL は -1), バイト列`。テキストは client_encoding（UTF8）で、末尾に NUL は付けない。

### 2.4 CommandComplete タグ（`cmdtag.c` の `BuildQueryCompletionString` で確認）

| 文 | タグ |
|---|---|
| INSERT | `INSERT 0 <行数>`（OID 欄は常に 0） |
| UPDATE / DELETE / MERGE | `UPDATE n` / `DELETE n` / `MERGE n` |
| SELECT（および SELECT INTO / CREATE TABLE AS） | `SELECT n` |
| FETCH / MOVE / COPY | `FETCH n` / `MOVE n` / `COPY n` |
| DDL | `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `ALTER TABLE`, `CREATE SCHEMA`, `TRUNCATE TABLE` など（行数なし） |
| トランザクション | `BEGIN`, `COMMIT`, `ROLLBACK`, `START TRANSACTION`, `SAVEPOINT`, `RELEASE`。失敗状態で COMMIT すると `ROLLBACK` を返す |
| SET / RESET / SHOW | `SET`, `RESET`, `SHOW`（SHOW は 1 行 1 列の結果を返す。列名は変数名の小文字） |

- tokio-postgres はタグの**最後の単語を数値として読み**、失敗したら 0 にする（`query.rs` の `extract_row_affected`）。node-postgres は `rowCount` を、psycopg は `rowcount` を、pgJDBC は `executeUpdate` の戻り値をそれぞれタグから得る。INSERT の形式を間違えると件数が狂う。

### 2.5 ErrorResponse('E') / NoticeResponse('N')

`(Byte1 フィールド種別, CString 値)* , Byte 0`。主なフィールド（protocol-error-fields）:

| 記号 | 内容 | 推奨 |
|---|---|---|
| `S` | 重大度（ローカライズ版）ERROR/FATAL/PANIC。通知は WARNING/NOTICE/DEBUG/INFO/LOG | **必須** |
| `V` | 重大度（非ローカライズ版。9.6 以降） | 付ける（S と同じ値） |
| `C` | SQLSTATE（5 文字） | **必須** |
| `M` | 主メッセージ | **必須** |
| `D` / `H` | 詳細 / ヒント | 任意 |
| `P` | エラー位置（クエリ文字列内の 1 始まりの文字位置） | 任意。あると psql が `LINE 1: ...^` を出す |
| `s` `t` `c` `d` `n` | スキーマ / テーブル / 列 / 型 / 制約名 | 任意 |
| `F` `L` `R` | ソースファイル / 行 / ルーチン | 任意 |

- **tokio-postgres は S、C、M のどれかが欠けると解析エラーになる**（`error/mod.rs`: "`S` field missing" など）。psycopg は C を使って例外クラス（UniqueViolation など）を選ぶ。
- よく使う SQLSTATE: `42601` syntax_error、`42P01` undefined_table、`42703` undefined_column、`42P07` duplicate_table、`23505` unique_violation、`23502` not_null_violation、`22P02` invalid_text_representation、`22012` division_by_zero、`25P02` in_failed_sql_transaction、`0A000` feature_not_supported、`3D000` invalid_catalog_name、`28000` invalid_authorization_specification、`57014` query_canceled、`08P01` protocol_violation、`XX000` internal_error。
- NoticeResponse の例: トランザクション外で COMMIT すると `WARNING 25P01 there is no transaction in progress`。BEGIN を二重に実行すると `WARNING 25001 there is already a transaction in progress`。

### 2.6 複数の文を含む Query と、途中のエラー（protocol-flow, Multiple Statements in a Simple Query）

- 1 つの Query に `;` 区切りで複数の文がある場合、明示的なトランザクション制御がなければ**全体を 1 つの暗黙トランザクションとして実行する**。途中でエラーになると、それまでの文もロールバックされ、残りは実行されない。ドキュメントの例: `INSERT 1; SELECT 1/0; INSERT 2;` では最初の INSERT も取り消される。
- 途中に `BEGIN` があれば、そこから明示トランザクションに切り替わる。`COMMIT` があれば、そこまでを確定する。
- 文ごとの応答（T/D/C）は順番に返し、エラーが出た時点で E を返して終了し、最後に Z を 1 回だけ返す。
- psql は対話モードでも `-f` でも、**クライアント側で文を `;` で分割して 1 文ずつ送る**。複数文が 1 つの Query で届くのは主に `psql -c "a; b"`、tokio-postgres の `batch_execute`/`simple_query`、psycopg で引数なしの `execute("a; b")`、node-postgres で値なしの `query("a; b")` の場合。

### 2.7 ReadyForQuery のトランザクション状態

`'I'` = トランザクション外、`'T'` = トランザクションブロック中、`'E'` = 失敗したトランザクションブロック中（ROLLBACK するまで、どの文も `25P02 current transaction is aborted, commands ignored until end of transaction block` になる）。
- **この値は実際に使われている（事実）**
  - psql: `AUTOCOMMIT off` のとき、状態が `I` なら `BEGIN` を自動送信する（`common.c` L1141）。`ON_ERROR_ROLLBACK` のときは `SAVEPOINT pg_psql_temporary_savepoint`。プロンプトの `%x` にも使う。
  - psycopg3: **既定は autocommit=False**。状態が `IDLE` なら、最初のクエリの前に `BEGIN` を Simple Query で送る（`_connection_base.py` の `_start_query`、`_exec_command`）。`commit()` は状態が IDLE なら何もしない。
- したがって M1 でも **BEGIN/COMMIT/ROLLBACK を受理し、状態バイトを正しく `I`/`T`/`E` で返すこと**が必要。実際のトランザクション分離（MVCC）が未実装でも、状態遷移だけは正しく返さないと psycopg の挙動がおかしくなる。

---

## 3. psql（16/17）が自動で送るクエリと、メタコマンドが触るカタログ

### 3.1 接続時

- **psql も libpq も、既定では接続時にクエリを 1 つも送らない**（事実）。psql は `PQparameterStatus("server_version")` と `PQserverVersion` でバナーと警告を出すだけ（`connection_warnings`, `SyncVariables`）。
- 例外: libpq で `target_session_attrs=read-write|read-only|primary|standby` を指定した場合、`default_transaction_read_only`/`in_hot_standby` が報告されていないと、`SHOW transaction_read_only` や `SELECT pg_catalog.pg_is_in_recovery()` を送ってくる（§1.4）。
- 対話モードで **TAB 補完**を使うと、そのたびにカタログを問い合わせる（tab-complete.c。`pg_catalog.pg_class` / `quote_ident` / `pg_table_is_visible` など）。失敗しても候補が出ないだけで、psql は止まらない。
- psql の `-l` は `listAllDbs` を `postgres` データベース相手に実行する。

### 3.2 メタコマンド（REL_17_STABLE の describe.c で確認）

**`\dt`（`listTables`）**
```sql
SELECT n.nspname as "Schema", c.relname as "Name",
  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view'
    WHEN 'i' THEN 'index' WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table'
    WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' WHEN 'I' THEN 'partitioned index' END as "Type",
  pg_catalog.pg_get_userbyid(c.relowner) as "Owner"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r','p','')
      AND n.nspname <> 'pg_catalog'
      AND n.nspname !~ '^pg_toast'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;
```
- 必要なもの: `pg_class(oid, relname, relnamespace, relkind, relowner)`、`pg_namespace(oid, nspname)`、`pg_get_userbyid(oid)`、`pg_table_is_visible(oid)`、`LEFT JOIN`、`CASE`、`IN`、`<>`、**正規表現演算子 `!~`**、`ORDER BY` の列番号指定、`AS "引用識別子"`。
- 結果が 0 行なら psql は "Did not find any relations." と表示する。
- `\dt+`（verbose）ではさらに `pg_size_pretty(pg_table_size(c.oid))`、`obj_description(c.oid,'pg_class')`、`c.relpersistence` を使い、sversion が 12 以上なら `pg_am`（`am.amname`）も使う。
- パターンを付けた場合（`\dt foo*`）は `c.relname OPERATOR(pg_catalog.~) '^(foo.*)$' COLLATE pg_catalog.default` という形の条件になる。

**`\l`（`listAllDbs`）**: `pg_database(datname, datdba, encoding, datlocprovider[15+], datcollate, datctype, datlocale[17+] / daticulocale[15-16], daticurules[16+], datacl)`、`pg_get_userbyid`、`pg_encoding_to_char`、`array_to_string(d.datacl, E'\n')`。`\l+` ではさらに `has_database_privilege`、`pg_database_size`、`pg_tablespace`、`shobj_description`、`pg_has_role` を使う。**参照する列は報告した server_version で変わる。**

**`\d <table>`（`describeTableDetails` → `describeOneTableDetails`）**: もっとも重い。
1. `SELECT c.oid, n.nspname, c.relname FROM pg_class c LEFT JOIN pg_namespace n ... WHERE c.relname OPERATOR(pg_catalog.~) '^(t)$' COLLATE pg_catalog.default AND pg_table_is_visible(c.oid) ORDER BY 2, 3;`
2. pg_class の詳細: `relchecks, relkind, relhasindex, relhasrules, relhastriggers, relrowsecurity, relforcerowsecurity, relispartition, reloptions, reltablespace, reloftype::regtype, relpersistence, relreplident, am.amname`。`pg_class tc` と `pg_am` を JOIN し、`array_to_string`、`unnest`、配列連結 `||`、`array(...)` も使う。
3. 列一覧: `a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), (SELECT pg_get_expr(d.adbin, d.adrelid, true) FROM pg_attrdef d ...), a.attnotnull, (SELECT c.collname FROM pg_collation c, pg_type t ...), a.attidentity, a.attgenerated FROM pg_attribute a WHERE a.attrelid = '<oid>' AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum`
4. その後、インデックス（`pg_index`, `pg_get_indexdef`, `pg_constraint`, `pg_get_constraintdef`）、CHECK 制約、外部キー、`pg_policy`、`pg_statistic_ext`、`pg_publication*`、`pg_inherits`、`pg_trigger`、`pg_rewrite` などを、テーブルの属性に応じて順に問い合わせる。

   describeOneTableDetails 内で参照されるカタログと関数の出現回数（多い順）: pg_class(25), regclass(20), pg_get_expr(9), pg_constraint(8), array_to_string(8), unnest(6), quote_ident(6), pg_get_constraintdef(6), regtype(5), pg_publication(5), pg_inherits(5), pg_attribute(4), pg_trigger(3), pg_relation_is_publishable(3), pg_partition_ancestors(3), pg_index(3), regnamespace, quote_literal, pg_statistic_ext, pg_rewrite, pg_publication_rel, pg_options_to_table, pg_namespace, pg_get_ruledef, pg_get_indexdef, pg_depend, pg_am, format_type, pg_type, pg_sequence, pg_roles, pg_policy, pg_get_viewdef, pg_get_triggerdef, pg_collation, pg_attrdef, generate_series, col_description など。
   - `'16384'::regclass` のような **OID 文字列リテラルと oid 列の比較**（暗黙キャスト）も頻繁に出てくる。

### 3.3 M1 と後続マイルストーンの切り分け

| 区分 | 内容 |
|---|---|
| **M1（必須）** | なし。psql は接続時にカタログを一切問い合わせないので、**カタログがなくても接続と CREATE/INSERT/SELECT はできる**。 |
| M1 でできれば | `SELECT version()`、`current_database()`、`current_user`/`current_schema()`、`SHOW <param>`。ユーザがすぐ試し、ツールもよく使う。 |
| M2 候補（`\dt`, `\l`, `\dn`, `\du`） | pg_class/pg_namespace/pg_database/pg_roles の**仮想ビュー**（自前のカタログから合成する）、`pg_get_userbyid`、`pg_table_is_visible`、`pg_encoding_to_char`、`array_to_string`、`~`/`!~` 演算子、LEFT JOIN、CASE、ORDER BY の列番号。 |
| M3 以降（`\d tbl`） | pg_attribute、pg_type、`format_type`、pg_attrdef/`pg_get_expr`、pg_index/pg_constraint/`pg_get_constraintdef`/`pg_get_indexdef`、空の pg_policy/pg_trigger/pg_inherits/pg_publication/pg_statistic_ext/pg_rewrite、regclass/regtype、unnest、サブクエリ、配列。 |

> 参考: CockroachDB、YugabyteDB、Materialize、RisingWave などの PG 互換 DB も、psql の \d に対応するために pg_catalog の仮想テーブルを大量に実装している。報告する server_version は「実装したカタログ列と矛盾しない版」に固定するのが定石（例: CockroachDB は `13.0.0` を報告する）。

---

## 4. 各ドライバが起動後に行うこと / Simple と Extended の使い分け

### 4.1 tokio-postgres（rust-postgres、0.7 系 master）
- 起動後に自分からクエリを送ることは**ない**（`connect_raw.rs` の `read_info` は ReadyForQuery まで K/S/N を集めて終わる）。
- **`Client::query` / `query_one` / `execute` / `prepare` は Extended**（Parse/Describe/Sync → Bind/Execute/Sync）。さらに、RowDescription やパラメータに**未知の型 OID があると `pg_catalog.pg_type`（と `pg_range`、`pg_namespace`、`pg_enum`、複合型なら `pg_attribute`）を prepared statement で問い合わせる**（`prepare.rs` の `TYPEINFO_QUERY`: `SELECT t.typname, t.typtype, t.typelem, r.rngsubtype, t.typbasetype, n.nspname, t.typrelid FROM pg_catalog.pg_type t LEFT OUTER JOIN pg_catalog.pg_range r ... WHERE t.oid = $1`。失敗したときのフォールバック版もある）。組み込み型の OID なら問い合わせない。
- **`simple_query` / `simple_query_raw` / `batch_execute` は Simple Query**。結果は `SimpleQueryMessage::{RowDescription, Row(テキスト), CommandComplete(n)}` で、型の問い合わせは一切しない。予期しないメッセージは `unexpected_message` エラーになる。RowDescription より前に DataRow が来てもエラーになる。
- 結論: **M1 で動くのは `simple_query`/`batch_execute` だけ**。`query`/`execute` は Extended Query（M2 以降）が必要。

### 4.2 psycopg 3（libpq ベース）
- 起動後のクエリは**ない**。ただし既定が **autocommit=False** なので、最初の execute の前に `BEGIN` を Simple Query で送る（`_start_query` → `_exec_command` は TEXT なら `send_query`）。
- `cursor.execute(sql)` を**パラメータなし**で呼ぶと Simple Query（`_cursor_base.py`: `force_extended or query.params or fmt == BINARY` のときだけ `send_query_params`）。**パラメータがある場合、binary=True の場合、`executemany`、pipeline モードでは Extended**。`prepare_threshold`（既定 5）による自動 PREPARE も Extended 経路だけで起きる。
- **`ClientCursor`**（`psycopg.ClientCursor` または `cursor_factory=ClientCursor`）を使うと、パラメータをクライアント側でリテラルに埋め込んで Simple Query で送るので、パラメータ付きでも M1 で動く。
- 型の問い合わせは `TypeInfo.fetch()` を明示的に呼んだときだけ。未知 OID は str として読む。
- ParameterStatus の依存先: `client_encoding`（コーデック）、`DateStyle`、`IntervalStyle`、`TimeZone`（日時型の解釈）。

### 4.3 pgJDBC（42.7.x 系 master）
- 既定は `preferQueryMode=extended`（`PGProperty.java`）。**`Statement.execute(String)` でも Parse/Bind/Execute（無名 statement）を使う**。`preferQueryMode=simple` を指定すると `'Q'` を使い、PreparedStatement のパラメータは `('123'::int4)` の形でリテラルに展開する（`SimpleParameterList.java` L188）。そのため **yuzhu の構文解析器は `'...'::型名` とその括弧付き形式を受け付ける必要がある**。
- 起動直後に `runInitialQueries` を実行する（`ConnectionFactoryImpl.java` L1119-）。
  - server_version が 12 未満なら `SET extra_float_digits = 3`（9.0 未満なら `= 2`）。**12 以上を報告すれば送られない。**
  - 既定では `assumeMinServerVersion` が未設定なので application_name は起動パケットに入らない。その代わり、**サーバが 9.0 以上なら `SET application_name = 'PostgreSQL JDBC Driver'` を送る**。→ **M1 でも `SET application_name = '...'` を受理する必要がある**（`assumeMinServerVersion=9.0` 以上を指定すれば起動パラメータで渡されるので、この SET は来ない）。
  - これは preferQueryMode に従った経路で送られる（既定なら Extended）。
- ParameterStatus の厳格な検査（`QueryExecutorImpl.receiveParameterStatus`）: client_encoding=UTF8、DateStyle が ISO で始まる、standard_conforming_strings と integer_datetimes が on/off のいずれか。
- 必要時に送るクエリ: `getTransactionIsolation()` → `SHOW TRANSACTION ISOLATION LEVEL`、`getCatalog()` → `select current_catalog`、`getSchema()` → `select current_schema()`、`setReadOnly` → `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY`。型情報は `TypeInfoCache` が `pg_catalog.pg_type` を遅延して問い合わせる（getObject、DatabaseMetaData、配列など）。DBeaver などの GUI ツールは接続直後にこれらを大量に呼ぶ。
- `isValid()` は空のクエリを送る（EmptyQueryResponse が必要）。
- 結論: **M1 では `preferQueryMode=simple` + `SET application_name` の受理 + server_version 12 以上の報告で、基本的な Statement/PreparedStatement が動く見込み**。既定設定では Extended Query が必要（M2）。

### 4.4 node-postgres（pg 8.x）
- 起動後のクエリは**ない**（`client.js`。ReadyForQuery で connect が完了する）。ParameterStatus は特に検査しない。
- `client.query(text)` を**値なし、name なし、rows 指定なし**で呼ぶと Simple Query。`values` を渡す、`name` を付ける、`rows` を指定する、`queryMode: 'extended'` を指定すると Extended（`query.js` の `requiresPreparation`）。pg-cursor や pg-query-stream も Extended。
- 型変換は pg-types による OID→パーサの対応表で、未知の OID は文字列のまま。pg_type は問い合わせない。
- 結論: 値なしの `client.query('...')` は M1 で動く。パラメータ付きは M2。

### 4.5 まとめ表

| ドライバ | M1（Simple のみ）で動く使い方 | Extended が必要な使い方 | 接続時に追加で送ってくるもの |
|---|---|---|---|
| psql | すべて（psql は Simple のみ。ただし `\bind` と `\parse` 系は Extended） | `\bind`、`\bind_named`、`\parse`（16/17） | なし |
| tokio-postgres | `simple_query`, `batch_execute` | `query`, `execute`, `prepare`。さらに未知 OID で pg_type を引く | なし |
| psycopg3 | パラメータなしの `execute`、`ClientCursor`、autocommit 有無どちらも可（BEGIN/COMMIT は Simple） | パラメータ付きの `execute`、`executemany`、binary、pipeline | なし（最初のクエリ前に BEGIN） |
| pgJDBC | `preferQueryMode=simple` | 既定（extended） | `SET application_name = '...'`、server_version が 12 未満なら `SET extra_float_digits` |
| node-postgres | 値なしの `query(text)` | values / name / rows 指定、cursor | なし |

---

## 5. yuzhu M1 推奨仕様（最小セット）

### 5.1 実装するプロトコルメッセージ

**受信（クライアント → サーバ）**
- 型バイトなしの最初のパケット: SSLRequest（`'N'` を返してループ）、GSSENCRequest（`'N'` を返してループ）、StartupMessage v3.0（v3.x の minor>0 には NegotiateProtocolVersion を返す）、CancelRequest（照合して閉じる）
- 型付きメッセージ: `Q` Query、`X` Terminate
- それ以外の型（`P` `B` `D` `E` `S` `H` `C` `F` `d` `c` `f` `p`）を受けたら、`ErrorResponse(08P01 / 0A000 "extended query protocol not supported yet")` を返す。Extended Query は Sync までの各メッセージを読み飛ばす作法があるので、M1 では「ErrorResponse を返して接続を閉じる」が安全で単純。
- 長さの上限チェック（PostgreSQL は起動パケット 10000 バイト、通常メッセージ 1GB 程度）を入れる。

**送信（サーバ → クライアント）**
- `R` AuthenticationOk（Int32 0）
- `S` ParameterStatus
- `K` BackendKeyData（pid は 1 以上の連番、secret は乱数の Int32）
- `Z` ReadyForQuery（`I`/`T`/`E`）
- `T` RowDescription（テーブル OID と列番号は 0 でよい、フォーマットは 0）
- `D` DataRow（テキスト）
- `C` CommandComplete（§2.4 のタグ形式を厳守）
- `I` EmptyQueryResponse
- `E` ErrorResponse（S, V, C, M を必ず付け、P は可能なら付ける）
- `N` NoticeResponse（WARNING 25P01/25001 など）
- `v` NegotiateProtocolVersion（3.2 要求への対応）
- 起動中のエラーは S=FATAL で送ってから切断する

### 5.2 起動時に送る ParameterStatus（推奨値）
```
server_version               = 16.0          (または 17.0。§3.3 のカタログ方針と合わせる。12 以上が必須級)
server_encoding              = UTF8
client_encoding              = UTF8          (クライアント指定を受理し、その値を返す。内部は UTF8)
DateStyle                    = ISO, MDY      (pgJDBC が送ってくる "ISO" は "ISO, MDY" に正規化)
IntervalStyle                = postgres
TimeZone                     = UTC           (クライアント指定があればその値)
integer_datetimes            = on
standard_conforming_strings  = on
is_superuser                 = on
session_authorization        = <user>
application_name             = <起動パラメータの値 or "">
default_transaction_read_only = off
in_hot_standby               = off
```
- server_version の選び方: libpq は `"16.0 (yuzhu 0.1.0)"` のように後ろに文字列が付いていても先頭の数値を読める（`sscanf "%d.%d.%d"`）。psql 17 クライアントで server が 16 なら `psql (17.x, server 16.0)` と表示されるだけで、警告は出ない（警告は「サーバのほうが新しい」か「9.2 未満」のときだけ）。将来 \d に対応するときに参照される列が少ないほうが楽なので、**最初は 16.0 を推奨する**（17 にすると \l で `datlocale` が必要になる程度の差で、大差はない）。

### 5.3 SQL レベルで最低限受理すべきもの

| 文 | M1 での扱い |
|---|---|
| `CREATE TABLE` / `INSERT` / `SELECT` | 本体の目標。タグは `CREATE TABLE` / `INSERT 0 n` / `SELECT n` |
| `BEGIN` / `START TRANSACTION` / `COMMIT` / `END` / `ROLLBACK` / `ABORT` | **必須**（psycopg3 の既定、psql の AUTOCOMMIT off）。ReadyForQuery の状態を I/T/E で正しく遷移させる。失敗状態で COMMIT したら `ROLLBACK` タグを返す。失敗状態でのその他の文は 25P02 |
| `SET name = value` / `SET name TO value` / `SET SESSION ...` / `SET LOCAL ...` / `RESET name` | **必須**（pgJDBC の `SET application_name`、`SET extra_float_digits`）。未知の GUC も受理して保存する（PG は未知の名前を 42704 で拒否するが、互換性を優先して寛容にしてよい）。報告対象の値を変えたら、ReadyForQuery の前に ParameterStatus を送る |
| `SHOW name` / `SHOW ALL` | 推奨。1 列（text、列名は変数名）、タグは `SHOW`。`SHOW TRANSACTION ISOLATION LEVEL` → `read committed`、`SHOW server_version`、`SHOW client_encoding` など |
| 空クエリ / `;` だけ | EmptyQueryResponse（pgJDBC の isValid） |
| `SELECT version()` | 推奨。`PostgreSQL 16.0 (yuzhu 0.1.0) on x86_64-pc-linux-gnu, compiled by rustc ...` のように **"PostgreSQL X.Y" で始める**（バージョン文字列を正規表現で読むツールがある） |
| `current_database()`, `current_user`, `session_user`, `current_schema()`, `current_catalog`, `current_setting(text)`, `pg_backend_pid()` | 推奨（JDBC/GUI ツール、ORM が多用する） |
| `'...'::type` のキャスト | pgJDBC の simple モード、psql 系 SQL のために推奨 |
| `SET SESSION CHARACTERISTICS AS TRANSACTION ...` | 受理して無視（pgJDBC の setReadOnly / setTransactionIsolation） |

### 5.4 カタログスタブ
- **M1 では不要**（psql も 4 ドライバも、Simple モードでは接続時にカタログを引かない）。
- M2 で `\dt` と `\l` に対応する: `pg_catalog.pg_class`、`pg_namespace`、`pg_database`、`pg_roles`/`pg_authid`、`pg_type`（tokio-postgres の `query` で未知 OID を引くためにも必要）を仮想テーブルにし、`pg_get_userbyid`、`pg_table_is_visible`、`pg_encoding_to_char`、`array_to_string`、`format_type` を関数として用意する。正規表現演算子 `~`/`!~` と `OPERATOR(pg_catalog.~)` 構文、`COLLATE pg_catalog.default` も必要。
- 名前解決として `pg_catalog.` 修飾を受け付け、search_path の既定を `"$user", public` にする。`public` スキーマは最初から作っておく。

### 5.5 M1 時点で各クライアントが動く範囲（見込み）
- **psql 16/17**: 接続、任意の SQL、トランザクション、`\timing`、`\x`、`\i`、`\set`、`\echo` が動く。`\d` 系は「relation "pg_catalog.pg_class" does not exist」のようなエラーになる（M2 で対応）。
- **tokio-postgres**: `NoTls` で接続し、`simple_query` / `batch_execute` が動く。`query`/`execute` は不可。
- **psycopg3**: パラメータなしの `execute`、`ClientCursor`、`commit()`/`rollback()`、autocommit=True/False が動く。サーバ側バインドは不可。
- **pgJDBC**: `?preferQueryMode=simple`（必要なら `&assumeMinServerVersion=16`）で Statement/PreparedStatement の基本操作が動く。既定の extended は不可。sslmode=prefer の既定のままで接続できる（`N` で平文にフォールバックする）。
- **node-postgres**: `ssl` 未設定で、値なしの `client.query()` が動く。

### 5.6 落とし穴チェックリスト
1. SSLRequest に `N` を返した後、**同じ接続で** StartupMessage を読む（`N` を返して切断してはいけない）。GSSENCRequest も同じ。
2. 1 つの Query に対して ReadyForQuery は**ちょうど 1 回**。エラー時も送る。
3. INSERT のタグは `INSERT 0 n`。`INSERT n` だと tokio-postgres は最後の単語を読むので偶然動くが、libpq の `PQcmdTuples` などは INSERT の 2 番目の数値を読むので壊れる。
4. ErrorResponse には S/C/M を必ず付ける（tokio-postgres が S/C/M のどれかが欠けると解析エラーにする）。
5. `standard_conforming_strings=on` を報告するなら、文字列リテラル中の `\` を通常の文字として扱う（`E'...'` だけがエスケープを解釈する）。
6. pgJDBC 向けに、DateStyle は必ず "ISO" で始め、client_encoding は必ず "UTF8" を返す。
7. トランザクション状態バイトを正確に返す（psycopg の BEGIN 自動送信、psql のプロンプト）。
8. 複数文の Query は暗黙トランザクションとして扱い、途中でエラーが出たら全体をロールバックする。
9. NUL 終端文字列（CString）の中に NUL が含まれる入力は 08P01 として拒否する。
