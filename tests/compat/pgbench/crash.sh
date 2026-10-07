#!/usr/bin/env bash
# pgbench の途中で kill -9 → 再起動 → 原子性・主キー・件数の検査（11 §3.5.3、D11-15）。--quick は 2 ラウンド。
set -uo pipefail
D="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
. "$D/../lib.sh"
compat_env || exit 2

ROUNDS=5; [ "${QUICK:-0}" = 1 ] && ROUNDS=2
pgbench -i -s 1 -q >/dev/null 2>&1 || { echo "pgbench -i に失敗" >&2; exit 1; }
for r in $(seq 1 "$ROUNDS"); do
    pgbench -n -c 4 -T 5 -M simple >/dev/null 2>&1 &
    pb=$!
    sleep $((1 + RANDOM % 3))
    crash_server >/dev/null 2>&1 || { echo "再起動に失敗" >&2; kill "$pb" 2>/dev/null; exit 1; }
    wait "$pb" 2>/dev/null || true
    wait_ready || exit 1
    vals="$(psql -X -Atq -f "$D/invariants.sql")"
    IFS='|' read -r a t b h <<<"$vals"
    [ -n "$a" ] && [ "$a" = "$t" ] && [ "$t" = "$b" ] && [ "$b" = "$h" ] || { echo "round $r: 原子性が崩れた: $vals" >&2; exit 1; }
    n="$(q 'select count(*) from pgbench_accounts')"
    [ "$n" = 100000 ] || { echo "round $r: pgbench_accounts が $n 行" >&2; exit 1; }
    dup="$(q 'select count(*) - count(distinct aid) from pgbench_accounts')"
    [ "$dup" = 0 ] || { echo "round $r: aid に重複 $dup" >&2; exit 1; }
    seq_n="$(psql -X -Atq -c 'set enable_seqscan = on; set enable_indexscan = off; set enable_bitmapscan = off; select count(*) from pgbench_accounts where aid > 0' | tail -1)"
    idx_n="$(psql -X -Atq -c 'set enable_seqscan = off; select count(*) from pgbench_accounts where aid > 0' | tail -1)"
    [ "$seq_n" = "$idx_n" ] || { echo "round $r: 全走査 $seq_n と索引走査 $idx_n が違う" >&2; exit 1; }
    echo "round $r OK: $vals"
done
