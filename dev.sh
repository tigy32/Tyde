#!/usr/bin/env bash

set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1

cd "$(dirname "$0")"

section() {
    printf '\n==> %s\n' "$1"
}

check() {
    unset TYDE_RUN_REAL_AI_TESTS
    unset TYDE_LIVE_CODEX_TEST
    unset TYDE_RUN_CLAUDE_INTEGRATION

    if ! command -v cargo-nextest >/dev/null 2>&1; then
        printf 'cargo-nextest is required. Install it with: cargo install cargo-nextest --locked\n' >&2
        exit 1
    fi

    section "cargo fmt --all --check"
    cargo fmt --all --check

    section "cargo check --all-targets"
    cargo check --all-targets

    section "cargo clippy --all-targets -- -D warnings"
    cargo clippy --all-targets -- -D warnings

    section "cargo nextest run"
    cargo nextest run

    section "tools/run-wasm-tests.sh"
    tools/run-wasm-tests.sh

    section "web loader tests"
    (
        cd web/loader
        node --test test/*.test.js
    )
}

case "${1:-}" in
    check)
        check
        ;;
    release)
        shift
        exec tools/release.sh "$@"
        ;;
    *)
        printf 'Usage: %s check\n       %s release <command> [args]\n' "$0" "$0" >&2
        exit 2
        ;;
esac
