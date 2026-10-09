#!/usr/bin/env bash
# 周辺ツール（psql・COPY・pgbench）の互換テスト（11-tests-plan.md §3.5、10-explain-copy-compat.md §7.4・§8.3）。
#
#   tests/compat/run.sh --target pg|yuzhu [--host H] [--port N] [--update] [--quick]
#                       [--suite psql|copy|pgbench|drivers|auth ...] [--driver NAME] [--auth trust|scram] [suite ...]
#
# suite = psql | copy | pgbench | drivers | auth（省略は psql copy pgbench。drivers はネットワークが要るので既定に入れない）。
# - drivers: tests/compat/drivers/<名前>/run.sh（--driver で選ぶ。省略は存在する全部）が出す `RESULT <id> <ok|FAIL|KD> <詳細>` を
#   「ドライバ・シナリオ ID・結果」の表にする。失敗が 1 つでもあれば終了コード 1。--auth scram なら PGPASSWORD の既定は yuzhu-test-pw。
#
# サーバは呼び出し側が新しい状態で起動しておく（他のテストの残りが \dt に混ざる）。
# - psql / copy: tests/compat/<suite>/*.sql を psql -X -q に流し、正規化した出力を <suite>/expected/<名前>.out と比べる。
#   --update --target pg: expected/ を PostgreSQL の出力で作り直す（レビューしてコミットする）。
# - pgbench: init.sh（-i -s 1 と -i -I dtGvp -s 1）、run.sh（-c 4 -T 30 -M simple。--quick は -T 5）、crash.sh（--quick は 2 ラウンド）。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET=""; PORT=""; HOST="127.0.0.1"; UPDATE=0; QUICK=0; SUITES=(); DRIVERS=(); AUTH_MODE="trust"
while [ $# -gt 0 ]; do
    case "$1" in
        --target) TARGET="$2"; shift 2 ;;
        --host) HOST="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --update) UPDATE=1; shift ;;
        --quick) QUICK=1; shift ;;
        --suite) IFS=, read -r -a _s <<<"$2"; SUITES+=("${_s[@]}"); shift 2 ;;
        --driver) IFS=, read -r -a _d <<<"$2"; DRIVERS+=("${_d[@]}"); shift 2 ;;
        --auth) AUTH_MODE="$2"; shift 2 ;;
        -h|--help) sed -n "2,13p" "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        psql|copy|pgbench|drivers|auth) SUITES+=("$1"); shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$TARGET" ] || { echo "--target pg|yuzhu が必要" >&2; exit 2; }
[ ${#SUITES[@]} -gt 0 ] || SUITES=(psql copy pgbench)
if [ "$UPDATE" -eq 1 ] && [ "$TARGET" != "pg" ]; then echo "--update は --target pg だけ" >&2; exit 2; fi

# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh"
compat_env || exit 2
[ "$AUTH_MODE" = scram ] && export PGPASSWORD="${PGPASSWORD:-yuzhu-test-pw}"
export TARGET PORT HOST QUICK
case " ${SUITES[*]} " in *" pgbench "*) check_client_version pgbench || exit 2 ;; *) check_client_version || exit 2 ;; esac
wait_ready || exit 1

FAIL=0
run_sql_suite() {
    local suite="$1" f name out
    mkdir -p "$SCRIPT_DIR/$suite/expected"
    for f in "$SCRIPT_DIR/$suite"/*.sql; do
        [ -e "$f" ] || continue
        name="$(basename "$f" .sql)"
        out="$(psql -X -q -f - <"$f" 2>&1 | normalize)"
        [ "$name" = l ] && out="$(printf '%s\n' "$out" | grep '|' | cut -d'|' -f1,2)"
        if [ "$UPDATE" -eq 1 ]; then
            printf '%s\n' "$out" >"$SCRIPT_DIR/$suite/expected/$name.out"
            echo "updated $suite/$name"
        elif diff -u "$SCRIPT_DIR/$suite/expected/$name.out" <(printf '%s\n' "$out") >/tmp/compat-diff.$$ 2>&1; then
            echo "ok   $suite/$name"
        else
            echo "FAIL $suite/$name"; sed -n '1,60p' /tmp/compat-diff.$$; FAIL=1
        fi
        rm -f /tmp/compat-diff.$$
    done
}

DRIVER_ROWS=()
DRIVER_NG=0

run_drivers() {
    local d dir out err rc line id st detail found
    [ "$UPDATE" -eq 1 ] && return 0
    case "$AUTH_MODE" in trust|scram) ;; *) echo "--auth は trust|scram" >&2; FAIL=1; return 0 ;; esac
    export TARGET AUTH="$AUTH_MODE" PGPORT="$PORT" PGHOST="$HOST"
    [ "$AUTH_MODE" = scram ] && export PGPASSWORD="${PGPASSWORD:-yuzhu-test-pw}"
    if [ ${#DRIVERS[@]} -eq 0 ]; then
        for dir in "$SCRIPT_DIR"/drivers/*/; do [ -x "$dir/run.sh" ] && DRIVERS+=("$(basename "$dir")"); done
    fi
    for d in "${DRIVERS[@]}"; do
        dir="$SCRIPT_DIR/drivers/$d"
        if [ ! -x "$dir/run.sh" ]; then DRIVER_ROWS+=("$d|-|FAIL|ドライバが存在しない"); FAIL=1; continue; fi
        err="$(mktemp)"
        out="$(bash "$dir/run.sh" 2>"$err")"; rc=$?
        found=0
        while IFS= read -r line; do
            case "$line" in RESULT\ *) ;; *) continue ;; esac
            read -r _ id st detail <<<"$line"
            found=1
            DRIVER_ROWS+=("$d|$id|$st|$detail")
            [ "$st" = FAIL ] && FAIL=1
        done <<<"$out"
        if [ "$found" -eq 0 ] || { [ "$rc" -ne 0 ] && ! grep -q "^RESULT .* FAIL" <<<"$out"; }; then
            DRIVER_ROWS+=("$d|(実行)|FAIL|終了コード $rc: $(tail -n 5 "$err" | tr '\n' ' ')")
            FAIL=1
        fi
        rm -f "$err"
    done
    print_driver_table
}

print_driver_table() {
    local row ok=0 ng=0 kd=0
    printf '%-14s %-11s %-5s %s\n' ドライバ シナリオID 結果 詳細
    for row in "${DRIVER_ROWS[@]}"; do
        IFS="|" read -r d id st detail <<<"$row"
        printf '%-14s %-11s %-5s %s\n' "$d" "$id" "$st" "$detail"
        case "$st" in ok) ok=$((ok+1)) ;; KD) kd=$((kd+1)) ;; *) ng=$((ng+1)) ;; esac
    done
    echo "成功 $ok / 失敗 $ng / KD $kd"
}

for s in "${SUITES[@]}"; do
    echo "== compat/$s ($TARGET $HOST:$PORT)"
    case "$s" in
        psql|copy) run_sql_suite "$s" ;;
        drivers) run_drivers ;;
        auth)
            if [ -x "$SCRIPT_DIR/auth/run.sh" ]; then bash "$SCRIPT_DIR/auth/run.sh" || FAIL=1; else echo "FAIL auth（tests/compat/auth/run.sh が未実装）"; FAIL=1; fi
            ;;
        pgbench)
            [ "$UPDATE" -eq 1 ] && continue
            for sc in init run crash; do
                if bash "$SCRIPT_DIR/pgbench/$sc.sh"; then echo "ok   pgbench/$sc"; else echo "FAIL pgbench/$sc"; FAIL=1; fi
            done
            ;;
    esac
done
exit "$FAIL"
