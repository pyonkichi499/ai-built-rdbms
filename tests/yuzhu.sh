#!/usr/bin/env bash
# yuzhu-server をテスト用に起動・停止する補助スクリプト（tests/pg.sh と同じ形）。
#
#   tests/yuzhu.sh start|stop|restart|status|clean [stop のモード] [--port N] [--data DIR] [--shared-buffers SIZE]
#
# - start:   データディレクトリがなければ yuzhu-initdb -U postgres --no-sync で作り、yuzhu-server -D で起動して、
#            待ち受けが始まるまで待つ。
# - stop:    smart(SIGTERM) | fast(SIGINT、既定) | immediate(SIGQUIT)。終了を待つ。データは消さない。
# - restart: fast で止めて、同じデータディレクトリ・同じオプションで起動し直す。
#            --port などを渡すと、そのオプションを上書きして起動する。
# - clean:   止めて、データディレクトリと状態を消す。
# - status:  起動していれば 0、していなければ 1。
#
# 状態（データ、ログ、pid、前回のオプション）は $YUZHU_STATE（既定 /tmp/yuzhu-test）に置く。
# バイナリは $YUZHU_BIN_DIR、なければ ${CARGO_TARGET_DIR:-impl/rust/target}/release から探す。
# 環境変数でも指定できる: YUZHU_PORT（既定 5432）、YUZHU_SHARED_BUFFERS（未指定ならサーバの既定）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
STATE="${YUZHU_STATE:-/tmp/yuzhu-test}"
BIN_DIR="${YUZHU_BIN_DIR:-${CARGO_TARGET_DIR:-$REPO_ROOT/impl/rust/target}/release}"
PORT="${YUZHU_PORT:-5432}"
DATA=""
SHARED_BUFFERS="${YUZHU_SHARED_BUFFERS:-}"
STOP_MODE="fast"

usage() {
    sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

[ $# -ge 1 ] || usage
CMD="$1"; shift
GIVEN_OPTS=0
while [ $# -gt 0 ]; do
    case "$1" in
        --port) PORT="$2"; GIVEN_OPTS=1; shift 2 ;;
        --data) DATA="$2"; GIVEN_OPTS=1; shift 2 ;;
        --shared-buffers) SHARED_BUFFERS="$2"; GIVEN_OPTS=1; shift 2 ;;
        smart|fast|immediate) STOP_MODE="$1"; shift ;;
        -h|--help) usage ;;
        *) echo "unknown option: $1" >&2; usage ;;
    esac
done

PID_FILE="$STATE/server.pid"
LOG_FILE="$STATE/server.log"
OPTS_FILE="$STATE/options"

# 前回のオプションを引き継ぐ（restart で何も渡さなかったとき）。
if [ "$GIVEN_OPTS" -eq 0 ] && [ -f "$OPTS_FILE" ] && [ "$CMD" = restart ]; then
    # shellcheck disable=SC1090
    . "$OPTS_FILE"
fi
DATA="${DATA:-$STATE/data}"

is_running() {
    [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null
}

port_open() {
    (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null
}

do_start() {
    if is_running; then
        echo "yuzhu-server is already running (pid $(cat "$PID_FILE"), port $PORT)"
        return 0
    fi
    for b in yuzhu-server yuzhu-initdb; do
        if [ ! -x "$BIN_DIR/$b" ]; then
            echo "$BIN_DIR/$b not found. build: (cd impl/rust && cargo build --release -p yuzhu-server)" >&2
            return 1
        fi
    done
    mkdir -p "$STATE"
    if [ ! -e "$DATA/global/yuzhu_control" ]; then
        rm -rf "$DATA"
        "$BIN_DIR/yuzhu-initdb" -D "$DATA" -U postgres --no-sync >"$STATE/initdb.log" 2>&1 \
            || { cat "$STATE/initdb.log" >&2; return 1; }
    fi
    {
        printf 'PORT=%q\n' "$PORT"
        printf 'DATA=%q\n' "$DATA"
        printf 'SHARED_BUFFERS=%q\n' "$SHARED_BUFFERS"
    } >"$OPTS_FILE"

    local args=(-D "$DATA" --listen 127.0.0.1 --port "$PORT")
    [ -z "$SHARED_BUFFERS" ] || args+=(--shared-buffers "$SHARED_BUFFERS")
    nohup "$BIN_DIR/yuzhu-server" "${args[@]}" >>"$LOG_FILE" 2>&1 &
    echo $! >"$PID_FILE"

    for _ in $(seq 1 120); do
        if ! is_running; then
            echo "yuzhu-server exited during startup" >&2
            tail -n 30 "$LOG_FILE" >&2 || true
            return 1
        fi
        if port_open; then
            echo "yuzhu-server is ready on 127.0.0.1:$PORT (data $DATA)"
            return 0
        fi
        sleep 0.25
    done
    echo "yuzhu-server did not start listening in time" >&2
    tail -n 30 "$LOG_FILE" >&2 || true
    return 1
}

do_stop() {
    local mode="$1" sig
    if ! is_running; then
        echo "yuzhu-server is not running"
        rm -f "$PID_FILE"
        return 0
    fi
    case "$mode" in
        smart) sig=TERM ;;
        fast) sig=INT ;;
        immediate) sig=QUIT ;;
    esac
    local pid
    pid="$(cat "$PID_FILE")"
    kill -s "$sig" "$pid"
    for _ in $(seq 1 240); do
        if ! kill -0 "$pid" 2>/dev/null; then
            rm -f "$PID_FILE"
            echo "yuzhu-server stopped ($mode)"
            return 0
        fi
        sleep 0.25
    done
    echo "yuzhu-server (pid $pid) did not stop in time after $mode shutdown" >&2
    return 1
}

case "$CMD" in
    start) do_start ;;
    stop) do_stop "$STOP_MODE" ;;
    restart)
        do_stop "$STOP_MODE"
        do_start
        ;;
    status)
        if is_running; then
            echo "running (pid $(cat "$PID_FILE"), port $PORT)"
        else
            echo "not running"
            exit 1
        fi
        ;;
    clean)
        do_stop fast || true
        rm -rf "$STATE"
        echo "removed $STATE"
        ;;
    *) usage ;;
esac
