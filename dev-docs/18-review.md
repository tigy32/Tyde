# Review

Spec for the inline-comment **Review** feature. A user opens a frozen diff
snapshot, leaves typed comments on lines/hunks/files, optionally runs an AI
reviewer that proposes more comments (which the user accepts/rejects), and
submits the bundle as a structured user message back to the originating agent.

Audience: implementation agents and future maintainers.

This doc represents consensus between Claude and Codex design proposals (see
session 2026-05-04). Where they disagreed, the resolution and rationale are
recorded inline.

---

## 1. Goals

- A diff produced by an agent is reviewable in-place: select lines/hunks,
  attach markdown comments.
- An optional **AI reviewer** runs as a sub-agent and proposes additional
  comments via a typed tool call. The user accepts, rejects, or edits each.
- On submit, all accepted comments + the diff snapshot are bundled into a
  single deterministic user message and delivered to the originating agent
  session for rework.
- Reviews persist across server restarts and survive the originating agent
  process restarting (delivery routes through the durable session id).

Non-goals (v1):

- Multi-user / shared reviews.
- Re-anchoring comments when the working tree drifts after open.
- Reply threads on comments.
- Auto-running the AI reviewer on diff open.
- Backwards-compat for existing wire shapes (no deployed remotes).

---

## 2. Data model

All types live in `protocol/src/types.rs`. Strong typing throughout; no
strings-as-enums.

### IDs

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReviewId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReviewCommentId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReviewSuggestionId(pub String);
```

### Lifecycle

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewStatus {
    /// User editing — comments and AI suggestions can change.
    Draft,
    /// Frozen, accepted comments locked. Bundle queued for delivery; the
    /// originating agent may not be live yet.
    Submitted { submitted_at_ms: u64 },
    /// Bundle delivered to a live agent actor for the originating session.
    Consumed {
        submitted_at_ms: u64,
        consumed_at_ms: u64,
        target_agent_id: AgentId,
    },
    /// Explicit user discard. Terminal.
    Cancelled { cancelled_at_ms: u64 },
}
```

`Draft` is the only mutable state. `Submitted → Consumed` is driven by the
agent actor receiving and acknowledging the bundle (see §6). There is no
intermediate `Submitting` — submission is a server action, not a state.

### Diff selection

The review's default and only v1 scope is **all uncommitted changes** — the
combination of staged and unstaged work, equivalent to `git diff HEAD`. This
captures everything the agent has produced since the last commit, regardless
of whether the user has staged any of it.

The existing `ProjectDiffScope` is `Unstaged | Staged` only; we extend it with
a third variant:

```rust
pub enum ProjectDiffScope {
    Unstaged,
    Staged,
    /// `git diff HEAD` — staged + unstaged combined. Used by Review.
    Uncommitted,
}
```

`server/src/project_stream.rs` extends `read_diff()` to drive `git diff HEAD`
on `Uncommitted`. The existing `ProjectReadDiff` paths (git panel,
file-explorer diff tabs) are unchanged.

The selection itself is a typed enum so the v2 narrowing case (single root,
single file) is encodable without protocol churn:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewDiffSelection {
    /// v1 default. All uncommitted changes across all roots in the project.
    AllUncommitted,
    /// v2. One root, optionally narrowed to a path.
    Root {
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: Option<String>,
    },
}
```

Server resolves `AllUncommitted` to one `ProjectGitDiffPayload` per root with
`scope: Uncommitted` and `context_mode: FullFile`. Reviews always use
`FullFile` regardless of selection — comment anchors use absolute line numbers
and need stable line numbering. (Frontend may render the diff with a different
*display* mode; that's a render concern, not a snapshot concern.)

### Comment locations

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewLocation {
    pub root: ProjectRootPath,
    pub relative_path: String,
    pub anchor: ReviewAnchor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewAnchor {
    File,
    Hunk {
        hunk_id: String,
        old_start: u32,
        old_count: u32,
        new_start: u32,
        new_count: u32,
    },
    LineRange {
        side: ReviewDiffSide,
        start_line: u32,
        end_line: u32, // inclusive
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDiffSide {
    Old,
    New,
}
```

Rules enforced by the server actor on every mutation:

- Added/context lines → `LineRange { side: New, .. }`.
- Removed lines → `LineRange { side: Old, .. }`.
- `start_line <= end_line`, both within the snapshot's actual line numbers for
  that side.
- `Hunk { hunk_id, .. }` → `hunk_id` matches one in the snapshot.
- `File` always valid for any file present in the snapshot.

Invalid locations get `ReviewErrorCode::InvalidLocation`. No fuzzy matching, no
silent re-anchoring.

### Comments and suggestions (separate types)

User-authored and AI-suggested comments are distinct types. Accepting an AI
suggestion creates a real `ReviewComment` that references the suggestion id.
This eliminates the awkward "user comments are never Pending" implicit
invariant a single shared type would require, and makes the submit filter
trivial (ship `ReviewComment`s, ignore everything else).

```rust
pub struct ReviewComment {
    pub id: ReviewCommentId,
    pub location: ReviewLocation,
    pub body: String,                               // markdown
    pub source: ReviewCommentSource,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewCommentSource {
    User,
    AiSuggestion {
        suggestion_id: ReviewSuggestionId,
        edited: bool, // true if the user changed body before accepting
    },
}

pub struct ReviewSuggestedComment {
    pub id: ReviewSuggestionId,
    pub location: ReviewLocation,
    pub body: String,
    pub rationale: Option<String>,
    pub severity: ReviewSeverity,
    pub state: ReviewSuggestionState,
    pub reviewer_agent_id: AgentId,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity { Info, Warn, Bug }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewSuggestionState {
    Pending,
    Accepted { comment_id: ReviewCommentId },
    Rejected,
}
```

### Review record

```rust
pub struct Review {
    pub id: ReviewId,
    pub project_id: ProjectId,
    pub origin_agent_id: AgentId,       // live-routing hint
    pub origin_session_id: SessionId,   // durable anchor for delivery
    pub selection: ReviewDiffSelection,
    pub status: ReviewStatus,

    /// Frozen at open. Reuses existing diff payload type unchanged.
    pub diffs: Vec<ProjectGitDiffPayload>,

    pub comments: Vec<ReviewComment>,
    pub suggestions: Vec<ReviewSuggestedComment>,
    pub ai_reviewer: ReviewAiReviewerState,

    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
```

A review is tied to **both** project and session: project owns the diff/file
context, session owns the durable "send feedback back to the same work thread"
semantics. The live `AgentId` is only the current delivery endpoint; if the
agent process dies, the review still routes via session id when a new agent
attaches.

There is no `Manual` origin. Every review is born from an agent's diff and
must have a session to submit back to.

### AI reviewer state

```rust
pub struct ReviewAiReviewerState {
    pub status: ReviewAiReviewerStatus,
    pub agent_id: Option<AgentId>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAiReviewerStatus {
    Idle,
    Running,
    Completed,
    Failed,
}
```

No `counts` field on `Review` — counts derive from `comments` + `suggestions`.
The frontend computes them via `Memo`. Caching counts in the protocol would
violate the philosophy ("do not cache by default").

---

## 3. Protocol

### Stream

```text
/review/<review_id>
```

Server-created on `ReviewCreate`. First event on attach is always
`ReviewEvent::Snapshot`.

### `FrameKind` additions

Three new variants. Granular operations are tagged-union payloads inside
`ReviewAction` and `ReviewEvent` rather than dozens of FrameKind variants —
this matches existing protocol patterns and keeps `FrameKind` tractable while
preserving exhaustive matching on the server side.

```rust
pub enum FrameKind {
    // ...existing variants...

    // Client → server
    ReviewCreate, // on /project/<project_id>
    ReviewAction, // on /review/<review_id>

    // Server → client
    ReviewEvent,  // on /review/<review_id>
}
```

### `ReviewCreate` (client → server)

Sent on `/project/<project_id>`. Creation failures arrive on the existing
project error stream because the review stream doesn't exist yet.

```rust
pub struct ReviewCreatePayload {
    pub origin_agent_id: AgentId,
    pub selection: ReviewDiffSelection,
}
```

### `ReviewAction` (client → server)

Sent on `/review/<review_id>`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewActionPayload {
    AddComment { location: ReviewLocation, body: String },
    UpdateComment { comment_id: ReviewCommentId, body: String },
    DeleteComment { comment_id: ReviewCommentId },

    AcceptSuggestion {
        suggestion_id: ReviewSuggestionId,
        /// If `Some`, the user edited the body before accepting.
        edit: Option<String>,
    },
    RejectSuggestion { suggestion_id: ReviewSuggestionId },

    StartAiReview {
        backend_kind: BackendKind,
        cost_hint: Option<SpawnCostHint>,
        instructions: Option<String>,
    },

    Submit,
    Cancel,
}
```

### `ReviewEvent` (server → client)

Sent on `/review/<review_id>`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewEventPayload {
    Snapshot { review: Review },
    CommentUpsert { comment: ReviewComment },
    CommentDelete { comment_id: ReviewCommentId },
    SuggestionUpsert { suggestion: ReviewSuggestedComment },
    AiReviewerChanged { state: ReviewAiReviewerState },
    StatusChanged { status: ReviewStatus },
    Error { error: ReviewErrorPayload },
}
```

`Snapshot` is sent once on subscribe and after recovery from a transient
failure. All other events are deltas; the client maintains the projection.

### Errors

```rust
pub struct ReviewErrorPayload {
    pub code: ReviewErrorCode,
    pub message: String,
    pub fatal: bool,
    pub context: ReviewErrorContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewErrorCode {
    InvalidStatus,
    InvalidLocation,
    UnknownComment,
    UnknownSuggestion,
    OriginAgentNotRunning,
    AmbiguousOriginSession,
    ReviewerAlreadyRunning,
    ReviewerBackendUnsupported,
    GitFailed,
    IoFailed,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewErrorContext {
    AddComment,
    UpdateComment { comment_id: ReviewCommentId },
    DeleteComment { comment_id: ReviewCommentId },
    AcceptSuggestion { suggestion_id: ReviewSuggestionId },
    RejectSuggestion { suggestion_id: ReviewSuggestionId },
    StartAiReview,
    Submit,
    Cancel,
}
```

### Project-stream addition

The project stream needs to announce existing reviews so the UI can list them
without subscribing to every `/review/<id>`. Add one event:

```rust
// In ProjectEventPayload (existing enum)
ReviewListChanged {
    reviews: Vec<ReviewSummary>,
}

pub struct ReviewSummary {
    pub id: ReviewId,
    pub status: ReviewStatus,
    pub origin_session_id: SessionId,
    pub origin_agent_id: AgentId,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub user_comment_count: u32,    // derived; convenience for list rendering
    pub pending_suggestion_count: u32,
}
```

Counts here are *summary-only* convenience for the list view (cheap to derive
on emit, expensive to compute reactively across all reviews). They do not
appear on the per-review `Review` record.

---

## 4. Server behavior

### Module layout

```
server/src/review/
    mod.rs           // ReviewRegistry, ReviewHandle
    actor.rs         // ReviewActor + ReviewCommand
    bundle.rs        // ReviewFeedbackBundle assembly + markdown rendering
    reviewer.rs      // ReviewerToolBridge — adapts AI sub-agent tool calls
server/src/store/
    review.rs        // JSON persistence
```

### Registry and actors

`ReviewRegistry` is a single tokio task owning `HashMap<ReviewId,
ReviewHandle>`. No `Arc<Mutex<...>>`. Each `ReviewHandle` is an `mpsc::Sender`
to a per-review `ReviewActor` task.

```rust
pub enum ReviewCommand {
    Subscribe { conn: ConnectionId, tx: mpsc::Sender<Envelope> },
    Unsubscribe { conn: ConnectionId },
    Action(ReviewActionPayload, ConnectionId),
    /// From the AI reviewer tool bridge.
    AiSuggestion { suggestion: ReviewSuggestedComment },
    AiReviewerExited { result: Result<(), String> },
    /// From the agent actor when it accepts a review-bundle message.
    BundleConsumed { target_agent_id: AgentId, at_ms: u64 },
}
```

`HostState` holds a `ReviewRegistryHandle`. The router dispatches
`ReviewCreate` to the registry; `ReviewAction` and review subscription frames
to the per-review actor.

### Creation flow

1. Router receives `FrameKind::ReviewCreate` on `/project/<project_id>`.
2. Host validates: project exists, originating agent is alive and bound to
   that project, originating session id is recoverable from the agent.
3. Server resolves `ReviewDiffSelection` to concrete `(root, scope, path)`
   tuples. For each, call `project_stream::read_diff(.., DiffContextMode::FullFile)`
   — the same function `ProjectReadDiff` uses today.
4. Snapshots stored verbatim on the new `Review`.
5. Registry spawns a `ReviewActor` and persists the record.
6. Actor emits `ReviewEvent::Snapshot` to the creating connection and pushes
   `ProjectEventPayload::ReviewListChanged` to project subscribers.

The diff snapshot is **frozen** for the review's lifetime. Filesystem changes
to the working tree do not retroactively change the review. Rationale: comment
anchors use absolute line numbers; live recompute would silently drift them
("invalid states unrepresentable").

### Mutation flow

Every `ReviewAction` runs through the actor:

- Validate `status == Draft` (or `Cancel`/`Submit` from `Draft`).
- Validate IDs and locations against the snapshot.
- Mutate the in-memory record.
- Persist to disk.
- Emit the corresponding typed `ReviewEvent`.

No frontend-created optimistic state, no fuzzy lookups, no fallbacks. Every
failure becomes a `ReviewEvent::Error` with full `context` and a typed `code`.

### Submit flow

`ReviewActionPayload::Submit`:

1. Require `status == Draft`.
2. Require at least one `ReviewComment` (suggestions don't count until
   accepted).
3. Require AI reviewer not in `Running` (the user should wait or cancel it).
4. Set `status = Submitted { submitted_at_ms }`. Persist. Emit `StatusChanged`.
5. Build a `ReviewFeedbackBundle` (see §6).
6. Look up the originating session in the agent registry:
   - If exactly one live agent maps to `origin_session_id` → deliver to that
     agent actor (not necessarily the original `origin_agent_id` — sessions
     outlive processes).
   - If zero live agents → leave `Submitted`; deliver later when the session
     resumes.
   - If multiple → emit `ReviewError { code: AmbiguousOriginSession, fatal:
     false }`. Do not guess.
7. The agent actor sends back `ReviewCommand::BundleConsumed` once the message
   is enqueued for the backend; the actor flips status to `Consumed`.

Session-resume hook: when an agent actor starts and binds to a session, it
queries the review registry for `Submitted` reviews targeting that session and
attempts delivery.

### Cancel flow

`ReviewActionPayload::Cancel`: requires `Draft`. Sets `status = Cancelled
{ cancelled_at_ms }`. Persist. Emit `StatusChanged`. Cancelled reviews are
hidden from `ReviewListChanged` by default; a future history view can opt in.

### Persistence

JSON, matching existing stores (`server/src/store/session.rs`,
`server/src/store/project.rs`). Path: `~/.tyde/reviews.json` or, if size
warrants, one file per review under `~/.tyde/reviews/<id>.json`.

A `ReviewStore` mirrors `SessionStore`:

```rust
#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<ReviewId, Review>,
}

pub struct ReviewStore { path: PathBuf }
```

On startup, the registry loads all records and starts an actor for each. AI
reviewer sub-agents are **not** resumed automatically — backend processes
don't survive restart, same rule as regular sub-agents. The user re-runs
`StartAiReview` if needed.

---

## 5. Frontend behavior

### Entry points

Every review must have an originating agent (we dropped the `Manual` origin
during consensus), so reviews can only be started from a surface that already
identifies one. v1 has two entry points:

- **Agent header.** A "Review changes" button appears whenever the agent owns
  a project and the project has uncommitted changes. Click → `ReviewCreate`
  with `selection: AllUncommitted` and `origin_agent_id` set to the current
  agent. This is the primary path.
- **Reviews list (project surface).** `ProjectEventPayload::ReviewListChanged`
  populates a list of existing reviews with status badges. Click navigates to
  the review tab. The list does **not** offer "create new" — creation always
  goes through an agent.

The git panel does **not** start reviews (it has no agent context). It can,
however, *display* an indicator that a review is open against the current
working tree, with a link to that review's tab.

### `ReviewView` component

Lives at `frontend/src/components/review_view.rs`. Mounts as a workbench tab
when the user opens a review. The review can span many files across many
roots, so the layout is a **three-pane multi-file workspace**, not a
single-file view.

```
┌───────────────────────────────────────────────────────────────────────┐
│ Review header: status badge, origin agent, created-at                 │
├──────────────┬──────────────────────────────────────┬─────────────────┤
│              │                                      │                 │
│ File list    │  Diff for the selected file          │  Sidebar        │
│ (left rail)  │  (center, scrollable)                │  (right rail)   │
│              │                                      │                 │
│ root-a/      │  src/foo.rs                          │  Run AI         │
│  src/foo.rs  │  ┌──────────────────────────────┐    │  reviewer       │
│  · 2 ⓘ      │  │ @@ -10,3 +10,4 @@           │    │  [Backend ▾]    │
│  src/bar.rs  │  │  fn handle()                 │    │  [Cost ▾]       │
│  · 1 AI      │  │ +    let x = 1;              │    │  [Run]          │
│              │  │      if x > 0 { ... }        │    │                 │
│ root-b/      │  │                              │    │  AI status:     │
│  Cargo.toml  │  │  💬 user — "explain"  · ····│    │  Idle           │
│              │  │  💬 ai/warn — "..."  [Acc.] │    │                 │
│              │  └──────────────────────────────┘    │  Submit ▶       │
│              │                                      │  Cancel         │
└──────────────┴──────────────────────────────────────┴─────────────────┘
```

**Reuse vs new:**

- The **diff renderer is reused** wholesale — `diff_view::UnifiedHunk` and
  `diff_view::SideBySideHunk` from `frontend/src/components/diff_view.rs`,
  same syntax highlighting, same gutter layout, same display-mode toggle. We
  add a single prop: a reactive `comments_at(file, line, side) -> Vec<...>`
  lookup that returns the comments + pending suggestions anchored to that
  row. The hunk renderer calls it for each row and inserts a thread region
  beneath.
- The **file list is new** — it's a small component that renders one row per
  file in the review (grouped by root), each with comment-count badges
  (`2 user`, `1 AI pending`). Click selects that file as the visible diff.
  This is structurally similar to the git panel's file list but reads from
  the review's frozen `diffs` rather than live git status.
- The **sidebar is new** — review-specific controls.

**Pane 1 — file list (left rail, ~220px):**

- Tree grouped by root, files listed flat under each root.
- Each row: file path, change indicator (added / modified / deleted), and
  comment count badges.
- The currently selected file is highlighted; navigation via click,
  arrow-keys, or `j`/`k`.
- Empty-file-with-comments stays in the list (file-level comments anchor to
  `ReviewAnchor::File`).

**Pane 2 — diff for selected file (center):**

- Renders the selected file's diff using the existing hunk renderers, fed
  from `Review.diffs`.
- The file-level header gets a comment affordance (file-level comment +
  thread region for `ReviewAnchor::File` comments).
- Each hunk header gets a hunk-level comment affordance (`ReviewAnchor::Hunk`).
- Hovering any code row reveals a `+` gutter button (line comment).
- Drag-select multiple rows extends the range (`LineRange { side, start, end }`).
- Comment threads render inline under the relevant row, with per-source
  affordances:
  - User comment: Edit, Delete.
  - Pending AI suggestion: Accept, Edit & Accept, Reject.
  - Accepted AI suggestion: rendered as the resulting `ReviewComment` with
    an "AI" pill.
  - Rejected AI suggestion: collapsed by default; "show N rejected" toggle.
- The display-mode toggle (unified vs side-by-side) and context-mode toggle
  (full-file vs hunks) from the existing diff toolbar are reused. Note: the
  *snapshot* is always FullFile; the toggle just changes how the snapshot is
  rendered.

**Pane 3 — sidebar (right rail, ~260px):**

- "Run AI reviewer" group: backend picker, cost-hint picker, optional
  instructions textarea, Run button. Same shape as `SpawnAgent`.
- AI reviewer status indicator (Idle / Running / Completed / Failed) plus a
  link to the reviewer sub-agent's chat in the agents panel for users who
  want to watch its reasoning.
- Submit button: disabled when `status != Draft` or zero `ReviewComment`s.
- Cancel review button: disabled when `status != Draft`.
- Status banner when `Submitted` (waiting for delivery) or `Consumed`
  (delivered to agent X at time T).

### Composer

The inline composer (textarea + Save / Cancel) opens pinned to whichever
location the user picked (file header, hunk header, or row range). Body text
is local UI state (like chat input). The committed comment is server state,
only created on `AddComment` action and reflected back via `CommentUpsert`.

### Reactivity rules (philosophy compliance)

- No optimistic UI. Action buttons disable on click and re-enable when the
  server echoes the change.
- No frontend caches of derived state. Counts, "is this row commented", "are
  there pending suggestions" all derive via `Memo` from the per-review signal.
- `state.rs` adds `reviews: RwSignal<HashMap<ReviewId, Review>>`. Dispatch
  applies `ReviewEvent` deltas onto the entry.
- `ReviewView` mounts `ReviewBody` exactly once per review id, gated only by a
  loaded boolean. Subsequent `ReviewEvent` deltas must flow through child
  memos/signals and must not remount `ReviewBody` or reset the diff scroll
  container.
- Late-subscribe replay: server emits `Snapshot` first, then current state
  events as needed.

### Diff rendering note

Inline comment threads have variable height. They render as decoration slots
inside the existing diff renderer rather than through a separate review-only
diff path. Rows without decorations still preserve the normal virtualized
path, but decorated rows can invalidate the fixed-row-height spacer
assumptions. Large reviews therefore need scroll-stability regression tests;
precise variable-height virtualization remains deferred.

Thread regions must clamp to the visible diff inline-size, not the full
horizontal scroll width. `ReviewView` maintains `--diff-scrollport-width` with
a `ResizeObserver` on `.diff-content`, and `.review-thread-region` uses that
custom property for its width/max-width. Do not use `100cqi` here: in WKWebView
horizontal scroll containers, container query inline units resolve against the
scroll-width, which lets review threads expand wider than the visible diff
viewport.

---

## 6. AI reviewer

### Trigger

Manual only in v1. A "Run AI reviewer" button issues
`ReviewActionPayload::StartAiReview { backend_kind, cost_hint, instructions }`.
Auto-on-open is tempting but has a real LLM cost per diff; keep it explicit.

### Mechanism: a real sub-agent

The reviewer is a first-class sub-agent of the originating agent's session,
spawned via the existing sub-agent path (see `dev-docs/15-sub-agents.md`):

- `parent_agent_id = Some(origin_agent_id)`
- `origin = AgentOrigin::AgentControl`
- `project_id = Some(review.project_id)`
- `name = "AI Review"`
- Backend constructed with a review-only system prompt that lists the changed
  files. The frozen diff snapshot is **not** embedded in the prompt; reviewers
  inspect the current uncommitted files with read-only file tools.

Why a real sub-agent and not a one-shot internal task: uniformity. Sub-agents
already replay across frontends, persist sessions, surface errors via the
agents panel, and support `Interrupt` for cancellation. The user can watch
the reviewer's chat stream if they want to.

### Suggestion routing

The reviewer gets a dedicated `tyde-review-feedback` MCP server, separate from
the generic `tyde-agent-control` orchestration surface. It must call a typed
internal Tyde tool:
`propose_review_comment(location, body, severity, rationale)`. No file edits,
no `run_command` — the reviewer's tool surface is restricted to the propose
tool and read-only context tools.

The review MCP tool handler receives each `propose_review_comment` call,
identifies the calling reviewer from the injected agent id, and:

1. Validates the arguments against the review's frozen diff.
2. Builds a `ReviewSuggestedComment { state: Pending, .. }`.
3. Sends `ReviewCommand::AiSuggestion { suggestion }` to the right
   `ReviewActor`.
4. Returns a tool result to the reviewer (success or validation error).

The actor stores the suggestion and emits `SuggestionUpsert` to its
subscribers.

`ReviewerToolBridge` observes the reviewer's agent stream for lifecycle and
error events. When the reviewer goes idle or its session terminates, the bridge sends
`AiReviewerExited` and the actor emits
`AiReviewerChanged { status: Completed | Failed }`.

### Cancel

`Interrupt` on the reviewer's agent stream — same path as any sub-agent.
There is no `ReviewActionPayload::CancelAi`; cancelling the agent is the
mechanism.

### Tool surface for v2

Reviewers use backend read-only file tools to inspect the files under review.
The server still validates proposed anchors against the frozen uncommitted diff.

---

## 7. Submitting back to the agent

### No new `AgentInput` variant

The bundle is delivered as a **regular** user message via existing
`SendMessagePayload`. Backends accept text input today; this is the clean
boundary that doesn't require per-backend awareness of "review bundles".

To preserve the linkage (so the agent actor can flip the review to
`Consumed`), extend `SendMessagePayload` with an optional origin tag:

```rust
pub struct SendMessagePayload {
    pub text: String,
    pub attachments: Vec<Attachment>,
    // ...
    #[serde(default)]
    pub origin: Option<MessageOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageOrigin {
    User,
    Review { review_id: ReviewId },
}
```

Default `None` keeps existing call sites unchanged (`#[serde(default)]` is
acceptable here — the field is genuinely additive metadata, not a fallback for
malformed input).

### Bundle assembly

```rust
pub struct ReviewFeedbackBundle {
    pub review_id: ReviewId,
    pub project_id: ProjectId,
    pub origin_session_id: SessionId,
    pub comments: Vec<ReviewFeedbackComment>,
}

pub struct ReviewFeedbackComment {
    pub comment_id: ReviewCommentId,
    pub location: ReviewLocation,
    pub body: String,
    pub source: ReviewCommentSource,
    /// The diff lines this comment is anchored to, copied from the snapshot
    /// for context. Empty for `ReviewAnchor::File`.
    pub excerpt: Vec<ProjectGitDiffLine>,
}
```

The agent actor renders the bundle into a deterministic markdown user message
with a fenced `tyde-review` JSON block at the top and per-comment headed
sections:

```text
The user finished a review with N comments. Address each and update the code.

```tyde-review
{ "review_id": "...", "comments": [...] }
```

### src/foo.rs:42-45 (user)
> The error path here swallows the parse error.

> Original lines:
> ```
>     return Ok(());
> ```

### src/foo.rs:80 (ai, warn)
> ...
```

The fenced JSON block is opt-in for backends that want structured input; the
human-readable sections work for any backend that consumes plain text. This
keeps the integration additive — no `Backend` trait changes needed for v1.

### Consumption

The agent actor:

1. Receives `SendMessagePayload { origin: Some(Review { review_id }), .. }`.
2. Records `(review_id → session_id, agent_id)` and forwards
   `ReviewCommand::BundleConsumed { target_agent_id, at_ms }` to the review
   registry.
3. Sends the rendered text to the backend via the normal send path.

The review actor flips `Submitted → Consumed` on `BundleConsumed` and emits
`StatusChanged`. The synthetic message also appears in the agent's chat
naturally because it goes through the same path. UI polish (collapsed "Review
submitted: 3 comments" card with expand) is deferred.

---

## 8. Lifecycle and storage

| State        | Mutable | Persisted | Survives restart                   |
|--------------|---------|-----------|------------------------------------|
| Draft        | yes     | yes       | yes — actor reloaded               |
| Submitted    | no      | yes       | yes — delivery retried on resume   |
| Consumed     | no      | yes       | yes — viewable as history          |
| Cancelled    | no      | yes       | hidden from list by default        |

Multi-frontend: same replay model as agents/projects. New frontend connects →
project stream emits `ReviewListChanged` → user opens a review → frontend
subscribes to `/review/<id>` → server emits `Snapshot`. Subsequent deltas fan
out to all subscribers.

AI reviewer sub-agents are not auto-resumed across server restart (backend
processes are dead). The `ai_reviewer.status` is reset to `Idle` on rehydrate
for any review that was `Running` at shutdown.

---

## 9. File change checklist

Protocol (`protocol/src/types.rs`):

- New IDs: `ReviewId`, `ReviewCommentId`, `ReviewSuggestionId`.
- Extend `ProjectDiffScope` with `Uncommitted` variant.
- New enums: `ReviewStatus`, `ReviewDiffSelection`, `ReviewAnchor`,
  `ReviewDiffSide`, `ReviewCommentSource`, `ReviewSuggestionState`,
  `ReviewSeverity`, `ReviewAiReviewerStatus`, `ReviewErrorCode`,
  `ReviewErrorContext`, `MessageOrigin`.
- New structs: `Review`, `ReviewComment`, `ReviewSuggestedComment`,
  `ReviewLocation`, `ReviewAiReviewerState`, `ReviewSummary`, payload structs.
- New `FrameKind`: `ReviewCreate`, `ReviewAction`, `ReviewEvent`.
- Extend `ProjectEventPayload` with `ReviewListChanged { reviews }`.
- Extend `SendMessagePayload` with `origin: Option<MessageOrigin>`.

Server:

- `server/src/review/{mod,actor,bundle,reviewer}.rs` — new module.
- `server/src/store/review.rs` — JSON persistence.
- `server/src/host.rs` — wire `ReviewRegistryHandle` into `HostState`,
  dispatch new frame kinds, emit `ReviewListChanged` on project subscribe.
- `server/src/router.rs` — route `ReviewCreate` to registry; route
  `ReviewAction`/subscribe frames to per-review actor.
- `server/src/agent/...` — receive `BundleConsumed` confirmation path; honor
  `MessageOrigin::Review` when recording session linkage.

Frontend:

- `frontend/src/state.rs` — `reviews: RwSignal<HashMap<ReviewId, Review>>`,
  `review_summaries: RwSignal<HashMap<ProjectId, Vec<ReviewSummary>>>`.
- `frontend/src/dispatch.rs` — handle `ReviewEvent` deltas and
  `ProjectEventPayload::ReviewListChanged`.
- `frontend/src/components/review_view.rs` — new three-pane component
  (file list + reused diff renderer + sidebar).
- `frontend/src/components/diff_view.rs` — accept an optional review-mode
  prop with `comments_at` lookup; render gutter `+` affordance and inline
  thread region. Hunk renderers (`UnifiedHunk`, `SideBySideHunk`) reused
  unchanged otherwise.
- `frontend/src/components/{chat_view,header}.rs` — "Review changes" button on
  the agent header when the project has uncommitted changes.
- `frontend/src/components/git_panel.rs` — *display-only* indicator linking
  to an open review against the current working tree (no creation button).

---

## 10. Tests

Protocol (`protocol/tests/`):

- Serde round-trip for every new type and payload variant.
- `MessageOrigin` defaults to `None` when missing from JSON.

Server (`server/src/review/...` unit tests + `tests/tests/review.rs`):

- Create review → snapshot emitted with correct frozen diff.
- Add/update/delete comment → typed delta events; no mutation emits or requires
  a fresh `Snapshot`; persistence on disk.
- Invalid location (line out of range, wrong side) → typed error, no mutation.
- AI reviewer suggestion via tool bridge → `SuggestionUpsert` with
  `state: Pending`. Accept → `ReviewComment` created with `source:
  AiSuggestion { suggestion_id, edited: false }`, suggestion transitions to
  `Accepted { comment_id }`. Reject → suggestion transitions to `Rejected`.
- Accept/reject suggestion and status mutations are delta events; `Snapshot` is
  reserved for subscribe/recovery and must not be part of a normal mutation
  response.
- Submit with no comments → `InvalidStatus`-class error.
- Submit with origin agent live → `Consumed` after `BundleConsumed`.
- Submit with origin session offline → `Submitted`, then on session resume
  delivery completes → `Consumed`.
- Submit with multiple live agents on same session → `AmbiguousOriginSession`,
  status reverts to `Draft`.
- Cancel from `Draft` → `Cancelled`. Cancel from `Submitted` → error.
- Restart server → `Draft`/`Submitted`/`Consumed`/`Cancelled` reviews all
  rehydrate; `Running` AI reviewer state resets to `Idle`.

Frontend (`frontend/src/**/wasm_tests`, see `CLAUDE.md` for the rules — these
tests are inviolate):

- Review view renders threads at the correct lines for both unified and
  side-by-side modes.
- Pending AI suggestion renders Accept/Reject/Edit affordances; rejected
  suggestion is hidden by default.
- Submit button disabled when `status != Draft` or `comments.is_empty()`.
- Counts shown in sidebar derive from the comments/suggestions signal (no
  cached count field).
- Review deltas do not remount `ReviewBody` or replace the diff scroll
  container; tests assert scroll container identity/scroll position survives
  comment and suggestion mutations.
- Inline thread regions are width-bounded to the visible diff viewport in both
  unified and side-by-side modes.
- The `.diff-content` `ResizeObserver` wires `--diff-scrollport-width` and
  updates it when the scrollport is resized.

End-to-end (`tests/tests/review_e2e.rs`):

- Spawn agent, agent edits a file, frontend opens review, adds a comment,
  runs AI reviewer (mock backend that always proposes one suggestion),
  accepts the suggestion, submits, asserts the originating agent receives a
  `SendMessagePayload` with `origin: Review { .. }` and the rendered
  `tyde-review` block.

---

## 11. Deferred

- Auto-run AI review on diff open.
- Reply threads (`parent_comment_id`) on comments.
- Re-anchoring comments when the working tree drifts after open.
- Retention / GC of old `Consumed` and `Cancelled` reviews.
- "Review history" panel that shows `Cancelled` reviews.
- Backend-side parsing of the `tyde-review` fenced block as structured tool
  input (v1 just sends text).
- `read_files` / additional context tools for the AI reviewer.
- Multi-root review UX polish (each root currently shown as its own diff
  block in the same view).
- More precise virtualized rendering/measurement for variable-height inline
  comment threads.

---

Protocol v4 note: review streams start with `review_bootstrap` seq 0 carrying the current review snapshot. Later review changes remain `review_event` deltas. Project-level review summaries are part of `project_bootstrap`.
