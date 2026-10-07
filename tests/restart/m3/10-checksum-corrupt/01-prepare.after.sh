#!/usr/bin/env bash
# サーバが止まっている間に、cr10_bad のヒープ（目印の値を含むリレーションファイル）の
# ブロック 0 の中ほどを 0xFF で上書きして、ページチェックサムを壊す。
# 環境変数 YUZHU_DATA がデータディレクトリ。
set -euo pipefail
: "${YUZHU_DATA:?}"
# 先のシナリオのテーブルが残っていることがあるので、目印の値 cr10badmark を含む最新のファイルを選ぶ
target="$(grep -rl --binary-files=text 'cr10badmark' "$YUZHU_DATA/base" 2>/dev/null \
    | grep -E '/[0-9]+$' | xargs -r ls -t 2>/dev/null | head -n 1 || true)"
[ -n "$target" ] || { echo "no user relation file found under $YUZHU_DATA/base" >&2; exit 1; }
echo "corrupting $target (block 0, offset 4000)"
printf '\xff\xff\xff\xff\xff\xff\xff\xff' | dd of="$target" bs=1 seek=4000 conv=notrunc status=none
