# Desktop app updates

Implements [#67](https://github.com/tigy32/Tyde/issues/67).

Settings → This Device → Updates controls the **Release** (stable only) or
**Preview** (stable and prerelease) channel. Release is the default. Versions
use semantic precedence, including numeric prerelease identifiers; switching
channels never downgrades an installation.

Automatic checks run at startup and every six hours. A newer version in a
Welcome, Reject, or managed-host lifecycle snapshot triggers an immediate check.
The server supplies a version hint only: it cannot supply an installer URL or
change the selected channel. Repeated hints for the same version are deduplicated;
network failures retry on the hourly cadence. Checks and installs never overlap.

The prompt offers **Never**, **Not now**, and **Yes**. Never disables automatic
checks; users can enable them again in Settings. Not now defers automatic
prompts for 24 hours, across restarts. Manual checks work with either preference.
Yes downloads and verifies a package, installs it, then restarts Tyde. Progress
and recoverable errors remain visible. Dismissing the prompt never downloads a
package. Settings are in the device's Tauri configuration directory as
`updates.json`, written by atomic replacement.

Tauri's updater handles macOS app archives, Linux AppImages/deb/rpm packages,
and Windows NSIS/MSI installers. Package-manager installs can request operating
system administrator authentication. Development binaries cannot replace
themselves. Mobile applications and the web loader retain their existing
platform-specific update delivery.

Before installing, quitting, or restarting, the embedded host runs the same
idempotent shutdown used by standalone SIGTERM/SIGINT and managed-host stop.
Admissions and restoration stop first. Parked agent waits expire with a typed
host-stopped error; every registered actor (including children and team members)
persists its interruption before interrupting and shutting down its backend.
This is not Close: queued messages and restore intent survive. Concurrent callers
share one shutdown. Required actor checkpoints must be durably committed before
owned process scopes are sealed; runtime Drop/reaper scheduling is not relied
on to deliver that signal. No deadline substitutes for the accounting guard,
and no process is selected by name.

The embedded host cannot serve again after this shutdown. If the installer fails
or is rejected (for example, cancelled administrator authentication) after the
host stopped, the update status reports the error and a native dialog shows it;
dismissing the dialog relaunches the unchanged app, whose normal startup
reconstructs the host and continues interrupted turns. Further installs are
refused until that relaunch, so one process never runs two hosts. A download
failure happens before shutdown and leaves the host running.

The consent prompt explains that interrupted turns continue when **Resume previous
agents** is enabled, and that approvals/questions still require a user response.
Remote hosts are not stopped by a local app update. Managed remote stop validates
the recorded PID's command line, sends SIGTERM, allows 35 seconds, then uses
SIGKILL if needed. SSH allows 45 seconds for this operation.

Turn recovery is durable session state: `InFlight` is written at the active-turn
funnel and cleared only at genuine foreground idle; restart changes it to
`InterruptedByRestart`. A crash leaves `InFlight`, reported as `UnexpectedStop`
on resume. Typed recovery notices render in both desktop and mobile history.
Old stores default to no marker.

Continuation is server-owned and runs only when **Resume previous agents** is
enabled. Restoration registers every record under its persisted agent id, so
orchestrators keep the child ids they already hold; a record whose recovery
marker is set gets a server-composed continuation dispatched as a real live turn
with `MessageOrigin::HostRestart`. The connection layer rejects that origin from
clients, backends emit no user bubble for it, and it runs before the agent's
previously queued messages, whose order is unchanged. A backend that resumes its
own live turn is adopted instead of being sent a second prompt. Either path
publishes `RestartRecovery(Continuing)`; a startup failure publishes
`ContinuationFailed` and clears the marker. Idle records get no continuation.
A usage-limit hold delays the continuation until the hold lifts.

Continuations are released in post-order: a parent's is held until each of its
restored children has been admitted, so an orchestrator never sees a child idle
and spawns a replacement. Its continuation names the restored child ids and says
to reuse them through `tyde_list_agents`/`tyde_await_agents`. Every
continuation says to inspect results before repeating side effects and that it
is not a user answer or approval. Manual resume from History reports
`Interrupted` once, clears the marker, and never continues on its own. Once an
agent is prepared for restart it starts no queued work, even if its current turn
ends before its stop command arrives.

On Linux, backend children request `PR_SET_PDEATHSIG(SIGKILL)` and reject a
parent-exit race before exec. This is best effort: it covers the direct backend
child, not descendants that detach or escape its process group. macOS has no
parent-death signal equivalent here; an uncatchable host SIGKILL can leave backend
processes running. Graceful shutdown kills owned groups on both platforms.

## Missing-checkpoint diagnostics

A missing final task accounting checkpoint still refuses shutdown and therefore
installation. This diagnostic change does **not** fix the original updater
failure. No snapshot is reconstructed or substituted, and no continuation or
queue state is inferred from history.

The default info-level native disk logs (`~/.tyde/tracing/tyde-shell.log*`, or
`tyde-host-stdio.log*` / `tyde-host-uds.log*`) include the content-free target
`server::restart_accounting`. `TYDE_TRACING_DIR_PATH` selects an explicit log
directory. Explicit logging filters can disable these records.

Each process gets an independent random `diagnostic_run` nonce, and each actor
gets an `accounting_actor` slot within it. Neither derives from an agent,
session, provider or OS identifier. Both fields accompany every actor record;
the nonce also scopes host phases across concurrent processes and app launches. Its records name creation, stop request,
dispatch, acknowledgement settlement, final checkpoint publication, cancellation
request and termination. Missing-checkpoint errors carry that same slot and the
observed termination class. Host records name preparation, settled stops,
accounting commit/refusal and shutdown settlement. Booleans distinguish a
rejected command from a lost acknowledgement and publication from a missing
checkpoint. No panic payload, error payload, prompt, name, path, argv, environment
or credential is added to these records.

Termination classes come from the lifetime of the submitted actor future:

- `Returned`: the actor's process-scoped future returned normally.
- `Unwinding`: it was dropped while the thread was unwinding. No panic payload
  is read or logged, and the existing panic hook/propagation is unchanged.
- `Dropped`: the future was dropped without return or unwind. An explicit abort
  and runtime teardown are not distinguishable from that observation alone;
  `CancellationRequested` records the explicit abort call when present.
- `Running`: termination has not yet been recorded at the observation point.
  Use the later `ActorTerminated` record for that slot, not an inferred cause.

These observations never drive product lifecycle or accounting decisions. They
add no supervisor, recovery path, waits, timeouts or locks to production shutdown.
A process kill, aborting panic, power loss or full/dropped log sink can prevent a
final record. Missing records are not evidence of normal completion. Correlate by `(diagnostic_run, accounting_actor)`, never by the slot alone. Older builds cannot
retroactively provide a termination class they never recorded.

### Redacted export of existing logs

No new reproduction, provider run, restart, settings change or upload is needed.
From a checkout, run:

```sh
python3 tools/export-restart-diagnostics.py > tyde-update-diagnostics.jsonl
```

An explicit directory can be supplied as the only argument. The exporter reads
existing Tyde tracing files only and emits allowlisted phases, classes,
diagnostic correlation values, booleans, counts and timestamps, never whole lines or file paths. It also recognizes the
older accounting refusal/commit and updater-failure records. An existing runtime
panic header, if retained in those files, is reported without its payload and
without claiming attribution to an actor. Zero records is an evidence limit,
not a successful shutdown or a finding that no panic occurred. Unreadable
files or damaged UTF-8 fail explicitly with content-free context; partial output
from a failed export is not complete evidence.

The protocol-level disk-log regression uses the real server and shared mock
fixture: clean idle shutdown, cancellation after a stop is accepted but before
publication, and a controlled unwind during startup. It checks the unchanged
missing-checkpoint error, byte-identical archived accounting in the actual SQLite store, and typed client
recovery of original identity, continuation, queued input and known tokens.
Archived root rollups are intentionally not advertised before their live actor
is restored; storage auditing supplements the protocol assertions, not a
server-internal method test. Fault injection demonstrates
observability; it does not reproduce or identify the screenshot's original cause.

## Publishing

The application pins an updater public key in `tauri.conf.json`. Its private
key must be backed up and stored as the GitHub Actions secret
`TAURI_SIGNING_PRIVATE_KEY`; an encrypted key also needs
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. A missing key fails the release build.
Never commit a private key or silently rotate the public key: existing clients
must continue to trust future updates.

Tauri signs the installers during the build. `tools/update_manifest.py package`
stages the existing Windows and Linux installers for a single upload, preserving
their names and bytes. macOS additionally needs an app archive for each
architecture, with distinct filenames. Signatures are embedded in
`tyde-update.json`; standalone `.sig` files are not uploaded. This produces 21
assets for preview releases, or 22 for stable releases including MSI, excluding
GitHub's automatic source archives.

The `assemble` command requires every platform fragment before uploading the
manifest. Normal publication depends on that job. The manual release guard also
requires the complete installer/manifest asset set and rejects duplicate updater
copies or loose signatures. Draft releases cannot be offered
by the public updater. An installed app selects the highest eligible published
version that carries this manifest, independently of GitHub's latest-release
pointer. Metadata and downloads use HTTPS and package signatures are mandatory.

The first release carrying this implementation must be installed normally.
Subsequent signed releases can update it in place. Creating a signing key and
landing the code locally do not configure GitHub secrets or publish a release;
those remote actions follow the repository's explicit-approval rules.

## Coverage

The native HTTP flow exercises packaging and manifest assembly for stable and
preview releases, rejection of duplicate assets and missing or mismatched
signatures, release channel selection, semantic ordering,
draft/unsigned-release exclusion, downgrade prevention, valid signed downloads,
tampering, and failed requests. Its signing fixtures contain only a public key,
signature, and inert package bytes; their private key is not retained in Git.
The DOM flow exercises a server Welcome triggering the prompt, all three choices,
channel selection, disabled controls during download, progress, and a recoverable
installation error. Run both through `./dev.sh check`. These tests never install
an update over the running developer application or call a paid AI provider.
