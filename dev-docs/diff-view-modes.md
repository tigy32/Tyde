# Diff View Modes

Spec for two user-facing diff toggles: **layout** (unified vs side-by-side) and
**context** (hunks vs whole file). Audience: implementation agents and future
maintainers.

## 1. Goal

Add two global user preferences to the git diff view:

- **Layout**: Unified (current) or SideBySide.
- **Context**: Hunks (3 lines, current) or FullFile (whole file).

Both surfaced in the Settings panel *and* in a toolbar on the diff view itself.
Both signals drive both surfaces — toolbar and panel stay in sync because they
bind the same signal.

## 2. Preference storage

**Decision: frontend `localStorage`, per-reader.** Not `HostSettings`.

- Key `tyde-diff-view-mode` → `DiffViewMode`.
- Key `tyde-diff-context-mode` → `DiffContextMode`.

Rationale: a reader's layout preference travels with the reader, not the host.
Matches existing precedent for `theme`, `font-size`, `font-family`,
`tabs_enabled` in `frontend/src/components/settings_panel.rs:26–28,104–159`.
Rejected `HostSettings` because: prefs are not host-owned, would cost
persistence/validation/fanout/replay for no gain, and request-time parameters
(context mode on `ProjectReadDiff`) are not settings.

## 3. Protocol (`protocol/src/types.rs`)

Source of truth. Strong types, no defaults, no fallbacks.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffContextMode {
    Hunks,
    FullFile,
}

pub struct ProjectReadDiffPayload {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
    pub context_mode: DiffContextMode, // NEW, required
}

pub struct ProjectGitDiffPayload {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
    pub context_mode: DiffContextMode, // NEW, echoed — detect stale responses
    pub files: Vec<ProjectGitDiffFile>,
}

pub struct ProjectGitDiffFile {
    pub relative_path: String,
    pub hunks: Vec<ProjectGitDiffHunk>,
}

pub struct ProjectGitDiffHunk {
    pub hunk_id: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<ProjectGitDiffLine>,
    // `header: String` is REMOVED. UIs reconstruct "@@" labels from typed fields.
}

pub struct ProjectGitDiffLine {
    pub kind: ProjectGitDiffLineKind,      // Context | Added | Removed (unchanged)
    pub text: String,                      // prefix-free: "foo", not "+foo"
    pub old_line_number: Option<u32>,      // None for Added
    pub new_line_number: Option<u32>,      // None for Removed
}
```

`DiffViewMode` is **not** in the protocol — it is pure presentation.

## 4. Frontend-only types (`frontend/src/state.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffViewMode { Unified, SideBySide }
```

New signals on `AppState`:

```rust
pub diff_view_mode: RwSignal<DiffViewMode>,
pub diff_context_mode: RwSignal<DiffContextMode>, // protocol type, UI-owned value
```

Initialized from localStorage (strong-typed defaults `Unified` / `Hunks` if key
missing; write back immediately). Persist-on-change via the same helper used
for theme.

`DiffViewState` gains `path: Option<String>` and `context_mode: DiffContextMode`
so stale responses (context mode changed mid-flight) are detectable and
discardable.

## 5. Server behavior

`server/src/project_stream.rs:read_diff()` accepts `DiffContextMode`:

- `Hunks` → `git diff -U3` (current behavior).
- `FullFile` → `git diff -U9999999` (standard git idiom for whole-file
  context; produces one hunk spanning the file).

`parse_git_diff()` (`project_stream.rs:874`) is extended to emit:

- `old_start`, `old_count`, `new_start`, `new_count` per hunk (already parsed
  from `@@` headers — propagate instead of re-stringifying).
- `old_line_number` / `new_line_number` per line: walk hunk lines,
  incrementing the appropriate counter based on `kind`.
- `text` with the leading `+`/`-`/` ` prefix stripped.

`server/src/host.rs:2091 read_project_diff` threads `context_mode` through and
echoes it on the emitted payload.

`stage_hunk` is unchanged. `hunk_id` semantics preserved. In `FullFile` mode a
file has one hunk, so per-hunk staging is equivalent to stage-file; the
frontend hides the per-hunk button in FullFile render.

## 6. Frontend behavior (`frontend/src/components/diff_view.rs`)

Delete the header-parsing block at lines 69–90. Render directly from typed
line numbers. Split the renderer:

- `UnifiedHunk` — single column; gutters show `old | new` line numbers.
- `SideBySideHunk` — pure function `Vec<ProjectGitDiffLine>` →
  `Vec<SideBySideRow>`. Pair consecutive Removed/Added runs by position;
  unpaired runs render as empty on the opposite side; Context spans both
  columns.

`DiffView` selects via `move || match diff_view_mode.get() { ... }` — reactive,
no snapshot. Switching view mode does **not** re-request; same data re-lays out.

A reactive effect keyed on `(root, scope, path, diff_context_mode)`
re-dispatches `ProjectReadDiff` when context mode changes. On response,
`dispatch.rs:646` stores the echoed `context_mode` on `DiffViewState`; renders
guard against a mismatch with the current signal.

Two toggle surfaces, both bound to the same signals:

- Settings panel: new "Diff" section in `settings_panel.rs` with two segmented
  controls.
- Diff view toolbar: two toggle buttons inside the diff view.

## 7. Coordination with untracked-files fix

The typed-rows shape accommodates untracked files with no special casing. An
untracked file emits as one `ProjectGitDiffFile` with a single hunk:

- `old_start = 0`, `old_count = 0`, `new_start = 1`, `new_count = <file length>`.
- Every line: `kind = Added`, `old_line_number = None`,
  `new_line_number = Some(n)`, `text` = the file line.

Both renderers handle this uniformly. The two efforts share a target shape; if
typed-rows lands first, the untracked fix slots in. If they land together, one
atomic PR.

## 8. Migration

One atomic change across protocol, server, and frontend. No dual wire shapes,
no `#[serde(default)]` back-compat, no fallback parsing. The old
`header: String` field is deleted outright; any consumer that wanted it
reconstructs from the typed range fields. No deployed remote hosts, so no
wire-compat story required.

## 9. Tests

**Protocol** (`protocol/tests/`): serde round-trip for new types and envelopes.

**Server** (`server/src/project_stream.rs` unit tests + `tests/tests/projects.rs`):

- `Hunks`: multi-hunk fixture — correct `old_start/old_count/new_start/new_count`
  and monotonic per-line numbers.
- `FullFile`: one hunk spanning the file.
- Added / Removed / Renamed files: correct `Option<u32>` line numbers.
- Untracked file (with its fix): one all-Added hunk in the new shape.

**Frontend**:

- Side-by-side pairing function: only-removed, only-added, equal-run replace,
  unequal replace, interleaved context.
- LocalStorage round-trip for both prefs.
- Stale-response guard: response whose `context_mode` doesn't match current
  signal is ignored.

## 10. Deferred / open

- "Show more context" (N-line expander). Ship binary Hunks/FullFile first.
  Future variant: `DiffContextMode::Expanded { lines: u32 }`.
- FullFile size cap on very large files. Acceptable at current scale; add a
  server-side cap only if profiling demands it.
- Binary files in SideBySide. Existing binary short-circuit continues to apply
  in both layouts.
- Rename/copy detection. Out of scope; current parser behavior preserved.

## 11. File change checklist

- `protocol/src/types.rs` — `DiffContextMode`; `context_mode` on request and
  response; typed hunk ranges + per-line numbers; prefix-free `text`; drop
  `header`.
- `server/src/project_stream.rs` — accept `DiffContextMode`, pass `-U<n>`,
  enrich parser output.
- `server/src/host.rs:2091` — thread `context_mode`, echo in emitted payload.
- `frontend/src/state.rs` — `DiffViewMode` enum; `diff_view_mode` and
  `diff_context_mode` signals; `DiffViewState { path, context_mode }`.
- `frontend/src/dispatch.rs:646` — read new payload fields; stale-response
  guard.
- `frontend/src/components/diff_view.rs` — delete header parsing; split into
  `UnifiedHunk` / `SideBySideHunk`; reactive re-request on context-mode
  change; hide per-hunk stage in FullFile.
- `frontend/src/components/settings_panel.rs` — new "Diff" section with two
  segmented controls.
- Diff-view toolbar (new or extended) — two toggle buttons bound to the same
  signals.
