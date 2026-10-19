#!/usr/bin/env bash
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

case "${1:-}" in
    -h|--help)
        cat <<'USAGE'
Usage: tools/release_check.sh [vX.Y.Z]

Check clean main, coherent versions, and version-only changes from origin/main.
Source must already have passed the ordinary local workbench and clean-main
./dev.sh check gates before reaching origin/main. No tests or builds run here.
Release cut fetches origin/main and additionally checks approval, tags and pushes.
USAGE
        exit 0
        ;;
esac

python3 -B "$SCRIPT_DIR/release_cadence.py" guard "$@"
