# Local startup and UI stall diagnostics

## Automatic local desktop log

No Debug setting or capture is needed. The desktop shell writes to the existing
hourly `~/.tyde/tracing/tyde-shell.log.YYYY-MM-DD-HH` files (on macOS,
`/Users/<you>/.tyde/tracing/`). `TYDE_TRACING_DIR_PATH`, when configured,
selects the directory instead. The default `info` filter includes these lines;
a restrictive `RUST_LOG` can exclude them. Shell logs retain the newest 48
hourly files, not a guaranteed 48 hours of history. Other log prefixes keep
their existing retention. File-logger initialization errors fail startup
explicitly on stderr; there is no console-only substitute.

Look for `ui.startup`, `ui.diagnostic`, and adjacent `webview.lifecycle` lines
around the incident. These new lines are pasteable, content-free diagnostics:

- `ui.startup layer=native`: begin/end, duration and completion/aborted status
  for environment setup, macOS WebKit defaults, shell build, host-store load,
  host startup, native media and updater initialization. Nested phase durations
  overlap: do not add them. A begin without an end narrows where startup stopped.
- `ui.diagnostic event=enabled`: version and actual sampling/retention policy.
- `layer=frontend ... record=... event=startup`: monotonic `since_wasm_ms`
  markers for WASM entry, mount, highlight warmup, host listener installation,
  host-list refresh return, connection scheduling and readiness acknowledgement.
  These are concurrent startup paths, not a serial waterfall. Refresh return
  does not imply success; scheduling does not imply connected. Readiness is not
  proof of paint or completion of host replay. Before WASM runs, only native
  phases and page-load events can be present.
- `layer=frontend ... event=event_loop_gap`: a visible-page timer interval of
  at least 1250ms (nominal 1000ms). Includes focus, the longest completed
  instrumented synchronous operation >=100ms since the preceding tick, and
  the number of pending bridge waits. The operation duration is inclusive;
  it is not a union, sum, CPU sample or proof of causation. Bridge waits are
  **not** blocking time. `longest_work` carries `operation`, `duration_ms`,
  `frame` (the protocol frame kind for `host_receive`/`host_dispatch`, else
  `null`), `bytes` and `items`. Their meaning depends on the operation:

  | `operation` | `bytes` | `items` |
  |---|---|---|
  | `host_receive` | received line | 1 |
  | `host_dispatch` | 0 | 1 |
  | `markdown` | source text | inline tokens |
  | `syntax_highlight` | source text | 0 |
  | `worker_submit` | 0 | lines |
  | `worker_receive`, `composer_input`, `composer_resize` | 0 | 1 |
  | `global_keydown` | 0 | 0 |
  | `file_tree_build` | 0 | listing entries |
  | `chat_tab_binding` | 0 | 1 tab-to-agent lookup |
  | `chat_row_projection` | 0 | projected loaded row handles |
  | `chat_context_scan`, `chat_context_reported_scan` | 0 | rows actually visited |
  | `chat_ink_layout`, `chat_virtual_window` | 0 | loaded rows supplied to calculation |
  | `agents_sidebar_projection` | 0 | projected agent rows |

  No other details or arguments are recorded. The desktop shell drops a record
  whose `frame` is not a protocol frame kind. Example:
  `{"event":"event_loop_gap","interval_ms":1690,"expected_ms":1000,"focused":true,"longest_work":{"operation":"host_receive","duration_ms":610,"frame":"project_bootstrap","bytes":3580000,"items":1},"pending_bridge_calls":0}`.
  The selected deferred chat and sidebar projections below feed this same bounded
  aggregate, including when detailed capture is off. They can run after
  host dispatch has returned; receive/dispatch timing cannot cover that work.
  Their counts are not persisted session size, mounted rows or bytes cloned.
  Browser layout, paint and other uninstrumented work remain uncovered.
  `focused` is `document.hasFocus()` at callback time, not a focus history.
  `longest_work:null` means no qualifying completed span was retained for that
  sampling window, not that the UI did no work. Pending calls are counted at
  callback time, not throughout the interval. Every tick consumes the longest
  span, including ticks that do not emit a gap; visibility changes clear it.
- `layer=native event=dispatch_pending/dispatch_completed`: a background task
  queued a main-thread callback, observed it still pending at the next tick,
  and later measured its queue interval. At most one callback is pending.
  This distinguishes native UI-dispatch delay from frontend-only delay without
  claiming an OS/browser stack or guessing which work blocked the thread.
- On Linux, the first `web_process_terminated` line per webview records WebKit's
  coarse termination reason. This observes process loss without reloading or
  inventing a recovery policy. macOS keeps its existing termination/recovery
  lifecycle logs. Neither is a stack or an explanation of a crash's cause.

There is no upload, new diagnostic export/collection workflow, interaction
listener, payload logging, DOM scan, stack sampling or always-on animation-frame
loop. Normal one-second samples write nothing. Existing named call sites keep
only a longest-duration aggregate and pending-wait count. The frontend has one
in-flight delivery, at most 16 queued records, and at most 32 admitted records
per minute; native ingress and native dispatch each have a 32-line/minute
budget. `dropped`/`native_dropped` report suppression on the next admitted line.
The existing nonblocking tracing writer is bounded and lossy under saturation;
these limits bound diagnostic contribution, not all pre-existing log traffic
or total file bytes. Existing general logs can contain paths/IDs: do not assume
an entire log file is sanitized just because these new lines are.

These records locate delayed callbacks; they cannot reconstruct a stack or
name work that was never instrumented. A 20,062ms interval on a 1,000ms timer
is not proof of 20,062ms of continuous UI blocking. Correlate retained native
dispatch and lifecycle records without treating their absence in a selected
excerpt as evidence of an absent transition. Bind interpretation to the
client's own `event=enabled version=...` line and matching source; a connected
host's version does not identify the client build. An explicit capture must
already have been running to provide its richer evidence retrospectively.

**Limits:** scheduling gaps can also mean OS suspension, debugger pauses,
unfocused-window WebKit throttling, or uninstrumented browser work. Visibility
changes reset frontend baselines; focus is reported, not guessed. Frontend
records reach disk only when JS and the desktop bridge can execute; a permanent
WebContent hang or process exit can prevent delivery. Native sampling starts in
shell setup and requires the async runtime to run; an entire-process suspension
cannot be diagnosed while suspended. File timestamps are receipt times;
`since_wasm_ms` is a page-relative source timestamp. Missing lines are not proof
of responsiveness, and these diagnostics cannot establish the reported work
Mac's cause retrospectively. No affected-machine reproduction is claimed.

**App-build coverage:** startup, gap and native dispatch lines ship from
v0.9.6-beta.7. The `frame`, `bytes`, `items` fields and `file_tree_build`
operation ship in builds after v0.9.6-beta.8; earlier gap lines name only the
operation and duration.

## Large-project connection loss

Workbench creation/removal on a 100,352-file project over real SSH exposed a
separate connection failure: a 13,346,950-byte `project_event` exceeded the
8 MiB chat output budget even with an empty queue. Directory notifications
expanded tracked descendants, and the frontend publisher included unread files
in one `FilesChanged` event. Replacement bootstraps followed roughly 17.8-second
operation intervals; this does not establish an original UI-thread freeze.

Frontend invalidations now contain only paths whose versions the project actor
has supplied to readers or file subscribers, including subsequent code-intel
subscriptions. Internal code-intel workspace notifications, file listings and
the existing queue limits are unchanged. Unread inventory growth therefore
does not grow that frontend event.

Source history dates this inventory-expansion path to `e43a8bd1`, first shipped
in v0.9.4-beta.2. The earlier queue budget shipped in v0.8.19-beta.52. The
v0.9.4-beta.15 rescan correction did not cover directory expansion or all real
inventory changes. These are source/tag ancestry findings, not historical
binary reproductions or proof that earlier versions cannot stall.

## Existing opt-in detailed capture

The client recorder is off until explicitly started. It uses the existing
UI-debug Evaluate endpoint (or the webview console); no data is uploaded or
persisted. Start **before** reproducing a freeze:

```js
return window.__tydeDiagnostics.start();
```

After a freeze, stop and retrieve the bounded JSON export through Evaluate:

```js
window.__tydeDiagnostics.stop();
return window.__tydeDiagnostics.export();
```

`snapshot()` returns the same data as an object while retaining capture state.
`clear()` stops and discards it. Starting again discards the previous capture.
Capture survives ordinary navigation inside this window, not app/webview
restart. No new Settings controls are required. Debug MCP must already be
available; it is not automatically enabled by this recorder.

## Reading the evidence

All relative times use the page's monotonic performance clock. `startedUnixMs`
is a wall-clock anchor for comparing host tracing, **not clock synchronization**
with remote hosts. IDs are capture-local operation ordinals, not host, session,
agent, task, stream or native IDs. `parent` links nested synchronous work or a
bridge wait initiated by that work.

- `client_work`: measured inclusive synchronous execution of named call sites.
  Covered paths: global keyboard handling, composer input/autosize, host JSON
  receive/typed dispatch, file explorer tree build, markdown/synchronous
  highlighting, highlight worker submission/callback. Nested durations must not
  be added together. Host receive spans acquire their typed frame kind after
  parsing; dispatch spans identify the frame from the start. Neither identifies
  subsequent deferred reactive rendering or browser layout/paint. The following
  projection spans measure selected deferred chat and sidebar work separately:

  | Projection `operation` | `items` |
  |---|---|
  | `chat_tab_binding` | 1 tab-to-agent lookup |
  | `chat_row_projection` | projected loaded row handles, including streaming row |
  | `chat_context_scan` | rows actually visited in the reverse context-breakdown scan |
  | `chat_context_reported_scan` | rows actually visited in the ever-reported scan |
  | `chat_ink_layout` | loaded rows supplied to stretch layout |
  | `chat_virtual_window` | loaded rows supplied to viewport/spacer calculation |
  | `agents_sidebar_projection` | projected agent rows, including nested children; swarm rows are not counted |

  These spans wrap the actual existing work, including full-entry clones in
  context scans. Counts are not persisted session size, mounted DOM rows,
  exact loop iterations in the window calculation, or bytes cloned. All have
  `bytes:0` and empty detail; neither bodies nor identifiers are inspected for
  diagnostics. The reverse context-breakdown scan remains bypassed for Codex's
  typed stats. Stretch layout runs only for Flow/Notebook, not Default. Hidden
  hot-set chats can still execute projections; visibility alone does not imply
  suspension. No extra signal subscriptions or transcript scans are added.
  The sidebar span covers computing the projection, not the DOM rows built
  from it; rows are keyed, so a lifecycle frame rebuilds only changed rows.

  Compare these intervals to typed receive/dispatch records and frame gaps on
  the same monotonic timeline. Synchronous nesting retains capture-local
  `parent` ordinals. Deferred work has no invented parent frame or protocol
  request correlation; nearby events alone do not establish which caused it.
  Nested durations overlap. A large layout/window span narrows an application
  hotspot; time outside named spans remains unattributed, not a CPU sample.
- `async_wait`: bridge invocation-to-settlement interval. It includes transport,
  native/server/remote work **and** delayed delivery back to the client. It is
  not UI-thread blocking time or proof of a slow server. Pending intervals are
  retained explicitly; no fabricated protocol request/response pairing.
- `interaction`: delivery of a keydown, input or pointerdown, without keys,
  values, target names or contents. Delivery delay is null when the event clock
  cannot be compared to the page clock. Synthetic events are not physical-input
  latency measurements.
- `gaps`: foreground timer intervals >=200ms or animation-frame intervals
  >=100ms. These are scheduling observations, **not CPU profiles**. Visibility
  transitions reset the baselines so hidden-tab throttling is not called a UI
  stall. OS suspension, debugger pauses and browser scheduling can still cause
  foreground gaps. Frame gaps alone can be rendering/compositor scheduling;
  corroborating timer gaps show the client event loop was delayed.

Each gap retains its longest overlapping client spans, recent activity and
pending async work even after the general ring rolls. `instrumentedClientUnionMs`
counts the union of retained overlapping synchronous spans, avoiding nested
span double-counting. `unattributedIntervalMs` is the remaining observed
interval, **not unmeasured CPU consumption**. It can include idle time, browser
layout/paint/GC, uninstrumented application work or scheduling. Overlap is not
causation. `scope_end` means a scope returned, not successful backend work.
`incomplete` marks work still active when stopped/exported.

Browser observer delivery can follow a gap's timer/frame callback. Late numeric
browser intervals are attached to overlapping retained incidents, still capped
at 12 per incident. Stop drains queued observer entries before disconnecting;
no later callback changes the stopped export. Browser intervals are not added
to the synchronous client union and do not establish which application call
caused them. Unsupported browser observers remain explicitly unsupported.

Portable timers, animation frames, DOM events and explicit call-site timings
work in Tauri/WebKit as well as Chromium. Optional Long Tasks/Long Animation
Frames add numeric intervals only where supported; capabilities explicitly
report `unsupported` or `registration_failed` otherwise. There are no portable
JS stacks/GC/native-thread samples here; use the platform profiler for the
unattributed portion. No Chrome-only attribution is promised on macOS.

## Bounds and privacy

The recorder retains 1,024 ordinary records, 64 incidents and at most 32 active
synchronous operations and 32 active asynchronous waits (separate capacity so
pending remote work cannot hide synchronous UI spans). Each incident retains at most 12 client spans, 12 browser intervals,
12 recent records and 32 pending waits. Overflow counters are exported; missing
records are not proof that no work occurred. Export serializes only this bounded
state, on explicit request. There is no per-record console/network/disk output,
DOM traversal, stack capture, payload serialization, mutation observer or global
function patching. While detailed capture is disabled, named synchronous work,
including these selected chat projections, updates only the automatic local
longest-span aggregate described above. No per-projection history is retained
off-capture, and the existing disk admission budgets are unchanged.

Records contain only fixed operation/command names, protocol FrameKind names,
times, numeric sizes/counts, coarse outcome and visibility. No argument values,
raw errors, chat/source, paths, URLs, credentials or native identifiers enter the
recorder. Existing console `[perf]` logs are separate and **not** a safe capture
export: some contain file paths. Existing server tracing and agent timing do
not measure the browser thread. The unlanded responsiveness-audit workbench has
additional specialized debug/test harnesses; those are not prerequisites for
this recorder and are not represented as shipped capabilities.

## Long-session reproduction bounds

The mounted Chrome long-transcript flow exercises 200 generated payload rows,
window/spacer geometry, an unrelated tab and chat-row-map mutation, a hidden
hot tab, Default/Flow branches, and the Codex context-scan bypass. It checks
record presence, counts, nesting, privacy, loss and off/stopped behavior, never
elapsed-time or host-speed thresholds. The fixture appends only its three
bounded `UI_PROJECTION_CAPTURE` exports to the pinned browser runner's result
output, which the canonical stage retains even on success; ordinary successful
test console remains captured. Exports use the real browser performance clock;
timings are observations of this generated Linux/Chrome fixture, not pass/fail
requirements or CPU samples. It is UI-boundary coverage, not a real
provider or macOS reproduction.

A fresh real instance initially receives a server-selected history tail, not
an entire old session. Loaded projection counts in a capture, not session age
or persisted message count, establish which condition was actually exercised.
The disposable dev-instance API currently has no history-seed/replay input.
Do not inject frontend events, copy primary stores or synthesize transcript
files to claim a real-server long-session reproduction. Retain explicit local
capture exports with their exact client source/build and operation scope; no
upload or automatic disk sink is added. These diagnostics cannot retrospectively
explain earlier Mac timer gaps or establish that any candidate is their cause.
