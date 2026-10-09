#!/usr/bin/env bash
# pgbench の各スクリプト（init.sh / run.sh）の共通部分。10 §6.10.3。
# PSQL / PGBENCH 環境変数でクライアントを差し替えられる（例: docker の postgres:17 の `docker exec ... psql`）。

PGB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib.sh
. "$PGB_DIR/../lib.sh"
compat_env || exit 2

# shellcheck disable=SC2086
psql() { command ${PSQL:-psql} "$@"; }
# shellcheck disable=SC2086
pgbench() { command ${PGBENCH:-pgbench} "$@"; }

# 結果の行（run.sh と同じ形式。compat/run.sh の drivers の表と揃える）
result() { echo "RESULT $1 $2 $3"; }

# pgbench_* の不変条件（4 つの和が一致）。空の和は 0 と見なす。
check_invariants() {
    local vals a t b h
    vals="$(psql -X -Atq -f "$PGB_DIR/invariants.sql")" || { echo "invariants.sql に失敗" >&2; return 1; }
    IFS='|' read -r a t b h <<<"$vals"
    a="${a:-0}"; t="${t:-0}"; b="${b:-0}"; h="${h:-0}"
    [ "$a" = "$t" ] && [ "$t" = "$b" ] && [ "$b" = "$h" ] || { echo "不変条件が崩れた: abalance=$a tbalance=$t bbalance=$b delta=$h" >&2; return 1; }
    INV_VALUE="$a"
}

# pgbench の出力から値を取る。 stat <出力> <行の前置>
pgb_field() { printf '%s\n' "$1" | sed -n "s/^$2: *\([0-9][0-9]*\).*/\1/p" | head -n 1; }
pgb_tps() { printf '%s\n' "$1" | sed -n 's/^tps = \([0-9.]*\).*/\1/p' | head -n 1; }

# pgbench_history の行数 = 成功したトランザクション数
check_history_count() {
    local n="$1" cnt
    cnt="$(q 'select count(*) from pgbench_history')" || return 1
    [ "$cnt" = "$n" ] || { echo "pgbench_history の行数 $cnt と処理数 $n が違う" >&2; return 1; }
}

# スケール 1 で初期化し直す（-n で走らせる各シナリオの前）。
reinit() {
    pgbench -i -s 1 -q >/dev/null 2>&1 || { echo "pgbench -i に失敗" >&2; return 1; }
}

# 時間の既定（--quick は 5 秒）
secs() { if [ "${QUICK:-0}" = 1 ]; then echo 5; else echo "$1"; fi; }
