#!/usr/bin/env bash

set -euo pipefail

binary="$1"
shift

if [[ "$(uname -s)" != "Darwin" ]]; then
    exec "$binary" "$@"
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_key="$(stat -f '%d-%i' "$repo_root")"
binary_key="$(stat -f '%i-%m-%c-%z' "$binary")"
cache_dir="${TMPDIR:-/tmp}/tyde-nextest/$workspace_key"
binary_name="$(basename "$binary")"
cached_binary="$cache_dir/$binary_name.$binary_key"
lock_dir="$cached_binary.lock"

mkdir -p "$cache_dir"

if [[ ! -x "$cached_binary" ]]; then
    if mkdir "$lock_dir" 2>/dev/null; then
        temporary_binary="$cached_binary.$$"
        cleanup() {
            rm -f "$temporary_binary"
            rmdir "$lock_dir"
        }
        trap cleanup EXIT

        for stale_binary in "$cache_dir/$binary_name".*; do
            if [[ -f "$stale_binary" ]]; then
                rm -f "$stale_binary"
            fi
        done
        cp -c "$binary" "$temporary_binary"
        xattr -c "$temporary_binary"
        mv "$temporary_binary" "$cached_binary"
        rmdir "$lock_dir"
        trap - EXIT
    else
        for _ in {1..200}; do
            if [[ -x "$cached_binary" ]]; then
                break
            fi
            sleep 0.05
        done
        if [[ ! -x "$cached_binary" ]]; then
            printf 'Timed out preparing nextest binary: %s\n' "$binary" >&2
            exit 1
        fi
    fi
fi

exec "$cached_binary" "$@"
