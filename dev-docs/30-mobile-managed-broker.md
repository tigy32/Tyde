# Mobile managed broker access

This document is the documentation and contract for the Tyggs Pass +
`tycode.dev` managed AWS MQTT mobile access plan.

It is a design/specification document, not an alternate source of truth for
Tyde protocol shapes. When this plan is implemented, all Tyde wire payloads and
state enums still belong in Rust in `protocol/src/types.rs`; the desktop and
mobile UIs render the server-emitted state from those protocol types.

---

## Locked decisions

These decisions are already approved and must not be redesigned by later
implementation work:

1. **Tyggs stays generic.** `tyggs.com` owns account login, OAuth, and a
   generic Tyggs Pass ownership proof/API. It does not expose Tyde- or
   Tycode-specific pairing, broker, topic, device, or host APIs.
2. **`tycode.dev` owns Tyde mobile access.** Tyde mobile pairing, device
   records, broker credential grants, repair state, and AWS MQTT integration
   are `tycode.dev` product infrastructure. Storage belongs to that service
   now, with a DSQL-backed store later.
3. **Mobile authenticates before redeeming pairing.** The mobile web app must
   complete Tyggs OAuth and present a valid generic Tyggs Pass proof to
   `tycode.dev` before redeeming a Tyde pairing offer. If the user has no
   Tyggs Pass, the app shows a splash/paywall link. Tyde does not interpret
   billing state beyond generic pass ownership.
4. **The host is not Tyggs-authenticated.** The desktop host never receives
   Tyggs tokens or pass proofs. It still must obtain broker credentials through
   `tycode.dev` pairing; production must not silently fall back to a public or
   free MQTT broker.
5. **AWS IoT Core is the managed broker target.** MQTT access is authorized by
   an AWS IoT custom authorizer that validates `tycode.dev`-signed broker
   credentials and enforces the scoped topic namespace, participant role, and
   MQTT client id.
6. **Tyggs secrets stop at `tycode.dev`.** Tyggs OAuth tokens and pass proofs
   are consumed only by `tycode.dev`. They are never sent to the Tyde host,
   stored in Tyde host settings, embedded in pairing QR codes, or sent to AWS
   IoT Core.
7. **No usage counters in MVP.** Do not add Tyde or `tycode.dev` tables such as
   `usage_counters` or `mobile_usage_counters` for the first managed-broker
   release. Use AWS-native monitoring, alarms, budgets, and broker metrics
   later if usage controls become necessary.
8. **Legacy public-broker pairings fail closed.** Existing anonymous
   public-broker records and old pairing records require repair/re-pair. They
   must not silently continue through the old public broker and must not be
   upgraded without an explicit `tycode.dev` pairing flow.
9. **Tyde stays server-centric.** The protocol source of truth remains
   `protocol/src/types.rs`; the server owns mobile access behavior and emits
   state. The UI only renders state and sends typed events.

---

## Ownership boundaries

### `tyggs.com`

`tyggs.com` owns only generic identity and pass ownership:

- OAuth login/account identity.
- Generic Tyggs Pass ownership proof issuance/validation.
- Generic paywall or pass purchase pages.

Tyggs does not know about Tyde host ids, MQTT rooms, pairing offers, AWS topic
names, Tyde release versions, or Tycode device records.

### `tycode.dev`

`tycode.dev` owns Tyde-specific managed access:

- Mobile access API endpoints under `https://tycode.dev/api/tyde/mobile/v1/`.
- Validation of Tyggs OAuth/pass proof with generic Tyggs APIs.
- Short-lived mobile API sessions derived from validated Tyggs pass ownership.
- Host pairing offers and one-time pairing redemption.
- Device/host pairing records and repair/revocation state.
- Broker credential minting for AWS IoT Core.
- Custom-authorizer signing keys, revocation checks, audit logs, and metrics.

The host can be anonymous to Tyggs while still authenticated to `tycode.dev` by
pairing secrets issued during a successful pass-gated mobile redeem.

### Tyde desktop/server

The desktop/server owns local host behavior:

- Starts/cancels pairing through typed host-stream events.
- Calls `tycode.dev` to create offers and obtain scoped broker credentials.
- Stores only Tyde pairing metadata and `tycode.dev` pairing secrets needed to
  reconnect to the managed broker.
- Emits typed mobile access state to the desktop UI.
- Fails closed when managed access is unavailable or a stored pairing is legacy.

It does not store Tyggs OAuth tokens, Tyggs pass proofs, billing state, or AWS
long-lived credentials.

### Tyde mobile web/PWA

The mobile web app owns the user-facing mobile access flow:

- Scans/receives a Tyde pairing QR/link.
- Authenticates with Tyggs before redeeming the offer.
- Shows a splash/paywall link if `tycode.dev` reports no generic Tyggs Pass.
- Redeems the offer with `tycode.dev`, stores mobile pairing credentials in the
  mobile shell/web storage layer, and connects to the managed broker.
- Renders Tyde protocol state emitted by the host after the MQTT transport is
  established.

The mobile app may understand the generic `pass_required` state so it can show a
paywall link. It must not branch on Tyde-specific billing tiers or plan details.

### AWS IoT Core

AWS IoT Core is the MQTT broker. It validates only `tycode.dev` broker
credentials through the custom authorizer. It never receives Tyggs OAuth tokens
or pass proofs.

---

## End-to-end flow

### First-time pairing

1. The desktop user enables mobile access and starts pairing.
2. Tyde server calls `tycode.dev` `POST /host/offers`.
3. `tycode.dev` creates a pending offer, returns a short-lived offer token,
   one-time offer secret, managed broker endpoint, and host-scoped broker
   credentials.
4. The host connects to AWS IoT Core with the returned host credentials and
   builds the QR from the offer id/secret, managed broker endpoint, and
   host-generated Tyde rendezvous room plus data-room PSK.
5. The mobile web app scans the URL and selects the matching release bundle
   through the existing loader rules.
6. Before redeeming, the mobile app completes Tyggs OAuth and exchanges the
   generic pass proof with `tycode.dev` for a short-lived mobile API session.
7. If no Tyggs Pass is present, `tycode.dev` returns `pass_required`; the mobile
   app shows the splash/paywall link and does not redeem the offer.
8. With a valid pass, the mobile app calls `POST /pairings/redeem` using the
   scanned offer secret and mobile API session.
9. `tycode.dev` atomically consumes the offer, creates a pairing, returns
   mobile-scoped broker credentials to the phone, and lets the host poll observe
   the redeemed pairing plus its host pairing secret.
10. Host and phone use AWS IoT Core only with `tycode.dev`-signed, scoped
    broker credentials. The existing Tyde MQTT rendezvous/data-room protocol
    then carries the normal Tyde NDJSON stream.

### Subsequent reconnects

1. Host and mobile retain their own `tycode.dev` pairing secrets. They do not
   retain Tyggs tokens in host storage.
2. Each side requests fresh short-lived broker credentials from `tycode.dev`.
3. For mobile requests, `tycode.dev` requires a current mobile API session whose
   Tyggs Pass proof is still valid, or asks the app to refresh OAuth/proof.
4. For host requests, `tycode.dev` authenticates the host pairing secret and
   the stored pairing state. The host is never redirected to Tyggs.
5. If a pairing is revoked, expired, legacy, or otherwise not serviceable,
   `tycode.dev` returns a repair/re-pair status and Tyde surfaces it. There is
   no public-broker fallback.

---

## `tycode.dev` HTTP API contract

All endpoints are HTTPS-only JSON APIs. The stable base path is:

```text
https://tycode.dev/api/tyde/mobile/v1
```

Requests and responses use `snake_case` JSON fields. Timestamps are Unix epoch
milliseconds. Identifiers are opaque strings with a type prefix in examples;
clients must not parse them for semantics.

### Common error shape

```json
{
  "error": {
    "code": "pass_required",
    "message": "A Tyggs Pass is required for Tyde mobile access.",
    "retryable": false,
    "state": "pass_required",
    "paywall_url": "https://tyggs.com/pass"
  }
}
```

Common error codes:

| Code | HTTP | Meaning |
| --- | ---: | --- |
| `invalid_request` | 400 | Malformed JSON, missing fields, or invalid enum value. |
| `invalid_tyggs_auth` | 401 | OAuth token or pass proof is invalid or expired. |
| `mobile_session_required` | 401 | Mobile endpoint needs a `tycode.dev` mobile session. |
| `pass_required` | 402 | The Tyggs account does not currently prove generic pass ownership. |
| `forbidden` | 403 | Authenticated caller is not allowed to perform the action. |
| `not_found` | 404 | Offer, pairing, or device does not exist for the caller. |
| `offer_already_redeemed` | 409 | A one-time offer was already consumed. |
| `duplicate_device` | 409 | The submitted device identity conflicts with an active pairing. |
| `offer_expired` | 410 | The offer expired before redemption. |
| `repair_required` | 410 | The stored pairing is legacy or unusable and must be re-paired. |
| `version_mismatch` | 422 | Protocol or release compatibility prevents pairing. |
| `broker_unavailable` | 503 | AWS IoT or credential minting is temporarily unavailable. |
| `service_unavailable` | 503 | `tycode.dev` mobile access is temporarily unavailable before broker-specific work can proceed. |
| `rate_limited` | 429 | Caller exceeded offer, redeem, or credential minting limits. |
| `internal` | 500 | Unexpected service failure. |

The desktop host must treat all non-success responses as explicit failures to
surface through server-owned state. It must not substitute any default broker.

### Shared broker endpoint shape

```json
{
  "endpoint": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt",
  "provider": "aws_iot_core",
  "region": "us-west-2",
  "authorizer_name": "tycode-mobile-v1"
}
```

### Shared broker credential shape

```json
{
  "grant_id": "grant_01J...",
  "client_id": "tyde/prod/pair_01J.../host/grant_01J...",
  "connect": {
    "username": "tyde?x-amz-customauthorizer-name=tycode-mobile-v1",
    "password": "<tycode-signed-grant-jwt>",
    "websocket_url": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=<tycode-signed-grant-jwt>",
    "headers": {
      "x-amz-customauthorizer-name": "tycode-mobile-v1",
      "tycode-grant": "<tycode-signed-grant-jwt>"
    }
  },
  "scope": {
    "namespace": "tyde/prod/pair_01J...",
    "role": "host",
    "publish": ["tyde/prod/pair_01J.../rooms/+/host-to-client"],
    "subscribe": ["tyde/prod/pair_01J.../rooms/+/client-to-host"]
  },
  "issued_at_ms": 1760000000000,
  "expires_at_ms": 1760000900000
}
```

Both native and browser managed WSS transports must use the exact validated
service-issued `connect.websocket_url` for AWS IoT Core custom-authorizer
connections. The base broker endpoint is validation context only and is never a
connect fallback. A missing, malformed, or endpoint-mismatched
`connect.websocket_url` is a terminal typed transport-configuration error.
The current service response also includes `connect.username`,
`connect.password`, and `connect.headers` for contract compatibility. Tyde
retains those fields in the typed response but managed transports do not send
them. Supplying the service username/password after a successful Upgrade causes
AWS IoT to reject MQTT CONNECT without invoking the custom authorizer. Adding
the `x-amz-customauthorizer-name` selector as an HTTP header alongside the query
causes a pre-authorizer 403; `tycode-grant` is the token key/value, not another
selector. Both native and browser therefore use the query-bearing WebSocket URL
as the single authentication channel and preserve the exact service-issued
`client_id`, role, and scoped topic namespace.

The `rooms/+` filter is intentional for Tyde's managed **ephemeral** MQTT path.
The stored Tyde room is a rendezvous channel only; during
`connect_managed_ephemeral` the peers negotiate a fresh random data room and
then reconnect with the same short-lived broker credentials. MVP credentials
therefore authorize exactly one room wildcard within the pairing/offer
namespace and exactly one role direction. They still must not authorize `#`,
extra filters, another namespace, another role direction, or another MQTT
client id. Exact room-scoped grants are valid only for a direct/non-ephemeral
managed connection, or for a future two-mint design that remints credentials
after the negotiated data room is known.

### `POST /auth/session`

Mobile-only. Exchanges Tyggs OAuth + generic pass proof for a short-lived
`tycode.dev` mobile API session.

Request:

```json
{
  "tyggs_oauth_access_token": "<oauth-access-token>",
  "tyggs_pass_proof": "<generic-pass-proof-jwt>",
  "client": {
    "kind": "mobile_web",
    "release_version": "0.8.19",
    "protocol_version": 36
  }
}
```

Success response:

```json
{
  "session_token": "tycode_mobile_session_01J...",
  "expires_at_ms": 1760003600000,
  "tyggs_subject_hash": "sha256:...",
  "pass": {
    "kind": "tyggs_pass",
    "state": "active",
    "proof_expires_at_ms": 1760003600000
  }
}
```

`tyggs_subject_hash` is for local display/debug correlation only. It must not be
used by Tyde as an account id and must never be sent to the host or broker.

Failure response when the user lacks a pass:

```json
{
  "error": {
    "code": "pass_required",
    "message": "A Tyggs Pass is required for Tyde mobile access.",
    "retryable": false,
    "state": "pass_required",
    "paywall_url": "https://tyggs.com/pass"
  }
}
```

### `POST /host/offers`

Host endpoint. Creates a pending pairing offer. The host is not
Tyggs-authenticated.

Request:

```json
{
  "host_label": "Mike's MacBook Pro",
  "host_release_version": "0.8.19",
  "protocol_version": 36,
  "transport_protocol_version": 3,
  "host_nonce": "base64url-random-16-plus-bytes"
}
```

Success response:

```json
{
  "offer_id": "offer_01J...",
  "offer_secret": "offer_secret_01J...",
  "host_offer_token": "host_offer_01J...",
  "expires_at_ms": 1760000300000,
  "broker": {
    "endpoint": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt",
    "provider": "aws_iot_core",
    "region": "us-west-2",
    "authorizer_name": "tycode-mobile-v1"
  },
  "host_broker_credentials": {
    "grant_id": "grant_01J...",
    "client_id": "tyde/prod/offer_01J.../host/grant_01J...",
    "connect": {
      "username": "tyde?x-amz-customauthorizer-name=tycode-mobile-v1",
      "password": "<tycode-signed-grant-jwt>",
      "websocket_url": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=<tycode-signed-grant-jwt>",
      "headers": {
        "x-amz-customauthorizer-name": "tycode-mobile-v1",
        "tycode-grant": "<tycode-signed-grant-jwt>"
      }
    },
    "scope": {
      "namespace": "tyde/prod/offer_01J...",
      "role": "host",
      "publish": ["tyde/prod/offer_01J.../rooms/+/host-to-client"],
      "subscribe": ["tyde/prod/offer_01J.../rooms/+/client-to-host"]
    },
    "issued_at_ms": 1760000000000,
    "expires_at_ms": 1760000300000
  },
  "pairing_url": "https://tycode.dev/tyde/#tyde-pair://v2?<opaque-offer-payload>",
  "status": "pending"
}
```

`offer_secret` is returned only to the host that created the offer. The host
uses it with its locally generated Tyde rendezvous room and data-room
pre-shared key to build the canonical `tyde-pair://v2` QR fragment. The
`pairing_url` field is compatibility metadata; Tyde hosts must not surface a
service-built URL unless it already carries the host-generated Tyde room and
PSK. The tycode.dev service must not receive or store the Tyde data-room PSK.

Offer credentials are short-lived and scoped only to the pending offer
namespace. Durable pairing credentials are issued only after a pass-gated mobile
redeem.

### `GET /host/offers/{offer_id}`

Host endpoint. Polls offer state using the `host_offer_token` returned by
`POST /host/offers`.

Headers:

```text
Authorization: Bearer host_offer_01J...
```

Pending response:

```json
{
  "offer_id": "offer_01J...",
  "status": "pending",
  "expires_at_ms": 1760000300000
}
```

Redeemed response:

```json
{
  "offer_id": "offer_01J...",
  "status": "redeemed",
  "pairing_id": "pair_01J...",
  "host_pairing_secret": "host_pairing_secret_01J...",
  "device": {
    "device_id": "dev_01J...",
    "label": "Mike's iPhone",
    "created_at_ms": 1760000100000
  },
  "broker": {
    "endpoint": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt",
    "provider": "aws_iot_core",
    "region": "us-west-2",
    "authorizer_name": "tycode-mobile-v1"
  },
  "host_broker_credentials": {
    "grant_id": "grant_01J...",
    "client_id": "tyde/prod/pair_01J.../host/grant_01J...",
    "connect": {
      "username": "tyde?x-amz-customauthorizer-name=tycode-mobile-v1",
      "password": "<tycode-signed-grant-jwt>",
      "websocket_url": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=<tycode-signed-grant-jwt>",
      "headers": {
        "x-amz-customauthorizer-name": "tycode-mobile-v1",
        "tycode-grant": "<tycode-signed-grant-jwt>"
      }
    },
    "scope": {
      "namespace": "tyde/prod/pair_01J...",
      "role": "host",
      "publish": ["tyde/prod/pair_01J.../rooms/+/host-to-client"],
      "subscribe": ["tyde/prod/pair_01J.../rooms/+/client-to-host"]
    },
    "issued_at_ms": 1760000100000,
    "expires_at_ms": 1760001000000
  }
}
```

`host_pairing_secret` is returned only to the host that created the offer. If
the host loses it before storing it in the owner-only Tyde pairing store, the
repair path is to re-pair.

Terminal offer states are `redeemed`, `expired`, `cancelled`, and `failed`.
Tyde must surface terminal failure state and stop polling.

### `DELETE /host/offers/{offer_id}`

Host endpoint. Cancels a pending offer.

Headers:

```text
Authorization: Bearer host_offer_01J...
```

Success response:

```json
{
  "offer_id": "offer_01J...",
  "status": "cancelled"
}
```

### `POST /pairings/redeem`

Mobile-only. Redeems a scanned offer. Requires a `tycode.dev` mobile session,
which in turn requires a valid generic Tyggs Pass proof.

Headers:

```text
Authorization: Bearer tycode_mobile_session_01J...
```

Request:

```json
{
  "offer_id": "offer_01J...",
  "offer_secret": "offer_secret_from_qr",
  "device_label": "Mike's iPhone",
  "device_nonce": "base64url-random-16-plus-bytes",
  "release_version": "0.8.19",
  "protocol_version": 36,
  "transport_protocol_version": 3
}
```

Success response:

```json
{
  "status": "active",
  "pairing_id": "pair_01J...",
  "device_id": "dev_01J...",
  "device_pairing_secret": "device_pairing_secret_01J...",
  "paired_host": {
    "label": "Mike's MacBook Pro",
    "host_release_version": "0.8.19",
    "protocol_version": 36
  },
  "broker": {
    "endpoint": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt",
    "provider": "aws_iot_core",
    "region": "us-west-2",
    "authorizer_name": "tycode-mobile-v1"
  },
  "mobile_broker_credentials": {
    "grant_id": "grant_01J...",
    "client_id": "tyde/prod/pair_01J.../mobile/dev_01J.../grant_01J...",
    "connect": {
      "username": "tyde?x-amz-customauthorizer-name=tycode-mobile-v1",
      "password": "<tycode-signed-grant-jwt>",
      "websocket_url": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=<tycode-signed-grant-jwt>",
      "headers": {
        "x-amz-customauthorizer-name": "tycode-mobile-v1",
        "tycode-grant": "<tycode-signed-grant-jwt>"
      }
    },
    "scope": {
      "namespace": "tyde/prod/pair_01J...",
      "role": "mobile",
      "publish": ["tyde/prod/pair_01J.../rooms/+/client-to-host"],
      "subscribe": ["tyde/prod/pair_01J.../rooms/+/host-to-client"]
    },
    "issued_at_ms": 1760000100000,
    "expires_at_ms": 1760001000000
  }
}
```

`offer_secret` is one-time. A successful redeem consumes the offer atomically.
A second redeem must return `offer_already_redeemed` and must not reveal pairing
secrets.

### `POST /pairings/{pairing_id}/broker-credentials`

Host or mobile endpoint. Mints fresh short-lived broker credentials for an
existing pairing.

Authentication:

- Host role: HMAC authentication with the stored `host_pairing_secret`.
- Mobile role: `Authorization: Bearer tycode_mobile_session_...` plus HMAC
  authentication with the stored `device_pairing_secret`.

Canonical HMAC input is the HTTP method, path, request body SHA-256, nonce, and
timestamp. The header format is intentionally service-owned, but it must bind
`pairing_id`, `role`, and the request body so a captured signature cannot be
replayed for another role or topic scope.

Request:

```json
{
  "role": "host",
  "client_instance_id": "uuid-v4",
  "protocol_version": 36,
  "transport_protocol_version": 3,
  "requested_rooms": [
    {
      "room_id": "base64url-room-id",
      "purpose": "rendezvous"
    }
  ]
}
```

`requested_rooms` records the Tyde rendezvous room being used for this
credential request. For the MVP ephemeral transport it must not be interpreted
as an instruction to narrow AWS policy to that exact rendezvous room, because
the final Tyde data stream moves to a negotiated random data room. A future
least-privilege refinement may mint rendezvous credentials first, negotiate the
data room, then mint a second exact data-room grant; that second-mint flow is
not part of this deployable MVP contract.

Success response:

```json
{
  "pairing_id": "pair_01J...",
  "status": "active",
  "broker": {
    "endpoint": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt",
    "provider": "aws_iot_core",
    "region": "us-west-2",
    "authorizer_name": "tycode-mobile-v1"
  },
  "broker_credentials": {
    "grant_id": "grant_01J...",
    "client_id": "tyde/prod/pair_01J.../host/grant_01J...",
    "connect": {
      "username": "tyde?x-amz-customauthorizer-name=tycode-mobile-v1",
      "password": "<tycode-signed-grant-jwt>",
      "websocket_url": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&tycode-grant=<tycode-signed-grant-jwt>",
      "headers": {
        "x-amz-customauthorizer-name": "tycode-mobile-v1",
        "tycode-grant": "<tycode-signed-grant-jwt>"
      }
    },
    "scope": {
      "namespace": "tyde/prod/pair_01J...",
      "role": "host",
      "publish": ["tyde/prod/pair_01J.../rooms/+/host-to-client"],
      "subscribe": ["tyde/prod/pair_01J.../rooms/+/client-to-host"]
    },
    "issued_at_ms": 1760000200000,
    "expires_at_ms": 1760001100000
  }
}
```

Failure states include `repair_required`, `pairing_revoked`,
`broker_unavailable`, `version_mismatch`, and, for mobile callers only,
`pass_required`.

### `GET /pairings/{pairing_id}`

Host or mobile endpoint. Returns service-owned pairing state without minting a
broker credential.

Response:

```json
{
  "pairing_id": "pair_01J...",
  "status": "active",
  "repair_reason": null,
  "device": {
    "device_id": "dev_01J...",
    "label": "Mike's iPhone",
    "created_at_ms": 1760000100000,
    "last_seen_at_ms": 1760000200000
  }
}
```

Statuses are `active`, `revoked`, `repair_required`, and `suspended`. `suspended`
is a service-owned state; Tyde should render the provided message and must not
infer billing details.

### `POST /pairings/{pairing_id}/revoke`

Host or mobile endpoint. Revokes a pairing or device. A host can revoke the
whole pairing. A mobile device can revoke itself.

Request:

```json
{
  "reason": "user_requested"
}
```

Response:

```json
{
  "pairing_id": "pair_01J...",
  "status": "revoked"
}
```

Revocation must immediately make future credential requests fail and should be
visible to the AWS authorizer through a cache/DSQL invalidation path.

---

## Storage and DSQL sketch

The MVP can start with the service's current durable store, but the target model
must fit a DSQL-compatible relational schema. The important boundary is that
this is `tycode.dev` storage, not Tyde host settings and not Tyggs product
storage.

### `mobile_api_sessions`

Short-lived sessions derived from Tyggs OAuth/pass proof.

| Column | Purpose |
| --- | --- |
| `session_id` | Opaque session id/token hash. |
| `tyggs_subject_hash` | Stable hash of generic Tyggs subject, not raw account data. |
| `pass_state` | `active`; absent/expired sessions are not stored as active. |
| `proof_expires_at_ms` | Upper bound from Tyggs proof. |
| `created_at_ms`, `expires_at_ms` | Session lifetime. |

### `mobile_pairing_offers`

Pending host-created offers.

| Column | Purpose |
| --- | --- |
| `offer_id` | Primary key. |
| `offer_secret_hash` | Hash of one-time QR secret. |
| `host_offer_token_hash` | Hash for polling/cancel auth. |
| `host_label` | Display label from host. |
| `host_release_version`, `protocol_version` | Compatibility inputs. |
| `transport_protocol_version` | MQTT transport compatibility input. |
| `status` | `pending`, `redeemed`, `expired`, `cancelled`, `failed`. |
| `created_at_ms`, `expires_at_ms`, `redeemed_at_ms` | Lifecycle timestamps. |
| `redeemed_by_pairing_id` | Set exactly once on successful redeem. |

### `mobile_pairings`

Durable Tyde mobile pairing records.

| Column | Purpose |
| --- | --- |
| `pairing_id` | Primary key. |
| `host_secret_hash` | Host HMAC secret hash. |
| `device_secret_hash` | Mobile HMAC secret hash. |
| `tyggs_subject_hash` | Hash of generic Tyggs subject that redeemed. |
| `device_id`, `device_label` | Mobile device identity/display. |
| `topic_namespace` | AWS topic namespace, e.g. `tyde/prod/pair_01J...`. |
| `status` | `active`, `revoked`, `repair_required`, `suspended`. |
| `repair_reason` | Machine-readable reason for re-pair flows. |
| `created_at_ms`, `last_seen_at_ms`, `revoked_at_ms` | Lifecycle timestamps. |

### `broker_credential_grants`

Issued short-lived AWS IoT authorizer grants.

| Column | Purpose |
| --- | --- |
| `grant_id` | JWT `jti` and primary key. |
| `pairing_id` or `offer_id` | Scope owner. |
| `role` | `host` or `mobile`. |
| `client_id` | Exact MQTT client id authorized. |
| `topic_namespace` | Authorized topic prefix. |
| `issued_at_ms`, `expires_at_ms` | Grant lifetime. |
| `revoked_at_ms` | Optional explicit revocation. |

### `mobile_access_audit_events`

Append-only security/audit events: offer created, redeem attempted, redeem
succeeded, credential minted, credential denied, pairing revoked, repair
required. Audit records must not include Tyggs tokens, pass proofs, raw pairing
secrets, or broker grant tokens.

### Explicit non-table

Do not add `usage_counters`, `mobile_usage_counters`, or equivalent
application-level usage accounting for MVP. If cost control is needed later,
start with AWS IoT metrics, CloudWatch alarms, AWS Budgets, and service-level
rate limits before adding product counters.

---

## AWS IoT custom authorizer contract

`tycode.dev` signs a short-lived broker grant. Its selector and token are carried
in the WebSocket Upgrade URL query. AWS IoT first returns HTTP 101, then resolves
the named custom authorizer while processing MQTT CONNECT. The authorizer's
allow/deny policy is scoped to the exact client id and topic namespace.

### Grant claims

The signed grant is a compact JWT or equivalent JWS with these semantic claims:

```json
{
  "v": 1,
  "iss": "https://tycode.dev",
  "aud": "aws-iot-tyde-mobile-mqtt",
  "sub": "pairing:pair_01J...:role:host",
  "jti": "grant_01J...",
  "pairing_id": "pair_01J...",
  "offer_id": null,
  "role": "host",
  "client_id": "tyde/prod/pair_01J.../host/grant_01J...",
  "topic_namespace": "tyde/prod/pair_01J...",
  "publish": ["tyde/prod/pair_01J.../rooms/+/host-to-client"],
  "subscribe": ["tyde/prod/pair_01J.../rooms/+/client-to-host"],
  "iat": 1760000200,
  "nbf": 1760000200,
  "exp": 1760001100,
  "kid": "tycode-mobile-2026-01"
}
```

Offer-scoped host grants use `offer_id` and an offer namespace instead of
`pairing_id`. They expire quickly and cannot authorize durable device traffic.

### Authorizer validation

The custom authorizer must:

1. Verify signature, `kid`, issuer, audience, `nbf`, and `exp`.
2. Verify the MQTT CONNECT `clientId` exactly equals the grant `client_id`.
3. Verify the grant has not been revoked and its pairing/offer is still in an
   authorizable state.
4. Verify the role is one of the known enum values.
5. Return an AWS IoT policy that allows:
   - `iot:Connect` only for the exact client id;
   - `iot:Publish` only for the grant's publish topic filters;
   - `iot:Subscribe` and `iot:Receive` only for the grant's subscribe topic
     filters.
6. Deny all other topics, client ids, roles, and expired/revoked grants.

Role topic direction is fixed:

| Role | Publish | Subscribe/receive |
| --- | --- | --- |
| `host` | `.../host-to-client` | `.../client-to-host` |
| `mobile` | `.../client-to-host` | `.../host-to-client` |

The authorizer should use a short cache TTL no longer than the credential
lifetime. Revocation should either invalidate that cache or use sufficiently
short grants that revocation delay is bounded and documented.

### Payload confidentiality

AWS authorizes MQTT connections and sees metadata such as client ids, topic
names, timing, and payload sizes. Tyde's MQTT transport must continue to encrypt
Tyde protocol bytes end-to-end with pairing/data-room keys before publishing.
AWS IoT Core and `tycode.dev` broker infrastructure must not need plaintext
Tyde NDJSON frames.

---

## Tyde integration points

### Protocol

All Tyde-visible state changes must be reflected as Rust protocol types in
`protocol/src/types.rs` before code implementation:

- Managed broker status: disabled, connecting, online, error, repair required.
- Pairing offer lifecycle: active, consumed, expired, cancelled, failed.
- Device lifecycle: paired, connected, revoked, repair required.
- Explicit error codes for managed-service failures: pass required, repair
  required, broker unavailable, broker rejected, version mismatch, and internal.

The exact names may differ when encoded as enums, but the source of truth is
Rust. Do not duplicate these shapes in frontend-only TypeScript/Rust structs or
service-specific ad hoc JSON parsers.

### Server/desktop host

Implementation must keep host behavior in `tyde-server`:

- `mobile_pairing_start` creates a `tycode.dev` offer and emits the resulting
  server-owned `mobile_access_state` / `mobile_pairing_offer` events.
- The host stores `pairing_id`, device summary, managed broker endpoint,
  `host_pairing_secret`, and display metadata in owner-only local storage.
- The host never stores Tyggs OAuth tokens, pass proofs, paywall URLs, or billing
  detail.
- Production broker credentials always come from `tycode.dev`. If the service
  is unavailable, Tyde emits an error state.
- Public/free MQTT broker configuration is not a production fallback. Local
  development or tests may use an explicit dev/test-only override, but it must
  be visibly non-production and must not silently activate in releases.
- Existing stored records that lack `tycode.dev` pairing identity or contain an
  anonymous/public broker endpoint are marked repair required and do not connect.

### Mobile web/PWA

The mobile app must gate redemption on `tycode.dev` mobile session creation:

- Scan/preview can happen before login so the app can show host/release context.
- Redeem cannot happen until `POST /auth/session` succeeds.
- `pass_required` renders the splash/paywall link and stops the redeem path.
- The app stores only `tycode.dev` mobile session/device pairing material and
  MQTT transport keys; it does not expose Tyggs tokens to the host or AWS.
- Once connected, app UI remains a projection of server-emitted Tyde state.

### Mobile loader and release selection

The existing `tycode.dev/tyde/` loader remains responsible for selecting a
bundle matching the host release/protocol. Managed broker access does not
replace the release manifest contract in `29-mobile-web-release.md`.

The loader/scan path may carry an opaque `tyde-pair://` payload in the fragment
so the browser does not send QR secrets to the static origin. Any new QR version
must keep that property.

### Host settings

Host settings may expose server-owned mobile access controls, but they must not
become an account/billing store. In particular:

- No Tyggs account id, pass state, OAuth token, proof, or paywall metadata in
  host settings.
- No persisted production custom broker URL that bypasses `tycode.dev`.
- No frontend-owned setting that changes mobile access without a server event.

When a user writes `mobile_broker_url`, the server must accept only explicit
loopback dev/test broker URLs or `null`. Non-loopback/public URLs are rejected
as invalid settings. Legacy stores that already contain such a URL may still
load so startup can emit a typed invalid-config/repair state instead of
bricking the host.

---

## Rollout and migration

1. **Contract first.** Land this document and cross-references before code.
2. **Build `tycode.dev` service API.** Implement the HTTP contract with a mock
   broker signer first, then AWS IoT signing/authorizer in staging.
3. **Add Rust protocol types.** Extend `protocol/src/types.rs` for managed
   broker states and errors. Regenerate consumers from the Rust source of truth.
4. **Integrate Tyde server.** Replace production public-broker setup with
   `tycode.dev` offer and credential calls. Keep dev/test overrides explicit.
5. **Integrate mobile web.** Add Tyggs OAuth/pass-proof session exchange before
   pairing redeem. Add the `pass_required` splash/paywall state.
6. **Migrate local stores fail-closed.** Detect old records by missing
   `pairing_id`, missing `host_pairing_secret`, old store version, anonymous
   broker auth, or known public broker endpoints. Emit repair-required state and
   require re-pair.
7. **Release mobile web and desktop together.** Use the release manifest rules
   in `29-mobile-web-release.md` so the mobile bundle and host protocol match.
8. **Monitor before counters.** Use AWS IoT metrics, CloudWatch alarms,
   dashboards, rate limits, and AWS Budgets. Do not add application usage
   counters in MVP.

Migration must be one-way and explicit. A user with an old public-broker pairing
sees a repair/re-pair prompt; the host does not attempt the old broker in the
background.

---

## Threat model and privacy

### Threats

- A copied QR code is redeemed by the wrong device.
- A stale or replayed offer is redeemed after expiry.
- A stolen broker credential is used with a different MQTT client id or topic.
- A host publishes as mobile, or mobile publishes as host.
- A legacy public-broker record bypasses managed authorization.
- Tyggs OAuth/pass proof leaks to the host, AWS, logs, or QR payloads.
- `tycode.dev` or AWS logs accidentally capture raw pairing secrets.
- The mobile app infers product-specific billing state instead of only generic
  pass ownership.
- A revoked pairing continues to connect because of authorizer cache TTL.

### Required mitigations

- QR offer secrets are one-time, high entropy, and short-lived.
- Offer redemption is atomic and pass-gated.
- Host and device pairing secrets are distinct and role-bound.
- Broker grants are short-lived, signed, revocable, and scoped to exact client
  id, role, and topic namespace.
- Tyde fails closed for legacy/public broker records.
- Tyggs tokens/proofs are accepted only by `tycode.dev` and excluded from host
  stores, broker credentials, QR payloads, and logs.
- Service logs redact offer secrets, pairing secrets, broker grant tokens,
  OAuth tokens, and pass proofs.
- Authorizer cache TTL is bounded by grant lifetime and revocation policy.
- The mobile UI handles only generic pass ownership states:
  `active`/`pass_required`/`auth_failed`.

### Privacy notes

- Tyggs sees generic OAuth/pass activity, not Tyde topics or host/device
  internals.
- `tycode.dev` sees pairing metadata, hashed Tyggs subject, device labels, and
  broker credential grants.
- AWS sees MQTT metadata: client id, topic names, connection timing, and payload
  sizes. It should not see plaintext Tyde protocol frames.
- The Tyde host sees paired device labels and Tyde protocol traffic, not Tyggs
  account tokens or pass proofs.

---

## Test plan

### `tycode.dev` service tests

- `POST /auth/session` accepts valid generic Tyggs pass proof and rejects
  invalid/expired proof.
- Missing pass returns `402 pass_required` with paywall URL and no pairing side
  effects.
- Host offer creation returns only offer-scoped broker credentials and no Tyggs
  data.
- Redeem requires a mobile session and consumes the offer exactly once.
- Concurrent redeem attempts produce one success and deterministic
  `offer_already_redeemed` failures.
- Broker credential minting requires the correct role secret and rejects body,
  role, path, or nonce tampering.
- Revoked and repair-required pairings cannot mint credentials.
- Logs and audit rows redact all tokens/secrets.

### AWS authorizer tests

- Valid host/mobile grants produce policies for only their exact client id and
  role-specific topic direction.
- Wrong `clientId`, wrong role, wrong namespace, expired `exp`, future `nbf`,
  bad signature, unknown `kid`, and revoked `jti` all deny.
- Offer-scoped grants cannot access pairing namespaces.
- Mobile grants cannot publish to `host-to-client`; host grants cannot publish
  to `client-to-host`.

### Tyde server/protocol tests

- Managed broker states are emitted as typed protocol events from the server.
- UI receives state through the normal host stream; no frontend-only billing or
  broker state is introduced.
- Pairing start calls the `tycode.dev` mock and surfaces offer, active, expired,
  failed, and repair-required states.
- Stored legacy public-broker records become repair-required and never connect.
- Production config with unavailable `tycode.dev` emits error; it does not fall
  back to `DEFAULT_MOBILE_MQTT_BROKER_URL` or any public broker.
- Host stores no Tyggs token/proof fields.

### Mobile web/PWA tests

- Scan/preview works before Tyggs login.
- Redeem is blocked until `POST /auth/session` succeeds.
- `pass_required` renders the splash/paywall link and does not call redeem.
- Successful redeem stores only mobile pairing/device credentials needed for
  reconnect.
- Version/protocol mismatch follows the loader repair path from
  `29-mobile-web-release.md`.

### Release and migration checks

- Release checks verify mobile web bundle/protocol coherence as before.
- Migration tests seed old public-broker host and mobile records and assert
  repair-required UI state.
- No test introduces real Tyggs, real AWS, or real-money backend calls unless a
  separate opt-in integration test is explicitly approved.
