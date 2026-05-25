#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

SIGNING_IDENTITY="${SIGNING_IDENTITY:-Developer ID Application: Steven Hershey (743QY8VN34)}"
NOTARY_PROFILE="${NOTARY_PROFILE:-tycode-notary}"

log() { echo "==> $*"; }
error() { echo "ERROR: $*" >&2; exit 1; }

run_tauri_clean() {
    env \
        -u NO_COLOR \
        -u TAURI_CONFIG \
        -u TAURI_ENV_TARGET_TRIPLE \
        -u TAURI_ANDROID_PACKAGE_NAME_APP_NAME \
        -u TAURI_ANDROID_PACKAGE_NAME_PREFIX \
        -u CARGO_MANIFEST_DIR \
        -u CARGO_MANIFEST_PATH \
        "$@"
}

usage() {
    cat <<EOF
Usage: $(basename "$0") <command>

Commands:
  check     Type-check the full workspace (native + WASM)
  build     Build the frontend WASM bundle (into frontend/dist/)
  dev       Run the Tauri desktop app from the live workspace with no hot reload
  tauri     Build and run the full Tauri desktop app
  ios       Build/install the bundled standalone iOS app (optional device name)
  release   Build release bundles and, on macOS, sign/notarize them
  clean     Remove build artifacts
EOF
    exit 1
}

ensure_python3() {
    command -v python3 &>/dev/null || error "python3 is required"
}

ensure_wasm_target() {
    if ! rustup target list --installed | grep -q wasm32-unknown-unknown; then
        log "Installing wasm32-unknown-unknown target..."
        rustup target add wasm32-unknown-unknown
    fi
}

ensure_trunk() {
    if ! command -v trunk &>/dev/null; then
        log "Installing trunk..."
        cargo install trunk
    fi
}

ensure_tauri_cli() {
    local tauri_bin="$SCRIPT_DIR/node_modules/.bin/tauri"
    if [[ -x "$tauri_bin" ]]; then
        return 0
    fi

    log "Tauri CLI not found in node_modules; installing frontend dependencies"
    npm install --include=dev

    [[ -x "$tauri_bin" ]] || error "Tauri CLI still missing after npm install. Expected $tauri_bin"
}

warn_running_desktop_host_needs_restart() {
    if [[ "$(uname)" != "Darwin" ]]; then
        return 0
    fi

    local pids
    pids="$(
        ps -axo pid=,command= |
            awk -v needle="$SCRIPT_DIR/target/debug/tyde" '
                {
                    pid = $1
                    $1 = ""
                    sub(/^ /, "")
                    if (index($0, needle) == 1) {
                        print pid
                    }
                }
            '
    )"
    if [[ -z "$pids" ]]; then
        return 0
    fi

    log "Detected running Tyde desktop host process(es): ${pids//$'\n'/, }"
    log "Restart Tyde on your computer after this finishes; running host processes do not pick up rebuilt mobile protocol/transport code."
}

warn_running_simulator_mobile_needs_shutdown() {
    if [[ "$(uname)" != "Darwin" ]]; then
        return 0
    fi

    local pids
    pids="$(pgrep -f "CoreSimulator.*Tyde Mobile.app/Tyde Mobile" 2>/dev/null || true)"
    if [[ -z "$pids" ]]; then
        return 0
    fi

    log "Detected running Tyde Mobile simulator process(es): ${pids//$'\n'/, }"
    log "Shut down the simulator app if you are testing a real phone; stale simulator builds can keep connecting with an old protocol version and confuse server logs."
}

uninstall_existing_ios_mobile_app() {
    local device="$1"
    local output_path
    output_path="$(mktemp)"

    log "Removing existing com.tyde.mobile install on $device so bundled frontend assets cannot stay cached..."
    if xcrun devicectl device uninstall app \
        --device "$device" \
        com.tyde.mobile \
        --timeout 30 >"$output_path" 2>&1; then
        log "Removed existing com.tyde.mobile install."
    else
        if grep -qiE "CoreDeviceService was unable|unable to locate a device|matching the requested device" "$output_path"; then
            cat "$output_path" >&2
            rm -f "$output_path"
            error "Selected iOS device $device is no longer available to CoreDevice"
        fi
        log "No existing com.tyde.mobile install was removed; continuing with a fresh install attempt."
    fi

    rm -f "$output_path"
}

verify_installed_ios_mobile_app() {
    local device="$1"
    local expected_version="$2"
    local json_path
    json_path="$(mktemp)"

    xcrun devicectl device info apps \
        --device "$device" \
        --json-output "$json_path" >/dev/null
    python3 - "$json_path" "$expected_version" <<'PY'
import json
import sys

path = sys.argv[1]
expected = sys.argv[2]
apps = json.load(open(path)).get("result", {}).get("apps", [])
for app in apps:
    if app.get("bundleIdentifier") == "com.tyde.mobile":
        version = app.get("version")
        build = app.get("bundleVersion")
        print(f"==> Installed com.tyde.mobile version {version} build {build}")
        if version != expected or build != expected:
            raise SystemExit(
                f"installed com.tyde.mobile is {version} ({build}), expected {expected}"
            )
        break
else:
    raise SystemExit("com.tyde.mobile was not found after install")
PY
    rm -f "$json_path"
}

resolve_connected_ios_device() {
    local requested="${1:-}"
    local json_path
    json_path="$(mktemp)"
    xcrun devicectl list devices --timeout 10 --json-output "$json_path" >/dev/null
    local status=0
    python3 - "$json_path" "$requested" <<'PY' || status=$?
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
requested = sys.argv[2].strip()
devices = json.loads(path.read_text()).get("result", {}).get("devices", [])


def nested(device, *keys):
    value = device
    for key in keys:
        if not isinstance(value, dict):
            return None
        value = value.get(key)
    return value


def name(device):
    return nested(device, "deviceProperties", "name") or device.get("name")


def identifier(device):
    return device.get("identifier")


def platform(device):
    return nested(device, "hardwareProperties", "platform")


def tunnel_state(device):
    return nested(device, "connectionProperties", "tunnelState") or "unknown"


def is_connected_ios(device):
    return platform(device) == "iOS" and tunnel_state(device) == "connected"


def match_tokens(device):
    hardware = device.get("hardwareProperties", {})
    connection = device.get("connectionProperties", {})
    ecid = hardware.get("ecid")
    values = [
        name(device),
        identifier(device),
        connection.get("hostname"),
        str(ecid) if ecid is not None else None,
        f"ecid_{ecid}" if ecid is not None else None,
    ]
    return {value for value in values if value}


def describe(device):
    device_name = name(device) or "<unnamed>"
    device_id = identifier(device) or "<no identifier>"
    product = nested(device, "hardwareProperties", "productType") or "unknown model"
    return (
        f"{device_name} ({device_id}, {product}, "
        f"tunnelState={tunnel_state(device)})"
    )


ios_devices = [device for device in devices if platform(device) == "iOS"]
if requested:
    matches = [
        device for device in ios_devices
        if requested in match_tokens(device)
        or requested.casefold() in {token.casefold() for token in match_tokens(device)}
    ]
    if not matches:
        visible = "\n".join(f"  - {describe(device)}" for device in ios_devices)
        raise SystemExit(
            f'iOS device "{requested}" was not found by CoreDevice.'
            + (f"\nVisible iOS devices:\n{visible}" if visible else "\nNo iOS devices are visible.")
        )
    if len(matches) != 1:
        visible = "\n".join(f"  - {describe(device)}" for device in matches)
        raise SystemExit(
            f'iOS device "{requested}" matched multiple devices:\n{visible}\n'
            "Pass the exact device identifier instead."
        )
    device = matches[0]
    if not is_connected_ios(device):
        raise SystemExit(
            f'iOS device "{name(device) or requested}" is visible but unavailable to CoreDevice '
            f"(tunnelState={tunnel_state(device)}).\n"
            "Unlock the phone, keep it awake, trust this Mac if prompted, and reconnect the "
            "USB cable or Wi-Fi pairing. Then rerun `xcrun devicectl list devices` until "
            "the device is available/connected."
        )
    print(identifier(device) or name(device))
    raise SystemExit(0)

connected = [device for device in ios_devices if is_connected_ios(device)]
if len(connected) != 1:
    visible = "\n".join(f"  - {describe(device)}" for device in ios_devices)
    raise SystemExit(
        "expected exactly one connected iOS device; "
        f"found {len(connected)}"
        + (f"\nVisible iOS devices:\n{visible}" if visible else "\nNo iOS devices are visible.")
    )
device = connected[0]
print(identifier(device) or name(device))
PY
    rm -f "$json_path"
    return "$status"
}

check_release_versions() {
    ensure_python3
    python3 "$SCRIPT_DIR/tools/check_release_version.py"
}

sign_and_notarize() {
    local target="$1"
    if [[ "$(uname)" != "Darwin" ]]; then
        log "Skipping signing for $target (not macOS)"
        return 0
    fi

    log "Signing $target"
    codesign --force --options runtime --deep --sign "$SIGNING_IDENTITY" "$target"
    codesign --verify --verbose "$target"

    local zip="${target}.zip"
    log "Zipping for notarization"
    ditto -c -k --keepParent "$target" "$zip"

    log "Submitting to Apple for notarization (typically 30s-2min)"
    xcrun notarytool submit "$zip" --keychain-profile "$NOTARY_PROFILE" --wait

    rm "$zip"

    log "Stapling notarization ticket to $target"
    xcrun stapler staple "$target"

    log "Signed and notarized: $target"
}

write_no_reload_tauri_config() {
    local repo_dir port config_path
    repo_dir="$1"
    port="$2"
    config_path="$repo_dir/tauri.no-reload.conf.json"

    python3 - "$repo_dir" "$port" "$config_path" <<'PY'
import json
import pathlib
import shlex
import sys

repo_dir = pathlib.Path(sys.argv[1])
port = sys.argv[2]
config_path = pathlib.Path(sys.argv[3])
source_path = repo_dir / "frontend/tauri-shell/tauri.conf.json"
trunk_config_path = repo_dir / "frontend/Trunk.toml"

with source_path.open() as handle:
    config = json.load(handle)

config["build"]["beforeDevCommand"] = (
    f"trunk serve --port {port} --config {shlex.quote(str(trunk_config_path))} --no-autoreload"
)
config["build"]["devUrl"] = f"http://127.0.0.1:{port}"

with config_path.open("w") as handle:
    json.dump(config, handle, indent=2)
    handle.write("\n")
PY

    printf '%s\n' "$config_path"
}

cmd_check() {
    log "Checking workspace (native)..."
    cargo check
    log "Checking frontend (WASM)..."
    ensure_wasm_target
    cargo check -p frontend --target wasm32-unknown-unknown
    log "All checks passed."
}

cmd_build() {
    ensure_wasm_target
    ensure_trunk
    log "Building frontend WASM bundle..."
    (
        cd "$SCRIPT_DIR/frontend"
        trunk build --release
    )
    log "Built to frontend/dist/"
}

cmd_dev() {
    ensure_wasm_target
    ensure_trunk
    ensure_python3
    ensure_tauri_cli

    local frontend_port config_path
    frontend_port="${TYDE_SNAPSHOT_FRONTEND_PORT:-1420}"
    config_path="$(write_no_reload_tauri_config "$SCRIPT_DIR" "$frontend_port")"

    cleanup_config() {
        rm -f "$config_path"
    }
    trap cleanup_config EXIT

    log "Starting Tauri with hot reload disabled on http://127.0.0.1:${frontend_port}"
    log "This instance runs from the live workspace, but it will not auto-reload. Restart it to pick up changes."

    (
        cd "$SCRIPT_DIR/frontend/tauri-shell"
        run_tauri_clean "$SCRIPT_DIR/node_modules/.bin/tauri" dev --config "$config_path" --no-watch
    )
}

cmd_tauri() {
    ensure_wasm_target
    ensure_trunk
    log "Building frontend..."
    (
        cd "$SCRIPT_DIR/frontend"
        trunk build --release
    )
    log "Building Tauri desktop app..."
    (
        cd "$SCRIPT_DIR/frontend/tauri-shell"
        cargo build --release
    )
    log "Running Tyde..."
    (
        cd "$SCRIPT_DIR/frontend/tauri-shell"
        cargo run --release
    )
}

cmd_ios() {
    ensure_wasm_target
    ensure_trunk
    ensure_tauri_cli
    ensure_python3

    local tauri_bin="$SCRIPT_DIR/node_modules/.bin/tauri"
    local device="${1:-}"
    local install_device
    install_device="$(resolve_connected_ios_device "$device")"
    if [[ -n "$device" && "$install_device" != "$device" ]]; then
        log "Resolved iOS device '$device' to $install_device"
    fi

    log "Building desktop host binary so mobile protocol code is current..."
    cargo build -p tauri-shell --bin tyde
    warn_running_desktop_host_needs_restart
    warn_running_simulator_mobile_needs_shutdown

    local app_version
    app_version="$(python3 -c 'import json; print(json.load(open("mobile/src-tauri/tauri.conf.json"))["version"])')"
    local ios_team="${IOS_DEVELOPMENT_TEAM:-}"
    if [[ -z "$ios_team" && "$SIGNING_IDENTITY" == *"("*")" ]]; then
        ios_team="${SIGNING_IDENTITY##*(}"
        ios_team="${ios_team%)}"
    fi

    log "Removing stale mobile frontend bundle..."
    rm -rf "$SCRIPT_DIR/mobile-frontend/dist"

    if [[ ! -d "$SCRIPT_DIR/mobile/src-tauri/gen/apple" ]]; then
        log "Initializing Tauri iOS target..."
        (
            cd "$SCRIPT_DIR/mobile/src-tauri"
            run_tauri_clean "$tauri_bin" ios init --ci
        )
    fi

    if [[ -n "$ios_team" ]]; then
        log "Configuring iOS development team $ios_team..."
    else
        log "Configuring iOS Xcode build script..."
    fi
    python3 - "$SCRIPT_DIR/mobile/src-tauri/gen/apple/project.yml" "$ios_team" "$app_version" <<'PY'
import re
import sys
from pathlib import Path

path = Path(sys.argv[1])
team = sys.argv[2]
app_version = sys.argv[3]
text = path.read_text()
text, script_count = re.subn(
    r"(?m)^      - script: .*?\btauri ios xcode-script\b(?P<args>.*)$",
    (
        r"      - script: env -u TAURI_CONFIG "
        r"-u TAURI_ENV_TARGET_TRIPLE -u CARGO_MANIFEST_DIR "
        r"-u CARGO_MANIFEST_PATH node ../../../../node_modules/.bin/tauri "
        r"ios xcode-script\g<args>"
    ),
    text,
)
if script_count != 1:
    raise SystemExit(f"could not find exactly one Tauri iOS Xcode script in {path}")
text = re.sub(
    r"(?m)^        CFBundleShortVersionString:.*$",
    f"        CFBundleShortVersionString: {app_version}",
    text,
)
text = re.sub(
    r"(?m)^        CFBundleVersion:.*$",
    f'        CFBundleVersion: "{app_version}"',
    text,
)
if team:
    text = re.sub(r"(?m)^        DEVELOPMENT_TEAM:.*\n", "", text)
    text = re.sub(r"(?m)^        CODE_SIGN_STYLE:.*\n", "", text)
    marker = "        ENABLE_BITCODE: false\n"
    if marker not in text:
        raise SystemExit(f"could not find iOS target settings marker in {path}")
    text = text.replace(
        marker,
        marker + f"        DEVELOPMENT_TEAM: {team}\n        CODE_SIGN_STYLE: Automatic\n",
        1,
    )
path.write_text(text)
PY
    (
        cd "$SCRIPT_DIR/mobile/src-tauri/gen/apple"
        xcodegen generate --spec project.yml
    )

    local apple_build_dir="$SCRIPT_DIR/mobile/src-tauri/gen/apple/build"
    local app_bundle="$apple_build_dir/tyde-mobile-shell_iOS.xcarchive/Products/Applications/Tyde Mobile.app"
    rm -rf "$apple_build_dir/tyde-mobile-shell_iOS.xcarchive" "$apple_build_dir/arm64"

    log "Building bundled iOS app from frontendDist (no laptop dev server)..."
    local build_status=0
    (
        cd "$SCRIPT_DIR/mobile/src-tauri"
        run_tauri_clean "$tauri_bin" ios build --debug --export-method debugging --ci
    ) || build_status=$?

    if [[ "$build_status" -ne 0 ]]; then
        log "Tauri iOS export exited with status $build_status; checking for built app bundle..."
    fi
    [[ -d "$app_bundle" ]] || error "Built iOS app bundle not found at $app_bundle"
    [[ -x "$app_bundle/Tyde Mobile" ]] || error "Built iOS app binary not found at $app_bundle/Tyde Mobile"
    local app_strings
    app_strings="$(mktemp)"
    strings "$app_bundle/Tyde Mobile" >"$app_strings"
    if grep -q "com.tyde.app" "$app_strings"; then
        rm -f "$app_strings"
        error "Built iOS app embedded the desktop Tauri identifier com.tyde.app"
    fi
    if ! grep -q "com.tyde.mobile" "$app_strings"; then
        rm -f "$app_strings"
        error "Built iOS app did not embed the mobile Tauri identifier com.tyde.mobile"
    fi
    rm -f "$app_strings"
    codesign --verify --deep --strict "$app_bundle"

    uninstall_existing_ios_mobile_app "$install_device"

    log "Installing Tyde Mobile on $install_device..."
    xcrun devicectl device install app \
        --device "$install_device" \
        "$app_bundle" \
        --timeout 60
    verify_installed_ios_mobile_app "$install_device" "$app_version"

    log "Launching Tyde Mobile on $install_device..."
    local launch_status=0
    xcrun devicectl device process launch \
        --device "$install_device" \
        com.tyde.mobile \
        --terminate-existing \
        --timeout 30 || launch_status=$?
    if [[ "$launch_status" -ne 0 ]]; then
        log "Install succeeded, but automatic launch failed. Unlock the phone and open Tyde Mobile manually."
    fi
}

cmd_release() {
    ensure_wasm_target
    ensure_trunk
    ensure_tauri_cli
    check_release_versions

    log "Building Tyde release bundle"
    (
        cd "$SCRIPT_DIR/frontend/tauri-shell"
        run_tauri_clean "$SCRIPT_DIR/node_modules/.bin/tauri" build
    )

    local bundle_dir="$SCRIPT_DIR/target/release/bundle"

    if [[ "$(uname)" != "Darwin" ]]; then
        log "Release build complete. Bundles are in $bundle_dir/"
        return 0
    fi

    local app_bundle
    app_bundle="$(find "$bundle_dir/macos" -maxdepth 1 -name '*.app' | head -1)"
    [[ -n "$app_bundle" ]] || error "No macOS .app bundle found in $bundle_dir/macos"

    sign_and_notarize "$app_bundle"

    local app_name dmg_dir dmg staging_dir dmg_exit=0
    app_name="$(basename "$app_bundle" .app)"
    dmg_dir="$bundle_dir/dmg"
    rm -f "$dmg_dir"/*.dmg
    dmg="$dmg_dir/${app_name}.dmg"

    if ! command -v create-dmg &>/dev/null; then
        error "create-dmg not found. Install with: brew install create-dmg"
    fi

    log "Creating DMG from signed app"
    staging_dir="$(mktemp -d)"
    cp -R "$app_bundle" "$staging_dir/"

    create-dmg \
        --volname "$app_name" \
        --window-pos 200 120 \
        --window-size 660 400 \
        --icon-size 80 \
        --icon "$app_name.app" 180 220 \
        --app-drop-link 480 220 \
        --no-internet-enable \
        "$dmg" \
        "$staging_dir" || dmg_exit=$?

    rm -rf "$staging_dir"

    if [[ "$dmg_exit" -ne 0 && "$dmg_exit" -ne 2 ]]; then
        error "create-dmg failed with exit code $dmg_exit"
    fi

    sign_and_notarize "$dmg"

    log "Release build complete. Bundles are in $bundle_dir/"
}

cmd_clean() {
    log "Cleaning build artifacts..."
    cargo clean
    rm -rf frontend/dist
    log "Clean."
}

[[ $# -lt 1 ]] && usage

case "$1" in
    check)     cmd_check ;;
    build)     cmd_build ;;
    dev)       cmd_dev ;;
    tauri)     cmd_tauri ;;
    ios)       shift; cmd_ios "$@" ;;
    release)   cmd_release ;;
    clean)     cmd_clean ;;
    *)         usage ;;
esac
