#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

usage() {
    echo "Usage: $0 <command>"
    echo ""
    echo "Commands:"
    echo "  check     Type-check the full workspace (native + WASM)"
    echo "  build     Build the frontend WASM bundle (into frontend/dist/)"
    echo "  dev       Run the Tauri desktop app from the live workspace with no hot reload"
    echo "  tauri     Build and run the full Tauri desktop app"
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
    if ! command -v python3 &>/dev/null; then
        echo "python3 is required for dev" >&2
        exit 1
    fi

    local repo_dir frontend_port config_path
    repo_dir="$(pwd)"
    frontend_port="${TYDE_SNAPSHOT_FRONTEND_PORT:-1420}"
    config_path="$(write_no_reload_tauri_config "$repo_dir" "$frontend_port")"

    cleanup_config() {
        rm -f "$config_path"
    }
    trap cleanup_config EXIT

    echo "==> Starting Tauri with hot reload disabled on http://127.0.0.1:${frontend_port}"
    echo "==> This instance runs from the live workspace, but it will not auto-reload. Restart it to pick up changes."

    cd "$repo_dir/frontend/tauri-shell"
    npx tauri dev --config "$config_path" --no-watch
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
    clean)     cmd_clean ;;
    *)         usage ;;
esac
