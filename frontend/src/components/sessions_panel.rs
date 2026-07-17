use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{BackendKind, DeleteSessionPayload, FrameKind, ListSessionsPayload};

use crate::actions::resume_session;
use crate::send::send_frame;
use crate::state::{
    ActiveProjectRef, AppState, ConnectionStatus, SessionInfo, SessionsPanelFilters,
};

fn backend_class(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "backend-badge tycode",
        BackendKind::Kiro => "backend-badge kiro",
        BackendKind::Claude => "backend-badge claude",
        BackendKind::Codex => "backend-badge codex",
        BackendKind::Antigravity => "backend-badge antigravity",
        BackendKind::Hermes => "backend-badge hermes",
    }
}

fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Antigravity => "Antigravity",
        BackendKind::Hermes => "Hermes",
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

fn session_title(s: &SessionInfo) -> String {
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

fn session_id_short(s: &SessionInfo) -> String {
    s.summary.id.0.chars().take(8).collect()
}

/// Pure predicate used by the Sessions/History panel filter memo. Extracted
/// so the filter behavior can be unit-tested without a Leptos runtime.
pub fn session_passes_filters(
    session: &SessionInfo,
    filters: &SessionsPanelFilters,
    active_project: Option<&ActiveProjectRef>,
    lowercase_query: &str,
) -> bool {
    if !filters.show_child_sessions && session.summary.parent_id.is_some() {
        return false;
    }
    if !filters.show_other_projects {
        let matches = match active_project {
            None => session.summary.project_id.is_none(),
            Some(ap) => {
                session.host_id == ap.host_id
                    && session.summary.project_id.as_ref() == Some(&ap.project_id)
            }
        };
        if !matches {
            return false;
        }
    }
    if !lowercase_query.is_empty() {
        let title = session_title(session).to_lowercase();
        let workspace_match = session
            .summary
            .workspace_roots
            .iter()
            .any(|w| w.to_lowercase().contains(lowercase_query));
        let backend_match = backend_label(session.summary.backend_kind)
            .to_lowercase()
            .contains(lowercase_query);
        if !title.contains(lowercase_query) && !workspace_match && !backend_match {
            return false;
        }
    }
    true
}

#[component]
pub fn SessionsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let search = RwSignal::new(String::new());

    // Per-project filter values. Falls back to context-aware defaults when
    // the user hasn't toggled anything yet for this project.
    let filters_state = state.clone();
    let current_filters = Memo::new(move |_| {
        let active = filters_state.active_project.get();
        let overrides = filters_state.sessions_panel_filters.get();
        overrides
            .get(&active)
            .cloned()
            .unwrap_or_else(|| SessionsPanelFilters::defaults_for(active.as_ref()))
    });

    let update_filters = {
        let state = state.clone();
        move |mutate: Box<dyn FnOnce(&mut SessionsPanelFilters)>| {
            let active = state.active_project.get_untracked();
            state.sessions_panel_filters.update(|map| {
                let entry = map
                    .entry(active.clone())
                    .or_insert_with(|| SessionsPanelFilters::defaults_for(active.as_ref()));
                mutate(entry);
            });
        }
    };

    let filter_state = state.clone();
    let filtered_sessions = Memo::new(move |_| {
        let active_project = filter_state.active_project.get();
        let query = search.get().to_lowercase();
        let filters = current_filters.get();

        filter_state.sessions.with(|sessions| {
            sessions
                .iter()
                .filter(|s| session_passes_filters(s, &filters, active_project.as_ref(), &query))
                .cloned()
                .collect::<Vec<_>>()
        })
    });

    let on_search = move |ev: leptos::ev::Event| {
        let val = event_target_value(&ev);
        search.set(val);
    };

    let toggle_children = move |_| {
        update_filters(Box::new(|f: &mut SessionsPanelFilters| {
            f.show_child_sessions = !f.show_child_sessions;
        }));
    };

    let toggle_other_projects = move |_| {
        update_filters(Box::new(|f: &mut SessionsPanelFilters| {
            f.show_other_projects = !f.show_other_projects;
        }));
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
                    &ListSessionsPayload::default(),
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
                    class=move || if current_filters.get().show_child_sessions { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_children
                >
                    "Show sub-agents"
                </button>
                <button
                    class=move || if current_filters.get().show_other_projects { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_other_projects
                >
                    "Show all projects"
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

fn session_card(state: AppState, session: SessionInfo) -> impl IntoView {
    let title = session_title(&session);
    let short_id = session_id_short(&session);
    let full_id = session.summary.id.0.clone();
    let backend = session.summary.backend_kind;
    let last_active = format_date(session.summary.updated_at_ms);
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
    let session_project_id = session.summary.project_id.clone();

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

    // Reactive project name: resolve the session's `project_id` against
    // `state.projects` at render time so a rename (which updates
    // `state.projects` via `ProjectNotify`) immediately re-renders this
    // badge. Sessions without a project_id, or whose project is no longer
    // in `state.projects`, render no badge.
    let project_state = state.clone();
    let project_host_for_name = session_host_id.clone();
    let project_id_for_name = session_project_id.clone();
    let project_name = move || {
        let pid = project_id_for_name.as_ref()?;
        project_state.projects.with(|projects| {
            projects
                .iter()
                .find(|p| p.host_id == project_host_for_name && &p.project.id == pid)
                .map(|p| p.project.name.clone())
        })
    };

    // Clone before closures move session_id, session_host_id, and state.
    let delete_host_id = session_host_id.clone();
    let delete_session_id = session_id.clone();
    let state_for_delete = state.clone();

    // Shared resume action used by both click and keydown handlers.
    let resume_state = state.clone();
    let resume_sid = session_id.clone();
    let resume_host = session_host_id.clone();
    let resume_project_id = session_project_id.clone();
    let do_resume = std::rc::Rc::new(move || {
        resume_session(
            &resume_state,
            resume_host.clone(),
            backend,
            resume_sid.clone(),
            resume_project_id.clone(),
        );
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
                <span class="session-card-date">{last_active}</span>
                {move || project_name().map(|n| view! {
                    <span class="session-card-project">{n}</span>
                })}
                {(!workspace.is_empty()).then(|| view! {
                    <span class="session-card-workspace">{workspace}</span>
                })}
                <span class="session-card-msgs">{format!("{msg_count} msgs")}</span>
            </div>
            <div class="session-card-id">{short_id}</div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionInfo;
    use protocol::{BackendKind, ProjectId, SessionId, SessionSummary};

    fn mk_session(
        id: &str,
        host: &str,
        project_id: Option<&str>,
        parent: Option<&str>,
    ) -> SessionInfo {
        SessionInfo {
            host_id: host.to_string(),
            summary: SessionSummary {
                id: SessionId(id.to_string()),
                backend_kind: BackendKind::Tycode,
                launch_profile_id: None,
                workspace_roots: vec![],
                project_id: project_id.map(|s| ProjectId(s.to_string())),
                alias: None,
                user_alias: None,
                parent_id: parent.map(|p| SessionId(p.to_string())),
                created_at_ms: 0,
                updated_at_ms: 0,
                message_count: 0,
                token_count: None,
                resumable: true,
                compacted_from_session_id: None,
                compacted_to_session_id: None,
                compacted_at_ms: None,
                compaction_summary_preview: None,
            },
        }
    }

    fn active(host: &str, project: &str) -> ActiveProjectRef {
        ActiveProjectRef {
            host_id: host.to_string(),
            project_id: ProjectId(project.to_string()),
        }
    }

    #[test]
    fn defaults_for_home_shows_other_projects_true() {
        assert!(SessionsPanelFilters::defaults_for(None).show_other_projects);
    }

    #[test]
    fn defaults_for_specific_project_shows_other_projects_false() {
        let ap = active("h", "p");
        assert!(!SessionsPanelFilters::defaults_for(Some(&ap)).show_other_projects);
    }

    #[test]
    fn defaults_hide_child_sessions_by_default() {
        assert!(!SessionsPanelFilters::defaults_for(None).show_child_sessions);
        let ap = active("h", "p");
        assert!(!SessionsPanelFilters::defaults_for(Some(&ap)).show_child_sessions);
    }

    #[test]
    fn child_sessions_hidden_unless_toggled_on() {
        let filters = SessionsPanelFilters {
            show_child_sessions: false,
            show_other_projects: true,
        };
        let parent = mk_session("p", "h", Some("proj"), None);
        let child = mk_session("c", "h", Some("proj"), Some("p"));
        assert!(session_passes_filters(
            &parent,
            &filters,
            Some(&active("h", "proj")),
            ""
        ));
        assert!(!session_passes_filters(
            &child,
            &filters,
            Some(&active("h", "proj")),
            ""
        ));

        let allow_children = SessionsPanelFilters {
            show_child_sessions: true,
            show_other_projects: true,
        };
        assert!(session_passes_filters(
            &child,
            &allow_children,
            Some(&active("h", "proj")),
            ""
        ));
    }

    #[test]
    fn show_other_projects_off_on_home_keeps_only_none_project() {
        let filters = SessionsPanelFilters {
            show_child_sessions: false,
            show_other_projects: false,
        };
        let home_session = mk_session("home", "h", None, None);
        let project_session = mk_session("proj", "h", Some("p1"), None);
        assert!(session_passes_filters(&home_session, &filters, None, ""));
        assert!(!session_passes_filters(
            &project_session,
            &filters,
            None,
            ""
        ));
    }

    #[test]
    fn show_other_projects_off_in_project_requires_host_and_project_match() {
        let filters = SessionsPanelFilters::defaults_for(Some(&active("h1", "p1")));
        assert!(!filters.show_other_projects);

        let same = mk_session("same", "h1", Some("p1"), None);
        let other_project = mk_session("other_p", "h1", Some("p2"), None);
        let other_host = mk_session("other_h", "h2", Some("p1"), None);
        let home_session = mk_session("home", "h1", None, None);
        let active_ref = active("h1", "p1");
        assert!(session_passes_filters(
            &same,
            &filters,
            Some(&active_ref),
            ""
        ));
        assert!(!session_passes_filters(
            &other_project,
            &filters,
            Some(&active_ref),
            ""
        ));
        assert!(!session_passes_filters(
            &other_host,
            &filters,
            Some(&active_ref),
            ""
        ));
        assert!(!session_passes_filters(
            &home_session,
            &filters,
            Some(&active_ref),
            ""
        ));
    }

    #[test]
    fn show_other_projects_on_bypasses_project_check() {
        let filters = SessionsPanelFilters {
            show_child_sessions: false,
            show_other_projects: true,
        };
        let other_project = mk_session("other_p", "h1", Some("p2"), None);
        let other_host = mk_session("other_h", "h2", Some("p1"), None);
        let home_session = mk_session("home", "h1", None, None);
        let active_ref = active("h1", "p1");
        assert!(session_passes_filters(
            &other_project,
            &filters,
            Some(&active_ref),
            ""
        ));
        assert!(session_passes_filters(
            &other_host,
            &filters,
            Some(&active_ref),
            ""
        ));
        assert!(session_passes_filters(
            &home_session,
            &filters,
            Some(&active_ref),
            ""
        ));
    }

    #[test]
    fn search_matches_alias_workspace_and_backend_case_insensitively() {
        let filters = SessionsPanelFilters {
            show_child_sessions: false,
            show_other_projects: true,
        };
        let mut s = mk_session("id", "h", None, None);
        s.summary.user_alias = Some("My Cool Chat".to_string());
        s.summary.workspace_roots = vec!["/Users/me/Projects/foo".to_string()];
        s.summary.backend_kind = BackendKind::Claude;
        assert!(session_passes_filters(&s, &filters, None, "cool"));
        assert!(session_passes_filters(&s, &filters, None, "foo"));
        assert!(session_passes_filters(&s, &filters, None, "claude"));
        assert!(!session_passes_filters(&s, &filters, None, "nope"));
        // Empty query passes all (subject to other filters).
        assert!(session_passes_filters(&s, &filters, None, ""));
    }
}
