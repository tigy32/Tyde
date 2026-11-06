# Pinned render-effect observation

Registry source: reactive_graph 0.2.14, upstream commit
fbc9ed631ff8c19c5eed02a1d4459e202cff7043 (see .cargo_vcs_info.json).
Registry package SHA-256:
00c5a025366836190c7030e883cc2bcd9e384ff555336e3c7954741ca411b177. Upstream code
is MIT licensed; tests and registry-only metadata are not needed for this patch.

The optional tyde-diagnostics feature adds a thread-local content-free observer
and an immediate registration scope. Only new_with_value_erased is observed:
initial callback, receiver ready/closed, paused branch, dependency check result,
and conditional callback. It does not observe async render effects, ordinary
Effect loops, memos individually, other executors, browser work or CPU time.
The frontend writes only to its existing opt-in bounded memory recorder.
No graph ownership, scheduling, synchronization or cancellation changes.
Keep these diagnostic hooks until the user signs off.
