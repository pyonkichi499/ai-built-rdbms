#!/usr/bin/env bash
# psql 17 の Extended Query・FK の \d・\du・\l・\c・\password（10 §6.10.2、§6.10.4 A6）。
#
#   tests/compat/psql/extended/run.sh [--update]      （TARGET / PORT / HOST は compat/run.sh が export。単独でも実行できる）
#
# - *.sql を psql -X -q に流し、正規化した出力を expected/<名前>.out と比べる。--update は --target pg だけ（PostgreSQL の出力で作り直す）。
# - named.sql（\parse / \bind_named / \close_stmt）は psql 18 以降だけ流す。psql 17 にはこの 3 つが無い（U-8。\bind だけ）。
# - password.sh は \password を stdin の入力で流し、保存された値が SCRAM の秘密情報（反復回数 4096）であることを確かめる。
# 出力の `RESULT <id> <ok|FAIL|SKIP> <詳細>` の行と終了コード（失敗があれば 1）を返す。
set -uo pipefail
D="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../lib.sh
. "$D/../../lib.sh"
: "${TARGET:=pg}"
export TARGET
compat_env || exit 2

UPDATE="${UPDATE:-0}"
[ "${1:-}" = "--update" ] && UPDATE=1
if [ "$UPDATE" = 1 ] && [ "$TARGET" != pg ]; then echo "--update は --target pg だけ" >&2; exit 2; fi
wait_ready || exit 1

mkdir -p "$D/expected"
FAIL=0
psql_major="$(psql --version | sed -E 's/^[^0-9]*([0-9]+).*/\1/')"

# 接続エラーの文言に入るポートとホストを固定する。
norm() { normalize | sed -E "s/port [0-9]+/port <PORT>/g; s/\"127\.0\.0\.1\"/\"<HOST>\"/g; s/\"${PGHOST}\"/\"<HOST>\"/g"; }

for f in "$D"/*.sql; do
    name="$(basename "$f" .sql)"
    if [ "$name" = named ] && [ "$psql_major" -lt 18 ]; then
        echo "RESULT psql/$name SKIP psql $psql_major には \\parse / \\bind_named / \\close_stmt が無い（18 以降。U-8）"
        continue
    fi
    out="$(psql -X -q -f - <"$f" 2>&1 | norm)"
    [ "$name" = l ] && out="$(printf '%s\n' "$out" | grep '|' | cut -d'|' -f1,2)"
    if [ "$UPDATE" = 1 ]; then
        printf '%s\n' "$out" >"$D/expected/$name.out"
        echo "updated psql/extended/$name"
    elif diff -u "$D/expected/$name.out" <(printf '%s\n' "$out") >"/tmp/compat-ext-diff.$$" 2>&1; then
        echo "RESULT psql/$name ok"
    else
        echo "RESULT psql/$name FAIL 期待との差分あり"; sed -n '1,60p' "/tmp/compat-ext-diff.$$"; FAIL=1
    fi
    rm -f "/tmp/compat-ext-diff.$$"
done

if [ "$UPDATE" != 1 ]; then
    if bash "$D/password.sh"; then echo "RESULT psql/password ok"; else echo "RESULT psql/password FAIL"; FAIL=1; fi
fi
exit "$FAIL"
