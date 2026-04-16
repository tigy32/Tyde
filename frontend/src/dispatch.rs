use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};
use wasm_bindgen_futures::spawn_local;

use protocol::{
    AgentErrorPayload, AgentId, AgentStartPayload, ChatEvent, Envelope, FrameKind,
    HostBrowseEntriesPayload, HostBrowseErrorPayload, HostBrowseOpenedPayload, HostSettingsPayload,
    ListSessionsPayload, NewAgentPayload, NewTerminalPayload, ProjectFileContentsPayload,
    ProjectFileListPayload, ProjectGitDiffPayload, ProjectGitStatusPayload, ProjectId,
    ProjectNotifyPayload, ProtocolValidator, RejectPayload, SessionListPayload, StreamPath,
    TerminalErrorPayload, TerminalExitPayload, TerminalOutputPayload, TerminalStartPayload,
};

use crate::send::send_frame;
use crate::state::{
    ActiveAgentRef, ActiveTerminalRef, AgentInfo, AppState, CenterView, ChatMessageEntry,
    ConnectionStatus, DiffViewState, OpenFile, ProjectInfo, SessionInfo, StreamingState,
    TerminalInfo, ToolRequestEntry, TransientEvent,
};

struct FrontendSeqValidator {
    expected: HashMap<(String, StreamPath), u64>,
}

impl FrontendSeqValidator {
    fn new() -> Self {
        Self {
            expected: HashMap::new(),
        }
    }

    fn validate(&mut self, host_id: &str, stream: &StreamPath, seq: u64, kind: FrameKind) -> bool {
        let key = (host_id.to_string(), stream.clone());
        let expected = self.expected.get(&key).copied().unwrap_or(0);
        if seq != expected {
            log::error!(
                "sequence mismatch on host {host_id} stream {stream} kind {kind}: expected {expected}, got {seq}"
            );
            return false;
        }
        self.expected.insert(key, expected + 1);
        true
    }
}

thread_local! {
    static INBOUND_SEQ: RefCell<FrontendSeqValidator> = RefCell::new(FrontendSeqValidator::new());
    static INBOUND_PROTOCOL: RefCell<ProtocolValidator> = RefCell::new(ProtocolValidator::new());
}

pub fn dispatch_envelope(state: &AppState, host_id: &str, envelope: Envelope) {
    INBOUND_SEQ.with(|validator| {
        validator
            .borrow_mut()
            .validate(host_id, &envelope.stream, envelope.seq, envelope.kind);
    });
    INBOUND_PROTOCOL.with(|validator| {
        if let Err(error) = validator.borrow_mut().validate_envelope(&envelope) {
            log::error!("protocol violation: {error}");
        }
    });

    match envelope.kind {
        FrameKind::Welcome => {
            state.connection_statuses.update(|statuses| {
                statuses.insert(host_id.to_string(), ConnectionStatus::Connected);
            });

            if let Some(stream) = state.host_stream_untracked(host_id) {
                let host_id = host_id.to_string();
                spawn_local(async move {
                    let _ = send_frame(
                        &host_id,
                        stream,
                        FrameKind::ListSessions,
                        &ListSessionsPayload {},
                    )
                    .await;
                });
            }

            log::info!("connected to host {}", host_id);
        }
        FrameKind::Reject => match envelope.parse_payload::<RejectPayload>() {
            Ok(payload) => {
                log::error!(
                    "connection rejected on host {}: {}",
                    host_id,
                    payload.message
                );
                state.connection_statuses.update(|statuses| {
                    statuses.insert(
                        host_id.to_string(),
                        ConnectionStatus::Error(payload.message),
                    );
                });
            }
            Err(error) => {
                log::error!("failed to parse reject payload: {error}");
                state.connection_statuses.update(|statuses| {
                    statuses.insert(
                        host_id.to_string(),
                        ConnectionStatus::Error("rejected".to_string()),
                    );
                });
            }
        },
        FrameKind::HostSettings => match envelope.parse_payload::<HostSettingsPayload>() {
            Ok(payload) => {
                state.host_settings_by_host.update(|settings| {
                    settings.insert(host_id.to_string(), payload.settings);
                });
            }
            Err(error) => log::error!("failed to parse host_settings payload: {error}"),
        },
        FrameKind::NewAgent => match envelope.parse_payload::<NewAgentPayload>() {
            Ok(payload) => {
                let agent_id = payload.agent_id.clone();
                let info = AgentInfo {
                    host_id: host_id.to_string(),
                    agent_id: payload.agent_id,
                    name: payload.name,
                    backend_kind: payload.backend_kind,
                    workspace_roots: payload.workspace_roots,
                    project_id: payload.project_id,
                    parent_agent_id: payload.parent_agent_id,
                    created_at_ms: payload.created_at_ms,
                    instance_stream: payload.instance_stream,
                    fatal_error: None,
                };
                let project_id = info.project_id.clone();
                state.agents.update(|agents| {
                    agents
                        .retain(|agent| !(agent.host_id == host_id && agent.agent_id == agent_id));
                    agents.push(info);
                });
                state.agent_initializing.set(false);

                let target_project =
                    project_id
                        .as_ref()
                        .map(|pid| crate::state::ActiveProjectRef {
                            host_id: host_id.to_string(),
                            project_id: pid.clone(),
                        });
                let active_project = state.active_project.get_untracked();
                let new_active_agent = ActiveAgentRef {
                    host_id: host_id.to_string(),
                    agent_id,
                };

                if target_project == active_project {
                    state.active_agent.set(Some(new_active_agent));
                    state.center_view.set(CenterView::Chat);
                } else if let Some(target) = target_project {
                    // Spawned for a project the user isn't currently viewing.
                    // Stash into that project's memory so switching over shows it.
                    state.project_view_memory.update(|map| {
                        let slot = map.entry(target).or_default();
                        slot.active_agent = Some(new_active_agent);
                        slot.center_view = Some(CenterView::Chat);
                    });
                } else {
                    // No project context — fall through to global behavior.
                    state.active_agent.set(Some(new_active_agent));
                    state.center_view.set(CenterView::Chat);
                }
            }
            Err(error) => log::error!("failed to parse new_agent payload: {error}"),
        },
        FrameKind::AgentStart => match envelope.parse_payload::<AgentStartPayload>() {
            Ok(_) => {}
            Err(error) => log::error!("failed to parse agent_start payload: {error}"),
        },
        FrameKind::AgentError => match envelope.parse_payload::<AgentErrorPayload>() {
            Ok(payload) => {
                let error_agent_id = payload.agent_id.clone();
                if payload.fatal {
                    state.agents.update(|agents| {
                        if let Some(agent) = agents.iter_mut().find(|agent| {
                            agent.host_id == host_id && agent.agent_id == payload.agent_id
                        }) {
                            agent.fatal_error = Some(payload.message.clone());
                        }
                    });
                }

                let entry = ChatMessageEntry {
                    message: protocol::ChatMessage {
                        timestamp: js_sys::Date::now() as u64,
                        sender: protocol::MessageSender::Error,
                        content: payload.message,
                        reasoning: None,
                        tool_calls: Vec::new(),
                        model_info: None,
                        token_usage: None,
                        context_breakdown: None,
                        images: None,
                    },
                    tool_requests: Vec::new(),
                };
                state.chat_messages.update(|map| {
                    map.entry(error_agent_id).or_default().push(entry);
                });
            }
            Err(error) => log::error!("failed to parse agent_error payload: {error}"),
        },
        FrameKind::ChatEvent => dispatch_chat_event(state, host_id, &envelope.stream, &envelope),
        FrameKind::SessionList => match envelope.parse_payload::<SessionListPayload>() {
            Ok(payload) => {
                state.sessions.update(|sessions| {
                    sessions.retain(|session| session.host_id != host_id);
                    sessions.extend(payload.sessions.into_iter().map(|summary| SessionInfo {
                        host_id: host_id.to_string(),
                        summary,
                    }));
                });
            }
            Err(error) => log::error!("failed to parse session_list payload: {error}"),
        },
        FrameKind::ProjectNotify => match envelope.parse_payload::<ProjectNotifyPayload>() {
            Ok(ProjectNotifyPayload::Upsert { project }) => {
                state.projects.update(|projects| {
                    if let Some(existing) = projects
                        .iter_mut()
                        .find(|entry| entry.host_id == host_id && entry.project.id == project.id)
                    {
                        existing.project = project;
                    } else {
                        projects.push(ProjectInfo {
                            host_id: host_id.to_string(),
                            project,
                        });
                    }
                });
            }
            Ok(ProjectNotifyPayload::Delete { project }) => {
                state.projects.update(|projects| {
                    projects.retain(|entry| {
                        !(entry.host_id == host_id && entry.project.id == project.id)
                    });
                });
                let deleted_ref = crate::state::ActiveProjectRef {
                    host_id: host_id.to_string(),
                    project_id: project.id.clone(),
                };
                state.forget_project_view_memory(&deleted_ref);
                if state
                    .active_project
                    .get_untracked()
                    .as_ref()
                    .is_some_and(|active| active == &deleted_ref)
                {
                    state.switch_active_project(None);
                }
            }
            Err(error) => log::error!("failed to parse project_notify payload: {error}"),
        },
        FrameKind::ProjectFileList => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "project_file_list on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<ProjectFileListPayload>() {
                Ok(payload) => {
                    let diff_entries: Vec<_> = payload
                        .roots
                        .into_iter()
                        .flat_map(|root| root.entries)
                        .collect();
                    state.file_tree.update(|file_tree| {
                        let existing = file_tree.entry(project_id.clone()).or_default();
                        for entry in diff_entries {
                            match entry.op {
                                protocol::FileEntryOp::Add => {
                                    if !existing.iter().any(|existing| {
                                        existing.relative_path == entry.relative_path
                                    }) {
                                        existing.push(entry);
                                    }
                                }
                                protocol::FileEntryOp::Remove => {
                                    existing.retain(|existing| {
                                        existing.relative_path != entry.relative_path
                                    });
                                }
                            }
                        }
                    });
                }
                Err(error) => log::error!("failed to parse project_file_list payload: {error}"),
            }
        }
        FrameKind::ProjectGitStatus => {
            let Some(project_id) = resolve_project_id(&envelope.stream) else {
                log::warn!(
                    "project_git_status on non-project stream {}",
                    envelope.stream
                );
                return;
            };
            match envelope.parse_payload::<ProjectGitStatusPayload>() {
                Ok(payload) => {
                    state.git_status.update(|git_status| {
                        git_status.insert(project_id, payload.roots);
                    });
                }
                Err(error) => log::error!("failed to parse project_git_status payload: {error}"),
            }
        }
        FrameKind::ProjectGitDiff => match envelope.parse_payload::<ProjectGitDiffPayload>() {
            Ok(payload) => {
                state.diff_content.set(Some(DiffViewState {
                    root: payload.root,
                    scope: payload.scope,
                    files: payload.files,
                }));
            }
            Err(error) => log::error!("failed to parse project_git_diff payload: {error}"),
        },
        FrameKind::ProjectFileContents => {
            match envelope.parse_payload::<ProjectFileContentsPayload>() {
                Ok(payload) => {
                    state.open_file.set(Some(OpenFile {
                        path: payload.path,
                        contents: payload.contents,
                        is_binary: payload.is_binary,
                    }));
                    state.center_view.set(CenterView::Editor);
                }
                Err(error) => log::error!("failed to parse project_file_contents payload: {error}"),
            }
        }
        FrameKind::NewTerminal => match envelope.parse_payload::<NewTerminalPayload>() {
            Ok(payload) => {
                let info = TerminalInfo {
                    host_id: host_id.to_string(),
                    terminal_id: payload.terminal_id,
                    stream: payload.stream,
                    project_id: None,
                    cwd: String::new(),
                    shell: String::new(),
                    cols: 80,
                    rows: 24,
                    created_at_ms: 0,
                    pending_output: Vec::new(),
                    widget_mounted: false,
                    exited: false,
                    exit_code: None,
                    exit_signal: None,
                };
                state
                    .terminals
                    .update(|terminals| terminals.push(info.clone()));
                if state.active_terminal.get_untracked().is_none() {
                    state.active_terminal.set(Some(ActiveTerminalRef {
                        host_id: info.host_id,
                        terminal_id: info.terminal_id,
                    }));
                }
            }
            Err(error) => log::error!("failed to parse new_terminal payload: {error}"),
        },
        FrameKind::TerminalStart => match envelope.parse_payload::<TerminalStartPayload>() {
            Ok(payload) => {
                state.terminals.update(|terminals| {
                    if let Some(terminal) = terminals.iter_mut().find(|terminal| {
                        terminal.host_id == host_id && terminal.stream == envelope.stream
                    }) {
                        terminal.project_id = payload.project_id;
                        terminal.cwd = payload.cwd;
                        terminal.shell = payload.shell;
                        terminal.cols = payload.cols;
                        terminal.rows = payload.rows;
                        terminal.created_at_ms = payload.created_at_ms;
                    }
                });
            }
            Err(error) => log::error!("failed to parse terminal_start payload: {error}"),
        },
        FrameKind::TerminalOutput => match envelope.parse_payload::<TerminalOutputPayload>() {
            Ok(payload) => {
                let mut write_tid: Option<String> = None;
                state.terminals.update(|terminals| {
                    if let Some(terminal) = terminals.iter_mut().find(|terminal| {
                        terminal.host_id == host_id && terminal.stream == envelope.stream
                    }) {
                        if terminal.widget_mounted {
                            write_tid = Some(terminal.terminal_id.0.clone());
                        } else {
                            terminal.pending_output.push(payload.data.clone());
                        }
                    }
                });
                if let Some(tid) = write_tid {
                    crate::term_bridge::write(&tid, &payload.data);
                }
            }
            Err(error) => log::error!("failed to parse terminal_output payload: {error}"),
        },
        FrameKind::TerminalExit => match envelope.parse_payload::<TerminalExitPayload>() {
            Ok(payload) => {
                state.terminals.update(|terminals| {
                    if let Some(terminal) = terminals.iter_mut().find(|terminal| {
                        terminal.host_id == host_id && terminal.stream == envelope.stream
                    }) {
                        terminal.exited = true;
                        terminal.exit_code = payload.exit_code;
                        terminal.exit_signal = payload.signal;
                    }
                });
            }
            Err(error) => log::error!("failed to parse terminal_exit payload: {error}"),
        },
        FrameKind::TerminalError => match envelope.parse_payload::<TerminalErrorPayload>() {
            Ok(payload) => {
                log::error!("terminal error ({:?}): {}", payload.code, payload.message);
                if payload.fatal {
                    state.terminals.update(|terminals| {
                        if let Some(terminal) = terminals.iter_mut().find(|terminal| {
                            terminal.host_id == host_id && terminal.stream == envelope.stream
                        }) {
                            terminal.exited = true;
                        }
                    });
                }
            }
            Err(error) => log::error!("failed to parse terminal_error payload: {error}"),
        },
        FrameKind::HostBrowseOpened => match envelope.parse_payload::<HostBrowseOpenedPayload>() {
            Ok(payload) => dispatch_browse_opened(state, host_id, &envelope.stream, payload),
            Err(error) => log::error!("failed to parse host_browse_opened payload: {error}"),
        },
        FrameKind::HostBrowseEntries => {
            match envelope.parse_payload::<HostBrowseEntriesPayload>() {
                Ok(payload) => dispatch_browse_entries(state, host_id, &envelope.stream, payload),
                Err(error) => log::error!("failed to parse host_browse_entries payload: {error}"),
            }
        }
        FrameKind::HostBrowseError => match envelope.parse_payload::<HostBrowseErrorPayload>() {
            Ok(payload) => dispatch_browse_error(state, host_id, &envelope.stream, payload),
            Err(error) => log::error!("failed to parse host_browse_error payload: {error}"),
        },
        _ => {
            log::warn!("unexpected frame kind from server: {}", envelope.kind);
        }
    }
}

fn active_browse_dialog(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
) -> Option<crate::state::BrowseDialogState> {
    state.browse_dialog.with_untracked(|dialog| {
        dialog
            .as_ref()
            .filter(|d| d.host_id == host_id && &d.browse_stream == stream)
            .cloned()
    })
}

fn dispatch_browse_opened(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    payload: HostBrowseOpenedPayload,
) {
    let Some(dialog) = active_browse_dialog(state, host_id, stream) else {
        log::warn!("host_browse_opened on inactive stream {stream}");
        return;
    };
    dialog.platform.set(Some(payload.platform));
    dialog.separator.set(payload.separator);
    dialog.home.set(Some(payload.home));
}

fn dispatch_browse_entries(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    payload: HostBrowseEntriesPayload,
) {
    let Some(dialog) = active_browse_dialog(state, host_id, stream) else {
        log::warn!("host_browse_entries on inactive stream {stream}");
        return;
    };
    dialog.error.set(None);
    dialog.current_path.set(Some(payload.path));
    dialog.parent.set(payload.parent);
    dialog.entries.set(payload.entries);
    dialog.loading.set(false);
}

fn dispatch_browse_error(
    state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    payload: HostBrowseErrorPayload,
) {
    let Some(dialog) = active_browse_dialog(state, host_id, stream) else {
        log::warn!("host_browse_error on inactive stream {stream}");
        return;
    };
    dialog.error.set(Some(payload));
    dialog.loading.set(false);
}

fn resolve_project_id(stream: &StreamPath) -> Option<ProjectId> {
    let suffix = stream.0.strip_prefix("/project/")?;
    if suffix.is_empty() {
        return None;
    }
    Some(ProjectId(suffix.to_string()))
}

fn resolve_agent_id(state: &AppState, host_id: &str, stream: &StreamPath) -> Option<AgentId> {
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|agent| agent.host_id == host_id && agent.instance_stream == *stream)
            .map(|agent| agent.agent_id.clone())
    })
}

fn dispatch_chat_event(state: &AppState, host_id: &str, stream: &StreamPath, envelope: &Envelope) {
    let Some(agent_id) = resolve_agent_id(state, host_id, stream) else {
        log::warn!("chat_event on unknown stream {stream}");
        return;
    };

    let event = match envelope.parse_payload::<ChatEvent>() {
        Ok(event) => event,
        Err(error) => {
            log::error!(
                "failed to parse chat_event payload: {error}\nraw: {}",
                serde_json::to_string(&envelope.payload).unwrap_or_default(),
            );
            return;
        }
    };

    match event {
        ChatEvent::TypingStatusChanged(typing) => {
            state.agent_turn_active.update(|map| {
                if typing {
                    map.insert(agent_id.clone(), true);
                } else {
                    map.remove(&agent_id);
                }
            });
        }
        ChatEvent::MessageAdded(message) => {
            let entry = ChatMessageEntry {
                message,
                tool_requests: Vec::new(),
            };
            state.chat_messages.update(|messages| {
                messages.entry(agent_id.clone()).or_default().push(entry);
            });
        }
        ChatEvent::StreamStart(data) => {
            let streaming = StreamingState {
                agent_name: data.agent,
                model: data.model,
                text: leptos::prelude::ArcRwSignal::new(String::new()),
                reasoning: leptos::prelude::ArcRwSignal::new(String::new()),
                tool_requests: leptos::prelude::ArcRwSignal::new(Vec::new()),
            };
            state.streaming_text.update(|map| {
                map.insert(agent_id.clone(), streaming);
            });
        }
        ChatEvent::StreamDelta(data) => {
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(streaming) = streaming {
                streaming.text.update(|text| text.push_str(&data.text));
            }
        }
        ChatEvent::StreamReasoningDelta(data) => {
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(streaming) = streaming {
                streaming
                    .reasoning
                    .update(|reasoning| reasoning.push_str(&data.text));
            }
        }
        ChatEvent::StreamEnd(data) => {
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            let tool_requests = streaming
                .as_ref()
                .map(|streaming| streaming.tool_requests.get_untracked())
                .unwrap_or_default();
            state.streaming_text.update(|map| {
                map.remove(&agent_id);
            });
            let has_renderable_content = !data.message.content.trim().is_empty()
                || data
                    .message
                    .reasoning
                    .as_ref()
                    .is_some_and(|reasoning| !reasoning.text.trim().is_empty())
                || !data.message.tool_calls.is_empty()
                || data
                    .message
                    .images
                    .as_ref()
                    .is_some_and(|images| !images.is_empty())
                || !tool_requests.is_empty();
            if !has_renderable_content {
                return;
            }
            let entry = ChatMessageEntry {
                message: data.message,
                tool_requests,
            };
            state.chat_messages.update(|messages| {
                messages.entry(agent_id.clone()).or_default().push(entry);
            });
        }
        ChatEvent::ToolRequest(request) => {
            let tool_name = request.tool_name.clone();
            let tool_call_id = request.tool_call_id.clone();
            let tool_entry = ToolRequestEntry {
                request,
                result: None,
            };
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(streaming) = streaming {
                streaming
                    .tool_requests
                    .update(|tools| tools.push(tool_entry));
                return;
            }
            state.chat_messages.update(|messages| {
                if let Some(agent_messages) = messages.get_mut(&agent_id) {
                    if let Some(last) = agent_messages.last_mut() {
                        last.tool_requests.push(tool_entry);
                    } else {
                        log::error!(
                            "TOOL REQUEST DROPPED: tool '{}' (call_id={}) for host {} agent {} — no messages exist yet",
                            tool_name, tool_call_id, host_id, agent_id
                        );
                    }
                } else {
                    log::error!(
                        "TOOL REQUEST DROPPED: tool '{}' (call_id={}) for host {} agent {} — agent has no message list",
                        tool_name, tool_call_id, host_id, agent_id
                    );
                }
            });
        }
        ChatEvent::ToolExecutionCompleted(data) => {
            let call_id = data.tool_call_id.clone();
            let tool_name = data.tool_name.clone();
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(streaming) = streaming {
                let mut matched = false;
                streaming.tool_requests.update(|tools| {
                    if let Some(tool) = tools
                        .iter_mut()
                        .find(|tool| tool.request.tool_call_id == call_id)
                    {
                        tool.result = Some(data.clone());
                        matched = true;
                    }
                });
                if matched {
                    return;
                }
            }
            state.chat_messages.update(|messages| {
                if let Some(agent_messages) = messages.get_mut(&agent_id) {
                    for message in agent_messages.iter_mut().rev() {
                        if let Some(tool) = message
                            .tool_requests
                            .iter_mut()
                            .find(|tool| tool.request.tool_call_id == call_id)
                        {
                            tool.result = Some(data);
                            return;
                        }
                    }
                    log::error!(
                        "TOOL RESULT ORPHANED: completion for tool '{}' (call_id={}) for host {} agent {} — no matching request found",
                        tool_name, call_id, host_id, agent_id
                    );
                } else {
                    log::error!(
                        "TOOL RESULT ORPHANED: completion for tool '{}' (call_id={}) for host {} agent {} — agent has no message list",
                        tool_name, call_id, host_id, agent_id
                    );
                }
            });
        }
        ChatEvent::TaskUpdate(task_list) => {
            state.task_lists.update(|task_lists| {
                task_lists.insert(agent_id.clone(), task_list);
            });
        }
        ChatEvent::OperationCancelled(data) => {
            state.streaming_text.update(|map| {
                map.remove(&agent_id);
            });
            state.transient_events.update(|events| {
                events.entry(agent_id.clone()).or_default().push(
                    TransientEvent::OperationCancelled {
                        message: data.message,
                    },
                );
            });
        }
        ChatEvent::RetryAttempt(data) => {
            state.transient_events.update(|events| {
                events
                    .entry(agent_id)
                    .or_default()
                    .push(TransientEvent::RetryAttempt {
                        attempt: data.attempt,
                        max_retries: data.max_retries,
                        error: data.error,
                        backoff_ms: data.backoff_ms,
                    });
            });
        }
    }
}
