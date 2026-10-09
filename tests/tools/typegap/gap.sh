#!/usr/bin/env bash
# numeric / bpchar の関数・演算子・キャストの差を PG17 の一覧と比べる（m5/05 §5.2）。
#
#   tests/tools/typegap/gap.sh --target pg|yuzhu [--port N] [--type numeric|bpchar]   一覧を標準出力へ
#   tests/tools/typegap/gap.sh --check [--port N]   yuzhu の一覧が pg17_<type>.tsv から expected_missing_<type>.txt を引いた集合と一致するか
#
# 一覧は kind と id で比べる（キャストは (castsource>casttarget) が鍵）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET=""; PORT=""; TYPE=""; CHECK=0
while [ $# -gt 0 ]; do
    case "$1" in
        --target) TARGET="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --type) TYPE="$2"; shift 2 ;;
        --check) CHECK=1; shift ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
oid_of() { case "$1" in numeric) echo 1700 ;; bpchar) echo 1042 ;; esac; }
dump() { # type port
    sed "s/@OID@/$(oid_of "$1")/g" "$DIR/query.sql.tpl" |
        psql -h 127.0.0.1 -p "$2" -U postgres -AtF$'\t' -f - | cut -f1,2 | sort
}
if [ "$CHECK" = 1 ]; then
    rc=0
    for t in ${TYPE:-numeric bpchar}; do
        ycut=$(mktemp); exp=$(mktemp)
        dump "$t" "${PORT:-5432}" > "$ycut"
        cut -f1,2 "$DIR/pg17_$t.tsv" | sort | comm -23 - "$ycut" | sort > "$exp.raw"
        grep -v '^#' "$DIR/expected_missing_$t.txt" | cut -f1,2 | sort > "$exp"
        if ! diff -u "$exp" "$exp.raw"; then echo "typegap: $t differs" >&2; rc=1; fi
        rm -f "$ycut" "$exp" "$exp.raw"
    done
    exit $rc
fi
[ -n "$TARGET" ] && [ -n "$TYPE" ] || { echo "need --target and --type" >&2; exit 2; }
case "$TARGET" in
    pg) dump "$TYPE" "${PORT:-55432}" ;;
    yuzhu) dump "$TYPE" "${PORT:-5432}" ;;
esac
