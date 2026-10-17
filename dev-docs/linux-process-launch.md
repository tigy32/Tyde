# Owned process creation

## Diagnosis, not a kernel workaround

The Ubuntu incident was reported on an external machine. Its kernel log and
any linking kernel lock were unavailable. Neither THP/XFS nor a page-fault
livelock was reproduced here. Finite fork overhead is not evidence of an
infinite kernel failure. No kernel settings, live hosts or reported PIDs were
changed or inspected.

The installed Rust is 1.99.1, commit
`58cf8f9c35b355199727627d07ae34a4e2846d67`; GNU development libc/compiler are
glibc 2.43 / GCC 15.2. Rust's Unix implementation rejects posix_spawn when
pre_exec hooks exist, and when a bare executable plus an overridden PATH needs
its fork-side search. command-group 5.0.1 installs a pre_exec group hook; Tyde
also installed a parent-death hook. Tokio's spawn enters that synchronous std
spawn, including waiting for the exec-error channel. Enough held children can
therefore occupy every runtime worker without any shared Backend-state lock.

Unprivileged seccomp notifications on disposable owned processes observed:

| Path | Actual creation |
| --- | --- |
| Old GNU Tyde / held real Claude exec | Non-VM, non-VFORK clone |
| Installed static musl beta.19 / startup probes | SYS_fork |
| GNU std / absolute program, native group, no hooks | VM + VFORK clone3 |
| GNU std / pre_exec, or bare program plus PATH | Non-VM, non-VFORK clone |
| Current canonical GNU path | VM + VFORK clone3 |
| Current canonical shipped-musl path | VM + VFORK clone |

A bounded 512 MiB touched-memory fixture measured about 0.34 ms for efficient
GNU creation versus 30–41 ms for fork. This demonstrates finite overhead only.
The actual linked shipped-musl binary's posix_spawn disassembly calls __clone
with 0x4111 (CLONE_VM | CLONE_VFORK | SIGCHLD); it is not GNU-host evidence
substituted for musl. Release CI builds the server for musl and the desktop for
GNU. The local musl ELF has no interpreter; local GNU requires GLIBC_2.42.

A one-worker real server completed initial Hello/Welcome/bootstrap, then held
an actual non-ephemeral Claude exec. The old fork artifact could not complete
an unrelated Welcome within the individual 600-second test guard. The same
fixture on rebuilt GNU and musl completed unrelated Welcome/bootstrap and
CloseAgent while held, then rejected/reaped the late child before host shutdown
and published no late session or agent. Each candidate trace contained 53
host-owned process-creation calls, all VM/VFORK (GNU clone3, musl clone).
The old artifact is identified by hash, not claimed to be an exact a543 build.
Private manifests and scalar-only syscall records are under ignored
`target/stalled-launch-evidence/`.

The held-exec fixture also exposed a separate Claude adapter bug: its detached
startup survived shutdown, and late runtime admission did not recheck closing.
Claude now cancels pending starts from its existing lifecycle authority and
rejects runtime installation on the existing state actor after close. Typed
questions, no-echo, stale-answer rejection and turn authority remain intact.

## One canonical runtime API

`tyde-process-env::launch` owns descriptions, native creation, cancellation,
stdio leases, groups and reaping. Backend subprocess re-exports that API; it is
not a second implementation. Async callers await group_spawn/output. Existing
synchronous Git/probe callers use its blocking adapter off runtime workers.
Entrypoints initialize login-shell PATH before runtime startup. That raw PATH
query uses the same engine with a helper session action, not a Rust hook.

Linux calls libc posix_spawn directly, executing a descriptor-pinned copy of
the running packaged executable in private helper mode. Darwin calls native
posix_spawn. No Rust callback executes inside libc's shared-address-space
child; there is no fork retry, shell wrapper, runtime download, separate helper
installation, or configurable spawn strategy. The fresh helper then performs
cwd, setsid/controlling-tty actions and CommandExt::exec in its own address
space. Avoiding addchdir_np also preserves the desktop's macOS 10.13 minimum.
Linux requires pidfds and procfs; missing capabilities are explicit errors.

Initial helper argv is fixed and its environment is empty. Target args/env/cwd
travel through a private socket, never helper argv or logs. Before reading the
plan, the trusted helper closes unrelated descriptors. Reserved control,
original-parent pidfd and creator-thread handles are CLOEXEC at target exec.
The error channel reports the exact failing stage and errno, including a missing
target; an EOF after readiness means successful exec, not guessed readiness.
Normal target argv visibility is unchanged.

Parent-death protection is installed after the first helper exec. Socket peer
credentials, the original-parent pidfd and getppid validate the original
process without accepting PID reuse. An inherited O_PATH procfs thread anchor
also rejects a creator thread that died before PR_SET_PDEATHSIG: a process
pidfd alone cannot detect that race. The procfs anchor never follows a reused
numeric TID. The creating native thread remains alive through physical reaping,
including late completion after cancellation. Four actual process/race cases
passed on GNU and musl; the creator-before-prctl case was RED without the anchor.

Pending ownership is registered before native creation. Dropping/cancelling
startup can settle the async caller before native return; eventual completion
is rejected, signalled and reaped on the same native owner. Backend lifecycle
still owns successful children. Short synchronous native-handle registration /
signal critical sections never contain an await or a native spawn/wait; no
async lock or blocking pool is used for launch isolation. Obsolete unused
SubprocessBridge and its runtime-dependent reaper/try-lock fallback are removed;
LSP leases also reap without requiring a runtime in Drop.

Before posix_spawn returns, an inaccessible kernel-stuck child may not have a
userspace handle. Tyde retains its owning thread/request and cleans up an
eventual return; it does not claim instant recovery of an indefinitely
inaccessible kernel task. No arbitrary startup deadline masks this limitation.

Independent managed hosts and external applications explicitly transfer
lifetime. Until the live caller authorizes a private handoff, normal parent
protection and cancellation apply. Managed hosts retain SIGHUP immunity without
nohup/background shells. The actual post-bind host acknowledges startup over a
private socket or startup fails on real child exit, not a timer. Managed launch
preserves and verifies the installed executable alias used by ownership checks.
A departed acknowledgement receiver after authorized transfer does not revoke
the already-running service.

Desktop relaunch retains Tauri's secure current_binary/AppImage resolution and
Mac CFBundleExecutable validation. A Mac helper is prepared before an update
can replace its executable, then admits the installed target after installation.
Windows stdio uses native CreateProcess suspended, job assignment and resume;
owned jobs kill/await the process tree, while explicit lifetime transfer clears
kill-on-close without leaking a job handle. The canonical terminal adapter
retains native Windows ConPTY CreateProcessW. Unix openpty I/O is retained, with
session/controlling-tty and signal actions performed by the fresh helper.

## Inventory and enforcement

| First-party runtime area | Canonical integration |
| --- | --- |
| All Backend adapters | CLI sessions, version/help/model/import/capacity probes; remote SSH |
| Host / desktop / bridge | Managed host, SSH router/bootstrap, MCP already-owned stdio pipes |
| Git / projects / setup / LSP | Same description and native engine; no Git PATH retry |
| Terminal | Canonical PTY adapter; Unix helper, Windows native ConPTY |
| Desktop open/relaunch | Explicit external-application lifetime transfer; no Tauri restart spawn |
| Dev-driver / debug MCP | Same engine and packaged executable helper entrypoint |
| Client / agent-adapter / browser / mobile web | No native process-creation API found |

Remaining direct runtime APIs are intentional native boundaries, not exceptions
that fork:

- `process-env/src/launch/unix.rs`: libc posix_spawn; std Command is **exec-only**
  inside the fresh helper, never spawn/output/status.
- `process-env/src/launch/windows.rs`: Tokio Command.spawn invokes CreateProcess,
  with owned Windows job setup before target execution.
- `process-env/src/launch/pty.rs`: Windows portable-pty spawn_command invokes
  ConPTY CreateProcessW, retaining the original native console transport.

Direct APIs outside production runtime are classified separately:

- `server/tests`, `tyde-server/tests`, `process-env/tests`, `tests/tests` and
  cfg(test) updater fixtures create disposable real processes/providers. The
  parent-death C fixture deliberately forks isolated fixture parents to test
  kernel parent/thread-exit semantics, never a production launcher.
- `server/build.rs` compiles that C fixture only for Linux test-support builds;
  other build.rs files and `vendor/webrtc-audio-processing-sys` launch compilers,
  archivers and provisioning tools, not application children.
- `dev.sh`, `build.sh`, `tools/{native-build-tool,provision-native-build-tools,
  pretag_release_build,release_tool,release_cadence,smoke_headless_release,
  run-test-hang-guard}.py`, `tools/{mobile-ui-server,trunk-command}.mjs`, shell /
  PowerShell build/check/release/smoke scripts and their test fixtures orchestrate
  development, test and packaging tools. They are not shipped host launch paths.
- `tests/tests/conformance2/*.py`, `process-env/tests/login_shell_terminal.py`,
  mobile e2e / web loader test scripts and `services/chatgpt-dots/test` are
  boundary harnesses. Browser Workers/worklets and deployed services contain
  no first-party native creation API.
- The remote managed shell's filesystem/status commands execute inside SSH's
  interpreter. Its Tyde host launch itself now delegates to the packaged host's
  canonical API, not nohup or a background shell.

`tests/build.rs` walks production Rust source with syn, resolves imports/aliases,
and rejects raw command constructors, spawn wrappers, Rust pre_exec, raw fork /
vfork, PTY bypasses and framework restart calls outside the native boundary.
This caught the existing WebviewRecovery request_restart bypass during this
change. The kernel observer checks every creation on the original native
spawning thread, including a libc retry after clone3, without denying forks to
providers or their descendants. It is test-only, not inherited production
seccomp policy.

Third-party CLI/provider/browser/compiler/installer internal process creation is
not under Tyde's control. In particular the existing Tauri updater's native
installer helpers remain third-party operations; no privileged installation or
security-policy weakening is introduced. Mac x86/ARM and Windows process-env
cross-Clippy pass locally; that is not native OS runtime certification. A GNU
AppDir with 124 actual linked libraries and ORIGIN-relative RPATH passed helper
loading and parent-death checks; this is not a published AppImage or a
cross-distribution certification.

## Regression evidence and outstanding gates

The coarse real-UDS server flow holds kernel creation with one runtime worker,
asserts VM/VFORK, fresh handshake/bootstrap, an unrelated busy/idle/close,
pending CloseAgent before native return, late physical reaping and no target
execution/session resurrection. It also exercises real cwd/env/stdio/fd
isolation, descendant cleanup after leader exit, explicit exec errno and four
parent-lifetime races through an actual protocol-requested process fixture.
The mock is strictly above Backend, not provider coverage.

Retained phases proved a prior test hang was an incorrect AgentClosed stream
predicate: busy and idle both arrived, then the test waited on the agent stream
although Host publishes closure on host streams. Matching the exact typed
AgentClosed agent ID corrects only that predicate; cancellation/reap/absence
oracles are unchanged. This complete flow passed in dd9218d5's native gate.
That gate remained red for a separate managed-host executable-alias regression,
now corrected without loosening its identity checks.

The expanded real_process_launch case holds actual creation during real
Backend::spawn, proves same-runtime UDS progress, cancels before native return,
checks late rejection/reaping, then performs ordinary startup, pipe round-trip
and clean shutdown. Setup, collection and assertions are identical for every
eligible provider. Its first expanded run passed 5/8: Codex exposed a real
pending-start cancellation gap; Hermes/Kiro accidentally intercepted cold
synchronous PATH initialization. All now perform the same production-equivalent
PATH prerequisite before observation. Codex's existing startup cancellation
guard now cancels a native ownership subtree immediately, while leaving its
startup future alive to perform existing resource cleanup. Parent registration
is retained, so enclosing Backend/host shutdown authority is not bypassed.
Matching all-provider GREEN, final committed check and fresh review remain
required; earlier healthy-only 8/8 results do not validate this expanded case.

## Primary references

- [Installed Rust Unix source](https://github.com/rust-lang/rust/blob/58cf8f9c35b355199727627d07ae34a4e2846d67/library/std/src/sys/process/unix/unix.rs)
- [CommandExt hook/exec contract](https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html)
- [glibc posix_spawn VM/VFORK behavior](https://www.man7.org/linux/man-pages/man3/posix_spawn.3.html)
- [fork CoW/page-table costs](https://www.man7.org/linux/man-pages/man2/fork.2.html)
- [PDEATHSIG thread/exec semantics](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html)
- [Kernel procfs descriptor/PID reuse contract](https://docs.kernel.org/filesystems/proc.html)
- [Apple native exec/session ordering](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_exec.c)
- [Apple Unix peer identity](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/uipc_usrreq.c)
- [Microsoft job ownership](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)
- [ConPTY creation](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session)
- [ConPTY close/drain semantics](https://learn.microsoft.com/en-us/windows/console/closepseudoconsole)
- Installed Tauri 2.11.5 process.rs/app.rs; tauri-utils 2.9.3 config.rs;
  command-group 5.0.1 winres.rs/tokio/child/windows.rs; portable-pty 0.8.1.

### 15985b77 checkpoint

The identical expanded real_process_launch scenario passed all eight providers:
Claude, Codex, Hermes, Antigravity, Kiro, Grok, Muse and OpenCode. This includes
held native creation, runtime progress, cancellation before return and late
physical reaping, not only healthy startup. The committed check passed all 556
native cases, the three native cross-Clippy targets, GNU/musl builds and 852
desktop wasm cases. Its later RTC fixture lacked the configured packaged helper,
and the wrapper contract counted native plus cross Clippy as one invocation.
The wrapper now supplies the same built helper to native wasm-side fixtures,
and the contract requires exactly one native plus each of the three cross
invocations. A complete committed check is still required.

Fresh whole-range review was refused before any reviewer launched. Its
full-context serialized diff has at least 17,109,660 bytes of line records,
exceeding the 16,646,144-byte transport limit before file/hunk overhead.
`ReviewState::ensure_committed_diff` requests FullFile context through the
transport-limited read_diff path. No files or commits were omitted to evade
this limit, and refusal is not treated as review approval.

Real dev-instance QA also remains incomplete: the actual cargo-tauri CLI
terminated its frontend wait after 180 seconds while Trunk was still cold
compiling (one run recorded Cargo finishing at 176 seconds). Tyde's own
readiness wait has no startup deadline. No timer was extended and no healthy
command was manually stopped. This is retained failed QA, not a rendered-UI
pass or a claim that packaged native startup is broken.

The next committed check passed launch/native-platform stages but exposed a
root-disappearance race in the existing real project protocol flow: Git status
started on a readable root, the fixture removed it, and Git returned ENOENT.
The background warning reached the client as CommandError instead of allowing
the file-list's typed Unavailable root status to remain the authority. Git
status now rechecks root availability after failure, matching the file scanner's
existing race handling. Other Git errors still propagate; no error-string
heuristic, clean fabricated status, deadline or assertion weakening is used.
The failing unavailable_project_root_keeps_other_roots_live flow is retained.

The run11 native regression reached `CleanupFinished` but then hung in its
seccomp observer, not in child reaping. A disposable native fixture joined the
last filtered thread and observed POLLHUP without POLLIN. Linux
`seccomp_notify_recv` returns ENOENT at EOF, and AsyncFd reports HUP as readable.
The observer's retry loop could therefore run without yielding to its queued
reaping observation. The native EOF fixture also confirms ENOENT here. The
observer now stops receiving notifications at
HUP and continues waiting for the retained physical-reaping observation. No
cancellation, child-count, PID-absence or no-late-exec assertion was removed.
Source: https://github.com/torvalds/linux/blob/v6.17/kernel/seccomp.c

## Cold dev-instance frontend ordering

The installed cargo-tauri 2.10.1 starts `beforeDevCommand` and then performs
90 frontend connection attempts with a two-second delay. Both retained cold
attempts exited through that CLI deadline while compilation was still healthy.
The debug driver and server debug MCP now own the frontend process separately,
wait for HTTP 200 from a fresh per-instance dist or an actual process/I/O error,
and only then start Tauri with `beforeDevCommand` cleared. Tauri's readiness
contract is not weakened or reconfigured. Frontend lifetime belongs to the
instance and cancellation/failure/stop drops its canonical process lease.
The changed instance registries are channel-owned actors, not async locks.
The existing real debug-MCP launch failure flow now gates an actual HTTP
frontend, proves desktop ordering and frontend exit, and retains its isolated
Claude-state/credential assertions. Main subsequently removed the artificial
106-second fixture delay; integration retains that change and its server-owned
ephemeral debug listener rather than restoring the port reservation race.

Primary source for the installed CLI's ordering and fixed retry count:
https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.10.1/crates/tauri-cli/src/dev.rs
Trunk 0.21.14 builds before binding, but can serve an old dist after a build
error; the fresh dist plus successful index request prevents that false ready:
https://github.com/trunk-rs/trunk/blob/v0.21.14/src/serve/mod.rs

## Integration on c23308bd

The first rebased committed check rejected a newly landed direct Tokio Git
launch in `read_review_git` through the production architecture guard. That
read-only tool now uses the same canonical launch API as other Git callers,
retaining its arguments, authorization, pagination and explicit errors. The
canonical lease also retains cancellation and reaping rather than delegating
kill-on-drop to Tokio. The existing real review-MCP protocol flow exercises
this Git read and root authorization; provider behavior is unchanged.
