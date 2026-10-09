#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "${TYDE_RUN_TYCHAT_TESTS:-}" == 1 ]] || { echo 'Set TYDE_RUN_TYCHAT_TESTS=1 for the real local-Tychat integration test' >&2; exit 2; }
[[ -d "${TYCHAT_REPO:-}/crates/tychat-bot" ]] || { echo 'TYCHAT_REPO must name the real Tychat repository' >&2; exit 2; }
export CARGO_INCREMENTAL=0
exec cargo nextest run -p server --test tychat_real --run-ignored only --success-output final --status-level pass
