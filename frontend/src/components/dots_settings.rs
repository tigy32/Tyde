use super::settings_panel::send_host_replace;
use crate::state::AppState;
use leptos::prelude::*;
use protocol::{DotsCommandPayload, DotsConnectionStatus};

fn command(state: &AppState, payload: DotsCommandPayload) {
    let Some((host, stream)) = state.selected_host_stream_untracked() else {
        crate::notices::report_settings_error(
            state,
            "Connect to the host before changing its Dots connection.",
        );
        return;
    };
    let state = state.clone();
    leptos::task::spawn_local(async move {
        if crate::send::send_frame(&host, stream, protocol::FrameKind::DotsCommand, &payload)
            .await
            .is_err()
        {
            crate::notices::report_settings_error(
                &state,
                "Could not send the Dots connection command.",
            );
        }
    });
}

#[component]
pub fn DotsTab() -> impl IntoView {
    let state = StoredValue::new(expect_context::<AppState>());
    let snapshot = Memo::new(move |_| {
        let state = state.get_value();
        let host = state.selected_host_id.get()?;
        state
            .dots_by_host
            .with(|entries| entries.get(&host).cloned())
    });
    let settings = Memo::new(move |_| state.get_value().selected_host_settings());
    view! {
        <div class="settings-tychat">
            <div class="settings-panel-header">
                <h2 class="settings-panel-title">"ChatGPT Dots"</h2>
                <span class="settings-status-chip" role="status">{move || match snapshot.get().map(|s| s.status) {
                    None => "Loading",
                    Some(DotsConnectionStatus::Disabled) => "Disabled",
                    Some(DotsConnectionStatus::Connecting) => "Connecting",
                    Some(DotsConnectionStatus::AwaitingPairing) => "Awaiting pairing",
                    Some(DotsConnectionStatus::Connected) => "Connected",
                    Some(DotsConnectionStatus::Failed { .. }) => "Failed",
                }}</span>
            </div>
            <p class="settings-description settings-panel-intro">"Let your dot manage this host's Tyde agents through a private, authenticated connection. Each user pairs their own host with their own ChatGPT account; no OpenAI API keys or open inbound ports are needed."</p>
            <fieldset disabled=move || settings.get().is_none()>
                <div class="settings-toggle-row settings-field">
                    <div>
                        <label class="settings-label" for="dots-enabled">"Enable ChatGPT Dots"</label>
                        <p class="settings-description">"Tyde sets up and runs the host connection. Turning this off blocks new remote commands; agents already running continue."</p>
                    </div>
                    <label class="settings-toggle">
                        <input id="dots-enabled" type="checkbox"
                            prop:checked=move || settings.get().is_some_and(|s| s.chatgpt_dots_enabled)
                            on:change=move |event| send_host_replace(&state.get_value(), "/chatgpt_dots_enabled", event_target_checked(&event)) />
                        <span class="settings-toggle-slider"></span>
                    </label>
                </div>
            </fieldset>
            <p class="settings-description">"Pairing grants ChatGPT access to this host's projects and agents. Agents can modify files and use your configured providers. Tool inputs and results pass through the Tyde connection service and OpenAI; this connection is not end-to-end encrypted."</p>
            {move || snapshot.get().and_then(|snapshot| match snapshot.status {
                DotsConnectionStatus::AwaitingPairing => Some(view! {
                    <section class="settings-tychat-card">
                        <h3 class="settings-section-title">"Connect your ChatGPT account"</h3>
                        <ol class="settings-description">
                            <li>"Open ChatGPT → Plugins → Add custom MCP server."</li>
                            <li>"Paste the server URL below and choose OAuth authentication."</li>
                            <li>"Create and install the plugin, then paste this pairing code in the Tyde authorization page when connecting it."</li>
                        </ol>
                        <label class="settings-label" for="dots-server-url">"Server URL"</label>
                        <input id="dots-server-url" class="settings-input" readonly prop:value=snapshot.mcp_url />
                        <label class="settings-label" for="dots-pairing-code">"One-time pairing code"</label>
                        <input id="dots-pairing-code" class="settings-input" readonly type="password" autocomplete="off" prop:value=snapshot.pairing_code />
                        <p class="settings-description">"Copy the code only into the Tyde authorization page—not into a chat. It expires after 10 minutes."</p>
                        <button class="settings-btn" on:click=move |_| {
                            let state = state.get_value();
                            leptos::task::spawn_local(async move {
                                if let Err(reason) = crate::bridge::open_external_url("https://chatgpt.com/plugins".into()).await {
                                    crate::notices::report_settings_error(&state, format!("Could not open ChatGPT: {reason}"));
                                }
                            });
                        }>"Open ChatGPT"</button>
                        <button class="settings-btn" on:click=move |_| command(&state.get_value(), DotsCommandPayload::Retry)>"New pairing code"</button>
                    </section>
                }.into_any()),
                DotsConnectionStatus::Connected => Some(view! {
                    <section class="settings-tychat-card">
                        <p class="settings-description">"Your ChatGPT connection is authorized and this host is online. Ask your dot to list your Tyde projects or start an agent."</p>
                        <button class="settings-btn settings-btn-danger" on:click=move |_| {
                            let state = state.get_value();
                            leptos::task::spawn_local(async move {
                                if crate::bridge::confirm_dialog("Disconnect ChatGPT", "Revoke this host's ChatGPT access? Existing agents will continue. If enabled, Tyde will offer a new pairing code.").await {
                                    command(&state, DotsCommandPayload::Disconnect);
                                }
                            });
                        }>"Disconnect ChatGPT"</button>
                    </section>
                }.into_any()),
                DotsConnectionStatus::Failed { reason } => Some(view! {
                    <p class="settings-tychat-problem" role="alert">{reason}</p>
                    <button class="settings-btn" disabled=move || !settings.get().is_some_and(|s| s.chatgpt_dots_enabled) on:click=move |_| command(&state.get_value(), DotsCommandPayload::Retry)>"Retry"</button>
                }.into_any()),
                DotsConnectionStatus::Disabled | DotsConnectionStatus::Connecting => None,
            })}
        </div>
    }
}
