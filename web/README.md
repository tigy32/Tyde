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
| `cbor.js` | Pure: tiny self-contained CBOR reader (definite-length subset ciborium emits). |
| `manifest-policy.js` | Pure: manifest allowlist lookup, `minSupported`/`blocked` policy, semver-lite compare, SRI/path sanity. |
| `manifest.json` | **Example** allowlist (the authority). Phase 6 regenerates it. |
| `manifest.webmanifest` | PWA manifest (name, icons, `display: standalone`, `start_url`). |
| `sw.js` | Service worker: network-first shell/manifest, cache-first immutable bundles. |
| `loader.css` | Loader styling (external so the shell needs no inline `<style>`). |
| `icons/` | `icon.svg` + a README on the PNG sizes still needed for iOS. |
| `test/` | `node --test` unit tests + fixtures (real host URIs + synthetic abuse cases). |

## Run / test locally (no build step)

The loader has **no build step** — it is plain ES modules served as files.

```sh
cd web/loader
node --test          # unit tests (16 tests, no deps)
npm run serve        # python3 -m http.server 8088, then open http://127.0.0.1:8088/
```

## Security model (as implemented)

The threat is a malicious/forged pairing QR (or a compromised host) steering the
loader into running attacker code or a known-bad client. Defenses, in order:

1. **Never interpolate QR text into a URL or HTML.** The loader extracts only
   `release_version` from the QR and uses it solely as a *lookup key*. The URL
   and SRI hash that actually get used come from the **manifest entry**, never
   from string concatenation with QR input.
2. **Validate `release_version`** with the exact rules of Rust's
   `host-config::TydeReleaseVersion` (`validate_release_version`): numeric
   `major.minor.patch` core, optional `[0-9A-Za-z-]` prerelease, and rejection
   of empty / whitespace / `/` / `\` / `..`-bearing input. (`pairing.js`.)
3. **Manifest allowlist is the authority.** Only a version present in
   `manifest.json.versions` may boot. A `minSupported` floor and an explicit
   `blocked` list let the server refuse a downgraded/known-bad version even if
   an old host advertises it. (`manifest-policy.js`.)
4. **Subresource Integrity.** The version's entry module is injected with the
   `integrity` digest from the manifest and `crossorigin="anonymous"`, so a
   tampered bundle (e.g. a compromised CDN object) fails to execute. On an SRI
   failure the stored version is forgotten and the user falls back to pairing.
5. **Strict CSP.** `script-src 'self'` (the load-bearing directive — no
   third-party JS) plus `object-src 'none'`, `base-uri 'none'`,
   `default-src 'self'`, and `connect-src 'self' https: wss:` so the WASM app
   can still reach the broker. This CSP is set via `<meta>` for local use;
   **Phase 6 must also send it (and `frame-ancestors 'none'`, which `<meta>`
   ignores) as an HTTP response header.**

This is the loader hardening the design doc calls "load-bearing" for the PSK
storage threat model: the at-rest non-extractable `CryptoKey` is **not**
XSS-proof, so keeping third-party/injected JS off the origin is what protects
it. `style-src` allows inline styles because the injected Leptos app styles
itself inline; inline styles are not a script-execution vector.

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

## Phase 6 (deploy — OUT OF SCOPE here) will:

1. Build each release's `mobile-frontend` bundle with `trunk build --release`
   (with `wasm-opt` installed, per Phase 0) into `/tyde/v<version>/`. Trunk
   emits hash-stamped filenames; the entry module path is the value that goes
   into the manifest's `entry`.
2. **Generate `manifest.json`** by computing the SRI digest of each version's
   entry module — e.g.:

   ```sh
   echo "sha384-$(openssl dgst -sha384 -binary tyde-mobile.js | openssl base64 -A)"
   ```

   then merging the new `{ path, entry, integrity }` record into `versions`,
   and setting `minSupported`/`blocked` per release policy. The manifest is the
   *server-controlled* allowlist — it is published by the pipeline, not by any
   host.
3. Publish `web/loader/` to the origin root (`/tyde/`) and sync the versioned
   bundles to `/tyde/v<version>/` (S3 + CloudFront, or equivalent). Set
   `Cache-Control: immutable` on `/tyde/v<version>/*` and short/no-cache on the
   loader shell + `manifest.json`.
4. Send the CSP (incl. `frame-ancestors 'none'`) and HSTS as **HTTP headers**.
5. Rasterize `icons/icon.svg` to the PNG sizes listed in `icons/README.md`.

## Verifiable locally vs. needs a device

- **Locally (done):** unit tests (CBOR parse, version validation, injection
  rejection, manifest gating, returning-user resolution), JSON/webmanifest
  well-formedness, the shell serving over HTTP, and an `init()` smoke run of the
  returning-user boot path. Fixtures include **real** URIs emitted by the Rust
  `MobilePairingQrPayload`.
- **Needs a device/browser:** camera QR scan (`BarcodeDetector` + getUserMedia),
  iOS "Add to Home Screen" + standalone display, service-worker offline launch,
  and SRI enforcement against a real served bundle. These depend on the Phase 6
  deployed origin and on-device Safari/Chrome behavior.
