# Phase-0 de-risk spikes (mobile-web/PWA)

> **Throwaway.** These crates/probes exist only to answer the Phase-0 gating
> questions in `docs/plans/mobile-web-pwa.md`. They are isolated standalone
> workspaces (each Cargo.toml has an empty `[workspace]`) so they do **not**
> join the main Tyde workspace. Delete before/after Phase 1 as desired.

## 1. `mqttbytes-wasm/` — rumqttc codec on wasm32 (the #1 risk)

Tests whether `rumqttc 0.25.1`'s `v5::mqttbytes` codec compiles to
`wasm32-unknown-unknown` with `default-features = false`.

```sh
cd mqttbytes-wasm
cargo build --target wasm32-unknown-unknown    # EXPECTED: FAILS
```

**Result: FAILS.** Even with `default-features = false`, rumqttc unconditionally
pulls `tokio` (net) -> `mio` + `socket2`, which have no `wasm32-unknown-unknown`
support. The codec module can't be reached without compiling the whole crate, so
the rumqttc-verbatim path is not viable on wasm.

## 2. `mqttbytes-standalone-wasm/` — fallback ladder rung 1 (the winner)

The standalone `mqttbytes 0.6.0` crate (rumqttc's codec extracted; depends only
on `bytes`).

```sh
cd mqttbytes-standalone-wasm
cargo build --target wasm32-unknown-unknown            # PASSES (debug)
cargo build --release --target wasm32-unknown-unknown  # PASSES (release)
cargo test                                             # PASSES (native round-trip correctness)
```

**Result: PASSES.** Only deps are `mqttbytes` + `bytes`. PUBLISH encode/decode
round-trips correctly. This is the recommended codec for the WASM transport
backend. Porting note: the standalone crate's v5 API differs slightly from
rumqttc's (`Publish::new` takes 3 args, no separate props tuple in the
`Packet::Publish` variant), so the actor's packet matching is a near-, not
exact-, verbatim port.

## 3. `webcrypto-ios-probe/index.html` — PSK-at-rest feasibility (manual, iOS)

Standalone HTML+JS to validate the hardened PSK-storage decision on a real iOS
device. **Cannot be run headless** (and `crypto.subtle` requires a secure
context). Serve over HTTPS or localhost, e.g.:

```sh
cd webcrypto-ios-probe
python3 -m http.server 8000      # then open http://localhost:8000 (desktop)
                                 # for iOS, serve over HTTPS / a tunnel
```

It imports a **non-extractable** HKDF `CryptoKey`, derives 256 bits with the real
Tyde session info string (`tyde-mqtt-v1`), stores the `CryptoKey` in IndexedDB,
reloads it after a force-quit, re-derives (asserting byte-identical output), and
confirms the reloaded key is still non-extractable. Also reports
`navigator.storage.persist()` support. Run Step 1, force-quit, reopen, run Step 2
— including from the Home-Screen-installed PWA and after >7 days (ITP eviction).
