#!/usr/bin/env bash
# Workflow のトランスクリプトを集計し、journal/workflows.md の AUTO:ledger と AUTO:agents を更新する。
# 使い方: tools/journal-workflows.sh [--dry-run]
#   --dry-run  ファイルを書き換えず、現在の内容との差分(diff -u)を標準出力に出す。
# 環境変数: JOURNAL_TRANSCRIPT_ROOT で ~/.claude/projects/<proj> の場所を上書きできる。
# 情報源(いずれも読み取りのみ):
#   <root>/*/subagents/workflows/wf_*/journal.jsonl  (started / result の記録。時刻は持たない)
#   <root>/*/subagents/workflows/wf_*/agent-*.jsonl  (各担当の発言・tool 呼び出し・timestamp・usage)
#   <root>/*/workflows/scripts/*-<wf_id>.js          (meta.phases = 計画)
# 時刻はすべて JST。jq 1.6 で動く。
set -euo pipefail
export TZ=Asia/Tokyo LC_ALL=C.UTF-8

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
target="$repo/journal/workflows.md"

dry_run=0
for arg in "$@"; do
case "$arg" in
  --dry-run) dry_run=1 ;;
  --run-tests) ;;  # journal-all.sh が全スクリプトへ渡す。ここでは使わない
  "") ;;
  *) echo "usage: $0 [--dry-run] [--run-tests]" >&2; exit 2 ;;
esac
done

# --- 共通処理(journal-lib.sh があれば読み込み、無い関数だけ補う) ---
# shellcheck disable=SC1091
[ -f "$here/journal-lib.sh" ] && source "$here/journal-lib.sh"
export TZ=Asia/Tokyo

command -v jq >/dev/null || { echo "jq が必要です" >&2; exit 1; }

if ! declare -F replace_block >/dev/null; then
  # replace_block <file> <name> : 標準入力の内容で AUTO ブロックを置換。無ければ末尾に作る。
  replace_block() {
    local file="$1" name="$2" body tmp
    body="$(cat)"
    mkdir -p "$(dirname "$file")"; [ -f "$file" ] || : >"$file"
    tmp="$(mktemp)"
    if grep -qF "<!-- AUTO:$name BEGIN -->" "$file"; then
      BODY="$body" awk -v b="<!-- AUTO:$name BEGIN -->" -v e="<!-- AUTO:$name END -->" '
        $0==b {print; print ENVIRON["BODY"]; skip=1; next}
        $0==e {skip=0}
        !skip {print}' "$file" >"$tmp"
    else
      { cat "$file"; printf '\n<!-- AUTO:%s BEGIN -->\n%s\n<!-- AUTO:%s END -->\n' "$name" "$body" "$name"; } >"$tmp"
    fi
    cat "$tmp" >"$file"; rm -f "$tmp"
  }
fi

# --- トランスクリプトのルート解決 ---
proj='-home-hiroshi-work-private-github-ai-built-rdbms'
root=""
for c in "${JOURNAL_TRANSCRIPT_ROOT:-}" "/home/sandbox/.claude/projects/$proj" "${HOME:-}/.claude/projects/$proj"; do
  if [ -n "$c" ] && [ -d "$c" ]; then root="$c"; break; fi
done
[ -n "$root" ] || { echo "トランスクリプトのルートが見つかりません" >&2; exit 1; }

work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
: >"$work/wfs.ndjson"

# --- 担当 1 件の集計 ---
# message.id ごとにストリーム分割された複数行があるので、id ごとに最後の行(最終 usage)を採用する。
AGENT_JQ='
def ts: sub("\\.[0-9]+Z$";"Z") | fromdateiso8601;
def hits: ["初回","1周目","修正は不要","コード変更なし"];
($j | map(select(.type=="result" and .agentId==$id)) | last) as $r
| ([.[] | select(.timestamp) | .timestamp] | sort) as $t
| ([.[] | select(.type=="assistant" and .message.id)] | group_by(.message.id) | map(last)) as $m
| ([.[] | select(.type=="assistant") | .message.content[]? | select(.type=="tool_use") | .id] | unique | length) as $tools
| ([.[] | select(.type=="user") | .message.content | arrays | .[] | select(.type=="tool_result" and .is_error==true) | .tool_use_id] | unique | length) as $errs
| (if ($r != null and (($r.result|type)=="object")) then $r.result else null end) as $res
| (($res.summary? // "") | tostring) as $sum
| {
    id: $id, "label": $lab, phase: $phase,
    state: (if $r != null then "done" else "running" end),
    start: $t[0], end: ($t|last),
    dur: (if ($t|length)>0 then (($t|last|ts) - ($t[0]|ts)) else null end),
    turns: ($m|length), tools: $tools, errors: $errs,
    in:  ([$m[].message.usage.input_tokens // 0] | add // 0),
    out: ([$m[].message.usage.output_tokens // 0] | add // 0),
    cr:  ([$m[].message.usage.cache_read_input_tokens // 0] | add // 0),
    cc:  ([$m[].message.usage.cache_creation_input_tokens // 0] | add // 0),
    model: ([$m[].message.model // empty] | unique | join(",")),
    has_result_obj: ($res != null),
    has_passed: ($res != null and ($res|has("passed"))),
    passed: (if $res != null then $res.passed? else null end),
    result_keys: (if $res != null then ($res|keys) else null end),
    hit: [hits[] as $h | select($sum|contains($h)) | $h]
  }
  | .first_pass = ((.hit|length)>0 and .tools<=10 and .state=="done")
'
# 担当ファイルが無い場合
NOFILE_JQ='
($j | map(select(.type=="result" and .agentId==$id)) | last) as $r
| (if ($r != null and (($r.result|type)=="object")) then $r.result else null end) as $res
| {id:$id, "label":$lab, phase:$phase, state:(if $r!=null then "done" else "running" end), nofile:true,
   has_result_obj:($res!=null), has_passed:($res!=null and ($res|has("passed"))),
   passed:(if $res!=null then $res.passed? else null end),
   result_keys:(if $res!=null then ($res|keys) else null end), hit:[], first_pass:false}'

for wfdir in "$root"/*/subagents/workflows/wf_*; do
  [ -f "$wfdir/journal.jsonl" ] || continue
  wfid="$(basename "$wfdir")"
  sess="$(cd "$wfdir/../../.." && pwd)"
  jf="$wfdir/journal.jsonl"
  : >"$work/agents.ndjson"
  while IFS=$'\t' read -r id label phase; do
    f="$wfdir/agent-$id.jsonl"
    if [ -f "$f" ]; then
      jq -s -c --slurpfile j "$jf" --arg id "$id" --arg lab "$label" --arg phase "$phase" "$AGENT_JQ" "$f"
    else
      jq -n -c --slurpfile j "$jf" --arg id "$id" --arg lab "$label" --arg phase "$phase" "$NOFILE_JQ"
    fi >>"$work/agents.ndjson"
  done < <(jq -r 'select(.type=="started") | [.agentId, (.label // ""), (.phase // "")] | @tsv' "$jf" | awk -F'\t' '!seen[$1]++')

  # 計画(meta.phases)
  script="$(ls "$sess"/workflows/scripts/*-"$wfid".js 2>/dev/null | head -n1 || true)"
  if [ -n "$script" ]; then
    sname="$(sed -n "s/^  name: '\\(.*\\)',\$/\\1/p" "$script" | head -n1)"
    sdesc="$(sed -n "s/^  description: '\\(.*\\)',\$/\\1/p" "$script" | head -n1)"
    phases_json="$(awk '/phases: \[/{f=1;next} f&&/^  \],?/{f=0} f' "$script" \
      | sed -nE "s/.*title: *['\"]([^'\"]*)['\"].*detail: *['\"]([^'\"]*)['\"].*/\\1\t\\2/p" \
      | jq -R -s -c 'split("\n") | map(select(length>0) | split("\t") | {title:.[0], detail:.[1]})')"
    script_rel="${script#"$sess"/}"
  else
    sname=""; sdesc=""; phases_json="null"; script_rel=""
  fi

  jq -s -c --arg id "$wfid" --arg script "$script_rel" --arg name "$sname" --arg desc "$sdesc" \
     --argjson phases "$phases_json" --arg jlines "$(wc -l <"$jf" | tr -d ' ')" \
     --arg nfiles "$(ls "$wfdir"/agent-*.jsonl 2>/dev/null | wc -l | tr -d ' ')" \
     '{id:$id, script:$script, name:$name, desc:$desc, planned:$phases, journal_lines:($jlines|tonumber), agent_files:($nfiles|tonumber), agents:.}' \
     "$work/agents.ndjson" >>"$work/wfs.ndjson"
done

[ -s "$work/wfs.ndjson" ] || { echo "wf_* が見つかりません: $root" >&2; exit 1; }

# --- Markdown 生成 ---
RENDER_JQ='
def ts: sub("\\.[0-9]+Z$";"Z") | fromdateiso8601;
def pad2: tostring | if length<2 then "0"+. else . end;
def fd: if .==null then "不明" else
  (. as $s | if $s>=3600 then "\($s/3600|floor)h\(($s%3600/60|floor)|pad2)m" else "\($s/60|floor)m\(($s%60)|pad2)s" end) end;
def ft: if .==null then "不明" else (sub("\\.[0-9]+Z$";"Z") | fromdateiso8601 | . + 32400 | strftime("%Y-%m-%d %H:%M:%S")) end;
def pct: if .==null then "不明" else "\((. * 1000 | round) / 10)%" end;
def esc: gsub("\\|";"\\|");
def short: split($repo + "/") | join("");
def sum(f): [.[] | f // 0] | add // 0;
def or_unk: if .==null or .=="" then "不明" else . end;

def summarize:
  . as $w
  | ($w.agents | map(select(.nofile|not))) as $a
  | ($a | map(.start) | sort | first) as $s
  | ($a | map(.end) | sort | last) as $e
  | (if $s != null then (($e|ts) - ($s|ts)) else null end) as $wall
  | ($a | max_by(.dur // 0)) as $longest
  | ($a | sum(.dur)) as $busy
  | ($w.agents | length) as $cnt
  | ($w.agents | map(select(.state=="done")) | length) as $done
  | $w + {
      start: $s, end: $e, wall: $wall, count: $cnt, done: $done,
      status: (if $cnt==0 then "不明" elif $done==$cnt then "完了" else "進行中" end),
      longest: $longest, busy: $busy,
      longest_ratio: (if ($wall // 0)>0 and $longest != null then ($longest.dur / $wall) else null end),
      parallel: (if ($wall // 0)>0 then ($busy / $wall) else null end)
    };

([.[] | summarize] | sort_by(.start // "9")) as $ws
| ($ws | map(.end // empty) | max) as $latest
| ([
  "> 自動生成(tools/journal-workflows.sh)。時刻はすべて JST。出所: `\($root)/*/subagents/workflows/wf_*/` の journal.jsonl と agent-*.jsonl、`workflows/scripts/*.js` の meta.phases。",
  "> 最新ログ時刻: \($latest | ft) JST。壁時計 = 担当ファイルの最小 timestamp から最大 timestamp まで(journal.jsonl 自体は時刻を持たない)。進行中の Workflow は最新ログ時刻までの暫定値。",
  "> 最長/壁時計 = 最長担当の所要 ÷ 壁時計。並列効率 = 全担当の所要の合計 ÷ 壁時計(1.0 なら実質直列、大きいほど並列が効いている)。",
  "",
  "### Workflow 一覧",
  "",
  "| Workflow | 名前 | 状態 | 開始 (JST) | 終了 (JST) | 壁時計 | 担当数 (結果あり) | 最長担当 | 最長/壁時計 | 合計稼働 | 並列効率 |",
  "|---|---|---|---|---|---|---|---|---|---|---|",
  ($ws[] |
    "| \(.id) | \(.name | or_unk) | \(.status) | \(.start | ft) | \(.end | ft)\(if .status=="進行中" then " (暫定)" else "" end) | \(.wall | fd) | \(.count) (\(.done)) | \(if .longest then "\(.longest.label | short | esc) (\(.longest.dur | fd))" else "不明" end) | \(.longest_ratio | pct) | \(.busy | fd) | \(if .parallel then "×\((.parallel * 100 | round) / 100)" else "不明" end) |"),
  "",
  "### 計画(meta.phases)と実績",
  "",
  ($ws[] | . as $w | (
    "#### \($w.id) \($w.name) (\($w.status))",
    "",
    (if $w.script=="" then "- 計画スクリプト: 不明(`workflows/scripts/*-\($w.id).js` が見つからない)" else "- 計画スクリプト: `<session>/\($w.script)`" end),
    (if $w.desc!="" then "- 説明: \($w.desc)" else empty end),
    "- journal.jsonl の行数: \($w.journal_lines)、agent-*.jsonl の数: \($w.agent_files)、started の担当数: \($w.count)",
    "",
    "| フェーズ | 計画での説明 | 実績の担当数 (結果あり) | 開始 (JST) | 終了 (JST) | 壁時計 | 備考 |",
    "|---|---|---|---|---|---|---|",
    (
      ($w.planned // []) as $pl
      | ($w.agents | group_by(.phase) | map({key: .[0].phase, value: .}) | from_entries) as $byp
      | ($pl | map(.title)) as $titles
      | ( $pl[] | . as $p | ($byp[$p.title] // []) as $g | ($g | map(select(.nofile|not))) as $gf
          | ($gf | map(.start) | sort | first) as $ps | ($gf | map(.end) | sort | last) as $pe
          | "| \($p.title) | \($p.detail | esc) | \($g|length) (\($g | map(select(.state=="done")) | length)) | \($ps | ft) | \($pe | ft) | \(if $ps then (($pe|ts)-($ps|ts)) else null end | fd) | \(if ($g|length)==0 then (if $w.status=="進行中" then "未到達(進行中)" else "実績なし(スキップまたは未起動)" end) elif ($g|map(select(.state!="done"))|length)>0 then "進行中の担当あり" else "" end) |" ),
        ( ($byp | keys[]) as $k | select(($titles | index($k)) == null) | $byp[$k] as $g
          | "| \($k) | (計画外) | \($g|length) (\($g | map(select(.state=="done")) | length)) | \($g | map(.start // empty) | sort | first | ft) | \($g | map(.end // empty) | sort | last | ft) | 不明 | 計画に無いフェーズ |" )
    ),
    ""
  )),
  "### 結果スキーマの不揃い(result に passed キーが無い担当)",
  "",
  "出所: journal.jsonl の `type==result` の `result` オブジェクトのキー。Workflow ごとに結果スキーマが違うため、passed 以外(done / items など)を使うものは全てここに載る。完了した担当だけが対象。",
  "",
  ( [ $ws[] | . as $w | $w.agents[] | select(.state=="done" and .has_passed==false) | {wf:$w.id, a:.} ] as $bad
    | if ($bad|length)==0 then "- なし" else
        ( "| Workflow | 担当 | フェーズ | result のキー |", "|---|---|---|---|",
          ($bad[] | "| \(.wf) | \(.a.label | short | esc) | \(.a.phase) | \(if .a.has_result_obj then (.a.result_keys | join(", ")) else "result がオブジェクトでない" end) |") )
      end ),
  "",
  "### first-pass 判定",
  "",
  "条件: result.summary に「初回」「1周目」「修正は不要」「コード変更なし」のいずれかを含み、tool_use が 10 回以下(StructuredOutput 呼び出しを含む)。summary を持たない result(スキーマ違い)は判定できない。文字列一致による目安で、解釈は手書きで行う。",
  "",
  ( [ $ws[] | . as $w | $w.agents[] | select(.first_pass) | {wf:$w.id, a:.} ] as $fp
    | if ($fp|length)==0 then "- 該当なし" else
        ( "| Workflow | 担当 | tool_use | 一致した語 |", "|---|---|---|---|",
          ($fp[] | "| \(.wf) | \(.a.label | short | esc) | \(.a.tools) | \(.a.hit | join(" / ")) |") )
      end ),
  "",
  "### 進行中の担当(result 未着)",
  "",
  ( [ $ws[] | . as $w | $w.agents[] | select(.state=="running") | {wf:$w.id, a:.} ] as $run
    | if ($run|length)==0 then "- なし" else
        ( "| Workflow | 担当 | フェーズ | 開始 (JST) | 最終ログ (JST) | ここまでの所要 |", "|---|---|---|---|---|---|",
          ($run[] | "| \(.wf) | \(.a.label | short | esc) | \(.a.phase) | \(.a.start | ft) | \(.a.end | ft) | \(.a.dur | fd) |") )
      end )
] | join("\n")) as $ledger
| ([
  "> 自動生成。出所: agent-*.jsonl。ターン = message.id ごとに 1 回と数えた assistant 応答の数。tool_use = tool_use ブロックの id の重複なし件数(StructuredOutput を含む)。エラー = `is_error:true` の tool_result の件数。",
  "> トークンは message.usage の合計(message.id ごとに最後の行を採用)。入力 = input_tokens、出力 = output_tokens、cache読 = cache_read_input_tokens、cache作成 = cache_creation_input_tokens。モデル = message.model。",
  "> 状態: 「完了」= journal に result あり、「進行中」= result 未着(所要・トークンは暫定)。passed は result.passed(キー無しは「-」)。",
  "",
  ($ws[] | . as $w | (
    "### \($w.id) \($w.name) (\($w.status))",
    "",
    "| 担当 | フェーズ | 状態 | passed | 開始 (JST) | 終了 (JST) | 所要 | ターン | tool_use | エラー | 入力 | 出力 | cache読 | cache作成 | モデル | first-pass |",
    "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|",
    ($w.agents | sort_by(.start // "9") | .[] |
      if .nofile then
        "| \(.label | short | esc) | \(.phase) | \(if .state=="done" then "完了" else "進行中" end) | \(if .has_passed then (.passed|tostring) else "-" end) | 不明 | 不明 | 不明 | 不明 | 不明 | 不明 | 不明 | 不明 | 不明 | 不明 | 不明 | - |"
      else
        "| \(.label | short | esc) | \(.phase) | \(if .state=="done" then "完了" else "進行中" end) | \(if .has_passed then (.passed|tostring) else "-" end) | \(.start | ft) | \(.end | ft) | \(.dur | fd) | \(.turns) | \(.tools) | \(.errors) | \(.in) | \(.out) | \(.cr) | \(.cc) | \(.model | or_unk) | \(if .first_pass then "first-pass" else "-" end) |"
      end),
    "",
    ( $w.agents | map(select(.nofile|not)) as $a
      | "合計: ターン \($a | sum(.turns))、tool_use \($a | sum(.tools))、エラー \($a | sum(.errors))、入力 \($a | sum(.in))、出力 \($a | sum(.out))、cache読 \($a | sum(.cr))、cache作成 \($a | sum(.cc))" ),
    ""
  ))
] | join("\n")) as $agents
| {ledger: $ledger, agents: $agents}
'

jq -s --arg root "$root" --arg repo "$repo" "$RENDER_JQ" "$work/wfs.ndjson" >"$work/blocks.json" 2>"$work/jq.err" \
  || { cat "$work/jq.err" >&2; exit 1; }
jq -r '.ledger' "$work/blocks.json" >"$work/ledger.md"
jq -r '.agents' "$work/blocks.json" >"$work/agents.md"

if [ "$dry_run" -eq 1 ]; then
  export JOURNAL_DRY_RUN=1   # lib の replace_block が差分(diff -u)だけを出して書かない
  replace_block "$target" ledger <"$work/ledger.md"
  replace_block "$target" agents <"$work/agents.md"
else
  replace_block "$target" ledger <"$work/ledger.md"
  replace_block "$target" agents <"$work/agents.md"
  echo "更新しました: $target (ledger, agents)"
fi
