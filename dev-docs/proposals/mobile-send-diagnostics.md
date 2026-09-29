# Mobile Send investigation and beta diagnostics

Status: historical investigation; device diagnostics removed 2026-09-29.

Tyde does not collect device telemetry for its maintainers. The automatic
beta recorder, legacy query opt-in, composer observation hooks, app/Settings
controls, and file-sharing/download export have been removed. The evidence
below describes the former implementation, not current behavior.

The mounted Chrome and WebKit flows now require no recorder or diagnostic
controls, including on iOS standalone and with the former query opt-in.
Draft retention, ordinary Send/Queue behavior, shell geometry, header reachability,
and connection/navigation behavior remain covered. Assertions that demanded
Recording or exported captures certified the removed feature and contradicted
the new privacy contract; they are replaced with absence assertions rather
than assertions about a hidden or stopped recorder. The removal regression
failed before the fix: editing/rejected sends created a recorder object, and
AppSurface rendered the diagnostic status surface. Retained red-run evidence:
`target/dev-check-logs/run-20260929T211938Z-2039879/12-wasm-browser-tests.log`.

## Historical evidence

## Mounted Chrome investigation

The server remains the source of lifecycle truth. `ChatInput` reads the native
textarea on every input, including composing InputEvents. Its draft effect and
`prop:value` can write back; there is no compositionend/beforeinput handler.
The shared shell sizes on input but does not write text or special-case
composition. Send reads the draft signal, not a native-value fallback. The
primary node is stable; Send/Queue/Cancel are reactive projections. Streaming
text alone is not running state. The menu's Steer item is a separate node.
The rim has pointer-events disabled; the fixed menu backdrop intentionally
intercepts outside taps while open.

Two existing inline mounted wasm flows are extended (no new unit tests):

- `components/chat_input.rs`,
  `a_double_tap_on_an_agent_message_emits_exactly_one_frame`: actual constructed
  InputEvents with isComposing=true and insertCompositionText, immediate and
  next-task taps while composition remains open, insertReplacementText between
  down/up, an initially empty composer after a task, deferred local admission,
  double-tap suppression, rejection retention, CompositionEvent completion and
  final noncomposing input without another submission. Payload equality,
  visible/native draft, signal equality and custody counts are asserted.
  Immediate same-task cases start with an already enabled nonempty draft;
  they do not purport to model browser task scheduling for an initially empty
  disabled button.
- `app.rs`, `one_chat_mount_preserves_transcript_and_composer_draft`: actual
  AppSurface in a 393x852 iframe with production Tyde and vendored shell styles.
  Thirty wrap/newline/capped-height/shrink contacts at requested 0/50/250 ms,
  both after input and with input between down/up. Actual elapsed times through
  release must be below 300 ms, not just the requested timer. Assert geometry,
  max-height/internal scroll, native draft equality, stable connected primary,
  enabled state, current-center and original-contact hit tests, one correct
  SendMessage and clear after local admission. Typed AgentActivityChanged
  envelopes plus StreamStart/StreamDelta reducer events cross contact edges;
  Thinking/Idle render Queue/Send without Interrupt even with streaming text.
  Ring on/off, menu open/close, Steer disappearance and animated drawer open/close
  all retain the appropriate hit contract. Overlay toggles send no frames.

The admission seam is above the backend boundary, not backend coverage. It must
remain continuous across accepted submissions: restarting it reuses local IDs
and overwrites the same custody-map entry. An added count assertion exposed
that fixture mistake; retaining the original fixture fixes it without weakening
that assertion or changing production behavior.

The initial and expanded flows passed full `./dev.sh check`. A subsequent
expanded run failed only the fixture count above while the shell flow passed.
Its retained measured shell evidence is in
`target/mobile-send-local-20260926/measured-shell-evidence.txt` and
`measured-shell-summary.json`. Ten releases per requested delay were measured:
0.575–1.495 ms, 50.735–51.635 ms, and 250.875–251.805 ms respectively.
The shell heights were 44 px short, 152 px six-line, and 180 px capped.
These are Chrome synthetic event measurements, not iOS tap latencies.
Final whole-check results belong in the milestone report; earlier green runs
are not validation of later edits. No assertions are relaxed for timing misses.

## Offline dictation capability audit

The dev server is Linux with no xcrun. The retained Device Farm harness uses
native key-cap taps and separate native Send requests; it has no microphone
injection path. The pinned-family [XCUITest 10.43 command reference](https://appium.github.io/appium-xcuitest-driver/10.43/reference/execute-methods/)
describes startAudioRecording as recording a host hardware input, not injecting
speech into an iPhone keyboard. siriCommand supplies recognized text to Siri,
not keyboard dictation. The [AWS remote-access feature list](https://docs.aws.amazon.com/devicefarm/latest/developerguide/remote-access.html)
does not establish an audio-input route. This audit establishes **no supported
route in our setup**, not that every possible device service lacks one.

## Real WebKit and physical-device scope

The existing Playwright FixtureApp harness now mounts the actual Tyde composer
in real WebKit through the canonical `./dev.sh check` wasm stage. A dedicated
project disables retained screenshots, traces and video (one ephemeral browser
screenshot exercises disk output preflight). A successful fixture build is
required before starting its server, so a failed build cannot serve stale assets. Typed fixture activity events
and stream reducer events exercise UI behavior above the backend boundary.
Programmatic composing inputs, replacement across contact, running/idle changes,
0/50/250 ms grow/shrink contacts, stable button and center/original-point hits,
and menu interception passed before device allocation. These are synthetic DOM
contacts, not native dictation or native click-delivery evidence. Timing
assertions require measured elapsed time below 300 ms. The preflight full check
is `target/dev-check-logs/run-20260926T184502Z-4184903`.

The one authorized BEFORE allocation used the packaged actual Tyde fixture.
It verified standalone mode and reached composer focus, but failed harness
qualification before the first Send trial: the initial screenshot destination
was written before its directory was created. Consequently none of the three
planned composition/replacement/lifecycle/resize native-Send trials ran. There
was no repeat allocation and no original application reproduction. The result
must not be summarized as passing device coverage or native sub-300 ms coverage.

That allocation used 9.08 actual / 12 conservative minutes. Including the
previous ledger, totals are 69.04 actual / 84 conservative minutes. Cleanup
verified the temporary bucket, role and project absent. Sanitized outcome and
cleanup evidence are under
`target/ios-send-composition-20260926c/artifacts/devicefarm-tyde-send-composition-20260926c/`
(`milestone.json`, `cleanup-verification.json`, `result-ios265-1.json`). Raw cloud
state contains private identifiers and is not part of an export or handoff.
The six historical ordinary native sends at 475–761 ms do not establish
composition, dictation, running-state or sub-300 ms behavior.

## Final diagnostics-only device acceptance

The final authorized allocation ran the compiled beta gate on a genuine iPhone
14 Pro Max / iOS 26.5 Home Screen PWA without diagnostic query opt-in or platform
spoofing. Automatic Recording/schema 3 passed. Native controls passed Stop,
Clear, no-host persistence, explicit Start and restored-chat accessibility.
A screenshot-qualified software keyboard key produced trusted input, and native
contact capture was observed. The in-memory sanitized buffer contains 60 records
and no privacy sentinel; it is **not** an exported File or proof of retrieval.

The trial stopped at a real diagnostic-controls placement defect: after the
keyboard opened, the global fixed disclosure was not visible. Shell geometry
was y=0/height=487, body y=-386, visualViewport offsetTop=386. Native lookup of
the disclosure returned not displayed; no Export click, share sheet, download
or export-Blob inspection occurred. This is not an original Send reproduction
and does not establish that the native share API is unsupported. No second
allocation is authorized. The placement regression replays the observed viewport
projection in the real WebKit DOM, separately labeled as a simulation. It failed
before the CSS fix in `target/dev-check-logs/run-20260926T200007Z-2962714`:
the disclosure was 384 px above its shell (the local viewport still allowed
hit testing there; the failed assertion requires it to be inside the shell). The initial fix anchored the diagnostic disclosure inside the projected shell.
Review then demonstrated that its absolute positioning obstructed the header;
the final correction reserves a normal-flow row ahead of the shell regions and
caps its expanded height using the shell container height. No additional viewport
controller, shell/vendor change, focus manipulation or Send fallback is added.
The original failing visibility/hit assertion remains, followed by actual
Stop/Start clicks and the existing full export flow. This red/green is for the
new diagnostic UI defect, not the original Send report. No native after-run is
authorized, so local WebKit green must not be labeled a native after result.

The screenshot-destination defect from the prior run did not recur. The shared
`tools/devicefarm-output.mjs` writer creates the nested destination before every
write. Both canonical WebKit and the manual package preflight actually write and
read back an image through that path from an absent directory; scheduling rejects
packages without the successful preflight. The new keyboard screenshot was
successfully created on-device. The manual harness source is retained separately
in the review evidence bundle; it is not an alternate Tyde validation entry.

Final batch: 10.00 actual / 12 conservative minutes; all batches: 79.04 actual /
96 conservative minutes. Bucket, role and project absence verified. Evidence:
`target/ios-send-diagnostics-20260926d/artifacts/devicefarm-tyde-send-diagnostics-20260926d/`
(`milestone.json`, `cleanup-verification.json`, `result-ios265-1.json`). Native
export remains a stated validation gap, not a passing acceptance claim.

## Implemented automatic capture and retrieval

- **Gate:** AppSurface owns the capture session. The compile-time included root
  package version is parsed with the existing typed release-version parser;
  automatic capture requires its beta prerelease plus iOS/iPadOS platform and
  standalone display detection. No raw UA is read into records. No loader
  changes, persisted consent flag or query parameter is needed in the intended
  beta PWA. The pre-existing query remains an explicit developer opt-in.
- **Visibility:** an app-level status disclosure displays Recording, Stopped
  or Expired. It exposes Stop, Clear, Export and Start new capture in every
  app mode, including onboarding, pairing and the no-active-host picker. The
  existing Settings surface offers the same controls and privacy notice.
  The disclosure occupies its own normal-flow row, not an overlay. The existing
  shell measures the remaining regions and continues to own viewport projection.
  No duplicate visual-viewport controller or fixed header-height offset is added.
  Closing it removes the expanded panel from hit testing; the reserved status row
  never overlaps the title, subtitle, rename controls or Send contact region. Stop disables observers
  while retaining the buffer until expiry. Clear stops capture, cancels pending
  samples and clears records without renewing the lifetime. Neither Stop nor
  Clear restarts on navigation or reconnection. Start new capture is the explicit
  re-enable and resets the clocks and buffer.
- **Retention:** memory-only rolling ring of at most 2,048 records and 256 KiB
  serialized JSON, including 1 KiB reserved for envelope overhead. Byte pressure
  can evict before the count limit; oldest records go first and dropped count
  is visible in the export. One 15-minute timer plus foreground/event/export
  checks enforce expiry. Monotonic performance time drives relative timestamps;
  a wall-clock deadline additionally expires captures after suspension. Expiry
  stops and clears. Reload destroys the old capture and a qualifying beta starts
  a new one. Nothing is written to local storage or sent automatically.
- **Schema 3:** explicit field construction at storage and export. Allowlisted
  phase/action/guard/role/input-category/lifecycle values; relative timing and
  local event ordinal; trusted/defaultPrevented, composing and composition-open
  observations; native/signal equality; bucketed lengths (0, 1–32, 33–128,
  129–512, 513+); attachment presence, disabled/busy/loading/terminated and
  same-primary booleans. Unknown input/pointer categories become `other`.
  Lifecycle comes from typed state, never transcript inference.
- **Geometry:** a coalesced scheduled snapshot stores age, quantized bounded
  dimensions and current-center/original-contact hit roles. One transient point
  is retained only for hit testing, never exported. Samples are explicitly later
  observations, not geometry at the exact native-contact instant. One geometry
  timer, one bounded post-dispatch queue (16) and one expiry timer are owned and
  canceled. Post-dispatch cancellation observes the same event without changing
  it. Composer readers detach on unmount; app ownership survives navigation.
- **Privacy:** no message text, audio, input data, key values, hashes, selection,
  identifiers, credentials, URLs, HTML, arbitrary attributes, raw errors,
  console logs or automatic uploads. Export never spreads raw app/event state.
  Listener options are passive. No focus/default/input/click/retry behavior is
  changed. Local admission marks mean local acceptance, not backend delivery.
- **Export:** the actual Settings gesture creates a sanitized local JSON File
  with a generic filename. Platforms supporting file sharing use the share
  sheet; others use a Blob download. URLs are revoked, file contents are not
  cached, and cancellation/unavailability renders a static safe message. The
  user chooses whether and where to save/share. Async completion cannot update
  a destroyed/restarted capture. WebKit download and mounted File/share-boundary
  tests pass. Physical-iOS export was not reached because of the viewport issue
  above; neither native share-sheet opening nor file delivery is claimed.

## Diagnostic acceptance evidence and remaining gate

The existing mounted app flow asserts automatic capture without a query,
negative gate behavior, privacy sentinels in input/data/keys/attributes, ring
rollover and serialized byte bounds, monotonic time, contact composition,
aged geometry, actual File export, Stop/Clear/re-enable, expiry on foreground,
route persistence and cleanup. Existing admission and composing regressions
remain intact. A second real-WebKit flow exercises automatic gating, normal
Send and the real local download boundary without reloading away its records.

The automatic-visible-capture assertion genuinely failed before implementation
in `target/dev-check-logs/run-20260926T190020Z-433053`, but that bounded log was
subsequently removed. It is not claimed to have been recovered. The unchanged
assertion was rerun through the canonical check against a minimal query-only
pre-feature gate in `run-20260926T201724Z-3805875`. Its complete raw stage log,
source patch and provenance are retained outside the bounded log area in
`target/mobile-send-rereview-20260926/raw/` and included in the handoff bundle. This is red/green for a
missing diagnostics requirement, **not** an original Send regression. The
full wasm suite (314 tests) passed in run `20260926T191715Z-1270788`; that run's
new WebKit export test failed a test locator (Settings is a tab, not a button).
The locator was corrected without changing production behavior. Final check
status and any remaining failures belong in the handoff, not an inferred pass.

### Review corrections

The former app/WebKit assertion always expected Recording. That contradicted the
production compiled-version gate on stable releases. Both mounted flows now first
assert the actual compiled channel: beta iOS standalone records automatically
without an opt-in query, while stable shows no capture UI and creates no recorder.
Only afterward does an explicit test-owned beta context exercise the full recorder
scenario on stable builds. A forced stable context additionally mounts the entire
AppSurface and asserts both hidden controls and absent recorder, rather than
calling the gate directly on an isolated leaf. Channel overrides are compiled
only for tests or debug UI fixtures, never normal release production. The stable
metadata validation run temporarily changes the local manifest/lockfile and then
restores them byte-for-byte; it cuts or publishes no release. The actual stable
metadata run passed all 15 canonical stages in
`target/dev-check-logs/run-20260926T203554Z-1256528` (269 seconds), including
314 mobile wasm tests and both real WebKit flows.

The header-overlap regression failed in the real WebKit mounted flow before the
layout correction (`run-20260926T202250Z-26284`): panel bottom 46 px, header top
8 px, overlap and title/subtitle hit failure. Its raw stage log and geometric
probe are retained in the durable review bundle. The same assertions cover
Recording/open/closed, Stopped, Expired, keyboard projection, and actual rename
input/Save/Cancel reachability. Their contents are never printed by assertions.
The shell's existing resize observations account for the reserved row; production
Send, shared-shell/vendor, loader and dispatch behavior remain unchanged.

The session-settings mounted flow drains already queued work before opening its
outbound capture fixture, preventing a prior flow’s asynchronous ClientError
from contaminating its one-edit/one-frame assertion. That assertion remains.
There are no source loader or dispatch changes.

A full-check rerun also exposed an existing server-test transport race: writing
approval on a WebSocket does not acknowledge its processing before a separate
HTTP await request. The server correctly returned AwaitingUser. The existing
`agent_control_http_await_returns_while_exit_plan_mode_is_pending` flow now
observes its existing approval completion/idle events before the HTTP idle
assertion. Pending, idle, completion and follow-up assertions remain unchanged;
no server production or backend code changes. The neighboring active-resumption
wait test remains intact. Raw failure evidence is retained in the review bundle.


Existing MobileShellErrorBanner and transport diagnostics explain startup and
admission errors, not the keyboard/contact chain. This recorder complements
those surfaces and remains strictly observational, never application/server
state authority. Existing query hooks and investigation flows are retained.

Before landing: resolve remaining acceptance issues, make one scoped commit,
pass `./dev.sh check` on that committed workbench, obtain two fresh reviewer
approvals and explicit owner landing instruction. No workbench deletion.

### Clean-main integration gate: restart observation

The first clean-main check after landing the reviewed diagnostics commit failed
in the upstream `non_cancellable_restored_child_startup_holds_its_parent` server
sim. Its child bootstrap reported Thinking and Continuing; the subsequent live
idle marker arrived, but the observer had not seen a live continuation-text
delta. Its predicate accepted an already-idle bootstrap or live text followed by
idle, omitting the valid attach-between-text-and-idle case.

The test observer now retains typed `AgentActivityChanged` events and requires
both the Continuing phase and the latest authoritative activity Idle. This is
not a relaxation to timeout or text matching: an AwaitingUser or Thinking agent
cannot satisfy it. Existing continuation-failure and provider-acceptance-order
assertions remain unchanged. The server emits activity edges at the top of its
actor loop after status changes; bootstrap carries the starting activity. No
production server or diagnostics behavior changed. The raw failed native-stage
log is retained at `target/mobile-send-main-gate-20260926/raw/clean-main-native-red.log`.
