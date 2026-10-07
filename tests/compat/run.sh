#!/usr/bin/env bash
# 周辺ツール（psql・COPY・pgbench）の互換テスト（11-tests-plan.md §3.5、10-explain-copy-compat.md §7.4・§8.3）。
#
#   tests/compat/run.sh --target pg|yuzhu [--host H] [--port N] [--update] [--quick] [suite ...]
#
# suite = psql | copy | pgbench（省略は全部）。サーバは呼び出し側が新しい状態で起動しておく（他のテストの残りが \dt に混ざる）。
# - psql / copy: tests/compat/<suite>/*.sql を psql -X -q に流し、正規化した出力を <suite>/expected/<名前>.out と比べる。
#   --update --target pg: expected/ を PostgreSQL の出力で作り直す（レビューしてコミットする）。
# - pgbench: init.sh（-i -s 1 と -i -I dtGvp -s 1）、run.sh（-c 4 -T 30 -M simple。--quick は -T 5）、crash.sh（--quick は 2 ラウンド）。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET=""; PORT=""; HOST="127.0.0.1"; UPDATE=0; QUICK=0; SUITES=()
while [ $# -gt 0 ]; do
    case "$1" in
        --target) TARGET="$2"; shift 2 ;;
        --host) HOST="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --update) UPDATE=1; shift ;;
        --quick) QUICK=1; shift ;;
        -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        psql|copy|pgbench) SUITES+=("$1"); shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$TARGET" ] || { echo "--target pg|yuzhu が必要" >&2; exit 2; }
[ ${#SUITES[@]} -gt 0 ] || SUITES=(psql copy pgbench)
if [ "$UPDATE" -eq 1 ] && [ "$TARGET" != "pg" ]; then echo "--update は --target pg だけ" >&2; exit 2; fi

# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh"
compat_env || exit 2
export TARGET PORT HOST QUICK
check_client_version pgbench || exit 2
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

for s in "${SUITES[@]}"; do
    echo "== compat/$s ($TARGET $HOST:$PORT)"
    case "$s" in
        psql|copy) run_sql_suite "$s" ;;
        pgbench)
            [ "$UPDATE" -eq 1 ] && continue
            for sc in init run crash; do
                if bash "$SCRIPT_DIR/pgbench/$sc.sh"; then echo "ok   pgbench/$sc"; else echo "FAIL pgbench/$sc"; FAIL=1; fi
            done
            ;;
    esac
done
exit "$FAIL"
