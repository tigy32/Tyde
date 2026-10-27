# Host Settings

Server-owned host settings delivered over the `/host/*` stream. Builds on
`01-philosophy.md`, `02-protocol.md`, and `04-host-registry.md`.

Managed mobile access has an additional product/service boundary:
`30-mobile-managed-access.md` owns the Tyggs Pass, `tycode.dev`, and Cloudflare TURN
contract. Host settings may expose server-owned mobile controls, but they must
not store Tyggs account data, pass proofs, billing state, or relay
fallbacks that bypass `tycode.dev`.

---

## Problem

The rewrite had a settings overlay, but backend configuration was still local
frontend state:

- The frontend chose a default backend with a local signal.
- The backend settings page rendered static cards.
- New-chat behavior depended on frontend-local state rather than host state.
- There was no protocol event for current host settings and no persistence in
  `tyde-server`.

That violated the design rules:

- The server must own behavior; the UI only renders state.
- State flows through events, not hidden caches.
- Everything must use protocol types end-to-end.

The fix is a typed host settings model in `settings-model`, persisted and owned
by `tyde-server`, replayed on connect, and updated through compare-and-swap
host-stream operations.

---

## Scope

The persisted host-settings document covers:

- `enabled_backends`
- `default_backend`
- `delegation_launch_profile_order`, an advisory ordered list whose missing
  launch-profile IDs are retained across backend/catalog availability changes
- independent Nova conversation and Amazon Transcribe dictation enablement,
  the host-only AWS profile, explicit per-service regions, dictation language,
  and exact Nova model

These are host-level settings, not per-session settings or frontend
preferences. Backend-native settings are also exposed on the host stream, but
remain a separate backend-owned domain and are never written into the host
settings store.

Out of scope for this slice:

- MCP settings
- agent definitions
- notifications
- advanced settings
- host registry / multi-host selection

---

## Protocol Model

Host settings are strongly typed in `settings-model/src/lib.rs`; protocol owns
only the generic transport payloads.

### Types

```rust
pub struct HostSettings {
    pub enabled_backends: Vec<BackendKind>,
    pub default_backend: Option<BackendKind>,
    pub voice: VoiceSettings,
}
```

`BackendKind` is already an enum, so no stringly-typed backend identifiers are
introduced here.

### Input Events

Clients update settings on `/host/<uuid>` with `FrameKind::SettingsWrite`.
Each `SettingOp::Replace` or `SettingOp::Remove` carries an RFC 6901 path and a
mandatory value, version-token, or absent compare-and-swap expectation. A
write is atomic: overlapping operations or any failed expectation reject the
whole batch.

Profile create/delete and similar backend lifecycle operations use
`FrameKind::InvokeSettingsAction`, not document writes.

### Output Event

The server emits:

```rust
FrameKind::HostSettings
```

Payload:

```rust
pub struct HostSettingsPayload<HostSettings> {
    pub settings: HostSettings,
    pub etag: String,
    pub configured_secrets: Vec<ConfiguredSecret>,
}
```

`HostBootstrap` supplies initial state; `HostSettings` is the authoritative
live-update shape.

---

## Semantics

### Bootstrap and snapshots

`HostBootstrap` carries the initial redacted settings document, its etag, the
build-static JSON Schema, and revision tokens for configured write-only paths.
An applied write fans out `HostSettings` with the new full redacted snapshot,
etag, and configured-secret tokens.

### `settings_write`

Under one settings-apply lock, the host checks path knowledge, secret rules,
overlap, and every CAS expectation; applies the batch to a candidate document;
deserializes and validates the typed model; commits the host store; propagates
backend effects; and publishes the snapshot. The requester alone receives a
correlated `SettingsWriteResult`. Semantic rejection is a typed result, not a
`CommandError`.

The client does not optimistically replace server-owned settings. It waits for
the authoritative fanout.

## Invariants

The server enforces:

- `enabled_backends` may be empty
- if `default_backend` is set, it must be present in `enabled_backends`

Normalization rules:

- backend lists are canonicalized in fixed enum order
- duplicate/unknown values are not preserved
- if the store file is missing, the server returns an empty settings snapshot
- invalid persisted settings fail load instead of being silently repaired

There is no protocol-level backend default in this slice.

If `enabled_backends` is changed such that the current `default_backend` is no
longer enabled, the host clears `default_backend` to `null` and emits that
updated snapshot.

---

## Persistence

Host settings are persisted in a dedicated server-owned store:

- default path: `~/.tyde/settings.json`
- override: `TYDE_SETTINGS_STORE_PATH`

This lives beside the existing session and project stores, but remains a
separate file because it is a separate domain model.

Current store shape:

```json
{
  "settings": {
    "enabled_backends": [],
    "default_backend": null
  }
}
```

The store is read on demand and replaced atomically on write, matching the
existing store pattern used by sessions and projects.

---

## Server Ownership

The host actor owns the settings lifecycle.

### Registration Replay

When a host stream is registered, the host replays:

1. `host_settings`
2. existing projects
3. existing agents

That order is intentional:

- settings are general host state
- projects are host-owned inventory
- agents are runtime instances

This keeps startup state replay aligned with the "events in, events out" model.

### Mutation Fanout

When settings change, the host fans out the latest `host_settings` snapshot to
all connected host subscribers.

No special-case frontend refresh logic is required beyond normal event handling.

---

## Frontend Data Flow

The frontend stores host-keyed settings, schemas, and configured-secret tokens.
They are seeded by `HostBootstrap`; settings and secret tokens are replaced by
each `HostSettings` fanout.

### Settings Overlay

The settings overlay renders complex editors from the typed model and primitive
rows from the server-published schema. Edits emit path-scoped `SettingsWrite`
operations against the selected host.

### Runtime Behavior

`spawn_new_chat` reads `host_settings.default_backend` when creating a new
agent. If no default is configured, the frontend does not invent one.

That is the intended direction: settings should affect runtime via server-owned
state, not isolated frontend preferences.

---

## Why This Design

This design follows the philosophy document directly:

- One source of truth: `HostSettings` lives in `settings-model`.
- Server owns behavior: persistence, validation, and fanout all happen in
  `tyde-server`.
- UI only renders state: the frontend does not own backend settings anymore.
- Bootstrap supplies initial state; `host_settings` supplies authoritative
  updates.
- The protocol transports generic JSON operations while server and clients share
  the typed settings model.

---

## Next Steps

Natural extensions add typed fields to `HostSettings`, expose primitive rows
through its schema annotations, and keep complex editors in Rust. No new wire
enum is needed for ordinary fields. Settings remain server-owned, and schema
validation on the client is advisory; the server is authoritative.

## Codex native settings

The Codex page uses grouped `BackendNativeSettingsSnapshot` documents. It
reads the installed app-server's `config/read` user layer and effective
configuration, plus `model/list` for model and speed choices. Only an allowlist
of non-secret fields enters the client snapshot; no credentials, MCP
configuration, or raw configuration layers are sent.

Saves diff the editable values, then use `config/batchWrite` with the user
layer's expected version and server-resolved file path. Null removes a user
override. Unknown fields and invalid edited values are rejected before the
write; the CLI also validates the native configuration. Unrelated fields and
comments remain owned by Codex. A save refreshes the snapshot even on conflict.

`reloadUserConfig` is false: saves never reconfigure running threads. New
threads inherit defaults, while startup-level controls apply to new Codex
processes, including processes used to resume threads. Session model/effort/
speed overrides remain authoritative. Tyde's own agent-control depth settings
are independent of native Codex subagent concurrency.

Coverage: the native UI flow exercises section switching, editing, save
locking and reset. The opt-in `real_codex_global_settings` conformance case
runs the real host and Codex CLI in an isolated `CODEX_HOME`, checking native
read/write/reset, stale-version rejection, invalid-value rejection and
preservation. It creates no model conversations.


## Self-improvement suggestion dock

Settings is the only place that enables/disables learning, selects its scope,
mode and analyzer. Suggestions live in the normal Self-improvement dock tab,
not Settings or a chat tray. Its host and rule scope are explicit. The dock uses
the existing client layout for placement, resizing and visibility; its pending
badge never changes visibility or focus. Only this client's successful,
correlated Settings enable write opens it once. Bootstrap and other clients'
settings changes do not open it.

Pending and History render the host's `SelfImprovementState`. Accept, Edit,
Dismiss, Undo and Restore send typed `SelfImprovementAction`; transport completion
never manufactures state. The diff preserves the server's before and proposed
title/text; rationale is an optional disclosure, not raw feedback. Before means
the proposal's recorded baseline, not a fresh canonical lookup. Existing-rule
links open the owning host's Steering/Review settings. The sidebar gear carries
its selected host and scope to Settings without navigating the active chat.

Proposal rows keep one keyed lifetime across tab, scope, chat and host navigation
so unsaved form drafts survive. Cancel closes an editor without deleting its
draft. Edit carries the whole expected operation as an actor-checked CAS; a
stale proposal is rejected without changing the durable journal. Each edit has
a typed identity, persisted as `last_edit_id` only after successful save. Only
that client's accepted edit with the submitted content closes its unchanged
local draft. Coincident text from another client is not an acknowledgment.
A newer local draft stays open. Latest proposal disclosure and Keep my edit
allow explicit adoption of a changed baseline while retaining local text.

Restore returns Dismissed/Undone to Pending only if the current canonical rule
still matches the recorded baseline. It preserves proposal identity and content,
uses the current per-rule revision and requires approval, even in Automatic mode.
Unapplied Conflicts can be dismissed; an applied Undo conflict cannot be hidden
as a dismissal. Apply/Undo retain their canonical revision and text protections.
Proposal mutations are staged and persisted before publication; failed Edit,
Restore or Dismiss never creates success-shaped in-memory state.

Protocol92 adds required Edit expectations/identity, Restore and typed proposal
error context. Older journals have no accepted new-contract edit identity. Action
errors are scoped by `CommandErrorContext::SelfImprovementChange`, not inferred
from host-error text. The existing inline-notice store displays them on the owning
card, and edit rejections only alongside the corresponding submitted edit.
Analyzer/input failures retain their existing host/scope ownership. No duplicate
proposal cache, optimistic history, analyzer/provider or security-policy changes.

The existing wasm settings/audit flow now exercises the single dock instead of
two duplicate chat/Settings presentations. Old rich-markdown/source-feedback
assertions described that removed UI; the replacement checks exact escaped
diff text, absence of fetched images/raw feedback, draft identity and confirmed
lifecycle transitions, stale-baseline adoption, correlated proposal errors and
Restore. The real-server protocol flow checks Restore/Apply/Undo cycles,
two-client stale edits and idempotent replay, real journal-write failure,
canonical conflict dismissal, protected human text and reconnect durability.

Narrow docking follows the workspace container's width, not a second mobile
visibility preference. In a narrow workspace, opening this dock hides the other
panes through the same persisted layout, including activation of a saved
interface mode without a resize. Explicitly opening Agents or Terminal
hides this dock instead; all components and proposal drafts retain their lifetime.
Real native-window QA found that the welcome panel's
intrinsic minimum pushed the composer below the compressed center. Only while
this dock is open in a narrow workspace, the welcome content may scroll inside
its allotted space; the composer remains inside the center. The existing dock
DOM flow checks this non-overlap contract across all three chat presentations.
This is not a physical iPhone/PWA viewport certification.

The mode-switch regression caught a lazy reactive memo that was only tracked,
never evaluated: selecting saved Vibe at unchanged390px left both panes visible.
The effect now reads the memo before reconciliation. The same DOM visibility and
non-overlap assertions remain intact; activation diagnostics record mode/width.

Explicit dock hiding, tab-menu removal and compact activation of another pane
cancel the same pending enable-open intent. A delayed successful Settings write
cannot reinsert a removed suggestion tab or undo that later layout choice.
Removing active Self-improvement selects a remaining listed tab. Existing
non-Self-improvement tab-hiding semantics remain unchanged.
The existing DOM audit flow exercises removal and compact hiding before delayed
successful write events. No second visibility or proposal-state model is added.

The removal DOM oracle targets the actual right tab strip, not every button in
its component: the open menu deliberately retains an unchecked Self-improvement
checkbox after removal. A broad button query rejected that correct output. The
flow checks both absent tab and unchecked menu before the delayed result, then
keeps the absent-tab assertion afterward. Downstream portal failures from the
original panic were not separate product failures; no assertions were relaxed.

The existing History flow caught an over-broad active-tab removal change: it
intentionally retains the active History view after hiding its tab. That behavior
is restored, with the new removal handling limited to Self-improvement. The
proposal-focus flow restores its composer-focus precondition after the added menu
interaction (Escape intentionally restores menu-anchor focus); the original
post-notify focus assertion is unchanged.

Settings Open suggestions requires the existing host and scope selection. The
no-project/unselected-scope DOM case checks that the action is unavailable until
a scope is chosen, then actionable. If the selection disappears during the action,
an explicit Settings error replaces the old silent no-op. This does not restrict
any server operation or add a sidebar feature toggle.

Final visual triage: an empty-title edit now identifies the required title at
server validation, without changing accepted inputs or analyzer behavior. Typed
proposal Conflict uses the existing bordered error presentation (not a duplicate
notice or inferred command failure); its Dismiss remains a normal button.
The existing real-server flow verifies title rejection ownership, correction and
unchanged durable proposal; the DOM flow verifies Conflict alert prominence.
Normal tab removal removes Self-improvement but can leave the generic empty dock
visible. This differs from closing the dock; neither action disables learning.
Narrow scope/header vertical density remains a nonblocking layout-polish limit.
