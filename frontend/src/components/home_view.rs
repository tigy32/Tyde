use leptos::prelude::*;

use crate::actions::begin_new_chat_default;
use crate::components::host_browser::open_project_browser;
use crate::components::launch_menu::{LaunchMenuBody, SubmenuAlign};
use crate::state::{AppState, ConnectionStatus};

#[component]
pub fn HomeView() -> impl IntoView {
    let state = expect_context::<AppState>();

    let connected_state = state.clone();
    let connected = Memo::new(move |_| {
        matches!(
            connected_state.chat_context_connection_status(),
            ConnectionStatus::Connected
        )
    });

    // Setup progress for the getting-started guide. The guide is always on
    // screen — it doubles as orientation for returning users — and only the
    // step markers and CTAs react to progress.
    let backend_state = state.clone();
    let has_backend = Memo::new(move |_| {
        backend_state
            .host_settings_by_host
            .get()
            .values()
            .any(|settings| !settings.enabled_backends.is_empty())
    });
    let project_state = state.clone();
    let has_project = Memo::new(move |_| !project_state.projects.get().is_empty());
    let agents_state = state.clone();
    let has_agent = Memo::new(move |_| !agents_state.agents.get().is_empty());

    let open_backends_state = state.clone();
    let open_backend_settings = move |_| {
        open_backends_state
            .settings_tab_request
            .set(Some("Backends"));
        open_backends_state.settings_open.set(true);
    };

    let create_project_state = state.clone();
    let on_create_project = move |_| open_project_browser(&create_project_state);

    let manage_hosts_state = state.clone();
    let on_manage_hosts = move |_| {
        manage_hosts_state.settings_tab_request.set(Some("Hosts"));
        manage_hosts_state.settings_open.set(true);
    };

    let help_state = state.clone();
    let on_help = move |_| help_state.help_tour_step.set(Some(0));

    view! {
        <div class="home-view">
            <div class="home-hero">
                <img class="home-logo" src="icon.png" alt="Tyde" />
                <h1 class="home-title">"Tyde"</h1>
                <p class="home-tagline">"Coding Agent Studio"</p>
            </div>

            <div class="home-getstarted">
                <h2 class="home-getstarted-title">"Getting started"</h2>
                <p class="home-getstarted-lede">
                    "Tyde is a control center for AI coding agents. It runs the agent backends you already know — Claude, Codex, Antigravity and more — and keeps every session organized, so you can run many agents across many projects at once."
                </p>
                <ol class="home-getstarted-steps">
                    <li class="home-getstarted-step" class:done=move || has_backend.get()>
                        <span class="home-getstarted-marker">
                            {move || if has_backend.get() { "✓" } else { "1" }}
                        </span>
                        <div class="home-getstarted-body">
                            <div class="home-getstarted-step-title">"Connect an agent backend"</div>
                            <p class="home-getstarted-step-desc">
                                "Tyde brings no AI of its own — it runs external agent backends like Claude, Codex, and Antigravity. Pick one, install it, and sign in; backends already on your machine are enabled automatically."
                            </p>
                            <Show when=move || !has_backend.get()>
                                <button
                                    class="action-btn primary home-getstarted-cta"
                                    on:click=open_backend_settings
                                >
                                    "Connect an agent backend →"
                                </button>
                            </Show>
                        </div>
                    </li>
                    <li class="home-getstarted-step" class:done=move || has_project.get()>
                        <span class="home-getstarted-marker">
                            {move || if has_project.get() { "✓" } else { "2" }}
                        </span>
                        <div class="home-getstarted-body">
                            <div class="home-getstarted-step-title">"Create a project"</div>
                            <p class="home-getstarted-step-desc">
                                "A project is one or more folders an agent can read and edit — usually a codebase. Your projects live in the left sidebar; switch between them anytime."
                            </p>
                            <Show when=move || !has_project.get()>
                                <button
                                    class="action-btn primary home-getstarted-cta"
                                    on:click=on_create_project.clone()
                                    disabled=move || !connected.get()
                                    title=move || if connected.get() {
                                        "Pick a folder to create your first project"
                                    } else {
                                        "Connect to a host first to create a project"
                                    }
                                >
                                    "Choose a folder →"
                                </button>
                            </Show>
                        </div>
                    </li>
                    <li class="home-getstarted-step" class:done=move || has_agent.get()>
                        <span class="home-getstarted-marker">
                            {move || if has_agent.get() { "✓" } else { "3" }}
                        </span>
                        <div class="home-getstarted-body">
                            <div class="home-getstarted-step-title">"Run agents in it"</div>
                            <p class="home-getstarted-step-desc">
                                "Open a chat inside a project (New Chat, or ⌘N), describe a task, and an agent gets to work in that folder. Each project can run several agents at once — Tyde keeps every session organized so you can jump between them."
                            </p>
                        </div>
                    </li>
                </ol>
            </div>

            <div class="home-actions">
                <NewChatButton connected_sig=connected />
                <button class="action-btn" on:click=on_manage_hosts>
                    "Manage Hosts"
                </button>
                <button
                    class="action-btn"
                    title="Take a quick tour of the interface"
                    on:click=on_help
                >
                    "Help"
                </button>
            </div>
        </div>
    }
}

#[component]
fn NewChatButton(connected_sig: Memo<bool>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let open = RwSignal::new(false);

    let state_for_default = state.clone();
    let on_primary_click = move |_| {
        if !connected_sig.get() {
            return;
        }
        begin_new_chat_default(&state_for_default);
    };

    let on_toggle = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        open.update(|v| *v = !*v);
    };

    view! {
        <>
            <Show when=move || open.get()>
                <div
                    style="position:fixed;inset:0;z-index:99;"
                    on:click=move |_| open.set(false)
                />
            </Show>
            <div
                class="new-chat-button-wrap"
                style=move || {
                    let base = "position:relative;display:inline-flex;";
                    if open.get() { format!("{base}z-index:100;") } else { base.to_string() }
                }
            >
                <button
                    class="action-btn primary"
                    style="border-radius:6px 0 0 6px;"
                    on:click=on_primary_click
                    disabled=move || !connected_sig.get()
                >
                    "New Chat"
                </button>
                <button
                    class="action-btn primary"
                    style="padding:0 0.6rem;border-left:1px solid rgba(255,255,255,0.15);border-radius:0 6px 6px 0;"
                    on:click=on_toggle
                    disabled=move || !connected_sig.get()
                    title="Pick a launch profile and custom agent"
                >
                    "▾"
                </button>
                <Show when=move || open.get()>
                    <div
                        class="new-chat-flyout"
                        role="menu"
                        style="position:absolute;top:calc(100% + 4px);right:0;background:var(--bg-surface, #1e1e1e);border:1px solid var(--border-subtle, #333);border-radius:6px;padding:0.5rem 25px;z-index:100;box-shadow:0 4px 16px rgba(0,0,0,0.4);"
                    >
                        <LaunchMenuBody open_sig=open submenu_align=SubmenuAlign::Right />
                    </div>
                </Show>
            </div>
        </>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AgentInfo, ProjectInfo};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, BackendKind, Project, ProjectId, ProjectRootPath, ProjectSource,
        StreamPath,
    };
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

    /// Yield to the browser event loop so reactive effects flush and the DOM
    /// reflects the rendered view before we assert on it.
    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn visible_text(container: &HtmlElement) -> String {
        container.text_content().unwrap_or_default()
    }

    /// Find a rendered `<button>` by its visible label, ignoring markup
    /// structure so the test survives styling refactors.
    fn find_button_by_text(container: &HtmlElement, text: &str) -> Option<HtmlElement> {
        let nodes = container.query_selector_all("button").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|btn| btn.text_content().unwrap_or_default().contains(text))
    }

    fn enable_backend(state: &AppState, host_id: &str) {
        state.host_settings_by_host.update(|map| {
            map.insert(
                host_id.to_owned(),
                protocol::HostSettings {
                    enabled_backends: vec![BackendKind::Claude],
                    default_backend: Some(BackendKind::Claude),
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: false,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    supervisor: Default::default(),
                    code_intel: Default::default(),
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: Vec::new(),
                },
            );
        });
    }

    fn add_project(state: &AppState, host_id: &str) {
        state.projects.update(|projects| {
            projects.push(ProjectInfo {
                host_id: host_id.to_owned(),
                project: Project {
                    id: ProjectId("p-1".to_owned()),
                    name: "demo".to_owned(),
                    source: ProjectSource::Standalone {
                        roots: vec![ProjectRootPath("/tmp/demo".to_owned())],
                    },
                    sort_order: 0,
                },
            });
        });
    }

    fn add_agent(state: &AppState, host_id: &str) {
        state.agents.update(|agents| {
            agents.push(AgentInfo {
                host_id: host_id.to_owned(),
                agent_id: AgentId("a-1".to_owned()),
                name: "agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: Some(ProjectId("p-1".to_owned())),
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 1,
                instance_stream: StreamPath("/agent/a-1/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
            });
        });
    }

    fn checkmark_count(container: &HtmlElement) -> usize {
        visible_text(container).matches('✓').count()
    }

    /// A brand-new user (no backend enabled anywhere, no projects) must see the
    /// getting-started guide explaining what Tyde is, with all three setup
    /// steps pending and working calls-to-action: backend setup deep-links to
    /// Settings → Backends, and project creation offers a folder picker.
    #[wasm_bindgen_test]
    async fn fresh_install_shows_getting_started_with_ctas() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <HomeView /> }
        });
        next_tick().await;

        let text = visible_text(&container);
        assert!(
            text.contains("Getting started"),
            "fresh install must show the getting-started guide, got: {text}"
        );
        for step in [
            "Connect an agent backend",
            "Create a project",
            "Run agents in it",
        ] {
            assert!(text.contains(step), "missing setup step {step:?}: {text}");
        }
        assert_eq!(
            checkmark_count(&container),
            0,
            "no step is complete yet, so no checkmarks expected: {text}"
        );

        let folder_cta = find_button_by_text(&container, "Choose a folder →")
            .expect("step 2 must offer a folder picker button");
        assert!(
            folder_cta
                .dyn_ref::<web_sys::HtmlButtonElement>()
                .expect("button element")
                .disabled(),
            "folder CTA must be disabled while no host is connected"
        );

        let cta = find_button_by_text(&container, "Connect an agent backend →")
            .expect("step 1 must offer a backend-setup button");
        cta.click();
        next_tick().await;
        assert!(
            state.settings_open.get_untracked(),
            "backend CTA must open settings"
        );
        assert_eq!(
            state.settings_tab_request.get_untracked(),
            Some("Backends"),
            "backend CTA must deep-link to the Backends tab"
        );
    }

    /// The guide stays on screen permanently as orientation: each completed
    /// step flips to a checkmark and drops its CTA, and the guide is still
    /// visible once everything is set up.
    #[wasm_bindgen_test]
    async fn getting_started_tracks_progress_and_stays_visible() {
        let container = make_container();
        let state = AppState::new();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <HomeView /> }
        });
        next_tick().await;

        enable_backend(&state, "local");
        next_tick().await;
        assert_eq!(
            checkmark_count(&container),
            1,
            "backend step must show as completed"
        );
        assert!(
            find_button_by_text(&container, "Connect an agent backend →").is_none(),
            "backend CTA must disappear once a backend is enabled"
        );

        add_project(&state, "local");
        next_tick().await;
        assert_eq!(
            checkmark_count(&container),
            2,
            "project step must show as completed"
        );
        assert!(
            find_button_by_text(&container, "Choose a folder →").is_none(),
            "folder CTA must disappear once a project exists"
        );

        add_agent(&state, "local");
        next_tick().await;
        assert_eq!(
            checkmark_count(&container),
            3,
            "running an agent completes the last step"
        );

        let text = visible_text(&container);
        assert!(
            text.contains("Getting started"),
            "guide must stay visible after setup is complete: {text}"
        );

        let help =
            find_button_by_text(&container, "Help").expect("home screen must offer a Help button");
        help.click();
        next_tick().await;
        assert_eq!(
            state.help_tour_step.get_untracked(),
            Some(0),
            "Help button must start the guided tour at step 1"
        );
    }
}
