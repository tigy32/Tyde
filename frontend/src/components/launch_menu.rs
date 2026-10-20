use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::actions::{begin_new_chat_with_profile, begin_new_chat_with_profile_in_project};
use crate::state::{ActiveProjectRef, AppState, DEFAULT_CUSTOM_AGENT_ID};

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
    let mark = match kind {
        BackendKind::Tycode => view! { <path d="m7 5-5 7 5 7m10-14 5 7-5 7m-3-16-4 18" /> }.into_any(),
        BackendKind::Kiro => view! {
            <path d="m12 2 9 5v10l-9 5-9-5V7z" />
            <circle cx="12" cy="12" r="3" />
            <circle cx="13" cy="11" r="0.8" fill="currentColor" />
        }.into_any(),
        BackendKind::Claude => view! {
            <g stroke="#d97757" stroke-width="1.9">
                <path d="M12 1v22M1 12h22M4.2 4.2l15.6 15.6M4.2 19.8 19.8 4.2M7.8 1.8l8.4 20.4M1.8 7.8l20.4 8.4M1.8 16.2l20.4-8.4M7.8 22.2l8.4-20.4" />
            </g>
        }.into_any(),
        BackendKind::Codex => view! {
            <g stroke-width="1.4">
                {(0..6).map(|i| view! {
                    <path transform=format!("rotate({} 12 12)", i * 60)
                        d="M12 3c-3-3-8 0-7 4l1 6 6 3 5-3V7l-5-3-5 3v6" />
                }).collect_view()}
            </g>
        }.into_any(),
        BackendKind::Antigravity => view! {
            <path d="m3 20 9-17 9 17" stroke="#a58cff" stroke-width="4" />
            <path d="m12 3 4.5 8.5" stroke="#f39b88" stroke-width="4" />
            <path d="m16.5 11.5 4.5 8.5" stroke="#69b8ff" stroke-width="4" />
        }.into_any(),
        BackendKind::Hermes => view! {
            <path d="M2 3c4 7 13 2 20 14-7-7-14 0-19-8m1 3c5 7 12 1 18 8-7-3-11 2-16-4m3 3c4 4 8 1 12 3" />
        }.into_any(),
        BackendKind::Grok => view! {
            <path d="M18 4a9 9 0 1 0 3 6M2 22 22 2m-5 1h5v5" />
        }.into_any(),
        BackendKind::Opencode => view! {
            <rect x="2" y="2" width="20" height="20" rx="3" />
            <path d="m6 7 4 5-4 5m7 0h5" />
        }.into_any(),
        BackendKind::Muse => {
            // Bundle https://muse.ai/images/favicon/app-squiggle.svg unchanged;
            // its embedded SVG keeps gradient IDs private and works offline.
            let source = format!(
                "data:image/svg+xml,{}",
                String::from(js_sys::encode_uri_component(include_str!("../../muse.svg")))
            );
            view! { <image href=source width="24" height="24" /> }.into_any()
        },
    };
    view! {
        <svg class="launch-backend-icon" width="24" height="24" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"
            stroke-linejoin="round" aria-hidden="true" focusable="false">{mark}</svg>
    }
}

#[component]
pub fn LaunchChooser(
    open_sig: RwSignal<bool>,
    #[prop(optional)] initial_backend: Option<BackendKind>,
    #[prop(optional)] project_context: Option<Memo<Option<ActiveProjectRef>>>,
    #[prop(optional)] on_created: Option<Callback<()>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let chosen_project = project_context.and_then(|context| context.get_untracked());
    let host = match project_context {
        Some(_) => chosen_project
            .as_ref()
            .map(|project| project.host_id.clone()),
        None => state.chat_context_host_id_untracked(),
    };
    let context_label = chosen_project.as_ref().map(|target| {
        let projects = state.projects;
        let hosts = state.configured_hosts;
        let target = target.clone();
        move || {
            let project = projects.with(|projects| {
                projects
                    .iter()
                    .find(|project| {
                        project.host_id == target.host_id && project.project.id == target.project_id
                    })
                    .map(|project| project.project.name.clone())
            });
            let host = hosts.with(|hosts| {
                hosts
                    .iter()
                    .find(|host| host.id == target.host_id)
                    .map(|host| host.label.clone())
            });
            match (host, project) {
                (Some(host), Some(project)) => format!("{host} · {project}"),
                _ => "Project unavailable".to_owned(),
            }
        }
    });
    let entries_state = state.clone();
    let entries_host = host.clone();
    let entries = Memo::new(move |_| {
        entries_state.launch_profile_catalog.with(|catalogs| {
            entries_host
                .as_ref()
                .and_then(|host| catalogs.get(host))
                .map(|catalog| catalog.entries.clone())
                .unwrap_or_default()
        })
    });
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
    let initial_agent =
        if chosen_project.is_none() && state.active_project_ref_untracked().is_none() {
            state.launch_profile_catalog.with_untracked(|catalogs| {
                catalogs.get(host.as_ref()?)?.home_custom_agent_id.clone()
            })
        } else {
            None
        };
    let selected_agent = RwSignal::new(initial_agent);
    let backend_query = RwSignal::new(String::new());
    let role_query = RwSignal::new(String::new());
    let agents_state = state.clone();
    let agents_host = host.clone();
    let agents = Memo::new(move |_| {
        let mut agents: Vec<CustomAgent> = agents_state.custom_agents.with(|hosts| {
            agents_host
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
        entries.get().into_iter().find_map(|entry| match entry {
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

    let context_state = state.clone();
    let context_host = host.clone();
    let context_project = chosen_project.clone();
    let context_matches = Memo::new(move |_| match project_context {
        Some(current) => current
            .get()
            .is_some_and(|project| Some(project) == context_project),
        None => context_state.chat_context_host_id() == context_host,
    });
    Effect::new(move |_| {
        if !context_matches.get() {
            open_sig.set(false);
        }
    });

    let create = Callback::new(move |()| {
        if !open_sig.get_untracked()
            || !context_matches.get_untracked()
            || !role_available.get_untracked()
        {
            return;
        }
        if let Some(profile) = ready_profile.get_untracked() {
            open_sig.set(false);
            match chosen_project.clone() {
                Some(project) => begin_new_chat_with_profile_in_project(
                    &state,
                    profile,
                    selected_agent.get_untracked(),
                    project,
                ),
                None => {
                    begin_new_chat_with_profile(&state, profile, selected_agent.get_untracked())
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
                let Ok(nodes) =
                    dialog.query_selector_all("button:not([disabled]), input:not([disabled])")
                else {
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
                    <div><h2>"New agent"</h2><p>"Choose a backend and an agent role."</p>{context_label.map(|label| view! { <p class="launch-project-context">{label}</p> })}</div>
                    <button type="button" class="launch-chooser-close" aria-label="Close agent chooser"
                        on:click=move |_| open_sig.set(false)>"×"</button>
                </header>
                <div class="launch-chooser-body">
                    <section class="launch-backends" aria-label="Backend">
                        <h3>"Backend"</h3>
                        <input type="search" aria-label="Find a backend" placeholder="Find a backend…"
                            node_ref=search_ref prop:value=move || backend_query.get()
                            on:input=move |ev| backend_query.set(event_target_value(&ev)) />
                        <div class="launch-backend-list" role="group" aria-label="Backend choices">
                            {move || {
                                let query = backend_query.get().trim().to_lowercase();
                                let filtered: Vec<_> = entries.get().into_iter().filter(|entry| {
                                    let label = match entry { LaunchProfileEntry::Ready { profile } => &profile.label,
                                        LaunchProfileEntry::Unavailable { label, .. } => label };
                                    label.to_lowercase().contains(&query) || backend_label_for(entry.backend_kind()).to_lowercase().contains(&query)
                                }).collect();
                                if filtered.is_empty() {
                                    return view! { <p class="launch-empty">{if entries.get().is_empty() {
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
                                    title=move || default_description.get() on:click=move |_| selected_agent.set(Some(CustomAgentId(DEFAULT_CUSTOM_AGENT_ID.to_owned())))>
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
                                                on:click=move |_| selected_agent.set(Some(id.clone()))>
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
                        <Show when=move || !role_available.get()><small>"The selected role is no longer available."</small></Show>
                    </div>
                    <div class="launch-footer-actions">
                        <div><button type="button" class="launch-cancel" on:click=move |_| open_sig.set(false)>"Cancel"</button>
                            <button type="button" class="launch-create" disabled=move || ready_profile.get().is_none() || !role_available.get()
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

    fn make_container() -> HtmlElement {
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

    /// When the host has no launch profiles yet, the empty state guides the
    /// user to enable a backend rather than showing a dead end.
    #[wasm_bindgen_test]
    async fn empty_catalog_shows_actionable_copy() {
        let container = make_container();
        let state = AppState::new();
        // Host selected but no catalog entries → empty state.
        state.selected_host_id.set(Some("local".to_owned()));
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
                ready("claude:default", "Claude", BackendKind::Claude),
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
