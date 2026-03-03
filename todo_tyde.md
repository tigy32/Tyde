# Tyde — TODO

Priorities: **P0** = broken/blocking, **P1** = important, **P2** = should do, **P3** = nice to have, **P4** = someday, **P5** = wishlist

## P0 — Broken / Blocking

_(none)_

## P1 — Important

- [ ] **Broken pipe crash detection.** `write_all` errors should trigger the crash detection flow, not just return an error string.

## P2 — Should Do

- [ ] **Source links in TODO rendering.** When TODO/markdown files are viewed in Tyde's chat, file links like `[file.rs](src-tauri/src/file.rs:123)` should be clickable and open the file at that line.

- [ ] **Message editing.** Click a previous user message to edit and re-send it, creating a branch in conversation history.
- [ ] **Conversation clear/reset.** Clear the current conversation and start fresh without killing the subprocess.
- [ ] **Branch display and switching.** Show current branch, allow switching branches.
- [ ] **Commit history / log.** Show recent commits with hash, message, author, date.
- [ ] **Pull/push operations.** Fetch, pull, and push buttons with remote tracking.
- [ ] **Virtual scrolling for chat.** Long conversations degrade scroll performance. Only render visible messages.

## P3 — Nice to Have

- [ ] **Stderr forwarding to frontend.** Subprocess stderr is logged but never surfaced to the user.
- [ ] **Markdown gaps.** Missing: tables, strikethrough, horizontal rules, nested lists (depth > 1), task lists (checkboxes).
- [ ] **Word-level diff highlighting.** Highlight changed words/characters within modified lines, not just entire lines.
- [ ] **Tool card interactivity.** Click to expand raw JSON request/response, "View Diff" action for file modifications, "Re-run" for commands.
- [ ] **Drag-and-drop files into chat.** Drag files from the explorer into the chat input to reference them.
- [ ] **Large diff virtualization.** Diffs with thousands of lines should be paginated or virtualized.

## P4 — Someday

- [ ] **Cross-platform CI/CD.** Build for macOS (x64 + ARM), Linux (AppImage/deb), and Windows (MSI/NSIS).
- [ ] **Auto-updater.** Integrate Tauri's updater plugin for automatic updates.
- [ ] **Native menus.** macOS menu bar with standard Edit/View/Window menus and app-specific items.
- [ ] **Deep linking.** Register `tycode://` URL scheme to open workspaces or sessions from external apps.
