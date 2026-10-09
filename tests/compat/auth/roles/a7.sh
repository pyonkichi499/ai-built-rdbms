#!/usr/bin/env bash
# A7: authentication_timeout。認証の途中で止まったクライアントが、設定の時間（短い値の 2 秒）で切断される（生のソケット）。
#   silent   接続しただけで何も送らない
#   startup  起動パケットを送り、SASL の要求を受け取っても応答しない
#   drip     起動パケットを 100ms に 1 バイトずつ送る（1 バイトごとのタイムアウトでは切れない）
# 3 つとも、設定の時間（2 秒）の前後（1.5〜4.5 秒）で閉じること。そのあと通常の接続ができること。
# yuzhu は認証の途中の期限切れを FATAL 57014 `canceling authentication due to timeout` で通知する（08 §5.5）。PostgreSQL の通知は観測値を記録する。

a7() {
    local secs=2 mode out ms rest
    inst_start to "$TARGET" "$secs" || { rec A7 FAIL "サーバを起動できない"; return; }
    inst_admin to -c "create role alice7 login password 'pw'" >/dev/null || { rec A7 FAIL "ロールを作れない"; return; }
    write_hba "$WORK/to.hba" scram-sha-256
    inst_set_hba to "$WORK/to.hba" || { rec A7 FAIL "hba を差し替えて起動できない"; return; }
    for mode in silent startup drip; do
        out="$("$AUTHC" stall 127.0.0.1 "${INST_PORT[to]}" "$mode" 9000)"
        case "$out" in
            CLOSED\ *)
                ms="$(printf '%s' "$out" | awk '{print $2}')"; rest="${out#CLOSED $ms}"; rest="${rest# }"
                if [ "$ms" -ge 1500 ] && [ "$ms" -le 4500 ]; then
                    rec "A7-$mode" ok "${ms}ms で切断（authentication_timeout=${secs}s）。通知: ${rest:-なし}"
                else
                    rec "A7-$mode" FAIL "切断までの時間が範囲外: ${ms}ms（期待 1500〜4500）"
                fi
                if [ "$TARGET" = yuzhu ] && [ "$mode" = startup ]; then
                    case "$rest" in
                        *"ERR 57014 FATAL canceling authentication due to timeout"*) rec A7-startup-57014 ok "FATAL 57014 canceling authentication due to timeout" ;;
                        *) rec A7-startup-57014 FAIL "通知が 57014 でない:$rest" ;;
                    esac
                fi
                ;;
            *) rec "A7-$mode" FAIL "切断されない（9 秒待った）: $out" ;;
        esac
    done
    expect_line A7-after "タイムアウトの後も通常の接続ができる" "OK scram" "$(try_connect 127.0.0.1 "${INST_PORT[to]}" alice7 postgres pw)"
}
