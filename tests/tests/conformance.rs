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

use protocol::{
    BackendKind, ChatEvent, ContextCompactionStatus, MessageSender, SessionId,
    ToolExecutionOutcome, ToolExecutionResult, ToolRequestType,
};
use tyde_agent_adapter::BackendCapability;
use uuid::Uuid;

use conformance_fixture::*;

const READY_MARKER: &str = "TYDE_READY";
const WROTE_MARKER: &str = "TYDE_WROTE";
const MULTI_MARKER: &str = "TYDE_MULTI";
const BG_MARKER: &str = "TYDE_BG";
const WAITED_MARKER: &str = "TYDE_WAITED";
const DELETED_MARKER: &str = "TYDE_DELETED";
const WORKFLOW_MARKER: &str = "TYDE_WORKFLOW";
const MEMORIZED_MARKER: &str = "TYDE_MEMORIZED";
const HELLO_FILE: &str = "hello.txt";
const BG_FILE: &str = "background.txt";
const MAPPING_FILE: &str = "mapping.txt";
const MAPPED_CREATE_MARKER: &str = "TYDE_MAPPED_CREATE";
const MAPPED_EDIT_MARKER: &str = "TYDE_MAPPED_EDIT";
const MAPPED_RUN_MARKER: &str = "TYDE_MAPPED_RUN";
const MAPPED_DELETE_MARKER: &str = "TYDE_MAPPED_DELETE";

/// Three files via three tool calls. Single-tool turns miss a whole class of
/// defect: Codex joins a `commandExecution` back to its declaration through
/// `claim_unambiguous_raw_exec_owner_for_turn` (`codex.rs:2946`), which claims
/// nothing when a turn holds more than one unclaimed candidate — so a turn with
/// two or more shell calls orphans every one of them.
const MULTI_FILES: [&str; 3] = ["multi_a.txt", "multi_b.txt", "multi_c.txt"];

/// A 20s command plus whatever polling interval the backend uses to notice it
/// finished.
const BG_SETTLE: Duration = Duration::from_secs(60);

/// Long enough for a replay that has already finished to flush whatever it
/// recorded, short enough that a clean resume does not pay for it.
const RESUME_SETTLE: Duration = Duration::from_secs(5);

#[test]
#[ignore = "paid real-backend suite; use --run-ignored all with TYDE_RUN_REAL_AI_TESTS=1"]
fn real_conversation() {
    run_scenario(&[], |mut host| async move {
        let payload = unique_payload();
        let agent = spawn_agent(&mut host, &launch_prompt()).await;

        // Asserted next to the turn that produced it rather than in a block at
        // the end, matching the newer scenarios: a failure then names the turn
        // that caused it instead of one four turns later.
        let launched = collect_turn(&mut host, &agent, &launch_prompt()).await;
        assert_final_text_contains(&launched, READY_MARKER);

        let wrote = ask(&mut host, &agent, write_prompt(&payload)).await;
        assert_wrote_file(&wrote, host.workspace(), &payload);
        assert_final_text_contains(&wrote, WROTE_MARKER);

        let read_back = ask(&mut host, &agent, read_prompt()).await;
        assert_read_back_payload(&read_back, &payload);

        let multi = ask(&mut host, &agent, multi_tool_prompt()).await;
        assert_multi_tool_turn(&multi, host.workspace());

        let deleted = ask(&mut host, &agent, delete_prompt()).await;
        assert_deleted_directory(&deleted, host.workspace());

        assert_universal_contract(&[launched, wrote, read_back, multi, deleted]);

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

/// What the tool cards are made of, not just that they exist.
///
/// Every other scenario in this suite counts tool requests and consults the
/// filesystem, so all of them pass unchanged if a backend maps every provider
/// tool to `ToolRequestType::Other { args }`. `Other` is a real card — it just
/// renders as a JSON blob with no diff, no file name, no exit code and no
/// output. The whole point of Tyde's normalized tool types is that a write
/// becomes a diff and a command becomes a terminal, and nothing paid checks that
/// the mapping happens. Five backends build these types independently, each from
/// its own provider's argument shapes.
///
/// The prompts here name the tool *category*, unlike the rest of the suite. That
/// is deliberate and it is what makes the assertions mean anything: a model that
/// satisfies "create a file" by shelling out to `printf >` produces a perfectly
/// correct `RunCommand` and says nothing at all about the diff card.
#[test]
#[ignore = "paid real-backend suite; use --run-ignored all with TYDE_RUN_REAL_AI_TESTS=1"]
fn real_tool_type_mappings() {
    run_scenario(&[], |mut host| async move {
        let created = unique_payload();
        let edited = unique_payload();
        let token = unique_payload();
        let diffs = host.declares(BackendCapability::GenericModifyFile);
        let reads = host.declares(BackendCapability::GenericReadFiles);

        let agent = spawn_agent(&mut host, &launch_prompt()).await;
        let launched = collect_turn(&mut host, &agent, &launch_prompt()).await;
        assert_final_text_contains(&launched, READY_MARKER);

        if !diffs {
            eprintln!(
                "COVERAGE: {:?} does not declare GenericModifyFile, so this run asserts nothing \
                 about diff cards",
                host.backend()
            );
        }

        // The file the later turns operate on has to exist however this backend
        // makes files, so the write turns run everywhere; only the diff-card
        // assertions are gated. Each is checked before the next turn runs — both
        // read the file to decide whether the work happened, and the edit turn
        // overwrites what the create turn is judged against.
        let create = ask(&mut host, &agent, mapping_create_prompt(&created)).await;
        assert_final_text_contains(&create, MAPPED_CREATE_MARKER);
        if diffs {
            assert_create_maps_to_a_diff(&create, host.workspace(), &created);
        }

        let edit = ask(&mut host, &agent, mapping_edit_prompt(&created, &edited)).await;
        assert_final_text_contains(&edit, MAPPED_EDIT_MARKER);
        if diffs {
            assert_edit_maps_to_a_non_empty_diff(&edit, host.workspace(), &created, &edited);
        }

        let mut turns = vec![launched, create, edit];

        if reads {
            let unseen = unique_payload();
            std::fs::write(
                host.workspace().join(MAPPING_FILE),
                format!("alpha\n{unseen}\nomega\n"),
            )
            .expect("rewrite mapping.txt out of band");
            let read = ask(&mut host, &agent, mapping_read_prompt()).await;
            assert_read_maps_to_read_files(&read, host.workspace(), &unseen);
            turns.push(read);
        } else {
            eprintln!(
                "COVERAGE: {:?} does not declare GenericReadFiles, so this run asserts nothing \
                 about read cards",
                host.backend()
            );
        }

        let ran = ask(&mut host, &agent, mapping_command_prompt(&token)).await;
        assert_command_maps_to_run_command(&ran, host.workspace(), &token);
        assert_final_text_contains(&ran, MAPPED_RUN_MARKER);
        turns.push(ran);

        let deleted = ask(&mut host, &agent, mapping_delete_prompt()).await;
        assert_delete_is_not_an_opaque_card(&deleted, host.workspace());
        assert_final_text_contains(&deleted, MAPPED_DELETE_MARKER);
        turns.push(deleted);

        assert_universal_contract(&turns);
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

        // Rewritten out of band, because the payload the conversation asked for
        // is *in* that conversation: replayed history is part of the resumed
        // model's context, so "read the file and reply with its contents" was
        // answerable from memory. Measured — minimax answered
        // `TYDE_PAYLOAD_14DBFFFAFA4F` in 1.4s with no tool call at all, and the
        // check below then read as a dropped card. This payload has never
        // appeared in the conversation, so reporting it requires actually
        // reading the file, and the tool-card assertion becomes a real test of
        // whether cards survive a resume instead of a bet on how eagerly a
        // given model reaches for tools.
        let after_resume_payload = unique_payload();
        std::fs::write(
            host.workspace().join(HELLO_FILE),
            format!("{after_resume_payload}\n"),
        )
        .expect("rewrite hello.txt out of band");

        // Resumed sessions rendering blank is one bug; resumed sessions
        // silently losing every subsequent tool card is a worse one.
        let follow_up = ask(&mut host, &resumed, reread_prompt()).await;
        assert_read_back_payload(&follow_up, &after_resume_payload);
        assert!(
            follow_up.tool_requests().next().is_some(),
            "{}: a new turn on a resumed session reported the rewritten contents of \
                 {HELLO_FILE} but emitted zero tool requests, so the read that produced them \
                 never reached the client as a card",
            follow_up.label()
        );
        assert_universal_contract(&[follow_up]);

        assert_replay_has_no_duplicates(
            &resumed,
            host.backend(),
            &[launch_prompt(), write_prompt(&payload)],
        );

        assert_clean_close(&mut host, &resumed).await;

        // Last, because if this guarantee ever breaks the agent's working
        // directory moves for good, and anything after it would be asserting
        // against a directory the session has left.
        assert_a_session_cannot_move_out_from_under_tyde(&mut host, &session.id).await;
    });
}

/// A session's provider file must stay where Tyde derives it.
///
/// Claude names its session directory after the cwd it is running in.
/// `EnterWorktree` switches the *session's* working directory, and the CLI moves
/// the session file into the new directory's project folder — recording it only
/// in the file itself:
///
/// ```text
/// {"type":"relocated","sessionId":"…","relocatedCwd":"…/.claude/worktrees/…"}
/// ```
///
/// Nothing announces it on the stream (measured: no such event exists, and only
/// the `system/init` frame carries a cwd), so Tyde has no way to learn the
/// session moved. Its derived path is then permanently wrong. On 2026-08-19 a
/// resume a day later looked in the old directory, found nothing, and started a
/// *new* CLI session at the same id — reporting success over an empty context
/// while the real 29,502-line conversation sat intact in the worktree's
/// directory.
///
/// The codeword is the only oracle that can see this. Bootstrap replay comes
/// from Tyde's own transcript, so the reopened agent looks fully populated
/// either way; nothing in the event stream distinguishes the two.
async fn assert_a_session_cannot_move_out_from_under_tyde(host: &mut Host, session_id: &SessionId) {
    let backend = host.backend();
    if backend != BackendKind::Claude {
        eprintln!(
            "COVERAGE: {backend:?} has no known session-relocating tool; the worktree shape \
             asserts nothing for it"
        );
        return;
    }

    let worktree = add_worktree(host, "relocated");
    let derived = claude_session_file(host.workspace(), session_id);
    let relocated = claude_session_file(&worktree, session_id);
    assert!(
        derived.exists(),
        "{backend:?}: expected the live session at {} before asking for a worktree",
        derived.display()
    );

    let reopened = resume_agent(host, session_id).await;
    let secret = unique_payload();
    let memorized = ask(host, &reopened, remember_prompt(&secret)).await;
    assert_final_text_contains(&memorized, MEMORIZED_MARKER);

    let attempt = ask(host, &reopened, enter_worktree_prompt(&worktree)).await;

    // The model reaches this tool through `ToolSearch`, so the search is
    // evidence it tried whether or not the tool was available to it. Without
    // this, a model that ignored the prompt would sail through every assertion
    // below having exercised nothing.
    assert!(
        attempt
            .assistant_messages()
            .flat_map(|message| message.tool_calls.iter())
            .any(|call| call.name == "EnterWorktree"
                || (call.name == "ToolSearch"
                    && call.arguments.to_string().contains("EnterWorktree"))),
        "{}: never reached for EnterWorktree at all, so nothing below is exercising the \
         guarantee",
        attempt.label()
    );

    assert!(
        derived.exists() && !relocated.exists(),
        "{backend:?}: the session left the directory Tyde derives for it — {} (exists={}) vs {} \
         (exists={}). Tyde cannot see this move, so every later resume looks in the wrong place.",
        derived.display(),
        derived.exists(),
        relocated.display(),
        relocated.exists()
    );

    assert_clean_close(host, &reopened).await;

    // The move is only half the defect; this is the half the user feels.
    let after = resume_agent(host, session_id).await;
    let recalled = ask(host, &after, recall_prompt()).await;
    assert!(
        recalled.final_text().contains(&secret),
        "{backend:?}: resuming after the worktree attempt came back without the conversation — \
         the model answered {:?} instead of the codeword {secret}.",
        recalled.final_text()
    );
    assert_clean_close(host, &after).await;
}

/// A resumed session must group a response's tool calls exactly like a fresh one.
///
/// Both halves run the same prompt and the same oracle on purpose. The fresh
/// half is the control: it establishes that this backend and this model do
/// group parallel calls, so a failure in the resumed half is the resume path
/// and not the model declining to parallelize. Without that control the
/// resumed assertion cannot distinguish the two.
///
/// Codex fails the resumed half today: its app-server sends no `rawResponse*`
/// on a resumed thread (openai/codex#34353), so the provider-response splitter
/// is disabled and every tool falls back to a path that emits one message per
/// call.
#[test]
#[ignore = "paid real-backend suite; use --run-ignored all with TYDE_RUN_REAL_AI_TESTS=1"]
fn real_resumed_session_groups_parallel_tool_calls() {
    run_scenario(&[BackendCapability::ResumeSession], |mut host| async move {
        let agent = spawn_agent(&mut host, &launch_prompt()).await;
        let launched = collect_turn(&mut host, &agent, &launch_prompt()).await;

        let fresh = ask(&mut host, &agent, parallel_tool_prompt()).await;
        assert_multi_tool_turn(&fresh, host.workspace());
        assert_response_groups_its_tool_calls(&fresh);

        assert_universal_contract(&[launched, fresh]);

        let session = stored_session(&mut host).await;
        assert!(
            session.resumable,
            "{:?}: session is not resumable, so the rest of this scenario cannot run",
            host.backend()
        );
        assert_clean_close(&mut host, &agent).await;

        let resumed = resume_agent(&mut host, &session.id).await;
        assert_replayed_history_is_not_empty(&resumed, host.backend());

        let after_resume = ask(&mut host, &resumed, parallel_tool_prompt()).await;
        assert_multi_tool_turn(&after_resume, host.workspace());
        assert_response_groups_its_tool_calls(&after_resume);
        assert_universal_contract(&[after_resume]);

        assert_clean_close(&mut host, &resumed).await;
    });
}

/// Compaction, and what a session looks like once it has been compacted.
///
/// Compaction is not just another turn: it rewrites the provider's own session
/// file, which is the file a resume replays. Both halves of this scenario exist
/// because that rewrite is unobservable from any single turn.
#[test]
#[ignore = "paid real-backend suite; use --run-ignored all with TYDE_RUN_REAL_AI_TESTS=1"]
fn real_compaction_and_resume() {
    run_scenario(
        &[
            BackendCapability::CompactionReported,
            BackendCapability::ResumeSession,
        ],
        |mut host| async move {
            let payload = unique_payload();
            let agent = spawn_agent(&mut host, &launch_prompt()).await;
            let launched = collect_turn(&mut host, &agent, &launch_prompt()).await;

            // Tool calls before each compaction, because both defects this
            // scenario covers are carried by tool declarations: a conversation
            // of plain text compacts and resumes cleanly while still being
            // wrong.
            let wrote = ask(&mut host, &agent, write_prompt(&payload)).await;
            assert_wrote_file(&wrote, host.workspace(), &payload);
            let from_idle = compact(&mut host, &agent).await;

            // The second one is requested *mid-turn* on purpose. Compacting an
            // idle agent dispatches immediately; compacting a busy one parks the
            // request until the turn ends and then dispatches it into a loop
            // that is already draining that turn's events. Only the second shape
            // puts the operation's terminal result and the backend's own
            // observation of the compaction in a position to arrive out of
            // order, and correlating them is what keeps it to one row.
            send_prompt(&mut host, &agent, &multi_tool_prompt()).await;
            let mid_turn = compact(&mut host, &agent).await;

            assert_compaction_left_one_marker(&from_idle);
            assert_compaction_left_one_marker(&mid_turn);
            assert_multi_tool_files_were_written(&mid_turn, host.workspace());
            assert_universal_contract(&[launched, wrote]);

            let session = stored_session(&mut host).await;
            assert!(
                session.resumable,
                "{:?}: session is not resumable after compaction, so the rest of this scenario \
                 cannot run",
                host.backend()
            );
            assert_clean_close(&mut host, &agent).await;

            let resumed = resume_agent(&mut host, &session.id).await;
            assert_replayed_history_is_not_empty(&resumed, host.backend());
            assert_replay_has_no_duplicates(
                &resumed,
                host.backend(),
                &[launch_prompt(), write_prompt(&payload), multi_tool_prompt()],
            );

            // `TurnEmitter` batches the protocol violations it caught into one
            // Error card and flushes it when the turn that recorded them goes
            // idle. Violations recorded while replaying a resumed session belong
            // to no prompt, so the card can land in the bootstrap, in the quiet
            // window after it, or in the next turn — all three are checked.
            let bootstrap_label = format!("{:?} resume replay", host.backend());
            assert_no_error_message(&bootstrap_label, &resumed.replayed_history);
            let settled = drain_events_for(&mut host, RESUME_SETTLE).await;
            assert_no_error_message(&bootstrap_label, &settled);

            // A compacted session that resumes into a broken turn is the same
            // failure as one that resumes blank, one step later.
            let follow_up = ask(&mut host, &resumed, read_prompt()).await;
            assert_read_back_payload(&follow_up, &payload);
            assert_universal_contract(&[follow_up]);

            assert_clean_close(&mut host, &resumed).await;
        },
    );
}

/// Everything about asking the user a question, in one conversation.
///
/// `backend.rs` spends roughly twenty separate paid conversations on this tool
/// — shape, waiting, answering, interrupting, closing, reconnecting, forking —
/// and one of them (`assert_user_question_waits_for_answer`, `backend.rs:14550`)
/// already asserts the invariant that broke in production. It never caught it,
/// because a suite that costs twenty conversations to check one tool does not
/// get run. The same guarantees fit in one conversation.
///
/// A question is the only tool that is *supposed* to outlive its turn: the turn
/// goes idle and the card stays open, because the thing it is waiting for is a
/// human. Everything here follows from that.
#[test]
#[ignore = "paid real-backend suite; use --run-ignored all with TYDE_RUN_REAL_AI_TESTS=1"]
fn real_user_question() {
    run_scenario(
        &[BackendCapability::UserQuestionRequests],
        |mut host| async move {
            let agent = spawn_agent(&mut host, &launch_prompt()).await;
            let launched = collect_turn(&mut host, &agent, &launch_prompt()).await;
            assert_final_text_contains(&launched, READY_MARKER);

            let asked = ask_question(&mut host, &agent, &question_prompt()).await;
            assert_question_shape(&asked);
            assert_question_waits_for_an_answer(&asked);

            // Answering with a label the provider actually offered, so this
            // tests the tool rather than the prompt.
            let choice = asked
                .first_option()
                .expect("question shape assertion guarantees an option")
                .to_owned();
            let answered = answer_question(&mut host, &agent, &asked, &choice).await;
            assert_question_answer_reached_the_model(&asked, &answered, &choice);

            // Second question, abandoned rather than answered. Cancelling is
            // the user's escape hatch from an interactive tool, and it is the
            // one path where a card and a turn can be terminalized out of step.
            let abandoned = ask_question(&mut host, &agent, &question_prompt()).await;
            assert_question_shape(&abandoned);
            let cancelled = cancel_turn(&mut host, &agent).await;
            assert_no_error_message(&format!("{:?} question cancel", host.backend()), &cancelled);

            // The assertion the wedge costs: a cancelled question must leave an
            // agent that still works. A latched turn queues every later message
            // instead of running it, and no further cancel can clear it.
            let recovered = ask_expecting_delivery(&mut host, &agent, &launch_prompt()).await;
            assert_final_text_contains(&recovered, READY_MARKER);

            assert_universal_contract(&[launched, recovered]);

            assert_clean_close(&mut host, &agent).await;
        },
    );
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

/// Everything about a native workflow, in one conversation.
///
/// A workflow is the second thing in Tyde that is *supposed* to outlive its own
/// turn, and it is the harder of the two. A question outlives its turn with
/// somebody waiting on it; a workflow outlives its turn with nobody waiting at
/// all. The provider's tool returns a task id straight away, the turn that
/// launched it goes idle seconds later, and the run then reports progress —
/// and its terminal state — into a conversation that has moved on.
///
/// Everything asserted here is about that gap. The run itself is the provider's
/// own subprocess and works regardless; what breaks is Tyde's account of it.
#[test]
#[ignore = "paid real-backend suite; use --run-ignored all with TYDE_RUN_REAL_AI_TESTS=1"]
fn real_native_workflow() {
    run_scenario(
        &[BackendCapability::WorkflowProgress],
        |mut host| async move {
            let agent = spawn_agent(&mut host, &launch_prompt()).await;
            let launched = collect_turn(&mut host, &agent, &launch_prompt()).await;
            assert_final_text_contains(&launched, READY_MARKER);

            let prompt = workflow_prompt(host.backend());
            let workflow = run_workflow(&mut host, &agent, &prompt).await;

            // The filesystem first: it separates "the run never happened" from
            // "the run happened and Tyde lost the report", and every assertion
            // after this one is about the second.
            assert_workflow_agents_did_their_work(&workflow, host.workspace());
            assert_no_error_message(&workflow.label(), workflow.events());
            assert_workflow_reported_its_agents(&workflow);
            assert_workflow_reached_terminal(&workflow);
            assert_workflow_outlived_its_tool_call(&workflow);

            assert_universal_contract(&[launched]);
            assert_universal_contract(std::slice::from_ref(workflow.turn()));

            assert_clean_close(&mut host, &agent).await;
        },
    );
}

fn launch_prompt() -> String {
    format!("Reply with exactly {READY_MARKER} and nothing else. Do not use any tools.")
}

/// Two agents, each writing its own file.
///
/// Two rather than one because a workflow snapshot's per-agent state is folded
/// from a stream of index-addressed deltas (`apply_workflow_agent_delta`,
/// `claude.rs:7310`), and a single-agent run cannot tell a working fold from one
/// that collapses every delta into slot zero.
const WORKFLOW_FILES: [&str; 2] = ["workflow_a.txt", "workflow_b.txt"];

/// Given verbatim rather than described.
///
/// Claude's Workflow tool takes a JavaScript script with a `meta` literal, and a
/// small pinned model asked to author one writes a script that fails to compile
/// often enough to matter — which surfaces as a run that reports nothing, the
/// exact shape of the defect this scenario hunts. Handing over the script keeps
/// the subject Tyde's handling of a workflow rather than the model's ability to
/// write one.
fn workflow_script() -> String {
    let [a, b] = WORKFLOW_FILES;
    format!(
        "export const meta = {{ name: 'tyde_conformance', description: 'conformance probe', \
         phases: [{{ title: 'Probe' }}] }}\n\
         phase('Probe')\n\
         await parallel([\n\
         () => agent('Create a file named {a} in the workspace root whose contents are exactly \
         A, then reply DONE.'),\n\
         () => agent('Create a file named {b} in the workspace root whose contents are exactly \
         B, then reply DONE.'),\n\
         ])\n\
         return 'ok'"
    )
}

fn workflow_prompt(backend_kind: BackendKind) -> String {
    let launch = match backend_kind {
        BackendKind::Claude => format!(
            "Call the Workflow tool exactly once, passing this script verbatim as its `script` \
             parameter and changing nothing in it:\n\n{}\n",
            workflow_script()
        ),
        _ => format!(
            "Use your native workflow tool exactly once to run two agents in parallel: one \
             creating a file named {} in the workspace root whose contents are exactly A, the \
             other creating {} whose contents are exactly B.",
            WORKFLOW_FILES[0], WORKFLOW_FILES[1]
        ),
    };
    format!(
        "{launch} As soon as the tool returns, reply with exactly {WORKFLOW_MARKER} and nothing \
         else. Do not wait for the workflow to finish and do not do the work yourself."
    )
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

/// The read a resumed session is asked for, after the file has been rewritten
/// out of band.
///
/// Still names a goal rather than a tool, but it has to say the file changed:
/// the conversation itself dictated the old contents, so a resumed model holding
/// that history can answer `read_prompt` from memory — measured, minimax replied
/// with the superseded payload and ran nothing. Saying so is what makes the
/// turn's tool cards the thing under test rather than how eagerly a given model
/// reaches for a tool.
fn reread_prompt() -> String {
    format!(
        "The contents of {HELLO_FILE} in the workspace root changed on disk after your last \
         message. Read it again now and reply with exactly its current contents and nothing \
         else. Do not answer from earlier in this conversation."
    )
}

/// Three lines, because the edit that follows has to change one of them and
/// leave the others as diff context. A one-line file cannot tell a targeted edit
/// from a whole-file rewrite.
fn mapping_create_prompt(payload: &str) -> String {
    format!(
        "Use your file-editing tool — not the shell — to create {MAPPING_FILE} in the workspace \
         root with exactly these three lines:\nalpha\n{payload}\nomega\nThen reply with exactly \
         {MAPPED_CREATE_MARKER} and nothing else."
    )
}

/// One line of three, changed in place.
///
/// A whole-file rewrite is a legitimate way to satisfy this and still produces a
/// truthful card, so the assertion accepts either. What it does not accept is a
/// card whose `before` and `after` are equal — the UI computes its diff from
/// exactly those two strings (`modify_file.rs:92`), so a card that carries the
/// same text twice renders an edit with no lines in it.
fn mapping_edit_prompt(old: &str, new: &str) -> String {
    format!(
        "Use your file-editing tool — not the shell — to change the middle line of \
         {MAPPING_FILE} in the workspace root from {old} to {new}. Leave the alpha and omega \
         lines exactly as they are. Then reply with exactly {MAPPED_EDIT_MARKER} and nothing else."
    )
}

/// Has to say the file changed, and the caller has to actually change it.
///
/// The edit turn dictated the middle line in its own prompt, so a model holding
/// that history can answer this without opening anything — measured, tycode
/// replied with the correct payload in one turn having emitted zero tool
/// requests, and the ReadFiles assertion then read as a dropped card. Reading a
/// line the conversation has never mentioned is the only version of this turn
/// that tests the mapping rather than the model's appetite for tools.
fn mapping_read_prompt() -> String {
    format!(
        "The contents of {MAPPING_FILE} in the workspace root changed on disk after your last \
         message. Use your file-reading tool — not the shell — to read it again now, then reply \
         with exactly its middle line and nothing else. Do not answer from earlier in this \
         conversation."
    )
}

/// Echoes a token the conversation has never seen, so the completion's `stdout`
/// has to come from a real process rather than from the request being replayed
/// back as its own result.
fn mapping_command_prompt(token: &str) -> String {
    format!(
        "Run this exact shell command in the workspace root: echo {token}\nThen reply with \
         exactly {MAPPED_RUN_MARKER} and nothing else."
    )
}

/// Goal-only, because Tyde has no `DeleteFile` request type: a delete arrives as
/// whatever the provider reached for, and this turn exists to check that it is
/// still a card a human can read. `delete_prompt` names a recursive shell
/// command instead, which pins the answer to `RunCommand` and would tell this
/// turn nothing.
///
/// Authorizing the delete in the prompt is what keeps the choice open. Kiro
/// refuses a bare "delete this file" outright — measured, it replied "this is a
/// destructive operation… I require explicit user confirmation" and ran nothing
/// — so without this the turn stalls on a safety gate rather than reporting a
/// mapping. That gate is real behaviour and is worth testing, but
/// `assert_deleted_directory` in `real_conversation` already covers it.
fn mapping_delete_prompt() -> String {
    format!(
        "Delete the file {MAPPING_FILE} from the workspace root. I am explicitly authorizing this \
         deletion now, so do not ask me to confirm it — go ahead and delete it. Then reply with \
         exactly {MAPPED_DELETE_MARKER} and nothing else."
    )
}

/// Deliberately un-writable. A codeword held only in the conversation is the one
/// thing a model that has lost its context cannot answer; every file-backed
/// oracle in this suite it still answers correctly.
fn remember_prompt(secret: &str) -> String {
    format!(
        "Remember this codeword for the rest of our conversation: {secret}. Do not write it to a \
         file and do not use any tools. Reply with exactly {MEMORIZED_MARKER} and nothing else."
    )
}

fn recall_prompt() -> String {
    "Reply with exactly the codeword I asked you to remember earlier, and nothing else. Do not \
     use any tools and do not read any files."
        .to_owned()
}

/// Names the tool outright rather than describing a goal, unlike the rest of the
/// suite: the point of this turn is the CLI-side session move that this specific
/// tool performs, not whatever a model might choose to reach a worktree with.
fn enter_worktree_prompt(worktree: &Path) -> String {
    format!(
        "Work in a worktree from now on: enter the existing git worktree at {} with the \
         EnterWorktree tool. Then tell me whether you succeeded.",
        worktree.display()
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

/// Destructive, through a shell command, and *recursive* specifically.
///
/// Recursion is what the gates key on. Hermes ships 47 approval patterns and 12
/// hard blocks (`~/.hermes/hermes-agent/tools/approval.py`); `rm <file>` matches
/// none of them, while `rm -r` matches "recursive delete". An earlier version of
/// this prompt deleted a single file, and every backend sailed through it —
/// a green result that asserted nothing about the gate.
///
/// Tyde cannot answer a prompt a provider raises inside its own runtime, so a
/// backend that gates here does not fail loudly: it goes quiet with the work
/// undone, which is why the oracle below is the filesystem and not the reply.
///
/// The target is a directory this scenario seeded inside its own temporary
/// workspace, which is the whole of what the agent can reach.
fn delete_prompt() -> String {
    format!(
        "Delete the directory {SCRATCH_DIR} and everything in it from the workspace root, by \
         running a single recursive shell command. Then reply with exactly {DELETED_MARKER} and \
         nothing else."
    )
}

/// Names a decision the model cannot make for itself, so asking is the only way
/// forward. A prompt the model can resolve on its own produces a clean, fast,
/// meaningless pass.
fn question_prompt() -> String {
    "I want you to name a file, but only I know which name is right. Ask me to choose between \
     exactly two options, ALPHA and BETA, using your question tool. Ask, and then stop and wait \
     for my answer — do not guess, do not pick one yourself, and do not create any file yet."
        .to_owned()
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

/// Forces several tool calls out of a *single* provider response.
///
/// `multi_tool_prompt` deliberately leaves the model free to work one file at a
/// time, which is a different shape: three responses of one call each is
/// correct there. Here the calls must share one response, because that is the
/// only way a client can observe whether a response's calls stay together.
fn parallel_tool_prompt() -> String {
    let [a, b, c] = MULTI_FILES;
    format!(
        "Issue all three of these tool calls at once, in a single response, in parallel: create \
         {a} containing exactly A, create {b} containing exactly B, and create {c} containing \
         exactly C. Do not wait for one result before issuing the next, and do not combine them \
         into a single command. Then reply with exactly {MULTI_MARKER} and nothing else."
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
/// One provider response's tool calls must arrive as one chat message.
///
/// A chat message is meant to be exactly one provider response, and its
/// `tool_calls` list is the client's only handle on which response issued a
/// card. A backend that mints a fresh message per tool still renders every
/// card, so no other oracle in this suite notices: the turn just silently
/// becomes N single-tool bubbles with the response's own text stranded in a
/// message of its own.
///
/// Deliberately not "one message for the whole turn" — a turn legitimately
/// holds several responses. The claim is only that a response does not get
/// shredded, so it fails when *every* call sits alone and tolerates a model
/// that splits three calls as 2+1.
fn assert_response_groups_its_tool_calls(turn: &Turn) {
    let declarations: Vec<(String, Option<String>)> = turn
        .tool_requests()
        .map(|request| {
            let owner = turn
                .assistant_messages()
                .find(|message| {
                    message
                        .tool_calls
                        .iter()
                        .any(|call| call.tool_call_id == request.tool_call_id)
                })
                .and_then(|message| message.message_id.as_ref())
                .map(|id| id.0.clone());
            (request.tool_call_id.clone(), owner)
        })
        .collect();

    let orphans: Vec<&str> = declarations
        .iter()
        .filter(|(_, owner)| owner.is_none())
        .map(|(id, _)| id.as_str())
        .collect();
    assert!(
        orphans.is_empty(),
        "{}: tool calls {orphans:?} were never declared by any assistant message, so the client \
         cannot tell which response issued them. Saw messages {:?}",
        turn.label(),
        turn.assistant_messages()
            .map(|message| message.tool_calls.len())
            .collect::<Vec<_>>()
    );

    if declarations.len() < 2 {
        return;
    }
    let owners: BTreeSet<&str> = declarations
        .iter()
        .filter_map(|(_, owner)| owner.as_deref())
        .collect();
    assert!(
        owners.len() < declarations.len(),
        "{}: {} tool calls arrived as {} separate chat messages — every call got its own message, \
         so no provider response kept its calls together. Tools: {:?}",
        turn.label(),
        declarations.len(),
        owners.len(),
        turn.tool_request_names(),
    );
}

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

/// Whether a path a card reports names the file the card is about.
///
/// Backends report the path however their provider gave it — absolute for some,
/// workspace-relative for others — and both are correct. The card's path is what
/// the header shows and what opening the file uses, so what has to hold is that
/// it resolves to the real file. Canonicalized on both sides because a macOS
/// tempdir is reached through a symlink and a sandboxed provider reports the
/// resolved form.
fn resolves_to(reported: &str, workspace: &Path, file: &str) -> bool {
    let reported = Path::new(reported);
    let resolved = if reported.is_absolute() {
        reported.to_path_buf()
    } else {
        workspace.join(reported)
    };
    match (resolved.canonicalize(), workspace.join(file).canonicalize()) {
        (Ok(reported), Ok(expected)) => reported == expected,
        _ => false,
    }
}

/// Every `ModifyFile` request in the turn that names `MAPPING_FILE`, with the
/// request id so its completion can be found.
fn diff_cards<'a>(
    turn: &'a Turn,
    workspace: &Path,
) -> Vec<(&'a str, &'a String, &'a String, &'a String)> {
    turn.tool_requests()
        .filter_map(|request| match &request.tool_type {
            ToolRequestType::ModifyFile {
                file_path,
                before,
                after,
            } => Some((request.tool_call_id.as_str(), file_path, before, after)),
            _ => None,
        })
        .filter(|(_, file_path, _, _)| resolves_to(file_path, workspace, MAPPING_FILE))
        .collect()
}

/// The result the client received for one request, by id.
fn result_for<'a>(turn: &'a Turn, tool_call_id: &str) -> Option<&'a ToolExecutionResult> {
    turn.tool_completions()
        .find(|completion| completion.tool_call_id == tool_call_id)
        .and_then(|completion| match &completion.outcome {
            ToolExecutionOutcome::Succeeded { result } => Some(result),
            _ => None,
        })
}

/// Creating a file produces a diff card whose `after` is the new file.
fn assert_create_maps_to_a_diff(turn: &Turn, workspace: &Path, payload: &str) {
    let path = workspace.join(MAPPING_FILE);
    let contents = std::fs::read_to_string(&path).ok();
    assert!(
        contents
            .as_deref()
            .is_some_and(|contents| contents.contains(payload)),
        "{}: {} does not contain {payload:?} (contents: {contents:?}), so nothing below this line \
         is about how a write is mapped. The turn emitted {:?} and replied {:?}.",
        turn.label(),
        path.display(),
        turn.tool_request_names(),
        turn.final_text()
    );

    let cards = diff_cards(turn, workspace);
    let [(tool_call_id, _, before, after)] = cards.as_slice() else {
        panic!(
            "{}: writing {MAPPING_FILE} produced {} ModifyFile card(s) naming it, expected exactly \
             one. The file was written, so a tool ran — every other mapping renders the write as \
             something that is not a diff. Requests seen: {:?}",
            turn.label(),
            cards.len(),
            turn.tool_request_names()
        );
    };

    assert!(
        after.contains(payload),
        "{}: the diff card for {MAPPING_FILE} does not show the content that was written. \
         `after` is {after:?} and the file now holds {payload:?}. The card is what the user reads \
         instead of the file.",
        turn.label()
    );
    assert!(
        before.is_empty(),
        "{}: the diff card for a file that did not exist reports `before` as {before:?}. The UI \
         diffs `before` against `after` verbatim, so a created file renders as a modification of \
         text that was never there.",
        turn.label()
    );

    let result = result_for(turn, tool_call_id);
    assert!(
        matches!(result, Some(ToolExecutionResult::ModifyFile { lines_added, .. }) if *lines_added > 0),
        "{}: the completed write reported {result:?}. The card's footer shows `+A -B` from \
         ModifyFile's line counts; anything else leaves a finished edit with no summary of what \
         it did.",
        turn.label()
    );
}

/// Editing a file produces a diff card with something in it.
///
/// The one shape that is always wrong is `before == after`: the UI builds its
/// diff from those two strings with `TextDiff::from_lines`, so equal strings
/// render a card that claims an edit and shows zero lines. Whether they hold the
/// whole file or just the replaced hunk is the provider's choice and both render
/// correctly, so this asserts on the change being visible rather than on which
/// of the two conventions the backend follows.
fn assert_edit_maps_to_a_non_empty_diff(turn: &Turn, workspace: &Path, old: &str, new: &str) {
    let path = workspace.join(MAPPING_FILE);
    let contents = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        contents.contains(new) && !contents.contains(old),
        "{}: {} still reads {contents:?}, so the edit never happened and nothing below this line \
         is about how an edit is mapped. The turn emitted {:?} and replied {:?}.",
        turn.label(),
        path.display(),
        turn.tool_request_names(),
        turn.final_text()
    );

    let cards = diff_cards(turn, workspace);
    assert!(
        !cards.is_empty(),
        "{}: {MAPPING_FILE} was edited on disk but the turn emitted no ModifyFile card naming it. \
         Requests seen: {:?}",
        turn.label(),
        turn.tool_request_names()
    );

    for (tool_call_id, _, before, after) in &cards {
        assert!(
            before != after,
            "{}: the edit card for {MAPPING_FILE} carries the same text as `before` and `after` \
             ({before:?}). The UI diffs them verbatim, so this renders as an edit with no lines \
             in it.",
            turn.label()
        );
        assert!(
            before.contains(old),
            "{}: the edit card's `before` is {before:?}, which does not contain the text that was \
             replaced ({old:?}). The removed side of the diff is not what the file actually held.",
            turn.label()
        );
        assert!(
            after.contains(new),
            "{}: the edit card's `after` is {after:?}, which does not contain the text that was \
             written ({new:?}). The added side of the diff is not what the file now holds.",
            turn.label()
        );

        let result = result_for(turn, tool_call_id);
        assert!(
            matches!(
                result,
                Some(ToolExecutionResult::ModifyFile { lines_added, lines_removed })
                    if *lines_added > 0 && *lines_removed > 0
            ),
            "{}: the completed edit reported {result:?}. A replaced line is one added and one \
             removed; the card's `+A -B` footer comes from these counts.",
            turn.label()
        );
    }
}

/// A read produces a `ReadFiles` card naming the file, and a result listing it.
///
/// `payload` is on disk and nowhere in the conversation, so quoting it back is
/// proof the file was opened. Checked before the card, because the two failures
/// are different problems wearing the same symptom: a model that answered from
/// history emits no card and neither does a backend that dropped one, and only
/// the reply tells them apart. Measured — tycode answers this turn with the
/// payload its own earlier prompt dictated, having run nothing.
fn assert_read_maps_to_read_files(turn: &Turn, workspace: &Path, payload: &str) {
    let answer = turn.final_text();
    assert!(
        answer.contains(payload),
        "{}: the reply is {answer:?}, which does not contain {payload:?} — the payload written to \
         {MAPPING_FILE} out of band and never mentioned in this conversation. The model answered \
         from history instead of opening the file, so this turn ran no read to map. It emitted \
         {:?}.",
        turn.label(),
        turn.tool_request_names()
    );

    let reads: Vec<_> = turn
        .tool_requests()
        .filter_map(|request| match &request.tool_type {
            ToolRequestType::ReadFiles { file_paths } => {
                Some((request.tool_call_id.as_str(), file_paths))
            }
            _ => None,
        })
        .collect();

    let named = reads.iter().find(|(_, file_paths)| {
        file_paths
            .iter()
            .any(|path| resolves_to(path, workspace, MAPPING_FILE))
    });
    let Some((tool_call_id, _)) = named else {
        panic!(
            "{}: reading {MAPPING_FILE} emitted no ReadFiles card naming it. The card lists the \
             files it opened; without the mapping the user sees a JSON blob. Requests seen: {:?}, \
             ReadFiles paths seen: {:?}",
            turn.label(),
            turn.tool_request_names(),
            reads.iter().map(|(_, paths)| paths).collect::<Vec<_>>()
        );
    };

    let result = result_for(turn, tool_call_id);
    assert!(
        matches!(result, Some(ToolExecutionResult::ReadFiles { files }) if !files.is_empty()),
        "{}: the completed read reported {result:?}. The card lists each file and its size from \
         this result.",
        turn.label()
    );
}

/// A shell command produces a terminal card: the command, where it ran, and what
/// it printed.
fn assert_command_maps_to_run_command(turn: &Turn, workspace: &Path, token: &str) {
    let commands: Vec<_> = turn
        .tool_requests()
        .filter_map(|request| match &request.tool_type {
            ToolRequestType::RunCommand {
                command,
                working_directory,
            } => Some((request.tool_call_id.as_str(), command, working_directory)),
            _ => None,
        })
        .collect();

    // Contained rather than equal, because the wrapper is part of the truth.
    // Codex runs commands through a login shell and reports what it really
    // executed — `/bin/zsh -lc 'echo TYDE_…'` — while Claude's Bash tool reports
    // the bare line it was handed. Both cards are honest about the process that
    // ran, and a card that hid the wrapper would be less so. The whole
    // `echo <token>` phrase still has to be there, so this rejects a card
    // carrying only the provider's raw arguments.
    let wanted = format!("echo {token}");
    let Some((tool_call_id, _, working_directory)) = commands
        .iter()
        .find(|(_, command, _)| command.contains(&wanted))
    else {
        panic!(
            "{}: running `{wanted}` emitted no RunCommand card carrying that command line. The \
             card shows the user the command that is about to run; any other mapping shows the \
             provider's raw arguments instead. Requests seen: {:?}, commands \
             seen: {:?}",
            turn.label(),
            turn.tool_request_names(),
            commands
                .iter()
                .map(|(_, command, _)| command)
                .collect::<Vec<_>>()
        );
    };

    // Asserted only when the card reports one. Claude's `Bash` tool takes no
    // working-directory argument, and `claude_tool_request_type` reads this
    // field straight out of the provider's arguments (`claude.rs:11304`), so it
    // is structurally always empty there — and the card renders that correctly,
    // hiding the row behind `cwd_present` (`run_command.rs:58`) rather than
    // showing a blank. Filling in the workspace root would be worse than empty:
    // that shell carries `cd` state between calls, so the root is a guess about
    // where the command ran, and a card that guesses is the thing this scenario
    // exists to catch. What has to hold is that a reported directory is true.
    if !working_directory.is_empty() {
        assert_eq!(
            Path::new(working_directory.as_str()).canonicalize().ok(),
            workspace.canonicalize().ok(),
            "{}: the terminal card says the command ran in {working_directory:?}, but the agent's \
             workspace is {}. The card's directory is how a user tells one agent's shell from \
             another's.",
            turn.label(),
            workspace.display()
        );
    }

    let result = result_for(turn, tool_call_id);
    assert!(
        matches!(
            result,
            Some(ToolExecutionResult::RunCommand { exit_code, stdout, .. })
                if *exit_code == 0 && stdout.contains(token)
        ),
        "{}: the completed command reported {result:?}. The card renders the exit code and the \
         captured output from this result, and the token proves the output came from the process \
         rather than from the request echoed back.",
        turn.label()
    );
}

/// A delete still reaches the user as a card they can read.
///
/// Tyde has no `DeleteFile` request type, so there is no single correct mapping
/// here — a provider that shells out gives `RunCommand`, one with a native file
/// tool gives `ModifyFile` emptying the file. Both are legible. `Other` is not:
/// it renders the provider's raw arguments and never names the file. This asserts
/// the floor rather than a specific type.
fn assert_delete_is_not_an_opaque_card(turn: &Turn, workspace: &Path) {
    let path = workspace.join(MAPPING_FILE);
    assert!(
        !path.exists(),
        "{}: {} still exists after a turn asked to delete it, so nothing below this line is about \
         how a delete is mapped. The turn emitted {:?} and replied {:?}.",
        turn.label(),
        path.display(),
        turn.tool_request_names(),
        turn.final_text()
    );

    let requests = turn.tool_requests().count();
    assert!(
        requests > 0,
        "{}: {} was deleted but the turn emitted zero tool requests",
        turn.label(),
        path.display()
    );

    let opaque = turn
        .tool_requests()
        .filter(|request| matches!(request.tool_type, ToolRequestType::Other { .. }))
        .count();
    assert_eq!(
        opaque,
        0,
        "{}: {opaque} of {requests} cards in the delete turn are ToolRequestType::Other, which \
         renders the provider's raw arguments with no file name and no diff. Requests seen: {:?}",
        turn.label(),
        turn.tool_request_names()
    );

    // A backend that renders the delete as a diff has to render a diff that
    // removes the file. `Other` is not the only way a delete card can say
    // nothing: Codex reports a delete as a `fileChange` carrying the removed
    // file's content, which read as a diff put every line on both sides and
    // produced a card claiming an edit with no lines in it. Matched on the file
    // name because the file is gone by now and no path resolves.
    for request in turn.tool_requests() {
        if let ToolRequestType::ModifyFile {
            file_path,
            before,
            after,
        } = &request.tool_type
            && Path::new(file_path)
                .file_name()
                .is_some_and(|name| name == MAPPING_FILE)
        {
            assert!(
                !before.is_empty() && after.is_empty(),
                "{}: the delete is rendered as a diff card with before {before:?} and after \
                 {after:?}. Removing a file is every line leaving it, so `before` is the file and \
                 `after` is empty; anything else renders a deletion the user cannot see.",
                turn.label()
            );
        }
    }
}

/// The directory is gone, and the model says so.
///
/// The filesystem is the oracle in both directions. A backend that stops to ask
/// its own user for confirmation leaves a turn that looks entirely reasonable —
/// tool requested, turn ended, no error — with the work simply not done, and
/// only the surviving directory tells those apart. The reverse also happens: a
/// model that reports deleting something it never touched.
fn assert_deleted_directory(turn: &Turn, workspace: &Path) {
    let path = workspace.join(SCRATCH_DIR);
    assert!(
        !path.exists(),
        "{}: {} still exists after a turn that was asked to remove it recursively. The turn \
         emitted {:?} and {} completion(s), and replied {:?}. A provider that gates recursive \
         deletes behind its own confirmation ends the turn exactly like this, with the work undone.",
        turn.label(),
        path.display(),
        turn.tool_request_names(),
        turn.tool_completions().count(),
        turn.final_text()
    );
    assert_final_text_contains(turn, DELETED_MARKER);
}

/// The out-of-band check for a workflow: its agents really ran.
///
/// A workflow that never started and a workflow whose every progress event Tyde
/// discarded produce the same near-empty stream, so no stream assertion can tell
/// them apart. The files can: the agents write them from inside the provider's
/// own subprocess, which reporting defects do not reach. This passing while the
/// assertions below fail is the signature of the whole bug class — the work
/// happened and the account of it was lost.
fn assert_workflow_agents_did_their_work(workflow: &Workflow, workspace: &Path) {
    let missing: Vec<_> = WORKFLOW_FILES
        .iter()
        .filter(|name| !workspace.join(name).is_file())
        .collect();
    assert!(
        missing.is_empty(),
        "{}: {missing:?} were never written, so the workflow's agents never ran and nothing below \
         this line asserts anything about how a run is reported. The launching turn emitted {:?} \
         and replied {:?}.",
        workflow.label(),
        workflow.turn().tool_request_names(),
        workflow.turn().final_text()
    );
    assert_final_text_contains(workflow.turn(), WORKFLOW_MARKER);
}

/// The run's agents reach the client, not just the run.
///
/// The snapshot taken when the run starts carries no agents yet; every agent the
/// user ever sees arrives on a later one. A backend that emits the first snapshot
/// and drops the rest still produces a card, a name and a spinner — it just never
/// fills in what the workflow is actually doing.
fn assert_workflow_reported_its_agents(workflow: &Workflow) {
    let reported = workflow
        .snapshots()
        .map(|snapshot| snapshot.agents.len())
        .max()
        .unwrap_or(0);
    assert!(
        reported >= WORKFLOW_FILES.len(),
        "{}: the richest of {} workflow snapshot(s) named {reported} agent(s), expected at least \
         {}. The agents ran — their files are on disk — so their progress reached Tyde and was \
         dropped before the client.",
        workflow.label(),
        workflow.snapshots().count(),
        WORKFLOW_FILES.len()
    );
}

/// A run that finishes says so.
///
/// The terminal snapshot is the only thing that ever clears the card's spinner or
/// retires the run from the in-flight tray: both key on a non-Running status
/// (`agent/mod.rs:10812`). A run whose terminal snapshot is dropped renders as
/// permanently Running, and survives that way through reload and resume, because
/// the tray is rebuilt from these same events.
fn assert_workflow_reached_terminal(workflow: &Workflow) {
    let statuses: Vec<_> = workflow
        .snapshots()
        .map(|snapshot| snapshot.status)
        .collect();
    let terminal = workflow
        .snapshots()
        .find(|snapshot| snapshot.status != protocol::WorkflowRunStatus::Running);
    let Some(terminal) = terminal else {
        panic!(
            "{}: the workflow never reported a terminal snapshot; the client saw only {statuses:?}. \
             Its agents finished — their files are on disk — so the card is left spinning on a run \
             that is over, and stays that way.",
            workflow.label()
        )
    };
    assert_eq!(
        terminal.status,
        protocol::WorkflowRunStatus::Completed,
        "{}: the workflow terminalized as {:?} though every agent wrote its file; snapshots seen: \
         {statuses:?}",
        workflow.label(),
        terminal.status
    );
    let unfinished: Vec<_> = terminal
        .agents
        .iter()
        .filter(|agent| agent.state != protocol::WorkflowAgentStatus::Done)
        .map(|agent| format!("{}={:?}", agent.label, agent.state))
        .collect();
    assert!(
        unfinished.is_empty(),
        "{}: the workflow reported Completed while still showing {unfinished:?} unfinished; the \
         card contradicts itself",
        workflow.label()
    );
}

/// The run really did outlive the tool call that launched it.
///
/// This is the discriminator, and without it the scenario is vacuous. A provider
/// whose workflow tool *blocks* for the duration of the run would deliver every
/// snapshot while the card is still open — the easy case, and not the one that
/// broke. Claude Code 2.1.220 returns a task id roughly two milliseconds after
/// the run starts, so the card completes and is retired long before the run
/// reports anything, and late progress addressed to a retired card is what got
/// discarded. If this assertion ever fires, the provider changed shape and the
/// rest of this scenario stopped testing what it says it tests.
fn assert_workflow_outlived_its_tool_call(workflow: &Workflow) {
    let completion = workflow.launching_completion_position().unwrap_or_else(|| {
        panic!(
            "{}: never saw the launching tool call complete, so there is no boundary to order \
             progress against",
            workflow.label()
        )
    });
    let terminal = workflow.terminal_snapshot_position().unwrap_or_else(|| {
        panic!(
            "{}: no terminal snapshot to order against the launching tool call",
            workflow.label()
        )
    });
    assert!(
        terminal > completion,
        "{}: the workflow reported its terminal state at event {terminal}, before its own tool \
         call completed at event {completion}. The run did not outlive its tool call, so this \
         scenario exercised none of the late-progress handling it exists for.",
        workflow.label()
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

/// What the user is actually shown: a question with text, and choices to pick.
///
/// A backend that normalizes the tool into an empty question, or into a choice
/// with no labels, renders as an unanswerable card — and every later assertion
/// here would pass over it, because the tool call did happen.
fn assert_question_shape(question: &Question) {
    let asked = question.question();
    assert!(
        !asked.question.trim().is_empty(),
        "{}: emitted a question with no text; the card has nothing to read",
        question.label()
    );
    assert!(
        !asked.options.is_empty(),
        "{}: asked {:?} with no options, so there is nothing for the user to pick",
        question.label(),
        asked.question
    );
    for option in &asked.options {
        assert!(
            !option.label.trim().is_empty(),
            "{}: emitted an unlabelled option among {:?}",
            question.label(),
            asked.options
        );
    }
}

/// The one guarantee unique to interactive tools: the turn ends, the card does
/// not.
///
/// `TurnEmitter` treats any foreground tool still open at idle as a protocol
/// violation and cancels it (`turn_emitter.rs:370`), which for a question means
/// destroying the card the user was about to answer. The real answer then
/// arrives for an id the emitter has already retired and is dropped, and the
/// provider waits forever on a response Tyde can no longer send.
fn assert_question_waits_for_an_answer(question: &Question) {
    assert_no_error_message(&question.label(), question.events());
    let completions: Vec<_> = question
        .completions()
        .map(|completion| format!("{:?}", completion.outcome))
        .collect();
    assert!(
        completions.is_empty(),
        "{}: the question {:?} was completed before anyone answered it ({completions:?}). The card \
         the user was asked to act on was terminalized behind their back.",
        question.label(),
        question.question().question
    );
}

/// An answered question must close its card exactly once *and* reach the model.
///
/// These are separate failures: a card can be completed for the user while the
/// answer never reaches the provider, in which case the model carries on as if
/// it were never told, and the conversation silently diverges from the UI.
fn assert_question_answer_reached_the_model(question: &Question, answered: &Turn, choice: &str) {
    let completions = answered
        .tool_completions()
        .filter(|completion| completion.tool_call_id == question.tool_call_id())
        .count();
    assert_eq!(
        completions,
        1,
        "{}: answering the question produced {completions} completions for {:?}, expected exactly \
         1. Zero leaves the card spinning; more than one means two owners answered it.",
        answered.label(),
        question.tool_call_id()
    );
    let final_text = answered.final_text();
    assert!(
        final_text.contains(choice),
        "{}: the model's reply {final_text:?} never mentions the chosen option {choice:?}. The card \
         closed but the answer did not reach the provider.",
        answered.label()
    );
}

/// The mid-turn compaction must not have swallowed the turn it interrupted.
///
/// A compaction requested while a turn is in flight is parked and dispatched at
/// turn end, so the work still has to finish. Without this, a backend that
/// dropped the turn on the floor would satisfy every marker assertion here.
fn assert_multi_tool_files_were_written(compaction: &Compaction, workspace: &Path) {
    let missing: Vec<_> = MULTI_FILES
        .iter()
        .filter(|name| !workspace.join(name).is_file())
        .collect();
    assert!(
        missing.is_empty(),
        "{}: {missing:?} were never written, so the turn that was interrupted by this compaction \
         never completed its work",
        compaction.label()
    );
}

/// One compaction leaves exactly one row in the transcript.
///
/// A Tyde-requested compaction is sighted twice — once as the requested
/// operation's terminal marker, once as the backend's own observation of the
/// same event — and the agent loop correlates them into a single row by looking
/// up the in-flight operation. The terminal result and the observation reach the
/// loop on different channels, so when the terminal wins the race it has already
/// taken the flight the correlation needs, and the observation lands as a second
/// independent row. Both rows are persisted, so the duplicate survives reload.
fn assert_compaction_left_one_marker(compaction: &Compaction) {
    assert!(
        matches!(
            compaction.terminal().status,
            ContextCompactionStatus::Completed
        ),
        "{}: reported {:?}, so nothing here asserted anything about a compaction that worked",
        compaction.label(),
        compaction.terminal().status
    );
    assert_no_error_message(&compaction.label(), compaction.events());

    let markers: Vec<_> = compaction
        .markers()
        .map(|marker| {
            (
                marker.marker_id.0.clone(),
                marker.trigger,
                marker.operation_id.as_ref().map(|id| id.0.clone()),
            )
        })
        .collect();
    assert_eq!(
        markers.len(),
        1,
        "{}: one compaction produced {} timeline markers, so the chat shows {} rows for a single \
         event: {markers:?}",
        compaction.label(),
        markers.len(),
        markers.len()
    );
}

/// A resumed session replays each thing that happened once.
///
/// Claude rewrites its whole conversation into the session file again on every
/// compaction, preserving each row's original uuid, so a transcript compacted
/// twice holds three copies of the earliest turns. Nothing downstream treats a
/// repeated row as a repeat: the replay re-declares tool ids it has already
/// declared and completed, and re-emits assistant turns the user already read.
///
/// The prompt check is the one that sees a re-appended transcript. Duplicate
/// tool ids do not survive to the client — `TurnEmitter` remembers completed ids
/// and drops the second declaration — so the tool half of this assertion is a
/// guard on that ledger rather than a detector, and the visible damage is the
/// user's own messages appearing twice. Both are asserted because either failing
/// alone points somewhere different.
fn assert_replay_has_no_duplicates(agent: &Agent, backend_kind: BackendKind, prompts: &[String]) {
    let mut requests: BTreeMap<&str, usize> = BTreeMap::new();
    for event in &agent.replayed_history {
        if let ChatEvent::ToolRequest(request) = event {
            *requests.entry(request.tool_call_id.as_str()).or_default() += 1;
        }
    }
    let repeated: Vec<_> = requests
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(id, count)| format!("{id}×{count}"))
        .collect();
    assert!(
        repeated.is_empty(),
        "{backend_kind:?}: the resumed session replayed {} tool request(s) more than once out of \
         {} distinct id(s): {repeated:?}. One tool call became several cards in the restored \
         conversation.",
        repeated.len(),
        requests.len()
    );

    for prompt in prompts {
        let count = agent
            .replayed_history
            .iter()
            .filter(|event| {
                matches!(event, ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::User)
                        && message.content.contains(prompt.as_str()))
            })
            .count();
        assert!(
            count <= 1,
            "{backend_kind:?}: the resumed session replayed the prompt {:?} {count} times; the \
             user's history repeats itself",
            prompt.chars().take(48).collect::<String>()
        );
    }
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
