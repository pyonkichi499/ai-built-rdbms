#!/usr/bin/env bash
# A8: SCRAM のクライアント間の互換。ここでは libpq（psql）と rust の postgres-protocol（tokio-postgres と同じ実装。authc）の 2 つ。
#     残りの 3 つ（pgJDBC・node-postgres・psycopg 3）は drivers/*/run.sh の C1（SCRAM）が受け持つ
#     （`compat/run.sh --suite drivers --auth scram`）。5 つすべてが通って A8 が成立する。
# A9: チャネルバインディング。TLS なしでは channel_binding=require の接続は失敗する（クライアント側の検査。KD-12）。

a8() {
    ensure_sc || { rec A8 FAIL "SCRAM のサーバを起動できない"; return; }
    local out
    sc_admin -c "create role alice8 login password 'pw8'" >/dev/null || { rec A8 FAIL "ロールを作れない"; return; }
    out="$(PGPASSWORD=pw8 psql -X -Atq -h 127.0.0.1 -p "${INST_PORT[sc]}" -U alice8 -d postgres -c 'select current_user' 2>&1)"
    expect_eq A8-libpq "libpq（psql 17）で SCRAM 接続できる" "alice8" "$out"
    out="$(PGPASSWORD=pw8 psql -X -Atq -h 127.0.0.1 -p "${INST_PORT[sc]}" -U alice8 -d postgres -c 'select 1' 2>&1)"
    expect_eq A8-libpq-simple "libpq の 2 回目の接続" "1" "$out"
    expect_line A8-postgres-protocol "rust の postgres-protocol（tokio-postgres と同じ）で SCRAM 接続できる" "OK scram" "$(sc_connect alice8 postgres pw8)"
    # 長いパスワードと非 ASCII（SASLprep）
    sc_admin -c "create role alice8u login password 'pässwörd-日本語-0123456789-0123456789-0123456789-0123456789'" >/dev/null
    expect_line A8-utf8-password "非 ASCII の長いパスワード（SASLprep）" "OK scram" "$(sc_connect alice8u postgres 'pässwörd-日本語-0123456789-0123456789-0123456789-0123456789')"
    out="$(PGPASSWORD='pässwörd-日本語-0123456789-0123456789-0123456789-0123456789' psql -X -Atq -h 127.0.0.1 -p "${INST_PORT[sc]}" -U alice8u -d postgres -c 'select 1' 2>&1)"
    expect_eq A8-utf8-password-libpq "（libpq でも）" "1" "$out"
}

a9() {
    ensure_sc || { rec A9 FAIL "SCRAM のサーバを起動できない"; return; }
    local out cb
    sc_admin -c "create role alice9 login password 'pw9'" >/dev/null || { rec A9 FAIL "ロールを作れない"; return; }
    for cb in require; do
        out="$(PGPASSWORD=pw9 psql -X -Atq "host=127.0.0.1 port=${INST_PORT[sc]} user=alice9 dbname=postgres sslmode=disable channel_binding=$cb" -c 'select 1' 2>&1)"
        case "$out" in
            *"channel binding required, but SSL not in use"*) rec A9-require ok "TLS なしの channel_binding=require は失敗（クライアント側の検査）" ;;
            *) rec A9-require FAIL "出力: $out" ;;
        esac
    done
    for cb in prefer disable; do
        out="$(PGPASSWORD=pw9 psql -X -Atq "host=127.0.0.1 port=${INST_PORT[sc]} user=alice9 dbname=postgres sslmode=disable channel_binding=$cb" -c 'select 1' 2>&1)"
        expect_eq "A9-$cb" "TLS なしの channel_binding=$cb は接続できる" "1" "$out"
    done
    # sslmode の既定（prefer）では SSLRequest が先に送られ、サーバは 'N' で応じてから SCRAM に進む
    out="$(PGPASSWORD=pw9 psql -X -Atq "host=127.0.0.1 port=${INST_PORT[sc]} user=alice9 dbname=postgres channel_binding=prefer" -c 'select 1' 2>&1)"
    expect_eq A9-sslrequest "SSLRequest に 'N' で応じた後も SCRAM で接続できる" "1" "$out"
}
