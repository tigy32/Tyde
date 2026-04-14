# Workflows Gap Analysis

## Pass 1 - GPT-5 Codex - 2026-04-13

### Legacy reference points
- `~/Tyde/src/workflows/panel.ts`
- `~/Tyde/src/workflows/store.ts`
- `~/Tyde/src/workflows/engine.ts`
- `~/Tyde/src/workflows/builder.ts`

### Rewrite reference points
- No rewrite workflow UI or workflow state modules were found in `frontend/src` or `server/src`.

### Legacy coverage
- Dedicated workflow subsystem with definitions, builder, run engine, run history, status expansion, and links back into agent conversations.
- Workflow execution was visible and operable from the workspace surface.

### Rewrite coverage
- No workflow feature surface found.

### Confirmed gaps vs legacy
- No workflow definitions.
- No workflow builder/editor.
- No workflow run engine in the app shell.
- No workflow run history panel.
- No workflow-to-agent integration in the UI.

### Suggested next slices
- Keep this as a separate parity track rather than mixing it into agents or chat.
- If workflows are still in scope for the rewrite, they currently need greenfield reintroduction.

## Pass 2 - GPT-5 Codex - 2026-04-13

### Additional confirmed gaps
- Second-pass check still found no workflow-related frontend/server/protocol surface in the rewrite codebase. This is a full subsystem omission, not a partially implemented feature.
- In the legacy app, workflows were integrated with workspace navigation and agent conversations. That integration layer is also absent in the rewrite.

### Architectural note
- If workflows remain part of the product, they should probably be tracked as a discrete parity milestone rather than folded into chat or agents as incidental follow-up work.

## Pass 4 - GPT-5 Codex - 2026-04-13

### Test-backed behavior gaps
- Legacy E2E coverage verifies workflow empty state, run-menu population, run cards, expandable action details, hide-completed behavior, manager access, and opening linked agent conversations from workflow runs.
- The rewrite has no workflow UI at all, so every tested workflow behavior remains missing rather than partially implemented.
