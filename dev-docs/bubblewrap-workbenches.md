# Bubblewrap workbenches

Settings → Workbenches selects the engine for **new** workbenches on that host.
Native is the default everywhere. Existing workbenches do not change. UI and
MCP creation use the same host-owned Git lifecycle and record the environment
on the normal workbench project. Worktrunk is neither used nor required.

Bubblewrap requires Linux, working unprivileged namespaces, Python 3.9+ with
pidfd support, Git, and GNU cp with strict reflinks on the source filesystem.
Basic namespace execution requires no compiler or package manager. Artifact discovery uses Python 3.11+ for Rust manifest/configuration inspection.
Missing project build tools leave the checkout cold; agents install/build only
when directed by normal repository instructions or steering. Unsupported
configuration fails explicitly: there is no Native or full-copy fallback.
The settings capability report is probed during host initialization, not on each
connection. Workbench creation validates the current prerequisites again; changing
host tools or namespace policy requires a host restart to refresh that report.

This is a performance environment, not a hostile-code sandbox. Each workbench
mounts its real Git sources at stable logical paths, with private writable files
and artifacts. A single root remains `/mnt/tyde-workspace`. Multiple roots
retain their paths relative to their common registered ancestor beneath that
anchor, so `../shared` resolves to the private registered sibling, not its
parent checkout. Duplicate and nested Git roots fail explicitly rather than
being flattened. Every root participates in the composite revision identity. Host executables and native config/auth remain runtime references;
HOME and credentials are never copied into snapshots. Native filesystem
permissions are preserved, including config/session paths outside HOME; the
engine-owned cohort store is mounted read-only. Backend security/tool policies
and host namespace policy are unchanged. Bubblewrap prepares the mounts; its
host parent enters those user/mount namespaces before starting the command.
Commands therefore retain the caller's AppArmor profile instead of inheriting
Bubblewrap's child profile, which can prohibit nested builds and backend
sandboxes. Namespace setup capabilities are released before command execution.
Nested workbenches replace inherited engine mountpoints inside their own private
mount table; they do not modify the host or parent workbench mounts. All enabled backends use the same
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
Modes requiring nested namespaces use freshly prepared runtime support, not a
workbench's persisted creation-time report. They fail explicitly if host policy
denies them; explicit backend permission settings remain unchanged.
The foreground command owns its exit status, independently of compiler-cache
services. Adopted descendants are joined and their statuses logged; required
service failures still fail the execution explicitly.

## Cold readiness and opportunistic artifact reuse

Creation does not choose or execute build/install commands, project scripts or
background builds. There is no recipe or approval step. An entirely unbuilt
checkout and a new empty artifact store are supported ready states. Agents use
normal repository instructions and steering to decide what to build.
`WorkbenchInitialization::Cold` means usable without restored artifacts;
`Snapshot` means verified private restoration, not tests or application readiness.
Older persisted `Prepared`/`Blocked` states describe the previous contract only.
The existing internal `PrepareBuilds` operation stage prepares the environment
and checks for existing artifacts; it does not authorize a build.

`setup.py` discovers artifact **boundaries**, not build commands. Supported
layouts are project-relative Cargo targets and npm/pnpm/Yarn dependency/output
paths, including declared TypeScript output directories. Every registered root
participates in the composite source/configuration/toolchain/environment identity.
A different revision of any root starts cold. Relative sibling inputs keep their
registered logical layout. Duplicate/nested roots, external local dependencies,
external output paths, conflicting lockfiles and invalid explicit configuration
remain explicit errors. This is not universal build-system support: an unknown
layout remains usable cold, with no automatic artifact reuse.

Discovery may inspect installed host tools with fixed version queries. It never
runs arbitrary project commands; Rust toolchain auto-install/update checks are
disabled for these identity queries. Missing build tools mean no reusable
artifacts for that root, not a namespace failure. Namespace, storage, runtime,
provenance and integrity errors still fail explicitly without a Native substitute.
Yarn identity queries support configuration-free installed tooling only: Yarn rc
files in the checkout, ancestors or HOME, custom rc filenames and tool-dispatch
environment overrides are rejected before invocation, including explicit
`provenance_tools` queries. Delegated managers/plugins are not supported by this
artifact contract. JavaScript identity queries also reject `NODE_OPTIONS` and
`NODE_PATH` loader overrides. These are explicit unsupported configurations,
not permission to run project tooling during discovery.
Toolchain/compiler identities and effective Cargo configuration join provenance.
Representable ancestor Cargo configurations remain read-only references at the
same logical hierarchy. A version-matched sccache wrapper uses an owned foreground
namespace-local service with private storage/endpoints; the host daemon is never
reused or reconfigured. Runtime tools are provisioned only by explicit builds,
not during creation or claimed as portable artifacts.

On normal removal Tyde first joins its agents, terminals and DEV processes.
Existing outputs are eligible for sealing only if committed inputs, provenance
and exact tracked-source metadata still match the environment's creation or
restoration authority. No command interception, producer helper or successful-build
receipt is needed: these are existing artifacts, not certified build results.
The normal build tool remains responsible for deciding freshness and correctness.
Artifacts are inventoried, strictly reflinked and reverified before publication.
Recipients verify the immutable cohort and restore source timestamps only after
content/mode identity checks, preserving real incremental-build behavior.
Cargo fingerprints a build script without `rerun-if` directives by the first
newest package file in directory order. When checkout mtimes tie and directory
order differs per checkout (as on ZFS), that script and its dependents may
rebuild. Tyde does not rewrite timestamps to steer that choice.

Changed source metadata or provenance declines publication with an explicit
reason; it does **not** prevent ordinary source-preserving Git removal. Restoring
source bytes after a scratch edit/rebuild must not publish its modified executable.
No timestamps or receipts are rewritten to make such output eligible. Old
producer-only environments lack recipe-free input authority and discard their
artifacts on removal. No build is run just to enable cleanup. `force` still
controls Git source deletion and discards artifacts; ordinary removal never
implicitly enables it. Dirty-source Git guards remain unchanged.

No output is a normal cold result. Malformed/unsafe artifacts, absolute/external
symlink chains, special files, traversal, overlapping includes and corrupt stored
cohorts remain explicit errors, never hidden as cold success. Compatible outputs
use independent private inodes, preserved metadata and relative links within the
registered roots. Credentials, live service/cache state and validation receipts
must not be selected as artifacts.

Cohorts live in the Git common directory's `tyde-workbench-snapshots-v2`.
Owned runtime resources live beside the host project store in
`workbench-environments`. Creation acquires its environment directory
atomically together with a private creation claim. Persistent removal phases
permit retry without reopening partly removed environments; there is no
parallel project registry.

## Optional project configuration

A tracked `.tyde/workbench.json` can declare custom artifact boundaries,
provenance and runtime resources. It is not a build recipe and never supplies a
command to execute. It overrides conventional discovery for that root.
For example, an opaque Python project's existing output can be selected with:

```json
{
  "snapshot": {
    "include": ["outputs"],
    "provenance_paths": ["/usr/bin/python3", "/etc/os-release"],
    "provenance_environment": ["PROJECT_PROFILE"]
  },
  "environment": {"PROJECT_PROFILE": "development"}
}
```

The agent runs its ordinary build explicitly. Custom projects must declare all
external toolchain, ABI, profile and other inputs on which their opaque artifacts
depend. Paths are hashed, never copied; environment values are hashed, never
stored raw in receipts. No generic mechanism can certify undeclared inputs.
The obsolete `snapshot.producer` and arbitrary `provenance_commands` fields are
rejected; remove them from old tracked configuration. Fixed `provenance_tools`
identities are `cargo`, `rustc`, `node`, `npm`, `pnpm` and `yarn`.

## Explicit runtime and external services

Optional configuration fields are typed by `protocol::WorkbenchConfiguration`:

- `snapshot.provenance_tools`: fixed installed-tool identity queries.
- `snapshot.optional_include`: declared artifact paths allowed to be absent.
- `readonly_bindings`: native file/directory references at private logical
  ancestor mountpoints (`host_path`, `execution_path`), never source overlays.
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
use private endpoints/storage. Rust artifact discovery handles a version-matched sccache wrapper using a private
foreground service. Custom runtime configurations and other compiler daemons must declare
their service, endpoint/storage and native configuration overlays explicitly.
Tyde never silently reuses a host daemon for namespace-local paths. Other external
services require equivalent explicit configuration; unconfigured compatibility
is not claimed.

## Validation prerequisites

Linux behavioral regressions require actual Bubblewrap user/mount namespaces
and a filesystem supporting strict reflinks. Missing prerequisites fail those
checks with actionable errors; separate protocol flows cover typed unsupported
states. Native runtime behavior on macOS/Windows does not require Bubblewrap.
DEV teardown cancels pending frontend/desktop readiness and joins owned process
groups and output capture before acknowledging workbench removal.
