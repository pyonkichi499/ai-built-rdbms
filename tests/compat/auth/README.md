# tests/compat/auth: 認証の互換テスト（M5。10 §6.10.4）

`tests/compat/run.sh --target pg|yuzhu --suite auth` が `auth/run.sh` を呼ぶ。単独でも `TARGET=pg tests/compat/auth/run.sh [A2 A3 ...]` で動く。

- **サーバはシナリオごとに専用のものを起動する**（`lib.sh`）。認証方式（hba）と `authentication_timeout` を変えるので、共有サーバの方式には依存しない。
  TARGET が `pg` なら `initdb` / `pg_ctl`（コンテナでは nss_wrapper）、`yuzhu` なら `YUZHU_BIN_DIR`（または `$CARGO_TARGET_DIR/release`）の
  `yuzhu-initdb` / `yuzhu-server`。ポートは `AUTH_PORT_BASE`（既定 7430）から順に使う。終了時にすべて止めて消す（`AUTH_KEEP=1` で残す）。
- **`client/` の `authc`**: 生ソケットのクライアント（postgres-protocol）。libpq は接続エラーの SQLSTATE を出さず、送信元アドレスも選べないので、
  A2・A3・A5 の判定（`ok/<要求された認証> | SQLSTATE`）と A7（認証の途中で止まる）に使う。独立した Cargo プロジェクト（`Cargo.lock` 固定。`--locked`）。
- 期待値は **PostgreSQL 17 で確かめたもの**。`hba/*.conf` は `pg_hba.conf` と `yuzhu_hba.conf` の同じ内容、`*.cases` は
  `(ユーザー, DB, 接続元アドレス, パスワード) → 結果` の表。

| ID | 場所 | 内容 |
|---|---|---|
| A2 | `roles/a2.sh` | 誤ったパスワード・存在しないユーザー・パスワードなし（28P01 と同じ文言）、NOLOGIN（28000） |
| A3 | `hba/a3.sh`、`hba/*.conf`、`hba/*.cases`、`hba/bad/` | hba の照合の表。`bad/` は解析エラーでサーバが起動しないこと |
| A4 | `secret-compat/a4.sh` | 秘密の相互互換。固定ベクトル（TARGET のみ）、yuzhu ⇄ PostgreSQL（両方を起動。片方が使えなければ SKIP）。`AUTH_A4_OTHER=pg` でシナリオ自体を PG 同士で検証できる |
| A5 | `roles/a5.sh` | VALID UNTIL、CONNECTION LIMIT、PASSWORD NULL、`password` 方式、`scram_iterations` |
| A6 | `roles/a6.sh` | psql `\password`（管理者が他のロール / 本人）→ 再接続 |
| A7 | `roles/a7.sh` | `authentication_timeout=2s`: 沈黙・起動パケットのみ・1 バイトずつの 3 通り |
| A8 | `roles/a8_a9.sh` | libpq と postgres-protocol。残り 3 つ（pgJDBC・node-postgres・psycopg）は drivers の C1（`--auth scram`） |
| A9 | `roles/a8_a9.sh` | TLS なしの `channel_binding=require` は失敗（クライアント側） |

## PostgreSQL 17.11 での実測（設計書との差）

- コンマの前の空白（`app ,other`）は区切りにならず、PostgreSQL は `invalid authentication method` で起動しない（`hba/bad/04-*`）。08 §3.4 は `a ,b` を `a,b` と同じとしている。
- 連続しないマスク（`127.0.0.0 255.0.255.0`）を PostgreSQL は受け付ける。08 §3.4 は解析エラーにする（PostgreSQL と異なる）。
- `authentication_timeout` の期限切れで、PostgreSQL は何も通知せず接続を閉じる（起動パケット前も SASL の途中も）。yuzhu の設計は FATAL 57014（A7-startup-57014 は yuzhu だけの主張）。
