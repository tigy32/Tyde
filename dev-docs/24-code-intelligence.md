# Code Intelligence

This document specifies the **code-intelligence service**: the server-owned
subsystem that gives the Tyde editor go-to-definition, hover, diagnostics, and
find-references for source files inside a project.

It is **generic and language-agnostic**. rust-analyzer is the first provider,
not the design center. Adding a second language must cost one server module,
one match arm, and an extension mapping — with **zero** protocol or frontend
change.

It builds on:

- `01-philosophy.md` for the architectural constraints it must obey
- `02-protocol.md` for envelope, stream, and error rules
- `07-project-stream.md` for the stream this rides on, the file model, and the
  versioning prerequisite

---

## 1. Goals

We want the editor to answer "what is this symbol and where is it defined?"
without the frontend ever speaking a word of LSP, knowing what a language
server is, or distinguishing local from remote.

Concretely the service must support:

- **command-click / F12 navigation** that resolves as a *local* lookup against
  data the server already pushed — not a round-trip per click
- **hover** popovers with type/doc information
- **inline diagnostics** (squiggles + gutter markers) pushed unsolicited
- **find-references** streamed on demand, like project search

This is server-owned behavior. The frontend renders pushed semantic state. It
does not run a language server, parse LSP, convert encodings, or model what
"definition" means.

---

## 2. Design Shape (the locked decisions)

These were converged on independently by two backends, reconciled, and the
product calls were made by the user. They are the spec, not options.

### 2.1 Push model, whole-file scope, delivered incrementally

On file open / subscribe, the server **pushes a resolved semantic model** for
the **whole file** so command-click is a local lookup, not a request. Whole
file — *not* viewport-gated — because partial semantic data produces dead
clickable identifiers, which violates "fail visibly."

But "whole file at once" is not "blocking until everything resolves." The model
arrives **asynchronously and incrementally**, mirroring the syntax-highlight
worker:

1. **Immediately**: push the occurrence ranges (the clickable identifier set)
   derived from a single `textDocument/semanticTokens` call, and — separately —
   the latest diagnostics. `semanticTokens` provides *ranges only*; it does not
   carry diagnostics. Diagnostics come from the most recent
   `textDocument/publishDiagnostics` snapshot (a full-file replace, §4.2), which
   the provider pushes unsolicited and the service forwards.
2. **Then stream** resolved definition targets as they complete. There is no
   LSP batch API for this — it is one `textDocument/definition` per position —
   so targets trickle in and the **frontend merges them into the model by
   range**.
3. **Before a symbol's target has arrived**, a click falls back to a single
   on-demand `CodeIntelNavigate` for that one position.

The viewport (`CodeIntelSetVisibleRange`) is a **prioritization hint only**. It
reorders which positions get resolved first. It is never a gate on what is
clickable.

### 2.2 Rides the existing project stream

Code intelligence is part of a project, so it uses the existing
`/project/<project_id>` stream from `07-project-stream.md`. **No new stream.**

New frames are `CodeIntel*` `FrameKind` variants. They are **not** `Lsp*` —
the wire protocol is about Tyde's code-intelligence model, not about LSP, which
is an implementation detail of one provider. Adding these variants bumps
`PROTOCOL_VERSION` from its current value `13` to `14`
(`protocol/src/types.rs:10`).

**Positions on the wire are byte offsets.** This matches
`ProjectSearchMatch.ranges` (byte offsets, `protocol/src/types.rs:2607`) and the
`FileLines` byte model in `frontend/src/line_source.rs`. LSP speaks UTF-16 code
units; that conversion is confined **entirely inside the rust-analyzer
provider**. The frontend never sees a UTF-16 offset.

Recommended representation: **absolute file byte offsets** with **half-open
ranges** `[start, end)`. Render-time line/column is derived with `FileLines`
byte↔line helpers in `frontend/src/line_source.rs` — but those helpers **do not
exist yet**: `FileLines` currently exposes only `new`, `len`, and `line`. Adding
the byte↔line (and line↔byte) conversion helpers is a **prerequisite task**
(M0/M3), not something already present.

### 2.3 Provider abstraction is an actor, per project root

The authoritative `CodeIntelService` **actor** is **per project root**,
mirroring `ProjectStreamHandle` (`server/src/project_stream.rs:88`): an `mpsc`
command channel, one tokio task, owned state. **Not** `Arc<Mutex<T>>` — see
"Actors Over Locks." One rust-analyzer instance per project root; it handles
multi-crate Cargo workspaces *within* that root itself. Each provider is its
**own actor owning its subprocess**, owned by the root's service actor.

A project can have several roots, so at the project level there is only a
**thin router** — it does not own provider state or do code-intel work. It
maps each `CodeIntel*` frame to the right root (via the frame's `ProjectPath`)
and **delegates to that root's `CodeIntelService` actor**, then forwards that
actor's output frames back onto `/project/<project_id>`. The per-project layer
is plumbing; the per-root `CodeIntelService` is the service.

Provider selection is **server-side only**: a `match` on an internal `Language`
enum (so the compiler forces us to handle a new language) combined with a
`supports_path` check. This `Language` enum **never appears on the wire** —
putting a closed enum on the wire would mean a protocol bump and frontend
codegen for every new language, contradicting the goal. Instead the wire uses
**open string newtypes** (`CodeIntelLanguageId(String)`,
`CodeIntelProviderId(String)`, §4.2) that the frontend renders as opaque labels.

Adding a language is therefore:

- one new server module under `server/src/code_intel/`
- one new `match` arm on the internal `Language`
- one extension → `Language` mapping entry

…and nothing else. **No protocol change, no frontend codegen change** — the
string identifiers absorb the new value with no new variant on the wire.

New modules:

```text
server/src/code_intel/mod.rs           // public surface, Language enum, selection
server/src/code_intel/service.rs       // CodeIntelService actor (per project)
server/src/code_intel/provider.rs      // CodeIntelProvider trait + provider actor contract
server/src/code_intel/rust_analyzer.rs // first provider: spawns + drives RA
server/src/code_intel/lsp_codec.rs     // Content-Length framing codec
```

Reuse:

- `server/src/process_env.rs` for binary discovery and login-shell `PATH`
  (including `~/.cargo/bin`) — `find_executable_in_path` is at
  `server/src/process_env.rs:26`.
- `server/src/backend/subprocess.rs` for process-group spawn and lifecycle.

Do **not** reuse the subprocess NDJSON reader: LSP is not newline-delimited. It
needs a new **`Content-Length`** codec, which is why `lsp_codec.rs` exists.

### 2.4 File versioning prerequisite

There is **no file version on the wire today.** As of this writing
`ProjectFileContentsPayload` (`protocol/src/types.rs:2559`) carries only `path`,
`contents`, and `is_binary` — no version field — and the `ProjectFileVersion`
newtype does not exist. Code intelligence makes versioning load-bearing, so M0
must **introduce** it:

- add a `ProjectFileVersion(u64)` newtype (typed, not bare `u64`)
- add a `version: ProjectFileVersion` field to `ProjectFileContentsPayload`
- the **project-stream actor owns the single counter** per file: file read,
  notify-watcher change, and agent-write all bump the **same** counter

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectFileVersion(pub u64);
```

Every `CodeIntel*` frame carries the `ProjectFileVersion` of the contents it
describes. The client applies a `CodeIntel*` frame **only when its version
equals the version of the file contents currently rendered.** A frame with a
*newer* version is **stashed** until the matching `ProjectFileContents` arrives
(then applied); a frame with an *older* version is **dropped**. This is what
prevents stale-decoration races in both directions: a definition map computed
against version 4 must never paint over text the user is viewing at version 5,
and a freshly-arrived version-6 map must not paint over version-5 text before
the version-6 contents land.

### 2.5 Editor is read-only for now

Sync is via the file watcher + the version counter (§2.4): an external change
bumps the version, the service re-resolves, and re-pushes. There is no
`didChange` editing path yet.

The version model deliberately leaves the door open for **inline editing
later**, via a server-owned document actor that owns buffer state and drives the
LSP `didChange` path. That is future work (§9). Do not build it now.

### 2.6 rust-analyzer bootstrap: detect + hint only

No bundled binary, no managed download in v1. Discovery order:

1. `find_executable_in_path("rust-analyzer")`
2. else `rustup which --toolchain stable rust-analyzer`
3. else emit `CodeIntelStatus` with `state: Unavailable` and the hint
   `rustup component add rust-analyzer`

If a language ever needs an install confirmation, it must use the native
`confirm_dialog` helper in `frontend/src/bridge.rs` — **never**
`window.confirm` / `window.alert` (root `CLAUDE.md`; they are silently no-op'd
in the Tauri WKWebView). Managed download is a deferred future option (§9), not
v1.

---

## 3. Status Model

Cold start must read as "indexing," never as a faked empty result. Status is
**typed and scoped**.

The scope is a **tagged enum that carries identity** — a bare `Project` /
`Provider` / `File` discriminant would tell the UI *something* changed but not
*which* provider or file, so it must carry the relevant identifiers.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodeIntelStatusScope {
    Project,
    Provider { root: ProjectRootPath },
    File { path: ProjectPath, version: ProjectFileVersion },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelState {
    Unsupported,   // no provider matches this language
    Unavailable,   // provider exists but the backing binary is absent
    Starting,
    Indexing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelResourceMode {
    Full,
    Limited,
    Unavailable,
}

pub struct CodeIntelStatusPayload {
    pub scope: CodeIntelStatusScope,
    pub state: CodeIntelState,
    pub resource_mode: CodeIntelResourceMode,
    /// Present while indexing; mapped from RA `$/progress`.
    pub work_done: Option<u32>,
    pub total_work: Option<u32>,
    /// Human-readable hint, e.g. "rustup component add rust-analyzer".
    pub message: Option<String>,
}
```

`work_done` / `total_work` are mapped from rust-analyzer's `$/progress`
notifications. `resource_mode` reflects host capability — the **only** host
variable in this design (local == remote otherwise). While the provider is
`Indexing`, navigation falls back to on-demand and the UI shows progress; it
does not pretend there are zero symbols.

---

## 4. Wire Protocol

All frames ride `/project/<project_id>`. New `FrameKind` variants, snake_case on
the wire.

### 4.1 Input events (client → server)

| Frame | Purpose |
|-------|---------|
| `code_intel_subscribe_file` | start pushing the semantic model for one file |
| `code_intel_unsubscribe_file` | stop pushing for one file |
| `code_intel_set_visible_range` | reprioritize resolution (hint, not a gate) |
| `code_intel_hover` | on-demand hover at a position (domain id) |
| `code_intel_navigate` | miss-fill: resolve one definition not yet pushed (domain id) |
| `code_intel_find_references` | start a streamed references query (domain id) |
| `code_intel_cancel_references` | cancel / supersede a references query |

```rust
pub struct CodeIntelSubscribeFilePayload {
    pub path: ProjectPath,
}

pub struct CodeIntelUnsubscribeFilePayload {
    pub path: ProjectPath,
}

/// Pure prioritization hint. Never gates which identifiers are clickable.
pub struct CodeIntelSetVisibleRangePayload {
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    pub range: ByteRange, // [start, end)
}

/// On-demand hover. `hover_id` is a client-chosen domain id (cf. `search_id`),
/// not a generic request id — it correlates the streamed result.
pub struct CodeIntelHoverPayload {
    pub hover_id: u64,
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    pub offset: u32, // byte offset into the file
}

/// Miss-fill for a click whose target has not been pushed yet.
pub struct CodeIntelNavigatePayload {
    pub navigate_id: u64,
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    pub offset: u32,
}
```

`ByteRange` is the shared half-open range newtype:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u32, // inclusive, byte offset
    pub end: u32,   // exclusive, byte offset
}
```

### 4.2 Output events (server → client)

| Frame | Purpose |
|-------|---------|
| `code_intel_status` | typed scoped status (§3) |
| `code_intel_file_model` | the pushed semantic model (occurrences + targets) |
| `code_intel_navigate_result` | answer to a miss-fill `code_intel_navigate` |
| `code_intel_diagnostics` | full-file replace snapshot of diagnostics |
| `code_intel_hover_result` | answer to a `code_intel_hover` |
| `code_intel_references_results` | one file's references (streamed) |
| `code_intel_references_complete` | terminal references frame |
| `code_intel_error` | typed code-intel failure |

```rust
/// Open string identifiers — NOT closed enums. Adding pyright/gopls adds no
/// protocol variant and no frontend codegen. The frontend treats these as
/// opaque display labels; the closed `Language` enum lives server-side only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CodeIntelLanguageId(pub String); // e.g. "rust", "python", "go"

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CodeIntelProviderId(pub String); // e.g. "rust-analyzer", "pyright"

/// Progressive coverage of the file, NOT a permanent range gate. `ByteRange`
/// with `completeness: Partial` is a transient chunk streamed on the way to an
/// eventual `FullFile` + `Complete` model. Resource limits change the *pace*
/// these chunks arrive, never the final whole-file scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodeIntelModelRange {
    FullFile,
    ByteRange { range: ByteRange },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelCompleteness {
    Complete, // whole file resolved: every occurrence has its target(s)
    Partial,  // more occurrences/targets still streaming toward Complete
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelRole {
    Definition,
    Reference,
    // future: Write, ModuleRef, MacroRef, …
}

pub struct CodeIntelLocation {
    pub path: ProjectPath,
    pub range: ByteRange,
}

pub struct CodeIntelOccurrence {
    pub range: ByteRange,          // the clickable identifier span
    pub role: CodeIntelRole,
    pub display: String,           // short label for tooltip/affordance
    /// Empty until targets stream in; the client merges by range. LSP
    /// `textDocument/definition` can return multiple locations, so this is a
    /// list, not a single target.
    pub definition: Vec<CodeIntelLocation>,
}

pub struct CodeIntelFileModelPayload {
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    pub provider: CodeIntelProviderId,
    pub language: CodeIntelLanguageId,
    pub model_range: CodeIntelModelRange,
    pub completeness: CodeIntelCompleteness,
    pub occurrences: Vec<CodeIntelOccurrence>,
}
```

The first `code_intel_file_model` typically arrives `Partial` with every
`definition` empty (occurrence set from `semanticTokens`). Subsequent
`code_intel_file_model` frames at the same version carry filled-in `definition`
targets; the frontend **merges by `range`**. When the whole file is resolved,
the server emits a `FullFile` + `Complete` model.

Diagnostics are a **full-file replace snapshot**, pushed unsolicited from RA
`publishDiagnostics`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

pub struct CodeIntelDiagnostic {
    pub range: ByteRange,
    pub severity: CodeIntelSeverity,
    pub message: String,
    pub source: Option<String>, // e.g. "rustc", "clippy"
}

pub struct CodeIntelDiagnosticsPayload {
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    pub diagnostics: Vec<CodeIntelDiagnostic>, // replaces the prior set wholesale
}
```

Miss-fill navigation answers carry the correlating `navigate_id`. Definition can
resolve to more than one location (overloads, trait impls), so `targets` is a
list — the client jumps on **this** frame:

```rust
pub struct CodeIntelNavigateResultPayload {
    pub navigate_id: u64,
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    /// Empty means "no definition found here" (a valid answer, not an error).
    pub targets: Vec<CodeIntelLocation>,
}
```

Hover answers carry the correlating `hover_id`:

```rust
pub struct CodeIntelHoverResultPayload {
    pub hover_id: u64,
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    /// None means "nothing to show here" (a valid answer, not an error).
    pub contents: Option<String>, // markdown
    pub range: Option<ByteRange>,
}
```

### 4.3 Find-references (streamed, like project search)

Find-references over a whole project is too expensive to push. It is modeled
directly on `search_project_files` (`server/src/host.rs:5877`) and the
`ProjectSearch*` frames (`protocol/src/types.rs:2565`): a client-chosen domain
id, one streamed frame per file, a single terminal frame, supersession + cancel.

```rust
pub struct CodeIntelFindReferencesPayload {
    pub references_id: u64,     // domain id, like search_id
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
    pub offset: u32,            // the symbol to find references to
    pub include_declaration: bool,
}

pub struct CodeIntelCancelReferencesPayload {
    pub references_id: u64,
}

pub struct CodeIntelReferenceLine {
    pub line_number: u32,      // 1-based
    pub line_text: String,     // sent verbatim
    pub ranges: Vec<ByteRange>, // byte ranges into line_text
}

pub struct CodeIntelReferencesFileResult {
    pub path: ProjectPath,
    pub lines: Vec<CodeIntelReferenceLine>,
    pub truncated: bool,       // per-file cap hit
}

/// One matching file's references. Streamed incrementally.
pub struct CodeIntelReferencesResultsPayload {
    pub references_id: u64,
    pub file: CodeIntelReferencesFileResult,
}

/// Terminal frame: totals, truncation, cancellation, error.
pub struct CodeIntelReferencesCompletePayload {
    pub references_id: u64,
    pub total_files: u32,
    pub total_references: u32,
    pub truncated: bool,
    pub cancelled: bool,
    pub error: Option<String>,
}
```

A newer `references_id` (or a matching cancel) supersedes any in-flight query,
exactly as a newer `search_id` supersedes a project search.

### 4.4 Errors

Code-intel failures are operational failures scoped to the project stream
(`02-protocol.md` §6.2), not protocol violations.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIntelErrorCode {
    ProviderUnavailable, // binary absent
    ProviderCrashed,
    UnsupportedLanguage,
    StaleVersion,        // request referenced a version the server no longer holds
    Timeout,
    ProtocolError,       // malformed LSP traffic from the provider
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodeIntelErrorContext {
    Subscribe { path: ProjectPath },
    Hover { hover_id: u64, path: ProjectPath },
    Navigate { navigate_id: u64, path: ProjectPath },
    FindReferences { references_id: u64, path: ProjectPath },
    Provider { language: CodeIntelLanguageId },
}

pub struct CodeIntelErrorPayload {
    pub code: CodeIntelErrorCode,
    pub message: String,
    pub context: CodeIntelErrorContext,
    pub fatal: bool,
}
```

`fatal: false` fails the one intent and leaves the stream usable; `fatal: true`
means the provider for that scope is dead. A crashed provider surfaces as
`code_intel_error` **and** a `code_intel_status { state: Failed }` — never a
silent empty model.

---

## 5. Server Architecture

```text
project-stream actor (owns ProjectFileVersion counter)
    │  routes CodeIntel* frames
    ▼
project code-intel router (thin; per project, no provider state)
    │  picks root by ProjectPath  →  delegates to that root's service
    ▼
CodeIntelService actor (authoritative, per project root)
    │  match on internal Language + supports_path  →  selects provider
    ▼
provider actor (owns one subprocess for this root)
    rust_analyzer.rs  →  subprocess.rs (group spawn)  →  lsp_codec.rs (Content-Length)
```

- The **project-stream actor** owns the version counter and routes `CodeIntel*`
  input frames to the project's thin router. All `CodeIntel*` output frames flow
  back out on the same `/project/<project_id>` stream.
- The **project router** is plumbing only — it holds no provider state, picks
  the target root from the frame's `ProjectPath`, and delegates to that root's
  `CodeIntelService` actor.
- The **`CodeIntelService` actor** (the authoritative service, per root) maps
  each subscribed file to a provider by `match`ing on the internal `Language`
  enum and checking `supports_path`. It owns no subprocess itself; it owns the
  root's provider handle.
- Each **provider actor** owns exactly one subprocess for that root, drives the
  LSP lifecycle (`initialize` → `initialized` → `didOpen` → … → `shutdown`),
  and is the **only** place UTF-16↔byte conversion happens.

Local and remote are identical: the provider runs host-side in both cases, with
**no transport branching**. The only thing that differs by host is
`CodeIntelResourceMode` — a constrained host advertises `Limited`, which changes
the **pace and eagerness** of progressive delivery (§ M6), not the final scope:
the model still converges on the whole file. The code path is one.

---

## 6. Frontend Rendering

The frontend renders pushed state. It never asks "what is a definition."

- **Separate signal.** Code-intel state lives in its own signal keyed by
  `(host_id, project_id, path, ProjectFileVersion)`. Do **not** add byte
  offsets or decorations to the `Token` struct in
  `frontend/src/syntax_highlight.rs`. That struct feeds per-row rendering, and
  `frontend/src/components/file_view.rs` has a wasm test guarding against
  per-row text mangling (root `CLAUDE.md` hard rule). Mixing semantic data into
  tokens would put decoration logic on the exact path that test protects.

- **Per-line decorations as a `Memo`.** Derive a `Vec<LineDecoration>` per line
  from the code-intel signal, then **split syntax spans at decoration byte
  boundaries** using `FileLines` byte↔line helpers
  (`frontend/src/line_source.rs`). Those helpers do not exist yet (only `new`,
  `len`, `line`), so adding them is a prerequisite (see §2.2). Splitting must
  preserve the exact text — the rendered characters are identical with or
  without the overlay.

- **Navigation** is a local lookup of the pushed `definition` targets for the
  clicked occurrence (now a `Vec` — a click with multiple targets shows a
  chooser). On a miss (no target streamed yet), fire exactly one
  `code_intel_navigate` and **jump when the correlated `code_intel_navigate_result`
  arrives** (using its `navigate_id`). Command-click is not a request in the
  common case.

- **Hover** is on-demand `code_intel_hover` → a Leptos popover component. No
  `window.*` anything.

- **Diagnostics** render as inline squiggles + gutter markers from the
  replace-snapshot. A newer snapshot replaces the prior set wholesale.

- **Version match, not just stale-drop.** A `CodeIntel*` frame is applied
  **only when its `ProjectFileVersion` equals the version of the file contents
  currently rendered.** A frame with a *newer* version is **stashed** until the
  matching `ProjectFileContents` arrives, then applied; a frame with an *older*
  version is **dropped**. A simple "drop if older" rule is insufficient — it
  would let a newer-version map paint over older still-rendered text before the
  new contents land. The UI never paints v4 decorations over v5 text, and never
  paints v6 decorations over v5 text.

---

## 7. Milestones

Each milestone is independently shippable.

- **M0 — Protocol + skeleton.** `CodeIntel*` `FrameKind` variants, payloads, and
  serde; bump `PROTOCOL_VERSION` `13` → `14`; **introduce** the
  `ProjectFileVersion(u64)` newtype **and add a `version` field to
  `ProjectFileContentsPayload`** (neither exists today), with the centralized
  per-file counter in the project-stream actor; the per-root `CodeIntelService`
  actor (plus the thin project router) with a **mock provider**; honest
  `code_intel_status` rendering. No real language server yet.

- **M1 — Diagnostics.** Mock diagnostics first, then real rust-analyzer:
  `lsp_codec.rs` Content-Length framing, `initialize`/`didOpen`,
  `publishDiagnostics` → `code_intel_diagnostics` replace snapshots → squiggles
  + gutter.

- **M2 — Go-to-definition + hover, on-demand.** `code_intel_navigate` and
  `code_intel_hover` answered per request. No pushed map yet; clicks always
  miss-fill. Proves resolution + UTF-16↔byte conversion end to end.

- **M3 — Push the whole-file definition map.** Async/incremental, viewport
  prioritized. `code_intel_file_model` ships occurrences immediately, then
  streams targets the client merges by range. **No protocol or frontend frame
  change vs M2** — clicks now hit locally instead of miss-filling.

- **M4 — Versioning & external-change correctness.** Watcher / agent-write bump
  the single counter → service re-resolves → re-pushes; client drops stale
  versions. Closes the stale-decoration race.

- **M5 — Find-references.** Streamed, domain id, per-file previews + truncation,
  supersession + cancel — the project-search shape.

- **M6 — Large-file progressive delivery.** `code_intel_set_visible_range`
  prioritization plus resource caps. On big files the model streams as
  `ByteRange` + `Partial` chunks (visible range first) **on the way to** an
  eventual `FullFile` + `Complete` model. This is transient pacing, **not** a
  permanent range gate: resource limits change how fast and how eagerly chunks
  arrive, never the final whole-file scope. The UI shows partial coverage
  honestly while it fills in.

- **M7 — Second-language proof.** pyright or gopls: one new module + one
  `Language` match arm + extension mapping. **Zero** protocol or frontend
  change. This milestone exists to prove the abstraction, not to ship a
  language.

---

## 8. Testing

Per root `CLAUDE.md`. Native tests run under plain `cargo test`; UI tests run
under `tools/run-wasm-tests.sh`.

**Native:**

- `FrameKind::to_string()` covers every new `CodeIntel*` variant.
- serde round-trip for **every** new payload (and every enum variant).
- UTF-16 ↔ UTF-8 byte-offset conversion, with adversarial inputs: emoji,
  combining characters, CRLF.
- `lsp_codec.rs` Content-Length framing (partial reads, multiple messages in one
  buffer, header parsing).
- language / root detection (extension → `Language`, `supports_path`).
- version supersession: an older `ProjectFileVersion` frame is rejected.
- a **rust-analyzer-gated integration test** that drives a real RA subprocess
  and **skips-with-log** if RA is absent (never a silent pass, never a hard
  failure on a machine without RA).

**Inline wasm UI tests** (`frontend/src/components/file_view.rs` style):

- visible text is byte-for-byte unchanged when semantic spans overlay syntax
  tokens (the split-at-boundary invariant).
- diagnostics render at the correct line.
- command-click navigates from **pushed** data — assert it does **not** emit a
  request frame when the target is present.
- a stale-version model does not render over current text.

**The hard rule (root `CLAUDE.md`):** when one of these UI assertions fails
after a change, it may **not** be weakened or deleted to make the change pass
without explicit human approval. Fix the code, or explain why the assertion is
wrong and ask first. Routing around these tests defeats their purpose.

---

## 9. Open / Deferred

- **Managed binary download.** v1 is detect-and-hint only. A managed download
  (fetch a pinned rust-analyzer into `~/.tyde/...`, with a `confirm_dialog`
  prompt) is a possible future, not built now.
- **Inline editing / document actor.** The version model is built so a
  server-owned document actor can later own buffer state and drive LSP
  `didChange`. Out of scope until the editor becomes writable.
- **Idle-provider shutdown policy.** When to stop a provider subprocess after
  its files are all unsubscribed (immediate vs. grace period vs. project-lifetime)
  is unspecified. Pick a policy with measurement, not by guessing.
- **RA process-per-root vs per-workspace-folder.** v1 is one instance per
  project root, and RA handles multi-crate workspaces inside that root. Whether
  a multi-root project should instead use one RA with multiple `workspaceFolders`
  is an open nuance to revisit if process count or cross-root navigation
  pressure it.
