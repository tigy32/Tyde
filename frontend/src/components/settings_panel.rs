use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::app::{connect_one_host, refresh_configured_hosts};
use crate::bridge::{self, HostTransportConfig as BridgeHostTransportConfig};
use crate::send::send_frame;
use crate::state::{AppState, DiffViewMode, ToolOutputMode};

use protocol::{
    BackendConfigField, BackendConfigFieldType, BackendConfigSnapshotStatus, BackendConfigValues,
    BackendKind, BackendSetupAction, BackendSetupInfo, BackendSetupStatus, BackgroundAgentFeature,
    BrokerUrl, CodeIntelProviderId, CustomAgent, CustomAgentId, DEFAULT_MOBILE_MQTT_BROKER_URL,
    DiffContextMode, FrameKind, HostExecutablePath, HostLaunchProfileConfig, HostSettingValue,
    LaunchProfileId, McpServerConfig, McpServerId, McpTransportConfig, MobileAccessStatePayload,
    MobileBrokerStatus, MobileDeviceState, MobilePairingOfferId, MobilePairingOfferPayload,
    MobilePairingState, ProjectId, RunBackendSetupPayload, SelectOption, SessionSchemaEntry,
    SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema, SessionSettingsValues,
    SetSettingPayload, Skill, SkillId, Steering, SteeringId, SteeringScope, ToolPolicy,
};

use std::collections::{HashMap, HashSet};

use crate::components::session_settings::SessionSettingsControls;
use crate::send::{
    custom_agent_delete, custom_agent_upsert, mcp_server_delete, mcp_server_upsert,
    mobile_device_revoke, mobile_pairing_cancel, mobile_pairing_start, skill_refresh,
    steering_delete, steering_upsert,
};

const RESERVED_MCP_NAMES: &[&str] = &["tyde-debug", "tyde-agent-control", "tyde-review-feedback"];

/// Frontend-side mirror of `mqtt-transport::validate_broker_url`'s
/// scheme acceptance rules. We intentionally check ONLY the scheme
/// here (a coarse, low-risk filter) — the server is still the
/// authoritative validator and will reject finer-grained problems
/// like fragments or embedded credentials. Keeping this filter narrow
/// means the user gets immediate visible feedback on the most common
/// mistake ("mqtt://" vs "mqtts://") without the UI duplicating the
/// full URL grammar the server checks.
fn validate_broker_url_input(raw: &str) -> Result<(), &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("mqtts://") || lower.starts_with("wss://") {
        let after_scheme = trimmed
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or("");
        if after_scheme.is_empty() {
            return Err("Broker URL is missing a host after the scheme.");
        }
        if after_scheme.contains('@') {
            return Err(
                "Broker credentials must be supplied out-of-band, not embedded in the URL.",
            );
        }
        if after_scheme.contains('#') {
            return Err("Broker URL fragments (#…) are not supported.");
        }
        Ok(())
    } else if lower.starts_with("mqtt://")
        || lower.starts_with("ws://")
        || lower.starts_with("tcp://")
    {
        Err("Insecure scheme — use mqtts:// or wss:// instead.")
    } else if lower.contains("://") {
        Err("Unsupported scheme — only mqtts:// and wss:// are accepted.")
    } else {
        Err("Broker URL must start with mqtts:// or wss://.")
    }
}

/// Render the pairing `qr_uri` as an inline SVG QR code. Returns an
/// SVG string that callers splat into the DOM via `inner_html`.
/// Uses `qrcodegen` (pure Rust, no transitive deps) with Medium ECC —
/// the QR fits comfortably on the user's monitor and the pairing
/// session has a short TTL so degraded scan ergonomics matter less
/// than keeping the QR small. Returns `None` if the input exceeds
/// QR-version-40 capacity (~2953 bytes), which would indicate a
/// malformed `qr_uri` from the server rather than a legitimate
/// pairing payload.
fn render_pairing_qr_svg(qr_uri: &str) -> Option<String> {
    let qr = qrcodegen::QrCode::encode_text(qr_uri, qrcodegen::QrCodeEcc::Medium).ok()?;
    // `qrcodegen` ships the module bitmap but no SVG writer (their
    // demo crate provides one). Implement a tiny SVG emitter inline.
    // border=2 keeps the quiet zone the spec requires. Fill/stroke
    // are CSS-overridable via `.settings-mobile-pairing-qr rect`.
    let border: i32 = 2;
    let size = qr.size();
    let dim = size + border * 2;
    use std::fmt::Write;
    let mut out = String::with_capacity((size * size) as usize * 32);
    let _ = write!(
        out,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {dim} {dim}" stroke="none" shape-rendering="crispEdges"><rect width="100%" height="100%" fill="#ffffff"/>"##,
    );
    for y in 0..size {
        for x in 0..size {
            if qr.get_module(x, y) {
                let _ = write!(
                    out,
                    r##"<rect x="{}" y="{}" width="1" height="1" fill="#000000"/>"##,
                    x + border,
                    y + border,
                );
            }
        }
    }
    out.push_str("</svg>");
    Some(out)
}

/// Compute how many seconds remain until `expires_at_ms` (millis since
/// epoch). Returns `None` if the host clock can't be read. Saturating
/// arithmetic so a stale offer reports 0 instead of overflowing.
fn expires_in_seconds(expires_at_ms: u64) -> Option<u64> {
    let now = js_sys::Date::now();
    if !now.is_finite() {
        return None;
    }
    let now_ms = now.max(0.0) as u64;
    let remaining_ms = expires_at_ms.saturating_sub(now_ms);
    Some(remaining_ms / 1000)
}

/// Short, user-facing label for the broker connection state.
/// Doesn't include the broker URL itself — that's already visible
/// below in the Broker URL field. Errors include the server message
/// verbatim because it's the most actionable info we have.
fn broker_status_line(status: &MobileBrokerStatus) -> String {
    match status {
        MobileBrokerStatus::Disabled => "Mobile connections disabled".to_owned(),
        MobileBrokerStatus::Connecting { .. } => "Connecting to broker…".to_owned(),
        MobileBrokerStatus::Online { .. } => "Broker online".to_owned(),
        MobileBrokerStatus::Error { message, .. } => format!("Broker error: {message}"),
    }
}

/// Slug used by CSS to pick a per-state color for the broker pill.
fn broker_status_slug(status: &MobileBrokerStatus) -> &'static str {
    match status {
        MobileBrokerStatus::Disabled => "disabled",
        MobileBrokerStatus::Connecting { .. } => "connecting",
        MobileBrokerStatus::Online { .. } => "online",
        MobileBrokerStatus::Error { .. } => "error",
    }
}

/// Status line for the pairing lifecycle. Returns `None` for `Idle`
/// (no useful message to show — the absence of a status line is
/// itself the signal that pairing is idle).
fn pairing_status_line(phase: &MobilePairingState) -> Option<String> {
    match phase {
        MobilePairingState::Idle => None,
        MobilePairingState::Active { .. } => Some("Pairing in progress — scan the QR.".to_owned()),
        MobilePairingState::Consumed { .. } => {
            Some("Device paired. Open Tyde on your mobile to confirm.".to_owned())
        }
        MobilePairingState::Expired { .. } => {
            Some("Pairing expired. Start a new pairing session to try again.".to_owned())
        }
        MobilePairingState::Cancelled { .. } => Some("Pairing cancelled.".to_owned()),
        MobilePairingState::Failed { message, .. } => Some(format!("Pairing failed: {message}")),
    }
}

const STORAGE_THEME: &str = "tyde-theme";
const STORAGE_FONT_SIZE: &str = "tyde-font-size";
const STORAGE_FONT_FAMILY: &str = "tyde-font-family";
const STORAGE_SYNTAX_THEME: &str = "tyde-syntax-theme";
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

pub fn persist_syntax_theme(name: &str) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(STORAGE_SYNTAX_THEME, name);
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

    // ---- broker URL validator ----
    //
    // These tests mirror the rules the server-side `validate_broker_url`
    // enforces in `mqtt-transport`. The frontend's filter is intentionally
    // coarser (scheme + common shape) — the server is the authoritative
    // validator. If the server tightens its rules and the frontend
    // doesn't, a typed URL will still hit a server-side error and the
    // existing inline-error path will surface it.

    #[test]
    fn broker_url_validator_accepts_empty() {
        // Empty input = "use host default", not an error.
        assert!(validate_broker_url_input("").is_ok());
        assert!(validate_broker_url_input("   ").is_ok());
    }

    #[test]
    fn broker_url_validator_accepts_mqtts_and_wss() {
        assert!(validate_broker_url_input("mqtts://broker.example:8883").is_ok());
        assert!(validate_broker_url_input("wss://broker.example/relay").is_ok());
        // Case-insensitive on scheme — URLs are case-insensitive there.
        assert!(validate_broker_url_input("MQTTS://broker.example:8883").is_ok());
    }

    #[test]
    fn broker_url_validator_rejects_insecure_schemes() {
        for bad in [
            "mqtt://broker.example",
            "ws://broker.example",
            "tcp://x.test",
        ] {
            let err = validate_broker_url_input(bad)
                .expect_err(&format!("expected {bad:?} to be rejected"));
            assert!(
                err.contains("Insecure") || err.contains("insecure"),
                "error for {bad:?} should mention insecure scheme: {err}"
            );
        }
    }

    #[test]
    fn broker_url_validator_rejects_unknown_or_missing_scheme() {
        // Wrong scheme.
        assert!(validate_broker_url_input("http://broker.example").is_err());
        // No scheme separator at all.
        assert!(validate_broker_url_input("broker.example:8883").is_err());
        // Empty after scheme.
        assert!(validate_broker_url_input("mqtts://").is_err());
    }

    #[test]
    fn broker_url_validator_rejects_embedded_credentials() {
        let err = validate_broker_url_input("mqtts://user:pass@broker.example")
            .expect_err("URL with @ must be rejected");
        assert!(
            err.contains("credentials"),
            "error must mention credentials: {err}"
        );
    }

    #[test]
    fn broker_url_validator_rejects_fragments() {
        let err = validate_broker_url_input("mqtts://broker.example#frag")
            .expect_err("URL with fragment must be rejected");
        assert!(
            err.contains("fragment"),
            "error must mention fragments: {err}"
        );
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

    if let Ok(Some(theme_name)) = storage.get_item(STORAGE_SYNTAX_THEME)
        && crate::syntax_highlight::set_selected_theme(&theme_name)
    {
        state.syntax_theme.set(theme_name);
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
    Mobile,
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
            Self::Mobile => "Mobile",
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
                "Code intelligence",
                "rust-analyzer binary path",
                "custom toolchain",
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
                "Antigravity",
                "Hermes",
                "Nous",
                "Nous Research",
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
            Self::Mobile => &[
                "Mobile",
                "Mobile connections",
                "Enable mobile connections",
                "Broker URL",
                "Public MQTT broker",
                "MQTT",
                "broker.emqx.io",
                "Encryption",
                "Metadata",
                "QR",
                "Pairing",
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

const ALL_TABS: [SettingsTab; 10] = [
    SettingsTab::Hosts,
    SettingsTab::Appearance,
    SettingsTab::General,
    SettingsTab::Backends,
    SettingsTab::CustomAgents,
    SettingsTab::McpServers,
    SettingsTab::Steering,
    SettingsTab::Skills,
    SettingsTab::Mobile,
    SettingsTab::Debug,
];

#[component]
pub fn SettingsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let active_tab = RwSignal::new(SettingsTab::Appearance);
    let search_query = RwSignal::new(String::new());

    // Honor deep-link requests (e.g. the onboarding "Set up an AI engine" CTA
    // asking to open straight to the Backends tab).
    {
        let state = state.clone();
        Effect::new(move |_| {
            if let Some(label) = state.settings_tab_request.get() {
                if let Some(tab) = ALL_TABS.into_iter().find(|tab| tab.label() == label) {
                    active_tab.set(tab);
                }
                state.settings_tab_request.set(None);
            }
        });
    }

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
                                SettingsTab::Mobile => view! { <MobileTab /> }.into_any(),
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
                bridge::RemoteHostLifecycleConfig::ManagedTyde
            } else {
                bridge::RemoteHostLifecycleConfig::Manual
            };

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
                            connect_one_host(state.clone(), host_id).await;
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
            <p class="settings-description">"The embedded local host is always present. Managed SSH hosts install and launch the exact Tyde Server release matching this app at ~/.tyde/bin/<version>/tyde-server."</p>
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
                                        <p class="host-card-lifecycle">{format!("Tyde server: {lifecycle_text}")}</p>
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
                                                        // Explicit user disconnect ends the connection
                                                        // lifecycle, so release the one-shot forced-upgrade
                                                        // guard: a later manual reconnect can attempt the
                                                        // auto-upgrade once more. Only cleared here (not on
                                                        // transport-drop) to preserve the no-loop invariant.
                                                        state.clear_upgrade_attempted(&host_id);
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
                                                        connect_one_host(state, host_id).await;
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
            <p class="settings-description">"Configure a remote host over SSH. Leave Remote command blank for managed install/launch of the same Tyde release as this app. Set Remote command only for a manual bridge command."</p>
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
            lifecycle: bridge::RemoteHostLifecycleConfig::ManagedTyde,
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
            let target = &snapshot.target_version;
            match &snapshot.running {
                bridge::RemoteTydeRunningState::Managed { version } if version == target => {
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
                if version == &snapshot.target_version =>
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

    let on_syntax_theme = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlSelectElement = target.unchecked_into();
        let name = el.value();
        if crate::syntax_highlight::set_selected_theme(&name) {
            state.syntax_theme.set(name.clone());
            persist_syntax_theme(&name);
        }
    };

    let syntax_themes: Vec<String> = crate::syntax_highlight::available_themes();

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
            <label class="settings-label">"Syntax Theme"</label>
            <p class="settings-description">"Color scheme for syntax highlighting in the file viewer, diff viewer, and chat code blocks. Reopen a file to see the change."</p>
            <select
                class="settings-select"
                prop:value=move || state.syntax_theme.get()
                on:change=on_syntax_theme
            >
                {syntax_themes.into_iter().map(|name| {
                    let label = name.clone();
                    view! { <option value=name>{label}</option> }
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

        <BackgroundAgentFeaturesSection />
        <CodeIntelSettingsSection />
    }
}

const RUST_ANALYZER_PROVIDER_ID: &str = "rust-analyzer";

fn rust_analyzer_provider_id() -> CodeIntelProviderId {
    CodeIntelProviderId(RUST_ANALYZER_PROVIDER_ID.to_owned())
}

#[component]
fn CodeIntelSettingsSection() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_value = state.clone();
    let state_for_disabled = state.clone();
    let state_for_commit = state.clone();
    let state_for_keydown = state.clone();
    let state_for_clear = state.clone();

    let path_value = move || {
        state_for_value
            .selected_host_settings()
            .and_then(|settings| {
                settings
                    .code_intel
                    .language_server_paths
                    .get(&rust_analyzer_provider_id())
                    .map(|path| path.0.clone())
            })
            .unwrap_or_default()
    };
    let disabled = move || state_for_disabled.selected_host_settings().is_none();
    let disabled_for_input = disabled.clone();
    let disabled_for_button = disabled.clone();

    let commit_path = move |state: &AppState, raw: &str| {
        let trimmed = raw.trim();
        let path = if trimmed.is_empty() {
            None
        } else {
            Some(HostExecutablePath(trimmed.to_owned()))
        };
        send_host_setting(
            state,
            HostSettingValue::CodeIntelLanguageServerPath {
                provider: rust_analyzer_provider_id(),
                path,
            },
        );
    };

    let on_commit = move |ev: web_sys::Event| {
        let Some(target) = ev.target() else {
            return;
        };
        let Ok(input) = target.dyn_into::<web_sys::HtmlInputElement>() else {
            return;
        };
        commit_path(&state_for_commit, &input.value());
    };
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() != "Enter" {
            return;
        }
        ev.prevent_default();
        let Some(target) = ev.target() else {
            return;
        };
        let Ok(input) = target.dyn_into::<web_sys::HtmlInputElement>() else {
            return;
        };
        commit_path(&state_for_keydown, &input.value());
    };
    let on_clear = move |_: web_sys::MouseEvent| {
        send_host_setting(
            &state_for_clear,
            HostSettingValue::CodeIntelLanguageServerPath {
                provider: rust_analyzer_provider_id(),
                path: None,
            },
        );
    };

    view! {
        <h3 class="settings-section-title">"Code intelligence"</h3>

        <div class="settings-field">
            <label class="settings-label">"rust-analyzer binary path"</label>
            <p class="settings-description">
                "Optional absolute path to a standalone rust-analyzer binary. Use this for custom toolchains where the rustup proxy in ~/.cargo/bin cannot install rust-analyzer."
            </p>
            <div class="settings-mobile-broker-row">
                <input
                    class="settings-input settings-code-intel-path-input"
                    type="text"
                    prop:value=path_value
                    placeholder="/path/to/rust-analyzer"
                    disabled=disabled_for_input
                    aria-label="rust-analyzer binary path"
                    spellcheck="false"
                    {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                    autocapitalize="none"
                    autocomplete="off"
                    on:change=on_commit
                    on:keydown=on_keydown
                />
                <button
                    type="button"
                    class="filter-toggle settings-code-intel-path-clear"
                    disabled=disabled_for_button
                    title="Clear rust-analyzer binary path"
                    on:click=on_clear
                >
                    "Clear"
                </button>
            </div>
        </div>
    }
}

/// "Background agent features" — opt-in background model calls that enhance the
/// agent UI. Both toggles spend money because they run extra model calls, so
/// the copy is explicit about cost and the activity-summaries toggle defaults
/// off. Values are reflected from `HostSettings.background_agent_features` and
/// each change is sent as a typed `BackgroundAgentFeatureEnabled` setting.
#[component]
fn BackgroundAgentFeaturesSection() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_names_checked = state.clone();
    let state_for_summaries_checked = state.clone();
    let state_for_names_disabled = state.clone();
    let state_for_summaries_disabled = state.clone();

    let names_checked = move || {
        state_for_names_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.background_agent_features.auto_generate_agent_names)
    };
    let summaries_checked = move || {
        state_for_summaries_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.background_agent_features.agent_activity_summaries)
    };
    let names_disabled = move || state_for_names_disabled.selected_host_settings().is_none();
    let summaries_disabled = move || {
        state_for_summaries_disabled
            .selected_host_settings()
            .is_none()
    };

    let names_on_toggle = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            send_host_setting(
                &state,
                HostSettingValue::BackgroundAgentFeatureEnabled {
                    feature: BackgroundAgentFeature::AutoGenerateAgentNames,
                    enabled: input.checked(),
                },
            );
        }
    };

    let summaries_on_toggle = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            send_host_setting(
                &state,
                HostSettingValue::BackgroundAgentFeatureEnabled {
                    feature: BackgroundAgentFeature::AgentActivitySummaries,
                    enabled: input.checked(),
                },
            );
        }
    };

    view! {
        <h3 class="settings-section-title">"Background agent features"</h3>

        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Auto-generate agent names"</label>
                    <p class="settings-description">
                        "When an agent is started without a name, Tyde asks a cheap model to name it from the opening prompt. This makes an extra background model call that costs money. When off, the agent keeps a simple name derived from its prompt and no model is called."
                    </p>
                </div>
                <label class="settings-toggle">
                    <input
                        type="checkbox"
                        prop:checked=names_checked
                        disabled=names_disabled
                        on:change=names_on_toggle
                    />
                    <span class="settings-toggle-slider"></span>
                </label>
            </div>
        </div>

        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Agent activity summaries"</label>
                    <p class="settings-description">
                        "Periodically summarize what each active agent is doing so a short \"what is this agent doing?\" line can appear in agent views. This runs a model in the background on a schedule and costs money for as long as agents stay active. Off by default."
                    </p>
                </div>
                <label class="settings-toggle">
                    <input
                        type="checkbox"
                        prop:checked=summaries_checked
                        disabled=summaries_disabled
                        on:change=summaries_on_toggle
                    />
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

        <ComplexityTiersSection />
        <BackendConfigSection />
        <LaunchProfilesSection />
    }
}

// ── Launch Profiles ─────────────────────────────────────────────────────

type PendingLaunchProfileDelete = (LaunchProfileId, String);

#[derive(Clone)]
struct LaunchProfileForm {
    id: RwSignal<String>,
    is_new: bool,
    label: RwSignal<String>,
    description: RwSignal<String>,
    backend_kind: RwSignal<BackendKind>,
    session_settings: RwSignal<SessionSettingsValues>,
}

impl LaunchProfileForm {
    fn from_config(config: &HostLaunchProfileConfig) -> Self {
        Self {
            id: RwSignal::new(config.id.0.clone()),
            is_new: false,
            label: RwSignal::new(config.label.clone()),
            description: RwSignal::new(config.description.clone().unwrap_or_default()),
            backend_kind: RwSignal::new(config.backend_kind),
            session_settings: RwSignal::new(config.session_settings.clone()),
        }
    }

    fn blank() -> Self {
        Self {
            id: RwSignal::new(String::new()),
            is_new: true,
            label: RwSignal::new(String::new()),
            description: RwSignal::new(String::new()),
            backend_kind: RwSignal::new(BackendKind::Hermes),
            session_settings: RwSignal::new(SessionSettingsValues::default()),
        }
    }

    fn validate_and_build(&self) -> Result<HostLaunchProfileConfig, String> {
        let id = self.id.get_untracked().trim().to_string();
        if id.is_empty() {
            return Err("Profile id is required.".to_string());
        }
        let label = self.label.get_untracked().trim().to_string();
        if label.is_empty() {
            return Err("Label is required.".to_string());
        }
        if is_reserved_launch_profile_id(&id) {
            return Err(format!(
                "\"{id}\" is reserved for a built-in default profile. Choose a different id."
            ));
        }
        let description = self.description.get_untracked().trim().to_string();
        Ok(HostLaunchProfileConfig {
            id: LaunchProfileId(id),
            label,
            description: if description.is_empty() {
                None
            } else {
                Some(description)
            },
            backend_kind: self.backend_kind.get_untracked(),
            session_settings: self.session_settings.get_untracked(),
        })
    }
}

/// Explicit server-owned Launch Profiles: named backend + session-settings
/// presets (e.g. `hermes:claude`) that show up as ready entries in the New Chat
/// menu. Persisted through `HostSettingValue::LaunchProfiles`.
#[component]
fn LaunchProfilesSection() -> impl IntoView {
    let state = expect_context::<AppState>();
    let form: RwSignal<Option<LaunchProfileForm>> = RwSignal::new(None);
    let pending_delete: RwSignal<Option<PendingLaunchProfileDelete>> = RwSignal::new(None);

    let state_for_rows = state.clone();
    let rows = Memo::new(move |_| {
        state_for_rows
            .selected_host_settings()
            .map(|settings| settings.launch_profiles)
            .unwrap_or_default()
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
        let Some(settings) = state_for_confirm_delete.selected_host_settings_untracked() else {
            return;
        };
        let profiles = settings
            .launch_profiles
            .into_iter()
            .filter(|p| p.id != id)
            .collect();
        send_host_setting(
            &state_for_confirm_delete,
            HostSettingValue::LaunchProfiles { profiles },
        );
    });

    view! {
        <div class="settings-field">
            <label class="settings-label">"Launch Profiles"</label>
            <p class="settings-description">
                "Named backend + session-settings presets that appear as ready entries in the New Chat menu. Saved on the selected host."
            </p>
            <div class="settings-form-footer">
                <button
                    class="settings-btn settings-btn-primary"
                    disabled=move || state_for_new_disabled.selected_host_id.get().is_none()
                    on:click=move |_| form.set(Some(LaunchProfileForm::blank()))
                >
                    "+ New launch profile"
                </button>
            </div>

            {move || form.get().map(|f| view! { <LaunchProfileEditor form=f editor_signal=form /> })}

            <div class="settings-host-list">
                {move || {
                    let list = rows.get();
                    if list.is_empty() {
                        view! { <div class="panel-empty">"No launch profiles on this host."</div> }.into_any()
                    } else {
                        view! {
                            <>
                            {list.into_iter().map(|config| view! {
                                <LaunchProfileRow config=config editor_signal=form delete_signal=pending_delete />
                            }).collect_view()}
                            </>
                        }.into_any()
                    }
                }}
            </div>

            {move || {
                pending_delete.get().map(|(_, label)| {
                    let on_cancel = on_cancel_delete;
                    let on_confirm = on_confirm_delete;
                    let body = format!("Delete launch profile \"{label}\"? This cannot be undone.");
                    view! {
                        <SettingsConfirmDialog
                            title="Delete launch profile".to_string()
                            body=body
                            confirm_label="Delete".to_string()
                            on_cancel=on_cancel
                            on_confirm=on_confirm
                        />
                    }
                })
            }}
        </div>
    }
}

#[component]
fn LaunchProfileRow(
    config: HostLaunchProfileConfig,
    editor_signal: RwSignal<Option<LaunchProfileForm>>,
    delete_signal: RwSignal<Option<PendingLaunchProfileDelete>>,
) -> impl IntoView {
    let config_for_edit = config.clone();
    let on_edit =
        move |_| editor_signal.set(Some(LaunchProfileForm::from_config(&config_for_edit)));

    let id_for_delete = config.id.clone();
    let label_for_delete = config.label.clone();
    let on_delete =
        move |_| delete_signal.set(Some((id_for_delete.clone(), label_for_delete.clone())));

    let subtitle = format!("{} · {}", config.id.0, backend_label(config.backend_kind));

    view! {
        <div class="host-card">
            <div class="host-card-main">
                <div class="host-card-title-row">
                    <span class="host-card-label">{config.label.clone()}</span>
                    <span class=backend_badge_class(config.backend_kind)>
                        {backend_label(config.backend_kind)}
                    </span>
                </div>
                <p class="host-card-transport">{subtitle}</p>
            </div>
            <div class="host-card-actions">
                <button class="settings-btn" on:click=on_edit>"Edit"</button>
                <button class="settings-btn settings-btn-danger" on:click=on_delete>"Delete"</button>
            </div>
        </div>
    }
}

#[component]
fn LaunchProfileEditor(
    form: LaunchProfileForm,
    editor_signal: RwSignal<Option<LaunchProfileForm>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let title = if form.is_new {
        "New Launch Profile"
    } else {
        "Edit Launch Profile"
    };

    let id_sig = form.id;
    let is_new = form.is_new;
    let label_sig = form.label;
    let description_sig = form.description;
    let backend_kind_sig = form.backend_kind;
    let session_settings_sig = form.session_settings;

    let error_sig: RwSignal<Option<String>> = RwSignal::new(None);

    // Typed session-settings controls for the selected backend, sourced from the
    // host's session schema. Falls back to a note when the schema is not
    // available (backend not installed / no configurable settings).
    let state_for_schema = state.clone();
    let schema_for_backend = move || -> Option<SessionSettingsSchema> {
        let host_id = state_for_schema.selected_host_id.get()?;
        let kind = backend_kind_sig.get();
        match state_for_schema
            .session_schemas
            .get()
            .get(&host_id)?
            .get(&kind)?
        {
            SessionSchemaEntry::Ready { schema } => Some(schema.clone()),
            _ => None,
        }
    };

    let settings_values: Signal<SessionSettingsValues> =
        Signal::derive(move || session_settings_sig.get());
    let settings_on_change =
        Callback::new(move |values: SessionSettingsValues| session_settings_sig.set(values));

    let on_backend_change = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlSelectElement = target.unchecked_into();
        let Some(kind) = parse_backend_kind(&el.value()) else {
            log::error!(
                "unknown backend value {} in launch profile editor",
                el.value()
            );
            return;
        };
        backend_kind_sig.set(kind);
        // Session settings are keyed per backend; drop stale keys on switch.
        session_settings_sig.set(SessionSettingsValues::default());
    };

    let backend_options = all_backends()
        .into_iter()
        .map(|kind| view! { <option value=backend_value(kind)>{backend_label(kind)}</option> })
        .collect::<Vec<_>>();

    // A profile whose backend isn't enabled is still stored server-side, but the
    // server filters it out of the launch catalog — so warn that it won't appear
    // in New Chat until the backend is enabled.
    let state_for_enabled = state.clone();
    let backend_disabled = move || {
        !state_for_enabled
            .selected_host_settings()
            .map(|settings| settings.enabled_backends.contains(&backend_kind_sig.get()))
            .unwrap_or(false)
    };

    let state_for_save = state.clone();
    let editor_signal_for_save = editor_signal;
    let error_sig_for_save = error_sig;
    let on_save = move |_| {
        let config = match form.validate_and_build() {
            Ok(config) => config,
            Err(error) => {
                error_sig_for_save.set(Some(error));
                return;
            }
        };
        let Some(settings) = state_for_save.selected_host_settings_untracked() else {
            error_sig_for_save.set(Some("No host selected.".to_string()));
            return;
        };
        let mut profiles = settings.launch_profiles;
        if is_new && profiles.iter().any(|p| p.id == config.id) {
            error_sig_for_save.set(Some(format!(
                "A launch profile with id \"{}\" already exists.",
                config.id.0
            )));
            return;
        }
        match profiles.iter_mut().find(|p| p.id == config.id) {
            Some(existing) => *existing = config,
            None => profiles.push(config),
        }
        error_sig_for_save.set(None);
        send_host_setting(
            &state_for_save,
            HostSettingValue::LaunchProfiles { profiles },
        );
        editor_signal_for_save.set(None);
    };

    let on_cancel = move |_| editor_signal.set(None);

    view! {
        <div class="settings-field">
            <label class="settings-label">{title}</label>
            <div class="settings-form">
                <label class="settings-form-label">
                    <span>"Id"<span class="settings-form-hint">" (e.g. hermes:claude)"</span></span>
                    <input
                        class="settings-text-input"
                        type="text"
                        placeholder="hermes:claude"
                        prop:value=move || id_sig.get()
                        on:input=move |ev| id_sig.set(event_target_value(&ev))
                        disabled=!is_new
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
                    />
                </label>

                <label class="settings-form-label">
                    <span>"Label"</span>
                    <input
                        class="settings-text-input"
                        type="text"
                        prop:value=move || label_sig.get()
                        on:input=move |ev| label_sig.set(event_target_value(&ev))
                        spellcheck="false"
                        {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                        autocapitalize="none"
                        autocomplete="off"
                    />
                </label>

                <label class="settings-form-label">
                    <span>"Description"<span class="settings-form-hint">" (optional)"</span></span>
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
                    <span>"Backend"</span>
                    <select
                        class="settings-select"
                        prop:value=move || backend_value(backend_kind_sig.get()).to_string()
                        on:change=on_backend_change
                    >
                        {backend_options}
                    </select>
                </label>

                <Show when=backend_disabled>
                    <p class="settings-form-warning" role="note">
                        "This backend is not enabled on the selected host. The profile will be saved, but it won't appear in New Chat until you enable the backend."
                    </p>
                </Show>

                <div class="settings-form-label">
                    <span>"Session settings"</span>
                    {move || match schema_for_backend() {
                        Some(schema) if !schema.fields.is_empty() => view! {
                            <SessionSettingsControls
                                schema=schema
                                values=settings_values
                                on_change=settings_on_change
                            />
                        }.into_any(),
                        Some(_) => view! {
                            <p class="settings-description">
                                "This backend has no configurable session settings."
                            </p>
                        }.into_any(),
                        None => view! {
                            <p class="settings-description">
                                "Session settings for this backend are unavailable on the selected host."
                            </p>
                        }.into_any(),
                    }}
                </div>

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

/// "Task complexity tiers" — master toggle plus, when enabled, the
/// per-backend Low/High tier mappings. The rows are generated from each
/// backend's session settings schema, so they show exactly the fields
/// (model, effort, ...) that backend supports.
#[component]
fn ComplexityTiersSection() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_checked = state.clone();
    let state_for_disabled = state.clone();
    let state_for_toggle = state.clone();
    let state_for_rows = state.clone();

    let checked = move || {
        state_for_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.complexity_tiers_enabled)
    };
    let disabled = move || state_for_disabled.selected_host_settings().is_none();
    let on_toggle = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let input: web_sys::HtmlInputElement = target.unchecked_into();
        send_host_setting(
            &state_for_toggle,
            HostSettingValue::ComplexityTiersEnabled {
                enabled: input.checked(),
            },
        );
    };

    view! {
        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Task complexity tiers"</label>
                    <p class="settings-description">
                        "Let agents and spawn dialogs request a cheaper, faster setup for trivial tasks (low) or the most capable one for extremely complex tasks (high). When disabled, every spawn uses the backend's own defaults and agents are never offered the choice."
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
            {move || complexity_tier_rows(&state_for_rows)}
        </div>
    }
}

fn complexity_tier_rows(state: &AppState) -> Option<AnyView> {
    let settings = state.selected_host_settings()?;
    if !settings.complexity_tiers_enabled {
        return None;
    }
    let host_id = state.selected_host_id.get()?;
    let schemas = state.session_schemas.get();
    let host_schemas = schemas.get(&host_id)?;
    let rows = settings
        .enabled_backends
        .iter()
        .copied()
        .filter_map(|kind| {
            let SessionSchemaEntry::Ready { schema } = host_schemas.get(&kind)? else {
                return None;
            };
            let select_fields = schema
                .fields
                .iter()
                .filter_map(|field| match &field.field_type {
                    SessionSettingFieldType::Select { options, .. } => {
                        Some((field.key.clone(), field.label.clone(), options.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if select_fields.is_empty() {
                return None;
            }
            let config = settings
                .backend_tier_configs
                .get(&kind)
                .cloned()
                .unwrap_or_default();
            Some(view! {
                <div class="settings-tier-backend">
                    <div class="settings-tier-backend-name">{backend_label(kind)}</div>
                    {tier_row(state, kind, false, &config.low, &select_fields)}
                    {tier_row(state, kind, true, &config.high, &select_fields)}
                </div>
            })
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Some(
            view! {
                <p class="settings-description">
                    "No enabled backends with configurable session settings on this host."
                </p>
            }
            .into_any(),
        );
    }
    Some(view! { <div class="settings-tier-list">{rows}</div> }.into_any())
}

fn tier_row(
    state: &AppState,
    kind: BackendKind,
    is_high: bool,
    values: &SessionSettingsValues,
    fields: &[(String, String, Vec<SelectOption>)],
) -> AnyView {
    let selects = fields
        .iter()
        .map(|(key, label, options)| {
            let current = match values.0.get(key) {
                Some(SessionSettingValue::String(value)) => value.clone(),
                _ => String::new(),
            };
            let option_views = options
                .iter()
                .map(|option| {
                    view! { <option value=option.value.clone()>{option.label.clone()}</option> }
                })
                .collect::<Vec<_>>();
            let state = state.clone();
            let key = key.clone();
            view! {
                <label class="settings-tier-select">
                    <span class="settings-tier-select-label">{label.clone()}</span>
                    <select
                        class="settings-select"
                        prop:value=current
                        on:change=move |ev: web_sys::Event| {
                            let target = ev.target().unwrap();
                            let el: web_sys::HtmlSelectElement = target.unchecked_into();
                            update_tier_setting(&state, kind, is_high, &key, el.value());
                        }
                    >
                        <option value="">"Backend default"</option>
                        {option_views}
                    </select>
                </label>
            }
        })
        .collect::<Vec<_>>();
    view! {
        <div class="settings-tier-row">
            <span class="settings-tier-name">{if is_high { "High" } else { "Low" }}</span>
            {selects}
        </div>
    }
    .into_any()
}

fn update_tier_setting(
    state: &AppState,
    kind: BackendKind,
    is_high: bool,
    key: &str,
    value: String,
) {
    let Some(mut settings) = state.selected_host_settings_untracked() else {
        return;
    };
    let mut config = settings
        .backend_tier_configs
        .remove(&kind)
        .unwrap_or_default();
    let tier_values = if is_high {
        &mut config.high
    } else {
        &mut config.low
    };
    if value.is_empty() {
        tier_values.0.remove(key);
    } else {
        tier_values
            .0
            .insert(key.to_owned(), SessionSettingValue::String(value));
    }
    send_host_setting(
        state,
        HostSettingValue::BackendTiers {
            backend: kind,
            config,
        },
    );
}

/// Per-backend deep configuration (e.g. Hermes default model/provider/base URL).
/// Rows are generated from each backend's `BackendConfigSchema`, so a backend
/// controls exactly which fields appear here with no frontend changes.
#[component]
fn BackendConfigSection() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_rows = state.clone();
    view! {
        {move || backend_config_rows(&state_for_rows)}
    }
}

fn backend_config_rows(state: &AppState) -> Option<AnyView> {
    let settings = state.selected_host_settings()?;
    let host_id = state.selected_host_id.get()?;
    let schemas = state.backend_config_schemas.get();
    let host_schemas = schemas.get(&host_id)?;
    let snapshots = state.backend_config_snapshots.get();
    let host_snapshots = snapshots.get(&host_id);

    let cards = settings
        .enabled_backends
        .iter()
        .copied()
        .filter_map(|kind| {
            let schema = host_schemas.get(&kind)?;
            if schema.fields.is_empty() {
                return None;
            }
            let values = settings
                .backend_config
                .get(&kind)
                .cloned()
                .unwrap_or_default();
            let snapshot = host_snapshots.and_then(|m| m.get(&kind));
            // Backend-native current values, only when the server could actually
            // read them. Never invented client-side.
            let native = snapshot
                .filter(|s| s.status == BackendConfigSnapshotStatus::Ready)
                .map(|s| s.values.clone())
                .unwrap_or_default();
            // The server owns field order, so render the first field as the
            // card's emphasized primary control (Tycode → Active Provider,
            // Hermes → Default Model) and the rest as a secondary grid. Emphasis
            // follows schema order, not any hard-coded key name.
            let fields = schema
                .fields
                .iter()
                .enumerate()
                .map(|(idx, field)| {
                    backend_config_field(state, kind, field, &values, &native, idx == 0)
                })
                .collect::<Vec<_>>();
            // Surface the server's own reason when it can't read native settings
            // instead of silently showing schema defaults as if they were live.
            let snapshot_note = snapshot
                .filter(|s| s.status == BackendConfigSnapshotStatus::Unavailable)
                .map(|s| {
                    let message = s.message.clone().unwrap_or_else(|| {
                        "Backend-native settings are currently unavailable on this host.".to_owned()
                    });
                    view! { <p class="settings-backend-config-snapshot-note">{message}</p> }
                });
            Some(view! {
                <div class="settings-backend-config-card">
                    <div class="settings-backend-config-card-header">
                        <span class=backend_badge_class(kind)>{backend_label(kind)}</span>
                    </div>
                    {snapshot_note}
                    <div class="settings-backend-config-fields">{fields}</div>
                </div>
            })
        })
        .collect::<Vec<_>>();

    if cards.is_empty() {
        return None;
    }
    Some(
        view! {
            <div class="settings-field">
                <label class="settings-label">"Backend Settings"</label>
                <p class="settings-description">
                    "Current backend-native settings for each enabled backend on the selected host, grouped per backend. Editing a field saves an explicit Tyde override that applies to every new session; clearing it restores the backend's own value."
                </p>
                <div class="settings-backend-config-list">{cards}</div>
            </div>
        }
        .into_any(),
    )
}

fn backend_config_field(
    state: &AppState,
    kind: BackendKind,
    field: &BackendConfigField,
    values: &BackendConfigValues,
    native: &BackendConfigValues,
    primary: bool,
) -> AnyView {
    let key = field.key.clone();
    let description = field.description.clone();

    let control = match &field.field_type {
        BackendConfigFieldType::Text {
            placeholder,
            multiline,
            ..
        } => {
            // Seed with the Tyde override when set, else the backend-native
            // current value from the snapshot. Editing writes an override.
            let current = string_value(values, &key).or_else(|| string_value(native, &key));
            let placeholder = placeholder.clone().unwrap_or_default();
            let state = state.clone();
            let key_for_change = key.clone();
            let on_change = move |ev: web_sys::Event| {
                let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
                commit_text_value(&state, kind, &key_for_change, el.value());
            };
            if *multiline {
                view! {
                    <textarea
                        class="settings-input settings-backend-config-input"
                        prop:value=current
                        placeholder=placeholder
                        on:change=on_change
                    ></textarea>
                }
                .into_any()
            } else {
                view! {
                    <input
                        type="text"
                        class="settings-input settings-backend-config-input"
                        prop:value=current
                        placeholder=placeholder
                        autocomplete="off"
                        spellcheck="false"
                        on:change=on_change
                    />
                }
                .into_any()
            }
        }
        BackendConfigFieldType::Secret { placeholder } => {
            // Never pre-fill the stored secret; show whether one is set.
            let has_value = string_value(values, &key).is_some_and(|value| !value.is_empty());
            let placeholder = placeholder.clone().unwrap_or_else(|| {
                if has_value {
                    "•••••••• (stored — type to replace)".to_owned()
                } else {
                    String::new()
                }
            });
            let state = state.clone();
            let key_for_change = key.clone();
            let on_change = move |ev: web_sys::Event| {
                let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
                // Empty clears the stored secret; non-empty replaces it.
                commit_text_value(&state, kind, &key_for_change, el.value());
            };
            view! {
                <input
                    type="password"
                    class="settings-input settings-backend-config-input"
                    placeholder=placeholder
                    autocomplete="off"
                    on:change=on_change
                />
            }
            .into_any()
        }
        BackendConfigFieldType::Select {
            options,
            nullable,
            default,
        } => {
            let current = match values.0.get(&key).or_else(|| native.0.get(&key)) {
                Some(SessionSettingValue::String(value)) => value.clone(),
                _ => default.clone().unwrap_or_default(),
            };
            let nullable = *nullable;
            let option_views = options
                .iter()
                .map(|option| {
                    view! { <option value=option.value.clone()>{option.label.clone()}</option> }
                })
                .collect::<Vec<_>>();
            let state = state.clone();
            let key_for_change = key.clone();
            let on_change = move |ev: web_sys::Event| {
                let el: web_sys::HtmlSelectElement = ev.target().unwrap().unchecked_into();
                let value = el.value();
                let update = if value.is_empty() {
                    None
                } else {
                    Some(SessionSettingValue::String(value))
                };
                update_backend_config(&state, kind, &key_for_change, update);
            };
            view! {
                <select class="settings-select" prop:value=current on:change=on_change>
                    {nullable.then(|| view! { <option value="">"Auto"</option> })}
                    {option_views}
                </select>
            }
            .into_any()
        }
        BackendConfigFieldType::Toggle { default } => {
            let current = match values.0.get(&key).or_else(|| native.0.get(&key)) {
                Some(SessionSettingValue::Bool(value)) => *value,
                _ => *default,
            };
            let state = state.clone();
            let key_for_change = key.clone();
            let on_change = move |ev: web_sys::Event| {
                let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
                update_backend_config(
                    &state,
                    kind,
                    &key_for_change,
                    Some(SessionSettingValue::Bool(el.checked())),
                );
            };
            view! {
                <label class="settings-toggle">
                    <input type="checkbox" prop:checked=current on:change=on_change />
                    <span class="settings-toggle-slider"></span>
                </label>
            }
            .into_any()
        }
        BackendConfigFieldType::Integer {
            min,
            max,
            step,
            default,
        } => {
            let current = match values.0.get(&key).or_else(|| native.0.get(&key)) {
                Some(SessionSettingValue::Integer(value)) => *value,
                _ => *default,
            };
            let (min, max, step) = (*min, *max, *step);
            let state = state.clone();
            let key_for_change = key.clone();
            let on_change = move |ev: web_sys::Event| {
                let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
                if let Ok(parsed) = el.value().parse::<i64>() {
                    let clamped = parsed.clamp(min, max);
                    update_backend_config(
                        &state,
                        kind,
                        &key_for_change,
                        Some(SessionSettingValue::Integer(clamped)),
                    );
                }
            };
            view! {
                <input
                    type="number"
                    class="settings-input settings-backend-config-input"
                    prop:value=move || current.to_string()
                    min=min.to_string()
                    max=max.to_string()
                    step=step.to_string()
                    autocomplete="off"
                    on:change=on_change
                />
            }
            .into_any()
        }
    };

    let field_class = if primary {
        "settings-backend-config-field settings-backend-config-field-primary"
    } else {
        "settings-backend-config-field"
    };
    let caption = backend_config_field_caption(values.0.contains_key(&key), native.0.get(&key));
    view! {
        <div class=field_class>
            <span class="settings-tier-select-label">{field.label.clone()}</span>
            {control}
            {caption}
            {description
                .map(|text| view! { <p class="settings-description">{text}</p> })}
        </div>
    }
    .into_any()
}

/// Human-readable rendering of a native snapshot value, or `None` when there is
/// nothing meaningful to show (empty string or an explicit backend `Null`).
fn native_value_display(value: &SessionSettingValue) -> Option<String> {
    match value {
        SessionSettingValue::String(s) if !s.is_empty() => Some(s.clone()),
        SessionSettingValue::String(_) => None,
        SessionSettingValue::Bool(b) => Some(if *b { "On" } else { "Off" }.to_owned()),
        SessionSettingValue::Integer(i) => Some(i.to_string()),
        SessionSettingValue::Null => None,
    }
}

/// The provenance caption under a backend-config control: whether the shown
/// value is an explicit Tyde override (and what backend value it diverges from)
/// or the backend's own current value. Purely derived from server-provided
/// override + snapshot data — no inference.
fn backend_config_field_caption(
    override_present: bool,
    native: Option<&SessionSettingValue>,
) -> Option<AnyView> {
    let native_str = native.and_then(native_value_display);
    match (override_present, native_str) {
        (true, Some(backend)) => Some(
            view! {
                <div class="settings-backend-config-status">
                    <span class="settings-config-override-badge">"Tyde override"</span>
                    <span class="settings-config-native-value">
                        {format!("backend: {backend}")}
                    </span>
                </div>
            }
            .into_any(),
        ),
        (true, None) => Some(
            view! {
                <div class="settings-backend-config-status">
                    <span class="settings-config-override-badge">"Tyde override"</span>
                </div>
            }
            .into_any(),
        ),
        (false, Some(_)) => Some(
            view! {
                <div class="settings-backend-config-status">
                    <span class="settings-config-native-value">"From backend"</span>
                </div>
            }
            .into_any(),
        ),
        (false, None) => None,
    }
}

fn string_value(values: &BackendConfigValues, key: &str) -> Option<String> {
    match values.0.get(key) {
        Some(SessionSettingValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

/// Commit a text/secret field edit: a trimmed-empty value clears the key.
fn commit_text_value(state: &AppState, kind: BackendKind, key: &str, value: String) {
    let update = if value.trim().is_empty() {
        None
    } else {
        Some(SessionSettingValue::String(value))
    };
    update_backend_config(state, kind, key, update);
}

/// Persist a single backend-config field change. Only the edited key is sent;
/// the server merges it into the stored config and preserves every sibling key
/// (see `HostSettingValue::BackendConfig`). Clearing a field sends an explicit
/// `SessionSettingValue::Null` for that key — never omission — so the server can
/// tell "clear this one field" apart from "leave it untouched".
fn update_backend_config(
    state: &AppState,
    kind: BackendKind,
    key: &str,
    value: Option<SessionSettingValue>,
) {
    let mut values = BackendConfigValues::default();
    values
        .0
        .insert(key.to_owned(), value.unwrap_or(SessionSettingValue::Null));
    send_host_setting(
        state,
        HostSettingValue::BackendConfig {
            backend: kind,
            values,
        },
    );
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

/// Mobile pairing / broker settings. Two host-scoped settings live here:
///   * `enable_mobile_connections` — master kill switch.
///   * `mobile_broker_url` — optional override for the public MQTT
///     broker the host uses for the relay path. The default is a free
///     public broker (`wss://broker.emqx.io:8084/mqtt`); the user can
///     point at their own broker by overriding here. Empty input
///     resets to "use server default" (None on the wire).
///
/// The Tyde end-to-end encryption layer (paired session keys) sits on
/// top of MQTT and is independent of the broker chosen. The warning
/// block makes that contract explicit and reminds the user that
/// metadata (their IP, message timing, topic names) is still visible
/// to whoever runs the broker.
///
/// Frontend never claims to operate the broker — copy uses "Public
/// MQTT broker" / "Broker URL" wording. "Tyde broker" is forbidden:
/// Tyde is the client, not the operator of `broker.emqx.io`.
#[component]
fn MobileTab() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_enabled_checked = state.clone();
    let state_for_enabled_disabled = state.clone();
    let state_for_broker_value = state.clone();
    let state_for_broker_disabled = state.clone();
    let state_for_broker_commit = state.clone();
    let state_for_broker_keydown = state.clone();
    let state_for_broker_reset = state.clone();
    let state_for_pairing_lookup = state.clone();
    let state_for_offer_lookup = state.clone();
    let state_for_start_pending = state.clone();
    let state_for_start_click = state.clone();
    let state_for_cancel_click = state.clone();

    // Inline error surfaced when the user types something the server
    // would reject. Cleared when the user types again, when the field
    // is reset via "Use default", or when a valid URL commits.
    let broker_error: RwSignal<Option<String>> = RwSignal::new(None);
    // Inline error surfaced when MobilePairingStart fails locally
    // (e.g. no host stream). Server-side failures land via
    // `MobileAccessState::Failed` instead and render via
    // `pairing_status_line`.
    let pairing_error: RwSignal<Option<String>> = RwSignal::new(None);

    let enabled_checked = move || {
        state_for_enabled_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.enable_mobile_connections)
    };
    let enabled_disabled = move || {
        state_for_enabled_disabled
            .selected_host_settings()
            .is_none()
    };
    let on_enabled_toggle = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            send_host_setting(
                &state,
                HostSettingValue::EnableMobileConnections {
                    enabled: input.checked(),
                },
            );
        }
    };

    // The text field always reflects the current override. Empty
    // input commits `None`, which the server resolves to its built-in
    // default.
    let broker_value = move || {
        state_for_broker_value
            .selected_host_settings()
            .and_then(|settings| settings.mobile_broker_url)
            .map(|url| url.as_str().to_owned())
            .unwrap_or_default()
    };
    let broker_disabled = {
        let state = state_for_broker_disabled.clone();
        move || state.selected_host_settings().is_none()
    };
    let broker_disabled_for_input = broker_disabled.clone();
    let broker_disabled_for_button = broker_disabled.clone();

    // Validate + send. Used by both `change` (blur) and Enter so the
    // two code paths can't drift. Returns the input element so the
    // caller can still touch the DOM if needed (none currently do).
    let commit_broker = move |state: &AppState, raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            broker_error.set(None);
            send_host_setting(
                state,
                HostSettingValue::MobileBrokerUrl { broker_url: None },
            );
            return;
        }
        if let Err(message) = validate_broker_url_input(trimmed) {
            broker_error.set(Some(message.to_owned()));
            return;
        }
        match BrokerUrl::new(trimmed.to_owned()) {
            Ok(url) => {
                broker_error.set(None);
                send_host_setting(
                    state,
                    HostSettingValue::MobileBrokerUrl {
                        broker_url: Some(url),
                    },
                );
            }
            Err(error) => {
                log::error!("invalid broker URL {trimmed:?}: {error}");
                broker_error.set(Some(error.to_string()));
            }
        }
    };

    // `commit_broker` only captures `Copy` handles (RwSignal), so we
    // can hand it to both event closures by value without an Rc.
    let on_broker_commit = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let input: web_sys::HtmlInputElement = target.unchecked_into();
        let raw = input.value();
        commit_broker(&state_for_broker_commit, &raw);
    };
    let on_broker_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() != "Enter" {
            return;
        }
        ev.prevent_default();
        let Some(target) = ev.target() else {
            return;
        };
        let Ok(input) = target.dyn_into::<web_sys::HtmlInputElement>() else {
            return;
        };
        let raw = input.value();
        commit_broker(&state_for_broker_keydown, &raw);
    };
    // Typing clears any prior error so the user isn't yelled at while
    // they're still editing.
    let on_broker_input = move |_: web_sys::Event| {
        broker_error.set(None);
    };
    let on_broker_reset = move |_: web_sys::MouseEvent| {
        broker_error.set(None);
        send_host_setting(
            &state_for_broker_reset,
            HostSettingValue::MobileBrokerUrl { broker_url: None },
        );
    };

    // ---- Pairing section reactive lookups ----
    // Each closure looks up the slice of state for the *currently
    // selected* host. The host can change without remounting the tab
    // (e.g. via the Hosts surface), so each lookup re-reads
    // `selected_host_id` on every tracked invocation.

    let mobile_state_for_host = move || -> Option<MobileAccessStatePayload> {
        let host_id = state_for_pairing_lookup.selected_host_id.get()?;
        state_for_pairing_lookup
            .mobile_access_state
            .with(|m| m.get(&host_id).cloned())
    };
    let mobile_offer_for_host = move || -> Option<MobilePairingOfferPayload> {
        let host_id = state_for_offer_lookup.selected_host_id.get()?;
        state_for_offer_lookup
            .mobile_pairing_offer
            .with(|m| m.get(&host_id).cloned())
    };
    let mobile_start_pending = move || -> bool {
        let host_id = state_for_start_pending.selected_host_id.get();
        let Some(host_id) = host_id else {
            return false;
        };
        state_for_start_pending
            .mobile_pairing_start_pending
            .with(|set| set.contains(&host_id))
    };
    let paired_devices = move || -> Vec<protocol::MobileDeviceSummary> {
        mobile_state_for_host()
            .map(|state| state.paired_devices)
            .unwrap_or_default()
    };

    let on_start_pairing_click = move |_: web_sys::MouseEvent| {
        pairing_error.set(None);
        let Some((host_id, host_stream)) = state_for_start_click.selected_host_stream_untracked()
        else {
            pairing_error.set(Some("No host selected.".to_owned()));
            return;
        };
        // Optimistically gate the button so a double-click doesn't fire
        // two Start frames. Cleared when MobileAccessState/Offer lands
        // or on a local send error.
        let host_id_for_gate = host_id.clone();
        state_for_start_click
            .mobile_pairing_start_pending
            .update(|set| {
                set.insert(host_id_for_gate);
            });
        let state_for_async = state_for_start_click.clone();
        spawn_local(async move {
            if let Err(err) = mobile_pairing_start(&host_id, host_stream).await {
                log::error!("mobile_pairing_start send failed: {err}");
                let host_id_for_clear = host_id.clone();
                state_for_async.mobile_pairing_start_pending.update(|set| {
                    set.remove(&host_id_for_clear);
                });
                pairing_error.set(Some(format!("Could not start pairing: {err}")));
            }
        });
    };

    let on_cancel_pairing_click = move |_: web_sys::MouseEvent| {
        let Some(offer) = mobile_offer_for_host() else {
            // No active offer — nothing to cancel. Could happen if the
            // server already pushed an Expired/Cancelled state between
            // render and click.
            return;
        };
        let Some((host_id, host_stream)) = state_for_cancel_click.selected_host_stream_untracked()
        else {
            pairing_error.set(Some("No host selected.".to_owned()));
            return;
        };
        let offer_id: MobilePairingOfferId = offer.offer_id.clone();
        spawn_local(async move {
            if let Err(err) = mobile_pairing_cancel(&host_id, host_stream, offer_id).await {
                log::error!("mobile_pairing_cancel send failed: {err}");
                pairing_error.set(Some(format!("Could not cancel pairing: {err}")));
            }
        });
    };

    // Hide-or-disable rules:
    // * Start button is *visible* when the host has settings loaded
    //   AND mobile is enabled AND broker is Online AND we're not
    //   already mid-pairing (Active offer or a Start in flight).
    // * Cancel button is visible when we have an Active offer.
    let pairing_phase = move || -> Option<MobilePairingState> {
        mobile_state_for_host().map(|state| state.pairing)
    };
    let broker_phase = move || -> Option<MobileBrokerStatus> {
        mobile_state_for_host().map(|state| state.broker_status)
    };
    let state_for_can_start_settings = state.clone();
    let can_start_pairing = move || -> bool {
        let Some(host_id) = state_for_can_start_settings.selected_host_id.get() else {
            return false;
        };
        let enabled = state_for_can_start_settings
            .host_settings_by_host
            .with(|m| {
                m.get(&host_id)
                    .map(|settings| settings.enable_mobile_connections)
                    .unwrap_or(false)
            });
        if !enabled {
            return false;
        }
        let online = matches!(broker_phase(), Some(MobileBrokerStatus::Online { .. }));
        let in_flight = mobile_start_pending()
            || matches!(pairing_phase(), Some(MobilePairingState::Active { .. }));
        online && !in_flight
    };

    view! {
        <h2 class="settings-panel-title">"Mobile"</h2>

        <p class="settings-description settings-panel-intro">
            "Pair the Tyde mobile app with this host. The pairing QR carries the broker URL the mobile app should use, so the mobile app does not need any preconfigured Tyde infrastructure."
        </p>

        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Enable mobile connections"</label>
                    <p class="settings-description">
                        "When enabled, this host can accept pairing requests from the Tyde mobile app and route mobile traffic over the broker below."
                    </p>
                </div>
                <label class="settings-toggle">
                    <input
                        type="checkbox"
                        prop:checked=enabled_checked
                        disabled=enabled_disabled
                        on:change=on_enabled_toggle
                    />
                    <span class="settings-toggle-slider"></span>
                </label>
            </div>
        </div>

        <div class="settings-field settings-mobile-pairing">
            <label class="settings-label">"Pair a mobile device"</label>
            <p class="settings-description">
                "Start a pairing session, then scan the QR code with the Tyde mobile app. The QR carries the broker URL and a one-shot pre-shared key; the pairing session expires after a couple of minutes."
            </p>
            // Broker status pill — surfaces broker_status from the
            // MobileAccessState snapshot. Keeps the user informed when
            // the broker is offline / errored so a missing Start button
            // is self-explanatory.
            {move || broker_phase().map(|status| view! {
                <p class=format!("settings-mobile-pairing-broker settings-mobile-pairing-broker-{}", broker_status_slug(&status))>
                    {broker_status_line(&status)}
                </p>
            })}
            {move || pairing_phase().and_then(|phase| pairing_status_line(&phase)).map(|line| view! {
                <p class="settings-mobile-pairing-status" role="status">{line}</p>
            })}
            {move || mobile_offer_for_host().map(|offer| {
                let qr_uri = offer.qr_uri.0.clone();
                let qr_svg = render_pairing_qr_svg(&qr_uri).unwrap_or_else(|| {
                    "<p>QR rendering failed; use the URI below.</p>".to_owned()
                });
                let expires_in = expires_in_seconds(offer.expires_at_ms);
                let expires_label = match expires_in {
                    None => "expiry unknown".to_owned(),
                    Some(0) => "expiring now".to_owned(),
                    Some(seconds) => format!("expires in {seconds}s"),
                };
                let qr_uri_for_text = qr_uri.clone();
                view! {
                    <div class="settings-mobile-pairing-active" role="region" aria-label="Active pairing QR">
                        <div
                            class="settings-mobile-pairing-qr"
                            // SAFETY: `qr_svg` is produced by `qrcodegen`
                            // which emits a fixed structural SVG with no
                            // attacker-controlled attribute content; the
                            // QR modules are rendered as <rect> elements
                            // with the QR module bitmap, not user input.
                            inner_html=qr_svg
                        />
                        <div class="settings-mobile-pairing-active-meta">
                            <p class="settings-description settings-mobile-pairing-expires">
                                {expires_label}
                            </p>
                            <details class="settings-mobile-pairing-fallback">
                                <summary>"Show pairing URI"</summary>
                                <p class="settings-description">
                                    "If your mobile device can't scan the QR, paste this URI into the Tyde mobile app's pairing screen instead. Treat it like a one-shot password — anyone with the URI before it expires can pair as a device on this host."
                                </p>
                                <textarea
                                    class="settings-input settings-mobile-pairing-uri"
                                    readonly=true
                                    spellcheck="false"
                                    autocapitalize="none"
                                    autocomplete="off"
                                    aria-label="Pairing URI"
                                    rows="3"
                                    prop:value=qr_uri_for_text
                                />
                            </details>
                            <button
                                type="button"
                                class="filter-toggle settings-mobile-pairing-cancel"
                                on:click=on_cancel_pairing_click.clone()
                            >
                                "Cancel pairing"
                            </button>
                        </div>
                    </div>
                }
            })}
            {move || {
                // The Start button stays in the DOM at all times for
                // discoverability — disabling instead of hiding keeps
                // the affordance visible while the user fixes the
                // precondition (enable / fix broker / wait for an
                // active offer to settle).
                let can = can_start_pairing();
                let title = if can {
                    "Start a fresh pairing session"
                } else if mobile_start_pending() {
                    "Starting pairing…"
                } else if matches!(pairing_phase(), Some(MobilePairingState::Active { .. })) {
                    "A pairing session is already active — cancel it first"
                } else if !matches!(broker_phase(), Some(MobileBrokerStatus::Online { .. })) {
                    "Broker is not online yet"
                } else {
                    "Enable mobile connections to pair a device"
                };
                view! {
                    <button
                        type="button"
                        class="filter-toggle settings-mobile-pairing-start"
                        disabled=!can
                        title=title
                        on:click=on_start_pairing_click.clone()
                    >
                        "Start pairing"
                    </button>
                }
            }}
            {move || pairing_error.get().map(|message| view! {
                <p class="settings-mobile-broker-error" role="alert">{message}</p>
            })}
            {move || {
                let devices = paired_devices();
                (!devices.is_empty()).then(|| view! {
                    <div class="settings-mobile-pairing-devices">
                        <p class="settings-mobile-pairing-devices-heading">"Paired devices"</p>
                        <p class="settings-description settings-mobile-pairing-devices-description">
                            "Remove stale test pairings here. Removed devices must scan a fresh QR before they can connect again."
                        </p>
                        <ul class="settings-mobile-pairing-devices-list">
                            {devices.into_iter().map(|device| {
                                let state_label = match device.state {
                                    MobileDeviceState::Connected => "connected",
                                    MobileDeviceState::Paired => "offline",
                                    MobileDeviceState::Revoked => "revoked",
                                };
                                let state_class = format!("settings-mobile-pairing-device-state settings-mobile-pairing-device-state-{state_label}");
                                let device_label = device.label.clone();
                                let device_id = device.device_id.clone();
                                let state_for_remove = state.clone();
                                let pairing_error_for_remove = pairing_error;
                                view! {
                                    <li class="settings-mobile-pairing-device">
                                        <div class="settings-mobile-pairing-device-main">
                                            <span class="settings-mobile-pairing-device-label">{device_label.clone()}</span>
                                            <span class=state_class>{state_label}</span>
                                        </div>
                                        <button
                                            type="button"
                                            class="filter-toggle settings-mobile-pairing-device-remove"
                                            title="Remove this paired mobile device"
                                            on:click=move |_: web_sys::MouseEvent| {
                                                let state = state_for_remove.clone();
                                                let device_id = device_id.clone();
                                                let device_label = device_label.clone();
                                                spawn_local(async move {
                                                    let message = format!(
                                                        "Remove mobile device \"{device_label}\"? It will need to scan a fresh pairing QR before it can connect again."
                                                    );
                                                    if !crate::bridge::confirm_dialog("Remove mobile pairing", &message).await {
                                                        return;
                                                    }
                                                    let Some((host_id, host_stream)) = state.selected_host_stream_untracked() else {
                                                        pairing_error_for_remove.set(Some("No host selected.".to_owned()));
                                                        return;
                                                    };
                                                    if let Err(err) = mobile_device_revoke(&host_id, host_stream, device_id).await {
                                                        log::error!("mobile_device_revoke send failed: {err}");
                                                        pairing_error_for_remove.set(Some(format!("Could not remove mobile device: {err}")));
                                                    }
                                                });
                                            }
                                        >
                                            "Remove"
                                        </button>
                                    </li>
                                }
                            }).collect::<Vec<_>>()}
                        </ul>
                    </div>
                })
            }}
        </div>

        <div class="settings-field">
            <label class="settings-label">"Broker URL"</label>
            <p class="settings-description">
                "Public MQTT broker used to ferry pairing offers and encrypted traffic between this host and the mobile app. Leave blank to use the host default — the pairing QR will carry whichever broker URL is active, so the mobile app does not need to be preconfigured."
            </p>
            <div class="settings-mobile-broker-row">
                <input
                    type="text"
                    class="settings-input settings-mobile-broker-input"
                    prop:value=broker_value
                    placeholder=DEFAULT_MOBILE_MQTT_BROKER_URL
                    disabled=broker_disabled_for_input
                    autocapitalize="none"
                    autocomplete="off"
                    spellcheck="false"
                    aria-label="Broker URL"
                    aria-invalid=move || if broker_error.get().is_some() { "true" } else { "false" }
                    on:input=on_broker_input
                    on:change=on_broker_commit
                    on:keydown=on_broker_keydown
                />
                <button
                    type="button"
                    class="filter-toggle settings-mobile-broker-reset"
                    disabled=broker_disabled_for_button
                    title="Use the host default broker"
                    on:click=on_broker_reset
                >
                    "Use default"
                </button>
            </div>
            {move || broker_error.get().map(|message| view! {
                <p class="settings-mobile-broker-error" role="alert">{message}</p>
            })}
        </div>

        <div class="settings-mobile-warning" role="note">
            <p class="settings-mobile-warning-heading">
                "Public broker — encrypted contents, visible metadata"
            </p>
            <p class="settings-description">
                "The broker is run by a third party and is untrusted. Tyde end-to-end encrypts every message between this host and your paired mobile devices, so the broker operator cannot read your chats, files, or commands. However, metadata like your IP address, connection timing, topic names, and message sizes is visible to the broker operator. Point this at your own MQTT broker if you need to hide that metadata too."
            </p>
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
                    let unavailable = info.status == BackendSetupStatus::Unavailable;
                    // The server explains *why* a backend probe failed; show it
                    // verbatim rather than a generic "not installed".
                    let diagnostic_message = info.diagnostic.as_ref().map(|d| d.message.clone());
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
                            {diagnostic_message.map(|message| view! {
                                <p class="settings-backend-note settings-backend-note-warning">
                                    {message}
                                </p>
                            })}
                            {unavailable.then(|| {
                                view! {
                                    <p class="settings-backend-note">
                                        "This backend is currently unavailable on the selected host. Resolve the issue above, then it can be used for new chats."
                                    </p>
                                }
                            })}
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
        Some(BackendSetupStatus::Unavailable) => "Unavailable",
        Some(BackendSetupStatus::Unsupported) => "Unsupported",
        None => "Checking…",
    }
}

fn backend_setup_status_class(info: Option<&BackendSetupInfo>) -> &'static str {
    match info.map(|info| info.status) {
        Some(BackendSetupStatus::Installed) => "settings-status-chip installed",
        Some(BackendSetupStatus::NotInstalled) => "settings-status-chip missing",
        Some(BackendSetupStatus::Unavailable) => "settings-status-chip unavailable",
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

fn all_backends() -> [BackendKind; 6] {
    [
        BackendKind::Tycode,
        BackendKind::Kiro,
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Antigravity,
        BackendKind::Hermes,
    ]
}

/// Mirror the server's reserved launch-profile id rule (`store::settings`):
/// `<backend>:default` ids belong to the built-in default profiles and are
/// rejected by the settings store, so reject them in the editor too rather than
/// letting the save fail silently on the wire.
fn is_reserved_launch_profile_id(id: &str) -> bool {
    all_backends()
        .into_iter()
        .any(|kind| id == format!("{}:default", backend_value(kind)))
}

fn parse_backend_kind(value: &str) -> Option<BackendKind> {
    match value {
        "tycode" => Some(BackendKind::Tycode),
        "kiro" => Some(BackendKind::Kiro),
        "claude" => Some(BackendKind::Claude),
        "codex" => Some(BackendKind::Codex),
        "antigravity" => Some(BackendKind::Antigravity),
        "hermes" => Some(BackendKind::Hermes),
        _ => None,
    }
}

fn backend_value(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "tycode",
        BackendKind::Kiro => "kiro",
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Antigravity => "antigravity",
        BackendKind::Hermes => "hermes",
    }
}

fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Antigravity => "Antigravity",
        BackendKind::Hermes => "Hermes",
    }
}

fn backend_description(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode subprocess backend",
        BackendKind::Kiro => "Kiro ACP backend",
        BackendKind::Claude => "Anthropic Claude — advanced reasoning and coding",
        BackendKind::Codex => "OpenAI Codex — code completion and generation",
        BackendKind::Antigravity => "Google Antigravity CLI — agentic coding assistant",
        BackendKind::Hermes => "Hermes — native JSON-RPC agent backend",
    }
}

fn backend_badge_class(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "backend-badge tycode",
        BackendKind::Kiro => "backend-badge kiro",
        BackendKind::Claude => "backend-badge claude",
        BackendKind::Codex => "backend-badge codex",
        BackendKind::Antigravity => "backend-badge antigravity",
        BackendKind::Hermes => "backend-badge hermes",
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
    let is_default_agent = !form.is_new && form.id.0.as_str() == "tyde-default";

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

                {if is_default_agent {
                    view! {
                        <>
                            <div class="settings-form-label">
                                <span>"Skills"</span>
                                <div class="settings-description">"All host skills"</div>
                            </div>
                            <div class="settings-form-label">
                                <span>"MCP Servers"</span>
                                <div class="settings-description">"All configured servers"</div>
                            </div>
                        </>
                    }.into_any()
                } else {
                    view! {
                        <>
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
                        </>
                    }.into_any()
                }}

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
            "Configure MCP servers (HTTP or Stdio). Names \"tyde-debug\", \"tyde-agent-control\", and \"tyde-review-feedback\" are reserved."
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::AppState;
    use leptos::mount::mount_to;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{HtmlElement, HtmlInputElement, HtmlOptionElement, HtmlSelectElement};

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 1024px; height: 768px;",
            )
            .unwrap();
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

    /// Find the syntax-theme `<select>` by looking for one whose options
    /// contain known bundled theme names. Resilient to ordering changes
    /// among the page's other dropdowns (font family, model, etc.).
    fn find_syntax_theme_select(container: &HtmlElement) -> Option<HtmlSelectElement> {
        let nodes = container.query_selector_all("select").ok()?;
        for i in 0..nodes.length() {
            let node = nodes.item(i)?;
            let select: HtmlSelectElement = node.dyn_into().ok()?;
            for j in 0..select.length() {
                let Some(option_node) = select.item(j) else {
                    continue;
                };
                let Ok(option) = option_node.dyn_into::<HtmlOptionElement>() else {
                    continue;
                };
                if option.value() == "Catppuccin Mocha" {
                    return Some(select);
                }
            }
        }
        None
    }

    /// The Settings → Appearance pane must expose a syntax-theme picker
    /// with the popular bundled themes available. If this regresses we
    /// either lost the picker UI entirely or the bundled theme set was
    /// silently shrunk — either way the user can no longer change colors
    /// via the documented path.
    ///
    /// Asserts on the user-perceivable surface: a dropdown exists,
    /// contains theme names users recognize, and has enough breadth to
    /// be useful. Doesn't assert on internal class names of the wrapper.
    #[wasm_bindgen_test]
    async fn theme_dropdown_lists_popular_themes() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            // SettingsPanel is gated on settings_open; opening it is the
            // documented user gesture (Cmd+, / Ctrl+,).
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;

        let select =
            find_syntax_theme_select(&container).expect("syntax theme dropdown should be present");

        let options: Vec<String> = (0..select.length())
            .filter_map(|i| {
                select
                    .item(i)
                    .and_then(|n| n.dyn_into::<HtmlOptionElement>().ok())
                    .map(|o| o.value())
            })
            .collect();

        // Popular themes that must remain bundled. Loss of any of these
        // is a regression worth surfacing — they're what users see and
        // recognize.
        for expected in [
            "Catppuccin Mocha",
            "Dracula",
            "Nord",
            "GitHub",
            "Monokai Extended",
        ] {
            assert!(
                options.iter().any(|o| o == expected),
                "expected `{expected}` in syntax theme dropdown; got {options:?}"
            );
        }

        // Sanity: the dropdown should be substantively populated, not a
        // single fallback theme. Threshold is generous so adding/removing
        // one theme doesn't falsify the assertion.
        assert!(
            options.len() >= 20,
            "expected >=20 themes in dropdown, got {}: {options:?}",
            options.len()
        );
    }

    // ---- Mobile tab ----

    /// Install the host-settings snapshot the Mobile tab depends on,
    /// taking ownership of the provided `AppState`. Mirrors the data
    /// path the dispatcher uses when it receives a real
    /// `HostSettingsPayload` from the server.
    fn install_mobile_host_settings(state: &AppState, broker_url: Option<&str>, enabled: bool) {
        let host_id = "host-mobile".to_owned();
        state.selected_host_id.set(Some(host_id.clone()));
        state.host_streams.update(|m| {
            m.insert(
                host_id.clone(),
                protocol::StreamPath(format!("/host/{host_id}")),
            );
        });
        state.connection_statuses.update(|m| {
            m.insert(host_id.clone(), crate::state::ConnectionStatus::Connected);
        });
        state.host_settings_by_host.update(|m| {
            m.insert(
                host_id,
                protocol::HostSettings {
                    enabled_backends: vec![protocol::BackendKind::Claude],
                    default_backend: Some(protocol::BackendKind::Claude),
                    enable_mobile_connections: enabled,
                    mobile_broker_url: broker_url
                        .map(|s| protocol::BrokerUrl::new(s.to_owned()).expect("broker url")),
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    code_intel: Default::default(),
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: Vec::new(),
                },
            );
        });
    }

    fn click_tab(container: &HtmlElement, label: &str) {
        // Walk every button rendered anywhere inside the settings UI
        // and pick the one whose visible text matches the tab label.
        let nodes = container
            .query_selector_all("button")
            .expect("settings buttons");
        let mut observed: Vec<String> = Vec::new();
        for i in 0..nodes.length() {
            let Some(node) = nodes.item(i) else { continue };
            let Ok(el) = node.dyn_into::<HtmlElement>() else {
                continue;
            };
            let text = el
                .text_content()
                .map(|s| s.trim().to_owned())
                .unwrap_or_default();
            if text == label {
                el.click();
                return;
            }
            observed.push(text);
        }
        panic!("settings tab labelled {label:?} not found among {observed:?}");
    }

    fn broker_input(container: &HtmlElement) -> web_sys::HtmlInputElement {
        container
            .query_selector(".settings-mobile-broker-input")
            .unwrap()
            .expect("broker URL input must render on the Mobile tab")
            .dyn_into()
            .unwrap()
    }

    /// When the host has no `mobile_broker_url` override, the broker
    /// URL input must render empty but display the
    /// `wss://broker.emqx.io:8084/mqtt` placeholder so the user can see
    /// what the host default resolves to. This is the foundation of
    /// the "QR carries the broker URL even when the user hasn't set
    /// one" contract — the placeholder is purely informational so
    /// the user knows what they're opting into.
    #[wasm_bindgen_test]
    async fn mobile_tab_broker_default_placeholder_when_no_override() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, false);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        let input = broker_input(&container);
        assert_eq!(
            input.value(),
            "",
            "broker URL input must be empty when no host override exists"
        );
        assert_eq!(
            input.get_attribute("placeholder").as_deref(),
            Some(DEFAULT_MOBILE_MQTT_BROKER_URL),
            "broker URL placeholder must surface the public default"
        );
    }

    /// When the host has an explicit override, the broker URL input
    /// must display it (not the placeholder).
    #[wasm_bindgen_test]
    async fn mobile_tab_broker_input_reflects_host_override() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, Some("mqtts://mybroker.example/relay"), true);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        let input = broker_input(&container);
        assert_eq!(
            input.value(),
            "mqtts://mybroker.example/relay",
            "broker URL input must reflect the host override exactly"
        );
    }

    /// The warning copy must call out: (1) the broker is public /
    /// untrusted, (2) Tyde contents are encrypted, (3) metadata may
    /// be visible. Tests on user-perceived text content, not on a
    /// CSS class — if a future refactor moves the warning into a
    /// different element the assertion still passes as long as the
    /// content is reachable.
    #[wasm_bindgen_test]
    async fn mobile_tab_warning_covers_public_encrypted_metadata() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, false);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        let text = container.text_content().unwrap_or_default().to_lowercase();
        assert!(
            text.contains("public"),
            "mobile warning must call the broker public; got: {text:?}"
        );
        assert!(
            text.contains("end-to-end encrypt") || text.contains("encrypt"),
            "mobile warning must mention encryption; got: {text:?}"
        );
        assert!(
            text.contains("metadata"),
            "mobile warning must call out metadata leakage; got: {text:?}"
        );
        // Inverse: must NOT imply Tyde runs the broker.
        assert!(
            !text.contains("tyde broker"),
            "mobile copy must not say 'Tyde broker' (we are the client, not the operator); got: {text:?}"
        );
        // The "untrusted" framing should be present in some form.
        assert!(
            text.contains("untrusted") || text.contains("third party"),
            "mobile warning must frame the broker as untrusted/third-party; got: {text:?}"
        );
    }

    /// The "Use default" button must always be present alongside the
    /// broker URL input so the user can revert to the host default
    /// without manually clearing the field.
    #[wasm_bindgen_test]
    async fn mobile_tab_has_use_default_button() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, Some("mqtts://override/relay"), true);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        let buttons = container.query_selector_all("button").unwrap();
        let mut found = false;
        for i in 0..buttons.length() {
            let Some(node) = buttons.item(i) else {
                continue;
            };
            let Ok(el) = node.dyn_into::<HtmlElement>() else {
                continue;
            };
            if el.text_content().as_deref().map(str::trim) == Some("Use default") {
                found = true;
                break;
            }
        }
        assert!(
            found,
            "Mobile tab must surface a 'Use default' button to revert the broker override"
        );
    }

    /// The tab nav must include a "Mobile" entry; the previous tab
    /// list omitted it entirely. This is the discoverability gate —
    /// without the nav button the page is unreachable.
    #[wasm_bindgen_test]
    async fn settings_nav_includes_mobile_tab() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;

        let buttons = container.query_selector_all("button").unwrap();
        let mut found = false;
        for i in 0..buttons.length() {
            let Some(node) = buttons.item(i) else {
                continue;
            };
            let Ok(el) = node.dyn_into::<HtmlElement>() else {
                continue;
            };
            if el.text_content().as_deref().map(str::trim) == Some("Mobile") {
                found = true;
                break;
            }
        }
        assert!(
            found,
            "Settings nav must include a 'Mobile' tab so the broker URL surface is reachable"
        );
    }

    // ---- Mobile tab: send-frame behaviour + inline validation ----

    /// Stub `window.__TAURI__.core.invoke` to record every call into
    /// `window.__test_send_calls = [[cmd, JSON.stringify(args)], …]`
    /// and resolve immediately. Mirrors the pattern used by
    /// `teams_panel::wasm_tests::install_send_stub`.
    fn install_settings_send_stub() -> js_sys::Array {
        let code = r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    if (cmd === "plugin:dialog|message") {
                        return Promise.resolve("Ok");
                    }
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
                return window.__test_send_calls;
            })();
        "#;
        let calls = js_sys::eval(code).expect("install tauri stub");
        calls.dyn_into::<js_sys::Array>().expect("array")
    }

    /// Find every SetSetting frame recorded against the send-stub and
    /// return the parsed `setting` JSON for each. Narrowed to
    /// SetSetting so we can ignore handshake/other invokes.
    fn recorded_set_setting_payloads(calls: &js_sys::Array) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for entry in calls.iter() {
            let arr = match entry.dyn_into::<js_sys::Array>() {
                Ok(arr) => arr,
                Err(_) => continue,
            };
            if arr.get(0).as_string().as_deref() != Some("send_host_line") {
                continue;
            }
            let Some(args_json) = arr.get(1).as_string() else {
                continue;
            };
            let args: serde_json::Value = match serde_json::from_str(&args_json) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(line) = args.get("line").and_then(|v| v.as_str()) else {
                continue;
            };
            let envelope: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if envelope.get("kind").and_then(|v| v.as_str()) != Some("set_setting") {
                continue;
            }
            if let Some(setting) = envelope
                .get("payload")
                .and_then(|p| p.get("setting"))
                .cloned()
            {
                out.push(setting);
            }
        }
        out
    }

    /// Dispatch a synthetic DOM event on `input` from JS. `web_sys` in
    /// this feature set doesn't expose `KeyboardEventInit`, and
    /// `Event::new` does not carry the `key` property our handler
    /// reads — building the event in JS sidesteps both limitations.
    fn dispatch_event_from_js(input: &web_sys::HtmlInputElement, kind: &str, key: Option<&str>) {
        let id = format!("__tyde_dispatch_target_{kind}");
        input.set_id(&id);
        let key_part = key.map(|k| format!(", key: {k:?}")).unwrap_or_default();
        let code = format!(
            r#"
            (function() {{
                var el = document.getElementById({id:?});
                if (!el) {{ throw new Error('dispatch target not found'); }}
                var ev;
                if ({kind:?} === 'keydown') {{
                    ev = new KeyboardEvent('keydown', {{ bubbles: true, cancelable: true{key_part} }});
                }} else {{
                    ev = new Event({kind:?}, {{ bubbles: true, cancelable: true }});
                }}
                el.dispatchEvent(ev);
            }})();
            "#
        );
        js_sys::eval(&code).expect("dispatch event from JS");
    }

    fn dispatch_change(input: &web_sys::HtmlInputElement) {
        dispatch_event_from_js(input, "change", None);
    }

    fn dispatch_enter(input: &web_sys::HtmlInputElement) {
        dispatch_event_from_js(input, "keydown", Some("Enter"));
    }

    /// Pressing Enter on a valid override commits a `SetSetting` frame
    /// whose payload is `MobileBrokerUrl { broker_url: Some(...) }`.
    /// Load-bearing assertion that the typed-URL commit path actually
    /// reaches the wire.
    #[wasm_bindgen_test]
    async fn mobile_tab_enter_commits_valid_broker_url() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, true);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        let input = broker_input(&container);
        input.set_value("mqtts://override.example:8883");
        dispatch_enter(&input);
        for _ in 0..4 {
            next_tick().await;
        }

        let settings = recorded_set_setting_payloads(&calls);
        let mobile = settings
            .iter()
            .find(|s| s.get("kind").and_then(|k| k.as_str()) == Some("mobile_broker_url"))
            .expect("Enter on a valid broker URL must emit a MobileBrokerUrl SetSetting frame");
        let broker_url = mobile
            .get("broker_url")
            .and_then(|v| v.as_str())
            .expect("MobileBrokerUrl payload must carry the URL on commit");
        assert_eq!(broker_url, "mqtts://override.example:8883");
    }

    /// Clicking "Use default" commits `MobileBrokerUrl { broker_url:
    /// None }`. The server resolves None to the host's built-in
    /// default, so this is how the user reverts an override without
    /// manually clearing the field.
    #[wasm_bindgen_test]
    async fn mobile_tab_use_default_button_commits_none() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, Some("mqtts://override.example"), true);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        // Find the Use default button by text and click it.
        let buttons = container.query_selector_all("button").unwrap();
        let mut clicked = false;
        for i in 0..buttons.length() {
            let Some(node) = buttons.item(i) else {
                continue;
            };
            let Ok(el) = node.dyn_into::<HtmlElement>() else {
                continue;
            };
            if el.text_content().as_deref().map(str::trim) == Some("Use default") {
                el.click();
                clicked = true;
                break;
            }
        }
        assert!(clicked, "Use default button must be present and clickable");
        for _ in 0..4 {
            next_tick().await;
        }

        let settings = recorded_set_setting_payloads(&calls);
        let mobile = settings
            .iter()
            .find(|s| s.get("kind").and_then(|k| k.as_str()) == Some("mobile_broker_url"))
            .expect("Use default must emit a MobileBrokerUrl SetSetting frame");
        // `broker_url: None` is encoded as `Option::None`, which serde
        // serialises as absent OR explicit null depending on payload
        // attributes. Accept either to keep the test resilient.
        let broker_url = mobile.get("broker_url");
        match broker_url {
            None => {}
            Some(value) if value.is_null() => {}
            Some(value) => {
                panic!("Use default must clear the override (broker_url: None); got {value:?}")
            }
        }
    }

    /// Toggling the "Enable mobile connections" checkbox commits a
    /// `SetSetting` frame whose payload is
    /// `EnableMobileConnections { enabled }`. Without this assertion
    /// the toggle could silently become a no-op and nothing would
    /// notice.
    #[wasm_bindgen_test]
    async fn mobile_tab_enable_toggle_commits_setting() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, false);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        // The enable toggle is the only checkbox inside the Mobile tab.
        let toggles = container
            .query_selector_all("input[type='checkbox']")
            .unwrap();
        assert!(
            toggles.length() >= 1,
            "Mobile tab must render at least one checkbox"
        );
        let toggle: web_sys::HtmlInputElement = toggles
            .item(0)
            .unwrap()
            .dyn_into()
            .expect("checkbox element");
        toggle.set_checked(true);
        dispatch_change(&toggle);
        for _ in 0..4 {
            next_tick().await;
        }

        let settings = recorded_set_setting_payloads(&calls);
        let enable = settings
            .iter()
            .find(|s| s.get("kind").and_then(|k| k.as_str()) == Some("enable_mobile_connections"))
            .expect("Toggling Enable must emit an EnableMobileConnections SetSetting frame");
        assert_eq!(
            enable.get("enabled").and_then(|v| v.as_bool()),
            Some(true),
            "EnableMobileConnections payload must carry enabled=true after toggle on"
        );
    }

    /// Pressing Enter on an insecure-scheme URL must (a) NOT emit any
    /// SetSetting frame for the broker URL and (b) render an inline
    /// error message that mentions the scheme problem. This is the
    /// silent-failure regression guard the prior review called out.
    #[wasm_bindgen_test]
    async fn mobile_tab_invalid_url_shows_inline_error_and_suppresses_send() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, true);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        let input = broker_input(&container);
        input.set_value("mqtt://broker.example:1883");
        dispatch_enter(&input);
        for _ in 0..4 {
            next_tick().await;
        }

        // No MobileBrokerUrl SetSetting frame should have been sent.
        let settings = recorded_set_setting_payloads(&calls);
        assert!(
            settings
                .iter()
                .all(|s| s.get("kind").and_then(|k| k.as_str()) != Some("mobile_broker_url")),
            "Invalid broker URL must not be committed; recorded settings: {settings:?}"
        );

        // The inline error must be visible AND mention scheme/insecure
        // so the user can correct it without guessing.
        let error_el = container
            .query_selector(".settings-mobile-broker-error")
            .unwrap()
            .expect("Invalid broker URL must surface an inline error message");
        let error_text = error_el.text_content().unwrap_or_default().to_lowercase();
        assert!(
            error_text.contains("insecure")
                || error_text.contains("mqtts")
                || error_text.contains("scheme"),
            "Inline error must explain the scheme problem; got: {error_text:?}"
        );

        // aria-invalid must flip so screen readers announce the error.
        let aria_invalid = input.get_attribute("aria-invalid");
        assert_eq!(
            aria_invalid.as_deref(),
            Some("true"),
            "Broker URL input must set aria-invalid=true while showing the error"
        );
    }

    // ---- Mobile pairing section ----

    /// Seed an Online broker `MobileAccessState` snapshot under the
    /// installed host so the Start-pairing button can render enabled.
    fn install_online_broker_state(state: &AppState) {
        let url = BrokerUrl::new("mqtts://broker.test:8883").expect("broker url");
        let payload = protocol::MobileAccessStatePayload {
            broker_status: MobileBrokerStatus::Online { broker_url: url },
            pairing: MobilePairingState::Idle,
            paired_devices: Vec::new(),
        };
        state.mobile_access_state.update(|m| {
            m.insert("host-mobile".to_owned(), payload);
        });
    }

    /// Inject an active offer + matching MobileAccessState so the UI
    /// renders the QR + Cancel button without going through the
    /// server round-trip.
    fn install_active_offer(state: &AppState, offer_id: &str, qr_uri: &str, expires_at_ms: u64) {
        let offer = protocol::MobilePairingOfferPayload {
            offer_id: protocol::MobilePairingOfferId(offer_id.to_owned()),
            qr_uri: protocol::MobilePairingQrUri(qr_uri.to_owned()),
            expires_at_ms,
        };
        state.mobile_pairing_offer.update(|m| {
            m.insert("host-mobile".to_owned(), offer);
        });
        let url = BrokerUrl::new("mqtts://broker.test:8883").expect("broker url");
        let payload = protocol::MobileAccessStatePayload {
            broker_status: MobileBrokerStatus::Online { broker_url: url },
            pairing: MobilePairingState::Active {
                offer_id: protocol::MobilePairingOfferId(offer_id.to_owned()),
                expires_at_ms,
            },
            paired_devices: Vec::new(),
        };
        state.mobile_access_state.update(|m| {
            m.insert("host-mobile".to_owned(), payload);
        });
    }

    fn find_button_by_text(container: &HtmlElement, label: &str) -> Option<HtmlElement> {
        let buttons = container.query_selector_all("button").ok()?;
        for i in 0..buttons.length() {
            let node = buttons.item(i)?;
            let el = node.dyn_into::<HtmlElement>().ok()?;
            if el.text_content().as_deref().map(str::trim) == Some(label) {
                return Some(el);
            }
        }
        None
    }

    /// Find every Mobile* frame recorded against the send-stub and
    /// return the parsed envelope JSON (so we can assert on `kind`
    /// and `payload`).
    fn recorded_mobile_envelopes(calls: &js_sys::Array) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for entry in calls.iter() {
            let arr = match entry.dyn_into::<js_sys::Array>() {
                Ok(arr) => arr,
                Err(_) => continue,
            };
            if arr.get(0).as_string().as_deref() != Some("send_host_line") {
                continue;
            }
            let Some(args_json) = arr.get(1).as_string() else {
                continue;
            };
            let args: serde_json::Value = match serde_json::from_str(&args_json) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(line) = args.get("line").and_then(|v| v.as_str()) else {
                continue;
            };
            let envelope: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let kind = envelope
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if kind.starts_with("mobile_") {
                out.push(envelope);
            }
        }
        out
    }

    /// When mobile is enabled and the broker is Online, the
    /// `Start pairing` button is rendered enabled and clicking it
    /// fires exactly one `MobilePairingStart` frame on the host
    /// stream.
    #[wasm_bindgen_test]
    async fn mobile_tab_start_pairing_sends_frame_when_broker_online() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, true);
            install_online_broker_state(&state);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        let btn = find_button_by_text(&container, "Start pairing")
            .expect("Start pairing button must render on the Mobile tab");
        assert!(
            !btn.has_attribute("disabled"),
            "Start pairing must be enabled when mobile is on and broker is Online"
        );
        btn.click();
        for _ in 0..4 {
            next_tick().await;
        }

        let envelopes = recorded_mobile_envelopes(&calls);
        let start = envelopes
            .iter()
            .find(|env| env.get("kind").and_then(|k| k.as_str()) == Some("mobile_pairing_start"))
            .expect("Click must emit a MobilePairingStart frame");
        let stream = start.get("stream").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            stream.starts_with("/host/"),
            "MobilePairingStart must target the host stream; got: {stream:?}"
        );
    }

    /// While mobile is *disabled*, the Start button is rendered (for
    /// discoverability) but disabled — and clicking it must not emit
    /// a frame. The disabled-attribute alone suppresses the click in
    /// real browsers, but the test asserts on both surfaces to keep
    /// the contract clear.
    #[wasm_bindgen_test]
    async fn mobile_tab_start_pairing_disabled_when_mobile_disabled() {
        let _calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, false);
            install_online_broker_state(&state);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        let btn = find_button_by_text(&container, "Start pairing")
            .expect("Start pairing button must still render so users can see the affordance");
        assert!(
            btn.has_attribute("disabled"),
            "Start pairing must be disabled when mobile is not enabled"
        );
    }

    /// Each paired mobile device row has a Remove action. Clicking it
    /// confirms, then sends `MobileDeviceRevoke` so stale test devices
    /// disappear from the host-side pairing store.
    #[wasm_bindgen_test]
    async fn mobile_tab_remove_paired_device_sends_revoke() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, true);
            let url = BrokerUrl::new("mqtts://broker.test:8883").expect("broker url");
            state.mobile_access_state.update(|m| {
                m.insert(
                    "host-mobile".to_owned(),
                    protocol::MobileAccessStatePayload {
                        broker_status: MobileBrokerStatus::Online { broker_url: url },
                        pairing: MobilePairingState::Idle,
                        paired_devices: vec![protocol::MobileDeviceSummary {
                            device_id: protocol::MobileDeviceId("device-1".to_owned()),
                            label: "Old Test Phone".to_owned(),
                            key_fingerprint: "fp".to_owned(),
                            created_at_ms: 1,
                            last_seen_at_ms: None,
                            state: MobileDeviceState::Paired,
                        }],
                    },
                );
            });
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Old Test Phone"),
            "Paired device label must render before removal: {text}"
        );
        let remove =
            find_button_by_text(&container, "Remove").expect("Device row must render Remove");
        remove.click();
        for _ in 0..6 {
            next_tick().await;
        }

        let envelopes = recorded_mobile_envelopes(&calls);
        let revoke = envelopes
            .iter()
            .find(|env| env.get("kind").and_then(|k| k.as_str()) == Some("mobile_device_revoke"))
            .expect("Remove click must emit a MobileDeviceRevoke frame");
        let device_id = revoke
            .get("payload")
            .and_then(|p| p.get("device_id"))
            .and_then(|v| v.as_str());
        assert_eq!(
            device_id,
            Some("device-1"),
            "MobileDeviceRevoke must target the selected device"
        );
    }

    /// When an active offer is in state, the Mobile tab renders a QR
    /// (as inline SVG), the raw pairing URI in a copyable readonly
    /// textarea fallback, and a Cancel button. Clicking Cancel emits
    /// `MobilePairingCancel` with the matching `offer_id`.
    #[wasm_bindgen_test]
    async fn mobile_tab_active_offer_renders_qr_fallback_and_cancel() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, true);
            install_active_offer(
                &state,
                "offer-abc",
                "tyde-pair://v1?token-data-here",
                u64::MAX, // arbitrarily far in future so the expires line is positive
            );
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        // QR renders as inline SVG inside the pairing container.
        let qr_container = container
            .query_selector(".settings-mobile-pairing-qr")
            .unwrap()
            .expect("Active offer must render a QR container");
        let qr_svg = qr_container
            .query_selector("svg")
            .unwrap()
            .expect("QR container must contain an inline <svg> element");
        // The SVG must contain real QR module rects — otherwise we
        // could ship an empty <svg/> and still satisfy the "is the
        // element present" check. Each dark module renders as a
        // <rect width="1" height="1" .../> with a black fill; require
        // a healthy count so a regression that emits the placeholder
        // background only (or a stub) trips this.
        let rects = qr_svg.query_selector_all("rect").unwrap();
        assert!(
            rects.length() > 16,
            "QR SVG must contain many dark module rects (got {})",
            rects.length()
        );
        // The raw pairing URI must never appear inside the SVG. The
        // URI carries the pre-shared key; embedding it as SVG text or
        // an attribute would leak it via DOM scraping / accessibility
        // tree traversal. The QR encodes it as bitmap modules only.
        let svg_outer = qr_svg.outer_html();
        assert!(
            !svg_outer.contains("tyde-pair://"),
            "QR SVG must not embed the raw pairing URI as text/attributes"
        );
        assert!(
            !svg_outer.contains("token-data-here"),
            "QR SVG must not leak the pre-shared key portion of the URI"
        );

        // Fallback URI textarea must carry the exact qr_uri so the
        // user can copy-paste it on devices that can't scan.
        let textarea: web_sys::HtmlTextAreaElement = container
            .query_selector(".settings-mobile-pairing-uri")
            .unwrap()
            .expect("Active offer must render the URI fallback textarea")
            .dyn_into()
            .unwrap();
        assert_eq!(textarea.value(), "tyde-pair://v1?token-data-here");

        // Cancel button must be present + clicking it fires
        // MobilePairingCancel with the offer_id.
        let cancel = find_button_by_text(&container, "Cancel pairing")
            .expect("Active offer must render a Cancel pairing button");
        cancel.click();
        for _ in 0..4 {
            next_tick().await;
        }

        let envelopes = recorded_mobile_envelopes(&calls);
        let cancel_env = envelopes
            .iter()
            .find(|env| env.get("kind").and_then(|k| k.as_str()) == Some("mobile_pairing_cancel"))
            .expect("Cancel click must emit a MobilePairingCancel frame");
        let offer_id = cancel_env
            .get("payload")
            .and_then(|p| p.get("offer_id"))
            .and_then(|v| v.as_str());
        assert_eq!(
            offer_id,
            Some("offer-abc"),
            "MobilePairingCancel must carry the active offer's id"
        );
    }

    /// When the broker is in Error state, the pairing card surfaces
    /// the server error message via the broker status pill, and the
    /// Start button is disabled even when mobile is enabled.
    #[wasm_bindgen_test]
    async fn mobile_tab_broker_error_disables_start_and_shows_message() {
        let _calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, true);
            let payload = protocol::MobileAccessStatePayload {
                broker_status: MobileBrokerStatus::Error {
                    broker_url: None,
                    code: protocol::MobileAccessErrorCode::BrokerConnectionFailed,
                    message: "broker unreachable".to_owned(),
                },
                pairing: MobilePairingState::Idle,
                paired_devices: Vec::new(),
            };
            state.mobile_access_state.update(|m| {
                m.insert("host-mobile".to_owned(), payload);
            });
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Mobile");
        next_tick().await;

        let btn = find_button_by_text(&container, "Start pairing")
            .expect("Start pairing button must render even on broker error");
        assert!(
            btn.has_attribute("disabled"),
            "Start pairing must be disabled while the broker is in error state"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("broker unreachable"),
            "Broker error message must surface in the pairing card; got: {text:?}"
        );
    }

    /// QA blocker regression: the server can replace one Active
    /// pairing with another by broadcasting `MobileAccessState
    /// { Active { offer_id: NEW } }` and then sending the matching
    /// `MobilePairingOffer { offer_id: NEW }` only to the *requester*.
    /// A bystander UI that already had stored offer A must NOT keep
    /// rendering A's QR after the Active.offer_id changes to B —
    /// otherwise Cancel would target the now-stale offer A and the
    /// user would scan an obsolete QR.
    ///
    /// Drives the dispatcher directly (no DOM) so the assertion is
    /// about state-level reconciliation, where the bug lives.
    #[wasm_bindgen_test]
    async fn dispatch_mobile_access_state_drops_stale_offer_on_id_mismatch() {
        use crate::dispatch::dispatch_envelope;
        use protocol::{Envelope, MobilePairingOfferId, MobilePairingQrUri};

        // Independent host id so this test's INBOUND_SEQ /
        // INBOUND_PROTOCOL state doesn't collide with any other
        // settings_panel test the runner happens to schedule before
        // us. `reset_inbound_protocol` also clears stream-registration
        // state so a fresh host stream is accepted.

        let state = AppState::new();
        let host_id = "h-mobile-mismatch";
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.host_streams.update(|m| {
            m.insert(
                host_id.to_owned(),
                protocol::StreamPath(format!("/host/{host_id}")),
            );
        });

        crate::dispatch::prime_host_for_tests(&state, host_id);

        // Seed: bystander already has offer A stored from an earlier
        // pairing the server broadcast.
        let offer_a = protocol::MobilePairingOfferPayload {
            offer_id: MobilePairingOfferId("offer-A".to_owned()),
            qr_uri: MobilePairingQrUri("tyde-pair://v1?stale-A".to_owned()),
            expires_at_ms: u64::MAX,
        };
        state.mobile_pairing_offer.update(|m| {
            m.insert(host_id.to_owned(), offer_a);
        });

        // Server broadcasts a fresh Active state for a *different*
        // offer id. The new offer payload itself is only delivered to
        // the requester (not this bystander).
        let new_state = protocol::MobileAccessStatePayload {
            broker_status: MobileBrokerStatus::Online {
                broker_url: BrokerUrl::new("mqtts://broker.test:8883").expect("broker url"),
            },
            pairing: MobilePairingState::Active {
                offer_id: MobilePairingOfferId("offer-B".to_owned()),
                expires_at_ms: u64::MAX,
            },
            paired_devices: Vec::new(),
        };
        let stream = protocol::StreamPath(format!("/host/{host_id}"));
        let envelope = Envelope::from_payload(stream, FrameKind::MobileAccessState, 0, &new_state)
            .expect("envelope serialize");
        dispatch_envelope(&state, host_id, envelope);

        // The stale offer A must be gone. Without the fix, this
        // assertion fails because the bystander keeps rendering
        // offer A while the server has already rotated to B.
        let stored = state
            .mobile_pairing_offer
            .with_untracked(|m| m.get(host_id).cloned());
        assert!(
            stored.is_none(),
            "Active.offer_id changed; stale stored offer must be cleared (still had: {stored:?})"
        );
        // The new MobileAccessState should be stored regardless.
        let access = state
            .mobile_access_state
            .with_untracked(|m| m.get(host_id).cloned());
        assert!(
            matches!(
                access.as_ref().map(|s| &s.pairing),
                Some(MobilePairingState::Active { .. })
            ),
            "MobileAccessState snapshot must still be stored"
        );
    }

    /// Counterpart: when `Active.offer_id` matches the stored
    /// offer's id, the stored offer must NOT be cleared. (Without
    /// this we'd churn the QR on every state replay.)
    #[wasm_bindgen_test]
    async fn dispatch_mobile_access_state_keeps_offer_when_id_matches() {
        use crate::dispatch::dispatch_envelope;
        use protocol::{Envelope, MobilePairingOfferId, MobilePairingQrUri};

        let state = AppState::new();
        let host_id = "h-mobile-match";
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.host_streams.update(|m| {
            m.insert(
                host_id.to_owned(),
                protocol::StreamPath(format!("/host/{host_id}")),
            );
        });

        crate::dispatch::prime_host_for_tests(&state, host_id);

        let offer_id_str = "offer-X";
        let offer = protocol::MobilePairingOfferPayload {
            offer_id: MobilePairingOfferId(offer_id_str.to_owned()),
            qr_uri: MobilePairingQrUri("tyde-pair://v1?valid".to_owned()),
            expires_at_ms: u64::MAX,
        };
        state.mobile_pairing_offer.update(|m| {
            m.insert(host_id.to_owned(), offer);
        });

        let access_state = protocol::MobileAccessStatePayload {
            broker_status: MobileBrokerStatus::Online {
                broker_url: BrokerUrl::new("mqtts://broker.test:8883").expect("broker url"),
            },
            pairing: MobilePairingState::Active {
                offer_id: MobilePairingOfferId(offer_id_str.to_owned()),
                expires_at_ms: u64::MAX,
            },
            paired_devices: Vec::new(),
        };
        let stream = protocol::StreamPath(format!("/host/{host_id}"));
        let envelope =
            Envelope::from_payload(stream, FrameKind::MobileAccessState, 0, &access_state)
                .expect("envelope serialize");
        dispatch_envelope(&state, host_id, envelope);

        let stored = state
            .mobile_pairing_offer
            .with_untracked(|m| m.get(host_id).cloned());
        assert_eq!(
            stored.as_ref().map(|o| o.offer_id.0.as_str()),
            Some(offer_id_str),
            "Matching Active.offer_id must NOT clear the stored offer"
        );
    }

    /// Non-Active phases (Consumed / Expired / Cancelled / Failed)
    /// must always clear the stored offer regardless of id. Cancelled
    /// covers the Cancel-roundtrip case the bystander would otherwise
    /// see after the server confirms a stale-id Cancel.
    #[wasm_bindgen_test]
    async fn dispatch_mobile_access_state_drops_offer_on_non_active_phase() {
        use crate::dispatch::dispatch_envelope;
        use protocol::{Envelope, MobilePairingOfferId, MobilePairingQrUri};

        let state = AppState::new();
        let host_id = "h-mobile-cancelled";
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.host_streams.update(|m| {
            m.insert(
                host_id.to_owned(),
                protocol::StreamPath(format!("/host/{host_id}")),
            );
        });

        crate::dispatch::prime_host_for_tests(&state, host_id);

        let offer = protocol::MobilePairingOfferPayload {
            offer_id: MobilePairingOfferId("offer-Z".to_owned()),
            qr_uri: MobilePairingQrUri("tyde-pair://v1?cancelled".to_owned()),
            expires_at_ms: u64::MAX,
        };
        state.mobile_pairing_offer.update(|m| {
            m.insert(host_id.to_owned(), offer);
        });

        let access_state = protocol::MobileAccessStatePayload {
            broker_status: MobileBrokerStatus::Online {
                broker_url: BrokerUrl::new("mqtts://broker.test:8883").expect("broker url"),
            },
            // Cancelled: offer_id is still in the state but the
            // pairing lifecycle is no longer Active — UI should
            // stop rendering the QR.
            pairing: MobilePairingState::Cancelled {
                offer_id: MobilePairingOfferId("offer-Z".to_owned()),
            },
            paired_devices: Vec::new(),
        };
        let stream = protocol::StreamPath(format!("/host/{host_id}"));
        let envelope =
            Envelope::from_payload(stream, FrameKind::MobileAccessState, 0, &access_state)
                .expect("envelope serialize");
        dispatch_envelope(&state, host_id, envelope);

        let stored = state
            .mobile_pairing_offer
            .with_untracked(|m| m.get(host_id).cloned());
        assert!(
            stored.is_none(),
            "Non-Active phase must clear the stored offer (still had: {stored:?})"
        );
    }

    // ---- General tab: background agent features ----

    /// Install a connected host whose `background_agent_features` are set to
    /// the given values, so the General tab's toggles have a selected host to
    /// read from and a stream to commit settings against.
    fn install_general_host_settings(
        state: &AppState,
        auto_generate_agent_names: bool,
        agent_activity_summaries: bool,
        rust_analyzer_path: Option<&str>,
    ) {
        let host_id = "host-general".to_owned();
        state.selected_host_id.set(Some(host_id.clone()));
        state.host_streams.update(|m| {
            m.insert(
                host_id.clone(),
                protocol::StreamPath(format!("/host/{host_id}")),
            );
        });
        state.connection_statuses.update(|m| {
            m.insert(host_id.clone(), crate::state::ConnectionStatus::Connected);
        });
        let mut code_intel = protocol::CodeIntelSettings::default();
        if let Some(path) = rust_analyzer_path {
            code_intel.language_server_paths.insert(
                CodeIntelProviderId("rust-analyzer".to_owned()),
                HostExecutablePath(path.to_owned()),
            );
        }

        state.host_settings_by_host.update(|m| {
            m.insert(
                host_id,
                protocol::HostSettings {
                    enabled_backends: vec![protocol::BackendKind::Claude],
                    default_backend: Some(protocol::BackendKind::Claude),
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: protocol::BackgroundAgentFeaturesSettings {
                        auto_generate_agent_names,
                        agent_activity_summaries,
                    },
                    code_intel,
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: Vec::new(),
                },
            );
        });
    }

    /// Find the checkbox inside the `.settings-toggle-row` whose visible text
    /// contains `label`. Resolves toggles by what the user reads, not by
    /// element ordering or private classes.
    fn toggle_for_label(container: &HtmlElement, label: &str) -> web_sys::HtmlInputElement {
        let rows = container
            .query_selector_all(".settings-toggle-row")
            .expect("toggle rows");
        let mut observed: Vec<String> = Vec::new();
        for i in 0..rows.length() {
            let Some(node) = rows.item(i) else { continue };
            let Ok(row) = node.dyn_into::<HtmlElement>() else {
                continue;
            };
            let txt = row.text_content().unwrap_or_default();
            if txt.contains(label) {
                let input = row
                    .query_selector("input[type='checkbox']")
                    .unwrap()
                    .expect("toggle row must contain a checkbox");
                return input.dyn_into().expect("checkbox element");
            }
            observed.push(txt);
        }
        panic!("no settings-toggle-row containing {label:?}; saw {observed:?}");
    }

    /// Toggling "Agent activity summaries" on must commit a `SetSetting`
    /// frame whose payload is
    /// `BackgroundAgentFeatureEnabled { feature: AgentActivitySummaries,
    /// enabled: true }`. This is the load-bearing wire assertion for the
    /// paid opt-in: if the toggle silently became a no-op nothing else
    /// would notice.
    #[wasm_bindgen_test]
    async fn general_tab_activity_summaries_toggle_commits_setting() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_general_host_settings(&state, true, false, None);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "General");
        next_tick().await;

        let toggle = toggle_for_label(&container, "Agent activity summaries");
        assert!(
            !toggle.checked(),
            "summaries start off in this fixture before the user toggles"
        );
        toggle.set_checked(true);
        dispatch_change(&toggle);
        for _ in 0..4 {
            next_tick().await;
        }

        let settings = recorded_set_setting_payloads(&calls);
        let frame = settings
            .iter()
            .find(|s| {
                s.get("kind").and_then(|k| k.as_str()) == Some("background_agent_feature_enabled")
                    && s.get("feature").and_then(|f| f.as_str()) == Some("agent_activity_summaries")
            })
            .expect(
                "toggling activity summaries must emit a BackgroundAgentFeatureEnabled SetSetting",
            );
        assert_eq!(
            frame.get("enabled").and_then(|v| v.as_bool()),
            Some(true),
            "the committed frame must carry enabled=true: {frame:?}"
        );
    }

    /// The Background agent features section must reflect the host's current
    /// `background_agent_features` values (checked/unchecked) and tell the
    /// user the summaries feature costs money.
    #[wasm_bindgen_test]
    async fn general_tab_background_features_reflect_current_settings() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_general_host_settings(&state, false, true, None);
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "General");
        next_tick().await;

        let names = toggle_for_label(&container, "Auto-generate agent names");
        let summaries = toggle_for_label(&container, "Agent activity summaries");
        assert!(
            !names.checked(),
            "auto_generate_agent_names=false must render as an unchecked toggle"
        );
        assert!(
            summaries.checked(),
            "agent_activity_summaries=true must render as a checked toggle"
        );

        let body = container.text_content().unwrap_or_default().to_lowercase();
        assert!(
            body.contains("costs money") || body.contains("cost money"),
            "the section must warn the user that summaries cost money: {body}"
        );
    }

    fn rust_analyzer_path_input(container: &HtmlElement) -> web_sys::HtmlInputElement {
        container
            .query_selector("input[aria-label='rust-analyzer binary path']")
            .unwrap()
            .expect("rust-analyzer binary path input must render on the General tab")
            .dyn_into()
            .unwrap()
    }

    #[wasm_bindgen_test]
    async fn general_tab_rust_analyzer_path_commits_set_and_clear() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_general_host_settings(&state, true, false, Some("/old/rust-analyzer"));
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "General");
        next_tick().await;

        let input = rust_analyzer_path_input(&container);
        assert_eq!(
            input.value(),
            "/old/rust-analyzer",
            "rust-analyzer path input must reflect the current host setting"
        );
        input.set_value("/opt/bin/rust-analyzer");
        dispatch_enter(&input);
        for _ in 0..4 {
            next_tick().await;
        }

        let settings = recorded_set_setting_payloads(&calls);
        let set_frame = settings
            .iter()
            .find(|s| {
                s.get("kind").and_then(|k| k.as_str()) == Some("code_intel_language_server_path")
                    && s.get("provider").and_then(|p| p.as_str()) == Some("rust-analyzer")
                    && s.get("path").and_then(|p| p.as_str()) == Some("/opt/bin/rust-analyzer")
            })
            .expect("Enter must emit CodeIntelLanguageServerPath with the typed path");
        assert_eq!(
            set_frame.get("path").and_then(|p| p.as_str()),
            Some("/opt/bin/rust-analyzer")
        );

        let clear = find_button_by_text(&container, "Clear").expect("Clear button must render");
        clear.click();
        for _ in 0..4 {
            next_tick().await;
        }

        let settings = recorded_set_setting_payloads(&calls);
        let clear_frame = settings
            .iter()
            .rev()
            .find(|s| {
                s.get("kind").and_then(|k| k.as_str()) == Some("code_intel_language_server_path")
                    && s.get("provider").and_then(|p| p.as_str()) == Some("rust-analyzer")
            })
            .expect("Clear must emit CodeIntelLanguageServerPath for rust-analyzer");
        match clear_frame.get("path") {
            None => {}
            Some(value) if value.is_null() => {}
            Some(value) => panic!("Clear must send path=None/null; got {value:?}"),
        }
    }

    fn host_settings_with_hermes_config(
        backend_config: std::collections::HashMap<BackendKind, BackendConfigValues>,
    ) -> protocol::HostSettings {
        protocol::HostSettings {
            enabled_backends: vec![BackendKind::Hermes],
            default_backend: Some(BackendKind::Hermes),
            enable_mobile_connections: false,
            mobile_broker_url: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            complexity_tiers_enabled: false,
            backend_tier_configs: std::collections::HashMap::new(),
            background_agent_features: Default::default(),
            code_intel: Default::default(),
            backend_config,
            launch_profiles: Vec::new(),
        }
    }

    fn hermes_config_schema() -> protocol::BackendConfigSchema {
        let text = || BackendConfigFieldType::Text {
            default: None,
            placeholder: None,
            multiline: false,
        };
        protocol::BackendConfigSchema {
            backend_kind: BackendKind::Hermes,
            fields: vec![
                BackendConfigField {
                    key: "default_model".to_owned(),
                    label: "Default Model".to_owned(),
                    description: None,
                    field_type: text(),
                },
                BackendConfigField {
                    key: "default_provider".to_owned(),
                    label: "Default Provider".to_owned(),
                    description: None,
                    field_type: text(),
                },
                BackendConfigField {
                    key: "api_base_url".to_owned(),
                    label: "API Base URL".to_owned(),
                    description: None,
                    field_type: text(),
                },
            ],
        }
    }

    /// The settings-panel deep-config section renders a backend's schema fields
    /// and seeds each control from the stored host-level value.
    #[wasm_bindgen_test]
    async fn backend_config_section_renders_schema_fields_and_seeds_stored_values() {
        let container = make_container();
        let state = AppState::new();
        let host_id = "host-a".to_owned();
        state.selected_host_id.set(Some(host_id.clone()));

        let mut values = BackendConfigValues::default();
        values.0.insert(
            "default_model".to_owned(),
            SessionSettingValue::String("anthropic/claude-sonnet-5".to_owned()),
        );
        let mut backend_config = std::collections::HashMap::new();
        backend_config.insert(BackendKind::Hermes, values);
        state.host_settings_by_host.update(|map| {
            map.insert(
                host_id.clone(),
                host_settings_with_hermes_config(backend_config),
            );
        });
        state.backend_config_schemas.update(|map| {
            map.entry(host_id.clone())
                .or_default()
                .insert(BackendKind::Hermes, hermes_config_schema());
        });

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendConfigSection /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Backend Settings"),
            "section heading must render: {text:?}"
        );
        for label in ["Default Model", "Default Provider", "API Base URL"] {
            assert!(
                text.contains(label),
                "field label {label:?} must render: {text:?}"
            );
        }

        let inputs = container
            .query_selector_all("input.settings-backend-config-input")
            .unwrap();
        assert_eq!(inputs.length(), 3, "one input rendered per schema field");
        let first: HtmlInputElement = inputs.item(0).unwrap().dyn_into().unwrap();
        assert_eq!(
            first.value(),
            "anthropic/claude-sonnet-5",
            "the stored default_model value must seed its input"
        );
    }

    /// A backend that exposes no config schema renders nothing (no empty card).
    #[wasm_bindgen_test]
    async fn backend_config_section_is_empty_without_schema() {
        let container = make_container();
        let state = AppState::new();
        let host_id = "host-a".to_owned();
        state.selected_host_id.set(Some(host_id.clone()));
        state.host_settings_by_host.update(|map| {
            map.insert(
                host_id.clone(),
                host_settings_with_hermes_config(std::collections::HashMap::new()),
            );
        });
        // No schema pushed for this host.

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendConfigSection /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("Backend Settings"),
            "no schema means no section: {text:?}"
        );
        assert_eq!(
            container
                .query_selector_all("input.settings-backend-config-input")
                .unwrap()
                .length(),
            0,
            "no config inputs without a schema"
        );
    }

    /// Install a connected host with the Hermes deep-config schema plus stored
    /// values, and select it — enough for `BackendConfigSection` to render and
    /// persist edits over the wire.
    fn install_backend_config_host(state: &AppState, values: BackendConfigValues) {
        let host_id = "host-cfg".to_owned();
        state.selected_host_id.set(Some(host_id.clone()));
        state.host_streams.update(|m| {
            m.insert(
                host_id.clone(),
                protocol::StreamPath(format!("/host/{host_id}")),
            );
        });
        state.connection_statuses.update(|m| {
            m.insert(host_id.clone(), crate::state::ConnectionStatus::Connected);
        });
        let mut backend_config = std::collections::HashMap::new();
        backend_config.insert(BackendKind::Hermes, values);
        state.host_settings_by_host.update(|m| {
            m.insert(
                host_id.clone(),
                host_settings_with_hermes_config(backend_config),
            );
        });
        state.backend_config_schemas.update(|m| {
            m.entry(host_id)
                .or_default()
                .insert(BackendKind::Hermes, hermes_config_schema());
        });
    }

    /// Set an input's value and fire a `change` event, then drop the dispatch id
    /// so a later dispatch on a sibling input doesn't resolve back to this one.
    fn set_and_change(input: &HtmlInputElement, value: &str) {
        input.set_value(value);
        dispatch_event_from_js(input, "change", None);
        let _ = input.remove_attribute("id");
    }

    /// Most recent `backend_config` SetSetting `setting` payload, if any.
    fn last_backend_config(calls: &js_sys::Array) -> Option<serde_json::Value> {
        recorded_set_setting_payloads(calls)
            .into_iter()
            .rev()
            .find(|s| s.get("kind").and_then(|k| k.as_str()) == Some("backend_config"))
    }

    /// Editing one backend-config field sends a partial update carrying only
    /// that key (server merges, siblings preserved), and clearing a field sends
    /// an explicit `Null` for it rather than dropping the key.
    #[wasm_bindgen_test]
    async fn backend_config_edit_sends_partial_update_and_null_clear() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let mut stored = BackendConfigValues::default();
        stored.0.insert(
            "default_model".to_owned(),
            SessionSettingValue::String("anthropic/claude-sonnet-5".to_owned()),
        );
        let stored_for_mount = stored.clone();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_backend_config_host(&state, stored_for_mount.clone());
            provide_context(state);
            view! { <BackendConfigSection /> }
        });
        next_tick().await;

        let inputs = container
            .query_selector_all("input.settings-backend-config-input")
            .unwrap();
        // Schema field order: default_model (0), default_provider (1), api_base_url (2).
        let provider: HtmlInputElement = inputs.item(1).unwrap().dyn_into().unwrap();
        set_and_change(&provider, "openrouter");
        next_tick().await;

        let setting = last_backend_config(&calls).expect("backend_config frame after an edit");
        assert_eq!(
            setting.get("backend").and_then(|b| b.as_str()),
            Some("hermes"),
            "edit targets the Hermes backend: {setting:?}"
        );
        let values = setting
            .get("values")
            .and_then(|v| v.as_object())
            .expect("values object");
        assert_eq!(
            values.len(),
            1,
            "only the edited key is sent so the server merge preserves siblings: {values:?}"
        );
        assert_eq!(
            values
                .get("default_provider")
                .and_then(|v| v.get("string"))
                .and_then(|s| s.as_str()),
            Some("openrouter"),
            "the edited value is carried typed: {values:?}"
        );
        assert!(
            !values.contains_key("default_model"),
            "an unchanged sibling key must not be resent: {values:?}"
        );

        // Clearing the stored default_model sends an explicit Null, not omission.
        let model: HtmlInputElement = inputs.item(0).unwrap().dyn_into().unwrap();
        set_and_change(&model, "");
        next_tick().await;

        let setting = last_backend_config(&calls).expect("backend_config frame after a clear");
        let values = setting
            .get("values")
            .and_then(|v| v.as_object())
            .expect("values object");
        assert_eq!(
            values.len(),
            1,
            "clear sends just the cleared key: {values:?}"
        );
        assert_eq!(
            values.get("default_model").and_then(|v| v.as_str()),
            Some("null"),
            "clearing a field sends an explicit Null so the server clears just it: {values:?}"
        );
    }

    /// Install a backend-native config snapshot for the Hermes card on the
    /// `host-cfg` host used by `install_backend_config_host`.
    fn set_backend_snapshot(
        state: &AppState,
        status: BackendConfigSnapshotStatus,
        values: BackendConfigValues,
        message: Option<&str>,
    ) {
        state.backend_config_snapshots.update(|m| {
            m.entry("host-cfg".to_owned()).or_default().insert(
                BackendKind::Hermes,
                protocol::BackendConfigSnapshot {
                    backend_kind: BackendKind::Hermes,
                    status,
                    values,
                    message: message.map(|s| s.to_owned()),
                },
            );
        });
    }

    /// With no Tyde override, the control shows the backend's own current value
    /// from the snapshot and labels it as coming from the backend — the UI never
    /// invents this value locally.
    #[wasm_bindgen_test]
    async fn backend_config_native_snapshot_seeds_controls() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_backend_config_host(&state, BackendConfigValues::default());
            let mut native = BackendConfigValues::default();
            native.0.insert(
                "default_model".to_owned(),
                SessionSettingValue::String("anthropic/claude-opus".to_owned()),
            );
            set_backend_snapshot(&state, BackendConfigSnapshotStatus::Ready, native, None);
            provide_context(state);
            view! { <BackendConfigSection /> }
        });
        next_tick().await;

        let inputs = container
            .query_selector_all("input.settings-backend-config-input")
            .unwrap();
        let first: HtmlInputElement = inputs.item(0).unwrap().dyn_into().unwrap();
        assert_eq!(
            first.value(),
            "anthropic/claude-opus",
            "the native snapshot value seeds the control when there is no Tyde override"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("From backend"),
            "an unoverridden field is labelled as coming from the backend: {text:?}"
        );
    }

    /// An explicit Tyde override wins over the native value in the control, and
    /// the caption still shows the backend value it diverges from.
    #[wasm_bindgen_test]
    async fn backend_config_override_wins_over_native_and_shows_both() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let mut overrides = BackendConfigValues::default();
            overrides.0.insert(
                "default_model".to_owned(),
                SessionSettingValue::String("my-override".to_owned()),
            );
            install_backend_config_host(&state, overrides);
            let mut native = BackendConfigValues::default();
            native.0.insert(
                "default_model".to_owned(),
                SessionSettingValue::String("native-model".to_owned()),
            );
            set_backend_snapshot(&state, BackendConfigSnapshotStatus::Ready, native, None);
            provide_context(state);
            view! { <BackendConfigSection /> }
        });
        next_tick().await;

        let inputs = container
            .query_selector_all("input.settings-backend-config-input")
            .unwrap();
        let first: HtmlInputElement = inputs.item(0).unwrap().dyn_into().unwrap();
        assert_eq!(
            first.value(),
            "my-override",
            "the Tyde override wins over the native value in the control"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Tyde override"),
            "an overridden field is badged as an override: {text:?}"
        );
        assert!(
            text.contains("native-model"),
            "the override caption shows the backend value it diverges from: {text:?}"
        );
    }

    /// When the server reports the snapshot is unavailable, its reason is shown
    /// verbatim (never swallowed) and the schema fields still render so overrides
    /// stay editable.
    #[wasm_bindgen_test]
    async fn backend_config_unavailable_snapshot_surfaces_message() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_backend_config_host(&state, BackendConfigValues::default());
            set_backend_snapshot(
                &state,
                BackendConfigSnapshotStatus::Unavailable,
                BackendConfigValues::default(),
                Some("Hermes gateway not reachable"),
            );
            provide_context(state);
            view! { <BackendConfigSection /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Hermes gateway not reachable"),
            "the server's unavailable reason must be surfaced, not swallowed: {text:?}"
        );
        assert_eq!(
            container
                .query_selector_all("input.settings-backend-config-input")
                .unwrap()
                .length(),
            3,
            "schema fields still render so overrides remain editable while native values are unavailable"
        );
    }

    // ---- Launch Profiles editor ----

    fn launch_profile_config(id: &str, label: &str) -> HostLaunchProfileConfig {
        HostLaunchProfileConfig {
            id: LaunchProfileId(id.to_owned()),
            label: label.to_owned(),
            description: None,
            backend_kind: BackendKind::Hermes,
            session_settings: SessionSettingsValues::default(),
        }
    }

    /// Install a connected host whose Hermes session schema exposes a `model`
    /// select, plus any explicit launch profiles, and select it. Enough for the
    /// Launch Profiles editor to render and persist typed settings.
    fn install_launch_profile_host(state: &AppState, profiles: Vec<HostLaunchProfileConfig>) {
        let host_id = "host-lp".to_owned();
        state.selected_host_id.set(Some(host_id.clone()));
        state.host_streams.update(|m| {
            m.insert(
                host_id.clone(),
                protocol::StreamPath(format!("/host/{host_id}")),
            );
        });
        state.connection_statuses.update(|m| {
            m.insert(host_id.clone(), crate::state::ConnectionStatus::Connected);
        });
        state.host_settings_by_host.update(|m| {
            m.insert(
                host_id.clone(),
                protocol::HostSettings {
                    enabled_backends: vec![BackendKind::Hermes],
                    default_backend: Some(BackendKind::Hermes),
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    code_intel: Default::default(),
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: profiles,
                },
            );
        });
        state.schemas_loaded_for_host.update(|m| {
            m.insert(host_id.clone(), true);
        });
        state.session_schemas.update(|m| {
            let host = m.entry(host_id).or_default();
            host.insert(
                BackendKind::Hermes,
                SessionSchemaEntry::Ready {
                    schema: SessionSettingsSchema {
                        backend_kind: BackendKind::Hermes,
                        fields: vec![protocol::SessionSettingField {
                            key: "model".to_owned(),
                            label: "Model".to_owned(),
                            description: None,
                            field_type: SessionSettingFieldType::Select {
                                options: vec![
                                    SelectOption {
                                        value: "sonnet".to_owned(),
                                        label: "Sonnet".to_owned(),
                                    },
                                    SelectOption {
                                        value: "opus".to_owned(),
                                        label: "Opus".to_owned(),
                                    },
                                ],
                                default: Some("sonnet".to_owned()),
                                nullable: false,
                            },
                            use_slider: false,
                        }],
                    },
                },
            );
        });
    }

    fn set_input_value(input: &web_sys::HtmlInputElement, value: &str) {
        input.set_value(value);
        dispatch_event_from_js(input, "input", None);
        // `dispatch_event_from_js` tags the element with a fixed id; clear it so
        // dispatching on a sibling input doesn't resolve back to this one.
        let _ = input.remove_attribute("id");
    }

    /// Return the parsed `profiles` array of the most recent LaunchProfiles
    /// SetSetting frame, or `None` if none was emitted.
    fn last_launch_profiles(calls: &js_sys::Array) -> Option<Vec<serde_json::Value>> {
        recorded_set_setting_payloads(calls)
            .into_iter()
            .rev()
            .find(|s| s.get("kind").and_then(|k| k.as_str()) == Some("launch_profiles"))
            .and_then(|s| {
                s.get("profiles")
                    .and_then(|p| p.as_array())
                    .map(|a| a.to_vec())
            })
    }

    /// Existing explicit launch profiles render as rows with a "New" affordance.
    #[wasm_bindgen_test]
    async fn launch_profiles_render_existing_rows() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_launch_profile_host(
                &state,
                vec![launch_profile_config("hermes:claude", "Hermes · Claude")],
            );
            provide_context(state);
            view! { <LaunchProfilesSection /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Hermes · Claude"),
            "existing profile label must render: {text:?}"
        );
        assert!(
            text.contains("hermes:claude"),
            "existing profile id must render: {text:?}"
        );
        assert!(
            find_button_by_text(&container, "+ New launch profile").is_some(),
            "New launch profile button must be present"
        );
    }

    /// Adding a profile emits a `LaunchProfiles` SetSetting carrying the new
    /// entry alongside any existing ones.
    #[wasm_bindgen_test]
    async fn launch_profiles_add_emits_set_setting() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_launch_profile_host(&state, Vec::new());
            provide_context(state);
            view! { <LaunchProfilesSection /> }
        });
        next_tick().await;

        find_button_by_text(&container, "+ New launch profile")
            .expect("New button")
            .click();
        next_tick().await;

        let inputs = container
            .query_selector_all(".settings-form .settings-text-input")
            .unwrap();
        let id_input: web_sys::HtmlInputElement = inputs.item(0).unwrap().dyn_into().unwrap();
        let label_input: web_sys::HtmlInputElement = inputs.item(1).unwrap().dyn_into().unwrap();
        set_input_value(&id_input, "hermes:grok");
        set_input_value(&label_input, "Hermes · Grok");
        next_tick().await;

        find_button_by_text(&container, "Save")
            .expect("Save button")
            .click();
        for _ in 0..3 {
            next_tick().await;
        }

        let profiles =
            last_launch_profiles(&calls).expect("a LaunchProfiles frame must be emitted");
        assert_eq!(profiles.len(), 1, "one profile persisted: {profiles:?}");
        assert_eq!(
            profiles[0].get("id").and_then(|v| v.as_str()),
            Some("hermes:grok")
        );
        assert_eq!(
            profiles[0].get("label").and_then(|v| v.as_str()),
            Some("Hermes · Grok")
        );
        assert_eq!(
            profiles[0].get("backend_kind").and_then(|v| v.as_str()),
            Some("hermes"),
            "backend kind must be carried typed"
        );
    }

    /// Editing a profile's typed session setting (Hermes model) persists a
    /// `LaunchProfiles` frame whose `session_settings` carries the typed value.
    #[wasm_bindgen_test]
    async fn launch_profiles_edit_persists_typed_session_settings() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_launch_profile_host(
                &state,
                vec![launch_profile_config("hermes:claude", "Hermes · Claude")],
            );
            provide_context(state);
            view! { <LaunchProfilesSection /> }
        });
        next_tick().await;

        find_button_by_text(&container, "Edit")
            .expect("Edit button")
            .click();
        next_tick().await;

        let select: web_sys::HtmlSelectElement = container
            .query_selector(".settings-form .session-setting-select")
            .unwrap()
            .expect("typed session-setting select must render from the Hermes schema")
            .dyn_into()
            .unwrap();
        select.set_value("opus");
        dispatch_event_from_js(&select.clone().unchecked_into(), "change", None);
        next_tick().await;

        find_button_by_text(&container, "Save")
            .expect("Save button")
            .click();
        for _ in 0..3 {
            next_tick().await;
        }

        let profiles =
            last_launch_profiles(&calls).expect("a LaunchProfiles frame must be emitted");
        assert_eq!(profiles.len(), 1, "still one profile: {profiles:?}");
        let model = profiles[0]
            .get("session_settings")
            .and_then(|s| s.get("model"))
            .and_then(|m| m.get("string"))
            .and_then(|v| v.as_str());
        assert_eq!(
            model,
            Some("opus"),
            "typed session settings must be persisted on the profile: {profiles:?}"
        );
    }

    /// Removing a profile confirms then emits a `LaunchProfiles` frame with the
    /// remaining profiles only.
    #[wasm_bindgen_test]
    async fn launch_profiles_remove_emits_set_setting() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_launch_profile_host(
                &state,
                vec![
                    launch_profile_config("hermes:claude", "Hermes · Claude"),
                    launch_profile_config("hermes:codex", "Hermes · Codex"),
                ],
            );
            provide_context(state);
            view! { <LaunchProfilesSection /> }
        });
        next_tick().await;

        // Delete the first row's profile.
        find_button_by_text(&container, "Delete")
            .expect("Delete button")
            .click();
        next_tick().await;
        // Confirm in the dialog (scoped to the modal, since row buttons also
        // read "Delete").
        let confirm: HtmlElement = container
            .query_selector(".settings-confirm-modal .settings-btn-danger")
            .unwrap()
            .expect("confirm dialog Delete button")
            .dyn_into()
            .unwrap();
        confirm.click();
        for _ in 0..3 {
            next_tick().await;
        }

        let profiles =
            last_launch_profiles(&calls).expect("a LaunchProfiles frame must be emitted");
        let ids: Vec<&str> = profiles
            .iter()
            .filter_map(|p| p.get("id").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(profiles.len(), 1, "one profile must remain: {profiles:?}");
        assert_eq!(
            ids,
            vec!["hermes:codex"],
            "the other profile must be removed"
        );
    }

    /// A reserved default id (e.g. `claude:default`) is rejected in-editor,
    /// mirroring the server rule. No `LaunchProfiles` frame is sent and the
    /// error stays visible instead of the save closing optimistically.
    #[wasm_bindgen_test]
    async fn launch_profiles_reject_reserved_default_id() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_launch_profile_host(&state, Vec::new());
            provide_context(state);
            view! { <LaunchProfilesSection /> }
        });
        next_tick().await;

        find_button_by_text(&container, "+ New launch profile")
            .expect("New button")
            .click();
        next_tick().await;

        let inputs = container
            .query_selector_all(".settings-form .settings-text-input")
            .unwrap();
        let id_input: web_sys::HtmlInputElement = inputs.item(0).unwrap().dyn_into().unwrap();
        let label_input: web_sys::HtmlInputElement = inputs.item(1).unwrap().dyn_into().unwrap();
        set_input_value(&id_input, "claude:default");
        set_input_value(&label_input, "Reserved");
        next_tick().await;

        find_button_by_text(&container, "Save")
            .expect("Save button")
            .click();
        for _ in 0..3 {
            next_tick().await;
        }

        assert!(
            last_launch_profiles(&calls).is_none(),
            "a reserved id must not reach the wire"
        );
        let error_text = container
            .query_selector(".settings-error")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default();
        assert!(
            error_text.contains("reserved"),
            "the editor must show a visible reserved-id error: {error_text:?}"
        );
        // Editor still open (Save button present) so the user can fix the id.
        assert!(
            find_button_by_text(&container, "Save").is_some(),
            "editor must stay open on validation failure"
        );
    }

    /// Selecting a backend that isn't enabled on the host surfaces an inline
    /// warning that the profile won't appear in New Chat until it's enabled.
    /// The warning is absent for an enabled backend.
    #[wasm_bindgen_test]
    async fn launch_profiles_warn_on_disabled_backend() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            // Host enables Hermes only.
            install_launch_profile_host(&state, Vec::new());
            provide_context(state);
            view! { <LaunchProfilesSection /> }
        });
        next_tick().await;

        find_button_by_text(&container, "+ New launch profile")
            .expect("New button")
            .click();
        next_tick().await;

        // Default backend is Hermes (enabled) → no warning.
        assert!(
            container
                .query_selector(".settings-form-warning")
                .unwrap()
                .is_none(),
            "no warning when the selected backend is enabled"
        );

        // Switch to Codex, which is not enabled on this host.
        let select: web_sys::HtmlSelectElement = container
            .query_selector(".settings-form .settings-select")
            .unwrap()
            .expect("backend select must render")
            .dyn_into()
            .unwrap();
        select.set_value("codex");
        dispatch_event_from_js(&select.clone().unchecked_into(), "change", None);
        next_tick().await;

        let warning = container
            .query_selector(".settings-form-warning")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default();
        assert!(
            warning.contains("not enabled") && warning.contains("New Chat"),
            "disabled backend must show a clear inline warning: {warning:?}"
        );
    }
}
