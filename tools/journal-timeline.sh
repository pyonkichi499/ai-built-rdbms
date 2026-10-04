#!/usr/bin/env bash
# journal-timeline.sh: journal/timeline.md の AUTO ブロックを更新する（時刻はすべて UTC）。
#   AUTO:commits       git log（コミット時刻を UTC に統一、直前コミットとの間隔）
#   AUTO:workflows     Workflow ごとの開始・終了・壁時計（未完了は「進行中」）
#   AUTO:interventions ユーザー発言（task-notification を除く、先頭 80 字）
#   AUTO:waits         ユーザー発言から次の Workflow 開始までの時間
# 使い方: tools/journal-timeline.sh   （JOURNAL_DRY_RUN=1 で差分表示のみ）
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/journal-lib.sh"
need jq; need git

ROOT="$(repo_root)"
OUT="$ROOT/journal/timeline.md"
cd "$ROOT"

hms() { local s="$1"; printf '%dm%02ds' $((s / 60)) $((s % 60)); }
utc() { date -u -d "$1" '+%Y-%m-%d %H:%M:%S'; }
jst_hm() { TZ=Asia/Tokyo date -d "$1" '+%H:%M'; }

# ---- commits ----
{
  echo "| hash | 時刻 (UTC) | 件名 | 変更ファイル | 追加行 | 削除行 | 直前コミットから |"
  echo "|---|---|---|---|---|---|---|"
  # 古い順に処理して間隔を出す
  prev=""
  while IFS='|' read -r h at ad subj; do
    stat="$(git show --shortstat --format= "$h" | tr -d '\n')"
    f="$(sed -En 's/.* ([0-9]+) files? changed.*/\1/p' <<<"$stat")"
    a="$(sed -En 's/.* ([0-9]+) insertions?\(\+\).*/\1/p' <<<"$stat")"
    d="$(sed -En 's/.* ([0-9]+) deletions?\(-\).*/\1/p' <<<"$stat")"
    if [[ -n "$prev" ]]; then gap="$(hms $((at - prev)))"; [[ $((at - prev)) -ge 3600 ]] && gap="$((($at - prev) / 3600))h$(((at - prev) % 3600 / 60))m"; else gap="-"; fi
    printf '| %s | %s | %s | %s | %s | %s | %s |\n' "$h" "$(date -u -d "@$at" '+%Y-%m-%d %H:%M:%S')" "$subj" "${f:-0}" "${a:-0}" "${d:-0}" "$gap"
    prev="$at"
  done < <(git log --reverse --format='%h|%at|%ad|%s' --date=format-local:'%Y-%m-%d %H:%M:%S')
  echo
  echo "出所: \`git log\`（TZ=UTC）と \`git show --shortstat\`。生成: $(date -u '+%Y-%m-%d %H:%M:%S') UTC"
} | replace_block "$OUT" commits

# ---- workflows / interventions / waits（トランスクリプトが無ければスキップ）----
if TX="$(tx_root)"; then
  MAIN="$(ls "$TX"/*.jsonl 2>/dev/null | head -1 || true)"
  SESSION_DIR="${MAIN%.jsonl}"
  WFDIR="$SESSION_DIR/subagents/workflows"
  SCRIPTS="$SESSION_DIR/workflows/scripts"

  # wf 一覧: id|開始(script の mtime = 起動時刻)|最初の agent 時刻|最後の agent 時刻|started 数|result 数|完了か|名前|説明
  rows="$(mktemp)"; trap 'rm -f "$rows"' EXIT
  for d in "$WFDIR"/wf_*; do
    [[ -d "$d" ]] || continue
    id="$(basename "$d")"
    scr="$(ls "$SCRIPTS"/*-"$id".js 2>/dev/null | head -1 || true)"
    launch=""; name="不明"; desc="不明"
    if [[ -n "$scr" ]]; then
      launch="$(date -u -d "@$(stat -c %Y "$scr")" '+%Y-%m-%dT%H:%M:%SZ')"
      name="$(sed -En "s/^ *name: '(.*)',/\1/p" "$scr" | head -1)"
      desc="$(sed -En "s/^ *description: '(.*)',/\1/p" "$scr" | head -1)"
    fi
    read -r first last < <(cat "$d"/agent-*.jsonl 2>/dev/null | jq -r '.timestamp // empty' | sort | sed -n '1p;$p' | paste -sd' ')
    ns="$(grep -c '"type":"started"' "$d/journal.jsonl" || true)"
    nr="$(grep -c '"type":"result"' "$d/journal.jsonl" || true)"
    done_flag="進行中"; [[ -f "$SESSION_DIR/workflows/$id.json" ]] && done_flag="完了"
    printf '%s|%s|%s|%s|%s|%s|%s|%s|%s\n' "$id" "${launch:-${first:-}}" "${first:-}" "${last:-}" "$ns" "$nr" "$done_flag" "$name" "$desc"
  done | sort -t'|' -k2 >"$rows"

  {
    echo "| wf ID | 名前 / 目的 | 開始 (UTC) | 終了 (UTC) | 壁時計 | 担当 (結果済/起動) |"
    echo "|---|---|---|---|---|---|"
    while IFS='|' read -r id start first last ns nr st name desc; do
      s="$(date -u -d "$start" +%s)"
      if [[ "$st" == "完了" ]]; then
        e="$(date -u -d "$last" +%s)"; endcol="$(utc "$last")"; wall="$(hms $((e - s)))"
      else
        endcol="進行中"; wall="進行中（最終ログ ${last:+$(date -u -d "$last" +%H:%M:%S)} 時点で $(hms $(( $(date -u -d "$last" +%s) - s )))）"
      fi
      printf '| %s | %s: %s | %s | %s | %s | %s/%s |\n' "$id" "$name" "$desc" "$(utc "$start")" "$endcol" "$wall" "$nr" "$ns"
    done <"$rows"
    echo
    echo "出所: 開始 = \`workflows/scripts/*-<wf ID>.js\` の mtime（起動時刻）。終了 = 最後の agent ログの timestamp（\`workflows/<wf ID>.json\` がある場合のみ完了扱い）。journal.jsonl には時刻が無い。"
  } | replace_block "$OUT" workflows

  # ユーザー発言（content が文字列で、task-notification でないもの）
  msgs="$(mktemp)"; trap 'rm -f "$rows" "$msgs"' EXIT
  jq -r 'select(.type=="user" and (.message.content|type)=="string")
         | select(.message.content|startswith("<task-notification>")|not)
         | [.timestamp, (.message.content|gsub("\n";" ")|.[0:80])]|@tsv' "$MAIN" >"$msgs"
  {
    echo "| 時刻 (UTC) | JST | 発言（先頭 80 字） |"
    echo "|---|---|---|"
    while IFS=$'\t' read -r ts text; do
      printf '| %s | %s | %s |\n' "$(utc "$ts")" "$(jst_hm "$ts")" "${text//|/\\|}"
    done <"$msgs"
    echo
    echo "出所: メイン会話 transcript（$(basename "$MAIN")）の type==user かつ content が文字列の行。このセッションの最初の記録は $(jq -r '.timestamp // empty' "$MAIN" | sort | head -1)。それ以前のセッションの発言は含まれない。"
  } | replace_block "$OUT" interventions

  {
    echo "| ユーザー発言 (UTC) | 次の Workflow 起動 (UTC) | 発言から起動まで | 起動した wf |"
    echo "|---|---|---|---|"
    while IFS=$'\t' read -r ts text; do
      t="$(date -u -d "$ts" +%s)"; next=""; nid=""
      while IFS='|' read -r id start _; do
        [[ $(date -u -d "$start" +%s) -ge $t ]] && { next="$start"; nid="$id"; break; }
      done <"$rows"
      if [[ -n "$next" ]]; then
        printf '| %s | %s | %s | %s |\n' "$(utc "$ts")" "$(utc "$next")" "$(hms $(( $(date -u -d "$next" +%s) - t )))" "$nid"
      else
        printf '| %s | （以降の Workflow なし） | - | - |\n' "$(utc "$ts")"
      fi
    done <"$msgs"
    echo
    echo "注意: 「次の Workflow」は直後の発言で起動されたとは限らない（承認や追加発言を挟むことがある）。"
  } | replace_block "$OUT" waits
fi
