#!/usr/bin/env bash
# tokio-postgres のシナリオを流す（Cargo.lock で固定。ネットワークが要る）。
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
export CARGO_TARGET_DIR="${COMPAT_CARGO_TARGET_DIR:-${CARGO_TARGET_DIR:-/tmp/yuzhu-compat-target}}"
exec cargo run --locked --quiet --release
