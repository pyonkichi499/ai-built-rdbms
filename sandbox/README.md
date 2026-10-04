# sandbox/

Claude Code を `claude-sandbox`（承認なしで動かすための Docker コンテナ）で使うためのイメージと補助スクリプトです。

| ファイル | 内容 |
|---|---|
| `Dockerfile` | イメージ `yuzhu-sandbox`。Rust stable（rustfmt / clippy）、sqllogictest-bin 0.29.1、PostgreSQL 17、Claude Code |
| `pg.sh` | コンテナ内で PostgreSQL 17 を `127.0.0.1:55432` に起動・停止する（`tests/pg.sh` の代わり） |

```sh
docker build -t yuzhu-sandbox sandbox      # ホストで実行（Rust や Claude Code を更新するときも同じ）
claude-sandbox                             # リポジトリ内で実行（イメージは .claude-sandbox.toml で指定済み）
```

## コンテナ内での違い

- ビルド成果物は `$CARGO_TARGET_DIR`（`/home/sandbox/cargo-target`）に出る。ホストの `impl/rust/target` とは別
- PostgreSQL は `sandbox/pg.sh start` で起動する（docker は使えない）。`PGHOST` / `PGPORT` / `PGUSER` は設定済み
- コミットはできる（署名なし）。push はホストで行う

