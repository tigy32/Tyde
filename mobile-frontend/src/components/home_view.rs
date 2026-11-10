use crate::components::ui::{
    Button, ButtonSize, ButtonVariant, Card, EmptyState, Pill, PillTone, Skeleton, StatusDot,
    StatusTone,
};
use crate::state::{
    ActiveAgentRef, ActiveProjectRef, AgentInfo, AppState, ConnectionStatus, MobileTab,
    sort_project_infos,
};
use leptos::prelude::*;

#[component]
pub fn HomeView() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

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
            ConnectionStatus::Bootstrapping => (
                StatusTone::Pending,
                "Loading host",
                PillTone::Warning,
                "Loading",
            ),
            ConnectionStatus::Disconnected => {
                (StatusTone::Muted, "Offline", PillTone::Neutral, "Offline")
            }
            ConnectionStatus::Error(_) => (StatusTone::Error, "Error", PillTone::Error, "Error"),
            ConnectionStatus::UpdateRequired { .. } => (
                StatusTone::Error,
                "Update required",
                PillTone::Error,
                "Update required",
            ),
            ConnectionStatus::NeedsAction { code, .. } => {
                if crate::state::needs_tyggs_sign_in(code) {
                    (
                        StatusTone::Error,
                        "Sign-in required",
                        PillTone::Error,
                        "Sign in",
                    )
                } else {
                    (
                        StatusTone::Error,
                        "Re-pair required",
                        PillTone::Error,
                        "Re-pair",
                    )
                }
            }
        });

    let on_new_chat = crate::components::new_chat_callback(&state);

    let native_heading_state = state.clone();
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

    // Skeleton hint shown while the host is connecting/connected but its
    // bootstrap snapshot hasn't landed yet — keeps the dashboard from popping
    // into existence. `host_snapshot_pending` returns false once the snapshot
    // arrives AND on a terminal connection state (error, disconnected, or a
    // sticky `UpdateRequired` protocol reject), so the skeleton never outlives
    // a connection that will never deliver data.
    let s_loading = state.clone();
    let initial_loading = move || s_loading.host_snapshot_pending();

    let board_state = state.clone();
    let project_state = state.clone();
    let showing_board = Memo::new(move |_| {
        board_state.board_project.get().filter(|target| {
            Some(target.local_host_id.clone()) == board_state.active_local_host_id.get()
        })
    });
    view! {
        <Show when=move || showing_board.get().is_some()>
            {move || showing_board.get().map(|target| view! { <super::project_board::ProjectBoard target=target /> })}
        </Show>
        <div hidden=move || showing_board.get().is_some() inert=move || showing_board.get().is_some().then_some("") class="view home-view" data-mobile-test="home-view">
            <div class="shell-flow">
                <div class="home-layout">
                <div class="home-primary">
                <header class="home-hero" hidden=move || crate::native::root_status_owned(&native_heading_state)
                    && native_heading_state.active_host_label().is_some()>
                    <h1 class="home-hero-host">
                        {move || {
                            let (_, _, pill_tone, pill_label) = connection_tone.get();
                            view! {
                                <span>{host_label}</span>
                                <Show when=|| !crate::native::is_native()><Pill
                                    label=pill_label.to_string()
                                    tone=pill_tone
                                    data_mobile_test="home-connection-pill"
                                /></Show>
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
                    let state = project_state.clone();
                    let host = state.active_local_host_id.get();
                    let mut projects = state.projects.get().into_iter().filter(|info|
                        Some(info.local_host_id.clone()) == host && info.project.parent_project_id().is_none()
                    ).collect::<Vec<_>>();
                    sort_project_infos(&mut projects);
                    (!projects.is_empty()).then(|| view! {
                        <section class="home-projects" aria-label="Projects">
                            <div class="section-heading">"Projects"</div>
                            <div class="home-project-list">
                                {projects.into_iter().map(|info| {
                                    let state = state.clone();
                                    view! { <button class="home-project-row" on:click=move |_| {
                                        state.active_tab.set(MobileTab::Home);
                                        state.board_project.set(Some(ActiveProjectRef {
                                            local_host_id: info.local_host_id.clone(), project_id: info.project.id.clone()
                                        }));
                                    }>{info.project.name}</button> }
                                }).collect_view()}
                            </div>
                        </section>
                    })
                }}
                {move || {
                    if !has_active_host() {
                        view! {
                            <EmptyState
                                title="No host connected"
                                body="Pair a Tyde desktop to bring agent chats and sessions onto your phone."
                                icon="\u{1F517}"
                                data_mobile_test="home-empty-no-host"
                            />
                        }
                        .into_any()
                    } else if initial_loading() {
                        view! {
                            <div class="home-loading" data-mobile-test="home-loading">
                                <Skeleton width="100%".to_string() height="52px".to_string() rounded=false />
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="home-new-chat">
                                <Button
                                    label="New chat"
                                    variant=ButtonVariant::Primary
                                    size=ButtonSize::Large
                                    full_width=true
                                    data_mobile_test="home-new-chat"
                                    on_click=on_new_chat
                                />
                            </div>
                        }
                        .into_any()
                    }
                }}

                </div>
                {move || {
                    let agents = recent_agents();
                    if agents.is_empty() || !has_active_host() {
                        None
                    } else {
                        let s = state.clone();
                        Some(view! {
                            <section class="home-recents" aria-label="Active agents">
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
                                        if crate::native::is_native() {
                                            s_row.active_tab.set(MobileTab::Agents);
                                        }
                                        s_row.open_agent_chat(ActiveAgentRef {
                                            local_host_id: host_id.clone(),
                                            agent_id: agent_id.clone(),
                                        });
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
                            </section>
                        })
                    }
                }}
                </div>
            </div>
        </div>
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
            review_recipient: false,
        }
    }

    /// Two hosts, disjoint agent sets. Home view recent-agent list
    /// reflects only the active host, and flipping `active_local_host_id`
    /// re-projects without remount.
    #[wasm_bindgen_test]
    async fn home_projects_and_recent_agents_are_scoped_to_active_host() {
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
            state.projects.set(
                [
                    (&host_a_for_mount, "Alpha project"),
                    (&host_b_for_mount, "Bravo project"),
                ]
                .into_iter()
                .map(|(host, name)| crate::state::ProjectInfo {
                    local_host_id: host.clone(),
                    project: protocol::Project {
                        id: protocol::ProjectId(name.into()),
                        name: name.into(),
                        sort_order: 0,
                        source: protocol::ProjectSource::Standalone { roots: vec![] },
                    },
                })
                .collect(),
            );
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
        assert_eq!(
            container
                .query_selector("[data-mobile-test='home-active-count']")
                .unwrap()
                .unwrap()
                .text_content()
                .as_deref(),
            Some("2")
        );
        assert!(
            text.contains("Alpha project") && !text.contains("Bravo project"),
            "Home lists only the active host's projects"
        );
        assert!(
            text.contains("Agent Alpha") && text.contains("Agent Bravo"),
            "host-a agents should appear: {text}"
        );
        assert!(
            !text.contains("Agent Charlie"),
            "host-b agent must not leak into host-a's home view: {text}"
        );

        // Switch active host to host-b — both lists flip.
        let state = state_handle.borrow().as_ref().unwrap().clone();
        state.active_local_host_id.set(Some(host_b.clone()));
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert_eq!(
            container
                .query_selector("[data-mobile-test='home-active-count']")
                .unwrap()
                .unwrap()
                .text_content()
                .as_deref(),
            Some("1")
        );
        assert!(
            text.contains("Bravo project") && !text.contains("Alpha project"),
            "Home projects re-project when switching hosts"
        );
        state.projects.set(vec![]);
        next_tick().await;
        assert!(
            container
                .query_selector(".home-projects")
                .unwrap()
                .is_none(),
            "Empty projects render nothing"
        );
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

    /// A sticky `UpdateRequired` protocol reject must NOT leave the dashboard
    /// stuck on the loading skeleton (the "spinning forever" bug). Instead the
    /// connection surfaces render an actionable "Update required" label.
    #[wasm_bindgen_test]
    async fn home_view_update_required_shows_actionable_state_not_skeleton() {
        let host = LocalHostId("host-update".to_owned());
        let container = make_container();
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            // No HostSettings ever arrives on a rejected handshake; before the
            // fix this alone forced the skeleton forever.
            state.connection_statuses.update(|m| {
                m.insert(
                    host_for_mount.clone(),
                    ConnectionStatus::UpdateRequired {
                        host_protocol: 31,
                        app_protocol: 30,
                        release_version: None,
                    },
                );
            });
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            provide_context(state);
            view! { <HomeView /> }
        });
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='home-loading']")
                .unwrap()
                .is_none(),
            "update-required host must not render the indefinite loading skeleton"
        );
        let pill = container
            .query_selector("[data-mobile-test='home-connection-pill']")
            .unwrap()
            .expect("connection pill must render");
        assert_eq!(
            pill.text_content().unwrap_or_default().trim(),
            "Update required"
        );
    }

    /// Connection status surfaces via both the status pill and the
    /// status dot so color isn't the only signal. Failed state must be
    /// reachable in either surface for screen readers.
    #[wasm_bindgen_test]
    async fn home_view_connection_status_appears_in_pill_and_dot() {
        crate::components::test_styles::ensure_styles_loaded();
        let host = LocalHostId("host-x".to_owned());
        let container = make_container();
        container.set_class_name("mobile-app");
        container
            .style()
            .set_css_text("width:386px;height:700px;position:relative");
        let state_handle = std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_for_mount = state_handle.clone();
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
            *state_for_mount.borrow_mut() = Some(state.clone());
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
        let state: AppState = state_handle.borrow().as_ref().unwrap().clone();
        state.connection_statuses.update(|statuses| {
            statuses.insert(host, ConnectionStatus::Connected);
        });
        next_tick().await;
        let dot = container
            .query_selector("[data-mobile-test='home-status-dot']")
            .unwrap()
            .unwrap();
        let label = dot.next_element_sibling().unwrap();
        assert_eq!(label.text_content().as_deref(), Some("Connected"));
        assert_eq!(
            dot.get_attribute("aria-label").as_deref(),
            Some("Connected")
        );
        let window = web_sys::window().unwrap();
        let style = |element: &web_sys::Element, property: &str| {
            window
                .get_computed_style(element)
                .unwrap()
                .unwrap()
                .get_property_value(property)
                .unwrap()
        };
        let browser_gap =
            label.get_bounding_client_rect().left() - dot.get_bounding_client_rect().right();
        let browser_font = style(&label, "font-size");
        let browser_dot = dot.get_bounding_client_rect();
        container.set_attribute("data-native-host", "").unwrap();
        for width in [320, 386, 669, 951] {
            container
                .style()
                .set_property("width", &format!("{width}px"))
                .unwrap();
            next_tick().await;
            let dot_bounds = dot.get_bounding_client_rect();
            let label_bounds = label.get_bounding_client_rect();
            let spacing = style(container.unchecked_ref(), "--space-2")
                .trim_end_matches("px")
                .trim()
                .parse::<f64>()
                .unwrap();
            assert!(
                label_bounds.left() - dot_bounds.right() >= spacing,
                "Native connection text must clear the status indicator and its halo at {width}px"
            );
            assert_eq!(
                style(&label, "font-size"),
                browser_font,
                "Native status spacing must retain the existing theme text size"
            );
            assert_eq!(
                (dot_bounds.width(), dot_bounds.height()),
                (browser_dot.width(), browser_dot.height()),
                "Native status spacing must retain the existing indicator size"
            );
        }
        container.style().set_property("width", "386px").unwrap();
        container.remove_attribute("data-native-host").unwrap();
        next_tick().await;
        assert_eq!(
            label.get_bounding_client_rect().left() - dot.get_bounding_client_rect().right(),
            browser_gap,
            "Native status spacing must not change browser/PWA layout"
        );
    }
}
