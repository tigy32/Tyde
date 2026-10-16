use leptos::portal::Portal;
use leptos::prelude::*;

use crate::actions::begin_new_chat_default;
use crate::components::host_browser::{
    open_project_browser, open_project_browser_on_selected_host,
};
use crate::components::launch_menu::LaunchChooser;
use crate::state::{AppState, ConnectionStatus};
use protocol::{BackendKind, BackendSetupAction, BackendSetupStatus, LaunchProfileEntry};
use wasm_bindgen::JsCast;

use crate::components::launch_menu::backend_icon;
use crate::components::settings_panel::{
    BackendCard, backend_label, backend_setup_status_label, send_run_backend_setup,
};
use crate::state::ActiveProjectRef;

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
    let connection_problem_state = state.clone();
    let connection_problem = Memo::new(move |_| {
        connection_problem_state.selected_host_id.get()?;
        match connection_problem_state.selected_host_connection_status() {
            ConnectionStatus::Error(message) => Some(format!(
                "Tyde could not connect to the selected host: {message}"
            )),
            ConnectionStatus::Disconnected => Some(
                "The selected host is offline. Connect it before creating projects or starting chats."
                    .to_owned(),
            ),
            ConnectionStatus::Reconnecting { .. } => Some("Disconnected — reconnecting. Your workspace is preserved.".into()),
            ConnectionStatus::Connecting | ConnectionStatus::Connected => None,
        }
    });

    // Setup progress for the getting-started guide. The guide is always on
    // screen — it doubles as orientation for returning users — and only the
    // step markers and CTAs react to progress.
    let backend_state = state.clone();
    let has_backend = Memo::new(move |_| {
        let Some(host) = backend_state.selected_host_id.get() else {
            return false;
        };
        let settings = backend_state.host_settings_by_host.get();
        let setup = backend_state.backend_setup_by_host.get();
        settings.get(&host).is_some_and(|settings| {
            settings.enabled_backends.iter().any(|enabled| {
                setup.get(&host).is_some_and(|infos| {
                    infos.iter().any(|info| {
                        info.backend_kind == *enabled
                            && info.status == BackendSetupStatus::Installed
                    })
                })
            })
        })
    });
    let backend_problem_state = state.clone();
    let backend_problem = Memo::new(move |_| {
        let host_id = backend_problem_state.selected_host_id.get()?;
        let settings = backend_problem_state
            .host_settings_by_host
            .get()
            .get(&host_id)
            .cloned()?;
        let setup = backend_problem_state.backend_setup_by_host.get();
        let infos = setup.get(&host_id)?;
        settings.enabled_backends.iter().find_map(|enabled| {
            let info = infos
                .iter()
                .find(|info| info.backend_kind == *enabled)?;
            match info.status {
                BackendSetupStatus::Installed => None,
                BackendSetupStatus::NotInstalled => Some(format!(
                    "{} is enabled but was not found on this host. Open Backend settings to install or repair it.",
                    backend_label(*enabled)
                )),
                BackendSetupStatus::Unavailable => Some(
                    info.diagnostic
                        .as_ref()
                        .map(|diagnostic| diagnostic.message.clone())
                        .unwrap_or_else(|| {
                            format!(
                                "{} is installed but unavailable. Open Backend settings for repair steps.",
                                backend_label(*enabled)
                            )
                        }),
                ),
                BackendSetupStatus::Unsupported => Some(format!(
                    "{} is enabled but unsupported on this host.",
                    backend_label(*enabled)
                )),
            }
        })
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

    let guide_state = state.clone();
    let on_getting_started = move |_| guide_state.getting_started_open.set(true);
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
                <h2 class="home-getstarted-title">"Your first agent, one step at a time"</h2>
                <button class="action-btn primary home-getstarted-cta" on:click=on_getting_started>
                    "Getting started"
                </button>
                <Show when=move || connection_problem.get().is_some()>
                    <div class="home-connection-error" role="alert">
                        {move || connection_problem.get().unwrap_or_default()}
                    </div>
                </Show>
                <Show when=move || backend_problem.get().is_some()>
                    <div class="home-backend-error" role="alert">
                        {move || backend_problem.get().unwrap_or_default()}
                    </div>
                </Show>
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
                                "Tyde brings no AI of its own — it runs external agent backends like Claude, Codex, and Antigravity. Pick one, install it, and sign in through its own CLI. Tyde checks installation, not your sign-in status."
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
            <div class="new-chat-button-wrap" style="display:inline-flex;">
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
                    title="Choose backend and agent role"
                    aria-label="Choose backend and agent role"
                    aria-haspopup="dialog"
                    aria-expanded=move || open.get().to_string()
                >
                    "▾"
                </button>
                <Show when=move || open.get()>
                    <Portal><LaunchChooser open_sig=open /></Portal>
                </Show>
            </div>
        </>
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupStep {
    Backend,
    Folder,
    Agent,
}

#[component]
pub fn GettingStarted() -> impl IntoView {
    let state = expect_context::<AppState>();
    view! {
        <Show when=move || state.getting_started_open.get()>
            <GettingStartedGuide />
        </Show>
    }
}

#[component]
fn GettingStartedGuide() -> impl IntoView {
    let state = expect_context::<AppState>();
    let step = RwSignal::new(SetupStep::Backend);
    let selection = RwSignal::new(None::<BackendKind>);
    let chooser_open = RwSignal::new(false);
    let selected_state = state.clone();
    let selected = Memo::new(move |_| {
        selection.get().or_else(|| {
            selected_state
                .selected_host_settings()
                .and_then(|settings| settings.default_backend)
        })
    });
    let info_state = state.clone();
    let info = Memo::new(move |_| {
        let kind = selected.get()?;
        info_state
            .selected_host_backend_setup()?
            .into_iter()
            .find(|info| info.backend_kind == kind)
    });
    let connected_state = state.clone();
    let connected = Memo::new(move |_| {
        matches!(
            connected_state.selected_host_connection_status(),
            ConnectionStatus::Connected
        )
    });
    let enabled_state = state.clone();
    let enabled = Memo::new(move |_| {
        selected.get().is_some_and(|kind| {
            enabled_state
                .selected_host_settings()
                .is_some_and(|settings| settings.enabled_backends.contains(&kind))
        })
    });
    let backend_usable = Memo::new(move |_| {
        connected.get()
            && enabled.get()
            && info
                .get()
                .is_some_and(|info| info.status == BackendSetupStatus::Installed)
    });
    let project_state = state.clone();
    let project = Memo::new(move |_| {
        let host = project_state.selected_host_id.get()?;
        let active = project_state.active_project.get()?;
        project_state.projects.get().into_iter().find(|project| {
            project.host_id == host
                && active.host_id == host
                && project.project.id == active.project_id
        })
    });
    let launch_project = Memo::new(move |_| {
        project.get().map(|project| ActiveProjectRef {
            host_id: project.host_id,
            project_id: project.project.id,
        })
    });
    let catalog_state = state.clone();
    let launch_available = Memo::new(move |_| {
        let Some(host) = catalog_state.selected_host_id.get() else {
            return false;
        };
        catalog_state.launch_profile_catalog.with(|catalogs| {
            catalogs.get(&host).is_some_and(|catalog| {
                catalog.entries.iter().any(|entry| {
                    matches!(entry, LaunchProfileEntry::Ready { profile }
                    if Some(profile.backend_kind) == selected.get()
                        && profile.kind == protocol::LaunchProfileKind::BackendDefault)
                })
            })
        })
    });
    let next_enabled = Memo::new(move |_| match step.get() {
        SetupStep::Backend => backend_usable.get(),
        SetupStep::Folder => backend_usable.get() && project.get().is_some(),
        SetupStep::Agent => {
            backend_usable.get() && project.get().is_some() && launch_available.get()
        }
    });
    let error_state = state.clone();
    let connection_message = Memo::new(move |_| {
        if error_state.selected_host_id.get().is_none() {
            return Some(
                "Choose a computer in Host settings to connect a backend and browse its folders."
                    .to_owned(),
            );
        }
        match error_state.selected_host_connection_status() {
            ConnectionStatus::Connected => None,
            ConnectionStatus::Connecting => Some("Connecting to your computer… Setup will update when it connects.".to_owned()),
            ConnectionStatus::Reconnecting { .. } => Some("Connection lost — reconnecting. Your configuration and chats are preserved.".to_owned()),
            ConnectionStatus::Disconnected => Some("This computer is disconnected. Open Host settings to connect it before continuing.".to_owned()),
            ConnectionStatus::Error(message) => Some(format!("Tyde could not connect to this computer: {message}")),
        }
    });
    let notice_host = state.selected_host_id;
    let notice_scopes =
        Signal::derive(move || vec![crate::notices::NoticeScope::Settings(notice_host.get())]);
    let visible_state = state.clone();
    let visible = Memo::new(move |_| {
        !visible_state.settings_open.get()
            && visible_state.browse_dialog.get().is_none()
            && !crate::notices::has_notice(&crate::notices::NoticeScope::Browser)
            && !chooser_open.get()
    });
    let opener: StoredValue<Option<web_sys::HtmlElement>, LocalStorage> = StoredValue::new_local(
        web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.active_element())
            .and_then(|element| element.dyn_into().ok()),
    );
    on_cleanup(move || {
        if let Some(opener) = opener.get_value() {
            let _ = opener.focus();
        }
    });
    let title_ref = NodeRef::<leptos::html::H2>::new();
    Effect::new(move |_| {
        let _ = step.get();
        if visible.get()
            && let Some(title) = title_ref.get()
        {
            let _ = title.focus();
        }
    });
    let dialog_ref = NodeRef::<leptos::html::Div>::new();
    let key_state = state.clone();
    let on_keydown = move |event: web_sys::KeyboardEvent| {
        event.stop_propagation();
        if event.is_composing() {
            return;
        }
        match event.key().as_str() {
            "Escape" => {
                event.prevent_default();
                key_state.getting_started_open.set(false);
            }
            "Tab" => {
                let Some(dialog) = dialog_ref.get_untracked() else {
                    return;
                };
                let Ok(nodes) = dialog.query_selector_all(
                    "button:not([disabled]), input:not([disabled]), a[href], summary",
                ) else {
                    return;
                };
                let focusable: Vec<web_sys::HtmlElement> = (0..nodes.length())
                    .filter_map(|i| nodes.item(i)?.dyn_into::<web_sys::HtmlElement>().ok())
                    .filter(|element| element.get_bounding_client_rect().height() > 0.0)
                    .collect();
                let (Some(first), Some(last)) = (focusable.first(), focusable.last()) else {
                    return;
                };
                let active = web_sys::window()
                    .and_then(|w| w.document())
                    .and_then(|d| d.active_element());
                let on_first = active
                    .as_ref()
                    .is_some_and(|element| element.is_same_node(Some(first)));
                let on_last = active
                    .as_ref()
                    .is_some_and(|element| element.is_same_node(Some(last)));
                let on_title = active
                    .as_ref()
                    .is_some_and(|element| element.id() == "getting-started-title");
                if event.shift_key() && (on_first || on_title) {
                    event.prevent_default();
                    let _ = last.focus();
                } else if !event.shift_key() && on_last {
                    event.prevent_default();
                    let _ = first.focus();
                }
            }
            _ => {}
        }
    };
    let next = move |_| {
        if !next_enabled.get_untracked() {
            return;
        }
        match step.get_untracked() {
            SetupStep::Backend => step.set(SetupStep::Folder),
            SetupStep::Folder => step.set(SetupStep::Agent),
            SetupStep::Agent => chooser_open.set(true),
        }
    };
    let host_state = state.clone();
    let settings_state = state.clone();
    let folder_state = state.clone();
    let close_state = state.clone();
    let skip_state = state.clone();
    let project_list_state = state.clone();
    let terminal_state = state.clone();
    let terminal_setup = Callback::new(move |()| terminal_state.getting_started_open.set(false));
    let created_state = state.clone();
    let created = Callback::new(move |()| created_state.getting_started_open.set(false));
    view! {
        <Show when=move || visible.get()>
            {let host_state = host_state.clone();
            let close_state = close_state.clone();
            let settings_state = settings_state.clone();
            let skip_state = skip_state.clone();
            let project_list_state = project_list_state.clone();
            let folder_state = folder_state.clone();
            let state = state.clone();
            view! { <div class="getting-started-overlay">
                <div class="getting-started" role="dialog" aria-modal="true" aria-label="Getting started"
                    aria-describedby="getting-started-description" node_ref=dialog_ref on:keydown=on_keydown>
                    <header class="getting-started-header">
                        <div><strong>"Welcome to Tyde"</strong><p id="getting-started-description">"Your first agent, one step at a time."</p></div>
                        <span class="getting-started-host">{move || {
                            let host = host_state.selected_host();
                            let name = host.map(|host| host.label).unwrap_or_else(|| "No computer selected".to_owned());
                            format!("{} · {}", name, if connected.get() { "Connected" } else { "Not connected" })
                        }}</span>
                        <button class="launch-chooser-close" aria-label="Close getting started" on:click=move |_| close_state.getting_started_open.set(false)>"×"</button>
                    </header>
                    <div class="getting-started-body">
                        <nav class="getting-started-rail" aria-label="Setup steps">
                            <ol>{[
                                (SetupStep::Backend, "Connect a backend", "Use an account you already have"),
                                (SetupStep::Folder, "Choose a folder", "Give your agent a place to work"),
                                (SetupStep::Agent, "Start your first agent", "Ask a question or hand off a task"),
                            ].into_iter().enumerate().map(|(index, (target, title, description))| view! {
                                <li><button aria-current=move || (step.get() == target).then_some("step") on:click=move |_| step.set(target)>
                                    <span class="getting-started-number">{index + 1}</span>
                                    <span><strong>{title}</strong><small>{description}</small></span>
                                </button></li>
                            }).collect_view()}</ol>
                            <button class="getting-started-link" on:click=move |_| {
                                settings_state.settings_tab_request.set(Some("Hosts"));
                                settings_state.settings_open.set(true);
                            }>"Host settings / remote computer"</button>
                        </nav>
                        <main class="getting-started-content">
                            <p class="getting-started-eyebrow">{move || match step.get() {
                                SetupStep::Backend => "STEP 1 OF 3", SetupStep::Folder => "STEP 2 OF 3", SetupStep::Agent => "STEP 3 OF 3",
                            }}</p>
                            <h2 id="getting-started-title" tabindex="-1" node_ref=title_ref>{move || match step.get() {
                                SetupStep::Backend => "Connect an AI backend", SetupStep::Folder => "Choose a folder", SetupStep::Agent => "Start your first agent",
                            }}</h2>
                            {move || connection_message.get().map(|message| view! { <div class="getting-started-notice" role="status">{message}</div> })}
                            <crate::notices::InlineNotices scopes=notice_scopes />
                            <Show when=move || step.get() == SetupStep::Backend>
                                {let state = state.clone();
                                view! { <p>"Tyde runs the coding agents you already use. Choose a backend on this computer."</p>
                                <SetupBackendChoices selected selection more=false />
                                <details class="getting-started-more" open=move || selected.get().is_some_and(|kind| !matches!(kind, BackendKind::Codex | BackendKind::Claude))>
                                    <summary>"More backends"</summary>
                                    <SetupBackendChoices selected selection more=true />
                                </details>
                                <Show when=move || info.get().is_none()>
                                    <p role="status">"No backend selected yet. Choose one above, or open Backend settings to install and enable one."</p>
                                </Show>
                                {move || selected.get().map(|kind| view! {
                                    <fieldset class="getting-started-details" aria-label="Backend setup details" disabled=move || !connected.get()>
                                        <BackendCard kind on_setup=terminal_setup />
                                        <p>{move || if enabled.get() { "Enabled on this computer." } else { "Enable this backend using the switch above." }}</p>
                                        <p>"Tyde checks installation, not sign-in. An installed CLI may still need your account. Authentication stays in the backend's own terminal; never paste credentials into a chat."</p>
                                        <SetupSignIn kind connected />
                                    </fieldset>
                                })}
                                <button class="getting-started-link" on:click={let state = state.clone(); move |_| {
                                    state.settings_tab_request.set(Some("Backends")); state.settings_open.set(true);
                                }}>"Open Backend settings"</button>
                                }}
                            </Show>
                            <Show when=move || step.get() == SetupStep::Folder>
                                {let project_list_state = project_list_state.clone();
                                let folder_state = folder_state.clone();
                                view! { <p>"A project is a folder your agent can read and edit. Choose an existing project, or browse for another folder."</p>
                                <fieldset class="getting-started-projects"><legend>"Projects on this computer"</legend>
                                    {move || {
                                        let host = project_list_state.selected_host_id.get();
                                        project_list_state.projects.get().into_iter().filter(|project| Some(&project.host_id) == host.as_ref()).map(|entry| {
                                            let active = ActiveProjectRef { host_id: entry.host_id, project_id: entry.project.id.clone() };
                                            let roots = entry.project.root_paths().iter().map(|root| root.0.as_str()).collect::<Vec<_>>().join(" · ");
                                            let checked = active.clone();
                                            let state = project_list_state.clone();
                                            let select_state = project_list_state.clone();
                                            view! {
                                                <label class="getting-started-choice" class:selected=move || state.active_project.get().as_ref() == Some(&checked)>
                                                    <input type="radio" name="getting-started-project" prop:checked={let state = select_state.clone(); let active = active.clone(); move || state.active_project.get().as_ref() == Some(&active)}
                                                        on:change=move |_| select_state.switch_active_project(Some(active.clone())) />
                                                    <span><strong>{entry.project.name}</strong><small>{roots}</small></span>
                                                </label>
                                            }
                                        }).collect_view()
                                    }}
                                </fieldset>
                                <Show when=move || project.get().is_none()><p role="status">"Choose a project or add a folder before continuing."</p></Show>
                                <button class="action-btn" disabled=move || !connected.get() on:click=move |_| open_project_browser_on_selected_host(&folder_state)>"Choose a folder…"</button>
                                <p class="getting-started-hint">"Browse folders on this computer. To use another computer, choose it in Host settings first. Cancel the browser to return without changes."</p>
                                }}
                            </Show>
                            <Show when=move || step.get() == SetupStep::Agent>
                                <p>"Choose New agent below to review the backend and role in the usual agent dialog. Then describe your task in the new chat to start working."</p>
                                <section class="getting-started-details" aria-label="Your choices">
                                    <h3>"Your choices"</h3>
                                    <p>{move || selected.get().map(|kind| format!("Backend: {}", backend_label(kind))).unwrap_or_else(|| "Choose a backend in step 1.".to_owned())}</p>
                                    <p>{move || project.get().map(|project| format!("Project: {}", project.project.name)).unwrap_or_else(|| "Choose a folder in step 2.".to_owned())}</p>
                                    <p>"Try: “Explain this project and suggest a small first task.”"</p>
                                </section>
                                <Show when=move || !backend_usable.get()><p class="getting-started-notice" role="status">"Return to step 1 to connect, install and enable your backend."</p></Show>
                                <Show when=move || backend_usable.get() && !launch_available.get()><p role="status">"Launch options for this backend are not available yet. Check Backend settings for details."</p></Show>
                                <p>"Nothing is sent until you send a message. Existing agents keep running, and closing this guide never closes your chats."</p>
                            </Show>
                        </main>
                    </div>
                    <footer class="getting-started-footer">
                        <span class="getting-started-summary" aria-live="polite">{move || {
                            let label = selected.get().map(backend_label).unwrap_or("No backend selected");
                            let status = backend_setup_status_label(info.get().as_ref());
                            format!("{label} · {status}{}", if enabled.get() { " · Enabled" } else { "" })
                        }}</span>
                        <div class="getting-started-actions">
                            <button class="getting-started-link" on:click=move |_| skip_state.getting_started_open.set(false)>"Skip for now"</button>
                            <Show when=move || step.get() != SetupStep::Backend>
                                <button class="action-btn" on:click=move |_| step.update(|step| *step = match step {
                                    SetupStep::Backend | SetupStep::Folder => SetupStep::Backend,
                                    SetupStep::Agent => SetupStep::Folder,
                                })>"Back"</button>
                            </Show>
                            <button class="action-btn primary" disabled=move || !next_enabled.get() on:click=next>{move || match step.get() {
                                SetupStep::Backend => "Continue to folder →", SetupStep::Folder => "Continue to agent →", SetupStep::Agent => "New agent →",
                            }}</button>
                        </div>
                    </footer>
                </div>
            </div>
            }}
        </Show>
        <Show when=move || chooser_open.get()>
            {move || selected.get().map(|kind| view! { <LaunchChooser open_sig=chooser_open initial_backend=kind project_context=launch_project on_created=created /> })}
        </Show>
    }
}

#[component]
fn SetupBackendChoices(
    selected: Memo<Option<BackendKind>>,
    selection: RwSignal<Option<BackendKind>>,
    more: bool,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    view! {
        <fieldset class="getting-started-backends"><legend>{if more { "More backend choices" } else { "Backend choices" }}</legend>
            {move || {
                let mut choices: Vec<_> = state.selected_host_backend_setup().unwrap_or_default().into_iter()
                    .filter(|backend| matches!(backend.backend_kind, BackendKind::Codex | BackendKind::Claude) != more).collect();
                choices.sort_by_key(|backend| backend.backend_kind != BackendKind::Codex);
                choices.into_iter().map(|backend| {
                    let kind = backend.backend_kind;
                    let status = backend_setup_status_label(Some(&backend));
                    view! {
                        <label class="getting-started-choice" class:selected=move || selected.get() == Some(kind)>
                            {backend_icon(kind)}
                            <span><strong>{backend_label(kind)}</strong><small>{status}</small></span>
                            <input type="radio" name="getting-started-backend" prop:checked=move || selected.get() == Some(kind)
                                on:change=move |_| selection.set(Some(kind)) />
                        </label>
                    }
                }).collect_view()
            }}
        </fieldset>
    }
}

#[component]
fn SetupSignIn(kind: BackendKind, connected: Memo<bool>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let info_state = state.clone();
    move || {
        let info = info_state
            .selected_host_backend_setup()?
            .into_iter()
            .find(|info| info.backend_kind == kind)?;
        let command = info.sign_in_command?;
        let state = state.clone();
        Some(view! {
            <button class="action-btn" disabled=move || !connected.get() || !command.runnable || info.status != BackendSetupStatus::Installed
                on:click=move |_| {
                    send_run_backend_setup(&state, kind, BackendSetupAction::SignIn);
                    state.getting_started_open.set(false);
                }>"Sign in in terminal"</button>
            <p class="getting-started-hint">"The guide closes so you can use the terminal. Reopen Getting started from Home, Help or the command palette when you're done."</p>
        })
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
        if document
            .get_element_by_id("getting-started-test-css")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("getting-started-test-css");
            style.set_text_content(Some(include_str!("../../styles.css")));
            document.head().unwrap().append_child(&style).unwrap();
        }
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container
            .set_attribute(
                "style",
                "display:flex;flex-direction:column;width:100%;height:680px;",
            )
            .unwrap();
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
                settings_model::HostSettings {
                    resume_previous_agents: settings_model::default_resume_previous_agents(),
                    pause_swarms_on_restart: false,
                    review: Default::default(),
                    enabled_backends: vec![BackendKind::Claude],
                    default_backend: Some(BackendKind::Claude),
                    default_launch_profile_id: None,
                    enable_mobile_connections: false,

                    mobile_direct_hosting_enabled: false,
                    mobile_direct_bind_addr: None,
                    mobile_direct_public_origin: None,
                    mobile_direct_bundle_dir: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: false,
                    tyde_agent_control_max_depth: settings_model::default_agent_control_max_depth(),
                    delegation_launch_profile_order:
                        settings_model::default_delegation_launch_profile_order(),
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    supervisor: Default::default(),
                    usage_limits: Default::default(),
                    code_intel: Default::default(),
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: Default::default(),
                    hermes_disabled_providers: Default::default(),
                    voice: Default::default(),
                    tychat: Default::default(),
                    chatgpt_dots_enabled: false,
                },
            );
        });
        state.backend_setup_by_host.update(|map| {
            map.insert(
                host_id.to_owned(),
                vec![protocol::BackendSetupInfo {
                    backend_kind: BackendKind::Claude,
                    status: BackendSetupStatus::Installed,
                    installed_version: Some("Claude Code test".to_owned()),
                    docs_url: "https://example.com".to_owned(),
                    install_command: None,
                    diagnostic: None,
                    sign_in_command: None,
                }],
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
                tyde_permissions: Default::default(),
                host_id: host_id.to_owned(),
                agent_id: AgentId("a-1".to_owned()),
                name: "agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: Some(ProjectId("p-1".to_owned())),
                parent_agent_id: None,
                team_member_id: None,
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

        state.selected_host_id.set(Some("local".to_owned()));
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

    #[wasm_bindgen_test]
    async fn first_run_host_failure_is_explained_inline() {
        let container = make_container();
        let state = AppState::new();
        state.selected_host_id.set(Some("local".to_owned()));
        state.connection_statuses.update(|statuses| {
            statuses.insert(
                "local".to_owned(),
                ConnectionStatus::Error("embedded host failed to start".to_owned()),
            );
        });
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <HomeView /> }
        });

        next_tick().await;
        let error = container
            .query_selector(".home-connection-error")
            .unwrap()
            .expect("first-run host failure must render inline");
        assert_eq!(error.get_attribute("role").as_deref(), Some("alert"));
        let text = error.text_content().unwrap_or_default();
        assert!(
            text.contains("embedded host failed to start"),
            "the real connection error must be visible: {text}"
        );
    }

    #[wasm_bindgen_test]
    async fn enabled_but_broken_backend_is_not_reported_as_ready() {
        let container = make_container();
        let state = AppState::new();
        state.selected_host_id.set(Some("local".to_owned()));
        enable_backend(&state, "local");
        state.backend_setup_by_host.update(|setup| {
            setup.insert(
                "local".to_owned(),
                vec![protocol::BackendSetupInfo {
                    backend_kind: BackendKind::Claude,
                    status: BackendSetupStatus::Unavailable,
                    installed_version: None,
                    docs_url: "https://example.com".to_owned(),
                    install_command: None,
                    diagnostic: Some(protocol::BackendSetupDiagnostic {
                        code: protocol::BackendSetupDiagnosticCode::CommandTimedOut,
                        message:
                            "Tyde found claude, but its --version check did not finish within 60 seconds"
                                .to_owned(),
                    }),
                    sign_in_command: None,
                }],
            );
        });
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <HomeView /> }
        });

        next_tick().await;
        assert_eq!(
            checkmark_count(&container),
            0,
            "an enabled but unusable backend must not complete first-run setup"
        );
        let warning = container
            .query_selector(".home-backend-error")
            .unwrap()
            .expect("broken enabled backend must be explained on Home");
        let text = warning.text_content().unwrap_or_default();
        assert!(
            text.contains("found claude") && text.contains("within 60 seconds"),
            "Home must show the backend probe's real diagnostic: {text}"
        );
    }
    fn guide(container: &HtmlElement) -> HtmlElement {
        container
            .query_selector("[role=dialog][aria-label='Getting started']")
            .unwrap()
            .expect("guide is open")
            .dyn_into()
            .unwrap()
    }

    fn button(container: &HtmlElement, label: &str) -> web_sys::HtmlButtonElement {
        find_button_by_text(container, label)
            .expect("visible action")
            .dyn_into()
            .unwrap()
    }

    fn key(target: &web_sys::HtmlElement, name: &str, shift: bool) {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key(name);
        init.set_shift_key(shift);
        init.set_bubbles(true);
        init.set_cancelable(true);
        target
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init)
                    .unwrap(),
            )
            .unwrap();
    }

    fn is_focused(element: &web_sys::HtmlElement) -> bool {
        web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .active_element()
            .is_some_and(|active| active.is_same_node(Some(element)))
    }

    fn install_send_recorder() -> js_sys::Array {
        js_sys::eval(
            r#"(() => {
            const calls = [];
            window.__TAURI__ = window.__TAURI__ || {};
            window.__TAURI__.core = { invoke: (command, args) => {
                if (command === 'send_host_line') calls.push(JSON.parse(args.line));
                return Promise.resolve();
            }};
            return calls;
        })()"#,
        )
        .unwrap()
        .dyn_into()
        .unwrap()
    }

    fn sent(calls: &js_sys::Array, kind: &str) -> Vec<serde_json::Value> {
        calls
            .iter()
            .map(|frame| serde_wasm_bindgen::from_value::<serde_json::Value>(frame).unwrap())
            .filter(|frame| frame["kind"] == kind)
            .collect()
    }

    #[wasm_bindgen_test]
    async fn getting_started_reopens_without_changing_configuration() {
        let container = make_container();
        let state = AppState::new();
        let calls = install_send_recorder();
        let state_for_mount = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <HomeView /> <crate::components::help_tour::HelpTour /> <GettingStarted />
            <crate::components::command_palette::CommandPalette /> }
        });
        next_tick().await;
        let open = find_button_by_text(&container, "Getting started")
            .expect("Home must have a permanent Getting started action");
        open.focus().unwrap();
        open.click();
        next_tick().await;
        let dialog = guide(&container);
        assert!(visible_text(&dialog).contains("Choose a computer"));
        assert!(button(&dialog, "Continue to folder").disabled());
        assert!(is_focused(
            &dialog
                .query_selector("h2")
                .unwrap()
                .unwrap()
                .dyn_into()
                .unwrap()
        ));
        let close: HtmlElement = dialog
            .query_selector("button[aria-label='Close getting started']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        let skip: HtmlElement = button(&dialog, "Skip for now").unchecked_into();
        skip.focus().unwrap();
        key(&skip, "Tab", false);
        assert!(is_focused(&close), "Tab wraps inside the modal");
        key(&close, "Tab", true);
        assert!(is_focused(&skip), "Shift+Tab wraps inside the modal");
        skip.click();
        next_tick().await;
        assert!(
            container
                .query_selector("[role=dialog][aria-label='Getting started']")
                .unwrap()
                .is_none()
        );
        assert!(is_focused(&open), "closing restores focus to Home's opener");

        state.selected_host_id.set(Some("local".to_owned()));
        enable_backend(&state, "local");
        add_project(&state, "local");
        add_agent(&state, "local");
        let settings = state
            .selected_host_settings_untracked()
            .unwrap()
            .enabled_backends;
        let tabs: Vec<_> = state
            .center_zone
            .get_untracked()
            .all_tabs()
            .map(|(_, tab)| tab.id)
            .collect();
        open.click();
        next_tick().await;
        let text = visible_text(&guide(&container));
        assert!(text.contains("Installed") && text.contains("Enabled"));
        assert!(!text.contains("Signed in") && !text.contains("is ready"));
        key(&guide(&container), "Escape", false);
        next_tick().await;
        button(&container, "Help").click();
        next_tick().await;
        let help: HtmlElement = container
            .query_selector(".help-tour-card")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(
            visible_text(&help).contains("Your projects"),
            "UI tour remains distinct"
        );
        button(&help, "Getting started").click();
        next_tick().await;
        assert_eq!(state.help_tour_step.get_untracked(), None);
        assert!(visible_text(&guide(&container)).contains("Installed"));
        button(&guide(&container), "Skip for now").click();
        next_tick().await;
        state.settings_open.set(true);
        state.command_palette_open.set(true);
        next_tick().await;
        let input: web_sys::HtmlInputElement = container
            .query_selector(".cp-input")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value("> Getting started");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        key(input.unchecked_ref(), "Enter", false);
        next_tick().await;
        assert!(visible_text(&guide(&container)).contains("Connect an AI backend"));
        assert!(!state.command_palette_open.get_untracked());
        assert!(!state.settings_open.get_untracked());
        button(&guide(&container), "Skip for now").click();
        next_tick().await;
        assert_eq!(
            state
                .selected_host_settings_untracked()
                .unwrap()
                .enabled_backends,
            settings
        );
        assert_eq!(state.projects.get_untracked().len(), 1);
        assert_eq!(state.agents.get_untracked().len(), 1);
        assert_eq!(
            state
                .center_zone
                .get_untracked()
                .all_tabs()
                .map(|(_, tab)| tab.id)
                .collect::<Vec<_>>(),
            tabs
        );
        assert!(sent(&calls, "interrupt").is_empty());
        assert!(sent(&calls, "close_agent").is_empty());
        drop(handle);
        container.remove();
    }

    #[wasm_bindgen_test]
    async fn guided_setup_tracks_live_backend_folder_and_agent_flow() {
        let container = make_container();
        let calls = install_send_recorder();
        let state = AppState::new();
        state.selected_host_id.set(Some("local".to_owned()));
        state.connection_statuses.update(|statuses| {
            statuses.insert("local".to_owned(), ConnectionStatus::Connected);
        });
        state.host_streams.update(|streams| {
            streams.insert("local".to_owned(), StreamPath("/host".to_owned()));
        });
        state.configured_hosts.set(vec![
            crate::bridge::ConfiguredHost {
                id: "local".to_owned(),
                label: "First computer".to_owned(),
                transport: crate::bridge::HostTransportConfig::LocalEmbedded,
                auto_connect: true,
            },
            crate::bridge::ConfiguredHost {
                id: "other".to_owned(),
                label: "Second computer".to_owned(),
                transport: crate::bridge::HostTransportConfig::LocalEmbedded,
                auto_connect: true,
            },
        ]);
        state.connection_statuses.update(|statuses| {
            statuses.insert("other".to_owned(), ConnectionStatus::Connected);
        });
        state.host_streams.update(|streams| {
            streams.insert("other".to_owned(), StreamPath("/host-other".to_owned()));
        });
        enable_backend(&state, "local");
        state.backend_setup_by_host.update(|hosts| {
            let info = &mut hosts.get_mut("local").unwrap()[0];
            info.status = BackendSetupStatus::NotInstalled;
            info.install_command = Some(protocol::BackendSetupCommand {
                title: "Install".to_owned(),
                description: "Install in terminal".to_owned(),
                command: "backend-installer".to_owned(),
                display_command: None,
                runnable: true,
            });
        });
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            let active_state = mount_state.clone();
            Effect::new(move |_| {
                if let Some(tab) = active_state.center_zone.with(|zone| zone.active_tab_id()) {
                    active_state.bump_tab_lru(tab);
                }
            });
            view! { <crate::components::center_zone::CenterZone /> <GettingStarted /> <crate::components::host_browser::HostBrowser /> <crate::components::terminal_view::TerminalView /> }
        });
        next_tick().await;
        button(&container, "Getting started").click();
        next_tick().await;
        assert!(button(&guide(&container), "Continue to folder").disabled());
        button(&guide(&container), "Install").click();
        next_tick().await;
        assert!(
            !state.getting_started_open.get_untracked(),
            "terminal setup must not remain behind the modal"
        );
        assert_eq!(
            sent(&calls, "run_backend_setup")[0]["payload"]["action"],
            "install"
        );
        assert_eq!(
            sent(&calls, "run_backend_setup")[0]["payload"]["backend_kind"],
            "claude"
        );
        crate::dispatch::clear_host_seqs("local");
        crate::dispatch::dispatch_envelope(
            &state,
            "local",
            protocol::Envelope::from_payload(
                StreamPath("/host".to_owned()),
                protocol::FrameKind::CommandError,
                0,
                &protocol::CommandErrorPayload {
                    context: None,
                    request_id: None,
                    stream: StreamPath("/host".to_owned()),
                    request_kind: protocol::FrameKind::RunBackendSetup,
                    operation: "run_backend_setup".to_owned(),
                    code: protocol::CommandErrorCode::Internal,
                    message:
                        "The host could not open the setup terminal. Check available disk space."
                            .to_owned(),
                    fatal: false,
                },
            )
            .unwrap(),
        );
        next_tick().await;
        let terminal: HtmlElement = container
            .query_selector(".terminal-view")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        let error = terminal
            .query_selector("[role=alert]")
            .unwrap()
            .expect("setup failures remain visible on the handoff surface");
        assert!(
            error
                .text_content()
                .unwrap()
                .contains("could not open the setup terminal")
        );
        assert!(error.get_bounding_client_rect().height() > 0.0);
        terminal
            .query_selector("button[aria-label='Dismiss notice']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        enable_backend(&state, "local");
        state.getting_started_open.set(true);
        next_tick().await;
        assert!(!button(&guide(&container), "Continue to folder").disabled());
        state.backend_setup_by_host.update(|hosts| {
            let info = &mut hosts.get_mut("local").unwrap()[0];
            info.status = BackendSetupStatus::Unavailable;
            info.diagnostic = Some(protocol::BackendSetupDiagnostic {
                code: protocol::BackendSetupDiagnosticCode::CommandFailed,
                message: "The CLI version check failed. Repair the installation.".to_owned(),
            });
        });
        next_tick().await;
        assert!(visible_text(&guide(&container)).contains("The CLI version check failed"));
        assert!(
            button(&guide(&container), "Continue to folder").disabled(),
            "same-backend server updates must revoke progression"
        );
        enable_backend(&state, "local");
        next_tick().await;
        button(&guide(&container), "Continue to folder").click();
        next_tick().await;
        assert!(button(&guide(&container), "Continue to agent").disabled());
        button(&guide(&container), "Choose a folder…").click();
        next_tick().await;
        assert!(
            container
                .query_selector("[aria-label='Getting started']")
                .unwrap()
                .is_none()
        );
        let browser: HtmlElement = container
            .query_selector("[aria-label='Folder browser']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert_eq!(
            sent(&calls, "host_browse_start").len(),
            1,
            "use the canonical folder-browser operation"
        );
        let hosts: web_sys::HtmlSelectElement = browser
            .query_selector("select")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(
            hosts.disabled(),
            "onboarding browsing stays on its explicitly selected computer"
        );
        assert!(
            visible_text(&browser).contains("Second computer"),
            "exercise a real two-host picker"
        );
        assert_eq!(hosts.value(), "local");
        let address: HtmlElement = browser
            .query_selector("input[aria-label='Folder path']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(is_focused(&address));
        key(&address, "Escape", false);
        next_tick().await;
        assert!(visible_text(&guide(&container)).contains("STEP 2 OF 3"));
        button(&guide(&container), "Choose a folder…").click();
        next_tick().await;
        state.browse_dialog.with_untracked(|dialog| {
            let dialog = dialog.as_ref().unwrap();
            dialog
                .current_path
                .set(Some(protocol::HostAbsPath("/tmp/demo".to_owned())));
            dialog.loading.set(false);
        });
        next_tick().await;
        let browser: HtmlElement = container
            .query_selector("[aria-label='Folder browser']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        button(&browser, "Open Project Here").click();
        next_tick().await;
        assert_eq!(
            sent(&calls, "project_create")[0]["payload"]["roots"][0],
            "/tmp/demo"
        );
        add_project(&state, "local");
        next_tick().await;
        let radio: web_sys::HtmlInputElement = guide(&container)
            .query_selector("input[name='getting-started-project']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        radio.click();
        next_tick().await;
        assert!(!button(&guide(&container), "Continue to agent").disabled());
        button(&guide(&container), "Continue to agent").click();
        next_tick().await;
        assert!(visible_text(&guide(&container)).contains("Project: demo"));
        assert!(
            button(&guide(&container), "New agent").disabled(),
            "missing server launch options must block launch"
        );
        state.launch_profile_catalog.update(|catalogs| {
            catalogs.insert(
                "local".to_owned(),
                protocol::LaunchProfileCatalog {
                    home_custom_agent_id: None,
                    entries: vec![LaunchProfileEntry::Ready {
                        profile: protocol::LaunchProfile {
                            id: protocol::LaunchProfileId("claude:default".to_owned()),
                            kind: protocol::LaunchProfileKind::BackendDefault,
                            label: "Claude".to_owned(),
                            description: Some("Coding agent".to_owned()),
                            backend_kind: BackendKind::Claude,
                            session_settings: Default::default(),
                        },
                    }],
                    default_profile_id: None,
                    custom_profile_schemas: Vec::new(),
                },
            );
        });
        next_tick().await;
        button(&guide(&container), "Back").click();
        next_tick().await;
        let selected_project: web_sys::HtmlInputElement = guide(&container)
            .query_selector("input[name='getting-started-project']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(
            selected_project.checked(),
            "Back preserves the actual project selection"
        );
        button(&guide(&container), "Continue to agent").click();
        next_tick().await;
        state.tabs_enabled.set(false);
        state.open_tab(
            crate::state::TabContent::empty_chat(),
            "Existing chat".to_owned(),
            true,
        );
        next_tick().await;
        next_tick().await;
        let draft: web_sys::HtmlTextAreaElement = container
            .query_selector("textarea")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        draft.set_value("Keep this unsent draft");
        draft
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        button(&guide(&container), "New agent").click();
        next_tick().await;
        let chooser: HtmlElement = container
            .query_selector("[aria-label='New agent']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(visible_text(&chooser).contains("Claude · Default agent"));
        button(&chooser, "Cancel").click();
        next_tick().await;
        assert!(visible_text(&guide(&container)).contains("STEP 3 OF 3"));
        button(&guide(&container), "Skip for now").click();
        next_tick().await;
        let retained: web_sys::HtmlTextAreaElement = container
            .query_selector("textarea")
            .unwrap()
            .expect("cancellation must retain the existing chat composer")
            .dyn_into()
            .unwrap();
        assert!(
            retained.value() == "Keep this unsent draft",
            "Cancelling setup must preserve the unsent draft"
        );
        assert!(retained.get_bounding_client_rect().height() > 0.0);
        state.getting_started_open.set(true);
        next_tick().await;
        button(&guide(&container), "Start your first agent").click();
        next_tick().await;
        button(&guide(&container), "New agent").click();
        next_tick().await;
        let chooser: HtmlElement = container
            .query_selector("[aria-label='New agent']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        button(&chooser, "Create agent").click();
        next_tick().await;
        next_tick().await;
        assert!(!state.getting_started_open.get_untracked());
        assert!(
            container
                .query_selector("[role=tab][aria-selected=true][aria-label='New Chat']")
                .unwrap()
                .is_some()
        );
        let composer = container
            .query_selector("textarea")
            .unwrap()
            .expect("new chat has a real composer");
        assert!(composer.get_bounding_client_rect().height() > 0.0);
        assert!(
            sent(&calls, "spawn_agent").is_empty(),
            "no backend call until the user sends a task"
        );
        assert!(sent(&calls, "interrupt").is_empty());
        drop(handle);
        container.remove();
    }
    #[wasm_bindgen_test]
    async fn guided_setup_is_truthful_accessible_and_contained_in_both_themes() {
        let container = make_container();
        let calls = install_send_recorder();
        let state = AppState::new();
        state.selected_host_id.set(Some("local".to_owned()));
        state.connection_statuses.update(|statuses| {
            statuses.insert("local".to_owned(), ConnectionStatus::Connected);
        });
        state.host_streams.update(|streams| {
            streams.insert("local".to_owned(), StreamPath("/host".to_owned()));
        });
        enable_backend(&state, "other-computer");
        let mounted = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mounted.clone());
            view! { <HomeView /> <GettingStarted /> <crate::components::settings_panel::SettingsPanel /> <crate::components::terminal_view::TerminalView /> }
        });
        next_tick().await;
        button(&container, "Getting started").click();
        next_tick().await;
        assert!(
            button(&guide(&container), "Continue to folder").disabled(),
            "another host's backend cannot make this computer usable"
        );
        enable_backend(&state, "local");
        state.backend_setup_by_host.update(|hosts| {
            hosts.get_mut("local").unwrap()[0].sign_in_command =
                Some(protocol::BackendSetupCommand {
                    title: "Sign in".to_owned(),
                    description: "CLI-native authentication".to_owned(),
                    command: "claude auth login".to_owned(),
                    display_command: None,
                    runnable: true,
                });
        });
        next_tick().await;
        state
            .host_settings_by_host
            .update(|hosts| hosts.get_mut("local").unwrap().enabled_backends.clear());
        next_tick().await;
        let enable: web_sys::HtmlInputElement = guide(&container)
            .query_selector("input[aria-label='Enable Claude']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        enable.click();
        next_tick().await;
        assert!(!sent(&calls, "settings_write").is_empty());
        assert!(
            !enable.checked(),
            "the enable switch must not claim success before the server confirms it"
        );
        assert!(
            button(&guide(&container), "Continue to folder").disabled(),
            "a settings request is not proof that the server enabled the backend"
        );
        crate::dispatch::clear_host_seqs("local");
        crate::dispatch::dispatch_envelope(
            &state,
            "local",
            protocol::Envelope::from_payload(
                StreamPath("/host".to_owned()),
                protocol::FrameKind::SettingsWriteResult,
                0,
                &protocol::SettingsWriteResultPayload {
                    write_id: protocol::SettingsWriteId(
                        sent(&calls, "settings_write")[0]["payload"]["write_id"]
                            .as_str()
                            .unwrap()
                            .to_owned(),
                    ),
                    applied: false,
                    current_etag: "unchanged".to_owned(),
                    field_errors: vec![protocol::SettingsFieldError {
                        pointer: "/enabled_backends".to_owned(),
                        code: protocol::SettingsErrorCode::Invalid,
                        message: "This backend could not be enabled. Check the host configuration."
                            .to_owned(),
                    }],
                },
            )
            .unwrap(),
        );
        next_tick().await;
        let alert = guide(&container)
            .query_selector("[role=alert]")
            .unwrap()
            .expect("rejected enable operation is explained in the guide");
        assert!(
            alert
                .text_content()
                .unwrap()
                .contains("This backend could not be enabled")
        );
        guide(&container)
            .query_selector("button[aria-label='Dismiss notice']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        state.host_settings_by_host.update(|hosts| {
            hosts
                .get_mut("local")
                .unwrap()
                .enabled_backends
                .push(BackendKind::Claude)
        });
        next_tick().await;
        assert!(!button(&guide(&container), "Continue to folder").disabled());
        let overlay: HtmlElement = container
            .query_selector(".getting-started-overlay")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        for theme in ["dark", "light"] {
            overlay.set_attribute("data-theme", theme).unwrap();
            for width in [960, 520, 360] {
                overlay.set_attribute("style", &format!("width:{width}px;height:640px;left:0;top:0;right:auto;bottom:auto;padding:8px;")).unwrap();
                next_tick().await;
                let dialog = guide(&container);
                let bounds = dialog.get_bounding_client_rect();
                assert!(
                    dialog.scroll_width() <= dialog.client_width(),
                    "guide must not scroll horizontally"
                );
                let footer = dialog
                    .query_selector("footer")
                    .unwrap()
                    .unwrap()
                    .get_bounding_client_rect();
                assert!(
                    footer.bottom() <= bounds.bottom() + 1.0 && footer.height() > 0.0,
                    "next action remains visible"
                );
                let next = button(&dialog, "Continue to folder");
                let rect = next.get_bounding_client_rect();
                assert!(rect.left() >= bounds.left() && rect.right() <= bounds.right() + 1.0);
                // BackendCard also contains a compact enable-switch label. It is
                // not a backend choice card; check the actual radio-card targets.
                let choices = dialog
                    .query_selector_all("input[name='getting-started-backend']")
                    .unwrap();
                for index in 0..choices.length() {
                    let rect = choices
                        .item(index)
                        .unwrap()
                        .dyn_into::<web_sys::Element>()
                        .unwrap()
                        .parent_element()
                        .unwrap()
                        .get_bounding_client_rect();
                    wasm_bindgen_test::console_log!(
                        "guide geometry theme={theme} width={width}: card={}x{} left={} right={} dialog={}..{}",
                        rect.width(),
                        rect.height(),
                        rect.left(),
                        rect.right(),
                        bounds.left(),
                        bounds.right()
                    );
                    assert!(
                        rect.left() >= bounds.left()
                            && rect.right() <= bounds.right() + 1.0
                            && rect.height() >= 44.0
                    );
                }
                let contrast = js_sys::Function::new_with_args("root", r#"
                    const canvas = document.createElement('canvas'); canvas.width = 1; canvas.height = 1;
                    const ctx = canvas.getContext('2d');
                    const pixel = (color, base) => {
                        ctx.clearRect(0,0,1,1); ctx.fillStyle = base; ctx.fillRect(0,0,1,1);
                        ctx.fillStyle = color; ctx.fillRect(0,0,1,1); return Array.from(ctx.getImageData(0,0,1,1).data).slice(0,3);
                    };
                    const luminance = rgb => rgb.map(v => v/255).map(v => v <= .04045 ? v/12.92 : ((v+.055)/1.055)**2.4).reduce((a,v,i) => a+v*[.2126,.7152,.0722][i],0);
                    const label = root.querySelector('label');
                    const base = getComputedStyle(root).backgroundColor;
                    const bg = luminance(pixel(getComputedStyle(label).backgroundColor, base));
                    return Math.min(...['strong','small'].map(selector => {
                        const fg = luminance(pixel(getComputedStyle(label.querySelector(selector)).color, base));
                        return (Math.max(fg,bg)+.05)/(Math.min(fg,bg)+.05);
                    }));
                "#).call1(&wasm_bindgen::JsValue::NULL, dialog.as_ref()).unwrap().as_f64().unwrap();
                assert!(
                    contrast >= 4.5,
                    "choice text must meet WCAG AA in {theme}: {contrast}"
                );
                let next: HtmlElement = next.unchecked_into();
                next.focus().unwrap();
                let outline = web_sys::window()
                    .unwrap()
                    .get_computed_style(&next)
                    .unwrap()
                    .unwrap()
                    .get_property_value("outline-width")
                    .unwrap();
                assert_ne!(outline, "0px", "keyboard focus must be visible");
                let enable: HtmlElement = dialog
                    .query_selector("input[aria-label='Enable Claude']")
                    .unwrap()
                    .unwrap()
                    .dyn_into()
                    .unwrap();
                enable.focus().unwrap();
                assert!(is_focused(&enable));
                let slider = enable.next_element_sibling().unwrap();
                let rect = slider.get_bounding_client_rect();
                assert!(
                    rect.width() >= 30.0 && rect.height() >= 18.0,
                    "the focus indicator belongs to the visible switch target"
                );
                let style = web_sys::window()
                    .unwrap()
                    .get_computed_style(&slider)
                    .unwrap()
                    .unwrap();
                assert_ne!(style.get_property_value("outline-width").unwrap(), "0px");
                assert_ne!(style.get_property_value("outline-style").unwrap(), "none");
                assert_eq!(style.get_property_value("opacity").unwrap(), "1");
            }
        }
        state.connection_statuses.update(|statuses| {
            statuses.insert(
                "local".to_owned(),
                ConnectionStatus::Error("Host connection refused".to_owned()),
            );
        });
        next_tick().await;
        assert!(visible_text(&guide(&container)).contains("Host connection refused"));
        assert!(button(&guide(&container), "Continue to folder").disabled());
        assert!(button(&guide(&container), "Sign in in terminal").disabled());
        button(&guide(&container), "Open Backend settings").click();
        next_tick().await;
        assert!(
            container
                .query_selector("[aria-label='Getting started']")
                .unwrap()
                .is_none()
        );
        let settings: HtmlElement = container
            .query_selector(".settings-overlay")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(visible_text(&settings).contains("Enabled backends"));
        assert!(
            settings
                .query_selector("input[aria-label='Enable Claude']")
                .unwrap()
                .is_some()
        );
        settings
            .query_selector("button[title='Close settings']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert!(visible_text(&guide(&container)).contains("Connect an AI backend"));
        state.connection_statuses.update(|statuses| {
            statuses.insert("local".to_owned(), ConnectionStatus::Connected);
        });
        next_tick().await;
        button(&guide(&container), "Sign in in terminal").click();
        next_tick().await;
        assert!(!state.getting_started_open.get_untracked());
        crate::notices::report_send_failure(
            "local",
            &StreamPath("/host".to_owned()),
            protocol::FrameKind::RunBackendSetup,
            "The host connection closed.",
        );
        next_tick().await;
        let terminal: HtmlElement = container
            .query_selector(".terminal-view")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(visible_text(&terminal).contains("The host connection closed"));
        terminal
            .query_selector("button[aria-label='Dismiss notice']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        let commands = sent(&calls, "run_backend_setup");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["payload"]["action"], "sign_in");
        assert_eq!(commands[0]["payload"]["backend_kind"], "claude");
        assert_eq!(
            commands[0]["payload"].as_object().unwrap().len(),
            2,
            "only the typed operation, never credentials, crosses this boundary"
        );
        drop(handle);
        container.remove();
    }
}
