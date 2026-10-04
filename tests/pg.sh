#!/usr/bin/env bash
# 本物の PostgreSQL 17（C ロケール、trust 認証）を docker で起動・停止する補助スクリプト。
#
#   tests/pg.sh start|stop|status [--port N] [--name NAME] [--image IMAGE]
#
# 環境変数でも指定できる: PG_PORT（既定 55432）、PG_CONTAINER（既定 yuzhu-test-pg）、
# PG_IMAGE（既定 postgres:17）。
set -euo pipefail

PORT="${PG_PORT:-55432}"
NAME="${PG_CONTAINER:-yuzhu-test-pg}"
IMAGE="${PG_IMAGE:-postgres:17}"

usage() {
    sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'
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
