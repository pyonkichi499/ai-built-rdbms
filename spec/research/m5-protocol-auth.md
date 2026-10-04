# yuzhu M5 調査レポート: Extended Query プロトコル、SCRAM-SHA-256 認証、ロール、CREATE / DROP DATABASE

M5（実用化）のうち、**ドライバの既定モードで動くこと**と**パスワード認証**、**複数データベース**に関わる部分を調べたレポートです。対象は次の 4 つです。

1. Extended Query プロトコル（Parse / Bind / Describe / Execute / Close / Sync / Flush）
2. 各ドライバが Extended Query で何を期待するか（tokio-postgres、psycopg 3、pgJDBC、node-postgres）
3. SCRAM-SHA-256 認証、CREATE ROLE / ALTER ROLE PASSWORD、pg_hba 相当の最小設定
4. CREATE DATABASE / DROP DATABASE と、別の DB への接続

前提（既存の調査・設計）: `spec/design/m1.md`（プロトコルの最小セット、Extended のメッセージは 0A000 で Sync まで読み捨て）、`spec/research/research-pg-protocol.md`（ドライバの Simple / Extended の使い分け）、`spec/research/m2-catalog.md`（pg_database / pg_authid、`base/<dboid>/` 構成、テンプレートのコピー）、`spec/research/m3-*.md`（MVCC、WAL、トランザクションの意味論）。

**根拠の表記**: 【確認】= 本調査で PostgreSQL 17 の文書またはソースを読んで確かめた。【記憶】= 筆者の知識による。ソースの行までは確かめていない。実装前に本物の PostgreSQL 17 で確かめること（テストケースにする）。【推奨】= yuzhu としての提案。

---

## 0. 主な出典

| 内容 | URL |
|---|---|
| メッセージの流れ（Extended Query、パイプライン、SASL） | <https://www.postgresql.org/docs/17/protocol-flow.html> 【確認】 |
| メッセージの形式 | <https://www.postgresql.org/docs/17/protocol-message-formats.html> |
| SASL 認証（SCRAM-SHA-256） | <https://www.postgresql.org/docs/17/sasl-authentication.html> 【確認】 |
| パスワード認証の方式 | <https://www.postgresql.org/docs/17/auth-password.html> |
| pg_hba.conf | <https://www.postgresql.org/docs/17/auth-pg-hba-conf.html> |
| CREATE ROLE / ALTER ROLE | <https://www.postgresql.org/docs/17/sql-createrole.html> / <https://www.postgresql.org/docs/17/sql-alterrole.html> |
| CREATE DATABASE / DROP DATABASE | <https://www.postgresql.org/docs/17/sql-createdatabase.html> / <https://www.postgresql.org/docs/17/sql-dropdatabase.html> |
| PREPARE / EXECUTE / DEALLOCATE | <https://www.postgresql.org/docs/17/sql-prepare.html> |
| Extended Query の実装（`exec_parse_message`、`exec_bind_message`、`exec_execute_message`、`exec_describe_*`、Sync の処理） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/tcop/postgres.c> |
| パラメータ型の推論（`variable_paramref_hook`、`variable_coerce_param_hook`、`check_variable_parameters`） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/parser/parse_param.c> |
| ポータル | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/tcop/pquery.c>、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/mmgr/portalmem.c> |
| プランキャッシュ | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/utils/cache/plancache.c> |
| SCRAM（サーバ側） | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/libpq/auth-scram.c> 【確認: 秘密情報の形式、`mock_scram_secret`、既定反復回数 4096】 |
| SCRAM 共通処理 | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/common/scram-common.c>、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/include/common/scram-common.h> |
| SASLprep | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/common/saslprep.c> |
| 認証の振り分け | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/libpq/auth.c>、<https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/libpq/hba.c> |
| ロールの DDL | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/commands/user.c> |
| データベースの DDL | <https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/commands/dbcommands.c> |
| RFC | RFC 5802（SCRAM）<https://www.rfc-editor.org/rfc/rfc5802>、RFC 7677（SCRAM-SHA-256）<https://www.rfc-editor.org/rfc/rfc7677>、RFC 4013（SASLprep）<https://www.rfc-editor.org/rfc/rfc4013> |
| ドライバ | tokio-postgres <https://github.com/sfackler/rust-postgres>（`tokio-postgres/src/prepare.rs`、`query.rs`、`postgres-protocol/src/authentication/sasl.rs`）、psycopg 3 <https://github.com/psycopg/psycopg>、pgJDBC <https://github.com/pgjdbc/pgjdbc>（`QueryExecutorImpl.java`、`TypeInfoCache.java`）、node-postgres <https://github.com/brianc/node-postgres>（`packages/pg/lib/query.js`） |

---

## 1. 結論（推奨の要約）

- **Extended Query は M5 の最優先**。pgJDBC（既定 `preferQueryMode=extended`）、tokio-postgres の `query` / `execute`、psycopg 3 のパラメータ付き `execute`、node-postgres の `values` 付き `query` は、すべて Extended Query を使う。これが無いと、ほとんどのアプリが動かない。
- Extended Query の中心は**パラメータ型の推論**（`$n` の型を文脈から決める）と、**エラー後に Sync まで読み捨てる状態機械**と、**暗黙のトランザクション**（Sync で commit）の 3 つ。メッセージの読み書き自体は難しくない。
- **バイナリ形式は必須**。tokio-postgres はパラメータも結果もバイナリで送受信する【記憶】。M1 の型（bool、int2/4/8、float4/8、text、varchar、name、oid）のバイナリ形式は単純（ビッグエンディアンの固定長、または UTF-8 のバイト列）。
- **型推論の結果が PG と違うと、tokio-postgres が壊れる**。tokio-postgres は ParameterDescription の OID を見て、Rust の値をその型のバイナリにする。`WHERE id = $1`（id が int8）で PG は int8 と答えるので、利用者は `i64` を渡す。yuzhu が int4 と答えると、利用者のコードは「PG では動くが yuzhu では動かない」ことになる。
- SCRAM-SHA-256 は **RustCrypto の `sha2` + `hmac` + `pbkdf2`**（暗号用途なので CLAUDE.md の許容範囲）と、`base64`、乱数に `getrandom`、SASLprep に `stringprep` を使う。チャネルバインディング（`-PLUS`）は TLS が入る M6 以降。
- 秘密情報は PG と**同じ形式** `SCRAM-SHA-256$<iter>:<salt>$<StoredKey>:<ServerKey>` で `pg_authid.rolpassword` に保存する。psql の `\password` はクライアント側でこの形式に変換して送ってくるので、そのまま受け入れる。
- 認証設定は **PG の pg_hba.conf と同じ書式の部分集合**（`host` 行、方式は `trust` / `scram-sha-256` / `password` / `reject`）にする。
- CREATE DATABASE は **`STRATEGY = FILE_COPY` 相当**（チェックポイント → `base/<src>/` を `base/<new>/` にコピー → WAL 記録 → チェックポイント）。M2 の initdb で作るコピー処理を再利用する。
- 共通テストは、sqllogictest-rs の `--engine postgres-extended` で同じスイートを Extended Query でも流す（`research-slt.md` 参照）。プロトコル固有の挙動（エラー後の読み捨て、PortalSuspended など）は、Rust の結合テストで**生のメッセージ**を送って確かめる。

---

## 2. Extended Query プロトコル

### 2.1 メッセージ一覧

フロントエンド → バックエンド（型バイト、内容）【記憶: 形式は protocol-message-formats.html の内容】

| 型 | 名前 | 内容 |
|---|---|---|
| `P` | Parse | 文の名前（cstring、空 = 無名）、クエリ文字列（cstring）、int16 パラメータ型の数 N、int32 × N のパラメータ型 OID（0 = 未指定） |
| `B` | Bind | ポータル名（cstring）、文の名前（cstring）、int16 パラメータ形式コードの数 C、int16 × C、int16 パラメータ値の数 M、各値（int32 長さ、-1 = NULL、続いてバイト列）、int16 結果形式コードの数 R、int16 × R |
| `D` | Describe | `S`（文）または `P`（ポータル）、名前（cstring） |
| `E` | Execute | ポータル名（cstring）、int32 最大行数（0 = 全部） |
| `C` | Close | `S` または `P`、名前（cstring） |
| `S` | Sync | なし |
| `H` | Flush | なし |

形式コード（Bind の C と R）: **数が 0 なら全部テキスト、1 なら全部にその 1 つを適用、それ以外は 1 つずつ**。0 = テキスト、1 = バイナリ。【記憶】

バックエンド → フロントエンド

| 型 | 名前 | 送る場面 |
|---|---|---|
| `1` | ParseComplete | Parse の成功 |
| `2` | BindComplete | Bind の成功 |
| `3` | CloseComplete | Close（存在しない名前でも成功扱い）【確認】 |
| `t` | ParameterDescription | Describe（文）の最初。int16 数、int32 × 数の型 OID |
| `T` | RowDescription | Describe（文）の 2 つ目、または Describe（ポータル）。文の Describe では形式コードはすべて 0【確認】 |
| `n` | NoData | 行を返さない文・ポータルの Describe【確認】 |
| `D` | DataRow | Execute の結果行 |
| `C` | CommandComplete | Execute の完了 |
| `I` | EmptyQueryResponse | 空のクエリ文字列から作ったポータルの Execute【確認】 |
| `s` | PortalSuspended | 最大行数に達して Execute が途中で止まった【確認】 |
| `E` | ErrorResponse | |
| `Z` | ReadyForQuery | **Sync への応答だけ**（Extended Query の各メッセージには返さない） |

- Execute の 1 回は、CommandComplete、EmptyQueryResponse、ErrorResponse、PortalSuspended の**ちょうど 1 つ**で終わる【確認】。
- Execute は RowDescription を送らない。クライアントは事前に Describe（ポータル）で受け取る（pgJDBC、node-postgres は Bind の後に Describe（ポータル）を送る。tokio-postgres は prepare 時の Describe（文）の結果を使う）【記憶】。
- Parse のクエリ文字列に**文が 2 つ以上あるとエラー**（42601 `cannot insert multiple commands into a prepared statement`）【確認: 文書は「syntax error」とだけ書く。文言は記憶】。

### 2.2 名前付き / 無名の文とポータルの寿命【確認】

| 対象 | 寿命 | 同じ名前で作り直すとき |
|---|---|---|
| 無名の文 | 次に無名の文を指定した Parse が来るまで。**Simple Query（`Q`）でも破棄される** | 自動で置き換え |
| 名前付きの文 | セッションの終わりまで（Close か DEALLOCATE で消す） | 先に Close が必要（しないと 42P05 `prepared statement "x" already exists`【記憶】） |
| 無名のポータル | トランザクションの終わりまで。または次に無名ポータルを指定した Bind まで | 自動で置き換え |
| 名前付きのポータル | トランザクションの終わりまで（Close で消す） | 先に Close が必要（42P03 `portal "x" already exists`【記憶】） |

- 存在しない文: 26000 `prepared statement "x" does not exist`。存在しないポータル: 34000 `portal "x" does not exist`【記憶】。無名の場合は名前が空文字列になる（`unnamed prepared statement does not exist` という文言が別にある【記憶】）。
- 名前付きの文は、SQL の `PREPARE` で作ったものと**同じ名前空間**にある（`pg_prepared_statements` に両方が出る）。`DEALLOCATE name` / `DEALLOCATE ALL` / `DISCARD ALL` で消える【記憶】。
- **ポータルはトランザクションの終わりで消える**ので、暗黙のトランザクション（下記 2.4）では Sync の後にポータルは残らない。

### 2.3 処理の流れとエラー時の読み捨て

```
通常:   Parse → Bind → Describe(P) → Execute → Sync
応答:   1     → 2    → T / n       → D... C  → Z
```

- **エラーが起きたら ErrorResponse を返し、Sync が来るまでメッセージを読み捨てる。Sync で ReadyForQuery を返して通常に戻る**【確認】。読み捨て中に来た Flush も無視する【記憶】。Terminate（`X`）は読み捨て中でも処理する【記憶】。
- これにより、パイプライン（Sync を挟まずに複数の文を送る）では、前の文が失敗すると後の文が自動でスキップされる【確認】。クライアントは「送った Sync の数」だけ ReadyForQuery を数えて完了を判断する【確認】。
- Simple Query（`Q`）は読み捨て中には来ない前提（来た場合も読み捨てる）【記憶: PG の `ignore_till_sync` は `Q` も含めて捨てる】。
- **Flush** は「出力バッファにあるものを送る」だけ。Sync 以外のメッセージの後で結果を見たいクライアントは Flush を送る【確認】。yuzhu の接続は `BufWriter` で書くので、**Flush と Sync（ReadyForQuery の後）でだけ flush する**。それ以外で flush しない（PG も同じ【記憶】）。
- ParameterStatus や NoticeResponse は、Extended Query の途中でも送ってよい（`SET` を Execute した場合など）。

**状態機械（yuzhu の推奨実装）**

```rust
enum ExtState { Normal, SkipTillSync }

loop {
    let msg = read_message()?;
    if state == SkipTillSync && !matches!(msg, Sync | Terminate) { continue; }
    let r = match msg {
        Parse(..) | Bind(..) | Describe(..) | Execute(..) | Close(..) => session.handle_ext(msg, &mut sink),
        Flush => { sink.flush()?; Ok(()) }
        Sync => { state = Normal; session.sync(&mut sink)?;  // 暗黙のトランザクションを閉じる
                  sink.ready_for_query(session.tx_status())?; sink.flush()?; Ok(()) }
        Query(sql) => session.execute_simple(sql, &mut sink),
        ...
    };
    if let Err(e) = r { sink.error(&e)?; if msg.is_extended() { state = SkipTillSync; } }
}
```

### 2.4 トランザクション状態との関係

PG の挙動（`postgres.c` の `start_xact_command` / `finish_xact_command` と Sync の処理）【確認: 文書の記述。細部は記憶】

1. **BEGIN の外**では、最初の Extended メッセージで暗黙のトランザクションが始まり、**Sync で閉じる**（エラーが無ければ commit、あれば rollback）【確認】。Sync を挟まずに送った複数の Execute は、1 つの暗黙のトランザクションになる。
2. **BEGIN の中**では、Sync はトランザクションを閉じない【確認】。ReadyForQuery は `T`（またはエラー後なら `E`）。
3. 暗黙のトランザクションの途中で BEGIN を Execute すると、そこから明示的なブロックになる。COMMIT / ROLLBACK を Execute すると、その時点で閉じる（Sync を待たない）【記憶】。
4. **失敗したブロック（`E` 状態）では**、ROLLBACK / COMMIT / ABORT / `ROLLBACK TO SAVEPOINT` 以外の Parse・Bind・Execute は 25P02 `current transaction is aborted, commands ignored until end of transaction block` になる【記憶: `exec_parse_message` などの `IsAbortedTransactionBlockState()` の検査】。Describe（文）も、行を返す文なら同じエラーになる【記憶】。
5. CREATE DATABASE のような「ブロック内で実行できない文」は、パイプラインの先頭でないと失敗し、成功すると即座に commit する【確認】。yuzhu では「暗黙のトランザクションの中で、それまでに別の文を実行していたらエラー（25001）」とする。
6. Simple Query の `Q` は、それ自体が暗黙のトランザクションを持つ（M1 と同じ）。`Q` が来たら無名の文と無名のポータルを破棄する。

**MVCC との関係（M3 の設計とつなぐ点）**【推奨】

- Read Committed のスナップショットは**文ごと**。Extended Query では「ポータルの実行開始時（最初の Execute、または PG と同じく Bind 直後の PortalStart）」に取る。PG はポータルの開始時にスナップショットを取り、**同じポータルに対する 2 回目以降の Execute は同じスナップショットで続きを返す**【記憶】。
- Repeatable Read ではトランザクションのスナップショットを使うので、違いは生じない。
- ポータルが生きている間（PortalSuspended の後）は、スナップショットを「使用中」として登録しておく（VACUUM の削除境界 `xmin horizon` に効く）。トランザクションの終わりで必ずポータルを破棄して登録を外す。
- M5 で複数ライターになると、ポータルの実行中に同じテーブルへ別のセッションが書き込むことがある。スナップショットで隔離されるので問題ない。同じセッションの中で「ポータルを途中まで読んで、別の文で UPDATE して、続きを読む」場合は、PG ではポータルのスナップショットの `curcid`（コマンド ID）より後の変更は見えない【記憶】。yuzhu も M3 の command id 方式で同じになるはず。

### 2.5 パラメータ型の推論

PG の仕組み（`parse_param.c`）【記憶。ただし関数名は確認済みの文書・ソース構成による】

- Parse で型を指定した `$n` は、その型の値（Param ノード）として扱う。型 0 または配列が短くて指定が無い `$n` は **unknown** として扱う。
- unknown のパラメータは、**文字列リテラル（unknown 型）と同じ規則**で演算子・関数の解決に参加する。そして、型の強制変換（coerce）が起きた時点で、そのパラメータの型を**変換先の型に確定する**（`variable_coerce_param_hook`）。キャスト式を挟まず、Param の型そのものを書き換える。
- 同じ `$n` に別々の型が推論されたら 42P08 `inconsistent types deduced for parameter $1`【記憶】。
- 解析が終わっても unknown のままなら 42P18 `could not determine data type of parameter $1`【記憶】（`check_variable_parameters`）。
- ParameterDescription には確定した型を返す。

代表例（**すべて PG17 で要確認**。テストケースにする）

| SQL | `$1` の型 | 理由 |
|---|---|---|
| `SELECT * FROM t WHERE id = $1`（id は int8） | int8 | `int8 = unknown` → unknown を int8 とみなして `int8 = int8` を選ぶ |
| `SELECT $1 + 1` | int4 | `unknown + int4` → int4 |
| `INSERT INTO t (c) VALUES ($1)`（c は varchar(10)） | varchar（typmod は付かない） | 代入の文脈で列の型に変換 |
| `UPDATE t SET c = $1 WHERE id = $2` | c の型、id の型 | 同上 |
| `SELECT $1::int4` | int4 | 明示的なキャストでも Param の型が int4 に確定する |
| `SELECT $1` | text | PG10 以降、出力列に残った unknown は text に解決される【記憶】 |
| `SELECT $1 IS NULL` | エラー 42P18 の可能性 | NullTest は引数を変換しない【記憶・要確認】 |
| `LIMIT $1` / `OFFSET $1` | int8 | |
| `SELECT $1 = $2` | text, text | unknown 同士は文字列カテゴリが優先される【要確認】 |
| `WHERE c LIKE $1` | text | |
| `WHERE id IN ($1, $2)` | 列の型 | |
| `SELECT $2`（`$1` を使わない） | `$1` もエラー 42P18 になるか、要確認 | 歯抜けの番号の扱い |

yuzhu の実装【推奨】

- 字句解析器に `$n` トークン（`Param(u16)`）を追加。AST に `Expr::Param { index, span }`。Simple Query で `$1` が来たら 42P02 `there is no parameter $1`【記憶】。
- アナライザに `ParamTypes { declared: Vec<Oid>, inferred: Vec<Option<Oid>> }` を渡す（可変の参照）。
  - `$n` を見たら、宣言があればその型の `Bound::Param`、無ければ unknown 型の `Bound::Param` を作る。
  - M1 の `coerce.rs` で「unknown のリテラルを T に変換する」場所に、「unknown の Param なら型を T に確定して記録する」分岐を足す。
  - 解析の最後に、出力列に残った unknown の Param を text にし、それ以外で unknown が残ったら 42P18。
- 型推論は PG と完全に同じでなくてもアプリは大抵動くが、**tokio-postgres はバイナリで型を厳密に検査する**ので、上の表は全部テストにする。

### 2.6 バイナリ形式（M1 の型）

PG の各型の `*send` / `*recv` 関数と同じにする【記憶。pqformat.c はネットワークバイト順】。

| 型 | OID | バイナリ表現 |
|---|---|---|
| bool | 16 | 1 バイト（0 / 1） |
| int2 / int4 / int8 | 21 / 23 / 20 | ビッグエンディアンの 2 / 4 / 8 バイト |
| float4 / float8 | 700 / 701 | IEEE 754 のビットをビッグエンディアンで 4 / 8 バイト（NaN、Infinity もそのまま） |
| text / varchar / name / unknown | 25 / 1043 / 19 / 705 | UTF-8 のバイト列（長さは外側の int32）。受信時は UTF-8 として正しいか検査する（不正なら 22021 `invalid byte sequence for encoding "UTF8"`）。varchar(n) の長さ検査は代入時の変換で行う |
| oid | 26 | ビッグエンディアンの 4 バイト（符号なし） |

- 受信したバイナリの長さが合わない、または余りがあるときは 22P03 `incorrect binary data format in bind parameter 1`【記憶】。
- テキスト形式のパラメータは、M1 の `types::io::input_text` をそのまま使う（`'123'::int4` と同じ処理）。
- M5 で追加する型のバイナリ形式（参考）【記憶】: `date` = 2000-01-01 からの日数（int32）、`timestamp` / `timestamptz` = 2000-01-01 00:00 UTC からのマイクロ秒（int64）、`bytea` = 生のバイト列、`uuid` = 16 バイト、`numeric` = int16 ndigits / weight / sign / dscale と、基数 10000 の int16 の並び（最も手間がかかる。M）。
- **float の出力の違いに注意**: テキスト形式の結果は M1 と同じ `output_text`。sqllogictest の extended エンジンで float8 `1e20` が `100000000000000000000` になるのは、tokio-postgres がバイナリで受け取ってクライアント側で整形するため（`research-slt.md` で検証済み）。yuzhu の不具合ではない。

### 2.7 行数制限と PortalSuspended

- Execute の最大行数が 0 でなく、まだ行が残っているのにその数に達したら、DataRow をその数だけ送って **PortalSuspended** を返す【確認】。同じポータルへの次の Execute で続きを返す。全部返したら CommandComplete（`SELECT n`）。
- `SELECT n` の n は、その Execute で返した行数か、ポータル全体の累計か【要確認。PG ではポータルの `portalPos` から計算した累計に見えるが未確認】。
- 行数制限を実際に使うのは、**pgJDBC で `setFetchSize(n)` かつ autocommit が off**のとき（autocommit が on だと Sync でポータルが消えるので、pgJDBC は行数制限を使わない）【記憶】。psycopg 3 のサーバ側カーソル（名前付きカーソル）は `DECLARE CURSOR` + `FETCH` を使う（Simple / Extended どちらでも）【記憶】。node-postgres の pg-cursor も Execute の行数制限を使う【記憶】。
- 実装上、M1 の Executor は Volcano 型（`next()` で 1 行ずつ）なので、**ポータルが Executor の木を持ったまま中断できる**。`Portal { plan_root: Box<dyn Executor>, snapshot, row_desc, result_formats, done: bool }`。
- INSERT / UPDATE / DELETE ... RETURNING のように副作用がある文は、PG では最初の Execute で**全部実行して結果をためておき**、行数制限に従って小分けに返す（`PORTAL_ONE_RETURNING`）【記憶】。yuzhu も同じにする（途中で止めると、未実行の行が残ったまま別の文が走ってしまう）。
- 中断したポータルに Simple Query の `Q` が来ても、名前付きのポータルは BEGIN ブロック内なら残る。

### 2.8 準備済み文のキャッシュと DDL

- **最小の実装**: 名前付きの文には「SQL テキスト、宣言されたパラメータ型、解析済みの Bound ツリー（または再解析用のテキストだけ）、結果の列記述、カタログの世代番号」を保存する。Bind のたびに、カタログの世代番号が変わっていれば**解析し直す**（`m2-catalog.md` §6 の generation 方式）。
- 再解析で**結果の列の型が変わったら** 0A000 `cached plan must not change result type`【記憶】（クライアントは prepare 時の RowDescription でデコードしているため）。
- PG は「最初の 5 回はパラメータ値を使ったカスタムプラン、その後は汎用プランと比べる」（`plan_cache_mode`）。yuzhu の M4 のルールベース最適化ではパラメータ値で計画が変わることはほぼ無いので、**Bind ごとに計画し直すか、汎用プランを 1 つだけ持つ**かのどちらかでよい。【推奨】最初は Bind ごとに planner を通す（M4 の planner は軽い）。必要になったら汎用プランを保存する。
- 無名の文は、PG では 1 回しか使わない前提でパラメータ値に合わせて計画する（M4 のインデックス選択で `$1` の値を見たい場合に効く）。

### 2.9 SQL レベルの PREPARE / EXECUTE / DEALLOCATE / DISCARD

- `PREPARE name [(type, ...)] AS stmt`、`EXECUTE name [(args)]`、`DEALLOCATE [PREPARE] {name | ALL}`、`DISCARD ALL`（文、ポータル、一時的な設定をすべて捨てる。ブロック内では 25001）【記憶】。
- 名前空間はプロトコルの名前付き文と共通。接続プール（pgbouncer の transaction モードなど）は `DISCARD ALL` を送る。工数は S（Extended Query の部品を流用できる）。
- `pg_prepared_statements` ビュー（`name, statement, prepare_time, parameter_types, result_types, from_sql, generic_plans, custom_plans`）は M6 の pg_catalog 拡充でよい。

### 2.10 Extended Query 関連の SQLSTATE（【記憶】。実装時に PG17 で確認）

| SQLSTATE | 名前 | 文言の例 |
|---|---|---|
| 08P01 | protocol_violation | `bind message supplies 1 parameters, but prepared statement "" requires 2`、`bind message has 2 result formats but query has 1 columns`、不正なメッセージ長 |
| 26000 | invalid_sql_statement_name | `prepared statement "s1" does not exist` |
| 34000 | invalid_cursor_name | `portal "p1" does not exist` |
| 42P05 | duplicate_prepared_statement | `prepared statement "s1" already exists` |
| 42P03 | duplicate_cursor | `portal "p1" already exists`（要確認） |
| 42P02 | undefined_parameter | `there is no parameter $1` |
| 42P08 | ambiguous_parameter | `inconsistent types deduced for parameter $1` |
| 42P18 | indeterminate_datatype | `could not determine data type of parameter $1` |
| 22P03 | invalid_binary_representation | `incorrect binary data format in bind parameter 1` |
| 22021 | character_not_in_repertoire | `invalid byte sequence for encoding "UTF8": 0xff` |
| 0A000 | feature_not_supported | `cached plan must not change result type`、未対応の形式コード（2 以上は 08P01 の可能性あり） |
| 25P02 | in_failed_sql_transaction | 失敗したブロック内の Parse / Bind / Execute |
| 42601 | syntax_error | `cannot insert multiple commands into a prepared statement` |

### 2.11 yuzhu の設計案（M1 の境界の拡張）【推奨】

`yuzhu-core::session` に次を足す。プロトコルの解釈（メッセージの読み書き、SkipTillSync）は `yuzhu-server` に置き、文とポータルの管理は `Session` に置く（M1 の「Session は ResultSink に書く」設計を維持する）。

```rust
pub enum Format { Text, Binary }

pub struct PreparedStatement {
    pub sql: String,
    pub param_types: Vec<Oid>,          // 推論後に確定した型
    pub row_desc: Option<Vec<ColumnDesc>>, // None = NoData
    pub catalog_generation: u64,
    // 内部: 解析済みの文（再解析が必要なら作り直す）
}

impl Session {
    pub fn parse(&mut self, name: &str, sql: &str, param_types: &[Oid]) -> Result<()>;
    pub fn bind(&mut self, portal: &str, stmt: &str,
                params: &[Option<&[u8]>], param_formats: &[Format],
                result_formats: &[Format]) -> Result<()>;
    pub fn describe_statement(&self, name: &str) -> Result<(Vec<Oid>, Option<Vec<ColumnDesc>>)>;
    pub fn describe_portal(&self, name: &str) -> Result<Option<Vec<ColumnDesc>>>; // 形式コード入り
    pub fn execute(&mut self, portal: &str, max_rows: u32, sink: &mut dyn ResultSink) -> Result<ExecOutcome>;
    pub fn close_statement(&mut self, name: &str);
    pub fn close_portal(&mut self, name: &str);
    pub fn sync(&mut self) -> Result<()>;  // 暗黙のトランザクションを閉じる
}
pub enum ExecOutcome { Complete(CommandTag), Suspended, EmptyQuery }
```

- `ResultSink::row()` は、列ごとの形式（テキスト / バイナリ）を見て出力する。`types::io` に `output_binary(&Datum, SqlType) -> Vec<u8>` と `input_binary(&[u8], SqlType) -> Result<Datum>` を足す。
- `ColumnDesc` に `format: i16` を持たせる（M1 では常に 0）。
- メモリ: パラメータ値は `Bind` のメッセージ本体から借用せず、Datum に変換して Portal が持つ。

---

## 3. ドライバごとに必要なこと

### 3.1 tokio-postgres（`postgres` クレートも同じ）【記憶】

- `client.prepare(sql)`: `Parse(name="s<連番>", 型なし)` → `Describe(S)` → `Sync`。ParameterDescription と RowDescription の型 OID が**組み込みの静的な表に無ければ**、`pg_catalog.pg_type` を問い合わせる（`prepare.rs` の `TYPEINFO_QUERY`。`pg_type t LEFT JOIN pg_range r ... JOIN pg_namespace n ... WHERE t.oid = $1`、列は `typname, typtype, typelem, rngsubtype, typbasetype, nspname, typrelid`）。enum なら `pg_enum`、複合型なら `pg_attribute` も引く。問い合わせが失敗したら、`pg_range` を使わない版に切り替える（`research-pg-protocol.md` §4.1）。
- `client.query(sql, &params)`: 上の prepare を内部で行い、`Bind(無名ポータル, パラメータは全部バイナリ, 結果は全部バイナリ)` → `Execute(0)` → `Sync`。文はドロップ時に `Close(S)` + `Sync` で閉じる。
- **静的な表は PG の組み込み型をほぼすべて含む**（配列型も）。yuzhu が PG と同じ OID を使っている限り、pg_type の問い合わせは起きない。ドメインやユーザー定義の enum を作らない限り不要。
- 必要なもの: Extended Query 一式、**バイナリ形式（送受信）**、型推論の正確さ。`query_raw` / `bind` はポータルを使う。`Transaction::bind` + `query_portal(portal, max_rows)` で行数制限を使う。
- SCRAM-SHA-256 に対応（`postgres-protocol/src/authentication/sasl.rs`）。チャネルバインディングは TLS がある場合だけ使う（`channel_binding=prefer` が既定。TLS が無ければ `n,,` を送る）。

### 3.2 psycopg 3【記憶】

- パラメータ付きの `cursor.execute(sql, params)` は Extended Query（libpq の `PQsendQueryParams` 相当: 無名の Parse / Bind / Describe(P) / Execute / Sync）。
- パラメータの型 OID: Python の `int` は値の大きさに応じて int2 / int4 / int8 / numeric を選ぶ、`float` は float8、`bool` は bool、**`str` は既定で OID 0（unknown）を送り、サーバに推論させる**、`None` は OID 0 と NULL【記憶・要確認】。したがって **unknown のパラメータの推論が必須**。
- 形式: 既定はテキスト（パラメータも結果も）。`binary=True` や `%b` プレースホルダでバイナリ。
- `prepare_threshold`（既定 5）回同じ SQL を実行すると、名前付きの文（`_pg3_<n>`）を作って再利用する。`DEALLOCATE` で掃除する。
- psycopg のプレースホルダは `%s` / `%(name)s` で、クライアント側で `$1` に書き換えて送る。
- pg_type は、利用者が `TypeInfo.fetch()` や enum / 複合型の登録をしたときだけ問い合わせる（`SELECT ... FROM pg_type WHERE oid = %s::regtype` 系。`regtype` 型が必要になる）。通常は不要。
- 接続時に `SCRAM-SHA-256` を libpq 経由で処理する。

### 3.3 pgJDBC（既定 `preferQueryMode=extended`）【記憶】

- `Statement.execute(sql)` も `PreparedStatement` も Extended Query。1 回の往復で `Parse(無名) → Bind → Describe(P) → Execute → Sync` を送る。`prepareThreshold`（既定 5）回目からは名前付きの文（`S_<n>`）を使い、最初に `Parse → Describe(S) → Sync` で型を確かめることがある。
- パラメータの型: `setInt` → int4、`setLong` → int8、`setString` → **varchar（1043）**（`stringtype=unspecified` を指定すると 0）、`setNull(Types.X)` → 対応する OID、`setObject` は値の型による。varchar で送られた値が int 列と比べられると、PG では `integer = character varying` の演算子が無く 42883 になる（PG と同じ挙動になるので yuzhu も同じでよい）。
- **バイナリ形式**: `binaryTransfer=true`（既定）で、名前付きの文になった後、int2 / int4 / int8 / float4 / float8 / bool / bytea / uuid / date / timestamp などの結果をバイナリで要求する。最初の数回はテキスト。つまり **5 回目以降にバイナリが必要**（テストで 5 回以上ループさせないと見落とす）。
- `autocommit=false` のとき、最初の文の前に `BEGIN` を Parse/Bind/Execute で送る（同じ Sync の中に入れる）。`setFetchSize(n)` + autocommit off で Execute の行数制限と PortalSuspended を使う。
- `TypeInfoCache` は組み込み型の静的な表を持つ。それ以外（`getObject` で未知の OID、`DatabaseMetaData`、配列の要素型など）は `SELECT n.nspname = ANY(current_schemas(true)), n.nspname, t.typname FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid WHERE t.oid = $1` のような問い合わせを送る【記憶・要確認】。`current_schemas(boolean)` と `= ANY(array)` が必要。
- 接続直後: `SET application_name = 'PostgreSQL JDBC Driver'`（Extended）。`server_version` が古いと `SET extra_float_digits = 3`。
- SCRAM-SHA-256 に対応（`com.ongres.scram` ライブラリ）。

### 3.4 node-postgres（pg 8.x）【記憶】

- `client.query(text, values)` は `Parse(無名、型 OID なし) → Bind(テキスト形式) → Describe(P) → Execute(0) → Sync`。`name` を付けると名前付きの文を作り、2 回目以降は Parse を省く。
- パラメータはすべてテキスト、**型 OID はすべて 0**（推論が必須）。結果もテキスト（`binary: true` で全部バイナリ）。
- 型の変換は `pg-types` が OID ごとに行い、pg_type を問い合わせない。
- `pg-cursor` / `pg-query-stream` は Execute の行数制限（PortalSuspended）を使う。
- SCRAM-SHA-256 に対応（`lib/crypto/sasl.js`）。

### 3.5 まとめ表

| ドライバ | 既定で Extended を使う場面 | パラメータの型 OID | 形式 | 行数制限 | pg_type の問い合わせ |
|---|---|---|---|---|---|
| tokio-postgres | `query` / `execute` / `prepare` | 送らない（0）。サーバが推論した型に合わせてクライアントが変換 | パラメータも結果もバイナリ | `query_portal` | 未知の OID のときだけ |
| psycopg 3 | パラメータ付き `execute`、`executemany` | int 系・float・bool は指定、str は 0 | テキスト（選択でバイナリ） | 使わない（サーバ側カーソルは DECLARE / FETCH） | 利用者が要求したときだけ |
| pgJDBC | すべて | 指定（setString は varchar） | 5 回目以降は一部バイナリ | fetchSize + autocommit off | 未知の OID、メタデータ |
| node-postgres | `values` / `name` / `rows` 指定時 | 0 | テキスト | pg-cursor | なし |
| psql 17 | `\bind` / `\parse` / `\bind_named` / `\close` | 0 | テキスト | なし | なし |

---

## 4. SCRAM-SHA-256 認証

### 4.1 メッセージの流れ【確認: protocol-flow、sasl-authentication】

```
S→C  AuthenticationSASL         R, int32 10, 機構名の一覧（cstring の並び + 空 cstring）: "SCRAM-SHA-256"
C→S  SASLInitialResponse        p, 機構名(cstring), int32 長さ, client-first-message
S→C  AuthenticationSASLContinue R, int32 11, server-first-message
C→S  SASLResponse               p, client-final-message
S→C  AuthenticationSASLFinal    R, int32 12, server-final-message
S→C  AuthenticationOk           R, int32 0
（以降は M1 と同じ: ParameterStatus..., BackendKeyData, ReadyForQuery）
```

- 失敗したら ErrorResponse（FATAL）を返して接続を閉じる。
- TLS が無いので、機構の一覧は `SCRAM-SHA-256` だけを送る（`SCRAM-SHA-256-PLUS` は TLS のときだけ【確認】）。
- `p` は文脈で意味が変わる（PasswordMessage / SASLInitialResponse / SASLResponse で同じ型バイト）。接続の状態で区別する。

### 4.2 SCRAM の中身（RFC 5802 / 7677）

```
client-first-message = gs2-header client-first-message-bare
  gs2-header: "n,,"（チャネルバインディング非対応） / "y,,"（クライアントは対応だがサーバは非対応と判断） / "p=tls-server-end-point,,"
  client-first-message-bare: "n=,r=<cnonce>"   ※PG ではユーザー名は空で、無視される【確認】
server-first-message = "r=<cnonce><snonce>,s=<base64(salt)>,i=<iterations>"
client-final-message = "c=<base64(gs2-header)>,r=<nonce>,p=<base64(ClientProof)>"
  "n,," のとき c=biws
server-final-message = "v=<base64(ServerSignature)>"

SaltedPassword  = PBKDF2-HMAC-SHA-256(SASLprep(password), salt, i, 32 バイト)
ClientKey       = HMAC(SaltedPassword, "Client Key")
StoredKey       = SHA-256(ClientKey)
ServerKey       = HMAC(SaltedPassword, "Server Key")
AuthMessage     = client-first-message-bare + "," + server-first-message + "," + client-final-message-without-proof
ClientSignature = HMAC(StoredKey, AuthMessage)
ClientProof     = ClientKey XOR ClientSignature
ServerSignature = HMAC(ServerKey, AuthMessage)
```

サーバの検証: `ClientKey' = ClientProof XOR ClientSignature` を計算し、`SHA-256(ClientKey') == StoredKey` を**定数時間で**比較する。サーバはパスワードを知らなくても検証できる（StoredKey と ServerKey だけを保存する）。

サーバ側の検査項目（`auth-scram.c` の `read_client_first_message` / `read_client_final_message`）【記憶】

- gs2 ヘッダが `y` で、サーバが `-PLUS` を提示していた場合はダウングレード攻撃として拒否（TLS が無い yuzhu では起きない）。
- `p=` なのに `SCRAM-SHA-256` を選んだ場合は拒否【確認: 逆の場合の文言 "The client selected SCRAM-SHA-256-PLUS, but the SCRAM message does not include channel binding data."】。
- `a=`（authzid）は非対応としてエラー。拡張属性 `m=` もエラー。
- client-final の `r=` がサーバの送った nonce と一致しなければ `invalid SCRAM response`（DETAIL `Nonce does not match.`）【確認】。`c=` が gs2 ヘッダと一致するかも確かめる。
- 形式エラーは 08P01 `malformed SCRAM message`【記憶】。

既定値【記憶: `scram-common.h`】

- ソルト長 `SCRAM_DEFAULT_SALT_LEN` = 16 バイト、サーバ nonce の元のバイト数 `SCRAM_RAW_NONCE_LEN` = 18（base64 で送る）、反復回数 `scram_iterations` = 4096【確認: 4096】。PG16 から GUC `scram_iterations`（ParameterStatus でも報告）。

### 4.3 SASLprep【確認: 文書】

- パスワードを UTF-8 とみなして SASLprep（RFC 4013: 空白類のマッピング、NFKC 正規化、禁止文字・双方向文字の検査）にかける。**UTF-8 として不正、または禁止文字を含む場合はエラーにせず、生のバイト列をそのまま使う**【確認】。
- PG の実装は `src/common/saslprep.c`（Unicode の表を持つ自前実装）。
- yuzhu【推奨】: `stringprep` クレートの `saslprep()` を使い、`Err` なら生のバイト列にフォールバックする（PG と同じ）。依存は `unicode-normalization` などを含む。依存を避けたいなら、**ASCII だけのパスワードは SASLprep の影響を受けない**（印字可能 ASCII はそのまま、制御文字は禁止 → 生のバイト列）ので、「ASCII なら素通し、非 ASCII なら stringprep」の 2 段構えでもよい。クライアント（libpq、tokio-postgres は `stringprep` を使う）と結果が一致しないと、非 ASCII のパスワードで認証できなくなる点に注意。

### 4.4 pg_authid.rolpassword の保存形式【確認: `parse_scram_secret`】

```
SCRAM-SHA-256$<iterations>:<base64(salt)>$<base64(StoredKey)>:<base64(ServerKey)>
例: SCRAM-SHA-256$4096:c2FsdHNhbHRzYWx0c2FsdA==$<44 文字>:<44 文字>
```

- `CREATE ROLE ... PASSWORD 'plain'` のとき、`password_encryption`（既定 `scram-sha-256`）に従ってサーバでこの形式に変換する。
- **値がすでに SCRAM の形式なら、そのまま保存する**【記憶: `encrypt_password()` が `get_password_type()` で判定】。psql の `\password` や libpq の `PQchangePassword`（PG17）は、クライアント側で SCRAM 形式に変換した値を `ALTER USER x PASSWORD 'SCRAM-SHA-256$...'` で送るので、これに対応しないと psql の `\password` が使えない。
- `md5` + 32 桁の 16 進数（MD5 形式）も PG17 は受け付ける（PG18 で非推奨の警告）。yuzhu は MD5 認証を実装しないので、MD5 形式は**保存を拒否する**（0A000）のを推奨（確認事項 C-6）。
- パスワードの暗号化前の値は**ログや pg_stat 類に出さない**。PG は `CREATE ROLE ... PASSWORD` の文自体はログに残りうる（`log_statement`）ので注意喚起している。
- `pg_authid` はスーパーユーザーだけが読める。一般向けの `pg_roles` ビューは `rolpassword` を `'********'` にする【記憶】。M5 では GRANT が無いので、`pg_authid` を読めるのはスーパーユーザーだけ、という検査を特別扱いで入れる（42501）。

### 4.5 存在しないユーザー・失敗時の応答【記憶 + 確認】

- ユーザーが存在しない、パスワードが無い（`rolpassword IS NULL`）、SCRAM 以外の形式で保存されている、のいずれでも、**最後まで SCRAM のやり取りを行い**、偽のソルト（ユーザー名から決定的に作る `scram_mock_salt`）を返して最後に失敗させる【確認: `mock_scram_secret`】。これでユーザーの存在を推測させない。
- 失敗時: `FATAL 28P01 password authentication failed for user "alice"`【記憶】。理由の詳細（存在しない、期限切れなど）はサーバのログにだけ書く（`DETAIL` はクライアントに送らない）。
- `rolvaliduntil` を過ぎていたら失敗扱い（28P01）。
- 認証の後で: `rolcanlogin = false` なら `FATAL 28000 role "alice" is not permitted to log in`、`rolconnlimit` 超過なら `FATAL 53300 too many connections for role "alice"`【記憶】。trust で存在しないユーザーなら `FATAL 28000 role "alice" does not exist`。
- 認証のタイムアウト（`authentication_timeout`、既定 1 分）を設ける。認証が終わるまで読み込みにタイムアウトを付けないと、接続を開いたまま放置する攻撃でスレッドを使い切られる（接続ごとに 1 スレッドの yuzhu では重要）。

### 4.6 暗号クレート【推奨】

| 用途 | クレート | 備考 |
|---|---|---|
| SHA-256 | `sha2` | RustCrypto。内部に unsafe（SIMD）があるが、`#![forbid(unsafe_code)]` は自分のクレートにだけ効くので問題ない |
| HMAC | `hmac` | `Mac::verify_slice` で定数時間比較もできる |
| PBKDF2 | `pbkdf2`（`hmac` 機能） | 4096 回で 1 回あたり数 ms |
| base64 | `base64` | 標準アルファベット、パディングあり |
| 乱数（nonce、ソルト、BackendKeyData の秘密鍵） | `getrandom`（または `rand`） | OS の CSPRNG |
| 定数時間比較 | `subtle`（または `hmac` の verify） | |
| SASLprep | `stringprep` | tokio-postgres も使用 |

- 依存は `yuzhu-core` ではなく **`yuzhu-auth`（新クレート）か `yuzhu-server`** に閉じ込める。ただし `CREATE ROLE ... PASSWORD` でサーバ側の SCRAM 変換が要るので、コア（コマンドの実行）から呼べる位置が必要。【推奨】小さな `yuzhu-auth` クレート（`scram::make_secret(password, iterations) -> String`、`scram::ServerExchange` の状態機械）を作り、core と server の両方が依存する。
- **PBKDF2 は CPU を使う**（4096 回で数 ms）。接続ごとのスレッドで計算するので、他の接続を止めることはない。ロックを持ったまま計算しないこと（カタログの読み取りロックを先に外す）。
- 自前実装（SHA-256 と HMAC で約 200 行）も可能だが、CLAUDE.md は暗号を外部クレートの許容範囲としているので使う。

### 4.7 その他の認証方式（参考）

| 方式 | 応答 | yuzhu |
|---|---|---|
| trust | AuthenticationOk | M1 から |
| password（平文） | `R` int32 3 → PasswordMessage `p`（cstring） | **S。** SCRAM の秘密情報に対して `scram_verify_plain_password` 相当で検証できる。TLS 無しでは盗聴されるので、既定では使わない |
| md5 | `R` int32 5 + 4 バイトのソルト | 実装しない（PG18 で非推奨）【推奨】 |
| reject | `FATAL 28000 pg_hba.conf rejects connection for host ...` | S |
| cert / gss / ldap など | | M6 以降または対象外 |

---

## 5. ロールの DDL（CREATE ROLE / ALTER ROLE / DROP ROLE）

### 5.1 構文（M5 で受理する範囲）【記憶: sql-createrole.html】

```
CREATE ROLE name [ [ WITH ] option [ ... ] ]
CREATE USER name ...            -- CREATE ROLE ... LOGIN と同じ
option:
    SUPERUSER | NOSUPERUSER | CREATEDB | NOCREATEDB | CREATEROLE | NOCREATEROLE
  | INHERIT | NOINHERIT | LOGIN | NOLOGIN | REPLICATION | NOREPLICATION
  | BYPASSRLS | NOBYPASSRLS | CONNECTION LIMIT n
  | [ ENCRYPTED ] PASSWORD 'password' | PASSWORD NULL
  | VALID UNTIL 'timestamp'
  | IN ROLE ... | ROLE ... | ADMIN ... | SYSID n      -- M6（ロールの所属）。M5 では 0A000

ALTER ROLE name [ WITH ] option [ ... ]
ALTER ROLE name RENAME TO new_name
ALTER ROLE { name | CURRENT_USER | SESSION_USER } SET configuration_parameter { TO | = } value  -- M6 でもよい
DROP ROLE [ IF EXISTS ] name [, ...]
```

- `VALID UNTIL` は timestamptz（M5 の型追加の後）。型が入るまでは 0A000 でよい。
- `pg_authid` の列は `m2-catalog.md` の通り（`oid, rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, rolreplication, rolbypassrls, rolconnlimit, rolpassword, rolvaliduntil`）。CREATE ROLE の既定値: `rolinherit = true`、`rolcanlogin = false`（CREATE USER なら true）、`rolconnlimit = -1`、ほかは false【記憶】。

### 5.2 権限と検査（GRANT が無い M5 での最小限）【記憶】

| 操作 | 必要な権限 | エラー |
|---|---|---|
| CREATE ROLE | スーパーユーザー、または CREATEROLE 属性 | 42501 `permission denied to create role` |
| SUPERUSER 属性を付ける / 外す | スーパーユーザー | 42501 |
| 自分のパスワードを変える | 誰でも | |
| 他人の属性を変える | スーパーユーザー、または CREATEROLE（PG16 以降は ADMIN OPTION も必要。M5 は CREATEROLE だけで簡略化） | 42501 |
| DROP ROLE | 同上 | 42501 |
| 既存の名前 | | 42710 `role "alice" already exists` |
| 存在しない | | 42704 `role "alice" does not exist`（IF EXISTS なら NOTICE） |
| `pg_` で始まる名前 | | 42939 `role name "pg_x" is reserved` |
| 自分自身・接続中のユーザーを DROP | | 55006 `current user cannot be dropped` / `session user cannot be dropped` |
| 所有物がある | | 2BP01 `role "alice" cannot be dropped because some objects depend on it`（DETAIL に所有物の一覧） |
| ブートストラップユーザー（OID 10） | | 55006 系（要確認） |

- 「所有物がある」の判定: PG は `pg_shdepend` を使う。yuzhu の M5 では、`pg_database.datdba`、`pg_class.relowner`（全 DB）、`pg_namespace.nspowner` を走査する。他の DB のカタログは開かないと見えないので、**M5 では「pg_database の所有者だけ検査し、テーブルの所有者は現在の DB だけ検査する」**か、`pg_shdepend` 相当の共有カタログを作るかを決める必要がある（確認事項 C-7）。
- `ALTER ROLE ... RENAME` で MD5 形式のパスワードは消える（名前がソルトなので）が、SCRAM なら残る【記憶】。yuzhu は SCRAM のみなので気にしなくてよい。
- 権限の検査は M6 の GRANT / REVOKE につながる。M5 では「スーパーユーザーか、属性を持つか」の 2 値の検査を `Session::require_role_attr(Attr)` のような関数に集める。

---

## 6. pg_hba 相当の設定（最小）

### 6.1 PG の仕組み【記憶: auth-pg-hba-conf.html】

- `pg_hba.conf` は 1 行に 1 規則。**上から順に見て、接続の種類・データベース・ユーザー・アドレスが一致した最初の行**の方式で認証する。一致する行が無ければ拒否。
- 書式: `TYPE DATABASE USER ADDRESS METHOD [options]`
  - TYPE: `local`（Unix ソケット）、`host`（TCP、TLS の有無を問わない）、`hostssl`、`hostnossl`、`hostgssenc`、`hostnogssenc`
  - DATABASE: `all`、`sameuser`、`samerole`、`replication`、名前（カンマ区切り）、`@file`
  - USER: `all`、名前、`+group`（所属）、`/正規表現`（PG16+）
  - ADDRESS: `127.0.0.1/32`、`::1/128`、`0.0.0.0/0`、`samehost`、`samenet`、ホスト名
  - METHOD: `trust`、`reject`、`scram-sha-256`、`md5`、`password`、`peer`、`ident`、`cert` ほか
- 一致する行が無いとき: `FATAL 28000 no pg_hba.conf entry for host "10.0.0.5", user "alice", database "app", no encryption`【記憶】。
- 設定の再読み込みは SIGHUP または `pg_reload_conf()`。新しい接続から効く。
- `md5` 方式で、保存されたパスワードが SCRAM なら、SCRAM 認証を行う（`md5` は「md5 か scram」の意味）【記憶】。
- initdb の既定は `trust`（警告を出す）。`--auth`, `--auth-host`, `--auth-local` で変えられる。

### 6.2 yuzhu の推奨（M5）

- ファイル名 `yuzhu_hba.conf`（データディレクトリ直下。initdb が生成する）。**書式は PG と同じ**で、次の部分集合だけ受け付ける。
  - TYPE: `host`（M5）。`hostssl` / `hostnossl` は M6 の TLS と一緒。Unix ソケットに対応する場合は `local`（現状 yuzhu は TCP のみの前提なので、確認事項 C-8）。
  - DATABASE: `all`、`sameuser`、名前のカンマ区切り。
  - USER: `all`、名前のカンマ区切り。
  - ADDRESS: CIDR（IPv4 / IPv6）、`all`。
  - METHOD: `trust`、`reject`、`scram-sha-256`、`password`。`md5` は「scram-sha-256 として扱う」（PG と同じ解釈）。それ以外はファイル読み込み時にエラーで起動しない。
- 起動時に読み、解析エラーなら起動を止める（PG は再読み込み時のエラーなら古い設定を使い続ける）。再読み込み（SIGHUP、`pg_reload_conf()`）は M6 でよい。
- initdb の既定【推奨】: `host all all 127.0.0.1/32 trust` と `host all all ::1/128 trust`（PG の initdb と同じく trust ＋警告）。`yuzhu-initdb --auth-host=scram-sha-256 --pwfile=...` で変えられるようにする。
- テスト: sqllogictest-bin の既定パスワードは `postgres`（`research-slt.md`）。CI では yuzhu と本物の PG の両方で、ユーザー `postgres` / パスワード `postgres` / `scram-sha-256` の構成でも 1 ジョブ流す。
- 工数: S（解析 0.5 日、照合 0.5 日）。

---

## 7. CREATE DATABASE / DROP DATABASE と別 DB への接続

### 7.1 CREATE DATABASE【記憶: sql-createdatabase.html、dbcommands.c】

```
CREATE DATABASE name [ WITH ]
    [ OWNER [=] user ] [ TEMPLATE [=] template ] [ ENCODING [=] encoding ]
    [ STRATEGY [=] { WAL_LOG | FILE_COPY } ]
    [ LOCALE [=] locale ] [ LC_COLLATE [=] .. ] [ LC_CTYPE [=] .. ] [ LOCALE_PROVIDER ..] ...
    [ TABLESPACE [=] .. ] [ ALLOW_CONNECTIONS [=] bool ] [ CONNECTION LIMIT [=] n ]
    [ IS_TEMPLATE [=] bool ] [ OID [=] oid ]
```

- 既定のテンプレートは `template1`、既定の方式は PG15 以降 `WAL_LOG`。
- **トランザクションブロック内では実行できない**: 25001 `CREATE DATABASE cannot run inside a transaction block`【記憶】。
- 権限: スーパーユーザーか CREATEDB 属性。無ければ 42501 `permission denied to create database`。
- 既存の名前: 42P04 `database "x" already exists`。テンプレートが無い: 3D000 `template database "x" does not exist`。
- テンプレートに**他の接続があると失敗**: 55006 `source database "template1" is being accessed by other users`（DETAIL `There is 1 other session using the database.`）【記憶】。コピー中に内容が変わるのを防ぐため。
- `datistemplate = false` の DB をテンプレートにできるのは、スーパーユーザーかその DB の所有者だけ（42501）。
- ENCODING / LOCALE: yuzhu は UTF8 と C ロケールだけ。それ以外を指定したら PG と同じ 22023 `invalid locale name` 系か、0A000 にする（確認事項 C-9）。`ENCODING 'UTF8'` は受け付ける。

**PG の 2 つの方式**【記憶】

| 方式 | やること | 長所・短所 |
|---|---|---|
| `FILE_COPY`（PG14 以前の唯一の方式） | (1) チェックポイント（テンプレートの汚れたページをディスクに書く）→ (2) ディレクトリをファイル単位でコピー → (3) `XLOG_DBASE_CREATE_FILE_COPY` を WAL に書く（「src を dst にコピーした」という論理レコード）→ (4) もう一度チェックポイント（リカバリ時にコピーをやり直さずに済むように） | WAL が少ない。チェックポイントが 2 回あり遅い。リカバリ時に (3) を再生すると、その時点のテンプレートのファイルをコピーすることになり、テンプレートがその後に変更されていると厳密には正しくない（PG が WAL_LOG を作った理由の一つ） |
| `WAL_LOG`（PG15 以降の既定） | テンプレートの全ページをバッファプール経由で読み、新しいリレーションに書き、**各ページを WAL に記録する**（FPI） | チェックポイント不要。大きいテンプレートでは WAL が大量。リカバリ・レプリケーションに強い |

**yuzhu の推奨: FILE_COPY 相当**

1. `CREATE DATABASE` を受けたら、トランザクションブロックの外であることを確かめる。
2. クラスタ全体の「DB 作成ロック」を取る。テンプレートに他の接続が無いことを確かめ、**作成が終わるまでテンプレートへの新しい接続を止める**（PG は `LockSharedObject` で接続を待たせる）。
3. 新しい DB の OID を共有の OID カウンタから割り当てる。
4. チェックポイント（M3 のチェックポイント処理を呼ぶ。少なくともテンプレートの DB に属する汚れたページを書き出す）。
5. `base/<src>/` を `base/<new>/` にコピーする（ファイル I/O の抽象化レイヤ経由。障害注入できるように）。各ファイルを fsync し、ディレクトリも fsync。
6. WAL に `DbaseCreate { src_db, dst_db }` を書く。REDO は「dst があれば消してから src をコピー」（冪等）。
7. `pg_database` に行を挿入して commit（`datname, datdba, encoding=6, datistemplate, datallowconn, datconnlimit, datcollate='C', datctype='C', ...`）。
8. チェックポイント（6 の REDO が走る範囲を狭める。PG と同じ）。
9. 失敗時（5〜7 の途中）は `base/<new>/` を消す（PG の `createdb_failure_callback`）。commit の前にクラッシュした場合、ディレクトリだけが残る。M5 では「起動時に pg_database に無い `base/<oid>/` を警告して残す」か「消す」かを決める（確認事項 C-10）。

- M2 の initdb で template0 / postgres を作るときのコピー処理（`m2-catalog.md` §5）をそのまま使える。
- コピーされたカタログのタプルの xmin はテンプレートのもの。**コミットログ（clog 相当）と xid はクラスタ全体で共有**なので、そのまま正しく見える（M3 の設計で xid と clog がクラスタ単位であることが前提。DB ごとに持つ設計にしていたら破綻する）。
- **バッファプールのキーは (dboid, relfilenode, block)** でなければならない（同じ relfilenode が DB ごとに存在する）。M2 の BufferTag に dboid が入っているか確認すること。
- `STRATEGY = WAL_LOG` の指定は「受理して FILE_COPY で実行」か 0A000 か（確認事項 C-11）。

工数: M（2〜3 日。テンプレートへの接続制御とクラッシュ時の後始末が中心）。

### 7.2 DROP DATABASE【記憶: sql-dropdatabase.html、dbcommands.c `dropdb`】

```
DROP DATABASE [ IF EXISTS ] name [ [ WITH ] ( FORCE [, ...] ) ]
```

- ブロック内では不可（25001）。権限: 所有者かスーパーユーザー（42501 `must be owner of database x`）。
- 存在しない: 3D000 `database "x" does not exist`（IF EXISTS なら NOTICE）。
- **接続中の DB は消せない**: 55006 `cannot drop the currently open database`。
- 他の接続があると: 55006 `database "x" is being accessed by other users`（DETAIL `There are 2 other sessions using the database.`）。`WITH (FORCE)` なら他の接続を終了させる（`pg_terminate_backend` 相当の仕組みが必要。M5 では受理して 0A000 にするか、接続の強制終了を実装するか。確認事項 C-12）。
- テンプレート（`datistemplate = true`）は消せない: `cannot drop a template database`（SQLSTATE 要確認、おそらく 55006 か 22023）。
- PG15 以降の手順: (1) pg_database の行の `datconnlimit` を `-2`（DATCONNLIMIT_INVALID_DB、「壊れた DB」の印）にして commit 相当で確定、(2) ファイルを消す、(3) 行を消す。途中でクラッシュしても「印の付いた DB には接続できず、DROP DATABASE だけができる」状態になる【記憶】。

**yuzhu の推奨手順**

1. 接続数を確かめ、新しい接続を止める（DB 単位のロック）。
2. pg_database の行を削除して commit（WAL に記録）。
3. **バッファプールからその DB のページを書き出さずに捨てる**（汚れていても捨てる）。
4. WAL に `DbaseDrop { db }` を書き、`base/<db>/` を削除。REDO は「ディレクトリがあれば消す」（冪等）。
5. 2 と 4 の間でクラッシュしたら、ディレクトリが残るだけで害は無い（7.1 の 9 と同じ後始末で消せる）。

PG の「無効の印」方式は、行を消すより前にファイルを消し始めるための工夫。yuzhu は「先に行を消してからファイルを消す」順序にするので、印は要らない。

工数: S〜M（1〜1.5 日）。

### 7.3 別の DB への接続

- 起動パラメータの `database`（省略時はユーザー名と同じ）で pg_database を引く。`m2-catalog.md` §4 で設計済みの流れに、次を加える。
  - `datallowconn = false` → `FATAL 55000 database "template0" is not currently accepting connections`（SQLSTATE 要確認）。
  - `datconnlimit` 超過 → `FATAL 53300 too many connections for database "x"`（スーパーユーザーは除外）。
  - 作成中・削除中の DB への接続は待たせるか拒否する（7.1 の 2、7.2 の 1 のロックと同じ）。
  - CONNECT 権限の検査は M6（GRANT と一緒）。
- psql の `\c dbname` は新しい接続を作るだけなので、サーバ側で特別な処理は要らない。
- **接続数の数え方**: `Cluster` が「DB の OID → 接続しているセッションの一覧」を持つ。CREATE / DROP DATABASE と接続数の制限はここを見る。DROP DATABASE WITH (FORCE) の実装でも同じ表を使う。
- `current_database()`、ParameterStatus には影響しない（PG は `database` を ParameterStatus で送らない）。

---

## 8. テストの方針

1. **sqllogictest の extended エンジン**: `sqllogictest --engine postgres-extended` で `tests/` の既存スイートを流す（Simple との出力の違い、とくに float の整形は `skipif postgres-extended` か、クライアント側の整形に合わせた別ファイルで吸収）。本物の PG17 でも同じスイートを流して正解を確かめる。
2. **プロトコルの結合テスト（Rust）**: `yuzhu-server` の dev-dependency に `postgres`（同期版）を入れ、`query` / `prepare` / パラメータ / バイナリ / `query_portal` を試す。同じテストを環境変数で本物の PG17 にも向けられるようにする（共通テストの原則）。
3. **生メッセージのテスト**: 自前の小さなクライアント（テスト用）で、Parse のエラー → 読み捨て → Sync → ReadyForQuery、無名の文の上書き、存在しないポータル、パイプラインでの途中のエラー、BEGIN 中の Sync、PortalSuspended → Sync（自動 commit）→ 同じポータルの Execute（34000）を確かめる。期待値は本物の PG17 で記録する（「ゴールデン」ファイル）。
4. **型推論の表**（2.5）を、`psql` の `\parse` + `\bind_named`、または結合テストの `prepare` で PG17 と照合する。ParameterDescription の OID を比べるテストにする。
5. **ドライバの動作確認**（CI の別ジョブ）: pgJDBC（Statement / PreparedStatement を 6 回以上ループしてバイナリ切り替えを踏む、fetchSize）、psycopg 3（str パラメータ、prepare_threshold 超え）、node-postgres（values、pg-cursor）、tokio-postgres。最初は手動でもよい。
6. **SCRAM**: RFC 7677 の 4.節のテストベクタ（ユーザー `user`、パスワード `pencil`、固定 nonce とソルト）で単体テスト【記憶: RFC 7677 Section 3 に例がある】。PG17 で `CREATE ROLE x PASSWORD 'pencil'` した後の `rolpassword` を、同じソルトと反復回数で yuzhu が再現できることも確かめる（`SELECT rolpassword FROM pg_authid` は salt が乱数なので、ソルトと反復回数を取り出して検証する）。psql の `\password` で変えたパスワードで接続できること。
7. **CREATE / DROP DATABASE**: 障害注入（コピーの途中、WAL 記録の前後、commit の前後）でクラッシュさせ、再起動後に整合していることを確かめる。

---

## 9. 工数の見積もり

| 項目 | 規模 | 目安 |
|---|---|---|
| Extended Query のメッセージの読み書き、SkipTillSync の状態機械 | S | 1 日 |
| 文・ポータルの管理、暗黙のトランザクション、Sync / Close / Flush | M | 2 日 |
| パラメータ型の推論（アナライザの変更）と PG との照合テスト | M | 2〜3 日 |
| バイナリ形式（M1 の型。M5 で追加する型は型追加の作業に含める。numeric は別途 M） | S | 1 日 |
| 行数制限と PortalSuspended、RETURNING の結果保持 | S〜M | 1〜1.5 日 |
| 準備済み文の再解析（カタログ世代）、`cached plan must not change result type` | S | 0.5〜1 日 |
| PREPARE / EXECUTE / DEALLOCATE / DISCARD ALL | S | 1 日 |
| ドライバ 4 種の動作確認と修正 | M | 2〜3 日（見つかる不具合次第） |
| SCRAM-SHA-256（yuzhu-auth、状態機械、偽ソルト、タイムアウト） | M | 2 日 |
| password（平文）方式 | S | 0.5 日 |
| CREATE ROLE / ALTER ROLE / DROP ROLE、属性の検査、pg_roles | M | 2 日 |
| yuzhu_hba.conf（解析・照合・initdb の生成） | S | 1 日 |
| CREATE DATABASE（FILE_COPY 相当、接続制御、後始末） | M | 2〜3 日 |
| DROP DATABASE | S〜M | 1〜1.5 日 |
| 別 DB への接続の追加検査（datallowconn、接続数） | S | 0.5 日 |
| **合計** | **L** | **約 20〜26 日** |

並列化の目安: (a) Extended Query（プロトコル＋セッション）、(b) 型推論＋バイナリ形式、(c) SCRAM＋ロール＋hba、(d) CREATE / DROP DATABASE は、境界（`Session` の API、`yuzhu-auth`、`Cluster` の接続表）を先に決めれば 4 並列で進められる。

---

## 10. 確認事項（仮決めした点。ユーザーに確認したい）

- **C-1 Extended Query の優先度**: M5 の中で Extended Query を最初に実装する（ドライバの既定モードがこれに依存するため）。M4 完了直後に前倒しする案もある。
- **C-2 準備済み文の計画**: Bind のたびに計画し直す（汎用プランは保存しない）。性能が問題になったら汎用プランのキャッシュを足す。
- **C-3 型推論の正確さの基準**: 2.5 の表を PG17 と照合するテストを作り、**ParameterDescription の OID が PG と一致すること**を合格条件にする（tokio-postgres のため）。
- **C-4 暗号クレート**: `sha2`、`hmac`、`pbkdf2`、`base64`、`getrandom`、`subtle`、`stringprep` を追加する。これらは新しい `yuzhu-auth` クレートに閉じ込める。自前実装はしない。
- **C-5 SASLprep**: `stringprep` クレートで PG と同じ処理（失敗時は生のバイト列）をする。依存を減らしたい場合は「ASCII は素通し」の簡略版にする（非 ASCII パスワードでクライアントと不一致になる恐れあり）。
- **C-6 MD5**: MD5 認証は実装しない。`ALTER ROLE ... PASSWORD 'md5...'`（MD5 形式の値）は 0A000 で拒否する。hba の `md5` は PG と同じく SCRAM として扱う。
- **C-7 DROP ROLE の依存検査**: `pg_shdepend` を作らず、pg_database の所有者と現在の DB のオブジェクトだけを検査する（他の DB のテーブルの所有者は見逃す）。正確にするなら `pg_shdepend` 相当を作る（+M）。
- **C-8 Unix ソケット**: M5 でも TCP のみとし、hba の `local` は受け付けない。psql は `-h localhost` が必要なまま。
- **C-9 ENCODING / LOCALE**: CREATE DATABASE は UTF8 と C ロケールだけを受け付け、それ以外は PG と同じ SQLSTATE（要確認）でエラーにする。
- **C-10 CREATE DATABASE のクラッシュ後の後始末**: pg_database に無い `base/<oid>/` は起動時に削除する（PG は残す。yuzhu は単純さと容量のために消す案）。
- **C-11 STRATEGY**: `STRATEGY = WAL_LOG` を指定されても FILE_COPY 相当で実行する（エラーにはしない）。
- **C-12 DROP DATABASE WITH (FORCE)**: M5 では 0A000 にし、接続の強制終了（`pg_terminate_backend`）と一緒に M6 で実装する。
- **C-13 認証の既定**: initdb の既定は PG と同じく `trust`（警告付き）。`--auth-host=scram-sha-256` で変更可能にする。CI では scram 構成のジョブも 1 つ追加する。
- **C-14 `server_version`**: 現在は 16.0 を名乗っている（Q-004）。Extended Query やドライバの挙動に版数依存はほとんど無い【記憶】ので、M5 でも変えない。
