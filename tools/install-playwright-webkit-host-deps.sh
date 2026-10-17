#!/usr/bin/env bash
# Host libraries for the lockfile-pinned Playwright WebKit.
# Stable promotion runs ./dev.sh check in an older candidate checkout, so the
# workflow tree that is actually executing must install these first.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ ! -x node_modules/.bin/playwright ]]; then
  printf 'playwright is not installed; run npm ci before WebKit host libraries\n' >&2
  exit 1
fi

exec node_modules/.bin/playwright install-deps webkit
