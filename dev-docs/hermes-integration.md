# Hermes Backend Integration

Tyde's Hermes backend talks to Hermes through the same native gateway used by
Hermes's Ink TUI. The transport is deliberately narrow and Hermes-local:

```text
<hermes-python> -m tui_gateway.entry
newline-delimited JSON-RPC over stdio
```

Tyde does not drive Hermes through the dashboard WebSocket, a PTY, xterm, ANSI
parsing, plain text, or ACP fallback. If the gateway is missing, returns
malformed data, or omits a required field, the backend surfaces an explicit
Tyde error instead of guessing.

## Process selection

Local sessions resolve `<hermes-python>` using the order documented by the
Hermes TUI:

1. `HERMES_PYTHON`
2. `PYTHON`
3. `$VIRTUAL_ENV/bin/python`
4. `./.venv/bin/python`
5. `./venv/bin/python`
6. `python3`
7. `python`

Remote `ssh://host/path` workspaces spawn the same module remotely. The remote
interpreter defaults to `python3` and can be overridden with
`TYDE_REMOTE_HERMES_PYTHON`.

Startup waits for the gateway's `gateway.ready` event. The default startup
timeout is 15 seconds and can be overridden with
`HERMES_TUI_STARTUP_TIMEOUT_MS`. Individual JSON-RPC requests use
`HERMES_TUI_RPC_TIMEOUT_MS` and default to 120 seconds.

## JSON-RPC methods used

The MVP uses these native gateway methods:

- `session.create`
- `prompt.submit`
- `session.resume`
- `session.list`
- `session.history`
- `session.usage`
- `session.interrupt`
- `config.get`
- `config.set`
- `model.options`
- `approval.respond`

`session.create` seeds Tyde's combined system/read-only instructions through
Hermes history messages. `prompt.submit` requires a non-empty user message.
`session.usage` is sampled after `message.complete`; Tyde derives per-turn
usage deltas from the cumulative Hermes usage snapshot when the completion
event did not include usage.

## Event mapping

Hermes gateway events map to `ChatEvent` as follows:

| Hermes event | Tyde event |
| --- | --- |
| `message.start` | `StreamStart` |
| `message.delta` | `StreamDelta` |
| `message.complete` | `StreamEnd`, plus final typing/cancel state |
| `thinking.delta` / `reasoning.delta` | validated and suppressed; raw reasoning text is not emitted |
| `reasoning.available` | validated and suppressed; raw reasoning text is not emitted |
| `tool.start` | `ToolRequest(Other)` |
| `tool.progress` | `ToolProgress(Other)` |
| `tool.complete` | `ToolExecutionCompleted(Other)` |
| `approval.request` | `ToolRequest(ExitPlanMode)` |
| `session.info` | System readiness / credential warning messages |
| `status.update` | System status message |
| `error` | Error message and `TypingStatusChanged(false)` |

Missing required fields such as tool IDs, tool names, or session IDs are
treated as protocol errors and surfaced in the chat. `message.delta.text`
may be an empty string, which Tyde treats as a no-op. `message.complete.text`
is optional because Hermes can emit reasoning-only completions. Tyde closes the
stream and emits a visible warning/error when Hermes finishes without visible
assistant text, but does not render or store raw Hermes reasoning text.

## Session settings

Hermes session settings are server-owned and flow through Tyde's normal
`SessionSettingsSchema` surface:

- `model`: dynamic `Select` built from Hermes `model.options` authenticated
  provider rows. Model labels include provider context, and selected values are
  passed back to Hermes as per-session model/provider overrides.
- `reasoning_effort`: nullable `Select` using Hermes-supported
  `none`/`minimal`/`low`/`medium`/`high`/`xhigh`; Auto leaves the profile
  default untouched.
- `fast`: `Toggle` for Hermes fast service tier.

Tyde does not store Hermes API keys. Provider authentication remains owned by
Hermes (`~/.hermes/.env`, keychains, or provider-native auth); if Hermes cannot
report authenticated model options, Tyde marks the Hermes session schema
unavailable rather than inventing a model list.

## Backend configuration (deep, host-level)

Deep setup that is broader than the 2–3-knob session-settings bar lives in the
settings panel's **Backend Configuration** section, driven by a
`BackendConfigSchema` (the host-level sibling of `SessionSettingsSchema`, with a
richer field-type set — `Text`, `Secret`, plus `Select`/`Toggle`/`Integer`).
Values persist host-side in `HostSettings.backend_config` and apply to every new
session on that host; per-session settings still override where they overlap.

Hermes exposes three `Text` fields:

- `default_model`: model id every new session starts with. Supplied to
  `session.create` verbatim, so — unlike the session-settings `model` dropdown,
  which is built from a locally probed `model.options` list — it is also correct
  for remote `ssh://` workspaces whose authenticated providers differ from the
  local host.
- `default_provider`: provider slug for the default model.
- `api_base_url`: optional base URL override applied at session start.

API keys are intentionally **not** a Hermes config field: credentials remain
Hermes-owned by the design above. The framework supports a `Secret` field type
for backends that opt in, but Hermes does not persist keys through Tyde.

## Cancellation ordering

`session.interrupt` is cooperative. When Tyde cancels a turn it preserves the
agent protocol invariants:

1. close any open stream with `StreamEnd`
2. complete any open tools as cancelled
3. emit `OperationCancelled`
4. emit `TypingStatusChanged(false)`

If Hermes later sends an interrupted `message.complete` for the same turn, Tyde
absorbs it after the local cancellation sequence has already closed the stream.

## Explicitly deferred

- Image input is disabled until Hermes's native image contract is verified.
- MCP startup injection and custom MCP server configuration are rejected until
  Hermes gateway startup/tool policy parameters are verified.
- Custom Tyde tool policies are rejected unless they are representable by the
  verified Hermes gateway contract.
- Hermes delegation/subagent events currently surface as warnings. They are not
  projected into Tyde `SubAgentProgress` or first-class backend-native relay
  agents yet.
- API-key editing from Tyde is intentionally out of scope; credentials stay
  Hermes-owned. Default model/provider and base URL are configurable via the
  Backend Configuration surface above.
