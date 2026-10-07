#!/usr/bin/env bash
# pgbench -c 4 -T 30 -M simple（--quick は -T 5）と 10 §7.3 の 3〜4。
set -uo pipefail
D="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
. "$D/../lib.sh"
compat_env || exit 2

T=30; [ "${QUICK:-0}" = 1 ] && T=5
pgbench -i -s 1 -q >/dev/null 2>&1 || { echo "pgbench -i に失敗" >&2; exit 1; }
out="$(pgbench -c 4 -T "$T" -M simple 2>&1)" || { echo "$out"; exit 1; }
echo "$out" | grep -E 'number of (transactions actually processed|failed transactions)'
echo "$out" | grep -q 'number of failed transactions: 0 (0.000%)' || { echo "失敗したトランザクションがある" >&2; exit 1; }
n="$(echo "$out" | sed -n 's/^number of transactions actually processed: //p')"
vals="$(psql -X -Atq -f "$D/invariants.sql")"
IFS='|' read -r a t b h <<<"$vals"
[ -n "$a" ] && [ "$a" = "$t" ] && [ "$t" = "$b" ] && [ "$b" = "$h" ] || { echo "不変条件が崩れた: $vals" >&2; exit 1; }
cnt="$(q 'select count(*) from pgbench_history')"
[ "$cnt" = "$n" ] || { echo "pgbench_history の行数 $cnt と処理数 $n が違う" >&2; exit 1; }
echo "不変条件 OK: $vals（$n 件）"
