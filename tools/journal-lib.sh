#!/usr/bin/env bash
# journal-lib.sh: tools/journal-*.sh が共通で使う関数群（source して使う）。
#   source "$(dirname "${BASH_SOURCE[0]}")/journal-lib.sh"
# 時刻はすべて UTC。書き込み先は journal/ 配下に限定する。
#
# 環境変数:
#   JOURNAL_DRY_RUN=1  replace_block が差分を標準出力に出すだけで書かない
#                      （journal-all.sh --dry-run が export する想定）

set -euo pipefail
export TZ=UTC

_JOURNAL_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# repo_root: リポジトリのルート（tools/ の親）を返す。
repo_root() {
  (cd "${_JOURNAL_LIB_DIR}/.." && pwd)
}

# tx_root: Workflow トランスクリプトのルートを返す。
# $HOME 側を優先し、無ければ /home/sandbox 側を試す。どちらも無ければ警告して 1 を返す
# （呼び出し側は該当節をスキップする）。
tx_root() {
  local name="-home-hiroshi-work-private-github-ai-built-rdbms"
  local c
  for c in "${HOME:-}/.claude/projects/${name}" "/home/sandbox/.claude/projects/${name}"; do
    if [[ -d "$c" ]]; then
      printf '%s\n' "$c"
      return 0
    fi
  done
  echo "warn: トランスクリプトのルートが見つからない。該当節をスキップする" >&2
  return 1
}

# need <cmd>: コマンドの存在を確認する。無ければ終了する。例: need jq
need() {
  local cmd="${1:?need: コマンド名が必要}"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "error: ${cmd} が見つからない" >&2
    exit 1
  fi
}

# fmt_dur <秒>: XmYYs に整形する（例: 125 -> 2m05s、3725 -> 62m05s）。
fmt_dur() {
  local s="${1:-}"
  if [[ ! "$s" =~ ^[0-9]+$ ]]; then
    # jq の小数秒は切り捨てる
    s="${s%%.*}"
    [[ "$s" =~ ^[0-9]+$ ]] || { printf '不明\n'; return 0; }
  fi
  printf '%dm%02ds\n' $((s / 60)) $((s % 60))
}

# _assert_in_journal <file>: 書き込み先が <repo>/journal/ 配下か検査する。
_assert_in_journal() {
  local target resolved root
  target="$1"
  root="$(repo_root)/journal"
  resolved="$(realpath -m -- "$target")"
  case "$resolved" in
    "$root"/*) ;;
    *)
      echo "error: journal/ 配下以外には書かない: ${resolved}" >&2
      exit 1
      ;;
  esac
}

# replace_block <file> <name>: 標準入力の内容で
#   <!-- AUTO:name BEGIN --> と <!-- AUTO:name END --> の間を置換する。
# マーカーが無ければファイル末尾に（マーカーごと）追加する。ファイルが無ければ作る。
# JOURNAL_DRY_RUN=1 なら unified diff を標準出力に出し、書かない。
replace_block() {
  local file="${1:?replace_block: file}" name="${2:?replace_block: name}"
  _assert_in_journal "$file"

  local tmp_blk tmp_new
  tmp_blk="$(mktemp)"
  tmp_new="$(mktemp)"
  # shellcheck disable=SC2064
  trap "rm -f '$tmp_blk' '$tmp_new'" RETURN
  cat >"$tmp_blk"

  local begin="<!-- AUTO:${name} BEGIN -->" end="<!-- AUTO:${name} END -->"
  local src="$file"
  [[ -f "$file" ]] || src=/dev/null

  if grep -qxF -- "$begin" "$src" && grep -qxF -- "$end" "$src"; then
    awk -v begin="$begin" -v end="$end" -v blk="$tmp_blk" '
      $0 == begin && !done {
        print
        while ((getline line < blk) > 0) print line
        skip = 1; next
      }
      skip && $0 == end { print; skip = 0; done = 1; next }
      !skip { print }
    ' "$src" >"$tmp_new"
  else
    {
      [[ "$src" == /dev/null ]] || cat "$src"
      if [[ -s "$src" ]]; then printf '\n'; fi
      printf '%s\n' "$begin"
      cat "$tmp_blk"
      printf '%s\n' "$end"
    } >"$tmp_new"
  fi

  if [[ "${JOURNAL_DRY_RUN:-0}" == "1" ]]; then
    diff -u --label "a/${file}" --label "b/${file}" "$src" "$tmp_new" || true
    return 0
  fi

  if [[ -f "$file" ]] && cmp -s "$file" "$tmp_new"; then
    return 0 # 変更なし（冪等）
  fi
  mkdir -p -- "$(dirname -- "$file")"
  cat "$tmp_new" >"$file"
}
