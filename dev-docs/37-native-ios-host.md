# Native iPhone/iPad host

Tyde's native app is a remote client, not a phone-hosted server or agent CLI.
`ios/Tyde.xcodeproj` is a dedicated UIKit host. The existing desktop Tauri
application is not an iOS target. Native APNs and a Swift rewrite of the mobile
screens are not part of this feature.

## Early TestFlight preview

The user authorizes an early preview with known gaps while native QA continues;
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

`./dev.sh ios-bundle` builds local reviewed executables, generates Swift bindings,
records exact package release/protocol/bridge versions, hashes every asset,
creates CSP and binds the result to a source manifest. Xcode consumes
`target/ios-client`. It never includes the PWA downloadable release-switch loader.
The native package marker requires the native bridge; a missing/mismatched host
fails at boot, not as a browser-mode substitute. Host release/protocol mismatch
fails visibly; the app never downloads another executable client.

The document keeps the stable `http://127.0.0.1:18736` origin for its existing
app-scoped IndexedDB and bridge permissions, but does not bind or request that
TCP endpoint. `WKWebView.loadSimulatedRequest` loads the hash-verified packaged
HTML at that origin. The immutable resource listener always asks the OS for one
loopback port, before loading the document. Independent Simulator processes can
coexist; there is no port search, alternate-port retry, shared asset service,
run queue, storage migration or copied credentials.

Native packaging moves the generated module bootstrap into a bundled module,
resolves its exact hashed Wasm relative to that module, and substitutes only the
assigned resource origin into script/style URLs and the document's early CSP
meta. Cross-origin resources use anonymous CORS and existing integrity hashes;
the listener requires its exact Host and the canonical document Origin. It does
not serve HTML. A packaged Blob worker keeps voice's worker same-origin, while
worklets use the assigned resource origin. Browser packaging/media stay unchanged.
The native fetch seam permits only this resource transport or existing typed
HTTPS networking; document-origin HTTP is an explicit error, never a peer request.
All bridge/input/media permission checks still require the original main-frame
origin. Unexpected bundle format, missing templates, integrity, listener and
native platform failures surface explicitly. The image CSP permits HTTPS for
reused Markdown images; executable scripts remain restricted to packaged assets.
Element image loads do not pass through the JavaScript fetch bridge. The native
test driver observes a
real platform error as a terminal failure instead of waiting only for navigation.

External HTTP uses the app's sandbox-scoped URLSession: the managed API or pinned
HTTPS direct-pair endpoint only, no redirect following. Direct protocol WebSockets
and managed WebRTC continue through the existing Wasm transport/handshake.
Direct host origins come from the complete scanned HTTPS pairing link and are
shown before redemption. Browser records still use their document origin; native
records require their own pinned origin, never an inferred host.

Pairing secrets go through the existing secret-store seam to app-scoped Keychain,
WhenUnlocked/ThisDeviceOnly. Host summaries remain in origin-scoped IndexedDB;
they contain key references, not secret copies. No primary app/browser credentials,
profiles or sessions are imported. Explicit user-activated HTTP, HTTPS and mailto
links open in the external application, matching the reused Markdown renderer;
they never replace the retained document. HTTP links do not enable HTTP pairing,
platform bridge requests or embedded document navigation. The original native
allowlist omitted HTTP and incorrectly turned a supported content link into a
platform-error overlay; only that explicit external-link branch is corrected.
Unexpected document navigation and web-process termination still fail explicitly.
Native acceptance must activate an actual rendered HTTP link, observe external
application opening, then return to the same retained chat/document/draft and
connection without a platform-error overlay, reload or new pairing. No new typing
episode is needed merely to prepare that link; use genuine existing chat content.
Camera/microphone prompts are restricted to the bundled main frame. Native
push is not implemented; PWA-only notification advice is not shown in native.

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
