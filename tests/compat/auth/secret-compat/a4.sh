#!/usr/bin/env bash
# A4: 秘密の相互互換。
#   (1) 固定のベクトル: PostgreSQL 17 が作った SCRAM の秘密（パスワード "correct horse"）を ALTER ROLE ... PASSWORD で設定して接続できる
#       （TARGET のサーバだけで通る。yuzhu は PostgreSQL の秘密を受け取れる）
#   (2) yuzhu → PostgreSQL: yuzhu で CREATE ROLE x PASSWORD 'pw' → pg_authid.rolpassword（スーパーユーザー）を PostgreSQL に
#       ALTER ROLE x PASSWORD '<値>' で設定して pw で接続できる。(3) 逆（PostgreSQL → yuzhu）。ソルトと反復回数は取り出して比べる。
#   (2)(3) は yuzhu と PostgreSQL の両方を起動する（片方が使えなければ SKIP）。反復回数が既定（4096）でない秘密も通す。

FIXED_SECRET='SCRAM-SHA-256$4096:Ze1Y1SzuZBCqQgykvN5cUQ==$J30nQNgMD8O7izrovNj4Cnw5s281dDscDa9FIE/59GI=:P4iLr/DOvA3CoGizGl2DovxZKkMFDkbJG8iKldZ1Q4Q='
SECRET_RE='^SCRAM-SHA-256\$[0-9]+:[A-Za-z0-9+/]{22}==\$[A-Za-z0-9+/]{43}=:[A-Za-z0-9+/]{43}=$'

secret_of() { inst_admin "$1" -c "select rolpassword from pg_authid where rolname = '$2'"; }
# iterations と salt を取り出す: "<iter> <salt>"
secret_params() { printf '%s' "$1" | sed -E 's/^SCRAM-SHA-256\$([0-9]+):([^$]*)\$.*/\1 \2/'; }
a4_hba_scram() { # <inst>
    write_hba "$WORK/$1.hba" scram-sha-256
    inst_set_hba "$1" "$WORK/$1.hba"
}
a4_conn() { try_connect 127.0.0.1 "${INST_PORT[$1]}" "$2" postgres "$3"; }

a4() {
    local other y p
    # (1) 固定のベクトル
    inst_start a4a "$TARGET" || { rec A4 FAIL "$TARGET のサーバを起動できない"; return; }
    inst_admin a4a -c "create role vec login" -c "alter role vec password '$FIXED_SECRET'" >/dev/null \
        || { rec A4-fixed-vector FAIL "PostgreSQL の秘密を ALTER ROLE で設定できない"; return; }
    expect_eq A4-fixed-vector-stored "設定した秘密がそのまま pg_authid に入る（再暗号化しない）" "$FIXED_SECRET" "$(secret_of a4a vec)"
    a4_hba_scram a4a || { rec A4 FAIL "hba を差し替えて起動できない"; return; }
    expect_line A4-fixed-vector "PostgreSQL が作った秘密で接続できる（パスワード correct horse）" "OK scram" "$(a4_conn a4a vec 'correct horse')"
    expect_line A4-fixed-vector-wrong "誤ったパスワードは 28P01" "ERR 28P01 FATAL password authentication failed for user \"vec\"" "$(a4_conn a4a vec wrong)"

    # (2)(3) 両方のサーバ
    case "$TARGET" in pg) other=yuzhu ;; *) other=pg ;; esac
    other="${AUTH_A4_OTHER:-$other}"   # 試験用: AUTH_A4_OTHER=pg で PostgreSQL 同士を比べる（シナリオ自体の検証）
    if ! kind_available "$other"; then
        rec A4-yuzhu-to-pg SKIP "$other のサーバが使えない（initdb / バイナリが無い、または認証 --hba-file に未対応）"; rec A4-pg-to-yuzhu SKIP "$other のサーバが使えない"; return
    fi
    inst_start a4b "$other" || { rec A4 FAIL "$other のサーバを起動できない"; return; }
    if [ "$TARGET" = yuzhu ]; then y=a4a; p=a4b; else y=a4b; p=a4a; fi
    # 以降 a4a は scram の hba、a4b は trust のまま。接続の確認の前に両方 scram にする。
    a4_hba_scram a4b || { rec A4 FAIL "hba を差し替えて起動できない"; return; }

    local iters s s2 sp sp2
    for iters in 4096 1000; do
        # yuzhu で作る → PostgreSQL に設定
        inst_admin "$y" -c "set scram_iterations = $iters" -c "create role xy$iters login password 'pw-y$iters'" >/dev/null \
            || { rec "A4-yuzhu-to-pg-$iters" FAIL "yuzhu で CREATE ROLE できない"; continue; }
        s="$(secret_of "$y" "xy$iters")"
        if printf '%s' "$s" | grep -Eq "$SECRET_RE" && [ "$(secret_params "$s" | cut -d' ' -f1)" = "$iters" ]; then
            rec "A4-yuzhu-secret-form-$iters" ok "yuzhu の秘密は SCRAM-SHA-256\$$iters:<16 バイトのソルト>\$<StoredKey>:<ServerKey>"
        else
            rec "A4-yuzhu-secret-form-$iters" FAIL "yuzhu の rolpassword の形が違う: $s"
        fi
        inst_admin "$p" -c "create role xy$iters login" -c "alter role xy$iters password '$s'" >/dev/null \
            || { rec "A4-yuzhu-to-pg-$iters" FAIL "PostgreSQL に ALTER ROLE できない"; continue; }
        s2="$(secret_of "$p" "xy$iters")"
        expect_eq "A4-yuzhu-to-pg-params-$iters" "ソルトと反復回数が一致" "$(secret_params "$s")" "$(secret_params "$s2")"
        expect_line "A4-yuzhu-to-pg-$iters" "yuzhu の秘密で PostgreSQL に接続できる（反復 $iters）" "OK scram" "$(a4_conn "$p" "xy$iters" "pw-y$iters")"
        expect_line "A4-yuzhu-to-pg-wrong-$iters" "（誤ったパスワードは 28P01）" "ERR 28P01 FATAL password authentication failed for user \"xy$iters\"" "$(a4_conn "$p" "xy$iters" wrong)"

        # PostgreSQL で作る → yuzhu に設定
        inst_admin "$p" -c "set scram_iterations = $iters" -c "create role xp$iters login password 'pw-p$iters'" >/dev/null \
            || { rec "A4-pg-to-yuzhu-$iters" FAIL "PostgreSQL で CREATE ROLE できない"; continue; }
        s="$(secret_of "$p" "xp$iters")"
        inst_admin "$y" -c "create role xp$iters login" -c "alter role xp$iters password '$s'" >/dev/null \
            || { rec "A4-pg-to-yuzhu-$iters" FAIL "yuzhu に ALTER ROLE できない（PostgreSQL の秘密を受け取れない）"; continue; }
        s2="$(secret_of "$y" "xp$iters")"
        expect_eq "A4-pg-to-yuzhu-stored-$iters" "PostgreSQL の秘密がそのまま保存される（ソルト・反復回数・鍵が同じ）" "$s" "$s2"
        expect_line "A4-pg-to-yuzhu-$iters" "PostgreSQL の秘密で yuzhu に接続できる（反復 $iters）" "OK scram" "$(a4_conn "$y" "xp$iters" "pw-p$iters")"
        expect_line "A4-pg-to-yuzhu-wrong-$iters" "（誤ったパスワードは 28P01）" "ERR 28P01 FATAL password authentication failed for user \"xp$iters\"" "$(a4_conn "$y" "xp$iters" wrong)"
    done

    # 同じパスワードでもソルトは毎回違う（ランダム）
    inst_admin "$y" -c "create role same1 login password 'same'" -c "create role same2 login password 'same'" >/dev/null || true
    sp="$(secret_params "$(secret_of "$y" same1)")"; sp2="$(secret_params "$(secret_of "$y" same2)")"
    if [ -n "$sp" ] && [ "$sp" != "$sp2" ]; then rec A4-yuzhu-salt-random ok "同じパスワードの 2 つのロールでソルトが違う"; else rec A4-yuzhu-salt-random FAIL "ソルトが同じ（または取得できない）: $sp / $sp2"; fi
}
