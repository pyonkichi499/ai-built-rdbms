#!/usr/bin/env bash
# journal/metrics.md の AUTO:series（推移表）に 1 行追記し、AUTO:slt-by-area を作り直す。
# 使い方: tools/journal-metrics.sh [--label "M2 A 完了後"] [--run-tests] [--dry-run]
#   --run-tests : tests/run.sh で slt と restart を実行して通過数を取る（無ければ前回値を引き継ぐ）
#   --dry-run   : 書き換えずに、更新後の AUTO ブロックを標準出力へ出す
# 冪等: 直前の行と HEAD と状態 ID（git status と git diff のハッシュ）が同じなら追記しない。
# 書き換えるのは journal/metrics.md の AUTO ブロックだけ。時刻はすべて UTC。
set -euo pipefail
export TZ=UTC LC_ALL=C
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
OUT=journal/metrics.md
LABEL="(ラベルなし)"; RUN=0; DRY=0; [ "${JOURNAL_DRY_RUN:-0}" = 1 ] && DRY=1
while [ $# -gt 0 ]; do
  case "$1" in
    --label) LABEL="$2"; shift 2 ;;
    --run-tests) RUN=1; shift ;;
    --dry-run) DRY=1; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [ -f tools/journal-lib.sh ]; then
  # shellcheck source=/dev/null
  . tools/journal-lib.sh
fi
if ! declare -F replace_block >/dev/null; then
  # journal-lib.sh が無いときの代替。replace_block <file> <name> < 本文
  replace_block() {
    local file="$1" name="$2" body tmp; body="$(cat)"; tmp="$(mktemp)"
    local b="<!-- AUTO:$name BEGIN -->" e="<!-- AUTO:$name END -->"
    if [ -f "$file" ] && grep -qF "$b" "$file"; then
      awk -v b="$b" -v e="$e" -v body="$body" '
        $0==b {print; print body; skip=1; next}
        $0==e {skip=0}
        !skip {print}' "$file" > "$tmp"
    else
      { [ -f "$file" ] && cat "$file"; printf '%s\n%s\n%s\n' "$b" "$body" "$e"; } > "$tmp"
    fi
    mv "$tmp" "$file"
  }
fi

# ---- 計測 ----
now="$(date -u '+%Y-%m-%d %H:%M:%S')"
head="$(git rev-parse --short HEAD)"
rs_lines="$(find impl -name '*.rs' -print0 | xargs -0 cat | wc -l)"
tests_n="$(grep -rh '#\[test\]' impl --include='*.rs' | wc -l)"
slt_all="$(find tests -name '*.slt' | wc -l)"
slt_m1="$(find tests/slt/m1 -name '*.slt' 2>/dev/null | wc -l)"
slt_m2="$(find tests/slt/m2 -name '*.slt' 2>/dev/null | wc -l)"
areas_m1="$(find tests/slt/m1 -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)"
areas_m2="$(find tests/slt/m2 -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)"
spec_d="$(cat spec/design/* 2>/dev/null | wc -l)"
spec_r="$(cat spec/research/* 2>/dev/null | wc -l)"
restart_n="$(find tests/restart -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)"
EX=(':!journal' ':!tools/journal-*.sh')   # 日誌自身の変更は状態に含めない
dirty_files="$(git status --short -- . "${EX[@]}" | wc -l)"
untracked_lines="$(git ls-files --others --exclude-standard -z -- . "${EX[@]}" | xargs -0 -r cat 2>/dev/null | wc -l)"
read -r add del < <(git diff HEAD --numstat -- . "${EX[@]}" | awk '{a+=$1;d+=$2}END{print a+0,d+0}')
dirty_lines="+$((add + untracked_lines))/-$del"
stars="$(grep -c '★' QUESTIONS.md || true)"
dirty_id="$( { git status --short -- . "${EX[@]}"; git diff HEAD -- . "${EX[@]}"; } | sha1sum | cut -c1-8)"

# ---- 既存の推移表 ----
existing=""
[ -f "$OUT" ] && existing="$(awk '/<!-- AUTO:series BEGIN -->/{f=1;next}/<!-- AUTO:series END -->/{f=0}f' "$OUT")"
rows="$(printf '%s\n' "$existing" | grep -E '^\| 20' || true)"
last="$(printf '%s\n' "$rows" | tail -n1)"
cell() { printf '%s' "$1" | awk -F'|' -v n="$2" '{gsub(/^ +| +$/,"",$n); print $n}'; }  # 先頭の | を含め n 番目
if [ -n "$last" ] && [ "$(cell "$last" 4)" = "$head" ] && [ "$(cell "$last" 19)" = "$dirty_id" ]; then
  echo "同じ HEAD と状態 ID ($head / $dirty_id) なので追記しない" >&2; new_rows="$rows"
else
  carry() { local v; v="$(cell "$last" "$1")"; [ -z "$v" ] && v="未実行"; case "$v" in *"(前回値)"*|未実行) printf '%s' "$v" ;; *) printf '%s (前回値)' "$v" ;; esac; }
  y_pass="$(carry 11)"; p_pass="$(carry 12)"; r_pass="$(carry 14)"
  if [ "$RUN" = 1 ]; then
    count() { # count <target> <dir>  → 通過/総数（接続できなければ 未計測）
      local ok=0 tot=0 f
      for f in $(find "$2" -name '*.slt' | sort); do
        tot=$((tot + 1)); timeout 180 tests/run.sh --target "$1" "$f" >/dev/null 2>&1 && ok=$((ok + 1))
      done; [ "$ok" = 0 ] && [ "$tot" -gt 0 ] && { echo 未計測; return; }; echo "$ok/$tot"
    }
    y_pass="m1 $(count yuzhu tests/slt/m1), m2 $(count yuzhu tests/slt/m2)"
    if sandbox/pg.sh status >/dev/null 2>&1; then
      p_pass="m1 $(count pg tests/slt/m1), m2 $(count pg tests/slt/m2)"
      if tests/run.sh --target pg --restart >/dev/null 2>&1; then r_pass="pg OK"; else r_pass="pg NG"; fi
    else p_pass="未計測"; fi
  fi
  row="| $now | $LABEL | $head | $rs_lines | $tests_n | $slt_all | $slt_m1 / $slt_m2 | m1:${areas_m1}領域 / m2:${areas_m2}領域 | $spec_d / $spec_r | $y_pass | $p_pass | $restart_n | $r_pass | $dirty_files | $dirty_lines | $stars | 手書き | $dirty_id |"
  new_rows="$(printf '%s\n%s' "$rows" "$row" | sed '/^$/d')"
fi

series="$(printf '%s\n%s\n%s\n' \
'| UTC 時刻 | ラベル | HEAD | Rust 行数 | #[test] 数 | slt 総数 | slt m1 / m2 | 領域数 | spec 行数 design / research | slt 通過 yuzhu | slt 通過 pg | restart 数 | restart 通過 | 未コミット変更ファイル数 | 未コミット行 (+/-) | QUESTIONS ★行数 | 未承認★数 | 状態 ID |' \
'|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|' "$new_rows")"

areas="$( { echo '| 世代 | 領域 | ファイル数 |'; echo '|---|---|---|'
  for g in m1 m2; do for d in tests/slt/$g/*/; do [ -d "$d" ] && echo "| $g | $(basename "$d") | $(find "$d" -name '*.slt' | wc -l) |"; done; done
  echo "| restart | (シナリオ ${restart_n} 件) | $(find tests/restart -name '*.slt' 2>/dev/null | wc -l) |"
  echo "| 計 | 全 .slt | $slt_all |"; echo; echo "計測時刻: $now UTC / HEAD $head"; } )"

if [ "$DRY" = 1 ]; then printf '%s\n\n%s\n' "$series" "$areas"; exit 0; fi
printf '%s\n' "$series" | replace_block "$OUT" series
printf '%s\n' "$areas" | replace_block "$OUT" slt-by-area
echo "updated $OUT"
