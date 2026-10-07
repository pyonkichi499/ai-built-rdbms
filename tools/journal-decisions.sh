#!/usr/bin/env bash
# journal-decisions.sh: journal/decisions.md の自動節を更新する。
#
# 更新するブロック（<!-- AUTO:名前 BEGIN/END --> の間だけ。手書き欄、判断の状態列、追認状況には触れない）:
#   qindex      QUESTIONS.md の Q 番号、見出し、★、行番号、初出コミット（git log -S）
#   dtable      spec/design/*.md の D 表（grep -n '^| D[0-9]* '）
#   unverified  「未検証」の出現数（spec/design と spec/research のファイルごと）と前回値との差分
#   deps        Cargo.toml / Cargo.lock の追加依存（git log -p と未コミットの git diff HEAD）
#   pgdiff      「PG との差」「PostgreSQL と違」の該当行（ファイル:行番号）
#   supersede   「改訂」「前倒し」の注記行（ファイル:行番号）
#
# 使い方: tools/journal-decisions.sh [--dry-run] [--run-tests]
#   --dry-run   差分を標準出力に出すだけで書かない
#   --run-tests 受け取るだけで使わない（journal-all.sh からの引き回し用）
# 時刻はすべて JST。書き込み先は journal/decisions.md のみ。

set -euo pipefail
# shellcheck source=journal-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/journal-lib.sh"

for arg in "$@"; do
  case "$arg" in
    --dry-run) export JOURNAL_DRY_RUN=1 ;;
    --run-tests) ;;
    -h | --help)
      sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "不明な引数: $arg" >&2
      exit 2
      ;;
  esac
done

need git
ROOT="$(repo_root)"
cd "$ROOT"
OUT="journal/decisions.md"
NOW="$(date '+%Y-%m-%d %H:%M:%S')"

# 長い文字列を n 文字で切る（マルチバイト対応は bash の ${var:0:n} に任せる）。
cut_chars() {
  local s="$1" n="$2"
  if ((${#s} > n)); then printf '%s…' "${s:0:n}"; else printf '%s' "$s"; fi
}

# Markdown の表セル用に | をエスケープする。
esc() { sed 's/|/\\|/g' <<<"$1"; }

# ---------------------------------------------------------------- qindex
gen_qindex() {
  echo "_生成: ${NOW} JST。出所: QUESTIONS.md（行番号は現在のファイル）、初出コミットは \`git log -S\` の最古の結果（日時は JST）。_"
  echo
  echo "| Q | 見出し | ★ | 行 | 初出コミット (JST) |"
  echo "|---|---|---|---|---|"
  local n=0 star=0 line ln id title star_mark first
  while IFS= read -r line; do
    ln="${line%%:*}"
    line="${line#*:}"
    # 形式: - **[★]Q-001 見出し**: 本文
    if [[ "$line" =~ ^-\ \*\*(★|)((M2-)?Q-?[0-9]+)\ ([^*]*)\*\* ]]; then
      star_mark="${BASH_REMATCH[1]}"
      id="${BASH_REMATCH[2]}"
      title="${BASH_REMATCH[4]}"
      title="${title%:}"
      # -S は「その文字列の出現数が変わったコミット」。ID の後ろに空白を付けて M2-Q1 と M2-Q10 を区別する。
      first="$(git log --format='%h %ad' --date=format-local:'%Y-%m-%d %H:%M' -S"${id} " -- QUESTIONS.md | tail -n 1)"
      [[ -n "$first" ]] || first="未コミット"
      printf '| %s | %s | %s | %s | %s |\n' "$id" "$(esc "$title")" "${star_mark:+★}" "$ln" "$first"
      n=$((n + 1))
      [[ -n "$star_mark" ]] && star=$((star + 1))
    fi
  done < <(grep -n '^- \*\*' QUESTIONS.md)
  echo
  echo "合計 ${n} 件、★ ${star} 件（QUESTIONS.md の見出し行から数えた値。★ の追認状況は手書き欄で管理する）。"
}

# ---------------------------------------------------------------- dtable
gen_dtable() {
  echo "_生成: ${NOW} JST。出所: \`grep -n '^| D[0-9][0-9]* ' spec/design/*.md\`。列は 行番号 / ID / 題 / 採用（4 列目。列が無い表は 不明）。_"
  local f any
  for f in spec/design/*.md; do
    any="$(grep -c '^| D[0-9][0-9]* ' "$f" || true)"
    echo
    if [[ "$any" == "0" ]]; then
      echo "- \`${f}\`: D 表なし（0 行）"
      continue
    fi
    echo "#### \`${f}\`（${any} 行）"
    echo
    echo "| 行 | ID | 題 | 採用 |"
    echo "|---|---|---|---|"
    grep -n '^| D[0-9][0-9]* ' "$f" | awk -F'|' '
      function trim(s) { gsub(/^ +| +$/, "", s); return s }
      function cut(s, n) { return length(s) > n ? substr(s, 1, n) "…" : s }
      {
        ln = $1; sub(/:.*/, "", ln)
        chosen = (NF >= 6) ? trim($5) : "不明"
        if (chosen == "") chosen = "不明"
        printf "| %s | %s | %s | %s |\n", ln, trim($2), trim($3), chosen
      }'
  done
}

# ---------------------------------------------------------------- unverified
gen_unverified() {
  # 前回値: 既存ブロック内の <!-- unv path cur prev --> から読む。
  # 今回の数が保存済みの cur と同じなら prev を据え置く（再実行で差分が消えない）。
  declare -A cur0=() prev0=()
  if [[ -f "$OUT" ]]; then
    local _t p c q
    while read -r _t _ p c q _; do
      [[ "$_t" == "<!--" ]] || continue
      cur0["$p"]="$c"
      prev0["$p"]="$q"
    done < <(grep '^<!-- unv ' "$OUT" || true)
  fi

  echo "_生成: ${NOW} JST。出所: \`grep -c 未検証\`（行数ベース）。前回値は本ブロック内のコメント行に保存している。_"
  echo
  echo "| ファイル | 現在 | 前回 | 差分 |"
  echo "|---|---|---|---|"
  local f n p d total=0 state=""
  for f in spec/design/*.md spec/research/*.md; do
    n="$(grep -c '未検証' "$f" || true)"
    [[ "$n" == "0" && -z "${cur0[$f]:-}" ]] && continue
    if [[ -z "${cur0[$f]:-}" ]]; then
      p="不明"
    elif [[ "${cur0[$f]}" == "$n" ]]; then
      p="${prev0[$f]}"
    else
      p="${cur0[$f]}"
    fi
    if [[ "$p" =~ ^[0-9]+$ ]]; then d="$((n - p))"; ((d > 0)) && d="+${d}"; else d="不明"; fi
    printf '| %s | %s | %s | %s |\n' "$f" "$n" "$p" "$d"
    total=$((total + n))
    state+="<!-- unv ${f} ${n} ${p} -->"$'\n'
  done
  echo
  echo "合計 ${total} 行（0 件のファイルは、過去に記録がある場合を除き省く）。"
  echo
  printf '%s' "$state"
}

# ---------------------------------------------------------------- deps
gen_deps() {
  echo "_生成: ${NOW} JST。出所: \`git log -p -- '*Cargo.toml' '*Cargo.lock'\`（コミット済み）と \`git diff HEAD\`（未コミット）。追加行（+）のみ。内部クレート yuzhu-* と package メタデータは除く。_"
  echo
  echo "| 状態 | コミット | ファイル | 追加された行 |"
  echo "|---|---|---|---|"
  local filter='
    /^COMMIT / { c = $2; next }
    /^\+\+\+ b\// { f = substr($0, 7); next }
    /^\+\+\+ / { next }
    /^\+/ {
      l = substr($0, 2)
      if (f ~ /Cargo\.lock$/) {
        if (l ~ /^name = "/ && l !~ /"yuzhu/) { print c "\t" f "\t" l }
      } else if ((l ~ /^[A-Za-z0-9_-]+ *= *("[0-9^~=<>*]|\{ *(version|path|git|workspace) *=)/ || l ~ /^[A-Za-z0-9_-]+\.workspace *= *true/) && l !~ /^(name|version|edition|license|publish|rust-version|description|resolver|path|authors|repository|readme)[. ]/ && l !~ /^yuzhu/) {
        print c "\t" f "\t" l
      }
    }'
  local pathspec=('*Cargo.toml' '*Cargo.lock')
  # Cargo.toml の行はそのまま、Cargo.lock は推移的依存が多いのでコミットごとに件数と名前を 1 行にまとめる。
  emit_rows() { # <状態> <行(タブ区切り: コミット, ファイル, 内容)>
    local state="$1" rows="$2" c fl l
    while IFS=$'\t' read -r c fl l; do
      [[ -n "$fl" && "$fl" != *Cargo.lock ]] || continue
      printf '| %s | %s | %s | `%s` |\n' "$state" "$c" "$fl" "$(esc "$l")"
    done <<<"$rows"
    local commits
    commits="$(awk -F'\t' '$2 ~ /Cargo\.lock$/ {print $1}' <<<"$rows" | awk '!s[$0]++')"
    while IFS= read -r c; do
      [[ -n "$c" ]] || continue
      local names cnt
      names="$(awk -F'\t' -v c="$c" '$1 == c && $2 ~ /Cargo\.lock$/ {gsub(/name = "|"/, "", $3); print $3}' <<<"$rows" | paste -sd, -)"
      cnt="$(awk -F, '{print NF}' <<<"$names")"
      printf '| %s | %s | Cargo.lock | %s 件の新規パッケージ: %s |\n' "$state" "$c" "$cnt" "$(esc "$(cut_chars "$names" 300)")"
    done <<<"$commits"
  }
  local rows
  rows="$(git log -p --reverse --format='COMMIT %h' -- "${pathspec[@]}" | awk "$filter" || true)"
  emit_rows コミット済み "$rows"
  rows="$(git diff HEAD --no-color -- "${pathspec[@]}" | awk -v c='-' "$filter" || true)"
  emit_rows 未コミット "$rows"
  echo
  echo "注: 依存の承認状況（CLAUDE.md「依存追加は慎重に」との関係、M2-Q6 など）は手書き欄で扱う。Cargo.lock の行は推移的依存を含む。"
}

# ---------------------------------------------------------------- pgdiff / supersede
gen_grep_block() { # <見出し文> <grep パターン> <対象...>
  local desc="$1" pat="$2"
  shift 2
  echo "_生成: ${NOW} JST。出所: ${desc}_"
  echo
  echo "| 場所 | 該当行（先頭 100 字） |"
  echo "|---|---|"
  local n=0 hit loc text
  while IFS= read -r hit; do
    loc="${hit%%:*}"
    hit="${hit#*:}"
    text="${hit#*:}"
    loc="${loc}:${hit%%:*}"
    printf '| `%s` | %s |\n' "$loc" "$(esc "$(cut_chars "$text" 100)")"
    n=$((n + 1))
  done < <(grep -nH -e "$pat" "$@" 2>/dev/null | sed 's/^\([^:]*\):\([0-9]*\):/\1:\2:/' || true)
  echo
  echo "合計 ${n} 行。"
}

gen_pgdiff() {
  gen_grep_block "\`grep -n 'PG との差\\|PostgreSQL と違'\` を spec/ と QUESTIONS.md に対して実行。転記はせず、該当位置だけを示す。" \
    'PG との差\|PostgreSQL と違' -r spec QUESTIONS.md
}

gen_supersede() {
  gen_grep_block "\`grep -n '改訂\\|前倒し'\` を spec/ と QUESTIONS.md に対して実行。判断が後から変わった箇所の候補で、真の改訂かどうかは手書き欄で判断する。" \
    '改訂\|前倒し' -r spec QUESTIONS.md
}

# ---------------------------------------------------------------- 書き込み
gen_qindex | replace_block "$OUT" qindex
gen_dtable | replace_block "$OUT" dtable
gen_unverified | replace_block "$OUT" unverified
gen_deps | replace_block "$OUT" deps
gen_pgdiff | replace_block "$OUT" pgdiff
gen_supersede | replace_block "$OUT" supersede
