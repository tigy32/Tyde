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

Local sessions use explicit overrides authoritatively before auto-discovery:

1. `HERMES_PYTHON`, when set, must point at a Python interpreter that can import
   `tui_gateway.entry`. If it cannot, Hermes setup is reported as unavailable
   with a typed diagnostic and runtime launch fails with the same cause; Tyde
   does not fall back to another executable.
2. `HERMES_EXECUTABLE`, when set, must point at a Hermes CLI whose `--version`
   output reports a `Project:` root and whose wrapper/console script can be
   traced to a Python interpreter that can import `tui_gateway.entry`. If this
   probe fails, Tyde reports the explicit failure instead of falling back to
   PATH or Python.
3. Without explicit overrides, Tyde probes the Hermes CLI discovered from
   common user-bin locations such as `~/.local/bin` and the resolved host PATH.

For executable probes, Tyde runs `hermes --version`, reads Hermes's reported
project root, resolves the gateway Python from the Hermes executable itself
(shell wrapper targets and console-script shebangs) and from legacy project
virtualenv locations when present, then verifies that interpreter can import
`tui_gateway.entry`. Tyde launches the verified interpreter as:

```text
<resolved-hermes-python> -m tui_gateway.entry
```

If no explicit override or verified CLI is available, setup reports Hermes as
not installed with instructions to install Hermes or set `HERMES_EXECUTABLE` or
`HERMES_PYTHON`. Tyde does not infer Hermes from `PYTHON`, `VIRTUAL_ENV`,
workspace `.venv`/`venv`, `python3`, or `python`; those interpreters are only
used when explicitly named by `HERMES_PYTHON` or when the verified Hermes CLI
wrapper/shebang itself names them.

Setup diagnostics distinguish subprocess failures, probe timeouts, nonzero
`--version`, missing `Project:`, failure to resolve a gateway Python from the
found CLI, and gateway import failures. A found CLI whose `--version` succeeds
but whose gateway Python cannot be resolved or cannot import
`tui_gateway.entry` is reported as unavailable, not not-installed; the
diagnostic names the executable, version, and project when known and describes
the gateway-Python/import problem. Its remedy is to re-run the Hermes installer
to restore Hermes's Python environment, or set `HERMES_PYTHON` to a Python
interpreter that can import `tui_gateway.entry`; it does not tell the user to
put `hermes` on PATH or set `HERMES_EXECUTABLE` for a CLI that was already
found. The setup sign-in command uses the resolved Hermes executable path when
Hermes was verified through `HERMES_EXECUTABLE` or PATH; if Hermes is only
available through `HERMES_PYTHON`, Tyde does not invent a `hermes setup`
command.

Remote `ssh://host/path` workspaces spawn the same module remotely. The remote
interpreter defaults to `python3` and can be overridden with
`TYDE_REMOTE_HERMES_PYTHON`.

Startup waits for the gateway's `gateway.ready` event. The default startup
timeout is 15 seconds and can be overridden with
`HERMES_TUI_STARTUP_TIMEOUT_MS`. Individual JSON-RPC requests use
`HERMES_TUI_RPC_TIMEOUT_MS` and default to 120 seconds.

## Profiles

Hermes's effective root is `HERMES_HOME` when set and the platform Hermes home
otherwise. The root is the `default` profile. Named profiles are independent
Hermes homes under `<root>/profiles/<name>`, where names follow
`^[a-z0-9][a-z0-9_-]{0,63}$`.

Tyde discovers the root and every valid named directory. It publishes
`hermes:default` plus `hermes:profile:<name>` launch profiles, and carries the
selected native profile as an immutable per-session setting. A named session
spawns the gateway and MCP-registration subprocess with that profile
directory as `HERMES_HOME`; a missing or failed named profile stays visible as
an unavailable launch option with its probe error.

Named profiles are local-only. An SSH-backed workspace cannot safely forward a
local Hermes profile directory, so Tyde rejects that combination rather than
silently launching the remote default. A running session cannot change
profiles; start a new session to select another profile.

## MCP startup injection

Hermes discovers MCP servers from `mcp_servers` in its native configuration,
not from `session.create`. For a session with Tyde-provided MCP servers, Tyde
therefore starts the same verified Hermes Python with a process-local bootstrap
instead of `-m tui_gateway.entry`. The bootstrap reads one private JSON line
from the gateway's stdin, overlays those servers onto the configuration returned
by `hermes_cli.config`, and then runs `tui_gateway.entry` in the same process.
The rest of stdin remains the normal newline-delimited gateway JSON-RPC stream.

This keeps Hermes as the MCP client and preserves its native configuration
validation, environment interpolation, server discovery, tool registration,
and reload. The bootstrap also adds the session-provided server names to the
gateway's process-local enabled toolset selection, so a user's native
`no_mcp` preference continues to apply to native servers without hiding the MCP
servers explicitly selected for the Tyde session. Stdio servers map to Hermes
`command`/`args`/`env` entries. HTTP servers map to `url`/`headers`; a Tyde
`bearer_token_env_var` becomes an `Authorization: Bearer ${ENV_VAR}` template so
Hermes resolves the token from its own environment or secret scope.

The overlay keeps the session's effective `HERMES_HOME`. In an ordinary
production launch that is the user's selected Hermes home, so its credentials,
state database, memories, and user-configured MCP servers remain available. In
an explicitly contained dev instance it is the attested disposable home
described below. Tyde-provided entries win a same-name collision inside that
gateway process. Wrapped Hermes save operations restore the native MCP section
before writing, so session-only entries never leak into the selected
`config.yaml`. The bootstrap does not put the injected configuration or header
values in process arguments or environment variables. The same stdin bootstrap
works through the remote SSH transport.

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
`HostSettings.backend_config` stores only explicit Tyde-managed overrides.
Backend-native current settings are server-owned snapshots emitted separately as
`BackendConfigSnapshots` on the host stream, so the UI can render the backend's
current provider/model state without treating a form read as a destructive save.

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
Hermes snapshots currently map `model.options.model` to `default_model` and
`model.options.provider` to `default_provider`; `api_base_url` remains an
explicit Tyde override because the native gateway does not expose a verified
read contract for that value here.

## Contained dev instances and loopback stubs

`tyde_dev_instance_start` preserves its historical behavior when its optional
`hermes` input is absent. Such a dev instance inherits the launching process's
`HOME` and `HERMES_HOME` and is **not** safe for destructive Hermes QA merely
because its Tyde stores are ephemeral.

The typed opt-in requires `loopbackStub`. Before redirecting `HOME`, the parent
launcher resolves and canonicalizes the Hermes executable and an explicitly
configured `HERMES_PYTHON`, expanding `$HOME` launcher chains for both against
the parent home; start fails closed if neither is usable. It still resolves the
Hermes executable when `HERMES_PYTHON` is explicit so both runtime surfaces are
attested. It then creates a fresh home inside the instance store, exports
canonical `HOME` and `HERMES_HOME` plus final runtime invocation paths, seeds
requested named profiles, and returns those values in `hermesEnvironment` with
derived `homeEphemeral` and `hermesHomeEphemeral` facts. Runtime attestations
also include `resolvedHermesExecutable` / `resolvedHermesPython` canonical
targets. The exported Python invocation path intentionally retains a
virtual-environment symlink because replacing it with the base interpreter can
lose that environment's imports. Stop removes the entire instance store.

Resolving a launcher chain to a bare `HERMES_EXECUTABLE` or `HERMES_PYTHON`
cannot reproduce shell-wrapper environment mutations. The attestation reports
the full `hermesLauncherChain` / `hermesPythonLauncherChain`, the
`skippedLaunchers` / `skippedPythonLaunchers`, and
`launcherEnvironmentPreserved`. When the last value is `false`, containment is
still valid, but wrapper-provided toolset, tool-progress, `PYTHONPATH`, and
network-guard behavior was not retained. Do not cite that run as
production-wrapper fidelity evidence for findings sensitive to those settings.

The contained no-paid-call mode:

- accepts only `http://127.0.0.1:<port>` with an explicit port and no URL
  credentials, query, or fragment;
- writes a synthetic OpenAI-compatible model configuration and fake local key
  into the disposable default and named profiles and sets
  `bedrock.discovery.enabled: false`;
- removes inherited provider credential, token, credential-file, endpoint,
  base-URL, organization, project, and cloud-profile environment surfaces;
- redirects Tyde's configured-host store with the other mutable stores; and
- routes HTTP(S) proxy traffic to a closed loopback endpoint while exempting
  only `127.0.0.1`.

The returned network policy is `loopback_stub_only`. This is a fail-closed
provider-testing configuration, not a general OS network sandbox for arbitrary
tools or subprocesses. The stub must already be listening on the supplied
loopback URL. There is no contained no-stub or inherited-network escape hatch.
It proves Tyde/Hermes request structure and lifecycle without provider spend;
it does not verify a production provider's exact tool event order,
cache/context accounting, reasoning fields, error body, latency, or billing.
Those claims require a separately approved live call.

The reference contract is OpenAI-compatible `GET /v1/models` and
`POST /v1/chat/completions`, including normal JSON for `stream: false` and
valid SSE chunks ending in `[DONE]` for `stream: true`. Agent creation always
sends an automatic naming completion before the agent exists and before the
user's visible turn. The stub's first completion must return a non-empty short
assistant name and a valid usage object; only later completions should consume
the scripted turn responses. Keep supervisor helpers off unless the stub also
scripts all helper completions. This request ordering is part of the fixture
contract, not proof that a paid provider emits identical streaming or usage
metadata.

Example:

```json
{
  "project_dir": "/path/to/Tyde2",
  "hermes": {
    "profiles": ["qa"],
    "loopbackStub": {
      "baseUrl": "http://127.0.0.1:43123/v1",
      "model": "tyde-stub"
    }
  }
}
```

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
- Custom Tyde tool policies are rejected unless they are representable by the
  verified Hermes gateway contract.
- Hermes delegation/subagent events currently surface as warnings. They are not
  projected into Tyde `SubAgentProgress` or first-class backend-native relay
  agents yet.
- Named-profile provider disconnect is disabled because the current Hermes RPC
  cannot prove that it will mutate only the selected profile. Tyde does not
  fall back to a default-home disconnect. Credentials stay Hermes-owned;
  default model/provider and base URL remain configurable through Backend
  Configuration.
- Screenshot capture, a second UI client, native confirmation-dialog control,
  and viewport resizing are not currently exposed by the dev-instance debug
  protocol. Runtime QA must report those checks blocked rather than infer a
  pass from DOM state.
