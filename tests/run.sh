#!/usr/bin/env bash
# 共有 sqllogictest スイートを実行する。
#
#   tests/run.sh --target pg|yuzhu [--host H] [--port N] [--user U] [--db D] [--restart] [--crash] [--] [files or dirs...]
#
# - ファイルもディレクトリも指定しなければ tests/slt 以下（m1〜m4）の全 .slt を流す（パスの C ロケール昇順。m4/z_final が最後）。
#   --restart のときは tests/restart 直下と tests/restart/m4（mode が restart か both）の全シナリオを流す。
# - ランナーは sqllogictest-bin 0.29.1（cargo install sqllogictest-bin --locked --version 0.29.1）。
# - エンジンは --engine postgres（Simple Query のみ）。
# - 既定値: user=postgres, db=postgres, host=127.0.0.1, port は pg なら 55432、yuzhu なら 5432。
# - onlyif/skipif 用に --label pg / --label yuzhu を付ける。
# - 環境変数 SLT_BIN でランナーのパスを、SLT_EXTRA_ARGS で追加の引数を渡せる。
#
# --crash: --restart と同じだが、フェーズの間でサーバを kill -9 で落として起動し直す（クラッシュリカバリを通す）。
#   --crash だけで --restart も有効になる。既定のシナリオは tests/restart/m3 と tests/restart/m4（mode が crash・both・mixed）、
#   --restart だけなら tests/restart 直下（m3 を除く）と tests/restart/m4（mode が restart・both）。
#   明示のパスを渡したときは mode を無視して全部流す（mixed のシナリオはフェーズごとの .mode に従う）。
#   - pg:    環境変数 PG_CRASH_CMD（なければ tests/pg.sh crash。docker なしなら sandbox/pg.sh で起動した PG を kill -9）
#   - yuzhu: tests/yuzhu.sh crash（SIGKILL → 同じデータディレクトリで起動）
#   シナリオのディレクトリの追加ファイル:
#   - yuzhu.only:       あれば pg ではそのシナリオを飛ばす（PG に対応する挙動がないもの。例: ページチェックサムの破損）
#   - NN-<名前>.after.sh: あれば、フェーズ NN のあと、サーバが止まっている間（起動の前）に実行する。
#                       環境変数 YUZHU_DATA にデータディレクトリが入る（yuzhu だけ。データファイルを壊す用）
#   - mode (m4。1 行): restart | crash | both | mixed。なければ both。
#       restart = --restart だけ、crash = --crash だけ、both = どちらでも、mixed = --crash の中で 1 回だけ（フェーズごとの .mode に従う）
#   - NN-<名前>.mode (m4。mixed のシナリオ): フェーズ NN の「後」の停止の方法 restart | crash（なければ restart）
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
CRASH=0
PATHS=()

usage() {
    sed -n '2,38p' "$0" | sed 's/^# \{0,1\}//'
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
        --crash) RESTART=1; CRASH=1; shift ;;
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

# サーバを止めて起動し直す。CRASH=1 なら kill -9 で落とす（クラッシュリカバリを通す）。
# 第 1 引数が --hook <path> なら、yuzhu ではサーバが止まっている間にそのスクリプトを実行する。
restart_server() {
    local hook=""
    if [ "${1:-}" = "--hook" ]; then hook="$2"; shift 2; fi
    case "$TARGET" in
        pg)
            if [ "$CRASH" -eq 1 ]; then
                if [ -n "${PG_CRASH_CMD:-}" ]; then
                    PG_PORT="$PORT" bash -c "$PG_CRASH_CMD"
                else
                    PG_PORT="$PORT" "$SCRIPT_DIR/pg.sh" crash
                fi
            elif [ -n "${PG_RESTART_CMD:-}" ]; then
                PG_PORT="$PORT" bash -c "$PG_RESTART_CMD"
            elif command -v docker >/dev/null 2>&1 && docker container inspect "${PG_CONTAINER:-yuzhu-test-pg}" >/dev/null 2>&1; then
                PG_PORT="$PORT" "$SCRIPT_DIR/pg.sh" restart
            else
                PG_PORT="$PORT" "$SCRIPT_DIR/../sandbox/pg.sh" restart
            fi
            ;;
        yuzhu)
            local sub=restart
            [ "$CRASH" -eq 1 ] && sub=crash
            YUZHU_BETWEEN_HOOK="$hook" "$SCRIPT_DIR/yuzhu.sh" "$sub" --port "$PORT" "$@"
            ;;
    esac
}

# 初期のオプション（yuzhu.args）の適用と復元は、クラッシュではなく通常の再起動で行う。
restart_server_graceful() {
    local saved="$CRASH" rc=0
    CRASH=0
    restart_server "$@" || rc=$?
    CRASH="$saved"
    return "$rc"
}

# シナリオの mode（restart | crash | both | mixed。なければ both）を出力する。不正な値なら 1。
scenario_mode() {
    local dir="$1" m=both
    if [ -f "$dir/mode" ]; then m="$(tr -d '[:space:]' < "$dir/mode")"; fi
    case "$m" in
        restart|crash|both|mixed) echo "$m" ;;
        *) echo "invalid mode '$m' in $dir/mode (restart|crash|both|mixed)" >&2; return 1 ;;
    esac
}

# フェーズ NN-<名前>.slt の「後」の停止の方法（NN-<名前>.mode。restart | crash。なければ restart）。
phase_mode() {
    local f="${1%.slt}.mode" m=restart
    if [ -f "$f" ]; then m="$(tr -d '[:space:]' < "$f")"; fi
    case "$m" in
        restart|crash) echo "$m" ;;
        *) echo "invalid phase mode '$m' in $f (restart|crash)" >&2; return 1 ;;
    esac
}

# シナリオ（NN-*.slt を持つディレクトリ）を流す。失敗したら 1 を返す。
run_scenario() {
    local dir="$1" phases=() f applied_args=0 m
    m="$(scenario_mode "$dir")" || return 2
    while IFS= read -r f; do phases+=("$f"); done < <(find "$dir" -maxdepth 1 -type f -name '*.slt' | LC_ALL=C sort)
    echo "== scenario $dir (${#phases[@]} phases, mode=$m)"

    if [ "$TARGET" = pg ] && [ -f "$dir/yuzhu.only" ]; then
        echo "-- skipped on pg (yuzhu.only)"
        return 0
    fi

    if [ "$TARGET" = yuzhu ] && [ -f "$dir/yuzhu.args" ]; then
        # shellcheck disable=SC2046
        restart_server_graceful $(cat "$dir/yuzhu.args")
        applied_args=1
    fi

    local i rc=0 hook saved_crash="$CRASH"
    for i in "${!phases[@]}"; do
        echo "-- phase $((i + 1))/${#phases[@]}: ${phases[$i]}"
        if ! run_slt "${phases[$i]}"; then
            rc=1
            break
        fi
        if [ "$i" -lt $((${#phases[@]} - 1)) ]; then
            if [ "$m" = mixed ]; then
                # 停止の方法はフェーズごとの .mode が決める（--crash の中で流す前提）
                case "$(phase_mode "${phases[$i]}")" in
                    crash) CRASH=1 ;;
                    *) CRASH=0 ;;
                esac
            fi
            hook="${phases[$i]%.slt}.after.sh"
            if [ -f "$hook" ] && [ "$TARGET" = yuzhu ]; then
                restart_server --hook "$hook" || { rc=1; CRASH="$saved_crash"; break; }
            else
                restart_server || { rc=1; CRASH="$saved_crash"; break; }
            fi
            CRASH="$saved_crash"
        fi
    done

    if [ "$applied_args" -eq 1 ]; then
        restart_server_graceful --shared-buffers "" || rc=1
    fi
    return "$rc"
}

# 再起動テストのシナリオ（*.slt を直接持つディレクトリ）を $1 以下から集める。
# $2 が flat なら $1 の直下だけ（m3 などの入れ子は含めない）。
collect_scenarios() {
    local root="$1" mode="${2:-recursive}" d
    if [ -n "$(find "$root" -maxdepth 1 -type f -name '*.slt' -print -quit)" ]; then
        SCENARIOS+=("$root")
        return
    fi
    if [ "$mode" = flat ]; then
        while IFS= read -r d; do
            if [ -n "$(find "$d" -maxdepth 1 -type f -name '*.slt' -print -quit)" ]; then SCENARIOS+=("$d"); fi
        done < <(find "$root" -mindepth 1 -maxdepth 1 -type d | LC_ALL=C sort)
    else
        while IFS= read -r d; do
            if [ -n "$(find "$d" -maxdepth 1 -type f -name '*.slt' -print -quit)" ]; then SCENARIOS+=("$d"); fi
        done < <(find "$root" -mindepth 1 -type d | LC_ALL=C sort)
    fi
}

# tests/restart/m4 のシナリオのうち、mode が $1（空白区切りの許可する値）に含まれるものを集める。
collect_m4_scenarios() {
    local allowed=" $1 " d m
    [ -d "$SCRIPT_DIR/restart/m4" ] || return 0
    while IFS= read -r d; do
        [ -n "$(find "$d" -maxdepth 1 -type f -name '*.slt' -print -quit)" ] || continue
        m="$(scenario_mode "$d")" || exit 2
        case "$allowed" in *" $m "*) SCENARIOS+=("$d") ;; esac
    done < <(find "$SCRIPT_DIR/restart/m4" -mindepth 1 -maxdepth 1 -type d | LC_ALL=C sort)
}

if [ "$RESTART" -eq 1 ]; then
    SCENARIOS=()
    if [ ${#PATHS[@]} -eq 0 ]; then
        if [ "$CRASH" -eq 1 ]; then
            collect_scenarios "$SCRIPT_DIR/restart/m3"
            collect_m4_scenarios "crash both mixed"
        else
            collect_scenarios "$SCRIPT_DIR/restart" flat
            collect_m4_scenarios "restart both"
        fi
    else
        for p in "${PATHS[@]}"; do
            [ -d "$p" ] || { echo "no such directory: $p" >&2; exit 1; }
            collect_scenarios "$p"
        done
    fi
    for s in "${SCENARIOS[@]}"; do scenario_mode "$s" >/dev/null || exit 2; done
    [ ${#SCENARIOS[@]} -gt 0 ] || { echo "no scenarios found" >&2; exit 1; }
    echo "running ${#SCENARIOS[@]} restart scenario(s) against $TARGET at $HOST:$PORT (user=$USER_NAME db=$DB, crash=$CRASH)"
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
