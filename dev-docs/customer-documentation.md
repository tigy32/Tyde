# Customer documentation

The customer entry points are `README.md` and the Tyde User Guide under `docs/`.
The book is a static site: no framework installation, third-party scripts,
external fonts, or network-dependent content rendering. The repository edition
is `docs/book.md`; the designed edition begins at `docs/index.html`.

## Editorial scope

The README introduces the product and explains why someone might choose it.
The book is a user guide for someone who has already installed and opened Tyde.
Explain what a feature does, where its controls are, how to use it, and what
result to expect. Use exact UI labels, numbered procedures, and task examples.
Do not lead chapters with benefits, slogans, installation instructions, or
reasons to adopt the product. Screenshots should identify controls and state;
animation should explain behavior, such as disconnecting from a remote host.

## Editing and rendering

- Edit chapter copy in `docs/chapters/*.html` (semantic HTML fragments).
- Edit titles, descriptions, and reading order in `docs/contents.json`.
- Edit the shared page template in `docs/build.py` and presentation in
  `docs/assets/book.css` and `docs/assets/book.js`.
- Run `python3 docs/build.py` to render the site, repository edition, and search
  index. Commit the generated files with their sources.
- Preview with `python3 -m http.server 4318 --bind 127.0.0.1 --directory docs`,
  or open `docs/index.html` directly. Search also works over `file://`.
- The customer-documentation stage verifies generated output, local links, and
  chapter anchors without modifying files.
- Run repository validation only through `./dev.sh check`, before landing and
  again on clean main, as required by AGENTS.md.

Publication is a separate, explicitly authorized action. This change does not
modify the existing marketing site, mobile loader, or deployment workflows.
A static host can serve this directory at any prefix: chapter and asset links
are relative. Publish the rendered root HTML files and `assets/`; chapter
fragments, the builder, and contents manifest are authoring files.

## Content evidence

The initial book was checked against source at
`d78715d49026b0470ebe3a09897486660de2c6af` (0.9.3-beta.1).
This was a documentation audit, not a backend conformance certification.

| Contract | Source |
| --- | --- |
| Projectless chat and setup | `frontend/src/components/home_view.rs` |
| Multi-root selection and restrictions | `file_explorer.rs`, `host_browser.rs` |
| Working and committed changes | `git_panel.rs`, `diff_view.rs` |
| File comments and submission gates | `review_layer.rs`, `review_view.rs` |
| History and agent organization | `sessions_panel.rs`, `agents_panel.rs` |
| SSH managed install and host settings | `settings_panel.rs`, Hosts tab |
| Host skills and custom agent assignment | `settings_panel.rs`, Skills and Custom Agents tabs |
| Workbench lifecycle | `project_rail.rs`, `dev-docs/18-workbenches.md` |
| Delegation and teams | `dev-docs/15-sub-agents.md`, `dev-docs/19-agent-teams.md` |
| Rust and Python language support | `server/src/code_intel/mod.rs`, `pyright.rs` |
| Platform shortcut formatting | `frontend/src/components/command_palette.rs` |

Component paths above are relative to `frontend/src/components/` unless a full
repository-relative path is shown. Avoid copying unsupported provider claims
from the old website. Configuration unification is host-scoped and does not
promise identical capabilities across harnesses.

## Product captures

`docs/assets/home.png`, `review.png`, and `hosts.png` originate from the real
Tyde desktop frontend at the source revision above. The example is a disposable
Git repository named Garden, with one committed HTML page and one working-tree
change: converting a navigation button into a link. The file review comment was
entered and saved through the real rendered UI. No agent was launched, no AI
responses were fabricated, and no review was submitted to a provider.

Capture limitations and method:

1. The installed debug MCP launcher used protocol 57, while this checkout used
   58. Its first cold build timed out; a warm attempt exposed the mismatch.
2. The already-built desktop binary was launched with every mutable store
   redirected according to `DEV_INSTANCE_MUTABLE_PATHS`, including configured
   hosts, workflows, sessions, projects, settings, and tracing. The frontend
   was served from the matching built `frontend/dist`. The app used local
   loopback host and UI-debug endpoints. This was only used for UI capture,
   not backend certification or live agent work.
3. Native macOS window capture was unavailable. The real rendered DOM was
   exported using UI-debug evaluate, with its original stylesheets. Scripts
   and preload links were omitted so the exported state would remain still.
4. Chromium rendered that DOM at 2× scale: Home at 1200×800, review at 1200×650,
   and Hosts at 1280×1000. Finite CSS animations were finished for the Hosts
   capture. These are captures of actual rendered product state, not native
   WKWebView screenshots; font metrics can differ between renderers.
5. The disposable instance and stores were removed after capture. No production
   project or session records were used.

For future captures, prefer native capture when available. Recreate the Garden
repository and use actual UI actions. Check visible text and inspect each image
before replacing it. Keep text legible at chapter width and link to full-size
images. Never invent agent transcripts or use generated imagery as a product
screenshot.

### Full studio hero

`docs/assets/studio.png` shows the desktop frontend from `07895ebd` with
three disposable Git projects: Garden (a static publishing site), Atlas API
(a small Python service), and Field Notes (article drafts). Eight actual Codex
agents ran bounded tasks using `gpt-5.4-mini`: accessibility, mobile layout,
editorial guidelines, search behavior, API contracts, request validation, an
article outline, and a remote-work introduction. Each agent read real example
files and returned its own response. Their names were changed through the UI
to begin with `Demo ·`, and the sidebar is filtered to those demo agents with
other projects visible. These are real conversations, not scripted responses.

The selected search conversation first inspected `scripts/search.js` and
`index.html`, then received this follow-up:

> Please implement a friendly empty state for search. Show ‘No notes found’
> with a Clear search button, keep it keyboard accessible, and match the
> existing design. Check the result and explain what changed.

The DOM was captured while its sidebar status was Thinking and its composer
showed Cancel. The visible command cards are completed file inspections during
that still-active turn; the other seven displayed agents were idle. This image
does not imply that all eight agents were executing simultaneously or that the
implementation had passed verification. The capture was rendered with the
original application styles at 1680×1050, 2× scale (3360×2100 PNG), using the same
DOM-export method as the initial images. Viewport and scroll positioning are
presentation choices; transcript text and activity state were not altered.

The initial Claude setup attempt encountered expired OAuth credentials. The
successful demo used Codex instead. Its failed setup chat was excluded by the
Demo filter and its tab was closed. The protocol-57 installed launcher still
could not attest readiness against protocol 58, so capture used the matching
built binary with all mutable paths redirected to a fresh disposable directory
according to `DEV_INSTANCE_MUTABLE_PATHS`. Before launching demo chats, the
paths were checked for containment and the projects/session stores contained
no production records. This was screenshot production, not backend conformance
certification. Demo processes and their temporary stores were cleaned up after
capture. No backend implementation was changed.

The animated host diagram is explicitly labeled an interactive concept diagram.
Its example agents are illustrative. Disconnect changes only the diagram's
client state; its host agents continue pulsing. Pause and reduced-motion support
make animation optional. It is not evidence of a live remote-host run.

## Editorial direction

Lead with concurrent agents across projects and machines. Teach one local agent
before projects, then review and organization, then remote hosts, delegation,
skills, and workbenches. Distinguish agent instances, saved sessions, custom
agent definitions, and hosts. Include prerequisites, visible actions, and the
expected outcome in workflow chapters.

When adding a major feature, update its chapter, relevant reference material,
and screenshots in the same change. Capture long-form narrated videos from
real workflows only after their UI and scripts are stable; the initial book
uses static product captures and an interactive animated explanation.

## Mobile capture

`docs/assets/mobile-chat.png` is a WebKit capture of the real Leptos mobile
conversation UI, built with the repository's `ui-fixtures` development feature.
It uses the existing `chat` fixture with demo agent Mira and sample conversation
text. The follow-up was typed into the composer and left unsent. It is not a
live host pairing, provider response, or backend conformance result.

Capture at 393 × 680 CSS pixels with device scale 3 (1179 × 2040 PNG), using the
fixture route `?tyde-fixture=chat`. Wait for the composer, type the follow-up,
blur the input, and capture the viewport. The browser image does not include
iOS system chrome or an on-screen keyboard. Do not add fictional device status
or describe demo content as live work.

The mobile chapter was checked against the host Mobile settings, mobile
navigation, conversation, session, notification, and pairing components, plus
`dev-docs/35-mobile-direct-hosting.md`. It distinguishes cloud pairing from
direct hosting and desktop History from mobile Sessions.
