#!/usr/bin/env bash
# journal/README.md の AUTO ブロック（index, speclines）を更新する。冪等。TZ=Asia/Tokyo。
# 使い方: tools/journal-readme.sh [--dry-run]
set -euo pipefail
export TZ=Asia/Tokyo LC_ALL=C.UTF-8
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
target=journal/README.md
dry=0; [ "${1:-}" = "--dry-run" ] && dry=1

# journal-lib.sh があればそれを使う。無ければ同等の関数を定義する。
if [ -f tools/journal-lib.sh ]; then
  # shellcheck disable=SC1091
  . tools/journal-lib.sh
fi
if ! declare -F replace_block >/dev/null; then
  replace_block() { # <file> <name>  (新しい中身は stdin)
    local f="$1" n="$2" body tmp
    body="$(cat)"; tmp="$(mktemp)"
    if grep -q "<!-- AUTO:$n BEGIN -->" "$f"; then
      awk -v n="$n" -v b="$body" '
        $0 ~ "<!-- AUTO:" n " BEGIN -->" {print; print b; skip=1; next}
        $0 ~ "<!-- AUTO:" n " END -->" {skip=0}
        !skip {print}' "$f" >"$tmp"
    else
      { cat "$f"; printf '\n<!-- AUTO:%s BEGIN -->\n%s\n<!-- AUTO:%s END -->\n' "$n" "$body" "$n"; } >"$tmp"
    fi
    cat "$tmp" >"$f"; rm -f "$tmp"
  }
fi

mtime() { [ -f "$1" ] && date -d "@$(stat -c %Y "$1")" '+%Y-%m-%d %H:%M JST' || echo '未作成'; }
kind() { case "$1" in
  timeline.md|workflows.md|metrics.md) echo '自動 + 手書き';;
  decisions.md|setbacks.md) echo '自動 + 手書き';;
  *) echo '手書き中心';; esac; }

index() {
  echo '| ファイル | 種別 | 最終更新 |'
  echo '|---|---|---|'
  for f in README timeline decisions workflows setbacks metrics retrospective snapshots; do
    printf '| `journal/%s.md` | %s | %s |\n' "$f" "$(kind "$f.md")" "$(mtime "journal/$f.md")"
  done
}

speclines() {
  echo '| ファイル | 行数 | バイト |'
  echo '|---|---:|---:|'
  for f in spec/design/*.md spec/research/*.md; do
    printf '| `%s` | %s | %s |\n' "$f" "$(wc -l <"$f")" "$(wc -c <"$f")"
  done
}

if [ "$dry" = 1 ]; then
  echo '--- index ---'; index; echo '--- speclines ---'; speclines; exit 0
fi
[ -f "$target" ] || { echo "$target が無い" >&2; exit 1; }
index | replace_block "$target" index
speclines | replace_block "$target" speclines
