#!/usr/bin/env bash
# pgbench -i -s 1（10 §7.3 の 1〜2）、-I dtGvp（サーバ側生成）、-I dtgvpf（G1: FK 5 件）。TARGET / PORT は run.sh が export する。
set -uo pipefail
D="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=common.sh
. "$D/common.sh"

check_counts() {
    local want="100000|1|10|0" got t
    got="$(psql -X -Atq -c "select (select count(*) from pgbench_accounts) || '|' || (select count(*) from pgbench_branches) || '|' || (select count(*) from pgbench_tellers) || '|' || (select count(*) from pgbench_history)")"
    [ "$got" = "$want" ] || { echo "行数が違う: want $want, got $got" >&2; return 1; }
    for t in pgbench_accounts pgbench_branches pgbench_tellers; do
        got="$(psql -X -Atq -c "select conname from pg_constraint where conrelid = '$t'::regclass and contype = 'p'")"
        [ "$got" = "${t}_pkey" ] || { echo "$t の主キーがない（$got）" >&2; return 1; }
    done
}

# G1: FK は 5 件（07 §6.8。PG17.11 の -I dtgvpf で確認）。名前と参照先まで固定する。
check_fk() {
    local got want
    got="$(psql -X -Atq -c "select conname || ' ' || conrelid::regclass || ' -> ' || confrelid::regclass || ' ' || pg_get_constraintdef(oid) from pg_constraint where contype = 'f' and conrelid::regclass::text like 'pgbench\_%' order by conname")"
    want='pgbench_accounts_bid_fkey pgbench_accounts -> pgbench_branches FOREIGN KEY (bid) REFERENCES pgbench_branches(bid)
pgbench_history_aid_fkey pgbench_history -> pgbench_accounts FOREIGN KEY (aid) REFERENCES pgbench_accounts(aid)
pgbench_history_bid_fkey pgbench_history -> pgbench_branches FOREIGN KEY (bid) REFERENCES pgbench_branches(bid)
pgbench_history_tid_fkey pgbench_history -> pgbench_tellers FOREIGN KEY (tid) REFERENCES pgbench_tellers(tid)
pgbench_tellers_bid_fkey pgbench_tellers -> pgbench_branches FOREIGN KEY (bid) REFERENCES pgbench_branches(bid)'
    [ "$got" = "$want" ] || { echo "FK が違う:"; diff <(printf '%s\n' "$want") <(printf '%s\n' "$got"); return 1; } >&2
    # 張られた FK が効いている（存在しない親への INSERT は 23503）
    got="$(psql -X -Atq -c "insert into pgbench_history (tid, bid, aid, delta, mtime) values (999999, 1, 1, 0, now())" 2>&1)"
    case "$got" in *'violates foreign key constraint "pgbench_history_tid_fkey"'*) ;; *) echo "FK 違反が検出されない: $got" >&2; return 1 ;; esac
}

rc=0
for opts in "" "-I dtGvp" "-I dtgvpf"; do
    case "$opts" in "") id=init ;; "-I dtGvp") id=init-dtGvp ;; *) id=G1 ;; esac
    echo "-- pgbench -i -s 1 $opts"
    # shellcheck disable=SC2086
    pgbench -i -s 1 $opts >/tmp/compat-pgbench-init.$$ 2>&1 || { cat /tmp/compat-pgbench-init.$$; rc=1; result "$id" FAIL "pgbench -i が失敗"; continue; }
    if check_counts && { [ "$opts" != "-I dtgvpf" ] || check_fk; }; then
        result "$id" ok "行数と主キー$([ "$id" = G1 ] && echo "、FK 5 件")"
    else
        result "$id" FAIL "検査に失敗"; rc=1
    fi
done
rm -f /tmp/compat-pgbench-init.$$
exit "$rc"
