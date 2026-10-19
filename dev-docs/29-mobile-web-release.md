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
A source-checked version-only snapshot is merged into main's ancestry without
replacing newer source, and the stable tag points at that snapshot. Main advances
to `0.9.5-beta.0`; the next beta with source changes is `0.9.5-beta.1`. The
`.0` is bookkeeping, not a published beta. Older beta tags in the current cycle
can be selected even when newer changes have already landed.

Beta and stable require exactly the same local source validation: ordinary
implementation commits pass `./dev.sh check` in their committed workbench and
again on clean main before pushing to `origin/main`. Release preparation reuses
that validation. Neither beta nor stable runs extra test/compilation passes for
version-only candidates, promotion landing commits, or release bookkeeping on
main. Release cadence installs no promotion-only suite dependencies or frontend
prerequisite builds. Manual `cut` uses the same source-preservation/version guard
and refuses source differences from fetched `origin/main`; land and validate
actual changes through the ordinary implementation path first.

This trusts the existing validated-main process, not a CI proof of local test
execution. Git history cannot independently prove those tests ran. The release
guard reports unchanged source and **no tests run**, not a new test-pass receipt.
No check cache records are copied or forged; ordinary cache keys and gates are
unchanged. Only release version values are normalized: changes to dependencies,
tests, configuration or tooling cannot masquerade as version bookkeeping. When
promoting an older beta, current main's orchestration checks that beta's source
without copying newer tests or application changes into its snapshot.

Both paths require clean trees, unchanged source, coherent versions, and
main ancestry. The workflow atomically pushes main and the annotated tag, then
explicitly dispatches the Release workflow with the expected tag SHA.
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

Use the canonical local guard before release builds, after fetching origin/main:

```sh
tools/release_check.sh [v<release>]
```

It checks clean main, coherent release versions, ancestry and version-only source
preservation relative to `origin/main`. It runs no suites or builds and does not
replace approval, fresh upstream/tag checks or safe pushes in `release cut`.
The GitHub release and mobile-web publication workflows retain actual artifact
builds, signing, packaging and deployed-manifest integrity checks, not redundant
source suites. Linux local checks do not produce signed macOS/Windows installers.
PR checks, CodeQL and ordinary source CI are unaffected. Headless smoke testing
remains in the explicitly requested pre-tag build path.

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

## Explicit macOS artifact recovery

`macos-release.yml` is the macOS packaging path, called by `release.yml` for
normal releases and dispatched separately for an approved architecture-only
repair. It never tags, publishes, deploys mobile web, or builds other platforms.
The application checkout is **`source_sha` at the existing tag**. The controller
checkout is **the dispatch ref's exact `github.sha`**, independently checked
against main ancestry. All new helpers execute from `.release-controller/`,
not from the old application tag. The frozen source's own build scripts and
configuration still build that source; no newer application files are copied
into it. Controller changes follow the normal validated-main implementation
contract above; recovery generates no test attestation and adds no version-only
test/build gate.

Before any notarization, `prepare` retains signed `app.zip`, signed `server.zip`,
and a source/tag/architecture/SHA-256 receipt. App and server submissions use
`notarytool submit --output-format json` **without `--wait`**. Each submission ID
is saved privately in the receipt immediately after the submission returns,
then uploaded as an immutable Actions checkpoint before waiting. `package`
waits on those exact IDs with `notarytool wait`, requires `Accepted`, staples
and validates the app, and runs Gatekeeper. It then creates the DMG **containing
the stapled app**, signs it, and retains its digest plus the stapled, signed
updater before submitting the DMG. That submission ID is checkpointed before
DMG wait/staple/validate/Gatekeeper. A standalone binary's ZIP is notarized but
cannot itself be stapled; this is Apple's documented standalone-binary contract.

A network failure does not establish a signing rejection. Receipts retain exact
started/completed phases, exit codes, Apple endpoint hostnames, network error
codes, DNS results and route/interface observations, never raw tool output,
credentials, submission URLs or signed upload URLs. They are diagnostic evidence,
not a claim of an Apple-wide outage or a fix for an unknown route failure.
Submission IDs alone never stand in for missing payload bytes. Apple documents
submission IDs, waiting, container tickets and stapling in its
[custom notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).

After inspecting a failed run, select the **exact artifact ID and run ID** of
its latest valid checkpoint. Do not rerun the build job to resume. Dispatch
`macos-release.yml` from a reviewed, landed controller commit with the same
`version`, `source_sha`, and `target`, and select exactly one operation:

- `submit-server` / `submit-app`: submit that retained payload, only if no
  submission or prior submission-start event exists.
- `package`: finish app/server notarization and produce/retain the signed DMG
  and updater, without compiling either application or server.
- `submit-dmg`: submit the newly retained DMG once, without waiting.
- `resume`: wait for the retained DMG submission, staple, validate, Gatekeeper,
  then upload only the completed architecture's verified outputs to the draft.
- `assemble`: select the completed macOS checkpoint and `fragments_run_id` of
  the original same-source Release run. Download its other-platform updater
  fragments through run-bound artifact IDs/digests, cryptographically verify
  every referenced updater against the tagged application's pinned public key,
  and compare this architecture's uploaded updater to its retained hash. Only
  after every platform (including stable MSI) and the complete expected release
  asset set exists does it upload the aggregate `tyde-update.json`. It does not
  publish. No fragment or asset is trusted merely because its filename matches.

Every mutation requires an existing draft with the correct channel, unchanged
remote tag, coherent source versions, and source/controller main ancestry.
Restore checks repository/workflow/run/controller provenance, artifact transport
SHA-256, and every retained payload digest. Missing receipts/outputs, wrong
source/architecture/digests and published releases fail closed. A prior ambiguous
submission-start without an ID requires private reconciliation; no operation
silently resubmits it or chooses another artifact. Final checkpoints are uploaded
with `always()` on success, failure and cancellation; the pre-network checkpoints
already exist if a runner is abruptly lost. Inspect successful artifact-upload
receipts: an interrupted upload is not durable storage. Runner work copies,
keychain, certificate and API key are cleaned separately; signed originals and
phase receipts remain in Actions for 30 days. Export a checkpoint securely before
expiry if recovery is delayed. After verified release completion, the Lander may
explicitly delete the selected Actions artifacts; no automatic cleanup deletes
recovery evidence or release assets.

### Frozen v0.9.5 operational handoff

The discarded ARM DMG from the old run cannot be recovered from its submission
ID. After this controller lands, the Lander must obtain authorization for **one
new ARM-only build**, then dispatch this workflow from the landed controller
commit with `operation=build`, `version=v0.9.5`,
`source_sha=83ad2d3c03b4c2661adb22ad435dd073a9530bab`, and
`target=aarch64-apple-darwin`. This builds only that frozen tag, not main's newer
mobile/features. Successful Linux/Windows/Intel platforms are not rebuilt.
If remote completion fails, use the selected checkpoint and explicit operations
above. Once ARM succeeds, `assemble` can reuse the verified other-platform
fragments from Release run `39193235317` with explicit same-source provenance.
Any missing/expired fragment, invalid signature, missing stable MSI or asset
mismatch blocks aggregation rather than triggering a rebuild or partial publish.
The Lander retains authority for final integrity verification and publication;
this tooling change authorizes neither a build nor a release action.
