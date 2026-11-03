#!/usr/bin/env bash
set -euo pipefail
: "${TYDE_NATIVE_CURRENT_CLIENT:?The canonical check owns the current client output directory}"
# Compile the current production client once, outside individual test watchdogs.
# Historical shell/client fixtures are never rebuilt by this path.
tools/build-mobile-web-bundle.sh --out "$TYDE_NATIVE_CURRENT_CLIENT"
exec cargo nextest run
