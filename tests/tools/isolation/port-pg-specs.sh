#!/usr/bin/env bash
# PostgreSQL 本体の isolation spec を REL_17_STABLE から取得し、tests/isolation/ported/ に置く（設計 10 §6.4）。
#
#   tests/tools/isolation/port-pg-specs.sh [--port N] [--host H] [--accept] [--check-only]
#
# - 取得するのは PORT_SPECS の spec（specs/<name>.spec）と、参照用の上流の期待（$WORK/upstream/ に置くだけで、リポジトリには入れない）。
# - UPSTREAM に commit SHA と取得日（JST）を書く。spec の先頭に PostgreSQL License のヘッダを付ける。
# - --accept: 取得した spec を PostgreSQL 17 に流し、出力を expected/<name>.out に書く（上流の期待との差は diff で表示）。
# - skip.txt は SKIP_RULES から生成する。上流の isolation_schedule に存在しない名前・どの分類にも入らない名前は報告する（U-3）。
# ネットワークが必要。事前に PostgreSQL 17 を起動し `createdb isolation_regression` しておくこと。
set -euo pipefail

HOST=127.0.0.1
PORT=55432
ACCEPT=0
CHECK_ONLY=0
while [ $# -gt 0 ]; do
    case "$1" in
        --host) HOST="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --accept) ACCEPT=1; shift ;;
        --check-only) CHECK_ONLY=1; shift ;;
        -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$(cd "$HERE/../../isolation" && pwd)/ported"
WORK="${WORK:-$(mktemp -d)}"
REF=REL_17_STABLE
RAW=https://raw.githubusercontent.com/postgres/postgres
BASE="$RAW/$REF/src/test/isolation"

# 移植して PostgreSQL と yuzhu の両方で流す spec（分類は完全一致か variant。yuzhu 側の確定は TS-1b）
PORT_SPECS=(
    deadlock-simple deadlock-hard deadlock-soft deadlock-soft-2
    skip-locked skip-locked-2 skip-locked-3
    nowait nowait-2 nowait-3
    lock-nowait lock-update-traversal update-locked-tuple
    fk-contention fk-deadlock fk-deadlock2
    multixact-no-forget timeouts
)

# skip 規則: 「名前（シェルのパターン）|KD|理由」。上流の isolation_schedule の名前に当てはめて skip.txt を作る。
# 上から順に最初に合致したものを採用する。PORT_SPECS に入っている名前には当てはめない。
SKIP_RULES=(
    'read-only-anomaly*|KD-3|SERIALIZABLE（SSI）が前提'
    'read-write-unique*|KD-3|BEGIN ISOLATION LEVEL SERIALIZABLE を使う'
    'simple-write-skew|KD-3|SERIALIZABLE（SSI）が前提'
    'receipt-report|KD-3|SERIALIZABLE（SSI）が前提'
    'temporal-range-integrity|KD-3|SERIALIZABLE（SSI）が前提'
    'project-manager|KD-3|SERIALIZABLE（SSI）が前提'
    'classroom-scheduling|KD-3|SERIALIZABLE（SSI）が前提'
    'total-cash|KD-3|SERIALIZABLE（SSI）が前提'
    'referential-integrity|KD-3|SERIALIZABLE（SSI）が前提'
    'ri-trigger|KD-3|SERIALIZABLE（SSI）が前提'
    'two-ids|KD-3|SERIALIZABLE（SSI）が前提'
    'multiple-row-versions|KD-3|SERIALIZABLE（SSI）が前提'
    'predicate-*|KD-3|述語ロック・SSI が前提'
    'serializable-parallel*|KD-3|SERIALIZABLE（SSI）と並列クエリが前提'
    'update-conflict-out|KD-3|SERIALIZABLE（SSI）が前提'
    'partial-index|KD-3|SERIALIZABLE（SSI）が前提'
    'matview-write-skew|KD-3|SERIALIZABLE と マテリアライズドビューが前提'
    'subxid-overflow|KD-4|サブトランザクション（SAVEPOINT・例外ブロック）が前提'
    'delete-abort-savept*|KD-4|SAVEPOINT が前提'
    'aborted-keyrevoke|KD-4|SAVEPOINT が前提'
    'propagate-lock-delete|KD-4|SAVEPOINT が前提'
    'multixact-no-deadlock|KD-4|SAVEPOINT が前提'
    'tuplelock-conflict|KD-4|SAVEPOINT が前提（MultiXact の版も KD-1）'
    'tuplelock-upgrade-no-deadlock|KD-4|SAVEPOINT が前提（KD-1 も）'
    'tuplelock-update|KD-19|勧告ロック（pg_advisory_lock）を使う'
    'tuplelock-partition|KD-28|パーティションが前提'
    'skip-locked-4|KD-19|勧告ロックを使う'
    'nowait-4|KD-19|勧告ロックを使う'
    'nowait-5|KD-19|勧告ロックを使う（ctid・xmax・PREPARE も）'
    'lock-update-delete|KD-19|勧告ロックを使う'
    'lock-committed-*|KD-19|勧告ロックと SERIALIZABLE を使う'
    'eval-plan-qual|KD-25|MERGE・パーティション・ALTER TABLE・PL/pgSQL 関数を使う（単一テーブルの部分は共有の epq-single、TS-1b）'
    'eval-plan-qual-trigger|KD-2|トリガが前提'
    'deadlock-parallel|KD-28|並列クエリが前提'
    'fk-snapshot|KD-28|パーティションと DEFERRABLE（KD-16）を使う'
    'fk-partitioned-*|KD-28|パーティションが前提'
    'vacuum-concurrent-drop|KD-28|パーティションが前提（VACUUM / ANALYZE の対象指定は KD-8）'
    'vacuum-skip-locked|KD-28|パーティションと ALTER TABLE を使う'
    'vacuum-no-cleanup-lock|KD-8|VACUUM の内部動作（MultiXact・ページのクリーンアップロック）が前提'
    'vacuum-conflict|KD-8|VACUUM のオプションと内部動作が前提'
    'freeze-the-dead|KD-8|VACUUM FREEZE と pageinspect が前提'
    'index-only-scan|KD-8|ビジビリティマップが前提'
    'horizons|KD-8|VACUUM と統計の内部動作が前提'
    'stats|KD-18|pg_stat_* が前提'
    'insert-conflict-*|KD-25|ON CONFLICT が前提'
    'merge-*|KD-25|MERGE が前提'
    'alter-table-*|KD-26|ALTER TABLE の大半が前提'
    'drop-index-concurrently-*|KD-26|DROP INDEX CONCURRENTLY が前提'
    'reindex-*|KD-26|REINDEX CONCURRENTLY・スキーマが前提'
    'multiple-cic|KD-26|CREATE INDEX CONCURRENTLY が前提'
    'detach-partition-concurrently-*|KD-28|パーティションが前提'
    'partition-*|KD-28|パーティションが前提'
    'cluster-conflict*|KD-26|CLUSTER が前提'
    'inherit-temp|KD-28|継承が前提'
    'temp-schema-cleanup|KD-27|スキーマ（一時スキーマ）が前提'
    'intra-grant-inplace*|KD-23|GRANT が前提'
    'inplace-inval|KD-26|インプレース更新・ALTER TABLE が前提'
    'truncate-conflict|KD-23|ロールと GRANT が前提'
    'drop-owned-grant|KD-23|DROP OWNED・GRANT が前提'
    'ddl-dependency-locking|KD-27|スキーマ・関数・型の依存が前提'
    'tablespace-dependency-locking|KD-27|テーブルスペース・スキーマが前提'
    'pub-concurrent-drop|KD-27|パブリケーションが前提'
    'create-trigger|KD-2|トリガが前提'
    'sequence-ddl|KD-26|ALTER SEQUENCE が前提'
    'plpgsql-toast|KD-27|PL/pgSQL・TOAST が前提'
    'async-notify|KD-24|LISTEN / NOTIFY が前提'
)

echo "取得元: $RAW/$REF"
SHA="$(curl -sfL -H 'Accept: application/vnd.github.sha' "https://api.github.com/repos/postgres/postgres/commits/$REF")"
[ -n "$SHA" ] || { echo "commit SHA を取得できない（ネットワーク）" >&2; exit 1; }
curl -sfL -o "$WORK/isolation_schedule" "$BASE/isolation_schedule"
mapfile -t UPSTREAM_TESTS < <(awk '/^test:/ { for (i = 2; i <= NF; i++) print $i }' "$WORK/isolation_schedule")

in_array() { local x="$1"; shift; for y in "$@"; do [ "$x" = "$y" ] && return 0; done; return 1; }

# 存在確認（U-3）
for t in "${PORT_SPECS[@]}"; do
    in_array "$t" "${UPSTREAM_TESTS[@]}" || { echo "上流に存在しない: $t" >&2; exit 1; }
done

# skip.txt の生成と、未分類の報告
SKIP_TXT="$WORK/skip.txt"
{
    echo "# 移植しない PostgreSQL 本体の spec。形式: <name> <KD-n> <理由>（設計 10 §6.4）"
    echo "# port-pg-specs.sh が生成する（規則は同スクリプトの SKIP_RULES）。手で編集しない。"
} > "$SKIP_TXT"
UNCLASSIFIED=()
for t in "${UPSTREAM_TESTS[@]}"; do
    in_array "$t" "${PORT_SPECS[@]}" && continue
    hit=0
    for r in "${SKIP_RULES[@]}"; do
        IFS='|' read -r pat kd why <<<"$r"
        # shellcheck disable=SC2254
        case "$t" in $pat) echo "$t $kd $why" >> "$SKIP_TXT"; hit=1; break ;; esac
    done
    [ $hit = 1 ] || UNCLASSIFIED+=("$t")
done
if [ ${#UNCLASSIFIED[@]} -gt 0 ]; then
    echo "未分類（SKIP_RULES か PORT_SPECS に足すこと）: ${UNCLASSIFIED[*]}" >&2
    exit 1
fi
[ $CHECK_ONLY = 1 ] && { echo "分類は完全（移植 ${#PORT_SPECS[@]} 本、skip $(($(wc -l < "$SKIP_TXT") - 2)) 本）"; exit 0; }

# spec の取得
mkdir -p "$OUT/specs" "$OUT/expected" "$WORK/upstream"
for t in "${PORT_SPECS[@]}"; do
    curl -sfL -o "$WORK/upstream/$t.spec" "$BASE/specs/$t.spec"
    for suffix in "" _1 _2 _3; do
        curl -sfL -o "$WORK/upstream/$t$suffix.out" "$BASE/expected/$t$suffix.out" || rm -f "$WORK/upstream/$t$suffix.out"
    done
    {
        echo "# PostgreSQL の src/test/isolation/specs/$t.spec（$REF、commit ${SHA:0:12}）。"
        echo "# Portions Copyright (c) 1996-2025, PostgreSQL Global Development Group"
        echo "# Portions Copyright (c) 1994, The Regents of the University of California"
        echo "# PostgreSQL License: https://www.postgresql.org/about/licence/"
        echo
        cat "$WORK/upstream/$t.spec"
    } > "$OUT/specs/$t.spec"
done
cp "$SKIP_TXT" "$OUT/skip.txt"
{
    echo "repository: https://github.com/postgres/postgres"
    echo "ref: $REF"
    echo "commit: $SHA"
    echo "fetched: $(TZ=Asia/Tokyo date '+%Y-%m-%d %H:%M JST')"
    echo "license: PostgreSQL License (https://www.postgresql.org/about/licence/)"
    echo "specs: ${PORT_SPECS[*]}"
} > "$OUT/UPSTREAM"

if [ $ACCEPT = 1 ]; then
    cargo build --release --quiet --manifest-path "$HERE/Cargo.toml"
    BIN="${CARGO_TARGET_DIR:-$HERE/target}/release/yuzhu-isolation"
    SPECS=(); for t in "${PORT_SPECS[@]}"; do SPECS+=("$OUT/specs/$t.spec"); done
    # 上流の期待は pg_regress と同じ条件（datestyle）で生成されているので同じ設定で流す。
    "$BIN" --host "$HOST" --port "$PORT" --dbname isolation_regression \
        --set 'datestyle=Postgres, MDY' --accept "${SPECS[@]}" || true
    echo "== 上流の期待との差（レビュー用）"
    for t in "${PORT_SPECS[@]}"; do
        up="$WORK/upstream/$t.out"
        if [ -f "$up" ] && ! diff -q "$up" "$OUT/expected/$t.out" >/dev/null; then
            echo "--- $t"; diff -u "$up" "$OUT/expected/$t.out" || true
        fi
    done
fi
echo "完了。作業ディレクトリ: $WORK"
