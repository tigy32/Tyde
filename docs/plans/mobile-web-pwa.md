# Plan: Tyde mobile client as a Web/PWA with versioned bundles

> Status: design (no code yet). Product of investigation + three independent
> agents (two planners cross-reviewed + a dedicated PSK-storage investigator,
> Codex-reviewed). Owner decisions locked: **mobile-only scope**;
> **investigate hardened PSK storage first** (resolved below).

## Context

Tyde ships a native iOS mobile client today. Shipping through the App Store
couples client releases to Apple review and forces client and host to upgrade
in lockstep. Goal: **stop shipping a native mobile app** and serve the Tyde
mobile client as a **website / PWA** ("Add to Home Screen"). A thin, stable
loader page learns the paired host's release version and boots the matching
**versioned static bundle** (e.g. `tycode.dev/tyde/v0.8.19-beta.2/...`), so the
client protocol always matches the host. This decouples client releases from
App Store review.

## Why this is feasible (key findings)

- **The host transport is already a public cloud relay, not LAN/localhost.**
  Mobile connects via MQTT 5 to `mqtts://broker.emqx.io:8883`
  (`protocol/src/types.rs:16`, `mqtt-transport/src/types.rs:77-83`). Both host
  and client connect *outbound* to the broker. So the classic PWA blocker —
  "an HTTPS page can't reach a plaintext host on the LAN" — **does not apply**.
  An HTTPS PWA -> `wss://` broker -> host path is clean (no mixed content).
- **The app stack above the transport is transport-agnostic.**
  `mqtt_transport::connect_ephemeral` returns an `EnvelopeStream`
  (`mqtt-transport/src/stream.rs`), just an async byte stream; the Tyde
  handshake `client::connect_parts` is generic over `AsyncRead + AsyncWrite`
  (`client/src/lib.rs:~1449`). Nothing above the transport knows it is MQTT.
- **Crypto/framing/rendezvous/session layers are pure Rust** (ChaCha20Poly1305
  + HKDF-SHA256 in `mqtt-transport/src/session.rs`; `framing.rs`,
  `rendezvous.rs`, `types.rs`). WASM-portable.
- **Only the I/O driver is not WASM-portable.** `mqtt-transport/src/client.rs`
  uses `rumqttc` + `tokio` + `tokio-rustls`; none compile to
  `wasm32-unknown-unknown`. This `MqttActor` event loop is the single piece
  that needs a second (browser) implementation.
- **Version is already exchanged**, but with the wrong type for bundle
  selection (below): `HelloPayload`/`WelcomePayload`/`RejectPayload`
  (`protocol/src/types.rs:780-790,1309`) and `MobilePairingQrPayload`
  (`mqtt-transport/src/types.rs:105`) carry `protocol_version: u32` +
  `tyde_version: Version`.

## The version-key correction

`protocol::Version` (`protocol/src/types.rs:18-23`) is `major/minor/patch: u32`
and **cannot represent prereleases**; `TYDE_VERSION` is stale (`0.8.14`) vs the
shipped `0.8.19-beta.2` (`package.json:4`). So `tyde_version` is **not** a valid
key for selecting a static bundle. Reuse the existing, prerelease-capable,
traversal-safe type: **`host-config::TydeReleaseVersion`**
(`host-config/src/lib.rs:35`). It parses `0.8.20-beta.1`, exposes
`github_tag()`, and **already rejects `../0.8.19` and malformed input**
(`host-config/src/lib.rs:362-364`) — doubling as the loader's path-injection
guard.

## Recommended approach

A **thin stable loader shell + per-version static bundles**, plus a **browser
MQTT-over-WebSocket transport** added to `mqtt-transport` behind a transport
trait so the pure protocol logic is shared with the native build.

Locked decisions:
- **Scope:** mobile client -> PWA only; desktop stays a native Tauri host.
- **Broker:** move host + web client to a shared `wss://` endpoint
  (e.g. `wss://broker.emqx.io:8084/mqtt`). Native host already supports `wss`
  (`client.rs:1447`, `wss_options`), so no dual-URL QR and no `MOBILE_QR_VERSION`
  bump.
- **PSK at rest:** store as a **non-extractable WebCrypto HKDF `CryptoKey`** in
  IndexedDB (see PSK section) — stronger than plaintext IndexedDB, with a
  documented fallback.

## Critical files

- `mqtt-transport/src/client.rs` — split into transport-agnostic protocol driver
  + native `rumqttc` backend; add a WASM `web-sys::WebSocket` backend.
- `mqtt-transport/src/stream.rs` — `EnvelopeStream`; needs a WASM-compatible I/O
  surface (`tokio::io` impls are the only platform tie).
- `mqtt-transport/src/types.rs` — `MobilePairingQrPayload`, `validate_broker_url`,
  `MqttTransportPolicy`; the `tyde-pair://v1?` URI is the stable loader contract.
  `PreSharedKey` lives at `types.rs:368`.
- `protocol/src/types.rs` — `DEFAULT_MOBILE_MQTT_BROKER_URL`,
  `Hello/Welcome/Reject` payloads (add `release_version`).
- `host-config/src/lib.rs` — reuse `TydeReleaseVersion` as the bundle key.
- `mobile-frontend/src/bridge.rs` — the `window.__TAURI__` bridge; in the web
  build, transport/storage/QR go direct-to-WASM instead of through Tauri.
- `mobile/src-tauri/src/{mqtt_connection.rs,psk_store.rs,paired_hosts.rs}` —
  reference behavior to port into WASM (connection manager, PSK store, host
  store).

## Protocol / wire changes

1. Add `release_version: Option<TydeReleaseVersion>` to `WelcomePayload`
   (`types.rs:788`), `RejectPayload` (`types.rs:1309`), and
   `MobilePairingQrPayload` (`mqtt-transport/types.rs:105`). `Option` keeps
   backward compatibility; leave `protocol_version`/`tyde_version` as-is so the
   exact-match handshake gate in `server/src/acceptor.rs` is unchanged.
2. Populate `release_version` on the host from the real build version (the
   `0.8.19-beta.2` source used by packaging), not `TYDE_VERSION`.
3. Switch `DEFAULT_MOBILE_MQTT_BROKER_URL` to the shared `wss` endpoint and
   update host QR generation (`mobile/src-tauri/src/lib.rs`,
   `server/src/mobile_access.rs`) to publish a wss-reachable broker. Update the
   test `default_broker_endpoint_is_emqx_mqtts` (`mqtt-transport/types.rs:500`).
4. WASM backend accepts only `wss://`; on a stored `mqtts://` host record it
   surfaces "re-pair needed" rather than failing opaquely.

## Transport refactor seam

Introduce an `MqttLink` trait inside `mqtt-transport` with a transport-neutral
`LinkEvent` enum (Publish / Disconnect), so the core never names `rumqttc`
types:

- **Reusable (shared `protocol_driver` module):** `establish_session`,
  salt-ordering, `boxcar_outbound`/`BoxcarBatch`/`append_or_defer`,
  `decode_publish`, `defer_data_frame`/`flush_pending_data_frames`/
  `handle_data_frame`, rendezvous decision logic, `PublishPacer`/
  `PublishRetryBackoff` (re-timed), all `validate_*` helpers
  (`client.rs:528-1268,1533-1561`).
- **Per-platform backends:** native = `AsyncClient`+`EventLoop`+TLS; WASM =
  `web-sys::WebSocket` + the **standalone `mqttbytes 0.6.0` codec crate**.
  NOTE: do **not** try to reuse rumqttc's own `v5::mqttbytes` module on wasm — it
  does not compile to `wasm32-unknown-unknown` (the rumqttc crate pulls
  tokio/mio/native sockets). Phase-0 verified the standalone crate compiles and
  round-trips; the native and wasm codecs are therefore separate crates with
  slightly different APIs (`Publish::new` arity, no props tuple on the standalone
  `Packet::Publish`), so the wasm packet matching is a *near*-, not byte-,
  verbatim port of the native arms.
- Replace `tokio::time`/`tokio::select!`/`tokio::spawn` with wasm-friendly
  equivalents (`gloo-timers`/`web-time`, `futures` channels) in the WASM build.
- Enable `getrandom 0.3` `wasm_js` backend for RNG in
  `rendezvous.rs`/`reconnect.rs`.

## PSK storage (hardened — owner-requested investigation, Codex-reviewed)

**Decision: store the long-term PSK as a non-extractable WebCrypto HKDF
`CryptoKey` in IndexedDB.** Meaningfully stronger than plaintext IndexedDB; the
root secret never exists as readable bytes at rest.

Why it works: every at-rest use of the long-term PSK is HKDF-SHA256
extract-then-expand to 32 bytes —
`derive_session_key` (`session.rs:153`), `derive_rendezvous_key`
(`rendezvous.rs:237`), `derive_ephemeral_psk` (`rendezvous.rs:136`). The only
non-HKDF use, `credential_fingerprint` (`paired_hosts.rs:549`, plain SHA-256
over broker+room+psk), runs only at pairing time (`lib.rs:156`) while the raw
bytes are still in hand — so it does not force the root to be readable at rest.

Design:
- At pairing: compute `credential_fingerprint` from the raw bytes first, then
  `crypto.subtle.importKey("raw", pskBytes, "HKDF", /*extractable=*/false,
  ["deriveBits"])`, `put` the resulting `CryptoKey` into IndexedDB keyed by host
  id, then drop the raw bytes.
- Thereafter derive sub-keys via
  `crypto.subtle.deriveBits({name:"HKDF",hash:"SHA-256",salt,info}, baseKey, 256)`.
  Salt ordering and info strings are passed verbatim to match the Rust code
  (`b"tyde-mqtt-v1"`, `b"tyde-mqtt-rendezvous-v1"`,
  `b"tyde-mqtt-ephemeral-data-v1"`).
- **Ephemeral-PSK chain** (root PSK -> HKDF -> ephemeral PSK -> HKDF -> session
  key): derive the ephemeral PSK bytes with `deriveBits`, then either re-import
  them as a temporary non-extractable HKDF key for the second HKDF, or run the
  second HKDF in Rust on the (already-exposed) ephemeral bytes. **Do not collapse
  the two HKDFs** — that changes the protocol. Only the root stays sealed.
- **ChaCha20Poly1305 is not a WebCrypto algorithm**, so the AEAD stays in
  Rust/WASM; derived 32-byte session keys are deliberately readable output handed
  to the WASM cipher. That is fine — they are not the protected secret.

Code that changes (crypto layer): introduce a `KeyDeriver`/PSK abstraction that,
on `wasm32`, holds a `CryptoKey` handle and exposes
`async hkdf_expand(salt, info) -> [u8;32]` via `web-sys`
(`crypto.subtle.deriveBits`); on native it stays the current in-memory
`PreSharedKey` over RustCrypto `hkdf`. The three HKDF call sites
(`session.rs:153`, `rendezvous.rs:237`, `rendezvous.rs:136`) route through it.
Derivation becomes **async on wasm** — contained to connection/handshake setup
(`client.rs:267,348` + rendezvous encode/decode); **keep the AEAD/stream path
synchronous** after the session key exists. The PWA pairing/persistence layer
replaces `psk_store.rs` + the iOS Keychain branch in `paired_hosts.rs`;
`PairedHostRecord` already stores no PSK material (only a key id + fingerprint,
`paired_hosts.rs:36-45`), so its `psk_keychain_key_id` just repoints at the
IndexedDB `CryptoKey` entry.

**Honest limits (must be stated to users/threat model):**
- Non-extractable blocks `exportKey`/`wrapKey`, **not authorized `deriveBits`**.
  This defends against passive IndexedDB dumps / at-rest exposure, but **NOT
  active same-origin XSS**, which can use the key as a derivation oracle. This is
  **stronger-than-plaintext, but not Keychain-equivalent and not XSS-proof** —
  which is why loader CSP / SRI / no-third-party-JS hardening is load-bearing.
- iOS Home-Screen PWA persistence of a stored `CryptoKey` is expected (the 7-day
  ITP cap is documented as not applying to installed web apps) but is
  operational behavior, not a cryptographic guarantee — **test on target iOS
  versions**. `navigator.storage.persist()` has limited Safari support —
  feature-detect; do not rely on it.
- Treat key loss as recoverable: re-scan the QR to re-pair.

**Fallback if infeasible** (target iOS won't persist a non-extractable key, or
the async-HKDF refactor is deemed too invasive for v1): store the raw 32-byte
PSK in IndexedDB (inside a `persist()`-ed bucket if granted). **What is lost:**
the long-term key becomes recoverable by anything that can read the object store
(same-origin XSS, malicious extension, forensic profile dump). Pair with a clear
re-pair-on-loss flow. **Stronger upgrade path (optional, later):** wrap the PSK
with a key derived from a user passphrase (PBKDF2/Argon2) — defends against
offline device theft, at the cost of a passphrase prompt on cold start;
orthogonal to and layerable on the non-extractable-key approach.

## Phased implementation

**Phase 0 — de-risk (parallel, gating). DONE — results inline below.**
- Codec spike **RESOLVED**: `rumqttc 0.25.1` does **not** compile to
  `wasm32-unknown-unknown` even with `default-features=false` — it pulls tokio →
  `mio`/`socket2` (native sockets) unconditionally, so `v5::mqttbytes` is
  unreachable on wasm. **Use the standalone `mqttbytes 0.6.0` crate** (rumqttc's
  codec extracted; depends only on `bytes`): it compiles to wasm32 (debug +
  release) and round-trips a v5 PUBLISH. This is the WASM codec for the Phase-2
  backend. (Fallback ladder rungs `mqtt-protocol` / MQTT.js were not needed.)
- Bundle size **MEASURED**: real `trunk build --release` of `mobile-frontend` →
  raw `.wasm` 8.0 MB, **gzip 1.64 MB** (JS glue 8 KB gz). Comfortably in range;
  **code-splitting is not required for v1.** Caveat: this figure is
  **pre-wasm-opt** — `wasm-opt`/binaryen is **not currently installed**, so trunk
  skipped optimization and the wasm still carries rustc debug paths. **Install
  `wasm-opt`/binaryen in the build + CI** and re-measure; `-Oz` should reduce it
  further (~1.0–1.3 MB gz, estimated).
- Spike: WebCrypto non-extractable HKDF `CryptoKey` import + `deriveBits` +
  IndexedDB structured-clone persistence on the target iOS Safari versions
  (validates the PSK storage decision before the async refactor).
- Confirm broker wss reachability (`wss://broker.emqx.io:8084/mqtt`).

**Phase 1 — transport refactor. DONE (native-only, no behavior change).**
Introduced `MqttLink`/`LinkEvent` (`mqtt-transport/src/link.rs`); moved the
reusable protocol logic (session establishment, salt ordering, boxcar batching,
rendezvous, publish pacing/retry, deferred-frame handling, validators) into a
generic `ProtocolDriver<L: MqttLink>` (`protocol_driver.rs`); wrapped the
existing rumqttc `AsyncClient`+`EventLoop`+TLS as `NativeMqttLink`
(`link_native.rs`, the only module that names rumqttc). `client.rs` is now a thin
native entry point. Library-specific PUBACK/SUBACK reason-code interpretation
stays in the native backend (it translates packets into already-validated neutral
`LinkEvent`s), keeping the driver free of rumqttc types. Native `cargo build` +
the full `mqtt-transport` test suite stay green.

**MQTT transport v3 — receiver-credit data pipelining. DONE.**
Data frames now pipeline within authenticated cumulative receiver credit:
`DATA_CREDIT_WINDOW = 16`, `MAX_DATA_QOS1_INFLIGHT = 16`, and the broker-facing
MQTT window remains 32 for headroom. Standalone credit frames use transport
version `0x02`, tag `0x05`, independent AEAD nonce directions, and a separate
control counter. Broker PUBACK frees broker in-flight capacity and completes the
local write ack, but never advances Tyde receiver credit; beyond-window data
counter gaps remain fatal.

**Phase 2 — WASM transport backend.** Implement `link_wasm.rs` over
`web-sys::WebSocket` + the Phase-0 codec; give `EnvelopeStream` a WASM I/O
surface; run the pure crypto/handshake in a headless-wasm test against a wss
broker.

**Phase 3 — web client app shell.** Replace `mobile-frontend`'s Tauri bridge
with direct-WASM equivalents: connection manager (port `mqtt_connection.rs`),
PSK/host store in IndexedDB with the non-extractable `CryptoKey` design above
(port `psk_store.rs`/`paired_hosts.rs`), QR scan via `BarcodeDetector`/
getUserMedia (or paste), native dialogs -> in-app modals. Includes the async
key-deriver boundary.

**Phase 4 — versioned loader + manifest.**
- Build per-version bundles to `/tyde/v<TydeReleaseVersion>/` in CI on release.
- Publish a server-controlled `manifest.json` (allowlist of valid versions + SRI
  hashes) at the stable origin.
- Loader: tiny stable page parses `tyde-pair://v1?` QR (minimal JS CBOR extract
  of `release_version`), validates it via `TydeReleaseVersion` semantics, looks
  it up in the manifest (authority), then boots that bundle with SRI + CSP
  `script-src 'self'`. Returning users boot the stored host's version with no QR;
  `Reject`/version drift self-heals via re-pair.

**Phase 5 — PWA packaging.** Add web manifest + service worker (offline shell,
caching of immutable versioned bundles), iOS "Add to Home Screen" metadata.

## Verification

- **Native regression:** `cargo test` (workspace) + existing wasm UI tests via
  `tools/run-wasm-tests.sh` stay green after the Phase-1 refactor.
- **Transport parity:** a wasm-bindgen-test running the full salt handshake +
  encrypted round-trip against a wss broker (or local `rumqttd` with wss),
  asserting decrypt + counter validation match the native path.
- **Crypto parity:** test that WebCrypto-HKDF-derived session/rendezvous/ephemeral
  keys are byte-identical to the RustCrypto HKDF outputs for fixed
  salt/info/IKM vectors.
- **End-to-end:** pair a real desktop host's QR from the web client, connect,
  send/receive lines; confirm host on version X boots the client from `/tyde/vX/`
  and a mismatched stored version triggers re-pair.
- **Injection:** unit-test the loader rejects `release_version` values like
  `../evil`, `99.99.99` (not in manifest), and non-semver input.
- **Bundle/perf:** record release `.wasm` gz size and cold-load time on a
  throttled mobile profile (Phase 0 gate).

## Risks & open questions

- **iOS Safari PWA limits:** background WebSockets suspend when the PWA is
  backgrounded (drops connection; needs reconnect/resume UX — partially covered
  by existing `MqttReconnectBackoff`). Storage eviction can drop the PSK
  `CryptoKey` -> re-pair. No web push without extra setup (current app has none,
  low impact).
- **PSK at rest is not XSS-proof / not Keychain-equivalent** (see PSK section) —
  loader CSP/SRI/no-third-party-JS is load-bearing.
- **Version->URL injection:** mitigated by `TydeReleaseVersion` validation +
  manifest allowlist (authority) + SRI + CSP; loader never interpolates raw QR
  text into a URL.
- **Public broker:** broker.emqx.io is a shared test broker (rate-limited;
  `PublishPacer` handles quota). Production likely needs an owned wss broker.
- ~~**Codec spike** (Phase 0) is the main technical unknown~~ **RESOLVED:**
  standalone `mqttbytes 0.6.0` compiles to wasm32 and is the chosen codec;
  rumqttc's module does not.

## Uncertain / to verify during implementation

- ~~Whether `rumqttc`'s `mqttbytes` cleanly drops tokio on wasm32~~ **RESOLVED
  (Phase 0):** it does not; use the standalone `mqttbytes 0.6.0` crate.
- ~~Real release WASM bundle size~~ **MEASURED (Phase 0):** 1.64 MB gz
  (pre-wasm-opt); install wasm-opt/binaryen in build+CI and re-measure.
- Whether the web client reuses `client::connect_parts` directly (decides if the
  handshake bound stays `tokio::io` or moves to `futures_io`).
- Exact host-side source of truth for the real release version string feeding
  `release_version`.
- iOS-version behavior of Home-Screen storage persistence + `persist()` grant.
- broker.emqx.io wss endpoint/path/subprotocol specifics under load.
