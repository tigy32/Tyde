use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    BackendKind, FrameKind, ListSessionsPayload, SessionSummary, SpawnAgentParams,
    SpawnAgentPayload,
};

use crate::send::send_frame;
use crate::state::{AppState, ConnectionStatus};

fn backend_class(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "backend-badge tycode",
        BackendKind::Kiro => "backend-badge kiro",
        BackendKind::Claude => "backend-badge claude",
        BackendKind::Codex => "backend-badge codex",
        BackendKind::Gemini => "backend-badge gemini",
    }
}

fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Gemini => "Gemini",
    }
}

fn format_date(ms: u64) -> String {
    let date = js_sys::Date::new_0();
    date.set_time(ms as f64);
    let month = date.get_month() + 1;
    let day = date.get_date();
    let year = date.get_full_year();
    let hours = date.get_hours();
    let mins = date.get_minutes();
    format!("{year}-{month:02}-{day:02} {hours:02}:{mins:02}")
}

fn last_path_component(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn session_title(s: &SessionSummary) -> String {
    if let Some(ref ua) = s.user_alias
        && !ua.is_empty()
    {
        return ua.clone();
    }
    if let Some(ref a) = s.alias
        && !a.is_empty()
    {
        return a.clone();
    }
    let id_str = s.id.0.clone();
    id_str.chars().take(50).collect()
}

fn session_id_short(s: &SessionSummary) -> String {
    s.id.0.chars().take(8).collect()
}

#[component]
pub fn SessionsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let search = RwSignal::new(String::new());
    let hide_child_sessions = RwSignal::new(false);

    let is_connected =
        Memo::new(move |_| state.connection_status.get() == ConnectionStatus::Connected);

    let filtered_sessions = Memo::new(move |_| {
        let sessions = state.sessions.get();
        let query = search.get().to_lowercase();
        let hide_children = hide_child_sessions.get();

        sessions
            .into_iter()
            .filter(|s| {
                if hide_children && s.parent_id.is_some() {
                    return false;
                }
                if !query.is_empty() {
                    let title = session_title(s).to_lowercase();
                    let workspace_match = s
                        .workspace_roots
                        .iter()
                        .any(|w| w.to_lowercase().contains(&query));
                    let backend_match = backend_label(s.backend_kind)
                        .to_lowercase()
                        .contains(&query);
                    if !title.contains(&query) && !workspace_match && !backend_match {
                        return false;
                    }
                }
                true
            })
            .collect::<Vec<_>>()
    });

    let on_search = move |ev: leptos::ev::Event| {
        let val = event_target_value(&ev);
        search.set(val);
    };

    let toggle_children = move |_| {
        hide_child_sessions.set(!hide_child_sessions.get());
    };

    let state_for_refresh = state.clone();
    let on_refresh = move |_| {
        let state = state_for_refresh.clone();
        spawn_local(async move {
            let host_id = state.host_id.get();
            let host_stream = state.host_stream.get();
            if let (Some(hid), Some(hs)) = (host_id, host_stream)
                && let Err(e) =
                    send_frame(&hid, hs, FrameKind::ListSessions, &ListSessionsPayload {}).await
            {
                log::error!("failed to send ListSessions: {e}");
            }
        });
    };

    view! {
        <div class="panel sessions-panel">
            <div class="panel-search">
                <input
                    type="text"
                    class="panel-search-input"
                    placeholder="Filter sessions..."
                    prop:value=search
                    on:input=on_search
                />
            </div>
            <div class="panel-filters">
                <button
                    class=move || if hide_child_sessions.get() { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_children
                >
                    "Hide agent sessions"
                </button>
                <button class="filter-toggle refresh-btn" on:click=on_refresh>
                    "Refresh"
                </button>
            </div>
            <div class="panel-content">
                {move || {
                    let sessions = filtered_sessions.get();
                    if sessions.is_empty() {
                        let msg = if search.get().is_empty() {
                            "No saved sessions"
                        } else {
                            "No matching sessions"
                        };
                        view! {
                            <div class="panel-empty">{msg}</div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="session-card-list">
                                {sessions.into_iter().map(|session| {
                                    session_card(state.clone(), session, is_connected)
                                }).collect_view()}
                            </div>
                        }.into_any()
                    }
                }}
            </div>
        </div>
    }
}

fn session_card(
    state: AppState,
    session: SessionSummary,
    is_connected: Memo<bool>,
) -> impl IntoView {
    let title = session_title(&session);
    let short_id = session_id_short(&session);
    let full_id = session.id.0.clone();
    let backend = session.backend_kind;
    let created = format_date(session.created_at_ms);
    let workspace = session
        .workspace_roots
        .first()
        .map(|w| last_path_component(w).to_string())
        .unwrap_or_default();
    let msg_count = session.message_count;
    let session_id = session.id.clone();
    let resumable = session.resumable;

    let on_click = move |_| {
        if !is_connected.get() || !resumable {
            return;
        }
        let state = state.clone();
        let sid = session_id.clone();
        spawn_local(async move {
            let host_id = state.host_id.get();
            let host_stream = state.host_stream.get();
            if let (Some(hid), Some(hs)) = (host_id, host_stream) {
                let payload = SpawnAgentPayload {
                    name: format!("resume-{}", sid),
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::Resume {
                        session_id: sid,
                        prompt: None,
                    },
                };
                if let Err(e) = send_frame(&hid, hs, FrameKind::SpawnAgent, &payload).await {
                    log::error!("failed to send SpawnAgent (resume): {e}");
                }
            }
        });
    };

    let disabled_class = move || {
        if !is_connected.get() || !resumable {
            "session-card disabled"
        } else {
            "session-card"
        }
    };

    view! {
        <button class=disabled_class title=full_id on:click=on_click>
            <div class="session-card-top">
                <span class="session-card-title">{title}</span>
                <span class={backend_class(backend)}>{backend_label(backend)}</span>
            </div>
            <div class="session-card-meta">
                <span class="session-card-date">{created}</span>
                {(!workspace.is_empty()).then(|| view! {
                    <span class="session-card-workspace">{workspace}</span>
                })}
                <span class="session-card-msgs">{format!("{msg_count} msgs")}</span>
            </div>
            <div class="session-card-id">{short_id}</div>
        </button>
    }
}
