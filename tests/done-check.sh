#!/usr/bin/env bash
# M5 の完了判定をローカルで一発実行し、各条件の合否を一覧で出す（spec/design/m5/10-tests-plan.md §7.2・§7.3）。
#
#   tests/done-check.sh [--only 1,2,5] [--skip 6] [--quick] [--keep] [--pg-only] [--log-dir DIR]
#
# 条件（M5 の 1〜13 は 10 §7.3。14・15 は M4 の条件から引き継ぐ確認）:
#   1  cargo fmt / clippy / test、forbid(unsafe_code)、yuzhu-core が外部クレートに直接依存しない、format_version 拒否のテスト
#   2  tests/slt の m1〜m5 を pg・yuzhu × simple・extended（+ slttools lint）
#   3  isolation 共有 spec を pg・yuzhu × simple・extended（+ ランナーの cargo test）
#   4  isolation ported（pg は期待の確認、yuzhu は --variant yuzhu-m5 と skip.txt）
#   5  restart / crash（既定の直下・m3・m4 と m5）を pg と yuzhu で
#   6  クラッシュ試験 層 1（crash_sim。W1〜W11、変異 25 件の検出テストが 25 件以上）
#   7  クラッシュ試験 層 2（crash_kill9 --ignored。YUZHU_KILL9_ROUNDS=200。--quick は 5）
#   8  並行ストレス S1〜S7（プロセス内 + wire 版を pg・yuzhu に）
#   9  compat（psql・copy・pgbench）、drivers、SCRAM の auth
#   10 lint-tests.sh --release（skipif yuzhu 0 件、台帳）、q-index.sh --check
#   11 性能ゲート（tests/perf/run.sh --compare tests/perf/baseline.json）
#   12 M4 との統合チェックリスト（人が確認。DC_M12_CONFIRMED=1 で PASS）
#   13 ドキュメント（tests/README.md、QUESTIONS.md、PROGRESS.md、spec/design/m5-changes.md）
#   14 差分ファジング（tests/tools/difffuzz。固定シード 1〜32 × 200 ケース、長時間 1001〜1004 × 10,000 ケース）
#   15 EXPLAIN の書式（explain/format.slt、deparse_*.slt）と plan_variants を pg と yuzhu で
#
# - 他のエージェントが使っている PostgreSQL（55432）と yuzhu（5432）には触らない。専用のインスタンスを起動する
#   （PostgreSQL は 127.0.0.1:$DC_PG_PORT（既定 55443）、yuzhu は 127.0.0.1:$DC_YUZHU_PORT（既定 5443）。終了時に止めて消す。--keep で残す）。
# - 条件ごとに失敗しても次へ進み、最後に表を出す。1 つでも FAIL / MISSING があれば終了コード 1。
#   まだ存在しないスクリプト・テストは MISSING と出す（一部のステップだけなら、他が通っても条件は MISSING）。
# - ログは --log-dir（既定 /tmp/yuzhu-done-check/<日時>）に条件ごと・ステップごとに残す。
# - --pg-only: yuzhu を使うステップ（名前が yuzhu で始まるもの）と条件 14 を飛ばす。yuzhu 無しでテスト自体を PG で検証する用（完了判定には使わない）。
# - --quick: 7・8・11・14 の長時間実行、4 の extended、compat の pgbench の時間を短くする。完了判定には使わない（表に QUICK と出る）。
# - 前提: sqllogictest-bin 0.29.1、psql/pgbench 17（条件 9）、cargo。sandbox/pg.sh か docker。
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
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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
declare -A FAILED_STEPS MISSING_STEPS

# 条件の結果を確定する
finish() {
    local cond="$1"
    if [ -n "${FAILED_STEPS[$cond]:-}" ]; then
        RESULT[$cond]=FAIL
        NOTE[$cond]="${FAILED_STEPS[$cond]}${MISSING_STEPS[$cond]:+ MISSING: ${MISSING_STEPS[$cond]}}"
    elif [ -n "${MISSING_STEPS[$cond]:-}" ]; then
        RESULT[$cond]=MISSING
        NOTE[$cond]="${MISSING_STEPS[$cond]}"
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

echo "== M5 完了判定 (log: $LOG_DIR)"
[ "$QUICK" -eq 1 ] && echo "   --quick: 完了判定としては無効"
[ "$PG_ONLY" -eq 1 ] && echo "   --pg-only: 完了判定としては無効（yuzhu を使うステップを飛ばす）"

exists_any() { compgen -G "$1" >/dev/null 2>&1; }
# テスト名の一覧から数える（cargo test -- --list）
core_test_count() { # <テスト名の正規表現> <cargo test の追加引数...>
    local re="$1"; shift
    (cd impl/rust && cargo test --release -p yuzhu-core "$@" -- --list 2>/dev/null | grep -Ec "$re")
}

NEED_PG=0; NEED_YZ=0
for c in 2 3 4 5 8 9 14 15; do selected "$c" && NEED_PG=1; done
for c in 2 3 4 5 8 9 14 15; do selected "$c" && NEED_YZ=1; done
if [ "$NEED_YZ" -eq 1 ]; then
    step 0 yuzhu-build bash -c 'cd impl/rust && cargo build --release -p yuzhu-server'
fi

# ------------------------------------------------------------ 1
check_forbid_unsafe() {
    local rc=0 f
    grep -q 'unsafe_code *= *"forbid"' impl/rust/Cargo.toml || { echo "ワークスペース lint の unsafe_code = forbid がない"; rc=1; }
    for f in impl/rust/crates/*/src/lib.rs impl/rust/crates/*/src/main.rs; do
        [ -f "$f" ] || continue
        grep -q 'forbid(unsafe_code)' "$f" || { echo "$f に #![forbid(unsafe_code)] がない"; rc=1; }
    done
    return "$rc"
}
check_core_deps() { # yuzhu-core の直接依存（normal）が path 依存だけ（D29）
    local out rc=0 line
    out="$(cd impl/rust && cargo tree -p yuzhu-core --depth 1 --edges normal --prefix none)" || return 1
    echo "$out"
    while IFS= read -r line; do
        [[ "$line" == yuzhu-core\ * ]] && continue
        [ -z "$line" ] && continue
        [[ "$line" == *"(/"* ]] || { echo "crates.io の直接依存: $line"; rc=1; }
    done <<<"$out"
    return "$rc"
}
check_format_version_test() { # D35: format_version の不一致で起動を拒否するテストがあること
    grep -rEn 'fn [a-z0-9_]*format_version[a-z0-9_]*' impl/rust/crates --include=*.rs | grep -q . \
        || { echo "format_version の不一致を検査するテスト（名前に format_version を含む fn）がない"; return 1; }
}
if selected 1; then
    echo "-- 条件 1: Rust の確認コマンド・forbid(unsafe_code)・yuzhu-core の依存・format_version"
    step 1 fmt bash -c 'cd impl/rust && cargo fmt --check'
    step 1 clippy bash -c 'cd impl/rust && cargo clippy --all-targets -- -D warnings'
    step 1 test bash -c 'cd impl/rust && cargo test'
    step 1 forbid-unsafe check_forbid_unsafe
    step 1 core-no-external-deps check_core_deps
    step 1 format-version-test check_format_version_test
    finish 1
fi

# ------------------------------------------------------------ 2
if selected 2; then
    echo "-- 条件 2: tests/slt（m1〜m5）を pg・yuzhu × simple・extended"
    SLTTOOLS="$(build_tool "$SLTTOOLS_MANIFEST" slttools 2>"$LOG_DIR/c2-build-slttools.log")" || SLTTOOLS=""
    if [ -n "$SLTTOOLS" ]; then
        step 2 slt-lint "$SLTTOOLS" lint
        step 2 consistency-sync "$SLTTOOLS" consistency --check
        step 2 plan-variants-check "$SLTTOOLS" plan-variants check
    else
        FAILED_STEPS[2]+="build slttools; "; echo "  [2] build slttools FAIL"
    fi
    for proto in simple extended; do
        step 2 pg-prepare-$proto pg_fresh
        step 2 pg-slt-$proto "${SLT_PG[@]}" --protocol "$proto" tests/slt
        step 2 yuzhu-prepare-$proto yuzhu_fresh
        step 2 yuzhu-slt-$proto "${SLT_YZ[@]}" --protocol "$proto" tests/slt
    done
    finish 2
fi

# ------------------------------------------------------------ 3
if selected 3; then
    echo "-- 条件 3: isolation 共有 spec（pg・yuzhu × simple・extended）"
    ISO="$(build_tool "$ISOLATION_MANIFEST" yuzhu-isolation 2>"$LOG_DIR/c3-build.log")" || ISO=""
    if [ -z "$ISO" ]; then
        FAILED_STEPS[3]+="build yuzhu-isolation; "
    else
        step 3 isolation-tool-test cargo test --release --manifest-path "$ISOLATION_MANIFEST"
        for proto in simple extended; do
            step 3 pg-prepare-$proto pg_fresh
            step 3 pg-isolation-$proto "$ISO" --port "$DC_PG_PORT" --protocol "$proto" tests/isolation/specs
            step 3 yuzhu-prepare-$proto yuzhu_fresh
            step 3 yuzhu-isolation-$proto "$ISO" --port "$DC_YUZHU_PORT" --protocol "$proto" tests/isolation/specs
        done
    fi
    finish 3
fi

# ------------------------------------------------------------ 4
if selected 4; then
    echo "-- 条件 4: isolation ported（pg は期待の確認、yuzhu は variant yuzhu-m5 と skip.txt）"
    if [ ! -d tests/isolation/ported/specs ]; then
        missing 4 "tests/isolation/ported/specs がない"
    else
        ISO="$(build_tool "$ISOLATION_MANIFEST" yuzhu-isolation 2>"$LOG_DIR/c4-build.log")" || ISO=""
        if [ -z "$ISO" ]; then
            FAILED_STEPS[4]+="build yuzhu-isolation; "
        else
            step 4 pg-prepare pg_fresh
            step 4 pg-ported "$ISO" --port "$DC_PG_PORT" tests/isolation/ported/specs
            step 4 yuzhu-prepare yuzhu_fresh
            step 4 yuzhu-ported "$ISO" --port "$DC_YUZHU_PORT" --variant yuzhu-m5 --skip-list tests/isolation/ported/skip.txt tests/isolation/ported/specs
            # ported の extended は夜間（10 §6.10.6）。--quick でないときだけ流す
            if [ "$QUICK" -eq 0 ]; then
                step 4 yuzhu-prepare-ext yuzhu_fresh
                step 4 yuzhu-ported-extended "$ISO" --port "$DC_YUZHU_PORT" --protocol extended --variant yuzhu-m5 --skip-list tests/isolation/ported/skip.txt tests/isolation/ported/specs
            fi
        fi
        finish 4
    fi
fi

# ------------------------------------------------------------ 5
if selected 5; then
    echo "-- 条件 5: 再起動・クラッシュ（既定の直下・m3・m4 と m5）"
    if [ ! -d tests/restart/m5 ]; then
        missing 5 "tests/restart/m5 がない"
    else
        step 5 pg-prepare pg_fresh
        step 5 pg-restart pg_env "${SLT_PG[@]}" --restart
        step 5 pg-restart-m5 pg_env "${SLT_PG[@]}" --restart tests/restart/m5
        step 5 pg-crash pg_env "${SLT_PG[@]}" --crash
        step 5 pg-crash-m5 pg_env "${SLT_PG[@]}" --crash tests/restart/m5
        step 5 yuzhu-prepare yuzhu_fresh
        step 5 yuzhu-restart "${SLT_YZ[@]}" --restart
        step 5 yuzhu-restart-m5 "${SLT_YZ[@]}" --restart tests/restart/m5
        step 5 yuzhu-crash "${SLT_YZ[@]}" --crash
        step 5 yuzhu-crash-m5 "${SLT_YZ[@]}" --crash tests/restart/m5
        finish 5
    fi
fi

# ------------------------------------------------------------ 6
if selected 6; then
    echo "-- 条件 6: クラッシュ試験 層 1（W1〜W11、I1〜I24、変異 25 件、CrashCoverage、決定性）"
    step 6 crash-sim bash -c 'cd impl/rust && cargo test --release -p yuzhu-core --test crash_sim'
    check_mutation_count() { # M3 の 7 + M5 の 18 = 25 件の検出テストが名前で見えること（M4 の変異は別枠）
        local n
        n="$(core_test_count 'mutation.*detects_|detects_' --test crash_sim)" || return 1
        echo "検出テストの数: $n"
        [ "$n" -ge 25 ]
    }
    step 6 mutation-count-ge-25 check_mutation_count
    finish 6
fi

# ------------------------------------------------------------ 7
if selected 7; then
    echo "-- 条件 7: クラッシュ試験 層 2（crash_kill9。夜間は 200 回）"
    rounds=200; [ "$QUICK" -eq 1 ] && rounds=5
    step 7 crash-kill9 bash -c "cd impl/rust && YUZHU_KILL9_ROUNDS=$rounds cargo test --release -p yuzhu-server --test crash_kill9 -- --ignored"
    finish 7
    [ "$QUICK" -eq 1 ] && RESULT[7]="${RESULT[7]}(QUICK)"
fi

# ------------------------------------------------------------ 8
if selected 8; then
    echo "-- 条件 8: 並行ストレス S1〜S7（プロセス内。wire 版を pg・yuzhu に）"
    if ! exists_any 'impl/rust/crates/yuzhu-core/tests/stress*'; then
        missing 8 "yuzhu-core の stress テストがまだない（TS-2a / TS-2b）"
    else
        step 8 stress-inproc bash -c 'cd impl/rust && cargo test --release -p yuzhu-core --test stress'
        if exists_any 'impl/rust/crates/yuzhu-server/tests/concurrency*'; then
            secs=600; [ "$QUICK" -eq 1 ] && secs=5
            step 8 pg-prepare pg_fresh
            step 8 pg-stress-wire bash -c "cd impl/rust && YUZHU_STRESS_TARGET=pg YUZHU_STRESS_PORT=$DC_PG_PORT YUZHU_STRESS_SECS=$secs cargo test --release -p yuzhu-server --test concurrency -- --ignored"
            step 8 yuzhu-prepare yuzhu_fresh
            step 8 yuzhu-stress-wire bash -c "cd impl/rust && YUZHU_STRESS_TARGET=yuzhu YUZHU_STRESS_PORT=$DC_YUZHU_PORT YUZHU_STRESS_SECS=$secs cargo test --release -p yuzhu-server --test concurrency -- --ignored"
        else
            FAILED_STEPS[8]+="yuzhu-server/tests/concurrency.rs（wire 版）がない; "
        fi
        finish 8
        [ "$QUICK" -eq 1 ] && RESULT[8]="${RESULT[8]}(QUICK)"
    fi
fi

# ------------------------------------------------------------ 9
if selected 9; then
    echo "-- 条件 9: ドライバと互換（compat psql・copy・pgbench、drivers、SCRAM の auth）"
    if [ ! -x tests/compat/run.sh ]; then
        missing 9 "tests/compat/run.sh がない"
    else
        extra=(); [ "$QUICK" -eq 1 ] && extra=(--quick)
        step 9 pg-prepare pg_fresh
        step 9 pg-compat pg_env tests/compat/run.sh --target pg --port "$DC_PG_PORT" ${extra[@]+"${extra[@]}"}
        step 9 yuzhu-prepare yuzhu_fresh
        step 9 yuzhu-compat tests/compat/run.sh --target yuzhu --port "$DC_YUZHU_PORT" ${extra[@]+"${extra[@]}"}
        if grep -q 'drivers' tests/compat/run.sh; then
            step 9 pg-drivers pg_env tests/compat/run.sh --target pg --port "$DC_PG_PORT" --suite drivers ${extra[@]+"${extra[@]}"}
            step 9 yuzhu-drivers tests/compat/run.sh --target yuzhu --port "$DC_YUZHU_PORT" --suite drivers ${extra[@]+"${extra[@]}"}
        else
            MISSING_STEPS[9]+="compat/run.sh に --suite drivers がない（TS-4a）; "
        fi
        if grep -q -- '--auth' tests/yuzhu.sh && grep -q 'auth' tests/compat/run.sh; then
            step 9 yuzhu-scram-prepare bash -c 'tests/yuzhu.sh clean >/dev/null 2>&1; tests/yuzhu.sh start --port "$0" --auth scram' "$DC_YUZHU_PORT"
            step 9 yuzhu-scram-slt "${SLT_YZ[@]}" --password "${DC_SCRAM_PASSWORD:-postgres}" tests/slt/m5
            step 9 yuzhu-scram-auth tests/compat/run.sh --target yuzhu --port "$DC_YUZHU_PORT" --suite auth --auth scram
        else
            MISSING_STEPS[9]+="tests/yuzhu.sh --auth または compat の auth suite がない（TS-4b、AU-2）; "
        fi
        finish 9
    fi
fi

# ------------------------------------------------------------ 10
if selected 10; then
    echo "-- 条件 10: 台帳と lint（lint-tests.sh --release、q-index、skipif yuzhu 0 件）"
    if [ ! -x tests/tools/lint-tests.sh ]; then
        missing 10 "tests/tools/lint-tests.sh がない（TS-5a）"
    else
        step 10 lint-tests-self-test tests/tools/lint-tests.sh --self-test
        step 10 lint-tests-release tests/tools/lint-tests.sh --release
        if [ -x tests/tools/q-index.sh ]; then
            step 10 q-index-check tests/tools/q-index.sh --check
        else
            MISSING_STEPS[10]+="tests/tools/q-index.sh がない; "
        fi
        finish 10
    fi
fi

# ------------------------------------------------------------ 11
if selected 11; then
    echo "-- 条件 11: 性能ゲート P1〜P10（baseline.json は M5 の値）"
    if [ ! -x tests/perf/run.sh ]; then
        missing 11 "tests/perf/run.sh がない（TS-4b）"
    else
        extra=(); [ "$QUICK" -eq 1 ] && grep -q -- '--quick' tests/perf/run.sh && extra=(--quick)
        if [ "$PG_ONLY" -eq 1 ]; then echo "  [11] --pg-only のため飛ばす"; else
            step 11 yuzhu-prepare yuzhu_fresh
            if [ -f tests/perf/baseline.json ]; then
                step 11 yuzhu-perf tests/perf/run.sh --target yuzhu --port "$DC_YUZHU_PORT" --compare tests/perf/baseline.json ${extra[@]+"${extra[@]}"}
            else
                FAILED_STEPS[11]+="tests/perf/baseline.json がない; "
            fi
            finish 11
            [ "$QUICK" -eq 1 ] && RESULT[11]="${RESULT[11]}(QUICK)"
        fi
    fi
fi

# ------------------------------------------------------------ 12
if selected 12; then
    echo "-- 条件 12: M4 との統合チェックリスト（§8.6 の A1〜A12。人が確認する）"
    if [ "${DC_M12_CONFIRMED:-0}" = 1 ]; then
        RESULT[12]=PASS; NOTE[12]="DC_M12_CONFIRMED=1（人が確認済み）"
    else
        missing 12 "A1〜A12 と M4 章の突き合わせの確認は人が行う。済んだら DC_M12_CONFIRMED=1 で実行"
    fi
fi

# ------------------------------------------------------------ 13
check13() {
    local rc=0
    grep -q 'M5' tests/README.md 2>/dev/null || { echo "tests/README.md に M5 の記述がない"; rc=1; }
    grep -Eq 'M5-Q1([^0-9]|$)' QUESTIONS.md 2>/dev/null || { echo "QUESTIONS.md に M5-Q1〜（99-questions.md の転記）がない"; rc=1; }
    grep -Eq "M5.*(完了|done)" PROGRESS.md 2>/dev/null || { echo "PROGRESS.md に M5 完了の記述がない"; rc=1; }
    [ -f spec/design/m5-changes.md ] || { echo "spec/design/m5-changes.md がない"; rc=1; }
    # M4 から引き継ぐ確認
    grep -q "99-questions" QUESTIONS.md 2>/dev/null || { echo "QUESTIONS.md に 99-questions の言及がない"; rc=1; }
    grep -Eq "M4.*(完了|done)" PROGRESS.md 2>/dev/null || { echo "PROGRESS.md に M4 完了の記述がない"; rc=1; }
    return "$rc"
}
if selected 13; then
    echo "-- 条件 13: ドキュメント（README.md / QUESTIONS.md / PROGRESS.md / m5-changes.md）"
    step 13 docs check13
    finish 13
fi

# ------------------------------------------------------------ 14（M4 から引き継ぐ: 差分ファジング）
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
if selected 14; then
    if [ "$PG_ONLY" -eq 1 ]; then echo "-- 条件 14: --pg-only のため飛ばす"; else
    echo "-- 条件 14: 差分ファジング（difffuzz。既知の差は tests/tools/difffuzz/known-excludes.txt に書いた針で除外した範囲だけを許す）"
    DT="$(build_tool "$DIFFFUZZ_MANIFEST" difffuzz 2>"$LOG_DIR/c14-build.log")" || DT=""
    if [ -z "$DT" ]; then
        FAILED_STEPS[14]+="build difffuzz（tests/tools/difffuzz がビルドできない）; "
    else
        step 14 pg-prepare pg_fresh
        step 14 yuzhu-prepare yuzhu_fresh
        step 14 fixed-seeds-1-32 fuzz_seeds "$DC_PG_PORT" "$DC_YUZHU_PORT" "$DT" 1 32 200
        if [ "$QUICK" -eq 1 ]; then
            step 14 long-QUICK fuzz_seeds "$DC_PG_PORT" "$DC_YUZHU_PORT" "$DT" 1001 1001 300
        else
            step 14 long-1001-1004 fuzz_seeds "$DC_PG_PORT" "$DC_YUZHU_PORT" "$DT" 1001 1004 10000
        fi
    fi
    finish 14
    [ "$QUICK" -eq 1 ] && RESULT[14]="${RESULT[14]}(QUICK)"
    fi
fi

# ------------------------------------------------------------ 15（M4 から引き継ぐ: EXPLAIN の書式と plan_variants）
if selected 15; then
    echo "-- 条件 15: EXPLAIN の書式と plan_variants"
    files=()
    while IFS= read -r f; do files+=("$f"); done < <(
        { ls tests/slt/m4/explain/format.slt tests/slt/m4/explain/deparse_*.slt 2>/dev/null; find tests/slt/m4/plan_variants -name '*.slt' 2>/dev/null; } | LC_ALL=C sort)
    if [ ${#files[@]} -eq 0 ]; then
        missing 15 "explain/format.slt・deparse_*.slt・plan_variants/*.slt がない"
    else
        [ -f tests/slt/m4/explain/format.slt ] || FAILED_STEPS[15]+="explain/format.slt がない; "
        ls tests/slt/m4/explain/deparse_*.slt >/dev/null 2>&1 || FAILED_STEPS[15]+="explain/deparse_*.slt がない; "
        [ -d tests/slt/m4/plan_variants ] || FAILED_STEPS[15]+="plan_variants/ がない; "
        step 15 pg-prepare pg_fresh
        step 15 pg-explain "${SLT_PG[@]}" "${files[@]}"
        step 15 yuzhu-prepare yuzhu_fresh
        step 15 yuzhu-explain "${SLT_YZ[@]}" "${files[@]}"
        finish 15
    fi
fi

# ------------------------------------------------------------ 一覧
echo
echo "== 結果"
printf '%-4s %-8s %s\n' "条件" "判定" "内容 / 備考"
declare -A TITLE=(
    [1]="cargo fmt / clippy / test、forbid(unsafe)、yuzhu-core 依存、format_version"
    [2]="slt m1〜m5（pg・yuzhu × simple・extended）+ lint"
    [3]="isolation 共有 spec（pg・yuzhu × simple・extended）"
    [4]="isolation ported（variant・skip.txt）"
    [5]="restart / crash（直下・m3・m4・m5。pg・yuzhu）"
    [6]="クラッシュ試験 層 1（W1〜W11、変異 25 件）"
    [7]="クラッシュ試験 層 2（crash_kill9）"
    [8]="並行ストレス S1〜S7"
    [9]="ドライバ・compat・pgbench・SCRAM"
    [10]="台帳と lint（--release、skipif yuzhu 0 件）"
    [11]="性能ゲート P1〜P10"
    [12]="M4 との統合チェックリスト（人が確認）"
    [13]="ドキュメント"
    [14]="差分ランダムテスト（M4 から。シード 1〜32、長時間 1001〜1004）"
    [15]="EXPLAIN の書式・plan_variants（M4 から）"
)
overall=0
for c in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    r="${RESULT[$c]:-SKIPPED}"
    printf '%-4s %-8s %s %s\n' "$c" "$r" "${TITLE[$c]}" "${NOTE[$c]:+- ${NOTE[$c]}}"
    case "$r" in PASS) ;; SKIPPED) ;; *) overall=1 ;; esac
done
echo
if [ "$overall" -eq 0 ] && [ "$QUICK" -eq 0 ] && [ "$PG_ONLY" -eq 0 ] && [ -z "$ONLY$SKIP" ]; then
    echo "M5 完了: 条件 1〜15（M5 の 1〜13 と M4 から引き継ぐ 14・15）がすべて PASS"
elif [ "$overall" -eq 0 ]; then
    echo "選択した条件はすべて PASS（--only / --skip / --quick / --pg-only のため、M5 完了の判定ではない）"
else
    echo "M5 は未完了（FAIL / MISSING の条件がある。ログ: $LOG_DIR）"
fi
exit "$overall"
