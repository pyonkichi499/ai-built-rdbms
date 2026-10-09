#!/usr/bin/env bash
# pgbench G2〜G9（10 §6.10.3）。--quick（環境変数 QUICK=1）は各 5 秒。
#
#   G2 -M simple -c 8 -j 4 -T 20 / G3 -M extended / G4 -M prepared -c 32 -j 8 -T 30 / G5 RR + --max-tries=20 /
#   G6 -S（select-only）/ G7 -C（接続ごと）/ G8 夜間（COMPAT_NIGHTLY=1。300 秒、膨張しない）/ G9 custom.sql（FOR UPDATE と INSERT RETURNING）
#
# 環境変数: PGB_ONLY="G2 G5"（流すシナリオを限る）、G8_SECONDS（G8 の秒数。既定 300）、G8_BLOAT_LIMIT（既定 50 倍。U-5: PG17.11 の実測は 300 秒で branches が 29 倍 = 8KB → 232KB、tellers 8 倍。10 倍では PG 自身が落ちる）。
# 各シナリオは「初期化 → 実行 → 不変条件（abalance・tbalance・bbalance・delta の和が一致）→ 履歴の行数 = 成功した件数」。
# 出力: `RESULT <ID> <ok|FAIL> <詳細>`。1 つでも失敗すれば終了コード 1。
set -uo pipefail
D="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=common.sh
. "$D/common.sh"

FAIL=0
want() { [ -z "${PGB_ONLY:-}" ] || [[ " $PGB_ONLY " == *" $1 "* ]]; }

# run_g <ID> <説明> <pgbench の引数...>（PGB_PGOPTIONS で PGOPTIONS を渡す。G5 の RR）
run_g() {
    local id="$1" desc="$2"; shift 2
    local out rc n failed tps retried extra=""
    reinit || { result "$id" FAIL "初期化に失敗"; FAIL=1; return; }
    out="$(PGOPTIONS="${PGB_PGOPTIONS:-}" "${PGBENCH_CMD[@]}" "$@" 2>&1)"; rc=$?
    if [ "$rc" -ne 0 ]; then
        printf '%s\n' "$out" | tail -n 15 >&2
        result "$id" FAIL "pgbench が終了コード $rc（$desc）"; FAIL=1; return
    fi
    n="$(pgb_field "$out" 'number of transactions actually processed')"
    failed="$(pgb_field "$out" 'number of failed transactions')"
    tps="$(pgb_tps "$out")"
    retried="$(pgb_field "$out" 'number of transactions retried')"
    [ -n "$n" ] && [ "$n" -gt 0 ] || { printf '%s\n' "$out" | tail -n 15 >&2; result "$id" FAIL "処理数が 0（$desc）"; FAIL=1; return; }
    if [ "${ALLOW_FAILS:-0}" = 1 ]; then
        # RR: 失敗はすべて再試行できる種類（直列化 / デッドロック）。PG17.11 の実測（-c 8、スケール 1、--max-tries=20）でも
        # 5 秒で約 7% が 20 回の再試行を使い切って失敗する（設計書の「収束」は 0 件の意味ではない）ので、進捗（成功 > 失敗）と、再試行が実際に起きたことを見る
        local ser dl
        ser="$(pgb_field "$out" 'number of serialization failures')"; dl="$(pgb_field "$out" 'number of deadlock failures')"
        if [ "${failed:-0}" != "$(( ${ser:-0} + ${dl:-0} ))" ] || [ "${failed:-0}" -ge "$n" ] || [ "${retried:-0}" -eq 0 ]; then
            printf '%s\n' "$out" | tail -n 20 >&2
            result "$id" FAIL "失敗 ${failed:-?} 件（直列化 ${ser:-?}、デッドロック ${dl:-?}、処理 $n）。再試行が起きない、または成功より失敗が多い"; FAIL=1; return
        fi
        extra="、直列化失敗 ${ser:-0} 件（再試行 ${retried:-0}）"
    else
        [ "${failed:-0}" = 0 ] || { printf '%s\n' "$out" | tail -n 15 >&2; result "$id" FAIL "失敗したトランザクション ${failed}（$desc）"; FAIL=1; return; }
    fi
    check_invariants || { result "$id" FAIL "不変条件が崩れた（$desc）"; FAIL=1; return; }
    if [ "${NO_HISTORY_COUNT:-0}" != 1 ]; then
        check_history_count "$n" || { result "$id" FAIL "履歴の行数が処理数と違う（$desc）"; FAIL=1; return; }
    fi
    result "$id" ok "$desc: ${n} 件、tps ${tps}${extra}"
}

SECS20="$(secs 20)"; SECS30="$(secs 30)"; SECS10="$(secs 10)"
PGBENCH_CMD=(${PGBENCH:-pgbench})

want G2 && run_g G2 "-M simple -c 8 -j 4 -T $SECS20" -n -M simple -c 8 -j 4 -T "$SECS20"
want G3 && run_g G3 "-M extended -c 8 -j 4 -T $SECS20" -n -M extended -c 8 -j 4 -T "$SECS20"
want G4 && run_g G4 "-M prepared -c 32 -j 8 -T $SECS30" -n -M prepared -c 32 -j 8 -T "$SECS30"
want G5 && PGB_PGOPTIONS='-c default_transaction_isolation=repeatable\ read' ALLOW_FAILS=1 \
    run_g G5 "RR -M prepared -c 8 -T $SECS20 --max-tries=20" -n -M prepared -c 8 -T "$SECS20" --max-tries=20 --failures-detailed
want G6 && NO_HISTORY_COUNT=1 run_g G6 "-S -M prepared -c 16 -j 8 -T $SECS20" -n -S -M prepared -c 16 -j 8 -T "$SECS20"
want G7 && run_g G7 "-C -c 8 -T $SECS10" -n -C -c 8 -T "$SECS10"
want G9 && run_g G9 "-f custom.sql（FOR UPDATE + INSERT RETURNING）-c 8 -T $SECS10" -n -f "$D/custom.sql" -c 8 -T "$SECS10"

# G8: 夜間（COMPAT_NIGHTLY=1）。膨張しない: branches と tellers のサイズが初期の G8_BLOAT_LIMIT 倍（既定 50。PG17.11 の実測は branches 29 倍。U-5）以下。
if want G8 && [ "${COMPAT_NIGHTLY:-0}" = 1 ]; then
    limit="${G8_BLOAT_LIMIT:-50}"
    reinit || { result G8 FAIL "初期化に失敗"; FAIL=1; }
    size() { q "select pg_relation_size('$1')"; }
    s_b0="$(size pgbench_branches)"; s_t0="$(size pgbench_tellers)"
    out="$(pgbench -n -M simple -c 8 -j 4 -T "${G8_SECONDS:-300}" 2>&1)" || { printf '%s\n' "$out" | tail -n 15 >&2; result G8 FAIL "pgbench が失敗"; FAIL=1; }
    s_b1="$(size pgbench_branches)"; s_t1="$(size pgbench_tellers)"
    # 8KB ページ 1 つ分の下駄を履かせる（初期が 8KB なら 16KB まで）
    if check_invariants && [ "$s_b1" -le $(( (s_b0 > 8192 ? s_b0 : 8192) * limit )) ] && [ "$s_t1" -le $(( (s_t0 > 8192 ? s_t0 : 8192) * limit )) ]; then
        result G8 ok "branches ${s_b0}→${s_b1} バイト、tellers ${s_t0}→${s_t1} バイト（上限 ${limit} 倍）"
    else
        result G8 FAIL "膨張または不変条件の破れ: branches ${s_b0}→${s_b1}、tellers ${s_t0}→${s_t1}"; FAIL=1
    fi
fi

# 履歴を残さない（共有サーバに居座らせない）
psql -X -Atq -c 'drop table if exists pgbench_history, pgbench_tellers, pgbench_accounts, pgbench_branches cascade' >/dev/null 2>&1 || true
exit "$FAIL"
