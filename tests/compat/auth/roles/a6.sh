#!/usr/bin/env bash
# A6: psql の \password で変更 → 再接続。管理者が他のロールの分を変える（\password ROLE）場合と、本人が自分の分を変える（\password）場合。
# libpq の PQchangePassword は SHOW password_encryption と ParameterStatus の scram_iterations を使って SCRAM の秘密を作り、
# ALTER USER ... PASSWORD 'SCRAM-SHA-256$...' で送る（AU-D11）。

# psql_password <ロール名 | 空> <現在のパスワード> <ユーザー> <新しいパスワード> [<2 回目の入力>]
# 制御端末を切り離して stdin から入力させる（\password は /dev/tty があればそこから読む）。
psql_password() {
    local target="$1" curpw="$2" user="$3" newpw="$4" again="${5:-$4}"
    printf '%s\n%s\n' "$newpw" "$again" | setsid -w env PGPASSWORD="$curpw" \
        psql -X -q -h 127.0.0.1 -p "${INST_PORT[sc]}" -U "$user" -d postgres -f <(printf '\\password %s\n' "$target") 2>&1
}

a6() {
    ensure_sc || { rec A6 FAIL "SCRAM のサーバを起動できない"; return; }
    local out secret
    sc_admin -c "create role alice6 login" >/dev/null || { rec A6 FAIL "ロールを作れない"; return; }

    out="$(psql_password alice6 "$ADMIN_PW" postgres first-pw)" || { rec A6-admin-sets FAIL "\\password alice6 が失敗: $out"; return; }
    secret="$(sc_admin -c "select rolpassword from pg_authid where rolname = 'alice6'")"
    case "$secret" in
        'SCRAM-SHA-256$4096:'*) rec A6-admin-sets ok "管理者の \\password alice6 が SCRAM（4096）の秘密を保存した" ;;
        *) rec A6-admin-sets FAIL "rolpassword: $secret（出力: $out）" ;;
    esac
    expect_line A6-reconnect "変更したパスワードで再接続（authc）" "OK scram" "$(sc_connect alice6 postgres first-pw)"
    expect_line A6-reconnect-old "誤ったパスワードでは接続できない" 'ERR 28P01 FATAL password authentication failed for user "alice6"' "$(sc_connect alice6 postgres wrong)"
    expect_eq A6-reconnect-psql "変更したパスワードで再接続（psql）" "alice6" \
        "$(PGPASSWORD=first-pw psql -X -Atq -h 127.0.0.1 -p "${INST_PORT[sc]}" -U alice6 -d postgres -c 'select current_user' 2>&1)"

    out="$(psql_password '' first-pw alice6 second-pw)" || { rec A6-self FAIL "\\password（本人）が失敗: $out"; return; }
    expect_line A6-self-new "本人の \\password で変えたパスワードで接続できる" "OK scram" "$(sc_connect alice6 postgres second-pw)"
    expect_line A6-self-old "変える前のパスワードは使えない" 'ERR 28P01 FATAL password authentication failed for user "alice6"' "$(sc_connect alice6 postgres first-pw)"

    # 確認の入力が違えば変更しない
    out="$(psql_password '' second-pw alice6 aaa bbb)"
    case "$out" in *"Passwords didn't match."*) rec A6-mismatch ok "確認が違えば Passwords didn't match." ;; *) rec A6-mismatch FAIL "出力: $out" ;; esac
    expect_line A6-mismatch-unchanged "（パスワードは変わらない）" "OK scram" "$(sc_connect alice6 postgres second-pw)"
}
