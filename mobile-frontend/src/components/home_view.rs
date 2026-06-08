use crate::components::ui::{
    Button, ButtonSize, ButtonVariant, Card, EmptyState, Pill, PillTone, SafeArea, Skeleton,
    StatusDot, StatusTone,
};
use crate::state::{ActiveAgentRef, AgentInfo, AppState, ConnectionStatus, MobileTab};
use leptos::prelude::*;

/// Dashboard surface — the user's first impression once a host is
/// connected. Renders a compact hero, a 2×2 stat card grid, quick
/// actions, and an Active Agents list. When the host is disconnected
/// or no host is selected the surface degrades gracefully via
/// `EmptyState`-style messaging without losing the global affordances
/// (Pair another host).
#[component]
pub fn HomeView() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    // Phase C HIGH 5: every projection filters by `active_local_host_id`.
    // If no host is selected, counts are zero — Home never aggregates
    // across paired hosts.
    let s_agents = state.clone();
    let active_agent_count = move || {
        let Some(active) = s_agents.active_local_host_id.get() else {
            return 0;
        };
        s_agents
            .agents
            .get()
            .iter()
            .filter(|a| a.local_host_id == active && a.started && a.fatal_error.is_none())
            .count()
    };

    let s_projects = state.clone();
    let project_count = move || {
        let Some(active) = s_projects.active_local_host_id.get() else {
            return 0;
        };
        s_projects
            .projects
            .get()
            .iter()
            .filter(|p| p.local_host_id == active)
            .count()
    };

    let s_sessions = state.clone();
    let session_count = move || {
        let Some(active) = s_sessions.active_local_host_id.get() else {
            return 0;
        };
        s_sessions
            .sessions
            .get()
            .iter()
            .filter(|s| s.local_host_id == active)
            .count()
    };

    let s_host_label = state.clone();
    let host_label = move || -> String {
        let Some(active) = s_host_label.active_local_host_id.get() else {
            return "No host".to_owned();
        };
        s_host_label
            .paired_hosts
            .get()
            .into_iter()
            .find(|h| h.local_host_id == active)
            .map(|h| h.host_label)
            .unwrap_or_else(|| "Tyde".to_owned())
    };

    // Project the active host's connection state into a small tuple
    // used by the pill, dot, and the "Connection" stat card. A `Memo`
    // is `Copy`, so we can hand it to multiple closures without
    // worrying about consuming it.
    let s_status = state.clone();
    let connection_tone: Memo<(StatusTone, &'static str, PillTone, &'static str)> =
        Memo::new(move |_| match s_status.active_host_connection_status() {
            ConnectionStatus::Connected => {
                (StatusTone::Online, "Connected", PillTone::Success, "Online")
            }
            ConnectionStatus::Connecting => (
                StatusTone::Pending,
                "Connecting",
                PillTone::Warning,
                "Connecting",
            ),
            ConnectionStatus::Disconnected => {
                (StatusTone::Muted, "Offline", PillTone::Neutral, "Offline")
            }
            ConnectionStatus::Error(_) => (StatusTone::Error, "Error", PillTone::Error, "Error"),
        });

    let s_new_chat = state.clone();
    let on_new_chat = Callback::new(move |_: ()| {
        s_new_chat.active_agent.set(None);
        s_new_chat.chat_input.set(String::new());
        s_new_chat.viewing_chat.set(true);
    });

    let s_nav_agents = state.clone();
    let on_view_agents = Callback::new(move |_: ()| {
        s_nav_agents.active_tab.set(MobileTab::Agents);
    });
    let s_nav_sessions = state.clone();
    let on_view_sessions = Callback::new(move |_: ()| {
        s_nav_sessions.active_tab.set(MobileTab::Sessions);
    });
    let s_nav_projects = state.clone();
    let on_view_projects = Callback::new(move |_: ()| {
        s_nav_projects.active_tab.set(MobileTab::Projects);
    });

    let s_recent = state.clone();
    let recent_agents = move || -> Vec<AgentInfo> {
        let Some(active) = s_recent.active_local_host_id.get() else {
            return Vec::new();
        };
        s_recent
            .agents
            .get()
            .into_iter()
            .filter(|a| a.local_host_id == active && a.started && a.fatal_error.is_none())
            .take(5)
            .collect()
    };

    let s_has_host = state.clone();
    let has_active_host = move || s_has_host.active_local_host_id.get().is_some();

    // Skeleton hint shown when the user has selected a host but the
    // server's first `HostSettings` echo hasn't landed yet — keeps
    // the dashboard from popping into existence.
    let s_loading = state.clone();
    let initial_loading = move || {
        let active = s_loading.active_local_host_id.get();
        let Some(active) = active else { return false };
        s_loading
            .host_settings_by_host
            .with(|m| !m.contains_key(&active))
    };

    view! {
        <SafeArea inset_top=true inset_bottom=false data_mobile_test="home-safe-area">
        <div class="view home-view" data-mobile-test="home-view">
            <header class="home-hero">
                <p class="home-hero-greeting">"Welcome back"</p>
                <h1 class="home-hero-host">
                    {move || {
                        let (_, _, pill_tone, pill_label) = connection_tone.get();
                        view! {
                            <span>{host_label}</span>
                            <Pill
                                label=pill_label.to_string()
                                tone=pill_tone
                                data_mobile_test="home-connection-pill"
                            />
                        }
                    }}
                </h1>
                <div class="home-hero-status" data-mobile-test="home-hero-status">
                    {move || {
                        let (dot, label, _, _) = connection_tone.get();
                        view! {
                            <StatusDot
                                label=label.to_string()
                                tone=dot
                                data_mobile_test="home-status-dot"
                            />
                            <span class="home-hero-status-label">{label}</span>
                        }
                    }}
                </div>
            </header>

            {move || {
                if !has_active_host() {
                    view! {
                        <EmptyState
                            title="No host connected"
                            body="Pair a Tyde desktop to bring chats, projects, and sessions onto your phone."
                            icon="\u{1F517}"
                            data_mobile_test="home-empty-no-host"
                        />
                    }
                    .into_any()
                } else if initial_loading() {
                    // Server hasn't pushed HostSettings yet — show a
                    // skeleton so the dashboard doesn't pop in.
                    view! {
                        <div class="home-grid" data-mobile-test="home-loading">
                            <Skeleton width="100%".to_string() height="64px".to_string() rounded=false />
                            <Skeleton width="100%".to_string() height="64px".to_string() rounded=false />
                            <Skeleton width="100%".to_string() height="64px".to_string() rounded=false />
                            <Skeleton width="100%".to_string() height="64px".to_string() rounded=false />
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="home-grid" data-mobile-test="home-stats">
                            <Card data_mobile_test="home-stat-agents" dense=true>
                                <div class="home-stat-card">
                                    <div class="home-stat-value">{active_agent_count}</div>
                                    <p class="home-stat-label">"Active agents"</p>
                                </div>
                            </Card>
                            <Card data_mobile_test="home-stat-sessions" dense=true>
                                <div class="home-stat-card">
                                    <div class="home-stat-value">{session_count}</div>
                                    <p class="home-stat-label">"Sessions"</p>
                                </div>
                            </Card>
                            <Card data_mobile_test="home-stat-projects" dense=true>
                                <div class="home-stat-card">
                                    <div class="home-stat-value">{project_count}</div>
                                    <p class="home-stat-label">"Projects"</p>
                                </div>
                            </Card>
                            <Card data_mobile_test="home-stat-host" dense=true>
                                <div class="home-stat-card">
                                    <div class="home-stat-value" style="font-size: var(--text-lg);">
                                        {move || {
                                            let (_, label, _, _) = connection_tone.get();
                                            label.to_string()
                                        }}
                                    </div>
                                    <p class="home-stat-label">"Connection"</p>
                                </div>
                            </Card>
                        </div>

                        <div class="home-quick-actions">
                            <p class="home-quick-actions-title">"Quick actions"</p>
                            <Button
                                label="New chat"
                                variant=ButtonVariant::Primary
                                size=ButtonSize::Large
                                full_width=true
                                data_mobile_test="home-new-chat"
                                on_click=on_new_chat
                            />
                            <div style="display: grid; grid-template-columns: 1fr 1fr; gap: var(--space-2);">
                                <Button
                                    label="Agents"
                                    variant=ButtonVariant::Secondary
                                    full_width=true
                                    data_mobile_test="home-view-agents"
                                    on_click=on_view_agents
                                />
                                <Button
                                    label="Sessions"
                                    variant=ButtonVariant::Secondary
                                    full_width=true
                                    data_mobile_test="home-view-sessions"
                                    on_click=on_view_sessions
                                />
                            </div>
                            <Button
                                label="Projects"
                                variant=ButtonVariant::Ghost
                                full_width=true
                                data_mobile_test="home-view-projects"
                                on_click=on_view_projects
                            />
                        </div>
                    }
                    .into_any()
                }
            }}

            {move || {
                let agents = recent_agents();
                if agents.is_empty() || !has_active_host() {
                    None
                } else {
                    let s = state.clone();
                    Some(view! {
                        <div class="section-heading">
                            <span>"Active agents"</span>
                            <span class="section-heading-trailing">
                                <Pill
                                    label=format!("{}", agents.len())
                                    tone=PillTone::Accent
                                    data_mobile_test="home-active-count"
                                />
                            </span>
                        </div>
                        <div class="agent-list compact" data-mobile-test="home-recent-agents">
                            {agents.into_iter().map(|agent| {
                                let agent_id = agent.agent_id.clone();
                                let host_id = agent.local_host_id.clone();
                                let name = agent.name.clone();
                                let backend = format!("{:?}", agent.backend_kind);
                                let s_row = s.clone();
                                let on_click = Callback::new(move |_: ()| {
                                    s_row.active_agent.set(Some(ActiveAgentRef {
                                        local_host_id: host_id.clone(),
                                        agent_id: agent_id.clone(),
                                    }));
                                    s_row.viewing_chat.set(true);
                                });
                                view! {
                                    <Card
                                        data_mobile_test="home-recent-agent-row"
                                        interactive=true
                                        dense=true
                                        on_click=on_click
                                        aria_label=format!("Open chat with {name}")
                                    >
                                        <div class="list-row list-row-flush">
                                            <StatusDot
                                                label="Active".to_string()
                                                tone=StatusTone::Active
                                            />
                                            <div class="list-row-primary">
                                                <div class="list-row-title">{name}</div>
                                                <div class="list-row-subtitle">{backend}</div>
                                            </div>
                                            <span class="list-row-chevron" aria-hidden="true">"\u{203A}"</span>
                                        </div>
                                    </Card>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    })
                }
            }}
        </div>
        </SafeArea>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AgentInfo, AppState, ConnectionStatus, LocalHostId};
    use leptos::mount::mount_to;
    use protocol::{AgentId, AgentOrigin, BackendKind, StreamPath};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn fixture_agent(host: &LocalHostId, id: &str, name: &str) -> AgentInfo {
        AgentInfo {
            local_host_id: host.clone(),
            agent_id: AgentId(id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: StreamPath(format!("/agent/{id}")),
            started: true,
            fatal_error: None,
        }
    }

    /// Two hosts, disjoint agent sets. Home view recent-agent list
    /// reflects only the active host, and flipping `active_local_host_id`
    /// re-projects without remount.
    #[wasm_bindgen_test]
    async fn home_view_counts_and_recent_list_are_scoped_to_active_host() {
        let host_a = LocalHostId("host-a".to_owned());
        let host_b = LocalHostId("host-b".to_owned());

        let container = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_handle_for_mount = state_handle.clone();
        let host_a_for_mount = host_a.clone();
        let host_b_for_mount = host_b.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.agents.set(vec![
                fixture_agent(&host_a_for_mount, "a-1", "Agent Alpha"),
                fixture_agent(&host_a_for_mount, "a-2", "Agent Bravo"),
                fixture_agent(&host_b_for_mount, "b-1", "Agent Charlie"),
            ]);
            state.connection_statuses.update(|m| {
                m.insert(host_a_for_mount.clone(), ConnectionStatus::Connected);
                m.insert(host_b_for_mount.clone(), ConnectionStatus::Connected);
            });
            state
                .active_local_host_id
                .set(Some(host_a_for_mount.clone()));
            *state_handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <HomeView /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Agent Alpha") && text.contains("Agent Bravo"),
            "host-a agents should appear: {text}"
        );
        assert!(
            !text.contains("Agent Charlie"),
            "host-b agent must not leak into host-a's home view: {text}"
        );

        // Switch active host to host-b — list flips.
        let state = state_handle.borrow().as_ref().unwrap().clone();
        state.active_local_host_id.set(Some(host_b.clone()));
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Agent Charlie"),
            "host-b agent should appear after switching: {text}"
        );
        assert!(
            !text.contains("Agent Alpha") && !text.contains("Agent Bravo"),
            "host-a agents must disappear after switching: {text}"
        );
    }

    /// When no host is selected the dashboard renders the no-host empty
    /// state with a clear call to action.
    #[wasm_bindgen_test]
    async fn home_view_renders_empty_state_when_no_host_selected() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            // No active host.
            provide_context(state);
            view! { <HomeView /> }
        });
        next_tick().await;
        let empty = container
            .query_selector("[data-mobile-test='home-empty-no-host']")
            .unwrap();
        assert!(
            empty.is_some(),
            "no-host state must render the dedicated empty state"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.to_lowercase().contains("pair"),
            "empty-state copy must guide the user to pair a host: {text}"
        );
    }

    /// Connection status surfaces via both the status pill and the
    /// status dot so color isn't the only signal. Failed state must be
    /// reachable in either surface for screen readers.
    #[wasm_bindgen_test]
    async fn home_view_connection_status_appears_in_pill_and_dot() {
        let host = LocalHostId("host-x".to_owned());
        let container = make_container();
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.connection_statuses.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    ConnectionStatus::Error("transport".to_owned()),
                );
            });
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            provide_context(state);
            view! { <HomeView /> }
        });
        next_tick().await;
        let pill = container
            .query_selector("[data-mobile-test='home-connection-pill']")
            .unwrap()
            .expect("connection pill must render");
        let dot = container
            .query_selector("[data-mobile-test='home-status-dot']")
            .unwrap()
            .expect("status dot must render");
        assert_eq!(pill.text_content().unwrap_or_default().trim(), "Error");
        assert_eq!(
            dot.get_attribute("aria-label").as_deref(),
            Some("Error"),
            "status dot must carry an accessible label for screen readers"
        );
    }
}
