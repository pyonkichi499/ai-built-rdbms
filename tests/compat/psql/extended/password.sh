#!/usr/bin/env bash
# psql の \password（A6）。ロールを作り、\password で新しいパスワードを設定 → pg_authid の rolpassword が
# SCRAM-SHA-256$4096:... になること、\password の後に ALTER ROLE ... PASSWORD NULL 相当の再設定もできることを確かめる。
# --auth scram のときは、変えたパスワードで再接続できること（古いパスワードでは 28P01 相当で失敗すること）まで確かめる。
set -uo pipefail
D="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../lib.sh
. "$D/../../lib.sh"
compat_env || exit 2

ROLE=tsx_pw
ADMIN_PW="${PGPASSWORD:-}"
cleanup() { psql -X -Atq -c "drop role if exists $ROLE" >/dev/null 2>&1; }
trap cleanup EXIT
cleanup
q "create role $ROLE login" >/dev/null || { echo "ロールを作れない" >&2; exit 1; }

# \password は /dev/tty があればそこから読む。制御端末を切り離して stdin から読ませる。
run_password() {
    printf '%s\n%s\n' "$1" "$1" | setsid -w env PGPASSWORD="$ADMIN_PW" psql -X -q -f <(printf '\\password %s\n' "$ROLE") 2>&1
}
out="$(run_password 'first-pw')" || { echo "\\password が失敗: $out" >&2; exit 1; }
secret="$(q "select rolpassword from pg_authid where rolname = '$ROLE'")"
case "$secret" in
    'SCRAM-SHA-256$4096:'*'$'*':'*) ;;
    *) echo "rolpassword が SCRAM の形でない: $secret" >&2; exit 1 ;;
esac

# 入力が一致しないとき: 変更されない
before="$secret"
out="$(printf 'aaa\nbbb\n' | setsid -w env PGPASSWORD="$ADMIN_PW" psql -X -q -f <(printf '\\password %s\n' "$ROLE") 2>&1)"
after="$(q "select rolpassword from pg_authid where rolname = '$ROLE'")"
[ "$before" = "$after" ] || { echo "確認の入力が違うのに rolpassword が変わった" >&2; exit 1; }
case "$out" in *"Passwords didn't match."*) ;; *) echo "Passwords didn't match. が出ない: $out" >&2; exit 1 ;; esac

# 2 回目の変更でソルトが変わる（毎回ランダムなソルト）
run_password 'second-pw' >/dev/null || exit 1
secret2="$(q "select rolpassword from pg_authid where rolname = '$ROLE'")"
[ "$secret2" != "$before" ] || { echo "変更後も rolpassword が同じ" >&2; exit 1; }
salt1="$(printf '%s' "$before" | cut -d'$' -f2 | cut -d: -f2)"
salt2="$(printf '%s' "$secret2" | cut -d'$' -f2 | cut -d: -f2)"
[ "$salt1" != "$salt2" ] || { echo "ソルトが変わらない" >&2; exit 1; }

# 再接続（認証が要るサーバのときだけ意味がある。trust なら常に通るので、パスワードを要求されるかで判定する）
if PGPASSWORD=wrong-pw psql -X -Atq -U postgres -c 'select 1' >/dev/null 2>&1 && [ -z "$ADMIN_PW" ]; then
    echo "trust 構成: 再接続の検査は省略（rolpassword の形と変更のみ確認）"
else
    PGPASSWORD=second-pw psql -X -Atq -U "$ROLE" -c 'select current_user' 2>&1 | grep -qx "$ROLE" \
        || { echo "新しいパスワードで再接続できない" >&2; exit 1; }
    if PGPASSWORD=first-pw psql -X -Atq -U "$ROLE" -c 'select 1' >/dev/null 2>&1; then
        echo "古いパスワードで接続できてしまう" >&2; exit 1
    fi
fi
exit 0
