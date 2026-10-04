#!/usr/bin/env bash
# claude-sandbox のコンテナ内で、検証用の PostgreSQL 17（C ロケール、trust 認証）を起動・停止する。
# コンテナ内では docker が使えないので、tests/pg.sh の代わりにこちらを使う。
#
#   sandbox/pg.sh start|stop|status [--port N]
#
# 環境変数でも指定できる: PG_PORT（既定 55432）、PG_DATA（既定 /tmp/yuzhu-pg17）。
# tests/pg.sh と同じく、stop でデータを消す（start のたびに initdb し直す）。
# TCP（127.0.0.1）のみで待ち受け、UNIX ソケットは作らない。
set -euo pipefail

PORT="${PG_PORT:-55432}"
DATA="${PG_DATA:-/tmp/yuzhu-pg17}"

usage() {
    sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

[ $# -ge 1 ] || usage
CMD="$1"; shift
while [ $# -gt 0 ]; do
    case "$1" in
        --port) PORT="$2"; shift 2 ;;
        -h|--help) usage ;;
        *) echo "unknown option: $1" >&2; usage ;;
    esac
done

command -v pg_ctl >/dev/null 2>&1 || { echo "pg_ctl not found (claude-sandbox の yuzhu-sandbox イメージで実行すること)" >&2; exit 1; }

# コンテナはホストの UID で動き、/etc/passwd に載っていない。initdb / postgres は
# ユーザー名を引けないと失敗するので、公式 postgres イメージと同じく nss_wrapper で補う。
with_user() {
    if getent passwd "$(id -u)" >/dev/null 2>&1; then
        "$@"
        return
    fi
    local lib
    lib="$(find /usr/lib -name libnss_wrapper.so 2>/dev/null | head -n 1)"
    [ -n "$lib" ] || { echo "libnss_wrapper.so not found" >&2; exit 1; }
    local passwd group
    passwd="$(mktemp)"; group="$(mktemp)"
    echo "postgres:x:$(id -u):$(id -g):PostgreSQL:$DATA:/bin/false" > "$passwd"
    echo "postgres:x:$(id -g):" > "$group"
    local rc=0
    LD_PRELOAD="$lib" NSS_WRAPPER_PASSWD="$passwd" NSS_WRAPPER_GROUP="$group" "$@" || rc=$?
    rm -f "$passwd" "$group"
    return "$rc"
}

running() { with_user pg_ctl -D "$DATA" status >/dev/null 2>&1; }

case "$CMD" in
    start)
        if running; then
            echo "PostgreSQL is already running (data $DATA)"
            exit 0
        fi
        rm -rf "$DATA"
        with_user initdb -D "$DATA" -U postgres --auth=trust --locale=C --encoding=UTF8 >/dev/null
        cat >> "$DATA/postgresql.conf" <<EOF
listen_addresses = '127.0.0.1'
unix_socket_directories = ''
EOF
        with_user pg_ctl -D "$DATA" -l "$DATA/server.log" -o "-p $PORT" -w -t 60 start >/dev/null \
            || { tail -n 50 "$DATA/server.log" >&2 || true; exit 1; }
        psql -h 127.0.0.1 -p "$PORT" -U postgres -d postgres -Atqc 'SELECT 1' >/dev/null
        echo "PostgreSQL is ready on 127.0.0.1:$PORT (data $DATA)"
        ;;
    stop)
        if running; then with_user pg_ctl -D "$DATA" -m fast -w stop >/dev/null; fi
        rm -rf "$DATA"
        echo "stopped"
        ;;
    status)
        if running; then echo "running (data $DATA)"; else echo "not running"; exit 1; fi
        ;;
    *) usage ;;
esac
