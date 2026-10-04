#!/usr/bin/env bash
# 共有 sqllogictest スイートを実行する。
#
#   tests/run.sh --target pg|yuzhu [--host H] [--port N] [--user U] [--db D] [--] [files or dirs...]
#
# - ファイルもディレクトリも指定しなければ tests/slt/m1 以下の全 .slt を流す。
# - ランナーは sqllogictest-bin 0.29.1（cargo install sqllogictest-bin --locked --version 0.29.1）。
# - エンジンは --engine postgres（Simple Query のみ）。
# - 既定値: user=postgres, db=postgres, host=127.0.0.1, port は pg なら 55432、yuzhu なら 5432。
# - onlyif/skipif 用に --label pg / --label yuzhu を付ける。
# - 環境変数 SLT_BIN でランナーのパスを、SLT_EXTRA_ARGS で追加の引数を渡せる。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET=""
HOST="127.0.0.1"
PORT=""
USER_NAME="postgres"
DB="postgres"
PATHS=()

usage() {
    sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --target) TARGET="$2"; shift 2 ;;
        --host) HOST="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --user) USER_NAME="$2"; shift 2 ;;
        --db) DB="$2"; shift 2 ;;
        -h|--help) usage ;;
        --) shift; PATHS+=("$@"); break ;;
        -*) echo "unknown option: $1" >&2; usage ;;
        *) PATHS+=("$1"); shift ;;
    esac
done

case "$TARGET" in
    pg) : "${PORT:=55432}" ;;
    yuzhu) : "${PORT:=5432}" ;;
    *) echo "--target pg|yuzhu is required" >&2; usage ;;
esac

SLT="${SLT_BIN:-sqllogictest}"
if ! command -v "$SLT" >/dev/null 2>&1; then
    echo "sqllogictest not found. install: cargo install sqllogictest-bin --locked --version 0.29.1" >&2
    exit 1
fi

if [ ${#PATHS[@]} -eq 0 ]; then
    PATHS=("$SCRIPT_DIR/slt/m1")
fi

FILES=()
for p in "${PATHS[@]}"; do
    if [ -d "$p" ]; then
        while IFS= read -r f; do FILES+=("$f"); done < <(find "$p" -type f -name '*.slt' | LC_ALL=C sort)
    elif [ -f "$p" ]; then
        FILES+=("$p")
    else
        echo "no such file or directory: $p" >&2
        exit 1
    fi
done
if [ ${#FILES[@]} -eq 0 ]; then
    echo "no .slt files found" >&2
    exit 1
fi

EXTRA=()
if "$SLT" --help 2>&1 | grep -q -- '--shutdown-timeout'; then
    EXTRA+=(--shutdown-timeout 5)
fi
# shellcheck disable=SC2206
EXTRA+=(${SLT_EXTRA_ARGS:-})

echo "running ${#FILES[@]} file(s) against $TARGET at $HOST:$PORT (user=$USER_NAME db=$DB)"
exec "$SLT" --engine postgres \
    --host "$HOST" --port "$PORT" --user "$USER_NAME" --db "$DB" \
    --label "$TARGET" "${EXTRA[@]}" "${FILES[@]}"
