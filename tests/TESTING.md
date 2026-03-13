# Tyde Testing Guide

## Purpose

High-confidence smoke tests that validate end-to-end user workflows. Each test file covers one major feature area with a single comprehensive flow.

## Philosophy

1. **One test per feature area.** Each file exercises the full user journey for its feature — not isolated unit checks.
2. **Smoke tests for confidence.** The goal is fast feedback that nothing is fundamentally broken after a change.
3. **Test through the UI.** Drive the app with `click`, `type`, keyboard shortcuts. Assert what a real user would observe: visible text, enabled/disabled controls, panel visibility.
4. **Avoid implementation coupling.** Do not assert DOM tree shape, internal class fields, localStorage keys, or method calls. If a refactor changes CSS classes but preserves behavior, tests should still pass.
5. **Long flows are fine.** A single test can perform a dozen interactions if needed to cover the full workflow.

## Test Structure

| File | Feature Area |
|------|-------------|
| `home.test.ts` | App launch, home view, header actions, opening a workspace |
| `chat.test.ts` | Chat tab lifecycle: new chat, messages, streaming, context bar, tool calls, subprocess exit |
| `workspace.test.ts` | Multi-workspace: tab isolation, state preservation, DOM isolation, welcome screens |
| `settings.test.ts` | Settings overlay: open/close, backend data, providers, profiles, module schemas |
| `remote.test.ts` | Remote SSH: connection dialog, step completion, auto-dismiss, failure handling |
| `git.test.ts` | Git panel: non-git project handling |
| `workbench.test.ts` | Workbenches: create git worktree, switch, rename, remove, sidebar/home grouping |

## Fixture Pattern

Tests use deterministic mocks at the Tauri command/event boundary:

- `mocks/tauri-core.ts` — Controlled backend (invoke commands, mock responses)
- `mocks/tauri-event.ts` — Event emission (`chat-event`, `remote-connection-progress`)
- `mocks/tauri-dialog.ts` — Deterministic workspace path selection

Mock responses produce realistic event sequences: `TypingStatusChanged` → `StreamStart` → `StreamDelta` → `StreamEnd`.

## Helpers

Shared helpers in `helpers.ts` encapsulate repeated setup:

- `openWorkspace()` — Navigate to home, click open, wait for workspace title
- `openWorkspaceAndWaitForChat()` — Open workspace + Ctrl+N to create chat tab
- `sendPromptAndWaitForAssistant(prompt)` — Type message, click send, wait for non-streaming response
- `emitChatEvent(conversationId, kind, data)` — Directly emit chat events for fine-grained control

## Assertion Strategy

Good: "Message input is enabled after workspace opens." / "Context usage shows 50.0K after first response." / "Typing indicator hides after StreamEnd."

Avoid: "Method X was called." / "Internal map has 3 entries." / "localStorage key Y exists."
