#!/usr/bin/env bash
# ランナー自身の検証用: PostgreSQL 17 本体の isolation テスト（spec と期待ファイル）を取得し、
# 本物の PostgreSQL に対して実行して、isolationtester と同じ出力になるかを確かめる。
#
#   tests/tools/isolation/validate-pg-suite.sh [--port N] [--host H] [--detection pg|timeout] [テスト名...]
#
# 事前に `createdb isolation_regression` しておくこと（pg_isolation_regress と同じ DB 名を使う。
# application_name などに現れるため）。取得したファイルは $WORK（既定: 一時ディレクトリ）に置く。
# PostgreSQL のマイナーバージョン差で期待ファイルとずれるテスト（REL_17_STABLE の先端で
# 追加・変更されたもの）は失敗することがある。
set -euo pipefail

HOST=127.0.0.1
PORT=55432
DETECTION=pg
while [ $# -gt 0 ]; do
    case "$1" in
        --host) HOST="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --detection) DETECTION="$2"; shift 2 ;;
        -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
        *) break ;;
    esac
done

HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="${WORK:-$(mktemp -d)}"
BASE=https://raw.githubusercontent.com/postgres/postgres/REL_17_STABLE/src/test/isolation
mkdir -p "$WORK/specs" "$WORK/expected"

if [ $# -gt 0 ]; then
    TESTS=("$@")
else
    curl -sfL -o "$WORK/isolation_schedule" "$BASE/isolation_schedule"
    mapfile -t TESTS < <(awk '/^test:/ { for (i = 2; i <= NF; i++) print $i }' "$WORK/isolation_schedule")
fi

for t in "${TESTS[@]}"; do
    echo "specs/$t.spec $BASE/specs/$t.spec"
    echo "expected/$t.out $BASE/expected/$t.out"
    for i in 1 2 3; do echo "expected/${t}_$i.out $BASE/expected/${t}_$i.out"; done
done | (cd "$WORK" && xargs -P 16 -n 2 sh -c 'curl -sfL -o "$0" "$1" || rm -f "$0"')

cargo build --release --quiet --manifest-path "$HERE/Cargo.toml"
SPECS=()
for t in "${TESTS[@]}"; do SPECS+=("$WORK/specs/$t.spec"); done

# pg_isolation_regress は PGDATESTYLE="Postgres, MDY" を設定して isolationtester を起動する。
exec "$HERE/target/release/yuzhu-isolation" \
    --host "$HOST" --port "$PORT" --dbname isolation_regression \
    --blocking-detection "$DETECTION" \
    --set 'datestyle=Postgres, MDY' \
    "${SPECS[@]}"
