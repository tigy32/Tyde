use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::actions::{begin_new_chat_with_profile_at_home, begin_new_chat_with_profile_in_project};
use crate::state::{ActiveProjectRef, AppState, ConnectionStatus, DEFAULT_CUSTOM_AGENT_ID};

use protocol::{BackendKind, CustomAgent, CustomAgentId, LaunchProfileEntry, LaunchProfileKind};

fn backend_label_for(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Antigravity => "Antigravity",
        BackendKind::Hermes => "Hermes",
        BackendKind::Grok => "Grok",
        BackendKind::Opencode => "OpenCode",
        BackendKind::Muse => "Muse",
    }
}

/// The subtle backend tag shown on the right of a launch-profile row, or `None`
/// when it would be redundant. Backend-default rows never show it — their label
/// already *is* the backend name — so we don't render e.g. "Claude   CLAUDE".
/// Custom rows show it only when their label differs from the backend identity,
/// so a custom profile that happens to be named after its backend also stays
/// clean. Driven entirely by the server-provided `kind`, never inferred from
/// the label string.
fn backend_tag_for(
    kind: LaunchProfileKind,
    backend: BackendKind,
    label: &str,
) -> Option<&'static str> {
    match kind {
        LaunchProfileKind::BackendDefault => None,
        LaunchProfileKind::Custom => {
            let name = backend_label_for(backend);
            if label.trim().eq_ignore_ascii_case(name) {
                None
            } else {
                Some(name)
            }
        }
    }
}

pub(super) fn backend_icon(kind: BackendKind) -> impl IntoView {
    use base64::Engine;

    let (light, dark, media_type, backing): (&[u8], Option<&[u8]>, &str, Option<&str>) = match kind
    {
        BackendKind::Tycode => (
            include_bytes!("../../provider-icons/tycode.svg"),
            None,
            "image/svg+xml",
            Some("white"),
        ),
        BackendKind::Kiro => (
            include_bytes!("../../provider-icons/kiro.svg"),
            None,
            "image/svg+xml",
            None,
        ),
        BackendKind::Claude => (
            include_bytes!("../../provider-icons/claude.svg"),
            None,
            "image/svg+xml",
            None,
        ),
        BackendKind::Codex => (
            include_bytes!("../../provider-icons/openai-black.svg"),
            Some(include_bytes!("../../provider-icons/openai-white.svg")),
            "image/svg+xml",
            None,
        ),
        BackendKind::Antigravity => (
            include_bytes!("../../provider-icons/antigravity.svg"),
            None,
            "image/svg+xml",
            None,
        ),
        BackendKind::Hermes => (
            include_bytes!("../../provider-icons/hermes.svg"),
            None,
            "image/svg+xml",
            None,
        ),
        BackendKind::Grok => (
            include_bytes!("../../provider-icons/grok.png"),
            None,
            "image/png",
            Some("black"),
        ),
        BackendKind::Opencode => (
            include_bytes!("../../provider-icons/opencode-light.svg"),
            Some(include_bytes!("../../provider-icons/opencode-dark.svg")),
            "image/svg+xml",
            None,
        ),
        BackendKind::Muse => (
            include_bytes!("../../muse.svg"),
            None,
            "image/svg+xml",
            None,
        ),
    };
    let source = |bytes: &[u8]| {
        format!(
            "data:{media_type};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    };
    let light_source = source(light);
    let dark_source = dark.map(source);
    let themed = dark_source.is_some();
    view! {
        <svg class="launch-backend-icon" width="24" height="24" viewBox="0 0 24 24"
            fill="none" aria-hidden="true" focusable="false">
            // Select supplied colorways, never filter or recolor vendor artwork.
            <style>{".launch-backend-icon .provider-dark { display: none; } [data-theme=dark] .launch-backend-icon .provider-light { display: none; } [data-theme=dark] .launch-backend-icon .provider-dark { display: inline; }"}</style>
            {backing.map(|color| view! { <rect width="24" height="24" rx="2" fill=color /> })}
            <image class=if themed { "provider-light" } else { "" }
                href=light_source width="24" height="24" preserveAspectRatio="xMidYMid meet" />
            {dark_source.map(|source| view! {
                <image class="provider-dark" href=source width="24" height="24" preserveAspectRatio="xMidYMid meet" />
            })}
        </svg>
    }
}

#[component]
fn LaunchProjectPicker(
    selection: RwSignal<(Option<String>, Option<ActiveProjectRef>)>,
    label: Memo<String>,
    on_select: Callback<(Option<String>, Option<ActiveProjectRef>)>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let projects = state.projects;
    let open = RwSignal::new(false);
    let query = RwSignal::new(String::new());
    let storage = web_sys::window()
        .ok_or_else(|| "Browser window unavailable for workbench preferences.".to_owned())
        .and_then(|window| {
            window
                .local_storage()
                .map_err(|error| format!("Could not access workbench preferences: {error:?}"))
        })
        .and_then(|storage| {
            storage.ok_or_else(|| {
                "Allow browser storage to remember workbench preferences.".to_owned()
            })
        });
    let remembered = storage.as_ref().map_err(Clone::clone).and_then(|storage| {
        match storage.get_item("tyde.launch.show-workbenches") {
            Ok(None) => Ok(false),
            Ok(Some(value)) if value == "true" => Ok(true),
            Ok(Some(value)) if value == "false" => Ok(false),
            Ok(Some(_)) => Err("Saved workbench preference is invalid.".to_owned()),
            Err(error) => Err(format!("Could not read workbench preferences: {error:?}")),
        }
    });
    let show_workbenches = RwSignal::new(remembered);
    let storage = StoredValue::new_local(storage);
    let wrapper = NodeRef::<leptos::html::Div>::new();
    let trigger = NodeRef::<leptos::html::Button>::new();
    let search = NodeRef::<leptos::html::Input>::new();
    let close = Callback::new(move |()| {
        open.set(false);
        if let Some(trigger) = trigger.get_untracked() {
            let _ = trigger.focus();
        }
    });
    Effect::new(move |_| {
        if let Some(search) = search.get() {
            let _ = search.focus();
        }
    });
    let outside = window_event_listener(leptos::ev::pointerdown, move |event| {
        if open.get_untracked()
            && !wrapper.get_untracked().is_some_and(|wrapper| {
                event
                    .target()
                    .and_then(|target| target.dyn_into::<web_sys::Node>().ok())
                    .is_some_and(|target| wrapper.contains(Some(&target)))
            })
        {
            open.set(false);
        }
    });
    on_cleanup(move || outside.remove());
    let rows = Memo::new(move |_| {
        let host = selection.get().0;
        let projects: Vec<_> = projects
            .get()
            .into_iter()
            .filter(|entry| Some(&entry.host_id) == host.as_ref())
            .collect();
        let query = query.get().trim().to_lowercase();
        let mut rows = Vec::new();
        for parent in projects
            .iter()
            .filter(|entry| !entry.project.is_workbench())
        {
            let parent_matches = parent.project.name.to_lowercase().contains(&query);
            let children: Vec<_> = projects
                .iter()
                .filter(|child| {
                    child.project.parent_project_id() == Some(&parent.project.id)
                        && show_workbenches.get().is_ok_and(|shown| shown)
                        && (parent_matches || child.project.name.to_lowercase().contains(&query))
                })
                .collect();
            if parent_matches || !children.is_empty() {
                rows.push((parent.clone(), false));
                rows.extend(children.into_iter().map(|child| (child.clone(), true)));
            }
        }
        rows
    });
    view! {
        <div class="launch-project-picker" node_ref=wrapper
            on:focusout=move |ev| {
                // Only focus moving to an element outside closes the picker. macOS
                // WebKit never mouse-focuses buttons: pressing one blurs with no
                // related target, and closing then would unmount a choice before
                // its click or let the Project button's click reopen the picker.
                // Outside presses close above.
                let left = ev.related_target().and_then(|target| target.dyn_into::<web_sys::Node>().ok())
                    .is_some_and(|target| wrapper.get_untracked().is_some_and(|wrapper| !wrapper.contains(Some(&target))));
                if left { open.set(false); }
            }
            on:keydown=move |ev| {
                if !open.get_untracked() { return; }
                if ev.key() == "Escape" {
                    ev.prevent_default(); ev.stop_propagation(); close.run(());
                } else if ev.key() == "Enter" {
                    // Project search must never submit the launch dialog.
                    ev.stop_propagation();
                    if ev.target().and_then(|target| target.dyn_into::<web_sys::Element>().ok()).is_some_and(|target| target.tag_name() == "INPUT") { ev.prevent_default(); }
                } else if matches!(ev.key().as_str(), "ArrowDown" | "ArrowUp" | "Home" | "End") {
                    let Some(wrapper) = wrapper.get_untracked() else { return; };
                    let Ok(nodes) = wrapper.query_selector_all(".launch-project-options button") else { return; };
                    let buttons: Vec<_> = (0..nodes.length()).filter_map(|i| nodes.item(i)?.dyn_into::<web_sys::HtmlElement>().ok()).collect();
                    if buttons.is_empty() { return; }
                    let active = web_sys::window().and_then(|window| window.document()).and_then(|document| document.active_element());
                    let index = buttons.iter().position(|button| active.as_ref().is_some_and(|active| active.is_same_node(Some(button))));
                    if matches!(ev.key().as_str(), "Home" | "End") && index.is_none() { return; }
                    let next = match ev.key().as_str() {
                        "Home" => 0, "End" => buttons.len() - 1,
                        "ArrowUp" => index.map_or(buttons.len() - 1, |i| (i + buttons.len() - 1) % buttons.len()),
                        _ => index.map_or(0, |i| (i + 1) % buttons.len()),
                    };
                    ev.prevent_default(); ev.stop_propagation();
                    let _ = buttons[next].focus();
                    buttons[next].scroll_into_view_with_bool(false);
                }
            }>
            <label for="launch-project-select">"Project"</label>
            <button id="launch-project-select" type="button" class="settings-select" aria-label="Project"
                aria-expanded=move || open.get().to_string() aria-controls="launch-project-popover"
                aria-describedby="launch-project-value" title=move || label.get()
                node_ref=trigger on:keydown=move |ev| {
                    if !open.get_untracked() && matches!(ev.key().as_str(), "ArrowDown" | "ArrowUp") {
                        ev.prevent_default(); ev.stop_propagation(); query.set(String::new()); open.set(true);
                    }
                } on:click=move |_| { query.set(String::new()); open.update(|open| *open = !*open); }>
                <span id="launch-project-value">{move || label.get()}</span>
            </button>
            <Show when=move || open.get()>
                <div id="launch-project-popover" class="launch-project-popover" role="group" aria-label="Project choices"
                    on:mousedown=move |ev| {
                        // Presses inside the picker keep keyboard focus in it; macOS
                        // WebKit would otherwise strand focus on the page.
                        let in_search = ev.target().is_some_and(|target| search.get_untracked()
                            .is_some_and(|search| search.is_same_node(target.dyn_ref::<web_sys::Node>())));
                        if !in_search { ev.prevent_default(); }
                    }>
                    <input type="search" aria-label="Find a project" placeholder="Find a project…" node_ref=search
                        prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev)) />
                    <Show when=move || show_workbenches.get().is_ok()>
                        <div class="launch-workbench-toggle">
                            <span>"Show workbenches"</span>
                            <label class="settings-toggle">
                                <input type="checkbox" role="switch" aria-label="Show workbenches" prop:checked=move || show_workbenches.get().is_ok_and(|shown| shown)
                                    on:change=move |ev| {
                                        let checked = event_target_checked(&ev);
                                        let saved = storage.with_value(|storage| storage.as_ref().map_err(Clone::clone).and_then(|storage| {
                                            storage.set_item("tyde.launch.show-workbenches", if checked { "true" } else { "false" })
                                                .map(|()| checked).map_err(|error| format!("Could not save workbench preferences: {error:?}"))
                                        }));
                                        // A failed save removes this switch; keep keyboard focus
                                        // in the open picker.
                                        if saved.is_err() && let Some(search) = search.get_untracked() {
                                            let _ = search.focus();
                                        }
                                        show_workbenches.set(saved);
                                    } />
                                <span class="settings-toggle-slider"></span>
                            </label>
                        </div>
                    </Show>
                    {move || show_workbenches.get().err().map(|error| view! { <p class="launch-empty" role="alert">{error}</p> })}
                    <div class="launch-project-options">
                        <button type="button" aria-pressed=move || selection.get().1.is_none().to_string()
                            on:click=move |_| { on_select.run((selection.get_untracked().0, None)); close.run(()); }>
                            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d="m3 11 9-8 9 8M5 10v11h14V10M9 21v-7h6v7" /></svg><span>"No project / Home"</span>
                        </button>
                        {move || rows.get().into_iter().map(|(entry, child)| {
                            let target = ActiveProjectRef { host_id: entry.host_id, project_id: entry.project.id };
                            let selected = target.clone();
                            view! {
                                <button type="button" class:launch-workbench-option=child
                                    aria-pressed=move || (selection.get().1 == Some(selected.clone())).to_string()
                                    on:click=move |_| { on_select.run((Some(target.host_id.clone()), Some(target.clone()))); close.run(()); }>
                                    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d=if child { "M6 6v12m0-6c8 0 12 0 12-6M9 4a3 3 0 1 1-6 0 3 3 0 0 1 6 0ZM9 20a3 3 0 1 1-6 0 3 3 0 0 1 6 0ZM21 4a3 3 0 1 1-6 0 3 3 0 0 1 6 0Z" } else { "M3 5h7l2 3h9v13H3Z" } /></svg><span>{entry.project.name}</span>
                                </button>
                            }
                        }).collect_view()}
                        <Show when=move || rows.get().is_empty()><p class="launch-empty">"No matching projects."</p></Show>
                    </div>
                </div>
            </Show>
        </div>
    }
}

/// Where a New Agent dialog opens. A project names its own host; a chat with
/// no project still belongs to exactly one host, so that host travels with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchDestination {
    Project(ActiveProjectRef),
    NoProject { host_id: String },
}

#[component]
pub fn LaunchChooser(
    open_sig: RwSignal<bool>,
    #[prop(optional)] initial_backend: Option<BackendKind>,
    /// The destination the caller opened the dialog for. `Some(None)` means no
    /// particular destination: the dialog opens without a project on the chat
    /// context's host. Omitted, it opens on the active project.
    #[prop(optional)]
    destination: Option<Memo<Option<LaunchDestination>>>,
    #[prop(optional)] on_created: Option<Callback<()>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let destination = match destination {
        Some(destination) => destination.get_untracked(),
        None => state
            .active_project_ref_untracked()
            .map(LaunchDestination::Project),
    };
    let (host, chosen_project) = match destination {
        Some(LaunchDestination::Project(project)) => (Some(project.host_id.clone()), Some(project)),
        Some(LaunchDestination::NoProject { host_id }) => (Some(host_id), None),
        None => (state.chat_context_host_id_untracked(), None),
    };
    let selection = RwSignal::new((host.clone(), chosen_project));
    let selected_host = Memo::new(move |_| selection.get().0);
    let hosts = state.configured_hosts;
    let projects = state.projects;
    let project_available = Memo::new(move |_| {
        let (host, project) = selection.get();
        host.as_ref().is_some_and(|host| {
            hosts.with(|hosts| hosts.iter().any(|entry| &entry.id == host))
                && project.as_ref().is_none_or(|selected| {
                    &selected.host_id == host
                        && projects.with(|projects| {
                            projects.iter().any(|entry| {
                                entry.host_id == selected.host_id
                                    && entry.project.id == selected.project_id
                            })
                        })
                })
        })
    });
    let context_label = Memo::new(move |_| {
        let Some(selected) = selection.get().1 else {
            return "No project / Home".to_owned();
        };
        projects.with(|projects| {
            let Some(entry) = projects.iter().find(|entry| {
                entry.host_id == selected.host_id && entry.project.id == selected.project_id
            }) else {
                return "Project unavailable".to_owned();
            };
            match entry.project.parent_project_id() {
                Some(parent) => match projects.iter().find(|parent_entry| {
                    parent_entry.host_id == selected.host_id && &parent_entry.project.id == parent
                }) {
                    Some(parent) => format!("{} / {}", parent.project.name, entry.project.name),
                    None => "Workbench parent unavailable".to_owned(),
                },
                None => entry.project.name.clone(),
            }
        })
    });
    let entries_state = state.clone();
    let entries_host = selected_host;
    let entries = Memo::new(move |_| {
        entries_state.launch_profile_catalog.with(|catalogs| {
            entries_host
                .get()
                .as_ref()
                .and_then(|host| catalogs.get(host))
                .map(|catalog| catalog.entries.clone())
        })
    });
    let catalog_connection_state = state.clone();
    let missing_catalog_message = move || {
        let status = selected_host.get().and_then(|host| {
            catalog_connection_state
                .connection_statuses
                .with(|hosts| hosts.get(&host).cloned())
        });
        match status {
            Some(ConnectionStatus::Connected) => {
                "Waiting for this host's launch profiles.".to_owned()
            }
            Some(ConnectionStatus::Connecting | ConnectionStatus::Reconnecting { .. }) => {
                "Connecting to this host to load launch profiles.".to_owned()
            }
            Some(ConnectionStatus::Error(message)) => {
                format!("Launch profiles unavailable: {message}")
            }
            Some(ConnectionStatus::Disconnected) | None => {
                "Connect to this host to load launch profiles.".to_owned()
            }
        }
    };
    let initial_profile = state.launch_profile_catalog.with_untracked(|catalogs| {
        let catalog = catalogs.get(host.as_ref()?)?;
        if let Some(backend) = initial_backend {
            return catalog.entries.iter().find_map(|entry| match entry {
                LaunchProfileEntry::Ready { profile }
                    if profile.backend_kind == backend
                        && profile.kind == LaunchProfileKind::BackendDefault =>
                {
                    Some(profile.id.clone())
                }
                _ => None,
            });
        }
        let id = catalog.default_profile_id.as_ref()?;
        catalog.entries.iter().find_map(|entry| match entry {
            LaunchProfileEntry::Ready { profile } if &profile.id == id => Some(id.clone()),
            _ => None,
        })
    });
    let selected_profile = RwSignal::new(initial_profile);
    let role_override = RwSignal::new(None::<CustomAgentId>);
    let role_state = state.clone();
    let selected_agent = Memo::new(move |_| {
        role_override.get().or_else(|| {
            if selection.get().1.is_some() {
                return None;
            }
            role_state.launch_profile_catalog.with(|catalogs| {
                catalogs
                    .get(selected_host.get().as_ref()?)?
                    .home_custom_agent_id
                    .clone()
            })
        })
    });
    let backend_query = RwSignal::new(String::new());
    let role_query = RwSignal::new(String::new());
    let agents_state = state.clone();
    let agents_host = selected_host;
    let agents = Memo::new(move |_| {
        let mut agents: Vec<CustomAgent> = agents_state.custom_agents.with(|hosts| {
            agents_host
                .get()
                .as_ref()
                .and_then(|host| hosts.get(host))
                .map(|agents| agents.values().cloned().collect())
                .unwrap_or_default()
        });
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents
    });
    let ready_profile = Memo::new(move |_| {
        let id = selected_profile.get()?;
        entries.get()?.into_iter().find_map(|entry| match entry {
            LaunchProfileEntry::Ready { profile } if profile.id == id => Some(profile),
            _ => None,
        })
    });
    let role_available = Memo::new(move |_| {
        selected_agent
            .get()
            .is_none_or(|id| agents.get().iter().any(|agent| agent.id == id))
    });
    let role_name = Memo::new(move |_| match selected_agent.get() {
        None => "Default agent".to_owned(),
        Some(id) if id.0 == DEFAULT_CUSTOM_AGENT_ID => "Default agent".to_owned(),
        Some(id) => agents
            .get()
            .into_iter()
            .find(|agent| agent.id == id)
            .map(|agent| agent.name)
            .unwrap_or_else(|| "Role unavailable".to_owned()),
    });
    let default_description = Memo::new(move |_| {
        agents
            .get()
            .into_iter()
            .find(|agent| agent.id.0 == DEFAULT_CUSTOM_AGENT_ID)
            .map(|agent| agent.description)
            .unwrap_or_else(|| "Use this host's default agent".to_owned())
    });

    let origin_state = state.clone();
    let origin_host = origin_state.chat_context_host_id_untracked();
    Effect::new(move |_| {
        if origin_state.chat_context_host_id() != origin_host {
            open_sig.set(false);
        }
    });

    let project_catalogs = state.launch_profile_catalog;
    let change_destination =
        Callback::new(move |target: (Option<String>, Option<ActiveProjectRef>)| {
            if selection.get_untracked() == target {
                return;
            }
            let profile = project_catalogs.with_untracked(|catalogs| {
                catalogs.get(target.0.as_ref()?)?.default_profile_id.clone()
            });
            selection.set(target);
            selected_profile.set(profile);
            role_override.set(None);
        });

    let create = Callback::new(move |()| {
        if !open_sig.get_untracked()
            || !project_available.get_untracked()
            || !role_available.get_untracked()
        {
            return;
        }
        if let Some(profile) = ready_profile.get_untracked() {
            open_sig.set(false);
            match selection.get_untracked().1 {
                Some(project) => begin_new_chat_with_profile_in_project(
                    &state,
                    profile,
                    selected_agent.get_untracked(),
                    project,
                ),
                None => {
                    if let Some(host) = selected_host.get_untracked() {
                        let state = state.clone();
                        let agent = selected_agent.get_untracked();
                        wasm_bindgen_futures::spawn_local(async move {
                            match begin_new_chat_with_profile_at_home(&state, profile, agent, host)
                                .await
                            {
                                Ok(()) => {
                                    if let Some(on_created) = on_created {
                                        on_created.run(());
                                    }
                                }
                                Err(error) => crate::components::header::report_user_error(
                                    format!("Could not create agent: {error}"),
                                ),
                            }
                        });
                        return;
                    }
                }
            }
            if let Some(on_created) = on_created {
                on_created.run(());
            }
        }
    });
    let dialog_ref = NodeRef::<leptos::html::Div>::new();
    let search_ref = NodeRef::<leptos::html::Input>::new();
    let opener: StoredValue<Option<web_sys::HtmlElement>, LocalStorage> = StoredValue::new_local(
        web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.active_element())
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok()),
    );
    Effect::new(move |_| {
        if let Some(search) = search_ref.get() {
            let _ = search.focus();
        }
    });
    on_cleanup(move || {
        if let Some(opener) = opener.get_value() {
            let _ = opener.focus();
        }
    });
    let backdrop_press = RwSignal::new(false);
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        ev.stop_propagation();
        if ev.is_composing() {
            return;
        }
        match ev.key().as_str() {
            "Escape" => {
                ev.prevent_default();
                open_sig.set(false);
            }
            "Enter"
                if !ev.repeat()
                    && ev
                        .target()
                        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                        .is_some_and(|target| target.tag_name() == "INPUT") =>
            {
                ev.prevent_default();
                create.run(());
            }
            "Tab" => {
                let Some(dialog) = dialog_ref.get_untracked() else {
                    return;
                };
                let Ok(nodes) = dialog.query_selector_all(
                    "button:not([disabled]), input:not([disabled]), select:not([disabled])",
                ) else {
                    return;
                };
                let (Some(first), Some(last)) = (
                    nodes
                        .item(0)
                        .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok()),
                    nodes
                        .item(nodes.length().saturating_sub(1))
                        .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok()),
                ) else {
                    return;
                };
                let active = web_sys::window()
                    .and_then(|w| w.document())
                    .and_then(|d| d.active_element());
                if ev.shift_key()
                    && active
                        .as_ref()
                        .is_some_and(|el| el.is_same_node(Some(&first)))
                {
                    ev.prevent_default();
                    let _ = last.focus();
                } else if !ev.shift_key()
                    && active
                        .as_ref()
                        .is_some_and(|el| el.is_same_node(Some(&last)))
                {
                    ev.prevent_default();
                    let _ = first.focus();
                }
            }
            _ => {}
        }
    };

    view! {
        <div class="launch-chooser-overlay"
            on:pointerdown=move |ev| backdrop_press.set(ev.target() == ev.current_target())
            on:pointercancel=move |_| backdrop_press.set(false)
            on:click=move |ev| {
                if backdrop_press.get_untracked() && ev.target() == ev.current_target() { open_sig.set(false); }
                backdrop_press.set(false);
            }>
            <div class="launch-chooser" role="dialog" aria-modal="true" aria-label="New agent"
                node_ref=dialog_ref on:keydown=on_keydown>
                <header class="launch-chooser-header">
                    <div><h2>"New agent"</h2><p>"Choose a backend and an agent role."</p></div>
                    <button type="button" class="launch-chooser-close" aria-label="Close agent chooser"
                        on:click=move |_| open_sig.set(false)>"×"</button>
                    <div class="launch-destination-picker">
                        <div class="launch-host-picker">
                            <label for="launch-host-select">"Host"</label>
                            <select id="launch-host-select" class="settings-select" aria-label="Host"
                                on:change=move |ev| {
                                    let host = event_target_value(&ev);
                                    if hosts.with_untracked(|hosts| hosts.iter().any(|entry| entry.id == host)) {
                                        change_destination.run((Some(host), None));
                                    }
                                }>
                                <Show when=move || selected_host.get().as_ref().is_none_or(|id| !hosts.with(|hosts| hosts.iter().any(|host| &host.id == id)))><option selected=true disabled=true>"Host unavailable"</option></Show>
                                {move || hosts.get().into_iter().map(|host| {
                                    let label = hosts.with(|hosts| if hosts.iter().filter(|entry| entry.label == host.label).count() > 1 {
                                        format!("{} ({})", host.label, host.id)
                                    } else { host.label });
                                    let id = host.id.clone();
                                    view! { <option value=host.id selected=move || selected_host.get().as_ref() == Some(&id)>{label}</option> }
                                }).collect_view()}
                            </select>
                        </div>
                        <LaunchProjectPicker selection=selection label=context_label on_select=change_destination />
                    </div>
                </header>
                <div class="launch-chooser-body">
                    <section class="launch-backends" aria-label="Backend">
                        <h3>"Backend"</h3>
                        <input type="search" aria-label="Find a backend" placeholder="Find a backend…"
                            node_ref=search_ref prop:value=move || backend_query.get()
                            on:input=move |ev| backend_query.set(event_target_value(&ev)) />
                        <div class="launch-backend-list" role="group" aria-label="Backend choices">
                            {move || {
                                let Some(available) = entries.get() else {
                                    let message = missing_catalog_message();
                                    return view! { <p class="launch-empty" role="status">{message}</p> }.into_any();
                                };
                                let query = backend_query.get().trim().to_lowercase();
                                let catalog_empty = available.is_empty();
                                let filtered: Vec<_> = available.into_iter().filter(|entry| {
                                    let label = match entry { LaunchProfileEntry::Ready { profile } => &profile.label,
                                        LaunchProfileEntry::Unavailable { label, .. } => label };
                                    label.to_lowercase().contains(&query) || backend_label_for(entry.backend_kind()).to_lowercase().contains(&query)
                                }).collect();
                                if filtered.is_empty() {
                                    return view! { <p class="launch-empty">{if catalog_empty {
                                        "No launch profiles available. Enable a backend in Settings → Backends."
                                    } else { "No backends match your search." }}</p> }.into_any();
                                }
                                filtered.into_iter().map(|entry| match entry {
                                    LaunchProfileEntry::Ready { profile } => {
                                        let id = profile.id.clone();
                                        let pressed_id = id.clone();
                                        let tag = backend_tag_for(profile.kind, profile.backend_kind, &profile.label);
                                        view! {
                                            <button type="button" class="launch-backend-choice"
                                                aria-pressed=move || (selected_profile.get() == Some(pressed_id.clone())).to_string()
                                                title=profile.description on:click=move |_| selected_profile.set(Some(id.clone()))>
                                                {backend_icon(profile.backend_kind)}
                                                <span class="launch-choice-text"><span>{profile.label}</span>
                                                    {tag.map(|tag| view! { <small class="launch-backend-tag">{tag}</small> })}
                                                </span>
                                                <span class="launch-selection-mark" aria-hidden="true"></span>
                                            </button>
                                        }.into_any()
                                    }
                                    LaunchProfileEntry::Unavailable { kind, backend_kind, label, message, .. } => {
                                        let tag = backend_tag_for(kind, backend_kind, &label);
                                        let title = message.clone();
                                        view! {
                                            <div class="launch-backend-choice launch-backend-unavailable" aria-disabled="true" title=title>
                                                {backend_icon(backend_kind)}
                                                <span class="launch-choice-text"><span>{label}</span>
                                                    {tag.map(|tag| view! { <small class="launch-backend-tag">{tag}</small> })}
                                                    <small class="launch-unavailable-reason">{message}</small>
                                                </span>
                                            </div>
                                        }.into_any()
                                    }
                                }).collect_view().into_any()
                            }}
                        </div>
                    </section>
                    <section class="launch-roles" aria-label="Agent role">
                        <h3>"Agent role"</h3>
                        <input type="search" aria-label="Find a role" placeholder="Find a role…"
                            prop:value=move || role_query.get() on:input=move |ev| role_query.set(event_target_value(&ev)) />
                        <div class="launch-role-list" role="group" aria-label="Agent role choices">
                            <Show when=move || "default agent".contains(&role_query.get().trim().to_lowercase())
                                || default_description.get().to_lowercase().contains(&role_query.get().trim().to_lowercase())>
                                <button type="button" class="launch-role-choice" aria-pressed=move || selected_agent.get().is_none_or(|id| id.0 == DEFAULT_CUSTOM_AGENT_ID).to_string()
                                    title=move || default_description.get() on:click=move |_| role_override.set(Some(CustomAgentId(DEFAULT_CUSTOM_AGENT_ID.to_owned())))>
                                    <span class="launch-choice-text"><strong>"Default agent"</strong><small>{default_description}</small></span>
                                    <span class="launch-selection-mark" aria-hidden="true"></span>
                                </button>
                            </Show>
                            {move || {
                                let query = role_query.get().trim().to_lowercase();
                                agents.get().into_iter().filter(|agent| agent.id.0 != DEFAULT_CUSTOM_AGENT_ID
                                    && (agent.name.to_lowercase().contains(&query) || agent.description.to_lowercase().contains(&query)))
                                    .map(|agent| {
                                        let id = agent.id;
                                        let pressed_id = id.clone();
                                        let title = agent.description.clone();
                                        view! {
                                            <button type="button" class="launch-role-choice" title=title
                                                aria-pressed=move || (selected_agent.get() == Some(pressed_id.clone())).to_string()
                                                on:click=move |_| role_override.set(Some(id.clone()))>
                                                <span class="launch-choice-text"><strong>{agent.name}</strong><small>{agent.description}</small></span>
                                                <span class="launch-selection-mark" aria-hidden="true"></span>
                                            </button>
                                        }
                                    }).collect_view()
                            }}
                            <Show when=move || {
                                let query = role_query.get().trim().to_lowercase();
                                !("default agent".contains(&query) || default_description.get().to_lowercase().contains(&query))
                                    && !agents.get().iter().any(|agent| agent.id.0 != DEFAULT_CUSTOM_AGENT_ID
                                        && (agent.name.to_lowercase().contains(&query) || agent.description.to_lowercase().contains(&query)))
                            }><p class="launch-empty">"No roles match your search."</p></Show>
                        </div>
                    </section>
                </div>
                <footer class="launch-chooser-footer">
                    <div class="launch-summary" aria-live="polite">
                        <small>"Ready to create"</small>
                        <strong>{move || ready_profile.get().map(|profile| format!("{} · {}", profile.label, role_name.get()))
                            .unwrap_or_else(|| "Choose an available backend".to_owned())}</strong>
                        <small>{context_label}</small>
                        <Show when=move || !project_available.get()><small>"The selected project is no longer available."</small></Show>
                        <Show when=move || selected_profile.get().is_some() && ready_profile.get().is_none()><small>"The selected backend is no longer available."</small></Show>
                        <Show when=move || !role_available.get()><small>"The selected role is no longer available."</small></Show>
                    </div>
                    <div class="launch-footer-actions">
                        <div><button type="button" class="launch-cancel" on:click=move |_| open_sig.set(false)>"Cancel"</button>
                            <button type="button" class="launch-create" disabled=move || ready_profile.get().is_none() || !role_available.get() || !project_available.get()
                                on:click=move |_| create.run(())>"Create agent"</button></div>
                        <small>"Enter in search to create · Esc to close"</small>
                    </div>
                </footer>
            </div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::wasm_test_support::ScopedRoot;
    use leptos::mount::mount_to;
    use protocol::{
        CustomAgentId, LaunchProfile, LaunchProfileCatalog, LaunchProfileId, SessionSettingValue,
        SessionSettingsValues, ToolPolicy,
    };
    use std::collections::HashMap;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> ScopedRoot {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("launch-chooser-test-css")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("launch-chooser-test-css");
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
        ScopedRoot::new(container.dyn_into::<HtmlElement>().unwrap(), "launch-menu")
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

    /// Presses a control the way native macOS WebKit delivers a real pointer
    /// click: pointerdown/mousedown reach the element under the pointer;
    /// unless the page cancels mousedown, the focused field blurs with no
    /// related target because buttons and switches are never mouse-focusable
    /// there; the page renders; mouseup/click reach whatever is under the
    /// pointer at release.
    async fn press(control: &web_sys::Element) {
        let document = web_sys::window().unwrap().document().unwrap();
        let rect = control.get_bounding_client_rect();
        let (x, y) = (
            rect.left() + rect.width() / 2.0,
            rect.top() + rect.height() / 2.0,
        );
        let pressed = document
            .element_from_point(x as f32, y as f32)
            .filter(|hit| control.contains(Some(hit)))
            .expect("the pressed control is the topmost element under the pointer");
        let pointer = web_sys::PointerEventInit::new();
        pointer.set_bubbles(true);
        pointer.set_cancelable(true);
        pointer.set_pointer_id(1);
        pointer.set_pointer_type("mouse");
        pointer.set_is_primary(true);
        pointer.set_client_x(x as i32);
        pointer.set_client_y(y as i32);
        pressed
            .dispatch_event(
                &web_sys::PointerEvent::new_with_event_init_dict("pointerdown", &pointer).unwrap(),
            )
            .unwrap();
        let mouse = |kind: &str, target: &web_sys::Element| {
            let init = web_sys::MouseEventInit::new();
            init.set_bubbles(true);
            init.set_cancelable(true);
            init.set_client_x(x as i32);
            init.set_client_y(y as i32);
            target
                .dispatch_event(
                    &web_sys::MouseEvent::new_with_mouse_event_init_dict(kind, &init).unwrap(),
                )
                .unwrap()
        };
        let focus_moves = mouse("mousedown", &pressed);
        if focus_moves
            && let Some(focused) = document
                .active_element()
                .and_then(|focused| focused.dyn_into::<HtmlElement>().ok())
        {
            focused.blur().unwrap();
        }
        next_tick().await;
        let released = document
            .element_from_point(x as f32, y as f32)
            .expect("the release lands on a rendered element");
        mouse("mouseup", &released);
        if pressed.is_connected() {
            let mut target = released;
            while !target.contains(Some(&pressed)) {
                target = target.parent_element().unwrap();
            }
            mouse("click", &target);
        }
        next_tick().await;
    }

    fn visible_text(container: &HtmlElement) -> String {
        container.text_content().unwrap_or_default()
    }

    fn find_button_by_text(container: &HtmlElement, text: &str) -> Option<HtmlElement> {
        let nodes = container.query_selector_all("button").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlElement>().ok())
            .find(|btn| btn.text_content().unwrap_or_default().contains(text))
    }

    fn search(container: &HtmlElement, label: &str, value: &str) {
        let input = container
            .query_selector(&format!("input[aria-label='{label}']"))
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        input.set_value(value);
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
    }

    fn ready(id: &str, label: &str, backend: BackendKind) -> LaunchProfileEntry {
        ready_kind(id, label, backend, LaunchProfileKind::BackendDefault)
    }

    fn ready_kind(
        id: &str,
        label: &str,
        backend: BackendKind,
        kind: LaunchProfileKind,
    ) -> LaunchProfileEntry {
        LaunchProfileEntry::Ready {
            profile: LaunchProfile {
                id: LaunchProfileId(id.to_owned()),
                kind,
                label: label.to_owned(),
                description: None,
                backend_kind: backend,
                session_settings: SessionSettingsValues::default(),
            },
        }
    }

    fn set_catalog(state: &AppState, host_id: &str, entries: Vec<LaunchProfileEntry>) {
        state.configured_hosts.update(|hosts| {
            if !hosts.iter().any(|host| host.id == host_id) {
                hosts.push(crate::bridge::ConfiguredHost {
                    id: host_id.to_owned(),
                    label: host_id.to_owned(),
                    transport: crate::bridge::HostTransportConfig::LocalEmbedded,
                    auto_connect: true,
                });
            }
        });
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.connection_statuses.update(|hosts| {
            hosts.insert(
                host_id.to_owned(),
                crate::state::ConnectionStatus::Connected,
            );
        });
        state.host_settings_by_host.update(|hosts| {
            hosts.insert(
                host_id.to_owned(),
                settings_model::HostSettings {
                    workbenches: Default::default(),
                    enabled_backends: vec![BackendKind::Claude, BackendKind::Codex],
                    default_backend: Some(BackendKind::Claude),
                    ..Default::default()
                },
            );
        });
        state.session_schemas.update(|hosts| {
            let schemas = hosts.entry(host_id.to_owned()).or_default();
            for backend_kind in [BackendKind::Claude, BackendKind::Codex] {
                schemas.insert(
                    backend_kind,
                    protocol::SessionSchemaEntry::Ready {
                        schema: protocol::SessionSettingsSchema {
                            backend_kind,
                            model_resolutions: Default::default(),
                            fields: vec![protocol::SessionSettingField {
                                key: "model".to_owned(),
                                label: "Model".to_owned(),
                                description: None,
                                use_slider: false,
                                select_options_by_setting: None,
                                field_type: protocol::SessionSettingFieldType::Select {
                                    nullable: true,
                                    default: None,
                                    options: vec![
                                        protocol::SelectOption {
                                            value: "opus".to_owned(),
                                            label: "Selected Opus".to_owned(),
                                        },
                                        protocol::SelectOption {
                                            value: "updated-model".to_owned(),
                                            label: "Updated model".to_owned(),
                                        },
                                    ],
                                },
                            }],
                        },
                    },
                );
            }
        });
        state.launch_profile_catalog.update(|map| {
            map.insert(
                host_id.to_owned(),
                LaunchProfileCatalog {
                    home_custom_agent_id: None,
                    entries,
                    default_profile_id: None,
                    custom_profile_schemas: Vec::new(),
                },
            );
        });
    }

    fn track_rendered_tabs(state: &AppState) {
        let active_state = state.clone();
        let active = Memo::new(move |_| active_state.center_zone.with(|zone| zone.active_tab_id()));
        let lru_state = state.clone();
        // App installs this tracker; CenterZone reads it to mount active tabs.
        Effect::new(move |_| {
            if let Some(tab) = active.get() {
                lru_state.bump_tab_lru(tab);
            }
        });
    }

    fn mount_menu(container: &HtmlElement, state: &AppState, open: RwSignal<bool>) -> impl Drop {
        let state = state.clone();
        mount_to(container.clone(), move || {
            provide_context(state.clone());
            track_rendered_tabs(&state);
            view! {
                <crate::components::center_zone::CenterZone />
                <Show when=move || open.get()><LaunchChooser open_sig=open /></Show>
            }
        })
    }

    async fn assert_rendered_draft(container: &HtmlElement, backend: &str, model: &str) {
        next_tick().await;
        next_tick().await;
        assert!(
            container
                .query_selector("[role=tab][aria-selected=true][aria-label='New Chat']")
                .unwrap()
                .is_some(),
            "confirmation must open and activate a visible New Chat tab"
        );
        let textarea = container
            .query_selector("textarea")
            .unwrap()
            .expect("the new draft has a real composer");
        let rect = textarea.get_bounding_client_rect();
        assert!(
            rect.width() > 0.0 && rect.height() > 0.0,
            "the new draft composer must be visible"
        );
        let heading = format!("Session Settings ({backend})");
        let toggle = find_button_by_text(container, &heading)
            .expect("the draft shows the selected backend's settings");
        let accordion = toggle
            .closest(".session-settings-accordion")
            .unwrap()
            .unwrap();
        let controls_present = accordion.query_selector("select").unwrap().is_some();
        wasm_bindgen_test::console_log!(
            "Draft settings: controls_present_before_open={controls_present}"
        );
        // The real bar preserves expansion across backend changes. Clicking an
        // already-open bar hides the correct controls instead of exposing them.
        if !controls_present {
            toggle.click();
        }
        next_tick().await;
        let select = accordion
            .query_selector("select")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlSelectElement>()
            .unwrap();
        let selected = select.item(select.selected_index() as u32).unwrap();
        assert_eq!(
            selected.text_content().as_deref(),
            Some(model),
            "the selected model must be visibly rendered, not just stored"
        );
    }

    /// Ready profiles render as selectable rows with a backend badge, and
    /// unavailable entries stay visible with their server-provided reason —
    /// they are never silently dropped from the menu.
    #[wasm_bindgen_test]
    async fn renders_ready_profiles_and_unavailable_reason() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![
                ready("claude:default", "Claude", BackendKind::Claude),
                LaunchProfileEntry::Unavailable {
                    id: LaunchProfileId("hermes:codex".to_owned()),
                    kind: LaunchProfileKind::Custom,
                    backend_kind: BackendKind::Codex,
                    label: "Hermes · Codex".to_owned(),
                    message: "Codex CLI not installed".to_owned(),
                },
            ],
        );
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;

        let text = visible_text(&container);
        assert!(
            text.contains("Claude"),
            "ready profile label missing: {text}"
        );
        assert!(
            text.contains("Hermes · Codex"),
            "unavailable profile label missing: {text}"
        );
        assert!(
            text.contains("Codex CLI not installed"),
            "unavailable reason must be shown, not silently dropped: {text}"
        );
        assert!(
            find_button_by_text(&container, "Codex CLI not installed").is_none(),
            "unavailable entry must not be a clickable button"
        );
        assert!(
            find_button_by_text(&container, "Claude").is_some(),
            "ready profile must be a clickable button"
        );

        find_button_by_text(&container, "Claude").unwrap().click();
        next_tick().await;
        state.launch_profile_catalog.update(|catalogs| {
            catalogs.get_mut("local").unwrap().entries[0] = LaunchProfileEntry::Unavailable {
                id: LaunchProfileId("claude:default".to_owned()),
                kind: LaunchProfileKind::BackendDefault,
                backend_kind: BackendKind::Claude,
                label: "Claude".to_owned(),
                message: "Backend is no longer ready".to_owned(),
            };
        });
        next_tick().await;
        let create = find_button_by_text(&container, "Create agent").unwrap();
        assert!(
            create.has_attribute("disabled"),
            "a selected profile that becomes unavailable must not launch"
        );
        create.click();
        assert!(open.get_untracked());
        assert_eq!(
            state.composer_untracked().launch_profile_id.get_untracked(),
            None
        );
        assert!(visible_text(&container).contains("Backend is no longer ready"));
        state.selected_host_id.set(Some("other".to_owned()));
        next_tick().await;
        assert!(
            !open.get_untracked(),
            "changing hosts must retire choices from the previous host"
        );
    }

    #[wasm_bindgen_test]
    async fn project_picker_routes_catalogs_and_rejects_disappearing_selections() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("local-default", "Local Claude", BackendKind::Claude)],
        );
        set_catalog(
            &state,
            "remote",
            vec![ready("remote-default", "Remote Codex", BackendKind::Codex)],
        );
        state.selected_host_id.set(Some("local".into()));
        for host in ["local", "remote"] {
            state.custom_agents.update(|hosts| {
                let id = CustomAgentId(format!("{host}-role"));
                hosts.entry(host.into()).or_default().insert(
                    id.clone(),
                    CustomAgent {
                        id,
                        name: format!("{host} role"),
                        description: "Host-specific role".into(),
                        instructions: None,
                        skill_ids: vec![],
                        mcp_server_ids: vec![],
                        tool_policy: ToolPolicy::Unrestricted,
                    },
                );
            });
            state.projects.update(|projects| {
                projects.push(crate::state::ProjectInfo {
                    host_id: host.into(),
                    project: protocol::Project {
                        id: protocol::ProjectId("same-project-id".into()),
                        name: "Shared name".into(),
                        sort_order: 0,
                        source: protocol::ProjectSource::Standalone { roots: vec![] },
                    },
                })
            });
            state.launch_profile_catalog.update(|catalogs| {
                catalogs.get_mut(host).unwrap().default_profile_id =
                    Some(LaunchProfileId(format!("{host}-default")));
            });
        }
        state.configured_hosts.update(|hosts| {
            hosts.push(crate::bridge::ConfiguredHost {
                id: "unbootstrapped".into(),
                label: "Offline host".into(),
                transport: crate::bridge::HostTransportConfig::SshStdio {
                    ssh_destination: "qa-unavailable.invalid".into(),
                    lifecycle: crate::bridge::RemoteHostLifecycleConfig::Manual,
                },
                auto_connect: false,
            });
        });
        let local = ActiveProjectRef {
            host_id: "local".into(),
            project_id: protocol::ProjectId("same-project-id".into()),
        };
        let remote = ActiveProjectRef {
            host_id: "remote".into(),
            project_id: protocol::ProjectId("same-project-id".into()),
        };
        let storage = web_sys::window().unwrap().local_storage().unwrap().unwrap();
        let previous_preference = storage.get_item("tyde.launch.show-workbenches").unwrap();
        storage.remove_item("tyde.launch.show-workbenches").unwrap();
        let workbench = ActiveProjectRef {
            host_id: "local".into(),
            project_id: protocol::ProjectId("local-workbench".into()),
        };
        state.projects.update(|projects| {
            projects.push(crate::state::ProjectInfo {
                host_id: "local".into(),
                project: protocol::Project {
                    id: workbench.project_id.clone(),
                    name: "UI polish".into(),
                    sort_order: 0,
                    source: protocol::ProjectSource::GitWorkbench {
                        parent_project_id: local.project_id.clone(),
                        branch: protocol::GitBranchName("ui-polish".into()),
                        roots: vec![],
                        environment: Default::default(),
                    },
                },
            })
        });
        state.active_project.set(Some(workbench.clone()));
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;
        let picker = || {
            container
                .query_selector("button[aria-label='Project']")
                .unwrap()
                .unwrap()
                .dyn_into::<HtmlElement>()
                .unwrap()
        };
        // The selected trigger also contains the workbench name when discovery
        // is hidden. Only buttons in the open choices group are discoverable rows.
        let project_choice = |text: &str| {
            let nodes = container
                .query_selector_all("[role=group][aria-label='Project choices'] button")
                .unwrap();
            (0..nodes.length())
                .filter_map(|i| nodes.item(i)?.dyn_into::<HtmlElement>().ok())
                .find(|button| button.text_content().as_deref() == Some(text))
        };
        let choose_host = |host: &str| {
            let select = container
                .query_selector("select[aria-label='Host']")
                .unwrap()
                .unwrap()
                .dyn_into::<web_sys::HtmlSelectElement>()
                .unwrap();
            select.set_value(host);
            select
                .dispatch_event(&web_sys::Event::new("change").unwrap())
                .unwrap();
        };
        let choose = async |host: &str, project: Option<ActiveProjectRef>| {
            choose_host(host);
            next_tick().await;
            press(&picker()).await;
            let text = if project.is_some() {
                "Shared name"
            } else {
                "No project / Home"
            };
            press(&project_choice(text).unwrap()).await;
            assert_eq!(picker().text_content().as_deref(), Some(text));
            assert_eq!(
                picker().get_attribute("aria-expanded").as_deref(),
                Some("false"),
                "a pointer choice closes the popover"
            );
        };
        assert_eq!(
            picker().text_content().as_deref(),
            Some("Shared name / UI polish")
        );
        find_button_by_text(&container, "Create agent")
            .unwrap()
            .click();
        next_tick().await;
        assert!(!open.get_untracked());
        assert_eq!(
            state.active_project.get_untracked(),
            Some(workbench.clone()),
            "hidden active workbench remains the actual launch target"
        );
        assert_rendered_draft(&container, "Claude", "Auto").await;
        open.set(true);
        next_tick().await;
        picker().click();
        next_tick().await;
        assert!(
            project_choice("UI polish").is_none(),
            "workbench discovery defaults off without changing the active launch target"
        );
        assert_eq!(
            picker().text_content().as_deref(),
            Some("Shared name / UI polish"),
            "hiding discovery does not hide the selected destination"
        );
        let toggle = container
            .query_selector("[role=switch][aria-label='Show workbenches']")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        let switch = toggle.closest("label").unwrap().unwrap();
        press(&switch).await;
        assert_eq!(
            storage
                .get_item("tyde.launch.show-workbenches")
                .unwrap()
                .as_deref(),
            Some("true")
        );
        state.projects.update(|projects| {
            projects.extend((0..24).map(|index| crate::state::ProjectInfo {
                host_id: "local".into(),
                project: protocol::Project {
                    id: protocol::ProjectId(format!("extra-{index}")),
                    name: format!("Extra project {index:02}"),
                    sort_order: index,
                    source: protocol::ProjectSource::Standalone { roots: vec![] },
                },
            }));
        });
        next_tick().await;
        let options = project_choice("No project / Home")
            .unwrap()
            .parent_element()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        assert!(
            options.scroll_height() > options.client_height(),
            "large project lists scroll inside the constrained popover"
        );
        options.set_scroll_top(options.scroll_height());
        next_tick().await;
        let last = project_choice("Extra project 23")
            .unwrap()
            .get_bounding_client_rect();
        let bounds = options.get_bounding_client_rect();
        assert!(
            last.top() >= bounds.top() && last.bottom() <= bounds.bottom() + 1.0,
            "the final choice remains reachable inside the popover"
        );
        options.set_scroll_top(0);
        let child = project_choice("UI polish").unwrap();
        assert_eq!(child.get_attribute("aria-pressed").as_deref(), Some("true"));
        let search = container
            .query_selector("input[aria-label='Find a project']")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        search.focus().unwrap();
        for key in ["Home", "End"] {
            let init = web_sys::KeyboardEventInit::new();
            init.set_key(key);
            init.set_bubbles(true);
            init.set_cancelable(true);
            let event = web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init)
                .unwrap();
            search.dispatch_event(&event).unwrap();
            assert!(
                !event.default_prevented(),
                "text-editing keys retain their native behavior"
            );
            assert!(
                web_sys::window()
                    .unwrap()
                    .document()
                    .unwrap()
                    .active_element()
                    .unwrap()
                    .is_same_node(Some(&search))
            );
            assert_eq!(
                picker().text_content().as_deref(),
                Some("Shared name / UI polish")
            );
        }
        let down = web_sys::KeyboardEventInit::new();
        down.set_key("ArrowDown");
        down.set_bubbles(true);
        search
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &down)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .active_element()
                .unwrap()
                .text_content()
                .as_deref(),
            Some("No project / Home")
        );
        search.focus().unwrap();
        search.set_value("absent");
        search
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert!(project_choice("UI polish").is_none());
        assert_eq!(
            picker().text_content().as_deref(),
            Some("Shared name / UI polish")
        );
        press(&switch.previous_element_sibling().unwrap()).await;
        assert!(
            web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .active_element()
                .unwrap()
                .is_same_node(Some(&search)),
            "a press inside the open picker keeps keyboard focus in it"
        );
        assert_eq!(
            picker().get_attribute("aria-expanded").as_deref(),
            Some("true")
        );
        press(&switch).await;
        assert_eq!(
            picker().text_content().as_deref(),
            Some("Shared name / UI polish")
        );
        let storage_set = js_sys::eval("Storage.prototype.setItem").unwrap();
        js_sys::eval("Storage.prototype.setItem = function() { throw new DOMException('Storage denied', 'SecurityError'); }").unwrap();
        press(&switch).await;
        js_sys::Reflect::set(
            &js_sys::eval("Storage.prototype").unwrap(),
            &"setItem".into(),
            &storage_set,
        )
        .unwrap();
        assert!(visible_text(&container).contains("Could not save workbench preferences"));
        assert!(
            container
                .query_selector("[role=switch][aria-label='Show workbenches']")
                .unwrap()
                .is_none(),
            "a failed preference write must not present a successful switch"
        );
        assert_eq!(
            picker().text_content().as_deref(),
            Some("Shared name / UI polish")
        );
        let focused = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .active_element()
            .unwrap();
        assert!(
            focused.is_same_node(Some(&search)),
            "keyboard focus stays in the open picker when its switch disappears"
        );
        let escape = web_sys::KeyboardEventInit::new();
        escape.set_key("Escape");
        escape.set_bubbles(true);
        focused
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &escape)
                    .unwrap(),
            )
            .unwrap();
        next_tick().await;
        assert!(
            open.get_untracked(),
            "Escape closes the project popover, not the chooser"
        );
        assert_eq!(
            picker().get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        assert!(
            web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .active_element()
                .unwrap()
                .is_same_node(Some(&picker()))
        );
        let expanded = || picker().get_attribute("aria-expanded");
        press(&picker()).await;
        assert_eq!(expanded().as_deref(), Some("true"));
        press(&picker()).await;
        assert_eq!(
            expanded().as_deref(),
            Some("false"),
            "pressing the Project button closes the open picker"
        );
        press(&picker()).await;
        let heading = container
            .query_selector("[role=dialog] h2")
            .unwrap()
            .unwrap();
        press(&heading).await;
        assert_eq!(
            expanded().as_deref(),
            Some("false"),
            "a press outside closes the picker"
        );
        assert_eq!(
            picker().text_content().as_deref(),
            Some("Shared name / UI polish")
        );
        press(&picker()).await;
        press(&project_choice("Shared name").unwrap()).await;
        assert_eq!(
            picker().text_content().as_deref(),
            Some("Shared name"),
            "a pointer press selects the requested project"
        );
        assert_eq!(
            picker().get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
        assert!(
            web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .active_element()
                .unwrap()
                .is_same_node(Some(&picker())),
            "choosing returns focus to the Project control"
        );
        // Choosing a destination never navigates the current workspace.
        assert_eq!(state.active_project.get_untracked(), Some(workbench));
        state.active_project.set(Some(local.clone()));
        match previous_preference {
            Some(value) => storage
                .set_item("tyde.launch.show-workbenches", &value)
                .unwrap(),
            None => storage.remove_item("tyde.launch.show-workbenches").unwrap(),
        }
        choose_host("remote");
        next_tick().await;
        assert_eq!(
            picker().text_content().as_deref(),
            Some("No project / Home"),
            "changing hosts clears the old-host launch project even when IDs match"
        );
        picker().click();
        next_tick().await;
        assert!(
            project_choice("UI polish").is_none(),
            "other-host workbenches never leak into choices"
        );
        press(&project_choice("No project / Home").unwrap()).await;
        choose("local", Some(local.clone())).await;
        assert!(find_button_by_text(&container, "Local Claude").is_some());
        choose("unbootstrapped", None).await;
        next_tick().await;
        assert!(visible_text(&container).contains("Connect to this host to load launch profiles."));
        assert!(!visible_text(&container).contains("Enable a backend in Settings"));
        assert!(
            find_button_by_text(&container, "Create agent")
                .unwrap()
                .has_attribute("disabled")
        );
        assert_eq!(state.active_project.get_untracked(), Some(local.clone()));
        state.connection_statuses.update(|hosts| {
            hosts.insert("unbootstrapped".into(), ConnectionStatus::Connected);
        });
        next_tick().await;
        assert!(visible_text(&container).contains("Waiting for this host's launch profiles."));
        state.launch_profile_catalog.update(|hosts| {
            hosts.insert(
                "unbootstrapped".into(),
                LaunchProfileCatalog {
                    home_custom_agent_id: None,
                    entries: vec![],
                    default_profile_id: None,
                    custom_profile_schemas: vec![],
                },
            );
        });
        next_tick().await;
        assert!(visible_text(&container).contains("Enable a backend in Settings"));
        assert!(!visible_text(&container).contains("Waiting for this host's launch profiles."));
        let late_help = CustomAgentId("late-help".into());
        state.custom_agents.update(|hosts| {
            hosts.entry("unbootstrapped".into()).or_default().insert(
                late_help.clone(),
                CustomAgent {
                    id: late_help.clone(),
                    name: "Late Help".into(),
                    description: "Home role delivered with the catalog".into(),
                    instructions: None,
                    skill_ids: vec![],
                    mcp_server_ids: vec![],
                    tool_policy: ToolPolicy::Unrestricted,
                },
            );
        });
        state.launch_profile_catalog.update(|hosts| {
            let catalog = hosts.get_mut("unbootstrapped").unwrap();
            catalog.home_custom_agent_id = Some(late_help);
            catalog.entries = vec![ready("late-claude", "Late Claude", BackendKind::Claude)];
        });
        next_tick().await;
        assert_eq!(
            find_button_by_text(&container, "Late Help")
                .unwrap()
                .get_attribute("aria-pressed")
                .as_deref(),
            Some("true"),
            "Home reflects the canonical role when its catalog arrives"
        );
        find_button_by_text(&container, "Late Claude")
            .unwrap()
            .click();
        next_tick().await;
        assert!(visible_text(&container).contains("Late Claude · Late Help"));
        choose("remote", Some(remote.clone())).await;
        next_tick().await;
        assert!(find_button_by_text(&container, "Remote Codex").is_some());
        assert!(find_button_by_text(&container, "Local Claude").is_none());
        assert!(find_button_by_text(&container, "remote role").is_some());
        assert!(find_button_by_text(&container, "local role").is_none());
        assert_eq!(
            state.active_project.get_untracked(),
            Some(local.clone()),
            "Picking is not navigation"
        );
        state
            .projects
            .update(|projects| projects.retain(|project| project.host_id != "remote"));
        next_tick().await;
        assert!(visible_text(&container).contains("The selected project is no longer available."));
        assert!(
            find_button_by_text(&container, "Create agent")
                .unwrap()
                .has_attribute("disabled")
        );
        assert!(
            open.get_untracked(),
            "Invalid selection stays visible, never silently switches to Home"
        );
        state.projects.update(|projects| {
            projects.push(crate::state::ProjectInfo {
                host_id: "remote".into(),
                project: protocol::Project {
                    id: remote.project_id.clone(),
                    name: "Shared name".into(),
                    sort_order: 0,
                    source: protocol::ProjectSource::Standalone { roots: vec![] },
                },
            })
        });
        next_tick().await;
        find_button_by_text(&container, "Create agent")
            .unwrap()
            .click();
        next_tick().await;
        assert!(!open.get_untracked());
        assert_eq!(state.active_project.get_untracked(), Some(remote));
        assert_rendered_draft(&container, "Codex", "Auto").await;

        open.set(true);
        next_tick().await;
        choose("local", Some(local)).await;
        next_tick().await;
        assert!(find_button_by_text(&container, "Local Claude").is_some());
        state
            .launch_profile_catalog
            .update(|catalogs| catalogs.get_mut("local").unwrap().entries.clear());
        next_tick().await;
        assert!(visible_text(&container).contains("The selected backend is no longer available."));
        assert!(
            find_button_by_text(&container, "Create agent")
                .unwrap()
                .has_attribute("disabled")
        );
        struct RestoreInvoke(wasm_bindgen::JsValue);
        impl Drop for RestoreInvoke {
            fn drop(&mut self) {
                let core = js_sys::eval("window.__TAURI__.core").unwrap();
                js_sys::Reflect::set(&core, &"invoke".into(), &self.0).unwrap();
            }
        }
        let _restore = RestoreInvoke(js_sys::eval("window.__TAURI__.core.invoke").unwrap());
        let store = serde_json::json!({
            "hosts": state.configured_hosts.get_untracked(), "selected_host_id": "remote",
        });
        js_sys::eval(&format!("window.__TAURI__.core.invoke = (cmd) => cmd === 'set_selected_host' ? new Promise(resolve => window.__finishHomeHost = () => resolve({store})) : Promise.resolve()")).unwrap();
        choose("remote", None).await;
        next_tick().await;
        assert!(
            picker()
                .text_content()
                .unwrap()
                .contains("No project / Home")
        );
        assert!(find_button_by_text(&container, "Remote Codex").is_some());
        find_button_by_text(&container, "Create agent")
            .unwrap()
            .click();
        next_tick().await;
        assert!(
            js_sys::eval("typeof window.__finishHomeHost === 'function'")
                .unwrap()
                .as_bool()
                .unwrap(),
            "the explicit host handoff is held at its completion event"
        );
        state.launch_profile_catalog.update(|catalogs| {
            catalogs.get_mut("remote").unwrap().home_custom_agent_id =
                Some(CustomAgentId("remote-role".into()));
        });
        js_sys::eval("window.__finishHomeHost(); delete window.__finishHomeHost").unwrap();
        next_tick().await;
        assert!(!open.get_untracked());
        assert!(state.active_project.get_untracked().is_none());
        assert_eq!(
            state.composer_untracked().custom_agent_id.get_untracked(),
            None,
            "the confirmed Default role must not change during the host handoff"
        );
        assert_eq!(
            state.selected_host_id.get_untracked().as_deref(),
            Some("remote")
        );
        state.drop_draft_bound_to_another_host();
        assert_eq!(
            state
                .composer_untracked()
                .selection_host
                .get_untracked()
                .as_deref(),
            Some("remote")
        );
        assert_rendered_draft(&container, "Codex", "Auto").await;
    }

    /// Selecting a ready profile sets the draft launch profile id, backend, and
    /// session settings straight from the server-provided profile — no id
    /// parsing — and closes the menu.
    #[wasm_bindgen_test]
    async fn selecting_ready_profile_sets_draft_from_profile() {
        let container = make_container();
        let state = AppState::new();
        let mut settings = HashMap::new();
        settings.insert(
            "model".to_owned(),
            SessionSettingValue::String("opus".to_owned()),
        );
        let entry = LaunchProfileEntry::Ready {
            profile: LaunchProfile {
                id: LaunchProfileId("claude:default".to_owned()),
                kind: LaunchProfileKind::BackendDefault,
                label: "Claude".to_owned(),
                description: None,
                backend_kind: BackendKind::Claude,
                session_settings: SessionSettingsValues(settings.clone()),
            },
        };
        set_catalog(&state, "local", vec![entry]);
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;

        find_button_by_text(&container, "Claude")
            .expect("ready profile row")
            .click();
        next_tick().await;
        assert!(
            open.get_untracked(),
            "selection stays open until confirmation"
        );
        find_button_by_text(&container, "Create agent")
            .unwrap()
            .click();
        next_tick().await;

        assert_eq!(
            state.composer_untracked().launch_profile_id.get_untracked(),
            Some(LaunchProfileId("claude:default".to_owned())),
            "draft launch profile id must be set from the selected profile"
        );
        assert_eq!(
            state.composer_untracked().backend_override.get_untracked(),
            Some(BackendKind::Claude),
            "draft backend must come from the profile"
        );
        assert_eq!(
            state.composer_untracked().session_settings.get_untracked(),
            SessionSettingsValues(settings),
            "draft session settings must come from the profile"
        );
        assert!(
            !open.get_untracked(),
            "selecting a profile must close the menu"
        );
        assert_rendered_draft(&container, "Claude", "Selected Opus").await;
    }

    /// The two visible choices still compose the custom agent with its profile.
    #[wasm_bindgen_test]
    async fn custom_role_composes_with_profile() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );
        state.custom_agents.update(|map| {
            let host = map.entry("local".to_owned()).or_default();
            host.insert(
                CustomAgentId("reviewer".to_owned()),
                CustomAgent {
                    id: CustomAgentId("reviewer".to_owned()),
                    name: "Reviewer".to_owned(),
                    description: "Reviews code".to_owned(),
                    instructions: None,
                    skill_ids: Vec::new(),
                    mcp_server_ids: Vec::new(),
                    tool_policy: ToolPolicy::Unrestricted,
                },
            );
        });
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;

        // Roles are now visible without hover; the profile + role contract is unchanged.
        find_button_by_text(&container, "Claude").unwrap().click();
        let agent_btn = find_button_by_text(&container, "Reviewer")
            .expect("custom role must already be visible");
        agent_btn.click();
        let create = find_button_by_text(&container, "Create agent").unwrap();
        let disabled_before_flush = create.has_attribute("disabled");
        // Same-task programmatic clicks otherwise hit the still-disabled
        // confirmation button, unlike separate browser pointer events.
        next_tick().await;
        wasm_bindgen_test::console_log!(
            "Chooser confirmation: disabled_before_flush={}, disabled_after_flush={}",
            disabled_before_flush,
            create.has_attribute("disabled")
        );
        assert!(!create.has_attribute("disabled"));
        create.click();
        next_tick().await;

        assert_eq!(
            state.composer_untracked().custom_agent_id.get_untracked(),
            Some(CustomAgentId("reviewer".to_owned())),
            "selecting a custom agent must set the draft custom agent id"
        );
        assert_eq!(
            state.composer_untracked().launch_profile_id.get_untracked(),
            Some(LaunchProfileId("claude:default".to_owned())),
            "custom agent selection must keep the composed launch profile"
        );
        assert_eq!(
            state.composer_untracked().backend_override.get_untracked(),
            Some(BackendKind::Claude),
            "custom agent selection must keep the profile backend"
        );
        assert_rendered_draft(&container, "Claude", "Auto").await;
    }

    // The approved chooser removes hover flyouts. Their former ARIA and
    // alignment checks pinned that removed UI; preserve selection accessibility
    // and viewport containment against the actual two-pane rendered output.
    #[wasm_bindgen_test]
    async fn ready_row_exposes_selected_state_without_submenu_semantics() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;
        let row = find_button_by_text(&container, "Claude").unwrap();
        assert_eq!(row.get_attribute("aria-pressed").as_deref(), Some("false"));
        assert!(
            row.get_attribute("aria-haspopup").is_none(),
            "selection does not hide a submenu"
        );
        row.click();
        next_tick().await;
        assert_eq!(row.get_attribute("aria-pressed").as_deref(), Some("true"));
        assert!(
            container
                .query_selector("[role=dialog][aria-modal=true]")
                .unwrap()
                .is_some()
        );
    }

    #[wasm_bindgen_test]
    async fn backend_and_role_panes_stay_visible_inside_the_dialog() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;
        let dialog = container
            .query_selector("[role=dialog]")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect();
        let backend = container
            .query_selector("section[aria-label=Backend]")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect();
        let roles = container
            .query_selector("section[aria-label='Agent role']")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect();
        assert!(
            backend.right() <= roles.left() + 1.0,
            "choices must sit alongside each other, not hover over one another"
        );
        assert!(backend.left() >= dialog.left() && roles.right() <= dialog.right());
        for label in ["Claude", "Default agent"] {
            let rect = find_button_by_text(&container, label)
                .unwrap()
                .get_bounding_client_rect();
            assert!(
                rect.width() >= 44.0 && rect.height() >= 44.0,
                "choices need comfortable pointer targets"
            );
        }
    }

    /// When the host reports an empty catalog, the empty state guides the
    /// user to enable a backend rather than showing a dead end.
    #[wasm_bindgen_test]
    async fn empty_catalog_shows_actionable_copy() {
        let container = make_container();
        let state = AppState::new();
        // Without a received catalog, the DOM correctly asks to connect the host.
        // This flow guards the distinct server-reported empty catalog guidance.
        set_catalog(&state, "local", vec![]);
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;

        let text = visible_text(&container);
        assert!(
            text.contains("Settings → Backends"),
            "empty menu must point the user at enabling a backend: {text}"
        );
    }

    /// Unavailable profile rows expose disabled semantics for assistive
    /// technology instead of looking like a plain clickable row.
    #[wasm_bindgen_test]
    async fn unavailable_row_exposes_aria_disabled() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![LaunchProfileEntry::Unavailable {
                id: LaunchProfileId("hermes:codex".to_owned()),
                kind: LaunchProfileKind::Custom,
                backend_kind: BackendKind::Codex,
                label: "Hermes · Codex".to_owned(),
                message: "Codex CLI not installed".to_owned(),
            }],
        );
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;

        let disabled = container
            .query_selector("[aria-disabled=\"true\"]")
            .unwrap();
        assert!(
            disabled.is_some(),
            "unavailable profile row must expose disabled semantics"
        );
    }

    /// Backend and role choices compose only on confirmation and keep tracking
    /// live server catalogs, including same-id label and setting updates.
    #[wasm_bindgen_test]
    async fn eight_backends_and_roles_are_clickable_without_hover_or_early_launch() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![
                ready("kiro:default", "Kiro (ACP)", BackendKind::Kiro),
                ready("claude:default", "Claude", BackendKind::Claude),
                ready("codex:default", "Codex", BackendKind::Codex),
                ready(
                    "antigravity:default",
                    "Antigravity",
                    BackendKind::Antigravity,
                ),
                ready("hermes:default", "Hermes", BackendKind::Hermes),
                ready("grok:default", "Grok", BackendKind::Grok),
                ready("opencode:default", "OpenCode", BackendKind::Opencode),
                ready_kind(
                    "hermes:local",
                    "Hermes — qwen-local",
                    BackendKind::Hermes,
                    LaunchProfileKind::Custom,
                ),
            ],
        );
        state.custom_agents.update(|map| {
            let agents = map.entry("local".to_owned()).or_default();
            for name in [
                "AXDB Ops",
                "Code Reviewer",
                "Help",
                "Implementer",
                "Lander",
                "Orchestrator",
                "Project Manager",
            ] {
                let id = CustomAgentId(name.to_owned());
                agents.insert(
                    id.clone(),
                    CustomAgent {
                        id,
                        name: name.to_owned(),
                        description: format!("{name} role"),
                        instructions: None,
                        skill_ids: Vec::new(),
                        mcp_server_ids: Vec::new(),
                        tool_policy: ToolPolicy::Unrestricted,
                    },
                );
            }
        });
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;

        // The live desktop's 1280x784 content viewport clipped the final
        // backend target by 23px. Match that layout box in the DOM fixture.
        container
            .query_selector(".launch-chooser-overlay")
            .unwrap()
            .unwrap()
            .set_attribute(
                "style",
                "width:1280px;height:784px;inset:auto;top:0;left:0;",
            )
            .unwrap();
        next_tick().await;
        // The searchable picker uses a button with the same themed control;
        // requiring a native select rejects the approved rendered UI.
        let picker = container
            .query_selector("button[aria-label='Project']")
            .unwrap()
            .unwrap();
        let picker_style = web_sys::window()
            .unwrap()
            .get_computed_style(&picker)
            .unwrap()
            .unwrap();
        assert_eq!(
            picker_style.get_property_value("appearance").unwrap(),
            "none"
        );
        assert_ne!(
            picker_style.get_property_value("background-image").unwrap(),
            "none",
            "The project dropdown paints Tyde's existing chevron"
        );
        assert!(
            picker.get_bounding_client_rect().height() >= 32.0,
            "The project dropdown retains the app's normal control height"
        );
        let backend_list = container
            .query_selector("[aria-label='Backend choices']")
            .unwrap()
            .unwrap();
        wasm_bindgen_test::console_log!(
            "Chooser backend layout: visible_height={}, content_height={}",
            backend_list.client_height(),
            backend_list.scroll_height()
        );
        assert_eq!(
            backend_list.client_height(),
            backend_list.scroll_height(),
            "all eight backend targets must fit completely at the standard desktop size"
        );

        assert_eq!(
            container
                .query_selector_all("[aria-label='Backend choices'] button svg")
                .unwrap()
                .length(),
            8
        );
        assert_eq!(
            container
                .query_selector_all("[aria-label='Agent role choices'] button")
                .unwrap()
                .length(),
            8
        );
        assert_eq!(
            container
                .query_selector_all("[aria-label='Agent role choices'] svg")
                .unwrap()
                .length(),
            0
        );
        search(&container, "Find a backend", "qwen-local");
        search(&container, "Find a role", "Implementer");
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all("[aria-label='Backend choices'] button")
                .unwrap()
                .length(),
            1
        );
        assert_eq!(
            container
                .query_selector_all("[aria-label='Agent role choices'] button")
                .unwrap()
                .length(),
            1
        );
        search(&container, "Find a backend", "");
        search(&container, "Find a role", "");
        next_tick().await;
        let implementer = find_button_by_text(&container, "Implementer")
            .expect("roles must be visible without hovering over a backend");
        implementer.click();
        find_button_by_text(&container, "Codex").unwrap().click();
        next_tick().await;
        assert!(
            open.get_untracked(),
            "choosing options must not launch or dismiss"
        );
        assert_eq!(
            state.composer_untracked().launch_profile_id.get_untracked(),
            None,
            "backend clicks must not create a draft before confirmation"
        );
        state.launch_profile_catalog.update(|catalogs| {
            let catalog = catalogs.get_mut("local").unwrap();
            let entry = catalog
                .entries
                .iter_mut()
                .find(|entry| entry.id().0 == "codex:default")
                .unwrap();
            if let LaunchProfileEntry::Ready { profile } = entry {
                profile.label = "Codex updated".to_owned();
                profile.session_settings.0.insert(
                    "model".to_owned(),
                    SessionSettingValue::String("updated-model".to_owned()),
                );
            }
        });
        state.custom_agents.update(|hosts| {
            hosts
                .get_mut("local")
                .unwrap()
                .get_mut(&CustomAgentId("Implementer".to_owned()))
                .unwrap()
                .name = "Updated implementer".to_owned();
        });
        next_tick().await;
        assert!(
            find_button_by_text(&container, "Codex updated").is_some(),
            "same-id catalog updates must render live"
        );
        assert!(
            find_button_by_text(&container, "Updated implementer").is_some(),
            "same-id role updates must render live"
        );
        find_button_by_text(&container, "Create agent")
            .unwrap()
            .click();
        next_tick().await;
        assert_eq!(
            state.composer_untracked().launch_profile_id.get_untracked(),
            Some(LaunchProfileId("codex:default".to_owned()))
        );
        assert_eq!(
            state.composer_untracked().custom_agent_id.get_untracked(),
            Some(CustomAgentId("Implementer".to_owned()))
        );
        assert_eq!(
            state
                .composer_untracked()
                .session_settings
                .get_untracked()
                .0
                .get("model"),
            Some(&SessionSettingValue::String("updated-model".to_owned())),
            "confirmation must use current server-owned profile settings, not a stale snapshot"
        );
        assert!(
            !open.get_untracked(),
            "only confirmation closes the chooser"
        );
        assert_rendered_draft(&container, "Codex", "Updated model").await;
        container
            .query_selector("button[title='New Chat']:not([role=tab])")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        assert_rendered_draft(&container, "Claude", "Auto").await;
        assert_eq!(
            state.composer_untracked().custom_agent_id.get_untracked(),
            None,
            "one-click New Chat must reset a previously chosen custom role"
        );
        assert_eq!(
            state.composer_untracked().launch_profile_id.get_untracked(),
            None,
            "without a catalog default, the primary button leaves resolution to the server"
        );
    }

    #[wasm_bindgen_test]
    async fn primary_new_chat_honors_ready_default_profile() {
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );
        state.launch_profile_catalog.update(|map| {
            if let Some(catalog) = map.get_mut("local") {
                catalog.default_profile_id = Some(LaunchProfileId("claude:default".to_owned()));
            }
        });

        state.connection_statuses.update(|statuses| {
            statuses.insert(
                "local".to_owned(),
                crate::state::ConnectionStatus::Connected,
            );
        });
        let container = make_container();
        let mount_state = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            track_rendered_tabs(&mount_state);
            view! { <crate::components::center_zone::CenterZone /> }
        });
        next_tick().await;
        find_button_by_text(&container, "New Chat").unwrap().click();
        next_tick().await;
        assert!(
            web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .query_selector("[role=dialog][aria-label='New agent']")
                .unwrap()
                .is_none(),
            "primary button opens a draft directly without the portaled chooser"
        );
        assert_eq!(
            state.composer_untracked().custom_agent_id.get_untracked(),
            None,
            "primary button retains the default agent"
        );

        assert_eq!(
            state.composer_untracked().launch_profile_id.get_untracked(),
            Some(LaunchProfileId("claude:default".to_owned())),
            "primary button must preselect the server default profile"
        );
        assert_eq!(
            state.composer_untracked().backend_override.get_untracked(),
            Some(BackendKind::Claude),
            "primary button must use the default profile's backend"
        );

        assert_rendered_draft(&container, "Claude", "Auto").await;
        let arrow = container
            .query_selector("button[aria-label='Choose backend and agent role']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        arrow.focus().unwrap();
        arrow.click();
        next_tick().await;
        let document = web_sys::window().unwrap().document().unwrap();
        let dialog = document
            .query_selector("[role=dialog][aria-label='New agent']")
            .unwrap()
            .expect("only the adjacent arrow opens the chooser");
        assert_eq!(
            dialog
                .query_selector_all("button[aria-pressed=true]")
                .unwrap()
                .length(),
            2,
            "the server default profile and default role start selected"
        );
        let close = dialog
            .query_selector("button[aria-label='Close agent chooser']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        close.focus().unwrap();
        let tab = web_sys::KeyboardEventInit::new();
        tab.set_key("Tab");
        tab.set_shift_key(true);
        tab.set_bubbles(true);
        tab.set_cancelable(true);
        close
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &tab)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            document.active_element().unwrap().text_content().as_deref(),
            Some("Create agent"),
            "keyboard focus wraps inside the dialog"
        );
        let escape = web_sys::KeyboardEventInit::new();
        escape.set_key("Escape");
        escape.set_bubbles(true);
        escape.set_cancelable(true);
        document
            .active_element()
            .unwrap()
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &escape)
                    .unwrap(),
            )
            .unwrap();
        next_tick().await;
        assert!(
            document
                .query_selector("[role=dialog][aria-label='New agent']")
                .unwrap()
                .is_none()
        );
        assert!(
            document
                .active_element()
                .unwrap()
                .is_same_node(Some(&arrow)),
            "closing returns focus to the chooser trigger"
        );
    }

    /// With no server `default_profile_id`, the primary button opens an
    /// ordinary draft: no profile, no backend override (the server resolves its
    /// own default at spawn time). No inference from the catalog contents.
    #[wasm_bindgen_test]
    async fn primary_new_chat_without_default_leaves_profile_unset() {
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );

        state.connection_statuses.update(|statuses| {
            statuses.insert(
                "local".to_owned(),
                crate::state::ConnectionStatus::Connected,
            );
        });
        let container = make_container();
        let mount_state = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            track_rendered_tabs(&mount_state);
            view! { <crate::components::center_zone::CenterZone /> }
        });
        next_tick().await;
        find_button_by_text(&container, "New Chat").unwrap().click();
        next_tick().await;
        assert!(
            web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .query_selector("[role=dialog][aria-label='New agent']")
                .unwrap()
                .is_none(),
            "primary button opens a draft directly without the portaled chooser"
        );
        assert_eq!(
            state.composer_untracked().custom_agent_id.get_untracked(),
            None,
            "primary button retains the default agent"
        );

        assert_eq!(
            state.composer_untracked().launch_profile_id.get_untracked(),
            None,
            "no server default → no profile preselected"
        );
        assert_eq!(
            state.composer_untracked().backend_override.get_untracked(),
            None,
            "no server default → no backend override; server resolves its default"
        );
        assert_rendered_draft(&container, "Claude", "Auto").await;
    }

    #[wasm_bindgen_test]
    async fn home_help_default_preserves_explicit_choices_and_project_defaults() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );
        let help_id = CustomAgentId(protocol::OPERATOR_CUSTOM_AGENT_ID.to_owned());
        state.launch_profile_catalog.update(|catalogs| {
            let catalog = catalogs.get_mut("local").unwrap();
            catalog.home_custom_agent_id = Some(help_id.clone());
            catalog.default_profile_id = Some(LaunchProfileId("claude:default".to_owned()));
        });
        state.custom_agents.update(|hosts| {
            hosts.entry("local".to_owned()).or_default().insert(
                CustomAgentId(DEFAULT_CUSTOM_AGENT_ID.to_owned()),
                CustomAgent {
                    id: CustomAgentId(DEFAULT_CUSTOM_AGENT_ID.to_owned()),
                    name: "Default".to_owned(),
                    description: "My project defaults".to_owned(),
                    instructions: Some("Preserve these project defaults".to_owned()),
                    skill_ids: Vec::new(),
                    mcp_server_ids: Vec::new(),
                    tool_policy: ToolPolicy::Unrestricted,
                },
            );
            hosts.entry("local".to_owned()).or_default().insert(
                help_id.clone(),
                CustomAgent {
                    id: help_id.clone(),
                    name: "My setup guide".to_owned(),
                    description: "My customized Help".to_owned(),
                    instructions: Some("Keep my preferences".to_owned()),
                    skill_ids: Vec::new(),
                    mcp_server_ids: Vec::new(),
                    tool_policy: ToolPolicy::Unrestricted,
                },
            );
        });
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;
        assert_eq!(
            find_button_by_text(&container, "My setup guide")
                .unwrap()
                .get_attribute("aria-pressed")
                .as_deref(),
            Some("true"),
            "Home chooses the server-provided canonical preset, keeping its customized name"
        );
        assert_eq!(
            find_button_by_text(&container, "Default agent")
                .unwrap()
                .get_attribute("aria-pressed")
                .as_deref(),
            Some("false")
        );
        find_button_by_text(&container, "Create agent")
            .unwrap()
            .click();
        next_tick().await;
        assert_eq!(
            state.composer_untracked().custom_agent_id.get_untracked(),
            Some(help_id.clone())
        );
        assert_rendered_draft(&container, "Claude", "Auto").await;

        open.set(true);
        next_tick().await;
        find_button_by_text(&container, "Default agent")
            .unwrap()
            .click();
        next_tick().await;
        find_button_by_text(&container, "Create agent")
            .unwrap()
            .click();
        next_tick().await;
        assert_eq!(
            state.composer_untracked().custom_agent_id.get_untracked(),
            Some(CustomAgentId(DEFAULT_CUSTOM_AGENT_ID.to_owned())),
            "An explicit Default choice must not be mistaken for an unspecified Home role"
        );
        crate::actions::begin_new_chat_default(&state);
        next_tick().await;
        assert_eq!(
            state.composer_untracked().custom_agent_id.get_untracked(),
            Some(help_id)
        );

        let project = ActiveProjectRef {
            host_id: "local".to_owned(),
            project_id: protocol::ProjectId("approved-project".to_owned()),
        };
        state.projects.update(|projects| {
            projects.push(crate::state::ProjectInfo {
                host_id: project.host_id.clone(),
                project: protocol::Project {
                    id: project.project_id.clone(),
                    name: "Approved project".into(),
                    sort_order: 0,
                    source: protocol::ProjectSource::Standalone { roots: vec![] },
                },
            })
        });
        state.switch_active_project(Some(project));
        open.set(true);
        next_tick().await;
        assert_eq!(
            find_button_by_text(&container, "Default agent")
                .unwrap()
                .get_attribute("aria-pressed")
                .as_deref(),
            Some("true")
        );
        find_button_by_text(&container, "Create agent")
            .unwrap()
            .click();
        next_tick().await;
        assert_eq!(
            state.composer_untracked().custom_agent_id.get_untracked(),
            None,
            "Project launches retain the ordinary Default"
        );
        assert!(
            !open.get_untracked(),
            "Project launch must actually create the draft"
        );
        assert_rendered_draft(&container, "Claude", "Auto").await;
        container.remove();
    }

    fn row_button_text(container: &HtmlElement) -> String {
        container
            .query_selector("[aria-label=\"Backend choices\"] button")
            .unwrap()
            .expect("ready profile row button")
            .dyn_into::<HtmlElement>()
            .unwrap()
            .text_content()
            .unwrap_or_default()
    }

    fn count_occurrences(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    /// A backend-default profile's label already *is* the backend name, so the
    /// row must not also render a backend badge — the name appears exactly once,
    /// never doubled up as "Claude … Claude". Suppression is driven by the typed
    /// `LaunchProfileKind::BackendDefault`, not by comparing label strings.
    #[wasm_bindgen_test]
    async fn backend_default_row_shows_backend_name_once() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready_kind(
                "claude:default",
                "Claude",
                BackendKind::Claude,
                LaunchProfileKind::BackendDefault,
            )],
        );
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;

        let text = row_button_text(&container);
        assert_eq!(
            count_occurrences(&text, "Claude"),
            1,
            "backend-default row must show the backend name exactly once: {text:?}"
        );
    }

    /// A custom profile whose label differs from its backend keeps a subtle
    /// backend tag, so the user can still see which backend it runs on even
    /// though the label doesn't say so.
    #[wasm_bindgen_test]
    async fn custom_profile_shows_label_and_backend_tag() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready_kind(
                "team:reviewer",
                "My Reviewer",
                BackendKind::Claude,
                LaunchProfileKind::Custom,
            )],
        );
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;

        let text = row_button_text(&container);
        assert!(
            text.contains("My Reviewer"),
            "custom profile label must render: {text:?}"
        );
        assert!(
            text.contains("Claude"),
            "custom profile keeps a subtle backend tag when its label differs \
             from the backend: {text:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn backend_icons_replace_chevrons_and_roles_remain_text_only() {
        let container = make_container();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![
                ready("tycode:default", "Tycode", BackendKind::Tycode),
                ready("kiro:default", "Kiro", BackendKind::Kiro),
                ready("claude:default", "Claude", BackendKind::Claude),
                ready("codex:default", "Codex", BackendKind::Codex),
                ready(
                    "antigravity:default",
                    "Antigravity",
                    BackendKind::Antigravity,
                ),
                ready("hermes:default", "Hermes", BackendKind::Hermes),
                ready("grok:default", "Grok", BackendKind::Grok),
                ready("opencode:default", "OpenCode", BackendKind::Opencode),
                ready("muse:default", "Muse", BackendKind::Muse),
                ready_kind(
                    "muse:custom",
                    "Code helper",
                    BackendKind::Muse,
                    LaunchProfileKind::Custom,
                ),
            ],
        );
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;
        let backend = find_button_by_text(&container, "Claude").unwrap();
        let icon = backend
            .query_selector("svg[aria-hidden=true]")
            .unwrap()
            .unwrap();
        let rect = icon.get_bounding_client_rect();
        assert!(
            rect.width() >= 20.0 && rect.height() >= 20.0,
            "backend identity has a visible icon"
        );
        let root = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .document_element()
            .unwrap();
        let previous_theme = root.get_attribute("data-theme");
        for theme in ["light", "dark"] {
            root.set_attribute("data-theme", theme).unwrap();
            for label in [
                "Tycode",
                "Kiro",
                "Claude",
                "Codex",
                "Antigravity",
                "Hermes",
                "Grok",
                "OpenCode",
                "Muse",
            ] {
                let row = find_button_by_text(&container, label).unwrap();
                let mark = row
                    .query_selector("svg[aria-hidden=true]")
                    .unwrap()
                    .unwrap();
                mark.set_attribute("style", "width:18px;height:18px")
                    .unwrap();
                let bounds = mark.get_bounding_client_rect();
                assert_eq!((bounds.width(), bounds.height()), (18.0, 18.0));
                let images = mark.query_selector_all("image").unwrap();
                let mut visible = 0;
                for index in 0..images.length() {
                    let image = images
                        .item(index)
                        .unwrap()
                        .dyn_into::<web_sys::Element>()
                        .unwrap();
                    let style = web_sys::window()
                        .unwrap()
                        .get_computed_style(&image)
                        .unwrap()
                        .unwrap();
                    if style.get_property_value("display").unwrap() == "none" {
                        continue;
                    }
                    visible += 1;
                    let source = image.get_attribute("href").unwrap();
                    assert!(
                        source.starts_with("data:image/"),
                        "{label} must render offline"
                    );
                    assert_eq!(
                        image.get_attribute("preserveAspectRatio").as_deref(),
                        Some("xMidYMid meet")
                    );
                    let painted_bounds = image.get_bounding_client_rect();
                    assert_eq!(
                        (painted_bounds.width(), painted_bounds.height()),
                        (18.0, 18.0),
                        "{label}'s mounted image must occupy its slot"
                    );
                    assert_eq!(style.get_property_value("visibility").unwrap(), "visible");
                    assert!(
                        style
                            .get_property_value("opacity")
                            .unwrap()
                            .parse::<f64>()
                            .unwrap()
                            > 0.0
                    );
                    let snapshot = mark
                        .clone_node_with_deep(true)
                        .unwrap()
                        .dyn_into::<web_sys::Element>()
                        .unwrap();
                    let snapshot_images = snapshot.query_selector_all("image").unwrap();
                    for other in (0..snapshot_images.length()).rev() {
                        if other != index {
                            snapshot_images
                                .item(other)
                                .unwrap()
                                .dyn_into::<web_sys::Element>()
                                .unwrap()
                                .remove();
                        }
                    }
                    snapshot.query_selector("style").unwrap().unwrap().remove();
                    let markup = snapshot.outer_html().replacen(
                        "<svg ",
                        "<svg xmlns=\"http://www.w3.org/2000/svg\" ",
                        1,
                    );
                    let decoded = web_sys::HtmlImageElement::new().unwrap();
                    decoded.set_src(&format!(
                        "data:image/svg+xml,{}",
                        String::from(js_sys::encode_uri_component(&markup))
                    ));
                    wasm_bindgen_futures::JsFuture::from(decoded.decode())
                        .await
                        .unwrap();
                    let canvas = web_sys::window()
                        .unwrap()
                        .document()
                        .unwrap()
                        .create_element("canvas")
                        .unwrap()
                        .dyn_into::<web_sys::HtmlCanvasElement>()
                        .unwrap();
                    canvas.set_width(18);
                    canvas.set_height(18);
                    let context = canvas
                        .get_context("2d")
                        .unwrap()
                        .unwrap()
                        .dyn_into::<web_sys::CanvasRenderingContext2d>()
                        .unwrap();
                    // Chrome decodes the SVG's 24px attributes as its natural
                    // size despite its mounted 18px CSS size; retain that viewport.
                    context
                        .draw_image_with_html_image_element_and_dw_and_dh(
                            &decoded,
                            0.0,
                            0.0,
                            bounds.width(),
                            bounds.height(),
                        )
                        .unwrap();
                    let pixels = context
                        .get_image_data(0.0, 0.0, 18.0, 18.0)
                        .unwrap()
                        .data()
                        .0;
                    assert!(
                        pixels.chunks_exact(4).any(|pixel| pixel[3] > 200),
                        "{label} must paint at 18px in {theme} mode"
                    );
                    if matches!(label, "Tycode" | "Grok") {
                        assert!(
                            pixels
                                .chunks_exact(4)
                                .any(|p| p[3] > 200 && p[..3].iter().all(|v| *v < 60))
                                && pixels
                                    .chunks_exact(4)
                                    .any(|p| p[3] > 200 && p[..3].iter().all(|v| *v > 220)),
                            "{label}'s mounted mark must contrast with its backing"
                        );
                    }
                    if label == "OpenCode" {
                        let expected = if theme == "light" {
                            [33, 30, 30]
                        } else {
                            [241, 236, 236]
                        };
                        assert_eq!(
                            &pixels[(9 * 18 + 2) * 4..(9 * 18 + 2) * 4 + 3],
                            &expected,
                            "use the supplied {theme} colorway, not a tint"
                        );
                        assert!(
                            (0..18).all(|y| pixels[y * 18 * 4 + 3] == 0
                                && pixels[(y * 18 + 17) * 4 + 3] == 0),
                            "OpenCode's portrait mark must not stretch to fill the square slot"
                        );
                        assert!(
                            pixels[(9 * 18 + 2) * 4 + 3] > 200
                                && pixels[(9 * 18 + 15) * 4 + 3] > 200,
                            "OpenCode retains its full centered width"
                        );
                    }
                }
                assert_eq!(
                    visible, 1,
                    "exactly one supplied {label} colorway in {theme}"
                );
                mark.remove_attribute("style").unwrap();
            }
        }
        match previous_theme {
            Some(theme) => root.set_attribute("data-theme", &theme).unwrap(),
            None => root.remove_attribute("data-theme").unwrap(),
        }
        for label in ["Muse", "Code helper"] {
            let row = find_button_by_text(&container, label).unwrap();
            let mark = row
                .query_selector("svg[aria-hidden=true]")
                .unwrap()
                .unwrap();
            let bounds = mark.get_bounding_client_rect();
            assert!(bounds.width() >= 20.0 && bounds.height() >= 20.0);
            let mounted_image = mark
                .query_selector("image")
                .unwrap()
                .expect("Muse renders its official mark, not the placeholder letter");
            let mounted_bounds = mounted_image.get_bounding_client_rect();
            assert!(
                mounted_bounds.width() >= 20.0 && mounted_bounds.height() >= 20.0,
                "the actual mounted mark must occupy the icon slot"
            );
            let style = web_sys::window()
                .unwrap()
                .get_computed_style(&mounted_image)
                .unwrap()
                .unwrap();
            assert_ne!(style.get_property_value("display").unwrap(), "none");
            assert_ne!(style.get_property_value("visibility").unwrap(), "hidden");
            assert!(
                style
                    .get_property_value("opacity")
                    .unwrap()
                    .parse::<f64>()
                    .unwrap()
                    > 0.0
            );
            // Give the actual DOM serialization its XML namespace; retain its
            // mounted image dimensions, visibility and embedded asset intact.
            let markup = mark.outer_html().replacen(
                "<svg ",
                "<svg xmlns=\"http://www.w3.org/2000/svg\" ",
                1,
            );
            let image = web_sys::HtmlImageElement::new().unwrap();
            image.set_src(&format!(
                "data:image/svg+xml,{}",
                String::from(js_sys::encode_uri_component(&markup))
            ));
            wasm_bindgen_futures::JsFuture::from(image.decode())
                .await
                .expect("the bundled Muse mark decodes without a network request");
            let canvas = web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .create_element("canvas")
                .unwrap()
                .dyn_into::<web_sys::HtmlCanvasElement>()
                .unwrap();
            canvas.set_width(bounds.width().ceil() as u32);
            canvas.set_height(bounds.height().ceil() as u32);
            let context = canvas
                .get_context("2d")
                .unwrap()
                .unwrap()
                .dyn_into::<web_sys::CanvasRenderingContext2d>()
                .unwrap();
            context
                .draw_image_with_html_image_element(&image, 0.0, 0.0)
                .unwrap();
            let pixels = context
                .get_image_data(
                    0.0,
                    0.0,
                    f64::from(canvas.width()),
                    f64::from(canvas.height()),
                )
                .unwrap()
                .data()
                .0;
            assert!(
                pixels
                    .chunks_exact(4)
                    .filter(|rgba| rgba[2] > 180 && rgba[0] < 40 && rgba[3] > 200)
                    .count()
                    > 30,
                "both default and custom Muse rows display the blue brand mark"
            );
        }
        assert!(!row_button_text(&container).contains(['▶', '◀']));
        assert!(
            find_button_by_text(&container, "Default agent")
                .unwrap()
                .query_selector("svg")
                .unwrap()
                .is_none(),
            "role choices have no pictograms"
        );
    }

    async fn dialog_at_edge(style: &str) -> (f64, f64, f64, f64) {
        let container = make_container();
        container.set_attribute("style", style).unwrap();
        let state = AppState::new();
        set_catalog(
            &state,
            "local",
            vec![ready("claude:default", "Claude", BackendKind::Claude)],
        );
        let open = RwSignal::new(true);
        let _handle = mount_menu(&container, &state, open);
        next_tick().await;
        let dialog = container
            .query_selector("[role=dialog]")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect();
        let viewport = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .document_element()
            .unwrap();
        (
            dialog.left(),
            dialog.right(),
            viewport.client_width() as f64,
            dialog.bottom(),
        )
    }

    #[wasm_bindgen_test]
    async fn chooser_stays_on_screen_from_a_left_edge_trigger() {
        let (left, right, width, bottom) =
            dialog_at_edge("position:fixed;top:0;left:0;width:160px;").await;
        assert!(
            left >= 0.0 && right <= width,
            "left-edge trigger cannot clip the chooser"
        );
        assert!(
            bottom
                <= web_sys::window()
                    .unwrap()
                    .inner_height()
                    .unwrap()
                    .as_f64()
                    .unwrap()
        );
    }

    #[wasm_bindgen_test]
    async fn chooser_stays_on_screen_from_a_right_edge_trigger() {
        let (left, right, width, bottom) =
            dialog_at_edge("position:fixed;top:0;right:0;width:160px;").await;
        assert!(
            left >= 0.0 && right <= width,
            "right-edge trigger cannot clip the chooser"
        );
        assert!(
            bottom
                <= web_sys::window()
                    .unwrap()
                    .inner_height()
                    .unwrap()
                    .as_f64()
                    .unwrap()
        );
    }
}
