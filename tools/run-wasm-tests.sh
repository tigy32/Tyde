#!/usr/bin/env bash
#
# Run the frontend's wasm-bindgen tests in a headless browser.
#
# Handles the things that make this annoying to set up by hand:
#   - uses locally installed Chrome when available, otherwise downloads
#     Chrome for Testing (cached under target/wasm-test-cache/),
#   - downloads a chromedriver that matches Chrome,
#   - ad-hoc re-signs a newly downloaded driver during preparation on macOS,
#   - installs wasm-bindgen-cli at the version pinned in Cargo.lock,
#   - sets CHROMEDRIVER and runs `cargo test --target wasm32-unknown-unknown`
#     in `frontend/`, passing any extra args through.
#
# Usage:
#   tools/run-wasm-tests.sh                  # run all wasm tests
#   tools/run-wasm-tests.sh wasm_tests::     # filter to wasm tests
#   tools/run-wasm-tests.sh some_test_name   # filter to a single test
#   tools/run-wasm-tests.sh --prepare FILE   # provision once for dev.sh
#   tools/run-wasm-tests.sh --identity       # read-only current identity
#
# Requires: cargo; curl; unzip.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cache_dir="$repo_root/target/wasm-test-cache"
versions_json="$cache_dir/known-good-versions.json"
channels_json="$cache_dir/last-known-good-versions.json"
prepared_identity="$cache_dir/prepared.identity"
mode="run"
prepare_output=""
webdriver_source_json="${TYDE_WASM_WEBDRIVER_SOURCE_JSON:-${WASM_BINDGEN_TEST_WEBDRIVER_JSON:-}}"

case "${1:-}" in
    --prepare)
        [[ $# -eq 2 ]] || {
            printf 'Usage: %s --prepare <environment-output>\n' "$0" >&2
            exit 2
        }
        mode="prepare"
        prepare_output="$2"
        ;;
    --identity)
        [[ $# -eq 1 ]] || {
            printf 'Usage: %s --identity\n' "$0" >&2
            exit 2
        }
        mode="identity"
        ;;
esac

log() { printf '[run-wasm-tests] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

[[ -z "${CHROME:-}" || -x "$CHROME" ]] || die "CHROME is not executable: $CHROME"
[[ -z "${CHROMEDRIVER:-}" || -x "$CHROMEDRIVER" ]] ||
    die "CHROMEDRIVER is not executable: $CHROMEDRIVER"
[[ -z "${WASM_BINDGEN_TEST_RUNNER:-}" || -x "$WASM_BINDGEN_TEST_RUNNER" ]] ||
    die "WASM_BINDGEN_TEST_RUNNER is not executable: $WASM_BINDGEN_TEST_RUNNER"
if [[ -n "${WASM_BINDGEN_TEST_RUNNER:-}" &&
    "$(basename "$WASM_BINDGEN_TEST_RUNNER")" != "wasm-bindgen-test-runner" ]]; then
    die "WASM_BINDGEN_TEST_RUNNER must be named wasm-bindgen-test-runner so Cargo runs the fingerprinted executable"
fi

sha256_file() {
    python3 - "$1" <<'PY'
import hashlib
import sys

digest = hashlib.sha256()
with open(sys.argv[1], "rb") as source:
    for chunk in iter(lambda: source.read(1024 * 1024), b""):
        digest.update(chunk)
print(digest.hexdigest())
PY
}

validate_prepared_identity() {
    local identity="$1"
    local stored_chrome stored_chrome_version stored_driver stored_driver_version
    local stored_runner stored_runner_version stored_required current_version raw_version
    local current_driver_number
    local webdriver_source webdriver_identity required

    stored_chrome="$(sed -n 's/^wasm.chrome.path=//p' "$identity")"
    stored_chrome_version="$(sed -n 's/^wasm.chrome.version=//p' "$identity")"
    stored_driver="$(sed -n 's/^wasm.chromedriver.path=//p' "$identity")"
    stored_driver_version="$(sed -n 's/^wasm.chromedriver.version=//p' "$identity")"
    stored_runner="$(sed -n 's/^wasm.bindgen.path=//p' "$identity")"
    stored_runner_version="$(sed -n 's/^wasm.bindgen.version=//p' "$identity")"
    stored_required="$(sed -n 's/^wasm.bindgen.required=//p' "$identity")"
    [[ -n "$stored_chrome_version" ]] ||
        die "prepared identity has no Chrome version: $identity"
    [[ -n "$stored_driver_version" ]] ||
        die "prepared identity has no chromedriver version: $identity"
    [[ -n "$stored_runner_version" ]] ||
        die "prepared identity has no wasm runner version: $identity"
    [[ -n "$stored_required" ]] ||
        die "prepared identity has no required wasm-bindgen version: $identity"
    [[ -x "$stored_chrome" ]] ||
        die "prepared identity Chrome is missing or not executable: $stored_chrome"
    [[ -x "$stored_driver" ]] ||
        die "prepared identity chromedriver is missing or not executable: $stored_driver"
    [[ -x "$stored_runner" ]] ||
        die "prepared identity wasm runner is missing or not executable: $stored_runner"
    [[ "$(basename "$stored_runner")" == "wasm-bindgen-test-runner" ]] ||
        die "prepared identity wasm runner has the wrong basename: $stored_runner"
    [[ -z "${CHROMEDRIVER:-}" || "$CHROMEDRIVER" == "$stored_driver" ]] ||
        die "explicit CHROMEDRIVER does not match prepared identity: $stored_driver"
    [[ -z "${WASM_BINDGEN_TEST_RUNNER:-}" ||
        "$WASM_BINDGEN_TEST_RUNNER" == "$stored_runner" ]] ||
        die "explicit WASM_BINDGEN_TEST_RUNNER does not match prepared identity: $stored_runner"

    raw_version="$("$stored_chrome" --version 2>/dev/null || true)"
    current_version="$(printf '%s' "$raw_version" \
        | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | head -n1 || true)"
    [[ "$current_version" == "$stored_chrome_version" ]] ||
        die "prepared identity Chrome changed from $stored_chrome_version to ${current_version:-unreadable}"
    raw_version="$("$stored_driver" --version 2>/dev/null || true)"
    current_version="$(printf '%s\n' "$raw_version" | head -n1)"
    [[ "$current_version" == "$stored_driver_version" ]] ||
        die "prepared identity chromedriver changed from $stored_driver_version to ${current_version:-unreadable}"
    current_driver_number="$(printf '%s' "$current_version" \
        | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | head -n1 || true)"
    [[ -n "$current_driver_number" ]] ||
        die "prepared identity chromedriver version is invalid: $current_version"
    [[ "${stored_chrome_version%%.*}" == "${current_driver_number%%.*}" ]] ||
        die "prepared identity Chrome and chromedriver have different major versions"
    raw_version="$("$stored_runner" --version 2>/dev/null || true)"
    current_version="$(printf '%s\n' "$raw_version" | awk '{print $NF}')"
    [[ "$current_version" == "$stored_runner_version" ]] ||
        die "prepared identity wasm runner changed from $stored_runner_version to ${current_version:-unreadable}"
    required="$(awk '/^name = "wasm-bindgen"$/ { getline; sub(/version = "/, ""); sub(/"$/, ""); print; exit }' \
        "$repo_root/Cargo.lock")"
    [[ "$required" == "$stored_required" ]] ||
        die "prepared identity requires wasm-bindgen $stored_required but Cargo.lock requires $required"

    webdriver_source="$(sed -n 's/^wasm.webdriver.source=//p' "$identity")"
    webdriver_identity="$(sed -n 's/^wasm.webdriver.identity=sha256://p' "$identity")"
    [[ -n "$webdriver_source" ]] ||
        die "prepared identity has no webdriver source: $identity"
    if [[ -n "$webdriver_source" && "$webdriver_source" != "default" ]]; then
        [[ -f "$webdriver_source" ]] ||
            die "prepared identity webdriver config is missing: $webdriver_source"
        [[ -n "$webdriver_identity" ]] ||
            die "prepared identity webdriver config has no SHA-256 identity"
        [[ "$(sha256_file "$webdriver_source")" == "$webdriver_identity" ]] ||
            die "prepared identity webdriver config changed: $webdriver_source"
    fi
}

refresh_cft_metadata() {
    mkdir -p "$cache_dir"
    local tmp="$versions_json.tmp"
    if curl -fsSL \
        "https://googlechromelabs.github.io/chrome-for-testing/known-good-versions-with-downloads.json" \
        -o "$tmp"; then
        mv "$tmp" "$versions_json"
    elif [[ -s "$versions_json" ]]; then
        rm -f "$tmp"
        log "could not refresh Chrome for Testing metadata; using cached copy"
    else
        rm -f "$tmp"
        die "could not download Chrome for Testing metadata"
    fi
}

refresh_cft_channels_metadata() {
    mkdir -p "$cache_dir"
    local tmp="$channels_json.tmp"
    if curl -fsSL \
        "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json" \
        -o "$tmp"; then
        mv "$tmp" "$channels_json"
    elif [[ -s "$channels_json" ]]; then
        rm -f "$tmp"
        log "could not refresh Chrome for Testing channel metadata; using cached copy"
    else
        rm -f "$tmp"
        die "could not download Chrome for Testing channel metadata"
    fi
}

stable_cft_version_with_chrome() {
    python3 - "$channels_json" "$cft_platform" <<'PY'
import json, sys

channels_path, platform = sys.argv[1], sys.argv[2]
with open(channels_path) as f:
    data = json.load(f)

stable = data["channels"]["Stable"]

def has_download(kind):
    downloads = stable.get("downloads", {}).get(kind, [])
    return any(download.get("platform") == platform for download in downloads)

if not has_download("chrome") or not has_download("chromedriver"):
    sys.exit(f"no stable Chrome for Testing download for {platform}")
print(stable["version"])
PY
}

stable_cft_download_url() {
    local kind="$1"
    local version="$2"
    python3 - "$channels_json" "$kind" "$version" "$cft_platform" <<'PY'
import json, sys

channels_path, kind, wanted_version, platform = sys.argv[1:5]
with open(channels_path) as f:
    data = json.load(f)

stable = data["channels"]["Stable"]
if stable["version"] != wanted_version:
    sys.exit(
        f"stable Chrome for Testing changed from {wanted_version} to {stable['version']}"
    )
for download in stable.get("downloads", {}).get(kind, []):
    if download.get("platform") == platform:
        print(download["url"])
        sys.exit(0)

sys.exit(f"no stable {kind} download for {wanted_version} on {platform}")
PY
}

cft_download_url() {
    local kind="$1"
    local version="$2"
    python3 - "$versions_json" "$kind" "$version" "$cft_platform" <<'PY'
import json, sys

versions_path, kind, wanted_version, platform = sys.argv[1:5]
with open(versions_path) as f:
    data = json.load(f)

for version in data["versions"]:
    if version["version"] != wanted_version:
        continue
    for download in version.get("downloads", {}).get(kind, []):
        if download.get("platform") == platform:
            print(download["url"])
            sys.exit(0)
    sys.exit(f"no {kind} download for {wanted_version} on {platform}")

sys.exit(f"Chrome for Testing version {wanted_version} not found")
PY
}

latest_chromedriver_url_for_major() {
    python3 - "$versions_json" "$chrome_major" "$cft_platform" <<'PY'
import json, sys

versions_path, major, platform = sys.argv[1], sys.argv[2], sys.argv[3]
with open(versions_path) as f:
    data = json.load(f)

def version_key(version):
    return tuple(int(part) for part in version["version"].split("."))

matching = [
    version for version in data["versions"]
    if version["version"].startswith(f"{major}.")
    and any(
        download.get("platform") == platform
        for download in version.get("downloads", {}).get("chromedriver", [])
    )
]
if not matching:
    sys.exit(f"no chromedriver download for Chrome major {major} on {platform}")

chosen = sorted(matching, key=version_key)[-1]
for download in chosen["downloads"]["chromedriver"]:
    if download.get("platform") == platform:
        print(download["url"])
        sys.exit(0)
PY
}

find_cft_chrome_bin() {
    local chrome_dir="$1"
    if [[ "$(uname -s)" == "Darwin" ]]; then
        find "$chrome_dir" \
            -path "*/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing" \
            -type f -perm -u+x 2>/dev/null | head -n1 || true
    else
        find "$chrome_dir" -path "*/chrome-linux64/chrome" -type f -perm -u+x 2>/dev/null \
            | head -n1 || true
    fi
}

write_webdriver_config() {
    local chrome_bin="$1"
    local source_json="${WASM_BINDGEN_TEST_WEBDRIVER_JSON:-}"
    local output_json="$cache_dir/webdriver-chrome-$chrome_major-$cft_platform.json"
    python3 - "$chrome_bin" "$source_json" "$output_json" <<'PY'
import json, os, sys

chrome_bin, source_json, output_json = sys.argv[1:4]
capabilities = {}
if source_json:
    with open(source_json) as f:
        capabilities = json.load(f)

chrome_options = capabilities.setdefault("goog:chromeOptions", {})
if not isinstance(chrome_options, dict):
    sys.exit("goog:chromeOptions must be a JSON object")
chrome_options["binary"] = chrome_bin

tmp = f"{output_json}.tmp"
os.makedirs(os.path.dirname(output_json), exist_ok=True)
with open(tmp, "w") as f:
    json.dump(capabilities, f, indent=2)
    f.write("\n")
os.replace(tmp, output_json)
PY
    export WASM_BINDGEN_TEST_WEBDRIVER_JSON="$output_json"
}

write_prepared_environment() {
    local output="$1"
    local temporary="$output.tmp.$$"
    local identity_output="$output.identity"
    local identity_temporary="$identity_output.tmp.$$"
    mkdir -p "$(dirname "$output")"
    print_identity >"$identity_temporary"
    mv "$identity_temporary" "$identity_output"
    cp "$identity_output" "$prepared_identity.tmp.$$"
    mv "$prepared_identity.tmp.$$" "$prepared_identity"
    {
        printf 'export TYDE_WASM_TOOLS_PREPARED=1\n'
        printf 'export CHROME=%q\n' "$chrome_bin"
        printf 'export CHROMEDRIVER=%q\n' "$driver_bin"
        printf 'export WASM_BINDGEN_TEST_RUNNER=%q\n' "$runner_bin"
        printf 'export TYDE_WASM_WEBDRIVER_SOURCE_JSON=%q\n' "$webdriver_source_json"
        printf 'export WASM_BINDGEN_TEST_WEBDRIVER_JSON=%q\n' \
            "$WASM_BINDGEN_TEST_WEBDRIVER_JSON"
        printf 'export TYDE_WASM_IDENTITY_FILE=%q\n' "$identity_output"
    } >"$temporary"
    mv "$temporary" "$output"
}

print_identity() {
    printf 'wasm.chrome.source=%s\n' "$chrome_source"
    printf 'wasm.chrome.path=%s\n' "$chrome_bin"
    printf 'wasm.chrome.version=%s\n' "$chrome_version_full"
    printf 'wasm.chromedriver.source=%s\n' "$driver_source"
    printf 'wasm.chromedriver.path=%s\n' "$driver_bin"
    printf 'wasm.chromedriver.version=%s\n' "$driver_version"
    printf 'wasm.bindgen.required=%s\n' "$wb_version"
    printf 'wasm.bindgen.path=%s\n' "$runner_bin"
    printf 'wasm.bindgen.version=%s\n' "$installed"
    if [[ -n "$webdriver_source_json" ]]; then
        [[ -f "$webdriver_source_json" ]] ||
            die "WASM_BINDGEN_TEST_WEBDRIVER_JSON does not exist: $webdriver_source_json"
        printf 'wasm.webdriver.source=%s\n' "$webdriver_source_json"
        printf 'wasm.webdriver.identity=sha256:%s\n' \
            "$(sha256_file "$webdriver_source_json")"
    else
        printf 'wasm.webdriver.source=default\n'
    fi
}

# ── Platform detection ────────────────────────────────────────────────────
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)  cft_platform="mac-arm64" ;;
    Darwin-x86_64) cft_platform="mac-x64" ;;
    Linux-x86_64)  cft_platform="linux64" ;;
    Linux-aarch64) cft_platform="" ;;
    *) die "unsupported platform: $(uname -s)-$(uname -m)" ;;
esac

# ── Locate Chrome and read its version ────────────────────────────────────
chrome_bin=""
downloaded_chrome=0
chrome_source=""
if [[ -n "${CHROME:-}" ]]; then
    [[ -x "$CHROME" ]] || die "CHROME is not executable: $CHROME"
    chrome_bin="$CHROME"
    chrome_source="override"
elif [[ "${TYDE_WASM_TOOLS_PREPARED:-0}" == 1 ]]; then
    die "prepared wasm tools require an explicit CHROME path"
elif [[ "$(uname -s)" == "Darwin" ]]; then
    chrome_bin="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    [[ -x "$chrome_bin" ]] && chrome_source="system"
else
    for candidate in google-chrome google-chrome-stable chromium chromium-browser; do
        if command -v "$candidate" >/dev/null 2>&1; then
            chrome_bin="$(command -v "$candidate")"
            chrome_source="system"
            break
        fi
    done
fi

if [[ ! -x "$chrome_bin" ]]; then
    if [[ "$mode" == "identity" ]]; then
        if [[ -s "$prepared_identity" ]]; then
            validate_prepared_identity "$prepared_identity"
            cat "$prepared_identity"
            exit 0
        fi
        printf 'wasm.chrome.source=unprovisioned-stable-cft\n'
        printf 'wasm.platform=%s\n' "$cft_platform"
        printf 'wasm.bindgen.required=%s\n' \
            "$(awk '/^name = "wasm-bindgen"$/ { getline; sub(/version = "/, ""); sub(/"$/, ""); print; exit }' "$repo_root/Cargo.lock")"
        exit 0
    fi
    [[ -n "$cft_platform" ]] \
        || die "Google Chrome not found and Chrome for Testing is unavailable for $(uname -s)-$(uname -m)"

    log "Google Chrome not found; using stable Chrome for Testing fallback…"
    refresh_cft_channels_metadata
    chrome_version_full="$(stable_cft_version_with_chrome)"
    chrome_major="${chrome_version_full%%.*}"
    chrome_dir="$cache_dir/chrome-$chrome_version_full-$cft_platform"
    chrome_bin="$(find_cft_chrome_bin "$chrome_dir")"

    if [[ ! -x "$chrome_bin" ]]; then
        mkdir -p "$chrome_dir"
        chrome_zip="$chrome_dir/chrome.zip"
        chrome_url="$(stable_cft_download_url chrome "$chrome_version_full")"
        curl -fsSL "$chrome_url" -o "$chrome_zip"
        unzip -q -o "$chrome_zip" -d "$chrome_dir"
        rm -f "$chrome_zip"
        chrome_bin="$(find_cft_chrome_bin "$chrome_dir")"
        [[ -x "$chrome_bin" ]] || die "Chrome for Testing binary not found in download"
    fi

    if [[ "$(uname -s)" == "Darwin" ]]; then
        xattr -dr com.apple.quarantine "$chrome_dir" 2>/dev/null || true
    fi

    downloaded_chrome=1
    chrome_source="chrome-for-testing"
fi

chrome_version_full="$("$chrome_bin" --version 2>/dev/null \
    | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | head -n1)"
[[ -n "$chrome_version_full" ]] || die "could not parse Chrome version"
chrome_major="${chrome_version_full%%.*}"
if [[ $downloaded_chrome -eq 1 ]]; then
    log "using Chrome for Testing $chrome_version_full at $chrome_bin"
else
    log "detected Chrome $chrome_version_full (major $chrome_major)"
fi

# ── Find or download a matching chromedriver ──────────────────────────────
# Cache by major version. Chrome auto-updates within a major; chromedriver is
# only required to match the major number per Chrome for Testing's policy.
driver_source=""
downloaded_driver=0
if [[ -n "${CHROMEDRIVER:-}" ]]; then
    [[ -x "$CHROMEDRIVER" ]] || die "CHROMEDRIVER is not executable: $CHROMEDRIVER"
    driver_bin="$CHROMEDRIVER"
    driver_source="override"
elif [[ "${TYDE_WASM_TOOLS_PREPARED:-0}" == 1 ]]; then
    die "prepared wasm tools require an explicit CHROMEDRIVER path"
elif [[ -z "$cft_platform" ]]; then
    driver_bin=""
    for candidate in chromedriver \
        /snap/chromium/current/usr/lib/chromium-browser/chromedriver \
        /snap/chromium/*/usr/lib/chromium-browser/chromedriver; do
        if [[ "$candidate" == "chromedriver" ]]; then
            if command -v chromedriver >/dev/null 2>&1; then
                driver_bin="$(command -v chromedriver)"
                driver_source="system"
                break
            fi
        elif [[ -x "$candidate" ]]; then
                driver_bin="$candidate"
                driver_source="system"
                break
        fi
    done
    [[ -x "$driver_bin" ]] || die "chromedriver not found for $(uname -s)-$(uname -m)"
else
    driver_dir="$cache_dir/chromedriver-$chrome_major-$cft_platform"
    driver_bin="$driver_dir/chromedriver"
    driver_source="chrome-for-testing"
fi

if [[ -n "$cft_platform" && ! -x "$driver_bin" ]]; then
    [[ "$mode" != "identity" ]] || {
        printf 'wasm.chrome.source=%s\n' "$chrome_source"
        printf 'wasm.chrome.path=%s\n' "$chrome_bin"
        printf 'wasm.chrome.version=%s\n' "$chrome_version_full"
        printf 'wasm.chromedriver.source=unprovisioned-for-major-%s\n' "$chrome_major"
        printf 'wasm.bindgen.required=%s\n' \
            "$(awk '/^name = "wasm-bindgen"$/ { getline; sub(/version = "/, ""); sub(/"$/, ""); print; exit }' "$repo_root/Cargo.lock")"
        exit 0
    }
    log "downloading chromedriver for Chrome major $chrome_major ($cft_platform)…"
    mkdir -p "$driver_dir"
    refresh_cft_metadata
    if [[ $downloaded_chrome -eq 1 ]]; then
        download_url="$(cft_download_url chromedriver "$chrome_version_full")"
    else
        download_url="$(latest_chromedriver_url_for_major)"
    fi
    [[ -n "$download_url" ]] || die "failed to resolve chromedriver download URL"
    zip_path="$driver_dir/chromedriver.zip"
    curl -fsSL "$download_url" -o "$zip_path"
    unzip -q -o "$zip_path" -d "$driver_dir"
    # Chrome for Testing zips contain a subdir like `chromedriver-mac-arm64/`.
    found="$(find "$driver_dir" -name chromedriver -type f -perm -u+x 2>/dev/null \
        | head -n1 || true)"
    [[ -n "$found" ]] || die "chromedriver binary not found in download"
    if [[ "$found" != "$driver_bin" ]]; then
        mv "$found" "$driver_bin"
    fi
    rm -f "$zip_path"
    downloaded_driver=1
fi

# macOS Gatekeeper rejects the downloaded binary; ad-hoc re-sign it once.
if [[ "$mode" == "prepare" && $downloaded_driver -eq 1 &&
    "$(uname -s)" == "Darwin" ]]; then
    if ! "$driver_bin" --version >/dev/null 2>&1; then
        log "ad-hoc signing chromedriver for macOS…"
        codesign --remove-signature "$driver_bin" 2>/dev/null || true
        codesign -s - --force "$driver_bin"
    fi
fi

if ! driver_version="$("$driver_bin" --version 2>/dev/null | head -n1)"; then
    die "chromedriver is not executable by the OS: $driver_bin; run preparation to provision a check-owned driver"
fi
driver_version_full="$(printf '%s' "$driver_version" \
    | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | head -n1)"
[[ -n "$driver_version_full" ]] || die "could not parse chromedriver version: $driver_version"
[[ "${driver_version_full%%.*}" == "$chrome_major" ]] ||
    die "Chrome $chrome_version_full and chromedriver $driver_version_full have different major versions"
log "using $driver_version"

# ── Ensure wasm-bindgen-test-runner is at the lockfile version ────────────
wb_version="$(awk '/^name = "wasm-bindgen"$/ { getline; sub(/version = "/, ""); sub(/"$/, ""); print; exit }' \
    "$repo_root/Cargo.lock")"
[[ -n "$wb_version" ]] || die "could not read wasm-bindgen version from Cargo.lock"

needs_install=1
runner_bin=""
if [[ -n "${WASM_BINDGEN_TEST_RUNNER:-}" ]]; then
    [[ -x "$WASM_BINDGEN_TEST_RUNNER" ]] ||
        die "WASM_BINDGEN_TEST_RUNNER is not executable: $WASM_BINDGEN_TEST_RUNNER"
    runner_bin="$WASM_BINDGEN_TEST_RUNNER"
elif command -v wasm-bindgen-test-runner >/dev/null 2>&1; then
    runner_bin="$(command -v wasm-bindgen-test-runner)"
fi
if [[ -n "$runner_bin" ]]; then
    installed="$("$runner_bin" --version 2>/dev/null | awk '{print $NF}')"
    [[ "$installed" == "$wb_version" ]] && needs_install=0
fi
if [[ $needs_install -eq 1 ]]; then
    [[ "${TYDE_WASM_TOOLS_PREPARED:-0}" != 1 ]] ||
        die "prepared wasm-bindgen-test-runner ${installed:-missing} does not match required $wb_version"
    if [[ "$mode" == "identity" ]]; then
        printf 'wasm.chrome.source=%s\n' "$chrome_source"
        printf 'wasm.chrome.path=%s\n' "$chrome_bin"
        printf 'wasm.chrome.version=%s\n' "$chrome_version_full"
        printf 'wasm.chromedriver.source=%s\n' "$driver_source"
        printf 'wasm.chromedriver.path=%s\n' "$driver_bin"
        printf 'wasm.chromedriver.version=%s\n' "$driver_version"
        printf 'wasm.bindgen.required=%s\n' "$wb_version"
        printf 'wasm.bindgen.source=unprovisioned\n'
        exit 0
    fi
    log "installing wasm-bindgen-cli@$wb_version (this may take a minute)…"
    cargo install wasm-bindgen-cli --version "$wb_version" --locked
    runner_bin="$(command -v wasm-bindgen-test-runner)"
    installed="$("$runner_bin" --version 2>/dev/null | awk '{print $NF}')"
    [[ "$installed" == "$wb_version" ]] ||
        die "installed wasm-bindgen-test-runner $installed does not match required $wb_version"
fi

if [[ "$mode" == "identity" ]]; then
    print_identity
    exit 0
fi

if [[ "$mode" == "prepare" ]]; then
    write_webdriver_config "$chrome_bin"
    write_prepared_environment "$prepare_output"
    print_identity
    exit 0
fi
if [[ "${TYDE_WASM_TOOLS_PREPARED:-0}" == 1 ]]; then
    [[ -f "${WASM_BINDGEN_TEST_WEBDRIVER_JSON:-}" ]] ||
        die "prepared webdriver configuration is missing: ${WASM_BINDGEN_TEST_WEBDRIVER_JSON:-unset}"
else
    write_webdriver_config "$chrome_bin"
fi

# ── Run the tests ─────────────────────────────────────────────────────────
export CHROMEDRIVER="$driver_bin"
export PATH="$(dirname "$runner_bin"):$PATH"

cd "$repo_root/frontend"
log "running: cargo test --target wasm32-unknown-unknown $* (frontend)"
cargo test --target wasm32-unknown-unknown "$@"

cd "$repo_root/mobile-frontend"
log "running: cargo test --target wasm32-unknown-unknown $* (mobile-frontend)"
exec cargo test --target wasm32-unknown-unknown "$@"
