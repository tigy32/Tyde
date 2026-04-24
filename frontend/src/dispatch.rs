use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::{GetUntracked, Set, Update, WithUntracked};
use wasm_bindgen_futures::spawn_local;

use protocol::{
    AgentClosedPayload, AgentErrorPayload, AgentId, AgentOrigin, AgentRenamedPayload,
    AgentStartPayload, BackendSetupPayload, ChatEvent, CommandErrorPayload,
    CustomAgentNotifyPayload, Envelope, FrameKind, HostBrowseEntriesPayload,
    HostBrowseErrorPayload, HostBrowseOpenedPayload, HostSettingsPayload, ListSessionsPayload,
    McpServerNotifyPayload, NewAgentPayload, NewTerminalPayload, ProjectFileContentsPayload,
    ProjectFileListPayload, ProjectGitDiffPayload, ProjectGitStatusPayload, ProjectId,
    ProjectNotifyPayload, ProtocolValidator, QueuedMessagesPayload, RejectPayload,
    SessionListPayload, SessionSchemasPayload, SessionSettingsPayload, SkillNotifyPayload,
    SteeringNotifyPayload, StreamPath, TerminalErrorPayload, TerminalExitPayload,
    TerminalOutputPayload, TerminalStartPayload,
};

use crate::send::send_frame;
use crate::state::{
    ActiveAgentRef, ActiveTerminalRef, AgentInfo, AppState, ChatMessageEntry, ConnectionStatus,
    OpenFile, ProjectInfo, SessionInfo, StreamingState, TabContent, TerminalInfo, ToolRequestEntry,
    TransientEvent, reduce_diff_response, root_display_name, sort_project_infos,
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

    fn validate(
        &mut self,
        host_id: &str,
        stream: &StreamPath,
        seq: u64,
        kind: FrameKind,
    ) -> Result<(), String> {
        let key = (host_id.to_string(), stream.clone());
        let expected = self.expected.get(&key).copied().unwrap_or(0);
        if seq != expected {
            return Err(format!(
                "sequence mismatch on host {host_id} stream {stream} kind {kind}: expected {expected}, got {seq}"
            ));
        }
        self.expected.insert(key, expected + 1);
        Ok(())
    }
}

thread_local! {
    static INBOUND_SEQ: RefCell<FrontendSeqValidator> = RefCell::new(FrontendSeqValidator::new());
    static INBOUND_PROTOCOL: RefCell<ProtocolValidator> = RefCell::new(ProtocolValidator::new());
}

fn report_dispatch_error(
    _state: &AppState,
    host_id: &str,
    stream: &StreamPath,
    kind: FrameKind,
    message: impl Into<String>,
) {
    let message = message.into();
    log::error!(
        "frontend dispatch error host={} stream={} kind={}: {}",
        host_id,
        stream,
        kind,
        message
    );
}

pub fn dispatch_envelope(state: &AppState, host_id: &str, envelope: Envelope) {
    if let Err(error) = INBOUND_SEQ.with(|validator| {
        validator
            .borrow_mut()
            .validate(host_id, &envelope.stream, envelope.seq, envelope.kind)
    }) {
        report_dispatch_error(state, host_id, &envelope.stream, envelope.kind, error);
        return;
    }
    if let Err(error) =
        INBOUND_PROTOCOL.with(|validator| validator.borrow_mut().validate_envelope(&envelope))
    {
        report_dispatch_error(
            state,
            host_id,
            &envelope.stream,
            envelope.kind,
            format!("protocol violation: {error}"),
        );
        return;
    }

    match envelope.kind {
        FrameKind::Welcome => {
            state.command_errors_by_host.update(|errors| {
                errors.remove(host_id);
            });
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
                report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse reject payload: {error}"),
                );
            }
        },
        FrameKind::CommandError => match envelope.parse_payload::<CommandErrorPayload>() {
            Ok(payload) => {
                let message = format!(
                    "{} failed on {}: {}",
                    payload.operation, payload.stream, payload.message
                );
                log::error!(
                    "command error host={} request_kind={} operation={} request_stream={} code={:?}: {}",
                    host_id,
                    payload.request_kind,
                    payload.operation,
                    payload.stream,
                    payload.code,
                    payload.message
                );
                state.command_errors_by_host.update(|errors| {
                    errors.insert(host_id.to_string(), message);
                });
            }
            Err(error) => {
                report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse command_error payload: {error}"),
                );
            }
        },
        FrameKind::HostSettings => match envelope.parse_payload::<HostSettingsPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch host_settings host={} enabled_backends={} default_backend={:?} debug_mcp={} agent_control_mcp={}",
                    host_id,
                    payload.settings.enabled_backends.len(),
                    payload.settings.default_backend,
                    payload.settings.tyde_debug_mcp_enabled,
                    payload.settings.tyde_agent_control_mcp_enabled
                );
                state.host_settings_by_host.update(|settings| {
                    settings.insert(host_id.to_string(), payload.settings);
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse host_settings payload: {error}"),
            ),
        },
        FrameKind::BackendSetup => match envelope.parse_payload::<BackendSetupPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch backend_setup host={} backends={}",
                    host_id,
                    payload.backends.len()
                );
                state.backend_setup_by_host.update(|setup| {
                    setup.insert(host_id.to_string(), payload.backends);
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse backend_setup payload: {error}"),
            ),
        },
        FrameKind::SessionSchemas => match envelope.parse_payload::<SessionSchemasPayload>() {
            Ok(payload) => {
                state.session_schemas.update(|schemas_by_host| {
                    let host_schemas = schemas_by_host.entry(host_id.to_string()).or_default();
                    host_schemas.clear();
                    for schema in payload.schemas {
                        host_schemas.insert(schema.backend_kind(), schema);
                    }
                });
                state.schemas_loaded_for_host.update(|loaded| {
                    loaded.insert(host_id.to_string(), true);
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse session_schemas payload: {error}"),
            ),
        },
        FrameKind::SessionSettings => {
            let Some(agent_id) = resolve_agent_id(state, host_id, &envelope.stream) else {
                log::warn!("session_settings on unknown stream {}", envelope.stream);
                return;
            };
            match envelope.parse_payload::<SessionSettingsPayload>() {
                Ok(payload) => {
                    state.agent_session_settings.update(|map| {
                        map.insert(agent_id, payload.values);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse session_settings payload: {error}"),
                ),
            }
        }
        FrameKind::QueuedMessages => {
            let Some(agent_id) = resolve_agent_id(state, host_id, &envelope.stream) else {
                log::warn!("queued_messages on unknown stream {}", envelope.stream);
                return;
            };
            match envelope.parse_payload::<QueuedMessagesPayload>() {
                Ok(payload) => {
                    log::info!(
                        "dispatch queued_messages host={} agent_id={} count={}",
                        host_id,
                        agent_id,
                        payload.messages.len()
                    );
                    state.agent_message_queue.update(|map| {
                        map.insert(agent_id, payload.messages);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse queued_messages payload: {error}"),
                ),
            }
        }
        FrameKind::NewAgent => match envelope.parse_payload::<NewAgentPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch new_agent host={} agent_id={} name={} backend={:?} instance_stream={}",
                    host_id,
                    payload.agent_id,
                    payload.name,
                    payload.backend_kind,
                    payload.instance_stream
                );
                let agent_id = payload.agent_id.clone();
                let origin = payload.origin;
                let info = AgentInfo {
                    host_id: host_id.to_string(),
                    agent_id: payload.agent_id,
                    name: payload.name,
                    origin,
                    backend_kind: payload.backend_kind,
                    workspace_roots: payload.workspace_roots,
                    project_id: payload.project_id,
                    parent_agent_id: payload.parent_agent_id,
                    custom_agent_id: payload.custom_agent_id,
                    created_at_ms: payload.created_at_ms,
                    instance_stream: payload.instance_stream,
                    started: false,
                    fatal_error: None,
                };
                let project_id = info.project_id.clone();
                // Only User-origin agents auto-open a chat tab and steal focus.
                // AgentControl and BackendNative agents appear in the sidebar
                // but must not disrupt the user's current view.
                let is_programmatic = !matches!(origin, AgentOrigin::User);
                state.agents.update(|agents| {
                    agents
                        .retain(|agent| !(agent.host_id == host_id && agent.agent_id == agent_id));
                    agents.push(info);
                });

                if is_programmatic {
                    return;
                }

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

                let agent_name = state
                    .agents
                    .with_untracked(|agents| {
                        agents
                            .iter()
                            .find(|a| {
                                a.host_id == host_id && a.agent_id == new_active_agent.agent_id
                            })
                            .map(|a| a.name.clone())
                    })
                    .unwrap_or_else(|| "Chat".to_string());

                if target_project == active_project {
                    state.active_agent.set(Some(new_active_agent.clone()));
                    // Upgrade a "New Chat" tab if one exists, otherwise open new
                    state.center_zone.update(|cz| {
                        let new_chat = TabContent::Chat { agent_ref: None };
                        if let Some(tab) = cz.tabs.iter_mut().find(|t| t.content == new_chat) {
                            tab.content = TabContent::Chat {
                                agent_ref: Some(new_active_agent.clone()),
                            };
                            tab.label = agent_name.clone();
                            cz.active_tab_id = Some(tab.id);
                        } else {
                            cz.open(
                                TabContent::Chat {
                                    agent_ref: Some(new_active_agent.clone()),
                                },
                                agent_name.clone(),
                                true,
                            );
                        }
                    });
                } else if let Some(target) = target_project {
                    // Spawned for a project the user isn't currently viewing.
                    // Stash into that project's memory so switching over shows it.
                    state.project_view_memory.update(|map| {
                        let slot = map.entry(target).or_default();
                        slot.active_agent = Some(new_active_agent.clone());
                        let cz = slot.center_zone.get_or_insert_with(Default::default);
                        let new_chat = TabContent::Chat { agent_ref: None };
                        if let Some(tab) = cz.tabs.iter_mut().find(|t| t.content == new_chat) {
                            tab.content = TabContent::Chat {
                                agent_ref: Some(new_active_agent),
                            };
                            tab.label = agent_name;
                            cz.active_tab_id = Some(tab.id);
                        } else {
                            cz.open(
                                TabContent::Chat {
                                    agent_ref: Some(new_active_agent),
                                },
                                agent_name,
                                true,
                            );
                        }
                    });
                } else {
                    // No project context — fall through to global behavior.
                    state.active_agent.set(Some(new_active_agent.clone()));
                    state.center_zone.update(|cz| {
                        let new_chat = TabContent::Chat { agent_ref: None };
                        if let Some(tab) = cz.tabs.iter_mut().find(|t| t.content == new_chat) {
                            tab.content = TabContent::Chat {
                                agent_ref: Some(new_active_agent.clone()),
                            };
                            tab.label = agent_name.clone();
                            cz.active_tab_id = Some(tab.id);
                        } else {
                            cz.open(
                                TabContent::Chat {
                                    agent_ref: Some(new_active_agent),
                                },
                                agent_name,
                                true,
                            );
                        }
                    });
                }
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse new_agent payload: {error}"),
            ),
        },
        FrameKind::AgentStart => match envelope.parse_payload::<AgentStartPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch agent_start host={} agent_id={} name={} backend={:?}",
                    host_id,
                    payload.agent_id,
                    payload.name,
                    payload.backend_kind
                );
                apply_agent_started(state, host_id, &payload.agent_id);
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse agent_start payload: {error}"),
            ),
        },
        FrameKind::AgentRenamed => match envelope.parse_payload::<AgentRenamedPayload>() {
            Ok(payload) => apply_agent_rename(state, host_id, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse agent_renamed payload: {error}"),
            ),
        },
        FrameKind::AgentClosed => match envelope.parse_payload::<AgentClosedPayload>() {
            Ok(payload) => apply_agent_closed(state, host_id, payload.agent_id),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse agent_closed payload: {error}"),
            ),
        },
        FrameKind::AgentError => match envelope.parse_payload::<AgentErrorPayload>() {
            Ok(payload) => {
                log::error!(
                    "dispatch agent_error host={} agent_id={} fatal={} code={:?} message={}",
                    host_id,
                    payload.agent_id,
                    payload.fatal,
                    payload.code,
                    payload.message
                );
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
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse agent_error payload: {error}"),
            ),
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
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse session_list payload: {error}"),
            ),
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
                    sort_project_infos(projects);
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
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse project_notify payload: {error}"),
            ),
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
                    state.file_tree.update(|file_tree| {
                        apply_project_file_list(file_tree, project_id, payload);
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_file_list payload: {error}"),
                ),
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
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_git_status payload: {error}"),
                ),
            }
        }
        FrameKind::ProjectGitDiff => match envelope.parse_payload::<ProjectGitDiffPayload>() {
            Ok(payload) => {
                let key = (payload.root.clone(), payload.scope);
                let current = state
                    .diff_contents
                    .with_untracked(|diffs| diffs.get(&key).cloned());
                match reduce_diff_response(current.as_ref(), payload) {
                    Some(next) => {
                        state.diff_contents.update(|diffs| {
                            diffs.insert(key, next);
                        });
                    }
                    None => {
                        log::debug!(
                            "ignoring stale/unmatched ProjectGitDiff payload for {:?}",
                            key,
                        );
                    }
                }
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse project_git_diff payload: {error}"),
            ),
        },
        FrameKind::ProjectFileContents => {
            match envelope.parse_payload::<ProjectFileContentsPayload>() {
                Ok(payload) => {
                    let path = payload.path.clone();
                    let base_label = path
                        .relative_path
                        .rsplit('/')
                        .next()
                        .unwrap_or(&path.relative_path)
                        .to_string();
                    let multi_root = state
                        .active_project_info_untracked()
                        .is_some_and(|project| project.project.roots.len() > 1);
                    let label = if multi_root {
                        format!("{base_label} · {}", root_display_name(&path.root))
                    } else {
                        base_label
                    };
                    state.open_files.update(|files| {
                        files.insert(
                            path.clone(),
                            OpenFile {
                                path: payload.path,
                                contents: payload.contents,
                                is_binary: payload.is_binary,
                            },
                        );
                    });
                    state.open_tab(TabContent::File { path }, label, true);
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse project_file_contents payload: {error}"),
                ),
            }
        }
        FrameKind::NewTerminal => match envelope.parse_payload::<NewTerminalPayload>() {
            Ok(payload) => {
                let info = TerminalInfo {
                    host_id: host_id.to_string(),
                    terminal_id: payload.terminal_id,
                    stream: payload.stream,
                    project_id: None,
                    root: None,
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
                let force_focus = state
                    .pending_terminal_focus
                    .with_untracked(|p| p.as_deref() == Some(host_id));
                if force_focus || state.active_terminal.get_untracked().is_none() {
                    state.active_terminal.set(Some(ActiveTerminalRef {
                        host_id: info.host_id,
                        terminal_id: info.terminal_id,
                    }));
                }
                if force_focus {
                    state.pending_terminal_focus.set(None);
                }
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse new_terminal payload: {error}"),
            ),
        },
        FrameKind::TerminalStart => match envelope.parse_payload::<TerminalStartPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch terminal_start host={} stream={} project_id={:?} cwd={} shell={}",
                    host_id,
                    envelope.stream,
                    payload.project_id,
                    payload.cwd,
                    payload.shell
                );
                state.terminals.update(|terminals| {
                    if let Some(terminal) = terminals.iter_mut().find(|terminal| {
                        terminal.host_id == host_id && terminal.stream == envelope.stream
                    }) {
                        terminal.project_id = payload.project_id;
                        terminal.root = payload.root;
                        terminal.cwd = payload.cwd;
                        terminal.shell = payload.shell;
                        terminal.cols = payload.cols;
                        terminal.rows = payload.rows;
                        terminal.created_at_ms = payload.created_at_ms;
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse terminal_start payload: {error}"),
            ),
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
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse terminal_output payload: {error}"),
            ),
        },
        FrameKind::TerminalExit => match envelope.parse_payload::<TerminalExitPayload>() {
            Ok(payload) => {
                log::info!(
                    "dispatch terminal_exit host={} stream={} exit_code={:?} signal={:?}",
                    host_id,
                    envelope.stream,
                    payload.exit_code,
                    payload.signal
                );
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
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse terminal_exit payload: {error}"),
            ),
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
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse terminal_error payload: {error}"),
            ),
        },
        FrameKind::HostBrowseOpened => match envelope.parse_payload::<HostBrowseOpenedPayload>() {
            Ok(payload) => dispatch_browse_opened(state, host_id, &envelope.stream, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse host_browse_opened payload: {error}"),
            ),
        },
        FrameKind::HostBrowseEntries => {
            match envelope.parse_payload::<HostBrowseEntriesPayload>() {
                Ok(payload) => dispatch_browse_entries(state, host_id, &envelope.stream, payload),
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse host_browse_entries payload: {error}"),
                ),
            }
        }
        FrameKind::HostBrowseError => match envelope.parse_payload::<HostBrowseErrorPayload>() {
            Ok(payload) => dispatch_browse_error(state, host_id, &envelope.stream, payload),
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse host_browse_error payload: {error}"),
            ),
        },
        FrameKind::CustomAgentNotify => {
            match envelope.parse_payload::<CustomAgentNotifyPayload>() {
                Ok(CustomAgentNotifyPayload::Upsert { custom_agent }) => {
                    state.custom_agents.update(|map| {
                        let host_map = map.entry(host_id.to_string()).or_default();
                        host_map.insert(custom_agent.id.clone(), custom_agent);
                    });
                }
                Ok(CustomAgentNotifyPayload::Delete { id }) => {
                    state.custom_agents.update(|map| {
                        if let Some(host_map) = map.get_mut(host_id) {
                            host_map.remove(&id);
                        }
                    });
                }
                Err(error) => report_dispatch_error(
                    state,
                    host_id,
                    &envelope.stream,
                    envelope.kind,
                    format!("failed to parse custom_agent_notify payload: {error}"),
                ),
            }
        }
        FrameKind::McpServerNotify => match envelope.parse_payload::<McpServerNotifyPayload>() {
            Ok(McpServerNotifyPayload::Upsert { mcp_server }) => {
                state.mcp_servers.update(|map| {
                    let host_map = map.entry(host_id.to_string()).or_default();
                    host_map.insert(mcp_server.id.clone(), mcp_server);
                });
            }
            Ok(McpServerNotifyPayload::Delete { id }) => {
                state.mcp_servers.update(|map| {
                    if let Some(host_map) = map.get_mut(host_id) {
                        host_map.remove(&id);
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse mcp_server_notify payload: {error}"),
            ),
        },
        FrameKind::SteeringNotify => match envelope.parse_payload::<SteeringNotifyPayload>() {
            Ok(SteeringNotifyPayload::Upsert { steering }) => {
                state.steering.update(|map| {
                    let host_map = map.entry(host_id.to_string()).or_default();
                    host_map.insert(steering.id.clone(), steering);
                });
            }
            Ok(SteeringNotifyPayload::Delete { id }) => {
                state.steering.update(|map| {
                    if let Some(host_map) = map.get_mut(host_id) {
                        host_map.remove(&id);
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse steering_notify payload: {error}"),
            ),
        },
        FrameKind::SkillNotify => match envelope.parse_payload::<SkillNotifyPayload>() {
            Ok(SkillNotifyPayload::Upsert { skill }) => {
                state.skills.update(|map| {
                    let host_map = map.entry(host_id.to_string()).or_default();
                    host_map.insert(skill.id.clone(), skill);
                });
            }
            Ok(SkillNotifyPayload::Delete { id }) => {
                state.skills.update(|map| {
                    if let Some(host_map) = map.get_mut(host_id) {
                        host_map.remove(&id);
                    }
                });
            }
            Err(error) => report_dispatch_error(
                state,
                host_id,
                &envelope.stream,
                envelope.kind,
                format!("failed to parse skill_notify payload: {error}"),
            ),
        },
        _ => {
            log::warn!("unexpected frame kind from server: {}", envelope.kind);
        }
    }
}

fn apply_project_file_list(
    file_tree: &mut HashMap<ProjectId, Vec<protocol::ProjectRootListing>>,
    project_id: ProjectId,
    payload: ProjectFileListPayload,
) {
    let existing_roots = file_tree.entry(project_id).or_default();
    for incoming_root in payload.roots {
        let root_index = existing_roots
            .iter()
            .position(|existing| existing.root == incoming_root.root)
            .unwrap_or_else(|| {
                existing_roots.push(protocol::ProjectRootListing {
                    root: incoming_root.root.clone(),
                    entries: Vec::new(),
                });
                existing_roots.len() - 1
            });
        let existing_root = &mut existing_roots[root_index];

        for entry in incoming_root.entries {
            match entry.op {
                protocol::FileEntryOp::Add => {
                    if !existing_root
                        .entries
                        .iter()
                        .any(|existing| existing.relative_path == entry.relative_path)
                    {
                        existing_root.entries.push(entry);
                    }
                }
                protocol::FileEntryOp::Remove => {
                    existing_root
                        .entries
                        .retain(|existing| existing.relative_path != entry.relative_path);
                }
            }
        }
        existing_root
            .entries
            .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
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

fn apply_agent_started(state: &AppState, host_id: &str, agent_id: &AgentId) {
    state.agents.update(|agents| {
        if let Some(agent) = agents
            .iter_mut()
            .find(|agent| agent.host_id == host_id && agent.agent_id == *agent_id)
        {
            agent.started = true;
        }
    });
}

fn apply_agent_rename(state: &AppState, host_id: &str, payload: AgentRenamedPayload) {
    let agent_id = payload.agent_id;
    let name = payload.name;

    state.agents.update(|agents| {
        if let Some(agent) = agents
            .iter_mut()
            .find(|agent| agent.host_id == host_id && agent.agent_id == agent_id)
        {
            agent.name = name.clone();
        }
    });

    state.streaming_text.update(|map| {
        if let Some(streaming) = map.get_mut(&agent_id) {
            streaming.agent_name = name.clone();
        }
    });

    state
        .center_zone
        .update(|cz| rename_agent_tabs(cz, host_id, &agent_id, &name));
    state.project_view_memory.update(|memories| {
        for memory in memories.values_mut() {
            if let Some(center_zone) = memory.center_zone.as_mut() {
                rename_agent_tabs(center_zone, host_id, &agent_id, &name);
            }
        }
    });
}

fn apply_agent_closed(state: &AppState, host_id: &str, agent_id: AgentId) {
    state.agents.update(|agents| {
        agents.retain(|agent| !(agent.host_id == host_id && agent.agent_id == agent_id));
    });
    state.chat_messages.update(|map| {
        map.remove(&agent_id);
    });
    state.streaming_text.update(|map| {
        map.remove(&agent_id);
    });
    state.agent_turn_active.update(|map| {
        map.remove(&agent_id);
    });
    state.transient_events.update(|map| {
        map.remove(&agent_id);
    });
    state.task_lists.update(|map| {
        map.remove(&agent_id);
    });
    state.agent_message_queue.update(|map| {
        map.remove(&agent_id);
    });
    state.agent_session_settings.update(|map| {
        map.remove(&agent_id);
    });

    let was_active = state.active_agent.with_untracked(|a| {
        a.as_ref()
            .is_some_and(|a| a.host_id == host_id && a.agent_id == agent_id)
    });
    if was_active {
        state.active_agent.set(None);
    }

    state
        .center_zone
        .update(|cz| close_agent_tabs(cz, host_id, &agent_id));
    state.project_view_memory.update(|memories| {
        for memory in memories.values_mut() {
            if let Some(center_zone) = memory.center_zone.as_mut() {
                close_agent_tabs(center_zone, host_id, &agent_id);
            }
            if memory
                .active_agent
                .as_ref()
                .is_some_and(|a| a.host_id == host_id && a.agent_id == agent_id)
            {
                memory.active_agent = None;
            }
        }
    });
}

fn close_agent_tabs(
    center_zone: &mut crate::state::CenterZoneState,
    host_id: &str,
    agent_id: &AgentId,
) {
    let remove_ids: Vec<_> = center_zone
        .tabs
        .iter()
        .filter(|tab| {
            matches!(
                &tab.content,
                TabContent::Chat { agent_ref: Some(ar) }
                    if ar.host_id == host_id && ar.agent_id == *agent_id
            )
        })
        .map(|tab| tab.id)
        .collect();
    for id in remove_ids {
        // Preserve non-closeable tabs (shouldn't exist for chats, but be safe).
        let closeable = center_zone
            .tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.closeable)
            .unwrap_or(true);
        if closeable {
            center_zone.close(id);
        }
    }
}

fn rename_agent_tabs(
    center_zone: &mut crate::state::CenterZoneState,
    host_id: &str,
    agent_id: &AgentId,
    name: &str,
) {
    for tab in &mut center_zone.tabs {
        let matches_agent = matches!(
            &tab.content,
            TabContent::Chat {
                agent_ref: Some(agent_ref)
            } if agent_ref.host_id == host_id && agent_ref.agent_id == *agent_id
        );
        if matches_agent {
            tab.label = name.to_string();
        }
    }
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
            log::info!(
                "dispatch chat_event host={} agent_id={} type=typing active={}",
                host_id,
                agent_id,
                typing
            );
            if typing {
                state.transient_events.update(|events| {
                    events.remove(&agent_id);
                });
            }
            state.agent_turn_active.update(|map| {
                if typing {
                    map.insert(agent_id.clone(), true);
                } else {
                    map.remove(&agent_id);
                }
            });
        }
        ChatEvent::MessageAdded(message) => {
            log::info!(
                "dispatch chat_event host={} agent_id={} type=message_added sender={:?} text_len={}",
                host_id,
                agent_id,
                message.sender,
                message.content.len()
            );
            let entry = ChatMessageEntry {
                message,
                tool_requests: Vec::new(),
            };
            state.chat_messages.update(|messages| {
                messages.entry(agent_id.clone()).or_default().push(entry);
            });
        }
        ChatEvent::StreamStart(data) => {
            log::info!(
                "dispatch chat_event host={} agent_id={} type=stream_start message_id={:?} model={:?}",
                host_id,
                agent_id,
                data.message_id,
                data.model
            );
            state.transient_events.update(|events| {
                events.remove(&agent_id);
            });
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
            log::info!(
                "dispatch chat_event host={} agent_id={} type=stream_delta message_id={:?} text_len={}",
                host_id,
                agent_id,
                data.message_id,
                data.text.len()
            );
            let streaming = state
                .streaming_text
                .with_untracked(|map| map.get(&agent_id).cloned());
            if let Some(streaming) = streaming {
                streaming.text.update(|text| text.push_str(&data.text));
            }
        }
        ChatEvent::StreamReasoningDelta(data) => {
            log::info!(
                "dispatch chat_event host={} agent_id={} type=reasoning_delta message_id={:?} text_len={}",
                host_id,
                agent_id,
                data.message_id,
                data.text.len()
            );
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
            log::info!(
                "dispatch chat_event host={} agent_id={} type=stream_end text_len={} tool_calls={}",
                host_id,
                agent_id,
                data.message.content.len(),
                data.message.tool_calls.len()
            );
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
            log::info!(
                "dispatch chat_event host={} agent_id={} type=tool_request tool_call_id={} tool_name={}",
                host_id,
                agent_id,
                request.tool_call_id,
                request.tool_name
            );
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
            log::info!(
                "dispatch chat_event host={} agent_id={} type=tool_execution_completed tool_call_id={} tool_name={} success={}",
                host_id,
                agent_id,
                data.tool_call_id,
                data.tool_name,
                data.success
            );
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
            log::info!(
                "dispatch chat_event host={} agent_id={} type=task_update items={}",
                host_id,
                agent_id,
                task_list.tasks.len()
            );
            state.task_lists.update(|task_lists| {
                task_lists.insert(agent_id.clone(), task_list);
            });
        }
        ChatEvent::OperationCancelled(data) => {
            log::warn!(
                "dispatch chat_event host={} agent_id={} type=operation_cancelled message={}",
                host_id,
                agent_id,
                data.message
            );
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
            log::warn!(
                "dispatch chat_event host={} agent_id={} type=retry attempt={} max_retries={} backoff_ms={} error={}",
                host_id,
                agent_id,
                data.attempt,
                data.max_retries,
                data.backoff_ms,
                data.error
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{FileEntryOp, ProjectFileEntry, ProjectFileKind, ProjectRootPath};

    fn file_entry(relative_path: &str, op: FileEntryOp) -> ProjectFileEntry {
        ProjectFileEntry {
            relative_path: relative_path.to_owned(),
            kind: ProjectFileKind::File,
            op,
        }
    }

    fn root_listing(root: &str, entries: Vec<ProjectFileEntry>) -> protocol::ProjectRootListing {
        protocol::ProjectRootListing {
            root: ProjectRootPath(root.to_owned()),
            entries,
        }
    }

    #[test]
    fn file_list_preserves_same_relative_path_in_different_roots() {
        let project_id = ProjectId("project".to_owned());
        let mut file_tree = HashMap::new();

        apply_project_file_list(
            &mut file_tree,
            project_id.clone(),
            ProjectFileListPayload {
                incremental: false,
                roots: vec![
                    root_listing(
                        "/repo/root-a",
                        vec![file_entry("same.txt", FileEntryOp::Add)],
                    ),
                    root_listing(
                        "/repo/root-b",
                        vec![file_entry("same.txt", FileEntryOp::Add)],
                    ),
                ],
            },
        );

        let roots = file_tree.get(&project_id).expect("project file tree");
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].root.0, "/repo/root-a");
        assert_eq!(roots[1].root.0, "/repo/root-b");
        assert_eq!(roots[0].entries[0].relative_path, "same.txt");
        assert_eq!(roots[1].entries[0].relative_path, "same.txt");
    }

    #[test]
    fn file_list_remove_is_scoped_to_root() {
        let project_id = ProjectId("project".to_owned());
        let mut file_tree = HashMap::new();

        apply_project_file_list(
            &mut file_tree,
            project_id.clone(),
            ProjectFileListPayload {
                incremental: false,
                roots: vec![
                    root_listing(
                        "/repo/root-a",
                        vec![file_entry("same.txt", FileEntryOp::Add)],
                    ),
                    root_listing(
                        "/repo/root-b",
                        vec![file_entry("same.txt", FileEntryOp::Add)],
                    ),
                ],
            },
        );
        apply_project_file_list(
            &mut file_tree,
            project_id.clone(),
            ProjectFileListPayload {
                incremental: true,
                roots: vec![root_listing(
                    "/repo/root-a",
                    vec![file_entry("same.txt", FileEntryOp::Remove)],
                )],
            },
        );

        let roots = file_tree.get(&project_id).expect("project file tree");
        let root_a = roots
            .iter()
            .find(|root| root.root.0 == "/repo/root-a")
            .expect("root-a");
        let root_b = roots
            .iter()
            .find(|root| root.root.0 == "/repo/root-b")
            .expect("root-b");
        assert!(root_a.entries.is_empty());
        assert_eq!(root_b.entries[0].relative_path, "same.txt");
    }
}
