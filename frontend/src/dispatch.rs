use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};

use protocol::{
    AgentErrorPayload, AgentId, AgentStartPayload, ChatEvent, Envelope, FrameKind,
    HostSettingsPayload, NewAgentPayload, NewTerminalPayload, Project, ProjectFileContentsPayload,
    ProjectFileListPayload, ProjectGitDiffPayload, ProjectGitStatusPayload, ProjectId,
    ProjectNotifyPayload, ProtocolValidator, RejectPayload, SessionListPayload, StreamPath,
    TerminalErrorPayload, TerminalExitPayload, TerminalOutputPayload, TerminalStartPayload,
};

use crate::state::{
    AgentInfo, AppState, CenterView, ChatMessageEntry, ConnectionStatus, DiffViewState, OpenFile,
    StreamingState, TerminalInfo, ToolRequestEntry, TransientEvent,
};

/// Non-panicking sequence validator for inbound frames.
/// Logs errors on mismatch instead of panicking like the protocol crate's `SeqValidator`.
struct FrontendSeqValidator {
    expected: HashMap<StreamPath, u64>,
}

impl FrontendSeqValidator {
    fn new() -> Self {
        Self {
            expected: HashMap::new(),
        }
    }

    fn validate(&mut self, stream: &StreamPath, seq: u64, kind: FrameKind) -> bool {
        let expected = self.expected.get(stream).copied().unwrap_or(0);
        if seq != expected {
            log::error!(
                "sequence mismatch on stream {stream} kind {kind}: expected {expected}, got {seq}"
            );
            return false;
        }
        self.expected.insert(stream.clone(), expected + 1);
        true
    }
}

thread_local! {
    static INBOUND_SEQ: RefCell<FrontendSeqValidator> = RefCell::new(FrontendSeqValidator::new());
    static INBOUND_PROTOCOL: RefCell<ProtocolValidator> = RefCell::new(ProtocolValidator::new());
}

pub fn dispatch_envelope(state: &AppState, envelope: Envelope) {
    // Validate inbound sequence numbers (log-only, still dispatch on mismatch)
    INBOUND_SEQ.with(|v| {
        v.borrow_mut()
            .validate(&envelope.stream, envelope.seq, envelope.kind)
    });
    INBOUND_PROTOCOL.with(|v| {
        if let Err(error) = v.borrow_mut().validate_envelope(&envelope) {
            log::error!("protocol violation: {error}");
        }
    });

    match envelope.kind {
        FrameKind::Welcome => {
            state.connection_status.set(ConnectionStatus::Connected);
            log::info!("connected to server");
        }
        FrameKind::Reject => match envelope.parse_payload::<RejectPayload>() {
            Ok(p) => {
                log::error!("connection rejected: {} ({:?})", p.message, p.code);
                state
                    .connection_status
                    .set(ConnectionStatus::Error(p.message));
            }
            Err(e) => {
                log::error!("failed to parse reject payload: {e}");
                state
                    .connection_status
                    .set(ConnectionStatus::Error("rejected".into()));
            }
        },
        FrameKind::HostSettings => match envelope.parse_payload::<HostSettingsPayload>() {
            Ok(p) => {
                state.host_settings.set(Some(p.settings));
            }
            Err(e) => log::error!("failed to parse host_settings payload: {e}"),
        },
        FrameKind::NewAgent => match envelope.parse_payload::<NewAgentPayload>() {
            Ok(p) => {
                let agent_id = p.agent_id.clone();
                let info = AgentInfo {
                    agent_id: p.agent_id,
                    name: p.name,
                    backend_kind: p.backend_kind,
                    workspace_roots: p.workspace_roots,
                    project_id: p.project_id,
                    parent_agent_id: p.parent_agent_id,
                    created_at_ms: p.created_at_ms,
                    instance_stream: p.instance_stream,
                    fatal_error: None,
                };
                state
                    .agents
                    .update(|agents: &mut Vec<AgentInfo>| agents.push(info));
                state.active_agent_id.set(Some(agent_id));
                state.agent_initializing.set(false);
                state.center_view.set(CenterView::Chat);
            }
            Err(e) => log::error!("failed to parse new_agent payload: {e}"),
        },
        FrameKind::AgentStart => match envelope.parse_payload::<AgentStartPayload>() {
            Ok(_p) => {
                // AgentStart is the birth certificate — no state change needed.
                // Agent runtime state is implicit from StreamStart/StreamEnd events.
            }
            Err(e) => log::error!("failed to parse agent_start payload: {e}"),
        },
        FrameKind::AgentError => match envelope.parse_payload::<AgentErrorPayload>() {
            Ok(p) => {
                let error_msg = p.message.clone();
                let error_agent_id = p.agent_id.clone();
                if p.fatal {
                    state.agents.update(|agents: &mut Vec<AgentInfo>| {
                        if let Some(agent) = agents.iter_mut().find(|a| a.agent_id == p.agent_id) {
                            agent.fatal_error = Some(p.message);
                        }
                    });
                }
                // Also inject an inline error message into the chat
                let entry = ChatMessageEntry {
                    message: protocol::ChatMessage {
                        timestamp: js_sys::Date::now() as u64,
                        sender: protocol::MessageSender::Error,
                        content: error_msg,
                        reasoning: None,
                        tool_calls: Vec::new(),
                        model_info: None,
                        token_usage: None,
                        context_breakdown: None,
                        images: None,
                    },
                    tool_requests: Vec::new(),
                };
                state.chat_messages.update(
                    |map: &mut std::collections::HashMap<
                        protocol::AgentId,
                        Vec<ChatMessageEntry>,
                    >| {
                        map.entry(error_agent_id).or_default().push(entry);
                    },
                );
            }
            Err(e) => log::error!("failed to parse agent_error payload: {e}"),
        },
        FrameKind::ChatEvent => {
            dispatch_chat_event(state, &envelope.stream, &envelope);
        }
        FrameKind::SessionList => match envelope.parse_payload::<SessionListPayload>() {
            Ok(p) => {
                state.sessions.set(p.sessions);
            }
            Err(e) => log::error!("failed to parse session_list payload: {e}"),
        },
        FrameKind::ProjectNotify => match envelope.parse_payload::<ProjectNotifyPayload>() {
            Ok(ProjectNotifyPayload::Upsert { project }) => {
                state.projects.update(|projects: &mut Vec<Project>| {
                    if let Some(existing) = projects.iter_mut().find(|p| p.id == project.id) {
                        *existing = project;
                    } else {
                        projects.push(project);
                    }
                });
            }
            Ok(ProjectNotifyPayload::Delete { project }) => {
                state
                    .projects
                    .update(|projects: &mut Vec<Project>| projects.retain(|p| p.id != project.id));
            }
            Err(e) => log::error!("failed to parse project_notify payload: {e}"),
        },
        FrameKind::ProjectFileList => {
            let project_id = match resolve_project_id(&envelope.stream) {
                Some(id) => id,
                None => {
                    log::warn!(
                        "project_file_list on non-project stream {}",
                        envelope.stream
                    );
                    return;
                }
            };
            match envelope.parse_payload::<ProjectFileListPayload>() {
                Ok(p) => {
                    let diff_entries: Vec<_> =
                        p.roots.into_iter().flat_map(|r| r.entries).collect();
                    state.file_tree.update(
                        |map: &mut HashMap<ProjectId, Vec<protocol::ProjectFileEntry>>| {
                            let existing = map.entry(project_id.clone()).or_default();
                            for entry in diff_entries {
                                match entry.op {
                                    protocol::FileEntryOp::Add => {
                                        if !existing
                                            .iter()
                                            .any(|e| e.relative_path == entry.relative_path)
                                        {
                                            existing.push(entry);
                                        }
                                    }
                                    protocol::FileEntryOp::Remove => {
                                        existing.retain(|e| e.relative_path != entry.relative_path);
                                    }
                                }
                            }
                        },
                    );
                }
                Err(e) => log::error!("failed to parse project_file_list payload: {e}"),
            }
        }
        FrameKind::ProjectGitStatus => {
            let project_id = match resolve_project_id(&envelope.stream) {
                Some(id) => id,
                None => {
                    log::warn!(
                        "project_git_status on non-project stream {}",
                        envelope.stream
                    );
                    return;
                }
            };
            match envelope.parse_payload::<ProjectGitStatusPayload>() {
                Ok(p) => {
                    state.git_status.update(
                        |map: &mut HashMap<ProjectId, Vec<protocol::ProjectRootGitStatus>>| {
                            map.insert(project_id, p.roots);
                        },
                    );
                }
                Err(e) => log::error!("failed to parse project_git_status payload: {e}"),
            }
        }
        FrameKind::ProjectGitDiff => match envelope.parse_payload::<ProjectGitDiffPayload>() {
            Ok(p) => {
                state.diff_content.set(Some(DiffViewState {
                    root: p.root,
                    scope: p.scope,
                    files: p.files,
                }));
            }
            Err(e) => log::error!("failed to parse project_git_diff payload: {e}"),
        },
        FrameKind::ProjectFileContents => {
            match envelope.parse_payload::<ProjectFileContentsPayload>() {
                Ok(p) => {
                    state.open_file.set(Some(OpenFile {
                        path: p.path,
                        contents: p.contents,
                        is_binary: p.is_binary,
                    }));
                    state.center_view.set(CenterView::Editor);
                }
                Err(e) => log::error!("failed to parse project_file_contents payload: {e}"),
            }
        }
        FrameKind::NewTerminal => {
            match envelope.parse_payload::<NewTerminalPayload>() {
                Ok(p) => {
                    let info = TerminalInfo {
                        terminal_id: p.terminal_id,
                        stream: p.stream,
                        project_id: None,
                        cwd: String::new(),
                        shell: String::new(),
                        cols: 80,
                        rows: 24,
                        created_at_ms: 0,
                        output_buffer: String::new(),
                        exited: false,
                        exit_code: None,
                    };
                    state
                        .terminals
                        .update(|terms: &mut Vec<TerminalInfo>| terms.push(info));
                    // Auto-select first terminal
                    if state.active_terminal_id.get_untracked().is_none() {
                        let id = state
                            .terminals
                            .with_untracked(|terms| terms.last().map(|t| t.terminal_id.clone()));
                        state.active_terminal_id.set(id);
                    }
                }
                Err(e) => log::error!("failed to parse new_terminal payload: {e}"),
            }
        }
        FrameKind::TerminalStart => match envelope.parse_payload::<TerminalStartPayload>() {
            Ok(p) => {
                state.terminals.update(|terms: &mut Vec<TerminalInfo>| {
                    if let Some(t) = terms.iter_mut().find(|t| t.stream == envelope.stream) {
                        t.project_id = p.project_id;
                        t.cwd = p.cwd;
                        t.shell = p.shell;
                        t.cols = p.cols;
                        t.rows = p.rows;
                        t.created_at_ms = p.created_at_ms;
                    }
                });
            }
            Err(e) => log::error!("failed to parse terminal_start payload: {e}"),
        },
        FrameKind::TerminalOutput => match envelope.parse_payload::<TerminalOutputPayload>() {
            Ok(p) => {
                state.terminals.update(|terms: &mut Vec<TerminalInfo>| {
                    if let Some(t) = terms.iter_mut().find(|t| t.stream == envelope.stream) {
                        t.output_buffer.push_str(&p.data);
                    }
                });
            }
            Err(e) => log::error!("failed to parse terminal_output payload: {e}"),
        },
        FrameKind::TerminalExit => match envelope.parse_payload::<TerminalExitPayload>() {
            Ok(p) => {
                state.terminals.update(|terms: &mut Vec<TerminalInfo>| {
                    if let Some(t) = terms.iter_mut().find(|t| t.stream == envelope.stream) {
                        t.exited = true;
                        t.exit_code = p.exit_code;
                    }
                });
            }
            Err(e) => log::error!("failed to parse terminal_exit payload: {e}"),
        },
        FrameKind::TerminalError => match envelope.parse_payload::<TerminalErrorPayload>() {
            Ok(p) => {
                log::error!("terminal error ({:?}): {}", p.code, p.message);
                if p.fatal {
                    state.terminals.update(|terms: &mut Vec<TerminalInfo>| {
                        if let Some(t) = terms.iter_mut().find(|t| t.stream == envelope.stream) {
                            t.exited = true;
                        }
                    });
                }
            }
            Err(e) => log::error!("failed to parse terminal_error payload: {e}"),
        },
        // Client->server kinds should never arrive from server
        _ => {
            log::warn!("unexpected frame kind from server: {}", envelope.kind);
        }
    }
}

/// Extract project_id from a stream path like `/project/{project_id}`.
fn resolve_project_id(stream: &StreamPath) -> Option<ProjectId> {
    let path = &stream.0;
    let suffix = path.strip_prefix("/project/")?;
    if suffix.is_empty() {
        return None;
    }
    Some(ProjectId(suffix.to_owned()))
}

fn resolve_agent_id(state: &AppState, stream: &StreamPath) -> Option<AgentId> {
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|a| a.instance_stream == *stream)
            .map(|a| a.agent_id.clone())
    })
}

fn dispatch_chat_event(state: &AppState, stream: &StreamPath, envelope: &Envelope) {
    let agent_id = match resolve_agent_id(state, stream) {
        Some(id) => id,
        None => {
            log::warn!("chat_event on unknown stream {stream}");
            return;
        }
    };

    let event = match envelope.parse_payload::<ChatEvent>() {
        Ok(ev) => ev,
        Err(e) => {
            log::error!(
                "failed to parse chat_event payload: {e}\nraw: {}",
                serde_json::to_string(&envelope.payload).unwrap_or_default(),
            );
            return;
        }
    };

    match event {
        ChatEvent::TypingStatusChanged(typing) => {
            state.agent_turn_active.update(|map| {
                if typing {
                    map.insert(agent_id, true);
                } else {
                    map.remove(&agent_id);
                }
            });
        }

        ChatEvent::MessageAdded(msg) => {
            log::info!(
                "MessageAdded for {}: sender={:?} token_usage={} tools={}",
                agent_id,
                msg.sender,
                msg.token_usage.is_some(),
                msg.tool_calls.len(),
            );
            let entry = ChatMessageEntry {
                message: msg,
                tool_requests: Vec::new(),
            };
            state.chat_messages.update(
                |map: &mut std::collections::HashMap<AgentId, Vec<ChatMessageEntry>>| {
                    map.entry(agent_id).or_default().push(entry);
                },
            );
        }

        ChatEvent::StreamStart(data) => {
            let ss = StreamingState {
                agent_name: data.agent,
                model: data.model,
                text: leptos::prelude::ArcRwSignal::new(String::new()),
                reasoning: leptos::prelude::ArcRwSignal::new(String::new()),
                tool_requests: leptos::prelude::ArcRwSignal::new(Vec::new()),
            };
            state.streaming_text.update(
                |map: &mut std::collections::HashMap<AgentId, StreamingState>| {
                    map.insert(agent_id, ss);
                },
            );
        }

        ChatEvent::StreamDelta(data) => {
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(ss) = streaming {
                ss.text.update(|text| text.push_str(&data.text));
            }
        }

        ChatEvent::StreamReasoningDelta(data) => {
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(ss) = streaming {
                ss.reasoning
                    .update(|reasoning| reasoning.push_str(&data.text));
            }
        }

        ChatEvent::StreamEnd(data) => {
            let has_token_usage = data.message.token_usage.is_some();
            let has_context = data.message.context_breakdown.is_some();
            let tool_call_count = data.message.tool_calls.len();
            log::info!(
                "StreamEnd for {}: token_usage={}, context_breakdown={}, tool_calls={}",
                agent_id,
                has_token_usage,
                has_context,
                tool_call_count,
            );
            if let Some(ref tu) = data.message.token_usage {
                log::info!(
                    "  token_usage: input={} output={} cached={:?} reasoning={:?}",
                    tu.input_tokens,
                    tu.output_tokens,
                    tu.cached_prompt_tokens,
                    tu.reasoning_tokens,
                );
            }
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            let tool_requests = streaming
                .as_ref()
                .map(|ss| ss.tool_requests.get_untracked())
                .unwrap_or_default();
            state.streaming_text.update(
                |map: &mut std::collections::HashMap<AgentId, StreamingState>| {
                    map.remove(&agent_id);
                },
            );
            let entry = ChatMessageEntry {
                message: data.message,
                tool_requests,
            };
            state.chat_messages.update(
                |map: &mut std::collections::HashMap<AgentId, Vec<ChatMessageEntry>>| {
                    map.entry(agent_id).or_default().push(entry);
                },
            );
        }

        ChatEvent::ToolRequest(req) => {
            let tool_name = req.tool_name.clone();
            let tool_call_id = req.tool_call_id.clone();
            log::info!(
                "ToolRequest for {}: tool={} call_id={}",
                agent_id,
                tool_name,
                tool_call_id,
            );
            let tool_entry = ToolRequestEntry {
                request: req,
                result: None,
            };
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(ss) = streaming {
                ss.tool_requests.update(|tools| tools.push(tool_entry));
                return;
            }
            state.chat_messages.update(
                |map: &mut std::collections::HashMap<AgentId, Vec<ChatMessageEntry>>| {
                    if let Some(messages) = map.get_mut(&agent_id) {
                        if let Some(last) = messages.last_mut() {
                            last.tool_requests.push(tool_entry);
                        } else {
                            log::error!(
                                "TOOL REQUEST DROPPED: tool '{}' (call_id={}) for agent {} — no messages exist yet to attach it to",
                                tool_name, tool_call_id, agent_id
                            );
                        }
                    } else {
                        log::error!(
                            "TOOL REQUEST DROPPED: tool '{}' (call_id={}) for agent {} — agent has no message list",
                            tool_name, tool_call_id, agent_id
                        );
                    }
                },
            );
        }

        ChatEvent::ToolExecutionCompleted(data) => {
            let call_id = data.tool_call_id.clone();
            let tool_name = data.tool_name.clone();
            log::info!(
                "ToolExecutionCompleted for {}: tool={} call_id={} success={}",
                agent_id,
                tool_name,
                call_id,
                data.success,
            );
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(ss) = streaming {
                let mut matched = false;
                ss.tool_requests.update(|tools| {
                    if let Some(tr) = tools.iter_mut().find(|t| t.request.tool_call_id == call_id) {
                        tr.result = Some(data.clone());
                        matched = true;
                    }
                });
                if matched {
                    return;
                }
            }
            state.chat_messages.update(
                |map: &mut std::collections::HashMap<AgentId, Vec<ChatMessageEntry>>| {
                    if let Some(messages) = map.get_mut(&agent_id) {
                        // Find the tool request by call id in any message
                        for msg in messages.iter_mut().rev() {
                            if let Some(tr) = msg
                                .tool_requests
                                .iter_mut()
                                .find(|t| t.request.tool_call_id == call_id)
                            {
                                tr.result = Some(data);
                                return;
                            }
                        }
                        log::error!(
                            "TOOL RESULT ORPHANED: completion for tool '{}' (call_id={}) for agent {} — no matching request found (was the request dropped?)",
                            tool_name, call_id, agent_id
                        );
                    } else {
                        log::error!(
                            "TOOL RESULT ORPHANED: completion for tool '{}' (call_id={}) for agent {} — agent has no message list",
                            tool_name, call_id, agent_id
                        );
                    }
                },
            );
        }

        ChatEvent::TaskUpdate(task_list) => {
            state.task_lists.update(
                |map: &mut std::collections::HashMap<AgentId, protocol::TaskList>| {
                    map.insert(agent_id, task_list);
                },
            );
        }

        ChatEvent::OperationCancelled(data) => {
            state.streaming_text.update(
                |map: &mut std::collections::HashMap<AgentId, StreamingState>| {
                    map.remove(&agent_id);
                },
            );
            let event = TransientEvent::OperationCancelled {
                message: data.message,
            };
            state.transient_events.update(
                |map: &mut std::collections::HashMap<AgentId, Vec<TransientEvent>>| {
                    map.entry(agent_id).or_default().push(event);
                },
            );
        }

        ChatEvent::RetryAttempt(data) => {
            let event = TransientEvent::RetryAttempt {
                attempt: data.attempt,
                max_retries: data.max_retries,
                error: data.error,
                backoff_ms: data.backoff_ms,
            };
            state.transient_events.update(
                |map: &mut std::collections::HashMap<AgentId, Vec<TransientEvent>>| {
                    map.entry(agent_id).or_default().push(event);
                },
            );
        }
    }
}
