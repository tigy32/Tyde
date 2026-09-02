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
| `mobile_direct_bundle_dir` | Optional. A bundle directory to serve instead of the one compiled into the binary. |

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

Then point **Mobile web bundle** at that directory. A configured directory
always wins over the compiled-in bundle, so this is also how you test a bundle
change without rebuilding the server.

Do not assemble the directory by hand. The manifest carries sha384 SRI over
every executable artifact and the loader enforces it, so a shell copied from
one build and a bundle from another produces an origin that loads and then
refuses its own scripts.

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

"Pair over this host" stays disabled until the origin reports itself serving
*and* a public URL is set. A QR built without both sends the phone to a dead
address, where the failure surfaces away from the settings that caused it.

## Tests

`tests/tests/mobile_direct_hosting.rs` drives the real HTTP origin: asset
serving and cache headers, the master switch and the enable toggle taking the
origin down, pairing redemption through `/tyde/pair`, a full `client::connect`
handshake over the real WebSocket, token rejection, and offer expiry.

The compile-time embed is the one thing `./dev.sh check` cannot cover — it never
sets `TYDE_MOBILE_BUNDLE_DIR`, so every checked build has an empty table. What
check *does* cover is that such a build says so: with no directory configured it
reports an error naming the builder script rather than binding a port that 404s.
