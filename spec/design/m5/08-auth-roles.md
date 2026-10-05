# yuzhu M5 基本設計 08: 認証とロール（SCRAM-SHA-256・yuzhu_hba.conf・ロールの DDL）

M5 のゴールのうち「パスワード認証（SCRAM-SHA-256）で守られる」部分を作る章です。接続の前に**パスワードで本人を確かめ**（`yuzhu_hba.conf` が方式を決める）、接続の後に**ロールの属性**（LOGIN・SUPERUSER・CREATEDB・CONNECTION LIMIT ほか）を見て、`CREATE / ALTER / DROP ROLE` でそれを管理できるようにします。TLS・GRANT / REVOKE・ロールの所属（メンバーシップ）は M6 なので、**M5 のロールは「認証と属性」だけで、データのアクセス制御はまだしません**（§1.4）。

- 略号は **AU**（作業パッケージ AU-1〜AU-3）。契約は `spec/design/m5/00-contracts.md`（以下「00」）に従う。担当する決定は **D28・D29・D33**、型は **§4.8（`Cluster` と認証）・§4.11（`yuzhu-auth`）・§4.12（`hba.rs`）**。00 と食い違う点は §11 に書いた（章の中で黙って変えない）。
- 前提の設計書: `spec/design/m1.md`（プロトコル、`connection.rs`）、`m2.md`（§6.8 カタログ、§6.9.1 接続、§5.8 initdb）、`m3.md`、`spec/design/m4/00-contracts.md`（`ddl/`、`DdlCtx`、`CatalogReader`）。同時に書かれている 01（LK。`LockManager`、仮想リレーションの仕組み）、06（TD。`Clock`）、09（DB。`Cluster::connect` の接続数）と境界を接する（§6.9）。
- 調査（根拠）: `spec/research/m5-protocol-auth.md` §4（SCRAM）、§5（ロールの DDL）、§6（pg_hba）、§8（テスト）、§10（確認事項 C-4〜C-9、C-13）。PostgreSQL 17 が正解。
- 根拠の記号は 00 §9 と同じ。【確認】ソース（REL_17_STABLE。`PG:<path>` は `https://github.com/postgres/postgres/blob/REL_17_STABLE/<path>`）・RFC・手元の計算で確かめた。【記憶】未照合。【提案】yuzhu への推奨。「（未検証）」は §9 に集めた。
- この設計書の作成時に、**RFC 7677 §3 のテストベクタ**（ClientProof、ServerSignature）と、そこから導いた StoredKey・ServerKey、RFC 7914 §11 の PBKDF2 のベクタを `openssl` と `perl` で再計算し、§3.6 の固定値と一致することを確かめた【確認】。単体テストの固定値はそのまま写してよい。

---

## 0. 決定（この章で扱う論点。調査間の食い違いも）

`AU-Dn` はこの章の決定。00 の D28（認証方式）・D29（暗号の依存）・D33（仮想リレーション）はそのまま実装の形にする。

| # | 論点 | 選択肢（出典） | 決定 | 理由 |
|---|---|---|---|---|
| AU-D1 | SCRAM の実装 | SCRAM 全体を実装した外部クレートを使う／状態機械は手書きし、ハッシュ・HMAC・PBKDF2・base64・乱数・SASLprep だけ外部クレート（調査 §4.6） | **後者。状態機械・メッセージの解析・秘密情報の形式は `yuzhu-auth` に手書きする（約 400 行）。外部クレートは §6.1 の 7 つだけ** | サーバ側の SCRAM を実装した保守されたクレートが見当たらない（`postgres-protocol` はクライアント側）。CLAUDE.md は暗号を外部クレートの許容範囲とするので、プリミティブだけを借りる |
| AU-D2 | クレートのバージョン | 調査は版を決めていない | **`sha2` 0.11.0、`hmac` 0.13.0、`pbkdf2` 0.13.0（`default-features = false`、`features = ["hmac"]`）、`base64` 0.22.1、`getrandom` 0.4.3（`features = ["std"]`）、`subtle` 2.6.1、`stringprep` 0.1.5**。`sha2`・`hmac`・`pbkdf2` は同じ `digest` 0.11 系で揃っている（`cargo info` と `~/.cargo/registry` のソースで API を確認。§6.1）。`base64` は最新が 0.23.1 だが、`Cargo.lock` に既にある 0.22.1 と重複させない | 版が揃っていないと `digest` が 2 系統入る。`hmac` 0.13 / `sha2` 0.11 / `digest` 0.11.3 は `postgres`（dev-dependency）経由で既に `Cargo.lock` にある |
| AU-D3 | チャネルバインディング | M5 から対応／`none` だけ（調査 §4.1。TLS は M6） | **`none` だけ。機構名は `SCRAM-SHA-256` だけを提示する（`-PLUS` は出さない）。gs2 ヘッダは `n,,` と `y,,` を受理し、`p=...` は `08P01` で拒否する** | TLS が無い。`y` は「クライアントは対応しているが、サーバが提示しなかった」で、PostgreSQL の TLS 無しのサーバも受理する【確認】 |
| AU-D4 | 存在しないロールの偽のソルト | 調査: 「ユーザー名から決定的に作る」／PostgreSQL: ユーザー名にクラスタごとの秘密の nonce を足してハッシュ（`mock_auth_nonce`。`pg_control` に保存） | **PostgreSQL と同じ。`salt = SHA-256(ユーザー名のバイト列 ‖ mock_auth_nonce)` の先頭 16 バイト、反復回数 4096、StoredKey・ServerKey は 32 バイトの 0。`mock_auth_nonce`（32 バイトの乱数）は initdb が作り、制御ファイルのオフセット 128 に持つ（§3.3）。契約の `ServerExchange::new(username, secret)` はそのまま残し、nonce を渡す `new_with_mock_nonce` を足す** | ユーザー名だけで決まる偽のソルトは、誰でも計算できるので「存在しないロールの印」になってしまう（実在するロールのソルトは乱数）。クラスタごとの秘密が要る。制御ファイルの変更は F0 が `next_multi` と一緒に行う（§11 C2）。変更が入らない場合の代替はプロセスごとの乱数（再起動をまたぐと偽のソルトが変わる漏れが残る） |
| AU-D5 | `AuthenticationOk` を送る時機 | M1・M2: `Session::new` が成功してから（認証の失敗と接続の失敗が区別されない）／PostgreSQL: 認証の直後（ロール・データベースの検査の前） | **PostgreSQL と同じ。認証が成功したら（SCRAM は `AuthenticationSASLFinal` の直後に）すぐ `AuthenticationOk`。その後の失敗（3D000、28000、53300 など）は ErrorResponse（FATAL）になる** | libpq・pgJDBC・tokio-postgres は ErrorResponse が `AuthenticationOk` の前後どちらに来ても扱えるが、SCRAM の最後のメッセージ（`SASLFinal`）の後の ErrorResponse は PostgreSQL と同じ並びにしておくのが安全。ドライバの実装は PostgreSQL に対して試されている |
| AU-D6 | `max_connections` の検査の位置 | PostgreSQL: 認証より前（`sorry, too many clients already`）／M1 のまま（確立したセッションだけを数える。認証の後） | **M1 のまま（認証の後。`AuthenticationOk` の後に FATAL 53300）。スレッド数の上限（`max_threads`）は認証前にも効く** | M1 の設計（`connection.rs` 冒頭）: 認証前に黙って居座るソケットが本物のクライアントを締め出さないようにする。認証前の締め出しはスレッド上限と `authentication_timeout` が受け持つ。既知の差（§10 M5-AU-Q2） |
| AU-D7 | 認証のタイムアウト | M1 の実装: 読み書きごとの `SO_RCVTIMEO` だけ（1 バイトずつ送ると延々と生き延びる）／PostgreSQL: 起動パケットの受信から認証完了までの壁時計 | **全体の期限（`Instant`）を持ち、読み書きの前に残り時間でソケットのタイムアウトを張り直す。期限切れは FATAL 57014 `canceling authentication due to timeout`（起動パケットの受信中は黙って切る）。既定 60 秒（`authentication_timeout`）** | 接続ごとに 1 スレッドなので、認証前の放置はスレッドを占有する。1 バイトずつの送信（slowloris）でも期限で切る |
| AU-D8 | pg_hba の範囲 | 調査 §6.2: `host`・`all`/`sameuser`/名前・CIDR・`trust`/`reject`/`scram-sha-256`/`password`／PostgreSQL の全機能 | **調査の部分集合に、PostgreSQL と同じ意味で書ける周辺を足す: `local`・`hostssl`・`hostgssenc` の行は「この接続には決して一致しない」行として読み捨てる（TLS・GSS・Unix ソケットが無い PostgreSQL と同じ意味。WARNING をログに出す）。`hostnossl`・`hostnogssenc` は `host`。アドレスは CIDR、`IP マスク`の 2 語、`all`。ホスト名・`samehost`・`samenet`・`+group`・`/regex`・`@file`・方式のオプション・`md5` と `scram-sha-256` と `password` と `trust` と `reject` 以外の方式は解析エラーで起動しない** | PostgreSQL の標準の `pg_hba.conf` をそのまま貼っても（`local` 行があっても）起動する。黙って意味を変える機能（`+group` は M6 のメンバーシップが無いと意味がずれる）は受け付けない |
| AU-D9 | 認証方式 `md5` | D28: hba の `md5` は `scram-sha-256` として扱う／拒否 | **D28 のとおり（`md5` の行は `scram-sha-256` として動く）。ただし PostgreSQL は「保存されたパスワードが MD5 形式なら MD5 認証をする」ので、MD5 形式のパスワードを持つロールは yuzhu では認証できない（偽のソルトで進めて失敗させる）。MD5 形式は保存も拒否する（0A000。AU-D16）ので、M5 の initdb で作ったクラスタでは起きない** | 既知の差。PostgreSQL 18 で MD5 は非推奨 |
| AU-D10 | 接続の検査の順序 | M2 §6.9.1: データベース（3D000）→ `datallowconn`（55000）→ ロール（28000）／PostgreSQL（`InitPostgres`）: 認証 → ロールの存在と LOGIN（28000）→ ロールの接続数 → データベースの存在・`datallowconn`・データベースの接続数 | **PostgreSQL の順序にそろえる（§5.1 手順 9）。M2 は順序が逆なので、`Cluster::connect` の中の検査の順序を入れ替える**（`engine.rs` の持ち主 DB と合わせる。§6.9） | 両方が違う接続（存在しないロールで存在しないデータベース）のエラーが PostgreSQL と同じになる。**ロールの接続数の検査（53300）は rolcanlogin（28000）の直後で、データベースを引く（3D000）より前**（PG17 の `InitializeSessionUserId` が `CountUserBackends` を呼ぶ位置。【実機】PG17.11: `CONNECTION LIMIT 0` のロールが存在しない DB に接続しても `too many connections for role`、LOGIN できないロールは 28000 が先。09 DB-D1 と一致。レビュー対応 R-21）。BackendRegistry への登録（数え始め）はデータベースの存在の検査より前に行う |
| AU-D11 | パスワードの保存 | 調査 §4.4・C-6: `password_encryption` に従い SCRAM に変換。SCRAM 形式の値はそのまま保存。MD5 形式は 0A000 | **調査のとおり。加えて PostgreSQL 17 の `encrypt_password` と同じく、(1) 既に SCRAM 形式として解析できる値はそのまま保存（psql の `\password` と libpq の `PQchangePassword` が使う）、(2) 空文字列と「空のパスワードの SCRAM 秘密情報」は NOTICE `empty string is not a valid password, clearing password` を出してパスワードを NULL にする、(3) `password_encryption = 'md5'` と MD5 形式の値は 0A000 `MD5 password encryption is not supported`** | libpq の `PQencryptPasswordConn` は先に `SHOW password_encryption` を実行し、`scram_iterations` を ParameterStatus から読む（【記憶】。§9 の確認項目）。この 2 つが PostgreSQL と同じ値を返す必要がある |
| AU-D12 | `scram_iterations` | 調査 §4.2: PG16 から GUC で ParameterStatus でも報告（未検証）／報告しない | **GUC として持ち（既定 4096、1〜`i32::MAX`、SET 可）、ParameterStatus で報告する（起動時の報告が 13 個から 14 個になる）。【実機】PG17.11 の起動時の ParameterStatus に `scram_iterations=4096` が含まれることを確認した（14 個: `in_hot_standby`・`integer_datetimes`・`TimeZone`・`IntervalStyle`・`is_superuser`・`application_name`・`default_transaction_read_only`・`scram_iterations`・`DateStyle`・`standard_conforming_strings`・`session_authorization`・`client_encoding`・`server_version`・`server_encoding`。04 §5.10 と同じ）。レビュー対応 R-14** | libpq の `PQencryptPasswordConn`（`\password`）が値を使う。使われなければ libpq は 4096 を既定にするので、外れていても害はなかったが、実機で報告対象だった
| AU-D13 | ロールの DDL のロック | 一意インデックスで重複を防ぐ（PostgreSQL）／M5 のカタログにはインデックスが無い／`LockTag::Object` の名前ロック | **`LockTag::Object { db: 0, class: 1260, obj }`。名前を FNV-1a 64 でハッシュし最上位ビットを立てた値（名前ロック）を、CREATE / ALTER / DROP / RENAME のすべてが Exclusive・トランザクションスコープで取り、取ってから最新のカタログスナップショットで重複・存在を調べる。DROP は加えてロールの OID（最上位ビットなし）を AccessExclusive で取る。他のオブジェクトの所有者にする側（CREATE TABLE・CREATE DATABASE）は同じ OID のロックを AccessShare で取る約束にする（§5.9、§11 C9）** | カタログのインデックスが無いので、一意性は「名前のロック → 最新のスナップショットで検査」で守る（00 §3.1 の名前予約ロックと同じ考え）。コミットまでロックを持つ（D38: 解放は最後）ので、待った側は必ず相手の結果を見る |
| AU-D14 | DROP ROLE の依存の検査 | 調査 §5.2・C-7: `pg_shdepend` を作らず、`pg_database` と現在のデータベースだけを見る（他のデータベースの所有物を見逃す）／`pg_shdepend` 相当を作る | **`pg_shdepend` は作らない。代わりに、全データベース（`pg_database` の全行）の `pg_class.relowner`・`pg_namespace.nspowner`・`pg_proc.proowner`・`pg_type.typowner` と、`pg_database.datdba` を走査する（`Cluster::database_handle(oid)` で他のデータベースのカタログを開く）。DROP ROLE は稀なので走査の費用は問題にならない。調査の C-7 の「他のデータベースの所有物を見逃す」を解消する** | 所有物があるロールの DROP は PostgreSQL と同じく 2BP01 で拒否できる。`pg_shdepend` は他の機能（`DROP OWNED`、`REASSIGN OWNED`）が M6 で必要とするときに足す |
| AU-D15 | ロールの権限の検査 | PostgreSQL 16 以降: CREATEROLE は「自分が ADMIN OPTION を持つロール」にしか効かない（メンバーシップが前提）／PostgreSQL 15 以前: CREATEROLE は非スーパーユーザーのすべてのロールに効く | **PostgreSQL 15 以前の意味にする（メンバーシップが無いため）。ただしエラーの文言（`permission denied to ...` と DETAIL）と、「自分が持たない CREATEDB / REPLICATION / BYPASSRLS を他人に付けられない」規則は PostgreSQL 16 以降と同じにする（§5.7）。スーパーユーザーは常に全部できる。検査はセッション開始時の値ではなく、文の実行時に `pg_authid` から最新の値を読む（PostgreSQL の `superuser()` と同じ）** | 00 §1.1: GRANT・メンバーシップは M6。PostgreSQL 16 の ADMIN OPTION を真似ると、ロールの所属の仕組みが先に要る |
| AU-D16 | MD5 形式と `password_encryption = md5` | 受け付ける（PostgreSQL 17）／0A000（D28） | **0A000 `MD5 password encryption is not supported`（yuzhu 独自の文言。HINT は付けない）。`SET password_encryption = 'md5'` 自体は受け付け（`SHOW` できる）、パスワードを保存するときに 0A000** | D28 |
| AU-D17 | `pg_authid` の見え方 | M2 §6.8.7・M2-Q13(6): 全ユーザーが SELECT できる（TODO）／PostgreSQL: `PUBLIC` に何も付与しない（`pg_roles` を使わせる） | **スーパーユーザー以外が `pg_authid` を SELECT したら 42501 `permission denied for table pg_authid`。検査は `catalog::roles::check_catalog_select`（LK-4 の「参照するリレーションの解決」が 1 行で呼ぶ。§11 C5）。`pg_roles`（仮想リレーション）は誰でも読め、`rolpassword` は `'********'`** | M2-Q13 の TODO の解消。パスワードのハッシュ（オフラインで総当たりできる）を一般ユーザーに見せない |
| AU-D18 | `\du`（psql）が使う `pg_auth_members` | 作らない（00 §1.1 でロールの所属は M6）／空の仮想リレーションを置く | **`pg_auth_members`（PostgreSQL の OID 1261）を行が 0 件の仮想リレーションとして置く。`pg_roles` は OID 9810 を使う（§11 C6）** | PostgreSQL 17 の psql の `\du` は `pg_auth_members` と `pg_roles` を JOIN する副問い合わせ（`ARRAY(SELECT ...)`）を使う【記憶。§9】。表が無いと `\du` が 42P01 になる |
| AU-D19 | BackendKeyData の乱数 | M1: `RandomState` と時刻から作る弱い乱数／D29: `yuzhu_auth::random` | **`yuzhu_auth::random::fill` の 4 バイト（失敗したら FATAL XX000）。M1 の `random_secret` は削除する** | CancelRequest の秘密鍵は推測できてはいけない（D29 の「BackendKeyData の乱数も `yuzhu-auth` の乱数を使う」） |
| AU-D20 | `CREATE GROUP` ほか | 受け付けない／別名として受け付ける | **`CREATE GROUP` と `DROP GROUP` は `CREATE ROLE` / `DROP ROLE` と同じに動く（PostgreSQL と同じ）。`ALTER GROUP ... RENAME TO` も同じ。`ALTER GROUP ... ADD / DROP USER`、`CREATE ROLE ... IN ROLE / ROLE / ADMIN`、`ALTER ROLE ... SET / RESET`、`DROP OWNED`、`REASSIGN OWNED`、`GRANT ROLE` は 0A000** | 実装がほぼ要らない。メンバーシップは M6 |
| AU-D21 | `initdb` の既定 | 調査 §6.2・C-13: `trust`（PostgreSQL と同じ。警告つき）／`scram-sha-256` | **`trust` のまま（`yuzhu-initdb` の `--auth-host` の既定。警告を出す）。`--auth-host=scram-sha-256` / `password` と `--pwfile` を足す。パスワードが無いのに `scram-sha-256` を指定したら PostgreSQL と同じく失敗する。既定の待ち受けは 127.0.0.1 のみ（M1）なので、`trust` でも外からは届かない** | 既存のテスト（`tests/run.sh`、`tests/yuzhu.sh`、各クレートの結合テスト）が認証なしで動き続ける。CI に SCRAM 構成のジョブを足す（TS-4） |

---

## 1. 範囲

### 1.1 対応するもの

- **認証**: `trust`、`reject`、`password`（平文を送らせて SCRAM の秘密情報に対して検証する）、`scram-sha-256`（チャネルバインディングなし）。`yuzhu_hba.conf` による方式の選択。`authentication_timeout`。
- **ロールの DDL**: `CREATE ROLE` / `CREATE USER` / `CREATE GROUP`、`ALTER ROLE` / `ALTER USER`（属性、パスワード、`RENAME TO`）、`DROP ROLE` / `DROP USER` / `DROP GROUP`（`IF EXISTS`、複数）。属性は `SUPERUSER`・`CREATEDB`・`CREATEROLE`・`INHERIT`・`LOGIN`・`REPLICATION`・`BYPASSRLS`（反対の `NO...` を含む）、`CONNECTION LIMIT n`、`[ENCRYPTED] PASSWORD 'p'`・`PASSWORD NULL`、`VALID UNTIL 'ts'`、`SYSID n`（無視）。
- **ロールの属性の検査**: 接続時の LOGIN・VALID UNTIL（パスワード認証のとき）・CONNECTION LIMIT、DDL の権限（§5.7〜§5.9）、`pg_authid` の SELECT の制限。
- **観測**: `pg_roles`（仮想リレーション）、`pg_auth_members`（行なしの仮想リレーション）、`SHOW is_superuser`、`SHOW password_encryption`、ParameterStatus の `scram_iterations`。
- **initdb**: `--auth-host`、`--pwfile`、`yuzhu_hba.conf` の生成、超ユーザーのパスワード、`mock_auth_nonce`。

### 1.2 しないもの（実行すると `0A000`、または起動に失敗する）

| 項目 | 挙動 |
|---|---|
| MD5 認証、MD5 形式のパスワード、`password_encryption = md5` でのパスワード保存 | 0A000（AU-D16）。hba の `md5` は `scram-sha-256` として動く（AU-D9） |
| `cert`・`peer`・`ident`・`gss`・`sspi`・`ldap`・`pam`・`radius`・`bsd` | `yuzhu_hba.conf` の解析エラー（起動しない） |
| TLS、チャネルバインディング（`SCRAM-SHA-256-PLUS`）、`hostssl` に一致する接続 | M6。`SSLRequest` には `N`（M1 のまま） |
| Unix ソケット、`local` 行 | 読み捨て（AU-D8）。サーバは TCP だけ |
| ロールの所属（`IN ROLE`・`ROLE`・`ADMIN`・`GRANT role`・`SET ROLE`・`+group`）、`ALTER ROLE ... SET`、`DROP OWNED`、`REASSIGN OWNED`、`GRANT` / `REVOKE` | 0A000（hba の `+group` は解析エラー） |
| `UNENCRYPTED PASSWORD` | 0A000 `UNENCRYPTED PASSWORD is no longer supported`（HINT `Remove UNENCRYPTED to store the password in encrypted form instead.`。PostgreSQL 10 以降と同じ） |
| hba の再読み込み（SIGHUP、`pg_reload_conf()`） | M6。変更はサーバの再起動で反映する |
| 認証失敗後の遅延（`auth_delay`）、`log_connections` | なし。失敗は INFO でサーバのログに残す |
| ロールごとの設定（`pg_db_role_setting`）、`pg_shdepend`、`pg_auth_members` の中身 | M6 |

### 1.3 保証すること

1. **パスワードがネットワークに流れない**（`scram-sha-256` のとき）。サーバが保存するのは StoredKey と ServerKey だけで、パスワードもそこから逆算できる値（SaltedPassword、ClientKey）も保存しない。
2. **ロールが存在するかどうかを認証の応答から推測できない**: 存在しない・パスワードなし・期限切れ・SCRAM でない形式のロールは、偽のソルト（ユーザーごとに安定）で最後まで進め、同じ `28P01` で失敗する（AU-D4）。`password`（平文）方式は PostgreSQL と同じく、パスワードを受け取ってから判定する。
3. **認証前の接続が資源を占有し続けない**: 全体の期限（AU-D7）と、メッセージの長さの上限（SASL 1024 バイト、パスワード 65535 バイト）。
4. **パスワードの平文がログ・エラーメッセージ・デバッグ出力に出ない**（§6.5 の `Secret` 型。`CREATE / ALTER ROLE ... PASSWORD` の SQL テキストを記録しない）。
5. **定数時間の比較**: 証明（ClientProof から作った鍵のハッシュ）と StoredKey の比較は `subtle` で行う。
6. ロールの DDL はトランザクショナル（ROLLBACK で戻る）で、WAL に載り、クラッシュしても整合する。

### 1.4 M5 のロールの意味（利用者に伝えておくこと）

GRANT / REVOKE・所有者の検査・スキーマの権限は M6 です。M5 では、ログインできるロールは**すべてのテーブルを読み書きでき、他のロールのテーブルを DROP することもできます**。ロールが効くのは、(1) 認証（誰が接続できるか）、(2) ロールの DDL の権限（CREATEROLE）、(3) CREATE DATABASE の権限（CREATEDB。09 章）、(4) `pg_authid` の SELECT（スーパーユーザーだけ）、(5) 接続数の制限、(6) `is_superuser` の値、の 6 つだけです。この範囲は §10 M5-AU-Q7 で確認します。

---

## 2. 構成

`★` 新規、`△` 変更。

```
impl/rust/
├── Cargo.toml                          △ members に crates/yuzhu-auth、[workspace.dependencies] に §6.1 の 7 つと yuzhu-auth（AU-1 が最初のコミットで 1 回だけ。§11 C8）
└── crates/
    ├── yuzhu-auth/                     ★ AU-1。#![forbid(unsafe_code)]。外部依存は §6.1 の 7 つだけ（yuzhu-core には依存しない）
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs                  AuthError、再公開
    │       ├── scram.rs                秘密情報（作成・解析・検証）、ServerExchange（サーバ側の状態機械）、偽のソルト
    │       ├── scram_client.rs         ClientExchange（テストと将来の CLI 用。サーバの試験の相手役）
    │       ├── saslprep.rs             SASLprep（失敗したら生のバイト列）
    │       └── random.rs               OS の乱数
    ├── yuzhu-core/src/
    │   ├── catalog/
    │   │   ├── roles.rs                ★ AU-3。AuthidRow、pg_authid の読み書き（impl SharedCatalogStore）、権限の述語、所有物の走査
    │   │   ├── virtual_rel.rs          △ （LK が仕組み。AU が pg_roles / pg_auth_members の実体を足す。ファイルは roles.rs の中に置いて registry() に 1 行ずつ登録）
    │   │   └── rows.rs                 △ authid_rows(superuser, secret)、InitParams.superuser_secret（AU-2）
    │   ├── sql/
    │   │   ├── ast.rs                  △ CreateRoleStmt / AlterRoleStmt / DropRoleStmt の中身、Secret（F0 が空の構造体を作る。AU-3 が埋める）
    │   │   └── parser/role.rs          ★ AU-3
    │   ├── analyzer/ddl_ext/role.rs    ★ AU-3。BoundCreateRole / BoundAlterRole / BoundDropRole を作る
    │   ├── ddl/role.rs                 ★ AU-3。create_role / alter_role / drop_role
    │   ├── settings.rs                 △ password_encryption、scram_iterations（報告対象）、authentication_timeout（読み取り専用）
    │   ├── engine.rs                   △ lookup_role、mock_auth_nonce、connect のロール側の検査（DB と分担。§6.9）
    │   ├── bootstrap.rs                △ InitdbOptions に auth・superuser_password、yuzhu_hba.conf の生成、mock_auth_nonce
    │   ├── control.rs                  △ mock_auth_nonce（F0。§11 C2）
    │   └── session/mod.rs              △ StartupParams.client_addr、is_superuser の設定（F0 が口、AU が行を足す）
    └── yuzhu-server/
        ├── Cargo.toml                  △ yuzhu-auth に path 依存
        ├── src/
        │   ├── hba.rs                  ★ AU-2。HbaRules
        │   ├── auth.rs                 ★ AU-2。認証メッセージの読み書き、方式ごとの手順、RoleDirectory
        │   ├── connection.rs           △ AU-2。起動の順序（§5.1）、期限、BackendKeyData の乱数（認証の部分だけ。XQ のメッセージループには触れない）
        │   ├── config.rs               △ AU-2。hba_file、authentication_timeout
        │   └── bin/yuzhu-initdb.rs     △ AU-2。--auth-host、--pwfile
        └── tests/auth.rs               ★ AU-2（接続）、AU-3（ロールの DDL）
```

**依存の方向**（00 §2 に従う）: `yuzhu-auth` は何にも依存しない。`yuzhu-core` は `yuzhu-auth` に path 依存する（`ddl/role.rs` の `make_secret`、`bootstrap.rs`）。`yuzhu-server` は `yuzhu-auth` と `yuzhu-core` に依存し、`hba.rs`・`auth.rs` は `yuzhu-core` の `Cluster`・`RoleAuthInfo` を `RoleDirectory` トレイト越しにだけ使う。`catalog::roles` は `catalog::{store, schema, rows, virtual_rel}` と `storage`・`txn` を使い、`ddl::role` が `catalog::roles` を使う（00 §2 の依存の図の `catalog::{rows, store, cache, reader, roles}` → `ddl`）。

**この章の規約**:

1. パスワード・SaltedPassword・ClientKey は `String` / `Vec<u8>` のまま `Debug` で出さない。`Secret`（§6.5）か手書きの `Debug` で `<redacted>` にする。`tracing` のフィールドにも渡さない。
2. 認証の失敗の詳細（存在しない・期限切れ・不一致）はサーバのログ（INFO）にだけ書き、クライアントには `28P01` と固定の文言だけを返す。
3. 待つ関数は `WaitCtl` を受け取る（00 規約 2）。ロールの DDL は `DdlCtx.wait` を渡す。
4. 時刻は `Cluster::clock()`（TD。`util/clock.rs`）から取る（00 規約 6）。乱数は `yuzhu_auth::random` だけ。
5. PBKDF2（4096 回で数 ms）は、LockManager のロック・ページのラッチ・カタログのロックを持たずに計算する（`ddl/role.rs` は最初に計算する）。

---

## 3. ディスク上の形式と固定値

### 3.1 `pg_authid.rolpassword`（SCRAM の秘密情報）

PostgreSQL と同じ文字列形式【確認: PG:src/common/scram-common.c、src/backend/libpq/auth-scram.c の `parse_scram_secret` と `scram_build_secret`】。

```text
SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>

  <iterations>  C の `strtol(10 進)` と同じ読み方: **先頭の空白と `+` / `-` の符号を許し**、数字列の後ろがすぐ `:` であること（`4096x` は不可）。i32 に収まること。**0 や負の値も秘密情報の形式としては有効**（【実機】PG17.11: `SCRAM-SHA-256$+4096:…`、`$ 4096:…`、`$-4096:…`、`$0:…` はいずれもそのまま保存され、`$4096x:…` だけが平文として再ハッシュされた。レビュー対応 R-33）。ただし認証（`ServerExchange::new`）で iterations < 1 の秘密情報は検証に使えず 28P01 にする（PG は PBKDF2 をそのまま計算するので、差。既知の差として 10 章の台帳に載せる。M5-AU-Q18）
  <salt>        base64（標準アルファベット、= で終わりを埋める）。復号した長さは問わない（作るときは 16 バイト）
  <StoredKey>   base64。復号して**ちょうど 32 バイト**（44 文字、末尾 =）
  <ServerKey>   base64。復号して**ちょうど 32 バイト**（44 文字、末尾 =）
```

- 解析（`ScramSecret::parse`）は上の条件を**すべて**満たすときだけ成功する。1 つでも外れる文字列は「SCRAM の秘密情報ではない」（`is_scram_secret` が偽）。区切りは `$`・`:`・`$`・`:` の順に 1 回ずつ。余分な文字は許さない。
- 値の意味（RFC 5802 §3）: `SaltedPassword = Hi(SASLprep(パスワード), salt, iterations)`（PBKDF2-HMAC-SHA-256、出力 32 バイト）。`ClientKey = HMAC(SaltedPassword, "Client Key")`。`StoredKey = SHA-256(ClientKey)`。`ServerKey = HMAC(SaltedPassword, "Server Key")`。
- **例（固定値。RFC 7677 §3 のソルトで、パスワードは `pencil`、反復回数 4096）**:

```text
SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=

salt         = 5b 6d 99 68 9d 12 35 8e ec a0 4b 14 12 36 fa 81   （16 バイト）
SaltedPassword = c4a49510323ab4f952cac1fa99441939e78ea74d6be81ddf7096e87513dc615d
StoredKey    = 586e5df283e6dceb5c3e791d8b8528ec191e664045ce971792e2e6b5bb13e2a6
ServerKey    = c1f3cbc1c13a9d35a14c0990eed97629ea225863e566a4314ab99f3f00e5d9d5
```

- 判定の優先順位（`get_password_type` と同じ）: (1) `md5` + 32 桁の小文字 16 進 → MD5 形式（0A000）。(2) `ScramSecret::parse` が成功 → SCRAM。(3) それ以外 → 平文（ハッシュして保存する）。`SCRAM-SHA-256$` で始まるが解析できない文字列は (3) になる（PostgreSQL と同じ）。
- `pg_authid.rolvaliduntil`: `timestamptz`（`Datum::TimestampTz`。2000-01-01 からのマイクロ秒。`i64::MAX` は `infinity`、`i64::MIN` は `-infinity`）。`RoleAuthInfo.valid_until` は UNIX エポックからのマイクロ秒（00 §4.8）で、`unix = pg + PG_EPOCH_UNIX_MICROS`（946_684_800_000_000。TD の `util/clock.rs` の定数。飽和加算）。`infinity` は `None`（期限なし）、`-infinity` は `Some(i64::MIN)`（常に期限切れ）。

### 3.2 `pg_authid` の列（M2 §6.8.1 のまま。変更なし）

| 列 | 型 | 既定（CREATE ROLE） | CREATE USER |
|---|---|---|---|
| `oid` | oid | `OidAllocator` から（16384 以上） | 同じ |
| `rolname` | name | 指定 | 同じ |
| `rolsuper` | bool | false | false |
| `rolinherit` | bool | true | true |
| `rolcreaterole` | bool | false | false |
| `rolcreatedb` | bool | false | false |
| `rolcanlogin` | bool | **false** | **true** |
| `rolreplication` | bool | false | false |
| `rolbypassrls` | bool | false | false |
| `rolconnlimit` | int4 | -1 | -1 |
| `rolpassword` | text、NULL 可 | NULL | NULL |
| `rolvaliduntil` | timestamptz、NULL 可 | NULL | NULL |

initdb の初期行（M2 §6.8.2）: OID 10（超ユーザー。全属性 true、`rolconnlimit = -1`、`rolpassword` は `--pwfile` があれば SCRAM の秘密情報、なければ NULL）、OID 6171 `pg_database_owner`（全属性 false、`rolinherit = true`、`rolcanlogin = false`）。OID 16384 未満のロールは「システムのロール」で、DROP できない（§5.9）。

### 3.3 制御ファイルの `mock_auth_nonce`（F0 が実装。§11 C2）

| オフセット | サイズ | フィールド | 値 |
|---|---|---|---|
| 128 | 32 | `mock_auth_nonce` | initdb が `yuzhu_auth::random::fill` で作る 32 バイトの乱数。クラスタの寿命の間変えない。`control.rs` の `ControlData` に `mock_auth_nonce: [u8; 32]` を足し、`encode_slot` / `decode_slot` と CRC の対象（オフセット 0〜507）に含める |

オフセット 120〜127 は 00 §3.7 の `next_multi`。スロットは 512 バイトで CRC が 508 にあるので、128〜159 は空いている【確認: `control.rs` の `SLOT_SIZE` と `CRC_OFFSET`】。`Cluster::mock_auth_nonce(&self) -> [u8; 32]` で読む。

偽のソルトの例（固定値。ユーザー名 `alice`、nonce が 32 バイトの 0 のとき。手元で計算して確認した）: `SHA-256("alice" ‖ 00×32)` の先頭 16 バイトの base64 は `Uk7MU63Yg/hh/ptZ9hodwg==`。

### 3.4 `yuzhu_hba.conf`

データディレクトリ直下。initdb が生成する（§5.11）。書式は PostgreSQL の `pg_hba.conf` の部分集合。

```ini
# TYPE  DATABASE  USER      ADDRESS               METHOD
host    all       all       127.0.0.1/32          trust
host    all       all       ::1/128               trust
host    app       appuser   10.0.0.0/8            scram-sha-256
host    all       all       192.168.0.0 255.255.0.0   password
host    all       all       all                   reject
```

**字句の規則**（`hba::tokenize`）:

- 行単位。`#` から行末まではコメント（引用符の外のとき）。空行・コメントだけの行は読み飛ばす。行末の `\` は次の行へ続ける（続けた行を 1 行として扱い、エラーの行番号は先頭の行）。
- フィールドは空白（空白・タブ）で区切る。**コンマで区切った並びは 1 つのフィールド**（`a,b`、`a, b`、`a ,b` は同じ。コンマの前後の空白を無視する）。
- 二重引用符で囲むと空白・`#`・コンマを含められ、**キーワード（`all`、`sameuser`、`samerole`、`replication`）が普通の名前になる**。引用符の中の `"` は `""` で表す（PostgreSQL の `next_token` と同じかは未検証。§9）。
- 名前（データベース・ユーザー）は大文字小文字を区別する。キーワードと方式名と接続の種類は小文字だけ。

**行の文法**:

```text
line     := conn-type  databases  users  address  method
conn-type:= "host" | "hostnossl" | "hostnogssenc"            読み込んで使う
          | "local" | "hostssl" | "hostgssenc"               読み捨てる（決して一致しない行。WARNING をログに出す）
databases:= db ("," db)*          db := "all" | "sameuser" | "samerole" | "replication" | name
users    := user ("," user)*      user := "all" | name
address  := "all" | IP "/" prefix | IP mask                 IP は IPv4 か IPv6。mask は IP の形で連続した 1 の並び
method   := "trust" | "reject" | "password" | "scram-sha-256" | "md5"
```

- `sameuser`: 接続のデータベース名がユーザー名と同じ。`samerole`: M5 はメンバーシップが無いので `sameuser` と同じ。`replication`: レプリケーション接続だけに一致する（M5 には無いので決して一致しない）。
- `local` 行などの「決して一致しない行」は、`HbaRules` に入れない（行番号の検査と方式の検査は通常の行と同じに行い、エラーなら起動しない）。
- `all` の address は任意の IP アドレスに一致する。CIDR の `prefix` は IPv4 で 0〜32、IPv6 で 0〜128。ホストビット（マスクの外側のビット）が立っていても構わない（比較のときマスクして比べる。PostgreSQL と同じ【確認: `range_sockaddr_AF_INET`】）。`IP mask` の 2 語の形は mask が連続した 1 の後に 0 の並びでなければエラー。
- `md5` は `scram-sha-256` と同じ（AU-D9）。方式の後に何かが続いたら（オプション）エラー。

**解析エラーの文言**（`HbaError { line, message }`。`Display` は `yuzhu_hba.conf line {line}: {message}`）。起動時に 1 つでもあればサーバは起動しない（終了コード 1。クラスタを開く前に検査する）。

| 状況 | message |
|---|---|
| 接続の種類が不明 | `invalid connection type "{x}"` |
| 欄が足りない | `end-of-line before database specification` / `... role specification` / `... IP address specification` / `... authentication method` |
| 方式が不明 | `invalid authentication method "{x}"` |
| 方式が未対応 | `authentication method "{x}" is not supported by yuzhu` |
| address が不正 | `invalid IP address "{x}"`、`invalid CIDR mask in address "{x}"`、`invalid IP mask "{x}"`、`specifying both host name and CIDR mask is invalid: "{x}"` |
| ホスト名・`samehost`・`samenet` | `host names and samehost / samenet are not supported by yuzhu: "{x}"` |
| `+group`・`/regex`・`@file` | `group names (+...) are not supported by yuzhu yet`、`regular expressions are not supported by yuzhu yet`、`file inclusion (@...) is not supported by yuzhu` |
| 方式の後にオプション | `authentication options are not supported by yuzhu: "{x}"` |
| 引用符が閉じていない | `unterminated quoted string` |

ファイルが無い・読めない場合は起動しない（`could not open "{path}": {os error}`）。ファイルが空（有効な行が 0 件）のときは起動するが WARNING を出す（すべての接続が 28000 で拒否される。PostgreSQL と同じ）。`password` 方式の行があれば、TLS が無く平文が流れることを WARNING に出す。

### 3.5 ワイヤの形式（認証の部分）

整数はビッグエンディアン。「長さ」は自分自身の 4 バイトを含み、タイプバイトを含まない。

| 方向 | タイプ | 長さ | 本体 | 意味 |
|---|---|---|---|---|
| S→C | `R` | 8 | int32 `0` | AuthenticationOk |
| S→C | `R` | 8 | int32 `3` | AuthenticationCleartextPassword |
| S→C | `R` | 4+4+Σ(名前+1)+1 | int32 `10`、機構名（cstring）の並び、空の cstring 1 つ | AuthenticationSASL。M5 は `SCRAM-SHA-256` の 1 つだけ |
| C→S | `p` | 4+n+1 | パスワード（cstring） | PasswordMessage（`password` 方式。接続の状態で決まる） |
| C→S | `p` | 4+(機構名+1)+4+n | 機構名（cstring）、int32 n（初期応答の長さ。**-1 なら初期応答なし**）、client-first-message（n バイト） | SASLInitialResponse |
| S→C | `R` | 4+4+n | int32 `11`、server-first-message（n バイト） | AuthenticationSASLContinue |
| C→S | `p` | 4+n | client-final-message（n バイト。NUL で終わらない） | SASLResponse |
| S→C | `R` | 4+4+n | int32 `12`、server-final-message（n バイト） | AuthenticationSASLFinal |

- `p` は文脈で意味が変わる。サーバは自分が何を待っているかで解釈する（調査 §4.1）。`p` 以外のタイプバイトが来たら FATAL 08P01 `expected SASL response, got message type {n}`（SASL）/ `expected password response, got message type {n}`（平文）。`n` はタイプバイトの 10 進数。
- 長さの上限（本体）: SASLInitialResponse と SASLResponse は **1024 バイト**（`MAX_SASL_MESSAGE_LEN`。PG: `PG_MAX_SASL_MESSAGE_LENGTH`）、PasswordMessage は **65535 バイト**（`MAX_PASSWORD_MESSAGE_LEN`。PG: `PG_MAX_AUTH_TOKEN_LENGTH`）。超えたら**本体を読まずに** FATAL 08P01 `invalid message length`。長さが 4 未満も同じ。
- 認証が終わるまでは、`yuzhu-server/src/protocol/codec.rs` の `read_message`（上限 1 GiB）を使わない。`auth.rs` が `Read` から直接読む（タイプバイト 1 + 長さ 4 + 本体）。

**例（RFC 7677 §3。ユーザー `user`、パスワード `pencil`。サーバの nonce 部分は RFC の例のもの。実際のサーバは 18 バイトの乱数を base64 にした 24 文字を使う）**:

```text
C→S client-first-message  (32 バイト) : n,,n=user,r=rOprNGfwEbeRWgbNEkqO
S→C server-first-message  (86 バイト) : r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096
C→S client-final-message  (106 バイト): c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=
S→C server-final-message  (46 バイト) : v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=
```

```text
S→C  52 00 00 00 17  00 00 00 0A  53 43 52 41 4D 2D 53 48 41 2D 32 35 36 00  00
     R  長さ 23       10           "SCRAM-SHA-256" NUL                        終わりの NUL
C→S  70 00 00 00 36  53 43 52 41 4D 2D 53 48 41 2D 32 35 36 00  00 00 00 20  6E 2C 2C 6E 3D ...（32 バイト）
     p  長さ 54       "SCRAM-SHA-256" NUL                       n = 32        "n,,n=user,r=..."
S→C  52 00 00 00 5E  00 00 00 0B  72 3D 72 4F ...（86 バイト）          長さ 94 = 4 + 4 + 86
C→S  70 00 00 00 6E  63 3D 62 69 77 73 2C ...（106 バイト）               長さ 110 = 4 + 106
S→C  52 00 00 00 36  00 00 00 0C  76 3D 36 72 ...（46 バイト）           長さ 54 = 4 + 4 + 46
S→C  52 00 00 00 08  00 00 00 00                                          AuthenticationOk
```

失敗の応答（FATAL。M1 の `ErrorFields` の形。`S` と `V` に `FATAL`）:

```text
E  S "FATAL" NUL  V "FATAL" NUL  C "28P01" NUL  M "password authentication failed for user \"alice\"" NUL  NUL
```

### 3.6 テストベクタ（固定値。§7.1 が使う）

| 名前 | 入力 | 期待 | 出典 |
|---|---|---|---|
| PBKDF2-HMAC-SHA-256 | P = `passwd`、S = `salt`、c = 1、dkLen = 32 | `55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc` | RFC 7914 §11 の 64 バイトの出力の先頭 32 バイト。PBKDF2 の最初のブロックは dkLen に依らない【確認: `openssl kdf` で再計算】 |
| SCRAM の秘密情報 | パスワード `pencil`、salt = base64 `W22ZaJ0SNY7soEsUEjb6gQ==`、iterations = 4096 | §3.1 の例の文字列 | RFC 7677 §3 のソルトと反復回数【確認: `openssl` と `perl` で計算】 |
| ClientProof | §3.5 の 4 つのメッセージ（AuthMessage = `n=user,r=rOpr...,` + server-first + `,` + `c=biws,r=rOpr...$k0`） | `dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=` | RFC 7677 §3【確認: 再計算して一致】 |
| ServerSignature | 同上 | `6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=` | RFC 7677 §3【確認】 |
| SASLprep | `I\u{00AD}X` → `IX`、`user` → `user`、`USER` → `USER`、`\u{00AA}` → `a`、`\u{2168}` → `IX`、`\u{0007}`（禁止文字）→ エラー（呼び出し側は生の文字列を使う）、`\u{0627}\u{0031}`（双方向の検査）→ エラー | 左のとおり | RFC 4013 §3 の例【記憶】 |
| 偽のソルト | ユーザー名 `alice`、nonce = 32 バイトの 0 | base64 `Uk7MU63Yg/hh/ptZ9hodwg==` | §3.3（yuzhu の実装の回帰用。PostgreSQL との一致は §9） |

---

## 4. 共通の型（契約）

00 §4.8・§4.11・§4.12 のシグネチャは**そのまま**。ここは契約を実装の形にして、追加するものを示す（追加は 00 の「足りないものは追加してよい」の範囲。§11 に一覧）。

### 4.1 `yuzhu-auth`（AU-1）

```rust
// lib.rs
#![forbid(unsafe_code)]
pub mod random;
pub mod saslprep;
pub mod scram;
pub mod scram_client;

/// 認証ライブラリの失敗。ErrorResponse への対応は呼び出し側（yuzhu-server / yuzhu-core）が決める
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// 認証の失敗（証明の不一致、偽の秘密情報で進めた、など）。28P01。クライアントには理由を言わない
    Failed,
    /// メッセージの形式の誤り・順序の誤り。08P01。message は ErrorResponse の M、detail は D
    Protocol { message: &'static str, detail: Option<String> },
    /// 対応しない機能（authzid、拡張属性）。0A000
    Unsupported(&'static str),
    /// 引数の誤り（反復回数 0 など）。22023
    InvalidParameter(String),
    /// OS の乱数が取れない。XX000
    Random(String),
}
impl std::fmt::Display for AuthError { /* message と detail */ }
impl std::error::Error for AuthError {}
```

```rust
// random.rs
pub fn fill(buf: &mut [u8]) -> std::io::Result<()>;                 // 00 §4.11。getrandom::fill。失敗は io::Error
pub fn bytes<const N: usize>() -> std::io::Result<[u8; N]>;
```

```rust
// saslprep.rs
/// SASLprep（RFC 4013）。禁止文字・双方向の検査に失敗したときは入力をそのまま返す（PostgreSQL の `pg_saslprep` の呼び出し側と同じ）。
/// 結果が入力と同じなら Borrowed
pub fn prepare(password: &str) -> std::borrow::Cow<'_, str>;
/// 失敗を見たい試験用
pub fn try_prepare(password: &str) -> Result<std::borrow::Cow<'_, str>, ()>;
```

```rust
// scram.rs
pub const MECHANISM: &str = "SCRAM-SHA-256";
pub const DEFAULT_ITERATIONS: u32 = 4096;
pub const SALT_LEN: usize = 16;               // 新しい秘密情報のソルトの長さ
pub const SERVER_NONCE_RAW_LEN: usize = 18;   // サーバの nonce の乱数のバイト数（base64 で 24 文字）
pub const KEY_LEN: usize = 32;
pub const MOCK_NONCE_LEN: usize = 32;
pub const MAX_ITERATIONS: u32 = i32::MAX as u32;

/// "SCRAM-SHA-256$<iter>:<salt>$<StoredKey>:<ServerKey>"（§3.1）
#[derive(Clone, PartialEq, Eq)]
pub struct ScramSecret {
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub stored_key: [u8; KEY_LEN],
    pub server_key: [u8; KEY_LEN],
}
impl ScramSecret {
    pub fn parse(s: &str) -> Option<ScramSecret>;                                   // §3.1 の厳密な解析
    pub fn encode(&self) -> String;                                                 // parse の逆。標準の base64（= あり）
    pub fn derive(password: &str, salt: &[u8], iterations: u32) -> ScramSecret;     // §5.2 の式。password は SASLprep する
}
impl std::fmt::Debug for ScramSecret { /* iterations と salt の長さだけ。鍵は出さない */ }

// 00 §4.11 の契約（シグネチャは変えない）
pub fn make_secret(password: &str, iterations: u32) -> Result<String, AuthError>;           // 乱数のソルト 16 バイト。iterations == 0 は InvalidParameter
pub fn is_scram_secret(s: &str) -> bool;                                                    // ScramSecret::parse が Some
pub fn verify_plain_password(password: &str, secret: &str) -> bool;                         // secret が SCRAM でなければ false
// 追加
pub fn make_secret_with_salt(password: &str, salt: &[u8], iterations: u32) -> Result<String, AuthError>;   // 試験と PostgreSQL の再現用
pub fn mock_secret(username: &str, mock_nonce: &[u8; MOCK_NONCE_LEN]) -> ScramSecret;       // §5.4

pub struct ServerExchange { /* §6.1 */ }
impl ServerExchange {
    /// 00 §4.11 の契約。secret が None、または SCRAM として解析できなければ偽の秘密情報で進めて、最後に Failed にする。
    /// 偽のソルトの nonce は「プロセスごとの乱数」（最初の呼び出しで作って OnceLock に持つ）。サーバは使わない（試験用）
    pub fn new(username: &str, secret: Option<&str>) -> Result<Self, AuthError>;
    /// サーバが使う。mock_nonce は Cluster::mock_auth_nonce()（制御ファイル）
    pub fn new_with_mock_nonce(username: &str, secret: Option<&str>, mock_nonce: &[u8; MOCK_NONCE_LEN]) -> Result<Self, AuthError>;
    /// 偽の秘密情報で進めている（ログに理由を書くため。クライアントには見せない）
    pub fn is_doomed(&self) -> bool;
    /// SASLInitialResponse の本体（client-first-message）。server-first-message を返す
    pub fn client_first(&mut self, msg: &[u8]) -> Result<Vec<u8>, AuthError>;
    /// SASLResponse の本体（client-final-message）。成功なら server-final-message、失敗は Err(Failed)（偽の秘密情報でも同じ時間をかけて計算してから）
    pub fn client_final(&mut self, msg: &[u8]) -> Result<Vec<u8>, AuthError>;
}
```

```rust
// scram_client.rs。サーバの試験の相手役（RFC 5802 のクライアント側）。tokio-postgres を使えない生メッセージの試験（nonce の改ざんなど）に使う
pub struct ClientExchange { /* ... */ }
impl ClientExchange {
    pub fn new(username: &str, password: &str) -> Result<Self, AuthError>;                          // nonce は乱数
    pub fn with_nonce(username: &str, password: &str, client_nonce: &str) -> Self;                   // RFC のベクタ用
    pub fn client_first(&self) -> Vec<u8>;                                                           // "n,,n=<user>,r=<nonce>"
    pub fn client_final(&mut self, server_first: &[u8]) -> Result<Vec<u8>, AuthError>;
    pub fn verify_server_final(&self, server_final: &[u8]) -> Result<(), AuthError>;                 // ServerSignature を検査
}
```

### 4.2 `yuzhu-server/src/hba.rs`（AU-2）

```rust
// 00 §4.12 の契約（シグネチャは変えない）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HbaMethod { Trust, Reject, Password, ScramSha256 }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HbaError { pub line: usize, pub message: String }
impl std::fmt::Display for HbaError { /* "yuzhu_hba.conf line {line}: {message}" */ }
impl std::error::Error for HbaError {}

#[derive(Debug, Clone)]
pub struct HbaRules { /* Vec<HbaRule>、警告の一覧 */ }
impl HbaRules {
    pub fn parse(text: &str) -> Result<HbaRules, HbaError>;
    pub fn find(&self, addr: std::net::IpAddr, database: &str, user: &str) -> Option<HbaMethod>;
    // 追加
    pub fn find_rule(&self, addr: std::net::IpAddr, database: &str, user: &str) -> Option<&HbaRule>;   // ログに行番号を出す
    pub fn warnings(&self) -> &[String];                // 読み捨てた行、password 方式、有効な行が 0 件、など
    pub fn rules(&self) -> &[HbaRule];
    /// すべての接続を trust で許す（Server::with_cluster。試験用。ファイルを読まない）
    pub fn allow_all_trust() -> HbaRules;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HbaRule {
    pub line: usize,                       // 1 始まり（続き行は先頭の行）
    pub databases: Vec<HbaDatabase>,
    pub users: Vec<HbaUser>,
    pub address: HbaAddress,
    pub method: HbaMethod,
}
#[derive(Debug, Clone, PartialEq, Eq)] pub enum HbaDatabase { All, SameUser, SameRole, Replication, Name(String) }
#[derive(Debug, Clone, PartialEq, Eq)] pub enum HbaUser { All, Name(String) }
#[derive(Debug, Clone, PartialEq, Eq)] pub enum HbaAddress { All, Cidr { ip: std::net::IpAddr, prefix: u8 } }
```

### 4.3 `yuzhu-server/src/auth.rs`（AU-2）

```rust
/// 認証が必要とするクラスタの機能。Cluster が実装する。試験は差し替える
pub trait RoleDirectory {
    fn lookup_role(&self, user: &str) -> yuzhu_core::Result<Option<yuzhu_core::RoleAuthInfo>>;
    fn mock_auth_nonce(&self) -> [u8; yuzhu_auth::scram::MOCK_NONCE_LEN];
    fn now_unix_micros(&self) -> i64;                    // Cluster::clock().unix_micros()
}
impl RoleDirectory for yuzhu_core::Cluster { /* ... */ }

/// 認証の入力
pub struct AuthContext<'a> {
    pub directory: &'a dyn RoleDirectory,
    pub user: &'a str,
    pub database: &'a str,
    pub client_addr: std::net::IpAddr,
}

/// クライアントに返す FATAL。log_detail はサーバのログにだけ書く
#[derive(Debug)]
pub struct FatalError {
    pub sqlstate: &'static str,
    pub message: String,
    pub detail: Option<String>,
    pub log_detail: Option<String>,
}
#[derive(Debug)]
pub enum AuthFailure {
    /// ErrorResponse（FATAL）を送って切る
    Fatal(FatalError),
    /// 読み書きの失敗（切断を含む）。何も送らず切る
    Io(std::io::Error),
    /// 全体の期限切れ（§5.5）
    Timeout,
}

pub const MAX_SASL_MESSAGE_LEN: usize = 1024;
pub const MAX_PASSWORD_MESSAGE_LEN: usize = 65535;

/// 認証メッセージ（サーバ → クライアント）。AuthenticationOk は M1 の BackendMessage::AuthenticationOk を使う
#[derive(Debug, Clone, Copy)]
pub enum AuthRequest<'a> { CleartextPassword, Sasl(&'a [&'a str]), SaslContinue(&'a [u8]), SaslFinal(&'a [u8]) }
pub fn encode_auth_request(req: &AuthRequest<'_>) -> Vec<u8>;           // §3.5 の表

/// 1 つの `p` メッセージを読む。タイプバイトが `p` でなければ 08P01、本体の長さが limit を超えるか 4 未満なら
/// 本体を読まずに 08P01 `invalid message length`
pub fn read_p_message<R: std::io::Read>(r: &mut R, limit: usize, expect: &'static str) -> Result<Vec<u8>, AuthFailure>;

/// 方式ごとの認証（§5.2〜§5.4）。Trust / Reject もここに通す（Reject は 28000 の FATAL を返す）。
/// `tick` は読み書きの前に呼ばれ、期限が切れていれば Err(Timeout) を返し、残り時間でソケットのタイムアウトを張り直す（§5.5）。
/// 成功したら AuthenticationOk は**まだ書かない**（SASLFinal まで書いて flush 済み。呼び出し側が AuthenticationOk を書く）
pub fn authenticate<R: std::io::Read, W: std::io::Write>(
    r: &mut R, w: &mut W, method: HbaMethod, ctx: &AuthContext<'_>, tick: &mut dyn FnMut() -> Result<(), AuthFailure>,
) -> Result<(), AuthFailure>;
```

### 4.4 `yuzhu-core/src/catalog/roles.rs`（AU-3）

```rust
/// pg_authid の 1 行（12 列すべて）
#[derive(Clone, PartialEq, Eq)]
pub struct AuthidRow {
    pub oid: Oid, pub name: String,
    pub superuser: bool, pub inherit: bool, pub create_role: bool, pub create_db: bool, pub can_login: bool,
    pub replication: bool, pub bypass_rls: bool, pub conn_limit: i32,
    pub password: Option<String>,                 // rolpassword（SCRAM の秘密情報）
    pub valid_until: Option<i64>,                 // rolvaliduntil（timestamptz。2000-01-01 からのマイクロ秒。i64::MAX = infinity）
}
impl std::fmt::Debug for AuthidRow { /* password は <redacted> */ }
impl AuthidRow {
    pub fn from_row(r: &Row) -> Result<AuthidRow>;
    pub fn to_row(&self) -> Row;                              // pg_authid の列の順
    pub fn to_role_row(&self) -> RoleRow;                     // 00 §4.8 の RoleRow（create_db・create_role・conn_limit を足したもの）
    pub fn to_auth_info(&self) -> RoleAuthInfo;               // valid_until を UNIX エポックに換算（§3.1）
}

/// 00 §4.8: `RoleRow { oid, name, can_login, superuser, create_db, create_role, conn_limit }`（M2 の 4 項目に 3 項目を足す）

/// pg_authid の読み書き。SharedCatalogStore に別ファイルで impl する（00 §8。store.rs は直接編集しない。§11 C1）
impl SharedCatalogStore {
    pub fn authid_all(&self, snap: &Snapshot) -> Result<Vec<HeapTuple>>;
    pub fn authid_by_name(&self, snap: &Snapshot, name: &str) -> Result<Option<(Tid, AuthidRow)>>;
    pub fn authid_by_oid(&self, snap: &Snapshot, oid: Oid) -> Result<Option<(Tid, AuthidRow)>>;
    /// OidAllocator から採り、pg_authid を「全版が見える」スナップショット（snapshot_any）で走査して重複を避ける（M2 §6.9.2。
    /// CatalogStore::get_new_oid は共有カタログを受け付けないので、この関数が持つ）
    pub fn new_role_oid(&self, alloc: &OidAllocator) -> Result<Oid>;
    pub fn insert_authid(&self, w: &WriteCtx, row: &AuthidRow) -> Result<()>;
    /// TmResult が Ok 以外なら XX000 `tuple concurrently updated`（00 §3.8）
    pub fn update_authid(&self, w: &WriteCtx, snap: &Snapshot, tid: Tid, new: &AuthidRow) -> Result<()>;
    pub fn delete_authid(&self, w: &WriteCtx, snap: &Snapshot, tid: Tid) -> Result<()>;   // 非 Ok は XX000 `tuple concurrently deleted`
}

// ロックの鍵（§5.7）
pub const AUTHID_CLASS: Oid = 1260;
pub const ROLE_NAME_LOCK_FLAG: u64 = 1 << 63;
pub fn role_name_lock(name: &str) -> LockTag;         // Object { db: 0, class: 1260, obj: fnv1a64(name) | FLAG }
pub fn role_oid_lock(oid: Oid) -> LockTag;            // Object { db: 0, class: 1260, obj: oid as u64 }
/// 所有者にするロール（CREATE TABLE・CREATE DATABASE の所有者）を DROP ROLE から守る。AccessShare・トランザクションスコープ。
/// 取った後で呼び出し側が authid_by_oid で存在を確かめ直す（無ければ 42704）
pub fn lock_role_shared(locks: &LockManager, backend: BackendId, role: Oid, wait: &WaitCtl<'_>) -> Result<()>;

// 権限の述語（スーパーユーザーは常に真。PostgreSQL の has_createrole_privilege などと同じ）
pub fn has_createrole(r: &AuthidRow) -> bool;
pub fn has_createdb(r: &AuthidRow) -> bool;           // 09 章（CREATE DATABASE）が呼ぶ
pub fn has_replication(r: &AuthidRow) -> bool;
pub fn has_bypassrls(r: &AuthidRow) -> bool;
/// 現在のロール（DdlCtx.role.oid）の最新の行。スナップショットはカタログ用（D12）。行が無ければ（他のセッションが DROP した）属性なしのロールとして扱う
pub fn current_role(ctx: &DdlCtx<'_>) -> Result<AuthidRow>;
/// pg_authid の SELECT の検査（AU-D17）。42501 `permission denied for table pg_authid`
pub fn check_catalog_select(rel_oid: Oid, role: &RoleRow) -> Result<()>;

/// DROP ROLE の依存の走査の結果（§5.9）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedObject { pub db_oid: Oid, pub db_name: String, pub kind: OwnedKind, pub name: String }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedKind { Database, Table, Index, Sequence, View, Schema, Function, Type }
pub fn owned_objects(cluster: &Cluster, snap: &Snapshot, role: Oid) -> Result<Vec<OwnedObject>>;
```

### 4.5 AST と `BoundDdl`（AU-3）

```rust
// sql/ast.rs（F0 が空の構造体を作る。中身はこの章）
/// 文字列リテラルのパスワード。Debug・Display で値を出さない
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);
impl std::fmt::Debug for Secret { /* "<redacted>" */ }

#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum RoleKeyword { Role, User, Group }     // コマンドタグには影響しない（どれも CREATE ROLE）

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleOption {
    Superuser(bool), CreateDb(bool), CreateRole(bool), Inherit(bool), Login(bool), Replication(bool), BypassRls(bool),
    ConnectionLimit(i32),
    /// Some(p) = PASSWORD 'p'（ENCRYPTED はあってもなくても同じ）、None = PASSWORD NULL
    Password(Option<Secret>),
    ValidUntil(String),                     // 'timestamp' の文字列。実行時に timestamptz として解釈する
    Sysid(i64),                             // 無視
    /// IN ROLE / IN GROUP / ROLE / USER / ADMIN（M6。解析は通し、analyzer が 0A000）
    InRole(Vec<String>), Role(Vec<String>), Admin(Vec<String>),
    /// UNENCRYPTED PASSWORD（analyzer が 0A000）
    UnencryptedPassword,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleOptionItem { pub option: RoleOption, pub pos: u32 }                // pos は ErrorResponse の位置（1 始まりの文字位置）

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRoleStmt { pub keyword: RoleKeyword, pub name: String, pub name_pos: u32, pub options: Vec<RoleOptionItem> }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleSpec { Name(String), CurrentUser, CurrentRole, SessionUser }          // ALTER ROLE の対象。DROP ROLE は Name だけ
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterRoleAction {
    Options(Vec<RoleOptionItem>),
    Rename(String),
    /// SET / RESET / IN DATABASE / ALL / ADD USER / DROP USER（analyzer が 0A000。文言の元を持つ）
    Unsupported(&'static str),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterRoleStmt { pub keyword: RoleKeyword, pub target: RoleSpec, pub target_pos: u32, pub action: AlterRoleAction }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropRoleStmt { pub keyword: RoleKeyword, pub names: Vec<(RoleSpec, u32)>, pub if_exists: bool }

// analyzer/bound.rs（BoundDdl の変種。00 §4.7）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundCreateRole {
    pub name: String,
    pub superuser: bool, pub inherit: bool, pub create_role: bool, pub create_db: bool, pub can_login: bool,
    pub replication: bool, pub bypass_rls: bool, pub conn_limit: i32,
    pub password: Option<Secret>,                 // PASSWORD NULL と省略は None
    pub valid_until: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundAlterRole { pub target: RoleSpec, pub action: BoundAlterRoleAction }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundAlterRoleAction {
    /// 指定された項目だけ Some
    Attrs(RoleChanges),
    Rename(String),
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleChanges {
    pub superuser: Option<bool>, pub inherit: Option<bool>, pub create_role: Option<bool>, pub create_db: Option<bool>,
    pub can_login: Option<bool>, pub replication: Option<bool>, pub bypass_rls: Option<bool>,
    pub conn_limit: Option<i32>,
    pub password: Option<Option<Secret>>,         // Some(None) = PASSWORD NULL
    pub valid_until: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundDropRole { pub names: Vec<String>, pub if_exists: bool }
```

### 4.6 `Cluster` と接続（00 §4.8。AU が実装する部分）

```rust
impl Cluster {
    /// 00 §4.8。カタログ用スナップショット（D12。`take_snapshot` で登録する・短命。LK-D9、R-02）で pg_authid を引く。ロールが無ければ None
    pub fn lookup_role(&self, user: &str) -> Result<Option<RoleAuthInfo>>;
    /// 制御ファイルの mock_auth_nonce（§3.3）
    pub fn mock_auth_nonce(&self) -> [u8; 32];
    pub fn clock(&self) -> &Arc<dyn Clock>;          // TD（06 章 §4.2）が足す
}
// RoleAuthInfo、ConnectRequest、ConnectGrant、Cluster::connect は 00 §4.8 のとおり。
// StartupParams に client_addr: Option<IpAddr> を足す（§11 C3）。Session::new がそれを ConnectRequest.client_addr に渡す
```

### 4.7 設定（`settings.rs`。AU が自分の分を足す）

| 名前 | 型・既定 | 報告 | SET | 備考 |
|---|---|---|---|---|
| `password_encryption` | 列挙 `scram-sha-256` / `md5`・既定 `scram-sha-256` | しない | セッション | `md5` は SET できるが、パスワードを保存するとき 0A000（AU-D16）。`SHOW` の値は PostgreSQL と同じ書式 |
| `scram_iterations` | 整数・既定 4096・1〜2147483647 | **する**（AU-D12） | セッション | `make_secret` に渡す反復回数 |
| `authentication_timeout` | 時間・既定 `1min` | しない | 不可（読み取り専用） | サーバの設定値を `Cluster::server_settings` 経由で見せる（§11 C10） |

PostgreSQL 17 の `is_superuser` は既に報告対象で、既定値が `on` 固定になっている（M1）。`Session::new` が `ConnectGrant.role.superuser` から `on` / `off` を設定する（§6.7）。

---

## 5. 処理の流れ

### 5.1 接続の起動の順序（`connection.rs`。AU-2）

M1・M2 の `handle_connection` の流れ（起動パケット → `max_connections` → `Session::new` → `AuthenticationOk` → ParameterStatus → BackendKeyData → ReadyForQuery）に、認証と期限と hba を足す。**番号は固定する**（テストが順序を確かめる）。

```
run_connection(stream, shared, pid, registration)
  1. client_addr = stream.peer_addr().ip().to_canonical()       取れなければ FATAL 28000 "could not determine the client address"
     deadline = Deadline::new(shared.authentication_timeout)      起動パケットの受信の前に作る（§5.5）
  2. startup()  [M1 の関数。tick を呼びながら読む]
       SSLRequest / GSSENCRequest → 'N'、CancelRequest → 切る、StartupMessage → params
       user と database は 63 バイト（文字の境界）に切り詰める（PostgreSQL は NAMEDATALEN - 1 で切る）。database 省略は user
       user が無い → FATAL 28000 "no PostgreSQL user name specified in startup packet"（M1 のまま）
  3. rule = shared.hba.find_rule(client_addr, database, user)
       None → FATAL 28000: no pg_hba.conf entry for host "{addr}", user "{user}", database "{database}", no encryption
       Some(rule) → method = rule.method
       （メッセージの "pg_hba.conf" は PostgreSQL の文言のまま。M5-AU-Q4）
  4. auth::authenticate(method, ctx, tick)                        §5.2〜§5.4。失敗は FATAL を送って切る（AuthFailure）
       Reject      → FATAL 28000: pg_hba.conf rejects connection for host "{addr}", user "{user}", database "{database}", no encryption
       Trust       → 何もしない（ロールの存在は手順 9 が確かめる）
       Password    → §5.3
       ScramSha256 → §5.2
  5. AuthenticationOk を書く（SCRAM では SASLFinal の直後。AU-D5）
  6. ソケットの読み書きのタイムアウトを外す（M1 のまま）
  7. max_connections の検査（M1 のまま。CountGuard）。超えたら FATAL 53300 "sorry, too many clients already"
  8. Session::new(cluster, params)  → Cluster::connect(&ConnectRequest)
  9. connect() の検査（AU-D10。PostgreSQL の InitPostgres の順序）:
       a. ロール: lookup_role → None なら FATAL 28000 `role "{user}" does not exist`
       b. LOGIN: can_login が偽なら FATAL 28000 `role "{user}" is not permitted to log in`
       c. データベースを引く（`database_by_name`。**無くてもここでは失敗しない**。ロールの接続数の検査が先。09 §5.1 手順 2）                       （09 章）
       d. BackendRegistry に登録（自分を数える。db_oid は c の行の OID、無ければ 0）                                                          （09 章）
       e. **ロールの接続数**: スーパーユーザーでなく conn_limit >= 0 で count_of_role(role) > conn_limit なら FATAL 53300 `too many connections for role "{user}"`   （この章。**データベースの存在の検査より前**。【実機】PG17.11: 存在しないデータベースへの接続でも `too many connections for role`）
       f. データベース（09 章 §5.1 手順 4〜7）: 無ければ FATAL 3D000 `database "{db}" does not exist`、Database ロックの取得（CREATE / DROP DATABASE の完了を待つ）、行の取り直し（3D000）、datallowconn が偽なら FATAL 55000 `database "{db}" is not currently accepting connections`、データベースの接続数 `too many connections for database "{db}"`
     失敗したら、登録は BackendGuard の Drop で外れる。FATAL を送って切る
 10. ParameterStatus（`is_superuser` は role.superuser、`session_authorization` はユーザー名）、BackendKeyData { pid, secret }、ReadyForQuery
       secret = yuzhu_auth::random::fill の 4 バイト（i32）。失敗したら FATAL XX000 "could not generate random numbers"
```

- 手順 9 の e は「自分を数えて超えたら拒否」（PostgreSQL の `CountUserBackends(roleid) > rolconnlimit` と同じ）。2 つの接続が同時に登録してから数えると、両方が拒否されることはあっても、制限を超えて両方が通ることはない。`conn_limit = 0` は（スーパーユーザー以外の）すべての接続を拒否する。
- 手順 9 の a で `lookup_role` を引くので、`trust` の接続は存在しないロールでも `28000 role "x" does not exist` で拒否される（M2 §6.9.1 のとおり）。パスワード系の方式では、存在しないロールは手順 4 で `28P01` になり、ここまで来ない。
- 手順 4 と 9 の間でロールが変更されても構わない（手順 9 の最新の行が正）。

### 5.2 SCRAM-SHA-256 のサーバ側（`scram.rs` と `auth.rs`）

#### 5.2.1 `auth.rs` の手順（`ScramSha256`）

```
authenticate_scram(r, w, ctx, tick):
  role   = ctx.directory.lookup_role(ctx.user)?                           // Option<RoleAuthInfo>。PBKDF2 は走らせない
  now    = ctx.directory.now_unix_micros()
  secret = role.and_then(|r| r.password)                                  // Option<String>
           ただし role が無い・password が None・valid_until < now（期限切れ。valid_until は Some のときだけ。無期限は None）なら None
  log_reason（サーバのログ用）:
           role なし → "Role \"{user}\" does not exist."、password なし → "User \"{user}\" has no password assigned."、
           期限切れ → "User \"{user}\" has an expired password."、SCRAM でない形式 → "User \"{user}\" does not have a valid SCRAM secret."
  ex = ServerExchange::new_with_mock_nonce(ctx.user, secret.as_deref(), &ctx.directory.mock_auth_nonce())?    // 乱数が取れなければ FATAL XX000
  tick()?; write AuthenticationSASL(["SCRAM-SHA-256"]); flush
  tick()?; msg = read_p_message(r, MAX_SASL_MESSAGE_LEN, "SASL response")?
  解析: mechanism = 最初の NUL までの文字列。"SCRAM-SHA-256" でなければ FATAL 08P01 "client selected an invalid SASL authentication mechanism"
        続く int32 n: -1 なら初期応答なし → ex.client_first に空の入力を渡す（Protocol "The message is empty." になる）
        n >= 0 で、残りの長さが n でなければ FATAL 08P01 "malformed SASLInitialResponse"
  server_first = ex.client_first(client_first)                             // Err(Protocol) → 08P01、Err(Unsupported) → 0A000
  tick()?; write AuthenticationSASLContinue(server_first); flush
  tick()?; msg2 = read_p_message(r, MAX_SASL_MESSAGE_LEN, "SASL response")?     // 本体の全部が client-final-message
  match ex.client_final(msg2):
     Ok(server_final) → write AuthenticationSASLFinal(server_final); flush; 成功
     Err(Failed)      → log INFO（log_reason / "Password does not match for user \"{user}\"."）; FATAL 28P01 `password authentication failed for user "{user}"`
     Err(Protocol{..})→ FATAL 08P01（message と detail。例: invalid SCRAM response / Nonce does not match.）
     Err(Unsupported) → FATAL 0A000
```

- 失敗の応答は、**存在しないロール・パスワードなし・期限切れ・不一致のすべてで同じ**（`28P01`、同じ文言）。偽の秘密情報でも `client_first` と `client_final` が同じ回数のハッシュを計算してから失敗する（`ServerExchange` が `is_doomed` でも検証の計算を飛ばさない）。
- 成功後、`AuthenticationOk` を書くのは呼び出し側（§5.1 手順 5）。SASLFinal を書いたあとは flush 済み。

#### 5.2.2 `ServerExchange` の状態機械

```
状態: Init → SaltSent → Done。Done や順序違いの呼び出しは Protocol { "unexpected SCRAM message", None }
保持: salt、iterations、stored_key、server_key、doomed、gs2_header（"n,," か "y,,"）、
      client_first_bare、client_nonce、server_nonce、server_first

new_with_mock_nonce(username, secret, mock_nonce):
  secret が Some(s) で ScramSecret::parse(s) = Some(sec) → (salt, iterations, stored_key, server_key) = sec、doomed = false
  それ以外（None / 解析できない）→ mock_secret(username, mock_nonce) の値、doomed = true
  server_nonce = base64(random 18 バイト)（24 文字。= が付かない）

client_first(msg):   // msg = SASLInitialResponse の初期応答。UTF-8 でなければ、または NUL を含めば Protocol "malformed SCRAM message"
  1. 空 → Protocol { "malformed SCRAM message", "The message is empty." }
  2. gs2 チャネルバインディングのフラグ（1 文字）:
       'n' → 続行。'y' → 続行（クライアントは対応しているが、サーバは提示していない。ダウングレード検知は TLS が無いので不要）
       'p' → Protocol { "malformed SCRAM message", "The client selected SCRAM-SHA-256 without channel binding, but the SCRAM message includes channel binding data." }
       他 → Protocol { "malformed SCRAM message", "Unexpected channel-binding flag \"{c}\"." }
  3. 次の 1 文字が ',' でなければ Protocol "Comma expected, but found character \"{c}\"."
  4. authzid: 次が 'a' → Unsupported("client uses authorization identity, but it is not supported")。',' でなければ Protocol "Unexpected attribute \"{c}\" in client-first-message."
     gs2_header = ここまで（"n,," または "y,,"）。client_first_bare = 残り
  5. client_first_bare の先頭が 'm' → Unsupported("client requires an unsupported SCRAM extension")
  6. 属性 "n=<username>,": 必ず存在すること（値は読み捨てる。ユーザー名は起動パケットのものを使う）。違えば Protocol "Expected attribute \"n\" but found \"{c}\"."
  7. 属性 "r=<nonce>": nonce は ASCII の印字可能文字（0x21〜0x7E）でコンマを含まない、1 文字以上。違えば Protocol "non-printable characters in SCRAM nonce"
     （空は Protocol "malformed SCRAM message" "Expected attribute \"r\" but found end of message."）。後ろの拡張（",..."）は読み捨てる
  8. server_first = "r=" + client_nonce + server_nonce + ",s=" + base64(salt) + ",i=" + iterations
  9. 状態 = SaltSent。server_first のバイト列を返す

client_final(msg):
  1. 状態が SaltSent でなければ Protocol "unexpected SCRAM message"
  2. msg を UTF-8 として、コンマで分けた属性の並びとして見る。
       先頭の属性は "c=<base64>"、次は "r=<nonce>"、最後は "p=<base64>"、その間（拡張）は読み捨てる。形が違えば Protocol "malformed SCRAM message"
       （"Expected attribute \"c\" but found ...。Garbage found at the end of client-final-message."）
       client_final_without_proof = msg の、最後の ",p=" の手前まで（",p=" を含まない）
       proof = base64 復号。**ちょうど 32 バイト**でなければ Protocol { "malformed SCRAM message", "Malformed proof in client-final-message." }
  3. c= の値が base64(gs2_header)（"biws" か "eSws"）と違えば Protocol "unexpected SCRAM channel-binding attribute in client-final-message"
  4. r= が client_nonce + server_nonce と違えば Protocol { "invalid SCRAM response", "Nonce does not match." }
  5. AuthMessage = client_first_bare + "," + server_first + "," + client_final_without_proof
     ClientSignature = HMAC-SHA-256(key = StoredKey, AuthMessage)
     ClientKey'      = proof XOR ClientSignature                       （32 バイトの XOR）
     ok = (SHA-256(ClientKey') を subtle::ConstantTimeEq で StoredKey と比べる) AND NOT doomed
     ok でなければ状態 = Done、Err(Failed)
  6. ServerSignature = HMAC-SHA-256(key = ServerKey, AuthMessage)
     状態 = Done。"v=" + base64(ServerSignature) を返す
```

- 計算式は RFC 5802 §3 と RFC 7677（SHA-256 版）。`Hi` は PBKDF2-HMAC-SHA-256（`U1 = HMAC(str, salt ‖ INT(1))`、`Ui = HMAC(str, Ui-1)`、結果は U1 から Ui の XOR。dkLen 32）。`pbkdf2_hmac::<Sha256>` が計算する。
- すべての比較に使う文字列・バイト列で、秘密に依存するのは「`SHA-256(ClientKey')` と StoredKey の比較」だけ（`ConstantTimeEq`）。ほかは公開値。
- `ScramSecret::derive(password, salt, iterations)`: `pw = saslprep::prepare(password)`、`SaltedPassword = pbkdf2_hmac_array::<Sha256, 32>(pw.as_bytes(), salt, iterations)`、`ClientKey = HMAC(SaltedPassword, "Client Key")`、`StoredKey = SHA-256(ClientKey)`、`ServerKey = HMAC(SaltedPassword, "Server Key")`。
- `verify_plain_password(password, secret)`: `ScramSecret::parse(secret)` が `None` なら偽。成功なら `derive(password, sec.salt, sec.iterations).stored_key` を `ConstantTimeEq` で `sec.stored_key` と比べる（PostgreSQL の `scram_verify_plain_password` と同じ）。
- `make_secret(password, iterations)`: `iterations` が 0 なら `InvalidParameter`。ソルト 16 バイトを乱数で作り `derive(..).encode()`。

### 5.3 平文パスワード方式（`password`。`authenticate_password`）

```
role   = lookup_role(user)?;  now = now_unix_micros()
tick()?; write AuthenticationCleartextPassword; flush
tick()?; msg = read_p_message(r, MAX_PASSWORD_MESSAGE_LEN, "password response")?
本体は NUL で終わる 1 つの文字列でなければならない（長さ - 1 の位置の NUL 以外に NUL が無い）。違えば FATAL 08P01 "invalid password packet size"
長さ 1（空のパスワード）→ FATAL 28P01 "empty password returned by client"
UTF-8 でなければ偽（SCRAM の秘密情報の元は UTF-8 なので一致しえない）
ok = role.password が Some で期限切れでなく、verify_plain_password(password, secret)
成功 → 戻る。失敗 → INFO ログ（理由）+ FATAL 28P01 `password authentication failed for user "{user}"`
```

- ロールが存在しない場合も、パスワードを受け取ってから失敗する（PostgreSQL と同じ。PBKDF2 を走らせないぶん応答は速く、存在が時間差から推測されうる。PostgreSQL も同じ。§10 M5-AU-Q10）。
- 保存されたパスワードが SCRAM でなければ（NULL を含む）常に失敗する。

### 5.4 存在しないロール・失敗時の応答

- 偽の秘密情報: `mock_secret(username, mock_nonce)` = `ScramSecret { iterations: 4096、salt: SHA-256(username のバイト列 ‖ mock_nonce)[0..16]、stored_key: [0; 32]、server_key: [0; 32] }`。PostgreSQL の `mock_scram_secret` と同じ作り【記憶。§9】。
- 同じユーザー名には、いつでも同じソルトを返す（実在するロールのソルトも固定）。したがって応答のソルトを繰り返し取っても、存在するかどうかは分からない。ただし、偽のソルトの反復回数は 4096 固定で、実在するロールが別の反復回数を持つなら見分けがつく（PostgreSQL も同じ。既知の差ではない）。
- クライアントへの文言: `FATAL 28P01 password authentication failed for user "{user}"`。DETAIL は付けない。サーバのログ（INFO、`tracing`）に `pid`、`user`、理由（上の `log_reason`）、方式、hba の行番号を書く。
- 失敗した後、サーバは待たずに切る（遅延なし。1.2）。

### 5.5 `authentication_timeout`（全体の期限）

```
struct Deadline { until: Instant }
impl Deadline {
    fn new(total: Duration) -> Self;                                  // until = now + total
    fn remaining(&self) -> Option<Duration>;                          // 0 以下なら None
}
tick(): remaining() が None なら Err(Timeout)。Some(d) なら stream.set_read_timeout(Some(d)) と set_write_timeout(Some(d))（最小 1ms）
```

- `tick` は、起動パケットの各読み取りの前、認証メッセージの各読み書きの前に呼ぶ。1 バイトずつ送って延々と待たせるクライアントも、合計 `authentication_timeout`（既定 60 秒）で切られる。
- 期限切れ（`io::ErrorKind::WouldBlock` / `TimedOut` または `tick` の `Timeout`）のときの扱い: **起動パケットを読み終えた後**（認証中）は FATAL 57014 `canceling authentication due to timeout` を書こうとして（書き込みのタイムアウト 500ms）切る。起動パケットの受信中は何も送らずに切る（M1 の `startup timed out` のまま）。
- 手順 6（タイムアウトを外す）は認証の成功の後。`max_connections` の検査と `Session::new` はタイムアウトの外。
- 設定: `authentication_timeout`（秒、既定 60。設定ファイル `authentication_timeout`、コマンドライン `--authentication-timeout`）。`Server::with_authentication_timeout`（M1 にある）はそのまま使える。

### 5.6 hba の照合（`HbaRules::find_rule`）

```
find_rule(addr, database, user):
  addr = addr.to_canonical()                              // ::ffff:a.b.c.d は IPv4 として扱う（yuzhu の判断。PostgreSQL の挙動は未検証）
  for rule in rules（ファイルの上から順）:
     address:   All → 一致。Cidr { ip, prefix } → addr と ip の種類（IPv4 / IPv6）が同じで、上位 prefix ビットが一致
     database:  databases のいずれかが一致（All、Name(n) は文字列の完全一致、SameUser は database == user、SameRole は同じ、Replication は偽）
     user:      users のいずれかが一致（All、Name(n) は完全一致）
     3 つとも一致したらその行を返す（**最初の一致だけ**。その行の認証が失敗しても次の行は見ない）
  一致なし → None
```

### 5.7 CREATE ROLE（`ddl/role.rs::create_role`）

PostgreSQL 17 の `CreateRole`（PG:src/backend/commands/user.c）の順序に従う。`ddl::execute` が `BoundDdl::CreateRole` を受ける。コマンドタグは `CREATE ROLE`（`CREATE USER` と `CREATE GROUP` も）。通常のトランザクションの中で動く（ブロックの中でもよい。ROLLBACK で戻る）。

```
create_role(ctx: &mut DdlCtx, b: BoundCreateRole):
  1. 権限（最新の行。AU-D15）: me = current_role(ctx)
       !has_createrole(me)                 → 42501 "permission denied to create role"   DETAIL "Only roles with the CREATEROLE attribute may create roles."
       b.superuser && !me.superuser         → 42501 "permission denied to create role"   DETAIL "Only roles with the SUPERUSER attribute may create roles with the SUPERUSER attribute."
       b.create_db && !has_createdb(me)     → 同じ書式で CREATEDB
       b.replication && !has_replication(me)→ 同じ書式で REPLICATION
       b.bypass_rls && !has_bypassrls(me)   → 同じ書式で BYPASSRLS
  2. 予約名: name が "pg_" で始まる → 42939 "role name \"{name}\" is reserved"   DETAIL "Role names starting with \"pg_\" are reserved."
       （"public" と "none" はパーサが 42939 `role name "public" is reserved` で拒否済み）
  3. パスワードの変換（ロックの外で。PBKDF2 が走る）: secret = encrypt_password(ctx.password_policy, name, b.password)   §5.7.1
  4. VALID UNTIL: Some(s) なら input_text(s, timestamptz, ctx.type_env)（失敗は 22007 `invalid input syntax for type timestamp with time zone: "..."`）
  5. 名前ロック: ctx.locks.acquire(backend, role_name_lock(name), Exclusive, Transaction, ctx.wait)?     // 待つ。デッドロック検出の対象
  6. snap = 最新のカタログ用スナップショット（txn_manager.take_snapshot(txn.xid, txn.cid)（登録する・短命。R-02）。待った後なので取り直す。D12）
     重複: authid_by_name(snap, name) が Some → 42710 "role \"{name}\" already exists"
  7. oid = shared.new_role_oid(oid_allocator)
  8. insert_authid(w, AuthidRow { oid, name, 属性…, password: secret, valid_until })
  9. 戻り値 "CREATE ROLE"
```

- `CONNECTION LIMIT` の範囲は analyzer が検査する（`n < -1` → 22023 `invalid connection limit: {n}`）。
- オプションの重複・矛盾（`SUPERUSER NOSUPERUSER`、`PASSWORD` が 2 回など）は analyzer が `42601 conflicting or redundant options`（位置はあとから現れたオプション）にする（PostgreSQL の `errorConflictingDefElem`）。
- 長さが 63 バイトを超えるロール名はレキサーが切り詰めて NOTICE を出す（M1 のとおり。`identifier "x" will be truncated to "y"`）。
- 読み取り専用のトランザクション（M3 の `READ ONLY`）では、ほかの DDL と同じく session の共通の検査（25006）で拒否される。

#### 5.7.1 `encrypt_password`（`ddl/role.rs`）

```
encrypt_password(policy: &PasswordPolicy, role: &str, password: Option<&Secret>) -> Result<Option<String>>
  None（PASSWORD NULL や省略）→ Ok(None)
  Some(p):
    p が空文字列、または is_scram_secret(p) かつ verify_plain_password("", p) → NOTICE "empty string is not a valid password, clearing password"、Ok(None)
    md5 形式（"md5" + 32 桁の小文字 16 進）→ Err(0A000 "MD5 password encryption is not supported")
    is_scram_secret(p) → Ok(Some(p))                      そのまま保存（クライアントが計算した値。psql の \password）
    policy.encryption == Md5 → Err(0A000 "MD5 password encryption is not supported")
    それ以外 → Ok(Some(make_secret(p, policy.scram_iterations)?))
pub struct PasswordPolicy { pub encryption: PasswordEncryption, pub scram_iterations: u32 }       // DdlCtx.password（§11 C4）
pub enum PasswordEncryption { ScramSha256, Md5 }
```

### 5.8 ALTER ROLE（`ddl/role.rs::alter_role`）

`BoundAlterRole`。コマンドタグ `ALTER ROLE`。`target` が `CURRENT_USER` / `CURRENT_ROLE` / `SESSION_USER` なら `ctx.role` の名前（M5 は同じ）。

```
alter_role(ctx, b):
  1. パスワードの変換（Attrs の password が Some(Some(p)) のとき）: encrypt_password。ロックの外
  2. VALID UNTIL の解析（Some のとき）
  3. 名前ロック（対象の名前。Rename は新しい名前も。名前のハッシュの昇順で取る＝デッドロックを減らす）
  4. snap = 最新のカタログ用スナップショット。target = authid_by_name(snap, name)
       None → 42704 "role \"{name}\" does not exist"
  5. 権限（PostgreSQL 17 の AlterRole の判定を、メンバーシップ抜きにしたもの。me = current_role）:
       a. target.superuser または変更後に superuser になる、または対象が SUPERUSER を変える → me.superuser でなければ
            42501 "permission denied to alter role"  DETAIL "Only roles with the SUPERUSER attribute may alter roles with the SUPERUSER attribute."
       b. target.replication または REPLICATION を変える → !has_replication(me) なら 同じ書式で REPLICATION
       c. BYPASSRLS を true にする → !has_bypassrls(me) なら 同じ書式で BYPASSRLS
       d. CREATEDB を true にする → !has_createdb(me) なら 同じ書式で CREATEDB
       e. 上のいずれでもなく、has_createrole(me) でもない場合: 「自分自身（target.oid == me.oid）で、変更が PASSWORD だけ」なら許す。
          それ以外は 42501 "permission denied to alter role"  DETAIL "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{name}\" may alter this role."
          （ADMIN OPTION は M5 にない。文言だけ PostgreSQL に合わせる）
       f. ブートストラップ超ユーザー（OID 10）の SUPERUSER を false にする → **0A000** `permission denied to alter role`、DETAIL `The bootstrap superuser must have the SUPERUSER attribute.`（【実機】PG17.11。`AlterRole`（`user.c`）が `ERRCODE_FEATURE_NOT_SUPPORTED` で、文言は本文が `permission denied to alter role`、DETAIL が `The bootstrap superuser ...`。22023 でも 42501 でもない。レビュー対応 R-24）
  6. 新しい行 = target の行に changes を適用（指定されない項目は変えない。password は Some(None) で NULL、Some(Some(s)) で secret）
       Rename: 新しい名前が "pg_" で始まる → 42939 `role name "{new}" is reserved`。同名が既にある → 42710 `role "{new}" already exists`。
               ブートストラップ超ユーザー・自分自身（session user）の改名 → 0A000 "session user cannot be renamed" / "current user cannot be renamed"
               （スーパーユーザーだけが superuser のロールを、CREATEROLE が非スーパーユーザーを改名できる。権限の規則は a〜e と同じ。
               改名で MD5 のパスワードが消える PostgreSQL の挙動は、MD5 を持たないので関係ない）
  7. update_authid(w, snap, tid, new)
  8. "ALTER ROLE"
```

- 他のセッションが同じロールを同時に ALTER しても、名前ロックで直列になる。ロックを取ってから最新のスナップショットで読み直すので、後から来たほうは先のコミットを見て更新する（`tuple concurrently updated` は、名前ロックを取らない書き手が現れない限り起きない）。
- ALTER ROLE は接続中のセッションには影響しない（NOLOGIN にしても切断されない。PostgreSQL と同じ）。

### 5.9 DROP ROLE（`ddl/role.rs::drop_role`）

`BoundDropRole`。コマンドタグ `DROP ROLE`。複数の名前は先頭から 1 つずつ処理し、途中の失敗は文全体の失敗（トランザクションが失敗状態になる）。

```
drop_role(ctx, b):
  for name in b.names（1 件ごとに command_counter_increment を呼ぶ。`DROP ROLE a, a` の 2 回目が「存在しない」になる）:
    1. 名前ロック（Exclusive）
    2. snap = 最新のカタログ用スナップショット。row = authid_by_name(snap, name)
         None → if_exists なら NOTICE "role \"{name}\" does not exist, skipping" で次へ。そうでなければ 42704 "role \"{name}\" does not exist"
    3. 自分自身: row.oid == ctx.role.oid → 55006 "current user cannot be dropped"
          （M5 は current user = session user。`session user cannot be dropped` は SET SESSION AUTHORIZATION が入ってから。M6）
    4. 権限: row.superuser && !me.superuser → 42501 "permission denied to drop role"
              DETAIL "Only roles with the SUPERUSER attribute may drop roles with the SUPERUSER attribute."
            !has_createrole(me) → 42501 "permission denied to drop role"
              DETAIL "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{name}\" may drop this role."
    5. システムのロール（OID < 16384。ブートストラップ超ユーザーと pg_database_owner）→ 2BP01 "cannot drop role {name} because it is required by the database system"
          （00 §3.5 の DEPENDENT_OBJECTS_STILL_EXIST。PostgreSQL は pg_shdepend の「pin」で同じエラー。【記憶】）
    6. OID ロック: acquire(role_oid_lock(row.oid), AccessExclusive, Transaction, wait)       // CREATE TABLE などの lock_role_shared と衝突する
       もう一度 snap を取り直して authid_by_oid(snap, oid) を確かめる（待っている間に消えていたら 2 に戻って「存在しない」）
    7. 所有物の検査: objs = owned_objects(cluster, snap, row.oid)
         objs が空でなければ 2BP01 "role \"{name}\" cannot be dropped because some objects depend on it"
            DETAIL: 現在のデータベースの所有物は 1 行ずつ "owner of table t" / "owner of index t_pkey" / "owner of sequence s" / "owner of schema s" など
                    （データベース = 現在でない所有物は、データベースごとに "{n} object in database {db}" / "{n} objects in database {db}"。
                     共有オブジェクト = データベースの所有者は "owner of database {db}"）
                    行は種類・名前の順に並べ、100 行を超えたら "and {n} other object" / "and {n} other objects"
    8. delete_authid(w, snap, tid)
  "DROP ROLE"
```

`owned_objects(cluster, snap, role)`:

```
dbs = pg_database の全行（snap）                                     // template0・template1 も含める
結果 = []
for db in dbs:
   db.datdba == role → OwnedObject { kind: Database, name: db.datname, db_oid: db.oid }
   handle = cluster.database_handle(db.oid)?                           // 接続しなくてもカタログを読める
   pg_class     で relowner == role の行 → kind は relkind（r → Table、i → Index、S → Sequence、v → View）
   pg_namespace で nspowner == role → Schema、pg_proc の proowner → Function、pg_type の typowner → Type
   （すべて snap で走査。OID 16384 未満のシステムのオブジェクトの所有者は OID 10 だけなので、除外は不要）
```

- 他のセッションが CREATE TABLE（所有者 = このロール）を同時に実行していても、その側が `lock_role_shared` を AccessShare で取るので、DROP ROLE の OID ロックの取得で待つ（または後から入ったほうが「ロールが無い」42704 になる）。取らない書き手があれば見逃す（§11 C9。M4 の `ddl/` の CREATE TABLE が 1 行足せば塞がる。足されるまでは既知の差）。
- `DROP OWNED`・`REASSIGN OWNED` は 0A000（パーサが認識して analyzer が返す）。

### 5.10 `pg_authid` と `pg_roles`

- **`pg_authid`**: `check_catalog_select(1260, role)` は `!role.superuser` なら `42501 permission denied for table pg_authid`。呼び出しは LK-4 の「参照するリレーションの解決」（`session/locking.rs`）が、AccessShare を取る前に 1 回（§11 C5）。`INSERT` / `UPDATE` / `DELETE`（M2 §6.8.7 の 42501 の検査）は変わらない。`pg_roles` の中身はこの検査の外（仮想リレーションの提供関数が直接読む）。
- **`pg_roles`**（OID 9810、relkind `v`）: PostgreSQL 17 のビューと同じ列と順序。提供関数は最新のカタログ用スナップショットで `pg_authid` を全件読む。

| 列 | 型（OID） | 値 |
|---|---|---|
| `rolname` | name (19) | `rolname` |
| `rolsuper` `rolinherit` `rolcreaterole` `rolcreatedb` `rolcanlogin` `rolreplication` | bool (16) | そのまま |
| `rolconnlimit` | int4 (23) | そのまま |
| `rolpassword` | text (25) | 常に `'********'`（パスワードの有無に関わらず。PostgreSQL と同じ） |
| `rolvaliduntil` | timestamptz (1184) | そのまま（NULL 可） |
| `rolbypassrls` | bool (16) | そのまま |
| `rolconfig` | text[] (1009) | 常に NULL（`pg_db_role_setting` は M6） |
| `oid` | oid (26) | `oid` |

  すべての列が NULL 可（ビューなので `attnotnull = false`）。行の順序は `pg_authid` の走査順で、保証しない（テストは `ORDER BY`）。

- **`pg_auth_members`**（OID 1261、relkind `v`）: 列は PostgreSQL 17 の `oid`・`roleid`・`member`・`grantor`（oid）、`admin_option`・`inherit_option`・`set_option`（bool）。行は常に 0 件。psql の `\du` の `memberof` 列の副問い合わせが動くようにするためだけに置く（AU-D18。`ARRAY(SELECT ...)` は TY-5 の範囲）。
- どちらも `VirtualRelation` を実装し（00 §4.10）、`catalog::virtual_rel::registry()` に登録する。initdb が `pg_class`（`relkind = 'v'`、`relowner = 10`、`relnamespace = 11`）と `pg_attribute` の行を書く。INSERT / UPDATE / DELETE は **55000**（`cannot insert into view "pg_roles"` など。【実機】PG17.11 のビューと同じ。TRUNCATE / DROP は 42809）。エラーを返す関数は LK の `virtual_rel`（01 §4.10・§6.5）。00 §4.10 は R-10 で 55000 に直した。

### 5.11 initdb（`bootstrap.rs` と `yuzhu-initdb`）

M2 の `InitdbOptions { superuser, no_sync, rel_seg_blocks }` に次を足す（§11 C7。呼び出し側の更新は F0）。

```rust
pub struct InitdbOptions {
    pub superuser: String,                       // M2 のまま。空はエラー（22023）。既定は呼び出し側が決める（yuzhu-initdb は "postgres"）
    pub no_sync: bool,
    pub rel_seg_blocks: u32,
    pub auth_host: InitAuth,                     // ★ yuzhu_hba.conf の host 行の方式。既定 Trust
    pub superuser_password: Option<Secret>,      // ★ --pwfile の 1 行目。Debug で出さない
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitAuth { Trust, ScramSha256, Password }
impl Default for InitdbOptions { /* superuser = "postgres"、no_sync = false、rel_seg_blocks = DEFAULT_RELSEG_SIZE、Trust、None */ }
```

```
initdb の追加の手順（M2 §5.8 の手順 3 と 8 の間）:
  - superuser_password が Some(p) なら secret = scram::make_secret(p, DEFAULT_ITERATIONS)（p が空なら NULL。NOTICE は出さない）を
    InitParams.superuser_secret に渡す。authid_rows が OID 10 の rolpassword に入れる
  - mock_auth_nonce = random::bytes::<32>()（失敗は XX000）を制御ファイルに書く（§3.3）
  - yuzhu_hba.conf をデータディレクトリ直下に書く（fsync。内容は下）
  - auth_host が ScramSha256 / Password で superuser_password が None → 22023 "must specify a password for the superuser to enable {method} authentication"
yuzhu_hba.conf の内容（bootstrap::default_hba(auth)）:
  # yuzhu_hba.conf: client authentication (see spec/design/m5/08-auth-roles.md)
  # TYPE  DATABASE  USER  ADDRESS       METHOD
  host    all       all   127.0.0.1/32  <method>
  host    all       all   ::1/128       <method>
  <method> は trust / scram-sha-256 / password
```

`yuzhu-initdb`（`bin/yuzhu-initdb.rs`）の追加オプション: `--auth-host <trust|scram-sha-256|password|md5>`（`md5` は `scram-sha-256` と同じ。警告なし）、`--pwfile <FILE>`（1 行目がパスワード。末尾の改行（`\n` / `\r\n`）を除く。ファイルが空なら `password file "{path}" is empty` で失敗）。`trust` のときは PostgreSQL と同じ趣旨の警告を標準エラーに出す:

```text
yuzhu-initdb: warning: enabling "trust" authentication for host connections
You can change this by editing yuzhu_hba.conf or using the option --auth-host the next time you run yuzhu-initdb.
```

---

## 6. モジュールごとの仕様

### 6.1 `yuzhu-auth`

**`Cargo.toml`**（`[workspace.dependencies]` に置いて各クレートは `workspace = true`）:

```toml
[dependencies]
sha2 = "0.11"
hmac = "0.13"
pbkdf2 = { version = "0.13", default-features = false, features = ["hmac"] }
base64 = "0.22"
getrandom = { version = "0.4", features = ["std"] }
subtle = "2.6"
stringprep = "0.1"
```

| クレート | 版 | 用途 | 使う API（確認済み） |
|---|---|---|---|
| `sha2` | 0.11.0 | SHA-256（StoredKey、偽のソルト） | `sha2::{Sha256, Digest}` |
| `hmac` | 0.13.0 | HMAC-SHA-256 | `hmac::{Hmac, KeyInit, Mac}`（`Hmac::<Sha256>::new_from_slice(key)`、`update`、`finalize().into_bytes()`）。`hmac` 0.13 は `KeyInit` / `Mac` を再公開している |
| `pbkdf2` | 0.13.0 | `Hi` | `pbkdf2::pbkdf2_hmac_array::<Sha256, 32>(password, salt, rounds) -> [u8; 32]`（`hmac` 機能） |
| `base64` | 0.22.1 | 標準アルファベット（= あり） | `base64::engine::general_purpose::STANDARD`、`Engine::{encode, decode}` |
| `getrandom` | 0.4.3 | OS の CSPRNG | `getrandom::fill(&mut [u8]) -> Result<(), getrandom::Error>` |
| `subtle` | 2.6.1 | 定数時間比較 | `subtle::ConstantTimeEq` |
| `stringprep` | 0.1.5 | SASLprep | `stringprep::saslprep(&str) -> Result<Cow<str>, Error>`（ASCII の印字可能文字だけなら素通し。ASCII の制御文字は `Err`）。推移的な依存に `unicode-normalization`・`unicode-bidi`・`unicode-properties` が入る |

- `yuzhu-auth` の `#![forbid(unsafe_code)]` は自分のコードにだけ効く（外部クレートの内部の unsafe は対象外）。
- `ServerExchange` の内部: 文字列の取り回しは `&str` / `String`（UTF-8 検査後）。`HMAC` は `Hmac<Sha256>` を 1 回の計算ごとに作る。`ServerExchange::new` の「プロセスごとの乱数 nonce」は `static MOCK: OnceLock<[u8; 32]>` に最初の呼び出しで `random::bytes` を入れる。
- PBKDF2 の反復回数が大きい `scram_iterations` の SET（例: 2^31-1）は、`make_secret` が数十分かかる。割り込み（キャンセル）を受けられない。上限は PostgreSQL と同じ（`i32::MAX`）で、制限しない。
- `ClientExchange`（`scram_client.rs`）は RFC 5802 のクライアント側。`client_first` は `n,,n=<user>,r=<nonce>`（`user` に `=` と `,` があれば `=3D` と `=2C` に置き換える）、`client_final` は §5.2.2 の逆（`c=biws`、`p=` は `ClientKey XOR ClientSignature`）。

### 6.2 `hba.rs`

- `parse(text)`: 行に分け（続き行を結合）、`tokenize` → 欄の並びに分ける → §3.4 の文法で検査する。**最初のエラーで `Err` を返す**（行番号つき）。`all` の address は `HbaAddress::All`。`IP mask` は、mask が `255.255.0.0` のように連続した 1 の並びで、`IpAddr` の種類（v4 / v6）が揃っていなければエラー（`invalid IP mask`）。
- `find_rule` は §5.6。`rules` が空なら常に `None`。
- 警告（`warnings()`）の文言: `line {n}: connection type "{t}" never matches (yuzhu has no Unix sockets, TLS or GSSAPI); line ignored`、`line {n}: "password" sends the password in clear text (no TLS)`、`no valid entries; every connection will be rejected`。サーバが起動時に `tracing::warn!` で出す。
- `Server::bind(config)` は **クラスタを開く前に** `config.hba_file`（既定 `<data_directory>/yuzhu_hba.conf`）を読み、`HbaRules::parse` に失敗したら `io::Error::other(HbaError の文字列)` を返す（`main` は終了コード 1）。`Server::with_cluster`（テスト用）は `HbaRules::allow_all_trust()`。`Server::with_hba(self, rules)` で差し替えられる。

### 6.3 `auth.rs` と `connection.rs`

- `read_p_message`: タイプバイトを 1 バイト読み、`b'p'` でなければ `FATAL 08P01 expected {expect}, got message type {byte as decimal}`。続く 4 バイトの長さ `len` が 4 未満、または `len - 4 > limit` なら本体を読まずに `FATAL 08P01 invalid message length`。そうでなければ `len - 4` バイトを読む。EOF は `AuthFailure::Io`（切断。何も送らない）。
- `encode_auth_request` は §3.5 の表のとおり（長さ・タイプバイトを含めて `Vec<u8>` を返す）。
- `connection.rs` の変更は §5.1 の手順 1〜10 に対応する部分だけ。`message_loop` と Extended Query（XQ-1）には触れない。XQ-1 と衝突しないよう、認証と起動の関数は `auth.rs` に置き、`connection.rs` からは `auth::authenticate` と `Deadline` を呼ぶだけにする。
- `Shared` に `hba: HbaRules` を足す（不変。再読み込みは M6）。`Config` に `hba_file: PathBuf` と `authentication_timeout: Duration`（M1 の `DEFAULT_AUTHENTICATION_TIMEOUT` を既定にする）。設定ファイルのキーは `hba_file`・`authentication_timeout`（秒）、コマンドラインは `--hba-file`・`--authentication-timeout`。`FileConfig` は `deny_unknown_fields` なので 2 つのキーを足す。
- ログ（`tracing`）: 成功 `info!(pid, user, database, method, hba_line, "authenticated")`、失敗 `info!(pid, user, database, method, hba_line, reason, "authentication failed")`。パスワード・nonce・証明・秘密情報は出さない。

### 6.4 `catalog/roles.rs`

- `AuthidRow::{from_row, to_row}` は `schema::PG_AUTHID_COLUMNS` の順（`oid`、`rolname`、`rolsuper`、`rolinherit`、`rolcreaterole`、`rolcreatedb`、`rolcanlogin`、`rolreplication`、`rolbypassrls`、`rolconnlimit`、`rolpassword`、`rolvaliduntil`）。`rolname` は `Datum::Text`（M2 の `name` の保持と同じ）、`rolpassword` は `Datum::Text` か `Datum::Null`、`rolvaliduntil` は `Datum::TimestampTz` か `Datum::Null`。
- `authid_by_name` / `authid_by_oid` は `pg_authid` の全件走査（行は数個から数十個。M4 のカタログインデックスがあれば置き換えてよい）。
- `new_role_oid`: `alloc.next_raw()` で採り、`oid >= FIRST_NORMAL_OBJECT_ID`（16384）で、`snapshot_any()` の走査に同じ OID が無ければ返す。最大 100 万回試して失敗なら 54000（M2 の `get_new_oid` と同じ）。
- `update_authid` / `delete_authid`: `TableStore::update(rel, w, snap, tid, new_row, None, None)` / `delete(.., None, None)`（待たない）。`TmResult::Ok` 以外は `Error::new(XX000, "tuple concurrently updated")`（`Deleted` / `Updated` / `BeingModified`）。`SelfModified` は `27000`（M2 のとおり）。
- `fnv1a64(bytes)`: オフセット基底 `0xcbf29ce484222325`、素数 `0x100000001b3`。
- `owned_objects` は §5.9。他のデータベースのカタログを読むとき、そのデータベースの `CatalogStore` のスキャン関数を使う（`pub(crate)` にする依頼は §11 C1）。
- `has_*` は §4.4 のとおり（`superuser || 属性`）。`current_role(ctx)` は `authid_by_oid(snap, ctx.role.oid)`。

### 6.5 パーサ・アナライザ・実行（`sql/parser/role.rs`、`analyzer/ddl_ext/role.rs`、`ddl/role.rs`）

**パーサ**（`CREATE`・`ALTER`・`DROP` の後に `ROLE` / `USER` / `GROUP` が続くとき）:

```text
CREATE (ROLE|USER|GROUP) name [WITH] option*
ALTER  (ROLE|USER) rolespec [WITH] option*
ALTER  (ROLE|USER|GROUP) rolespec RENAME TO name
ALTER  (ROLE|USER) rolespec [IN DATABASE name] (SET ... | RESET ...)          → AlterRoleAction::Unsupported
ALTER  ROLE ALL (SET|RESET) ...                                                → Unsupported
ALTER  GROUP name (ADD|DROP) USER name [, ...]                                 → Unsupported
DROP   (ROLE|USER|GROUP) [IF EXISTS] name [, name ...]
DROP   OWNED BY ... / REASSIGN OWNED BY ...                                     → 解析して analyzer が 0A000
option := SUPERUSER | NOSUPERUSER | CREATEDB | NOCREATEDB | CREATEROLE | NOCREATEROLE | INHERIT | NOINHERIT
        | LOGIN | NOLOGIN | REPLICATION | NOREPLICATION | BYPASSRLS | NOBYPASSRLS
        | CONNECTION LIMIT signed-int | [ENCRYPTED] PASSWORD 'string' | PASSWORD NULL | UNENCRYPTED PASSWORD 'string'
        | VALID UNTIL 'string' | SYSID int | IN ROLE name-list | IN GROUP name-list | ROLE name-list | USER name-list | ADMIN name-list
```

- 属性名（`SUPERUSER` など）は PostgreSQL では識別子として読まれる非予約語。レキサーのキーワードにせず、**識別子（引用符なし、小文字化後）として比較する**。引用符つき（`"SUPERUSER"`）は一致しない（`unrecognized role option`）。未知の語は `42601 unrecognized role option "{x}"`（位置つき）。`ALTER ROLE` に `IN ROLE` / `SYSID` / `ADMIN` などの CREATE 専用のオプションが来たら `42601 syntax error at or near "{x}"`。
- `rolespec`: 識別子 | `CURRENT_USER` | `CURRENT_ROLE` | `SESSION_USER`。`CREATE` の名前に `public` / `none` が来たら `42939 role name "{x}" is reserved`、`CURRENT_USER` などは `42939 {X} cannot be used as a role name here`。`DROP ROLE` に特殊な指定が来たら `22023 cannot use special role specifier in DROP ROLE`。
- **`Secret`** は `CreateRoleStmt` / `AlterRoleStmt` / `BoundCreateRole` / `RoleChanges` の中でだけパスワードを持つ。`Debug` は `<redacted>`。`PartialEq` は値を比べる（試験用）。SQL テキスト全体を記録する機能は M5 の範囲に無いが、将来の `log_statement`（M6）は `Statement::{CreateRole, AlterRole}` を伏せ字にする。

**アナライザ**（`ddl_ext::role`。`analyzer/ddl.rs` の振り分けの 1 行は F0）:

- オプションを先頭から見て、同じ属性・同じ種類が 2 回あれば `42601 conflicting or redundant options`（位置はあとのほうの `pos`）。`SUPERUSER` と `NOSUPERUSER` のような反対どうしも同じ。
- `UNENCRYPTED PASSWORD` → `0A000 UNENCRYPTED PASSWORD is no longer supported`（HINT つき）。`IN ROLE` / `ROLE` / `USER` / `ADMIN` → `0A000 role membership is not supported yet`。`AlterRoleAction::Unsupported(what)` → `0A000 {what} is not supported yet`（例: `ALTER ROLE ... SET`）。
- `CONNECTION LIMIT n` で `n < -1` → `22023 invalid connection limit: {n}`。
- 既定値の適用: `CREATE ROLE` は `inherit = true`、`can_login = false`、`conn_limit = -1`、ほかは false。`CREATE USER` は `can_login = true`（明示の `NOLOGIN` が勝つ）。`ALTER` は指定された項目だけ `Some`。
- `BoundDdl` の変種は `CreateRole(BoundCreateRole)`・`AlterRole(BoundAlterRole)`・`DropRole(BoundDropRole)`（00 §4.7）。

**実行**（`ddl/role.rs`）は §5.7〜§5.9。`DdlCtx` が持つもの: `ctx.cluster`（`LockManager`、`oid_allocator`、`txn_manager`）、`ctx.db.shared`（`SharedCatalogStore`）、`ctx.txn`、`ctx.backend`、`ctx.wait`、`ctx.role`、`ctx.notices`、`ctx.type_env`、`ctx.password: PasswordPolicy`（§11 C4）。コマンドタグは `CREATE ROLE` / `ALTER ROLE` / `DROP ROLE`。

### 6.6 仮想リレーション（`catalog/roles.rs` の中）

```rust
#[derive(Debug)] pub struct PgRoles;        // oid() = 9810、name() = "pg_roles"、columns() = §5.10 の表
#[derive(Debug)] pub struct PgAuthMembers;  // oid() = 1261、name() = "pg_auth_members"、rows() = Ok(vec![])
// LK の registry() に &PgRoles と &PgAuthMembers を 1 行ずつ足す（virtual_rel.rs の持ち主は LK。依頼は §11 C6）
impl VirtualRelation for PgRoles {
    fn rows(&self, ctx: &VirtualCtx<'_>) -> Result<Vec<Row>> {
        // 最新のカタログ用スナップショットで authid_all → AuthidRow::from_row → 13 列の Row（rolpassword は Datum::Text("********")）
    }
}
```

### 6.7 `session` と `settings`

- `Session::new(cluster, params)`: `cluster.connect(&ConnectRequest { database, user, client_addr: params.client_addr, session_id, pid, interrupts })` を呼び（00 §4.8）、`ConnectGrant.role.superuser` から `is_superuser`（`on` / `off`）を `Settings` の読み取り専用の値として設定する（`set_server_value`）。M2 の `session_authorization` と同じ経路。
- `settings.rs`: §4.7 の 3 つ。`scram_iterations` を `reported = true`、`password_encryption` を列挙（`scram-sha-256`・`md5`）にする。M1 の試験 `initial_parameter_status_lists_reported_parameters`（13 個）は 14 個に変える。`INERT_GUCS`（M4）には足さない（値を使うので）。
- `DdlCtx.password` は session が文ごとに `Settings` から作る（`password_encryption`、`scram_iterations`）。

### 6.8 エラー一覧（SQLSTATE。00 §3.5・§3.8 に従う。表に無いものは §11 で足した）

| 状況 | SQLSTATE | 重大度 | メッセージ |
|---|---|---|---|
| hba に一致なし | 28000 | FATAL | `no pg_hba.conf entry for host "{addr}", user "{user}", database "{db}", no encryption` |
| hba の reject | 28000 | FATAL | `pg_hba.conf rejects connection for host "{addr}", user "{user}", database "{db}", no encryption` |
| 認証失敗 | 28P01 | FATAL | `password authentication failed for user "{user}"` |
| 平文の空パスワード | 28P01 | FATAL | `empty password returned by client` |
| ロールが無い（trust） | 28000 | FATAL | `role "{user}" does not exist` |
| LOGIN でない | 28000 | FATAL | `role "{user}" is not permitted to log in` |
| ロールの接続数 | 53300 | FATAL | `too many connections for role "{user}"` |
| `max_connections` | 53300 | FATAL | `sorry, too many clients already`（M1 のまま） |
| 認証の期限切れ | 57014 | FATAL | `canceling authentication due to timeout` |
| SCRAM の形式・順序の誤り | 08P01 | FATAL | `malformed SCRAM message` / `invalid SCRAM response` / `unexpected SCRAM channel-binding attribute in client-final-message` / `non-printable characters in SCRAM nonce` / `unexpected SCRAM message`（DETAIL は §5.2.2） |
| 機構名の誤り・メッセージの種類・長さ | 08P01 | FATAL | `client selected an invalid SASL authentication mechanism` / `expected SASL response, got message type {n}` / `expected password response, got message type {n}` / `invalid message length` / `invalid password packet size` / `malformed SASLInitialResponse` |
| authzid・拡張属性 | 0A000 | FATAL | `client uses authorization identity, but it is not supported` / `client requires an unsupported SCRAM extension` |
| 乱数が取れない | XX000 | FATAL | `could not generate random numbers` |
| ロールの重複 | 42710 | ERROR | `role "{x}" already exists` |
| ロールが無い | 42704 | ERROR | `role "{x}" does not exist`（`IF EXISTS` は NOTICE `role "{x}" does not exist, skipping`） |
| 予約名 | 42939 | ERROR | `role name "{x}" is reserved`（`pg_` は DETAIL `Role names starting with "pg_" are reserved.`） |
| 権限不足 | 42501 | ERROR | `permission denied to create role` / `alter role` / `drop role`（DETAIL は §5.7〜§5.9） |
| `pg_authid` の SELECT | 42501 | ERROR | `permission denied for table pg_authid` |
| 自分自身の DROP | 55006 | ERROR | `current user cannot be dropped` |
| 所有物あり | 2BP01 | ERROR | `role "{x}" cannot be dropped because some objects depend on it`（DETAIL は §5.9） |
| システムのロールの DROP | 2BP01 | ERROR | `cannot drop role {x} because it is required by the database system` |
| オプションの矛盾 | 42601 | ERROR | `conflicting or redundant options`（位置つき） |
| 不明なオプション | 42601 | ERROR | `unrecognized role option "{x}"`（位置つき） |
| 接続数の値 | 22023 | ERROR | `invalid connection limit: {n}` |
| 日時の書式 | 22007 | ERROR | `invalid input syntax for type timestamp with time zone: "{x}"` |
| MD5 | 0A000 | ERROR | `MD5 password encryption is not supported` |
| UNENCRYPTED | 0A000 | ERROR | `UNENCRYPTED PASSWORD is no longer supported` |
| メンバーシップなど | 0A000 | ERROR | `role membership is not supported yet` / `{x} is not supported yet` |
| カタログ行の競合 | XX000 | ERROR | `tuple concurrently updated` / `tuple concurrently deleted` |

### 6.9 他の章との境界

- **09 章（DB）**: `Cluster::connect` の骨格（`ConnectRequest` / `ConnectGrant`、`BackendRegistry` への登録、データベースの検査、`Database` ロック、データベースの接続数）は 09。この章は §5.1 手順 9 の a・b・e（ロールの存在・LOGIN・ロールの接続数）の中身と、その順序（AU-D10）を決める。順序の入れ替え（M2 のデータベース → ロールを、ロール → データベースに）は 09 の `connect` の実装に含める。CREATE DATABASE の権限は `catalog::roles::{current_role, has_createdb}` を 09 が呼ぶ（メッセージは 09 が決める）。データベースの所有者にするロールは `lock_role_shared` を取る。
- **01 章（LK）**: 仮想リレーションの仕組み（`VirtualRelation`、`registry()`、`VirtualScan`、relkind `v` の行を書く initdb の口）は 01。この章は `pg_roles` と `pg_auth_members` の実体と OID を足す。リレーションの解決のところで `check_catalog_select` を呼ぶのは LK-4（§11 C5）。`LockTag::Object` の使い方は §5.7 の鍵の規則に従い、`LockManager` の API（00 §4.2）をそのまま使う。
- **06 章（TD）**: `Cluster::clock()`（`Clock::unix_micros`、`PG_EPOCH_UNIX_MICROS`）。`VALID UNTIL` の解析は `input_text` + `TypeEnv.datetime`（M4 / TD）。
- **04 章（XQ）**: `connection.rs` の持ち主。この章が足すのは §5.1 の手順 1〜10 に対応する起動処理だけ（00 §8 の注）。メッセージループの変更とは別の関数・別のファイルにして衝突を避ける。`BackendKeyData` の `secret` の乱数も、起動処理の一部としてここで変える。
- **05 章（TY）**: `gen_random_uuid()` は `yuzhu_auth::random::fill`。`rolconfig` の型 `text[]`（OID 1009）の `pg_type` の行は TY が持つ。

---

## 7. テスト

### 7.1 `yuzhu-auth` の単体テスト（AU-1）

| 対象 | 内容 |
|---|---|
| PBKDF2 | §3.6 の RFC 7914 のベクタ（`passwd` / `salt` / 1 回 → 32 バイト）。反復回数が大きいときの先頭の一致（`Password` / `NaCl` / 80000 回など、RFC 7914 の別のベクタは本文で照合してから足す） |
| 秘密情報 | `make_secret_with_salt("pencil", salt, 4096)` が §3.1 の例の文字列と**完全に一致**。`ScramSecret::parse` ↔ `encode` の往復。不正な形式が解析に失敗する表（接頭辞違い、`$` が足りない・多い、反復回数が 0・負・符号つき・非数・`i32::MAX + 1`、ソルトの base64 の誤り、StoredKey が 31 / 33 バイト、ServerKey の長さ違い、パディングなし、末尾の余分な文字、小文字の接頭辞）。`is_scram_secret` と `md5` + 32 桁の判別 |
| `verify_plain_password` | 正しいパスワード、違うパスワード、空のパスワード、SCRAM でない `secret`（平文・MD5・空）、非 ASCII（NFC と NFD のどちらで作った秘密情報も、どちらの綴りの入力でも一致） |
| SASLprep | §3.6 の 7 つの例。ASCII は `Borrowed`。禁止文字では `try_prepare` が `Err` で、`prepare` は入力をそのまま返す |
| `ServerExchange`（RFC のベクタ） | 試験用の nonce の固定（`#[cfg(test)]` のコンストラクタで `server_nonce = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0"`）。§3.5 の 4 つのメッセージを入出力にして、`client_first` が server-first-message と**バイト単位で一致**、`client_final` が server-final-message と一致 |
| `ServerExchange`（誤りの表） | 次の入力ごとに期待するエラー: 空、`p=tls-server-end-point,,n=,r=x`、`x,,n=,r=x`、`n;n=,r=x`、`n,a=foo,n=,r=x`（Unsupported）、`n,,m=x,n=,r=x`（Unsupported）、`n,,r=x`、`n,,n=u`、`n,,n=,r=`（nonce が空）、`n,,n=,r=a b`（印字不能の nonce）。`client_final`: 順序違い（`client_first` の前）、`c=` の不一致（`biws` の代わりに `eSws`、`p=...` の gs2 ヘッダ）、nonce の不一致、証明の長さが 31 / 33 バイト、属性の欠落、末尾に余分な属性、1 ビットだけ違う証明（`Failed`）、2 回目の `client_final`（`Protocol`） |
| 偽の秘密情報 | `secret = None`、平文の `secret`、MD5 形式の `secret` のすべてで、(1) `client_first` が成功し、(2) ソルトが `mock_secret` と一致し、(3) 正しいパスワードで作った `ClientExchange` でも `client_final` が `Failed`。同じユーザー名・同じ nonce で 2 回作ると server-first のソルトと `i=` が同じ（server nonce は違う）。違うユーザー名・違う nonce ではソルトが違う。`alice` + 0 の nonce で `Uk7MU63Yg/hh/ptZ9hodwg==`（§3.3） |
| 往復（proptest） | ランダムなパスワード（ASCII・非 ASCII・空を除く）、ランダムなソルトと反復回数（1〜200）で、`make_secret` → `ClientExchange` ↔ `ServerExchange` が成功し、`verify_server_final` も成功。パスワードを 1 文字変えると `Failed` |
| `random` | `fill` が 0 でない値を返す（長さ 32 で全部 0 は偽陽性が 2^-256）、`bytes::<N>` |
| `Debug` | `ScramSecret` の `Debug` に鍵が出ない（文字列に StoredKey の base64 を含まない） |

### 7.2 `hba.rs` の単体テスト（AU-2）

| 対象 | 内容 |
|---|---|
| 解析（正常） | §3.4 の例のファイルが 5 行の規則になる。コメント・空行・続き行・タブ・引用符つきの名前（`"all"` が名前）・コンマ区切り（`a,b`・`a, b`）。`local` / `hostssl` / `hostgssenc` の行が読み捨てられて警告になる。`hostnossl` が `host` になる。`md5` が `ScramSha256` になる。`IP マスク` の形（`192.168.0.0 255.255.0.0` が `/16`）。IPv6 の CIDR |
| 解析（エラー） | §3.4 の表の全行。行番号が正しい（続き行は先頭の行）。最初のエラーで止まる |
| 照合 | 上から順の最初の一致（後ろの `reject` に落ちない）、IPv4 / IPv6 の種類の違いは不一致、`::ffff:127.0.0.1` が IPv4 の規則に一致、`/0`・`/32`・`/128`、ホストビットが立った CIDR、`sameuser`（database == user）、データベース省略時の既定（database = user）、名前の大文字小文字を区別、`replication` は決して一致しない、一致なしは `None` |
| 既定の hba | `bootstrap::default_hba` の 3 つの方式がすべて `parse` に通り、127.0.0.1 と ::1 に一致し、192.0.2.1 に一致しない |

### 7.3 `yuzhu-server/tests/auth.rs`（AU-2。接続の統合テスト）

`yuzhu-server` を `Server::with_cluster` で起動し（`SimVfs` ではなく一時ディレクトリ + `initdb`）、`Server::with_hba` で規則を差し替える。クライアントは `postgres`（dev-dependency。SCRAM を話す）と、生メッセージの試験用の小さなクライアント（`ClientExchange` を使う）。

| テスト | 内容 |
|---|---|
| `trust_connects` | `trust` で接続でき、`SELECT current_user` が返る。存在しないロール → 28000 `role "ghost" does not exist`（AuthenticationOk の後の ErrorResponse） |
| `scram_success` | `ALTER ROLE ... PASSWORD` で設定したパスワードで `postgres` クレートが接続できる。`SHOW is_superuser`・`current_user` が正しい |
| `scram_wrong_password` | 28P01 `password authentication failed for user "alice"`（FATAL。DETAIL なし） |
| `scram_no_such_role` | 存在しないロールでも 28P01 で、メッセージが「間違ったパスワード」と**完全に同じ**。生メッセージの試験で、同じユーザー名を 3 回試すと server-first のソルトと `i=` が毎回同じで、実在のロールのソルト（乱数）と違う。別のユーザー名ではソルトが違う |
| `scram_no_password_or_expired` | パスワードが NULL のロール、`VALID UNTIL` が過去のロール（`ManualClock` で時刻を進めて確かめる）はどちらも 28P01。`VALID UNTIL 'infinity'` は成功 |
| `scram_nologin_role` | 認証は成功し、その後 28000 `role "x" is not permitted to log in`。パスワードが誤りなら 28P01 が先（LOGIN の検査は認証の後） |
| `password_method` | `password` 方式で成功・失敗（28P01）。空のパスワード（`empty password returned by client`）。NUL の位置が不正（08P01 `invalid password packet size`）。存在しないロール |
| `reject_and_no_match` | `reject` の行 → 28000 `pg_hba.conf rejects connection ...`。一致なし → 28000 `no pg_hba.conf entry for host "127.0.0.1", user "u", database "d", no encryption`。最初の一致が勝つ（`trust` の前に `reject` を置くと reject）。IPv6 の `::1` |
| `sameuser_and_default_db` | `database` 省略の接続（database = user）と `sameuser` |
| `scram_protocol_errors` | 生メッセージ: 機構名が違う、初期応答なし（長さ -1）、長さが 1025 以上の `p`（本体を送らずに 08P01 `invalid message length`。サーバは本体を読まない）、`p` 以外のタイプバイト、client-final の nonce 改ざん（08P01 `invalid SCRAM response`）、`c=` 改ざん、証明の改ざん（28P01）、`p=` でのチャネルバインディング（08P01）。いずれも接続が閉じる |
| `channel_binding_flag_y` | gs2 ヘッダ `y,,` で成功（`ClientExchange` を拡張せず、client-first を手で組み立てる） |
| `auth_timeout` | `with_authentication_timeout(300ms)`: (1) 何も送らない → 閉じる（メッセージなし）、(2) 起動パケットを送って SASLInitialResponse を送らない → FATAL 57014、(3) 100ms ごとに 1 バイトずつ送る → 合計 300ms 付近で閉じる（1 バイトごとのタイムアウトでは閉じない） |
| `connection_limit_role` | `CONNECTION LIMIT 1` のロールで 2 本目 → 53300 `too many connections for role "x"`。スーパーユーザーは対象外。1 本目を閉じると 2 本目が通る。`CONNECTION LIMIT 0` は全拒否 |
| `max_connections_after_auth` | M1 の試験の回帰: 認証前のソケットが `max_connections` に数えられず、認証後に超過なら 53300 |
| `psql_password_flow` | `ClientExchange` が計算した SCRAM の秘密情報を `ALTER USER "alice" PASSWORD 'SCRAM-SHA-256$...'`（libpq の `PQchangePassword` と同じ形）で渡して保存でき、そのパスワードで接続できる。`SHOW password_encryption` が `scram-sha-256`、ParameterStatus に `scram_iterations`（`4096`）が含まれる |
| `backend_key_secret` | 2 つの接続の `BackendKeyData` の `secret` が異なる（乱数）。CancelRequest（M3）が正しい鍵で効き、違う鍵では効かない |
| `initdb_pwfile` | `initdb` に `auth_host = ScramSha256` と超ユーザーのパスワードを渡し、`Server` を起動して、そのパスワードで接続できる。パスワードなしで `ScramSha256` を指定すると 22023 |
| `default_hba_loads` | `Server::bind` が `initdb` の生成した `yuzhu_hba.conf` を読んで起動する。壊れたファイルでは `bind` が `Err`（クラスタを開く前。pid ファイルが残らない）。ファイルが無いと `Err` |

#### 7.3a 変異テスト（10 章 §6.9 の #18。レビュー対応 R-09）

この章は `DebugKnobs` に **`scram_skip_proof_check`**（`ServerExchange::client_final` がクライアントの `ClientProof` を検証せずに成功にする）を足す（既定は無効。CLI・設定・SQL から変えられない）。`yuzhu-auth` は `yuzhu-core` に依存しないので、スイッチは `yuzhu-auth` の `ServerExchange` の `pub fn with_skip_proof_check_for_mutation_test(self) -> Self`（`#[cfg(any(test, feature = "mutation-test"))]`）として持ち、`yuzhu-server/src/auth.rs` が `Cluster` の `DebugKnobs.scram_skip_proof_check` を見て呼ぶ。F0 が `DebugKnobs` に項目を足す（§11 の C13）。検出するもの: `yuzhu-server/tests/auth.rs` の「誤ったパスワードで接続できてしまう」、`tests/compat/auth`。

### 7.4 ロールの DDL のテスト（AU-3）

**共有の SQL テスト**（`tests/slt/m5/auth/`。PostgreSQL 17 でも通ること。超ユーザー `postgres` で実行し、**各ファイルの終わりで作ったロールを必ず DROP する**（M2-Q22 の教訓）。ロール名は `slt_<ファイル名>_<n>` で衝突を避ける）:

| ファイル | 内容 |
|---|---|
| `role_create.slt` | `CREATE ROLE` / `CREATE USER` / `CREATE GROUP` の既定値（`pg_roles` の列）、全属性の指定と `NO...`、`WITH` の有無、`CONNECTION LIMIT`、`VALID UNTIL`（`infinity` を含む）、`PASSWORD NULL`、`SYSID`（無視）、コマンドタグ、エラー（42710 重複、42939 の `pg_`・`public`・`none`、42601 の矛盾したオプション・不明なオプション、22023 の `CONNECTION LIMIT -2`、22007 の `VALID UNTIL 'garbage'`、0A000 の `IN ROLE` と `UNENCRYPTED PASSWORD`）。SQLSTATE とメッセージの先頭を照合する（DETAIL は照合しない） |
| `role_alter.slt` | 属性の変更（指定した項目だけ変わる）、`RENAME TO`、`ALTER USER`、`CURRENT_USER`、`PASSWORD` の変更の後の `rolpassword` が `SCRAM-SHA-256$4096:` で始まる（値そのものは比べない）、NOTICE `empty string is not a valid password, clearing password`、存在しないロール（42704）、`pg_` への改名（42939）、既存の名前への改名（42710）、`ALTER ROLE ... SET` の 0A000 は yuzhu 専用のファイル（`role_alter_unsupported.slt`。`--target pg` では実行しない） |
| `role_drop.slt` | `DROP ROLE` / `IF EXISTS`（NOTICE）/ 複数、存在しない（42704）、自分自身（55006）、所有物があるロール（2BP01。テーブルの所有者にするには `SET ROLE` が要るので、**所有物のある DROP は Rust のテストで確かめ、このファイルには入れない**）、システムのロール（2BP01 `cannot drop role pg_database_owner because it is required by the database system`。PostgreSQL では `pg_database_owner` が pinned のため同じエラーになることを実機で確かめてから入れる。§9） |
| `pg_roles.slt` | 列の名前・順序・型（`information_schema` が無いので `pg_attribute` と `format_type`）、`rolpassword` が常に `********`、`rolconfig` が NULL、`pg_auth_members` が 0 行・列、`pg_roles` への INSERT が 55000（`cannot insert into view "pg_roles"`。【実機】PG17.11 も同じ SQLSTATE とメッセージなので共有テストに入れてよい。R-10）、ROLLBACK で CREATE ROLE が消える、COMMIT 前は別接続から見えない（`connection` 指示子） |

**Rust のテスト**（`yuzhu-core` の単体、`yuzhu-server/tests/auth.rs`）:

| 対象 | 内容 |
|---|---|
| `AuthidRow` | `to_row` ↔ `from_row` の往復、`valid_until` の換算（`infinity` → `None`、`-infinity` → `Some(i64::MIN)`、2000-01-01 → 946_684_800_000_000）、`Debug` にパスワードが出ない |
| パーサ | 全オプションの組み合わせ、`CONNECTION LIMIT -1`、引用符つきの属性名（不明なオプション）、`ALTER ROLE ... IN DATABASE ... SET`（Unsupported）、`DROP ROLE a, b`、位置（`pos`）、`Secret` が `Debug` に出ない |
| 権限（一般ロールで接続） | CREATEROLE なしの `CREATE ROLE` → 42501 `permission denied to create role`。CREATEROLE ありで `CREATE ROLE x` は成功、`CREATE ROLE x SUPERUSER` は 42501、CREATEDB を持たないのに `CREATEDB` を付けると 42501。CREATEROLE で他の非スーパーユーザーの ALTER・DROP が成功し、スーパーユーザーの ALTER・DROP は 42501。自分のパスワードだけは CREATEROLE なしでも変更できる（`ALTER ROLE me CONNECTION LIMIT 1` は 42501）。**ロールの属性を ALTER した直後の同じセッションの権限判定が最新になる**（接続時の値を使わない） |
| `pg_authid` の制限 | 一般ロールの `SELECT * FROM pg_authid` が 42501、`pg_roles` は成功、スーパーユーザーは両方成功 |
| 所有物の検査 | ロール `r` が所有するテーブルを作り（一般ロールで接続して CREATE TABLE）、別のセッションの DROP ROLE が 2BP01 + DETAIL `owner of table t`。別のデータベースにあるテーブルでも 2BP01（DETAIL `1 object in database {db}`）。データベースの所有者のとき `owner of database {db}`。テーブルを DROP してから DROP ROLE が成功 |
| 並行 | 2 つのセッションが同じ名前を `CREATE ROLE`: 後のほうが待ち、先のコミットの後 42710、ROLLBACK の後は成功。`CREATE ROLE x` と `DROP ROLE x` の競合。`ALTER ROLE` の同時実行が直列になり、両方の変更が（別の項目なら）残る。デッドロック（A: `CREATE ROLE x` → B: `CREATE ROLE y` → A: `CREATE ROLE y`、B: `CREATE ROLE x`）が 40P01。`DROP ROLE` と、そのロールを所有者にする `CREATE TABLE`（`lock_role_shared` を取る側）の競合 |
| トランザクション | ブロック内の `CREATE ROLE` → ROLLBACK で消え、クラッシュ（M3 の `crash_sim`）の後も、コミットした CREATE / ALTER / DROP が残り、コミットしていないものは見えない。**`global/` の共有カタログへの書き込みが WAL・FPW・REDO で動く**こと（§9） |
| パスワード | `password_encryption = md5` で `PASSWORD 'x'` → 0A000、`PASSWORD 'md5<32 桁>'` → 0A000、SCRAM 形式の値はそのまま保存（`pg_authid` で完全一致）、`scram_iterations = 10` の後の `CREATE ROLE ... PASSWORD` の `rolpassword` が `SCRAM-SHA-256$10:` で始まる |
| `lookup_role` / `connect` | 存在しないロール、LOGIN でない、`conn_limit` が `-1` / `0` / `1`、スーパーユーザーの除外、`valid_until` の換算、ロール → データベースの順序（存在しないロールと存在しないデータベースの両方 → 28000） |
| 仮想リレーション | `pg_roles` の列・型・`rolpassword` の伏せ字、`pg_auth_members` が 0 行、`\du` の問い合わせ（`ARRAY(SELECT ...)` を使う。TY-5 が済んでから。副問い合わせなしの `SELECT rolname FROM pg_roles` は先に動く） |
| initdb | 超ユーザーの行が OID 10、`rolpassword` が `--pwfile` の SCRAM、`yuzhu_hba.conf` が `default_hba`、制御ファイルの `mock_auth_nonce` が 0 でない（initdb ごとに違う） |

### 7.5 psql・psycopg の接続とトレース（TS-4 が CI のジョブにする。この章が内容を決める）

準備: `yuzhu-initdb -D $DIR -U postgres --auth-host=scram-sha-256 --pwfile=<パスワード `postgres` を書いたファイル>`、`yuzhu-server`、比較用の本物の PostgreSQL 17（`scram-sha-256` の `host` 行。`POSTGRES_PASSWORD=postgres` と `POSTGRES_HOST_AUTH_METHOD=scram-sha-256`）。

1. **psql**（`postgres:17` コンテナの psql を使う。`PGPASSWORD=postgres`、`-h host -p port -U postgres`）: `SELECT 1`、間違ったパスワード（`FATAL: password authentication failed for user "postgres"`。PostgreSQL と標準エラーが完全に同じ）、パスワードなしの接続（`fe_sendauth: no password supplied`）、`\password alice`（`SHOW password_encryption` と `scram_iterations` の読み取り → `ALTER USER "alice" PASSWORD 'SCRAM-SHA-256$...'`）の後に alice で接続できる、`\du`（TY-5 が済んでから）。
2. **psycopg 3**（`psycopg.connect("host=... user=... password=...")`）: 成功と失敗（`psycopg.OperationalError` の文言が PostgreSQL と同じ）。`SELECT current_user`。`sslmode=prefer` の既定で `SSLRequest` → `N` → 続行できる。
3. **プロトコルのトレース**: psycopg の `conn.pgconn.trace(fileno)`（libpq の `PQtrace`）の出力から、認証の部分（`AuthenticationSASL`、`SASLInitialResponse`、`AuthenticationSASLContinue`、`SASLResponse`、`AuthenticationSASLFinal`、`AuthenticationOk`、`ParameterStatus` の並び、`BackendKeyData`、`ReadyForQuery`）を取り出し、nonce・ソルト・MAC・pid・秘密鍵・時刻の値を伏せ字にして、PostgreSQL 17 と yuzhu の**メッセージの種類と長さの並びが一致する**ことを比べる（ゴールデンファイルは PostgreSQL で採る）。**ParameterStatus に `scram_iterations` が含まれるかをここで確かめ**（含まれなければ AU-D12 を戻す）、順序も一致させる。失敗の接続（間違ったパスワード）のトレースも比べる（`ErrorResponse` の `S V C M` の欄）。
4. **PostgreSQL が作った秘密情報の再現**（`tests/compat/` の Rust テスト。環境変数 `YUZHU_TEST_PG` があるときだけ動く）: 本物の PostgreSQL 17 で `CREATE ROLE x PASSWORD 'p'` した後の `rolpassword` を読み、ソルトと反復回数を取り出して `make_secret_with_salt('p', salt, iter)` が**完全に一致**する。非 ASCII のパスワード（NFC / NFD）でも一致する。逆に、yuzhu の `make_secret` が作った値を PostgreSQL の `ALTER ROLE ... PASSWORD '...'` に渡して、その値で PostgreSQL に SCRAM 接続できる。
5. **sqllogictest（`tests/run.sh --target yuzhu` と `--target pg`）の SCRAM 構成**: `--user postgres --password postgres`（TS-4 が `run.sh` に `--password` を足す。§11 C12）。M1〜M4 のスイートが SCRAM 構成でもそのまま通る。

---

## 8. 実装の分担と工数

00 §7 の WP（ID と見積もりは変えない）を詳しくした。工数は AI の実装エージェント 1 本の日数（粗い見積もり）。

| WP | 内容 | ファイル（持ち主） | 依存 | 日数 |
|---|---|---|---|---|
| **AU-1** | `yuzhu-auth`: `scram.rs`（秘密情報、`ServerExchange`、偽のソルト）1、`saslprep.rs`・`random.rs` 0.25、`scram_client.rs` 0.5、RFC のベクタ・誤りの表・往復の試験 0.5、ルートの `Cargo.toml` 0.25 | `yuzhu-auth/*`、ルートの `Cargo.toml`（5 行） | F0（ただし新規クレートだけなので、M4 の実装中に始めてよい。D49） | 2.5 |
| **AU-2** | サーバの認証: `hba.rs`（解析・照合）0.75、`auth.rs`（メッセージの読み書き、方式ごとの手順、`RoleDirectory`）1、`connection.rs` の起動の順序・期限・乱数・`config.rs` 0.5、`bootstrap.rs` / `rows.rs` / `yuzhu-initdb`（`auth_host`、`pwfile`、`mock_auth_nonce`、hba の生成）0.5、`tests/auth.rs`・`hba` の単体試験（上の各項目に含む）0.25 | `yuzhu-server/src/{hba,auth,connection(認証の部分),config}.rs`、`bin/yuzhu-initdb.rs`、`tests/auth.rs`、`yuzhu-core/src/{bootstrap,catalog/rows}.rs` の追加分 | AU-1。`Cluster::lookup_role`・`mock_auth_nonce`（AU-3 と同時）、F0（制御ファイルの `mock_auth_nonce`、`StartupParams.client_addr`） | 3 |
| **AU-3** | ロールの DDL: `sql/parser/role.rs` と AST 0.5、`analyzer/ddl_ext/role.rs` 0.5、`catalog/roles.rs`（`AuthidRow`、`SharedCatalogStore` の読み書き、述語、ロック鍵）0.75、`ddl/role.rs`（CREATE / ALTER）0.75、DROP と `owned_objects` 0.5、`pg_roles` / `pg_auth_members` と `pg_authid` の制限 0.25、`Cluster::lookup_role`・`connect` から呼ばれるロールの検査の関数（§5.1 手順 9 の a・b・e。**`Cluster::connect` の本体は DB-1（09）が置き換える**。AU は `engine.rs` の `connect` を編集しない。00 §8.1。R-38）・`is_superuser`・`settings.rs` 0.25 | `sql/parser/role.rs`、`analyzer/ddl_ext/role.rs`、`ddl/role.rs`、`catalog/roles.rs`、`engine.rs`（`lookup_role` など）、`settings.rs`（自分の分）、`tests/slt/m5/auth/*`、ロールの Rust テスト | AU-1（`make_secret`）、LK-5（仮想リレーションの仕組み）、F0（`Statement` と `BoundDdl` の変種、`DdlCtx` の項目）、M4（`ddl/` の `DdlCtx`）、LK-1（`LockTag::Object`） | 3.5 |

- **進め方**: (1) AU-1 を先に始める（F0 を待たない）。(2) F0 が済んだら AU-2 の `hba.rs`・`auth.rs`（`Cluster` を使わず `RoleDirectory` の試験用の実装で書ける）と AU-3 のパーサ・アナライザ・`catalog/roles.rs` を並列に始める。(3) `Cluster::lookup_role` を足した時点で AU-2 の接続の統合試験が動く。(4) LK-5 が済んだら `pg_roles`。
- **ほかの章への依頼**（§11 に一覧）: F0 が制御ファイル・`StartupParams`・`InitdbOptions` の呼び出し側・`store.rs` のアクセサ。LK が `registry()` と `check_catalog_select` の呼び出し。DB が `connect` の順序。TS が `run.sh` / `yuzhu.sh` の SCRAM 構成と CI のジョブ。
- **カットライン上の位置**: AU は 00 §1.3 のカットラインに入っていない。遅れたときに落とせるのは、(1) `--auth-host=password` と `password` 方式（`scram-sha-256` と `trust` だけでも ドライバは動く）、(2) `owned_objects` の他のデータベースの走査（現在のデータベースと `pg_database` だけに戻す。C-7 のとおり）、(3) `pg_auth_members`（`\du` が 42P01 になる）。

---

## 9. 未検証の点（実装前に確かめるもの）

| # | 内容 | 確かめ方 |
|---|---|---|
| 1 | ~~**`scram_iterations` が PostgreSQL 17 の ParameterStatus の報告対象か**~~ **確認済み（レビュー対応 R-14）**: 【実機】PG17.11 の起動時の ParameterStatus は 14 個で `scram_iterations=4096` を含む（順序は `in_hot_standby`、`integer_datetimes`、`TimeZone`、`IntervalStyle`、`is_superuser`、`application_name`、`default_transaction_read_only`、`scram_iterations`、`DateStyle`、`standard_conforming_strings`、`session_authorization`、`client_encoding`、`server_version`、`server_encoding`。順序まで合わせるかは実装時に決める）。元の記述: 起動時の ParameterStatus が 14 個になるか | PostgreSQL 17 に libpq のトレース（§7.5 の 3）で接続して、`ParameterStatus` の名前の一覧を取る。含まれなければ `settings.rs` の `reported` を戻し、M1 の試験（13 個）を戻す。含まれる場合は順序（PostgreSQL は GUC の定義順）も合わせる |
| 2 | 偽のソルトの作り方（`SHA-256(user ‖ mock_auth_nonce)` の先頭 16 バイト、反復回数 4096、鍵は 0）と、`mock_auth_nonce` が 32 バイトであること【記憶: PG:src/backend/libpq/auth-scram.c の `scram_mock_salt` と `mock_scram_secret`、`pg_control.h` の `MOCK_AUTH_NONCE_LEN`】 | ソースを読む。違っても yuzhu の内部の一貫性（同じユーザー名で同じソルト）は保たれるので、回帰試験の固定値（§3.6）だけ直せばよい |
| 3 | SCRAM のメッセージの誤りの DETAIL の文言（§5.2.2）と、`08P01` / `0A000` の割り当て【記憶: `read_client_first_message`・`read_client_final_message`】 | `auth-scram.c` を読んで文言を合わせる。試験は SQLSTATE と `message` だけを固定し、DETAIL は部分一致にしてある |
| 4 | `stringprep::saslprep` と PostgreSQL の `pg_saslprep` の出力が、非 ASCII のパスワードで一致するか（Unicode 3.2 の表と NFKC の扱い）。クライアント（libpq は `pg_saslprep`、tokio-postgres は `stringprep`）と食い違うと非 ASCII のパスワードで認証できない | §7.5 の 4（PostgreSQL が作った秘密情報を再現）に、NFC / NFD・全角・結合文字・禁止文字を含む例を足す。一致しない文字があれば、その文字を例外として表に起こす |
| 5 | hba の字句: 引用符の中の `""`、続き行、コンマの前後の空白、`all` を引用符で囲んだときの扱い（§3.4）【記憶: PG:src/backend/libpq/hba.c の `next_token`、`tokenize_expand_file`】 | ソースを読む。違う場合は §3.4 を直し、`HbaRules::parse` の試験に足す |
| 6 | PostgreSQL が IPv4-mapped の IPv6 アドレス（`::ffff:127.0.0.1`）を IPv4 の規則に一致させるか（§5.6 は一致させる） | 実機で `listen_addresses = '::'` にして IPv4 で接続し、`pg_stat_activity.client_addr` を見る。yuzhu の既定の待ち受けは 127.0.0.1 なので影響は小さい |
| 7 | `authentication_timeout` の期限切れのメッセージ（`canceling authentication due to timeout`、57014）と、起動パケットの受信中の期限切れで何も送らないこと【記憶: PG:src/backend/tcop/postgres.c の `ProcessInterrupts`、postmaster.c の `StartupPacketTimeoutHandler`】 | ソースを読む。`authentication_timeout = 1s` の PostgreSQL に `nc` で接続して挙動を見る |
| 8 | `InitPostgres` の検査の順序（認証 → ロールの存在・LOGIN・ロールの接続数 → データベースの存在・`datallowconn`・データベースの接続数。AU-D10）、`rolconnlimit` の検査の位置（`miscinit.c` の `InitializeSessionUserId` か `postinit.c` か）【記憶】 | `psql -U 存在しないロール -d 存在しないデータベース`（trust）の結果が `role ... does not exist` になるか。ロールの接続数とデータベースの接続数の両方が超過した接続で、どちらのエラーが出るか |
| 9 | `CREATE ROLE` / `ALTER ROLE` / `DROP ROLE` の権限の判定の細部（PG:src/backend/commands/user.c）: CREATEROLE 属性を付ける権限（§5.7 は CREATEROLE 保持者に許す）、`ALTER ROLE` で自分のパスワードだけ変えられる条件、エラーの DETAIL の文言、ブートストラップ超ユーザーが SUPERUSER を失えないエラーの SQLSTATE と文言【記憶。PG16 の変更を含む】 | `user.c` を読む。実機で一般ロールを作って各操作の結果を採り、`tests/slt/m5/auth/` の期待値にする（メッセージの先頭だけを照合） |
| 10 | 空文字列のパスワードの NOTICE（`empty string is not a valid password, clearing password`）が `CREATE ROLE` と `ALTER ROLE` の両方で出ること、「空のパスワードの SCRAM 秘密情報」の場合も出ること【記憶】 | 実機で確かめる |
| 11 | `DROP ROLE` の DETAIL の書式（他のデータベースのオブジェクトの `N object in database X`、100 行の上限、`owner of table t` の schema 修飾の規則）と、`pg_database_owner`・ブートストラップ超ユーザーの DROP のエラー（`cannot drop role ... because it is required by the database system`）【記憶: `shdepReportDependentObjects`、`checkSharedDependencies`】 | 実機で確かめる。違う DETAIL は `role_drop.slt` の期待値に影響しない（DETAIL は照合しない）が、Rust のテストの期待値は直す |
| 12 | **共有カタログ（`global/`。`db_oid = 0`）のヒープへの書き込みが M3 の WAL・FPW・REDO・チェックポイントで動くか**。M3 までは initdb 以外に共有カタログを書く経路が無い（ロールの DDL と CREATE DATABASE が最初の利用者）。`RelFileLocator` の共有カタログの扱い、REDO でファイルを開く場所（`global/` か `base/0/` か）、ページのチェックサムの検査 | `crash_sim` に「CREATE ROLE をコミット → クラッシュ → 残る」を足して確かめる（§7.4）。動かなければ F0 / 該当の章に報告 |
| 13 | PostgreSQL 17 の psql の `\du` の問い合わせが使う表・関数（`pg_auth_members`、`ARRAY(SELECT ...)`、`pg_roles` の列）【記憶】と、`pg_type` に `_text`（OID 1009）の行があること | `psql -E` で `\du` の SQL を採る。`_text` が無ければ TY-1 に依頼（`rolconfig` の型） |
| 14 | libpq の `PQencryptPasswordConn` が `SHOW password_encryption` を実行し、`scram_iterations` を ParameterStatus から読むこと【記憶: PG:src/interfaces/libpq/fe-auth.c】 | `PQtrace` で `\password` の通信を採る |
| 15 | `getrandom` 0.4 の `std` 機能が `std::io::Error` への変換を提供すること、`Cargo.lock` に既にある `getrandom` 0.3.4 との共存で問題が出ないこと | `cargo tree -d` と最初のビルド。変換が無ければ `io::Error::other(e.to_string())` |
| 16 | `pbkdf2` 0.13 の `pbkdf2_hmac_array::<Sha256, 32>` の型引数の形（`sha2::Sha256` は `EagerHash`）、`hmac` 0.13 の `Mac::finalize().into_bytes()` | ソースの `pbkdf2-0.13.0/src/lib.rs`・`hmac-0.13.0/src/lib.rs` は確認済み。初回のビルドで型が通ること |
| 17 | ロール DDL の同時実行のレース（`DROP ROLE` と、所有者にする `CREATE TABLE`）を M4 の `ddl/` が `lock_role_shared` で塞ぐ（§11 C9）までの間、所有者のいないテーブルが残りうること | M4 のマージ後に CREATE TABLE の 1 行を足す |

---

## 10. 確認事項

仮決めのまま進めます。ユーザーに確認したい点です。変更する場合の影響を併記します。

- **M5-AU-Q1 initdb の既定は `trust`（PostgreSQL と同じ。警告つき）**（AU-D21）。`--auth-host=scram-sha-256 --pwfile` で変えられる。理由: 既存のテスト・CI・`tests/yuzhu.sh` が認証なしで動き続ける。待ち受けの既定が 127.0.0.1 なので外には届かない。変えたい場合: 既定を `scram-sha-256` にすると、`yuzhu-initdb` の呼び出し（`tests/yuzhu.sh`、各クレートの結合テスト 5 か所、CI）にパスワードの引数を足す必要がある（+0.5 日）。
- **M5-AU-Q2 `max_connections` の検査が認証の後**（AU-D6）。PostgreSQL は認証の前。理由: M1 の設計（認証前のソケットが本物のクライアントを締め出さない）を保つ。認証前の資源はスレッド上限と `authentication_timeout` で守る。変えたい場合: 認証の前に数える（接続ごとの `CountGuard` を起動の最初に移す。+0.25 日）。認証前のソケットで締め出される。
- **M5-AU-Q3 制御ファイルに `mock_auth_nonce`（32 バイト、オフセット 128）を足す**（AU-D4。F0 の作業に +0.25 日）。理由: 偽のソルトがユーザー名だけで決まると、存在しないロールが見分けられる。変えたい場合: プロセスごとの乱数にする（制御ファイルの変更が要らない。サーバの再起動をまたぐと偽のソルトが変わるので、再起動を観測できる攻撃者には存在が分かる）。
- **M5-AU-Q4 認証エラーの文言の `pg_hba.conf` は PostgreSQL のまま**（`yuzhu_hba.conf` と書かない）。理由: ドライバ・テストが PostgreSQL の文言を照合する。ファイル名の違いは利用者の混乱の元になりうるので、将来 DETAIL で補うことはできる。変えたい場合: 文言を `yuzhu_hba.conf` にする（既存のスクリプトの照合が壊れる可能性）。
- **M5-AU-Q5 hba は PostgreSQL の部分集合（AU-D8）**: `local`・`hostssl`・`hostgssenc` は読み捨て、ホスト名・`samehost`・`samenet`・`+group`・`/regex`・`@file`・ident / peer / cert など・方式のオプションは起動エラー。再読み込みは M6。理由: 黙って意味を変える機能は受け付けない。標準の `pg_hba.conf` の貼り付けが `local` 行で壊れない。変えたい場合: ホスト名（逆引き）と `samehost` を足すと +1 日、SIGHUP での再読み込みは +0.5 日。
- **M5-AU-Q6 DROP ROLE は全データベースの所有物を走査する**（AU-D14。調査 C-7 の「見逃す」案を採らない）。`pg_shdepend` は作らない。理由: DROP ROLE は稀で、全データベースのカタログを読む費用は問題にならない。所有者が消えたテーブルが残る事故を防ぐ。変えたい場合: 現在のデータベースだけにする（−0.25 日。他のデータベースの所有物を見逃す）。`pg_shdepend` を作る場合は +3 日（`DROP OWNED` / `REASSIGN OWNED` と一緒に M6 で）。
- **M5-AU-Q7 M5 にはデータのアクセス制御が無い**（§1.4）。ログインできるロールはすべてのテーブルを読み書きでき、他のロールのテーブルを DROP できる。理由: GRANT / REVOKE・所有者の検査は M6（00 §1.1）。最小の追加は「所有者かスーパーユーザーだけが DROP / ALTER / TRUNCATE できる」検査（`ddl/` の各文に `current_role` と `relowner` の比較を足す。+1.5 日。M4 の `ddl/` と VC の TRUNCATE に触る）。入れるなら M6 の GRANT と一緒に。
- **M5-AU-Q8 ロールの権限モデルは PostgreSQL 15 以前の意味**（AU-D15）。CREATEROLE は非スーパーユーザーのすべてのロールに効く（PostgreSQL 16 以降は ADMIN OPTION が要る）。エラーの文言と「持っていない属性を付けられない」規則は PostgreSQL 16 以降と同じ。メンバーシップ（`IN ROLE`・`GRANT role`・`SET ROLE`）は 0A000。変えたい場合: メンバーシップ（`pg_auth_members` の中身、`GRANT role`、`SET ROLE`、`pg_hba` の `+group`）を M6 で入れるときに ADMIN OPTION の検査も入れる。
- **M5-AU-Q9 `pg_authid` の SELECT はスーパーユーザーだけ**（AU-D17。M2-Q13 の (6) の解消）。`pg_roles` は誰でも読めて `rolpassword` が `********`。検査の呼び出しは LK-4 の関係の解決に 1 行（C5）。変えたい場合: 全員に読ませる（パスワードのハッシュが一般ロールに見える。推奨しない）。
- **M5-AU-Q10 平文の `password` 方式では、存在しないロールの判定が時間差で分かる**（パスワードを受け取ってから失敗し、PBKDF2 を走らせない分速い）。PostgreSQL も同じ。`scram-sha-256` は偽のソルトで時間差を消す。理由: `password` 方式は TLS が無いと平文が流れるので、そもそも勧めない（起動時に WARNING）。
- **M5-AU-Q11 `scram_iterations` を ParameterStatus で報告する**（AU-D12。【実機】PG17.11 で報告対象と確認済み。レビュー対応 R-14）。変えたい場合: 報告しない（`reported = false`。libpq は 4096 を使う【記憶】）。PG と差が出る。
- **M5-AU-Q12 接続の検査の順序を PostgreSQL にそろえる**（AU-D10）。M2 はデータベース → ロールで、PostgreSQL はロール → データベース。両方が違う接続のエラーが変わる（3D000 → 28000）。M2 の試験に該当があれば直す。09 章の `connect` の実装と合わせる。
- **M5-AU-Q13 `\du` のために `pg_auth_members` を 0 行の仮想リレーションとして置く**（AU-D18）。`\du` は `ARRAY(SELECT ...)`（TY-5。カットライン上位）が済むまで動かない。変えたい場合: 置かない（`\du` が 42P01）。
- **M5-AU-Q14 認証の期限は全体の壁時計**（AU-D7）。PostgreSQL と同じ。M1 の「読み書きごとの期限」から変わる。遅い回線のクライアントが 60 秒以内に認証を終えられないと切られる（PostgreSQL と同じ）。
- **M5-AU-Q15 `pg_roles` の OID を 9810 にする**（00 §3.2 の 9800〜9899 の申請。C6）。`pg_auth_members` は PostgreSQL の OID 1261 をそのまま使う。
- **M5-AU-Q16 `yuzhu-initdb` の既定の超ユーザー名は `postgres`**（M2 のコードのまま。PostgreSQL の initdb は OS のユーザー名）。理由: `psql -h localhost -U postgres`（既定のデータベース名 = ユーザー名 = `postgres`）がそのまま動く。M2 §5.8 の設計書（OS のユーザー名）とコードが食い違っていたので、コードに合わせて記録する。
- **M5-AU-Q17 認証失敗の遅延は入れない**（`auth_delay` 相当なし）。総当たりへの対策は `CONNECTION LIMIT` と hba の `reject` だけ。理由: M5 の範囲外。変えたい場合: 失敗の後に 1 秒の遅延を入れるのは接続ごとのスレッドなので数行だが、スレッドを占有する（DoS の道具になる）。
- **M5-AU-Q18 SCRAM の秘密情報の iterations の読み方は PG と同じ（strtol。前置の空白・符号・0・負を許す）。ただし認証で iterations < 1 の秘密情報は 28P01 にする**（R-33。§3.1）。理由: 他クラスタから移した秘密情報や `\password` の値を PG と同じ判定で「SCRAM 形式」と扱い、平文として再ハッシュしない。変えたい場合: 厳しい読み方（符号・空白を拒否）にすると、PG が SCRAM 形式と判定する値を yuzhu が平文と見なして再ハッシュする差が出る（10 章の台帳に載せる）。

### PostgreSQL との既知の差（`10-tests-plan.md` が集める。共有テストには入れない）

1. CREATEROLE の権限モデル（ADMIN OPTION なし。M5-AU-Q8）。
2. `max_connections` の検査が認証の後（M5-AU-Q2）。
3. MD5 認証・MD5 形式のパスワード・`password_encryption = md5` での保存は 0A000。保存された MD5 形式のパスワードを持つロールは認証できない（PostgreSQL は MD5 認証をする。AU-D9）。
4. hba の機能の範囲（M5-AU-Q5）、再読み込みなし。
5. 他のデータベースの所有物がある DROP ROLE の DETAIL の書式（§9-11）。
6. ロールの所属・`SET ROLE`・`GRANT` / `REVOKE`・ロールごとの設定・`DROP OWNED` は 0A000。データのアクセス制御なし（M5-AU-Q7）。
7. `yuzhu-initdb` の既定の超ユーザー名（M5-AU-Q16）。
8. カタログの一意性を名前ロックで守る（PostgreSQL は一意インデックス）。同じ名前の同時の CREATE ROLE の待ちの動きは同じだが、`tuple concurrently updated` が出る場面が違いうる。

---

## 11. 契約への変更依頼

00 のシグネチャ（§4.8・§4.11・§4.12）は**変えていない**。追加と、他の章・F0 への依頼を次に挙げる。

| # | 内容 | 相手 | 理由 |
|---|---|---|---|
| C1 | `catalog/store.rs` の `SharedCatalogStore` に、`pg_authid` と `pg_database` の読み書きを別ファイルに書けるよう、(1) `pub(crate) fn rel(&self, catalog_oid: Oid) -> Option<&(Arc<TableDef>, RelHandle)>`、(2) `pub(crate) fn storage(&self) -> &Arc<dyn TableStore>` を足す。`CatalogStore` の private な `scan`（M2 の `get_new_oid` が使っているもの）を `pub(crate) fn scan_catalog(&self, snap: &Snapshot, catalog_oid: Oid, keep: impl FnMut(&Row) -> Result<bool>) -> Result<Vec<HeapTuple>>` として公開する（他のデータベースの `pg_class` などの走査に使う） | F0（store.rs は M4 のファイル。00 §8 の例外としてアクセサだけ） | 00 §8 は `store.rs` を各章が直接編集せず `roles.rs` などに `impl` を書くとしているが、フィールドが private で書けない。DB 章の `store_db.rs` も同じものが要る |
| C2 | **制御ファイルに `mock_auth_nonce: [u8; 32]`（オフセット 128）を足す**。`ControlData`・`encode_slot` / `decode_slot` に含め、initdb が `yuzhu_auth::random::bytes::<32>()` で作る。`Cluster::mock_auth_nonce()` で読む。`format_version` 3 に含める（D35 のとおり形式の版は 1 回だけ上げる） | F0（`control.rs`。`next_multi` と同時） | AU-D4。00 §3.7 の制御ファイルの表（`next_multi` のオフセット 120）に 1 行足す |
| C3 | `StartupParams` に `client_addr: Option<std::net::IpAddr>` を足し、`Session::new` が `ConnectRequest.client_addr` と `BackendInfo.client_addr` に渡す。`Session::new` は `ConnectGrant.role.superuser` から `is_superuser` を設定する | F0（`session/mod.rs`。AU が行を足す） | 00 §4.1・§4.8 の `client_addr` の出どころが無い |
| C4 | `DdlCtx`（00 §4.7）に `pub password: PasswordPolicy`（`PasswordPolicy { encryption: PasswordEncryption, scram_iterations: u32 }`。型は `ddl/role.rs`）を足す。session が `Settings` の `password_encryption` と `scram_iterations` から文ごとに作る | F0・LK（`DdlCtx` の持ち主） | `SessionInfo` が設定を持たないため、`ddl/role.rs` が設定を読めない |
| C5 | LK-4 の「文が参照するリレーションの解決」（`session/locking.rs`）が、AccessShare を取る前に `catalog::roles::check_catalog_select(rel_oid, &role)` を呼ぶ（`pg_authid` = OID 1260 だけが `42501` を返す。ほかは `Ok(())`） | LK | AU-D17。アナライザにはロールが見えない |
| C6 | **OID の申請（00 §3.2）**: `pg_roles` = **9810**（relkind `v`、データベースごとの `pg_class` の行）。`pg_auth_members` は PostgreSQL の **1261** を使う（yuzhu 独自の範囲ではない）。LK が `pg_locks` に使う OID と重ならないこと（LK が 9811 以降を使うなら問題なし）。LK の `virtual_rel::registry()` に `&PgRoles`・`&PgAuthMembers`（§6.6）を 1 行ずつ足す | LK | 仮想リレーションの登録 |
| C7 | `InitdbOptions` に `auth_host: InitAuth`・`superuser_password: Option<Secret>` を足し、`Default` を実装する。`catalog::rows::InitParams` に `superuser_secret: Option<String>` を足し、`authid_rows(superuser, secret)` が OID 10 の `rolpassword` に入れる。構造体リテラルで作っている呼び出し側（`yuzhu-core/src/testing.rs`、`yuzhu-server/tests/shutdown.rs`、`bin/yuzhu-initdb.rs`）を `..Default::default()` に直す | F0（呼び出し側の機械的な更新）、AU-2（追加の中身） | §5.11 |
| C8 | ルートの `impl/rust/Cargo.toml` の `members` に `crates/yuzhu-auth`、`[workspace.dependencies]` に `yuzhu-auth`（path）と §6.1 の 7 つを足す。AU-1 が M4 の実装中に最初のコミットで 1 回だけ行う（F0 が先に済ませていればそれを使う） | F0 / AU-1 | D49: AU-1 は M4 の実装中に始めてよい |
| C9 | **所有者にするロールのロック**: M4 の `ddl/` の CREATE TABLE（と CREATE INDEX・CREATE SEQUENCE の所有者）と 09 章の CREATE DATABASE が、所有者のロール OID に `catalog::roles::lock_role_shared` を呼ぶ（AccessShare・トランザクションスコープ。M4 のマージ後に 1 行） | M4 の `ddl/` の持ち主（F0 経由）、DB | §5.9 の DROP ROLE との競合を塞ぐ。未対応の間は既知の差（§9-17） |
| C10 | `ClusterOptions` に `authentication_timeout: Duration`（既定 60 秒）を足し、`Cluster::server_settings()` に `("authentication_timeout", "1min")` を載せる（`SHOW` できるようにするだけ。サーバが実際に使う値は `Config.authentication_timeout`） | LK・DB（`engine.rs`） | 00 §3.6 の設定の表（`authentication_timeout` は AU） |
| C11 | `Cluster::connect` の検査の順序の変更（AU-D10。§5.1 手順 9）と、ロールの接続数（手順 9 の e。`BackendRegistry::count_of_role`）を 09 章の実装に含める。M2 の `connect` の「データベース → `datallowconn` → ロール」はロール → データベースに変わる | DB | AU-D10。00 §4.8 の `connect` の説明は「3D000 / 55000 / 28000 / 53300 を FATAL で返す」で、順序を書いていない |
| C12 | `tests/yuzhu.sh` に `--auth scram`（`yuzhu-initdb --auth-host=scram-sha-256 --pwfile` と、サーバの起動待ちのパスワード付き接続確認）、`tests/run.sh` に `--password`（sqllogictest-bin の `--password` に渡す）を足し、CI に SCRAM 構成のジョブを足す（`--target yuzhu` と `--target pg` の両方） | TS（TS-4） | §7.5 |
| C13 | `DebugKnobs` に `scram_skip_proof_check` を足す（F0。§7.3a。10 章 §6.9 の #18） | F0 | 変異テスト。R-09 |

- 追加のみで 00 のシグネチャを変えていないもの: `yuzhu-auth` の `AuthError`・`ScramSecret`・`make_secret_with_salt`・`mock_secret`・`ServerExchange::new_with_mock_nonce`・`is_doomed`・`scram_client`・`saslprep`・`random::bytes`、`hba::{HbaError, HbaRule, find_rule, warnings, rules, allow_all_trust}`、`Cluster::{mock_auth_nonce, clock}`（`clock` は TD の追加）。
- **00 と食い違うところ**: なし。00 §3.6 の `scram_iterations`（「ParameterStatus で報告する」）は【実機】PG17.11 で報告対象と確認済み（R-14。00 §3.6・§10.2 を直した）。
