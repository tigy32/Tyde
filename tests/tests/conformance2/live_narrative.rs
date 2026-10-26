use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub async fn real_live_narrative_child<B: Backend>(host: &Harness<B>) -> bool {
    if std::env::var_os("TYDE_NARRATIVE_GATE_DIR").is_some() {
        return true;
    }
    let script = host.workspace().join("live_narrative.py");
    std::fs::write(&script, include_str!("live_narrative.py")).expect("write real stream fixture");
    let status = tokio::process::Command::new("python3")
        .arg(script)
        .arg(std::env::current_exe().expect("conformance executable"))
        .arg(&host.test_name)
        .status()
        .await
        .expect("run real stream fixture");
    assert!(
        status.success(),
        "real narrative scenario failed; see retained fixture evidence"
    );
    false
}

pub async fn held_live_narrative_turn<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    phase: &str,
) -> Turn {
    let directory = std::path::PathBuf::from(
        std::env::var_os("TYDE_NARRATIVE_GATE_DIR").expect("real stream control directory"),
    );
    let listener = tokio::net::UnixListener::bind(directory.join("gate.sock"))
        .expect("bind stream receipt control");
    let (receipts, mut processed) = tokio::sync::mpsc::unbounded_channel();
    NARRATIVE_RECEIPTS.with(|sender| *sender.borrow_mut() = Some(receipts));
    std::fs::write(directory.join("armed"), phase).expect("arm next real inference");
    let status = std::process::Command::new("mkfifo")
        .arg(host.workspace().join("live-release.fifo"))
        .status()
        .expect("create command barrier");
    assert!(status.success(), "create command barrier");
    let prompt = "First write a commentary paragraph beginning LIVE_PREFIX explaining in three sentences that you will run one command and await its completion. Then use exactly one code-mode exec cell with a 1 ms exec yield. In that cell await tools.exec_command({cmd: \"python3 -c \\\"open('live-release.fifo').read(); print('LIVE_TOOL')\\\"\", yield_time_ms: 300000}) and print the returned result. The harness releases the FIFO, not you. If the cell yields use wait until it completes. Do not use sleeps or any other command. Finally reply LIVE_DONE.";
    send_prompt(host, agent, prompt).await;
    let (mut gate, _) = listener
        .accept()
        .await
        .expect("real inference terminal gate");
    assert_eq!(gate.read_u8().await.expect("terminal held receipt"), 1);
    while processed.recv().await.expect("adapter dispatch receipt") != NativeReceipt::Narrative {}
    let mut turn = host.turn(prompt);
    drain_settled_events(host, &mut turn);
    let live = turn.events.iter().any(
        |event| matches!(event, ChatEvent::StreamDelta(delta) if !delta.text.trim().is_empty()),
    );
    let premature_end = turn.events.iter().any(|event| {
        matches!(
            event,
            ChatEvent::StreamEnd(_) | ChatEvent::TypingStatusChanged(false)
        )
    });
    eprintln!(
        "LIVE NARRATIVE EVIDENCE phase={phase} actual_terminal_held=true native_delta_processed=true live_text={live} premature_end={premature_end}"
    );
    gate.write_u8(1)
        .await
        .expect("release real inference terminal");
    NARRATIVE_RECEIPTS.with(|sender| *sender.borrow_mut() = None);
    let is_command = |event: &ChatEvent| matches!(event, ChatEvent::ToolRequest(request) if matches!(&request.tool_type, ToolRequestType::RunCommand { command, .. } if command.contains("live-release.fifo")));
    if !turn.events.iter().any(is_command) {
        collect_turn_until(host, &mut turn, is_command).await;
    }
    tokio::fs::write(host.workspace().join("live-release.fifo"), b"release")
        .await
        .expect("release observed command");
    collect_turn_until(host, &mut turn, |event| {
        matches!(event, ChatEvent::TypingStatusChanged(false))
    })
    .await;
    std::fs::remove_file(host.workspace().join("live-release.fifo"))
        .expect("remove completed command barrier");
    drop(listener);
    std::fs::remove_file(directory.join("gate.sock")).expect("remove completed stream control");
    assert!(
        live,
        "the session withheld live text until the real inference terminal"
    );
    assert!(
        !premature_end,
        "a live inference cannot have ended its response"
    );
    let text = turn
        .events
        .iter()
        .position(|event| matches!(event, ChatEvent::StreamDelta(_)))
        .expect("live text");
    let command = turn
        .events
        .iter()
        .position(is_command)
        .expect("observed command");
    assert!(text < command, "the native commentary precedes its command");
    super::super::assert_final_text_contains(&turn, "LIVE_DONE");
    assert_exact_streamed_content(&turn);
    super::super::assert_no_well_formed_zeros(&turn);
    turn
}

fn drain_settled_events<B: Backend>(host: &mut Harness<B>, turn: &mut Turn) {
    let events = host.events.as_mut().expect("live backend events");
    while let Ok(event) = events.try_recv_backend() {
        match event {
            BackendEvent::Chat(event) => turn.events.push(event),
            BackendEvent::ModelRequestTokenUsage(usage) => turn.model_requests.push(usage),
            BackendEvent::Compaction(_) => panic!("unexpected compaction in small live narrative"),
            BackendEvent::ResumeReplayComplete(_) => panic!("resume was already replayed"),
        }
    }
}

pub async fn interrupted_held_narrative<B: Backend>(host: &mut Harness<B>, agent: &Agent) -> Turn {
    let directory = std::path::PathBuf::from(
        std::env::var_os("TYDE_NARRATIVE_GATE_DIR").expect("stream gate directory"),
    );
    let listener = tokio::net::UnixListener::bind(directory.join("gate.sock"))
        .expect("bind text terminal gate");
    let (receipts, mut processed) = tokio::sync::mpsc::unbounded_channel();
    NARRATIVE_RECEIPTS.with(|sender| *sender.borrow_mut() = Some(receipts));
    std::fs::write(directory.join("armed"), "interrupted").expect("arm incomplete narrative");
    let prompt = "Run exactly one exec_command: printf LIVE_TOOL_RECEIPT. Do not write any commentary before the command. After its result, write a commentary paragraph beginning LIVE_INTERRUPTED_PREFIX explaining the command in three sentences. Do not use further tools.";
    send_prompt(host, agent, prompt).await;
    let (mut gate, _) = listener
        .accept()
        .await
        .expect("real partial text terminal gate");
    assert_eq!(gate.read_u8().await.expect("text terminal held"), 1);
    let mut tool_completed = false;
    while let NativeReceipt::ToolCompletion =
        processed.recv().await.expect("actual native dispatch")
    {
        tool_completed = true;
    }
    let mut turn = host.turn(prompt);
    drain_settled_events(host, &mut turn);
    let prefix_before = turn
        .events
        .iter()
        .filter_map(|event| match event {
            ChatEvent::StreamDelta(delta) => Some(delta.text.as_str()),
            _ => None,
        })
        .collect::<String>()
        .contains("LIVE_INTERRUPTED_PREFIX");
    interrupt_backend(host).await;
    collect_turn_until(host, &mut turn, |event| {
        matches!(event, ChatEvent::TypingStatusChanged(false))
    })
    .await;
    gate.write_u8(1)
        .await
        .expect("release cancelled transport fixture");
    NARRATIVE_RECEIPTS.with(|sender| *sender.borrow_mut() = None);
    drop(listener);
    std::fs::remove_file(directory.join("gate.sock")).expect("remove completed stream control");
    let prefix_end = turn.events.iter().position(|event| matches!(event, ChatEvent::StreamEnd(end) if end.message.content.contains("LIVE_INTERRUPTED_PREFIX")));
    let mut streamed = String::new();
    let mut prefix_delta = None;
    for (position, event) in turn.events.iter().enumerate() {
        match event {
            ChatEvent::StreamStart(_) => streamed.clear(),
            ChatEvent::StreamDelta(delta) => {
                streamed.push_str(&delta.text);
                if prefix_delta.is_none() && streamed.contains("LIVE_INTERRUPTED_PREFIX") {
                    prefix_delta = Some(position);
                }
            }
            _ => {}
        }
    }
    eprintln!(
        "LIVE NARRATIVE EVIDENCE phase=interrupted actual_partial_item=true tool_completed_first={tool_completed} prefix_already_live={prefix_before} prefix_preserved={}",
        prefix_end.is_some()
    );
    assert!(
        tool_completed,
        "the real command completion must be processed before its following narrative"
    );
    assert!(
        turn.tool_completions().any(|completion| matches!(&completion.outcome,
            ToolExecutionOutcome::Succeeded { result: ToolExecutionResult::RunCommand { exit_code: 0, stdout, .. } }
                if stdout.trim() == "LIVE_TOOL_RECEIPT")),
        "the completed command keeps its actual result through the interruption"
    );
    assert!(
        prefix_before,
        "narrative after a completed tool must be live before the inference terminal"
    );
    assert!(
        matches!((prefix_delta, prefix_end), (Some(delta), Some(end)) if delta < end),
        "interrupted partial text must precede its own response end"
    );
    assert_exact_streamed_content(&turn);
    super::super::assert_no_well_formed_zeros(&turn);
    turn
}

/// Interrupts a turn while its real model request is held before any response
/// byte, then requires the same session to answer its next turn.
pub async fn interrupted_held_request<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    phase: &str,
) -> Turn {
    let directory = std::path::PathBuf::from(
        std::env::var_os("TYDE_NARRATIVE_GATE_DIR").expect("stream gate directory"),
    );
    let listener = tokio::net::UnixListener::bind(directory.join("gate.sock"))
        .expect("bind model request gate");
    std::fs::write(directory.join("armed"), format!("request:{phase}"))
        .expect("arm held model request");
    let prompt = "Reply TYDE_READY without tools.";
    send_prompt(host, agent, prompt).await;
    let (mut gate, _) = listener.accept().await.expect("real model request gate");
    assert_eq!(gate.read_u8().await.expect("model request held"), 1);
    let mut interrupted = host.turn(prompt);
    interrupt_backend(host).await;
    let events = host.events.as_mut().expect("live backend events");
    loop {
        match events
            .recv_backend()
            .await
            .expect("backend closed during interrupted request")
        {
            BackendEvent::Chat(event) => {
                let idle = matches!(event, ChatEvent::TypingStatusChanged(false));
                interrupted.events.push(event);
                if idle {
                    break;
                }
            }
            BackendEvent::ModelRequestTokenUsage(usage) => interrupted.model_requests.push(usage),
            BackendEvent::Compaction(_) => panic!("unexpected compaction in held request"),
            BackendEvent::ResumeReplayComplete(_) => panic!("resume was already replayed"),
        }
    }
    gate.write_u8(1)
        .await
        .expect("release cancelled model request");
    drop(listener);
    std::fs::remove_file(directory.join("gate.sock")).expect("remove completed request control");
    super::super::assert_no_error_message(&interrupted.label(), &interrupted.events);
    assert_eq!(
        interrupted
            .events
            .iter()
            .filter(|event| matches!(event, ChatEvent::OperationCancelled(_)))
            .count(),
        1,
        "the held request's interrupt must be reported exactly once"
    );
    let follow_up = ask(host, agent, prompt).await;
    super::super::assert_ready_handshake(&follow_up);
    eprintln!("REQUEST INTERRUPT EVIDENCE phase={phase} request_held=true follow_up_answered=true");
    follow_up
}

fn assert_exact_streamed_content(turn: &Turn) {
    let mut stream = None;
    for event in &turn.events {
        match event {
            ChatEvent::StreamStart(_) => {
                assert!(
                    stream.replace(String::new()).is_none(),
                    "response starts once"
                );
            }
            ChatEvent::StreamDelta(delta) => {
                stream
                    .as_mut()
                    .expect("delta follows its start")
                    .push_str(&delta.text);
            }
            ChatEvent::StreamEnd(end) => {
                let text = stream.take().expect("response ends once");
                assert!(
                    text == end.message.content,
                    "final response retains exactly its streamed text without replay or duplication"
                );
            }
            _ => {}
        }
    }
    assert!(
        stream.is_none(),
        "every live prefix reaches its own response end"
    );
}

/// A chain of reads where each file names the next forces one model request
/// per read. Every request must close its own response, live, before the next
/// request acts: a rejoined session must not grow one card across requests.
pub async fn chained_request_turn<B: Backend>(
    host: &mut Harness<B>,
    agent: &Agent,
    phase: &str,
) -> Turn {
    let tag = uuid::Uuid::new_v4().simple().to_string()[..10].to_owned();
    let names = ["a", "b", "c"].map(|step| format!("chain-{tag}-{step}.txt"));
    let done = format!("CHAIN_DONE_{}", tag.to_ascii_uppercase());
    for (index, name) in names.iter().enumerate() {
        let next = names.get(index + 1).unwrap_or(&done);
        std::fs::write(host.workspace().join(name), next).expect("write chained read");
    }
    let prompt = format!(
        "Read the file {} in the current directory with a shell command. Its contents name the \
         next file to read; read that one the same way, and repeat until a file's contents begin \
         CHAIN_DONE. Issue each read only after you have seen the previous result: every read is \
         its own tool call from its own response, and no call, cell or script may read more than \
         one file. Write no text before or between the reads. Finally reply with the last file's \
         complete contents, copied exactly, and nothing else.",
        names[0]
    );
    let turn = ask(host, agent, &prompt).await;
    let refers = |value: &str, name: &str| value.contains(name);
    let declaring_ends = names
        .iter()
        .map(|name| {
            turn.events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    matches!(event, ChatEvent::StreamEnd(end) if end.message.tool_calls.iter().any(|call| refers(&call.arguments.to_string(), name)))
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let requests = names
        .iter()
        .map(|name| {
            turn.events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    matches!(event, ChatEvent::ToolRequest(request) if turn.tool_declarations().any(|call| call.tool_call_id == request.tool_call_id && refers(&call.arguments.to_string(), name)))
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let responses = turn
        .events
        .iter()
        .filter(|event| matches!(event, ChatEvent::StreamEnd(_)))
        .count();
    let shape = turn
        .events
        .iter()
        .map(|event| match event {
            ChatEvent::StreamEnd(end) => format!(
                "StreamEnd(calls={},final={})",
                end.message.tool_calls.len(),
                end.message.content.contains(&done)
            ),
            ChatEvent::ToolRequest(_) => "ToolRequest".to_owned(),
            ChatEvent::ToolProgress(_) => "ToolProgress".to_owned(),
            ChatEvent::ToolExecutionCompleted(_) => "ToolExecutionCompleted".to_owned(),
            other => crate::describe_event(other),
        })
        .collect::<Vec<_>>();
    eprintln!(
        "REQUEST BOUNDARY EVIDENCE phase={phase} responses={responses} model_requests={} declaring_ends={declaring_ends:?} chained_requests={requests:?} events={shape:?}",
        turn.model_requests.len()
    );
    for ends in &declaring_ends {
        assert_eq!(
            ends.len(),
            1,
            "each chained read is declared by exactly one response"
        );
    }
    for step in 1..names.len() {
        assert!(
            declaring_ends[step - 1][0] < declaring_ends[step][0],
            "each chained read belongs to its own, later response"
        );
        assert!(
            requests[step]
                .iter()
                .all(|request| *request > declaring_ends[step - 1][0]),
            "a request's response closes before the next request acts"
        );
    }
    let last = turn
        .events
        .iter()
        .rposition(|event| matches!(event, ChatEvent::StreamEnd(_)))
        .expect("final response");
    assert!(
        last > declaring_ends[names.len() - 1][0]
            && matches!(&turn.events[last], ChatEvent::StreamEnd(end) if end.message.tool_calls.is_empty() && end.message.content.contains(&done)),
        "the final answer is its own response after the last read"
    );
    assert_exact_streamed_content(&turn);
    turn
}
