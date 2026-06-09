# Review

Spec for Tyde's inline Review feature.

Reviews are an **always-on, workspace-scoped inline layer** over a project's
current unstaged git diffs. In this document, **workspace** means the
`Project`, keyed by `ProjectId`, spanning every path in `Project.roots`.
There is exactly one active draft review per project, even when the project has
one root. The primary UX is still the project diff surface: comments, AI
suggestions, stale-anchor badges, and submit controls render inline with the
changed files from any root in the project.

Audience: implementation agents and future maintainers.

---

## 1. Current model

- Exactly one active draft review exists for each `ProjectId`.
- The active review scope is `ReviewDiffSelection::Workspace { scope:
  ProjectDiffScope::Unstaged }`.
- Active reviews are implicit. Project bootstrap and review summary updates
  surface the active id; clients should not send `ReviewCreate` just to make a
  review appear in the UI.
- `Review.diffs` contains one `ProjectGitDiffPayload` per project root that can
  be read as a git repository. Each payload is normalized to
  `ProjectDiffScope::Unstaged` and `DiffContextMode::FullFile`.
- Submitting feedback does not move the active review into a durable
  submitted/consumed queue. After successful delivery the server clears
  comments, suggestions, and AI reviewer state and keeps the same draft review
  ready for the next unstaged workspace diff.
- Diff refreshes never silently re-anchor comments. If a stored anchor no
  longer matches the current unstaged diff for its root, the server marks it
  stale and leaves the original `ReviewLocation` unchanged.
- Clean reset is workspace-wide: the active review clears only when **all**
  project roots have clean unstaged state. If one root becomes clean while
  another root is still dirty, comments in the clean root become stale through
  normal anchor-status refresh.
- Feedback can be submitted either to an existing open same-project agent or to
  a newly spawned same-project agent.

Legacy root-scoped draft records may still exist in persisted stores and may
remain subscribable by id. They are not emitted as active summaries and are not
merged into the workspace review. This is a start-fresh model: there is no
migration or comment-merging path from old per-root drafts into the active
workspace draft.

---

## 2. Protocol contract

### Streams

```text
/project/<project_id>   ReviewCreate (legacy/direct get-or-create),
                       ProjectBootstrap,
                       ProjectEvent::ReviewListChanged
/review/<review_id>     ReviewSubscribe, ReviewAction, ReviewEvent
```

### Project bootstrap and summaries

`ProjectBootstrapPayload.review_summaries` and
`ProjectEventPayload::ReviewListChanged` are the source of active review ids.
For each project they contain exactly one active draft summary with workspace
scope:

```rust
pub struct ReviewSummary {
    pub id: ReviewId,
    pub scope: ReviewSummaryScope, // Workspace for active summaries
    pub status: ReviewStatus,     // Draft for active summaries
    pub user_comment_count: u32,
    pub pending_suggestion_count: u32,
    pub file_comment_counts: Vec<ReviewFileCommentCount>,
    // legacy origin fields...
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewSummaryScope {
    Workspace,
    Root { root: ProjectRootPath }, // legacy/direct reviews only
}

pub struct ReviewFileCommentCount {
    pub root: ProjectRootPath,
    pub relative_path: String,
    pub user_comment_count: u32,
    pub ai_comment_count: u32,
    pub pending_suggestion_count: u32,
}
```

For a project with multiple roots, the summary list still contains one active
summary, not one summary per root. Clients bind active inline review state by
`(project_id, ReviewSummaryScope::Workspace)`.

`file_comment_counts` lets the normal git file list show comment badges without
subscribing to the full review diff. The `root` field is required because the
same relative path can exist in multiple project roots. Per-file totals are
`user_comment_count + ai_comment_count + pending_suggestion_count`. Human
comments, accepted AI comments, and pending AI suggestions count even when
their anchors are stale; rejected suggestions do not count. The legacy
aggregate `ReviewSummary.user_comment_count` remains the total accepted
comment-record count for summary hubs, including human comments and accepted AI
comments.

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
does not refresh the full workspace diff; mutation, submit, AI review, and full
subscribe paths still refresh diffs before they need them. If the same
connection has already subscribed to that review with `include_diffs = true`,
the full mode is sticky for that connection/review so a later lightweight
subscribe cannot downgrade full diff payloads.

### Create

```rust
pub struct ReviewCreatePayload {
    pub selection: ReviewDiffSelection,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewDiffSelection {
    AllUncommitted, // legacy; active create normalizes to Workspace/Unstaged
    Workspace { scope: ProjectDiffScope },
    Root { root: ProjectRootPath, scope: ProjectDiffScope, path: Option<String> },
}
```

New UI paths should prefer the active id from project bootstrap/summaries.
When `ReviewCreate` is used with `Workspace` or legacy `AllUncommitted`, the
server normalizes it to:

```rust
ReviewDiffSelection::Workspace {
    scope: ProjectDiffScope::Unstaged,
}
```

and returns the one active workspace draft for the project, creating it if
needed.

`ReviewCreate::Root` remains a legacy/direct path for callers that already know
a review id or need a root-scoped draft. Root-scoped drafts are normalized to
unstaged full-root selection, remain subscribable by id, and are not emitted as
active project summaries.

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
using `project.roots` and sends the review bundle as the initial user input
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

`Submit` validates comments and target, delivers one feedback bundle for all
accepted comments across roots, then resets the review on success.
`ClearComments` explicitly resets comments/suggestions/AI state without
delivering anything. `Cancel` remains deserializable for backcompat but new
server/UI paths should not depend on it for lifecycle.
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
    pub location: ReviewLocation, // includes root
    pub anchor_status: ReviewAnchorStatus,
    // ...
}

pub struct ReviewSuggestedComment {
    pub location: ReviewLocation, // includes root
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
workspace reset. Clients should replace their local review projection with the
included review.

### Diff files

Review diffs use the normal project diff payload with
`ProjectDiffScope::Unstaged` and `DiffContextMode::FullFile`.
`Review.diffs` may contain multiple `ProjectGitDiffPayload` values, one for
each git root in the project. `ProjectGitDiffFile.is_binary` marks binary
additions/modifications. Binary files carry no hunks, so line and hunk anchors
are impossible, but file-level `ReviewAnchor::File` comments are valid.

---

## 3. Server behavior

### Registry

`ReviewRegistry` owns review actors and enforces active singleton semantics per
`ProjectId`. Project summaries first ensure the implicit active workspace draft
exists for the project, then return exactly one workspace summary.

When legacy records are loaded:

- draft workspace records are normalized to unstaged workspace selection;
- draft root records remain safe/subscribable by id but are hidden from active
  summaries;
- legacy `AllUncommitted` drafts remain safe/subscribable by id but are not
  treated as active workspace summaries;
- submitted/consumed/cancelled records deserialize safely and are not
  redelivered by the inline flow.

There is intentionally no migration or merge logic that moves comments from old
per-root drafts into the active workspace review.

### Diff refresh and stale anchors

Review actors refresh `diffs` from every project root's unstaged diff on full
subscribe and before mutating/submitting/starting AI review. After each refresh
the actor checks every comment and suggestion location against the refreshed
workspace diff:

- valid location => `anchor_status = Current`
- invalid location => `anchor_status = Stale { reason }`

The server never changes `ReviewLocation` to make an anchor fit. Submitting with
any stale/invalid accepted comment fails with `InvalidLocation`.

Untracked binary files are included in refreshed unstaged diffs as
`is_binary = true` with empty hunks instead of failing review creation or
refresh. Because binary and metadata-only changes can have no hunks, clean-reset
logic treats the workspace diff as clean only when every refreshed diff contains
no files.

### Clean reset

A refresh that observes no changed files in any root clears the active workspace
review. Project-stream git status refreshes also notify the registry only when
all project roots have clean unstaged state (`unstaged == None` and not
untracked). Staged-only changes therefore leave the active unstaged review
clear.

If one root becomes clean while another root remains dirty, the review is not
cleared. Comments in the clean root remain in the review and are marked stale by
anchor-status refresh because their diff file is no longer present.

### AI review

`StartAiReview` spawns one agent named `AI Review` for the workspace review.
The spawn request uses:

- `project_id = Some(review.project_id)`
- `workspace_roots = project.roots`
- read-only access mode
- one reviewer prompt containing all refreshed review diffs
- the review-feedback MCP server so the reviewer can call
  `propose_review_comment`

The reviewer proposes typed `ReviewLocation` values. The `root` in each
location must be one of the project root paths present in the review diff.

### Submit

`Submit { target }` flow:

1. Require draft review, at least one accepted comment, and no running AI
   reviewer.
2. Refresh all roots' unstaged diffs and mark stale anchors.
3. Reject if any accepted comment is stale/invalid.
4. Build one `ReviewFeedbackBundle` containing comments across all roots and
   render the deterministic markdown message.
5. Deliver to the chosen target:
   - existing agent: live, same project, receives `AgentInput::SendMessage`
   - new agent: spawned in the same project with `project.roots` and the bundle
     as initial input
6. On success, clear comments, suggestions, and AI state; keep `status = Draft`;
   emit `Cleared` and update project review summaries.

Delivery failures leave the draft content intact so the user can choose another
target or retry.

---

## 4. Migration notes

- `ReviewSummary.root` is replaced by `ReviewSummary.scope`.
- Active summaries use `ReviewSummaryScope::Workspace`.
- `ReviewSummaryScope::Root { root }` exists only for legacy/direct summaries,
  not for the active project surface.
- `ReviewFileCommentCount.root` is required on new payloads and defaults to an
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
- Active reviews normalize to `ReviewDiffSelection::Workspace { scope:
  Unstaged }`.
- Old per-root drafts are start-fresh legacy data: they may be subscribed by id,
  but they are not emitted as active summaries and are not merged.

---

## 5. Implementation checklist

- Bind active review state by `(project_id, ReviewSummaryScope::Workspace)`.
- Read `ReviewFileCommentCount.root` and `relative_path` together for per-file
  badges.
- Do not create per-root active reviews from bootstrap or summary handling.
- Do not clear the workspace review just because one root became clean.
- Do not add migration or comment-merging logic for old per-root drafts.
