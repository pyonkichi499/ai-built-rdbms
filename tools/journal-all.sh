#!/usr/bin/env bash
# journal-all.sh: Workflow 終了ごとに実行する journal/ 更新の入口。
#
# tools/journal-{timeline,workflows,decisions,metrics,verify,snapshots,readme}.sh を順に実行する。
# 1 つが失敗しても続行し、最後に失敗した節を一覧にして非 0 で終了する。
# 書き込み先は journal/ のみ（各スクリプトの責務）。git add / commit / push は行わない。
# 時刻はすべて UTC。
#
# 使い方:
#   tools/journal-all.sh [--dry-run] [--run-tests]
#     --dry-run    差分を標準出力に出すだけで、ファイルを書き換えない（各スクリプトへ引き回す）
#     --run-tests  tests/run.sh を実行して slt 通過率を取る（各スクリプトへ引き回す。主に metrics が使う）
#   -h, --help     このヘルプ

set -uo pipefail

export TZ=UTC
export LC_ALL=C.UTF-8 2>/dev/null || true

TOOLS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$TOOLS_DIR/.." && pwd)"

SECTIONS=(timeline workflows decisions metrics verify snapshots readme)

usage() {
  sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

PASS_ARGS=()
for arg in "$@"; do
  case "$arg" in
    --dry-run | --run-tests) PASS_ARGS+=("$arg") ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "不明な引数: $arg" >&2
      usage >&2
      exit 2
      ;;
  esac
done

cd "$REPO_ROOT" || exit 1

echo "== journal-all 開始 (UTC $(date '+%Y-%m-%d %H:%M:%S')) 引数: ${PASS_ARGS[*]:-なし} =="

FAILED=()
for name in "${SECTIONS[@]}"; do
  script="$TOOLS_DIR/journal-$name.sh"
  echo
  echo "-- journal-$name.sh --"
  if [[ ! -f "$script" ]]; then
    echo "スクリプトが存在しない: $script" >&2
    FAILED+=("$name (スクリプト未存在)")
    continue
  fi
  # 実行ビットに依存せず bash で起動する
  if bash "$script" ${PASS_ARGS[@]+"${PASS_ARGS[@]}"}; then
    echo "-- $name: OK --"
  else
    rc=$?
    echo "-- $name: 失敗 (終了コード $rc) --" >&2
    FAILED+=("$name (終了コード $rc)")
  fi
done

echo
echo "== git diff --stat -- journal/ =="
git diff --stat -- journal/ || true
# 未追跡の新規ファイルも把握できるようにする（読み取りのみ）
untracked="$(git ls-files --others --exclude-standard -- journal/ 2>/dev/null)"
if [[ -n "$untracked" ]]; then
  echo "未追跡:"
  printf '  %s\n' $untracked
fi

echo
if ((${#FAILED[@]} > 0)); then
  echo "== 失敗した節 (${#FAILED[@]} 件) ==" >&2
  printf '  - %s\n' "${FAILED[@]}" >&2
  exit 1
fi
echo "== 全節成功 =="
