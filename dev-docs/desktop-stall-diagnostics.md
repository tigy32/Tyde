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
