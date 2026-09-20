# Managed Remote Connect and Upgrade

Managed remote hosts use an exact-release lifecycle. The desktop app chooses the
expected Tyde release from `TYDE_RELEASE_TAG` via
`current_app_release_version()`, and the remote server is compatible only when
its probed running release is exactly that value.

This document extends `12-remote-hosts.md` for the managed lifecycle path.

---

## Source of truth

The compatibility gate is the remote lifecycle snapshot read over SSH:

- `~/.tyde/run/tyde-host-version` reports the running managed server release.
- `~/.tyde/bin/<target>/tyde-server` determines whether the exact target binary
  is installed and executable.
- `~/.tyde/tyde.sock` plus the managed pid file distinguishes managed, stopped,
  and unknown-socket states.

The pid file is owned by the server, not by the launch script. A managed
launch runs `tyde-server host --uds --managed`, and that process writes
`tyde-host-version` and then `tyde-host.pid` only after it has bound
`~/.tyde/tyde.sock`. A server that loses the bind never records itself, so the
pid file always names the socket owner. The launch script succeeds only when
the pid file names the process it started; the mere existence of a socket
proves nothing, because another server may own it.

Launches are serialized by `~/.tyde/run/tyde-host-launch.pid`, created with a
shell noclobber redirection (`O_EXCL`). Do not use `mkdir` as the lock: uutils
coreutils reports success when it loses the create race. With the lock held, a launch that finds a live socket settles on the recorded server instead
of starting a second one against a bound socket.

Releases before the server-owned pid file could record a server that then
failed to bind, leaving the real owner running but untracked. When the socket
is live and the pid file names no live process, the probe and the launch adopt
the owner if exactly one process launched from `~/.tyde/bin/<release>/` is
running `host --uds`, rewriting the pid and version files. A live socket with no
such process is an unknown socket. A socket file nothing listens on is a stopped
host: SIGTERM does not remove it, and the next server replaces it.

The scripts live in `host_config::managed_host` and are exercised against the
real binary by `tyde-server/tests/managed_host_lifecycle.rs`.

GitHub release metadata is not part of compatibility detection. It is only an
install source when a launch or upgrade genuinely needs the target binary.

---

## Connect state machine

A normal managed connect performs these steps:

1. `ProbePlatform`: SSH to the host and read `uname -s` / `uname -m`.
2. `ProbeInstallation`: SSH to read a `RemoteHostLifecycleSnapshot` for the
   app's exact target release. This probe never calls GitHub.
3. Plan the action from the snapshot with no network access.

The action table is:

| Running state | Target binary installed | Action |
| --- | --- | --- |
| Managed, running release == target | yes | Serve as-is |
| Managed, running release == target | no | Error |
| Managed, running release != target | yes | Stop old server, launch target |
| Managed, running release != target | no | Install target, stop old server, launch target |
| Not running | yes | Launch target |
| Not running | no | Install target, launch target |
| Unknown socket | either | Error |

`Serve as-is` emits `Connect` and returns the snapshot. It does not resolve a
GitHub release, select an asset, download an asset, install a binary, or update
`current`.

A running server with the expected release but without
`~/.tyde/bin/<target>/tyde-server` is an explicit error. The bridge must execute
the exact target binary, so compatible connect must not silently repair this by
contacting GitHub.

---

## GitHub invariant

GitHub may be contacted only inside the target-install step, and that step is
reachable only for launch or upgrade actions when the exact target binary is not
already installed.

Install flow:

1. Emit `ResolveRelease` and derive the exact `v<target>` GitHub release URL.
2. Emit `DownloadAsset`, then instruct the remote host over SSH to download the
   platform asset directly from GitHub with `curl` or `wget`.
3. On the remote host, extract `tyde-server`, verify that its reported version
   exactly matches `<target>`, and atomically install it at
   `~/.tyde/bin/<target>/tyde-server`.
4. Emit `InstallBinary` after the remote install command completes.
5. Re-probe the snapshot and fail if `installed_target` is still false.

The desktop never downloads the release archive or streams the server binary
through SSH. The managed remote requires `curl` or `wget` plus `unzip`.

If GitHub is unavailable during install, the error must name the target version
and propagate visibly. There is no fallback to `current`, an older cached
release, or any different version.

Status probes also never call GitHub. They only SSH to the host and read the
snapshot.

---

## Exact target bridge

For managed remotes, the SSH bridge command executes:

```text
~/.tyde/bin/<target>/tyde-server host --bridge-uds
```

It must not execute `~/.tyde/bin/current/tyde-server`, because compatible
connect can serve an already-running daemon without relaunching or updating the
`current` symlink. If the exact target bridge binary is missing or not
executable, the bridge fails with an explicit error.

---

## Force upgrade entry point

Phase 2 handshake handling may receive a server-side
`Reject { IncompatibleProtocol }` after a normal compatible-looking probe. That
path should call the forced managed lifecycle entry point once.

Forced upgrade still probes platform and installation first, then bypasses the
`Serve as-is` action:

- install the target binary if it is missing,
- stop a managed running server even when it reports the target release,
- launch and verify the target once,
- error on an unknown socket.

The force path does not retry indefinitely and does not change the exact-release
compatibility rule.
