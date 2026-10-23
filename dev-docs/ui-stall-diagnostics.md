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
  **not** blocking time. No operation details or arguments are recorded.
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
  receive/typed dispatch, markdown/synchronous highlighting, highlight worker
  submission/callback. Nested durations must not be added together.
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
function patching. While detailed capture is disabled, call sites still update the automatic
local aggregate described above.

Records contain only fixed operation/command names, protocol FrameKind names,
times, numeric sizes/counts, coarse outcome and visibility. No argument values,
raw errors, chat/source, paths, URLs, credentials or native identifiers enter the
recorder. Existing console `[perf]` logs are separate and **not** a safe capture
export: some contain file paths. Existing server tracing and agent timing do
not measure the browser thread. The unlanded responsiveness-audit workbench has
additional specialized debug/test harnesses; those are not prerequisites for
this recorder and are not represented as shipped capabilities.
