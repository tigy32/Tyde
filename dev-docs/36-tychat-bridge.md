# Tychat bridge

Tyde runs the owner's personal Tychat bot. Tychat is a generic encrypted bot
transport and knows nothing about Tyde. The cross-repository contract is
the owner-approved `tychat-bots-spec.md` (2026-10-09); this document records
Tyde's implementation contract.

## Ownership and phases

Protocol types live in `protocol/src/types.rs`. The host owns the singleton,
settings, lifecycle, delivery and durable outbound journal. Frontends project
host events; they never infer activity, turn boundaries or delivery from history.
`server/src/tychat_bridge.rs` runs the real `tychat-bot` client. The public SDK
is pinned to git revision `b123ad6c76dc2aa9504a3ae03bee1c7fe56c8613` from
`https://github.com/tigy32/tychat-bot`; Tyde does not fork its crypto or simulate
Tychat. Production uses the root origin `https://chat.tyggs.com`, matching
`tychat_bot::PRODUCTION_API_BASE`. The SDK appends `/api/v1`. The origin is not a
user setting: new pairings always redeem against production, and each pairing
journals the origin it was redeemed against for reconnects. Only the
`test-support` `HostRuntimeConfig::tychat_api_base` override points the host at a
numeric loopback origin for local integration. Settings and secret journals
written by v0.9.5-beta.10, which exposed the field, are migrated on load.

## Identity and lifecycle

`AgentOrigin::Tychat` is the server-owned Tychat agent role marker. There is one
per host while enabled and paired. It has no parent or project and appears
pinned and badged as **Tychat agent**. Ordinary close, resume and fork must not
create or destroy a second singleton. Disabling suspends delivery; unpair erases
bot secrets and stops the agent. Reset closes its old session and creates a
fresh one, including a short greeting turn (the backend spawn contract requires
an initial input). That turn uses the ordinary outbound path. Host restart
resumes the recorded backend session, independently of the ordinary
restore-agents preference. Session ownership remains recorded after reset or
unpair, so archived sessions cannot be adopted as another Tychat agent. Resume
failure is visible, never an implicit fresh conversation.

Agent-control and await MCP are always injected, even when ordinary injection
is disabled. The agent always runs unrestricted. It runs as the custom agent
chosen in settings, by default the builtin **Tyde Operator** (id `tyde-help`,
formerly Help), which also gets `tyde-config` and coordinates independent
top-level agents. Tychat instructions identify the owner/phone channel, request
short replies, and ask the agent to hand work to other top-level agents unless
the owner asks it to act directly; tool policy does not forbid direct work. The agent uses
`global: true` for host-wide list/read/await/send operations. Existing MCP
credential and authorization checks remain intact.

## Settings and secrets

The typed host settings contain enablement, custom agent (`None` is the plain
Default agent), backend, backend-native session values and optional launch
profile. A custom agent that no longer exists is rejected. Only backends whose
server declaration says they can steer mid-turn are admitted. Claude, Codex and
Hermes implement `SteerOutcome` today; other providers inherit Unsupported.
The frontend filters the server-published capability entries, not backend names.
Session controls reuse backend-defined schemas and launch-profile resolution.
A changed launch profile definition also explicitly applies on reset.
Live session settings use the existing acknowledged application path. Custom agent,
backend and profile changes explicitly display **Applies on reset**; they
never silently replace a live session. A pending backend change does not block
resuming the existing backend session. Reset remains available after a failed
resume, even without a live agent. Invalid settings fail before persistence.
Writes to the retired `/tychat/api_base_url` and `/tychat/access_mode` paths
are rejected. Settings and secret journals written by v0.9.5-beta.10 are
migrated on load to the Operator; a session started by beta.10 keeps running
as before and reports **Applies on reset** until it is reset.

Bot credentials, private keys, owner pin and cursors are serialized SDK
`BotState`, kept opaque outside the bridge. They are never settings,
bootstrap payloads, diagnostics or Debug output. Storage uses the same
`atomic_write_owner_only`/`enforce_owner_only_file` boundary as mobile pairings,
with 0600 permissions, beside the host stores. Fingerprints are public and are
published separately. The SDK owns their format (24 base64url characters in six
groups of four); the host re-derives them from `BotState` on load, so journals
holding the older 60-digit form show the current format after upgrade. Pairing codes are transient commands, never persisted.

Bridge status is typed: Unpaired, Connecting, Connected,
AwaitingOwnerConfirmation, Paused { reason }, Failed { reason }. The bridge maps
SDK trust and revocation events, including a paused saved state rejected by
`connect`; only re-pairing clears that trust stop. Verified owner succession
updates the durable SDK state and the displayed owner fingerprint. Untrusted messages
are dropped/count-only; their contents are never logged.

## Inbound contract

Only the bridge admits verified owner messages. Every accepted message carries
`MessageOrigin::Tychat` with its transport message identity. Claude, Codex and
Hermes carry the exact `SendMessagePayload.origin` into each admitted send or
steer user-message event. The agent actor never correlates origins with echoed
messages by order; UI input retains its own origin even when echoes are delayed.
Delivery is serialized at the agent actor, which owns actual turn and
pending-question state:

* Idle: start immediately.
* Running: steer into the current turn.
* NoActiveTurn: start immediately, without waiting for an idle event.
* Unsupported: interrupt, wait for authoritative turn completion, then start.
  This is an in-flight interrupt handoff, never the user-message queue.
* Closed or a compaction/admission barrier: explicit failure, never enqueue.
* Pending question: typed AskUserQuestion answer. Pending plan approval accepts
  explicit numbered approve/reject choices; ambiguous replies fail visibly.

The delivery receipt records Started, Steered, StartedAfterRace,
InterruptedThenStarted or Answered. IDs are deduplicated durably after acceptance;
read receipts follow acceptance. The secret journal first records verified
OwnerMessage events in a transport inbox, before persisting the SDK cursor that
covers them. This is crash recovery at the transport boundary, not the agent
message queue: live delivery always calls the dedicated always-steer API. Delivery interrupted by a host
crash before that durable receipt is an uncertain boundary, not an exactly-once
claim about model execution. Phase B must expose that uncertainty rather than
claiming it can transact with a provider.

## Outbound contract

Typing comes from server-owned activity, never transcript content. The actor
tracks each live turn from authoritative lifecycle events, retaining only its
latest assistant-visible final message in source order. A newer empty assistant
completion clears the previous candidate. On turn completion, it durably records
one outbox entry before advertising it to the bridge. Questions are published
as numbered text immediately so a blocked turn can receive an answer.
Proactive turns use exactly the same producer path.

Turn identities and outbox records survive restart. Message IDs are a
domain-separated deterministic digest of agent ID and turn ID (questions also
include the canonical tool ID). Retry sends exactly the stored ID and body.
Acknowledgement removes the pending item durably; a send-success/ack-crash
retries the same ID and relies on BotClient's idempotent append. This is
at-least-once transport with once-only user-visible delivery, not a fictional
transaction spanning Tyde and Tychat. Replayed session history never produces
outbound items.

Replies longer than 8000 UTF-16 code units are split at Unicode scalar
boundaries into labelled `[Part N]` messages, counting the label against the
limit. The labels also allow whitespace-only spans without losing whitespace.
IDs are the first 16 bytes of Blake2s256 over `tyde.tychat.chunk.v1\0`, the
original outbound ID, and the zero-based big-endian u64 chunk index. The original
journal entry is acknowledged only after every part succeeds. Restart can retry
all parts safely without needing a second progress ledger or truncating text.

## SDK actor lifecycle

Pair uses the same typed host Settings command path as the UI. Settings applies
are serialized across redemption so a spent pairing code cannot lose its state
to a concurrent URL change. The redeemed state is persisted before session
startup or connection. A process lock prevents two hosts using the same bot
state while paired; unpaired hosts retain no bot-ownership lock. Unpair waits for the client to stop before erasing the journal. Owner
revocation (including any post-connect HTTP 401) erases the state and singleton.

SDK events are drained independently of delivery and outbound HTTP operations;
a slow model or append must not block StateChanged persistence. Shutdown takes
`client.state()`, journals queued owner events through its cursor, persists the
snapshot, then drops the SDK client. Host restart waits for this lifecycle before
releasing the process lock. The pending outbox and inbox remain durable.

The SDK owns websocket reconnection. Disconnected publishes Connecting, not a
new parallel socket. This SDK revision does not re-emit Ready for an unchanged
conversation on reconnect; a successful authenticated typing refresh provides
the explicit acknowledgement for returning to Connected. Transient SDK requests
retry after a bounded delay with unchanged message IDs; trust, crypto, malformed
state and conflicting-ID failures stop visibly rather than retrying blindly.
Only safe error categories and dropped-envelope counts enter diagnostics.

## Bridge-facing Rust boundary

The server exports `HostHandle` and `server::tychat::SecretBotState(Vec<u8>)`.
All other argument/result types below come from `protocol`; `watch` is
`tokio::sync::watch`. These are the public `HostHandle` signatures:

```rust
pub async fn tychat_settings(&self) -> Result<TychatSettings, String>;
pub async fn tychat_state(&self) -> TychatStatePayload;
pub async fn subscribe_tychat(&self) -> watch::Receiver<u64>;
pub async fn tychat_bot_state(
    &self,
) -> Option<(TychatPairingId, String, SecretBotState)>;
pub async fn install_tychat_pairing(
    &self,
    secret: SecretBotState,
    fingerprints: TychatFingerprints,
) -> Result<TychatPairingId, String>;
pub async fn persist_tychat_bot_state(
    &self,
    generation: &TychatPairingId,
    secret: SecretBotState,
) -> Result<(), String>;
pub async fn set_tychat_bridge_status(
    &self,
    generation: &TychatPairingId,
    status: TychatBridgeStatus,
) -> Result<(), String>;
pub async fn tychat_outbound(
    &self,
    generation: &TychatPairingId,
) -> Result<TychatOutboundSnapshot, String>;
pub async fn acknowledge_tychat_outbound(
    &self,
    generation: &TychatPairingId,
    message_id: TychatOutboundId,
) -> Result<(), String>;
pub async fn deliver_tychat_message(
    &self,
    generation: &TychatPairingId,
    message: TychatOwnerMessage,
) -> Result<TychatDeliveryReceipt, String>;
```

`tychat_bot_state` returns the pairing generation, API URL and opaque secret
snapshot. A new pairing gets a new typed ID; stale callbacks after unpair or
re-pair fail rather than updating another owner's state. Install only after
successful SDK redemption and verification. Successful installation persists the
pairing even if session startup fails; that failure is visible in bridge status.
The bridge owns connection/trust status and must sanitize SDK error reasons
before publishing them. Only Connected admits inbound messages.

Subscribe before reading settings/state/outbound snapshots. A watch revision is
a wake to reread current state, not a transport event or a polling interval.
Persist SDK StateChanged snapshots through `persist_tychat_bot_state`; pass only
verified OwnerMessage events into `deliver_tychat_message`. Send pending outbox
items using their existing IDs; acknowledge each only after SDK success. The
actor must stop transport and suppress sends while disabled, paused, awaiting
confirmation or revoked. Pair/unpair/reset enter through the typed host `TychatCommandPayload`.

Protocol version 77 adds `AgentOrigin::Tychat`, `MessageOrigin::Tychat`, optional
`ChatMessage.origin`, typed settings/status/application/capabilities, pairing and
message/turn IDs, delivery paths/receipts, outbound snapshots, and the host
TychatCommand/TychatState frames. HostBootstrap includes the same TychatState
shape used for live updates. No secret state appears in either frame.

## Validation

Real-server protocol sim flows cover singleton ownership, resume/reset, settings
validation and application, capability filtering, all delivery dispositions,
pending typed answers, and durable outbound acknowledgement/retry after restart.
They extend the existing mock backend strictly above the backend boundary.
The Settings tab is exercised in a mounted Leptos DOM under headless Chrome.
Regression sensitivity is demonstrated by disabling the guarded behavior and
running the canonical check, then restoring it. Ordinary validation is only
`./dev.sh check`. The separate opt-in integration flow uses the actual local
Tychat binary and SDK owner kit; it is ignored by the ordinary gate. No paid
provider calls are needed for the host-only paths; typed origin emission is additionally
verified through real backend conformance on Claude, Codex and Hermes.

```sh
TYDE_RUN_TYCHAT_TESTS=1 TYCHAT_REPO=/home/tyggs/Tychat/Tychat-bots \
  tools/run-tychat-integration.sh
```

`real_tychat_pair_steer_chunk_restart_and_revoke` starts the real local server
with an in-memory store and dev identities. It redeems a real code through the
host protocol, confirms both fingerprints, steers an owner message into a held
mock turn, verifies a real encrypted read receipt, and reconstructs a long
Unicode/whitespace reply from actual owner-decrypted chunks. A test-only barrier
holds the local outbox acknowledgement after successful real appends; restarting
Tyde must resend without creating another Tychat message. A subsequent turn and
owner revocation cover recovery and credential erasure. Re-pairing, confirming a
second bot and explicitly unpairing the connected SDK cover user-driven teardown;
another Tyde restart proves the erased singleton stays gone. The server child process
group is owned by the test and killed on success or failure.

### Flow inventory and regression evidence

`server/tests/tychat.rs` runs against the real server and protocol fixture:

* `singleton_settings_resume_reset_and_secret_boundary`: one host-owned agent,
  forced MCP, close/archive protections, live and reset-only settings, backend
  filtering, disable/re-enable, pending backend change across restart, fresh
  reset, paused delivery, secret permissions and unpair erasure.
* `always_steer_and_outbox_survive_restart`: idle start, held-turn steering,
  typed message origin, inbound deduplication, no queue, server typing,
  deterministic final ID/body, restart replay and durable acknowledgement.
* `unsupported_interrupts_and_questions_use_typed_answers`: interrupt handoff,
  numbered plan/question output, ambiguous approval rejection, typed plan and
  freeform answers, and no backend-boundary violations.
* `turn_end_race_starts_without_queueing`: backend-controlled NoActiveTurn race,
  immediate next turn, distinct final IDs and no interrupt/queue fallback.

The mounted desktop flow
`tychat_tab_renders_host_truth_and_sends_typed_changes` covers typed host-state
updates, capability-filtered choices, backend-native session controls, settings
writes, transient pairing commands, fingerprints, pending settings, trust pause
and reset availability after resume failure.

The canonical check was deliberately run with each guarded behavior removed.
In `run-20261009T085311Z-1843205/11-cargo-nextest-run.log`, removing close guards,
discarding the persisted outbox on load, rejecting typed answers, and rejecting
the NoActiveTurn start produced exactly the four corresponding failures (540
other native tests passed). In
`run-20261009T084640Z-1266188/12-wasm-browser-tests.log`, admitting Unsupported
backends made the DOM flow reject the rendered Kiro option (835 other desktop
flows passed). All mutations were restored. Logs are in the bounded
`target/dev-check-logs` store; these run IDs record the sensitivity evidence,
not permanent fixtures or alternative validation commands.

The expanded run also exposed a fixture clock mismatch: the paused-time queue
flow `ordinary_message_withdraws_async_question_in_live_and_replayed_state`
timed out its mobile bootstrap after 1.48 seconds of wall time because Tokio
advanced its five-second virtual deadline while real store I/O was outstanding
(`run-20261009T083627Z-20617`). The mobile connector now shares the desktop
connector's real-wallclock bootstrap helper and exact first-frame assertion.
No timeout was increased and no behavioral assertion was removed.

The real integration flow initially failed its held-turn typing assertion. A
retained diagnostic showed canonical activity `Thinking` but projected typing
`false`: singleton admission had missed the initial status transition. Admission
now replays the same canonical status fanout used by live events, without reading
history or inferring activity. The unchanged assertion and the full real-server
flow pass with that fix.

An initial Phase B ordinary gate also found that retaining the bot process lock
for unpaired hosts rejected 36 existing fresh-host flows. Lock ownership now
starts with a pairing and ends after SDK shutdown/unpair; unpaired host lifecycle
is unchanged. No existing assertions or coverage were removed.

Phase B's expanded real integration passed in 9.097 seconds (nextest run
`ca6482ee-f7fd-43dc-bfd0-a7f3c2120d57`), including the connected-SDK unpair path.
Live dev-instance QA was retried: the 105-second launcher deadline expired while
Trunk compiled the frontend and Tauri waited for `http://127.0.0.1:33575/`.
No UI-debug endpoint became ready; the instance list was empty after cleanup.
This is blocked manual UI evidence, not a claimed live-UI pass.

The full Phase B check passed in `run-20261009T102305Z-2347257`: 544 native,
836 desktop DOM, 321 mobile DOM and the RTC smoke flow. An earlier run also
caught redundant TychatState emission for unrelated backend changes on unpaired
hosts; those changes no longer wake a nonexistent bot transport. The same run
saw two existing project-watcher flows fail during overflow rescans. Their
assertions and behavior are unchanged; both passed on the diagnostic rerun,
which showed no overflow for those flows. No watcher fix is claimed; the
unrelated diagnostics were removed during review.

## Origin review regression

The always-steer sim flow now interleaves a UI-started turn with a Tychat steer
while the mock transport holds complete typed user echoes. The original FIFO
mislabelled the UI message, failing `UI input must not consume a Tychat origin
while its echo is in flight` in `run-20261009T104741Z-3677002`; the other 543
native tests passed. The mock holds whole events only to model transport latency;
production has no origin queue or order-based correlation.

The real `real_mid_turn_steering` conformance flow also checks exact distinct
origins for send and steer on all three capability-eligible backends, alongside
its existing foreground-tool completion and same-turn reply assertions.

The origin conformance extension failed on Claude, Codex and Hermes before the
emitter fix (run `c5723cca-ad73-4765-a076-eda96d2dc60f`) and passed on all three
afterward (run `3111c838-7580-4a3b-a747-f58567f17033`, 39.023 seconds). No provider
scenario or oracle differs between those backends. The unchanged default-origin
calls for ACP and Antigravity only adapt to the typed emitter signature.

Review-time live QA retried the workbench launcher as its build warmed. The
first attempt completed frontend compilation at 101 seconds but hit the launch
deadline; the next reached native dependency compilation. The third finished
the native build and opened the desktop webview, UI-debug endpoint and host
listener, but the launcher repeatedly received
`IncompatibleProtocol { client: 76, server: 77 }`. The launcher client therefore
cannot register this protocol-77 dev instance. Cleanup left the instance list
empty. Settings inspection and the UI-visible Tychat round trip remain blocked,
not passed. The capabilities endpoint also reports `screenshot: false`.

The post-review real Tychat integration passed again in 8.772 seconds (run
`12ecfa33-5f9a-41b9-9057-c514c402c34f`): pairing/confirmation, held-turn steering,
read receipts, complete Unicode chunks, duplicate-free restart retry, subsequent
turn, revocation, re-pair and connected-SDK unpair all completed.
