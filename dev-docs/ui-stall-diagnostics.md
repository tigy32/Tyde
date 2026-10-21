# Local UI stall capture

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
function patching. While disabled, call sites only check the recorder.

Records contain only fixed operation/command names, protocol FrameKind names,
times, numeric sizes/counts, coarse outcome and visibility. No argument values,
raw errors, chat/source, paths, URLs, credentials or native identifiers enter the
recorder. Existing console `[perf]` logs are separate and **not** a safe capture
export: some contain file paths. Existing server tracing and agent timing do
not measure the browser thread. The unlanded responsiveness-audit workbench has
additional specialized debug/test harnesses; those are not prerequisites for
this recorder and are not represented as shipped capabilities.
