# One compiler cache per host

Rust build output stays in each workbench's own `target/`. Compiler cache
storage and its daemon are host resources, not repository resources.
`dev.sh check` no longer invents a cache directory, port, namespace, or cap,
and no longer reads `tyde.sccacheSizeGiB` or `tyde.sccacheNamespace`.

## Host configuration

Install `tools/host-sccache` as `$CARGO_HOME/bin/host-sccache` (normally
`~/.cargo/bin/host-sccache`). Set the absolute installed path as
`[build].rustc-wrapper` in `$CARGO_HOME/config.toml`, preserving unrelated
settings. Put the canonical settings in `$CARGO_HOME/sccache-env`:

```sh
export RUSTC_WRAPPER="/absolute/cargo/bin/host-sccache"
export SCCACHE_EXECUTABLE="/absolute/cargo/bin/sccache"
export SCCACHE_CONF="/absolute/config/sccache/config.toml"
export SCCACHE_DIR="/absolute/shared/compiler-cache"
export SCCACHE_CACHE_SIZE="1000G"
export SCCACHE_SERVER_PORT="36897"
export SCCACHE_IDLE_TIMEOUT="600"
```

Use the host's actual paths and endpoint; these example values are not
repository defaults. `SCCACHE_CONF` pins the native sccache configuration
independently of a dev instance's isolated HOME. Explicit directory and size
settings take precedence over its disk settings. The installed launcher
clears inherited `SCCACHE_*` settings before loading this file, so an old
shell or caller cannot silently select a different cache. It never retries
with a different cache or without the wrapper.

Ordinary Cargo, Trunk, and dev-instance builds use Cargo's global wrapper.
The launcher finds its configuration relative to its installed location,
including when HOME is isolated. Checks source the same file, then use that
same wrapper for compilation, version checks, and statistics. A missing or
invalid explicitly selected environment fails. Without this file, checks
require an explicit `RUSTC_WRAPPER` shared by all builds (as CI configures)
and preserve the native `SCCACHE_*` environment. They never replace Cargo's
configured launcher with bare sccache when host setup is incomplete. `SCCACHE_BUILD_ENV`
can explicitly select a different environment for isolated tooling fixtures.
Isolated cache tests use their own daemon and storage, never the real cache.

Checks still require the pinned sccache version and a positive local-disk
capacity, validate explicit directory/cap settings against the running
daemon, and retain shared-daemon metrics. They never restart a daemon.
Inspect the canonical writer with `~/.cargo/bin/host-sccache --show-stats`.
A bare underlying `sccache` command without the host environment is not the
configured Cargo wrapper and may still address a legacy daemon.

## Migrating a populated cache without interrupting builds

Select the already-running populated daemon's **exact directory, endpoint,
and capacity** in the host environment. Do not move or copy its live files,
change its capacity, or launch another endpoint pointing at that directory.
New builds immediately join its existing writer. Already-running compiler
clients may finish on their original endpoint. Retain old cache contents
and leave old daemons alone while any existing clients may still use them.
Changing capacity later requires all host builds to finish first.

On the development host, migration reused the populated 1000 GiB writer on
port 36897 in place. Its historical repository-hashed directory is now an
explicit host setting, not an identity computed or hardcoded by Tyde. The
10 GiB legacy daemon and its contents were retained without interruption.
No cache contents, per-workbench targets, concurrency limits, or test scopes
were changed. Old workbenches that still contain the former check script
continue using that same populated writer until their source is updated.

The retired local Git size/namespace settings are intentionally retained on
the development host for older, unchanged workbenches. They are ignored by
the new check script; removing them while old scripts still run could select
a different cache. They can be removed after those workbenches are updated.
