# Managed mobile WebRTC transport

Managed mobile/web connections use one reliable, ordered WebRTC data channel
(`tyde.v1`, negotiated channel 0), with Cloudflare TURN as the required relay.
The local `/tyde/ws` endpoint continues to use WebSockets. Both feed the same
Tyde framed byte protocol into the existing connection handlers: handshake,
sequence validation, replay, chat, files, terminals, and voice stay server-owned.
There is no automatic MQTT or direct-network fallback.

## Connection establishment

1. Pairing keeps its durable host/device secrets and the QR-shared pairing key.
   New managed QR offers advertise WebRTC transport version 1. Existing stored
   managed identities can connect after both applications and the service update.
2. Each reconnect signs `POST /pairings/{id}/webrtc` with its role's pairing
   secret. Mobile also needs its current Tyggs Pass session cookie. The service
   checks pairing status and issues fresh Cloudflare credentials and a scoped
   signaling grant. Neither grant is cached or written to device storage.
3. The mobile peer gathers a relay offer, signs the complete SDP with the pairing
   key, and publishes it to `/webrtc/signal`. The host polls this authenticated
   HTTPS endpoint while waiting for a phone. It verifies the offer, gathers its
   relay answer, signs it, and publishes the answer. Mobile verifies the answer
   before applying it. The session ID and description kind are included in each
   HMAC, binding the DTLS fingerprints to this pairing and negotiation.
4. Signaling retains one short-lived session per pairing in DSQL. A host claims
   each offer once; roles, session IDs, expiration and pairing revocation are
   checked on every exchange. HTTP polling uses the existing Lambda deployment;
   it carries connection setup only. Active data connections need no polling.
5. The data channel adapter bounds chunks and receive queues. A write flush waits
   for the peer to enqueue every chunk, and backpressure propagates to the sender.
   Normal connection heartbeat and disconnect handling remain in the Tyde layer.
   Connections close before TURN credential expiration and reconnect with fresh
   credentials. Errors surface through the existing mobile connection state.

Cloudflare sees relay IP addresses, timing and volume. The service sees signed
connection descriptions. Neither can read the DTLS-encrypted application data
or forge peer authentication without the pairing key.

The pinned WebRTC dependency includes a small TURN driver queue-ordering fix;
see `vendor/webrtc/TYDE-PATCH.md` for its source and regression evidence.

## Reconnect recovery

The transport keeps a stream alive through the temporary ICE `Disconnected`
state, as defined by [WebRTC](https://www.w3.org/TR/webrtc/#dom-rtcicetransportstate).
Previously the native ICE agent detected five seconds of missing traffic and
the adapter immediately destroyed the connection. The real relay regression
drops traffic for eight seconds, restores it, and requires the same stream to
preserve bulk bytes and bidirectional acknowledgements. Browser/native coverage
also resumes its existing stream after the outage. `Failed`, `Closed`, channel
errors, and the existing application liveness deadlines still end the stream.
Host close diagnostics identify which connection task ended and its elapsed time.

The shared browser timer layer owns and cancels individual `setTimeout` calls.
The former timer driver accumulated callbacks for cancelled long deadlines and
stopped scheduling the next event when its reference count exceeded 20. In the
real browser recovery flow, a 50 ms wait then slept until an unrelated 2-second
or 15-second deadline fired. Heartbeat, retry, signaling and frame-reassembly
timers all use the replacement; native timing remains on Tokio.

Mobile service requests have a 15-second deadline covering both response headers
and the complete response body. Cancelling or timing out a request aborts its
browser fetch. The complete managed credential-and-negotiation attempt is bounded
by the existing 60-second connection deadline. Returning after a background pause
can interrupt an attempt that is still obtaining credentials or negotiating.

After three consecutive failures of the same kind, the banner shows the error
and explains that retries continue automatically. Reconnect remains available
while connecting and after an error; it cancels the old attempt and starts fresh.
Request-stage and elapsed-time diagnostics contain no cookies or relay secrets.

The browser regression stalls real HTTP headers and bodies, restores the service,
and drives the recovery controls through a real TURN connection to a host. It
requires a fresh bootstrap and heartbeat reply. Before the fix, the initial
stalled response prevented even a second credential request.

## Network requirement

Native hosts connect to `turns:turn.cloudflare.com:443?transport=tcp` using
certificate-verified TLS over an outgoing TCP connection. Their WebRTC runtime
creates no UDP sockets or inbound TCP listeners, and rejects plain TCP and UDP
relay URLs. No router port forwarding is required. The service must return the
TLS endpoint explicitly; absence is an error rather than a transport fallback.

WebRTC's DTLS encryption still protects the application data end to end. TLS
additionally authenticates the Cloudflare relay and encrypts the host-to-relay
connection, including TURN control messages. TCP alone does not add encryption.
Browser/mobile peers retain the browser's UDP/TCP/TLS TURN connectivity. Relays
may exchange UDP internally; that does not require UDP access on the host.

The MQTT crate, loopback override, broker settings and credential APIs have been
removed. Existing managed pairing records keep their identities and keys while
ignoring obsolete transport fields. Old public-broker pairings require re-pairing.

## Canonical contract

`protocol/src/types.rs::mobile_rtc` is the source of truth for relay credentials,
roles, signaling commands, session IDs and signed descriptions. Export it to the
companion service with:

```sh
python3 tools/export-mobile-rtc.py /path/to/TydeMobileService
```

The generated service module must be committed with the corresponding API change.
The application protocol version increases for the removal of broker fields and
settings. The WebRTC transport version and byte framing remain unchanged.

## Deployment

TURN is already deployed. Removing its predecessor requires a separate approved
service/schema rollout after clients use TURN. The companion service documents
migration `004_retire_mobile_broker.sql`, removal of IoT resources, and cleanup
of the obsolete signing key from the existing SSM SecureString. Pairing records
and encryption keys remain intact. The code-only deploy guard rejects these
infrastructure/schema changes. Local validation performs no production changes.

## Validation

`./dev.sh check` includes a real native TURN flow, server handshakes and replay
across reconnections, and a browser/native TURN test using the production byte
adapters. The browser fixture is bound to loopback and exists only for the suite.
Both flows transfer four MiB with a delayed reader and assert complete ordered
bytes. Native peers connect through a real TLS endpoint to the real TURN server;
the same flow rejects an untrusted certificate and a trusted certificate for the
wrong hostname before exercising successful transfer and reconnection. The
browser also sends immediately from its channel-open callback, checking native
startup event ordering before the bulk transfer. The native flow rejects a
different pairing key and a substituted DTLS fingerprint. Mobile DOM/service
flows exercise fresh credential minting,
authentication failures and invalid/expired grants.

The service suite exercises real HTTP boundaries for signaling isolation,
role/session binding, expiration, disabled pairings and credential issuance.
Cloudflare issuance in that suite uses a local HTTP fixture. Production DSQL,
Cloudflare and a physical phone still need a smoke test after an approved rollout.
