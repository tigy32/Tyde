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
    local bundle_flags=()
    if [[ "$(uname)" == "Linux" ]]; then
        bundle_flags=(--bundles appimage)
    fi
    "$SCRIPT_DIR/node_modules/.bin/tauri" build "${bundle_flags[@]}"

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
    hdiutil create -volname "$app_name" -srcfolder "$app_bundle" \
        -ov -format UDZO "$dmg"

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
