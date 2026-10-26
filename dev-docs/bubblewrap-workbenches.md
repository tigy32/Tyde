# Bubblewrap workbenches

Settings → Workbenches selects the engine for **new** workbenches on that host.
Native is the default everywhere. Existing workbenches do not change. UI and
MCP creation use the same host-owned Git lifecycle and record the environment
on the normal workbench project. Worktrunk is neither used nor required.

Bubblewrap requires Linux, working unprivileged namespaces, Python 3.9+ with
pidfd support, Git, and GNU cp with strict reflinks on the source filesystem.
Basic namespace execution requires no compiler or package manager. Automatic
conventional setup additionally requires the project's normal build tools
(and Python 3.11+ for Rust manifest/configuration inspection). Unsupported
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

## Automatic warm setup

Selecting Bubblewrap is sufficient for normal supported projects. There is no
second enable switch, recipe approval or baseline-production approval. During
creation the server emits `ProjectOperationStage::PrepareBuilds`. The app does
not show an app-wide notice for any workbench operation state. Failures and
warnings remain available in the affected project or workbench dialog.

Language-aware discovery (`setup.py`) selects normal commands and artifact
boundaries; the generic engine (`runner.py`) executes opaque producers and
verifies cohorts. Discovery never imports arbitrary host output as a baseline.

- Rust: `Cargo.toml`, the effective Cargo configuration and environment select
  a private project-relative target directory. Project and representable ancestor
  Cargo configurations retain their discovery order through read-only native
  references in the logical hierarchy; contents/credentials are not copied.
  Local dependency, patch, replacement and path-override inputs must remain
  in the registered logical roots. Run `cargo build --locked` with
  an existing lockfile, otherwise build and retain the generated lockfile.
  Compiler/Cargo identities, configuration and build environment participate
  in provenance. An effective sccache wrapper selects the same installed
  sccache version with an owned foreground namespace-local service and private
  endpoint/storage. Caching is not disabled and the host daemon is untouched.
  Tyde's pinned Meson/Ninja provisioner uses the private execution runtime,
  not Cargo's target tree, inside Bubblewrap. Its interpreter-referencing venv
  is never sealed as a portable artifact. Normal native builds keep their
  existing tool location; explicit tool-root overrides remain authoritative.
  Runtime tools are provisioned on demand per launch, not claimed as restored
  compiler artifacts.
- JS/TS: one npm, pnpm or Yarn lockfile selects its package manager. Enforce
  a declared `packageManager` version; perform the locked install and run the
  declared `build` script when present. Retain dependencies and conventional
  `dist`/`build` outputs, plus an explicit TypeScript `outDir` from `tsconfig.json`
  (JSONC supported). Modern Yarn also retains private PnP/install/cache outputs.
  Node/package-manager identities and build environment join provenance.
- Every registered root is mounted before any producer executes. Relative
  cross-root source dependencies and relative artifact links remain local to
  the registered logical layout. A change to any root invalidates the combined
  cohort. Independent roots need no separate setup actions.

An absent compatible cohort triggers a real producer build at those same
stable paths. After successful command/descendant settlement the engine verifies
unchanged source and inventories artifacts, then publishes the sealed cohort. Failed preparation retains private,
environment-secret-redacted build diagnostics outside the cohort and reports
the producer root/exit or underlying typed service failure.
`WorkbenchInitialization::Prepared` means **initial build**, not warm reuse or
validated tests. `Snapshot` means verified private snapshot restoration; the
normal build tool remains the incremental-rebuild authority. Neither state
means the application is running. Per-root `build_blockers` expose unrecognized
build layouts; a project with no supported build manifest reports `Blocked`,
not a warm success. Artifact-link errors identify the selected artifact and the offending link in
its chain; absolute/external links remain rejected, including indirect links.
Tool/configuration, producer, namespace and storage errors
fail explicitly and roll back creation without a Native substitute.

Automatic setup does not guess conflicting package managers, external output
directories, undeclared external workspace links or custom service contracts.
These are explicit blockers. Cargo ancestor configurations deeper than the
available private logical hierarchy also report an explicit blocker. Declared local source dependencies must resolve
inside the registered logical layout; native absolute aliases are rejected
rather than reading a parent checkout. Native projects/workbenches are never
converted. Older manifests without authoritative root mappings fail with a
recreation instruction; execution does not synthesize legacy mappings.

## Optional project configuration

A tracked `.tyde/workbench.json` remains an explicit generic recipe/runtime
contract for custom projects; it takes precedence over conventional discovery
for that root. Its existing explicit producer lifecycle is unchanged. It is
not required for ordinary Rust or JS/TS setup.

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
live caches are cloned. An explicit custom recipe without a completed producer reports cold
initialization, not a warm build or DEV-ready claim. Automatically discovered
recipes instead prepare and seal a compatible baseline during creation. Forced removal discards artifacts. Compatible
recipients get strict reflinks with independent inodes and preserved metadata;
relative links within the registered logical root layout and empty directories
are preserved. Traversal,
external/absolute artifact symlinks, special files and overlapping includes fail.
Never include credentials, live service/cache directories, validation receipts
or served output that the project's next normal launch must regenerate.

Cohorts live under the Git common directory's `tyde-workbench-snapshots-v2`.
Owned runtime resources live beside the host project store in
`workbench-environments`. Persistent removal phases allow retry without reopening
partly removed environments. No second project/workbench registry exists.

## Explicit runtime and external services

Optional configuration fields are typed by `protocol::WorkbenchConfiguration`:

- `snapshot.provenance_commands`: generic command-output identity hashes.
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
use private endpoints/storage. Automatic Rust setup handles a version-matched sccache wrapper using a private
foreground service. Custom recipes and other compiler daemons must declare
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
