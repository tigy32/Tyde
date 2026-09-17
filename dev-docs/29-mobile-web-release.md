# Mobile web release automation

The mobile web/PWA bundle is release-owned infrastructure. A desktop/server
release must not publish without a matching `/tyde/v<release>/` mobile web
bundle built from the same source protocol.

Managed Tyggs Pass + AWS MQTT access is specified in
`30-mobile-managed-access.md`. Release automation still owns bundle/protocol
coherence; the managed broker plan adds the requirement that a production
mobile bundle authenticate with Tyggs through `tycode.dev` before redeeming a
pairing offer.

## Source of truth

- Wire protocol version: `protocol/src/types.rs::PROTOCOL_VERSION`.
- Host release key in pairing QR: the host binary package version, exposed as
  `release_version` by `server::host_release_version()` and stamped into
  `MobilePairingQrPayload`.
- Web manifest authority: `web/loader/manifest.json` as deployed at
  `https://tycode.dev/tyde/manifest.json`.

Generated manifest entries include:

```json
{
  "path": "/tyde/v0.8.19/",
  "entry": "/tyde/v0.8.19/mobile-frontend-...js",
  "integrity": "sha384-...",
  "protocolVersion": 23,
  "artifacts": {
    "/tyde/v0.8.19/mobile-frontend-..._bg.wasm": "sha384-..."
  }
}
```

`protocolVersion` is parsed from Rust by `web/deploy/generate-manifest.mjs`; do
not hand-copy it into JS or workflow YAML.

## Automation

- `.github/workflows/release.yml` deploys the mobile web bundle for every release
  tag via the `deploy-mobile-web` job after release artifacts build. Publishing
  from that workflow is gated on the deploy job succeeding.
- `.github/workflows/mobile-web-release.yml` is the backstop for GitHub releases
  created/published outside the main release workflow. It also provides manual
  backfill through `workflow_dispatch`.
- Both workflows use one global `mobile-web-manifest` concurrency group for
  manifest writers.
- Real deploys use `web/deploy/deploy.sh --confirm`, which automatically fetches
  the live manifest before generation and again immediately before the final
  manifest upload. This preserves newer published entries when an older tag is
  backfilled and fails closed if the live manifest cannot be read or parsed.
- The versioned bundle is uploaded and verified before `manifest.json` is
  published. Loader shell upload explicitly excludes `manifest.json`; the
  manifest upload is last so production never advertises a bundle before its
  files exist.
- Generated manifests raise `minSupported` to the shared mobile-web self-heal
  floor in `web/deploy/mobile-web-policy.json` (`0.8.19-beta.16` as of the
  beta16 protocol-32 repair). If the first protocol-stamped entry is newer than
  that floor, generation raises to the newer protocol-stamped floor instead.
  This keeps older entries in history without advertising them as bootable.
- Loader shell assets are re-uploaded with short cache metadata on every deploy;
  do not switch this back to metadata-preserving `sync`, because unchanged
  `loader.js`/`sw.js` keys can otherwise retain stale long-lived cache headers.

## AWS requirements

GitHub Actions uses AWS OIDC. Configure `AWS_TYDE_WEB_DEPLOY_ROLE_ARN` as a
repository secret or variable. The role should grant only:

- `s3:ListBucket` on `tycode-static`, scoped to the `tyde/` prefix.
- `s3:GetObject` and `s3:PutObject` on `arn:aws:s3:::tycode-static/tyde/*`.
- `cloudfront:CreateInvalidation` for distribution `E3JJ1OF4I8TP6U`.

Do not grant delete permissions. The deploy script also refuses `--delete` and
asserts every S3 destination stays under `s3://tycode-static/tyde/`.

## Local checks

### Nightly betas and stable promotion

`Release cadence` runs nightly at **03:00 America/Los_Angeles**, which is Seattle
local time including daylight saving. GitHub schedules are best-effort: builds
may start late. The workflow uses only upstream `main`. Agents must land,
validate, and push ordinary changes there for the next beta to include them.

No source changes since the newest beta means no new beta. Version values are
normalized for this comparison; dependency changes in lockfiles still count.
Unpublished failed builds are retried at the same tag. Successful nightly builds
are published automatically as prereleases, never GitHub's latest stable.
Enable automatic updates on **Preview** to receive them.

Once the user approves a specific beta, run:

```sh
./dev.sh release promote v0.9.4-beta.3 --confirm
```

This dispatches `Release cadence` on `main` with that explicit beta. The same
input can be supplied manually in GitHub Actions. There is **no automatic stable
promotion**. The example rebuilds the selected beta's exact source, changing
only tracked version metadata to `0.9.4`; it does not include newer main changes.
A validated version-only snapshot is merged into main's ancestry without
replacing newer source, and the stable tag points at that snapshot. Main advances
to `0.9.5-beta.0`; the next beta with source changes is `0.9.5-beta.1`. The
`.0` is bookkeeping, not a published beta. Older beta tags in the current cycle
can be selected even when newer changes have already landed.

Every prepared snapshot passes `./dev.sh check` and the release-coherence
guard; each landing workbench and clean main also passes `./dev.sh check`. The workflow then atomically pushes main and the annotated tag,
and explicitly dispatches the Release workflow with the expected tag SHA.
Dispatch is necessary because pushes using `GITHUB_TOKEN` do not trigger another
workflow. Release builds verify tag ancestry/version/SHA and never overwrite a
published release. Stable builds retain the existing stable updater behavior;
no client migration is needed.

Repository Actions must permit `contents: write` and `actions: write`, and branch
protection must permit the release bot's ordinary main updates. No PAT is needed.
Existing signing and mobile-web deployment secrets remain required. If main
moves during preparation, the run fails before pushing; rerun against current
main. No force pushes or automatic conflict resolution occur on main.

A failed build leaves a draft. Inspect Release cadence diagnostics for
preparation failures, and the Release workflow for build/deployment failures.
If a tag was pushed but dispatch failed, rerun the nightly for a beta, or dispatch
Release on main manually with `version`, the exact tagged `source_sha`, and
`publish=true` for an approved stable promotion. Never delete/recreate tags to
recover. `release status/wait/verify` also work for SHA-bound main dispatches.

The deterministic human release entry points are:

```sh
./dev.sh release prepare v<release> [--commit]
./dev.sh release cut v<release> --confirm [--no-wait]
./dev.sh release status v<release>
./dev.sh release wait v<release> [--timeout 90m] [--interval 30]
./dev.sh release verify v<release>
./dev.sh release publish v<beta-release> --confirm
./dev.sh release promote v<beta-release> --confirm
```

`cut` requires the explicit `--confirm` mutation gate before it runs the
release-coherence guard, pushes `main` before the annotated tag, and waits
by polling GitHub unless `--no-wait` is given. Manual tag-push beta workflows leave a verified
draft (nightly dispatches publish automatically); `publish` is the separate beta-only publication step and also requires
`--confirm`. Neither command prompts for terminal input, and neither bypasses
the release checks, hook, clean-tree, or `main` requirements in `AGENTS.md`.
The `wait --timeout` deadline bounds the complete wait subprocess, including
GitHub, network, and helper calls; timeout exits remain `3` when no matching run
was seen and `4` after a matching run reached a nonterminal state. A deadline
hit while normalizing status or verifying a completed run exits `5` as a
network/tool error.

Use the canonical local guard before release builds. It runs the cached full
repository suite and release-specific coherence tests. The GitHub release
workflow does not repeat these tests:

```sh
tools/release_check.sh [v<release>]
```

It validates the repository plus build/version/protocol/mobile-web coherence by
generating a temporary manifest entry for the checked version instead of
requiring `web/loader/manifest.json` to already contain the release. This guard
does not replace the AGENTS release steps for a clean tree, `main`, tag checks,
approval, or pushes.

The native iOS shell is end-of-life. External distribution is unconfirmed, so
if an installed native build exists, migrate to the PWA with Add to Home Screen
and re-scan the host QR. Pairing state cannot migrate from the native Keychain
and app-container stores to the PWA's IndexedDB store, so re-pairing is
required. Any App Store, TestFlight, or native signing retirement remains an
owner follow-up; desktop Tauri signing and notarization stay supported.

Use these focused checks for release-infra changes:

```sh
bash -n web/deploy/deploy.sh
node --test web/deploy/*.test.mjs
python3 -m unittest tools/test_check_mobile_web_manifest.py
python3 tools/check_mobile_web_manifest.py --manifest <manifest.json> v<release>
python3 tools/merge_mobile_web_manifest.py v<release> \
  --base <live-manifest.json> \
  --entry-source <generated-manifest.json> \
  --out <merged-manifest.json>
```

The last command should be run against both the generated local manifest and the
post-deploy manifest fetched from S3.

## Self-heal floor and beta15 recovery

The mobile web loader normally remembers the last successfully paired bundle in
`localStorage` (`tyde.loader.version`) and prefers it when paired hosts exist.
That remembered-first behavior is intentional for supported versions: it lets an
older still-supported paired host boot its matching bundle instead of always
forcing the newest web client.

The beta16 incident is the boundary case this floor protects. An installed PWA
could remember `0.8.19-beta.15` and boot that protocol-31 bundle against a
`0.8.19-beta.16` protocol-32 host. Beta15 predates the loader repair event, so
it can look connected but never receive `HostBootstrap`. Publishing a manifest
with `minSupported >= 0.8.19-beta.16` makes the fresh loader reject the remembered
beta15 target and fall through to the latest bootable beta16+ bundle.

That manifest floor is not injected into an already-running beta15 WASM bundle.
Users already stuck inside beta15 need a loader reload boundary after the live
manifest is fixed: force-quit/swipe away and reopen an installed iOS PWA, reload
the page, or clear site data if the shell is wedged. Simply backgrounding and
foregrounding an installed PWA may not be enough to re-run the loader. Once the
beta16+ bundle is running, future host/client protocol drift can use the
app-dispatched loader repair events
(`tyde:repair-needed` / `tyde:repair-version`) to reload into the matching
bundle without a manual rescan when the app has enough host version information.

## Backfilling a release

To remediate a missed release without deploying from a dirty tree, run the
GitHub Actions workflow **Mobile Web Release Deploy** from the default branch
with:

```text
version = v0.8.19
confirm = true
```

Use the exact target tag, for example `v0.8.19` or `v0.8.19-beta.16`. Pre-floor
prereleases such as beta9 are intentionally not normal backfill targets once the
self-heal floor is beta16: the manifest may retain historical entries, but
`minSupported` makes the loader reject them with `below-min-supported` rather
than advertising them as bootable. The workflow checks out the tag as the
release source, verifies that tag is contained in the default branch, runs
current deploy tooling from the default branch, builds the tag's
`mobile-frontend`, stamps the tag's Rust `PROTOCOL_VERSION`, merges the live
manifest, publishes only under `tyde/`, invalidates CloudFront, and verifies the
deployed target entry.

This backfill fixes fresh loader-routed pairing for that release. It cannot
inject newer app-dispatched repair behavior into an already-running stale PWA
bundle; a host handshake `Reject` by itself only surfaces as a connection error
in those historical bundles. Users already stuck inside that stale bundle may
need to force-quit/swipe away an installed iOS PWA, close/reopen the PWA, reload
or clear site data, or open the target release QR through the loader so the
loader can choose the matching `/tyde/v<release>/` bundle.
Protocol-stamped bundles can use the loader repair path when the app explicitly
dispatches `tyde:repair-needed`, such as after in-app QR/protocol validation
detects drift.
