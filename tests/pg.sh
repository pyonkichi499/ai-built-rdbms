#!/usr/bin/env bash
# 本物の PostgreSQL 17（C ロケール、trust 認証）を docker で起動・停止する補助スクリプト。
#
#   tests/pg.sh start|stop|restart|crash|status [--port N] [--name NAME] [--image IMAGE]
#
# restart はデータを残したまま再起動する（smart shutdown）。
# crash はデータを残したまま kill -9 で落として起動し直す（クラッシュリカバリを通す）。
#   docker のコンテナがあれば docker kill -s KILL + docker start。なければ（claude-sandbox のコンテナ内）
#   sandbox/pg.sh が起動した PostgreSQL（$PG_DATA、既定 /tmp/yuzhu-pg17）のプロセスを kill -9 して pg_ctl start する。
# 環境変数でも指定できる: PG_PORT（既定 55432）、PG_CONTAINER（既定 yuzhu-test-pg）、
# PG_IMAGE（既定 postgres:17）。
set -euo pipefail

PORT="${PG_PORT:-55432}"
NAME="${PG_CONTAINER:-yuzhu-test-pg}"
IMAGE="${PG_IMAGE:-postgres:17}"
LOCAL_DATA="${PG_DATA:-/tmp/yuzhu-pg17}"

usage() {
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

[ $# -ge 1 ] || usage
CMD="$1"; shift
while [ $# -gt 0 ]; do
    case "$1" in
        --port) PORT="$2"; shift 2 ;;
        --name) NAME="$2"; shift 2 ;;
        --image) IMAGE="$2"; shift 2 ;;
        -h|--help) usage ;;
        *) echo "unknown option: $1" >&2; usage ;;
    esac
done

exists() { docker container inspect "$NAME" >/dev/null 2>&1; }
running() { [ "$(docker container inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" = "true" ]; }

wait_ready() {
    # pg_isready はコンテナ内の UNIX ソケットで判定する。初期化中の一時サーバでも
    # 応答してしまうので、TCP（-h 127.0.0.1）で確認し、さらに実際のクエリを投げて確かめる。
    for _ in $(seq 1 120); do
        if docker exec "$NAME" pg_isready -q -h 127.0.0.1 -U postgres >/dev/null 2>&1 &&
           docker exec "$NAME" psql -h 127.0.0.1 -U postgres -d postgres -Atqc 'SELECT 1' >/dev/null 2>&1; then
            echo "PostgreSQL is ready on port $PORT (container $NAME)"
            return 0
        fi
        sleep 0.5
    done
    echo "PostgreSQL did not become ready in time" >&2
    docker logs --tail 50 "$NAME" >&2 || true
    return 1
}

# --- docker なし（claude-sandbox のコンテナ内）で sandbox/pg.sh が起動した PostgreSQL を kill -9 して起動し直す。
local_crash() {
    local pidfile="$LOCAL_DATA/postmaster.pid"
    [ -f "$pidfile" ] || { echo "PostgreSQL is not running (no $pidfile)" >&2; return 1; }
    local postmaster pids
    postmaster="$(head -n 1 "$pidfile")"
    kill -0 "$postmaster" 2>/dev/null || { echo "postmaster $postmaster is not alive" >&2; return 1; }
    # postmaster と、その子孫（backend、checkpointer、walwriter など）をまとめて SIGKILL する。
    pids="$postmaster $(pgrep -P "$postmaster" | tr '\n' ' ')"
    # shellcheck disable=SC2086
    kill -s KILL $pids 2>/dev/null || true
    for _ in $(seq 1 100); do
        # shellcheck disable=SC2086
        if ! kill -0 $pids 2>/dev/null; then break; fi
        sleep 0.1
    done
    local lib passwd="" group=""
    local env_cmd=()
    if ! getent passwd "$(id -u)" >/dev/null 2>&1; then
        lib="$(find /usr/lib -name libnss_wrapper.so 2>/dev/null | head -n 1)"
        [ -n "$lib" ] || { echo "libnss_wrapper.so not found" >&2; return 1; }
        passwd="$(mktemp)"; group="$(mktemp)"
        echo "postgres:x:$(id -u):$(id -g):PostgreSQL:$LOCAL_DATA:/bin/false" > "$passwd"
        echo "postgres:x:$(id -g):" > "$group"
        env_cmd=(env LD_PRELOAD="$lib" NSS_WRAPPER_PASSWD="$passwd" NSS_WRAPPER_GROUP="$group")
    fi
    local rc=0
    "${env_cmd[@]}" pg_ctl -D "$LOCAL_DATA" -l "$LOCAL_DATA/server.log" -o "-p $PORT" -w -t 120 start >/dev/null || rc=$?
    rm -f $passwd $group
    [ "$rc" -eq 0 ] || { tail -n 50 "$LOCAL_DATA/server.log" >&2 || true; return "$rc"; }
    psql -h 127.0.0.1 -p "$PORT" -U postgres -d postgres -Atqc 'SELECT 1' >/dev/null
    echo "PostgreSQL crashed (SIGKILL) and restarted on 127.0.0.1:$PORT (data $LOCAL_DATA)"
}

case "$CMD" in
    start)
        if running; then
            echo "container $NAME is already running"
        else
            if exists; then docker rm -f "$NAME" >/dev/null; fi
            docker run -d --name "$NAME" \
                -e POSTGRES_HOST_AUTH_METHOD=trust \
                -e POSTGRES_INITDB_ARGS='--locale=C --encoding=UTF8' \
                -p "127.0.0.1:${PORT}:5432" \
                "$IMAGE" >/dev/null
        fi
        wait_ready
        ;;
    restart)
        # データを残したまま PostgreSQL を再起動する（docker restart。コンテナ内で smart shutdown して起動し直す）。
        if ! exists; then echo "container $NAME does not exist" >&2; exit 1; fi
        docker restart -t 60 "$NAME" >/dev/null
        wait_ready
        ;;
    crash)
        # kill -9 で落としてから起動し直す。起動時にクラッシュリカバリが走る。
        if command -v docker >/dev/null 2>&1 && exists; then
            docker kill -s KILL "$NAME" >/dev/null
            docker start "$NAME" >/dev/null
            wait_ready
        else
            local_crash
        fi
        ;;
    stop)
        if exists; then
            docker rm -f "$NAME" >/dev/null
            echo "container $NAME removed"
        else
            echo "container $NAME does not exist"
        fi
        ;;
    status)
        if running; then
            echo "running ($NAME, port $PORT)"
        else
            echo "not running ($NAME)"
            exit 1
        fi
        ;;
    *) usage ;;
esac
