# Bubblewrap workbenches

Settings → Workbenches selects the engine for **new** workbenches on that host.
Native is the default everywhere. Existing workbenches do not change. UI and
MCP creation use the same host-owned Git lifecycle and record the environment
on the normal workbench project. Worktrunk is neither used nor required.

Bubblewrap requires Linux, working unprivileged namespaces, Python 3.9+ with
pidfd support, Git, and GNU cp with strict reflinks on the source filesystem.
No language, compiler, package manager or cache service is required. Unsupported
configuration fails explicitly: there is no Native or full-copy fallback.
The settings capability report is probed during host initialization, not on each
connection. Workbench creation validates the current prerequisites again; changing
host tools or namespace policy requires a host restart to refresh that report.

This is a performance environment, not a hostile-code sandbox. Each workbench
mounts its real Git source at `/mnt/tyde-workspace`, with private writable files
and artifacts. Host executables and native config/auth remain runtime references;
HOME and credentials are never copied into snapshots. Native filesystem
permissions are preserved, including config/session paths outside HOME; the
engine-owned cohort store is mounted read-only. Backend security/tool policies
and host namespace policy are unchanged. All enabled backends use the same
host/execution workspace boundary; no backend declares Bubblewrap compatibility.
Native session IDs remain unchanged, while native workspace inputs use the
stable execution path and Tyde keeps host-root project ownership. Native catalog
IDs join to that canonical session ownership; an unowned native transcript keeps
its reported execution path rather than being assigned to the caller's project.
Listing shared execution paths without the canonical session store fails explicitly.
Host-serviced native file and terminal requests carry their driver's live
execution descriptor. They use that instance's namespace, runtime files and
services, rather than launching another environment. Closing the instance
invalidates the descriptor; it never retargets a new launch. Private runtime
files initialize afresh on each native driver launch, including resume.
Host-only controls reject parent traversal and engine-private paths instead
of projecting them onto unrelated host files.
Live cross-engine conversation moves remain unsupported.
Modes requiring nested namespaces fail explicitly if host policy denies them.

## Optional project configuration

A tracked `.tyde/workbench.json` configures opaque snapshot paths and runtime
references. With no file, arbitrary normal commands still run, but no artifact
snapshot is produced. Tyde does not recognize Cargo, npm, Make, compiler
fingerprints, output-directory conventions, build success or application readiness.
It never builds a project automatically on create, start or remove.

For example, a Python project can explicitly configure:

```json
{
  "snapshot": {
    "producer": ["python3", "assemble.py"],
    "include": ["outputs", "generated.data"],
    "provenance_paths": ["/usr/bin/python3", "/etc/os-release"],
    "provenance_environment": ["PROJECT_PROFILE"]
  },
  "environment": {"PROJECT_PROFILE": "development"}
}
```

The producer runs **only** when requested explicitly inside the environment:

```
/mnt/tyde-runtime/bin/tyde-workbench-produce
```

That helper runs the configured command unchanged, joins its descendants, and
records successful completion only with clean, unchanged committed inputs. It
is not a test/validation receipt. Normal commands are not intercepted. The
project must declare all external toolchain, ABI, profile and other inputs on
which its opaque artifacts depend; incomplete declarations cannot establish
compatibility. Provenance paths hash actual referenced files/directories without
copying them. Selected environment values are hashed, not logged or stored raw
in snapshot receipts. Configuration, source commit/tree, host platform and
OS identity are also part of the cohort. No generic mechanism can certify
undeclared external inputs or an incorrectly specified producer.

On non-forced removal, Tyde joins owned agents, terminals and DEV processes,
then seals only when the producer receipt, current provenance, source metadata
and opaque artifact inventory still match. No live compiler outputs or shared
live caches are cloned. A missing compatible cohort is honest cold initialization,
not a warm build or DEV-ready claim. Forced removal discards artifacts. Compatible
recipients get strict reflinks with independent inodes and preserved metadata;
relative project-internal symlinks and empty directories are preserved. Traversal,
external/absolute artifact symlinks, special files and overlapping includes fail.
Never include credentials, live service/cache directories, validation receipts
or served output that the project's next normal launch must regenerate.

Cohorts live under the Git common directory's `tyde-workbench-snapshots-v2`.
Owned runtime resources live beside the host project store in
`workbench-environments`. Persistent removal phases allow retry without reopening
partly removed environments. No second project/workbench registry exists.

## Explicit runtime and external services

Optional configuration fields are typed by `protocol::WorkbenchConfiguration`:

- `environment`: explicit command environment overrides.
- `remove_environment`: explicitly removed inherited environment keys.
- `writable_paths`: existing absolute host directory/file references.
- `runtime_directories`, `runtime_files`: private per-launch relative resources.
- `runtime_file_bindings`: read-only overlays of a configured private runtime
  file onto an existing native config file (`runtime_file`, `host_path`).
- `services`: owned foreground commands with `name`, `command`, `environment`,
  `ready_notification` and `stop_command`.

References may use `${HOME}` and other existing environment variables, plus
`${TYDE_WORKBENCH_ROOT}` and `${TYDE_WORKBENCH_RUNTIME}`. Missing variables and
invalid/absent explicit bindings fail rather than guessing replacements.
Service readiness is an explicit nonempty Unix-stream notification to its
configured relative runtime socket, or a real process exit. Services run in the
same mount namespace as commands. Unexpected service exit fails execution.
Commands with configured services wait for owned descendants while monitoring
service failure. Ordinary commands without services terminate and join leftover
descendants after their native leader exits. Shutdown runs the configured stop
command, or sends SIGTERM when that is the configured empty-stop policy, and
joins the owned processes. Failure is surfaced;
Tyde never controls host/peer services.

**External host services are not automatically namespace-compatible.** A daemon
that opens logical project paths must run inside this workbench namespace and
use private endpoints/storage. Projects using compiler daemons such as sccache
must explicitly configure their foreground service, endpoint/cache environment
and any native config overlays that a login shell sources. Tyde does not infer
this from a language or silently reuse a host cache daemon. Other external
services require equivalent explicit configuration; unconfigured compatibility
is not claimed.

## Validation prerequisites

Linux behavioral regressions require actual Bubblewrap user/mount namespaces
and a filesystem supporting strict reflinks. Missing prerequisites fail those
checks with actionable errors; separate protocol flows cover typed unsupported
states. Native runtime behavior on macOS/Windows does not require Bubblewrap.
DEV teardown cancels pending frontend/desktop readiness and joins owned process
groups and output capture before acknowledging workbench removal.
