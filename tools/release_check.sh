#!/usr/bin/env bash
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

TEMP_DIR=""

log() { echo "==> $*"; }
die() { echo "ERROR: $*" >&2; exit 1; }

require_command() {
    local name="$1"
    local install_hint="$2"

    if ! command -v "$name" >/dev/null 2>&1; then
        die "$name is required. $install_hint"
    fi
}

require_wasm_target() {
    if ! rustup target list --installed | grep -qx "wasm32-unknown-unknown"; then
        die "wasm32-unknown-unknown Rust target is required. Install it with: rustup target add wasm32-unknown-unknown"
    fi
}

cleanup() {
    if [[ -n "${TEMP_DIR:-}" ]]; then
        rm -rf "$TEMP_DIR"
    fi
}
trap cleanup EXIT

usage() {
    cat <<'USAGE'
Usage: tools/release_check.sh [vX.Y.Z]

Runs the canonical local release guard: release-version consistency, AGENTS
checks, mobile web tooling checks, and a generated mobile web manifest coherence
check for the current release version.
USAGE
}

if [[ $# -gt 1 ]]; then
    usage >&2
    exit 2
fi

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
esac

require_command python3 "Install Python 3 and rerun release-check."
require_command cargo "Install Rust/Cargo and rerun release-check."
require_command cargo-nextest "Install it with: cargo install cargo-nextest --locked"
require_command rustup "Install rustup or add the wasm32-unknown-unknown target in this Rust toolchain."
require_command node "Install Node.js and rerun release-check."
require_command trunk "Install it with: cargo install trunk"
require_wasm_target

VERSION_ARG="${1:-}"
if [[ -n "$VERSION_ARG" ]]; then
    log "Checking release version against $VERSION_ARG"
    VERSION="$(python3 tools/check_release_version.py "$VERSION_ARG")"
else
    log "Checking release version"
    VERSION="$(python3 tools/check_release_version.py)"
fi
VERSION="${VERSION#v}"
log "Release version: $VERSION"

log "Checking release tooling shell syntax"
bash -n dev.sh tools/release.sh tools/release_check.sh

log "Checking release tooling Python syntax"
python3 -B - \
    tools/check_release_version.py \
    tools/set_release_version.py \
    tools/release_tool.py <<'PY'
import pathlib
import sys

for raw_path in sys.argv[1:]:
    path = pathlib.Path(raw_path)
    compile(path.read_text(encoding="utf-8"), str(path), "exec")
PY

log "Running release tooling Python tests"
python3 -B -m unittest \
    tools/test_dev_check.py \
    tools/test_release_tooling.py \
    tools/test_check_mobile_web_manifest.py

log "Running canonical dev checks"
./dev.sh check

log "Running web deploy manifest tests"
node --test web/deploy/*.test.mjs

log "Checking web deploy shell syntax"
bash -n web/deploy/deploy.sh

TEMP_DIR="$(mktemp -d)"
TEMP_DIST="$TEMP_DIR/mobile-frontend-dist"
TEMP_MANIFEST="$TEMP_DIR/manifest.json"

log "Building mobile-frontend release bundle for /tyde/v$VERSION/"
(
    cd mobile-frontend
    env -u NO_COLOR trunk build --release \
        --public-url "/tyde/v$VERSION/" \
        --dist "$TEMP_DIST" \
        "$REPO_ROOT/mobile-frontend/index.html"
)

log "Generating temporary mobile web release manifest"
node web/deploy/generate-manifest.mjs \
    --protocol-source protocol/src/types.rs \
    --manifest web/loader/manifest.json \
    --dist "$TEMP_DIST" \
    --version "$VERSION" \
    --prefix /tyde \
    --out "$TEMP_MANIFEST"

log "Validating temporary mobile web release manifest"
python3 tools/check_mobile_web_manifest.py \
    --manifest "$TEMP_MANIFEST" \
    --protocol-source protocol/src/types.rs \
    "v$VERSION"


log "Release check passed."
