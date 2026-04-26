use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::app::{connect_one_host, refresh_configured_hosts};
use crate::bridge::{self, HostTransportConfig as BridgeHostTransportConfig};
use crate::send::send_frame;
use crate::state::{AppState, DiffViewMode, ToolOutputMode};

use protocol::{
    BackendKind, BackendSetupAction, BackendSetupInfo, BackendSetupStatus, CustomAgent,
    CustomAgentId, DiffContextMode, FrameKind, HostSettingValue, McpServerConfig, McpServerId,
    McpTransportConfig, ProjectId, RunBackendSetupPayload, SetSettingPayload, Skill, SkillId,
    Steering, SteeringId, SteeringScope, ToolPolicy,
};

use std::collections::{HashMap, HashSet};

use crate::send::{
    custom_agent_delete, custom_agent_upsert, mcp_server_delete, mcp_server_upsert, skill_refresh,
    steering_delete, steering_upsert,
};

const RESERVED_MCP_NAMES: &[&str] = &["tyde-debug", "tyde-agent-control"];

const STORAGE_THEME: &str = "tyde-theme";
const STORAGE_FONT_SIZE: &str = "tyde-font-size";
const STORAGE_FONT_FAMILY: &str = "tyde-font-family";
const STORAGE_TABS_ENABLED: &str = "tyde-tabs-enabled";
const STORAGE_DIFF_VIEW_MODE: &str = "tyde-diff-view-mode";
const STORAGE_DIFF_CONTEXT_MODE: &str = "tyde-diff-context-mode";
const STORAGE_TOOL_OUTPUT_MODE: &str = "tyde-tool-output-mode";

const DIFF_VIEW_MODE_UNIFIED: &str = "unified";
const DIFF_VIEW_MODE_SIDE_BY_SIDE: &str = "side_by_side";
const DIFF_CONTEXT_MODE_HUNKS: &str = "hunks";
const DIFF_CONTEXT_MODE_FULL_FILE: &str = "full_file";
const TOOL_OUTPUT_MODE_SUMMARY: &str = "summary";
const TOOL_OUTPUT_MODE_COMPACT: &str = "compact";
const TOOL_OUTPUT_MODE_FULL: &str = "full";

fn diff_view_mode_to_str(mode: DiffViewMode) -> &'static str {
    match mode {
        DiffViewMode::Unified => DIFF_VIEW_MODE_UNIFIED,
        DiffViewMode::SideBySide => DIFF_VIEW_MODE_SIDE_BY_SIDE,
    }
}

fn diff_view_mode_from_str(s: &str) -> Option<DiffViewMode> {
    match s {
        DIFF_VIEW_MODE_UNIFIED => Some(DiffViewMode::Unified),
        DIFF_VIEW_MODE_SIDE_BY_SIDE => Some(DiffViewMode::SideBySide),
        _ => None,
    }
}

fn diff_context_mode_to_str(mode: DiffContextMode) -> &'static str {
    match mode {
        DiffContextMode::Hunks => DIFF_CONTEXT_MODE_HUNKS,
        DiffContextMode::FullFile => DIFF_CONTEXT_MODE_FULL_FILE,
    }
}

fn diff_context_mode_from_str(s: &str) -> Option<DiffContextMode> {
    match s {
        DIFF_CONTEXT_MODE_HUNKS => Some(DiffContextMode::Hunks),
        DIFF_CONTEXT_MODE_FULL_FILE => Some(DiffContextMode::FullFile),
        _ => None,
    }
}

pub fn persist_diff_view_mode(mode: DiffViewMode) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(STORAGE_DIFF_VIEW_MODE, diff_view_mode_to_str(mode));
    }
}

pub fn persist_diff_context_mode(mode: DiffContextMode) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(STORAGE_DIFF_CONTEXT_MODE, diff_context_mode_to_str(mode));
    }
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

#[cfg(test)]
mod diff_pref_tests {
    use super::*;

    #[test]
    fn diff_view_mode_roundtrip() {
        for mode in [DiffViewMode::Unified, DiffViewMode::SideBySide] {
            let s = diff_view_mode_to_str(mode);
            assert_eq!(diff_view_mode_from_str(s), Some(mode));
        }
    }

    #[test]
    fn diff_context_mode_roundtrip() {
        for mode in [DiffContextMode::Hunks, DiffContextMode::FullFile] {
            let s = diff_context_mode_to_str(mode);
            assert_eq!(diff_context_mode_from_str(s), Some(mode));
        }
    }

    #[test]
    fn diff_view_mode_unknown_is_none() {
        assert_eq!(diff_view_mode_from_str(""), None);
        assert_eq!(diff_view_mode_from_str("bogus"), None);
    }

    #[test]
    fn diff_context_mode_unknown_is_none() {
        assert_eq!(diff_context_mode_from_str(""), None);
        assert_eq!(diff_context_mode_from_str("bogus"), None);
    }

    #[test]
    fn tool_output_mode_roundtrip() {
        for mode in [
            ToolOutputMode::Summary,
            ToolOutputMode::Compact,
            ToolOutputMode::Full,
        ] {
            let s = tool_output_mode_to_str(mode);
            assert_eq!(tool_output_mode_from_str(s), Some(mode));
        }
    }

    #[test]
    fn tool_output_mode_unknown_is_none() {
        assert_eq!(tool_output_mode_from_str(""), None);
        assert_eq!(tool_output_mode_from_str("bogus"), None);
    }
}

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

    if let Ok(Some(tabs_str)) = storage.get_item(STORAGE_TABS_ENABLED) {
        let enabled = tabs_str != "false";
        state.tabs_enabled.set(enabled);
    }

    match storage.get_item(STORAGE_DIFF_VIEW_MODE) {
        Ok(Some(raw)) => match diff_view_mode_from_str(&raw) {
            Some(mode) => state.diff_view_mode.set(mode),
            None => {
                log::warn!(
                    "unrecognized diff_view_mode in localStorage: {raw:?}; resetting to default"
                );
                let default = state.diff_view_mode.get_untracked();
                persist_diff_view_mode(default);
            }
        },
        Ok(None) => persist_diff_view_mode(state.diff_view_mode.get_untracked()),
        Err(e) => log::warn!("failed to read diff_view_mode from localStorage: {e:?}"),
    }

    match storage.get_item(STORAGE_DIFF_CONTEXT_MODE) {
        Ok(Some(raw)) => match diff_context_mode_from_str(&raw) {
            Some(mode) => state.diff_context_mode.set(mode),
            None => {
                log::warn!(
                    "unrecognized diff_context_mode in localStorage: {raw:?}; resetting to default"
                );
                let default = state.diff_context_mode.get_untracked();
                persist_diff_context_mode(default);
            }
        },
        Ok(None) => persist_diff_context_mode(state.diff_context_mode.get_untracked()),
        Err(e) => log::warn!("failed to read diff_context_mode from localStorage: {e:?}"),
    }

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

#[derive(Clone, Copy, Debug, PartialEq)]
enum SettingsTab {
    Hosts,
    Appearance,
    General,
    Backends,
    CustomAgents,
    McpServers,
    Steering,
    Skills,
    Debug,
}

impl SettingsTab {
    fn label(self) -> &'static str {
        match self {
            Self::Hosts => "Hosts",
            Self::Appearance => "Appearance",
            Self::General => "General",
            Self::Backends => "Backends",
            Self::CustomAgents => "Custom Agents",
            Self::McpServers => "MCP Servers",
            Self::Steering => "Steering",
            Self::Skills => "Skills",
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
                "Tab Bar",
                "Show a tab bar for managing multiple open views",
                "Diff Layout",
                "Unified",
                "Side by Side",
                "Diff Context",
                "Hunks",
                "Full File",
                "Tool Output",
                "Summary",
                "Compact",
                "Full",
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
            Self::CustomAgents => &[
                "Custom Agents",
                "Name",
                "Description",
                "Instructions",
                "Tool Policy",
                "Skills",
                "MCP Servers",
                "Unrestricted",
                "Allow list",
                "Deny list",
                "New custom agent",
            ],
            Self::McpServers => &[
                "MCP Servers",
                "Transport",
                "Http",
                "Stdio",
                "URL",
                "Bearer token env var",
                "Command",
                "Args",
                "Environment",
                "Headers",
                "New MCP server",
            ],
            Self::Steering => &[
                "Steering",
                "Scope",
                "Host",
                "Project",
                "Title",
                "Content",
                "New steering",
            ],
            Self::Skills => &["Skills", "Refresh", "SKILL.md", "Filesystem skills"],
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

const ALL_TABS: [SettingsTab; 9] = [
    SettingsTab::Hosts,
    SettingsTab::Appearance,
    SettingsTab::General,
    SettingsTab::Backends,
    SettingsTab::CustomAgents,
    SettingsTab::McpServers,
    SettingsTab::Steering,
    SettingsTab::Skills,
    SettingsTab::Debug,
];

#[component]
pub fn SettingsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let active_tab = RwSignal::new(SettingsTab::Appearance);
    let search_query = RwSignal::new(String::new());

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
                                    spellcheck="false"
                                    {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                                    autocapitalize="none"
                                    autocomplete="off"
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
                            <div class="settings-nav-footer">
                                <button class="settings-feedback-link" on:click=move |_| {
                                    state.settings_open.set(false);
                                    state.feedback_open.set(true);
                                }>"Send Feedback"</button>
                            </div>
                        </nav>

                        <div class="settings-content">
                            {move || match active_tab.get() {
                                SettingsTab::Hosts => view! { <HostsTab /> }.into_any(),
                                SettingsTab::Appearance => view! { <AppearanceTab /> }.into_any(),
                                SettingsTab::General => view! { <GeneralTab /> }.into_any(),
                                SettingsTab::Backends => view! { <BackendsTab /> }.into_any(),
                                SettingsTab::CustomAgents => view! { <CustomAgentsTab /> }.into_any(),
                                SettingsTab::McpServers => view! { <McpServersTab /> }.into_any(),
                                SettingsTab::Steering => view! { <SteeringTab /> }.into_any(),
                                SettingsTab::Skills => view! { <SkillsTab /> }.into_any(),
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
    let error_sig: RwSignal<Option<String>> = RwSignal::new(None);

    let on_add = {
        let state = state.clone();
        move |_| {
            let label = label_sig.get_untracked().trim().to_string();
            let ssh_destination = ssh_destination_sig.get_untracked().trim().to_string();
            let remote_command = remote_command_sig.get_untracked().trim().to_string();
            let auto_connect = auto_connect_sig.get_untracked();
            if label.is_empty() || ssh_destination.is_empty() {
                error_sig.set(Some("Label and SSH destination are required.".to_string()));
                return;
            }
            let remote_command = if remote_command.is_empty() {
                None
            } else {
                Some(remote_command)
            };
            let lifecycle = if remote_command.is_none() {
                bridge::RemoteHostLifecycleConfig::ManagedTyde {
                    release: bridge::TydeReleaseTarget::Latest,
                }
            } else {
                bridge::RemoteHostLifecycleConfig::Manual
            };
            let should_prepare_managed = matches!(
                &lifecycle,
                bridge::RemoteHostLifecycleConfig::ManagedTyde { .. }
            );

            let state = state.clone();
            spawn_local(async move {
                let result = bridge::upsert_configured_host(bridge::UpsertConfiguredHostRequest {
                    id: None,
                    label,
                    transport: BridgeHostTransportConfig::SshStdio {
                        ssh_destination,
                        remote_command,
                        lifecycle,
                    },
                    auto_connect,
                })
                .await;

                match result {
                    Ok(store) => {
                        error_sig.set(None);
                        label_sig.set(String::new());
                        ssh_destination_sig.set(String::new());
                        remote_command_sig.set(String::new());
                        auto_connect_sig.set(true);
                        let new_host_id = if auto_connect {
                            let existing_ids: std::collections::HashSet<String> = state
                                .configured_hosts
                                .get_untracked()
                                .into_iter()
                                .map(|h| h.id)
                                .collect();
                            store
                                .hosts
                                .iter()
                                .find(|h| !existing_ids.contains(&h.id))
                                .map(|h| h.id.clone())
                        } else {
                            None
                        };
                        refresh_configured_hosts(&state).await;
                        if let Some(host_id) = new_host_id {
                            if should_prepare_managed {
                                match bridge::ensure_configured_host_ready(host_id.clone()).await {
                                    Ok(snapshot) => {
                                        state.host_lifecycle_statuses.update(|statuses| {
                                            statuses.insert(
                                                host_id.clone(),
                                                bridge::RemoteHostLifecycleStatus::Snapshot {
                                                    snapshot,
                                                },
                                            );
                                        });
                                        connect_one_host(state.clone(), host_id).await;
                                    }
                                    Err(error) => {
                                        error_sig.set(Some(format!(
                                            "Failed to prepare remote host: {error}"
                                        )));
                                        state.host_lifecycle_statuses.update(|statuses| {
                                            statuses.insert(
                                                host_id,
                                                bridge::RemoteHostLifecycleStatus::Error {
                                                    message: error,
                                                },
                                            );
                                        });
                                    }
                                }
                            } else {
                                connect_one_host(state.clone(), host_id).await;
                            }
                        }
                    }
                    Err(e) => error_sig.set(Some(format!("Failed to add host: {e}"))),
                }
            });
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
                            Err(e) => error_sig.set(Some(format!("Failed to set selected host: {e}"))),
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
            <p class="settings-description">"The embedded local host is always present. Managed SSH hosts install Tyde Server under ~/.tyde/bin/<version>/tyde-server and launch ~/.tyde/bin/current/tyde-server when needed."</p>
            <div class="settings-host-list">
                {move || state_for_configured_hosts.configured_hosts.get().into_iter().map(|host| {
                    let host_id = host.id.clone();
                    let is_local = matches!(host.transport, BridgeHostTransportConfig::LocalEmbedded);
                    let is_managed_remote = is_managed_remote_host(&host.transport);
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
                    let lifecycle_status = state_for_configured_hosts
                        .host_lifecycle_statuses
                        .get()
                        .get(&host_id)
                        .cloned()
                        .unwrap_or(bridge::RemoteHostLifecycleStatus::Idle);
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
                                {is_managed_remote.then(|| {
                                    let lifecycle_text = lifecycle_status_text(&lifecycle_status);
                                    view! {
                                        <p class="host-card-transport">{format!("Tyde server: {lifecycle_text}")}</p>
                                    }
                                })}
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
                                                        if let Err(e) = bridge::disconnect_host(host_id.clone()).await {
                                                            error_sig.set(Some(format!("Failed to disconnect host: {e}")));
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
                                    } else if is_managed_remote {
                                        let lifecycle_status = lifecycle_status.clone();
                                        let label = managed_lifecycle_button_label(&lifecycle_status);
                                        let disabled = is_connecting || managed_lifecycle_button_disabled(&lifecycle_status);
                                        view! {
                                            <button
                                                class="settings-btn settings-btn-primary"
                                                disabled=disabled
                                                on:click=move |_| {
                                                    let state = connect_state.clone();
                                                    let host_id = host_id_for_connect.clone();
                                                    spawn_local(async move {
                                                        match bridge::ensure_configured_host_ready(host_id.clone()).await {
                                                            Ok(snapshot) => {
                                                                state.host_lifecycle_statuses.update(|statuses| {
                                                                    statuses.insert(
                                                                        host_id.clone(),
                                                                        bridge::RemoteHostLifecycleStatus::Snapshot { snapshot },
                                                                    );
                                                                });
                                                                connect_one_host(state, host_id).await;
                                                            }
                                                            Err(error) => {
                                                                error_sig.set(Some(format!("Failed to prepare remote host: {error}")));
                                                                state.host_lifecycle_statuses.update(|statuses| {
                                                                    statuses.insert(
                                                                        host_id,
                                                                        bridge::RemoteHostLifecycleStatus::Error { message: error },
                                                                    );
                                                                });
                                                            }
                                                        }
                                                    });
                                                }
                                            >
                                                {label}
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
                                                        Err(e) => error_sig.set(Some(format!("Failed to remove host: {e}"))),
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
            <p class="settings-description">"Configure a remote host over SSH. Leave Remote command blank for managed install/launch from GitHub releases. Set Remote command only for a manual bridge command."</p>
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
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
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
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                    </label>
                </div>
                <label class="settings-form-label">
                    <span>"Remote command"<span class="settings-form-hint">" (optional)"</span></span>
                    <input
                        class="settings-text-input"
                        type="text"
                        placeholder="tyde host --bridge-uds"
                        prop:value=move || remote_command_sig.get()
                        on:input=move |ev| remote_command_sig.set(event_target_value(&ev))
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
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
                <Show when=move || error_sig.get().is_some()>
                    <p class="settings-error">{move || error_sig.get().unwrap_or_default()}</p>
                </Show>
            </div>
        </div>
    }
}

fn is_managed_remote_host(transport: &BridgeHostTransportConfig) -> bool {
    matches!(
        transport,
        BridgeHostTransportConfig::SshStdio {
            lifecycle: bridge::RemoteHostLifecycleConfig::ManagedTyde { .. },
            ..
        }
    )
}

fn lifecycle_status_text(status: &bridge::RemoteHostLifecycleStatus) -> String {
    match status {
        bridge::RemoteHostLifecycleStatus::Idle => "not checked".to_string(),
        bridge::RemoteHostLifecycleStatus::Running {
            step,
            target_version,
        } => match target_version {
            Some(version) => format!("{} v{}", lifecycle_step_label(*step), version),
            None => lifecycle_step_label(*step).to_string(),
        },
        bridge::RemoteHostLifecycleStatus::Snapshot { snapshot } => {
            let target = snapshot.target_version;
            match &snapshot.running {
                bridge::RemoteTydeRunningState::Managed { version } if *version == target => {
                    format!("v{version} running")
                }
                bridge::RemoteTydeRunningState::Managed { version } => {
                    format!("v{version} running; v{target} available")
                }
                bridge::RemoteTydeRunningState::UnknownSocket => {
                    "running, unmanaged socket".to_string()
                }
                bridge::RemoteTydeRunningState::NotRunning if snapshot.installed_target => {
                    format!("v{target} installed, not running")
                }
                bridge::RemoteTydeRunningState::NotRunning => {
                    format!("v{target} not installed")
                }
            }
        }
        bridge::RemoteHostLifecycleStatus::Error { message } => format!("error: {message}"),
    }
}

fn lifecycle_step_label(step: bridge::RemoteHostLifecycleStep) -> &'static str {
    match step {
        bridge::RemoteHostLifecycleStep::ProbePlatform => "Checking platform",
        bridge::RemoteHostLifecycleStep::ResolveRelease => "Resolving release",
        bridge::RemoteHostLifecycleStep::ProbeInstallation => "Checking install",
        bridge::RemoteHostLifecycleStep::DownloadAsset => "Downloading",
        bridge::RemoteHostLifecycleStep::InstallBinary => "Installing",
        bridge::RemoteHostLifecycleStep::StopOldServer => "Stopping old server",
        bridge::RemoteHostLifecycleStep::LaunchServer => "Launching",
        bridge::RemoteHostLifecycleStep::VerifyRunning => "Verifying",
        bridge::RemoteHostLifecycleStep::Connect => "Ready",
    }
}

fn managed_lifecycle_button_label(status: &bridge::RemoteHostLifecycleStatus) -> String {
    match status {
        bridge::RemoteHostLifecycleStatus::Running { step, .. } => {
            lifecycle_step_label(*step).to_string()
        }
        bridge::RemoteHostLifecycleStatus::Snapshot { snapshot } => match &snapshot.running {
            bridge::RemoteTydeRunningState::Managed { version }
                if *version == snapshot.target_version =>
            {
                "Connect".to_string()
            }
            bridge::RemoteTydeRunningState::Managed { .. } => "Upgrade & Relaunch".to_string(),
            bridge::RemoteTydeRunningState::UnknownSocket => "Unmanaged Server".to_string(),
            bridge::RemoteTydeRunningState::NotRunning if snapshot.installed_target => {
                "Launch".to_string()
            }
            bridge::RemoteTydeRunningState::NotRunning => "Install & Launch".to_string(),
        },
        bridge::RemoteHostLifecycleStatus::Error { .. }
        | bridge::RemoteHostLifecycleStatus::Idle => "Install & Launch".to_string(),
    }
}

fn managed_lifecycle_button_disabled(status: &bridge::RemoteHostLifecycleStatus) -> bool {
    matches!(
        status,
        bridge::RemoteHostLifecycleStatus::Running { .. }
            | bridge::RemoteHostLifecycleStatus::Snapshot {
                snapshot: bridge::RemoteHostLifecycleSnapshot {
                    running: bridge::RemoteTydeRunningState::UnknownSocket,
                    ..
                }
            }
    )
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

        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Tab Bar"</label>
                    <p class="settings-description">"Show a tab bar for managing multiple open views. When disabled, the center zone shows one view at a time."</p>
                </div>
                <label class="settings-toggle">
                    <input
                        type="checkbox"
                        prop:checked=move || state.tabs_enabled.get()
                        on:change=move |ev: web_sys::Event| {
                            let target = ev.target().unwrap();
                            let input: web_sys::HtmlInputElement = target.unchecked_into();
                            let enabled = input.checked();
                            state.tabs_enabled.set(enabled);
                            if let Some(storage) = local_storage() {
                                let _ = storage.set_item(STORAGE_TABS_ENABLED, if enabled { "true" } else { "false" });
                            }
                        }
                    />
                    <span class="settings-toggle-slider"></span>
                </label>
            </div>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Diff Layout"</label>
            <p class="settings-description">"Choose how git diffs are displayed: a single column (Unified) or side by side."</p>
            <div class="settings-segmented-control">
                <button
                    class=move || if state.diff_view_mode.get() == DiffViewMode::Unified { "segment active" } else { "segment" }
                    on:click=move |_| {
                        state.diff_view_mode.set(DiffViewMode::Unified);
                        persist_diff_view_mode(DiffViewMode::Unified);
                    }
                >"Unified"</button>
                <button
                    class=move || if state.diff_view_mode.get() == DiffViewMode::SideBySide { "segment active" } else { "segment" }
                    on:click=move |_| {
                        state.diff_view_mode.set(DiffViewMode::SideBySide);
                        persist_diff_view_mode(DiffViewMode::SideBySide);
                    }
                >"Side by Side"</button>
            </div>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Diff Context"</label>
            <p class="settings-description">"Show only changed hunks with surrounding context, or the full file."</p>
            <div class="settings-segmented-control">
                <button
                    class=move || if state.diff_context_mode.get() == DiffContextMode::Hunks { "segment active" } else { "segment" }
                    on:click=move |_| {
                        state.diff_context_mode.set(DiffContextMode::Hunks);
                        persist_diff_context_mode(DiffContextMode::Hunks);
                    }
                >"Hunks"</button>
                <button
                    class=move || if state.diff_context_mode.get() == DiffContextMode::FullFile { "segment active" } else { "segment" }
                    on:click=move |_| {
                        state.diff_context_mode.set(DiffContextMode::FullFile);
                        persist_diff_context_mode(DiffContextMode::FullFile);
                    }
                >"Full File"</button>
            </div>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Tool Output"</label>
            <p class="settings-description">"Choose how much of each tool call is shown in chat: a header-only summary, a compact preview with caps, or the full output."</p>
            <div class="settings-segmented-control">
                <button
                    class=move || if state.tool_output_mode.get() == ToolOutputMode::Summary { "segment active" } else { "segment" }
                    on:click=move |_| {
                        state.tool_output_mode.set(ToolOutputMode::Summary);
                        persist_tool_output_mode(ToolOutputMode::Summary);
                    }
                >"Summary"</button>
                <button
                    class=move || if state.tool_output_mode.get() == ToolOutputMode::Compact { "segment active" } else { "segment" }
                    on:click=move |_| {
                        state.tool_output_mode.set(ToolOutputMode::Compact);
                        persist_tool_output_mode(ToolOutputMode::Compact);
                    }
                >"Compact"</button>
                <button
                    class=move || if state.tool_output_mode.get() == ToolOutputMode::Full { "segment active" } else { "segment" }
                    on:click=move |_| {
                        state.tool_output_mode.set(ToolOutputMode::Full);
                        persist_tool_output_mode(ToolOutputMode::Full);
                    }
                >"Full"</button>
            </div>
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
        <div class="settings-panel-header">
            <h2 class="settings-panel-title">"Backends"</h2>
        </div>

        <p class="settings-description settings-panel-intro">
            "Toggle backends, install them on the selected host, and run sign-in when available. Install and sign-in commands run in the host terminal so output stays visible."
        </p>

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
                                        log::error!("unknown backend value {} in select", el.value());
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
            <p class="settings-description">"Toggle which backends are available for creating agents, then use the setup commands below to install them on the selected host."</p>
            <div class="settings-backend-list settings-backend-list-rich">
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
    let state_for_debug_checked = state.clone();
    let state_for_debug_disabled = state.clone();
    let state_for_ac_checked = state.clone();
    let state_for_ac_disabled = state.clone();

    let debug_checked = move || {
        state_for_debug_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.tyde_debug_mcp_enabled)
    };
    let debug_disabled = move || state_for_debug_disabled.selected_host_settings().is_none();

    let debug_on_toggle = {
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

    let ac_checked = move || {
        state_for_ac_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.tyde_agent_control_mcp_enabled)
    };
    let ac_disabled = move || state_for_ac_disabled.selected_host_settings().is_none();

    let ac_on_toggle = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            send_host_setting(
                &state,
                HostSettingValue::TydeAgentControlMcpEnabled {
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
                        prop:checked=debug_checked
                        disabled=debug_disabled
                        on:change=debug_on_toggle
                    />
                    <span class="settings-toggle-slider"></span>
                </label>
            </div>
        </div>

        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Agent Control MCP"</label>
                    <p class="settings-description">
                        "When enabled, newly created chats are started with the agent control MCP server so agents can spawn, message, and orchestrate other agents."
                    </p>
                </div>
                <label class="settings-toggle">
                    <input
                        type="checkbox"
                        prop:checked=ac_checked
                        disabled=ac_disabled
                        on:change=ac_on_toggle
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
    let state_for_setup = state.clone();

    let checked = move || {
        state_for_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.enabled_backends.contains(&kind))
    };
    let disable_toggle = move || state_for_disable.selected_host_settings().is_none();
    let setup_info = move || {
        state_for_setup
            .selected_host_backend_setup()
            .and_then(|infos| infos.into_iter().find(|info| info.backend_kind == kind))
    };
    let setup_info_for_status = setup_info.clone();
    let setup_info_for_label = setup_info.clone();
    let setup_info_for_version = setup_info.clone();
    let setup_info_for_details = setup_info.clone();

    let on_toggle = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            let Some(settings) = state.selected_host_settings_untracked() else {
                log::error!("backend toggle fired before host settings loaded");
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
        <div class="settings-backend-card settings-backend-card-rich">
            <div class="settings-backend-header settings-backend-header-rich">
                <div class="settings-backend-title-wrap">
                    <span class=badge_class>{name}</span>
                    <span class=move || backend_setup_status_class(setup_info_for_status().as_ref())>
                        {move || backend_setup_status_label(setup_info_for_label().as_ref())}
                    </span>
                </div>
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

            {move || setup_info_for_version().and_then(|info| info.installed_version).map(|version| {
                view! { <p class="settings-backend-version">{version}</p> }
            })}

            {move || match setup_info_for_details() {
                Some(info) => {
                    let state_for_install = state.clone();
                    let state_for_signin = state.clone();
                    let docs_url = info.docs_url.clone();
                    let install_runnable = info
                        .install_command
                        .as_ref()
                        .map(|command| command.runnable)
                        .unwrap_or(false);
                    let signin_runnable = info
                        .sign_in_command
                        .as_ref()
                        .map(|command| command.runnable)
                        .unwrap_or(false);
                    let show_install = info.status == BackendSetupStatus::NotInstalled
                        && info.install_command.is_some();
                    let show_signin = info.status == BackendSetupStatus::Installed
                        && info.sign_in_command.is_some();
                    let unsupported = info.status == BackendSetupStatus::Unsupported;
                    view! {
                        <div class="settings-backend-setup">
                            <div class="settings-backend-actions">
                                {show_install.then(|| view! {
                                    <button
                                        class="settings-btn settings-btn-primary"
                                        disabled=!install_runnable
                                        on:click=move |_| {
                                            send_run_backend_setup(
                                                &state_for_install,
                                                kind,
                                                BackendSetupAction::Install,
                                            );
                                        }
                                    >
                                        "Install"
                                    </button>
                                })}
                                {show_signin.then(|| view! {
                                    <button
                                        class="settings-btn"
                                        disabled=!signin_runnable
                                        on:click=move |_| {
                                            send_run_backend_setup(
                                                &state_for_signin,
                                                kind,
                                                BackendSetupAction::SignIn,
                                            );
                                        }
                                    >
                                        "Sign in"
                                    </button>
                                })}
                                <a class="settings-doc-link" href=docs_url target="_blank" rel="noreferrer">"Docs"</a>
                            </div>
                            {unsupported.then(|| {
                                view! {
                                    <p class="settings-backend-note">
                                        "Automatic setup is not available for this host platform. Use the docs link for manual setup steps."
                                    </p>
                                }
                            })}
                        </div>
                    }
                    .into_any()
                }
                None => view! {
                    <div class="settings-backend-setup">
                        <p class="settings-backend-note">"Checking install status for this host…"</p>
                    </div>
                }
                .into_any(),
            }}
        </div>
    }
}

fn send_run_backend_setup(state: &AppState, backend_kind: BackendKind, action: BackendSetupAction) {
    let Some((host_id, host_stream)) = state.selected_host_stream_untracked() else {
        log::error!("send_run_backend_setup called without a selected host stream");
        return;
    };

    state.bottom_dock.set(crate::state::DockVisibility::Visible);
    state.pending_terminal_focus.set(Some(host_id.clone()));

    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            host_stream,
            FrameKind::RunBackendSetup,
            &RunBackendSetupPayload {
                backend_kind,
                action,
            },
        )
        .await
        {
            log::error!("failed to send RunBackendSetup: {error}");
        }
    });
}

fn backend_setup_status_label(info: Option<&BackendSetupInfo>) -> &'static str {
    match info.map(|info| info.status) {
        Some(BackendSetupStatus::Installed) => "Installed",
        Some(BackendSetupStatus::NotInstalled) => "Not installed",
        Some(BackendSetupStatus::Unsupported) => "Unsupported",
        None => "Checking…",
    }
}

fn backend_setup_status_class(info: Option<&BackendSetupInfo>) -> &'static str {
    match info.map(|info| info.status) {
        Some(BackendSetupStatus::Installed) => "settings-status-chip installed",
        Some(BackendSetupStatus::NotInstalled) => "settings-status-chip missing",
        Some(BackendSetupStatus::Unsupported) => "settings-status-chip unsupported",
        None => "settings-status-chip loading",
    }
}

fn send_host_setting(state: &AppState, setting: HostSettingValue) {
    let Some((host_id, host_stream)) = state.selected_host_stream_untracked() else {
        log::error!("send_host_setting called without a selected host stream");
        return;
    };

    spawn_local(async move {
        if let Err(error) = send_frame(
            &host_id,
            host_stream,
            FrameKind::SetSetting,
            &SetSettingPayload { setting },
        )
        .await
        {
            log::error!("failed to send SetSetting: {error}");
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

fn generate_id() -> String {
    format!(
        "id-{:x}-{:x}",
        js_sys::Date::now() as u64,
        (js_sys::Math::random() * (u64::MAX as f64)) as u64
    )
}

fn parse_kv_lines(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.is_empty() {
                out.insert(k.to_string(), v.trim().to_string());
            }
        }
    }
    out
}

fn format_kv_lines(map: &HashMap<String, String>) -> String {
    let mut pairs: Vec<_> = map.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_args_lines(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn host_stream_with_id(state: &AppState, host_id: &str) -> Option<(String, protocol::StreamPath)> {
    let stream = state.host_stream_untracked(host_id)?;
    Some((host_id.to_string(), stream))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolPolicyKind {
    Unrestricted,
    AllowList,
    DenyList,
}

impl ToolPolicyKind {
    fn from_policy(policy: &ToolPolicy) -> Self {
        match policy {
            ToolPolicy::Unrestricted => Self::Unrestricted,
            ToolPolicy::AllowList { .. } => Self::AllowList,
            ToolPolicy::DenyList { .. } => Self::DenyList,
        }
    }
}

fn tool_policy_tools(policy: &ToolPolicy) -> String {
    match policy {
        ToolPolicy::Unrestricted => String::new(),
        ToolPolicy::AllowList { tools } | ToolPolicy::DenyList { tools } => tools.join(", "),
    }
}

fn parse_tool_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ── Custom Agents ───────────────────────────────────────────────────────

#[derive(Clone)]
struct CustomAgentForm {
    id: CustomAgentId,
    is_new: bool,
    name: RwSignal<String>,
    description: RwSignal<String>,
    instructions: RwSignal<String>,
    skill_ids: RwSignal<Vec<SkillId>>,
    mcp_server_ids: RwSignal<Vec<McpServerId>>,
    tool_policy_kind: RwSignal<ToolPolicyKind>,
    tool_policy_tools: RwSignal<String>,
}

impl CustomAgentForm {
    fn from_agent(agent: &CustomAgent) -> Self {
        Self {
            id: agent.id.clone(),
            is_new: false,
            name: RwSignal::new(agent.name.clone()),
            description: RwSignal::new(agent.description.clone()),
            instructions: RwSignal::new(agent.instructions.clone().unwrap_or_default()),
            skill_ids: RwSignal::new(agent.skill_ids.clone()),
            mcp_server_ids: RwSignal::new(agent.mcp_server_ids.clone()),
            tool_policy_kind: RwSignal::new(ToolPolicyKind::from_policy(&agent.tool_policy)),
            tool_policy_tools: RwSignal::new(tool_policy_tools(&agent.tool_policy)),
        }
    }

    fn blank() -> Self {
        Self {
            id: CustomAgentId(generate_id()),
            is_new: true,
            name: RwSignal::new(String::new()),
            description: RwSignal::new(String::new()),
            instructions: RwSignal::new(String::new()),
            skill_ids: RwSignal::new(Vec::new()),
            mcp_server_ids: RwSignal::new(Vec::new()),
            tool_policy_kind: RwSignal::new(ToolPolicyKind::Unrestricted),
            tool_policy_tools: RwSignal::new(String::new()),
        }
    }

    fn validate_and_build(&self) -> Result<CustomAgent, String> {
        let name = self.name.get_untracked().trim().to_string();
        if name.is_empty() {
            return Err("Name is required.".to_string());
        }

        let description = self.description.get_untracked().trim().to_string();
        if description.is_empty() {
            return Err("Description is required.".to_string());
        }

        let tool_policy = match self.tool_policy_kind.get_untracked() {
            ToolPolicyKind::Unrestricted => ToolPolicy::Unrestricted,
            ToolPolicyKind::AllowList => {
                let tools = parse_tool_list(&self.tool_policy_tools.get_untracked());
                validate_tool_policy_tools(&tools)?;
                ToolPolicy::AllowList { tools }
            }
            ToolPolicyKind::DenyList => {
                let tools = parse_tool_list(&self.tool_policy_tools.get_untracked());
                validate_tool_policy_tools(&tools)?;
                ToolPolicy::DenyList { tools }
            }
        };

        let instructions = self.instructions.get_untracked().trim().to_string();
        Ok(CustomAgent {
            id: self.id.clone(),
            name,
            description,
            instructions: if instructions.is_empty() {
                None
            } else {
                Some(instructions)
            },
            skill_ids: self.skill_ids.get_untracked(),
            mcp_server_ids: self.mcp_server_ids.get_untracked(),
            tool_policy,
        })
    }
}

fn validate_tool_policy_tools(tools: &[String]) -> Result<(), String> {
    if tools.is_empty() {
        return Err("Tool policy must include at least one tool.".to_string());
    }

    let mut seen = HashSet::new();
    for tool in tools {
        let trimmed = tool.trim();
        if trimmed.is_empty() {
            return Err("Tool policy must not include blank tool names.".to_string());
        }
        if !seen.insert(trimmed.to_string()) {
            return Err(format!("Tool policy contains duplicate tool '{trimmed}'."));
        }
    }

    Ok(())
}

#[component]
fn SettingsConfirmDialog(
    title: String,
    body: String,
    confirm_label: String,
    on_cancel: Callback<()>,
    on_confirm: Callback<()>,
) -> impl IntoView {
    let cancel_on_backdrop = on_cancel;
    let on_backdrop = move |_| cancel_on_backdrop.run(());

    let cancel_on_keydown = on_cancel;
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Escape" {
            cancel_on_keydown.run(());
        }
    };

    let cancel_on_click = on_cancel;
    let on_cancel_click = move |_| cancel_on_click.run(());

    let confirm_on_click = on_confirm;
    let on_confirm_click = move |_| confirm_on_click.run(());

    view! {
        <div class="settings-confirm-overlay" on:click=on_backdrop on:keydown=on_keydown tabindex="0">
            <div class="settings-confirm-modal" on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
                <h3 class="settings-confirm-title">{title}</h3>
                <p class="settings-confirm-description">{body}</p>
                <div class="settings-form-footer">
                    <button class="settings-btn" on:click=on_cancel_click>"Cancel"</button>
                    <button class="settings-btn settings-btn-danger" on:click=on_confirm_click>
                        {confirm_label}
                    </button>
                </div>
            </div>
        </div>
    }
}

type PendingCustomAgentDelete = (CustomAgentId, String);
type PendingMcpDelete = (McpServerId, String);
type PendingSteeringDelete = (SteeringId, String);

#[component]
fn CustomAgentsTab() -> impl IntoView {
    let state = expect_context::<AppState>();
    let form: RwSignal<Option<CustomAgentForm>> = RwSignal::new(None);
    let pending_delete: RwSignal<Option<PendingCustomAgentDelete>> = RwSignal::new(None);

    let state_for_rows = state.clone();
    let rows = Memo::new(move |_| {
        let Some(host_id) = state_for_rows.selected_host_id.get() else {
            return Vec::new();
        };
        let mut agents: Vec<CustomAgent> = state_for_rows
            .custom_agents
            .get()
            .get(&host_id)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents
    });

    let state_for_new_disabled = state.clone();
    let pending_delete_for_cancel = pending_delete;
    let on_cancel_delete = Callback::new(move |_| pending_delete_for_cancel.set(None));

    let pending_delete_for_confirm = pending_delete;
    let state_for_confirm_delete = state.clone();
    let on_confirm_delete = Callback::new(move |_| {
        let Some((id, _)) = pending_delete_for_confirm.get_untracked() else {
            return;
        };
        pending_delete_for_confirm.set(None);
        let Some(host_id) = state_for_confirm_delete.selected_host_id.get_untracked() else {
            return;
        };
        let Some((host_id, host_stream)) = host_stream_with_id(&state_for_confirm_delete, &host_id)
        else {
            return;
        };
        spawn_local(async move {
            if let Err(error) = custom_agent_delete(&host_id, host_stream, id).await {
                log::error!("failed to send custom_agent_delete: {error}");
            }
        });
    });

    view! {
        <div class="settings-panel-header">
            <h2 class="settings-panel-title">"Custom Agents"</h2>
        </div>
        <p class="settings-description settings-panel-intro">
            "Define reusable agent presets: instructions, skills, MCP servers, and tool policy. Changes are saved on the selected host."
        </p>

        <div class="settings-field">
            <div class="settings-form-footer">
                <button
                    class="settings-btn settings-btn-primary"
                    disabled=move || state_for_new_disabled.selected_host_id.get().is_none()
                    on:click=move |_| form.set(Some(CustomAgentForm::blank()))
                >
                    "+ New custom agent"
                </button>
            </div>
        </div>

        {move || form.get().map(|f| view! { <CustomAgentEditor form=f editor_signal=form /> })}

        <div class="settings-field">
            <div class="settings-host-list">
                {move || {
                    let list = rows.get();
                    if list.is_empty() {
                        view! { <div class="panel-empty">"No custom agents on this host."</div> }.into_any()
                    } else {
                        view! {
                            <>
                            {list.into_iter().map(|agent| view! {
                                <CustomAgentRow agent=agent editor_signal=form delete_signal=pending_delete />
                            }).collect_view()}
                            </>
                        }.into_any()
                    }
                }}
            </div>
        </div>

        {move || {
            pending_delete.get().map(|(_, name)| {
                let on_cancel = on_cancel_delete;
                let on_confirm = on_confirm_delete;
                let body = format!("Delete custom agent \"{name}\"? This cannot be undone.");
                view! {
                    <SettingsConfirmDialog
                        title="Delete custom agent".to_string()
                        body=body
                        confirm_label="Delete".to_string()
                        on_cancel=on_cancel
                        on_confirm=on_confirm
                    />
                }
            })
        }}
    }
}

#[component]
fn CustomAgentRow(
    agent: CustomAgent,
    editor_signal: RwSignal<Option<CustomAgentForm>>,
    delete_signal: RwSignal<Option<PendingCustomAgentDelete>>,
) -> impl IntoView {
    let agent_for_edit = agent.clone();
    let on_edit = move |_| {
        editor_signal.set(Some(CustomAgentForm::from_agent(&agent_for_edit)));
    };

    let agent_id_for_delete = agent.id.clone();
    let name_for_delete = agent.name.clone();
    let on_delete =
        move |_| delete_signal.set(Some((agent_id_for_delete.clone(), name_for_delete.clone())));

    let description = if agent.description.is_empty() {
        "No description".to_string()
    } else {
        agent.description.clone()
    };

    view! {
        <div class="host-card">
            <div class="host-card-main">
                <div class="host-card-title-row">
                    <span class="host-card-label">{agent.name.clone()}</span>
                </div>
                <p class="host-card-transport">{description}</p>
            </div>
            <div class="host-card-actions">
                <button class="settings-btn" on:click=on_edit>"Edit"</button>
                <button class="settings-btn settings-btn-danger" on:click=on_delete>"Delete"</button>
            </div>
        </div>
    }
}

#[component]
fn CustomAgentEditor(
    form: CustomAgentForm,
    editor_signal: RwSignal<Option<CustomAgentForm>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let title = if form.is_new {
        "New Custom Agent"
    } else {
        "Edit Custom Agent"
    };

    let name_sig = form.name;
    let description_sig = form.description;
    let instructions_sig = form.instructions;
    let skill_ids_sig = form.skill_ids;
    let mcp_server_ids_sig = form.mcp_server_ids;
    let tool_policy_kind_sig = form.tool_policy_kind;
    let tool_policy_tools_sig = form.tool_policy_tools;

    let state_for_skills = state.clone();
    let available_skills = Memo::new(move |_| {
        let Some(host_id) = state_for_skills.selected_host_id.get() else {
            return Vec::new();
        };
        let mut skills: Vec<Skill> = state_for_skills
            .skills
            .get()
            .get(&host_id)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    });

    let state_for_mcp = state.clone();
    let available_mcp = Memo::new(move |_| {
        let Some(host_id) = state_for_mcp.selected_host_id.get() else {
            return Vec::new();
        };
        let mut servers: Vec<McpServerConfig> = state_for_mcp
            .mcp_servers
            .get()
            .get(&host_id)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        servers
    });

    let error_sig: RwSignal<Option<String>> = RwSignal::new(None);

    let form_for_save = form.clone();
    let state_for_save = state.clone();
    let error_sig_for_save = error_sig;
    let editor_signal_for_save = editor_signal;
    let on_save = move |_| {
        let custom_agent = match form_for_save.validate_and_build() {
            Ok(custom_agent) => custom_agent,
            Err(error) => {
                error_sig_for_save.set(Some(error));
                return;
            }
        };
        let Some(host_id) = state_for_save.selected_host_id.get_untracked() else {
            error_sig_for_save.set(Some("No host selected.".to_string()));
            return;
        };
        let Some((host_id, host_stream)) = host_stream_with_id(&state_for_save, &host_id) else {
            error_sig_for_save.set(Some("Host is not connected.".to_string()));
            return;
        };
        error_sig_for_save.set(None);
        spawn_local(async move {
            match custom_agent_upsert(&host_id, host_stream, custom_agent).await {
                Ok(()) => editor_signal_for_save.set(None),
                Err(error) => {
                    error_sig_for_save.set(Some(format!("Failed to save custom agent: {error}")))
                }
            }
        });
    };

    let on_cancel = move |_| editor_signal.set(None);

    let kind_radio = move |target: ToolPolicyKind, label: &'static str| {
        view! {
            <label class="settings-toggle-row" style="gap:0.5rem;">
                <input
                    type="radio"
                    name="tool_policy_kind"
                    prop:checked=move || tool_policy_kind_sig.get() == target
                    on:change=move |_| tool_policy_kind_sig.set(target)
                />
                <span>{label}</span>
            </label>
        }
    };

    view! {
        <div class="settings-field">
            <label class="settings-label">{title}</label>
            <div class="settings-form">
                <label class="settings-form-label">
                    <span>"Name"</span>
                    <input
                        class="settings-text-input"
                        type="text"
                        prop:value=move || name_sig.get()
                        on:input=move |ev| name_sig.set(event_target_value(&ev))
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
                    />
                </label>

                <label class="settings-form-label">
                    <span>"Description"</span>
                    <input
                        class="settings-text-input"
                        type="text"
                        prop:value=move || description_sig.get()
                        on:input=move |ev| description_sig.set(event_target_value(&ev))
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
                    />
                </label>

                <label class="settings-form-label">
                    <span>"Instructions"<span class="settings-form-hint">" (optional)"</span></span>
                    <textarea
                        class="settings-text-input"
                        rows="5"
                        prop:value=move || instructions_sig.get()
                        on:input=move |ev| instructions_sig.set(event_target_value(&ev))
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
                    />
                </label>

                <label class="settings-form-label">
                    <span>"Skills"</span>
                    <div class="settings-backend-list">
                        {move || {
                            let list = available_skills.get();
                            if list.is_empty() {
                                view! { <div class="settings-description">"No skills on this host."</div> }.into_any()
                            } else {
                                view! {
                                    <>
                                    {list.into_iter().map(|skill| {
                                        let id = skill.id.clone();
                                        let label = skill.name.clone();
                                        let id_for_check = id.clone();
                                        let id_for_toggle = id.clone();
                                        view! {
                                            <div class="settings-checkbox-row">
                                                <input
                                                    type="checkbox"
                                                    prop:checked=move || skill_ids_sig.get().contains(&id_for_check)
                                                    on:change=move |ev: web_sys::Event| {
                                                        let target = ev.target().unwrap();
                                                        let input: web_sys::HtmlInputElement = target.unchecked_into();
                                                        let id = id_for_toggle.clone();
                                                        if input.checked() {
                                                            skill_ids_sig.update(|v| {
                                                                if !v.contains(&id) { v.push(id); }
                                                            });
                                                        } else {
                                                            skill_ids_sig.update(|v| v.retain(|s| s != &id));
                                                        }
                                                    }
                                                />
                                                <span>{label}</span>
                                            </div>
                                        }
                                    }).collect_view()}
                                    </>
                                }.into_any()
                            }
                        }}
                    </div>
                </label>

                <label class="settings-form-label">
                    <span>"MCP Servers"</span>
                    <div class="settings-backend-list">
                        {move || {
                            let list = available_mcp.get();
                            if list.is_empty() {
                                view! { <div class="settings-description">"No MCP servers on this host."</div> }.into_any()
                            } else {
                                view! {
                                    <>
                                    {list.into_iter().map(|server| {
                                        let id = server.id.clone();
                                        let label = server.name.clone();
                                        let id_for_check = id.clone();
                                        let id_for_toggle = id.clone();
                                        view! {
                                            <div class="settings-checkbox-row">
                                                <input
                                                    type="checkbox"
                                                    prop:checked=move || mcp_server_ids_sig.get().contains(&id_for_check)
                                                    on:change=move |ev: web_sys::Event| {
                                                        let target = ev.target().unwrap();
                                                        let input: web_sys::HtmlInputElement = target.unchecked_into();
                                                        let id = id_for_toggle.clone();
                                                        if input.checked() {
                                                            mcp_server_ids_sig.update(|v| {
                                                                if !v.contains(&id) { v.push(id); }
                                                            });
                                                        } else {
                                                            mcp_server_ids_sig.update(|v| v.retain(|s| s != &id));
                                                        }
                                                    }
                                                />
                                                <span>{label}</span>
                                            </div>
                                        }
                                    }).collect_view()}
                                    </>
                                }.into_any()
                            }
                        }}
                    </div>
                </label>

                <label class="settings-form-label">
                    <span>"Tool Policy"</span>
                    <div class="settings-form-row">
                        {kind_radio(ToolPolicyKind::Unrestricted, "Unrestricted")}
                        {kind_radio(ToolPolicyKind::AllowList, "Allow list")}
                        {kind_radio(ToolPolicyKind::DenyList, "Deny list")}
                    </div>
                </label>

                <Show when=move || tool_policy_kind_sig.get() != ToolPolicyKind::Unrestricted>
                    <label class="settings-form-label">
                        <span>"Tools"<span class="settings-form-hint">" (comma-separated names)"</span></span>
                        <input
                            class="settings-text-input"
                            type="text"
                            placeholder="bash, read, edit"
                            prop:value=move || tool_policy_tools_sig.get()
                            on:input=move |ev| tool_policy_tools_sig.set(event_target_value(&ev))
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                    </label>
                </Show>

                <Show when=move || error_sig.get().is_some()>
                    <p class="settings-error">{move || error_sig.get().unwrap_or_default()}</p>
                </Show>

                <div class="settings-form-footer">
                    <button class="settings-btn" on:click=on_cancel>"Cancel"</button>
                    <button class="settings-btn settings-btn-primary" on:click=on_save>"Save"</button>
                </div>
            </div>
        </div>
    }
}

// ── MCP Servers ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum McpTransportKind {
    Http,
    Stdio,
}

#[derive(Clone)]
struct McpForm {
    id: McpServerId,
    is_new: bool,
    name: RwSignal<String>,
    transport_kind: RwSignal<McpTransportKind>,
    url: RwSignal<String>,
    headers: RwSignal<String>,
    bearer_token_env_var: RwSignal<String>,
    command: RwSignal<String>,
    args: RwSignal<String>,
    env: RwSignal<String>,
    error: RwSignal<Option<String>>,
}

impl McpForm {
    fn from_server(server: &McpServerConfig) -> Self {
        let (kind, url, headers, bearer, command, args, env) = match &server.transport {
            McpTransportConfig::Http {
                url,
                headers,
                bearer_token_env_var,
            } => (
                McpTransportKind::Http,
                url.clone(),
                format_kv_lines(headers),
                bearer_token_env_var.clone().unwrap_or_default(),
                String::new(),
                String::new(),
                String::new(),
            ),
            McpTransportConfig::Stdio { command, args, env } => (
                McpTransportKind::Stdio,
                String::new(),
                String::new(),
                String::new(),
                command.clone(),
                args.join("\n"),
                format_kv_lines(env),
            ),
        };
        Self {
            id: server.id.clone(),
            is_new: false,
            name: RwSignal::new(server.name.clone()),
            transport_kind: RwSignal::new(kind),
            url: RwSignal::new(url),
            headers: RwSignal::new(headers),
            bearer_token_env_var: RwSignal::new(bearer),
            command: RwSignal::new(command),
            args: RwSignal::new(args),
            env: RwSignal::new(env),
            error: RwSignal::new(None),
        }
    }

    fn blank() -> Self {
        Self {
            id: McpServerId(generate_id()),
            is_new: true,
            name: RwSignal::new(String::new()),
            transport_kind: RwSignal::new(McpTransportKind::Http),
            url: RwSignal::new(String::new()),
            headers: RwSignal::new(String::new()),
            bearer_token_env_var: RwSignal::new(String::new()),
            command: RwSignal::new(String::new()),
            args: RwSignal::new(String::new()),
            env: RwSignal::new(String::new()),
            error: RwSignal::new(None),
        }
    }

    fn validate_and_build(&self) -> Result<McpServerConfig, String> {
        let name = self.name.get_untracked().trim().to_string();
        if name.is_empty() {
            return Err("Name is required".to_string());
        }
        if RESERVED_MCP_NAMES.contains(&name.as_str()) {
            return Err(format!(
                "\"{name}\" is reserved for built-in MCP servers. Choose another name."
            ));
        }
        let transport = match self.transport_kind.get_untracked() {
            McpTransportKind::Http => {
                let url = self.url.get_untracked().trim().to_string();
                if url.is_empty() {
                    return Err("URL is required for HTTP transport".to_string());
                }
                let bearer = self.bearer_token_env_var.get_untracked().trim().to_string();
                McpTransportConfig::Http {
                    url,
                    headers: parse_kv_lines(&self.headers.get_untracked()),
                    bearer_token_env_var: if bearer.is_empty() {
                        None
                    } else {
                        Some(bearer)
                    },
                }
            }
            McpTransportKind::Stdio => {
                let command = self.command.get_untracked().trim().to_string();
                if command.is_empty() {
                    return Err("Command is required for Stdio transport".to_string());
                }
                McpTransportConfig::Stdio {
                    command,
                    args: parse_args_lines(&self.args.get_untracked()),
                    env: parse_kv_lines(&self.env.get_untracked()),
                }
            }
        };
        Ok(McpServerConfig {
            id: self.id.clone(),
            name,
            transport,
        })
    }
}

#[component]
fn McpServersTab() -> impl IntoView {
    let state = expect_context::<AppState>();
    let form: RwSignal<Option<McpForm>> = RwSignal::new(None);
    let pending_delete: RwSignal<Option<PendingMcpDelete>> = RwSignal::new(None);

    let state_for_rows = state.clone();
    let rows = Memo::new(move |_| {
        let Some(host_id) = state_for_rows.selected_host_id.get() else {
            return Vec::new();
        };
        let mut servers: Vec<McpServerConfig> = state_for_rows
            .mcp_servers
            .get()
            .get(&host_id)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        servers
    });

    let state_for_new_disabled = state.clone();
    let pending_delete_for_cancel = pending_delete;
    let on_cancel_delete = Callback::new(move |_| pending_delete_for_cancel.set(None));

    let pending_delete_for_confirm = pending_delete;
    let state_for_confirm_delete = state.clone();
    let on_confirm_delete = Callback::new(move |_| {
        let Some((id, _)) = pending_delete_for_confirm.get_untracked() else {
            return;
        };
        pending_delete_for_confirm.set(None);
        let Some(host_id) = state_for_confirm_delete.selected_host_id.get_untracked() else {
            return;
        };
        let Some((host_id, host_stream)) = host_stream_with_id(&state_for_confirm_delete, &host_id)
        else {
            return;
        };
        spawn_local(async move {
            if let Err(error) = mcp_server_delete(&host_id, host_stream, id).await {
                log::error!("failed to send mcp_server_delete: {error}");
            }
        });
    });

    view! {
        <div class="settings-panel-header">
            <h2 class="settings-panel-title">"MCP Servers"</h2>
        </div>
        <p class="settings-description settings-panel-intro">
            "Configure MCP servers (HTTP or Stdio). Names \"tyde-debug\" and \"tyde-agent-control\" are reserved."
        </p>

        <div class="settings-field">
            <div class="settings-form-footer">
                <button
                    class="settings-btn settings-btn-primary"
                    disabled=move || state_for_new_disabled.selected_host_id.get().is_none()
                    on:click=move |_| form.set(Some(McpForm::blank()))
                >
                    "+ New MCP server"
                </button>
            </div>
        </div>

        {move || form.get().map(|f| view! { <McpEditor form=f editor_signal=form /> })}

        <div class="settings-field">
            <div class="settings-host-list">
                {move || {
                    let list = rows.get();
                    if list.is_empty() {
                        view! { <div class="panel-empty">"No MCP servers on this host."</div> }.into_any()
                    } else {
                        view! {
                            <>
                            {list.into_iter().map(|server| view! {
                                <McpRow server=server editor_signal=form delete_signal=pending_delete />
                            }).collect_view()}
                            </>
                        }.into_any()
                    }
                }}
            </div>
        </div>

        {move || {
            pending_delete.get().map(|(_, name)| {
                let on_cancel = on_cancel_delete;
                let on_confirm = on_confirm_delete;
                let body = format!("Delete MCP server \"{name}\"? This cannot be undone.");
                view! {
                    <SettingsConfirmDialog
                        title="Delete MCP server".to_string()
                        body=body
                        confirm_label="Delete".to_string()
                        on_cancel=on_cancel
                        on_confirm=on_confirm
                    />
                }
            })
        }}
    }
}

#[component]
fn McpRow(
    server: McpServerConfig,
    editor_signal: RwSignal<Option<McpForm>>,
    delete_signal: RwSignal<Option<PendingMcpDelete>>,
) -> impl IntoView {
    let transport_label = match &server.transport {
        McpTransportConfig::Http { url, .. } => format!("HTTP · {url}"),
        McpTransportConfig::Stdio { command, .. } => format!("Stdio · {command}"),
    };

    let server_for_edit = server.clone();
    let on_edit = move |_| editor_signal.set(Some(McpForm::from_server(&server_for_edit)));

    let id_for_delete = server.id.clone();
    let name_for_delete = server.name.clone();
    let on_delete =
        move |_| delete_signal.set(Some((id_for_delete.clone(), name_for_delete.clone())));

    view! {
        <div class="host-card">
            <div class="host-card-main">
                <div class="host-card-title-row">
                    <span class="host-card-label">{server.name.clone()}</span>
                </div>
                <p class="host-card-transport">{transport_label}</p>
            </div>
            <div class="host-card-actions">
                <button class="settings-btn" on:click=on_edit>"Edit"</button>
                <button class="settings-btn settings-btn-danger" on:click=on_delete>"Delete"</button>
            </div>
        </div>
    }
}

#[component]
fn McpEditor(form: McpForm, editor_signal: RwSignal<Option<McpForm>>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let title = if form.is_new {
        "New MCP Server"
    } else {
        "Edit MCP Server"
    };

    let name_sig = form.name;
    let transport_kind_sig = form.transport_kind;
    let url_sig = form.url;
    let headers_sig = form.headers;
    let bearer_sig = form.bearer_token_env_var;
    let command_sig = form.command;
    let args_sig = form.args;
    let env_sig = form.env;
    let error_sig = form.error;

    let form_for_save = form.clone();
    let state_for_save = state.clone();
    let error_sig_for_save = error_sig;
    let editor_signal_for_save = editor_signal;
    let on_save = move |_| {
        let server = match form_for_save.validate_and_build() {
            Ok(server) => server,
            Err(err) => {
                error_sig_for_save.set(Some(err));
                return;
            }
        };
        let Some(host_id) = state_for_save.selected_host_id.get_untracked() else {
            error_sig_for_save.set(Some("No host selected".to_string()));
            return;
        };
        let Some((host_id, host_stream)) = host_stream_with_id(&state_for_save, &host_id) else {
            error_sig_for_save.set(Some("Host stream missing".to_string()));
            return;
        };
        error_sig_for_save.set(None);
        spawn_local(async move {
            match mcp_server_upsert(&host_id, host_stream, server).await {
                Ok(()) => editor_signal_for_save.set(None),
                Err(error) => {
                    error_sig_for_save.set(Some(format!("Failed to save MCP server: {error}")))
                }
            }
        });
    };

    let on_cancel = move |_| editor_signal.set(None);

    let transport_radio = move |target: McpTransportKind, label: &'static str| {
        view! {
            <label class="settings-toggle-row" style="gap:0.5rem;">
                <input
                    type="radio"
                    name="mcp_transport_kind"
                    prop:checked=move || transport_kind_sig.get() == target
                    on:change=move |_| transport_kind_sig.set(target)
                />
                <span>{label}</span>
            </label>
        }
    };

    view! {
        <div class="settings-field">
            <label class="settings-label">{title}</label>
            <div class="settings-form">
                <Show when=move || error_sig.get().is_some()>
                    <div class="chat-input-error">{move || error_sig.get().unwrap_or_default()}</div>
                </Show>

                <label class="settings-form-label">
                    <span>"Name"</span>
                    <input
                        class="settings-text-input"
                        type="text"
                        prop:value=move || name_sig.get()
                        on:input=move |ev| name_sig.set(event_target_value(&ev))
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
                    />
                </label>

                <label class="settings-form-label">
                    <span>"Transport"</span>
                    <div class="settings-form-row">
                        {transport_radio(McpTransportKind::Http, "HTTP")}
                        {transport_radio(McpTransportKind::Stdio, "Stdio")}
                    </div>
                </label>

                <Show when=move || transport_kind_sig.get() == McpTransportKind::Http>
                    <label class="settings-form-label">
                        <span>"URL"</span>
                        <input
                            class="settings-text-input"
                            type="text"
                            placeholder="https://example.com/mcp"
                            prop:value=move || url_sig.get()
                            on:input=move |ev| url_sig.set(event_target_value(&ev))
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                    </label>
                    <label class="settings-form-label">
                        <span>"Headers"<span class="settings-form-hint">" (key=value per line)"</span></span>
                        <textarea
                            class="settings-text-input"
                            rows="3"
                            prop:value=move || headers_sig.get()
                            on:input=move |ev| headers_sig.set(event_target_value(&ev))
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                    </label>
                    <label class="settings-form-label">
                        <span>"Bearer token env var"<span class="settings-form-hint">" (optional)"</span></span>
                        <input
                            class="settings-text-input"
                            type="text"
                            placeholder="MY_TOKEN"
                            prop:value=move || bearer_sig.get()
                            on:input=move |ev| bearer_sig.set(event_target_value(&ev))
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                    </label>
                </Show>

                <Show when=move || transport_kind_sig.get() == McpTransportKind::Stdio>
                    <label class="settings-form-label">
                        <span>"Command"</span>
                        <input
                            class="settings-text-input"
                            type="text"
                            placeholder="/path/to/mcp-server"
                            prop:value=move || command_sig.get()
                            on:input=move |ev| command_sig.set(event_target_value(&ev))
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                    </label>
                    <label class="settings-form-label">
                        <span>"Arguments"<span class="settings-form-hint">" (one per line)"</span></span>
                        <textarea
                            class="settings-text-input"
                            rows="3"
                            prop:value=move || args_sig.get()
                            on:input=move |ev| args_sig.set(event_target_value(&ev))
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                    </label>
                    <label class="settings-form-label">
                        <span>"Environment"<span class="settings-form-hint">" (key=value per line)"</span></span>
                        <textarea
                            class="settings-text-input"
                            rows="3"
                            prop:value=move || env_sig.get()
                            on:input=move |ev| env_sig.set(event_target_value(&ev))
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                    </label>
                </Show>

                <div class="settings-form-footer">
                    <button class="settings-btn" on:click=on_cancel>"Cancel"</button>
                    <button class="settings-btn settings-btn-primary" on:click=on_save>"Save"</button>
                </div>
            </div>
        </div>
    }
}

// ── Steering ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct SteeringForm {
    id: SteeringId,
    is_new: bool,
    scope_kind: RwSignal<String>, // "host" or project id string
    title: RwSignal<String>,
    content: RwSignal<String>,
}

impl SteeringForm {
    fn from_steering(item: &Steering) -> Self {
        let scope_kind = match &item.scope {
            SteeringScope::Host => "host".to_string(),
            SteeringScope::Project(id) => id.0.clone(),
        };
        Self {
            id: item.id.clone(),
            is_new: false,
            scope_kind: RwSignal::new(scope_kind),
            title: RwSignal::new(item.title.clone()),
            content: RwSignal::new(item.content.clone()),
        }
    }

    fn blank() -> Self {
        Self {
            id: SteeringId(generate_id()),
            is_new: true,
            scope_kind: RwSignal::new("host".to_string()),
            title: RwSignal::new(String::new()),
            content: RwSignal::new(String::new()),
        }
    }

    fn to_steering(&self) -> Steering {
        let raw_scope = self.scope_kind.get_untracked();
        let scope = if raw_scope == "host" {
            SteeringScope::Host
        } else {
            SteeringScope::Project(ProjectId(raw_scope))
        };
        Steering {
            id: self.id.clone(),
            scope,
            title: self.title.get_untracked().trim().to_string(),
            content: self.content.get_untracked(),
        }
    }
}

#[component]
fn SteeringTab() -> impl IntoView {
    let state = expect_context::<AppState>();
    let form: RwSignal<Option<SteeringForm>> = RwSignal::new(None);
    let pending_delete: RwSignal<Option<PendingSteeringDelete>> = RwSignal::new(None);

    let state_for_rows = state.clone();
    let rows = Memo::new(move |_| {
        let Some(host_id) = state_for_rows.selected_host_id.get() else {
            return Vec::new();
        };
        let mut items: Vec<Steering> = state_for_rows
            .steering
            .get()
            .get(&host_id)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        items.sort_by(|a, b| a.title.cmp(&b.title));
        items
    });

    let state_for_new_disabled = state.clone();
    let pending_delete_for_cancel = pending_delete;
    let on_cancel_delete = Callback::new(move |_| pending_delete_for_cancel.set(None));

    let pending_delete_for_confirm = pending_delete;
    let state_for_confirm_delete = state.clone();
    let on_confirm_delete = Callback::new(move |_| {
        let Some((id, _)) = pending_delete_for_confirm.get_untracked() else {
            return;
        };
        pending_delete_for_confirm.set(None);
        let Some(host_id) = state_for_confirm_delete.selected_host_id.get_untracked() else {
            return;
        };
        let Some((host_id, host_stream)) = host_stream_with_id(&state_for_confirm_delete, &host_id)
        else {
            return;
        };
        spawn_local(async move {
            if let Err(error) = steering_delete(&host_id, host_stream, id).await {
                log::error!("failed to send steering_delete: {error}");
            }
        });
    });

    view! {
        <div class="settings-panel-header">
            <h2 class="settings-panel-title">"Steering"</h2>
        </div>
        <p class="settings-description settings-panel-intro">
            "Long-lived guidance injected into agent context. Scope to the host or a specific project."
        </p>

        <div class="settings-field">
            <div class="settings-form-footer">
                <button
                    class="settings-btn settings-btn-primary"
                    disabled=move || state_for_new_disabled.selected_host_id.get().is_none()
                    on:click=move |_| form.set(Some(SteeringForm::blank()))
                >
                    "+ New steering"
                </button>
            </div>
        </div>

        {move || form.get().map(|f| view! { <SteeringEditor form=f editor_signal=form /> })}

        <div class="settings-field">
            <div class="settings-host-list">
                {move || {
                    let list = rows.get();
                    if list.is_empty() {
                        view! { <div class="panel-empty">"No steering on this host."</div> }.into_any()
                    } else {
                        view! {
                            <>
                            {list.into_iter().map(|item| view! {
                                <SteeringRow item=item editor_signal=form delete_signal=pending_delete />
                            }).collect_view()}
                            </>
                        }.into_any()
                    }
                }}
            </div>
        </div>

        {move || {
            pending_delete.get().map(|(_, title)| {
                let on_cancel = on_cancel_delete;
                let on_confirm = on_confirm_delete;
                let body = format!("Delete steering \"{title}\"? This cannot be undone.");
                view! {
                    <SettingsConfirmDialog
                        title="Delete steering".to_string()
                        body=body
                        confirm_label="Delete".to_string()
                        on_cancel=on_cancel
                        on_confirm=on_confirm
                    />
                }
            })
        }}
    }
}

#[component]
fn SteeringRow(
    item: Steering,
    editor_signal: RwSignal<Option<SteeringForm>>,
    delete_signal: RwSignal<Option<PendingSteeringDelete>>,
) -> impl IntoView {
    let scope_label = match &item.scope {
        SteeringScope::Host => "Host".to_string(),
        SteeringScope::Project(id) => format!("Project · {}", id.0),
    };

    let item_for_edit = item.clone();
    let on_edit = move |_| editor_signal.set(Some(SteeringForm::from_steering(&item_for_edit)));

    let id_for_delete = item.id.clone();
    let title_display = if item.title.is_empty() {
        "(untitled)".to_string()
    } else {
        item.title.clone()
    };
    let title_for_delete = title_display.clone();
    let on_delete =
        move |_| delete_signal.set(Some((id_for_delete.clone(), title_for_delete.clone())));

    view! {
        <div class="host-card">
            <div class="host-card-main">
                <div class="host-card-title-row">
                    <span class="host-card-label">{title_display}</span>
                </div>
                <p class="host-card-transport">{scope_label}</p>
            </div>
            <div class="host-card-actions">
                <button class="settings-btn" on:click=on_edit>"Edit"</button>
                <button class="settings-btn settings-btn-danger" on:click=on_delete>"Delete"</button>
            </div>
        </div>
    }
}

#[component]
fn SteeringEditor(
    form: SteeringForm,
    editor_signal: RwSignal<Option<SteeringForm>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let title = if form.is_new {
        "New Steering"
    } else {
        "Edit Steering"
    };

    let scope_kind_sig = form.scope_kind;
    let title_sig = form.title;
    let content_sig = form.content;

    let state_for_projects = state.clone();
    let available_projects = Memo::new(move |_| {
        let Some(host_id) = state_for_projects.selected_host_id.get() else {
            return Vec::new();
        };
        state_for_projects
            .projects
            .get()
            .into_iter()
            .filter(|p| p.host_id == host_id)
            .collect::<Vec<_>>()
    });

    let steering_error_sig: RwSignal<Option<String>> = RwSignal::new(None);

    let form_for_save = form.clone();
    let state_for_save = state.clone();
    let steering_error_sig_for_save = steering_error_sig;
    let editor_signal_for_save = editor_signal;
    let on_save = move |_| {
        if form_for_save.title.get_untracked().trim().is_empty() {
            steering_error_sig_for_save.set(Some("Title is required.".to_string()));
            return;
        }
        let Some(host_id) = state_for_save.selected_host_id.get_untracked() else {
            steering_error_sig_for_save.set(Some("No host selected.".to_string()));
            return;
        };
        let Some((host_id, host_stream)) = host_stream_with_id(&state_for_save, &host_id) else {
            steering_error_sig_for_save.set(Some("Host is not connected.".to_string()));
            return;
        };
        steering_error_sig_for_save.set(None);
        let steering = form_for_save.to_steering();
        spawn_local(async move {
            match steering_upsert(&host_id, host_stream, steering).await {
                Ok(()) => editor_signal_for_save.set(None),
                Err(error) => steering_error_sig_for_save
                    .set(Some(format!("Failed to save steering: {error}"))),
            }
        });
    };

    let on_cancel = move |_| editor_signal.set(None);

    view! {
        <div class="settings-field">
            <label class="settings-label">{title}</label>
            <div class="settings-form">
                <label class="settings-form-label">
                    <span>"Scope"</span>
                    <select
                        class="settings-select settings-select-full"
                        prop:value=move || scope_kind_sig.get()
                        on:change=move |ev: web_sys::Event| {
                            let target = ev.target().unwrap();
                            let el: web_sys::HtmlSelectElement = target.unchecked_into();
                            scope_kind_sig.set(el.value());
                        }
                    >
                        <option value="host">"Host"</option>
                        {move || available_projects.get().into_iter().map(|p| {
                            let id = p.project.id.0.clone();
                            let label = format!("Project · {}", p.project.name);
                            view! { <option value=id>{label}</option> }
                        }).collect_view()}
                    </select>
                </label>

                <label class="settings-form-label">
                    <span>"Title"</span>
                    <input
                        class="settings-text-input"
                        type="text"
                        prop:value=move || title_sig.get()
                        on:input=move |ev| title_sig.set(event_target_value(&ev))
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
                    />
                </label>

                <label class="settings-form-label">
                    <span>"Content"</span>
                    <textarea
                        class="settings-text-input"
                        rows="8"
                        prop:value=move || content_sig.get()
                        on:input=move |ev| content_sig.set(event_target_value(&ev))
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
                    />
                </label>

                <Show when=move || steering_error_sig.get().is_some()>
                    <p class="settings-error">{move || steering_error_sig.get().unwrap_or_default()}</p>
                </Show>

                <div class="settings-form-footer">
                    <button class="settings-btn" on:click=on_cancel>"Cancel"</button>
                    <button class="settings-btn settings-btn-primary" on:click=on_save>"Save"</button>
                </div>
            </div>
        </div>
    }
}

// ── Skills ──────────────────────────────────────────────────────────────

#[component]
fn SkillsTab() -> impl IntoView {
    let state = expect_context::<AppState>();

    let state_for_rows = state.clone();
    let rows = Memo::new(move |_| {
        let Some(host_id) = state_for_rows.selected_host_id.get() else {
            return Vec::new();
        };
        let mut skills: Vec<Skill> = state_for_rows
            .skills
            .get()
            .get(&host_id)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    });

    let state_for_refresh = state.clone();
    let state_for_refresh_disabled = state.clone();
    let on_refresh = move |_| {
        let Some(host_id) = state_for_refresh.selected_host_id.get_untracked() else {
            log::error!("skills: refresh clicked without a selected host");
            return;
        };
        let Some((host_id, host_stream)) = host_stream_with_id(&state_for_refresh, &host_id) else {
            log::error!("skills: refresh clicked without a host stream");
            return;
        };
        spawn_local(async move {
            if let Err(error) = skill_refresh(&host_id, host_stream).await {
                log::error!("failed to send skill_refresh: {error}");
            }
        });
    };

    view! {
        <div class="settings-panel-header">
            <h2 class="settings-panel-title">"Skills"</h2>
        </div>
        <p class="settings-description settings-panel-intro">
            "Skills are discovered from the filesystem. Edit SKILL.md under "<code>"~/.tyde/skills/<name>/"</code>" and click Refresh to re-scan."
        </p>

        <div class="settings-field">
            <div class="settings-form-footer">
                <button
                    class="settings-btn settings-btn-primary"
                    disabled=move || state_for_refresh_disabled.selected_host_stream_untracked().is_none()
                    on:click=on_refresh
                >"Refresh"</button>
            </div>
        </div>

        <div class="settings-field">
            <div class="settings-host-list">
                {move || {
                    let list = rows.get();
                    if list.is_empty() {
                        view! { <div class="panel-empty">"No skills on this host."</div> }.into_any()
                    } else {
                        view! {
                            <>
                            {list.into_iter().map(|skill| {
                                let title = skill.title.clone().unwrap_or_else(|| skill.name.clone());
                                let description = skill.description.clone().unwrap_or_else(|| "No description".to_string());
                                view! {
                                    <div class="host-card">
                                        <div class="host-card-main">
                                            <div class="host-card-title-row">
                                                <span class="host-card-label">{title}</span>
                                            </div>
                                            <p class="host-card-transport">{description}</p>
                                        </div>
                                    </div>
                                }
                            }).collect_view()}
                            </>
                        }.into_any()
                    }
                }}
            </div>
        </div>
    }
}
