#!/usr/bin/env bash
# PostgreSQL 17 の pg_proc から builtin.rs の BuiltinProc 行を生成する（手で写さない）。
#
#   tools/gen_procs.sh [--agg] [OID...]
#
# OID を引数か標準入力（空白・改行区切り）で渡す。--agg のときは builtin.rs の AGGREGATES の OID を使う。
# 接続先: PGHOST（既定 127.0.0.1）、PGPORT（既定 55432）、PGUSER（既定 postgres）、PGDATABASE（既定 postgres）。
# 先に sandbox/pg.sh start で PG17 を起動しておく。出力は oid 昇順。
set -euo pipefail

export PGHOST="${PGHOST:-127.0.0.1}" PGPORT="${PGPORT:-55432}" PGUSER="${PGUSER:-postgres}" PGDATABASE="${PGDATABASE:-postgres}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILTIN="$ROOT/impl/rust/crates/yuzhu-core/src/catalog/builtin.rs"

oids=()
if [ "${1:-}" = "--agg" ]; then
    shift
    while read -r o; do oids+=("$o"); done < <(
        awk '/pub static AGGREGATES/{f=1;next} f&&/^\];/{f=0} f' "$BUILTIN" | sed -n 's/^ *agg(\([0-9]*\),.*/\1/p')
fi
for a in "$@"; do oids+=("$a"); done
if [ ${#oids[@]} -eq 0 ] && [ ! -t 0 ]; then
    while read -r o; do [ -n "$o" ] && oids+=("$o"); done < <(tr -s ' \t' '\n\n')
fi
[ ${#oids[@]} -gt 0 ] || { sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }
for o in "${oids[@]}"; do [[ "$o" =~ ^[0-9]+$ ]] || { echo "invalid oid: $o" >&2; exit 2; }; done
list=$(IFS=,; echo "${oids[*]}")

psql -X -At -F '|' -v ON_ERROR_STOP=1 -c "
SELECT oid, proname, array_to_string(proargtypes::oid[], ','), prorettype, proisstrict, provolatile, proparallel,
       proleakproof, procost, prosrc
FROM pg_proc WHERE oid IN ($list) ORDER BY oid" |
while IFS='|' read -r oid name args ret strict vol par leak cost src; do
    [ "$strict" = t ] && strict=true || strict=false
    [ "$leak" = t ] && leak=true || leak=false
    case "$cost" in *.*) ;; *) cost="$cost.0" ;; esac
    printf '    BuiltinProc { oid: %s, name: "%s", args: &[%s], result: %s, strict: %s, volatility: '"'%s'"', parallel: '"'%s'"', leakproof: %s, cost: %s, prosrc: "%s" },\n' \
        "$oid" "$name" "${args//,/, }" "$ret" "$strict" "$vol" "$par" "$leak" "$cost" "$src"
done
