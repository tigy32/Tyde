# Mobile web release automation

The mobile web/PWA bundle is release-owned infrastructure. A desktop/server
release must not publish without a matching `/tyde/v<release>/` mobile web
bundle built from the same source protocol.

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
- Deploys use `web/deploy/deploy.sh --confirm --live-manifest-base`, which fetches
  the live manifest before generation and again immediately before the final
  manifest upload. This preserves newer published entries when an older tag is
  backfilled and fails closed if the live manifest cannot be read or parsed.
- The versioned bundle is uploaded and verified before `manifest.json` is
  published. Loader shell sync explicitly excludes `manifest.json`; the manifest
  upload is last so production never advertises a bundle before its files exist.
- Generated manifests raise `minSupported` to the first entry that carries
  `protocolVersion` when older entries lack the field. That keeps older
  non-stamped entries in history without advertising them as bootable to a loader
  that requires protocol metadata.

## AWS requirements

GitHub Actions uses AWS OIDC. Configure `AWS_TYDE_WEB_DEPLOY_ROLE_ARN` as a
repository secret or variable. The role should grant only:

- `s3:ListBucket` on `tycode-static`, scoped to the `tyde/` prefix.
- `s3:GetObject` and `s3:PutObject` on `arn:aws:s3:::tycode-static/tyde/*`.
- `cloudfront:CreateInvalidation` for distribution `E3JJ1OF4I8TP6U`.

Do not grant delete permissions. The deploy script also refuses `--delete` and
asserts every S3 destination stays under `s3://tycode-static/tyde/`.

## Local checks

Use the canonical local guard before release builds:

```sh
tools/release_check.sh [v<release>]
```

It validates build/version/protocol/mobile-web coherence by generating a
temporary manifest entry for the checked version instead of requiring
`web/loader/manifest.json` to already contain the release. It also prints the
native-mobile drift reminder: installed native apps are bundled and must be
rebuilt/reinstalled after protocol changes. This guard does not replace the
AGENTS release steps for a clean tree, `main`, tag checks, approval, or pushes.

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

## Backfilling a release

To remediate a missed release without deploying from a dirty tree, run the
GitHub Actions workflow **Mobile Web Release Deploy** from the default branch
with:

```text
version = v0.8.19
confirm = true
```

Use the exact target tag, for example `v0.8.19` or `v0.8.19-beta.9`. The
workflow checks out that tag as the release source, verifies that tag is
contained in the default branch, runs current deploy tooling from the default
branch, builds the tag's `mobile-frontend`, stamps the tag's Rust
`PROTOCOL_VERSION`, merges the live manifest, publishes only under `tyde/`,
invalidates CloudFront, and verifies the deployed target entry.

This backfill fixes fresh loader-routed pairing for that release. It cannot
inject newer app-dispatched repair behavior into an already-running stale PWA
bundle; a host handshake `Reject` by itself only surfaces as a connection error
in those historical bundles. Users already stuck inside that stale bundle may
need to close/reopen the PWA, reload or clear site data, or open the target
release QR through the loader so the loader can choose the matching
`/tyde/v<release>/` bundle.
Protocol-stamped bundles can use the loader repair path when the app explicitly
dispatches `tyde:repair-needed`, such as after in-app QR/protocol validation
detects drift.
