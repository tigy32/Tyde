use super::settings_panel::{backend_label, backend_value, send_host_replace};
use crate::components::session_settings::SessionSettingsControls;
use crate::state::AppState;
use leptos::prelude::*;
use protocol::{
    BackendAccessMode, LaunchProfileEntry, LaunchProfileKind, MidTurnSteeringCapability,
    SessionSchemaEntry, TychatBridgeStatus, TychatCommandPayload, TychatSettingsApplication,
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
    let code = RwSignal::new(String::new());
    Effect::new(move |_| {
        state.get_value().selected_host_id.get();
        code.set(String::new());
    });
    let status = move || match snapshot.get().map(|state| state.status) {
        None => "Waiting for host state".to_owned(),
        Some(TychatBridgeStatus::Unpaired) => "Unpaired".into(),
        Some(TychatBridgeStatus::Connecting) => "Connecting".into(),
        Some(TychatBridgeStatus::Connected) => "Connected".into(),
        Some(TychatBridgeStatus::AwaitingOwnerConfirmation) => "Awaiting owner confirmation".into(),
        Some(TychatBridgeStatus::Paused { reason }) => format!("Paused: {reason}"),
        Some(TychatBridgeStatus::Failed { reason }) => format!("Failed: {reason}"),
    };
    view! {
        <div class="settings-tychat">
        <h2 class="settings-section-title">"Tychat"</h2>
        <p class="settings-description">"Pair this host with your personal bot. Messages are decrypted on this host and delivered to the Tychat agent."</p>
        <p role="status">{status}</p>
        {move || snapshot.get().and_then(|state| state.fingerprints).map(|fingerprints| view! {
            <dl><dt>"Bot fingerprint"</dt><dd>{fingerprints.bot}</dd><dt>"Owner fingerprint"</dt><dd>{fingerprints.owner}</dd></dl>
        })}
        <label class="settings-field">"Pairing code"<input class="settings-input" type="password" autocomplete="off" prop:value=move || code.get()
            on:input=move |event| code.set(event_target_value(&event)) /></label>
        <button class="settings-btn" disabled=move || code.get().trim().is_empty() on:click=move |_| {
            let value = code.get_untracked(); code.set(String::new());
            command(&state.get_value(), TychatCommandPayload::Pair { code: value });
        }>"Pair"</button>
        <button class="settings-btn" disabled=move || snapshot.get().is_none_or(|state| state.fingerprints.is_none()) on:click=move |_| {
            let state = state.get_value();
            leptos::task::spawn_local(async move {
                if crate::bridge::confirm_dialog("Unpair Tychat", "Erase this host's bot keys and disconnect the Tychat agent?").await { command(&state, TychatCommandPayload::Unpair); }
            });
        }>"Unpair"</button>
        <fieldset disabled=move || config.get().is_none()>
            <label class="settings-toggle-row"><input type="checkbox" prop:checked=move || config.get().is_some_and(|config| config.enabled)
                on:change=move |event| send_host_replace(&state.get_value(), "/tychat/enabled", event_target_checked(&event)) />"Enable Tychat"</label>
            <label class="settings-field">"API base URL"<input class="settings-input" type="url" prop:value=move || config.get().map(|config| config.api_base_url).unwrap_or_default()
                on:change=move |event| send_host_replace(&state.get_value(), "/tychat/api_base_url", event_target_value(&event)) /></label>
            <label class="settings-field">"Backend"<select class="settings-select" prop:value=move || config.get().and_then(|config| config.backend_kind).map(|kind| backend_value(kind).to_owned()).unwrap_or_default()
                on:change=move |event| {
                    let key = event_target_value(&event);
                    let kind = snapshot.get_untracked().and_then(|state| state.backend_capabilities.into_iter().find(|entry| backend_value(entry.backend_kind) == key)).map(|entry| entry.backend_kind);
                    if let Some(mut config) = config.get_untracked() {
                        config.backend_kind = kind; config.launch_profile_id = None; config.session_settings = Default::default();
                        send_host_replace(&state.get_value(), "/tychat", config);
                    }
                }>
                <option value="">"Choose a steer-capable backend"</option>
                {move || {
                    let enabled = state.get_value().selected_host_settings().map(|settings| settings.enabled_backends).unwrap_or_default();
                    snapshot.get().map(|state| state.backend_capabilities).unwrap_or_default().into_iter()
                        .filter(|entry| entry.mid_turn == MidTurnSteeringCapability::Supported && enabled.contains(&entry.backend_kind))
                        .map(|entry| view! { <option value=backend_value(entry.backend_kind)>{backend_label(entry.backend_kind)}</option> }).collect_view()
                }}
            </select></label>
            <label class="settings-field">"Launch profile"<select class="settings-select" prop:value=move || config.get().and_then(|config| config.launch_profile_id).map(|id| id.0).unwrap_or_default()
                on:change=move |event| {
                    let value = event_target_value(&event);
                    send_host_replace(&state.get_value(), "/tychat/launch_profile_id", (!value.is_empty()).then_some(protocol::LaunchProfileId(value)));
                }>
                <option value="">"No launch profile"</option>
                {move || {
                    let state = state.get_value();
                    let backend = config.get().and_then(|config| config.backend_kind);
                    state.selected_host_id.get().and_then(|host| state.launch_profile_catalog.with(|catalogs| catalogs.get(&host).cloned()))
                        .map(|catalog| catalog.entries).unwrap_or_default().into_iter().filter_map(|entry| match entry {
                            LaunchProfileEntry::Ready { profile } if Some(profile.backend_kind) == backend => Some(view! { <option value=profile.id.0>{profile.label}</option> }),
                            _ => None,
                        }).collect_view()
                }}
            </select></label>
            <label class="settings-field">"Access mode"<select class="settings-select" prop:value=move || match config.get().map(|config| config.access_mode) { Some(BackendAccessMode::ReadOnly) => "read_only", _ => "unrestricted" }
                on:change=move |event| send_host_replace(&state.get_value(), "/tychat/access_mode", if event_target_value(&event) == "read_only" { BackendAccessMode::ReadOnly } else { BackendAccessMode::Unrestricted })>
                <option value="unrestricted">"Unrestricted"</option><option value="read_only">"Read-only (advisory)"</option>
            </select></label>
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
                                .find(|entry| entry.launch_profile_id == id).map(|entry| entry.schema);
                        }
                    }
                    state_value.session_schemas.with(|hosts| hosts.get(&host).and_then(|schemas| schemas.get(&kind).cloned()))
                });
                match entry {
                    Some(SessionSchemaEntry::Ready { schema }) => view! {
                        <SessionSettingsControls schema=schema values=Signal::derive(move || config.get().map(|config| config.session_settings).unwrap_or_default())
                            on_change=Callback::new(move |values: protocol::SessionSettingsValues| send_host_replace(&state.get_value(), "/tychat/session_settings", values)) remove_unsupported_keys=true />
                    }.into_any(),
                    Some(SessionSchemaEntry::Unavailable { message, .. }) => view! { <p>{message}</p> }.into_any(),
                    _ => view! { <p>"Session settings are not available yet."</p> }.into_any(),
                }
            }}
        </fieldset>
        <p role="status">{move || match snapshot.get().map(|state| state.settings_application) {
            Some(TychatSettingsApplication::Live) => "Settings applied to the live session".to_owned(),
            Some(TychatSettingsApplication::AppliesOnReset) => "Applies on reset".into(),
            Some(TychatSettingsApplication::Failed { reason }) => format!("Settings failed: {reason}"),
            _ => "Settings apply when the Tychat agent starts".into(),
        }}</p>
        <button class="settings-btn" disabled=move || !config.get().is_some_and(|config| config.enabled) || snapshot.get().is_none_or(|state| state.fingerprints.is_none()) on:click=move |_| {
            let state = state.get_value();
            leptos::task::spawn_local(async move {
                if crate::bridge::confirm_dialog("Reset Tychat agent", "Start a fresh Tychat agent session?").await { command(&state, TychatCommandPayload::ResetAgent); }
            });
        }>"Reset Tychat agent"</button>
        </div>
    }
}
