#!/usr/bin/env bash
# journal/snapshots.md の AUTO:status ブロック（git status と git log、UTC）だけを更新する。冪等。
# 使い方: tools/journal-snapshots.sh [--dry-run]
set -euo pipefail
export TZ=UTC
root="$(cd "$(dirname "$0")/.." && pwd)"
file="$root/journal/snapshots.md"
dry=0; [ "${1:-}" = "--dry-run" ] && dry=1
cd "$root"

body() {
  echo "取得: $(date -u '+%Y-%m-%d %H:%M:%S') UTC / HEAD $(git rev-parse --short HEAD) / ブランチ $(git branch --show-current)"
  echo
  echo "git status --short: $(git status --short | wc -l) 件"
  echo
  echo '```text'
  git status --short | awk '{print $2}' | sed -E 's#^(impl/rust/crates/[^/]+/src/[^/]+).*#\1#' | sort | uniq -c | sort -rn | head -15
  echo '```'
  echo
  echo '```text'
  git status --short
  echo '```'
  echo
  echo 'git log -n 5（UTC）:'
  echo
  echo '```text'
  git log -n 5 --format='%h %ad %s' --date=format-local:'%Y-%m-%d %H:%M:%S'
  echo '```'
}

new="$(body)"
if [ "$dry" = 1 ]; then echo "$new"; exit 0; fi
[ -f "$file" ] || { echo "$file が無い" >&2; exit 1; }

b='<!-- AUTO:status BEGIN -->'; e='<!-- AUTO:status END -->'
tmp="$(mktemp)"
if grep -qF "$b" "$file"; then
  awk -v b="$b" -v e="$e" -v n="$new" '
    $0==b {print; print n; skip=1; next}
    $0==e {skip=0}
    !skip {print}' "$file" > "$tmp"
else
  { cat "$file"; echo; echo "$b"; echo "$new"; echo "$e"; } > "$tmp"
fi
cat "$tmp" > "$file"; rm -f "$tmp"
