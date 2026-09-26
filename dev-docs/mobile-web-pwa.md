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

### Bottom-inset regression: physical evidence

The beta.5 migration inherited the library's `max(10px, safe-bottom)` bottom
margin instead of Tyde's `max(8px, safe-bottom - 8px)`. Tyde now supplies its
original spacing through `--tws-bottom-gap`; the shared shell still owns all
viewport measurement, keyboard detection, compact layout and clearance. The
44px controls, default status bar and loader version-pinning policy are unchanged.
The inline app-surface DOM flow pins composer and tab proximity at both 0px and
34px safe-bottom inputs, instead of accepting any bottom above the viewport edge.
It failed against the old styles with `expected=26, actual=34`.

A bounded AWS Device Farm comparison on iPhone 12 / iOS 26.6 verified real
Home Screen launch. Tab measurements executed CDN-downloaded, SRI-verified
beta.4/beta.10 release WASM with an isolated offline synthetic host. Composer
measurements used exact-tag `ui-fixtures` builds with the same app CSS. Release
and fixture tab geometry agreed. Private HTTPS documents preserved loader CSS
and each version's metadata, but did not exercise production-origin pairing,
CSP, service workers or loader selection. No production account was used.

Both versions reported a 797px client/visual viewport on an 844px screen.
The fresh beta.4 legacy-translucent icon put that document at native y=0, with
47px safe-top and a 26px capsule inset: its physical bottom gap was 73px.
A fresh beta.10 standard-status icon put the document at native y=47, with
zero safe-top and a 34px inset: its physical gap was 34px. Native textarea
rectangles independently confirmed the coordinate origins.

Loading beta.10 in the same installed beta.4 icon, without reinstalling,
retained native y=0 and 47px safe-top despite the new document's `default`
status-bar metadata. Its capsule bottom moved up 8px (physical gap 81px), and
its top moved up 16px because the capsule also grew from 48px to 56px tall.
Restoring the inset repairs that 8px regression; it does **not** claim to remove
iOS's separate retained 47px installation strip. Expanding the shell to
`screen.height` would violate its 797px paintable cap, so no such recovery is
added. Keep `status-bar-style=default`; fresh-install evidence supports it.

Served beta.9 and beta.10 app CSS, shell CSS and shell JS were byte-identical.
The manifest protocol version changed from 63 to 64 at beta.10. The loader
retains the saved bundle until incompatible-protocol repair; a pre-shell bundle
can therefore survive through beta.9 and switch at beta.10. The exact prior
bundle stored on the user's phone was not observed.

The first two allocations completed all 11 observations each. The third retained
seven observations, including the legacy-icon upgrade, but failed its subsequent
native icon-relaunch lookup; that relaunch is not a pass. iOS 26.6 did not expose
native keyboard accessibility bounds, so screenshots and actual keyboard-driven
viewport contraction are not a native-AX keyboard-suite pass. Exact-owner
hardware/OS, cold relaunch and existing-origin service-worker upgrades remain
separate acceptance gaps. Full measurements, hashes and original screenshots
are retained on the dev server under
`/home/tyggs/Tyde/mobile-bottom-evidence/`.

The fourth allocation passed all 14 observations, including an unchanged-to-fixed
CSS switch within the legacy icon. Closed and dismissed composer/tab gaps were
26px; the real keyboard-open gap stayed 10px. Shell height remained 797px closed
and 468px with the keyboard open; the client cap stayed 797px. Capsule/input
heights stayed 56px/44px. The physical legacy-icon gap returned from 81px to
73px, not to 26px: the retained 47px status-bar strip is explicitly unresolved.
All temporary cloud resources were deleted after four allocations (42.81
reported device-minutes; trial balance 809.08 to 799.52).

The repository gate also exposed an unrelated descriptor-exhaustion test race:
`WatcherInitialize` exhausted the process descriptors while initial review-store
creation still needed a temporary file, so no `ProjectBootstrap` arrived. The
existing real-server flow now gates that injection on the received bootstrap;
it still creates real descriptor exhaustion at watcher initialization and keeps
every original error/recovery assertion. No production server behavior or test
timeout was changed.

A subsequent native run reached all 484 cases but timed out before the
retired-backend fork test's initial `HostBootstrap`. Unlike the shared fixture,
that case spawned a host with live CLI/model discovery enabled; host registration
awaits that unrelated discovery before bootstrap. It now uses the fixture's
existing discovery-disable flag while retaining the real provider factory and
all unsupported-fork/source-session assertions. This is server protocol coverage,
not provider-discovery conformance. Production code and timeouts are unchanged.

The drawer DOM flow also stalled with its reveal animation at time zero. A
visibility assertion reproduced the fixture error: the capsule was at
1369.53–1425.53px in a 437px viewport, below the shared test document's visible
area. The flow now anchors its fixture to the viewport bottom, keeps that new
visibility assertion, and retains every existing transition/geometry check and
timeout. No production animation or layout behavior was changed for this case.

## Exact selected-host release synchronization

The mobile dispatcher consumes the typed WelcomePayload.release_version on
the current host stream, even when protocol numbers are compatible.
RejectPayload.release_version enters the same selected-host coordinator.
There is no protocol bump: the existing optional, prerelease-capable field is
the authority; TYDE_VERSION and the mobile crate's package version are not
bundle identities. The loader exposes the target actually executing.

Selection is restored before frontend-attach replay. Only a single paired host
may be automatically selected; multiple hosts without a valid saved selection
show the picker. A background Welcome/rejection never changes that choice.
Live Welcome authority is invalidated on disconnect, replacement, reconnect,
or protocol failure. A terminal incompatible rejection retains the existing
sticky verdict after its rejected socket closes, without reviving its stream
or connection runtime; selecting that host can repair the incompatibility.

For a differing release, the loader fetches fresh manifest policy, checks exact
release/protocol metadata, and verifies all executable SRI before asking for a
reload. Preparation leaves the running UI mounted. Selection, stream authority,
operation generation and safety are checked again immediately before committing.
Superseded fetches are aborted. Browser failures have typed operation outcomes
and a persistent banner; they are not invented host business state. Missing
release metadata, unpublished builds, policy denial, protocol drift, SRI,
storage and bridge failures never select latest as a substitute.

Automatic commit is deliberately conservative: only workspace Home, with no
mounted chat/editor, open project/settings sheet, text draft, draft settings,
pending/recovery submission, voice activity/recovery, shell error or boot
pairing/auth handoff. Empty global text alone is insufficient: the composer
owns attachments and queue edits locally. No DOM scraping, forced blur or
unmount makes a switch safe. The banner asks the user to finish/save their work
and return Home; this change neither changes draft ownership nor touches the
composer, keyboard, shared shell or send path.

The selected identity and exact handoff must be persisted successfully before
reload. A confirmed matching Welcome also records the exact target, so later
revocation cannot silently replace it with latest. Cold starts still fetch the
network-only manifest and fail closed offline. A running disconnected client
does not reload from an old Welcome; existing disconnect/recovery behavior
remains unchanged. First launch without host truth bootstraps from the manifest.
Repeated switches to the same host/target/protocol are session-bounded; a
confirmed match resets the guard, and deliberate A > B > A selection is not
subject to a lifetime reload quota.

### One-time bootstrap for existing installations

The independently updated, unversioned root loader can acquire this behavior
without clearing pairings or requiring a protocol incompatibility. Its
tyde.loader.follow-host.v1 migration ignores an old remembered pin once and
boots the latest **capable** allowed manifest entry. The release generator
stamps followsSelectedHost: 1 only when that release's built HTML declares
tyde-follow-selected-host capability. Running new tooling against historical
artifacts does not mark them capable. No support floor is raised, and no
published historical executable is changed.

Publication ordering matters: the capable immutable artifacts and their
manifest entry must exist before migration activates. Updating only the root
loader leaves existing pins alone. After a successful capable boot the marker
is durable. An exact legacy QR/rejection repair before capability publication
retains its target but does not complete migration: on a later cold navigation
it remains eligible for the first capable bootstrap. A selected-host-owned
target takes precedence; intentionally switching to an allowed older host
after migration does not trigger
the migration again on the next cold navigation. Exact-target failures retain
the target rather than booting latest. Completion is written only after a
successful boot whose fresh manifest entry declares the capability, never by
preparation, commit, confirmation, or an incapable exact repair.

The coordinator records a selected host's typed release before waiting for a
safe reload. Thus a cold navigation during an A-to-B deferral targets B, not A;
recording authority itself never discards the running draft or mounts a bundle.
Selection changes retire the previous owner's target and remembered pin. Cold
startup also checks ownership against the persisted selection and readable
IndexedDB host identities, so forgetting A with B/C remaining can bootstrap
the picker even if A is now revoked. Unknown storage cannot prove a host was
forgotten. A still-selected known target remains fail-closed under the fresh
manifest, including while switching is deferred. Exact boot preserves the
owner rather than rewriting an anonymous target.

The worker's network-first shell path picks up new loader code on navigation.
Worker activation/foreground resume alone does **not** replace an already
executing immutable WASM app. An installation still running old code therefore
needs its next real navigation/cold launch (iOS may retain a resumed document
until the PWA is closed). No storage clearing, re-pairing, protocol bump or
reinstall is part of this migration.

There is an unavoidable historical-client limit: selecting an immutable release
from before this coordinator executes that historical client's behavior.
It cannot learn selected-host switching retroactively, and its old rejection
repair can still act without selection awareness. The root loader preserves
that explicit older choice instead of repeatedly forcing a newer client.
Likewise, release identity is what the host advertises; a locally modified
binary advertising an existing published release is indistinguishable from
that release under the current handshake. Missing or genuinely unpublished
release identities are surfaced, not guessed.

### Evidence and remaining device acceptance

The canonical check mounts the real mobile surface and drives dispatch,
selection, deferred/unavailable banners, draft retention, delayed browser
preparation and reload commitment. Its loader suite also drives real headless
Chrome through HTTP, IndexedDB, CacheStorage and service-worker lifecycle:
an existing stored pin, independent root update before capability publication,
pre-publication legacy repair, one-time migration, retained pairing/key stores,
exact older-target switching, deferred A-to-B authority and forgotten-owner
invalidation with two remaining pairings,
repeat navigation, revocation, offline policy, SRI/protocol failures, storage
failure and bounded retries. It invokes the real release-manifest generator on
immutable artifact fixtures and checks that historical fixtures remain unmarked.

The loader fixture executes small SRI-verified JS modules and a hashed WASM
artifact, not a released Tyde WASM application or real phone pairing. The
mounted Rust suite covers the app half separately. These are not an iPhone
standalone acceptance claim. The existing Device Farm package replaces the
loader/CSP and its supervisor detaches the runner; it cannot certify this
storage/worker path unchanged. No device allocation, paid backend call,
production publication, release or tag is needed for these local checks.

The canonical gate also exposed two pre-existing server-sim ordering races.
The watcher flow observed unchanged contents at version 1 after its baseline
read returned version 0: the baseline preceded watcher initialization. It now
reads after the already-required watcher-ready listing, retaining the original
contents/version equality assertions. The plan-approval flow observed
awaiting_user when its independent HTTP query beat the protocol approval write.
It now observes the already-required completion, approved response and final
idle events before making the original HTTP idle assertion. The adjacent gated
scenario still checks that await stays active during approval execution. These
are fixture-ordering corrections, not server/provider behavior changes or
weakened assertions; the new ordering diagnostic remains privacy-safe.
