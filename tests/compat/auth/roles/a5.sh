#!/usr/bin/env bash
# A5: VALID UNTIL（期限切れは 28P01）、CONNECTION LIMIT（53300）、ALTER ROLE ... PASSWORD NULL（SCRAM が使えない → 28P01）、
# password 方式（平文要求 R 3）、scram_iterations の変更（既存の秘密は影響を受けない）。

a5() {
    ensure_sc || { rec A5 FAIL "SCRAM のサーバを起動できない"; return; }
    local pw_alice_before pw_alice_after secret sql_limit holder
    sc_admin \
        -c "create role expired login password 'pw' valid until '2000-01-01 00:00:00+00'" \
        -c "create role future login password 'pw' valid until '2099-01-01 00:00:00+00'" \
        -c "create role infinite login password 'pw' valid until 'infinity'" \
        -c "create role nullified login password 'pw'" \
        -c "create role limited login password 'limited-pw' connection limit 1" \
        -c "create role alice5 login password 'pw'" >/dev/null || { rec A5 FAIL "ロールを作れない"; return; }

    expect_line A5-valid-until-expired "期限切れは 28P01" 'ERR 28P01 FATAL password authentication failed for user "expired"' "$(sc_connect expired postgres pw)"
    expect_line A5-valid-until-future "期限が先なら接続できる" "OK scram" "$(sc_connect future postgres pw)"
    expect_line A5-valid-until-infinity "infinity は期限なし" "OK scram" "$(sc_connect infinite postgres pw)"
    sc_admin -c "alter role expired valid until '2099-01-01 00:00:00+00'" >/dev/null
    expect_line A5-valid-until-extended "期限を延ばせば接続できる" "OK scram" "$(sc_connect expired postgres pw)"

    expect_line A5-password-before-null "PASSWORD NULL の前は接続できる" "OK scram" "$(sc_connect nullified postgres pw)"
    sc_admin -c "alter role nullified password null" >/dev/null
    expect_line A5-password-null "PASSWORD NULL の後は SCRAM が使えない（28P01）" 'ERR 28P01 FATAL password authentication failed for user "nullified"' "$(sc_connect nullified postgres pw)"
    expect_eq A5-password-null-catalog "pg_authid.rolpassword が NULL" "" "$(sc_admin -c "select coalesce(rolpassword, '') from pg_authid where rolname = 'nullified'")"

    # CONNECTION LIMIT: 1 つ目を握ったまま 2 つ目 → 53300（認証の後）
    ( echo "select 1"; sleep 8 ) | PGPASSWORD=limited-pw psql -X -q -h 127.0.0.1 -p "${INST_PORT[sc]}" -U limited -d postgres >/dev/null 2>&1 &
    holder=$!
    sleep 1.5
    expect_line A5-connection-limit "CONNECTION LIMIT 1 の 2 つ目の接続" 'ERR 53300 FATAL too many connections for role "limited"' "$(sc_connect limited postgres limited-pw)"
    # 他のロールには影響しない
    expect_line A5-connection-limit-other "他のロールは接続できる" "OK scram" "$(sc_connect future postgres pw)"
    kill "$holder" 2>/dev/null; wait "$holder" 2>/dev/null
    sleep 1
    expect_line A5-connection-limit-released "切断すると再び接続できる" "OK scram" "$(sc_connect limited postgres limited-pw)"

    # scram_iterations: 既存の秘密は変わらず、新しい秘密は指定の反復回数
    pw_alice_before="$(sc_admin -c "select rolpassword from pg_authid where rolname = 'alice5'")"
    sc_admin -c "set scram_iterations = 1000" -c "create role it1000 login password 'pw'" >/dev/null
    secret="$(sc_admin -c "select rolpassword from pg_authid where rolname = 'it1000'")"
    case "$secret" in 'SCRAM-SHA-256$1000:'*) rec A5-scram-iterations ok "scram_iterations=1000 で作った秘密は反復回数 1000" ;; *) rec A5-scram-iterations FAIL "rolpassword: $secret" ;; esac
    expect_line A5-scram-iterations-login "反復回数 1000 の秘密でも接続できる" "OK scram" "$(sc_connect it1000 postgres pw)"
    pw_alice_after="$(sc_admin -c "select rolpassword from pg_authid where rolname = 'alice5'")"
    expect_eq A5-scram-iterations-existing "既存の秘密は変わらない（4096 のまま）" "$pw_alice_before" "$pw_alice_after"
    case "$pw_alice_after" in 'SCRAM-SHA-256$4096:'*) rec A5-scram-iterations-default ok "既定は 4096" ;; *) rec A5-scram-iterations-default FAIL "$pw_alice_after" ;; esac
    expect_line A5-scram-iterations-existing-login "既存の秘密でも接続できる" "OK scram" "$(sc_connect alice5 postgres pw)"
    expect_eq A5-scram-iterations-param "ParameterStatus scram_iterations=4096" "PARAM scram_iterations=4096" \
        "$("$AUTHC" connect 127.0.0.1 "${INST_PORT[sc]}" future postgres --password pw | grep '^PARAM scram_iterations=')"

    # password 方式（平文要求 R 3）。SCRAM の秘密に対して検証する
    write_hba "$WORK/sc-password.hba" password
    inst_set_hba sc "$WORK/sc-password.hba" || { rec A5-password-method FAIL "hba を差し替えて起動できない"; return; }
    expect_line A5-password-method "password 方式は平文を要求する（R 3）" "OK cleartext" "$(sc_connect future postgres pw)"
    expect_line A5-password-method-wrong "password 方式の誤ったパスワード" 'ERR 28P01 FATAL password authentication failed for user "future"' "$(sc_connect future postgres wrong)"
    expect_line A5-password-method-null "password 方式でパスワードの無いロール" 'ERR 28P01 FATAL password authentication failed for user "nullified"' "$(sc_connect nullified postgres pw)"
    inst_set_hba_prepared_sc || rec A5 FAIL "hba を元に戻せない"
}
