#!/usr/bin/env bash

set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PYTHON_HELPER="$SCRIPT_DIR/release_tool.py"
VERSION_SETTER="$SCRIPT_DIR/set_release_version.py"
VERSION_CHECKER="$SCRIPT_DIR/check_release_version.py"
RELEASE_CHECK="$SCRIPT_DIR/release_check.sh"

EXIT_FAILURE=1
EXIT_USAGE=2
EXIT_NOT_FOUND=3
EXIT_RUNNING=4
EXIT_TOOL=5
EXIT_TIMEOUT=124
TEMP_DIR=""
TEMP_PATH=""
WAIT_TIMEOUT_RAW=""
WAIT_TIMEOUT_SECONDS=""
WAIT_INTERVAL_RAW=""
WAIT_INTERVAL_SECONDS=""

cleanup() {
    if [[ -n "$TEMP_DIR" ]]; then
        rm -rf "$TEMP_DIR"
    fi
}
trap cleanup EXIT

set_temp_path() {
    local name="$1"
    if [[ -z "$TEMP_DIR" ]]; then
        TEMP_DIR="$(mktemp -d)"
    fi
    TEMP_PATH="$TEMP_DIR/$name"
}

usage() {
    cat <<'USAGE'
Usage:
  ./dev.sh release prepare vX.Y.Z [--commit]
  ./dev.sh release cut vX.Y.Z [--no-wait]
  ./dev.sh release status vX.Y.Z
  ./dev.sh release wait vX.Y.Z [--timeout 90m] [--interval 30]
  ./dev.sh release verify vX.Y.Z
  ./dev.sh release publish vX.Y.Z

wait/status exit codes: 0 success, 1 failed, 3 not found, 4 still running,
5 network/tool error. Usage errors exit 2.
USAGE
}

die() {
    local message="$1"
    local code="${2:-$EXIT_FAILURE}"
    printf 'ERROR: %s\n' "$message" >&2
    exit "$code"
}

require_command() {
    local command_name="$1"
    if ! command -v "$command_name" >/dev/null 2>&1; then
        die "$command_name is required" "$EXIT_TOOL"
    fi
}

strict_tag_syntax() {
    local tag="$1"
    local body core prerelease identifier
    local -a core_parts prerelease_parts

    [[ "$tag" == v* ]] || return 1
    body="${tag#v}"
    [[ "$body" != *+* ]] || return 1
    core="${body%%-*}"
    [[ "$core" != .* && "$core" != *. && "$core" != *..* ]] || return 1
    IFS='.' read -r -a core_parts <<< "$core"
    [[ ${#core_parts[@]} -eq 3 ]] || return 1
    for identifier in "${core_parts[@]}"; do
        [[ "$identifier" =~ ^(0|[1-9][0-9]*)$ ]] || return 1
    done
    if [[ "$body" == *-* ]]; then
        prerelease="${body#*-}"
        [[ -n "$prerelease" ]] || return 1
        [[ "$prerelease" != .* && "$prerelease" != *. && "$prerelease" != *..* ]] || return 1
        IFS='.' read -r -a prerelease_parts <<< "$prerelease"
        for identifier in "${prerelease_parts[@]}"; do
            [[ "$identifier" =~ ^[0-9A-Za-z-]+$ ]] || return 1
            if [[ "$identifier" =~ ^[0-9]+$ ]]; then
                [[ "$identifier" =~ ^(0|[1-9][0-9]*)$ ]] || return 1
            fi
        done
    fi
}

require_strict_tag() {
    local tag="$1"
    strict_tag_syntax "$tag" || die \
        "invalid release tag '$tag'; expected strict vMAJOR.MINOR.PATCH[-PRERELEASE] semver" \
        "$EXIT_USAGE"
}

parse_duration_value() {
    local raw="$1"
    local value suffix multiplier
    [[ "$raw" =~ ^([1-9][0-9]*)([smh]?)$ ]] || return 1
    value="${BASH_REMATCH[1]}"
    suffix="${BASH_REMATCH[2]}"
    case "$suffix" in
        ""|s) multiplier=1 ;;
        m) multiplier=60 ;;
        h) multiplier=3600 ;;
        *) return 1 ;;
    esac
    DURATION_SECONDS=$((value * multiplier))
    [[ $DURATION_SECONDS -gt 0 ]]
}

parse_wait_options() {
    local timeout_raw="90m"
    local interval_raw="30"
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --timeout)
                [[ $# -ge 2 ]] || { usage >&2; exit "$EXIT_USAGE"; }
                timeout_raw="$2"
                shift 2
                ;;
            --interval)
                [[ $# -ge 2 ]] || { usage >&2; exit "$EXIT_USAGE"; }
                interval_raw="$2"
                shift 2
                ;;
            *)
                usage >&2
                exit "$EXIT_USAGE"
                ;;
        esac
    done
    parse_duration_value "$timeout_raw" || die \
        "invalid duration '$timeout_raw'; use seconds or an s/m/h suffix" \
        "$EXIT_USAGE"
    WAIT_TIMEOUT_RAW="$timeout_raw"
    WAIT_TIMEOUT_SECONDS="$DURATION_SECONDS"
    parse_duration_value "$interval_raw" || die \
        "invalid duration '$interval_raw'; use seconds or an s/m/h suffix" \
        "$EXIT_USAGE"
    WAIT_INTERVAL_RAW="$interval_raw"
    WAIT_INTERVAL_SECONDS="$DURATION_SECONDS"
}

mark_wait_state() {
    local state="$1"
    if [[ -n "${TYDE_WAIT_STATE_FILE:-}" ]]; then
        printf '%s\n' "$state" > "$TYDE_WAIT_STATE_FILE"
    fi
}

require_tools_and_auth() {
    local command_name
    for command_name in git gh python3; do
        require_command "$command_name"
    done
    if ! gh auth status >/dev/null 2>&1; then
        die "GitHub CLI authentication failed; run 'gh auth login'" "$EXIT_TOOL"
    fi
}

require_repo_root() {
    local actual_root
    if ! actual_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
        die "current directory is not a Git repository"
    fi
    if [[ "$actual_root" != "$REPO_ROOT" ]]; then
        die "run this command from the Tyde repository root: $REPO_ROOT"
    fi
    cd "$REPO_ROOT"
}

require_clean_tree() {
    local status
    status="$(git status --porcelain=v1)" || die "could not inspect the working tree" "$EXIT_TOOL"
    [[ -z "$status" ]] || die "working tree is not clean"
}

require_main_branch() {
    local branch
    branch="$(git branch --show-current)" || die "could not determine the current branch" "$EXIT_TOOL"
    [[ "$branch" == "main" ]] || die "release cuts require branch exactly 'main' (found '${branch:-detached}')"
    git symbolic-ref --quiet HEAD >/dev/null 2>&1 || die "release cuts refuse a detached HEAD"
}

require_release_hook() {
    local hooks_path
    hooks_path="$(git config --get core.hooksPath 2>/dev/null || true)"
    [[ "$hooks_path" == ".githooks" ]] || die \
        "core.hooksPath must equal .githooks; run tools/install-git-hooks.sh"
    [[ -x "$REPO_ROOT/.githooks/pre-push" ]] || die \
        ".githooks/pre-push must exist and be executable"
}

fetch_origin_main() {
    git fetch --no-tags origin main || die "failed to fetch origin/main" "$EXIT_TOOL"
}

require_origin_not_ahead() {
    local status
    if git merge-base --is-ancestor origin/main HEAD >/dev/null 2>&1; then
        return
    else
        status=$?
    fi
    [[ $status -eq 1 ]] || die "could not compare HEAD with origin/main" "$EXIT_TOOL"
    if git merge-base --is-ancestor HEAD origin/main >/dev/null 2>&1; then
        die "local main is behind origin/main"
    else
        status=$?
    fi
    [[ $status -eq 1 ]] || die "could not compare HEAD with origin/main" "$EXIT_TOOL"
    die "local main has diverged from origin/main"
}

check_release_version() {
    local tag="$1"
    python3 "$VERSION_CHECKER" "$tag" >/dev/null || die \
        "tracked release versions do not match $tag"
}

remote_tag_lines() {
    local tag="$1"
    git ls-remote --tags origin "refs/tags/$tag" "refs/tags/$tag^{}"
}

remote_tag_commit() {
    local tag="$1"
    local output status sha ref direct="" peeled=""
    if output="$(remote_tag_lines "$tag")"; then
        :
    else
        status=$?
        return "$EXIT_TOOL"
    fi
    while read -r sha ref; do
        [[ -n "${sha:-}" ]] || continue
        if [[ "$ref" == "refs/tags/$tag^{}" ]]; then
            peeled="$sha"
        elif [[ "$ref" == "refs/tags/$tag" ]]; then
            direct="$sha"
        fi
    done <<< "$output"
    if [[ -n "$peeled" ]]; then
        printf '%s\n' "$peeled"
        return 0
    fi
    if [[ -n "$direct" ]]; then
        printf '%s\n' "$direct"
        return 0
    fi
    return "$EXIT_NOT_FOUND"
}

require_tag_absent() {
    local tag="$1"
    local status output
    if git show-ref --verify --quiet "refs/tags/$tag"; then
        die "tag $tag already exists locally"
    else
        status=$?
        [[ $status -eq 1 ]] || die "could not inspect local tags" "$EXIT_TOOL"
    fi
    if output="$(remote_tag_lines "$tag")"; then
        [[ -z "$output" ]] || die "tag $tag already exists on origin"
    else
        die "could not inspect tags on origin" "$EXIT_TOOL"
    fi
}

require_head_unchanged() {
    local expected="$1"
    local actual
    actual="$(git rev-parse HEAD)" || die "could not read HEAD" "$EXIT_TOOL"
    [[ "$actual" == "$expected" ]] || die \
        "HEAD changed during the release cut (expected $expected, found $actual)"
}

cheap_cut_gates() {
    local tag="$1"
    local release_sha="$2"
    require_clean_tree
    require_main_branch
    require_head_unchanged "$release_sha"
    check_release_version "$tag"
    fetch_origin_main
    require_origin_not_ahead
    require_tag_absent "$tag"
}

confirm_exact_tag() {
    local action="$1"
    local tag="$2"
    local response
    [[ -t 0 && -t 1 ]] || die "$action requires an interactive TTY; no bypass is available"
    printf '%s %s by typing the exact tag: ' "$action" "$tag"
    IFS= read -r response || die "$action confirmation was not provided"
    [[ "$response" == "$tag" ]] || die "$action confirmation did not match $tag"
}

command_prepare() {
    local tag="${1:-}"
    local commit=false
    local changed_file commit_subject
    [[ -n "$tag" ]] || { usage >&2; exit "$EXIT_USAGE"; }
    shift
    if [[ $# -gt 0 ]]; then
        [[ $# -eq 1 && "$1" == "--commit" ]] || { usage >&2; exit "$EXIT_USAGE"; }
        commit=true
    fi

    require_strict_tag "$tag"
    require_command python3
    require_command git
    require_repo_root
    python3 "$PYTHON_HELPER" validate-tag "$tag" >/dev/null || exit "$EXIT_USAGE"
    if [[ "$commit" == true ]]; then
        require_clean_tree
        commit_subject="Bump release to ${tag#v}"
        [[ ${#commit_subject} -le 50 ]] || die \
            "release tag is too long for the required 50-character commit subject"
    fi

    set_temp_path changed-version-files
    changed_file="$TEMP_PATH"
    python3 "$VERSION_SETTER" "$tag" > "$changed_file" || die \
        "failed to update release versions"
    python3 "$VERSION_CHECKER" "$tag" >/dev/null || die \
        "release version check failed immediately after prepare"
    printf 'Prepared %s\n' "$tag"

    if [[ "$commit" == true ]]; then
        [[ -s "$changed_file" ]] || die "release version files already match $tag; nothing to commit"
        "$REPO_ROOT/dev.sh" check
        while IFS= read -r path; do
            [[ -n "$path" ]] && git add -- "$path"
        done < "$changed_file"
        git diff --cached --quiet && die "no release version changes were staged"
        git commit -m "$commit_subject" \
            -m "Set all tracked release versions to $tag."
        printf 'Created local release bump commit; nothing was pushed.\n'
    fi
}

command_cut() {
    local tag="${1:-}"
    local no_wait=false
    local release_sha subject remote_sha
    [[ -n "$tag" ]] || { usage >&2; exit "$EXIT_USAGE"; }
    shift
    if [[ $# -gt 0 ]]; then
        [[ $# -eq 1 && "$1" == "--no-wait" ]] || { usage >&2; exit "$EXIT_USAGE"; }
        no_wait=true
    fi

    require_strict_tag "$tag"
    require_tools_and_auth
    python3 "$PYTHON_HELPER" validate-tag "$tag" >/dev/null || exit "$EXIT_USAGE"
    require_repo_root
    require_clean_tree
    require_main_branch
    require_release_hook
    fetch_origin_main
    require_origin_not_ahead
    check_release_version "$tag"
    require_tag_absent "$tag"
    release_sha="$(git rev-parse HEAD)" || die "could not record HEAD" "$EXIT_TOOL"
    subject="$(git log -1 --format=%s)" || die "could not record HEAD subject" "$EXIT_TOOL"
    printf 'Release candidate: %s %s %s\n' "$tag" "$release_sha" "$subject"

    "$RELEASE_CHECK" "$tag" || die "canonical release check failed"
    cheap_cut_gates "$tag" "$release_sha"
    confirm_exact_tag "Cut release" "$tag"
    cheap_cut_gates "$tag" "$release_sha"

    git tag -a "$tag" -m "Release $tag" || die "failed to create local annotated tag $tag"
    if ! git push origin main; then
        die "PARTIAL RELEASE: local tag $tag exists; origin/main was not confirmed and the tag was not pushed. Do not delete remote state automatically."
    fi
    if ! git fetch --no-tags origin main; then
        die "PARTIAL RELEASE: main was pushed, but remote main verification failed; tag $tag was not pushed. Do not roll back remote state."
    fi
    if ! git merge-base --is-ancestor "$release_sha" origin/main >/dev/null 2>&1; then
        die "PARTIAL RELEASE: main was pushed, but origin/main does not contain $release_sha; tag $tag was not pushed. Do not roll back remote state."
    fi
    if ! git push origin "$tag"; then
        die "PARTIAL RELEASE: origin/main contains $release_sha, but the $tag push was not confirmed. The local tag remains; inspect the remote and do not roll back state."
    fi
    if remote_sha="$(remote_tag_commit "$tag")"; then
        :
    else
        die "PARTIAL RELEASE: main and tag pushes completed, but the remote tag could not be verified. Do not delete or recreate remote state."
    fi
    [[ "$remote_sha" == "$release_sha" ]] || die \
        "PARTIAL RELEASE: remote tag $tag peels to $remote_sha, expected $release_sha. Do not alter remote state."
    printf 'Cut %s at %s and verified both remote refs.\n' "$tag" "$release_sha"

    if [[ "$no_wait" == true ]]; then
        printf 'Release workflow was not awaited. Run: ./dev.sh release wait %s\n' "$tag"
        return
    fi
    command_wait_bounded "$tag"
}

network_command_setup() {
    local tag="$1"
    require_strict_tag "$tag"
    require_tools_and_auth
    python3 "$PYTHON_HELPER" validate-tag "$tag" >/dev/null || exit "$EXIT_USAGE"
    require_repo_root
}

resolve_release_sha() {
    local tag="$1"
    local status sha
    if sha="$(remote_tag_commit "$tag")"; then
        printf '%s\n' "$sha"
        return
    else
        status=$?
    fi
    if [[ $status -eq $EXIT_NOT_FOUND ]]; then
        return "$EXIT_NOT_FOUND"
    fi
    return "$EXIT_TOOL"
}

require_remote_tag_on_origin_main() {
    local tag="$1"
    local sha status
    if sha="$(resolve_release_sha "$tag")"; then
        :
    else
        status=$?
        [[ $status -eq $EXIT_NOT_FOUND ]] && die \
            "remote tag $tag was not found" "$EXIT_NOT_FOUND"
        die "could not resolve remote tag $tag" "$EXIT_TOOL"
    fi
    fetch_origin_main
    if git merge-base --is-ancestor "$sha" origin/main >/dev/null 2>&1; then
        return
    fi
    die "remote tag $tag at $sha is not contained in origin/main; refusing publication"
}

fetch_run_view() {
    local tag="$1"
    local sha="$2"
    local run_list_file select_error_file status
    set_temp_path run-list.json
    run_list_file="$TEMP_PATH"
    set_temp_path run-view.json
    RUN_VIEW_FILE="$TEMP_PATH"
    set_temp_path select-run-error
    select_error_file="$TEMP_PATH"
    if ! gh run list --workflow release.yml --limit 100 \
        --json databaseId,workflowName,headBranch,headSha,status,conclusion,url,createdAt \
        > "$run_list_file"; then
        return "$EXIT_TOOL"
    fi
    if RUN_ID="$(python3 "$PYTHON_HELPER" select-run "$tag" "$sha" \
        --input "$run_list_file" 2> "$select_error_file")"; then
        :
    else
        status=$?
        [[ $status -eq $EXIT_NOT_FOUND ]] && return "$EXIT_NOT_FOUND"
        cat "$select_error_file" >&2
        return "$EXIT_TOOL"
    fi
    if ! gh run view "$RUN_ID" \
        --json databaseId,status,conclusion,url,jobs > "$RUN_VIEW_FILE"; then
        return "$EXIT_TOOL"
    fi
}

print_run_failure() {
    local job_id job_name step_name job_url log_file
    job_id="$(python3 "$PYTHON_HELPER" failure-field id --input "$RUN_VIEW_FILE")"
    job_name="$(python3 "$PYTHON_HELPER" failure-field job --input "$RUN_VIEW_FILE")"
    step_name="$(python3 "$PYTHON_HELPER" failure-field step --input "$RUN_VIEW_FILE")"
    job_url="$(python3 "$PYTHON_HELPER" failure-field url --input "$RUN_VIEW_FILE")"
    printf 'Failed job: %s\nFailed step: %s\nURL: %s\n' "$job_name" "$step_name" "$job_url" >&2
    if [[ -n "$job_id" ]]; then
        set_temp_path failed.log
        log_file="$TEMP_PATH"
        if gh run view "$RUN_ID" --job "$job_id" --log-failed > "$log_file" 2>&1; then
            printf '%s\n' '--- sanitized failed log excerpt ---' >&2
            python3 "$PYTHON_HELPER" sanitize-log --max-lines 80 --max-chars 12000 \
                < "$log_file" >&2
            printf '%s\n' '--- end excerpt ---' >&2
        else
            printf 'Could not retrieve failed logs; use the job URL above.\n' >&2
        fi
    fi
}

command_status() {
    local tag="${1:-}"
    local sha status outcome
    [[ -n "$tag" && $# -eq 1 ]] || { usage >&2; exit "$EXIT_USAGE"; }
    network_command_setup "$tag"
    if sha="$(resolve_release_sha "$tag")"; then
        :
    else
        status=$?
        [[ $status -eq $EXIT_NOT_FOUND ]] && die "remote tag $tag was not found" "$EXIT_NOT_FOUND"
        die "could not resolve remote tag $tag" "$EXIT_TOOL"
    fi
    if fetch_run_view "$tag" "$sha"; then
        :
    else
        status=$?
        [[ $status -eq $EXIT_NOT_FOUND ]] && die \
            "Release workflow for $tag at $sha was not found" "$EXIT_NOT_FOUND"
        die "could not read GitHub workflow status" "$EXIT_TOOL"
    fi
    python3 "$PYTHON_HELPER" run-report --tag "$tag" --input "$RUN_VIEW_FILE" || die \
        "could not normalize GitHub workflow status" "$EXIT_TOOL"
    outcome="$(python3 "$PYTHON_HELPER" run-outcome --input "$RUN_VIEW_FILE")" || die \
        "could not normalize GitHub workflow outcome" "$EXIT_TOOL"
    case "$outcome" in
        success) return 0 ;;
        failure) return "$EXIT_FAILURE" ;;
        running) return "$EXIT_RUNNING" ;;
        *) die "unknown workflow outcome $outcome" "$EXIT_TOOL" ;;
    esac
}

read_release_json() {
    local tag="$1"
    local error_file
    set_temp_path release.json
    RELEASE_FILE="$TEMP_PATH"
    set_temp_path release-error
    error_file="$TEMP_PATH"
    if gh release view "$tag" \
        --json tagName,isDraft,isPrerelease,url,assets > "$RELEASE_FILE" 2> "$error_file"; then
        return 0
    fi
    if grep -Eiq 'release not found|HTTP 404|Not Found' "$error_file"; then
        return "$EXIT_NOT_FOUND"
    fi
    cat "$error_file" >&2
    return "$EXIT_TOOL"
}

require_release_not_latest() {
    local tag="$1"
    local latest_file error_file latest_tag
    set_temp_path latest-release-tag
    latest_file="$TEMP_PATH"
    set_temp_path latest-release-error
    error_file="$TEMP_PATH"
    if gh api 'repos/{owner}/{repo}/releases/latest' --jq .tag_name \
        > "$latest_file" 2> "$error_file"; then
        latest_tag="$(cat "$latest_file")"
        [[ "$latest_tag" != "$tag" ]] || die \
            "GitHub still reports beta $tag as the latest release"
        return
    fi
    if grep -Eiq 'HTTP 404|Not Found' "$error_file"; then
        return
    fi
    cat "$error_file" >&2
    die "could not verify GitHub latest-release state" "$EXIT_TOOL"
}

command_verify() {
    local tag="${1:-}"
    local sha status protocol_source manifest_file plan_file release_report
    local count index url integrity wasm headers body content_type curl_status
    [[ -n "$tag" && $# -eq 1 ]] || { usage >&2; exit "$EXIT_USAGE"; }
    network_command_setup "$tag"
    require_command curl
    if sha="$(resolve_release_sha "$tag")"; then
        :
    else
        status=$?
        [[ $status -eq $EXIT_NOT_FOUND ]] && die "remote tag $tag was not found" "$EXIT_NOT_FOUND"
        die "could not resolve remote tag $tag" "$EXIT_TOOL"
    fi
    if read_release_json "$tag"; then
        :
    else
        status=$?
        [[ $status -eq $EXIT_NOT_FOUND ]] && die "GitHub release $tag was not found" "$EXIT_NOT_FOUND"
        die "could not read GitHub release $tag" "$EXIT_TOOL"
    fi
    release_report="$(python3 "$PYTHON_HELPER" validate-release "$tag" --input "$RELEASE_FILE")" || die \
        "GitHub release asset/state validation failed"
    printf '%s\n' "$release_report"

    set_temp_path tagged-types.rs
    protocol_source="$TEMP_PATH"
    if ! gh api "repos/{owner}/{repo}/contents/protocol/src/types.rs?ref=$tag" \
        -H 'Accept: application/vnd.github.raw+json' > "$protocol_source"; then
        die "could not download protocol/src/types.rs from $tag" "$EXIT_TOOL"
    fi
    set_temp_path live-manifest.json
    manifest_file="$TEMP_PATH"
    if curl --fail --silent --show-error --location \
        --connect-timeout 20 --max-time 120 \
        --output "$manifest_file" https://tycode.dev/tyde/manifest.json; then
        :
    else
        curl_status=$?
        if [[ $curl_status -eq 22 ]]; then
            die "live tycode.dev manifest is not reachable"
        fi
        die "network error while downloading the live tycode.dev manifest" "$EXIT_TOOL"
    fi
    python3 "$SCRIPT_DIR/check_mobile_web_manifest.py" \
        --manifest "$manifest_file" --protocol-source "$protocol_source" "$tag" || die \
        "live mobile web manifest does not match $tag"

    set_temp_path manifest-plan.json
    plan_file="$TEMP_PATH"
    python3 "$PYTHON_HELPER" manifest-plan "$tag" \
        --manifest "$manifest_file" --base-url https://tycode.dev --output "$plan_file" || die \
        "could not build the mobile web verification plan"
    count="$(python3 "$PYTHON_HELPER" json-field count --input "$plan_file")"
    index=0
    while [[ $index -lt $count ]]; do
        url="$(python3 "$PYTHON_HELPER" json-field "items.$index.url" --input "$plan_file")"
        integrity="$(python3 "$PYTHON_HELPER" json-field "items.$index.integrity" --input "$plan_file")"
        wasm="$(python3 "$PYTHON_HELPER" json-field "items.$index.wasm" --input "$plan_file")"
        set_temp_path "headers-$index"
        headers="$TEMP_PATH"
        set_temp_path "body-$index"
        body="$TEMP_PATH"
        if curl --fail --silent --show-error --location \
            --connect-timeout 20 --max-time 120 \
            --dump-header "$headers" --output "$body" "$url"; then
            :
        else
            curl_status=$?
            if [[ $curl_status -eq 22 ]]; then
                die "mobile web asset is not reachable: $url"
            fi
            die "network error while downloading $url" "$EXIT_TOOL"
        fi
        content_type="$(python3 "$PYTHON_HELPER" header-content-type --input "$headers")" || die \
            "invalid HTTP headers for $url"
        if [[ "$wasm" == "true" ]]; then
            python3 "$PYTHON_HELPER" validate-download --path "$body" \
                --integrity "$integrity" --content-type "$content_type" --wasm || die \
                "mobile web WASM validation failed: $url"
        else
            python3 "$PYTHON_HELPER" validate-download --path "$body" \
                --integrity "$integrity" --content-type "$content_type" || die \
                "mobile web entry validation failed: $url"
        fi
        printf 'verified %s\n' "$url"
        index=$((index + 1))
    done
    printf 'Verified %s at %s (release SHA %s).\n' "$tag" "https://tycode.dev/tyde/" "$sha"
}

command_wait() {
    local tag="${1:-}"
    local timeout_raw interval_raw timeout interval
    local sha status start deadline now remaining sleep_for
    local signature="" previous_signature="" outcome seen_run=false
    [[ -n "$tag" ]] || { usage >&2; exit "$EXIT_USAGE"; }
    shift
    parse_wait_options "$@"
    timeout_raw="$WAIT_TIMEOUT_RAW"
    interval_raw="$WAIT_INTERVAL_RAW"
    timeout="$WAIT_TIMEOUT_SECONDS"
    interval="$WAIT_INTERVAL_SECONDS"
    network_command_setup "$tag"
    require_command date
    require_command sleep
    if sha="$(resolve_release_sha "$tag")"; then
        :
    else
        status=$?
        [[ $status -eq $EXIT_NOT_FOUND ]] && die "remote tag $tag was not found" "$EXIT_NOT_FOUND"
        die "could not resolve remote tag $tag" "$EXIT_TOOL"
    fi
    start="$(date +%s)"
    deadline=$((start + timeout))

    while true; do
        if fetch_run_view "$tag" "$sha"; then
            seen_run=true
            mark_wait_state matched
            signature="$(python3 "$PYTHON_HELPER" run-signature --input "$RUN_VIEW_FILE")" || die \
                "could not normalize GitHub workflow state" "$EXIT_TOOL"
            if [[ "$signature" != "$previous_signature" ]]; then
                python3 "$PYTHON_HELPER" run-report --tag "$tag" --input "$RUN_VIEW_FILE"
                previous_signature="$signature"
            fi
            outcome="$(python3 "$PYTHON_HELPER" run-outcome --input "$RUN_VIEW_FILE")" || die \
                "could not normalize GitHub workflow outcome" "$EXIT_TOOL"
            case "$outcome" in
                success)
                    mark_wait_state verifying
                    command_verify "$tag"
                    if [[ "$tag" == *-* ]]; then
                        if ! read_release_json "$tag"; then
                            die "verified $tag but could not re-read its draft state" "$EXIT_TOOL"
                        fi
                        if [[ "$(python3 "$PYTHON_HELPER" json-field isDraft --input "$RELEASE_FILE")" == "true" ]]; then
                            printf '\nBETA RELEASE IS VERIFIED BUT REMAINS A DRAFT.\n'
                            printf 'Publish it with: ./dev.sh release publish %s\n' "$tag"
                        fi
                    fi
                    return 0
                    ;;
                failure)
                    print_run_failure
                    return "$EXIT_FAILURE"
                    ;;
                running)
                    mark_wait_state running
                    ;;
                *) die "unknown workflow outcome $outcome" "$EXIT_TOOL" ;;
            esac
        else
            status=$?
            if [[ $status -eq $EXIT_NOT_FOUND ]]; then
                if [[ "$previous_signature" != "not-found" ]]; then
                    printf '%s: Release workflow not found yet\n' "$tag"
                    previous_signature="not-found"
                fi
            else
                die "could not poll GitHub workflow status" "$EXIT_TOOL"
            fi
        fi

        now="$(date +%s)"
        if [[ $now -ge $deadline ]]; then
            if [[ "$seen_run" == true ]]; then
                printf 'Workflow is still running after %s.\n' "$timeout_raw" >&2
                return "$EXIT_RUNNING"
            fi
            printf 'Release workflow was not found within %s.\n' "$timeout_raw" >&2
            return "$EXIT_NOT_FOUND"
        fi
        remaining=$((deadline - now))
        sleep_for="$interval"
        [[ $sleep_for -le $remaining ]] || sleep_for="$remaining"
        sleep "$sleep_for"
    done
}

command_wait_bounded() {
    local tag="${1:-}"
    local state_file status state
    [[ -n "$tag" ]] || { usage >&2; exit "$EXIT_USAGE"; }
    shift
    parse_wait_options "$@"
    set_temp_path wait-state
    state_file="$TEMP_PATH"
    printf 'not-found\n' > "$state_file"

    if TYDE_RELEASE_INTERNAL_WAIT=1 TYDE_WAIT_STATE_FILE="$state_file" \
        python3 -B "$PYTHON_HELPER" run-command \
        --timeout "$WAIT_TIMEOUT_SECONDS" -- \
        "$SCRIPT_DIR/release.sh" __wait "$tag" "$@"; then
        return
    else
        status=$?
    fi
    if [[ $status -ne $EXIT_TIMEOUT ]]; then
        return "$status"
    fi

    state="$(cat "$state_file")"
    if [[ "$state" == "running" ]]; then
        printf 'Workflow is still running after %s.\n' "$WAIT_TIMEOUT_RAW" >&2
        return "$EXIT_RUNNING"
    fi
    if [[ "$state" != "not-found" ]]; then
        printf 'Wait timed out while checking or verifying the release.\n' >&2
        return "$EXIT_TOOL"
    fi
    printf 'Release workflow was not found within %s.\n' "$WAIT_TIMEOUT_RAW" >&2
    return "$EXIT_NOT_FOUND"
}

command_publish() {
    local tag="${1:-}"
    local status draft
    [[ -n "$tag" && $# -eq 1 ]] || { usage >&2; exit "$EXIT_USAGE"; }
    require_strict_tag "$tag"
    [[ "$tag" == *-* ]] || die "$tag is stable; release publish is beta-only" "$EXIT_USAGE"
    command_verify "$tag"
    require_remote_tag_on_origin_main "$tag"
    confirm_exact_tag "Publish beta release" "$tag"

    if read_release_json "$tag"; then
        :
    else
        status=$?
        [[ $status -eq $EXIT_NOT_FOUND ]] && die "GitHub release $tag was not found" "$EXIT_NOT_FOUND"
        die "could not re-read GitHub release $tag" "$EXIT_TOOL"
    fi
    python3 "$PYTHON_HELPER" validate-release "$tag" --input "$RELEASE_FILE" >/dev/null || die \
        "release changed after verification"
    draft="$(python3 "$PYTHON_HELPER" json-field isDraft --input "$RELEASE_FILE")"
    [[ "$draft" == "true" ]] || die "$tag is not a draft; refusing to republish it"
    require_remote_tag_on_origin_main "$tag"
    if ! gh release edit "$tag" --draft=false --prerelease --latest=false; then
        die "GitHub failed to publish $tag" "$EXIT_TOOL"
    fi
    if ! read_release_json "$tag"; then
        die "published $tag but could not re-read it; inspect GitHub before retrying" "$EXIT_TOOL"
    fi
    python3 "$PYTHON_HELPER" validate-release "$tag" --input "$RELEASE_FILE" \
        --require-published || die \
        "GitHub did not preserve draft=false and prerelease=true"
    require_release_not_latest "$tag"
    printf 'Published beta release %s with prerelease=true and latest=false.\n' "$tag"
}

case "${1:-}" in
    prepare)
        shift
        command_prepare "$@"
        ;;
    cut)
        shift
        command_cut "$@"
        ;;
    status)
        shift
        command_status "$@"
        ;;
    wait)
        shift
        command_wait_bounded "$@"
        ;;
    __wait)
        [[ "${TYDE_RELEASE_INTERNAL_WAIT:-}" == "1" ]] || die \
            "internal wait entry point cannot be called directly" "$EXIT_USAGE"
        shift
        command_wait "$@"
        ;;
    verify)
        shift
        command_verify "$@"
        ;;
    publish)
        shift
        command_publish "$@"
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        usage >&2
        exit "$EXIT_USAGE"
        ;;
esac
