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

## Network requirement

The pinned native `webrtc` 0.20.5 TURN implementation supports UDP relay
allocation. The service therefore issues UDP URLs explicitly to hosts; it does
not send unsupported TCP/TLS URLs and rely on the library silently skipping them.
Hosts need outbound UDP to Cloudflare TURN (normally port 3478). Browser/mobile
peers receive Cloudflare's UDP, TCP and TLS relay URLs, including TLS on 443.
A host with UDP blocked fails visibly. Supporting native TURN over TCP/TLS needs
an implementation with that capability; this migration does not claim it.

The loopback MQTT override remains an explicit development/test facility. The
managed production connection path never invokes it. The old broker metadata in
persisted pairing records and the service's legacy pairing response remains for
stored-identity/schema compatibility; it is not a connection fallback.

## Canonical contract

`protocol/src/types.rs::mobile_rtc` is the source of truth for relay credentials,
roles, signaling commands, session IDs and signed descriptions. Export it to the
companion service with:

```sh
python3 tools/export-mobile-rtc.py /path/to/TydeMobileService
```

The generated service module must be committed with the corresponding API change.
The existing Tyde application protocol version is unchanged; its payloads and
framing have not changed.

## Deployment

The companion `TydeMobileService` migration must precede the application rollout:

- Apply `003_mobile_rtc_signaling.sql` and grant the runtime role access to
  `mobile_rtc_signals` through the migration tool.
- Add `TYCODE_RTC_SIGNING_KEY` (at least 32 random bytes) and
  `TYCODE_TURN_API_TOKEN` to the existing Secrets Manager runtime JSON.
- Set the CloudFormation `CloudflareTurnKeyId` parameter and deploy the updated
  service configuration. `TYCODE_SIGNALING_URL` points to the HTTPS signal route.
- Deploy matching host and mobile builds; old managed QR codes are versioned and
  should be rescanned from the updated host.

This requires a configuration/database deployment; the code-only deployment guard
must reject it. No production resource changes are part of local validation.

## Validation

`./dev.sh check` includes a real native TURN flow, server handshakes and replay
across reconnections, and a browser/native TURN test using the production byte
adapters. The browser fixture is bound to loopback and exists only for the suite.
Both flows transfer four MiB with a delayed reader and assert complete ordered
bytes. The native flow also rejects a different pairing key and a substituted
DTLS fingerprint. Mobile DOM/service flows exercise fresh credential minting,
authentication failures and invalid/expired grants.

The service suite exercises real HTTP boundaries for signaling isolation,
role/session binding, expiration, disabled pairings and credential issuance.
Cloudflare issuance in that suite uses a local HTTP fixture. Production DSQL,
Cloudflare and a physical phone still need a smoke test after an approved rollout.
