# claude-sandbox 用のイメージ（Rust + sqllogictest + PostgreSQL 17 + Claude Code）。
#   docker build -t yuzhu-sandbox sandbox
#   claude-sandbox（イメージはリポジトリ直下の .claude-sandbox.toml で指定）
# コンテナはホストの UID（/etc/passwd に無い）で動き、HOME は /home/sandbox になる。
FROM debian:bookworm-slim

# 開発ツールと PostgreSQL 17（PGDG）。メインクラスタは自動で作らせない（sandbox/pg.sh で作る）。
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git curl ca-certificates build-essential pkg-config libssl-dev \
      ripgrep jq less procps libnss-wrapper \
 && install -d /usr/share/postgresql-common/pgdg \
 && curl -fsSL -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc \
      https://www.postgresql.org/media/keys/ACCC4CF8.asc \
 && echo 'deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main' \
      > /etc/apt/sources.list.d/pgdg.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends postgresql-common \
 && sed -ri 's/^#?\s*create_main_cluster\s*=.*/create_main_cluster = false/' /etc/postgresql-common/createcluster.conf \
 && apt-get install -y --no-install-recommends postgresql-17 \
 && rm -rf /var/lib/apt/lists/*

# Rust（rust-toolchain.toml は channel = "stable"。CI と同じくビルド時点の最新 stable）と sqllogictest-bin。
# どの UID からも使えるよう /opt に読み取り専用で置く。
ENV RUSTUP_HOME=/opt/rustup \
    PATH=/opt/cargo/bin:/usr/lib/postgresql/17/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs \
      | CARGO_HOME=/opt/cargo sh -s -- -y --no-modify-path --profile minimal \
          --default-toolchain stable --component rustfmt,clippy \
 && CARGO_HOME=/opt/cargo cargo install sqllogictest-bin --locked --version 0.29.1 \
 && rm -rf /opt/cargo/registry /opt/cargo/git \
 && chmod -R a+rX /opt/rustup /opt/cargo

# Claude Code
RUN HOME=/opt/claude bash -c 'curl -fsSL https://claude.ai/install.sh | bash' \
 && ln -s "$(readlink -f /opt/claude/.local/bin/claude)" /usr/local/bin/claude \
 && chmod -R a+rX /opt/claude
ENV DISABLE_AUTOUPDATER=1

# cargo のキャッシュとビルド成果物は HOME（リポジトリごとの state ディレクトリで、起動をまたいで残る）に置く。
# ホストの impl/rust/target などとは取り合わない。
# PostgreSQL のクライアントは sandbox/pg.sh で起動するサーバ（TCP のみ）に繋ぐ。
ENV CARGO_HOME=/home/sandbox/.cargo \
    CARGO_TARGET_DIR=/home/sandbox/cargo-target \
    CARGO_TERM_COLOR=always \
    PGHOST=127.0.0.1 \
    PGPORT=55432 \
    PGUSER=postgres
