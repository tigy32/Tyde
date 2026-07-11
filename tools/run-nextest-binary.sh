#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$(uname -s)" != "Darwin" ]]; then
    if [[ "${1:-}" == "--cleanup-stale" ]]; then
        printf '0\n'
        exit 0
    fi
    exec "$@"
fi

workspace_key="$(stat -f '%d-%i' "$repo_root")"
cache_dir="${TMPDIR:-/tmp}/tyde-nextest/$workspace_key"
ownerless_grace_seconds=5

state_mtime() {
    stat -f '%m' "$1"
}

state_is_past_grace() {
    local state="$1"
    local now modified
    now="$(date +%s)"
    modified="$(state_mtime "$state" 2>/dev/null || true)"
    [[ "$modified" =~ ^[0-9]+$ ]] || return 1
    ((now - modified >= ownerless_grace_seconds))
}

state_owner_pid() {
    local state="$1"
    if [[ -L "$state" ]]; then
        readlink "$state" 2>/dev/null || true
    elif [[ -d "$state" ]]; then
        sed -n 's/^pid=//p' "$state/owner" 2>/dev/null || true
    elif [[ -f "$state" ]]; then
        sed -n 's/^pid=//p' "$state" 2>/dev/null || true
    fi
    return 0
}

if [[ "${1:-}" == "--cleanup-stale" ]]; then
    [[ "${TYDE_DEV_CHECK_LOCK_HELD:-0}" == 1 ]] || {
        printf '%s\n' \
            'Refusing nextest cleanup without the repository dev-check lock' >&2
        exit 1
    }
    before_kib=0
    after_kib=0
    [[ -d "$cache_dir" ]] &&
        before_kib="$(du -sk "$cache_dir" | awk 'NR == 1 { print $1 }')"
    if [[ -d "$cache_dir" ]]; then
        for state_dir in "$cache_dir"/*.lock "$cache_dir"/*.use.*; do
            [[ -e "$state_dir" || -L "$state_dir" ]] || continue
            owner_pid="$(state_owner_pid "$state_dir")"
            if [[ "$owner_pid" =~ ^[0-9]+$ ]] && ! kill -0 "$owner_pid" 2>/dev/null; then
                rm -rf "$state_dir"
            elif [[ -z "$owner_pid" ]] && state_is_past_grace "$state_dir"; then
                rm -rf "$state_dir"
            fi
        done
        count=0
        while IFS=$'\t' read -r _ cached; do
            [[ -f "$cached" ]] || continue
            count=$((count + 1))
            if ((count <= 64)); then
                continue
            fi
            in_use=false
            for lease in "$cached".use.*; do
                [[ -e "$lease" || -L "$lease" ]] || continue
                owner_pid="$(state_owner_pid "$lease")"
                if [[ "$owner_pid" =~ ^[0-9]+$ ]] && kill -0 "$owner_pid" 2>/dev/null; then
                    in_use=true
                elif [[ -z "$owner_pid" ]] && ! state_is_past_grace "$lease"; then
                    in_use=true
                fi
            done
            [[ "$in_use" == true ]] || rm -f "$cached"
        done < <(
            for cached in "$cache_dir"/*; do
                [[ -f "$cached" && -x "$cached" ]] || continue
                printf '%s\t%s\n' "$(stat -f '%m' "$cached")" "$cached"
            done | LC_ALL=C sort -rn
        )
        after_kib="$(du -sk "$cache_dir" | awk 'NR == 1 { print $1 }')"
    fi
    printf '%s\n' "$(((before_kib - after_kib) * 1024))"
    exit 0
fi

[[ $# -ge 1 ]] || {
    printf 'Usage: %s <test-binary> [args...]\n' "$0" >&2
    exit 2
}

binary="$1"
shift
binary_name="$(basename "$binary")"
if [[ "$binary_name" =~ ^(.+)-[0-9a-f]{16}$ ]]; then
    logical_name="${BASH_REMATCH[1]}"
else
    logical_name="$binary_name"
fi
binary_key="$(stat -f '%i-%m-%c-%z' "$binary")"
cached_binary="$cache_dir/$logical_name.$binary_key"
lock_dir="$cache_dir/$logical_name.lock"

mkdir -p "$cache_dir"
LOCK_HELD=false

acquire_lock() {
    local attempt owner_pid
    for ((attempt = 1; attempt <= 200; attempt++)); do
        if mkdir "$lock_dir" 2>/dev/null; then
            printf 'pid=%s\n' "$$" >"$lock_dir/owner.tmp.$$"
            mv "$lock_dir/owner.tmp.$$" "$lock_dir/owner"
            LOCK_HELD=true
            return
        fi
        owner_pid="$(state_owner_pid "$lock_dir")"
        if [[ "$owner_pid" =~ ^[0-9]+$ ]] && ! kill -0 "$owner_pid" 2>/dev/null; then
            if mv "$lock_dir" "$lock_dir.stale.$$" 2>/dev/null; then
                rm -rf "$lock_dir.stale.$$"
                continue
            fi
        elif [[ -z "$owner_pid" ]] && state_is_past_grace "$lock_dir"; then
            if mv "$lock_dir" "$lock_dir.stale.$$" 2>/dev/null; then
                rm -rf "$lock_dir.stale.$$"
                continue
            fi
        fi
        sleep 0.05
    done
    printf 'Timed out preparing nextest binary %s; lock: %s\n' \
        "$binary" "$lock_dir" >&2
    exit 1
}

release_lock() {
    local owner_pid=""
    if [[ "$LOCK_HELD" == true ]]; then
        owner_pid="$(state_owner_pid "$lock_dir")"
        if [[ "$owner_pid" == "$$" ]]; then
            rm -f "$lock_dir/owner"
            rmdir "$lock_dir" 2>/dev/null || true
        fi
        LOCK_HELD=false
    fi
}

stale_binary_is_in_use() {
    local stale_binary="$1"
    local lease owner_pid
    local in_use=false

    for lease in "$stale_binary".use.*; do
        [[ -e "$lease" || -L "$lease" ]] || continue
        owner_pid="$(state_owner_pid "$lease")"
        if [[ "$owner_pid" =~ ^[0-9]+$ ]] && kill -0 "$owner_pid" 2>/dev/null; then
            in_use=true
        elif [[ -n "$owner_pid" ]]; then
            rm -rf "$lease"
        elif ! state_is_past_grace "$lease"; then
            in_use=true
        else
            rm -rf "$lease"
        fi
    done
    [[ "$in_use" == true ]]
}

acquire_lock
temporary_binary="$cached_binary.$$"
lease_dir=""
cleanup() {
    rm -f "$temporary_binary"
    [[ -z "$lease_dir" ]] || rm -f "$lease_dir"
    release_lock
}
trap cleanup EXIT

if [[ ! -x "$cached_binary" ]]; then
    for stale_binary in "$cache_dir/$logical_name".*; do
        if [[ -f "$stale_binary" && -x "$stale_binary" &&
            "$stale_binary" != "$cached_binary" ]]; then
            if ! stale_binary_is_in_use "$stale_binary"; then
                rm -f "$stale_binary"
            fi
        fi
    done
    cp -c "$binary" "$temporary_binary"
    xattr -c "$temporary_binary"
    mv "$temporary_binary" "$cached_binary"
fi

lease_dir="$(mktemp "$cached_binary.use.$$.XXXXXX")"
printf 'pid=%s\n' "$$" >"$lease_dir"
release_lock

set +e
"$cached_binary" "$@"
status=$?
set -e
rm -f "$lease_dir"
trap - EXIT
exit "$status"
