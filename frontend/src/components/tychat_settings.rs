use super::settings_panel::{backend_label, backend_value, send_host_replace};
use crate::components::session_settings::SessionSettingsControls;
use crate::state::AppState;
use leptos::prelude::*;
use protocol::{
    CustomAgentId, LaunchProfileEntry, LaunchProfileKind, MidTurnSteeringCapability,
    OPERATOR_CUSTOM_AGENT_ID, SessionSchemaEntry, TychatBridgeStatus, TychatCommandPayload,
    TychatSettingsApplication,
};

fn command(state: &AppState, payload: TychatCommandPayload) {
    let Some((host, stream)) = state.selected_host_stream_untracked() else {
        crate::notices::report_settings_error(
            state,
            "Connect to the host before changing Tychat pairing.",
        );
        return;
    };
    let state = state.clone();
    leptos::task::spawn_local(async move {
        if crate::send::send_frame(&host, stream, protocol::FrameKind::TychatCommand, &payload)
            .await
            .is_err()
        {
            crate::notices::report_settings_error(&state, "Could not send the Tychat command.");
        }
    });
}

fn status_chip(status: Option<&TychatBridgeStatus>) -> (&'static str, &'static str) {
    match status {
        None => ("Loading", "loading"),
        Some(TychatBridgeStatus::Unpaired) => ("Not paired", "loading"),
        Some(TychatBridgeStatus::Connecting) => ("Connecting", "missing"),
        Some(TychatBridgeStatus::Connected) => ("Connected", "installed"),
        Some(TychatBridgeStatus::AwaitingOwnerConfirmation) => {
            ("Awaiting owner confirmation", "missing")
        }
        Some(TychatBridgeStatus::Paused { .. }) => ("Paused", "missing"),
        Some(TychatBridgeStatus::Failed { .. }) => ("Failed", "unavailable"),
    }
}

#[component]
pub fn TychatTab() -> impl IntoView {
    let state = StoredValue::new(expect_context::<AppState>());
    let config = Memo::new(move |_| {
        state
            .get_value()
            .selected_host_settings()
            .map(|settings| settings.tychat)
    });
    let snapshot = Memo::new(move |_| {
        let state = state.get_value();
        let host = state.selected_host_id.get()?;
        state
            .tychat_by_host
            .with(|entries| entries.get(&host).cloned())
    });
    let fingerprints = Memo::new(move |_| snapshot.get().and_then(|state| state.fingerprints));
    let paired = Memo::new(move |_| fingerprints.with(Option::is_some));
    let code = RwSignal::new(String::new());
    Effect::new(move |_| {
        state.get_value().selected_host_id.get();
        code.set(String::new());
    });
    let pair = move || {
        let value = code.get_untracked();
        if value.trim().is_empty() {
            return;
        }
        code.set(String::new());
        command(
            &state.get_value(),
            TychatCommandPayload::Pair { code: value },
        );
    };
    let custom_agents = Memo::new(move |_| {
        let state = state.get_value();
        let Some(host) = state.selected_host_id.get() else {
            return Vec::new();
        };
        let mut agents: Vec<(CustomAgentId, String)> = state.custom_agents.with(|hosts| {
            hosts
                .get(&host)
                .map(|agents| {
                    agents
                        .values()
                        .map(|agent| (agent.id.clone(), agent.name.clone()))
                        .collect()
                })
                .unwrap_or_default()
        });
        agents.sort_by(|a, b| {
            (a.0.0 != OPERATOR_CUSTOM_AGENT_ID, a.1.to_lowercase())
                .cmp(&(b.0.0 != OPERATOR_CUSTOM_AGENT_ID, b.1.to_lowercase()))
        });
        agents
    });
    let launch_profiles = Memo::new(move |_| {
        let state = state.get_value();
        let backend = config.get().and_then(|config| config.backend_kind);
        state
            .selected_host_id
            .get()
            .and_then(|host| {
                state
                    .launch_profile_catalog
                    .with(|catalogs| catalogs.get(&host).cloned())
            })
            .map(|catalog| catalog.entries)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| match entry {
                LaunchProfileEntry::Ready { profile } if Some(profile.backend_kind) == backend => {
                    Some((profile.id, profile.label))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    });
    view! {
        <div class="settings-tychat">
        <div class="settings-panel-header">
            <h2 class="settings-panel-title">"Tychat"</h2>
            {move || {
                let (label, class) = status_chip(snapshot.get().as_ref().map(|state| &state.status));
                view! { <span class=format!("settings-status-chip {class}") role="status">{label}</span> }
            }}
        </div>
        <p class="settings-description settings-panel-intro">"Message this host from Tychat on your phone. Messages are end-to-end encrypted and decrypted only here."</p>
        {move || match snapshot.get().map(|state| state.status) {
            Some(TychatBridgeStatus::Paused { reason } | TychatBridgeStatus::Failed { reason }) => {
                Some(view! { <p class="settings-tychat-problem">{reason}</p> })
            }
            _ => None,
        }}
        <section class="settings-tychat-card">
        {move || if let Some(fingerprints) = fingerprints.get() {
            view! {
                <div class="settings-tychat-card-header">
                    <div>
                        <span class="settings-label">"Paired bot"</span>
                        <p class="settings-description">"These codes match the bot's details in Tychat."</p>
                    </div>
                    <button class="settings-btn settings-btn-danger" on:click=move |_| {
                        let state = state.get_value();
                        leptos::task::spawn_local(async move {
                            if crate::bridge::confirm_dialog("Unpair Tychat", "Erase this host's bot keys and disconnect the Tychat agent?").await { command(&state, TychatCommandPayload::Unpair); }
                        });
                    }>"Unpair"</button>
                </div>
                <dl class="settings-tychat-fingerprints">
                    <dt>"Bot"</dt><dd>{fingerprints.bot}</dd>
                    <dt>"Owner"</dt><dd>{fingerprints.owner}</dd>
                </dl>
            }.into_any()
        } else {
            view! {
                <label class="settings-label" for="tychat-pairing-code">"Pairing code"</label>
                <p class="settings-description">"In Tychat, open Settings → Bots, create a bot, and paste its pairing code here."</p>
                <div class="settings-tychat-pair-row">
                    <input id="tychat-pairing-code" class="settings-input" type="password" autocomplete="off" placeholder="Paste pairing code"
                        prop:value=move || code.get()
                        on:input=move |event| code.set(event_target_value(&event))
                        on:keydown=move |event: web_sys::KeyboardEvent| if event.key() == "Enter" { pair(); } />
                    <button class="settings-btn settings-btn-primary" disabled=move || code.get().trim().is_empty() on:click=move |_| pair()>"Pair"</button>
                </div>
            }.into_any()
        }}
        </section>
        <h3 class="settings-section-title">"Tychat agent"</h3>
        <fieldset class="settings-tychat-agent" disabled=move || config.get().is_none()>
            <div class="settings-toggle-row settings-field">
                <div>
                    <label class="settings-label" for="tychat-enabled">"Enable Tychat agent"</label>
                    <p class="settings-description">"Run an agent on this host that answers your Tychat messages."</p>
                </div>
                <label class="settings-toggle">
                    <input id="tychat-enabled" type="checkbox" prop:checked=move || config.get().is_some_and(|config| config.enabled)
                        on:change=move |event| send_host_replace(&state.get_value(), "/tychat/enabled", event_target_checked(&event)) />
                    <span class="settings-toggle-slider"></span>
                </label>
            </div>
            <div class="settings-field">
                <label class="settings-label" for="tychat-custom-agent">"Agent"</label>
                <p class="settings-description">"The Tyde Operator hands your requests to other agents and reports back."</p>
                <select id="tychat-custom-agent" class="settings-select"
                    prop:value=move || config.get().and_then(|config| config.custom_agent_id).map(|id| id.0).unwrap_or_default()
                    on:change=move |event| {
                        let value = event_target_value(&event);
                        send_host_replace(&state.get_value(), "/tychat/custom_agent_id", (!value.is_empty()).then_some(CustomAgentId(value)));
                    }>
                    <option value="">"Default agent"</option>
                    {move || custom_agents.get().into_iter().map(|(id, name)| view! { <option value=id.0>{name}</option> }).collect_view()}
                </select>
            </div>
            <div class="settings-field">
                <label class="settings-label" for="tychat-backend">"Backend"</label>
                <select id="tychat-backend" class="settings-select" prop:value=move || config.get().and_then(|config| config.backend_kind).map(|kind| backend_value(kind).to_owned()).unwrap_or_default()
                    on:change=move |event| {
                        let key = event_target_value(&event);
                        let kind = snapshot.get_untracked().and_then(|state| state.backend_capabilities.into_iter().find(|entry| backend_value(entry.backend_kind) == key)).map(|entry| entry.backend_kind);
                        if let Some(mut config) = config.get_untracked() {
                            config.backend_kind = kind; config.launch_profile_id = None; config.session_settings = Default::default();
                            send_host_replace(&state.get_value(), "/tychat", config);
                        }
                    }>
                    <option value="">"Choose a backend"</option>
                    {move || {
                        let enabled = state.get_value().selected_host_settings().map(|settings| settings.enabled_backends).unwrap_or_default();
                        snapshot.get().map(|state| state.backend_capabilities).unwrap_or_default().into_iter()
                            .filter(|entry| entry.mid_turn == MidTurnSteeringCapability::Supported && enabled.contains(&entry.backend_kind))
                            .map(|entry| view! { <option value=backend_value(entry.backend_kind)>{backend_label(entry.backend_kind)}</option> }).collect_view()
                    }}
                </select>
            </div>
            {move || (!launch_profiles.get().is_empty()).then(|| view! {
                <div class="settings-field">
                    <label class="settings-label" for="tychat-launch-profile">"Launch profile"</label>
                    <select id="tychat-launch-profile" class="settings-select" prop:value=move || config.get().and_then(|config| config.launch_profile_id).map(|id| id.0).unwrap_or_default()
                        on:change=move |event| {
                            let value = event_target_value(&event);
                            send_host_replace(&state.get_value(), "/tychat/launch_profile_id", (!value.is_empty()).then_some(protocol::LaunchProfileId(value)));
                        }>
                        <option value="">"None"</option>
                        {move || launch_profiles.get().into_iter().map(|(id, label)| view! { <option value=id.0>{label}</option> }).collect_view()}
                    </select>
                </div>
            })}
            {move || {
                let state_value = state.get_value();
                let entry = config.get().and_then(|config| {
                    let host = state_value.selected_host_id.get()?;
                    let kind = config.backend_kind?;
                    if let Some(id) = config.launch_profile_id {
                        let catalog = state_value.launch_profile_catalog.with(|hosts| hosts.get(&host).cloned())?;
                        let profile = catalog.entries.into_iter().find_map(|entry| match entry {
                            LaunchProfileEntry::Ready { profile } if profile.id == id => Some(profile),
                            _ => None,
                        })?;
                        if profile.kind == LaunchProfileKind::Custom {
                            return catalog.custom_profile_schemas.into_iter()
                                .find(|entry| entry.launch_profile_id == id).map(|entry| Some(entry.schema));
                        }
                    }
                    Some(state_value.session_schemas.with(|hosts| hosts.get(&host).and_then(|schemas| schemas.get(&kind).cloned())))
                });
                match entry {
                    None => None,
                    Some(Some(SessionSchemaEntry::Ready { schema })) => Some(view! {
                        <div class="settings-field settings-tychat-session">
                            <span class="settings-label">"Session settings"</span>
                            <SessionSettingsControls schema=schema values=Signal::derive(move || config.get().map(|config| config.session_settings).unwrap_or_default())
                                on_change=Callback::new(move |values: protocol::SessionSettingsValues| send_host_replace(&state.get_value(), "/tychat/session_settings", values)) remove_unsupported_keys=true />
                        </div>
                    }.into_any()),
                    Some(Some(SessionSchemaEntry::Unavailable { message, .. })) => Some(view! { <p class="settings-description">{message}</p> }.into_any()),
                    Some(_) => Some(view! { <p class="settings-description">"Loading session settings…"</p> }.into_any()),
                }
            }}
        </fieldset>
        {move || paired.get().then(|| view! {
            <div class="settings-tychat-footer">
                <p class="settings-description" role="status">{move || match snapshot.get().map(|state| state.settings_application) {
                    Some(TychatSettingsApplication::AppliesOnReset) => "Applies on reset: reset the Tychat agent to use these settings.".to_owned(),
                    Some(TychatSettingsApplication::Failed { reason }) => format!("Settings failed: {reason}"),
                    _ => String::new(),
                }}</p>
                <button class="settings-btn" disabled=move || !config.get().is_some_and(|config| config.enabled) on:click=move |_| {
                    let state = state.get_value();
                    leptos::task::spawn_local(async move {
                        if crate::bridge::confirm_dialog("Reset Tychat agent", "Start a fresh Tychat agent session?").await { command(&state, TychatCommandPayload::ResetAgent); }
                    });
                }>"Reset Tychat agent"</button>
            </div>
        })}
        </div>
    }
}
