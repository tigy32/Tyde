use std::cell::Cell;

use leptos::prelude::*;
use protocol::{
    AgentId, AgentOrigin, BackendKind, ChatMessage, ChatMessageId, MessageSender, SessionId,
    StreamPath,
};
use wasm_bindgen::JsValue;

use crate::bridge::{Accepted, LocalSubmissionId};
use crate::state::{
    ActiveAgentRef, AgentInfo, AppMode, AppState, ChatMessageEntry, ConnectionStatus, LocalHostId,
    MobileShellError, MobileTab,
};

const FIXTURE_QUERY_KEY: &str = "tyde-fixture";

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
        connection_instance_id: 1,
        local_submission_id,
    }
}

pub fn seed_state(state: &AppState) {
    let name = fixture_name();
    if name == "onboarding" {
        state.app_mode.set(AppMode::Onboarding);
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
    state.heartbeat_round_trip_ms_by_host.update(|round_trips| {
        round_trips.insert(host.clone(), 47);
    });
    state.agents.set(vec![AgentInfo {
        local_host_id: host.clone(),
        agent_id: agent_id.clone(),
        name: "Mira".to_owned(),
        origin: AgentOrigin::User,
        backend_kind: BackendKind::Codex,
        workspace_roots: vec!["/Users/mike/Tyggs/Tyde".to_owned()],
        project_id: None,
        parent_agent_id: None,
        session_id: Some(SessionId("fixture-session".to_owned())),
        custom_agent_id: None,
        created_at_ms: 1_721_000_000_000,
        instance_stream: stream,
        started: true,
        fatal_error: None,
    }]);
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
        messages.insert(agent_ref, fixture_messages());
    });

    match name.as_str() {
        "chat" | "chat-light" | "disconnected" | "error" => {
            state.viewing_chat.set(true);
        }
        _ => {
            state.active_tab.set(MobileTab::Home);
            state.viewing_chat.set(false);
        }
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
