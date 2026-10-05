#!/usr/bin/env bash
# Workflow の各エージェントの所要時間を Markdown の表で出力する。
# 使い方: tools/worklog.sh <workflow の transcript ディレクトリ> [見出し]
#   例: tools/worklog.sh ~/.claude/projects/<proj>/<session>/subagents/workflows/wf_xxx "M1 完成"
set -euo pipefail
dir="${1:?transcript dir}"; title="${2:-$(basename "$dir")}"
echo "### $title"; echo
echo "| 担当 | フェーズ | 開始 (JST) | 終了 (JST) | 所要 |"
echo "|---|---|---|---|---|"
grep -h '"type":"started"' "$dir/journal.jsonl" | jq -r '[.agentId,.label,.phase]|@tsv' | while IFS=$'\t' read -r id label phase; do
  f="$dir/agent-$id.jsonl"; [ -f "$f" ] || continue
  read -r s e < <(jq -rs '[.[].timestamp|select(.)]|[first,last]|@tsv' "$f")
  se=$(date -u -d "$s" +%s); ee=$(date -u -d "$e" +%s); d=$((ee-se))
  printf '| %s | %s | %s | %s | %dm%02ds |\n' "$label" "$phase" "$(TZ=Asia/Tokyo date -d "$s" +%H:%M:%S)" "$(TZ=Asia/Tokyo date -d "$e" +%H:%M:%S)" $((d/60)) $((d%60))
done | sort -t'|' -k4
