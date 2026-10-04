#!/usr/bin/env bash
# journal-verify.sh: エージェントの報告と実状態の不一致を機械的に洗い出し、
# journal/setbacks.md の AUTO:reports を更新する。事実の列挙のみで、解釈はしない（手書き欄で行う）。
#
# 出す内容:
#   1. 報告ハッシュの存在確認: 各 wf_*/journal.jsonl の result.summary から
#      7 桁以上の 16 進文字列（英字と数字の両方を含むもの）を抜き、git cat-file -t で確認する。
#      wf_ ID と agentId の断片は誤検出なので除外する。存在しても HEAD から到達できないもの
#      （amend や reset で履歴から外れ、reflog にだけ残るもの）は、その旨を併記する。
#   2. 「未実行」「未存在」を含む報告行と、実ファイルの状態（tests/slt/m2 の件数など）の並置。
#
# 使い方: tools/journal-verify.sh [--dry-run] [--run-tests]
#   --dry-run   差分を標準出力に出すだけで書かない
#   --run-tests 受け取るだけで使わない（journal-all.sh からの引き回し用）
# 時刻はすべて UTC。書き込み先は journal/setbacks.md のみ。

set -euo pipefail
# shellcheck source=journal-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/journal-lib.sh"

for arg in "$@"; do
  case "$arg" in
    --dry-run) export JOURNAL_DRY_RUN=1 ;;
    --run-tests) ;;
    -h | --help)
      sed -n '2,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "不明な引数: $arg" >&2
      exit 2
      ;;
  esac
done

need jq
need git
ROOT="$(repo_root)"
cd "$ROOT"
OUT="$ROOT/journal/setbacks.md"

# 1 行 1 結果: wf \t agentId \t label \t summary（@tsv により改行は \n の文字列になる）
collect() {
  local txroot wf f w a s label
  txroot="$(tx_root)" || return 0
  for f in "$txroot"/*/subagents/workflows/wf_*/journal.jsonl; do
    [[ -f "$f" ]] || continue
    wf="$(basename "$(dirname "$f")")"
    jq -r --arg wf "$wf" '
      select(.type == "result")
      | [$wf, .agentId,
         (if (.result | type) == "object" then (.result.summary // "") else (.result | tostring) end)]
      | @tsv' "$f" 2>/dev/null |
      while IFS=$'\t' read -r w a s; do
        label="$(jq -r --arg a "$a" 'select(.type=="started" and .agentId==$a) | .label' "$f" | head -n1)"
        printf '%s\t%s\t%s\t%s\n' "$w" "$a" "${label:-不明}" "$s"
      done
  done
}

RESULTS="$(mktemp)"
IDS="$(mktemp)"
trap 'rm -f "$RESULTS" "$IDS"' EXIT
collect >"$RESULTS"

# 誤検出の除外リスト: wf_ ID と agentId（どちらも 16 進を含む）
{
  cut -f1,2 "$RESULTS" | tr '\t' '\n'
  if root="$(tx_root 2>/dev/null)"; then
    find "$root" -path '*/subagents/workflows/*' \( -name 'wf_*' -o -name 'agent-*.jsonl' \) -printf '%f\n' 2>/dev/null
  fi
} | sort -u >"$IDS"

HEX_RE='(?<![0-9A-Za-z])[0-9a-f]{7,40}(?![0-9A-Za-z])'

gen() {
  local now
  now="$(date -u '+%Y-%m-%d %H:%M:%S')"
  echo "生成: ${now} UTC / 出所: 各 wf_*/journal.jsonl の type==result の result.summary、git、tests/slt/"
  echo "対象 result 数: $(wc -l <"$RESULTS")（出所: 各 journal.jsonl の type==result 行の合計）"
  echo

  echo "### 報告ハッシュの存在確認（git cat-file -t）"
  echo
  echo "| 状態 | ハッシュ | Workflow | agentId | label | git の応答 |"
  echo "|---|---|---|---|---|---|"
  local n_ok=0 n_orphan=0 n_ng=0 w a l s h t
  while IFS=$'\t' read -r w a l s; do
    while IFS= read -r h; do
      [[ -n "$h" ]] || continue
      [[ "$h" =~ [0-9] && "$h" =~ [a-f] ]] || continue # 英字と数字の両方を含むものだけ
      grep -qF -- "$h" "$IDS" && continue              # wf_ ID / agentId の断片は除外
      l="${l##*/}"
      if t="$(git cat-file -t "$h" 2>&1)"; then
        if git merge-base --is-ancestor "$h" HEAD 2>/dev/null; then
          echo "| 存在 | \`$h\` | $w | $a | $l | $t（HEAD から到達可） |"
          n_ok=$((n_ok + 1))
        else
          echo "| **履歴から外れている** | \`$h\` | $w | $a | $l | $t（HEAD から到達不能。reflog にのみ残る可能性） |"
          n_orphan=$((n_orphan + 1))
        fi
      else
        echo "| **報告ハッシュ不在** | \`$h\` | $w | $a | $l | ${t//|/\\|} |"
        n_ng=$((n_ng + 1))
      fi
    done < <(printf '%s' "$s" | grep -oP "$HEX_RE" | sort -u || true)
  done <"$RESULTS"
  echo
  echo "件数: 到達可 ${n_ok} / 履歴から外れている ${n_orphan} / オブジェクト不在 ${n_ng}（出所: 上表）"
  echo

  echo "### 「未実行」「未存在」を含む報告"
  echo
  local hits=0 line
  while IFS=$'\t' read -r w a l s; do
    l="${l##*/}"
    while IFS= read -r line; do
      [[ -n "$line" ]] || continue
      echo "- ${w} / ${a} / ${l}: ${line:0:240}"
      hits=$((hits + 1))
    done < <(printf '%s' "$s" | sed 's/\\n/\n/g' | grep -E '未実行|未存在' || true)
  done <"$RESULTS"
  [[ "$hits" -gt 0 ]] || echo "- 該当なし"
  echo
  echo "該当行数: ${hits}（出所: 上記）"
  echo

  echo "### 実ファイルの状態（${now} UTC 時点）"
  echo
  echo "| 項目 | 値 | 出所 |"
  echo "|---|---|---|"
  local d
  for d in tests/slt/m1 tests/slt/m2; do
    if [[ -d "$d" ]]; then
      echo "| ${d} のファイル数 | $(find "$d" -type f | wc -l) | \`find ${d} -type f\` |"
    else
      echo "| ${d} のファイル数 | 不在 | \`test -d ${d}\` |"
    fi
  done
  if [[ -d tests/slt/m2 ]]; then
    for d in tests/slt/m2/*/; do
      [[ -d "$d" ]] || continue
      echo "| ${d%/} | $(find "$d" -type f | wc -l) | \`find ${d} -type f\` |"
    done
  fi
  local p
  for p in tests/yuzhu.sh tests/pg.sh tests/run.sh sandbox/pg.sh impl/rust/crates/yuzhu-server impl/rust/crates/yuzhu-initdb; do
    if [[ -e "$p" ]]; then echo "| ${p} | あり | \`test -e\` |"; else echo "| ${p} | なし | \`test -e\` |"; fi
  done
  echo "| HEAD | $(git rev-parse --short HEAD) | \`git rev-parse --short HEAD\` |"
  echo "| 作業ツリーの変更ファイル数 | $(git status --short | wc -l) | \`git status --short\` |"
  echo
  echo "※ 上記は事実の並置のみ。報告と実状態の食い違いの判断・原因・対処は手書き欄に書く。"
}

gen | replace_block "$OUT" reports
