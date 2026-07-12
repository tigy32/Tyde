# 27. Agent Activity Summaries

## 1. Context and constraints

This design follows the repository guidance in `AGENTS.md`, the testing
strategy in `tests/TESTING.md`, and the Tyde2 architecture rules in
`dev-docs/01-philosophy.md`.

The feature is a server-owned, optional background summarizer that periodically
summarizes a live agent's recent activity using a cheap model. The frontend only
renders typed server state. There are no frontend-only caches, no inferred
summaries, and no silent fallback text when summarization fails.

The same server-owned activity snapshot also carries token totals:
`AgentActivityStats.token_usage` is the authoritative per-agent cumulative
`agent_total` stamped on `ChatMessage.turn_token_usage`. It is not a frontend
roll-up and it does not include sub-agent tokens.

Goals:

- Show a short, human-readable "what is this agent doing?" summary for active
  agents.
- Reuse the server-owned helper-generation pattern used for agent names.
- Keep the feature behind a user setting because it spends money.
- Use typed protocol state and events end-to-end.
- Make model calls mockable in tests and disabled by default unless the user
  opts in.

Non-goals:

- Persist long-term summaries into the session store.
- Replace the transcript, task list, tool progress, or agent status.
- Summarize every streaming token. The summary is a sampled progress signal, not
  a live transcript mirror.

## 2. Existing internal agent-name mechanism

Tyde already has an internal model call for generated agent names. Activity
summaries reuse its cheap-model selection and mock behavior, but not its spawn
ordering: name generation is part of spawn resolution, while summaries remain
background work for an already-visible agent.

### 2.1 Trigger and gating

When spawning `SpawnAgentParams::New`, the host resolves the displayed name from
`payload.name`. If the user supplied a name, Tyde treats it as a user alias. If
no name was supplied and automatic names are enabled, Tyde completes the
internal generation call before registering or announcing the real agent. A
generation failure therefore fails the spawn command without exposing a
provisional agent.

Only the no-explicit-name path runs a generated-name request, and only if startup
resolution has not already failed. The request captures:

- target backend kind,
- workspace roots,
- the original prompt,
- startup MCP servers,
- whether this host is using the mock backend (`server/src/host.rs:1843-1849`).

Resume/fork paths use stored or explicit session names and do not run this
generation call.

`HostSettings.background_agent_features.auto_generate_agent_names` controls
whether the internal generation call runs. When disabled, Tyde uses the
deterministic prompt-derived name without making a model call.

### 2.2 What backend/model it uses

`GenerateAgentNameRequest` carries the original agent's `BackendKind`, workspace
roots, prompt, startup MCP servers, and `use_mock_backend`
(`server/src/agent/mod.rs:334-340`).

For real backends, `generate_agent_name` constructs a `BackendSpawnConfig` with
`cost_hint: Some(SpawnCostHint::Low)`, no custom agent, no session settings, and
a default resolved spawn config (`server/src/agent/mod.rs:354-367`). It then
spawns the same backend kind directly through `spawn_backend`
(`server/src/agent/mod.rs:374-384`, `server/src/agent/mod.rs:490-546`). This is
not inserted into `AgentRegistry`, so it does not appear as a normal Tyde agent.
The transient `name_agent_id` exists only for the direct backend spawn
(`server/src/agent/mod.rs:374-383`).

Important divergence to preserve later: generated names pass through the
`startup_mcp_servers` captured from the user-agent spawn path
(`server/src/host.rs:1843-1849`). That list is built from host settings and can
include Tyde's agent-control MCP when `tyde_agent_control_mcp_enabled` is true
(`server/src/host.rs:8982-8991`). The activity summarizer must **not** copy that
field across; it is a pure text task and should receive no MCP servers.

The backend-level cheap mappings are already backend-owned:

- Claude Low maps to `model = haiku`, `effort = low`
  (`server/src/backend/claude.rs:7737-7758`).
- Codex does not guess a Low model or reasoning value in its backend resolver.
  Normal complexity tiers are resolved by the host from current Codex model
  metadata; direct helper spawns use the provider default rather than risking
  an unsupported hardcoded effort.
- Kiro Low maps to `claude-haiku-4.5` (`server/src/backend/kiro.rs:3105-3123`).
- Antigravity Low maps to `ANTIGRAVITY_LOW_MODEL`
  (`server/src/backend/antigravity.rs:1221-1232`).
- The generic resolver applies cost-hint defaults before explicit settings and
  schema defaults (`server/src/backend/mod.rs:378-398`).

Normal user spawns may have cost hints stripped when complexity tiers are off,
but generated-name helper calls bypass that host-spawn path and pass
`SpawnCostHint::Low` directly to the backend.

### 2.3 What history it feeds

The generated-name call does **not** feed recent transcript history. It feeds
only the initial prompt, wrapped in a tight instruction:

```text
Return only a short 2-4 word work name for this request. No quotes, no markdown,
no explanation. Request: {prompt}
```

The prompt builder is `build_name_generation_prompt`
(`server/src/agent/mod.rs:3435-3438`). Empty prompts return the fixed
`Image Review Task` name without a model call (`server/src/agent/mod.rs:345-348`).

The activity-summary feature should reuse this mechanism's background-call
shape, but its input must come from the live agent's recent replay/history, not
only the initial prompt.

### 2.4 Result collection, sanitization, storage, and emission

The name generator streams backend output, accumulates `StreamDelta`, and uses
the final `StreamEnd` message content when available (`server/src/agent/mod.rs:395-442`).
If tests run with `use_mock_backend`, it returns a deterministic generated name
without spawning a real backend (`server/src/agent/mod.rs:350-352`,
`server/src/agent/mod.rs:3457-3467`).

The generated output is sanitized to a 2-4 word title. A reasoning-only or
otherwise empty `StreamEnd` is not a final name: collection continues until an
assistant answer segment completes. If the turn or backend ends without usable
answer text, or the completed answer is invalid, generation fails explicitly
instead of treating the prompt-derived deterministic name as a successful model
result. The resolved generated name is included in the initial registry spawn
and persisted as the session alias, so the first `NewAgent` and `AgentStart`
already agree. The summary feature likewise must not silently synthesize
fallback summary text; it should emit an explicit error state.

### 2.5 Tests already cover this pattern

The integration test for spawning without an explicit name asserts the generated
name arrives in `NewAgent`, `AgentStart`, and `SessionList` using the mock
backend (`tests/tests/agents.rs:3100-3151`). The session-store test asserts that
generated aliases do not override user aliases (`tests/tests/settings.rs:212-235`).

Those tests are the model for the activity-summary tests: client → server → mock
backend, observable protocol events, no real AI spend.

## 3. Server-side summarizer design

### 3.1 Ownership model

The server owns all summary behavior and state.

Add a host-owned `AgentActivitySummarizer` coordinator that runs inside the host
process. It watches live agents and writes the current per-agent summary state
into host-owned state, for example:

```rust
HashMap<AgentId, AgentActivitySummaryState>
```

This state is transient. It is not stored in `SessionStore`, because it is a
live progress projection rather than durable session metadata. New host
subscribers receive the current state in `HostBootstrapPayload.agents` through
an extension to `NewAgentPayload`; live updates are emitted as typed protocol
events.

The agent actor remains the source of transcript/replay data. It already owns an
`event_log` (`server/src/agent/mod.rs:779-782`), records replayable chat events
(`server/src/agent/mod.rs:3001-3021`), coalesces tool progress in the replay log
(`server/src/agent/mod.rs:3165-3184`), and can return recent output through
`AgentHandle::read_output` (`server/src/agent/mod.rs:283-297`,
`server/src/agent/mod.rs:3216-3228`). Agent stream attach already replays
`AgentBootstrap` from that log plus active stream events
(`server/src/agent/mod.rs:3289-3308`).

For activity summaries, add a dedicated actor command rather than teaching the
frontend to read transcripts:

```rust
AgentCommand::ReadActivityHistory {
    after_seq: Option<u64>,
    max_events: usize,
    max_bytes: usize,
    reply: oneshot::Sender<AgentActivityHistorySnapshot>,
}
```

`AgentActivityHistorySnapshot` should be a bounded, server-internal summary
input that includes:

- replay-log `ChatEvent` and `AgentError` entries after `after_seq`, capped by
  event count and byte count;
- the same active-stream replay events currently included in `AgentBootstrap`,
  so a running agent can produce a progress summary before the turn ends;
- metadata: `from_seq`, `through_seq`, `event_count`, and whether active-stream
  text was included.

Do not create a frontend cache or a summarizer-only transcript mirror. If a
summary needs information that is not currently in the agent replay log, plumb it
into the actor-owned replay/history model explicitly.

### 3.2 Trigger cadence

Use a debounced, status-change-driven scheduler plus a slow active-agent tick.
The registry already exposes status snapshots and a watch channel:

- `AgentStatus` tracks `started`, `terminated`, `is_thinking`, `turn_completed`,
  `last_error`, and `activity_counter` (`server/src/agent/registry.rs:31-40`).
- `AgentStatus::is_active` identifies active agents
  (`server/src/agent/registry.rs:47-50`).
- Every status update increments a registry-wide watch counter
  (`server/src/agent/registry.rs:98-108`).
- The host exposes `subscribe_agent_status_changes` and per-agent status
  snapshots (`server/src/host.rs:5140-5142`, `server/src/host.rs:5105-5115`).

Recommended cadence:

1. **Initial delay:** after an enabled agent starts and has at least one
   meaningful history event, wait 10-15 seconds before the first summary. This
   avoids summarizing startup noise.
2. **Debounce:** after each significant status/history change, wait 5-10 seconds
   before scheduling. Coalesce all changes during the debounce window into one
   request.
3. **Max frequency:** while an agent remains active and changed, summarize at
   most once per 60-90 seconds per agent.
4. **Completion refresh:** when a turn reaches idle or fails, allow one final
   summary after a short debounce even if the max-frequency window would
   otherwise delay it. This produces a useful final state for the agent row.
5. **No unchanged calls:** compare the candidate history `through_seq` and the
   last summarized `through_seq`. If there are no new source events, do not call
   a model.

"Significant change" should be server-defined: status activity-counter changes,
new replay events, terminal errors, tool request/progress/completion, task
updates, and stream end. Do not trigger one call per token delta.

### 3.3 Summary input slice

Feed a compact, typed rendering of recent activity, not raw JSON dumps.
Recommended caps:

- last 30-50 relevant events;
- max 12-16 KiB of rendered input text;
- max 1-2 KiB per individual message/tool/event;
- include only textual message/tool/error/task information;
- omit image bytes and large/raw tool outputs;
- include event sequence numbers and timestamps when available.

The rendered input should favor:

- latest user-visible assistant messages;
- current or recent tool names, statuses, and compact progress text;
- errors and cancellation messages;
- active-stream text/reasoning preview if the agent is still working;
- the previous summary, if present, as context for incremental updates.

The prompt should make the output small and stable:

```text
You summarize live coding-agent activity for a UI.
Return one concise sentence, max 18 words.
Describe what the agent is currently doing or just finished.
Do not mention that you are summarizing. Do not invent facts.
If the input is insufficient, return exactly: No clear activity yet.

Previous summary: {previous_or_none}
Recent activity:
{bounded_event_rendering}
```

The post-processor accepts only plain text, strips quotes/markdown, collapses
whitespace, and caps to a small character limit, for example 180 chars. If the
backend returns empty output, malformed tool-only output, or a refusal unrelated
to the input, emit an explicit `Error` or `Empty` state instead of fabricating a
summary.

### 3.4 Cheap backend/model selection

Reuse the generated-name pattern, with a stricter tool-safety envelope:

- background calls are not inserted into `AgentRegistry`;
- use the target agent's `BackendKind`;
- spawn through the same direct `spawn_backend` helper;
- pass `BackendSpawnConfig { cost_hint: Some(SpawnCostHint::Low), .. }`;
- set `resolved_spawn_config.access_mode = BackendAccessMode::ReadOnly` so
  backend-native file/shell tools cannot write even if the backend tries to use
  them;
- pass **no MCP servers**;
- feed only bounded text history in the prompt;
- collect streamed/final assistant text exactly like name generation collects a
  title;
- use `use_mock_backend` to avoid real calls in tests.

Tool-safety strategy: the summarizer is a pure text task. Omitting Tyde MCP
servers removes Tyde tools and recursion paths, but agentic backends can still
have built-in file/shell tools independent of MCP. Therefore the summarizer must
not treat tool-call events as automatic failure. It should deny/ignore tool-call
attempts at the summary layer, rely on read-only access mode plus no MCP to make
them harmless, continue reading streamed text, and emit `Error` only if no usable
summary text is produced.

Workspace roots: prefer `workspace_roots = Vec::new()` because the task needs no
repo context. If a backend cannot spawn without at least one valid root, pass the
target agent's first/root set as the minimum spawn requirement, still with
read-only access and no MCP, and still take only the text response.

Differences from generated names:

- Do **not** copy the generated-name `startup_mcp_servers`. Name generation can
  attach Tyde's agent-control MCP via the normal startup-MCP path
  (`server/src/host.rs:1847`, `server/src/host.rs:8982-8991`); activity
  summaries must pass an empty MCP list so the background call cannot spawn,
  await, or message Tyde agents.
- Add a `debug_assert!` in the host coordinator that the summarizer's transient
  backend id never resolves in `AgentRegistry` before or after the direct
  backend spawn.
- Use an internal background-task label in logs/metrics, e.g.
  `BackgroundAgentTask::ActivitySummary`, so failures are diagnosable.
- Guard in-flight results with a host settings epoch. Disabling the setting stops
  new scheduling immediately and causes any late result from an already-running
  call to be discarded.

Because the setting defaults to off, using the target backend with Low cost is a
reasonable Phase 1 tradeoff: it avoids a new model-selector surface while using
credentials and backend availability the user already configured.

### 3.5 Per-agent summary state

Add typed protocol state that is expressive enough for loading, empty, fresh,
stale, disabled, and error UI states without frontend inference.

Recommended protocol shape:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivitySummary {
    pub text: String,
    pub generated_at_ms: u64,
    pub source_from_seq: Option<u64>,
    pub source_through_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentActivitySummaryState {
    Disabled,
    Empty,
    Pending {
        requested_at_ms: u64,
        previous: Option<AgentActivitySummary>,
    },
    Fresh {
        summary: AgentActivitySummary,
    },
    Stale {
        summary: AgentActivitySummary,
        reason: AgentActivitySummaryStaleReason,
    },
    Error {
        message: String,
        occurred_at_ms: u64,
        previous: Option<AgentActivitySummary>,
    },
}

impl Default for AgentActivitySummaryState {
    fn default() -> Self {
        Self::Disabled
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivitySummaryStaleReason {
    NewActivity,
    MaxAge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivitySummaryPayload {
    pub agent_id: AgentId,
    pub state: AgentActivitySummaryState,
}
```

Store exactly one current `AgentActivitySummaryState` per live agent. The server
sets `Pending` when a call starts, `Fresh` when it completes against the latest
known source range, `Stale` when new activity arrives after a fresh summary, and
`Error` when a call fails. Emit `Stale` only on the `Fresh -> Stale` edge; once
an agent is already stale, additional status/history events should update
server-local dirtiness without flooding the host stream. When the setting is
off, emit `Disabled` and suppress or hide the UI.

## 4. Cost controls

The feature must be conservative by default.

1. **Default off.** Activity summaries spend money, so the new setting should be
   disabled on fresh installs and after migration unless the user opts in.
2. **Debounce per agent.** Coalesce bursts; never summarize every token/event.
3. **Max frequency per agent.** Enforce a hard minimum interval, e.g. 60-90
   seconds between successful or failed calls for the same agent while active.
4. **Active/changed only.** Only schedule agents where `AgentStatus::is_active`
   is true or a final completion refresh is pending, and only if the history
   `through_seq` changed since the last summary.
5. **No recursion.** Background summarizer calls are not registered agents, emit
   no `NewAgent`, have no Tyde agent-control MCP server, and are never eligible
   for summarization.
6. **Concurrency cap.** Use a host-wide semaphore. Start with one concurrent
   summarizer call per host; consider two only if UX proves too slow.
7. **Host-wide rate cap.** Add a token-bucket calls/minute limit across all
   agents as a cheap spend guard. Phase 1 can keep this conservative and static;
   Phase 3 can expose tuning if active-agent count makes linear spend too high.
8. **Queue coalescing.** Keep at most one queued summarization request per agent.
   A newer dirty mark replaces the older queued request.
9. **Immediate off switch.** When the setting flips off, stop new scheduling,
   clear queued requests, advance a settings epoch, fan out `Disabled` states,
   and discard any late result whose epoch no longer matches. Do not promise
   instant abort of token billing; with semaphore=1, at most one in-flight call
   may drain.
10. **Failure backoff.** After a backend error, set `Error` and apply exponential
    or fixed backoff before retrying that agent. Do not loop on failing backends.
11. **Terminal agents.** After a final completion summary, do not continue
    summarizing terminated/closed agents; remove their transient summary state
    when `AgentClosed` removes the live agent.

## 5. Settings design

### 5.1 New background-agent settings group

Add a typed settings group under `HostSettings`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundAgentFeaturesSettings {
    #[serde(default = "default_auto_generate_agent_names_enabled")]
    pub auto_generate_agent_names: bool,
    #[serde(default)]
    pub agent_activity_summaries: bool,
}
```

Then add it to `HostSettings`:

```rust
#[serde(default = "default_background_agent_features")]
pub background_agent_features: BackgroundAgentFeaturesSettings,
```

Defaults:

- `auto_generate_agent_names: true` to preserve existing behavior.
- `agent_activity_summaries: false` because it creates periodic paid calls.

Add a strongly typed setting update:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundAgentFeature {
    AutoGenerateAgentNames,
    AgentActivitySummaries,
}

HostSettingValue::BackgroundAgentFeatureEnabled {
    feature: BackgroundAgentFeature,
    enabled: bool,
}
```

This keeps the protocol strongly typed rather than string-keyed.

### 5.2 Store and propagation

Mirror the existing settings pattern:

- Add serde defaults in `protocol/src/types.rs`, alongside the current
  `HostSettings` defaults (`protocol/src/types.rs:1530-1551`).
- Add defaults in `empty_settings` (`server/src/store/settings.rs:384-394`).
- Extend `apply_setting` to mutate only the requested background feature
  (`server/src/store/settings.rs:276-330`).
- Keep validation explicit; if future background feature settings add durations
  or limits, validate ranges in `validate_settings`.
- `HostHandle::set_setting` already applies a setting, fans out `HostSettings`,
  and notifies dependent subsystems (`server/src/host.rs:4417-4436`). Extend that
  path so the summarizer coordinator receives the new setting and immediately
  enables/disables scheduling.
- New clients receive `HostSettings` in `HostBootstrapPayload.settings`
  (`protocol/src/types.rs:1086-1103`, `server/src/host.rs:729-751`), and live
  clients receive `FrameKind::HostSettings` via `fan_out_host_settings`
  (`server/src/host.rs:10035-10050`, `server/src/host.rs:10271-10282`).

### 5.3 Existing generated names under the same group

The generated-name path consults
`settings.background_agent_features.auto_generate_agent_names` before invoking
the internal generator.

When disabled:

- resolve the deterministic name from `derive_agent_name`;
- do not invoke `GenerateAgentNameRequest`;
- do not call a model;
- do not emit a synthetic error, because the user explicitly disabled the
  background enhancement.

This surfaces the existing paid/background feature without breaking current
fresh-install behavior.

## 6. Protocol changes

Required protocol changes:

1. Add `BackgroundAgentFeaturesSettings` and `BackgroundAgentFeature`.
2. Add `HostSettings.background_agent_features`.
3. Add `HostSettingValue::BackgroundAgentFeatureEnabled`.
4. Add `AgentActivitySummary`, `AgentActivitySummaryState`,
   `AgentActivitySummaryStaleReason`, and `AgentActivitySummaryPayload`.
5. Add output `FrameKind::AgentActivitySummary` with display string
   `agent_activity_summary`.
6. Extend `NewAgentPayload` with:

   ```rust
   #[serde(default)]
   pub activity_summary: AgentActivitySummaryState,
   ```

   `AgentActivitySummaryState::default()` must be `Disabled`, so old payloads and
   feature-off hosts deserialize to the safe non-rendering state.

   This lets `HostBootstrapPayload.agents` and live `NewAgent` events carry the
   server-owned current summary state for agent lists.
7. Optionally extend `AgentStartPayload` with the same field if agent-stream
   replay should be self-contained. If this is done, the server must update the
   actor's stored `AgentStartPayload` or emit a separate replay event whenever
   summary state changes. Phase 1 can avoid that coupling by making the
   host-stream `NewAgentPayload` plus `AgentActivitySummary` event the canonical
   UI source.
8. Extend `AgentControlAgentRef` or the `tyde_await_agents` result with an
   optional summary in a later phase if manager agents should receive the same
   text in MCP JSON. The UI can render from `AgentInfo` without changing tool
   progress first.
9. Bump `PROTOCOL_VERSION` in `protocol/src/types.rs` from 17
   (`protocol/src/types.rs:16`) to the next value and update the protocol-version
   test (`protocol/src/types.rs:4702-4703`).

Because frontend, client, server, and dev-driver use Rust protocol types, the
compiler will identify all payload construction and dispatch sites that need the
new fields.

## 7. Server changes

### 7.1 Background generation helper

Introduce a second background call next to `GenerateAgentNameRequest`:

```rust
pub(crate) struct GenerateAgentActivitySummaryRequest {
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub rendered_history: String,
    pub previous_summary: Option<String>,
    pub source_from_seq: Option<u64>,
    pub source_through_seq: Option<u64>,
    pub use_mock_backend: bool,
}
```

`generate_agent_activity_summary` should mirror `generate_agent_name`:

- return deterministic mock output when `use_mock_backend` is true;
- build a strict one-sentence prompt;
- spawn the target backend directly with `SpawnCostHint::Low`, read-only access,
  empty workspace roots when supported, and an empty MCP server list;
- collect stream deltas/final content;
- ignore/deny tool-call attempts for summary extraction rather than failing the
  whole summary call;
- sanitize and cap the returned text;
- return typed errors with context only when no usable text is produced or the
  backend itself fails.

A small shared helper can be introduced only if it removes duplication between
name and summary generation without obscuring error handling.

### 7.2 Host coordinator

Add an `AgentActivitySummarizer` coordinator owned by `HostState` or spawned by
`HostHandle` at host startup. It should:

- read `HostSettings.background_agent_features.agent_activity_summaries`;
- subscribe to registry status changes;
- inspect live `AgentStatus` snapshots;
- request bounded history snapshots from agent actors;
- manage debounce/frequency/concurrency;
- write current `AgentActivitySummaryState` into host-owned state;
- fan out `FrameKind::AgentActivitySummary` on the host stream.

Fanout should mirror existing host-level notification helpers such as
`fan_out_host_settings` (`server/src/host.rs:10035-10050`) and
`emit_host_settings_for_subscriber` (`server/src/host.rs:10271-10282`). New
subscribers should receive current summary state via `NewAgentPayload` during
host bootstrap and live `NewAgent` emission (`server/src/host.rs:697-712`,
`server/src/host.rs:9559-9585`).

### 7.3 Agent history snapshot command

Add an actor command for bounded summary input. Do not reuse
`tyde_read_agent` output directly for the UI feature, because `read_output` is a
public MCP-facing read API and currently excludes active-stream replay state
(`server/src/agent/mod.rs:3216-3228`). The summarizer needs a server-internal
snapshot that includes active-stream events the same way `AgentBootstrap` does
(`server/src/agent/mod.rs:3289-3308`).

The command should be cheap, bounded, and non-blocking:

- it reads actor-owned in-memory state only;
- it does not call a backend;
- it returns a capped rendered or typed snapshot;
- it includes `through_seq` so the coordinator can skip unchanged agents.

### 7.4 Error handling

Errors must be visible:

- backend spawn failure → `AgentActivitySummaryState::Error`;
- backend emits an error message and no usable text → `Error`;
- tool-call attempts without useful text → `Error`; tool-call attempts with a
  usable streamed/final text summary → accept the text;
- empty/invalid output → `Error` or `Empty`, depending on input sufficiency;
- setting disabled → `Disabled`;
- no meaningful source events → `Empty` and no model call.

Do not keep an old summary labeled as fresh after new history arrives. Mark it
`Stale { reason: NewActivity }` until the next successful summary.

## 8. UI surfacing

The frontend renders server state from protocol events. It should not parse
transcripts, stream text, or tool cards to infer an activity summary.

### 8.1 Recommended surfaces

1. **Await-agents tool card — Phase 1.** The user specifically called out the
   await-agents view. The existing tool card already has an
   `AgentControlAgentRow` for `tyde_await_agents` and shows live streaming
   preview when available (`frontend/src/components/tool_card/mod.rs:588-613`,
   `frontend/src/components/tool_card/mod.rs:648-763`). Replace or augment that
   preview with the server summary from `AgentInfo.activity_summary`. This is the
   only required UI surface for Phase 1.

2. **Agent rows / left agents panel — Phase 2.** The agents panel card already renders the
   name, status, time, custom-agent badge, backend badge, and errors
   (`frontend/src/components/agents_panel.rs:660-850`). Add one compact summary
   line under the metadata, hidden when disabled/empty.

3. **Agents center — Phase 2.** `AgentMonitorView` is the richer agents center surface
   (`frontend/src/components/agent_monitor_view.rs:943-1043`). Its row view
   renders status, name/backend, metadata, and tags
   (`frontend/src/components/agent_monitor_view.rs:2637-2691`). Add the summary
   under the name/meta block, with stale/error styling.

4. **Chat header — Phase 2, restrained.** The chat view already centralizes current
   `AgentInfo` lookup (`frontend/src/components/chat_view.rs:266-281`) and
   renders the agent header name/backend/tool-output toggle
   (`frontend/src/components/chat_view.rs:813-825`). Show the summary as a small
   secondary line or tooltip only when fresh/stale/error and non-empty. Avoid
   making the header tall for disabled/empty states.

### 8.2 UI states

Render directly from `AgentActivitySummaryState`:

- `Disabled`: hide the summary UI; settings panel explains the feature is off.
- `Empty`: show nothing or a subtle "No activity summary yet" only in Agents
  Center, not every compact row.
- `Pending { previous: None }`: show "Summarizing…" with a spinner/skeleton on
  expanded surfaces; compact surfaces may hide it.
- `Pending { previous: Some(_) }`: show previous text with a "updating…" chip.
- `Fresh`: show the summary text and a freshness label such as "just now" or
  "2m old" based on `generated_at_ms`.
- `Stale`: show the previous summary with a "stale" or "new activity" chip.
- `Error`: show a warning icon/tooltip and previous text if present; otherwise
  show a compact error on expanded surfaces only.

The frontend may format timestamps for display, but freshness/stale/error state
comes from the server enum.

### 8.3 Dispatch/state

Add `activity_summary: AgentActivitySummaryState` to `AgentInfo`
(`frontend/src/state.rs:53-72`). Populate it from `NewAgentPayload` in the
`NewAgent` dispatch path (`frontend/src/dispatch.rs:749-785`) and bootstrap path
(`frontend/src/dispatch.rs:4196-4239`). Add a new `FrameKind::AgentActivitySummary`
dispatch arm that finds the matching `AgentInfo` and replaces only that field.

Do not add a separate frontend map keyed by agent unless a measured rendering
problem requires it. `AgentInfo` is already the row/header source for these
surfaces.

## 9. Testing strategy

Follow `tests/TESTING.md`: tests should be client-level end-to-end flows through
the public client API, a real server, and the mock backend. They should assert on
protocol responses/events and observable UI rendering, not private maps or task
internals.

Do **not** run `backend.rs` real-AI tests for this feature, and do not set
`TYDE_RUN_REAL_AI_TESTS`, `TYDE_LIVE_CODEX_TEST`, or
`TYDE_RUN_CLAUDE_INTEGRATION`. The summarizer call must be mockable/gated so the
suite never spends money.

Recommended tests:

1. **Protocol round trips.** Add serde tests for `AgentActivitySummaryState`,
   `AgentActivitySummaryPayload`, and the new background settings. Update the
   `PROTOCOL_VERSION` assertion.
2. **Settings default and toggle.** With `Fixture::new`, assert the bootstrap
   settings default to generated names enabled and activity summaries disabled.
   Send `SetSetting` for `AgentActivitySummaries`; assert a `HostSettings` event
   reflects the change.
3. **Disabled means no calls.** Spawn and drive a mock agent with summaries off;
   assert no `AgentActivitySummary` event appears in the observed event window.
4. **Enabled emits pending/fresh.** Enable summaries, spawn a mock agent, drive a
   turn, and assert `AgentActivitySummary` transitions through `Pending` to
   `Fresh` with deterministic mock text and source seq metadata.
5. **Changed agents only.** After a fresh summary, wait through another cadence
   window without new history and assert no new summary event is emitted.
6. **Stale state.** After a fresh summary, send another message or mock progress
   event; assert the server emits `Stale { reason: NewActivity }` before the next
   fresh summary.
7. **Off switch discards late results.** Enable summaries, arrange a slow mock
   summarizer call, disable the setting, and assert queued summaries stop,
   `Disabled` is emitted, and any late result from the drained in-flight call is
   discarded by epoch guard.
8. **No recursion.** Assert summary background calls do not emit `NewAgent` and
   do not appear in `agent_ids`/agent-control listings.
9. **Error state.** Force the mock summary generator to fail; assert an explicit
   `Error` state is emitted and no fallback text is synthesized.
10. **Frontend wasm tests.** Phase 1 adds component coverage for the
    await-agents tool card only. Phase 2 adds agents panel row, agent monitor
    row, and chat header tests. Mount real components with
    `AgentInfo.activity_summary` states and assert fresh/stale/pending/error text
    and classes. Do not weaken existing UI assertions.

The existing fixture already uses mock backend by default and skips expensive
real backend probes (`tests/tests/fixture.rs:56-98`). Keep activity-summary tests
inside the existing related integration files where practical rather than adding
many new test files.

## 10. Phasing

### Phase 1: Server, settings, protocol, minimal UI

Deliver the core architecture:

- protocol types, frame kind, and protocol-version bump;
- `HostSettings.background_agent_features` and settings UI toggle;
- generated-name path respects `auto_generate_agent_names`;
- server summarizer coordinator with debounce/frequency/concurrency controls;
- actor history snapshot command;
- host-owned summary state and host-stream fanout;
- `AgentInfo.activity_summary` dispatch;
- minimal rendering in the await-agents tool card only;
- integration tests with mock backend.

This phase proves the server-owned protocol path and cost controls.

### Phase 2: Richer UI surfaces and polish

Add:

- Agents Center row summary line;
- left agents panel summary line;
- chat header summary line/tooltip;
- stale/freshness chips and better CSS;
- settings copy explaining cost;
- optional MCP result additions for `tyde_await_agents` so manager agents see
  the same summaries in JSON;
- metrics/logging for skipped/cancelled/succeeded summary calls.

### Phase 3: Tuning and optional configurability

Only after observing usage:

- expose interval/concurrency/rate-cap settings if users need them;
- consider a dedicated background backend/model selector;
- consider persisted last-known summaries for resumed sessions;
- tune prompt/caps based on real backend output.

## 11. Decisions and rationale

- **Default off for activity summaries.** Periodic model calls cost money, so the
  user must opt in.
- **Keep generated names default on.** This preserves current UX while giving
  users a way to disable the existing paid/background behavior.
- **Use the same backend kind with `SpawnCostHint::Low`.** This reuses existing
  credentials, backend availability, and backend-owned cheap-model mappings.
- **Host-stream update event.** Agent rows and Agents Center are host-level
  surfaces; they should receive updates even if a client has not lazily attached
  to an agent stream.
- **Transient state.** Activity summaries are live UI state, not durable session
  metadata.
- **Explicit error/stale states.** The UI should never show stale text as fresh
  or synthesize a summary from frontend observations.
- **No summarizer agents in the registry.** The background call should behave
  like generated-name calls: no `NewAgent`, no normal status row, no recursion.

## 12. Open risks and follow-up decisions

1. **Backend tool use.** Some coding-agent backends may try to use built-in
   file/shell tools despite the summary prompt. The Phase 1 strategy is explicit:
   read-only access mode, no MCP servers, preferably empty workspace roots, and
   take the streamed/final text. Tool-call attempts are ignored/denied for
   summary extraction and are not chronic `Error` triggers by themselves. Emit
   `Error` only when the call produces no usable summary text or the backend
   itself fails.
2. **History completeness.** Some backends do not emit user messages into replay.
   If summaries need original user prompts, add typed actor-owned input replay
   rather than a summarizer-only cache.
3. **Low-cost guarantees vary.** `SpawnCostHint::Low` is backend-specific and
   can still cost money. The off-by-default setting is the primary safety guard.
4. **Protocol churn.** Extending `NewAgentPayload` touches many tests and
   constructors. The protocol-version bump is required and should be done once
   with the full payload shape.
5. **UI density.** Agent rows are already information-rich. Phase 1 should keep
   summary rendering minimal and expand only after visual review.
6. **Cancellation semantics.** Backend shutdown support varies. The off switch
   must stop new scheduling immediately and ignore any late result whose
   generation epoch no longer matches the enabled setting. Do not rely on
   shutdown as the cost bound; with semaphore=1, the one in-flight call may drain.
7. **Mobile/lazy clients.** Host-stream summary events solve the main lazy-stream
   issue, but mobile-specific rendering should be reviewed separately if mobile
   agents surfaces adopt this feature.
