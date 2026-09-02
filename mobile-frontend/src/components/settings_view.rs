use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::components::backend_capacity::SubscriptionCapacitySection;
use crate::components::host_browser::HostBrowser;
use crate::components::ui::{Button, ButtonVariant, ConfirmModal, EmptyState, Pill, PillTone};
use crate::push::PushAvailability;
use crate::state::{AppState, PairedHostSummary, ToolOutputMode};

const STORAGE_TOOL_OUTPUT_MODE: &str = "tyde-mobile-tool-output-mode";
const TOOL_OUTPUT_MODE_SUMMARY: &str = "summary";
const TOOL_OUTPUT_MODE_COMPACT: &str = "compact";
const TOOL_OUTPUT_MODE_FULL: &str = "full";

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn tool_output_mode_to_str(mode: ToolOutputMode) -> &'static str {
    match mode {
        ToolOutputMode::Summary => TOOL_OUTPUT_MODE_SUMMARY,
        ToolOutputMode::Compact => TOOL_OUTPUT_MODE_COMPACT,
        ToolOutputMode::Full => TOOL_OUTPUT_MODE_FULL,
    }
}

fn tool_output_mode_from_str(s: &str) -> Option<ToolOutputMode> {
    match s {
        TOOL_OUTPUT_MODE_SUMMARY => Some(ToolOutputMode::Summary),
        TOOL_OUTPUT_MODE_COMPACT => Some(ToolOutputMode::Compact),
        TOOL_OUTPUT_MODE_FULL => Some(ToolOutputMode::Full),
        _ => None,
    }
}

pub fn persist_tool_output_mode(mode: ToolOutputMode) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(STORAGE_TOOL_OUTPUT_MODE, tool_output_mode_to_str(mode));
    }
}

pub fn restore_appearance(state: &AppState) {
    let Some(storage) = local_storage() else {
        return;
    };
    match storage.get_item(STORAGE_TOOL_OUTPUT_MODE) {
        Ok(Some(raw)) => match tool_output_mode_from_str(&raw) {
            Some(mode) => state.tool_output_mode.set(mode),
            None => {
                log::warn!(
                    "unrecognized tool_output_mode in localStorage: {raw:?}; resetting to default"
                );
                let default = state.tool_output_mode.get_untracked();
                persist_tool_output_mode(default);
            }
        },
        Ok(None) => persist_tool_output_mode(state.tool_output_mode.get_untracked()),
        Err(e) => log::warn!("failed to read tool_output_mode from localStorage: {e:?}"),
    }
}

/// One label/control line of a settings group. Every row is at least a full
/// touch target tall so the screen reads as a list, not a form.
#[component]
fn SettingsRow(#[prop(into)] label: String, children: Children) -> impl IntoView {
    view! {
        <div class="settings-row" data-mobile-test="settings-row">
            <span class="settings-label">{label}</span>
            {children()}
        </div>
    }
}

/// A group with nothing to list.
#[component]
fn SettingsEmpty(
    #[prop(into)] text: String,
    #[prop(optional)] data_mobile_test: Option<&'static str>,
) -> impl IntoView {
    view! {
        <div class="settings-group">
            <p class="settings-empty" data-mobile-test=data_mobile_test>{text}</p>
        </div>
    }
}

fn backend_setup_status(status: &protocol::BackendSetupStatus) -> (&'static str, PillTone) {
    match status {
        protocol::BackendSetupStatus::Installed => ("Installed", PillTone::Success),
        protocol::BackendSetupStatus::NotInstalled => ("Not installed", PillTone::Error),
        protocol::BackendSetupStatus::Unavailable => ("Unavailable", PillTone::Error),
        protocol::BackendSetupStatus::Unsupported => ("Unsupported", PillTone::Error),
    }
}

/// Whether the browser will deliver notifications is a browser-local fact the
/// host cannot observe, so unlike the rest of this app it is read from the
/// platform rather than from server state. What the *host* knows — that it holds
/// a live subscription for a device — is rendered in the desktop device list.
///
/// "On" means the browser holds a push subscription, not merely that permission
/// was granted. Permission survives the browser dropping the subscription, and
/// in that state every host silently has nothing to deliver to; the section
/// must say so and offer the way back, or the only control left is "Turn off".
#[component]
fn NotificationsSection() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let availability = RwSignal::new(PushAvailability::Unsupported);
    let subscribed = RwSignal::new(Option::<bool>::None);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);

    // Read once on mount, then after every action, so the row always reflects
    // the permission and subscription the browser actually holds.
    let refresh = move || {
        availability.set(crate::push::availability());
        spawn_local(async move {
            match crate::push::subscribed().await {
                Ok(value) => subscribed.set(Some(value)),
                Err(message) => {
                    subscribed.set(Some(false));
                    error.set(Some(format!("Notifications unavailable: {message}")));
                }
            }
        });
    };
    refresh();

    let enable = {
        let state = state.clone();
        move |_| {
            if busy.get_untracked() {
                return;
            }
            busy.set(true);
            error.set(None);
            let state = state.clone();
            spawn_local(async move {
                if let Err(message) = crate::actions::enable_push_notifications(&state).await {
                    error.set(Some(message));
                }
                busy.set(false);
                refresh();
            });
        }
    };

    let disable = {
        let state = state.clone();
        move |_| {
            if busy.get_untracked() {
                return;
            }
            busy.set(true);
            error.set(None);
            let state = state.clone();
            spawn_local(async move {
                if let Err(message) = crate::actions::disable_push_notifications(&state).await {
                    error.set(Some(message));
                }
                busy.set(false);
                refresh();
            });
        }
    };

    view! {
        <section class="settings-section" data-mobile-test="settings-notifications">
            <h2 class="settings-section-title">"Notifications"</h2>
            <div class="settings-group">
                {move || match availability.get() {
                    PushAvailability::Unsupported => view! {
                        <p class="settings-note">
                            "This browser cannot receive notifications. On iPhone, add Tyde to your Home Screen and open it from there."
                        </p>
                    }.into_any(),
                    PushAvailability::Denied => view! {
                        <p class="settings-note">
                            "Notifications are blocked for this site. Re-enable them in your browser settings."
                        </p>
                    }.into_any(),
                    PushAvailability::Prompt => view! {
                        <>
                            <p class="settings-note">
                                "Get notified when an agent finishes a turn or asks you something."
                            </p>
                            <div class="settings-row settings-row-action">
                                <Button
                                    label="Enable notifications"
                                    variant=ButtonVariant::Primary
                                    full_width=true
                                    data_mobile_test="settings-notifications-enable"
                                    disabled=Signal::derive(move || busy.get())
                                    on_click=Callback::new(enable.clone())
                                />
                            </div>
                        </>
                    }.into_any(),
                    PushAvailability::Granted => match subscribed.get() {
                        None => view! {
                            <SettingsRow label="Agent idle alerts">
                                <span class="settings-value">"Checking…"</span>
                            </SettingsRow>
                        }.into_any(),
                        Some(true) => view! {
                            <>
                                <SettingsRow label="Agent idle alerts">
                                    <span class="settings-value" data-mobile-test="settings-notifications-state">"On"</span>
                                </SettingsRow>
                                <div class="settings-row settings-row-action">
                                    <Button
                                        label="Turn off"
                                        variant=ButtonVariant::Secondary
                                        full_width=true
                                        data_mobile_test="settings-notifications-disable"
                                        disabled=Signal::derive(move || busy.get())
                                        on_click=Callback::new(disable.clone())
                                    />
                                </div>
                            </>
                        }.into_any(),
                        Some(false) => view! {
                            <>
                                <SettingsRow label="Agent idle alerts">
                                    <span class="settings-value" data-mobile-test="settings-notifications-state">"Off"</span>
                                </SettingsRow>
                                <p class="settings-note">
                                    "Notifications are allowed, but this device is not subscribed, so your hosts cannot reach it. Turn them on to subscribe again."
                                </p>
                                <div class="settings-row settings-row-action">
                                    <Button
                                        label="Turn on"
                                        variant=ButtonVariant::Primary
                                        full_width=true
                                        data_mobile_test="settings-notifications-enable"
                                        disabled=Signal::derive(move || busy.get())
                                        on_click=Callback::new(enable.clone())
                                    />
                                </div>
                            </>
                        }.into_any(),
                    },
                }}
                {move || error.get().map(|message| view! {
                    <p class="settings-error" role="alert" data-mobile-test="settings-notifications-error">
                        {message}
                    </p>
                })}
            </div>
        </section>
    }
}

#[component]
pub fn SettingsView() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    view! {
        <div class="view settings-view" data-mobile-test="settings-view">
            <div class="view-header">
                <h1 class="view-title">"Settings"</h1>
            </div>
            <div class="view-body settings-body">
                <section class="settings-section" data-mobile-test="settings-appearance">
                    <h2 class="settings-section-title">"Appearance"</h2>
                    <div class="settings-group">
                        <SettingsRow label="Theme">
                            <select
                                class="settings-select"
                                data-mobile-test="settings-theme"
                                aria-label="Theme"
                                prop:value=move || state.theme.get()
                                on:change=move |ev| {
                                    state.theme.set(event_target_value(&ev));
                                }
                            >
                                <option value="dark">"Dark"</option>
                                <option value="light">"Light"</option>
                            </select>
                        </SettingsRow>
                        <SettingsRow label="Tool output">
                            <select
                                class="settings-select"
                                data-mobile-test="settings-tool-output"
                                aria-label="Tool output mode"
                                prop:value=move || tool_output_mode_to_str(state.tool_output_mode.get()).to_owned()
                                on:change=move |ev| {
                                    let raw = event_target_value(&ev);
                                    if let Some(mode) = tool_output_mode_from_str(&raw) {
                                        state.tool_output_mode.set(mode);
                                        persist_tool_output_mode(mode);
                                    }
                                }
                            >
                                <option value=TOOL_OUTPUT_MODE_SUMMARY>"Summary"</option>
                                <option value=TOOL_OUTPUT_MODE_COMPACT>"Compact"</option>
                                <option value=TOOL_OUTPUT_MODE_FULL>"Full"</option>
                            </select>
                        </SettingsRow>
                    </div>
                </section>

                <PairedHostSection />

                <NotificationsSection />

                <section class="settings-section" data-mobile-test="settings-host">
                    <h2 class="settings-section-title">"Host"</h2>
                    {let state = state.clone(); move || {
                        match state.active_host_settings() {
                            Some(hs) => {
                                let backends: Vec<String> = hs.enabled_backends.iter().map(|b| format!("{b:?}")).collect();
                                let default = hs.default_backend.map(|b| format!("{b:?}")).unwrap_or_else(|| "None".to_string());
                                view! {
                                    <div class="settings-group">
                                        <SettingsRow label="Enabled backends">
                                            <span class="settings-value">{backends.join(", ")}</span>
                                        </SettingsRow>
                                        <SettingsRow label="Default backend">
                                            <span class="settings-value">{default}</span>
                                        </SettingsRow>
                                    </div>
                                }.into_any()
                            }
                            None => view! { <SettingsEmpty text="Not connected to a host" /> }.into_any(),
                        }
                    }}
                </section>

                <section class="settings-section" data-mobile-test="settings-backend-setup">
                    <h2 class="settings-section-title">"Backend setup"</h2>
                    {let state = state.clone(); move || {
                        let setup = state.active_host_backend_setup();
                        if setup.is_empty() {
                            return view! { <SettingsEmpty text="No backend setup info" /> }.into_any();
                        }
                        view! {
                            <div class="settings-group" data-mobile-test="settings-backend-setup-list">
                                {setup.iter().map(|info| {
                                    let (label, tone) = backend_setup_status(&info.status);
                                    view! {
                                        <SettingsRow label=format!("{:?}", info.backend_kind)>
                                            <Pill label=label tone=tone data_mobile_test="settings-backend-status" />
                                        </SettingsRow>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        }.into_any()
                    }}
                </section>

                <SubscriptionCapacitySection />

                <section class="settings-section" data-mobile-test="settings-custom-agents">
                    <h2 class="settings-section-title">"Custom agents"</h2>
                    {let state = state.clone(); move || {
                        let agents = state.active_host_custom_agents();
                        if agents.is_empty() {
                            return view! {
                                <SettingsEmpty
                                    text="No custom agents configured"
                                    data_mobile_test="settings-custom-agents-empty"
                                />
                            }.into_any();
                        }
                        let mut sorted: Vec<_> = agents.into_values().collect();
                        sorted.sort_by(|a, b| a.name.cmp(&b.name));
                        view! {
                            <div class="settings-group settings-list" data-mobile-test="settings-custom-agents-list">
                                {sorted.into_iter().map(|agent| {
                                    let name = agent.name.clone();
                                    let desc = agent.description.clone();
                                    view! {
                                        <div class="settings-item" data-mobile-test="settings-custom-agent-row">
                                            <span class="settings-item-title">{name}</span>
                                            <span class="settings-item-subtitle">{desc}</span>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        }.into_any()
                    }}
                </section>

                <McpServersSection />
                <SteeringSection />
                <SkillsSection />
                <HostToolsSection />

                <section class="settings-section" data-mobile-test="settings-native-voice">
                    <h2 class="settings-section-title">"Native voice"</h2>
                    {move || {
                        let settings = state
                            .active_local_host_id
                            .get()
                            .and_then(|host| state.host_settings_by_host.with(|values| values.get(&host).cloned()));
                        match settings {
                            Some(settings) => {
                                let status = if settings.voice.enabled { "Enabled" } else { "Disabled" };
                                let turn_ending = match settings.voice.endpointing_sensitivity {
                                    settings_model::VoiceEndpointingSensitivity::High => "Fast",
                                    settings_model::VoiceEndpointingSensitivity::Medium => "Balanced",
                                    settings_model::VoiceEndpointingSensitivity::Low => "Patient",
                                };
                                view! {
                                    <div class="settings-group">
                                        <SettingsRow label="Status">
                                            <span class="settings-value">{status}</span>
                                        </SettingsRow>
                                        <SettingsRow label="Model">
                                            <span class="settings-value">{settings.voice.nova_model}</span>
                                        </SettingsRow>
                                        <SettingsRow label="Turn ending">
                                            <span class="settings-value">{turn_ending}</span>
                                        </SettingsRow>
                                    </div>
                                    <p class="settings-footnote">
                                        "Voice capture is foreground-only and uses this device’s echo cancellation, noise suppression, and gain control. Configure AWS profile and region on the desktop host."
                                    </p>
                                }.into_any()
                            }
                            None => view! {
                                <SettingsEmpty text="Connect a host to see voice availability." />
                            }.into_any(),
                        }
                    }}
                </section>

                <section class="settings-section" data-mobile-test="settings-about">
                    <h2 class="settings-section-title">"About"</h2>
                    <div class="settings-group">
                        <SettingsRow label="App">
                            <span class="settings-value">"Tyde Mobile"</span>
                        </SettingsRow>
                        <SettingsRow label="Version">
                            <span class="settings-value">"0.1.0"</span>
                        </SettingsRow>
                    </div>
                </section>
            </div>
        </div>
    }
}

/// Per-paired-host card for the active host: shows the host_label, broker URL,
/// device ID, last-connected time, an auto-connect toggle, and a "Forget host"
/// button. Forget runs `bridge::forget_paired_host` and warns the user that
/// the desktop's `MobileDeviceRevoke` is the authoritative server-side revoke.
#[component]
fn PairedHostSection() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    view! {
        <section class="settings-section" data-mobile-test="settings-paired-host">
            <h2 class="settings-section-title">"Paired host"</h2>
            {move || {
                let Some(active_id) = state.active_local_host_id.get() else {
                    return view! {
                        <EmptyState
                            title="No host selected"
                            body="Pair a host to see its details here."
                            icon="\u{1F517}"
                            data_mobile_test="settings-paired-host-empty"
                        />
                    }
                    .into_any();
                };
                let Some(host) = state
                    .paired_hosts
                    .get()
                    .into_iter()
                    .find(|h| h.local_host_id == active_id)
                else {
                    return view! {
                        <EmptyState
                            title="Paired host not found"
                            body="The selected host has been removed. Pick another from the host picker."
                            icon="\u{26A0}"
                            data_mobile_test="settings-paired-host-missing"
                        />
                    }
                    .into_any();
                };
                view! { <PairedHostCard host=host /> }.into_any()
            }}
        </section>
    }
}

#[component]
fn PairedHostCard(host: PairedHostSummary) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let local_host_id = host.local_host_id.clone();
    let host_label = host.host_label.clone();
    // A host paired over its own origin has no broker or room to show; say so
    // rather than rendering an empty field that reads like missing data.
    let broker_url = host
        .broker
        .as_ref()
        .map(|broker| broker.url.to_string())
        .unwrap_or_else(|| "Hosted by this Tyde".to_owned());
    let room_id = host
        .room
        .as_ref()
        .map(|room| room.to_string())
        .unwrap_or_else(|| "—".to_owned());
    let credential_fingerprint = host.credential_fingerprint.clone();
    let last_connected = host
        .last_connected_at_ms
        .map(format_relative_time_ms)
        .unwrap_or_else(|| "Never".to_string());

    // Phase C MEDIUM: bind the auto-connect checkbox reactively — re-read
    // `paired_hosts` for this host on every render. The checkbox is a pure
    // projection of the bridge's state; on click we fire the command and
    // wait for the `paired-hosts-changed` event to flip the projection.
    let id_for_checked = local_host_id.clone();
    let state_for_checked = state.clone();
    let auto_connect_checked = move || {
        state_for_checked
            .paired_hosts
            .get()
            .into_iter()
            .find(|h| h.local_host_id == id_for_checked)
            .map(|h| h.auto_connect)
            .unwrap_or(false)
    };

    let id_for_toggle = local_host_id.clone();
    let on_toggle_auto = move |ev: web_sys::Event| {
        let target: web_sys::HtmlInputElement = event_target(&ev);
        let next = target.checked();
        let id = id_for_toggle.clone();
        spawn_local(async move {
            if let Err(error) = bridge::set_paired_host_auto_connect(&id, next).await {
                log::error!("set_paired_host_auto_connect({id}, {next}) failed: {error}");
            }
        });
    };

    // Destructive forget is gated by an in-app confirmation modal (never
    // `window.confirm`, which is a no-op in the Tauri webview and unstyled in
    // the browser). The same modal serves both bridge backends.
    let confirming_forget = RwSignal::new(false);
    let on_request_forget = Callback::new(move |_: ()| confirming_forget.set(true));
    let on_cancel_forget = Callback::new(move |_: ()| confirming_forget.set(false));

    let id_for_forget = local_host_id.clone();
    let state_for_forget = state.clone();
    let forget_host_label = host.host_label.clone();
    let on_confirm_forget = Callback::new(move |_: ()| {
        confirming_forget.set(false);
        let id = id_for_forget.clone();
        let state = state_for_forget.clone();
        spawn_local(async move {
            if let Err(error) = bridge::forget_paired_host(&id).await {
                log::error!("forget_paired_host({id}) failed: {error}");
                return;
            }
            state.clear_host_runtime(&id);
        });
    });

    view! {
        <div class="settings-group">
            <SettingsRow label="Label">
                <span class="settings-value">{host_label}</span>
            </SettingsRow>
            <SettingsRow label="Broker">
                <span class="settings-value settings-value-mono" data-mobile-test="settings-broker-url">
                    {broker_url}
                </span>
            </SettingsRow>
            <SettingsRow label="Room">
                <span class="settings-value settings-value-mono">{room_id}</span>
            </SettingsRow>
            <SettingsRow label="Credential">
                <span class="settings-value settings-value-mono">{credential_fingerprint}</span>
            </SettingsRow>
            <SettingsRow label="Last connected">
                <span class="settings-value">{last_connected}</span>
            </SettingsRow>
            // The whole row is the switch's label, so the tap target is the row.
            <label class="settings-row settings-row-toggle" data-mobile-test="settings-row">
                <span class="settings-label">"Auto-connect"</span>
                <input
                    class="settings-toggle"
                    type="checkbox"
                    role="switch"
                    data-mobile-test="settings-auto-connect"
                    prop:checked=auto_connect_checked
                    on:change=on_toggle_auto
                />
            </label>
            <div class="settings-row settings-row-action">
                <Button
                    label="Forget host"
                    variant=ButtonVariant::Destructive
                    full_width=true
                    data_mobile_test="settings-forget-host"
                    aria_label="Forget paired host on this device".to_string()
                    on_click=on_request_forget
                />
            </div>
        </div>
        <p class="settings-footnote" data-mobile-test="settings-forget-host-hint">
            "Forget removes the pairing on this device only. To revoke server-side, use Settings → Mobile on the desktop."
        </p>
        <ConfirmModal
            open=confirming_forget
            title="Forget host?"
            message=format!("This removes the saved pairing for \"{forget_host_label}\" on this device. You can re-pair from the host's QR.")
            confirm_label="Forget"
            cancel_label="Cancel"
            destructive=true
            data_mobile_test="settings-forget-host-modal"
            on_confirm=on_confirm_forget
            on_cancel=on_cancel_forget
        />
    }
}

/// Lists every MCP server defined on the active host. Read-only —
/// editing happens on desktop; the mobile UI surfaces the inventory so
/// users can confirm what's installed before spawning a chat that
/// depends on it.
#[component]
fn McpServersSection() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    view! {
        <section class="settings-section" data-mobile-test="settings-mcp-servers">
            <h2 class="settings-section-title">"MCP servers"</h2>
            {move || {
                let Some(host) = state.active_local_host_id.get() else {
                    return view! { <SettingsEmpty text="Not connected to a host" /> }.into_any();
                };
                let servers = state
                    .mcp_servers_by_host
                    .with(|m| m.get(&host).cloned())
                    .unwrap_or_default();
                if servers.is_empty() {
                    return view! {
                        <SettingsEmpty
                            text="No MCP servers configured"
                            data_mobile_test="settings-mcp-servers-empty"
                        />
                    }.into_any();
                }
                let mut sorted: Vec<_> = servers.into_values().collect();
                sorted.sort_by(|a, b| a.name.cmp(&b.name));
                view! {
                    <div class="settings-group settings-list" data-mobile-test="settings-mcp-servers-list">
                        {sorted.into_iter().map(|server| {
                            let name = server.name.clone();
                            let transport = mcp_transport_label(&server.transport);
                            view! {
                                <div class="settings-item" data-mobile-test="settings-mcp-server-row">
                                    <span class="settings-item-title">{name}</span>
                                    <span class="settings-item-subtitle">{transport}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                }.into_any()
            }}
        </section>
    }
}

fn mcp_transport_label(transport: &protocol::McpTransportConfig) -> String {
    match transport {
        protocol::McpTransportConfig::Http { url, .. } => format!("HTTP — {url}"),
        protocol::McpTransportConfig::Stdio { command, args, .. } => {
            if args.is_empty() {
                format!("stdio — {command}")
            } else {
                format!("stdio — {command} {}", args.join(" "))
            }
        }
    }
}

/// Lists every steering document on the active host, scoped by host or
/// per-project so users can tell which projects each one influences.
#[component]
fn SteeringSection() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    view! {
        <section class="settings-section" data-mobile-test="settings-steering">
            <h2 class="settings-section-title">"Steering"</h2>
            {move || {
                let Some(host) = state.active_local_host_id.get() else {
                    return view! { <SettingsEmpty text="Not connected to a host" /> }.into_any();
                };
                let docs = state
                    .steering_by_host
                    .with(|m| m.get(&host).cloned())
                    .unwrap_or_default();
                if docs.is_empty() {
                    return view! {
                        <SettingsEmpty
                            text="No steering documents configured"
                            data_mobile_test="settings-steering-empty"
                        />
                    }.into_any();
                }
                let mut sorted: Vec<_> = docs.into_values().collect();
                sorted.sort_by(|a, b| a.title.cmp(&b.title));
                view! {
                    <div class="settings-group settings-list" data-mobile-test="settings-steering-list">
                        {sorted.into_iter().map(|doc| {
                            let title = doc.title.clone();
                            let scope_label = match doc.scope {
                                protocol::SteeringScope::Host => "Host-wide".to_string(),
                                protocol::SteeringScope::Project(pid) => format!("Project: {}", pid.0),
                            };
                            view! {
                                <div class="settings-item" data-mobile-test="settings-steering-row">
                                    <span class="settings-item-title">{title}</span>
                                    <span class="settings-item-subtitle">{scope_label}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                }.into_any()
            }}
        </section>
    }
}

/// Lists every skill available on the active host. Mostly informational
/// for v1; users can spawn a chat that uses them via custom agents.
#[component]
fn SkillsSection() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    view! {
        <section class="settings-section" data-mobile-test="settings-skills">
            <h2 class="settings-section-title">"Skills"</h2>
            {move || {
                let Some(host) = state.active_local_host_id.get() else {
                    return view! { <SettingsEmpty text="Not connected to a host" /> }.into_any();
                };
                let skills = state
                    .skills_by_host
                    .with(|m| m.get(&host).cloned())
                    .unwrap_or_default();
                if skills.is_empty() {
                    return view! {
                        <SettingsEmpty
                            text="No skills installed"
                            data_mobile_test="settings-skills-empty"
                        />
                    }.into_any();
                }
                let mut sorted: Vec<_> = skills.into_values().collect();
                sorted.sort_by(|a, b| a.name.cmp(&b.name));
                view! {
                    <div class="settings-group settings-list" data-mobile-test="settings-skills-list">
                        {sorted.into_iter().map(|skill| {
                            let display = skill.title.clone().unwrap_or_else(|| skill.name.clone());
                            let subtitle = skill.description.clone().unwrap_or_else(|| skill.name.clone());
                            view! {
                                <div class="settings-item" data-mobile-test="settings-skill-row">
                                    <span class="settings-item-title">{display}</span>
                                    <span class="settings-item-subtitle">{subtitle}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                }.into_any()
            }}
        </section>
    }
}

fn event_target<T: wasm_bindgen::JsCast>(ev: &web_sys::Event) -> T {
    ev.target()
        .expect("event must have a target")
        .dyn_into::<T>()
        .expect("event target type mismatch")
}

/// Host filesystem browsing from Settings. Mobile intentionally does not
/// expose terminals; terminal control is desktop-only.
#[component]
fn HostToolsSection() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let browse_stream: RwSignal<Option<protocol::StreamPath>> = RwSignal::new(None);

    let state_for_open_browser = state.clone();
    let on_open_browser = Callback::new(move |_: ()| {
        let Some(host) = state_for_open_browser.active_local_host_id.get_untracked() else {
            return;
        };
        let state = state_for_open_browser.clone();
        spawn_local(async move {
            match crate::actions::start_host_browse(
                &state,
                &host,
                protocol::HostBrowseInitial::Home,
                false,
            )
            .await
            {
                Ok(stream) => browse_stream.set(Some(stream)),
                Err(e) => log::error!("start_host_browse failed: {e}"),
            }
        });
    });
    let state_for_close_browser = state.clone();
    let on_close_browser = Callback::new(move |_: ()| {
        let Some(host) = state_for_close_browser.active_local_host_id.get_untracked() else {
            browse_stream.set(None);
            return;
        };
        let Some(stream) = browse_stream.get_untracked() else {
            return;
        };
        let state = state_for_close_browser.clone();
        spawn_local(async move {
            let _ = crate::actions::close_host_browse(&state, &host, stream).await;
        });
        browse_stream.set(None);
    });
    let on_select_path = Callback::new(move |path: protocol::HostAbsPath| {
        log::info!("host browser selected path: {}", path.0);
        // v1: just close. The "add this as a project root" flow can
        // land later — protocol payloads (`ProjectAddRootPayload`) are
        // ready but the UX is outside this slice.
    });

    view! {
        <section class="settings-section" data-mobile-test="settings-host-tools">
            <h2 class="settings-section-title">"Host tools"</h2>
            <div class="settings-group">
                <SettingsRow label="Browse host filesystem">
                    <Button
                        label="Open"
                        variant=ButtonVariant::Ghost
                        data_mobile_test="settings-open-host-browser"
                        on_click=on_open_browser
                    />
                </SettingsRow>
            </div>
            {move || {
                let Some(stream) = browse_stream.get() else { return view! { <div></div> }.into_any(); };
                let Some(host) = state.active_local_host_id.get_untracked() else { return view! { <div></div> }.into_any(); };
                view! {
                    <div class="settings-overlay" data-mobile-test="settings-host-browser-overlay">
                        <HostBrowser
                            host=host
                            browse_stream=stream
                            on_close=on_close_browser
                            on_select=on_select_path
                        />
                    </div>
                }.into_any()
            }}
        </section>
    }
}

fn format_relative_time_ms(timestamp_ms: u64) -> String {
    let now = js_sys::Date::now() as u64;
    let diff_ms = now.saturating_sub(timestamp_ms);
    let minutes = diff_ms / 60_000;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if hours < 24 {
        format!("{hours}h ago")
    } else {
        format!("{days}d ago")
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, LocalHostId};
    use leptos::mount::mount_to;
    use protocol::{
        McpServerConfig, McpServerId, McpTransportConfig, ProjectId, Skill, SkillId, Steering,
        SteeringId, SteeringScope,
    };
    use std::collections::HashMap;
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

    /// Verifies the MCP / Steering / Skills sections render their
    /// per-row selectors when state is populated. Confirms the
    /// settings surface is wired to the dispatch outputs.
    #[wasm_bindgen_test]
    async fn settings_renders_mcp_steering_skills_when_populated() {
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.host_streams.update(|streams| {
                streams.insert(
                    host_for_mount.clone(),
                    protocol::StreamPath("/host/host-1".to_owned()),
                );
            });
            // MCP server (http transport).
            let mut mcp = HashMap::new();
            mcp.insert(
                McpServerId("m-1".to_owned()),
                McpServerConfig {
                    id: McpServerId("m-1".to_owned()),
                    name: "search-mcp".to_owned(),
                    supports_parallel_tool_calls: false,
                    transport: McpTransportConfig::Http {
                        url: "https://example.com/mcp".to_owned(),
                        headers: HashMap::new(),
                        bearer_token_env_var: None,
                    },
                },
            );
            state.mcp_servers_by_host.update(|m| {
                m.insert(host_for_mount.clone(), mcp);
            });
            // Steering doc (host scope).
            let mut steering = HashMap::new();
            steering.insert(
                SteeringId("s-1".to_owned()),
                Steering {
                    id: SteeringId("s-1".to_owned()),
                    scope: SteeringScope::Host,
                    title: "Style guide".to_owned(),
                    content: "Use 2-space indents".to_owned(),
                },
            );
            // Plus one with project scope, to exercise that branch.
            steering.insert(
                SteeringId("s-2".to_owned()),
                Steering {
                    id: SteeringId("s-2".to_owned()),
                    scope: SteeringScope::Project(ProjectId("p-1".to_owned())),
                    title: "Project rules".to_owned(),
                    content: "...".to_owned(),
                },
            );
            state.steering_by_host.update(|m| {
                m.insert(host_for_mount.clone(), steering);
            });
            // Skill.
            let mut skills = HashMap::new();
            skills.insert(
                SkillId("sk-1".to_owned()),
                Skill {
                    id: SkillId("sk-1".to_owned()),
                    name: "code-review".to_owned(),
                    title: Some("Code review".to_owned()),
                    description: Some("Reviews PRs".to_owned()),
                },
            );
            state.skills_by_host.update(|m| {
                m.insert(host_for_mount.clone(), skills);
            });
            provide_context(state);
            view! { <SettingsView /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        // MCP row visible with transport label.
        assert!(
            container
                .query_selector("[data-mobile-test='settings-mcp-server-row']")
                .unwrap()
                .is_some(),
            "MCP server row must render"
        );
        assert!(
            text.contains("search-mcp") && text.contains("HTTP"),
            "MCP row must show name and HTTP transport label"
        );
        // Steering rows.
        assert!(
            container
                .query_selector("[data-mobile-test='settings-steering-row']")
                .unwrap()
                .is_some(),
            "Steering row must render"
        );
        assert!(
            text.contains("Style guide") && text.contains("Host-wide"),
            "Steering host-scope label must render"
        );
        assert!(
            text.contains("Project rules") && text.contains("Project: p-1"),
            "Steering project-scope label must render"
        );
        // Skill row.
        assert!(
            container
                .query_selector("[data-mobile-test='settings-skill-row']")
                .unwrap()
                .is_some(),
            "Skill row must render"
        );
        assert!(
            text.contains("Code review"),
            "Skill display title must render"
        );
    }

    /// Empty state for MCP / Steering / Skills must show distinct
    /// empty selectors so tests can distinguish "not loaded" from
    /// "loaded but empty."
    #[wasm_bindgen_test]
    async fn settings_renders_empty_states_for_mcp_steering_skills() {
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            provide_context(state);
            view! { <SettingsView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='settings-mcp-servers-empty']")
                .unwrap()
                .is_some(),
            "MCP empty selector must render"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='settings-steering-empty']")
                .unwrap()
                .is_some(),
            "Steering empty selector must render"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='settings-skills-empty']")
                .unwrap()
                .is_some(),
            "Skills empty selector must render"
        );
    }

    /// Notifications must never render as a silently dead toggle. Whatever the
    /// browser's capability, the section says what the user can do about it:
    /// an actionable control when a prompt is possible, and a plain explanation
    /// when it is not. A section that rendered nothing would leave the user
    /// tapping a control that can never work.
    #[wasm_bindgen_test]
    async fn notifications_section_always_explains_what_the_user_can_do() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            provide_context(AppState::new());
            view! { <NotificationsSection /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Notifications"),
            "the notifications section must be titled, got {text:?}"
        );

        let document = web_sys::window().unwrap().document().unwrap();
        let enable = document
            .query_selector("[data-mobile-test=\"settings-notifications-enable\"]")
            .unwrap();

        match crate::push::availability() {
            PushAvailability::Prompt => {
                assert!(
                    enable.is_some(),
                    "a browser that can be asked for permission must offer the control; got {text:?}"
                );
                assert!(
                    text.contains("finishes a turn") || text.contains("asks you something"),
                    "the prompt state must say what the notifications are for, got {text:?}"
                );
            }
            PushAvailability::Unsupported => {
                assert!(
                    enable.is_none(),
                    "a browser without the Push API must not offer a control that cannot work"
                );
                assert!(
                    text.contains("Home Screen"),
                    "an unsupported browser must be told how to make it work, got {text:?}"
                );
            }
            PushAvailability::Denied => {
                assert!(
                    text.contains("blocked"),
                    "a blocked site must say so rather than offering a dead toggle, got {text:?}"
                );
            }
            PushAvailability::Granted => {
                // Granted permission is not the same as a live subscription:
                // the section reports the subscription and, when it is gone,
                // offers the way back rather than a lone "Turn off".
                let subscribed = crate::push::subscribed().await.unwrap_or(false);
                if subscribed {
                    assert!(
                        text.contains("On"),
                        "a subscribed browser must show notifications as on, got {text:?}"
                    );
                } else {
                    assert!(
                        text.contains("not subscribed") && enable.is_some(),
                        "a granted but unsubscribed browser must say so and offer Turn on, got {text:?}"
                    );
                }
            }
        }
    }

    fn host_summary_with_long_values(id: &str) -> PairedHostSummary {
        PairedHostSummary {
            local_host_id: LocalHostId(id.to_owned()),
            host_label: "Studio Mac".to_owned(),
            broker: Some(mobile_shell_types::BrokerEndpointSummary {
                url: protocol::BrokerUrl::new(
                    "wss://a1b2c3d4e5f6g7h8i9j0-ats.iot.us-west-2.amazonaws.com:443/mqtt/tyde-mobile-broker",
                )
                .unwrap(),
                auth: mobile_shell_types::BrokerAuthSummary::Anonymous,
            }),
            room: Some(mobile_shell_types::RoomIdSummary(
                "AQEBAQEBAQEBAQEBAQEBAQ".to_owned(),
            )),
            credential_fingerprint: "SHA256:8f3c1d9e7b2a4c6e0f1a2b3c4d5e6f70".to_owned(),
            auto_connect: true,
            last_connected_at_ms: Some(0),
        }
    }

    fn seed_populated_settings(state: &AppState, host: &LocalHostId) {
        state.active_local_host_id.set(Some(host.clone()));
        state.host_streams.update(|streams| {
            streams.insert(
                host.clone(),
                protocol::StreamPath("/host/host-1".to_owned()),
            );
        });
        state
            .paired_hosts
            .set(vec![host_summary_with_long_values(&host.0)]);
        state.host_settings_by_host.update(|m| {
            m.insert(
                host.clone(),
                settings_model::HostSettings {
                    enabled_backends: vec![
                        protocol::BackendKind::Claude,
                        protocol::BackendKind::Codex,
                    ],
                    default_backend: Some(protocol::BackendKind::Claude),
                    ..settings_model::HostSettings::default()
                },
            );
        });
        state.backend_setup_by_host.update(|m| {
            m.insert(
                host.clone(),
                vec![
                    protocol::BackendSetupInfo {
                        backend_kind: protocol::BackendKind::Claude,
                        status: protocol::BackendSetupStatus::Installed,
                        installed_version: Some("2.1.0".to_owned()),
                        docs_url: "https://example.test/claude".to_owned(),
                        install_command: None,
                        diagnostic: None,
                        sign_in_command: None,
                    },
                    protocol::BackendSetupInfo {
                        backend_kind: protocol::BackendKind::Codex,
                        status: protocol::BackendSetupStatus::NotInstalled,
                        installed_version: None,
                        docs_url: "https://example.test/codex".to_owned(),
                        install_command: None,
                        diagnostic: None,
                        sign_in_command: None,
                    },
                ],
            );
        });
        let mut skills = HashMap::new();
        skills.insert(
            SkillId("sk-1".to_owned()),
            Skill {
                id: SkillId("sk-1".to_owned()),
                name: "release-notes".to_owned(),
                title: Some("Release notes".to_owned()),
                description: Some(
                    "Drafts release notes from the merged pull requests since the last tag, \
                     groups them by area, and flags anything that needs a migration note \
                     before publishing."
                        .to_owned(),
                ),
            },
        );
        state.skills_by_host.update(|m| {
            m.insert(host.clone(), skills);
        });
        let mut mcp = HashMap::new();
        mcp.insert(
            McpServerId("m-1".to_owned()),
            McpServerConfig {
                id: McpServerId("m-1".to_owned()),
                name: "filesystem".to_owned(),
                supports_parallel_tool_calls: false,
                transport: McpTransportConfig::Http {
                    url: "https://mcp.example.test/workspaces/tyde/some/very/long/path/that/keeps/going/v1/endpoint"
                        .to_owned(),
                    headers: HashMap::new(),
                    bearer_token_env_var: None,
                },
            },
        );
        state.mcp_servers_by_host.update(|m| {
            m.insert(host.clone(), mcp);
        });
    }

    fn rect_of(element: &web_sys::Element) -> web_sys::DomRect {
        element.get_bounding_client_rect()
    }

    /// Settings reads as a deliberate, touchable list at phone width: every
    /// control is at least a 44pt target, nothing runs off the right edge,
    /// no row value is clipped, each row's label and value sit side by side
    /// on one centre line, and the Forget hint sits under its button rather
    /// than crushed beside it.
    #[wasm_bindgen_test]
    async fn settings_lays_out_as_a_touchable_list_at_phone_width() {
        crate::components::test_styles::ensure_styles_loaded();
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        container.style().set_property("width", "390px").unwrap();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            seed_populated_settings(&state, &host_for_mount);
            provide_context(state);
            view! { <SettingsView /> }
        });
        next_tick().await;
        next_tick().await;

        // Every control is a full touch target. A checkbox is tapped through
        // its row, so its target is the enclosing label.
        let controls = container
            .query_selector_all("button, select, input")
            .unwrap();
        let mut measured = 0;
        for index in 0..controls.length() {
            let control: web_sys::Element = controls.get(index).unwrap().dyn_into().unwrap();
            let target = control
                .closest("label")
                .unwrap()
                .unwrap_or_else(|| control.clone());
            let rect = rect_of(&target);
            if rect.width() == 0.0 && rect.height() == 0.0 {
                continue;
            }
            measured += 1;
            assert!(
                rect.height() >= 44.0 && rect.width() >= 44.0,
                "{} #{index} must be at least 44pt on both axes, got {}x{}",
                control.tag_name(),
                rect.width(),
                rect.height()
            );
        }
        assert!(
            measured >= 5,
            "expected the two selects, the toggle, and the action buttons; measured {measured}"
        );

        // Nothing runs off the right edge at phone width.
        let view = container
            .query_selector("[data-mobile-test='settings-view']")
            .unwrap()
            .expect("settings view");
        assert!(
            view.scroll_width() <= view.client_width(),
            "settings must not overflow horizontally: {} > {}",
            view.scroll_width(),
            view.client_width()
        );

        // Each row: label then value, no overlap, one centre line, no clipping.
        let rows = container
            .query_selector_all("[data-mobile-test='settings-row']")
            .unwrap();
        let mut checked = 0;
        for index in 0..rows.length() {
            let row: web_sys::Element = rows.get(index).unwrap().dyn_into().unwrap();
            if row.child_element_count() < 2 {
                continue;
            }
            let label = row.first_element_child().unwrap();
            let value = row.last_element_child().unwrap();
            let (l, v) = (rect_of(&label), rect_of(&value));
            let text = label.text_content().unwrap_or_default();
            assert!(
                v.left() >= l.right() - 0.5,
                "row '{text}': value must not overlap its label ({} < {})",
                v.left(),
                l.right()
            );
            let (lc, vc) = (l.top() + l.height() / 2.0, v.top() + v.height() / 2.0);
            assert!(
                (lc - vc).abs() <= 6.0,
                "row '{text}': label and value must share a centre line ({lc} vs {vc})"
            );
            assert!(
                value.scroll_width() <= value.client_width() + 1,
                "row '{text}': the value must wrap, not clip ({} > {})",
                value.scroll_width(),
                value.client_width()
            );
            assert!(
                rect_of(&row).height() >= 44.0,
                "row '{text}' must be at least a touch target tall"
            );
            checked += 1;
        }
        assert!(
            checked >= 12,
            "expected the host, appearance, voice and about rows; checked {checked}"
        );

        // The Forget hint explains the button; it belongs under it.
        let forget = container
            .query_selector("[data-mobile-test='settings-forget-host']")
            .unwrap()
            .expect("forget button");
        let hint = container
            .query_selector("[data-mobile-test='settings-forget-host-hint']")
            .unwrap()
            .expect("forget hint");
        assert!(
            rect_of(&hint).top() >= rect_of(&forget).bottom(),
            "the forget hint must sit below the button, not beside it"
        );

        // The long broker URL is shown in full, wrapped.
        let broker = container
            .query_selector("[data-mobile-test='settings-broker-url']")
            .unwrap()
            .expect("broker url");
        assert!(
            broker
                .text_content()
                .unwrap_or_default()
                .contains("tyde-mobile-broker"),
            "the broker URL is shown to the end"
        );
        let status_text = container.text_content().unwrap_or_default();
        assert!(
            status_text.contains("Installed") && status_text.contains("Not installed"),
            "backend setup states are spelled out, got {status_text:?}"
        );
    }
}
