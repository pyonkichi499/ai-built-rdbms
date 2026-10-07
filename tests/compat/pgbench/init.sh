#!/usr/bin/env bash
# pgbench -i -s 1 と -i -I dtGvp -s 1（10 §7.3 の 1〜2）。TARGET / PORT は run.sh が export する。
set -uo pipefail
D="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
. "$D/../lib.sh"
compat_env || exit 2

check_counts() {
    local want="100000|1|10|0" got
    got="$(psql -X -Atq -c "select (select count(*) from pgbench_accounts) || '|' || (select count(*) from pgbench_branches) || '|' || (select count(*) from pgbench_tellers) || '|' || (select count(*) from pgbench_history)")"
    [ "$got" = "$want" ] || { echo "行数が違う: want $want, got $got" >&2; return 1; }
    for t in pgbench_accounts pgbench_branches pgbench_tellers; do
        got="$(psql -X -Atq -c "select conname from pg_constraint where conrelid = '$t'::regclass and contype = 'p'")"
        [ "$got" = "${t}_pkey" ] || { echo "$t の主キーがない（$got）" >&2; return 1; }
    done
}

rc=0
for opts in "" "-I dtGvp"; do
    echo "-- pgbench -i -s 1 $opts"
    # shellcheck disable=SC2086
    pgbench -i -s 1 $opts >/tmp/compat-pgbench-init.$$ 2>&1 || { cat /tmp/compat-pgbench-init.$$; rc=1; continue; }
    check_counts || rc=1
done
rm -f /tmp/compat-pgbench-init.$$
exit "$rc"
