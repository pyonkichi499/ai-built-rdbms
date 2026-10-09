#!/usr/bin/env bash
# A2: 誤ったパスワードは 28P01 `password authentication failed for user "alice"`（FATAL）。存在しないユーザーでも同じ形のメッセージ
# （偽のソルトで最後まで進む。00 §4.11）。NOLOGIN のロールは正しいパスワードのとき 28000 `role "alice" is not permitted to log in`。

a2() {
    ensure_sc || { rec A2 FAIL "SCRAM のサーバを起動できない"; return; }
    sc_admin \
        -c "create role alice login password 'alice-pw'" \
        -c "create role locked nologin password 'locked-pw'" \
        -c "create role dave login" >/dev/null || { rec A2 FAIL "ロールを作れない"; return; }

    expect_line A2-ok "正しいパスワードで接続できる（SCRAM）" "OK scram" "$(sc_connect alice postgres alice-pw)"
    expect_line A2-wrong-password "誤ったパスワード" 'ERR 28P01 FATAL password authentication failed for user "alice"' "$(sc_connect alice postgres wrong)"
    expect_line A2-unknown-user "存在しないユーザーも同じ形のメッセージ" 'ERR 28P01 FATAL password authentication failed for user "ghost"' "$(sc_connect ghost postgres whatever)"
    expect_line A2-no-password "パスワードの無いロールは同じ形のメッセージ" 'ERR 28P01 FATAL password authentication failed for user "dave"' "$(sc_connect dave postgres whatever)"
    expect_line A2-nologin "NOLOGIN のロール（正しいパスワード）" 'ERR 28000 FATAL role "locked" is not permitted to log in' "$(sc_connect locked postgres locked-pw)"
    expect_line A2-nologin-wrong "NOLOGIN のロール（誤ったパスワード）は先にパスワードで落ちる" 'ERR 28P01 FATAL password authentication failed for user "locked"' "$(sc_connect locked postgres wrong)"
    expect_line A2-unknown-db "存在しないデータベース" 'ERR 3D000 FATAL database "nodb" does not exist' "$(sc_connect alice nodb alice-pw)"
}
