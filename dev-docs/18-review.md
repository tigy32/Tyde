# Review

Spec for Tyde's inline Review feature.

Reviews are now a **project-scoped singleton inline layer** over the current
uncommitted git diff (`git diff HEAD`). The primary UX is the existing project
diff view: comments, AI suggestions, stale-anchor badges, and submit controls
render inline with changed files. There is no standalone Review tab as the
primary experience.

Audience: implementation agents and future maintainers.

---

## 1. Current model

- One active review exists per project.
- The review is always `Draft` for new flows. Submitting feedback does not move
  the review into a durable submitted/consumed queue; after successful delivery
  the server clears comments, suggestions, and AI reviewer state and keeps the
  same review ready for the next uncommitted diff.
- The review tracks the project's current uncommitted diff. Diff refreshes never
  silently re-anchor comments. If a stored anchor no longer matches the current
  diff, the server marks it stale and leaves the original location unchanged.
- When the working tree becomes clean, the singleton review resets: comments,
  suggestions, AI state, and diff payloads are cleared.
- Feedback can be submitted either to an existing open same-project agent or to
  a newly spawned same-project agent.

Legacy `Submitted`, `Consumed`, and origin-session records may still be present
in persisted stores. They must deserialize safely, but new review submissions do
not use the old durable origin-session redelivery path.

---

## 2. Protocol contract

### Streams

```text
/project/<project_id>   ReviewCreate
/review/<review_id>     ReviewSubscribe, ReviewAction, ReviewEvent
```

`ReviewCreate` is sent on the project stream. The project id comes from that
stream path, not from the payload.

### Create

```rust
pub struct ReviewCreatePayload {
    pub selection: ReviewDiffSelection,
}
```

`ReviewCreate` is get-or-create. If the project already has a draft singleton,
the server subscribes the caller to that review and sends `ReviewBootstrap` for
it. Otherwise the server creates the singleton, reads the current uncommitted
full-file diff, persists the record, and sends `ReviewBootstrap`.

Older clients may still include `origin_agent_id`; serde ignores that field.
The server no longer requires an origin agent to create a review.

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
    StartAiReview { backend_kind: BackendKind, cost_hint: Option<SpawnCostHint>, instructions: Option<String> },
    Submit { target: ReviewSubmitTarget },
    ClearComments,
    Cancel, // Legacy/discard path; not the primary inline UX.
}
```

`Submit` validates comments and target, delivers the bundle, then resets the
review on success. `ClearComments` explicitly resets comments/suggestions/AI
state without delivering anything.

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

`Cleared` is emitted after successful submit, explicit clear, and clean working
tree reset. Clients should replace their local review projection with the
included review.

---

## 3. Server behavior

### Registry

`ReviewRegistry` owns review actors and implements get-or-create singleton
semantics per project. The active singleton is the latest non-cancelled draft
for that project. Legacy submitted/consumed records may be loaded for safe
migration, but they are hidden from project summaries and are not redelivered.

### Diff refresh and stale anchors

Review actors refresh `diffs` from `git diff HEAD` on subscribe and before
mutating/submitting. After each refresh the actor checks every comment and
suggestion location against the refreshed diff:

- valid location => `anchor_status = Current`
- invalid location => `anchor_status = Stale { reason }`

The server never changes `ReviewLocation` to make an anchor fit. Submitting with
any stale/invalid accepted comment fails with `InvalidLocation`.

### Clean reset

A refresh that observes no changed files clears the review. Project-stream git
status refreshes also notify the registry when all project roots are clean so
subscribed clients reset even if no review action is in flight.

### Submit

`Submit { target }` flow:

1. Require draft review, at least one accepted comment, and no running AI
   reviewer.
2. Refresh the uncommitted diff and mark stale anchors.
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

- `ReviewCreatePayload.origin_agent_id` is removed from the protocol payload.
  Older JSON with that extra field is ignored.
- `ReviewActionPayload::Submit` now requires an explicit `target`.
- `ReviewComment.anchor_status` and `ReviewSuggestedComment.anchor_status`
  default to `Current` when absent.
- Persisted legacy `Submitted`/`Consumed` reviews deserialize safely but are not
  redelivered by the new review flow.
- `MessageOrigin::Review` remains unchanged and is required on all delivered
  feedback bundle messages.

---

## 5. UI contract

Frontend/mobile should:

- Treat `ProjectBootstrap.review_summaries` and
  `ProjectEventPayload::ReviewListChanged` as the source of the current project
  singleton id.
- Call `ReviewCreate { selection }` from the project diff surface; it will
  attach to an existing singleton when one exists.
- Render `anchor_status = Stale` distinctly and avoid silently changing the
  anchor location.
- Use `ClearComments` for an explicit user reset.
- Use `Submit { target: ExistingAgent { agent_id } }` or
  `Submit { target: NewAgent { backend_kind, cost_hint, name } }`.
- Replace local review state with the review included in `Cleared`.
