use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::{Get, Set, Update};
use protocol::{
    AgentErrorPayload, AgentId, AgentStartPayload, ChatEvent, Envelope, FrameKind,
    NewAgentPayload, NewTerminalPayload, Project, ProjectFileContentsPayload,
    ProjectFileListPayload, ProjectGitDiffPayload, ProjectGitStatusPayload, ProjectId,
    ProjectNotifyPayload, RejectPayload, SessionListPayload, StreamPath, TerminalErrorPayload,
    TerminalExitPayload, TerminalOutputPayload, TerminalStartPayload,
};

use crate::state::{
    AgentInfo, AgentStatus, AppState, CenterView, ChatMessageEntry, ConnectionStatus,
    DiffViewState, OpenFile, StreamingState, TerminalInfo, ToolRequestEntry, TransientEvent,
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
}

pub fn dispatch_envelope(state: &AppState, envelope: Envelope) {
    // Validate inbound sequence numbers (log-only, still dispatch on mismatch)
    INBOUND_SEQ.with(|v| {
        v.borrow_mut()
            .validate(&envelope.stream, envelope.seq, envelope.kind)
    });

    match envelope.kind {
        FrameKind::Welcome => {
            state.connection_status.set(ConnectionStatus::Connected);
            log::info!("connected to server");
        }
        FrameKind::Reject => {
            match envelope.parse_payload::<RejectPayload>() {
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
            }
        }
        FrameKind::NewAgent => {
            match envelope.parse_payload::<NewAgentPayload>() {
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
                        status: AgentStatus::Starting,
                    };
                    state.agents.update(|agents: &mut Vec<AgentInfo>| agents.push(info));
                    state.active_agent_id.set(Some(agent_id));
                    state.center_view.set(CenterView::Chat);
                }
                Err(e) => log::error!("failed to parse new_agent payload: {e}"),
            }
        }
        FrameKind::AgentStart => {
            match envelope.parse_payload::<AgentStartPayload>() {
                Ok(p) => {
                    state.agents.update(|agents: &mut Vec<AgentInfo>| {
                        if let Some(agent) =
                            agents.iter_mut().find(|a| a.agent_id == p.agent_id)
                        {
                            agent.status = AgentStatus::Running;
                        }
                    });
                }
                Err(e) => log::error!("failed to parse agent_start payload: {e}"),
            }
        }
        FrameKind::AgentError => {
            match envelope.parse_payload::<AgentErrorPayload>() {
                Ok(p) => {
                    state.agents.update(|agents: &mut Vec<AgentInfo>| {
                        if let Some(agent) =
                            agents.iter_mut().find(|a| a.agent_id == p.agent_id)
                        {
                            agent.status = AgentStatus::Error(p.message);
                        }
                    });
                }
                Err(e) => log::error!("failed to parse agent_error payload: {e}"),
            }
        }
        FrameKind::ChatEvent => {
            dispatch_chat_event(state, &envelope.stream, &envelope);
        }
        FrameKind::SessionList => {
            match envelope.parse_payload::<SessionListPayload>() {
                Ok(p) => {
                    state.sessions.set(p.sessions);
                }
                Err(e) => log::error!("failed to parse session_list payload: {e}"),
            }
        }
        FrameKind::ProjectNotify => {
            match envelope.parse_payload::<ProjectNotifyPayload>() {
                Ok(ProjectNotifyPayload::Upsert { project }) => {
                    state.projects.update(|projects: &mut Vec<Project>| {
                        if let Some(existing) =
                            projects.iter_mut().find(|p| p.id == project.id)
                        {
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
            }
        }
        FrameKind::ProjectFileList => {
            let project_id = match resolve_project_id(&envelope.stream) {
                Some(id) => id,
                None => {
                    log::warn!("project_file_list on non-project stream {}", envelope.stream);
                    return;
                }
            };
            match envelope.parse_payload::<ProjectFileListPayload>() {
                Ok(p) => {
                    let entries: Vec<_> = p.roots.into_iter().flat_map(|r| r.entries).collect();
                    state.file_tree.update(|map: &mut HashMap<ProjectId, Vec<protocol::ProjectFileEntry>>| {
                        map.insert(project_id, entries);
                    });
                }
                Err(e) => log::error!("failed to parse project_file_list payload: {e}"),
            }
        }
        FrameKind::ProjectGitStatus => {
            let project_id = match resolve_project_id(&envelope.stream) {
                Some(id) => id,
                None => {
                    log::warn!("project_git_status on non-project stream {}", envelope.stream);
                    return;
                }
            };
            match envelope.parse_payload::<ProjectGitStatusPayload>() {
                Ok(p) => {
                    state.git_status.update(|map: &mut HashMap<ProjectId, Vec<protocol::ProjectRootGitStatus>>| {
                        map.insert(project_id, p.roots);
                    });
                }
                Err(e) => log::error!("failed to parse project_git_status payload: {e}"),
            }
        }
        FrameKind::ProjectGitDiff => {
            match envelope.parse_payload::<ProjectGitDiffPayload>() {
                Ok(p) => {
                    state.diff_content.set(Some(DiffViewState {
                        root: p.root,
                        scope: p.scope,
                        files: p.files,
                    }));
                }
                Err(e) => log::error!("failed to parse project_git_diff payload: {e}"),
            }
        }
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
                    state.terminals.update(|terms: &mut Vec<TerminalInfo>| terms.push(info));
                    // Auto-select first terminal
                    if state.active_terminal_id.get().is_none() {
                        let id = state.terminals.get().last().map(|t| t.terminal_id.clone());
                        state.active_terminal_id.set(id);
                    }
                }
                Err(e) => log::error!("failed to parse new_terminal payload: {e}"),
            }
        }
        FrameKind::TerminalStart => {
            match envelope.parse_payload::<TerminalStartPayload>() {
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
            }
        }
        FrameKind::TerminalOutput => {
            match envelope.parse_payload::<TerminalOutputPayload>() {
                Ok(p) => {
                    state.terminals.update(|terms: &mut Vec<TerminalInfo>| {
                        if let Some(t) = terms.iter_mut().find(|t| t.stream == envelope.stream) {
                            t.output_buffer.push_str(&p.data);
                        }
                    });
                }
                Err(e) => log::error!("failed to parse terminal_output payload: {e}"),
            }
        }
        FrameKind::TerminalExit => {
            match envelope.parse_payload::<TerminalExitPayload>() {
                Ok(p) => {
                    state.terminals.update(|terms: &mut Vec<TerminalInfo>| {
                        if let Some(t) = terms.iter_mut().find(|t| t.stream == envelope.stream) {
                            t.exited = true;
                            t.exit_code = p.exit_code;
                        }
                    });
                }
                Err(e) => log::error!("failed to parse terminal_exit payload: {e}"),
            }
        }
        FrameKind::TerminalError => {
            match envelope.parse_payload::<TerminalErrorPayload>() {
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
            }
        }
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
    let agents = state.agents.get();
    agents
        .iter()
        .find(|a| a.instance_stream == *stream)
        .map(|a| a.agent_id.clone())
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
            log::error!("failed to parse chat_event payload: {e}");
            return;
        }
    };

    match event {
        ChatEvent::MessageAdded(msg) => {
            let entry = ChatMessageEntry {
                message: msg,
                tool_requests: Vec::new(),
            };
            state.chat_messages.update(|map: &mut std::collections::HashMap<AgentId, Vec<ChatMessageEntry>>| {
                map.entry(agent_id).or_default().push(entry);
            });
        }

        ChatEvent::StreamStart(data) => {
            let ss = StreamingState {
                agent_name: data.agent,
                model: data.model,
                text: String::new(),
                reasoning: String::new(),
            };
            state.streaming_text.update(|map: &mut std::collections::HashMap<AgentId, StreamingState>| {
                map.insert(agent_id, ss);
            });
        }

        ChatEvent::StreamDelta(data) => {
            state.streaming_text.update(|map: &mut std::collections::HashMap<AgentId, StreamingState>| {
                if let Some(ss) = map.get_mut(&agent_id) {
                    ss.text.push_str(&data.text);
                }
            });
        }

        ChatEvent::StreamReasoningDelta(data) => {
            state.streaming_text.update(|map: &mut std::collections::HashMap<AgentId, StreamingState>| {
                if let Some(ss) = map.get_mut(&agent_id) {
                    ss.reasoning.push_str(&data.text);
                }
            });
        }

        ChatEvent::StreamEnd(data) => {
            state.streaming_text.update(|map: &mut std::collections::HashMap<AgentId, StreamingState>| {
                map.remove(&agent_id);
            });
            let entry = ChatMessageEntry {
                message: data.message,
                tool_requests: Vec::new(),
            };
            state.chat_messages.update(|map: &mut std::collections::HashMap<AgentId, Vec<ChatMessageEntry>>| {
                map.entry(agent_id).or_default().push(entry);
            });
        }

        ChatEvent::ToolRequest(req) => {
            let tool_entry = ToolRequestEntry {
                request: req,
                result: None,
            };
            state.chat_messages.update(|map: &mut std::collections::HashMap<AgentId, Vec<ChatMessageEntry>>| {
                if let Some(messages) = map.get_mut(&agent_id) {
                    // Attach to the last assistant message
                    if let Some(last) = messages.last_mut() {
                        last.tool_requests.push(tool_entry);
                    }
                }
            });
        }

        ChatEvent::ToolExecutionCompleted(data) => {
            let call_id = data.tool_call_id.clone();
            state.chat_messages.update(|map: &mut std::collections::HashMap<AgentId, Vec<ChatMessageEntry>>| {
                if let Some(messages) = map.get_mut(&agent_id) {
                    // Find the tool request by call id in any message
                    for msg in messages.iter_mut().rev() {
                        if let Some(tr) = msg.tool_requests.iter_mut().find(|t| t.request.tool_call_id == call_id) {
                            tr.result = Some(data);
                            return;
                        }
                    }
                }
            });
        }

        ChatEvent::TaskUpdate(task_list) => {
            state.task_lists.update(|map: &mut std::collections::HashMap<AgentId, protocol::TaskList>| {
                map.insert(agent_id, task_list);
            });
        }

        ChatEvent::OperationCancelled(data) => {
            state.streaming_text.update(|map: &mut std::collections::HashMap<AgentId, StreamingState>| {
                map.remove(&agent_id);
            });
            let event = TransientEvent::OperationCancelled { message: data.message };
            state.transient_events.update(|map: &mut std::collections::HashMap<AgentId, Vec<TransientEvent>>| {
                map.entry(agent_id).or_default().push(event);
            });
        }

        ChatEvent::RetryAttempt(data) => {
            let event = TransientEvent::RetryAttempt {
                attempt: data.attempt,
                max_retries: data.max_retries,
                error: data.error,
                backoff_ms: data.backoff_ms,
            };
            state.transient_events.update(|map: &mut std::collections::HashMap<AgentId, Vec<TransientEvent>>| {
                map.entry(agent_id).or_default().push(event);
            });
        }
    }
}
