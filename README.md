# yuzhu

yuzhu は、PostgreSQL ワイヤプロトコル互換のクライアント/サーバ型 RDBMS をゼロから作るプロジェクトです。
まず Rust で実装し、その後ほかの言語（Go / C / C++ / Python 以外）でも再実装します。

- 要件定義: <https://claude.ai/code/artifact/9381e901-9f2a-49a4-9bb2-479295dde6e2>

## 名前の由来

ゲーム『活俠傳』に登場する **郁竹（yuzhu）** から。崆峒派・鉄拳門の鍛冶師で、
道具を一から鍛え上げる職人のように、データベースの中核部品をすべて手作りすることにちなんでいます。

## ディレクトリ構成

```
.
├── spec/        # 言語非依存の共有仕様
├── tests/       # 言語非依存の共有 sqllogictest スイートとランナー設定
└── impl/
    └── rust/    # Rust 実装（Cargo ワークスペース）
        └── crates/
            ├── yuzhu-core/    # コアライブラリ
            └── yuzhu-server/  # サーババイナリ
```

## ビルド方法（Rust 実装）

動作環境は Linux のみです（開発は WSL2 を想定）。Rust stable（1.96 以上）が必要です。

```sh
cd impl/rust
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo run -p yuzhu-server
```

## Docker

リポジトリ直下の `Dockerfile` で yuzhu-server のイメージを作れます（マルチステージビルド。ビルドは `rust:1.96-bookworm`、実行は `debian:bookworm-slim`、非 root ユーザー `yuzhu` で動作）。

```sh
docker build -t yuzhu:dev .
docker run --rm -p 5432:5432 yuzhu:dev
psql -h 127.0.0.1 -p 5432 -U postgres
```

コンテナは `yuzhu-server --listen 0.0.0.0 --port 5432` で起動します。追加の引数（例: `--log-level debug`、`--max-connections 50`）は `docker run` のイメージ名の後ろに付けてください。現時点（M1）ではデータはメモリ上のみで、コンテナを止めると消えます。

## ライセンス

[LICENSE](LICENSE) を参照してください。
