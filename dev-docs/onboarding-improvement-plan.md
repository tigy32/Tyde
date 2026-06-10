# Tyde onboarding — "I can't figure out how to start" — analysis & plan

> Written after driving a live dev instance and mapping the first-run code
> path. The complaint ("people open Agent Studio and have no idea how to use
> it") reproduces. This documents *why* and proposes a phased fix.

## Status update (2026-06-10)

Implemented (in working tree, verified live + by wasm/unit tests):

- Getting-started guide on the home screen — now **always visible** (Mike's
  call: it doubles as orientation for everyone), with live per-step progress
  (✓ markers), a backend CTA deep-linking to Settings → Backends, and a
  "Choose a folder →" CTA that opens the project browser directly.
- **Home dashboard removed entirely** (Projects/Agents tabs, host sections,
  project cards, agent rows). Everything it showed was duplicated in the left
  rail / right panel; rename-project lives in the rail, remove-root in the
  file explorer. Home went from ~hundreds of interactive elements to 4
  buttons.
- **Guided spotlight tour** (`components/help_tour.rs`): a Help button on the
  home screen launches a 5-step overlay that dims the screen and draws a
  highlight ring around the *real* UI — project rail → New Chat → left dock
  (files/git) → right dock (agents/history/teams) → bottom dock (terminals) —
  with a callout card (Next/Back/×). Hidden panels fall back to a centered
  card with a "toggle it with the Left/Bottom/Right header button" hint.
  Driven by `state.help_tour_step`; a plain-text "Finding your way around"
  list was tried first and rejected (not visual, made the home view scroll
  on laptops).
- "Manage Hosts" deep-links to Settings → Hosts (and ⌘-, CTAs use the new
  `settings_tab_request` signal).
- Chat composer shows an inline "no backend yet" notice with a setup CTA
  instead of silently eating the first message.
- Backend taglines in the New Chat picker; contextual tooltips on disabled
  `+` buttons.
- Server seeds enabled backends from installed CLIs on first run and picks a
  default (Claude > Codex > Gemini > Kiro > Tycode).

Decisions from Mike:

- **Sign-in stays in the terminal.** The CLI's own OAuth flow is the
  canonical way to authenticate (e.g. running `claude`); we don't rebuild it.
  Improvements should be around it: one-click launch in the dock terminal
  (exists), detecting signed-in state up front, and plain-language
  success/failure status instead of raw terminal output as the only signal.
- Written getting-started docs are not the fix; in-app guidance is.

Remaining work is tracked in the phases below — primarily backend sign-in
*status detection*, rail legibility, and hiding the host concept for
local-only users.

## TL;DR

A new user must understand and configure **five concepts in the right order**
— host → backend (enable + install + sign-in) → project → root → chat — before
a single agent will run, and **nothing in the UI tells them that or walks them
through it**. The first screen is simultaneously *empty of guidance* and
*visually overwhelming*. The fastest, highest-leverage fix is a first-run
guided setup plus making "open a folder → start chatting" the one obvious path,
not curing each concept individually.

## What a new user actually hits (the journey)

1. **Launch → a dense, label-less wall.** The home screen shows a "Tyde —
   Coding Agent Studio" splash with three keyboard hints (⌘K / ⌘N / ⌘,) and a
   left rail. On a populated machine the rail is a column of cryptic 2–4 letter
   codes (`tc`, `tyco`, `tg`, `tumb`, `mb`, `b4`, `wtr`, `gr0`…) — auto-initials
   of project names, full name only on hover. On a *fresh* machine it's nearly
   blank with a `+`. Three unlabeled panel toggles ("Left / Bottom / Right").
   `1/2 hosts connected` with no explanation of what a host is.
   - `frontend/src/components/home_view.rs:50-166` (splash, tabs, hints)
   - `frontend/src/components/project_rail.rs` (rail + abbreviations)

2. **The one obvious action is disabled.** "New Chat" is greyed out until a
   host is connected **and** a backend is resolvable — with **no tooltip
   explaining why**. The only working button is "Manage Hosts".
   - `frontend/src/components/home_view.rs:509-597` (disabled NewChatButton)
   - The only hint that backends matter is buried in the New Chat dropdown:
     "No enabled backends. Enable one in Settings → Backends."
     (`home_view.rs:400-406`)

3. **Settings is a 12-tab maze.** ⌘, opens to **Hosts**, not Backends. The user
   has to discover the Backends tab themselves among Hosts / Appearance /
   General / Backends / Custom Agents / MCP Servers / Steering / Skills /
   Mobile / Debug / …
   - `frontend/src/components/settings_panel.rs:1505-1585` (Backends tab)

4. **Backends require a 3-step terminal dance.** For each backend the user must
   (a) toggle it on, (b) click **Install** (runs a shell command, output in the
   bottom dock), (c) click **Sign in** (OAuth / API key in the terminal). No
   recommendation on *which* backend a newcomer should pick; five equal toggles
   (Claude / Codex / Kiro / Tycode / Gemini). Failures surface as raw terminal
   text.
   - `frontend/src/components/settings_panel.rs:2386-2540` (BackendCard)

5. **Projects vs roots is never explained.** To give the agent code access the
   user must create a Project (a named container) and add Roots (filesystem
   paths). The UI surfaces "No project / + root / No files loaded" and "No
   workspace roots" with no definition of either term. Creating a project is a
   `+` in the rail that opens a folder browser — discoverable only by clicking.
   - `protocol/src/types.rs:2021-2027` (Project = name + roots)
   - `frontend/src/components/host_browser.rs:418-473` (create project / add root)

6. **Only now** can they type a message and run an agent
   (`frontend/src/actions.rs:90-176`).

### Root causes (not symptoms)

- **Concept load before first value.** 5 ordered concepts, ~9 clicks, and a
  terminal install/sign-in before the first token. Best-in-class agent tools
  get you to "ask a question about this folder" in 1–2 steps.
- **Zero first-run guidance.** There is no welcome flow, no checklist, no
  empty-state CTA, no "you need a backend first" inline. (Confirmed: no
  onboarding component exists anywhere in `frontend/src`.)
- **Disabled-without-explanation.** The primary CTA is dead on arrival with no
  "why" and no link to fix it.
- **Visual density.** Even the author's own home screen rendered **525
  buttons**. A newcomer can't find the 2 that matter.

## The plan

Sequenced so each phase ships value independently. Phases 0–1 remove the "I'm
stuck" complaint; Phase 2 attacks the underlying complexity; Phase 3 is polish.

### Phase 0 — Stop the bleeding (≈ half a day, no model changes)

Cheap, high-signal, no architectural risk:

- **Tooltip + reason on the disabled "New Chat".** When disabled, hover/inline
  text: "Connect a host and enable a backend to start." Make the text a button
  that jumps straight to Settings → Backends.
- **Empty-state CTAs on the home view.** When there are no projects: a single
  prominent "Open a folder to start" card. When no backend is enabled: "Enable
  a backend (1 min)" card that deep-links to Backends. Replace silent blank
  states with one clear next action.
- **Deep-link ⌘, to the right tab.** If no backend is enabled yet, open Settings
  directly on Backends, scrolled to a recommended backend.
- **Recommend a default backend.** Badge one backend "Recommended for new
  users" instead of five equal toggles.

### Phase 1 — First-run guided setup (the core fix, ≈ 2–3 days)

A dismissible first-run flow shown when `hosts==connected-local-only` **and**
no backend is enabled **and** no projects exist. Three steps, each doing the
work for the user, not just describing it:

1. **Pick a backend** → runs install + sign-in inline with progress + plain-
   language error states (not raw terminal). Reuse the existing install /
   sign-in commands from `BackendSetupStatus`.
2. **Open your first folder** → folder picker that auto-creates a Project named
   after the folder with that folder as its root (the flow already exists in
   `host_browser.rs`, just front-load it).
3. **Try it** → pre-fills the chat with a starter prompt ("Give me a tour of
   this codebase") and enables Send.

Implementation notes: new `components/onboarding.rs`, gated by a persisted
`onboarding_completed` flag (host setting or local storage); reuse existing
spawn/install/project actions rather than new backend protocol. Add a
"Getting started" entry to the palette (⌘K) and a `?` in the header to re-open
it.

### Phase 2 — Shrink the mental model (the real "is it too complicated?" answer)

The honest answer to "is projects+roots too complicated?" is: **the concepts
are fine for power users, but they shouldn't be mandatory or front-loaded for
newcomers.** Don't remove them — make them implicit until needed:

- **Make "Open a folder" the canonical entry**, not "create project then add
  root." One folder → one project → one root, named automatically. Multi-root
  and host concepts stay, but appear only when the user reaches for them.
- **Hide "host" for the common case.** 90% of users only ever use the embedded
  Local host. Don't show "1/2 hosts connected" or a Hosts-first Settings tab to
  someone who has never added a remote host; surface hosts only once they click
  "Add remote."
- **Rail legibility.** Show project names (or name + icon), not bare initials,
  at least until the rail is full; the cryptic codes are unreadable cold.

### Phase 3 — Reduce home-screen density (polish)

- Collapse the home screen to a focused "Start" state for new users (CTA-first)
  vs. the current everything-at-once dashboard for power users.
- Progressive disclosure of the Left/Bottom/Right panels — start with just the
  chat, reveal panels as they're used.

## Recommendation

Do **Phase 0 immediately** (it directly kills the "disabled button, now what?"
dead-end) and **Phase 1 next** (the guided setup is what converts "I'm
confused" into "oh, that was easy"). Treat Phase 2 as the strategic answer to
"is it too complicated" — keep projects/roots, but stop making users learn them
before they get value. A standalone written "getting started guide" is the
*weakest* option on its own: docs people don't read can't fix an empty screen —
an in-app guided flow that does the setup for them is far stronger.

## Open questions for Mike

- Which backend should be the "recommended for new users" default?
- Is there appetite to hide the host concept entirely for local-only users, or
  must "1/2 hosts connected" stay visible?
- Should onboarding state persist per-host (server setting) or per-device
  (local storage)?
