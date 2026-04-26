#!/usr/bin/env bash
#
# Run the frontend's wasm-bindgen tests in a headless browser.
#
# Handles the things that make this annoying to set up by hand:
#   - downloads a chromedriver that matches the locally installed Chrome
#     (Chrome for Testing, cached under target/wasm-test-cache/),
#   - ad-hoc re-signs it on macOS so Gatekeeper doesn't kill it,
#   - installs wasm-bindgen-cli at the version pinned in Cargo.lock,
#   - sets CHROMEDRIVER and runs `cargo test --target wasm32-unknown-unknown`
#     in `frontend/`, passing any extra args through.
#
# Usage:
#   tools/run-wasm-tests.sh                  # run all wasm tests
#   tools/run-wasm-tests.sh wasm_tests::     # filter to wasm tests
#   tools/run-wasm-tests.sh some_test_name   # filter to a single test
#
# Requires: Google Chrome installed locally; cargo; curl; unzip.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cache_dir="$repo_root/target/wasm-test-cache"

log() { printf '[run-wasm-tests] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

# ── Platform detection ────────────────────────────────────────────────────
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)  cft_platform="mac-arm64" ;;
    Darwin-x86_64) cft_platform="mac-x64" ;;
    Linux-x86_64)  cft_platform="linux64" ;;
    *) die "unsupported platform: $(uname -s)-$(uname -m)" ;;
esac

# ── Locate Chrome and read its version ────────────────────────────────────
chrome_bin=""
if [[ "$(uname -s)" == "Darwin" ]]; then
    chrome_bin="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
else
    for candidate in google-chrome google-chrome-stable chromium chromium-browser; do
        if command -v "$candidate" >/dev/null 2>&1; then
            chrome_bin="$(command -v "$candidate")"
            break
        fi
    done
fi
[[ -x "$chrome_bin" ]] || die "Google Chrome not found. Install it first."

chrome_version_full="$("$chrome_bin" --version 2>/dev/null \
    | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | head -n1)"
[[ -n "$chrome_version_full" ]] || die "could not parse Chrome version"
chrome_major="${chrome_version_full%%.*}"
log "detected Chrome $chrome_version_full (major $chrome_major)"

# ── Find or download a matching chromedriver ──────────────────────────────
# Cache by major version. Chrome auto-updates within a major; chromedriver is
# only required to match the major number per Chrome for Testing's policy.
driver_dir="$cache_dir/chromedriver-$chrome_major-$cft_platform"
driver_bin="$driver_dir/chromedriver"

if [[ ! -x "$driver_bin" ]]; then
    log "downloading chromedriver for Chrome major $chrome_major ($cft_platform)…"
    mkdir -p "$driver_dir"
    versions_json="$cache_dir/known-good-versions.json"
    curl -fsSL \
        "https://googlechromelabs.github.io/chrome-for-testing/known-good-versions-with-downloads.json" \
        -o "$versions_json"
    download_url="$(python3 - "$versions_json" "$chrome_major" "$cft_platform" <<'PY'
import json, sys
versions_path, major, platform = sys.argv[1], sys.argv[2], sys.argv[3]
with open(versions_path) as f:
    data = json.load(f)
matching = [
    v for v in data["versions"]
    if v["version"].startswith(f"{major}.")
    and any(d["platform"] == platform for d in v.get("downloads", {}).get("chromedriver", []))
]
if not matching:
    sys.exit(f"no chromedriver download for Chrome major {major} on {platform}")
chosen = matching[-1]  # latest in major
for d in chosen["downloads"]["chromedriver"]:
    if d["platform"] == platform:
        print(d["url"])
        sys.exit(0)
PY
)"
    [[ -n "$download_url" ]] || die "failed to resolve chromedriver download URL"
    zip_path="$driver_dir/chromedriver.zip"
    curl -fsSL "$download_url" -o "$zip_path"
    unzip -q -o "$zip_path" -d "$driver_dir"
    # Chrome for Testing zips contain a subdir like `chromedriver-mac-arm64/`.
    found="$(find "$driver_dir" -name chromedriver -type f -perm -u+x | head -n1)"
    [[ -n "$found" ]] || die "chromedriver binary not found in download"
    if [[ "$found" != "$driver_bin" ]]; then
        mv "$found" "$driver_bin"
    fi
    rm -f "$zip_path"
fi

# macOS Gatekeeper rejects the downloaded binary; ad-hoc re-sign it once.
if [[ "$(uname -s)" == "Darwin" ]]; then
    if ! "$driver_bin" --version >/dev/null 2>&1; then
        log "ad-hoc signing chromedriver for macOS…"
        codesign --remove-signature "$driver_bin" 2>/dev/null || true
        codesign -s - --force "$driver_bin"
    fi
fi

driver_version="$("$driver_bin" --version 2>/dev/null | head -n1)"
log "using $driver_version"

# ── Ensure wasm-bindgen-test-runner is at the lockfile version ────────────
wb_version="$(awk '/^name = "wasm-bindgen"$/ { getline; sub(/version = "/, ""); sub(/"$/, ""); print; exit }' \
    "$repo_root/Cargo.lock")"
[[ -n "$wb_version" ]] || die "could not read wasm-bindgen version from Cargo.lock"

needs_install=1
if command -v wasm-bindgen-test-runner >/dev/null 2>&1; then
    installed="$(wasm-bindgen-test-runner --version 2>/dev/null | awk '{print $NF}')"
    [[ "$installed" == "$wb_version" ]] && needs_install=0
fi
if [[ $needs_install -eq 1 ]]; then
    log "installing wasm-bindgen-cli@$wb_version (this may take a minute)…"
    cargo install wasm-bindgen-cli --version "$wb_version" --locked
fi

# ── Run the tests ─────────────────────────────────────────────────────────
export CHROMEDRIVER="$driver_bin"
cd "$repo_root/frontend"
log "running: cargo test --target wasm32-unknown-unknown $*"
exec cargo test --target wasm32-unknown-unknown "$@"
