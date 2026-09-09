#!/usr/bin/env bash
#
# Point this clone's git hooks at the tracked .githooks/ directory.
# core.hooksPath is local config (not version-controlled), so every clone must
# run this once. Idempotent.
#
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
git config core.hooksPath .githooks
chmod +x .githooks/* 2>/dev/null || true
echo "git hooks installed: core.hooksPath -> .githooks"
echo "active hooks: $(ls .githooks 2>/dev/null | tr '\n' ' ')"
