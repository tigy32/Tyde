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
repair. Build/resume/assemble never publish or deploy mobile web; the separate,
explicitly approved stable `finalize` operation deploys and publishes without
building any desktop/headless platform. No recovery operation tags or promotes
source.
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

- `finalize`: requires `confirm_publish=true` and a stable tag. Restore the
  selected completed checkpoint and explicitly select the original fragment
  run again. Recompute the signed aggregate read-only and require its exact
  remote bytes, all 22 stable assets including MSI, each GitHub asset's size and
  SHA-256, every updater's pinned-key minisign signature, valid headless ZIPs,
  Linux installer checksums, and both DMGs' image/signature/stapled-ticket and
  Gatekeeper checks. The repaired architecture's desktop/server/updater bytes
  must match its retained output hashes. This checks integrity, not execution
  of Windows/Linux binaries on a macOS runner or new notarization of old assets.
  Run the existing `web/deploy/deploy.sh --confirm --live-manifest-base
  --source-root <frozen-source>` from an immutable controller-tooling snapshot,
  under the same global `mobile-web-manifest` writer lock. Only the mobile web
  bundle is built, from the frozen checkout; newer application files are never
  copied into it. Verify the S3 manifest against the frozen protocol and the
  public manifest/JS/WASM SRI and WASM MIME using existing verification helpers.
  Recheck source/tag/draft identity and unchanged asset IDs/sizes/hashes before
  publishing that verified release ID with the normal stable semantics:
  `draft=false`, `prerelease=false`, `make_latest="true"`. Reread published state,
  asset metadata and latest stable. Any deploy/coherence/integrity failure blocks
  publication. A failure after the PATCH requires inspecting published state,
  never blindly repeating publication. Published releases remain immutable.

`fragment_runs` (CLI `--fragment-runs`) explicitly overrides individual
platforms with a JSON object of target triples to run-ID **strings**. Omitted
platforms keep the existing selection: this architecture's complete checkpoint,
and `fragments_run_id` for every other platform. An explicit entry never retries
or searches the original run. Unknown/duplicate keys and invalid IDs reject.
Each selected run must be a main-ancestor controller with exact same-source
Release provenance or, for a Mac fragment, the exact same-source macOS
`build`/`resume` run title. Each artifact is selected uniquely by its canonical
platform name, then checked against its own metadata run, controller, ID, size
and transport digest. Assembly retains those IDs/digests and the fragment hash
in `assembly.json`; full pinned-key verification remains mandatory. Explicitly
selecting this architecture also requires its restored checkpoint run and exact retained
fragment bytes. Restore provenance is retained separately from later continuation
checkpoint stamps, including checkpoint artifact ID/digest and receipt hash.
This adds no new build, submission, deployment or publication
path. Stable `finalize` accepts the same map; beta publication remains the
existing beta-only `./dev.sh release publish` path, not `finalize`.

### Dual-recovery beta4 handoff

For `v0.9.6-beta.4`, frozen app/source is
`e9a12ec602d468c87075990af3e8fd0d45b2c635`. Original Release run
`39477893943` has Windows and both Linux fragments, but neither Mac fragment:
both Mac jobs compiled and passed native startup, then failed at the network
completion boundary. The old assembler cannot select fragments from both
recovery runs; this is the confirmed controller limitation, not a new Apple
failure diagnosis. ARM recovery `39491501652` completed and retained final
checkpoint `12296172491`. Intel is being resumed from original final checkpoint
`12302063028`; that original artifact is **not** its eventual complete checkpoint
or fragment run. Root must record Intel's exact successful recovery run after
completion, and its artifact upload receipt. Do not guess a latest run.

After this controller lands, Root can use ARM's existing complete checkpoint
and explicitly select both recovered fragment runs:

```sh
REPO=tigy32/Tyde
INTEL_COMPLETE_RUN='<exact successful Intel resume run ID>'
FRAGMENT_RUNS=$(printf '{"aarch64-apple-darwin":"39491501652","x86_64-apple-darwin":"%s"}' "$INTEL_COMPLETE_RUN")
gh workflow run macos-release.yml --repo "$REPO" --ref main \
  -f operation=assemble -f version=v0.9.6-beta.4 \
  -f source_sha=e9a12ec602d468c87075990af3e8fd0d45b2c635 \
  -f target=aarch64-apple-darwin \
  -f retained_run_id=39491501652 -f retained_artifact_id=12296172491 \
  -f fragments_run_id=39477893943 -f fragment_runs="$FRAGMENT_RUNS"
```

Record and watch that exact dispatch run, checking its controller `head_sha`
against the landed controller and its successful retained `assembly.json`.
This only uploads the verified aggregate. No desktop build or Apple submission
runs. Missing/expired fragments, wrong source, missing provenance, invalid
signatures or an incomplete asset set block assembly. After successful assembly,
Root owns the existing approved beta publication operation:
`./dev.sh release publish v0.9.6-beta.4 --confirm`. Do not use stable `finalize`,
create another release/tag/version, or rerun the original Release DAG.


Draft lookup has one path: paginate the release list, require a unique exact-tag
match, and verify its ID/tag/channel/draft state by ID. It never tries the
published-only tag endpoint. API/auth/rate/transport errors fail closed with a
controlled operation name, HTTP status (or explicitly unavailable), and gh exit
code; raw headers, bodies, credentials and private IDs are not printed. The
read-only operational probe uses the same lookup as packaging:

```sh
python3 tools/macos_release.py lookup-draft --repository tigy32/Tyde --tag v0.9.5
```

The host probe on 2026-10-19 passed against the actual existing draft: 19 assets,
all 19 with SHA-256 metadata, no mutation or secret setup. This does not establish
the failed runner's undisclosed HTTP status, future runner-token access, live
Apple acceptance, or successful deployment/publication.

Every mutation requires an existing draft with the correct channel, unchanged
remote tag, coherent source versions, and source/controller main ancestry.
Restore checks repository/workflow/run/controller provenance, artifact transport
SHA-256, and every retained payload digest. Missing receipts/outputs, wrong
source/architecture/digests and published releases fail closed. A prior ambiguous
submission-start without an ID requires private reconciliation; no operation
silently resubmits it or chooses another artifact. Final checkpoints are uploaded
with `always()` on success, failure and cancellation when a receipt exists;
a preflight failure before any payload explicitly reports **no retained payload**
and skips empty uploads rather than generating secondary retention errors or a
fake checkpoint. The pre-network checkpoints already exist if a runner is
abruptly lost. Inspect successful artifact-upload
receipts: an interrupted upload is not durable storage. Runner work copies,
keychain, certificate and API key are cleaned separately; signed originals and
phase receipts remain in Actions for 30 days. Export a checkpoint securely before
expiry if recovery is delayed. After verified release completion, the Lander may
explicitly delete the selected Actions artifacts; no automatic cleanup deletes
recovery evidence or release assets.

### Post-build rejection and quarantined evidence

The workflow records pre-build checkout/tag/version/draft provenance in a
separate evidence directory, then builds the frozen source. Its `capture` step
runs even after a failed build attempt (but not a skipped one). Before **any**
post-build Git, version, draft, signature or architecture introspection, it
archives only the known `Tyde.app` and `tyde-server` product paths with real tar,
without following symlinks, and records archive SHA-256 plus executable sizes
and SHA-256. Missing products and the build step's actual outcome are explicit;
files present after a failed attempt are not claimed to be fresh successful
outputs. Partial capture and failure diagnostics are preserved too. No runner
home, signing key, certificate, keychain or private-key directory is archived.

`quarantine.json` has its own schema, typed phase and
`quarantined-non-releasable` eligibility. It records requested commits, the
available pre/post source provenance, failed guard observations, and explicitly
unverified binary version, architecture, signatures, notarization, stapling,
Gatekeeper and updater chain. A successful source inspection does **not** change
this eligibility. An immediate quarantine artifact upload precedes server
signing verification/startup smoke/`prepare`; a separate `always()` artifact
retains diagnostics and quarantine receipts after later failures or cancellation. Inspect
the action's successful upload receipt: files on a lost runner are not durable.

Only a successful build with matching pre/post source provenance may proceed to
`prepare`. It rechecks unchanged application inputs and the bounded derived
metadata contract below, tag, versions, draft, binary version,
architecture and code signatures and creates a **separate signed checkpoint**.
A rejected source observation remains rejected even if somebody later cleans
the checkout. Quarantine archives are never extracted or promoted by the
controller. Restore, submission, package, completion, upload, assembly and
finalization reject quarantine as a checkpoint. Existing signed-checkpoint
resume and the Accepted-notary/stapler/Gatekeeper/updater checks remain required;
there is no quarantine-resume or publication bypass.

Git diagnostics name the operation and checkout role, exit code, and semantic
result: missing origin config, tracked changes, non-ancestry, or process failure.
Diffs use name-only/NUL-delimited output, not patches or external diff tools;
counts and bounded safe relative paths are retained. Failed invocations retain
stderr category and byte count, never raw stderr, credentials, remote URLs,
submission identifiers or tool output. Missing refs and repository/config errors
are not reported as successful cleanliness checks. Diagnostics remain available
when no signed receipt can exist.

For run `39233563294` (controller `da1e598d`, frozen source `83ad2d3c`), actual ARM
compilation and desktop startup smoke passed, but post-build `prepare` failed
before a signed checkpoint or Apple submission. The old helper discarded Git
stdout/stderr, so **the exact failed guard remains unknown**. The only retained
artifact contained four startup/tracing logs and no binaries. This fixes those
confirmed diagnostic/retention defects; it does not certify the original RCA,
recover the lost bytes or authorize another compile.

Local real checkouts of those exact commits pass the eight local Git guards
before and after the workflow's `npm ci --ignore-scripts --no-audit --no-fund`.
Installed `tauri-build 2.6.3` (`src/acl.rs`, `save_acl_manifests`/`build`) and
`tauri-utils 2.9.3` (`src/acl/schema.rs`, `generate_capability_schema`) establish
that Tauri writes tracked schema files. A local replay of an already
ordinary-built native build-script against a clean, real frozen 83ad checkout
confirms this path without compiling the app: the script exits 0, and only
`frontend/tauri-shell/gen/schemas/macOS-schema.json` changes. Its SHA-256 changes
from `6f5f8d7fb9c5b031c3abe83fee0184d9148db6d4cd59ba82bd585cada035004e`
to `f3210b918a4926d01dcda27c2f975d3aec67c43a723c741ac166ab0b86b15445`,
matching the already-tracked desktop/Linux schema with `app-updates`.

The replay uses the pinned Tauri/plugin permission inputs, target
`aarch64-apple-darwin`, release-mode metadata, and a PATH without compiler/build
tools. An exec trace shows only the existing build-script process, no child
compilation. The actual da1 helper then fails with the same opaque Git exit 1,
before creating a checkpoint. This proves a **real local generated-source
mutation and guard rejection**, not which guard failed in the historical runner.
The first replay omitted three plugin metadata inputs and failed capability
validation after writing schemas; that incomplete result is retained separately,
not used as the successful reproduction. No macOS application compile or CI
probe was performed.

The pinned producer hardcodes `gen/schemas`; neither its schema function
parameters nor `tauri-build::Attributes` provide an out-of-tree schema directory.
Changing `OUT_DIR` does not redirect this file. Its schema JSON is derived editor
metadata, not a capability/application input: `tauri-build` parses
`capabilities/**/*`, validates those permissions against plugin manifests, and
copies the resolved ACL/capabilities to `OUT_DIR`. Pinned `tauri-codegen 2.6.3`
reads those `OUT_DIR` ACL/capability files to construct runtime authority, not the
platform schema. Tauri also documents these JSON schemas as editor validation
and autocompletion data in [Schema Files](https://v2.tauri.app/security/capabilities/#schema-files).
The installed producer/consumer source files match the cached crate archives;
their SHA-256 checksums match frozen 83ad's Cargo.lock.

The controller now explicitly distinguishes strict `frozen` preflight from
`post-build` source inspection. Preflight requires the entire tracked checkout
and controller to be clean and records a source/run-bound contract from frozen
Git blobs. Post-build inspection may account for **only** this one regular
100644 `macOS-schema.json`, with exactly the frozen desktop schema's SHA-256,
under the audited tauri-build 2.6.3 / tauri-utils 2.9.3 registry checksums. The
receipt records the path, before/after hashes, unchanged mode, pinned producer
and exact-match verification method; it is not a fabricated producer execution
attestation. Initial dirty checkouts, any other bytes/path/mode/symlink,
Cargo.lock, application/capability/ACL/desktop-schema or controller mutations
still reject. Unsupported producer versions get strict tracked-source checking,
not a broader exception. No file is ignored, reset, restored or changed by the
guard. Signed checkpoints carry the derived-output receipt; quarantine remains
non-releasable even when this bounded inspection succeeds.

A corrected local flow runs strict preflight, the real existing native generator,
and the new post-build guard through real nested Git checkouts, local Git/HTTP
transport and CLI/archive boundaries: generation and source inspection pass;
Cargo.lock, Rust source, capability, controller, mode and symlink mutations
reject. Archived product bytes in this flow are explicitly local fixtures, not
newly compiled/signed macOS binaries. Automated regression replays the exact
recorded generator bytes and exercises signed preparation with Apple process
doubles, additional ACL/desktop-schema mutations, and failure quarantine.
The original run's exact failure remains unknown and is not claimed fixed. Any diagnostic Actions probe or further ARM compile requires a
new parent/Lander plan and authorization; the consumed ARM compile authorization
is not reusable. Old notary network failures and Intel upload 422 are not claimed
fixed. Beta/stable validated-main and frozen-source contracts are unchanged.

### Frozen v0.9.5 operational handoff

The discarded ARM DMG from the old run cannot be recovered from its submission
ID. Canonical ARM run `39226704908`, attempt 1, stopped in draft preflight before
compilation or Apple submission; it produced no payload/checkpoint to resume.
That historical authorization was subsequently consumed by ARM run
`39233563294`, which passed compilation/smoke but lost its binaries at the
post-build Git boundary described above. **No additional ARM compile is
currently authorized.** Any later action belongs to the Lander after a newly
approved plan; the historical command below is not authorization to execute it.
Do not rerun the failed original multi-platform release workflow or mark its jobs green.

Run the landed main workflow/controller, never helpers from the old app tag:

```sh
REPO=tigy32/Tyde
SOURCE=83ad2d3c03b4c2661adb22ad435dd073a9530bab

# Read-only preflight, no credential setup or mutations:
python3 tools/macos_release.py lookup-draft --repository "$REPO" --tag v0.9.5

# Only after approval for ONE new ARM build:
gh workflow run macos-release.yml --repo "$REPO" --ref main \
  -f operation=build -f version=v0.9.5 -f source_sha="$SOURCE" \
  -f target=aarch64-apple-darwin
```

Record the **exact dispatched run ID**, not a guessed latest run. Check its
immutable `head_sha` is the landed controller main SHA, and watch completion:

```sh
RUN='<exact dispatched run ID>'
gh api "repos/$REPO/actions/runs/$RUN" \
  --jq '{head_sha,path,head_branch,status,conclusion}'
gh run watch "$RUN" --repo "$REPO" --exit-status
gh api "repos/$REPO/actions/runs/$RUN/artifacts?per_page=100" \
  --jq '.artifacts[] | {id,name,expired,digest}'
```

This builds only frozen stable 83ad, not main's 0.9.6 application. Successful
Linux x64/ARM, Windows and Intel platforms are reused, never rebuilt.
If completion fails, select its exact valid checkpoint, preferably the final
artifact containing the latest phase receipt. Use **one explicit phase** and
record the new run/artifact for each continuation:

```sh
CHECKPOINT_RUN='<exact selected checkpoint run ID>'
CHECKPOINT_ARTIFACT='<exact selected artifact ID>'
# If the DMG submission ID already exists, only resume (no submit/build):
gh workflow run macos-release.yml --repo "$REPO" --ref main \
  -f operation=resume -f version=v0.9.5 -f source_sha="$SOURCE" \
  -f target=aarch64-apple-darwin \
  -f retained_run_id="$CHECKPOINT_RUN" \
  -f retained_artifact_id="$CHECKPOINT_ARTIFACT"
```

For earlier checkpoints select `submit-server` / `submit-app` only when neither
a submission nor prior submission-start exists; `package` to complete those
submissions and retain signed DMG/updater; `submit-dmg` to submit that retained
DMG once; then `resume`. Ambiguous submission starts require private
reconciliation, not resubmission. Unknown Apple/network routes and Intel upload
422 causes remain unresolved; this fix does not claim to repair them.

After completed ARM upload, select its completed final checkpoint. Assemble
only; this is **not** publication approval:

```sh
COMPLETE_RUN='<exact completed ARM checkpoint run ID>'
COMPLETE_ARTIFACT='<exact completed ARM checkpoint artifact ID>'
gh workflow run macos-release.yml --repo "$REPO" --ref main \
  -f operation=assemble -f version=v0.9.5 -f source_sha="$SOURCE" \
  -f target=aarch64-apple-darwin \
  -f retained_run_id="$COMPLETE_RUN" \
  -f retained_artifact_id="$COMPLETE_ARTIFACT" \
  -f fragments_run_id=39193235317
```

Watch that exact assembly run. Missing/expired fragments, wrong source, invalid
signatures, missing stable MSI or incomplete assets block it. Once assembly
succeeds and the owner explicitly approves final deployment/publication of
v0.9.5, use the same completed ARM checkpoint and source-bound fragment run:

```sh
gh workflow run macos-release.yml --repo "$REPO" --ref main \
  -f operation=finalize -f confirm_publish=true \
  -f version=v0.9.5 -f source_sha="$SOURCE" \
  -f target=aarch64-apple-darwin \
  -f retained_run_id="$COMPLETE_RUN" \
  -f retained_artifact_id="$COMPLETE_ARTIFACT" \
  -f fragments_run_id=39193235317
```

Watch the exact finalization run, then inspect its retained `finalization.json`
privately and its published stable/latest state. This supported path verifies,
deploys frozen mobile web and publishes without depending on the old failed
build DAG. `assemble`, `lookup-draft`, `verify`, builds and resumes cannot invoke
it implicitly; `confirm_publish` defaults false and CLI `finalize` also requires
`--confirm`. The existing beta-only `./dev.sh release publish` remains unchanged.
No ad hoc `gh release edit`, source/tag promotion, rebuild of successful
platforms, validation attestation, or version-only source gate is introduced.

### Signed macOS server ZIP contract

`prepare` archives the signed file at
`target/<target>/release/tyde-server` with
`ditto -c -k --keepParent`. The canonical macOS ZIP has exactly one member:
`release/tyde-server`, a nonempty Unix regular executable with mode `100755`.
`prepare` validates that output before recording a signed checkpoint. Every
receipt consumer validates the same contract after checking payload hashes;
`package` verifies the extracted `work/release/tyde-server`. Completion copies
that ZIP byte-for-byte to the headless release asset, and finalization checks
this same macOS layout. Linux and Windows headless layouts are unchanged.
There is no root-member alternative or archive search.

ARM run `39260355030` (controller `25038b19`, frozen stable source `83ad2d3c`)
retained artifact `12194400987` with this exact layout and executable mode.
Its transport SHA-256 is
`fede32580e071b6f1f97d0bbad34d119dac694e4fef60753e7fb071c78cdda05`;
its server ZIP SHA-256 is
`49b0617293c4dd4fb5a8414e38a295f6e82131708899e487cec3f4762cd3ed6e`.
Both submissions were Accepted. The former package consumer instead requested
`work/tyde-server`, and finalization also expected a root-level server member.
This is an archive contract mismatch, not evidence of source, signing or Apple
rejection. The original raw lipo stderr was redacted, so no more specific tool
failure diagnosis is claimed.

The CLI regression uses real ZIP bytes, Git checkouts and HTTP artifact
transport, with platform-command substitutes on non-macOS hosts. Its producer
models the observed file-input ditto layout, and its lipo substitute now rejects
missing paths. Previously the producer substitute stripped `release/` and lipo
reported success regardless of path existence, masking the mismatch. The flow
restores an Accepted checkpoint after deleting the build products, packages it
without submitting/waiting/building, and checks unchanged signed ZIP bytes and
submission records. Malformed members are rejected through restore even when
payload and transport hashes match. These tests do not establish actual macOS
stapler/Gatekeeper behavior; a separately authorized retained-checkpoint
continuation must establish that on macOS. No rebuild or resubmission is needed
to exercise the corrected package path.

An additional continuation flow checks out the actual original `25038b19`
controller and frozen `83ad2d3c` source from real Git history, runs that
controller's `prepare` producer with small product substitutes, and seals an
Accepted checkpoint using the original run/artifact metadata. It deletes the
build products, restores through the corrected controller, proves the original
controller fails at lipo, and proves the corrected consumer reaches signature
verification and the explicit Gatekeeper-rejection boundary without compiling
or resubmitting. This is a fixture artifact, not the retained signed production
payload or a claim that Apple verification passed. The complete synthetic
package flow separately covers updater/DMG continuation. Fixture product modes
are explicit, matching observed `644`/`755` rather than inheriting host umask.

### GitHub download negotiation

Actions artifact ZIP and Release asset endpoints have different contracts.
[Actions artifact download](https://docs.github.com/en/rest/actions/artifacts#download-an-artifact)
negotiates with `Accept: application/vnd.github+json` and returns a signed ZIP
redirect. [Release asset download](https://docs.github.com/en/rest/releases/assets#get-a-release-asset)
requires `Accept: application/octet-stream` for bytes, with either a direct
response or a redirect. The controller uses `gh api` for both, letting gh follow
redirects without logging signed URLs; it requires the final HTTP 200 and
endpoint-specific content type, then checks metadata size and SHA-256 before
consuming bytes. It never retries through an alternate transport.

The package-only run `39276264731` failed before packaging because the shared
binary flag sent octet-stream to Actions. Read-only live requests reproduced
HTTP 415 with that header and downloaded the exact selected retained artifact
with JSON negotiation (HTTP 302 → 200, `application/zip`). Release asset GETs
returned HTTP 302 → 200 with octet-stream. One-byte read-only Range probes for
each of the eight existing draft upload MIME families returned octet-stream;
partial HTTP 206 responses are **not** accepted by the controller.

`tools/probe_macos_downloads.py` is a separate, opt-in live regression, not part
of the deterministic gate. With existing gh authentication, explicit selection
and a fresh private output directory, it verifies the live source tag, original
receipt hash, transport and payload hashes, run/controller provenance, all four
same-source fragment downloads, and the Intel draft updater signature using
the exact frozen source key:

```sh
python3 tools/probe_macos_downloads.py \
  --repository tigy32/Tyde --tag v0.9.5 \
  --source-sha 83ad2d3c03b4c2661adb22ad435dd073a9530bab \
  --producer-sha 25038b19a98fa2cc3a13d85f76c6eef81928746c \
  --run-id 39260355030 --artifact-id 12194400987 \
  --fragments-run-id 39193235317 --target aarch64-apple-darwin \
  --receipt-sha256 2877cd96c3df2164d8f06ac8a5bcb440fdc31160904cf0f793af779ef9a8e3a2 \
  --output /absolute/path/to/new-private-evidence
```

For the red regression, supply `--controller-file` pointing to the pre-fix
`macos_release.py` saved from Git; it fails at GitHub with HTTP 415, not a
fixture assertion. The corrected probe passes with the original receipt and
Accepted statuses unchanged. It executes no recovery context, package, Apple,
upload, deployment, or publication operation. In particular it cannot attest
macOS Gatekeeper or future 22-asset finalization, and does not bypass the
unlanded controller's recovery main-ancestry guard. Deterministic CLI/HTTP flows
separately exercise status/media/provenance/hash rejection and both direct and
redirected Release responses; they are not proof of GitHub behavior.
