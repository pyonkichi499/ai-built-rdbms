#!/usr/bin/env bash
# 共有 sqllogictest スイートを実行する。
#
#   tests/run.sh --target pg|yuzhu [--host H] [--port N] [--user U] [--db D] [--restart] [--] [files or dirs...]
#
# - ファイルもディレクトリも指定しなければ tests/slt 以下（m1, m2）の全 .slt を流す。
#   --restart のときは tests/restart 以下の全シナリオを流す。
# - ランナーは sqllogictest-bin 0.29.1（cargo install sqllogictest-bin --locked --version 0.29.1）。
# - エンジンは --engine postgres（Simple Query のみ）。
# - 既定値: user=postgres, db=postgres, host=127.0.0.1, port は pg なら 55432、yuzhu なら 5432。
# - onlyif/skipif 用に --label pg / --label yuzhu を付ける。
# - 環境変数 SLT_BIN でランナーのパスを、SLT_EXTRA_ARGS で追加の引数を渡せる。
#
# --restart: 引数は再起動テストのシナリオのディレクトリ（tests/restart/<シナリオ>、または
#   その親）。シナリオの NN-*.slt をフェーズとして順に流し、フェーズの間でサーバを再起動する
#   （フェーズごとにランナーのプロセスを起動し直す）。
#   - pg:    環境変数 PG_RESTART_CMD（なければ docker があれば tests/pg.sh restart、なければ sandbox/pg.sh restart）
#   - yuzhu: tests/yuzhu.sh restart（fast shutdown）
#   シナリオのディレクトリに yuzhu.args があれば、yuzhu ではシナリオの前にその内容
#   （例: --shared-buffers 1MB）でサーバを再起動し、終わったら既定のオプションに戻す。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET=""
HOST="127.0.0.1"
PORT=""
USER_NAME="postgres"
DB="postgres"
RESTART=0
PATHS=()

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --target) TARGET="$2"; shift 2 ;;
        --host) HOST="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --user) USER_NAME="$2"; shift 2 ;;
        --db) DB="$2"; shift 2 ;;
        --restart) RESTART=1; shift ;;
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

EXTRA=()
if "$SLT" --help 2>&1 | grep -q -- '--shutdown-timeout'; then
    EXTRA+=(--shutdown-timeout 5)
fi
# shellcheck disable=SC2206
EXTRA+=(${SLT_EXTRA_ARGS:-})

run_slt() {
    "$SLT" --engine postgres \
        --host "$HOST" --port "$PORT" --user "$USER_NAME" --db "$DB" \
        --label "$TARGET" "${EXTRA[@]}" "$@"
}

# ---------------------------------------------------------------- 再起動テスト

restart_server() {
    case "$TARGET" in
        pg)
            if [ -n "${PG_RESTART_CMD:-}" ]; then
                PG_PORT="$PORT" bash -c "$PG_RESTART_CMD"
            elif command -v docker >/dev/null 2>&1 && docker container inspect "${PG_CONTAINER:-yuzhu-test-pg}" >/dev/null 2>&1; then
                PG_PORT="$PORT" "$SCRIPT_DIR/pg.sh" restart
            else
                PG_PORT="$PORT" "$SCRIPT_DIR/../sandbox/pg.sh" restart
            fi
            ;;
        yuzhu)
            "$SCRIPT_DIR/yuzhu.sh" restart --port "$PORT" "$@"
            ;;
    esac
}

# シナリオ（NN-*.slt を持つディレクトリ）を流す。失敗したら 1 を返す。
run_scenario() {
    local dir="$1" phases=() f applied_args=0
    while IFS= read -r f; do phases+=("$f"); done < <(find "$dir" -maxdepth 1 -type f -name '*.slt' | LC_ALL=C sort)
    echo "== scenario $dir (${#phases[@]} phases)"

    if [ "$TARGET" = yuzhu ] && [ -f "$dir/yuzhu.args" ]; then
        # shellcheck disable=SC2046
        restart_server $(cat "$dir/yuzhu.args")
        applied_args=1
    fi

    local i rc=0
    for i in "${!phases[@]}"; do
        echo "-- phase $((i + 1))/${#phases[@]}: ${phases[$i]}"
        if ! run_slt "${phases[$i]}"; then
            rc=1
            break
        fi
        if [ "$i" -lt $((${#phases[@]} - 1)) ]; then
            restart_server || { rc=1; break; }
        fi
    done

    if [ "$applied_args" -eq 1 ]; then
        restart_server --shared-buffers "" || rc=1
    fi
    return "$rc"
}

if [ "$RESTART" -eq 1 ]; then
    if [ ${#PATHS[@]} -eq 0 ]; then
        PATHS=("$SCRIPT_DIR/restart")
    fi
    SCENARIOS=()
    for p in "${PATHS[@]}"; do
        [ -d "$p" ] || { echo "no such directory: $p" >&2; exit 1; }
        if [ -n "$(find "$p" -maxdepth 1 -type f -name '*.slt' -print -quit)" ]; then
            SCENARIOS+=("$p")
        else
            while IFS= read -r d; do SCENARIOS+=("$d"); done < <(find "$p" -mindepth 1 -maxdepth 1 -type d | LC_ALL=C sort)
        fi
    done
    [ ${#SCENARIOS[@]} -gt 0 ] || { echo "no scenarios found" >&2; exit 1; }
    echo "running ${#SCENARIOS[@]} restart scenario(s) against $TARGET at $HOST:$PORT (user=$USER_NAME db=$DB)"
    FAILED=()
    for s in "${SCENARIOS[@]}"; do
        run_scenario "$s" || FAILED+=("$s")
    done
    if [ ${#FAILED[@]} -gt 0 ]; then
        echo "FAILED scenarios:" >&2
        printf '  %s\n' "${FAILED[@]}" >&2
        exit 1
    fi
    echo "all restart scenarios passed"
    exit 0
fi

# ---------------------------------------------------------------- 通常のテスト

if [ ${#PATHS[@]} -eq 0 ]; then
    PATHS=("$SCRIPT_DIR/slt")
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

echo "running ${#FILES[@]} file(s) against $TARGET at $HOST:$PORT (user=$USER_NAME db=$DB)"
exec "$SLT" --engine postgres \
    --host "$HOST" --port "$PORT" --user "$USER_NAME" --db "$DB" \
    --label "$TARGET" "${EXTRA[@]}" "${FILES[@]}"
