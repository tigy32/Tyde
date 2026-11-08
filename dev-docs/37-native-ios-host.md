# Native iPhone/iPad host

Tyde's native app is a remote client, not a phone-hosted server or agent CLI.
`ios/Tyde.xcodeproj` is a dedicated UIKit host. The existing desktop Tauri
application is not an iOS target. Native APNs and a Swift rewrite of the mobile
screens are not part of this feature.

## Historical early TestFlight preview

The following describes the earlier bundled-client preview, not authorization
to distribute the thin-shell candidate below.

The user authorized an early preview with known gaps while native QA continues;
complete chat/four-layout acceptance is not a preview-publication prerequisite.
This does not mark any historical RED, core chat or the whole feature passed.
Canonical committed-source validation and configured review still apply. The
Lander owns source landing; the distribution owner owns the Apple app record,
signing, device Release archive/export, compliance and TestFlight availability.

The user approves matching `v0.9.6-beta.19` host and TestFlight builds. Keep
protocol95: native New/Fork/Resume requires its connection-owned creation stream
and server publication event, which the released beta18/protocol94 host lacks.
The native client also requires the host's exact packaged release. This preview
therefore requires the user's host to update to the matching beta19; it does not
support beta18 and must fail explicitly rather than downgrade or infer creation.
The Lander lands this feature and publishes beta19 from validated main under the
release workflow. The distribution owner then generates fresh native assets from
that exact beta19 main commit before archiving. Frozen beta16 QA bundles are
evidence only, never shipping assets. No source-owner tag or upload is implied.

Release uses the registered `dev.tycode.tyde` on the existing macOS Tyde team
`743QY8VN34`.
Debug retains `dev.tyde.mobile.nativeqa`, its existing data and its independent
Keychain service; the preview is a separate install, not a QA-data migration.
No additional entitlements, Associated Domains or push capability are requested.
The marketing version comes from the existing generated release metadata;
the Xcode app target owns its build number (`CURRENT_PROJECT_VERSION`). Apple
rejected the 0.9.6 build 1 upload because the marketing icon had an alpha
channel (90717). The next Release upload of this same packaged 0.9.6-beta.19
source is build 2. Debug stays build 1. Allocate a new tracked build number for
each subsequent upload; asset packaging must not silently reset it to 1. The
App Store icon is the opaque 1024 RGB `AppIcon.png` and must not gain an alpha
channel. Do not let an
export tool rewrite the reviewed version/build identity.

Known preview limits: normal pairing/bootstrap passed on the retained iPad with
explicit user Settings Allow; the default-consent failure remains unresolved.
Native modal exclusion/restoration is proved there. Full keyboard input,
retention/normal Send/response, remaining major screens and four genuine Duo
layouts remain unproved. Native84/85 stay RED; Source48 only corrects premature
test synchronization and does not establish the cause of missing edits. Preserve
these limits in the TestFlight test notes rather than calling the preview ready
for production. This documentation is not proof of an accepted/signed archive.

The app implements pairing/transport cryptography beyond merely using HTTPS;
do not declare encryption absent or exempt from that alone. The distribution
owner must complete Apple's export-compliance assessment and any required
documentation. Privacy/account declarations and Apple's processed build status
remain distribution checks, not inferred source/test success.

## Approved thin-shell preview build 4

The user approved an updated private TestFlight preview and the
`v0.9.6-beta.25` host/bootstrap rollout. This supersedes the bundled-client
preview above, not its historical QA limitations. Release build 4 is tracked in
`ios/Tyde.xcodeproj/project.pbxproj`; Debug remains build 1. The existing ASC app
is Tyde Agent Studio (`6829602646`), bundle `dev.tycode.tyde`, team `743QY8VN34`.
A read-only ASC API check on 2026-11-04 at 02:56:55 UTC returned only builds 2
and 3, both VALID; build 4 was unallocated. Recheck before upload and stop if
another upload has allocated it.

The source baseline is beta25 commit `9e3a6cc0`, including the integrated
thin-shell/ABI6 source landed in `5eb3af98`. The Lander must land this bookkeeping
commit and pass the clean-main gate before distribution. Generate fresh
production resources from that landed source with `./dev.sh ios-bundle`, without
`--qa-origin`. The entry remains `https://tycode.dev/tyde/?native=1`; do not ship
legacy bundled-client assets, a frozen QA bundle or a QA-origin app. The installed
shell and authenticated host-selected full client retain independent release
identities; build 4 does not pin future hosts to beta25.

Archive the Tyde scheme's Release configuration with canonical marketing version
0.9.6 and build 4, without archive/export/IPA version or build-number overrides.
Export must preserve the reviewed identity. Source validation is not an accepted
or signed archive: production bootstrap/client publication, signing/profile,
archive resources and identity, export compliance, upload processing and actual
TestFlight availability remain the distribution owner's checks. The Lander owns
publication; the source owner does not archive, upload or deploy this change.

## Native HTTP signaling responses

The native fetch bridge preserves the actual request URL on its returned
`Response` and its clones. Native URLSession refuses redirects, so that URL
is authoritative. Wasm reqwest consumes this metadata during WebRTC signaling;
an empty synthetic response URL throws before normal connection error handling,
even when the HTTP request succeeded. PWA fetch is unchanged. The real TURN
browser flow exercises the shipped bridge and reqwest through an owned HTTP
platform fixture; it does not certify iOS URLSession, production auth or the
user's installed TestFlight connection. Updated bridge bytes require a new
native build, not replacement of an already published host client.

The preceding private preview tracked Release build 6 in the Xcode
project; Debug remained build 1. The read-only ASC query on 2026-11-04 at
21:11 UTC found builds 2–5 VALID, old build 1 awaiting upload, and no build 6
record or reservation. Build 5's archive-time override was a one-off, not the
route for build 6. Recheck allocation before upload; use the tracked identity
without archive-time overrides.

Build 6 carries native-response source `90f5754099f28f51b54dfc9c6f7ac5cdf2a9a3ef`
and bridge SHA256
`e2a99530db7daa8a56a081c6fde69dbe308f94a7c7a890fbe180d044adb77fa6`.
Distribution follows source landing, clean-main validation and push, using fresh
production resources from the final landed source. Existing beta26 and its
matched client suffice; no host restart, Service change or client republication
is required. The boundary proof above does not establish physical TestFlight
acceptance, the user's live error stack or ordinary native UI Send/response.

## Historical private preview build 7 preparation

At preparation, Release tracked build 7; Debug remained build 1. The GET-only
ASC preflight
on 2026-11-05 at 08:56:31 UTC found builds 2–6 VALID with uploads COMPLETE,
build 1 AWAITING_UPLOAD, and no build 7 record or reservation. This prepares
the next unused tracked identity; it does not reserve or upload it. Recheck
allocation before distribution and preserve the tracked identity without
archive/export overrides. Build 5's one-off override must not be reused.

Finished UI source `0e162fa1fb2b0901d0e7749e067aaf47f5823f24` is integrated
in checked/pushed main `bfc9e4b5c9ba987eca0dcb47a9e4d166b87cd1d4`.
This bookkeeping adds no product changes or unfinished WebContent recovery/crash
work. Changed Swift and `native-host.js` require a fresh native build from final
landed source with production resources. Hosted mobile assets containing the UI
must be published separately; a native archive alone does not update the
host-selected full client. No manual beta28 approval is held, and this source
preparation authorizes no cut, tag, deployment, host/device update or restart.

Protocol 97, native ABI 6, bootstrap 1, production origin, same-team identity,
entitlements and existing pairings remain unchanged. Source validation/review
do not establish physical crash recovery, keyboard or hold-interaction
acceptance, nor close the earlier native QA limits. The Lander owns clean-main
validation/push; the distribution owner owns signing, archive, compliance,
upload and private TestFlight availability after the required approvals.

## Historical private preview build 8 preparation

Release tracks build 8; Debug stays build 1. The attested GET-only ASC preflight
at `2026-11-05T19:35:16.043962Z` found builds 2–7 VALID/COMPLETE, old build 1
AWAITING_UPLOAD and no build 8 record or reservation. Identity remains app
`6829602646`, bundle `dev.tycode.tyde`, team `743QY8VN34`, private group
`0da66ed5-d509-48c1-9ffe-517772097422`. Build 7 from source `693aed6e` is
already AVAILABLE to existing A and VALID/COMPLETE
(delivery `23154306-e145-49ba-886d-a6f982a9a93b`);
preserve it without duplicate upload or reservation. Recheck allocation before
upload and stop if build 8 is taken. Use normal tracked metadata, never native
archive/export/build-number overrides.

Finished source base `120cddb11b96faea68a911758b62ff573acd2748` includes
SSH POSIX-shell launch guards (`120cddb1`) atop `c4efd78c`, with finished UI/
scroll (`bfc9e4b5`/`0e162fa1`), lifecycle recovery (`79581004`/`6e8097c8` source),
decoder/publication cache correction (`0152be11`/`2155a706` source), native
discovery error contract (`f03f72f2`) and registration wording (`c4efd78c`).
The discovery contract and wording change only shared loader and
mobile-frontend sources. Screenshot source `348733b0` is mapped unchanged to
`ca756889` atop this base: only three unasserted discovery captures and their
evidence plumbing are removed, with separate visual QA documented. All
remaining scenario and assertion bytes are preserved. Build 7
contains none of the Swift recovery. Changed Swift lifecycle and native bridge
recovery require a new native binary even though protocol 97, native ABI 6 and
bootstrap 1 remain unchanged; there is no release-version lock reason for 8.
This commit changes only Release build metadata and provenance, not product,
runtime, protocol, ABI or release versions.

After source landing, clean-main validation and push, generate fresh production
resources with `./dev.sh ios-bundle` from that exact landed source. Keep
`https://tycode.dev/tyde/?native=1`, without `--qa-origin`, old/frozen bundles or
resource substitution. Preserve exact source/archive identity, existing paired
records and Keychain data. A native archive does not publish the host-selected
full client. Signing, resource/identity inspection, compliance and processed
upload remain separate distribution checks, not source-validation claims.

Native empty-input loss/recovery passed only stable captures 08–10; transient
07 blank is an accepted known limitation, not a proved recovering affordance.
Nonempty text, photos, unresolved Send, no-replay and direct-handshake order
remain unproved natively; DOM/protocol proof is separate. No keyboard-crash or
tabs cause/fix, physical Send or full-app acceptance is claimed. The discovery
error contract and registration wording have HTTP/browser and mounted-banner
proof only; no native Retry proof or native foreground-incident cure is claimed.

The earlier metadata candidate `a9f74735` remains a historical gate FAIL;
`8f636f1c` passed only its earlier `0152be11` source. Neither is validation
of this composed source. Native reconnect, keyboard, tabs and incomplete
four-pose QA remain unresolved; no new native acceptance is claimed, and
four-pose completion is not an upload gate.

Standing authorization covers the normal private preview upload by its owner.
Immutable `v0.9.6-beta.28` at `9a181e3d` excludes SSH `120cddb1`, this
screenshot change and new metadata. No retag, new release, group change or A
activation is authorized here; existing A availability for 7 is preserved.
No account, Service, IAM, deployment, host/device update or restart is authorized.
The Lander alone owns landing/check/push and publisher steps; this bookkeeping
executor performs no Apple API, archive, upload or native QA operations.

## Current private preview build 9 preparation

Release tracks build 9; Debug remains build 1. The GET-only ASC catalog check
at `2026-11-07T13:55:01.717180+00:00` found builds 2–8 VALID/COMPLETE,
old build 1 AWAITING_UPLOAD, and no build 9 record or reservation. This does
not reserve 9; recheck allocation before distribution and stop if taken.
The existing TydeAgentStudio app is cataloged as Tyde Agent Studio
(`6829602646`), bundle `dev.tycode.tyde`, macOS team `743QY8VN34`.
The sole existing internal group, Private Preview (A), is
`0da66ed5-d509-48c1-9ffe-517772097422`; preserve its membership and availability.

Source base `23fe9c53` includes native status-dock actions (`f261a4c5`),
dismissible reconnect overlays (`32d0fc87`), live Back destinations
(`8170571a`) and native dock reconnect projection (`23fe9c53`). This commit
changes only Release build metadata and preparation provenance. It changes no
implementation, UI, protocol, ABI, bootstrap, marketing or host release version.
Existing native QA limitations remain; preparation is not native acceptance.

The Lander must land this bookkeeping commit, pass the clean-main gate and push
before build follow-up. PM's upload-versus-wait question remains unanswered:
no archive, upload or group enablement is authorized by this preparation.
Build/upload follow-up requires exact landed main source and explicit PM GO;
coordinate all Mac resources through PM and sole executor 1f. Do not run GUI,
global input, XCTest or runtime QA on air for this preparation.

From the exact checked landed source, generate fresh production resources with
`./dev.sh ios-bundle`, never `--qa-origin` or frozen/substituted resources.
Verify `tools/ios-source-manifest.py verify`, production bootstrap
`https://tycode.dev/tyde/?native=1`, generated marketing version 0.9.6,
tracked Release 9 and Debug 1. Archive the Tyde scheme using Release and
`generic/platform=iOS`, with the existing same-team distribution certificate
and valid App Store profile for `743QY8VN34.dev.tycode.tyde`. The distribution
owner must verify current signing prerequisites in its owned Mac session;
this preparation does not establish their present validity. Use normal archive
and export, no version/build overrides; export must set
`manageAppVersionAndBuildNumber` false. Verify signed archive and IPA identity,
source/resource hashes, arm64 iOS platform, matching dSYM, opaque marketing icon,
unchanged entitlements and export compliance before any approved upload.
A native archive does not publish the host-selected full client. No Git beta,
account/IAM/team/tester/agreement change, deployment, host/device update or
restart is authorized. Preserve credentials, tools, simulators and pairings.

## Adaptive Agents hierarchy

The approved Duo v4 composition keeps the Agents/project hierarchy on the left
only in regular-width landscape, with global tabs still owned by UIKit. Regular
portrait and compact layouts show one destination at a time. UIKit alone places
its bars, Back and system reservations; there are no web side-tabs, pose/edge
preferences, fake status indicators or Swift agent/status records.

`TydeHostController.reportLayout` projects the root view's current bounds and
horizontal size class on layout and trait changes. The typed native layout is
presentation input, independent of keyboard contraction, screen identity and
connection foreground recovery. The native sidebar action changes the one
Wasm-owned visibility choice; it hides redundant Back while the hierarchy is
visible, respects existing modal exclusion, and retains its choice across size
changes. Existing retained panes and the shared shell keep owning mounted
content, scrolling and keyboard geometry. Native ABI 6 adds these projections.

An iOS 27.1-or-newer shell separately declares `chatDockOwner` from UIKit's
actual `verticalBarEdge` trait: `shell` on the vertical axis, `client` on the
horizontal axis. Typed `chat_dock_shell`/`chat_dock_client` presentation intents
update that ownership on layout changes; they never navigate. Horizontal poses
keep one compact chat-local header, with latency at its action edge rather than
inside its title. There is no second native horizontal action row.
The client keeps the compact chat-local title and full-height untitled Agents
hierarchy, but transfers Back, sidebar, menu and healthy latency to UIKit.
`NativeChatDock` uses the navigation item's system Back action, leading groups
and system overflow, not a bottom toolbar or manually positioned rail. UIKit
places those controls below system status and above tabs on Duo's vertical
axis and manages safe areas and overflow as the device resizes. The compact
custom latency item explicitly permits vertical placement with `axisBehavior`.
It is a status readout, not a control, so it hides the shared glass background
and stacks its status dot above the value inside the 40pt slot. The shared
`NativeLatencyText` keeps the value to at most three digits plus a unit, also
for the root header; the accessible description retains exact milliseconds.
While a modal is open, the shared dock omits Back, leading groups and overflow;
disabling the legacy navigation bar cannot exclude the separate shared bar.
Closing the modal restores those controls from the same typed presentation.

The additive optional `chat_dock_owner`, `chat_connected_status` and
`chat_back_label` presentation fields acknowledge ownership and project the
existing selected-host heartbeat and canonical Back label. No title parsing,
new navigation action, route stack, gesture or WK attachment behavior is added.
Shells without this declaration retain their existing header ownership. Root
status and actionable connection-error/recovery surfaces are unchanged. The
mounted DOM navigation flow covers ownership, duplicate-control removal,
chat/sidebar geometry, live latency, retained editing and modal exclusion;
actual native placement, gesture behavior and fold/safe-area acceptance still
require the sole native GUI custodian and Root-approved original screenshots.

Opening an existing agent from Home selects the native Agents destination,
just like selecting its Agents row. Both entries use the same landscape
hierarchy availability and preserve the user's sidebar visibility choice.
Portrait and compact remain single-destination navigation; browser/PWA Home
and new-chat entry are unchanged. This is a hosted-client navigation change,
not a Swift binary or native ABI change.

The existing mounted-DOM flow covers sidebar open/close, portrait/compact
transitions, modal exclusion, filter/segment behavior and retained conversation,
draft selection and scroll. Its native boundary is deliberately a fixture; it
is not proof of actual UIKit resizing or Duo displays. Actual app screenshots,
Duo integrated-display routing/fold transitions and keyboard/safe-area geometry
remain acceptance work in an owned DeviceHub MCP session. DeviceHub connection
or capture failures establish apparatus limits, not product layout failures.

## Ownership

One retained WKWebView/document runs the mobile Rust/Wasm AppState, dispatcher,
connection manager and protocol. UIKit renders system tabs, navigation/Back and
agent toolbar actions. `mobile-shell-types/src/native.rs` declares presentation
and platform I/O types and generates the Swift bindings. UIKit receives no agent
records, transcripts, independent working/idle model or protocol command map.
Navigation intents return to the same Wasm state. Existing modal owners project
an interaction barrier to native tabs, Back and toolbars; UIKit does not inspect
the DOM or invent its own modal lifecycle. Native tab switches retain
mounted screens, draft/selection and scroll, instead of reloading documents.

Only native packaging suppresses global web navigation. Browser/PWA tabs, chat
Back, theme fonts/sizes, auth and executable release loading remain unchanged.
The native wide Agents hierarchy is local content, not another global tab rail.

Home's native Hosts toolbar action opens the existing host picker as a system
navigation detail with Back to Home. It retains the selected typed record while
browsing; the workspace stays mounted so Back and Open on the same host retain
component-owned drafts, photos, selection and scroll, including Pair another
host/Cancel before switching records. The one native auth-recovery surface is
mounted above host selection, so a failed/cancelled sign-in or Pass-required
result stays visible in an unselected multi-host chooser as well as a workspace.
Its explicit host identity and existing authentication flow are unchanged.
Open selects that exact row; switching
hosts retires the previous host's complete unsent composer, incompatible project/
chat selection, new-chat options and pending resume navigation, without deleting
its records or transcript. Late server events still update
that host's data but cannot reopen it over an explicitly selected other host.
Direct-host rows expose their persisted pinned origin and nonsecret credential
fingerprint, including accessible Open/Delete names: the production label
"Tyde Host" is not an identity. No component looks up or infers an origin.
Pair another host opens the existing scanner/Paste link flow.
Native direct or managed Pair success opens Home for the exact
`LocalHostId` returned by that redemption, not a matching label, newest row or
background connection. Previous records/credentials remain intact. Browser/PWA
selection, tabs and pairing navigation are unchanged. Pair success starts the
existing connection actor; only its protocol/dispatcher bootstrap can establish
Connected. Navigation never supplies that status.
Managed pairing publishes its saved record before starting the existing connection
actor. A pre-actor secure-store/load error stays a visible pairing failure with
that reason; it never reports success or removes the saved credentials. Transport
failures after actor startup remain the connection manager's responsibility.
Saved auto-connect credential failures also reach the existing app-attach error
boundary, instead of returning success before an actor/status exists. Attachment
still attempts the other independent hosts before surfacing required failures.
The mounted two-host flow covers cross-host text retirement through opening an
existing conversation and its real typed send, plus reopening a saved managed
pairing whose native secret read is denied. Actual40 showed two required startup
failures from the two saved auto-connect records; the original single-error
oracle incorrectly rejected that visible aggregate. The corrected flow observes
the first actor's terminal state before attachment and requires both failures.
Normal Reconnect also surfaces a pre-actor credential failure through the existing
typed shell-error owner rather than only logging it. It does not change storage,
connection retry behavior or server status. Same-host draft retention remains
covered by the existing Hosts/Back and tab flows.

UIKit lays out the WKWebView within the selected controller's system safe-area
layout guide. The shared `@tyggs/web-shell` remains the sole inner viewport and
keyboard owner. Native CSS clears already-consumed safe-area tokens; there is
no native keyboard-overlap observer, viewport controller or copied shell fork.
Native Home's status row uses the existing spacing token between the unchanged
indicator and text; its theme sizes and browser/PWA layout remain unchanged.
Native host-picker and pairing content share onboarding's bounded32rem column,
while existing header/navigation placement stays unchanged. Compact windows use
the available width; text/input/button theme roles are not resized. Pairing's
focused field uses one contiguous rim rather than a second offset outline.
Scanner guidance names the desktop Settings/Mobile location without assuming
the remote client is a phone. Browser/PWA layout, focus and guidance stay intact.
System containers decide tab/bar placement and Liquid Glass. No raster-positioned
OS chrome, clock, status strip or Left/Right preference exists.

## Creation navigation

Native New chat, Fork + send and Resume each send one SpawnAgent on a fresh
`/host/<connection>/spawn/<UUID>` child stream. Protocol95 admits only that
connection's child and one submission (seq0). The server emits one AgentStart
publication descriptor or matching CommandError on the child. This is creation
identity, not backend-started/readiness state. It does not replace the ordinary
NewAgent/HostBootstrap descriptor or the requester's private agent instance stream.

Publication owns the terminal even if post-fanout work is cancelled. Building a
bootstrap is not publication; successful enqueue is. Child output waits for its
parent bootstrap, not a nonexistent bootstrap on the child. Every other client
still receives discovery on its own root, never another client's completion.
Browser/desktop root-stream creation and navigation stay unchanged.

The native destination joins only its exact operation and server AgentId with
that host's typed descriptor. Discovery alone cannot replace the current chat.
Explicit later navigation, another creation, host switch or disconnect retires
that destination; a stale result cannot reclaim it. Fork/Resume capture the exact
source draft text, not a second transcript or live-status model. Later text edits
or photo changes retire automatic navigation, leaving the edited source composer
intact while the published conversation remains normally discoverable. No session/name/status guess
or root-stream retry exists. Protocol94 hosts remain compatible with frozen
Source28 for its independent banner/input proof; a candidate actually using this
new creation contract requires a genuine protocol95 host.

## Assets, origin and credentials

`./dev.sh ios-bundle` packages only the generated native bridge, installed
shell release, bootstrap version and trusted loader origin. Its source manifest
and bridge hash bind these resources to reviewed source. No full JS/Wasm client
or global Tyde wire schema is bundled into the shell.

The native document loads `https://tycode.dev/tyde/?native=1`. The canonical PWA
loader has an explicit native entry: before executing a full client it resolves
a fresh authenticated host descriptor, then selects that exact published client
release. The publisher manifest must permit its native ABI, match global and
transport protocols and the complete executable-set digest. Every executable's
SRI is verified and placed in the controlling worker's verified cache before
import. Missing, stale, revoked, mismatched or unavailable targets fail visibly;
there is no latest-client or old-client fallback. Browser/PWA admission and
loading policy remain unchanged.

A complete direct HTTPS pairing link supplies origin and offer authority, not
release authority. Paired direct hosts resolve `/tyde/bootstrap` with their
retained device credential. Private/SSH managed hosts register their actual
client descriptor with Service; mobile resolves it with its retained pairing
HMAC and owning authenticated session. Registration uses generation CAS and a
bounded lease; renewals stop on error. Reconnect resolves again before opening
the normal authenticated transport. The server's Welcome confirms the selected
descriptor before bootstrap. Host behavior/status still belongs to that full
client and the normal server wire protocol, never Swift.

The same full-client document retains its AppState, drafts and native navigation
until an actual target or host change requires a deliberate document lifecycle.
An unsafe draft/chat transition is refused, not silently discarded to reload.
Bootstrap can access pairing secrets to choose a host; before client execution
the bridge seals reads/replacements to that selected client's secret handles.
A provisional navigation suspends reads until the next committed bootstrap.
Host summaries remain IndexedDB data and Keychain contains secrets. The trusted
origin differs from historical loopback installs: no silent credential migration
or published beta20/native3 compatibility is claimed.

Only trusted main-frame bootstrap navigation and media requests receive native
capabilities. Explicit content links open externally. Service and pinned direct
HTTPS HTTP remain URLSession operations without redirects; protocol WebSockets,
WebRTC, framing, crypto, auth and sequence checks are unchanged. Bridge changes
can require an App Store update; ordinary host/client release changes do not.
The loader is remotely delivered code, not an App Review-policy exemption.
No App Review approval, production authentication, real RTC/TURN, Swift/Keychain
or physical fold coverage follows from Linux boundary checks.

## Native failure diagnostics

WebKit rejects reply-handler failures with a JavaScript `Error`, not a string.
The client preserves that error's controlled message and typed endpoint in the
failure screen. An invalid rejection shape is an explicit bridge contract error,
not a substitute error reason. Native logs retain endpoint, controlled failure
reason and framework error domain/code. Keychain failures retain OSStatus; keys,
values, URLs and NSError userInfo are never logged.

The initial Simulator wrapper disabled signing. In Source07's actual live-code
attempt, HTTP redemption created a durable host device; the app's subsequent
`secret_write` reached `SecItemUpdate`, and securityd rejected that same app
process with -34018 because both default Keychain identity entitlements were
missing. The client's string-only rejection extraction then hid the Keychain
error. Restore normal Xcode Simulator identity generation, not a different
secret store, removed OS check, copied profile or custom shared access group.
Actual signed build entitlements and the no-offer platform check must establish
that boundary on the Mac before another pairing attempt. The mounted-DOM
regression fails on the old extraction and preserves the real failure message
through the existing recovery screen; it does not simulate Keychain success.

Native image bar items name their product action explicitly for accessibility.
Source10's actual Agents AX capture exposed the new-chat icon as `compose`,
despite its UIAction title being `New chat`; no `New chat` native button existed
for VoiceOver or the original navigation assertion. The bar item's explicit
label fixes that product semantic boundary without changing its icon, action or
web content. Navigation acceptance still locates `New chat`, asserts that label,
and waits for the selected native destination plus an enabled, hittable control
before activation. It never substitutes the web empty-state action.

Native shell errors participate in the native content layout rather than using
browser fixed-position chrome. They wrap at the existing text size, leave the
page title/Cancel and system bars reachable, and keep a touch-sized dismissal.
The existing typed code remains in state/diagnostic metadata, not primary copy.
A native Welcome rejected by the existing exact-release policy surfaces the
existing `VersionMismatch` code and actionable message; protocol invalidation
and its detailed diagnostic still occur. Dismissal does not change the saved
pairing, compatibility verdict or connection lifecycle. Browser/PWA banner
presentation and the desktop frontend are unchanged.

Native workspace errors have one visible owner: the shell error, then a required
bundle/storage notice, then the selected host's command error, then its connection
notice. A global error has no implicit host ownership: its Dismiss action exposes
normal typed host recovery; the chooser keeps each row's own Connect action.
Selected-host command errors retain their host recovery. Fixed-package
compatibility never displays the PWA executable-loader paragraph as a second
native error. Its typed rejection remains unchanged and visible after dismissing
a shell notice. Home keeps its honest dot/status, not a second pill and repeated
connection statistic. Browser/PWA loader notices, diagnostics, Home and actions
are unchanged. Actual28's duplicated native Home was rejected; new captures and
Root's personal approval are still required for this refinement.

### Explicit navigation-delivery capture

The opt-in trace also reads each known native navigation view's public
accessibility frame and activation point in screen coordinates, converts the
point through the actual screen coordinate space into its own UIWindow, and
records the public hit-test result without dispatching input. It captures the
same fields at natural layout and before original touch dispatch. Case59 proved
that a successful Agents selection need not trigger another parent layout call.
A separate `projection_applied` record therefore reads the controls after the
valid typed projection has actually been applied, including the native selected
controller's exact tab identity, each control's optional UIControl selected value
and accessibility selected trait. The original `projection` still means receipt
BEFORE applying UIKit state; script completion alone is not selection authority.

The applied snapshot is unconditional for each successful render, not suppressed
by layout deduplication. It invokes no layout, transaction, focus or input action.
These getters report their actual values at that event; they do not certify a
settled compositor frame. Qualify matching projected/native/control selection
and actual AX/pixels, and report disagreements rather than force another record.
Native view traversal excludes web content and fails explicitly if its bounds
are exceeded. These are observed accessibility properties, not XCTest's
undisclosed planned point; no activation point, hit box or eligibility is changed.
Case56's missing historical value cannot be reconstructed by a later capture.

A Debug build normally uses ordinary UIWindow. Only an explicit
`TYDE_IOS_NAVIGATION_DIAGNOSTICS=1` launch selects a pass-through UIWindow
observer. Its `sendEvent` calls super exactly once and never changes touch,
hit-test result, focus, action, timing or hit-target eligibility. Before/after
normal touch dispatch it records the actual touch phase, point, receiving
window geometry, target ancestors and public UIKit hit-test ancestors. It also
records native intent submission, script completion and the existing typed
presentation's tab/route/modal flags. This is observation, not status authority
or a diagnostic action-invocation path; it introduces no bridge endpoint,
WebView script, private selector, LLDB hook, input generator or new test target.

Capture is local, explicit and content-free: no view labels/text, transcript,
field values, URLs, credentials or native identifiers. Only known app control
identifiers are retained. One owner-only `tmp/native-navigation-diagnostics-*.jsonl`
file has at most128 records of at most32KiB each; exhaustion/write failure is a
failed diagnostic, not permission to replay. The normal app behavior continues
unchanged after recording failure. Read the actual error and partial file.

Before one unchanged normal action, the executor must qualify the exact build,
opt-in launch, original data, actual WDA/app/display/window identities, healthy
trace and actual `Native navigation diagnostics ready` layout boundary. Observe
its actual native target/window and correlate with that one command's normal
synthesis logs and selected-tab/route/pixels oracle. No endpoint-success label
or absence of a recorded touch proves delivery or cause by itself. No new
source fix is established if the failure does not reproduce under observation.
This opt-in does not enable or alter the original single-action exact-draft
input diagnostic, which remains independently owned and unresolved.

## Managed auth dependency

Native auth requires service `/auth/native/start?provider=apple|google`: URLSession
captures its 303 and OAuth-start cookie, then ASWebAuthenticationSession opens
the provider Location with exact `tyde-mobile://auth/callback`. The existing
handoff exchange uses that same URLSession cookie jar and typed `native_ios`
client identity. HttpOnly cookies and OAuth/Account credentials do not enter JS.
Browser auth is separate and unchanged. Service and Account must explicitly
support this callback contract; source integration alone is not deployment or
successful managed-auth evidence. Do not substitute an embedded OAuth WebView,
copy cookies, bypass TLS or assume Associated Domains/signing setup.

## Validation and platform limits

Native XCTest identifies one containing semantic tab owner by its exact native
identifier. Actual iPad floating tabs have no TabBar AX ancestor, and UIKit
copies an item's identifier onto a nested Button. The owner must contain every
other same-ID representation; unrelated duplicates fail. Actionability, selected
state, intended label and actual destination remain required. No first-match,
coordinate or alternate-control action repairs an ambiguous native hierarchy.
After that ownership/actionability gate, ALL native tab tests use one ordinary
physical center touch through public XCUICoordinate, anchored to that element.
Its current screen point must be finite and inside the owner/window; its actual
point/frame are attached before the single touch. There is no semantic tap first,
coordinate fallback, alternate point or retry. Selected state and actual content
remain mandatory afterward; native delivery capture also requires the intended
received target/intent and matching applied client/native selection.

Actual63 repeats the semantic Home failure with healthy contemporaneous Home
activation903,464/hitHome, but received882,492/hitAgents and honest Agents intent/
projection/pixels. This proves the semantic-tap apparatus delivered the wrong
native target, not a Home-to-Agents app handler defect. Whether point choice or
later synthesis caused that discrepancy is unproved and not a mandatory private-
framework observability requirement. The physical path avoids that unreliable
computed-hit-point operation using Apple's supported coordinate input; it does
not change production handlers, tab layout, hitboxes or accessibility metadata.
Both historical semantic failures remain failures. Actual33 case64 qualified the
pinned public element-centered physical path: one903,464 Home touch, Home intent,
matching applied native/client Home, successful script and selected Home AX/
pixels. Root approved that pictured Home/navigation slice. This proves the new
physical QA path, not Apple's internal semantic-tap cause, a production app fix,
the changed XCTest's compilation, original text input or full/four-layout UI.
The bounded current33 WDA execution packet is retained externally at
`/home/tyggs/tyde-evidence/native-ios-navigation-host-20261031/source33-physical-tab64/`.
Its one-shot helper reuses the exact existing helper's read-only strict target/
fixture query, requires the actual frame center to equal the observed public
activation point and intended native hit, then sends ONE existing element-
anchored WDA coordinate touch. The full native navigation oracle stays separate
from HTTP success. It needs no app/tool rebuild or production change.
Apple's public coordinate API defines its position relative to the element frame:
https://developer.apple.com/documentation/xcuiautomation/xcuielement/coordinate(withnormalizedoffset:)
https://developer.apple.com/documentation/xcuiautomation/xcuicoordinate/tap()

`./dev.sh check` remains the mandatory complete repository gate. Its mounted-DOM
flow covers native-only navigation suppression, retained editor selection and
session filter, wide/compact hierarchy, modal exclusion, pending/failed session
resume, visible context occupancy, sticky version rejection and restored browser
navigation. No native
Rust frontend/unit tests or provider fakes are added.

For the separately requested native platform boundary:

- `./dev.sh check --ios-build`: generic Simulator build using prepared
  exact-source assets; no device boot/install or Rust build on the Mac. The
  Simulator-only wrapper selects Xcode's local ad-hoc identity (`-`) and retains
  normal app-identity entitlement generation. It never disables code signing,
  selects a developer certificate, invents a team/access group, installs profiles
  or modifies accounts. Device signing/distribution is outside this workflow.
- `./dev.sh check --ios-platform <owned-uuid>`: targeted XCTest of the real
  app's production Keychain write/read/update/delete functions with an isolated
  noncredential check item. This explicit Debug launch does not load the web
  document, paired records or networking. It captures a terminal pass or the
  actual safe OSStatus failure. Require this boundary to pass before requesting
  another pairing offer; it is not pairing, WSS or visual acceptance.
- `./dev.sh check --ios-pairing <owned-uuid>`: builds and attaches the real
  XCTest driver, opens the retained host chooser and normal manual form, then prints `TYDE_IOS_PAIRING_READY`
  with a private FIFO path and writes `driver-ready.json`. Only that actual
  actionable-app boundary permits Root to request a fresh offer. No URL is
  supplied at process startup; old pre-issued URL environments fail explicitly.
  Rotation/capture preparation waits for actual window and retained WebView
  geometry in the requested orientation plus the relevant control's hittability.
  A device/interface-orientation notification alone is not layout completion.
  The empty Pairing link field must be focused with usable native keyboard
  geometry before READY; this same state boundary applies to later chat
  rotation captures, without sleeps or retries.
- `./dev.sh check --ios-paste-pairing <owned-uuid>`: the distinct normal system
  Paste journey described below. It does not run or rescore the typing case.
- `./dev.sh check --ios-offer <ready-fifo> <owner-only-handoff.json>` delivers
  the complete unchanged private handoff through the blocking channel, never
  argv or logs. The already-running normal driver owns input, exact full UTF8
  equality, Continue, issued-origin confirmation, Pair, Connected/bootstrap and
  newly selected record correlation. It does not inject an app launch URL.
  Actual terminal errors fail the case. The pairing-only case stops before the
  screen inventory; tabs alone never establish WSS or backend readiness.
- `./dev.sh check --ios-simulator <already-paired-owned-uuid>` runs the retained
  navigation/chat flow on that same app/store without re-pairing or resetting it.
  Both phases target only the explicit owned simulator. Logs/captures remain in
  `target/ios-results`; there is no sleep, TTL override or speed assertion. The
  existing per-test watchdog diagnoses a genuinely hung driver/handoff, not a
  performance requirement. This is UIKit/WebKit platform QA, not a substitute
  for Wasm UI tests or the full committed-workbench gate.
- `./dev.sh check --ios-animation-diagnostics <owned-uuid>` shares the normal
  manual route, focused-field/keyboard/window readiness and one unchanged
  default whole-string `typeText`, native input-size completion and exact UTF8
  equality. The return-time scalar remains a separate content-free observation.
  Its distinct `TYDE_IOS_ANIMATION_CAPTURE_READY` marker is NOT permission to
  issue an offer. After a separately qualified native observer is ready, deliver
  only the explicitly retained expired input through the existing private FIFO.
  It stops after input equality/capture, BEFORE Continue, classification, Pair
  or connection. Normal `--ios-pairing` never inherits this early stop; externally
  preset capture flags are rejected. No app code, recorder, animation, XCTest
  quiescence setting, typing rate or pairing lifetime changes in this mode.
  Source34 passed421-byte input but five animation completion notifications
  remained pending while its main run loop replied. Historical65/66 recovered
  RBS invalidation, not the missing CA/XCTest registration identity. A live
  observer's actual coverage must be reported; private owner metadata unavailable
  in actual67 is not a prerequisite for the public behavioral control. Log
  volume, a successful input or nonreproduction is not a causal fix. Raw logs
  remain private. Capture completeness, native compile and reproduction are
  separate from repository validation and full feature approval.
- `./dev.sh check --ios-animation-control <owned-uuid>` uses the existing plain
  WKWebView control document with the manual field's label/rows/placeholder,
  same embed/configuration/document origin, real focus/software keyboard and
  unchanged whole-string typing/exact UTF8 guard. It contains no Tyde bridge,
  AppState, shared shell, input recorder or page scripts; the existing native
  capability read remains. At its distinct ANIMATION_CONTROL_READY deliver only
  the retained expired34 input, never mint an offer. It stops before Continue
  or Pair. Observe ordinary XCTest completion outcomes, not performance limits
  or inaccessible private registration IDs. Reproducing the animation wait here
  establishes that Tyde page code is not necessary; it does not identify an
  Apple defect or exclude the shared native/WebKit embedding. Nonreproduction
  narrows the differing page context, not proof of a particular Tyde handler.
  Original32/control REDs remain independent; no typing, input-method, animation,
  checker, quiescence or expiry workaround is introduced.
- `./dev.sh check --ios-hosts-diagnostics <already-paired-owned-uuid>` reads the
  unique Hosts control's direct public XCTest `isHittable`, frame and center,
  with actual Home selected, without tapping or changing orientation/selection.
  The failed `UIUpdateLink` observer and its startup prerequisite are retired.
  Actual37/38/40 failed before observing Hosts:40 measured disabled/noncontinuous
  defaults, contrary to the probe's enabled-default assumption. Those failures
  establish no navigation or typing cause; all historical source/raw evidence
  remains external. The existing opt-in pass-through touch, intent, projection
  and natural-layout trace remains unchanged; no post-update/settled-frame claim
  is made. Normal product launches do not enable this trace.
- `./dev.sh check --ios-agents-diagnostics <already-paired-owned-uuid>` is the
  bounded diagnostic for the preserved empty Agents navigation failure. It
  relaunches that same app/store, selects Agents once, waits for the selected
  native tab and visible reused empty-screen control, then attaches the actual
  screenshot, full AX hierarchy and native navigation button properties. It
  stops without pressing either new-chat action or starting the navigation
  acceptance flow. Keep these raw attachments private; diagnostic completion
  is not a passing toolbar-action or navigation assertion. No offer or reset.
- `./dev.sh check --ios-project-picker-diagnostics <already-paired-owned-uuid>`
  relaunches that same app, selects Agents and activates the verified semantic
  native New chat control once. It captures the actual picker screenshot/full
  AX tree and No project candidate types, labels, identifiers and frames, then
  stops without selecting any option. This diagnoses WebKit's real platform
  mapping rather than guessing that a DOM button with `role=option` is exposed
  as an XCTest button. It is not picker-selection or full-flow acceptance.
- `./dev.sh check --ios-input-diagnostics <already-paired-owned-uuid>` follows
  the original navigation path through the one unchanged draft `typeText`,
  waits for the actual native value to equal the complete exact draft, records
  that scalar's presence/length/exact-match result, captures the real screen/AX,
  asserts the complete exact draft, then stops before the menu. The same exact
  value precondition precedes the ordinary navigation/retention flow. A returned
  typing command or native key acknowledgment is not editing completion. The
  wait observes state after ONE typing action; it never retypes or repairs input.
  It ends on exact equality, an explicit native platform error, or the existing
  ten-minute hung-test guard, not a sleep or an editing-speed requirement.
  An explicitly enabled DEBUG-only main-frame observer records composer input,
  composition, focus and programmatic value-setter lengths/caret positions to
  an app-sandbox `tmp/native-input-diagnostics-*.jsonl` file, mode0600, capped at
  512 events. It forwards the original setter unchanged and records no text,
  keys, credentials, host/device identifiers or network data. Export that owned
  file privately alongside the original recording/AX; absence/overflow/error is
  not clean evidence. Each composer observation includes exact-match and prefix
  booleans against the declared fixed input, including both sides of a value
  setter. Equal lengths alone never establish equal input. Document timestamps
  and native receipt timestamps permit correlation with retained platform logs;
  they are observations, not speed assertions. The ordinary app and original
  full navigation assertions remain unchanged. This is diagnosis, not a product
  fix or navigation acceptance;
  no typing retry, paste, field seeding, rate change, offer or reset is allowed.
  Source19's recovered trace stops at15 trusted input events without observed
  setter shortening. To distinguish key semantics/cancellation from missing text
  input, the same diagnostic also records content-free keydown/keypress/keyup
  classes, ordinal matches against its declared fixed test input, modifiers and
  cancellation calls, including keypress and beforeinput cancellation. Focus and
  keyboard events outside the composer are marked as such without reading that
  field's value, input data or key content. Native keyup drains retained keydown
  events for final cancellation
  observations; an early microtask is not treated as post-dispatch proof. The
  original preventDefault method is forwarded unchanged. A keyboard-complete
  marker reports the declared keyup count, not successful input or all future
  events. Actual cause remains unproven until this boundary is observed; no
  raw keys, expected text or event data are written to the trace file. The
  `settled` entry is only an input-listener microtask snapshot, not proof that
  every handler or future edit has completed.

  Native84 on Source46 remains RED at the exact draft-retention assertion:
  the same eight-byte prefix was already visible before opening the send menu.
  Retained native records acknowledge all32 requested characters in order,
  but native acknowledgments are not DOM input or complete-value evidence.
  Source47 changes this existing DEBUG recorder only to distinguish native-to-
  DOM routing, DOM cancellation, and actual value writes. Its real-DOM check
  verifies pass-through setters/cancellation and content-free observations;
  constructed browser events are not native keyboard delivery.

  Native85 on Source47 independently remains RED: the return-time scalar was18
  of32; its healthy307-record trace observed32 matching character keydowns and
  keypresses but only28 trusted text edits. One focused, connected composer kept
  expected-prefix values with no observed cancellation or shortening setter.
  Retained same-clock records prove synthesis completed at01:50:40.273 while
  UIKit insertions continued through01:50:43.322. The immediately cached scalar
  was not an editing-completion oracle. Source48 corrects only that harness
  assumption by waiting for complete exact value before reading/asserting it or
  opening the menu. It does not explain the four missing edits or claim that a
  wait repairs them. No typing action, input strategy, fixed fixture, complete-
  input/retention/Send assertion, expiry, product handler or launch changes.
  Native84/85 and their raw scalars remain untouched. A new authorized case is
  not their rescore or proof of navigation acceptance. If the recorder lacks a healthy start or
  necessary terminal event custody, missing entries are not absence evidence.

Every native capture includes the actual app AX hierarchy as a private
attachment, not just the missing element's query description. Positive element
waits retain a destination screenshot/tree before waiting, and the supported
XCTest issue-recording hook captures the failure boundary while forwarding the
original issue unchanged. A process-killing watchdog cannot promise a final
snapshot, so pre-wait evidence is retained too. Attachments are diagnostics,
never parsed as test assertions, and raw AX content must not enter chat/logs.

Source12's actual picker capture identifies the selected, enabled and hittable
No project option as AX Other under its named web dialog, not Button. The
original button-only native query was a harness assumption; the product's
listbox/option semantics remain unchanged. Native acceptance scopes the observed
option type and original name to that dialog, requires a unique selected and
actionable choice, activates it, then requires the composer and dialog dismissal.
That phone capture shows modal native bars/tabs absent from AX, rather than
queryable disabled buttons; it does not establish the adaptive iPad result.
All shared-modal navigation assertions require that exclusion, matching the
production modal projection instead of weakening focus ownership. Future
boundaries retain actual screenshots/AX before waits.

Source45 actual83 normal system-Paste pairing passed one test with zero
failures, including exact421 UTF-8 equality, origin confirmation, Pair and
Connected/bootstrap. This is the separately qualified Settings-Allow journey,
not proof of default consent or complete keyboard typing. Its subsequent
navigation test opened the actual project picker, then reached the ten-minute
per-test watchdog while waiting for native navigation exclusion. The original
pre-wait screenshot and AX attachment
`8A6F255E-E093-409A-B6A9-2C7AC393975A.txt` show the native navigation bar excluded
but all four floating iPad tab owners and their nested buttons still exposed.
The unchanged predicate repeatedly reaches Home existence and cannot advance.
This is a product accessibility defect, not an idle XCTest tap or a missing
project option. UIKit hid only descendants of the legacy `tabBar` view, which
does not own that floating representation.

The host now drives the active tab bar's visibility with
`UITabBarController.setTabBarHidden(_:animated:)`, available from the app's
iOS18 minimum. The same typed modal projection hides it without animation and
restores it when the web sheet closes. Navigation-item disabling, native intent
rejection, the retained WKWebView and all exact input/modal assertions remain
unchanged. Source46 still requires actual native regression captures before
claiming the iPad fix or remaining navigation/chat/layout acceptance passed.

The actual Source13 pre-action and failure trees likewise expose the correctly
named More send actions popup trigger as Other, while the adjacent Send message
action is Button. The native test scopes the observed trigger to its Send
actions group, requires uniqueness/actionability, and proves the real menu
contents appear and disappear without changing the draft. It does not remove
`aria-haspopup`, alter product menu semantics, or treat every control as Other.

Source13 phone captures also showed a clipped backend selection and competing
textarea/capsule focus outlines. Native-only new-chat fields now use their own
available width to choose one or two columns without reducing theme fonts; the
controls retain usable touch height. Editing uses the capsule's existing focus
rim rather than an additional textarea outline. Keyboard-focused send controls
keep their own visible indicator without a second capsule ring. The existing
mounted-DOM backend-selection flow measures actual selected-label fit at phone
and wide widths, unchanged font family/size, single editing focus and restored
browser styling. These app-specific overrides do not alter web/PWA layouts or
the shared shell's viewport/keyboard ownership.

Source15's real Sessions filter capture shows the system keyboard covering the
native tab frames. XCTest's subsequent Settings action computed a hit point of
`{-1,-1}`; the next capture has no keyboard but still selects Sessions. Those
facts do not establish delivery to the tab delegate or a failed client intent.
The original harness tried an obscured tab without an actionability precondition.
Native navigation acceptance finishes editing through the ONE actual native
keyboard dismissal control: the phone's observed Done accessory or iPad's
integrated Hide keyboard action. It does not try one and substitute the other;
ambiguous/missing controls remain failures. It waits for keyboard/accessory
absence and an enabled, hittable native tab, taps that same destination once,
and requires its selected projection before waiting for the original content.
Actual app AX/pixels are captured before actionability and selection waits.
The canonical physical tab action above never bypasses this keyboard gate. No
alternate destination, retry, keyboard geometry producer or product navigation
workaround is added. The Appearance and retained filter/draft
assertions remain; actual corrected Mac interaction is still required.

The same native focus rule covers the reused search-field primitive: its existing
editing rim remains, without the shared shell's second outer outline. Theme fonts
and browser/PWA focus stay unchanged. The existing mounted Sessions filtering
flow asserts that native paint, visible rim, unchanged fonts and restored browser
outline, waiting on real CSS animation completion rather than a delay.

Recovered Source16 establishes real native Settings selection and rendering.
Its remaining title-case StaticText query was wrong: all twelve source `h2`
section labels use CSS uppercase, and actual WebKit AX exposes each as a named
Other heading group with an uppercase StaticText child. Native QA now scopes the
unique exact section label to that observed heading group, ignoring presentation
case. These are noninteractive headings; the tap only scrolls the existing label
into view. Product text, heading semantics and browser styling are unchanged.
The same remaining-flow audit checks modal names/buttons/dismissal, the observed
system Back identity and destination, fresh-draft reset, native agent menu/sheet,
and the named assistant article without assuming an unobserved XCTest category.
Actual unvisited native mappings remain unproven, not claimed from source alone.

Source16's latency badge also overlapped the Theme selector: its browser floating
position assumes a web header that native packaging hides. In native only, the
existing connection component now occupies a nonshrinking normal-flow row above
the workspace, rather than overlaying content. Status, RTT, errors and recovery
actions retain their existing state owner; fonts and browser/PWA floating layout
stay unchanged. The existing native mounted flow checks reserved HUD space and
non-overlap with rendered interactive controls across Home/chat, Agents, Sessions
and Settings at compact/wide sizes, with healthy and reconnect states. This is
ordinary DOM geometry coverage, not all-four or genuine Duo platform evidence.

Acceptance photos require consistent observable window/WebView/control and
keyboard/accessory geometry. Actual31 iPad has one visible software keyboard,
a focused empty Pairing link field, SystemInputAssistantView above the keyboard
and NO Done. Its Done-count watchdog was a harness fault, not an input result.
The shared predicate now requires an enabled/hittable focused input, one software
keyboard fully in-window and a field above keyboard/actual accessory occlusion.
A Done or system assistant bar is validated if present, never required merely
because a phone displayed one. Nonkeyboard captures still reject the phone's
orphan Done and any visible system assistant after keyboard absence.

Normal manual pairing uses that predicate for READY and checks it again before
input; changed context fails with no typing. The already-running XCTest case
blocks on its private FIFO and owns the complete input/Continue/confirmation/Pair
continuation. Issue and delivery are one operation after actual driver readiness,
not a model handoff after issuance. The private channel preserves the issuer's
expiry; ordinary pairing rejects missing or expired metadata without typing.
The explicit whole-string animation/control modes retain their non-offer inputs.

Ordinary entry uses public `XCUIElement.typeText(String(character))`, acknowledging
each changed native WebKit field value against the exact issued prefix before
sending the next character. A wrong changed value fails immediately; it is never
retried or reread until green. This reuses the per-character XCTest strategy
proved in Tychat, not its later unproved public CG keyboard sender. Native AX
value acknowledgement is the existing Tyde oracle; no page evaluator or event
receiver is added. Actual75's CG sender changed the first byte incorrectly and
stopped with0ACKs/0Continue/0Pair. That failure remains RED and does not identify
an application, layout, modifier or OS cause. The unchanged bulk controls remain
separate historical diagnostics, not ordinary-input requirements.

Actual37's real Tyde A returned237/421 bytes, then recorded249/260-byte prefixes;
its script-free B returned307/421, then324/347-byte prefixes. Both completed their
observed animation/main-loop checks. The typing API return therefore did not
establish completed insertion in either document. Neither recording establishes
an eventual421-byte value or the shared pipeline's deeper cause.

The final full-input assertion preserves the return-time count, then synchronizes on native
value size reaching at least the requested byte count before ONE exact UTF8
comparison of that first completing scalar, including an already-complete return
value. No later native reread may replace it. It does not wait for equality:
a full-length wrong value fails, as does an oversized value, missing field,
platform error or lost foreground.
A prefix can never pass. A genuinely unfinished input still hits the existing
individual-test watchdog, not a longer typing lifetime or performance threshold.
Every character is sent once; no retry, setter, paste, pacing or
animation/quiescence change is allowed. The complete full-UTF8 assertion remains
mandatory after all prefix acknowledgements, and the issued expiry is checked
before input, each character, Continue and Pair.
The correction is to the harness's completion oracle, not production input code;
actual native regression proof remains required. The historical immediate REDs
remain valid observations and are not relabelled. The original32 single-action
draft diagnostic/oracle remains unchanged. This rejects Source16's offscreen
keyboard/accessory-over-tabs transition without inferring a product keyboard
bug, adding a keyboard owner, disabling animation or waiting a real delay.
Raw pre-wait/failure attachments remain diagnostics, not settled UI approval.

The focus regression checks outline paint style, not an assumed zero computed
width: current CSS UI4/WPT `outline-009` keeps width independent of style none.
The old width-only assertion rejected the corrected CSS. Safe computed
style/width diagnostics remain in test logs; visible capsule and send-control
focus plus the browser baseline are still required. No product rule is changed
to satisfy that incorrect CSSOM assumption.

Use actual SDK/runtime/device facts. The source has no exact SDK27.1 pin:
SDKROOT is unversioned, deployment target is iOS18, and validation uses selected
Xcode. A supported newer SDK/runtime that actually supplies genuine Duo
postures is acceptable. Xcode27.0 ordinary phone/iPad proof is not unobserved
Duo proof; a listed runtime is not boot proof. Full acceptance includes every major screen and key
menu/keyboard states across the four approved layout concepts, tab/Back/chat,
rotation/resize and lifecycle preservation, plus browser baseline. Root must
personally inspect actual final screenshots before its sole Lander lands.
No SDK/runtime install, signing/account/certificate change, production restart,
protected QA-device mutation or distribution is implicit in these commands.

### Distinct normal system-Paste journey

Actual source43/episode77 remains RED:29 public per-character `typeText` calls,
28 exact native prefix ACKs, no complete421-byte equality, Continue or Pair.
The first-to-last ACK event span is106.56s. Its failure image is1.167s after the
unchanged offer expiry, with focused partial input and a visible keyboard.
Line425 combines ACK/native-state/expiry guards; the exclusive first failing
condition is unproved. This is not a wrong-prefix verdict or proof of a product
input defect. Original75/76/77 receipts, bytes and screenshots remain unchanged
under `native-ios-navigation-host-20261031/mac-executor-01/` in evidence custody.

Actual source44/episode78 remains pre-READY RED. Its original native AX
(`source44-systempaste78-bcd01/SOURCE44-PRE-PASTE-AX.private.txt`, SHA256
`e0b4c0d306bbfc906e1d4a5946c9e0f0ebfd21b7f40b8d2b2d43683fc528c475`)
exposes `SystemInputAssistantView` containing the single
`assistantPaste:forEvent:` button, label Paste, outside the WebView. The
`inputAssistantView` literal observed in77 and queried by source44 is absent.
The keyboard precondition passed, but the Paste predicate reached the per-test
watchdog:0 READY events, offers, clipboard writes, Paste calls, Continue or Pair.
Correct only that ancestor identifier to the actual78 value; retain uniqueness,
label, actionability, focus, geometry, expiry and exact complete-input assertions.
No live individual predicate results were captured, so other readiness terms
and product Paste delivery remain unproved. The original78 receipt and images
remain unchanged; neither78 nor77 is rescored by this harness correction.

The separately approved product flow uses the existing Paste link screen and
the real OS input assistant's Paste button. These are public XCTest locator
attributes, not a selector invoked by the test. There is one observed route,
no alternate ancestor ID, edit-menu/keyboard/CG substitute or production change.

The explicit `testNormalSystemPastePairing` case shares the existing normal
launch/Hosts/Pair-another/manual-field and pairing continuation. Its distinct
`TYDE_IOS_SYSTEM_PASTE_READY` marker and0600 `driver-ready.json` identify
`entry=normal_manual_system_paste`, `input_strategy=native_assistant_system_paste`,
the actual runner PID and `continuation_preloaded=true`. READY requires an empty,
focused/hittable real field, in-window software keyboard and the unique visible
native Paste action above it. It does not claim input delivery or connection.
The case is already blocked on the same private FIFO before issuance.

Only after private payload delivery does the test put the unchanged421-byte
issued link on this simulator's system pasteboard using public `UIPasteboard`.
The item is local-only, excluded from Handoff, and expires at the original
issuer timestamp. No desktop clipboard, app field setter, page script, URL
injection, typing call or production bridge is involved. The case checks the
real enabled/actionable Paste control, foreground/focus, unchanged clipboard
generation and offer expiry, then invokes exactly one public `paste.tap()`.
The first native scalar reaching the full expected size is frozen for the
independent exact UTF8 comparison: partial, oversized or wrong full input cannot
pass; no reread-to-green, repeat Paste, retyping or repair follows failure.
Expiry and focus loss terminate the Paste completion wait. Cleanup clears only
the pasteboard generation this case wrote; an intervening owner is untouched.
Apple's public API defines these privacy/expiry options:
[UIPasteboard options](https://developer.apple.com/documentation/uikit/uipasteboard/optionskey).

The same preloaded case then performs normal Continue, actual confirmation of
the issued HTTPS origin, Pair, authenticated Connected/bootstrap and selected
NEW record/fingerprint correlation before Agents. The original expiry remains
mandatory before input, Continue and Pair. No quiescence, clock, rate, animation,
test mode or production behavior is changed. A Paste PASS would prove system
Paste and that journey only, not421-character keyboard typing or all layouts.

Root execution packet:

1. Schedule an available authorized native executor on its OWN retained iPad;
   do not interrupt the independent web/Duo task, reset data, reuse old77's job
   claim or run two input actors. Preserve old sources, receipts and negatives.
2. Stage the new frozen source/bundle and verify its hashes. Use the SAME
   ordinary owned-device keyboard configuration qualified in77. Run only:

       ./dev.sh check --ios-paste-pairing E0845742-DE7D-4EDB-BB01-EC092C064FFF

   Reuse the existing managed canonical supervisor with this explicit testcase,
   new output namespace and distinct READY/strategy/scalar markers. No new
   sender, WDA fixture, observer, UI evaluator or QA framework is required.
3. Export actual current readiness, runner/process identity, source identity,
   native field/keyboard/Paste screenshot and live0600 FIFO identity to Root.
   Do not issue during compilation or merely because files are staged. Missing
   native geometry/control or an actual failure means0offers, no alternate tap.
4. HOLD for Root's explicit episode authorization.7117 alone reuses the same
   actualQA32/protocol95 host/population and75/77's BOUND-before-issue transport.
   Bind the private writer to this live Swift reader FIRST; then issue and
   deliver the entire unchanged handoff in ONE command with no model/tool turn
   between issuance and execution. The Swift case is already running and owns
   the whole continuation. No reoffer/TTL change, old claim rewrite or desktop
   clipboard delivery. This packet itself authorizes no fresh offer.
5. Report the one system Paste request, exact native full421 readback verdict,
   actual Continue/origin/Pair/bootstrap/new selected record and server/device
   correlation. Keep any failure original, stop and join OWN jobs. No retry,
   reset or alternate input. Root personally inspects actual screenshots before
   continuing chat/retention/four genuine Duo-layout acceptance. This source is
   not ready for the Lander merely because compilation/check/review passed.

### Plain WebKit input control

One explicit Debug control is selected through the same canonical input route:

```sh
TEST_RUNNER_TYDE_IOS_INPUT_CONTROL=plain-webkit \
  ./dev.sh check --ios-input-diagnostics <owned-uuid>
```

It uses the actual app/WKWebView construction, configuration, default data store,
origin and load path with one empty standard textarea and the unchanged bounded
content-free DOM recorder. It omits Tyde bridge/startup/module/shell scripts,
reactive binding, sizing handlers and native presentation. The app and XCTest
both reject unknown or conflicting control modes. A visible plain-page marker
and `plain_webkit_control` attachment distinguish it from product acceptance.

The real native tap and existing keyboard/window/accessory geometry must precede
ONE original default `typeText`. As in the product path, acknowledge the complete
exact32 native value through observable state, then preserve the exact scalar
assertion. Source48 corrects the same premature-completion assumption here; the
historical control RED is not rescored. There is no typing retry, paste, seed or
rate change. Stop after this one case. A passing control
is not product acceptance or an identified cause. A healthy control failure only
shows that full Tyde page behavior is not necessary for that failure; WebKit,
XCTest, native embedding and the original recorder remain common components.
A missing/failed recorder makes the diagnostic incomplete, not absence evidence.

The invasive private-runtime LLDB observer, pre-input FIFO and capture CLI have
been retired. Historical source, failed qualifiers and raw captures remain in
external custody; none is relabelled as a clean reproduction or input fix. The
canonical input diagnostic never installs private-runtime debugger hooks or
changes Main Thread Checker. Obsolete capture flags fail explicitly rather than silently releasing
another typing action.

## Native shared images and pairing diagnostics

### Build 4 thin-scanner observation packet

The independent decoder-error defect dates to `d9bbf6a5` (2026-06-22), first
contained in tag `v0.8.19-beta.5`. Its `catch { return null; }` makes a failed
decoder indistinguishable from a successfully read frame containing no QR;
the scan loop also used to swallow frame/detector exceptions indefinitely.
Build4 first reaches that thin scanner through `5eb3af98` in beta25. This
provenance does not identify the cause of the reported physical-camera failure
or native pinch drift. Neither original native episode has been reproduced by this change.

The fresh-offer rendered-pixel RED is a separate decoder-geometry defect.
At the actual 256px desktop QR box (238px SVG), all 6,561 module centres match
the producer and remain correct after binarization; the box is unclipped and
fully opaque. The existing mounted Settings DOM flow independently checks the
same 256px box and 238px SVG geometry with production CSS; its fixture offer is
not the live server offer. The bundled locator nevertheless estimates 79 modules for the
77-module version-15 code. The same private offer decodes exactly as a source
matrix and at 600px, while the original screenshot fails. The 600px comparison
is diagnostic only, never accepted input. Reading the existing BCH-protected
version metadata from the first extraction yields version 15; extracting at its
authoritative 77-module dimension recovers every module and the exact payload
from the unchanged 256px pixels. The decoder now performs that metadata-driven
refinement before payload decoding, not after a failed decode or by guessing,
rescaling or trying alternate dimensions. Both vendored copies remain identical.
The renderer, camera constraints and native geometry are unchanged.

This geometry defect entered the loader with vendored jsQR in `d9bbf6a5`, first
contained in `v0.8.19-beta.5`; the full-client copy arrived in `0946f663`, first
contained in `v0.8.19-beta.8`. The desktop SVG renderer dates to `998bd5aa` (first
containing tag `v0.8.15`) and is unchanged. These are source ancestry findings,
not historical binary/camera reproduction. The original physical-camera cause
and native pinch drift remain open; this product decoder reproduction does not
establish the original camera's captured pixels.

The retained RED run read an expired direct-link fixture from actual canvas
pixels: exact decode=true and blank-frame=true, but exception propagation=false.
The new flow instead uses a fresh real-server offer and the unchanged
desktop SVG producer (now shared through `mobile-pairing`), browser SVG raster
and actual bundled decoder, the native loader recognizer, and its real Scan
button/offer continuation through Pair confirmation and authenticated connection.
A canvas-backed MediaStream supplies input above the camera boundary; there is
no claim of mounted desktop UI, physical capture, permissions or UIKit. A separate
harness stall was observed as a live track and `play` event with zero dimensions,
no metadata/data event and no decoder call: the detached canvas had only painted
before capture started. The source now paints and explicitly requests its frame
on the real video `play` event, with manual capture rather than a periodic timer.
This changes only the owned test video producer, not scanner lifecycle. QR SVG,
pixels and text stay in the private driver pipe/browser; receipts contain only
booleans. Failure screenshots also hide the scanner video.
The error correction preserves genuine no-QR frames, waits for current video
data, stops capture on frame/decode failure, and renders a content-free alert.
It leaves canonical offer continuation, camera constraints and permission
requests, Paste, native host code and zoom/geometry unchanged. No native binary
change is required for this origin-loader correction; no publication is made.
The smaller DOM flow replaces the old in-process decoder-only assertions.

The rendered-pixel qualification uses an isolated real desktop DOM fixture with
the production stylesheet and QR producer, then decodes the element's screenshot
pixels at its actual rendered size after the finite overlay animation completes.
Content-free receipts retain decode stage, dimensions, contrast and recognition
booleans on failure as well as success. PNG bytes remain in memory/private browser
arguments, never an artifact or log. This is not the mounted Settings component
or a camera image. Direct continuation uses the existing real-server flow;
the managed offer is exercised separately through the same pixel/Scan boundary,
then the actual Service redemption and unchanged auth/revocation handoff. The
private handoff retains `qr_uri`/`release`/`phase`, adds producer `qr_svg` for the
initial managed offer, and preserves `managed-saved` pairing/device bindings,
`{reload, verifyRevoked, context}` and the final managed-revocation handshake.
Other managed phases retain Paste coverage. These are qualified only when the
canonical run is supplied its owned local Service fixture, never inferred from
the direct offer.

The Chromium pinch qualifier did not qualify: six completed CDP input commands
left `visualViewport.scale` at 1 in every sample. Those commands are not counted
as actual gestures, and their capability assertion is removed from the product
gate rather than treated as a scanner regression or made green by setting scale.
The real DOM measurement remains: viewport/loader 844px, empty app root 844px,
document 1688px. That extra extent is an observation, not established native
drift causality; no geometry or accessibility-zoom change is made. The failed
qualifier is retained in external content-free evidence.

The existing WebKit composer/onboarding fixture is the minimal available
loopback route, but no camera-capability assertions are added to that automated
frontend suite: checking fixture globals would not guard user-visible scanner
behavior. No WebKit camera observation, Safari, UIKit or physical capture is
claimed. The native document packet remains paused at the clipboard boundary.

The collateral watcher gate failed in the root-disappearance flow with a Git
`ls-files` error. The stream actor already reports availability changes without
CommandError; its error branch means the root was still classified watchable.
Recursive deletion can remove `.git` before that root disappears, so the harness
was injecting repository corruption while asserting only root absence. The
corrected harness atomically renames the root at the same scan hook. It retains
the exact unavailable-root assertions and still rejects every CommandError.
Watcher diagnostics record only root-directory and root `.git` entry presence
on inventory failure;
no error is swallowed and no actor/connection ownership is changed.
A subsequent recovery test saw version `0 -> 1` after a recorded catch-up rescan.
The returned integrated QR candidate hit the ten-minute watchdog in
`project_watcher_prunes_ignored_build_trees`, after its last successful source
write and before the queued-create race produced another listing. The retained
trace does not distinguish the following listing-hold and child-event-hold waits.
The child hold nevertheless assumes that native events retain paths through
coalescing; `Command::Rescan` does not. The same test deliberately creates native
queue loss. Its listing race now uses a fresh existing server fixture, isolated
from that overflow storm, and holds the watcher after acknowledging a protocol
file observation. Observation does not schedule a listing refresh. A reviewed
intermediate source-write barrier was rejected because releasing that unrelated
write after deletion could mask broken child-removal handling.
The independent listing trigger is now an actual directory creation, observed
processed before the non-refreshing hold. Barrier reads use a separate existing
protocol client so they cannot consume the original client's removal listing.

The real listing must include the unclassified child and settle before deletion.
The test then waits for the actual native removal to enter the worker queue,
releases the non-refreshing barrier, and completes another protocol observation
behind that removal. After the actor's file refresh settles, a second real
connection must receive a cached bootstrap without the child, and the original
connection must receive the removal listing. Cached replay does not rescan the
filesystem; it makes broken removal handling fail on stale visible state instead
of waiting for the watchdog. Content-free logs identify each hold/wait. The
unavailable-root contract, production event classification and watchdog remain
unchanged. The original timeout alone does not prove which wait stalled or that
QR changes caused a production watcher defect.

The controlled regression run removed only the existing uninventoried-removal
branch. The isolated race reached every hold, listed the child, settled, queued
its actual native removal, and then failed on stale cached bootstrap state in
28.517s:595 other native tests passed, with no timeout. Native create, modify and
remove classification each reported zero relevant paths for that missing child.
Canonical removal handling is restored in the final candidate; neither the
original disappearance assertions nor the watchdog was weakened.

A subsequent run passed the watcher but failed the driver's compound
late-publisher cancellation assertion. Its old artifact cannot identify which
predicate failed; a visibility-only run recorded all five predicates true and
is not treated as a causal repair. Independently, the publisher hold introduced
with `5eb3af98`/`v0.9.6-beta.25` matched every non-publication manifest read, not
the pairing operation. A controlled real HTTP read consumed that hold, with
`held_pairing=false`, and deterministically failed before cancellation while
595 other native tests passed. This is a confirmed fixture defect, not exclusive
attribution of the earlier compound failure.

The existing driver now marks the actual unminified `preparePairing` caller at
its manifest fetch; only that test-marked request can consume this scoped hold.
The caller stack is inspected in memory and never retained. An unrelated real
publisher read must finish without consuming the hold. Cancel must then be
acknowledged by the existing reload/admission owner before the held response is
released; the original cancelled outcome, document, draft, redemption and pending
URI assertions all remain. Markers, wrappers and content-free receipts exist
only in this fixture, are removed after the phase, and add no shipped telemetry
or native host change. The actual600s watcher wait and6c's underlying agent hang
remain unconfirmed; neither is explained by this separate HTTP fixture defect.

Versions identify invalidation generations, not content hashes: the watcher
explicitly invalidates every observed file after lost events. Its unchanged-read
assertion now compares two reads after the corrupt-index watcher stops, retaining
exact content and version equality without pinning the earlier watch generation.

Root routes this packet only to AirGUI operator6c's **unpaired spare**, never
the preserved paired A66. Use the existing owned native/inspector route; no
XCTest, host GUI automation, alternate evaluator, reset or release is authorized.
If that route cannot evaluate the existing document, report that limitation;
do not substitute Safari/PWA or an injected camera.

Baseline source is `563a7bbebf3360a1af7cfa7e71b93710ceb5510f`, in workbench
`227488e9-7fa2-4dbb-9280-dc12d9332e5b`, branch
`fix/native-qr-scanner-interaction-20261104`. The ready-to-paste single console
expression is `tools/observe-native-scanner.js`, probe
`thin-scanner-observation-v2`, SHA-256
`a044d876703d00ba282f3dbdf10e2d7b52dca11e652d612d834a54a0dd7c5f4a`.
It admits only the production native entry or E266's exact existing
`qa-thin-nativebf-fc10ed.tycode.dev` native entry, labels that distinction in
every result, and checks the fetched loader/CSS/decoder hashes against its
embedded baseline before becoming ready.
Those network hashes identify fetched source, not proof of previously executed
cached bytes. Preserve the existing route's load/source receipt alongside them.

1. Record the installed build and the spare's actual window/orientation in the
   existing episode receipt. E266 is QA-origin simulator evidence, not TF4
   production/physical-camera acceptance. Its original Allow → moving test
   pattern baseline has zero QR/pair attempts and zero pinch gestures; the
   retained helper lacks two-contact input and the exact-device SDK capture
   failed before input, then closed. Do not count six samples without six
   actual gestures as pinch evidence. Open its normal initial Scan QR camera; keep
   one Root-authorized current host QR in view. Preserve the original screenshot
   for Root, without copying/exporting the QR, clipboard, page text or storage.
2. Evaluate the entire file once in that actual native document. The sole
   success result starts `thin-scanner-observation-v2 ready (e266_qa)` or
   `thin-scanner-observation-v2 ready (production)`. Any error is STOP,
   not permission to change page state or retry using a different route.
3. Evaluate `window.tydeScannerObservation.sample(0)` before gestures. After
   each of six real pinch-and-release gestures, preserve the original screenshot
   and evaluate `.sample(1)` through `.sample(6)` respectively. Each evaluation
   returns one JSON string. There are no timers, input injection, zoom resets,
   lifecycle nudges, native overrides, camera replacement or retained pixels.
4. If normal recognition advances the page, stop sampling and preserve that
   original transition/error screenshot instead. Continue only through the
   normal canonical pairing route under Root's episode authorization. Root
   personally inspects original UI screenshots; JSON is not UI signoff.

The probe reads one current real video frame per sample, calls the bundled
decoder without altering the production scan, and discards pixels/payload. Its
only output is bounded numeric geometry/readiness, booleans and fixed failure
stages. It rejects nonnumeric, out-of-order, extra or eighth arguments/samples.
**Never pass a raw QR/link as an argument, return its text, export camera pixels
or include any credential in the handoff.** Return only the seven JSON strings,
original screenshot custody references and Root's UI verdict. A simulator/no
camera result cannot establish physical recognition. JS viewport/document
geometry is not UIKit contentOffset/zoomScale; obtain those only if the existing
route already exposes them, and otherwise state the gap explicitly.

Native Swarm shared images use the existing in-document typed image viewer,
including Close/focus restoration and modal navigation exclusion. Browser/PWA
image anchors remain unchanged. No data-URL document-navigation exception is
added to the native host.

Native scanning uses the bundled jsQR decoder as its single implementation;
a missing bundled decoder fails explicitly, without trying a different decoder.
The browser's existing capability/selection behavior remains unchanged.
Native camera request failures use the platform DOMException name to render a
concise actionable state, not its implementation-specific message. In particular,
NotAllowedError reports access is not allowed; it does not assume which permission
or policy denied access. Browser error presentation remains unchanged.

Source26 iPad portrait Paste focus has a confirmed paint defect: the original
framebuffer lacks the field/placeholder/action while its matching native AX tree
still reports the field and Continue inside the visible screen. That is not proof
of hidden DOM, an incorrect keyboard inset, or a compositor cause. The DEBUG app
can opt into WebKit's supported inspector with TYDE_IOS_WEB_INSPECTOR=1; it is off
by default and unavailable in Release. Source27 had a later unconditional DEBUG
assignment that overrode this opt-in; removing it restores one canonical policy,
not a claim that Safari discovery is repaired. Inspect only the owned app and content-free
geometry/styles; no storage/credential/text capture or inspector styling edits.
Original native typing and keyboard ownership remain untouched.

### Published executable authority

The server's native descriptor resolves its actual release in the fixed trusted
`https://tycode.dev/tyde/manifest.json` catalog. It validates that record's actual
protocol, transport and native bridge ABI and hashes the complete sorted
executable/SRI map. Independently built embedded direct-host assets are not
assumed byte-identical to the CDN. The native shell still fetches policy freshly
and verifies every executable SRI before import. Missing publication is an
explicit native admission failure; local/PWA direct hosting remains unchanged.
The sim uses an owned HTTPS publisher/proxy/CA at that same canonical URL, not a
production network override or a fabricated artifact descriptor.

### Immutable installed-shell guard

`tests/fixtures/native-bootstrap-integrated-v1.tar.gz` retains the integrated
ABI6 shell candidate (UI78 public layout/sidebar contract plus typed listener
readiness). The earlier `native-bootstrap-shell-v1` bytes remain an unreleased
prototype, not the accepted baseline. The archive contains generated Swift,
the actual bundled native bridge and a consumer contract generated from those
same Rust native types. The canonical real-DOM/server flow validates actual
client requests and native intents against that unchanged archived contract,
including its closed enums and required fields; it never regenerates the old
contract from current source. It does not execute Swift Codable/UIKit or claim
native Keychain, input, window resize or physical fold coverage.

The same coarse flow covers exact published assets, retained pairing and core
conversation through real process/store handover, host switching with an
inactive saved record, unsafe-draft refusal before redemption, publication
hold/failure/recovery, native auth/revocation actions, and fail-closed pack
faults. Source-upgrade evidence additionally supplies hash-checked real older
server and full-client artifacts. These are compatible unpublished source
candidates, not a claim that published beta20/native3 supports the new baseline.
The first supported release is the release containing this bootstrap/ABI and
Service contract; ordinary global wire releases do not ratchet that minimum.

The same flow advances only an injected monotonic clock beyond a discovery
lease while leaving the real offer clock unchanged. Exactly one redemption
then uses fresh paired-device authority. Managed admission also holds the
real publisher through two unchanged-source restarts: one with a registered
pending offer and one with an offer created during the hold. Both must
complete normally after publication, without replacing their QR or inventing
registration from the old request.

### Explicit isolated QA build

Production bootstrap/publisher remains canonical. For a dedicated normally
trusted HTTPS QA origin, `./dev.sh ios-bundle --qa-origin HTTPS_ORIGIN` binds
the same loader, app-bound domain, native HTTP/auth authority and integrity
manifest at build time. Its Debug-only resources fail compilation in Release
and identify the separate Tyde QA bundle. There is no runtime origin selector,
trust override, loader fork or old-client fallback.

`./dev.sh mobile-direct-export ABS_NEW_DIRECTORY --qa-origin HTTPS_ORIGIN`
builds the existing bounded source-linked host with its publisher catalog at
that same origin. The server build refuses QA configuration in Release. Use
the normal direct bundle layout under `/tyde/`, with the actual source-built
versioned assets and manifest/SRI identities; never overwrite a published
release record. A dedicated QA catalog may expose these genuine unreleased
source candidates without a production deployment. Reuse the identified QA
app binary across host revisions. GUI interaction remains device-scoped MCP.

### Mobile decoder cache identity

The root-shell decoder is published at
`decoder/jsqr-<sha256>.js`, with matching SHA-384 script integrity. Its filename
must change with its bytes. The versioned client copies that same source;
there is no second maintained decoder or legacy-URL alias. The shell worker's
precache list, deployment metadata keys and scanner observation follow the
same filename. Generated version manifests still cover every executable.

The original worker classifier (`2675f222`) treated `/tyde/vendor/` as a
versioned bundle, and direct hosting (`4cc043e2`) made every `v`-prefixed
directory immutable. Decoder correction `ea654f4d` changed bytes at the old
`vendor/jsqr.js` URL from SHA-256 `30412062…` to `5f0b8270…`; successful
versioned-bundle SRI did not establish the root scanner's identity. The new
URL names the corrected bytes without cache-busting queries, purges, permission
changes, or alternate loading paths. Both classifiers now require a release
path segment, leaving non-release assets in the shell regime.

The direct-hosting protocol/HTTP flow guards mutable vendor and version-like
paths alongside immutable release bytes. The real browser flow seeds stale
vendor bytes in the verified-bundle cache, fetches the current decoder through
the installed worker, and verifies its URL digest, script integrity and loaded
global. Existing boot, revocation and fail-closed bundle checks remain intact.

The decoder directory deliberately does not start with `v`: an installed old
worker would reject an uncached native script even at a new hash under
`vendor/`. The browser upgrade flow starts with the frozen pre-fix worker,
withholds worker publication until after the first `?native=1` document,
and requires that document to execute the corrected decoder. This reproduced
`window.jsQR` being unavailable with the hash still under `vendor/`; moving
the canonical asset to `decoder/` does not weaken the old worker's verified
bundle enforcement.

### SDK shell generation identity

The static publisher and direct-bundle builder use
`web/deploy/assemble-shell.mjs`. A SHA-256 generation names the shell source
snapshot (including the document, worker, module names and bytes). The root
`index.html` selects `sdk/<generation>/loader.js`; config, CSS and decoder
references select that same generation. Static and dynamic SDK imports remain
relative within that directory. The generation is independent of client release,
global wire version, bootstrap1 and ABI6. Exact-client manifest/SRI admission,
worker scope/script URL, native secrets and pairing storage are unchanged.

Publish every immutable generation member before the root selector. The S3
publisher checks generation presence/cache metadata before publishing the
selector; it never deletes old generations or rewrites legacy root modules.
Direct builds retain prior published assets and reject different bytes at an
existing generated path. Root document/worker writes use atomic replacement. The worker precaches
identified members, not mutable SDK modules; no new CacheStorage epoch or purge
is introduced. An older controlling worker passes
these non-release paths through without invoking native verified-client rules.
The worker remains at `/tyde/sw.js` for installed clients and push subscriptions.

Origin root cache headers do not control all delivery layers: observed edge
responses allowed four hours of freshness. A stale *generated* document still
names its complete old generation; neither prompt revalidation nor HTTP-cache
invalidation is required for coherence. A missing member fails module loading,
not selection of another URL. No cache clear, query buster, compatibility module,
new ABI or alternate import path is introduced. Direct hosting may still use its
existing short shell TTL for `sdk/`; identity does not depend on the TTL.

This does not repair a previously cached, unversioned document retroactively.
In particular, the recovered six-module0904 native cache graph is coherent;
its selection by the failed native document and the original TypeError cause
remain unproved. Linux HTTP/module/worker and archived ABI-contract flows are
not physical native acceptance. Same-origin QA hosting and retained-device
requests stay with their designated custodians through the PM.

### Live native Back surfaces on the stable platform baseline

Bootstrap1/platform ABI6 is stable across nightly host/client releases. The
proposed ABI7 minimum was withdrawn before publication: Back does not require a
new discovery, HTTP, credential or presentation baseline. Server descriptors,
Service registration/resolution and full-client manifests remain ABI6. There
is no ABI range/floor reinterpretation or Service/account change. A publisher
still selects the exact authenticated host's client/wire; that is NOT an exact
native-binary release lock. Normal nightly wire/client updates do not raise the
native minimum.

The new binary advertises generated `backSurfaceVersion: 1`; the current full
client explicitly echoes `back_surface_version: 1`, with typed optional
navigation/readiness revisions and destination. Only this acknowledged contract
admits native live Back. Unknown extension versions fail before full-client
fetch/execution. Missing capability means the supported base contract, not an
error recovery or a second renderer/reducer. Neither endpoint failures nor
missing destination data select another path. A declared but malformed surface
contract fails explicitly. The same full client, AppState, registry and editor
serve both consumers.

| Installed native / client | Old/current server29 (ABI6) | Updated nightly host/current client |
| --- | --- | --- |
| TestFlight8, platform6, no Back extension | Ordinary existing paired operation; known old-binary native Back flash remains | Ordinary paired operation remains supported; current client emits no live-Back acknowledgement |
| New Back binary (formerly proposed ABI7; now platform6 + Back1) | Existing ABI6 full client boots; canonical Back taps remain supported, live gesture withheld without client acknowledgement | Same baseline admission; live native pop enabled only after Back1 destination readiness |

A native update is needed once for the new native compositor/gesture behavior,
not once per nightly. Updating only the host/client cannot fix old Swift content
ownership. No reset/re-pair, store migration, alternate old-client selection or
credential rewrite is required. New binary + old full client intentionally has
no new interactive-pop claim; the existing canonical Back intent/tap remains.

`native-bootstrap-integrated-v1` is the unchanged UI78 ABI6 consumer, not an
exact TestFlight8 fixture. `native-bootstrap-testflight8-v1` maps real retained
Build8 archive bridge/manifest bytes and its exact generated source consumer
from18edb87d, IPA SHA256 e214935b417cc72a7728bc39de3dc91cc709f49a1fce919905add18428e4dc54.
Its custody names the activation receipt and source manifest; real browser proof
is not physical TestFlight execution. `native-bootstrap-compatible-back-v1`
freezes the independent new platform6/Back1 consumer. The existing real-server
bootstrap/upgrade journey keeps the old integrated consumer and exercises both
mapped consumers before/after host restart, authenticated discovery and frozen
serialization. The inline mounted Wasm journey covers ordinary base-contract
Back/draft/selection and Back1 acknowledgement, progress and canonical commits. Unknown Back2 is refused before any
full-client fetch. Literal old server29/native installation and original native
pixels remain the sole GUI custodian's explicit matrix, not inferred from schema.

The workspace WK remains in the host ancestor across pops and tabs, with viewport
from the current public safe-area guide. One native edge recognizer drives a real
UIKit percent-driven pop even with client-owned/hidden-bar chat; stock gesture is
disabled. Modal/open-sidebar states exclude Back. The live target comes from the
same canonical ancestry that completion commits. Its typed chrome carries root
health or the parent chat's Back label, health, sidebar and action ownership;
prepared and active chat chrome use the same native dock renderer. Transition
bar visibility uses the same header/dock ownership rule as settled rendering.
Cancel changes presentation only; Complete commits once, qualified by source revision. Layout readiness and
injected-progress DOM tests do not prove native raster. Sole1f must record genuine
Hosts and hidden-bar root/nested chat entry, cancel/complete, continuously live
destination pixels and exact draft/range/photos; Root owns original-frame signoff.
No Back tap substitutes for a pan in that live-surface acceptance matrix.
