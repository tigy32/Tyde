use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    BackendKind, DeleteSessionPayload, FrameKind, ListSessionsPayload, SpawnAgentParams,
    SpawnAgentPayload,
};

use crate::send::send_frame;
use crate::state::{ActiveProjectRef, AppState, ConnectionStatus};

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

fn session_title(s: &crate::state::SessionInfo) -> String {
    if let Some(ref ua) = s.summary.user_alias
        && !ua.is_empty()
    {
        return ua.clone();
    }
    if let Some(ref a) = s.summary.alias
        && !a.is_empty()
    {
        return a.clone();
    }
    let id_str = s.summary.id.0.clone();
    id_str.chars().take(50).collect()
}

fn session_id_short(s: &crate::state::SessionInfo) -> String {
    s.summary.id.0.chars().take(8).collect()
}

#[component]
pub fn SessionsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let search = RwSignal::new(String::new());
    let show_child_sessions = RwSignal::new(false);
    let filtered_sessions = Memo::new(move |_| {
        let sessions = state.sessions.get();
        let query = search.get().to_lowercase();
        let show_children = show_child_sessions.get();

        sessions
            .into_iter()
            .filter(|s| {
                if !show_children && s.summary.parent_id.is_some() {
                    return false;
                }
                if !query.is_empty() {
                    let title = session_title(s).to_lowercase();
                    let workspace_match = s
                        .summary
                        .workspace_roots
                        .iter()
                        .any(|w| w.to_lowercase().contains(&query));
                    let backend_match = backend_label(s.summary.backend_kind)
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
        show_child_sessions.set(!show_child_sessions.get());
    };

    let state_for_refresh = state.clone();
    let on_refresh = move |_| {
        let state = state_for_refresh.clone();
        spawn_local(async move {
            if let Some((host_id, host_stream)) = state.selected_host_stream_untracked()
                && let Err(e) = send_frame(
                    &host_id,
                    host_stream,
                    FrameKind::ListSessions,
                    &ListSessionsPayload {},
                )
                .await
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
                    spellcheck="false"
                    {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                    autocapitalize="none"
                    autocomplete="off"
                />
            </div>
            <div class="panel-filters">
                <button
                    class=move || if show_child_sessions.get() { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_children
                >
                    "Show sub-agents"
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
                                    session_card(state.clone(), session)
                                }).collect_view()}
                            </div>
                        }.into_any()
                    }
                }}
            </div>
        </div>
    }
}

fn session_card(state: AppState, session: crate::state::SessionInfo) -> impl IntoView {
    let title = session_title(&session);
    let short_id = session_id_short(&session);
    let full_id = session.summary.id.0.clone();
    let backend = session.summary.backend_kind;
    let created = format_date(session.summary.created_at_ms);
    let workspace = session
        .summary
        .workspace_roots
        .first()
        .map(|w| last_path_component(w).to_string())
        .unwrap_or_default();
    let msg_count = session.summary.message_count;
    let session_id = session.summary.id.clone();
    let resumable = session.summary.resumable;
    let session_host_id = session.host_id.clone();

    // Per-row connection status keyed on this session's host, not the selected host.
    let host_id_for_connected = session_host_id.clone();
    let state_for_connected = state.clone();
    let is_connected = Memo::new(move |_| {
        state_for_connected
            .connection_statuses
            .get()
            .get(&host_id_for_connected)
            .is_some_and(|s| matches!(s, ConnectionStatus::Connected))
    });

    // Clone before closures move session_id, session_host_id, and state.
    let delete_host_id = session_host_id.clone();
    let delete_session_id = session_id.clone();
    let state_for_delete = state.clone();

    // Shared resume action used by both click and keydown handlers.
    let resume_state = state.clone();
    let resume_sid = session_id.clone();
    let resume_host = session_host_id.clone();
    let resume_project_id = session.summary.project_id.clone();
    let do_resume = std::rc::Rc::new(move || {
        let state = resume_state.clone();
        let sid = resume_sid.clone();
        let host_id = resume_host.clone();
        // Switch to the session's project synchronously so the NewAgent event
        // lands in the user's current view (upgrading the fresh "New Chat" tab
        // into the resumed chat). Sessions without a project_id drop the user
        // to the global/home view.
        let target = resume_project_id.clone().map(|pid| ActiveProjectRef {
            host_id: host_id.clone(),
            project_id: pid,
        });
        state.switch_active_project(target);
        spawn_local(async move {
            if let Some(host_stream) = state.host_stream_untracked(&host_id) {
                let payload = SpawnAgentPayload {
                    name: None,
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::Resume {
                        session_id: sid,
                        prompt: None,
                    },
                };
                if let Err(e) =
                    send_frame(&host_id, host_stream, FrameKind::SpawnAgent, &payload).await
                {
                    log::error!("failed to send SpawnAgent (resume): {e}");
                }
            }
        });
    });

    let do_resume2 = do_resume.clone();
    let on_click = move |_: web_sys::MouseEvent| {
        if !is_connected.get() || !resumable {
            return;
        }
        do_resume();
    };

    let on_keydown_card = move |ev: web_sys::KeyboardEvent| {
        if matches!(ev.key().as_str(), "Enter" | " ") {
            ev.prevent_default();
            if !is_connected.get() || !resumable {
                return;
            }
            do_resume2();
        }
    };

    let disabled_class = move || {
        if !is_connected.get() || !resumable {
            "session-card disabled"
        } else {
            "session-card"
        }
    };

    view! {
        <div
            class=disabled_class
            title=full_id
            tabindex="0"
            role="button"
            on:click=on_click
            on:keydown=on_keydown_card
        >
            <div class="session-card-top">
                <span class="session-card-title">{title}</span>
                <div>
                    {move || {
                        if !is_connected.get() {
                            return None;
                        }
                        // Create the handler fresh each time so the move closure
                        // doesn't exhaust its captured values across invocations.
                        let state = state_for_delete.clone();
                        let host_id = delete_host_id.clone();
                        let sid = delete_session_id.clone();
                        let on_delete = move |ev: web_sys::MouseEvent| {
                            ev.stop_propagation();
                            let state = state.clone();
                            let host_id = host_id.clone();
                            let sid = sid.clone();
                            spawn_local(async move {
                                if let Some(host_stream) = state.host_stream_untracked(&host_id)
                                    && let Err(e) = send_frame(
                                        &host_id,
                                        host_stream,
                                        FrameKind::DeleteSession,
                                        &DeleteSessionPayload { session_id: sid },
                                    )
                                    .await
                                {
                                    log::error!("failed to send DeleteSession: {e}");
                                }
                            });
                        };
                        Some(view! {
                            <button type="button" class="filter-toggle" on:click=on_delete>
                                "Delete"
                            </button>
                        })
                    }}
                    <span class={backend_class(backend)}>{backend_label(backend)}</span>
                </div>
            </div>
            <div class="session-card-meta">
                <span class="session-card-date">{created}</span>
                {(!workspace.is_empty()).then(|| view! {
                    <span class="session-card-workspace">{workspace}</span>
                })}
                <span class="session-card-msgs">{format!("{msg_count} msgs")}</span>
            </div>
            <div class="session-card-id">{short_id}</div>
        </div>
    }
}
