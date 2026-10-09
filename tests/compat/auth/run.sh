#!/usr/bin/env bash
# 認証の互換テスト A2〜A9（10 §6.10.4）。compat/run.sh --suite auth から呼ばれる（TARGET / PORT / HOST は export 済み）。
#
#   tests/compat/auth/run.sh [A2 A3 ...]      単独実行: TARGET=pg|yuzhu tests/compat/auth/run.sh
#   環境変数 AUTH_ONLY="A2 A3 A4"（流すシナリオを限る。引数でも可）、AUTH_KEEP=1、AUTH_PORT_BASE
#
# シナリオは専用のサーバを起動して流す（共有サーバの認証方式には依存しない。PORT は使わない）。
# A1（slt などを SCRAM で流す）は `--auth scram` の各スイートが受け持つ。
# 出力: `RESULT <ID> <ok|FAIL|SKIP> <詳細>` と最後の表。失敗が 1 つでもあれば終了コード 1。
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
: "${TARGET:?TARGET（pg|yuzhu）が未設定}"
case "$TARGET" in pg|yuzhu) ;; *) echo "unknown target: $TARGET" >&2; exit 2 ;; esac
export PSQLRC=/dev/null PGCONNECT_TIMEOUT=10
unset PGOPTIONS PGPASSWORD PGHOST PGPORT PGUSER PGDATABASE

# shellcheck source=lib.sh
. "$HERE/lib.sh"
for f in hba/a3.sh secret-compat/a4.sh roles/a2.sh roles/a5.sh roles/a6.sh roles/a7.sh roles/a8_a9.sh; do
    # shellcheck disable=SC1090
    . "$HERE/$f"
done

ONLY="${AUTH_ONLY:-$*}"
want() { [ -z "$ONLY" ] || [[ " $ONLY " == *" $1 "* ]]; }

build_client || { echo "FAIL authc（認証のテスト用クライアント）をビルドできない" >&2; exit 1; }
kind_available "$TARGET" || { rec AUTH FAIL "$TARGET のサーバ（initdb / バイナリ）が見つからない"; ONLY="none"; }

want A2 && a2
want A3 && a3
want A4 && a4
want A5 && a5
want A6 && a6
want A7 && a7
want A8 && a8
want A9 && a9

echo
printf '%-22s %-5s %s\n' シナリオID 結果 詳細
ok=0; ng=0; sk=0
for r in "${RESULTS[@]:-}"; do
    [ -n "$r" ] || continue
    IFS='|' read -r id st detail <<<"$r"
    printf '%-22s %-5s %s\n' "$id" "$st" "$(printf '%s' "$detail" | head -n 1)"
    case "$st" in ok) ok=$((ok + 1)) ;; SKIP) sk=$((sk + 1)) ;; *) ng=$((ng + 1)) ;; esac
done
echo "成功 $ok / 失敗 $ng / 省略 $sk"
exit "$FAIL"
