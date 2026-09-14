# Managed mobile access

Managed access uses Cloudflare TURN and the Tyde WebRTC byte transport. The
former MQTT implementation, development broker override, broker grants and
AWS IoT authorizer are retired. See `mobile-webrtc-turn.md` for connection,
recovery, network, privacy and validation details. `protocol/src/types.rs`
remains the source of truth for Tyde wire types; `TydeMobileService` owns the
HTTP API, durable schema, runtime secrets and approved deployment procedure.

## Ownership and authentication

Tyggs Account provides generic identity, OAuth and Pass ownership. It has no
Tyde-specific pairing endpoints. The phone signs in and establishes a managed
mobile session before redeeming an offer. Pass-required and authentication
failures render explicit recovery screens. The host never receives Account
OAuth tokens, Pass proofs or billing data.

The host creates a short-lived offer through `POST /host/offers`. Its QR carries
the offer secret, release/protocol versions and pairing PSK inside a URL fragment.
The loader keeps that fragment out of HTTP requests and OAuth return URLs.
The phone redeems with `POST /pairings/redeem`, and the host polls and acknowledges
the durable handoff. Host and phone persist their role-specific pairing secrets;
the PSK authenticates WebRTC descriptions including their DTLS fingerprints.

Each reconnect obtains fresh scoped relay/signaling credentials through
`POST /pairings/{id}/webrtc`. Requests use the role's HMAC pairing credential;
mobile also presents the managed session cookie. `POST /webrtc/signal` exchanges
signed offers and answers, enforcing pairing, role, session and expiration.
`GET /pairings/{id}` and `POST /pairings/{id}/revoke` retain status and revocation.
Service failures never select another transport.

The service stores sessions, offers, pairings, replay nonces, signaling and audit
records in DSQL. Runtime signing and TURN API secrets come from encrypted SSM.
Neither temporary relay grants nor Account secrets belong in host settings,
QR codes, logs or persisted phone host records.

## Existing pairings and local hosting

Stored managed records retain identity, PSK and role credentials. Obsolete broker
and room fields are ignored and disappear when records are rewritten. Old v1
public-broker QR codes and unmanaged stored records require explicit re-pairing;
the removed protocol is never decoded into a connectable transport.

Self-hosted `/tyde/ws` connections remain an explicit mode selected by a direct
pairing QR. They are not a fallback from managed access. Their origin, device
token, revocation and TLS proxy requirements remain unchanged.

The protocol version increases when broker fields/status/settings are removed,
so host and mobile must run matching release bundles. This does not change the
WebRTC transport version. The service's broker-free schema and infrastructure
must be deployed through the separately reviewed retirement rollout documented
in `TydeMobileService`; local code changes do not remove deployed resources.
