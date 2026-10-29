# Bubblewrap workbenches

Settings → Workbenches selects the engine for **new** workbenches on that host.
Native is the default everywhere. Existing workbenches do not change. UI and
MCP creation use the same host-owned Git lifecycle and record the environment
on the normal workbench project. Worktrunk is neither used nor required.

Bubblewrap requires Linux, working unprivileged namespaces, Python 3.9+ with
pidfd support, Git and GNU cp. Unsupported configuration fails explicitly;
there is no Native fallback. The settings capability report is probed during
host initialization, not on each connection. Workbench creation validates the
current prerequisites again; changing host tools or namespace policy requires a
host restart to refresh that report.

This is a performance environment, not a hostile-code sandbox. Backend
security/tool policies, read-only modes and host namespace policy are unchanged.

## Whole-workspace clone at the parent's paths

A Bubblewrap workbench is a language-agnostic clone of the parent checkout as
it is: tracked edits, staged changes, untracked and ignored files, installed
dependencies and build outputs. There are no recipes, artifact catalogs or
publication steps, and creation and removal never run a build.

Creation registers a `git worktree add --no-checkout` on the new branch at the
parent's current commit (a different base is rejected), then copies every
top-level entry of the parent except `.git` with `cp -a --reflink=auto`, and
installs a copy of the parent's index. Copy-on-write is used where the
filesystem supports it, otherwise files are copied in full;
`WorkbenchInitialization::Captured { copy_on_write }` reports which.

Inside the workbench the clone is mounted at the parent's own absolute path,
so absolute paths in build outputs, virtualenvs, caches and tool
configuration stay valid. Each root keeps its own path; multiple roots,
`../sibling` references and duplicate/nested root rejection follow from that.
The parent checkout is not visible at its path inside the workbench.

### Git

The workbench has its own worktree administration: HEAD, branch and index are
independent of the parent, while objects, refs, remotes and config are the
shared repository's. Inside the namespace the common directory is mounted at
`/mnt/tyde-git/<root>` and the clone's `.git` file is overlaid read-only to
point there, so the parent's `.git` is never shadowed or required. Normal
remotes, `git push` and landing work unchanged.

Capture fails explicitly for parents with an in-progress merge, cherry-pick,
revert, bisect or rebase; sparse checkout; split index; relative worktree
paths; or submodules with absorbed Git directories. It also fails if the
parent's HEAD moves during the copy.

### Known limits

- The copy is a rolling copy of a live directory, not an atomic snapshot:
  concurrent writes in the parent during creation may be partially captured.
- Per-worktree config, per-worktree refs and stashes are not copied.
- `includeIf "gitdir:..."` conditions see the `/mnt/tyde-git` view path.
- `git worktree add`/`repair` run inside a workbench record view paths.
- A dirty parent produces a dirty workbench; removing it needs `force` like
  any workbench with uncommitted work.
- Host daemons outside the namespace (for example a shared sccache server) see
  the parent's files at those paths, not the workbench's. Such services must
  run inside the namespace; see below.
- Rootless namespaces map only the caller's UID; setuid helpers and files
  owned by other users behave as they do for an unprivileged process.

## Shared HOME, auth and SSH

HOME, credentials, agents and the complete PATH are the host's. OpenSSH
refuses configuration files whose owner it cannot see; for each launch, the
root-owned files reachable from `~/.ssh/config` and `/etc/ssh/ssh_config`
`Include` directives are copied byte-for-byte into private runtime storage and
mounted read-only at their real paths. Only configuration is projected: keys,
known hosts and agents are used in place. Only the default configuration files
are followed; `ssh -F` alternatives are not.

## Execution

Bubblewrap prepares the mounts; its host parent enters those user/mount
namespaces before starting the command. Commands therefore retain the
caller's AppArmor profile instead of inheriting Bubblewrap's child profile,
which can prohibit nested builds and backend sandboxes. Namespace setup
capabilities are released before command execution. Nested workbenches replace
inherited engine mountpoints inside their own private mount table; they do not
modify the host or parent workbench mounts. All enabled backends use the same
host/execution workspace boundary.

Agents, terminals, reviews and DEV services all use the workbench's one
binding descriptor. Native session IDs remain unchanged, while native
workspace inputs use the execution path and Tyde keeps host-root project
ownership. Host-serviced native file and terminal requests carry their
driver's live execution descriptor; closing the instance invalidates it.
Private runtime files initialize afresh on each native driver launch,
including resume. Host-only controls reject parent traversal and engine-private
paths (`/mnt/tyde-runtime`, `/mnt/tyde-git`, `/mnt/tyde-producer-receipts`).
Live cross-engine conversation moves remain unsupported. Modes requiring nested
namespaces use freshly prepared runtime support and fail explicitly if host
policy denies them. The foreground command owns its exit status, independently
of runtime services. A launch owns its terminal only when started by the session
leader controlling it; an engine command run from a workbench terminal uses that
terminal without job control. The capability probe reports `namespace_denied`
only when Bubblewrap exits before delivering its namespaces to the command; any
later failure is an `incompatible_environment`.

## Removal

Removal joins the workbench's agents, terminals, DEV processes and services,
then removes the Git worktree (with `force` when it has uncommitted work) and
the owned environment directory. Nothing is inspected or published; shared
caches and the parent are untouched. A persisted removal phase makes an
interrupted removal retryable. Workbenches created by older releases (logical
root `/mnt/tyde-workspace`, artifact snapshots) remain launchable and are
removed the same way; their old snapshot stores are not consulted.

Owned runtime resources live beside the host project store in
`workbench-environments`. Creation acquires its environment directory
atomically together with a private creation claim; an interrupted creation
removes its partial clone and claim on recovery.

## Project configuration

An optional tracked `.tyde/workbench.json` in the parent declares runtime
resources. It is read from the parent at creation and never supplies a build
command. Fields are typed by `protocol::WorkbenchConfiguration`:

- `environment`, `remove_environment`: explicit command environment changes.
- `writable_paths`: existing absolute host directory/file references.
- `readonly_bindings`: native references at private `/mnt/<name>` mountpoints
  (`host_path`, `execution_path`), never source overlays.
- `runtime_directories`, `runtime_files`: private per-launch relative resources.
- `runtime_file_bindings`: read-only overlays of a configured runtime file onto
  an existing native config file (`runtime_file`, `host_path`).
- `services`: owned foreground commands with `name`, `command`, `environment`,
  `ready_notification` and `stop_command`.

References may use existing environment variables plus
`${TYDE_WORKBENCH_ROOT}` and `${TYDE_WORKBENCH_RUNTIME}`. Missing variables and
invalid bindings fail rather than guessing. Service readiness is an explicit
nonempty Unix-stream notification to its relative runtime socket, or a real
process exit. Services run in the workbench namespace; unexpected service exit
fails execution, and shutdown runs the stop command (or SIGTERM) and joins the
owned processes. Tyde never controls host or peer services.

This repository's configuration runs a private sccache server inside each
workbench, with its own cache directory and socket, because the shared host
daemon would compile the parent's files at the preserved paths.

## Validation prerequisites

Linux behavioral regressions require actual Bubblewrap user/mount namespaces.
Missing prerequisites fail those checks with actionable errors; separate
protocol flows cover typed unsupported states. Native runtime behavior on
macOS/Windows does not require Bubblewrap.
