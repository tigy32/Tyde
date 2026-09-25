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
share one shutdown, with a host-wide 25-second budget. At the deadline, owned
process scopes synchronously kill their groups; runtime Drop/reaper scheduling
is not relied on to deliver that signal. No process is selected by name.

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

On Linux, backend children request `PR_SET_PDEATHSIG(SIGKILL)` and reject a
parent-exit race before exec. This is best effort: it covers the direct backend
child, not descendants that detach or escape its process group. macOS has no
parent-death signal equivalent here; an uncatchable host SIGKILL can leave backend
processes running. Graceful shutdown kills owned groups on both platforms.

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
