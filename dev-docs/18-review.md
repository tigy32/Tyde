# Review

Spec for Tyde's inline Review feature.

Reviews are an **always-on, root-scoped inline layer** over each project root's
current unstaged git diff. The primary UX is the project diff view: comments,
AI suggestions, stale-anchor badges, and submit controls render inline with the
changed files for that root. There is no standalone Review tab and no
start/open/cancel/close lifecycle in the new UX.

Audience: implementation agents and future maintainers.

---

## 1. Current model

- Exactly one active draft review exists for each `(project_id, root)` pair.
  `root` is a `ProjectRootPath` from `Project { roots: Vec<String> }`.
- Active reviews are implicit. Project bootstrap and review summary updates
  surface them; clients should not send `ReviewCreate` just to make a review
  appear in the UI.
- Active reviews always track `ProjectDiffScope::Unstaged` for their root.
  Staged-only changes are not part of the active inline review.
- Submitting feedback does not move the active review into a durable
  submitted/consumed queue. After successful delivery the server clears
  comments, suggestions, and AI reviewer state and keeps the same draft review
  ready for the next unstaged diff.
- Diff refreshes never silently re-anchor comments. If a stored anchor no
  longer matches the current unstaged diff, the server marks it stale and
  leaves the original location unchanged.
- When a root's unstaged diff becomes clean, that root's active review resets:
  comments, suggestions, AI state, and diff payloads are cleared.
- Feedback can be submitted either to an existing open same-project agent or to
  a newly spawned same-project agent.

Legacy `Submitted`, `Consumed`, `Cancelled`, project-only draft, and
origin-session records may still be present in persisted stores. They must
safely deserialize and remain subscribable by id where possible, but they are
not the active inline review surface. Project-only legacy drafts are hidden from
active summaries so they cannot create multiple active reviews for a root.

---

## 2. Protocol contract

### Streams

```text
/project/<project_id>   ReviewCreate (legacy/get-or-create), ProjectBootstrap,
                       ProjectEvent::ReviewListChanged
/review/<review_id>     ReviewSubscribe, ReviewAction, ReviewEvent
```

### Project bootstrap and summaries

`ProjectBootstrapPayload.review_summaries` and
`ProjectEventPayload::ReviewListChanged` are the source of active review ids.
Each active summary includes the root it belongs to:

```rust
pub struct ReviewSummary {
    pub id: ReviewId,
    pub root: ProjectRootPath,
    pub status: ReviewStatus, // Draft for active reviews
    pub user_comment_count: u32,
    pub pending_suggestion_count: u32,
    pub file_comment_counts: Vec<ReviewFileCommentCount>,
    // legacy origin fields...
}

pub struct ReviewFileCommentCount {
    pub relative_path: String,
    pub user_comment_count: u32,
    pub ai_comment_count: u32,
    pub pending_suggestion_count: u32,
}
```

For a project with multiple roots, the summary list contains one active draft
summary per root. Clients bind inline review state by `(project_id, root)`, not
by project alone.

`file_comment_counts` lets the normal git file list show comment badges without
subscribing to the full review diff. Per-file totals are
`user_comment_count + ai_comment_count + pending_suggestion_count`. Human
comments, accepted AI comments, and pending AI suggestions count even when
their anchors are stale; rejected suggestions do not count.
The legacy aggregate `ReviewSummary.user_comment_count` remains the total
accepted comment-record count for summary hubs, including human comments and
accepted AI comments.

### Subscribe

```rust
pub struct ReviewSubscribePayload {
    pub include_diffs: bool, // defaults to true; default serializes as {}
}
```

Older `{}` subscribe payloads receive the full `ReviewBootstrap.review.diffs`.
Clients that only need comment/suggestion state for a lightweight Comments
surface may send `{ "include_diffs": false }`; the server then clears `diffs`
from `ReviewBootstrap` and any later `Snapshot`/`Cleared` review payload sent
to that subscriber. Other review events are unchanged. Lightweight subscribe
does not refresh the root's full-file diff; mutation, submit, AI review, and
full subscribe paths still refresh diffs before they need them.
If the same connection has already subscribed to that review with
`include_diffs = true`, the full mode is sticky for that connection/review so a
later lightweight subscribe cannot downgrade full diff payloads.

### Create

```rust
pub struct ReviewCreatePayload {
    pub selection: ReviewDiffSelection,
}
```

`ReviewCreate` remains for older clients and direct get-or-create flows. New UI
paths should prefer the active id from project bootstrap/summaries. When create
is used, the server resolves it to the selected root, normalizes the active
selection to:

```rust
ReviewDiffSelection::Root {
    root,
    scope: ProjectDiffScope::Unstaged,
    path: None,
}
```

If a draft already exists for `(project_id, root)`, the caller is subscribed to
that review and receives `ReviewBootstrap`. Otherwise the server creates the
root-scoped draft and subscribes the caller. Legacy `AllUncommitted` create is
accepted only when the project has a single root; multi-root callers must send a
root selection.

Older clients may still include `origin_agent_id`; serde ignores that field.
The server no longer requires an origin agent to create or use a review.

### Submit target

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewSubmitTarget {
    ExistingAgent { agent_id: AgentId },
    NewAgent {
        backend_kind: BackendKind,
        cost_hint: Option<SpawnCostHint>,
        custom_agent_id: Option<CustomAgentId>,
        name: Option<String>,
        instructions: Option<String>,
    },
}
```

`ExistingAgent` is valid only when the agent is live and bound to the same
project as the review. `NewAgent` spawns an unrestricted same-project agent
using the project's roots and sends the review bundle as the initial user input
with `MessageOrigin::Review { review_id }`.

### Actions

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewActionPayload {
    AddComment { location: ReviewLocation, body: String },
    UpdateComment { comment_id: ReviewCommentId, body: String },
    DeleteComment { comment_id: ReviewCommentId },
    AcceptSuggestion { suggestion_id: ReviewSuggestionId, edit: Option<String> },
    RejectSuggestion { suggestion_id: ReviewSuggestionId },
    StartAiReview { backend_kind: Option<BackendKind>, cost_hint: Option<SpawnCostHint>, instructions: Option<String> },
    Submit { target: ReviewSubmitTarget },
    ClearComments,
    Cancel, // Legacy/discard path; not used by the always-on inline UX.
}
```

`Submit` validates comments and target, delivers the bundle, then resets the
review on success. `ClearComments` explicitly resets comments/suggestions/AI
state without delivering anything. `Cancel` remains deserializable for
backcompat but new server/UI paths should not depend on it for lifecycle.
`StartAiReview.backend_kind = None` asks the host to use
`HostSettings.default_backend`, or the first enabled backend if no default is
configured; `Some(kind)` remains an explicit override.

### Anchor status

Comments and AI suggestions carry explicit anchor metadata:

```rust
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReviewAnchorStatus {
    Current,
    Stale { reason: String },
}

pub struct ReviewComment {
    pub anchor_status: ReviewAnchorStatus,
    // ...
}

pub struct ReviewSuggestedComment {
    pub anchor_status: ReviewAnchorStatus,
    // ...
}
```

Missing `anchor_status` fields deserialize as `Current` for migration.

### Events

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewEventPayload {
    Snapshot { review: Review },
    CommentUpsert { comment: ReviewComment },
    CommentDelete { comment_id: ReviewCommentId },
    SuggestionUpsert { suggestion: ReviewSuggestedComment },
    AiReviewerChanged { state: ReviewAiReviewerState },
    StatusChanged { status: ReviewStatus },
    Cleared { review: Review },
    Error { error: ReviewErrorPayload },
}
```

`Cleared` is emitted after successful submit, explicit clear, and clean
unstaged-root reset. Clients should replace their local review projection with
the included review.

### Diff files

Review diffs use the normal project diff payload with
`ProjectDiffScope::Unstaged` and `DiffContextMode::FullFile`.
`ProjectGitDiffFile.is_binary` marks binary additions/modifications. Binary
files carry no hunks, so line and hunk anchors are impossible, but file-level
`ReviewAnchor::File` comments are valid.

---

## 3. Server behavior

### Registry

`ReviewRegistry` owns review actors and enforces get-or-create singleton
semantics per `(project_id, root)`. Project summaries first ensure an implicit
active draft exists for every current project root, then return exactly one
active draft summary per root.

When legacy records are loaded:

- draft root records are normalized to unstaged, full-root active selections;
- multiple drafts for the same root are hidden behind the latest active one;
- project-only `AllUncommitted` drafts remain safe/subscribable by id but are
  not exposed as active summaries;
- submitted/consumed/cancelled records deserialize safely and are not
  redelivered by the inline flow.

### Diff refresh and stale anchors

Review actors refresh `diffs` from the root's unstaged diff on subscribe and
before mutating/submitting. After each refresh the actor checks every comment and
suggestion location against the refreshed diff:

- valid location => `anchor_status = Current`
- invalid location => `anchor_status = Stale { reason }`

The server never changes `ReviewLocation` to make an anchor fit. Submitting with
any stale/invalid accepted comment fails with `InvalidLocation`.

Untracked binary files are included in refreshed unstaged diffs as
`is_binary = true` with empty hunks instead of failing review creation or
refresh. Because binary and metadata-only changes can have no hunks, clean-reset
logic treats the unstaged diff as clean only when refreshed diffs contain no
files.

### Clean reset

A refresh that observes no changed files for the review root clears that root's
active review. Project-stream git status refreshes also notify the registry for
roots whose unstaged state is clean (`unstaged == None` and not untracked), so
subscribed clients reset even if no review action is in flight. Staged-only
changes therefore leave the active unstaged review clear.

### Submit

`Submit { target }` flow:

1. Require draft review, at least one accepted comment, and no running AI
   reviewer.
2. Refresh the root's unstaged diff and mark stale anchors.
3. Reject if any accepted comment is stale/invalid.
4. Build `ReviewFeedbackBundle` and render the deterministic markdown message.
5. Deliver to the chosen target:
   - existing agent: live, same project, receives `AgentInput::SendMessage`
   - new agent: spawned in the same project with the bundle as initial input
6. On success, clear comments, suggestions, and AI state; keep `status = Draft`;
   emit `Cleared` and update project review summaries.

Delivery failures leave the draft content intact so the user can choose another
target or retry.

---

## 4. Migration notes

- `ReviewSummary.root` is required on new summary payloads and defaults to an
  empty `ProjectRootPath` only for legacy JSON deserialization.
- `ReviewSummary.file_comment_counts` defaults to an empty list for legacy
  summary JSON.
- `ReviewCreatePayload.origin_agent_id` is removed from the protocol payload.
  Older JSON with that extra field is ignored.
- `ReviewSubscribePayload.include_diffs` defaults to `true`, so legacy
  subscribe payloads keep receiving full diffs.
- `ReviewActionPayload::StartAiReview.backend_kind` is optional. Missing values
  use the host default backend, then the first enabled backend.
- `ReviewActionPayload::Submit` requires an explicit `target`.
- Active reviews normalize to `ReviewDiffSelection::Root { scope: Unstaged,
  path: None }`.
- `ReviewComment.anchor_status` and `ReviewSuggestedComment.anchor_status`
  default to `Current` when absent.
- Persisted legacy `Submitted`/`Consumed`/`Cancelled` reviews deserialize safely
  but are not redelivered by the new review flow.
- `MessageOrigin::Review` remains unchanged and is required on all delivered
  feedback bundle messages.

---

## 5. UI contract

Frontend/mobile should:

- Treat `ProjectBootstrap.review_summaries` and
  `ProjectEventPayload::ReviewListChanged` as the source of active review ids.
- Bind active review state by `(project_id, summary.root)`.
- Render per-file comment badges in the normal git diff/file list from
  `ReviewSummary.file_comment_counts` when available.
- Subscribe to `/review/<summary.id>` when the diff surface for that root needs
  full review state; do not create a review just to make one exist.
- For a lightweight Comments surface, subscribe with `include_diffs = false`
  and render small snippets around comments/suggestions from already-loaded
  project diff data instead of treating the all-root full diff as the primary
  entrypoint.
- Render `anchor_status = Stale` distinctly and avoid silently changing the
  anchor location.
- Use `ClearComments` for an explicit user reset.
- Use `Submit { target: ExistingAgent { agent_id } }` or
  `Submit { target: NewAgent { backend_kind, cost_hint, name } }`.
- Replace local review state with the review included in `Cleared`.
