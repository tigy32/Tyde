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
