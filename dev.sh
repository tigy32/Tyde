#!/usr/bin/env bash

set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1

cd "$(dirname "$0")"

readonly DEV_CHECK_CACHE_SCHEMA="1"
readonly DEV_CHECK_CACHE_DIR="target/dev-check-cache"

section() {
    printf '\n==> %s\n' "$1"
}

die() {
    printf 'ERROR: %s\n' "$1" >&2
    exit 1
}

hash_text() {
    git hash-object --stdin
}

hash_command() {
    local label="$1"
    shift
    local output

    if ! output="$("$@" 2>&1)"; then
        die "could not read $label identity"
    fi
    printf 'tool.%s.version=%s\n' "$label" "$(printf '%s\n' "$output" | head -n 1)"
    printf 'tool.%s.identity=%s\n' "$label" "$(printf '%s' "$output" | hash_text)"
}

worktree_identity() {
    local temp_dir temp_index real_index head_commit head_tree staged_tree worktree_tree
    temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/tyde-dev-check-index.XXXXXX")"
    temp_index="$temp_dir/index"

    if ! real_index="$(git rev-parse --git-path index)" ||
        ! head_commit="$(git rev-parse --verify HEAD)" ||
        ! head_tree="$(git rev-parse --verify 'HEAD^{tree}')" ||
        ! staged_tree="$(GIT_INDEX_FILE="$real_index" git write-tree)" ||
        ! GIT_INDEX_FILE="$temp_index" git read-tree "$staged_tree" ||
        ! GIT_INDEX_FILE="$temp_index" git add -A -- . ||
        ! worktree_tree="$(GIT_INDEX_FILE="$temp_index" git write-tree)"; then
        rm -rf "$temp_dir"
        die "could not fingerprint the Git worktree"
    fi

    rm -rf "$temp_dir"
    printf 'git.head_commit=%s\n' "$head_commit"
    printf 'git.head_tree=%s\n' "$head_tree"
    printf 'git.staged_tree=%s\n' "$staged_tree"
    printf 'git.worktree_tree=%s\n' "$worktree_tree"
}

environment_identity() {
    local name value_hash
    local -a names=()

    while IFS= read -r name; do
        case "$name" in
            CI | HOME | PATH | PATHEXT | SHELL | USERPROFILE | LANG | LC_* | TZ | \
                CLAUDE_CONFIG_DIR | HERMES_PYTHON | NO_COLOR | NODE_OPTIONS | \
                WASM_BINDGEN_TEST_WEBDRIVER_JSON | CHROME* | CHROMEDRIVER | \
                RUST* | CARGO* | NEXTEST* | \
                TYDE* | CC | CXX | AR | CFLAGS | CPPFLAGS | CXXFLAGS | LDFLAGS | \
                MACOSX_DEPLOYMENT_TARGET | SDKROOT | LD_LIBRARY_PATH | DYLD_* | \
                ASAN_OPTIONS | LSAN_OPTIONS | MSAN_OPTIONS | TSAN_OPTIONS | \
                UBSAN_OPTIONS | HTTP_PROXY | HTTPS_PROXY | NO_PROXY | \
                http_proxy | https_proxy | no_proxy)
                names+=("$name")
                ;;
        esac
    done < <(compgen -e | LC_ALL=C sort)

    printf 'env.names='
    if [[ ${#names[@]} -gt 0 ]]; then
        (IFS=,; printf '%s' "${names[*]}")
    fi
    printf '\n'

    for name in "${names[@]}"; do
        value_hash="$(printf '%s=%s' "$name" "${!name}" | hash_text)"
        printf 'env.%s=%s\n' "$name" "$value_hash"
    done

    for name in \
        TYDE_RUN_REAL_AI_TESTS \
        TYDE_LIVE_CODEX_TEST \
        TYDE_RUN_CLAUDE_INTEGRATION; do
        printf 'env.%s=unset\n' "$name"
    done
}

cache_inputs() {
    local path
    local -a relevant_files=(
        dev.sh
        .config/nextest.toml
        tools/run-nextest-binary.sh
        tools/run-wasm-tests.sh
    )

    printf 'cache.schema=%s\n' "$DEV_CHECK_CACHE_SCHEMA"
    worktree_identity

    for path in "${relevant_files[@]}"; do
        [[ -f "$path" ]] || die "cache input is missing: $path"
        printf 'script.%s=%s\n' "$path" "$(git hash-object "$path")"
    done

    printf 'shell.bash.version=%s\n' "$BASH_VERSION"
    printf 'platform.os=%s\n' "$(uname -s)"
    printf 'platform.release=%s\n' "$(uname -r)"
    printf 'platform.arch=%s\n' "$(uname -m)"
    hash_command git git --version
    hash_command rustc rustc -Vv
    hash_command cargo cargo -Vv
    hash_command nextest cargo nextest --version
    hash_command node node --version
    if command -v rustup >/dev/null 2>&1; then
        hash_command rustup rustup show active-toolchain
        hash_command rust-targets rustup target list --installed
    else
        printf 'tool.rustup.version=unavailable\n'
        printf 'tool.rustup.identity=unavailable\n'
    fi
    environment_identity
}

cache_key_for_inputs() {
    printf '%s\n' "$1" | hash_text
}

success_summary() {
    local repetitions="$1"
    cat <<SUMMARY
Prior successful stage summary:
  cargo fmt --all --check: 1/1 passed
  cargo check --all-targets: 1/1 passed
  cargo clippy --all-targets -- -D warnings: 1/1 passed
  cargo nextest run: $repetitions/$repetitions passed
  tools/run-wasm-tests.sh: $repetitions/$repetitions passed
  web loader tests: $repetitions/$repetitions passed
SUMMARY
}

cache_record_is_valid() {
    local path="$1"
    local key="$2"
    [[ -f "$path" ]] &&
        grep -Fqx "schema=$DEV_CHECK_CACHE_SCHEMA" "$path" &&
        grep -Fqx "key=$key" "$path" &&
        grep -Fqx "complete=true" "$path"
}

write_cache_record() {
    local path="$1"
    local key="$2"
    local repetitions="$3"
    local temp_path

    mkdir -p "$DEV_CHECK_CACHE_DIR"
    temp_path="$(mktemp "$DEV_CHECK_CACHE_DIR/.success.XXXXXX")"
    if ! {
        printf 'schema=%s\n' "$DEV_CHECK_CACHE_SCHEMA"
        printf 'key=%s\n' "$key"
        printf 'complete=true\n'
        printf 'completed_at=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        success_summary "$repetitions"
    } >"$temp_path" || ! mv -f "$temp_path" "$path"; then
        rm -f "$temp_path"
        die "could not write dev check cache record"
    fi
}

run_repeated_stage() {
    local label="$1"
    local repetitions="$2"
    shift 2
    local run

    for ((run = 1; run <= repetitions; run++)); do
        section "$label (run $run/$repetitions)"
        "$@"
    done
}

run_web_loader_tests() {
    (
        cd web/loader
        node --test test/*.test.js
    )
}

check_usage() {
    cat <<'USAGE'
Usage: ./dev.sh check [--force | --no-cache | --explain-cache]

  --force         Ignore a cached success, run authoritative 3x tests, and cache success
  --no-cache      Run every stage once without reading or writing the cache
  --explain-cache Print the canonical cache inputs and key without running checks
USAGE
}

check() {
    local mode="default"
    local repetitions=3
    local cache_read=true
    local cache_write=true
    local inputs key record_path refreshed_inputs refreshed_key

    if [[ $# -gt 1 ]]; then
        check_usage >&2
        exit 2
    fi
    case "${1:-}" in
        "") ;;
        --force)
            mode="force"
            cache_read=false
            ;;
        --no-cache)
            mode="no-cache"
            repetitions=1
            cache_read=false
            cache_write=false
            ;;
        --explain-cache)
            mode="explain"
            cache_read=false
            cache_write=false
            ;;
        -h | --help)
            check_usage
            return
            ;;
        *)
            check_usage >&2
            exit 2
            ;;
    esac

    unset TYDE_RUN_REAL_AI_TESTS
    unset TYDE_LIVE_CODEX_TEST
    unset TYDE_RUN_CLAUDE_INTEGRATION

    if [[ -n "${CI:-}" && "$mode" != "force" && "$mode" != "explain" ]]; then
        die "CI must invoke ./dev.sh check --force"
    fi

    if ! command -v cargo-nextest >/dev/null 2>&1; then
        die "cargo-nextest is required. Install it with: cargo install cargo-nextest --locked"
    fi

    if [[ "$mode" != "no-cache" ]]; then
        inputs="$(cache_inputs)"
        key="$(cache_key_for_inputs "$inputs")"
        record_path="$DEV_CHECK_CACHE_DIR/$key.success"
    fi

    if [[ "$mode" == "explain" ]]; then
        printf '%s\n' "Dev check cache inputs:" "$inputs"
        printf 'cache.key=%s\n' "$key"
        printf 'cache.record=%s\n' "$record_path"
        return
    fi

    if [[ "$cache_read" == true ]] && cache_record_is_valid "$record_path" "$key"; then
        refreshed_inputs="$(cache_inputs)"
        refreshed_key="$(cache_key_for_inputs "$refreshed_inputs")"
        if [[ "$refreshed_key" == "$key" ]]; then
            section "dev check cache hit"
            printf 'Cache key: %s\n' "$key"
            sed -n '/^Prior successful stage summary:/,$p' "$record_path"
            return
        fi
        inputs="$refreshed_inputs"
        key="$refreshed_key"
        record_path="$DEV_CHECK_CACHE_DIR/$key.success"
    fi

    if [[ "$mode" == "no-cache" ]]; then
        section "dev check cache disabled"
    elif [[ "$mode" == "force" ]]; then
        section "dev check cache bypassed"
        printf 'Cache key: %s\n' "$key"
    else
        section "dev check cache miss"
        printf 'Cache key: %s\n' "$key"
    fi

    section "cargo fmt --all --check"
    cargo fmt --all --check

    section "cargo check --all-targets"
    cargo check --all-targets

    section "cargo clippy --all-targets -- -D warnings"
    cargo clippy --all-targets -- -D warnings

    run_repeated_stage "cargo nextest run" "$repetitions" cargo nextest run
    run_repeated_stage "tools/run-wasm-tests.sh" "$repetitions" tools/run-wasm-tests.sh
    run_repeated_stage "web loader tests" "$repetitions" run_web_loader_tests

    if [[ "$cache_write" == true ]]; then
        refreshed_inputs="$(cache_inputs)"
        refreshed_key="$(cache_key_for_inputs "$refreshed_inputs")"
        if [[ "$refreshed_key" != "$key" ]]; then
            die "cache inputs changed while checks were running; success was not cached"
        fi
        write_cache_record "$record_path" "$key" "$repetitions"
        section "dev check passed and cached"
        printf 'Cache key: %s\n' "$key"
        success_summary "$repetitions"
    else
        section "dev check passed without cache"
        success_summary "$repetitions"
    fi
}

usage() {
    printf 'Usage: %s check [--force | --no-cache | --explain-cache]\n' "$0"
    printf '       %s release <command> [args]\n' "$0"
}

case "${1:-}" in
    check)
        shift
        check "$@"
        ;;
    release)
        shift
        exec tools/release.sh "$@"
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
