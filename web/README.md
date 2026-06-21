# Tyde web — loader shell + PWA packaging

This directory holds the **browser-served** artifacts for the Tyde mobile→PWA
conversion (design: `docs/plans/mobile-web-pwa.md`). It is intentionally
separate from the Rust crates and from `mobile-frontend/` (the Leptos/WASM app
that compiles into the *versioned* bundles).

## Why `web/loader/`

- The loader is **not** Rust and **not** versioned — it is a tiny, stable,
  hand-written HTML+JS shell that must change as rarely as possible (every byte
  is permanent, un-versioned attack surface). Keeping it out of the Cargo
  workspace makes that separation obvious and keeps `cargo`/`trunk` from trying
  to build it.
- `web/` is the namespace for everything published to the static origin
  (`https://tycode.dev/tyde/`). Phase 6 deploy tooling will also live under
  `web/` (e.g. `web/deploy/`), so the loader sits beside, not inside, it.
- `web/loader/` maps 1:1 onto what is served at the origin **root** (`/tyde/`).
  The immutable per-version app bundles are published *next to it* at
  `/tyde/v<version>/` by the Phase 6 release pipeline (built from
  `mobile-frontend` via `trunk`), so the deployed tree looks like:

  ```
  /tyde/                      <- web/loader/ (this dir: stable shell)
  /tyde/manifest.json         <- allowlist authority (server-controlled)
  /tyde/v0.8.19-beta.2/...    <- immutable app bundle (from mobile-frontend)
  /tyde/v0.8.19/...           <- immutable app bundle
  ```

## What's here

| File | Role |
| --- | --- |
| `index.html` | Loader shell. CSP, iOS add-to-home meta, PWA manifest link, the four UI views. |
| `loader.js` | The only DOM/network module: QR scan + paste, orchestration, SRI boot, SW registration, returning-user fast path, self-heal listener. |
| `pairing.js` | Pure: base64url decode, `tyde-pair://v1?` parse, `release_version` validation (mirror of `TydeReleaseVersion`). |
| `cbor.js` | Pure: tiny self-contained CBOR reader (definite-length subset ciborium emits) with depth/item/byte DoS caps. |
| `pairing.js` | Pure: base64url decode, `tyde-pair://v1?` parse, `release_version` validation (mirror of `TydeReleaseVersion`), URI/version length caps, Rust-matching whitespace trim. |
| `manifest-policy.js` | Pure: manifest allowlist lookup, fail-closed `minSupported`/`blocked` policy, semver-lite compare, per-artifact SRI/path sanity. |
| `integrity.js` | Pure (injectable): SRI-verifies EVERY executable artifact (entry JS + wasm + chunks) via WebCrypto before boot; caches only verified bytes. |
| `manifest.json` | **Example** allowlist (the authority). Phase 6 regenerates it. |
| `manifest.webmanifest` | PWA manifest (name, icons, `display: standalone`, `start_url`). |
| `sw.js` | Service worker: network-first shell, **network-only manifest**, cache-first immutable bundles (cache populated only by the page after SRI-verify). |
| `loader.css` | Loader styling (external so the shell needs no inline `<style>`). |
| `icons/` | `icon.svg` + a README on the PNG sizes still needed for iOS. |
| `test/` | `node --test` unit tests + fixtures (real host URIs + synthetic abuse cases). |

## Run / test locally (no build step)

The loader has **no build step** — it is plain ES modules served as files.

```sh
cd web/loader
node --test          # unit tests, no deps
npm run serve        # python3 -m http.server 8088, then open http://127.0.0.1:8088/
```

## Security model (as implemented)

The threat is a malicious/forged pairing QR (or a compromised host) steering the
loader into running attacker code or a known-bad client. Defenses, in order:

1. **Never interpolate QR text into a URL or HTML.** The loader extracts only
   `release_version` from the QR and uses it solely as a *lookup key*. The URLs
   and SRI hashes that actually get used come from the **manifest entry**, never
   from string concatenation with QR input.
2. **Validate `release_version`** with the exact rules of Rust's
   `host-config::TydeReleaseVersion` (`validate_release_version`): numeric
   `major.minor.patch` core, optional `[0-9A-Za-z-]` prerelease, and rejection
   of empty / whitespace / `/` / `\` / `..`-bearing input. Trimming uses Rust's
   Unicode White_Space set (which excludes U+FEFF), so the loader is never
   *looser* than the authoritative host parser. A `release_version` length cap
   (256), URI length cap, and CBOR depth/item/byte caps bound parser DoS.
   (`pairing.js`, `cbor.js`.)
3. **Manifest allowlist is the authority, and fails closed.** Only a version in
   `manifest.json.versions` may boot. A `minSupported` floor and an explicit
   `blocked` list (normalized through the same validator) refuse a
   downgraded/known-bad version even if an old host advertises it. A manifest
   with malformed policy fields (non-array `blocked`, present-but-invalid
   `minSupported`) is rejected wholesale rather than silently degraded — a
   corrupt/partial manifest can never *widen* what is allowed. (`manifest-policy.js`.)
4. **Subresource Integrity over EVERY executable artifact.** A `<script
   integrity>` only covers the entry JS; the entry then fetches its `.wasm`
   (and any code-split chunks) on its own. So the manifest entry carries an
   `artifacts` map of `{ path: integrity }` for the wasm + chunks, and the
   loader SRI-verifies **all** of them (entry JS included) with WebCrypto
   *before* the bundle runs (`integrity.js`). Only verified bytes are written to
   the bundle cache, so the subsequent `<script>`/wasm load reads exactly those
   bytes. A tampered same-version `.wasm` is therefore rejected — the gap an
   entry-only integrity left open. On any failure the stored version is
   forgotten, the cached artifacts are purged, and the user falls back to
   pairing.
5. **Strict CSP.** `script-src 'self' 'wasm-unsafe-eval'` — the load-bearing
   directive: no third-party JS, but the Leptos/Trunk bundle's
   `WebAssembly.instantiate` is allowed (we do NOT grant general
   `'unsafe-eval'`). Plus `object-src 'none'`, `base-uri 'none'`,
   `default-src 'self'`, and a narrow `connect-src 'self' wss:` (the app reaches
   the broker over wss and fetches same-origin; `https:` is intentionally
   omitted). Set via `<meta>` for local use; **Phase 6 must also send it (and
   `frame-ancestors 'none'`, which `<meta>` ignores) as an HTTP response
   header.**

This is the loader hardening the design doc calls "load-bearing" for the PSK
storage threat model: the at-rest non-extractable `CryptoKey` is **not**
XSS-proof, so keeping third-party/injected JS off the origin is what protects
it. `style-src` allows inline styles because the injected Leptos app styles
itself inline; inline styles are not a script-execution vector.

## Pairing handoff (loader → app)

On the pair path the loader stashes the **full raw `tyde-pair://…` URI** in
`sessionStorage` under `tyde.pair.uri` (NOT the URL — the URI carries the PSK,
so it must never enter history/referrer), then boots the bundle. The booted
WASM app consumes it at startup via
`mobile-frontend`'s `bridge::web::take_pending_pairing_uri` (key kept in sync
as `PENDING_PAIRING_URI_KEY`), runs the **authoritative** parse, and lands the
user on the pairing Confirm screen so one tap finishes first-time pairing. The
loader makes no pairing decision from the stash beyond `release_version`; a
forged/stale stash is rejected by the app's parse and cleared on read.

## Service worker & revocation

- **Loader shell:** network-first, cache fallback (offline UI).
- **`manifest.json`:** NETWORK-ONLY — never cached, never served stale. It is
  the revocation authority, so a forced outage fails closed (the loader can't
  boot) rather than letting a stale allowlist defeat a `blocked`/`minSupported`
  revocation.
- **Versioned bundles `/tyde/v…/`:** cache-first *read*, but the SW never writes
  them on a plain fetch — only the loader page writes, and only after SRI
  verification. So a tampered 200 is never persisted (no cache-before-verify),
  and an SRI/load failure purges the cached artifacts to avoid a permanent
  SRI-fail wedge. Once verified, the immutable bundle serves from cache for fast
  relaunch.

## Returning users & self-healing

- On a successful pairing the validated version is stored in `localStorage`
  (`tyde.loader.version`). On next launch the loader boots it directly — **no
  QR** — *after* re-checking it against a freshly fetched manifest.
- If that version is gone from the manifest (or now blocked / below
  `minSupported`), the fast path **fails closed**: the stored version is
  forgotten and the loader shows the pair/re-pair flow.
- A host upgrade self-heals: the upgraded host rejects the stale client at the
  handshake (`Reject` on protocol mismatch). The WASM app surfaces that by
  dispatching `window` event `tyde:repair-needed`; the loader listens for it,
  forgets the stored version, and returns to pairing so the user re-scans the
  new QR and boots the matching bundle.

## Phase 6 — deploy (`web/deploy/`)

Phase 6 tooling lives in `web/deploy/`:

| File | Role |
| --- | --- |
| `deploy.sh` | One-command deploy: build bundle → generate manifest → sync loader + versioned bundle → invalidate. **Dry-run by default.** |
| `generate-manifest.mjs` | Node, no deps. Hashes every executable artifact of a built `dist/` and merges a version record into `manifest.json` (additive). |
| `cloudfront-setup.md` | One-time manual CloudFront setup: a `tyde/*` cache behavior + a CSP/HSTS/CORS `ResponseHeadersPolicy`. Owner runs it once. |

### Where it publishes (ground truth)

```
tycode.dev → CloudFront E3JJ1OF4I8TP6U → S3 bucket tycode-static (us-west-2)
our prefix → s3://tycode-static/tyde/         (loader shell, additive)
           → s3://tycode-static/tyde/v<ver>/  (immutable app bundle)
```

`tycode-static` **already serves the live marketing site** at the bucket root
(`index.html`, `blog.html`, `posts/…`, …). The deploy only ever writes under
`tyde/`.

### Runbook

```sh
# 0. One time only: add the tyde/* CloudFront behavior + security headers.
#    Follow web/deploy/cloudfront-setup.md (owner runs it; not automated).

# 1. ALWAYS dry-run first (default). Builds nothing, mutates nothing — it runs
#    `aws s3 sync --dryrun` so you can confirm every write is under tyde/ with
#    no deletes, and SKIPS the CloudFront invalidation.
web/deploy/deploy.sh

# 2. Real deploy. Version defaults to tools/check_release_version.py; pass one
#    explicitly only if it matches the repo's canonical version.
web/deploy/deploy.sh --confirm
web/deploy/deploy.sh 0.8.19-beta.2 --confirm
```

`deploy.sh --confirm` then:

1. `trunk build --release --public-url /tyde/v<ver>/` of `mobile-frontend` →
   its `dist/` (Trunk emits hash-stamped filenames).
2. `generate-manifest.mjs` computes **sha384** SRI for **every** executable
   artifact — entry `.js`, the `.wasm`, and any chunks/snippets — and merges a
   `{ path, entry, integrity, artifacts: { "<path>": "<sri>" } }` record into
   `web/loader/manifest.json` (preserving other versions, `minSupported`,
   `blocked`). The loader rejects the boot if ANY listed artifact's bytes don't
   match, so the generator enumerates them all. Equivalent per-artifact hash:

   ```sh
   echo "sha384-$(openssl dgst -sha384 -binary <artifact> | openssl base64 -A)"
   ```

3. Syncs `web/loader/` → `s3://tycode-static/tyde/` with **short cache**
   (`max-age=60`) so loader logic + revocations propagate, then re-stamps
   `manifest.json` as **`no-store`** (it is the revocation authority and must
   never be cached).
4. Syncs `dist/` → `s3://tycode-static/tyde/v<ver>/` with
   `Cache-Control: public,max-age=31536000,immutable`, then fixes the `.wasm`
   `Content-Type` to `application/wasm` (S3 would otherwise mislabel it and
   break `WebAssembly.instantiateStreaming`).
5. `aws cloudfront create-invalidation --distribution-id E3JJ1OF4I8TP6U
   --paths '/tyde/*'`.

### Guardrails (enforced by `deploy.sh`)

- **Dry-run by default.** A bare invocation never touches prod; the real deploy
  requires `--confirm`.
- **Never `--delete`.** The sync is strictly additive; `--delete` is refused as
  an input and never passed to `aws`.
- **Scoped to `tyde/` only.** Every S3 destination is built from the bucket +
  `tyde` prefix and asserted to live under `s3://tycode-static/tyde/` before any
  write. The marketing keys at the bucket root, the `tycode.dev` bucket beyond
  `tyde/`, and `tyggs.*` are **never** touched.
- **Version validated.** The version must pass the host's release-version rules
  and is cross-checked against `tools/check_release_version.py`.
- **CSP only on `tyde/*`.** The security `ResponseHeadersPolicy` attaches solely
  to the `tyde/*` behavior — never the default, which would break the marketing
  pages (see `cloudfront-setup.md`).

### Still needs a manual step

- The CSP (incl. `frame-ancestors 'none'`) + HSTS + CORS are sent as **HTTP
  headers** via the CloudFront `ResponseHeadersPolicy` in `cloudfront-setup.md`
  (one-time).
- Rasterize `icons/icon.svg` to the PNG sizes listed in `icons/README.md`.

## Verifiable locally vs. needs a device

- **Locally:** unit tests (CBOR parse + DoS caps, version validation incl.
  injection/whitespace/length, manifest gating + fail-closed policy, per-artifact
  SRI incl. a tampered-wasm/chunk case, the loader→app URI handoff, returning-user
  resolution), JSON/webmanifest well-formedness, and the shell serving over HTTP.
  Fixtures include **real** URIs emitted by the Rust `MobilePairingQrPayload`.
- **Needs a device/browser:** camera QR scan (`BarcodeDetector` + getUserMedia),
  iOS "Add to Home Screen" + standalone display, service-worker offline launch,
  native SRI/CSP enforcement against a real served bundle, and the end-to-end
  loader→app pairing handoff in a real Safari/Chrome. These depend on the
  Phase 6 deployed origin and on-device behavior.
