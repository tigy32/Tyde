# Mobile push notifications

The mobile client is a website (a PWA served from `tycode.dev/tyde/`), and it
receives Web Push. This documents how, and — as importantly — what it
deliberately does not do.

## The shape

The host is the only party that knows an agent went idle, and it is the party
that encrypts and delivers the notification. `tycode.dev` is not involved: it
mints no keys, stores no subscriptions, and never sees notification content.

```
agent turn ends
  -> AgentStatusHandle::update computes the status edge      (server/src/agent/registry.rs)
  -> AgentStatusTransition broadcast
  -> mobile access actor's idle notifier                     (server/src/mobile_access.rs)
  -> RFC 8291 encrypt + RFC 8292 VAPID sign                  (server/src/mobile_push.rs)
  -> HTTPS POST to the device's push endpoint (Apple / Google)
  -> browser decrypts, service worker shows it               (web/loader/sw.js)
```

## Locked decisions

### 1. The device owns the VAPID key pair, and shares it with every host

A browser allows **one push subscription per service worker registration**,
bound to a **single application server key**. If each host minted its own VAPID
key, a phone paired to two hosts could only ever be notified by one of them —
and Tyde has first-class multi-host support (`dev-docs/12-remote-hosts.md`).

So the device generates one P-256 key pair (`mobile-frontend/src/push.rs`) and
hands the private half to each paired host over the already-encrypted pairing
channel. All hosts sign with the same key, so one subscription serves all of
them.

This grants a paired host the ability to push notifications to the device. That
is not an escalation: a paired host already holds the PSK for the end-to-end
channel and can send arbitrary protocol frames.

### 2. Content stays end-to-end encrypted, without inventing a scheme

The host encrypts under the subscription's own `p256dh`/`auth` keys, so Apple
and Google relay ciphertext they cannot read; the browser decrypts and hands the
service worker plaintext. No second encryption layer was needed, because no
third party was put in the content path in the first place.

### 3. Suppression is state, not a timer

Two rules decide whether an idle edge becomes a notification, and both read real
state — there is no debounce window (see the "no timeouts over local state"
rule):

- **The device is currently connected** (`connected_tasks` in the mobile access
  actor). The app is open and already showing the agent; a push would be noise.
- **The agent has messages queued behind the turn**
  (`AgentStatus::has_queued_messages`). It resumes immediately, so `Idle` here
  means "between turns", not "finished".

The second rule is why `has_queued_messages` was added to `AgentStatus`.
`dispatch_queued_message` marks the turn active *after* the previous turn has
already ended, so there is a genuine transient `Idle` between them. A debounce
window would have papered over that; the flag describes it.

A third rule skips agents restored from a saved session that have not run a live
turn (`restored_without_live_turn`), so reopening history does not buzz the
phone.

### 3a. Only agents the user started

`AgentOrigin` decides, as a `match` so a new variant forces the question:
`User` and `SideQuestion` notify; `AgentControl`, `BackendNative`, `TeamMember`,
and `Workflow` do not. Those have a parent agent waiting on them, not a human —
a workflow fanning out to a dozen sub-agents would otherwise buzz the phone a
dozen times.

### 3b. Mobile access off means no notifications

A host with `enable_mobile_connections` off cannot be reached by any device, so
a notification would open an app that cannot connect. The notifier returns early
rather than delivering a dead end.

### 4. The status edge is typed, not reconstructed

`subscribe_agent_status_changes` returns a bare `watch::Receiver<u64>` — a
change counter. Detecting a Thinking -> Idle edge from it would mean the notifier
keeping its own mirror of every agent's last status, which is the derived-cache
smell `dev-docs/01-philosophy.md` §6/§7 rejects. Instead the registry, which
already owns the status and computes it, emits a typed
`AgentStatusTransition`. The notifier holds nothing.

### 5. The subscription is bound to the authenticated device

`ConnectionOrigin::Mobile { device_id }` carries the device the transport
authenticated as. `MobilePushSubscribe` is bound to *that* device; a client
cannot name another. The frame is rejected outright on a desktop connection.

### 6. Rotation self-heals on connect

Push services expire subscriptions. iOS does not reliably fire
`pushsubscriptionchange`, and a service worker has no route back to the host
anyway (the MQTT session lives in the page's WASM). So the page reads
`getSubscription()` on **every** connect and re-sends it unconditionally —
cheap, idempotent, and it removes the dependence on an event that may never
arrive.

When a push service answers `404`/`410`, the device is marked
`MobilePushState::Expired` and that reaches the desktop device list. A dead
subscription says so rather than silently delivering nothing.

## What this does not do

- **No deep link.** Tapping a notification focuses or opens the app; it does not
  navigate to the agent, because the mobile app has no agent-level URL routing
  yet. The `agent_id` is carried in the notification's `data` and posted to the
  focused client, ready for when it does.
- **No per-agent or per-reason preferences.** Every qualifying idle edge
  notifies every subscribed, disconnected device.
- **No quiet hours or rate limiting.**
- **iOS requires installation.** Web Push does not work in a Safari tab; the app
  must be added to the Home Screen. The settings section says so rather than
  offering a control that cannot work.

## Testing

`server/tests/mobile_push.rs` drives the real server over the real protocol from
a real mobile-origin connection: it registers a subscription whose endpoint
points at an HTTP server the test runs, completes a turn on the mock backend,
then **decrypts the captured body with the subscription's private key** and
asserts the notification content, the `aes128gcm` content encoding, and that the
VAPID authorization names the right key. A second test asserts a `410` reaches
the device list as `Expired`. A third spawns a child through the real
agent-control MCP and asserts it does *not* notify, paired with a user-started
spawn as the positive control — without which the test would also pass if pushes
were broken outright.

The endpoint is client-supplied data, so nothing in production branches for the
test.

**What the tests do not establish:** both the encrypting and the decrypting side
of the payload are code in this repository, so the crypto tests prove
self-consistency, not RFC conformance. A shared misreading of RFC 8291 would pass
them. Only delivery to a real browser proves interop — see below.

**Not yet run on a real device.** No iPhone or Android browser has received a
notification from this code. The encryption, the VAPID signature, and the service
worker handlers are all exercised only against this repository's own
implementations.

**Known coverage gap:** suppression-while-connected is not covered by an
automated test. Registering a subscription needs a mobile-origin connection, and
the actor only records a device in `connected_tasks` when *it* dials out over
MQTT — which a sim test cannot do. Reaching it would need a test-only hook in
production code, which is not allowed. The rule reads the same map that the
device list already renders `Connected` from.
