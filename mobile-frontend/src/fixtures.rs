use std::cell::Cell;

use leptos::prelude::*;
use protocol::{
    AgentId, AgentOrigin, BackendKind, ChatMessage, ChatMessageId, CustomAgent, CustomAgentId,
    MessageSender, Project, ProjectId, ProjectRootPath, ProjectSource, SessionId, StreamPath,
    ToolPolicy,
};
use settings_model::HostSettings;
use wasm_bindgen::JsValue;

use crate::bridge::{Accepted, LocalSubmissionId};
use crate::state::{
    ActiveAgentRef, AgentInfo, AppMode, AppState, ChatMessageEntry, ConnectionStatus, LocalHostId,
    MobileShellError, MobileTab, ProjectInfo,
};

const FIXTURE_QUERY_KEY: &str = "tyde-fixture";
const FIXTURE_CONNECTION_INSTANCE_ID: u64 = 1;

thread_local! {
    static NEXT_SUBMISSION_ID: Cell<u64> = const { Cell::new(1) };
}

pub fn fixture_name() -> String {
    web_sys::window()
        .and_then(|window| window.location().search().ok())
        .and_then(|search| {
            web_sys::UrlSearchParams::new_with_str(&search)
                .ok()
                .and_then(|params| params.get(FIXTURE_QUERY_KEY))
        })
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "home".to_owned())
}

pub fn is_requested() -> bool {
    web_sys::window()
        .and_then(|window| window.location().search().ok())
        .and_then(|search| web_sys::UrlSearchParams::new_with_str(&search).ok())
        .is_some_and(|params| params.has(FIXTURE_QUERY_KEY))
}

pub fn mark_ready() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let _ = js_sys::Reflect::set(
        window.as_ref(),
        &JsValue::from_str("__TYDE_FIXTURE_READY__"),
        &JsValue::TRUE,
    );
}

pub fn capture_send(line: &str) -> Accepted {
    if let Some(window) = web_sys::window() {
        let key = JsValue::from_str("__TYDE_FIXTURE_SENT_LINES__");
        let lines = js_sys::Reflect::get(window.as_ref(), &key)
            .ok()
            .filter(js_sys::Array::is_array)
            .map(|value| js_sys::Array::from(&value))
            .unwrap_or_default();
        lines.push(&JsValue::from_str(line));
        let _ = js_sys::Reflect::set(window.as_ref(), &key, lines.as_ref());
    }

    let local_submission_id = NEXT_SUBMISSION_ID.with(|next| {
        let id = next.get();
        next.set(id.saturating_add(1));
        LocalSubmissionId(id)
    });
    Accepted {
        connection_instance_id: FIXTURE_CONNECTION_INSTANCE_ID,
        local_submission_id,
    }
}

pub fn seed_state(state: &AppState) {
    let name = fixture_name();
    if name == "onboarding" {
        state.app_mode.set(AppMode::Onboarding);
        return;
    }
    if name == "pairing-paste" {
        state
            .app_mode
            .set(AppMode::Pairing(crate::state::PairingScreen::ManualPaste));
        return;
    }

    let host = LocalHostId("fixture-host".to_owned());
    let agent_id = AgentId("fixture-agent".to_owned());
    let agent_ref = crate::state::AgentRef {
        local_host_id: host.clone(),
        agent_id: agent_id.clone(),
    };
    let stream = StreamPath("/agent/fixture-agent/fixture".to_owned());

    state.app_mode.set(AppMode::Workspace);
    state.active_local_host_id.set(Some(host.clone()));
    state.host_streams.update(|streams| {
        streams.insert(host.clone(), StreamPath("/host/fixture".to_owned()));
    });
    state.bootstrapped_host_streams.update(|streams| {
        streams.insert(host.clone(), StreamPath("/host/fixture".to_owned()));
    });
    state.connection_statuses.update(|statuses| {
        statuses.insert(
            host.clone(),
            if name == "disconnected" {
                ConnectionStatus::Disconnected
            } else {
                ConnectionStatus::Connected
            },
        );
    });
    if name != "disconnected" {
        state.active_connection_instance_ids.update(|connections| {
            connections.insert(host.clone(), FIXTURE_CONNECTION_INSTANCE_ID);
        });
    }
    state.heartbeat_round_trip_ms_by_host.update(|round_trips| {
        round_trips.insert(host.clone(), 47);
    });
    state.host_settings_by_host.update(|settings| {
        let mut host_settings = HostSettings {
            workbenches: Default::default(),
            review: Default::default(),
            self_improvement: Default::default(),
            enabled_backends: vec![BackendKind::Codex, BackendKind::Claude],
            default_backend: Some(BackendKind::Codex),
            ..HostSettings::default()
        };
        if name == "voice" {
            host_settings.voice.enabled = true;
        }
        settings.insert(host.clone(), host_settings);
    });
    if name == "voice" {
        state.voice_capabilities_by_host.update(|capabilities| {
            capabilities.insert(
                host.clone(),
                protocol::VoiceCapabilitiesPayload::for_connection(true, false, false),
            );
        });
    }
    state.custom_agents_by_host.update(|agents| {
        agents.entry(host.clone()).or_default().insert(
            CustomAgentId("fixture-reviewer".to_owned()),
            CustomAgent {
                id: CustomAgentId("fixture-reviewer".to_owned()),
                name: "Code reviewer".to_owned(),
                description: "Review a change for correctness and regressions.".to_owned(),
                instructions: None,
                skill_ids: Vec::new(),
                mcp_server_ids: Vec::new(),
                tool_policy: ToolPolicy::Unrestricted,
            },
        );
    });
    // Two projects and a workbench under one of them: the agent list groups by
    // project and the new-chat sheet lists them, so a fixture with no projects
    // would leave both surfaces impossible to look at.
    state.projects.set(vec![
        ProjectInfo {
            local_host_id: host.clone(),
            project: Project {
                id: ProjectId("fixture-tyde".to_owned()),
                name: "Tyde".to_owned(),
                sort_order: 0,
                source: ProjectSource::Standalone {
                    roots: vec![ProjectRootPath("/Users/mike/Tyggs/Tyde".to_owned())],
                },
            },
        },
        ProjectInfo {
            local_host_id: host.clone(),
            project: Project {
                id: ProjectId("fixture-tychat".to_owned()),
                name: "Tychat".to_owned(),
                sort_order: 1,
                source: ProjectSource::Standalone {
                    roots: vec![ProjectRootPath("/Users/mike/Tyggs/Tychat".to_owned())],
                },
            },
        },
    ]);
    state.agents.set(vec![
        AgentInfo {
            local_host_id: host.clone(),
            agent_id: agent_id.clone(),
            name: "Mira".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Codex,
            workspace_roots: vec!["/Users/mike/Tyggs/Tyde".to_owned()],
            project_id: Some(ProjectId("fixture-tyde".to_owned())),
            parent_agent_id: None,
            session_id: Some(SessionId("fixture-session".to_owned())),
            custom_agent_id: None,
            created_at_ms: 1_721_000_000_000,
            instance_stream: stream,
            started: true,
            fatal_error: None,
            review_recipient: false,
        },
        AgentInfo {
            local_host_id: host.clone(),
            agent_id: AgentId("fixture-agent-tychat".to_owned()),
            name: "Read-state sync".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: vec!["/Users/mike/Tyggs/Tychat".to_owned()],
            project_id: Some(ProjectId("fixture-tychat".to_owned())),
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 1_721_000_100_000,
            instance_stream: StreamPath("/agent/fixture-agent-tychat/instance".to_owned()),
            started: true,
            fatal_error: None,
            review_recipient: false,
        },
        AgentInfo {
            local_host_id: host.clone(),
            agent_id: AgentId("fixture-agent-loose".to_owned()),
            name: "What does dvh do on iOS?".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Codex,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 1_721_000_200_000,
            instance_stream: StreamPath("/agent/fixture-agent-loose/instance".to_owned()),
            started: true,
            fatal_error: None,
            review_recipient: false,
        },
    ]);
    state.active_agent.set(Some(ActiveAgentRef {
        local_host_id: host.clone(),
        agent_id,
    }));
    state.agent_load_requests.update(|requests| {
        requests.insert(agent_ref.clone());
    });
    state.agent_loaded.update(|loaded| {
        loaded.insert(agent_ref.clone());
    });
    state.chat_messages.update(|messages| {
        messages.insert(
            agent_ref.clone(),
            if name == "images" {
                image_messages()
            } else {
                fixture_messages()
            },
        );
    });
    // Puts the header's context bar on screen. The fixtures are how this
    // chrome gets reviewed, and a bar that only ever appears against a live
    // agent never gets looked at.
    state.agent_activity_stats.update(|stats| {
        stats.insert(
            agent_ref,
            protocol::AgentActivityStats {
                current_context_usage: Some(protocol::CurrentContextUsage::Known {
                    input_tokens: 84_000,
                    context_window: 200_000,
                }),
                ..Default::default()
            },
        );
    });

    let event_state = state.clone();
    let event_host = host.clone();
    let envelope =
        wasm_bindgen::closure::Closure::<dyn Fn(String) -> bool>::new(move |line: String| {
            match serde_json::from_str::<protocol::Envelope>(&line) {
                Ok(envelope) => {
                    crate::dispatch::dispatch_envelope(&event_state, &event_host, envelope);
                    true
                }
                Err(_) => false,
            }
        });
    js_sys::Reflect::set(
        &js_sys::global(),
        &"__TYDE_FIXTURE_ENVELOPE__".into(),
        envelope.as_ref(),
    )
    .expect("fixture event hook");
    let envelope = send_wrapper::SendWrapper::new(envelope);
    on_cleanup(move || {
        js_sys::Reflect::delete_property(&js_sys::global(), &"__TYDE_FIXTURE_ENVELOPE__".into())
            .expect("remove fixture event hook");
        drop(envelope.take());
    });

    let navigation_state = state.clone();
    let navigation = wasm_bindgen::closure::Closure::<dyn Fn(bool)>::new(move |connected: bool| {
        navigation_state
            .active_local_host_id
            .set(connected.then(|| LocalHostId("fixture-host".to_owned())));
        navigation_state.viewing_chat.set(connected);
    });
    js_sys::Reflect::set(
        &js_sys::global(),
        &"__TYDE_FIXTURE_HOST__".into(),
        navigation.as_ref(),
    )
    .expect("fixture navigation hook");
    let navigation = send_wrapper::SendWrapper::new(navigation);
    on_cleanup(move || {
        js_sys::Reflect::delete_property(&js_sys::global(), &"__TYDE_FIXTURE_HOST__".into())
            .expect("remove fixture navigation hook");
        drop(navigation.take());
    });

    match name.as_str() {
        "new-chat" => {
            state.active_agent.set(None);
            state.viewing_chat.set(true);
        }
        "chat" | "chat-light" | "disconnected" | "error" | "voice" | "images" => {
            state.viewing_chat.set(true);
        }
        _ => {
            state.active_tab.set(MobileTab::Home);
            state.viewing_chat.set(false);
        }
    }
    if name == "images" {
        state
            .tool_output_mode
            .set(crate::state::ToolOutputMode::Summary);
    }
    if name == "chat-light" {
        state.theme.set("light".to_owned());
    }
    if name == "error" {
        state.mobile_shell_error.set(Some(MobileShellError {
            code: protocol::MobileAccessErrorCode::TransportFailed,
            message: "The fixture connection dropped before the last message was acknowledged."
                .to_owned(),
        }));
    }
}

fn fixture_messages() -> Vec<ChatMessageEntry> {
    vec![
        ChatMessageEntry {
            message: ChatMessage {
                origin: None,
                message_id: Some(ChatMessageId("fixture-user".to_owned())),
                timestamp: 1_721_000_000_000,
                sender: MessageSender::User,
                content: "Can you make the mobile composer easier to use?".to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
            tool_requests: Vec::new(),
        },
        ChatMessageEntry {
            message: ChatMessage {
                origin: None,
                message_id: Some(ChatMessageId("fixture-assistant".to_owned())),
                timestamp: 1_721_000_003_000,
                sender: MessageSender::Assistant {
                    agent: "Mira".to_owned(),
                },
                content: "I tightened the spacing and kept every action within a comfortable thumb reach. The composer now supports **photo attachments** too."
                    .to_owned(),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: Some(protocol::ModelInfo {
                    model: "codex".to_owned(),
                }),
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
            tool_requests: Vec::new(),
        },
    ]
}

fn image_messages() -> Vec<ChatMessageEntry> {
    let images = ["#ea7b54", "#78a7c5"].into_iter().map(|color| {
        let svg = format!(r##"<svg xmlns="http://www.w3.org/2000/svg" width="1400" height="1000" viewBox="0 0 1400 1000"><rect width="1400" height="1000" fill="{color}"/><circle cx="1050" cy="260" r="160" fill="#ffe7b0"/><path d="M0 800 400 300 850 850 1150 530 1400 800V1000H0Z" fill="#1c4053"/><path d="m0 920 500-350 600 430" fill="#427183"/></svg>"##);
        protocol::ImageData { media_type: "image/svg+xml".to_owned(), data: web_sys::window().unwrap().btoa(&svg).unwrap() }
    }).collect::<Vec<_>>();
    let mut messages = Vec::new();
    for index in 0..8 {
        for (role, mut entry) in fixture_messages().into_iter().enumerate() {
            entry.message.message_id = Some(ChatMessageId(format!(
                "fixture-image-history-{index}-{role}"
            )));
            messages.push(entry);
        }
    }
    let mut output = fixture_messages().remove(1);
    output.message.message_id = Some(ChatMessageId("fixture-generated-images".to_owned()));
    output.message.content = "Two landscape studies".to_owned();
    output.tool_requests.push(crate::state::ToolRequestEntry {
        tool_name: "generate_image".to_owned(),
        request: protocol::ToolRequest {
            tool_call_id: "fixture-image-output".to_owned(),
            tool_name: "generate_image".to_owned(),
            tool_type: protocol::ToolRequestType::GenerateImage {
                prompt: Some("Two landscape studies".to_owned()),
            },
        },
        result: Some(protocol::ToolExecutionCompletedData {
            tool_call_id: "fixture-image-output".to_owned(),
            outcome: protocol::ToolExecutionOutcome::Succeeded {
                result: protocol::ToolExecutionResult::NativeContent {
                    text: "Generated landscapes".to_owned(),
                    images,
                },
            },
        }),
    });
    messages.push(output);
    messages
}
