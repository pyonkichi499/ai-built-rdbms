#!/usr/bin/env bash
# A3: hba の互換。hba/*.conf を PostgreSQL の pg_hba.conf と yuzhu の yuzhu_hba.conf の同じ内容として使い、
# hba/*.cases の (ユーザー, DB, アドレス, パスワード) → 結果（ok/<要求された認証 | 28000 | 28P01）の表が一致することを確かめる。
# 期待値（.cases）は PostgreSQL 17 で確かめたもの。md5 の行は比べない（KD-12）。bad/*.conf は解析エラー: どちらも起動しない。
# A3 の hba 構成は shell から source される（run.sh）。

a3() {
    local conf base cases line user db addr pw want got host ok ng fails
    inst_start hba "$TARGET" || { rec A3 FAIL "サーバを起動できない"; return; }
    inst_admin hba \
        -c "create role alice login password 'alice-pw'" \
        -c "create role bob login password 'bob-pw'" \
        -c "create role dave login" \
        -c "create role app login password 'app-pw'" \
        -c "create database app" -c "create database other" >/dev/null \
        || { rec A3 FAIL "ロールとデータベースを作れない"; return; }
    for conf in "$AUTH_DIR"/hba/*.conf; do
        base="$(basename "$conf" .conf)"
        if ! inst_set_hba hba "$conf"; then rec "A3-$base" FAIL "この hba でサーバが起動しない"; continue; fi
        ok=0; ng=0; fails=""
        while read -r user db addr pw want; do
            case "$user" in ''|'#'*) continue ;; esac
            got="$(verdict "$(try_connect "$addr" "${INST_PORT[hba]}" "$user" "$db" "$pw")")"
            if [ "$got" = "$want" ]; then ok=$((ok + 1)); else ng=$((ng + 1)); fails="$fails
    ($user, $db, $addr, pw=$pw): 期待 $want 実際 $got"; fi
        done <"${conf%.conf}.cases"
        if [ "$ng" -eq 0 ]; then rec "A3-$base" ok "$ok 件が一致"; else rec "A3-$base" FAIL "$ng 件が不一致（$ok 件は一致）$fails"; fi
    done
    # 解析エラー: 起動しない（クラスタを開く前に検査する）
    ok=0; ng=0; fails=""
    for conf in "$AUTH_DIR"/hba/bad/*.conf; do
        if inst_try_set_hba hba "$conf"; then ng=$((ng + 1)); fails="$fails $(basename "$conf")"; inst_halt hba; else ok=$((ok + 1)); fi
    done
    if [ "$ng" -eq 0 ]; then rec "A3-bad" ok "$ok 件の誤った hba でサーバが起動しない"; else rec "A3-bad" FAIL "誤った hba で起動してしまった:$fails"; fi
    inst_halt hba
}
