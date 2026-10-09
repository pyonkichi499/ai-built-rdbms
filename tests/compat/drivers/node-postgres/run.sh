#!/usr/bin/env bash
# node-postgres のシナリオを流す（Node.js 18 以上。依存は package-lock.json で固定。ネットワークが要る）。
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
command -v node >/dev/null 2>&1 || { echo "RESULT setup FAIL node が見つからない（Node.js 18 以上が必要）"; exit 1; }
command -v npm >/dev/null 2>&1 || { echo "RESULT setup FAIL npm が見つからない"; exit 1; }
if [ ! -d node_modules/pg ]; then
    npm ci --no-audit --no-fund --silent >&2 || { echo "RESULT setup FAIL npm ci に失敗"; exit 1; }
fi
# JS のタイムゾーンを UTC 以外にして、Date パラメータ（timestamptz）の往復を踏む
TZ=Asia/Tokyo exec node main.js
