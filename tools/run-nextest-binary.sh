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
            [[ -d "$state_dir" ]] || continue
            owner_pid="$(sed -n 's/^pid=//p' "$state_dir/owner" 2>/dev/null || true)"
            if [[ "$owner_pid" =~ ^[0-9]+$ ]] && ! kill -0 "$owner_pid" 2>/dev/null; then
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
                [[ -d "$lease" ]] || continue
                owner_pid="$(sed -n 's/^pid=//p' "$lease/owner" 2>/dev/null || true)"
                if [[ "$owner_pid" =~ ^[0-9]+$ ]] && kill -0 "$owner_pid" 2>/dev/null; then
                    in_use=true
                elif [[ -z "$owner_pid" ]]; then
                    in_use=true
                fi
            done
            [[ "$in_use" == true ]] || rm -f "$cached"
        done < <(
            for cached in "$cache_dir"/*; do
                [[ -f "$cached" ]] || continue
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
            printf 'pid=%s\n' "$$" >"$lock_dir/owner"
            LOCK_HELD=true
            return
        fi
        owner_pid=""
        if [[ -r "$lock_dir/owner" ]]; then
            owner_pid="$(sed -n 's/^pid=//p' "$lock_dir/owner" 2>/dev/null || true)"
        fi
        if [[ "$owner_pid" =~ ^[0-9]+$ ]] && ! kill -0 "$owner_pid" 2>/dev/null; then
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
        owner_pid="$(sed -n 's/^pid=//p' "$lock_dir/owner" 2>/dev/null || true)"
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
        [[ -d "$lease" ]] || continue
        owner_pid="$(sed -n 's/^pid=//p' "$lease/owner" 2>/dev/null || true)"
        if [[ "$owner_pid" =~ ^[0-9]+$ ]] && kill -0 "$owner_pid" 2>/dev/null; then
            in_use=true
        elif [[ -n "$owner_pid" ]]; then
            rm -rf "$lease"
        else
            in_use=true
        fi
    done
    [[ "$in_use" == true ]]
}

acquire_lock
temporary_binary="$cached_binary.$$"
lease_dir="$cached_binary.use.$$"
cleanup() {
    rm -f "$temporary_binary"
    rm -rf "$lease_dir"
    release_lock
}
trap cleanup EXIT

if [[ ! -x "$cached_binary" ]]; then
    for stale_binary in "$cache_dir/$logical_name".*; do
        if [[ -f "$stale_binary" && "$stale_binary" != "$cached_binary" ]]; then
            if ! stale_binary_is_in_use "$stale_binary"; then
                rm -f "$stale_binary"
            fi
        fi
    done
    cp -c "$binary" "$temporary_binary"
    xattr -c "$temporary_binary"
    mv "$temporary_binary" "$cached_binary"
fi

mkdir "$lease_dir"
printf 'pid=%s\n' "$$" >"$lease_dir/owner"
release_lock

set +e
"$cached_binary" "$@"
status=$?
set -e
rm -rf "$lease_dir"
trap - EXIT
exit "$status"
