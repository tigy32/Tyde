use super::settings_panel::send_host_replace;
use crate::state::AppState;
use leptos::prelude::*;
use protocol::{DotsCommandPayload, DotsConnectionStatus};

fn command(state: &AppState, payload: DotsCommandPayload) {
    let Some((host, stream)) = state.selected_host_stream_untracked() else {
        crate::notices::report_settings_error(
            state,
            "Connect to the host before changing remote MCP access.",
        );
        return;
    };
    let state = state.clone();
    leptos::task::spawn_local(async move {
        if crate::send::send_frame(&host, stream, protocol::FrameKind::DotsCommand, &payload)
            .await
            .is_err()
        {
            crate::notices::report_settings_error(&state, "Could not send the remote MCP command.");
        }
    });
}

fn copy_text(
    state: &AppState,
    text: String,
    copied: RwSignal<Option<String>>,
    failure: &'static str,
) {
    let state = state.clone();
    copied.set(None);
    leptos::task::spawn_local(async move {
        match crate::bridge::write_clipboard_text(&text).await {
            Ok(()) => copied.set(Some(text)),
            Err(_) => crate::notices::report_settings_error(&state, failure),
        }
    });
}

#[component]
pub fn DotsTab() -> impl IntoView {
    let state = StoredValue::new(expect_context::<AppState>());
    let copied_server_url = RwSignal::new(None::<String>);
    let copied_pairing_code = RwSignal::new(None::<String>);
    let snapshot = Memo::new(move |_| {
        let state = state.get_value();
        let host = state.selected_host_id.get()?;
        state
            .dots_by_host
            .with(|entries| entries.get(&host).cloned())
    });
    let settings = Memo::new(move |_| state.get_value().selected_host_settings());
    view! {
        <div class="settings-tychat settings-dots">
            <div class="settings-panel-header">
                <h2 class="settings-panel-title">"Remote MCP"</h2>
                <span class=move || match snapshot.get().map(|s| s.status) {
                    None | Some(DotsConnectionStatus::Connecting) => "settings-status-chip loading",
                    Some(DotsConnectionStatus::Disabled) => "settings-status-chip unsupported",
                    Some(DotsConnectionStatus::AwaitingPairing) => "settings-status-chip missing",
                    Some(DotsConnectionStatus::Connected) => "settings-status-chip installed",
                    Some(DotsConnectionStatus::Failed { .. }) => "settings-status-chip unavailable",
                } role="status">{move || match snapshot.get().map(|s| s.status) {
                    None => "Loading",
                    Some(DotsConnectionStatus::Disabled) => "Disabled",
                    Some(DotsConnectionStatus::Connecting) => "Connecting",
                    Some(DotsConnectionStatus::AwaitingPairing) => "Awaiting pairing",
                    Some(DotsConnectionStatus::Connected) => "Connected",
                    Some(DotsConnectionStatus::Failed { .. }) => "Failed",
                }}</span>
            </div>
            <p class="settings-description settings-panel-intro">"Manage remote MCP access to this host's Tyde projects and agents. No API keys or open inbound ports are needed."</p>
            <section class="settings-tychat-card settings-dots-card">
                <div class="settings-toggle-row settings-dots-enable">
                    <div>
                        <label class="settings-label" for="dots-enabled">"Enable remote MCP"</label>
                        <p class="settings-description">"Allow paired MCP clients to issue commands for this host. Turning this off blocks new commands; running agents continue."</p>
                    </div>
                    <label class="settings-toggle">
                        <input id="dots-enabled" type="checkbox"
                            disabled=move || settings.get().is_none()
                            prop:checked=move || settings.get().is_some_and(|s| s.chatgpt_dots_enabled)
                            on:change=move |event| send_host_replace(&state.get_value(), "/chatgpt_dots_enabled", event_target_checked(&event)) />
                        <span class="settings-toggle-slider"></span>
                    </label>
                </div>
            </section>
            {move || snapshot.get().and_then(|snapshot| match snapshot.status {
                DotsConnectionStatus::AwaitingPairing => {
                    let server_url = snapshot.mcp_url.clone();
                    let pairing_code = snapshot.pairing_code.clone();
                    let copy_server_url = server_url.clone();
                    let copy_pairing_code = pairing_code.clone();
                    let copied_url_value = server_url.clone();
                    let copied_code_value = pairing_code.clone();
                    Some(view! {
                        <section class="settings-tychat-card settings-dots-card settings-dots-pairing">
                            <h3 class="settings-section-title">"Connect an MCP client"</h3>
                            <ol class="settings-description settings-dots-steps">
                                <li>"In your MCP client, add a remote server and use this URL."</li>
                                <li>"Choose OAuth. When the client opens Tyde's authorization page, enter the one-time pairing code below."</li>
                                <li>"Keep the code private and use it only on Tyde's authorization page, never in a chat. It expires after 10 minutes."</li>
                            </ol>
                            <div class="settings-dots-field">
                                <span class="settings-label">"Server URL"</span>
                                {match server_url.clone() {
                                    Some(url) => view! {
                                        <code id="dots-server-url" class="settings-dots-endpoint">{url}</code>
                                    }.into_any(),
                                    None => view! {
                                        <p class="settings-tychat-problem" role="alert">"The server URL is unavailable."</p>
                                    }.into_any(),
                                }}
                            </div>
                            <div class="settings-dots-field">
                                <label class="settings-label" for="dots-pairing-code">"One-time pairing code"</label>
                                {match pairing_code.clone() {
                                    Some(code) => view! {
                                        <input id="dots-pairing-code" class="settings-input settings-dots-code" readonly type="password" autocomplete="off" aria-describedby="dots-pairing-hint" prop:value=code />
                                    }.into_any(),
                                    None => view! {
                                        <p class="settings-tychat-problem" role="alert">"The pairing code is unavailable."</p>
                                    }.into_any(),
                                }}
                                <p id="dots-pairing-hint" class="settings-description">"The generated pairing code is private, only for OAuth authorization, and expires after 10 minutes. Never paste it into a chat."</p>
                            </div>
                            <div class="settings-dots-actions">
                                <button class="settings-btn" disabled=server_url.is_none() on:click=move |_| {
                                    if let Some(url) = copy_server_url.clone() {
                                        copy_text(&state.get_value(), url, copied_server_url, "Could not copy the server URL.");
                                    }
                                }>
                                    {move || if copied_server_url.get().is_some_and(|copied| copied_url_value.as_deref() == Some(copied.as_str())) { "Server URL copied" } else { "Copy server URL" }}
                                </button>
                                <button class="settings-btn" disabled=pairing_code.is_none() on:click=move |_| {
                                    if let Some(code) = copy_pairing_code.clone() {
                                        copy_text(&state.get_value(), code, copied_pairing_code, "Could not copy the pairing code.");
                                    }
                                }>
                                    {move || if copied_pairing_code.get().is_some_and(|copied| copied_code_value.as_deref() == Some(copied.as_str())) { "Pairing code copied" } else { "Copy pairing code" }}
                                </button>
                                <button class="settings-btn" on:click=move |_| command(&state.get_value(), DotsCommandPayload::Retry)>"New pairing code"</button>
                            </div>
                        </section>
                        <section class="settings-tychat-card settings-dots-card settings-dots-pairing">
                            <h3 class="settings-section-title">"Connect ChatGPT"</h3>
                            <ol class="settings-description settings-dots-steps">
                                <li>"In ChatGPT, open personal Plugins → + → Add custom MCP server."</li>
                                <li>"Name the server Tyde. Open ChatGPT copies the server URL first; paste it into the custom server URL field."</li>
                                <li>"Choose OAuth. Leave the client ID and secret blank for automatic registration."</li>
                                <li>"Finish setup, then enter the pairing code on Tyde's authorization page."</li>
                            </ol>
                            <div class="settings-dots-actions">
                                <button class="settings-btn settings-btn-primary" disabled=server_url.is_none() on:click=move |_| {
                                    let state = state.get_value();
                                    let Some(url) = server_url.clone() else {
                                        return;
                                    };
                                    leptos::task::spawn_local(async move {
                                        if crate::bridge::write_clipboard_text(&url).await.is_err() {
                                            crate::notices::report_settings_error(&state, "Could not copy the server URL. Copy it manually before opening ChatGPT.");
                                            return;
                                        }
                                        if crate::bridge::open_external_url("https://chatgpt.com/plugins?directoryTab=personal".into()).await.is_err() {
                                            crate::notices::report_settings_error(&state, "Could not open ChatGPT.");
                                        }
                                    });
                                }>"Open ChatGPT"</button>
                            </div>
                        </section>
                    }.into_any())
                },
                DotsConnectionStatus::Connected => Some(view! {
                    <section class="settings-tychat-card settings-dots-card">
                        <h3 class="settings-section-title">"Remote MCP is connected"</h3>
                        <p class="settings-description">"Paired MCP clients can access this host's projects and agents. Agents can modify files and use your configured providers."</p>
                        <div class="settings-dots-actions">
                            <button class="settings-btn settings-btn-danger" on:click=move |_| {
                                let state = state.get_value();
                                leptos::task::spawn_local(async move {
                                    if crate::bridge::confirm_dialog("Revoke remote MCP access", "Revoke remote MCP access to this host? Running agents will continue. If enabled, Tyde will offer a new pairing code.").await {
                                        command(&state, DotsCommandPayload::Disconnect);
                                    }
                                });
                            }>"Revoke remote MCP access"</button>
                        </div>
                    </section>
                }.into_any()),
                DotsConnectionStatus::Failed { reason } => Some(view! {
                    <section class="settings-tychat-card settings-dots-card">
                        <p class="settings-tychat-problem" role="alert">{reason}</p>
                        <div class="settings-dots-actions">
                            <button class="settings-btn" disabled=move || !settings.get().is_some_and(|s| s.chatgpt_dots_enabled) on:click=move |_| command(&state.get_value(), DotsCommandPayload::Retry)>"Retry"</button>
                        </div>
                    </section>
                }.into_any()),
                DotsConnectionStatus::Disabled | DotsConnectionStatus::Connecting => None,
            })}
            <section class="settings-dots-disclosure" aria-label="Access and privacy">
                <h3 class="settings-label">"Access & privacy"</h3>
                <p class="settings-description">"Pairing grants a remote MCP client access to this host's projects and agents. Agents can modify files and use your configured providers. Tool inputs and results may be visible to the connected client and its provider, and pass through the Tyde connection service. This connection is not end-to-end encrypted."</p>
            </section>
        </div>
    }
}
