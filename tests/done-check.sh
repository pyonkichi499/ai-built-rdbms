#!/usr/bin/env bash
# M4 の完了判定をローカルで一発実行し、各条件の合否を一覧で出す（spec/design/m4/01-scope-decisions.md §3、11-tests-plan.md §1.3）。
#
#   tests/done-check.sh [--only 1,2,5] [--skip 6] [--quick] [--keep] [--pg-only] [--log-dir DIR]
#
# 条件（11 §1.3 の表）:
#   1 cargo fmt / clippy / test（impl/rust）
#   2 tests/run.sh --target pg と --target yuzhu（tests/slt の m1〜m4）と slttools lint
#   3 --restart / --crash（tests/restart、restart/m3、restart/m4）を pg と yuzhu で
#   4 isolation の全 spec を pg と yuzhu で
#   5 tests/compat/run.sh --target pg と yuzhu
#   6 差分ファジング（tests/tools/difffuzz。固定シード 1〜32 × 200 ケース、長時間 1001〜1004 × 10,000 ケース。全領域。1 ケース = 約 18 文）
#   7 クラッシュ試験 層 1（cargo test --release -p yuzhu-core --test crash_sim。変異テストを含む）
#   8 EXPLAIN の書式（explain/format.slt、deparse_*.slt）と plan_variants を pg と yuzhu で
#   9 QUESTIONS.md / PROGRESS.md の運用の確認
#
# - 他のエージェントが使っている PostgreSQL（55432）と yuzhu（5432）には触らない。専用のインスタンスを起動する
#   （PostgreSQL は 127.0.0.1:$DC_PG_PORT（既定 55443）、yuzhu は 127.0.0.1:$DC_YUZHU_PORT（既定 5443）。終了時に止めて消す。--keep で残す）。
# - 条件ごとに失敗しても次へ進み、最後に表を出す。1 つでも FAIL / MISSING があれば終了コード 1。
# - ログは --log-dir（既定 /tmp/yuzhu-done-check/<日時>）に条件ごと・ステップごとに残す。
# - --pg-only: yuzhu を使うステップ（名前が yuzhu で始まるもの）と条件 6 を飛ばす。yuzhu 無しでテスト自体を PG で検証する用（完了判定には使わない）。
# - --quick: 6 の長時間実行と（compat の）pgbench の時間を短くする。完了判定には使わない（表に QUICK と出る）。
# - 前提: sqllogictest-bin 0.29.1、psql/pgbench 17（条件 5）、cargo。sandbox/pg.sh か docker。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ONLY=""
SKIP=""
QUICK=0
PG_ONLY=0
KEEP=0
LOG_DIR=""
while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --skip) SKIP="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --keep) KEEP=1; shift ;;
        --pg-only) PG_ONLY=1; shift ;;
        --log-dir) LOG_DIR="$2"; shift 2 ;;
        -h|--help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

DC_PG_PORT="${DC_PG_PORT:-55443}"
DC_YUZHU_PORT="${DC_YUZHU_PORT:-5443}"
DC_PG_DATA="${DC_PG_DATA:-/tmp/yuzhu-pg17-done}"
DC_PG_NAME="${DC_PG_NAME:-yuzhu-done-pg}"
export YUZHU_STATE="${YUZHU_STATE_DONE:-/tmp/yuzhu-done-state}"
LOG_DIR="${LOG_DIR:-/tmp/yuzhu-done-check/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$LOG_DIR"

TOOLS_BIN="${CARGO_TARGET_DIR:-}"
SLTTOOLS_MANIFEST="$ROOT/tests/tools/slttools/Cargo.toml"
ISOLATION_MANIFEST="$ROOT/tests/tools/isolation/Cargo.toml"
DIFFFUZZ_MANIFEST="$ROOT/tests/tools/difffuzz/Cargo.toml"

declare -A RESULT NOTE
STEP_FAILS=0

selected() {
    local n="$1"
    if [ -n "$ONLY" ] && ! [[ ",$ONLY," == *",$n,"* ]]; then return 1; fi
    if [ -n "$SKIP" ] && [[ ",$SKIP," == *",$n,"* ]]; then return 1; fi
    return 0
}

# step <条件番号> <名前> <コマンド...>: ログに流して合否を記録する
step() {
    local cond="$1" name="$2"; shift 2
    local log="$LOG_DIR/c${cond}-${name//[^A-Za-z0-9_-]/_}.log"
    if [ "$PG_ONLY" -eq 1 ] && [[ "$name" == yuzhu* ]]; then printf "  [%s] %s ... skip (--pg-only)\n" "$cond" "$name"; return 0; fi
    printf '  [%s] %s ... ' "$cond" "$name"
    if "$@" >"$log" 2>&1; then
        echo ok
    else
        echo "FAIL (log: $log)"
        STEP_FAILS=$((STEP_FAILS + 1))
        FAILED_STEPS["$cond"]+="${name}; "
    fi
}
declare -A FAILED_STEPS

# 条件の結果を確定する
finish() {
    local cond="$1"
    if [ -n "${FAILED_STEPS[$cond]:-}" ]; then
        RESULT[$cond]=FAIL
        NOTE[$cond]="${FAILED_STEPS[$cond]}"
    else
        RESULT[$cond]=PASS
    fi
}

missing() { RESULT[$1]=MISSING; NOTE[$1]="$2"; echo "  [$1] MISSING: $2"; }

# ------------------------------------------------------------ 専用サーバ
have_docker() { command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; }

pg_start() {
    if have_docker; then
        tests/pg.sh start --port "$DC_PG_PORT" --name "$DC_PG_NAME"
    else
        PG_PORT="$DC_PG_PORT" PG_DATA="$DC_PG_DATA" sandbox/pg.sh stop >/dev/null 2>&1 || true
        PG_PORT="$DC_PG_PORT" PG_DATA="$DC_PG_DATA" sandbox/pg.sh start
    fi
}
pg_fresh() { pg_stop >/dev/null 2>&1 || true; pg_start; }
pg_stop() {
    if have_docker; then tests/pg.sh stop --port "$DC_PG_PORT" --name "$DC_PG_NAME"
    else PG_PORT="$DC_PG_PORT" PG_DATA="$DC_PG_DATA" sandbox/pg.sh stop; fi
}
# run.sh からの再起動・クラッシュは環境変数で専用インスタンスへ向ける
pg_env() {
    if have_docker; then
        PG_CONTAINER="$DC_PG_NAME" PG_PORT="$DC_PG_PORT" "$@"
    else
        PG_DATA="$DC_PG_DATA" PG_PORT="$DC_PG_PORT" "$@"
    fi
}

yuzhu_fresh() {
    tests/yuzhu.sh clean >/dev/null 2>&1 || true
    tests/yuzhu.sh start --port "$DC_YUZHU_PORT"
}
yuzhu_stop() { tests/yuzhu.sh clean; }

cleanup() {
    [ "$KEEP" -eq 1 ] && return
    tests/yuzhu.sh clean >/dev/null 2>&1 || true
    pg_stop >/dev/null 2>&1 || true
}
trap cleanup EXIT

build_tool() { # <manifest> <bin 名>。出力: バイナリのパス
    local m="$1" b="$2" dir
    cargo build --release --manifest-path "$m" >&2 || return 1
    dir="${CARGO_TARGET_DIR:-$(dirname "$m")/target}"
    echo "$dir/release/$b"
}

SLT_PG=(tests/run.sh --target pg --port "$DC_PG_PORT")
SLT_YZ=(tests/run.sh --target yuzhu --port "$DC_YUZHU_PORT")

echo "== M4 完了判定 (log: $LOG_DIR)"
[ "$QUICK" -eq 1 ] && echo "   --quick: 完了判定としては無効"

# ------------------------------------------------------------ 1
if selected 1; then
    echo "-- 条件 1: Rust の確認コマンド"
    step 1 fmt bash -c 'cd impl/rust && cargo fmt --check'
    step 1 clippy bash -c 'cd impl/rust && cargo clippy --all-targets -- -D warnings'
    step 1 test bash -c 'cd impl/rust && cargo test'
    finish 1
fi

NEED_PG=0; NEED_YZ=0
for c in 2 3 4 5 6 8; do selected "$c" && NEED_PG=1; done
for c in 2 3 4 5 6 8; do selected "$c" && NEED_YZ=1; done
if [ "$NEED_YZ" -eq 1 ]; then
    step 0 build-yuzhu bash -c 'cd impl/rust && cargo build --release -p yuzhu-server'
fi

# ------------------------------------------------------------ 2
if selected 2; then
    echo "-- 条件 2: tests/slt（m1〜m4）"
    SLTTOOLS="$(build_tool "$SLTTOOLS_MANIFEST" slttools 2>"$LOG_DIR/c2-build-slttools.log")" || SLTTOOLS=""
    if [ -n "$SLTTOOLS" ]; then
        step 2 slt-lint "$SLTTOOLS" lint
        step 2 consistency-sync "$SLTTOOLS" consistency --check
        step 2 plan-variants-check "$SLTTOOLS" plan-variants check
    else
        FAILED_STEPS[2]+="build slttools; "; echo "  [2] build slttools FAIL"
    fi
    step 2 pg-prepare pg_fresh
    step 2 pg-slt "${SLT_PG[@]}"
    step 2 yuzhu-prepare yuzhu_fresh
    step 2 yuzhu-slt "${SLT_YZ[@]}"
    finish 2
fi

# ------------------------------------------------------------ 3
if selected 3; then
    echo "-- 条件 3: 再起動・クラッシュ"
    step 3 pg-prepare pg_fresh
    step 3 pg-restart pg_env "${SLT_PG[@]}" --restart
    step 3 pg-crash pg_env "${SLT_PG[@]}" --crash
    step 3 yuzhu-prepare yuzhu_fresh
    step 3 yuzhu-restart "${SLT_YZ[@]}" --restart
    step 3 yuzhu-crash "${SLT_YZ[@]}" --crash
    finish 3
fi

# ------------------------------------------------------------ 4
if selected 4; then
    echo "-- 条件 4: isolation"
    ISO="$(build_tool "$ISOLATION_MANIFEST" yuzhu-isolation 2>"$LOG_DIR/c4-build.log")" || ISO=""
    if [ -z "$ISO" ]; then
        FAILED_STEPS[4]+="build yuzhu-isolation; "
    else
        step 4 pg-prepare pg_fresh
        step 4 pg-isolation "$ISO" --port "$DC_PG_PORT" tests/isolation/specs
        step 4 yuzhu-prepare yuzhu_fresh
        step 4 yuzhu-isolation "$ISO" --port "$DC_YUZHU_PORT" --blocking-detection timeout --variant yuzhu-m3 tests/isolation/specs
    fi
    finish 4
fi

# ------------------------------------------------------------ 5
if selected 5; then
    echo "-- 条件 5: 周辺ツール互換 (tests/compat)"
    if [ ! -x tests/compat/run.sh ]; then
        missing 5 "tests/compat/run.sh がない（K4 の compat は未作成）"
    else
        extra=(); [ "$QUICK" -eq 1 ] && extra=(--quick)
        step 5 pg-prepare pg_fresh
        step 5 pg-compat pg_env tests/compat/run.sh --target pg --port "$DC_PG_PORT" ${extra[@]+"${extra[@]}"}
        step 5 yuzhu-prepare yuzhu_fresh
        step 5 yuzhu-compat tests/compat/run.sh --target yuzhu --port "$DC_YUZHU_PORT" ${extra[@]+"${extra[@]}"}
        finish 5
    fi
fi

# ------------------------------------------------------------ 6
fuzz_seeds() { # <ref ポート> <test ポート> <bin> <seed 開始> <終了> <ケース数>
    # yuzhu は書き込みが 1 本ずつなので、1 台へ並列に流すと待ちが出る。ワーカーごとに専用の yuzhu を起動し、
    # PostgreSQL は共有する（生成器の共有名はシードとケースの番号で分けてある）。ワーカー数は DC_FUZZ_JOBS（既定 4）。
    local rp="$1" tp="$2" bin="$3" s="$4" e="$5" q="$6" ex=() n w jobs="${DC_FUZZ_JOBS:-4}" rc=0 pids=() port
    while IFS= read -r n; do
        case "$n" in ''|'#'*) ;; *) ex+=(--exclude "$n") ;; esac
    done < "$ROOT/tests/tools/difffuzz/known-excludes.txt"
    [ "$jobs" -gt $((e - s + 1)) ] && jobs=$((e - s + 1))
    for ((w = 0; w < jobs; w++)); do
        port="$tp"
        if [ "$w" -gt 0 ]; then
            port=$((tp + w))
            YUZHU_STATE="$YUZHU_STATE-w$w" tests/yuzhu.sh clean >/dev/null 2>&1 || true
            YUZHU_STATE="$YUZHU_STATE-w$w" tests/yuzhu.sh start --port "$port" >"$LOG_DIR/fuzz-yuzhu-w$w.log" 2>&1 || { echo "worker $w: yuzhu を起動できない"; rc=1; continue; }
        fi
        (
            wrc=0
            for ((seed = s + w; seed <= e; seed += jobs)); do
                "$bin" --domain all --skip-unsupported-legacy --ignore-trailing-space --seed "$seed" --cases "$q" ${ex[@]+"${ex[@]}"} \
                    --pg "host=127.0.0.1 port=$rp user=postgres dbname=postgres" \
                    --yuzhu "host=127.0.0.1 port=$port user=postgres dbname=postgres" \
                    --out "$LOG_DIR/difffuzz-fail-$seed.jsonl" || { echo "seed $seed: 差分あり（$LOG_DIR/difffuzz-fail-$seed.jsonl）"; wrc=1; }
            done
            exit "$wrc"
        ) &
        pids+=("$!")
    done
    for n in ${pids[@]+"${pids[@]}"}; do wait "$n" || rc=1; done
    for ((w = 1; w < jobs; w++)); do YUZHU_STATE="$YUZHU_STATE-w$w" tests/yuzhu.sh clean >/dev/null 2>&1 || true; done
    return "$rc"
}
if selected 6; then
    if [ "$PG_ONLY" -eq 1 ]; then echo "-- 条件 6: --pg-only のため飛ばす"; else
    echo "-- 条件 6: 差分ファジング（difffuzz。既知の差は tests/tools/difffuzz/known-excludes.txt に書いた針で除外した範囲だけを許す）"
    DT="$(build_tool "$DIFFFUZZ_MANIFEST" difffuzz 2>"$LOG_DIR/c6-build.log")" || DT=""
    if [ -z "$DT" ]; then
        FAILED_STEPS[6]+="build difffuzz（tests/tools/difffuzz がビルドできない）; "
    else
        step 6 pg-prepare pg_fresh
        step 6 yuzhu-prepare yuzhu_fresh
        step 6 fixed-seeds-1-32 fuzz_seeds "$DC_PG_PORT" "$DC_YUZHU_PORT" "$DT" 1 32 200
        if [ "$QUICK" -eq 1 ]; then
            step 6 long-QUICK fuzz_seeds "$DC_PG_PORT" "$DC_YUZHU_PORT" "$DT" 1001 1001 300
        else
            step 6 long-1001-1004 fuzz_seeds "$DC_PG_PORT" "$DC_YUZHU_PORT" "$DT" 1001 1004 10000
        fi
    fi
    finish 6
    [ "$QUICK" -eq 1 ] && RESULT[6]="${RESULT[6]}(QUICK)"
    fi
fi

# ------------------------------------------------------------ 7
if selected 7; then
    echo "-- 条件 7: クラッシュ試験 層 1（ワークロード 1〜8、I1〜I16、変異テスト）"
    step 7 crash-sim bash -c 'cd impl/rust && cargo test --release -p yuzhu-core --test crash_sim'
    finish 7
fi

# ------------------------------------------------------------ 8
if selected 8; then
    echo "-- 条件 8: EXPLAIN の書式と plan_variants"
    files=()
    while IFS= read -r f; do files+=("$f"); done < <(
        { ls tests/slt/m4/explain/format.slt tests/slt/m4/explain/deparse_*.slt 2>/dev/null; find tests/slt/m4/plan_variants -name '*.slt' 2>/dev/null; } | LC_ALL=C sort)
    if [ ${#files[@]} -eq 0 ]; then
        missing 8 "explain/format.slt・deparse_*.slt・plan_variants/*.slt がまだない（K3 の範囲）"
    else
        [ -f tests/slt/m4/explain/format.slt ] || FAILED_STEPS[8]+="explain/format.slt がない; "
        ls tests/slt/m4/explain/deparse_*.slt >/dev/null 2>&1 || FAILED_STEPS[8]+="explain/deparse_*.slt がない; "
        [ -d tests/slt/m4/plan_variants ] || FAILED_STEPS[8]+="plan_variants/ がない; "
        step 8 pg-prepare pg_fresh
        step 8 pg-explain "${SLT_PG[@]}" "${files[@]}"
        step 8 yuzhu-prepare yuzhu_fresh
        step 8 yuzhu-explain "${SLT_YZ[@]}" "${files[@]}"
        finish 8
    fi
fi

# ------------------------------------------------------------ 9
check9() {
    local rc=0
    grep -q "99-questions" QUESTIONS.md 2>/dev/null || { echo "QUESTIONS.md に 99-questions.md の確認事項の転記がない（'99-questions' の言及がない）"; rc=1; }
    grep -Eq "M4.*(完了|done)" PROGRESS.md 2>/dev/null || { echo "PROGRESS.md に M4 完了の記述がない"; rc=1; }
    return "$rc"
}
if selected 9; then
    echo "-- 条件 9: 運用（QUESTIONS.md / PROGRESS.md）"
    step 9 docs check9
    finish 9
fi

# ------------------------------------------------------------ 一覧
echo
echo "== 結果"
printf '%-4s %-8s %s\n' "条件" "判定" "内容 / 備考"
declare -A TITLE=(
    [1]="cargo fmt / clippy / test"
    [2]="tests/run.sh pg・yuzhu（slt m1〜m4）+ lint"
    [3]="--restart / --crash（pg・yuzhu）"
    [4]="isolation（pg・yuzhu）"
    [5]="tests/compat（psql・copy・pgbench）"
    [6]="差分ランダムテスト（シード 1〜32、長時間 1001〜1004）"
    [7]="クラッシュ試験 層 1"
    [8]="EXPLAIN の書式・plan_variants"
    [9]="QUESTIONS.md / PROGRESS.md"
)
overall=0
for c in 1 2 3 4 5 6 7 8 9; do
    r="${RESULT[$c]:-SKIPPED}"
    printf '%-4s %-8s %s %s\n' "$c" "$r" "${TITLE[$c]}" "${NOTE[$c]:+- ${NOTE[$c]}}"
    case "$r" in PASS) ;; SKIPPED) ;; *) overall=1 ;; esac
done
echo
if [ "$overall" -eq 0 ] && [ "$QUICK" -eq 0 ] && [ "$PG_ONLY" -eq 0 ] && [ -z "$ONLY$SKIP" ]; then
    echo "M4 完了: 条件 1〜9 がすべて PASS"
elif [ "$overall" -eq 0 ]; then
    echo "選択した条件はすべて PASS（--only / --skip / --quick のため、M4 完了の判定ではない）"
else
    echo "M4 は未完了（FAIL / MISSING の条件がある。ログ: $LOG_DIR）"
fi
exit "$overall"
