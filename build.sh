#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SIGNING_IDENTITY="${SIGNING_IDENTITY:-Developer ID Application: Steven Hershey (743QY8VN34)}"
NOTARY_PROFILE="${NOTARY_PROFILE:-tycode-notary}"

log() { echo "==> $*"; }
error() { echo "ERROR: $*" >&2; exit 1; }

sign_and_notarize() {
    local target="$1"
    if [[ "$(uname)" != "Darwin" ]]; then
        log "Skipping signing (not macOS)"
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

ensure_tauri_cli() {
    local tauri_bin="$SCRIPT_DIR/node_modules/.bin/tauri"
    if [[ -x "$tauri_bin" ]]; then
        return 0
    fi

    log "Tauri CLI not found in node_modules; installing frontend dependencies"
    cd "$SCRIPT_DIR"
    npm install --include=dev

    if [[ ! -x "$tauri_bin" ]]; then
        error "Tauri CLI still missing after npm install. Expected $tauri_bin"
    fi
}

cmd_release() {
    ensure_tauri_cli
    log "Building Tyde release bundle"
    cd "$SCRIPT_DIR"
    if [[ "$(uname)" == "Linux" ]]; then
        "$SCRIPT_DIR/node_modules/.bin/tauri" build --bundles appimage
    else
        "$SCRIPT_DIR/node_modules/.bin/tauri" build
    fi

    local bundle_dir="$SCRIPT_DIR/src-tauri/target/release/bundle"

    # Sign and notarize the .app bundle first
    local app_bundle
    app_bundle="$(find "$bundle_dir/macos" -name '*.app' -maxdepth 1 | head -1)"
    if [[ -n "$app_bundle" ]]; then
        sign_and_notarize "$app_bundle"
    fi

    # Recreate DMG from the now-signed .app (Tauri's DMG has the unsigned copy)
    local app_name
    app_name="$(basename "$app_bundle" .app)"
    local dmg_dir="$bundle_dir/dmg"
    rm -f "$dmg_dir"/*.dmg

    local dmg="$dmg_dir/${app_name}.dmg"
    log "Creating DMG from signed app"

    if ! command -v create-dmg &>/dev/null; then
        error "create-dmg not found. Install with: brew install create-dmg"
    fi

    # create-dmg needs a staging folder containing the .app
    local staging_dir
    staging_dir=$(mktemp -d)
    cp -R "$app_bundle" "$staging_dir/"

    # create-dmg returns exit code 2 when it can't set icon positions (common in CI)
    # but the DMG is still valid, so we allow it
    create-dmg \
        --volname "$app_name" \
        --window-pos 200 120 \
        --window-size 660 400 \
        --icon-size 80 \
        --icon "$app_name.app" 180 220 \
        --app-drop-link 480 220 \
        --no-internet-enable \
        "$dmg" \
        "$staging_dir" || local dmg_exit=$?

    rm -rf "$staging_dir"

    if [[ "${dmg_exit:-0}" -ne 0 && "${dmg_exit:-0}" -ne 2 ]]; then
        error "create-dmg failed with exit code $dmg_exit"
    fi

    sign_and_notarize "$dmg"

    log "Release build complete. Bundles are in $bundle_dir/"
}

cmd_debug() {
    ensure_tauri_cli
    log "Building Tyde debug bundle"
    cd "$SCRIPT_DIR"
    local bundle_type="app"
    if [[ "$(uname)" == "Linux" ]]; then
        bundle_type="appimage"
    fi
    "$SCRIPT_DIR/node_modules/.bin/tauri" build --debug --bundles "$bundle_type"
    log "Debug build complete."
}

cmd_setup() {
    log "Installing frontend dependencies"
    cd "$SCRIPT_DIR"
    npm install
    log "Setup complete."
}

usage() {
    cat <<EOF
Usage: $(basename "$0") [command]

Commands:
  release   Build release bundle (default)
  debug     Build debug bundle
  setup     Install frontend dependencies
EOF
}

case "${1:-release}" in
    release) cmd_release ;;
    debug)   cmd_debug ;;
    setup)   cmd_setup ;;
    -h|--help|help) usage ;;
    *) error "Unknown command: $1. Run with --help for usage." ;;
esac
