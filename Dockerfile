# yuzhu-server の Docker イメージ（マルチステージビルド）
#   docker build -t yuzhu:dev .
#   docker run --rm -p 5432:5432 yuzhu:dev

# ---- build stage ----
FROM rust:1.96-bookworm AS build
# rust-toolchain.toml は channel = "stable" を指定しているが、イメージ同梱の
# ツールチェインをそのまま使う（ビルド中に別の stable をダウンロードしない）。
ENV RUSTUP_TOOLCHAIN=1.96 \
    CARGO_TERM_COLOR=always
WORKDIR /src
COPY impl/rust/ ./
RUN cargo build --release --locked -p yuzhu-server \
 && install -D -m 0755 target/release/yuzhu-server /out/yuzhu-server

# ---- runtime stage ----
FROM debian:bookworm-slim AS runtime
RUN groupadd --system --gid 10001 yuzhu \
 && useradd --system --uid 10001 --gid yuzhu --home-dir /var/lib/yuzhu --create-home --shell /usr/sbin/nologin yuzhu
COPY --from=build /out/yuzhu-server /usr/local/bin/yuzhu-server
USER yuzhu:yuzhu
WORKDIR /var/lib/yuzhu
EXPOSE 5432
ENTRYPOINT ["/usr/local/bin/yuzhu-server", "--listen", "0.0.0.0", "--port", "5432"]
