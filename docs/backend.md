# Backend Specification

## Purpose
Each backend is a protocol adapter. It must:

1. Accept Tyde conversation/admin commands.
2. Translate them to its provider/subprocess protocol.
3. Emit Tyde `ChatEvent` JSON compatible with the Tycode event model.

The backend contract is the stability boundary for UI behavior (streaming, tool cards, diffs, sessions, agents).

## Canonical Contracts

- Tyde runtime command model: `src-tauri/src/backend.rs` (`SessionCommand`).
- Tyde frontend event/tool schema: `src/core_types.ts`.
- Tyde runtime parser constraints: `src/protocol.ts`.
- Canonical Tycode subprocess/event model:
  - `tycode-core/src/chat/events.rs` (Tycode repo)
  - `tycode-core/src/chat/actor.rs` (Tycode repo)
  - `tycode-core/src/chat/ai.rs` (Tycode repo)
  - `tycode-core/src/chat/tools.rs` (Tycode repo)
  - `tycode-subprocess/src/lib.rs` (Tycode repo)
  - `tests/e2e/chat.test.ts`

`src/core_types.ts` defines the JSON shape Tyde consumes, but the event lifecycle semantics come from Tycode's actor/AI/tool loop. Backend adapters must preserve those semantics, not just the field names.

## Wire-Level Requirements (Tycode JSON Subprocess)

- Use line-delimited JSON over stdin/stdout (`one JSON object/string per line`).
- Parse every stdout line as JSON event.
- Forward stderr lines as:
  - `{ "kind": "SubprocessStderr", "data": "<line>" }`
- Emit process exit as:
  - `{ "kind": "SubprocessExit", "data": { "exit_code": <number|null> } }`
- Support cancel sentinel:
  - raw line `CANCEL`

## Command Ingress Requirements

Backends should implement `SessionCommand`, but conformance is split into core commands and Tycode-specific extensions.

Core commands (expected for all conversation backends):

- `SendMessage { message, images? }`
- `CancelConversation`
- `ListSessions`
- `ResumeSession { session_id }`
- `DeleteSession { session_id }`
- `ListModels`
- `UpdateSettings { settings, persist }`

Tycode-specific extensions (optional for non-Tycode backends such as Codex/Claude):

- `GetSettings`
- `ListProfiles`
- `SwitchProfile { profile_name }`
- `GetModuleSchemas`

For optional/unsupported commands, do not crash. Return success with a safe no-op or emit a clear `Error` event.

Tycode payload mapping (when speaking to `tycode-subprocess`):

- `SendMessage` -> `{ "UserInput": "<text>" }`
- `SendMessage` + images -> `{ "UserInputWithImages": { "text": "...", "images": [...] } }`
- `CancelConversation` -> `CANCEL`
- `GetSettings` -> `"GetSettings"`
- `ListSessions` -> `"ListSessions"`
- `ResumeSession` -> `{ "ResumeSession": { "session_id": "..." } }`
- `DeleteSession` -> `{ "DeleteSession": { "session_id": "..." } }`
- `ListProfiles` -> `"ListProfiles"`
- `SwitchProfile` -> `{ "SwitchProfile": { "profile_name": "..." } }`
- `GetModuleSchemas` -> `"GetModuleSchemas"`
- `UpdateSettings` -> `{ "SaveSettings": { "settings": {...}, "persist": true } }`

Note: Tycode subprocess builds can differ by version. If a target subprocess does not support a command variant, the backend must handle that gracefully (no crash, clear error/no-op behavior).

## Event Egress Requirements

Emit only supported `ChatEvent.kind` values (plus `SubprocessStderr` / `SubprocessExit`).

Core event kinds consumed by Tyde:

- `MessageAdded`
- `StreamStart`
- `StreamDelta`
- `StreamReasoningDelta`
- `StreamEnd`
- `Settings`
- `TypingStatusChanged`
- `ConversationCleared`
- `ToolRequest`
- `ToolExecutionCompleted`
- `OperationCancelled`
- `RetryAttempt`
- `TaskUpdate`
- `SessionsList`
- `ProfilesList`
- `ModelsList`
- `TimingUpdate`
- `ModuleSchemas`
- `Error`

Event ordering requirements:

- At the start of a user request: emit `TypingStatusChanged(true)` once.
- For each assistant API call:
  - emit `StreamStart`
  - emit `StreamDelta` / `StreamReasoningDelta`
  - emit `StreamEnd` with the fully assembled assistant `ChatMessage`
- If that assistant message requested tools:
  - include those calls in `StreamEnd.data.message.tool_calls`
  - after `StreamEnd`, emit `ToolRequest` / `ToolExecutionCompleted` pairs for those calls
  - do not emit the next `StreamStart` until all tool completions for the previous assistant message have been emitted
- On overall request completion: emit `TypingStatusChanged(false)`.
- On cancellation: `OperationCancelled` and `TypingStatusChanged(false)`.
- On fatal backend/process failure: `SubprocessExit`.

Canonical repeated pattern for tool-driven turns:

- `TypingStatusChanged(true)`
- `StreamStart`
- `StreamDelta` / `StreamReasoningDelta`
- `StreamEnd` with `message.tool_calls = [...]`
- `ToolRequest`
- `ToolExecutionCompleted`
- `ToolRequest`
- `ToolExecutionCompleted`
- `StreamStart`
- `StreamDelta` / `StreamReasoningDelta`
- `StreamEnd`
- `TypingStatusChanged(false)`

This mirrors Tycode's actor loop in `tycode-core/src/chat/ai.rs` and tool executor in `tycode-core/src/chat/tools.rs` in the Tycode repo. Tyde's frontend tests also depend on pending tool cards appearing immediately after `StreamEnd`, before explicit `ToolRequest` events.

## Tool Bridging Requirements

### Lifecycle

- Every tool call must have stable `tool_call_id`.
- Tool events belong to the assistant message that most recently ended with that `tool_call_id` in `message.tool_calls`.
- `ToolRequest` should be emitted before `ToolExecutionCompleted`.
- `ToolExecutionCompleted` must reuse the exact same `tool_call_id`.
- On failure, still emit `ToolExecutionCompleted` with `success: false`.
- `StreamEnd.data.message.tool_calls` should include known tool calls so Tyde can pre-render pending cards before explicit `ToolRequest` events arrive.
- Do not invent a new `StreamStart` boundary just to show tool progress. Tool progress is represented only by `ToolRequest` / `ToolExecutionCompleted` after the owning `StreamEnd`.

### File modifications (diff rendering requirement)

To render frontend diffs, `ToolRequest` must include:

```json
{
  "kind": "ToolRequest",
  "data": {
    "tool_call_id": "id",
    "tool_name": "modify_file",
    "tool_type": {
      "kind": "ModifyFile",
      "file_path": "path/to/file",
      "before": "old text",
      "after": "new text"
    }
  }
}
```

Completion must include:

```json
{
  "kind": "ToolExecutionCompleted",
  "data": {
    "tool_call_id": "id",
    "tool_name": "modify_file",
    "tool_result": { "kind": "ModifyFile", "lines_added": 10, "lines_removed": 4 },
    "success": true
  }
}
```

If one provider item edits multiple files, split into multiple logical calls (deterministic IDs like `item#1`, `item#2`, ...).

### File reads

- Request:
  - `tool_type.kind = "ReadFiles"`
  - `file_paths: string[]`
- Completion:
  - `tool_result.kind = "ReadFiles"`
  - `files: [{ path, bytes }]`

### Commands

- Request:
  - `tool_type.kind = "RunCommand"`
  - `command`, `working_directory`
- Completion:
  - `tool_result.kind = "RunCommand"`
  - `exit_code`, `stdout`, `stderr`

### Other tools

Unknown/provider-specific tools should map to `kind: "Other"` with raw args/result payloads.

### User-input gating / approvals

Use `tool_name: "ask_user_question"` for prompts requiring user response. This is used by agent/runtime UI state (`waiting_input`).

## Session Requirements

### Listing sessions

`SessionsList.data.sessions[]` should include:

- `id` and/or `session_id`
- `title`
- `last_modified` (seconds or ms accepted; ms preferred)
- optional: `created_at`, `last_message_preview`, `workspace_root`, `message_count`, `backend_kind`

### Resume session

- Emit `ConversationCleared`.
- Replay session history using `MessageAdded`.
- Replay tool history with `ToolRequest` + `ToolExecutionCompleted` when available.

### Delete session

- Delete backend session by `session_id`.
- Emit refreshed `SessionsList` or ensure caller can request it immediately.

## Agent Compatibility Requirements

Tyde agents are powered by normal conversation events. Backends must emit accurate status-driving events:

- `StreamStart`
- `ToolRequest`
- `ToolExecutionCompleted`
- `StreamEnd`
- `TypingStatusChanged`
- `OperationCancelled`
- `Error`
- `SubprocessExit`

If these are missing or misordered, agent status, `wait_for_agent`, and agent summaries become incorrect.

## Sub-Agent Spawning Semantics

The Tycode subprocess protocol does not define a dedicated `SubAgentSpawned`/`SubAgentCompleted` chat event. Canonical sub-agent lifecycle is represented through normal tool lifecycle events and turn boundaries.

Canonical Tycode behavior:

- Assistant emits `StreamStart` / `StreamDelta` / `StreamEnd`.
- If sub-agent operations are requested, the owning assistant `StreamEnd.data.message.tool_calls` contains tool calls such as `spawn_agent` or `complete_task`.
- After that `StreamEnd`, emit `ToolRequest` then `ToolExecutionCompleted` for each tool call.
- Emit the next assistant `StreamStart` only after all tool completions for the previous message are done.

This behavior is implemented in:

- `tycode-core/src/spawn/spawn_agent.rs` (Tycode repo)
- `tycode-core/src/spawn/complete_task.rs` (Tycode repo)
- `tycode-core/src/chat/tools.rs` (Tycode repo)
- `tycode-core/src/chat/ai.rs` (Tycode repo)

Provider mapping rule (important):

- Provider-internal delegation tools (for example Claude Code `Task`) are tool calls, not Tyde runtime agent lifecycle events.
- Map them as normal tool events (`ToolRequest`/`ToolExecutionCompleted`) with `kind: "Other"` unless there is a first-class typed mapping.
- Do not synthesize Tyde runtime sub-agent spawn/pop events from provider `Task` calls.
- Do not start a new stream turn to represent tool progress; keep tool progress attached to the most recent assistant `StreamEnd`.

`TaskUpdate` semantics:

- `TaskUpdate` is optional progress/status metadata.
- `TaskUpdate` does not replace `StreamStart`/`StreamEnd` or `ToolRequest`/`ToolExecutionCompleted`.
- `TaskUpdate` alone must not be interpreted as sub-agent spawn/completion.

## Admin Subprocess Requirements

Admin sessions use the same event schema as conversation sessions.

Core admin capabilities:

- list sessions
- delete session

Tycode-specific admin extensions:

- get/update settings
- list/switch profiles
- module schemas

## Conformance Checklist

- Core commands are implemented for the backend kind without crashing.
- Tycode-specific command extensions are either implemented or explicitly treated as optional no-ops.
- Events are valid `ChatEvent` JSON (plus `Subprocess*` wrappers).
- File-edit tool requests include `before`/`after` so diffs render.
- File-read and command tools use structured `ReadFiles`/`RunCommand` payloads.
- Every tool call emits matching request/completion IDs.
- Session list/resume/delete flows work per backend kind.
- Agent status transitions are correct from emitted events.
