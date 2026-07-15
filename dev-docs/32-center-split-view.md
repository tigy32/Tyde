# Desktop Center Split View

This document specifies the first desktop center-workspace split view. It
builds on:

- `01-philosophy.md` for server authority and typed state
- `06-projects.md` for project identity and project switching
- `18-workbenches.md` for explicit resource ownership
- `diff-view-modes.md` for local presentation preferences

The feature is desktop-only. Mobile and PWA navigation and layout remain
unchanged. It requires no protocol or server change.

---

## 1. Scope and topology

The center workspace starts in the existing single-pane layout. A user may
create one horizontal, side-by-side split, producing exactly two panes. The
initial release supports file beside file and file beside agent chat; each pane
has independent focus, scrolling, tab selection, and content lifecycle.

The bound is explicit in the state model:

```rust
pub enum PaneId {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitRatio(f64);

impl SplitRatio {
    pub const MIN: f64 = 0.2;
    pub const MAX: f64 = 0.8;

    pub fn new(value: f64) -> Self {
        Self(value.clamp(Self::MIN, Self::MAX))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

pub enum CenterLayout {
    Single(PaneState),
    Split {
        primary: PaneState,
        secondary: PaneState,
        focused: PaneId,
        ratio: SplitRatio,
    },
}
```

`Single` has no redundant focus field. `Split` always contains both panes and
always identifies an existing focused pane. Creating a split also opens,
duplicates, or moves a resource into the destination pane; v1 has no empty
drop-target pane. `SplitRatio` is constructed only through its clamping
constructor, so a primary-pane share outside 20% through 80% is not
representable. Pointer, keyboard, restored, and programmatic ratios all pass
through that constructor.

Closing the last tab in the secondary pane returns to `Single(primary)`.
Closing the last tab in the primary pane promotes the secondary pane to
`Single`. Moving the last tab out of a pane also collapses the split, with the
moved tab in the surviving pane. Closing the last tab in `Single` retains the
existing behavior of recreating the non-closeable Home tab; Home never seeds or
duplicates into a second pane.

Vertical splits, nested grids, and more than two panes are not partially
modeled in v1. Supporting them later requires a deliberate new layout variant.

---

## 2. Resource occurrence rules

The occurrence invariant is:

> A loaded file may have at most one occurrence in each pane. Every other
> resource may have at most one occurrence across the entire center layout.

One loaded file may therefore have two tabs total, one in `Primary` and one in
`Secondary`. The tabs have different `TabId` values and independent
`tab_scroll_state`, but both render the same authoritative
`open_files[FileResourceKey]` record.

Creating the second occurrence is synchronous. It sends no `ProjectReadFile`,
sends no `CodeIntelSubscribeFile`, and creates no pending file-open intent. The
file contents and code-intelligence subscription remain shared by key. A file
that is still loading cannot be duplicated.

Chats are never duplicated, whether live, draft, or pending-team-member:

- draft-to-agent upgrade currently identifies the one draft by content;
  duplication would bind the spawned agent arbitrarily;
- pending-team-member upgrade likewise requires one matching tab; and
- a live chat must have one unambiguous composer owner and exactly one mounted
  `ChatInput`.

Diffs, comments, workflows, Agent Monitor, and Home are also never duplicated.
No gesture offers duplication for those resources.

Every open path first checks the target pane. A second occurrence in the same
pane is unreachable. Ordinary open never duplicates any resource: it activates
an occurrence in the focused pane, reveals an occurrence in the other pane, or
opens a new tab in the focused pane. File duplication occurs only through an
explicit eligible `Split Right` or `Open to the Side` action.

---

## 3. Explicit file identity

Files use a fully qualified identity:

```rust
pub struct FileResourceKey {
    pub host_id: String,
    pub project_id: ProjectId,
    pub path: ProjectPath,
}
```

`TabContent::File` carries this key, and open-file state is keyed by it. A file
view receives the key directly; it must not infer host or project from the
currently active project. File content, refreshes, code-intelligence
subscriptions, and teardown all use the same key.

Two projects or hosts may contain the same relative path without sharing a
record or subscription. Closing a file releases only its exact key and only
after its final occurrence closes. This is local identity for projecting
authoritative server events, not a frontend mirror of server-owned file state.

`BackingResource` identifies teardown-bearing resources without creating a
second general resource hierarchy:

```rust
pub enum BackingResource {
    File(FileResourceKey),
    Diff(DiffKey),
}
```

---

## 4. Occurrence-aware close and bulk teardown

Closing one of two file occurrences must leave the shared contents and
code-intelligence subscription intact. Closing the final occurrence releases
the backing resource and unsubscribes exactly once.

Single-tab close determines whether any tab other than the closing `TabId`
still references the same `BackingResource`. Teardown happens only when no such
survivor exists.

Bulk close must not perform that check repeatedly against the unmodified
pre-close layout. Instead it computes two sets against the intended post-close
projection:

1. `survivors`: backing resources referenced by tabs that will remain.
2. `released`: backing resources referenced by doomed tabs but absent from
   `survivors`.

Each member of `released` is torn down exactly once. Each doomed `TabId` then
loses its LRU and scroll state before the tabs are removed. This prevents both
over-teardown when one occurrence survives and under-teardown when a bulk
operation closes both occurrences.

The same occurrence-aware path governs close tab, close other tabs, close tabs
to the right, close all tabs, close pane, and agent-tab cleanup. With one
occurrence, behavior is unchanged from the single-pane implementation.

---

## 5. Tab-targeted file navigation

File navigation names a particular occurrence, not just a path:

```rust
pending_goto_line: RwSignal<Option<(TabId, u32)>>,
pending_goto_offset: RwSignal<Option<(TabId, u32)>>,
code_intel_focus: RwSignal<Option<FileFocus>>,

pub struct FileFocus {
    pub tab: TabId,
    pub key: FileResourceKey,
    pub version: ProjectFileVersion,
}
```

`FileView` consumes a pending navigation only when the target `TabId` equals
its own. Two occurrences of one file therefore keep independent scroll
positions instead of responding in lockstep.

The target occurrence is resolved when navigation is invoked:

- go-to-definition, find-references, and Command-click target the occurrence
  that initiated the navigation, as recorded in `code_intel_focus.tab`;
- if definition navigation opens a different file, that file opens in the
  initiating pane;
- a search-result click targets the focused pane's occurrence when present,
  otherwise reveals an existing occurrence, otherwise opens in the focused
  pane; and
- references-panel and review/diff-to-file jumps follow the same rule.

The file key still names the shared authoritative record and subscription;
`TabId` names only the local view occurrence that should move.

---

## 6. Ownership boundary and persistence

Files, agents, sessions, chats, and their updates remain server-owned typed
state. Both panes render the same event-driven projections used by the
single-pane center view. Pane code never branches on local versus remote
transport and never guesses a resource association. Missing, deleted,
disconnected, or unavailable content is shown explicitly in its own pane; the
other pane's content is never substituted as a fallback.

Topology, pane focus, tab placement, and the split ratio are local presentation
state inside `CenterZoneState`. The split is per project. Existing per-project
`ProjectViewMemory` preserves topology, tabs, focus, and ratio while switching
projects during the current application session. `Open to the Side` is offered
only for resources in the active project; there is no cross-project split
affordance or warning banner.

Across reloads, only the split ratio is persisted, in local storage under
`tyde-center-split-ratio`. The ratio is clamped to 20% through 80%. Split
topology, focus, tabs, and resource placement are not persisted across reloads,
so a cold start cannot create a phantom pane or stale resource reference.

No pane state is added to the protocol, server, host settings, or session
artifacts. Nothing is synchronized between windows, devices, or clients. The
ratio is a window-local preference, not domain state.

---

## 7. Focus and composer ownership

`active_tab()` means the active tab in the focused pane: what the user is
looking at. Pane focus changes when the user clicks or tabs into a pane, selects
one of its tabs, uses a pane-focus command, or completes an action targeted at
that pane. Focus is visible in pane chrome and never encoded as server state.

The chat composer remains a singleton. `composer_owner()` is derived from the
layout using this precedence:

1. The focused pane's active tab, if it is a chat.
2. The other pane's active tab, if it is a chat.
3. No owner.

The entire compose and send target is derived from `composer_owner()`, never
directly from `active_tab()`. This includes `active_agent`, live-agent send and
steer, draft spawn and fork/send, the pending-team-member target, and every
draft choice: backend override, custom agent, launch profile, session settings,
and the session-settings dirty flag. Loading, displaying, editing, resetting,
and submitting those choices must all use the same composer owner. A focus
change to a file pane must not retarget or clear the chat pane's draft.

The pending-team-member accessor used by `chat_input` and `teams_panel` is
therefore composer-owner-based. Teams-panel selection, enablement, and actions
must observe that accessor rather than the focused active tab, so a pending
member beside a focused file remains the one the visible composer will spawn.
No compose-path consumer may independently reconstruct its target from pane
focus or tab activity.

Thus a file beside a chat keeps the chat composer available even while the file
pane is focused. With two different chats, the focused pane owns the composer.
With two files, no composer appears. Duplicate chat occurrences are forbidden,
so composer ownership is never ambiguous.

Exactly one composer is mounted. A non-owning chat remains live and interactive
for reading, streaming, scrolling, and tool output, but shows a keyboard-
accessible “Reply in this pane” affordance instead of a second composer.
Controls that mutate an agent or session receive that pane's explicit agent
identity; they must not read a global active agent and infer their target.

`ToolOutputModeToggle` is different: it controls one client-global local
storage preference, not an agent, chat, or pane property. It renders only once,
with the composer-owning chat, to avoid redundant controls in a two-chat split.
It must not accept an agent identity, imply per-agent state, or change value
when composer ownership moves between panes.

---

## 8. Lazy file opens and destination precedence

Synchronous resources open immediately in the pane resolved when the command
is invoked. A second occurrence of an already-loaded file is also synchronous
and uses only existing local state. Cold file opens remain lazy: their tab is
created only after authoritative `ProjectFileContents` arrives, so their
resolved destination must survive that round trip.

```rust
pub enum OpenTarget {
    Focused,
    Beside,
}

pub enum PendingFileOpen {
    RefreshInPlace,
    Open { destination: PaneId },
}
```

`OpenTarget` is converted to a concrete `PaneId` at invocation time. A
`Secondary` destination remains meaningful before the split exists; a
successful response creates the split and places the tab in that slot. Response
handling never consults the focus that happens to exist when bytes arrive.

Pending entries exist only for cold opens and refreshes, never for duplication
of a loaded file. There is at most one pending intent per `FileResourceKey`,
with this precedence:

> `Open` supersedes `RefreshInPlace`; `RefreshInPlace` never supersedes `Open`.

Two cold opens of the same file during read latency resolve to the latest
intent. Once the file is loaded, an explicit split duplicates it synchronously.

On response:

- a pending refresh updates an already-open file without opening or focusing a
  tab;
- a pending user open updates the file, opens the tab in the recorded
  destination, and focuses that destination;
- an unsolicited response updates state in place but never invents a tab or
  destination, and is logged as an error; and
- a response for a project that is no longer active drops its pending intent
  and does not route content into the new project.

A failed read creates neither a tab nor a split, matching existing lazy-open
behavior. Pending intents are scoped by the explicit key and cleared on project
switch. Eager loading tabs require typed, correlatable read-error context and
are deferred because that would require a protocol change.

---

## 9. Opening, splitting, and moving resources

The primary actions have distinct semantics:

| Action | Result |
|---|---|
| Ordinary open | Activate in the focused pane; reveal in the other pane if that is the only occurrence; otherwise open in the focused pane |
| Split Right (`Command+\`) | Ensure the focused loaded file has an occurrence in the other pane, then focus it |
| Open to the Side: loaded file | Duplicate from the focused pane, or activate an occurrence already in the other pane |
| Open to the Side: cold file | Open asynchronously in the other pane |
| Open to the Side: non-file | Move an open resource to the other pane, or open a new resource there |
| Move Tab to Other Pane | Move any tab while split, preserving `TabId`, content, and scroll state |
| Drag tab to other pane | Move any tab while split, preserving `TabId`, content, and scroll state |

`Split Right` is idempotent: it duplicates the active file if absent from the
other pane and focuses the existing occurrence if already present. It is files-
only and is enabled only when the focused active tab is a loaded file. Every
disabled case remains visible with a specific reason:

| Condition | Reason |
|---|---|
| Home active | “Open a file to split.” |
| Chat active | “Chats can't be split — only files can appear in both panes. Use Move to Other Pane to put this chat beside a file.” |
| Diff, comments, workflow, or Agent Monitor active | “Only files can be split. Use Move to Other Pane to put this beside something.” |
| File still loading | “Wait for the file to finish loading.” |
| Center workspace narrower than 645px | “Not enough width to split — widen the window or hide a side panel.” |
| Tabs disabled | “Enable tabs to use split view.” |

The supported compositions follow directly:

- same file beside itself: `Split Right`;
- different files: `Open to the Side` on the second file;
- file beside chat: `Open to the Side` on the chat, or move the chat from an
  existing tab; and
- two different chats: move one chat to the other pane.

Files are duplicated only by `Split Right` or the loaded-file form of `Open to
the Side`. Chats and other non-files are moved, never duplicated.

Direct **Open to the Side** is mandatory on live-agent rows in both the agents
panel and Agent Monitor. It uses the state-owned typed outcome:

```rust
pub enum AgentOpenToSideResult {
    Opened { tab: TabId, pane: PaneId },
    Moved { tab: TabId, source: PaneId, target: PaneId },
    Revealed { tab: TabId, pane: PaneId },
    TabsDisabled,
    CrossProject,
    NothingWouldRemain,
    MoveRefused(MoveTabResult),
}
```

An unopened active-project chat opens opposite the focused pane. A chat in the
focused pane moves only when a tab remains behind. A chat already in the other
pane is revealed. The operation never duplicates a live chat and never converts
or duplicates a draft or pending-team-member tab. Components consume typed
state eligibility/outcomes and their authoritative reasons; they do not
reconstruct these rules from pane focus.

Unavailable side-open actions remain in the row or context menu with
`aria-disabled="true"` and accessible reason text. A contextual shortcut on an
unavailable item refuses with the same reason; it never silently performs an
ordinary open.

### Go to Chat

`GoToChat` prioritizes conversation continuity in this exact order:

1. Reveal and focus the currently visible `composer_owner()`, when one exists.
   In `file | agent`, this selects the already-visible agent and preserves the
   file beside it.
2. Otherwise reveal the last hidden chat in the focused pane.
3. Otherwise reveal the last chat in the other pane.
4. Otherwise ensure exactly one draft in the focused pane by revealing the
   existing draft or creating one. Never create a second draft.

In ordinary side-by-side mode both panes are visible. In narrow mode the pane
hidden with `display: none` is not visible for the first step. The lookup spans
both panes and includes live, draft, and pending-team-member chat tabs without
duplicating any of them.

### Server-driven chat upgrades

Focus and pane selection are user-owned. `NewAgent` and team-member events
upgrade exactly one located draft or pending-team-member tab by mutating that
same `TabId`'s content and label. They do not focus a pane, select a tab, change
strip order or closeability, or relocate the project-memory entry. Missing,
ambiguous, or already-open routing mutates no tab and opens no fallback chat.

---

## 10. Cross-pane drag and drop

Cross-pane tab drag ships in v1 and is move-only. It requires an existing split
and targets the other pane's whole surface. A translucent overlay appears only
over that valid target.

Drag state carries the source pane and `TabId` in component-local typed state;
browser `dataTransfer` is supplementary rather than authoritative. Both
`effectAllowed` and `dropEffect` are `move`. Dropping on the source pane is
refused, and dragging the final tab out of a pane moves the tab before
collapsing the split. `dragend`, including an Escape cancellation, clears local
drag state.

The keyboard equivalent is `Move Tab to Other Pane`. Drag never duplicates a
file, even though explicit commands may do so. Copy-drag, intra-strip reorder,
edge-drop-to-split, and explorer drag sources are deferred.

---

## 11. Divider and narrow-window behavior

The divider is operable by pointer and keyboard and exposes separator
semantics:

- `role="separator"`, `aria-orientation="vertical"`, and an accessible name;
- `aria-valuenow` is the **rendered** primary-pane percentage, and
  `aria-valuemin` / `aria-valuemax` are the bounds that pane can **physically
  reach at the current width**, not the policy bounds. `SplitRatio`'s 20–80% is
  a policy limit; the 320px pane minimum is a physical one, and at most widths it
  is the binding constraint (at a 911px workspace the primary pane cannot leave
  35–65%). A separator that advertises 20% while the pane stops at 35% is
  reporting a position it cannot take, and leaves a dead zone where the keyboard
  announces movement that does not happen;
- the ratio is a share of the **usable** width — the workspace less the divider —
  so an even split is even: charging the divider to one side makes "50/50" render
  as 455.5 / 450.5. Both panes must be equal within 1px;
- Left/Right Arrow changes the primary share by 2%; Shift+Arrow changes it by
  10%; Home/End select the reachable bounds; and double-click restores 50/50.
  Steps move from where the panes *are*, so the first press after a clamp is not
  swallowed;
- changes are announced through one polite live region, and **a request that
  cannot move the divider announces nothing** — silence is more honest than
  reporting a move that did not occur; and
- every ratio the divider produces is rounded to a precision the layout can use,
  so repeated steps neither accumulate float noise nor persist it.

The requested ratio is kept as requested: a width too narrow to honor it clamps
what is *rendered*, never what is stored, so widening the workspace restores the
position the user asked for.

Each pane has a 320px minimum width, enforced in layout CSS with
`.editor-pane { min-width: 320px; }`. A new split requires at least 645px of
center-workspace width, including the divider. When the unsplit workspace is
too narrow, split commands remain discoverable but disabled with a reason to
widen the window or hide a side panel.

Three independent guards are required because different layout paths can
violate the minimum: the CSS minimum protects normal flex layout and window
resize, a center-workspace width observer controls split availability and
narrow-mode rendering, and `SplitRatio` clamps every requested or restored
ratio to 20% through 80%. A drag-only numerical minimum is insufficient.

Shrinking an existing split below the threshold does not destroy it. Both panes
stay mounted and in state; only the focused pane is visible. A visible notice
states that two panes exist, gives the pane-switch shortcuts, and explains that
widening restores both. Widening restores the prior ratio without data loss.

Pane groups and active tabs have accessible names. Duplicate occurrences of a
file are distinguished by their pane names. Tab strips use tablist, tab, and
tabpanel semantics and roving tab focus. DOM order is primary strip and
content, divider, secondary strip and content. Focus styling differs by weight
as well as color. Full tab labels remain available to assistive technology when
visually ellipsized. New motion respects reduced-motion preferences, and zoom
or large text enters the narrow-window mode rather than producing unusable
slivers.

---

## 12. Commands and discoverability

Core functionality is exposed through the tab-strip split control, tab context
menu, command palette, relevant resource results, keyboard, and cross-pane tab
drag:

| Command | Shortcut | Availability |
|---|---|---|
| Split Right | `Command+\` | Focused active tab is a loaded file, tabs are enabled, and width is sufficient |
| Open to the Side | `Command/Ctrl+Enter` on a focused palette, explorer, search, or references result | Active-project resource list items only |
| Move Tab to Other Pane | `Command/Ctrl+Shift+\` | Split exists and the target pane has no conflicting occurrence |
| Focus Primary Pane | `Command+1` | Always |
| Focus Secondary Pane | `Command+2` | Split exists |
| Close Editor Pane | None | Split exists |
| Close Other Pane / Return to Single Pane | None | Split exists |

Disabled commands remain visible, use `aria-disabled="true"`, and explain why they
are unavailable. Command identity is a typed `CommandId`, and command
availability is a typed `CommandAvailability`. Command execution is an
exhaustive match with no unknown-string fallback. The command table is the
single source for execution and shortcut display.

`Command/Ctrl+Shift+\` is the accepted global Move shortcut; the shifted key
may arrive as `|`. Its descriptor, platform-aware hint, and generated binding
come from the same typed entry.

The `Command/Ctrl+Enter` Open-to-the-Side chord is attached only to resource
list items that can be opened: palette results, explorer entries, and search
and references results. It is a typed contextual result activation, not a
targetless palette `CommandId`, and is never registered as a global `app.rs`
keydown binding. In the chat composer, existing chords keep their meanings:
`Command/Ctrl+Enter` sends
normally or steers while the agent is thinking, and
`Command/Ctrl+Shift+Enter` performs Fork + send when available. Resource-row
handling must not intercept or reinterpret either composer chord.

Context menus use menu semantics and remain within the viewport. Overflowing
tab strips support horizontal wheel scrolling. Pane focus and
move/split/unsplit operations are announced. The focused tab strip has a
visible accent edge, while unfocused content remains fully legible.

---

## 13. Invariants and deferred scope

The following invariants are required throughout v1:

- The default layout is one pane; a split contains exactly two horizontal
  panes in one project.
- A loaded file has at most one occurrence per pane and at most two total.
- File occurrences share contents and subscription by `FileResourceKey` but
  own navigation and scroll state by `TabId`.
- Unloaded files, chats, diffs, comments, workflows, Agent Monitor, and Home
  cannot be duplicated.
- Closing resources is occurrence-aware; backing state is released exactly
  once after the final occurrence closes.
- A lazy open destination and navigation target are fixed at invocation time.
- User-open intent has precedence over refresh intent.
- Every pane's active tab remains mounted, including in narrow-window mode.
- Exactly one chat composer exists. Its entire target, including pending team
  member and all draft settings, derives from `composer_owner()` rather than
  directly from `active_tab()`.
- `ToolOutputModeToggle` is one client-global preference rendered only with the
  composer owner; it is not per agent.
- `GoToChat` preserves a visible composer owner before considering hidden chats
  or creating exactly one draft.
- `NewAgent` and team-member upgrades mutate one exact tab and never change
  pane focus or selection.
- Drag and `Move Tab to Other Pane` always move and never copy.
- Unavailable actions remain visible, `aria-disabled`, and expose the
  authoritative reason; contextual activation never falls back silently.
- Local presentation state never becomes server-owned resource state.
- Collapsing or narrowing a split never silently substitutes or discards a
  surviving resource.
- Single-pane behavior remains unchanged when no split exists.
- Desktop split behavior does not enter mobile or PWA layout.

Explicitly deferred from v1:

- copy-drag or modifier-copy;
- intra-strip tab reordering;
- edge-drop-to-split and explorer drag sources;
- duplicate chats, diffs, comments, workflows, Agent Monitor, or Home;
- vertical, nested, or more-than-two-pane layouts;
- cross-project splits;
- empty persistent drop-target panes;
- per-tab chat drafts or multiple composers;
- eager loading tabs and typed `ProjectReadFile` error correlation;
- restoring tabs, split topology, focus, or resource placement after reload;
  and
- synchronizing layout across windows, devices, or clients.

---

## 14. Tests and QA

`./dev.sh check` is the sole repository validation entry point. Existing
single-pane assertions remain unchanged. Fixture construction may adapt to the
typed layout, file key, and `TabId`-targeted navigation shapes. Navigation tests
must preserve and sharpen their existing contracts rather than weakening them.

Native state coverage must establish:

- one loaded-file occurrence per pane, shared contents and version, independent
  scroll state, and no read, subscribe, or pending-open work on duplication;
- rejection of unloaded-file, live-chat, draft-chat, pending-team-chat, diff,
  comments, workflow, Agent Monitor, and Home duplication;
- closing one occurrence without teardown, closing the last with exactly one
  teardown, and correct survivor/released sets for every bulk-close path;
- go-to-line, go-to-offset, search, references, and definition navigation
  targeting only the resolved `TabId` occurrence;
- exact file identity across projects and exact-key code-intelligence teardown;
- pending-open precedence, unsolicited and superseded response behavior, and
  cold-open destination stability across focus changes;
- composer ownership for file/chat, different-chat/chat, and file/file splits;
- pending-team-member, teams-panel, send/steer, spawn, and draft-setting target
  derivation from `composer_owner()` across pane focus changes;
- `GoToChat` ordering: visible composer owner, hidden focused-pane chat,
  other-pane chat, then exactly one draft;
- mutation-only ordinary-draft and team-member upgrades that preserve focus,
  both pane selections, strip order, and `TabId`, with missing or ambiguous
  intent opening nothing;
- one non-redundant global `ToolOutputModeToggle` whose value does not follow
  agent or pane identity;
- pinning both panes' active tabs;
- `SplitRatio` clamping pointer, keyboard, restored, and programmatic values to
  20% through 80%;
- every `Split Right` eligibility and disabled-reason case;
- typed direct-agent side open covering open, move, reveal, tabs-disabled,
  cross-project, nothing-would-remain, and move-refused outcomes;
- idempotent file duplication, move semantics, pane collapse and promotion;
  and
- per-project in-session split round trips.

Wasm component coverage uses sized containers and asserts user-observable
behavior: one versus two tab strips; 50/50 and 70/30 geometry; pointer and
keyboard divider behavior and ARIA values; pane focus shortcuts; narrow-window
hiding with both panes retained in the DOM; restoration on widening; exactly
one composer and the non-owner affordance; disabled-reason text; accessible
tab, pane, menu, and separator semantics; and the full duplicate-file flow.
That flow verifies the same file renders in both panes, scrolling and navigation
affect only the targeted occurrence, and closing one leaves the other intact.
Geometry coverage also verifies the computed 320px pane minimum and the width
observer's transition between split availability, narrow mode, and restored
side-by-side layout without losing the clamped ratio.

Keyboard coverage verifies that Open to the Side fires from focused resource
list items only, that no global app binding claims `Command/Ctrl+Enter`, and
that chat `Command/Ctrl+Enter` send/steer plus
`Command/Ctrl+Shift+Enter` Fork + send remain unchanged. It also verifies the
global Move binding and hint are `Command/Ctrl+Shift+\`, including the shifted
`|` event spelling.

Drag coverage verifies cross-pane movement, preserved `TabId` and scroll state,
source-pane refusal, collapse after moving the final tab, move-only drop effect,
drag-state cleanup, and the valid-target overlay.

Client-level tests use the mock backend and assert on events and observable
state. They cover same-path files in different projects, exact unsubscribe
frames, late project file responses, zero read/subscribe frames for loaded-file
duplication, no unsubscribe when one occurrence closes, unsubscribe after the
last occurrence, independent chat streams, and closing one chat without
changing another.

Debug-instance QA exercises real input and rendered output: the same file in
both panes at different scroll positions; different files; file/chat; real
cross-pane tab drag; pointer and keyboard resizing; narrow and restored widths;
reload preserving only the ratio and creating no phantom split; project
switching; and host disconnect or missing-resource states without cross-pane
substitution.

---

## 15. Current implementation and verification status

The current coordination handoffs and inspected split-center diff report the
v1 contract in this document as implemented. That includes the typed state and
eligibility outcomes, exact occurrence cleanup and `TabId` navigation,
composer-owner targeting with the hidden narrow-pane exception, mutation-only
agent upgrades, two-pane UI, file-only Split Right, move-only cross-pane drag,
accessible divider and narrow behavior, ratio-only persistence, and the
contextual side-open surfaces for files, Search, References, live agents, and
Git diffs. Direct agent and Git actions consume their state-owned typed
outcomes; file surfaces consume shared file availability. Refusals remain
visible and `aria-disabled`, with no ordinary-open fallback. The state,
Agents, and Git fixture migrations previously listed as gaps are present.

This status is based on implementation handoffs and source inspection, not a
validation result. No final `./dev.sh check` has been reported for the combined
concurrent worktree, and the corrective-review follow-ups await independent
closure confirmation. The feature therefore must not be called `CLEAN`,
passing, or release-ready yet. The outstanding work is verification and
multi-owner integration risk, not a reason to reopen the finalized chat
priority, shortcuts, typed eligibility, no-duplication, focus ownership,
accessibility, protocol boundary, or persistence policy.
