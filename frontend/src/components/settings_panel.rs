use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::app::{connect_one_host, refresh_configured_hosts};
use crate::bridge::{self, HostTransportConfig as BridgeHostTransportConfig};
use crate::send::send_frame;
use crate::state::AppState;

use protocol::{BackendKind, DumpSettingsPayload, FrameKind, HostSettingValue, SetSettingPayload};

const STORAGE_THEME: &str = "tyde-theme";
const STORAGE_FONT_SIZE: &str = "tyde-font-size";
const STORAGE_FONT_FAMILY: &str = "tyde-font-family";

const FONT_FAMILIES: &[(&str, &str, &str)] = &[
    (
        "system",
        "System Default",
        "system-ui, -apple-system, sans-serif",
    ),
    (
        "mono",
        "Monospace",
        "\"Cascadia Code\", \"Fira Code\", Consolas, monospace",
    ),
    ("inter", "Inter", "\"Inter\", system-ui, sans-serif"),
    (
        "sf",
        "SF Pro",
        "\"-apple-system\", \"SF Pro Text\", system-ui, sans-serif",
    ),
];

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn document_element() -> Option<web_sys::HtmlElement> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
        .map(|el| {
            let el: web_sys::HtmlElement = el.unchecked_into();
            el
        })
}

/// Apply theme to the DOM and persist to localStorage.
fn apply_theme(theme: &str) {
    if let Some(el) = document_element() {
        let _ = el.set_attribute("data-theme", theme);
    }
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(STORAGE_THEME, theme);
    }
}

/// Apply font size to the DOM and persist to localStorage.
fn apply_font_size(size: u32) {
    if let Some(el) = document_element() {
        let _ = el
            .style()
            .set_property("--base-font-size", &format!("{size}px"));
    }
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(STORAGE_FONT_SIZE, &size.to_string());
    }
}

/// Apply font family to the DOM and persist to localStorage.
fn apply_font_family(key: &str) {
    let css_value = FONT_FAMILIES
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, _, css)| *css)
        .unwrap_or("system-ui, -apple-system, sans-serif");

    if let Some(el) = document_element() {
        let _ = el.style().set_property("--font-sans", css_value);
    }
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(STORAGE_FONT_FAMILY, key);
    }
}

/// Restore appearance settings from localStorage into AppState and apply to DOM.
/// Called once at startup.
pub fn restore_appearance(state: &AppState) {
    let storage = match local_storage() {
        Some(s) => s,
        None => return,
    };

    if let Ok(Some(theme)) = storage.get_item(STORAGE_THEME) {
        apply_theme(&theme);
        state.theme.set(theme);
    }

    if let Ok(Some(size_str)) = storage.get_item(STORAGE_FONT_SIZE)
        && let Ok(size) = size_str.parse::<u32>()
    {
        apply_font_size(size);
        state.font_size.set(size);
    }

    if let Ok(Some(family)) = storage.get_item(STORAGE_FONT_FAMILY) {
        apply_font_family(&family);
        state.font_family.set(family);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SettingsTab {
    Hosts,
    Appearance,
    General,
    Backends,
    Debug,
}

impl SettingsTab {
    fn label(self) -> &'static str {
        match self {
            Self::Hosts => "Hosts",
            Self::Appearance => "Appearance",
            Self::General => "General",
            Self::Backends => "Backends",
            Self::Debug => "Debug",
        }
    }

    /// All searchable text for this tab: labels, descriptions, option names.
    fn search_text(self) -> &'static [&'static str] {
        match self {
            Self::Hosts => &[
                "Hosts",
                "Configured Hosts",
                "Add remote host",
                "SSH destination",
                "Remote command",
                "Auto-connect",
                "Select host",
                "Connect",
                "Disconnect",
                "Remove host",
            ],
            Self::Appearance => &[
                "Appearance",
                "Color Theme",
                "Choose the color scheme for the interface",
                "Dark",
                "Light",
                "System",
                "Font Size",
                "Adjust the base font size used throughout the interface",
                "Font Family",
                "Select the font family for UI text",
                "Monospace",
            ],
            Self::General => &[
                "General",
                "Auto-connect on Launch",
                "Automatically connect to the host server when the application starts",
                "Connection",
            ],
            Self::Backends => &[
                "Backends",
                "Default Backend",
                "The backend to use by default when creating new agents",
                "Enabled Backends",
                "Toggle which backends are available for creating agents",
                "Tycode",
                "Kiro",
                "Claude",
                "Codex",
                "Gemini",
                "Anthropic",
                "OpenAI",
                "Google",
            ],
            Self::Debug => &[
                "Debug",
                "Tyde Debug MCP",
                "Enable the Tyde debug MCP server for new chats",
                "JavaScript evaluation",
                "Frontend debugging",
                "MCP server",
            ],
        }
    }

    fn matches_query(self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let q = query.to_lowercase();
        self.search_text()
            .iter()
            .any(|text| text.to_lowercase().contains(&q))
    }
}

const ALL_TABS: [SettingsTab; 5] = [
    SettingsTab::Hosts,
    SettingsTab::Appearance,
    SettingsTab::General,
    SettingsTab::Backends,
    SettingsTab::Debug,
];

#[component]
pub fn SettingsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let active_tab = RwSignal::new(SettingsTab::Appearance);
    let search_query = RwSignal::new(String::new());

    let state_for_refresh = state.clone();
    Effect::new(move |_| {
        if state_for_refresh.settings_open.get() {
            request_host_settings(&state_for_refresh);
            let state = state_for_refresh.clone();
            spawn_local(async move {
                refresh_configured_hosts(&state).await;
            });
        }
    });

    let on_close = move |_| {
        state.settings_open.set(false);
    };

    let on_search = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlInputElement = target.unchecked_into();
        search_query.set(el.value());
    };

    view! {
        <Show when=move || state.settings_open.get()>
            <div class="settings-overlay">
                <div class="settings-root">
                    <button class="settings-close-btn" on:click=on_close title="Close settings">"×"</button>

                    <div class="settings-layout">
                        <nav class="settings-nav">
                            <div class="settings-search-wrap">
                                <input
                                    class="settings-search-input"
                                    type="text"
                                    placeholder="Search settings..."
                                    prop:value=move || search_query.get()
                                    on:input=on_search
                                />
                            </div>
                            <div class="settings-nav-group">
                                <div class="settings-nav-group-title">"Settings"</div>
                                <div class="settings-nav-group-items">
                                    {ALL_TABS.map(|tab| {
                                        let is_active = move || active_tab.get() == tab;
                                        let matches_search = move || {
                                            tab.matches_query(&search_query.get())
                                        };
                                        view! {
                                            <Show when=matches_search>
                                                <button
                                                    class="settings-nav-item"
                                                    class:active=is_active
                                                    on:click=move |_| active_tab.set(tab)
                                                >
                                                    {tab.label()}
                                                </button>
                                            </Show>
                                        }
                                    }).collect_view()}
                                </div>
                            </div>
                        </nav>

                        <div class="settings-content">
                            {move || match active_tab.get() {
                                SettingsTab::Hosts => view! { <HostsTab /> }.into_any(),
                                SettingsTab::Appearance => view! { <AppearanceTab /> }.into_any(),
                                SettingsTab::General => view! { <GeneralTab /> }.into_any(),
                                SettingsTab::Backends => view! { <BackendsTab /> }.into_any(),
                                SettingsTab::Debug => view! { <DebugTab /> }.into_any(),
                            }}
                        </div>
                    </div>
                </div>
            </div>
        </Show>
    }
}

#[component]
fn HostsTab() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_selected_host = state.clone();
    let state_for_configured_hosts = state.clone();
    let label_sig = RwSignal::new(String::new());
    let ssh_destination_sig = RwSignal::new(String::new());
    let remote_command_sig = RwSignal::new(String::new());
    let auto_connect_sig = RwSignal::new(true);

    let on_add = {
        let state = state.clone();
        move |_| {
            let label = label_sig.get_untracked().trim().to_string();
            let ssh_destination = ssh_destination_sig.get_untracked().trim().to_string();
            let remote_command = remote_command_sig.get_untracked().trim().to_string();
            let auto_connect = auto_connect_sig.get_untracked();
            if label.is_empty() || ssh_destination.is_empty() {
                log::error!("host label and ssh destination are required");
                return;
            }

            let state = state.clone();
            spawn_local(async move {
                let result = bridge::upsert_configured_host(bridge::UpsertConfiguredHostRequest {
                    id: None,
                    label,
                    transport: BridgeHostTransportConfig::SshStdio {
                        ssh_destination,
                        remote_command: if remote_command.is_empty() {
                            None
                        } else {
                            Some(remote_command)
                        },
                    },
                    auto_connect,
                })
                .await;

                match result {
                    Ok(_) => refresh_configured_hosts(&state).await,
                    Err(error) => log::error!("failed to save configured host: {error}"),
                }
            });

            label_sig.set(String::new());
            ssh_destination_sig.set(String::new());
            remote_command_sig.set(String::new());
            auto_connect_sig.set(true);
        }
    };

    view! {
        <h2 class="settings-panel-title">"Hosts"</h2>

        <div class="settings-field">
            <label class="settings-label">"Selected Host"</label>
            <p class="settings-description">"Choose which host the host-scoped settings tabs operate on."</p>
            <select
                class="settings-select settings-select-full"
                prop:value=move || state.selected_host_id.get().unwrap_or_default()
                on:change=move |ev: web_sys::Event| {
                    let target = ev.target().unwrap();
                    let select: web_sys::HtmlSelectElement = target.unchecked_into();
                    let state = state_for_selected_host.clone();
                    spawn_local(async move {
                        match bridge::set_selected_host(bridge::SetSelectedHostRequest {
                            host_id: Some(select.value()),
                        }).await {
                            Ok(_) => refresh_configured_hosts(&state).await,
                            Err(error) => log::error!("failed to set selected host: {error}"),
                        }
                    });
                }
            >
                {move || state.configured_hosts.get().into_iter().map(|host| {
                    view! { <option value=host.id>{host.label}</option> }
                }).collect_view()}
            </select>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Configured Hosts"</label>
            <p class="settings-description">"The embedded local host is always present and connected automatically."</p>
            <div class="settings-host-list">
                {move || state_for_configured_hosts.configured_hosts.get().into_iter().map(|host| {
                    let host_id = host.id.clone();
                    let is_local = host_id == "local";
                    let host_id_for_connect = host_id.clone();
                    let host_id_for_disconnect = host_id.clone();
                    let host_id_for_remove = host_id.clone();
                    let status = state_for_configured_hosts
                        .connection_statuses
                        .get()
                        .get(&host_id)
                        .cloned()
                        .unwrap_or(crate::state::ConnectionStatus::Disconnected);
                    let (status_class, status_text) = match &status {
                        crate::state::ConnectionStatus::Connected => ("connected", "Connected".to_string()),
                        crate::state::ConnectionStatus::Connecting => ("connecting", "Connecting…".to_string()),
                        crate::state::ConnectionStatus::Disconnected => ("disconnected", "Disconnected".to_string()),
                        crate::state::ConnectionStatus::Error(message) => ("error", format!("Error: {message}")),
                    };
                    let is_connected = matches!(status, crate::state::ConnectionStatus::Connected);
                    let is_connecting = matches!(status, crate::state::ConnectionStatus::Connecting);
                    let connect_state = state.clone();
                    let disconnect_state = state.clone();
                    let remove_state = state.clone();
                    let (badge_class, badge_text, transport_text) = match &host.transport {
                        BridgeHostTransportConfig::LocalEmbedded => (
                            "host-badge host-badge-local",
                            "Local",
                            "Embedded local host".to_string(),
                        ),
                        BridgeHostTransportConfig::SshStdio { ssh_destination, .. } => (
                            "host-badge host-badge-ssh",
                            "SSH",
                            format!("ssh {ssh_destination}"),
                        ),
                    };

                    view! {
                        <div class="host-card">
                            <div class="host-card-main">
                                <div class="host-card-title-row">
                                    <span class=badge_class>{badge_text}</span>
                                    <span class="host-card-label">{host.label.clone()}</span>
                                </div>
                                <p class="host-card-transport">{transport_text}</p>
                                <div class="host-card-status">
                                    <span class=format!("status-dot {status_class}")></span>
                                    <span class="status-text">{status_text}</span>
                                </div>
                            </div>
                            <div class="host-card-actions">
                                {(!is_local).then(|| {
                                    let host_id_for_connect = host_id_for_connect.clone();
                                    let host_id_for_disconnect = host_id_for_disconnect.clone();
                                    let connect_state = connect_state.clone();
                                    let disconnect_state = disconnect_state.clone();
                                    if is_connected {
                                        view! {
                                            <button
                                                class="settings-btn"
                                                on:click=move |_| {
                                                    let host_id = host_id_for_disconnect.clone();
                                                    let state = disconnect_state.clone();
                                                    spawn_local(async move {
                                                        if let Err(error) = bridge::disconnect_host(host_id.clone()).await {
                                                            log::error!("failed to disconnect host: {error}");
                                                        }
                                                        state.connection_statuses.update(|statuses| {
                                                            statuses.insert(host_id.clone(), crate::state::ConnectionStatus::Disconnected);
                                                        });
                                                        state.clear_host_runtime(&host_id);
                                                    });
                                                }
                                            >
                                                "Disconnect"
                                            </button>
                                        }.into_any()
                                    } else {
                                        view! {
                                            <button
                                                class="settings-btn settings-btn-primary"
                                                disabled=is_connecting
                                                on:click=move |_| {
                                                    let state = connect_state.clone();
                                                    let host_id = host_id_for_connect.clone();
                                                    spawn_local(async move {
                                                        connect_one_host(state, host_id).await;
                                                    });
                                                }
                                            >
                                                {if is_connecting { "Connecting…" } else { "Connect" }}
                                            </button>
                                        }.into_any()
                                    }
                                })}
                                {(!is_local).then(|| {
                                    let host_id = host_id_for_remove.clone();
                                    view! {
                                        <button
                                            class="settings-btn settings-btn-danger"
                                            on:click=move |_| {
                                                let state = remove_state.clone();
                                                let host_id = host_id.clone();
                                                spawn_local(async move {
                                                    match bridge::remove_configured_host(host_id).await {
                                                        Ok(_) => refresh_configured_hosts(&state).await,
                                                        Err(error) => log::error!("failed to remove host: {error}"),
                                                    }
                                                });
                                            }
                                        >
                                            "Remove"
                                        </button>
                                    }
                                })}
                            </div>
                        </div>
                    }
                }).collect_view()}
            </div>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Add Remote Host"</label>
            <p class="settings-description">"Configure a remote host that Tyde can connect to over SSH."</p>
            <div class="settings-form">
                <div class="settings-form-row">
                    <label class="settings-form-label">
                        <span>"Label"</span>
                        <input
                            class="settings-text-input"
                            type="text"
                            placeholder="e.g. Workstation"
                            prop:value=move || label_sig.get()
                            on:input=move |ev| label_sig.set(event_target_value(&ev))
                        />
                    </label>
                    <label class="settings-form-label">
                        <span>"SSH destination"</span>
                        <input
                            class="settings-text-input"
                            type="text"
                            placeholder="user@host"
                            prop:value=move || ssh_destination_sig.get()
                            on:input=move |ev| ssh_destination_sig.set(event_target_value(&ev))
                        />
                    </label>
                </div>
                <label class="settings-form-label">
                    <span>"Remote command"<span class="settings-form-hint">" (optional)"</span></span>
                    <input
                        class="settings-text-input"
                        type="text"
                        placeholder="tyde host --stdio"
                        prop:value=move || remote_command_sig.get()
                        on:input=move |ev| remote_command_sig.set(event_target_value(&ev))
                    />
                </label>
                <div class="settings-form-footer">
                    <div class="settings-checkbox-row">
                        <label class="settings-toggle">
                            <input
                                type="checkbox"
                                prop:checked=move || auto_connect_sig.get()
                                on:change=move |ev: web_sys::Event| {
                                    let target = ev.target().unwrap();
                                    let input: web_sys::HtmlInputElement = target.unchecked_into();
                                    auto_connect_sig.set(input.checked());
                                }
                            />
                            <span class="settings-toggle-slider"></span>
                        </label>
                        <span>"Auto-connect on launch"</span>
                    </div>
                    <button class="settings-btn settings-btn-primary" on:click=on_add>"Add Host"</button>
                </div>
            </div>
        </div>
    }
}

#[component]
fn AppearanceTab() -> impl IntoView {
    let state = expect_context::<AppState>();

    let set_theme = move |theme: &'static str| {
        move |_| {
            state.theme.set(theme.to_owned());
            apply_theme(theme);
        }
    };

    let theme_class = move |target: &'static str| {
        move || {
            if state.theme.get() == target {
                "segment active"
            } else {
                "segment"
            }
        }
    };

    let on_font_size = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlInputElement = target.unchecked_into();
        if let Ok(v) = el.value().parse::<u32>() {
            state.font_size.set(v);
            apply_font_size(v);
        }
    };

    let on_font_family = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlSelectElement = target.unchecked_into();
        let key = el.value();
        state.font_family.set(key.clone());
        apply_font_family(&key);
    };

    view! {
        <h2 class="settings-panel-title">"Appearance"</h2>

        <div class="settings-field">
            <label class="settings-label">"Color Theme"</label>
            <p class="settings-description">"Choose the color scheme for the interface."</p>
            <div class="settings-segmented-control">
                <button class=theme_class("dark") on:click=set_theme("dark")>"Dark"</button>
                <button class=theme_class("light") on:click=set_theme("light")>"Light"</button>
            </div>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Font Size"</label>
            <p class="settings-description">"Adjust the base font size used throughout the interface."</p>
            <div class="settings-inline-control">
                <input
                    type="range"
                    class="settings-slider"
                    min="11"
                    max="20"
                    prop:value=move || state.font_size.get().to_string()
                    on:input=on_font_size
                />
                <span class="settings-slider-value">{move || format!("{}px", state.font_size.get())}</span>
            </div>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Font Family"</label>
            <p class="settings-description">"Select the font family for UI text."</p>
            <select
                class="settings-select"
                prop:value=move || state.font_family.get()
                on:change=on_font_family
            >
                {FONT_FAMILIES.iter().map(|(key, label, _)| {
                    view! { <option value=*key>{*label}</option> }
                }).collect::<Vec<_>>()}
            </select>
        </div>
    }
}

#[component]
fn GeneralTab() -> impl IntoView {
    view! {
        <h2 class="settings-panel-title">"General"</h2>

        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Auto-connect on Launch"</label>
                    <p class="settings-description">"Automatically connect to the host server when the application starts."</p>
                </div>
                <label class="settings-toggle">
                    <input type="checkbox" checked=true disabled=true />
                    <span class="settings-toggle-slider"></span>
                </label>
            </div>
        </div>
    }
}

#[component]
fn BackendsTab() -> impl IntoView {
    let state = expect_context::<AppState>();

    view! {
        <h2 class="settings-panel-title">"Backends"</h2>

        <div class="settings-field">
            <label class="settings-label">"Default Backend"</label>
            <p class="settings-description">"The backend to use by default when creating new agents."</p>
            {move || match state.selected_host_settings() {
                Some(settings) => {
                    let state_for_change = state.clone();
                    let has_enabled = !settings.enabled_backends.is_empty();
                    let default_backend_value = settings
                        .default_backend
                        .map(backend_value)
                        .unwrap_or("")
                        .to_owned();
                    let options = settings
                        .enabled_backends
                        .into_iter()
                        .map(|backend| {
                            view! {
                                <option value=backend_value(backend)>{backend_label(backend)}</option>
                            }
                        })
                        .collect::<Vec<_>>();

                    view! {
                        <select
                            class="settings-select"
                            prop:value=default_backend_value
                            disabled=!has_enabled
                            on:change=move |ev: web_sys::Event| {
                                let target = ev.target().unwrap();
                                let el: web_sys::HtmlSelectElement = target.unchecked_into();
                                let default_backend = if el.value().is_empty() {
                                    None
                                } else {
                                    let Some(kind) = parse_backend_kind(&el.value()) else {
                                        log::error!("unknown backend value {}", el.value());
                                        return;
                                    };
                                    Some(kind)
                                };
                                send_host_setting(
                                    &state_for_change,
                                    HostSettingValue::DefaultBackend { default_backend },
                                );
                            }
                        >
                            <option value="">"No default backend"</option>
                            {options}
                        </select>
                    }
                    .into_any()
                }
                None => view! { <p class="settings-description">"Host settings not loaded for the selected host."</p> }.into_any(),
            }}
        </div>

        <div class="settings-field">
            <label class="settings-label">"Enabled Backends"</label>
            <p class="settings-description">"Toggle which backends are available for creating agents."</p>
            <div class="settings-backend-list">
                {all_backends()
                    .into_iter()
                    .map(|kind| view! { <BackendCard kind /> })
                    .collect::<Vec<_>>()}
            </div>
        </div>
    }
}

#[component]
fn DebugTab() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_checked = state.clone();
    let state_for_disabled = state.clone();

    let checked = move || {
        state_for_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.tyde_debug_mcp_enabled)
    };
    let disabled = move || state_for_disabled.selected_host_settings().is_none();

    let on_toggle = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            send_host_setting(
                &state,
                HostSettingValue::TydeDebugMcpEnabled {
                    enabled: input.checked(),
                },
            );
        }
    };

    view! {
        <h2 class="settings-panel-title">"Debug"</h2>

        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Tyde Debug MCP"</label>
                    <p class="settings-description">
                        "When enabled, newly created chats are started with the Tyde debug MCP server so agents can inspect and drive the frontend through JavaScript."
                    </p>
                </div>
                <label class="settings-toggle">
                    <input
                        type="checkbox"
                        prop:checked=checked
                        disabled=disabled
                        on:change=on_toggle
                    />
                    <span class="settings-toggle-slider"></span>
                </label>
            </div>
        </div>
    }
}

#[component]
fn BackendCard(kind: BackendKind) -> impl IntoView {
    let state = expect_context::<AppState>();
    let name = backend_label(kind);
    let description = backend_description(kind);
    let badge_class = backend_badge_class(kind);
    let state_for_checked = state.clone();
    let state_for_disable = state.clone();

    let checked = move || {
        state_for_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.enabled_backends.contains(&kind))
    };
    let disable_toggle = move || state_for_disable.selected_host_settings().is_none();

    let on_toggle = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            let Some(settings) = state.selected_host_settings_untracked() else {
                log::error!("backend toggle requested before host settings loaded");
                return;
            };

            let enabled_backends = all_backends()
                .into_iter()
                .filter(|candidate| {
                    if *candidate == kind {
                        input.checked()
                    } else {
                        settings.enabled_backends.contains(candidate)
                    }
                })
                .collect::<Vec<_>>();

            send_host_setting(
                &state,
                HostSettingValue::EnabledBackends { enabled_backends },
            );
        }
    };

    view! {
        <div class="settings-backend-card">
            <div class="settings-backend-header">
                <span class=badge_class>{name}</span>
                <label class="settings-toggle">
                    <input
                        type="checkbox"
                        prop:checked=checked
                        disabled=disable_toggle
                        on:change=on_toggle
                    />
                    <span class="settings-toggle-slider"></span>
                </label>
            </div>
            <p class="settings-backend-desc">{description}</p>
        </div>
    }
}

fn request_host_settings(state: &AppState) {
    let Some((host_id, host_stream)) = state.selected_host_stream_untracked() else {
        log::error!("request_host_settings: no selected connected host");
        return;
    };

    spawn_local(async move {
        if let Err(e) = send_frame(
            &host_id,
            host_stream,
            FrameKind::DumpSettings,
            &DumpSettingsPayload {},
        )
        .await
        {
            log::error!("failed to send DumpSettings: {e}");
        }
    });
}

fn send_host_setting(state: &AppState, setting: HostSettingValue) {
    let Some((host_id, host_stream)) = state.selected_host_stream_untracked() else {
        log::error!("send_host_setting: no selected connected host");
        return;
    };

    spawn_local(async move {
        if let Err(e) = send_frame(
            &host_id,
            host_stream,
            FrameKind::SetSetting,
            &SetSettingPayload { setting },
        )
        .await
        {
            log::error!("failed to send SetSetting: {e}");
        }
    });
}

fn all_backends() -> [BackendKind; 5] {
    [
        BackendKind::Tycode,
        BackendKind::Kiro,
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Gemini,
    ]
}

fn parse_backend_kind(value: &str) -> Option<BackendKind> {
    match value {
        "tycode" => Some(BackendKind::Tycode),
        "kiro" => Some(BackendKind::Kiro),
        "claude" => Some(BackendKind::Claude),
        "codex" => Some(BackendKind::Codex),
        "gemini" => Some(BackendKind::Gemini),
        _ => None,
    }
}

fn backend_value(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "tycode",
        BackendKind::Kiro => "kiro",
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Gemini => "gemini",
    }
}

fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Gemini => "Gemini",
    }
}

fn backend_description(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode subprocess backend",
        BackendKind::Kiro => "Kiro ACP backend",
        BackendKind::Claude => "Anthropic Claude — advanced reasoning and coding",
        BackendKind::Codex => "OpenAI Codex — code completion and generation",
        BackendKind::Gemini => "Google Gemini — multimodal AI assistant",
    }
}

fn backend_badge_class(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "backend-badge tycode",
        BackendKind::Kiro => "backend-badge kiro",
        BackendKind::Claude => "backend-badge claude",
        BackendKind::Codex => "backend-badge codex",
        BackendKind::Gemini => "backend-badge gemini",
    }
}
