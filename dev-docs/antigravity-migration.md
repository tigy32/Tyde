# Antigravity Migration

I re-read `AGENTS.md`, `tests/TESTING.md`, and
`dev-docs/01-philosophy.md` before updating this document. This is the design
for fully removing the Gemini CLI backend and replacing it with Google's
Antigravity CLI (`agy`). Implementation happens in a later phase.

---

## A. `agy` contract

These facts come from empirical probes against the real binary installed on the
host.

### Binary and invocation

- Binary: `~/.local/bin/agy`
- Version: `1.0.6`
- Headless one-shot invocation works from a normal non-interactive subprocess;
  no PTY is required for `--print` / `-p`.
- Run with `cwd` set to the primary workspace root.
- Pass one `--add-dir <path>` for each additional workspace root.
- Always set `--print-timeout`; invalid model names were observed to hang.

Validated invocation shape:

```text
~/.local/bin/agy \
    --model '<exact model label>' \
    [--sandbox] \
    [--dangerously-skip-permissions] \
    --print-timeout <duration> \
    --add-dir <extra-root> \
    -p '<prompt>'
```

Example readiness probe shape:

```text
~/.local/bin/agy \
    --model 'Gemini 3.5 Flash (Low)' \
    --print-timeout 30s \
    --dangerously-skip-permissions \
    -p 'Reply exactly with ok'
```

Tyde must **not** use Antigravity native conversation resume for `agy` `1.0.6`.
The CLI does not emit a structured session/conversation id comparable to
Gemini's `SessionStarted.session_id`; scraping `~/.gemini` logs or
`last_conversations.json` would be inference, races under concurrent agents, and
violates `01-philosophy.md`. See "Spawn contract and session identity" below for
the Tyde-owned session mapping.

### Output contract

`agy -p` writes **plain text** to stdout. It does not emit JSON, NDJSON,
`stream-json`, or ACP/JSON-RPC events in version `1.0.6`.

Observed behavior:

- Model narration lines and final answer text are interleaved on stdout.
- Lines may arrive incrementally before process exit, but they are prose, not
  typed events.
- Process exit is the only reliable stream-end boundary.
- Stderr was empty in normal probes.

Unsupported / absent in `1.0.6`:

```text
--output-format stream-json
--acp
ACP JSON-RPC over stdio
NDJSON event stream
structured tool-call events
```

Tyde must not invent structured semantics from prose. The Antigravity backend can
expose assistant text streaming, but it cannot expose reliable `ToolRequest` /
tool-completion events until `agy` provides structured output.

### Auth

Authenticated runs use existing on-disk Antigravity OAuth/keyring state under
`~/.gemini`. No API-key environment variable path was found to work.

Probes with clean `HOME` and fake `GEMINI_API_KEY`, `GOOGLE_API_KEY`, and
`ANTIGRAVITY_API_KEY` all produced the same OAuth flow. Unauthenticated stdout
contains an OAuth URL and then times out:

```text
Authentication required. Please visit the URL to log in:
  https://accounts.google.com/o/oauth2/auth?...
Waiting for authentication (timeout 30s)...
Error: authentication timed out.
```

Tyde setup should require the user to sign in with `agy` on the host. If a probe
sees `Authentication required`, surface that as a setup/auth error; do not try an
API-key fallback.

### Models

`agy models` returned these model labels. Ship them verbatim, including
parentheses:

- `Gemini 3.5 Flash (Low)`
- `Gemini 3.5 Flash (Medium)`
- `Gemini 3.5 Flash (High)`
- `Gemini 3.1 Pro (Low)`
- `Gemini 3.1 Pro (High)`
- `Claude Sonnet 4.6 (Thinking)`
- `Claude Opus 4.6 (Thinking)`
- `GPT-OSS 120B (Medium)`

These labels are accepted by `--model '<exact name>'`. Because invalid model
names can hang, every invocation must include `--print-timeout <duration>`.

### Permissions and sandboxing

`agy --help` exposes only these relevant permission/sandbox flags:

```text
--sandbox                       Run in a sandbox with terminal restrictions enabled
--dangerously-skip-permissions  Auto-approve all tool permission requests
```

Empirical probes showed `--sandbox` is **not** a true read-only mode. It still
allowed shell writes in and outside the workspace. Therefore Tyde must not map
`BackendAccessMode::ReadOnly` to `--sandbox` and claim the read-only contract is
honored.

Design decision for Antigravity `ReadOnly`:

- Fail closed for `BackendAccessMode::ReadOnly` with a clear backend error.
- Explain that Antigravity CLI `1.0.6` has no enforceable read-only mode.
- Keep `--sandbox` available only as an optional terminal restriction in future
  work, not as Tyde's read-only enforcement.

This follows `dev-docs/20-backend-access-mode.md`: a backend must either honor
`ReadOnly` or reject it; it must never silently downgrade to unrestricted.

For `BackendAccessMode::Unrestricted`, pass `--dangerously-skip-permissions` so
headless turns do not block on interactive approvals.

### Sessions, resume, and interrupt

Native Antigravity resume is **unsupported** in Tyde for `agy` `1.0.6`.

Tyde should use one-shot `agy -p` subprocess turns only:

- Mint Tyde-owned `SessionId` values; do not derive them from Antigravity files.
- Do not read or scrape `~/.gemini/antigravity-cli/conversations`,
  `~/.gemini/antigravity-cli/cache/last_conversations.json`, or logs.
- Do not persist stdout prefixes, hashes, cursors, or replay-diff state in
  `sessions.json`.
- Mark Antigravity `BackendSession` summaries as `resumable: false`.
- `resume` and `fork` should fail explicitly as unsupported for Antigravity
  until `agy` emits a structured native session id and a non-replaying resume
  contract.

Live follow-up turns, if supported by the backend actor, must be
Tyde-managed-context turns: the server-owned actor may keep a bounded in-memory
transcript for the currently live session and include that context in the next
one-shot prompt. That state is not a native Antigravity session and is not a
persisted replay cache.

`--print-timeout <duration>` exits print mode and writes a plain-text error such
as:

```text
Error: timed out waiting for response
```

Process-group SIGINT/SIGTERM probes also exited without a structured cancel
protocol. Tyde should implement interrupt by terminating the active process group
and emitting Tyde's own cancellation/error events. Do not treat exit code `0` as
success if stdout contains an Antigravity `Error:` line.

### MCP config

Antigravity CLI `1.0.6` reads MCP config from this global path:

```text
~/.gemini/config/mcp_config.json
```

It does **not** use the old Gemini per-workspace `.gemini/settings.json` path for
Tyde's startup MCP injection.

A valid stdio server shape is:

```json
{
  "mcpServers": {
    "tyde_probe": {
      "command": "/path/to/server",
      "args": [],
      "env": {
        "TYDE_MCP_PROBE": "yes"
      }
    }
  }
}
```

Antigravity initialized that server with newline-delimited JSON-RPC:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"antigravity-client","version":"v1.0.0"},"protocolVersion":"2025-11-25","capabilities":{"elicitation":{"form":{},"url":{}},"roots":{"listChanged":true}}}}
{"jsonrpc":"2.0","method":"notifications/roots/list_changed","params":{}}
```

Binary strings and docs indicate HTTP MCP entries use `serverUrl` and may carry
`headers`.

---

## B. Touchpoint map

Line numbers are from the investigation snapshot and are approximate anchors for
the implementation phase. Implementation must begin and end with this grep over
compiled non-`old/` code to catch any remaining string, enum arm, test fixture,
or docs reference that exhaustive matches do not catch:

```text
rg -n --glob '!old/**' --glob '!target/**' 'BackendKind::Gemini|"gemini"|Gemini' \
  protocol server frontend mobile-frontend dev-driver tests dev-docs
```

Exhaustive-match build failures will catch many Rust enum arms, but this grep is
still required because persisted strings, test fixtures, labels, and docs are not
all protected by the compiler.

### Protocol

- `protocol/src/types.rs:354-359` — `BackendKind` enum currently includes
  `Gemini`; replace it with `Antigravity`.
- `protocol/src/types.rs:363-366` — `supports_image_input`; Gemini already
  returns `false`, and Antigravity should also return `false` because `agy -p`
  exposes no validated image-input contract through Tyde. This is no behavior
  change at the chat input image gate for this backend.

### Server backend registration and dispatch

- `server/src/backend/mod.rs:4` — `pub mod gemini`; replace with
  `pub mod antigravity`.
- `server/src/backend/mod.rs:264` — session schema match arm calls
  `gemini::GeminiBackend::session_settings_schema()`.
- `server/src/backend/mod.rs:277` — session settings resolution arm calls
  `gemini::resolve_session_settings(config)`.
- `server/src/backend/mod.rs:343` — tier defaults arm calls
  `gemini::gemini_cost_hint_defaults`.

### Server setup / availability

- `server/src/backend/setup.rs:29` — `GEMINI_CLI_CANDIDATES` should become
  Antigravity candidates (`agy`, with `~/.local/bin/agy` first if the setup code
  supports explicit paths).
- `server/src/backend/setup.rs:35-40` — setup iteration list includes
  `BackendKind::Gemini`.
- `server/src/backend/setup.rs:109` — probe match arm uses Gemini candidates.
- `server/src/backend/setup.rs:325` — Gemini docs URL.
- `server/src/backend/setup.rs:360-368` — Gemini npm install command.
- `server/src/backend/setup.rs:396-399` — Gemini sign-in command.

### Server exports, host tests, and agent dispatch

- `server/src/lib.rs:25` — public re-export includes `gemini`; replace with
  `antigravity`.
- `server/src/host.rs:8939-8942` —
  `resolve_ai_reviewer_backend_kind_respects_override` asserts an explicit
  `BackendKind::Gemini` override round-trips.
- `server/src/agent/mod.rs:20` — imports `GeminiBackend`; replace with
  `AntigravityBackend`.
- `server/src/agent/mod.rs:532-536` — spawn match arm for Gemini.
- `server/src/agent/mod.rs:579-582` — resume match arm for Gemini.
- `server/src/agent/mod.rs:644-649` — fork match arm for Gemini.

### Agent-control MCP / dev-driver schema

- `server/src/agent_control_mcp.rs:67-72` — `BackendKindInput` includes
  `Gemini`.
- `server/src/agent_control_mcp.rs:82` — maps `BackendKindInput::Gemini` to
  `BackendKind::Gemini`.
- `dev-driver/src/agent_control.rs:935-950` — dev-driver `BackendKindInput` also
  includes Gemini.
- `dev-driver/src/agent_control.rs:1260-1264` — JSON schema enum includes
  `gemini`.

### Stores and migrations

- `server/src/store/settings.rs:7-12` — `CANONICAL_BACKENDS` is hardcoded to
  length `5` and includes `BackendKind::Gemini`.
- `server/src/store/settings.rs:55-62` — settings store currently strictly
  deserializes typed `HostSettings`; unknown backend variants will fail.
- `server/src/store/settings.rs:173-209` — validation/canonicalization requires
  `default_backend` to be present in `enabled_backends` and must run only after
  the lenient Gemini-to-Antigravity migration.
- `server/src/store/session.rs:20` — `SessionRecord.backend_kind` is required
  and typed as `BackendKind`.
- `server/src/store/session.rs:350-355` — `read_from_disk` strictly deserializes
  the whole `sessions.json`; removing Gemini without a pre-pass would break the
  entire store if one Gemini record remains.
- `server/src/store/agent_teams.rs:23-26` — validation refs include enabled
  backend kinds and legacy backend kind used by migrations.
- `server/src/store/agent_teams.rs:669` — validation checks each
  `TeamMember.backend_kind` against enabled backends.
- `server/src/store/agent_teams.rs:723-767` — `migrate_store_file` parses the
  whole agent-teams JSON value and then strict-deserializes `AgentTeamsStoreFile`.
- `server/src/store/agent_teams.rs:944-957` — legacy migration inserts a typed
  `backend_kind` into members; a persisted `"gemini"` member would break strict
  deserialization after the enum variant is removed.

### Gemini backend file

- `server/src/backend/gemini.rs` — remove the Gemini implementation. The new
  `server/src/backend/antigravity.rs` can mirror the actor structure but must
  replace Gemini's NDJSON parser and `.gemini/settings.json` MCP injection.
- `server/src/backend/gemini.rs:1650-1656` — Gemini receives a structured
  `SessionStarted.session_id`; Antigravity has no equivalent, so the new backend
  must mint its own Tyde `SessionId` at spawn time instead of scraping native
  files.
- `server/src/backend/gemini.rs:1660` — Gemini resolves `ready_tx` with the
  native session id; Antigravity must resolve `ready_tx` with the Tyde-minted id
  after its actor is constructed and before the first `agy -p` process starts.
- `server/src/backend/gemini.rs:2083-2108` — in-file Gemini arg/permission tests
  must be replaced by Antigravity arg construction and read-only fail-closed
  unit tests.

### Tests

- `tests/tests/backend.rs:44` — real-backend binary availability probes
  `gemini`.
- `tests/tests/backend.rs:82` — runtime availability treats Gemini like Claude
  and Kiro.
- `tests/tests/backend.rs:142-153` — Gemini readiness probe uses
  `gemini -y ... --output-format stream-json`.
- `tests/tests/backend.rs:249` — backend label returns `gemini`.
- `tests/tests/backend.rs:1309` and `1616` — Gemini-specific exceptions for
  unstable stream deltas.
- `tests/tests/backend.rs:2049-2051` — Gemini-specific interrupt prompt.
- `tests/tests/backend.rs:2177` and `2224` — real-backend lists include Gemini.
- `tests/tests/backend.rs:2230` — `real_backends_emit_stream_deltas` skips
  Gemini because live Gemini deltas were unstable.
- `tests/tests/backend.rs:2276` — `real_backends_emit_typing_status` list
  includes Gemini.
- `tests/tests/custom_agents.rs:928-929` — non-Claude tool-policy rejection case
  uses Gemini.
- `tests/tests/settings.rs:165` — persisted backend list includes `gemini`.
- `tests/tests/settings.rs:177` — expected canonical list includes
  `BackendKind::Gemini`.

### Dev docs

- `dev-docs/20-backend-access-mode.md:92-103` — replace the `### Gemini`
  section with `### Antigravity`, documenting that `agy` `1.0.6` has no true
  read-only mode and Tyde fails closed for read-only Antigravity sessions.

### Frontend / mobile touchpoints

These are owned by the frontend implementation phase, but the protocol enum
change will make their matches non-exhaustive until updated.

- `frontend/styles.css:1492-1495` — `.backend-badge.gemini`.
- `frontend/src/components/home_view.rs:22,360,370` — Gemini label/badge arms.
- `frontend/src/components/chat_view.rs:700` — Gemini label/badge arm.
- `frontend/src/components/center_zone.rs:45` — Gemini label arm.
- `frontend/src/components/agents_panel.rs:58,68` — Gemini badge/label arms.
- `frontend/src/components/sessions_panel.rs:18,28` — Gemini badge/label arms.
- `frontend/src/components/session_settings.rs:322,333,343` — Gemini settings
  header/error/loading text.
- `frontend/src/components/settings_panel.rs:615` — settings search text.
- `frontend/src/components/settings_panel.rs:2606-2614` — `all_backends()`
  hardcoded backend list.
- `frontend/src/components/settings_panel.rs:2622,2633,2643,2653,2663` —
  parse/value/label/description/badge arms for Gemini.
- `frontend/src/components/review_view.rs:1025,1122,3598-3602` — label/parser
  arms and Gemini test fixture.
- `frontend/src/components/teams_panel.rs:2216,2226,2236` — team backend
  label/value/parser arms.
- `frontend/src/components/chat_input.rs:808,881,887` — image gate calls
  `BackendKind::supports_image_input`; Gemini is already false and
  Antigravity=false preserves that closed gate.
- `frontend/src/state.rs:2706-2707` — host settings seed/test fixture uses
  Gemini.
- `mobile-frontend/src/components/diff_viewer.rs:2126-2161` — mobile test
  fixtures use Gemini.

---

## C. Implementation design

### Protocol model

Replace `BackendKind::Gemini` with:

```rust
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Tycode,
    Kiro,
    Claude,
    Codex,
    Antigravity,
}
```

`BackendKind::Antigravity` should serialize as `"antigravity"`.

`supports_image_input` should return `false` for Antigravity. Gemini already
returns `false`, so this is not a chat-input behavior regression; it preserves
the existing closed image gate for the backend being replaced. Although
Antigravity as a product may support multimodal features, the validated headless
`agy -p` contract in this phase is plain-text prompt in, plain-text stdout out.
Tyde should not expose image upload until there is a validated headless
image-input path.

### Backend module

Create `server/src/backend/antigravity.rs`. It should mirror the broad shape of
`gemini.rs` where that shape still fits Tyde's backend actor model:

- `AntigravityBackend` implementing `Backend`.
- `spawn`, `resume`, and `fork` entry points, with `resume` and `fork` returning
  explicit unsupported errors.
- An input channel for `AgentInput`.
- An interrupt channel that terminates the active child process group.
- A spawn-ready timeout so startup cannot hang indefinitely.
- Session settings schema and cost-hint defaults in the backend module.

Do not copy Gemini's NDJSON parser, native session-id handling, or old MCP config
injection. Antigravity uses plain stdout, Tyde-owned session ids, and a global
MCP config file.

Recommended constants:

```rust
const ANTIGRAVITY_AGENT_NAME: &str = "antigravity";
const ANTIGRAVITY_SPAWN_TIMEOUT: Duration = Duration::from_secs(120);
const ANTIGRAVITY_PRINT_TIMEOUT: &str = "5m";
```

Arg construction should be unit-tested. For unrestricted turns:

```text
agy --print-timeout 5m \
    --dangerously-skip-permissions \
    --model '<exact-model-label>' \
    --add-dir '<extra-root>' \
    -p '<combined prompt>'
```

Do not add native conversation flags. There is no supported native resume path
for `agy` `1.0.6` in Tyde.

### Spawn contract and session identity

The `Backend::spawn` trait contract needs a `SessionId` at spawn time. Gemini can
wait for a native `SessionStarted.session_id`; Antigravity cannot. Therefore
Antigravity uses a Tyde-owned session identity minted at spawn:

1. Validate fail-fast inputs before launching any child:
   - `access_mode` must not be `ReadOnly`.
   - resolved model must be one of the exact known labels.
   - workspace roots must resolve to the primary `cwd` plus `--add-dir` extras.
2. Mint a Tyde session id, for example
   `SessionId(format!("antigravity-{}", Uuid::new_v4()))`.
3. Construct the backend actor, event stream, input channel, and interrupt
   channel.
4. Resolve `ready_tx` with the Tyde-minted `SessionId` once the actor is ready to
   accept input. This is the Antigravity spawn-ready point; it is not based on an
   Antigravity event.
5. Launch the first one-shot `agy -p` turn after the ready handshake.

The user-visible stream is then prose-only:

- The existing agent layer emits its normal new-agent / agent-start lifecycle
  events after spawn readiness.
- The Antigravity backend emits the first assistant `StreamStart` only when the
  `agy` subprocess produces the first assistant stdout text chunk.
- Process exit marks stream end.

Session capability rules:

- `Backend::session_id()` returns the Tyde-minted id.
- `BackendSession.resumable` must be `false`.
- `list_sessions()` should return `Ok(Vec::new())` for Antigravity because there
  are no safe native sessions to expose.
- `resume(session_id, ...)` returns an explicit unsupported error. It must not
  try to find a native Antigravity conversation with the same or similar id.
- `fork(session_id, ...)` returns an explicit unsupported error for the same
  reason.

### Tyde-managed context for live turns

Because native resume is unsupported, follow-up turns in an already-running
Antigravity actor must be one-shot Tyde-managed-context turns.

Concrete behavior:

- The actor keeps a bounded in-memory transcript for the live session: user text
  submitted through Tyde and assistant text emitted from plain stdout.
- Each new `AgentInput` builds a combined prompt containing that bounded context
  plus the new user message.
- The transcript is server-owned runtime state, not frontend state, and is not a
  native Antigravity session.
- The transcript is not written to `sessions.json`; after process/app restart the
  Antigravity session is non-resumable and must not be offered as resumable.

If the implementation cannot safely build a bounded context prompt within the
existing backend trait, fail follow-up sends visibly rather than silently sending
a contextless prompt. Do not add fallback native resume.

### Chat event mapping

Because `agy` provides no structured event stream, the Antigravity backend must
map only what is directly observable:

1. Before launching the child, emit the Tyde user message through the existing
   backend emitter path.
2. Read stdout chunks as plain text.
3. On the first non-empty stdout text that is not an Antigravity `Error:` line,
   emit `StreamStart` for an assistant message.
4. Emit each stdout chunk as `StreamDelta` / `StreamTextDelta`, preserving
   newlines enough for readable output.
5. On process exit, emit `StreamEnd` if an assistant stream was started and the
   turn was not cancelled or failed.
6. If the process exits without assistant text, emit a visible backend error.
7. If stdout begins with `Authentication required` or `Error:`, surface that as
   a backend error with the captured text.
8. On interrupt, terminate the child process group and emit Tyde's cancellation
   outcome; do not rely on Antigravity's exit code to decide whether the turn was
   semantically cancelled.

`ToolRequest`, `ToolResult`, permission prompts, and typed tool cards are not
representable from `agy -p` in version `1.0.6`. Do not scrape prose such as
"I will run..." into fake tool events. That would violate the no-fallback and
server-owned-behavior rules from `01-philosophy.md`.

### Access mode mapping

`BackendAccessMode::Unrestricted`:

- Pass `--dangerously-skip-permissions`.
- Optionally pass `--sandbox` only if a future product decision wants terminal
  restrictions, but do not treat it as a security boundary.

`BackendAccessMode::ReadOnly`:

- Fail closed before launching `agy`.
- Emit a clear backend startup/turn error, for example:

```text
Antigravity CLI 1.0.6 has no enforceable read-only mode; refusing read_only spawn
```

This is the safest viable way to honor Tyde's read-only contract today. A later
implementation could add a real external OS sandbox and then enable read-only
spawns, but that is out of scope for the Gemini-removal replacement.

### Auth and setup

Setup should probe for `agy` instead of `gemini`.

Install command:

```text
curl -fsSL https://antigravity.google/cli/install.sh | bash
```

Sign-in command:

```text
agy
```

Readiness probe should run a bounded print-mode command in a temp workspace:

```text
agy --model 'Gemini 3.5 Flash (Low)' \
    --print-timeout 30s \
    --dangerously-skip-permissions \
    -p 'Reply exactly with ok'
```

If stdout contains `Authentication required`, report an auth/setup failure. If
stdout contains `Error: timed out waiting for response`, report timeout. Do not
retry with API-key environment variables.

### Model-list wiring

`AntigravityBackend::session_settings_schema()` should emit one non-null select
field:

```rust
SessionSettingField {
    key: "model".to_string(),
    label: "Model".to_string(),
    description: None,
    use_slider: false,
    field_type: SessionSettingFieldType::Select {
        options: antigravity_known_models(),
        default: Some("Gemini 3.5 Flash (Medium)".to_string()),
        nullable: false,
    },
}
```

`antigravity_known_models()` should return the exact labels from the validated
`agy models` output. The select option `value` should equal the exact CLI model
label because `--model` accepts that label.

Cost-hint defaults:

- `Low` -> `Gemini 3.5 Flash (Low)`
- `Medium` -> `Gemini 3.5 Flash (Medium)`
- `High` -> `Gemini 3.1 Pro (High)`

The resolved model is always passed to `agy --model '<exact label>'`. If a stored
or incoming session setting lacks a model, resolve it to the schema default
`Gemini 3.5 Flash (Medium)` before constructing argv.

### MCP wiring

Antigravity's MCP file is global, so Tyde must serialize Antigravity turns that
inject startup MCP servers.

Implementation outline:

1. Protect Antigravity MCP config mutation with a process-wide async mutex.
2. Read exact original bytes from `~/.gemini/config/mcp_config.json`, if the
   file exists.
3. Parse existing JSON and preserve unrelated servers. If the file is malformed,
   fail the spawn/turn with a visible error; do not overwrite it.
4. Merge Tyde startup MCP servers into `mcpServers` using deterministic
   namespaced keys that cannot collide with user servers, for example
   `tyde_<session-id>_<server-name>`.
5. Write the merged config atomically.
6. Run `agy`.
7. Restore the exact original bytes, or remove the file if it did not exist.
8. Restore in every exit path: success, process failure, timeout, and interrupt.

MCP config conversion:

- Stdio: `{ "command": ..., "args": [...], "env": {...} }`
- HTTP: `{ "serverUrl": ..., "headers": {...} }`

Do not write old Gemini workspace `.gemini/settings.json`; Antigravity did not
use it in the probes.

### Native Antigravity resume is unsupported

There must be no native Antigravity session tracking in Tyde for `agy` `1.0.6`.
Specifically:

- Do not scrape logs, SQLite files, or `last_conversations.json`.
- Do not call native resume flags from Tyde.
- Do not persist emitted stdout prefixes, hashes, cursors, or replay-diff state.
- Do not try to correlate Tyde session ids with Antigravity native UUIDs.

This removes the growing transcript cache proposed in the earlier design and
avoids races between concurrent agents. The supported design is Tyde-minted,
non-resumable session ids plus one-shot `agy -p` turns with optional bounded
in-memory context while the actor is live.

### Session cleanup / migration

Removing `BackendKind::Gemini` breaks strict deserialization anywhere persisted
JSON still contains `"gemini"`. The migration must be one-shot at store `load()`
time, and `read_from_disk` must remain strict afterward because it is called on
every mutation.

#### `sessions.json`

`~/.tyde/sessions.json` uses a `StoreFile` wrapper:

```json
{
  "records": {
    "session-id": { "backend_kind": "gemini" }
  }
}
```

Exact session-store migration:

1. In `SessionStore::load`, read the file as `serde_json::Value` before calling
   strict `read_from_disk`.
2. Descend into the wrapper's `records` object; do not treat the top-level JSON
   as the records map.
3. Remove every record whose `backend_kind` is exactly `"gemini"`.
4. Collect the removed session ids in a `HashSet<SessionId>` for the agent-teams
   migration.
5. Rewrite `sessions.json` atomically if any records were removed.
6. Then call the existing strict `read_from_disk` path.
7. If a remaining record has an unknown backend kind, return the existing parse
   error; do not silently drop arbitrary unknown variants.

Effect: persisted Gemini sessions disappear on upgrade, and all non-Gemini
sessions survive. This intentionally differs from settings migration: sessions
are purged because the native Gemini/Antigravity session state is incompatible.

#### `settings.json`

`~/.tyde/settings.json` also uses a `StoreFile` wrapper:

```json
{
  "settings": {
    "enabled_backends": ["gemini"],
    "default_backend": "gemini",
    "backend_tier_configs": { "gemini": {} }
  }
}
```

Settings are user preference/configuration, not backend-native session state, so
Gemini settings should migrate to Antigravity instead of being dropped.

Exact settings-store migration:

1. In `HostSettingsStore::load`, read the file as `serde_json::Value` before
   strict `read_from_disk`.
2. Descend into the wrapper's `settings` object.
3. In `settings.enabled_backends`, map every `"gemini"` entry to
   `"antigravity"`, then let canonicalization/deduplication enforce the final
   order.
4. If `settings.default_backend` is `"gemini"`, set it to `"antigravity"` and
   ensure `"antigravity"` is present in `enabled_backends`. This keeps
   `default_backend ∈ enabled_backends`, so `validate_settings` at
   `settings.rs:175-183` continues to pass.
5. In `settings.backend_tier_configs`, remove the `"gemini"` key. If complexity
   tiers are enabled, seed an `"antigravity"` key from
   `antigravity_cost_hint_defaults` unless the user already has one.
6. Rewrite `settings.json` atomically if changed.
7. Then call strict `read_from_disk` and existing validation.

This keeps a user's explicit backend preference through the replacement while
still letting setup surface the auth-required state for `agy`. It does not
resurrect Gemini sessions; session records are purged separately because they
cannot be safely mapped to Antigravity native state.

#### `agent_teams.json`

`~/.tyde/agent_teams.json` has top-level `version`, `teams`, and `members` fields.
`TeamMember` persists both `backend_kind` and `session_id`, and strict
deserialization means one persisted `"gemini"` member can break the whole teams
store after the enum variant is removed.

Exact agent-teams migration:

1. Run after the session purge has produced the set of purged Gemini
   `SessionId`s and after settings migration has made Antigravity an enabled
   backend when Gemini was enabled.
2. In `AgentTeamsStore::load`, read the file as `serde_json::Value` before
   strict `AgentTeamsStoreFile` deserialization.
3. Descend into top-level `members`.
4. For each member whose `backend_kind` is exactly `"gemini"`:
   - set `backend_kind` to `"antigravity"`;
   - clear `session_id` because the old session either was purged or refers to a
     Gemini-native conversation that Antigravity cannot resume.
5. For any member whose `session_id` is in the purged Gemini session-id set,
   clear `session_id` even if its `backend_kind` was already something else.
6. Preserve teams and members otherwise; do not drop whole teams as a side
   effect of backend migration.
7. Rewrite `agent_teams.json` atomically if changed.
8. Then run existing version migrations, strict deserialization, and validation.

Implementation should plumb the purged Gemini session-id set explicitly, for
example by having the session-store load/migration return it to host startup and
adding it to the refs passed into `AgentTeamsStore::load`. Do not infer dangling
sessions by filesystem lookup or heuristic matching.

### Gemini removal checklist

Codex implementation should remove every compiled Gemini touchpoint:

- Begin with the grep command from section B and repeat it before handoff.
- Delete `server/src/backend/gemini.rs`.
- Add `server/src/backend/antigravity.rs`.
- Replace `BackendKind::Gemini` with `BackendKind::Antigravity` in
  `protocol/src/types.rs`.
- Update `BackendKind::supports_image_input` to keep the backend false.
- Update `server/src/backend/mod.rs` module exports and match arms.
- Update `server/src/backend/setup.rs` candidates, setup iteration, docs URL,
  install command, sign-in command, and auth-required messaging.
- Update `server/src/lib.rs` re-export.
- Update `server/src/host.rs` tests/references.
- Update `server/src/agent/mod.rs` spawn/resume/fork dispatch.
- Update `server/src/agent_control_mcp.rs` backend input enum.
- Update `dev-driver/src/agent_control.rs` backend input enum and schema.
- Update `server/src/store/settings.rs` canonical backend list and one-shot
  settings migration.
- Update `server/src/store/session.rs` one-shot Gemini-session purge migration.
- Update `server/src/store/agent_teams.rs` one-shot Gemini member/session repair
  migration.
- Update `dev-docs/20-backend-access-mode.md` `### Gemini` section to describe
  Antigravity fail-closed read-only behavior.
- Update server-side tests in `tests/tests/backend.rs`,
  `tests/tests/custom_agents.rs`, and `tests/tests/settings.rs`.
- Replace Gemini in-file backend tests with Antigravity arg construction,
  timeout, model, MCP config, spawn handshake, unsupported resume/fork, and
  read-only rejection tests.
- Ignore the `old/` tree unless it is actually compiled.

### Test plan

Follow `tests/TESTING.md`: tests are client-level end-to-end flows through
client -> server -> mock backend, asserting observable protocol events rather
than internals.

Server/mock protocol tests:

- Spawn an Antigravity mock agent and assert `NewAgent`, `AgentStart`, user echo,
  assistant events, and `SessionList` carry `BackendKind::Antigravity`.
- Assert Antigravity spawn returns a Tyde-minted `SessionId` at the ready
  handshake and that the listed session has `resumable: false`.
- Assert resume/fork requests for Antigravity fail visibly with unsupported
  backend errors and do not attempt native session lookup.
- Update host settings to enable/default Antigravity and assert subsequent new
  chat/review/team flows use Antigravity through observable protocol payloads.
- Assert Antigravity session settings schema is emitted as a ready schema with a
  non-null `model` select containing the exact model labels and defaulting to
  `Gemini 3.5 Flash (Medium)`.
- Assert read-only Antigravity spawn fails visibly with a command/backend error
  rather than silently spawning unrestricted.

Store/unit tests:

- `settings.rs`: a wrapped store with persisted `"gemini"` in
  `settings.enabled_backends`, `settings.default_backend`, and
  `settings.backend_tier_configs` migrates in `load()`, rewrites once, preserves
  `default_backend ∈ enabled_backends`, and then passes strict `read_from_disk`.
- `session.rs`: a wrapped `sessions.json` with one Gemini record and one
  Claude/Codex record loads successfully, rewrites without the Gemini record,
  returns the purged Gemini session-id set, and summaries contain only the
  non-Gemini session.
- `agent_teams.rs`: a store with a Gemini member and a member pointing at a
  purged Gemini session migrates in `load()`, maps the backend to Antigravity,
  clears dangling `session_id`s, rewrites once, and then strict-deserializes.
- `antigravity.rs`: arg construction includes `--print-timeout`, exact
  `--model`, `--add-dir`, and `--dangerously-skip-permissions` for unrestricted
  mode; it never includes native resume flags.
- `antigravity.rs`: spawn mints the Tyde session id before the first child
  process starts, resolves ready with that id, and reports `resumable: false`.
- `antigravity.rs`: `list_sessions` returns empty, and `resume` / `fork` return
  explicit unsupported errors.
- `antigravity.rs`: read-only arg/spawn path rejects before launching `agy`.
- `antigravity.rs`: plain-text stdout parser emits stream start/deltas/end and
  surfaces `Authentication required` / `Error:` lines as backend errors.
- `antigravity.rs`: MCP config merge/restore preserves exact original bytes and
  serializes concurrent Antigravity MCP mutations.

Real backend tests in `tests/tests/backend.rs` should be updated but only run
under the existing backend-test rules from `AGENTS.md`: they exercise real AI
agents and should only be run when changing a backend. The readiness probe must
use bounded `agy -p` plain text, not Gemini `stream-json`.

---

## D. Work split

### Codex-owned implementation

Codex owns protocol and server work:

- `protocol/src/types.rs`
- all `server/src/**` backend/setup/agent/store/lib/host/agent-control changes,
  including `server/src/store/agent_teams.rs`
- `dev-driver/src/agent_control.rs`
- `dev-docs/20-backend-access-mode.md`
- server-side tests under `tests/tests/**`
- the new `server/src/backend/antigravity.rs`
- deletion of `server/src/backend/gemini.rs`

### Claude-owned implementation

Claude owns frontend and mobile frontend updates:

- `frontend/styles.css:1492-1495` — replace `.backend-badge.gemini`.
- `frontend/src/components/home_view.rs:22,360,370`.
- `frontend/src/components/chat_view.rs:700`.
- `frontend/src/components/center_zone.rs:45`.
- `frontend/src/components/agents_panel.rs:58,68`.
- `frontend/src/components/sessions_panel.rs:18,28`.
- `frontend/src/components/session_settings.rs:322,333,343`.
- `frontend/src/components/settings_panel.rs:615`.
- `frontend/src/components/settings_panel.rs:2606-2614`.
- `frontend/src/components/settings_panel.rs:2622,2633,2643,2653,2663`.
- `frontend/src/components/review_view.rs:1025,1122,3598-3602`.
- `frontend/src/components/teams_panel.rs:2216,2226,2236`.
- `frontend/src/components/chat_input.rs:808,881,887`.
- `frontend/src/state.rs:2706-2707`.
- `mobile-frontend/src/components/diff_viewer.rs:2126-2161`.

### Sequencing

The protocol `BackendKind` change must land first with the server changes.
Frontend matches over the generated enum will become non-exhaustive until the
frontend is updated, so Claude's frontend update should land in lockstep after
Codex's protocol/server branch or in the same coordinated integration window.

Do not push, tag, open PRs, or affect remotes during either phase without
explicit user approval.
