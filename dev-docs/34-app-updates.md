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

A restart stops the embedded local host and local running agents. The prompt
states this before consent. Saved conversations and settings are retained;
remote servers are not stopped. Restoring the full open-agent working set is
tracked separately in [#68](https://github.com/tigy32/Tyde/issues/68).

## Publishing

The application pins an updater public key in `tauri.conf.json`. Its private
key must be backed up and stored as the GitHub Actions secret
`TAURI_SIGNING_PRIVATE_KEY`; an encrypted key also needs
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. A missing key fails the release build.
Never commit a private key or silently rotate the public key: existing clients
must continue to trust future updates.

The release workflow creates updater artifacts, then `tools/update_manifest.py
package` signs uniquely named packages for each target. Its `assemble` command
requires every platform fragment before uploading `tyde-update.json`. Normal
publication depends on that job. The manual release guard also requires the
complete manifest/package/signature asset set. Draft releases cannot be offered
by the public updater. An installed app selects the highest eligible published
version that carries this manifest, independently of GitHub's latest-release
pointer. Metadata and downloads use HTTPS and package signatures are mandatory.

The first release carrying this implementation must be installed normally.
Subsequent signed releases can update it in place. Creating a signing key and
landing the code locally do not configure GitHub secrets or publish a release;
those remote actions follow the repository's explicit-approval rules.

## Coverage

The native HTTP flow exercises release channel selection, semantic ordering,
draft/unsigned-release exclusion, downgrade prevention, valid signed downloads,
tampering, and failed requests. Its signing fixtures contain only a public key,
signature, and inert package bytes; their private key is not retained in Git.
The DOM flow exercises a server Welcome triggering the prompt, all three choices,
channel selection, disabled controls during download, progress, and a recoverable
installation error. Run both through `./dev.sh check`. These tests never install
an update over the running developer application or call a paid AI provider.
