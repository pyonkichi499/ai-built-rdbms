# yuzhu M5 基本設計 04: Extended Query・パラメータ型推論・準備済み文・バイナリ形式

この章は M5 の **XQ**（Extended Query）の設計です。ゴールは、pgJDBC・tokio-postgres・psycopg 3・node-postgres が**既定のモード（Extended Query）で繋がり、パラメータ付きの文とバイナリ形式の値を扱える**ことです。実装者がこの章だけ読めば実装できる粒度で、メッセージの受信と応答、Session の API の挙動、パラメータ型の推論、準備済み文のキャッシュ、バイナリ形式、COPY（任意）、ドライバの検証手順を定めます。

- 前提の設計: `spec/design/m1.md`（Session と `ResultSink`、Simple Query の意味論）、`m2.md`、`m3.md`（トランザクションと設定）、`spec/design/m4/00-contracts.md`（`Expr<C, Q>`、`BoundStatement`、`PhysicalQuery`、`ExecCtx`、COPY、`ddl/`）、**`spec/design/m5/00-contracts.md`（契約。D19、D20、D21、D45。§4.7 の `msg_*`、§4.9 の `types/binary.rs`、§4.12 の `ExtState`、§5.1 の文の実行）**。この章の WP は XQ-1〜XQ-7（00 §7）。
- 調査: `spec/research/m5-protocol-auth.md` §2（Extended Query）、§3（ドライバ）、`pg-compat-tools.md` §2.1・§4（COPY）、`research-pg-protocol.md` §4、`m5-types-fk.md` §2.8・§3.8（バイナリ形式）。
- 根拠の記号: **【実機】**＝この章を書くときに PostgreSQL 17.11（`sandbox/pg.sh start`、127.0.0.1:55432）へ生のプロトコルでメッセージを送って採取した（再現用の transcript は §7.2 に写した）。**【確認】**＝ソース（tokio-postgres 0.7.18 など、手元のクレート）や文書を読んで確かめた。**【記憶】**＝未照合。**【提案】**＝yuzhu への推奨。PostgreSQL のソースは `PG:<path>`（REL_17_STABLE）。
- 調査レポート（m5-protocol-auth）の【記憶】のうち、実機と食い違ったものは §0 の決定表に出典つきで書いた（XQ-D3、D4、D9、D10、D13。ポータルの重複の文言は §6.2.1 と C4）。

---

## 0. 決定（この章で扱う論点。調査間の食い違いも）

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| XQ-D1 | プロトコルの状態機械の置き場所 | 全部 server が持つ／Session に `handle_ext(msg)` を 1 つ（m5-protocol-auth §2.3）／契約の `msg_*` を 1 メッセージ 1 メソッド（00 §4.7） | **契約どおり。メッセージの読み書きと `SkipTillSync`・flush・COPY の受信ループは yuzhu-server、文・ポータル・トランザクションは Session**。server は Session の内部状態（暗黙のトランザクションなど）を見ない | M1 の「Session は `ResultSink` に書く」境界を保つ。Session だけで単体テストができる |
| XQ-D2 | 暗黙のトランザクションの寿命 | メッセージごとに開閉／Sync まで（m5-protocol-auth §2.4） | **Sync まで 1 つ。`TxState::Implicit` が複数のメッセージにまたがる**。途中に Simple Query の `Q` が来たら、**同じ暗黙のトランザクションを引き継ぎ、`Q` の終わりでコミットする**（ReadyForQuery は `Q` の分と、その後の Sync の分の 2 回）。ポータルは暗黙のトランザクションが終わると全部消える | 【実機】2 つの Execute を Sync なしで送り、2 つ目が一意制約違反になると 1 つ目も巻き戻る。`P B E Q S` では `Q` の結果に `E` の INSERT が見え、ReadyForQuery が 2 回返る |
| XQ-D3 | ポータルの種別と、スナップショットを取る時点 | 全ポータルを Bind 直後に（00 §5.1）／最初の Execute（m5-protocol-auth §2.4「または Bind 直後」と曖昧）／PostgreSQL の `PortalStrategy` と同じ | **PostgreSQL と同じ 5 種（`OneSelect`、`OneReturning`、`UtilSelect`、`MultiQuery`、`Empty`）に分け、スナップショットは `OneSelect` だけ Bind で取り、ほかは Execute で取る**。計画する文（SELECT・DML・EXPLAIN）のロックは Bind で取り（D21）、DDL・COPY・ユーティリティ文のロックは Execute で取る（Simple Query と同じ §5.1 d の手順）。00 §5.1 の最後の項目を §11 の変更依頼 C1 で直す。**ただし Repeatable Read の最初のスナップショットだけは、計画する文の Bind でロックの前（5.5 の手順 5'。02 RW-D10、01 LK-D27）**。RC の `OneSelect` のスナップショットはロックの後 | 【実機】トランザクション内で `SELECT count(*)` のポータルを Bind してから INSERT を実行すると、Execute しても INSERT は見えない。逆に DELETE のポータルを先に Bind し、あとから別の文で INSERT した行は、Execute すると削除される（`DELETE 3`）。中断した SELECT のポータルは、同じトランザクションの後続の書き込みを見ない |
| XQ-D4 | CommandComplete の行数 | ポータル全体の累計／その Execute で返した行数（m5-protocol-auth §2.7「要確認」） | **その Execute で返した行数**（`SELECT n`、`INSERT 0 n` の n）。中断した後の最後の Execute は残りの行数、完了したポータルを再度 Execute すると `SELECT 0`（`OneSelect`、`OneReturning`、`UtilSelect`）。`MultiQuery` は再度 Execute すると `55000 portal "p" cannot be run` | 【実機】5 行を max_rows = 2 で 3 回 Execute すると `SELECT 1`、続く 4 回目が `SELECT 0`。ちょうど割り切れる場合（4 行・2 件ずつ）は 2 回目も `PortalSuspended` で、3 回目が `SELECT 0`。`INSERT ... RETURNING` の 3 行を max_rows = 2 で取ると、1 回目は 2 行と `PortalSuspended`、2 回目は 1 行と `INSERT 0 1`、3 回目は `INSERT 0 0` |
| XQ-D5 | 結果の送り方 | Output に溜めてから送る（M3 の Simple Query）／ストリーミング | **Extended の `OneSelect` は 1 行ずつ `sink.data_row_raw` へ流す**（途中のエラーまでの行はクライアントに届く。PostgreSQL と同じ）。`OneReturning` と `UtilSelect`（SHOW、EXPLAIN、EXECUTE）は最初の Execute で完走して整形済みの行を保存し、max_rows ごとに返す。**Simple Query の方式（Output に溜める）は変えない** | 中断できる Volcano 型の実行器をそのまま持てる（§5.9）。DML は途中で止めると未実行の行が残る |
| XQ-D6 | 準備済み文が持つもの | 汎用プランを保存（m5-protocol-auth §2.8）／AST + 世代（D19）／Bound を保存 | **AST（`Arc<Statement>`）、宣言された型、確定した型、結果の列、解析結果（`Arc<BoundStatement>`。SELECT と DML だけ）、解析の鍵（世代・search_path・DateStyle・TimeZone）を持つ。計画（`planner::plan`）は Bind ごとにやり直し、汎用プランは作らない**。DDL・COPY・ユーティリティ文は Parse では構文だけを検査し、解析は Execute のたびに行う（PostgreSQL と同じ。名前の解決のエラーは Execute で出る）。EXPLAIN は内側の文を Parse で解析し、SELECT と同じに保存する | **D19 からの逸脱（D19 の方を本章に合わせて改めた。§11 C14。R-15）**。解析（名前解決と型付け）は世代が同じなら再利用し、計画は M4 の planner が軽いので毎回行う。参照リレーションの一覧は持たない（Bind のたびに生のパース木から集める。C7） |
| XQ-D7 | 解析結果が古いかの判定 | カタログ世代だけ（m5-protocol-auth §2.8）／無効化メッセージ | **鍵 `AnalysisKey { generation, search_path, datestyle, timezone }` が変わった、または自分のトランザクションがカタログを変更中（`txn.catalog_dirty`）、または解析したときに `catalog_dirty` だった、のどれかで再解析する** | 世代（`db.cache.generation()`）はコミットされた DDL でしか進まない。自分の未コミットの DDL（D12 でカタログ用スナップショットには見える）と、リテラルのキャストを解析時に評価する日時の設定（M4 §12.4）を落とさない |
| XQ-D8 | `$n` の型推論の実装 | アナライザを別の入口に複製／既存のアナライザに `ParamCtx` を渡す | **`Analyzer` に `params: Option<&ParamCtx>` を足し、PostgreSQL の `parse_param.c`（`variable_paramref_hook`、`variable_coerce_param_hook`、`check_variable_parameters`）と同じ 3 点を実装する**。`analyze(stmt, catalog)` の署名は変えない（D45、00 §4.7） | 型推論の規則（演算子・関数の解決、`select_common_type`、代入）を二重に持たない |
| XQ-D9 | 型推論のエラー | 42P08 は「同じ `$n` に別の型」だけ（m5-protocol-auth §2.5）／実機 | **42P08 を 2 種類出す。(a) `inconsistent types deduced for parameter $n`（DETAIL `integer versus text`）、(b) `could not determine data type of parameter $n`（位置つき。文の中に型が決まらないまま残った `$n` の節点があり、ほかの出現で型が決まったとき）。どの出現でも決まらなければ 42P18（位置なし）。(b) の検査が先** | 【実機】`SELECT $1 IS NULL OR $1 = 1`（`WHERE $1 IS NULL OR a = $1` と同じ形。ORM がよく書く）は 42P08 (b)。`SELECT $1 IS NULL OR $1 IS NOT NULL` は 42P18。調査は (b) を知らなかった |
| XQ-D10 | バイナリのパラメータの長さ違反 | どちらも 22P03（00 §4.9）／実機 | **長さが足りなければ `08P01 insufficient data left in message`、余りがあれば `22P03 incorrect binary data format in bind parameter N`**。値の中身の不正（numeric の符号など）は型ごとの recv が 22P03 と固有の文言で返す | 【実機】int4 に 2 バイトで 08P01、5 バイトで 22P03。00 §4.9 の「長さの過不足は 22P03」を §11 の C5 で直す |
| XQ-D11 | パラメータの typmod | Bind で列の typmod を渡す／渡さない | **Bind は typmod = −1 で値を作る。長さ・精度の検査と丸めはプランの `CoerceTypmod`（代入の文脈）が行う**。ParameterDescription は OID だけを返す | PostgreSQL と同じ。【実機】`INSERT INTO t(c) VALUES ($1)`（`c varchar(10)`）に 16 文字は 22001、`numeric(10,2)` に `'1.005'` は `1.01` |
| XQ-D12 | ParameterStatus を送る時点 | SET の直後／Sync の ReadyForQuery の直前 | **Sync の処理で、ReadyForQuery の直前に、前回送った値から変わった項目だけ送る**（`Session::pending_parameter_status`。Simple Query の最後と同じ仕組み） | 【実機】`SET application_name` を Execute した結果は `CommandComplete(SET)`、`ParameterStatus`、`ReadyForQuery` の順 |
| XQ-D13 | flush の規則 | Flush と Sync だけ（00 §4.12）／実機 | **Flush（`H`）、Sync の応答、ErrorResponse、NoticeResponse、CopyInResponse の後で flush する。それ以外のメッセージでは flush しない**。00 §4.12 を §11 の C3 で補う | 【実機】Parse だけ送ると応答は来ない（flush されない）。Parse の構文エラーは Flush も Sync もなしで 1 秒以内に届く。NoticeResponse も同じ |
| XQ-D14 | `pg_prepared_statements` | M5 で作る／作らない | **作らない（M6）**。準備済み文の存在は `EXECUTE` / `DEALLOCATE` / Bind のエラー（26000）で確かめる | 仮想リレーション（D33）の `VirtualCtx` にセッションの情報が無く、`regtype[]`（OID 2211）の型の行も要る。ドライバは使わない。作る場合の費用は M5-XQ-Q1 |
| XQ-D15 | 結果の形式コードの検査 | PostgreSQL は DataRow を作るときに 22023（m5-protocol-auth に記載なし）／Bind で検査 | **Bind で検査する**（0 と 1 以外は `22023 unsupported format code: N`。個数の不一致は `08P01 bind message has N result formats but query has M columns`）。バイナリを持たない型の値は、出力のときに `42883 no binary output function available for type T` | 【実機】結果の形式コード 2 は Bind が通り、出力で初めて失敗する。yuzhu は早く失敗する（既知の差。M5-XQ-Q3）。バイナリを持たない型（aclitem）は行が 0 件なら成功する |
| XQ-D16 | `$n` の番号の上限 | 65535 を超えても通す／上限を設ける | **字句解析で `$n` の n が 65535 を超えたら `42601 parameter number too large`**。n = 0 は解析で `42P02 there is no parameter $0` | ParameterDescription と Bind の個数は 16 ビット。AST の `Param.index` は `u16` |
| XQ-D17 | Execute の最大行数 | `u32` のまま／符号つき | **server が `i32` で読み、0 以下を 0（全行）にして `u32` で Session に渡す** | 【実機】`-1` は全行を返す |
| XQ-D18 | `F`（FunctionCall）と、COPY の外の `d`・`c`・`f` | 拡張扱いで Sync まで読み捨て（M1）／PostgreSQL と同じ | **`F` は `0A000 function call is not supported` を返し、Sync を待たずに ReadyForQuery を返す**（`F` は `Q` と同じく 1 メッセージで完結する）。**COPY の外で来た `d`・`c`・`f` は黙って無視する**（COPY が失敗した後もクライアントが送り続けるため）。それ以外の未知の型は FATAL 08P01 | M1 の「`F` の後に Sync まで読み捨て」はクライアントが Sync を送らず固まる。`d` `c` `f` の無視は PostgreSQL の `PostgresMain` と同じ【記憶】 |
| XQ-D19 | COPY（XQ-6） | Extended では 0A000／COPY IN と OUT を Extended でも | **XQ-6 で `COPY ... TO STDOUT`、CSV（FROM と TO）、Extended 経由の COPY（IN と OUT）を足す。COPY IN の間に来た Sync・Flush は無視し、コピーが終わった後の次の Sync で ReadyForQuery を返す。COPY FROM STDIN の失敗（CopyFail、データのエラー）は Sync まで読み捨てる** | 【実機】tokio-postgres の `copy_in` は Bind・Execute・Sync を送った後で CopyData・CopyDone・Sync を送る（【確認】`copy_in.rs`）。最初の Sync は無視され、ReadyForQuery は 1 回だけ返る |
| XQ-D20 | SQL の `PREPARE` / `EXECUTE` / `DEALLOCATE` / `DISCARD` | 別の名前空間／プロトコルの名前付き文と共通 | **共通の名前空間**。`PREPARE` は Parse と同じ解析と型推論をし、`from_sql = true` の文を作る。`EXECUTE` は一時のポータルで完走する。`DISCARD ALL` はブロック内で 25001、文・ポータル・設定・シーケンスの状態を捨てる（§6.3） | 【実機】SQL の PREPARE した文を Bind でき、Parse した名前を EXECUTE できる。`EXECUTE` の Describe は対象の文の列を返す |
| XQ-D21 | Describe（文）の再解析 | 古くても保存した値を返す／再解析して検査 | **世代の鍵が変わっていたら、ロックを取って再解析し、結果の列の（型 OID, typmod）が保存したものと違えば `0A000 cached plan must not change result type`** | 【実機】`SELECT *` の文を Describe した後で列を足すと Describe も Bind もこのエラー。列を元に戻すと通る。比較するのは名前ではなく型と typmod（PG の `equalRowTypes`。【記憶】） |
| XQ-D22 | エラーの位置 | 実行時の文のテキスト／Parse のときの SQL のテキスト | **Session が `PreparedStmt.sql` で `Error::resolve_position` してから返す**（Bind・Execute で出るエラーも Parse のときの SQL に対する位置） | 【実機】再解析で出る `42P01` の `P=15` は Parse のときの SQL の位置 |
| XQ-D23 | Bind のパラメータ変換の失敗の CONTEXT | 付けない／PostgreSQL と同じ | **`W`（context）に `unnamed portal parameter $1 = '...'`（名前付きは `portal "p" parameter $1 = '...'`）を付ける。値は伏せる** | 【実機】PG 17 の既定（`log_parameter_max_length_on_error = 0`）と同じ |
| XQ-D24 | ドライバの検証 | 実機の接続だけ／スクリプト化して PG と yuzhu で比べる | **同じシナリオを PostgreSQL 17 と yuzhu の両方に流して結果を比べる（`tests/compat/drivers/`、TS が持つ共通部品の上に XQ が各ドライバのシナリオを書く）。tokio-postgres は `postgres` クレート（dev-dependency に既存）で `cargo test` に入れる** | CLAUDE.md「PostgreSQL が正解」。ドライバが何を送るかは実機で確かめるのが確実 |

---

## 1. 範囲

**対応するもの**

- メッセージ `P`・`B`・`D`・`E`・`C`・`S`・`H` の受信と応答（§3）。名前付き・無名の文とポータル。エラー後の Sync までの読み捨て。パイプライン（Sync を挟まず複数の文を送る）。`Q` との混在。
- 暗黙のトランザクション（Sync で閉じる）、明示的なブロック（BEGIN〜COMMIT）、失敗したブロック（25P02）の中での挙動。
- 行数制限と `PortalSuspended`。複数のポータルの同時の中断。中断中のポータルのスナップショットの登録（D11）。`EmptyQueryResponse`。コマンドタグ。`RETURNING`（M4 の欄を RW-6 が仕上げる。この章は返し方を決める）。
- パラメータ型の推論（`$n`、Parse の宣言、42P18・42P08・42P02）と、準備済み文のカタログ世代による再解析（D19、D21）。
- バイナリ形式の振り分け（`types/binary.rs`）と M1 の型の send / recv。結果の列ごとの形式指定（0 個、1 個、N 個）。M5 で足す型（numeric・日付時刻・bytea・uuid・bpchar・配列）の実体は 05・06 章が持つ（この章は口と表だけ）。
- SQL の `PREPARE` / `EXECUTE` / `DEALLOCATE [PREPARE] {name | ALL}` / `DISCARD {ALL | PLANS | SEQUENCES | TEMP}`。
- ParameterStatus を Sync で送る規則。
- **XQ-6（任意。カットライン上位）**: `COPY ... TO STDOUT`（`COPY (query) TO STDOUT` を含む）、CSV 形式（FROM と TO）、Extended 経由の COPY（IN と OUT）。
- ドライバの検証（tokio-postgres、psycopg 3、pgJDBC、node-postgres）の手順とシナリオ。

**対応しない（実行すると 0A000 か、PostgreSQL と同じエラー）**

| 項目 | 結果 |
|---|---|
| SQL のカーソル（`DECLARE` / `FETCH` / `MOVE` / `CLOSE cursor`）、`WITH HOLD` | 構文エラー（M1 のパーサが知らない文）。psycopg 3 の名前付きカーソルと `cursor.stream()` は M6 |
| `F`（FunctionCall） | `0A000 function call is not supported`。ReadyForQuery を返して続行（XQ-D18） |
| COPY のバイナリ形式（`FORMAT binary`） | `0A000`（M6）。`FORMAT csv` は XQ-6 |
| 汎用プラン、`plan_cache_mode` の効果 | 設定は保存するだけ（§3.6 の 00）。常に Bind ごとに計画する |
| `pg_prepared_statements` | M6（XQ-D14） |
| Parse の `$n` が 65535 を超える文 | `42601 parameter number too large`（XQ-D16） |
| SQL の `PREPARE` の本文に SELECT・INSERT・UPDATE・DELETE・VALUES 以外 | 構文エラー（PostgreSQL と同じ。`MERGE` は M4 でも `0A000`） |
| `EXECUTE` の引数に副問い合わせ・集約・列参照 | `0A000` / `42803` / `42703`（§6.3.3） |

**この章が保証すること**

1. 上の対応範囲で、メッセージ列（どのメッセージが何回、どの順で返るか）が PostgreSQL 17 と一致する。差が出る点は §10 の確認事項と、10 章の「既知の差」の一覧に載せる（M5-XQ-Q3〜Q6）。
2. エラーが起きた文の副作用は取り消され、Sync まで後続のメッセージは実行されない。暗黙のトランザクションの中の先行する文も取り消される。
3. 中断中のポータルが生きている間、そのスナップショットが `TxnManager` に登録されていて VACUUM の horizon を止める（D11）。ポータルはトランザクションの終わりで必ず破棄され、登録が外れる。バッファのピン・ページのラッチ・コミットゲートは Execute をまたいで持たない（M5 規約 1、`assert_no_pins` が毎回通る）。
4. 同じ準備済み文は、DDL の後も、結果の列の型が変わらない限り使い続けられる。変わるなら `0A000`。
5. Simple Query の動作（M1〜M4 の slt）は変わらない。

---

## 2. 構成

`★` は M5 で新規、`△` は変更。括弧内は WP。

```
impl/rust/crates/
├── yuzhu-core/src/
│   ├── sql/
│   │   ├── lexer.rs                 △ `$n` → Token::Param(u16)（XQ-3）。`$` の後が数字以外なら M1 のドルクォート / 構文エラーのまま
│   │   ├── ast.rs                   △ Expr::Param { index: u16, span }（XQ-3）。PrepareStmt / ExecuteStmt / DeallocateStmt / DiscardStmt の中身（XQ-5。変種は F0 が作る）
│   │   └── parser/prepare.rs        ★ PREPARE / EXECUTE / DEALLOCATE / DISCARD（XQ-5）。expr.rs に `$n` の 1 分岐（XQ-3）
│   ├── expr/mod.rs                  △ ExprKind::ExternParam(u16)（XQ-3。D45）。walk.rs・deparse・eval・planner の網羅 match を直す
│   ├── analyzer/
│   │   ├── params.rs                ★ ParamCtx、analyze_with_params、analyze_execute_param（XQ-3）
│   │   ├── mod.rs                   △ Analyzer に params 欄（XQ-3）。analyze() は変えない
│   │   ├── coerce.rs resolve.rs expr.rs select.rs dml.rs   △ フック点（§6.4.2 の 6 か所）。M4 / RW のファイルに分岐を足すだけ
│   ├── types/
│   │   ├── binary.rs                ★ RecvBuf、振り分け表、M1 の型の send / recv（XQ-4）。numeric・日付時刻・bytea・uuid・配列の本体は 05・06 章の各モジュールが提供し、ここの表から呼ぶ
│   │   └── io.rs                    TY が持つ。XQ は触らない（`encode_value` は session/extended.rs に置く）
│   ├── session/
│   │   ├── extended.rs              ★ ExtStore、Portal、msg_* の実体、行の符号化、COPY の入口（XQ-2、XQ-6）
│   │   ├── prepared.rs              ★ PreparedStmt、解析の鍵、再解析、PREPARE / EXECUTE / DEALLOCATE / DISCARD（XQ-5）
│   │   └── mod.rs                   △ `Session.ext: ExtStore`、commit / rollback の 1 行（`ext.end_of_transaction()`）、execute_simple の先頭の 1 行（無名の文とポータルの破棄）。F0 が口を作り XQ が足す
│   └── copy/                        M4 の持ち物。XQ-6 は `to.rs`・`csv.rs` を新規に足し、`mod.rs`・`from.rs` には CSV の分岐と Extended の入口を足すだけ（§11 C13）
└── yuzhu-server/
    ├── src/connection.rs            △ メッセージループ（ExtState、flush、COPY の受信ループ。XQ-1）。起動処理の認証は AU-2 が足す
    ├── src/protocol/messages.rs     △ FrontendMessage に Parse / Bind / Describe / Execute / Close / Flush / CopyData / CopyDone / CopyFail（XQ-1）
    ├── src/protocol/codec.rs        △ 上の本体の復号、ParameterDescription・NoData・ParseComplete・BindComplete・CloseComplete・PortalSuspended・CopyOut 系の符号化、DataRow のバイナリ版（XQ-1）
    └── tests/extended.rs            ★ 生プロトコルのテスト（§7.3。XQ-1、XQ-2、XQ-5）
```

**依存の方向**: `session::extended` / `session::prepared` → `analyzer`（`analyze_with_params`）、`planner`、`executor`、`types::binary`、`txn`（`take_snapshot`）、`session::locking`（LK-4）。`analyzer::params` → `types`、`catalog` だけ。`types::binary` → `types::{numeric, datetime, bytea, uuid, array, ...}`（各型のモジュールの `binary_send` / `binary_recv` を呼ぶ）。`yuzhu-server` は `Session` の公開 API と `ResultSink` だけを使う。

**この章の規約**

1. **Session の `msg_*` は、Err を返す前に自分でトランザクションの状態を直す**（暗黙 → 中断して Idle、ブロック → 中断して Failed）。server は ErrorResponse を送ってから `msg_abort_implicit()` を呼ぶが、これは冪等で、server 自身が見つけたエラー（メッセージ本体の不正など）の後始末のためにある（§5.1）。
2. **Execute をまたいで持ってよいのは、`BoxedExecutor`・`Arc<PhysicalQuery>`・束縛済みの値・`RegisteredSnapshot`・実行器の所有する状態（部分結果）だけ**。ピン・ラッチ・`&` 参照・`ExecCtx` は持たない。`ExecCtx` は Execute ごとに作り直す（§5.9）。
3. **エラーの位置は Session が Parse のときの SQL に対して解決してから返す**（XQ-D22）。
4. **型推論の分岐は `ExprKind::ExternParam` と `Expr.ty == UNKNOWN` の組み合わせだけを見る**。文字列リテラルの unknown と同じ道を通し、`$n` 専用の演算子・関数の解決を作らない。
5. server の復号は**バイト列のまま**パラメータを Session に渡す（text のパラメータの UTF-8 検査は `input_text` の前に Session が行う。22021）。

---

## 3. ワイヤ形式（バイト単位）と例

この章にディスク形式はない。代わりに、Extended Query のメッセージを書く。整数はビッグエンディアン。`cstring` は NUL 終端。**メッセージの長さは自分自身の 4 バイトを含み、型バイトを含まない**。

### 3.1 フロントエンド → バックエンド

| 型 | 名前 | 本体 | 復号の検査（違反は `ERROR 08P01 invalid message format`。FATAL にしない。Sync まで読み捨て） |
|---|---|---|---|
| `P` | Parse | 文の名前 cstring、問い合わせ cstring、`u16` n（宣言した型の数）、`u32` × n（型の OID。0 = 未指定） | 本体を使い切る。名前と問い合わせは UTF-8（違反は 22021） |
| `B` | Bind | ポータル名 cstring、文の名前 cstring、`i16` nf（パラメータの形式コードの数）、`i16` × nf、`i16` np（値の数）、各値（`i32` 長さ。−1 = NULL、続いてそのバイト数）、`i16` nr、`i16` × nr（結果の形式コード） | 長さが −1 以外の負なら `08P01 insufficient data left in message`。形式コードは 0 / 1 以外を server が `22023`。末尾の余りは不正 |
| `D` | Describe | `S` か `P`（1 バイト）、名前 cstring | 先頭が `S` `P` 以外なら `08P01 invalid DESCRIBE message subtype N` |
| `E` | Execute | ポータル名 cstring、`i32` 最大行数（0 以下 = 全行） | |
| `C` | Close | `S` か `P`、名前 cstring | `08P01 invalid CLOSE message subtype N` |
| `S` | Sync | なし | |
| `H` | Flush | なし | |
| `d` `c` `f` | CopyData、CopyDone、CopyFail（本体は cstring） | COPY IN の間だけ意味がある（§6.7） | |
| `F` | FunctionCall | 本体は読み捨てる | XQ-D18 |

形式コード: 0 = テキスト、1 = バイナリ。**個数は 0（全部テキスト）、1（全部にその 1 つ）、それ以外は値の数と同じでなければならない**。パラメータは `08P01 bind message has N parameter formats but M parameters`。結果は `08P01 bind message has N result formats but query has M columns`。Bind の値の数が文の型の数と違えば `08P01 bind message supplies N parameters, but prepared statement "s" requires M`（無名の文は `""`）【実機】。

### 3.2 バックエンド → フロントエンド（Extended Query で使うもの）

| 型 | 名前 | 本体 |
|---|---|---|
| `1` `2` `3` | ParseComplete、BindComplete、CloseComplete | なし |
| `t` | ParameterDescription | `i16` n、`u32` × n（型の OID） |
| `T` | RowDescription | M1 のまま。**形式コードの欄は列ごとの実際の形式**（Describe（文）では常に 0。Describe（ポータル）では Bind の結果形式）【実機】 |
| `n` | NoData | なし |
| `D` | DataRow | M1 のまま。バイナリの列は send 関数の出力。NULL は長さ −1 |
| `C` | CommandComplete | タグ |
| `I` | EmptyQueryResponse | なし |
| `s` | PortalSuspended | なし |
| `G` `H` `d` `c` | CopyInResponse、CopyOutResponse、CopyData、CopyDone | §6.7 |
| `S` | ParameterStatus | M1 のまま |
| `N` `E` | NoticeResponse、ErrorResponse | M1 のまま。`W`（context）を使う（XQ-D23） |
| `Z` | ReadyForQuery | **Sync への応答と、`Q`・`F` への応答だけ** |

### 3.3 例（そのまま単体テストの固定値にできる）

```text
Parse(name="s1", sql="SELECT $1+1", types=[23]):
  50 00 00 00 19  73 31 00  53 45 4c 45 43 54 20 24 31 2b 31 00  00 01  00 00 00 17
Bind(portal="", stmt="s1", param formats=[1], params=[int4 41], result formats=[1]):
  42 00 00 00 1a  00  73 31 00  00 01 00 01  00 01  00 00 00 04 00 00 00 29  00 01 00 01
Describe(S, "s1"):   44 00 00 00 08  53 73 31 00
Execute("", 0):      45 00 00 00 09  00 00 00 00 00
Close(P, "p1"):      43 00 00 00 08  50 70 31 00
Sync:                53 00 00 00 04
Flush:               48 00 00 00 04
ParameterDescription([23]):                   74 00 00 00 0a  00 01 00 00 00 17
RowDescription([?column? int4, binary]):      54 00 00 00 21  00 01  3f 63 6f 6c 75 6d 6e 3f 00  00 00 00 00  00 00  00 00 00 17  00 04  ff ff ff ff  00 01
DataRow([int4 42, binary]):                   44 00 00 00 0e  00 01  00 00 00 04 00 00 00 2a
CommandComplete("SELECT 1"):                  43 00 00 00 0d  53 45 4c 45 43 54 20 31 00
ParseComplete 31 00 00 00 04 / BindComplete 32 00 00 00 04 / CloseComplete 33 00 00 00 04
NoData 6e 00 00 00 04 / PortalSuspended 73 00 00 00 04 / EmptyQueryResponse 49 00 00 00 04
```

RowDescription の `?column?` の行は「名前 `3f 63 .. 3f 00`、table OID `00 00 00 00`、attnum `00 00`、型 OID `00 00 00 17`、型の長さ `00 04`、typmod `ff ff ff ff`、形式 `00 01`」。

---

## 4. 共通の型（契約）

00 §4.7・§4.9・§4.12 の再掲に、この章が足すものを加える。**足したものには ★**。00 と食い違うところは §11 に書いた。

### 4.1 session（公開 API。`session/extended.rs`、`session/mod.rs`）

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format { Text = 0, Binary = 1 }
pub struct ColumnDesc { /* M2 の項目 */ pub format: Format }       // M1〜M4 は常に Text

pub struct StatementDescription { pub param_types: Vec<Oid>, pub columns: Option<Vec<ColumnDesc>> }   // None = NoData
pub enum ExecOutcome {
    Complete(String /* コマンドタグ */),
    Suspended,
    EmptyQuery,
    CopyIn,          // ★ COPY FROM STDIN を始めた（CopyInResponse は送信済み。XQ-6）。server は COPY 受信ループに入る
}

impl Session {
    pub fn msg_parse(&mut self, name: &str, sql: &str, param_types: &[Oid]) -> Result<()>;
    pub fn msg_bind(&mut self, portal: &str, stmt: &str, param_formats: &[Format],
                    params: &[Option<&[u8]>], result_formats: &[Format]) -> Result<()>;
    pub fn msg_describe_statement(&mut self, name: &str) -> Result<StatementDescription>;
    pub fn msg_describe_portal(&mut self, name: &str) -> Result<Option<Vec<ColumnDesc>>>;
    pub fn msg_execute(&mut self, portal: &str, max_rows: u32 /* 0 = 全行 */, sink: &mut dyn ResultSink) -> Result<ExecOutcome>;
    pub fn msg_close_statement(&mut self, name: &str);
    pub fn msg_close_portal(&mut self, name: &str);
    pub fn msg_sync(&mut self) -> Result<()>;
    pub fn msg_abort_implicit(&mut self);
    /// ★ Sync の処理の最後（ReadyForQuery の直前）に server が呼ぶ。前回送った値から変わった ParameterStatus を返し、送ったものとして記録する
    pub fn pending_parameter_status(&mut self) -> Vec<(String, String)>;
}

pub trait ResultSink {
    // 既存（M4 の copy_in_response を含む）はそのまま
    fn data_row_raw(&mut self, values: &[Option<Vec<u8>>]) -> std::io::Result<()>;             // 00 §4.7
    // ★ XQ-6（COPY TO）。既定の実装は Err(Unsupported)
    fn copy_out_response(&mut self, format: u8, column_formats: &[u8]) -> std::io::Result<()>;
    fn copy_out_data(&mut self, chunk: &[u8]) -> std::io::Result<()>;
    fn copy_out_done(&mut self) -> std::io::Result<()>;
}
```

- `msg_*` は `Result` の `Err` を「そのメッセージの ErrorResponse」として返す。Err の `position` は解決済み（XQ-D22）。クライアントへの書き込み失敗は `Error`（SQLSTATE `08006`、`Severity::Fatal`）にして返す。server はこれを受けたら ErrorResponse を試さず接続を閉じる。
- `msg_close_*` は存在しない名前でも成功し、失敗したブロックの中でも許される（【実機】）。
- `msg_execute` の `max_rows` は 0 が全行。i32 の負の値は server が 0 にする（XQ-D17）。

### 4.2 session（内部。`session/prepared.rs`、`session/extended.rs`）

```rust
// prepared.rs
pub(crate) struct PreparedStmt {
    pub name: String,                           // 無名は ""
    pub sql: Arc<str>,                          // エラーの位置の解決に使う（XQ-D22）
    pub stmt: Option<Arc<Statement>>,           // None = 空の問い合わせ
    pub kind: StmtKind,
    pub declared: Vec<Oid>,                     // Parse が宣言した型（0 = 未指定）
    pub param_types: Vec<Oid>,                  // 確定した型。ParameterDescription。unknown（705）を含まない
    pub columns: Option<Arc<[ColumnDesc]>>,     // 結果の列（format = Text）。None = NoData
    pub analysis: Option<Analysis>,             // kind が Query / Dml / Explain のときだけ Some
    pub from_sql: bool,                         // SQL の PREPARE で作った
    pub created_at: i64,                        // PostgreSQL のエポックからのマイクロ秒
}
pub(crate) struct Analysis { pub key: AnalysisKey, pub bound: Arc<BoundStatement>, pub volatile: bool }
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct AnalysisKey { pub generation: u64, pub search_path: Vec<String>, pub datestyle: String, pub timezone: String }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StmtKind {
    Empty,
    Query,                       // SELECT / VALUES（FOR UPDATE などを含む）
    Dml { returning: bool },     // INSERT / UPDATE / DELETE
    Ddl,                         // CREATE TABLE / DROP TABLE / TRUNCATE ...（LOCK TABLE、VACUUM、ロール、データベースを含む）。Parse では解析しない
    Copy,                        // Parse では解析しない
    Explain,                     // 内側の文を Parse で解析する（EXPLAIN ANALYZE は Execute で実行する）
    TxExit,                      // COMMIT / END / ROLLBACK / ABORT（失敗したブロックの中でも許される）
    TxOther,                     // BEGIN / START TRANSACTION / SAVEPOINT 系 / SET TRANSACTION
    Utility { returns_rows: bool }, // SET / RESET / SHOW / CHECKPOINT / PREPARE / EXECUTE / DEALLOCATE / DISCARD
}
impl StmtKind { pub(crate) fn of(stmt: &Statement) -> StmtKind; pub(crate) fn strategy(self) -> PortalStrategy; }

// extended.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PortalStrategy { OneSelect, OneReturning, UtilSelect, MultiQuery, Empty }

pub(crate) struct ExtStore {
    stmts: HashMap<String, Arc<PreparedStmt>>,     // 名前付き（SQL の PREPARE と共通）
    unnamed: Option<Arc<PreparedStmt>>,
    portals: HashMap<String, Portal>,
    unnamed_portal: Option<Portal>,
    batch: BatchState,                             // 最後の Sync から後の状況
}
pub(crate) struct BatchState { pub open: bool, pub executed: u32 }   // open: 拡張メッセージを処理中。executed: 完了した Execute の数（パイプラインの判定）
impl ExtStore {
    pub(crate) fn end_of_transaction(&mut self);   // すべてのポータルを破棄（RegisteredSnapshot・実行器が外れる）
}

pub(crate) struct Portal {
    pub name: String,
    pub stmt: Arc<PreparedStmt>,
    pub txn_serial: u64,                           // 作ったトランザクションの通し番号。違えば無効（COMMIT を実行したポータル自身の再登録を防ぐ）
    pub strategy: PortalStrategy,
    pub params: Vec<Datum>,                        // 束縛済み。ExprKind::ExternParam(i) が読む
    pub result_formats: Vec<Format>,               // 列数に展開済み
    pub columns: Option<Vec<ColumnDesc>>,          // format 入り。None = NoData
    pub out_types: Vec<SqlType>,                   // unknown は text
    pub plan: Option<Arc<PhysicalQuery>>,          // Query / Dml のとき
    pub snapshot: Option<StatementSnapshot>,       // OneSelect のときは Bind で Some。型は 02 §4.5 の StatementSnapshot { snap, registered }（独自の PortalSnapshot は作らない。R-03）
    pub run: PortalRun,
}
// StatementSnapshot（02 §4.5）: Read Committed は registered = Some（ポータルを閉じるまで登録を保つ）、Repeatable Read は registered = None（xact_snapshot が登録を持つ）
pub(crate) enum PortalRun {
    Ready,                                         // 未実行
    Select(Box<ActiveSelect>),                     // OneSelect の途中（Suspended）
    Stored(StoredRows),                            // OneReturning / UtilSelect: 整形済みの行と、返した位置
    AtEnd,                                         // 全行を返し終えた（再実行は 0 行。OneSelect / OneReturning / UtilSelect）
    Done,                                          // MultiQuery の完了（再実行は 55000）
    Failed,                                        // エラーで落ちた（ブロックの中で、トランザクションの終わりまで残る）
}
pub(crate) struct ActiveSelect {
    pub exec: BoxedExecutor,
    pub state: ExecState,                          // ExecCtx の所有する状態（params・mem・subplans・ctes）。Execute ごとに ExecCtx へ移し、戻す（§5.9）
}
pub(crate) struct StoredRows { pub rows: VecDeque<Vec<Option<Vec<u8>>>>, pub tag_prefix: String /* "INSERT 0 " など */, pub affected: u64 }
```

`ExecState`（`SubPlanStates`・`CteStates`・`MemBudget`・実行時パラメータをまとめた構造体）は M4 の executor が持つ。M4 の `ExecCtx` はこれらを所有する pub フィールドなので、`std::mem::take` で出し入れできる形（`Default` の実装）を M4 の担当に頼む（§11 C6）。

### 4.3 analyzer

```rust
// analyzer/mod.rs（00 §4.7）
pub fn analyze_with_params(stmt: &Statement, catalog: &dyn CatalogReader, declared: &[Oid]) -> Result<(BoundStatement, Vec<Oid>)>;

// analyzer/params.rs ★
pub(crate) struct ParamCtx { /* types: RefCell<Vec<Oid>>、unresolved: RefCell<BTreeMap<(u32 /*span.start*/, u16 /*index*/), ()>> */ }
impl ParamCtx {
    pub(crate) fn new(declared: &[Oid]) -> Result<Self>;                                  // 0 → UNKNOWN。未知の OID は XX000
    /// variable_paramref_hook。`$n` の出現ごとに呼ぶ。戻り値は ExternParam(n-1) の節点の型（解決済みならその型、未解決なら UNKNOWN）
    pub(crate) fn reference(&self, n: u16, span: Span) -> Result<Oid>;
    /// variable_coerce_param_hook。unknown の `$n` を target に強制変換する場面で呼ぶ。型が違えば 42P08 (a)
    pub(crate) fn resolve(&self, index: u16, target: Oid, span: Span) -> Result<()>;
    /// check_variable_parameters と、未確定の検査。(b) → 42P18 の順
    pub(crate) fn finish(self) -> Result<Vec<Oid>>;
}
/// EXECUTE の引数 1 つ。assignment の文脈で target に強制変換し、列参照・集約・副問い合わせを含めば拒否する
pub fn analyze_execute_param(e: &Expr, target: Oid, index: usize, catalog: &dyn CatalogReader) -> Result<BoundExpr>;
```

`ExprKind::ExternParam(u16)` と `Expr::Param { index: u16, span }`（AST。`index` は `$n` の n で 1 始まり）は 00 §4.6・D45 のとおり。型は `Expr.ty`。**解析を終えた木に `ty == UNKNOWN` の `ExternParam` は残らない**（`finish` が保証する）。

### 4.4 types/binary.rs

```rust
pub const BINARY_TRAILING_MSG: &str = "incorrect binary data format";        // 余りがあるときの 22P03 の文言（Bind が " in bind parameter N" を足す）

/// pqformat.c の pq_getmsg* に相当する読み取り器。足りなければ 08P01 "insufficient data left in message"
#[derive(Debug)]
pub struct RecvBuf<'a> { /* buf: &'a [u8], pos: usize */ }
impl<'a> RecvBuf<'a> {
    pub fn new(buf: &'a [u8]) -> Self;
    pub fn remaining(&self) -> usize;
    pub fn get_u8(&mut self) -> Result<u8>;   pub fn get_i16(&mut self) -> Result<i16>;
    pub fn get_i32(&mut self) -> Result<i32>; pub fn get_i64(&mut self) -> Result<i64>;
    pub fn get_f32(&mut self) -> Result<f32>; pub fn get_f64(&mut self) -> Result<f64>;          // ビット列の読み替え
    pub fn get_bytes(&mut self, n: usize) -> Result<&'a [u8]>;
    pub fn get_rest(&mut self) -> &'a [u8];                                                       // text / bytea 用
    /// 呼び出し側（input_binary）が呼ぶ。余りがあれば 22P03 BINARY_TRAILING_MSG
    pub fn finish(&self) -> Result<()>;
}

// 00 §4.9 の署名（変更なし）
pub fn supports_binary(ty: SqlType) -> bool;
pub fn output_binary(d: &Datum, ty: SqlType) -> Result<Vec<u8>>;     // *send。NULL は呼ばない（呼び出し側が None にする）
pub fn input_binary(buf: &[u8], ty: SqlType) -> Result<Datum>;       // *recv + finish()。typmod は −1 として扱う（XQ-D11）

/// 型ごとの実体が実装する関数の型（各モジュールが pub fn binary_send / binary_recv を持ち、振り分け表がこれを引く）
pub(crate) struct BinaryCodec {
    pub send: fn(&Datum, SqlType) -> Result<Vec<u8>>,
    pub recv: fn(&mut RecvBuf<'_>, SqlType) -> Result<Datum>,
}
pub(crate) fn codec(oid: Oid) -> Option<&'static BinaryCodec>;        // 配列は要素の OID から配列の codec を作る（array.rs が codec(elem) を再帰で引く）
```

### 4.5 yuzhu-server

```rust
// protocol/messages.rs
pub enum DescribeTarget { Statement, Portal }
pub enum FrontendMessage {
    Query(String), Terminate, Sync, Flush,
    Parse { name: String, sql: String, param_types: Vec<u32> },
    Bind { portal: String, stmt: String, param_formats: Vec<i16>, params: Vec<Option<Vec<u8>>>, result_formats: Vec<i16> },
    Describe { target: DescribeTarget, name: String },
    Close { target: DescribeTarget, name: String },
    Execute { portal: String, max_rows: i32 },
    CopyData(Vec<u8>), CopyDone, CopyFail(String),
    FunctionCall,
    Unknown(u8),
}
pub enum ProtocolError {
    Io(io::Error), TooLong, /* 既存。フレーミングの不正は FATAL 08P01 */
    /// ★ 本体の不正。ERROR（FATAL ではない）。名前や問い合わせが UTF-8 でないときは sqlstate = "22021"、それ以外は "08P01"
    BadBody { tag: u8, sqlstate: &'static str, message: String },
}

// connection.rs（00 §4.12）
enum ExtState { Normal, SkipTillSync }
```

---

## 5. 処理の流れ

### 5.1 メッセージの受信と応答（完全な表）

yuzhu-server の `message_loop` が 1 メッセージずつ読み、次の表のとおりに Session を呼ぶ。**成功の応答は server が書く**（`msg_*` が値を返し、server が符号化する）。行・Notice・COPY のデータだけは `msg_execute` が `ResultSink` へ直接書く。

| 受信 | Normal のときの処理 | 成功の応答（この順） | 失敗 | flush |
|---|---|---|---|---|
| `P` Parse | `msg_parse` | `ParseComplete` | ErrorResponse。`msg_abort_implicit`。SkipTillSync | しない |
| `B` Bind | `msg_bind`（server が形式コードを `Format` に直す。0・1 以外は 22023） | `BindComplete` | 同上 | しない |
| `D` Describe（`S`） | `msg_describe_statement` | `ParameterDescription`、続けて `RowDescription`（列あり）か `NoData` | 同上 | しない |
| `D` Describe（`P`） | `msg_describe_portal` | `RowDescription`（形式は Bind で指定したもの）か `NoData` | 同上 | しない |
| `E` Execute | `msg_execute(portal, max_rows, sink)`。`max_rows` は i32 を 0 以下 → 0 にして渡す | `DataRow` × 行数（`sink` が書く）、続けて **`Complete(tag)` → `CommandComplete(tag)`、`Suspended` → `PortalSuspended`、`EmptyQuery` → `EmptyQueryResponse`、`CopyIn` → COPY 受信ループへ（§6.7）**。Notice は行の前に出てよい | 同上。**途中まで書いた DataRow はそのまま残る** | しない（CopyIn は `CopyInResponse` の後で flush） |
| `C` Close | `msg_close_statement` / `msg_close_portal`（失敗しない） | `CloseComplete` | なし | しない |
| `S` Sync | `msg_sync`。続けて `pending_parameter_status()` の各項目を `ParameterStatus` で書く | `ReadyForQuery`（状態は `transaction_status()`） | commit の失敗は ErrorResponse の後に `ReadyForQuery`（SkipTillSync にしない） | **する** |
| `H` Flush | 何もしない | なし | なし | **する** |
| `Q` Query | `execute_simple`。`is_copying_in()` なら Simple の COPY 受信ループ（M4） | M1〜M4 のまま。最後に `ReadyForQuery` | 文のエラーは `execute_simple` が送る | **する**（ReadyForQuery の後） |
| `X` Terminate | 接続を閉じる（`session.terminate()`） | なし | なし | — |
| `d` `c` `f` | **COPY の外では黙って無視する**（XQ-D18） | なし | なし | しない |
| `F` FunctionCall | 本体を読み捨てる | **`ErrorResponse 0A000 function call is not supported`、続けて `ReadyForQuery`**（Sync を待たない。SkipTillSync にしない） | — | する |
| それ以外 | — | — | `FATAL 08P01 invalid frontend message type N` で閉じる | する |

**ErrorResponse・NoticeResponse の後は flush する**（XQ-D13。`Sink::error` / `Sink::notice` が `flush()` を呼ぶ）。ほかのメッセージでは `BufWriter` の容量（8KB）を超えたとき以外は flush しない。

**SkipTillSync（エラーの後の読み捨て）**

- Extended のメッセージ（`P` `B` `D` `E` `C`）が ErrorResponse を返したら `ExtState::SkipTillSync` にする。
- この状態では、**`S`（Sync）と `X`（Terminate）以外を読み捨てる。`H`（Flush）も、`Q` も、`F` も、`d` `c` `f` も**【実機】（`P` が構文エラーの後に `Q` を送ると、その `Q` は実行されず、結果も出ない）。読み捨てるメッセージの本体が不正でも何も返さない。
- Sync が来たら Normal に戻り、通常の Sync として処理する（`msg_sync`、ReadyForQuery）。
- `Q`・`F` が自分でエラーになったときは SkipTillSync にしない（`Q` は ReadyForQuery で終わる）。
- **本体の復号に失敗した拡張メッセージ**（`ProtocolError::BadBody`。名前が UTF-8 でない、長さが合わない、Describe の対象が `S` `P` でない、など）も ERROR（08P01 か 22021）として扱う。ErrorResponse の後に `session.msg_abort_implicit()` を呼び、SkipTillSync にする。`Q` の本体が不正なら ErrorResponse と ReadyForQuery（M1 と同じ）。
- フレーミングの不正（長さが 4 未満・上限超過・未知の型バイト）は FATAL 08P01 で閉じる（M1 のまま）。

### 5.2 メッセージループ（疑似コード）

```rust
fn message_loop(reader, writer, session: &mut Session, shared, pid) -> io::Result<()> {
    let mut ext = ExtState::Normal;
    loop {
        if interrupt.is_terminate_requested() { return terminated_by_administrator(writer); }
        let raw = match read_raw(reader, shared.max_message_len) {           // 型バイトと本体（長さの検査まで）
            Ok(Some(r)) => r, Ok(None) => return Ok(()),                    // EOF
            Err(ProtocolError::Io(e)) => return io_error(e),                 // 停止要求のときは 57P01
            Err(e) => return send_fatal(writer, "08P01", &e.to_string()),
        };
        if ext == ExtState::SkipTillSync {
            match raw.tag { b'S' => ext = ExtState::Normal, b'X' => return Ok(()), _ => continue }
        }
        let msg = match decode(raw) {                                         // 本体の復号
            Ok(m) => m,
            Err(ProtocolError::BadBody { tag, sqlstate, message }) => {          // InvalidUtf8 も sqlstate = "22021" の BadBody にする
                write_error(writer, "ERROR", sqlstate, &message)?;                 // flush する
                if is_ext_tag(tag) { session.msg_abort_implicit(); ext = ExtState::SkipTillSync; }
                else { ready_for_query(writer, session)?; }                   // Q の本体の不正
                continue;
            }
            Err(e) => return send_fatal(writer, "08P01", &e.to_string()),
        };
        match msg {
            FrontendMessage::Query(sql) => {
                session.execute_simple(&sql, &mut Sink::new(writer))?;
                if session.is_copying_in() { copy_in_loop(.., CopyOrigin::Simple)?; }   // M4。XQ-6 が Extended 版を足す
                if session.is_closing() { return writer.flush(); }
                ready_for_query(writer, session)?;                                       // flush する
            }
            FrontendMessage::Sync => {
                let r = session.msg_sync();
                for (n, v) in session.pending_parameter_status() { write_parameter_status(writer, &n, &v)?; }
                if let Err(e) = r { write_core_error(writer, &e, severity_str(e.severity))?; }
                if session.is_closing() { return writer.flush(); }
                ready_for_query(writer, session)?;
            }
            FrontendMessage::Flush => writer.flush()?,
            FrontendMessage::Terminate => return Ok(()),
            FrontendMessage::CopyData(_) | CopyDone | CopyFail(_) => {}                  // COPY の外は無視
            FrontendMessage::FunctionCall => { write_error(.., "0A000", "function call is not supported")?; ready_for_query(..)?; }
            FrontendMessage::Unknown(tag) => return send_fatal(writer, "08P01", &format!("invalid frontend message type {tag}")),
            m => {                                                                       // Parse / Bind / Describe / Execute / Close
                match handle_ext(session, m, writer)? {
                    Ok(Next::Continue) => {}
                    Ok(Next::CopyIn) => match copy_in_loop(.., CopyOrigin::Extended)? {  // §6.7
                        CopyEnd::Done => {}                                              // CommandComplete は書き済み。次の Sync を待つ
                        CopyEnd::Failed => ext = ExtState::SkipTillSync,                 // ErrorResponse は書き済み
                        CopyEnd::Closed => return Ok(()),
                    },
                    Err(e) => {                                                          // Session の Err。書き込みの失敗（io）は `handle_ext(..)?` が返す
                        write_core_error(writer, &e, severity_str(e.severity))?;         // flush する
                        session.msg_abort_implicit();
                        if session.is_closing() { return writer.flush(); }
                        ext = ExtState::SkipTillSync;
                    }
                }
            }
        }
    }
}
```

`handle_ext` は 1 メッセージずつ Session を呼び、§5.1 の表の「成功の応答」を書く。たとえば Describe（文）は `StatementDescription` から `ParameterDescription` を書き、`columns` が `Some` なら `RowDescription`（形式は全部 Text）、`None` なら `NoData`。Execute は `Complete(tag)` なら `CommandComplete(tag)` を書く。**`msg_execute` が返した Err のとき、`DataRow` は書き済みでも取り消さない**。

### 5.3 暗黙のトランザクションと Sync

Session の内部状態 `TxState`（M3）のうち `Implicit` を、1 つの `Q` から **Sync までの複数メッセージ**に広げる。

| 状態 | `P` `B` `D` `E` を受けたとき | `S`（`msg_sync`） | メッセージがエラーになったとき | `Q` を受けたとき |
|---|---|---|---|---|
| Idle | `begin_transaction(Implicit)` してから処理する（`msg_close_*` は開始しない） | 何もしない（ReadyForQuery `I`） | — | `execute_simple` が `Implicit` を開始（M3 のまま） |
| Implicit | そのまま処理する。`BEGIN` を Execute すると Block になる（トランザクションは同じ。`txn_serial` は変わらない） | **コミットして Idle**。ポータルは全部消える。失敗は `Err`（Panic はクラスタを poison） | 中断して Idle。ポータルは全部消える | 引き継ぐ。`Q` の文を同じトランザクションで実行し、**`Q` の終わりでコミット**して Idle（ReadyForQuery は `Q` の分。後の `S` でもう 1 回） |
| Block | そのまま処理する | 何もしない（`T`） | 中断して **Failed**。ポータルはトランザクションの終わりまで残るが使えない（Execute は 25P02） | M3 のまま |
| Failed | §5.7 の表 | 何もしない（`E`） | — | M3 のまま（`ROLLBACK` / `COMMIT` 以外は 25P02） |

- **ポータルはトランザクションが終わるたびに全部破棄する**（`COMMIT`・`ROLLBACK`・Sync によるコミット・エラーによる中断）。Session は `commit_transaction` と `rollback_transaction` の先頭で `ext.end_of_transaction()` を呼ぶ（§6.2.1。RegisteredSnapshot と `BoxedExecutor` がここで外れる）。**ステートメント（準備済み文）は破棄しない**（セッションの寿命）。
- **`BEGIN` を実行したポータルが同じトランザクションの続きで生きる**ことと、**`COMMIT` を実行したポータル自身が破棄の対象にならない**ことのため、ポータルは `txn_serial`（`begin_transaction` ごとに増える通し番号。`BEGIN` による Implicit → Block では増えない）を持つ。実行後にポータルを戻すときに `txn_serial` が一致しなければ捨てる。
- `msg_sync` は `batch = BatchState::default()` に戻す（`executed` を 0 に）。
- キャンセル（`InterruptFlag`）は、バッチの最初のメッセージ（`batch.open == false` のとき）の先頭で `clear_cancel()`（アイドル中に届いたキャンセルを捨てる。M3 §5.2 の手順 0 と同じ）。`statement_timeout` は **Parse・Bind・Execute それぞれの 1 メッセージごと**に数える（M5-XQ-Q5）。
- `idle_in_transaction_session_timeout` は `Block` と `Failed` だけに効く（`Implicit` は `None`。M3 §5.10 の `idle_timeout()` のまま）。

### 5.4 Parse

```
msg_parse(name, sql, declared):
  0. バッチの開始（clear_cancel）。begin_implicit_if_idle()。statement_timeout の期限を設定
  1. name == "" なら無名の文を捨てる（失敗しても捨てたまま。【実機】失敗した Parse の後の Describe(S, "") は
     `26000 unnamed prepared statement does not exist`）。名前付きで既にあれば 42P05 `prepared statement "s" already exists`
  2. sql::parse(sql)。Err はそのまま（42601 など。位置は sql に対して）。0 文 → kind = Empty、2 文以上 →
     42601 `cannot insert multiple commands into a prepared statement`
  3. kind = StmtKind::of(stmt)。TxState::Failed で kind が TxExit 以外 → 25P02 （【実機】構文エラーが先。解析の前）
  3'. self.note_snapshot_use(&stmt)（02 §4.5。スナップショットを要する文なら「最初のスナップショットを取った」印だけ立てる。RW-D11、RW-K7。
      PostgreSQL の Parse は解析の前に RR の最初のスナップショットを取る。yuzhu は印だけで、実体は Bind が 0' の順序で作る）
  4. 解析（kind が Query / Dml / Explain のとき。**DDL・COPY・ユーティリティ文は Parse では解析しない**。PostgreSQL と同じく、名前の解決のエラーは Execute で出る）:
       catalog = StatementCatalog { snapshot: take_snapshot で取ったカタログ用スナップショット（D12。登録する。解析が終わったら Drop。R-02）, gen_at_snapshot: db.cache.generation(), .. }
       (bound, types) = analyze_with_params(stmt, &catalog, declared)?          // §6.4。42P18 / 42P08 / 42P02 はここで出る
     解析しない文（Ddl・Copy・Utility・Tx*）はパラメータを持てない（`$n` は構文エラーか、Execute の解析で 42P02）。ParameterDescription は宣言の型だけ（宣言が 0 のものは 42P18）
     宣言の個数が使われた最大の n より多ければ、そのまま param_types に入る（ParameterDescription は宣言の数まで）
  5. columns:
       Query → bound.columns。Dml → returning の列があれば。Explain → [QUERY PLAN text]
       Utility（returns_rows）→ utility_columns(stmt)（SHOW → 1 列（名前は設定名、text）、SHOW ALL → name / setting / description、
       EXECUTE → 対象の文の columns を Describe / Bind のときに引く）
  6. analysis: kind が Query / Dml / Explain なら Some(Analysis { key: 現在の鍵, bound: Arc::new(bound), volatile: txn.catalog_dirty })
  7. PreparedStmt を作って stmts（名前付き）か unnamed に入れる
  Err のとき: 作らない。Block なら Failed。Implicit なら中断
```

Parse は**リレーションロックを取らない**（D21）。解析に使うカタログ用スナップショットは文ごとに `take_snapshot` で取り直す（D12。登録する。R-02）。

### 5.5 Bind（RR の最初のスナップショット → ロック → 世代の比較 → 再解析 → スナップショット）

```
msg_bind(portal_name, stmt_name, param_formats, params, result_formats):
  0. begin_implicit_if_idle()
  1. stmt = 名前で引く。無ければ 26000 `prepared statement "x" does not exist`
     （無名は `unnamed prepared statement does not exist`）
  2. Failed のとき: stmt.kind == TxExit && params.is_empty() のとき以外は 25P02（【実機】SET も、パラメータなしの SELECT も 25P02）
  3. 個数の検査: param_formats.len() ∈ {0, 1, params.len()}（違えば 08P01）、params.len() == stmt.param_types.len()（違えば 08P01）
  4. ポータル名: 名前付きで既にあれば 42P03 `cursor "p" already exists`（【実機】文言は portal ではなく cursor）。
     無名なら既存の無名ポータルを破棄（active でなければ。Execute 中の再入は起きない）
  5. パラメータの変換（§6.5.3）→ params: Vec<Datum>。失敗は CONTEXT つき（XQ-D23）
  5'. **Repeatable Read の最初のスナップショット（ロックの前。02 RW-D10、01 LK-D27 §5.9 手順 0'。レビュー対応 R-03）**:
      kind が Query / Dml / Explain（計画する文）で、`isolation == RepeatableRead` なら `self.statement_snapshot()`（02 §4.5）を呼んで
      `txn.xact_snapshot` を（なければ）作る。**手順 6 のロックより前**に行う。DML のポータルでも Bind で作る（実体の取得は Execute。
      PostgreSQL の `exec_bind_message` も、計画する文では Bind の先頭でスナップショットを取ってから `GetCachedPlan`（ロック）に入る）。
      Read Committed は何もしない（RC のスナップショットは手順 9 / Execute で、ロックの後）
  6. 再解析ループ（kind が Query / Dml / Explain のとき。それ以外は何もしない）:
       loop {
         lock_statement_relations(stmt)        // 生のパース木から集めて 解決 → ロック → 再解決（LK-4。待つときは WaitCtl。C7）。名前の解決は毎回行う（カタログのキャッシュが効く）
         g = db.cache.generation()
         if analysis が有効（key == 現在の鍵(g)、!volatile、!txn.catalog_dirty）: break
         catalog = take_snapshot で取ったカタログ用スナップショット（登録。再解析が終わったら Drop）で StatementCatalog を作る
         (bound, types) = analyze_with_params(stmt.stmt, &catalog, &stmt.param_types)?       // 宣言は確定済みの型。推論は動かない
         新しい columns と stmt.columns を比べる（型 OID と typmod。名前は見ない）→ 違えば 0A000 `cached plan must not change result type`
         新しい Analysis（key = (g, ...)、volatile = txn.catalog_dirty）で stmt を差し替える（Arc<PreparedStmt> を作り直して map に戻す）
         // 再解析で参照するリレーションが変わっているかもしれない。ロックを取り直す（取ったロックは外さない）
         check_interrupts()
       }
  7. 結果の形式: ncols = columns の個数。result_formats.len() ∈ {0, 1, ncols}（違えば 08P01）→ ncols 個に展開
  8. 計画（Query / Dml）: planner::plan(&bound, &PlanEnv { .. }) → Arc<PhysicalQuery>（Bind ごと。汎用プランは作らない）
  9. スナップショット（strategy が OneSelect のときだけ。XQ-D3）: **`self.statement_snapshot()`（02 §4.5。独自の関数は作らない）** を呼び、返った
       `StatementSnapshot` をポータルに持たせる。Read Committed は `take_snapshot(txn.xid, txn.cid)`（登録。ポータルを閉じるまで保持。**ロックの後**）、
       Repeatable Read は 5' で作った `xact_snapshot` の写し（`RegisteredSnapshot::for_statement`。`registered = None`）
       FOR UPDATE などを含む SELECT は txn.xid が None なら assign_xid（00 §5.1 d.3）。読み取り専用のトランザクションでは 25006（RW-D12。解析後）
       「最初のスナップショットを取った」の印は Parse の `note_snapshot_use`（3'）と `statement_snapshot` が立てる（**RW-D11 の規則**。`SELECT 1` も立てる。
       M3 の「FROM のない SELECT は立てない」は使わない。R-03）
 10. Portal を作り、map に入れる。BindComplete
```

- **ロックは Bind で取る**（D21）。Parse のロックは Sync で解放されうるので意味がない。再解析ループの「ロックを取ってから世代を読む」順序は、PostgreSQL の `AcquireExecutorLocks` → `is_valid` の確認と同じで、**ロックを持っている間は他のセッションの DDL（AccessExclusive）が進まない**ので、世代が一致すれば、実行まで解析結果が有効（DDL は待つ）。
- 無効な解析で再解析を繰り返す回数に上限は置かない（各周で DDL が割り込む必要があり、実害のある回数にならない）。`check_interrupts` で中断できる。

### 5.6 Describe、Close、Sync

**Describe（文）** `msg_describe_statement(name)`:
1. 文を引く（26000）。Failed のとき、`columns` が `Some` で kind が TxExit でなければ 25P02（【実機】INSERT・ROLLBACK の Describe は成功し、SELECT の Describe は 25P02）。
2. 再解析が必要なら（Bind の手順 6 と同じ判定）ロックを取って再解析し、結果の列を比べる。違えば 0A000 `cached plan must not change result type`。（【実機】PG はこの前に `ParameterDescription` を送ってからエラーを送る。yuzhu はエラーだけ送る。M5-XQ-Q6）
3. `StatementDescription { param_types, columns }` を返す。`columns` の `format` は常に Text。

**Describe（ポータル）** `msg_describe_portal(name)`: ポータルを引く（34000 `portal "x" does not exist`）。Failed のときは `columns` が `Some` なら 25P02。`columns`（Bind の結果形式が入った）を返す。**Execute の後でも呼べる**（【実機】中断した後の Describe は成功する）。

**Close** `msg_close_statement(name)` / `msg_close_portal(name)`: あれば消す。文を消しても、その文から作ったポータルは影響を受けない（ポータルは `Arc<PreparedStmt>` を持つ）。無名の名前 `""` も対象。

**Sync** `msg_sync()`:
1. `Implicit` なら `commit_transaction()`（失敗は Err。状態は Idle になる）。それ以外は何もしない。
2. `batch` を初期化する。無名の文は残す（【実機】Sync の後も無名の文は使える）。無名のポータルは `Implicit` のコミットで消える。ブロックの中では残る。

### 5.7 失敗したブロック（Failed）の中（【実機】）

| メッセージ | 結果 |
|---|---|
| Parse | 構文エラーが先。それ以外は kind が TxExit（`COMMIT` `END` `ROLLBACK` `ABORT`）なら成功、そうでなければ 25P02（無名の文は捨てられる） |
| Bind | stmt.kind が TxExit かつパラメータ 0 個なら成功。それ以外（`SET` も）は 25P02 |
| Describe（文） | `columns` が `None`（INSERT、`ROLLBACK`、`SET`）なら成功、`Some` なら 25P02 |
| Describe（ポータル） | `columns` が `Some` なら 25P02（【実機】SELECT のポータル）。`None` のポータルは成功（【記憶】Describe（文）と同じ規則） |
| Execute | ポータルが無ければ 34000 が先。あれば、TxExit 以外は 25P02。`COMMIT` を Execute するとタグは `ROLLBACK`（M3 のとおり） |
| Close | 成功（CloseComplete） |
| Sync | ReadyForQuery `E` |

### 5.8 Execute

```
msg_execute(portal_name, max_rows, sink):
  0. begin_implicit_if_idle()。ポータルを map から取り出す（無ければ 34000 `portal "x" does not exist`）。
     portal.txn_serial が現在と違えば破棄して 34000
  1. Failed のとき: stmt.kind が TxExit 以外なら 25P02（ポータルは戻す）
  2. statement_timeout の期限を設定。clear_cancel は呼ばない（バッチの最初だけ）
  3. strategy で分ける（下）
  4. 戻す: 完了した（Suspended 以外の）Execute なら CCI（`txn.command_counter_increment()`。PostgreSQL の exec_execute_message が
     完了のたびに CommandCounterIncrement する）と `batch.executed += 1`。ポータルは txn_serial が同じなら map に戻す。
     Err なら、そのポータルを Failed にして実行器とスナップショットを捨てる（map には残す。ブロックの Failed でも暗黙の中断でも、トランザクションの終わりで全部消える）
  5. statement_timeout を解除
```

| strategy | 処理 |
|---|---|
| `Empty` | `sink` に何も書かず `EmptyQuery` を返す（再実行しても同じ） |
| `OneSelect` | 初回の Execute で `executor::build(&plan.root)` して `PortalRun::Select` に入れる（Bind では作らない）。§5.9 の流れ。1 行ずつ `sink.data_row_raw`。`max_rows` に達したら `Suspended`、尽きたら `Complete("SELECT n")`（n はこの Execute で返した行数）。尽きた後の再実行は `Complete("SELECT 0")`（`AtEnd`） |
| `OneReturning` | 初回に、スナップショット（Read Committed は文用に登録、Repeatable Read は xact_snapshot）を取り、`txn.xid` を割り当て、実行器を最後まで回して RI のキュー（FK-2）を処理し、**行を整形して `StoredRows` に入れる**（メモリは `MemBudget`）。以降は `max_rows` ごとに取り出す。尽きたら `Complete("INSERT 0 n" / "UPDATE n" / "DELETE n")`（n はこの Execute で返した行数。XQ-D4）、残りがあれば `Suspended`。初回に全行を実行済みなので、`Suspended` のまま Sync でコミットしても DML の効果は全部残る（【実機】3 行を 1 行だけ取って Sync しても 3 行入る） |
| `UtilSelect` | `SHOW` / `EXPLAIN` / `EXECUTE`（行を返すもの）。初回に完走して `StoredRows` に入れ、`OneReturning` と同じに返す。タグは `SHOW` / `EXPLAIN` / 内側の文のタグ（`SELECT n`） |
| `MultiQuery` | DML（RETURNING なし）・DDL・COPY（§6.7）・トランザクション制御・SET など。`max_rows` を無視して完走し、`Complete(tag)`。以降は `Done`（再実行は `55000 portal "p" cannot be run`）。DML はスナップショットを Execute のときに取る（Read Committed は登録して文の終わりに外す）。**DDL・COPY・ユーティリティ文（解析済みでないもの）は、Execute のときに §5.1 d の 1〜6（リレーションのロック → XID → スナップショット → 解析 → `ddl::execute` / `copy::begin`）を行う**（`$n` が文中にあれば、この解析で 42P02）。`BEGIN` / `COMMIT` / `ROLLBACK` は M3 の `exec_transaction` を呼ぶ |

- **コマンドタグ**は Simple Query と同じ（M1 §4.2、M4 §14.6。`SELECT n`、`INSERT 0 n`、`UPDATE n`、`DELETE n`、`COPY n`、`CREATE TABLE`、`BEGIN`、`SET`、`SHOW`、`EXPLAIN`、`PREPARE`、`DEALLOCATE` ...）。Extended で違うのは、行を返す文の n が「その Execute で返した行数」（XQ-D4）であること、空の問い合わせが `EmptyQueryResponse`（タグなし）であること。
- **読み取り専用のトランザクションの DML・DDL は Execute の最初（スナップショットと XID の前）に 25006**。FOR UPDATE を含む SELECT は Bind で。
- `VACUUM`・`CREATE DATABASE`・`DROP DATABASE`（`execute_standalone`）は `DdlCtx.outside_block = (state == Implicit && batch.executed == 0)`。ブロックの中なら `25001 X cannot run inside a transaction block`、同じバッチで先に Execute が完了していたら **`25001 X cannot be executed within a pipeline`**（【実機】`SELECT` を Execute した直後の `VACUUM` がこのエラー）。
- **`ResultSink` の書き込み（`io::Error`）が失敗したら**、実行器を捨ててポータルを Failed にし、`Error`（08006、Fatal）を返す。

### 5.9 OneSelect の実行器の保持

実行器（`BoxedExecutor`）は `ExecCtx` を持たず、`next(&mut ctx)` ごとに ctx を受け取る（M1 の規約。M4 でも変わらない）。ポータルは中断をまたいで次のものを持つ。

- `Arc<PhysicalQuery>`（`ExecCtx.query` が指す）、`BoxedExecutor`、`ExecState`（M4 の `ExecCtx` が所有する `params`・`mem`・`subplans`・`ctes`）、束縛済みの `params: Vec<Datum>`（`ExecCtx.bind_params`）、`StatementSnapshot`（`ExecCtx.snapshot`）。

Execute のたびに次を行う。

```
ctx_snapshot = txn_manager.take_snapshot(txn.xid, txn.cid)       // カタログ用（D12）。登録する（Execute の間だけ。終わったら Drop。R-02）
catalog = StatementCatalog { db, snapshot: &ctx_snapshot, gen_at_snapshot: db.cache.generation(), bypass_cache: txn.catalog_dirty, search_path }
info = self.session_info(); runtime = SessionRuntime { .. }; type_env = settings.type_env(zones)
ctx = ExecCtx { catalog: &catalog, storage, indexes, txn: &mut self.txn, snapshot: &portal.snapshot.snap, session: &info,
                runtime: &runtime, interrupts, query: &*plan, params: take(state.params), mem: take(state.mem),
                subplans: take(state.subplans), ctes: take(state.ctes), type_env: &type_env,
                locks, procs, wait, bind_params: &portal.params, xact_snapshot: (RR のとき Some), ri: &mut ri }   // locks・procs は dml.rs の wait_for_xid が使う（00 §4.6。R-05）
loop {
    if max_rows != 0 && sent == max_rows { outcome = Suspended; break }
    ctx.check_interrupts()?;                                       // 行ごと
    match exec.next(&mut ctx)? {
        Some(row) => { sink.data_row_raw(&encode_row(&row[..ncols], &out_types, &formats, &type_env)?)?; sent += 1 }
        None => { outcome = Complete(format!("SELECT {sent}")); break }
    }
}
// 後始末（成功でもエラーでも）
state.params = take(ctx.params); state.mem = take(ctx.mem); state.subplans = take(ctx.subplans); state.ctes = take(ctx.ctes);
buffer::assert_no_pins();                                         // 中断してもピンは持たない
```

- **中断中のポータルが持つもの**: スナップショットの登録（`StatementSnapshot.registered`。Repeatable Read は `xact_snapshot` の登録）、`ExecState` が溜めた中間結果（Sort・HashAggregate・Materialize など）。ページのピン・ラッチ・コミットゲート・`LockManager` の待ちは持たない。**取得済みのリレーションロックは、Bind で取った時点からトランザクションの終わりまで持つ**。
- 実行器が行ごとに引くカタログ（regclass の出力など）は、Execute ごとの `StatementCatalog` を使う。中断中のポータルのリレーションには AccessShare が掛かっている（Bind）ので、DROP されることはない。
- **中断中に他のメッセージ（別のポータルの Execute、Q、BEGIN の中の DML）が走ってよい**。ポータルのスナップショットの `curcid` より後の自分の書き込みは見えない（XQ-D3）。
- ポータルが 2 つ同時に中断していても、それぞれ別の `ExecState` と `RegisteredSnapshot` を持つ。

### 5.10 ParameterStatus と SET

`SET`・`RESET` を Execute した結果の ParameterStatus は、**その Execute の応答には含めない**。Sync で `pending_parameter_status()` が前回送った値との差を返し、server が ReadyForQuery の直前に書く（XQ-D12）。ブロックの中で SET して ROLLBACK で戻った場合も、最後に送った値と違うときだけ送る（M1 §5.4 のとおり）。報告対象は M1 の `reported` の一覧（`TimeZone`・`DateStyle`・`IntervalStyle`・`application_name` ほか）。`SET LOCAL` の効果の取り消しもトランザクションの終わりの後の Sync で拾える。

---

## 6. モジュールごとの仕様

### 6.1 yuzhu-server（XQ-1）

#### 6.1.1 `protocol/codec.rs`

- `read_message` を **`read_raw`**（型バイトと本体。長さの検査まで。上限は `max_message_len`）と **`decode(RawMessage) -> Result<FrontendMessage, ProtocolError>`** に分ける（既存の `read_message` は両方を呼ぶ薄い関数として残す。SkipTillSync 中は復号せずに捨てられる）。
- 復号は `RecvBuf`（§4.4）に似た本体用の読み取り器で書く（足りなければ `BadBody { sqlstate: "08P01", message: "insufficient data left in message" }`、余りがあれば `"invalid message format"`）。cstring は UTF-8 で検査し、違えば `BadBody { sqlstate: "22021", message: "invalid byte sequence for encoding \"UTF8\": 0x.." }`。
- 値の長さは `i32`。−1 が NULL、0 以上はそのバイト数（残りより大きければ「足りない」）、それ以外の負は「足りない」と同じ扱い（【実機】`pq_getmsgbytes` が 08P01 を返す）。
- Bind の形式コードの個数・値は復号では検査せず、`handle_ext` で `Format` に直すときに検査する（0・1 以外は `22023 unsupported format code: N`。ErrorResponse の後は他のエラーと同じ）。
- 符号化に足す `BackendMessage`: `ParseComplete`、`BindComplete`、`CloseComplete`、`NoData`、`PortalSuspended`、`ParameterDescription(&[u32])`、`DataRowRaw(&[Option<Vec<u8>>])`、`CopyInResponse` / `CopyOutResponse { format: u8, columns: &[u8] }`、`CopyData(&[u8])`、`CopyDone`。`RowDescription` の `FieldDescription.format` は M1 から `i16` で、`Sink::row_description` が `ColumnDesc.format as i16` を入れる。

#### 6.1.2 `Sink`

```rust
impl<W: Write> ResultSink for Sink<'_, W> {
    fn row_description(&mut self, columns: &[ColumnDesc]) -> io::Result<()>;   // format を列ごとに入れる
    fn data_row_raw(&mut self, values: &[Option<Vec<u8>>]) -> io::Result<()>;  // DataRowRaw
    fn error(&mut self, err: &Error) -> io::Result<()>;                        // 書いて flush する（XQ-D13）
    fn notice(&mut self, notice: &Notice) -> io::Result<()>;                    // 書いて flush する
    fn copy_out_response / copy_out_data / copy_out_done                        // XQ-6。copy_out_response の後は flush しない
}
```

M1 の `data_row`（テキスト）は Simple Query のために残す（`data_row_raw` の既定実装は、全列がテキストと見なして `data_row` に変換する。M1〜M4 の `ResultSink` の実装（テスト用の収集器など）がそのまま動く）。

#### 6.1.3 `handle_ext`

```rust
enum Next { Continue, CopyIn }
/// 外側の io::Result は書き込みの失敗（接続を閉じる）、内側の Result は Session のエラー（ErrorResponse にして SkipTillSync）
fn handle_ext(session: &mut Session, msg: FrontendMessage, w: &mut BufWriter<TcpStream>) -> io::Result<Result<Next>> {
    macro_rules! core { ($e:expr) => { match $e { Ok(v) => v, Err(e) => return Ok(Err(e)) } } }   // Session のエラーを内側へ
    match msg {
        Parse { name, sql, param_types } => { core!(session.msg_parse(&name, &sql, &oids(param_types))); write(w, ParseComplete)?; }
        Bind { portal, stmt, param_formats, params, result_formats } => {
            let pf = core!(formats(&param_formats)); let rf = core!(formats(&result_formats));      // 22023
            let p: Vec<Option<&[u8]>> = params.iter().map(|v| v.as_deref()).collect();
            core!(session.msg_bind(&portal, &stmt, &pf, &p, &rf)); write(w, BindComplete)?;
        }
        Describe { target: DescribeTarget::Statement, name } => {
            let d = core!(session.msg_describe_statement(&name));
            write(w, ParameterDescription(&d.param_types))?; write_row_description_or_no_data(w, d.columns.as_deref())?;
        }
        Describe { target: DescribeTarget::Portal, name } =>
            write_row_description_or_no_data(w, core!(session.msg_describe_portal(&name)).as_deref())?,
        Execute { portal, max_rows } => match core!(session.msg_execute(&portal, max_rows.max(0) as u32, &mut Sink::new(w))) {
            ExecOutcome::Complete(tag) => write(w, CommandComplete(&tag))?,
            ExecOutcome::Suspended => write(w, PortalSuspended)?,
            ExecOutcome::EmptyQuery => write(w, EmptyQueryResponse)?,
            ExecOutcome::CopyIn => return Ok(Ok(Next::CopyIn)),
        },
        Close { target: DescribeTarget::Statement, name } => { session.msg_close_statement(&name); write(w, CloseComplete)?; }
        Close { target: DescribeTarget::Portal, name } => { session.msg_close_portal(&name); write(w, CloseComplete)?; }
        _ => unreachable!(),
    }
    Ok(Ok(Next::Continue))
}
```

`message_loop` は `match handle_ext(session, m, writer)? { Ok(next) => .., Err(e) => ErrorResponse → msg_abort_implicit → SkipTillSync }` と受ける（§5.2）。`session.is_closing()`（FATAL）は各メッセージの後で確かめる。

### 6.2 Session（XQ-2）

#### 6.2.1 文とポータルの寿命（【実機】）

| 対象 | 作る | 置き換える | 消える |
|---|---|---|---|
| 無名の文 | `msg_parse("")` | 次の `msg_parse("")` が**先に**捨てる（Parse が失敗しても捨てたまま） | `Q`（`execute_simple` の先頭）、`DISCARD ALL`、セッション終了。**Sync では消えない** |
| 名前付きの文 | `msg_parse(name)` / SQL の `PREPARE` | 同じ名前の Parse は 42P05（先に Close か `DEALLOCATE`） | `msg_close_statement`、`DEALLOCATE`、`DISCARD ALL`、セッション終了。トランザクションのロールバックでは**消えない** |
| 無名のポータル | `msg_bind("")` | 次の `msg_bind("")` が捨てる | `Q`、トランザクションの終わり |
| 名前付きのポータル | `msg_bind(name)` | 同じ名前の Bind は 42P03 `cursor "p" already exists` | `msg_close_portal`、トランザクションの終わり（Sync によるコミットを含む） |

- `ExtStore::end_of_transaction()` はポータルを全部 drop する。`Portal` の drop で `RegisteredSnapshot` が外れ、`BoxedExecutor` が捨てられる。**`commit_transaction` / `rollback_transaction` の先頭**（XID の確定の前）で呼ぶ。
- `execute_simple` の先頭で `ext.unnamed = None; ext.unnamed_portal = None;`（`Q` は無名の文と無名のポータルを捨てる。00 §4.7）。**名前付きのポータル**は、ブロックの中なら `Q` でも残る。
- 文を `stmts` に持つ `Arc<PreparedStmt>` は、そこから作ったポータルも持つ（`Arc`）。`Close` や `DEALLOCATE` で `stmts` から消しても、動いているポータルは止まらない。

#### 6.2.2 行の符号化

```rust
fn encode_row(row: &[Datum], types: &[SqlType], formats: &[Format], env: &TypeEnv<'_>, cat: &dyn CatalogReader)
    -> Result<Vec<Option<Vec<u8>>>>;
fn encode_value(d: &Datum, ty: SqlType, fmt: Format, env: &TypeEnv<'_>, cat: &dyn CatalogReader) -> Result<Option<Vec<u8>>> {
    if matches!(d, Datum::Null) { return Ok(None); }
    match fmt {
        Format::Text => Ok(Some(text_of(d, ty, env, cat)?.into_bytes())),     // Simple Query の row_to_text と同じ関数（regclass・regproc の名前引きを含む）
        Format::Binary if !binary::supports_binary(ty) =>
            Err(Error::new(sqlstate::UNDEFINED_FUNCTION, format!("no binary output function available for type {}", type_display_name(ty.oid)))),
        Format::Binary => binary::output_binary(d, ty).map(Some),
    }
}
```

- `types` は結果の列の型（`unknown` は `text`。M3 の `row_to_text` と同じ）。結果の列の数より長い行（resjunk の列）は先頭の `ncols` 個だけ使う。
- バイナリの `regclass` / `regproc` / `regtype` は OID の 4 バイト（名前の引き直しをしない。【実機】`'pg_class'::regclass` は `00 00 04 eb`）。

#### 6.2.3 パラメータの変換（Bind の手順 5）

```rust
fn bind_param(i: usize, raw: Option<&[u8]>, fmt: Format, ty: Oid, portal: &str, env: &TypeEnv<'_>) -> Result<Datum> {
    let Some(raw) = raw else { return Ok(Datum::Null) };
    let r = match fmt {
        Format::Text => {
            let s = std::str::from_utf8(raw).map_err(|e| invalid_utf8(raw, e))?;            // 22021。NUL も 22021 `0x00`
            io::input_text(s, SqlType::of(ty), env)                                         // typmod は −1
        }
        Format::Binary => {
            if !binary::supports_binary(SqlType::of(ty)) { return Err(no_binary_input(ty)); }   // 42883 `no binary input function available for type aclitem`
            binary::input_binary(raw, SqlType::of(ty)).map_err(|e| fix_trailing(e, i))          // 22P03 の文言に " in bind parameter N" を足す
        }
    };
    r.map_err(|e| e.with_context(format!("{} parameter ${} = '...'", portal_label(portal), i + 1)))   // XQ-D23
}
// portal_label: 無名は "unnamed portal"、名前付きは `portal "p"`
```

`invalid_utf8` の文言は `invalid byte sequence for encoding "UTF8": 0xff`（不正な位置から `error_len` バイト、なければ末尾までを `0x..` で空白区切りに並べる。【実機】`ff fe` → `0xff`、`00` → `0x00`）。

#### 6.2.4 エラーの処理

`msg_*` は内部の `fail(e: Error, sql: Option<&str>) -> Error` を通して返す（M3 の `report_error` から「送る部分」を除いたもの。F0 が分ける。§11 C8）。

1. `e.resolve_position(sql)`（XQ-D22）。
2. `Severity::Panic` ならクラスタを poison して Fatal に直す。Fatal なら `closing = true`。
3. 状態: `Implicit` → `rollback_transaction()`（Idle。ポータル全消え）、`Block` → `rollback_transaction()` して `Failed`（ポータルは `end_of_transaction` で全部消える）。**ブロックの Failed でもポータルの map は即座には消さない**（【実機】Failed の後の Execute は 34000 ではなく 25P02）。ただし、エラーが出たポータルの実行器とスナップショットは捨てる（`PortalRun::Failed`）。
4. 戻す。

`msg_abort_implicit()` は上の 3 を、まだ済んでいなければ行う（冪等）。server が見つけたエラー（本体の不正・形式コード）の後始末のためにある。

#### 6.2.5 `Session::execute_simple` との共有

`execute_simple`・Execute は、次の部品を共有する（F0 が `session/mod.rs`・`simple.rs` からこの形で切り出す。§11 C8）。

```rust
impl Session {
    pub(super) fn begin_implicit_if_idle(&mut self);
    pub(super) fn catalog_snapshot(&self) -> RegisteredSnapshot;                          // D12。take_snapshot（登録する。短命。R-02）
    // acquire_statement_snapshot は作らない。02 §4.5 の `Session::statement_snapshot(&mut self) -> Result<StatementSnapshot>`（00 §5.1 d の 0'・4。
    // RC: 登録、RR: xact_snapshot の写し。RR の最初のスナップショットはロックの前に Bind が呼ぶ。R-03）と `note_snapshot_use` を使う
    pub(super) fn assign_xid_for_write(&mut self) -> Result<()>;                          // §5.1 d.3（読み取り専用なら 25006）
    pub(super) fn exec_ctx_parts(&mut self) -> ExecParts<'_>;                             // session_info・runtime・type_env・WaitCtx
    pub(super) fn exec_statement(&mut self, stmt: &Statement, out: &mut Output) -> Result<String>;   // M3。トランザクション制御・SET・SHOW・CHECKPOINT
}
```

### 6.3 準備済み文（XQ-5。`session/prepared.rs`）

#### 6.3.1 解析の鍵と再解析

```rust
fn current_key(&self) -> AnalysisKey {
    AnalysisKey { generation: db.cache.generation(), search_path: self.settings.search_path(),
                  datestyle: self.settings.get("datestyle").to_owned(), timezone: self.settings.get("timezone").to_owned() }
}
fn analysis_is_valid(&self, s: &PreparedStmt) -> bool {
    matches!(s.kind, StmtKind::Query | StmtKind::Dml { .. } | StmtKind::Explain)
        && s.analysis.as_ref().is_some_and(|a| !a.volatile && a.key == self.current_key())
        && !self.txn.catalog_dirty
}
/// Bind と Describe（文）が呼ぶ。ロック → 世代の確認 → 再解析（§5.5 の手順 6）。戻り値は有効な PreparedStmt
fn revalidate(&mut self, stmt: &Arc<PreparedStmt>) -> Result<Arc<PreparedStmt>>;
```

- **世代は「ロックを取った後」に読む**（PostgreSQL の `AcquireExecutorLocks` の後の確認と同じ）。ロックの取得が待ちに入ると、その間に DDL がコミットして世代が進むことがある。読み直さないと古い解析で実行する。
- 結果の列の比較は `(type_oid, type_modifier)` の列の並びで行う（名前・table_oid・attnum は見ない）。個数が違えば不一致。
- 不一致のとき、文（`stmts` の中）は**古いまま残す**。同じ文は再び Bind しても同じエラーになる（【実機】Close するまで）。列を元に戻す DDL の後は成功する。
- `Ddl`・`Copy`・`Utility` の文は Parse で解析せず、**Execute のたびに**ロック → XID → スナップショット → 解析 → 実行（§5.1 d。Simple Query と同じ）を行う。DDL の解析結果は解決済みの OID を含み、再利用すると古い OID を操作するため。`Explain` は `Query` と同じ鍵で管理する。
- `volatile`（解析のときに `txn.catalog_dirty` だった）の文は、そのトランザクションが終わるまで毎回再解析する。トランザクションがロールバックされても、解析結果は捨てる（`analysis` を `None` に戻す）。

#### 6.3.2 `PREPARE name [ ( type [, ...] ) ] AS statement`

1. 本文は SELECT・INSERT・UPDATE・DELETE・VALUES のどれか（それ以外は構文エラー。パーサが `PreparableStmt` だけ受ける）。型名は M1〜M5 の型名の規則で OID にする（typmod は無視。`PREPARE p(varchar(5))` は通る）。未知の型は `42704 type "x" does not exist`、実装していない型は `0A000`。
2. 名前が既にあれば `42P05 prepared statement "q" already exists`。
3. Parse と同じ解析（`analyze_with_params(&body, &catalog, &declared)`）で型を決める。確定しなければ 42P18。結果は `from_sql = true` の `PreparedStmt` として `stmts` に入れる（プロトコルの Parse した文と同じ名前空間）。
4. タグ `PREPARE`。**ブロックの中でも外でも使える**。ロールバックしても文は残る（【実機】`BEGIN; PREPARE x AS SELECT 1; ROLLBACK; EXECUTE x` が成功する）。

#### 6.3.3 `EXECUTE name [ ( expr [, ...] ) ]`

1. 文を引く（無ければ `26000 prepared statement "q" does not exist`）。引数の個数が `param_types.len()` と違えば **`42601 wrong number of parameters for prepared statement "q"`（DETAIL `Expected 1 parameters but got 2.`）**（【実機】）。
2. 引数は `analyze_execute_param`（assignment の文脈で宣言の型へ強制変換。変換できなければ `42804 parameter $1 of type integer cannot be coerced to the expected type text`、HINT `You will need to rewrite or cast the expression.`（【記憶】））で解析し、`lower_single_rel` + `eval_const` で値にする。副問い合わせは `0A000 cannot use subquery in EXECUTE parameter`、集約は `42803 aggregate functions are not allowed in EXECUTE parameters`（【記憶】）、列参照は `42703`。変換の失敗（`'x'` を int へ）は `22P02`（【実機】`invalid input syntax for type integer: "x"`、位置つき）。
3. 値から一時のポータルを作り（Bind の手順 6〜9 と同じ。ロック・再解析・計画・スナップショット）、**完走するまで実行する**（`max_rows = 0`）。結果の行は、Simple Query では `Output`（テキスト）へ、Extended の `UtilSelect` では `StoredRows` へ入れる。タグは内側の文のもの（`SELECT n` / `INSERT 0 n` ...）。
4. `EXECUTE` の `StmtKind` は `Utility { returns_rows }`。`returns_rows` と `columns` は**対象の文の `columns`**（【実機】`EXECUTE q9` の Describe は対象の `?column? int4` を返す）。対象が無ければ Describe / Bind のときに 26000。

#### 6.3.4 `DEALLOCATE` と `DISCARD`

| 文 | 動作 | タグ |
|---|---|---|
| `DEALLOCATE [PREPARE] name` | `stmts` から消す。無ければ `26000 prepared statement "q" does not exist` | `DEALLOCATE` |
| `DEALLOCATE [PREPARE] ALL` | `stmts` を空にする（無名の文は残す） | `DEALLOCATE ALL` |
| `DISCARD ALL` | **ブロックの中なら `25001 DISCARD ALL cannot run inside a transaction block`**（【実機】）。`stmts`・無名の文・設定（`RESET ALL`）・シーケンスの状態（`currval` / `lastval`）を捨てる | `DISCARD ALL` |
| `DISCARD PLANS` | `Analysis` をすべて `None` にする | `DISCARD PLANS` |
| `DISCARD SEQUENCES` | シーケンスのセッション状態を捨てる | `DISCARD SEQUENCES` |
| `DISCARD TEMP` | 何もしない（一時テーブルが無い） | `DISCARD TEMP` |

`DISCARD ALL` は暗黙のトランザクションの中で実行されるので、その時点で存在するポータルは自分自身（実行中。取り出し済み）だけ。`Q` の `DISCARD ALL` の後は ParameterStatus が（設定が戻っていれば）送られる。

#### 6.3.5 `utility_columns`

`Utility { returns_rows: true }` の `columns`: `SHOW name` → `(name, text)` の 1 列、`SHOW ALL` → `name` / `setting` / `description`（text）、`EXPLAIN` → `QUERY PLAN`（text）、`EXECUTE` → 対象の文の列。これ以外のユーティリティは `NoData`（【実機】`SET`・`BEGIN` の Describe は `NoData`）。

### 6.4 パラメータ型の推論（XQ-3）

#### 6.4.1 字句・構文・解析の口

- **字句**: `$` に 1 桁以上の数字が続くものを `Token::Param(u16)` にする。数字の直後に識別子の文字が続けば `42601 trailing junk after parameter`。`n > 65535` は `42601 parameter number too large`。`$` の後が数字以外ならドルクォートか構文エラー（M1 のまま）。
- **AST**: `Expr::Param { index: u16, span }`（`index` は `$n` の n）。式の文法のどこでも書ける（リテラルと同じ優先順位。`$1::int` は `::` が結合する）。
- **Simple Query で `$n`** を解析すると `Analyzer.params` が `None` なので `42P02 there is no parameter $1`（位置つき）。DEFAULT・CHECK の保存された式、`analyze_column_default` なども同じ（`$n` は入れられない）。
- **解析**: `Analyzer` に `params: Option<&ParamCtx>`。`Expr::Param` を見たら `ctx.reference(n, span)` で型を得て `ExprKind::ExternParam(n - 1)`（型は得た型）の節点を作る。`n == 0` は `42P02 there is no parameter $0`。

#### 6.4.2 フック点（`variable_coerce_param_hook` の 6 か所）

PostgreSQL の `parse_param.c` は「unknown の `$n` が型変換の対象になった時点で、その場で `$n` の型を変換先に確定する」。yuzhu のアナライザ（M2 の `coerce.rs`・`resolve.rs`・`expr.rs`・`select.rs`・`dml.rs`。M4 で書き直された後の同じ位置）に次の 6 か所の分岐を足す。**すべて「節点が `ExternParam` で `ty == UNKNOWN`」のときだけ働く**。

| # | 場所 | 追加する分岐 |
|---|---|---|
| H1 | `transform_expr` の `Expr::Param` | `ctx.reference(n, span)` → `ExternParam` の節点。`ctx` が無ければ 42P02 |
| H2 | `coerce_type` の `src == UNKNOWN` の分岐。**多相型への早期 return の後、`Literal` の分岐の前**（PostgreSQL の `coerce_type` の順序） | `ExternParam` なら `ctx.resolve(index, target, span)?` して、**節点の型だけ `target` に書き換えて返す**（キャストを挟まない。typmod は適用しない）。以降の `category == 'S'` の InOut キャストは `ExternParam` には使わない |
| H3 | `resolve_unknown`（M2 では自由関数。SELECT 句・ORDER BY・副問い合わせの出力の unknown を text にする） | `ExternParam` なら `coerce_type(e, TEXT)`（= H2）を通す。自由関数をメソッド化して `ParamCtx` を見られるようにする |
| H4 | 「unknown のリテラルか」を `Literal` かどうかで判定している箇所（`make_func_call` の `FuncNameAsType` など） | `ExternParam`（`ty == UNKNOWN`）も unknown のリテラルとして扱う（`fn is_unknown_const(e)`） |
| H5 | `x = ANY (unknown)`・`<> ALL (unknown)` の右辺、`ARRAY[...]` の要素（TY-5 の配列の式） | 右辺が unknown なら**左辺の型の配列型**（`get_array_type`）へ `coerce_type`（H2）。`ARRAY[$1, 1]` は `select_common_type` の結果の要素型に揃えてから配列型 |
| H6 | `INSERT ... VALUES`・`INSERT ... SELECT` の各列、`UPDATE ... SET`、`LIMIT` / `OFFSET`（int8）、`WHERE` の bool | 既存の `coerce_assignment` / `coerce_to_specific_type` が `coerce_type` を呼ぶので、H2 で足りる。`INSERT ... SELECT $1` は SELECT の unknown の出力を**解決せず**に、列の型へ強制変換する（M2 の `dml.rs` の「unknown output columns take the target type」と同じ経路。H3 を先に通さない） |

**最後の検査（`ParamCtx::finish`。`analyze_with_params` の末尾）**: PostgreSQL の `check_variable_parameters` と、`exec_parse_message` の未確定の検査を、この順で行う。

1. **(b)** `reference` が UNKNOWN を返した節点で、`resolve` を一度も通らなかったもの（`unresolved` に残る `(span.start, index)`）のうち、`types[index]` が**ほかの出現で確定している**ものがあれば `42P08 could not determine data type of parameter $n`（位置はその節点。複数あれば位置の小さいもの）。
2. 未確定のまま残った `$n`（`types[n-1] == UNKNOWN`。使われなかった番号を含む）があれば `42P18 could not determine data type of parameter $n`（**位置なし**。n は最小のもの）。
3. 確定した型の一覧（長さは `max(宣言の数, 使われた最大の n)`）を返す。

`resolve` が `types[index]` が確定済みで `target` と違う節点を見つけたときは `42P08 inconsistent types deduced for parameter $n`、DETAIL `{確定済みの型} versus {target}`（`format_type_be` の名前。例 `integer versus text`）、位置は節点。

**解析の順序は PostgreSQL と同じでなければならない**（SELECT 句 → FROM → WHERE → …、最後に SELECT 句の unknown の解決）。【実機】`SELECT $1 FROM t WHERE a = $1` は、WHERE の `a = $1` で `$1` が int4 に確定した後で SELECT 句の `$1`（unknown の節点）を text に解決しようとして `inconsistent types ... integer versus text`。

#### 6.4.3 文脈ごとの規則と、実機で確認した例

下の表の「`$1` の型」は **PostgreSQL 17.11 の ParameterDescription**（【実機】）。yuzhu のテスト（§7.4）の期待値にそのまま使う。**`abs($1)` のように関数の候補の集合に依存する行は、yuzhu の `pg_proc` の行が PostgreSQL と同じ候補を持つこと（M4・TY の関数の登録）が前提**で、違えば `func_select_candidate` の結果が変わる（§9）。

| 文脈 | 規則（PostgreSQL の関数） | SQL | `$1` の型（実機） |
|---|---|---|---|
| 比較（相手が型を持つ） | `binary_oper_exact`: unknown 側は相手の型 → H2 | `SELECT * FROM t WHERE id = $1`（`id int8`） | int8 (20) |
| 比較（unknown どうし） | `func_select_candidate`: 全部 unknown なら文字列カテゴリの preferred（text） | `SELECT $1 = $2` | text, text |
| 比較（2 回目以降の出現は確定済みの型） | `variable_paramref_hook` が確定済みの型で節点を作る | `SELECT $1 = 1 AND $1 = 1.5`、`... WHERE a = $1 AND f = $1`（`f float8`） | int4（2 回目は int4 → numeric / float8 のキャストを挟む。型は動かない） |
| 比較（IS NULL は変換しない） | `NullTest` は引数を強制変換しない | `SELECT $1 IS NULL` / `... IS NOT NULL` | **42P18**（位置なし） |
| IS NULL と別の出現の併用 | `check_variable_parameters` | `SELECT $1 IS NULL AND $1 = 1`、`WHERE $1 IS NULL OR a = $1` | **42P08 (b)**（位置は IS NULL の `$1`）。`$1 = 1 AND $1 IS NULL` は int4 で通る（確定した後の出現は確定済みの型の節点） |
| 算術 | `$1 + 1` → `int4 + int4`（unknown 側が相手の型） | `SELECT $1 + 1`、`SELECT 1 + $1` | int4 |
| 算術（相手が numeric リテラル） | 相手の型 | `SELECT $1 * 2.5` | numeric (1700) |
| 算術（unknown どうし） | 候補が決まらない | `SELECT $1 + $2`、`SELECT -$1` | **42725** `operator is not unique: unknown + unknown`（`- unknown`）。HINT あり |
| 算術（単項で型が決まる） | 明示キャストで確定 | `SELECT -$1::int` | int4 |
| 日付時刻 | 演算子の解決 | `SELECT g + $1 FROM t`（`g timestamp`）、`g - $1` | interval (1186)、timestamp (1114)（`g - unknown` は `timestamp - timestamp` が選ばれる） |
| INSERT の値 | `transformAssignedExpr`: 列の型へ assignment（H2）。typmod は付かない | `INSERT INTO t(id, c) VALUES ($1, $2)`（`c varchar(10)`） | int8, varchar (1043) |
| INSERT ... SELECT | SELECT 句の unknown を列の型へ（H6） | `INSERT INTO t(id, a) SELECT $1, $2` | int8, int4 |
| INSERT の複数行 | 各行の各列を列の型へ | `INSERT ... VALUES ($1, $2), ($3, 5)` | int8, int4, int8 |
| INSERT の値が式 | 式の型（unknown + int4 → int4）が列の型へ assignment | `INSERT INTO t(id) VALUES ($1 + 1)` | int4 |
| UPDATE の SET / WHERE | 列の型 / 比較 | `UPDATE t SET c = $1 WHERE id = $2` | varchar (1043), int8 |
| UPDATE の式 | 演算子の解決 | `UPDATE t SET d = $1 * 2 WHERE id = $2`（`d numeric(10,2)`） | int4, int8 |
| LIMIT / OFFSET | `coerce_to_specific_type(int8)` | `SELECT * FROM t LIMIT $1 OFFSET $2`、`FETCH FIRST $1 ROWS ONLY` | int8, int8 |
| LIMIT のキャスト | 明示キャスト | `LIMIT $1::int` | int4 |
| キャスト | `coerce_to_target_type(explicit)`。型だけ確定し、typmod は `CoerceTypmod` が適用 | `SELECT $1::int4`、`SELECT $1::numeric(4,1)`、`CAST($1 AS char(3))` | int4、numeric (1700)、bpchar (1042)。RowDescription の typmod は 262149 / 7 |
| 同じ `$n` に別のキャスト | 2 つ目のキャストは確定済みの型からのキャスト | `SELECT $1::int, $1::bigint`、`SELECT $1::int8 + $1::int4` | int4 / int8（最初に確定した型） |
| SELECT 句の裸の `$1` | `resolveTargetListUnknowns` → text | `SELECT $1`、`SELECT $1 AS x`、`SELECT ($1)`、`VALUES ($1)`、`SELECT $1 ORDER BY 1`、`SELECT DISTINCT $1`、`SELECT $1 LIMIT 1` | text (25) |
| SELECT 句と別の出現 | 先に確定した型と text が衝突 | `SELECT $1, $1::int4`、`SELECT $1 FROM t WHERE a = $1` | **42P08 (a)** `integer versus text`（位置は SELECT 句の `$1`） |
| 確定した後の SELECT 句 | | `SELECT $1::int4, $1` | int4（SELECT 句の 2 つ目は int4 の節点） |
| LIKE / ~ | text の演算子 | `WHERE b LIKE $1`、`SELECT $1 ~ 'a'`、`SELECT $1 LIKE 'a%'` | text |
| IN のリスト | `select_common_type`（unknown だけなら text） | `id IN ($1, $2)`、`SELECT $1 IN (1, 2)`、`SELECT $1 IN ('a','b')`、`SELECT $1 IN ($2, $3)` | int8, int8 / int4 / text / text, text, text |
| BETWEEN | `a >= $1 AND a <= $2` に展開 | `WHERE a BETWEEN $1 AND $2` | int4, int4 |
| IS [NOT] DISTINCT FROM | 比較と同じ | `a IS DISTINCT FROM $1`、`SELECT $1 IS DISTINCT FROM 1` | int4 |
| = ANY / <> ALL | 右辺の unknown は左辺の配列型（H5） | `id = ANY($1)`、`a = ANY($1)`、`b = ANY($1)`、`a <> ALL($1)` | int8[] (1016)、int4[] (1007)、text[] (1009)、int4[] |
| 配列の式 | `select_common_type` → 配列型 | `ARRAY[$1, $2]`、`ARRAY[$1, 1]`、`$1::int[]` | text, text / int4 / int4[] (1007) |
| 関数の引数 | `func_select_candidate`（unknown は preferred の型） | `length($1)`、`upper($1)`、`lower($1) = $2`、`substring($1, 1, 2)`、`substring($1 from $2)`、`position($1 in 'abc')` | text（すべて） |
| 関数の引数（数値） | 候補の preferred（float8） | `abs($1)`、`round($1)`、`power($1, 2)`、`$1 ^ 2` | float8 (701) |
| 関数（2 引数） | | `round($1, 2)` | numeric (1700) |
| 関数（共通の型） | `select_common_type` | `coalesce($1, 1)`、`NULLIF($1, 1)`、`greatest($1, 2)`、`coalesce($1::int, $2)` | int4 |
| 関数（共通の型が unknown だけ） | text | `coalesce($1, $2)` | text, text |
| 関数（FROM 句） | | `SELECT * FROM generate_series(1, $1)` | int4 |
| CASE | `select_common_type`、WHEN は bool | `CASE WHEN $1 THEN $2 ELSE $3 END`、`CASE WHEN true THEN $1 ELSE 1 END, $1`、`CASE $1 WHEN 1 THEN 'a' ELSE 'b' END` | bool, text, text / int4 / **42883** `text = integer` |
| WHERE の真偽 | `coerce_to_boolean` | `WHERE $1`、`WHERE e AND $1`、`NOT $1` | bool |
| 集合演算 | `select_common_type`（腕をそろえる） | `SELECT $1 UNION SELECT 1`、`SELECT a FROM t UNION ALL SELECT $1` | int4 |
| 副問い合わせ | 副問い合わせの出力の unknown は内側で text に解決される | `SELECT (SELECT $1)`、`a = (SELECT $1)`、`a IN (SELECT $1)` | text / **42883** `integer = text` / **42883** |
| 集約・GROUP BY・HAVING | | `sum(a + $1)`、`HAVING count(*) > $1`、`GROUP BY $1` | int4、int8、text |
| RETURNING | 出力の unknown は text | `INSERT INTO t(id) VALUES ($1) RETURNING $2` | int8, text |
| 連結 | `\|\|` は text 同士の候補（`anytextcat`） | `SELECT 'a' \|\| $1`、`SELECT $1 \|\| 'a'`、`SELECT 1 \|\| $1`、`SELECT $1 \|\| $2` | text |
| 宣言された型（Parse の型の OID） | `variable_paramref_hook` が最初から宣言の型で節点を作る。unknown ではないので H2 は働かない | `SELECT $1 + 1` に int8 / text | int8 / **42883** `operator does not exist: text + integer` |
| 宣言（キャストの元が宣言型） | 宣言の型から明示キャスト | `SELECT $1::int4` に text (25) | text（ParameterDescription は宣言の型のまま）。RowDescription は int4 |
| 宣言（0 = 未指定、使われない番号） | 未確定は 42P18 | `SELECT $1`（宣言 `[0, 23]`）/ `SELECT 1`（宣言 `[0]`） / `SELECT $1::int`（宣言 `[0, 0, 25]`） | text, int4（個数は宣言の数まで）/ **42P18** `$1` / **42P18** `$2` |
| 番号の歯抜け | 小さい番号が先に検査される | `SELECT $2`、`SELECT $1, $3` | **42P18** `$1`、`$2` |
| 宣言の OID が未知 | `cache lookup failed for type N` | `SELECT $1`（宣言 99999） | **XX000**（同じ文言） |
| `$0` | `variable_paramref_hook` | `SELECT $0` | **42P02** `there is no parameter $0`（位置つき） |
| 文が複数 | `exec_parse_message` | `SELECT 1; SELECT 2` | **42601** `cannot insert multiple commands into a prepared statement` |
| 空の問い合わせ | | `""`、`-- c`、`;` | ParameterDescription は空、NoData。Bind・Execute は `EmptyQueryResponse` |

**宣言された型の検査**: `ParamCtx::new(declared)` は 0 を UNKNOWN にし、非 0 の OID が `type_by_oid` に無ければ XX000 `cache lookup failed for type N`、実装していない型（`is_supported_type` が偽）は `0A000 type X is not supported yet`。

#### 6.4.4 `ExternParam` が各層で通る場所（D45）

`ExprKind::ExternParam(u16)` を 1 回だけ足す（`expr/mod.rs`）。Rust の網羅 match の警告に従って、次の場所を直す。

| 場所 | 扱い |
|---|---|
| `expr/walk.rs` の `walk` / `try_map` | 葉。`try_map` はそのまま複製（`C` `Q` を変えても型に影響しない） |
| `expr::lower_single_rel` | そのまま通す（物理層でも `ExternParam`） |
| `deparse/`（EXPLAIN・`pg_get_expr`） | `$n`（n = index + 1） |
| `executor/eval.rs` | `ctx.bind_params[i].clone()`。範囲外は `Error::internal`（Bind が個数を検査済み） |
| `planner/rules.rs`・`physicalize.rs` | **定数ではない**ので畳み込まない。一方、`IndexScanKey::Eq(expr)`・`Limit`・`RangeBound`・`NestedLoopParam` の「実行時に 1 回評価できる式」の判定（列参照を含まず volatile でない）は `ExternParam` を許す（M4 の判定関数に分岐を足す。§11 C9）。`LIKE` の前置一致の範囲化はリテラルのパターンだけ |
| `analyzer`（M4 の `FromItem` 内の関数、CHECK・DEFAULT の保存式） | CHECK・DEFAULT・GENERATED の式に `$n` は書けない（`Analyzer.params == None` → 42P02） |
| 物理プランの `PhysicalQuery.n_params`（M4 の `ParamId`。相関サブクエリ用） | **別物**。`ExternParam` の個数ではない（D45） |

#### 6.4.5 再解析と宣言の固定

Bind の再解析（§5.5 手順 6）と `PREPARE` した文の再解析は、`declared = stmt.param_types`（確定済みの型）を渡す。すべて宣言済みなので `reference` は最初から確定した型を返し、`resolve` は呼ばれても一致するだけ。型推論が再解析で変わることはない（`PreparedStmt.param_types` は Parse のときに固定される）。

### 6.5 バイナリ形式（XQ-4。`types/binary.rs`）

#### 6.5.1 振り分け

`codec(oid)` は次の表を `match` で引く（`pg_type.typsend` / `typreceive` 列は TY-1 が同じ表から埋める）。**この章（XQ-4）が実装する型**と、**各章が実装して表に登録する型**:

| 型 | OID | send | recv（typmod は −1） | 持ち主 |
|---|---|---|---|---|
| bool | 16 | 1 バイト（0 / 1） | 1 バイト。**0 以外は true**。余りは 22P03（【実機】`01 01` → 22P03、`02` → true） | XQ-4 |
| "char" | 18 | 1 バイト | 1 バイト | XQ-4 |
| name | 19 | UTF-8 のバイト列 | UTF-8 検査。63 バイトを超えたら `42622 identifier too long`（【記憶】DETAIL `Identifier must be less than 64 characters.`） | XQ-4 |
| int8 / int2 / int4 | 20 / 21 / 23 | 8 / 2 / 4 バイト BE | 同じ長さ。足りなければ 08P01、余りは 22P03（【実機】int4 に 5 バイト → 22P03、2 バイトと 0 バイト → 08P01。int2 に 3 バイト → 22P03） | XQ-4 |
| float4 / float8 | 700 / 701 | IEEE 754 のビット列 BE（NaN は `7f f8 00 00 00 00 00 00`、-Infinity は `ff 80 00 00`（float4）） | 同じ長さ。足りなければ 08P01 | XQ-4 |
| text / varchar / unknown / pg_node_tree | 25 / 1043 / 705 / 194 | UTF-8 のバイト列そのまま | 残り全部を UTF-8 検査（22021）。空は空文字列 | XQ-4 |
| oid / xid / cid / regproc / regclass / regtype | 26 / 28 / 29 / 24 / 2205 / 2206 | `u32` BE | 4 バイト | XQ-4 |
| tid | 27 | block `u32` BE + offset `u16` BE（6 バイト。`(1,2)` → `00 00 00 01 00 02`） | 6 バイト | XQ-4 |
| int2vector | 22 | **配列の形式で、下限が 0**（`'1 2'` → ndim 1、flags 0、elemtype 21、len 2、lbound 0、各要素 `00 00 00 02` + `i16`） | 同じ形式。ndim = 1・NULL なし・elemtype = int2・lbound = 0 を要求し、違えば `22P03 invalid int2vector data`（【記憶】） | XQ-4（`array.rs` の部品を使う） |
| bytea | 17 | 生のバイト列 | 残り全部 | TY-6 |
| bpchar | 1042 | UTF-8（空白で埋めた文字列そのまま） | UTF-8。typmod が −1 なので埋めない（プランの `CoerceTypmod` が埋める） | TY-6 |
| uuid | 2950 | 16 バイト | 16 バイト | TY-6 |
| numeric | 1700 | `ndigits i16, weight i16, sign u16, dscale u16, digits [i16; ndigits]`（BE）。`yuzhu-numeric` の `to_binary` | 検査: `sign` ∈ {0x0000, 0x4000, 0xC000, 0xD000, 0xF000}（違えば `22P03 invalid sign in external "numeric" value`）、`dscale` ∈ 0..=16383（`invalid scale in external "numeric" value`）、各 digit ∈ 0..=9999（`invalid digit in external "numeric" value`）、`ndigits` 個ぴったり（余りは 22P03）（【記憶】文言） | TY-6（M4 の統合の上） |
| date | 1082 | `i32` 日（2000-01-01 から） | 同じ。範囲外は `22008 date out of range`（【記憶】）。`i32::MAX` / `MIN` は ±infinity | TD-5 |
| timestamp / timestamptz | 1114 / 1184 | `i64` マイクロ秒（2000-01-01 00:00:00 から。timestamptz は UTC） | 同じ | TD-5 |
| time | 1083 | `i64` マイクロ秒（0 時から） | 範囲 0..=86400000000 | TD-5 |
| interval | 1186 | `i64` マイクロ秒、`i32` 日、`i32` 月（16 バイト） | 同じ | TD-5 |
| 1 次元の配列（`int2[]`、`int4[]`、`text[]` ... とカタログの `oid[]`） | 1005、1007、1009 ... | `ndim i32`、`flags i32`（NULL 要素があれば 1）、`elemtype u32`、次元ごとに `len i32` と `lbound i32`、各要素は `len i32`（−1 = NULL）+ 要素の send。`{}` は `ndim 0, flags 0, elemtype` の 12 バイト | `elemtype` が配列の要素型と違えば `22P03 wrong element type`、`ndim > 1` は `0A000 multidimensional arrays are not supported yet`、`flags` の NULL 要素の整合、要素ごとに recv + 長さの一致 | TY-6（`array.rs`） |
| aclitem、anyarray ほか | 1033、2277 | **無し**（`supports_binary` が偽） | **無し** | — |

**バイナリを持たない型**: パラメータでバイナリを指定されたら `42883 no binary input function available for type aclitem`（Bind のとき。【実機】）。結果でバイナリを指定されたら、**値が NULL でない行を出力するときに** `42883 no binary output function available for type aclitem`（行が 0 件・NULL だけなら成功。【実機】XQ-D15）。

#### 6.5.2 例（PostgreSQL 17.11 の `*_send` の出力。固定値として単体テストにする）

| 値 | 型 | バイト列（16 進） |
|---|---|---|
| `true` | bool | `01` |
| `-1`、`1` | int2、int8 | `ffff`、`0000000000000001` |
| `1.5` | float4 | `3fc00000` |
| `'NaN'` | float8 | `7ff8000000000000` |
| `'(1,2)'` | tid | `000000010002` |
| `'pg_class'` | regclass | `000004eb` |
| `'1 2'::int2vector` | int2vector | `00000001` `00000000` `00000015` `00000002` `00000000` `00000002` `0001` `00000002` `0002`（ndim、flags、elemtype、len、lbound、要素 2 つ） |
| `'{1,NULL}'::oid[]` | oid[] | `00000001` `00000001` `0000001a` `00000002` `00000001` `00000004` `00000001` `ffffffff` |
| `ARRAY[1,2,NULL]::int4[]` | int4[] | `00000001 00000001 00000017 00000003 00000001 00000004 00000001 00000004 00000002 ffffffff` |
| `'{}'::int4[]` | int4[] | `00000000 00000000 00000017` |
| `ARRAY['a','b']::text[]` | text[] | `00000001 00000000 00000019 00000002 00000001 00000001 61 00000001 62` |
| `1.5::numeric(5,2)` | numeric | `0002 0000 0000 0002 0001 1388`（ndigits 2、weight 0、sign 0、dscale 2、digits `[1, 5000]`） |
| `-12345.678` | numeric | `0003 0001 4000 0003 0001 0929 1a7c`（digits `[1, 2345, 6780]`） |
| `100000000` | numeric | `0001 0002 0000 0000 0001` |
| `0`、`'NaN'` | numeric | `0000 0000 0000 0000`、`0000 0000 c000 0000` |
| `0.0001` | numeric | `0001 ffff 0000 0004 0001` |
| `'infinity'`、`'-infinity'` | numeric | `0000 0000 d000 0020`、`0000 0000 f000 0020`（**dscale 欄が 0x0020**。PostgreSQL 17.11 の実測。`yuzhu-numeric` の `to_binary` が同じ値を返すかは §9。クライアントは dscale を無視する） |
| `date '2000-01-02'` | date | `00000001` |
| `'infinity'::date` | date | `7fffffff` |
| `timestamp '2000-01-01 00:00:01.5'` | timestamp | `000000000016e360` |
| `'-infinity'::timestamp` | timestamp | `8000000000000000` |
| `timestamptz '2000-01-01 09:00:00+09'` | timestamptz | `0000000000000000` |
| `time '01:02:03.5'` | time | `00000000ddf019e0` |
| `interval '1 month 2 days 3 seconds'` | interval | `00000000002dc6c0` `00000002` `00000001` |
| `'\x0102'::bytea` | bytea | `0102` |
| `'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid` | uuid | `a0eebc999c0b4ef8bb6d6bb9bd380a11` |

#### 6.5.3 エラーの使い分け（【実機】）

| 入力 | SQLSTATE | メッセージ |
|---|---|---|
| バイナリの値が型の長さより短い（int4 に 2 バイト、float8 に 4 バイト、int4 に 0 バイト） | 08P01 | `insufficient data left in message` |
| 余りがある（int4 に 5 バイト、int2 に 3 バイト、bool に 2 バイト、oid に 5 バイト、numeric に余り） | 22P03 | `incorrect binary data format in bind parameter N` |
| テキストのパラメータの不正な書式（int4 に `abc`） | 22P02 | `invalid input syntax for type integer: "abc"`（CONTEXT `unnamed portal parameter $1 = '...'`） |
| UTF-8 でないバイト列（text に `ff fe`、NUL を含む） | 22021 | `invalid byte sequence for encoding "UTF8": 0xff` / `0x00` |
| バイナリを持たない型 | 42883 | `no binary input function available for type aclitem` |
| 値の範囲（numeric(10,2) に 123456789012） | 22003 | `numeric field overflow`（プランの `CoerceTypmod` が出す。DETAIL `A field with precision 10, scale 2 must round to an absolute value less than 10^8.`） |

#### 6.5.4 typmod が適用される場所（XQ-D11）

パラメータの recv / input は typmod = −1。長さ・精度の検査は、アナライザが代入やキャストの上に作る `CoerceTypmod`（M1 §5.2、M4 §6.2）が実行時に行う。**この結果、PostgreSQL が Bind のときに定数畳み込みで出す 22001・22003・22012 などを、yuzhu は Execute のときに出す**。パイプライン（`P B E S`）ではどちらもエラー → Sync まで読み捨て → ReadyForQuery の同じ形になる（BindComplete の有無だけが違う。M5-XQ-Q4）。

### 6.6 ParameterStatus と SET の関係

- 報告対象は `settings.rs` の `reported = true` の項目。接続時の一覧（【実機】PostgreSQL 17.11）: `in_hot_standby`、`integer_datetimes`、`TimeZone`、`IntervalStyle`、`is_superuser`、`application_name`、`default_transaction_read_only`、**`scram_iterations`（4096。PostgreSQL 16 以降。AU が `settings.rs` に足す。00 §3.6 の「未検証」はこれで確認済み）**、`DateStyle`、`standard_conforming_strings`、`session_authorization`、`client_encoding`、`server_version`、`server_encoding`。
- 値が変わる経路は `SET [LOCAL]`、`RESET`、`SET SESSION CHARACTERISTICS AS TRANSACTION ...` と `SET default_transaction_read_only`（reported）、`DISCARD ALL`（`RESET ALL`）、トランザクションのロールバック（`SET` の巻き戻し）、`ALTER ROLE ... SET` と `ALTER DATABASE ... SET`（M5 では持たない）。
- **送る時点は Sync の ReadyForQuery の直前**（XQ-D12）。Simple Query は `Q` の最後（M1 のまま）。1 つのバッチで `SET TimeZone` を 2 回実行しても、送るのは最終値 1 回（最後に送った値と違うときだけ）。
- 型の入出力に効く設定（`DateStyle`・`TimeZone`・`IntervalStyle`・`extra_float_digits`・`bytea_output`）は `TypeEnv` に反映され、**中断中のポータルにも次の Execute から効く**（Execute ごとに `TypeEnv` を作る。PostgreSQL もポータルの出力関数は呼び出し時の設定を使う）。

### 6.7 COPY（XQ-6。任意）

M4 は `COPY ... FROM STDIN`（テキスト、Simple Query のみ）を持つ（M4 §14.5）。XQ-6 は次を足す。**`copy/` は M4 の持ち物なので、新規ファイル `copy/to.rs`・`copy/csv.rs` を足し、M4 のファイルは CSV の分岐と Extended の入口を足すだけ**（§11 C13）。

#### 6.7.1 範囲

| 項目 | 内容 |
|---|---|
| `COPY table [(cols)] TO STDOUT [WITH (options)]`、`COPY (query) TO STDOUT` | テキスト形式（既定）と CSV。行ごとに `d`（CopyData）。【実機】1 メッセージ 1 行。`CopyOutResponse`、`CopyData`...、`CopyDone`、`CommandComplete("COPY n")` |
| CSV（FROM と TO） | オプション `FORMAT csv`、`HEADER [true/false]`、`DELIMITER 'c'`、`NULL 'str'`、`QUOTE 'c'`、`ESCAPE 'c'`、`FORCE_QUOTE (cols \| *)`（TO）、`FORCE_NOT_NULL (cols)`・`FORCE_NULL (cols)`（FROM）、`ENCODING 'UTF8'`、`FREEZE`（無視）。**それ以外は `0A000`**。`FORMAT binary` は `0A000`（M6）。`WHERE`（PostgreSQL 12）は `0A000` |
| Extended 経由 | COPY IN: Execute で `CopyInResponse` を送って `ExecOutcome::CopyIn`。COPY OUT: Execute の中で `CopyOutResponse`〜`CopyDone` を送り、`Complete("COPY n")` |

**XQ-6 を M6 に回した場合**: COPY を含む Parse は `0A000 COPY is not supported in the extended query protocol yet` を Parse のときに返す（PostgreSQL では動くので既知の差。Simple Query の `COPY ... FROM STDIN` は M4 のまま動く）。

#### 6.7.2 メッセージの流れ（【実機】）

COPY TO（Simple と Extended で同じ）:

```
Parse/Bind/Describe(S)  → ParseComplete、ParameterDescription、NoData、BindComplete   ※ Describe(P) も NoData
Execute                 → CopyOutResponse（text、列数、各列の形式 0）、CopyData × n 行、CopyDone、CommandComplete(COPY n)
```

- Describe は `NoData`（COPY は行を返す文ではない）。Execute の `max_rows` は**無視**（【実機】1 を指定しても全部出る）。Bind の結果形式（`[1]`）も無視する。`PortalStrategy` は `MultiQuery`。
- 実行は 1 つのスナップショット（Execute のとき）で、`COPY (SELECT ...)` は SELECT と同じ計画・実行器で回す。途中のエラーは ErrorResponse（その後は他と同じ Sync まで読み捨て）。

COPY FROM STDIN（Extended。【実機】）:

```
P B (D) E [S] → ... BindComplete、CopyInResponse            ← ここで flush。server は COPY 受信ループに入る
d × n、c      → （成功）CommandComplete(COPY n)            ← flush されない
S             → ReadyForQuery                                  ← COPY の途中に来た Sync は無視される。コピーの後の次の Sync で返る
f（CopyFail） → ErrorResponse 57014 `COPY from stdin failed: <メッセージ>`、SkipTillSync、次の Sync で ReadyForQuery
```

- **COPY IN の間に来た `S`・`H` は無視する**（flush もしない）。`d` `c` `f` 以外の型は `08P01 unexpected message type 0xNN during COPY from stdin`（【記憶】）で、COPY を失敗させる（Extended は SkipTillSync）。
- データのエラー（列数の不足、型の不正）は ErrorResponse。M4 の COPY はエラーの後も CopyDone / CopyFail までデータを捨てる必要がある（クライアントは送り続ける）。Extended から始めた COPY は、その後 Sync まで読み捨てる。Simple から始めた COPY は、`CopyDone` / `CopyFail` を受けてから ReadyForQuery（M4）。
- tokio-postgres の `copy_in` は Bind・Execute・**Sync**、CopyData…、CopyDone・**Sync** の順に送る（【確認】`copy_in.rs`）。1 つ目の Sync は無視され、2 つ目の Sync で ReadyForQuery が 1 回返る。

COPY 受信ループ（server。M4 の Simple 版を共通化する）:

```rust
enum CopyOrigin { Simple, Extended }
enum CopyEnd { Done, Failed, Closed }
fn copy_in_loop(reader, writer, session, origin) -> io::Result<CopyEnd> {
    loop {
        match read_message(..)? {
            CopyData(chunk) => if let Err(e) = session.copy_data(&chunk, sink) { return fail(e) },   // エラーなら ErrorResponse、以降は CopyDone / CopyFail まで捨てる（Simple）か Sync まで捨てる（Extended）
            CopyDone => { session.copy_done(sink)?; return Ok(CopyEnd::Done) }                    // Extended は CommandComplete("COPY n") を書くだけ。Simple は続きの文を実行
            CopyFail(msg) => { session.copy_fail(&msg, sink)?; return Ok(CopyEnd::Failed) }
            Sync | Flush => {}                                                                      // 無視
            Terminate => return Ok(CopyEnd::Closed),
            _ => { 08P01 で失敗させる }
        }
    }
}
```

M4 の `Session` が持つ COPY の状態は「Simple の続きの文」を持つので、`CopyOrigin::Extended` のときは続きの文が無いことを表す（§11 C13）。

#### 6.7.3 CSV の規則（PostgreSQL 17 の `copyto.c` / `copyfromparse.c`。【記憶】。差分は TS が PostgreSQL と比べる）

出力（TO）:

- 区切り（既定 `,`）、行末 `\n`、NULL は引用符なしの NULL 文字列（既定は空文字列）。**空文字列の値は `""`（NULL と区別するため）**。
- 値に区切り・引用符（既定 `"`）・`\r`・`\n` が含まれる、または NULL 文字列と等しい、または**1 列だけの行で値が `\.`** なら、引用符で囲む。値の中の引用符は ESCAPE（既定は QUOTE と同じ）を前に付けて 2 重にする。`FORCE_QUOTE` の列は常に囲む（NULL は囲まない）。
- `HEADER`: 最初の行に列名（引用符の規則は同じ）。

入力（FROM）:

- 引用符の外の NULL 文字列は NULL。引用符で囲まれた値は NULL にならない（`FORCE_NULL` の列を除く）。`FORCE_NOT_NULL` の列は、引用符の外の NULL 文字列も文字列として扱う。引用符の中の改行は値の一部。`\.` だけの行（引用符なし）は入力の終わり（PostgreSQL 17 の挙動。18 で変わる）。
- `HEADER` の行は読み捨てる（列名の照合はしない。`HEADER MATCH` は `0A000`）。
- 列数の不足 `22P04 missing data for column "x"`、余り `22P04 extra data after last expected column`、引用符が閉じない `22P04 unterminated CSV quoted field`。CONTEXT は `COPY t, line 3, column a: "..."`（M4 §14.4 の `with_context`）。

### 6.8 ドライバの検証（XQ-7）

#### 6.8.1 進め方

1. **シナリオを言語に依存しない表で書く**（`tests/compat/drivers/scenarios.md`。TS が置き場所と実行の枠組みを持つ。各シナリオ: SQL、パラメータの値と型、期待する結果の行・タグ・エラーの SQLSTATE）。
2. **同じシナリオを PostgreSQL 17 と yuzhu の両方に流し、結果を JSON で出して比べる**（片方だけ通るものは、PostgreSQL が正解）。
3. 失敗したら、ドライバが送ったメッセージ列を採る。PostgreSQL 側は `log_statement = all`、`log_min_messages = debug1`。yuzhu 側は `RUST_LOG=yuzhu_server=debug`（`connection.rs` は受信メッセージの種類と SQL を `tracing::debug!` で出す）。**生のバイト列を見たいときは §7.2 のハーネスと同じ方法（TCP プロキシか、ドライバを `127.0.0.1:55432` の自作サーバに向ける）**。
4. シナリオは、ドライバを 5 回以上ループさせる（`prepareThreshold`・`prepare_threshold` = 5 の後で、名前付きの文とバイナリ形式に切り替わるため）。

#### 6.8.2 ドライバごとの期待動作

| ドライバ | 送るもの | yuzhu が満たすこと | 実行方法 | 状況 |
|---|---|---|---|---|
| **tokio-postgres 0.7.18**（`postgres` 0.19.14 は同じ） | `prepare`: `Parse(s<n>, 型なし)`、`Describe(S)`、`Sync`。`query` / `execute`: `Bind(無名ポータル、パラメータもバイナリ、結果もバイナリ)`、`Execute(0)`、`Sync`。`query_typed`: `Parse(無名, 型つき)`、`Bind`、`Describe(S)`、`Execute`、`Sync`。`Transaction::bind` は `Bind` + `Sync`、`query_portal(portal, n)` は `Execute(n)` + `Sync`（【確認】`query.rs`・`bind.rs`）。`copy_in` は §6.7。文のドロップで `Close(S)` + `Sync` | ParseComplete → ParameterDescription → RowDescription / NoData の**この順**（`prepare.rs` が順序を検査する）。静的な表に無い OID だけ `pg_type` を引く: `SELECT t.typname, t.typtype, t.typelem, r.rngsubtype, t.typbasetype, n.nspname, t.typrelid FROM pg_catalog.pg_type t LEFT OUTER JOIN pg_catalog.pg_range r ON r.rngtypid = t.oid INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid WHERE t.oid = $1`（`pg_range` が無ければ `42P01` で、フォールバックの版に切り替わる）。結果の `"char"`（OID 18）と `name` のバイナリ | `cargo test`（`yuzhu-server/tests/drivers_postgres.rs`。dev-dependency の `postgres` は既存）。型は std だけで書ける範囲（bool、i16、i32、i64、f32、f64、String、Vec<u8>、`SystemTime`（timestamp / timestamptz）、`Vec<T>`（配列）、numeric は自作の `FromSql`） | XQ-7 |
| **psycopg 3** | パラメータ付きの `execute` は無名の `Parse`・`Bind`・`Describe(P)`・`Execute`・`Sync`（libpq の `PQsendQueryParams`）。**`str` は OID 0（unknown）、`int` は値に応じて int2 / int4 / int8 / numeric、`float` は float8、`bool` は bool**（【記憶】）。既定はテキスト形式、`%b` と `binary=True` でバイナリ。5 回で名前付きの文（`_pg3_<n>`）。pipeline モードは `Flush`（`H`）も送る。SAVEPOINT（SQLAlchemy の接続時）は M6 | 型の推論（unknown の `str`）。`prepare_threshold` 後の名前付きの文の再利用と `DEALLOCATE`（Simple かどうかは未検証）。`cursor.copy()` の経路（Extended か Simple か。**未検証**）。サーバ側カーソル（名前付き）は M6 で `0A000` / 構文エラー | `uv run`（Python 3.12 + `psycopg[binary]`）。`tests/compat/drivers/psycopg3/` | XQ-7（TS-4 が CI に載せる） |
| **pgJDBC** | `Statement.execute` も `PreparedStatement` も Extended。`setString` は varchar (1043)、`setInt` は int4、`setLong` は int8、`setNull(Types.X)` は対応する OID。**`prepareThreshold`（既定 5）回目から名前付きの文と一部の型のバイナリ受信**（int2・int4・int8・float4・float8・bool・bytea・uuid・date・timestamp など。【記憶】）。`autocommit = false` の最初の文の前に `BEGIN` を同じバッチで送る。`setFetchSize(n)` + autocommit off で `Execute(n)` と `PortalSuspended`。`addBatch` は `Parse` 1 回 + `Bind` / `Execute` の繰り返し + `Sync`。`RETURN_GENERATED_KEYS` は `RETURNING *` を付ける（この行は【記憶】。§9 の 4） | **`reWriteBatchedInserts` の複数行 INSERT は `$n` が最大 65535**。`DatabaseMetaData` と未知の型 OID の `pg_type` の問い合わせ（`current_schemas(boolean)`・`= ANY(array)` が要る。**未検証**）は M6 の対象 | JDK 17 + 配布の jar（`java -cp postgresql.jar Scenario.java`）。`tests/compat/drivers/pgjdbc/` | XQ-7（TS-4） |
| **node-postgres（pg 8）** | `values` を渡すと `Parse(無名 / name, 型 OID なし)`、`Bind(全部テキスト)`、`Describe(P)`、`Execute(0)`、`Sync`。`name` つきは 2 回目から Parse を省く。`rowMode: 'array'`。pg-cursor / pg-query-stream は `Execute(n)` と `PortalSuspended`。型の変換は OID でテキストを解釈（`pg_type` を引かない）（この行は【記憶】。§9 の 5） | **型の OID がすべて 0 なので、推論が PostgreSQL と同じでなければならない**。int8 は文字列で返る | `npm ci`（`pg`、`pg-cursor`）+ Node 20。`tests/compat/drivers/node-pg/` | XQ-7（TS-4） |
| **psql 17** | `\bind`（無名の Parse・Bind・Describe(P)・Execute・Sync。パラメータはテキスト、型 OID 0） | `\bind` + `\g` の実行（`SELECT $1::int \bind 5 \g`、DML、エラー） | `tests/compat/psql/` | XQ-7 |

#### 6.8.3 共通のシナリオ（抜粋。全体は `scenarios.md`）

| ID | 内容 | 確かめること |
|---|---|---|
| D01 | `SELECT $1::int4`（1 つの値）、int2 / int8 / float4 / float8 / bool / text / bytea（TY）の往復 | 型推論と、パラメータ・結果のバイナリ（tokio-postgres・pgJDBC）またはテキスト（psycopg 3・pg） |
| D02 | `INSERT ... VALUES ($1, $2) RETURNING id` を 100 回（5 回目から名前付き） | 準備済み文の再利用、RETURNING、タグ `INSERT 0 1` |
| D03 | `SELECT * FROM t WHERE id = $1`（int8 の列に int4 / int8 / 文字列の値） | `$1` の型が int8 であること。pgJDBC の `setString` は varchar → `42883`（PostgreSQL と同じ） |
| D04 | numeric(10,2)・timestamp・timestamptz・date・uuid・配列の往復（TY・TD の型。バイナリ） | バイナリ形式 |
| D05 | `setFetchSize(2)` / `query_portal(.., 2)` / pg-cursor で 5 行を 3 回 | `PortalSuspended`、スナップショット（途中で別のセッションが INSERT しても見えない） |
| D06 | トランザクション内でエラー → `ROLLBACK`、続けて通常のクエリ | `25P02`、ReadyForQuery の `E` と `I` |
| D07 | 文を準備した後で `ALTER TABLE ... ADD COLUMN`（M6 までは `CREATE INDEX` と `DROP INDEX`）、同じ準備済み文を実行 | 世代の変化を越えて再解析（列が変わらないとき成功、`SELECT *` の列が増えたら `0A000`） |
| D08 | パイプライン（psycopg 3 の `pipeline()`、tokio-postgres の複数 future の join、pgJDBC の batch）で途中の 1 文がエラー | Sync まで読み捨て、ReadyForQuery が揃う |
| D09 | `COPY ... FROM STDIN` / `TO STDOUT`（tokio-postgres・psycopg 3） | XQ-6 |
| D10 | `SET application_name` / `SET TimeZone` の後の ParameterStatus と、タイムゾーンつきの値の出力 | XQ-D12 |
| D11 | 100 接続 × 1000 回の `SELECT $1::int` の並列実行 | ポータル・文の寿命と、リークのないこと（接続を閉じて `RegisteredSnapshot` が残らない） |

---

## 7. テスト

テストは「PostgreSQL 17 が正解」の原則に従い、**期待値を PostgreSQL で採ったもの**を中心にする。層は 4 つ。

| 層 | 場所 | 何を確かめるか |
|---|---|---|
| 共有（言語非依存） | `tests/slt/m5/extended/`（SQL レベルの PREPARE など）、`tests/protocol/`（生のメッセージ列の transcript。§7.2）、`tests/compat/drivers/`（§6.8） | PostgreSQL と yuzhu の**両方**に流して同じ結果になること |
| Rust（server） | `yuzhu-server/tests/extended.rs`、`drivers_postgres.rs` | プロセス内でサーバを起動して、生のメッセージと `postgres` クレートで確かめる |
| Rust（core） | `session/extended.rs`・`prepared.rs` の `#[cfg(test)]`、`analyzer/params.rs`、`types/binary.rs` | Session 単体（`CollectSink`）、型推論の表、バイナリの固定値 |
| 並行・障害 | `extended.rs` の並行テスト、TS-2・TS-3 | DDL との競合、VACUUM の horizon、切断 |

### 7.1 共有 slt（`tests/slt/m5/extended/`。Simple Query エンジンで動く）

SQL レベルの `PREPARE` / `EXECUTE` / `DEALLOCATE` / `DISCARD`。**期待値は PostgreSQL 17 で採取済み**（【実機】）。

| ファイル | 内容（抜粋） |
|---|---|
| `prepare_execute.slt` | `PREPARE q1(int) AS SELECT $1 + 1` → `EXECUTE q1(4)` = 5、`PREPARE q2 AS SELECT $1`（型は text）、`EXECUTE q2('a')`、`PREPARE q3(int, text) AS INSERT ... RETURNING`、UPDATE / DELETE の PREPARE と `EXECUTE` のタグ、引数が式（`EXECUTE q1(1 + 2)`、`EXECUTE q1(abs(-4))`）、文字列から int への暗黙の変換（`EXECUTE q1('7')` = 8） |
| `prepare_errors.slt` | `PREPARE q1 AS ...` の重複（42P05 `prepared statement "q1" already exists`）、`EXECUTE nope`（26000）、引数の個数違い（42601 `wrong number of parameters for prepared statement "q1"`、DETAIL `Expected 1 parameters but got 2.`）、型の違い（`EXECUTE q1('x')` が 22P02）、`PREPARE q AS SELECT $1 IS NULL`（42P18）、`PREPARE q(int) AS SELECT $2`（42P18）、`DEALLOCATE nope`（26000）、`PREPARE q AS BEGIN`（構文エラー） |
| `deallocate_discard.slt` | `DEALLOCATE q1` の後の `EXECUTE q1` が 26000、`DEALLOCATE ALL` のタグ `DEALLOCATE ALL`、`DISCARD ALL` のタグと、その後の `EXECUTE`（26000）、`BEGIN; DISCARD ALL` が 25001（その後トランザクションが失敗状態）、`DISCARD PLANS` / `SEQUENCES` / `TEMP` のタグ、`DISCARD ALL` が `SET` を戻す（`SET application_name` の後で `SHOW`） |
| `prepare_ddl.slt` | 準備した後の `DROP TABLE` と再作成（同じ形なら `EXECUTE` が成功）、`CREATE INDEX` を挟んでも成功、テーブルが無ければ 42P01、列の型が変わる再作成で 0A000 `cached plan must not change result type`（ALTER は M6 まで無いので DROP + CREATE） |
| `prepare_tx.slt` | `BEGIN; PREPARE ...; ROLLBACK;` の後も文が残る、失敗したブロックで `EXECUTE` が 25P02、`PREPARE` 自体はブロックの中で成功 |

`tests/run.sh` に `--engine postgres-extended` のジョブ（`--label pg-ext` / `yuzhu-ext`）を足し（TS-4）、**M1〜M4 の slt（数千文）を Extended Query で流す**。float の整形が違うもの（【確認】`research-slt.md`：`1e20` が `100000000000000000000`）には `skipif postgres-extended` を付ける。これは XQ の最大の網で、**XQ-2 の完了条件の 1 つ**にする。

### 7.2 生のプロトコルの transcript（`tests/protocol/`。PostgreSQL でも流せる）

メッセージの列と応答の列を**テキストの transcript**にして、PostgreSQL と yuzhu の両方に流す（CLAUDE.md「`tests/` のテストは本物の PostgreSQL に対しても実行できる」）。この章を書くときに使ったハーネス（Perl の 150 行ほど。リポジトリには入れない）と同じ作りで、TS が Rust の小さなランナー（`tests/tools/protocol/`。isolation と同じ置き方）にする。**XQ は transcript を書く**。

形式（行指向。`#` はコメント）:

```text
# 名前: suspend-exact-multiple          ← ファイル名と同じ
Q BEGIN                                 ← 送る。続けて "<" の行で応答を書く（Simple Query は ReadyForQuery まで）
< CommandComplete BEGIN
< ReadyForQuery T
P "" "SELECT g FROM generate_series(1,4) g"          ← Parse(名前, SQL, [OID...])
B pm "" pf=[] params=[] rf=[]                         ← Bind(ポータル, 文, パラメータ形式, 値, 結果形式)。値は 'text' / NULL / x'0102'
E pm 2                                                ← Execute(ポータル, 最大行数)
E pm 2
E pm 2
S                                                      ← Sync（応答は次の ReadyForQuery まで読む）
< ParseComplete
< BindComplete
< DataRow '1'
< DataRow '2'
< PortalSuspended
< DataRow '3'
< DataRow '4'
< PortalSuspended
< CommandComplete SELECT 0
< ReadyForQuery T
H                                                      ← Flush（応答は 0.5 秒の静けさまで読む）
D S name / D P name / C S name / C P name              ← Describe / Close
Q ... / X                                              ← Terminate
```

- 応答の行は ErrorResponse なら `< ErrorResponse 42P01 relation "v1" does not exist`（SQLSTATE とメッセージ。位置 `P=` と DETAIL は `pos=15 detail=...` を任意で）。RowDescription は `< RowDescription id(oid=20,len=8,mod=-1,fmt=0)`。**表の OID（`tbl=`）と時刻・pid は比べない**。
- 期待値は `tests/protocol/expected/*.out`（PostgreSQL 17 で `--record` して生成。レビューで目視する）。yuzhu はそれと一致するか、`# known-diff: <M5-XQ-Qn>` の行で差を明示する。

**transcript の一覧（XQ が書く。各行 1 ファイル）**

| 名前 | 確かめること（実機で採取済み） |
|---|---|
| `basic-flow` | `P B D(P) E S`、`D(S)` の `ParameterDescription` と `RowDescription`（形式は 0）、Bind の結果形式（0 個・1 個・N 個）が RowDescription と DataRow に出る |
| `suspend-select` | 5 行を max_rows = 2 で 4 回（`SELECT 1` の後 `SELECT 0`）。割り切れる場合（4 行・2 件ずつ）は 2 回目も `PortalSuspended`、3 回目が `SELECT 0` |
| `suspend-returning` | `INSERT ... RETURNING` の 3 行を 2 件ずつ。`INSERT 0 1` の後 `INSERT 0 0`。1 行だけ取って Sync しても 3 行入る |
| `portal-lifetime` | 暗黙のトランザクションでは Sync の後にポータルが無い（34000）、ブロックの中では残る、`COMMIT` で消える。無名の文は Sync の後も残り、`Q` で消える。名前付きの重複は 42P05（文）・42P03（ポータル。`cursor "dup" already exists`） |
| `execute-twice` | 完了した SELECT ポータルの再実行は `SELECT 0`、INSERT は 55000 `portal "p3" cannot be run`、max_rows は DML では無視 |
| `snapshot-at-bind` | ブロックの中で、SELECT を Bind → INSERT を Execute → SELECT を Execute（INSERT は見えない）、DML の Bind を先にして別の文で INSERT してから Execute（その行も対象）、中断した SELECT は後続の書き込みを見ない |
| `error-skip` | エラーの後に `Flush`・`Q` を送っても何も返らず、Sync で ReadyForQuery だけ返る。エラー後の新しいバッチは動く。`ErrorResponse` は Flush なしで届く。Parse だけでは何も返らない（flush されない）。`Flush` で届く |
| `pipeline-tx` | Sync なしの 2 つの INSERT（2 つ目が一意制約違反）で 1 つ目も巻き戻る。Sync を挟めば 1 つ目は残る。`BEGIN` を Execute して同じバッチで INSERT、ReadyForQuery `T`。`P B E Q S`（ReadyForQuery が 2 回） |
| `failed-block` | §5.7 の表の全行（Parse・Bind・Describe(S)・Describe(P)・Execute・Close・Sync、`COMMIT` のタグ `ROLLBACK`） |
| `bind-errors` | 個数の不一致（パラメータ・パラメータ形式・結果形式）の 08P01 の文言、形式コード 2（Execute で 22023。行 0 件なら成功）、text の不正入力 22P02（CONTEXT つき）、不正な UTF-8 と NUL（22021）、`NULL` のパラメータ |
| `binary-params` | int2 / int4 / int8 / bool / float8 / text / oid の長さ違反（08P01 と 22P03）、正常値、空の text、バイナリの結果（`SELECT 1::int4, 2::int8, true, 1.5::float8, 'x'`） |
| `binary-types` | §6.5.2 の表のすべての値を `SELECT <値>` でバイナリの結果形式で受け、バイト列を比べる（numeric・日付時刻・bytea・uuid・配列は各章の完了後） |
| `infer-basic` | §6.4.3 の表のうち比較・算術・INSERT・UPDATE・LIMIT・キャスト・SELECT 句 |
| `infer-errors` | 42P18（IS NULL・歯抜け・宣言の 0）、42P08 (a)(b)、42P02（`$0`）、42883（宣言の型との不整合）、42601（複数の文） |
| `stale-plan` | §7.1 の `prepare_ddl` と同じ（Extended の Parse・Bind・Describe(S)） |
| `empty-query` | `""`・`;`・コメントだけ。Describe(S) は ParameterDescription と NoData、Execute は EmptyQueryResponse |
| `utility-extended` | `SET` を Execute（ParameterStatus は ReadyForQuery の直前）、`SHOW` のバイナリ結果（text）、`EXPLAIN` を max_rows = 1 で（`PortalSuspended`）、`PREPARE` / `EXECUTE` の Describe、`VACUUM`（先頭なら成功、2 つ目以降は 25001 `VACUUM cannot be executed within a pipeline`、ブロックの中は `... inside a transaction block`） |
| `copy-extended`（XQ-6） | `COPY FROM STDIN` の Extended（Sync が無視される、CopyFail の 57014）、`COPY TO STDOUT`（text・CSV・max_rows の無視）、`COPY (SELECT ...) TO STDOUT` |
| `function-call` | `F` は 0A000 と ReadyForQuery（SkipTillSync にならない）。COPY の外の `d` `c` `f` は無視 |

### 7.3 `yuzhu-server/tests/extended.rs`（Rust。XQ-1・XQ-2・XQ-5）

`tests/server.rs` と同じく、プロセス内でサーバを立て（`TestCluster`）、**生のメッセージを送る小さなクライアント**（`Raw`。TS が共通部品として持つ）で確かめる。transcript（§7.2）を読み込んで流す部分と、transcript で書けないもの（時間・複数接続・内部状態）に分ける。

| ID | テスト | 期待 |
|---|---|---|
| E01 | transcript の全件を yuzhu に流す | `expected/*.out` と一致（`known-diff` 以外） |
| E02 | 切断: `P B E`（暗黙のトランザクションが開いたまま）で TCP を閉じる | 他の接続から見てそのトランザクションの行は無い。`TxnManager` の実行中一覧が空（XID を持っていた場合） |
| E03 | 切断: 中断中のポータルを持ったまま閉じる | 登録済みスナップショットが 0 になる（`TxnManager::registered_snapshot_count()`。テスト用。§11 C12）。VACUUM の horizon が進む（VC 完了後） |
| E04 | 中断中のポータルが VACUUM の削除を止める | A が `SELECT` を max_rows = 1 で中断、B が行を DELETE して VACUUM → 削除された行はまだ見え（A が続きを取ると全行が返る）、A がポータルを閉じてから VACUUM すると回収される（VC 完了後。TS-2 と共有） |
| E05 | Bind のロック待ち | A が `LOCK TABLE t IN ACCESS EXCLUSIVE MODE`（`BEGIN` の中）、B が `Bind`（`SELECT FROM t`）→ Execute を送る。B は待つ（`pg_locks` に待ちが出る）。A が `COMMIT` したら B が進む（LK 完了後） |
| E06 | Bind のロック待ちの後の再解析 | E05 の A が `DROP TABLE t; CREATE TABLE t(...)` して COMMIT。B の Bind が成功し、同じ形なら成功、形が違えば 0A000 |
| E07 | キャンセル | Execute 中に別の接続から CancelRequest → 57014、SkipTillSync、Sync で ReadyForQuery。アイドル中に届いたキャンセルは次のバッチの先頭で捨てられる |
| E08 | `statement_timeout` | `SET statement_timeout = 100`、`SELECT pg_sleep(1)` を Execute → 57014。Parse・Bind では起きない |
| E09 | `Q` の途中割り込み | 中断中の名前付きポータルを持つブロックの中で `Q`、続けて元のポータルの続きを Execute（ブロックの中では残る） |
| E10 | 巨大な Bind（10 万個のパラメータ、`$1`〜`$65535`、メッセージ 100MB） | 上限内なら成功、`$65536` は 42601、メッセージ長の上限超過は FATAL 08P01 |
| E11 | 同じ接続の大量のポータル（1 万個を Bind して Close なしで Sync） | 暗黙のトランザクションの終わりで全部消え、メモリが戻る |
| E12 | 100 接続の並列 `SELECT $1::int` を 1000 回 | 全部成功。サーバのスレッド数・メモリが増え続けない |
| E13 | 不正なメッセージ: 長さの不足した Bind、`D` の対象が `X`、本体に余り、未知の型バイト | 08P01 の ERROR（FATAL ではない）→ Sync で復帰。未知の型バイトと長さの不正は FATAL |
| E14 | `execute_simple` と Extended の混在 100 万メッセージの fuzz（`proptest` で乱数列。Sync を必ず挟む） | パニックしない、ReadyForQuery の数が Sync・`Q` の数と一致 |
| E15 | ParameterStatus の順序 | `SET application_name` / `SET TimeZone` を Execute し、Sync で `ParameterStatus`、`ReadyForQuery` の順 |
| E16 | `drivers_postgres.rs`（`postgres` クレート） | §6.8.2 の tokio-postgres の行のシナリオ D01〜D08・D10（D09 は XQ-6） |

### 7.4 型推論のテスト（`analyzer/params.rs`。XQ-3）

§6.4.3 の表の**すべての行**を、`(SQL, 宣言の型, 期待)` の配列にして、`analyze_with_params` に流す。期待は「確定した型の一覧」か「エラーの SQLSTATE とメッセージ（と DETAIL、位置）」。約 110 ケース。加えて次を単体で確かめる。

- `ParamCtx` の状態遷移（`reference` → `resolve` → `finish`）: 確定済みの型の節点、UNKNOWN の節点が残る (b)、全部 UNKNOWN で 42P18、衝突の 42P08 (a) と DETAIL の向き（`integer versus text`）。
- フック H1〜H6 の 1 つずつ（`coerce_type` が `ExternParam` を書き換えて返す、`resolve_unknown` が通る、`FuncNameAsType` の `int4($1)` が通る、`= ANY($1)` が配列型になる、`INSERT ... SELECT $1` が列の型になる）。
- `analyze`（パラメータなし）が `$1` を 42P02 にする（Simple Query）。DEFAULT・CHECK の `$1` も 42P02。
- 再解析（`declared` が全部確定）で、元の推論と同じ型の木ができる（Parse の解析結果と Bind の再解析結果が `ExternParam` の型まで等しい）。
- `ExternParam` を含む計画（`WHERE id = $1` が IndexScan のキーになる、`LIMIT $1` が `Limit` の式になる、畳み込まれない）と、`eval` が `bind_params` を読むこと。EXPLAIN の deparse が `$1` を出すこと。

### 7.5 バイナリのテスト（`types/binary.rs`。XQ-4、TY-6、TD-5）

1. **固定値**: §6.5.2 の表（PostgreSQL 17.11 の出力）を `(型, 値, バイト列)` にして、`output_binary` が一致、`input_binary` が値に戻る（`cmp_datum` で等しい）。
2. **往復**: すべての `supports_binary` な型の `Datum`（境界値: int の最小・最大、float の NaN・±Infinity・−0、numeric の NaN・±Infinity・大きな scale、日付時刻の ±infinity、長さ 0 と 1MB の text / bytea、NULL 要素を含む配列）で `input_binary(output_binary(d)) == d`。
3. **エラーの表**: §6.5.3 の入力を型ごとに（短い・余り・不正な内容）流して、SQLSTATE とメッセージが PostgreSQL と一致（Bind 経由の `22P03` に `in bind parameter N` が付くこと）。
4. **fuzz**: 乱数のバイト列を `input_binary` に流して**パニックしない**、返す `Err` の SQLSTATE が 08P01 / 22P03 / 22021 / 22003 / 22008 / 0A000 のどれか。`proptest`。
5. **PostgreSQL との差分**: 1 の値を PostgreSQL に `SELECT` して結果をバイナリで受け、yuzhu の `output_binary` と同じバイト列であることを確かめるテスト（`yuzhu-server/tests/binary_diff.rs`。`PG_TEST_PORT` があるときだけ動く。TY-7 の差分コーパスの仕組みを使う）。

### 7.6 並行・障害（TS と共有）

- **DDL と準備済み文**: 100 スレッドが同じ準備済み文を Bind / Execute し続ける間に、別のスレッドが `CREATE INDEX` / `DROP INDEX` を繰り返す。全員成功する（結果が変わる列の変更はしない）。再解析の回数が DDL の回数程度。
- **中断中のポータルと horizon**: E03・E04。
- **COPY の切断・失敗**（XQ-6）: COPY IN の途中の切断、CopyFail、データのエラー、で暗黙のトランザクションが中断し、サーバが復帰する。
- **メモリ**: 100 万回の Parse / Bind / Close を繰り返しても `ExtStore` のサイズが増えない。名前付きの文を 100 万個作って `DEALLOCATE ALL`。

---

## 8. 実装の分担と工数

担当は 00 §7 の WP のとおり（日数は AI の実装エージェント 1 本の稼働日。粗い見積もり）。**XQ-2 は 4 日から 5 日に増やしたい**（§11 C11。ポータルの実行状態の保持と RETURNING の保存、Bind の再解析ループがあるため）。ほかは 00 のまま。合計は 20.5 日（00 の XQ 合計）→ **21.5 日**。

| WP | 内容 | ファイル（持ち主はこの章） | 依存 | 日数 | 完了条件 |
|---|---|---|---|---|---|
| **XQ-1** | サーバのメッセージループ | `yuzhu-server/src/{connection.rs,protocol/*}`、`tests/extended.rs` の枠 | F0 | 2.5 | §5.1 の表のとおり動く（Session は `msg_*` のスタブが `0A000` を返す状態で、`SkipTillSync`・flush・`F`・`d` `c` `f` の無視・本体の不正・PS の順序が `extended.rs` の E13・E15 で通る）。`Sink` の flush と `data_row_raw` |
| **XQ-2a** | 文とポータル、暗黙のトランザクション、Sync、Describe、Close | `session/extended.rs`（ExtStore、Portal、msg_*）、`session/mod.rs` の 3 行 | XQ-1、M4（`PhysicalQuery`、`BoundStatement`） | 2.5 | transcript の `basic-flow`・`portal-lifetime`・`pipeline-tx`・`failed-block`・`empty-query`・`error-skip`・`bind-errors` が通る。再解析なし（世代の鍵は比べるが、変わったら `0A000` で止める仮） |
| **XQ-2b** | ポータルの実行（OneSelect の中断、OneReturning・UtilSelect の保存、MultiQuery）、行の符号化、スナップショットの保持、CCI | 同上 | XQ-2a、M4 の executor（`ExecState` の出し入れ）、LK-3（`RegisteredSnapshot`） | 2.5 | `suspend-*`・`snapshot-at-bind`・`execute-twice`・`utility-extended` が通る。**M1〜M4 の slt が `--engine postgres-extended` で通る**（`skipif` 以外）。E03・E04 |
| **XQ-3** | `$n` の字句・AST・解析、`ParamCtx`、フック H1〜H6、`ExternParam` の全層、42P18 / 42P08 / 42P02 | `sql/{lexer.rs,ast.rs,parser/expr.rs}`、`expr/mod.rs`（+ walk・deparse・eval・planner の網羅 match）、`analyzer/params.rs`、`analyzer/{coerce,resolve,expr,select,dml}.rs` の分岐 | M4（analyzer、planner） | 3.5 | §7.4 の約 110 ケースが通る。`infer-*` transcript が PostgreSQL と一致。planner が `ExternParam` を畳み込まず、インデックスのキーにできる |
| **XQ-4** | バイナリ I/O の振り分けと M1 の型 | `types/binary.rs`、`RecvBuf` | F0 | 1.5 | §6.5.2 のうち M1 の型の固定値、往復、fuzz、エラーの表。`binary-params` transcript。型ごとの `binary_send` / `binary_recv` の口を TY-6・TD-5 に渡す |
| **XQ-5** | 準備済み文の世代と再解析、`PREPARE` / `EXECUTE` / `DEALLOCATE` / `DISCARD` | `session/prepared.rs`、`sql/parser/prepare.rs`、`sql/ast.rs` の 4 つの文、`analyzer/params.rs` の `analyze_execute_param` | XQ-2、LK-4（ロックの口） | 3 | §7.1 の 5 つの slt、`stale-plan`、E05・E06、DDL との並行（§7.6） |
| **XQ-6**（任意） | `COPY ... TO STDOUT`、CSV、Extended 経由の COPY | `copy/{to.rs,csv.rs}`、M4 の `copy/mod.rs`・`from.rs` に分岐、`session/extended.rs` の `CopyIn`、`connection.rs` の COPY 受信ループ、`ResultSink` の `copy_out_*` | XQ-2、M4 の COPY | 3（CSV の FROM と TO を含めると 4 になりうる） | `copy-extended` transcript、`COPY` の slt（TO・CSV）、tokio-postgres の `copy_in` / `copy_out`（D09）、pgbench `-i` が `COPY` を使えること |
| **XQ-7** | ドライバの検証 | `tests/compat/drivers/*`、`yuzhu-server/tests/drivers_postgres.rs` | XQ-5（と TY・TD の型） | 3 | §6.8.3 のシナリオ D01〜D11 が 4 つのドライバと psql で PostgreSQL と同じ結果。**`pgbench -M extended` と `-M prepared` の tpcb-like（`-c 4 -T 30`）が完走**（M4 の完了条件 D-25 は `-M simple`。ここで 2 つ増える） |

**依存と並列**: XQ-1 と XQ-4 は F0 の直後から並行して始められる（Session は触らない）。XQ-3 は M4 の analyzer が済んでから（M4 のマージ前は設計と `ParamCtx` の単体だけ）。XQ-2a は XQ-1 の `Sink` と `handle_ext` が動いてから。XQ-5 と XQ-6 は XQ-2b の後で並行。XQ-7 は XQ-5 と TY・TD の型が揃ってから（型は揃った分から）。

**カットライン**（00 §1.3）: XQ-6 は M6 に回してよい。回しても、tokio-postgres の `copy_in` を使わないアプリと psql の `\copy` 以外は影響を受けない（pgbench の初期化は Simple Query の COPY FROM）。

**M4 が済むまでの動き方**（D49。レビュー対応 R-17 で改訂）: **XQ の WP は F0（M4 のマージ後）に依存し、00 D49 / §1.3 の先行可能リスト（AU-1、TD-4 の Time、LK-1・LK-2）に XQ は含まれない**。XQ-1 は既存の `yuzhu-server/src/connection.rs`・`protocol/messages.rs`・`codec.rs`（§2 の △）を変更し、XQ-4 も `types/mod.rs`・`io.rs` の既存ファイルに触れる。M4 の COPY（`copy/` と `connection.rs` の COPY 受信ループ）も同じファイルに触れるので、M4 の実装中に始めると衝突する。**M4 の実装中に始めてよいのは、新規ファイルだけの純粋な部分に限る**: `types/binary.rs` の `RecvBuf` と `BinaryCodec` の枠（単体テストまで）、`analyzer/params.rs` の `ParamCtx`（単体テストまで）、transcript の生成（`tests/compat/` の下書き。実機の PostgreSQL だけで作れる）。フック H1〜H6、`connection.rs`・`messages.rs`・`codec.rs` の変更は M4 のマージ後に足す。

---

## 9. 未検証の点

実装前に確かめる。**出典**を付ける。

1. **M4 の最終的な構造**: `Analyzer` の `&self` の構造、`coerce_type`・`resolve_unknown` の位置、`ExecCtx` の所有する状態を `take` できること（`SubPlanStates`・`CteStates` の `Default`）、`PhysicalQuery` の形、`row_to_text` の後継。M4 の章 04（planner）・06・07・09・10 が確定したら、XQ-2・XQ-3 の担当は最初に突き合わせる（00 §10.2 と同じ）。
2. **`yuzhu-numeric` の `to_binary` / `from_binary`** が §6.5.2 の numeric の固定値（特に `'infinity'` の dscale 欄 `0x0020`。PostgreSQL 17.11 の実測。16.x の `numeric_send` と同じかは未確認）と検査（符号・dscale・digit の範囲）を満たすか。差分コーパス（`yuzhu-numeric/tests/pg_corpus.rs`）に send / recv を足す（TY-6）。
3. **psycopg 3**: `str` を unknown（OID 0）で送ること、`cursor.copy()` が Extended か Simple か、`pipeline()` の Flush（`H`）の使い方、`prepare_threshold` 後の名前付きの文の掃除（`DEALLOCATE` を Simple で送るか Close か）。手元に Python が無く未確認（【記憶】）。XQ-7 で `log_statement = all` と生のメッセージで確かめる。
4. **pgJDBC**: `prepareThreshold` 後にバイナリを要求する型の一覧と条件、`TypeInfoCache` の問い合わせの中身、`reWriteBatchedInserts` の `$n` の数、`autocommit = false` の `BEGIN` が同じ Sync の中に入ること。未確認（【記憶】、ソースは手元に無い）。
5. **node-postgres**: `Describe(P)` と `Describe(S)` のどちらを使うか（`name` つきの 2 回目以降）、`rowMode`、pg-cursor の `Execute(n)`。未確認。
6. **`statement_timeout` の Extended での数え方**: PostgreSQL は Parse から Sync までを 1 つの期間と見る可能性がある（【記憶】PG 13 以降）。yuzhu は 1 メッセージごと（M5-XQ-Q5）。
7. **`EXECUTE` の引数の強制変換のエラーの文言**（`parameter $1 of type integer cannot be coerced to the expected type text`、HINT）、`cannot use subquery in EXECUTE parameter`、`aggregate functions are not allowed in EXECUTE parameters`（【記憶】）。PostgreSQL 17 で採取する。
8. **Describe（ポータル）の失敗したブロックでの規則**（`columns` が無いポータルは成功か）。文は【実機】、ポータルは SELECT のポータルだけ【実機】。
9. **`DISCARD ALL` が `SET SESSION AUTHORIZATION` と一時テーブルとアドバイザリロックを戻す**こと（M5 には存在しないので no-op）。ポータルを閉じるか（ブロックの外ではポータルは自分だけ）。
10. **COPY の CSV の細部**（引用符・エスケープの組み合わせ、`FORCE_*`、`\.`、ヘッダ、エラーの CONTEXT）は【記憶】。PostgreSQL 17 に対して差分試験（TS）で確かめる。
11. **関数の候補の集合**に依存する推論（`abs($1)` → float8、`round($1)` → float8、`substring` ほか）。M4・TY が登録した `pg_proc` の行が PostgreSQL と同じ候補を持つか。違えば `func_select_candidate` の結果が変わる。§7.4 の表のうち関数の行は、候補の登録が揃ってから通る。
12. **42P08 (b) の位置**（PostgreSQL は問い合わせの木を歩いて最初の食い違う節点。yuzhu は文中の位置が最小のもの。`UNION`・副問い合わせ・CTE を含む文で食い違いうる）。
13. **PREPARE と暗黙のロック**: PostgreSQL の `PREPARE` は解析のときに AccessShare を取り、トランザクションの終わりまで持つ。yuzhu の Parse はロックを取らない（D21）。観測できる差は、ブロックの中の `PREPARE` の後に別のセッションの DDL が通る点だけ。
14. **`StoredRows` のメモリ**: `INSERT ... RETURNING` で数百万行を返す文は、全行を `Vec<Option<Vec<u8>>>` で持つ。`yuzhu.query_mem_limit` を超えたら 53200（PostgreSQL は tuplestore が work_mem を超えるとディスクに書く。M6）。
15. **`ParameterDescription` の個数が 65535 を超える**文（`reWriteBatchedInserts` が上限に近づく）。`$65535` まで動くことを E10 で確かめる。
16. **`Bind` のロックの順序**: `lock_statement_relations` が FROM の出現順に取る（LK-4）。準備済み文の再解析で参照するリレーションが変わるとき、ロックの順序が文ごとに違いうる。デッドロックは検出される（LK-2）が、頻度を TS-2 で見る。
17. **pgbench の `-M extended` / `-M prepared`**: pgbench が送るメッセージ（`Parse` の型 OID、`Describe`、バイナリの有無）は PostgreSQL 17 の pgbench のソースで確かめる。手元に pgbench はあるが（`/usr/lib/postgresql/17/bin`）未実行。

---

## 10. 確認事項（仮決めした点。ユーザーに確認したい）

「仮決め・理由・変えたい場合の影響」。ID は `M5-XQ-Q<n>`。10 章が集める。

| ID | 仮決め | 理由 | 変えたい場合の影響 |
|---|---|---|---|
| M5-XQ-Q1 | `pg_prepared_statements` は M5 で作らない（XQ-D14）。M6 | 仮想リレーション（D33）の `VirtualCtx` にセッションの情報が無く、`regtype[]` の型の行（OID 2211）も要る。ドライバは使わない | 作るなら +1.5 日。`VirtualCtx` に `prepared: &[PreparedInfo]`（`SessionInfo` に `Arc<[PreparedInfo]>` を足し、文ごとに写す）と `catalog/builtin` の `_regtype` 行を足す（契約変更が要る）。psycopg 3 / pgbouncer の運用ツールが `pg_prepared_statements` を引く場合に必要 |
| M5-XQ-Q2 | 汎用プランを作らず、解析結果（Bound）だけ世代の鍵でキャッシュし、計画は Bind ごと（XQ-D6、D19）。`plan_cache_mode` は保存するだけ | M4 の planner は軽い（ルールベース。統計なし）。パラメータ値で計画が変わる場面は少ない | 計画のキャッシュを足すなら +2〜3 日（`Arc<PhysicalQuery>` を `PreparedStmt` に持ち、`ExternParam` を含む計画を再利用。鍵が合えば Bind で計画を省く）。`pgbench -M prepared` の速度が足りないときの手段 |
| M5-XQ-Q3 | 結果の形式コードを Bind で検査する（XQ-D15）。PostgreSQL は Execute で最初の非 NULL の行を出力するときに 22023 | 早く失敗する方が単純 | PostgreSQL と同じにするなら、形式コードをそのまま持ち、`encode_value` で検査する（+0.3 日）。差が出るのは不正な形式コードを送る壊れたクライアントだけ |
| M5-XQ-Q4 | 定数畳み込みで PostgreSQL が Bind のときに出すエラー（22001・22003・22012 など）を、yuzhu は Execute で出す（XQ-D11）。BindComplete の有無だけが違う | 実行器が `ExternParam` を実行時に評価する。planner に畳み込みを入れない | Bind で畳み込むなら planner に「パラメータ値を既知として畳み込む」モードが要る（+2 日）。ドライバへの影響はない（エラー後の動きが同じ） |
| M5-XQ-Q5 | `statement_timeout` は Parse・Bind・Execute の 1 メッセージごとに数える | M3 D31 の「文ごと」の延長 | PostgreSQL がバッチ全体を 1 期間と見るなら、最初のメッセージで期限を決めて Sync まで持つ（+0.5 日）。パイプラインで多数の文を送るアプリで合計時間の上限が効く |
| M5-XQ-Q6 | Describe（文）が再解析で 0A000 になるとき、ParameterDescription を送らずエラーだけ送る（XQ-D21） | `msg_describe_statement` が Result を返す契約 | PostgreSQL は ParameterDescription の後にエラーを送る。同じにするなら `StatementDescription` にエラーを持たせる（+0.3 日）。tokio-postgres の `prepare` は ParameterDescription の後のエラーも受け取る（【確認】`prepare.rs` は順に読む）ので、どちらでも壊れない |
| M5-XQ-Q7 | Parse で宣言の OID が未知なら `XX000 cache lookup failed for type N`（PostgreSQL と同じ） | 【実機】 | `42704 type with OID N does not exist` にする方が親切（影響なし） |
| M5-XQ-Q8 | XQ-6（COPY TO・CSV・Extended の COPY）は任意で、入れる前提で見積もる（3〜4 日） | pg_dump・`\copy`・tokio-postgres の `copy_in` が使う | 外すと M6。pg_dump の出力のリストアと psql `\copy ... to` が動かない。CSV の FROM を M6 に回す（TO だけ M5）なら −1 日 |
| M5-XQ-Q9 | `DISCARD TEMP` は何もしない（一時テーブルが無い） | `CREATE TEMP TABLE` は M5 の範囲外 | 一時テーブルを入れるときに実装 |
| M5-XQ-Q10 | `F`（FunctionCall）は 0A000 | 古いプロトコル。ドライバは使わない（libpq の `PQfn` だけ） | 実装するなら +1 日（`pg_proc` の関数呼び出しと `V` メッセージ） |
| M5-XQ-Q11 | Parse は名前付きの文を、宣言の個数が使われた個数より多くても受ける（0 でない OID のみ）。ParameterDescription は宣言の個数 | 【実機】 | — |
| M5-XQ-Q12 | `SELECT` のポータルをストリーミングで送るので、**Extended では途中のエラーまでの行がクライアントに届く**。Simple Query は M2 から「溜めてから送る」まま | PostgreSQL と同じ。Simple の方式を変えると M1〜M4 の slt に影響 | Simple も同じにするなら `execute_simple` を `Output` から sink への直接書き込みに変える（+1 日。M2 のデータベースロックを前提にした作りは D10 で不要になる）。slt の期待値は変わらない（エラー時に行を出すテストが無い） |
| M5-XQ-Q13 | 暗黙のトランザクションが開いたままのとき（`P B E` の後）の `Q` は、そのトランザクションを引き継いで `Q` の終わりでコミットする（XQ-D2） | 【実機】 | — |
| M5-XQ-Q14 | 1 つの接続が持つ準備済み文・ポータルの数に上限を置かない | PostgreSQL も置かない | 上限（例 10 万）を置けば、悪意のクライアントがメモリを使い切るのを防げる（+0.2 日。超えたら `53200`）。接続プールの運用で必要になったら |

---

## 11. 契約への変更依頼

00 の契約と食い違う点と、他の章の担当への依頼。黙って変えず、ここに書く。**C1〜C5 は 00 の記述の修正（食い違い）、C6〜C10 は M4・LK・F0 など他の担当への依頼（口の追加）、C11〜C13 はそのほか。**

| # | 対象 | 内容 | 理由 |
|---|---|---|---|
| C1 | 00 §5.1 の最後の項目（「Extended Query は、Bind が 1〜4 を行い…、Execute が 6 を行う」） | 「**計画する文（SELECT・DML・EXPLAIN）の Bind は、RR のとき 0'（最初のスナップショットをロックの前に取る。02 RW-D10、01 LK-D27）に続けて 1・2（ロック、世代の比較と再解析）を行い、SELECT 系のポータル（`OneSelect`）だけ 3・4（XID の割り当て、スナップショット）も Bind で行う。DML のポータルは 3・4 を Execute で行う。DDL・COPY・ユーティリティ文は Bind で何もせず、Execute で 0'〜6 をすべて行う**」に直す（XQ-D3）。**00 §5.1 は R-03 で上のとおり反映済み**（02 依頼 1 と同じ節の別の直しだったものを 1 つにした）。この章は 02 §4.5 の `statement_snapshot` / `note_snapshot_use` / `StatementSnapshot` を使い、独自の取得関数は持たない | 【実機】SELECT のポータルは Bind のスナップショットを使い、DML は Execute のときのスナップショットを使う。RR の最初のスナップショットはロックの前（RW-D10）。`txn_snapshot_taken` は RW-D11 の規則 |
| C2 | 00 §4.7 `ExecOutcome` | `CopyIn` を足す（XQ-6）。`ResultSink` に `copy_out_response` / `copy_out_data` / `copy_out_done` を足す（既定の実装は `Err(Unsupported)`） | COPY の Extended と TO |
| C3 | 00 §4.7 `Session` | `pending_parameter_status(&mut self) -> Vec<(String, String)>` を足す。00 §4.12 の flush の規則に「ErrorResponse・NoticeResponse・CopyInResponse の後も flush する」を足す（XQ-D12、D13） | `msg_sync` に sink が無い。【実機】ErrorResponse は Flush なしで届く |
| C4 | 00 §3.8 の「準備済み文」の行 | ポータルの重複の文言を **`cursor "p" already exists`**（42P03）に直す（`does not exist` は `portal "p" does not exist` のまま。【実機】）。無名の文が無いときの文言 `unnamed prepared statement does not exist` を足す | 【実機】 |
| C5 | 00 §4.9 `input_binary` の注記 | 「長さの過不足は 22P03」を「**不足は `08P01 insufficient data left in message`、余りは `22P03`**」に直す（XQ-D10）。`BINARY_TRAILING_MSG` と `RecvBuf` を足す | 【実機】 |
| C6 | M4 の executor（`ExecCtx`、`SubPlanStates`、`CteStates`、`MemBudget`） | 中断中のポータルが `ExecCtx` の所有する状態を持ち越せるよう、`SubPlanStates: Default`、`CteStates: Default`、`MemBudget` を `std::mem::take` / `replace` できる形にし、`ExecCtx` の所有フィールド（`params`、`mem`、`subplans`、`ctes`）を pub のまま保つ。`ExecState` という名前の束ね（`struct ExecState { params, mem, subplans, ctes }`）を `executor/mod.rs` に置いてくれれば、この章はそれを使う | §5.9 |
| C7 | LK-4（`session/locking.rs`） | Bind と Describe（文）のために、`lock_statement_relations(&mut self, stmt: &Statement) -> Result<Vec<(Oid, LockMode)>>`（生のパース木から集めて解決 → ロック → 再解決。§5.1 d.1〜2）を `Session` の内部関数として切り出す。解析結果から OID の一覧を持ち、名前の解決を省いて OID でロックする最適化は M6。**M4 のマージ後の F0 が `session/locking.rs` を作るときに、この形で切る** | D21。Simple Query の文の実行と同じ取得手順を共有する |
| C8 | F0（`session/mod.rs`・`simple.rs` の分割） | §6.2.5 の部品（`begin_implicit_if_idle`、`catalog_snapshot`、`assign_xid_for_write`、`exec_ctx_parts`、`exec_statement`）と、M3 の `report_error` から「状態の遷移とエラーの整形」を取り出した `fail(e, sql) -> Error`（§6.2.4）を `pub(super)` で切る。`commit_transaction` と `rollback_transaction` の先頭に `self.ext.end_of_transaction()` の 1 行、`execute_simple` の先頭に無名の文・ポータルの破棄の 1 行、`begin_transaction` に `txn_serial += 1` の 1 行を足す | 00 §8 の「session/mod.rs は F0 が作り、各章が自分の行を足す」の範囲 |
| C9 | M4 の planner（`physicalize.rs`・`rules.rs`・`index_select`） | 「実行時に 1 回評価できる式」（インデックスのキー・`Limit`・`RangeBound`・`NestedLoopParam` のキー）の判定に `ExprKind::ExternParam` を含める。畳み込みの対象にしない。`LIKE` の前置一致の範囲化は `Literal` だけ | §6.4.4。`WHERE id = $1` が IndexScan にならないと、ORM のクエリが全表走査になる |
| C10 | TY・TD | `types/numeric.rs`・`bpchar.rs`・`bytea.rs`・`uuid.rs`・`array.rs`・`datetime.rs`・`interval.rs`・`time.rs` が、`pub fn binary_send(d: &Datum, ty: SqlType) -> Result<Vec<u8>>` と `pub fn binary_recv(buf: &mut RecvBuf<'_>, ty: SqlType) -> Result<Datum>` を持つ（§4.4 の `BinaryCodec`）。配列は要素型の `codec(elem_oid)` を再帰で引く | D20。XQ-4 が表を作り、型ごとの実体は各章 |
| C11 | 00 §7 の WP | **XQ-2 を 4 日 → 5 日**（XQ-2a 2.5、XQ-2b 2.5）。XQ-6 は「3（CSV の FROM と TO を含めると 4）」。合計 20.5 → 21.5 日 | §8 |
| C12 | LK-3 | テスト用に `TxnManager::registered_snapshot_count() -> usize`（`#[cfg(any(test, feature = "testing"))]`）を足す | E03 |
| C13 | M4（`copy/`、`session` の COPY 状態） | XQ-6 が `copy/to.rs`・`copy/csv.rs` を新規に足し、M4 の `copy/mod.rs`・`from.rs` に CSV の分岐を、`session` の COPY 状態に `origin: CopyOrigin`（Simple は続きの文を持つ、Extended は持たない）を足す。`copy_done` は origin が Extended なら続きの文を実行しない。`Session::is_copying_in()` / `copy_data` / `copy_done` / `copy_fail` の署名は M4 のまま | §6.7。M4 の COPY の担当と調整する（00 §8 に `copy/` の持ち主の行が無い） |
| C14 | 00 D19（Extended Query の計画） | D19 の「Bind ごとにアナライズからやり直す。準備済み文は SQL・型・結果の列・参照リレーションの一覧・カタログ世代を持つ」を、**「Bind ごとに計画（planner）をやり直す。解析結果（`Arc<BoundStatement>`）は `AnalysisKey`（世代・search_path・DateStyle・TimeZone）が変わらず自分のトランザクションがカタログを変更していなければ使い回す。準備済み文は SQL・AST・型・結果の列・解析結果・解析の鍵を持つ。参照リレーションの一覧は持たず、Bind のたびに生のパース木から集めてロックする（C7）」** に改める（XQ-D6、XQ-D7）。**00 D19 は R-15 で反映済み** | 再解析を毎回行うと、ORM の同じ文の Bind ごとに名前解決と型付けを繰り返す。鍵が変わらなければ解析結果は同じ（DDL のコミットは世代を進め、自分の未コミットの DDL は `catalog_dirty` で拾う）。ロックは毎回 Bind が取り直すので、鍵の判定は「ロックを取った後」に行う（5.5 の手順 6）。10 章の KD-20・M5-Q36 も合わせた |
| C15 | 00 §4.6 `ExecCtx` | `ExecCtx` の組み立て（5.9）に `locks: &Arc<LockManager>` と `procs: &Arc<ProcArray>` を渡す（`executor::dml::wait_for_xid` が使う。02 §4.4・§4.6、R-05）。00 §4.6 は R-05 で `procs` を足して反映済み | RW の `wait_for_xid` が `ctx.locks.wait_for_xact` と `ctx.procs.is_in_progress` を使う |

**C1〜C15 のうち、00 の本文を直すもの（C1、C3、C4、C5、C14、C15）は、レビュー対応（00 §6.3 の台帳）で 00 に反映した**（C3・C4 の細部は 00 §3.8・§4.7 の表）。反映前の記述との差があれば、この章の記述が優先する。

---
