# Local desktop stall diagnostics

This instrumentation diagnoses ownership; it does not fix a stall or establish
customer root cause. Retain the hooks and captured evidence until user signoff.

## Existing native logs

The maintained standalone exporter is `tools/export-desktop-stalls.py`:

```sh
python3 tools/export-desktop-stalls.py
```

It reads only the documented `~/.tyde/tracing/tyde-shell.log.YYYY-MM-DD-HH`
files and creates a private JSONL file on Desktop. It never uploads anything,
starts the app, inspects processes or reads provider/session stores. Inspect the
first export row: invalid-block counters and `truncated_or_sampled` describe
coverage loss. Missing fields mean unknown, not observed zero.

The CLI/filesystem harness is the existing V3 artifact harness, now maintained
in `tools/test_desktop_stalls_export.py` and run by `./dev.sh check`. Synthetic
format fixtures test export fidelity and privacy, not application performance.
It checks synchronous admission against `host-config/src/diagnostics.rs` and
exercises longest-work admission, malformed/unknown/duplicate whole-block
rejection, strict u32 metrics, private output mode and existing file/byte quotas.
Historical V1–V3 evidence scripts remain immutable, not parallel maintained tools.

## Opt-in lifecycle capture

In the existing Debug evaluation surface, call
`window.__tydeDiagnostics.start()` before the lifecycle registration of interest,
perform the owned flow, then stop on its observed settled event with
`window.__tydeDiagnostics.stop()` and inspect/export the snapshot. No automatic
upload, default-on per-call log stream or new UI is introduced.

Detailed schema 2 adds a bounded 4096-event lifecycle ring to the existing
recorder. Future ordinals are capture-local; poll ordinals belong to that future.
Registration records contain only static producer/site labels. Future identity
is assigned at immediate `UiExecutor::spawn_local`, not reconstructed from
history or inherited from a parent. The four existing TaskOrigin scopes are
unchanged. Existing operation spans carry the observed future/poll context.

The pinned `reactive_graph` 0.2.14 patch reports actual synchronous render-effect
initial callbacks, receiver results, paused branches, dependency-check entry and
result, and conditional callback entry/exit. Poll ready/pending and drop/retired
are separate observations. A poll is not evidence that a callback executed.
Initial callbacks may occur outside any registered future; null future/poll is
explicitly uncorrelated. Futures registered before capture are not retroactively
identified; `coverage.unregisteredEvents` counts those and other uncorrelated
events. Ring overwrites are explicit in `loss.lifecycleOverwritten`.

Coverage is limited to this executor and `new_with_value_erased`, not asynchronous
render effects, ordinary Effect internals, every memo, browser internals or native
work. Recorded times are monotonic relative wall times. Nested/overlapping
intervals are inclusive, never CPU or exclusive self-time. A quiet local flow
only means not reproduced there. Missing hooks or overwritten registration/poll
boundaries limit attribution. FileTreeBuild still excludes keyed child/DOM
construction; loaded counts are not mounted counts.

Trace data contains ordinals, static enums/operation labels, flags, relative
times and bounded counts only: no chat/source/model text, paths, native/host/
agent/session IDs, credentials, argv or environment. Existing detailed operation
records retain their pre-existing timing fields. This change adds no durable
wire record or separate lifecycle state model.

## Native ownership and generic future boundaries

The installed Debug capture API is asynchronous: **await** `start()`, `stop()`,
`snapshot()` and `export()`. Starting first enables the native recorder, then the
frontend recorder; stopping ends frontend collection, then native collection.
Their monotonic origins are separate, not synchronized clocks. Native samples
are returned under `snapshot.native`; they are not written to the default log.
The existing-log exporter cannot reconstruct these observations retroactively.

Native collection keeps 64 observations, with an overwrite counter. The bounded
16-request channel waits for admission (no silently dropped sample requests).
It observes at a pending main-dispatch probe and explicit snapshot/start/stop,
not every callback. Each observation queries the OS main-thread state off the
UI thread and brackets that query with the synchronous callback scope sequence.
Query start/end bracket the OS read, including blocking-worker scheduling; the
OS state was observed somewhere inside that interval, not necessarily at export.
If the sequence changes, ownership is unknown, never assigned by overlap.

The static native callback enum maps directly to the invoke dispatcher,
page/window/menu/run event callbacks, main-dispatch probe, and Linux clipboard
request/targets/image callbacks. Scopes nest and restore their actual caller;
off-main invocations do not acquire main-thread ownership. No guard crosses an
await. `gated_dev_fixture` is a debug-only, owned-DEV boundary exercise: it holds
an actual main-dispatch callback on an explicit channel gate, observes from the
recorder task, then releases and joins it. It does not simulate a customer cause.

Linux reads only the state field from its own main thread's procfs status;
macOS uses public `pthread_mach_thread_np` / `thread_info(THREAD_BASIC_INFO)` on
its own thread. Handles, IDs, names and all other returned fields are discarded.
Unsupported platforms and query failures are explicit. Running/runnable is not
CPU time or proof of continuous execution. Waiting is an instantaneous OS state,
not a wait-stack, App Nap diagnosis, or proof of which resource is awaited.
An unchanged non-null callback identifies the product scope containing the OS
observation; nested framework work can still be inside it. A null callback is
**unknown**, not proof that framework/OS code caused the delay. Uninstrumented
callbacks, native stacks and exclusive leaf work remain outside coverage.
Linux evidence does not establish macOS acceptance.

Registration and first-poll lifecycle rows additionally retain at most 12 numeric
wasm frames from the immediate boundary stack: function indices and module
byte offsets when the engine supplies them. Raw stacks, URLs and symbol strings
are never retained. Unsupported stack formats and observed-frame slice omissions are explicit.
Engine-side stack truncation is always unknown (including Chrome's default
stack depth); zero slice omissions never promises a complete caller stack. Later polls correlate by future ordinal, not an inherited/guessed
producer. These are call sites at the registration/first-poll boundary, **not**
a sample of a future's executing leaf or its wake source. Other executors and
futures registered before capture remain outside this coverage.

Retain the exact DEV `frontendBuild` attestation and matching wasm artifact
alongside a capture. Function indices/offsets belong only to that module; use
its name/debug sections for offline mapping to compiler symbols/module-relative
source. Do not map against a merely similar build. If the engine supplies only
an index or the artifact lacks line tables, report that symbol/line coverage
limit instead of inventing a source location. This is bounded opt-in boundary
observation, not a sampling profiler, stall repair or customer RCA.

### Installed desktop collection

In the ordinary installed desktop client, open **Settings → Debug → Desktop stall
capture**, click **Start capture** before the operation, then **Stop capture**
after it settles and **Copy capture**. Paste the resulting JSON into a local
file for inspection/sharing. This uses the existing recorder and native
clipboard plugin; no DEV launcher, console, SSH or reproduction request is
required. Closing Settings does not stop collection. Copy is disabled until a
capture has stopped. Failures appear in the same section, not as success.
Nothing is automatically uploaded. The copied capture includes the packaged
wasm preload's sanitized module filename and SHA-384 integrity hash, so an
installed build can be matched to its exact artifact without a DEV attestation.
The existing log exporter remains a distinct route for old aggregate logs;
it cannot produce this opt-in capture. A permanently frozen UI cannot stop or
copy until responsive again; this does not promise crash-proof persistence.

Validation scope for this extension: owned Linux DEV exercises the real native
callback gate and rendered recorder controls/clipboard route. It does not attest
a shipped installer, the old beta29 client, macOS main-thread observation, or a
customer incident. Mac acceptance requires an authorized, device-scoped MCP run;
no automated Mac tests, global inputs or additional OS privileges are required
or authorized by these hooks. No root-cause repair belongs in this change.

The recorder emits a state-change event for API/UI start, stop and clear.
Mounted Debug controls project that authority, including changes initiated
through DEV evaluation; they never decide to reset a capture from stale UI state.
The desktop capture always observes the current client, not the selected host.

### On-demand desktop memory owners (DEV)

The existing loopback UI debug endpoint accepts the typed `UiDebugRequest`
`{"kind":"memory_owners"}` and returns `UiDebugResponse::MemoryOwners`.
Use the `uiDebugAddr` from an owned, ready dev-instance attestation: send one
JSON request over TCP, shut down the socket's write side, then read the JSON
response to EOF. This is not a new MCP tool or a production collection loop.
It reads the current AppState synchronously, untracked and without cloning
payloads; it retains no snapshots, starts no recorder, and emits no content,
identifiers, paths, command arguments or credentials.

The snapshot separates transcript rows/attached results, live streaming buffers,
the completion-result index, and active versus saved-project file/diff bodies.
Index, question and progress counts are reported independently. Shared live
streaming signals are visited through `streaming_text`, not again through rows.
`len_bytes` is the current named buffer length; `capacity_bytes` is its actual
Rust String/Vec backing capacity, **not** a serialized-size estimate or the
capacity of Wasm linear memory. Row-vector slot capacity is a separate field.

This is deliberately not a total-heap measurement. It excludes IDs, paths,
request arguments, message/tool metadata, notice/compaction bodies, collection
and signal overhead, renderer/worker caches, detached reactive owners, JS,
native/compositor allocations and allocator overhead/free arenas. Result-body
measurement visits every result variant, including failure/cancellation text,
JSON strings/keys, native text and image data; typed IDs/paths are excluded.
Reasoning text includes its signature; reasoning blobs are counted separately.
Counts can establish named-owner lifetimes; they cannot prove that all memory
was released or explain a process footprint.

#### Fixed-workload incident observation, 2026-11-06

An owned Linux Tauri/WebKit desktop exercised the existing event bridge,
production dispatcher/AppState and rendered Leptos chat. The above-backend
fixture supplied 8 messages (32 KiB text) and 8 command results (16 KiB stdout),
without executing tools or starting a provider/backend actor. Synchronization
used typed completions and DOM predicates with MutationObserver, not sleeps,
GC requests, host-speed thresholds or timeout increases.

| Settled boundary | Message rows / attached results / indexed results | Message / attached-result / indexed-result backing capacity |
| --- | --- | --- |
| Baseline | 0 / 0 / 0 | 0 / 0 / 0 |
| Append | 8 / 8 / 8 | 32 / 16 / 16 KiB |
| Each of 8 same-size authoritative replacements | 8 / 8 / 8 | 32 / 16 / 16 KiB |
| Each of 3 tab close/reopens, without replay on reopen | 8 / 8 / 8 | 32 / 16 / 16 KiB |
| AgentClosed projection | 0 / 0 / 0 | 0 / 0 / 0 |

Every post-replacement and tab-boundary snapshot equaled the append snapshot,
including 768 bytes of row slots and 8 entries in each transcript index. The
final snapshot equaled baseline across every measured owner. Reopened chat
rendered all 8 tool cards and preserved the messages. Streaming, question,
progress and active/saved file/diff owners were empty in this workload; their
nonempty paths were not exercised. Evidence and executable workload are retained
locally under `~/tyde-evidence/desktop-memory-growth-20261106/owned-linux03/`,
with scalar-only owner snapshots in `BOUNDARY-RESULT.safe.json`.

This observes the additional 16 KiB owned by `completed_tool_results`; it does
not establish that the index is defective. No accumulating named-owner leak was
demonstrated by these fixed-state boundaries. Cached-chat retention is preserved.
The native document was hidden: this is real Linux WebKit execution, not
foreground rendering/compositor or macOS WK validation. There is no current
Mac allocation profile, customer workload/version match, total allocator-live
measurement, or retained-root proof. The reported 57.14 GB incident remains
unattributed; this change is observability, not a causal repair.

The memory-owner request requires the frontend's typed ready state and then
awaits completion/channel failure without an elapsed deadline. Existing debug
Evaluate/Ping/screenshot deadlines are unchanged. Pending debug replies are
owned by a channel-driven task, not an async mutex; this is diagnostic bridge
plumbing only, not a change to host/server lifecycle or application retention.

The final owned build also completed concurrent typed snapshots and pings and
rejected an unregistered response without echoing its request identity. A
proposed held-submission probe could not install: Tauri exposes `invoke` as a
read-only getter. That apparatus failure is preserved; it is not a product
failure, a held-request pass, or a reason to weaken the boundary. Both owned
instances were canonically stopped; their ephemeral stores and application
directories were confirmed removed. No installed app was restarted.

#### Ordinary installed-client memory collection

The ordinary **Settings → Debug → Desktop stall capture → Start capture →
Stop capture → Copy capture** route now exports `memoryOwners` and
`memoryOwnersObservedUnixMs` alongside the existing bounded stall capture and
packaged build identity. Paste the clipboard JSON into a local evidence file.
No DEV instance, debug TCP endpoint, console, host selection or backend turn is
required. This source change must reach a shipped build before it is available
in an already-installed client; it does not update or restart that client.

The owner snapshot is taken **at each Copy/export**, not at Start/Stop and not
periodically. Use separate exported files for before/after boundaries. The
observation timestamp distinguishes current owners from the earlier stopped
stall interval; it is not a memory-growth rate or allocation history. The same
Rust reader serves both installed export and typed DEV requests, without a
payload cache. Its callback lives with App and is unregistered during App
cleanup. A missing reader fails export explicitly rather than copying a
capture that silently lacks memory observations. No new controls or appearance
changes are introduced. A fully paused/frozen client still cannot execute this
reader until responsive; this is not crash-proof collection or a substitute for
a current macOS allocation profile. The customer incident remains OPEN.

Installed-route boundary evidence: the pre-integration owned build exported no
`memoryOwners` (RED). The integrated build's rendered Debug controls wrote real
native clipboard JSON that matched the typed owner snapshot after the 8-message /
8-result workload and again after AgentClosed (GREEN). Capture was stopped
before append, proving Copy reads current owners rather than a stopped snapshot.
The first GREEN apparatus attempt had a malformed selector expression and
failed before collection; that attempt is retained, not reported as a product
failure or erased by the corrected probe. This remains hidden Linux WebKit
acceptance, not a shipped installer or macOS acceptance. Receipts and copied
captures are in `~/tyde-evidence/desktop-memory-growth-20261106/installed-route-green02/`;
the predecessor RED is in `installed-route-red/` beside it.
