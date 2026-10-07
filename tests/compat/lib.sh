#!/usr/bin/env bash
# tests/compat の共通部分（run.sh と pgbench/*.sh が source する）。11-tests-plan.md §3.5。
#
# 呼び出し側が先に TARGET（pg|yuzhu）と PORT を決めて export する。接続先は PGHOST/PGPORT/PGUSER/PGDATABASE。
# サーバの起動と停止は呼び出し側（tests/done-check.sh、手動なら tests/pg.sh / sandbox/pg.sh / tests/yuzhu.sh）の責任。
# 前提: psql / pgbench が 17 系であること（psql が送る SQL は版で変わる）。

COMPAT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$COMPAT_DIR/../.." && pwd)"

compat_env() {
    : "${TARGET:?TARGET が未設定}"
    case "$TARGET" in
        pg) : "${PORT:=55432}" ;;
        yuzhu) : "${PORT:=5432}" ;;
        *) echo "unknown target: $TARGET" >&2; return 2 ;;
    esac
    export PGHOST="${HOST:-127.0.0.1}" PGPORT="$PORT" PGUSER="${PGUSER:-postgres}" PGDATABASE="${PGDATABASE:-postgres}"
    export PGCONNECT_TIMEOUT=10
    # 他の設定（~/.psqlrc、PGOPTIONS）で出力が変わらないようにする
    unset PGOPTIONS
    export PSQLRC=/dev/null
}

check_client_version() {
    local tool v
    for tool in psql "$@"; do
        command -v "$tool" >/dev/null 2>&1 || { echo "$tool が見つからない（17 系のクライアントが必要）" >&2; return 1; }
        v="$("$tool" --version | sed -E 's/.* ([0-9]+)(\.[0-9]+)*.*/\1/')"
        [ "$v" = "17" ] || { echo "$tool は 17 系が必要（実際: $("$tool" --version)）" >&2; return 1; }
    done
}

# psql の出力の正規化: 実行ごとに変わる値（時刻、OID、ファイルのパス）を固定する。
normalize() {
    sed -E \
        -e 's/[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?(\+[0-9]{2}(:[0-9]{2})?)?/<TIMESTAMP>/g' \
        -e 's/^(.*relation|.*OID) "?[0-9]{4,}"?/\1 <OID>/' \
        -e 's#/tmp/[A-Za-z0-9._/-]+#<TMP>#g'
}

# サーバを kill -9 で落として起動し直す（クラッシュリカバリを通す）。
crash_server() {
    case "$TARGET" in
        pg)
            if [ -n "${PG_CRASH_CMD:-}" ]; then
                PG_PORT="$PORT" bash -c "$PG_CRASH_CMD"
            else
                PG_PORT="$PORT" "$REPO_ROOT/tests/pg.sh" crash
            fi
            ;;
        yuzhu) "$REPO_ROOT/tests/yuzhu.sh" crash --port "$PORT" ;;
    esac
}

wait_ready() {
    local _
    for _ in $(seq 1 100); do
        psql -X -Atqc 'select 1' >/dev/null 2>&1 && return 0
        sleep 0.3
    done
    echo "サーバが応答しない" >&2
    return 1
}

q() { psql -X -Atq -v ON_ERROR_STOP=1 -c "$1"; }
