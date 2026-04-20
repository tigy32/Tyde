#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

SIGNING_IDENTITY="${SIGNING_IDENTITY:-Developer ID Application: Steven Hershey (743QY8VN34)}"
NOTARY_PROFILE="${NOTARY_PROFILE:-tycode-notary}"

log() { echo "==> $*"; }
error() { echo "ERROR: $*" >&2; exit 1; }

usage() {
    cat <<EOF
Usage: $(basename "$0") <command>

Commands:
  check     Type-check the full workspace (native + WASM)
  build     Build the frontend WASM bundle (into frontend/dist/)
  dev       Run the Tauri desktop app from the live workspace with no hot reload
  tauri     Build and run the full Tauri desktop app
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
        env -u NO_COLOR "$SCRIPT_DIR/node_modules/.bin/tauri" dev --config "$config_path" --no-watch
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

cmd_release() {
    ensure_wasm_target
    ensure_trunk
    ensure_tauri_cli
    check_release_versions

    log "Building Tyde release bundle"
    (
        cd "$SCRIPT_DIR/frontend/tauri-shell"
        env -u NO_COLOR "$SCRIPT_DIR/node_modules/.bin/tauri" build
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
    release)   cmd_release ;;
    clean)     cmd_clean ;;
    *)         usage ;;
esac
