#!/usr/bin/env bash
# psycopg 3 のシナリオを流す（uv。依存は uv.lock で固定。ネットワークが要る）。
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
command -v uv >/dev/null 2>&1 || { echo "RESULT setup FAIL uv が見つからない"; exit 1; }
uv run --frozen python main.py
