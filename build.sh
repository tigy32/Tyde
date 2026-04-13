#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

usage() {
    echo "Usage: $0 <command>"
    echo ""
    echo "Commands:"
    echo "  check     Type-check the full workspace (native + WASM)"
    echo "  build     Build the frontend WASM bundle (into frontend/dist/)"
    echo "  dev       Start Trunk dev server with hot-reload (port 1420)"
    echo "  tauri     Build and run the full Tauri desktop app"
    echo "  tauri-dev Run Tauri in dev mode (hot-reload frontend + native shell)"
    echo "  clean     Remove build artifacts"
    exit 1
}

ensure_wasm_target() {
    if ! rustup target list --installed | grep -q wasm32-unknown-unknown; then
        echo "Installing wasm32-unknown-unknown target..."
        rustup target add wasm32-unknown-unknown
    fi
}

ensure_trunk() {
    if ! command -v trunk &>/dev/null; then
        echo "Installing trunk..."
        cargo install trunk
    fi
}

cmd_check() {
    echo "==> Checking workspace (native)..."
    cargo check
    echo "==> Checking frontend (WASM)..."
    ensure_wasm_target
    cargo check -p frontend --target wasm32-unknown-unknown
    echo "==> All checks passed."
}

cmd_build() {
    ensure_wasm_target
    ensure_trunk
    echo "==> Building frontend WASM bundle..."
    cd frontend
    trunk build --release
    echo "==> Built to frontend/dist/"
}

cmd_dev() {
    ensure_wasm_target
    ensure_trunk
    echo "==> Starting Trunk dev server on http://127.0.0.1:1420 ..."
    cd frontend
    trunk serve --port 1420
}

cmd_tauri() {
    ensure_wasm_target
    ensure_trunk
    echo "==> Building frontend..."
    cd frontend
    trunk build --release
    cd tauri-shell
    echo "==> Building Tauri desktop app..."
    cargo build --release
    echo "==> Running Tyde..."
    cargo run --release
}

cmd_tauri_dev() {
    ensure_wasm_target
    ensure_trunk
    echo "==> Starting Trunk dev server on :1420 ..."
    cd frontend
    trunk serve --port 1420 &
    TRUNK_PID=$!
    trap "kill $TRUNK_PID 2>/dev/null" EXIT
    echo "==> Waiting for Trunk dev server..."
    for i in $(seq 1 60); do
        if curl -s http://127.0.0.1:1420 >/dev/null 2>&1; then break; fi
        sleep 1
    done
    echo "==> Building and running Tauri shell..."
    cd tauri-shell
    cargo run
}

cmd_clean() {
    echo "==> Cleaning build artifacts..."
    cargo clean
    rm -rf frontend/dist
    echo "==> Clean."
}

[[ $# -lt 1 ]] && usage

case "$1" in
    check)     cmd_check ;;
    build)     cmd_build ;;
    dev)       cmd_dev ;;
    tauri)     cmd_tauri ;;
    tauri-dev) cmd_tauri_dev ;;
    clean)     cmd_clean ;;
    *)         usage ;;
esac
