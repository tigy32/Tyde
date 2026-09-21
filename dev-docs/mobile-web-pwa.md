# Tyde mobile web/PWA

> Status: shipped as the versioned web/PWA client. The native iOS shell,
> root tooling, and obsolete production Tauri bridge have been removed.

## Context

Tyde previously contained a native iOS mobile client; external distribution is
unconfirmed. Native packaging coupled client releases to Apple review and
forced client and host to upgrade in lockstep. Tyde now serves the mobile client
as a **website / PWA** ("Add to Home Screen"). A thin, stable
loader page learns the paired host's release version and boots the matching
**versioned static bundle** (e.g. `tycode.dev/tyde/v0.8.19-beta.2/...`), so the
client protocol always matches the host. This decouples client releases from
App Store review.

## Mobile outbound write and recovery contract

Queue acceptance is not delivery. The browser bridge admits one encoded host
line through the existing bounded connection queue and returns
`Result<Accepted, SendRejected>`. `Accepted` identifies the connection instance
and an opaque client-local submission. It means only that the line entered that
connection's local queue. Callers never wait for a transport acknowledgement, and
the writer has no per-send reply oneshot.

The bounded data queue and connection control path are separate. Stop and typed
connection invalidation use a priority cancellation signal, so a full data
queue cannot prevent session teardown. Sequence or protocol invalidation
terminates the affected connected session and reconnects through the normal
backoff; it is not represented as a user Stop.

The transport exposes exactly four facts:

- `QueuedLocally`: admitted but not yet dequeued by the writer.
- `NotSent`: the session ended while the line was still queued, before dequeue.
- `TransportAcknowledged`: the transport completed the serialized write and flush
  for that line.
- `DeliveryUnknown`: the writer dequeued the line, then the session ended,
  failed, or timed out before `TransportAcknowledged`.

**A transport acknowledgement is never called delivered. `TransportAcknowledged` makes no claim that
the host received or applied the frame.** The boundary never correlates a
transport fact with a later server event and never infers server ownership or
application from it.

The mobile writer serializes exactly one logical line per write and flush. It
must not batch. That constraint is what makes one completed flush attributable
to one `TransportAcknowledged` submission through the byte transport.
`NotSent` is valid only before writer dequeue; every failure after dequeue is
conservatively `DeliveryUnknown`.

Writer work has a connection-liveness deadline. An I/O failure or deadline
cancels the entire connected session, classifies the dequeued and queued work,
emits the existing typed failure/disconnect status, and reconnects through the
existing `ReconnectBackoff`. Work from the dead connection is dropped and
is never replayed on the next connection. There is no second retry loop and no
automatic resend of user intent.

The UI recovery model follows those facts. On `Accepted`, submitted input moves
atomically from the composer into a client-owned pending record. Queued and
in-flight records remain silent; `TransportAcknowledged` retires them silently.
`NotSent` and `DeliveryUnknown` surface recovery, with ambiguous submissions
persisting until explicit dismissal. Resend is always deliberate. New-chat
recovery is host-scoped because no agent exists yet and ownership must not be
guessed.

Mobile is the only `AgentReplayMode::Lazy` client. `LoadAgent` followed by the
authoritative `AgentBootstrap` is the sole gate that marks mobile chat content
loaded; transport or protocol failures must therefore terminate that loading
state visibly rather than leave it pending.
Because nothing is attached before `LoadAgent`, the agent list's running/idle
state comes from `NewAgentPayload::turn_active` in `HostBootstrap`/`NewAgent`
and from `AgentTurnStateNotify` on the host stream for unattached agents (see
`21-bootstrap-streams.md`).

## PSK storage

The phone stores durable host identities in IndexedDB and keeps PSKs in a
separate store behind `PskStore`. The current implementation stores raw key
bytes encoded as base64; non-extractable WebCrypto storage remains future work.
Host summaries contain key IDs and fingerprints, never PSKs. Obsolete broker
fields in existing records are ignored without changing pairing identity.
Temporary relay/signaling grants are never persisted.

The versioned loader pins all executable artifacts with integrity hashes and
uses a strict same-origin script policy. XSS on this origin can act as the
paired phone, so third-party scripts must stay excluded. QR secrets stay in
fragments and are cleared before external navigation. See `web/README.md` for
loader integrity, release selection, CSP and deployment requirements.

## Transport and recovery

Managed traffic uses the relay-only WebRTC transport described in
`mobile-webrtc-turn.md`. Direct self-hosted pairing uses `/tyde/ws`. There is no
MQTT implementation or transport fallback. Mobile fetch deadlines, foreground
recovery and the manual Reconnect control cancel stalled attempts. Production
behavior is covered through real DOM, service HTTP and TURN/server boundaries
in `./dev.sh check`.

## Shared mobile layout

The Rust/Leptos UI imports Tyggs Web Shell through `wasm-bindgen`; it does not
copy the viewport algorithm. The pinned revision is
`b7b25b117b379c49e0419c612e3a8fbeef060241`, from Tychat's checked-in archive.
`mobile-frontend/vendor/web-shell/PROVENANCE.md` records its checksum. The
unmodified JS is a local wasm-bindgen module, so Trunk includes it under each
versioned bundle and the existing executable-integrity manifest covers it.
The shared CSS loads before Tyde's appearance styles.

`mobile-frontend/src/shell.rs` attaches one document owner after mounting and
rebinds only when navigation replaces its header, scroller, or bottom region.
Component cleanup disconnects the navigation observer and destroys the owner.
Keyboard changes never remount the textarea. The shared textarea helper owns
composer sizing; programmatic draft updates synchronize the native field before
notifying it. Queued-message editing retains its separate existing behavior.

The shared shell owns viewport geometry, safe areas, covered tabs, and measured
composer priority. Tyde supplies nested pane layout and glass appearance.
Measured start/end clearance is applied once, to the scroller's inner flow,
not to the scroller's own box. An empty pending-submission surface renders no
layout item. `followEnd: false` leaves transcript history anchoring with Tyde.
There is no legacy app-height controller or SafeArea wrapper alongside it.

The app-surface browser test exercises real mounted components through iframe
resizes (including 51px), draft/focus/selection retention, end clearance,
reading-history anchoring, all tabs, compact tab coverage/restoration, and
unmount cleanup. Existing composer, queued-editor, and transcript tests remain
part of `./dev.sh check`. The migration exposed and fixed programmatic draft
notification ordering, queued-editor CSS inheritance, and a compact footer gap
from an empty pending-submission element. These are DOM integration checks,
not evidence of native keyboard or existing-installation acceptance.

### Existing-installation acceptance still required

The document uses zoom-enabled viewport metadata with
`interactive-widget=resizes-content`, `viewport-fit=cover`, and the standard
`apple-mobile-web-app-status-bar-style=default`. The loader cache revision is
advanced; IndexedDB, pairing identities, bundle selection, and transport are
unchanged. Do not clear storage, unregister the worker, or reinstall to test it.

After the approved release is deployed, open the existing Home Screen icon and
verify the matching host/bundle version, retained pairings, and metadata update.
Exercise alphabet/emoji/search transitions without blurring, multiline drafts,
dismissal, history reading, rotation, background/resume, and lock/unlock. Check
visible pixels and touch reachability, including covered/restored tabs and the
absence of an extra message gap. Record hardware, OS, browser, and release.
Library device coverage does not certify Tyde's integration. Intermittent
startup/storage failures and Tychat's separate spacing deployment are not
claimed resolved by this migration.


### Completed-tap focus dependency update

The pinned shared-shell revision lets the browser focus an editable field after
its native tap completes. It removes capture-phase pointerdown focus, which
could start viewport movement during contact and lose focus on finger release.
Keyboard measurement, CSS, Rust bindings, draft ownership and explicit send
refocus are unchanged. Do not add a Tyde-only focus workaround.

The upstream regression reproduces the focus loss with trusted Chromium touch
and a real viewport contraction, and passes six reopen cycles after removal.
Its library checks pass 346 browser cases and three Rust/WASM cases. Four native
iOS 26.5 PWA contacts retain focus until intentional dismissal, but keyboard
bounds were not exposed to accessibility, so that farm run is not a full geometry
pass. Tyde's canonical workbench and clean-main gates remain required; a source
pin update is not a deployed Tyde release or exact-device acceptance.
