# Mobile direct hosting

Tyde's mobile web app normally lives at `tycode.dev` and reaches your host
through a managed AWS IoT broker. Direct hosting is the other shape: the host
serves the app itself over an HTTP port, and phones talk the Tyde protocol back
to that same origin over a WebSocket. Nothing tunnels out.

That exists for one deployment in particular — a work network where punching a
tunnel through the firewall is unwelcome but reaching an internal site is
routine. Direct hosting is an ordinary internal web service: put it behind the
reverse proxy you already run, and phones on the VPN reach it like anything
else.

## What the host serves

Everything lives under `/tyde`:

| Path | What it is |
|---|---|
| `/tyde/` | The loader shell — the same `web/loader/` shipped to `tycode.dev` |
| `/tyde/manifest.json` | SRI hashes and the revocation authority, served `no-store` |
| `/tyde/v<version>/…` | The immutable app bundle, served `immutable` for a year |
| `/tyde/pair` | Redeems a pairing offer for a durable device token |
| `/tyde/ws` | The protocol WebSocket |

It is the deployed site's layout, byte for byte, because it is the same loader
booting the same bundle through the same manifest. The Content-Security-Policy
matches the CloudFront policy in `web/deploy/cloudfront-setup.md`.

## You must terminate TLS in front of it

The host speaks plain HTTP and never terminates TLS itself. On a plain-HTTP
origin browsers disable service workers, `crypto.subtle`, `getUserMedia`, push
and PWA install — and the app needs all of them. So an origin without HTTPS
loads and then fails in ways that look like app bugs.

Tyde cannot see the name your proxy publishes it under, which is why
**Direct hosting public URL** is a setting rather than something derived: it is
what goes in the pairing QR.

## Settings

All four live under Settings → Mobile, and all four are host-scoped.

| Setting | Meaning |
|---|---|
| `enable_mobile_connections` | Master switch for *every* mobile transport. Off means the direct origin does not run either. |
| `mobile_direct_hosting_enabled` | Run the direct origin. |
| `mobile_direct_bind_addr` | Where to listen. Default `127.0.0.1:8730` — loopback only, so a proxy on the same machine can reach it and nothing else can. |
| `mobile_direct_public_origin` | The URL phones use, e.g. `https://tyde.corp.internal`. Required to generate a pairing QR. |

The Mobile tab reports the host's own view back: whether the origin is serving,
on what address, how many files, from which bundle, and the verbatim reason
when it failed to start.

## Where the bundle comes from

Release builds compile the bundle into `tyde-server`, so a downloaded server
serves the app with nothing to configure. `server/build.rs` does that when
`TYDE_MOBILE_BUNDLE_DIR` is set at build time; the release and pre-tag
workflows set it after running the builder below. A plain `cargo build` embeds
nothing.

To build one yourself — necessary for a development host, and for serving a
bundle newer than the binary:

```
./dev.sh mobile-bundle                      # -> target/mobile-web
./dev.sh mobile-bundle --out /opt/tyde/web  # anywhere you like
```

The Mobile settings UI uses the compiled-in bundle and exposes no bundle
picker. Development hosts and HTTP-boundary tests can set the host setting
`mobile_direct_bundle_dir` directly to serve a built directory without
rebuilding the server. That developer override wins over the compiled-in
bundle.

Do not assemble the directory by hand. The manifest carries sha384 SRI over
every executable artifact and the loader enforces it, so a shell copied from
one build and a bundle from another produces an origin that loads and then
refuses its own scripts.

The single-release manifest declares `servedClientRelease`, equal to its sole
version and `minSupported`. On a cold launch the loader boots that declared
client even if the browser remembers an older host release. This allows an
upgraded host to replace its embedded bundle without stranding paired phones.
The declaration selects executable files, not a host or its live release:
selection, credentials and saved handshake authority remain unchanged until
the selected host sends Welcome/Reject. The declared entry must support
selected-host synchronization and include protocol metadata; policy and all
executable integrity checks still apply. A malformed or unavailable declaration
fails visibly, never selecting another release.

The builder validates this contract with
`tools/check_mobile_web_manifest.py --direct-hosted`. Shared `tycode.dev`
manifests reject the field and retain remembered-host startup. The updated
loader ships inside the host; updating the shared site alone cannot repair a
direct origin. Existing phones pick it up on their next navigation/cold launch
after the host updates, without clearing storage or re-pairing.

## Pairing

Direct pairing is its own QR variant (`tyde-pair://v3`), and it is deliberately
not the managed one:

- The QR carries a one-time secret, not a device credential. The phone POSTs it
  to `/tyde/pair` and gets back a durable device token; the offer is consumed
  whether or not the exchange succeeds, so a leaked QR cannot be retried.
- **The payload names no origin.** The phone learns the origin from the URL it
  scanned. An origin inside the payload would just be a redirect target the
  host cannot vouch for.
- The device token authenticates the WebSocket through
  `Sec-WebSocket-Protocol: tyde.token.<token>`, so it never lands in a proxy
  access log the way a query parameter would.

Only hashes are stored on the host — never the plaintext secret or token.

"Pair via local server" stays disabled until the origin reports itself serving
*and* a public URL is set. A QR built without both sends the phone to a dead
address, where the failure surfaces away from the settings that caused it.

## Tests

`server/tests/mobile_direct_hosting.rs` uses the shared `fixture::Fixture` and
drives the real HTTP origin: asset serving and cache headers, the master
switch reporting why hosting stops, pairing redemption through `/tyde/pair`,
a full `client::connect` handshake over the real WebSocket, token rejection,
cancellation, stale-cancel
isolation, scheduled expiry and renewal, and outstanding-offer retirement when
mobile access is disabled. The coarse flow requires a server `Expired` event
before HTTP redemption, using an injected zero-duration deadline, not a sleep.
Cancellation is observed through ordered real protocol events, including the
terminal event if its normal grace has already returned the screen to Idle.
The same flow causes a real filesystem replacement failure, requires visible
`StoreLoadFailed` rather than a false cancellation, retries after repairing
storage, and restarts the host before checking the retired link again.

Direct offers use the same current-offer cancellation/expiry lifecycle as
managed offers. Terminal cleanup retires the current credential regardless of
transport; only the current offer ID can trigger it. Managed cancellation still
uses its existing service RPC and failure handling. No frontend timer repairs
server pairing state, and HTTP expiry uses the same server transition.
Retirement commits before changing active credentials or reporting success;
failed storage leaves the offer available for an explicit retry, not silently
retired only in memory. Direct redemption matches the direct credential's own
ID and rejects overlapping managed offers or handoffs.

The regression predates native iOS navigation: commit
`7c76181c26d61b128c7c86e5cfbd00d52ab042e6` (2026-09-02) introduced the separate
direct credential and expiry scheduling, while Cancel and expiry still checked
only the managed credential. The four extracted paths were unchanged between
`c9844587fa54fdd9c180b8dd6a5331f073afe24f` and native source05
`c492e654d1d3c77617952eeece6e7797b57b58da`. The preserved source06 extraction
receipt and red protocol check identify the pre-existing Cancel failure; they
are provenance, not validation of this independently committed fix.

The compile-time embed is the one thing `./dev.sh check` cannot cover — it never
sets `TYDE_MOBILE_BUNDLE_DIR`, so every checked build has an empty table. What
check *does* cover is that such a build says so: with no directory configured it
reports an error naming the builder script rather than binding a port that 404s.

## Bounded typed apparatus adapter

On Linux/macOS, the existing `tyde-dev-driver` provides exactly five private-file
operations for a disposable, empty canonical DEV host. It does not launch hosts,
create tunnels, redeem offers, change expiry/Cancel, or drive a device.

Build/export from the completed, committed workbench:

```sh
./dev.sh mobile-direct-export /absolute/new/private-export
```

First pass `./dev.sh check` on the committed implementation workbench, as for
any source change. Export requires that clean committed tree and builds only the
driver, not a new host or mobile bundle. It uses Cargo's actual artifact path and
exports `tyde-dev-driver`, private build logs, and `build-receipt.json`. The
receipt hashes the executable and every tracked source file and pins HEAD and
the compiler. This is a host-native executable; run the same canonical command
on macOS if the executor needs a macOS binary. Do not copy a Linux binary to macOS.

Each invocation is foreground and has only two path arguments:

```sh
/absolute/private-export/tyde-dev-driver mobile-direct \
  /absolute/private-run/request.json /absolute/private-run/new-receipt.json
```

All JSON inputs/outputs must be owned regular mode0600 files in canonical,
owned mode0700 directories. The adapter rejects symlink inputs, existing outputs,
mutable inventory mismatches and unrelated host settings changes. Do not put
endpoints, origins, offers, identifiers or credentials in argv or logs. stdout
contains only a completion marker; failures contain only stage context.

Save the **actual decoded canonical `tyde_dev_instance_wait` Ready result** as
`ready.json` (not the MCP JSON-RPC envelope). Never construct a substitute Ready.
Create one immutable `run.json` per case, with these fields:

```json
{
  "ready_path": "/absolute/private-run/ready.json",
  "expected": {
    "protocol_version": 94,
    "tyde_version": {"major": 0, "minor": 8, "patch": 14},
    "release_version": "0.9.6-beta.15",
    "project_dir": "/absolute/immutable-source"
  },
  "bundle_dir": "/absolute/immutable-mobile-web",
  "inventory": {"each/relative/file": "its-lowercase-sha256"}
}
```

`inventory` is the **complete exact** public directory inventory from the sealed
case, including shell, manifest and all versioned artifacts. No stage directories,
AppleDouble sidecars, symlinks or special files are permitted. Hashes are checked
before and after every operation. Packet owners still qualify manifest/SRI,
trusted HTTPS, native custody and authenticated mobile transport separately.

Each request has `run` (the absolute `run.json` path) and an `operation` object:

| `operation.kind` | Other operation fields | Result |
|---|---|---|
| `configure` | none | One four-field CAS, kernel `127.0.0.1:0`, Directory Online/count/bind |
| `set_origin` | `previous`, `origin_path` | CAS actual origin from a private JSON string file |
| `offer` | `previous`, `offer_path` | One direct offer; unchanged serialized production payload at new private path |
| `revoke` | `previous` | Wait for this offer's Consumed state; revoke its sole disposable device |
| `disable` | `previous` | CAS hosting off; await authoritative Disabled |

`previous` is the prior invocation's receipt path. Sequence is configure →
set_origin → offer → native episode → revoke → disable. Without an offer,
configure/set_origin may go directly to disable. Each predecessor is claimed
with a new `.used` file **before** sending; never retry a failed or lost operation.
Retain partial outputs/claims for diagnosis. A failed operation is not success
and the tool performs no rollback, renewal, automatic retry or hidden cleanup.
Revoke never accepts an arbitrary device ID. Disable refuses an active offer,
so it cannot conceal the separately owned expiry/Cancel defect. After Disabled,
a new sealed case may configure the same DEV host with a fresh run/receipt chain.

Every operation observes an exact source-typed Hello/Welcome probe, including
release_version, independently of `client::connect`, then uses that production
client's validated event/sequence stream. CAS success requires the matching
SettingsWriteResult, applied/no errors, HostSettings/current_etag/desired fields,
and authoritative hosting state; fanout and result may arrive in either order.
Receipts retain these **actual private observations**, not reconstructed status.
The probe is not OS process/executable attestation; retain the packet owner's
kernel/process custody evidence for the actual canonical host separately.

`server/tests/mobile_direct_adapter.rs` exercises the adapter across real TCP,
production server/protocol and real HTTP with the shared mock-backend fixture.
Only the test emulates phone redemption; no provider turns or native acceptance
are claimed. It rejects a wrong Welcome release, missing-origin offer creation
(with no offer payload), and an active-offer disable,
then proves configure/origin/offer/consumption/revoke/Disabled. Run only through
`./dev.sh check`.
