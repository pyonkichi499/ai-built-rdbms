# yuzhu-isolation

PostgreSQL の [isolationtester](https://github.com/postgres/postgres/tree/REL_17_STABLE/src/test/isolation) と同じ
`.spec` 形式を読み、複数の接続で permutation を実行する独立したテストランナーです。
`spec/research/m3-tx-semantics.md` §12 の「(b) ブロックする交互実行」の層を担います。

- **Simple Query プロトコルだけ**を使います（本家は Extended Query も使うため、M5 より前の yuzhu には向けられない）。
- 出力は isolationtester と同じ形式です。PostgreSQL 本体の `expected/*.out` とそのまま diff できます。
- `impl/rust` のワークスペースには入れない独立した Cargo プロジェクトです（言語非依存のテスト道具として `tests/` に置く）。

```
tests/
├── isolation/
│   ├── specs/*.spec        共有の isolation テスト
│   └── expected/*.out      期待出力（本物の PostgreSQL 17 で生成）
└── tools/isolation/        このランナー
    ├── src/spec.rs         spec のパーサ（specscanner.l / specparse.y と同じ文法）
    ├── src/conn.rs         Simple Query だけを話す同期クライアント（libpq の PQsendQuery / PQisBusy / PQgetResult 相当）
    ├── src/runner.rs       permutation の実行と出力（isolationtester.c を写したもの）
    ├── src/main.rs         CLI・期待ファイルとの比較
    └── validate-pg-suite.sh  PostgreSQL 本体の isolation テストでランナー自身を検証する
```

## ビルド

```sh
cd tests/tools/isolation
cargo build --release          # target/release/yuzhu-isolation
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

## 実行

```sh
# 本物の PostgreSQL に対して（期待ファイルの検証）
tests/pg.sh start
tests/tools/isolation/target/release/yuzhu-isolation --port 55432 tests/isolation/specs

# yuzhu に対して（pg_isolation_test_session_is_blocked() が未実装の間は timeout 判定を使う）
tests/tools/isolation/target/release/yuzhu-isolation --port 5432 \
    --blocking-detection timeout tests/isolation/specs
```

各 spec ごとに `ok` / `FAILED` を表示し、失敗したものは期待ファイルとの unified diff を出します。
1 つでも失敗すると終了コード 1 です。

### 主なオプション

| オプション | 既定値 | 説明 |
|---|---|---|
| `--host` / `-p, --port` | `127.0.0.1` / `5432` | 接続先。`--host` が `/` で始まれば UNIX ソケットのディレクトリ |
| `-U, --user` / `-d, --dbname` | `postgres` / `postgres` | |
| `--password` | 環境変数 `PGPASSWORD` | 平文・MD5・SCRAM-SHA-256 認証で使う（TLS は未対応） |
| `--set NAME=VALUE` | なし | 起動パケットで送る実行時パラメータ（複数可）。例: `--set 'datestyle=Postgres, MDY'` |
| `--blocking-detection pg\|timeout` | `pg` | ロック待ちの判定方法（下記） |
| `--block-timeout-ms` | `300` | timeout 判定で「待ち」とみなすまでの時間 |
| `--max-step-wait` | `30`（秒） | 1 ステップの最大待ち時間。超えたら CancelRequest を送り、2 倍で打ち切る（本家の `max_step_wait` は 360 秒） |
| `--expected-dir` | spec のディレクトリの隣の `expected/` | 期待ファイルの場所 |
| `--variant NAME` | なし | 実装別の期待ファイル `<name>.<NAME>.out` も正解の候補にする |
| `--results-dir DIR` | なし | 実際の出力を `DIR/<name>.out` に書く |
| `--print` | | 比較せず、出力をそのまま標準出力に書く |
| `--accept` | | 実際の出力で期待ファイルを上書きする（**本物の PostgreSQL に対してだけ**使う） |

期待ファイルは pg_regress と同じく `<name>.out`、`<name>_1.out` … `<name>_9.out` のどれかと一致すれば合格です。

## ロック待ちの判定

isolationtester はステップを送った後、10ms ごとに「応答が来たか」を確かめ、来ていなければ
ステップがロック待ちかを判定します。待ちなら `<waiting ...>` と表示して次のステップに進みます。

- **`pg`（既定）**: 本家と同じく、制御用の接続で
  `SELECT pg_catalog.pg_isolation_test_session_is_blocked(<pid>, '{<全セッションの pid>}')` を問い合わせます。
  pid は BackendKeyData から取ります（Simple Query なので pid は SQL に直接埋め込む）。
  yuzhu ではこの関数が実装されるまで使えません（`m3-tx-semantics.md` §5.5）。
- **`timeout`**: `--block-timeout-ms` の間に完了しなければ待ちとみなします。どのサーバでも動きますが、
  - 単に遅いステップ（`pg_sleep` など）も待ちと判定されることがある
  - 待ち中のステップを確かめるたびに最大 `--block-timeout-ms` かかる（待ちの多いテストは遅くなる）

  という弱点があるので、yuzhu が関数を実装したら `pg` に切り替えます。

## spec 形式

本家の `src/test/isolation/README` と同じです。

```
setup { SQL }               # 複数可。制御用の接続で permutation ごとに実行
teardown { SQL }
session <name>
  setup { SQL }             # 省略可
  step <name> { SQL }       # 1 つ以上
  teardown { SQL }          # 省略可
permutation <step> ...      # 省略するとすべての順序を実行
```

- `#` から行末まではコメント。名前は `"..."` で囲むとキーワードや空白も使えます。
- permutation のステップには完了報告を遅らせる指定を付けられます:
  `s1a(*)`（必ず一度 `<waiting ...>` と表示）、`s1a(s2b)`（s2b が終わるまで完了を報告しない）、
  `s1a(s2b notices 1)`（s2b のセッションが NOTICE を 1 つ出すまで報告しない）。
- 出力の形式も本家どおりです: `step s1: <SQL>`、`<waiting ...>`、`step s1: <... completed>`、
  PQprint 形式の結果表、`ERROR:  <主メッセージ>`（DETAIL は XID を含みうるので出さない）、
  `s1: NOTICE:  ...`、`s1: NOTIFY "ch" with payload "x" from s2`。
  `unused step name: ...` などの診断も（本家が `2>&1` で期待ファイルに含めるのと同じく）出力に混ぜます。

## 本家 isolationtester との違い

| 項目 | isolationtester | このランナー |
|---|---|---|
| プロトコル | Simple Query + Extended Query（`PQexecParams`、`PQprepare`） | Simple Query のみ |
| application_name | 接続後に `set_config()` で `<PGAPPNAME>/<セッション名>` | 起動パケットで `isolation/<テスト名>/<セッション名>`（pg_isolation_regress 経由で動かしたときと同じ値） |
| ロック待ち判定 | `pg_isolation_test_session_is_blocked()` | 同左、または timeout |
| `max_step_wait` | 360 秒（`PG_TEST_TIMEOUT_DEFAULT` の 2 倍） | `--max-step-wait`（既定 30 秒） |
| 実行時パラメータ | pg_isolation_regress が環境変数（`PGDATESTYLE` など）で設定 | 何も設定しない。必要なら `--set` |
| 未対応 | — | COPY、TLS、GSSAPI |

## 検証状況

`validate-pg-suite.sh` で PostgreSQL 17 本体の isolation スケジュール（123 テスト）を postgres:17（17.11）に対して
実行し、`--set 'datestyle=Postgres, MDY'` 付きで 120 テストが本家の期待ファイルと完全に一致しました。
残りの 3 つ（`insert-conflict-serializable`、`merge-delete`、`tablespace-dependency-locking`）は、REL_17_STABLE の先端で
直ったサーバ側の挙動の差で、ランナーの出力形式の問題ではありません。
`--blocking-detection timeout` でも、試した 16 テスト（`eval-plan-qual`、`deadlock-hard`、
`insert-conflict-specconflict` など）がすべて一致しました。

```sh
docker exec <コンテナ> createdb -U postgres isolation_regression
tests/tools/isolation/validate-pg-suite.sh --port 55432                     # 全テスト
tests/tools/isolation/validate-pg-suite.sh --port 55432 eval-plan-qual      # 一部だけ
```

## テストの追加

1. `tests/isolation/specs/<name>.spec` を書く（テーブル名はテストごとに一意にし、`teardown` で必ず DROP する）。
2. 本物の PostgreSQL 17 に対して `--accept` で期待ファイルを作り、中身をレビューする。
   ```sh
   tests/pg.sh start
   tests/tools/isolation/target/release/yuzhu-isolation --port 55432 --accept tests/isolation/specs/<name>.spec
   ```
3. yuzhu の段階的な制約で結果が違うもの（M3 の REPEATABLE READ 拒否など）は、PostgreSQL の期待ファイルを
   書き換えず、`--variant yuzhu-m3 --accept` で `expected/<name>.yuzhu-m3.out` を別に持つ（`m3-tx-semantics.md` §12.2）。
   ただし `--accept` は差分を確認してから使うこと。

現在のテスト:

| spec | 内容 | 必要な機能 |
|---|---|---|
| `rc-visibility` | Read Committed の可視性（未コミットは見えない・コミット後は次の文で見える・読み取りは待たない・ROLLBACK） | M3 |
| `lost-update` | 同じ行の同時 UPDATE で後者が待ち、RC では最新行で再実行、RR では 40001 | M3（RR の permutation は M5） |
| `write-skew-rr` | Repeatable Read では write skew を防げない（両方コミットでき、当番が 0 人になる） | M5（RR、count） |
