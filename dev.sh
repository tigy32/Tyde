#!/usr/bin/env bash

set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1

cd "$(dirname "$0")"

readonly DEV_CHECK_CACHE_SCHEMA="2"
readonly DEV_CHECK_CACHE_DIR="target/dev-check-cache"
readonly DEV_CHECK_LOG_DIR="target/dev-check-logs"
readonly DEV_CHECK_LOCK_DIR="target/dev-check.lock"
readonly DEV_CHECK_LOG_RETENTION=8
readonly DEV_CHECK_CACHE_RETENTION=16
readonly DEV_CHECK_SCCACHE_VERSION="0.16.0"
readonly DEV_CHECK_SCCACHE_SIZE="10G"
readonly DEV_CHECK_SCCACHE_SIZE_BYTES=10737418240

RUN_DIR=""
RUN_METADATA=""
RUN_SUMMARY=""
RUN_STARTED_EPOCH=0
RUN_FINALIZED=false
CHECK_LOCK_HELD=false
STAGE_NUMBER=0
CLEANUP_RECLAIMED_BYTES=0
SCCACHE_STATS_BEFORE=""

die() {
    printf 'ERROR: %s\n' "$1" >&2
    exit 1
}

hash_text() {
    git hash-object --stdin
}

format_bytes() {
    awk -v bytes="$1" 'BEGIN {
        if (bytes >= 1073741824) printf "%.1f GiB", bytes / 1073741824;
        else if (bytes >= 1048576) printf "%.1f MiB", bytes / 1048576;
        else if (bytes >= 1024) printf "%.1f KiB", bytes / 1024;
        else printf "%d B", bytes;
    }'
}

record_disk_snapshot() {
    local label="$1"
    local path filesystem blocks used available capacity mount

    for path in "$PWD" "${TMPDIR:-/tmp}"; do
        read -r filesystem blocks used available capacity mount < <(
            df -Pk "$path" | awk 'NR == 2 { print $1, $2, $3, $4, $5, $6 }'
        )
        printf 'disk.%s.%s.available_kib=%s\n' "$label" \
            "$(printf '%s' "$path" | hash_text)" "$available" >>"$RUN_METADATA"
        printf 'disk.%s.%s.capacity=%s\n' "$label" \
            "$(printf '%s' "$path" | hash_text)" "$capacity" >>"$RUN_METADATA"
        printf 'disk.%s.%s.path=%s\n' "$label" \
            "$(printf '%s' "$path" | hash_text)" "$path" >>"$RUN_METADATA"
    done
}

release_check_lock() {
    if [[ "$CHECK_LOCK_HELD" == true ]]; then
        rm -f "$DEV_CHECK_LOCK_DIR/owner"
        rmdir "$DEV_CHECK_LOCK_DIR" 2>/dev/null || true
        CHECK_LOCK_HELD=false
    fi
}

finish_on_exit() {
    local status=$?
    local finished_epoch

    if [[ -n "$RUN_DIR" && "$RUN_FINALIZED" != true ]]; then
        finished_epoch="$(date +%s)"
        printf 'overall.result=FAIL\n' >>"$RUN_METADATA"
        printf 'overall.exit_status=%s\n' "$status" >>"$RUN_METADATA"
        printf 'overall.duration_seconds=%s\n' \
            "$((finished_epoch - RUN_STARTED_EPOCH))" >>"$RUN_METADATA"
        record_disk_snapshot finish 2>/dev/null || true
    fi
    release_check_lock
}

trap finish_on_exit EXIT
trap 'exit 130' INT TERM HUP

acquire_check_lock() {
    local owner_pid=""
    mkdir -p target

    if ! mkdir "$DEV_CHECK_LOCK_DIR" 2>/dev/null; then
        if [[ -r "$DEV_CHECK_LOCK_DIR/owner" ]]; then
            owner_pid="$(sed -n 's/^pid=//p' "$DEV_CHECK_LOCK_DIR/owner")"
        fi
        if [[ "$owner_pid" =~ ^[0-9]+$ ]] && kill -0 "$owner_pid" 2>/dev/null; then
            die "another ./dev.sh check is already running (PID $owner_pid, lock $DEV_CHECK_LOCK_DIR)"
        fi
        [[ -n "$owner_pid" ]] ||
            die "check lock exists without a readable owner: $DEV_CHECK_LOCK_DIR"
        mv "$DEV_CHECK_LOCK_DIR" "$DEV_CHECK_LOCK_DIR.stale.$$" 2>/dev/null ||
            die "could not reclaim stale check lock: $DEV_CHECK_LOCK_DIR"
        rm -rf "$DEV_CHECK_LOCK_DIR.stale.$$"
        mkdir "$DEV_CHECK_LOCK_DIR" 2>/dev/null ||
            die "another ./dev.sh check acquired the lock: $DEV_CHECK_LOCK_DIR"
    fi

    {
        printf 'pid=%s\n' "$$"
        printf 'started_at=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        printf 'repository=%s\n' "$PWD"
    } >"$DEV_CHECK_LOCK_DIR/owner"
    CHECK_LOCK_HELD=true
    export TYDE_DEV_CHECK_LOCK_HELD=1
}

directory_bytes() {
    local path="$1"
    local kib=0
    if [[ -e "$path" ]]; then
        kib="$(du -sk "$path" 2>/dev/null | awk 'NR == 1 { print $1 }')"
    fi
    printf '%s\n' "$((kib * 1024))"
}

remove_old_entries() {
    local directory="$1"
    local pattern="$2"
    local keep="$3"
    local count=0 entry bytes
    local -a entries=()

    [[ -d "$directory" ]] || return 0
    while IFS= read -r entry; do
        entries+=("$entry")
    done < <(find "$directory" -mindepth 1 -maxdepth 1 -name "$pattern" \
        -print 2>/dev/null | LC_ALL=C sort -r)

    for entry in "${entries[@]}"; do
        count=$((count + 1))
        if ((count > keep)); then
            bytes="$(directory_bytes "$entry")"
            rm -rf "$entry"
            CLEANUP_RECLAIMED_BYTES=$((CLEANUP_RECLAIMED_BYTES + bytes))
        fi
    done
}

cleanup_check_artifacts() {
    local reclaimed

    remove_old_entries "$DEV_CHECK_LOG_DIR" 'run-*' "$((DEV_CHECK_LOG_RETENTION - 1))"
    remove_old_entries "$DEV_CHECK_CACHE_DIR" '*.success' "$DEV_CHECK_CACHE_RETENTION"
    if [[ "$(uname -s)" == "Darwin" ]]; then
        reclaimed="$(tools/run-nextest-binary.sh --cleanup-workspace)"
        [[ "$reclaimed" =~ ^[0-9]+$ ]] ||
            die "nextest cleanup returned an invalid byte count: $reclaimed"
        CLEANUP_RECLAIMED_BYTES=$((CLEANUP_RECLAIMED_BYTES + reclaimed))
    fi
}

initialize_run_log() {
    local timestamp
    timestamp="$(date -u '+%Y%m%dT%H%M%SZ')"
    RUN_DIR="$DEV_CHECK_LOG_DIR/run-$timestamp-$$"
    mkdir -p "$RUN_DIR"
    RUN_METADATA="$RUN_DIR/metadata.txt"
    RUN_SUMMARY="$RUN_DIR/summary.txt"
    RUN_STARTED_EPOCH="$(date +%s)"
    : >"$RUN_SUMMARY"
    {
        printf 'schema=%s\n' "$DEV_CHECK_CACHE_SCHEMA"
        printf 'overall.started_at=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        printf 'cleanup.reclaimed_bytes=%s\n' "$CLEANUP_RECLAIMED_BYTES"
        printf 'cleanup.reclaimed_human=%s\n' "$(format_bytes "$CLEANUP_RECLAIMED_BYTES")"
    } >"$RUN_METADATA"
    record_disk_snapshot start
}

time_command() {
    local timing_file="$1"
    local output_file="$2"
    shift 2

    if [[ "$(uname -s)" == "Darwin" ]]; then
        /usr/bin/time -l -o "$timing_file" "$@" >>"$output_file" 2>&1
    else
        /usr/bin/time -v -o "$timing_file" "$@" >>"$output_file" 2>&1
    fi
}

timing_duration() {
    local timing_file="$1"
    if [[ "$(uname -s)" == "Darwin" ]]; then
        awk '$2 == "real" { print $1; exit }' "$timing_file"
    else
        awk -F': ' '/Elapsed \(wall clock\) time/ {
            split($2, parts, ":");
            if (length(parts) == 2) print parts[1] * 60 + parts[2];
            else print parts[1] * 3600 + parts[2] * 60 + parts[3];
            exit
        }' "$timing_file"
    fi
}

timing_peak_rss_bytes() {
    local timing_file="$1"
    if [[ "$(uname -s)" == "Darwin" ]]; then
        awk '/maximum resident set size/ { print $1; exit }' "$timing_file"
    else
        awk -F': ' '/Maximum resident set size/ { print $2 * 1024; exit }' "$timing_file"
    fi
}

run_stage() {
    local label="$1"
    local repetitions="$2"
    shift 2
    local run status duration peak_rss timing_file
    local total_duration="0"
    local max_peak_rss=0
    local stage_log stage_slug

    STAGE_NUMBER=$((STAGE_NUMBER + 1))
    stage_slug="$(printf '%s' "$label" | tr -cs '[:alnum:]' '-' | sed 's/^-//; s/-$//' | tr '[:upper:]' '[:lower:]')"
    stage_log="$RUN_DIR/$(printf '%02d' "$STAGE_NUMBER")-$stage_slug.log"
    : >"$stage_log"
    printf 'START %s (%s run%s)\n' "$label" "$repetitions" \
        "$([[ "$repetitions" == 1 ]] || printf 's')"

    for ((run = 1; run <= repetitions; run++)); do
        timing_file="$RUN_DIR/.timing-$STAGE_NUMBER-$run"
        printf '\n===== run %s/%s: ' "$run" "$repetitions" >>"$stage_log"
        printf '%q ' "$@" >>"$stage_log"
        printf '=====\n' >>"$stage_log"
        if time_command "$timing_file" "$stage_log" "$@"; then
            status=0
        else
            status=$?
        fi
        duration="$(timing_duration "$timing_file")"
        peak_rss="$(timing_peak_rss_bytes "$timing_file")"
        rm -f "$timing_file"
        [[ "$duration" =~ ^[0-9]+([.][0-9]+)?$ ]] ||
            die "could not read wall timing for stage: $label"
        [[ "$peak_rss" =~ ^[0-9]+$ ]] ||
            die "could not read peak RSS for stage: $label"
        total_duration="$(awk -v total="$total_duration" -v value="$duration" \
            'BEGIN { printf "%.2f", total + value }')"
        ((peak_rss > max_peak_rss)) && max_peak_rss="$peak_rss"

        if ((status != 0)); then
            printf 'FAIL  %s (%s/%s, %ss, peak RSS %s)\n' "$label" "$run" \
                "$repetitions" "$total_duration" "$(format_bytes "$max_peak_rss")" >&2
            printf 'Complete diagnostics: %s\n' "$PWD/$stage_log" >&2
            cat "$stage_log" >&2
            printf 'stage.%02d.result=FAIL\n' "$STAGE_NUMBER" >>"$RUN_METADATA"
            printf 'stage.%02d.label=%s\n' "$STAGE_NUMBER" "$label" >>"$RUN_METADATA"
            printf 'stage.%02d.completed_runs=%s\n' "$STAGE_NUMBER" "$run" >>"$RUN_METADATA"
            printf 'stage.%02d.requested_runs=%s\n' "$STAGE_NUMBER" "$repetitions" >>"$RUN_METADATA"
            printf 'stage.%02d.duration_seconds=%s\n' "$STAGE_NUMBER" "$total_duration" >>"$RUN_METADATA"
            printf 'stage.%02d.peak_rss_bytes=%s\n' "$STAGE_NUMBER" "$max_peak_rss" >>"$RUN_METADATA"
            return "$status"
        fi
    done

    printf 'PASS  %s (%s/%s, %ss, peak RSS %s)\n' "$label" "$repetitions" \
        "$repetitions" "$total_duration" "$(format_bytes "$max_peak_rss")"
    printf 'PASS  %s (%s/%s, %ss, peak RSS %s)\n' "$label" "$repetitions" \
        "$repetitions" "$total_duration" "$(format_bytes "$max_peak_rss")" >>"$RUN_SUMMARY"
    printf 'stage.%02d.result=PASS\n' "$STAGE_NUMBER" >>"$RUN_METADATA"
    printf 'stage.%02d.label=%s\n' "$STAGE_NUMBER" "$label" >>"$RUN_METADATA"
    printf 'stage.%02d.completed_runs=%s\n' "$STAGE_NUMBER" "$repetitions" >>"$RUN_METADATA"
    printf 'stage.%02d.requested_runs=%s\n' "$STAGE_NUMBER" "$repetitions" >>"$RUN_METADATA"
    printf 'stage.%02d.duration_seconds=%s\n' "$STAGE_NUMBER" "$total_duration" >>"$RUN_METADATA"
    printf 'stage.%02d.peak_rss_bytes=%s\n' "$STAGE_NUMBER" "$max_peak_rss" >>"$RUN_METADATA"
    printf 'stage.%02d.log=%s\n' "$STAGE_NUMBER" "$PWD/$stage_log" >>"$RUN_METADATA"
}

prepare_rust_toolchain() {
    local channel active

    channel="$(sed -n 's/^channel = "\([^"]*\)"$/\1/p' rust-toolchain.toml)"
    [[ -n "$channel" && "$channel" != *$'\n'* ]] ||
        die "rust-toolchain.toml must declare exactly one channel"
    [[ "$channel" == "stable" ]] ||
        die "rust-toolchain.toml must declare the stable channel"
    command -v rustup >/dev/null 2>&1 ||
        die "rustup is required to update the repository Rust toolchain"
    run_stage "Update stable Rust toolchain" 1 env -u RUSTUP_TOOLCHAIN rustup update "$channel"
    run_stage "Install repository Rust toolchain" 1 env -u RUSTUP_TOOLCHAIN rustup toolchain install
    export RUSTUP_TOOLCHAIN="$channel"
    active="$(rustup show active-toolchain)" ||
        die "could not resolve the active Rust toolchain"
    [[ "$active" == "$channel"-* ]] ||
        die "stable Rust is required by rust-toolchain.toml; active toolchain is $active"
}

configure_sccache() {
    local executable repository_hash port stats

    command -v sccache >/dev/null 2>&1 ||
        die "sccache $DEV_CHECK_SCCACHE_VERSION is required in PATH"
    executable="$(command -v sccache)"
    [[ "$(sccache --version)" == "sccache $DEV_CHECK_SCCACHE_VERSION" ]] ||
        die "sccache $DEV_CHECK_SCCACHE_VERSION is required; found $(sccache --version 2>&1)"
    repository_hash="$(printf '%s' "$PWD" | hash_text)"
    port=$((45000 + 16#${repository_hash:0:4} % 1000))

    unset SCCACHE_BUCKET SCCACHE_ENDPOINT SCCACHE_REDIS SCCACHE_MEMCACHED
    unset SCCACHE_GCS_BUCKET SCCACHE_GCS_KEY_PATH SCCACHE_AZURE_CONNECTION_STRING
    unset SCCACHE_WEBDAV_ENDPOINT SCCACHE_S3_USE_SSL SCCACHE_GHA_ENABLED
    unset SCCACHE_OSS_BUCKET SCCACHE_COS_BUCKET
    export SCCACHE_DIR="$PWD/target/dev-check-sccache"
    export SCCACHE_CACHE_SIZE="$DEV_CHECK_SCCACHE_SIZE"
    export SCCACHE_IDLE_TIMEOUT=600
    export SCCACHE_SERVER_PORT="$port"
    export RUSTC_WRAPPER="$executable"
    export CARGO_INCREMENTAL=0
    mkdir -p "$SCCACHE_DIR"

    run_stage "Connect pinned local sccache" 1 \
        sccache --show-stats --stats-format=json
    stats="$RUN_DIR/sccache-before.json"
    sccache --show-stats --stats-format=json >"$stats" ||
        die "could not read sccache statistics"
    python3 - "$stats" "$SCCACHE_DIR" "$DEV_CHECK_SCCACHE_SIZE_BYTES" <<'PY'
import json
import pathlib
import sys

stats_path, cache_dir, expected_size = sys.argv[1:]
with open(stats_path, encoding="utf-8") as source:
    data = json.load(source)
expected_location = f'Local disk: "{pathlib.Path(cache_dir)}"'
if data.get("cache_location") != expected_location:
    raise SystemExit(
        f"sccache is not using the check-local cache: {data.get('cache_location')!r}"
    )
if data.get("max_cache_size") != int(expected_size):
    raise SystemExit(
        f"sccache cache limit is {data.get('max_cache_size')!r}, expected {expected_size}"
    )
PY
    SCCACHE_STATS_BEFORE="$stats"
    printf 'sccache.version=%s\n' "$DEV_CHECK_SCCACHE_VERSION" >>"$RUN_METADATA"
    printf 'sccache.executable=%s\n' "$executable" >>"$RUN_METADATA"
    printf 'sccache.directory=%s\n' "$SCCACHE_DIR" >>"$RUN_METADATA"
    printf 'sccache.max_bytes=%s\n' "$DEV_CHECK_SCCACHE_SIZE_BYTES" >>"$RUN_METADATA"
    printf 'sccache.server_port=%s\n' "$SCCACHE_SERVER_PORT" >>"$RUN_METADATA"
    printf 'cargo.incremental=%s\n' "$CARGO_INCREMENTAL" >>"$RUN_METADATA"
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
                RUST* | CARGO* | NEXTEST* | SCCACHE* | \
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

browser_identity() {
    local chrome_bin=""
    local chromedriver_bin=""
    local chrome_version chrome_major platform

    if [[ -n "${CHROME:-}" && -x "$CHROME" ]]; then
        chrome_bin="$CHROME"
    elif [[ "$(uname -s)" == "Darwin" && \
        -x "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" ]]; then
        chrome_bin="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    else
        for chrome_bin in google-chrome google-chrome-stable chromium chromium-browser; do
            if command -v "$chrome_bin" >/dev/null 2>&1; then
                chrome_bin="$(command -v "$chrome_bin")"
                break
            fi
        done
    fi
    [[ -x "$chrome_bin" ]] || die "could not resolve Chrome identity"
    hash_command chrome "$chrome_bin" --version
    chrome_version="$("$chrome_bin" --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | head -n1)"
    [[ -n "$chrome_version" ]] || die "could not parse Chrome identity"
    chrome_major="${chrome_version%%.*}"

    if [[ -n "${CHROMEDRIVER:-}" && -x "$CHROMEDRIVER" ]]; then
        chromedriver_bin="$CHROMEDRIVER"
    else
        case "$(uname -s)-$(uname -m)" in
            Darwin-arm64) platform="mac-arm64" ;;
            Darwin-x86_64) platform="mac-x64" ;;
            Linux-x86_64) platform="linux64" ;;
            *) platform="" ;;
        esac
        if [[ -n "$platform" && \
            -x "target/wasm-test-cache/chromedriver-$chrome_major-$platform/chromedriver" ]]; then
            chromedriver_bin="target/wasm-test-cache/chromedriver-$chrome_major-$platform/chromedriver"
        elif command -v chromedriver >/dev/null 2>&1; then
            chromedriver_bin="$(command -v chromedriver)"
        fi
    fi
    if [[ -x "$chromedriver_bin" ]]; then
        hash_command chromedriver "$chromedriver_bin" --version
    else
        printf 'tool.chromedriver.version=resolved-on-demand-for-chrome-%s\n' "$chrome_major"
        printf 'tool.chromedriver.identity=resolved-by-tools-run-wasm-tests\n'
    fi
}

cache_inputs() {
    local path
    local -a relevant_files=(
        dev.sh
        rust-toolchain.toml
        .config/nextest.toml
        tools/run-nextest-binary.sh
        tools/run-wasm-tests.sh
        tools/test_dev_check.py
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
    hash_command sccache sccache --version
    hash_command wasm-bindgen-test-runner wasm-bindgen-test-runner --version
    browser_identity
    if command -v rustup >/dev/null 2>&1; then
        hash_command rustup rustup show active-toolchain
        hash_command rust-targets rustup target list --installed
    else
        printf 'tool.rustup.version=unavailable\n'
        printf 'tool.rustup.identity=unavailable\n'
    fi
    printf 'sccache.directory=%s\n' "$SCCACHE_DIR"
    printf 'sccache.cache_size=%s\n' "$SCCACHE_CACHE_SIZE"
    printf 'sccache.idle_timeout=%s\n' "$SCCACHE_IDLE_TIMEOUT"
    printf 'sccache.server_port=%s\n' "$SCCACHE_SERVER_PORT"
    printf 'cargo.incremental=%s\n' "$CARGO_INCREMENTAL"
    environment_identity
}

cache_key_for_inputs() {
    printf '%s\n' "$1" | hash_text
}

cache_record_is_valid() {
    local path="$1"
    local key="$2"
    [[ -f "$path" ]] || return 1
    awk -v schema="$DEV_CHECK_CACHE_SCHEMA" -v key="$key" '
        NR == 1 && $0 == "schema=" schema { ordered_schema=1 }
        NR == 2 && $0 == "key=" key { ordered_key=1 }
        NR == 3 && $0 == "complete=true" { ordered_complete=1 }
        NR == 5 && $0 == "summary.begin" { ordered_begin=1 }
        $0 == "schema=" schema { schema_count++ }
        $0 == "key=" key { key_count++ }
        $0 == "complete=true" { complete_count++ }
        $0 == "summary.begin" { begin_count++ }
        $0 == "summary.end" { end_count++ }
        $0 == "record.end=true" { record_end_count++; record_end_line=NR }
        END {
            exit !(ordered_schema && ordered_key && ordered_complete && ordered_begin &&
                schema_count == 1 && key_count == 1 && complete_count == 1 &&
                begin_count == 1 && end_count == 1 && record_end_count == 1 &&
                record_end_line == NR)
        }
    ' "$path"
}

write_cache_record() {
    local path="$1"
    local key="$2"
    local temp_path

    mkdir -p "$DEV_CHECK_CACHE_DIR"
    temp_path="$(mktemp "$DEV_CHECK_CACHE_DIR/.success.XXXXXX")"
    if ! {
        printf 'schema=%s\n' "$DEV_CHECK_CACHE_SCHEMA"
        printf 'key=%s\n' "$key"
        printf 'complete=true\n'
        printf 'completed_at=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        printf 'summary.begin\n'
        cat "$RUN_SUMMARY"
        printf 'summary.end\n'
        printf 'record.end=true\n'
    } >"$temp_path" || ! mv -f "$temp_path" "$path"; then
        rm -f "$temp_path"
        die "could not write dev check cache record"
    fi
}

print_cached_summary() {
    local path="$1"
    sed -n '/^summary.begin$/,/^summary.end$/p' "$path" | sed '1d;$d;s/^/PRIOR /'
}

record_sccache_finish() {
    local after="$RUN_DIR/sccache-after.json"
    local metrics
    sccache --show-stats --stats-format=json >"$after" ||
        die "could not read final sccache statistics"
    metrics="$(python3 - "$SCCACHE_STATS_BEFORE" "$after" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    before = json.load(source)
with open(sys.argv[2], encoding="utf-8") as source:
    after = json.load(source)

def total(mapping):
    return sum(mapping.values())

def values(data):
    stats = data["stats"]
    return {
        "requests": stats["compile_requests"],
        "hits": total(stats["cache_hits"]["counts"]),
        "misses": total(stats["cache_misses"]["counts"]),
        "errors": (
            total(stats["cache_errors"]["counts"])
            + stats.get("cache_read_errors", 0)
            + stats.get("cache_write_errors", 0)
            + stats.get("cache_timeouts", 0)
            + stats.get("compile_fails", 0)
        ),
        "writes": stats["cache_writes"],
    }

old = values(before)
new = values(after)
for name in ("requests", "hits", "misses", "errors", "writes"):
    print(f"{name}={new[name] - old[name]}")
print(f"cache_size={after.get('cache_size') or 0}")
print(f"max_cache_size={after['max_cache_size']}")
PY
)"
    while IFS= read -r line; do
        printf 'sccache.delta.%s\n' "$line" >>"$RUN_METADATA"
    done <<<"$metrics"
    printf '%s\n' "$metrics"
}

finish_success() {
    local cache_state="$1"
    local finished_epoch duration sccache_metrics requests hits misses errors writes

    sccache_metrics="$(record_sccache_finish)"
    requests="$(sed -n 's/^requests=//p' <<<"$sccache_metrics")"
    hits="$(sed -n 's/^hits=//p' <<<"$sccache_metrics")"
    misses="$(sed -n 's/^misses=//p' <<<"$sccache_metrics")"
    errors="$(sed -n 's/^errors=//p' <<<"$sccache_metrics")"
    writes="$(sed -n 's/^writes=//p' <<<"$sccache_metrics")"
    finished_epoch="$(date +%s)"
    duration=$((finished_epoch - RUN_STARTED_EPOCH))
    record_disk_snapshot finish
    {
        printf 'overall.result=PASS\n'
        printf 'overall.cache=%s\n' "$cache_state"
        printf 'overall.duration_seconds=%s\n' "$duration"
        printf 'overall.stage_count=%s\n' "$STAGE_NUMBER"
        printf 'overall.log_directory=%s\n' "$PWD/$RUN_DIR"
    } >>"$RUN_METADATA"
    RUN_FINALIZED=true
    printf 'SCCACHE requests=%s hits=%s misses=%s writes=%s errors=%s\n' \
        "$requests" "$hits" "$misses" "$writes" "$errors"
    printf 'RESULT PASS (cache %s, %ss, %s stages, reclaimed %s; logs %s)\n' \
        "$cache_state" "$duration" "$STAGE_NUMBER" \
        "$(format_bytes "$CLEANUP_RECLAIMED_BYTES")" "$PWD/$RUN_DIR"
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
    local cache_state="miss"
    local inputs key record_path refreshed_inputs refreshed_key

    if [[ $# -gt 1 ]]; then
        check_usage >&2
        exit 2
    fi
    case "${1:-}" in
        "") ;;
        --force)
            mode="force"
            cache_state="bypass"
            cache_read=false
            ;;
        --no-cache)
            mode="no-cache"
            cache_state="disabled"
            repetitions=1
            cache_read=false
            cache_write=false
            ;;
        --explain-cache)
            mode="explain"
            cache_state="explain"
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

    acquire_check_lock
    cleanup_check_artifacts
    initialize_run_log
    printf 'overall.cache_requested=%s\n' "$cache_state" >>"$RUN_METADATA"
    prepare_rust_toolchain
    configure_sccache

    command -v cargo-nextest >/dev/null 2>&1 ||
        die "cargo-nextest is required. Install it with: cargo install cargo-nextest --locked"

    if [[ "$mode" != "no-cache" ]]; then
        inputs="$(cache_inputs)"
        key="$(cache_key_for_inputs "$inputs")"
        record_path="$DEV_CHECK_CACHE_DIR/$key.success"
        printf 'cache.key=%s\n' "$key" >>"$RUN_METADATA"
    fi

    if [[ "$mode" == "explain" ]]; then
        printf '%s\n' "Dev check cache inputs:" "$inputs"
        printf 'cache.key=%s\n' "$key"
        printf 'cache.record=%s\n' "$record_path"
        finish_success explain
        return
    fi

    if [[ "$cache_read" == true ]] && cache_record_is_valid "$record_path" "$key"; then
        refreshed_inputs="$(cache_inputs)"
        refreshed_key="$(cache_key_for_inputs "$refreshed_inputs")"
        if [[ "$refreshed_key" == "$key" ]]; then
            printf 'CACHE HIT %s\n' "$key"
            print_cached_summary "$record_path"
            finish_success hit
            return
        fi
        inputs="$refreshed_inputs"
        key="$refreshed_key"
        record_path="$DEV_CHECK_CACHE_DIR/$key.success"
    fi

    printf 'CACHE %s %s\n' "$(printf '%s' "$cache_state" | tr '[:lower:]' '[:upper:]')" \
        "${key:-not-written}"

    run_stage "cargo fmt --all --check" 1 cargo fmt --all --check
    run_stage "cargo check --all-targets" 1 cargo check --all-targets
    run_stage "cargo clippy --all-targets -- -D warnings" 1 \
        cargo clippy --all-targets -- -D warnings
    run_stage "cargo nextest run" "$repetitions" cargo nextest run
    run_stage "wasm browser tests" "$repetitions" tools/run-wasm-tests.sh
    run_stage "web loader tests" "$repetitions" \
        bash -c 'cd web/loader && exec node --test test/*.test.js'
    if [[ "${DEV_CHECK_CONTRACT_CHILD:-0}" != 1 ]]; then
        run_stage "dev check contract tests" 1 env DEV_CHECK_CONTRACT_CHILD=1 \
            python3 tools/test_dev_check.py
    fi

    if [[ "$cache_write" == true ]]; then
        refreshed_inputs="$(cache_inputs)"
        refreshed_key="$(cache_key_for_inputs "$refreshed_inputs")"
        if [[ "$refreshed_key" != "$key" ]]; then
            die "cache inputs changed while checks were running; success was not cached"
        fi
        write_cache_record "$record_path" "$key"
    fi
    finish_success "$cache_state"
}

usage() {
    printf 'Usage: %s check [--force | --no-cache | --explain-cache]\n' "$0"
    printf '       %s rust-toolchain\n' "$0"
    printf '       %s release <command> [args]\n' "$0"
}

case "${1:-}" in
    check)
        shift
        check "$@"
        ;;
    rust-toolchain)
        shift
        [[ $# -eq 0 ]] || die "rust-toolchain does not accept arguments"
        channel="$(sed -n 's/^channel = "\([^"]*\)"$/\1/p' rust-toolchain.toml)"
        [[ "$channel" == "stable" ]] || die "rust-toolchain.toml must declare stable"
        env -u RUSTUP_TOOLCHAIN rustup update "$channel"
        env -u RUSTUP_TOOLCHAIN rustup toolchain install
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
