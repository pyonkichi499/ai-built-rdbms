#!/usr/bin/env bash
# サーバが止まっている間に、最初のユーザーテーブル（OID 16384 以上で最小のリレーションファイル）の
# ブロック 0 の中ほどを 0xFF で上書きして、ページチェックサムを壊す。
# 環境変数 YUZHU_DATA がデータディレクトリ。
set -euo pipefail
: "${YUZHU_DATA:?}"
target="$(find "$YUZHU_DATA/base" -type f -regex '.*/[0-9]+' -size +0 2>/dev/null \
    | awk -F/ '$NF >= 16384 { print $NF, $0 }' | sort -n | head -n 1 | cut -d' ' -f2-)"
[ -n "$target" ] || { echo "no user relation file found under $YUZHU_DATA/base" >&2; exit 1; }
echo "corrupting $target (block 0, offset 4000)"
printf '\xff\xff\xff\xff\xff\xff\xff\xff' | dd of="$target" bs=1 seek=4000 conv=notrunc status=none
