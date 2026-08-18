//! Coarse, high-value real-backend conformance tests.
//!
//! **Few conversations, many assertions.** `backend.rs` spends one paid
//! conversation per assertion, which is why it is too expensive to run often
//! enough to catch anything. Here one conversation produces one event stream and
//! every check runs over that stream offline, so the assertion count can grow
//! without the API bill growing with it.
//!
//! **The filesystem is an out-of-band oracle.** Checks that read only the event
//! stream are structurally blind to a *dropped* event: `TurnEmitter` enforces the
//! tool lifecycle by deleting non-conformant events, so a stream missing a tool
//! card is conformant by construction. The file on disk says the tool ran; the
//! stream says whether the UI was ever told. Comparing them is the only way to
//! see an absence.
//!
//! **Prompts name a goal, not a tool**, so each provider picks its own tool and
//! the assertions run against Tyde's normalized form.
//!
//! **Capability gates are load-bearing.** Ungated, the background scenario ran
//! against Tycode — which cannot background anything — and reported a backend
//! doing the only thing it can do as a defect.
//!
//! Paid: `#[ignore]`d and additionally gated on `TYDE_RUN_REAL_AI_TESTS=1`, with
//! `TYDE_REAL_BACKENDS` narrowing the providers. See `AGENTS.md` §3.
//!
//! Driving a backend lives in `conformance_fixture`; what is guaranteed lives
//! here.

mod conformance_fixture;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use protocol::{BackendKind, ChatEvent, MessageSender, ToolExecutionOutcome};
use tyde_agent_adapter::BackendCapability;
use uuid::Uuid;

use conformance_fixture::*;

const READY_MARKER: &str = "TYDE_READY";
const WROTE_MARKER: &str = "TYDE_WROTE";
const MULTI_MARKER: &str = "TYDE_MULTI";
const BG_MARKER: &str = "TYDE_BG";
const WAITED_MARKER: &str = "TYDE_WAITED";
const HELLO_FILE: &str = "hello.txt";
const BG_FILE: &str = "background.txt";

/// Three files via three tool calls. Single-tool turns miss a whole class of
/// defect: Codex joins a `commandExecution` back to its declaration through
/// `claim_unambiguous_raw_exec_owner_for_turn` (`codex.rs:2946`), which claims
/// nothing when a turn holds more than one unclaimed candidate — so a turn with
/// two or more shell calls orphans every one of them.
const MULTI_FILES: [&str; 3] = ["multi_a.txt", "multi_b.txt", "multi_c.txt"];

/// A 20s command plus whatever polling interval the backend uses to notice it
/// finished.
const BG_SETTLE: Duration = Duration::from_secs(60);

#[test]
#[ignore = "paid real-backend suite; use --run-ignored all with TYDE_RUN_REAL_AI_TESTS=1"]
fn real_conversation() {
    run_scenario(&[], |mut host| async move {
        let payload = unique_payload();
        let agent = spawn_agent(&mut host, &launch_prompt()).await;

        let launched = collect_turn(&mut host, &agent, &launch_prompt()).await;
        let wrote = ask(&mut host, &agent, write_prompt(&payload)).await;
        let read_back = ask(&mut host, &agent, read_prompt()).await;
        let multi = ask(&mut host, &agent, multi_tool_prompt()).await;

        assert_final_text_contains(&launched, READY_MARKER);
        assert_wrote_file(&wrote, host.workspace(), &payload);
        assert_final_text_contains(&wrote, WROTE_MARKER);
        assert_read_back_payload(&read_back, &payload);
        assert_multi_tool_turn(&multi, host.workspace());
        assert_universal_contract(&[launched, wrote, read_back, multi]);

        let session = stored_session(&mut host).await;
        assert!(
            session.message_count > 0,
            "{:?}: stored session recorded zero assistant responses",
            host.backend()
        );
        assert_eq!(
            session.workspace_roots,
            host.workspace_roots(),
            "{:?}: stored session lost its workspace roots",
            host.backend()
        );

        assert_clean_close(&mut host, &agent).await;
    });
}

#[test]
#[ignore = "paid real-backend suite; use --run-ignored all with TYDE_RUN_REAL_AI_TESTS=1"]
fn real_conversation_on_resumed_session() {
    run_scenario(&[BackendCapability::ResumeSession], |mut host| async move {
        let payload = unique_payload();

        let source = spawn_agent(&mut host, &launch_prompt()).await;
        let launched = collect_turn(&mut host, &source, &launch_prompt()).await;
        let wrote = ask(&mut host, &source, write_prompt(&payload)).await;
        assert_wrote_file(&wrote, host.workspace(), &payload);
        assert_final_text_contains(&wrote, WROTE_MARKER);
        assert_universal_contract(&[launched, wrote]);
        assert_clean_close(&mut host, &source).await;

        let session = stored_session(&mut host).await;
        assert!(
            session.resumable,
            "{:?}: session is not resumable, so the rest of this scenario cannot run",
            host.backend()
        );

        // Bootstrap replay and the paged history are different server code
        // paths and have broken independently; the UI uses both.
        let resumed = resume_agent(&mut host, &session.id).await;
        assert_replayed_history_is_not_empty(&resumed, host.backend());
        let page = history_page(&mut host, &resumed).await;
        assert!(
            !page.events.is_empty(),
            "{:?}: fetch_session_history returned zero events for a session that already \
                 completed turns. Scrolling back shows nothing.",
            host.backend()
        );

        // Resumed sessions rendering blank is one bug; resumed sessions
        // silently losing every subsequent tool card is a worse one.
        let follow_up = ask(&mut host, &resumed, read_prompt()).await;
        assert_read_back_payload(&follow_up, &payload);
        assert!(
            follow_up.tool_requests().next().is_some(),
            "{}: a new turn on a resumed session emitted zero tool requests while reading \
                 {HELLO_FILE}; the model answered from a tool whose card never reached the client",
            follow_up.label()
        );
        assert_universal_contract(&[follow_up]);

        assert_clean_close(&mut host, &resumed).await;
    });
}

/// A tool still running when the turn ends.
///
/// Deliberately does *not* assert that every request completed — a backgrounded
/// command legitimately outlives its turn. It asserts that the turn ending does
/// not corrupt the stream, which is the ground on which a still-open tool gets
/// cancelled or its late completion rejected.
#[test]
#[ignore = "paid real-backend suite; use --run-ignored all with TYDE_RUN_REAL_AI_TESTS=1"]
fn real_background_task_outlives_its_turn() {
    run_scenario(
        &[BackendCapability::BackgroundTasks],
        |mut host| async move {
            let prompt = background_prompt(host.backend());
            let bg_path = host.workspace().join(BG_FILE);
            let agent = spawn_agent(&mut host, &prompt).await;
            let started = collect_turn(&mut host, &agent, &prompt).await;

            assert_no_error_message(&started.label(), started.events());
            assert_streams_are_balanced(&started);
            assert_reached_idle(&started);
            assert_final_text_contains(&started, BG_MARKER);

            // "Backgrounded a 20s sleep" and "never ran anything" both finish
            // fast and both satisfy the stream assertions, so without this a
            // green result means nothing.
            let requests = started.tool_requests().count();
            assert!(
                requests >= 1,
                "{}: emitted zero tool requests, so no command was ever started and this test \
                 asserted nothing",
                started.label()
            );
            // An unfinished *file* is not evidence the command outlived the
            // turn: Codex passed that check while backgrounding nothing, because
            // its detached subshell was reaped, the card completed in-turn, and
            // the file never appeared. Require an open card.
            let completions = started.tool_completions().count();
            assert!(
                requests > completions,
                "{}: completed all {requests} of its tool requests before the turn ended, so \
                 nothing outlived the turn and turn-end teardown was never exercised",
                started.label()
            );
            assert!(
                !bg_path.is_file(),
                "{}: found {} already written when the turn ended, so the command did not outlive \
                 its turn and this test did not exercise turn-end teardown",
                started.label(),
                bg_path.display()
            );

            // The failure shape needs a *later* turn in flight when the
            // background task reports its terminal state. Merely idling until it
            // finishes was tried and left the stream clean.
            let waited = ask(&mut host, &agent, wait_prompt()).await;
            assert_no_error_message(&waited.label(), waited.events());
            assert_final_text_contains(&waited, WAITED_MARKER);
            // A backend that declines to run the command produces a clean, fast,
            // meaningless pass — which is what happened when this prompt was a
            // bare `sleep`.
            assert!(
                waited.tool_requests().next().is_some(),
                "{}: emitted zero tool requests, so nothing was in flight to overlap the \
                 background task and this scenario asserted nothing",
                waited.label()
            );

            let settled = drain_events_for(&mut host, BG_SETTLE).await;
            assert!(
                bg_path.is_file(),
                "{}: waited {}s and {} was still not written, so the background command never \
                 finished and the late-completion path was never exercised",
                started.label(),
                BG_SETTLE.as_secs(),
                bg_path.display()
            );
            assert_no_error_message(&format!("{:?} background settle", host.backend()), &settled);

            assert_clean_close(&mut host, &agent).await;
        },
    );
}

#[test]
#[ignore = "paid real-backend suite; use --run-ignored all with TYDE_RUN_REAL_AI_TESTS=1"]
fn real_conversation_in_native_subagent() {
    run_scenario(
        &[BackendCapability::ForegroundSubagents],
        |mut host| async move {
            let payload = unique_payload();
            let prompt = subagent_prompt(&payload);
            let agent = spawn_agent(&mut host, &prompt).await;
            let delegated = collect_turn(&mut host, &agent, &prompt).await;

            assert_wrote_file(&delegated, host.workspace(), &payload);
            assert_read_back_payload(&delegated, &payload);

            let spawns = delegated
                .tool_requests()
                .filter(|request| {
                    matches!(
                        request.tool_type,
                        protocol::ToolRequestType::AgentSpawn { .. }
                    )
                })
                .count();
            assert!(
                spawns > 0,
                "{}: declares ForegroundSubagents but the turn emitted no normalized AgentSpawn \
                 request; delegated work produced no sub-agent card. Tool requests seen: {:?}",
                delegated.label(),
                delegated.tool_request_names()
            );
            assert_universal_contract(&[delegated]);

            assert_clean_close(&mut host, &agent).await;
        },
    );
}

fn launch_prompt() -> String {
    format!("Reply with exactly {READY_MARKER} and nothing else. Do not use any tools.")
}

fn write_prompt(payload: &str) -> String {
    format!(
        "Create a file named {HELLO_FILE} in the workspace root whose entire contents are exactly \
         {payload} followed by a newline. Then reply with exactly {WROTE_MARKER} and nothing else."
    )
}

fn read_prompt() -> String {
    format!(
        "Read the file {HELLO_FILE} from the workspace root and reply with exactly its contents \
         and nothing else."
    )
}

/// Claude's `Bash` tool has a first-class background mode, so asking generically
/// reaches it. Codex has no such flag — a command becomes a background task
/// there by still executing when the model starts its reply, which
/// `promote_root_commands_before_agent_response` then promotes. Asked
/// generically, spark ran `/bin/zsh -lc '(sleep 20; echo DONE > f) &'`, whose
/// outer shell exits immediately and whose subshell the sandbox reaps: nothing
/// was promoted and the file was never written.
fn background_prompt(backend_kind: BackendKind) -> String {
    let launch = match backend_kind {
        BackendKind::Codex => format!(
            "Run this exact shell command: sleep 20; echo DONE > {BG_FILE}. Run it as an \
             ordinary foreground command in the workspace root — do not append `&`, and do \
             not use `nohup`, `disown`, or a detached subshell. Do not wait for its output."
        ),
        _ => format!(
            "Start a shell command that sleeps for 20 seconds and then writes the word DONE \
             into a file named {BG_FILE} in the workspace root. Run it in the background and \
             do not wait for it to finish."
        ),
    };
    format!("{launch} As soon as it is started, reply with exactly {BG_MARKER} and nothing else.")
}

/// Wrapped in an interpreter rather than phrased as a bare `sleep`: Claude's
/// Bash tool refuses a long *leading* sleep ("Long leading `sleep` commands are
/// blocked"), which made an earlier version of this prompt run nothing at all
/// and still pass.
fn wait_prompt() -> String {
    format!(
        "Run this exact shell command and wait for it to finish — do not run it in the \
         background: python3 -c \"import time; time.sleep(25); print('OK')\". \
         Then reply with exactly {WAITED_MARKER} and nothing else."
    )
}

fn multi_tool_prompt() -> String {
    let [a, b, c] = MULTI_FILES;
    format!(
        "Create three files in the workspace root: {a} containing exactly A, {b} containing \
         exactly B, and {c} containing exactly C. Use a separate tool call for each file — do not \
         combine them into a single command. Then reply with exactly {MULTI_MARKER} and nothing \
         else."
    )
}

fn subagent_prompt(payload: &str) -> String {
    format!(
        "Delegate the following task to a single sub-agent and wait for it to finish: create a \
         file named {HELLO_FILE} in the workspace root whose entire contents are exactly \
         {payload} followed by a newline, then read that file back. When the sub-agent is done, \
         reply with exactly the contents of {HELLO_FILE} and nothing else."
    )
}

/// A nonce, so a stale file from an earlier run can never satisfy the oracle.
fn unique_payload() -> String {
    let uuid = Uuid::new_v4().simple().to_string();
    format!("TYDE_PAYLOAD_{}", uuid[..12].to_ascii_uppercase())
}

/// The guarantees that hold for every backend on every turn.
///
/// A malformed stream is the cause and a wrong outcome is the symptom, so the
/// scenario checks in each test run first and this runs last: reporting "the
/// file was not written" when the emitter already rejected the stream sends the
/// reader after the wrong thing.
fn assert_universal_contract(turns: &[Turn]) {
    assert!(!turns.is_empty(), "conversation produced no turns at all");
    for turn in turns {
        assert_no_error_message(&turn.label(), turn.events());
        assert_streams_are_balanced(turn);
        assert_every_request_was_declared(turn);
        assert_every_request_completed_exactly_once(turn);
        assert_no_completion_without_request(turn);
        assert_reached_idle(turn);
    }
    assert_text_was_streamed(turns);
    assert_tool_call_ids_are_unique(turns);
    assert_reported_model_is_pinned(turns);
}

/// Also the emitter-violation check: `TurnEmitter` aggregates the protocol
/// violations it caught into one Error card just before the turn goes idle.
fn assert_no_error_message(label: &str, events: &[ChatEvent]) {
    for event in events {
        if let ChatEvent::MessageAdded(message) = event
            && matches!(message.sender, MessageSender::Error)
        {
            panic!("{label}: emitted an Error message: {:?}", message.content);
        }
    }
}

/// A nested `StreamStart` means two provider responses were conflated into one,
/// which is how tool ownership gets lost.
fn assert_streams_are_balanced(turn: &Turn) {
    let mut open = false;
    let mut starts = 0usize;
    let mut ends = 0usize;
    for event in turn.events() {
        match event {
            ChatEvent::StreamStart(_) => {
                assert!(
                    !open,
                    "{}: StreamStart arrived while another assistant response was still open",
                    turn.label()
                );
                open = true;
                starts += 1;
            }
            ChatEvent::StreamEnd(_) => {
                assert!(
                    open,
                    "{}: StreamEnd closed a response that was never started",
                    turn.label()
                );
                open = false;
                ends += 1;
            }
            _ => {}
        }
    }
    assert!(
        !open,
        "{}: ended with an assistant response still open ({starts} StreamStart, {ends} StreamEnd)",
        turn.label()
    );
    assert!(
        starts > 0,
        "{}: produced no assistant response at all",
        turn.label()
    );
}

/// Set equality, not ordering. `TurnEmitter` does treat declaration as a hard
/// precondition, but it is constructed only in `claude.rs`, `codex.rs` and
/// `acp/backend.rs` — Hermes builds `ChatEvent::ToolRequest` directly
/// (`hermes.rs:4757`) and emits it before the `StreamEnd` that declares it.
/// Asserting order would encode an emitter detail as a universal law.
fn assert_every_request_was_declared(turn: &Turn) {
    let mut declared = BTreeSet::new();
    for event in turn.events() {
        let tool_calls = match event {
            ChatEvent::StreamEnd(end) => &end.message.tool_calls,
            ChatEvent::MessageAdded(message) => &message.tool_calls,
            _ => continue,
        };
        declared.extend(tool_calls.iter().map(|call| call.tool_call_id.clone()));
    }
    let requested: BTreeSet<_> = turn
        .tool_requests()
        .map(|request| request.tool_call_id.clone())
        .collect();

    let undeclared: Vec<_> = requested.difference(&declared).collect();
    assert!(
        undeclared.is_empty(),
        "{}: tool request(s) {undeclared:?} were never declared by any assistant response in the \
         turn; declared ids: {declared:?}",
        turn.label()
    );
    let unrequested: Vec<_> = declared.difference(&requested).collect();
    assert!(
        unrequested.is_empty(),
        "{}: tool(s) {unrequested:?} were declared by an assistant response but never became a \
         tool request, so the card was promised and never arrived; requested ids: {requested:?}",
        turn.label()
    );
}

fn assert_every_request_completed_exactly_once(turn: &Turn) {
    let mut completions: BTreeMap<&str, usize> = BTreeMap::new();
    for completion in turn.tool_completions() {
        *completions
            .entry(completion.tool_call_id.as_str())
            .or_default() += 1;
    }
    for request in turn.tool_requests() {
        let count = completions
            .get(request.tool_call_id.as_str())
            .copied()
            .unwrap_or(0);
        assert_eq!(
            count,
            1,
            "{}: tool request {:?} ({}) has {count} completions, expected exactly 1. A missing \
             completion leaves the card spinning forever; a duplicate means two owners believe \
             they ran it.",
            turn.label(),
            request.tool_call_id,
            tool_kind(request)
        );
    }
}

fn assert_no_completion_without_request(turn: &Turn) {
    let requested: BTreeSet<_> = turn
        .tool_requests()
        .map(|request| request.tool_call_id.as_str())
        .collect();
    for completion in turn.tool_completions() {
        assert!(
            requested.contains(completion.tool_call_id.as_str()),
            "{}: completion for {:?} has no matching tool request; requested ids: {requested:?}",
            turn.label(),
            completion.tool_call_id
        );
    }
}

fn assert_reached_idle(turn: &Turn) {
    assert!(
        turn.events()
            .iter()
            .any(|event| matches!(event, ChatEvent::TypingStatusChanged(false))),
        "{}: never reported going idle",
        turn.label()
    );
}

/// A backend that streams nothing renders as a frozen UI that snaps to a
/// finished answer. Asserted per conversation, not per turn, because a short
/// turn legitimately arrives in one chunk.
fn assert_text_was_streamed(turns: &[Turn]) {
    let deltas = turns
        .iter()
        .flat_map(Turn::events)
        .filter(|event| matches!(event, ChatEvent::StreamDelta(_)))
        .count();
    assert!(
        deltas > 0,
        "{}: emitted zero StreamDelta events across the entire conversation; nothing streamed",
        turns[0].label()
    );
}

/// Spans the conversation: a reused id makes two tool calls indistinguishable,
/// so a completion can be attributed to a card from an earlier turn.
fn assert_tool_call_ids_are_unique(turns: &[Turn]) {
    let mut seen: BTreeMap<&str, String> = BTreeMap::new();
    for turn in turns {
        for request in turn.tool_requests() {
            if let Some(previous) = seen.insert(request.tool_call_id.as_str(), turn.label()) {
                panic!(
                    "{}: tool_call_id {:?} was already used by {previous}",
                    turn.label(),
                    request.tool_call_id
                );
            }
        }
    }
}

/// A run that quietly escalates to an expensive model is a bill, not a result.
fn assert_reported_model_is_pinned(turns: &[Turn]) {
    let expected = pinned_models(turns[0].backend());
    if expected.is_empty() {
        return;
    }
    let mut reported = BTreeSet::new();
    for event in turns.iter().flat_map(Turn::events) {
        let model = match event {
            ChatEvent::StreamStart(start) => start.model.clone(),
            ChatEvent::StreamEnd(end) => end
                .message
                .model_info
                .as_ref()
                .map(|info| info.model.clone()),
            ChatEvent::MessageAdded(message) => {
                message.model_info.as_ref().map(|info| info.model.clone())
            }
            _ => None,
        };
        if let Some(model) = model {
            reported.insert(model);
        }
    }
    assert!(
        !reported.is_empty(),
        "{}: never reported which model it ran, so the pin cannot be verified",
        turns[0].label()
    );
    for model in &reported {
        assert!(
            expected.contains(model),
            "{}: ran an unpinned model {model:?}; expected one of {expected:?}",
            turns[0].label()
        );
    }
}

/// Asserts the shape of the turn as well as its outcome:
/// `assert_every_request_completed_exactly_once` can only catch an orphaned card
/// if the turn actually issued more than one tool call, so without the count
/// check this passes vacuously whenever a provider batches the work.
fn assert_multi_tool_turn(turn: &Turn, workspace: &Path) {
    let requests = turn.tool_requests().count();
    assert!(
        requests >= 2,
        "{}: asked for three files via separate tool calls but the turn emitted {requests} tool \
         request(s) {:?}; this turn exists to exercise multi-tool turns and asserts nothing if \
         the provider batches them",
        turn.label(),
        turn.tool_request_names()
    );

    let missing: Vec<_> = MULTI_FILES
        .iter()
        .filter(|name| !workspace.join(name).is_file())
        .collect();
    assert!(
        missing.is_empty(),
        "{}: {missing:?} were never written, though the turn emitted {requests} tool request(s) \
         and {} completion(s)",
        turn.label(),
        turn.tool_completions().count()
    );

    assert_final_text_contains(turn, MULTI_MARKER);
}

/// The out-of-band check. `TurnEmitter` sanitizes the stream, so a card it
/// dropped looks exactly like a card the model never requested. The workspace
/// does not lie: if the file is there, a tool ran and the client was owed a card.
fn assert_wrote_file(turn: &Turn, workspace: &Path, payload: &str) {
    let path = workspace.join(HELLO_FILE);
    let contents = std::fs::read_to_string(&path).ok();
    let file_has_payload = contents
        .as_deref()
        .is_some_and(|contents| contents.contains(payload));
    let succeeded = turn
        .tool_completions()
        .any(|completion| matches!(completion.outcome, ToolExecutionOutcome::Succeeded { .. }));

    assert!(
        !(file_has_payload && !succeeded),
        "{}: {} contains the expected payload, so a tool really did write it — but the turn \
         emitted no successful tool completion. The tool ran and the client was never told: the \
         card was dropped between the backend and the chat stream. Requests seen: {:?}",
        turn.label(),
        path.display(),
        turn.tool_request_names()
    );
    assert!(
        file_has_payload,
        "{}: {} does not contain {payload:?} (contents: {contents:?}); the turn emitted {:?} and \
         {} completion(s)",
        turn.label(),
        path.display(),
        turn.tool_request_names(),
        turn.tool_completions().count(),
    );
    assert!(
        turn.tool_requests().next().is_some(),
        "{}: {} was written but the turn emitted zero tool requests",
        turn.label(),
        path.display()
    );
}

/// Echoing the file back proves the tool *result* travelled into the model, not
/// merely that a card was rendered — a backend can emit a perfectly shaped
/// completion whose payload never reaches the provider.
fn assert_read_back_payload(turn: &Turn, payload: &str) {
    let final_text = turn.final_text();
    assert!(
        final_text.contains(payload),
        "{}: final response {final_text:?} does not contain {payload:?}. The model never received \
         the tool output it asked for. Completions in this turn: {:?}",
        turn.label(),
        turn.completion_summaries()
    );
}

fn assert_final_text_contains(turn: &Turn, needle: &str) {
    let final_text = turn.final_text();
    assert!(
        final_text.contains(needle),
        "{}: final response {final_text:?} does not contain {needle:?}",
        turn.label()
    );
}

/// Shutdown is the emitter's last flush, so a violation recorded after the final
/// turn ended surfaces here or nowhere.
async fn assert_clean_close(host: &mut Host, agent: &Agent) {
    let label = format!("{:?} shutdown", host.backend());
    let closing = close_agent(host, agent).await;
    assert_no_error_message(&label, &closing);
}

fn assert_replayed_history_is_not_empty(agent: &Agent, backend_kind: BackendKind) {
    let user_messages = agent
        .replayed_history
        .iter()
        .filter(|event| {
            matches!(event, ChatEvent::MessageAdded(message)
                if matches!(message.sender, MessageSender::User))
        })
        .count();
    let responses = agent
        .replayed_history
        .iter()
        .filter(|event| matches!(event, ChatEvent::StreamEnd(_) | ChatEvent::MessageAdded(_)))
        .count();
    assert!(
        user_messages > 0 && responses > 0,
        "{backend_kind:?}: the resumed agent's bootstrap replayed no prior conversation \
         ({user_messages} user message(s), {responses} message event(s) in {} replayed events). \
         A resumed session that renders blank has lost the user's history.",
        agent.replayed_history.len()
    );
}
